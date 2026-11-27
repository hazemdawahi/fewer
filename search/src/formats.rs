//! Log types: a source's kind, named from its file name and its first 64 KiB,
//! and each line's message, level and time fields.
//!
//! The kind only guides the catalog (keys, levels, times, records). The archive is the same
//! whatever the kind, and every line's bytes are kept exactly.
//!
//! How a kind is named ([`detect`]):
//! 1. crash reports (.ips, .crash, Android tombstones) and MySQL slow logs, by their name or
//!    their marks in the sample;
//! 2. each sampled line is given a kind (a JSON object, CRI, GitHub Actions, syslog, access
//!    log, logfmt, or none); the kind of 80% or more of the non-empty lines wins, the JSON kinds
//!    counting together;
//! 3. else csv or tsv: the same number of separators on every row, a header first;
//! 4. else terminal output, when the sample holds escape codes or '\r' redraws;
//! 5. else plain.
//!
//! The sample is cleaned first ([`clean_line`]: escape codes out, a redraw at its final state),
//! as every line is before its fields are read. Lines are given to [`Profile::fields`] clean,
//! one at a time, without their line end.

use std::borrow::Cow;

use crate::clean::{strip_escapes, without_end};
use crate::json::{self, Json};
use crate::msgkey::{MESSAGE_FIELDS, claude_parts, codex_parts, level_field};
use crate::times::{self, Clock, Form, Place, Stamp, Zone};

/// The clean text of a line, what the fields and the key are read
/// from: the line end dropped; escape sequences set aside (clean.rs's `strip_escapes`); a
/// line redrawn with '\r' kept at its final state (the last piece after a '\r' that is not
/// empty, as clean.rs's `shown` does); the other control bytes (below 0x20 but tab, and 0x7F)
/// dropped. A line without control bytes is returned as it is.
pub fn clean_line(line: &[u8]) -> Cow<'_, [u8]> {
    let control = |c: u8| (c < 0x20 && c != b'\t') || c == 0x7f;
    let l = without_end(line);
    if !l.iter().any(|&c| control(c)) {
        return Cow::Borrowed(l);
    }
    let (b, _) = strip_escapes(l);
    let piece: &[u8] = if b.contains(&b'\r') { b.split(|&c| c == b'\r').rev().find(|p| !p.is_empty()).unwrap_or(&[]) } else { &b };
    Cow::Owned(piece.iter().copied().filter(|&c| !control(c)).collect())
}

/// The bytes looked at to name a source's kind.
pub const SAMPLE_BYTES: usize = 64 * 1024;

/// A JSON line is read as JSON only up to this size (the key rule's limit).
pub const JSON_MAX: usize = crate::msgkey::JSON_MAX;

/// The kind of a log source. `name()` is what runs.jsonl's `format` field holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    Plain,
    Json,
    Docker,
    ClaudeSession,
    CodexSession,
    MacosLog,
    Journal,
    Cri,
    Syslog,
    Access,
    Logfmt,
    Gha,
    Terminal,
    Crash,
    Csv,
    Tsv,
    MysqlSlow,
}

impl Format {
    pub const ALL: [Format; 17] = [
        Format::Plain,
        Format::Json,
        Format::Docker,
        Format::ClaudeSession,
        Format::CodexSession,
        Format::MacosLog,
        Format::Journal,
        Format::Cri,
        Format::Syslog,
        Format::Access,
        Format::Logfmt,
        Format::Gha,
        Format::Terminal,
        Format::Crash,
        Format::Csv,
        Format::Tsv,
        Format::MysqlSlow,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Format::Plain => "plain",
            Format::Json => "json",
            Format::Docker => "docker",
            Format::ClaudeSession => "claude-session",
            Format::CodexSession => "codex-session",
            Format::MacosLog => "macos-log",
            Format::Journal => "journal",
            Format::Cri => "cri",
            Format::Syslog => "syslog",
            Format::Access => "access",
            Format::Logfmt => "logfmt",
            Format::Gha => "gha",
            Format::Terminal => "terminal",
            Format::Crash => "crash",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::MysqlSlow => "mysql-slow",
        }
    }

    pub fn from_name(s: &str) -> Option<Format> {
        Format::ALL.iter().copied().find(|f| f.name() == s)
    }

    /// One JSON object per line.
    pub fn is_json(self) -> bool {
        matches!(self, Format::Json | Format::Docker | Format::ClaudeSession | Format::CodexSession | Format::MacosLog | Format::Journal)
    }
}

/// The JSON kinds, the more specific first (they win a tie).
const JSON_KINDS: [Format; 6] = [Format::Docker, Format::ClaudeSession, Format::CodexSession, Format::MacosLog, Format::Journal, Format::Json];

/// A line's level, from least to most severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    None,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::None => "",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// The level of a level word, in any case: fatal, panic, critical, crit, emerg, alert, error,
/// err and severe give error; warn and warning give warn; info and notice give info; debug,
/// trace and verbose give debug.
pub fn level_word(w: &[u8]) -> Option<Level> {
    if w.is_empty() || w.len() > 8 {
        return None;
    }
    let mut b = [0u8; 8];
    for (d, s) in b.iter_mut().zip(w) {
        *d = s.to_ascii_lowercase();
    }
    match &b[..w.len()] {
        b"fatal" | b"panic" | b"critical" | b"crit" | b"emerg" | b"alert" | b"error" | b"err" | b"severe" => Some(Level::Error),
        b"warn" | b"warning" => Some(Level::Warn),
        b"info" | b"notice" => Some(Level::Info),
        b"debug" | b"trace" | b"verbose" => Some(Level::Debug),
        _ => None,
    }
}

/// Android's one-letter levels (logcat): V and D debug, I info, W warn, E error, and F
/// (fatal) and A (assert) error.
pub fn level_letter(c: u8) -> Option<Level> {
    match c {
        b'V' | b'D' => Some(Level::Debug),
        b'I' => Some(Level::Info),
        b'W' => Some(Level::Warn),
        b'E' | b'F' | b'A' => Some(Level::Error),
        _ => None,
    }
}

/// The level of a text line: Android's one-letter level right after the pid and tid columns;
/// else the first level word among the first 12 words of the first 160 bytes. A word is a run
/// of letters with no letter, digit, '_' or '/' next to it, and no '=' after it (`err=nil` is
/// a field, not a level). Letters inside names such as `R02-M1-N0` are not words and are not
/// counted.
pub fn text_level(s: &[u8]) -> Level {
    if let Some(l) = android_level(s) {
        return l;
    }
    let w = &s[..s.len().min(160)];
    let free = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'/');
    let mut i = 0;
    let mut words = 0;
    while i < w.len() && words < 12 {
        if !w[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let a = i;
        while i < w.len() && w[i].is_ascii_alphabetic() {
            i += 1;
        }
        let before = if a == 0 { b' ' } else { w[a - 1] };
        let after = s.get(i).copied().unwrap_or(b' ');
        if free(before) && free(after) && after != b'=' {
            words += 1;
            if let Some(l) = level_word(&w[a..i]) {
                return l;
            }
        }
    }
    Level::None
}

