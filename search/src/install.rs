//! `fewer install [--dry-run] [--yes]` and `fewer uninstall [--dry-run]
//! [--yes]`: register fewer's MCP server with Claude Code, turn fewer on and save Claude
//! Code's chats, all under the developer's one yes; and take it all back out.
//!
//! install:
//! 1. Find Claude Code's `claude` command: $FEWER_CLAUDE_BIN (a path, or a name looked up on
//!    PATH), by default `claude` on PATH. In development (FEWER_DEV), claude is run only from
//!    FEWER_CLAUDE_BIN: without it, install says how to set it and exits 2. Not found: print
//!    the command to run later; exit 1.
//! 2. `claude mcp get fewer`: when it exits 0 the server is registered already (in any
//!    scope), so say so, show what claude says about it, and stop.
//! 3. Show exactly what will change, and the command that changes it.
//! 4. Ask "Register it? [y/N]": only y or yes (in any case) goes ahead. stdin is not a
//!    terminal and there is no --yes: say why, exit 1. --dry-run prints only.
//! 5. Run `claude mcp add --scope user fewer [-e FEWER_HOME=<path>] -- <abs>/fewer
//!    mcp` and show its output. On failure, print the command so the developer can run it.
//! 6. Save Claude Code's chats under the same yes (said with the other changes before the
//!    question): `sessions = on` in the store's config, `sessions.claude = on` when a line there
//!    says otherwise, and every other agent's `sessions.<agent> = off` unless the config names it
//!    (install is for Claude Code). Not when they are saved already or Claude Code's folder is
//!    not known.
//! 7. Turn fewer on under the same yes (savers.rs), after showing
//!    what it changes in Claude Code's settings.json: the token savers (three hooks and an
//!    earlier compaction), a smaller start, and fewer's lean agents (agents/fewer-lean.md,
//!    fewer-lean-workflow.md and fewer-worker.md). Also when fewer is registered already; not again when it is on
//!    already. A copy of the file as it was is kept next to it; `fewer off` takes it
//!    back. The file is the one in Claude Code's folder (FEWER_CLAUDE_DIR, else its usual place;
//!    in development without FEWER_CLAUDE_DIR, nothing is changed).
//! 8. One line of use: open a new chat (or reopen an old one: a chat takes its setup when it
//!    opens), and ask Claude to run the tests and say what fails
//!
//! uninstall: steps 1 to 5 for `claude mcp remove --scope user fewer` ("Remove it? [y/N]"),
//! and the same yes takes the token savers out of Claude Code's settings.json (a copy kept);
//! the store and its config stay as they are. It finds claude as install does.
//!
//! Nothing else changes in Claude Code: no CLAUDE.md line. claude runs with its stdin on /dev/null, so it never reads an answer meant for us, and
//! its stdout and stderr go through one pipe, so its output is shown whole and in order after
//! it ends (and at most 2 s later: a process it leaves running never keeps install waiting).
//! It has no time limit of its own: the developer can stop it with Ctrl-C, and cutting it
//! short could stop it while it writes Claude Code's settings. The tests run a stub
//! (tests/data/claude-stub), never the real claude.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config;
use crate::savers;
use crate::sessions::{self, Places};
use crate::store;

/// The name of fewer's MCP server in Claude Code.
pub(crate) const NAME: &str = "fewer";

/// The line of use an install ends with.
pub const USE_LINE: &str = "\nNext: in Claude Code, open a new chat (or close and reopen an old one) and ask it to run your tests and tell you what fails\n  fewer tokens   the tokens fewer saved\n  fewer off      turn fewer off (fewer on turns it back on)";

const INSTALL_HELP: &str = "\
fewer install - put fewer into Claude Code: its settings, its MCP server, your chats saved

Usage: fewer install [claude] [--dry-run] [--yes]

After your yes, registers fewer's MCP server in Claude Code's user scope:
  claude mcp add --scope user fewer [-e FEWER_HOME=<store>] -- <this program> mcp
and turns fewer on: Claude Code's main agent becomes fewer's lean agent, the token savers
(hooks) and a smaller start go into settings.json, and three agents' files into agents/ (a copy
of settings.json is kept; fewer off takes it all back out). All of it is shown
first, under one question: \"Register fewer and turn it on (the changes above)?\", and so is
the last change the same yes makes: Claude Code's chats saved on this computer (with secrets
hidden), for the token card and the start card (to stop: fewer memory sessions off).
Nothing else in Claude Code changes. Run again (fewer update does), it brings fewer's files up
to this version and leaves fewer on or off as you set it.

  --dry-run   print what would change; change nothing
  --yes       answer the question yes (for scripts and agents)

Settings (environment): FEWER_HOME (the memory store, default ~/.fewer), FEWER_CLAUDE_BIN (the
claude command, default the claude on PATH) and FEWER_CLAUDE_DIR (Claude Code's folder, default
~/.claude or $CLAUDE_CONFIG_DIR).";

const UNINSTALL_HELP: &str = "\
fewer uninstall - take fewer out of Claude Code and remove it

Usage: fewer uninstall [claude] [--dry-run] [--yes]

After your yes, runs: claude mcp remove --scope user fewer, and takes fewer out of Claude
Code's settings.json (a copy of the file is kept), its agents' files and its slash commands.
Then it removes the programs install.sh put in (fewer and fewer-pack) and the PATH line it
added, and asks whether to delete what fewer saved too (kept with --yes, or with no one to
ask). When claude is not found, fewer is still taken out and it says how to remove the MCP
server later. The exit status is 0 when all of it was taken out; 3 when Claude Code was not
found; 1 for a no, or no one to ask.

  --dry-run   print what would change; change nothing
  --yes       remove without asking (for scripts)

Settings (environment): FEWER_CLAUDE_BIN (the claude command, default the claude on PATH) and
FEWER_CLAUDE_DIR (Claude Code's folder, holding settings.json, default ~/.claude).";

/// The options of install and uninstall.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Opts {
    /// print what would change; change nothing
    pub dry_run: bool,
    /// the developer's yes given ahead (scripts): registered or removed without asking
    pub yes: bool,
}

/// What install and uninstall need from the system, read once (the tests make their own).
pub struct Setup {
    /// the claude command, or why it was not found; None in development when
    /// FEWER_CLAUDE_BIN is not set (claude is then not looked for at all)
    pub claude: Option<Result<PathBuf, String>>,
    /// how printed commands name claude: FEWER_CLAUDE_BIN as given, else "claude"
    pub claude_shown: OsString,
    /// fewer's own path: the MCP server's command
    pub me: Result<PathBuf, String>,
    /// the memory store's folder (its config holds `sessions`)
    pub home: Result<PathBuf, String>,
    /// FEWER_HOME as given, passed to the server when it is set
    pub home_var: Option<OsString>,
    /// the agents' folders, for the sessions question's words (never read here)
    pub places: Places,
}

impl Setup {
    pub fn from_env() -> Setup {
        let var = crate::defaults::setting("FEWER_CLAUDE_BIN");
        Setup {
            claude: var.as_deref().map(|v| find_claude(v, std::env::var_os("PATH").as_deref())),
            claude_shown: var.unwrap_or_else(|| "claude".into()),
            me: own_path(),
            home: store::home(),
            home_var: std::env::var_os("FEWER_HOME").filter(|v| !v.is_empty()),
            places: Places::from_env(),
        }
    }
}

/// Why install and uninstall stop before anything runs when, in development, FEWER_CLAUDE_BIN is
/// not set: claude is then run only from it, so no development run starts the real claude, which
/// writes into the real home. Outside development, `claude` on PATH is the default.
fn no_claude_bin(cmd: &str) -> String {
    format!(
        "fewer {cmd}: FEWER_CLAUDE_BIN is not set. In development (FEWER_DEV), fewer runs Claude Code's `claude` only from FEWER_CLAUDE_BIN, never from PATH by itself. Set it to the claude program and run this again, for example:\n  FEWER_CLAUDE_BIN=\"$(command -v claude)\" fewer {cmd}"
    )
}

/// The conversation: answers are read from `input`; everything is written to `out` (claude's
/// output too), except why nothing was done, which goes to `err`. `asks`: stdin is a
/// terminal, so there is someone to ask.
pub struct Talk<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub asks: bool,
}

impl Talk<'_> {
    /// Ask a question with a yes-or-no answer: only y or yes (any case, spaces around) is a
    /// yes; any other answer, and the end of input, is a no.
    fn ask(&mut self, question: &str) -> io::Result<bool> {
        write!(self.out, "{question} [y/N] ")?;
        self.out.flush()?;
        let mut answer = Vec::new();
        if self.input.read_until(b'\n', &mut answer)? == 0 {
            // (the end of input: what comes next starts on a line of its own)
            writeln!(self.out)?;
        }
        Ok(is_yes(&String::from_utf8_lossy(&answer)))
    }

    /// A command line on a line of its own, indented, ready to copy.
    fn command(&mut self, to_err: bool, line: &[u8]) -> io::Result<()> {
        let w: &mut dyn Write = if to_err { &mut *self.err } else { &mut *self.out };
        w.write_all(b"  ")?;
        w.write_all(line)?;
        w.write_all(b"\n")
    }

    /// The same, indented further (under a numbered part of a screen).
    fn command_under(&mut self, line: &[u8]) -> io::Result<()> {
        self.out.write_all(b"       ")?;
        self.out.write_all(line)?;
        self.out.write_all(b"\n")
    }

    /// claude's output, as it wrote it (a last line without '\n' gets one).
    fn claude_said(&mut self, text: &[u8]) -> io::Result<()> {
        self.out.write_all(text)?;
        if !text.is_empty() && !text.ends_with(b"\n") {
            self.out.write_all(b"\n")?;
        }
        Ok(())
    }
}

/// How wide the screens people read are (a command to copy is never cut).
const WIDTH: usize = 96;

/// A path as the screens write it: the home folder as ~ (a command to copy keeps its whole path).
fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && h != "/" && (s == h || s.starts_with(&format!("{h}/"))) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

