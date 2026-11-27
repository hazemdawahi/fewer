//! `fewer table FILE...` (2026-11-24): a table's facts in one call.
//!
//! A question about a CSV file ("the total amount for each region, and which product sold the most
//! units") took an agent two programs when its first one guessed a column's name ("amount" for
//! amount_cents): a step more, re-read by every later step. `table` reads the file once and prints
//! every column (its name and kind), each number column's sum, smallest and largest (exact: sums
//! of decimals are kept as whole numbers of their smallest unit, never rounded), each text column's
//! values with their counts (20 values at most, else the most common), and the sums of every
//! number column by every text column of 2 to 20 values, the largest marked: such questions are
//! answered from it. Comma-, tab-, semicolon- or |-separated, by the header (Excel's `sep=X` first
//! line read); quoted fields as in RFC 4180; rows with another number of fields are counted and
//! named. Secrets are hidden.
//!
//! A log of key=value words is a table too (a log question, "how many 5xx per endpoint", took an
//! agent 3 to 4 steps): each line a row, each key a column, the plain words every line has
//! columns by their place, named by what they hold (time, level, method, path, else wordN). A
//! column of 2 to 20 values that repeat is a category (a number column too, such as a status:
//! its values counted); for each pair of categories the counts of one by the other are given
//! ("counts of status by path: /api/pay: 200 3801, 500 12; ...").

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

/// A text column of this many values or fewer is listed whole, and sums are given by it.
const LIST_VALUES: usize = 20;
/// Different values kept per column (past it, a column is "many values").
const TRACK_VALUES: usize = 10_000;
/// The most common values shown of a column of many.
const TOP: usize = 5;
/// A column is a number column when its filled cells are numbers but for this share at most.
const NOT_NUMBERS_PER_10: u64 = 1;
/// Files larger than this are not read.
const MAX_BYTES: u64 = 1 << 30;
/// Counts of one category by another: pairs shown at most; characters of one such line at most.
const MAX_PAIRS: usize = 15;
const PAIR_CHARS: usize = 800;

const HELP: &str = "usage: fewer table FILE...

A table's facts in one call (CSV, TSV, ;- or |-separated, by its header): rows and columns, each
column's kind; for a number column its sum (exact), smallest and largest; for a text column its
values with their counts (20 at most, else the most common); and the sum of every number column
by every column of 2 to 20 values that repeat (a category), the largest marked (most), and for
each pair of categories the counts of one by the other. A log of key=value words is read as a
table too: each key a column, and the plain words every line has, by their place (time, level,
method, path, ...). Rows with another number of fields are counted and named. Secrets are
hidden.";

pub fn cmd_table(args: Vec<OsString>) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return if args.is_empty() { 2 } else { 0 };
    }
    let mut out = String::new();
    let mut ok = true;
    // (one numbering of hidden secrets for the whole answer: the same secret the same number in
    // every file)
    let mut numbers: HashMap<String, usize> = HashMap::new();
    // (Claude Code's Read rules, which never see a file read through Bash: perms.rs)
    let cwd = std::env::current_dir().unwrap_or_default();
    let rules = crate::perms::Rules::load(&cwd);
    for f in &args {
        let p = Path::new(f);
        if let Some(line) = crate::perms::refused_read(&shown(p), &rules.read(p, &cwd)) {
            out.push_str(&format!("== {line}\n"));
            ok = false;
            continue;
        }
        match summarize(p, &mut numbers) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                out.push_str(&format!("== {}: {e}\n", shown(p)));
                ok = false;
            }
        }
    }
    // (secrets are hidden in the cells, before anything is counted: summarize)
    print!("{out}");
    if ok { 0 } else { 1 }
}

/// An exact decimal: `int` / 10^`scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dec {
    int: i128,
    scale: u32,
}

impl Dec {
    fn parse(s: &str) -> Option<Dec> {
        let t = s.trim();
        let (neg, body) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
        if (ip.is_empty() && fp.is_empty()) || ip.len() + fp.len() > 30 || !ip.bytes().chain(fp.bytes()).all(|c| c.is_ascii_digit()) {
            return None;
        }
        let digits = format!("{ip}{fp}");
        let v: i128 = digits.parse().ok()?;
        Some(Dec { int: if neg { -v } else { v }, scale: fp.len() as u32 })
    }

    /// The same value at a larger scale.
    fn at(self, scale: u32) -> Option<i128> {
        self.int.checked_mul(10i128.checked_pow(scale.checked_sub(self.scale)?)?)
    }

    fn text(self) -> String {
        let neg = self.int < 0;
        let digits = self.int.unsigned_abs().to_string();
        let s = if self.scale == 0 {
            digits
        } else {
            let d = format!("{:0>w$}", digits, w = self.scale as usize + 1);
            let (a, b) = d.split_at(d.len() - self.scale as usize);
            format!("{a}.{b}")
        };
        if neg { format!("-{s}") } else { s }
    }
}

/// A running exact sum: whole numbers of the smallest unit seen so far (None: too large).
#[derive(Clone, Copy, Debug)]
struct Sum {
    int: Option<i128>,
    scale: u32,
}

impl Sum {
    fn new() -> Sum {
        Sum { int: Some(0), scale: 0 }
    }

    fn add(&mut self, d: Dec) {
        if d.scale > self.scale {
            self.int = self.int.and_then(|v| v.checked_mul(10i128.checked_pow(d.scale - self.scale)?));
            self.scale = d.scale;
        }
        self.int = self.int.and_then(|v| v.checked_add(d.at(self.scale)?));
    }

    fn text(&self) -> String {
        match self.int {
            Some(v) => Dec { int: v, scale: self.scale }.text(),
            None => "too large to add up exactly".into(),
        }
    }
}

struct Col {
    name: String,
    filled: u64,
    empty: u64,
    numbers: u64,
    /// cells that are not numbers: how many, and the first few (row, value)
    not_numbers: Vec<(u64, String)>,
    not_number_count: u64,
    sum: Sum,
    min: Option<Dec>,
    max: Option<Dec>,
    whole: bool,
    values: HashMap<String, u64>,
    many: bool,
    /// the first and last values in the file's order
    first: Option<String>,
    last: Option<String>,
    /// a whole number written with leading zeros (000123): an id, not an amount
    padded: bool,
}

fn less(a: Dec, b: Dec) -> bool {
    let s = a.scale.max(b.scale);
    match (a.at(s), b.at(s)) {
        (Some(x), Some(y)) => x < y,
        _ => (a.int as f64) / 10f64.powi(a.scale as i32) < (b.int as f64) / 10f64.powi(b.scale as i32),
    }
}

/// The fields of each record of `text` (quotes as in RFC 4180), each given to `each`.
fn records(text: &str, sep: char, mut each: impl FnMut(Vec<String>)) {
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut at_start = true;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                at_start = false;
            }
            c if c == sep => {
                fields.push(std::mem::take(&mut cur));
                at_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                fields.push(std::mem::take(&mut cur));
                each(std::mem::take(&mut fields));
                at_start = true;
            }
            c => {
                cur.push(c);
                at_start = false;
            }
        }
    }
    if !cur.is_empty() || !fields.is_empty() {
        fields.push(cur);
        each(fields);
    }
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// A table read: its columns' names and its rows (cells in the columns' order), what kind of file
/// it is, and the records with another number of fields.
struct Table {
    names: Vec<String>,
    rows: Vec<Vec<String>>,
    kind: String,
    odd: Vec<u64>,
    odd_count: u64,
    /// the columns named as secrets (password, api_token, ...), whose cells are hidden, and the
    /// kind each one's cells had
    secret: Vec<bool>,
    secret_kind: Vec<Option<String>>,
    /// rows with another number of fields in a table with a column named as a secret: hidden
    /// whole, left out of the facts (their fields may have moved past the secret's column)
    hidden_rows: u64,
    /// a .csv or .tsv whose header no separator splits: one column
    no_sep: bool,
}

