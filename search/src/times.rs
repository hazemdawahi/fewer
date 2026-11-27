//! Times: "now" (FEWER_NOW for tests), times shown in answers (local: 2026-11-18 10:15:02),
//! the forms `since` and `until` take ("2026-11-18 10:00", ISO 8601, "15m", "2h", "3d",
//! "today", "yesterday"), and the time forms of log lines (below).
//!
//! Time forms: the time of a log line, or of a JSON or logfmt time field, read
//! as Unix milliseconds in UTC.
//!
//! The forms read, each tested on a public sample's shape:
//!
//! | Form | Examples |
//! |---|---|
//! | `Iso`: ISO 8601 and RFC 3339; `T` or a space; a fraction after `.` or `,`; `Z`, an offset, or ` UTC` | 2026-11-18T10:15:02.123Z, 2015-07-29 17:41:44,747 (Zookeeper), 2026-11-18 10:15:02.123 +0100 |
//! | `Slashed`: year first | 2026/11/18 10:15:02 (nginx errors, Go's log), 17/06/09 20:10:40 (Spark: yy/MM/dd) |
//! | `Syslog`: no year | Nov 18 10:15:02, Jul  1 09:00:55, Nov 9 12:01:01 |
//! | `Ctime` | Sun Dec 04 04:47:44 2005 (Apache errors), Wed Oct 11 14:32:52.123456 2000 |
//! | `Access` | 18/Nov/2026:10:00:00 +0000 (Apache and nginx access logs) |
//! | `Php` | 18-Nov-2026 10:15:02 UTC (PHP's error log) |
//! | `Compact` | 081109 203615 (HDFS), 20171223-22:15:29:606 (HealthApp), 2005-06-03-15.42.50.675872 (BGL), 181118 10:15:02 (old MySQL) |
//! | `NoYear` | 03-17 16:13:38.811 (Android), [10.30 16:49:06] (Proxifier) |
//! | `Klog`: a level letter, then no year | I1118 10:15:02.123456 (Kubernetes, glog) |
//! | `Unix` | 10 digits (seconds, a fraction allowed) or 13 (milliseconds) at the start of a line, in a time column, or in a time field (also 16 digits: µs; 19: ns) |
//! | `Audit` | audit(1700000000.123:42) anywhere in the line's first 200 bytes |
//! | `Boot` | [  123.456789] (dmesg): kept as time since boot, not turned into clock time |
//!
//! Rules:
//! - Times without a zone are read in this computer's zone ([`Zone::Local`]: sys.rs's
//!   `from_local` and `local`, the C library's mktime and localtime_r, with daylight saving),
//!   or in a fixed zone given by the caller (tests).
//! - Forms without a year take the year of the file's time, or the year before when their
//!   month is later than the file's month.
//! - Nothing ambiguous is guessed: day/month order never is (slashed dates are year first, and
//!   `dd/Mmm/yyyy` names its month); a date or time out of range is not a time.
//! - A line's time is looked for at the start of one token, the source's [`Place`], which
//!   [`learn_place`] finds from a sample; the arrival time of `run` output without times comes
//!   from the run's marks ([`Arrivals`]).

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

/// Now, in Unix milliseconds: FEWER_NOW when set (tests of "since yesterday"), else the clock.
pub fn now_ms() -> i64 {
    if let Some(v) = std::env::var_os("FEWER_NOW").and_then(|v| v.to_str().and_then(|s| s.trim().parse::<i64>().ok())) {
        return v;
    }
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Days since 1970-01-01 of a calendar date (proleptic Gregorian; H. Hinnant's method).
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The UTC calendar time of Unix seconds: (year, month, day, hour, minute, second).
pub fn civil(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400 + (m <= 2) as i64) as i32;
    (y, m, d, (sod / 3600) as u32, (sod / 60 % 60) as u32, (sod % 60) as u32)
}

/// A UTC time as the store's config and the token card's split write it ("2026-11-26T04:44:26",
/// "2026-11-23T10:00", "2026-11-23"; a trailing Z allowed), shown as answers show times: local, to
/// the minute ("2026-11-25 21:44"); the text as it is, with "UTC", when it is not one.
pub fn local_of_utc(s: &str) -> String {
    let t = s.trim().trim_end_matches('Z');
    let (d, tm) = t.split_once(['T', ' ']).unwrap_or((t, "00:00"));
    let num = |x: &str| x.parse::<i64>().ok();
    let dp: Vec<Option<i64>> = d.split('-').map(num).collect();
    let tp: Vec<Option<i64>> = tm.split(':').map(|x| num(x.split('.').next().unwrap_or(""))).collect();
    match (dp.as_slice(), tp.as_slice()) {
        ([Some(y), Some(m), Some(dd)], [Some(h), Some(mi), rest @ ..]) if (1..=12).contains(m) && (1..=31).contains(dd) && *h < 24 && *mi < 60 => {
            let sec = rest.first().copied().flatten().unwrap_or(0);
            let secs = days_from_civil(*y as i32, *m as u32, *dd as u32) * 86_400 + h * 3_600 + mi * 60 + sec;
            fmt_local(secs * 1000)[..16].to_string()
        }
        _ => format!("{} UTC", s.replacen('T', " ", 1)),
    }
}

/// A time as answers show it: local, "2026-11-18 10:15:02".
pub fn fmt_local(ms: i64) -> String {
    let (y, mo, d, h, mi, s, _) = crate::sys::local(ms.div_euclid(1000));
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// A duration as answers show it: "0.8 s", "12 s", "3 min 5 s", "2 h 4 min".
pub fn fmt_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 10_000 {
        format!("{}.{} s", ms / 1000, ms % 1000 / 100)
    } else if ms < 60_000 {
        format!("{} s", ms / 1000)
    } else if ms < 3_600_000 {
        format!("{} min {} s", ms / 60_000, ms / 1000 % 60)
    } else {
        format!("{} h {} min", ms / 3_600_000, ms / 60_000 % 60)
    }
}

fn num(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// A time given to `since` or `until`, in Unix ms, read against `now` (ms):
/// - relative: "45s", "15m", "2h", "3d" (that long before now), "now", "today" (local
///   midnight), "yesterday" (local midnight the day before);
/// - "2026-11-18", "2026-11-18 10:00", "2026-11-18 10:00:05" (local time);
/// - ISO 8601 / RFC 3339: "2026-11-18T10:00:00", with a fraction, and with "Z" or an offset
///   ("+02:00", "-0500"); without a zone it is local time.
pub fn parse_when(s: &str, now: i64) -> Result<i64, String> {
    let t = s.trim();
    let bad = || format!("cannot read the time '{s}': use \"2026-11-18 10:00\", ISO 8601, \"15m\", \"2h\", \"3d\", \"today\" or \"yesterday\"");
    // every form read here is ASCII, and the text below is cut by byte places: anything
    // else is not a time (cutting inside a character such as 'é' would panic)
    if !t.is_ascii() {
        return Err(bad());
    }
    if t.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if t.eq_ignore_ascii_case("today") || t.eq_ignore_ascii_case("yesterday") {
        let (y, mo, d, ..) = crate::sys::local(now.div_euclid(1000));
        let back = if t.eq_ignore_ascii_case("today") { 0 } else { 1 };
        // the local midnight of that day (found by its calendar date)
        let (y2, mo2, d2, ..) = civil((days_from_civil(y, mo, d) - back) * 86_400);
        return crate::sys::from_local(y2, mo2, d2, 0, 0, 0).map(|v| v * 1000).ok_or_else(bad);
    }
    // relative: a number and a unit
    if let Some(unit) = t.chars().last().filter(|c| c.is_ascii_alphabetic())
        && let Some(n) = num(&t[..t.len() - 1])
    {
        let mul: i64 = match unit.to_ascii_lowercase() {
            's' => 1000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            'w' => 7 * 86_400_000,
            _ => return Err(bad()),
        };
        return Ok(now - n as i64 * mul);
    }
    // a date, then an optional time
    let b = t.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    let (y, mo, d) = (num(&t[0..4]).ok_or_else(bad)? as i32, num(&t[5..7]).ok_or_else(bad)?, num(&t[8..10]).ok_or_else(bad)?);
    if !(1..=12).contains(&mo) || d == 0 || d > 31 {
        return Err(bad());
    }
    let mut rest = &t[10..];
    let (mut h, mut mi, mut sec, mut frac_ms) = (0u32, 0u32, 0u32, 0i64);
    if !rest.is_empty() {
        let r = rest.as_bytes();
        if r[0] != b'T' && r[0] != b't' && r[0] != b' ' {
            return Err(bad());
        }
        rest = rest[1..].trim_start();
        if rest.len() < 5 || rest.as_bytes()[2] != b':' {
            return Err(bad());
        }
        h = num(&rest[0..2]).ok_or_else(bad)?;
        mi = num(&rest[3..5]).ok_or_else(bad)?;
        rest = &rest[5..];
        if rest.starts_with(':') {
            sec = num(rest.get(1..3).ok_or_else(bad)?).ok_or_else(bad)?;
            rest = &rest[3..];
            if rest.starts_with('.') || rest.starts_with(',') {
                let digits: String = rest[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if digits.is_empty() {
                    return Err(bad());
                }
                let mut ms_text = digits.clone();
                ms_text.truncate(3);
                while ms_text.len() < 3 {
                    ms_text.push('0');
                }
                frac_ms = ms_text.parse().map_err(|_| bad())?;
                rest = &rest[1 + digits.len()..];
            }
        }
        if h > 23 || mi > 59 || sec > 60 {
            return Err(bad());
        }
    }
    let rest = rest.trim();
    let wall = |off_s: i64| (days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + sec) as i64 - off_s) * 1000 + frac_ms;
    if rest.is_empty() {
        return crate::sys::from_local(y, mo, d, h, mi, sec).map(|v| v * 1000 + frac_ms).ok_or_else(bad);
    }
    if rest == "Z" || rest == "z" {
        return Ok(wall(0));
    }
    let sign = match rest.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return Err(bad()),
    };
    let z: String = rest[1..].chars().filter(|&c| c != ':').collect();
    if z.len() != 4 && z.len() != 2 {
        return Err(bad());
    }
    let oh = num(&z[0..2]).ok_or_else(bad)? as i64;
    let om = if z.len() == 4 { num(&z[2..4]).ok_or_else(bad)? as i64 } else { 0 };
    Ok(wall(sign * (oh * 3600 + om * 60)))
}

// ---------------------------------------------------------------------------------------
// The time forms of log lines (the table at the top)

/// A time read from a log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    /// A clock time: Unix milliseconds, UTC.
    Utc(i64),
    /// Time since the computer started (dmesg), in microseconds, kept as written: not turned
    /// into a clock time.
    Boot(i64),
}

