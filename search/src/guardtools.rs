//! Claude's tools over the Guard notes and the fix memory: `guard` (the latest moments) and `fixes` (has this error happened before, and what came
//! right before it stopped?). The moments and the stops are guard.rs's; this module sits inside
//! tools.rs's, beside keytools.rs, and shares their scope, arguments and answer helpers.
//!
//! - guard: the moments of the scope's runs, newest first, each with its time, kind, key,
//!   exact count and a line with its reference. They are read from moments.jsonl when it and
//!   keys.cat hold every ready run of the project; else they are made now from the runs'
//!   catalogs (never written), and the answer says so. Runs still being saved are named.
//! - fixes: for each error key holding a text (or the key of the record a reference points
//!   into): how many times and when it was seen; each time it stopped, where it was last seen
//!   before, the run without it (its time, commit, branch and exit), the git commits in between
//!   and the files they changed, the agents' sessions of that time with the files they edited
//!   (and which of those the commits changed or the error's record names), and when it came
//!   back. It shows what came right before; it never names a cause.
//!
//! git is only read, in the project's root folder, with a 2 s limit per call: `git log` and
//! `git rev-list --count` over the range, `git diff --name-only` between the two commits, and
//! `git rev-parse --show-toplevel`. Commits are taken from runs.jsonl only when they are hex.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::keytools::{Cats, Lines, RECORD_LINES, Row, cut_note, minute_text, show_rows, table_cursor};
use super::{Args, Ctx, key_text, minute_span, not_ready, parse_line_ref, plural, projects, select, short_commit, what};
use crate::answer::{Answer, group, line_ref, next_call, run_ref};
use crate::catalog::{self, Catalog, RegKey, Registry, Seen};
use crate::formats::Level;
use crate::guard::{self, Kind, Note, Stop, no_log};
use crate::json::Json;
use crate::store::{self, Project, RunRec};

/// The most commits a stop lists, changed files it names, sessions it lists, and
/// edited files per session.
const COMMITS: usize = 10;
const FILES: usize = 20;
const SESSIONS: usize = 10;
const EDITED: usize = 10;

/// A project's key registry (with the fix memory's stops) and its Guard notes: as stored when
/// keys.cat and moments.jsonl hold every ready run, else made now from the runs' catalogs
/// (a catalog that does not fit is made from the run's bytes; nothing is written). Also why
/// they were made now.
fn project_notes(pi: usize, p: &Project, runs: &[RunRec], cats: &mut Cats) -> (Registry, Vec<Note>, Option<String>) {
    let ready: Vec<&RunRec> = runs.iter().filter(|r| r.state() == "ready" && !no_log(r)).collect();
    let ids: Vec<u64> = ready.iter().map(|r| r.id()).collect();
    if let Some((reg, notes)) = guard::stored(&p.dir, &ids) {
        return (reg, notes, None);
    }
    let why = match Registry::read(&p.dir) {
        None => "keys.cat is missing or damaged",
        Some(reg) if reg.runs != ids => "keys.cat does not hold exactly the saved runs",
        Some(_) => "moments.jsonl is missing, damaged, or not the one keys.cat was written with",
    };
    for r in &ready {
        let _ = cats.load(pi, p, r);
    }
    let pairs: Vec<(&RunRec, &Catalog)> = ready.iter().filter_map(|r| cats.cat(pi, r.id()).map(|c| (*r, c))).collect();
    let (reg, notes) = guard::derive(runs, &pairs);
    (reg, notes, Some(why.to_string()))
}

/// A run of `runs` by its id.
fn run_of(runs: &[RunRec], id: u64) -> Option<&RunRec> {
    runs.iter().find(|r| r.id() == id)
}

/// A run's branch and commit: "main@2222bbbb2", "commit 2222bbbb2", with " (dirty)" when it
/// ran with changes not committed; empty without a commit.
fn at_commit(r: &RunRec) -> String {
    let mut s = match (r.s("branch"), r.s("commit")) {
        (Some(b), _) => format!("{b}@{}", short_commit(r)),
        (None, Some(_)) => format!("commit {}", short_commit(r)),
        _ => return String::new(),
    };
    if r.0.get("dirty").and_then(Json::bool) == Some(true) {
        s.push_str(" (dirty)");
    }
    s
}

/// A run in a few words: "run ./fake-app --test, main@2222bbbb2 (dirty), exit 1".
fn run_words(r: &RunRec) -> String {
    let mut s = r.kind().to_string();
    let w = what(r);
    if !w.is_empty() {
        s.push_str(&format!(" {w}"));
    }
    let c = at_commit(r);
    if !c.is_empty() {
        s.push_str(&format!(", {c}"));
    }
    if let Some(e) = r.i("exit") {
        s.push_str(&format!(", exit {e}"));
    }
    if let Some(sig) = r.i("signal") {
        s.push_str(&format!(", signal {sig}"));
    }
    s
}

