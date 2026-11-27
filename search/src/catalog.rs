//! The catalog: the index pass over a run's text, the run's catalog file
//! (rNNNNNN.cat), and the project's key registry (keys.cat).
//!
//! The index pass reads a run's text in order, on one thread, after fewer-pack has packed it (in
//! `add`, and in pack-pending), so saving speed is untouched. For each line: its clean text
//! (formats::clean_line), its fields (formats.rs, by the source's log type), its time (its own,
//! else the previous line's, else its arrival time), its record (records.rs), and its key
//! (msgkey.rs, rule v1). A record counts once, under the key of its key line.
//!
//! What a catalog holds, every number exact:
//! - the head: lines (as grep counts them), records, text bytes, the earliest and the latest
//!   line time, the log type and the zone's offset;
//! - marks: each line where the time's minute changes, so every line's time is known to the
//!   minute (the counts per hour of `search` and `count`);
//! - keys: per key and level, in order of first appearance: records, lines, the first and last
//!   record's first line and minute, records per hour, sample records, and the first record's
//!   first and last line (a whole stack trace);
//! - the run's moments (Guard notes, guard.rs: jumps and bursts, found from each key's records
//!   per minute and the error records per minute), stderr lines (`run` in pipes mode)
//!   and the files a session edited.
//!
//! A record's level is its key line's level, or error when the record is a multi-line error
//! record (a stack trace or a crash report). A key's records can have different levels (an
//! access log's 5xx and 2xx lines share a key), so each (key, level) pair is an entry of its
//! own: counts by level stay exact. Across catalogs, and in the registry, keys are matched by
//! their text.
//!
//! A catalog fits one archive only: it holds the archive's length and its last 4 bytes (the
//! end record's CRC over every segment), and is refused for any other archive. It is written
//! by the store's writers only (add, pack-pending); the tools build a missing or stale one in
//! memory and never write it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::container::{get_varint, put_varint, unzz, zz};
use crate::crc::crc32;
use crate::formats::{self, Level, Profile};
use crate::guard;
use crate::json::Json;
use crate::msgkey;
use crate::records::Records;
use crate::store::{self, Project, RunRec};
use crate::times::{self, Carry, Clock, Stamp, Zone};

// version 2: catalogs hold the run's moments, and the registry the fix memory's stops;
// files of version 1 are refused, and made again
const CAT_MAGIC: &[u8; 5] = b"CMLC\x02";
const REG_MAGIC: &[u8; 5] = b"CMLK\x02";

/// A key keeps the first lines of this many records as samples when it has at most this many
/// records; else the first [`SAMPLE_HEAD`] and the last [`SAMPLE_TAIL`].
pub const SAMPLE_ALL: u64 = 16;
pub const SAMPLE_HEAD: usize = 8;
pub const SAMPLE_TAIL: usize = 4;

/// A run's catalog file name: r000042.cat.
pub fn cat_file(id: u64) -> String {
    format!("r{id:06}.cat")
}

/// The registry's file name in a project's folder.
pub const REGISTRY: &str = "keys.cat";

// ---------------------------------------------------------------- what a catalog holds

/// What the index pass needs besides the text.
#[derive(Clone, Debug, Default)]
pub struct Tags {
    /// The source's name: the file saved (its path), or the command; its log type is named
    /// from it and the text's first 64 KiB (formats::detect).
    pub name: String,
    /// The file's time in Unix ms, for dates written without a year. None: the time the
    /// archive records (the saved file's modification time).
    pub file_ms: Option<i64>,
    /// How times written without a zone are read; None: this computer's zone.
    pub zone: Option<Zone>,
    /// A `run` capture's arrival marks (rN.times): the run's start in Unix ms, and
    /// (line, ms since the start) pairs in order, lines counted from 1 as references count
    /// them. A line without a time of its own and after no line with one gets the start plus
    /// the last mark at or before it.
    pub arrivals: Option<(i64, Vec<(u64, u64)>)>,
    /// `run` in pipes mode: the stderr lines (rN.err), as (first line, number of lines).
    pub errs: Vec<(u64, u64)>,
    /// Sessions: the files the session edited, each with the line of its edit.
    pub files: Vec<(String, u64)>,
}

/// A line where the time's minute changes: from this line on, lines are in this minute
/// (minutes since 1970, UTC), or have no clock time (None) until the next mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub line: u64,
    pub minute: Option<i64>,
}

/// One key (with one level) of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub text: Vec<u8>,
    pub level: Level,
    pub records: u64,
    /// the lines of its records, all together
    pub lines: u64,
    /// the first line of its first record, and of its last record
    pub first_line: u64,
    pub last_line: u64,
    /// the minutes of those two records (their first line's time)
    pub first_minute: Option<i64>,
    pub last_minute: Option<i64>,
    /// records per hour (hours since 1970, UTC), in order; records without a clock time are
    /// in none
    pub hours: Vec<(i64, u64)>,
    /// sample records' first lines: all when records <= 16, else the first 8 and the last 4
    pub samples: Vec<u64>,
    /// the first record's first and last line (a whole stack trace)
    pub first_record: (u64, u64),
}

/// A moment of the run found by the index pass (guard.rs): a jump or a burst.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moment {
    /// guard::Kind::code
    pub kind: u8,
    pub minute: Option<i64>,
    /// the key's index in the catalog (a jump: its key's first entry; a burst: the entry of the
    /// minute's first error record)
    pub key: u64,
    /// records of the key (jump) or error records (burst) in the minute
    pub count: u64,
    /// the first line of the minute's first such record
    pub line: u64,
    /// a jump: twice its median per active minute in the hour before; a burst: the error
    /// records of the 5 minutes before
    pub before: u64,
}

/// A run's catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    /// the key rule's version (msgkey::RULE)
    pub rule: u32,
    /// the archive it fits: its length and its last 4 bytes
    pub archive_bytes: u64,
    pub end_crc: [u8; 4],
    pub lines: u64,
    pub records: u64,
    pub bytes: u64,
    /// the earliest and the latest line time (Unix ms), when any line has a clock time
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// the source's log type (formats::Format::name)
    pub format: String,
    /// the zone's offset in minutes east of UTC, at the first line time (else the file time)
    pub zone_minutes: i64,
    pub marks: Vec<Mark>,
    /// in order of first appearance (a key's id is its index)
    pub keys: Vec<Key>,
    pub moments: Vec<Moment>,
    pub errs: Vec<(u64, u64)>,
    pub files: Vec<(String, u64)>,
}

/// A level as a number (the catalog's encoding, and an index into per-level tables).
pub fn level_code(l: Level) -> usize {
    match l {
        Level::None => 0,
        Level::Debug => 1,
        Level::Info => 2,
        Level::Warn => 3,
        Level::Error => 4,
    }
}

/// The level of a number given by [`level_code`].
pub fn level_of(c: usize) -> Option<Level> {
    [Level::None, Level::Debug, Level::Info, Level::Warn, Level::Error].get(c).copied()
}

/// The hour (since 1970, UTC) of a minute.
pub fn hour_of(minute: i64) -> i64 {
    minute.div_euclid(60)
}

/// The minute of line `line`, from a catalog's marks (None: no clock time). (Search walks the
/// marks in order with MarkWalk.)
pub fn minute_of(marks: &[Mark], line: u64) -> Option<i64> {
    let k = marks.partition_point(|m| m.line <= line);
    if k == 0 { None } else { marks[k - 1].minute }
}

/// Lines asked in increasing order: their minutes, walking the marks once.
pub struct MarkWalk<'a> {
    marks: &'a [Mark],
    next: usize,
    cur: Option<i64>,
}

impl<'a> MarkWalk<'a> {
    pub fn new(marks: &'a [Mark]) -> MarkWalk<'a> {
        MarkWalk { marks, next: 0, cur: None }
    }

    /// The minute of `line`; lines must be asked in increasing order.
    pub fn at(&mut self, line: u64) -> Option<i64> {
        while self.next < self.marks.len() && self.marks[self.next].line <= line {
            self.cur = self.marks[self.next].minute;
            self.next += 1;
        }
        self.cur
    }
}

impl Catalog {
    /// The keys merged by text, in order of first appearance: (text, records, lines, the
    /// first line of its first record, the key's highest level).
    pub fn by_text(&self) -> Vec<(&[u8], u64, u64, u64, Level)> {
        let mut at: HashMap<&[u8], usize> = HashMap::new();
        let mut out: Vec<(&[u8], u64, u64, u64, Level)> = Vec::new();
        for k in &self.keys {
            match at.get(&k.text[..]) {
                Some(&i) => {
                    let e = &mut out[i];
                    e.1 += k.records;
                    e.2 += k.lines;
                    e.3 = e.3.min(k.first_line);
                    e.4 = e.4.max(k.level);
                }
                None => {
                    at.insert(&k.text, out.len());
                    out.push((&k.text, k.records, k.lines, k.first_line, k.level));
                }
            }
        }
        out
    }

