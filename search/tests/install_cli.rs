//! `install claude` and `uninstall claude` end to end, with stdin not a terminal
//! (checks/install_check.py checks the questions, with a terminal from Python's pty module):
//! claude found through FEWER_CLAUDE_BIN (a path, or a name on PATH), or missing; in development
//! nothing without FEWER_CLAUDE_BIN; the exact `claude mcp add` arguments; no --yes; --dry-run;
//! no store.
//!
//! claude is always the stub (tests/data/claude-stub), copied into the target folder's tmp/
//! (CARGO_TARGET_TMPDIR); the PATH given to the program is checked to hold no other claude,
//! and HOME is a folder there too, checked to stay empty.
//!
//! (Mac and Linux: the stub is a shell script run as a program, which Windows cannot run.)
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fewer");
const STUB: &str = include_str!("data/claude-stub");
const SYSTEM_PATH: &str = "/usr/bin:/bin";

struct Case {
    base: PathBuf,
    stub_dir: PathBuf,
    home: PathBuf,
}

impl Case {
    fn new(name: &str) -> Case {
        // the real claude must not be on the PATH the program gets
        for d in SYSTEM_PATH.split(':') {
            assert!(!Path::new(d).join("claude").exists(), "{d}/claude exists: these tests would find it");
        }
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("install-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let stub_dir = base.join("stub");
        fs::create_dir_all(&stub_dir).unwrap();
        fs::create_dir_all(base.join("fakehome")).unwrap();
        fs::write(stub_dir.join("claude"), STUB).unwrap();
        fs::set_permissions(stub_dir.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        Case { home: base.join("home"), stub_dir, base }
    }

    fn stub(&self) -> PathBuf {
        self.stub_dir.join("claude")
    }

    /// The program with a clean environment: PATH without claude, HOME and the store in this
    /// case's folder, claude given by FEWER_CLAUDE_BIN.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_clear().env("FEWER_DEV", "1")
            .env("PATH", SYSTEM_PATH)
            .env("HOME", self.base.join("fakehome"))
            .env("FEWER_HOME", &self.home)
            .env("FEWER_CLAUDE_BIN", self.stub())
            .stdin(Stdio::null());
        c
    }

    fn calls(&self) -> Vec<Vec<String>> {
        let b = fs::read(self.stub_dir.join("calls")).unwrap_or_default();
        b.split(|&c| c == 0x1e).filter(|c| !c.is_empty()).map(|c| c.split(|&x| x == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()).collect()
    }

    fn registered(&self) -> Option<String> {
        fs::read_to_string(self.stub_dir.join("server-fewer")).ok()
    }

    fn done(self) {
        // the program never wrote into HOME
        assert_eq!(fs::read_dir(self.base.join("fakehome")).unwrap().count(), 0);
        fs::remove_dir_all(&self.base).unwrap();
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|a| a.to_string()).collect()
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{:?}\n{}\n{}", o.status, text(&o.stdout), text(&o.stderr));
    text(&o.stdout)
}

#[test]
fn registers_with_the_exact_arguments() {
    let c = Case::new("exact");
    let home = c.home.display().to_string();
    let out = ok(&c.cmd(&["install", "claude", "--yes"]).output().unwrap());
    let add = strings(&["mcp", "add", "--scope", "user", "fewer", "-e", &format!("FEWER_HOME={home}"), "--", BIN, "mcp"]);
    assert_eq!(c.calls(), [strings(&["mcp", "get", "fewer"]), add]);
    // the stub read them as claude does: user scope, this program, mcp, the store
    assert_eq!(c.registered().unwrap(), format!("scope=user\ncommand={BIN}\narg=mcp\nenv=FEWER_HOME={home}\n"));
    assert!(out.contains(&format!("Added stdio MCP server fewer with command: {BIN} mcp to user config\n")), "{out}");
    assert!(out.ends_with("  fewer off      turn fewer off (fewer on turns it back on)\n"), "{out}");
    // no one to ask the sessions question: the config is not touched
    assert!(!c.home.join("config").exists());
    // again: registered already, nothing else run
    let out = ok(&c.cmd(&["install", "claude", "--yes"]).output().unwrap());
    assert!(out.starts_with("Claude Code has an MCP server named fewer already"), "{out}");
    assert_eq!(c.calls().len(), 3);
    c.done();
}

#[test]
fn finds_a_named_claude_on_path() {
    // FEWER_CLAUDE_BIN a name: looked up on the PATH given (here the stub's folder)
    let c = Case::new("path");
    let path = format!("{}:{SYSTEM_PATH}", c.stub_dir.display());
    let out = ok(&c.cmd(&["install", "claude", "--yes"]).env("FEWER_CLAUDE_BIN", "claude").env("PATH", &path).output().unwrap());
    assert!(out.contains("\n       claude mcp add --scope user fewer -e "), "{out}");
    assert!(c.registered().is_some());
    let out = ok(&c.cmd(&["uninstall", "claude", "--yes"]).env("FEWER_CLAUDE_BIN", "claude").env("PATH", &path).output().unwrap());
    assert!(out.contains("The command:\n  claude mcp remove --scope user fewer\nRemoved MCP server fewer from user config\n"), "{out}");
    assert_eq!(c.calls()[2..], [strings(&["mcp", "get", "fewer"]), strings(&["mcp", "remove", "--scope", "user", "fewer"])]);
    assert!(c.registered().is_none());
    c.done();
}

#[test]
fn claude_missing_or_not_set() {
    let c = Case::new("missing");
    let home = c.home.display().to_string();
    // FEWER_CLAUDE_BIN names no program: the command to run later, exit 1 (nothing done: a
    // script that removes the programs after it must not go on)
    let missing = c.base.join("nowhere").join("claude");
    let o = c.cmd(&["install", "claude"]).env("FEWER_CLAUDE_BIN", &missing).output().unwrap();
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.ends_with(&format!("When Claude Code is installed, run fewer install again (or, for fewer's MCP server alone: the command below).\n  {} mcp add --scope user fewer -e FEWER_HOME={home} -- {BIN} mcp\n", missing.display())), "{out}");
    // (uninstall: exit 3, Claude Code not found and nothing of fewer in its settings: install.sh
    // may remove the programs then)
    let o = c.cmd(&["uninstall", "claude"]).env("FEWER_CLAUDE_BIN", &missing).output().unwrap();
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(3), "{out}");
    assert!(out.ends_with(&format!("so fewer's MCP server cannot be taken out of Claude Code now. When Claude Code is there, run:\n  {} mcp remove --scope user fewer\n", missing.display())), "{out}");
    // in development, FEWER_CLAUDE_BIN not set: exit 2 and how to set it, even with the stub on PATH
    let path = format!("{}:{SYSTEM_PATH}", c.stub_dir.display());
    for args in [&["install", "claude", "--yes"][..], &["install", "claude", "--dry-run"], &["uninstall", "claude", "--yes"]] {
        let o = c.cmd(args).env_remove("FEWER_CLAUDE_BIN").env("PATH", &path).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(o.stdout.is_empty());
        let err = text(&o.stderr);
        assert!(err.contains("FEWER_CLAUDE_BIN is not set. In development (FEWER_DEV), fewer runs Claude Code's `claude` only from FEWER_CLAUDE_BIN"), "{args:?}: {err}");
        assert!(err.ends_with(&format!("  FEWER_CLAUDE_BIN=\"$(command -v claude)\" fewer {}\n", args[0])), "{err}");
    }
    assert!(c.calls().is_empty() && !c.home.exists());
    c.done();
}

