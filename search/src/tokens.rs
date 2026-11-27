//! `fewer tokens [--here] [--split TIME]`: how many tokens Claude Code spent, read from fewer's
//! own saved sessions (`sessions = on`), never from Claude Code's folder; numbers only. Each
//! reply is counted once (by its message id): without fewer or with it, by how its session
//! ran (Mode: Claude Code names the agent a session starts with, and fewer's is fewer's; a
//! session that names none, by when it started, as fewer's periods in the store's config say:
//! `savers.on`, which install sets); or each reply by its own time, before and after a time given
//! with --split (UTC); with neither, all in one column. Per side: sessions, replies
//! (and the share in helpers), tokens (input + cache writes + cache reads) and per reply, output
//! (thinking included), a session's first reply (its fixed start, the median), compactions,
//! workflow runs; the table shows chats, replies, tokens per reply and the first reply.
//! A session's helpers are pieces of it (sessions.rs), told apart by isSidechain. fewer's own runs
//! (helper, workflow, look, web: Claude Code saves no session of them) come from the store's
//! helpers.jsonl, each counted as a helper with the session that started it, on its side at the
//! run's time: its turns as replies, its tokens.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;

use crate::answer::group;
use crate::config;
use crate::json::{self, Json};
use crate::store::{self, Project, RunRec};

const USAGE: &str = "usage: fewer tokens [--here] [--split YYYY-MM-DD[THH:MM]]";

/// One side of the report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub sessions: u64,
    pub replies: u64,
    pub helper_replies: u64,
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    pub helper_tokens: u64,
    /// each session's first reply in its main thread (the start it re-reads every reply)
    pub starts: Vec<u64>,
    pub compactions: u64,
    pub workflows: u64,
}

impl Totals {
    pub fn tokens(&self) -> u64 {
        self.input + self.cache_write + self.cache_read
    }

    fn per_reply(&self, n: f64) -> f64 {
        n / self.replies.max(1) as f64
    }
}

/// fewer's periods: (from, until; None while it is on), as UTC times.
pub type Periods = Vec<(String, Option<String>)>;

/// Whether a UTC time (a transcript's, with its Z, or helpers.jsonl's) falls in one of fewer's
/// periods: from its start until it was turned off again (compared as text).
fn in_periods(t: &str, periods: &[(String, Option<String>)]) -> bool {
    periods.iter().any(|(s, e)| t >= s.as_str() && e.as_deref().is_none_or(|e| t < e))
}

/// fewer's own runs (helpers.jsonl, one line a run) added to `before` or `after`, as helpers'
/// replies (their turns) and tokens, as the Agent tool's helpers count in a session; a workflow
/// run is a workflow run. A run counts together with the session that started it: its `parent` (a
/// run started from a run's session: that run's parent, up to a session), only when that session
/// is among `counted` (the sessions counted already, this project's with --here: runs come and go
/// with their sessions), and on that session's side at the run's time (its Mode). A run Claude
/// Code saved as a session of its own (started outside it) is counted as that session, not again
/// here. A line without a parent (before 2026-11-26, or started outside any session) counts by
/// its own time, with every project, and with --here (`root`) when its folder's git root is the
/// project's root. A line without its input split counts all of it as input. (A workflow run's
/// agents count in its tokens, not in its replies: its result counts its own steps only.)
pub fn add_runs(text: &[u8], periods: &[(String, Option<String>)], root: Option<&std::path::Path>, counted: &HashMap<String, Mode>, before: &mut Totals, after: &mut Totals) {
    let lines: Vec<Json> = text.split(|&c| c == b'\n').filter(|l| !l.trim_ascii().is_empty()).filter_map(|l| json::parse(l).ok()).collect();
    let s = |o: &Json, k: &str| o.get(k).and_then(Json::str).unwrap_or("").to_string();
    // (each run's own session, and the session that started it)
    let up: std::collections::HashMap<String, String> = lines.iter().map(|o| (s(o, "session"), s(o, "parent"))).filter(|(own, _)| !own.is_empty()).collect();
    let mut roots: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for o in &lines {
        let Some(t) = o.get("time").and_then(Json::str) else { continue };
        let own = s(o, "session");
        if !own.is_empty() && counted.contains_key(&own) {
            continue;
        }
        let mut parent = s(o, "parent");
        for _ in 0..16 {
            match up.get(&parent) {
                Some(p) if !p.is_empty() && *p != parent => parent = p.clone(),
                _ => break,
            }
        }
        let late = if !parent.is_empty() {
            match counted.get(&parent) {
                Some(mode) => mode.with_at(t, periods),
                None => continue,
            }
        } else {
            let here = match root {
                None => true,
                Some(root) => {
                    let cwd = s(o, "cwd");
                    *roots.entry(cwd.clone()).or_insert_with(|| !cwd.is_empty() && crate::sessions::git_root(&cwd) == root)
                }
            };
            if !here {
                continue;
            }
            in_periods(t, periods)
        };
        let side = if late { &mut *after } else { &mut *before };
        let n = |k: &str| o.get(k).and_then(Json::int).unwrap_or(0).max(0) as u64;
        let (tokens, turns) = (n("tokens"), n("turns"));
        let (i, w, r) = (n("input"), n("cache_write"), n("cache_read"));
        if i + w + r == tokens {
            side.input += i;
            side.cache_write += w;
            side.cache_read += r;
        } else {
            side.input += tokens;
        }
        side.output += n("output");
        side.replies += turns;
        side.helper_replies += turns;
        side.helper_tokens += tokens;
        if o.get("kind").and_then(Json::str) == Some("workflow") {
            side.workflows += 1;
        }
    }
}

