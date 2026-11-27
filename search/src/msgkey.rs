//! The stable message key, rule v1, as pure functions.
//!
//! fewer's packer picks patterns per block to save bytes, so one message can get different
//! patterns in different blocks, runs or archives. The key instead comes from one fixed rule
//! applied to the line's own text. Two lines share a key exactly when their rewritten text is
//! identical: no similarity threshold, no eviction, exact counts.
//!
//! A key depends on nothing but the line's bytes: not on the source's log type, not on the
//! lines around it, not on how a file was cut into pieces. So a file saved whole and the same
//! file saved in pieces give every line the same key, and keys match across runs by their text.
//!
//! The rule works on bytes. A letter is an ASCII letter or a byte >= 0x80; a digit is 0-9.
//! 1. **Clean** ([`clean_line`]): the line end goes ('\n' and a '\r'
//!    before it); escape sequences go; a line redrawn with '\r' keeps its final state; the other
//!    control bytes but tab go. Then a UTF-8 byte order mark at the start goes.
//! 2. **Message part**: a JSON line (it starts with '{', is at most 1 MiB and parses as an
//!    object) is keyed by 5.2 ([`json_key`]); a CRI line (`<RFC 3339 time> stdout|stderr P|F
//!    <text>`) by its text, read as a JSON line when it is one; any other line whole.
//! 3. **Quoted strings**: `"..."` (backslash escapes; closed in the same message) becomes
//!    `<str>`. A quote that is never closed stays as written.
//! 4. **Values**: a value candidate is a longest run of letters, digits and the bytes
//!    `. : / @ % + ~ -` ('_' is not one: `blk_-123` gives `blk`, `_` and `-123`). A drive
//!    letter and ':' followed by '\' go on over the backslashes (`C:\Windows\x.dll` is one
//!    candidate). Each candidate is checked in this order:
//!    - a UUID (8-4-4-4-12 hex digits) gives `<uuid>`;
//!    - a date or a time of day written with its separators, alone or joined without a space,
//!      gives `<time>` (see [`is_time`]); bare digits such as `081109` are not times. A date
//!      written with spaces and a month's name, `Dec 10 06:55:46` (syslog) or
//!      `Sun Dec 04 04:47:44 2005` (ctime), is one `<time>` too, so keys do not change with
//!      the month or the day of the week;
//!    - an IPv4 address gives `<ip>`, with a port `<ip>:<num>`; hex digits and ':' with two or
//!      more ':' give `<ip>`;
//!    - `scheme://...` gives `<url>`;
//!    - a path (it starts with `/`, `./`, `../`, `~/` or a drive like `C:\` or `C:/`, or it
//!      holds two or more '/') gives `<path>`;
//!    - any other candidate holding a digit, or made of 12 or more of the letters a-f (any
//!      case), gives `<num>`;
//!    - a candidate without a digit stays as written: words, levels, `ERROR:`, `dfs.DataNode`.
//! 5. **The rest** stays as written, except that runs of spaces, tabs and other control bytes
//!    become one space, and leading and trailing ones go. A message part over 64 KiB is keyed on
//!    its first 64 KiB plus ` <long>`.
//!
//! JSON lines (5.2), in this order:
//! - Docker json-file lines (`log` a string, with `stream` and `time`): the message is `log`,
//!   cleaned as a line is (its final '\n' goes, and colors and redraws as in step 1). When that
//!   text is itself a JSON line, it is keyed as one;
//! - Claude Code session lines (`sessionId` and `type`): `{session <type> <message.role>
//!   <first content block's type> <first tool name>}`, each part only when present;
//! - Codex session lines (`payload`, `type` and `timestamp`, or `record_type`): the same with
//!   Codex's fields (see [`codex_parts`]);
//! - else the message is the first string among the top-level fields msg, message, log,
//!   MESSAGE, eventMessage, event, text, error, err, and the key is `{<level>} ` + the rule on
//!   it, where the level is the first of level, severity, lvl, levelname, log.level,
//!   messageType, PRIORITY that holds a string or a number, as written. Without a level field
//!   the key is the rule on the message alone; with an empty result it is `{<level>}` alone;
//! - without a message field: `{` + the top-level field names in order, joined by ',', + `}`.
//!
//! checks/msgkey_ref.py writes the same rule again in Python; both give the same keys on the
//! samples and on random lines.

use std::collections::HashMap;

use crate::formats::clean_line;
use crate::json::{self, Json};

/// The rule's version, stored in every catalog. A new rule rebuilds the catalogs.
pub const RULE: u32 = 1;

/// A message part longer than this is keyed on its first `MESSAGE_MAX` bytes plus ` <long>`.
pub const MESSAGE_MAX: usize = 64 * 1024;

/// A line is read as JSON only up to this size.
pub const JSON_MAX: usize = 1 << 20;

/// The fields that hold a JSON line's message, in order.
pub const MESSAGE_FIELDS: [&str; 9] = ["msg", "message", "log", "MESSAGE", "eventMessage", "event", "text", "error", "err"];

/// The fields that hold a JSON line's level, in order. `log.level` is also
/// read as {"log": {"level": ...}} when there is no field named "log.level".
pub const LEVEL_FIELDS: [&str; 7] = ["level", "severity", "lvl", "levelname", "log.level", "messageType", "PRIORITY"];

const BOM: &[u8] = b"\xEF\xBB\xBF";

