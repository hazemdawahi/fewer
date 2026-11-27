//! `fewer pack-pending [--wait]`: pack every finished
//! capture into its run's archive.
//!
//! - One at a time: raw/pack.lock is held for the whole pass (a second pack-pending waits,
//!   then makes its own pass, which finds what the first one left).
//! - A capture whose writer still holds its lock is a run in progress: left alone. A capture
//!   whose writer is gone without recording the run's end (kill -9, a crash, a power cut) is
//!   packed too, with the note "ended without a status" and the capture's last write as the
//!   run's end.
//! - Packing: the fewer-pack binary packs the capture file itself, in place, with half the cores
//!   (`fewer-pack -q -t N -o rNNNNNN.cmf raw/<pid>/rNNNNNN.out`), so the archive is
//!   the same bytes as `fewer-pack -q -o x.cmf` of the capture. Then the .idx (grep::build_index,
//!   the same bytes as `fewer-pack index`), then the catalog (catalog::index_run, with the
//!   capture's arrival marks and stderr lines), then the run is recorded as ready (its bytes,
//!   lines and archive size, and the catalog's fields), and only then are the capture files
//!   deleted.
//! - A pack that fails keeps the capture, records the run as failed with the reason, and is
//!   tried again by the next pack-pending (a full disk passes).
//! - The runs packed with dna_min bytes of text or more (16 MiB) get their DNA index from a
//!   detached pass once the project's captures are packed (dna_build::after_save).
//! - `run` starts `pack-pending --wait` detached when its command ends (`spawn_detached`).
//!   `pack-pending` alone does the same; `--wait` packs in the foreground and prints a line
//!   per run (for tests and scripts).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::capture::{self, err_file, out_file, times_file};
use crate::json::Json;
use crate::store::{self, Project, RunRec};
use crate::{add, catalog, grep, sys, sys_run};

const USAGE: &str = "usage: fewer pack-pending [--wait]";

/// The note of a run whose writer is gone without recording the end.
pub const NO_STATUS: &str = "ended without a status";

/// The start of the note part that says a pack failed (dropped once a later pass packs it).
const NOT_PACKED: &str = "not packed yet: ";

pub fn cmd_pack_pending(args: Vec<OsString>) -> i32 {
    let a: Vec<&str> = args.iter().map(|a| a.to_str().unwrap_or("?")).collect();
    match a.as_slice() {
        [] => match spawn_detached() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("fewer pack-pending: cannot start in the background: {e}");
                1
            }
        },
        ["--wait"] => pack_all(),
        ["-h" | "--help"] => {
            println!("{USAGE}\n\nPack every finished `run` capture of the memory store into its run's archive.\nWithout --wait it starts in the background and returns at once.");
            0
        }
        _ => {
            eprintln!("fewer pack-pending: unknown arguments\n{USAGE}");
            2
        }
    }
}

/// Start `pack-pending --wait` detached: a new session, its stdio /dev/null, not waited for.
pub fn spawn_detached() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    c.arg("pack-pending").arg("--wait").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: new_session makes one async-signal-safe call (setsid)
    unsafe { c.pre_exec(sys_run::new_session) };
    c.spawn().map(|_| ())
}

/// Threads for packing: half the cores.
fn threads() -> usize {
    (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) / 2).max(1)
}

/// One pass over the store; exit status 1 when a run could not be packed.
fn pack_all() -> i32 {
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fewer pack-pending: {e}");
            return 2;
        }
    };
    let raw = home.join("raw");
    let lock = fs::create_dir_all(&raw).and_then(|_| OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(raw.join("pack.lock")));
    let lock: File = match lock {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fewer pack-pending: cannot open {}: {e}", raw.join("pack.lock").display());
            return 1;
        }
    };
    if let Err(e) = lock.lock() {
        eprintln!("fewer pack-pending: cannot lock {}: {e}", raw.join("pack.lock").display());
        return 1;
    }
    let mut ok = true;
    let projects = store::list_projects(&home);
    for p in &projects {
        ok &= pack_project(p, &capture::raw_dir(&home, &p.pid));
    }
    // captures of a project the store does not have (made by hand, or a damaged store)
    if let Ok(rd) = fs::read_dir(&raw) {
        for e in rd.flatten() {
            let pid = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && !projects.iter().any(|p| p.pid == pid) {
                eprintln!("fewer pack-pending: {}: no such project in the store; left as it is", e.path().display());
            }
        }
    }
    drop(lock);
    // the memory's limit, after a save (memory.rs), once the pack lock is let go: it takes it
    if let Some(m) = crate::memory::after_save(&home) {
        println!("{m}");
    }
    (!ok) as i32
}

