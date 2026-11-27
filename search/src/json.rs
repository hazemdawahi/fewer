//! Our JSON (RFC 8259): one reader and one writer for MCP messages, runs.jsonl, agents'
//! session files and JSON log lines.
//!
//! - Numbers keep their text, so they round-trip exactly; they are parsed only when read
//!   (`int`, `num`).
//! - Objects keep their key order and duplicate keys; `get` takes the first.
//! - Strings are UTF-8. `\u` escapes take surrogate pairs; a lone surrogate is an error, as
//!   are raw control bytes inside a string and bytes that are not UTF-8.
//! - At most 64 levels of arrays and objects inside each other.
//! - The writer is compact and escapes '"', '\' and every byte below 0x20, so a written
//!   document never holds a raw line end (one MCP message per line).

use std::io::{BufRead, Write};

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// the number's text as written (checked against the grammar of RFC 8259)
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Most levels of arrays and objects inside each other.
pub const MAX_DEPTH: usize = 64;

/// One JSON value, then only whitespace.
pub fn parse(b: &[u8]) -> Result<Json, String> {
    let mut p = Parser { b, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != b.len() {
        return Err(p.err("more after the end of the value"));
    }
    Ok(v)
}

/// Whether parse failed because the value is nested too deep (MCP answers it as a parse
/// error like the others; kept apart for the tests).
#[cfg(test)]
pub fn too_deep(e: &str) -> bool {
    e.starts_with("nested deeper than")
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        format!("{what} at byte {}", self.i)
    }

    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn lit(&mut self, word: &[u8], v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("not JSON"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        let Some(&c) = self.b.get(self.i) else { return Err(self.err("the text ends where a value should be")) };
        match c {
            b'{' => {
                if depth >= MAX_DEPTH {
                    return Err(format!("nested deeper than {MAX_DEPTH} levels at byte {}", self.i));
                }
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return Err(self.err("expected a key in quotes"));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return Err(self.err("expected ':'"));
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    fields.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(fields));
                        }
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            b'[' => {
                if depth >= MAX_DEPTH {
                    return Err(format!("nested deeper than {MAX_DEPTH} levels at byte {}", self.i));
                }
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(items));
                        }
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => self.lit(b"true", Json::Bool(true)),
            b'f' => self.lit(b"false", Json::Bool(false)),
            b'n' => self.lit(b"null", Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("not JSON")),
        }
    }

    /// -? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?
    fn number(&mut self) -> Result<Json, String> {
        let s = self.i;
        let digits = |p: &mut Parser| {
            let a = p.i;
            while p.b.get(p.i).is_some_and(u8::is_ascii_digit) {
                p.i += 1;
            }
            p.i - a
        };
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.err("bad number")),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return Err(self.err("bad number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(self.err("bad number"));
            }
        }
        // (ASCII only: always UTF-8)
        Ok(Json::Num(String::from_utf8_lossy(&self.b[s..self.i]).into_owned()))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.b.get(self.i..self.i + 4).ok_or_else(|| self.err("bad \\u escape"))?;
        let mut v = 0u32;
        for &c in h {
            let d = (c as char).to_digit(16).ok_or_else(|| self.err("bad \\u escape"))?;
            v = v * 16 + d;
        }
        self.i += 4;
        Ok(v)
    }

    /// A string; self.i is at its opening quote.
    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            // a run of plain bytes, checked as UTF-8 at once
            let s = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > s {
                match std::str::from_utf8(&self.b[s..self.i]) {
                    Ok(t) => out.push_str(t),
                    Err(e) => {
                        self.i = s + e.valid_up_to();
                        return Err(self.err("not UTF-8"));
                    }
                }
            }
            let Some(&c) = self.b.get(self.i) else { return Err(self.err("the text ends inside a string")) };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else { return Err(self.err("the text ends inside a string")) };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // a high surrogate must be followed by a low one
                                if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return Err(self.err("a lone surrogate in a \\u escape"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("a lone surrogate in a \\u escape"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(self.err("a lone surrogate in a \\u escape"));
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).ok_or_else(|| self.err("bad \\u escape"))?);
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.err("bad escape in a string"));
                        }
                    }
                }
                _ => return Err(self.err("a control byte inside a string")),
            }
        }
    }
}

// ---------------------------------------------------------------- writing

/// The compact text of `v`, appended to `out`.
pub fn write(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(n),
        Json::Str(s) => write_str(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (k, x) in items.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                write(x, out);
            }
            out.push(']');
        }
        Json::Obj(fields) => {
            out.push('{');
            for (k, (key, x)) in fields.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                write_str(key, out);
                out.push(':');
                write(x, out);
            }
            out.push('}');
        }
    }
}

