//! `fewer run` and pack-pending end to end in pipes mode (checks/run_check.py checks
//! the rest, with a terminal from Python's pty module): exit statuses, both streams passed
//! through unchanged, secrets hidden in the capture, stderr lines listed, and pack-pending's
//! archive and .idx equal to fewer-pack's own.
//!
//! Stores live in the target folder's tmp/ (CARGO_TARGET_TMPDIR), never the home folder; each
//! command gets an environment built from nothing, with HOME a folder there. The fewer that
//! packs is $FEWER_PACK_BIN, else the repository's own build (../target/release/fewer-pack); without
//! it, the tests that pack fail and say how to build it.

use std::fs;
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

/// $FEWER_PACK_BIN, else the repository's own build of fewer-pack (in $CARGO_TARGET_DIR when it
/// is set, else ../target).
fn pack_bin() -> PathBuf {
    match std::env::var_os("FEWER_PACK_BIN").filter(|v| !v.is_empty()) {
        Some(b) => PathBuf::from(b),
        None => std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("target")).join("release").join(format!("fewer-pack{}", std::env::consts::EXE_SUFFIX)),
    }
}

/// The fewer-pack program that packs (a test that packs fails without it: it once passed
/// without packing anything).
fn fewer() -> Option<PathBuf> {
    let p = pack_bin();
    assert!(p.is_file(), "no fewer-pack at {}: build it first (cargo build --release in the repository's root), or set FEWER_PACK_BIN", p.display());
    Some(p)
}

struct Store {
    base: PathBuf,
    home: PathBuf,
    proj: PathBuf,
    fewer: PathBuf,
}