/// Text wrapped at WIDTH columns: its first line after `first`, the others after `rest`.
fn wrapped(first: &str, rest: &str, text: &str) -> String {
    let mut out = String::new();
    let mut line = first.to_string();
    let mut empty = true;
    for w in text.split(' ').filter(|w| !w.is_empty()) {
        if !empty && line.chars().count() + 1 + w.chars().count() > WIDTH {
            out.push_str(line.trim_end());
            out.push('\n');
            line = rest.to_string();
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(w);
        empty = false;
    }
    out.push_str(line.trim_end());
    out
}

/// Whether an answer is a yes: y or yes, in any case, with spaces around.
pub fn is_yes(answer: &str) -> bool {
    let a = answer.trim();
    a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

pub fn cmd_install(args: Vec<OsString>) -> i32 {
    match parse("install", args, INSTALL_HELP) {
        Ok(o) => {
            let s = Setup::from_env();
            let code = with_terminal(|t| install(&s, o, t));
            if code == 0 && !o.dry_run {
                record_places(&s);
            }
            code
        }
        Err(code) => code,
    }
}

/// Where fewer came from and where it was put (FEWER_FROM: the folder install.sh built it in, or
/// "release" for the ready-made programs get.sh downloaded; FEWER_BIN_DIR; FEWER_PATH_FILE when
/// the folder was added to PATH), into the store's config once fewer is in Claude Code (a no to
/// install's question changes nothing): `fewer update` builds or downloads again, `fewer
/// uninstall` removes the programs and that PATH line.
fn record_places(s: &Setup) {
    let Ok(home) = &s.home else { return };
    if config::get(&config::read(home), "savers.on").is_none() {
        return;
    }
    for (var, key) in [("FEWER_FROM", "install.from"), ("FEWER_BIN_DIR", "install.bin"), ("FEWER_PATH_FILE", "install.path_file")] {
        if let Some(v) = std::env::var_os(var).and_then(|v| v.into_string().ok()).filter(|v| v.starts_with('/') || (key == "install.from" && v == "release")) {
            let _ = config::set(home, key, &v);
        }
    }
}

pub fn cmd_uninstall(args: Vec<OsString>) -> i32 {
    match parse("uninstall", args, UNINSTALL_HELP) {
        Ok(o) => {
            let s = Setup::from_env();
            // (read first: taking fewer out clears its record)
            let cfg = s.home.as_ref().map(|h| config::read(h)).unwrap_or_default();
            let code = with_terminal(|t| uninstall(&s, o, t));
            if (code == 0 || code == 3) && !o.dry_run {
                return with_terminal(|t| remove_the_rest(&s, o, &cfg, t)).max(if code == 3 { 3 } else { 0 });
            }
            code
        }
        Err(code) => code,
    }
}

/// After fewer is out of Claude Code: the programs install.sh put in (only fewer and fewer-pack,
/// only in the folder it recorded), the PATH line it added (only its own two lines), and, asked
/// in a terminal (never with --yes), what fewer saved (`memory clear`, then its settings).
fn remove_the_rest(s: &Setup, o: Opts, cfg: &[(String, String)], t: &mut Talk) -> io::Result<i32> {
    if let Some(bin) = config::get(cfg, "install.bin").map(PathBuf::from) {
        let mut gone = Vec::new();
        for name in ["fewer", "fewer-pack"] {
            let p = bin.join(name);
            if p.is_file() {
                match fs::remove_file(&p) {
                    Ok(()) => gone.push(name),
                    Err(e) => writeln!(t.err, "fewer uninstall: {} could not be removed: {e}", p.display())?,
                }
            }
        }
        if !gone.is_empty() {
            writeln!(t.out, "✓ Removed {} from {}.", gone.join(" and "), tilde(&bin))?;
        }
        if let Some(rc) = config::get(cfg, "install.path_file").map(PathBuf::from)
            && let Ok(text) = fs::read_to_string(&rc)
        {
            let line = format!("export PATH=\"{}:$PATH\"", bin.display());
            let mut out: Vec<&str> = Vec::new();
            let mut lines = text.lines().peekable();
            let mut took = false;
            while let Some(l) = lines.next() {
                if l.starts_with("# fewer: its programs on PATH") && lines.peek() == Some(&line.as_str()) {
                    lines.next();
                    // (the empty line install.sh put before them)
                    if out.last() == Some(&"") {
                        out.pop();
                    }
                    took = true;
                    continue;
                }
                out.push(l);
            }
            if took {
                let mut new = out.join("\n");
                if text.ends_with('\n') && !new.is_empty() {
                    new.push('\n');
                }
                match fs::write(&rc, new) {
                    Ok(()) => writeln!(t.out, "✓ Took fewer's PATH line out of {}.", tilde(&rc))?,
                    Err(e) => writeln!(t.err, "fewer uninstall: {}: {e}", rc.display())?,
                }
            }
        }
    }
    let Ok(home) = &s.home else { return Ok(0) };
    if !home.join("config").exists() && !home.join("projects").exists() {
        return Ok(0);
    }
    if !o.yes && t.asks && t.ask(&format!("Also delete what fewer saved in {}?", tilde(home)))? {
        let code = crate::memory::clear_all(home);
        if code == 0 {
            let _ = fs::remove_file(home.join("config"));
            let _ = fs::remove_dir(home);
            writeln!(t.out, "✓ Deleted what fewer saved.")?;
        }
        return Ok(code);
    }
    writeln!(t.out, "Kept what fewer saved, in {} (to delete it: rm -r {}).", tilde(home), tilde(home))?;
    Ok(0)
}

/// Where the one-line install's script is: each release publishes it as install.sh.
pub const GET_URL: &str = "https://github.com/hazemdawahi/fewer/releases/latest/download/install.sh";

/// `fewer update`: the new version, the way this one came (the ready-made programs downloaded
/// again by the release's install.sh; else git pull where install.sh built it, built and
/// installed again), Claude Code's side brought up to it (`--yes`: the update was asked for;
/// fewer stays on or off as it was).
pub fn cmd_update(args: Vec<OsString>) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fewer update\n\nGets the new version of Fewer the way this one came: the ready-made programs downloaded from\nthe latest release ({GET_URL}), or a git pull and a build in the\nfolder install.sh built it from. Then it brings Claude Code's side up to it. Your settings and\nwhat Fewer saved are kept. It goes online only for that download or git pull.");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("usage: fewer update");
        return 2;
    }
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fewer update: {e}");
            return 2;
        }
    };
    let cfg = config::read(&home);
    // (the ready-made programs: downloaded again, to the folder they are in)
    if config::get(&cfg, "install.from") == Some("release") {
        println!("Getting the new version of Fewer...");
        let mut sh = std::process::Command::new("sh");
        sh.arg("-c").arg(format!("curl -fsSL {GET_URL} | sh -s -- --yes"));
        if let Some(bin) = config::get(&cfg, "install.bin") {
            sh.env("FEWER_BIN_DIR", bin);
        }
        return match sh.status() {
            Ok(st) => st.code().unwrap_or(1),
            Err(e) => {
                eprintln!("fewer update: sh could not be run: {e}");
                1
            }
        };
    }
    let Some(from) = config::get(&cfg, "install.from").map(PathBuf::from).filter(|f| f.join("install.sh").is_file()) else {
        eprintln!("fewer update: where Fewer came from is not known (its installer records it). Install it again: curl -fsSL {GET_URL} | sh");
        return 1;
    };
    println!("Getting the new version (git pull in {})...", tilde(&from));
    match std::process::Command::new("git").arg("-C").arg(&from).args(["pull", "--ff-only"]).status() {
        Ok(st) if st.success() => {}
        Ok(_) => {
            eprintln!("fewer update: git pull did not work in {}; nothing was changed (changes of your own there? git status shows them)", from.display());
            return 1;
        }
        Err(e) => {
            eprintln!("fewer update: git could not be run: {e}");
            return 1;
        }
    }
    let mut sh = std::process::Command::new("sh");
    sh.arg(from.join("install.sh")).arg("--yes");
    if let Some(prefix) = config::get(&cfg, "install.bin").map(Path::new).and_then(Path::parent) {
        sh.arg(prefix);
    }
    match sh.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fewer update: install.sh could not be run: {e}");
            1
        }
    }
}

/// The options, or the exit status of a usage error (2) or of --help (0).
fn parse(cmd: &str, args: Vec<OsString>, help: &str) -> Result<Opts, i32> {
    let usage = |msg: String| {
        eprintln!("fewer {cmd}: {msg}");
        eprintln!("usage: fewer {cmd} [claude] [--dry-run] [--yes]");
        Err(2)
    };
    let mut o = Opts::default();
    let mut target = None;
    for a in &args {
        match a.to_str() {
            Some("--dry-run") => o.dry_run = true,
            Some("--yes") => o.yes = true,
            Some("-h" | "--help") => {
                println!("{help}");
                return Err(0);
            }
            Some(t) if !t.starts_with('-') && target.is_none() => target = Some(t),
            _ => return usage(format!("unknown argument {}", a.to_string_lossy())),
        }
    }
    match target {
        Some("claude") => Ok(o),
        Some(t) => usage(format!("{t}: only claude (Claude Code) in this version")),
        // (Claude Code, the only agent for now)
        None => Ok(o),
    }
}

/// Talk with our own stdin, stdout and stderr; the exit status.
fn with_terminal(f: impl FnOnce(&mut Talk) -> io::Result<i32>) -> i32 {
    let stdin = io::stdin();
    let asks = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = io::stdout();
    let mut err = io::stderr();
    let mut t = Talk { input: &mut input, out: &mut out, err: &mut err, asks };
    match f(&mut t) {
        Ok(code) => code,
        Err(e) => {
            // (our own output or input failed: a closed stdout, a terminal gone)
            eprintln!("fewer: {e}");
            1
        }
    }
}

/// fewer's own path, as the system gives it, made absolute. A link's path is kept, not
/// resolved, so a package manager's link keeps working after an upgrade.
fn own_path() -> Result<PathBuf, String> {
    std::env::current_exe().and_then(std::path::absolute).map_err(|e| format!("cannot find fewer's own path: {e}"))
}

/// The claude command FEWER_CLAUDE_BIN names: a path as it is, or a name looked up on PATH.
/// Only absolute PATH folders are looked in, so a `claude` in the current folder is never run.
pub(crate) fn find_claude(var: &OsStr, path: Option<&OsStr>) -> Result<PathBuf, String> {
    if var.as_bytes().contains(&b'/') {
        let p = PathBuf::from(var);
        return if is_program(&p) { Ok(p) } else { Err(format!("FEWER_CLAUDE_BIN={} is not a program", p.display())) };
    }
    let found = path.into_iter().flat_map(std::env::split_paths).filter(|d| d.is_absolute()).map(|d| d.join(var)).find(|p| is_program(p));
    // (the usual `claude` not found: Claude Code is not installed, or not on PATH)
    if found.is_none() && var == "claude" && !crate::defaults::dev() && std::env::var_os("FEWER_CLAUDE_BIN").is_none_or(|v| v.is_empty()) {
        return Err("Claude Code's `claude` was not found on PATH: install Claude Code first, or set FEWER_CLAUDE_BIN to the claude program".into());
    }
    found.ok_or_else(|| format!("FEWER_CLAUDE_BIN={} was not found on PATH", var.to_string_lossy()))
}

/// A file with an execute bit: a program that can be run.
fn is_program(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn strings(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// `claude mcp add`'s arguments:
///   mcp add --scope user fewer [-e FEWER_HOME=<path>] -- <abs>/fewer mcp
/// `-e` comes after the server's name, as in claude's own example (`claude mcp add my-server
/// -e API_KEY=xxx -- npx my-mcp-server`): claude's `-e` takes every argument after it up to
/// the next one that starts with '-' (commander's `-e, --env <env...>`, read from Claude Code
/// 2.1.287's own code), so in the order `-e FEWER_HOME=<path> fewer --` it would take
/// `fewer` as a second variable and fail: "Invalid environment variable format: fewer".
pub fn add_args(home_var: Option<&OsStr>, me: &Path) -> Vec<OsString> {
    let mut v = strings(&["mcp", "add", "--scope", "user", NAME]);
    if let Some(h) = home_var {
        let mut e = OsString::from("FEWER_HOME=");
        e.push(h);
        v.push("-e".into());
        v.push(e);
    }
    v.push("--".into());
    v.push(me.into());
    v.push("mcp".into());
    v
}

/// `claude mcp get fewer`: it exits 0 when the server is registered, in any scope.
fn get_args() -> Vec<OsString> {
    strings(&["mcp", "get", NAME])
}

/// `claude mcp remove --scope user fewer`.
fn remove_args() -> Vec<OsString> {
    strings(&["mcp", "remove", "--scope", "user", NAME])
}

/// A command line as a shell reads it: each argument as it is when it holds only ASCII
/// letters, digits and `@%+=:,./_-`, else in single quotes ('\'' for a quote inside). Bytes
/// that are not UTF-8 stay as they are.
pub fn shell_line(prog: &OsStr, args: &[OsString]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, a) in std::iter::once(prog).chain(args.iter().map(OsString::as_os_str)).enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        let a = a.as_bytes();
        if !a.is_empty() && a.iter().all(|&c| c.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&c)) {
            out.extend_from_slice(a);
        } else {
            out.push(b'\'');
            for &c in a {
                if c == b'\'' {
                    out.extend_from_slice(b"'\\''");
                } else {
                    out.push(c);
                }
            }
            out.push(b'\'');
        }
    }
    out
}

/// How long claude's output is still read once claude has ended: a process it started and
/// left running with the pipe open (its MCP health check starts our server) never keeps
/// install waiting longer.
const AFTER_END: Duration = Duration::from_secs(2);

/// Run claude with these arguments: what it wrote to stdout and stderr, in order (one pipe
/// for both), and how it ended. Its stdin is /dev/null.
fn run_claude(claude: &Path, args: &[OsString]) -> io::Result<(Vec<u8>, ExitStatus)> {
    let (mut r, w) = io::pipe()?;
    let w2 = w.try_clone()?;
    // (the Command, and with it our copies of the pipe's writing end, is gone after this
    // line, so the pipe ends when claude and what it started have closed theirs)
    let mut child = Command::new(claude).args(args).stdin(Stdio::null()).stdout(w).stderr(w2).spawn()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    let status = child.wait()?;
    let end = Instant::now() + AFTER_END;
    let mut out = Vec::new();
    // until the pipe ends (the reader is gone) or AFTER_END has passed
    while let Ok(chunk) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        out.extend_from_slice(&chunk);
    }
    Ok((out, status))
}

/// How a command ended, in words.
fn ended(st: ExitStatus) -> String {
    match (st.code(), st.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(s)) => format!("ended by signal {s}"),
        _ => "ended".into(),
    }
}

/// Whether claude knows a server named fewer (`claude mcp get fewer` exits 0), with what it
/// said; an error when claude could not be run.
fn registered(claude: &Path) -> io::Result<Option<Vec<u8>>> {
    let (text, st) = run_claude(claude, &get_args())?;
    Ok(st.success().then_some(text))
}

