//! `run` when stdout is a terminal: the command gets a
//! pseudo-terminal, so it sees a terminal (colors, progress bars, line editing) and the
//! capture gets exactly the bytes that reach the screen.
//!
//! - The pseudo-terminal: posix_openpt, grantpt, unlockpt, ptsname_r (sys_run.rs). It starts
//!   with the settings and the window size of our terminal.
//! - The command is a Command whose stdout is the pseudo-terminal, with setsid() and
//!   TIOCSCTTY before exec: its own session, with the pseudo-terminal as its controlling
//!   terminal. Its stdin is the pseudo-terminal when ours is a terminal; otherwise it reads
//!   our stdin as it is (a file or a pipe ends as it would without `run`). Its stderr is the
//!   pseudo-terminal when ours is a terminal; otherwise (`2>err.log`, `2>/dev/null`) a pipe
//!   whose bytes go to our stderr unchanged and to the capture as stderr lines, listed in
//!   rN.err as in pipes mode, so a redirection keeps working.
//! - Our terminal goes into raw mode while we are its foreground (tcgetattr, cfmakeraw,
//!   tcsetattr), and is restored on every way out (a guard, also on a panic). Keys go to the
//!   pseudo-terminal as bytes: Ctrl-C reaches the command as its terminal's own Ctrl-C, and
//!   Ctrl-Z stops the command, as with `script`. When the keys are not ours (stdin is a file
//!   or a pipe), only our terminal's output processing is turned off, so the screen gets the
//!   bytes the capture gets; a background job changes nothing.
//! - The window size is copied whenever it changes (checked every 250 ms).
//! - A poll loop copies the keyboard to the pseudo-terminal, and its output to our stdout and,
//!   through a secrets::Stream, to the capture.
//! - Signals that reach `run` itself (kill, or a terminal that hangs up) are passed on to the
//!   command's process group: it is in a session of its own, so they would not reach it.
//! - When the command has ended, its last output is read (our copy of the terminal's other
//!   side is closed first, so the end is seen), for at most 2 s.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::capture;
use crate::run::{Done, wait_for};
use crate::secrets::{Extra, Hider, Stream};
use crate::sys_run::{self, POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT, PollFd, SIGCHLD, Signals, Termios};

/// How often the window size is compared with ours.
const SIZE_EVERY: Duration = Duration::from_millis(250);

/// The longest the command's last output is read for, once it has ended (output of
/// processes it left running is not waited for).
const LAST_OUTPUT: Duration = Duration::from_secs(2);

/// Keys read but not yet taken by the pseudo-terminal: at most this many bytes.
const KEYS_MAX: usize = 64 << 10;

/// Our terminal's settings changed while the command runs, restored when dropped.
struct TermGuard {
    fd: i32,
    orig: Termios,
    restore: std::ffi::c_int,
}

impl TermGuard {
    /// Raw mode, on the terminal the keys come from: restored with TCSAFLUSH, as `script`
    /// does, so the settings come back exactly as they were (keys the command never read are
    /// dropped with it).
    fn raw(fd: i32) -> Option<TermGuard> {
        let orig = sys_run::term_mode(fd)?;
        sys_run::set_term_mode(fd, sys_run::TCSANOW, &sys_run::raw_mode(&orig)).ok()?;
        Some(TermGuard { fd, orig, restore: sys_run::TCSAFLUSH })
    }

    /// Output processing off, on the screen's terminal when the keys are not ours (stdin is a
    /// file or a pipe): the command's own terminal has already turned its "\n" into "\r\n",
    /// so ours passes its bytes as they are, and the capture equals what reaches the screen.
    /// Keys still make signals. Restored once the output has gone out.
    fn plain_output(fd: i32) -> Option<TermGuard> {
        let orig = sys_run::term_mode(fd)?;
        sys_run::set_term_mode(fd, sys_run::TCSANOW, &sys_run::no_output_processing(&orig)).ok()?;
        Some(TermGuard { fd, orig, restore: sys_run::TCSADRAIN })
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = sys_run::set_term_mode(self.fd, self.restore, &self.orig);
    }
}

