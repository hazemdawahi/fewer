//! `fewer add` end to end (checks/add_check.py checks the rest, with the
//! Loghub copies, every tool, the MCP tools and the system's log): a folder's walk and its
//! counts, a gzip file read by the system's gzip, a file kept as it is, a binary file not
//! read, the same file again (unchanged, grown, rewritten), a missing tool, and
//! --hide-secrets.
//!
//! Stores live in the target folder's tmp/ (CARGO_TARGET_TMPDIR), never the home folder; each
//! command gets an environment built from nothing, with HOME a folder there. The fewer that
//! packs is $FEWER_PACK_BIN, else the repository's own build (../target/release/fewer-pack); without
//! either, the tests that pack fail and say how to build it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fewer");

/// $FEWER_PACK_BIN, else the repository's own build of fewer-pack (in $CARGO_TARGET_DIR when it
/// is set, else ../target).
fn pack_bin() -> PathBuf {
    match std::env::var_os("FEWER_PACK_BIN").filter(|v| !v.is_empty()) {
        Some(b) => PathBuf::from(b),
        None => std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("target")).join("release").join("fewer-pack"),
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
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("add-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (home, proj) = (base.join("home"), base.join("proj"));
        for d in [&home, &proj, &base.join("fakehome"), &base.join("files"), &base.join("no-programs")] {
            fs::create_dir_all(d).unwrap();
        }
        Store { base, home, proj, fewer }
    }

    fn file(&self, rel: &str, data: &[u8]) -> PathBuf {
        let p = self.base.join("files").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        p
    }

    /// `add ARGS`, with PATH the system's folders, or a folder with no program in it: (exit
    /// status, stdout, stderr).
    fn add(&self, args: &[&Path], programs: bool) -> (i32, String, String) {
        let path = if programs { PathBuf::from("/usr/bin:/bin") } else { self.base.join("no-programs") };
        let out = Command::new(BIN)
            .arg("add")
            .args(args)
            .env_clear().env("FEWER_DEV", "1")
            .env("PATH", path)
            .env("HOME", self.base.join("fakehome"))
            .env("FEWER_HOME", &self.home)
            .env("FEWER_PACK_BIN", &self.fewer)
            .env("FEWER_PROJECT", &self.proj)
            .current_dir(&self.proj)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn project(&self) -> PathBuf {
        fs::read_dir(self.home.join("projects")).unwrap().next().unwrap().unwrap().path()
    }

    /// runs.jsonl's lines.
    fn records(&self) -> Vec<String> {
        fs::read_to_string(self.project().join("runs.jsonl")).unwrap().lines().map(str::to_string).collect()
    }

    /// The text a run's archive holds (`fewer-pack cat`).
    fn text(&self, id: u64) -> Vec<u8> {
        let out = Command::new(&self.fewer).arg("cat").arg(self.project().join(format!("r{id:06}.cmf"))).output().unwrap();
        assert!(out.status.success(), "fewer-pack cat r{id}");
        out.stdout
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// The run id of the `saved PATH as PID/rN: ...` line of a path.
fn saved_id(out: &str, path: &Path) -> Option<u64> {
    let start = format!("saved {} as ", path.display());
    let line = out.lines().find(|l| l.starts_with(&start))?;
    let rest = &line[start.len()..];
    rest[rest.find("/r")? + 2..rest.find(": ")?].parse().ok()
}

fn lines(n: usize, tag: &str) -> Vec<u8> {
    (0..n).map(|i| format!("2026-11-18 10:{:02}:{:02} INFO {tag} request {i} done\n", i / 60 % 60, i % 60)).collect::<String>().into_bytes()
}

#[test]
fn folders_packed_kept_and_not_read() {
    let Some(c) = fewer() else { return };
    let s = Store::new("folder", c);
    let app = s.file("tree/app.log", &lines(300, "app"));
    s.file("tree/README.md", b"# not a log name\n");
    s.file("tree/.hidden.log", b"hidden\n");
    s.file("tree/node_modules/x.log", b"passed over\n");
    s.file("tree/bin.log", b"a\0binary\n");
    s.file("tree/old.log.zst", b"\x28\xb5\x2f\xfd made-up zstd frame");
    let syslog = s.file("tree/logs/syslog", &lines(10, "sys"));
    // a gzip file, made by the system's gzip when it is there
    let gz = s.base.join("files").join("tree").join("sub").join("api.log.gz");
    let gzip = Path::new("/usr/bin/gzip").is_file().then(|| {
        fs::create_dir_all(gz.parent().unwrap()).unwrap();
        let out = Command::new("/usr/bin/gzip").arg("-c").arg(&app).output().unwrap();
        fs::write(&gz, out.stdout).unwrap();
    });
    let (code, out, err) = s.add(&[&s.base.join("files").join("tree")], true);
    assert_eq!(code, 0, "{err}");
    let took = if gzip.is_some() { 5 } else { 4 };
    let summary = out.lines().last().unwrap();
    assert_eq!(
        summary,
        format!(
            "folder {}: took {took} files ({} saved, 1 kept as it is, 1 not read); passed over 2 files (1 not named as logs, 1 hidden) and 1 folder (.git, node_modules, target or hidden)",
            s.base.join("files").join("tree").display(),
            took - 2
        )
    );
    let id = saved_id(&out, &app).unwrap();
    assert_eq!(s.text(id), fs::read(&app).unwrap());
    assert!(saved_id(&out, &syslog).is_some(), "a file without a dot in a logs folder is taken: {out}");
    if gzip.is_some() {
        let id = saved_id(&out, &gz).unwrap();
        assert_eq!(s.text(id), fs::read(&app).unwrap(), "read by gzip: the unpacked text");
        assert!(out.contains("; read by gzip"));
    }
    assert!(out.contains("bin.log (recorded as ") && out.contains("not text: NUL bytes"), "{out}");
    assert!(out.contains("old.log.zst as ") && out.contains("a zstd file: read in a later version"), "{out}");
    let recs = s.records().join("\n");
    assert!(recs.contains("\"kind\":\"kept\"") && recs.contains("\"kind\":\"not-read\""));
    // the folder again: nothing new
    let n = s.records().len();
    let (code, out, _) = s.add(&[&s.base.join("files").join("tree")], true);
    assert_eq!(code, 0);
    assert!(out.lines().last().unwrap().contains(&format!("took {took} files ({took} already saved)")), "{out}");
    assert_eq!(s.records().len(), n, "no record added");
    // without the tool: kept as it is, with its note
    if gzip.is_some() {
        let copy = s.file("other/api.log.gz", &fs::read(&gz).unwrap());
        let (code, out, _) = s.add(&[&copy], false);
        assert_eq!(code, 0);
        assert!(out.starts_with(&format!("kept {} as ", copy.display())) && out.contains("not read: gzip is not installed (not found on PATH)"), "{out}");
    }
}

#[test]
fn the_same_file_again() {
    let Some(c) = fewer() else { return };
    let s = Store::new("again", c);
    let (a, b) = (lines(200, "first"), lines(100, "second"));
    let f = s.file("grows.log", &a);
    let (_, out, _) = s.add(&[&f], true);
    let r1 = saved_id(&out, &f).unwrap();
    // unchanged
    let n = s.records().len();
    let (code, out, _) = s.add(&[&f], true);
    assert_eq!((code, out.trim_end()), (0, format!("{}: already saved as {}/r{r1}, unchanged", f.display(), s.project().file_name().unwrap().to_string_lossy()).as_str()));
    assert_eq!(s.records().len(), n);
    // grown: only the new part, continuing r1
    fs::write(&f, [a.clone(), b.clone()].concat()).unwrap();
    let (_, out, _) = s.add(&[&f], true);
    let r2 = saved_id(&out, &f).unwrap();
    assert!(out.trim_end().ends_with(&format!("; its new part, after r{r1}")), "{out}");
    assert_eq!(s.text(r2), b);
    let recs = s.records().join("\n");
    assert!(recs.contains(&format!("\"continues\":{r1}")) && recs.contains(&format!("\"offset\":{}", a.len())));
    // rewritten: saved whole again
    let mut c = [a, b].concat();
    c[0] = b'X';
    fs::write(&f, &c).unwrap();
    let (_, out, _) = s.add(&[&f], true);
    let r3 = saved_id(&out, &f).unwrap();
    assert!(out.contains(&format!("saved whole again: it no longer starts with what r{r2} read")), "{out}");
    assert_eq!(s.text(r3), c);
}

#[test]
fn hide_secrets_on_request() {
    let Some(c) = fewer() else { return };
    let s = Store::new("secrets", c);
    let text = b"2026-11-18 10:00:00 WARN login password=hunter22 failed\n2026-11-18 10:00:01 INFO key AKIAIOSFODNN7EXAMPLE\n";
    let kept = s.file("kept.log", text);
    let hidden = s.file("hidden.log", text);
    let (_, out, _) = s.add(&[&kept], true);
    assert_eq!(s.text(saved_id(&out, &kept).unwrap()), text, "files keep every byte by default");
    let out = Command::new(BIN)
        .args(["add", "--hide-secrets"])
        .arg(&hidden)
        .env_clear().env("FEWER_DEV", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", s.base.join("fakehome"))
        .env("FEWER_HOME", &s.home)
        .env("FEWER_PACK_BIN", &s.fewer)
        .env("FEWER_PROJECT", &s.proj)
        .current_dir(&s.proj)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.contains("; 2 secrets hidden"), "{out}");
    let got = String::from_utf8(s.text(saved_id(&out, &hidden).unwrap())).unwrap();
    assert!(!got.contains("hunter22") && !got.contains("AKIAIOSFODNN7EXAMPLE") && got.matches("[hidden: ").count() == 2, "{got}");
}