/// Android logcat's level: V, D, I, W, E, F or A as a token of its own right after two number
/// tokens (the pid and tid columns), in the first 8 tokens.
fn android_level(s: &[u8]) -> Option<Level> {
    let mut nums = [false, false];
    let mut i = 0;
    for _ in 0..8 {
        while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
            i += 1;
        }
        if i >= s.len() {
            break;
        }
        let a = i;
        while i < s.len() && s[i] != b' ' && s[i] != b'\t' {
            i += 1;
        }
        let tok = &s[a..i];
        if tok.len() == 1
            && nums[0]
            && nums[1]
            && let Some(l) = level_letter(tok[0])
        {
            return Some(l);
        }
        nums = [nums[1], tok.iter().all(u8::is_ascii_digit)];
    }
    None
}

/// What the key rule (msgkey.rs) is applied to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    /// Text: a JSON line's message field (Docker's `log` without its final '\n' and cleaned),
    /// the text after a CRI or GitHub Actions line's prefix, logfmt's msg value, an .ips
    /// header's app name, or the whole clean line.
    Text(Cow<'a, [u8]>),
    /// A JSON object without a message field: its top-level field names in order (rule v1
    /// keys it as `{a,b,c}`).
    Names(Vec<String>),
    /// An agent's session line: the parts of its session key, each only when present (rule
    /// v1 keys it as `{session part part ...}`). Claude Code: type, message.role, the first
    /// content block's type, the first tool name. Codex: type, payload.type, payload.role, the
    /// first content block's type, payload.name.
    Session(Vec<String>),
}

/// The fields of one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fields<'a> {
    pub msg: Msg<'a>,
    pub level: Level,
    /// The level as written in a JSON, logfmt or csv level field (rule v1's `{level}` part).
    pub level_text: Option<String>,
    /// The line's own time; None: it takes the previous line's.
    pub time: Option<Stamp>,
    /// A CRI 'P' line, or a Docker piece without its final '\n': the next line goes on with
    /// it (the same record).
    pub partial: bool,
    /// Written to stderr (Docker, CRI): kept as a level hint; it does not set `level`.
    pub stderr: bool,
    /// A structured event (a JSON or logfmt line): it carries its own time and fields, so it
    /// is a record of its own and never continues the one before it.
    event: bool,
}

impl<'a> Fields<'a> {
    fn text(t: Cow<'a, [u8]>) -> Fields<'a> {
        Fields { msg: Msg::Text(t), level: Level::None, level_text: None, time: None, partial: false, stderr: false, event: false }
    }

    fn of(msg: Msg<'a>) -> Fields<'a> {
        Fields { msg, ..Fields::text(Cow::Borrowed(b"")) }
    }

    /// The text the record rules read (records.rs): the message text of a text line (the
    /// whole line for most kinds; the text after the prefix for CRI, GitHub Actions and Docker,
    /// whose lines are a program's raw output). None for structured events (JSON and logfmt
    /// lines), which are records of their own: a JSON message that starts with a space is not
    /// a continuation.
    pub fn body(&self) -> Option<&[u8]> {
        match &self.msg {
            Msg::Text(t) if !self.event => Some(t),
            _ => None,
        }
    }
}

/// What [`detect`] learns about a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub format: Format,
    /// Where the source's text lines hold their time.
    pub place: Place,
    /// csv and tsv: the separator, and the columns whose header names a time or a level.
    pub sep: u8,
    pub time_col: Option<usize>,
    pub level_col: Option<usize>,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// The fields that hold a JSON line's time, in order. The message and level
/// fields are msgkey.rs's, shared with the key rule.
const TIME_FIELDS: [&str; 7] = ["time", "ts", "timestamp", "@timestamp", "date", "eventTime", "__REALTIME_TIMESTAMP"];

/// logfmt's keys and csv's header names for the message, level and time (any case for csv).
const LOGFMT_MESSAGE: [&[u8]; 2] = [b"msg", b"message"];
const LOGFMT_LEVEL: [&[u8]; 4] = [b"level", b"lvl", b"severity", b"loglevel"];
const LOGFMT_TIME: [&[u8]; 6] = [b"time", b"ts", b"t", b"timestamp", b"@timestamp", b"date"];
const CSV_TIME: [&[u8]; 7] = [b"time", b"timestamp", b"@timestamp", b"ts", b"date", b"datetime", b"eventtime"];
const CSV_LEVEL: [&[u8]; 6] = [b"level", b"severity", b"lvl", b"levelname", b"loglevel", b"log_level"];

/// The first line of an Android tombstone.
const TOMBSTONE: &[u8] = b"*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***";

impl Profile {
    pub fn new(format: Format) -> Profile {
        Profile { format, place: Place::START, sep: b',', time_col: None, level_col: None }
    }

    /// The fields of one clean line (without its line end). `c` reads times without a zone
    /// or a year.
    pub fn fields<'a>(&self, line: &'a [u8], c: &Clock) -> Fields<'a> {
        let line = line.strip_prefix(BOM).unwrap_or(line);
        if self.format == Format::Crash {
            return crash_fields(line, c);
        }
        // Any JSON line, in any kind of source, is read as JSON.
        if let Some(o) = json_object(line) {
            return self.json_fields(&o, c);
        }
        match self.format {
            Format::Cri => {
                if let Some(x) = cri_line(line, c) {
                    return self.cri_fields(line, x, c);
                }
            }
            Format::Gha => {
                if let Some(f) = gha_time(line, c) {
                    return gha_fields(line, f);
                }
            }
            Format::Syslog => {
                if let Some(s) = syslog_line(line, c) {
                    // the message is the whole line; the level comes from the
                    // priority, else from the words after the tag
                    let level = s.sev.map(severity_level).unwrap_or_else(|| text_level(&line[s.body..]));
                    return Fields { level, time: Some(s.time.stamp), ..Fields::text(Cow::Borrowed(line)) };
                }
            }
            Format::Access => {
                if let Some(a) = access_line(line, c) {
                    let level = match a.status {
                        Some(500..=599) => Level::Error,
                        Some(400..=499) => Level::Warn,
                        _ => Level::Info,
                    };
                    return Fields { level, time: Some(a.time.stamp), ..Fields::text(Cow::Borrowed(line)) };
                }
            }
            Format::Logfmt => {
                let (pairs, _, _) = logfmt_pairs(line);
                if !pairs.is_empty() {
                    return self.logfmt_fields(line, pairs, c);
                }
            }
            Format::Csv | Format::Tsv => return self.csv_fields(line, c),
            Format::MysqlSlow => return mysql_fields(line, c),
            _ => {}
        }
        self.plain_fields(line, c)
    }

