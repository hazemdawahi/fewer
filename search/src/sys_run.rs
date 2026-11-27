//! The C library calls `run` needs that std does not offer: the
//! pseudo-terminal (posix_openpt, grantpt, unlockpt, ptsname_r), terminal modes (tcgetattr,
//! cfmakeraw, tcsetattr), window sizes and the controlling terminal (ioctl), poll, signals
//! caught into a pipe, setsid and kill. For macOS and Linux; tested on macOS.
//!
//! Every call is a thin wrapper: errors come back as io::Error, and nothing here keeps state
//! except the signal pipe ([`Signals`]).

use std::ffi::{CStr, OsStr, c_char, c_int, c_short, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicI32, Ordering};

/// A terminal's settings, kept as the C library's `struct termios` without reading its
/// fields (72 bytes on macOS, 60 on Linux): only tcgetattr, cfmakeraw and tcsetattr touch it.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct Termios([u8; 256]);

/// A terminal's size (`struct winsize`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// One entry of poll's table (`struct pollfd`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PollFd {
    pub fd: c_int,
    pub events: c_short,
    pub revents: c_short,
}

pub const POLLIN: c_short = 0x1;
pub const POLLOUT: c_short = 0x4;
pub const POLLERR: c_short = 0x8;
pub const POLLHUP: c_short = 0x10;
pub const POLLNVAL: c_short = 0x20;

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;
#[cfg(target_os = "macos")]
pub const SIGCHLD: i32 = 20;
#[cfg(not(target_os = "macos"))]
pub const SIGCHLD: i32 = 17;

/// tcsetattr: now; once the output written so far has gone out; or that, dropping keys not
/// read yet (as `script` restores a terminal: BSD kernels then leave no PENDIN flag behind).
pub const TCSANOW: c_int = 0;
pub const TCSADRAIN: c_int = 1;
pub const TCSAFLUSH: c_int = 2;

#[cfg(target_os = "macos")]
mod consts {
    pub type NFds = std::ffi::c_uint;
    pub const O_RDWR: i32 = 0x2;
    pub const O_NOCTTY: i32 = 0x20000;
    pub const O_NONBLOCK: i32 = 0x4;
    pub const TIOCSCTTY: std::ffi::c_ulong = 0x2000_7461;
    pub const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
    pub const TIOCSWINSZ: std::ffi::c_ulong = 0x8008_7467;
}
#[cfg(not(target_os = "macos"))]
mod consts {
    pub type NFds = std::ffi::c_ulong;
    pub const O_RDWR: i32 = 0o2;
    pub const O_NOCTTY: i32 = 0o400;
    pub const O_NONBLOCK: i32 = 0o4000;
    pub const TIOCSCTTY: std::ffi::c_ulong = 0x540E;
    pub const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
    pub const TIOCSWINSZ: std::ffi::c_ulong = 0x5414;
}
use consts::*;

const F_SETFD: c_int = 2;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const FD_CLOEXEC: c_int = 1;

unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname_r(fd: c_int, buf: *mut c_char, len: usize) -> c_int;
    fn isatty(fd: c_int) -> c_int;
    fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, when: c_int, t: *const Termios) -> c_int;
    fn cfmakeraw(t: *mut Termios);
    fn tcgetpgrp(fd: c_int) -> c_int;
    fn getpgrp() -> c_int;
    fn setsid() -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn poll(fds: *mut PollFd, n: NFds, timeout: c_int) -> c_int;
    fn pipe(fds: *mut c_int) -> c_int;
    // (the same declarations as sys.rs's)
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn signal(sig: c_int, handler: usize) -> usize;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    #[cfg(target_os = "macos")]
    fn __error() -> *mut c_int;
    #[cfg(not(target_os = "macos"))]
    fn __errno_location() -> *mut c_int;
}

fn check(r: c_int) -> io::Result<c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

// ---------------------------------------------------------------- the pseudo-terminal

/// A new pseudo-terminal: the side `run` reads and writes (master), and the terminal the
/// command gets (slave). Both are closed in child processes unless given to them as stdio.
pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