/// Whether an agent Claude Code names in a session's `agent-setting` line is one of fewer's (its
/// lean agent, or a helper's or workflow agent's): the session runs with fewer on from there.
fn fewers_agent(agent: &str) -> bool {
    agent.starts_with("fewer-")
}

/// How a saved session's replies (and the runs it started) are sided.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// each by its own time (--split TIME)
    Time,
    /// by how the session ran: from each switch's time on, with fewer or not. Claude Code writes
    /// an `agent-setting` line when a session starts with an agent (2.1.287); a switch's time is
    /// the time of the first line after it.
    Switches(Vec<(String, bool)>),
    /// a session with no `agent-setting` line (none set, or an older Claude Code): all of it on
    /// the side of when it started (fewer applies when a session starts)
    Start(bool),
}

impl Mode {
    /// Whether something of this session at time `t` counts with fewer.
    pub fn with_at(&self, t: &str, periods: &[(String, Option<String>)]) -> bool {
        match self {
            Mode::Time => in_periods(t, periods),
            Mode::Switches(v) => v.iter().rev().find(|(at, _)| at.as_str() <= t).is_some_and(|(_, lean)| *lean),
            Mode::Start(with) => *with,
        }
    }
}

/// Add one saved session (Claude Code's transcript lines, its helpers' pieces included) to
/// `before` (without fewer) or `after` (with it). `by_mode`: by how the session ran (Mode),
/// else each reply by its own time (--split). Its Mode, for the runs it started.
pub fn add_session(text: &[u8], periods: &[(String, Option<String>)], by_mode: bool, before: &mut Totals, after: &mut Totals) -> Mode {
    enum Ev {
        Compaction,
        Workflows(u64),
        Reply { id: String, side: bool, i: u64, w: u64, r: u64, out: u64 },
    }
    // (one pass: the events with their times; the switches and the session's start)
    let mut events: Vec<(String, Ev)> = Vec::new();
    let (mut switches, mut pending, mut start): (Vec<(String, bool)>, Option<bool>, Option<String>) = (Vec::new(), None, None);
    for line in text.split(|&c| c == b'\n') {
        if line.trim_ascii().is_empty() {
            continue;
        }
        let Ok(o) = json::parse(line) else { continue };
        let kind = o.get("type").and_then(Json::str);
        if kind == Some("agent-setting") {
            pending = Some(o.get("agentSetting").and_then(Json::str).is_some_and(fewers_agent));
            continue;
        }
        let ts = o.get("timestamp").and_then(Json::str).unwrap_or("").to_string();
        if !ts.is_empty() {
            if start.as_deref().is_none_or(|s| ts.as_str() < s) {
                start = Some(ts.clone());
            }
            if let Some(lean) = pending.take() {
                switches.push((ts.clone(), lean));
            }
        }
        match kind {
            Some("user") if o.get("isCompactSummary").and_then(Json::bool) == Some(true) => events.push((ts, Ev::Compaction)),
            Some("assistant") => {
                let side = o.get("isSidechain").and_then(Json::bool) == Some(true);
                let Some(m) = o.get("message") else { continue };
                if !side && let Some(Json::Arr(content)) = m.get("content") {
                    let n = content.iter().filter(|p| p.get("type").and_then(Json::str) == Some("tool_use") && p.get("name").and_then(Json::str) == Some("Workflow")).count() as u64;
                    if n > 0 {
                        events.push((ts.clone(), Ev::Workflows(n)));
                    }
                }
                let (Some(id), Some(u)) = (m.get("id").and_then(Json::str), m.get("usage")) else { continue };
                let n = |k: &str| u.get(k).and_then(Json::int).unwrap_or(0).max(0) as u64;
                events.push((ts, Ev::Reply { id: id.to_string(), side, i: n("input_tokens"), w: n("cache_creation_input_tokens"), r: n("cache_read_input_tokens"), out: n("output_tokens") }));
            }
            _ => {}
        }
    }
    switches.sort();
    let mode = if !by_mode {
        Mode::Time
    } else if !switches.is_empty() {
        Mode::Switches(switches)
    } else {
        Mode::Start(start.as_deref().is_some_and(|t| in_periods(t, periods)))
    };
    let mut seen = HashSet::new();
    let (mut in_before, mut in_after, mut started) = (false, false, false);
    for (ts, ev) in events {
        let late = mode.with_at(&ts, periods);
        let t = if late { &mut *after } else { &mut *before };
        match ev {
            Ev::Compaction => t.compactions += 1,
            Ev::Workflows(n) => t.workflows += n,
            Ev::Reply { id, side, i, w, r, out } => {
                // (each reply once by its message id: one line a content block)
                if !seen.insert(id) {
                    continue;
                }
                t.replies += 1;
                t.input += i;
                t.cache_write += w;
                t.cache_read += r;
                t.output += out;
                if side {
                    t.helper_replies += 1;
                    t.helper_tokens += i + w + r;
                } else if !started {
                    started = true;
                    t.starts.push(i + w + r);
                }
                if late {
                    in_after = true;
                } else {
                    in_before = true;
                }
            }
        }
    }
    before.sessions += u64::from(in_before);
    after.sessions += u64::from(in_after);
    mode
}

