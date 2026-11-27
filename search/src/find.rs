//! The tools' search: exact matching lines with their line
//! numbers, over every run of a scope.
//!
//! - `Matcher` is a copy of search::Needle with `find` public (the tools need each match's
//!   place, which grep_lines does not give). A test checks it against search::grep_lines.
//! - A run's archive is searched through the same pipeline as fewer-pack grep
//!   (container::search_archive with the engine filter and the .idx, planned by
//!   grep::Planner). Its jobs write records instead of lines: each matching line's '\n'
//!   count before it and its bytes, and each run of lines' '\n' count; a block left unopened
//!   gives its '\n' count from the run's .idx (`IdxTable`). The writer gets every entry in
//!   order and adds them up, so a match's line number is 1 + the '\n' before it. A number
//!   is never guessed: without a fitting .idx, no block is left unopened.
//! - `search_scope` searches many runs: runs under 8 MiB of text with one thread each,
//!   several at a time; bigger runs one at a time with all threads; results in scope
//!   order. The time limit and the stop flag are checked between blocks and between runs.
//! - The DNA index (dna.rs) leaves more blocks unopened: those no line of
//!   which can hold a piece of the text it looks up, also only at a '\n' edge and only with
//!   a .idx that fits. Each run's blocks, opened and left unopened, go to the debug log.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::container::{self, Ent, Run, get_varint, put_varint};
use crate::crc::crc32;
use crate::grep::{self, Planner};
use crate::msgkey::num_parts;
use crate::{engine, index, search};

// ---------------------------------------------------------------- Matcher

/// How common a byte is in log text (search.rs's commonness).
fn commonness(c: u8) -> u32 {
    let c = c.to_ascii_lowercase();
    match c {
        b' ' => 100,
        b'e' => 90,
        b't' | b'a' | b'o' | b'i' | b'n' => 80,
        b's' | b'r' | b'h' | b'l' | b'd' | b'c' => 70,
        b'0'..=b'9' => 75,
        b'u' | b'm' | b'f' | b'p' | b'g' | b'w' | b'y' | b'b' => 50,
        b'.' | b':' | b'-' | b'_' | b'/' | b',' => 60,
        b'v' | b'k' => 30,
        b'x' | b'j' | b'q' | b'z' => 10,
        _ => 20,
    }
}