impl Stamp {
    /// The clock time in Unix milliseconds; None for a time since boot.
    pub fn utc_ms(self) -> Option<i64> {
        match self {
            Stamp::Utc(ms) => Some(ms),
            Stamp::Boot(_) => None,
        }
    }
}

/// The form a time was written in (see the table at the top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Iso,
    Slashed,
    Syslog,
    Ctime,
    Access,
    Php,
    Compact,
    NoYear,
    Klog,
    Unix,
    Audit,
    Boot,
}

/// A time found in a line: its value, its form, the bytes it takes (`start..end`: an opening
/// '[', '(' or syslog priority before it and the matching ']' or ')' after it count), and
/// whether it was written with its zone (`Z`, an offset or UTC; Unix times count as zoned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub stamp: Stamp,
    pub form: Form,
    pub start: usize,
    pub end: usize,
    pub zoned: bool,
}

/// How times written without a zone are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// This computer's zone, with its daylight saving rules.
    Local,
    /// A fixed offset, in minutes east of UTC.
    Fixed(i32),
}

/// What reading a time needs besides its text: the zone for times written without one, and
/// the file's time for forms written without a year. One Clock serves one source. It keeps a
/// one-entry cache of the local zone's offset, so it is not shared between threads.
#[derive(Debug)]
pub struct Clock {
    zone: Zone,
    /// The file's year and month, in the clock's zone.
    year: i32,
    month: u32,
    /// The last local hour converted (hours since 1970, wall time) and its offset in seconds.
    cache: Cell<(i64, i64)>,
}

impl Clock {
    /// A clock for a file whose time (its modification time, or a run's end) is `file_ms`.
    pub fn new(zone: Zone, file_ms: i64) -> Clock {
        let t = file_ms.div_euclid(1000);
        let off = match zone {
            Zone::Fixed(m) => m as i64 * 60,
            Zone::Local => local_offset_at_utc(t),
        };
        let (year, month, ..) = civil(t + off);
        Clock { zone, year, month, cache: Cell::new((i64::MIN, 0)) }
    }

    /// This computer's zone.
    pub fn local(file_ms: i64) -> Clock {
        Clock::new(Zone::Local, file_ms)
    }

    pub fn zone(&self) -> Zone {
        self.zone
    }

    /// The year of a date written without one: the file's year, or the year before when the
    /// date's month is later than the file's month.
    pub fn year_for(&self, month: u32) -> i32 {
        if month > self.month { self.year - 1 } else { self.year }
    }

    /// UTC milliseconds of a wall-clock time written without a zone. `wall_ms` is that time
    /// counted as if it were UTC.
    pub fn wall_to_utc(&self, wall_ms: i64) -> i64 {
        let off = match self.zone {
            Zone::Fixed(m) => m as i64 * 60,
            Zone::Local => {
                let hour = wall_ms.div_euclid(3_600_000);
                let (h, o) = self.cache.get();
                if h == hour {
                    o
                } else {
                    let o = local_offset_at_wall(wall_ms.div_euclid(1000));
                    self.cache.set((hour, o));
                    o
                }
            }
        };
        wall_ms - off * 1000
    }

    /// The UTC stamp of a date and time; `zone`: seconds east of UTC, when written.
    fn stamp(&self, w: Wall, zone: Option<i64>) -> Option<Stamp> {
        if !w.valid() {
            return None;
        }
        let wall = w.ms();
        Some(Stamp::Utc(match zone {
            Some(off) => wall - off * 1000,
            None => self.wall_to_utc(wall),
        }))
    }
}

/// Where a source's lines hold their time: at the start of token `.0` (tokens are runs of
/// bytes other than space and tab; token 0 starts the line). See [`learn_place`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place(pub u8);

impl Place {
    /// The start of the line.
    pub const START: Place = Place(0);
}

/// A line's time is looked for in its first this many tokens ...
pub const MAX_TOKENS: usize = 8;
/// ... that start before this byte.
pub const MAX_SCAN: usize = 200;

/// The time of a line: at the source's place; else, for a place after the start, at the
/// start; else an `audit(...)` time in the first 200 bytes. None: the line has no time of its
/// own (it takes the previous line's).
pub fn line_time(line: &[u8], place: Place, c: &Clock) -> Option<Found> {
    let k = place.0 as usize;
    let at_token = |k: usize| token_start(line, k).filter(|&p| p < MAX_SCAN).and_then(|p| read_at(line, p, c));
    at_token(k).or_else(|| if k != 0 { at_token(0) } else { None }).or_else(|| find_audit(line, c))
}