pub fn open_pty() -> io::Result<Pty> {
    // SAFETY: posix_openpt only makes a new descriptor
    let m = check(unsafe { posix_openpt(O_RDWR | O_NOCTTY) })?;
    // SAFETY: m is a new descriptor that nothing else owns
    let master = unsafe { OwnedFd::from_raw_fd(m) };
    // SAFETY: fcntl(F_SETFD), grantpt and unlockpt only change the descriptor's settings
    check(unsafe { fcntl(m, F_SETFD, FD_CLOEXEC) })?;
    check(unsafe { grantpt(m) })?;
    check(unsafe { unlockpt(m) })?;
    let mut name = [0 as c_char; 256];
    // SAFETY: ptsname_r writes at most name.len() bytes, ending with a 0
    let r = unsafe { ptsname_r(m, name.as_mut_ptr(), name.len()) };
    if r != 0 {
        // (macOS returns -1 and sets errno; glibc returns the error number)
        return Err(if r == -1 { io::Error::last_os_error() } else { io::Error::from_raw_os_error(r) });
    }
    // SAFETY: ptsname_r wrote a 0-terminated name into the buffer
    let path = OsStr::from_bytes(unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes()).to_os_string();
    let slave = OpenOptions::new().read(true).write(true).custom_flags(O_NOCTTY).open(&path)?;
    Ok(Pty { master, slave: slave.into() })
}

/// In the child, between fork and exec: start a new session and make the terminal on `fd`
/// its controlling terminal, so that the terminal's Ctrl-C reaches it.
/// Only async-signal-safe calls.
pub fn take_terminal(fd: RawFd) -> io::Result<()> {
    // SAFETY: setsid and ioctl(TIOCSCTTY) only change this process's session and terminal
    check(unsafe { setsid() })?;
    check(unsafe { ioctl(fd, TIOCSCTTY, 0 as c_int) })?;
    Ok(())
}

/// In a child, between fork and exec: a new session with no terminal (a detached process).
pub fn new_session() -> io::Result<()> {
    // SAFETY: setsid only changes this process's session
    check(unsafe { setsid() }).map(|_| ())
}

// ---------------------------------------------------------------- terminal settings

pub fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty only reads the descriptor's kind
    unsafe { isatty(fd) == 1 }
}

/// The settings of the terminal on `fd` (None when it is not a terminal).
pub fn term_mode(fd: RawFd) -> Option<Termios> {
    let mut t = Termios([0; 256]);
    // SAFETY: tcgetattr writes one struct termios, which fits the 256-byte buffer
    (unsafe { tcgetattr(fd, &mut t) } == 0).then_some(t)
}

pub fn set_term_mode(fd: RawFd, when: c_int, t: &Termios) -> io::Result<()> {
    // SAFETY: tcsetattr reads one struct termios from the buffer
    check(unsafe { tcsetattr(fd, when, t) }).map(|_| ())
}

/// The raw form of terminal settings: no echo, no line editing, no signals from keys, no
/// output processing (cfmakeraw).
pub fn raw_mode(t: &Termios) -> Termios {
    let mut r = *t;
    // SAFETY: cfmakeraw changes the struct termios in the buffer
    unsafe { cfmakeraw(&mut r) };
    r
}

/// The same settings with output processing off (OPOST cleared in c_oflag), everything else
/// kept: bytes written go out as they are, while keys still make signals. `struct termios`
/// starts with c_iflag, then c_oflag: tcflag_t is an unsigned long on macOS (c_oflag at byte
/// 8, 8 bytes) and an unsigned int on Linux (byte 4, 4 bytes); OPOST is bit 0 on both.
pub fn no_output_processing(t: &Termios) -> Termios {
    // (OPOST is bit 0 of c_oflag's first byte: arm64 and x86-64 are little-endian)
    #[cfg(target_os = "macos")]
    const OFLAG: usize = 8;
    #[cfg(not(target_os = "macos"))]
    const OFLAG: usize = 4;
    let mut r = *t;
    r.0[OFLAG] &= !1u8;
    r
}