/// First i >= from with h[i] == a or h[i] == b (search.rs's find2).
#[inline]
fn find2(h: &[u8], from: usize, a: u8, b: u8) -> Option<usize> {
    let (sa, sb) = (u64::from_ne_bytes([a; 8]), u64::from_ne_bytes([b; 8]));
    let mut i = from;
    while i + 8 <= h.len() {
        let w = u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
        let (x, y) = (w ^ sa, w ^ sb);
        let t = (x.wrapping_sub(0x0101_0101_0101_0101) & !x | y.wrapping_sub(0x0101_0101_0101_0101) & !y) & 0x8080_8080_8080_8080;
        if t != 0 {
            return Some(i + (t.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    (i..h.len()).find(|&k| h[k] == a || h[k] == b)
}

/// search::Needle, copied with `find` public.
pub struct Matcher {
    text: Vec<u8>,
    icase: bool,
    /// the rarest byte's place in the needle
    at: usize,
    /// the needle holds a line end: no line contains it
    never: bool,
}

impl Matcher {
    pub fn new(text: &[u8], icase: bool) -> Matcher {
        let text: Vec<u8> = if icase { text.to_ascii_lowercase() } else { text.to_vec() };
        let at = (0..text.len()).min_by_key(|&i| commonness(text[i])).unwrap_or(0);
        let never = text.contains(&b'\n');
        Matcher { text, icase, at, never }
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    #[inline]
    fn matches_at(&self, h: &[u8], s: usize) -> bool {
        let n = &self.text;
        if s + n.len() > h.len() {
            return false;
        }
        if self.icase { h[s..s + n.len()].iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b) } else { h[s..s + n.len()] == n[..] }
    }

    /// Start of the first match in h[from..], if any.
    pub fn find(&self, h: &[u8], from: usize) -> Option<usize> {
        if self.never {
            return None;
        }
        if self.text.is_empty() {
            return if from <= h.len() { Some(from) } else { None };
        }
        let c = self.text[self.at];
        let (a, b) = if self.icase { (c, c.to_ascii_uppercase()) } else { (c, c) };
        let mut i = from + self.at;
        while let Some(k) = find2(h, i, a, b) {
            let s = k - self.at;
            if self.matches_at(h, s) {
                return Some(s);
            }
            i = k + 1;
        }
        None
    }
}

/// A byte that a whole-word match may not touch: an ASCII letter or digit, '_', '-', or a
/// byte >= 0x80.
pub fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80
}

// ---------------------------------------------------------------- the .idx's line counts

/// A block of a run's .idx: its size, its '\n' count, whether its text ends with '\n'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdxBlock {
    pub raw: u64,
    pub nl: u64,
    pub ends: bool,
}

/// The per-block table of a .idx (index.rs keeps it private): each segment's header and
/// blocks, for line numbers in blocks that are never opened.
pub struct IdxTable {
    /// (segment header, its first block, its number of blocks)
    pub segs: Vec<((u64, u64, u32), usize, usize)>,
    pub blocks: Vec<IdxBlock>,
}

const IDX_MAGIC: &[u8; 5] = b"CMLI\x01";

impl IdxTable {
    /// The table of a .idx's bytes made for an archive of `len` bytes.
    pub fn parse(d: &[u8], len: u64) -> Result<IdxTable, String> {
        let dam = || "damaged index".to_string();
        if d.len() < 9 || d[..5] != IDX_MAGIC[..] {
            return Err("not a fewer-pack index of this version".into());
        }
        let (d, c) = d.split_at(d.len() - 4);
        if crc32(d).to_le_bytes() != c {
            return Err(dam());
        }
        let p = &mut 5;
        let var = |p: &mut usize| get_varint(d, p).ok_or_else(dam);
        if var(p)? != len {
            return Err(crate::MISMATCH.into());
        }
        let nsegs = var(p)?;
        let mut segs = Vec::new();
        let mut blocks = Vec::new();
        for _ in 0..nsegs {
            let raw = var(p)?;
            let packed = var(p)?;
            let crc = u32::from_le_bytes(d.get(*p..*p + 4).ok_or_else(dam)?.try_into().unwrap());
            *p += 4;
            let nb = var(p)?;
            let first = blocks.len();
            let mut sum = 0u64;
            for _ in 0..nb {
                let braw = var(p)?;
                let nl = var(p)?;
                let flags = *d.get(*p).ok_or_else(dam)?;
                *p += 1;
                if flags & 2 != 0 {
                    *p += 8;
                    let b = var(p)? as usize;
                    if b == 0 {
                        return Err(dam());
                    }
                    *p = p.checked_add(b.checked_mul(3).ok_or_else(dam)?).ok_or_else(dam)?;
                    if *p > d.len() {
                        return Err(dam());
                    }
                }
                if braw == 0 || nl > braw {
                    return Err(dam());
                }
                sum += braw;
                blocks.push(IdxBlock { raw: braw, nl, ends: flags & 1 != 0 });
            }
            if sum != raw {
                return Err(dam());
            }
            segs.push(((raw, packed, crc), first, nb as usize));
        }
        if *p != d.len() {
            return Err(dam());
        }
        Ok(IdxTable { segs, blocks })
    }

    /// The .idx at `path` for an archive of `len` bytes.
    pub fn load(path: &Path, len: u64) -> Result<IdxTable, String> {
        let md = std::fs::metadata(path).map_err(|e| format!("cannot read: {e}"))?;
        if !md.is_file() {
            return Err("not a regular file".into());
        }
        let d = std::fs::read(path).map_err(|e| format!("cannot read: {e}"))?;
        IdxTable::parse(&d, len)
    }

    /// Each block's first line (1-based; a block may start inside a line of the one before).
    fn starts(&self) -> Vec<u64> {
        let mut v = Vec::with_capacity(self.blocks.len());
        let mut n = 1u64;
        for b in &self.blocks {
            v.push(n);
            n += b.nl;
        }
        v
    }

    /// Whether block k may be left unopened (Index::skips's rule): after a '\n' (or at the
    /// start) and ending with one (or the last block).
    fn edge(&self, k: usize) -> bool {
        (k == 0 || self.blocks[k - 1].ends) && (self.blocks[k].ends || k + 1 == self.blocks.len())
    }

    /// For segment g with header h: no block left unopened (the header checked).
    fn no_skips(&self, g: usize, h: (u64, u64, u32)) -> Option<Vec<(usize, bool)>> {
        let &(hdr, first, n) = self.segs.get(g)?;
        (hdr == h).then(|| self.blocks[first..first + n].iter().map(|b| (b.raw as usize, false)).collect())
    }
}

// ---------------------------------------------------------------- queries and records

/// What is searched for.
pub struct Query {
    pub text: Vec<u8>,
    pub icase: bool,
    /// only matches with no letter, digit, '_', '-' or byte >= 0x80 right before or after
    pub whole: bool,
    /// each matching line's message key is noted (msgkey.rs), for counts per key and for
    /// lines collapsed by key
    pub with_keys: bool,
    m: Matcher,
    flt: search::Filter,
    keys: Vec<u64>,
    /// the pieces of the text the DNA index looks up (dna::pieces)
    dna: Vec<Vec<u8>>,
}

impl Query {
    pub fn new(text: &[u8], icase: bool, whole: bool) -> Query {
        Query {
            text: text.to_vec(),
            icase,
            whole,
            with_keys: false,
            m: Matcher::new(text, icase),
            flt: search::Filter::new(text, icase, engine::value_names()),
            keys: index::needle_keys(text),
            dna: crate::dna::pieces(text),
        }
    }

    /// Whether the line h[ls..le] (no '\n' inside) holds a match, given its first match m.
    fn line_holds(&self, h: &[u8], ls: usize, le: usize, mut m: usize) -> bool {
        if !self.whole {
            return true;
        }
        let n = self.m.len();
        loop {
            let before = m > ls && word_byte(h[m - 1]);
            let after = m + n < le && word_byte(h[m + n]);
            if !before && !after {
                return true;
            }
            match self.m.find(h, m + 1) {
                Some(k) if k + n <= le => m = k,
                _ => return false,
            }
        }
    }
}

/// How much of the matching lines a job keeps (the rest is only counted).
#[derive(Clone, Copy)]
pub struct Keep {
    /// lines whose bytes are kept
    pub lines: usize,
    /// bytes kept, all together (a line starts being kept while under this)
    pub bytes: usize,
    /// most bytes kept of one line
    pub line_bytes: usize,
    /// lines collapsed by key (search): at most this many groups of lines with the
    /// same key per run, each with its first line; 0: no groups
    pub groups: usize,
}

impl Keep {
    pub const NONE: Keep = Keep { lines: 0, bytes: 0, line_bytes: 0, groups: 0 };
}

/// What reading a matching line's level needs: the run's log type, as its catalog was made
/// (`count` by level with a text).
pub struct LevelCtx {
    pub profile: crate::formats::Profile,
    pub zone: crate::times::Zone,
    pub file_ms: i64,
}

const TAG_END: u8 = 0;
const TAG_MATCH: u8 = 1;

fn count_nl(b: &[u8]) -> u64 {
    b.iter().filter(|&&c| c == b'\n').count() as u64
}

/// Search whole lines `text` (the last may lack its '\n'), writing records to `out`: for
/// each matching line, its '\n' count before it in `text`, its length and (within `keep`)
/// its bytes, with `q.with_keys` its key (and with groups its `<num>` values), with `level`
/// its level; then the '\n' count of all of `text`. Returns the matching lines.
pub fn scan_lines(text: &[u8], q: &Query, keep: Keep, level: Option<&LevelCtx>, out: &mut Vec<u8>) -> usize {
    let (mut count, mut pos) = (0usize, 0usize);
    let (mut nl, mut nl_pos) = (0u64, 0usize);
    let (mut kept_lines, mut kept_bytes) = (0usize, 0usize);
    let clock = level.map(|l| crate::times::Clock::new(l.zone, l.file_ms));
    let mut vals = Vec::new();
    while pos < text.len() {
        let Some(m) = q.m.find(text, pos) else { break };
        if q.m.len() == 0 && m == text.len() {
            break;
        }
        let ls = text[pos..m].iter().rposition(|&c| c == b'\n').map_or(pos, |k| pos + k + 1);
        let le = text[m..].iter().position(|&c| c == b'\n').map_or(text.len(), |k| m + k);
        if q.line_holds(text, ls, le, m) {
            nl += count_nl(&text[nl_pos..ls]);
            nl_pos = ls;
            out.push(TAG_MATCH);
            put_varint(out, nl);
            put_varint(out, (le - ls) as u64);
            if kept_lines < keep.lines && kept_bytes < keep.bytes {
                let k = (le - ls).min(keep.line_bytes);
                out.push(1);
                put_varint(out, k as u64);
                out.extend_from_slice(&text[ls..ls + k]);
                kept_lines += 1;
                kept_bytes += k;
            } else {
                out.push(0);
            }
            if q.with_keys {
                let key = crate::msgkey::line_key_values(&text[ls..le], &mut vals);
                put_varint(out, key.len() as u64);
                out.extend_from_slice(&key);
                if keep.groups > 0 {
                    put_varint(out, vals.len() as u64);
                    for v in &vals {
                        put_varint(out, v.at as u64);
                        match &v.num {
                            None => out.push(0),
                            Some(n) => {
                                out.push(1);
                                put_varint(out, n.len() as u64);
                                out.extend_from_slice(n);
                            }
                        }
                    }
                }
            }
            if let (Some(l), Some(c)) = (level, clock.as_ref()) {
                let clean = crate::formats::clean_line(&text[ls..le]);
                out.push(crate::catalog::level_code(l.profile.fields(&clean, c).level) as u8);
            }
            count += 1;
        }
        pos = le + 1;
    }
    nl += count_nl(&text[nl_pos..]);
    out.push(TAG_END);
    put_varint(out, nl);
    count
}

/// The record that says: this many '\n' passed (a block left unopened).
fn end_record(nl: u64) -> Vec<u8> {
    let mut v = vec![TAG_END];
    put_varint(&mut v, nl);
    v
}

/// A matching line: its number, the bytes kept of it (without its '\n'), its full length,
/// and whether its bytes are still to be read (a job kept only its first matches, which can
/// lie before a cursor's line: they are then read by line number, fetch.rs).
#[derive(Clone, Debug)]
pub struct Hit {
    pub line: u64,
    pub bytes: Vec<u8>,
    pub len: usize,
    pub missing: bool,
}

/// A number range of one `<num>` placeholder over a group's lines: its place in the key, the
/// smallest and the largest value written there (exact decimals with their unit, as written),
/// and whether every line of the group has a value there, with the same unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub at: u32,
    pub min: Vec<u8>,
    pub max: Vec<u8>,
    pub ok: bool,
}

/// Lines of one run with the same key, collapsed into one (search): the first line,
/// how many there are, the last one's number, and the ranges of their numbers.
#[derive(Clone, Debug)]
pub struct Group {
    /// the key's index in the run's `keys`
    pub key: u32,
    pub first: Hit,
    pub count: u64,
    pub last: u64,
    pub ranges: Vec<Range>,
}

/// Matches in a row with the same minute: (first line, last line, minute). The minutes of
/// the matches from any line on come from these (the "left out" line's time range).
pub type MinuteRun = (u64, u64, Option<i64>);

/// Runs of minutes kept per run at most (beyond, times of left-out lines are not told).
const MINUTE_RUNS_MAX: usize = 1_000_000;

/// What a search of one run found.
#[derive(Default, Debug)]
pub struct RunHits {
    /// matching lines at or after the start line (exact unless `stopped`)
    pub count: u64,
    /// the first of them (within the keep limits), in order
    pub kept: Vec<Hit>,
    /// the first matching line after them
    pub first_unkept: Option<u64>,
    /// lines covered: all of them when the search ended; when it stopped, the lines before
    /// the place it reached (the next line to search is lines + 1)
    pub lines: u64,
    /// stopped by the time limit or the stop flag
    pub stopped: bool,
    /// a damaged or unreadable run
    pub error: Option<String>,
    /// with keys: the distinct keys of the matching lines in order of first appearance, and
    /// per key its matching lines and the first one's number
    pub keys: Vec<Vec<u8>>,
    pub key_counts: Vec<u64>,
    pub key_first: Vec<u64>,
    /// with the run's marks: matching lines per hour (hours since 1970, UTC); `untimed`: the
    /// matching lines without a clock time (or all of them, without marks)
    pub hours: BTreeMap<i64, u64>,
    pub untimed: u64,
    /// the minutes of the matching lines, in runs; `minutes_cut`: too many runs to keep
    pub minutes: Vec<MinuteRun>,
    pub minutes_cut: bool,
    /// with levels: matching lines per level (catalog::level_code)
    pub levels: [u64; 5],
    /// with groups: the run's first `keep.groups` keys' groups, from the start line on, and
    /// the first matching line after them (the start of the next answer)
    pub groups: Vec<Group>,
    pub page_end: Option<u64>,
}

impl RunHits {
    /// The earliest and the latest minute of the matching lines from line `from` on (None:
    /// none of them has a clock time, or too many minute runs were seen to tell).
    pub fn minutes_from(&self, from: u64) -> Option<(i64, i64)> {
        if self.minutes_cut {
            return None;
        }
        let k = self.minutes.partition_point(|m| m.1 < from);
        let mut span: Option<(i64, i64)> = None;
        for &(_, _, m) in &self.minutes[k..] {
            if let Some(m) = m {
                span = Some(span.map_or((m, m), |(a, b)| (a.min(m), b.max(m))));
            }
        }
        span
    }
}

/// The order of two plain decimals (msgkey's `<num>` values: an optional sign, digits, an
/// optional fraction), exactly, without turning them into floating-point numbers.
pub fn dec_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn parts(x: &[u8]) -> (bool, &[u8], &[u8]) {
        let (neg, m) = match x.first() {
            Some(b'-') => (true, &x[1..]),
            Some(b'+') => (false, &x[1..]),
            _ => (false, x),
        };
        let (i, f) = match m.iter().position(|&c| c == b'.') {
            Some(p) => (&m[..p], &m[p + 1..]),
            None => (m, &m[m.len()..]),
        };
        let i = &i[i.iter().position(|&c| c != b'0').unwrap_or(i.len())..];
        let f = &f[..f.iter().rposition(|&c| c != b'0').map_or(0, |p| p + 1)];
        (neg && !(i.is_empty() && f.is_empty()), i, f)
    }
    fn mag((_, i1, f1): (bool, &[u8], &[u8]), (_, i2, f2): (bool, &[u8], &[u8])) -> Ordering {
        i1.len().cmp(&i2.len()).then(i1.cmp(i2)).then(f1.cmp(f2))
    }
    let (pa, pb) = (parts(a), parts(b));
    match (pa.0, pb.0) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => mag(pa, pb),
        (true, true) => mag(pb, pa),
    }
}

