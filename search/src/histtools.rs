//! history: the start card and the questions about what happened before. A measurement on real
//! sessions found main sessions of hundreds of replies, each reply reading the whole of it again;
//! started afresh every 25 to 50 replies with a short card they would use 5.6 to 7.6 times fewer
//! tokens, if nothing is lost. The card says what is failing now and since when, what was fixed,
//! the last runs, and what the last agent session ran (and what of it failed) and edited, each
//! with its reference; `command`, `session` and `file` answer the follow-up questions from the
//! same memory: the runs of a command, what a session did, which sessions edited a file and
//! what ran after. All from the run table, the catalogs and the sessions' own lines.

use std::sync::atomic::AtomicBool;

use super::keytools::{Cats, minute_text};
use super::{Args, Ctx, plural, projects, short_commit, what};
use crate::answer::{Answer, group, run_ref};
use crate::formats::Level;
use crate::store::{self, Project, RunRec};

/// The newest runs the card and the questions look at.
const RECENT: usize = 300;
/// Sessions read for `file` (each is read whole).
const SESSIONS_READ: usize = 40;

pub(super) fn tool_history(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let home = ctx.home.clone()?;
    let projects = projects(ctx, &home, a.s("project"))?;
    if projects.len() != 1 {
        return Err("history looks at one project: give its id or root folder, not *".into());
    }
    let p = &projects[0];
    let mut runs: Vec<RunRec> = store::read_runs(&p.dir);
    runs.sort_by_key(|r| std::cmp::Reverse(r.id()));
    runs.truncate(RECENT);
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    let mut cats = Cats::new(stop);
    match (a.s("command"), a.s("session"), a.s("file")) {
        (Some(c), _, _) => command_history(p, &runs, c, &mut cats, &mut ans),
        (None, Some(s), _) => session_history(p, &runs, s, &mut ans)?,
        (None, None, Some(f)) => file_history(p, &runs, f, &mut ans),
        (None, None, None) => card(p, &runs, &mut cats, &mut ans),
    }
    Ok(ans.finish("all shown"))
}

/// A command run's line: "r40 2026-11-23 18:40 exit 101, 7 error records, 41 s".
fn run_line(p: &Project, r: &RunRec, cats: &mut Cats) -> String {
    let mut s = format!("{} {}", run_ref(&p.pid, r.id()), r.i("start").map(minute_text).unwrap_or_default());
    s.push_str(&format!(", {}", status(r)));
    if let Some(n) = errors_of(p, r, cats).filter(|&n| n > 0) {
        s.push_str(&format!(", {}", plural(n, "error record")));
    }
    if let (Some(a), Some(b)) = (r.i("start"), r.i("end")) {
        s.push_str(&format!(", {}", seconds(b - a)));
    }
    let c = short_commit(r);
    if !c.is_empty() && c != "?" {
        s.push_str(&format!(", {c}"));
    }
    s
}

/// "exit 101", "signal 9", "ok", or the state of a run not ended yet.
fn status(r: &RunRec) -> String {
    match (r.i("exit"), r.i("signal")) {
        (_, Some(sig)) => format!("signal {sig}"),
        (Some(0), _) => "ok".into(),
        (Some(e), _) => format!("exit {e}"),
        _ => r.state().to_string(),
    }
}

fn failed(r: &RunRec) -> bool {
    r.i("signal").is_some() || r.i("exit").is_some_and(|e| e != 0)
}

/// A run's error records (its catalog's keys at level error), when its catalog can be read.
fn errors_of(p: &Project, r: &RunRec, cats: &mut Cats) -> Option<u64> {
    cats.load(0, p, r).ok()?;
    Some(cats.cat(0, r.id())?.keys.iter().filter(|k| k.level == Level::Error).map(|k| k.records).sum())
}

fn seconds(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 120 { format!("{s} s") } else { format!("{} min", s / 60) }
}

fn cut(s: &str, max: usize) -> String {
    let (t, n) = crate::clean::cut(s, max);
    if n > 0 { format!("{t}…") } else { t.to_string() }
}

