//! `fewer run [--] COMMAND [ARGS...]`: run a command and save its
//! output as a run of the current folder's project.
//!
//! 1. Saving paused for this project (its `paused` file), or the current folder under a
//!    folder of a `pause = /abs/folder` config line: the command is executed directly
//!    (exec); nothing is saved. So it is when the store cannot be used (no FEWER_HOME, a
//!    store that cannot be written): a warning on stderr, then exec. `run` never keeps the
//!    developer's command from running.
//! 2. A run id is taken and the run recorded: `{kind: run, state: running, start, cmd, cwd,
//!    branch, commit, dirty, tty, writer}` (tags.rs reads git first); the capture
//!    raw/<pid>/rNNNNNN.out is made and locked (capture.rs).
//! 3. Stdout is a terminal: a pseudo-terminal (pty.rs), so colors, progress-bar redraws and
//!    the terminal's "\r\n" reach the capture exactly as they reached the screen. Stdout is
//!    not a terminal (Claude Code's Bash tool, scripts, CI): pipes. stdout and stderr each go
//!    at once to our stdout and stderr, unchanged, and to the capture as whole lines; the
//!    stderr lines are listed in rNNNNNN.err.
//! 4. Secrets are hidden in what goes to the capture (secrets.rs): one
//!    secrets::Stream per output stream, which keeps a part line until its '\n', so a secret
//!    cut by a pause in the output can never reach the capture half hidden. The terminal shows
//!    the original.
//! 5. Ctrl-C: in a pseudo-terminal it is a byte to the command's terminal. In pipes mode the
//!    command is in our process group, so the terminal's Ctrl-C reaches it; `run` catches
//!    SIGINT, SIGTERM, SIGHUP and SIGQUIT (exec resets caught signals in the child, so the
//!    command still gets them) and waits for the command.
//! 6. Arrival marks: rNNNNNN.times (capture.rs).
//! 7. The end: `{state: packing, end, exit or signal, bytes, lines, hidden, note}` is
//!    recorded, then the capture is unlocked, `pack-pending` is started detached (pack.rs),
//!    and `run` exits with the command's status: its exit code, or 128 + the signal that
//!    ended it (127 when it could not be started, as shells do).
//!
//! `run` is dispatched before engine::clear_env_switches(): the command gets the
//! environment exactly as given.
//!
//! On Windows: always pipes (no pseudo-terminal), each pipe read by a thread of its own, and
//! Ctrl-C reaches only the command (it shares the console), so `run` records its end.

use std::ffi::OsString;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
use crate::portable::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use crate::capture;
use crate::json::Json;
use crate::secrets::{Extra, Hider, Stream};
use crate::store::{self, Project};
use crate::sys_run;
#[cfg(unix)]
use crate::sys_run::{POLLERR, POLLHUP, POLLIN, POLLNVAL, PollFd, SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM, Signals};
use crate::tags;

const USAGE: &str = "usage: fewer run [--short] [--] COMMAND [ARGS...]";

const HELP: &str = "\
fewer run - run a command and save its output in the memory store

Usage: fewer run [--short] [--] COMMAND [ARGS...]

The output reaches the terminal as always (with --short it does not: when the command ends,
a short view is printed in its place, for an AI agent: the exit status, the error groups with
a reference to each one's first line, the warnings in brief and the last lines; an output of
up to 40 lines is printed whole). A copy, with secrets hidden, is saved as a run
of the current folder's project, tagged with the command, the folder, the exit status and
git's branch, commit and dirty state, and packed by fewer-pack when the command ends. When
stdout is a terminal the command gets a pseudo-terminal (colors and progress bars are
kept); otherwise its stdout and stderr are read through pipes. run exits with the
command's status (128 + N when signal N ended it).";

/// Whether the command's output reaches our stdout and stderr as it comes (not with --short).
pub static PASS_THROUGH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn pass_through() -> bool {
    PASS_THROUGH.load(std::sync::atomic::Ordering::Relaxed)
}

/// The signals `run` catches (into sys_run's signal pipe).
#[cfg(unix)]
pub const CAUGHT: [i32; 5] = [SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGCHLD];

/// How the command ended, what was hidden from its output, and why saving stopped early when
/// `run` itself ran into a problem (the command was then still waited for).
pub struct Done {
    pub status: ExitStatus,
    pub hidden: u64,
    pub note: Option<String>,
}