/// The line install shows about Claude Code's chats, with the other changes before its question.
fn chats_line(s: &Setup, home: &Path, file: &Path, saved_already: bool, saves: bool) -> String {
    let text = if saved_already {
        format!("Your Claude Code chats: saved already (sessions = on in {}).", tilde(file))
    } else if saves {
        let read = s.places.of("claude").as_ref().map(|d| tilde(&sessions::read_in("claude", d))).unwrap_or_default();
        format!(
            "Save your Claude Code chats ({read}) on this computer too, in {}, with secrets hidden: the token card and the start card come from them (sessions = on in {}; to stop: fewer memory sessions off).",
            tilde(home),
            tilde(file)
        )
    } else {
        "Your Claude Code chats: not saved, as Claude Code's folder is not known (FEWER_CLAUDE_DIR).".to_string()
    };
    wrapped("  3. ", "     ", &text)
}

/// `install claude`, after its options: the exit status (0 registered, or nothing to do, or
/// the developer said no; 1 not done; 2 no store, or in development no FEWER_CLAUDE_BIN).
pub fn install(s: &Setup, o: Opts, t: &mut Talk) -> io::Result<i32> {
    let home = match &s.home {
        Ok(h) => h,
        Err(e) => {
            writeln!(t.err, "fewer install: {e}")?;
            return Ok(2);
        }
    };
    // (in development, claude only from FEWER_CLAUDE_BIN)
    let Some(found) = &s.claude else {
        writeln!(t.err, "{}", no_claude_bin("install"))?;
        return Ok(2);
    };
    let me = match &s.me {
        Ok(m) => m,
        Err(e) => {
            writeln!(t.err, "fewer install: {e}")?;
            return Ok(1);
        }
    };
    let args = add_args(s.home_var.as_deref(), me);
    let line = shell_line(&s.claude_shown, &args);
    // 1. claude
    let claude = match found {
        Ok(c) => c,
        Err(why) => {
            writeln!(t.out, "{why}. When Claude Code is installed, run fewer install again (or, for fewer's MCP server alone: the command below).")?;
            t.command(false, &line)?;
            return Ok(1);
        }
    };
    // 2. registered already
    match registered(claude) {
        Ok(Some(said)) => {
            writeln!(t.out, "Claude Code has an MCP server named fewer already (claude mcp get fewer):")?;
            t.claude_said(&said)?;
            // (another program registered, an install into another folder: said, so that Claude
            // Code does not run two versions, the release check's upgrade test)
            let mine = me.as_os_str().as_bytes();
            if !said.windows(mine.len().max(1)).any(|w| w == mine) {
                writeln!(t.out, "It runs another fewer than this one ({}). To register this one: fewer uninstall, then fewer install.", me.display())?;
            } else {
                writeln!(t.out, "It is left as it is.")?;
            }
            let ok = again_tenx(s, o, t, me)?;
            if o.dry_run {
                writeln!(t.out, "Dry run: nothing was changed.")?;
            } else {
                slash_step(s, t, me)?;
            }
            return Ok(if ok { 0 } else { 1 });
        }
        // (fewer on: Claude Code hides fewer's own MCP server (an early version: every server) and
        // refuses to add it; an update leaves the registration as it is: Claude runs fewer's
        // commands instead, and `fewer off` shows it again. Seen on a real machine)
        Ok(None) if savers::settings_file(&s.places).ok().and_then(|f| savers::read(&f).ok()).is_some_and(|(_, now)| savers::hides_own_server(&now)) => {
            writeln!(t.out, "fewer's MCP server is left as it is: fewer keeps it off (Claude runs fewer's commands instead), and Claude Code neither shows nor adds it then (fewer off shows it again).")?;
            let ok = again_tenx(s, o, t, me)?;
            if o.dry_run {
                writeln!(t.out, "Dry run: nothing was changed.")?;
            } else {
                slash_step(s, t, me)?;
            }
            return Ok(if ok { 0 } else { 1 });
        }
        Ok(None) => {}
        Err(e) => {
            writeln!(t.err, "fewer install: cannot run {}: {e}. To register fewer yourself, run:", claude.display())?;
            t.command(true, &line)?;
            return Ok(1);
        }
    }
    // 3. what will change, in three parts, before the one question that covers them
    writeln!(t.out, "fewer will make these changes in Claude Code{}:", if o.dry_run { " (a dry run: nothing is changed now)" } else { "" })?;
    writeln!(t.out)?;
    writeln!(t.out, "  1. Register fewer's MCP server for all your projects (user scope):")?;
    t.command_under(&line)?;
    writeln!(t.out)?;
    // (and fewer's changes, before the question that covers them: they were said only after
    // the yes, the release check of 2026-11-25)
    tenx_step(s, Opts { dry_run: true, yes: o.yes }, t, me, Plan::Preview)?;
    let file = config::path(home);
    let cfg = config::read(home);
    // Claude Code's chats, saved by install itself, said here
    // with the other changes, before the one question that covers them: saved already when the
    // config turns Claude's on; none when Claude Code's folder is not known
    let saved_already = sessions::agents_on(&cfg, &mut Vec::new()).contains(&"claude");
    let saves = s.places.of("claude").is_ok() && !saved_already;
    writeln!(t.out)?;
    writeln!(t.out, "{}", chats_line(s, home, &file, saved_already, saves))?;
    writeln!(t.out)?;
    if let Ok(settings) = savers::settings_file(&s.places) {
        let commands = settings.parent().map(|d| d.join("commands")).unwrap_or_default();
        writeln!(t.out, "  4. Add fewer's slash commands to Claude Code ({}): /fewer-tokens, /fewer-off,\n     /fewer-on and /fewer-memory. Only you run them: Claude is not told about them.", tilde(&commands))?;
        writeln!(t.out)?;
    }
    // 4. the developer's yes
    if o.dry_run {
        writeln!(t.out, "Dry run: nothing was changed. `fewer install` asks \"Register fewer and turn it on (the changes above)? [y/N]\" and makes them on a yes; `fewer off` takes fewer back out.")?;
        return Ok(0);
    }
    if !o.yes {
        if !t.asks {
            writeln!(t.err, "fewer install: not registered: stdin is not a terminal, so no one can answer \"Register fewer and turn it on (the changes above)? [y/N]\". Run it in a terminal, add --yes, or run the command above yourself (fewer's MCP server alone).")?;
            return Ok(1);
        }
        if !t.ask("Register fewer and turn it on (the changes above)?")? {
            writeln!(t.out, "Not registered. Nothing was changed.")?;
            return Ok(0);
        }
    }
    // 5. register
    match run_claude(claude, &args) {
        Ok((said, st)) => {
            t.claude_said(&said)?;
            if !st.success() {
                writeln!(t.err, "fewer install: claude mcp add failed ({}). To register fewer yourself, run:", ended(st))?;
                t.command(true, &line)?;
                return Ok(1);
            }
            writeln!(t.out, "✓ fewer's MCP server is registered.")?;
        }
        Err(e) => {
            writeln!(t.err, "fewer install: cannot run {}: {e}. To register fewer yourself, run:", claude.display())?;
            t.command(true, &line)?;
            return Ok(1);
        }
    }
    // 6. Claude Code's chats, as said before the question: sessions on, Claude's own line on, and
    // every other agent off unless the config names it (install is for Claude Code)
    if saves {
        let mut r = config::set(home, "sessions", "on");
        if r.is_ok() && config::get(&cfg, "sessions.claude").is_some_and(|v| v != "on") {
            r = config::set(home, "sessions.claude", "on");
        }
        for (agent, _) in sessions::SETTINGS {
            let key = format!("sessions.{agent}");
            if r.is_ok() && agent != "claude" && config::get(&cfg, &key).is_none() {
                r = config::set(home, &key, "off");
            }
        }
        if let Err(e) = r {
            writeln!(t.err, "fewer install: registered, but `sessions = on` could not be written to {}: {e}", file.display())?;
            return Ok(1);
        }
        writeln!(t.out, "✓ Your Claude Code chats will be saved (sessions = on in {}).", tilde(&file))?;
    }
    // 7. fewer, its changes shown already
    if !tenx_step(s, o, t, me, Plan::Shown)? {
        return Ok(1);
    }
    slash_step(s, t, me)?;
    // 8. what to do next
    writeln!(t.out, "{USE_LINE}")?;
    Ok(0)
}

/// fewer's slash commands into Claude Code's folder (install, and again on an update: brought up
/// to this version); one there that is not fewer's is said and left alone.
fn slash_step(s: &Setup, t: &mut Talk, me: &Path) -> io::Result<()> {
    let Ok(settings) = savers::settings_file(&s.places) else { return Ok(()) };
    let dir = settings.parent().map(Path::to_path_buf).unwrap_or_default();
    let Some(me) = me.to_str() else { return Ok(()) };
    let Some(home) = s.home_var.as_deref().map(|h| h.to_str()).map_or(Some(None), |h| h.map(Some)) else { return Ok(()) };
    match savers::write_slash(&dir, home, me) {
        Ok(()) => writeln!(t.out, "✓ fewer's slash commands are in Claude Code: type /fewer in a chat (/fewer-tokens, /fewer-off, /fewer-on, /fewer-memory)."),
        Err(e) => writeln!(t.out, "fewer's slash commands were not added: {e}."),
    }
}

/// fewer on a second `install claude` (an update, or the same again): left as the user chose.
/// On: only what is not there yet, its agents' files brought up to this version. Turned off with
/// `fewer off`: left off. Never on: its changes shown and asked about (a terminal, or --yes).
/// Ok(false) when it could not be carried out.
fn again_tenx(s: &Setup, o: Opts, t: &mut Talk, me: &Path) -> io::Result<bool> {
    let cfg: Vec<(String, String)> = s.home.as_ref().map(|h| config::read(h)).unwrap_or_default();
    let got = |k: &str| config::get(&cfg, k).filter(|v| !v.is_empty()).map(str::to_string);
    let on = got("savers.start").is_some() || got("savers.lean").is_some();
    if on {
        return tenx_step(s, o, t, me, Plan::Show);
    }
    if let Some(off) = got("savers.off") {
        writeln!(t.out, "fewer is off, as it was turned off ({}). To turn it on: fewer on", crate::times::local_of_utc(&off))?;
        return Ok(true);
    }
    if o.dry_run {
        return tenx_step(s, o, t, me, Plan::Show);
    }
    tenx_step(s, Opts { dry_run: true, yes: o.yes }, t, me, Plan::Preview)?;
    if !o.yes {
        if !t.asks {
            writeln!(t.out, "fewer is not turned on: stdin is not a terminal, so no one can answer \"Turn fewer on (the changes above)? [y/N]\". To turn it on: fewer on")?;
            return Ok(true);
        }
        if !t.ask("Turn fewer on (the changes above)?")? {
            writeln!(t.out, "fewer is not turned on. To turn it on later: fewer on")?;
            return Ok(true);
        }
    }
    tenx_step(s, o, t, me, Plan::Shown)
}

/// Now as a UTC time the transcripts' times compare with: 2026-11-23T18:02:09.
fn now_utc() -> String {
    let (y, mo, d, h, mi, sec) = crate::times::civil(crate::times::now_ms().div_euclid(1000));
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}")
}

