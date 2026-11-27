//! `fewer sessions sync` end to end, on made-up Claude Code sessions
//! (checks/sessions_check.py checks every agent): saved once, then only the new tail, secrets
//! hidden, the edited files in the run's catalog; nothing read while sessions are off; in
//! development nothing read without an agent's folder setting, whatever HOME holds.
//!
//! Stores and sessions live in the target folder's tmp/ (CARGO_TARGET_TMPDIR); HOME is a
//! folder there too, so the real home is never named. The fewer that packs is $FEWER_PACK_BIN,
//! else the repository's own build (../target/release/fewer-pack); without it, the test that
//! packs fails and says how to build it.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fewer");

/// What a program needs on Windows even in an environment built from nothing (its system
/// folder, for the network too, and the temporary folder); nothing on Mac and Linux.
fn windows_keep() -> Vec<(&'static str, std::ffi::OsString)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    ["SystemRoot", "TEMP", "TMP"].iter().filter_map(|k| std::env::var_os(k).map(|v| (*k, v))).collect()
}

/// The PATH of the system's own folders: /usr/bin:/bin (Windows: the PATH as given, which holds
/// its system folders, and Git's).
fn sys_path() -> std::ffi::OsString {
    if cfg!(windows) { std::env::var_os("PATH").unwrap_or_default() } else { "/usr/bin:/bin".into() }
}
const SID: &str = "11111111-2222-4333-8444-555555555555";

/// The fewer-pack program that packs (never run when nothing is saved).
fn fewer() -> PathBuf {
    pack_bin()
}

/// $FEWER_PACK_BIN, else the repository's own build of fewer-pack (in $CARGO_TARGET_DIR when it
/// is set, else ../target).
fn pack_bin() -> PathBuf {
    match std::env::var_os("FEWER_PACK_BIN").filter(|v| !v.is_empty()) {
        Some(b) => PathBuf::from(b),
        None => std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("target")).join("release").join(format!("fewer-pack{}", std::env::consts::EXE_SUFFIX)),
    }
}


struct Case {
    base: PathBuf,
}

impl Case {
    fn new(name: &str) -> Case {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("sessions-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in ["home", "fakehome", "claude/projects/-w-app", "proj/app"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        Case { base }
    }

    /// `sessions sync` with an environment built from nothing: HOME and every agent's folder
    /// in this case's folder.
    fn sync(&self) -> Output {
        let b = &self.base;
        self.cmd()
            .env("FEWER_CLAUDE_DIR", b.join("claude"))
            .env("FEWER_CODEX_DIR", b.join("codex"))
            .env("FEWER_AIDER_DIR", b.join("proj/app"))
            .env("FEWER_CLINE_DIR", b.join("cline"))
            .env("FEWER_CURSOR_DIR", b.join("cursor"))
            .output()
            .unwrap()
    }

    /// The program for `sessions sync`, without any agent's folder setting.
    fn cmd(&self) -> Command {
        let b = &self.base;
        let mut c = Command::new(BIN);
        c.args(["sessions", "sync"])
            .env_clear().envs(windows_keep()).env("FEWER_DEV", "1")
            .env("PATH", sys_path())
            .env("HOME", b.join("fakehome"))
            .env("FEWER_HOME", b.join("home"))
            .env("FEWER_PACK_BIN", fewer())
            .env("GIT_CEILING_DIRECTORIES", b)
            .current_dir(b.join("proj/app"))
            .stdin(Stdio::null());
        c
    }

    fn session(&self) -> PathBuf {
        self.base.join("claude/projects/-w-app").join(format!("{SID}.jsonl"))
    }

    fn runs(&self) -> Vec<String> {
        let p = self.base.join("home/projects");
        let Ok(rd) = fs::read_dir(&p) else { return Vec::new() };
        let mut v = Vec::new();
        for e in rd.flatten() {
            if let Ok(t) = fs::read_to_string(e.path().join("runs.jsonl")) {
                v.extend(t.lines().map(str::to_string));
            }
        }
        v
    }

    fn done(self) {
        fs::remove_dir_all(&self.base).unwrap();
    }
}

fn line(cwd: &Path, n: u32, body: &str) -> String {
    format!("{{\"cwd\":\"{}\",\"sessionId\":\"{SID}\",\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":{body}}},\"timestamp\":\"2026-11-18T10:00:{n:02}.000Z\"}}\n", js(cwd))
}

