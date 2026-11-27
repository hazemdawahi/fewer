//! `fewer memory`: only the person changes the memory, from the command
//! line. The MCP tools only read; they have no way to forget, pause or limit.
//!
//!   fewer memory [--runs] [--project P]      see what is saved
//!   fewer memory forget REF... | --session ID | --project P | --before DATE
//!                              | --sessions | --all  [--yes]
//!   fewer memory pause [FOLDER]  /  memory resume [FOLDER]
//!   fewer memory limit [--size SIZE] [--months N] | --none
//!   fewer memory sessions [on|off] [claude|codex|aider|cline|cursor]
//!   fewer memory dna [on|off|build]
//!   fewer memory secrets
//!
//! - **See**: per project its root, whether saving is paused, its runs by kind, the bytes of
//!   text its archives hold, the bytes its files take on disk (its folder and its captures in
//!   raw/) and the ratio of the two, its oldest and newest run; then the totals, the limit, the
//!   sessions and DNA settings, and what the limit forgot last. `--runs` lists each project's
//!   runs as the `logs` tool does.
//! - **Forget** says what will go and asks "Forget these? [y/N]" (`--yes` skips the question;
//!   without a terminal and without --yes nothing is forgotten). Holding the store's pack and
//!   sessions locks, and each project's lock, it deletes every file of each run (its archive,
//!   .idx, catalog, DNA index, kept file and capture: every file named rNNNNNN.* in the
//!   project's folder and in raw/<pid>/), writes runs.jsonl again without the run, and makes
//!   keys.cat and moments.jsonl again from the runs left. A session piece leaves a record of
//!   state "forgotten" that keeps only where it was read from (source, offset, size, hash,
//!   stamp, start, agent, session), so the sessions sweep does not save it again. Run ids are
//!   never used again. A run still being written, or being saved right now, is not forgotten
//!   (when it was named, nothing is, and the exit status is 1). A run is forgotten whole: a line
//!   reference names its whole run. It does not promise to wipe the disk.
//! - **Pause and resume**: without a folder, the project of the current folder (its `paused`
//!   file); with one, every run started under that folder (a `pause = /abs/folder` config line).
//!   While paused, `run` runs the command and saves nothing. `add` saves what it is given, and
//!   sessions have their own switch, `memory sessions off`.
//! - **Limit**: none by default, and nothing is ever deleted on its own (`memory` warns above
//!   20 GB). With a limit, after each save (add, pack-pending, the sessions sweep) the oldest
//!   saved runs past it are forgotten as `forget` does, never the newest run of the store, and
//!   $FEWER_HOME/forgotten.log gets a line saying what went; `memory` shows its last lines.
//!   `--size` counts the bytes the projects' files take on disk; `--months` is 30 days a month,
//!   by each run's start.
//! - **Sessions**: whether Claude's and other agents' sessions are saved (the config's
//!   `sessions` and `sessions.<agent>`).
//! - **DNA**: the DNA index (on by default).
//! - **Secrets**: the secret shapes hidden before saving (SECRETS.md), and the config's own.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use crate::portable::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::answer::{group, run_ref};
use crate::dna_build::{self, share, size_text};
use crate::json::{self, Json};
use crate::store::{self, Project, RunRec};
use crate::times::{fmt_local, now_ms};

const USAGE: &str = "\
usage: fewer memory [--runs] [--project P] [--total]
       fewer memory clear [--yes]
       fewer memory forget REF... | --session ID | --project P | --before DATE | --sessions | --all  [--yes]
       fewer memory pause [FOLDER]  |  memory resume [FOLDER]
       fewer memory limit [--size SIZE] [--months N] | --none
       fewer memory sessions [on|off] [claude|codex|aider|cline|cursor]
       fewer memory dna [on|off|build]
       fewer memory secrets

  (no command)  what is saved: per project its runs, the bytes of text, the bytes on disk and
                their ratio, the oldest and newest run; then the totals and the settings
    --runs        also list each project's runs, as Claude's `logs` tool does
    --project P   one project: its id (as in references: fewer-3fa2) or a folder in it
  clear         delete everything saved (command output, chats, web pages, helpers' records),
                the settings kept; it says what will go and asks first, --yes: no question
  forget        forget runs: REF is a run (fewer-3fa2/r42, or r42 in this folder's project; a
                line reference such as fewer-3fa2/r42:1201 names its whole run); --session ID
                a session's pieces; --project P a project's runs; --before DATE the runs that
                started before it (2026-11-01, \"2026-11-01 10:00\", 30d); --sessions every
                session piece; --all everything. --project also narrows the others. It says
                what will go and asks first (--yes: no question). The files are deleted; that
                does not wipe the disk
  pause         stop saving command output (`run`): for this folder's project, or under FOLDER
  resume        start saving again
  limit         keep the projects' files within SIZE on disk (20GB, 500MB, 1.5TB) and/or N
                months (30 days each) by forgetting the oldest runs after each save; --none:
                no limit (the default: nothing is ever deleted on its own); alone, the limit now
  sessions      save Claude's and other agents' sessions: on or off, for all or one agent
  dna           the DNA index: on (the default) or off, or build the missing ones now
  secrets       the secret shapes hidden before saving";

const DNA_USAGE: &str = "\
usage: fewer memory dna [on|off|build]
  memory dna          whether the DNA index is on, and what it holds
  memory dna on       turn it on (the default): runs with dna_min bytes of text or more
                      (16 MiB unless the config says otherwise) get one after saving and
                      while the computer is idle, at background priority
  memory dna off      turn it off: nothing is built, and search uses no DNA index
  memory dna build    build the missing ones now, in the foreground
The DNA index lets search leave unopened the parts of a big log that cannot hold a piece of a
word; answers are the same with it or without it.";

/// Above this, `memory` suggests a limit when none is set.
const WARN_BYTES: u64 = 20_000_000_000;
/// `add` keeps no lock while it packs: a run of its kinds marked packing for less than this is
/// being saved now; older, it is what a stopped save left.
const PACKING_FRESH_MS: i64 = 3_600_000;
/// Where the limit says what it forgot, in the store's folder.
const FORGOTTEN_LOG: &str = "forgotten.log";
/// A month, for `limit --months`.
const MONTH_MS: i64 = 30 * 86_400_000;
/// The agents whose sessions can be saved (sessions.rs).
const AGENTS: [&str; 5] = ["claude", "codex", "aider", "cline", "cursor"];
/// The fields a forgotten session piece keeps: where it was read from, so the sweep does not
/// save it again (sessions.rs `saved`, `chain_end` and the stamps).
const KEPT_FIELDS: [&str; 9] = ["source", "offset", "size", "hash", "stamp", "start", "agent", "session", "reader"];

pub fn cmd_memory(args: Vec<OsString>) -> i32 {
    let a: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let home = || store::home().map_err(|e| eprintln!("fewer memory: {e}"));
    match a.first().copied() {
        Some("-h" | "--help") => {
            println!("{USAGE}");
            0
        }
        None | Some("--runs" | "--project" | "--total") => home().map_or(2, |h| see(&h, &a)),
        Some("forget") => home().map_or(2, |h| forget(&h, &a[1..])),
        // everything saved deleted, the settings kept (it says what goes and asks first)
        Some("clear") if a[1..] == ["-h"] || a[1..] == ["--help"] => {
            println!("usage: fewer memory clear [--yes]\n\nDelete everything fewer saved (command output, Claude Code's chats, web pages, helpers' records); the settings stay. It says what will go and asks first; --yes: no question.");
            0
        }
        Some("clear") => home().map_or(2, |h| {
            if let Some(x) = a[1..].iter().find(|x| **x != "--yes") {
                eprintln!("fewer memory clear: unknown argument '{x}' (fewer memory clear [--yes])");
                return 2;
            }
            clear(&h, a[1..].contains(&"--yes"))
        }),
        Some("pause") => home().map_or(2, |h| pause(&h, &a[1..], true)),
        Some("resume") => home().map_or(2, |h| pause(&h, &a[1..], false)),
        Some("limit") => home().map_or(2, |h| limit(&h, &a[1..])),
        Some("sessions") => home().map_or(2, |h| sessions(&h, &a[1..])),
        Some("secrets") if a.len() == 1 => secrets(),
        Some("dna") => match &a[1..] {
            ["-h" | "--help"] => {
                println!("{DNA_USAGE}");
                0
            }
            [] => home().map_or(2, |h| dna_status(&h)),
            ["on"] => home().map_or(2, |h| dna_set(&h, true)),
            ["off"] => home().map_or(2, |h| dna_set(&h, false)),
            ["build"] => home().map_or(2, |h| dna_build::build_all(&h, &mut io::stdout())),
            _ => {
                eprintln!("fewer memory dna: unknown arguments\n{DNA_USAGE}");
                2
            }
        },
        Some(x) => {
            eprintln!("fewer memory: unknown command or arguments '{x}'\n{USAGE}");
            2
        }
    }
}

// ---------------------------------------------------------------- the store, read

/// A project and its runs (merged by id, sorted; forgotten session pieces included).
struct Scan {
    p: Project,
    runs: Vec<RunRec>,
}

fn scan(home: &Path) -> Vec<Scan> {
    store::list_projects(home)
        .into_iter()
        .map(|p| {
            let runs = store::read_runs_all(&p.dir);
            Scan { p, runs }
        })
        .collect()
}

/// A session piece that was forgotten: a record of where it was read from, with no text.
fn is_forgotten(r: &RunRec) -> bool {
    r.state() == "forgotten"
}

/// The files of run `id`: every regular file named rNNNNNN.* in the project's folder and in
/// raw/<pid>/ (its archive, .idx, catalog, DNA index, kept file, capture and leftovers).
fn run_files(home: &Path, p: &Project, id: u64) -> Vec<PathBuf> {
    let prefix = format!("r{id:06}.");
    let mut v = Vec::new();
    for dir in [p.dir.clone(), crate::capture::raw_dir(home, &p.pid)] {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) && e.file_type().is_ok_and(|t| t.is_file()) {
                    v.push(e.path());
                }
            }
        }
    }
    v.sort();
    v
}