/// The command's output: [0] the pseudo-terminal (to our stdout), [1] its stderr pipe when
/// it has one (to our stderr). Each goes out as it is, and through its own secrets::Stream to
/// the capture.
struct Out<'a> {
    cap: &'a mut capture::Writer,
    streams: [Stream; 2],
    /// our stdout and stderr still take the output (a terminal that is gone does not)
    pass: [bool; 2],
    hidden: Vec<u8>,
}

impl Out<'_> {
    /// Bytes of stream `s`; false when its reader is gone (stderr's: the pipe is closed then,
    /// so the command sees what it would see without `run`).
    fn take(&mut self, s: usize, data: &[u8]) -> bool {
        let mut open = true;
        if self.pass[s]
            && let Err(e) = sys_run::write_all_fd(1 + s as i32, data)
        {
            self.pass[s] = false;
            open = !(s == 1 && e.kind() == io::ErrorKind::BrokenPipe);
        }
        self.streams[s].push(data, &mut self.hidden);
        self.cap.write(s == 1, &self.hidden);
        self.hidden.clear();
        open
    }

    fn finish(&mut self, s: usize) {
        self.streams[s].finish(&mut self.hidden);
        self.cap.write(s == 1, &self.hidden);
        self.hidden.clear();
    }
}

/// One read of an output: Some(true) data taken, Some(false) the end, None nothing now.
fn read_into(fd: i32, s: usize, buf: &mut [u8], out: &mut Out) -> Option<bool> {
    match sys_run::read_fd(fd, buf) {
        Ok(0) => Some(false),
        Ok(n) => Some(out.take(s, &buf[..n])),
        Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => None,
        // (EIO on the pseudo-terminal: no process holds its other side any more)
        Err(_) => Some(false),
    }
}