/// The command's status, waited for; an exit status of 1 when it cannot be read (with why).
pub fn wait_for(child: &mut std::process::Child, note: &mut Option<String>) -> ExitStatus {
    match child.wait() {
        Ok(s) => s,
        Err(e) => {
            note.get_or_insert_with(|| format!("the command's status could not be read: {e}"));
            ExitStatus::from_raw(1 << 8)
        }
    }
}

/// An output a command printed elsewhere (Claude Code's Bash tool), saved as a run of the
/// current folder's project, as `run` saves one: Ok(None) when saving is paused there; else
/// the run's id and the text as saved (secrets hidden).
pub fn save_output(argv: &[OsString], text: &[u8], exit: Option<i64>) -> Result<Option<(u64, Vec<u8>)>, String> {
    Ok(Setup::open(argv)?.map(|s| s.save(text, exit)))
}

pub fn cmd_run(args: Vec<OsString>) -> i32 {
    let (argv, short) = match parse(args) {
        Ok(a) => a,
        Err(code) => return code,
    };
    // (Claude Code's Bash rules see `fewer run ...`, not the command inside: a command they deny
    // is not run, and one they ask about is left for Claude to run itself, so that Claude Code
    // asks; perms.rs)
    let cwd = std::env::current_dir().unwrap_or_default();
    match crate::perms::Rules::load(&cwd).command(&argv) {
        // (a command has no bounds: Claude Code's Bash rules see `fewer run`)
        crate::perms::Verdict::Free | crate::perms::Verdict::Outside(_) => {}
        crate::perms::Verdict::Deny(rule, file) => {
            eprintln!("fewer run: not run: your permission rule {rule} (in {}) denies this command", crate::perms::tilde(&file));
            return 126;
        }
        crate::perms::Verdict::Ask(rule, file) => {
            eprintln!(
                "fewer run: not run: your permission rule {rule} (in {}) says Claude Code asks you before this command; run it without fewer run, so Claude Code can ask you",
                crate::perms::tilde(&file)
            );
            return 126;
        }
    }
    // (--short: the output is saved, not shown; a short view is printed at the end)
    PASS_THROUGH.store(!short, std::sync::atomic::Ordering::Relaxed);
    match Setup::open(&argv) {
        Ok(Some(s)) => s.go(&argv, short),
        // saving paused: the command alone
        Ok(None) => exec_plain(&argv),
        Err(why) => {
            eprintln!("fewer run: not saving this run: {why}");
            exec_plain(&argv)
        }
    }
}

/// The command to run and whether --short was given, or the exit status of a usage error (2)
/// or of --help (0).
fn parse(mut args: Vec<OsString>) -> Result<(Vec<OsString>, bool), i32> {
    let short = args.first().is_some_and(|a| a.as_encoded_bytes() == b"--short");
    if short {
        args.remove(0);
    }
    let Some(first) = args.first() else {
        eprintln!("fewer run: give the command to run");
        eprintln!("{USAGE}");
        return Err(2);
    };
    let f = first.as_encoded_bytes();
    if f == b"-h" || f == b"--help" {
        println!("{HELP}");
        return Err(0);
    }
    let dashes = f == b"--";
    let argv = if dashes { args[1..].to_vec() } else { args };
    if argv.is_empty() {
        eprintln!("fewer run: give the command to run after --");
        eprintln!("{USAGE}");
        return Err(2);
    }
    let a0 = argv[0].as_encoded_bytes();
    if !dashes && a0.len() > 1 && a0.starts_with(b"-") {
        eprintln!("fewer run: unknown option {} (put -- before a command that starts with '-')", argv[0].to_string_lossy());
        eprintln!("{USAGE}");
        return Err(2);
    }
    Ok((argv, short))
}

/// Run the command in place of this process: nothing is saved. Returns only when the
/// command cannot be started (127 when it is not found, 126 otherwise, as shells do).
fn exec_plain(argv: &[OsString]) -> i32 {
    let e = Command::new(program(&argv[0])).args(&argv[1..]).exec();
    not_started(&argv[0], &e)
}