const MONTHS: [&[u8; 3]; 12] = [b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov", b"dec"];
const WEEKDAYS: [&[u8; 3]; 7] = [b"mon", b"tue", b"wed", b"thu", b"fri", b"sat", b"sun"];

// ---------------------------------------------------------------------------------------
// Lines

/// The key of one line, given as its raw bytes (with or without its line end).
pub fn line_key(line: &[u8]) -> Vec<u8> {
    clean_key(&clean_line(line))
}

/// The key of a line already cleaned by [`clean_line`] (steps 2 to 5), for callers that
/// have the clean text anyway (the catalog reads its fields from it too).
pub fn clean_key(clean: &[u8]) -> Vec<u8> {
    key_of(clean, None)
}

/// A `<num>` placeholder of a key: where it is in the key (its first byte), and the value it
/// stands for when that is a plain decimal (an optional sign, digits, and an optional '.' and
/// digits), maybe with a unit right after it ("250ms", "1.5s", "99%"; [`UNITS`]), at most
/// [`PLAIN_MAX`] bytes. Search answers show the ranges of measured values (with a unit here,
/// or the unit word after the placeholder in the key) over lines with the same key
/// ("12-48 ms").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumAt {
    pub at: u32,
    pub num: Option<Vec<u8>>,
}

/// The longest value kept.
pub const PLAIN_MAX: usize = 24;

/// Units of measured values, in lowercase: time, size and percent.
pub const UNITS: [&str; 30] = [
    "ns", "us", "µs", "ms", "s", "sec", "secs", "second", "seconds", "m", "min", "mins", "minute", "minutes", "h", "hr", "hrs", "hour", "hours", "d", "b", "byte",
    "bytes", "kb", "mb", "gb", "kib", "mib", "gib", "%",
];

/// A word is one of [`UNITS`] (any ASCII case).
pub fn is_unit(w: &[u8]) -> bool {
    UNITS.iter().any(|u| u.as_bytes().eq_ignore_ascii_case(w))
}

/// A value's number and its unit (empty when it has none): "250ms" gives "250" and "ms".
pub fn num_parts(v: &[u8]) -> (&[u8], &[u8]) {
    let mut i = usize::from(matches!(v.first(), Some(b'-' | b'+')));
    while v.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if v.get(i) == Some(&b'.') && v.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while v.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    v.split_at(i)
}

/// [`line_key`], and the values of the key's `<num>` placeholders, in order (`vals` is
/// cleared first). The key is made by the same code as [`line_key`]'s, so it is the same key.
pub fn line_key_values(line: &[u8], vals: &mut Vec<NumAt>) -> Vec<u8> {
    vals.clear();
    key_of(&clean_line(line), Some(vals))
}

/// Steps 2 to 5, with the `<num>` values collected when `vals` is given.
fn key_of(clean: &[u8], vals: Option<&mut Vec<NumAt>>) -> Vec<u8> {
    let c = clean.strip_prefix(BOM).unwrap_or(clean);
    if let Some(o) = json_line(c) {
        return json_key_v(&o, vals);
    }
    if let Some(t) = cri_text(c) {
        return match json_line(t) {
            Some(o) => json_key_v(&o, vals),
            None => text_key_v(t, vals),
        };
    }
    text_key_v(c, vals)
}

/// A JSON line: it starts with '{', is at most 1 MiB, and parses as an object.
fn json_line(c: &[u8]) -> Option<Json> {
    if c.first() != Some(&b'{') || c.len() > JSON_MAX {
        return None;
    }
    json::parse(c).ok().filter(|v| matches!(v, Json::Obj(_)))
}

/// A CRI line's text: `<time> stdout|stderr P|F` then the end of the line or one space and
/// the text. The time is RFC 3339 as container runtimes write it: yyyy-mm-ddThh:mm:ss, an
/// optional '.' and 1 to 9 digits, then 'Z' or +hh:mm or -hh:mm (shapes only).
fn cri_text(c: &[u8]) -> Option<&[u8]> {
    let t = rfc3339_end(c)?;
    let rest = &c[t..];
    let rest = rest.strip_prefix(b" stdout ").or_else(|| rest.strip_prefix(b" stderr "))?;
    let (&tag, rest) = rest.split_first()?;
    if tag != b'P' && tag != b'F' {
        return None;
    }
    if rest.is_empty() { Some(rest) } else { rest.strip_prefix(b" ") }
}