/// The writer of a run's records, in order (search_archive's `write`).
struct Writer<'m> {
    from: u64,
    keep: Keep,
    base: u64,
    kept_bytes: usize,
    hits: RunHits,
    /// what each match record carries besides its line (as scan_lines wrote it)
    keys: bool,
    level: bool,
    /// the run's marks: each match's minute
    walk: Option<crate::catalog::MarkWalk<'m>>,
    key_at: HashMap<Vec<u8>, u32>,
    /// the group of each key in this run's groups
    group_of: HashMap<u32, usize>,
    /// a match's values: (place in the key, the number's bytes in the record)
    vals: Vec<(u32, Option<(usize, usize)>)>,
}

impl<'m> Writer<'m> {
    fn new(from: u64, keep: Keep, keys: bool, level: bool, marks: Option<&'m [crate::catalog::Mark]>) -> Writer<'m> {
        Writer {
            from: from.max(1),
            keep,
            base: 0,
            kept_bytes: 0,
            hits: RunHits::default(),
            keys,
            level,
            walk: marks.map(crate::catalog::MarkWalk::new),
            key_at: HashMap::new(),
            group_of: HashMap::new(),
            vals: Vec::new(),
        }
    }

    fn take(&mut self, b: &[u8]) -> Result<(), String> {
        let bad = || "internal error: bad search record".to_string();
        let mut p = 0;
        while p < b.len() {
            let tag = b[p];
            p += 1;
            match tag {
                TAG_END => self.base += get_varint(b, &mut p).ok_or_else(bad)?,
                TAG_MATCH => {
                    let nl = get_varint(b, &mut p).ok_or_else(bad)?;
                    let len = get_varint(b, &mut p).ok_or_else(bad)? as usize;
                    let flag = *b.get(p).ok_or_else(bad)?;
                    p += 1;
                    let bytes = if flag == 1 {
                        let k = get_varint(b, &mut p).ok_or_else(bad)? as usize;
                        let s = b.get(p..p + k).ok_or_else(bad)?;
                        p += k;
                        Some(s)
                    } else {
                        None
                    };
                    let mut key = None;
                    self.vals.clear();
                    if self.keys {
                        let k = get_varint(b, &mut p).ok_or_else(bad)? as usize;
                        key = Some(b.get(p..p + k).ok_or_else(bad)?);
                        p += k;
                        if self.keep.groups > 0 {
                            let n = get_varint(b, &mut p).ok_or_else(bad)?;
                            for _ in 0..n {
                                let at = get_varint(b, &mut p).ok_or_else(bad)? as u32;
                                let f = *b.get(p).ok_or_else(bad)?;
                                p += 1;
                                let num = if f == 1 {
                                    let k = get_varint(b, &mut p).ok_or_else(bad)? as usize;
                                    b.get(p..p + k).ok_or_else(bad)?;
                                    p += k;
                                    Some((p - k, p))
                                } else {
                                    None
                                };
                                self.vals.push((at, num));
                            }
                        }
                    }
                    let level = if self.level {
                        let l = *b.get(p).ok_or_else(bad)?;
                        p += 1;
                        Some(l as usize)
                    } else {
                        None
                    };
                    let line = 1 + self.base + nl;
                    if line < self.from {
                        continue;
                    }
                    self.note(b, line, len, bytes, key, level);
                }
                _ => return Err(bad()),
            }
        }
        Ok(())
    }

    /// One matching line at or after the start line.
    fn note(&mut self, b: &[u8], line: u64, len: usize, bytes: Option<&[u8]>, key: Option<&[u8]>, level: Option<usize>) {
        let h = &mut self.hits;
        h.count += 1;
        // its minute (from the run's marks): per hour, and the runs of minutes
        let minute = self.walk.as_mut().and_then(|w| w.at(line));
        match minute {
            Some(m) => *h.hours.entry(crate::catalog::hour_of(m)).or_insert(0) += 1,
            None => h.untimed += 1,
        }
        let full = h.minutes.len() >= MINUTE_RUNS_MAX;
        match h.minutes.last_mut() {
            Some(r) if r.2 == minute => r.1 = line,
            _ if full => h.minutes_cut = true,
            _ => h.minutes.push((line, line, minute)),
        }
        if let Some(l) = level
            && l < 5
        {
            h.levels[l] += 1;
        }
        let id = key.map(|k| match self.key_at.get(k) {
            Some(&i) => {
                h.key_counts[i as usize] += 1;
                i
            }
            None => {
                let i = h.keys.len() as u32;
                self.key_at.insert(k.to_vec(), i);
                h.keys.push(k.to_vec());
                h.key_counts.push(1);
                h.key_first.push(line);
                i
            }
        });
        if self.keep.groups > 0 {
            // lines collapsed by key: a line joins its key's group, or starts one while there
            // is room; the first line with no room ends this run's groups
            let id = id.unwrap_or(0);
            if h.page_end.is_some() {
                return;
            }
            let num = |v: Option<(usize, usize)>| v.map(|(a, e)| &b[a..e]);
            match self.group_of.get(&id) {
                Some(&g) => {
                    let gr = &mut h.groups[g];
                    gr.count += 1;
                    gr.last = line;
                    let mut vi = 0;
                    for r in gr.ranges.iter_mut() {
                        while vi < self.vals.len() && self.vals[vi].0 < r.at {
                            vi += 1;
                        }
                        match self.vals.get(vi).filter(|v| v.0 == r.at).and_then(|v| num(v.1)) {
                            // (the same unit, or none, in every line)
                            Some(n) if r.ok && num_parts(n).1 == num_parts(&r.min).1 => {
                                if dec_cmp(num_parts(n).0, num_parts(&r.min).0).is_lt() {
                                    r.min = n.to_vec();
                                }
                                if dec_cmp(num_parts(n).0, num_parts(&r.max).0).is_gt() {
                                    r.max = n.to_vec();
                                }
                            }
                            _ => r.ok = false,
                        }
                    }
                }
                None if h.groups.len() >= self.keep.groups => h.page_end = Some(line),
                None => {
                    let ranges = self
                        .vals
                        .iter()
                        .map(|&(at, v)| {
                            let n = num(v).map(<[u8]>::to_vec);
                            Range { at, ok: n.is_some(), min: n.clone().unwrap_or_default(), max: n.unwrap_or_default() }
                        })
                        .collect();
                    let first = Hit { line, bytes: bytes.map(<[u8]>::to_vec).unwrap_or_default(), len, missing: bytes.is_none() };
                    self.group_of.insert(id, h.groups.len());
                    h.groups.push(Group { key: id, first, count: 1, last: line, ranges });
                }
            }
            return;
        }
        if h.first_unkept.is_none() {
            if h.kept.len() < self.keep.lines && self.kept_bytes < self.keep.bytes {
                self.kept_bytes += len.min(self.keep.line_bytes);
                h.kept.push(Hit { line, bytes: bytes.map(<[u8]>::to_vec).unwrap_or_default(), len, missing: bytes.is_none() });
            } else {
                h.first_unkept = Some(line);
            }
        }
    }
}

/// How one run is searched.
pub struct RunOpts<'a> {
    /// the first line that counts (a cursor; 1 = from the start)
    pub from: u64,
    pub keep: Keep,
    pub threads: usize,
    pub deadline: Instant,
    pub stop: &'a AtomicBool,
    /// the run's catalog marks: each match's minute (counts per hour, time ranges)
    pub marks: Option<&'a [crate::catalog::Mark]>,
    /// each match's level (count by level)
    pub level: Option<&'a LevelCtx>,
}