/// 1234567 as "1,234,567".
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Step 7, fewer (savers.rs): the token savers, a smaller start
/// and the lean agents, turned on by install under the yes that registers fewer (a copy of the
/// settings kept, one command to undo): shown, then
/// written in one write of Claude Code's settings.json, a copy of the file kept beside it; not
/// again when it is on already; --dry-run only shows. What fewer put there is recorded in the
/// store's config (savers.start, savers.lean), so `fewer off` (and uninstall) takes
/// back only that. Ok(false) when it could not be carried out.
fn tenx_step(s: &Setup, o: Opts, t: &mut Talk, me: &Path, plan_shown: Plan) -> io::Result<bool> {
    let (dir, file) = match savers::settings_file(&s.places) {
        Ok(f) => (f.parent().map(Path::to_path_buf).unwrap_or_default(), f),
        Err(why) => {
            writeln!(t.out, "fewer goes into Claude Code's settings, but {why}, so it is not turned on.")?;
            return Ok(true);
        }
    };
    let Some(me) = me.to_str() else {
        writeln!(t.out, "fewer is not turned on: this program's path is not UTF-8, so Claude Code's settings cannot name it.")?;
        return Ok(true);
    };
    let Some(home) = s.home_var.as_deref().map(|h| h.to_str()).map_or(Some(None), |h| h.map(Some)) else {
        writeln!(t.out, "fewer is not turned on: FEWER_HOME is not UTF-8, so Claude Code's settings cannot name it.")?;
        return Ok(true);
    };
    let (was, now) = match savers::read(&file) {
        Ok(x) => x,
        Err(e) => {
            writeln!(t.out, "fewer is not turned on: {}: {e}.", file.display())?;
            return Ok(true);
        }
    };
    // how the lean rules name fewer: by its name when PATH finds this one
    let word = savers::command_word(me, std::env::var_os("PATH").as_deref());
    // everything on a copy: the savers, the smaller start, the lean agents
    let mut after = now.clone();
    // (an update from an early version: what it turned off and this version keeps on (the add-ons), taken
    // back first at fewer's own values, and out of the record)
    let old_lean = s.home.as_ref().ok().and_then(|h| config::get(&config::read(h), "savers.lean").map(str::to_string)).unwrap_or_default();
    let (retired, kept_lean) = savers::retired_parts(&old_lean);
    let updated = !savers::remove_lean(&mut after, &retired).is_empty() || !retired.is_empty();
    let skills = savers::skills_found(&dir);
    // (a window the developer set is theirs, whatever its value: kept, never taken back)
    let their_window = now.get(savers::WINDOW_KEY).map(|v| v.to_string());
    let plan = savers::add(&mut after, home, me).and_then(|said| {
        let (workflows, artifact, names) = savers::start_plan(&after, &skills);
        let start = savers::add_start(&mut after, workflows, artifact, &names)?;
        let mut lean = savers::lean_plan(&after, home);
        lean.extend(savers::reads_plan(&after, &word));
        let lean = savers::add_lean(&mut after, &lean, home, their_window.is_some())?;
        Ok((said, names.len(), start, lean))
    });
    let (_, n_skills, start, lean) = match plan {
        Ok(p) => p,
        Err(e) => {
            writeln!(t.out, "fewer is not turned on: {}: {e}, so it is left as it is.", file.display())?;
            return Ok(true);
        }
    };
    if after == now && savers::agents_there(&dir, &word) {
        writeln!(t.out, "fewer is on already in {}.", file.display())?;
        return Ok(true);
    }
    // (on already, its agents' files from another version: only they are written, no copy of a
    // settings file that does not change, the token card's start kept; release check)
    if after == now {
        if o.dry_run || plan_shown == Plan::Preview {
            writeln!(t.out, "fewer is on; its agents' files would be brought up to this version ({} unchanged).", file.display())?;
            return Ok(true);
        }
        if let Err(e) = savers::write_lean_agent(&dir, &word) {
            writeln!(t.err, "fewer: its agents' files could not be written: {e}")?;
            return Ok(false);
        }
        writeln!(t.out, "{}", wrapped("✓ ", "  ", &format!("fewer is on; its agents' files are brought up to this version ({} unchanged): they apply to new chats, and to old ones when you reopen them.", tilde(&file))))?;
        return Ok(true);
    }
    let was_on = s.home.as_ref().map(|h| config::read(h)).is_ok_and(|c| ["savers.start", "savers.lean"].iter().any(|k| config::get(&c, k).is_some_and(|v| !v.is_empty())));
    // (fewer's reading commands allowed: savers::READS)
    let reads = lean.split(',').any(|p| p.starts_with("lean:allow:"));
    let reads_line = format!(
        "fewer's reading commands ({}) run without asking, as Claude Code's Read tool does: they keep your Read permission rules and stay inside the project themselves (a file outside it is left to the Read tool, which asks).",
        savers::READS.join(", ")
    );
    // (on already, from an earlier version: what this version changes, said as such)
    let updating = was_on && (updated || reads);
    if updating && plan_shown != Plan::Shown {
        writeln!(t.out, "{}", wrapped("", "", &format!("fewer is on; this version changes it in {} (a copy of the file is kept first) and in its agents' files:", tilde(&file))))?;
        if updated {
            writeln!(t.out, "{}", wrapped("  - ", "    ", "Your add-ons (MCP servers) and claude.ai's connectors work again, as they do without fewer; only fewer's own MCP server stays off (Claude runs fewer's commands instead)."))?;
            writeln!(t.out, "{}", wrapped("  - ", "    ", "Claude changes files with Claude Code's own Edit and Write tools, so rewind (Esc Esc) and your permission rules work as usual."))?;
        }
        if reads {
            writeln!(t.out, "{}", wrapped("  - ", "    ", &reads_line))?;
        }
    } else if plan_shown != Plan::Shown {
    // (numbered as the second part of install's screen; alone for `fewer on`)
    let numbered = plan_shown == Plan::Preview;
    let (head, rest, b1, b2) = if numbered { ("  2. ", "     ", "       - ", "         ") } else { ("", "", "  - ", "    ") };
    // (no settings.json yet: made, nothing to copy)
    let change = if was.is_some() { format!("it changes {} (a copy of the file is kept first)", tilde(&file)) } else { format!("it makes {} (there is none yet)", tilde(&file)) };
    let what = format!("87% fewer tokens in our tests, with the same answers: {change} and writes fewer's agents' files in {}:", tilde(&dir.join("agents")));
    writeln!(t.out, "{}", wrapped(head, rest, &if numbered { format!("Turn fewer on, {what}") } else { format!("fewer on turns it on, {what}") }))?;
    let bullet = |t: &mut Talk, text: &str| writeln!(t.out, "{}", wrapped(b1, b2, text));
    if lean.split(',').any(|r| r == "lean:agent") {
        bullet(t, &format!("Claude works through fewer's lean agent ({}): Claude Code's Bash, Read, Edit and Write tools and your add-ons, short rules that keep the safety rules, few steps; skills, helpers and workflows on demand (helpers and workflow agents start without your CLAUDE.md files and add-ons, to keep each step small: the job Claude writes for them is all they get). Every file change goes through Edit and Write, so rewind (Esc Esc) and your permission rules work as usual.", tilde(&savers::lean_agent_file(&dir))))?;
    } else {
        let theirs = after.get("agent").and_then(crate::json::Json::str).unwrap_or("?").to_string();
        bullet(t, &format!("Your own agent ({theirs}) stays Claude Code's main agent, so the lean start is not used (the savings need it): to use it, take \"agent\" out of {} and run fewer on.", tilde(&file)))?;
    }
    // (the developer's own window is kept, and said so)
    let compacted = match &their_window {
        Some(v) => format!("long chats compacted as you set it (autoCompactWindow {v})"),
        None => format!("long chats compacted at about {} tokens, with a short summary", group(savers::LEAN_WINDOW - savers::BELOW)),
    };
    bullet(t, &format!("Long output of commands that succeed comes back short (all of it saved); new and compacted chats start with fewer's card; {compacted}."))?;
    let skills_line = if n_skills > 0 { format!(", {n_skills} skills listed by name only") } else { String::new() };
    // (with the developer's own agent, fewer's own server stays on too: savers::lean_plan)
    let add_ons = if lean.split(',').any(|r| r == "lean:deny-own") {
        "; fewer's own MCP server off (Claude runs fewer's commands instead; your other add-ons stay as they are)"
    } else {
        ""
    };
    bullet(t, &format!("A smaller start: the Workflow and Artifact tools off (on demand){skills_line}{add_ons}; no AI-written session titles, no note of the tokens left after each step, and the note on commit lines moved into the Bash tool's own text (the lines are still written)."))?;
    if reads {
        bullet(t, &reads_line)?;
    }
    bullet(t, "Settings you set yourself stay as they are; fewer off takes all of this back out.")?;
    }
    if plan_shown == Plan::Preview {
        return Ok(true);
    }
    if o.dry_run {
        writeln!(t.out, "Dry run: nothing was changed. fewer on turns it on; fewer off turns it off.")?;
        return Ok(true);
    }
    // (what fewer makes itself, no settings.json or agents/ before: taken away again by fewer off,
    // not left as "{}" and an empty folder, the release check)
    let made_file = was.is_none();
    let made_agents = !dir.join("agents").exists();
    if let Err(e) = savers::write_lean_agent(&dir, &word) {
        writeln!(t.err, "fewer: not turned on: {e}")?;
        return Ok(false);
    }
    match savers::write(&file, was.as_deref(), &after) {
        Ok(copy) => {
            if let Ok(h) = &s.home {
                // (the moment fewer went on, the token card's split: kept when it was on
                // already, an update's write must not move it, the release check's upgrade test)
                if !was_on {
                    let _ = config::set(h, "savers.on", &now_utc());
                    // (on again: the token card counts from now, with no end)
                    let _ = config::set(h, "savers.off", "");
                }
                if made_file {
                    let _ = config::set(h, "savers.made", "settings.json");
                }
                if made_agents {
                    let _ = config::set(h, "savers.made_agents", "agents");
                }
                for (key, rec) in [("savers.start", &start), ("savers.lean", &lean)] {
                    let mut old = config::get(&config::read(h), key).unwrap_or("").to_string();
                    // (an earlier version's retired parts, taken back above, leave the record)
                    if key == "savers.lean" && !retired.is_empty() {
                        old = kept_lean.clone();
                    }
                    let merged = match (old.is_empty(), rec.is_empty()) {
                        (_, true) => old,
                        (true, false) => rec.clone(),
                        (false, false) => format!("{old},{rec}"),
                    };
                    let _ = config::set(h, key, &merged);
                }
            }
            let copy = copy.map(|c| format!(" (a copy of the file before this change: {})", tilde(&c))).unwrap_or_default();
            let done = if updating { "fewer is brought up to this version" } else { "fewer is on" };
            writeln!(t.out, "{}", wrapped("✓ ", "  ", &format!("{done}{copy}: it applies to new Claude Code chats, and to old ones when you reopen them.")))?;
            // (`fewer on` alone: what next; install says it after)
            if plan_shown == Plan::Show {
                writeln!(t.out, "  fewer tokens shows the tokens saved; fewer off turns it off.")?;
            }
            Ok(true)
        }
        Err(e) => {
            let _ = savers::remove_lean_agent(&dir);
            writeln!(t.err, "fewer: not turned on: {}: {e}", file.display())?;
            Ok(false)
        }
    }
}

/// How tenx_step shows its plan: `Show` the list first (the `fewer on` command, a dry run), `Shown`
/// (the list was shown before the question that covers it: the result only), `Preview` (the
/// list alone, nothing changed: before that question).
#[derive(Clone, Copy, PartialEq)]
enum Plan {
    Show,
    Shown,
    Preview,
}

/// What fewer put in Claude Code's settings, taken out on a copy: (the file, its bytes as
/// read, the settings after, what was taken out); None when nothing of it is there.
type TakeBack = (PathBuf, Option<Vec<u8>>, crate::json::Json, Vec<String>);

fn take_back_plan(s: &Setup) -> Option<TakeBack> {
    let record = s.home.as_ref().map(|h| config::get(&config::read(h), "savers.start").unwrap_or("").to_string()).unwrap_or_default();
    let lean = s.home.as_ref().map(|h| config::get(&config::read(h), "savers.lean").unwrap_or("").to_string()).unwrap_or_default();
    let (file, (was, now)) = savers::settings_file(&s.places).and_then(|f| savers::read(&f).map(|r| (f, r))).ok()?;
    let mut after = now;
    // (fewer's marks in the file, but no record of it in the store: the store removed, another
    // FEWER_HOME, a settings.json from another machine. What fewer writes is taken out at its own
    // values, so that the add-ons are not left off and Claude Code not pointed at a removed agent)
    let (record, lean) = if record.is_empty() && lean.is_empty() && savers::marked(&after) {
        let word = s.me.as_ref().ok().and_then(|m| m.to_str()).map(|m| savers::command_word(m, std::env::var_os("PATH").as_deref())).unwrap_or_else(|| "fewer".into());
        savers::full_records(&word)
    } else {
        (record, lean)
    };
    // (the window by its value only for an install older than fewer's record)
    let mut said = savers::remove(&mut after, lean.is_empty());
    said.extend(savers::remove_start(&mut after, &record));
    said.extend(savers::remove_lean(&mut after, &lean));
    (!said.is_empty()).then_some((file, was, after, said))
}

/// Take fewer back as planned: the record cleared, fewer's agents' files removed, the
/// settings written (a copy kept). The copy's path, or why it failed.
fn take_back(s: &Setup, (file, was, after, said): &TakeBack) -> Result<Option<PathBuf>, String> {
    let made = s.home.as_ref().map(|h| config::read(h)).unwrap_or_default();
    let made_file = config::get(&made, "savers.made") == Some("settings.json");
    let made_agents = config::get(&made, "savers.made_agents") == Some("agents");
    if let Ok(h) = &s.home {
        // (the period fewer was on, kept for the token card: each period on counts, when it
        // was turned off and on again)
        let cfg = config::read(h);
        let now = now_utc();
        if let Some(on) = config::get(&cfg, "savers.on").filter(|v| !v.is_empty())
            && config::get(&cfg, "savers.off").unwrap_or("").is_empty()
        {
            let was = config::get(&cfg, "savers.periods").unwrap_or("");
            let _ = config::set(h, "savers.periods", &format!("{was}{}{on}..{now}", if was.is_empty() { "" } else { "," }));
        }
        let _ = config::set(h, "savers.off", &now);
        let _ = config::set(h, "savers.start", "");
        let _ = config::set(h, "savers.lean", "");
        let _ = config::set(h, "savers.made", "");
        let _ = config::set(h, "savers.made_agents", "");
    }
    if let Ok(d) = &s.places.claude {
        savers::remove_lean_agent(d);
        if made_agents {
            // (only when empty: a file of the developer's keeps it)
            let _ = std::fs::remove_dir(d.join("agents"));
        }
    }
    // (nothing of fewer in settings.json, only the agents' files: the file is not written)
    if said.is_empty() {
        return Ok(None);
    }
    // (a settings.json fewer made, nothing left in it: removed, no copy of fewer's own settings)
    if made_file && *after == crate::json::Json::Obj(Vec::new()) {
        return std::fs::remove_file(file).map(|_| None).map_err(|e| e.to_string());
    }
    savers::write(file, was.as_deref(), after)
}

