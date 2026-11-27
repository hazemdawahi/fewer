//! The memory store: $FEWER_HOME, its projects, each project's run table
//! (runs.jsonl), the project lock and atomic writes.
//!
//!   $FEWER_HOME/projects/<pid>/project        root = /abs/path, name = .., next = 43, created = <ms>
//!   $FEWER_HOME/projects/<pid>/runs.jsonl     one JSON object per line; a later line with the same
//!                                             id updates that run's fields
//!   $FEWER_HOME/projects/<pid>/lock           the project lock (std File::lock)
//!   $FEWER_HOME/projects/<pid>/r000042.cmf    a run's archive, made by fewer-pack (and .cmf.idx)
//!   $FEWER_HOME/tmp/                          snapshots waiting to be packed
//!
//! Readers never lock: archives and indexes appear by rename, and runs.jsonl only grows.
//! The MCP tools only read (`Store` has no write calls; add.rs writes through the functions
//! marked "write").

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use crate::portable::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::{self, Json};

/// The store's folder: $FEWER_HOME, else ~/.fewer (in development, FEWER_DEV, no default: no
/// check touches the real home folder).
pub fn home() -> Result<PathBuf, String> {
    match crate::defaults::setting("FEWER_HOME") {
        Some(h) => {
            let p = PathBuf::from(h);
            if !p.is_absolute() {
                return Err("FEWER_HOME must be an absolute path".into());
            }
            Ok(p)
        }
        None => Err("FEWER_HOME is not set: set it to the memory store's folder (an absolute path)".into()),
    }
}

/// A run's file name: r000042.cmf (6 digits, so `ls` sorts them).
pub fn run_file(id: u64) -> String {
    format!("r{id:06}.cmf")
}

/// FNV-1a, 64 bits.
pub fn fnv1a64(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &c in b {
        h = (h ^ c as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A project's name from its root: the last part, lowercased, keeping ASCII letters, digits
/// and '-', at most 16 bytes ("root" when nothing is left).
pub fn project_name(root: &Path) -> String {
    let last = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut n: String = last.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').map(|c| c.to_ascii_lowercase()).collect();
    n.truncate(16);
    if n.is_empty() { "root".into() } else { n }
}

/// The project folder's root: FEWER_PROJECT, else the git root of `cwd`, else `cwd`.
pub fn project_root(cwd: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("FEWER_PROJECT").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return std::path::absolute(&p).unwrap_or(p);
    }
    let args = [OsStr::new("-C"), cwd.as_os_str(), OsStr::new("rev-parse"), OsStr::new("--show-toplevel")];
    if let Some(out) = run_quiet("git", &args, Duration::from_secs(2)) {
        let s = String::from_utf8_lossy(&out).trim_end_matches(['\n', '\r']).to_string();
        if !s.is_empty() && Path::new(&s).is_absolute() {
            return PathBuf::from(s);
        }
    }
    cwd.to_path_buf()
}

/// Run a read-only helper program (git) with no input, its output captured, within a time
/// limit: its stdout when it exits 0.
pub fn run_quiet(prog: &str, args: &[&OsStr], limit: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(prog)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut so = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if t0.elapsed() < limit => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    status.filter(|s| s.success()).map(|_| out)
}

/// A project of the store.
#[derive(Clone, Debug)]
pub struct Project {
    pub pid: String,
    pub root: String,
    pub dir: PathBuf,
}

/// "key = value" lines ('#' starts a comment).
pub fn read_settings(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn setting<'a>(s: &'a [(String, String)], key: &str) -> Option<&'a str> {
    s.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A project read from its folder (None when it has no readable `project` file).
pub fn read_project(dir: &Path) -> Option<Project> {
    let text = fs::read_to_string(dir.join("project")).ok()?;
    let s = read_settings(&text);
    let pid = dir.file_name()?.to_string_lossy().into_owned();
    Some(Project { pid, root: setting(&s, "root")?.to_string(), dir: dir.to_path_buf() })
}

/// The store's projects (sorted by pid).
pub fn list_projects(home: &Path) -> Vec<Project> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(home.join("projects")) {
        for e in rd.flatten() {
            if let Some(p) = read_project(&e.path()) {
                v.push(p);
            }
        }
    }
    v.sort_by(|a, b| a.pid.cmp(&b.pid));
    v
}

/// The pid of a project root: name + "-" + the first 4 hex digits of
/// FNV-1a-64 of the root path; when that pid's `project` file names another root, 6 hex
/// digits, then 8, then 16. Also whether the project exists in the store.
pub fn pid_for(home: &Path, root: &Path) -> (String, bool) {
    let name = project_name(root);
    let hex = format!("{:016x}", fnv1a64(root.as_os_str().as_encoded_bytes()));
    let rs = root.to_string_lossy();
    for n in [4, 6, 8, 16] {
        let pid = format!("{name}-{}", &hex[..n]);
        match read_project(&home.join("projects").join(&pid)) {
            Some(p) if p.root == rs => return (pid, true),
            Some(_) => continue,
            None => return (pid, false),
        }
    }
    (format!("{name}-{hex}"), false)
}

/// A run's merged record.
#[derive(Clone, Debug)]
pub struct RunRec(pub Json);

impl RunRec {
    pub fn id(&self) -> u64 {
        self.0.get("id").and_then(Json::int).unwrap_or(0) as u64
    }
    pub fn s(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Json::str)
    }
    pub fn i(&self, k: &str) -> Option<i64> {
        self.0.get(k).and_then(Json::int)
    }
    pub fn kind(&self) -> &str {
        self.s("kind").unwrap_or("?")
    }
    pub fn state(&self) -> &str {
        self.s("state").unwrap_or("?")
    }
}