    /// `<records>\t<key>\n` per key text, in order of first appearance, each key written by
    /// msgkey::write_key: what `fewer keys --dump` prints.
    pub fn dump(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (text, records, ..) in self.by_text() {
            out.extend_from_slice(records.to_string().as_bytes());
            out.push(b'\t');
            msgkey::write_key(text, &mut out);
            out.push(b'\n');
        }
        out
    }
}

// ---------------------------------------------------------------- the index pass

/// A record that holds a given line ([`record_at`]): its key, level, and first and last line.
pub type Probed = (Vec<u8>, Level, u64, u64);

/// A record while it is read.
struct Rec {
    first: u64,
    last: u64,
    key: Vec<u8>,
    /// its key line's level
    level: Level,
    /// a multi-line error record so far
    trace: bool,
    minute: Option<i64>,
}

/// A key entry while it is built.
struct Acc {
    key: Key,
    /// records per hour, the hour seen last kept apart (logs are mostly in order)
    hours: BTreeMap<i64, u64>,
    hour: Option<(i64, u64)>,
    head: Vec<u64>,
    tail: VecDeque<u64>,
    /// records per minute, for the Guard notes' jumps (guard.rs)
    minutes: guard::Minutes,
}

impl Acc {
    fn new(text: Vec<u8>, level: Level, r: &Rec) -> Acc {
        let key = Key {
            text,
            level,
            records: 0,
            lines: 0,
            first_line: r.first,
            last_line: r.first,
            first_minute: r.minute,
            last_minute: r.minute,
            hours: Vec::new(),
            samples: Vec::new(),
            first_record: (r.first, r.last),
        };
        Acc { key, hours: BTreeMap::new(), hour: None, head: Vec::new(), tail: VecDeque::new(), minutes: Vec::new() }
    }

    fn add(&mut self, r: &Rec) {
        let k = &mut self.key;
        k.records += 1;
        k.lines += r.last - r.first + 1;
        k.last_line = r.first;
        k.last_minute = r.minute;
        if let Some(m) = r.minute {
            guard::count_minute(&mut self.minutes, m, r.first);
            let h = hour_of(m);
            match &mut self.hour {
                Some((hh, n)) if *hh == h => *n += 1,
                cur => {
                    if let Some((hh, n)) = cur.take() {
                        *self.hours.entry(hh).or_insert(0) += n;
                    }
                    *cur = Some((h, 1));
                }
            }
        }
        if (self.head.len() as u64) < SAMPLE_ALL {
            self.head.push(r.first);
        }
        self.tail.push_back(r.first);
        if self.tail.len() > SAMPLE_TAIL {
            self.tail.pop_front();
        }
    }

    fn finish(mut self) -> Key {
        if let Some((h, n)) = self.hour.take() {
            *self.hours.entry(h).or_insert(0) += n;
        }
        self.key.hours = self.hours.into_iter().collect();
        self.key.samples = if self.key.records <= SAMPLE_ALL {
            self.head
        } else {
            let mut s = self.head[..SAMPLE_HEAD].to_vec();
            s.extend(self.tail);
            s
        };
        self.key
    }
}

/// Records per entry in windows of minutes, counted by the index pass when asked (the `quiet`
/// tool, against the window before): each record at the minute the given marks put its first
/// line in (a run's catalog's marks, so arrival times count as the catalog counted them), or
/// at `untimed` when they put it in none.
#[derive(Clone, Debug, Default)]
pub struct Windows {
    /// the windows: [start, end) in minutes since 1970 (UTC)
    pub spans: Vec<(i64, i64)>,
    pub marks: Vec<Mark>,
    pub untimed: i64,
    /// per entry, in the catalog's key order: per window, its records and the first one's line
    pub counts: Vec<Vec<(u64, u64)>>,
}

impl Windows {
    /// Count a record of entry `i`, whose first line is `line`; records come in line order.
    fn add(&mut self, i: usize, line: u64, walk: &mut (usize, Option<i64>)) {
        while walk.0 < self.marks.len() && self.marks[walk.0].line <= line {
            walk.1 = self.marks[walk.0].minute;
            walk.0 += 1;
        }
        let m = walk.1.unwrap_or(self.untimed);
        while self.counts.len() <= i {
            self.counts.push(vec![(0, 0); self.spans.len()]);
        }
        for (j, &(a, b)) in self.spans.iter().enumerate() {
            if a <= m && m < b {
                let c = &mut self.counts[i][j];
                c.0 += 1;
                if c.1 == 0 {
                    c.1 = line;
                }
            }
        }
    }
}

/// The pass once the log type is known.
struct Pass {
    profile: Profile,
    clock: Clock,
    recs: Records,
    carry: Carry,
    arrivals: Option<(i64, Vec<(u64, u64)>)>,
    /// the next arrival mark to pass, and the ms since the start reached
    next_arrival: usize,
    arrived: u64,
    /// lines so far
    n: u64,
    cur: Option<Rec>,
    accs: Vec<Acc>,
    /// key text -> its entry per level (u32::MAX: none yet)
    at: HashMap<Vec<u8>, [u32; 5]>,
    marks: Vec<Mark>,
    minute: Option<i64>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    records: u64,
    /// records counted in windows, when asked, and the place reached in its marks
    win: Option<Windows>,
    win_walk: (usize, Option<i64>),
    /// error records per minute, for the Guard notes' bursts: (records, the first one's first
    /// line, its entry)
    errors: BTreeMap<i64, (u64, u64, u64)>,
    /// a line whose record is asked for (`record_at`), and that record once it ended: its key,
    /// level, first and last line
    probe: Option<u64>,
    probed: Option<Probed>,
}

impl Pass {
    fn arrival(&mut self, line: u64) -> Option<i64> {
        let (start, marks) = self.arrivals.as_ref()?;
        while self.next_arrival < marks.len() && marks[self.next_arrival].0 <= line {
            self.arrived = marks[self.next_arrival].1;
            self.next_arrival += 1;
        }
        Some(start + self.arrived as i64)
    }

    fn line(&mut self, raw: &[u8]) {
        self.n += 1;
        let n = self.n;
        let clean = formats::clean_line(raw);
        let f = self.profile.fields(&clean, &self.clock);
        let arrival = self.arrival(n);
        let ms = self.carry.next(f.time, arrival).and_then(Stamp::utc_ms);
        if let Some(t) = ms {
            self.first_ms = Some(self.first_ms.map_or(t, |x| x.min(t)));
            self.last_ms = Some(self.last_ms.map_or(t, |x| x.max(t)));
        }
        let minute = ms.map(|t| t.div_euclid(60_000));
        if minute != self.minute {
            self.marks.push(Mark { line: n, minute });
            self.minute = minute;
        }
        let step = self.recs.push_fields(&f);
        if step.start {
            self.close();
            self.cur = Some(Rec { first: n, last: n, key: Vec::new(), level: Level::None, trace: false, minute });
        }
        let cur = self.cur.as_mut().expect("a record is open");
        cur.last = n;
        if step.start || step.key {
            cur.key = msgkey::clean_key(&clean);
            cur.level = f.level;
        }
        cur.trace = self.recs.is_trace();
    }

    /// Count the record that just ended under its key and level.
    fn close(&mut self) {
        let Some(mut r) = self.cur.take() else { return };
        self.records += 1;
        let level = if r.trace { Level::Error } else { r.level };
        if self.probe.is_some_and(|p| r.first <= p && p <= r.last) {
            self.probed = Some((r.key.clone(), level, r.first, r.last));
        }
        let lc = level_code(level);
        let i = match self.at.get(&r.key[..]).map(|a| a[lc]) {
            Some(i) if i != u32::MAX => i as usize,
            _ => {
                let i = self.accs.len();
                let text = std::mem::take(&mut r.key);
                self.at.entry(text.clone()).or_insert([u32::MAX; 5])[lc] = i as u32;
                self.accs.push(Acc::new(text, level, &r));
                i
            }
        };
        self.accs[i].add(&r);
        if let Some(w) = &mut self.win {
            w.add(i, r.first, &mut self.win_walk);
        }
        if level == Level::Error
            && let Some(m) = r.minute
        {
            // (records come in line order: the first one kept has the smallest line)
            self.errors.entry(m).or_insert((0, r.first, i as u64)).0 += 1;
        }
    }
}

/// The index pass, fed the text in pieces of any size.
pub struct Builder {
    name: String,
    file_ms: i64,
    zone: Zone,
    arrivals: Option<(i64, Vec<(u64, u64)>)>,
    /// the text until the log type is known (64 KiB, or all of a shorter text)
    head: Vec<u8>,
    pass: Option<Pass>,
    /// a line not ended yet
    part: Vec<u8>,
    bytes: u64,
    /// records to count in windows, until the pass starts
    win: Option<Windows>,
    /// the line whose record is asked for, until the pass starts
    probe: Option<u64>,
}

impl Builder {
    /// A pass over the text of `tags.name`, whose file time is `file_ms`.
    pub fn new(tags: &Tags, file_ms: i64) -> Builder {
        Builder {
            name: tags.name.clone(),
            file_ms,
            zone: tags.zone.unwrap_or(Zone::Local),
            arrivals: tags.arrivals.clone(),
            head: Vec::new(),
            pass: None,
            part: Vec::new(),
            bytes: 0,
            win: None,
            probe: None,
        }
    }

