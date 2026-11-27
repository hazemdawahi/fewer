//! When DNA indexes are made (on by default): for every saved run with at least `dna_min` (16 MiB) of text, after saving and
//! while the computer is idle, at background priority; never inside the save itself.
//!
//! - **Settings** (dna.rs `settings`): `dna = off` in the store's config turns it off: nothing
//!   is built, and search uses no .dna file. `memory dna on|off` sets it (memory.rs).
//! - **After saving**: when `add` or pack-pending saved runs that big, `after_save` starts
//!   `fewer dna-pass PID ID...` detached (a new session, its stdio /dev/null, not
//!   waited for), so the save goes on at once. The pass runs at background priority and
//!   builds those runs' DNA indexes on one thread.
//! - **While idle**: the MCP server's `IdleBuilder` thread, at background priority, every 60 s
//!   while the 1-minute load is under 25% of the cores, builds the DNA index of the biggest
//!   run of the store that has none. It asks the load again before each segment and block,
//!   and stops when it rose. A run whose build failed (a damaged archive) is passed over;
//!   `memory dna build` tries it again.
//! - **Now**: `memory dna build` builds every missing one in the foreground, on all cores,
//!   because the person asked.
//! - **A size cap**: a DNA index that would take more than `dna_max` (20%) of its run's archive,
//!   and more than 1 MiB, is not kept; the run records `dna_would_be` (its size), and search
//!   works without it, as before. `dna_max = none` in the config: no cap. A run over the cap is
//!   built again when the cap is raised past it (`memory dna build`, the idle builder).
//! - **One pass at a time**, through the store's lock raw/dna.lock: a pass after saving and
//!   `memory dna build` wait for it; the idle builder tries it and waits for its next turn.
//! - **Each run's record** gets `dna_bytes` and `dna_build_ms` when its DNA
//!   index is made, or `dna_error` when it could not be (a later success clears it). The
//!   file is written beside the archive through a temporary name, then put in place and
//!   recorded under the project lock, only while the run is still in the store with the same
//!   archive: a run forgotten meanwhile gets neither.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use crate::portable::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::answer::{group, run_ref};
use crate::dna::{self, Settings};
use crate::json::Json;
use crate::store::{self, Project, RunRec};
use crate::{sys_idle, sys_run};

/// The idle builder's turn comes every 60 s.
const IDLE_EVERY: Duration = Duration::from_secs(60);
/// The store's DNA lock, in its raw/ folder (as pack.lock and sessions.lock).
const LOCK: &str = "dna.lock";

fn debug(msg: &str) {
    crate::mcp::debug(msg)
}

/// The store folder a project is in (projects/<pid> is in it).
fn home_of(p: &Project) -> Option<PathBuf> {
    p.dir.parent()?.parent().map(Path::to_path_buf)
}

/// A share of a whole, for messages: "2.48%".
pub fn share(part: u64, whole: u64) -> String {
    if whole == 0 { "-".into() } else { format!("{:.2}%", 100.0 * part as f64 / whole as f64) }
}

/// The size cap's default: a DNA index over this share (%) of its run's archive is not kept.
pub const MAX_SHARE: u64 = 20;
/// ... unless it is this small: a small index costs nothing worth saving (a log that packs to
/// 708 bytes had an index of 314 bytes, 44% of it).
pub const MAX_FLOOR: u64 = 1 << 20;
/// What a build over the cap answers (its run records `dna_would_be` instead of an error).
const OVER: &str = "over the size cap";

/// The size cap in percent of the archive, from the config's `dna_max` ("20%", "20", or
/// "none" for no cap); a value that cannot be read keeps the default.
pub fn max_share(home: &Path) -> Option<u64> {
    let c = crate::config::read(home);
    match crate::config::get(&c, "dna_max").map(str::trim) {
        None => Some(MAX_SHARE),
        Some(v) if v.eq_ignore_ascii_case("none") => None,
        Some(v) => Some(v.trim_end_matches('%').trim().parse::<u64>().ok().filter(|&m| m > 0).unwrap_or(MAX_SHARE)),
    }
}