/// A time given as YYYY-MM-DD, YYYY-MM-DDTHH:MM or with seconds (a space for the T too), as
/// the text transcripts' UTC times compare with; None when it is not one.
pub fn split_time(s: &str) -> Option<String> {
    let t = s.trim().replacen(' ', "T", 1);
    let b = t.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r.clone()).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    let date = b.len() >= 10 && digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10);
    let rest_ok = match b.len() {
        10 => true,
        16 => b[10] == b'T' && digits(11..13) && b[13] == b':' && digits(14..16),
        19 => b[10] == b'T' && digits(11..13) && b[13] == b':' && digits(14..16) && b[16] == b':' && digits(17..19),
        _ => false,
    };
    (date && rest_ok).then_some(t)
}

/// The token card: a few lines on top of the table, for people (and for a screenshot): how much
/// smaller a reply is since fewer went on, and about how many tokens that saved (the replies
/// since, times what each reads less); else what is known so far. `saving`: whether Claude Code's
/// chats are being saved now (when not, the card says so: it does not grow).
pub fn card(scope: &str, periods: &[(String, Option<String>)], before: &Totals, after: &Totals, saving: bool) -> String {
    let mut out = format!("fewer: your Claude Code tokens, {scope}\n");
    match periods.first().map(|(s, _)| s.as_str()) {
        Some(when) if before.replies > 0 && after.replies > 0 => {
            let (b, a) = (before.per_reply(before.tokens() as f64), after.per_reply(after.tokens() as f64));
            out.push_str(&format!("  {} tokens per reply since {}: {} now, {} before\n", percent_fewer(b, a), day_words(when), group(a.round() as u64), group(b.round() as u64)));
            let saved = ((b - a).max(0.0) * after.replies as f64).round() as u64;
            out.push_str(&format!("  about {} tokens saved, in {}\n", human(saved), replies_word(after.replies)));
        }
        Some(when) => {
            out.push_str(&format!("  fewer on since {}: not enough replies yet to compare ({} before, {} with fewer)\n", day_words(when), group(before.replies), group(after.replies)));
        }
        None => {
            out.push_str(&format!("  {}, {} tokens; to compare, turn fewer on: fewer install\n", replies_word(before.replies), human(before.tokens())));
        }
    }
    if let Some((_, Some(off))) = periods.last() {
        out.push_str(&format!("  fewer is off since {}: fewer on turns it on again\n", day_words(off)));
    }
    if !saving {
        out.push_str("  your Claude Code chats are not being saved now (fewer memory sessions on), so this does not grow\n");
    }
    out
}

/// The table under the card: chats, replies, tokens per reply and a chat's first reply, before
/// fewer and with it (`by_fewer`; else before and after the time given), or all of them.
pub fn report(split: Option<&str>, by_fewer: bool, before: &Totals, after: &Totals) -> String {
    let median = |v: &[u64]| -> String {
        if v.is_empty() {
            return "-".into();
        }
        let mut v = v.to_vec();
        v.sort_unstable();
        group(v[v.len() / 2])
    };
    let cols: Vec<(&str, &Totals)> = match (split, by_fewer) {
        (Some(_), true) => vec![("before fewer", before), ("with fewer", after)],
        (Some(_), false) => vec![("before", before), ("after", after)],
        (None, _) => vec![("all", before)],
    };
    type Cell<'a> = Box<dyn Fn(&Totals) -> String + 'a>;
    let rows: [(&str, Cell); 4] = [
        ("chats", Box::new(|t: &Totals| group(t.sessions))),
        ("replies", Box::new(|t: &Totals| group(t.replies))),
        ("tokens per reply", Box::new(|t: &Totals| group(t.per_reply(t.tokens() as f64).round() as u64))),
        ("a chat's first reply", Box::new(move |t: &Totals| median(&t.starts))),
    ];
    let w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let mut out = format!("{:w$}", "");
    for (name, _) in &cols {
        out.push_str(&format!("  {name:>14}"));
    }
    out.push('\n');
    for (name, f) in &rows {
        out.push_str(&format!("{name:w$}"));
        for (_, t) in &cols {
            out.push_str(&format!("  {:>14}", f(t)));
        }
        out.push('\n');
    }
    out.push_str("\nCounted on this computer, from your own Claude Code chats.\n");
    out
}