/// Secrets hidden in the cells before anything is counted (the text was hidden line by line
/// already, as `read` hides it: summarize): a column named as a secret (as `password=`,
/// `api_token:` or `Authorization: Bearer` name one, in any case, its spaces as '_': "API Key")
/// has every filled cell hidden; in a table with such a column, a row with another number of
/// fields has every cell hidden (its fields may have moved into the next column); any other cell
/// loses the shapes found in it, and a cell that is a Bearer or Basic credential is hidden whole;
/// the columns' names too. The finished lines are not scanned again: in "api_token: text" the
/// word after the colon is the column's kind, not its value.
fn hide_secrets(t: &mut Table, extras: &[crate::secrets::Extra], numbers: &mut HashMap<String, usize>) {
    let one = t.names.len() == 1;
    let markers: Vec<Option<String>> = t.names.iter().map(|n| secret_name(n, one)).collect();
    let any = markers.iter().any(Option::is_some);
    let n = t.names.len();
    // (a column named as a secret keeps its kind, from its cells as read: max_token's 4096 is
    // still a whole number, only not shown)
    t.secret_kind = markers.iter().enumerate().map(|(k, m)| m.as_ref().map(|_| kind_of_cells(t.rows.iter().filter_map(|r| r.get(k)).map(String::as_str)))).collect();
    let mut hidden_in = vec![false; n];
    // (a row whose fields may have moved past a secret's column: hidden whole and left out, so
    // that the columns keep their kinds and sums over the other rows)
    if any {
        let before = t.rows.len();
        t.rows.retain(|r| r.len() == n || (r.len() == 1 && r[0].trim().is_empty()));
        t.hidden_rows = (before - t.rows.len()) as u64;
    }
    // (different secrets stay different: each secret hidden gets its own number in the answer,
    // by its value, so that counts and sums by it stay right, "[hidden: AWS key 2]")
    for row in &mut t.rows {
        for (k, cell) in row.iter_mut().enumerate() {
            if cell.trim().is_empty() {
                continue;
            }
            *cell = match markers.get(k) {
                // (true, false and null are no secret, as SECRETS.md's JSON rule keeps them)
                Some(Some(_)) if is_flag(cell) => cell.clone(),
                Some(Some(m)) => {
                    hidden_in[k] = true;
                    m.clone()
                }
                _ => {
                    let h = hide_cell(cell, extras);
                    if h == *cell { h } else { numbered(cell, &h, numbers) }
                }
            };
        }
    }
    t.secret = markers.iter().zip(&hidden_in).map(|(m, h)| m.is_some() && *h).collect();
    for n in &mut t.names {
        *n = crate::secrets::hide_str(n, extras);
        if n.chars().any(char::is_control) {
            *n = format!("{n:?}");
        }
    }
}

/// true, false or null (in any case): no secret, kept in a column named as one.
fn is_flag(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "false" | "null")
}

/// The kind of a column's cells as read: "whole numbers", "numbers" or "text".
fn kind_of_cells<'a>(cells: impl Iterator<Item = &'a str>) -> String {
    let mut whole = true;
    let mut any = false;
    for c in cells.map(str::trim).filter(|c| !c.is_empty()) {
        match Dec::parse(c) {
            Some(d) => {
                any = true;
                whole &= d.scale == 0;
            }
            None => return "text".into(),
        }
    }
    if !any { "text".into() } else if whole { "whole numbers".into() } else { "numbers".into() }
}

/// Each marker of a hidden cell numbered by the secret it stands for: "[hidden: AWS key]" as
/// "[hidden: AWS key 2]", the same secret the same number wherever it is in the answer.
fn numbered(cell: &str, hidden: &str, numbers: &mut HashMap<String, usize>) -> String {
    let originals = secrets_in(cell, hidden);
    let mut out = String::with_capacity(hidden.len() + 4);
    let mut rest = hidden;
    let mut k = 0;
    while let Some(i) = rest.find("[hidden: ") {
        let Some(j) = rest[i..].find(']') else { break };
        out.push_str(&rest[..i + j]);
        let key = originals.get(k).cloned().unwrap_or_else(|| cell.to_string());
        let next = numbers.len() + 1;
        let id = *numbers.entry(key).or_insert(next);
        out.push_str(&format!(" {id}]"));
        rest = &rest[i + j + 1..];
        k += 1;
    }
    out.push_str(rest);
    out
}

/// The text of `orig` that each marker of `hidden` stands for, in order (the text between the
/// markers is the same in both); markers with nothing between them stand for one text together;
/// when the two cannot be matched, the whole text for each.
fn secrets_in(orig: &str, hidden: &str) -> Vec<String> {
    let mut lits: Vec<&str> = Vec::new();
    let mut rest = hidden;
    while let Some((i, e)) = rest.find("[hidden: ").and_then(|i| rest[i..].find(']').map(|j| (i, i + j + 1))) {
        lits.push(&rest[..i]);
        rest = &rest[e..];
    }
    lits.push(rest);
    let markers = lits.len() - 1;
    let whole = || vec![orig.to_string(); markers];
    if !orig.starts_with(lits[0]) {
        return whole();
    }
    let mut pos = lits[0].len();
    let mut out = Vec::with_capacity(markers);
    let mut run = 0;
    for m in 0..markers {
        run += 1;
        let lit = lits[m + 1];
        if lit.is_empty() && m + 1 < markers {
            continue;
        }
        let end = if m + 1 == markers {
            if !orig.ends_with(lit) || orig.len() < pos + lit.len() {
                return whole();
            }
            orig.len() - lit.len()
        } else {
            match orig[pos..].find(lit) {
                Some(i) => pos + i,
                None => return whole(),
            }
        };
        out.extend(std::iter::repeat_n(orig[pos..end].to_string(), run));
        run = 0;
        pos = end + lit.len();
    }
    out
}

/// The marker of a column named as a secret's key, else None: the name as a key (its spaces as
/// '_'), and each part of it a separator would split (a header read as one name when the file's
/// separator was not found: "user,password,region"; `one`, the only column: also at its runs of
/// spaces, "user  password  note"), each tried as `NAME=x` and as `NAME: Bearer x`.
fn secret_name(name: &str, one: bool) -> Option<String> {
    let mut keys: Vec<String> = vec![name.trim().replace([' ', '\t'], "_")];
    keys.extend(name.split([',', ';', '|', '\t']).map(|p| p.trim().trim_matches('"').replace(' ', "_")).filter(|p| !p.is_empty()));
    if one {
        keys.extend(name.split_whitespace().map(|p| p.trim_matches('"').to_string()));
    }
    for key in keys {
        for probe in [format!("{key}=x"), format!("{key}: Bearer x")] {
            let (h, c) = crate::secrets::hide(probe.as_bytes(), &[]);
            if c.total() > 0 {
                let h = String::from_utf8_lossy(&h).into_owned();
                let m = h.find("[hidden: ").and_then(|i| h[i..].find(']').map(|j| h[i..=i + j].to_string()));
                return Some(m.unwrap_or_else(|| "[hidden: secret]".to_string()));
            }
        }
    }
    None
}