/// Learns where a source's lines hold their time, from sample lines (clean text): for each
/// line, the first token (of the first 8) where a time form starts. A token after the start
/// is the place only when it holds the first time of half or more of the sampled lines (HPC,
/// BGL and Thunderbird have Unix seconds in a column; OpenStack has a file name first; access
/// logs have the address and user first). Otherwise the place is the start of the line.
pub fn learn_place(lines: &[&[u8]], c: &Clock) -> Place {
    let mut counts = [0usize; MAX_TOKENS];
    let mut n = 0;
    for l in lines {
        if l.iter().all(|&b| b == b' ' || b == b'\t') {
            continue;
        }
        n += 1;
        for (k, count) in counts.iter_mut().enumerate() {
            let Some(p) = token_start(l, k) else { break };
            if p >= MAX_SCAN {
                break;
            }
            if read_at(l, p, c).is_some() {
                *count += 1;
                break;
            }
        }
    }
    let mut best = 0;
    for k in 1..MAX_TOKENS {
        if counts[k] > counts[best] {
            best = k;
        }
    }
    if best > 0 && counts[best] * 2 >= n { Place(best as u8) } else { Place::START }
}

/// The start of token `k` of a line (tokens are runs of bytes other than space and tab).
pub fn token_start(s: &[u8], k: usize) -> Option<usize> {
    let mut i = 0;
    let mut t = 0;
    loop {
        while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
            i += 1;
        }
        if i >= s.len() {
            return None;
        }
        if t == k {
            return Some(i);
        }
        while i < s.len() && s[i] != b' ' && s[i] != b'\t' {
            i += 1;
        }
        t += 1;
    }
}

/// A time starting at `s[i]`, in any form above. One opening '[' or '(' may come first (the
/// matching closer after the time is taken too), or a syslog priority `<n>` (with an RFC 5424
/// version "1 " after it).
pub fn read_at(s: &[u8], i: usize, c: &Clock) -> Option<Found> {
    let b0 = *s.get(i)?;
    let mut j = i;
    let mut close = 0u8;
    match b0 {
        b'[' => {
            if let Some((stamp, end)) = boot(s, i) {
                return Some(Found { stamp, form: Form::Boot, start: i, end, zoned: false });
            }
            j = i + 1;
            close = b']';
        }
        b'(' => {
            j = i + 1;
            close = b')';
        }
        b'<' => j = pri_end(s, i)?,
        _ => {}
    }
    let (stamp, form, mut end, zoned) = form_at(s, j, c)?;
    if close != 0 && at(s, end, close) {
        end += 1;
    }
    Some(Found { stamp, form, start: i, end, zoned })
}

/// The end of a syslog priority `<n>` (0-191) at `s[i]`, and of an RFC 5424 version "1 "
/// right after it.
pub fn pri_end(s: &[u8], i: usize) -> Option<usize> {
    if !at(s, i, b'<') {
        return None;
    }
    let (p, e) = var(s, i + 1, 3)?;
    if !at(s, e, b'>') || p > 191 {
        return None;
    }
    let mut j = e + 1;
    if digit(s, j) && at(s, j + 1, b' ') {
        j += 2;
    }
    Some(j)
}

/// The text of a time field (JSON or logfmt): a number (see [`number_time`]) or any form
/// above at its start.
pub fn text_time(s: &[u8], c: &Clock) -> Option<Stamp> {
    let t = trim(s);
    if !t.is_empty()
        && t.iter().all(|b| b.is_ascii_digit() || *b == b'.')
        && let Some(st) = number_time(t)
    {
        return Some(st);
    }
    read_at(t, 0, c).map(|f| f.stamp)
}

/// Oldest and newest clock times read from bare numbers (2001-09-09 and 2100-01-01): a
/// number outside them is not a time.
const MIN_MS: i64 = 1_000_000_000_000;
const MAX_MS: i64 = 4_102_444_800_000;

/// A number in a time field (its text): 10 digits are seconds (with or without a fraction),
/// 13 milliseconds, 16 microseconds (journald's `__REALTIME_TIMESTAMP`), 19 nanoseconds; with
/// an exponent, seconds or milliseconds by size. Only times from 2001-09-09 to 2099 are read.
pub fn number_time(s: &[u8]) -> Option<Stamp> {
    let s = trim(s);
    let n = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let ms = if n == s.len() {
        let v: i64 = std::str::from_utf8(s).ok()?.parse().ok()?;
        match n {
            10 => v * 1000,
            13 => v,
            16 => v / 1000,
            19 => v / 1_000_000,
            _ => return None,
        }
    } else if n == 10 && s[10] == b'.' && s.len() > 11 && s[11..].iter().all(u8::is_ascii_digit) {
        let v: i64 = std::str::from_utf8(&s[..10]).ok()?.parse().ok()?;
        let (frac, _) = fraction(s, 10);
        v * 1000 + frac as i64
    } else if s.iter().any(|&b| b == b'e' || b == b'E') {
        let v: f64 = std::str::from_utf8(s).ok()?.parse().ok()?;
        if (1e9..4.2e9).contains(&v) {
            (v * 1000.0) as i64
        } else if (1e12..4.2e12).contains(&v) {
            v as i64
        } else {
            return None;
        }
    } else {
        return None;
    };
    (MIN_MS..MAX_MS).contains(&ms).then_some(Stamp::Utc(ms))
}

/// Arrival times of a `run` capture: its marks are (line, ms since the
/// run's start), in order. A line without a time of its own arrived at the start plus the
/// last mark at or before it. Lines are asked in order.
pub struct Arrivals<'m> {
    start_ms: i64,
    marks: &'m [(u64, u64)],
    next: usize,
    cur: u64,
}

impl<'m> Arrivals<'m> {
    pub fn new(start_ms: i64, marks: &'m [(u64, u64)]) -> Arrivals<'m> {
        Arrivals { start_ms, marks, next: 0, cur: 0 }
    }

    /// The arrival time of line `line` (counted as the marks count lines), in Unix ms.
    pub fn at(&mut self, line: u64) -> i64 {
        while self.next < self.marks.len() && self.marks[self.next].0 <= line {
            self.cur = self.marks[self.next].1;
            self.next += 1;
        }
        self.start_ms + self.cur as i64
    }
}

/// A line's time as the catalog keeps it: the line's own time, else the
/// previous line's, else its arrival time. Lines are given in order.
#[derive(Clone, Copy, Debug, Default)]
pub struct Carry {
    last: Option<Stamp>,
}

impl Carry {
    pub fn new() -> Carry {
        Carry { last: None }
    }