/// A UTC time as the store's config writes it, as a local day people read: "Nov 26", with its
/// year when it is not this year's ("Dec 30, 2025").
pub fn day_words(utc: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = crate::times::local_of_utc(utc);
    let (y, mo, d, h, mi, sec) = crate::times::civil(crate::times::now_ms().div_euclid(1000));
    let this_year = crate::times::local_of_utc(&format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}")).get(..4).unwrap_or("").to_string();
    let p: Vec<u32> = local.get(..10).unwrap_or("").split('-').filter_map(|x| x.parse().ok()).collect();
    match p.as_slice() {
        [y, m @ 1..=12, d] if y.to_string() == this_year => format!("{} {d}", MONTHS[*m as usize - 1]),
        [y, m @ 1..=12, d] => format!("{} {d}, {y}", MONTHS[*m as usize - 1]),
        _ => local,
    }
}

/// A count of tokens in short: 9,870, 870K, 3.1M, 1.2B.
pub fn human(n: u64) -> String {
    let one = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    match n {
        0..=9_999 => group(n),
        10_000..=999_499 => format!("{}K", (n as f64 / 1e3).round() as u64),
        999_500..=999_949_999 => one(n as f64 / 1e6, "M"),
        _ => one(n as f64 / 1e9, "B"),
    }
}

fn replies_word(n: u64) -> String {
    if n == 1 { "1 reply".into() } else { format!("{} replies", group(n)) }
}