impl Store {
    fn new(name: &str, fewer: PathBuf) -> Store {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("run-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (home, proj) = (base.join("home"), base.join("proj"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(base.join("fakehome")).unwrap();
        Store { base, home, proj, fewer }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_clear().envs(windows_keep()).env("FEWER_DEV", "1")
            .env("PATH", sys_path())
            .env("HOME", self.base.join("fakehome"))
            .env("FEWER_HOME", &self.home)
            .env("FEWER_PACK_BIN", &self.fewer)
            .env("FEWER_PROJECT", &self.proj)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.proj)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str], no_pack: bool) -> Output {
        let mut c = self.cmd(&[&["run", "--"][..], args].concat());
        if no_pack {
            c.env("FEWER_TEST_NO_PACK", "1");
        }
        c.output().unwrap()
    }

    /// The only project's folder and its raw folder.
    fn dirs(&self) -> (PathBuf, PathBuf) {
        let p = fs::read_dir(self.home.join("projects")).unwrap().next().unwrap().unwrap().path();
        let raw = self.home.join("raw").join(p.file_name().unwrap());
        (p, raw)
    }

    /// The last record of run `id` merged, as text (enough for these checks).
    fn record(&self, id: u64) -> String {
        let (p, _) = self.dirs();
        let t = fs::read_to_string(p.join("runs.jsonl")).unwrap();
        t.lines().filter(|l| l.starts_with(&format!("{{\"id\":{id},"))).collect::<Vec<_>>().join("\n")
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // (the store is kept when a test failed, to look at); the program never wrote into HOME
        if !std::thread::panicking() {
            assert_eq!(fs::read_dir(self.base.join("fakehome")).unwrap().count(), 0);
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

#[test]
fn exit_statuses_pass_through() {
    let Some(fewer) = fewer() else { return };
    let s = Store::new("exit", fewer);
    let mut cases = vec![("exit 0", 0, "\"exit\":0"), ("exit 1", 1, "\"exit\":1"), ("exit 3", 3, "\"exit\":3")];
    // (Mac and Linux: a signal ends it; Windows has no signals)
    if cfg!(unix) {
        cases.push(("kill -TERM $$", 143, "\"signal\":15"));
    }
    for (script, code, field) in cases {
        let o = s.run(&["sh", "-c", script], true);
        assert_eq!(o.status.code(), Some(code), "{script}");
        let n = fs::read_to_string(s.dirs().0.join("runs.jsonl")).unwrap().lines().filter(|l| l.contains(field)).count();
        assert_eq!(n, 1, "{script}: {field}");
    }
    let o = s.run(&["no-such-command-for-run"], true);
    assert_eq!(o.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no-such-command-for-run"));
    let o = s.cmd(&["pack-pending", "--wait"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn pipes_capture_hides_secrets_and_packs_like_fewer_pack() {
    let Some(fewer) = fewer() else { return };
    let s = Store::new("pipes", fewer);
    // (the secret is put together at run time, and cut by a pause)
    // (a pause between the two pipes' last writes too: back to back, which comes first in the
    // capture was a race on a busy machine)
    let script = "printf 'one\\n'; printf 'two\\n' >&2; printf 'pass'; sleep 0.2; printf 'word=hunter2 x\\n'; printf 'three\\n' >&2; sleep 0.2; printf 'no end'";
    let o = s.run(&["sh", "-c", script], true);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stdout, b"one\npassword=hunter2 x\nno end");
    assert_eq!(o.stderr, b"two\nthree\n");
    let (p, raw) = s.dirs();
    let cap = raw.join("r000001.out");
    assert_eq!(fs::read(&cap).unwrap(), b"one\ntwo\npassword=[hidden: password] x\nthree\nno end");
    assert_eq!(fs::read_to_string(raw.join("r000001.err")).unwrap(), "2 1\n4 1\n");
    let rec = s.record(1);
    assert!(rec.contains("\"state\":\"packing\"") && rec.contains("\"hidden\":1") && rec.contains("\"lines\":5") && rec.contains("\"tty\":false"), "{rec}");
    // fewer-pack's own archive and index of the same capture, made before packing
    let x = s.base.join("x.cmf");
    assert!(Command::new(&s.fewer).arg("-q").arg("-o").arg(&x).arg(&cap).status().unwrap().success());
    assert!(Command::new(&s.fewer).arg("index").arg(&x).status().unwrap().success());
    let o = s.cmd(&["pack-pending", "--wait"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read(p.join("r000001.cmf")).unwrap(), fs::read(&x).unwrap());
    assert_eq!(fs::read(p.join("r000001.cmf.idx")).unwrap(), fs::read(s.base.join("x.cmf.idx")).unwrap());
    assert!(!cap.exists() && !raw.join("r000001.err").exists());
    // the real catalog was made by pack-pending (no stand-in any more): ready, with a log type
    let rec = s.record(1);
    assert!(rec.contains("\"state\":\"ready\"") && rec.contains("\"format\":") && p.join("r000001.cat").is_file(), "{rec}");
}

#[test]
fn a_run_starts_pack_pending_itself() {
    let Some(fewer) = fewer() else { return };
    let s = Store::new("detached", fewer);
    let o = s.run(&["sh", "-c", "echo hello"], false);
    assert_eq!(o.stdout, b"hello\n");
    // pack-pending --wait waits for the detached one (raw/pack.lock), then finds nothing to do
    let o = s.cmd(&["pack-pending", "--wait"]).output().unwrap();
    assert!(o.status.success());
    let (p, raw) = s.dirs();
    assert!(p.join("r000001.cmf").is_file() && !raw.join("r000001.out").exists());
    assert!(s.record(1).contains("\"state\":\"ready\""));
}

#[test]
fn usage_errors() {
    // (no fewer is run here)
    let s = Store::new("usage", PathBuf::from("/nonexistent/fewer"));
    for args in [&["run"][..], &["run", "--"], &["run", "-x"], &["pack-pending", "--now"], &["nothing"]] {
        let o = s.cmd(args).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
    }
    let o = s.cmd(&["run", "--help"]).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(Path::new(BIN).is_file());
}