fn len_of(p: &Path) -> u64 {
    fs::symlink_metadata(p).ok().filter(|m| m.is_file()).map_or(0, |m| m.len())
}

/// The bytes the regular files of a folder take, its sub-folders included (links are not
/// followed).
fn folder_bytes(dir: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                n += folder_bytes(&e.path());
            } else if t.is_file() {
                n += len_of(&e.path());
            }
        }
    }
    n
}

/// The bytes a project takes on disk: its folder and its captures (raw/<pid>/).
fn project_disk(home: &Path, p: &Project) -> u64 {
    folder_bytes(&p.dir) + folder_bytes(&crate::capture::raw_dir(home, &p.pid))
}

/// What a project's runs hold, for `memory`.
#[derive(Default)]
struct Counts {
    runs: u64,
    by_kind: BTreeMap<String, u64>,
    live: u64,
    failed: u64,
    text: u64,
    oldest: Option<i64>,
    newest: Option<i64>,
    forgotten: u64,
}

fn count(runs: &[RunRec]) -> Counts {
    let mut c = Counts::default();
    for r in runs {
        if is_forgotten(r) {
            c.forgotten += 1;
            continue;
        }
        c.runs += 1;
        *c.by_kind.entry(r.kind().to_string()).or_default() += 1;
        match r.state() {
            "running" | "packing" => c.live += 1,
            "failed" => c.failed += 1,
            _ => {}
        }
        // the text an archive holds: a ready run with an archive (a kept file holds bytes as
        // they are, not text)
        if r.state() == "ready" && r.i("archive").is_some() {
            c.text += r.i("bytes").unwrap_or(0).max(0) as u64;
        }
        if let Some(s) = r.i("start") {
            c.oldest = Some(c.oldest.map_or(s, |o| o.min(s)));
            c.newest = Some(c.newest.map_or(s, |n| n.max(s)));
        }
    }
    c
}

/// "5 command runs, 4 files, 2 session pieces": the kinds in a fixed order, then any other.
fn kinds_text(c: &Counts) -> String {
    const NAMES: [(&str, &str, &str); 6] = [
        ("run", "command run", "command runs"),
        ("add", "file", "files"),
        ("session", "session piece", "session pieces"),
        ("export", "export", "exports"),
        ("kept", "file kept as it is", "files kept as they are"),
        ("not-read", "file not read", "files not read"),
    ];
    let mut parts = Vec::new();
    for (k, one, many) in NAMES {
        if let Some(&n) = c.by_kind.get(k) {
            parts.push(format!("{} {}", group(n), if n == 1 { one } else { many }));
        }
    }
    for (k, &n) in &c.by_kind {
        if !NAMES.iter().any(|x| x.0 == k) {
            parts.push(format!("{} {k}", group(n)));
        }
    }
    let mut t = parts.join(", ");
    let mut more = Vec::new();
    if c.live > 0 {
        more.push(format!("{} in progress", group(c.live)));
    }
    if c.failed > 0 {
        more.push(format!("{} failed", group(c.failed)));
    }
    if !more.is_empty() {
        t.push_str("; ");
        t.push_str(&more.join(", "));
    }
    t
}

/// " (52.3x)": text bytes for each byte on disk; nothing when either is 0.
fn ratio(text: u64, disk: u64) -> String {
    if text == 0 || disk == 0 { String::new() } else { format!(" ({:.1}x)", text as f64 / disk as f64) }
}

fn runs_word(n: u64) -> String {
    if n == 1 { "1 run".into() } else { format!("{} runs", group(n)) }
}

/// The project a `--project` value names: its id, else a folder (the project whose root it
/// is, else the project of the folder's git root).
fn find_project<'a>(home: &Path, scans: &'a [Scan], p: &str) -> Result<&'a Scan, String> {
    if let Some(s) = scans.iter().find(|s| s.p.pid == p) {
        return Ok(s);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let folder = std::path::absolute(cwd.join(p)).unwrap_or_else(|_| cwd.join(p));
    let folder = PathBuf::from(folder.to_string_lossy().trim_end_matches('/'));
    // the folder itself, else its git root (not FEWER_PROJECT: the folder given is the question)
    let args = [std::ffi::OsStr::new("-C"), folder.as_os_str(), std::ffi::OsStr::new("rev-parse"), std::ffi::OsStr::new("--show-toplevel")];
    let git_root = store::run_quiet("git", &args, std::time::Duration::from_secs(2))
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim_end_matches(['\n', '\r'])))
        .filter(|r| r.is_absolute());
    for root in std::iter::once(folder.clone()).chain(git_root) {
        let (pid, exists) = store::pid_for(home, &root);
        if exists && let Some(s) = scans.iter().find(|s| s.p.pid == pid) {
            return Ok(s);
        }
    }
    // (a short line: a store may hold hundreds of projects; `fewer memory` lists them)
    Err(format!("no project '{p}' in the memory store (fewer memory lists the folders that have something saved)"))
}

/// The project of the current folder: (pid, root).
fn here(home: &Path) -> (String, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let root = store::project_root(&cwd);
    (store::pid_for(home, &root).0, root)
}

// ---------------------------------------------------------------- see