    /// Also count each entry's records in windows of minutes ([`Windows`]); `finish_windows`
    /// gives them back.
    pub fn with_windows(mut self, w: Windows) -> Builder {
        self.win = Some(w);
        self
    }

    /// The next bytes of the text.
    pub fn feed(&mut self, d: &[u8]) {
        self.bytes += d.len() as u64;
        if self.pass.is_none() {
            self.head.extend_from_slice(d);
            if self.head.len() >= formats::SAMPLE_BYTES {
                self.start();
            }
            return;
        }
        self.lines(d);
    }

    /// The log type is named from the first 64 KiB; then the lines read so far go through.
    fn start(&mut self) {
        let profile = formats::detect(&self.name, &self.head);
        let format = profile.format;
        self.pass = Some(Pass {
            profile,
            clock: Clock::new(self.zone, self.file_ms),
            recs: Records::new(format),
            carry: Carry::new(),
            arrivals: self.arrivals.take(),
            next_arrival: 0,
            arrived: 0,
            n: 0,
            cur: None,
            accs: Vec::new(),
            at: HashMap::new(),
            marks: Vec::new(),
            minute: None,
            first_ms: None,
            last_ms: None,
            records: 0,
            win: self.win.take(),
            win_walk: (0, None),
            errors: BTreeMap::new(),
            probe: self.probe,
            probed: None,
        });
        let head = std::mem::take(&mut self.head);
        self.lines(&head);
    }

    /// The record holding line `line`, once it has ended ([`record_at`]).
    fn probed(&self) -> Option<&Probed> {
        self.pass.as_ref().and_then(|p| p.probed.as_ref())
    }

    fn lines(&mut self, mut d: &[u8]) {
        let pass = self.pass.as_mut().expect("the pass started");
        while let Some(p) = d.iter().position(|&c| c == b'\n') {
            if self.part.is_empty() {
                pass.line(&d[..=p]);
            } else {
                self.part.extend_from_slice(&d[..=p]);
                let l = std::mem::take(&mut self.part);
                pass.line(&l);
            }
            d = &d[p + 1..];
        }
        self.part.extend_from_slice(d);
    }

    /// The catalog of the text (archive-bytes and end-crc left 0: the caller sets them for
    /// the archive it fits).
    pub fn finish(self) -> Catalog {
        self.finish_windows().0
    }

    /// The catalog, and the records counted in windows when they were asked for
    /// ([`Builder::with_windows`]).
    pub fn finish_windows(mut self) -> (Catalog, Option<Windows>) {
        if self.pass.is_none() {
            self.start();
        }
        let mut pass = self.pass.take().expect("the pass started");
        if !self.part.is_empty() {
            let l = std::mem::take(&mut self.part);
            pass.line(&l);
        }
        pass.close();
        // (every entry has its row: each record is counted, in a window or not)
        let win = pass.win.take();
        let zone_minutes = times::offset_at(pass.clock.zone(), pass.first_ms.unwrap_or(self.file_ms)) / 60;
        let mut minutes = Vec::with_capacity(pass.accs.len());
        let keys: Vec<Key> = pass
            .accs
            .into_iter()
            .map(|mut a| {
                minutes.push(std::mem::take(&mut a.minutes));
                a.finish()
            })
            .collect();
        // the Guard notes of the index pass: jumps and bursts
        let moments = guard::index_moments(&keys, &minutes, &pass.errors);
        let cat = Catalog {
            rule: msgkey::RULE,
            archive_bytes: 0,
            end_crc: [0; 4],
            lines: pass.n,
            records: pass.records,
            bytes: self.bytes,
            first_ms: pass.first_ms,
            last_ms: pass.last_ms,
            format: pass.profile.format.name().to_string(),
            zone_minutes,
            marks: pass.marks,
            keys,
            moments,
            errs: Vec::new(),
            files: Vec::new(),
        };
        (cat, win)
    }
}

/// The catalog of a text read from `text` (a capture still in raw/, or any file).
pub fn build(text: &mut dyn Read, tags: &Tags, file_ms: i64) -> Result<Catalog, String> {
    let mut b = Builder::new(tags, file_ms);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match text.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => b.feed(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read: {e}")),
        }
    }
    let mut c = b.finish();
    c.errs = tags.errs.clone();
    c.files = tags.files.clone();
    Ok(c)
}

/// An archive's length and its last 4 bytes (its end record's CRC over every segment): what
/// a catalog must hold to fit it.
pub fn archive_id(path: &Path) -> Result<(u64, [u8; 4]), String> {
    let mut f = fs::File::open(path).map_err(|e| format!("cannot read: {e}"))?;
    let len = f.metadata().map_err(|e| format!("cannot read: {e}"))?.len();
    if len < 4 {
        return Err("not a fewer archive".into());
    }
    let mut c = [0u8; 4];
    f.seek(SeekFrom::Start(len - 4)).and_then(|_| f.read_exact(&mut c)).map_err(|e| format!("cannot read: {e}"))?;
    Ok((len, c))
}

/// The catalog of a run's archive (a file archive made by fewer-pack), its text decoded on
/// `threads` threads and read by the pass in order. `stop` is asked between blocks.
pub fn build_archive(path: &Path, tags: &Tags, threads: usize, stop: &dyn Fn() -> bool) -> Result<Catalog, String> {
    Ok(pass_archive(path, tags, None, threads, stop)?.0)
}

/// The index pass over a run's archive again, also counting each entry's records in windows
/// of minutes ([`Windows`]: the `quiet` tool, for a run that a window's edge cuts). The
/// catalog it gives is the run's own (the same pass), so its entries are the stored
/// catalog's, in the same order.
pub fn archive_windows(path: &Path, tags: &Tags, w: Windows, threads: usize, stop: &dyn Fn() -> bool) -> Result<(Catalog, Windows), String> {
    let (c, w) = pass_archive(path, tags, Some(w), threads, stop)?;
    Ok((c, w.expect("windows were asked for")))
}

fn pass_archive(path: &Path, tags: &Tags, win: Option<Windows>, threads: usize, stop: &dyn Fn() -> bool) -> Result<(Catalog, Option<Windows>), String> {
    let (archive_bytes, end_crc) = archive_id(path)?;
    let (ar, _) = crate::grep::open_archive(path.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (catalogs are made for file archives)".into());
    }
    let meta = crate::grep::parse_file_meta(&ar.meta)?;
    let file_ms = tags.file_ms.or(meta.map(|(_, (s, ns))| s * 1000 + ns as i64 / 1_000_000)).unwrap_or_else(times::now_ms);
    let mut b = Builder::new(tags, file_ms);
    if let Some(w) = win {
        b = b.with_windows(w);
    }
    let mut stopped = false;
    crate::container::for_each_segment(ar, threads.max(1), &mut |d| {
        b.feed(d);
        stopped = stop();
        Ok(!stopped)
    })?;
    if stopped {
        return Err("stopped before the catalog was built".into());
    }
    let (mut c, w) = b.finish_windows();
    c.archive_bytes = archive_bytes;
    c.end_crc = end_crc;
    c.errs = tags.errs.clone();
    c.files = tags.files.clone();
    Ok((c, w))
}