const TENX_HELP: &str = "\
fewer on | off [--dry-run] - fewer in Claude Code

  fewer on       turn it on (install does it by itself); when it is on, it says so
  fewer off      turn it off: what fewer put in Claude Code's settings.json comes out,
                 and its agents' files; a copy of the file is kept each time
  --dry-run      show what would change; change nothing

Settings (environment): FEWER_CLAUDE_DIR (Claude Code's folder, holding settings.json, default
~/.claude) and FEWER_HOME (the memory store, which records what fewer put there, default
~/.fewer).";

/// `fewer on | off [--dry-run]`.
pub fn cmd_tenx(args: Vec<OsString>) -> i32 {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if words.iter().any(|w| w == "-h" || w == "--help") {
        println!("{TENX_HELP}");
        return 0;
    }
    let dry_run = words.iter().any(|w| w == "--dry-run");
    let rest: Vec<&str> = words.iter().map(String::as_str).filter(|w| *w != "--dry-run").collect();
    let s = Setup::from_env();
    match rest.as_slice() {
        [] => with_terminal(|t| tenx_status(&s, t)),
        ["on"] => with_terminal(|t| {
            let me = match &s.me {
                Ok(m) => m.clone(),
                Err(e) => {
                    writeln!(t.err, "fewer on: {e}")?;
                    return Ok(1);
                }
            };
            Ok(if tenx_step(&s, Opts { dry_run, yes: true }, t, &me, Plan::Show)? { 0 } else { 1 })
        }),
        ["off"] => with_terminal(|t| tenx_off(&s, dry_run, t)),
        _ => {
            eprintln!("usage: fewer on | off [--dry-run]");
            2
        }
    }
}

/// Whether fewer is on, and where.
fn tenx_status(s: &Setup, t: &mut Talk) -> io::Result<i32> {
    let Ok(file) = savers::settings_file(&s.places) else {
        writeln!(t.out, "fewer: Claude Code's folder is not known (FEWER_CLAUDE_DIR), so it cannot be said.")?;
        return Ok(1);
    };
    let on = take_back_plan(s).is_some() || s.places.claude.as_ref().is_ok_and(|d| savers::lean_agent_file(d).exists());
    let since = s.home.as_ref().ok().and_then(|h| config::get(&config::read(h), "savers.on").map(str::to_string)).filter(|v| !v.is_empty());
    if on {
        let since = since.map(|v| format!(" since {}", crate::times::local_of_utc(&v))).unwrap_or_default();
        writeln!(t.out, "fewer is on in {}{since}. To turn it off: fewer off", file.display())?;
    } else {
        writeln!(t.out, "fewer is off. To turn it on: fewer on")?;
    }
    Ok(0)
}

/// `fewer off`: the one command that takes fewer back (no question: it was asked
/// for by name).
fn tenx_off(s: &Setup, dry_run: bool, t: &mut Talk) -> io::Result<i32> {
    let plan = take_back_plan(s);
    let agents = s.places.claude.as_ref().is_ok_and(|d| savers::lean_agent_file(d).exists() || savers::workflow_agent_file(d).exists() || savers::worker_agent_file(d).exists());
    let Some(plan) = plan.or_else(|| {
        // only the agents' files are left: take them out alone
        agents.then(|| savers::settings_file(&s.places).ok().and_then(|f| savers::read(&f).ok().map(|(was, now)| (f, was, now, Vec::new())))).flatten()
    }) else {
        writeln!(t.out, "fewer is off already.")?;
        return Ok(0);
    };
    if plan.3.is_empty() {
        // (nothing of it in settings.json: the file is left as it is, or not made)
        writeln!(t.out, "{}", wrapped("", "", &format!("fewer off takes out fewer's agents' files (nothing of fewer is in {}):", tilde(&plan.0))))?;
        writeln!(t.out, "  fewer's agents' files.")?;
    } else {
        writeln!(t.out, "{}", wrapped("", "", &format!("fewer off takes fewer out of {} (a copy of the file is kept first):", tilde(&plan.0))))?;
        writeln!(t.out, "{}", wrapped("  ", "  ", &format!("{}, and fewer's agents' files.", plan.3.join(", "))))?;
    }
    if dry_run {
        writeln!(t.out, "Dry run: nothing was changed.")?;
        return Ok(0);
    }
    match take_back(s, &plan) {
        Ok(copy) => {
            let copy = copy.map(|c| format!(" (a copy of the file before this change: {})", tilde(&c))).unwrap_or_default();
            // (the token card counts fewer's replies until now)
            if let Ok(h) = &s.home {
                let _ = config::set(h, "savers.off", &now_utc());
            }
            writeln!(t.out, "{}", wrapped("✓ ", "  ", &format!("fewer is off{copy}. To turn it on again: fewer on")))?;
            Ok(0)
        }
        Err(e) => {
            writeln!(t.err, "fewer off: {}: {e}", plan.0.display())?;
            Ok(1)
        }
    }
}