    /// A text line: the whole line is the message; the level from its words; the time at
    /// the source's place.
    fn plain_fields<'a>(&self, line: &'a [u8], c: &Clock) -> Fields<'a> {
        let found = times::line_time(line, self.place, c);
        let level = match found {
            Some(f) if f.form == Form::Klog => level_letter(line[f.start]).unwrap_or(Level::None),
            _ => text_level(line),
        };
        Fields { level, time: found.map(|f| f.stamp), ..Fields::text(Cow::Borrowed(line)) }
    }

    /// A JSON line: the message, level and time fields; session keys for
    /// agents' sessions; Docker's `log`.
    fn json_fields<'a>(&self, o: &Json, c: &Clock) -> Fields<'a> {
        let time = json_time(o, c);
        match self.format {
            Format::ClaudeSession => return Fields { time, ..Fields::of(Msg::Session(claude_parts(o))) },
            Format::CodexSession => return Fields { time, ..Fields::of(Msg::Session(codex_parts(o))) },
            _ => {}
        }
        let (level, level_text) = json_level(o);
        if self.format == Format::Docker
            && o.get("stream").is_some()
            && let Some(m) = o.get("log").and_then(Json::str)
        {
            // the message is `log` without its final '\n', cleaned as a terminal line is; a
            // piece without the '\n' goes on in the next line
            let partial = !m.ends_with('\n');
            let text = clean_line(m.as_bytes()).into_owned();
            let stderr = o.get("stream").and_then(Json::str) == Some("stderr");
            if let Some(inner) = json_object(&text) {
                // an app that logs JSON inside Docker: its own fields, Docker's time
                let f = Profile::new(Format::Json).json_fields(&inner, c);
                return Fields { time: time.or(f.time), partial, stderr, ..f };
            }
            let level = if level == Level::None { text_level(&text) } else { level };
            return Fields { level, level_text, time, partial, stderr, ..Fields::text(Cow::Owned(text)) };
        }
        let Some(m) = MESSAGE_FIELDS.iter().find_map(|k| o.get(k).and_then(Json::str)) else {
            return Fields { level, level_text, time, event: true, ..Fields::of(Msg::Names(names(o))) };
        };
        Fields { level, level_text, time, event: true, ..Fields::text(Cow::Owned(m.as_bytes().to_vec())) }
    }

    /// A CRI line: the text after the prefix; 'P' lines go on in the next line.
    fn cri_fields<'a>(&self, line: &'a [u8], x: CriLine, c: &Clock) -> Fields<'a> {
        let text = &line[x.text..];
        let time = Some(x.time.stamp);
        if let Some(inner) = json_object(text) {
            let f = Profile::new(Format::Json).json_fields(&inner, c);
            return Fields { time, partial: x.partial, stderr: x.stderr, ..f };
        }
        Fields { level: text_level(text), time, partial: x.partial, stderr: x.stderr, ..Fields::text(Cow::Borrowed(text)) }
    }

    /// A logfmt line: the msg, level and time values.
    fn logfmt_fields<'a>(&self, line: &'a [u8], pairs: Pairs<'a>, c: &Clock) -> Fields<'a> {
        let get = |keys: &[&[u8]]| keys.iter().find_map(|k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone()));
        let msg = get(&LOGFMT_MESSAGE).unwrap_or(Cow::Borrowed(line));
        let (level, level_text) = match get(&LOGFMT_LEVEL) {
            Some(v) => {
                let t = String::from_utf8_lossy(&v).into_owned();
                (level_value("level", &t), Some(t))
            }
            None => (text_level(line), None),
        };
        let time = get(&LOGFMT_TIME).and_then(|v| times::text_time(&v, c)).or_else(|| times::line_time(line, self.place, c).map(|f| f.stamp));
        Fields { level, level_text, time, event: true, ..Fields::text(msg) }
    }

    /// A csv or tsv row: the whole row; the level and time from the columns the header names.
    fn csv_fields<'a>(&self, line: &'a [u8], c: &Clock) -> Fields<'a> {
        let cols = split_row(line, self.sep);
        let col = |i: Option<usize>| i.and_then(|i| cols.get(i)).map(|f| unquote(f));
        let time = col(self.time_col).and_then(|v| times::text_time(&v, c)).or_else(|| times::line_time(line, self.place, c).map(|f| f.stamp));
        let (level, level_text) = match col(self.level_col).filter(|v| !v.is_empty()) {
            Some(v) => {
                let t = String::from_utf8_lossy(&v).into_owned();
                (level_value("level", &t), Some(t))
            }
            None => (Level::None, None),
        };
        Fields { level, level_text, time, ..Fields::text(Cow::Borrowed(line)) }
    }
}

/// Names a source's kind from its file name (a path or a base name; may be empty) and its
/// first bytes (up to 64 KiB are looked at).
pub fn detect(name: &str, head: &[u8]) -> Profile {
    let head = &head[..head.len().min(SAMPLE_BYTES)];
    let cut = head.len() == SAMPLE_BYTES;
    let body = head.strip_prefix(BOM).unwrap_or(head);
    let mut raw: Vec<&[u8]> = body.split_inclusive(|&b| b == b'\n').collect();
    if cut && raw.len() > 1 && !raw.last().is_some_and(|l| l.ends_with(b"\n")) {
        raw.pop(); // the last line was cut at 64 KiB
    }
    let cleaned: Vec<Cow<[u8]>> = raw.iter().map(|l| clean_line(l)).collect();
    let lines: Vec<&[u8]> = cleaned.iter().map(|l| &l[..]).collect();
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").to_ascii_lowercase();
    // times are only told apart from other text here; the zone and year do not matter
    let clock = Clock::new(Zone::Fixed(0), 1_735_603_200_000);

    if is_crash(&base, &lines) {
        return Profile::new(Format::Crash);
    }
    if is_mysql_slow(&lines) {
        return Profile { place: times::learn_place(&lines, &clock), ..Profile::new(Format::MysqlSlow) };
    }

    let mut votes = [0usize; Format::ALL.len()];
    let mut total = 0;
    for l in &lines {
        if !is_blank(l) {
            total += 1;
            votes[line_kind(l, &clock) as usize] += 1;
        }
    }
    let most = |n: usize| total > 0 && n * 5 >= total * 4;
    let json_lines: usize = JSON_KINDS.iter().map(|&f| votes[f as usize]).sum();
    let format = if most(json_lines) {
        let mut best = JSON_KINDS[0];
        for f in JSON_KINDS {
            if votes[f as usize] > votes[best as usize] {
                best = f;
            }
        }
        best
    } else if let Some(f) = [Format::Cri, Format::Gha, Format::Syslog, Format::Access, Format::Logfmt].into_iter().find(|&f| most(votes[f as usize])) {
        f
    } else if let Some(p) = csv_profile(&base, &lines, &clock) {
        return p;
    } else if has_terminal_codes(body) {
        Format::Terminal
    } else {
        Format::Plain
    };
    let place = if format.is_json() { Place::START } else { times::learn_place(&lines, &clock) };
    Profile { place, ..Profile::new(format) }
}

/// The kind of one sampled line, for the vote.
fn line_kind(l: &[u8], c: &Clock) -> Format {
    if l.first() == Some(&b'{') {
        return json_object(l).map_or(Format::Plain, |o| json_kind(&o));
    }
    if cri_line(l, c).is_some() {
        Format::Cri
    } else if gha_time(l, c).is_some() {
        Format::Gha
    } else if syslog_line(l, c).is_some() {
        Format::Syslog
    } else if access_line(l, c).is_some() {
        Format::Access
    } else if is_logfmt(l) {
        Format::Logfmt
    } else {
        Format::Plain
    }
}

/// The kind of a JSON line, from the fields it has.
fn json_kind(o: &Json) -> Format {
    let has = |k: &str| o.get(k).is_some();
    if has("log") && has("stream") && has("time") {
        Format::Docker
    } else if has("sessionId") && has("type") {
        Format::ClaudeSession
    } else if (has("payload") && has("type") && has("timestamp")) || has("record_type") {
        Format::CodexSession
    } else if has("eventMessage") && (has("messageType") || has("eventType")) {
        Format::MacosLog
    } else if has("MESSAGE") && (has("PRIORITY") || has("__REALTIME_TIMESTAMP")) {
        Format::Journal
    } else {
        Format::Json
    }
}