fn see(home: &Path, args: &[&str]) -> i32 {
    // (--total: the totals and the settings only, not each project: /fewer-memory)
    let (mut list, mut total) = (false, false);
    let mut project = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--runs" => list = true,
            "--total" => total = true,
            "--project" if i + 1 < args.len() => {
                i += 1;
                project = Some(args[i]);
            }
            x => {
                eprintln!("fewer memory: unknown or incomplete argument '{x}'\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    let scans = scan(home);
    // (the overview, and in a terminal its menu: everything, or the totals alone (--total);
    // --project and --runs: one project's or every project's details, below)
    if project.is_none() && !list {
        return overview(home, &scans, total);
    }
    let chosen: Vec<&Scan> = match project {
        None => scans.iter().collect(),
        Some(p) => match find_project(home, &scans, p) {
            Ok(s) => vec![s],
            Err(e) => {
                eprintln!("fewer memory: {e}");
                return 1;
            }
        },
    };
    println!("The memory store {}:", home.display());
    if scans.is_empty() {
        println!("Nothing is saved yet.");
    }
    let (mut runs, mut text, mut disk) = (0u64, 0u64, 0u64);
    for s in &chosen {
        let c = count(&s.runs);
        let d = project_disk(home, &s.p);
        if total {
            runs += c.runs;
            text += c.text;
            disk += d;
            continue;
        }
        let paused = s.p.dir.join("paused").exists();
        println!();
        println!("{}: {}{}", s.p.pid, s.p.root, if paused { " (saving paused)" } else { "" });
        if c.runs == 0 {
            println!("  runs: 0");
        } else {
            println!("  runs: {} ({})", group(c.runs), kinds_text(&c));
        }
        println!("  text: {} bytes; on disk: {} bytes{}", group(c.text), group(d), ratio(c.text, d));
        if let (Some(a), Some(b)) = (c.oldest, c.newest) {
            println!("  from {} to {}", fmt_local(a), fmt_local(b));
        }
        if c.forgotten > 0 {
            println!("  {} forgotten session {} kept only as a record of where {} read from", group(c.forgotten), if c.forgotten == 1 { "piece is" } else { "pieces are" }, if c.forgotten == 1 { "it was" } else { "they were" });
        }
        if list {
            let ctx = crate::tools::Ctx { home: Ok(home.to_path_buf()), root: here(home).1 };
            let args = Json::o(vec![("project", s.p.pid.as_str().into()), ("kind", "all".into()), ("limit", Json::from(500u64)), ("max_tokens", Json::from(20_000u64))]);
            match crate::tools::call(&ctx, "logs", &args, &AtomicBool::new(false)) {
                Ok((t, _)) => {
                    for l in t.lines() {
                        println!("    {l}");
                    }
                }
                Err(e) => println!("    (cannot list the runs: {e})"),
            }
        }
        runs += c.runs;
        text += c.text;
        disk += d;
    }
    let config = crate::config::read(home);
    if project.is_none() && !scans.is_empty() {
        println!();
        println!("In all: {} {}, {}; text: {} bytes; on disk: {} bytes{}", group(scans.len() as u64), if scans.len() == 1 { "project" } else { "projects" }, runs_word(runs), group(text), group(disk), ratio(text, disk));
    }
    println!();
    for (k, v) in &config {
        if k == "pause" {
            println!("Saving paused under {v} (config).");
        }
    }
    let (size, months) = limits(&config);
    if size.is_none() && months.is_none() {
        println!("Limit: none: nothing is ever deleted on its own.");
        if project.is_none() && disk > WARN_BYTES {
            println!("The projects take over 20 GB: `fewer memory limit --size 20GB` keeps them within it by forgetting the oldest runs after each save.");
        }
    } else {
        println!("Limit: {}: after each save, the oldest runs past it are forgotten.", limit_text(size, months));
    }
    println!("{}", sessions_text(&config));
    // (the DNA index's settings: `fewer memory dna`)
    if !scans.is_empty() {
        println!("To delete what is saved: fewer memory clear (it asks first). More: fewer memory --help");
    }
    let log = fs::read_to_string(home.join(FORGOTTEN_LOG)).unwrap_or_default();
    let last: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    if !last.is_empty() {
        println!("Forgotten by the limit, the last time{}:", if last.len() > 3 { "s (the last 3)" } else { "s" });
        for l in &last[last.len().saturating_sub(3)..] {
            println!("  {l}");
        }
    }
    0
}

/// How many folders the overview lists by name (the biggest first); the rest are counted.
const LISTED: usize = 20;

/// The overview, for people: each saved folder by its size on disk (the biggest first, numbered),
/// then the totals, the limit and the chats; `total`: the totals alone (`/fewer-memory`, which
/// Claude Code runs without a terminal). In a terminal, a menu: a folder's number deletes what is
/// saved for it, "all" everything (each asks first, as forget and clear do).
fn overview(home: &Path, scans: &[Scan], total: bool) -> i32 {
    use std::io::IsTerminal;
    println!("fewer's memory, in {}:", crate::perms::tilde(home));
    if scans.is_empty() {
        println!("  nothing is saved yet");
        return 0;
    }
    let mut rows: Vec<(&Scan, u64)> = scans.iter().map(|s| (s, project_disk(home, &s.p))).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.p.root.cmp(&b.0.p.root)));
    let (mut runs, mut text, mut disk) = (0u64, 0u64, 0u64);
    for (s, d) in &rows {
        let c = count(&s.runs);
        runs += c.runs;
        text += c.text;
        disk += d;
    }
    // (a folder whose saved runs were all forgotten keeps only a small record: not listed or
    // counted as a folder, its few bytes still in the total on disk)
    rows.retain(|(s, _)| count(&s.runs).runs > 0);
    if !total {
        let w = rows.iter().take(LISTED).map(|(s, _)| crate::perms::tilde(Path::new(&s.p.root)).chars().count()).max().unwrap_or(0);
        for (i, (s, d)) in rows.iter().take(LISTED).enumerate() {
            let paused = if s.p.dir.join("paused").exists() { "  (saving paused)" } else { "" };
            println!("  {:>2}. {:w$}  {:>8}{paused}", i + 1, crate::perms::tilde(Path::new(&s.p.root)), size_words(*d));
        }
        if rows.len() > LISTED {
            println!("      and {} more folders, {} in all", group((rows.len() - LISTED) as u64), size_words(rows.iter().skip(LISTED).map(|r| r.1).sum()));
        }
        println!();
    }
    let folders = if rows.len() == 1 { "1 folder".to_string() } else { format!("{} folders", group(rows.len() as u64)) };
    println!("  {} outputs, files and chats, from {folders}", group(runs));
    let smaller = if text > disk && text > 0 { format!(" ({}% smaller)", (100.0 * (1.0 - disk as f64 / text as f64)).round() as u64) } else { String::new() };
    println!("  {}, packed into {} on disk{smaller}", size_words(text), size_words(disk));
    let config = crate::config::read(home);
    let (size, months) = limits(&config);
    if size.is_none() && months.is_none() {
        println!("  no size limit: nothing is deleted on its own");
    } else {
        println!("  limit {}: after each save, the oldest past it are deleted", limit_text(size, months));
    }
    for (k, v) in &config {
        if k == "pause" {
            println!("  saving paused under {}", crate::perms::tilde(Path::new(v)));
        }
    }
    let mut notes = Vec::new();
    if crate::sessions::agents_on(&config, &mut notes).is_empty() {
        println!("  your Claude Code chats are not saved (fewer memory sessions on saves them)");
    }
    let interactive = !total && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        println!("\nTo see each folder and delete what you want: fewer memory, in a terminal. More: fewer memory --help");
        return 0;
    }
    print!("\nDelete something? Type a number, \"all\", or press Enter to leave: ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return 1;
    }
    let pick = line.trim();
    if pick.is_empty() {
        return 0;
    }
    if pick.eq_ignore_ascii_case("all") {
        return clear(home, false);
    }
    match pick.parse::<usize>() {
        Ok(n) if (1..=rows.len().min(LISTED)).contains(&n) => {
            let pid = rows[n - 1].0.p.pid.clone();
            forget(home, &["--project", &pid])
        }
        _ => {
            println!("Nothing deleted: type a number from 1 to {}, or \"all\".", rows.len().min(LISTED));
            1
        }
    }
}

/// A size as people read it: 830 bytes, 12 KB, 4.2 MB, 2.1 GB.
fn size_words(n: u64) -> String {
    match n {
        0..=999 => format!("{n} bytes"),
        1_000..=999_999 => format!("{} KB", (n as f64 / 1e3).round() as u64),
        1_000_000..=999_999_999 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.1} GB", n as f64 / 1e9),
    }
}

// ---------------------------------------------------------------- forget

/// What `forget` was asked for.
#[derive(Default, Debug)]
struct Ask {
    refs: Vec<String>,
    session: Option<String>,
    project: Option<String>,
    before: Option<String>,
    sessions: bool,
    all: bool,
    yes: bool,
}