/// The saved Claude Code sessions of these projects, each read whole, added up (`by_mode`: as
/// add_session); and each session's Mode by its id.
fn gather(projects: &[Project], periods: &[(String, Option<String>)], by_mode: bool) -> (Totals, Totals, HashMap<String, Mode>) {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let (mut before, mut after, mut modes) = (Totals::default(), Totals::default(), HashMap::new());
    for p in projects {
        let runs = store::read_runs(&p.dir);
        let mut sessions: BTreeMap<&str, Vec<&RunRec>> = BTreeMap::new();
        for r in runs.iter().filter(|r| r.kind() == "session" && r.s("agent") == Some("claude") && r.state() == "ready") {
            sessions.entry(r.s("session").unwrap_or("?")).or_default().push(r);
        }
        for pieces in sessions.values_mut() {
            pieces.sort_by_key(|r| r.id());
            let mut text = Vec::new();
            for r in pieces.iter() {
                let n = r.i("lines").unwrap_or(0).max(0) as u64;
                if n == 0 {
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
            let mode = add_session(&text, periods, by_mode, &mut before, &mut after);
            if let Some(id) = pieces.first().and_then(|r| r.s("session")) {
                modes.insert(id.to_string(), mode);
            }
        }
    }
    (before, after, modes)
}

/// fewer's periods from the store's config: each one it was turned off after
/// (`savers.periods`: FROM..UNTIL,...), and the last one (`savers.on`, until `savers.off` when it
/// is off: from a version that kept no periods), in time order.
pub fn periods_of(cfg: &[(String, String)]) -> Periods {
    let mut out: Periods = config::get(cfg, "savers.periods")
        .unwrap_or("")
        .split(',')
        .filter_map(|p| {
            let (a, b) = p.split_once("..")?;
            Some((split_time(a)?, Some(split_time(b)?)))
        })
        .collect();
    if let Some(on) = config::get(cfg, "savers.on").and_then(split_time) {
        let off = config::get(cfg, "savers.off").filter(|v| !v.is_empty()).and_then(split_time).filter(|e| e.as_str() > on.as_str());
        // (off now: that period is among the recorded ones already, unless an earlier version
        // turned it off)
        if off.is_none() || !out.iter().any(|(s, _)| *s == on) {
            out.push((on, off));
        }
    }
    out.sort();
    out
}

pub fn cmd_tokens(args: Vec<OsString>) -> i32 {
    let (mut here, mut split) = (false, None);
    let mut it = args.iter().map(|a| a.to_str());
    while let Some(a) = it.next() {
        match a {
            Some("--here") => here = true,
            // (the earlier spelling: everything is the default now)
            Some("--all") => here = false,
            Some("--split") => match it.next().flatten().and_then(split_time) {
                Some(t) => split = Some(t),
                None => {
                    eprintln!("fewer tokens: --split takes a UTC time: YYYY-MM-DD or YYYY-MM-DDTHH:MM\n{USAGE}");
                    return 2;
                }
            },
            Some("-h" | "--help") => {
                println!("{USAGE}\n\nHow many tokens Claude Code used per reply before fewer and with it, and about how\nmany fewer saved: counted on this computer from your saved Claude Code chats (and fewer's\nown helper and web runs; only their token counts are read). All your projects, or this one with --here; --split\nTIME compares before and after a time of your own (UTC, YYYY-MM-DD or YYYY-MM-DDTHH:MM).");
                return 0;
            }
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fewer tokens: {e}");
            return 2;
        }
    };
    // (fewer's periods: from the store's config, or one from --split on)
    let cfg = config::read(&home);
    // (by how each session ran, unless a time to split at is given)
    let by_mode = split.is_none();
    let periods: Periods = match &split {
        Some(s) => vec![(s.clone(), None)],
        None => periods_of(&cfg),
    };
    let split = periods.first().map(|(s, _)| s.clone());
    // (Claude Code's newest chats saved first, when they are saved: the card counts them too)
    let saving = crate::sessions::agents_on(&cfg, &mut Vec::new()).contains(&"claude");
    if saving {
        crate::sessions::sweep(&home, &crate::sessions::Places::from_env(), true, &std::sync::atomic::AtomicBool::new(false));
    }
    let (projects, scope, root) = if !here {
        (store::list_projects(&home), "all projects".to_string(), None)
    } else {
        let ctx = crate::tools::Ctx::new();
        let pid = store::pid_for(&home, &ctx.root).0;
        let p = store::read_project(&home.join("projects").join(&pid));
        (p.into_iter().collect(), format!("this project ({pid})"), Some(ctx.root))
    };
    let (mut before, mut after, modes) = gather(&projects, &periods, by_mode);
    // (fewer's own runs: helpers, workflows, look and web, whose sessions Claude Code does not save)
    if let Ok(text) = std::fs::read(home.join("helpers.jsonl")) {
        add_runs(&text, &periods, root.as_deref(), &modes, &mut before, &mut after);
    }
    if before.sessions + after.sessions == 0 {
        // (no saved session: a short note, not a table of zeros (fewer's own runs count only with
        // their sessions); sessions on already: none saved yet, say that)
        let on = saving;
        let how = if on {
            "use Claude Code as usual, then look again\n  (`fewer sessions sync` saves its newest chats now)"
        } else {
            "`fewer install` saves Claude Code's chats\n  (or `fewer memory sessions on`), and this card counts them"
        };
        println!("fewer: your Claude Code tokens, {scope}\n  no saved Claude Code chat{}: {how}", if on { " yet" } else { "" });
        return 0;
    }
    print!("{}\n{}", card(&scope, &periods, &before, &after, saving), report(split.as_deref(), by_mode, &before, &after));
    0
}

/// How much smaller `now` is than `before`, as people read it (the card, the post): "97% fewer"
/// in whole percent, one decimal near 100 (never "100%" while something is left), "12% more"
/// when it grew.
pub fn percent_fewer(before: f64, now: f64) -> String {
    format!("{}%{}", percent_number(before, now), if now > before && before > 0.0 { " more" } else { " fewer" })
}

/// The number alone of `percent_fewer` (the card's big number): "97", "99.6", "12" (for 12% more).
pub fn percent_number(before: f64, now: f64) -> String {
    if before <= 0.0 {
        return "0".to_string();
    }
    let p = 100.0 * (1.0 - now / before);
    if p < 0.0 {
        return format!("{:.0}", -p);
    }
    if p.round() >= 100.0 && now > 0.0 { format!("{:.1}", p.min(99.9)) } else { format!("{:.0}", p.round()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn reply(id: &str, ts: &str, side: bool, i: u64, w: u64, r: u64, out: u64, workflow: bool) -> String {
        let tool = if workflow { r#",{"type":"tool_use","id":"t","name":"Workflow","input":{}}"# } else { "" };
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","isSidechain":{side},"message":{{"id":"{id}","content":[{{"type":"text","text":"x"}}{tool}],"usage":{{"input_tokens":{i},"cache_creation_input_tokens":{w},"cache_read_input_tokens":{r},"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn replies_counted_once_at_their_own_time() {
        let lines = [
            r#"{"type":"user","timestamp":"2026-11-20T10:00:00Z","message":{"content":"go"}}"#.to_string(),
            reply("m1", "2026-11-20T10:00:01.5Z", false, 10, 40_000, 0, 300, false),
            // the same message again (one line per content block): counted once
            reply("m1", "2026-11-20T10:00:01.5Z", false, 10, 40_000, 0, 300, true),
            reply("h1", "2026-11-20T10:00:02Z", true, 5, 30_000, 0, 100, false),
            reply("m2", "2026-11-20T10:00:03Z", false, 1, 500, 40_010, 200, false),
            r#"{"type":"user","timestamp":"2026-11-24T09:00:00Z","isCompactSummary":true,"message":{"content":"summary"}}"#.to_string(),
            reply("m3", "2026-11-24T09:00:01Z", false, 2, 1_000, 30_000, 50, false),
            "not json".to_string(),
            String::new(),
        ];
        let text = lines.join("\n");
        let (mut b, mut a) = (Totals::default(), Totals::default());
        let from = |s: &str, e: Option<&str>| vec![(s.to_string(), e.map(str::to_string))];
        assert_eq!(add_session(text.as_bytes(), &from("2026-11-23", None), false, &mut b, &mut a), Mode::Time);
        assert_eq!(b, Totals { sessions: 1, replies: 3, helper_replies: 1, input: 16, cache_write: 70_500, cache_read: 40_010, output: 600, helper_tokens: 30_005, starts: vec![40_010], compactions: 0, workflows: 1 });
        assert_eq!(a, Totals { sessions: 1, replies: 1, input: 2, cache_write: 1_000, cache_read: 30_000, output: 50, compactions: 1, ..Totals::default() });
        // no split: one column
        let (mut all, mut none) = (Totals::default(), Totals::default());
        add_session(text.as_bytes(), &[], false, &mut all, &mut none);
        // fewer turned off again: the replies after it count as before
        let (mut b2, mut a2) = (Totals::default(), Totals::default());
        add_session(text.as_bytes(), &from("2026-11-23", Some("2026-11-23T00:00:01")), false, &mut b2, &mut a2);
        assert_eq!(a2.replies, 0, "{a2:?}");
        let off = card("all projects", &from("2026-11-23", Some("2026-11-24")), &b2, &a2, true);
        assert!(off.contains(&format!("  fewer is off since {}: fewer on turns it on again\n", day_words("2026-11-24"))) && off.contains("not enough replies yet to compare"), "{off}");
        // on, off, on again: each period's replies are fewer's
        let (mut b3, mut a3) = (Totals::default(), Totals::default());
        let two = vec![("2026-11-20T10:00:01".to_string(), Some("2026-11-20T10:00:02.5".to_string())), ("2026-11-24".to_string(), None)];
        add_session(text.as_bytes(), &two, false, &mut b3, &mut a3);
        assert_eq!((b3.replies, a3.replies), (2, 2), "{b3:?} {a3:?}");
        assert!(card("all projects", &two, &b3, &a3, true).contains(" tokens per reply since "), "{}", card("all projects", &two, &b3, &a3, true));
        let cfg = vec![("savers.periods".to_string(), "2026-11-20T10:00:01..2026-11-21T00:00:00".to_string()), ("savers.on".to_string(), "2026-11-22T00:00:00".to_string()), ("savers.off".to_string(), String::new())];
        assert_eq!(periods_of(&cfg), vec![("2026-11-20T10:00:01".to_string(), Some("2026-11-21T00:00:00".to_string())), ("2026-11-22T00:00:00".to_string(), None)]);
        // (an install from before the periods: on and off alone)
        let old = vec![("savers.on".to_string(), "2026-11-20T10:00:01".to_string()), ("savers.off".to_string(), "2026-11-21T00:00:00".to_string())];
        assert_eq!(periods_of(&old), vec![("2026-11-20T10:00:01".to_string(), Some("2026-11-21T00:00:00".to_string()))]);
        assert_eq!((all.replies, all.compactions, none.replies), (4, 1, 0));
        let c = card("all projects", &from("2026-11-23", None), &b, &a, true);
        assert!(!c.contains("not being saved") && card("all projects", &from("2026-11-23", None), &b, &a, false).ends_with("  your Claude Code chats are not being saved now (fewer memory sessions on), so this does not grow\n"));
        assert!(c.starts_with("fewer: your Claude Code tokens, all projects\n  ") && c.contains(&format!(" tokens per reply since {}: ", day_words("2026-11-23"))) && c.contains(" tokens saved, in 1 reply\n"), "{c}");
        assert!(card("all projects", &[], &b, &Totals::default(), true).contains("3 replies, 111K tokens; to compare, turn fewer on"));
        let r = report(Some("2026-11-23"), true, &b, &a);
        assert!(r.lines().next().is_some_and(|l| l.trim().starts_with("before fewer") && l.trim().ends_with("with fewer")) && !r.contains("UTC") && !r.contains("API"), "{r}");
        assert!(r.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == ["replies", "3", "1"]), "{r}");
        assert!(r.ends_with("\nCounted on this computer, from your own Claude Code chats.\n"), "{r}");
        assert!(report(Some("2026-11-23"), false, &b, &a).lines().next().is_some_and(|l| l.split_whitespace().collect::<Vec<_>>() == ["before", "after"]));
    }

    #[test]
    fn fewers_own_runs_count_with_the_sessions_that_started_them() {
        let lines = [
            r#"{"time":"2026-11-22T11:00:00","kind":"web","ok":true,"turns":2,"tokens":9000,"output":300,"cwd":"/p","input":1000,"cache_write":3000,"cache_read":5000,"session":"w-1","parent":"main"}"#,
            r#"{"time":"2026-11-24T09:40:00","kind":"helper","turns":3,"tokens":12000,"output":500,"cwd":"/p","session":"h-outer","parent":"main"}"#,
            // (a helper started from a helper: up to the session that started the first one)
            r#"{"time":"2026-11-24T09:42:00","kind":"helper","turns":1,"tokens":4000,"output":50,"cwd":"/p","session":"h-inner","parent":"h-outer"}"#,
            // (a line from before parents and the input split: by its folder; all of it as input)
            r#"{"time":"2026-11-24T09:45:00","kind":"workflow","ok":true,"turns":1,"tokens":20000,"output":800,"cwd":"/p"}"#,
            r#"{"time":"2026-11-24T09:50:00","kind":"look","turns":2,"tokens":7000,"output":200,"cwd":"/elsewhere"}"#,
            // (started from a session that is not saved, or forgotten: not counted)
            r#"{"time":"2026-11-24T09:52:00","kind":"helper","turns":4,"tokens":8000,"output":90,"cwd":"/p","session":"h-2","parent":"gone"}"#,
            // (saved by Claude Code as a session of its own: counted as that session)
            r#"{"time":"2026-11-24T09:55:00","kind":"helper","turns":5,"tokens":99999,"output":1,"cwd":"/p","session":"own"}"#,
            "not json",
            "",
        ]
        .join("\n");
        let periods = vec![("2026-11-23T18:00:00".to_string(), None)];
        let counted: HashMap<String, Mode> = [("main".to_string(), Mode::Time), ("own".to_string(), Mode::Time)].into();
        let (mut b, mut a) = (Totals::default(), Totals::default());
        add_runs(lines.as_bytes(), &periods, None, &counted, &mut b, &mut a);
        assert_eq!(b, Totals { replies: 2, helper_replies: 2, input: 1_000, cache_write: 3_000, cache_read: 5_000, output: 300, helper_tokens: 9_000, ..Totals::default() });
        assert_eq!(a, Totals { replies: 7, helper_replies: 7, input: 43_000, output: 1_550, helper_tokens: 43_000, workflows: 1, ..Totals::default() });
        // --here: the runs of this project's sessions; a line without a parent when its folder's
        // git root is the project's (made-up folders: their own roots)
        let (mut b, mut a) = (Totals::default(), Totals::default());
        add_runs(lines.as_bytes(), &periods, Some(std::path::Path::new("/p")), &counted, &mut b, &mut a);
        assert_eq!((b.replies, a.replies, a.helper_tokens), (2, 5, 36_000));
        // another project's sessions: none of these runs but the one without a parent in its folder
        let (mut b, mut a) = (Totals::default(), Totals::default());
        add_runs(lines.as_bytes(), &periods, Some(std::path::Path::new("/elsewhere")), &HashMap::new(), &mut b, &mut a);
        assert_eq!((b.replies, a.replies, a.helper_tokens), (0, 2, 7_000));
        // no session saved at all: only lines without a parent count (every project)
        let (mut b, mut a) = (Totals::default(), Totals::default());
        add_runs(lines.as_bytes(), &periods, None, &HashMap::new(), &mut b, &mut a);
        assert_eq!((b.replies, a.replies), (0, 3 + 5));
        // a run goes on its session's side at the run's time: the session ran without fewer
        // until 2026-11-24T09:41, then with it (all the runs of "main" are after fewer's start)
        let switched: HashMap<String, Mode> = [("main".to_string(), Mode::Switches(vec![("2026-11-24T09:41:00".to_string(), true)]))].into();
        let (mut b, mut a) = (Totals::default(), Totals::default());
        add_runs(lines.as_bytes(), &periods, Some(std::path::Path::new("/nowhere")), &switched, &mut b, &mut a);
        // (web 2 and h-outer 3 before the switch; h-inner 1, started from h-outer, after it)
        assert_eq!((b.replies, a.replies), (2 + 3, 1));
    }

    #[test]
    fn a_session_counts_by_how_it_ran() {
        let user = |ts: &str| format!(r#"{{"type":"user","timestamp":"{ts}","message":{{"content":"go"}}}}"#);
        let lean = r#"{"type":"agent-setting","agentSetting":"fewer-lean","sessionId":"s"}"#.to_string();
        let other = r#"{"type":"agent-setting","agentSetting":"reviewer","sessionId":"s"}"#.to_string();
        let since = vec![("2026-11-23T18:00:00".to_string(), None)];
        let count = |lines: &[String]| {
            let (mut b, mut a) = (Totals::default(), Totals::default());
            let mode = add_session(lines.join("\n").as_bytes(), &since, true, &mut b, &mut a);
            (mode, b.replies, a.replies, b.sessions, a.sessions)
        };
        // started before fewer was turned on, still going after: without it, all of it (an
        // agent applies when a session starts; this one names none)
        let spans = [user("2026-11-23T17:00:00Z"), reply("m1", "2026-11-23T17:00:01Z", false, 1, 100, 0, 1, false), reply("m2", "2026-11-23T19:00:00Z", false, 1, 100, 100, 1, false)];
        assert_eq!(count(&spans), (Mode::Start(false), 2, 0, 1, 0));
        // started after it, naming no agent (none set, or an older Claude Code): by its start
        let later = [user("2026-11-24T09:00:00Z"), reply("m3", "2026-11-24T09:00:01Z", false, 1, 100, 0, 1, false)];
        assert_eq!(count(&later), (Mode::Start(true), 0, 1, 0, 1));
        // started with fewer's agent: with fewer from its first line, whatever the clock says
        let leanly = [lean.clone(), user("2026-11-22T09:00:00Z"), reply("m4", "2026-11-22T09:00:01Z", false, 1, 100, 0, 1, false)];
        assert_eq!(count(&leanly).1..=count(&leanly).2, 0..=1);
        assert_eq!(count(&leanly).0, Mode::Switches(vec![("2026-11-22T09:00:00Z".to_string(), true)]));
        // resumed with fewer on, then again with another agent: each part on its own side; a helper's
        // piece saved after the main one goes by its time, not by where it was saved
        let resumed = [
            user("2026-11-23T17:00:00Z"),
            reply("m5", "2026-11-23T17:00:01Z", false, 1, 100, 0, 1, false),
            lean.clone(),
            user("2026-11-24T09:00:00Z"),
            reply("m6", "2026-11-24T09:00:01Z", false, 1, 100, 0, 1, false),
            other,
            user("2026-11-25T09:00:00Z"),
            reply("m7", "2026-11-25T09:00:01Z", false, 1, 100, 0, 1, false),
            reply("h1", "2026-11-23T17:30:00Z", true, 1, 100, 0, 1, false),
            reply("h2", "2026-11-24T10:00:00Z", true, 1, 100, 0, 1, false),
        ];
        let (mode, b, a, bs, as_) = count(&resumed);
        assert_eq!((b, a, bs, as_), (3, 2, 1, 1), "{mode:?}");
        assert!(!mode.with_at("2026-11-23T23:00:00Z", &since) && mode.with_at("2026-11-24T09:30:00", &since) && !mode.with_at("2026-11-25T10:00:00", &since));
    }

    #[test]
    fn split_times() {
        assert_eq!(split_time("2026-11-23").as_deref(), Some("2026-11-23"));
        assert_eq!(split_time("2026-11-23 18:02").as_deref(), Some("2026-11-23T18:02"));
        assert_eq!(split_time("2026-11-23T18:02:09").as_deref(), Some("2026-11-23T18:02:09"));
        for bad in ["", "yesterday", "2026-1-23", "2026-11-23T18", "2026-11-23T18:02:09Z"] {
            assert_eq!(split_time(bad), None, "{bad}");
        }
        // a transcript's time compares as text with the split
        assert!("2026-11-23T00:00:00.000Z" >= "2026-11-23" && "2026-11-22T23:59:59Z" < "2026-11-23");
    }

    #[test]
    fn savings_said_in_percent() {
        // (percentages, as people read them)
        assert_eq!(percent_fewer(288_891.0, 9_289.0), "97% fewer");
        assert_eq!(percent_number(41_200.0, 3_320.0), "92");
        // near 100: one decimal, never 100% while something is left
        assert_eq!(percent_fewer(100_000.0, 400.0), "99.6% fewer");
        assert_eq!(percent_fewer(1_000_000.0, 1.0), "99.9% fewer");
        assert_eq!(percent_fewer(100.0, 0.0), "100% fewer");
        // more, said as more; nothing before: 0
        assert_eq!(percent_fewer(100.0, 112.0), "12% more");
        assert_eq!(percent_number(0.0, 5.0), "0");
    }

}