/// The runs of a project folder, as `read_runs_all` reads them, without the records forgotten
/// session pieces leave (state "forgotten", `memory forget`): they hold no text, and only the
/// sessions sweep and `memory` read them.
pub fn read_runs(dir: &Path) -> Vec<RunRec> {
    let mut v = read_runs_all(dir);
    v.retain(|r| r.state() != "forgotten");
    v
}

/// Every run record of a project folder: runs.jsonl merged by id (a later line with the same
/// id updates that run's fields), sorted by id. A last line cut short (a crash while it was
/// written) is ignored, and so is any line that is not a JSON object with an id.
pub fn read_runs_all(dir: &Path) -> Vec<RunRec> {
    let Ok(b) = fs::read(dir.join("runs.jsonl")) else { return Vec::new() };
    let mut runs: BTreeMap<u64, Json> = BTreeMap::new();
    let complete = match b.iter().rposition(|&c| c == b'\n') {
        Some(p) => &b[..p + 1],
        None => &b[..0],
    };
    for line in complete.split(|&c| c == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v @ Json::Obj(_)) = json::parse(line) else { continue };
        let Some(id) = v.get("id").and_then(Json::int).filter(|&i| i > 0) else { continue };
        let rec = runs.entry(id as u64).or_insert_with(|| Json::Obj(Vec::new()));
        if let Json::Obj(fields) = v {
            for (k, x) in fields {
                rec.set(&k, x);
            }
        }
    }
    runs.into_values().map(RunRec).collect()
}

// ---------------------------------------------------------------- writing (add.rs only)