/// The record that holds line `line` of a run's archive, as the index pass reads it (its key
/// line may be another line: a stack trace's first, a traceback's last): its key, its level,
/// and its first and last line. None when the run has fewer lines. The pass stops once that
/// record has ended.
pub fn record_at(path: &Path, tags: &Tags, line: u64, threads: usize, stop: &dyn Fn() -> bool) -> Result<Option<Probed>, String> {
    let (ar, _) = crate::grep::open_archive(path.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (catalogs are made for file archives)".into());
    }
    let meta = crate::grep::parse_file_meta(&ar.meta)?;
    let file_ms = tags.file_ms.or(meta.map(|(_, (s, ns))| s * 1000 + ns as i64 / 1_000_000)).unwrap_or_else(times::now_ms);
    let mut b = Builder::new(tags, file_ms);
    b.probe = Some(line);
    let mut stopped = false;
    crate::container::for_each_segment(ar, threads.max(1), &mut |d| {
        b.feed(d);
        stopped = stop();
        Ok(!stopped && b.probed().is_none())
    })?;
    if let Some(p) = b.probed() {
        return Ok(Some(p.clone()));
    }
    if stopped {
        return Err("stopped before the record was read".into());
    }
    // (the last record ends with the text)
    if b.pass.is_none() {
        b.start();
    }
    let mut pass = b.pass.take().expect("the pass started");
    if !b.part.is_empty() {
        let l = std::mem::take(&mut b.part);
        pass.line(&l);
    }
    pass.close();
    Ok(pass.probed)
}

// ---------------------------------------------------------------- the .cat file

fn put_opt(o: &mut Vec<u8>, v: Option<i64>) {
    put_varint(o, v.map_or(0, |x| zz(x).wrapping_add(1)));
}

fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_varint(o, b.len() as u64);
    o.extend_from_slice(b);
}

/// A reader of varints and byte strings, every read checked.
struct Rd<'a> {
    d: &'a [u8],
    p: usize,
}

fn damaged() -> String {
    "damaged catalog".to_string()
}

impl Rd<'_> {
    fn var(&mut self) -> Result<u64, String> {
        get_varint(self.d, &mut self.p).ok_or_else(damaged)
    }
    fn opt(&mut self) -> Result<Option<i64>, String> {
        let v = self.var()?;
        Ok(if v == 0 { None } else { Some(unzz(v - 1)) })
    }
    fn signed(&mut self) -> Result<i64, String> {
        Ok(unzz(self.var()?))
    }
    fn take(&mut self, n: u64) -> Result<&[u8], String> {
        let n = usize::try_from(n).map_err(|_| damaged())?;
        let e = self.p.checked_add(n).filter(|&e| e <= self.d.len()).ok_or_else(damaged)?;
        let s = &self.d[self.p..e];
        self.p = e;
        Ok(s)
    }
    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = self.var()?;
        Ok(self.take(n)?.to_vec())
    }
    /// A count of items that take at least one byte each (so a damaged count cannot ask for
    /// more memory than the file has bytes).
    fn count(&mut self) -> Result<usize, String> {
        let n = self.var()?;
        if n > (self.d.len() - self.p) as u64 {
            return Err(damaged());
        }
        Ok(n as usize)
    }
}

/// A file's bytes with the CRC-32 of all of them appended.
fn sealed(mut o: Vec<u8>) -> Vec<u8> {
    let c = crc32(&o);
    o.extend_from_slice(&c.to_le_bytes());
    o
}

/// The bytes before a file's CRC-32, when the CRC and the magic are right.
fn unsealed<'a>(d: &'a [u8], magic: &[u8; 5], what: &str) -> Result<&'a [u8], String> {
    if d.len() < 9 || d[..4] != magic[..4] {
        return Err(format!("not a {what}"));
    }
    if d[4] != magic[4] {
        return Err(format!("a {what} of another version ({})", d[4]));
    }
    let (body, c) = d.split_at(d.len() - 4);
    if crc32(body).to_le_bytes() != c {
        return Err(format!("damaged {what} (it does not match its checksum)"));
    }
    Ok(body)
}

impl Catalog {
    /// The .cat bytes: numbers as varints, CRC-32 at the end. The same
    /// catalog always gives the same bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = CAT_MAGIC.to_vec();
        put_varint(&mut o, self.rule as u64);
        put_varint(&mut o, self.archive_bytes);
        o.extend_from_slice(&self.end_crc);
        // head
        put_varint(&mut o, self.lines);
        put_varint(&mut o, self.records);
        put_varint(&mut o, self.bytes);
        put_opt(&mut o, self.first_ms);
        put_opt(&mut o, self.last_ms);
        put_bytes(&mut o, self.format.as_bytes());
        put_varint(&mut o, zz(self.zone_minutes));
        // marks: line deltas; minutes as deltas from the last mark that had one (0: none)
        put_varint(&mut o, self.marks.len() as u64);
        let (mut line, mut base) = (0u64, 0i64);
        for m in &self.marks {
            put_varint(&mut o, m.line - line);
            line = m.line;
            put_opt(&mut o, m.minute.map(|x| x - base));
            if let Some(x) = m.minute {
                base = x;
            }
        }
        // keys
        put_varint(&mut o, self.keys.len() as u64);
        for k in &self.keys {
            put_bytes(&mut o, &k.text);
            put_varint(&mut o, level_code(k.level) as u64);
            put_varint(&mut o, k.records);
            put_varint(&mut o, k.lines);
            put_varint(&mut o, k.first_line);
            put_varint(&mut o, k.last_line - k.first_line);
            put_opt(&mut o, k.first_minute);
            put_opt(&mut o, k.last_minute);
            put_varint(&mut o, k.hours.len() as u64);
            let mut h0 = 0i64;
            for &(h, n) in &k.hours {
                put_varint(&mut o, zz(h - h0));
                h0 = h;
                put_varint(&mut o, n);
            }
            let mut s0 = 0u64;
            for &s in &k.samples {
                put_varint(&mut o, s - s0);
                s0 = s;
            }
            put_varint(&mut o, k.first_record.0);
            put_varint(&mut o, k.first_record.1 - k.first_record.0);
        }
        // moments, stderr lines, edited files
        put_varint(&mut o, self.moments.len() as u64);
        for m in &self.moments {
            o.push(m.kind);
            put_opt(&mut o, m.minute);
            put_varint(&mut o, m.key);
            put_varint(&mut o, m.count);
            put_varint(&mut o, m.line);
            put_varint(&mut o, m.before);
        }
        put_varint(&mut o, self.errs.len() as u64);
        for &(l, n) in &self.errs {
            put_varint(&mut o, l);
            put_varint(&mut o, n);
        }
        put_varint(&mut o, self.files.len() as u64);
        for (f, l) in &self.files {
            put_bytes(&mut o, f.as_bytes());
            put_varint(&mut o, *l);
        }
        sealed(o)
    }

    /// A catalog from its bytes. With `keys` false, only the head and the marks are read
    /// (what search needs for times); the whole file is still checked against its CRC.
    pub fn decode(d: &[u8], keys: bool) -> Result<Catalog, String> {
        let body = unsealed(d, CAT_MAGIC, "catalog")?;
        let mut r = Rd { d: body, p: 5 };
        let rule = u32::try_from(r.var()?).map_err(|_| damaged())?;
        let archive_bytes = r.var()?;
        let end_crc: [u8; 4] = r.take(4)?.try_into().unwrap();
        let lines = r.var()?;
        let records = r.var()?;
        let bytes = r.var()?;
        let first_ms = r.opt()?;
        let last_ms = r.opt()?;
        let format = String::from_utf8(r.bytes()?).map_err(|_| damaged())?;
        let zone_minutes = r.signed()?;
        let nm = r.count()?;
        let mut marks = Vec::with_capacity(nm);
        let (mut line, mut base) = (0u64, 0i64);
        for _ in 0..nm {
            line = line.checked_add(r.var()?).ok_or_else(damaged)?;
            let minute = r.opt()?.map(|d| base.wrapping_add(d));
            if let Some(x) = minute {
                base = x;
            }
            if marks.last().is_some_and(|m: &Mark| m.line >= line) || line == 0 || line > lines {
                return Err(damaged());
            }
            marks.push(Mark { line, minute });
        }
        let mut c = Catalog {
            rule,
            archive_bytes,
            end_crc,
            lines,
            records,
            bytes,
            first_ms,
            last_ms,
            format,
            zone_minutes,
            marks,
            keys: Vec::new(),
            moments: Vec::new(),
            errs: Vec::new(),
            files: Vec::new(),
        };
        if !keys {
            return Ok(c);
        }
        let nk = r.count()?;
        c.keys.reserve(nk);
        for _ in 0..nk {
            let text = r.bytes()?;
            let level = usize::try_from(r.var()?).ok().and_then(level_of).ok_or_else(damaged)?;
            let krecords = r.var()?;
            let klines = r.var()?;
            let first_line = r.var()?;
            let last_line = first_line.checked_add(r.var()?).ok_or_else(damaged)?;
            let first_minute = r.opt()?;
            let last_minute = r.opt()?;
            let nh = r.count()?;
            let mut hours = Vec::with_capacity(nh);
            let mut h0 = 0i64;
            for _ in 0..nh {
                h0 = h0.wrapping_add(r.signed()?);
                hours.push((h0, r.var()?));
            }
            let ns = if krecords <= SAMPLE_ALL { krecords as usize } else { SAMPLE_HEAD + SAMPLE_TAIL };
            let mut samples = Vec::with_capacity(ns.min(64));
            let mut s0 = 0u64;
            for _ in 0..ns {
                s0 = s0.checked_add(r.var()?).ok_or_else(damaged)?;
                samples.push(s0);
            }
            let fr0 = r.var()?;
            let fr1 = fr0.checked_add(r.var()?).ok_or_else(damaged)?;
            if krecords == 0 || klines < krecords || last_line > lines || fr1 > lines {
                return Err(damaged());
            }
            c.keys.push(Key { text, level, records: krecords, lines: klines, first_line, last_line, first_minute, last_minute, hours, samples, first_record: (fr0, fr1) });
        }
        let nmo = r.count()?;
        for _ in 0..nmo {
            let kind = r.take(1)?[0];
            let minute = r.opt()?;
            let key = r.var()?;
            let count = r.var()?;
            let line = r.var()?;
            let before = r.var()?;
            if guard::Kind::of_code(kind).is_none() || key >= c.keys.len() as u64 || line == 0 || line > lines {
                return Err(damaged());
            }
            c.moments.push(Moment { kind, minute, key, count, line, before });
        }
        let ne = r.count()?;
        for _ in 0..ne {
            let l = r.var()?;
            c.errs.push((l, r.var()?));
        }
        let nf = r.count()?;
        for _ in 0..nf {
            let f = String::from_utf8(r.bytes()?).map_err(|_| damaged())?;
            c.files.push((f, r.var()?));
        }
        if r.p != body.len() {
            return Err(damaged());
        }
        Ok(c)
    }
}