/// A path as a JSON string holds it (Windows' `\` written `\\`, as Claude Code writes them).
fn js(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn saves_once_then_the_tail_with_secrets_hidden() {
    assert!(fewer().is_file(), "no fewer-pack at {}: build it first (cargo build --release in the repository's root), or set FEWER_PACK_BIN", fewer().display());
    let c = Case::new("tail");
    fs::write(c.base.join("home/config"), "sessions = on\n").unwrap();
    let cwd = c.base.join("proj/app");
    // a made-up key, put together here (no secret-like text in the source)
    let key = format!("AKIA{}", "Q7".repeat(8));
    let first = line(&cwd, 1, &format!("[{{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"{}/src/a.rs\"}}}}]", js(&cwd)))
        + &line(&cwd, 2, &format!("[{{\"type\":\"text\",\"text\":\"key {key}\"}}]"));
    fs::write(c.session(), &first).unwrap();
    let o = c.sync();
    assert!(o.status.success(), "{}", text(&o));
    let runs = c.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs[0].contains("\"kind\":\"session\"") && runs[0].contains("\"agent\":\"claude\"") && runs[0].contains(&format!("\"session\":\"{SID}\"")) && runs[0].contains("\"offset\":0"), "{}", runs[0]);
    assert!(runs[1].contains("\"state\":\"ready\"") && runs[1].contains("\"hidden\":1") && runs[1].contains(&format!("\"size\":{}", first.len())), "{}", runs[1]);
    // the archive holds the session with the key hidden
    let dir = fs::read_dir(c.base.join("home/projects")).unwrap().next().unwrap().unwrap().path();
    let dec = Command::new(fewer()).args(["-d", "-c"]).arg(dir.join("r000001.cmf")).output().unwrap();
    let saved = String::from_utf8(dec.stdout).unwrap();
    assert_eq!(saved, first.replace(&key, "[hidden: AWS key]"));
    // the run's catalog: the session's keys, and the edited file (its path is stored as it is)
    let cat = dir.join("r000001.cat");
    let keys = Command::new(BIN).args(["keys", "--cat"]).arg(&cat).env_clear().envs(windows_keep()).env("FEWER_DEV", "1").env("PATH", sys_path()).env("HOME", c.base.join("fakehome")).output().unwrap();
    let keys = String::from_utf8(keys.stdout).unwrap();
    assert!(keys.contains("1\t{session assistant assistant tool_use Edit}\n") && keys.contains("1\t{session assistant assistant text}\n"), "{keys}");
    let edited = format!("{}/src/a.rs", cwd.display());
    assert!(fs::read(&cat).unwrap().windows(edited.len()).any(|w| w == edited.as_bytes()), "the catalog names the edited file");
    // unchanged: nothing more
    assert!(text(&c.sync()).contains("0 pieces saved"));
    assert_eq!(c.runs().len(), 2);
    // grown: only the tail, continuing the first piece
    let more = line(&cwd, 3, "[{\"type\":\"text\",\"text\":\"done\"}]");
    fs::write(c.session(), format!("{first}{more}")).unwrap();
    let o = c.sync();
    assert!(text(&o).contains("its new part saved as") && text(&o).contains("(continues r1)"), "{}", text(&o));
    let runs = c.runs();
    assert!(runs[2].contains(&format!("\"offset\":{}", first.len())) && runs[2].contains("\"continues\":1"), "{}", runs[2]);
    let dec = Command::new(fewer()).args(["-d", "-c"]).arg(dir.join("r000002.cmf")).output().unwrap();
    assert_eq!(String::from_utf8(dec.stdout).unwrap(), more);
    assert_eq!(fs::read_dir(c.base.join("fakehome")).unwrap().count(), 0);
    c.done();
}