/// Whether a DNA index of `dna` bytes is over the cap `max` (%) for an archive of `archive`
/// bytes: more than that share of it, and more than MAX_FLOOR.
pub fn over_cap(dna: u64, archive: u64, max: Option<u64>) -> bool {
    max.is_some_and(|m| dna > MAX_FLOOR && dna.saturating_mul(100) > archive.saturating_mul(m))
}

/// The cap of the store a project is in.
fn cap_of(p: &Project) -> Option<u64> {
    home_of(p).map_or(Some(MAX_SHARE), |h| max_share(&h))
}

/// Whether run `r` was found over the cap, and still is under the cap set now.
pub fn over(p: &Project, r: &RunRec) -> bool {
    r.i("dna_would_be").is_some_and(|b| over_cap(b.max(0) as u64, r.i("archive").unwrap_or(0).max(0) as u64, cap_of(p)))
}

/// Whether run `r` of project `p` should have a DNA index under settings `s`: the index is
/// on; the run is saved (ready), a log the store packed (not a file kept as-is or not read),
/// with at least dna_min bytes of text; its archive is there; and it was not found over the
/// size cap (the cap set now).
pub fn wants(p: &Project, r: &RunRec, s: &Settings) -> bool {
    big_enough(p, r, s) && !over(p, r)
}

/// wants, but for the size cap.
fn big_enough(p: &Project, r: &RunRec, s: &Settings) -> bool {
    s.on && r.state() == "ready"
        && !matches!(r.kind(), "kept" | "not-read")
        && r.i("bytes").is_some_and(|b| b >= 0 && b as u64 >= s.min)
        && p.dir.join(store::run_file(r.id())).is_file()
}

/// Whether run `r` has a DNA index that fits its archive. `full`: the whole file read and
/// checked (dna::Dna::read); else its first bytes, and its size against the run's dna_bytes.
pub fn has_one(p: &Project, r: &RunRec, full: bool) -> bool {
    let archive = p.dir.join(store::run_file(r.id()));
    let path = dna::dna_path(&archive);
    let Ok((len, end)) = crate::catalog::archive_id(&archive) else { return false };
    let Ok(m) = fs::metadata(&path) else { return false };
    if !m.is_file() {
        return false;
    }
    if full {
        fs::read(&path).is_ok_and(|d| dna::Dna::read(d, len, end).is_ok())
    } else {
        r.i("dna_bytes") == Some(m.len() as i64) && dna::made_for(&path, len, end)
    }
}

/// What a build made: the DNA index's bytes and build time, and its archive's bytes.
#[derive(Clone, Copy, Debug)]
pub struct Built {
    pub bytes: u64,
    pub ms: u64,
    pub archive: u64,
}

/// What a build of a run gives when the run left the store, or its archive changed, while it
/// was built: nothing is written or recorded.
const GONE: &str = "forgotten or changed while its DNA index was built";