fn parse_forget(args: &[&str]) -> Result<Ask, String> {
    let mut a = Ask::default();
    let mut i = 0;
    let value = |i: usize, flag: &str| args.get(i + 1).map(|s| s.to_string()).ok_or_else(|| format!("{flag} needs a value"));
    while i < args.len() {
        match args[i] {
            "--yes" | "-y" => a.yes = true,
            "--all" => a.all = true,
            "--sessions" => a.sessions = true,
            "--session" => {
                a.session = Some(value(i, "--session")?);
                i += 1;
            }
            "--project" => {
                a.project = Some(value(i, "--project")?);
                i += 1;
            }
            "--before" => {
                a.before = Some(value(i, "--before")?);
                i += 1;
            }
            x if x.starts_with('-') => return Err(format!("unknown argument '{x}'")),
            x => a.refs.push(x.to_string()),
        }
        i += 1;
    }
    let kinds = [!a.refs.is_empty(), a.session.is_some(), a.before.is_some(), a.sessions, a.all].iter().filter(|&&x| x).count();
    if kinds > 1 {
        return Err("give one of REF..., --session, --before, --sessions or --all (--project may narrow --session, --before and --sessions)".into());
    }
    if kinds == 0 && a.project.is_none() {
        return Err("say what to forget: REF..., --session ID, --project P, --before DATE, --sessions or --all".into());
    }
    if a.all && a.project.is_some() {
        return Err("--all is every project; for one, give --project alone".into());
    }
    if !a.refs.is_empty() && a.project.is_some() {
        return Err("a reference names its project (fewer-3fa2/r42); --project does not go with references".into());
    }
    Ok(a)
}

/// A run reference: (project id or None, run id, whether it named lines).
fn parse_ref(s: &str) -> Option<(Option<String>, u64, bool)> {
    let s = s.trim();
    let (run, lines) = match s.rsplit_once(':') {
        Some((r, l)) => (r, Some(l)),
        None => (s, None),
    };
    if let Some(l) = lines {
        // pid/r42:1201 or pid/r42:1201-1210
        let ok = l.split_once('-').map_or_else(|| l.parse::<u64>().is_ok(), |(a, b)| a.parse::<u64>().is_ok() && b.parse::<u64>().is_ok());
        if !ok {
            return None;
        }
    }
    let (pid, r) = match run.rsplit_once('/') {
        Some((p, r)) if !p.is_empty() => (Some(p.to_string()), r),
        Some(_) => return None,
        None => (None, run),
    };
    let id = r.strip_prefix('r')?.parse::<u64>().ok().filter(|&i| i > 0)?;
    Some((pid, id, lines.is_some()))
}

/// Why a run cannot be forgotten now, if it cannot. A `run` capture or a session piece being
/// packed is not: forget_runs waits for the store's pack and sessions locks, and looks again.
fn busy(home: &Path, p: &Project, r: &RunRec, now: i64) -> Option<&'static str> {
    match r.state() {
        "running" if crate::capture::writer_alive(&crate::capture::raw_dir(home, &p.pid).join(crate::capture::out_file(r.id()))) => Some("it is still being written"),
        "packing" if !matches!(r.kind(), "run" | "session") && now - r.i("start").unwrap_or(0) < PACKING_FRESH_MS => Some("it is being saved right now"),
        _ => None,
    }
}

/// What a forget did.
#[derive(Default)]
struct Done {
    runs: u64,
    text: u64,
    disk: u64,
    kept: u64,
    refs: Vec<String>,
    skipped: Vec<String>,
}

/// The store's locks a forget holds, so that no `run` capture is packed and no session piece
/// is saved while it works (pack-pending and the sweep take them the same way). Each project's
/// lock is taken after them, as those writers do.
struct StoreLocks(#[allow(dead_code)] Vec<File>);

fn store_locks(home: &Path) -> io::Result<StoreLocks> {
    let raw = home.join("raw");
    fs::create_dir_all(&raw)?;
    let mut v = Vec::new();
    for name in ["pack.lock", "sessions.lock"] {
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(raw.join(name))?;
        f.lock()?;
        v.push(f);
    }
    Ok(StoreLocks(v))
}

/// Forget runs: per project, the ids. The caller has asked the person, or the limit decided.
fn forget_runs(home: &Path, plan: &[(Project, Vec<u64>)]) -> io::Result<Done> {
    let _locks = store_locks(home)?;
    let now = now_ms();
    let mut done = Done::default();
    for (p, ids) in plan {
        let _l = store::Lock::take(&p.dir)?;
        let runs: BTreeMap<u64, RunRec> = store::read_runs_all(&p.dir).into_iter().map(|r| (r.id(), r)).collect();
        let mut gone = BTreeSet::new();
        let mut marks = Vec::new();
        for &id in ids {
            let Some(r) = runs.get(&id) else { continue };
            if is_forgotten(r) {
                continue;
            }
            if let Some(why) = busy(home, p, r, now) {
                done.skipped.push(format!("{}: {why}", run_ref(&p.pid, id)));
                continue;
            }
            for f in run_files(home, p, id) {
                done.disk += len_of(&f);
                fs::remove_file(&f)?;
            }
            if r.state() == "ready" && r.i("archive").is_some() {
                done.text += r.i("bytes").unwrap_or(0).max(0) as u64;
            }
            if r.kind() == "session" {
                marks.push(forgotten_mark(r, now));
                done.kept += 1;
            }
            gone.insert(id);
            done.runs += 1;
            done.refs.push(run_ref(&p.pid, id));
        }
        if gone.is_empty() {
            continue;
        }
        rewrite_runs(&p.dir, &gone, &marks)?;
        crate::guard::rebuild_locked(&p.dir)?;
    }
    Ok(done)
}

/// The record a forgotten session piece leaves: where it was read from, and when it was
/// forgotten; no text, no counts.
fn forgotten_mark(r: &RunRec, now: i64) -> Json {
    let mut m = Json::o(vec![("id", Json::from(r.id())), ("kind", "session".into()), ("state", "forgotten".into())]);
    for k in KEPT_FIELDS {
        if let Some(v) = r.0.get(k) {
            m.set(k, v.clone());
        }
    }
    m.set("forgotten", Json::from(now));
    m
}

/// runs.jsonl again (the project lock is held): every line of a run in `gone` left out, every
/// other line kept byte for byte, and the forgotten pieces' records at the end. A last line cut
/// short by a crash, which readers ignore, is left out.
fn rewrite_runs(dir: &Path, gone: &BTreeSet<u64>, marks: &[Json]) -> io::Result<()> {
    let b = match fs::read(dir.join("runs.jsonl")) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let complete = match b.iter().rposition(|&c| c == b'\n') {
        Some(p) => &b[..p + 1],
        None => &b[..0],
    };
    let mut out = Vec::with_capacity(complete.len());
    for line in complete.split_inclusive(|&c| c == b'\n') {
        let body = &line[..line.len() - 1];
        let id = json::parse(body).ok().and_then(|v| v.get("id").and_then(Json::int));
        if id.is_some_and(|i| i > 0 && gone.contains(&(i as u64))) {
            continue;
        }
        out.extend_from_slice(line);
    }
    for m in marks {
        out.extend_from_slice(m.to_string().as_bytes());
        out.push(b'\n');
    }
    store::write_atomic(&dir.join("runs.jsonl"), &out)
}

fn forget(home: &Path, args: &[&str]) -> i32 {
    let ask = match parse_forget(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fewer memory forget: {e}\n{USAGE}");
            return 2;
        }
    };
    let now = now_ms();
    let before = match ask.before.as_deref().map(|d| crate::times::parse_when(d, now)) {
        None => None,
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("fewer memory forget: --before: {e}");
            return 2;
        }
    };
    let scans = scan(home);
    let scope: Vec<&Scan> = match &ask.project {
        None => scans.iter().collect(),
        Some(p) => match find_project(home, &scans, p) {
            Ok(s) => vec![s],
            Err(e) => {
                eprintln!("fewer memory forget: {e}");
                return 1;
            }
        },
    };
    // 1. the runs asked for
    let mut chosen: BTreeMap<usize, Vec<u64>> = BTreeMap::new(); // (index in scans) -> ids
    let index = |pid: &str| scans.iter().position(|s| s.p.pid == pid);
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    if !ask.refs.is_empty() {
        let (here_pid, _) = here(home);
        for r in &ask.refs {
            let Some((pid, id, lines)) = parse_ref(r) else {
                problems.push(format!("cannot read the reference '{r}': give pid/r42, r42, or a line reference such as pid/r42:1201"));
                continue;
            };
            let pid = pid.unwrap_or_else(|| here_pid.clone());
            let Some(k) = index(&pid) else {
                problems.push(format!("{r}: no project '{pid}' in the memory store"));
                continue;
            };
            match scans[k].runs.iter().find(|x| x.id() == id) {
                None => problems.push(format!("{r}: no run r{id} in {pid}{}", if next_id(&scans[k].p) > id { " (it was forgotten before)" } else { "" })),
                Some(x) if is_forgotten(x) => notes.push(format!("{}: forgotten before", run_ref(&pid, id))),
                Some(_) => {
                    if lines {
                        notes.push(format!("{r}: forgetting part of a run comes in a later version; the whole run {} is forgotten", run_ref(&pid, id)));
                    }
                    chosen.entry(k).or_default().push(id);
                }
            }
        }
    } else {
        for s in &scope {
            let k = index(&s.p.pid).unwrap_or(0);
            for r in &s.runs {
                if is_forgotten(r) {
                    continue;
                }
                let take = if let Some(id) = &ask.session {
                    r.kind() == "session" && r.s("session") == Some(id.as_str())
                } else if let Some(t) = before {
                    r.i("start").is_some_and(|s| s < t)
                } else if ask.sessions {
                    r.kind() == "session"
                } else {
                    true // --all, or --project alone
                };
                if take {
                    chosen.entry(k).or_default().push(r.id());
                }
            }
        }
    }
    // 2. runs that cannot be forgotten now: named ones stop everything; others are left
    let mut busy_named = Vec::new();
    for (&k, ids) in chosen.iter_mut() {
        let s = &scans[k];
        ids.sort_unstable();
        ids.dedup();
        ids.retain(|&id| {
            let r = s.runs.iter().find(|x| x.id() == id).expect("chosen from these runs");
            match busy(home, &s.p, r, now) {
                Some(why) => {
                    let line = format!("{}: {why}; forget it once it is saved", run_ref(&s.p.pid, id));
                    if ask.refs.is_empty() && ask.session.is_none() { notes.push(line) } else { busy_named.push(line) }
                    false
                }
                None => true,
            }
        });
    }
    chosen.retain(|_, ids| !ids.is_empty());
    for n in &notes {
        println!("{n}");
    }
    if !problems.is_empty() || !busy_named.is_empty() {
        for p in problems.iter().chain(&busy_named) {
            eprintln!("fewer memory forget: {p}");
        }
        eprintln!("fewer memory forget: nothing was forgotten");
        return 1;
    }
    if chosen.is_empty() {
        println!("Nothing to forget.");
        return 0;
    }
    // 3. what will go
    let (mut n, mut text, mut disk, mut pieces) = (0u64, 0u64, 0u64, 0u64);
    let mut lines = Vec::new();
    for (&k, ids) in &chosen {
        let s = &scans[k];
        for &id in ids {
            let r = s.runs.iter().find(|x| x.id() == id).expect("chosen from these runs");
            n += 1;
            disk += run_files(home, &s.p, id).iter().map(|f| len_of(f)).sum::<u64>();
            if r.state() == "ready" && r.i("archive").is_some() {
                text += r.i("bytes").unwrap_or(0).max(0) as u64;
            }
            pieces += u64::from(r.kind() == "session");
        }
        lines.push(format!("  {}: {}", s.p.pid, short_ids(ids)));
    }
    println!("To forget: {}, {} bytes of text, {} bytes on disk:", runs_word(n), group(text), group(disk));
    for l in &lines {
        println!("{l}");
    }
    if pieces > 0 {
        println!("Each session piece keeps a record of where it was read from (no text), so it is not saved again.");
    }
    if !ask.yes {
        if !io::stdin().is_terminal() {
            println!("Not forgotten: this asks first, and there is no terminal to answer. Run it again with --yes to forget them.");
            return 1;
        }
        print!("Forget these? [y/N] ");
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was forgotten.");
            return 0;
        }
    }
    // 4. forget them
    let plan: Vec<(Project, Vec<u64>)> = chosen.iter().map(|(&k, ids)| (scans[k].p.clone(), ids.clone())).collect();
    match forget_runs(home, &plan) {
        Ok(d) => {
            println!("Forgot {}: {} bytes of text, {} bytes on disk freed. Run ids are not used again. The files are deleted; that does not wipe the disk.", runs_word(d.runs), group(d.text), group(d.disk));
            for s in &d.skipped {
                println!("Not forgotten: {s}; forget it once it is saved.");
            }
            i32::from(!d.skipped.is_empty() && (!ask.refs.is_empty() || ask.session.is_some()))
        }
        Err(e) => {
            eprintln!("fewer memory forget: {e}; what was forgotten before this is gone, the rest is kept");
            1
        }
    }
}