/// A cell with its secrets hidden: the shapes found in it, and a Bearer or Basic credential whole
/// (an Authorization header's value, in a column of any name).
fn hide_cell(cell: &str, extras: &[crate::secrets::Extra]) -> String {
    let t = cell.trim_start();
    let lower = t.get(..6).map(str::to_ascii_lowercase).unwrap_or_default();
    if lower.starts_with("bearer") || lower.starts_with("basic ") {
        let (h, c) = crate::secrets::hide(format!("Authorization: {t}").as_bytes(), extras);
        if c.total() > 0 {
            return String::from_utf8_lossy(&h)["Authorization: ".len()..].to_string();
        }
    }
    crate::secrets::hide_str(cell, extras)
}

/// A file's name as the answer shows it (with its secrets hidden, as a cell's are).
fn shown(p: &Path) -> String {
    crate::secrets::hide_str(&p.display().to_string(), &crate::secrets::store_extras())
}

/// A value as the lists show it: in quotes when it could be read as more than one (", " or "; "
/// inside, a space at an end) or holds a control character (written escaped).
fn shown_value(v: &str) -> String {
    if v.contains(", ") || v.contains("; ") || v.starts_with(' ') || v.ends_with(' ') || v.chars().any(char::is_control) {
        format!("{v:?}")
    } else {
        v.to_string()
    }
}

/// A log line's words: split at spaces, but a quoted value (msg="two words") and a marker of a
/// hidden secret ("[hidden: password]") stay one word.
fn words(line: &str) -> Vec<String> {
    // (a quote after an odd number of backslashes is part of the value: msg="user \"ann\" in")
    let closes = |w: &str| w.ends_with('"') && w[..w.len() - 1].bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 0;
    let mut out: Vec<String> = Vec::new();
    let mut open: Option<char> = None;
    for w in line.split_ascii_whitespace() {
        if open.is_some() {
            let last = out.last_mut().expect("a word is open");
            last.push(' ');
            last.push_str(w);
            if (open == Some('"') && closes(w)) || (open == Some(']') && w.contains(']')) {
                open = None;
            }
            continue;
        }
        out.push(w.to_string());
        let quoted = w.split_once("=\"").is_some_and(|(_, v)| v.is_empty() || !closes(v));
        if quoted {
            open = Some('"');
        } else if w.contains("[hidden:") && !w.contains(']') {
            open = Some(']');
        }
    }
    out
}

/// A table of `text` (from its header on; `base`: the header's record number, 2 after Excel's
/// `sep=` line).
fn csv_table(text: &str, sep: char, sep_name: &str, base: u64) -> Table {
    let mut names: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let (mut odd, mut odd_count) = (Vec::new(), 0u64);
    records(text, sep, |fields| {
        let Some(n) = &names else {
            names = Some(fields.iter().map(|f| f.trim().to_string()).collect());
            return;
        };
        if fields.len() == 1 && fields[0].trim().is_empty() {
            return;
        }
        if fields.len() != n.len() {
            odd_count += 1;
            if odd.len() < 5 {
                odd.push(rows.len() as u64 + base + 1);
            }
        }
        rows.push(fields);
    });
    Table { names: names.unwrap_or_default(), rows, kind: sep_name.to_string(), odd, odd_count, secret: Vec::new(), secret_kind: Vec::new(), hidden_rows: 0, no_sep: false }
}

/// A `key=value` word: (key, value).
fn key_value(t: &str) -> Option<(&str, &str)> {
    let (k, v) = t.split_once('=')?;
    let first = k.chars().next()?;
    ((first.is_ascii_alphabetic() || first == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))).then_some((k, v))
}

/// A log of `key=value` words (or a .log file): read as a table, not as a CSV.
fn looks_like_log(p: &Path, text: &str) -> bool {
    let first: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(100).collect();
    if first.is_empty() || first[0].contains('\t') || (first[0].contains(',') && !p.extension().is_some_and(|e| e == "log")) {
        return false;
    }
    let kv = first.iter().filter(|l| words(l).iter().any(|t| key_value(t).is_some())).count();
    p.extension().is_some_and(|e| e == "log") || kv * 10 >= first.len() * 8
}

/// A name for a column of plain words, by what its values look like.
fn word_name(values: &[&str], k: usize) -> String {
    let all = |f: &dyn Fn(&str) -> bool| !values.is_empty() && values.iter().all(|v| f(v));
    const LEVELS: [&str; 11] = ["TRACE", "DEBUG", "INFO", "NOTICE", "WARN", "WARNING", "ERROR", "ERR", "FATAL", "CRITICAL", "CRIT"];
    const METHODS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
    let bare = |v: &str| v.trim_matches(|c| c == '[' || c == ']' || c == ':').to_ascii_uppercase();
    if all(&|v| v.len() >= 8 && v.starts_with(|c: char| c.is_ascii_digit()) && (v.contains(':') || v.contains('-'))) {
        "time".into()
    } else if all(&|v| LEVELS.contains(&bare(v).as_str())) {
        "level".into()
    } else if all(&|v| METHODS.contains(&v)) {
        "method".into()
    } else if all(&|v| v.starts_with('/')) {
        "path".into()
    } else {
        format!("word{k}")
    }
}

/// A log read as a table: the plain words every line has (by place: time, level, method, path,
/// word3 ...; a message's words past them left out) and the keys of its key=value words. The
/// plain words come from the line with its secrets hidden (a key and its value as words,
/// "password: x", "Authorization: Bearer x"); the key=value words' values as written, hidden
/// cell by cell after (hide_secrets: a column named as a secret keeps its kind and its flags,
/// different secrets get different numbers).
fn log_table(text: &str, extras: &[crate::secrets::Extra]) -> Table {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let plain: Vec<Vec<String>> = lines.iter().map(|l| words(&crate::secrets::hide_str(l, extras)).into_iter().filter(|t| key_value(t).is_none()).collect()).collect();
    let places = plain.iter().map(Vec::len).min().unwrap_or(0).min(8);
    let mut keys: Vec<String> = Vec::new();
    for l in &lines {
        for t in &words(l) {
            if let Some((k, _)) = key_value(t)
                && keys.len() < 40
                && !keys.iter().any(|x| x == k)
            {
                keys.push(k.to_string());
            }
        }
    }
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(lines.len());
    for (l, plain) in lines.iter().zip(&plain) {
        let mut row: Vec<String> = plain.iter().take(places).cloned().collect();
        let ws = words(l);
        let kv: Vec<(&str, &str)> = ws.iter().filter_map(|t| key_value(t)).collect();
        for k in &keys {
            // (a quoted value counted without its quotes, its \" and \\ as written)
            row.push(kv.iter().rev().find(|(x, _)| x == k).map(|(_, v)| unquoted(v)).unwrap_or_default());
        }
        rows.push(row);
    }
    let mut names: Vec<String> = Vec::new();
    for k in 0..places {
        let sample: Vec<&str> = rows.iter().take(200).map(|r| r[k].as_str()).collect();
        let mut n = word_name(&sample, k + 1);
        if names.contains(&n) || keys.contains(&n) {
            n = format!("word{}", k + 1);
        }
        names.push(n);
    }
    names.extend(keys);
    Table { names, rows, kind: "a log: its words by place and its key=value words".into(), odd: Vec::new(), odd_count: 0, secret: Vec::new(), secret_kind: Vec::new(), hidden_rows: 0, no_sep: false }
}

