//! Claude's tools over the message keys: `odd` (odd lines
//! first), `errors` (error groups), `compare` (two runs) and `quiet` (gone quiet). They count
//! records (a stack trace is one record) from the runs' catalogs (catalog.rs) and show lines
//! read by reference (fetch.rs): every count is exact and every line shown has a reference.
//! This module sits inside tools.rs's: it shares its scope, arguments and answer helpers.
//!
//! - odd: the keys of the scope's runs. The keys first seen in these runs come first (the
//!   project's key registry knows where each key was first seen: its earliest record, by
//!   time, then run, then line), then the rarest in the project.
//! - errors: the keys with records at level error (a key line at level error, or a multi-line
//!   error record: a stack trace, a crash report); a key's records at other levels are not
//!   counted. Most records first.
//! - compare: two runs, a before and b after: keys new in b, gone from b, changed in count.
//! - quiet: keys and sources whose records fell to 0, or by 90% or more: each source's last
//!   run against the run before it, or a window of time against the window before it.
//!
//! A source is what a run saved: its command (the same argv), else its file (the same path).
//! "The run before" a run is the newest earlier ready run of the same source in its project,
//! whatever the scope's other filters. A record without a clock time counts at its run's save
//! time, as the key registry counts it; times are shown to the minute (the catalogs' times).
//!
//! Runs that are not ready (in progress, being packed, failed) and runs whose catalog cannot be
//! read are named as not counted; a run without a catalog that fits gets one made from its
//! bytes for the answer (never written), and the answer says so. The tables go on with a cursor
//! "#N" (from row N, counted from 0), as `count`'s tables do.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Args, Ctx, clean_end, key_text, minute_span, not_ready, parse_run_ref, plural, projects, select, short_commit, what};
use crate::answer::{Answer, group, line_ref, next_call, run_ref, shown_line};
use crate::catalog::{self, Catalog, Registry, Seen, Windows};
use crate::fetch;
use crate::formats::Level;
use crate::guard::{failed, no_log, passed, source_of};
use crate::json::Json;
use crate::store::{self, Project, RunRec};
use crate::times;

/// The most lines of an error group's first record an answer shows.
pub(super) const RECORD_LINES: u64 = 40;

// ---------------------------------------------------------------- shared

/// When a record was, as the key registry takes it (Unix ms): its minute, else its run's save
/// time.
fn rec_ms(minute: Option<i64>, r: &RunRec) -> i64 {
    minute.map_or(r.i("start").unwrap_or(0), |m| m.saturating_mul(60_000))
}

/// A time to the minute, local: "2026-11-18 10:15".
pub(super) fn minute_text(ms: i64) -> String {
    times::fmt_local(ms)[..16].to_string()
}

/// A table's cursor, "#N": the rows to pass over (0 without a cursor).
pub(super) fn table_cursor(a: &Args) -> Result<usize, String> {
    match a.s("cursor") {
        None => Ok(0),
        Some(c) => c.strip_prefix('#').and_then(|n| n.parse().ok()).ok_or_else(|| format!("'{c}' is not a cursor of this tool (#40 goes on from row 40)")),
    }
}

/// The note of a last line on lines cut at line_max, with the call that gives one whole.
pub(super) fn cut_note(cuts: usize, cut_ref: Option<&str>, line_max: usize) -> String {
    if cuts == 0 {
        return String::new();
    }
    let call = Json::o(vec![("ref", cut_ref.unwrap_or_default().into()), ("line_max", Json::from(65_536u64))]);
    format!("; {} cut at {} bytes (line_max; one whole: lines {call})", plural(cuts as u64, "line"), group(line_max as u64))
}

/// The smallest and largest of some counts: "2 to 46", or "2" when they are the same.
fn range_text(v: impl Iterator<Item = u64> + Clone) -> String {
    let (lo, hi) = (v.clone().min().unwrap_or(0), v.max().unwrap_or(0));
    if lo == hi { group(lo) } else { format!("{} to {}", group(lo), group(hi)) }
}

/// Run ids as answers list them, at most 10: "r16 3, r10 1, r4 2 (+5 more runs)".
fn runs_list(runs: &[(u64, u64)]) -> String {
    let s: Vec<String> = runs.iter().take(10).map(|(id, n)| format!("r{id} {}", group(*n))).collect();
    let more = if runs.len() > 10 { format!(" (+{} more runs)", group((runs.len() - 10) as u64)) } else { String::new() };
    format!("{}{more}", s.join(", "))
}

/// The catalogs an answer reads, each read (or made) once, and the notes about them.
pub(super) struct Cats<'s> {
    stop: &'s AtomicBool,
    /// (project index, run id) -> its catalog; None when it is not counted
    got: HashMap<(usize, u64), Option<Catalog>>,
    /// runs not counted (not ready, or unreadable), and runs whose catalog was made now
    skipped: Vec<(String, String)>,
    made: Vec<(String, String)>,
}