/// A run's catalog from its .cat file, refused unless it fits the archive at `archive` and
/// was made with this version's key rule.
pub fn load(cat: &Path, archive: &Path, keys: bool) -> Result<Catalog, String> {
    let d = match fs::read(cat) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("no catalog yet".into()),
        Err(e) => return Err(format!("cannot read its catalog: {e}")),
    };
    let c = Catalog::decode(&d, keys)?;
    let (len, end) = archive_id(archive)?;
    if c.archive_bytes != len || c.end_crc != end {
        return Err(format!("its catalog was {}", crate::MISMATCH));
    }
    if c.rule != msgkey::RULE {
        return Err(format!("its catalog was made with key rule {} (this version uses rule {})", c.rule, msgkey::RULE));
    }
    Ok(c)
}

/// The source name of a saved run, as its catalog was made with: the file it saved (a packed
/// file's without the ending of the tool that read it: add::inner_name), else its command.
pub fn source_name(r: &RunRec) -> String {
    if let Some(s) = r.s("source") {
        return crate::add::inner_name(s, r.s("reader")).to_string();
    }
    if let Some(c) = r.0.get("cmd").and_then(Json::arr) {
        return c.iter().filter_map(Json::str).collect::<Vec<_>>().join(" ");
    }
    r.s("cmd").unwrap_or("").to_string()
}

/// A run's catalog for the tools: its .cat when it fits, else one built now from the
/// archive's bytes (never written: the tools only read). Also, when it was built now, why
/// ("no catalog yet", "its catalog was made for another archive, ...").
pub fn of_run(dir: &Path, r: &RunRec, keys: bool, stop: &dyn Fn() -> bool) -> Result<(Catalog, Option<String>), String> {
    let archive = dir.join(store::run_file(r.id()));
    match load(&dir.join(cat_file(r.id())), &archive, keys) {
        Ok(c) => Ok((c, None)),
        Err(why) => {
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            let tags = Tags { name: source_name(r), ..Tags::default() };
            let c = build_archive(&archive, &tags, threads, stop)?;
            Ok((c, Some(why)))
        }
    }
}

/// A run's log type as its catalog names it (the source's name and the text's first 64 KiB,
/// decoded from the archive), and the file time its dates without a year are read with:
/// what reading a matching line's level needs (`count` by level with a text).
pub fn profile_of(archive: &Path, name: &str) -> Result<(Profile, i64), String> {
    let (ar, _) = crate::grep::open_archive(archive.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (the tools read file archives)".into());
    }
    let meta = crate::grep::parse_file_meta(&ar.meta)?;
    let file_ms = meta.map(|(_, (s, ns))| s * 1000 + ns as i64 / 1_000_000).unwrap_or_else(times::now_ms);
    let mut head = Vec::new();
    crate::container::for_each_segment(ar, 1, &mut |d| {
        head.extend_from_slice(&d[..d.len().min(formats::SAMPLE_BYTES - head.len())]);
        Ok(head.len() < formats::SAMPLE_BYTES)
    })?;
    Ok((formats::detect(name, &head), file_ms))
}

/// The catalog of a run in progress: its capture's first `size` bytes, with its arrival
/// marks and stderr lines. Built in memory; never written.
pub fn of_capture(home: &Path, pid: &str, r: &RunRec, size: u64) -> Result<Catalog, String> {
    let raw = crate::capture::raw_dir(home, pid);
    let id = r.id();
    let f = fs::File::open(raw.join(crate::capture::out_file(id))).map_err(|e| format!("cannot read its capture: {e}"))?;
    let start = r.i("start").unwrap_or(0);
    let tags = Tags {
        name: source_name(r),
        file_ms: Some(times::now_ms()),
        arrivals: Some((start, crate::capture::read_marks(&raw.join(crate::capture::times_file(id))))),
        errs: crate::capture::read_errs(&raw.join(crate::capture::err_file(id))),
        ..Tags::default()
    };
    let mut c = build(&mut f.take(size), &tags, times::now_ms())?;
    // (the stderr listing may already name lines written after `size`)
    c.errs.retain(|e| e.0 <= c.lines);
    Ok(c)
}

/// profile_of for a run in progress: the log type from its capture's first 64 KiB.
pub fn profile_of_capture(capture: &Path, name: &str) -> Result<(Profile, i64), String> {
    let mut head = Vec::new();
    fs::File::open(capture).and_then(|f| f.take(formats::SAMPLE_BYTES as u64).read_to_end(&mut head)).map_err(|e| format!("cannot read its capture: {e}"))?;
    Ok((formats::detect(name, &head), times::now_ms()))
}

// ---------------------------------------------------------------- the key registry

/// Where a key was seen: by time (Unix ms), then run id, then line, so the
/// order of merging does not matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seen {
    pub ms: i64,
    pub run: u64,
    pub line: u64,
}

/// A key of the project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegKey {
    pub first: Seen,
    pub last: Seen,
    /// its last record by the order of runs (the newest run that holds it, its last record):
    /// the fix memory's last-seen place
    pub latest: Seen,
    /// the runs it was seen in
    pub runs: u64,
    pub records: u64,
    /// some of its records are errors (level error, or multi-line error records)
    pub error: bool,
    /// its stops, oldest first (guard.rs)
    pub stops: Vec<guard::Stop>,
}

/// The project's key registry (keys.cat): every key text of the project's catalogs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    /// the runs merged in, in order
    pub runs: Vec<u64>,
    /// the length and CRC-32 of the moments.jsonl written with it (guard.rs)
    pub moments: (u64, u32),
    /// by key text
    pub keys: BTreeMap<Vec<u8>, RegKey>,
}

fn put_seen(o: &mut Vec<u8>, s: Seen) {
    put_varint(o, zz(s.ms));
    put_varint(o, s.run);
    put_varint(o, s.line);
}

impl Rd<'_> {
    fn seen(&mut self) -> Result<Seen, String> {
        Ok(Seen { ms: self.signed()?, run: self.var()?, line: self.var()? })
    }
}