// (Mac and Linux: a folder no one may list is made with its mode, which Windows has not)
#[cfg(unix)]
#[test]
fn nothing_is_read_while_sessions_are_off() {
    let c = Case::new("off");
    fs::write(c.session(), line(&c.base.join("proj/app"), 1, "\"hi\"")).unwrap();
    for config in [None, Some("sessions = off\n"), Some("sessions = maybe\n")] {
        if let Some(t) = config {
            fs::write(c.base.join("home/config"), t).unwrap();
        }
        // the agent's folder cannot even be listed: reading it would be reported
        let projects = c.base.join("claude/projects");
        fs::set_permissions(&projects, fs::Permissions::from_mode(0o000)).unwrap();
        let o = c.sync();
        fs::set_permissions(&projects, fs::Permissions::from_mode(0o755)).unwrap();
        let t = text(&o);
        assert!(o.status.success() && t.contains("sessions are off: nothing was read") && !t.contains("cannot read"), "{config:?}: {t}");
        assert!(c.runs().is_empty() && !c.base.join("home/projects").exists() && !c.base.join("home/raw").exists(), "{config:?}");
    }
    assert_eq!(fs::read_dir(c.base.join("fakehome")).unwrap().count(), 0);
    c.done();
}

#[cfg(unix)]
#[test]
fn nothing_is_read_without_the_folders_settings() {
    // in development: sessions on, and HOME holding Claude Code's and Codex's folders with a session
    // each (Claude Code's and Codex's own settings pointing there too), plus Aider's history
    // in the current project: no agent's setting is set, so every agent is passed over with a
    // note, and no folder is even listed (each is made unreadable: a try would be reported)
    let c = Case::new("no-settings");
    let fake = c.base.join("fakehome");
    let claude = fake.join(".claude/projects/-w-app");
    let codex = fake.join(".codex/sessions/2026/11/18");
    fs::create_dir_all(&claude).unwrap();
    fs::create_dir_all(&codex).unwrap();
    fs::write(claude.join(format!("{SID}.jsonl")), line(&c.base.join("proj/app"), 1, "\"canary\"")).unwrap();
    fs::write(codex.join(format!("rollout-2026-11-18T10-00-00-{SID}.jsonl")), "{\"type\":\"session_meta\"}\n").unwrap();
    fs::write(c.base.join("proj/app/.aider.chat.history.md"), "#### canary\n").unwrap();
    fs::write(c.base.join("home/config"), "sessions = on\n").unwrap();
    let locked = [fake.join(".claude"), fake.join(".codex"), c.base.join("proj/app/.aider.chat.history.md")];
    for p in &locked {
        fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let o = c.cmd().env("CLAUDE_CONFIG_DIR", fake.join(".claude")).env("CODEX_HOME", fake.join(".codex")).env("XDG_CONFIG_HOME", &fake).output().unwrap();
    for p in &locked {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let t = text(&o);
    assert!(o.status.success() && t.contains("sessions: 0 pieces saved, 0 sources unchanged, 0 failed"), "{t}");
    for setting in ["FEWER_CLAUDE_DIR", "FEWER_CODEX_DIR", "FEWER_AIDER_DIR", "FEWER_CLINE_DIR", "FEWER_CURSOR_DIR"] {
        assert!(t.contains(&format!(": {setting} is not set (in development, an agent's sessions are read only from the folder set for it); passed over\n")), "{setting}: {t}");
    }
    assert!(!t.contains("cannot read") && !t.contains("canary"), "{t}");
    assert!(c.runs().is_empty() && !c.base.join("home/projects").exists(), "nothing saved");
    c.done();
}
