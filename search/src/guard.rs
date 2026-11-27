//! Guard notes and the fix memory.
//!
//! Guard notes are moments: a small timeline written as runs are indexed, never inside fewer's
//! saving, each moment with its exact time, count and a line with its reference.
//!
//! | Kind | When | Found by |
//! |---|---|---|
//! | new | a key's first record in the project | the registry merge |
//! | rare | a key seen before, with at most 3 records in the project so far | the registry merge |
//! | jump | a key has 20 or more records in one minute, and 10 or more times its median per active minute over the run's previous 60 minutes | the index pass |
//! | burst | 20 or more error records in one minute, after 5 minutes with fewer than 2 a minute | the index pass |
//! | failed | a run ended with an exit status other than 0, or a signal | the registry merge (runs.jsonl) |
//! | back | an error key that had stopped appears again | the registry merge |
//!
//! The rules, exactly:
//! - Minutes are the catalog's: a record is in the minute of its first line's time. Records
//!   without a clock time are in no minute, so they make no jump and no burst.
//! - jump: per key (its levels together), a minute m with 20 or more records, against the
//!   minutes m-60 to m-1 in which the key has records (its active minutes): 10 times their
//!   median (the middle count, or the mean of the two middle counts) is at most m's count,
//!   compared in whole numbers. A key without an active minute in that hour makes no jump.
//!   Minutes in a row that meet the rule are one jump, at the first.
//! - burst: a minute with 20 or more error records (every key), when each of the 5 minutes
//!   before it has fewer than 2 (minutes before the run's first line hold none). The minute
//!   after a burst minute is never a burst of its own.
//! - new, rare, back and failed are found run by run in the order of run ids (the order runs
//!   were saved): the moments of run N are what it gives against the runs before it.
//!
//! The fix memory: an error key's last record, by the order of runs, is its last-seen place
//! (the registry's `latest`). The key stopped at the first later run of the same source (the
//! same command's argv, or the same file) that holds no record of it and was not cut short (a
//! signal, a capture that stopped, a run without its status, a command that could not start).
//! A stop keeps where the key was last seen, and the stop run with its time (its first line's
//! time, else its save time). A record after a stop is a `back` moment and is kept with the
//! stop, and the key can stop again later. The `fixes` tool adds the commits in between, the
//! sessions of that time and the files they edited at answer time; nothing here names a cause.
//!
//! Where they are kept:
//! - the index pass's moments in each run's catalog (catalog.rs, `moments`);
//! - every moment in the project's moments.jsonl, one JSON object per line, run by run;
//! - the stops in the key registry (keys.cat), which also records moments.jsonl's length and
//!   CRC-32, so a moments.jsonl that is not the one written with it (a crash between the two
//!   writes, an edit) is seen and both are made again.
//!
//! Both files are derived: a run merged after a later one (a long `run` packed after a short
//! one), a damaged file, or a forget makes them again from every catalog, run by run, so they
//! always equal a fresh rebuild.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use crate::catalog::{self, Catalog, Key, Moment, Registry, Seen};
use crate::crc::crc32;
use crate::formats::Level;
use crate::json::{self, Json};
use crate::store::{self, RunRec};

/// The project's Guard notes.
pub const MOMENTS: &str = "moments.jsonl";

/// jump: at least this many records of a key in the minute,
pub const JUMP_MIN: u64 = 20;
/// and at least this many times its median per active minute
pub const JUMP_TIMES: u64 = 10;
/// over these minutes before it.
pub const JUMP_HOUR: i64 = 60;
/// burst: at least this many error records in the minute,
pub const BURST_MIN: u64 = 20;
/// after this many minutes
pub const BURST_QUIET: i64 = 5;
/// with fewer than this many error records each.
pub const BURST_QUIET_UNDER: u64 = 2;
/// rare: a key seen before with at most this many records in the project.
pub const RARE_MAX: u64 = 3;

/// A moment's kind: its name in moments.jsonl and in answers, and its byte in a catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    New,
    Rare,
    Jump,
    Burst,
    Failed,
    Back,
}

impl Kind {
    pub const ALL: [Kind; 6] = [Kind::New, Kind::Rare, Kind::Jump, Kind::Burst, Kind::Failed, Kind::Back];
    pub const NAMES: [&'static str; 6] = ["new", "rare", "jump", "burst", "failed", "back"];

    pub fn name(self) -> &'static str {
        Kind::NAMES[self as usize]
    }

    /// Its byte in a catalog's moments: 1 to 6.
    pub fn code(self) -> u8 {
        self as u8 + 1
    }

    pub fn of_code(c: u8) -> Option<Kind> {
        Kind::ALL.get((c as usize).wrapping_sub(1)).copied()
    }

    pub fn of_name(s: &str) -> Option<Kind> {
        Kind::NAMES.iter().position(|&n| n == s).map(|i| Kind::ALL[i])
    }
}

// ---------------------------------------------------------------- what runs.jsonl says

/// A run's source: its command (argv), else its file. Runs with the same source are runs of
/// one command or one file. None for a run with neither.
pub fn source_of(r: &RunRec) -> Option<String> {
    if let Some(c) = r.0.get("cmd") {
        return Some(format!("cmd {c}"));
    }
    r.s("source").map(|s| format!("file {s}"))
}

/// A file kept as it is, or not read: no log to count.
pub fn no_log(r: &RunRec) -> bool {
    matches!(r.kind(), "kept" | "not-read")
}