const STOPPED: &str = "\u{0}stopped";
const IDX_DIFFERS: &str = "\u{0}the index does not fit";

/// Search one run's archive (a file archive made by fewer-pack) for the query.
pub fn search_run(path: &Path, q: &Query, o: &RunOpts) -> RunHits {
    let r = match search_archive_file(path, q, o, true) {
        Err(e) if e == IDX_DIFFERS => search_archive_file(path, q, o, false),
        r => r,
    };
    match r {
        Ok(h) => h,
        Err(e) => RunHits { error: Some(e), ..Default::default() },
    }
}

fn search_archive_file(path: &Path, q: &Query, o: &RunOpts, use_idx: bool) -> Result<RunHits, String> {
    let (ar, fmd) = grep::open_archive(path.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (the tools read file archives)".into());
    }
    grep::parse_file_meta(&ar.meta)?;
    let len = fmd.map_or(0, |m| m.len());
    let ip = grep::idx_path(path.as_os_str());
    // the run's .idx: the '\n' counts of blocks left unopened; without one that fits,
    // every block is opened
    let tab = if use_idx { IdxTable::load(&ip, len).ok() } else { None };
    let ix = tab.as_ref().filter(|_| !q.keys.is_empty()).and_then(|_| index::load(&ip, len));
    // the run's DNA index: read only when the .idx leaves a block open that
    // may be left unopened; and the blocks the .idx leaves unopened (the debug log)
    let dna = crate::dna::Search::new(path, &q.dna);
    let by_idx = AtomicUsize::new(0);
    let idx_ok = AtomicBool::new(tab.is_some());
    let may = |p: &[u8], tw: Option<(&[u8], &[u8])>| !idx_ok.load(Ordering::SeqCst) || q.flt.may(p, tw);
    let from = o.from.max(1);
    let starts = tab.as_ref().map(IdxTable::starts).unwrap_or_default();
    let skip = |g: usize, h: (u64, u64, u32)| -> Option<Vec<(usize, bool)>> {
        let t = tab.as_ref()?;
        let mut r = match &ix {
            Some(ix) if t.segs.get(g).is_some_and(|s| s.0 == h) => ix.skips(g, h, &q.keys),
            _ => t.no_skips(g, h),
        };
        // a cursor: the blocks wholly before its line are not opened (their lines only counted)
        if let (Some(v), true) = (r.as_mut(), from > 1) {
            let first = t.segs[g].1;
            for (i, s) in v.iter_mut().enumerate() {
                let k = first + i;
                let b = t.blocks[k];
                if b.ends && starts[k] + b.nl <= from && t.edge(k) {
                    s.1 = true;
                }
            }
        }
        if let Some(v) = r.as_mut() {
            // the DNA index: more blocks no line of which can hold the text, also only at a
            // '\n' edge, as the .idx's
            let first = t.segs[g].1;
            by_idx.fetch_add(v.iter().filter(|s| s.1).count(), Ordering::Relaxed);
            let blocks: Vec<(u64, bool, bool)> = (first..first + v.len()).map(|k| (t.blocks[k].raw, t.blocks[k].ends, t.edge(k))).collect();
            dna.skips(g, h, &blocks, v);
        }
        if r.is_none() {
            // (set before any block of this segment is queued: nothing more is skipped)
            idx_ok.store(false, Ordering::SeqCst);
        }
        r
    };
    let keep = o.keep;
    let lines = |text: &[u8], _f: usize, out: &mut Vec<u8>| scan_lines(text, q, keep, o.level, out);
    let search = |text: &[u8], runs: &[Run], out: &mut Vec<u8>| -> usize { runs.iter().map(|&(a, b, f)| lines(&text[a..b], f, out)).sum() };
    let mut skipped = |ord: usize, size: usize, ents: &mut Vec<Ent>| -> Result<(), String> {
        let b = tab.as_ref().and_then(|t| t.blocks.get(ord)).ok_or(IDX_DIFFERS)?;
        if b.raw != size as u64 || !idx_ok.load(Ordering::SeqCst) {
            return Err(IDX_DIFFERS.into());
        }
        ents.push(Ent::Out(end_record(b.nl), 0));
        Ok(())
    };
    // the time limit applies once the lines up to the start line are behind (so every call
    // moves on): with the .idx, from the first block that starts after it; without one, after
    // the first block of a run searched from its start, and never in a run a cursor points
    // into (its lines are not known before it is read to the end)
    let check = |k: usize| -> Result<(), String> {
        if let Some(ms) = slow_blocks() {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let past = match starts.get(k) {
            Some(&s) if idx_ok.load(Ordering::SeqCst) => s > from,
            _ => k > 0 && from == 1,
        };
        if o.stop.load(Ordering::Relaxed) || (past && Instant::now() >= o.deadline) {
            return Err(STOPPED.into());
        }
        Ok(())
    };
    let mut planner = Planner::new(vec![u64::MAX], &lines, Some(&mut skipped), Some(&check));
    // (the blocks, and those opened, for the debug log)
    let (mut blocks, mut opened) = (0usize, 0usize);
    let mut plan = |seg: Option<&[u8]>, size: usize, ents: &mut Vec<Ent>| {
        blocks += 1;
        opened += usize::from(seg.is_some());
        planner.plan(seg, size, ents)
    };
    let mut w = Writer::new(o.from, keep, q.with_keys, o.level.is_some(), o.marks);
    // (the hits of a search that stopped keep their bytes as kept: the answer shows only the
    // ones with bytes)
    let mut write = |b: &[u8], _n: usize| w.take(b);
    let idx_fn = tab.as_ref().map(|_| &skip as container::IndexSkip);
    let r = container::search_archive(ar, o.threads.max(1), &may, idx_fn, &mut plan, &search, &mut write);
    crate::mcp::debug(&format!(
        "find: {}: {blocks} blocks: {opened} opened, {} left unopened by the .idx, {} more by the DNA index",
        path.display(),
        by_idx.load(Ordering::Relaxed),
        dna.ruled_out.load(Ordering::Relaxed)
    ));
    match r {
        Ok(()) => {}
        Err(e) if e == STOPPED => {
            w.hits.stopped = true;
            w.hits.lines = w.base;
            if o.stop.load(Ordering::Relaxed) {
                // cancelled: the request gets no answer, so nothing is read back; the hits
                // end before the first one without its bytes
                if let Some(k) = w.hits.kept.iter().position(|h| h.missing) {
                    w.hits.first_unkept = Some(w.hits.kept[k].line);
                    w.hits.kept.truncate(k);
                }
            } else {
                // the time limit: the hits found before it that a job kept no bytes of are
                // read back by line number, as below (at most `limit` lines). Ending the hits
                // before them instead could hand back the same cursor forever: a job keeps
                // the bytes of its first matches, and when more than `limit` of them lie
                // before the cursor's line in the same job, the hit at the cursor never has
                // its bytes, call after call.
                fill_missing(path, &mut w.hits, keep.line_bytes, o.threads)?;
            }
            return Ok(w.hits);
        }
        Err(e) => return Err(e),
    }
    let mut last = false;
    if let Some((f, carry)) = planner.rest() {
        let mut buf = Vec::new();
        lines(&carry, f, &mut buf);
        w.take(&buf)?;
        last = true;
    }
    w.hits.lines = w.base + last as u64;
    fill_missing(path, &mut w.hits, keep.line_bytes, o.threads)?;
    Ok(w.hits)
}

/// Search a run in progress: its capture's first `size` bytes (its
/// size when the request began), in runs of whole lines (capture::scan), each searched as a
/// job of an archive is (scan_lines), the records taken by the same Writer: line numbers
/// and counts are those the run's archive will give. The lines before the cursor's line are
/// only counted. A capture that is gone was packed meanwhile: `archive` is searched instead.
pub fn search_capture(path: &Path, size: u64, archive: &Path, q: &Query, o: &RunOpts) -> RunHits {
    let from = o.from.max(1);
    let mut w = Writer::new(o.from, o.keep, q.with_keys, o.level.is_some(), o.marks);
    let r = crate::capture::scan(path, size, from, &mut |first, text| {
        if let Some(ms) = slow_blocks() {
            std::thread::sleep(Duration::from_millis(ms));
        }
        // (as in an archive: the time limit applies once the cursor's line is behind)
        if o.stop.load(Ordering::Relaxed) || (first > from && Instant::now() >= o.deadline) {
            return Ok(false);
        }
        let nl = count_nl(text);
        let out = if text.ends_with(b"\n") && first + nl <= from {
            end_record(nl)
        } else {
            let mut out = Vec::new();
            scan_lines(text, q, o.keep, o.level, &mut out);
            out
        };
        w.take(&out)?;
        Ok(true)
    });
    let s = match r {
        Ok(s) => s,
        Err(e) if e == crate::capture::GONE => return search_run(archive, q, o),
        Err(e) => return RunHits { error: Some(e), ..Default::default() },
    };
    w.hits.stopped = s.stopped;
    w.hits.lines = w.base + (!s.stopped && s.part) as u64;
    if s.stopped && o.stop.load(Ordering::Relaxed) {
        // cancelled: no answer, so nothing is read back (as search_archive_file)
        if let Some(k) = w.hits.kept.iter().position(|h| h.missing) {
            w.hits.first_unkept = Some(w.hits.kept[k].line);
            w.hits.kept.truncate(k);
        }
        return w.hits;
    }
    if let Err(e) = fill_missing_with(&mut w.hits, o.keep.line_bytes, &|want| crate::capture::lines(path, size, want)) {
        return RunHits { error: Some(e), ..Default::default() };
    }
    w.hits
}

/// Read the bytes of the hits (and groups' first lines) a job did not keep, by line number.
fn fill_missing(path: &Path, rh: &mut RunHits, line_bytes: usize, threads: usize) -> Result<(), String> {
    fill_missing_with(rh, line_bytes, &|want| crate::fetch::fetch(path, want, threads.max(1)))
}

/// What reads lines by number (1-based ranges) for fill_missing: an archive's (fetch.rs), or a
/// capture's (capture::lines).
type ReadLines<'a> = &'a dyn Fn(&[(u64, u64)]) -> Result<Vec<(u64, Vec<u8>)>, String>;

/// fill_missing with the lines read by `read` (an archive's, or a capture's).
fn fill_missing_with(rh: &mut RunHits, line_bytes: usize, read: ReadLines) -> Result<(), String> {
    let mut hits: Vec<&mut Hit> = rh.kept.iter_mut().chain(rh.groups.iter_mut().map(|g| &mut g.first)).filter(|h| h.missing).collect();
    hits.sort_by_key(|h| h.line);
    let want: Vec<(u64, u64)> = hits.iter().map(|h| (h.line, h.line)).collect();
    if want.is_empty() {
        return Ok(());
    }
    let got = read(&want)?;
    let mut gi = 0;
    for h in hits.iter_mut() {
        while gi < got.len() && got[gi].0 < h.line {
            gi += 1;
        }
        let Some((l, b)) = got.get(gi) else { break };
        if *l == h.line {
            let b = b.strip_suffix(b"\n").unwrap_or(b);
            h.bytes = b[..b.len().min(line_bytes)].to_vec();
            h.missing = false;
        }
    }
    if hits.iter().any(|h| h.missing) {
        return Err("internal error: a matching line could not be read back by its number".into());
    }
    Ok(())
}

/// A test switch: FEWER_TEST_SLOW_BLOCK_MS makes every block of a search wait this long, so
/// that checks can reach the time limit and a cancel on purpose. Never set in normal use.
fn slow_blocks() -> Option<u64> {
    static V: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FEWER_TEST_SLOW_BLOCK_MS").ok().and_then(|v| v.parse().ok()).filter(|&v| v > 0))
}

// ---------------------------------------------------------------- many runs

/// One run to search: its archive, its text size, the first line that counts.
pub struct Job {
    pub path: PathBuf,
    pub bytes: u64,
    pub from: u64,
    /// the run's catalog marks (each match's minute), when the answer needs times
    pub marks: Option<Vec<crate::catalog::Mark>>,
    /// the run's log type (each match's level), when the answer needs levels
    pub level: Option<LevelCtx>,
    /// a run in progress: its capture and the capture's size when the request began (`path`
    /// is then the archive it will have, searched if the capture is packed meanwhile)
    pub capture: Option<(PathBuf, u64)>,
}

/// Runs with at least this much text are searched one at a time with all threads.
const BIG_RUN: u64 = 8 << 20;

struct PoolState {
    next: usize,
    consumed: usize,
    stop: bool,
    results: Vec<Option<RunHits>>,
}

/// Search every job's run, in order: `consume(k, hits)` gets each run's result in job order
/// and returns false to stop there. Runs still waiting when the time limit passes, or after
/// a stop, are not searched (consume never sees them; the return value is the number of jobs
/// handed to consume). `full`: once set (by consume), runs started later keep no bytes.
pub fn search_scope(jobs: &[Job], q: &Query, keep: Keep, deadline: Instant, stop: &AtomicBool, full: &AtomicBool, consume: &mut dyn FnMut(usize, RunHits) -> bool) -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let workers = cores.min(jobs.len()).max(1);
    let window = 2 * cores;
    let st = Mutex::new(PoolState { next: 0, consumed: 0, stop: false, results: (0..jobs.len()).map(|_| None).collect() });
    let (cv, big) = (Condvar::new(), Mutex::new(()));
    let (st, cv, big) = (&st, &cv, &big);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(move || {
                loop {
                    let k = {
                        let mut g = st.lock().unwrap();
                        while !g.stop && g.next < jobs.len() && g.next >= g.consumed + window {
                            g = cv.wait(g).unwrap();
                        }
                        // (the first job always starts, so every call moves on)
                        if g.stop || g.next >= jobs.len() || stop.load(Ordering::Relaxed) || (g.next > 0 && Instant::now() >= deadline) {
                            g.stop = true;
                            cv.notify_all();
                            return;
                        }
                        g.next += 1;
                        g.next - 1
                    };
                    let j = &jobs[k];
                    let kp = if full.load(Ordering::SeqCst) { Keep::NONE } else { keep };
                    // (a panic becomes this run's error: the results must all come in)
                    let one = || {
                        let o = |threads| RunOpts { from: j.from, keep: kp, threads, deadline, stop, marks: j.marks.as_deref(), level: j.level.as_ref() };
                        if let Some((cap, size)) = &j.capture {
                            // (a capture is read in order, on this thread)
                            search_capture(cap, *size, &j.path, q, &o(1))
                        } else if j.bytes >= BIG_RUN {
                            let _one = big.lock().unwrap_or_else(|e| e.into_inner());
                            search_run(&j.path, q, &o(cores))
                        } else {
                            search_run(&j.path, q, &o(1))
                        }
                    };
                    let h = std::panic::catch_unwind(std::panic::AssertUnwindSafe(one))
                        .unwrap_or_else(|p| RunHits { error: Some(format!("internal error in search: {}", panic_text(&p))), ..Default::default() });
                    let mut g = st.lock().unwrap();
                    g.results[k] = Some(h);
                    cv.notify_all();
                }
            });
        }
        let mut handed = 0;
        for k in 0..jobs.len() {
            let r = {
                let mut g = st.lock().unwrap();
                loop {
                    if let Some(r) = g.results[k].take() {
                        break Some(r);
                    }
                    if g.stop && g.next <= k {
                        break None;
                    }
                    let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50)).max(Duration::from_millis(1));
                    g = cv.wait_timeout(g, wait).unwrap().0;
                    if (stop.load(Ordering::Relaxed) || (k > 0 && Instant::now() >= deadline)) && g.next <= k {
                        g.stop = true;
                        cv.notify_all();
                    }
                }
            };
            let Some(r) = r else { break };
            handed += 1;
            {
                let mut g = st.lock().unwrap();
                g.consumed = k + 1;
                cv.notify_all();
            }
            if !consume(k, r) {
                break;
            }
        }
        let mut g = st.lock().unwrap();
        g.stop = true;
        cv.notify_all();
        drop(g);
        handed
    })
}