/// A key=value word's value without its quotes ("two words" as two words), \" and \\ inside read
/// as " and \.
fn unquoted(v: &str) -> String {
    let Some(inner) = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else { return v.to_string() };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(n @ ('"' | '\\'))) => {
                out.push(n);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The name of a separator, as the answer says it.
fn sep_name(c: char) -> String {
    match c {
        ',' => "comma-separated".into(),
        '\t' => "tab-separated".into(),
        ';' => "semicolon-separated".into(),
        c => format!("separated by {c:?}"),
    }
}

/// A table's separator, by its header: the one that splits it most (outside quotes), the file's
/// ending only breaking a tie (.tsv: a tab; else a comma). Excel's `sep=X` as the first line
/// names it (that line is not the header). (the separator, the text from the header on, the
/// header's record number, whether a separator was found)
fn separator<'a>(p: &Path, text: &'a str) -> (char, &'a str, u64, bool) {
    let first = text.lines().next().unwrap_or("").trim_end_matches('\r');
    if first.len() >= 5 && first.is_char_boundary(4) && first[..4].eq_ignore_ascii_case("sep=") && first[4..].chars().count() == 1 {
        let c = first[4..].chars().next().unwrap_or(',');
        return (c, text.split_once('\n').map_or("", |(_, r)| r), 2, true);
    }
    let tsv = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    let count = |c: char| {
        let mut quoted = false;
        first.chars().filter(|&x| {
            if x == '"' {
                quoted = !quoted;
            }
            x == c && !quoted
        }).count()
    };
    let order: [char; 4] = if tsv { ['\t', ',', ';', '|'] } else { [',', '\t', ';', '|'] };
    let best = order.iter().copied().max_by_key(|&c| (count(c), std::cmp::Reverse(order.iter().position(|&x| x == c))));
    match best.filter(|&c| count(c) > 0) {
        Some(c) => (c, text, 1, true),
        None => (order[0], text, 1, false),
    }
}

/// Each record's fields as byte ranges of `text` (a quoted field's range holds its quotes), as
/// `records` reads them; `sep` is one byte.
fn record_spans(text: &str, sep: u8) -> Vec<Vec<(usize, usize)>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut fields = Vec::new();
    let (mut start, mut i) = (0, 0);
    let (mut quoted, mut at_start) = (false, true);
    while i < b.len() {
        let c = b[i];
        if quoted {
            if c == b'"' {
                if b.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                quoted = false;
            }
        } else if c == b'"' && at_start {
            quoted = true;
            at_start = false;
        } else if c == sep {
            fields.push((start, i));
            start = i + 1;
            at_start = true;
        } else if c == b'\n' {
            fields.push((start, if i > start && b[i - 1] == b'\r' { i - 1 } else { i }));
            out.push(std::mem::take(&mut fields));
            start = i + 1;
            at_start = true;
        } else {
            at_start = false;
        }
        i += 1;
    }
    if start < b.len() || !fields.is_empty() {
        fields.push((start, b.len()));
        out.push(fields);
    }
    out
}

/// A .csv or .tsv file's cells under a column named as a secret hidden, as `table` hides them
/// (true, false and null kept; in such a table a row with another number of fields has each of
/// its cells hidden, as its fields may have moved), every other byte and every line kept: for
/// `read`, which shows files whole. The text and how many cells were hidden; None when the file
/// is not such a table or has no such column.
pub fn hide_columns(p: &Path, text: &str) -> Option<(String, u64)> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "csv" && ext != "tsv" {
        return None;
    }
    let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    let (sep, body, _, _) = separator(p, &text[bom..]);
    if !sep.is_ascii() {
        return None;
    }
    let offset = text.len() - body.len();
    let recs = record_spans(body, sep as u8);
    let header = recs.first()?;
    let names: Vec<&str> = header.iter().map(|&(s, e)| body[s..e].trim().trim_matches('"')).collect();
    let one = names.len() == 1;
    let markers: Vec<Option<String>> = names.iter().map(|n| secret_name(n, one)).collect();
    if !markers.iter().any(Option::is_some) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..offset]);
    let (mut last, mut count) = (0, 0u64);
    for rec in &recs[1..] {
        let moved = rec.len() != names.len();
        for (k, &(s, e)) in rec.iter().enumerate() {
            let cell = &body[s..e];
            let v = cell.trim().trim_matches('"').trim();
            if v.is_empty() || is_flag(v) {
                continue;
            }
            let Some(m) = (if moved { Some("[hidden: secret]".to_string()) } else { markers.get(k).cloned().flatten() }) else { continue };
            out.push_str(&body[last..s]);
            out.push_str(&m);
            // (a quoted cell's line ends kept, so that every line stays the file's)
            out.extend(std::iter::repeat_n('\n', cell.matches('\n').count()));
            last = e;
            count += 1;
        }
    }
    out.push_str(&body[last..]);
    Some((out, count))
}

/// The summary of one table file.
fn summarize(p: &Path, numbers: &mut HashMap<String, usize>) -> Result<String, String> {
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("a folder: give the files in it".into());
    }
    if meta.len() > MAX_BYTES {
        return Err(format!("{} bytes: larger than fewer reads as a table", meta.len()));
    }
    let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
    let extras = crate::secrets::store_extras();
    // a text holding a private key's block: its secrets hidden line by line first, as `read`
    // hides them (a key's lines, each a row of its own), before it is cut into cells; else the
    // cells are hidden one by one after (hide_secrets), so that different secrets stay different
    // and a column named as a secret keeps its kind (a log's plain words: log_table)
    let bytes = if bytes.windows(10).any(|w| w == b"-----BEGIN") { crate::secrets::hide(&bytes, &extras).0 } else { bytes };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut t = if looks_like_log(p, text) {
        log_table(text, &extras)
    } else {
        let (sep, body, base, found) = separator(p, text);
        let mut t = csv_table(body, sep, &sep_name(sep), base);
        t.no_sep = !found && t.names.len() <= 1 && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("tsv"));
        t
    };
    hide_secrets(&mut t, &extras, numbers);
    Ok(describe(p, &t))
}