/// A command's run that failed: an exit status other than 0, or a signal.
pub fn failed(r: &RunRec) -> bool {
    r.i("signal").is_some() || r.i("exit").is_some_and(|e| e != 0)
}

/// A command's run that passed: exit status 0 and no signal.
pub fn passed(r: &RunRec) -> bool {
    r.i("signal").is_none() && r.i("exit") == Some(0)
}

/// A run cut short: ended by a signal, or its note says that its saving stopped, that it ended
/// without a status, or that its command could not start. Its missing records say nothing.
pub fn cut_short(r: &RunRec) -> bool {
    r.i("signal").is_some() || r.s("note").is_some_and(|n| n.contains("ended without a status") || n.contains("saving stopped") || n.starts_with("could not start"))
}

/// A key as answers and moments.jsonl show it: bytes that are not UTF-8, and control bytes
/// other than tab, as \xHH.
pub fn key_shown(text: &[u8]) -> String {
    crate::clean::escape(text, false).0
}

// ---------------------------------------------------------------- the index pass: jump, burst

/// A key's records per minute as the index pass counts them: (minute, records, the first one's
/// first line), a new entry whenever the minute changes. A log whose times go back gives a
/// minute twice; the moments add them up.
pub type Minutes = Vec<(i64, u64, u64)>;

/// Count a record of minute `m` whose first line is `line` (records come in line order).
pub fn count_minute(v: &mut Minutes, m: i64, line: u64) {
    match v.last_mut() {
        Some(e) if e.0 == m => e.1 += 1,
        _ => v.push((m, 1, line)),
    }
}

/// Minutes in order, each once: their records added up, the first line the smallest.
fn per_minute(mut v: Minutes) -> Minutes {
    v.sort_unstable_by_key(|e| (e.0, e.2));
    let mut out: Minutes = Vec::with_capacity(v.len());
    for e in v {
        match out.last_mut() {
            // (sorted: the first entry of a minute has its first line)
            Some(o) if o.0 == e.0 => o.1 += e.1,
            _ => out.push(e),
        }
    }
    out
}

/// The jumps of one key, from its records per minute (in order, each minute once): for each,
/// the index of its minute and the sum of the two middle counts of the key's active minutes in
/// the hour before (twice their median; the same count twice when there is one middle).
pub fn jumps(v: &[(i64, u64, u64)]) -> Vec<(usize, u64)> {
    let mut out = Vec::new();
    let mut from = 0;
    // the last minute that met the rule: minutes in a row are one jump
    let mut met: Option<i64> = None;
    let mut counts: Vec<u64> = Vec::with_capacity(JUMP_HOUR as usize);
    for (i, &(m, n, _)) in v.iter().enumerate() {
        // the hour before: minutes m-60 to m-1 (v is in order, so v[from..i])
        while v[from].0 < m.saturating_sub(JUMP_HOUR) {
            from += 1;
        }
        if n < JUMP_MIN || from == i {
            continue;
        }
        counts.clear();
        counts.extend(v[from..i].iter().map(|e| e.1));
        counts.sort_unstable();
        let k = counts.len();
        let two = counts[(k - 1) / 2] + counts[k / 2];
        // n >= 10 × median, in whole numbers: 2n >= 10 × (the two middle counts)
        if n.saturating_mul(2) >= JUMP_TIMES.saturating_mul(two) {
            if met != Some(m - 1) {
                out.push((i, two));
            }
            met = Some(m);
        }
    }
    out
}

/// The bursts of a run, from its error records per minute (minute -> error records, the first
/// one's first line, its catalog entry): (minute, error records, line, entry, the error records
/// of the 5 minutes before, each fewer than 2).
pub fn bursts(errors: &BTreeMap<i64, (u64, u64, u64)>) -> Vec<(i64, u64, u64, u64, u64)> {
    let mut out = Vec::new();
    for (&m, &(n, line, entry)) in errors {
        if n < BURST_MIN {
            continue;
        }
        let before: Vec<u64> = errors.range(m.saturating_sub(BURST_QUIET)..m).map(|e| e.1.0).collect();
        if before.iter().all(|&c| c < BURST_QUIET_UNDER) {
            out.push((m, n, line, entry, before.iter().sum()));
        }
    }
    out
}

/// The index pass's moments of a run (jump and burst), as its catalog keeps them, in line
/// order. `minutes` holds each catalog entry's records per minute, `errors` the run's error
/// records per minute ([`bursts`]). A jump names its key's first entry; a burst the entry of
/// its minute's first error record.
pub fn index_moments(keys: &[Key], minutes: &[Minutes], errors: &BTreeMap<i64, (u64, u64, u64)>) -> Vec<Moment> {
    let mut out = Vec::new();
    // each key text's entries (one per level), in order of first appearance
    let mut by: HashMap<&[u8], Vec<usize>> = HashMap::new();
    let mut order: Vec<&[u8]> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        by.entry(&k.text)
            .or_insert_with(|| {
                order.push(&k.text);
                Vec::new()
            })
            .push(i);
    }
    for text in order {
        let ids = &by[text];
        let v = per_minute(ids.iter().flat_map(|&i| minutes.get(i).into_iter().flatten().copied()).collect());
        for (j, two) in jumps(&v) {
            let (m, n, line) = v[j];
            out.push(Moment { kind: Kind::Jump.code(), minute: Some(m), key: ids[0] as u64, count: n, line, before: two });
        }
    }
    for (m, n, line, entry, before) in bursts(errors) {
        out.push(Moment { kind: Kind::Burst.code(), minute: Some(m), key: entry, count: n, line, before });
    }
    out.sort_by_key(|x| (x.line, x.kind));
    out
}