/// The program to start for a command's first word: the word itself; on Windows a bare name
/// that is no .exe on PATH is looked for there as a .cmd, .bat or .com file too (npm, yarn and
/// many other tools are .cmd files there, which Windows does not start by their bare name).
#[cfg(windows)]
fn program(word: &std::ffi::OsStr) -> OsString {
    let p = Path::new(word);
    if p.components().count() != 1 || p.extension().is_some() {
        return word.to_os_string();
    }
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH").map(|v| std::env::split_paths(&v).filter(|d| d.is_absolute()).collect()).unwrap_or_default();
    for ext in ["exe", "cmd", "bat", "com"] {
        if let Some(found) = dirs.iter().map(|d| d.join(p).with_extension(ext)).find(|f| f.is_file()) {
            // (an .exe: as Windows itself finds it)
            return if ext == "exe" { word.to_os_string() } else { found.into_os_string() };
        }
    }
    word.to_os_string()
}

#[cfg(unix)]
fn program(word: &std::ffi::OsStr) -> OsString {
    word.to_os_string()
}

fn not_started(prog: &std::ffi::OsStr, e: &io::Error) -> i32 {
    eprintln!("fewer run: {}: {e}", prog.to_string_lossy());
    if e.kind() == io::ErrorKind::NotFound { 127 } else { 126 }
}

/// The store's config ("key = value" lines; none when there is no config file).
fn read_config(home: &Path) -> Vec<(String, String)> {
    // (lossily: one byte that is not UTF-8 must not drop every secret shape)
    crate::config::read(home)
}

/// Whether saving is paused for the project `pid` or for the folder `cwd`.
fn paused(home: &Path, pid: &str, cwd: &Path, config: &[(String, String)]) -> bool {
    if home.join("projects").join(pid).join("paused").exists() {
        return true;
    }
    config.iter().filter(|(k, _)| k == "pause").any(|(_, v)| {
        let f = Path::new(v);
        f.is_absolute() && (cwd.starts_with(f) || fs::canonicalize(f).is_ok_and(|r| cwd.starts_with(r)))
    })
}

/// The config's extra secret shapes (`secret = PREFIX CLASS MIN`); a line that cannot be
/// read is told on stderr and not used.
fn extra_secrets(config: &[(String, String)]) -> Vec<Extra> {
    let mut v = Vec::new();
    for (k, val) in config {
        if k == "secret" {
            match Extra::parse(val) {
                Ok(x) => v.push(x),
                Err(e) => eprintln!("fewer run: config line \"secret = {val}\": {e}; this shape is not used"),
            }
        }
    }
    v
}

/// Everything ready to run the command and save its output.
struct Setup {
    project: Project,
    id: u64,
    cap: capture::Writer,
    extras: Vec<Extra>,
    // (Windows: always false; run reads pipes there)
    #[cfg_attr(windows, allow(dead_code))]
    tty: bool,
}