/// A key as the answers quote it: "<time> ERROR db: ...", cut at 300 bytes.
fn quoted(key: &[u8]) -> String {
    Json::Str(key_text(key, 300)).to_string()
}

// ---------------------------------------------------------------- guard

/// A moment's two lines: what it is (time, kind, key, exact count, run), then its line with its
/// reference.
fn note_row(ps: &[Project], runs_of: &[Vec<RunRec>], lines: &Lines, pi: usize, n: &Note, line_max: usize) -> Row {
    let pid = &ps[pi].pid;
    let rf = run_ref(pid, n.run);
    let key = quoted(n.key.as_bytes());
    let at = minute_text(n.t);
    let head = match n.kind {
        Kind::New => format!("{at} new: {key}: first seen in the project, {} in {rf}", plural(n.count, "record")),
        Kind::Rare => {
            let s = n.seen.unwrap_or(Seen { ms: n.t, run: n.run, line: n.line });
            format!("{at} rare: {key}: {} in {rf}; {} before it in the project, the first {} at {}", plural(n.count, "record"), plural(n.before, "record"), minute_text(s.ms), line_ref(pid, s.run, s.line))
        }
        Kind::Jump => format!("{at} jump: {key}: {} in this minute in {rf}; its median was {} a minute in the hour before (the minutes it logged in)", plural(n.count, "record"), Note::median(n.before)),
        Kind::Burst => format!("{at} burst: {} in this minute in {rf}, after 5 minutes with fewer than 2 a minute ({} in them)", plural(n.count, "error record"), group(n.before)),
        Kind::Failed => {
            let how = match (n.signal, n.exit) {
                (Some(s), _) => format!("signal {s}"),
                (None, Some(e)) => format!("exit {e}"),
                _ => "a failure".to_string(),
            };
            let w = run_of(&runs_of[pi], n.run).map(what).unwrap_or_default();
            format!("{at} failed: {rf} {w} ended with {how} ({})", plural(n.count, "error record"))
        }
        Kind::Back => {
            let s = n.seen.unwrap_or(Seen { ms: n.t, run: n.run, line: n.line });
            let (srun, sms) = n.stop.unwrap_or((n.run, n.t));
            let tags = run_of(&runs_of[pi], srun).map(at_commit).filter(|c| !c.is_empty()).map(|c| format!(", {c}")).unwrap_or_default();
            let call = Json::o(vec![("ref", line_ref(pid, n.run, n.line).into())]);
            format!(
                "{at} back: {key} came back: {} in {rf}; it had stopped in {} ({}{tags}), last seen before at {} ({}). What came right before it stopped: fixes {call}",
                plural(n.count, "record"),
                run_ref(pid, srun),
                minute_text(sms),
                line_ref(pid, s.run, s.line),
                minute_text(s.ms)
            )
        }
    };
    let mut row = Row::default();
    row.lines.push(head);
    if n.line > 0 {
        let l = lines.show(ps, pi, n.run, n.line, line_max, &mut row);
        row.lines.push(format!("  {l}"));
    } else {
        row.lines.push("  (the run has no lines)".to_string());
    }
    row
}