impl Registry {
    /// Merge in run `run`'s catalog (`start_ms`: the run's save time, the time of records
    /// without a clock time of their own). A run already merged in changes nothing (false).
    /// The stops are not this merge's: they depend on the order of runs (guard::step).
    pub fn merge(&mut self, run: u64, start_ms: i64, cat: &Catalog) -> bool {
        let at = match self.runs.binary_search(&run) {
            Ok(_) => return false,
            Err(at) => at,
        };
        self.runs.insert(at, run);
        let ms = |m: Option<i64>| m.map_or(start_ms, |x| x.saturating_mul(60_000));
        // the run's keys by text: first and last record, records, error
        let mut by: HashMap<&[u8], (Seen, Seen, u64, bool)> = HashMap::new();
        for k in &cat.keys {
            let first = Seen { ms: ms(k.first_minute), run, line: k.first_line };
            let last = Seen { ms: ms(k.last_minute), run, line: k.last_line };
            let e = by.entry(&k.text).or_insert((first, last, 0, false));
            if k.first_line < e.0.line {
                e.0 = first;
            }
            if k.last_line > e.1.line {
                e.1 = last;
            }
            e.2 += k.records;
            e.3 |= k.level == Level::Error;
        }
        for (text, (first, last, records, error)) in by {
            match self.keys.get_mut(text) {
                Some(r) => {
                    r.first = r.first.min(first);
                    r.last = r.last.max(last);
                    // (by run, then line: the same whatever the order of merging)
                    if (last.run, last.line) > (r.latest.run, r.latest.line) {
                        r.latest = last;
                    }
                    r.runs += 1;
                    r.records += records;
                    r.error |= error;
                }
                None => {
                    self.keys.insert(text.to_vec(), RegKey { first, last, latest: last, runs: 1, records, error, stops: Vec::new() });
                }
            }
        }
        true
    }

    /// The keys.cat bytes ("CMLK" 02, varints, CRC-32 at the end); keys in byte order of
    /// their text, so the same registry always gives the same bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = REG_MAGIC.to_vec();
        put_varint(&mut o, self.runs.len() as u64);
        let mut r0 = 0u64;
        for &r in &self.runs {
            put_varint(&mut o, r - r0);
            r0 = r;
        }
        put_varint(&mut o, self.moments.0);
        o.extend_from_slice(&self.moments.1.to_le_bytes());
        put_varint(&mut o, self.keys.len() as u64);
        for (text, k) in &self.keys {
            put_bytes(&mut o, text);
            for s in [k.first, k.last, k.latest] {
                put_seen(&mut o, s);
            }
            put_varint(&mut o, k.runs);
            put_varint(&mut o, k.records);
            o.push(k.error as u8);
            put_varint(&mut o, k.stops.len() as u64);
            for s in &k.stops {
                put_seen(&mut o, s.last);
                put_varint(&mut o, s.run);
                put_varint(&mut o, zz(s.ms));
                match s.back {
                    None => o.push(0),
                    Some(b) => {
                        o.push(1);
                        put_seen(&mut o, b);
                    }
                }
            }
        }
        sealed(o)
    }

    pub fn decode(d: &[u8]) -> Result<Registry, String> {
        let body = unsealed(d, REG_MAGIC, "key registry")?;
        let mut r = Rd { d: body, p: 5 };
        let mut reg = Registry::default();
        let nr = r.count()?;
        let mut run = 0u64;
        for i in 0..nr {
            let d = r.var()?;
            if i > 0 && d == 0 {
                return Err(damaged());
            }
            run = run.checked_add(d).ok_or_else(damaged)?;
            reg.runs.push(run);
        }
        let len = r.var()?;
        reg.moments = (len, u32::from_le_bytes(r.take(4)?.try_into().unwrap()));
        let nk = r.count()?;
        for _ in 0..nk {
            let text = r.bytes()?;
            let (first, last, latest) = (r.seen()?, r.seen()?, r.seen()?);
            let runs = r.var()?;
            let records = r.var()?;
            let error = match r.take(1)?[0] {
                0 => false,
                1 => true,
                _ => return Err(damaged()),
            };
            let ns = r.count()?;
            let mut stops = Vec::with_capacity(ns);
            for _ in 0..ns {
                let last = r.seen()?;
                let run = r.var()?;
                let ms = r.signed()?;
                let back = match r.take(1)?[0] {
                    0 => None,
                    1 => Some(r.seen()?),
                    _ => return Err(damaged()),
                };
                stops.push(guard::Stop { last, run, ms, back });
            }
            if reg.keys.last_key_value().is_some_and(|(t, _)| *t >= text) {
                return Err(damaged());
            }
            reg.keys.insert(text, RegKey { first, last, latest, runs, records, error, stops });
        }
        if r.p != body.len() {
            return Err(damaged());
        }
        Ok(reg)
    }

    /// A project's registry as stored (None when it is missing or damaged).
    pub fn read(dir: &Path) -> Option<Registry> {
        Registry::decode(&fs::read(dir.join(REGISTRY)).ok()?).ok()
    }
}

// ---------------------------------------------------------------- saving (add, pack-pending)

/// What saving records in the run's "ready" record (runs.jsonl): `format`, `rule`, `first`,
/// `last`.
#[derive(Clone, Debug)]
pub struct Indexed {
    pub format: String,
    pub rule: u32,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub records: u64,
    pub keys: usize,
}

impl Indexed {
    /// The fields for the run's record.
    pub fn fields(&self) -> Vec<(&'static str, Json)> {
        let mut v = vec![("format", Json::from(self.format.as_str())), ("rule", Json::from(self.rule as u64))];
        if let Some(f) = self.first {
            v.push(("first", Json::from(f)));
        }
        if let Some(l) = self.last {
            v.push(("last", Json::from(l)));
        }
        v
    }
}

/// The index pass of a saved run, after fewer-pack has packed it and its .idx is written: its
/// catalog is built from the archive (rNNNNNN.cmf in the project's folder) and written
/// (rNNNNNN.cat), and merged into the project's key registry under the project lock.
/// `start_ms` is the run's save start. `add` calls it, and pack-pending calls it the
/// same way for each run it packs, with the capture's arrival marks and stderr lines in `tags`.
pub fn index_run(p: &Project, id: u64, start_ms: i64, tags: &Tags) -> Result<Indexed, String> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let archive = p.dir.join(store::run_file(id));
    let cat = build_archive(&archive, tags, threads, &|| false)?;
    store::write_atomic(&p.dir.join(cat_file(id)), &cat.encode()).map_err(|e| format!("cannot write its catalog: {e}"))?;
    merge_registry(p, id, start_ms, &cat).map_err(|e| format!("cannot write the key registry: {e}"))?;
    Ok(Indexed { format: cat.format.clone(), rule: cat.rule, first: cat.first_ms, last: cat.last_ms, records: cat.records, keys: cat.by_text().len() })
}