    /// The time of the next line: `own` from its fields, `arrival` (Unix ms) when the run has
    /// arrival marks.
    pub fn next(&mut self, own: Option<Stamp>, arrival: Option<i64>) -> Option<Stamp> {
        if own.is_some() {
            self.last = own;
        }
        self.last.or(arrival.map(Stamp::Utc))
    }
}

/// Seconds east of UTC of a zone at the instant `utc_ms` (the catalog's zone-minutes is this
/// divided by 60).
pub fn offset_at(zone: Zone, utc_ms: i64) -> i64 {
    match zone {
        Zone::Fixed(m) => m as i64 * 60,
        Zone::Local => local_offset_at_utc(utc_ms.div_euclid(1000)),
    }
}

/// "YYYY-MM-DD HH:MM:SS.mmm" of Unix milliseconds in UTC (for messages and tests).
pub fn utc_text(ms: i64) -> String {
    let (y, mo, d, h, mi, sec) = civil(ms.div_euclid(1000));
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{sec:02}.{:03}", ms.rem_euclid(1000))
}

// ---------------------------------------------------------------------------------------
// The forms

/// A time form starting exactly at `s[i]`: (stamp, form, end, zoned).
fn form_at(s: &[u8], i: usize, c: &Clock) -> Option<(Stamp, Form, usize, bool)> {
    let b0 = *s.get(i)?;
    let with = |r: Option<(Stamp, usize, bool)>, f: Form| r.map(|(t, e, z)| (t, f, e, z));
    if b0.is_ascii_digit() {
        let n = run(s, i);
        return match (n, s.get(i + n)) {
            (4, Some(b'-')) => with(iso(s, i, c), Form::Iso).or_else(|| with(bgl(s, i, c), Form::Compact)),
            (4, Some(b'/')) => with(slashed(s, i, c, true), Form::Slashed),
            (2, Some(b'/')) if alpha(s, i + 3) => with(access(s, i, c), Form::Access),
            (2, Some(b'/')) => with(slashed(s, i, c, false), Form::Slashed),
            (2, Some(b'-')) if alpha(s, i + 3) => with(php(s, i, c), Form::Php),
            (2, Some(b'-')) => with(android(s, i, c), Form::NoYear),
            (2, Some(b'.')) => with(proxifier(s, i, c), Form::NoYear),
            (8, Some(b'-')) => with(healthapp(s, i, c), Form::Compact),
            (6, Some(b' ')) => with(yymmdd(s, i, c), Form::Compact),
            (10, _) | (13, _) => with(unix(s, i, n), Form::Unix),
            _ => None,
        };
    }
    if b0.is_ascii_alphabetic() {
        if month(s, i).is_some() {
            return with(syslog(s, i, c), Form::Syslog);
        }
        if weekday(s, i) {
            return with(ctime(s, i, c), Form::Ctime);
        }
        if matches!(b0, b'I' | b'W' | b'E' | b'F') && digit(s, i + 1) {
            return with(klog(s, i, c), Form::Klog);
        }
        if s[i..].starts_with(b"audit(") {
            return audit(s, i).map(|(t, e)| (t, Form::Audit, e, true));
        }
    }
    None
}

type FormRead = Option<(Stamp, usize, bool)>;

/// ISO 8601 / RFC 3339: yyyy-mm-dd, 'T' or one space, hh:mm[:ss][fraction][zone].
fn iso(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let y = fixed(s, i, 4)?;
    let mo = fixed(s, i + 5, 2)?;
    let d = fixed(s, i + 8, 2)?;
    if !at(s, i + 7, b'-') || !matches!(s.get(i + 10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (h, mi, sec, e) = hms(s, i + 11, false)?;
    let (ms, e) = fraction(s, e);
    let (z, e) = zone(s, e);
    if digit(s, e) {
        return None;
    }
    let st = c.stamp(Wall { y: y as i32, mo, d, h, mi, s: sec, ms }, z)?;
    Some((st, e, z.is_some()))
}

/// BGL: yyyy-mm-dd-hh.mm.ss.ffffff.
fn bgl(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let y = fixed(s, i, 4)?;
    let mo = fixed(s, i + 5, 2)?;
    let d = fixed(s, i + 8, 2)?;
    let h = fixed(s, i + 11, 2)?;
    let mi = fixed(s, i + 14, 2)?;
    let sec = fixed(s, i + 17, 2)?;
    if !(at(s, i + 7, b'-') && at(s, i + 10, b'-') && at(s, i + 13, b'.') && at(s, i + 16, b'.')) {
        return None;
    }
    let (ms, e) = fraction(s, i + 19);
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: y as i32, mo, d, h, mi, s: sec, ms }, None)?, e, false))
}

/// yyyy/mm/dd hh:mm:ss[fraction][zone] (nginx errors, Go's log; 'T' allowed too), or
/// yy/mm/dd hh:mm:ss[fraction] (Spark).
fn slashed(s: &[u8], i: usize, c: &Clock, four: bool) -> FormRead {
    let (y, j) = if four { (fixed(s, i, 4)? as i32, i + 4) } else { (2000 + fixed(s, i, 2)? as i32, i + 2) };
    let mo = fixed(s, j + 1, 2)?;
    let d = fixed(s, j + 4, 2)?;
    if !at(s, j, b'/') || !at(s, j + 3, b'/') {
        return None;
    }
    if !(at(s, j + 6, b' ') || (four && at(s, j + 6, b'T'))) {
        return None;
    }
    let (h, mi, sec, e) = hms(s, j + 7, true)?;
    let (ms, e) = fraction(s, e);
    let (z, e) = if four { zone(s, e) } else { (None, e) };
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y, mo, d, h, mi, s: sec, ms }, z)?, e, z.is_some()))
}

/// dd/Mmm/yyyy:hh:mm:ss[fraction][ +hhmm] (Apache and nginx access logs).
fn access(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let d = fixed(s, i, 2)?;
    let mo = month(s, i + 3)?;
    let y = fixed(s, i + 7, 4)?;
    if !(at(s, i + 2, b'/') && at(s, i + 6, b'/') && at(s, i + 11, b':')) {
        return None;
    }
    let (h, mi, sec, e) = hms(s, i + 12, true)?;
    let (ms, e) = fraction(s, e);
    let (z, e) = zone(s, e);
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: y as i32, mo, d, h, mi, s: sec, ms }, z)?, e, z.is_some()))
}

/// dd-Mmm-yyyy hh:mm:ss[ UTC] (PHP's error log; another zone name is read as local time).
fn php(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let d = fixed(s, i, 2)?;
    let mo = month(s, i + 3)?;
    let y = fixed(s, i + 7, 4)?;
    if !(at(s, i + 2, b'-') && at(s, i + 6, b'-') && at(s, i + 11, b' ')) {
        return None;
    }
    let (h, mi, sec, e) = hms(s, i + 12, true)?;
    let (ms, e) = fraction(s, e);
    let (z, e) = zone(s, e);
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: y as i32, mo, d, h, mi, s: sec, ms }, z)?, e, z.is_some()))
}

/// mm-dd hh:mm:ss[.mmm] (Android logcat): the year from the file's time.
fn android(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let mo = fixed(s, i, 2)?;
    let d = fixed(s, i + 3, 2)?;
    if !at(s, i + 2, b'-') || !at(s, i + 5, b' ') {
        return None;
    }
    let (h, mi, sec, e) = hms(s, i + 6, true)?;
    let (ms, e) = fraction(s, e);
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: c.year_for(mo), mo, d, h, mi, s: sec, ms }, None)?, e, false))
}

/// mm.dd hh:mm:ss (Proxifier, inside brackets): the year from the file's time.
fn proxifier(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let mo = fixed(s, i, 2)?;
    let d = fixed(s, i + 3, 2)?;
    if !at(s, i + 2, b'.') || !at(s, i + 5, b' ') {
        return None;
    }
    let (h, mi, sec, e) = hms(s, i + 6, true)?;
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: c.year_for(mo), mo, d, h, mi, s: sec, ms: 0 }, None)?, e, false))
}

/// HealthApp: yyyymmdd-H:M:S:ms, hours, minutes and seconds with 1 or 2 digits, and the
/// milliseconds as a count of 1 to 3 digits (":8" is 8 ms, not 800).
fn healthapp(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let y = fixed(s, i, 4)?;
    let mo = fixed(s, i + 4, 2)?;
    let d = fixed(s, i + 6, 2)?;
    let (h, e) = var(s, i + 9, 2)?;
    if !at(s, e, b':') {
        return None;
    }
    let (mi, e) = var(s, e + 1, 2)?;
    if !at(s, e, b':') {
        return None;
    }
    let (sec, e) = var(s, e + 1, 2)?;
    if !at(s, e, b':') {
        return None;
    }
    let (ms, e) = var(s, e + 1, 3)?;
    Some((c.stamp(Wall { y: y as i32, mo, d, h, mi, s: sec, ms }, None)?, e, false))
}