/// A line that is a JSON object of at most 1 MiB.
fn json_object(l: &[u8]) -> Option<Json> {
    if l.first() != Some(&b'{') || l.len() > JSON_MAX {
        return None;
    }
    json::parse(l).ok().filter(|v| matches!(v, Json::Obj(_)))
}

/// A JSON line's time: the first time field, in the list's order, that holds a time (a string
/// in any time form, or a number: seconds, ms, µs or ns by its size).
fn json_time(o: &Json, c: &Clock) -> Option<Stamp> {
    TIME_FIELDS.iter().filter_map(|k| o.get(k)).find_map(|v| match v {
        Json::Str(s) => times::text_time(s.as_bytes(), c),
        Json::Num(n) => times::number_time(n.as_bytes()),
        _ => None,
    })
}

/// A JSON line's level: the first level field that holds a string or a number
/// (`msgkey::level_field`; `log.level` also as {"log": {"level"}}), its class, and its value
/// as written.
fn json_level(o: &Json) -> (Level, Option<String>) {
    match level_field(o) {
        Some((k, text)) => (level_value(k, text), Some(text.to_string())),
        None => (Level::None, None),
    }
}

/// The class of a level field's value: a level word, an Android letter, macOS's message
/// types (Fault and Error, Default and Info, Debug), journald's PRIORITY (0-3 error, 4 warn,
/// 5-6 info, 7 debug), or pino and bunyan's numbers (10-20 debug, 30 info, 40 warn, 50-60
/// error).
fn level_value(field: &str, v: &str) -> Level {
    let t = v.trim().as_bytes();
    if !t.is_empty() && t.len() <= 3 && t.iter().all(u8::is_ascii_digit) {
        let n: u32 = v.trim().parse().unwrap_or(u32::MAX);
        return if field == "PRIORITY" {
            severity_level(n)
        } else {
            match n {
                10 | 20 => Level::Debug,
                30 => Level::Info,
                40 => Level::Warn,
                50 | 60 => Level::Error,
                _ => Level::None,
            }
        };
    }
    if t.len() == 1 {
        return level_letter(t[0]).unwrap_or(Level::None);
    }
    if field == "messageType" {
        match v.trim().to_ascii_lowercase().as_str() {
            "fault" | "error" => return Level::Error,
            "default" | "info" => return Level::Info,
            "debug" => return Level::Debug,
            _ => {}
        }
    }
    level_word(t).unwrap_or(Level::None)
}

/// A syslog severity (0-7, also journald's PRIORITY): 0-3 error, 4 warn, 5-6 info, 7 debug.
fn severity_level(n: u32) -> Level {
    match n {
        0..=3 => Level::Error,
        4 => Level::Warn,
        5 | 6 => Level::Info,
        7 => Level::Debug,
        _ => Level::None,
    }
}

/// The top-level field names of a JSON object, in order.
fn names(o: &Json) -> Vec<String> {
    match o {
        Json::Obj(m) => m.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    }
}

/// A crash report's line: every line is an error (the whole file is one record); an .ips
/// header's message is its app name and its time the header's `timestamp`; "Date/Time:" and
/// "Timestamp:" lines give the time.
fn crash_fields<'a>(line: &'a [u8], c: &Clock) -> Fields<'a> {
    let mut f = Fields { level: Level::Error, ..Fields::text(Cow::Borrowed(line)) };
    if let Some(o) = json_object(line) {
        f.time = json_time(&o, c);
        if let Some(app) = ["app_name", "name", "procName"].iter().find_map(|k| o.get(k).and_then(Json::str)) {
            f.msg = Msg::Text(Cow::Owned(app.as_bytes().to_vec()));
        }
        return f;
    }
    for label in [&b"Date/Time:"[..], b"Timestamp:"] {
        if let Some(rest) = line.strip_prefix(label) {
            f.time = times::text_time(rest, c);
        }
    }
    f
}

/// A MySQL slow-log line: "# Time:" lines and "SET timestamp=N;" give the time.
fn mysql_fields<'a>(line: &'a [u8], c: &Clock) -> Fields<'a> {
    let mut f = Fields::text(Cow::Borrowed(line));
    if let Some(rest) = line.strip_prefix(b"# Time:") {
        f.time = times::text_time(rest, c);
    } else if let Some(rest) = line.strip_prefix(b"SET timestamp=") {
        let n = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        f.time = times::number_time(&rest[..n]);
    }
    f
}

/// A GitHub Actions line: the text after the time; `##[error]`, `##[warning]`, `##[notice]`
/// and `##[debug]` give the level, else the text's words.
fn gha_fields(line: &[u8], f: times::Found) -> Fields<'_> {
    let text = &line[(f.end + 1).min(line.len())..];
    let level = if text.starts_with(b"##[error]") {
        Level::Error
    } else if text.starts_with(b"##[warning]") {
        Level::Warn
    } else if text.starts_with(b"##[notice]") {
        Level::Info
    } else if text.starts_with(b"##[debug]") {
        Level::Debug
    } else {
        text_level(text)
    };
    Fields { level, time: Some(f.stamp), ..Fields::text(Cow::Borrowed(text)) }
}

/// A GitHub Actions line's time: an ISO time with 7 decimals and 'Z' (28 bytes), then a space
/// or the end.
fn gha_time(l: &[u8], c: &Clock) -> Option<times::Found> {
    if l.len() < 28 || l[10] != b'T' || l[19] != b'.' || l[27] != b'Z' || !l[20..27].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if l.len() > 28 && l[28] != b' ' {
        return None;
    }
    times::read_at(l, 0, c).filter(|f| f.form == Form::Iso && f.end == 28)
}

/// A CRI line (Kubernetes): `<RFC 3339 time> stdout|stderr P|F <text>`.
#[derive(Clone, Copy)]
struct CriLine {
    time: times::Found,
    stderr: bool,
    partial: bool,
    /// where the text starts
    text: usize,
}

fn cri_line(l: &[u8], c: &Clock) -> Option<CriLine> {
    let sp = l.iter().position(|&b| b == b' ')?;
    if l.get(10) != Some(&b'T') {
        return None;
    }
    let f = times::read_at(&l[..sp], 0, c).filter(|f| f.form == Form::Iso && f.zoned && f.end == sp)?;
    let rest = &l[sp + 1..];
    let stderr = if rest.starts_with(b"stdout ") {
        false
    } else if rest.starts_with(b"stderr ") {
        true
    } else {
        return None;
    };
    let tag = *rest.get(7)?;
    if tag != b'P' && tag != b'F' {
        return None;
    }
    let text = match rest.get(8) {
        None => l.len(),
        Some(b' ') => sp + 1 + 9,
        _ => return None,
    };
    Some(CriLine { time: f, stderr, partial: tag == b'P', text })
}

/// A syslog line: its time, where its text starts (after the tag), and its severity when it
/// has a priority.
struct SyslogLine {
    time: times::Found,
    body: usize,
    sev: Option<u32>,
}