/// Merge a run's catalog into keys.cat, with its Guard notes (moments.jsonl) and the fix
/// memory's stops (guard.rs), under the project lock. Registry and notes that are missing,
/// damaged, or hold a later run are made again from every catalog first, run by run.
pub fn merge_registry(p: &Project, id: u64, start_ms: i64, cat: &Catalog) -> std::io::Result<()> {
    let _l = store::Lock::take(&p.dir)?;
    let runs = store::read_runs(&p.dir);
    match guard::merged(&p.dir, &runs, id, start_ms, cat) {
        Some((reg, text)) => guard::write(&p.dir, reg, &text),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------- `fewer keys`

const KEYS_USAGE: &str = "usage: fewer keys --dump FILE | --lines FILE | --cat FILE.cat";

/// `fewer keys`:
///   --dump FILE      `<records>\t<key>` per key, in order of first appearance: the index pass
///                    over FILE's bytes (as add makes the catalog of a saved file)
///   --lines FILE     each line's key, one per line
///   --cat FILE.cat   a stored catalog's keys in the --dump form; refused unless it fits the
///                    archive beside it (the same name ending in .cmf)
/// Keys are written by msgkey::write_key ('\' as `\\`, control bytes as `\xHH`).
pub fn cmd_keys(args: Vec<OsString>) -> i32 {
    let a: Vec<&OsString> = args.iter().collect();
    if a.len() != 2 {
        eprintln!("fewer keys: {KEYS_USAGE}");
        return 2;
    }
    let path = PathBuf::from(a[1]);
    let out = match a[0].to_str() {
        Some("--dump") => keys_dump(&path),
        Some("--lines") => fs::read(&path).map_err(|e| format!("cannot read: {e}")).map(|t| {
            let mut out = Vec::with_capacity(t.len() / 2);
            for k in msgkey::line_keys(&t) {
                msgkey::write_key(&k, &mut out);
                out.push(b'\n');
            }
            out
        }),
        Some("--cat") => {
            let archive = path.with_extension("cmf");
            load(&path, &archive, true).map(|c| c.dump())
        }
        _ => {
            eprintln!("fewer keys: {KEYS_USAGE}");
            return 2;
        }
    };
    match out {
        Ok(b) => {
            use std::io::Write;
            let mut so = std::io::stdout().lock();
            if so.write_all(&b).and_then(|_| so.flush()).is_err() {
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("fewer keys: {}: {e}", path.display());
            1
        }
    }
}

/// `fewer catalog [--name NAME] FILE.cmf [OUT.cat]`: the catalog of a file archive,
/// built again from its text and written to OUT (default: FILE.cat beside it). NAME is the
/// source's name its log type is named from (runs.jsonl's `source`; default: none). A run's
/// catalog is rebuilt this way with the same bytes as saving made (a developer command: the
/// key registry is not touched).
pub fn cmd_catalog(args: Vec<OsString>) -> i32 {
    let mut name = String::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--name" {
            match it.next() {
                Some(n) => name = n.to_string_lossy().into_owned(),
                None => {
                    eprintln!("fewer catalog: --name needs a value");
                    return 2;
                }
            }
        } else {
            files.push(PathBuf::from(a));
        }
    }
    if files.is_empty() || files.len() > 2 {
        eprintln!("usage: fewer catalog [--name NAME] FILE.cmf [OUT.cat]");
        return 2;
    }
    let archive = &files[0];
    let out = files.get(1).cloned().unwrap_or_else(|| archive.with_extension("cat"));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let tags = Tags { name, ..Tags::default() };
    match build_archive(archive, &tags, threads, &|| false).and_then(|c| store::write_atomic(&out, &c.encode()).map(|_| c).map_err(|e| format!("cannot write {}: {e}", out.display()))) {
        Ok(c) => {
            println!("wrote {}: {} records of {} keys in {} lines", out.display(), c.records, c.by_text().len(), c.lines);
            0
        }
        Err(e) => {
            eprintln!("fewer catalog: {}: {e}", archive.display());
            1
        }
    }
}

/// The index pass over a file, as `add` makes the catalog of a saved file (its name and its
/// modification time), dumped.
fn keys_dump(path: &Path) -> Result<Vec<u8>, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("cannot read: {e}"))?;
    let md = f.metadata().map_err(|e| format!("cannot read: {e}"))?;
    let file_ms = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
    let abs = std::path::absolute(path).unwrap_or(path.to_path_buf());
    let tags = Tags { name: abs.to_string_lossy().into_owned(), ..Tags::default() };
    Ok(build(&mut f, &tags, file_ms)?.dump())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/made_up");

    fn made_up() -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<(String, Vec<u8>)> = fs::read_dir(DATA)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
            .map(|p| (p.to_string_lossy().into_owned(), fs::read(&p).unwrap()))
            .collect();
        v.sort();
        v
    }

    fn tags(name: &str) -> Tags {
        Tags { name: name.to_string(), zone: Some(Zone::Fixed(0)), ..Tags::default() }
    }

    fn cat_of(name: &str, text: &[u8], cut: usize) -> Catalog {
        let mut b = Builder::new(&tags(name), 1_795_003_200_000);
        for piece in text.chunks(cut.max(1)) {
            b.feed(piece);
        }
        b.finish()
    }

    /// The pass gives msgkey's record counts (the reference `keys --dump` is checked
    /// against), whatever size the pieces it is fed in.
    #[test]
    fn keys_and_records_as_msgkey_counts_them() {
        for (name, text) in made_up() {
            let want = msgkey::count_records(&name, &text).dump();
            for cut in [1, 7, 4096, usize::MAX] {
                let c = cat_of(&name, &text, cut);
                assert_eq!(String::from_utf8_lossy(&c.dump()), String::from_utf8_lossy(&want), "{name}, pieces of {cut}");
                let lines = text.split_inclusive(|&b| b == b'\n').count() as u64;
                assert_eq!((c.lines, c.bytes), (lines, text.len() as u64), "{name}");
                assert_eq!(c.records, c.keys.iter().map(|k| k.records).sum::<u64>());
                assert_eq!(c.keys.iter().map(|k| k.lines).sum::<u64>(), lines, "{name}: every line is in one record");
            }
        }
    }

    /// Built twice: the same bytes; read back: the same catalog; any changed byte or a cut
    /// file is refused.
    #[test]
    fn the_same_bytes_and_read_back() {
        for (name, text) in made_up() {
            let mut a = cat_of(&name, &text, 1000);
            a.archive_bytes = 12345;
            a.end_crc = [1, 2, 3, 4];
            a.moments.push(Moment { kind: guard::Kind::Jump.code(), minute: Some(29_000_000), key: 0, count: 30, line: 1, before: 4 });
            a.errs.push((2, 1));
            a.files.push(("/w/src/main.rs".into(), 1));
            let mut b = cat_of(&name, &text, 333);
            b.archive_bytes = 12345;
            b.end_crc = [1, 2, 3, 4];
            b.moments = a.moments.clone();
            b.errs = a.errs.clone();
            b.files = a.files.clone();
            let bytes = a.encode();
            assert_eq!(bytes, b.encode(), "{name}");
            assert_eq!(Catalog::decode(&bytes, true).unwrap(), a);
            let head = Catalog::decode(&bytes, false).unwrap();
            assert_eq!((head.lines, &head.marks, head.keys.len()), (a.lines, &a.marks, 0));
            for i in (0..bytes.len()).step_by(7) {
                let mut d = bytes.clone();
                d[i] ^= 0x10;
                assert!(Catalog::decode(&d, true).is_err(), "{name}: byte {i} changed");
            }
            assert!(Catalog::decode(&bytes[..bytes.len() - 1], true).is_err());
        }
    }

    /// Lines get their own time, else the previous line's; the marks give each line's minute
    /// and the hours count records.
    #[test]
    fn times_marks_and_hours() {
        let text = b"no time yet\n\
            2026-11-18T10:15:02Z one\n\
            go on\n\
            2026-11-18T10:15:59Z two\n\
            2026-11-18T10:16:00Z three\n\
            2026-11-18T11:00:00Z one\n\
            2026-11-18T09:59:00Z late\n";
        let c = cat_of("app.log", text, 5);
        // 2026-11-18 00:00 UTC (Python: calendar.timegm((2026, 11, 18, 0, 0, 0)) * 1000)
        let day = 1_794_960_000_000i64;
        let base = day / 60_000;
        let at = |h: i64, mi: i64| Some(base + h * 60 + mi);
        assert_eq!(c.first_ms, Some(day + (9 * 3600 + 59 * 60) * 1000));
        assert_eq!(c.last_ms, Some(day + 11 * 3600 * 1000));
        assert_eq!(
            c.marks,
            vec![
                Mark { line: 2, minute: at(10, 15) },
                Mark { line: 5, minute: at(10, 16) },
                Mark { line: 6, minute: at(11, 0) },
                Mark { line: 7, minute: at(9, 59) },
            ]
        );
        assert_eq!(minute_of(&c.marks, 1), None);
        assert_eq!(minute_of(&c.marks, 3), at(10, 15));
        assert_eq!(minute_of(&c.marks, 99), at(9, 59));
        let mut w = MarkWalk::new(&c.marks);
        for l in 1..=7 {
            assert_eq!(w.at(l), minute_of(&c.marks, l));
        }
        let one = c.keys.iter().find(|k| k.text == b"<time> one").unwrap();
        let h = |x: i64| hour_of(base) + x;
        assert_eq!((one.records, one.first_line, one.last_line), (2, 2, 6));
        assert_eq!(one.hours, vec![(h(10), 1), (h(11), 1)]);
        assert_eq!(one.lines, 2, "\"go on\" continues no record: it is a record of its own");
        let total: u64 = c.keys.iter().flat_map(|k| k.hours.iter().map(|x| x.1)).sum();
        assert_eq!(total, c.records - 1, "the line without a time is in no hour");
        assert_eq!(c.zone_minutes, 0);
    }

    /// Records counted in windows of minutes: each record at the minute the given marks put its
    /// first line in, or at `untimed`; the catalog is the same as without windows.
    #[test]
    fn records_in_windows() {
        let mut text = b"no time yet\n".to_vec();
        // (first line, minute, key, level) of each record after the first
        let mut want: Vec<(u64, i64, Vec<u8>, Level)> = Vec::new();
        let base = 1_794_960_000_000i64 / 60_000 + 10 * 60; // 2026-11-18 10:00 UTC, in minutes
        let mut line = 2;
        for i in 0..40i64 {
            let error = i % 5 == 0;
            text.extend_from_slice(format!("2026-11-18T10:{:02}:{:02}Z {} job {i} done\n", i / 4, i % 4 * 15, if error { "ERROR" } else { "INFO" }).as_bytes());
            let trace = i % 7 == 0;
            let key = if error { b"<time> ERROR job <num> done".to_vec() } else { b"<time> INFO job <num> done".to_vec() };
            want.push((line, base + i / 4, key, if error || trace { Level::Error } else { Level::Info }));
            line += 1;
            if trace {
                text.extend_from_slice(b"\tat a.B.c(B.java:1)\n");
                line += 1;
            }
        }
        let plain = cat_of("app.log", &text, 100);
        let spans = vec![(base, base + 3), (base + 3, base + 7), (base - 100, base + 100), (base + 20, base + 30)];
        for (marks, untimed) in [(plain.marks.clone(), base + 5), (Vec::new(), base + 1)] {
            let w = Windows { spans: spans.clone(), marks: marks.clone(), untimed, counts: Vec::new() };
            let mut b = Builder::new(&tags("app.log"), 1_795_003_200_000).with_windows(w);
            for piece in text.chunks(33) {
                b.feed(piece);
            }
            let (c, w) = b.finish_windows();
            assert_eq!(c, plain, "windows do not change the catalog");
            let w = w.unwrap();
            assert_eq!(w.counts.len(), c.keys.len());
            let mut expect = vec![vec![(0u64, 0u64); spans.len()]; c.keys.len()];
            let records = std::iter::once((1u64, None, b"no time yet".to_vec(), Level::None)).chain(want.iter().map(|(l, m, k, v)| (*l, Some(*m), k.clone(), *v)));
            for (l, m, key, level) in records {
                let i = c.keys.iter().position(|k| k.text == key && k.level == level).unwrap();
                // (without marks, every record is at `untimed`)
                let m = if marks.is_empty() { untimed } else { m.unwrap_or(untimed) };
                for (j, &(a, b)) in spans.iter().enumerate() {
                    if a <= m && m < b {
                        expect[i][j].0 += 1;
                        if expect[i][j].1 == 0 {
                            expect[i][j].1 = l;
                        }
                    }
                }
            }
            assert_eq!(w.counts, expect);
            assert_eq!(w.counts.iter().map(|x| x[2].0).sum::<u64>(), c.records, "a window over every minute holds every record");
        }
    }

    /// Arrival marks give times to lines that have none.
    #[test]
    fn arrival_times() {
        let t = Tags { arrivals: Some((1_794_960_000_000, vec![(1, 0), (3, 120_000)])), ..tags("cmd") };
        let mut b = Builder::new(&t, 0);
        b.feed(b"a\nb\nc\nd");
        let c = b.finish();
        let m0 = 1_794_960_000_000 / 60_000;
        assert_eq!(c.marks, vec![Mark { line: 1, minute: Some(m0) }, Mark { line: 3, minute: Some(m0 + 2) }]);
        assert_eq!(c.lines, 4);
    }

    /// Levels: a record is its key line's level, or error when it is a stack trace; one key
    /// with two levels is two entries; samples keep the first 8 and the last 4.
    #[test]
    fn levels_entries_and_samples() {
        let mut text = Vec::new();
        for i in 0..30 {
            text.extend_from_slice(format!("2026-11-18 10:00:{:02} request {i} done in {} ms\n", i, i * 3).as_bytes());
        }
        text.extend_from_slice(b"2026-11-18 10:01:00 ERROR request 99 done in 5 ms\n");
        text.extend_from_slice(b"Exception in thread \"main\" java.lang.IllegalStateException: boom\n\tat a.B.c(B.java:10)\n\tat a.B.main(B.java:3)\n");
        let c = cat_of("app.log", &text, 64);
        let done = c.keys.iter().find(|k| k.text == b"<time> <time> request <num> done in <num> ms").unwrap();
        assert_eq!((done.records, done.level), (30, Level::None));
        assert_eq!(done.samples, vec![1, 2, 3, 4, 5, 6, 7, 8, 27, 28, 29, 30]);
        let err = c.keys.iter().find(|k| k.text == b"<time> <time> ERROR request <num> done in <num> ms").unwrap();
        assert_eq!((err.records, err.level, err.samples.clone()), (1, Level::Error, vec![31]));
        let trace = c.keys.last().unwrap();
        assert_eq!((trace.level, trace.first_record, trace.lines), (Level::Error, (32, 34), 3));
        assert_eq!(c.records, 32);
    }

    /// One log saved whole and in pieces cut where a record starts (each piece a source of its
    /// own): the per-key record counts summed over the pieces equal the whole's.
    #[test]
    fn whole_and_pieces() {
        let mut compared = 0;
        for (name, text) in made_up() {
            let whole = cat_of(&name, &text, 4096);
            // where records start
            let profile = formats::detect(&name, &text);
            let lines: Vec<&[u8]> = text.split_inclusive(|&b| b == b'\n').collect();
            let clean: Vec<Vec<u8>> = lines.iter().map(|l| formats::clean_line(l).into_owned()).collect();
            let refs: Vec<&[u8]> = clean.iter().map(|l| &l[..]).collect();
            let recs = crate::records::split(&profile, &refs, &Clock::new(Zone::Fixed(0), 0));
            if recs.len() < 3 {
                continue;
            }
            let starts: Vec<usize> = recs.iter().map(|r| lines[..r.first].iter().map(|l| l.len()).sum()).collect();
            let want: BTreeMap<Vec<u8>, u64> = whole.by_text().into_iter().map(|(t, n, ..)| (t.to_vec(), n)).collect();
            for every in [2, 3] {
                let cuts: Vec<usize> = starts.iter().copied().skip(1).step_by(every).collect();
                let mut sum: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
                // each piece names its own log type, as a run of its own does: the sums are
                // compared where every piece gets the whole's
                let mut same_kind = true;
                let mut a = 0;
                for &c in cuts.iter().chain(std::iter::once(&text.len())) {
                    let p = cat_of(&name, &text[a..c], 4096);
                    same_kind &= p.format == whole.format;
                    for (t, n, ..) in p.by_text() {
                        *sum.entry(t.to_vec()).or_insert(0) += n;
                    }
                    a = c;
                }
                if same_kind {
                    assert_eq!(sum, want, "{name}: pieces of every {every} records");
                    compared += 1;
                }
            }
        }
        assert!(compared >= 20, "{compared}");
    }

    /// The registry: the same whatever the order of merging; a run merged twice counts once.
    #[test]
    fn registry_merges_in_any_order() {
        let cats: Vec<(u64, i64, Catalog)> = made_up().iter().enumerate().map(|(i, (n, t))| (i as u64 + 1, 1_000 * i as i64, cat_of(n, t, 512))).collect();
        let mut a = Registry::default();
        for (id, s, c) in &cats {
            assert!(a.merge(*id, *s, c));
        }
        assert!(!a.merge(cats[0].0, cats[0].1, &cats[0].2));
        let mut b = Registry::default();
        for (id, s, c) in cats.iter().rev() {
            b.merge(*id, *s, c);
        }
        assert_eq!(a.encode(), b.encode());
        assert_eq!(Registry::decode(&a.encode()).unwrap(), a);
        let total: u64 = cats.iter().map(|x| x.2.records).sum();
        assert_eq!(a.keys.values().map(|k| k.records).sum::<u64>(), total);
        let mut d = a.encode();
        let n = d.len();
        d[n / 2] ^= 1;
        assert!(Registry::decode(&d).is_err());
    }

    /// Values of `<num>` placeholders come with the same key.
    #[test]
    fn key_values() {
        let mut v = Vec::new();
        let k = msgkey::line_key_values(b"took 12 ms, 0x1F, -3 and 2.5s to 10.0.0.1:80 x7\n", &mut v);
        assert_eq!(k, msgkey::line_key(b"took 12 ms, 0x1F, -3 and 2.5s to 10.0.0.1:80 x7\n"));
        assert_eq!(String::from_utf8(k.clone()).unwrap(), "took <num> ms, <num>, <num> and <num> to <ip>:<num> <num>");
        let nums: Vec<Option<&[u8]>> = v.iter().map(|n| n.num.as_deref()).collect();
        assert_eq!(nums, vec![Some(&b"12"[..]), None, Some(&b"-3"[..]), Some(&b"2.5s"[..]), None]);
        assert_eq!(msgkey::num_parts(b"2.5s"), (&b"2.5"[..], &b"s"[..]));
        assert_eq!(msgkey::num_parts(b"-12"), (&b"-12"[..], &b""[..]));
        for n in &v {
            assert_eq!(&k[n.at as usize..n.at as usize + 5], b"<num>");
        }
        let k = msgkey::line_key_values(br#"{"level":"warn","msg":"slow 250 ms"}"#, &mut v);
        assert_eq!((&k[..], v[0].at, v[0].num.as_deref()), (&b"{warn} slow <num> ms"[..], 12, Some(&b"250"[..])));
        for (_, text) in made_up() {
            for l in text.split_inclusive(|&b| b == b'\n') {
                let k = msgkey::line_key_values(l, &mut v);
                assert_eq!(k, msgkey::line_key(l));
                for n in &v {
                    assert_eq!(&k[n.at as usize..n.at as usize + 5], b"<num>");
                }
            }
        }
    }
}