impl<'s> Cats<'s> {
    pub(super) fn new(stop: &'s AtomicBool) -> Cats<'s> {
        Cats { stop, got: HashMap::new(), skipped: Vec::new(), made: Vec::new() }
    }

    /// Read run `r`'s catalog (`p` is its project, `pi` the project's index), once: Err(why)
    /// when it is not counted. A run that is not ready, or whose catalog cannot be read or made,
    /// is named in the notes; a file kept as it is or not read holds no log and is not.
    pub(super) fn load(&mut self, pi: usize, p: &Project, r: &RunRec) -> Result<(), String> {
        let key = (pi, r.id());
        if let Some(c) = self.got.get(&key) {
            return match c {
                Some(_) => Ok(()),
                None => Err(self.why(r.id())),
            };
        }
        let short = format!("r{}", r.id());
        let stop = self.stop;
        let c = if no_log(r) {
            Err(format!("{}: {}", r.kind(), r.s("note").unwrap_or("not a log")))
        } else if r.state() != "ready" {
            // (a run in progress is searched from its capture, but its catalog is made once it
            // is saved: until then it is named, not counted)
            Err(not_ready(p, r, "counted once it is saved"))
        } else {
            match catalog::of_run(&p.dir, r, true, &|| stop.load(Ordering::Relaxed)) {
                Ok((c, why)) => {
                    if let Some(w) = why {
                        self.made.push((short.clone(), w));
                    }
                    Ok(c)
                }
                Err(e) => Err(e),
            }
        };
        match c {
            Ok(c) => {
                self.got.insert(key, Some(c));
                Ok(())
            }
            Err(e) => {
                if !no_log(r) {
                    self.skipped.push((short, e.clone()));
                }
                self.got.insert(key, None);
                Err(e)
            }
        }
    }

    fn why(&self, id: u64) -> String {
        let short = format!("r{id}");
        self.skipped.iter().find(|s| s.0 == short).map_or("not a log".to_string(), |s| s.1.clone())
    }

    pub(super) fn cat(&self, pi: usize, id: u64) -> Option<&Catalog> {
        self.got.get(&(pi, id)).and_then(Option::as_ref)
    }

    /// The lines naming the runs not counted and the catalogs made now.
    pub(super) fn notes(&self) -> Vec<String> {
        let list = |x: &[(String, String)]| {
            let s: Vec<String> = x.iter().take(10).map(|(r, why)| format!("{r} ({why})")).collect();
            let more = if x.len() > 10 { format!(" and {} more", x.len() - 10) } else { String::new() };
            format!("{}{more}", s.join(", "))
        };
        let mut v = Vec::new();
        if !self.skipped.is_empty() {
            v.push(format!("not counted: {}", list(&self.skipped)));
        }
        if !self.made.is_empty() {
            v.push(format!("catalogs made now from the runs' bytes: {}", list(&self.made)));
        }
        v
    }
}

/// One row of a table answer: its lines, in its section (whose heading is shown before the
/// section's first row in an answer).
#[derive(Default)]
pub(super) struct Row {
    pub(super) section: usize,
    pub(super) lines: Vec<String>,
    /// the reference of a line shown cleaned; lines cut at line_max and the first one's
    cleaned: Option<String>,
    cuts: usize,
    cut_ref: Option<String>,
}

impl Row {
    fn note(&mut self, rf: &str, cleaned: bool, cut: bool) {
        if cleaned && self.cleaned.is_none() {
            self.cleaned = Some(rf.to_string());
        }
        if cut {
            self.cuts += 1;
            if self.cut_ref.is_none() {
                self.cut_ref = Some(rf.to_string());
            }
        }
    }
}

/// Lines read by reference for an answer, each run's archive opened once.
#[derive(Default)]
pub(super) struct Lines {
    got: HashMap<(usize, u64, u64), Vec<u8>>,
    errs: HashMap<(usize, u64), String>,
}

impl Lines {
    /// Read the lines `want` names: (project index, run id) -> line ranges.
    pub(super) fn read(projects: &[Project], want: &BTreeMap<(usize, u64), Vec<(u64, u64)>>) -> Lines {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let mut l = Lines::default();
        for (&(pi, id), ranges) in want {
            match fetch::fetch(&projects[pi].dir.join(store::run_file(id)), ranges, threads) {
                Ok(v) => {
                    for (n, b) in v {
                        l.got.insert((pi, id, n), b);
                    }
                }
                Err(e) => {
                    l.errs.insert((pi, id), e);
                }
            }
        }
        l
    }

    /// A line's text as answers show it, noted in `row` when it is shown cleaned or cut.
    pub(super) fn text(&self, projects: &[Project], pi: usize, id: u64, line: u64, line_max: usize, row: &mut Row) -> String {
        match self.got.get(&(pi, id, line)) {
            Some(b) => {
                let b = clean_end(b);
                let (t, cl, cut) = shown_line(b, b.len(), line_max);
                row.note(&line_ref(&projects[pi].pid, id, line), cl, cut);
                t
            }
            None => format!("(not read: {})", self.errs.get(&(pi, id)).map_or("past the end of its run", String::as_str)),
        }
    }

    /// A line with its reference: "fewer-3fa2/r42:7 text".
    pub(super) fn show(&self, projects: &[Project], pi: usize, id: u64, line: u64, line_max: usize, row: &mut Row) -> String {
        let t = self.text(projects, pi, id, line, line_max, row);
        format!("{} {t}", line_ref(&projects[pi].pid, id, line))
    }
}

/// What an answer showed of a table: the first row not shown (None: every row from the
/// cursor's on), and the lines cut at line_max with the first one's reference.
pub(super) struct Shown {
    pub(super) next: Option<usize>,
    pub(super) cuts: usize,
    pub(super) cut_ref: Option<String>,
}

/// Show `rows`, the table's rows from row `first` on (of `total` rows), in order while `limit`
/// and the cap allow, each section's heading before its first row in the answer. The first
/// row is always shown (cut to the room left), so following cursors always moves on.
pub(super) fn show_rows(ans: &mut Answer, heads: &[String], rows: &[Row], first: usize, total: usize, limit: usize) -> Shown {
    let mut s = Shown { next: None, cuts: 0, cut_ref: None };
    let mut section = usize::MAX;
    for (i, row) in rows.iter().enumerate() {
        let head = if row.section != section { heads.get(row.section).filter(|h| !h.is_empty()) } else { None };
        let size = head.map_or(0, |h| h.len() + 1) + row.lines.iter().map(|l| l.len() + 1).sum::<usize>();
        if i >= limit || (i > 0 && !ans.fits(size)) {
            s.next = Some(first + i);
            return s;
        }
        // (they fit; the first row of an answer is cut to the room left when they do not)
        if let Some(h) = head {
            ans.must(h);
        }
        for l in &row.lines {
            ans.must(l);
        }
        if let Some(c) = &row.cleaned {
            ans.cleaned(c);
        }
        s.cuts += row.cuts;
        if s.cut_ref.is_none() {
            s.cut_ref = row.cut_ref.clone();
        }
        section = row.section;
    }
    if first + rows.len() < total {
        s.next = Some(first + rows.len());
    }
    s
}

/// A run's keys by text (its levels together): records, and the first record (the first by
/// line in the run, at its minute or the run's save time), as the key registry takes them.
fn run_keys<'c>(c: &'c Catalog, r: &RunRec) -> HashMap<&'c [u8], (u64, Seen)> {
    let mut m: HashMap<&[u8], (u64, Seen)> = HashMap::new();
    for k in &c.keys {
        let s = Seen { ms: rec_ms(k.first_minute, r), run: r.id(), line: k.first_line };
        let e = m.entry(&k.text[..]).or_insert((0, s));
        e.0 += k.records;
        if k.first_line < e.1.line {
            e.1 = s;
        }
    }
    m
}

// ---------------------------------------------------------------- odd

/// A project's key registry with every ready run of it merged in: keys.cat when it holds them
/// all, else one made now from the runs' catalogs (keys.cat missing, damaged, or without a run
/// whose catalog was not made when it was saved). Never written.
fn registry(pi: usize, p: &Project, cats: &mut Cats) -> Registry {
    let runs = store::read_runs(&p.dir);
    let logs: Vec<&RunRec> = runs.iter().filter(|r| r.state() == "ready" && !no_log(r)).collect();
    if let Some(reg) = Registry::read(&p.dir)
        && reg.runs.iter().copied().eq(logs.iter().map(|r| r.id()))
    {
        return reg;
    }
    let mut reg = Registry::default();
    for r in logs {
        if cats.load(pi, p, r).is_ok()
            && let Some(c) = cats.cat(pi, r.id())
        {
            reg.merge(r.id(), r.i("start").unwrap_or(0), c);
        }
    }
    reg
}

/// A key of `odd`'s table.
struct OddKey {
    pi: usize,
    text: Vec<u8>,
    /// records in the scope, and in the project
    here: u64,
    project: u64,
    /// where it was first seen in the project, and its first record in the scope
    first: Seen,
    sample: Seen,
    /// first seen in the scope's runs
    new: bool,
}