/// Write a file under a temporary name in the same folder (<name>.tmp-<process id>), then
/// give it its name: readers see the old file or the new one, never a part. Its mode is
/// 0666 less the umask, as fewer-pack's outputs.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let r = (|| {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o666).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Remove temporary files (<name>.tmp-<pid>) in `dir` whose writer no longer runs.
pub fn clear_leftovers(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some((_, pid)) = name.rsplit_once(".tmp-")
            && let Ok(pid) = pid.parse::<u32>()
            && pid != std::process::id()
            && !crate::sys::alive(pid)
        {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// The project lock, held while it lives.
pub struct Lock(fs::File);

impl Lock {
    pub fn take(dir: &Path) -> io::Result<Lock> {
        let f = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(dir.join("lock"))?;
        f.lock()?;
        Ok(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The project of `root` in the store, made when it is new (under its lock).
/// The store's own folder made when it is not there, readable by its owner alone (it holds
/// command lines, folders, branches and log keys); one that is there is left as it is.
pub fn make_home(home: &Path) -> io::Result<()> {
    use crate::portable::DirBuilderExt;
    if home.is_dir() {
        return Ok(());
    }
    if let Some(parent) = home.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(home) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

pub fn ensure_project(home: &Path, root: &Path) -> io::Result<Project> {
    let (pid, _) = pid_for(home, root);
    let dir = home.join("projects").join(&pid);
    make_home(home)?;
    fs::create_dir_all(&dir)?;
    let _l = Lock::take(&dir)?;
    clear_leftovers(&dir);
    if read_project(&dir).is_none() {
        let text = format!("root = {}\nname = {}\nnext = 1\ncreated = {}\n", root.display(), project_name(root), crate::times::now_ms());
        write_atomic(&dir.join("project"), text.as_bytes())?;
    }
    Ok(Project { pid, root: root.to_string_lossy().into_owned(), dir })
}

/// Take the project's next run id (ids are never used twice) and append the run's first
/// record, under the lock.
pub fn new_run(p: &Project, first: Json) -> io::Result<u64> {
    let _l = Lock::take(&p.dir)?;
    let text = fs::read_to_string(p.dir.join("project"))?;
    let mut s = read_settings(&text);
    let next: u64 = setting(&s, "next").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    // (an id is never used twice, even if `next` was lost: past every run in the table)
    let id = next.max(read_runs_all(&p.dir).last().map_or(0, |r| r.id() + 1));
    match s.iter_mut().find(|(k, _)| k == "next") {
        Some(e) => e.1 = (id + 1).to_string(),
        None => s.push(("next".into(), (id + 1).to_string())),
    }
    let body: String = s.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
    write_atomic(&p.dir.join("project"), body.as_bytes())?;
    let mut rec = Json::o(vec![("id", Json::from(id))]);
    if let Json::Obj(f) = first {
        for (k, v) in f {
            rec.set(&k, v);
        }
    }
    append_locked(&p.dir, &rec)?;
    Ok(id)
}

/// Append a record (fields of run `id`) to runs.jsonl, under the lock.
pub fn update_run(p: &Project, id: u64, fields: Json) -> io::Result<()> {
    let _l = Lock::take(&p.dir)?;
    let mut rec = Json::o(vec![("id", Json::from(id))]);
    if let Json::Obj(f) = fields {
        for (k, v) in f {
            rec.set(&k, v);
        }
    }
    append_locked(&p.dir, &rec)
}

/// Append a record to runs.jsonl; the caller holds the project lock (a DNA index's file and
/// its record are put in place under it, only while its run is still in the store).
pub fn append_locked(dir: &Path, rec: &Json) -> io::Result<()> {
    let mut line = rec.to_string();
    line.push('\n');
    let path = dir.join("runs.jsonl");
    let mut f = fs::OpenOptions::new().append(true).create(true).mode(0o644).open(&path)?;
    // a last line cut short by a crash is ended first, so this record starts a line
    let len = f.metadata()?.len();
    if len > 0 {
        let mut r = fs::File::open(&path)?;
        let mut last = [0u8; 1];
        io::Seek::seek(&mut r, io::SeekFrom::Start(len - 1))?;
        r.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.insert(0, '\n');
        }
    }
    f.write_all(line.as_bytes())?;
    f.sync_data()
}

/// How long a page or an answer kept whole stays (days): it is not kept for good.
const KEEP_DAYS: u64 = 7;

/// Keep a page or an answer whole in a file (the store's addons folder, named so by earlier
/// versions, else TMPDIR): its path. The folder and the files are the user's alone (0700, 0600),
/// each answer gets a name of its own (two answers in one millisecond do not meet), and answers
/// older than KEEP_DAYS are removed.
pub(crate) fn keep_whole(text: &str, name: &str, what: &str) -> Option<PathBuf> {
    use std::io::Write as _;
    use crate::portable::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    static SWEPT: AtomicBool = AtomicBool::new(false);
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>();
    let dir = crate::store::home()
        .ok()
        .map(|h| h.join("addons"))
        .filter(|d| std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d).is_ok() && std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).is_ok())
        .unwrap_or_else(std::env::temp_dir);
    if !SWEPT.swap(true, Ordering::Relaxed) && dir.ends_with("addons") {
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(KEEP_DAYS * 86_400);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "txt") && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < old) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    for _ in 0..8 {
        let file = dir.join(format!("{}-{}-{}-{}-{}.txt", safe(name), safe(what), crate::times::now_ms(), std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file) {
            Ok(mut f) => return f.write_all(text.as_bytes()).ok().map(|()| file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for a test, inside this crate's target folder.
    pub fn tmpdir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_and_pids() {
        assert_eq!(project_name(Path::new("/x/My_Project.v2")), "myprojectv2");
        assert_eq!(project_name(Path::new("/x/Très-long-name-of-a-project-folder")), "trs-long-name-of");
        assert_eq!(project_name(Path::new("/")), "root");
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        let home = tmpdir("pids");
        let (a, exists) = pid_for(&home, Path::new("/w/app"));
        assert!(!exists && a.starts_with("app-") && a.len() == 8);
        let p = ensure_project(&home, Path::new("/w/app")).unwrap();
        assert_eq!(p.pid, a);
        assert_eq!(pid_for(&home, Path::new("/w/app")), (a.clone(), true));
        // another root whose short pid is taken gets 6 hex digits
        fs::write(home.join("projects").join(&a).join("project"), "root = /elsewhere/app\n").unwrap();
        let (b, exists) = pid_for(&home, Path::new("/w/app"));
        assert!(!exists && b.len() == 10 && b.starts_with(&a));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn runs_table() {
        let home = tmpdir("runs");
        let p = ensure_project(&home, Path::new("/w/proj")).unwrap();
        let a = new_run(&p, Json::o(vec![("kind", "add".into()), ("state", "packing".into())])).unwrap();
        let b = new_run(&p, Json::o(vec![("kind", "add".into()), ("state", "packing".into())])).unwrap();
        assert_eq!((a, b), (1, 2));
        update_run(&p, a, Json::o(vec![("state", "ready".into()), ("lines", Json::from(7u64))])).unwrap();
        // a line cut short by a crash is ignored, and the next record still starts a line
        let mut f = fs::OpenOptions::new().append(true).open(p.dir.join("runs.jsonl")).unwrap();
        f.write_all(b"{\"id\":2,\"state\":\"rea").unwrap();
        let runs = read_runs(&p.dir);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].state(), runs[0].i("lines")), ("ready", Some(7)));
        assert_eq!(runs[1].state(), "packing");
        update_run(&p, b, Json::o(vec![("state", "failed".into())])).unwrap();
        let runs = read_runs(&p.dir);
        assert_eq!(runs[1].state(), "failed");
        // ids are never used again
        let c = new_run(&p, Json::o(vec![("kind", "add".into())])).unwrap();
        assert_eq!(c, 3);
        fs::remove_dir_all(&home).unwrap();
    }
}