/// RFC 3164 (`[<PRI>]Mmm dd hh:mm:ss host tag: text`, or an RFC 3339 time first, as rsyslog
/// writes) or RFC 5424 (`<PRI>1 time host app procid msgid sd text`). The tag is a token
/// ending with ':' among the 3 after the host; the host is not a level word.
fn syslog_line(l: &[u8], c: &Clock) -> Option<SyslogLine> {
    let mut i = 0;
    let mut sev = None;
    if l.first() == Some(&b'<') {
        let close = l.iter().take(5).position(|&b| b == b'>')?;
        let p: u32 = std::str::from_utf8(&l[1..close]).ok()?.parse().ok()?;
        if p > 191 {
            return None;
        }
        sev = Some(p % 8);
        i = close + 1;
        if l.get(i) == Some(&b'1') && l.get(i + 1) == Some(&b' ') {
            return rfc5424(l, i + 2, sev, c);
        }
    }
    let f = times::read_at(l, i, c)?;
    if !(f.form == Form::Syslog || (f.form == Form::Iso && f.zoned && l.get(i + 10) == Some(&b'T'))) {
        return None;
    }
    let h = skip_spaces(l, f.end);
    if h == f.end || h >= l.len() {
        return None;
    }
    let he = token_end(l, h);
    let host = &l[h..he];
    if host[0] == b'[' || level_word(host.strip_suffix(b":").unwrap_or(host)).is_some() {
        return None;
    }
    let mut p = he;
    for _ in 0..3 {
        let s = skip_spaces(l, p);
        if s == p || s >= l.len() {
            return None;
        }
        let e = token_end(l, s);
        if l[e - 1] == b':' {
            let body = if l.get(e) == Some(&b' ') { e + 1 } else { e };
            return Some(SyslogLine { time: f, body, sev });
        }
        p = e;
    }
    None
}

/// RFC 5424 after `<PRI>1 `: time, host, app, procid, msgid, structured data ('-' or
/// `[...]` blocks), text.
fn rfc5424(l: &[u8], i: usize, sev: Option<u32>, c: &Clock) -> Option<SyslogLine> {
    let f = times::read_at(l, i, c).filter(|f| f.form == Form::Iso)?;
    let mut p = f.end;
    for _ in 0..4 {
        let s = skip_spaces(l, p);
        if s == p || s >= l.len() {
            return None;
        }
        p = token_end(l, s);
    }
    let mut s = skip_spaces(l, p);
    if s == p {
        return None;
    }
    match l.get(s) {
        Some(b'-') => s += 1,
        Some(b'[') => {
            // one or more [id key="value" ...] blocks; '\]' and '\"' are escapes
            let mut quoted = false;
            while s < l.len() {
                match l[s] {
                    b'\\' => s += 1,
                    b'"' => quoted = !quoted,
                    b']' if !quoted && l.get(s + 1) != Some(&b'[') => {
                        s += 1;
                        break;
                    }
                    _ => {}
                }
                s += 1;
            }
        }
        _ => return None,
    }
    let body = if l.get(s) == Some(&b' ') { s + 1 } else { s.min(l.len()) };
    Some(SyslogLine { time: f, body, sev })
}

/// An access log line (Apache, nginx): the bracketed `dd/Mmm/yyyy:hh:mm:ss zone` time as the
/// 4th or 5th token (a virtual host may come first), then ` "` and the request, then the
/// status.
struct AccessLine {
    time: times::Found,
    status: Option<u16>,
}

fn access_line(l: &[u8], c: &Clock) -> Option<AccessLine> {
    for k in [3usize, 4] {
        let Some(p) = times::token_start(l, k) else { break };
        if l[p] != b'[' {
            continue;
        }
        let Some(f) = times::read_at(l, p, c).filter(|f| f.form == Form::Access) else { continue };
        if !l[f.end..].starts_with(b" \"") {
            continue;
        }
        let mut i = f.end + 2;
        while i < l.len() && l[i] != b'"' {
            i += if l[i] == b'\\' { 2 } else { 1 };
        }
        let status = if l.get(i + 1) == Some(&b' ') {
            let s = &l[(i + 2).min(l.len())..];
            let n = s.iter().take_while(|b| b.is_ascii_digit()).count();
            if n == 3 { std::str::from_utf8(&s[..3]).ok().and_then(|t| t.parse().ok()) } else { None }
        } else {
            None
        };
        return Some(AccessLine { time: f, status });
    }
    None
}

/// A logfmt line's (key, value) pairs.
type Pairs<'a> = Vec<(&'a [u8], Cow<'a, [u8]>)>;

/// The key=value pairs of a logfmt line (quoted values unescaped), how many tokens it has, and
/// whether its first token is a pair. Keys start with a letter, '_' or '@' and hold letters,
/// digits and `_ . - / @`.
fn logfmt_pairs(l: &[u8]) -> (Pairs<'_>, usize, bool) {
    let mut pairs = Vec::new();
    let mut tokens = 0;
    let mut first_pair = false;
    let mut i = 0;
    let key_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'/' | b'@');
    loop {
        while i < l.len() && (l[i] == b' ' || l[i] == b'\t') {
            i += 1;
        }
        if i >= l.len() {
            break;
        }
        tokens += 1;
        let ks = i;
        if l[i].is_ascii_alphabetic() || l[i] == b'_' || l[i] == b'@' {
            while i < l.len() && key_byte(l[i]) {
                i += 1;
            }
        }
        if i > ks && l.get(i) == Some(&b'=') {
            let key = &l[ks..i];
            i += 1;
            let value = if l.get(i) == Some(&b'"') {
                let vs = i + 1;
                i += 1;
                let mut esc = false;
                while i < l.len() && l[i] != b'"' {
                    if l[i] == b'\\' {
                        esc = true;
                        i += 1;
                    }
                    i += 1;
                }
                let raw = &l[vs..i.min(l.len())];
                i = (i + 1).min(l.len());
                if esc { Cow::Owned(unescape(raw)) } else { Cow::Borrowed(raw) }
            } else {
                let vs = i;
                while i < l.len() && l[i] != b' ' && l[i] != b'\t' {
                    i += 1;
                }
                Cow::Borrowed(&l[vs..i])
            };
            if tokens == 1 {
                first_pair = true;
            }
            pairs.push((key, value));
        } else {
            while i < l.len() && l[i] != b' ' && l[i] != b'\t' {
                i += 1;
            }
        }
    }
    (pairs, tokens, first_pair)
}

/// A logfmt line: its first token a pair, 3 or more pairs, and pairs making two thirds or
/// more of its tokens.
fn is_logfmt(l: &[u8]) -> bool {
    let (pairs, tokens, first) = logfmt_pairs(l);
    first && pairs.len() >= 3 && pairs.len() * 3 >= tokens * 2
}

/// Backslash escapes of a quoted logfmt value: \" \\ \n \t \r.
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 1 < s.len() {
            v.push(match s[i + 1] {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                o => o,
            });
            i += 2;
        } else {
            v.push(s[i]);
            i += 1;
        }
    }
    v
}