/// Pack a project's finished captures; false when one could not be packed.
fn pack_project(p: &Project, dir: &Path) -> bool {
    let runs: BTreeMap<u64, RunRec> = store::read_runs(&p.dir).into_iter().map(|r| (r.id(), r)).collect();
    let mut ids: Vec<u64> = match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().filter_map(|e| capture::capture_id(&e.file_name().to_string_lossy())).collect(),
        Err(_) => Vec::new(),
    };
    ids.sort_unstable();
    let mut ok = true;
    if !ids.is_empty() {
        clear_fewer_leftovers(&p.dir);
    }
    // (the runs packed: their DNA index comes from a detached pass)
    let mut packed = Vec::new();
    for &id in &ids {
        let out = dir.join(out_file(id));
        if capture::writer_alive(&out) {
            continue;
        }
        let Some(rec) = runs.get(&id) else {
            eprintln!("fewer pack-pending: {}: no run {id} in {}'s run table; left as it is", out.display(), p.pid);
            continue;
        };
        if rec.state() == "ready" {
            // (packed before; a pass that stopped after recording it left the capture)
            remove_capture(dir, id);
            continue;
        }
        match pack_one(p, dir, id, rec) {
            Ok(line) => {
                println!("{line}");
                packed.push(id);
            }
            Err(e) => {
                eprintln!("fewer pack-pending: {}/r{id}: {e}", p.pid);
                ok = false;
            }
        }
    }
    crate::dna_build::after_save(p, &packed);
    // runs still marked running whose writer is gone before its capture was made (the
    // records were read before this pass: runs that had a capture are not among them)
    for r in runs.values().filter(|r| r.state() == "running" && ids.binary_search(&r.id()).is_err()) {
        let writer = r.i("writer").filter(|&w| w > 0 && w <= u32::MAX as i64);
        if dir.join(out_file(r.id())).exists() || writer.is_none_or(|w| sys::alive(w as u32)) {
            continue;
        }
        let f = Json::o(vec![("state", "failed".into()), ("note", "ended before its output was saved".into())]);
        if let Err(e) = store::update_run(p, r.id(), f) {
            eprintln!("fewer pack-pending: {}/r{}: cannot write the run table: {e}", p.pid, r.id());
            ok = false;
        }
    }
    ok
}

/// The note parts of a run that are its own (not those of a failed pack).
fn own_note(rec: &RunRec) -> Vec<String> {
    // (each part once; a part an earlier version cut from a failed pack's note, "set
    // FEWER_PACK_BIN ...", goes with it)
    let mut v: Vec<String> = Vec::new();
    for s in rec.s("note").unwrap_or("").split("; ").filter(|s| !s.is_empty() && !s.starts_with(NOT_PACKED) && !s.starts_with("set FEWER_PACK_BIN")) {
        if !v.iter().any(|x| x == s) {
            v.push(s.to_string());
        }
    }
    v
}

fn note_json(parts: &[String]) -> Json {
    if parts.is_empty() { Json::Null } else { Json::from(parts.join("; ")) }
}

/// Pack one finished capture; the line to print.
fn pack_one(p: &Project, dir: &Path, id: u64, rec: &RunRec) -> Result<String, String> {
    let out = dir.join(out_file(id));
    let mut note = own_note(rec);
    // 1. a writer gone without the end: the capture's last write is the end
    let end = match rec.i("end") {
        Some(e) => e,
        None => {
            let md = fs::metadata(&out).map_err(|e| format!("cannot read its capture: {e}"))?;
            let end = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or_else(crate::times::now_ms, |d| d.as_millis() as i64);
            if !note.iter().any(|n| n == NO_STATUS) {
                note.push(NO_STATUS.to_string());
            }
            let f = Json::o(vec![("state", "packing".into()), ("end", Json::from(end)), ("note", note_json(&note))]);
            store::update_run(p, id, f).map_err(|e| format!("cannot write the run table: {e}"))?;
            end
        }
    };
    let start = rec.i("start").unwrap_or(end);
    match pack_files(p, dir, id, rec, start, end, &mut note) {
        Ok(line) => Ok(line),
        Err(e) => {
            // the capture stays, for the next pass
            let mut n = note.clone();
            n.push(format!("{NOT_PACKED}{e}"));
            let _ = store::update_run(p, id, Json::o(vec![("state", "failed".into()), ("note", note_json(&n))]));
            Err(e)
        }
    }
}