// ---------------------------------------------------------------- the registry merge

/// A stop of an error key: it was last seen at `last`, then run `run` of the
/// same source held no record of it (`ms`: that run's first line time, else its save time).
/// `back` is its first record after the stop, once it came back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stop {
    pub last: Seen,
    pub run: u64,
    pub ms: i64,
    pub back: Option<Seen>,
}

/// A moment of the project: a line of moments.jsonl.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// when, in Unix ms: the minute of its line (a record without a clock time: its run's save
    /// time), or for `failed` the run's end
    pub t: i64,
    pub kind: Kind,
    pub run: u64,
    /// its line: the first line of its record (new, rare, back), of the minute's first record
    /// (jump, burst), or the run's last line (failed; 0 when the run has no line)
    pub line: u64,
    /// its key as answers show it ([`key_shown`]); for a burst the key of its first record;
    /// empty for `failed`
    pub key: String,
    /// records: the key's in the run (new, rare, back) or in the minute (jump); error records
    /// in the minute (burst) or in the run (failed)
    pub count: u64,
    /// jump: twice its median per active minute in the hour before; burst: the error records of
    /// the 5 minutes before; rare: the key's records in the project before this run; else 0
    pub before: u64,
    /// rare: where the key was first seen; back: where it was last seen before it stopped
    pub seen: Option<Seen>,
    /// back: the stop, (the run without it, its time)
    pub stop: Option<(u64, i64)>,
    /// failed: its exit status, or the signal that ended it
    pub exit: Option<i64>,
    pub signal: Option<i64>,
    /// new and rare past the first LISTED of a run: how many keys this one moment counts (its
    /// count is their records, its line the first of them); 0 for a moment of one key
    pub more: u64,
}

/// At most this many `new` and this many `rare` moments of one run are listed one by one, in
/// order of first appearance; the rest of each kind are one moment that counts them, so a log
/// of random IDs cannot fill moments.jsonl (made-up logs once made 37 MB of it).
pub const LISTED: usize = 100;

impl Note {
    fn new(kind: Kind, at: Seen, key: String, count: u64) -> Note {
        Note { t: at.ms, kind, run: at.run, line: at.line, key, count, before: 0, seen: None, stop: None, exit: None, signal: None, more: 0 }
    }

    /// A median given as twice its value: "2", "2.5".
    pub fn median(two: u64) -> String {
        if two % 2 == 0 { (two / 2).to_string() } else { format!("{}.5", two / 2) }
    }

    /// Its note in moments.jsonl: what it is, in plain words (the answers name the references).
    fn text(&self) -> String {
        let records = |n: u64| if n == 1 { "1 record".to_string() } else { format!("{n} records") };
        match self.kind {
            Kind::New if self.more > 0 => format!("{} more keys first seen in the project, not listed one by one: {} in this run", self.more, records(self.count)),
            Kind::Rare if self.more > 0 => format!("{} more rare keys, not listed one by one: {} in this run", self.more, records(self.count)),
            Kind::New => format!("first seen in the project: {} in this run", records(self.count)),
            Kind::Rare => format!("{} in this run; seen before: {} in the project", records(self.count), records(self.before)),
            Kind::Jump => format!("{} in one minute, 10 or more times its median per active minute in the hour before ({})", records(self.count), Note::median(self.before)),
            Kind::Burst => format!("{} error records in one minute, after 5 minutes with fewer than 2 a minute ({} in all)", self.count, self.before),
            Kind::Failed => match (self.exit, self.signal) {
                (_, Some(s)) => format!("the run ended with signal {s}"),
                (Some(e), _) => format!("the run ended with exit status {e}"),
                _ => "the run failed".to_string(),
            },
            Kind::Back => format!("{} in this run after it had stopped in run {}", records(self.count), self.stop.map_or(0, |s| s.0)),
        }
    }

    pub fn to_json(&self) -> Json {
        let mut o = Json::o(vec![
            ("t", Json::from(self.t)),
            ("kind", self.kind.name().into()),
            ("run", Json::from(self.run)),
            ("line", Json::from(self.line)),
            ("key", self.key.as_str().into()),
            ("count", Json::from(self.count)),
            ("note", self.text().into()),
        ]);
        if matches!(self.kind, Kind::Jump | Kind::Burst | Kind::Rare) {
            o.set("before", Json::from(self.before));
        }
        if let Some(s) = self.seen {
            o.set("seen", Json::o(vec![("t", Json::from(s.ms)), ("run", Json::from(s.run)), ("line", Json::from(s.line))]));
        }
        if let Some((run, t)) = self.stop {
            o.set("stop", Json::o(vec![("t", Json::from(t)), ("run", Json::from(run))]));
        }
        if let Some(e) = self.exit {
            o.set("exit", Json::from(e));
        }
        if let Some(s) = self.signal {
            o.set("signal", Json::from(s));
        }
        if self.more > 0 {
            o.set("more", Json::from(self.more));
        }
        o
    }