/// yymmdd hhmmss (HDFS), or yymmdd h:mm:ss after one or two spaces (old MySQL slow logs).
fn yymmdd(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let y = 2000 + fixed(s, i, 2)? as i32;
    let mo = fixed(s, i + 2, 2)?;
    let d = fixed(s, i + 4, 2)?;
    let (h, mi, sec, e) = if let Some(t) = fixed(s, i + 7, 6).filter(|_| !digit(s, i + 13)) {
        (t / 10_000, t / 100 % 100, t % 100, i + 13)
    } else {
        let j = if at(s, i + 7, b' ') { i + 8 } else { i + 7 };
        let (h, e) = var(s, j, 2)?;
        if !at(s, e, b':') || !at(s, e + 3, b':') {
            return None;
        }
        let mi = fixed(s, e + 1, 2)?;
        let sec = fixed(s, e + 4, 2)?;
        if digit(s, e + 6) {
            return None;
        }
        (h, mi, sec, e + 6)
    };
    Some((c.stamp(Wall { y, mo, d, h, mi, s: sec, ms: 0 }, None)?, e, false))
}

/// 10 digits (Unix seconds, maybe with a fraction) or 13 (milliseconds), from 2001-09-09 to
/// 2099.
fn unix(s: &[u8], i: usize, n: usize) -> FormRead {
    let mut v: i64 = 0;
    for &b in &s[i..i + n] {
        v = v * 10 + (b - b'0') as i64;
    }
    let (ms, e) = if n == 10 {
        let (f, e) = if at(s, i + 10, b'.') { fraction(s, i + 10) } else { (0, i + 10) };
        (v * 1000 + f as i64, e)
    } else {
        (v, i + 13)
    };
    if digit(s, e) || !(MIN_MS..MAX_MS).contains(&ms) {
        return None;
    }
    Some((Stamp::Utc(ms), e, true))
}

/// Mmm dd hh:mm:ss[fraction], the day padded with a space or not ("Jul  1", "Nov 9"): the
/// year from the file's time.
fn syslog(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let mo = month(s, i)?;
    if !at(s, i + 3, b' ') {
        return None;
    }
    let j = if at(s, i + 4, b' ') { i + 5 } else { i + 4 };
    let (d, e) = var(s, j, 2)?;
    if !at(s, e, b' ') {
        return None;
    }
    let (h, mi, sec, e) = hms(s, e + 1, true)?;
    let (ms, e) = fraction(s, e);
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: c.year_for(mo), mo, d, h, mi, s: sec, ms }, None)?, e, false))
}

/// Www Mmm dd hh:mm:ss[fraction] yyyy[ +hhmm] (Apache errors, C's asctime, Python's ctime).
fn ctime(s: &[u8], i: usize, c: &Clock) -> FormRead {
    if !at(s, i + 3, b' ') {
        return None;
    }
    let mo = month(s, i + 4)?;
    if !at(s, i + 7, b' ') {
        return None;
    }
    let j = if at(s, i + 8, b' ') { i + 9 } else { i + 8 };
    let (d, e) = var(s, j, 2)?;
    if !at(s, e, b' ') {
        return None;
    }
    let (h, mi, sec, e) = hms(s, e + 1, true)?;
    let (ms, e) = fraction(s, e);
    if !at(s, e, b' ') {
        return None;
    }
    let y = fixed(s, e + 1, 4)?;
    if digit(s, e + 5) {
        return None;
    }
    let (z, e) = zone(s, e + 5);
    Some((c.stamp(Wall { y: y as i32, mo, d, h, mi, s: sec, ms }, z)?, e, z.is_some()))
}

/// Lmmdd hh:mm:ss[fraction]: klog and glog, a level letter (I, W, E, F) first; the year from
/// the file's time.
fn klog(s: &[u8], i: usize, c: &Clock) -> FormRead {
    let mo = fixed(s, i + 1, 2)?;
    let d = fixed(s, i + 3, 2)?;
    if !at(s, i + 5, b' ') {
        return None;
    }
    let (h, mi, sec, e) = hms(s, i + 6, true)?;
    let (ms, e) = fraction(s, e);
    if digit(s, e) {
        return None;
    }
    Some((c.stamp(Wall { y: c.year_for(mo), mo, d, h, mi, s: sec, ms }, None)?, e, false))
}

/// audit(1700000000.123:42) at `s[i]` (Linux audit records).
fn audit(s: &[u8], i: usize) -> Option<(Stamp, usize)> {
    let j = i + 6;
    if !s.get(i..)?.starts_with(b"audit(") || run(s, j) != 10 || !at(s, j + 10, b'.') {
        return None;
    }
    // the seconds and their fraction, then ':' and the record's serial number
    let (stamp, e, _) = unix(s, j, 10)?;
    let n = run(s, e + 1);
    if !at(s, e, b':') || n == 0 || !at(s, e + 1 + n, b')') {
        return None;
    }
    Some((stamp, e + 2 + n))
}

/// The first `audit(...)` time in a line's first 200 bytes.
fn find_audit(line: &[u8], _c: &Clock) -> Option<Found> {
    let w = &line[..line.len().min(MAX_SCAN)];
    let p = w.windows(6).position(|x| x == b"audit(")?;
    let (stamp, end) = audit(line, p)?;
    Some(Found { stamp, form: Form::Audit, start: p, end, zoned: true })
}

/// `[  123.456789]` (dmesg) at `s[i]`: microseconds since boot.
fn boot(s: &[u8], i: usize) -> Option<(Stamp, usize)> {
    if !at(s, i, b'[') {
        return None;
    }
    let mut j = i + 1;
    while at(s, j, b' ') {
        j += 1;
    }
    let n = run(s, j);
    if n == 0 || n > 10 || !at(s, j + n, b'.') {
        return None;
    }
    let f = run(s, j + n + 1);
    if f == 0 || f > 9 || !at(s, j + n + 1 + f, b']') {
        return None;
    }
    let mut secs: i64 = 0;
    for &b in &s[j..j + n] {
        secs = secs * 10 + (b - b'0') as i64;
    }
    let mut us: i64 = 0;
    for k in 0..6 {
        us = us * 10 + if k < f { (s[j + n + 1 + k] - b'0') as i64 } else { 0 };
    }
    Some((Stamp::Boot(secs * 1_000_000 + us), j + n + 2 + f))
}

// ---------------------------------------------------------------------------------------
// Parts of the forms

/// A date and time of day, as written.
#[derive(Clone, Copy, Debug)]
struct Wall {
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    ms: u32,
}

impl Wall {
    /// A real date (years 1900-2999) and time of day (a leap second, :60, allowed).
    fn valid(&self) -> bool {
        (1900..=2999).contains(&self.y)
            && (1..=12).contains(&self.mo)
            && self.d >= 1
            && self.d <= days_in_month(self.y, self.mo)
            && self.h <= 23
            && self.mi <= 59
            && self.s <= 60
            && self.ms <= 999
    }

    /// Milliseconds since 1970 of this time read as UTC.
    fn ms(&self) -> i64 {
        (days_from_civil(self.y, self.mo, self.d) * 86_400 + self.h as i64 * 3600 + self.mi as i64 * 60 + self.s as i64) * 1000
            + self.ms as i64
    }
}

/// hh:mm, then :ss (required when `need_secs`): (h, m, s, end).
fn hms(s: &[u8], i: usize, need_secs: bool) -> Option<(u32, u32, u32, usize)> {
    let h = fixed(s, i, 2)?;
    if !at(s, i + 2, b':') {
        return None;
    }
    let mi = fixed(s, i + 3, 2)?;
    if at(s, i + 5, b':') && digit(s, i + 6) {
        let sec = fixed(s, i + 6, 2)?;
        return Some((h, mi, sec, i + 8));
    }
    if need_secs || digit(s, i + 5) { None } else { Some((h, mi, 0, i + 5)) }
}