pub(super) fn tool_odd(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let sc = select(ctx, a, true)?;
    let skip = table_cursor(a)?;
    let limit = a.i("limit", 20) as usize;
    let line_max = a.i("line_max", 400) as usize;
    let mut cats = Cats::new(stop);
    // the scope's keys: (project, text) -> records, and the first record
    let mut here: HashMap<(usize, Vec<u8>), (u64, Seen)> = HashMap::new();
    let mut counted: Vec<HashSet<u64>> = vec![HashSet::new(); sc.projects.len()];
    for (pi, r) in &sc.runs {
        if cats.load(*pi, &sc.projects[*pi], r).is_err() {
            continue;
        }
        let Some(c) = cats.cat(*pi, r.id()) else { continue };
        counted[*pi].insert(r.id());
        for (text, (n, first)) in run_keys(c, r) {
            let e = here.entry((*pi, text.to_vec())).or_insert((0, first));
            e.0 += n;
            e.1 = e.1.min(first);
        }
    }
    let regs: Vec<Registry> = sc.projects.iter().enumerate().map(|(pi, p)| registry(pi, p, &mut cats)).collect();
    let mut keys: Vec<OddKey> = here
        .into_iter()
        .map(|((pi, text), (n, sample))| {
            // (a key the registry does not hold, which a registry made from the same catalogs
            // always does, counts with the scope's numbers)
            let (project, first) = regs[pi].keys.get(&text).map_or((n, sample), |k| (k.records, k.first));
            let new = counted[pi].contains(&first.run);
            OddKey { pi, text, here: n, project, first, sample, new }
        })
        .collect();
    keys.sort_by(|x, y| y.new.cmp(&x.new).then(x.project.cmp(&y.project)).then(x.here.cmp(&y.here)).then(x.first.cmp(&y.first)).then(x.pi.cmp(&y.pi)).then(x.text.cmp(&y.text)));
    let total = keys.len();
    let n_new = keys.iter().filter(|k| k.new).count();
    let runs_counted: usize = counted.iter().map(HashSet::len).sum();
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    ans.must(&format!(
        "odd lines first: {} in {} ({}): {} new (first seen in these runs), then the rarest in the project",
        plural(total as u64, "message key"),
        plural(runs_counted as u64, "run"),
        sc.desc,
        group(n_new as u64)
    ));
    for n in cats.notes() {
        ans.must(&n);
    }
    // the rows from the cursor's on: each key's first line in the scope, read by reference
    let shown_keys = keys.get(skip..total.min(skip.saturating_add(limit))).unwrap_or(&[]);
    let mut want: BTreeMap<(usize, u64), Vec<(u64, u64)>> = BTreeMap::new();
    for k in shown_keys {
        want.entry((k.pi, k.sample.run)).or_default().push((k.sample.line, k.sample.line));
    }
    let lines = Lines::read(&sc.projects, &want);
    let mut rows = Vec::new();
    for k in shown_keys {
        let mut row = Row::default();
        let l = lines.show(&sc.projects, k.pi, k.sample.run, k.sample.line, line_max, &mut row);
        row.lines.push(l);
        let counts = format!("{} in these runs, {} in the project", plural(k.here, "record"), group(k.project));
        row.lines.push(if k.new {
            format!("  new: {counts}; first seen here, {}", minute_text(k.first.ms))
        } else {
            format!("  {counts}; first seen {} at {}", minute_text(k.first.ms), line_ref(&sc.projects[k.pi].pid, k.first.run, k.first.line))
        });
        rows.push(row);
    }
    let s = show_rows(&mut ans, &[], &rows, skip, total, limit);
    let cuts = cut_note(s.cuts, s.cut_ref.as_deref(), line_max);
    let last = match s.next {
        None => format!("all shown{cuts}"),
        Some(i) => {
            let left = &keys[i..];
            let new_left = left.iter().filter(|k| k.new).count();
            let new_note = if new_left > 0 { format!("{} of them new, ", group(new_left as u64)) } else { String::new() };
            format!(
                "left out: {} of {} keys (#{i} on: {new_note}{} records in the project){cuts}. Next: {}",
                group(left.len() as u64),
                group(total as u64),
                range_text(left.iter().map(|k| k.project)),
                next_call("odd", &a.with_cursor(&format!("#{i}")))
            )
        }
    };
    Ok(ans.finish(&last))
}

// ---------------------------------------------------------------- errors

/// An error group: an error key's records at level error over the scope's runs.
struct ErrGroup {
    pi: usize,
    text: Vec<u8>,
    records: u64,
    /// (run id, records), in scope order: newest first
    runs: Vec<(u64, u64)>,
    first: Seen,
    /// the first record's first and last line (a whole stack trace), in run `first.run`
    record: (u64, u64),
    last: Seen,
}