/// What `memory clear` deletes besides the runs: (its name in the store, what it holds).
const CLEAR_ALSO: [(&str, &str); 5] = [
    ("addons", "web pages and long answers (each kept 7 days)"),
    ("helpers.jsonl", "helpers' and workflows' records: when, tokens, folder"),
    ("sessions-where", "the folder each Claude Code session started in"),
    ("web", "web indexes downloaded (DevDocs')"),
    (FORGOTTEN_LOG, "the list of what was forgotten"),
];

/// `memory clear [--yes]`: everything fewer saved deleted, its settings (config) kept: every
/// run (command output and chats) as `forget --all` forgets them, and what CLEAR_ALSO names.
/// Says what goes and asks first.
/// Everything saved deleted, no question (`fewer uninstall`, after its own question).
pub fn clear_all(home: &Path) -> i32 {
    clear(home, true)
}

fn clear(home: &Path, yes: bool) -> i32 {
    let scans = scan(home);
    let runs: u64 = scans.iter().map(|s| s.runs.iter().filter(|r| !is_forgotten(r)).count() as u64).sum();
    let also: Vec<(&str, &str)> = CLEAR_ALSO.iter().copied().filter(|(n, _)| home.join(n).exists()).collect();
    if runs == 0 && also.is_empty() {
        println!("Nothing is saved.");
        return 0;
    }
    println!("To delete everything fewer saved in {} (its settings stay):", home.display());
    if runs > 0 {
        println!("  {}: command output and Claude Code's chats", runs_word(runs));
    }
    for (n, what) in &also {
        println!("  {n}: {what}");
    }
    if !yes {
        if !io::stdin().is_terminal() {
            println!("Not deleted: this asks first, and there is no terminal to answer. Run it again with --yes to delete them.");
            return 1;
        }
        print!("Delete them? [y/N] ");
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was deleted.");
            return 0;
        }
    }
    let mut code = if runs > 0 { forget(home, &["--all", "--yes"]) } else { 0 };
    // (the names are fixed, in the store: forgotten.log too, which forgetting just wrote)
    let mut gone = Vec::new();
    for (n, _) in CLEAR_ALSO {
        let p = home.join(n);
        let r = match fs::symlink_metadata(&p) {
            Err(_) => continue,
            Ok(m) if m.is_dir() => fs::remove_dir_all(&p),
            Ok(_) => fs::remove_file(&p),
        };
        match r {
            Ok(()) => gone.push(n),
            Err(e) => {
                eprintln!("fewer memory clear: cannot delete {}: {e}", p.display());
                code = 1;
            }
        }
    }
    if !gone.is_empty() {
        println!("Deleted: {}.", gone.join(", "));
    }
    code
}

/// The project's next run id (ids below it were given out once).
fn next_id(p: &Project) -> u64 {
    let text = fs::read_to_string(p.dir.join("project")).unwrap_or_default();
    store::read_settings(&text).iter().find(|(k, _)| k == "next").and_then(|(_, v)| v.parse().ok()).unwrap_or(1)
}

/// "r3, r5-r9, r12": ids, runs in a row joined (at most 20 parts, then how many more).
fn short_ids(ids: &[u64]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        parts.push(if j > i { format!("r{}-r{}", ids[i], ids[j]) } else { format!("r{}", ids[i]) });
        i = j + 1;
    }
    if parts.len() > 20 {
        let more = parts.len() - 20;
        parts.truncate(20);
        parts.push(format!("and {more} more"));
    }
    parts.join(", ")
}

// ---------------------------------------------------------------- limit

/// The limit in the config: (bytes on disk, months); none for "none", unset or unreadable.
fn limits(config: &[(String, String)]) -> (Option<u64>, Option<i64>) {
    let size = crate::config::get(config, "limit_size").filter(|v| !v.eq_ignore_ascii_case("none")).and_then(parse_size);
    let months = crate::config::get(config, "limit_months").filter(|v| !v.eq_ignore_ascii_case("none")).and_then(|v| v.trim().parse::<i64>().ok()).filter(|&m| m > 0);
    (size, months)
}