pub(super) fn tool_guard(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let sc = select(ctx, a, true)?;
    let skip = table_cursor(a)?;
    let limit = a.i("limit", 20) as usize;
    let line_max = a.i("line_max", 400) as usize;
    // (the schema takes kind names only)
    let asked: Vec<Kind> = a.list("kinds").iter().filter_map(|k| Kind::of_name(k)).collect();
    let kinds: Vec<Kind> = if asked.is_empty() { Kind::ALL.to_vec() } else { asked.clone() };
    // a moment is in the window when its minute is (the catalogs' precision)
    let now = crate::times::now_ms();
    let since = a.s("since").map(|s| crate::times::parse_when(s, now)).transpose()?.map(|t| t.div_euclid(60_000));
    let until = a.s("until").map(|s| crate::times::parse_when(s, now)).transpose()?.map(|t| t.div_euclid(60_000));
    let in_scope: HashSet<(usize, u64)> = sc.runs.iter().map(|(pi, r)| (*pi, r.id())).collect();
    let short = |pi: usize, id: u64| if sc.projects.len() == 1 { format!("r{id}") } else { run_ref(&sc.projects[pi].pid, id) };
    let mut cats = Cats::new(stop);
    let mut made = Vec::new();
    let mut all: Vec<(usize, Note)> = Vec::new();
    let mut runs_of: Vec<Vec<RunRec>> = Vec::new();
    for (pi, p) in sc.projects.iter().enumerate() {
        let runs = store::read_runs(&p.dir);
        let (_, notes, why) = project_notes(pi, p, &runs, &mut cats);
        if let Some(w) = why {
            made.push(format!("{} ({w})", p.pid));
        }
        for n in notes {
            let m = n.t.div_euclid(60_000);
            if in_scope.contains(&(pi, n.run)) && kinds.contains(&n.kind) && since.is_none_or(|s| m >= s) && until.is_none_or(|u| m <= u) {
                all.push((pi, n));
            }
        }
        runs_of.push(runs);
    }
    // newest first: by time, then run (newest first), then line, then kind
    all.sort_by(|(pa, x), (pb, y)| y.t.cmp(&x.t).then(y.run.cmp(&x.run)).then(x.line.cmp(&y.line)).then(x.kind.cmp(&y.kind)).then(pa.cmp(pb)));
    let counted = sc.runs.iter().filter(|(_, r)| r.state() == "ready" && !no_log(r)).count() as u64;
    // the scope's runs that are being saved: their moments come once they are saved
    let waiting: Vec<String> = sc
        .runs
        .iter()
        .filter(|(_, r)| r.state() != "ready" && !no_log(r))
        .map(|(pi, r)| format!("{} ({})", short(*pi, r.id()), not_ready(&sc.projects[*pi], r, "its moments come once it is saved")))
        .collect();
    let per = |v: &[(usize, Note)]| {
        let s: Vec<String> = Kind::ALL.iter().map(|k| (k, v.iter().filter(|x| x.1.kind == *k).count())).filter(|x| x.1 > 0).map(|(k, n)| format!("{} {}", k.name(), group(n as u64))).collect();
        if s.is_empty() { "none".to_string() } else { s.join(", ") }
    };
    let total = all.len();
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    let only = if asked.is_empty() { String::new() } else { format!(", kinds {}", asked.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ")) };
    ans.must(&format!("Guard notes, newest first: {} in {} ({}{only}): {}", plural(total as u64, "moment"), plural(counted, "run"), sc.desc, per(&all)));
    if !made.is_empty() {
        ans.must(&format!("moments made now from the runs' catalogs: {}", made.join(", ")));
    }
    for n in cats.notes() {
        ans.must(&n);
    }
    if !waiting.is_empty() {
        let more = if waiting.len() > 10 { format!(" and {} more", waiting.len() - 10) } else { String::new() };
        ans.must(&format!("not in these notes yet: {}{more}", waiting.iter().take(10).cloned().collect::<Vec<_>>().join(", ")));
    }
    // the moments from the cursor's on, with their lines read by reference
    let shown = all.get(skip..total.min(skip.saturating_add(limit))).unwrap_or(&[]);
    let mut want: BTreeMap<(usize, u64), Vec<(u64, u64)>> = BTreeMap::new();
    for (pi, n) in shown.iter().filter(|x| x.1.line > 0) {
        want.entry((*pi, n.run)).or_default().push((n.line, n.line));
    }
    let lines = Lines::read(&sc.projects, &want);
    let rows: Vec<Row> = shown.iter().map(|(pi, n)| note_row(&sc.projects, &runs_of, &lines, *pi, n, line_max)).collect();
    let s = show_rows(&mut ans, &[], &rows, skip, total, limit);
    let cuts = cut_note(s.cuts, s.cut_ref.as_deref(), line_max);
    let last = match s.next {
        None => format!("all shown{cuts}"),
        Some(i) => {
            let left = &all[i..];
            let span = minute_span(left[left.len() - 1].1.t.div_euclid(60_000), left[0].1.t.div_euclid(60_000));
            format!(
                "left out: {} of {} moments (#{i} on, {span}: {}){cuts}. Next: {}",
                group(left.len() as u64),
                group(total as u64),
                per(left),
                next_call("guard", &a.with_cursor(&format!("#{i}")))
            )
        }
    };
    Ok(ans.finish(&last))
}

// ---------------------------------------------------------------- fixes

/// Whether `h` holds `n` (ASCII case ignored with `icase`).
fn find(h: &[u8], n: &[u8], icase: bool) -> Option<usize> {
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| if icase { w.eq_ignore_ascii_case(n) } else { w == n })
}

/// The asked text as the key rule writes it (its values as <num>, <ip>, <path>, <time>...), so
/// that a line pasted whole, or a message with its numbers, finds its key; None when that is
/// the text itself, or when nothing but values is left of it (a bare "42" would find every key
/// with a number).
fn as_key(text: &str) -> Option<Vec<u8>> {
    let k = crate::msgkey::line_key(text.as_bytes());
    if k == text.as_bytes() {
        return None;
    }
    let mut rest = String::from_utf8_lossy(&k).into_owned();
    for ph in ["<num>", "<ip>", "<time>", "<uuid>", "<url>", "<path>", "<str>", "<long>"] {
        rest = rest.replace(ph, " ");
    }
    let word = rest.as_bytes().split(|c| !c.is_ascii_alphabetic()).any(|w| w.len() >= 3);
    word.then_some(k)
}

/// Whether `text` names the file `name` (a path's last part) as a whole word: not inside a
/// longer name ("db.py" is not named by "mydb.py" or "db.pyc").
fn names_file(text: &[u8], name: &[u8]) -> bool {
    if name.len() < 3 {
        return false;
    }
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let mut i = 0;
    while let Some(p) = find(&text[i..], name, false) {
        let (s, e) = (i + p, i + p + name.len());
        if (s == 0 || !word(text[s - 1])) && (e == text.len() || !word(text[e])) {
            return true;
        }
        i = s + 1;
    }
    false
}

/// git, read only, in the project's root folder: its output when it exits 0 within 2 s.
fn git(root: &str, args: &[&str]) -> Option<String> {
    let mut a: Vec<&OsStr> = vec![OsStr::new("-C"), OsStr::new(root), OsStr::new("-c"), OsStr::new("log.showSignature=false")];
    a.extend(args.iter().map(OsStr::new));
    store::run_quiet("git", &a, Duration::from_secs(2)).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// A commit recorded in runs.jsonl, when it is one (hex digits; never passed to git otherwise).
fn commit_of(r: &RunRec) -> Option<&str> {
    r.s("commit").filter(|c| (4..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// What git says of the commits from `a` to `b`: the lines of `git log` (at most COMMITS,
/// newest first), how many there are, and the files changed between them (paths from the
/// repository's top folder) with that folder. Err: why it could not be read.
#[allow(clippy::type_complexity)]
fn commits_between(root: &str, a: &str, b: &str) -> Result<(Vec<String>, u64, Vec<String>, String), String> {
    let range = format!("{a}..{b}");
    let fail = || format!("git could not read them in {root} (not a git repository, or it does not have those commits)");
    let n = format!("-n{COMMITS}");
    let log = git(root, &["log", &n, "--no-decorate", "--date=iso", "--format=%h %ad %s", &range, "--"]).ok_or_else(fail)?;
    let count = git(root, &["rev-list", "--count", &range, "--"]).and_then(|s| s.trim().parse::<u64>().ok()).ok_or_else(fail)?;
    let diff = git(root, &["diff", "--name-only", "--no-ext-diff", "--no-textconv", a, b, "--"]).ok_or_else(fail)?;
    let top = git(root, &["rev-parse", "--show-toplevel"]).map(|s| s.trim_end_matches(['\n', '\r']).to_string()).unwrap_or_else(|| root.to_string());
    let lines = |s: &str| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>();
    Ok((lines(&log), count, lines(&diff), top))
}

/// A path as answers show it: from the project's root folder when it is inside it.
fn rel<'a>(path: &'a str, root: &str) -> &'a str {
    path.strip_prefix(root).and_then(|r| r.strip_prefix('/')).filter(|r| !r.is_empty()).unwrap_or(path)
}

/// An error key the answer covers.
struct Found {
    pi: usize,
    text: Vec<u8>,
    k: RegKey,
}

/// A row of fixes: a stop of key `key` (its index in `stops`), or the key's state now.
#[derive(Clone, Copy)]
enum FixRow {
    Stop(usize, usize),
    Now(usize),
}

/// The lines of a stop: where the key was last seen and the run without it, the commits in
/// between and the files they changed, the sessions of that time with the files they edited,
/// and when it came back.
#[allow(clippy::too_many_arguments)]
fn stop_row(ps: &[Project], runs_of: &[Vec<RunRec>], cats: &mut Cats, f: &Found, n: usize, s: &Stop, line_max: usize, row: &mut Row) {
    let (pi, p) = (f.pi, &ps[f.pi]);
    let runs = &runs_of[pi];
    let pid = &p.pid;
    let words = |id: u64| run_of(runs, id).map(run_words).unwrap_or_else(|| "a run no longer in runs.jsonl".to_string());
    row.lines.push(format!(
        "stop {n} of {}: last seen {} at {} ({}); not in {}, {} ({})",
        f.k.stops.len(),
        minute_text(s.last.ms),
        line_ref(pid, s.last.run, s.last.line),
        words(s.last.run),
        run_ref(pid, s.run),
        minute_text(s.ms),
        words(s.run)
    ));
    // the commits between the run it was last seen in and the run without it
    let (ra, rb) = (run_of(runs, s.last.run), run_of(runs, s.run));
    let mut changed: Vec<String> = Vec::new();
    let mut top = p.root.clone();
    match (ra.and_then(commit_of), rb.and_then(commit_of)) {
        (Some(a), Some(b)) if a == b => row.lines.push(format!("  no commit between them: both at {}", &a[..a.len().min(9)])),
        (Some(a), Some(b)) => match commits_between(&p.root, a, b) {
            Ok((log, count, diff, t)) => {
                let (a9, b9) = (&a[..a.len().min(9)], &b[..b.len().min(9)]);
                if count == 0 {
                    row.lines.push(format!("  no commit from {a9} to {b9} (git log {a9}..{b9} is empty)"));
                } else {
                    let shown = if (log.len() as u64) < count { format!(", the newest {} shown", log.len()) } else { String::new() };
                    row.lines.push(format!("  {} between {a9} and {b9}{shown}, newest first:", plural(count, "commit")));
                }
                for l in &log {
                    let (t, _) = crate::clean::escape(l.as_bytes(), false);
                    let (t, cut) = crate::clean::cut(&t, line_max);
                    row.lines.push(format!("    {t}{}", if cut > 0 { format!("…(+{} bytes)", group(cut as u64)) } else { String::new() }));
                }
                if diff.is_empty() {
                    row.lines.push("  no file changed between them".to_string());
                } else {
                    let names: Vec<String> = diff.iter().take(FILES).map(|d| crate::clean::escape(d.as_bytes(), false).0).collect();
                    let more = if diff.len() > FILES { format!(" (+{} more)", group((diff.len() - FILES) as u64)) } else { String::new() };
                    row.lines.push(format!("  files changed between them ({}): {}{more}", group(diff.len() as u64), names.join(", ")));
                }
                changed = diff;
                top = t;
            }
            Err(e) => row.lines.push(format!("  commits between {} and {}: {e}", &a[..a.len().min(9)], &b[..b.len().min(9)])),
        },
        (x, _) => {
            let id = if x.is_none() { s.last.run } else { s.run };
            row.lines.push(format!("  commits between: not known: {} has no commit (saved outside a git repository, or an added file)", run_ref(pid, id)));
        }
    }
    if rb.and_then(|r| r.0.get("dirty")).and_then(Json::bool) == Some(true) {
        row.lines.push(format!("  {} ran with changes not committed yet (dirty)", run_ref(pid, s.run)));
    }
    // the error's record: its first record in the run it was last seen in (a whole stack trace)
    let mut record: Vec<u8> = Vec::new();
    if let Some(ra) = ra
        && cats.load(pi, p, ra).is_ok()
        && let Some(c) = cats.cat(pi, ra.id())
    {
        let e = c.keys.iter().filter(|k| k.text == f.text).min_by_key(|k| (k.level != Level::Error, k.first_line));
        if let Some(e) = e {
            let (r0, r1) = e.first_record;
            let to = r1.min(r0.saturating_add(RECORD_LINES - 1));
            if let Ok(v) = crate::fetch::fetch(&p.dir.join(store::run_file(ra.id())), &[(r0, to)], 1) {
                for (_, b) in v {
                    record.extend_from_slice(&b);
                }
            }
        }
    }
    // the sessions of that time: saved agents' sessions of this project whose time overlaps
    // from where it was last seen to the run without it
    let span = |r: &RunRec| match (r.i("first"), r.i("last")) {
        (Some(a), Some(b)) => (a, b),
        _ => (r.i("start").unwrap_or(0), r.i("end").or(r.i("start")).unwrap_or(0)),
    };
    let sessions: Vec<&RunRec> = runs.iter().filter(|r| r.kind() == "session" && r.state() == "ready" && span(r).0 <= s.ms && span(r).1 >= s.last.ms).collect();
    let window = minute_span(s.last.ms.div_euclid(60_000), s.ms.div_euclid(60_000));
    if sessions.is_empty() {
        row.lines.push(format!("  no saved session in that time ({window})"));
    } else {
        let more = if sessions.len() > SESSIONS { format!(", the first {SESSIONS} shown") } else { String::new() };
        row.lines.push(format!("  {} in that time ({window}){more}:", plural(sessions.len() as u64, "saved session")));
    }
    let changed_paths: HashSet<String> = changed.iter().map(|d| format!("{top}/{d}")).collect();
    for r in sessions.iter().take(SESSIONS) {
        let (a, b) = span(r);
        let who = match (r.s("agent"), r.s("session")) {
            (Some(ag), Some(id)) => format!("{ag} session {}", id.chars().take(36).collect::<String>()),
            (Some(ag), None) => format!("{ag} session"),
            (None, Some(id)) => format!("session {}", id.chars().take(36).collect::<String>()),
            (None, None) => "session".to_string(),
        };
        let head = format!("    {} {who}, {}", run_ref(pid, r.id()), minute_span(a.div_euclid(60_000), b.div_euclid(60_000)));
        let files = match cats.load(pi, p, r) {
            Ok(()) => cats.cat(pi, r.id()).map(|c| c.files.clone()).unwrap_or_default(),
            Err(e) => {
                row.lines.push(format!("{head}; its edited files not read ({e})"));
                continue;
            }
        };
        if files.is_empty() {
            row.lines.push(format!("{head}; it edited no file"));
            continue;
        }
        row.lines.push(format!("{head}; it edited {}:", plural(files.len() as u64, "file")));
        // the files the commits changed or the record names first, then in the order edited
        let marks = |path: &str| {
            let mut m = Vec::new();
            if changed_paths.contains(path) || changed.iter().any(|d| d == rel(path, &p.root)) {
                m.push("those commits changed it");
            }
            let name = path.rsplit('/').next().unwrap_or(path);
            if names_file(&record, name.as_bytes()) {
                m.push("the error's record names it");
            }
            m
        };
        let mut v: Vec<(usize, &(String, u64), Vec<&str>)> = files.iter().enumerate().map(|(i, f)| (i, f, marks(&f.0))).collect();
        v.sort_by_key(|x| (x.2.is_empty(), x.0));
        for (_, (path, line), m) in v.iter().take(EDITED) {
            let (t, _) = crate::clean::escape(rel(path, &p.root).as_bytes(), false);
            let m = if m.is_empty() { String::new() } else { format!(" ({})", m.join("; ")) };
            row.lines.push(format!("      {t} at {}{m}", line_ref(pid, r.id(), *line)));
        }
        if files.len() > EDITED {
            row.lines.push(format!("      (+{} more files)", group((files.len() - EDITED) as u64)));
        }
    }
    if let Some(b) = s.back {
        row.lines.push(format!("  came back {} at {} ({})", minute_text(b.ms), line_ref(pid, b.run, b.line), words(b.run)));
    }
}

pub(super) fn tool_fixes(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let (text, rf) = (a.s("text"), a.s("ref"));
    match (text, rf) {
        (None, None) => return Err("give `text` (words of the error message) or `ref` (a reference to one of its lines, fewer-3fa2/r42:1201)".into()),
        (Some(_), Some(_)) => return Err("give `text` or `ref`, not both".into()),
        _ => {}
    }
    let home = ctx.home.clone()?;
    let skip = table_cursor(a)?;
    let limit = a.i("limit", 20) as usize;
    let line_max = a.i("line_max", 400) as usize;
    let icase = a.b("icase");
    // the projects: the reference's, else the call's
    let target = match rf {
        Some(s) => {
            let (pid, id, line, _) = parse_line_ref(s).ok_or_else(|| format!("'{s}' is not a line reference (fewer-3fa2/r42:1201)"))?;
            if pid == Some("*") {
                return Err(format!("'{s}' is not a line reference: it names one project (fewer-3fa2/r42:1201)"));
            }
            Some((pid, id, line))
        }
        None => None,
    };
    let ps = projects(ctx, &home, target.and_then(|t| t.0).or(a.s("project")))?;
    if target.is_some() && ps.len() > 1 {
        return Err(format!("'{}': give its project too (fewer-3fa2/r42:1201)", rf.unwrap_or("")));
    }
    let alt = text.and_then(as_key);
    let mut cats = Cats::new(stop);
    let mut made = Vec::new();
    let mut found: Vec<Found> = Vec::new();
    let mut runs_of: Vec<Vec<RunRec>> = Vec::new();
    let mut error_keys = 0u64;
    let mut heading = String::new();
    for (pi, p) in ps.iter().enumerate() {
        let runs = store::read_runs(&p.dir);
        let (reg, _, why) = project_notes(pi, p, &runs, &mut cats);
        if let Some(w) = why {
            made.push(format!("{} ({w})", p.pid));
        }
        error_keys += reg.keys.values().filter(|k| k.error).count() as u64;
        match (target, text) {
            (Some((_, id, line)), _) => {
                // the record that holds the line, as the index pass reads it: its key
                let r = run_of(&runs, id).ok_or_else(|| format!("no run r{id} in {}", p.pid))?;
                if r.state() != "ready" || no_log(r) {
                    let why = not_ready(p, r, "its records are read once it is saved");
                    return Err(format!("{}: {why}", run_ref(&p.pid, id)));
                }
                let tags = catalog::Tags { name: catalog::source_name(r), ..catalog::Tags::default() };
                let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
                let path = p.dir.join(store::run_file(id));
                let Some((key, level, first, last)) = catalog::record_at(&path, &tags, line, threads, &|| stop.load(Ordering::Relaxed)).map_err(|e| format!("{}: {e}", run_ref(&p.pid, id)))? else {
                    return Err(format!("{} is past the end of its run ({} lines)", line_ref(&p.pid, id, line), r.i("lines").map_or("?".to_string(), |n| group(n as u64))));
                };
                let lines = if first == last { format!("line {first}") } else { format!("lines {first} to {last}") };
                match reg.keys.get(&key) {
                    Some(k) if k.error => {
                        let level = if level == Level::None { "no level".to_string() } else { format!("level {}", level.name()) };
                        heading = format!("fixes for {}: its record ({lines}, {level}) has an error key of {}; each stop shows what came right before it, oldest first", line_ref(&p.pid, id, line), p.pid);
                        found.push(Found { pi, text: key, k: k.clone() });
                    }
                    _ => {
                        let seen = reg.keys.get(&key).map_or(String::new(), |k| format!(" ({} in {}, first seen {} at {})", plural(k.records, "record"), plural(k.runs, "run"), minute_text(k.first.ms), line_ref(&p.pid, k.first.run, k.first.line)));
                        heading = format!(
                            "fixes for {}: its record ({lines}) has the key {}{seen}, which has no error record in {}; fixes follows error keys (level error, or multi-line error records): try errors",
                            line_ref(&p.pid, id, line),
                            quoted(&key),
                            p.pid
                        );
                    }
                }
            }
            (None, Some(t)) => {
                for (kt, k) in &reg.keys {
                    if k.error && (find(kt, t.as_bytes(), icase).is_some() || alt.as_deref().is_some_and(|x| find(kt, x, icase).is_some())) {
                        found.push(Found { pi, text: kt.clone(), k: k.clone() });
                    }
                }
            }
            (None, None) => {}
        }
        runs_of.push(runs);
    }
    // the most recently seen first
    found.sort_by(|x, y| y.k.last.cmp(&x.k.last).then(x.pi.cmp(&y.pi)).then(x.text.cmp(&y.text)));
    let desc = if ps.len() <= 3 { ps.iter().map(|p| p.pid.as_str()).collect::<Vec<_>>().join(", ") } else { format!("{} projects", ps.len()) };
    if let Some(t) = text {
        let as_written = Json::Str(t.to_string());
        let also = alt.as_ref().map_or(String::new(), |k| format!(" (or, as keys write it, {})", quoted(k)));
        heading = if found.is_empty() {
            format!(
                "fixes for {as_written}{also} ({desc}): no error key holds it, of {}. Keys write values as <num>, <ip>, <path>, <time>: give words of the message, or a ref to one of its lines; errors lists the error keys",
                plural(error_keys, "error key")
            )
        } else {
            format!("fixes for {as_written}{also} ({desc}): {} {} it; each stop shows what came right before it, oldest first", plural(found.len() as u64, "error key"), if found.len() == 1 { "holds" } else { "hold" })
        };
    }
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    ans.must(&heading);
    if !made.is_empty() {
        ans.must(&format!("stops made now from the runs' catalogs: {}", made.join(", ")));
    }
    // the rows: per key its stops, oldest first, then its state now
    let mut heads = Vec::new();
    let mut all: Vec<FixRow> = Vec::new();
    for (i, f) in found.iter().enumerate() {
        let pid = &ps[f.pi].pid;
        let backs = f.k.stops.iter().filter(|s| s.back.is_some()).count() as u64;
        heads.push(format!(
            "== {}: {} in {}, first seen {} at {}, last seen {} at {}; stopped {}, came back {}",
            quoted(&f.text),
            plural(f.k.records, "record"),
            plural(f.k.runs, "run"),
            minute_text(f.k.first.ms),
            line_ref(pid, f.k.first.run, f.k.first.line),
            minute_text(f.k.last.ms),
            line_ref(pid, f.k.last.run, f.k.last.line),
            plural(f.k.stops.len() as u64, "time"),
            plural(backs, "time")
        ));
        for j in 0..f.k.stops.len() {
            all.push(FixRow::Stop(i, j));
        }
        all.push(FixRow::Now(i));
    }
    let total = all.len();
    let shown = all.get(skip..total.min(skip.saturating_add(limit))).unwrap_or(&[]);
    // the lines the "now" rows show: each key's last-seen line
    let mut want: BTreeMap<(usize, u64), Vec<(u64, u64)>> = BTreeMap::new();
    for x in shown {
        if let FixRow::Now(i) = x {
            let f = &found[*i];
            want.entry((f.pi, f.k.latest.run)).or_default().push((f.k.latest.line, f.k.latest.line));
        }
    }
    let lines = Lines::read(&ps, &want);
    let mut rows = Vec::new();
    for x in shown {
        let mut row = Row::default();
        match *x {
            FixRow::Stop(i, j) => {
                let f = &found[i];
                row.section = i;
                let s = f.k.stops[j];
                stop_row(&ps, &runs_of, &mut cats, f, j + 1, &s, line_max, &mut row);
            }
            FixRow::Now(i) => {
                let f = &found[i];
                row.section = i;
                let pid = &ps[f.pi].pid;
                let l = f.k.latest;
                row.lines.push(match f.k.stops.last() {
                    Some(s) if s.back.is_none() => format!("now: not seen since it stopped in {} ({}); last seen {} at {}", run_ref(pid, s.run), minute_text(s.ms), minute_text(l.ms), line_ref(pid, l.run, l.line)),
                    _ => format!("now: last seen {} at {}; it has not stopped since (no later run of the same command or file without it)", minute_text(l.ms), line_ref(pid, l.run, l.line)),
                });
                let shown_line = lines.show(&ps, f.pi, l.run, l.line, line_max, &mut row);
                row.lines.push(format!("  {shown_line}"));
            }
        }
        rows.push(row);
    }
    for n in cats.notes() {
        ans.must(&n);
    }
    // a first row too long for the cap: its lines while they fit, then the call that shows it
    // whole (a stop can list 10 commits and 10 sessions)
    if let (Some(row), Some(x)) = (rows.first_mut(), shown.first()) {
        // (the row within its key's answer: a ref to the key's last record gives that key alone)
        let (k, pos) = match *x {
            FixRow::Stop(k, j) => (k, j),
            FixRow::Now(k) => (k, found[k].k.stops.len()),
        };
        let head = heads[k].len() + 1;
        let size: usize = row.lines.iter().map(|l| l.len() + 1).sum();
        if head + size >= ans.room() {
            let f = &found[k];
            let call = Json::o(vec![
                ("ref", line_ref(&ps[f.pi].pid, f.k.latest.run, f.k.latest.line).into()),
                ("cursor", format!("#{pos}").into()),
                ("limit", Json::from(1u64)),
                ("max_tokens", Json::from(20_000u64)),
            ]);
            let note = |n: usize| format!("  (+{} more lines: {})", group(n as u64), next_call("fixes", &call));
            // (the note is kept whole: the row's first line is cut to leave room for it, down to
            // its start, "stop 2 of 3: " or "now: ")
            let room = ans.room().saturating_sub(head + note(row.lines.len()).len() + 1);
            if row.lines[0].len() + 1 > room {
                let start = row.lines[0].find(": ").map_or(0, |p| p + 2);
                let (t, n) = crate::clean::cut(&row.lines[0], room.saturating_sub(20).max(start));
                row.lines[0] = format!("{t}…(+{} bytes)", group(n as u64));
            }
            let (mut keep, mut used) = (1, row.lines[0].len() + 1);
            while keep < row.lines.len() && used + row.lines[keep].len() < room {
                used += row.lines[keep].len() + 1;
                keep += 1;
            }
            if keep < row.lines.len() {
                let n = row.lines.len() - keep;
                row.lines.truncate(keep);
                row.lines.push(note(n));
            }
        }
    }
    let s = show_rows(&mut ans, &heads, &rows, skip, total, limit);
    let cuts = cut_note(s.cuts, s.cut_ref.as_deref(), line_max);
    let last = match s.next {
        None => format!("all shown{cuts}"),
        Some(i) => {
            let keys: HashSet<usize> = all[i..].iter().map(|x| match x {
                FixRow::Stop(k, _) | FixRow::Now(k) => *k,
            }).collect();
            format!(
                "left out: {} of {} rows (#{i} on: {}){cuts}. Next: {}",
                group((total - i) as u64),
                group(total as u64),
                plural(keys.len() as u64, "key"),
                next_call("fixes", &a.with_cursor(&format!("#{i}")))
            )
        }
    };
    Ok(ans.finish(&last))
}