/// Build the DNA index of run `id` of project `p` (its blocks opened on `threads` threads;
/// `go_on` asked before each segment and block), put it in place and record it. When `go_on`
/// stops it (dna::STOPPED) or the run is gone, nothing is written or recorded; any other
/// failure is recorded as the run's dna_error.
pub fn build_run(p: &Project, id: u64, threads: usize, go_on: &mut dyn FnMut() -> bool) -> Result<Built, String> {
    let archive = p.dir.join(store::run_file(id));
    let had_error = store::read_runs(&p.dir).iter().any(|r| r.id() == id && r.s("dna_error").is_some());
    let made = (|| {
        let was = crate::catalog::archive_id(&archive)?;
        let t0 = Instant::now();
        let bytes = dna::build(&archive, threads, go_on)?;
        let ms = t0.elapsed().as_millis() as u64;
        // the size cap: not kept, and its would-be size recorded (no error)
        let alen = fs::metadata(&archive).map_or(0, |m| m.len());
        let max = cap_of(p);
        if over_cap(bytes.len() as u64, alen, max) {
            let mut f = vec![("dna_would_be", Json::from(bytes.len() as u64))];
            if had_error {
                f.push(("dna_error", Json::Null));
            }
            return match commit(p, id, &archive, Some(was), None, Json::o(f)) {
                Ok(true) => Err(format!(
                    "{OVER}: its DNA index would take {} bytes, {} of its {}-byte archive (dna_max = {}%); none is kept, and search works without it",
                    group(bytes.len() as u64),
                    share(bytes.len() as u64, alen),
                    group(alen),
                    max.unwrap_or(MAX_SHARE)
                )),
                Ok(false) => Err(GONE.to_string()),
                Err(e) => Err(e),
            };
        }
        let path = dna::dna_path(&archive);
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let r = (|| {
            write_new(&tmp, &bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            let mut f = vec![("dna_bytes", Json::from(bytes.len() as u64)), ("dna_build_ms", Json::from(ms))];
            if had_error {
                f.push(("dna_error", Json::Null));
            }
            commit(p, id, &archive, Some(was), Some((&tmp, &path)), Json::o(f))
        })();
        if !matches!(r, Ok(true)) {
            let _ = fs::remove_file(&tmp);
        }
        match r {
            Ok(true) => Ok(Built { bytes: bytes.len() as u64, ms, archive: was.0 }),
            Ok(false) => Err(GONE.to_string()),
            Err(e) => Err(e),
        }
    })();
    if let Err(e) = &made
        && e != dna::STOPPED
        && e != GONE
        && !e.starts_with(OVER)
    {
        let _ = commit(p, id, &archive, None, None, Json::o(vec![("dna_error", e.as_str().into())]));
    }
    made
}

/// Write a new file (mode 0666 less the umask, as the store's other files) and sync it.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o666).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Under the project lock, while run `id` is still in the store and ready, with the archive it
/// had when its build started (`was`; None: not asked): put a file in place (`file`: its
/// temporary name and its name) and append `fields` to the run's record. Ok(false) when the
/// run is not there as it was: then nothing is done.
fn commit(p: &Project, id: u64, archive: &Path, was: Option<(u64, [u8; 4])>, file: Option<(&Path, &Path)>, fields: Json) -> Result<bool, String> {
    let _l = store::Lock::take(&p.dir).map_err(|e| format!("cannot lock its project: {e}"))?;
    let there = store::read_runs(&p.dir).iter().any(|r| r.id() == id && r.state() == "ready");
    let same = was.is_none_or(|w| crate::catalog::archive_id(archive).ok() == Some(w));
    if !there || !same {
        return Ok(false);
    }
    if let Some((tmp, path)) = file {
        fs::rename(tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let mut rec = Json::o(vec![("id", Json::from(id))]);
    if let Json::Obj(f) = fields {
        for (k, v) in f {
            rec.set(&k, v);
        }
    }
    store::append_locked(&p.dir, &rec).map_err(|e| format!("cannot write the run table: {e}"))?;
    Ok(true)
}

/// The store's DNA lock: one pass at a time. `wait`: wait for it; else None when another
/// pass holds it. Held until the file is dropped.
fn lock(home: &Path, wait: bool) -> Result<Option<File>, String> {
    let raw = home.join("raw");
    let path = raw.join(LOCK);
    let f = fs::create_dir_all(&raw)
        .and_then(|_| OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path))
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if wait {
        f.lock().map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        return Ok(Some(f));
    }
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------------- after saving

/// The id the next run of project `p` gets at the earliest: past every run in its table. The
/// runs a command saves are those from it on (`runs_from`).
pub fn next_id(p: &Project) -> u64 {
    store::read_runs_all(&p.dir).last().map_or(1, |r| r.id() + 1)
}

/// The runs of project `p` from id `first` on.
pub fn runs_from(p: &Project, first: u64) -> Vec<u64> {
    store::read_runs(&p.dir).iter().map(RunRec::id).filter(|&id| id >= first).collect()
}

/// After `add` or pack-pending saved runs `ids` of project `p`: when the DNA index is on and
/// some of them hold dna_min bytes of text or more, a detached pass builds their DNA indexes
/// (`dna-pass`). It never slows the save: nothing is waited for, and a pass that cannot start
/// is only noted in the debug log (the idle builder catches the run later).
pub fn after_save(p: &Project, ids: &[u64]) {
    let Some(home) = home_of(p) else { return };
    let s = dna::settings(&home);
    if !s.on || ids.is_empty() {
        return;
    }
    let big: Vec<u64> = store::read_runs(&p.dir).iter().filter(|r| ids.contains(&r.id()) && wants(p, r, &s)).map(RunRec::id).collect();
    if big.is_empty() {
        return;
    }
    if let Err(e) = spawn_pass(&p.pid, &big) {
        debug(&format!("dna: cannot start the pass for {}: {e}", refs(&p.pid, &big)));
    }
}

fn refs(pid: &str, ids: &[u64]) -> String {
    ids.iter().map(|&id| run_ref(pid, id)).collect::<Vec<_>>().join(", ")
}

/// Start `fewer dna-pass PID ID...` detached: a new session, its stdio /dev/null, not
/// waited for (as pack.rs starts pack-pending).
fn spawn_pass(pid: &str, ids: &[u64]) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    c.arg("dna-pass").arg(pid).args(ids.iter().map(u64::to_string)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    sys_run::detach(&mut c);
    c.spawn().map(|_| ())
}

const PASS_USAGE: &str = "usage: fewer dna-pass PROJECT RUN... (started by add and pack-pending)";

/// `fewer dna-pass PID ID...`, started by `after_save`: at background priority and
/// under the store's DNA lock, build the DNA indexes of these runs of project PID that still
/// want one. Its notes go to the debug log (its stdio is /dev/null). Exit status 1 when one
/// could not be built.
pub fn cmd_dna_pass(args: Vec<OsString>) -> i32 {
    let a: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let ids: Option<Vec<u64>> = a.iter().skip(1).map(|s| s.parse().ok()).collect();
    let (Some(pid), Some(ids)) = (a.first(), ids) else {
        eprintln!("{PASS_USAGE}");
        return 2;
    };
    if ids.is_empty() || pid.contains('/') || pid.starts_with('.') {
        eprintln!("{PASS_USAGE}");
        return 2;
    }
    let prio = match sys_idle::background() {
        Ok(()) => sys_idle::priority(),
        Err(e) => e,
    };
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fewer dna-pass: {e}");
            return 2;
        }
    };
    let Some(p) = store::read_project(&home.join("projects").join(pid)) else {
        debug(&format!("dna pass: no project {pid} in the store"));
        return 1;
    };
    debug(&format!("dna pass for {}: started, at {prio}", refs(pid, &ids)));
    let _lock = match lock(&home, true) {
        Ok(l) => l,
        Err(e) => {
            debug(&format!("dna pass: {e}"));
            return 1;
        }
    };
    let mut failed = false;
    for id in ids {
        let s = dna::settings(&home);
        let runs = store::read_runs(&p.dir);
        let Some(r) = runs.iter().find(|r| r.id() == id) else { continue };
        if !wants(&p, r, &s) || has_one(&p, r, false) {
            continue;
        }
        match build_run(&p, id, 1, &mut || true) {
            Ok(b) => debug(&format!("dna pass: built {}: {} bytes, {} of its archive, in {} ms", run_ref(pid, id), group(b.bytes), share(b.bytes, b.archive), b.ms)),
            Err(e) => {
                debug(&format!("dna pass: {}: not built: {e}", run_ref(pid, id)));
                failed = true;
            }
        }
    }
    failed as i32
}

// ---------------------------------------------------------------- now: memory dna build

/// The runs of the store that want a DNA index and have none that fits (`full`: each file
/// read whole), the biggest first.
fn missing(home: &Path, s: &Settings, full: bool) -> Vec<(Project, RunRec)> {
    let mut out = Vec::new();
    for p in store::list_projects(home) {
        for r in store::read_runs(&p.dir) {
            if wants(&p, &r, s) && !has_one(&p, &r, full) {
                out.push((p.clone(), r));
            }
        }
    }
    out.sort_by_key(|(p, r)| (std::cmp::Reverse(r.i("bytes").unwrap_or(0)), p.pid.clone(), r.id()));
    out
}

/// `memory dna build`: every run of the store that wants a DNA index and has none that fits
/// (each file read whole), built now in the foreground on all cores, the biggest first, as
/// the person asked. Lines for the person go to `out`. Exit status 0, or 1 when the index is
/// off or one could not be built.
pub fn build_all(home: &Path, out: &mut dyn Write) -> i32 {
    let s = dna::settings(home);
    if !s.on {
        let _ = writeln!(out, "The DNA index is off: nothing is built. `fewer memory dna on` turns it on.");
        return 1;
    }
    let lock = match lock(home, false) {
        Ok(Some(l)) => Ok(l),
        Ok(None) => {
            let _ = writeln!(out, "Another DNA build is running; waiting for it to end.");
            lock(home, true).map(|l| l.expect("a lock waited for"))
        }
        Err(e) => Err(e),
    };
    let _lock = match lock {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(out, "fewer memory dna build: {e}");
            return 1;
        }
    };
    let todo = missing(home, &s, true);
    if todo.is_empty() {
        let over = status(home).over;
        let but = match over {
            0 => String::new(),
            1 => ", but 1 run over the size cap (dna_max)".to_string(),
            n => format!(", but {n} runs over the size cap (dna_max)"),
        };
        let _ = writeln!(out, "Every run with {} of text or more has its DNA index{but}.", size_text(s.min));
        return 0;
    }
    let threads = sys_idle::cores();
    let (mut built, mut failed, mut bytes, mut archives) = (0usize, 0usize, 0u64, 0u64);
    for (p, r) in &todo {
        match build_run(p, r.id(), threads, &mut || true) {
            Ok(b) => {
                let _ = writeln!(out, "built {}: {} bytes, {} of its {}-byte archive, in {} ms", run_ref(&p.pid, r.id()), group(b.bytes), share(b.bytes, b.archive), group(b.archive), group(b.ms));
                built += 1;
                bytes += b.bytes;
                archives += b.archive;
            }
            Err(e) if e.starts_with(OVER) => {
                let _ = writeln!(out, "not kept {}: {e}", run_ref(&p.pid, r.id()));
            }
            Err(e) => {
                let _ = writeln!(out, "not built {}: {e}", run_ref(&p.pid, r.id()));
                failed += 1;
            }
        }
    }
    let _ = writeln!(out, "{built} built: {} bytes, {} of their archives{}", group(bytes), share(bytes, archives), if failed > 0 { format!("; {failed} could not be built") } else { String::new() });
    (failed > 0) as i32
}