fn limit_text(size: Option<u64>, months: Option<i64>) -> String {
    let mut parts = Vec::new();
    if let Some(s) = size {
        parts.push(format!("at most {} bytes on disk", group(s)));
    }
    if let Some(m) = months {
        parts.push(format!("runs of the last {m} {} ({} days)", if m == 1 { "month" } else { "months" }, m * 30));
    }
    parts.join(" and ")
}

/// A size given to `limit --size`: a number (a fraction allowed) and a unit, B, KB, MB, GB, TB
/// (1000s) or KiB, MiB, GiB, TiB (1024s), in any case: "20GB", "1.5 TB", "500mb". None for 0
/// or anything else.
pub fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    let cut = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = (&t[..cut], t[cut..].trim());
    let (ip, fp) = num.split_once('.').unwrap_or((num, ""));
    if ip.is_empty() || (num.contains('.') && fp.is_empty()) || fp.contains('.') {
        return None;
    }
    let mul: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" | "byte" | "bytes" => 1,
        "kb" | "k" => 1_000,
        "mb" | "m" => 1_000_000,
        "gb" | "g" => 1_000_000_000,
        "tb" | "t" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return None,
    };
    let mut v = ip.parse::<u64>().ok()?.checked_mul(mul)?;
    if !fp.is_empty() {
        let fp = &fp[..fp.len().min(12)];
        let f: u128 = fp.parse().ok()?;
        v = v.checked_add((f * mul as u128 / 10u128.pow(fp.len() as u32)) as u64)?;
    }
    (v > 0).then_some(v)
}

/// The runs the limit forgets now: the oldest saved runs past it (ready or failed, oldest
/// start first), never the newest saved run of the store. Per project, the ids.
fn limit_plan(home: &Path, size: Option<u64>, months: Option<i64>, now: i64) -> Vec<(Project, Vec<u64>)> {
    let scans = scan(home);
    // (start, project index, id, bytes on disk)
    let mut cands: Vec<(i64, usize, u64, u64)> = Vec::new();
    for (k, s) in scans.iter().enumerate() {
        for r in &s.runs {
            if is_forgotten(r) || !matches!(r.state(), "ready" | "failed") {
                continue;
            }
            let bytes = run_files(home, &s.p, r.id()).iter().map(|f| len_of(f)).sum();
            cands.push((r.i("start").unwrap_or(0), k, r.id(), bytes));
        }
    }
    cands.sort_by(|a, b| (a.0, &scans[a.1].p.pid, a.2).cmp(&(b.0, &scans[b.1].p.pid, b.2)));
    // the newest saved run stays, whatever the limit
    cands.pop();
    let mut take: BTreeSet<(usize, u64)> = BTreeSet::new();
    if let Some(m) = months {
        let edge = now - m * MONTH_MS;
        for c in cands.iter().filter(|c| c.0 < edge) {
            take.insert((c.1, c.2));
        }
    }
    if let Some(limit) = size {
        let mut total: u64 = scans.iter().map(|s| project_disk(home, &s.p)).sum();
        total = total.saturating_sub(cands.iter().filter(|c| take.contains(&(c.1, c.2))).map(|c| c.3).sum::<u64>());
        for c in &cands {
            if total <= limit {
                break;
            }
            if take.insert((c.1, c.2)) {
                total = total.saturating_sub(c.3);
            }
        }
    }
    let mut plan: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
    for (k, id) in take {
        plan.entry(k).or_default().push(id);
    }
    plan.into_iter().map(|(k, ids)| (scans[k].p.clone(), ids)).collect()
}

/// After a save (add, pack-pending, the sessions sweep; no store lock held): with a limit set,
/// forget the oldest saved runs past it and say so in forgotten.log. A line for the person when
/// something went.
pub fn after_save(home: &Path) -> Option<String> {
    let (size, months) = limits(&crate::config::read(home));
    if size.is_none() && months.is_none() {
        return None;
    }
    let now = now_ms();
    let plan = limit_plan(home, size, months, now);
    if plan.is_empty() {
        return None;
    }
    let done = match forget_runs(home, &plan) {
        Ok(d) => d,
        Err(e) => {
            crate::mcp::debug(&format!("memory limit: cannot forget: {e}"));
            return Some(format!("The memory's limit could not forget the oldest runs: {e}"));
        }
    };
    if done.runs == 0 {
        return None;
    }
    let line = format!(
        "{} limit {}: forgot {}, {} bytes of text, {} bytes on disk: {}\n",
        fmt_local(now),
        limit_text(size, months),
        runs_word(done.runs),
        group(done.text),
        group(done.disk),
        done.refs.join(" ")
    );
    let _ = OpenOptions::new().append(true).create(true).mode(0o600).open(home.join(FORGOTTEN_LOG)).and_then(|mut f| f.write_all(line.as_bytes()));
    Some(format!("The memory's limit forgot {} of the oldest ({} bytes on disk); `fewer memory` says which.", runs_word(done.runs), group(done.disk)))
}

fn limit(home: &Path, args: &[&str]) -> i32 {
    let mut size = None;
    let mut months = None;
    let mut none = false;
    let mut i = 0;
    while i < args.len() {
        match (args[i], args.get(i + 1)) {
            ("--none", _) => none = true,
            ("--size", Some(v)) => {
                match parse_size(v) {
                    Some(_) => size = Some(v.trim().to_string()),
                    None => {
                        eprintln!("fewer memory limit: cannot read the size '{v}': give a number and a unit, such as 20GB, 500MB or 1.5TB");
                        return 2;
                    }
                }
                i += 1;
            }
            ("--months", Some(v)) => {
                match v.trim().parse::<i64>() {
                    Ok(m) if m > 0 => months = Some(m.to_string()),
                    _ => {
                        eprintln!("fewer memory limit: --months needs a whole number of months, 1 or more");
                        return 2;
                    }
                }
                i += 1;
            }
            (x, _) => {
                eprintln!("fewer memory limit: unknown or incomplete argument '{x}'\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    if none && (size.is_some() || months.is_some()) {
        eprintln!("fewer memory limit: --none goes alone");
        return 2;
    }
    let set = |k: &str, v: &str| crate::config::set(home, k, v).map_err(|e| eprintln!("fewer memory limit: cannot write {}: {e}", crate::config::path(home).display()));
    if none && set("limit_size", "none").and_then(|_| set("limit_months", "none")).is_err() {
        return 1;
    }
    if let Some(s) = &size
        && set("limit_size", s).is_err()
    {
        return 1;
    }
    if let Some(m) = &months
        && set("limit_months", m).is_err()
    {
        return 1;
    }
    let (size, months) = limits(&crate::config::read(home));
    if size.is_none() && months.is_none() {
        println!("Limit: none: nothing is ever deleted on its own.");
        return 0;
    }
    let disk: u64 = scan(home).iter().map(|s| project_disk(home, &s.p)).sum();
    println!("Limit: {}. The projects take {} bytes on disk now.", limit_text(size, months), group(disk));
    let plan = limit_plan(home, size, months, now_ms());
    let n: usize = plan.iter().map(|(_, ids)| ids.len()).sum();
    if n == 0 {
        println!("Nothing is past it now. After each save, the oldest runs past it are forgotten.");
    } else {
        println!("Past it now: {} of the oldest. They are forgotten at the next save (`forget` can do it now):", runs_word(n as u64));
        for (p, ids) in &plan {
            println!("  {}: {}", p.pid, short_ids(ids));
        }
    }
    0
}

// ---------------------------------------------------------------- pause, resume

fn pause(home: &Path, args: &[&str], on: bool) -> i32 {
    let verb = if on { "pause" } else { "resume" };
    match args {
        [] => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let root = store::project_root(&cwd);
            let p = match store::ensure_project(home, &root) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("fewer memory {verb}: cannot open the memory store {}: {e}", home.display());
                    return 1;
                }
            };
            let flag = p.dir.join("paused");
            if on {
                if let Err(e) = store::write_atomic(&flag, b"") {
                    eprintln!("fewer memory pause: cannot write {}: {e}", flag.display());
                    return 1;
                }
                println!("Saving is paused for {} ({}): `run` still runs commands and saves nothing. `fewer memory resume` starts saving again.", p.pid, p.root);
            } else if flag.exists() {
                if let Err(e) = fs::remove_file(&flag) {
                    eprintln!("fewer memory resume: cannot remove {}: {e}", flag.display());
                    return 1;
                }
                println!("Saving is on again for {} ({}).", p.pid, p.root);
            } else {
                println!("Saving was not paused for {} ({}).", p.pid, p.root);
            }
            // a folder pause over this folder still holds
            for (k, v) in crate::config::read(home) {
                if k == "pause" && under(&cwd, &v) {
                    println!("Saving is also paused under {v} (config): `fewer memory resume {v}` starts it there.");
                }
            }
            0
        }
        [folder] => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let abs = std::path::absolute(cwd.join(folder)).unwrap_or_else(|_| cwd.join(folder));
            let text = abs.to_string_lossy().trim_end_matches('/').to_string();
            let text = if text.is_empty() { "/".to_string() } else { text };
            let r = if on { crate::config::add_line(home, "pause", &text).map(usize::from) } else { crate::config::remove_lines(home, "pause", &|v| same_folder(v, &text)) };
            match r {
                Err(e) => {
                    eprintln!("fewer memory {verb}: cannot write {}: {e}", crate::config::path(home).display());
                    1
                }
                Ok(n) => {
                    match (on, n > 0) {
                        (true, true) => println!("Saving is paused under {text}: `run` still runs commands there and saves nothing. `fewer memory resume {text}` starts saving again."),
                        (true, false) => println!("Saving was already paused under {text}."),
                        (false, true) => println!("Saving is on again under {text}."),
                        (false, false) => println!("Saving was not paused under {text} (no such line in {}).", crate::config::path(home).display()),
                    }
                    0
                }
            }
        }
        _ => {
            eprintln!("fewer memory {verb}: give one folder, or none for this folder's project\n{USAGE}");
            2
        }
    }
}