/// A csv or tsv profile: the rows have the same number of separators (at least one) outside
/// quotes, a header comes first, and there are 5 rows or more (2 when the file's name says csv
/// or tsv). The header names the time and level columns, if any.
fn csv_profile(base: &str, lines: &[&[u8]], c: &Clock) -> Option<Profile> {
    let tsv_name = base.ends_with(".tsv") || base.ends_with(".tab");
    let csv_name = base.ends_with(".csv");
    let order = if tsv_name { *b"\t," } else { *b",\t" };
    for sep in order {
        let named = (sep == b'\t' && tsv_name) || (sep == b',' && csv_name);
        let Some(header) = csv_header(lines, sep, if named { 2 } else { 5 }, c) else { continue };
        let cols: Vec<Vec<u8>> = split_row(header, sep).iter().map(|f| unquote(f).to_ascii_lowercase()).collect();
        let find = |names: &[&[u8]]| cols.iter().position(|n| names.contains(&&n[..]));
        let format = if sep == b'\t' { Format::Tsv } else { Format::Csv };
        return Some(Profile { format, place: Place::START, sep, time_col: find(&CSV_TIME), level_col: find(&CSV_LEVEL) });
    }
    None
}

/// The header line, when the lines are csv rows (see csv_profile). A quoted field may go on to
/// the next line; blank lines between rows are passed over.
fn csv_header<'a>(lines: &[&'a [u8]], sep: u8, min_rows: usize, c: &Clock) -> Option<&'a [u8]> {
    let mut count = None;
    let mut header = None;
    let mut rows = 0;
    let mut cur = 0;
    let mut open = false;
    for &l in lines {
        if !open && is_blank(l) {
            continue;
        }
        for &b in l {
            if b == b'"' {
                open = !open;
            } else if b == sep && !open {
                cur += 1;
            }
        }
        if open {
            continue;
        }
        match count {
            None => {
                count = Some(cur);
                header = Some(l);
            }
            Some(n) if n != cur => return None,
            _ => {}
        }
        rows += 1;
        cur = 0;
    }
    if rows < min_rows || count.unwrap_or(0) == 0 {
        return None;
    }
    let h = header?;
    // a header: each name holds a letter, is not a time, and is new
    let mut seen: Vec<Cow<[u8]>> = Vec::new();
    for f in split_row(h, sep) {
        let u = unquote(f);
        if u.is_empty() || u.len() > 64 || !u.iter().any(u8::is_ascii_alphabetic) || times::read_at(&u, 0, c).is_some() || seen.contains(&u) {
            return None;
        }
        seen.push(u);
    }
    Some(h)
}

/// The fields of a csv or tsv row, split at separators outside double quotes.
pub(crate) fn split_row(l: &[u8], sep: u8) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut a = 0;
    let mut open = false;
    for (i, &b) in l.iter().enumerate() {
        if b == b'"' {
            open = !open;
        } else if b == sep && !open {
            out.push(&l[a..i]);
            a = i + 1;
        }
    }
    out.push(&l[a..]);
    out
}

/// A csv field without its spaces and quotes ("" inside quotes is one quote).
pub(crate) fn unquote(f: &[u8]) -> Cow<'_, [u8]> {
    let t = trim(f);
    if t.len() >= 2 && t[0] == b'"' && t[t.len() - 1] == b'"' {
        let inner = &t[1..t.len() - 1];
        if inner.windows(2).any(|w| w == b"\"\"") {
            let mut v = Vec::with_capacity(inner.len());
            let mut i = 0;
            while i < inner.len() {
                v.push(inner[i]);
                i += if inner[i] == b'"' && inner.get(i + 1) == Some(&b'"') { 2 } else { 1 };
            }
            return Cow::Owned(v);
        }
        return Cow::Borrowed(inner);
    }
    Cow::Borrowed(t)
}

/// Crash reports: .ips and .crash names, Android tombstones (by name or first line), an .ips
/// header (a JSON line with bug_type) followed by '{', or a macOS crash report as text
/// ("Process:" and "Exception Type:" near the top).
fn is_crash(base: &str, lines: &[&[u8]]) -> bool {
    if base.ends_with(".ips") || base.ends_with(".crash") || base.starts_with("tombstone_") {
        return true;
    }
    let mut it = lines.iter().filter(|l| !is_blank(l));
    let Some(first) = it.next() else { return false };
    if first.starts_with(TOMBSTONE) {
        return true;
    }
    if first.starts_with(b"{") && contains(first, b"\"bug_type\"") && it.next().is_some_and(|l| trim(l) == b"{") {
        return true;
    }
    let top = &lines[..lines.len().min(80)];
    top.iter().any(|l| l.starts_with(b"Process:")) && top.iter().any(|l| l.starts_with(b"Exception Type:"))
}

/// A MySQL slow log: "# User@Host:" and "# Query_time:" lines in the sample.
fn is_mysql_slow(lines: &[&[u8]]) -> bool {
    lines.iter().any(|l| l.starts_with(b"# User@Host:")) && lines.iter().any(|l| l.starts_with(b"# Query_time:"))
}

/// Escape codes (ESC) or '\r' redraws (a '\r' not before '\n') in the raw sample.
fn has_terminal_codes(raw: &[u8]) -> bool {
    raw.iter().enumerate().any(|(i, &b)| b == 0x1b || (b == b'\r' && i + 1 < raw.len() && raw[i + 1] != b'\n'))
}

fn is_blank(l: &[u8]) -> bool {
    l.iter().all(|&b| b == b' ' || b == b'\t')
}

fn trim(s: &[u8]) -> &[u8] {
    let a = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let e = s.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(a, |p| p + 1);
    &s[a..e]
}

fn contains(h: &[u8], n: &[u8]) -> bool {
    h.windows(n.len()).any(|w| w == n)
}

fn skip_spaces(l: &[u8], mut i: usize) -> usize {
    while i < l.len() && (l[i] == b' ' || l[i] == b'\t') {
        i += 1;
    }
    i
}