impl Setup {
    /// The store, the project, the run record and the capture; None when saving is paused.
    fn open(argv: &[OsString]) -> Result<Option<Setup>, String> {
        let home = store::home()?;
        let cwd = std::env::current_dir().map_err(|e| format!("cannot read the current folder: {e}"))?;
        // git's tags are read while the project is found (each call has its own 2 s limit)
        let (root, git) = std::thread::scope(|s| {
            let t = s.spawn(|| tags::read(&cwd));
            let root = store::project_root(&cwd);
            (root, t.join().unwrap_or_default())
        });
        let config = read_config(&home);
        let (pid, _) = store::pid_for(&home, &root);
        if paused(&home, &pid, &cwd, &config) {
            return Ok(None);
        }
        let extras = extra_secrets(&config);
        let project = store::ensure_project(&home, &root).map_err(|e| format!("cannot open the memory store {}: {e}", home.display()))?;
        let tty = sys_run::is_tty(1);
        let t0 = Instant::now();
        let mut first = Json::o(vec![
            ("kind", "run".into()),
            ("state", "running".into()),
            ("start", Json::from(crate::times::now_ms())),
            // (the command line with its secrets hidden, as its output: a token in a header, a
            // password=... in an argument or `password = x` over three arguments is not kept or
            // shown)
            ("cmd", Json::Arr(crate::secrets::hide_args(&argv.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), &extras).into_iter().map(Json::from).collect())),
            ("cwd", cwd.to_string_lossy().into_owned().into()),
        ]);
        if let Some(b) = git.branch {
            first.set("branch", b.into());
        }
        if let Some(c) = git.commit {
            first.set("commit", c.into());
        }
        if let Some(d) = git.dirty {
            first.set("dirty", d.into());
        }
        first.set("tty", tty.into());
        first.set("writer", Json::from(std::process::id() as u64));
        let id = store::new_run(&project, first).map_err(|e| format!("cannot write the run table: {e}"))?;
        let cap = match capture::Writer::create(&capture::raw_dir(&home, &project.pid), id, t0) {
            Ok(c) => c,
            Err(e) => {
                let why = format!("cannot write its capture: {e}");
                let _ = store::update_run(&project, id, Json::o(vec![("state", "failed".into()), ("end", Json::from(crate::times::now_ms())), ("note", why.clone().into())]));
                return Err(why);
            }
        };
        Ok(Some(Setup { project, id, cap, extras, tty }))
    }

    /// Save an output that ran elsewhere (Claude Code's Bash tool, through hook.rs): its text,
    /// secrets hidden as run hides them, as this run's capture; its end and exit (when known)
    /// recorded; pack-pending started. The run's id and the text as saved.
    fn save(mut self, text: &[u8], exit: Option<i64>) -> (u64, Vec<u8>) {
        let mut stream = Stream::new(Hider::new(self.extras.clone()));
        let mut hidden = Vec::new();
        stream.push(text, &mut hidden);
        stream.finish(&mut hidden);
        self.cap.write(true, &hidden);
        let sum = self.cap.finish();
        let mut f = Json::o(vec![("state", "packing".into()), ("end", Json::from(crate::times::now_ms())), ("bytes", Json::from(sum.bytes)), ("lines", Json::from(sum.lines)), ("via", "claude-bash".into())]);
        if let Some(e) = exit {
            f.set("exit", Json::from(e));
        }
        let _ = store::update_run(&self.project, self.id, f);
        let id = self.id;
        drop(self.cap);
        if std::env::var_os("FEWER_TEST_NO_PACK").is_none_or(|v| v.is_empty()) {
            let _ = crate::pack::spawn_detached();
        }
        (id, hidden)
    }

    /// Run the command, record its end, start pack-pending; the exit status to return.
    fn go(mut self, argv: &[OsString], short: bool) -> i32 {
        let started = std::time::Instant::now();
        #[cfg(windows)]
        let r = {
            sys_run::leave_ctrl_c_to_the_command();
            run_pipes(argv, &mut self.cap, &self.extras)
        };
        #[cfg(unix)]
        let sig = match Signals::catch(&CAUGHT) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fewer run: not saving this run: cannot catch signals: {e}");
                let _ = store::update_run(&self.project, self.id, Json::o(vec![("state", "failed".into()), ("end", Json::from(crate::times::now_ms())), ("note", format!("cannot catch signals: {e}").into())]));
                let path = self.cap.path();
                drop(self.cap);
                let _ = fs::remove_file(path);
                return exec_plain(argv);
            }
        };
        #[cfg(unix)]
        let r = if self.tty { crate::pty::run_pty(argv, &mut self.cap, &self.extras, &sig) } else { run_pipes(argv, &mut self.cap, &self.extras, &sig) };
        let sum = self.cap.finish();
        let mut f = Json::o(vec![("state", "packing".into()), ("end", Json::from(crate::times::now_ms())), ("bytes", Json::from(sum.bytes)), ("lines", Json::from(sum.lines))]);
        let mut note = sum.note;
        let code = match r {
            Ok(done) => {
                f.set("hidden", Json::from(done.hidden));
                if note.is_none() {
                    note = done.note;
                }
                match (done.status.code(), done.status.signal()) {
                    (Some(c), _) => {
                        f.set("exit", Json::from(c));
                        c
                    }
                    (None, Some(s)) => {
                        f.set("signal", Json::from(s));
                        128 + s
                    }
                    // (stopped or continued: not an end; waitpid without WUNTRACED never says so)
                    _ => 1,
                }
            }
            Err(e) => {
                let c = not_started(&argv[0], &e);
                f.set("hidden", Json::from(0u64));
                f.set("exit", Json::from(c));
                note = Some(format!("could not start: {e}"));
                c
            }
        };
        if let Some(n) = note {
            f.set("note", n.into());
        }
        let by_signal = f.get("signal").is_some();
        if let Err(e) = store::update_run(&self.project, self.id, f) {
            eprintln!("fewer run: cannot record the end of {}/r{}: {e}", self.project.pid, self.id);
        }
        // --short: the view of the saved output, before pack-pending can take the capture
        if short {
            let path = self.cap.path();
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let what = crate::secrets::hide_str(&argv.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" "), &self.extras);
            let now = crate::times::now_ms();
            let secs = started.elapsed().as_secs_f64();
            let tags = crate::catalog::Tags {
                name: what.clone(),
                file_ms: Some(now),
                zone: None,
                arrivals: Some((now - (secs * 1000.0) as i64, capture::read_marks(&dir.join(capture::times_file(self.id))))),
                errs: capture::read_errs(&dir.join(capture::err_file(self.id))),
                files: Vec::new(),
            };
            let status = match code {
                c if c > 128 && by_signal => format!("signal {}", c - 128),
                c => format!("exit {c}"),
            };
            let text = fs::read(&path).unwrap_or_default();
            println!("{}", crate::short::view(&what, &format!("r{}", self.id), &status, Some(secs), &text, &tags, now));
        }
        // (the end is recorded before the capture is unlocked: pack-pending never takes a
        // finished run for one whose writer died)
        drop(self.cap);
        #[cfg(unix)]
        drop(sig);
        if std::env::var_os("FEWER_TEST_NO_PACK").is_none_or(|v| v.is_empty())
            && let Err(e) = crate::pack::spawn_detached()
        {
            eprintln!("fewer run: cannot start pack-pending: {e}; run `fewer pack-pending` to pack {}/r{}", self.project.pid, self.id);
        }
        code
    }
}