fn rfc3339_end(c: &[u8]) -> Option<usize> {
    let d = |i: usize, n: usize| digits(c, i) == n;
    let at = |i: usize, b: u8| c.get(i) == Some(&b);
    let date_time = d(0, 4) && at(4, b'-') && d(5, 2) && at(7, b'-') && d(8, 2) && at(10, b'T') && d(11, 2) && at(13, b':') && d(14, 2) && at(16, b':') && d(17, 2);
    if !date_time {
        return None;
    }
    let mut i = 19;
    if at(i, b'.') {
        let n = digits(c, i + 1);
        if !(1..=9).contains(&n) {
            return None;
        }
        i += 1 + n;
    }
    match c.get(i) {
        Some(b'Z') => Some(i + 1),
        Some(b'+' | b'-') if d(i + 1, 2) && at(i + 3, b':') && d(i + 4, 2) => Some(i + 6),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// JSON lines

/// The key of a JSON line (see the module's doc).
pub fn json_key(o: &Json) -> Vec<u8> {
    json_key_v(o, None)
}

fn json_key_v(o: &Json, vals: Option<&mut Vec<NumAt>>) -> Vec<u8> {
    let has = |k: &str| o.get(k).is_some();
    if has("stream")
        && has("time")
        && let Some(log) = o.get("log").and_then(Json::str)
    {
        // Docker: `log` is a program's raw output, cleaned as a line is
        let m = clean_line(log.as_bytes());
        if let Some(inner) = json_line(&m) {
            return json_key_v(&inner, vals);
        }
        return leveled(o, &m, vals);
    }
    if has("sessionId") && has("type") {
        return session_key(&claude_parts(o));
    }
    if (has("payload") && has("type") && has("timestamp")) || has("record_type") {
        return session_key(&codex_parts(o));
    }
    match message_field(o) {
        Some(m) => leveled(o, m.as_bytes(), vals),
        None => names_key(o),
    }
}

/// The first string among a JSON line's message fields ([`MESSAGE_FIELDS`]).
pub fn message_field(o: &Json) -> Option<&str> {
    MESSAGE_FIELDS.iter().find_map(|k| o.get(k).and_then(Json::str))
}

/// A JSON line's level field: the first of [`LEVEL_FIELDS`] that holds a string or a number,
/// and its value as written (a number's text as written).
pub fn level_field(o: &Json) -> Option<(&'static str, &str)> {
    for k in LEVEL_FIELDS {
        let v = if k == "log.level" { o.get(k).or_else(|| o.get("log").and_then(|l| l.get("level"))) } else { o.get(k) };
        if let Some(Json::Str(s) | Json::Num(s)) = v {
            return Some((k, s.as_str()));
        }
    }
    None
}

/// `{<level>} ` + the rule on the message; the rule alone without a level field.
fn leveled(o: &Json, m: &[u8], mut vals: Option<&mut Vec<NumAt>>) -> Vec<u8> {
    let k = text_key_v(m, vals.as_deref_mut());
    let Some((_, level)) = level_field(o) else { return k };
    let mut out = Vec::with_capacity(level.len() + 3 + k.len());
    out.push(b'{');
    out.extend_from_slice(level.as_bytes());
    out.push(b'}');
    if !k.is_empty() {
        out.push(b' ');
        // (the placeholders move by the length of the level part)
        if let Some(v) = vals {
            for n in v.iter_mut() {
                n.at += out.len() as u32;
            }
        }
        out.extend_from_slice(&k);
    }
    out
}

/// `{` + the top-level field names in order (repeated names too), joined by ',', + `}`.
fn names_key(o: &Json) -> Vec<u8> {
    let mut out = vec![b'{'];
    if let Json::Obj(m) = o {
        for (n, (k, _)) in m.iter().enumerate() {
            if n > 0 {
                out.push(b',');
            }
            out.extend_from_slice(k.as_bytes());
        }
    }
    out.push(b'}');
    out
}

/// `{session` + one space and each part + `}`.
fn session_key(parts: &[String]) -> Vec<u8> {
    let mut out = b"{session".to_vec();
    for p in parts {
        out.push(b' ');
        out.extend_from_slice(p.as_bytes());
    }
    out.push(b'}');
    out
}

fn push_str(p: &mut Vec<String>, v: Option<&Json>) {
    if let Some(s) = v.and_then(Json::str) {
        p.push(s.to_string());
    }
}

/// A Claude Code session line's key parts, each only when it is a string: type,
/// message.role, the first content block's type, and the `name` of the first content block
/// that has a string `name` (the tool's name).
pub fn claude_parts(o: &Json) -> Vec<String> {
    let mut p = Vec::new();
    push_str(&mut p, o.get("type"));
    let m = o.get("message");
    push_str(&mut p, m.and_then(|m| m.get("role")));
    if let Some(Json::Arr(blocks)) = m.and_then(|m| m.get("content")) {
        push_str(&mut p, blocks.first().and_then(|b| b.get("type")));
        push_str(&mut p, blocks.iter().find_map(|b| b.get("name").filter(|n| n.str().is_some())));
    }
    p
}

/// A Codex CLI session line's key parts, each only when it is a string: type, then
/// payload.type, payload.role, the first content block's type and payload.name (older lines
/// carry role, content and name at the top, without a payload).
pub fn codex_parts(o: &Json) -> Vec<String> {
    let mut p = Vec::new();
    push_str(&mut p, o.get("type"));
    let body = match o.get("payload") {
        Some(b) => {
            push_str(&mut p, b.get("type"));
            b
        }
        None => o,
    };
    push_str(&mut p, body.get("role"));
    if let Some(Json::Arr(blocks)) = body.get("content") {
        push_str(&mut p, blocks.first().and_then(|b| b.get("type")));
    }
    push_str(&mut p, body.get("name"));
    p
}

// ---------------------------------------------------------------------------------------
// The rule on a message (steps 3 to 5)

/// The rule on a message part (steps 3 to 5), with the 64 KiB limit.
pub fn text_key(m: &[u8]) -> Vec<u8> {
    text_key_v(m, None)
}

fn text_key_v(m: &[u8], vals: Option<&mut Vec<NumAt>>) -> Vec<u8> {
    if m.len() <= MESSAGE_MAX {
        return rule(m, vals);
    }
    let mut k = rule(&m[..MESSAGE_MAX], vals);
    if !k.is_empty() {
        k.push(b' ');
    }
    k.extend_from_slice(b"<long>");
    k
}

/// Steps 3 to 5 in one pass from left to right; each `<num>` placeholder written is noted in
/// `vals` when it is given.
fn rule(m: &[u8], mut vals: Option<&mut Vec<NumAt>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(m.len().min(4096));
    let mut gap = false;
    // once a quote finds no closing quote, no later quote does (they are all escaped in that
    // quote's scan, which goes on the same way from each of them)
    let mut quotes_close = true;
    let mut i = 0;
    while i < m.len() {
        let b = m[i];
        if is_blank(b) {
            gap = true;
            i += 1;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(b' ');
        }
        gap = false;
        if b == b'"' {
            let end = if quotes_close { quote_end(m, i) } else { None };
            match end {
                Some(e) => {
                    out.extend_from_slice(b"<str>");
                    i = e;
                }
                None => {
                    quotes_close = false;
                    out.push(b'"');
                    i += 1;
                }
            }
            continue;
        }
        if !is_cand(b) {
            out.push(b);
            i += 1;
            continue;
        }
        if b.is_ascii_alphabetic()
            && let Some(e) = spaced_time(m, i)
        {
            out.extend_from_slice(b"<time>");
            i = e;
            continue;
        }
        let e = cand_end(m, i);
        let at = out.len();
        if classify(&m[i..e], &mut out)
            && let Some(v) = vals.as_deref_mut()
        {
            v.push(NumAt { at: at as u32, num: is_value(&m[i..e]).then(|| m[i..e].to_vec()) });
        }
        i = e;
    }
    out
}

/// A plain decimal (an optional '-' or '+', digits, and an optional '.' and digits), maybe
/// with a unit right after it, at most [`PLAIN_MAX`] bytes.
fn is_value(c: &[u8]) -> bool {
    let (n, unit) = num_parts(c);
    let digits = n.strip_prefix(b"-").or_else(|| n.strip_prefix(b"+")).unwrap_or(n);
    c.len() <= PLAIN_MAX && digits.first().is_some_and(u8::is_ascii_digit) && (unit.is_empty() || is_unit(unit))
}

/// Space, tab and the other control bytes.
fn is_blank(b: u8) -> bool {
    b <= b' ' || b == 0x7f
}

/// A byte of a value candidate: a letter (ASCII or >= 0x80), a digit, or . : / @ % + ~ -
fn is_cand(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80 || matches!(b, b'.' | b':' | b'/' | b'@' | b'%' | b'+' | b'~' | b'-')
}

/// The end of the quoted string that starts at `m[i]` ('"'), after its closing quote.
fn quote_end(m: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < m.len() {
        match m[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The end of the value candidate that starts at `m[i]`. A drive letter and ':' followed by
/// '\' (`C:\...`) go on over backslashes too.
fn cand_end(m: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < m.len() && is_cand(m[j]) {
        j += 1;
    }
    if j == i + 2 && m[i].is_ascii_alphabetic() && m[i + 1] == b':' && m.get(j) == Some(&b'\\') {
        while j < m.len() && (is_cand(m[j]) || m[j] == b'\\') {
            j += 1;
        }
    }
    j
}

/// Writes a candidate's placeholder, or the candidate itself when it is not a value. True
/// when the placeholder written is `<num>`.
fn classify(c: &[u8], out: &mut Vec<u8>) -> bool {
    let placeholder: &[u8] = if is_uuid(c) {
        b"<uuid>"
    } else if is_time(c) {
        b"<time>"
    } else if ipv4_end(c) == Some(c.len()) {
        b"<ip>"
    } else if is_ipv4_port(c) {
        b"<ip>:<num>"
    } else if is_hex_ip(c) {
        b"<ip>"
    } else if is_url(c) {
        b"<url>"
    } else if is_path(c) {
        b"<path>"
    } else if c.iter().any(u8::is_ascii_digit) || (c.len() >= 12 && c.iter().all(|&b| matches!(b, b'a'..=b'f' | b'A'..=b'F'))) {
        out.extend_from_slice(b"<num>");
        return true;
    } else {
        out.extend_from_slice(c);
        return false;
    };
    out.extend_from_slice(placeholder);
    false
}

/// The number of ASCII digits from `c[i]` on.
fn digits(c: &[u8], i: usize) -> usize {
    c.get(i..).map_or(0, |t| t.iter().take_while(|b| b.is_ascii_digit()).count())
}

/// 8-4-4-4-12 hex digits (any case).
fn is_uuid(c: &[u8]) -> bool {
    c.len() == 36 && c.iter().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { *b == b'-' } else { b.is_ascii_hexdigit() })
}

/// A date or a time of day written with its separators, alone or joined without a space:
/// - a date: yyyy-m-d, yyyy/m/d (m and d of 1 or 2 digits), yyyy.mm.dd; d-m-yy, d/m/yy,
///   d-m-yyyy, d/m/yyyy (d and m of 1 or 2 digits), dd.mm.yyyy; d/Mon/yyyy and d-Mon-yyyy
///   (a month's short name in any case);
/// - a time of day: h:mm:ss (h of 1 or 2 digits), then an optional '.' and digits, then an
///   optional zone: Z, z, +hh, +hhmm or +hh:mm (or '-');
/// - joined: a date, then 'T' or ':' and a time of day (2026-11-18T10:15:02.123Z,
///   18/Nov/2026:10:00:00), or '-' and a time of day with ':' or with '.' (BGL's
///   2005-06-03-15.42.50.675872; no zone after the '.' form).
///
/// Only the shapes are checked, not the values, so one field's values never fall on two
/// sides of the check.
pub fn is_time(c: &[u8]) -> bool {
    let n = c.len();
    let zoned_end = |t: usize| zone_end(c, t) == n;
    match date_end(c) {
        Some(d) if d == n => true,
        Some(d) => match c[d] {
            b'T' | b':' => tod_end(c, d + 1).is_some_and(zoned_end),
            b'-' => dots_tod_end(c, d + 1) == Some(n) || tod_end(c, d + 1).is_some_and(zoned_end),
            _ => false,
        },
        None => tod_end(c, 0).is_some_and(zoned_end),
    }
}

/// The end of a date at the start of `c` (forms in [`is_time`]).
fn date_end(c: &[u8]) -> Option<usize> {
    let r1 = digits(c, 0);
    let s = *c.get(r1)?;
    if r1 == 4 && matches!(s, b'-' | b'/' | b'.') {
        let ok = |n: usize| if s == b'.' { n == 2 } else { (1..=2).contains(&n) };
        let r2 = digits(c, 5);
        if !ok(r2) || c.get(5 + r2) != Some(&s) {
            return None;
        }
        let k = 6 + r2;
        let r3 = digits(c, k);
        return ok(r3).then_some(k + r3);
    }
    if (1..=2).contains(&r1) && matches!(s, b'-' | b'/') {
        let j = r1 + 1;
        if name3(c, j, &MONTHS) && c.get(j + 3) == Some(&s) {
            return (digits(c, j + 4) == 4).then_some(j + 8);
        }
        let r2 = digits(c, j);
        if !(1..=2).contains(&r2) || c.get(j + r2) != Some(&s) {
            return None;
        }
        let k = j + r2 + 1;
        let r3 = digits(c, k);
        return (r3 == 2 || r3 == 4).then_some(k + r3);
    }
    if r1 == 2 && s == b'.' && digits(c, 3) == 2 && c.get(5) == Some(&b'.') && digits(c, 6) == 4 {
        return Some(10);
    }
    None
}

/// The end of a time of day h:mm:ss[.digits] at `c[i]`.
fn tod_end(c: &[u8], i: usize) -> Option<usize> {
    let h = digits(c, i);
    if !(1..=2).contains(&h) || c.get(i + h) != Some(&b':') {
        return None;
    }
    let j = i + h + 1;
    if digits(c, j) != 2 || c.get(j + 2) != Some(&b':') || digits(c, j + 3) != 2 {
        return None;
    }
    Some(fraction_end(c, j + 5))
}

/// The end of BGL's time of day hh.mm.ss[.digits] at `c[i]`.
fn dots_tod_end(c: &[u8], i: usize) -> Option<usize> {
    let ok = digits(c, i) == 2 && c.get(i + 2) == Some(&b'.') && digits(c, i + 3) == 2 && c.get(i + 5) == Some(&b'.') && digits(c, i + 6) == 2;
    ok.then(|| fraction_end(c, i + 8))
}

/// After '.' and one or more digits at `c[e]`, their end; else `e`.
fn fraction_end(c: &[u8], e: usize) -> usize {
    let n = digits(c, e + 1);
    if c.get(e) == Some(&b'.') && n > 0 { e + 1 + n } else { e }
}

/// The end of an optional zone at `c[i]`: Z, z, +hh, +hhmm, +hh:mm (or '-'); else `i`.
fn zone_end(c: &[u8], i: usize) -> usize {
    match c.get(i) {
        Some(b'Z' | b'z') => i + 1,
        Some(b'+' | b'-') => match digits(c, i + 1) {
            4 => i + 5,
            2 if c.get(i + 3) == Some(&b':') && digits(c, i + 4) == 2 => i + 6,
            2 => i + 3,
            _ => i,
        },
        _ => i,
    }
}

/// Three ASCII letters at `c[i]` whose lowercase is one of `names`.
fn name3(c: &[u8], i: usize, names: &[&[u8; 3]]) -> bool {
    let Some(t) = c.get(i..i + 3) else { return false };
    let l = [t[0].to_ascii_lowercase(), t[1].to_ascii_lowercase(), t[2].to_ascii_lowercase()];
    t.iter().all(u8::is_ascii_alphabetic) && names.iter().any(|n| **n == l)
}

/// A date and time written with spaces, starting at `m[i]`: an optional weekday and one
/// space, a month's short name, one or two spaces, a day of 1 or 2 digits, one space, a time
/// of day (a whole candidate, as in [`is_time`]), and an optional space and 4-digit year (a
/// whole candidate). `Dec 10 06:55:46` (syslog), `Jun  9 06:06:20`, `Sun Dec 04 04:47:44 2005`
/// (ctime). Its end.
fn spaced_time(m: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    if name3(m, j, &WEEKDAYS) && m.get(j + 3) == Some(&b' ') {
        j += 4;
    }
    if !(name3(m, j, &MONTHS) && m.get(j + 3) == Some(&b' ')) {
        return None;
    }
    j += 4;
    if m.get(j) == Some(&b' ') {
        j += 1;
    }
    let d = digits(m, j);
    if !(1..=2).contains(&d) || m.get(j + d) != Some(&b' ') {
        return None;
    }
    j += d + 1;
    let t = cand_end(m, j);
    let c = &m[j..t];
    if c.is_empty() || !tod_end(c, 0).is_some_and(|e| zone_end(c, e) == c.len()) {
        return None;
    }
    let mut end = t;
    if m.get(end) == Some(&b' ') {
        let y = cand_end(m, end + 1);
        if y == end + 5 && m[end + 1..y].iter().all(u8::is_ascii_digit) {
            end = y;
        }
    }
    Some(end)
}

/// The end of an IPv4 address (4 groups of 1 to 3 digits) at the start of `c`.
fn ipv4_end(c: &[u8]) -> Option<usize> {
    let mut i = 0;
    for k in 0..4 {
        if k > 0 {
            if c.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
        let n = digits(c, i);
        if !(1..=3).contains(&n) {
            return None;
        }
        i += n;
    }
    Some(i)
}

/// An IPv4 address, ':' and a port of 1 or more digits.
fn is_ipv4_port(c: &[u8]) -> bool {
    ipv4_end(c).is_some_and(|e| c.get(e) == Some(&b':') && digits(c, e + 1) > 0 && e + 1 + digits(c, e + 1) == c.len())
}

/// Hex digits and ':' only, with two or more ':' and at least one hex digit (IPv6, MAC).
fn is_hex_ip(c: &[u8]) -> bool {
    c.iter().all(|b| b.is_ascii_hexdigit() || *b == b':') && c.iter().filter(|&&b| b == b':').count() >= 2 && c.iter().any(u8::is_ascii_hexdigit)
}

/// `scheme://...`: an ASCII letter, then letters, digits, '+', '.' or '-', then "://".
fn is_url(c: &[u8]) -> bool {
    if !c.first().is_some_and(u8::is_ascii_alphabetic) {
        return false;
    }
    let n = c.iter().take_while(|&&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')).count();
    c[n..].starts_with(b"://")
}

/// It starts with `/`, `./`, `../`, `~/` or a drive (`C:\`, `C:/`), or holds two or more '/'.
fn is_path(c: &[u8]) -> bool {
    c.starts_with(b"/")
        || c.starts_with(b"./")
        || c.starts_with(b"../")
        || c.starts_with(b"~/")
        || (c.len() >= 3 && c[0].is_ascii_alphabetic() && c[1] == b':' && (c[2] == b'\\' || c[2] == b'/'))
        || c.iter().filter(|&&b| b == b'/').count() >= 2
}

// ---------------------------------------------------------------------------------------
// Counting keys

/// Keys and how many times each was counted, in order of first appearance.
#[derive(Clone, Debug, Default)]
pub struct KeyCounts {
    keys: Vec<(Vec<u8>, u64)>,
    at: HashMap<Vec<u8>, usize>,
}

impl KeyCounts {
    pub fn new() -> KeyCounts {
        KeyCounts::default()
    }

    /// Counts `n` more for `key`.
    pub fn add(&mut self, key: &[u8], n: u64) {
        match self.at.get(key) {
            Some(&i) => self.keys[i].1 += n,
            None => {
                self.at.insert(key.to_vec(), self.keys.len());
                self.keys.push((key.to_vec(), n));
            }
        }
    }

    /// Adds another count after this one (the next piece of the same file, for example).
    pub fn add_all(&mut self, other: &KeyCounts) {
        for (k, n) in &other.keys {
            self.add(k, *n);
        }
    }

    /// The number of distinct keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The count of one key (0 when never counted).
    pub fn get(&self, key: &[u8]) -> u64 {
        self.at.get(key).map_or(0, |&i| self.keys[i].1)
    }

    /// The keys and their counts, in order of first appearance.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], u64)> {
        self.keys.iter().map(|(k, n)| (&k[..], *n))
    }

    /// `<count>\t<key>\n` per key, in order of first appearance, each key written by
    /// [`write_key`]: the form `fewer keys --dump` prints and checks/msgkey_ref.py reads.
    pub fn dump(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (k, n) in &self.keys {
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'\t');
            write_key(k, &mut out);
            out.push(b'\n');
        }
        out
    }
}

/// The keys of a text's lines, in order: one per line, the last line with or without '\n'.
pub fn line_keys(text: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    text.split_inclusive(|&b| b == b'\n').map(line_key)
}

/// How many lines of a text have each key.
pub fn count_lines(text: &[u8]) -> KeyCounts {
    let mut c = KeyCounts::new();
    for k in line_keys(text) {
        c.add(&k, 1);
    }
    c
}

/// How many records of a text have each key, as `fewer keys --dump` prints them:
/// the source's log type is named from `name` and the text's first 64 KiB
/// (formats.rs), lines are grouped into records (records.rs), and each record counts once,
/// under the key of its key line. Times play no part, so a fixed clock is used.
pub fn count_records(name: &str, text: &[u8]) -> KeyCounts {
    use crate::records::Records;
    use crate::times::{Clock, Zone};
    let profile = crate::formats::detect(name, text);
    let clock = Clock::new(Zone::Fixed(0), 1_795_003_200_000);
    let mut recs = Records::new(profile.format);
    let mut counts = KeyCounts::new();
    let mut current: Option<Vec<u8>> = None;
    for raw in text.split_inclusive(|&b| b == b'\n') {
        let clean = clean_line(raw);
        let step = recs.push_fields(&profile.fields(&clean, &clock));
        if step.start
            && let Some(k) = current.take()
        {
            counts.add(&k, 1);
        }
        if step.start || step.key {
            current = Some(clean_key(&clean));
        }
    }
    if let Some(k) = current {
        counts.add(&k, 1);
    }
    counts
}

/// A key on one line of text: '\' as `\\`, control bytes (below 0x20, and 0x7F) as `\xHH`,
/// every other byte as it is. Keys of text lines never hold control bytes; field names,
/// levels and session parts of JSON lines are kept as written and may.
pub fn write_key(key: &[u8], out: &mut Vec<u8>) {
    for &b in key {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b if b < 0x20 || b == 0x7f => out.extend_from_slice(format!("\\x{b:02x}").as_bytes()),
            b => out.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(line: &str) -> String {
        String::from_utf8(line_key(line.as_bytes())).unwrap()
    }

    fn kb(line: &[u8]) -> Vec<u8> {
        line_key(line)
    }

    #[test]
    fn the_design_example() {
        assert_eq!(
            k("081109 203615 148 INFO dfs.DataNode$PacketResponder: PacketResponder 1 for block blk_38865049064139660 terminating\n"),
            "<num> <num> <num> INFO dfs.DataNode$PacketResponder: PacketResponder <num> for block blk_<num> terminating"
        );
    }

    #[test]
    fn values() {
        // UUIDs
        assert_eq!(k("job 550e8400-e29b-41d4-a716-446655440000 done"), "job <uuid> done");
        assert_eq!(k("id 550E8400-E29B-41D4-A716-446655440000"), "id <uuid>");
        assert_eq!(k("req-550e8400-e29b-41d4-a716-446655440000"), "<num>"); // not only a UUID
        // IPv4, ports, IPv6 and MAC addresses
        assert_eq!(k("from 173.234.31.186 port 22"), "from <ip> port <num>");
        assert_eq!(k("to 10.251.73.220:50010 ok"), "to <ip>:<num> ok");
        assert_eq!(k("addr fe80::1ff:fe23:4567:890a and ::1 and 0:0:0:0:0:0:0:0"), "addr <ip> and <ip> and <ip>");
        assert_eq!(k("mac 00:1a:2b:3c:4d:5e"), "mac <ip>");
        assert_eq!(k("just :: here"), "just :: here"); // no hex digit
        // URLs and paths
        assert_eq!(k("GET https://example.com/a/b?x=1 done"), "GET <url>?x=<num> done");
        assert_eq!(k("open /var/log/syslog ./a ../b ~/c x/y/z"), "open <path> <path> <path> <path> <path>");
        assert_eq!(k("and/or TCP/IP"), "and/or TCP/IP"); // one '/' and no digit: words
        assert_eq!(k("HTTP/1.1"), "<num>");
        assert_eq!(k(r"Core: C:\Windows\winsxs\x.dll ok"), "Core: <path> ok");
        assert_eq!(k("C:/Users/x"), "<path>");
        // numbers and hex IDs
        assert_eq!(k("took 12ms, 0x1F, -3, 1.5e3, 99%"), "took <num>, <num>, <num>, <num>, <num>");
        assert_eq!(k("commit deadbeefcafe and DEADBEEFCAFEBABE and decade"), "commit <num> and <num> and decade");
        assert_eq!(k("blk_-1608999687919862906"), "blk_<num>");
        // words stay
        assert_eq!(k("ERROR: dfs.DataNode user@host --verbose"), "ERROR: dfs.DataNode user@host --verbose");
    }

    #[test]
    fn times() {
        for t in [
            "2026-11-18",
            "10:15:02",
            "1:02:03",
            "10:15:02.123",
            "2026-11-18T10:15:02.123Z",
            "2026-11-18T10:15:02+01:00",
            "2026-11-18T10:15:02-0800",
            "18/Nov/2026:10:00:00",
            "18-nov-2026",
            "2005-06-03-15.42.50.675872",
            "2005.06.03",
            "17/06/09",
            "11/18/2026",
            "1/5/2026",
            "18.11.2026",
            "2026/11/18",
        ] {
            assert!(is_time(t.as_bytes()), "{t} is a time");
            assert_eq!(k(t), "<time>", "{t}");
        }
        for t in ["081109", "03-17", "10.30", "2015-13", "10:15", "1.2.34", "123:45:67:", "20171223-22:15:29:606", "2026-11-18-backup"] {
            assert!(!is_time(t.as_bytes()), "{t} is not a time");
        }
        // with spaces: syslog and ctime, so keys do not change with the month or the weekday
        assert_eq!(k("Dec 10 06:55:46 LabSZ sshd[24200]: Invalid user webmaster from 173.234.31.186"), "<time> LabSZ sshd[<num>]: Invalid user webmaster from <ip>");
        assert_eq!(k("Jun  9 06:06:20 combo x"), "<time> combo x");
        assert_eq!(k("[Sun Dec 04 04:47:44 2005] [error] mod_jk child workerEnv in error state 6"), "[<time>] [error] mod_jk child workerEnv in error state <num>");
        assert_eq!(k("[Mon Dec 05 04:47:44 2005] [error] mod_jk child workerEnv in error state 7"), "[<time>] [error] mod_jk child workerEnv in error state <num>");
        assert_eq!(k("- 1131566461 2005.11.09 dn228 Nov 9 12:01:01 dn228/dn228 crond[2916]: (root) CMD"), "- <num> <time> <num> <time> <num> crond[<num>]: (root) CMD");
        // the zone and the joined forms
        assert_eq!(k("2015-07-29 17:41:44,747 - INFO"), "<time> <time>,<num> - INFO");
        assert_eq!(k("May the force"), "May the force");
        assert_eq!(k("Mon Nov 18 10:15:02 UTC 2026"), "<time> UTC <num>");
    }

    #[test]
    fn quotes_spaces_and_cleaning() {
        assert_eq!(k(r#"set name="a b \" c" and 'x' "#), "set name=<str> and 'x'");
        assert_eq!(k(r#"open "never closed 42"#), "open \"never closed <num>");
        assert_eq!(k("  a \t b   c  \r\n"), "a b c");
        assert_eq!(k("\x1b[31mERROR\x1b[0m disk 91% full\n"), "ERROR disk <num> full");
        assert_eq!(k("10%\r50%\rdone 100%\n"), "done <num>");
        assert_eq!(k("\u{feff}2026-11-18 boot"), "<time> boot");
        assert_eq!(k(""), "");
        assert_eq!(k("\n"), "");
        assert_eq!(k("é 1"), "é <num>");
        assert_eq!(kb(b"bad \xff\xfe 7"), b"bad \xff\xfe <num>"); // bytes that are not UTF-8 are letters
    }

    #[test]
    fn long_messages() {
        let long = "word ".repeat(20_000);
        let key = k(&long);
        assert!(key.ends_with(" <long>"), "{}", &key[key.len() - 20..]);
        assert!(key.len() < MESSAGE_MAX + 10);
        let exact = "g".repeat(MESSAGE_MAX);
        assert_eq!(k(&exact), exact);
        assert_eq!(k(&" ".repeat(MESSAGE_MAX + 1)), "<long>");
        // a JSON line over 1 MiB is keyed as text
        let big = format!("{{\"msg\":\"{}\"}}", "x".repeat(JSON_MAX));
        assert!(k(&big).ends_with("<long>"));
    }

    #[test]
    fn json_lines() {
        assert_eq!(k(r#"{"time":"12-17 19:31:36.263","pid":1795,"level":"I","tag":"PowerManager_screenOn","msg":"DisplayPowerStatesetColorFadeLevel: level=1.0"}"#), "{I} DisplayPowerStatesetColorFadeLevel: level=<num>");
        assert_eq!(k(r#"{"time":"Dec 10 06:55:46","host":"LabSZ","msg":"Invalid user webmaster from 173.234.31.186"}"#), "Invalid user webmaster from <ip>");
        assert_eq!(k(r#"{"level":30,"time":1700000000123,"msg":"listening on 8080"}"#), "{30} listening on <num>");
        assert_eq!(k(r#"{"log.level":"warn","message":"slow"}"#), "{warn} slow");
        assert_eq!(k(r#"{"log":{"level":"error"},"message":"boom"}"#), "{error} boom");
        assert_eq!(k(r#"{"msg":5,"message":"second wins","severity":"WARNING"}"#), "{WARNING} second wins");
        assert_eq!(k(r#"{"level":"INFO","msg":""}"#), "{INFO}");
        assert_eq!(k(r#"{"level":"info","msg":"line one\nline two\ttab"}"#), "{info} line one line two tab");
        // no message field: the field names, in order, repeats kept
        assert_eq!(k(r#"{"user":"a","n":2,"user":"b"}"#), "{user,n,user}");
        assert_eq!(k("{}"), "{}");
        // not JSON objects: text
        assert_eq!(k("{not json 42"), "{not json <num>");
        assert_eq!(k(r#" {"msg":"x"}"#), "{<str>:<str>}");
        assert_eq!(k(r#"{"msg":"x"} trailing"#), "{<str>:<str>} trailing");
        assert_eq!(k(r#"{"msg":"\ud800"}"#), "{<str>:<str>}"); // a lone surrogate is not JSON
    }

    #[test]
    fn docker_cri_and_sessions() {
        let d = |log: &str| format!(r#"{{"log":{log},"stream":"stderr","time":"2026-11-18T10:15:02.123456789Z"}}"#);
        assert_eq!(k(&d(r#""\u001b[33mWARN\u001b[0m cache miss for key user:42\n""#)), "WARN cache miss for key <num>");
        assert_eq!(k(&d(r#""{\"level\":\"error\",\"msg\":\"db connect failed\"}\n""#)), "{error} db connect failed");
        assert_eq!(k(&d(r#""10%\r100%\n""#)), "<num>");
        // a Docker line needs log to be a string
        assert_eq!(k(r#"{"log":7,"stream":"x","time":"t","msg":"m"}"#), "m");
        // CRI: the text after the prefix; JSON in it is read as JSON
        assert_eq!(k("2026-11-18T10:15:02.123456789Z stdout F Starting app version 1.4.2"), "Starting app version <num>");
        assert_eq!(k("2026-11-18T10:15:02+01:00 stderr P part"), "part");
        assert_eq!(k("2026-11-18T10:15:02Z stdout F"), "");
        assert_eq!(k(r#"2026-11-18T10:15:02Z stdout F {"level":"warn","msg":"slow"}"#), "{warn} slow");
        assert_eq!(k("2026-11-18T10:15:02Z stdout X text"), "<time> stdout X text"); // not CRI
        assert_eq!(k("2026-11-18T10:15:02.1234567890Z stdout F text"), "<time> stdout F text"); // 10 decimals: not CRI
        // agents' sessions
        let claude = r#"{"parentUuid":null,"sessionId":"s1","type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"Bash","input":{}}]},"timestamp":"2026-11-18T10:15:02.123Z"}"#;
        assert_eq!(k(claude), "{session assistant assistant text Bash}");
        assert_eq!(k(r#"{"sessionId":"s1","type":"user","message":{"role":"user","content":"why?"}}"#), "{session user user}");
        let codex = r#"{"timestamp":"2026-11-18T10:15:02.123Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#;
        assert_eq!(k(codex), "{session response_item function_call shell}");
        assert_eq!(k(r#"{"record_type":"state"}"#), "{session}");
    }

    #[test]
    fn keys_hold_no_control_bytes_from_text() {
        let mut s = 7u64;
        for _ in 0..2000 {
            let mut line = Vec::new();
            for _ in 0..(s % 60) {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                line.push((s >> 33) as u8);
            }
            let key = line_key(&line);
            if !key.starts_with(b"{") {
                assert!(!key.iter().any(|&b| b < 0x20 || b == 0x7f), "{line:?} -> {key:?}");
                assert!(!key.starts_with(b" ") && !key.ends_with(b" "));
            }
        }
    }

    #[test]
    fn counting_and_dumping() {
        let text = b"a 1\nb\r\na 2\n{\"x\":\"\\n\\\\\"}\nb";
        let c = count_lines(text);
        assert_eq!(c.len(), 3);
        assert_eq!((c.get(b"a <num>"), c.get(b"b"), c.get(b"{x}")), (2, 2, 1));
        assert_eq!(c.dump(), b"2\ta <num>\n2\tb\n1\t{x}\n");
        let mut pieces = count_lines(b"a 1\nb\r\n");
        pieces.add_all(&count_lines(b"a 2\n{\"x\":\"\\n\\\\\"}\nb"));
        assert_eq!(pieces.dump(), c.dump());
        let mut out = Vec::new();
        write_key(b"{a\nb\\c\x7f}", &mut out);
        assert_eq!(out, b"{a\\x0ab\\\\c\\x7f}");
        assert_eq!(line_keys(b"").count(), 0);
        assert_eq!(line_keys(b"\n").count(), 1);
    }
}