/// The message of a caught panic.
pub fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut(usize) -> usize {
        let mut x = seed;
        move |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n.max(1) as u64) as usize
        }
    }

    /// Matcher finds exactly the lines search::grep_lines prints: random texts and needles,
    /// with and without -i, longer than 63 bytes, empty, holding '\n'.
    #[test]
    fn matcher_same_as_grep_lines() {
        let mut rnd = rng(0x2545_f491_4f6c_dd1d);
        let alpha = b"aAbB01 \t.:-_\nxyzXYZ\xc3\xa9\r";
        let mut checked = 0;
        for round in 0..4000 {
            let n = rnd(400);
            let text: Vec<u8> = (0..n).map(|_| alpha[rnd(alpha.len())]).collect();
            let nd: Vec<u8> = match round % 10 {
                0 => Vec::new(),
                1 => (0..70).map(|_| alpha[rnd(4)]).collect(),
                2 => b"a\nb".to_vec(),
                _ if !text.is_empty() && rnd(3) > 0 => {
                    let a = rnd(text.len());
                    text[a..(a + 1 + rnd(6)).min(text.len())].to_vec()
                }
                _ => (0..1 + rnd(4)).map(|_| alpha[rnd(alpha.len())]).collect(),
            };
            for icase in [false, true] {
                let mut want = Vec::new();
                let wn = search::grep_lines(&text, &search::Needle::new(&nd, icase), b"", &mut want);
                let q = Query::new(&nd, icase, false);
                let mut rec = Vec::new();
                let all = Keep { lines: usize::MAX, bytes: usize::MAX, line_bytes: usize::MAX, groups: 0 };
                let got_n = scan_lines(&text, &q, all, None, &mut rec);
                let mut w = Writer::new(1, all, false, false, None);
                w.take(&rec).unwrap();
                assert_eq!(got_n, wn);
                assert_eq!(w.hits.count as usize, wn);
                let mut got = Vec::new();
                let lines: Vec<&[u8]> = text.split(|&c| c == b'\n').collect();
                for h in &w.hits.kept {
                    got.extend_from_slice(&h.bytes);
                    got.push(b'\n');
                    // the line number names the same line
                    assert_eq!(lines[h.line as usize - 1], &h.bytes[..]);
                }
                assert_eq!(got, want, "{:?} in {:?}", String::from_utf8_lossy(&nd), String::from_utf8_lossy(&text));
                assert_eq!(w.base, text.iter().filter(|&&c| c == b'\n').count() as u64);
                checked += 1;
            }
        }
        assert!(checked == 8000);
    }

    /// `whole`: a line counts only when one of its matches has no word byte right before
    /// or after it (every match in the line is tried).
    #[test]
    fn whole_matches() {
        let q = Query::new(b"id-7", false, true);
        let text = b"x id-7 y\nid-77\nzid-7\n(id-7)\nid-7\nid-77 id-7\nid-7_\n";
        let mut rec = Vec::new();
        let k100 = Keep { lines: 100, bytes: 1 << 20, line_bytes: 1000, groups: 0 };
        let n = scan_lines(text, &q, k100, None, &mut rec);
        let mut w = Writer::new(1, k100, false, false, None);
        w.take(&rec).unwrap();
        assert_eq!(n, 4);
        assert_eq!(w.hits.kept.iter().map(|h| h.line).collect::<Vec<_>>(), vec![1, 4, 5, 6]);
        // from a cursor line on, and within the keep limits
        let mut w = Writer::new(5, Keep { lines: 1, bytes: 1 << 20, line_bytes: 3, groups: 0 }, false, false, None);
        w.take(&rec).unwrap();
        assert_eq!(w.hits.count, 2);
        assert_eq!(w.hits.kept.len(), 1);
        assert_eq!((w.hits.kept[0].line, &w.hits.kept[0].bytes[..], w.hits.kept[0].len), (5, &b"id-7"[..], 4));
        assert_eq!(w.hits.first_unkept, Some(6));
        // a job keeps at most line_bytes of a line
        let mut rec = Vec::new();
        let k9 = Keep { lines: 9, bytes: 99, line_bytes: 3, groups: 0 };
        scan_lines(b"x id-7 yyyy\n", &q, k9, None, &mut rec);
        let mut w = Writer::new(1, k9, false, false, None);
        w.take(&rec).unwrap();
        assert_eq!((&w.hits.kept[0].bytes[..], w.hits.kept[0].len, w.base), (&b"x i"[..], 11, 1));
    }

    /// Exact decimals: signs, leading and trailing zeros, fractions, long numbers.
    #[test]
    fn decimals_in_order() {
        use std::cmp::Ordering::*;
        for (a, b, o) in [
            ("12", "48", Less),
            ("9", "10", Less),
            ("-3", "2", Less),
            ("-10", "-9", Less),
            ("007", "7", Equal),
            ("1.50", "1.5", Equal),
            ("-0", "0", Equal),
            ("+5", "5", Equal),
            ("2.25", "2.3", Less),
            ("123456789012345678901", "123456789012345678900", Greater),
            ("0.001", "-0.002", Greater),
        ] {
            assert_eq!(dec_cmp(a.as_bytes(), b.as_bytes()), o, "{a} {b}");
            assert_eq!(dec_cmp(b.as_bytes(), a.as_bytes()), o.reverse(), "{b} {a}");
        }
    }

    /// Lines collapsed by key: a group per key in order of first appearance, its count, its
    /// last line and the ranges of its measured values; the first line of a key beyond the
    /// limit ends the groups; minutes and hours come from the marks; keys are counted after
    /// the end too.
    #[test]
    fn groups_ranges_and_minutes() {
        let text = b"a took 12 ms id 7\nb other\na took 48 ms id 9\na took 30 ms id 1\nc tooth\na took 5ms id 2\nb other\n";
        let mut q = Query::new(b"o", false, false);
        q.with_keys = true;
        let keep = Keep { lines: 1, bytes: 1 << 20, line_bytes: 1000, groups: 2 };
        let mut rec = Vec::new();
        let n = scan_lines(text, &q, keep, None, &mut rec);
        assert_eq!(n, 7);
        let marks = [crate::catalog::Mark { line: 1, minute: Some(600) }, crate::catalog::Mark { line: 3, minute: Some(661) }, crate::catalog::Mark { line: 6, minute: None }];
        let mut w = Writer::new(1, keep, true, false, Some(&marks));
        w.take(&rec).unwrap();
        let h = &w.hits;
        assert_eq!(h.count, 7);
        assert_eq!(h.groups.len(), 2);
        let (g0, g1) = (&h.groups[0], &h.groups[1]);
        assert_eq!((g0.first.line, g0.count, g0.last, g0.first.missing), (1, 3, 4, false));
        assert_eq!((g1.first.line, g1.count, g1.last, g1.first.missing), (2, 1, 2, true));
        assert_eq!(h.page_end, Some(5), "the third key starts after the two groups");
        // "12 ms", "48 ms", "30 ms": 12-48; the ids 7, 9, 1 have no unit but are ranged too
        // (answers show only the ones with a unit)
        let r = &g0.ranges;
        assert_eq!(r.len(), 2);
        assert_eq!((&r[0].min[..], &r[0].max[..], r[0].ok), (&b"12"[..], &b"48"[..], true));
        assert_eq!((&r[1].min[..], &r[1].max[..], r[1].ok), (&b"1"[..], &b"9"[..], true));
        assert_eq!(&h.keys[g0.key as usize][..], b"a took <num> ms id <num>");
        // every match is counted per key, also after the groups end
        let per: Vec<(String, u64)> = h.keys.iter().zip(&h.key_counts).map(|(k, n)| (String::from_utf8_lossy(k).into_owned(), *n)).collect();
        assert_eq!(per, vec![("a took <num> ms id <num>".into(), 3), ("b other".into(), 2), ("c tooth".into(), 1), ("a took <num> id <num>".into(), 1)]);
        assert_eq!(h.key_first, vec![1, 2, 5, 6]);
        // minutes: lines 1-2 in minute 600, 3-5 in 661, 6-7 none
        assert_eq!(h.hours.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(), vec![(10, 2), (11, 3)]);
        assert_eq!(h.untimed, 2);
        assert_eq!(h.minutes, vec![(1, 2, Some(600)), (3, 5, Some(661)), (6, 7, None)]);
        assert_eq!(h.minutes_from(1), Some((600, 661)));
        assert_eq!(h.minutes_from(4), Some((661, 661)));
        assert_eq!(h.minutes_from(6), None);
        // from a cursor: lines before it are not counted, groups start there
        let mut w = Writer::new(3, keep, true, false, Some(&marks));
        w.take(&rec).unwrap();
        assert_eq!((w.hits.count, w.hits.groups[0].first.line, w.hits.groups[0].count), (5, 3, 2));
        assert_eq!(w.hits.groups[1].first.line, 5);
        assert_eq!(w.hits.page_end, Some(6));
        // a unit written as a word is part of the key; a unit written right after the number
        // is part of the value, and a group whose values change unit has no range there
        let text = b"x 5 ms\nx 7 ms\nx 9 s\ny 5ms\ny 7s\n";
        let mut rec = Vec::new();
        let mut q = Query::new(b" ", false, false);
        q.with_keys = true;
        let keep = Keep { groups: 10, ..keep };
        scan_lines(text, &q, keep, None, &mut rec);
        let mut w = Writer::new(1, keep, true, false, None);
        w.take(&rec).unwrap();
        let g = &w.hits.groups;
        assert_eq!(g.iter().map(|g| (g.first.line, g.count)).collect::<Vec<_>>(), vec![(1, 2), (3, 1), (4, 2)]);
        assert_eq!((&g[0].ranges[0].min[..], &g[0].ranges[0].max[..], g[0].ranges[0].ok), (&b"5"[..], &b"7"[..], true));
        assert!(!g[2].ranges[0].ok, "5ms and 7s");
    }

    /// Levels of matching lines, read with the run's log type.
    #[test]
    fn levels_of_matches() {
        let text = b"2026-11-18 10:00:00 ERROR boom\n2026-11-18 10:00:01 INFO ok boom\n2026-11-18 10:00:02 boom\n";
        let lc = LevelCtx { profile: crate::formats::detect("app.log", text), zone: crate::times::Zone::Fixed(0), file_ms: 0 };
        let q = Query::new(b"boom", false, false);
        let mut rec = Vec::new();
        scan_lines(text, &q, Keep::NONE, Some(&lc), &mut rec);
        let mut w = Writer::new(1, Keep::NONE, false, true, None);
        w.take(&rec).unwrap();
        assert_eq!(w.hits.levels, [1, 0, 1, 0, 1]);
    }
}