#[test]
fn asks_only_a_terminal() {
    let c = Case::new("no-terminal");
    // stdin /dev/null, and a pipe holding "y": not a terminal, so nothing is registered
    for input in [None, Some("y\ny\n")] {
        let mut cmd = c.cmd(&["install", "claude"]);
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        if let Some(i) = input {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(i.as_bytes()).unwrap();
        }
        let o = child.wait_with_output().unwrap();
        assert_eq!(o.status.code(), Some(1));
        assert!(text(&o.stderr).contains("not registered: stdin is not a terminal"), "{}", text(&o.stderr));
        assert!(!text(&o.stdout).contains("[y/N]"));
    }
    // --dry-run: shown, nothing changed
    let out = ok(&c.cmd(&["install", "claude", "--dry-run"]).output().unwrap());
    assert!(out.contains("\nDry run: nothing was changed. "), "{out}");
    assert_eq!(c.calls(), vec![strings(&["mcp", "get", "fewer"]); 3]);
    assert!(c.registered().is_none());
    assert!(!c.home.exists());
    // uninstall, the same: not a terminal, no --yes
    c.cmd(&["install", "claude", "--yes"]).output().unwrap();
    let o = c.cmd(&["uninstall", "claude"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("not removed: stdin is not a terminal"));
    assert!(c.registered().is_some());
    c.done();
}

#[test]
fn needs_a_store_and_good_arguments() {
    let c = Case::new("usage");
    let o = c.cmd(&["install", "claude", "--yes"]).env_remove("FEWER_HOME").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("FEWER_HOME is not set"));
    assert!(c.calls().is_empty());
    for args in [&["install", "codex"][..], &["install", "claude", "--force"], &["install", "claude", "claude"], &["uninstall", "claude", "-y"]] {
        let o = c.cmd(args).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(text(&o.stderr).contains("usage: fewer "), "{args:?}");
    }
    for cmd in ["install", "uninstall"] {
        let out = ok(&c.cmd(&[cmd, "--help"]).output().unwrap());
        assert!(out.starts_with(&format!("fewer {cmd} - ")), "{out}");
    }
    assert!(c.calls().is_empty());
    // without a name: Claude Code, the only agent for now (a dry run, as with `claude`)
    for cmd in ["install", "uninstall"] {
        let named = c.cmd(&[cmd, "claude", "--dry-run"]).output().unwrap();
        let plain = c.cmd(&[cmd, "--dry-run"]).output().unwrap();
        assert_eq!((plain.status.code(), text(&plain.stdout)), (named.status.code(), text(&named.stdout)), "{cmd}");
    }
    // uninstall needs no store
    c.cmd(&["install", "claude", "--yes"]).output().unwrap();
    let out = ok(&c.cmd(&["uninstall", "claude", "--yes"]).env_remove("FEWER_HOME").output().unwrap());
    assert!(out.contains("Removed from Claude Code.\n"), "{out}");
    c.done();
}