/// A size as the config writes it: 16 MiB, 1.5 MB, 300 bytes.
pub fn size_text(n: u64) -> String {
    if n >= 1 << 20 && n.is_multiple_of(1 << 20) {
        format!("{} MiB", n >> 20)
    } else if n >= 1 << 10 && n.is_multiple_of(1 << 10) {
        format!("{} KiB", n >> 10)
    } else {
        format!("{} bytes", group(n))
    }
}

/// What `memory dna` tells: the settings, the runs with a DNA index (their DNA files' and
/// archives' bytes), the runs that want one and have none, the runs below dna_min.
pub struct Status {
    pub settings: Settings,
    pub with: usize,
    pub dna_bytes: u64,
    pub archive_bytes: u64,
    pub missing: usize,
    /// the runs over the size cap (no DNA index kept), and the cap now
    pub over: usize,
    pub max: Option<u64>,
}

pub fn status(home: &Path) -> Status {
    let s = dna::settings(home);
    let on = Settings { on: true, ..s };
    let mut st = Status { settings: s, with: 0, dna_bytes: 0, archive_bytes: 0, missing: 0, over: 0, max: max_share(home) };
    for p in store::list_projects(home) {
        for r in store::read_runs(&p.dir) {
            if !big_enough(&p, &r, &on) {
                continue;
            }
            if over(&p, &r) && !has_one(&p, &r, false) {
                st.over += 1;
                continue;
            }
            if has_one(&p, &r, false) {
                st.with += 1;
                st.dna_bytes += r.i("dna_bytes").unwrap_or(0) as u64;
                st.archive_bytes += r.i("archive").unwrap_or(0) as u64;
            } else {
                st.missing += 1;
            }
        }
    }
    st
}