pub(super) fn tool_errors(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let sc = select(ctx, a, true)?;
    let skip = table_cursor(a)?;
    let limit = a.i("limit", 5) as usize;
    let line_max = a.i("line_max", 400) as usize;
    let mut cats = Cats::new(stop);
    let mut at: HashMap<(usize, Vec<u8>), usize> = HashMap::new();
    let mut groups: Vec<ErrGroup> = Vec::new();
    let (mut runs_counted, mut runs_with, mut records, mut errors) = (0u64, 0u64, 0u64, 0u64);
    for (pi, r) in &sc.runs {
        if cats.load(*pi, &sc.projects[*pi], r).is_err() {
            continue;
        }
        let Some(c) = cats.cat(*pi, r.id()) else { continue };
        runs_counted += 1;
        records += c.records;
        let mut any = false;
        // (a catalog has one entry per key and level: a key's error records are one entry)
        for k in c.keys.iter().filter(|k| k.level == Level::Error) {
            any = true;
            errors += k.records;
            let first = Seen { ms: rec_ms(k.first_minute, r), run: r.id(), line: k.first_line };
            let last = Seen { ms: rec_ms(k.last_minute, r), run: r.id(), line: k.last_line };
            let i = *at.entry((*pi, k.text.clone())).or_insert_with(|| {
                groups.push(ErrGroup { pi: *pi, text: k.text.clone(), records: 0, runs: Vec::new(), first, record: k.first_record, last });
                groups.len() - 1
            });
            let g = &mut groups[i];
            g.records += k.records;
            g.runs.push((r.id(), k.records));
            if first < g.first {
                g.first = first;
                g.record = k.first_record;
            }
            if last > g.last {
                g.last = last;
            }
        }
        runs_with += any as u64;
    }
    groups.sort_by(|x, y| y.records.cmp(&x.records).then(x.first.cmp(&y.first)).then(x.pi.cmp(&y.pi)).then(x.text.cmp(&y.text)));
    let total = groups.len();
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    if total == 0 {
        ans.must(&format!("no error records in {} ({}): none of their {} is at level error or in a multi-line error record", plural(runs_counted, "run"), sc.desc, plural(records, "record")));
    } else {
        ans.must(&format!(
            "{} ({}) in {} of {} ({}), most records first",
            plural(total as u64, "error group"),
            plural(errors, "error record"),
            group(runs_with),
            plural(runs_counted, "run"),
            sc.desc
        ));
    }
    for n in cats.notes() {
        ans.must(&n);
    }
    // the groups from the cursor's on, with their first records read by reference
    let end = total.min(skip.saturating_add(limit));
    let shown = groups.get(skip..end).unwrap_or(&[]);
    let mut want: BTreeMap<(usize, u64), Vec<(u64, u64)>> = BTreeMap::new();
    for g in shown {
        want.entry((g.pi, g.first.run)).or_default().push((g.record.0, g.record.1.min(g.record.0.saturating_add(RECORD_LINES - 1))));
    }
    let lines = Lines::read(&sc.projects, &want);
    let (mut cuts, mut cut_ref) = (0usize, None::<String>);
    let mut next = None;
    for (n, g) in shown.iter().enumerate() {
        let (pid, id) = (&sc.projects[g.pi].pid, g.first.run);
        let mut row = Row::default();
        // the group: its key, counts, first and last record and runs (needed), then its first
        // record, at most RECORD_LINES lines
        let mut head = vec![
            format!("{} {}", plural(g.records, "error record"), Json::Str(key_text(&g.text, 300))),
            format!(
                "  first {} {}, last {} {}{}",
                minute_text(g.first.ms),
                line_ref(pid, g.first.run, g.first.line),
                minute_text(g.last.ms),
                line_ref(pid, g.last.run, g.last.line),
                // (the runs and their counts only when the answer looks at more than one run)
                if runs_counted > 1 { format!("; in {}: {}", plural(g.runs.len() as u64, "run"), runs_list(&g.runs)) } else { String::new() }
            ),
        ];
        let (r0, r1) = g.record;
        let to = r1.min(r0.saturating_add(RECORD_LINES - 1));
        let mut body = Vec::new();
        if r0 == r1 {
            let l = lines.show(&sc.projects, g.pi, id, r0, line_max, &mut row);
            head.push(format!("  first record: {l}"));
        } else {
            head.push(format!("  first record, {}:{r0}-{r1} ({} lines):", run_ref(pid, id), group(r1 - r0 + 1)));
            for l in r0..=to {
                let t = lines.text(&sc.projects, g.pi, id, l, line_max, &mut row);
                body.push(format!("  {l} {t}"));
            }
        }
        let rest = |from: u64| {
            let call = Json::o(vec![("ref", format!("{}:{from}-{r1}", run_ref(pid, id)).into()), ("before", Json::from(0u64)), ("after", Json::from(0u64))]);
            format!("  (+{} more lines of this record: lines {call})", group(r1 - from + 1))
        };
        if to < r1 {
            body.push(rest(to + 1));
        }
        let size: usize = head.iter().chain(body.iter()).map(|l| l.len() + 1).sum();
        if n > 0 && !ans.fits(size) {
            next = Some(skip + n);
            break;
        }
        for l in &head {
            ans.must(l);
        }
        if body.is_empty() || ans.fits(body.iter().map(|l| l.len() + 1).sum()) {
            for l in &body {
                ans.must(l);
            }
        } else {
            // the first group of an answer, its record too big for the cap: the record's lines
            // while they fit, then where the rest is (body holds a record of several lines)
            let mut shown_to = r0.saturating_sub(1);
            for (j, l) in body.iter().enumerate().take((to - r0 + 1) as usize) {
                if !ans.fits(l.len() + 1 + 200) {
                    break;
                }
                ans.must(l);
                shown_to = r0 + j as u64;
            }
            ans.must(&rest(shown_to + 1));
        }
        if let Some(c) = &row.cleaned {
            ans.cleaned(c);
        }
        cuts += row.cuts;
        if cut_ref.is_none() {
            cut_ref = row.cut_ref.clone();
        }
    }
    if next.is_none() && end < total {
        next = Some(end);
    }
    let note = cut_note(cuts, cut_ref.as_deref(), line_max);
    let last = match next {
        None => format!("all shown{note}"),
        Some(i) => {
            let left = &groups[i..];
            let left_records: u64 = left.iter().map(|g| g.records).sum();
            format!(
                "left out: {} of {} error groups (#{i} on: {}, {} each){note}. Next: {}",
                group(left.len() as u64),
                group(total as u64),
                plural(left_records, "error record"),
                range_text(left.iter().map(|g| g.records)),
                next_call("errors", &a.with_cursor(&format!("#{i}")))
            )
        }
    };
    Ok(ans.finish(&last))
}

// ---------------------------------------------------------------- compare

/// A run as compare's heading shows it: reference, kind, command or file, start, duration,
/// exit, branch@commit, and its records and lines.
fn run_line(pid: &str, r: &RunRec, c: &Catalog) -> String {
    let mut s = format!("{} {}", run_ref(pid, r.id()), r.kind());
    let w = what(r);
    if !w.is_empty() {
        s.push_str(&format!(" {w}"));
    }
    if let Some(st) = r.i("start") {
        s.push_str(&format!(", {}", times::fmt_local(st)));
        if let Some(e) = r.i("end") {
            s.push_str(&format!(", {}", times::fmt_duration(e - st)));
        }
    }
    if let Some(e) = r.i("exit") {
        s.push_str(&format!(", exit {e}"));
    }
    if let Some(sig) = r.i("signal") {
        s.push_str(&format!(", signal {sig}"));
    }
    if let Some(b) = r.s("branch") {
        s.push_str(&format!(", {b}@{}", short_commit(r)));
    } else if r.s("commit").is_some() {
        s.push_str(&format!(", commit {}", short_commit(r)));
    }
    if r.0.get("dirty").and_then(Json::bool) == Some(true) {
        s.push_str(" (dirty)");
    }
    s.push_str(&format!(": {} in {}", plural(c.records, "record"), plural(c.lines, "line")));
    s
}