/// A string in quotes, with '"', '\' and every byte below 0x20 escaped (UTF-8 kept as is).
pub fn write_str(s: &str, out: &mut String) {
    out.push('"');
    let b = s.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        let esc: Option<&str> = match c {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            0x08 => Some("\\b"),
            0x0c => Some("\\f"),
            0..=0x1f => None,
            _ => continue,
        };
        out.push_str(&s[start..i]);
        match esc {
            Some(e) => out.push_str(e),
            None => out.push_str(&format!("\\u{:04x}", c)),
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        write(self, &mut s);
        f.write_str(&s)
    }
}

// ---------------------------------------------------------------- reading values

impl Json {
    /// The first field named `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(f) => f.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    /// A whole number: written as one (within i64), or a number with no fraction left
    /// (1.0, 3e2) within ±2^53.
    pub fn int(&self) -> Option<i64> {
        let Json::Num(t) = self else { return None };
        if let Ok(v) = t.parse::<i64>() {
            return Some(v);
        }
        let f: f64 = t.parse().ok()?;
        (f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0).then_some(f as i64)
    }
    #[allow(dead_code)]
    pub fn num(&self) -> Option<f64> {
        match self {
            Json::Num(t) => t.parse().ok(),
            _ => None,
        }
    }
    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
    #[allow(dead_code)]
    pub fn obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(f) => Some(f),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// An object from (key, value) pairs.
    pub fn o(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    /// Set a field of an object: the first one named `key`, else a new one at the end.
    pub fn set(&mut self, key: &str, v: Json) {
        if let Json::Obj(f) = self {
            match f.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = v,
                None => f.push((key.to_string(), v)),
            }
        }
    }
    /// Remove every field named `key` of an object.
    pub fn remove(&mut self, key: &str) {
        if let Json::Obj(f) = self {
            f.retain(|(k, _)| k != key);
        }
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}
impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}
impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}
impl From<i64> for Json {
    fn from(v: i64) -> Json {
        Json::Num(v.to_string())
    }
}
impl From<u64> for Json {
    fn from(v: u64) -> Json {
        Json::Num(v.to_string())
    }
}
impl From<usize> for Json {
    fn from(v: usize) -> Json {
        Json::Num(v.to_string())
    }
}
impl From<i32> for Json {
    fn from(v: i32) -> Json {
        Json::Num(v.to_string())
    }
}