/// "1 row", "2 rows".
fn plural_of(n: u64, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

/// What a table holds: its columns, sums, and counts by category.
fn describe(p: &Path, t: &Table) -> String {
    // first pass: each column on its own
    let mut cols: Vec<Col> = t
        .names
        .iter()
        .map(|n| Col { name: n.clone(), filled: 0, empty: 0, numbers: 0, not_numbers: Vec::new(), not_number_count: 0, sum: Sum::new(), min: None, max: None, whole: true, values: HashMap::new(), many: false, first: None, last: None, padded: false })
        .collect();
    for (r, fields) in t.rows.iter().enumerate() {
        for (i, c) in cols.iter_mut().enumerate() {
            let v = fields.get(i).map(|s| s.trim()).unwrap_or("");
            if v.is_empty() {
                c.empty += 1;
                continue;
            }
            c.filled += 1;
            if c.first.is_none() {
                c.first = Some(v.to_string());
            }
            c.last = Some(v.to_string());
            c.padded |= v.len() > 1 && v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit());
            match Dec::parse(v) {
                Some(d) => {
                    c.numbers += 1;
                    c.sum.add(d);
                    c.whole &= d.scale == 0;
                    if c.min.is_none_or(|m| less(d, m)) {
                        c.min = Some(d);
                    }
                    if c.max.is_none_or(|m| less(m, d)) {
                        c.max = Some(d);
                    }
                }
                None => {
                    c.not_number_count += 1;
                    if c.not_numbers.len() < 3 {
                        c.not_numbers.push((r as u64 + 2, v.to_string()));
                    }
                }
            }
            if !c.many {
                if c.values.len() < TRACK_VALUES || c.values.contains_key(v) {
                    *c.values.entry(v.to_string()).or_insert(0) += 1;
                } else {
                    c.many = true;
                }
            }
        }
    }
    let is_number = |c: &Col| c.numbers > 0 && c.not_number_count * 10 <= c.filled * NOT_NUMBERS_PER_10;
    // a category: 2 to 20 values that repeat; a measure: a number column that is not one
    let category = |c: &Col| !c.many && c.values.len() >= 2 && c.values.len() <= LIST_VALUES && (c.values.len() as u64) < c.filled;
    // an id: whole numbers, all different, written with leading zeros or in a column named so
    let id_name = |n: &str| {
        let n = n.to_ascii_lowercase();
        n == "id" || n == "req" || n == "request" || n.ends_with("_id") || n.ends_with("id") && n.len() <= 6
    };
    let all_different = |c: &Col| c.many || c.values.len() as u64 == c.filled;
    let id = |c: &Col| is_number(c) && c.whole && (c.padded || id_name(&c.name)) && all_different(c);
    let measure = |c: &Col| is_number(c) && !category(c) && !id(c);
    let listed = |c: &Col| !c.many && !c.values.is_empty() && c.values.len() <= LIST_VALUES;
    let sorted_values = |c: &Col| -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = c.values.iter().map(|(k, n)| (k.clone(), *n)).collect();
        if is_number(c) {
            v.sort_by(|a, b| match (Dec::parse(&a.0), Dec::parse(&b.0)) {
                (Some(x), Some(y)) if less(x, y) => std::cmp::Ordering::Less,
                (Some(x), Some(y)) if less(y, x) => std::cmp::Ordering::Greater,
                _ => a.0.cmp(&b.0),
            });
        } else {
            v.sort();
        }
        v
    };
    let rows = t.rows.len() as u64 + t.hidden_rows;
    let mut out = format!("== {}: {}, {} ({})", shown(p), plural_of(rows, "row"), plural_of(cols.len() as u64, "column"), t.kind);
    if t.no_sep {
        out.push_str(", no separator found in its header: each line read as one value");
    }
    if t.odd_count > 0 {
        let lines: Vec<String> = t.odd.iter().map(|l| l.to_string()).collect();
        out.push_str(&format!(", {} with another number of fields (records {}{})", plural_of(t.odd_count, "row"), lines.join(", "), if t.odd_count as usize > t.odd.len() { ", ..." } else { "" }));
        if t.hidden_rows > 0 {
            out.push_str(", hidden whole and left out of what follows (a column is named as a secret, and their fields may have moved past it)");
        }
    }
    out.push('\n');
    for (i, c) in cols.iter().enumerate() {
        let empty = if c.empty > 0 { format!(", {} empty", c.empty) } else { String::new() };
        if t.secret.get(i) == Some(&true) {
            let kind = t.secret_kind.get(i).cloned().flatten().unwrap_or_else(|| "text".into());
            out.push_str(&format!("{}: {kind}, {} filled{empty}, values hidden (secrets)\n", c.name, c.filled));
            continue;
        }
        let values = |c: &Col| sorted_values(c).iter().map(|(k, n)| format!("{} {n}", cut(&shown_value(k), 40))).collect::<Vec<_>>().join(", ");
        let ends = |c: &Col| format!("first {}, last {}", cut(&shown_value(c.first.as_deref().unwrap_or("")), 40), cut(&shown_value(c.last.as_deref().unwrap_or("")), 40));
        if c.filled == 0 {
            out.push_str(&format!("{}: empty in every row\n", c.name));
            continue;
        }
        if id(c) {
            out.push_str(&format!("{}: ids, {} filled{empty}, all different; {}\n", c.name, c.filled, ends(c)));
        } else if is_number(c) && category(c) {
            out.push_str(&format!("{}: {}, {} filled{empty}, {}: {}\n", c.name, if c.whole { "whole numbers" } else { "numbers" }, c.filled, plural_of(c.values.len() as u64, "value"), values(c)));
        } else if is_number(c) {
            let kind = if c.whole { "whole numbers" } else { "numbers" };
            let filled = if c.not_number_count > 0 {
                let s: Vec<String> = c.not_numbers.iter().map(|(r, v)| format!("{:?} in record {r}", cut(v, 30))).collect();
                format!(
                    "{} of {} filled (not a number: {}{}, left out of the sums)",
                    c.numbers,
                    c.filled,
                    s.join(", "),
                    if c.not_number_count as usize > c.not_numbers.len() { format!(" and {} more", c.not_number_count as usize - c.not_numbers.len()) } else { String::new() }
                )
            } else {
                format!("{} filled", c.filled)
            };
            out.push_str(&format!(
                "{}: {kind}, {filled}{empty}; sum {}, smallest {}, largest {}\n",
                c.name,
                c.sum.text(),
                c.min.map(Dec::text).unwrap_or_default(),
                c.max.map(Dec::text).unwrap_or_default()
            ));
        } else if listed(c) {
            out.push_str(&format!("{}: text, {} filled{empty}, {}: {}\n", c.name, c.filled, plural_of(c.values.len() as u64, "value"), values(c)));
        } else if all_different(c) && c.values.values().all(|n| *n == 1) {
            out.push_str(&format!("{}: text, {} filled{empty}, all different; {}\n", c.name, c.filled, ends(c)));
        } else {
            let mut v: Vec<(&String, &u64)> = c.values.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let top: Vec<String> = v.iter().take(TOP).map(|(k, n)| format!("{} ({n})", cut(k, 40))).collect();
            let n = if c.many { format!("more than {TRACK_VALUES} different values") } else { format!("{} different values", c.values.len()) };
            out.push_str(&format!("{}: text, {} filled{empty}, {n}; the most common: {}\n", c.name, c.filled, top.join(", ")));
        }
    }
    // second pass: each measure's sums by each category, and each pair of categories' counts
    let cats: Vec<usize> = (0..cols.len()).filter(|&i| category(&cols[i])).collect();
    let measures: Vec<usize> = (0..cols.len()).filter(|&i| measure(&cols[i])).collect();
    let mut sums: HashMap<(usize, String, usize), Sum> = HashMap::new();
    let pairs: Vec<(usize, usize)> = {
        let mut v = Vec::new();
        for (x, &i) in cats.iter().enumerate() {
            for &j in &cats[x + 1..] {
                // the first of a pair: the text column when one is text, else the one of more values
                let (ti, tj) = (is_number(&cols[i]), is_number(&cols[j]));
                let a_first = match (ti, tj) {
                    (false, true) => true,
                    (true, false) => false,
                    _ => cols[i].values.len() >= cols[j].values.len(),
                };
                v.push(if a_first { (i, j) } else { (j, i) });
            }
        }
        v.truncate(MAX_PAIRS);
        v
    };
    let mut counts: HashMap<(usize, String, String), u64> = HashMap::new();
    for fields in &t.rows {
        let cell = |i: usize| fields.get(i).map(|s| s.trim()).unwrap_or("");
        for &a in &cats {
            let av = cell(a);
            if av.is_empty() {
                continue;
            }
            for &m in &measures {
                if let Some(d) = Dec::parse(cell(m)) {
                    sums.entry((a, av.to_string(), m)).or_insert_with(Sum::new).add(d);
                }
            }
        }
        for (k, &(a, b)) in pairs.iter().enumerate() {
            let (av, bv) = (cell(a), cell(b));
            if !av.is_empty() && !bv.is_empty() {
                *counts.entry((k, av.to_string(), bv.to_string())).or_insert(0) += 1;
            }
        }
    }
    for &a in &cats {
        let values = sorted_values(&cols[a]);
        for &m in &measures {
            if m == a {
                continue;
            }
            // (a value with no number in that column: "none", not 0)
            let ss: Vec<(&String, Option<Sum>)> = values.iter().map(|(v, _)| (v, sums.get(&(a, v.clone(), m)).copied())).collect();
            let top = ss.iter().filter_map(|(_, s)| s.and_then(|s| s.int.map(|x| Dec { int: x, scale: s.scale }))).fold(None, |m: Option<Dec>, d| if m.is_none_or(|m| less(m, d)) { Some(d) } else { m });
            let parts: Vec<String> = ss
                .iter()
                .map(|(v, s)| match s {
                    Some(s) => {
                        let d = s.int.map(|x| Dec { int: x, scale: s.scale });
                        let most = top.is_some_and(|t| d.is_some_and(|d| !less(d, t) && !less(t, d)));
                        format!("{} {}{}", cut(v, 40), s.text(), if most { " (most)" } else { "" })
                    }
                    None => format!("{} none", cut(v, 40)),
                })
                .collect();
            out.push_str(&format!("{} by {}: {}\n", cols[m].name, cols[a].name, parts.join(", ")));
        }
    }
    for (k, &(a, b)) in pairs.iter().enumerate() {
        let bvals = sorted_values(&cols[b]);
        let groups: Vec<String> = sorted_values(&cols[a])
            .iter()
            .map(|(av, _)| {
                let inner: Vec<String> = bvals.iter().filter_map(|(bv, _)| counts.get(&(k, av.clone(), bv.clone())).map(|n| format!("{} {n}", cut(bv, 30)))).collect();
                format!("{}: {}", cut(av, 40), inner.join(", "))
            })
            .collect();
        let line = format!("counts of {} by {}: {}", cols[b].name, cols[a].name, groups.join("; "));
        out.push_str(&cut(&line, PAIR_CHARS));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_of(name: &str, text: &str) -> String {
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-table-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, text).unwrap();
        let s = summarize(&p, &mut HashMap::new()).unwrap().replace(&format!("{}", p.display()), name);
        std::fs::remove_dir_all(&d).unwrap();
        s
    }

    #[test]
    fn secrets_hidden_in_the_cells_not_the_kinds() {
        // a column named as a secret: every value hidden, its kind kept ("api_token: text" once
        // lost "text" and showed the passwords); a token in another column: hidden in its cell
        let csv = "user,password,api_token,note,region\nann,hunter2,abc,ok,North\nbob,letmein,AKIAABCDEFGHIJKLMNOP,key AKIAQRSTUVWXYZ234567 here,South\ncy,hunter2,abc,ok,North\n";
        let s = summary_of("users.csv", csv);
        for secret in ["hunter2", "letmein", "abc", "AKIA"] {
            assert!(!s.contains(secret), "{secret} shown:\n{s}");
        }
        assert!(s.contains("\npassword: text, 3 filled, values hidden (secrets)\napi_token: text, 3 filled, values hidden (secrets)\n"), "{s}");
        // (a hidden value numbered: different secrets stay different)
        assert!(s.contains("note: text, 3 filled, 2 values: key [hidden: AWS key 1] here 1, ok 2\n"), "{s}");
        assert!(s.contains("region: text, 3 filled, 2 values: North 2, South 1\n"), "{s}");
        // a log's key=value words: the same
        let s = summary_of("k.log", "ts INFO user=ann password=hunter2 msg=ok\nts INFO user=bob password=letmein msg=ok\n");
        assert!(!s.contains("hunter2") && !s.contains("letmein"), "{s}");
        assert!(s.contains("\npassword: text, 2 filled, values hidden (secrets)\n"), "{s}");
    }

    #[test]
    fn secrets_hidden_however_the_table_is_written() {
        let leaks = |s: &str, secrets: &[&str]| secrets.iter().filter(|x| s.contains(*x)).map(|x| x.to_string()).collect::<Vec<_>>();
        // names with spaces and capitals, an Authorization column, Bearer and Basic values
        let s = summary_of("w-a.csv", "user,API Key,Secret Key,Access Key,Private Key,Authorization,region\nann,k1x,s1x,a1x,p1x,Bearer tok1x,North\nbob,k2x,s2x,a2x,p2x,Basic dXNlcjpwYXNz,South\n");
        assert!(leaks(&s, &["k1x", "s1x", "a1x", "p1x", "tok1x", "dXNlcjpwYXNz", "k2x"]).is_empty(), "{s}");
        assert!(s.contains("\nAPI Key: text, 2 filled, values hidden (secrets)\n") && s.contains("\nAuthorization: text, 2 filled, values hidden (secrets)\n"), "{s}");
        // a row with more fields than names: its fields may have moved past the secret's column
        let s = summary_of("w-shift.csv", "user,password,region\nann,hunter2,North\nbob,let,me,in,South\n");
        // (left out whole: the other columns keep their kinds and values over the other rows)
        assert!(leaks(&s, &["hunter2", "bob", "let ", "me 1", "in 1", "South"]).is_empty() && s.contains("\nuser: text, 1 filled, 1 value: ann 1\n"), "{s}");
        // the separator not found: the header is one name, named as a secret by its parts
        let s = summary_of("w-sep.csv", "user;password;region\nann,hunter2,North\n");
        assert!(leaks(&s, &["hunter2"]).is_empty(), "{s}");
        // a log: a key and its value as words ("password: x"), a header line's credential, a
        // quoted value kept whole
        let s = summary_of("w-k.log", "2026-11-25T10:00:00 INFO password: hunter2 Authorization: Bearer tok9x msg=\"two words\" user=ann\n2026-11-25T10:00:01 INFO password: letmein msg=\"two words\" user=bob\n");
        assert!(leaks(&s, &["hunter2", "letmein", "tok9x"]).is_empty(), "{s}");
        assert!(s.contains("msg: text, 2 filled, 1 value: two words 2\n"), "{s}");
        // different secrets stay different: numbered, their sums apart
        let s = summary_of("w-m.csv", "key,amount\nAKIAQRSTUVWXYZ234567,100\nAKIAQRSTUVWXYZ234568,200\nAKIAQRSTUVWXYZ234567,300\nnone,5\n");
        assert!(!s.contains("AKIA") && s.contains("amount by key: [hidden: AWS key 1] 400 (most), [hidden: AWS key 2] 200, none 5\n"), "{s}");
        // a column named like a secret keeps its kind; true and false are shown
        let s = summary_of("w-k.csv", "model,max_token,has_password\na,4096,true\nb,8192,false\nc,4096,true\n");
        assert!(s.contains("max_token: whole numbers, 3 filled, values hidden (secrets)\n") && s.contains("has_password: text, 3 filled, 2 values: false 1, true 2\n"), "{s}");
        // an empty column; a value holding ", " quoted
        let s = summary_of("w-e.csv", "id,notes,who\n1,,\"Smith, Ann\"\n2,,\"Smith, Ann\"\n3,,Lee\n");
        assert!(s.contains("notes: empty in every row\n") && s.contains("\"Smith, Ann\" 2"), "{s}");
    }

    #[test]
    fn separators_numbers_and_moved_rows() {
        let leaks = |s: &str, secrets: &[&str]| secrets.iter().filter(|x| s.contains(*x)).map(|x| x.to_string()).collect::<Vec<_>>();
        // Excel's sep= line names the separator; that line is not the header
        let s = summary_of("e1.csv", "sep=;\nuser;password;amount\nalice;Hunter2xyzSEPa;10\nbob;Swordfish9SEPb;20\n");
        assert!(leaks(&s, &["Hunter2", "Swordfish9"]).is_empty() && s.starts_with("== e1.csv: 2 rows, 3 columns (semicolon-separated)\n") && s.contains("amount: whole numbers, 2 filled; sum 30,"), "{s}");
        // a header no separator splits, its names apart by spaces: named as a secret by its words
        let s = summary_of("w3.csv", "user  password  note\nalice  Hunter2xyzSPACE2a  hello\nbob  Swordfish9SPACE2b  hi\n");
        assert!(leaks(&s, &["Hunter2", "Swordfish9"]).is_empty() && s.contains("no separator found in its header"), "{s}");
        // the separator by the header, not by the ending: commas in a .tsv, | in a .csv
        let s = summary_of("wrongsep.tsv", "id,password,amount\n1,pwcommamadeup1,5\n2,pwcommamadeup2,7\n");
        assert!(leaks(&s, &["pwcomma"]).is_empty() && s.starts_with("== wrongsep.tsv: 2 rows, 3 columns (comma-separated)\n") && s.contains("amount: whole numbers, 2 filled; sum 12,"), "{s}");
        let s = summary_of("pipes.csv", "id|token|amount\n1|pipetokenmadeup01|5\n2|pipetokenmadeup02|7\n");
        assert!(leaks(&s, &["pipetoken"]).is_empty() && s.contains("3 columns (separated by '|')"), "{s}");
        // one secret, one number, wherever it is; two secrets in one cell, each its own
        let s = summary_of("n1.csv", "user,cred,note\nalice,AKIAAAAAAAAAAAAAAAA1,uses AKIAAAAAAAAAAAAAAAA1 daily\nbob,AKIAAAAAAAAAAAAAAAB2,uses AKIAAAAAAAAAAAAAAAB2 daily\n");
        assert!(!s.contains("AKIA") && s.contains("uses [hidden: AWS key 1] daily 1") && s.contains("uses [hidden: AWS key 2] daily 1") && s.contains("cred: text, 2 filled, 2 values: [hidden: AWS key 1] 1, [hidden: AWS key 2] 1"), "{s}");
        let s = summary_of("n4.csv", "k,n\nAKIAAAAAAAAAAAAAAAA1 AKIAAAAAAAAAAAAAAAB2,1\nAKIAAAAAAAAAAAAAAAA1 AKIAAAAAAAAAAAAAAAA1,2\n");
        assert!(s.contains("[hidden: AWS key 1] [hidden: AWS key 2] 1") && s.contains("[hidden: AWS key 1] [hidden: AWS key 1] 1"), "{s}");
        // a log: cells first, then hidden; different keys stay apart, their sums right
        let s = summary_of("awskeys.log", "ts=1 key_id=AKIAAAAAMADEUP000001 amount=10\nts=2 key_id=AKIAAAAAMADEUP000001 amount=20\nts=3 key_id=AKIAAAAAMADEUP000002 amount=5\nts=4 key_id=AKIAAAAAMADEUP000001 amount=30\nts=5 key_id=AKIAAAAAMADEUP000002 amount=7\nts=6 key_id=AKIAAAAAMADEUP000003 amount=100\n");
        assert!(!s.contains("AKIA") && s.contains("amount by key_id: [hidden: AWS key 1] 60, [hidden: AWS key 2] 12, [hidden: AWS key 3] 100 (most)\n"), "{s}");
        // a log's column named as a secret keeps its kind; true and false are shown
        let s = summary_of("app.log", "ts=1 model=a max_token=4096 has_password=true\nts=2 model=b max_token=2048 has_password=false\nts=3 model=a max_token=4096 has_password=true\n");
        assert!(s.contains("max_token: whole numbers, 3 filled, values hidden (secrets)\n") && s.contains("has_password: text, 3 filled, 2 values: false 1, true 2\n"), "{s}");
        // a row with another number of fields in a table with a secret's column: left out whole,
        // the number columns keep their kinds and sums
        let s = summary_of("odd.csv", "id,user,api_key,amount\n1,u1,k1madeup,10\n2,u2,k2madeup,20\n3,u3,k3madeup\n4,u4,k4madeup,40,extra\n");
        assert!(leaks(&s, &["madeup", "extra"]).is_empty() && s.contains("amount: whole numbers, 2 filled; sum 30, smallest 10, largest 20\n") && s.contains("id: whole numbers") || s.contains("id: ids"), "{s}");
        assert!(s.starts_with("== odd.csv: 4 rows, 4 columns (comma-separated), 2 rows with another number of fields (records 4, 5), hidden whole"), "{s}");
        // a log value's escaped quotes stay in it
        let s = summary_of("q2.log", "ts=1 msg=\"user \\\"ann\\\" logged in\" bytes=10\nts=2 msg=\"user \\\"ann\\\" logged out\" bytes=20\nts=3 msg=\"user \\\"ann\\\" logged in\" bytes=30\n");
        assert!(s.contains("msg: text, 3 filled, 2 values: \"user \\\"ann\\\" logged in\" 2, \"user \\\"ann\\\" logged out\" 1\n") || s.contains("msg: text, 3 filled, 2 values: user \"ann\" logged in 2, user \"ann\" logged out 1\n"), "{s}");
        assert!(!s.contains("word1") && s.contains("bytes by msg:"), "{s}");
    }

    #[test]
    fn read_hides_a_secret_column() {
        // a .csv's cells under a secret's name hidden for read, every other byte and line kept
        let text = "user,password,amount\nann,hunter2,10\nbob,\"multi\nline\",20\ncy,true,30\ndee,x9secret,40,extra\n";
        let (h, n) = hide_columns(Path::new("x.csv"), text).unwrap();
        assert_eq!(n, 6, "{h}");
        assert!(!h.contains("hunter2") && !h.contains("multi") && !h.contains("x9secret") && !h.contains("extra") && h.contains("cy,true,30\n"), "{h}");
        assert_eq!(h.lines().count(), text.lines().count(), "{h}");
        assert!(h.starts_with("user,password,amount\nann,[hidden: ") && h.ends_with("\n[hidden: secret],[hidden: secret],[hidden: secret],[hidden: secret]\n"), "{h}");
        // no such column, or not a .csv or .tsv: None
        assert!(hide_columns(Path::new("x.csv"), "a,b\n1,2\n").is_none() && hide_columns(Path::new("x.txt"), text).is_none());
        // a .tsv with Excel's line
        let (h, _) = hide_columns(Path::new("k.tsv"), "sep=\t\nname\tAPI Key\nann\tmadeuptsvkey01\n").unwrap();
        assert!(!h.contains("madeuptsvkey01") && h.starts_with("sep=\t\nname\tAPI Key\nann\t[hidden: "), "{h}");
    }

    #[test]
    fn sums_by_category_exact() {
        let csv = "region,product,units,amount_cents\neast,dowel,37,8790\nnorth,easel,28,20052\nnorth,dowel,18,54107\nwest,easel,7,47683\neast,easel,3,1\n";
        let s = summary_of("sales.csv", csv);
        assert_eq!(
            s,
            "== sales.csv: 5 rows, 4 columns (comma-separated)\n\
region: text, 5 filled, 3 values: east 2, north 2, west 1\n\
product: text, 5 filled, 2 values: dowel 2, easel 3\n\
units: whole numbers, 5 filled; sum 93, smallest 3, largest 37\n\
amount_cents: whole numbers, 5 filled; sum 130633, smallest 1, largest 54107\n\
units by region: east 40, north 46 (most), west 7\n\
amount_cents by region: east 8791, north 74159 (most), west 47683\n\
units by product: dowel 55 (most), easel 38\n\
amount_cents by product: dowel 62897, easel 67736 (most)\n\
counts of product by region: east: dowel 1, easel 1; north: dowel 1, easel 1; west: easel 1\n"
        );
    }

    #[test]
    fn decimals_quotes_and_odd_rows() {
        // decimals added exactly (0.1 + 0.2 is 0.3), quoted fields with commas and quotes, a row
        // with too few fields, a cell that is not a number, an empty cell
        let csv = "name,price,note\n\"Smith, J\",0.1,\"say \"\"hi\"\"\"\nLee,0.2,x\nAli,N/A,\nKim,1.25,y\nRay,2,z\nBo,3,w\nJo,4,v\nAl,5,u\nEd,6,t\nOz,7,s\nIo,8\n";
        let s = summary_of("t.csv", csv);
        assert!(s.starts_with("== t.csv: 11 rows, 3 columns (comma-separated), 1 row with another number of fields (records 12)\n"), "{s}");
        assert!(s.contains("price: numbers, 10 of 11 filled (not a number: \"N/A\" in record 4, left out of the sums); sum 36.55, smallest 0.1, largest 8\n"), "{s}");
        // (a column whose values are all different gives no sums by it)
        assert!(!s.contains(" by name:"), "{s}");
        assert!(s.contains("name: text, 11 filled, 11 values: Al 1, Ali 1, Bo 1, Ed 1, Io 1, Jo 1, Kim 1, Lee 1, Oz 1, Ray 1, \"Smith, J\" 1\n"), "{s}");
        assert!(s.contains("note: text, 9 filled, 2 empty, 9 values: s 1, say \"hi\" 1, t 1, u 1, v 1, w 1, x 1, y 1, z 1\n"), "{s}");
        // a category with no number in a column: none, not 0
        let g = summary_of("g.csv", "k,v\na,1\na,2\nb,x\nb,\nc,3\nc,4\nc,5\nd,6\nd,7\ne,8\ne,9\nf,10\nf,11\ng,12\ng,13\nh,14\nh,15\ni,16\ni,17\nj,18\nj,19\n");
        assert!(g.contains("v: whole numbers, 19 of 20 filled (not a number: \"x\" in record 4, left out of the sums), 1 empty; sum 190, smallest 1, largest 19\n"), "{g}");
        assert!(g.contains("v by k: a 3, b none, c 12, d 13, e 17, f 21, g 25, h 29, i 33, j 37 (most)\n"), "{g}");
        // semicolons and tabs by the header
        assert!(summary_of("s.csv", "a;b\nx;1\ny;2\n").contains("(semicolon-separated)"));
        assert!(summary_of("t.tsv", "a\tb\nx\t1\ny\t2\n").contains("b: whole numbers, 2 filled; sum 3, smallest 1, largest 2\n"));
        assert_eq!(Dec::parse("-0.05").unwrap().text(), "-0.05");
        assert!(Dec::parse("1e5").is_none() && Dec::parse(".").is_none() && Dec::parse("").is_none());
    }

    #[test]
    fn a_log_of_key_values() {
        let log = "2025-01-01T00:00:01 INFO req=0001 GET /a status=200 ms=5\n\
2025-01-01T00:00:02 ERROR req=0002 POST /b status=500 ms=7\n\
2025-01-01T00:00:03 INFO req=0003 GET /a status=200 ms=6\n\
2025-01-01T00:00:04 ERROR req=0004 GET /a status=503 ms=9\n\
2025-01-01T00:00:05 INFO req=0005 POST /b status=200 ms=4 and some words of a message\n";
        let s = summary_of("app.log", log);
        assert!(s.starts_with("== app.log: 5 rows, 7 columns (a log: its words by place and its key=value words)\n"), "{s}");
        // the plain words by place, named by what they hold; a message's words left out
        // (a few values all listed; many all different: the first and the last)
        assert!(s.contains("\ntime: text, 5 filled, 5 values: 2025-01-01T00:00:01 1, 2025-01-01T00:00:02 1,") && s.contains("\nlevel: text, 5 filled, 2 values: ERROR 2, INFO 3\n") && s.contains("\nmethod: text") && s.contains("\npath: text, 5 filled, 2 values: /a 3, /b 2\n") && !s.contains("message"), "{s}");
        // an id; a code as a category; an amount summed by each category
        assert!(s.contains("\nreq: ids, 5 filled, all different; first 0001, last 0005\n") && s.contains("\nstatus: whole numbers, 5 filled, 3 values: 200 3, 500 1, 503 1\n"), "{s}");
        assert!(s.contains("\nms: whole numbers, 5 filled; sum 31, smallest 4, largest 9\n") && s.contains("\nms by path: /a 20 (most), /b 11\n") && !s.contains("\nstatus by") && !s.contains("\nreq by"), "{s}");
        // the counts of one category by another: the 5xx of each path
        assert!(s.contains("\ncounts of status by path: /a: 200 2, 503 1; /b: 200 1, 500 1\n"), "{s}");
        assert!(s.contains("\ncounts of status by level: ERROR: 500 1, 503 1; INFO: 200 3\n"), "{s}");
    }

    #[test]
    fn many_values_the_most_common() {
        let mut csv = String::from("id,city\n");
        for i in 0..100 {
            csv.push_str(&format!("{i},{}\n", if i % 3 == 0 { "Paris".to_string() } else { format!("town{i}") }));
        }
        let s = summary_of("c.csv", &csv);
        assert!(s.contains("city: text, 100 filled, 67 different values; the most common: Paris (34), town1 (1)"), "{s}");
        assert!(s.contains("id: ids, 100 filled, all different; first 0, last 99\n") && !s.contains(" by city:") && !s.contains("id by"), "{s}");
    }
}