/// Whether `cwd` lies under the folder of a `pause` line (as `run` checks it).
fn under(cwd: &Path, folder: &str) -> bool {
    let f = Path::new(folder);
    f.is_absolute() && (cwd.starts_with(f) || fs::canonicalize(f).is_ok_and(|r| cwd.starts_with(r)))
}

/// Whether a `pause` line's folder is the folder given to `resume`.
fn same_folder(line: &str, folder: &str) -> bool {
    let a = line.trim().trim_end_matches('/');
    let b = folder.trim_end_matches('/');
    a == b || (fs::canonicalize(if a.is_empty() { "/" } else { a }).ok().zip(fs::canonicalize(if b.is_empty() { "/" } else { b }).ok()).is_some_and(|(x, y)| x == y))
}

// ---------------------------------------------------------------- sessions, secrets

fn sessions_text(config: &[(String, String)]) -> String {
    let mut notes = Vec::new();
    let on = crate::sessions::agents_on(config, &mut notes);
    if on.is_empty() {
        "Sessions: off: no agent's sessions are read.".into()
    } else {
        format!("Sessions: saved for {}.", on.iter().map(|a| crate::sessions::agent_name(a)).collect::<Vec<_>>().join(", "))
    }
}

fn sessions(home: &Path, args: &[&str]) -> i32 {
    let (key, value) = match args {
        [] => {
            println!("{}", sessions_text(&crate::config::read(home)));
            println!("Each agent's sessions are read from its own folder (Claude Code's ~/.claude, Codex's ~/.codex, Aider's history in the project, Cline's and Cursor's in their editor's storage), or from the folder its setting names (FEWER_CLAUDE_DIR, FEWER_CODEX_DIR, FEWER_AIDER_DIR, FEWER_CLINE_DIR, FEWER_CURSOR_DIR); an agent whose folder is not there is passed over.");
            return 0;
        }
        [v @ ("on" | "off")] => ("sessions".to_string(), *v),
        [v @ ("on" | "off"), agent] if AGENTS.contains(agent) => (format!("sessions.{agent}"), *v),
        _ => {
            eprintln!("fewer memory sessions: give on or off, and an agent ({}) or none for all\n{USAGE}", AGENTS.join(", "));
            return 2;
        }
    };
    let mut r = crate::config::set(home, &key, value);
    // (one agent turned on while sessions are off: sessions on, and every other agent the config
    // does not name set off, so that only the one named is saved)
    let cfg = crate::config::read(home);
    if r.is_ok() && value == "on" && key != "sessions" && crate::config::get(&cfg, "sessions") != Some("on") {
        r = crate::config::set(home, "sessions", "on");
        for a in AGENTS.iter().filter(|a| format!("sessions.{a}") != key) {
            let k = format!("sessions.{a}");
            if r.is_ok() && crate::config::get(&cfg, &k).is_none() {
                r = crate::config::set(home, &k, "off");
            }
        }
    }
    if let Err(e) = r {
        eprintln!("fewer memory sessions: cannot write {}: {e}", crate::config::path(home).display());
        return 1;
    }
    println!("{}", sessions_text(&crate::config::read(home)));
    0
}

fn secrets() -> i32 {
    println!("Secrets hidden before saving (the published list, SECRETS.md): each is replaced by a marker such as \"[hidden: AWS key]\".");
    for (name, what) in crate::secrets::SHAPES {
        println!("- {name}: {what}");
    }
    if let Ok(home) = store::home() {
        let extra: Vec<String> = crate::config::read(&home).into_iter().filter(|(k, _)| k == "secret").map(|(_, v)| v).collect();
        if !extra.is_empty() {
            println!("Your own, from {}:", crate::config::path(&home).display());
            for v in extra {
                match crate::secrets::Extra::parse(&v) {
                    Ok(_) => println!("- secret = {v}"),
                    Err(e) => println!("- secret = {v} (not used: {e})"),
                }
            }
        }
    }
    0
}

// ---------------------------------------------------------------- dna

/// `memory dna`: on or off, and what the store holds.
fn dna_status(home: &Path) -> i32 {
    dna_lines(home);
    0
}

fn dna_lines(home: &Path) {
    let st = dna_build::status(home);
    let s = st.settings;
    if s.on {
        println!("The DNA index is on{}.", if is_default(home) { " (the default)" } else { "" });
    } else {
        println!("The DNA index is off: nothing is built, and search uses no DNA index. `fewer memory dna on` turns it on.");
    }
    println!("It is made for runs with {} of text or more (dna_min), after saving and while the computer is idle.", size_text(s.min));
    match st.max {
        Some(m) => println!("One that would take more than {m}% of its run's archive, and more than 1 MiB, is not kept (dna_max); search works without it."),
        None => println!("It is kept whatever its size (dna_max = none)."),
    }
    if st.over > 0 {
        println!("{} over that size: no DNA index kept for {}.", runs_word(st.over as u64), if st.over == 1 { "it" } else { "them" });
    }
    if st.with > 0 {
        println!("{} with one: {} bytes of DNA indexes, {} of their {} bytes of archives.", runs_word(st.with as u64), group(st.dna_bytes), share(st.dna_bytes, st.archive_bytes), group(st.archive_bytes));
    }
    if st.missing > 0 {
        println!("{} of that size without one yet: `fewer memory dna build` builds {} now.", runs_word(st.missing as u64), if st.missing == 1 { "it" } else { "them" });
    } else if st.with == 0 {
        println!("No run is that big yet.");
    }
}

/// `memory dna on|off`: the store's config, then what it means now.
fn dna_set(home: &Path, on: bool) -> i32 {
    if let Err(e) = crate::config::set(home, "dna", if on { "on" } else { "off" }) {
        eprintln!("fewer memory dna: cannot write {}: {e}", crate::config::path(home).display());
        return 1;
    }
    let st = dna_build::status(home);
    if on {
        println!("The DNA index is on: runs with {} of text or more get one after saving and while the computer is idle.", size_text(st.settings.min));
        if st.missing > 0 {
            println!("{} of that size {} none yet: `fewer memory dna build` builds {} now.", runs_word(st.missing as u64), if st.missing == 1 { "has" } else { "have" }, if st.missing == 1 { "it" } else { "them" });
        }
    } else {
        println!("The DNA index is off: nothing is built, and search uses no DNA index.");
        if st.with > 0 {
            println!("The {} already made ({} bytes) are kept; `fewer memory dna on` uses them again.", if st.with == 1 { "DNA index".to_string() } else { format!("{} DNA indexes", st.with) }, group(st.dna_bytes));
        }
    }
    0
}