pub(super) fn tool_compare(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let sc = select(ctx, a, true)?;
    let home = ctx.home.clone()?;
    let skip = table_cursor(a)?;
    let limit = a.i("limit", 30) as usize;
    let line_max = a.i("line_max", 400) as usize;
    let failing = a.b("failing");
    // the projects of the two runs: the scope's, and those their references name
    let mut ps: Vec<Project> = sc.projects.clone();
    let mut runs_of: HashMap<usize, Vec<RunRec>> = HashMap::new();
    let mut named = |s: &str| -> Result<(usize, RunRec), String> {
        let (pid, id) = parse_run_ref(s).ok_or_else(|| format!("'{s}' is not a run reference (r42, fewer-3fa2/r42)"))?;
        let pi = match pid {
            Some("*") => return Err(format!("'{s}' is not a run reference: it names one project (fewer-3fa2/r{id})")),
            Some(pid) => match ps.iter().position(|p| p.pid == pid) {
                Some(pi) => pi,
                None => {
                    ps.extend(projects(ctx, &home, Some(pid))?);
                    ps.len() - 1
                }
            },
            None if ps.len() == 1 => 0,
            None => return Err(format!("'{s}': give its project too (fewer-3fa2/r{id}): this scope has {} projects", ps.len())),
        };
        let runs = runs_of.entry(pi).or_insert_with(|| store::read_runs(&ps[pi].dir));
        let r = runs.iter().find(|r| r.id() == id).ok_or_else(|| format!("no run r{id} in {}", ps[pi].pid))?;
        Ok((pi, r.clone()))
    };
    let given_a = a.s("a").map(&mut named).transpose()?;
    let given_b = a.s("b").map(&mut named).transpose()?;
    let picked_b = given_b.is_none();
    let ready = |r: &RunRec| r.state() == "ready" && !no_log(r);
    // b: given, else the last run of the scope (the last failing one with `failing`; after a,
    // of a's command or file, when only a is given)
    let b = match given_b {
        Some(b) => b,
        None => {
            let after_a = |pi: usize, r: &RunRec| given_a.as_ref().is_none_or(|(api, ar)| *api == pi && r.id() > ar.id() && source_of(r).is_some() && source_of(r) == source_of(ar));
            let found = sc.runs.iter().find(|(pi, r)| ready(r) && (!failing || failed(r)) && after_a(*pi, r));
            match found {
                Some((pi, r)) => (*pi, r.clone()),
                None if failing => return Err(format!("no failing run (an exit status other than 0, or a signal) in this scope ({}); give a and b", sc.desc)),
                None => return Err(format!("no run to compare in this scope ({}){}", sc.desc, if given_a.is_some() { " after a, of its command or file; give b" } else { "" })),
            }
        }
    };
    // a: given, else the run before b, of the same command or file (the passing one with
    // `failing`), in b's project
    let a_run = match given_a {
        Some(x) => x,
        None => {
            let src = source_of(&b.1).ok_or_else(|| format!("{} has no command or file to find the run before it: give a", run_ref(&ps[b.0].pid, b.1.id())))?;
            let runs = runs_of.entry(b.0).or_insert_with(|| store::read_runs(&ps[b.0].dir));
            let found = runs.iter().rev().find(|r| r.id() < b.1.id() && ready(r) && source_of(r).as_deref() == Some(src.as_str()) && (!failing || passed(r)));
            match found {
                Some(r) => (b.0, r.clone()),
                None => return Err(format!("no {}run of {} before {} to compare it with: give a", if failing { "passing " } else { "" }, what(&b.1), run_ref(&ps[b.0].pid, b.1.id()))),
            }
        }
    };
    if a_run.0 == b.0 && a_run.1.id() == b.1.id() {
        return Err(format!("a and b are the same run ({})", run_ref(&ps[b.0].pid, b.1.id())));
    }
    let mut cats = Cats::new(stop);
    for (pi, r) in [&a_run, &b] {
        cats.load(*pi, &ps[*pi], r).map_err(|e| format!("{}: {e}", run_ref(&ps[*pi].pid, r.id())))?;
    }
    let (ca, cb) = (cats.cat(a_run.0, a_run.1.id()).expect("loaded"), cats.cat(b.0, b.1.id()).expect("loaded"));
    // the keys of each: text -> (records, first line)
    let side = |c: &Catalog| -> HashMap<Vec<u8>, (u64, u64)> { c.by_text().into_iter().map(|(t, n, _, first, _)| (t.to_vec(), (n, first))).collect() };
    let (ka, kb) = (side(ca), side(cb));
    // the keys new in b, gone from b and changed: (records in a, in b, the line shown: the
    // key's first in b, or in a for a key gone from b)
    let mut new: Vec<(u64, u64, u64)> = Vec::new();
    let mut gone: Vec<(u64, u64, u64)> = Vec::new();
    let mut changed: Vec<(u64, u64, u64)> = Vec::new();
    let mut same = 0u64;
    for (t, &(nb, lb)) in &kb {
        match ka.get(t) {
            None => new.push((0, nb, lb)),
            Some(&(na, _)) if na != nb => changed.push((na, nb, lb)),
            Some(_) => same += 1,
        }
    }
    for (t, &(na, la)) in &ka {
        if !kb.contains_key(t) {
            gone.push((na, 0, la));
        }
    }
    new.sort_by_key(|x| x.2);
    gone.sort_by_key(|x| x.2);
    changed.sort_by(|x, y| x.0.abs_diff(x.1).cmp(&y.0.abs_diff(y.1)).reverse().then(x.2.cmp(&y.2)));
    let (pa, pb) = (&ps[a_run.0].pid, &ps[b.0].pid);
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    ans.must(&format!(
        "compare a = {} with b = {}, by message key (records; a stack trace is one): {} new in b, {} gone from b, {} changed, {} the same",
        run_ref(pa, a_run.1.id()),
        run_ref(pb, b.1.id()),
        group(new.len() as u64),
        group(gone.len() as u64),
        group(changed.len() as u64),
        group(same)
    ));
    ans.must(&format!("a: {}", run_line(pa, &a_run.1, ca)));
    ans.must(&format!("b: {}", run_line(pb, &b.1, cb)));
    for n in cats.notes() {
        ans.must(&n);
    }
    if picked_b {
        // newer runs of b's command or file that are not counted yet
        let src = source_of(&b.1);
        let newer: Vec<String> = sc.runs.iter().filter(|(pi, r)| *pi == b.0 && r.id() > b.1.id() && r.state() != "ready" && !no_log(r) && source_of(r) == src).map(|(_, r)| format!("r{} ({})", r.id(), r.state())).collect();
        if !newer.is_empty() {
            ans.must(&format!("newer runs of it not compared (not saved yet): {}", newer.join(", ")));
        }
    }
    let sum = |v: &[(u64, u64, u64)], b_side: bool| v.iter().map(|x| if b_side { x.1 } else { x.0 }).sum::<u64>();
    let heads = vec![
        format!("new in b ({}, {}), each with its first line in b:", plural(new.len() as u64, "key"), plural(sum(&new, true), "record")),
        format!("gone from b ({}, {}), each with its first line in a:", plural(gone.len() as u64, "key"), plural(sum(&gone, false), "record")),
        format!("changed ({}), the largest change first, each with its first line in b:", plural(changed.len() as u64, "key")),
    ];
    let all: Vec<(usize, u64, u64, u64)> = new.iter().map(|x| (0, x.0, x.1, x.2)).chain(gone.iter().map(|x| (1, x.0, x.1, x.2))).chain(changed.iter().map(|x| (2, x.0, x.1, x.2))).collect();
    let total = all.len();
    let shown = all.get(skip..total.min(skip.saturating_add(limit))).unwrap_or(&[]);
    let mut want: BTreeMap<(usize, u64), Vec<(u64, u64)>> = BTreeMap::new();
    let run_of = |section: usize| if section == 1 { (a_run.0, a_run.1.id()) } else { (b.0, b.1.id()) };
    for x in shown {
        want.entry(run_of(x.0)).or_default().push((x.3, x.3));
    }
    let lines = Lines::read(&ps, &want);
    let mut rows = Vec::new();
    for x in shown {
        let mut row = Row { section: x.0, ..Row::default() };
        let (pi, id) = run_of(x.0);
        let l = lines.show(&ps, pi, id, x.3, line_max, &mut row);
        row.lines.push(format!("{} -> {}  {l}", group(x.1), group(x.2)));
        rows.push(row);
    }
    let s = show_rows(&mut ans, &heads, &rows, skip, total, limit);
    let cuts = cut_note(s.cuts, s.cut_ref.as_deref(), line_max);
    let last = match s.next {
        None => format!("all shown{cuts}"),
        Some(i) => {
            let left = &all[i..];
            let n = |sec: usize| left.iter().filter(|x| x.0 == sec).count() as u64;
            // (the next call names a and b, so it goes on with the same two runs)
            let mut next = a.with_cursor(&format!("#{i}"));
            next.set("a", run_ref(pa, a_run.1.id()).into());
            next.set("b", run_ref(pb, b.1.id()).into());
            format!(
                "left out: {} of {} rows (#{i} on: {} new, {} gone, {} changed){cuts}. Next: {}",
                group(left.len() as u64),
                group(total as u64),
                group(n(0)),
                group(n(1)),
                group(n(2)),
                next_call("compare", &next)
            )
        }
    };
    Ok(ans.finish(&last))
}