/// A fraction of a second at `s[i]`: '.' or ',' then 1 to 9 digits: (milliseconds, end).
/// None there: (0, i).
fn fraction(s: &[u8], i: usize) -> (u32, usize) {
    if !(at(s, i, b'.') || at(s, i, b',')) || !digit(s, i + 1) {
        return (0, i);
    }
    let mut e = i + 1;
    let mut ms = 0;
    let mut n = 0;
    while digit(s, e) && n < 9 {
        if n < 3 {
            ms = ms * 10 + (s[e] - b'0') as u32;
        }
        n += 1;
        e += 1;
    }
    for _ in n..3 {
        ms *= 10;
    }
    (ms, e)
}

/// A zone after a time at `s[i]`: 'Z'; '+hh:mm', '+hhmm' or '+hh' (or '-'); one space and
/// '+hhmm' or '+hh:mm'; one space and UTC or GMT: (seconds east of UTC, end). None: (None, i).
fn zone(s: &[u8], i: usize) -> (Option<i64>, usize) {
    match s.get(i) {
        Some(b'Z' | b'z') if !alnum(s, i + 1) => (Some(0), i + 1),
        Some(b'+' | b'-') => match offset(s, i, true) {
            Some((o, e)) => (Some(o), e),
            None => (None, i),
        },
        Some(b' ') => {
            if matches!(s.get(i + 1), Some(b'+' | b'-'))
                && let Some((o, e)) = offset(s, i + 1, false)
            {
                return (Some(o), e);
            }
            let t = &s[i + 1..];
            if (t.starts_with(b"UTC") || t.starts_with(b"GMT")) && !alpha(s, i + 4) {
                return (Some(0), i + 4);
            }
            (None, i)
        }
        _ => (None, i),
    }
}

/// '+hh:mm', '+hhmm', and '+hh' when `short` (or '-'): (seconds east of UTC, end).
fn offset(s: &[u8], i: usize, short: bool) -> Option<(i64, usize)> {
    let sign = if s[i] == b'-' { -1 } else { 1 };
    let h = fixed(s, i + 1, 2)?;
    let (m, e) = if at(s, i + 3, b':') {
        (fixed(s, i + 4, 2)?, i + 6)
    } else if let Some(m) = fixed(s, i + 3, 2) {
        (m, i + 5)
    } else if short {
        (0, i + 3)
    } else {
        return None;
    };
    if digit(s, e) || h > 14 || m > 59 {
        return None;
    }
    Some((sign * (h as i64 * 3600 + m as i64 * 60), e))
}

const MONTHS: [&[u8; 3]; 12] = [b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov", b"dec"];
const WDAYS: [&[u8; 3]; 7] = [b"mon", b"tue", b"wed", b"thu", b"fri", b"sat", b"sun"];

/// A month's short name (any case) at `s[i]`, not followed by a letter: 1-12.
fn month(s: &[u8], i: usize) -> Option<u32> {
    let t = s.get(i..i + 3)?;
    if alpha(s, i + 3) {
        return None;
    }
    let l = [t[0].to_ascii_lowercase(), t[1].to_ascii_lowercase(), t[2].to_ascii_lowercase()];
    MONTHS.iter().position(|m| **m == l).map(|p| p as u32 + 1)
}

/// A weekday's short name (any case) at `s[i]`, not followed by a letter.
fn weekday(s: &[u8], i: usize) -> bool {
    let Some(t) = s.get(i..i + 3) else { return false };
    let l = [t[0].to_ascii_lowercase(), t[1].to_ascii_lowercase(), t[2].to_ascii_lowercase()];
    !alpha(s, i + 3) && WDAYS.iter().any(|w| **w == l)
}

#[inline]
fn at(s: &[u8], i: usize, c: u8) -> bool {
    s.get(i) == Some(&c)
}

#[inline]
fn digit(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_digit)
}

#[inline]
fn alpha(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_alphabetic)
}

#[inline]
fn alnum(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_alphanumeric)
}

/// The number of digits starting at `s[i]`.
fn run(s: &[u8], i: usize) -> usize {
    s.get(i..).map_or(0, |t| t.iter().take_while(|b| b.is_ascii_digit()).count())
}

/// Exactly `n` digits at `s[i]`: their value.
fn fixed(s: &[u8], i: usize, n: usize) -> Option<u32> {
    let t = s.get(i..i + n)?;
    let mut v = 0u32;
    for &b in t {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u32;
    }
    Some(v)
}

/// 1 to `max` digits at `s[i]`, and no more: (value, end).
fn var(s: &[u8], i: usize, max: usize) -> Option<(u32, usize)> {
    let n = run(s, i);
    if n == 0 || n > max {
        return None;
    }
    Some((fixed(s, i, n)?, i + n))
}

fn trim(s: &[u8]) -> &[u8] {
    let a = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let e = s.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(a, |p| p + 1);
    &s[a..e]
}

// ---------------------------------------------------------------------------------------
// Calendar and this computer's zone

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Seconds east of UTC of this computer's zone at the instant `t` (Unix seconds), through
/// sys.rs (the C library's localtime_r).
fn local_offset_at_utc(t: i64) -> i64 {
    crate::sys::local(t).6
}