// ---------------------------------------------------------------- pipes

/// Pipes mode: the command's stdout and stderr each go at once to ours, unchanged, and to
/// the capture through their own secrets::Stream, as whole lines. Reads until both pipes
/// end (as `tee` does: a background process that keeps them open keeps `run` waiting too).
#[cfg(unix)]
pub fn run_pipes(argv: &[OsString], cap: &mut capture::Writer, extras: &[Extra], sig: &Signals) -> io::Result<Done> {
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut pipes: [Option<OwnedFd>; 2] = [child.stdout.take().map(OwnedFd::from), child.stderr.take().map(OwnedFd::from)];
    let mut streams = [Stream::new(Hider::new(extras.to_vec())), Stream::new(Hider::new(extras.to_vec()))];
    // our stdout and stderr still take the output (not with --short)
    let mut pass = [pass_through(), pass_through()];
    let mut status: Option<ExitStatus> = None;
    let mut note = None;
    let mut buf = vec![0u8; 64 << 10];
    let mut hidden = Vec::new();
    while pipes.iter().any(Option::is_some) {
        let fd = |p: &Option<OwnedFd>| p.as_ref().map_or(-1, |f| f.as_raw_fd());
        let mut pf = [PollFd { fd: fd(&pipes[0]), events: POLLIN, revents: 0 }, PollFd { fd: fd(&pipes[1]), events: POLLIN, revents: 0 }, PollFd { fd: sig.fd(), events: POLLIN, revents: 0 }];
        if let Err(e) = sys_run::poll_fds(&mut pf, 100) {
            // (never seen: the pipes are closed, so the command cannot block on them, and
            // it is waited for below)
            note = Some(format!("saving stopped: {e}"));
            break;
        }
        for s in 0..2 {
            if pf[s].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL) == 0 || pipes[s].is_none() {
                continue;
            }
            let mut close = false;
            match sys_run::read_fd(pf[s].fd, &mut buf) {
                Ok(0) => close = true,
                Ok(n) => {
                    let data = &buf[..n];
                    if pass[s]
                        && let Err(e) = sys_run::write_all_fd(1 + s as i32, data)
                    {
                        pass[s] = false;
                        // our reader is gone: the command's pipe is closed too, so the
                        // command sees what it would see without `run` (SIGPIPE)
                        close = e.kind() == io::ErrorKind::BrokenPipe;
                    }
                    streams[s].push(data, &mut hidden);
                    cap.write(s == 1, &hidden);
                    hidden.clear();
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(_) => close = true,
            }
            if close {
                streams[s].finish(&mut hidden);
                cap.write(s == 1, &hidden);
                hidden.clear();
                pipes[s] = None;
            }
        }
        // SIGCHLD records the status early; the other signals only wake the loop (the
        // command, in our process group, got the terminal's own)
        if sig.take().contains(&SIGCHLD) && status.is_none() {
            status = child.try_wait().ok().flatten();
        }
        cap.tick();
    }
    for (s, p) in pipes.iter_mut().enumerate() {
        if p.take().is_some() {
            streams[s].finish(&mut hidden);
            cap.write(s == 1, &hidden);
            hidden.clear();
        }
    }
    let status = match status {
        Some(s) => s,
        None => wait_for(&mut child, &mut note),
    };
    Ok(Done { status, hidden: streams.iter().map(|s| s.counts().total()).sum(), note })
}