    pub fn of_json(j: &Json) -> Option<Note> {
        let u = |k: &str| j.get(k).and_then(Json::int).and_then(|v| u64::try_from(v).ok());
        let seen = match j.get("seen") {
            Some(s) => Some(Seen { ms: s.get("t")?.int()?, run: s.get("run")?.int()? as u64, line: s.get("line")?.int()? as u64 }),
            None => None,
        };
        let stop = match j.get("stop") {
            Some(s) => Some((s.get("run")?.int()? as u64, s.get("t")?.int()?)),
            None => None,
        };
        Some(Note {
            t: j.get("t")?.int()?,
            kind: Kind::of_name(j.get("kind")?.str()?)?,
            run: u("run")?,
            line: u("line")?,
            key: j.get("key")?.str()?.to_string(),
            count: u("count")?,
            before: u("before").unwrap_or(0),
            seen,
            stop,
            exit: j.get("exit").and_then(Json::int),
            signal: j.get("signal").and_then(Json::int),
            more: u("more").unwrap_or(0),
        })
    }
}

/// The moments of run `r` (its catalog `cat`) against `reg`, the registry of the runs before
/// it, in line order; the stops it makes are written into `reg`'s keys.
/// `src_of` gives a run's source by its id. The caller merges the run into `reg` afterwards.
pub fn step(reg: &mut Registry, r: &RunRec, cat: &Catalog, src_of: &dyn Fn(u64) -> Option<String>) -> Vec<Note> {
    let id = r.id();
    let start = r.i("start").unwrap_or(0);
    // a record's place as the registry takes it: its minute, else the run's save time
    let at = |minute: Option<i64>, line: u64| Seen { ms: minute.map_or(start, |m| m.saturating_mul(60_000)), run: id, line };
    // the run's keys by text (levels together), in order of first appearance: records, first
    let mut keys: Vec<(&[u8], u64, Seen)> = Vec::new();
    let mut here: HashMap<&[u8], usize> = HashMap::new();
    for k in &cat.keys {
        let first = at(k.first_minute, k.first_line);
        match here.get(&k.text[..]) {
            Some(&i) => {
                keys[i].1 += k.records;
                if first.line < keys[i].2.line {
                    keys[i].2 = first;
                }
            }
            None => {
                here.insert(&k.text, keys.len());
                keys.push((&k.text, k.records, first));
            }
        }
    }
    let mut notes = Vec::new();
    // past the first LISTED new and rare keys, one moment of each kind counts the rest
    let (mut listed_new, mut listed_rare) = (0usize, 0usize);
    // (the first of them, how many keys, their records)
    type More = Option<(Seen, u64, u64)>;
    let (mut more_new, mut more_rare): (More, More) = (None, None);
    let count_more = |m: &mut More, at: Seen, n: u64| match m {
        Some(x) => {
            x.1 += 1;
            x.2 += n;
        }
        None => *m = Some((at, 1, n)),
    };
    for &(text, n, first) in &keys {
        match reg.keys.get_mut(text) {
            None if listed_new < LISTED => {
                listed_new += 1;
                notes.push(Note::new(Kind::New, first, key_shown(text), n));
            }
            None => count_more(&mut more_new, first, n),
            Some(k) => {
                if k.records <= RARE_MAX {
                    if listed_rare < LISTED {
                        listed_rare += 1;
                        notes.push(Note { before: k.records, seen: Some(k.first), ..Note::new(Kind::Rare, first, key_shown(text), n) });
                    } else {
                        count_more(&mut more_rare, first, n);
                    }
                }
                if let Some(s) = k.stops.last_mut()
                    && s.back.is_none()
                {
                    s.back = Some(first);
                    notes.push(Note { seen: Some(s.last), stop: Some((s.run, s.ms)), ..Note::new(Kind::Back, first, key_shown(text), n) });
                }
            }
        }
    }
    for (kind, m) in [(Kind::New, more_new), (Kind::Rare, more_rare)] {
        if let Some((at, keys, records)) = m {
            let what = if kind == Kind::New { "new" } else { "rare" };
            notes.push(Note { more: keys, ..Note::new(kind, at, format!("({keys} more {what} keys in this run, not listed one by one)"), records) });
        }
    }
    // stops: the error keys last seen in a run of this run's source, with no record here
    if let Some(src) = source_of(r)
        && !cut_short(r)
    {
        let ms = cat.first_ms.unwrap_or(start);
        for (text, k) in reg.keys.iter_mut() {
            let stopped = k.stops.last().is_some_and(|s| s.back.is_none());
            if !k.error || stopped || k.latest.run >= id || here.contains_key(&text[..]) {
                continue;
            }
            if src_of(k.latest.run).as_deref() == Some(src.as_str()) {
                k.stops.push(Stop { last: k.latest, run: id, ms, back: None });
            }
        }
    }
    if failed(r) {
        let errors = cat.keys.iter().filter(|k| k.level == Level::Error).map(|k| k.records).sum();
        let end = Seen { ms: r.i("end").unwrap_or(start), run: id, line: cat.lines };
        notes.push(Note { exit: r.i("exit"), signal: r.i("signal"), ..Note::new(Kind::Failed, end, String::new(), errors) });
    }
    // the index pass's moments, from the catalog
    for m in &cat.moments {
        let Some(kind) = Kind::of_code(m.kind) else { continue };
        let key = cat.keys.get(m.key as usize).map(|k| key_shown(&k.text)).unwrap_or_default();
        notes.push(Note { before: m.before, ..Note::new(kind, at(m.minute, m.line), key, m.count) });
    }
    notes.sort_by(|a, b| a.line.cmp(&b.line).then(a.kind.cmp(&b.kind)));
    notes
}