/// `uninstall claude`, after its options: the exit status (0 removed, or nothing to remove,
/// or the developer said no; 1 not done; 2 in development no FEWER_CLAUDE_BIN).
pub fn uninstall(s: &Setup, o: Opts, t: &mut Talk) -> io::Result<i32> {
    // (in development: claude only from FEWER_CLAUDE_BIN)
    let Some(found) = &s.claude else {
        writeln!(t.err, "{}", no_claude_bin("uninstall"))?;
        return Ok(2);
    };
    let args = remove_args();
    let line = shell_line(&s.claude_shown, &args);
    // (claude not found: fewer is still taken out of settings.json, which needs no claude, and
    // the exit status says the MCP server was not: a program removed after a run that left Claude
    // Code wired to it broke every hook, the release check of 2026-11-25)
    let claude = match found {
        Ok(c) => Some(c),
        Err(why) => {
            // (not "install Claude Code first", as install says it)
            let why = why.split(": install Claude Code first").next().unwrap_or(why);
            writeln!(t.out, "{why}, so fewer's MCP server cannot be taken out of Claude Code now. When Claude Code is there, run:")?;
            t.command(false, &line)?;
            None
        }
    };
    let there = match claude {
        None => false,
        Some(c) => match registered(c) {
            Ok(Some(said)) => {
                writeln!(t.out, "Claude Code has an MCP server named fewer (claude mcp get fewer):")?;
                t.claude_said(&said)?;
                true
            }
            Ok(None) => false,
            Err(e) => {
                writeln!(t.err, "fewer uninstall: cannot run {}: {e}. To remove fewer's MCP server yourself, run:", c.display())?;
                t.command(true, &line)?;
                return Ok(1);
            }
        },
    };
    // (fewer on: Claude Code shows no MCP server, fewer's own too, so it is looked for again
    // once fewer is out. Seen on a real machine)
    let hidden = !there && claude.is_some() && savers::settings_file(&s.places).ok().and_then(|f| savers::read(&f).ok()).is_some_and(|(_, now)| savers::hides_own_server(&now));
    // fewer in Claude Code's settings.json and its agents' files, when any of it is there
    let savers = take_back_plan(s);
    if !there && savers.is_none() {
        // (Claude Code not found, and no fewer settings in its settings: nothing of fewer runs; 3 says
        // the MCP server may still be registered, so that install.sh can remove the programs)
        if claude.is_none() {
            return Ok(3);
        }
        writeln!(t.out, "Claude Code has no MCP server named fewer and no fewer settings: nothing to remove.")?;
        return Ok(0);
    }
    if there {
        writeln!(t.out, "Claude Code, user scope: remove the MCP server fewer")?;
        writeln!(t.out, "The command:")?;
        t.command(false, &line)?;
    } else if hidden {
        writeln!(t.out, "Claude Code, user scope: remove the MCP server fewer once fewer is out (Claude Code shows no MCP server while it is on)")?;
        writeln!(t.out, "The command:")?;
        t.command(false, &line)?;
    }
    if let Some((file, _, _, said)) = &savers {
        // (a settings.json fewer made, nothing else in it: removed, no copy)
        let made = s.home.as_ref().is_ok_and(|h| config::get(&config::read(h), "savers.made") == Some("settings.json"));
        let gone = made && savers::settings_file(&s.places).ok().and_then(|f| take_back_plan(s).filter(|p| p.0 == f)).is_some_and(|p| p.2 == crate::json::Json::Obj(Vec::new()));
        let copy = if gone { "the file goes, as fewer made it and nothing else is in it" } else { "a copy of the file is kept next to it" };
        writeln!(t.out, "Claude Code's settings ({}): take out fewer ({}) and its agents' files; {copy}.", file.display(), said.join(", "))?;
    }
    if o.dry_run {
        writeln!(t.out, "Dry run: nothing was changed.")?;
        return Ok(0);
    }
    // (a no, or no one to ask: exit 1, so that a script never removes the programs while Claude
    // Code still runs them)
    if !o.yes {
        if !t.asks {
            writeln!(t.err, "fewer uninstall: not removed: stdin is not a terminal, so no one can answer \"Remove it? [y/N]\". Run it in a terminal, or add --yes.")?;
            return Ok(1);
        }
        if !t.ask("Remove it?")? {
            writeln!(t.out, "Not removed. Nothing was changed.")?;
            return Ok(1);
        }
    }
    if let (true, Some(c)) = (there, claude) {
        match run_claude(c, &args) {
            Ok((said, st)) => {
                t.claude_said(&said)?;
                if !st.success() {
                    writeln!(t.err, "fewer uninstall: claude mcp remove failed ({}). To remove fewer's MCP server yourself, run:", ended(st))?;
                    t.command(true, &line)?;
                    return Ok(1);
                }
            }
            Err(e) => {
                writeln!(t.err, "fewer uninstall: cannot run {}: {e}. To remove fewer's MCP server yourself, run:", c.display())?;
                t.command(true, &line)?;
                return Ok(1);
            }
        }
    }
    if let Some(plan) = &savers {
        match take_back(s, plan) {
            Ok(Some(copy)) => writeln!(t.out, "fewer is out of {} (a copy of the file before this change: {}).", plan.0.display(), copy.display())?,
            Ok(None) => writeln!(t.out, "fewer is out of {}.", plan.0.display())?,
            Err(e) => {
                writeln!(t.err, "fewer uninstall: fewer could not be taken out of {}: {e}", plan.0.display())?;
                return Ok(1);
            }
        }
    }
    // (the MCP server fewer hid: looked for again now that fewer is out)
    if let (true, Some(c)) = (hidden, claude) {
        match registered(c) {
            Ok(Some(_)) => match run_claude(c, &args) {
                Ok((said, st)) => {
                    t.claude_said(&said)?;
                    if !st.success() {
                        writeln!(t.err, "fewer uninstall: claude mcp remove failed ({}). To remove fewer's MCP server yourself, run:", ended(st))?;
                        t.command(true, &line)?;
                        return Ok(1);
                    }
                }
                Err(e) => {
                    writeln!(t.err, "fewer uninstall: cannot run {}: {e}. To remove fewer's MCP server yourself, run:", c.display())?;
                    t.command(true, &line)?;
                    return Ok(1);
                }
            },
            Ok(None) => writeln!(t.out, "Claude Code has no MCP server named fewer: nothing more to remove.")?,
            Err(e) => {
                writeln!(t.err, "fewer uninstall: cannot run {}: {e}. To remove fewer's MCP server yourself, run:", c.display())?;
                t.command(true, &line)?;
                return Ok(1);
            }
        }
    }
    if let Ok(settings) = savers::settings_file(&s.places)
        && let Some(d) = settings.parent()
        && savers::remove_slash(d)
    {
        writeln!(t.out, "fewer's slash commands are out of {}.", tilde(&d.join("commands")))?;
    }
    // (an install after an uninstall is a first install: fewer turned on again, not "off, as
    // it was turned off"; its periods stay for the token card)
    if let Ok(h) = &s.home {
        let _ = config::set(h, "savers.on", "");
        let _ = config::set(h, "savers.off", "");
    }
    if claude.is_none() {
        // (the command to run later was given above, once)
        return Ok(3);
    }
    writeln!(t.out, "Removed from Claude Code.")?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The text with its wrapped lines joined: each line end, and the spaces around it, one space.
    fn flat(s: &str) -> String {
        s.split('\n').map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
    }

    /// The stand-in for claude (tests/data/claude-stub): no test runs the real claude.
    const STUB: &str = include_str!("../tests/data/claude-stub");

    /// The program that would be registered (never run by the stub).
    const ME: &str = "/opt/fewer/bin/fewer";

    /// A fresh folder for a test, inside this crate's target folder.
    fn tmpdir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-install-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A test's folder with the stub (named claude, in stub/) and a store folder (home/): the
    /// folder, and the setup that uses them.
    fn setup(name: &str) -> (PathBuf, Setup) {
        let d = tmpdir(name);
        fs::create_dir_all(d.join("stub")).unwrap();
        let stub = d.join("stub").join("claude");
        fs::write(&stub, STUB).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let home = d.join("home");
        let s = Setup {
            claude: Some(Ok(stub)),
            claude_shown: "claude".into(),
            me: Ok(PathBuf::from(ME)),
            home: Ok(home.clone()),
            home_var: Some(home.into_os_string()),
            places: places(&[("FEWER_CLAUDE_DIR", d.join("claude").to_str().unwrap())]),
        };
        (d, s)
    }

    /// The agents' places from these settings (the folders are never read by install).
    fn places(settings: &[(&str, &str)]) -> Places {
        Places::with(&|k| settings.iter().find(|(a, _)| *a == k).map(|(_, v)| OsString::from(v)))
    }

    /// The stub's calls, each as its arguments.
    fn calls(d: &Path) -> Vec<Vec<String>> {
        let b = fs::read(d.join("stub").join("calls")).unwrap_or_default();
        b.split(|&c| c == 0x1e).filter(|c| !c.is_empty()).map(|c| c.split(|&x| x == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()).collect()
    }

    fn get_call() -> Vec<String> {
        vec!["mcp".into(), "get".into(), "fewer".into()]
    }

    fn add_call(home: &Path) -> Vec<String> {
        let h = format!("FEWER_HOME={}", home.display());
        ["mcp", "add", "--scope", "user", "fewer", "-e", &h, "--", ME, "mcp"].iter().map(|a| a.to_string()).collect()
    }

    type Flow = fn(&Setup, Opts, &mut Talk) -> io::Result<i32>;

    /// Run install or uninstall with these answers: (exit status, stdout, stderr).
    fn talk(flow: Flow, s: &Setup, o: Opts, asks: bool, answers: &str) -> (i32, String, String) {
        let mut input = Cursor::new(answers.as_bytes().to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = flow(s, o, &mut Talk { input: &mut input, out: &mut out, err: &mut err, asks }).unwrap();
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn config_text(d: &Path) -> Option<String> {
        fs::read_to_string(d.join("home").join("config")).ok()
    }

    /// The config without fewer's records (savers.*), None when nothing else is left.
    fn config_rest(d: &Path) -> Option<String> {
        let t: String = config_text(d)?.lines().filter(|l| !l.starts_with("savers.")).map(|l| format!("{l}\n")).collect();
        (!t.is_empty()).then_some(t)
    }

    #[test]
    fn answers() {
        for a in ["y", "Y", "yes", "YES", "Yes", " yes \n", "y\r\n"] {
            assert!(is_yes(a), "{a:?}");
        }
        for a in ["", "\n", "n", "no", "yess", "ye", "y y", "ok", "oui", "1", "true"] {
            assert!(!is_yes(a), "{a:?}");
        }
    }

    #[test]
    fn the_exact_commands() {
        let args = |v: Vec<OsString>| v.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(args(add_args(Some(OsStr::new("/w/my store")), Path::new(ME))), ["mcp", "add", "--scope", "user", "fewer", "-e", "FEWER_HOME=/w/my store", "--", ME, "mcp"]);
        assert_eq!(args(add_args(None, Path::new(ME))), ["mcp", "add", "--scope", "user", "fewer", "--", ME, "mcp"]);
        assert_eq!(args(get_args()), ["mcp", "get", "fewer"]);
        assert_eq!(args(remove_args()), ["mcp", "remove", "--scope", "user", "fewer"]);
        // as the shell reads them
        let line = shell_line(OsStr::new("claude"), &add_args(Some(OsStr::new("/w/it's here")), Path::new("/a b/fewer")));
        assert_eq!(line, b"claude mcp add --scope user fewer -e 'FEWER_HOME=/w/it'\\''s here' -- '/a b/fewer' mcp");
        assert_eq!(shell_line(OsStr::new("/x/claude"), &[OsString::from(""), OsString::from("a$b"), OsString::from("~x")]), b"/x/claude '' 'a$b' '~x'");
        let raw = OsStr::from_bytes(b"/tmp/\xff");
        assert_eq!(shell_line(raw, &[]), b"'/tmp/\xff'");
    }

    #[test]
    fn a_process_left_running_does_not_keep_us_waiting() {
        // a claude that leaves a process running with its output pipe open: what claude said
        // and how it ended come back once claude has ended (plus AFTER_END), not 20 s later
        let d = tmpdir("left-running");
        let (script, pidfile) = (d.join("claude"), d.join("pid"));
        fs::write(&script, format!("#!/bin/sh\n/bin/sleep 20 &\necho $! > '{}'\necho said\necho also >&2\nexit 3\n", pidfile.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let t0 = Instant::now();
        let r = run_claude(&script, &[]);
        let waited = t0.elapsed();
        // (the sleep is stopped first, so nothing is left running whatever the result)
        let pid = fs::read_to_string(&pidfile).unwrap();
        Command::new("/bin/kill").arg(pid.trim()).status().unwrap();
        let (out, st) = r.unwrap();
        assert_eq!((out.as_slice(), st.code()), (&b"said\nalso\n"[..], Some(3)));
        assert!(waited < Duration::from_secs(15), "{waited:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn finds_claude() {
        let d = tmpdir("find");
        let (a, b) = (d.join("a"), d.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        // a `claude` that cannot be run is passed over; the first one that can is taken
        fs::write(a.join("claude"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(a.join("claude"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(b.join("claude"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(b.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([a.as_path(), Path::new("relative/dir"), b.as_path()]).unwrap();
        let path = Some(path.as_os_str());
        let name = OsStr::new("claude");
        assert_eq!(find_claude(name, path), Ok(b.join("claude")));
        // a relative folder on PATH is never looked in (this one holds a claude that can be
        // run, seen from the tests' working folder, the crate's root)
        let rel = d.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().join("b");
        assert!(rel.is_relative() && is_program(&rel.join("claude")));
        let rel = std::env::join_paths([rel.as_path(), a.as_path()]).unwrap();
        assert!(find_claude(name, Some(rel.as_os_str())).unwrap_err().contains("FEWER_CLAUDE_BIN=claude was not found on PATH"));
        assert!(find_claude(name, None).is_err());
        // a path is taken as it is, a name is looked up on PATH
        assert_eq!(find_claude(b.join("claude").as_os_str(), None), Ok(b.join("claude")));
        assert!(find_claude(a.join("claude").as_os_str(), path).unwrap_err().contains("is not a program"));
        assert!(find_claude(d.join("none").as_os_str(), path).unwrap_err().contains("is not a program"));
        assert!(find_claude(OsStr::new("claude-x"), path).unwrap_err().contains("FEWER_CLAUDE_BIN=claude-x was not found on PATH"));
        // a folder is not a program
        assert!(find_claude(b.as_os_str(), None).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_yes_and_yes() {
        let (d, s) = setup("yes-yes");
        let (code, out, err) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!((code, err.as_str()), (0, ""));
        let home = d.join("home");
        assert_eq!(calls(&d), [get_call(), add_call(&home)]);
        // what changes is shown before the question, then claude's own output
        let h = home.display();
        assert!(out.starts_with(&format!("fewer will make these changes in Claude Code:\n\n  1. Register fewer's MCP server for all your projects (user scope):\n       claude mcp add --scope user fewer -e FEWER_HOME={h} -- {ME} mcp\n\n  2. Turn fewer on, 87% fewer tokens in our tests")), "{out}");
        // (fewer's changes shown before the one question that covers them, release check)
        assert!(flat(&out).contains("Settings you set yourself stay as they are; fewer off takes all of this back out. 3. Save your Claude Code chats ("), "{out}");
        // (the chats, saved by the same yes, said before the question too)
        assert!(flat(&out).contains(&format!("3. Save your Claude Code chats ({}/claude/projects) on this computer too, in {}, with secrets hidden: the token card and the start card come from them (sessions = on in {}/config; to stop: fewer memory sessions off). 4. Add fewer's slash commands to Claude Code ({}/claude/commands): /fewer-tokens, /fewer-off, /fewer-on and /fewer-memory. Only you run them: Claude is not told about them. Register fewer and turn it on (the changes above)? [y/N] Added stdio MCP server fewer with command: ", tilde(&d), tilde(&home), tilde(&home), tilde(&d))), "{out}");
        assert_eq!(out.matches("[y/N]").count(), 1, "{out}");
        assert_eq!(out.matches("fewer on, 87% fewer tokens in our tests").count(), 1, "{out}");
        assert!(out.contains(&format!("✓ Your Claude Code chats will be saved (sessions = on in {}/config).\n", tilde(&home))), "{out}");
        assert!(out.ends_with(&format!("{USE_LINE}\n")));
        // the stub read the arguments as claude does
        let server = fs::read_to_string(d.join("stub").join("server-fewer")).unwrap();
        assert_eq!(server, format!("scope=user\ncommand={ME}\narg=mcp\nenv=FEWER_HOME={h}\n"));
        // (only Claude Code's chats: install is for Claude Code; the other agents are set off)
        assert_eq!(config_rest(&d).as_deref(), Some("sessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"));
        // fewer came on by itself, recorded
        assert!(config_text(&d).unwrap().contains("savers.lean = lean:agent,"));
        assert!(out.contains("✓ fewer is on: it applies to new Claude Code chats, and to old ones when you reopen them.\n"), "{out}");
        // (a second install: fewer on already, left so; savers.on kept, the token card's start)
        let on = config::get(&config::read(&home), "savers.on").map(str::to_string);
        let (code, out, _) = talk(install, &s, Opts::default(), true, "");
        assert_eq!(code, 0);
        assert!(out.contains("It is left as it is.\n") && out.contains("fewer is on already in "), "{out}");
        assert_eq!(config::get(&config::read(&home), "savers.on").map(str::to_string), on);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_yes_then_no_and_no() {
        let (d, s) = setup("yes-no");
        let (code, out, _) = talk(install, &s, Opts::default(), true, "yes\nno\n");
        assert_eq!(code, 0);
        assert_eq!(calls(&d).len(), 2);
        assert!(d.join("stub").join("server-fewer").exists());
        // one question only: the chats are saved under the same yes
        assert_eq!(out.matches("[y/N]").count(), 1, "{out}");
        assert_eq!(config_rest(&d).as_deref(), Some("sessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"));
        fs::remove_dir_all(&d).unwrap();

        // no: nothing changes
        let (d, s) = setup("no");
        for answer in ["n\n", "\n", "yess\n", ""] {
            let (code, out, err) = talk(install, &s, Opts::default(), true, answer);
            assert_eq!((code, err.as_str()), (0, ""), "{answer:?}");
            assert!(out.ends_with("Register fewer and turn it on (the changes above)? [y/N] Not registered. Nothing was changed.\n") || out.ends_with("Register fewer and turn it on (the changes above)? [y/N] \nNot registered. Nothing was changed.\n"), "{answer:?}: {out}");
            assert_eq!(out.matches("[y/N]").count(), 1);
        }
        assert!(calls(&d).iter().all(|c| *c == get_call()));
        assert!(!d.join("stub").join("server-fewer").exists());
        assert_eq!(config_text(&d), None);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_without_asking() {
        // --dry-run: only shown; claude is only asked whether fewer is registered
        let (d, s) = setup("dry");
        let (code, out, _) = talk(install, &s, Opts { dry_run: true, yes: false }, true, "y\ny\n");
        assert_eq!(code, 0);
        let file = d.join("home").join("config");
        assert!(flat(&out).contains(&format!("2. Turn fewer on, 87% fewer tokens in our tests, with the same answers: it makes {}/claude/settings.json (there is none yet)", tilde(&d))), "{out}");
        assert!(flat(&out).contains(&format!("3. Save your Claude Code chats ({}/claude/projects) on this computer too, in {}, with secrets hidden: the token card and the start card come from them (sessions = on in {}; to stop: fewer memory sessions off).", tilde(&d), tilde(&d.join("home")), tilde(&file))), "{out}");
        assert!(out.ends_with("Dry run: nothing was changed. `fewer install` asks \"Register fewer and turn it on (the changes above)? [y/N]\" and makes them on a yes; `fewer off` takes fewer back out.\n"), "{out}");
        assert!(!out.contains("Register it?") && !out.contains("[y/N] "));
        assert!(!d.join("claude").exists());
        let (code, _, _) = talk(install, &s, Opts { dry_run: true, yes: true }, false, "");
        assert_eq!(code, 0);
        assert_eq!(calls(&d), [get_call(), get_call()]);
        assert!(!d.join("stub").join("server-fewer").exists());
        assert!(!d.join("home").exists());

        // stdin not a terminal, no --yes: exit 1, nothing changed
        let (code, out, err) = talk(install, &s, Opts::default(), false, "y\ny\n");
        assert_eq!(code, 1);
        assert!(err.contains("not registered: stdin is not a terminal"), "{err}");
        assert!(out.contains("\n       claude mcp add --scope user fewer -e "), "{out}");
        assert_eq!(calls(&d).len(), 3);
        assert!(!d.join("stub").join("server-fewer").exists());
        assert!(!d.join("claude").exists());

        // --yes: registered without the question; the sessions question is not answered for the
        // developer; fewer comes on under the same yes
        let (code, out, err) = talk(install, &s, Opts { dry_run: false, yes: true }, false, "y\ny\n");
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(!out.contains("[y/N]"));
        // (--yes is the yes to the one question: the chats are saved too)
        assert!(out.contains("✓ Your Claude Code chats will be saved (sessions = on in "), "{out}");
        assert!(out.contains("✓ fewer is on: it applies to new Claude Code chats, and to old ones when you reopen them"), "{out}");
        assert_eq!(calls(&d)[3..], [get_call(), add_call(&d.join("home"))]);
        assert_eq!(config_rest(&d).as_deref(), Some("sessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"));
        let settings = fs::read_to_string(d.join("claude").join("settings.json")).unwrap();
        assert!(settings.contains(&format!("\"command\": \"FEWER_HOME={} {ME} hook post-bash\"", d.join("home").display())) && settings.contains("\"autoCompactWindow\": 100000"), "{settings}");
        assert!(settings.contains("\"agent\": \"fewer-lean\"") && settings.contains("\"CLAUDE_CODE_DISABLE_WORKFLOWS\": \"1\"") && settings.contains("\"enableArtifact\": false") && !settings.contains("MCP_DESCRIPTION_LENGTH") && !settings.contains("ENABLE_CLAUDEAI_MCP_SERVERS") && settings.contains("\"deniedMcpServers\": [\n    {\n      \"serverName\": \"fewer\"\n    }\n  ]"), "{settings}");
        // CLAUDE.md files stay on: the developer's own rules keep applying
        assert!(!settings.contains("CLAUDE_CODE_DISABLE_CLAUDE_MDS"), "{settings}");
        let agent = fs::read_to_string(d.join("claude").join("agents").join("fewer-lean.md")).unwrap();
        assert!(agent.contains(&format!("`{ME} changes [REV]`")) && agent.contains("ending in ` 2>&1 || true`") && agent.contains("its tools, as usual (ToolSearch loads them)") && !agent.contains("mcp list"), "{agent}");
        let wf = fs::read_to_string(d.join("claude").join("agents").join("fewer-lean-workflow.md")).unwrap();
        assert!(wf.contains("tools: Bash, Read, Edit, Write, Workflow, Skill\n") && wf.contains("agentType: 'fewer-worker'"), "{wf}");
        let wk = fs::read_to_string(d.join("claude").join("agents").join("fewer-worker.md")).unwrap();
        assert!(wk.contains("tools: Bash, Read, Edit, Write\n") && wk.contains(&format!("`{ME} changes [REV]`")) && !wk.contains("mcp list"), "{wk}");
        let cfg = config_text(&d).unwrap_or_default();
        assert!(cfg.contains("savers.start = workflows,artifact\n") && cfg.contains("savers.lean = lean:agent,"), "{cfg}");

        // registered already: said and shown, claude not run again; fewer on already
        let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!(code, 0);
        assert!(out.starts_with("Claude Code has an MCP server named fewer already (claude mcp get fewer):\nfewer:\n  Scope: user config\n"), "{out}");
        assert!(out.contains(&format!("  Command: {ME}\n")));
        assert!(out.contains("It is left as it is.\n"), "{out}");
        assert!(!out.contains("[y/N]") && out.contains("fewer is on already in"), "{out}");
        assert_eq!(calls(&d).len(), 6);
        // uninstall takes it all back: the settings as they were, the agents' files gone
        let (code, out, _) = talk(uninstall, &s, Opts::default(), true, "y\n");
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("CLAUDE_CODE_DISABLE_WORKFLOWS removed, enableArtifact removed, agent removed"), "{out}");
        // (a settings.json and an agents/ that fewer made: taken away, not left as "{}" and an
        // empty folder, the release check)
        assert!(!d.join("claude").join("settings.json").exists() && !d.join("claude").join("agents").exists());
        let cfg = config_text(&d).unwrap_or_default();
        assert!(cfg.contains("savers.start = \n") && cfg.contains("savers.lean = \n"), "{cfg}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_when_claude_is_missing_or_fails() {
        let (d1, mut s) = setup("missing");
        s.claude = Some(Err("FEWER_CLAUDE_BIN=claude was not found on PATH".into()));
        let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
        // (nothing installed: exit 1, release check)
        assert_eq!(code, 1);
        let h = d1.join("home").display().to_string();
        assert_eq!(out, format!("FEWER_CLAUDE_BIN=claude was not found on PATH. When Claude Code is installed, run fewer install again (or, for fewer's MCP server alone: the command below).\n  claude mcp add --scope user fewer -e FEWER_HOME={h} -- {ME} mcp\n"));
        assert!(calls(&d1).is_empty());

        // in development, FEWER_CLAUDE_BIN not set: exit 2 and how to set it, also for a dry run;
        // claude is not looked for, nothing is written
        s.claude = None;
        for o in [Opts::default(), Opts { dry_run: true, yes: false }, Opts { dry_run: false, yes: true }] {
            let (code, out, err) = talk(install, &s, o, true, "y\ny\n");
            assert_eq!((code, out.as_str()), (2, ""), "{o:?}");
            assert!(err.starts_with("fewer install: FEWER_CLAUDE_BIN is not set. In development (FEWER_DEV), fewer runs Claude Code's `claude` only from FEWER_CLAUDE_BIN"), "{err}");
            assert!(err.ends_with("\n  FEWER_CLAUDE_BIN=\"$(command -v claude)\" fewer install\n"), "{err}");
        }
        assert!(calls(&d1).is_empty() && !d1.join("home").exists());

        // claude mcp add fails: its output, then the command to run by hand; no sessions
        // question
        let (d2, s) = setup("fails");
        fs::write(d2.join("stub").join("fail-add"), "").unwrap();
        let (code, out, err) = talk(install, &s, Opts::default(), true, "y\ny\n");
        assert_eq!(code, 1);
        assert!(out.ends_with(&format!("Register fewer and turn it on (the changes above)? [y/N] Error: cannot write {}/.claude.json (claude-stub was told to fail)\n", d2.join("stub").display())), "{out}");
        let h = d2.join("home").display().to_string();
        assert_eq!(err, format!("fewer install: claude mcp add failed (exit status 1). To register fewer yourself, run:\n  claude mcp add --scope user fewer -e FEWER_HOME={h} -- {ME} mcp\n"));
        assert_eq!(config_text(&d2), None);

        // claude cannot be run at all
        let (d3, s) = setup("cannot-run");
        fs::write(d3.join("stub").join("claude"), "#!/no/such/shell\n").unwrap();
        let (code, _, err) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!(code, 1);
        assert!(err.contains("cannot run") && err.contains("To register fewer yourself, run:\n  claude mcp add"), "{err}");

        // no store: exit 2 before claude is run
        let (d4, mut s) = setup("no-home");
        s.home = Err("FEWER_HOME is not set: set it to the memory store's folder (an absolute path)".into());
        let (code, _, err) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!(code, 2);
        assert!(err.contains("FEWER_HOME is not set"));
        assert!(calls(&d4).is_empty());
        for d in [d1, d2, d3, d4] {
            fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn install_saves_claude_codes_chats() {
        // the config before, after the one yes: sessions on, Claude's own line on when it said
        // otherwise, every other agent off unless the config names it
        let cases = [
            ("# my settings\ndna = off\npause = /w/secret\n", "# my settings\ndna = off\npause = /w/secret\nsessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"),
            ("sessions = off\n", "sessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"),
            ("sessions = on\nsessions.claude = off\n", "sessions = on\nsessions.claude = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"),
            ("sessions.codex = on\n", "sessions.codex = on\nsessions = on\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"),
        ];
        for (i, (before, after)) in cases.iter().enumerate() {
            let (d, s) = setup(&format!("chats-{i}"));
            fs::create_dir_all(d.join("home")).unwrap();
            fs::write(d.join("home").join("config"), before).unwrap();
            let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
            assert_eq!(code, 0, "{before:?}");
            assert_eq!(out.matches("[y/N]").count(), 1, "{out}");
            assert_eq!(config_rest(&d).as_deref(), Some(*after), "{before:?}");
            fs::remove_dir_all(&d).unwrap();
        }
        // saved already: said, the config unchanged
        let (d, s) = setup("chats-on");
        fs::create_dir_all(d.join("home")).unwrap();
        fs::write(d.join("home").join("config"), "sessions = on\nsessions.claude = on\n").unwrap();
        let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!(code, 0);
        assert!(flat(&out).contains("3. Your Claude Code chats: saved already (sessions = on in"), "{out}");
        assert_eq!(config_rest(&d).as_deref(), Some("sessions = on\nsessions.claude = on\n"));
        fs::remove_dir_all(&d).unwrap();
        // no: nothing saved
        let (d, s) = setup("chats-no");
        let (code, _, _) = talk(install, &s, Opts::default(), true, "n\n");
        assert_eq!(code, 0);
        assert_eq!(config_text(&d), None);
        fs::remove_dir_all(&d).unwrap();
        // Claude Code's folder not known: said, nothing saved
        let (d, mut s) = setup("chats-none");
        s.places = places(&[]);
        let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!(code, 0);
        assert!(flat(&out).contains("Your Claude Code chats: not saved, as Claude Code's folder is not known (FEWER_CLAUDE_DIR)."), "{out}");
        assert_eq!(config_rest(&d), None);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn off_and_uninstall_without_the_stores_record() {
        // (the store removed after install: what fewer wrote is still taken out, by its values;
        // the developer's own settings stay)
        for way in ["off", "uninstall"] {
            let (d, s) = setup(&format!("no-record-{way}"));
            fs::create_dir_all(d.join("claude")).unwrap();
            let file = d.join("claude").join("settings.json");
            let theirs = "{\n  \"model\": \"opus\"\n}\n";
            fs::write(&file, theirs).unwrap();
            assert_eq!(talk(install, &s, Opts::default(), true, "y\n").0, 0);
            assert!(fs::read_to_string(&file).unwrap().contains("fewer-lean"));
            fs::remove_dir_all(d.join("home")).unwrap();
            let (code, out, _) = if way == "off" { talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "") } else { talk(uninstall, &s, Opts::default(), true, "y\n") };
            assert_eq!(code, 0, "{out}");
            assert_eq!(fs::read_to_string(&file).unwrap(), theirs, "{way}: {out}");
            fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn install_after_uninstall_is_a_first_install() {
        let (d, s) = setup("after-uninstall");
        fs::create_dir_all(d.join("claude")).unwrap();
        let file = d.join("claude").join("settings.json");
        let home = d.join("home");
        assert_eq!(talk(install, &s, Opts::default(), true, "y\n").0, 0);
        // off, then on again: the first period kept for the token card
        talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "");
        talk(|s, o, t| tenx_step(s, o, t, Path::new(ME), Plan::Show).map(|ok| if ok { 0 } else { 1 }), &s, Opts::default(), false, "");
        let cfg = config::read(&home);
        assert_eq!((crate::tokens::periods_of(&cfg).len(), config::get(&cfg, "savers.off")), (2, Some("")), "{cfg:?}");
        // uninstall where claude is not found (its MCP server left registered), then install
        // again: fewer on, as at a first install; both periods kept
        let no_claude = Setup { claude: Some(Err("Claude Code (claude) is not on PATH".into())), claude_shown: "claude".into(), me: Ok(PathBuf::from(ME)), home: Ok(home.clone()), home_var: Some(home.clone().into_os_string()), places: places(&[("FEWER_CLAUDE_DIR", d.join("claude").to_str().unwrap())]) };
        let (code, out, _) = talk(uninstall, &no_claude, Opts::default(), true, "y\n");
        assert_eq!(code, 3, "{out}");
        let cfg = config::read(&home);
        assert_eq!((config::get(&cfg, "savers.on"), config::get(&cfg, "savers.off"), crate::tokens::periods_of(&cfg).len()), (Some(""), Some(""), 2), "{cfg:?}");
        let (code, out, _) = talk(install, &s, Opts { yes: true, ..Opts::default() }, false, "");
        assert!(code == 0 && !out.contains("as it was turned off") && flat(&out).contains("fewer is on"), "{out}");
        assert!(fs::read_to_string(&file).unwrap().contains("hook post-bash"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_window_the_developer_set_stays_through_off_and_uninstall() {
        // (the values an earlier fewer wrote, 120000 and 150000, set by the developer: kept by
        // install, by off, by uninstall, and by uninstall after off)
        for v in ["120000", "150000"] {
            let (d, s) = setup(&format!("own-window-{v}"));
            fs::create_dir_all(d.join("claude")).unwrap();
            let file = d.join("claude").join("settings.json");
            let theirs = format!("{{\n  \"autoCompactWindow\": {v}\n}}\n");
            fs::write(&file, &theirs).unwrap();
            let on_again = |s: &Setup| talk(|s, o, t| tenx_step(s, o, t, Path::new(ME), Plan::Show).map(|ok| if ok { 0 } else { 1 }), s, Opts::default(), false, "");
            let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
            assert_eq!(code, 0, "{out}");
            assert!(fs::read_to_string(&file).unwrap().contains(&format!("\"autoCompactWindow\": {v},")), "{v}");
            let (code, out, _) = talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "");
            assert!(code == 0 && !out.contains("autoCompactWindow removed"), "{out}");
            assert_eq!(fs::read_to_string(&file).unwrap(), theirs);
            on_again(&s);
            let (code, out, _) = talk(uninstall, &s, Opts::default(), true, "y\n");
            assert!(code == 0 && !out.contains("autoCompactWindow removed"), "{out}");
            assert_eq!(fs::read_to_string(&file).unwrap(), theirs);
            let (code, out, _) = talk(install, &s, Opts::default(), true, "y\n");
            assert_eq!(code, 0, "{out}");
            talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "");
            let (code, out, _) = talk(uninstall, &s, Opts::default(), true, "y\n");
            assert!(code == 0 && !out.contains("autoCompactWindow removed"), "{out}");
            assert_eq!(fs::read_to_string(&file).unwrap(), theirs);
            fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn ten_x_mode_by_itself_and_its_undo() {
        // on by itself: written, the developer's own settings kept, a copy of the file as it was
        let (d, s) = setup("savers");
        fs::create_dir_all(d.join("claude")).unwrap();
        let file = d.join("claude").join("settings.json");
        let theirs = "{\n  \"model\": \"opus\",\n  \"autoCompactWindow\": 400000\n}\n";
        fs::write(&file, theirs).unwrap();
        let (code, out, err) = talk(install, &s, Opts::default(), true, "y\n");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(out.matches("[y/N]").count(), 1, "{out}");
        let h = d.join("home").display().to_string();
        assert!(flat(&out).contains("✓ fewer is on (a copy of the file before this change: ") && flat(&out).ends_with(&flat(&format!("it applies to new Claude Code chats, and to old ones when you reopen them.\n✓ fewer's slash commands are in Claude Code: type /fewer in a chat (/fewer-tokens, /fewer-off, /fewer-on, /fewer-memory).\n{USE_LINE}"))), "{out}");
        let now = fs::read_to_string(&file).unwrap();
        assert!(now.starts_with("{\n  \"model\": \"opus\",\n  \"autoCompactWindow\": 400000,\n  \"hooks\": {\n") && now.contains(&format!("FEWER_HOME={h} /opt/fewer/bin/fewer hook precompact")), "{now}");
        let copies = |d: &Path| -> Vec<PathBuf> { fs::read_dir(d.join("claude")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("settings.json.before-fewer-")).collect() };
        assert_eq!(copies(&d).len(), 1);
        assert_eq!(fs::read_to_string(&copies(&d)[0]).unwrap(), theirs);
        // the status, then the one command that takes it back (no question), then on again
        let (code, out, _) = talk(|s, _, t| tenx_status(s, t), &s, Opts::default(), false, "");
        assert!(code == 0 && out.starts_with(&format!("fewer is on in {} since ", file.display())) && out.ends_with(". To turn it off: fewer off\n") && !out.contains("UTC"), "{out}");
        let (code, out, _) = talk(|s, _, t| tenx_off(s, true, t), &s, Opts::default(), false, "");
        assert!(code == 0 && out.ends_with("Dry run: nothing was changed.\n") && fs::read_to_string(&file).unwrap() == now, "{out}");
        let (code, out, err) = talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(flat(&out).starts_with(&format!("fewer off takes fewer out of {} (a copy of the file is kept first): PostToolUse hook removed, SessionStart hook removed, PreCompact hook removed, ", tilde(&file))) && out.contains("agent removed") && out.ends_with("To turn it on again: fewer on\n"), "{out}");
        assert_eq!(fs::read_to_string(&file).unwrap(), theirs);
        assert!(["fewer-lean.md", "fewer-lean-workflow.md", "fewer-worker.md"].iter().all(|f| !d.join("claude").join("agents").join(f).exists()));
        assert_eq!(copies(&d).len(), 2);
        let (_, out, _) = talk(|s, _, t| tenx_status(s, t), &s, Opts::default(), false, "");
        assert_eq!(out, "fewer is off. To turn it on: fewer on\n");
        let (_, out, _) = talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "");
        assert_eq!(out, "fewer is off already.\n");
        let (code, out, _) = talk(|s, o, t| tenx_step(s, o, t, Path::new(ME), Plan::Show).map(|ok| if ok { 0 } else { 1 }), &s, Opts::default(), false, "");
        assert!(code == 0 && flat(&out).contains("fewer is on (a copy of the file before this change: "), "{out}");
        assert_eq!(fs::read_to_string(&file).unwrap(), now);
        // only the agents' files left (the settings changed by hand): fewer off takes them too
        fs::write(&file, theirs).unwrap();
        let (_, out, _) = talk(|s, _, t| tenx_off(s, false, t), &s, Opts::default(), false, "");
        assert!(out.contains("\n  fewer's agents' files.\n") && !d.join("claude").join("agents").join("fewer-lean.md").exists(), "{out}");
        // uninstall: the MCP server and fewer go under one yes; the developer's window stays
        talk(|s, o, t| tenx_step(s, o, t, Path::new(ME), Plan::Show).map(|ok| if ok { 0 } else { 1 }), &s, Opts::default(), false, "");
        let (code, out, err) = talk(uninstall, &s, Opts::default(), true, "y\n");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.contains("take out fewer (PostToolUse hook removed, SessionStart hook removed, PreCompact hook removed, ") && out.contains("agent removed"), "{out}");
        assert_eq!(fs::read_to_string(&file).unwrap(), theirs);
        assert!(!d.join("stub").join("server-fewer").exists());
        fs::remove_dir_all(&d).unwrap();

        // settings.json that fewer cannot read: not turned on, left as it is
        let (d2, s2) = setup("savers-bad");
        fs::create_dir_all(d2.join("claude")).unwrap();
        fs::write(d2.join("claude").join("settings.json"), "{ // mine\n}").unwrap();
        let (code, out, _) = talk(install, &s2, Opts::default(), true, "y\n");
        assert_eq!(code, 0);
        assert_eq!(out.matches("[y/N]").count(), 1, "{out}");
        assert!(out.contains("fewer is not turned on: ") && out.contains("left as it is"), "{out}");
        assert_eq!(fs::read_to_string(d2.join("claude").join("settings.json")).unwrap(), "{ // mine\n}");
        // an agent file of someone else's: not turned on, nothing written
        let (d3, s3) = setup("savers-theirs");
        fs::create_dir_all(d3.join("claude").join("agents")).unwrap();
        fs::write(d3.join("claude").join("agents").join("fewer-lean-workflow.md"), "mine\n").unwrap();
        let (code, _, err) = talk(install, &s3, Opts::default(), true, "y\nn\n");
        assert_eq!(code, 1);
        assert!(err.contains("is someone else's agent; it is left as it is"), "{err}");
        assert!(!d3.join("claude").join("settings.json").exists() && !d3.join("claude").join("agents").join("fewer-lean.md").exists());
        fs::remove_dir_all(&d2).unwrap();
        fs::remove_dir_all(&d3).unwrap();
    }

    #[test]
    fn uninstall_flows() {
        let (d, s) = setup("uninstall");
        let remove = || -> Vec<String> { ["mcp", "remove", "--scope", "user", "fewer"].iter().map(|a| a.to_string()).collect() };
        // not registered: nothing to remove, nothing asked
        let (code, out, _) = talk(uninstall, &s, Opts::default(), true, "y\n");
        assert_eq!((code, out.as_str()), (0, "Claude Code has no MCP server named fewer and no fewer settings: nothing to remove.\n"));
        // registered: no, dry run, not a terminal, then yes
        talk(install, &s, Opts { dry_run: false, yes: true }, false, "");
        let n = calls(&d).len();
        let (code, out, _) = talk(uninstall, &s, Opts::default(), true, "n\n");
        // (a no is no removal: exit 1, so that a script keeps the programs)
        assert_eq!(code, 1);
        assert!(out.contains("The command:\n  claude mcp remove --scope user fewer\nClaude Code's settings (") && out.ends_with("; the file goes, as fewer made it and nothing else is in it.\nRemove it? [y/N] Not removed. Nothing was changed.\n"), "{out}");
        let (code, _, _) = talk(uninstall, &s, Opts { dry_run: true, yes: true }, true, "y\n");
        assert_eq!(code, 0);
        let (code, _, err) = talk(uninstall, &s, Opts::default(), false, "y\n");
        assert_eq!(code, 1);
        assert!(err.contains("not removed: stdin is not a terminal"), "{err}");
        assert!(d.join("stub").join("server-fewer").exists());
        assert_eq!(calls(&d)[n..], [get_call(), get_call(), get_call()]);
        let (code, out, err) = talk(uninstall, &s, Opts::default(), true, "y\n");
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.contains("Remove it? [y/N] Removed MCP server fewer from user config\n"), "{out}");
        assert!(out.ends_with("Removed from Claude Code.\n"));
        assert_eq!(calls(&d)[n + 3..], [get_call(), remove()]);
        assert!(!d.join("stub").join("server-fewer").exists());
        // and installed again
        let (code, _, _) = talk(install, &s, Opts::default(), true, "y\nn\n");
        assert_eq!(code, 0);
        assert!(d.join("stub").join("server-fewer").exists());
        // claude mcp remove fails: the command to run by hand
        fs::write(d.join("stub").join("fail-remove"), "").unwrap();
        let (code, _, err) = talk(uninstall, &s, Opts { dry_run: false, yes: true }, false, "");
        assert_eq!(code, 1);
        assert!(err.ends_with("claude mcp remove failed (exit status 1). To remove fewer's MCP server yourself, run:\n  claude mcp remove --scope user fewer\n"), "{err}");
        // claude missing: the command to run later
        let (d2, mut s2) = setup("uninstall-missing");
        s2.claude = Some(Err("FEWER_CLAUDE_BIN=claude was not found on PATH".into()));
        let (code, out, _) = talk(uninstall, &s2, Opts::default(), true, "y\n");
        // (Claude Code not found and no fewer settings in its settings: exit 3, nothing of fewer runs)
        assert_eq!(code, 3);
        assert!(out.ends_with("so fewer's MCP server cannot be taken out of Claude Code now. When Claude Code is there, run:\n  claude mcp remove --scope user fewer\n"), "{out}");
        // in development, FEWER_CLAUDE_BIN not set: exit 2 and how to set it
        s2.claude = None;
        let (code, out, err) = talk(uninstall, &s2, Opts { dry_run: false, yes: true }, true, "y\n");
        assert_eq!((code, out.as_str()), (2, ""));
        assert!(err.contains("FEWER_CLAUDE_BIN is not set") && err.ends_with("fewer uninstall\n"), "{err}");
        assert!(calls(&d2).is_empty());
        fs::remove_dir_all(&d).unwrap();
        fs::remove_dir_all(&d2).unwrap();
    }
}