/// Whether the config leaves `dna` unset (on by default).
fn is_default(home: &Path) -> bool {
    crate::config::get(&crate::config::read(home), "dna").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20GB"), Some(20_000_000_000));
        assert_eq!(parse_size("20 gb"), Some(20_000_000_000));
        assert_eq!(parse_size("1.5TB"), Some(1_500_000_000_000));
        assert_eq!(parse_size("500MB"), Some(500_000_000));
        assert_eq!(parse_size("16MiB"), Some(16 << 20));
        assert_eq!(parse_size("1.25KiB"), Some(1280));
        assert_eq!(parse_size("300"), Some(300));
        assert_eq!(parse_size("0.001KB"), Some(1));
        for bad in ["", "GB", "0", "0GB", "-1GB", "1.GB", ".5GB", "1.2.3GB", "20 G B", "20XB", "1e9", "99999999999999999999TB"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn references() {
        assert_eq!(parse_ref("fewer-3fa2/r42"), Some((Some("fewer-3fa2".into()), 42, false)));
        assert_eq!(parse_ref("r42"), Some((None, 42, false)));
        assert_eq!(parse_ref("fewer-3fa2/r42:1201"), Some((Some("fewer-3fa2".into()), 42, true)));
        assert_eq!(parse_ref("fewer-3fa2/r42:1201-1210"), Some((Some("fewer-3fa2".into()), 42, true)));
        for bad in ["", "42", "r0", "/r42", "x/42", "r42:", "r42:a", "r42:1-", "fewer/r-1"] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
        assert_eq!(short_ids(&[3, 5, 6, 7, 9]), "r3, r5-r7, r9");
        assert_eq!(short_ids(&[1]), "r1");
    }

    #[test]
    fn forget_arguments() {
        assert!(parse_forget(&[]).is_err());
        assert!(parse_forget(&["--all", "--sessions"]).is_err());
        assert!(parse_forget(&["--all", "--project", "x"]).is_err());
        assert!(parse_forget(&["r1", "--project", "x"]).is_err());
        assert!(parse_forget(&["--before"]).is_err());
        assert!(parse_forget(&["--frobnicate"]).is_err());
        let a = parse_forget(&["--project", "x", "--before", "3d", "--yes"]).unwrap();
        assert_eq!((a.project.as_deref(), a.before.as_deref(), a.yes), (Some("x"), Some("3d"), true));
        let a = parse_forget(&["r1", "p-1/r2:5"]).unwrap();
        assert_eq!(a.refs, ["r1", "p-1/r2:5"]);
        assert!(parse_forget(&["--project", "x"]).is_ok());
    }

    #[test]
    fn runs_table_rewritten() {
        let d = crate::capture::tests::tmpdir("memory-rewrite");
        // lines of three runs, one with a later state line, a line that is not JSON, and a last
        // line cut short by a crash
        let text = b"{\"id\":1,\"kind\":\"run\",\"state\":\"running\"}\n{\"id\":2,\"kind\":\"session\",\"state\":\"packing\",\"source\":\"/s.jsonl\",\"offset\":0,\"size\":10,\"hash\":\"ab\",\"bytes\":10}\nnot json\n{\"id\":1,\"state\":\"ready\",\"bytes\":5}\n{\"id\":3,\"kind\":\"add\",\"state\":\"ready\"}\n{\"id\":2,\"state\":\"ready\",\"lines\":1}\n{\"id\":4,\"kind\"";
        fs::write(d.join("runs.jsonl"), text).unwrap();
        let runs = store::read_runs_all(&d);
        let two = runs.iter().find(|r| r.id() == 2).unwrap();
        let mark = forgotten_mark(two, 1_700_000_000_000);
        rewrite_runs(&d, &[1u64, 2].into_iter().collect(), &[mark]).unwrap();
        let after = fs::read(d.join("runs.jsonl")).unwrap();
        let want = "not json\n{\"id\":3,\"kind\":\"add\",\"state\":\"ready\"}\n{\"id\":2,\"kind\":\"session\",\"state\":\"forgotten\",\"source\":\"/s.jsonl\",\"offset\":0,\"size\":10,\"hash\":\"ab\",\"forgotten\":1700000000000}\n";
        assert_eq!(String::from_utf8_lossy(&after), want);
        let runs = store::read_runs_all(&d);
        assert_eq!(runs.iter().map(|r| (r.id(), r.state().to_string())).collect::<Vec<_>>(), vec![(2, "forgotten".to_string()), (3, "ready".to_string())]);
        // no text and no counts are kept in the mark
        assert!(runs[0].i("bytes").is_none() && runs[0].i("lines").is_none());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn forgets_files_and_keeps_ids() {
        let base = crate::capture::tests::tmpdir("memory-forget");
        let home = base.join("store");
        let root = base.join("proj");
        fs::create_dir_all(&root).unwrap();
        let p = store::ensure_project(&home, &root).unwrap();
        // run 1: a kept file; run 2: a failed command with a capture left; run 3 stays
        for (kind, state) in [("kept", "ready"), ("run", "failed"), ("kept", "ready")] {
            let id = store::new_run(&p, Json::o(vec![("kind", kind.into()), ("state", state.into()), ("start", Json::from(1_000i64))])).unwrap();
            fs::write(p.dir.join(format!("r{id:06}.keep")), vec![b'x'; id as usize * 10]).unwrap();
        }
        let raw = crate::capture::raw_dir(&home, &p.pid);
        fs::create_dir_all(&raw).unwrap();
        fs::write(raw.join(crate::capture::out_file(2)), b"output\n").unwrap();
        fs::write(raw.join(crate::capture::times_file(2)), b"1 5\n").unwrap();
        // a file of run 20 must stay: its name starts like run 2's would without the zeros
        fs::write(p.dir.join("r000020.keep"), b"other").unwrap();
        let d = forget_runs(&home, &[(p.clone(), vec![1, 2])]).unwrap();
        assert_eq!((d.runs, d.disk, d.kept), (2, 10 + 20 + 7 + 4, 0));
        assert_eq!(run_files(&home, &p, 1).len() + run_files(&home, &p, 2).len(), 0);
        assert!(p.dir.join("r000003.keep").exists() && p.dir.join("r000020.keep").exists());
        assert_eq!(store::read_runs(&p.dir).iter().map(RunRec::id).collect::<Vec<_>>(), vec![3]);
        // keys.cat and moments.jsonl are made again (empty: no catalog is left)
        assert!(p.dir.join(crate::catalog::REGISTRY).exists());
        // ids are not used again
        let id = store::new_run(&p, Json::o(vec![("kind", "run".into())])).unwrap();
        assert_eq!(id, 4);
        // forgetting again: nothing left to do
        let d = forget_runs(&home, &[(p.clone(), vec![1, 2])]).unwrap();
        assert_eq!(d.runs, 0);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_limit_keeps_the_newest() {
        let base = crate::capture::tests::tmpdir("memory-limit");
        let home = base.join("store");
        let root = base.join("proj");
        fs::create_dir_all(&root).unwrap();
        let p = store::ensure_project(&home, &root).unwrap();
        for (i, start) in [(1u64, 3_000i64), (2, 1_000), (3, 2_000)] {
            let id = store::new_run(&p, Json::o(vec![("kind", "kept".into()), ("state", "ready".into()), ("start", Json::from(start))])).unwrap();
            assert_eq!(id, i);
            fs::write(p.dir.join(format!("r{id:06}.keep")), vec![b'x'; 1000]).unwrap();
        }
        let other = folder_bytes(&p.dir) - 3000;
        // room for two runs: the oldest start (r2) goes
        let plan = limit_plan(&home, Some(other + 2000), None, 10_000);
        assert_eq!(plan.iter().map(|(_, ids)| ids.clone()).collect::<Vec<_>>(), vec![vec![2]]);
        // room for none: every run but the newest (r1, started last)
        let plan = limit_plan(&home, Some(1), None, 10_000);
        assert_eq!(plan[0].1, vec![2, 3]);
        // months: runs older than the edge, never the newest
        let plan = limit_plan(&home, None, Some(1), 1_500 + MONTH_MS);
        assert_eq!(plan[0].1, vec![2]);
        assert!(limit_plan(&home, None, Some(1), 0).is_empty());
        fs::remove_dir_all(&base).unwrap();
    }
}