// ---------------------------------------------------------------- quiet

/// One side of a source's comparison (quiet): per key text its records and the first one
/// (when, run, line), and all records and error records.
#[derive(Default)]
struct Side {
    keys: HashMap<Vec<u8>, (u64, Seen)>,
    records: u64,
    errors: u64,
    /// the runs with records on this side
    runs: Vec<u64>,
}

impl Side {
    /// Add `n` records of key `text`, `errors` of them at level error, the first at `first`.
    fn add(&mut self, text: &[u8], n: u64, errors: u64, first: Seen) {
        if n == 0 {
            return;
        }
        let e = self.keys.entry(text.to_vec()).or_insert((0, first));
        e.0 += n;
        if first < e.1 {
            e.1 = first;
        }
        self.records += n;
        self.errors += errors;
        if !self.runs.contains(&first.run) {
            self.runs.push(first.run);
        }
    }

    /// A whole run's records.
    fn add_run(&mut self, c: &Catalog, r: &RunRec) {
        for k in &c.keys {
            let errors = if k.level == Level::Error { k.records } else { 0 };
            self.add(&k.text, k.records, errors, Seen { ms: rec_ms(k.first_minute, r), run: r.id(), line: k.first_line });
        }
    }
}

/// A source compared: its label, the runs or window on each side, and the two sides.
struct Compared {
    pi: usize,
    label: String,
    /// "pid/r16 against pid/r13", or the runs in the windows
    sides: String,
    before: Side,
    after: Side,
}

/// Whether a key's records fell enough to be told: to 0, or by 90% or more.
fn fell(before: u64, after: u64) -> bool {
    before > 0 && after.saturating_mul(10) <= before
}

impl Compared {
    /// The keys that fell, the biggest fall first: (text, before, after, the first record before).
    fn quiet_keys(&self) -> Vec<(&[u8], u64, u64, Seen)> {
        let mut v: Vec<(&[u8], u64, u64, Seen)> = self
            .before
            .keys
            .iter()
            .filter_map(|(t, &(nb, first))| {
                let na = self.after.keys.get(t).map_or(0, |x| x.0);
                fell(nb, na).then_some((&t[..], nb, na, first))
            })
            .collect();
        v.sort_by(|x, y| (y.1 - y.2).cmp(&(x.1 - x.2)).then(x.3.cmp(&y.3)).then(x.0.cmp(y.0)));
        v
    }

    /// What the source did after, told apart: "logged nothing", "logged no errors", or its
    /// records' fall or change.
    fn status(&self) -> String {
        let (b, a) = (&self.before, &self.after);
        let before = format!("before: {}, {}", plural(b.records, "record"), plural(b.errors, "error"));
        if a.records == 0 {
            format!("logged nothing (0 records; {before})")
        } else if b.errors > 0 && a.errors == 0 {
            format!("logged no errors ({}, none an error; {before})", plural(a.records, "record"))
        } else if fell(b.records, a.records) {
            format!("fell by 90% or more ({}, {}; {before})", plural(a.records, "record"), plural(a.errors, "error"))
        } else {
            format!("{}, {} ({before})", plural(a.records, "record"), plural(a.errors, "error"))
        }
    }
}