/// Run the command on a pseudo-terminal. Err only when it cannot be started.
pub fn run_pty(argv: &[OsString], cap: &mut capture::Writer, extras: &[Extra], sig: &Signals) -> io::Result<Done> {
    let pty = sys_run::open_pty()?;
    let m = pty.master.as_raw_fd();
    let s = pty.slave.as_raw_fd();
    // the command's terminal starts as ours: its settings and its size
    if let Some(t) = sys_run::term_mode(0).or_else(|| sys_run::term_mode(1)) {
        let _ = sys_run::set_term_mode(s, sys_run::TCSANOW, &t);
    }
    let mut size = sys_run::win_size(1).or_else(|| sys_run::win_size(0));
    if let Some(w) = &size {
        let _ = sys_run::set_win_size(m, w);
    }
    // keys: when our stdin is a terminal, the command reads the pseudo-terminal, and we copy
    // the keyboard to it while we are the terminal's foreground
    let keyboard = sys_run::is_tty(0);
    let foreground = keyboard && sys_run::in_foreground(0);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(if keyboard { Stdio::from(pty.slave.try_clone()?) } else { Stdio::inherit() });
    cmd.stdout(Stdio::from(pty.slave.try_clone()?));
    cmd.stderr(if sys_run::is_tty(2) { Stdio::from(pty.slave.try_clone()?) } else { Stdio::piped() });
    let ctty = if keyboard { 0 } else { 1 };
    // SAFETY: take_terminal makes only async-signal-safe calls (setsid, ioctl)
    unsafe { cmd.pre_exec(move || sys_run::take_terminal(ctty)) };
    let mut child = cmd.spawn()?;
    let group = child.id() as i32;
    let mut errpipe: Option<OwnedFd> = child.stderr.take().map(OwnedFd::from);
    let raw = if foreground { TermGuard::raw(0) } else { None };
    // (a background job leaves the terminal alone: changing it would stop us, SIGTTOU)
    let plain = if raw.is_none() && sys_run::in_foreground(1) { TermGuard::plain_output(1) } else { None };
    let mut note = None;
    let _ = sys_run::set_nonblocking(m, true);
    let hider = || Stream::new(Hider::new(extras.to_vec()));
    let pass = crate::run::PASS_THROUGH.load(std::sync::atomic::Ordering::Relaxed);
    let mut out = Out { cap, streams: [hider(), hider()], pass: [pass, pass], hidden: Vec::new() };
    let mut keys: Vec<u8> = Vec::new();
    let mut read_keys = raw.is_some();
    let mut status: Option<ExitStatus> = None;
    let mut master_open = true;
    // (kept open while the command runs: no output is lost when the command closes its own)
    let mut slave: Option<OwnedFd> = Some(pty.slave);
    let mut buf = vec![0u8; 64 << 10];
    let mut next_size = Instant::now() + SIZE_EVERY;
    loop {
        let wait = next_size.saturating_duration_since(Instant::now()).as_millis() as i32;
        let efd = errpipe.as_ref().map_or(-1, |f| f.as_raw_fd());
        let mut pf = [
            PollFd { fd: if read_keys && keys.len() < KEYS_MAX { 0 } else { -1 }, events: POLLIN, revents: 0 },
            PollFd { fd: if master_open { m } else { -1 }, events: POLLIN | if keys.is_empty() { 0 } else { POLLOUT }, revents: 0 },
            PollFd { fd: efd, events: POLLIN, revents: 0 },
            PollFd { fd: sig.fd(), events: POLLIN, revents: 0 },
        ];
        if let Err(e) = sys_run::poll_fds(&mut pf, wait) {
            note = Some(format!("saving stopped: {e}"));
            break;
        }
        if pf[0].revents != 0 {
            match sys_run::read_fd(0, &mut buf) {
                Ok(0) => read_keys = false,
                Ok(n) => keys.extend_from_slice(&buf[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                // (our terminal is gone)
                Err(_) => read_keys = false,
            }
        }
        if master_open && !keys.is_empty() && pf[1].revents & POLLOUT != 0 {
            match sys_run::write_fd(m, &keys) {
                Ok(n) => {
                    keys.drain(..n);
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(_) => keys.clear(),
            }
        }
        if master_open && pf[1].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL) != 0 && read_into(m, 0, &mut buf, &mut out) == Some(false) {
            master_open = false;
        }
        if efd >= 0 && pf[2].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL) != 0 && read_into(efd, 1, &mut buf, &mut out) == Some(false) {
            out.finish(1);
            errpipe = None;
        }
        for sg in sig.take() {
            if sg == SIGCHLD {
                if status.is_none() {
                    status = child.try_wait().ok().flatten();
                }
            } else {
                // the command is in a session of its own: a signal sent to `run` is passed on
                sys_run::send_signal(-group, sg);
            }
        }
        if Instant::now() >= next_size {
            next_size = Instant::now() + SIZE_EVERY;
            let now = sys_run::win_size(1).or_else(|| sys_run::win_size(0));
            if let Some(w) = now
                && now != size
            {
                // (the system tells the command's foreground with SIGWINCH)
                let _ = sys_run::set_win_size(m, &w);
                size = now;
            }
            // (in case a SIGCHLD was missed)
            if status.is_none() {
                status = child.try_wait().ok().flatten();
            }
        }
        out.cap.tick();
        if status.is_some() || !master_open {
            break;
        }
    }
    // the command's last output: our copy of its terminal is closed, so the end shows
    drop(slave.take());
    let until = Instant::now() + LAST_OUTPUT;
    while (master_open || errpipe.is_some()) && Instant::now() < until {
        let efd = errpipe.as_ref().map_or(-1, |f| f.as_raw_fd());
        let mut pf = [PollFd { fd: if master_open { m } else { -1 }, events: POLLIN, revents: 0 }, PollFd { fd: efd, events: POLLIN, revents: 0 }];
        match sys_run::poll_fds(&mut pf, 100) {
            Ok(1..) => {
                if master_open && pf[0].revents != 0 && read_into(m, 0, &mut buf, &mut out) == Some(false) {
                    master_open = false;
                }
                if efd >= 0 && pf[1].revents != 0 && read_into(efd, 1, &mut buf, &mut out) == Some(false) {
                    out.finish(1);
                    errpipe = None;
                }
            }
            // nothing more for 100 ms (a process the command left running may hold the
            // terminal or the pipe; its output is not waited for)
            _ => break,
        }
    }
    out.finish(0);
    if errpipe.take().is_some() {
        out.finish(1);
    }
    // our terminal back as it was, before the command's status is waited for (a command
    // still running gets SIGHUP when the pseudo-terminal closes)
    drop(raw);
    drop(plain);
    drop(pty.master);
    let status = match status {
        Some(s) => s,
        None => wait_for(&mut child, &mut note),
    };
    let hidden = out.streams.iter().map(|s| s.counts().total()).sum();
    Ok(Done { status, hidden, note })
}