fn pack_files(p: &Project, dir: &Path, id: u64, rec: &RunRec, start: i64, end: i64, note: &mut Vec<String>) -> Result<String, String> {
    let out = dir.join(out_file(id));
    // 2. its exact size and lines (as grep counts them)
    let (bytes, lines) = count(&out).map_err(|e| format!("cannot read its capture: {e}"))?;
    // 3. packed by fewer-pack, from the capture file itself
    let archive = p.dir.join(store::run_file(id));
    let idx = grep::idx_path(archive.as_os_str());
    for old in [&archive, &idx] {
        // (an earlier pass that stopped half way)
        match fs::remove_file(old) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(format!("cannot replace {}: {e}", old.display())),
            _ => {}
        }
    }
    let t = threads();
    let r = Command::new(add::pack_bin())
        .arg("-q")
        .arg("-t")
        .arg(t.to_string())
        .arg("-o")
        .arg(&archive)
        .arg(&out)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run fewer-pack ({}): {e} (set FEWER_PACK_BIN to the fewer-pack program)", add::pack_bin().display()))?;
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr).trim().to_string();
        let _ = fs::remove_file(&archive);
        return Err(format!("fewer-pack could not pack it: {}", if why.is_empty() { r.status.to_string() } else { why }));
    }
    // 4. its index
    let ix = grep::build_index(&archive, t)?;
    store::write_atomic(&idx, &ix).map_err(|e| format!("cannot write its index: {e}"))?;
    // 5. its catalog, with the capture's arrival marks and stderr lines
    let tags = catalog::Tags {
        name: catalog::source_name(rec),
        file_ms: Some(end),
        zone: None,
        arrivals: Some((start, capture::read_marks(&dir.join(times_file(id))))),
        errs: capture::read_errs(&dir.join(err_file(id))),
        files: Vec::new(),
    };
    let abytes = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    let mut f = Json::o(vec![("state", "ready".into()), ("bytes", Json::from(bytes)), ("lines", Json::from(lines)), ("archive", Json::from(abytes))]);
    match catalog::index_run(p, id, start, &tags) {
        Ok(ixd) => {
            for (k, v) in ixd.fields() {
                f.set(k, v);
            }
        }
        // (the run is searchable without it; the tools build a missing catalog in memory)
        Err(e) => {
            note.push(format!("no catalog: {e}"));
            // its log type all the same: formats::detect on the capture's first 64 KiB
            if let Ok((profile, _)) = catalog::profile_of_capture(&out, &tags.name) {
                f.set("format", profile.format.name().into());
            }
        }
    }
    // (a note left by a failed pass is replaced; a run without a note gets none)
    if !note.is_empty() || rec.s("note").is_some() {
        f.set("note", note_json(note));
    }
    // 6. ready, and only then the capture goes
    store::update_run(p, id, f).map_err(|e| format!("cannot write the run table: {e}"))?;
    remove_capture(dir, id);
    let n = if note.is_empty() { String::new() } else { format!(" ({})", note.join("; ")) };
    Ok(format!("packed {}/r{id}: {lines} lines, {bytes} -> {abytes} bytes{n}", p.pid))
}

/// A file's size and its lines as grep counts them ('\n' bytes, and one more when the last
/// line has none).
fn count(path: &Path) -> io::Result<(u64, u64)> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut bytes, mut nl, mut last) = (0u64, 0u64, b'\n');
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        nl += buf[..n].iter().filter(|&&c| c == b'\n').count() as u64;
        last = buf[n - 1];
        bytes += n as u64;
    }
    Ok((bytes, nl + (bytes > 0 && last != b'\n') as u64))
}

fn remove_capture(dir: &Path, id: u64) {
    for name in [out_file(id), err_file(id), times_file(id)] {
        let _ = fs::remove_file(dir.join(name));
    }
}

/// fewer-pack's own temporary outputs (.fewer-tmp-<process id>-<n>) left in a project's folder by
/// a pack that was killed: removed when that process no longer runs.
fn clear_fewer_leftovers(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix(".fewer-tmp-")
            && let Some((pid, _)) = rest.split_once('-')
            && let Ok(pid) = pid.parse::<u32>()
            && !sys::alive(pid)
        {
            let _ = fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_notes() {
        let d = crate::capture::tests::tmpdir("count");
        for (text, want) in [(&b""[..], (0, 0)), (b"a", (1, 1)), (b"a\n", (2, 1)), (b"a\nb", (3, 2)), (b"\n\n", (2, 2))] {
            fs::write(d.join("x"), text).unwrap();
            assert_eq!(count(&d.join("x")).unwrap(), want);
        }
        let rec = |n: &str| RunRec(Json::o(vec![("id", Json::from(1u64)), ("note", n.into())]));
        assert_eq!(own_note(&rec("saving stopped at 4 GiB; not packed yet: fewer-pack could not pack it: x")), vec!["saving stopped at 4 GiB"]);
        assert_eq!(own_note(&rec("not packed yet: fewer-pack could not pack it: x")), Vec::<String>::new());
        assert_eq!(note_json(&[]), Json::Null);
        // fewer's leftovers of a process that is gone go; others stay
        let dead = d.join(".fewer-tmp-999999999-0");
        let mine = d.join(format!(".fewer-tmp-{}-0", std::process::id()));
        fs::write(&dead, "x").unwrap();
        fs::write(&mine, "x").unwrap();
        clear_fewer_leftovers(&d);
        assert!(!dead.exists() && mine.exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