/// Whether this process is in the foreground of the terminal on `fd` (a background job must
/// not change the terminal's settings or read from it).
pub fn in_foreground(fd: RawFd) -> bool {
    // SAFETY: tcgetpgrp and getpgrp only read process groups
    let fg = unsafe { tcgetpgrp(fd) };
    fg > 0 && fg == unsafe { getpgrp() }
}

pub fn win_size(fd: RawFd) -> Option<WinSize> {
    let mut w = WinSize::default();
    // SAFETY: TIOCGWINSZ writes one struct winsize
    (unsafe { ioctl(fd, TIOCGWINSZ, &mut w as *mut WinSize) } == 0).then_some(w)
}

pub fn set_win_size(fd: RawFd, w: &WinSize) -> io::Result<()> {
    // SAFETY: TIOCSWINSZ reads one struct winsize
    check(unsafe { ioctl(fd, TIOCSWINSZ, w as *const WinSize) }).map(|_| ())
}

pub fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: F_GETFL and F_SETFL only change the descriptor's flags
    let fl = check(unsafe { fcntl(fd, F_GETFL) })?;
    let new = if on { fl | O_NONBLOCK } else { fl & !O_NONBLOCK };
    if new != fl {
        check(unsafe { fcntl(fd, F_SETFL, new) })?;
    }
    Ok(())
}

// ---------------------------------------------------------------- poll, read, write, kill

/// poll: the number of entries with events; a signal while waiting gives 0.
pub fn poll_fds(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    // SAFETY: poll reads and writes the table it is given, of that length
    let r = unsafe { poll(fds.as_mut_ptr(), fds.len() as NFds, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(0) } else { Err(e) };
    }
    Ok(r as usize)
}

/// A file over a descriptor this code does not own: never closed by it.
fn borrowed(fd: RawFd) -> ManuallyDrop<File> {
    // SAFETY: the caller keeps `fd` open while the File is used; ManuallyDrop never closes it
    ManuallyDrop::new(unsafe { File::from_raw_fd(fd) })
}

/// One read(2); a signal gives Interrupted, a non-blocking descriptor with nothing to read
/// gives WouldBlock.
pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    (&*borrowed(fd)).read(buf)
}

/// One write(2).
pub fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    (&*borrowed(fd)).write(buf)
}