fn token_end(l: &[u8], mut i: usize) -> usize {
    while i < l.len() && l[i] != b' ' && l[i] != b'\t' {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-18 12:00 UTC.
    const FILE_MS: i64 = 1_795_003_200_000;

    fn clock() -> Clock {
        Clock::new(Zone::Fixed(0), FILE_MS)
    }

    fn kind(name: &str, text: &str) -> Format {
        detect(name, text.as_bytes()).format
    }

    fn text(f: &Fields) -> String {
        match &f.msg {
            Msg::Text(t) => String::from_utf8_lossy(t).into_owned(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn clean_lines() {
        let c = |s: &[u8]| clean_line(s).into_owned();
        assert_eq!(c(b"abc\r\n"), b"abc");
        assert!(matches!(clean_line(b"plain\n"), Cow::Borrowed(_)));
        assert_eq!(c(b"\x1b[31mERROR\x1b[0m boom\n"), b"ERROR boom");
        assert_eq!(c(b"a\x1b(Bb"), b"ab");
        assert_eq!(c(b"10%\r50%\r100%\n"), b"100%");
        assert_eq!(c(b"done 100%\r\r\n"), b"done 100%");
        assert_eq!(c(b"a\x00b\x7fc\td"), b"abc\td");
        assert_eq!(c(b"ok\x1b[3"), b"ok");
    }

    #[test]
    fn names_round_trip() {
        for f in Format::ALL {
            assert_eq!(Format::from_name(f.name()), Some(f));
        }
        assert_eq!(Format::from_name("nope"), None);
    }

    #[test]
    fn level_words_and_letters() {
        for (w, l) in [("FATAL", Level::Error), ("Err", Level::Error), ("severe", Level::Error), ("WARNING", Level::Warn), ("notice", Level::Info), ("Trace", Level::Debug), ("verbose", Level::Debug)] {
            assert_eq!(level_word(w.as_bytes()), Some(l), "{w}");
        }
        assert_eq!(level_word(b"errors"), None);
        assert_eq!(level_word(b"information"), None);
        assert_eq!(text_level(b"2015-10-18 18:01:47,978 INFO [main] org.apache.hadoop"), Level::Info);
        assert_eq!(text_level(b"[Sun Dec 04 04:47:44 2005] [error] mod_jk child workerEnv"), Level::Error);
        assert_eq!(text_level(b"03-17 16:13:38.811  1702  2395 D WindowManager: x error y"), Level::Debug);
        assert_eq!(text_level(b"03-17 16:13:38.811  1702  2395 E AndroidRuntime: FATAL"), Level::Error);
        assert_eq!(text_level(b"[2026-11-18 10:15:02] local.ERROR: boom"), Level::Error);
        assert_eq!(text_level(b"request done err=nil stderr=x error_count=0 /api/error/x"), Level::None);
        assert_eq!(text_level(b"Debugger listening on ws://127.0.0.1"), Level::None);
    }

    #[test]
    fn json_lines() {
        let c = clock();
        let p = Profile::new(Format::Json);
        let l = br#"{"time":"12-17 19:31:36.263","pid":1795,"level":"I","tag":"PowerManager_screenOn","msg":"DisplayPowerStatesetColorFadeLevel: level=1.0"}"#;
        let f = p.fields(l, &c);
        assert_eq!(text(&f), "DisplayPowerStatesetColorFadeLevel: level=1.0");
        assert_eq!((f.level, f.level_text.as_deref()), (Level::Info, Some("I")));
        assert_eq!(f.time, Some(Stamp::Utc(1_765_999_896_263))); // 2025-12-17: the file is from November 2026, so December is the year before
        let f = p.fields(br#"{"message":"m","msg":"first","severity":"WARNING","ts":1700000000.5}"#, &c);
        assert_eq!((text(&f).as_str(), f.level, f.time), ("first", Level::Warn, Some(Stamp::Utc(1_700_000_000_500))));
        let f = p.fields(br#"{"@timestamp":"2026-11-18T10:15:02.123Z","log.level":"error","message":"boom"}"#, &c);
        assert_eq!((f.level, f.time), (Level::Error, Some(Stamp::Utc(1_794_996_902_123))));
        let f = p.fields(br#"{"log":{"level":"warn"},"msg":"x","level_no":1}"#, &c);
        assert_eq!(f.level, Level::Warn);
        // a time field that holds no time: the next one is read
        let f = p.fields(br#"{"time":"soon","date":"2026-11-18 10:15:02","msg":"x"}"#, &c);
        assert_eq!(f.time, Some(Stamp::Utc(1_794_996_902_000)));
        let f = p.fields(br#"{"level":30,"time":1700000000123,"msg":"pino"}"#, &c);
        assert_eq!((f.level, f.level_text.as_deref(), f.time), (Level::Info, Some("30"), Some(Stamp::Utc(1_700_000_000_123))));
        let f = p.fields(br#"{"user":"a","n":2,"user":"b"}"#, &c);
        assert_eq!(f.msg, Msg::Names(vec!["user".into(), "n".into(), "user".into()]));
        assert_eq!(f.body(), None);
        // not a JSON object: read as text
        let f = p.fields(b"{not json", &c);
        assert_eq!(text(&f), "{not json");
    }

    #[test]
    fn docker_cri_and_sessions() {
        let c = clock();
        let d = Profile::new(Format::Docker);
        let f = d.fields(br#"{"log":"\u001b[31mERROR\u001b[0m db down\n","stream":"stderr","time":"2026-11-18T10:15:02.123456789Z"}"#, &c);
        assert_eq!((text(&f).as_str(), f.level, f.stderr, f.partial), ("ERROR db down", Level::Error, true, false));
        assert_eq!(f.time, Some(Stamp::Utc(1_794_996_902_123)));
        let f = d.fields(br#"{"log":"a long line cut by docker","stream":"stdout","time":"2026-11-18T10:15:02Z"}"#, &c);
        assert!(f.partial);
        let f = d.fields(br#"{"log":"{\"level\":\"warn\",\"msg\":\"slow\"}\n","stream":"stdout","time":"2026-11-18T10:15:02Z"}"#, &c);
        assert_eq!((text(&f).as_str(), f.level, f.time), ("slow", Level::Warn, Some(Stamp::Utc(1_794_996_902_000))));
        // another JSON line in a Docker log is read as JSON, not as a cut piece
        let f = d.fields(br#"{"msg":"not docker","level":"info"}"#, &c);
        assert_eq!((text(&f).as_str(), f.partial), ("not docker", false));

        let k = Profile::new(Format::Cri);
        let l = b"2026-11-18T10:15:02.123456789Z stderr P first part of a long line ";
        let f = k.fields(l, &c);
        assert_eq!((text(&f).as_str(), f.partial, f.stderr), ("first part of a long line ", true, true));
        let f = k.fields(b"2026-11-18T10:15:02.2Z stdout F WARN disk 91% full", &c);
        assert_eq!((f.level, f.partial, f.time), (Level::Warn, false, Some(Stamp::Utc(1_794_996_902_200))));
        let f = k.fields(b"2026-11-18T10:15:02Z stdout F", &c);
        assert_eq!(text(&f), "");

        let s = Profile::new(Format::ClaudeSession);
        let l = br#"{"parentUuid":null,"sessionId":"s1","type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"Bash","input":{}}]},"timestamp":"2026-11-18T10:15:02.123Z"}"#;
        let f = s.fields(l, &c);
        assert_eq!(f.msg, Msg::Session(vec!["assistant".into(), "assistant".into(), "text".into(), "Bash".into()]));
        assert_eq!(f.time, Some(Stamp::Utc(1_794_996_902_123)));
        let x = Profile::new(Format::CodexSession);
        let l = br#"{"timestamp":"2026-11-18T10:15:02.123Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#;
        assert_eq!(x.fields(l, &c).msg, Msg::Session(vec!["response_item".into(), "function_call".into(), "shell".into()]));
    }

    #[test]
    fn os_logs() {
        let c = clock();
        let m = br#"{"timestamp":"2026-11-18 10:15:02.123456-0800","messageType":"Fault","eventType":"logEvent","eventMessage":"crashed","subsystem":"com.x"}"#;
        let f = Profile::new(Format::MacosLog).fields(m, &c);
        assert_eq!((text(&f).as_str(), f.level, f.time), ("crashed", Level::Error, Some(Stamp::Utc(1_795_025_702_123))));
        let j = br#"{"__REALTIME_TIMESTAMP":"1700000000123456","PRIORITY":"4","MESSAGE":"disk almost full","_PID":"12"}"#;
        let f = Profile::new(Format::Journal).fields(j, &c);
        assert_eq!((text(&f).as_str(), f.level, f.time), ("disk almost full", Level::Warn, Some(Stamp::Utc(1_700_000_000_123))));
    }

    #[test]
    fn text_formats() {
        let c = clock();
        let p = detect("auth.log", b"Dec 10 06:55:46 LabSZ sshd[24200]: Invalid user webmaster from 173.234.31.186\n");
        assert_eq!(p.format, Format::Syslog);
        let l = b"Dec 10 06:55:46 LabSZ sshd[24200]: Invalid user webmaster";
        let f = p.fields(l, &c);
        assert_eq!(f.body(), Some(&l[..]));
        assert_eq!(f.time, Some(Stamp::Utc(1_765_349_746_000))); // 2025-12-10: December is after the file's November
        let f = p.fields(b"<11>Nov 18 10:15:02 host app[1]: disk ok", &c);
        assert_eq!(f.level, Level::Error); // priority 11: severity 3
        let p5 = Profile::new(Format::Syslog);
        let l = br#"<165>1 2026-11-18T10:15:02.003Z mymachine.example.com evntslog - ID47 [exampleSDID@32473 iut="3" eventSource="Application"] An application event"#;
        let f = p5.fields(l, &c);
        assert_eq!((f.level, f.time), (Level::Info, Some(Stamp::Utc(1_794_996_902_003))));
        let s = syslog_line(l, &c).unwrap();
        assert_eq!(&l[s.body..], b"An application event");

        let a = Profile { place: Place(3), ..Profile::new(Format::Access) };
        let f = a.fields(br#"10.0.0.1 - - [18/Nov/2026:10:00:00 +0000] "GET /x HTTP/1.1" 503 12 "-" "curl/8""#, &c);
        assert_eq!((f.level, f.time), (Level::Error, Some(Stamp::Utc(1_794_996_000_000))));
        let f = a.fields(br#"10.0.0.1 - bob [18/Nov/2026:10:00:00 +0000] "GET /missing HTTP/1.1" 404 0"#, &c);
        assert_eq!(f.level, Level::Warn);

        let lf = Profile::new(Format::Logfmt);
        let f = lf.fields(br#"time="2026-11-18T10:15:02Z" level=error msg="db \"main\" down" retry=3"#, &c);
        assert_eq!((text(&f).as_str(), f.level, f.level_text.as_deref()), ("db \"main\" down", Level::Error, Some("error")));
        assert_eq!(f.time, Some(Stamp::Utc(1_794_996_902_000)));

        let g = Profile::new(Format::Gha);
        let f = g.fields(b"2024-01-15T10:23:45.1234567Z ##[error]Process completed with exit code 1.", &c);
        assert_eq!((text(&f).as_str(), f.level, f.time), ("##[error]Process completed with exit code 1.", Level::Error, Some(Stamp::Utc(1_705_314_225_123))));

        let pl = Profile::new(Format::Plain);
        let f = pl.fields(b"E1118 10:15:02.123456       1 controller.go:12] sync failed", &c);
        assert_eq!((f.level, f.time), (Level::Error, Some(Stamp::Utc(1_794_996_902_123))));
    }

    #[test]
    fn csv_and_mysql_and_crash() {
        let c = clock();
        let p = detect("events.csv", b"time,level,message\n2026-11-18T10:15:02Z,error,\"db, main\"\n2026-11-18T10:15:03Z,info,ok\n");
        assert_eq!((p.format, p.time_col, p.level_col), (Format::Csv, Some(0), Some(1)));
        let f = p.fields(b"2026-11-18T10:15:02Z,error,\"db, main\"", &c);
        assert_eq!((f.level, f.time), (Level::Error, Some(Stamp::Utc(1_794_996_902_000))));
        // without a csv name, two rows are not enough
        assert_eq!(kind("out", "time,level\nnow,info\n"), Format::Plain);

        let m = Profile::new(Format::MysqlSlow);
        assert_eq!(m.fields(b"# Time: 2026-11-18T10:15:02.123456Z", &c).time, Some(Stamp::Utc(1_794_996_902_123)));
        assert_eq!(m.fields(b"SET timestamp=1700000000;", &c).time, Some(Stamp::Utc(1_700_000_000_000)));

        let k = Profile::new(Format::Crash);
        let h = br#"{"app_name":"MyApp","timestamp":"2026-11-18 10:15:02.00 +0100","bug_type":"309","name":"MyApp"}"#;
        let f = k.fields(h, &c);
        assert_eq!((text(&f).as_str(), f.level, f.time), ("MyApp", Level::Error, Some(Stamp::Utc(1_794_993_302_000))));
        assert_eq!(k.fields(b"Date/Time:           2026-11-18 10:15:02.123 +0100", &c).time, Some(Stamp::Utc(1_794_993_302_123)));
    }

    #[test]
    fn detection_of_each_kind() {
        let docker = r#"{"log":"start\n","stream":"stdout","time":"2026-11-18T10:15:02.1Z"}
{"log":"warn x\n","stream":"stderr","time":"2026-11-18T10:15:03.1Z"}
"#;
        assert_eq!(kind("abc-json.log", docker), Format::Docker);
        assert_eq!(kind("x.log", "2026-11-18T10:15:02.123456789Z stdout F hello\n2026-11-18T10:15:02.2Z stderr P part\n"), Format::Cri);
        assert_eq!(kind("job.txt", "\u{feff}2024-01-15T10:23:45.1234567Z ##[group]Run x\n2024-01-15T10:23:45.2234567Z ##[endgroup]\n"), Format::Gha);
        assert_eq!(kind("a.log", "10.0.0.1 - - [18/Nov/2026:10:00:00 +0000] \"GET / HTTP/1.1\" 200 5\n"), Format::Access);
        assert_eq!(kind("a.log", "time=2026-11-18T10:15:02Z level=info msg=hi\nts=1 lvl=warn msg=\"a b\" x=1\n"), Format::Logfmt);
        assert_eq!(kind("slow.log", "# Time: 2026-11-18T10:15:02Z\n# User@Host: a[a] @ localhost []\n# Query_time: 2.0\nSELECT 1;\n"), Format::MysqlSlow);
        assert_eq!(kind("x", "*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***\nBuild fingerprint: 'x'\n"), Format::Crash);
        assert_eq!(kind("MyApp.ips", "{\"bug_type\":\"309\"}\n{\n  \"x\" : 1\n}\n"), Format::Crash);
        assert_eq!(kind("out", "\x1b[32mok\x1b[0m test a\nplain\n"), Format::Terminal);
        assert_eq!(kind("out", "10%\r50%\r100%\ndone\n"), Format::Terminal);
        assert_eq!(kind("t.tsv", "a\tb\n1\t2\n"), Format::Tsv);
        assert_eq!(kind("x.log", "hello\nworld\n"), Format::Plain);
        assert_eq!(kind("x.log", ""), Format::Plain);
        // 80% rule: 4 JSON lines of 5 is enough, 3 of 5 is not
        assert_eq!(kind("x", "{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\nplain\n"), Format::Json);
        assert_eq!(kind("x", "{\"a\":1}\n{\"a\":1}\n{\"a\":1}\nplain\nplain\n"), Format::Plain);
    }

    #[test]
    fn a_line_cut_at_64_kib_is_not_sampled() {
        let line = "{\"msg\":\"x\"}\n";
        let mut head = line.repeat(SAMPLE_BYTES / line.len() + 1).into_bytes();
        head.truncate(SAMPLE_BYTES);
        assert!(!head.ends_with(b"\n"));
        assert_eq!(detect("x", &head).format, Format::Json);
    }
}