/// Seconds east of UTC of this computer's zone at a wall-clock time (`wall`: seconds since
/// 1970 counted as if the wall time were UTC), through sys.rs (the C library's mktime, which
/// decides daylight saving). A time the C library cannot place takes the offset of the same
/// instant read as UTC.
fn local_offset_at_wall(wall: i64) -> i64 {
    let (y, mo, d, h, mi, s) = civil(wall);
    match crate::sys::from_local(y, mo, d, h, mi, s) {
        Some(t) => wall - t,
        None => local_offset_at_utc(wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trips() {
        for days in (-800_000i64..800_000).step_by(997) {
            let (y, m, d, ..) = civil(days * 86_400);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1_763_460_000), (2025, 11, 18, 10, 0, 0));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn when_forms() {
        let now = 1_763_460_000_000; // 2025-11-18 10:00:00 UTC
        assert_eq!(parse_when("15m", now), Ok(now - 15 * 60_000));
        assert_eq!(parse_when("2h", now), Ok(now - 2 * 3_600_000));
        assert_eq!(parse_when("3d", now), Ok(now - 3 * 86_400_000));
        assert_eq!(parse_when("now", now), Ok(now));
        assert_eq!(parse_when("2025-11-18T10:00:00Z", now), Ok(now));
        assert_eq!(parse_when("2025-11-18T12:00:00+02:00", now), Ok(now));
        assert_eq!(parse_when("2025-11-18T05:00:00.250-0500", now), Ok(now + 250));
        assert_eq!(parse_when("2025-11-18 10:00:00,5Z", now), Ok(now + 500));
        // local forms agree with the C library's local time
        let (y, mo, d, h, mi, s, off) = crate::sys::local(now / 1000);
        assert_eq!(parse_when(&format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"), now), Ok(now));
        assert_eq!(parse_when(&format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}"), now), Ok(now - s as i64 * 1000));
        let today = parse_when("today", now).unwrap();
        assert_eq!(fmt_local(today), format!("{y:04}-{mo:02}-{d:02} 00:00:00"));
        let yest = parse_when("yesterday", now).unwrap();
        assert!(today - yest >= 23 * 3_600_000 && today - yest <= 25 * 3_600_000);
        assert_eq!(fmt_local(now), format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"));
        let _ = off;
        for b in ["", "x", "15", "15x", "2025-13-01", "2025-11-18X10:00", "2025-11-18 1000", "2025-11-18T10:00:00+2"] {
            assert!(parse_when(b, now).is_err(), "{b}");
        }
        // text that is not ASCII is refused, never cut inside a character (these panicked)
        for b in ["2026-11-1é", "2026-11-18 10:1é", "2026-11-18T10:00+1é1", "2026-11-18T10:00:00Zé", "1é", "é"] {
            assert!(parse_when(b, now).is_err(), "{b}");
        }
        assert_eq!(fmt_duration(800), "0.8 s");
        assert_eq!(fmt_duration(185_000), "3 min 5 s");
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;

    /// 2026-11-18 12:00 UTC: the "file's time" of most tests.
    const FILE_MS: i64 = 1_795_003_200_000;

    fn utc0() -> Clock {
        Clock::new(Zone::Fixed(0), FILE_MS)
    }

    /// The UTC ms of a time form at the start of `s`.
    fn t(s: &str, c: &Clock) -> Option<i64> {
        read_at(s.as_bytes(), 0, c).and_then(|f| f.stamp.utc_ms())
    }

    fn form(s: &str) -> Option<Form> {
        read_at(s.as_bytes(), 0, &utc0()).map(|f| f.form)
    }

    #[test]
    fn calendar_matches_python() {
        // values from Python's calendar.timegm
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 11, 18), 20_775);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2400, 2, 29), 157_113);
        for d in [-1000, -1, 0, 1, 11_016, 11_017, 20_775, 157_113, 200_000] {
            let (y, m, dd, ..) = civil(d * 86_400);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(utc_text(1_794_996_902_123), "2026-11-18 10:15:02.123");
    }

    #[test]
    fn iso_and_rfc3339() {
        let c = utc0();
        assert_eq!(t("2026-11-18T10:15:02.123Z", &c), Some(1_794_996_902_123));
        assert_eq!(t("2015-07-29 17:41:44,747 - INFO  [QuorumPeer", &c), Some(1_438_191_704_747));
        assert_eq!(t("2026-11-18 10:15:02.123 +0100 x", &c), Some(1_794_993_302_123));
        assert_eq!(t("2026-11-18T10:15:02-05:00", &c), Some(1_795_014_902_000));
        assert_eq!(t("2024-01-15T10:23:45.1234567Z ##[group]Run", &c), Some(1_705_314_225_123));
        assert_eq!(t("2016-09-28 04:30:30, Info  CBS", &c), Some(1_475_037_030_000));
        assert_eq!(t("2026-11-18 10:15:02.123 UTC [1234] LOG:", &c), Some(1_794_996_902_123));
        assert_eq!(t("2026-11-18 10:15:02.00 +0100", &c), Some(1_794_993_302_000));
        assert_eq!(t("2026-11-18 10:15:02.123456-0800", &c), Some(1_795_025_702_123));
        assert_eq!(t("2026-11-18T10:15:02.123456789+00:00 stdout F x", &c), Some(1_794_996_902_123));
        assert_eq!(t("2024-02-29 00:00:00", &c), Some(1_709_164_800_000));
        // zoned or not
        assert!(read_at(b"2026-11-18T10:15:02Z", 0, &c).unwrap().zoned);
        assert!(!read_at(b"2026-11-18 10:15:02", 0, &c).unwrap().zoned);
        // a time without a zone is read in the clock's zone: UTC+2 is two hours earlier in UTC
        let plus2 = Clock::new(Zone::Fixed(120), FILE_MS);
        assert_eq!(t("2015-07-29 17:41:44,747", &plus2), Some(1_438_191_704_747 - 7_200_000));
        // ... but a written zone wins
        assert_eq!(t("2026-11-18T10:15:02.123Z", &plus2), Some(1_794_996_902_123));
        // out of range: not a time
        for bad in ["2026-13-01 10:00:00", "2026-02-30 10:00:00", "2023-02-29 10:00:00", "2026-11-18 24:00:00", "2026-11-18 10:61:00", "2026-11-18_10:15:02", "2026-11-18 10:15:021"] {
            assert_eq!(t(bad, &c), None, "{bad}");
        }
    }

    #[test]
    fn slashed_dates() {
        let c = utc0();
        assert_eq!(t("2026/11/18 10:15:02 [error] 1234#0: *5 open()", &c), Some(1_794_996_902_000));
        assert_eq!(t("17/06/09 20:10:40 INFO executor.CoarseGrainedExecutorBackend", &c), Some(1_497_039_040_000));
        assert_eq!(form("17/06/09 20:10:40"), Some(Form::Slashed));
        // month/day/year is never guessed: 11/18/26 has month 18
        assert_eq!(t("11/18/26 10:00:00", &c), None);
        // a date alone is not a line time
        assert_eq!(t("2026/11/18 rest", &c), None);
    }

    #[test]
    fn syslog_and_the_year_rule() {
        let c = utc0(); // the file is from 2026-11-18
        assert_eq!(t("Nov 18 10:15:02 host sshd[1]: x", &c), Some(1_794_996_902_000));
        assert_eq!(t("Jul  1 09:00:55 calvisitor-10-105-160-95 kernel[0]:", &c), Some(1_782_896_455_000));
        assert_eq!(t("Nov 9 12:01:01 dn228/dn228 crond", &c), Some(1_794_225_661_000));
        // a file from March 2026: November is later than March, so it is the year before
        let march = Clock::new(Zone::Fixed(0), 1_772_323_200_000); // 2026-03-01
        assert_eq!(t("Nov 18 10:15:02 host x", &march), Some(1_763_460_902_000));
        assert_eq!(t("<13>Nov 18 10:15:02 host prog: x", &c), Some(1_794_996_902_000));
        assert_eq!(t("Nov 31 10:15:02 host", &c), None);
    }

    #[test]
    fn apache_nginx_php_and_ctime() {
        let c = utc0();
        assert_eq!(t("[Sun Dec 04 04:47:44 2005] [notice] workerEnv.init() ok", &c), Some(1_133_671_664_000));
        assert_eq!(t("Thu Jun 09 06:07:04 2005", &c), Some(1_118_297_224_000));
        assert_eq!(t("[Wed Oct 11 14:32:52.123456 2000] [core:error]", &c), Some(971_274_772_123));
        assert_eq!(t("[18/Nov/2026:10:00:00 +0000] \"GET / HTTP/1.1\"", &c), Some(1_794_996_000_000));
        assert_eq!(t("[10/Oct/2000:13:55:36 -0700]", &c), Some(971_211_336_000));
        assert_eq!(t("[18-Nov-2026 10:15:02 UTC] PHP Fatal error:", &c), Some(1_794_996_902_000));
        let f = read_at(b"[18/Nov/2026:10:00:00 +0000] \"GET", 0, &c).unwrap();
        assert_eq!((f.form, f.end), (Form::Access, 28));
    }

    #[test]
    fn compact_forms() {
        let c = utc0();
        assert_eq!(t("081109 203615 148 INFO dfs.DataNode$PacketResponder:", &c), Some(1_226_262_975_000));
        assert_eq!(t("20171223-22:15:29:606|Step_LSC|30002312|onStandStepChanged 3579", &c), Some(1_514_067_329_606));
        assert_eq!(t("20171223-22:25:0:162|Step_LSC|", &c), Some(1_514_067_900_162));
        assert_eq!(t("20171223-22:25:13:8|Step_StandReportReceiver|", &c), Some(1_514_067_913_008));
        assert_eq!(t("2005-06-03-15.42.50.675872 R02-M1-N0-C:J12-U11 RAS", &c), Some(1_117_813_370_675));
        assert_eq!(t("181118 10:15:02", &c), Some(1_542_536_102_000));
        assert_eq!(t("181118  9:15:02", &c), Some(1_542_532_502_000));
        assert_eq!(form("2005-06-03-15.42.50.675872"), Some(Form::Compact));
        // six digits and six digits that are not a date
        assert_eq!(t("123456 999999 x", &c), None);
    }

    #[test]
    fn no_year_forms() {
        let c = utc0();
        assert_eq!(t("03-17 16:13:38.811  1702  2395 D WindowManager:", &c), Some(1_773_764_018_811));
        assert_eq!(t("[10.30 16:49:06] chrome.exe - proxy", &c), Some(1_793_378_946_000));
        let june = Clock::new(Zone::Fixed(0), 1_780_272_000_000); // 2026-06-01
        assert_eq!(t("[10.30 16:49:06] chrome.exe", &june), Some(1_761_842_946_000));
        let f = read_at(b"[10.30 16:49:06] chrome.exe", 0, &c).unwrap();
        assert_eq!((f.form, f.end), (Form::NoYear, 16));
        assert_eq!(t("I1118 10:15:02.123456       1 controller.go:123] msg", &c), Some(1_794_996_902_123));
        assert_eq!(form("E1118 10:15:02.123456 1 x.go:1] boom"), Some(Form::Klog));
    }

    #[test]
    fn unix_audit_and_boot() {
        let c = utc0();
        assert_eq!(t("1700000000 rest", &c), Some(1_700_000_000_000));
        assert_eq!(t("1700000000.123 rest", &c), Some(1_700_000_000_123));
        assert_eq!(t("1700000000123 rest", &c), Some(1_700_000_000_123));
        assert_eq!(t("0123456789 rest", &c), None); // before 2001-09-09
        assert_eq!(t("12345678901 rest", &c), None); // 11 digits
        let a = line_time(b"type=SYSCALL msg=audit(1700000000.123:42): arch=c000003e syscall=59", Place::START, &c).unwrap();
        assert_eq!((a.stamp, a.form), (Stamp::Utc(1_700_000_000_123), Form::Audit));
        let b = read_at(b"[  123.456789] usb 1-1: new device", 0, &c).unwrap();
        assert_eq!((b.stamp, b.form, b.end), (Stamp::Boot(123_456_789), Form::Boot, 14));
        assert_eq!(b.stamp.utc_ms(), None);
    }

    #[test]
    fn numbers_in_time_fields() {
        let c = utc0();
        assert_eq!(number_time(b"1700000000"), Some(Stamp::Utc(1_700_000_000_000)));
        assert_eq!(number_time(b"1700000000123"), Some(Stamp::Utc(1_700_000_000_123)));
        assert_eq!(number_time(b"1700000000123456"), Some(Stamp::Utc(1_700_000_000_123)));
        assert_eq!(number_time(b"1700000000123456789"), Some(Stamp::Utc(1_700_000_000_123)));
        assert_eq!(number_time(b"1700000000.1234567"), Some(Stamp::Utc(1_700_000_000_123)));
        assert_eq!(number_time(b"1.7e9"), Some(Stamp::Utc(1_700_000_000_000)));
        assert_eq!(number_time(b"42"), None);
        assert_eq!(number_time(b"4102444800"), None); // 2100-01-01
        assert_eq!(text_time(b"12-17 19:31:36.263", &c), read_at(b"12-17 19:31:36.263", 0, &c).map(|f| f.stamp));
        assert_eq!(text_time(b" 2026-11-18T10:15:02.123Z ", &c), Some(Stamp::Utc(1_794_996_902_123)));
        assert_eq!(text_time(b"1700000000123456", &c), Some(Stamp::Utc(1_700_000_000_123)));
    }

    #[test]
    fn places_learned_from_samples() {
        let c = utc0();
        let hpc: Vec<&[u8]> = vec![
            b"134681 node-246 unix.hw state_change.unavailable 1077804742 1 Component State Change",
            b"350766 node-109 unix.hw state_change.unavailable 1084680778 1 Component State Change",
        ];
        assert_eq!(learn_place(&hpc, &c), Place(4));
        let bgl: Vec<&[u8]> = vec![
            b"- 1117838570 2005.06.03 R02-M1-N0-C:J12-U11 2005-06-03-15.42.50.675872 R02-M1-N0-C:J12-U11 RAS KERNEL INFO x",
            b"KERNDTLB 1117838573 2005.06.03 R02-M1-N0-C:J12-U11 2005-06-03-15.42.53.276129 R02-M1-N0-C:J12-U11 RAS x",
        ];
        assert_eq!(learn_place(&bgl, &c), Place(1));
        assert_eq!(line_time(bgl[0], Place(1), &c).unwrap().stamp, Stamp::Utc(1_117_838_570_000));
        let open_stack: &[u8] = b"nova-api.log.1.2017-05-16_13:53:08 2017-05-16 00:00:00.008 25746 INFO nova.osapi_compute.wsgi.server";
        assert_eq!(learn_place(&[open_stack], &c), Place(1));
        // a Java log: most lines are frames without a time; the place stays at the start
        let java: Vec<&[u8]> = vec![
            b"2026-11-18 10:15:02 ERROR boom",
            b"\tat a.B.c(B.java:1)",
            b"\tat a.B.d(B.java:2)",
            b"\tat a.B.e(B.java:3)",
        ];
        assert_eq!(learn_place(&java, &c), Place::START);
        // a time inside the message of a line without one is not taken at the start
        assert_eq!(line_time(b"job scheduled for 2026-11-20 10:00:00", Place::START, &c), None);
        assert_eq!(line_time(b"", Place(3), &c), None);
        assert_eq!(token_start(b"  a  bb c", 2), Some(8));
    }

    #[test]
    fn arrivals_from_marks() {
        let marks = [(0u64, 0u64), (10, 150), (25, 900)];
        let mut a = Arrivals::new(1_000_000, &marks);
        assert_eq!(a.at(0), 1_000_000);
        assert_eq!(a.at(9), 1_000_000);
        assert_eq!(a.at(10), 1_000_150);
        assert_eq!(a.at(30), 1_000_900);
    }

    #[test]
    fn carried_times() {
        let mut c = Carry::new();
        assert_eq!(c.next(None, Some(5)), Some(Stamp::Utc(5))); // nothing before: the arrival time
        assert_eq!(c.next(None, None), None);
        assert_eq!(c.next(Some(Stamp::Utc(100)), Some(7)), Some(Stamp::Utc(100)));
        assert_eq!(c.next(None, Some(9)), Some(Stamp::Utc(100))); // the previous line's
        assert_eq!(c.next(Some(Stamp::Boot(3)), None), Some(Stamp::Boot(3)));
    }

    #[test]
    fn local_zone_round_trip() {
        // A UTC instant shown as this computer's wall time and read back gives the same
        // instant, whatever the zone (times inside a daylight-saving change are not tried).
        let c = Clock::local(FILE_MS);
        for t in [1_768_478_400_i64, 1_784_116_800, 1_794_996_902, 1_100_000_000] {
            let off = local_offset_at_utc(t);
            assert!(off.abs() <= 14 * 3600 && off % 900 == 0, "offset {off}");
            let wall_ms = (t + off) * 1000 + 123;
            assert_eq!(c.wall_to_utc(wall_ms), t * 1000 + 123);
        }
        assert_eq!(Clock::new(Zone::Fixed(-420), 0).wall_to_utc(0), 7 * 3_600_000);
        assert_eq!(offset_at(Zone::Fixed(-420), 0), -7 * 3600);
        let t = 1_794_996_902_123;
        assert_eq!(offset_at(Zone::Local, t), crate::sys::local(t / 1000).6);
    }
}