// ---------------------------------------------------------------- while idle

/// The MCP server's idle builder: a thread at background priority that, every
/// 60 s while the store has the DNA index on and the computer is idle, builds the DNA index of
/// the biggest run that has none. Dropping it (at the end of serve) stops it: a build in
/// progress stops before its next block.
pub struct IdleBuilder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl IdleBuilder {
    pub fn start(home: PathBuf) -> IdleBuilder {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = std::thread::Builder::new().name("fewer-dna-idle".into()).spawn(move || idle_loop(&home, &st)).ok();
        IdleBuilder { stop, thread }
    }
}

impl Drop for IdleBuilder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The idle builder's turn: every 60 s, or the test switch FEWER_TEST_DNA_IDLE_MS (never set
/// in normal use).
fn idle_every() -> Duration {
    std::env::var("FEWER_TEST_DNA_IDLE_MS").ok().and_then(|v| v.parse().ok()).filter(|&ms| ms > 0).map_or(IDLE_EVERY, Duration::from_millis)
}

/// Wait `d`, in steps of at most 100 ms; false when `stop` was set meanwhile.
fn wait(d: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= end {
            return true;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

/// The biggest run of the store that wants a DNA index and has none, passing over runs
/// whose last build failed.
fn biggest_missing(home: &Path, s: &Settings) -> Option<(Project, RunRec)> {
    missing(home, s, false).into_iter().find(|(_, r)| r.s("dna_error").is_none())
}

/// A load for the log: "1-minute load 4.42 on 14 cores".
fn load_text(load: Option<f64>, cores: usize) -> String {
    match load {
        Some(l) => format!("1-minute load {l:.2} on {cores} cores"),
        None => format!("1-minute load unknown on {cores} cores"),
    }
}

/// The idle builder's thread.
fn idle_loop(home: &Path, stop: &AtomicBool) {
    let prio = match sys_idle::background() {
        Ok(()) => sys_idle::priority(),
        Err(e) => e,
    };
    debug(&format!("dna idle builder: started, at {prio}"));
    let every = idle_every();
    // what was last said, so that a state is said once, not every turn
    let mut said = String::new();
    let mut say = |m: String| {
        if m != said {
            debug(&format!("dna idle builder: {m}"));
            said = m;
        }
    };
    while wait(every, stop) {
        let s = dna::settings(home);
        if !s.on {
            say("the DNA index is off: nothing to build".into());
            continue;
        }
        let (idle, load, cores) = sys_idle::idle();
        if !idle {
            say(format!("not idle ({}; idle is under 25% of the cores): waiting", load_text(load, cores)));
            continue;
        }
        let _lock = match lock(home, false) {
            Ok(Some(l)) => l,
            Ok(None) => {
                say("another DNA pass is building: waiting".into());
                continue;
            }
            Err(e) => {
                say(e);
                continue;
            }
        };
        let Some((p, r)) = biggest_missing(home, &s) else {
            say("every run with dna_min bytes of text or more has its DNA index".into());
            continue;
        };
        let rf = run_ref(&p.pid, r.id());
        say(format!("idle ({}): building {rf}", load_text(load, cores)));
        let mut go_on = || !stop.load(Ordering::SeqCst) && sys_idle::idle().0;
        match build_run(&p, r.id(), 1, &mut go_on) {
            Ok(b) => say(format!("built {rf}: {} bytes, {} of its archive, in {} ms", group(b.bytes), share(b.bytes, b.archive), b.ms)),
            Err(e) if e == dna::STOPPED => {
                let why = if stop.load(Ordering::SeqCst) {
                    "the server ends".to_string()
                } else {
                    let (_, load, cores) = sys_idle::idle();
                    format!("the load rose ({})", load_text(load, cores))
                };
                say(format!("stopped building {rf}: {why}"));
            }
            Err(e) => say(format!("{rf}: not built: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::ArchiveWriter;

    /// A fresh store for a test, inside this crate's target folder.
    fn tmp_home(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-dnab-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A ready run of `text` in project `p`, packed in this process (the engine, as fewer-pack packs).
    fn saved_run(p: &Project, text: &[u8]) -> u64 {
        let id = store::new_run(p, Json::o(vec![("kind", "add".into()), ("state", "packing".into())])).unwrap();
        let mut aw = ArchiveWriter::new(Vec::new(), 0, &[]).unwrap();
        crate::container::pack_bytes(text, &mut aw, 2).unwrap();
        let (bytes, _) = aw.finish().unwrap();
        fs::write(p.dir.join(store::run_file(id)), &bytes).unwrap();
        let f = Json::o(vec![("state", "ready".into()), ("bytes", Json::from(text.len() as u64)), ("archive", Json::from(bytes.len() as u64))]);
        store::update_run(p, id, f).unwrap();
        id
    }

    fn rec(p: &Project, id: u64) -> RunRec {
        store::read_runs(&p.dir).into_iter().find(|r| r.id() == id).unwrap()
    }

    /// The size cap: the config's dna_max read; an index over the share and over 1 MiB is over
    /// it; a run recorded over it is not wanted, and is again when the cap is raised past it.
    #[test]
    fn the_size_cap() {
        let home = tmp_home("cap");
        assert_eq!(max_share(&home), Some(MAX_SHARE));
        for (v, want) in [("dna_max = 50%", Some(50)), ("dna_max = 5", Some(5)), ("dna_max = none", None), ("dna_max = NONE", None), ("dna_max = x", Some(MAX_SHARE)), ("dna_max = 0%", Some(MAX_SHARE))] {
            fs::write(crate::config::path(&home), format!("{v}\n")).unwrap();
            assert_eq!(max_share(&home), want, "{v}");
        }
        // 20% of a 10 MB archive is 2 MB: 3 MB is over it; 1.5 MB is not
        assert!(over_cap(3_000_000, 10_000_000, Some(20)));
        assert!(!over_cap(1_500_000, 10_000_000, Some(20)));
        assert!(!over_cap(3_000_000, 10_000_000, None));
        // under 1 MiB, never: a 314-byte index of a 708-byte archive is kept
        assert!(!over_cap(314, 708, Some(20)));
        assert!(!over_cap(MAX_FLOOR, 1, Some(20)));
        assert!(over_cap(MAX_FLOOR + 1, 1, Some(20)));
        // a run recorded over the cap: not wanted under it, wanted once it is raised
        fs::write(crate::config::path(&home), "").unwrap();
        let root = home.join("proj");
        fs::create_dir_all(&root).unwrap();
        let p = store::ensure_project(&home, &root).unwrap();
        let id = saved_run(&p, &b"2026-11-18 10:00:00 INFO a line of text\n".repeat(2000));
        let on = Settings { on: true, min: 1 };
        assert!(wants(&p, &rec(&p, id), &on));
        let archive = rec(&p, id).i("archive").unwrap() as u64;
        store::update_run(&p, id, Json::o(vec![("dna_would_be", Json::from(archive + 2 * MAX_FLOOR))])).unwrap();
        assert!(over(&p, &rec(&p, id)) && !wants(&p, &rec(&p, id), &on));
        assert_eq!(status(&home).over, 0); // (status reads the config's dna_min: 16 MiB, so this run is too small)
        fs::write(crate::config::path(&home), "dna_max = none\n").unwrap();
        assert!(!over(&p, &rec(&p, id)) && wants(&p, &rec(&p, id), &on));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn builds_records_and_refuses_what_does_not_fit() {
        let home = tmp_home("build");
        let p = store::ensure_project(&home, Path::new("/w/app")).unwrap();
        let text: Vec<u8> = (0..30_000).flat_map(|i| format!("line {i} user{} said refused{}\n", i % 7, i % 13).into_bytes()).collect();
        let id = saved_run(&p, &text);
        let small = saved_run(&p, b"tiny refused\n");
        let s = Settings { on: true, min: 100_000 };
        assert!(wants(&p, &rec(&p, id), &s) && !wants(&p, &rec(&p, small), &s));
        assert!(!wants(&p, &rec(&p, id), &Settings { on: false, min: 0 }), "off: nothing is wanted");
        assert!(!has_one(&p, &rec(&p, id), false) && !has_one(&p, &rec(&p, id), true));
        let b = build_run(&p, id, 2, &mut || true).unwrap();
        let r = rec(&p, id);
        let dna_file = dna::dna_path(&p.dir.join(store::run_file(id)));
        assert_eq!(r.i("dna_bytes"), Some(fs::metadata(&dna_file).unwrap().len() as i64));
        assert_eq!(r.i("dna_build_ms"), Some(b.ms as i64));
        assert!(has_one(&p, &r, false) && has_one(&p, &r, true));
        // no temporary file left
        assert!(!fs::read_dir(&p.dir).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".tmp-")));
        // another run's DNA index, a damaged one, one of another version: not fitting
        let good = fs::read(&dna_file).unwrap();
        let other = saved_run(&p, &text[..text.len() / 2]);
        build_run(&p, other, 1, &mut || true).unwrap();
        fs::copy(dna::dna_path(&p.dir.join(store::run_file(other))), &dna_file).unwrap();
        assert!(!has_one(&p, &r, true));
        let mut bad = good.clone();
        bad[good.len() / 2] ^= 4;
        fs::write(&dna_file, &bad).unwrap();
        assert!(!has_one(&p, &r, true), "damaged");
        assert!(has_one(&p, &r, false), "the quick check reads only the start");
        let mut v2 = good.clone();
        v2[4] = 2;
        fs::write(&dna_file, &v2).unwrap();
        assert!(!has_one(&p, &r, false) && !has_one(&p, &r, true), "another version");
        // a build stopped by its caller writes and records nothing
        fs::remove_file(&dna_file).unwrap();
        let mut asked = 0;
        let e = build_run(&p, id, 1, &mut || {
            asked += 1;
            asked < 2
        })
        .unwrap_err();
        assert_eq!(e, dna::STOPPED);
        assert!(!dna_file.exists());
        assert_eq!(rec(&p, id).s("dna_error"), None);
        // a damaged archive: recorded as dna_error, and passed over by the idle builder
        let broken = saved_run(&p, &text);
        let a = p.dir.join(store::run_file(broken));
        let mut ab = fs::read(&a).unwrap();
        let mid = ab.len() / 2;
        ab[mid] ^= 1;
        fs::write(&a, &ab).unwrap();
        assert!(build_run(&p, broken, 1, &mut || true).is_err());
        assert!(rec(&p, broken).s("dna_error").is_some());
        let s0 = Settings { on: true, min: 0 };
        let next = biggest_missing(&home, &s0).map(|(_, r)| r.id());
        assert!(next.is_some() && next != Some(broken), "{next:?}");
        // the status counts them (with the store's dna_min set low)
        fs::write(home.join("config"), "dna_min = 1KiB\n").unwrap();
        let st = status(&home);
        assert!(st.settings.on && st.with >= 1);
        fs::remove_dir_all(&home).unwrap();
    }

    /// A run forgotten while its DNA index was built gets neither the file nor a record.
    #[test]
    fn a_run_forgotten_meanwhile_gets_nothing() {
        let home = tmp_home("forget");
        let p = store::ensure_project(&home, Path::new("/w/app")).unwrap();
        let text: Vec<u8> = (0..5_000).flat_map(|i| format!("line {i} word{}\n", i % 9).into_bytes()).collect();
        let id = saved_run(&p, &text);
        let before = fs::read(p.dir.join("runs.jsonl")).unwrap();
        // the "forget" happens while the build runs: runs.jsonl rewritten without the run
        let mut first = true;
        let r = build_run(&p, id, 1, &mut || {
            if first {
                first = false;
                let kept: Vec<u8> = Vec::new();
                fs::write(p.dir.join("runs.jsonl"), &kept).unwrap();
            }
            true
        });
        assert_eq!(r.unwrap_err(), GONE);
        assert!(!dna::dna_path(&p.dir.join(store::run_file(id))).exists());
        assert!(fs::read(p.dir.join("runs.jsonl")).unwrap().is_empty(), "no record appended");
        assert!(!before.is_empty());
        assert!(!fs::read_dir(&p.dir).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".tmp-")));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn one_pass_at_a_time() {
        let home = tmp_home("lock");
        let a = lock(&home, false).unwrap();
        assert!(a.is_some());
        assert!(lock(&home, false).unwrap().is_none(), "held");
        drop(a);
        assert!(lock(&home, false).unwrap().is_some());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn sizes_said_as_written() {
        assert_eq!(size_text(16 << 20), "16 MiB");
        assert_eq!(size_text(1536), "1,536 bytes");
        assert_eq!(size_text(2048), "2 KiB");
        assert_eq!(size_text(1000), "1,000 bytes");
        assert_eq!(share(25, 1000), "2.50%");
        assert_eq!(share(1, 0), "-");
    }
}