/// The start card (no input): what is failing now, what was fixed, the last runs, the last
/// session.
fn card(p: &Project, runs: &[RunRec], cats: &mut Cats, ans: &mut Answer) {
    // (a command run's `cmd` is its argument list)
    let commands: Vec<&RunRec> = runs.iter().filter(|r| r.kind() == "run" && r.0.get("cmd").is_some()).collect();
    let sessions = runs.iter().filter(|r| r.kind() == "session").count();
    let newest = runs.first().and_then(|r| r.i("start")).map(minute_text).unwrap_or_default();
    ans.must(&format!("history of {} (the newest {} runs): {} of commands, {} of sessions; the newest {newest}", p.pid, runs.len(), commands.len(), sessions));
    // each command's latest run, newest first: failing now, or passing again after a failure
    let mut seen: Vec<String> = Vec::new();
    let (mut failing, mut fixed) = (Vec::new(), Vec::new());
    for r in &commands {
        let cmd = what(r);
        if seen.contains(&cmd) {
            continue;
        }
        seen.push(cmd.clone());
        let older: Vec<&&RunRec> = commands.iter().filter(|x| what(x) == cmd && x.id() < r.id()).collect();
        if failed(r) {
            let since = older.iter().take_while(|x| failed(x)).last().map(|x| x.id()).unwrap_or(r.id());
            let passed = older.iter().find(|x| !failed(x));
            failing.push(format!(
                "  {}: {}{}{}",
                cut(&cmd, 60),
                run_line(p, r, cats),
                if since != r.id() { format!("; failing since {}", run_ref(&p.pid, since)) } else { String::new() },
                passed.map(|x| format!("; last passed {}", run_ref(&p.pid, x.id()))).unwrap_or_else(|| "; never passed here".into())
            ));
        } else if let Some(x) = older.first().filter(|x| failed(x)) {
            fixed.push(format!("  {}: ok at {} (failed at {})", cut(&cmd, 60), run_ref(&p.pid, r.id()), run_ref(&p.pid, x.id())));
        }
    }
    if !failing.is_empty() {
        ans.must(&format!("failing now ({}):", failing.len()));
        for l in failing.iter().take(5) {
            ans.must(l);
        }
    } else if !commands.is_empty() {
        ans.must("failing now: none (each command's last run passed)");
    }
    if !fixed.is_empty() {
        ans.must("passing again:");
        for l in fixed.iter().take(3) {
            ans.must(l);
        }
    }
    let last: Vec<String> = runs.iter().filter(|r| r.kind() != "session").take(5).map(|r| format!("{} {} {}", run_ref(&p.pid, r.id()), cut(&what(r), 40), status(r))).collect();
    if !last.is_empty() {
        ans.must(&format!("last runs: {}", last.join(" · ")));
    }
    if let Some(s) = runs.iter().find(|r| r.kind() == "session") {
        let pieces = session_pieces(runs, s);
        let f = facts_of(p, &pieces);
        ans.must(&session_line(p, &pieces, &f));
        let bad: Vec<String> = f.commands.iter().filter(|c| c.1).map(|c| cut(&c.0, 50)).collect();
        if !bad.is_empty() {
            ans.must(&format!("  failed in it: {}", bad.iter().take(4).cloned().collect::<Vec<_>>().join(" · ")));
        }
        if !f.files.is_empty() {
            ans.must(&format!("  edited: {}", f.files.iter().take(6).map(|x| short_name(x)).collect::<Vec<_>>().join(", ")));
        }
    }
    ans.must("more: history with command, session (\"last\" or its id) or file; errors, guard and fixes for the errors");
}

/// A session's pieces (runs of one session id), oldest first.
fn session_pieces<'r>(runs: &'r [RunRec], s: &RunRec) -> Vec<&'r RunRec> {
    let mut v: Vec<&RunRec> = match s.s("session") {
        Some(id) => runs.iter().filter(|r| r.kind() == "session" && r.s("session") == Some(id)).collect(),
        None => vec![runs.iter().find(|r| r.id() == s.id()).unwrap_or(&runs[0])],
    };
    v.sort_by_key(|r| r.id());
    v
}

/// What a session did, from its pieces' lines.
fn facts_of(p: &Project, pieces: &[&RunRec]) -> crate::view::Facts {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut text = Vec::new();
    for r in pieces {
        let n = r.i("lines").unwrap_or(0) as u64;
        if r.state() != "ready" || n == 0 {
            continue;
        }
        if let Ok(got) = crate::fetch::fetch(&p.dir.join(store::run_file(r.id())), &[(1, n)], threads) {
            for (_, l) in got {
                text.extend_from_slice(&l);
                if !l.ends_with(b"\n") {
                    text.push(b'\n');
                }
            }
        }
    }
    crate::view::facts(&text)
}

/// "last session: claude 5e55a1d0 (r36-r37), 2026-11-20 14:00 to 14:46, 23 replies: 9 commands
/// (3 failed), 3 files edited".
fn session_line(p: &Project, pieces: &[&RunRec], f: &crate::view::Facts) -> String {
    let first = pieces.first().map(|r| r.id()).unwrap_or(0);
    let last = pieces.last().map(|r| r.id()).unwrap_or(0);
    let refs = if first == last { run_ref(&p.pid, first) } else { format!("{}-r{last}", run_ref(&p.pid, first)) };
    let agent = pieces.first().and_then(|r| r.s("agent")).unwrap_or("agent");
    let id = pieces.first().and_then(|r| r.s("session")).map(|s| s.chars().take(8).collect::<String>()).unwrap_or_default();
    let when = match (f.first.as_deref(), f.last.as_deref()) {
        (Some(a), Some(b)) => {
            // (the end's date too when it is another day, here)
            let (a, b) = (local_minute(a), local_minute(b));
            let end = if a.get(..10) == b.get(..10) { b.get(11..16).unwrap_or(&b).to_string() } else { b.clone() };
            format!(", {a} to {end}")
        }
        _ => String::new(),
    };
    let failed = f.commands.iter().filter(|c| c.1).count();
    format!(
        "last session: {agent} {id} ({refs}){when}, {}: {}{}, {}",
        if f.replies == 1 { "1 reply".to_string() } else { format!("{} replies", group(f.replies)) },
        plural(f.commands.len() as u64, "command"),
        if failed > 0 { format!(" ({failed} failed)") } else { String::new() },
        if f.files.len() == 1 { "1 file edited".to_string() } else { format!("{} files edited", group(f.files.len() as u64)) }
    )
}