/// `fewer json-check` (checks/json_check.py): each line of stdin is a document in
/// hex; each answer line is "OK " + the document written back compact, or "ERROR " + why.
pub fn cmd_check() -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().split(b'\n') {
        let Ok(line) = line else { return 1 };
        let hex = line.strip_suffix(b"\r").unwrap_or(&line);
        let mut doc = Vec::with_capacity(hex.len() / 2);
        let mut ok = hex.len() % 2 == 0;
        for p in hex.chunks(2) {
            match std::str::from_utf8(p).ok().and_then(|s| u8::from_str_radix(s, 16).ok()) {
                Some(v) => doc.push(v),
                None => ok = false,
            }
        }
        let text = if !ok {
            "ERROR bad hex".to_string()
        } else {
            match parse(&doc) {
                Ok(v) => format!("OK {v}"),
                Err(e) => format!("ERROR {e}"),
            }
        };
        if writeln!(out, "{text}").is_err() {
            return 1;
        }
    }
    if out.flush().is_err() { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Json {
        parse(s.as_bytes()).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }
    fn bad(b: &[u8]) {
        assert!(parse(b).is_err(), "accepted {:?}", String::from_utf8_lossy(b));
    }

    /// Valid documents (our own list, in the spirit of JSONTestSuite's y_ cases), each
    /// written back to the given compact text.
    #[test]
    fn valid_documents() {
        let cases: &[(&str, &str)] = &[
            ("null", "null"),
            (" true ", "true"),
            ("false", "false"),
            ("0", "0"),
            ("-0", "-0"),
            ("-0.0e-0", "-0.0e-0"),
            ("123456789012345678901234567890", "123456789012345678901234567890"),
            ("1E+2", "1E+2"),
            ("1.5e300", "1.5e300"),
            ("\"\"", "\"\""),
            ("\"a\\\"b\\\\c\\/d\"", "\"a\\\"b\\\\c/d\""),
            ("\"\\b\\f\\n\\r\\t\"", "\"\\b\\f\\n\\r\\t\""),
            ("\"\\u0000\\u001f\"", "\"\\u0000\\u001f\""),
            ("\"\\u00e9\\u20ac\\ud83d\\ude00\"", "\"é€😀\""),
            ("\"é€😀\"", "\"é€😀\""),
            ("[]", "[]"),
            ("{}", "{}"),
            ("[ 1 , [ 2 , [ ] ] , { } ]", "[1,[2,[]],{}]"),
            ("{\"a\":1,\"a\":2,\"b\":{\"c\":[null]}}", "{\"a\":1,\"a\":2,\"b\":{\"c\":[null]}}"),
            ("\t\r\n {\"k\" : \"v\"}\n", "{\"k\":\"v\"}"),
            ("\"\u{7f}\u{2028}\"", "\"\u{7f}\u{2028}\""),
        ];
        for (src, want) in cases {
            let v = ok(src);
            assert_eq!(v.to_string(), *want, "{src:?}");
            // written text parses back to the same value
            assert_eq!(parse(v.to_string().as_bytes()).unwrap(), v);
        }
        let v = ok("{\"a\":1,\"a\":2}");
        assert_eq!(v.get("a").and_then(Json::int), Some(1));
        assert_eq!(ok("[1.0]").arr().unwrap()[0].int(), Some(1));
        assert_eq!(ok("3e2").int(), Some(300));
        assert_eq!(ok("1.5").int(), None);
        assert_eq!(ok("-9223372036854775808").int(), Some(i64::MIN));
        assert_eq!(ok("1e400").int(), None);
        // 64 levels are fine
        let deep = "[".repeat(64) + &"]".repeat(64);
        ok(&deep);
    }

    /// Invalid documents (JSONTestSuite's n_ cases in spirit).
    #[test]
    fn invalid_documents() {
        let cases: &[&[u8]] = &[
            b"",
            b" ",
            b"nul",
            b"True",
            b"NaN",
            b"Infinity",
            b"-",
            b"01",
            b"1.",
            b".5",
            b"1e",
            b"1e+",
            b"+1",
            b"0x10",
            b"\"abc",
            b"\"\\x41\"",
            b"\"\\u12\"",
            b"\"\\uD800\"",
            b"\"\\uDC00\"",
            b"\"\\uD800\\u0041\"",
            b"\"a\tb\"",
            b"\"a\nb\"",
            b"\"\xff\"",
            b"\"\xc3\"",
            b"\"\xed\xa0\x80\"",
            b"[1,]",
            b"[,1]",
            b"[1 2]",
            b"{\"a\" 1}",
            b"{a:1}",
            b"{\"a\":1,}",
            b"{'a':1}",
            b"[1]]",
            b"[1] x",
            b"[",
            b"{",
            b"\xef\xbb\xbf{}",
            b"[1]\x00",
            b"/* c */ 1",
        ];
        for c in cases {
            bad(c);
        }
        let deep = "[".repeat(65) + &"]".repeat(65);
        let e = parse(deep.as_bytes()).unwrap_err();
        assert!(too_deep(&e), "{e}");
        let deep_obj = "{\"a\":".repeat(65) + "1" + &"}".repeat(65);
        assert!(too_deep(&parse(deep_obj.as_bytes()).unwrap_err()));
    }

    /// Random strings of every kind of byte and character round-trip through write and
    /// parse; written text never holds a raw control byte.
    #[test]
    fn strings_round_trip() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for _ in 0..20_000 {
            let n = rnd(20);
            let s: String = (0..n)
                .map(|_| match rnd(6) {
                    0 => char::from_u32(rnd(0x20) as u32).unwrap(),
                    1 => ['"', '\\', '/', '\u{7f}', '\u{2028}'][rnd(5) as usize],
                    2 => char::from_u32(0x80 + rnd(0x700) as u32).unwrap(),
                    3 => char::from_u32(0x10000 + rnd(0x1000) as u32).unwrap(),
                    _ => (b' ' + rnd(95) as u8) as char,
                })
                .collect();
            let v = Json::Arr(vec![Json::Str(s.clone()), Json::o(vec![(s.as_str(), Json::Null)])]);
            let t = v.to_string();
            assert!(!t.bytes().any(|c| c < 0x20), "{t:?}");
            assert_eq!(parse(t.as_bytes()).unwrap(), v);
        }
    }

    #[test]
    fn set_and_remove() {
        let mut v = ok("{\"a\":1,\"b\":2}");
        v.set("a", Json::from(5i64));
        v.set("c", Json::from("x"));
        v.remove("b");
        assert_eq!(v.to_string(), "{\"a\":5,\"c\":\"x\"}");
    }
}