pub(super) fn tool_quiet(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let skip = table_cursor(a)?;
    let limit = a.i("limit", 30) as usize;
    let line_max = a.i("line_max", 400) as usize;
    let window = a.s("against") == Some("window");
    let mut cats = Cats::new(stop);
    // the sources compared, and those that are not: (label, why)
    let mut done: Vec<Compared> = Vec::new();
    let mut not: Vec<(String, String)> = Vec::new();
    let (head, sc, pins) = if !window {
        let sc = select(ctx, a, true)?;
        // each source's last ready run of the scope, newest first; runs of it not ready yet
        let mut order: Vec<(usize, String)> = Vec::new();
        let mut last: HashMap<(usize, String), RunRec> = HashMap::new();
        let mut waiting: HashMap<(usize, String), Vec<String>> = HashMap::new();
        for (pi, r) in &sc.runs {
            let Some(src) = source_of(r).filter(|_| !no_log(r)) else { continue };
            let key = (*pi, src);
            if last.contains_key(&key) {
                continue;
            }
            if r.state() == "ready" {
                order.push(key.clone());
                last.insert(key, r.clone());
            } else {
                waiting.entry(key).or_default().push(format!("r{} ({})", r.id(), r.state()));
            }
        }
        // (a source whose runs in the scope are all still being saved)
        for (pi, r) in &sc.runs {
            if let Some(src) = source_of(r).filter(|_| !no_log(r))
                && !last.contains_key(&(*pi, src.clone()))
                && let Some(w) = waiting.remove(&(*pi, src))
            {
                not.push((what(r), format!("not saved yet: {}", w.join(", "))));
            }
        }
        let mut runs_of: HashMap<usize, Vec<RunRec>> = HashMap::new();
        for key in order {
            let r = &last[&key];
            let (pi, p) = (key.0, &sc.projects[key.0]);
            let label = what(r);
            let runs = runs_of.entry(pi).or_insert_with(|| store::read_runs(&p.dir));
            let Some(before) = runs.iter().rev().find(|x| x.id() < r.id() && x.state() == "ready" && !no_log(x) && source_of(x).as_deref() == Some(key.1.as_str())) else {
                not.push((label, format!("{} is its first saved run: nothing before it", run_ref(&p.pid, r.id()))));
                continue;
            };
            let before = before.clone();
            let la = cats.load(pi, p, r);
            let lb = cats.load(pi, p, &before);
            if let Err(e) = la.and(lb) {
                not.push((label, e));
                continue;
            }
            let mut c = Compared { pi, label, sides: format!("{} against {}", run_ref(&p.pid, r.id()), run_ref(&p.pid, before.id())), before: Side::default(), after: Side::default() };
            c.before.add_run(cats.cat(pi, before.id()).expect("loaded"), &before);
            c.after.add_run(cats.cat(pi, r.id()).expect("loaded"), r);
            if let Some(w) = waiting.get(&key) {
                c.sides.push_str(&format!(" (newer, not saved yet: {})", w.join(", ")));
            }
            done.push(c);
        }
        let head = format!("quiet: keys that fell to 0 or by 90% or more, each source's last run against the run before it ({})", sc.desc);
        (head, sc, Vec::new())
    } else {
        // the window, in whole minutes: from since's minute to until's (until's own minute
        // only when until is not on its start), and the same length before it
        let now = times::now_ms();
        let until = a.s("until").map(|s| times::parse_when(s, now)).transpose()?.unwrap_or(now);
        let since = a.s("since").map(|s| times::parse_when(s, now)).transpose()?.unwrap_or(until - 3_600_000);
        let (wa, wb) = (since.div_euclid(60_000), until.div_euclid(60_000) + (until.rem_euclid(60_000) > 0) as i64);
        if wb <= wa {
            return Err(format!("the window is empty: since ({}) must be before until ({})", times::fmt_local(since), times::fmt_local(until)));
        }
        let (pa, pb) = (wa - (wb - wa), wa);
        // the scope without since and until: the window picks the runs
        let mut args = a.0.clone();
        args.remove("since");
        args.remove("until");
        let sc = select(ctx, &Args(&args), true)?;
        let mut by: HashMap<(usize, String), Compared> = HashMap::new();
        // each source's newest run with records in the windows, or that may hold some but could
        // not be counted, and its label (sources are listed newest first)
        let mut newest: HashMap<(usize, String), (u64, String)> = HashMap::new();
        let mut broken: HashMap<(usize, String), Vec<String>> = HashMap::new();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let new_entry = |pi: usize| Compared { pi, label: String::new(), sides: String::new(), before: Side::default(), after: Side::default() };
        // runs oldest first, so each source's label is its newest run's
        for (pi, r) in sc.runs.iter().rev() {
            let Some(src) = source_of(r).filter(|_| !no_log(r)) else { continue };
            let key = (*pi, src);
            let p = &sc.projects[*pi];
            let start_min = r.i("start").unwrap_or(0).div_euclid(60_000);
            // its records' minutes, by what runs.jsonl says (its lines' times, else its save
            // times): a run that may hold records in the windows is read
            let (lo, hi) = match (r.i("first"), r.i("last")) {
                (Some(f), Some(l)) => (f.div_euclid(60_000).min(start_min), l.div_euclid(60_000).max(start_min)),
                _ => (start_min, r.i("end").map_or(i64::MAX, |e| e.div_euclid(60_000)).max(start_min)),
            };
            if hi < pa || lo >= wb {
                continue;
            }
            if let Err(err) = cats.load(*pi, p, r) {
                broken.entry(key.clone()).or_default().push(format!("r{} ({err})", r.id()));
                newest.insert(key, (r.id(), what(r)));
                continue;
            }
            let c = cats.cat(*pi, r.id()).expect("loaded");
            // its records' minutes, from its catalog: its lines' times, and its save time for
            // records without one
            let timed: u64 = c.keys.iter().flat_map(|k| k.hours.iter().map(|h| h.1)).sum();
            let untimed = c.records > timed;
            let mut span: Option<(i64, i64)> = c.first_ms.zip(c.last_ms).map(|(f, l)| (f.div_euclid(60_000), l.div_euclid(60_000)));
            if untimed {
                span = Some(span.map_or((start_min, start_min), |(f, l)| (f.min(start_min), l.max(start_min))));
            }
            let Some((lo, hi)) = span else { continue };
            if hi < pa || lo >= wb {
                continue;
            }
            if lo >= wa && hi < wb {
                by.entry(key.clone()).or_insert_with(|| new_entry(*pi)).after.add_run(c, r);
            } else if lo >= pa && hi < pb {
                by.entry(key.clone()).or_insert_with(|| new_entry(*pi)).before.add_run(c, r);
            } else {
                // a window's edge cuts the run: its records counted again, each at its minute
                let w = Windows { spans: vec![(pa, pb), (wa, wb)], marks: c.marks.clone(), untimed: start_min, counts: Vec::new() };
                let tags = catalog::Tags { name: catalog::source_name(r), ..catalog::Tags::default() };
                let path = p.dir.join(store::run_file(r.id()));
                match catalog::archive_windows(&path, &tags, w, threads, &|| stop.load(Ordering::Relaxed)) {
                    Ok((c2, w)) => {
                        if w.counts.iter().all(|x| x.iter().all(|n| n.0 == 0)) {
                            continue;
                        }
                        let e = by.entry(key.clone()).or_insert_with(|| new_entry(*pi));
                        for (i, k) in c2.keys.iter().enumerate() {
                            for (side, j) in [(&mut e.before, 0), (&mut e.after, 1)] {
                                let (n, line) = w.counts[i][j];
                                if n > 0 {
                                    let ms = rec_ms(catalog::minute_of(&c.marks, line), r);
                                    side.add(&k.text, n, if k.level == Level::Error { n } else { 0 }, Seen { ms, run: r.id(), line });
                                }
                            }
                        }
                    }
                    Err(err) => broken.entry(key.clone()).or_default().push(format!("r{} ({err})", r.id())),
                }
            }
            newest.insert(key, (r.id(), what(r)));
        }
        // (newest first, as `previous` lists them; across projects by run id, then project)
        let mut order: Vec<(usize, String)> = newest.keys().cloned().collect();
        order.sort_by(|x, y| newest[y].0.cmp(&newest[x].0).then(x.cmp(y)));
        for key in order {
            let label = newest[&key].1.clone();
            if let Some(b) = broken.get(&key) {
                // (a run of it that may hold records in the windows was not counted: its counts
                // would not be whole)
                not.push((label, format!("not counted: {}", b.join(", "))));
                continue;
            }
            let mut c = by.remove(&key).expect("a source with records in the windows");
            c.label = label;
            let runs = |s: &Side| if s.runs.is_empty() { "no run".to_string() } else { s.runs.iter().map(|r| format!("r{r}")).collect::<Vec<_>>().join(", ") };
            c.sides = format!("the window ({}) against the window before ({})", runs(&c.after), runs(&c.before));
            done.push(c);
        }
        let len = wb - wa;
        let len_text = if len % 60 == 0 { plural((len / 60) as u64, "hour") } else { plural(len as u64, "minute") };
        let head = format!("quiet: keys that fell to 0 or by 90% or more in {} against the {len_text} before it ({}) ({})", minute_span(wa, wb), minute_span(pa, pb), sc.desc);
        // (the next call names the window, so it goes on with the same minutes; times past
        // the years 0 to 9999 cannot be written so, and stay as given)
        let utc = |m: i64| {
            let t = times::utc_text(m * 60_000);
            format!("{}T{}Z", &t[..10], &t[11..19])
        };
        let year_ok = |m: i64| (0..253_402_300_800_000 / 60_000).contains(&m);
        let pins = if year_ok(wa) && year_ok(wb) { vec![("since", utc(wa)), ("until", utc(wb))] } else { Vec::new() };
        (head, sc, pins)
    };
    // sources with keys that fell, in order; the others in a line each kind
    let (with, without): (Vec<&Compared>, Vec<&Compared>) = done.iter().partition(|c| !c.quiet_keys().is_empty());
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    ans.must(&format!("{head}: {} compared, {} with keys that fell", plural(done.len() as u64, "source"), group(with.len() as u64)));
    for n in cats.notes() {
        ans.must(&n);
    }
    let names = |v: &[String]| {
        let more = if v.len() > 10 { format!(" and {} more", v.len() - 10) } else { String::new() };
        format!("{}{more}", v.iter().take(10).cloned().collect::<Vec<_>>().join(", "))
    };
    if !without.is_empty() {
        let v: Vec<String> = without.iter().map(|c| format!("{} ({})", c.label, c.status())).collect();
        ans.must(&format!("nothing fell: {}", names(&v)));
    }
    if !not.is_empty() {
        let v: Vec<String> = not.iter().map(|(l, why)| format!("{l} ({why})")).collect();
        ans.must(&format!("not compared: {}", names(&v)));
    }
    // the rows: per source with keys that fell, each key with its first line before
    let mut heads = Vec::new();
    let mut all: Vec<(usize, &[u8], u64, u64, Seen)> = Vec::new();
    for (si, c) in with.iter().enumerate() {
        heads.push(format!("== {}: {}: {}", c.label, c.sides, c.status()));
        for (t, nb, na, first) in c.quiet_keys() {
            all.push((si, t, nb, na, first));
        }
    }
    let total = all.len();
    let shown = all.get(skip..total.min(skip.saturating_add(limit))).unwrap_or(&[]);
    let mut want: BTreeMap<(usize, u64), Vec<(u64, u64)>> = BTreeMap::new();
    for x in shown {
        want.entry((with[x.0].pi, x.4.run)).or_default().push((x.4.line, x.4.line));
    }
    let lines = Lines::read(&sc.projects, &want);
    let mut rows = Vec::new();
    for x in shown {
        let mut row = Row { section: x.0, ..Row::default() };
        let l = lines.show(&sc.projects, with[x.0].pi, x.4.run, x.4.line, line_max, &mut row);
        row.lines.push(format!("  {} -> {}  {l}", group(x.2), group(x.3)));
        rows.push(row);
    }
    let s = show_rows(&mut ans, &heads, &rows, skip, total, limit);
    let cuts = cut_note(s.cuts, s.cut_ref.as_deref(), line_max);
    let last = match s.next {
        None => format!("all shown{cuts}"),
        Some(i) => {
            let left = &all[i..];
            let sources: HashSet<usize> = left.iter().map(|x| x.0).collect();
            let mut next = a.with_cursor(&format!("#{i}"));
            for (k, v) in &pins {
                next.set(k, v.as_str().into());
            }
            format!(
                "left out: {} of {} rows (#{i} on: keys of {}){cuts}. Next: {}",
                group(left.len() as u64),
                group(total as u64),
                plural(sources.len() as u64, "source"),
                next_call("quiet", &next)
            )
        }
    };
    Ok(ans.finish(&last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_and_sources() {
        // to 0, or by 90% or more: 10 -> 1 is told, 9 -> 1 (88.9%) is not
        assert!(fell(10, 1) && fell(10, 0) && fell(1, 0) && fell(100, 10));
        assert!(!fell(9, 1) && !fell(0, 0) && !fell(10, 2) && !fell(5, 5) && !fell(3, 4));
        let run = |s: &str| RunRec(crate::json::parse(s.as_bytes()).unwrap());
        let a = run(r#"{"id":1,"kind":"run","cmd":["make","test"],"exit":0}"#);
        let b = run(r#"{"id":2,"kind":"run","cmd":["make","test"],"exit":2,"source":"/x"}"#);
        let c = run(r#"{"id":3,"kind":"run","cmd":["make test"],"signal":15}"#);
        let d = run(r#"{"id":4,"kind":"add","source":"/w/app.log"}"#);
        assert_eq!(source_of(&a), source_of(&b));
        assert_ne!(source_of(&a), source_of(&c), "argv, not the text it joins to");
        assert_eq!(source_of(&d).as_deref(), Some("file /w/app.log"));
        assert!(passed(&a) && !failed(&a) && failed(&b) && failed(&c) && !passed(&c) && !passed(&d) && !failed(&d));
    }

    #[test]
    fn cursors_and_rows() {
        let j = crate::json::parse(br##"{"cursor":"#12"}"##).unwrap();
        assert_eq!(table_cursor(&Args(&j)), Ok(12));
        let j = crate::json::parse(br#"{"cursor":"r12"}"#).unwrap();
        assert!(table_cursor(&Args(&j)).is_err());
        let row = |section: usize, s: &str| Row { section, lines: vec![s.repeat(10)], ..Row::default() };
        let rows: Vec<Row> = (0..30).map(|i| row(i / 10, &format!("row {i:02} "))).collect();
        let heads = vec!["first".to_string(), "second".to_string(), "third".to_string()];
        // all of them, from row 5 on, in a big answer: each section's heading once
        let mut ans = Answer::new(20_000);
        let s = show_rows(&mut ans, &heads, &rows[5..], 5, 30, 500);
        assert!(s.next.is_none());
        let t = ans.finish("all shown");
        assert_eq!(t.matches("first\n").count() + t.matches("second\n").count() + t.matches("third\n").count(), 3);
        assert!(!t.contains("row 04") && t.contains("row 05") && t.contains("row 29"));
        // a small one: the rows that fit, then where to go on
        let mut ans = Answer::new(200);
        let s = show_rows(&mut ans, &heads, &rows, 0, 40, 500);
        let n = s.next.unwrap();
        assert!(n > 0 && n < 30);
        // the limit, and rows past those given
        let mut ans = Answer::new(20_000);
        assert_eq!(show_rows(&mut ans, &heads, &rows[..10], 0, 30, 10).next, Some(10));
        let mut ans = Answer::new(20_000);
        assert_eq!(show_rows(&mut ans, &heads, &rows[..10], 0, 30, 4).next, Some(4));
        // the first row always shows, however small the cap
        let big = vec![Row { lines: vec!["x".repeat(5000)], ..Row::default() }];
        let mut ans = Answer::new(200);
        assert_eq!(show_rows(&mut ans, &[], &big, 0, 2, 5).next, Some(1));
        assert!(ans.finish("").contains("xxxx"));
    }
}