/// A session's time as answers show times, local ("2026-11-25 20:02"): Claude Code writes
/// them in UTC ("2026-11-26T03:02:08.123Z").
fn local_minute(t: &str) -> String {
    let clock = crate::times::Clock::local(crate::times::now_ms());
    match crate::times::text_time(t.as_bytes(), &clock).and_then(|s| s.utc_ms()) {
        Some(ms) => minute_text(ms),
        None => t.get(..16).unwrap_or(t).replace('T', " "),
    }
}

/// A file's folder and name.
fn short_name(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// `command`: the runs of commands holding a text, newest first.
fn command_history(p: &Project, runs: &[RunRec], text: &str, cats: &mut Cats, ans: &mut Answer) {
    let mine: Vec<&RunRec> = runs.iter().filter(|r| r.kind() == "run" && r.0.get("cmd").is_some() && what(r).contains(text)).collect();
    let bad = mine.iter().filter(|r| failed(r)).count();
    ans.must(&format!("{} of commands holding {:?} (the newest first): {} failed", plural(mine.len() as u64, "run"), text, group(bad as u64)));
    for r in mine.iter().take(12) {
        if !ans.line(&format!("  {} {}", run_line(p, r, cats), cut(&what(r), 60))) {
            break;
        }
    }
    if mine.len() > 12 {
        ans.must(&format!("  and {} older (logs with command shows them all)", group(mine.len() as u64 - 12)));
    }
}

/// `session`: what a session did ("last", or its id or the start of it).
fn session_history(p: &Project, runs: &[RunRec], which: &str, ans: &mut Answer) -> Result<(), String> {
    let s = runs
        .iter()
        .find(|r| r.kind() == "session" && (which == "last" || r.s("session").is_some_and(|id| id.starts_with(which))))
        .ok_or_else(|| format!("no session {which:?} among the newest {} runs", runs.len()))?;
    let pieces = session_pieces(runs, s);
    let f = facts_of(p, &pieces);
    ans.must(&session_line(p, &pieces, &f).replacen("last session: ", "session: ", 1));
    if !f.commands.is_empty() {
        ans.must("commands, in order:");
        for (c, bad) in f.commands.iter().take(25) {
            if !ans.line(&format!("  {} {}", if *bad { "failed" } else { "ran" }, cut(c, 100))) {
                break;
            }
        }
        if f.commands.len() > 25 {
            ans.must(&format!("  and {} more (search the session's runs for them)", group(f.commands.len() as u64 - 25)));
        }
    }
    if !f.files.is_empty() {
        ans.must(&format!("edited: {}", f.files.iter().map(|x| short_name(x)).collect::<Vec<_>>().join(", ")));
    }
    Ok(())
}

/// `file`: the sessions that edited a file (a path ending in the text), newest first, and the
/// command runs right after each.
fn file_history(p: &Project, runs: &[RunRec], text: &str, ans: &mut Answer) {
    let mut seen: Vec<String> = Vec::new();
    let mut hits = 0;
    let mut read = 0;
    for s in runs.iter().filter(|r| r.kind() == "session") {
        let id = s.s("session").unwrap_or("").to_string();
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        read += 1;
        if read > SESSIONS_READ {
            break;
        }
        let pieces = session_pieces(runs, s);
        let f = facts_of(p, &pieces);
        let Some(path) = f.files.iter().find(|x| x.ends_with(text) || x.contains(text)) else { continue };
        hits += 1;
        let line = session_line(p, &pieces, &f).replacen("last session: ", "", 1);
        let end = pieces.last().map(|r| r.id()).unwrap_or(0);
        let after: Vec<String> = runs.iter().rev().filter(|r| r.kind() == "run" && r.id() > end).take(3).map(|r| format!("{} {} {}", run_ref(&p.pid, r.id()), cut(&what(r), 30), status(r))).collect();
        ans.must(&format!("{}: {line}", short_name(path)));
        if !after.is_empty() {
            ans.must(&format!("  then: {}", after.join(" · ")));
        }
    }
    if hits == 0 {
        ans.must(&format!("no session of the newest {} edited a file holding {:?}", group(read.min(SESSIONS_READ) as u64), text));
    }
}