/// Pipes mode on Windows, as on Mac and Linux (above), with a thread reading each pipe:
/// Windows has no poll for pipes. Our stdout and stderr get the bytes as they come (a
/// console that cannot take some, as not UTF-8, gets them with U+FFFD in their place).
#[cfg(windows)]
pub fn run_pipes(argv: &[OsString], cap: &mut capture::Writer, extras: &[Extra]) -> io::Result<Done> {
    use std::io::{Read, Write};
    use std::sync::mpsc::{self, RecvTimeoutError};
    let mut child = Command::new(program(&argv[0])).args(&argv[1..]).stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (tx, rx) = mpsc::channel::<(usize, Option<Vec<u8>>)>();
    let mut pipes: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(o) = child.stdout.take() {
        pipes.push(Box::new(o));
    }
    if let Some(e) = child.stderr.take() {
        pipes.push(Box::new(e));
    }
    for (s, mut pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 << 10];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send((s, Some(buf[..n].to_vec()))).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = tx.send((s, None));
        });
    }
    drop(tx);
    let mut streams = [Stream::new(Hider::new(extras.to_vec())), Stream::new(Hider::new(extras.to_vec()))];
    let mut pass = [pass_through(), pass_through()];
    let mut hidden = Vec::new();
    let show = |s: usize, data: &[u8]| -> io::Result<()> {
        let mut out: Box<dyn Write> = if s == 0 { Box::new(io::stdout().lock()) } else { Box::new(io::stderr().lock()) };
        match out.write_all(data) {
            Err(e) if e.kind() == io::ErrorKind::InvalidData => out.write_all(String::from_utf8_lossy(data).as_bytes()),
            r => r,
        }?;
        out.flush()
    };
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok((s, Some(data))) => {
                if pass[s] && show(s, &data).is_err() {
                    pass[s] = false;
                }
                streams[s].push(&data, &mut hidden);
                cap.write(s == 1, &hidden);
                hidden.clear();
            }
            Ok((s, None)) => {
                streams[s].finish(&mut hidden);
                cap.write(s == 1, &hidden);
                hidden.clear();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        cap.tick();
    }
    let mut note = None;
    let status = wait_for(&mut child, &mut note);
    Ok(Done { status, hidden: streams.iter().map(|s| s.counts().total()).sum(), note })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(parse(os(&["make", "-j4"])), Ok((os(&["make", "-j4"]), false)));
        assert_eq!(parse(os(&["--", "make", "--x"])), Ok((os(&["make", "--x"]), false)));
        assert_eq!(parse(os(&["--", "-weird"])), Ok((os(&["-weird"]), false)));
        assert_eq!(parse(os(&["-"])), Ok((os(&["-"]), false)));
        assert_eq!(parse(os(&[])), Err(2));
        assert_eq!(parse(os(&["--"])), Err(2));
        assert_eq!(parse(os(&["-x", "make"])), Err(2));
        assert_eq!(parse(os(&["--help"])), Err(0));
        // --short first: the output is saved and a short view printed at the end
        assert_eq!(parse(os(&["--short", "cargo", "test"])), Ok((os(&["cargo", "test"]), true)));
        assert_eq!(parse(os(&["--short", "--", "make", "-j4"])), Ok((os(&["make", "-j4"]), true)));
        assert_eq!(parse(os(&["--short"])), Err(2));
        assert_eq!(parse(os(&["--", "--short"])), Ok((os(&["--short"]), false)));
    }

    #[test]
    fn pause_lines_and_secret_shapes() {
        let d = crate::capture::tests::tmpdir("pause");
        let cfg = store::read_settings(&format!("# saving\npause = {}\nsecret = acme_ alnum 20\nsecret = bad\npause = relative/x\n", d.join("paused-here").display()));
        fs::create_dir_all(d.join("paused-here").join("sub")).unwrap();
        assert!(paused(&d, "p-0000", &d.join("paused-here").join("sub"), &cfg));
        assert!(!paused(&d, "p-0000", &d.join("elsewhere"), &cfg));
        fs::create_dir_all(d.join("projects").join("p-0000")).unwrap();
        fs::write(d.join("projects").join("p-0000").join("paused"), "").unwrap();
        assert!(paused(&d, "p-0000", &d.join("elsewhere"), &cfg));
        let x = extra_secrets(&cfg);
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].prefix, "acme_");
        fs::remove_dir_all(&d).unwrap();
    }
}