/// Write all of `buf`, waiting (poll) when the descriptor is non-blocking and full.
pub fn write_all_fd(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match write_fd(fd, buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut p = [PollFd { fd, events: POLLOUT, revents: 0 }];
                poll_fds(&mut p, 1000)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Send a signal to a process (`pid` > 0) or to a process group (-pgid). False when it
/// could not be sent (no such process).
pub fn send_signal(pid: i32, sig: i32) -> bool {
    // SAFETY: kill only sends a signal
    unsafe { kill(pid, sig) == 0 }
}

// ---------------------------------------------------------------- signals

/// The write end of the signal pipe, for the handler (-1: none).
static SIG_WRITE: AtomicI32 = AtomicI32::new(-1);

fn errno_place() -> *mut c_int {
    // SAFETY: both return this thread's errno
    #[cfg(target_os = "macos")]
    unsafe {
        __error()
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        __errno_location()
    }
}

/// The handler: writes the signal's number into the pipe. It only reads and restores errno
/// and calls write(2), which are async-signal-safe.
extern "C" fn on_signal(sig: c_int) {
    let e = errno_place();
    // SAFETY: errno is this thread's; write(2) on a non-blocking pipe never blocks
    unsafe {
        let saved = *e;
        let fd = SIG_WRITE.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = sig as u8;
            write(fd, (&b as *const u8).cast(), 1);
        }
        *e = saved;
    }
}

/// Signals caught into a pipe (the "self-pipe"): each signal caught writes one byte, its
/// number, so a `poll` loop wakes up and handles it in order with everything else. A caught
/// signal is reset to its default action by exec, so child processes still get the usual
/// behavior ("exec resets caught signals in the child").
pub struct Signals {
    read: OwnedFd,
    _write: OwnedFd,
}

impl Signals {
    /// Catch `sigs` from now on (once per process).
    pub fn catch(sigs: &[i32]) -> io::Result<Signals> {
        let mut fds = [-1 as c_int; 2];
        // SAFETY: pipe writes two new descriptors into the array
        check(unsafe { pipe(fds.as_mut_ptr()) })?;
        // SAFETY: both descriptors are new and owned here
        let (read, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [read.as_raw_fd(), write_end.as_raw_fd()] {
            // SAFETY: fcntl(F_SETFD) only marks the descriptor close-on-exec
            check(unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) })?;
            set_nonblocking(fd, true)?;
        }
        SIG_WRITE.store(write_end.as_raw_fd(), Ordering::SeqCst);
        for &s in sigs {
            // SAFETY: on_signal is async-signal-safe (see above)
            if unsafe { signal(s, on_signal as *const () as usize) } == usize::MAX {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Signals { read, _write: write_end })
    }

    /// The descriptor to poll for POLLIN.
    pub fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// The signals caught since the last call, in order.
    pub fn take(&self) -> Vec<i32> {
        let mut out = Vec::new();
        let mut b = [0u8; 64];
        while let Ok(n) = read_fd(self.read.as_raw_fd(), &mut b) {
            if n == 0 {
                break;
            }
            out.extend(b[..n].iter().map(|&s| s as i32));
        }
        out
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        // (a signal after this must not write into a descriptor that may be reused)
        SIG_WRITE.store(-1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pty_carries_bytes_both_ways() {
        let p = open_pty().unwrap();
        let (m, s) = (p.master.as_raw_fd(), p.slave.as_raw_fd());
        assert!(is_tty(s) && !is_tty(-1));
        // raw on both sides: bytes pass unchanged
        let t = term_mode(s).unwrap();
        set_term_mode(s, TCSANOW, &raw_mode(&t)).unwrap();
        let sent = b"a\r\nb\x1b[31mc\n";
        write_all_fd(s, sent).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < sent.len() {
            let mut f = [PollFd { fd: m, events: POLLIN, revents: 0 }];
            assert_eq!(poll_fds(&mut f, 2000).unwrap(), 1);
            let n = read_fd(m, &mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, sent);
        write_all_fd(m, b"key").unwrap();
        let mut f = [PollFd { fd: s, events: POLLIN, revents: 0 }];
        assert_eq!(poll_fds(&mut f, 2000).unwrap(), 1);
        let n = read_fd(s, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"key");
        // window sizes go through the master
        let w = WinSize { rows: 33, cols: 101, xpixel: 0, ypixel: 0 };
        set_win_size(m, &w).unwrap();
        assert_eq!(win_size(s), Some(w));
        // a cooked terminal turns "\n" into "\r\n" on the way out
        set_term_mode(s, TCSANOW, &t).unwrap();
        write_all_fd(s, b"x\n").unwrap();
        let mut got = Vec::new();
        while got.len() < 3 {
            let mut f = [PollFd { fd: m, events: POLLIN, revents: 0 }];
            assert_eq!(poll_fds(&mut f, 2000).unwrap(), 1);
            let n = read_fd(m, &mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"x\r\n");
        // output processing off, the rest kept: "\n" passes unchanged, the line mode stays
        set_term_mode(s, TCSANOW, &no_output_processing(&t)).unwrap();
        write_all_fd(s, b"y\n").unwrap();
        let mut got = Vec::new();
        while got.len() < 2 {
            let mut f = [PollFd { fd: m, events: POLLIN, revents: 0 }];
            assert_eq!(poll_fds(&mut f, 2000).unwrap(), 1);
            let n = read_fd(m, &mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"y\n");
        let now = term_mode(s).unwrap();
        assert!(now.0 != t.0 && no_output_processing(&now).0 == now.0, "only OPOST changed");
        set_nonblocking(m, true).unwrap();
        assert_eq!(read_fd(m, &mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }
}