/// The registry with its stops, and every moment, made from runs' catalogs run by run in the
/// order of run ids. `cats` holds the runs with a catalog (any order); `runs` every run of the
/// project (their sources).
pub fn derive(runs: &[RunRec], cats: &[(&RunRec, &Catalog)]) -> (Registry, Vec<Note>) {
    let src: HashMap<u64, Option<String>> = runs.iter().map(|r| (r.id(), source_of(r))).collect();
    let src_of = |id: u64| src.get(&id).cloned().flatten();
    let mut order: Vec<&(&RunRec, &Catalog)> = cats.iter().collect();
    order.sort_by_key(|(r, _)| r.id());
    let mut reg = Registry::default();
    let mut notes = Vec::new();
    for (r, c) in order {
        notes.extend(step(&mut reg, r, c, &src_of));
        reg.merge(r.id(), r.i("start").unwrap_or(0), c);
    }
    (reg, notes)
}

// ---------------------------------------------------------------- the files

/// moments.jsonl's text: one JSON object per moment, each on its line.
pub fn notes_text(notes: &[Note]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in notes {
        out.extend_from_slice(n.to_json().to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

/// The moments of moments.jsonl's text (None when a line is not one).
pub fn read_notes(text: &[u8]) -> Option<Vec<Note>> {
    let mut v = Vec::new();
    for line in text.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        v.push(Note::of_json(&json::parse(line).ok()?)?);
    }
    Some(v)
}

/// What keys.cat records of the moments.jsonl written with it: its length and CRC-32.
pub fn marker(text: &[u8]) -> (u64, u32) {
    (text.len() as u64, crc32(text))
}

/// A project's registry and Guard notes as stored, when moments.jsonl is the one keys.cat was
/// written with and keys.cat holds exactly the runs `want` (ids, in order). None when they are
/// missing, damaged or behind.
pub fn stored(dir: &Path, want: &[u64]) -> Option<(Registry, Vec<Note>)> {
    let reg = Registry::read(dir)?;
    if reg.runs != want {
        return None;
    }
    let text = fs::read(dir.join(MOMENTS)).ok()?;
    if marker(&text) != reg.moments {
        return None;
    }
    Some((reg, read_notes(&text)?))
}

/// The registry and Guard notes made again from the catalogs on disk: every ready run's whose
/// catalog fits its archive, and `extra` (a run being merged, not ready yet).
fn from_disk(dir: &Path, runs: &[RunRec], extra: Option<(&RunRec, &Catalog)>) -> (Registry, Vec<Note>) {
    let skip = extra.map(|e| e.0.id());
    let loaded: Vec<(&RunRec, Catalog)> = runs
        .iter()
        .filter(|r| r.state() == "ready" && !no_log(r) && Some(r.id()) != skip)
        .filter_map(|r| catalog::load(&dir.join(catalog::cat_file(r.id())), &dir.join(store::run_file(r.id())), true).ok().map(|c| (r, c)))
        .collect();
    let mut cats: Vec<(&RunRec, &Catalog)> = loaded.iter().map(|(r, c)| (*r, c)).collect();
    cats.extend(extra);
    derive(runs, &cats)
}

/// The registry and moments.jsonl's text once run `id` (its catalog `cat`, saved at
/// `start_ms`) is merged in; the caller holds the project lock and writes them. The stored ones
/// with this run's moments added, when they are whole, hold the ready runs with a catalog, and
/// every run in them is older; else made again from every catalog and this one. None when the
/// run is merged already.
pub fn merged(dir: &Path, runs: &[RunRec], id: u64, start_ms: i64, cat: &Catalog) -> Option<(Registry, Vec<u8>)> {
    let me = runs.iter().find(|r| r.id() == id).cloned().unwrap_or_else(|| RunRec(Json::o(vec![("id", Json::from(id)), ("start", Json::from(start_ms))])));
    if let Some(mut reg) = Registry::read(dir)
        && let Ok(text) = fs::read(dir.join(MOMENTS))
        && marker(&text) == reg.moments
    {
        if reg.runs.binary_search(&id).is_ok() {
            return None;
        }
        // (the runs in it are ready, and a ready run outside it has no catalog that fits)
        let ready = |r: &RunRec| r.state() == "ready" && !no_log(r);
        let whole = reg.runs.iter().all(|x| runs.iter().any(|r| r.id() == *x && ready(r)))
            && runs.iter().filter(|r| ready(r) && r.id() != id && reg.runs.binary_search(&r.id()).is_err()).all(|r| catalog::load(&dir.join(catalog::cat_file(r.id())), &dir.join(store::run_file(r.id())), false).is_err());
        if whole && reg.runs.last().is_none_or(|&l| l < id) {
            let src: HashMap<u64, Option<String>> = runs.iter().map(|r| (r.id(), source_of(r))).collect();
            let notes = step(&mut reg, &me, cat, &|x| src.get(&x).cloned().flatten());
            reg.merge(id, start_ms, cat);
            let mut text = text;
            text.extend(notes_text(&notes));
            return Some((reg, text));
        }
    }
    let (reg, notes) = from_disk(dir, runs, Some((&me, cat)));
    Some((reg, notes_text(&notes)))
}

/// Make keys.cat and moments.jsonl again from the ready runs' catalogs, under the project lock.
#[cfg(test)]
pub fn rebuild(p: &store::Project) -> std::io::Result<()> {
    let _l = store::Lock::take(&p.dir)?;
    rebuild_locked(&p.dir)
}

/// Make keys.cat and moments.jsonl again from the ready runs' catalogs, when the caller holds
/// the project lock (`memory forget`).
pub fn rebuild_locked(dir: &Path) -> std::io::Result<()> {
    let runs = store::read_runs(dir);
    let (reg, notes) = from_disk(dir, &runs, None);
    write(dir, reg, &notes_text(&notes))
}

/// Write moments.jsonl, then keys.cat with its marker (the project lock is held). A crash
/// between the two leaves a keys.cat whose marker does not fit: the next merge makes both again.
pub fn write(dir: &Path, mut reg: Registry, text: &[u8]) -> std::io::Result<()> {
    reg.moments = marker(text);
    store::write_atomic(&dir.join(MOMENTS), text)?;
    store::write_atomic(&dir.join(catalog::REGISTRY), &reg.encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Builder, Tags};
    use crate::times::Zone;

    fn v(m: &[(i64, u64)]) -> Minutes {
        m.iter().enumerate().map(|(i, &(m, n))| (m, n, 100 + i as u64)).collect()
    }

    fn minutes_of(j: &[(usize, u64)], v: &Minutes) -> Vec<(i64, u64)> {
        j.iter().map(|&(i, two)| (v[i].0, two)).collect()
    }

    /// The jump rule's edges: 20 records, 10 times the median (odd and even counts of active
    /// minutes), the hour before, minutes in a row, no active minute before.
    #[test]
    fn jump_rule() {
        // 2 a minute for an hour, then 20 (20 >= 10 × 2): a jump; 19 never is
        let mut a: Vec<(i64, u64)> = (0..60).map(|m| (m, 2)).collect();
        a.push((60, 20));
        let x = v(&a);
        assert_eq!(minutes_of(&jumps(&x), &x), vec![(60, 4)]);
        a.pop();
        a.push((60, 19));
        assert!(jumps(&v(&a)).is_empty());
        // median of 2 and 3 is 2.5: 25 is a jump, 24 is not (2 × 24 = 48 < 10 × 5)
        for (n, want) in [(25, true), (24, false)] {
            let x = v(&[(30, 2), (31, 3), (40, n)]);
            assert_eq!(!jumps(&x).is_empty(), want, "{n}");
        }
        // the hour before is m-60 to m-1: minute 10 counts for minute 70, minute 9 does not
        let x = v(&[(10, 5), (70, 50)]);
        assert_eq!(minutes_of(&jumps(&x), &x), vec![(70, 10)]);
        assert!(jumps(&v(&[(9, 5), (70, 50)])).is_empty(), "no active minute in the hour before");
        assert!(jumps(&v(&[(0, 500)])).is_empty(), "the first minute");
        // minutes in a row are one jump; a gap starts another
        let mut a: Vec<(i64, u64)> = (0..45).map(|m| (m, 2)).collect();
        a.extend([(50, 60), (51, 60), (52, 70), (54, 25)]);
        let x = v(&a);
        assert_eq!(minutes_of(&jumps(&x), &x), vec![(50, 4), (54, 4)]);
        // not 10 times: 25 against a median of 3
        let mut a: Vec<(i64, u64)> = (0..60).map(|m| (m, 3)).collect();
        a.push((65, 25));
        assert!(jumps(&v(&a)).is_empty());
    }

    /// The burst rule's edges: 20 error records, fewer than 2 in each of the 5 minutes before,
    /// the minute after a burst, the run's start.
    #[test]
    fn burst_rule() {
        let e = |m: &[(i64, u64)]| -> BTreeMap<i64, (u64, u64, u64)> { m.iter().map(|&(m, n)| (m, (n, m as u64 * 10, 0))).collect() };
        let at = |b: Vec<(i64, u64, u64, u64, u64)>| b.iter().map(|x| (x.0, x.1, x.4)).collect::<Vec<_>>();
        assert_eq!(at(bursts(&e(&[(10, 20)]))), vec![(10, 20, 0)], "the run's first minute");
        assert!(bursts(&e(&[(10, 19)])).is_empty());
        assert_eq!(at(bursts(&e(&[(5, 1), (7, 1), (10, 20), (11, 30)]))), vec![(10, 20, 2)], "1 a minute is quiet; the minute after is not a burst");
        assert!(bursts(&e(&[(7, 2), (10, 20)])).is_empty(), "2 in a minute before");
        assert_eq!(at(bursts(&e(&[(4, 9), (10, 25)]))), vec![(10, 25, 0)], "minute 4 is not one of the 5 before");
        assert_eq!(at(bursts(&e(&[(5, 9), (10, 25), (20, 21)]))), vec![(20, 21, 0)], "minute 5 is");
    }

    fn cat_of(text: &str) -> Catalog {
        let mut b = Builder::new(&Tags { name: "app.log".into(), zone: Some(Zone::Fixed(0)), ..Tags::default() }, 1_795_003_200_000);
        b.feed(text.as_bytes());
        b.finish()
    }

    // 2026-11-17 10:00 UTC in minutes
    const BASE: i64 = 1_794_909_600_000 / 60_000;

    fn stamp(m: i64, s: u64) -> String {
        let (y, mo, d, h, mi, _) = crate::times::civil(m * 60);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }

    /// The index pass gives a run's jumps and bursts with their exact minute, count, line and
    /// key, also for a key with two levels and for a stack trace (one record).
    #[test]
    fn moments_of_the_index_pass() {
        let mut t = String::new();
        let mut line = 0u64;
        let mut push = |t: &mut String, s: String| {
            t.push_str(&s);
            t.push('\n');
            line += 1;
            line
        };
        for m in 0..30 {
            push(&mut t, format!("{} INFO GET /items took {} ms", stamp(BASE + m, 1), m));
            push(&mut t, format!("{} INFO GET /items took {} ms", stamp(BASE + m, 2), m));
        }
        // minute 30: 25 records of the key (2 a minute before: 25 >= 10 × 2)
        let mut jump_line = 0;
        for s in 0..25 {
            let l = push(&mut t, format!("{} INFO GET /items took {} ms", stamp(BASE + 30, s), s));
            if s == 0 {
                jump_line = l;
            }
        }
        // minute 40: 19 single-line errors and a stack trace (one error record): 20, a burst
        let mut burst_line = 0;
        for s in 0..19 {
            let l = push(&mut t, format!("{} ERROR job {s} failed", stamp(BASE + 40, s)));
            if s == 0 {
                burst_line = l;
            }
        }
        push(&mut t, format!("{} ERROR crash", stamp(BASE + 40, 30)));
        push(&mut t, "\tat a.B.c(B.java:1)".into());
        let c = cat_of(&t);
        let key = c.keys.iter().position(|k| k.text == b"<time> INFO GET <path> took <num> ms").unwrap() as u64;
        let job = c.keys.iter().position(|k| k.text == b"<time> ERROR job <num> failed").unwrap() as u64;
        assert_eq!(
            c.moments,
            vec![
                Moment { kind: Kind::Jump.code(), minute: Some(BASE + 30), key, count: 25, line: jump_line, before: 4 },
                Moment { kind: Kind::Burst.code(), minute: Some(BASE + 40), key: job, count: 20, line: burst_line, before: 0 },
            ]
        );
    }

    fn run(j: &str) -> RunRec {
        RunRec(json::parse(j.as_bytes()).unwrap())
    }

    /// new, rare, back, failed and the stops, run by run: a stop needs a later run of the same
    /// source without the key that was not cut short; a key comes back once per stop.
    #[test]
    fn steps_and_stops() {
        let err = "2026-11-17T10:00:00Z ERROR db: connection refused\n";
        let ok = "2026-11-17T10:00:00Z INFO all good\n";
        let rare = "2026-11-17T10:00:00Z WARN rare thing\n";
        let texts = [
            (r#"{"id":1,"kind":"run","cmd":["t"],"exit":0,"start":1}"#, format!("{ok}{err}{rare}")),
            (r#"{"id":2,"kind":"add","source":"/w/other.log","start":2}"#, ok.to_string()),
            (r#"{"id":3,"kind":"run","cmd":["t"],"signal":2,"start":3}"#, ok.to_string()),
            (r#"{"id":4,"kind":"run","cmd":["t"],"exit":1,"start":4}"#, format!("{ok}{rare}")),
            (r#"{"id":5,"kind":"run","cmd":["t"],"exit":0,"start":5}"#, ok.to_string()),
            (r#"{"id":6,"kind":"run","cmd":["t"],"exit":1,"start":6}"#, format!("{err}{err}")),
            (r#"{"id":7,"kind":"run","cmd":["t"],"exit":0,"start":7}"#, ok.to_string()),
        ];
        let runs: Vec<RunRec> = texts.iter().map(|(j, _)| run(j)).collect();
        let cats: Vec<Catalog> = texts.iter().map(|(_, t)| cat_of(t)).collect();
        let pairs: Vec<(&RunRec, &Catalog)> = runs.iter().zip(cats.iter()).collect();
        let (reg, notes) = derive(&runs, &pairs);
        let got: Vec<(u64, &str, u64)> = notes.iter().map(|n| (n.run, n.kind.name(), n.count)).collect();
        assert_eq!(
            got,
            vec![
                (1, "new", 1),
                (1, "new", 1),
                (1, "new", 1),
                (2, "rare", 1),
                (3, "rare", 1),
                (3, "failed", 0),
                (4, "rare", 1),
                (4, "rare", 1),
                (4, "failed", 0),
                (6, "rare", 2),
                (6, "back", 2),
                (6, "failed", 2),
            ]
        );
        // the error stopped at run 4: run 2 is another source, run 3 was cut short (a signal)
        let k = &reg.keys[&b"<time> ERROR db: connection refused"[..]];
        assert_eq!(k.stops.len(), 2);
        assert_eq!((k.stops[0].last.run, k.stops[0].last.line, k.stops[0].run, k.stops[0].back.map(|b| b.run)), (1, 2, 4, Some(6)));
        assert_eq!((k.stops[1].last.run, k.stops[1].run, k.stops[1].back), (6, 7, None));
        let back = notes.iter().find(|n| n.kind == Kind::Back).unwrap();
        assert_eq!((back.seen.map(|s| (s.run, s.line)), back.stop.map(|s| s.0), back.line), (Some((1, 2)), Some(4), 1));
        // the rare key's notes say how many records came before
        let rares: Vec<u64> = notes.iter().filter(|n| n.kind == Kind::Rare && n.key.contains("rare thing")).map(|n| n.before).collect();
        assert_eq!(rares, vec![1]);
        // in any order of the pairs, the same; moments.jsonl reads back as written
        let mut rev = pairs.clone();
        rev.reverse();
        let (reg2, notes2) = derive(&runs, &rev);
        assert_eq!((reg2.encode(), &notes2), (reg.encode(), &notes));
        assert_eq!(read_notes(&notes_text(&notes)).unwrap(), notes);
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Runs merged out of order (a long run packed after a short one), and a damaged
    /// moments.jsonl, give the same keys.cat and moments.jsonl as runs merged in order.
    #[test]
    fn merges_in_any_order_give_the_same_files() {
        let err = |n: u64| format!("2026-11-17T10:0{n}:00Z ERROR worker {n} failed\n2026-11-17T10:0{n}:01Z ERROR db: timeout\n");
        let texts: Vec<String> = vec![err(1), "2026-11-17T10:02:00Z INFO fine\n".into(), err(3), "2026-11-17T10:04:00Z INFO fine\n".into(), err(5)];
        let mut files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for order in [vec![1u64, 2, 3, 4, 5], vec![2, 1, 4, 3, 5], vec![5, 4, 3, 2, 1], vec![3, 1, 2, 5, 4]] {
            let home = tmpdir(&format!("guard-order-{}", order.iter().map(|x| x.to_string()).collect::<String>()));
            let p = store::ensure_project(&home, Path::new("/w/app")).unwrap();
            for (i, t) in texts.iter().enumerate() {
                let id = store::new_run(&p, Json::o(vec![("kind", "run".into()), ("state", "packing".into()), ("start", Json::from(i as i64)), ("cmd", Json::Arr(vec!["t".into()]))])).unwrap();
                assert_eq!(id, i as u64 + 1);
                // an archive stand-in: a catalog fits a file of its length and last 4 bytes
                let fake = format!("archive of run {id}, {t}");
                fs::write(p.dir.join(store::run_file(id)), &fake).unwrap();
                let mut c = cat_of(t);
                c.archive_bytes = fake.len() as u64;
                c.end_crc = fake.as_bytes()[fake.len() - 4..].try_into().unwrap();
                fs::write(p.dir.join(catalog::cat_file(id)), c.encode()).unwrap();
            }
            for &id in &order {
                let c = catalog::load(&p.dir.join(catalog::cat_file(id)), &p.dir.join(store::run_file(id)), true).unwrap();
                catalog::merge_registry(&p, id, id as i64 - 1, &c).unwrap();
                store::update_run(&p, id, Json::o(vec![("state", "ready".into())])).unwrap();
            }
            let got = (fs::read(p.dir.join(catalog::REGISTRY)).unwrap(), fs::read(p.dir.join(MOMENTS)).unwrap());
            if let Some(first) = files.first() {
                assert_eq!(&got, first, "order {order:?}");
            }
            files.push(got);
            // stored notes are read back; a changed moments.jsonl is seen, and the next merge
            // (or a rebuild) makes both again
            let ids: Vec<u64> = (1..=5).collect();
            assert!(stored(&p.dir, &ids).is_some());
            assert!(stored(&p.dir, &ids[..4]).is_none(), "behind");
            let m = p.dir.join(MOMENTS);
            let good = fs::read(&m).unwrap();
            fs::write(&m, [&good[..], b"\n"].concat()).unwrap();
            assert!(stored(&p.dir, &ids).is_none());
            rebuild(&p).unwrap();
            assert_eq!(fs::read(&m).unwrap(), good);
            assert!(stored(&p.dir, &ids).is_some());
            fs::remove_dir_all(&home).unwrap();
        }
        // the error keys stop at runs 2 and 4 and come back in runs 3 and 5 (rare then too)
        let notes = read_notes(&files[0].1).unwrap();
        let got: Vec<(u64, &str)> = notes.iter().map(|n| (n.run, n.kind.name())).collect();
        let want = [(1, "new"), (1, "new"), (2, "new"), (3, "rare"), (3, "back"), (3, "rare"), (3, "back"), (4, "rare"), (5, "rare"), (5, "back"), (5, "rare"), (5, "back")];
        assert_eq!(got, want);
        let reg = Registry::decode(&files[0].0).unwrap();
        let stops: Vec<(u64, u64, Option<u64>)> = reg.keys[&b"<time> ERROR db: timeout"[..]].stops.iter().map(|s| (s.last.run, s.run, s.back.map(|b| b.run))).collect();
        assert_eq!(stops, vec![(1, 2, Some(3)), (3, 4, Some(5))]);
    }

    /// Past the first LISTED new keys (and rare keys) of a run, one moment counts the rest: its
    /// count is their records, `more` how many keys, its line the first of them.
    #[test]
    fn many_new_keys_are_counted_not_listed() {
        // 250 keys, each its own words (letters only, so no part of them is a value)
        let word = |mut i: usize| {
            let mut w = String::new();
            for _ in 0..3 {
                w.push((b'a' + (i % 26) as u8) as char);
                i /= 26;
            }
            w
        };
        let mut text = String::new();
        for i in 0..250 {
            text.push_str(&format!("2026-11-17T10:00:00Z INFO event {} {}\n", word(i), word(i * 7 + 3)));
        }
        let r = run(r#"{"id":1,"kind":"add","source":"/w/a.log","start":1}"#);
        let c = cat_of(&text);
        let (_, notes) = derive(std::slice::from_ref(&r), &[(&r, &c)]);
        let new: Vec<&Note> = notes.iter().filter(|n| n.kind == Kind::New).collect();
        assert_eq!(new.len(), LISTED + 1);
        let last = new.iter().find(|n| n.more > 0).unwrap();
        assert_eq!((last.more, last.count, last.line), (250 - LISTED as u64, 250 - LISTED as u64, LISTED as u64 + 1));
        assert!(last.key.starts_with("(150 more new keys"));
        // it reads back from moments.jsonl as it was written
        let line = notes_text(std::slice::from_ref(last));
        let back = Note::of_json(&json::parse(&line[..line.len() - 1]).unwrap()).unwrap();
        assert_eq!(&back, *last);
        assert!(String::from_utf8_lossy(&line).contains("\"more\":150"));
    }
}
