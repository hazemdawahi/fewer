//! Multi-line records: a record is a first line plus the lines that continue
//! it. A record counts once in key counts, error groups and odd lines (search and count with a
//! text still count lines, as grep does). A record ends after 1,000 lines.
//!
//! A line continues the record before it when it is:
//! - empty, or starts with a space or a tab;
//! - a frame: `at ` (Java, .NET, Node, Chrome; Rust's `at <path>:<n>`), `Caused by:`,
//!   `Suppressed:`, `... N more`, `from <file>:<n>` (Ruby), `File "...", line <n>` (Python),
//!   `goroutine <n> [` (Go), `<name>@<url>:<n>:<n>` (Firefox, Safari);
//! - `#<n> ` (PHP and Dart): `#0` after an error line or a trace header, then each next number;
//! - a compiler's code excerpt line, `<n> |` (rustc writes it at the start of the line);
//! - `<n>: ` inside a Rust backtrace;
//! - a trace header inside an error record: `Stack trace:`, `stack backtrace:`, `[stacktrace]`;
//! - Python: from `Traceback (most recent call last):` to the exception line (`Name: message`,
//!   not indented), with chained tracebacks ("During handling of the above exception ...");
//! - Go: after `panic: ` or `fatal error: `, the `[signal` line, `goroutine` blocks, call lines
//!   (`main.main()`) and `created by` lines;
//! - Rust: after `thread '...' panicked at ...:`, the message line, `stack backtrace:`, frames
//!   and the `note: ... RUST_BACKTRACE` line;
//! - Dart: the line after `Unhandled exception:`;
//! - Node's uncaught errors: `<file>.js:<n>`, the source line, the `^` line, the error line,
//!   its frames and `Node.js v<n>`;
//! - the next piece of a CRI 'P' line or a Docker line cut without its '\n';
//! - in a MySQL slow log, every line of an entry (an entry starts at `# Time:`, or at
//!   `# User@Host:` when no `# Time:` came just before it);
//! - in a crash report, every line (the whole file is one error record);
//! - in csv and tsv, the rest of a row whose quoted field holds a line end.
//!
//! The key line: the record's first line, except Python's (last) exception line, the line
//! after Dart's `Unhandled exception:`, the message line after Rust's `panicked at ...:`, Node's
//! error line after the `^`, a MySQL entry's first SQL line, and an Android tombstone's
//! `>>> name <<<` line.
//!
//! The rules read each line's body (`Fields::body`): the text after a CRI or GitHub Actions
//! line's time, Docker's `log` (a program's raw output, wrapped), or else the whole clean line
//! (a syslog line too, whose message is the whole line: its lines never continue by indent).
//! Structured events, JSON and logfmt lines, have no body: each is a record of its own.

use crate::formats::{Fields, Format, Profile};
use crate::times::Clock;

/// A record ends after this many lines.
pub const MAX_LINES: u32 = 1000;

/// What one line is to its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// The line starts a new record (the record before it, if any, ended on the line before).
    pub start: bool,
    /// The line gives its record's key: the first line, or a later line that names the error.
    /// A later key line replaces an earlier one.
    pub key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Python,
    Go,
    Rust,
    Node,
    Mysql,
    Crash,
}

/// Groups a source's lines into records, one line at a time.
#[derive(Clone, Debug)]
pub struct Records {
    format: Format,
    /// lines in the current record
    lines: u32,
    mode: Mode,
    /// the record holds a stack trace (an error record)
    trace: bool,
    /// the record's first line reads like an error (PHP and Dart frames may follow it)
    error_line: bool,
    /// the last `#<n>` frame number
    hash: Option<u32>,
    /// the line before pulls the next line into the record, and that line gives the key
    pull: bool,
    pull_key: bool,
    /// the line before was a CRI 'P' line or a Docker piece without its '\n'
    partial: bool,
    /// Python: the exception line was seen; a chain line ("During handling ...") was seen after it
    py_exc: bool,
    py_chain: bool,
    /// Node: the '^' line, then the error line, were seen
    node_caret: bool,
    node_err: bool,
    /// MySQL: the entry's first SQL line was seen; the entry so far is only its "# Time:" line
    sql_key: bool,
    time_only: bool,
    /// csv and tsv: a quoted field holds a line end
    csv_open: bool,
    /// a tombstone's ">>> name <<<" line was seen
    crash_key: bool,
}

impl Records {
    pub fn new(format: Format) -> Records {
        Records {
            format,
            lines: 0,
            mode: Mode::Normal,
            trace: false,
            error_line: false,
            hash: None,
            pull: false,
            pull_key: false,
            partial: false,
            py_exc: false,
            py_chain: false,
            node_caret: false,
            node_err: false,
            sql_key: false,
            time_only: false,
            csv_open: false,
            crash_key: false,
        }
    }

    /// Feeds the next line: `body` is what the rules read (`Fields::body`; None for
    /// structured events, JSON and logfmt lines, which always start a record), `partial` is
    /// `Fields::partial`.
    pub fn push(&mut self, body: Option<&[u8]>, partial: bool) -> Step {
        let step = if self.lines > 0 && self.lines < MAX_LINES && self.continues(body) {
            self.lines += 1;
            Step { start: false, key: self.take(body) }
        } else {
            self.begin(body);
            Step { start: true, key: true }
        };
        self.partial = partial;
        if matches!(self.format, Format::Csv | Format::Tsv) && body.is_some_and(|b| b.iter().filter(|&&c| c == b'"').count() % 2 == 1) {
            self.csv_open = !self.csv_open;
        }
        step
    }

    /// Feeds a line by its fields.
    pub fn push_fields(&mut self, f: &Fields) -> Step {
        self.push(f.body(), f.partial)
    }

    /// The current record holds a stack trace (or is a crash report): an error record.
    pub fn is_trace(&self) -> bool {
        self.trace
    }

    /// Lines in the current record so far.
    pub fn len(&self) -> u32 {
        self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.lines == 0
    }

    /// A new record starts with this line.
    fn begin(&mut self, body: Option<&[u8]>) {
        *self = Records { lines: 1, ..Records::new(self.format) };
        let b = body.unwrap_or(b"");
        match self.format {
            Format::Crash => {
                self.mode = Mode::Crash;
                self.trace = true;
                return;
            }
            Format::MysqlSlow => {
                self.mode = Mode::Mysql;
                self.time_only = b.starts_with(b"# Time:");
                self.sql_key = is_sql(b);
                return;
            }
            _ => {}
        }
        self.error_line = looks_error(b);
        if b.starts_with(b"Traceback (most recent call last):") {
            self.mode = Mode::Python;
            self.trace = true;
        } else if b.starts_with(b"thread '") && contains(b, b"' panicked at ") {
            self.mode = Mode::Rust;
            self.trace = true;
            // since Rust 1.73 the message is on the next line, and names the error
            if b.ends_with(b":") {
                self.pull = true;
                self.pull_key = true;
            }
        } else if b.starts_with(b"panic: ") || b.starts_with(b"fatal error: ") {
            self.mode = Mode::Go;
            self.trace = true;
        } else if b.starts_with(b"Unhandled exception:") {
            // Dart: the next line names the exception
            self.trace = true;
            self.pull = true;
            self.pull_key = true;
        } else if b.starts_with(b"Exception in thread ") {
            self.trace = true;
        } else if node_location(b) {
            // Node prints the source line next, then a '^' under the throw
            self.mode = Mode::Node;
            self.pull = true;
        }
    }

    /// Does this line continue the current record?
    fn continues(&self, body: Option<&[u8]>) -> bool {
        match self.mode {
            Mode::Crash => return true,
            Mode::Mysql => return !body.is_some_and(|b| self.mysql_starts(b)),
            _ => {}
        }
        if self.partial || self.pull || self.csv_open {
            return true;
        }
        let Some(b) = body else { return false };
        if is_blank(b) || b[0] == b' ' || b[0] == b'\t' || is_frame(b) || gutter_line(b) {
            return true;
        }
        if (self.trace || self.error_line) && is_stack_header(b) {
            return true;
        }
        if let Some(n) = hash_frame(b) {
            if n == 0 && (self.trace || self.error_line) && b.contains(&b'(') && b.contains(&b')') {
                return true;
            }
            if n > 0 && self.hash == Some(n - 1) {
                return true;
            }
        }
        match self.mode {
            Mode::Python => {
                if self.py_chain && b.starts_with(b"Traceback (most recent call last):") {
                    true
                } else if !self.py_exc {
                    is_exception_line(b)
                } else {
                    is_chain_line(b)
                }
            }
            Mode::Go => go_continues(b),
            Mode::Rust => rust_frame(b) || (b.starts_with(b"note: ") && contains(b, b"RUST_BACKTRACE")),
            Mode::Node => {
                if !self.node_caret {
                    is_caret(b)
                } else if !self.node_err {
                    true
                } else {
                    b.starts_with(b"Node.js v") || b.starts_with(b"(Use `node ") || b.starts_with(b"}")
                }
            }
            _ => false,
        }
    }

    /// Takes a line that continues the record; says whether it gives the record's key.
    fn take(&mut self, body: Option<&[u8]>) -> bool {
        let b = body.unwrap_or(b"");
        if self.pull {
            self.pull = false;
            let key = self.pull_key;
            self.pull_key = false;
            return key;
        }
        match self.mode {
            Mode::Crash => {
                if !self.crash_key && self.lines <= 10 && contains(b, b">>> ") && contains(b, b" <<<") {
                    self.crash_key = true;
                    return true;
                }
                return false;
            }
            Mode::Mysql => {
                self.time_only = false;
                if !self.sql_key && is_sql(b) {
                    self.sql_key = true;
                    return true;
                }
                return false;
            }
            _ => {}
        }
        let t = trim_start(b);
        if is_frame(t) || is_stack_header(t) || (self.mode == Mode::Rust && rust_frame(t)) {
            self.trace = true;
        }
        if let Some(n) = hash_frame(t) {
            self.hash = Some(n);
            self.trace = true;
        }
        if t.starts_with(b"goroutine ") && self.mode == Mode::Normal {
            self.mode = Mode::Go;
        }
        let indented = t.len() < b.len();
        match self.mode {
            Mode::Python if !indented && !is_blank(b) => {
                if !self.py_exc && is_exception_line(b) {
                    self.py_exc = true;
                    self.py_chain = false;
                    return true;
                }
                if is_chain_line(b) {
                    self.py_chain = true;
                    self.py_exc = false;
                } else if b.starts_with(b"Traceback (most recent call last):") {
                    self.py_exc = false;
                    self.py_chain = false;
                }
            }
            Mode::Node => {
                if !self.node_caret && is_caret(b) {
                    self.node_caret = true;
                } else if self.node_caret && !self.node_err && !indented && !is_blank(b) {
                    self.node_err = true;
                    self.trace = true;
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// MySQL slow log: does this line start a new entry?
    fn mysql_starts(&self, b: &[u8]) -> bool {
        b.starts_with(b"# Time:") || (b.starts_with(b"# User@Host:") && !self.time_only) || is_mysql_banner(b)
    }
}

/// A record found by [`split`]: its first and last line and its key line (indexes into the
/// lines given), and whether it holds a stack trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub first: usize,
    pub last: usize,
    pub key: usize,
    pub trace: bool,
}

/// Splits clean lines (without their line ends) into records. The catalog feeds [`Records`]
/// line by line instead; this is for tests and small inputs.
pub fn split(profile: &Profile, lines: &[&[u8]], c: &Clock) -> Vec<Record> {
    let mut r = Records::new(profile.format);
    let mut out: Vec<Record> = Vec::new();
    for (i, &l) in lines.iter().enumerate() {
        let f = profile.fields(l, c);
        let st = r.push_fields(&f);
        if st.start || out.is_empty() {
            out.push(Record { first: i, last: i, key: i, trace: false });
        }
        let cur = out.last_mut().expect("a record was started");
        cur.last = i;
        if st.key {
            cur.key = i;
        }
        cur.trace = r.is_trace();
    }
    out
}

/// `at ` and a frame after it (Java, .NET, Node, Chrome; Rust's `at <path>:<n>`): the text
/// after it does not start with a digit and holds '(' or ":<digit>" ("at least one" and
/// "at 10:00" are not frames).
fn at_frame(b: &[u8]) -> bool {
    let Some(r) = b.strip_prefix(b"at ") else { return false };
    !r.is_empty() && !r[0].is_ascii_digit() && (r.contains(&b'(') || r.windows(2).any(|w| w[0] == b':' && w[1].is_ascii_digit()))
}

/// A frame line, not indented: see the module's list.
fn is_frame(b: &[u8]) -> bool {
    at_frame(b)
        || b.starts_with(b"Caused by:")
        || b.starts_with(b"Suppressed:")
        || more_line(b)
        || ruby_from(b)
        || python_file(b)
        || goroutine(b)
        || firefox_frame(b)
}

/// `... 5 more` and `... 5 common frames omitted` (Java, logback).
fn more_line(b: &[u8]) -> bool {
    let Some(r) = b.strip_prefix(b"... ") else { return false };
    let n = r.iter().take_while(|c| c.is_ascii_digit()).count();
    n > 0 && (r[n..].starts_with(b" more") || r[n..].starts_with(b" common frames omitted"))
}

/// `from <file>:<n>` (Ruby).
fn ruby_from(b: &[u8]) -> bool {
    let Some(r) = b.strip_prefix(b"from ") else { return false };
    let tok = &r[..r.iter().position(|&c| c == b' ').unwrap_or(r.len())];
    tok.windows(2).any(|w| w[0] == b':' && w[1].is_ascii_digit())
}

/// `File "...", line <n>` (Python).
fn python_file(b: &[u8]) -> bool {
    b.starts_with(b"File \"") && b.windows(8).position(|w| w == b"\", line ").is_some_and(|p| b.get(p + 8).is_some_and(u8::is_ascii_digit))
}

/// `goroutine <n> [` (Go).
fn goroutine(b: &[u8]) -> bool {
    let Some(r) = b.strip_prefix(b"goroutine ") else { return false };
    let n = r.iter().take_while(|c| c.is_ascii_digit()).count();
    n > 0 && r[n..].starts_with(b" [")
}

/// `<name>@<url>:<n>:<n>` (Firefox, Safari): an '@' with a URL or a path after it, and the
/// line ending with two numbers. The name has no spaces, except Safari's "global code" and
/// the like.
fn firefox_frame(b: &[u8]) -> bool {
    let Some(at) = b.iter().position(|&c| c == b'@') else { return false };
    let (name, url) = (&b[..at], &b[at + 1..]);
    let named = !name.contains(&b' ') || name.ends_with(b" code");
    let place = url.starts_with(b"/") || contains(url, b"://");
    named && place && ends_with_two_numbers(url)
}

fn ends_with_two_numbers(b: &[u8]) -> bool {
    let mut i = b.len();
    for _ in 0..2 {
        let n = b[..i].iter().rev().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || i < n + 1 || b[i - n - 1] != b':' {
            return false;
        }
        i -= n + 1;
    }
    true
}

/// A trace header line.
fn is_stack_header(b: &[u8]) -> bool {
    [&b"Stack trace:"[..], b"Stack Trace:", b"Stacktrace:", b"stacktrace:", b"stack backtrace:", b"[stacktrace]"].iter().any(|h| b.starts_with(h))
}

/// `#<n>` and a space or tab (PHP, Dart): n.
fn hash_frame(b: &[u8]) -> Option<u32> {
    let r = b.strip_prefix(b"#")?;
    let n = r.iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 || n > 6 || !matches!(r.get(n), Some(b' ' | b'\t')) {
        return None;
    }
    std::str::from_utf8(&r[..n]).ok()?.parse().ok()
}

/// `<n>: ` (a Rust backtrace frame).
fn rust_frame(b: &[u8]) -> bool {
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    n > 0 && n <= 4 && b[n..].starts_with(b": ")
}

/// Python's exception line: a dotted name, then the end or ": " ("ValueError: bad",
/// "KeyboardInterrupt", "requests.exceptions.ConnectionError: ...").
fn is_exception_line(b: &[u8]) -> bool {
    if !b.first().is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
        return false;
    }
    let n = b.iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_' || **c == b'.').count();
    n == b.len() || b[n..].starts_with(b": ") || &b[n..] == b":"
}

/// The lines between chained Python tracebacks.
fn is_chain_line(b: &[u8]) -> bool {
    b.starts_with(b"During handling of the above exception, another exception occurred:")
        || b.starts_with(b"The above exception was the direct cause of the following exception:")
}

/// Go's lines inside a panic: the signal line, goroutine blocks, call lines, created-by lines.
fn go_continues(b: &[u8]) -> bool {
    b.starts_with(b"[signal ") || b.starts_with(b"created by ") || b.starts_with(b"...additional frames elided...") || go_call(b)
}

/// A Go call line: `main.main()`, `main.(*T).Serve(0xc000010000, {0x1, 0x2})`: no space before
/// the first '(' and a ')' at the end.
fn go_call(b: &[u8]) -> bool {
    let Some(p) = b.iter().position(|&c| c == b'(') else { return false };
    p > 0 && b[0].is_ascii_alphabetic() && !b[..p].contains(&b' ') && b.ends_with(b")")
}

/// Node's first line of an uncaught error: `<file>:<n>` alone, the file a JavaScript or
/// TypeScript one.
fn node_location(b: &[u8]) -> bool {
    let Some(c) = b.iter().rposition(|&x| x == b':') else { return false };
    let (path, num) = (&b[..c], &b[c + 1..]);
    !num.is_empty()
        && num.iter().all(u8::is_ascii_digit)
        && !path.contains(&b' ')
        && [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".jsx", ".tsx"].iter().any(|e| path.ends_with(e.as_bytes()))
}

/// A line of spaces and '^' (Node's pointer under the throw).
fn is_caret(b: &[u8]) -> bool {
    b.contains(&b'^') && b.iter().all(|&c| c == b' ' || c == b'^' || c == b'\t')
}

/// A line that reads like an error (its first 200 bytes, any case).
fn looks_error(b: &[u8]) -> bool {
    let w = &b[..b.len().min(200)];
    [&b"error"[..], b"exception", b"fatal", b"panic", b"uncaught", b"unhandled", b"traceback"]
        .iter()
        .any(|k| w.windows(k.len()).any(|x| x.eq_ignore_ascii_case(k)))
}

/// A code excerpt line of rustc (and of compilers like it): a line number, spaces, then '|'
/// and a space or the end ("2 |     let x: i32 = \"a\";"). rustc writes the widest line number
/// of an excerpt at the start of the line.
fn gutter_line(b: &[u8]) -> bool {
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 || n > 6 {
        return false;
    }
    let s = b[n..].iter().take_while(|&&c| c == b' ').count();
    s > 0 && b.get(n + s) == Some(&b'|') && matches!(b.get(n + s + 1), None | Some(b' '))
}

/// MySQL's server banner at the start of a slow log: `<path>, Version: ...`.
fn is_mysql_banner(b: &[u8]) -> bool {
    b.starts_with(b"/") && contains(b, b", Version: ")
}

/// A MySQL slow-log line holding the query (not a comment, a `SET timestamp=`, a `use db;`,
/// or the banner's lines).
fn is_sql(b: &[u8]) -> bool {
    !is_blank(b)
        && !b.starts_with(b"#")
        && !b.starts_with(b"SET timestamp=")
        && !(b.len() >= 4 && b[..4].eq_ignore_ascii_case(b"use "))
        && !is_mysql_banner(b)
        && !b.starts_with(b"Tcp port:")
        && !b.starts_with(b"Time ")
}

fn is_blank(b: &[u8]) -> bool {
    b.iter().all(|&c| c == b' ' || c == b'\t')
}

fn trim_start(b: &[u8]) -> &[u8] {
    &b[b.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(b.len())..]
}

fn contains(h: &[u8], n: &[u8]) -> bool {
    h.windows(n.len()).any(|w| w == n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::detect;
    use crate::times::Zone;

    fn clock() -> Clock {
        Clock::new(Zone::Fixed(0), 1_795_003_200_000)
    }

    /// (first, last, key) of each record of a plain log.
    fn recs(text: &str) -> Vec<(usize, usize, usize)> {
        recs_in(Format::Plain, text)
    }

    fn recs_in(format: Format, text: &str) -> Vec<(usize, usize, usize)> {
        let lines: Vec<&[u8]> = text.lines().map(str::as_bytes).collect();
        split(&Profile::new(format), &lines, &clock()).iter().map(|r| (r.first, r.last, r.key)).collect()
    }

    /// The made-up trace in `text` (after one ordinary line, and before another) is one record
    /// whose key line is `key`, and it is an error record.
    fn one_trace(text: &str, key: &str) {
        let full = format!("2026-11-18 10:15:01 INFO before\n{text}\n2026-11-18 10:15:09 INFO after\n");
        let lines: Vec<&[u8]> = full.lines().map(str::as_bytes).collect();
        let r = split(&Profile::new(Format::Plain), &lines, &clock());
        let n = lines.len();
        assert_eq!(r.len(), 3, "{text}\n{r:?}");
        assert_eq!((r[0].first, r[0].last), (0, 0));
        assert_eq!((r[1].first, r[1].last), (1, n - 2), "{text}");
        assert_eq!(full.lines().nth(r[1].key), Some(key), "{text}");
        assert!(r[1].trace, "{text}");
        assert_eq!((r[2].first, r[2].last), (n - 1, n - 1));
    }

    #[test]
    fn java() {
        one_trace(
            "Exception in thread \"main\" java.lang.IllegalStateException: boom\n\
             \tat com.example.App.run(App.java:42)\n\
             \tat com.example.App.main(App.java:10)\n\
             Caused by: java.io.IOException: disk full\n\
             \tat com.example.Io.write(Io.java:7)\n\
             \t... 2 more",
            "Exception in thread \"main\" java.lang.IllegalStateException: boom",
        );
        // frames whose indent a log shipper removed
        one_trace(
            "java.lang.NullPointerException: x is null\n\
             at com.example.A.b(A.java:1)\n\
             Suppressed: java.lang.RuntimeException: close\n\
             at com.example.A.close(A.java:9)\n\
             ... 3 common frames omitted",
            "java.lang.NullPointerException: x is null",
        );
    }

    #[test]
    fn python() {
        one_trace(
            "Traceback (most recent call last):\n  \
             File \"/app/main.py\", line 10, in <module>\n    \
             main()\n  \
             File \"/app/main.py\", line 6, in main\n    \
             raise ValueError(\"bad value\")\n\
             ValueError: bad value",
            "ValueError: bad value",
        );
        // chained: one record, keyed by the last exception
        one_trace(
            "Traceback (most recent call last):\n  \
             File \"a.py\", line 3, in <module>\n    \
             d[\"x\"]\n\
             KeyError: 'x'\n\
             \n\
             During handling of the above exception, another exception occurred:\n\
             \n\
             Traceback (most recent call last):\n  \
             File \"a.py\", line 5, in <module>\n    \
             raise RuntimeError(\"wrapped\")\n\
             RuntimeError: wrapped",
            "RuntimeError: wrapped",
        );
        // two tracebacks in a row without a chain line are two records
        let t = "Traceback (most recent call last):\n  File \"a.py\", line 1, in <module>\nKeyboardInterrupt\nTraceback (most recent call last):\n  File \"b.py\", line 2, in <module>\nOSError: no";
        assert_eq!(recs(t), vec![(0, 2, 2), (3, 5, 5)]);
    }

    #[test]
    fn go() {
        one_trace(
            "panic: runtime error: invalid memory address or nil pointer dereference\n\
             [signal SIGSEGV: segmentation violation code=0x1 addr=0x0 pc=0x10a3f4e]\n\
             \n\
             goroutine 1 [running]:\n\
             main.handler(0x0)\n\
             \t/app/main.go:12 +0x1e\n\
             main.(*Server).Serve(0xc000010000, {0x1, 0x2})\n\
             \t/app/server.go:40 +0x25\n\
             created by main.main in goroutine 1\n\
             \t/app/main.go:20 +0x99",
            "panic: runtime error: invalid memory address or nil pointer dereference",
        );
        // `go run` adds its own line after the trace: a new record
        assert_eq!(recs("panic: boom\n\ngoroutine 1 [running]:\nmain.main()\n\t/a.go:3 +0x1\nexit status 2"), vec![(0, 4, 0), (5, 5, 5)]);
    }

    #[test]
    fn node() {
        one_trace(
            "Error: connect ECONNREFUSED 127.0.0.1:5432\n    \
             at TCPConnectWrap.afterConnect [as oncomplete] (node:net:1555:16)\n    \
             at Socket.emit (node:events:517:28)",
            "Error: connect ECONNREFUSED 127.0.0.1:5432",
        );
        // an uncaught error as node prints it: the source line, '^', the error, Node's version
        one_trace(
            "/app/index.js:10\n\
             throw new Error('boom');\n\
             ^\n\
             \n\
             Error: boom\n    \
             at Object.<anonymous> (/app/index.js:10:7)\n    \
             at Module._compile (node:internal/modules/cjs/loader:1256:14)\n    \
             at node:internal/main/run_main_module:23:47\n\
             \n\
             Node.js v20.10.0",
            "Error: boom",
        );
    }

    #[test]
    fn rust() {
        one_trace(
            "thread 'main' panicked at src/main.rs:5:5:\n\
             called `Option::unwrap()` on a `None` value\n\
             stack backtrace:\n   \
             0: rust_begin_unwind\n             \
             at /rustc/abc/library/std/src/panicking.rs:665:5\n   \
             1: core::panicking::panic_fmt\n             \
             at /rustc/abc/library/core/src/panicking.rs:74:14\n   \
             2: main::main\n             \
             at ./src/main.rs:5:5\n\
             note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.",
            "called `Option::unwrap()` on a `None` value",
        );
        // frames whose indent was removed, and the old one-line form
        one_trace(
            "thread 'main' panicked at src/main.rs:5:5:\nindex out of bounds: the len is 3 but the index is 5\nstack backtrace:\n0: rust_begin_unwind\nat /rustc/x/panicking.rs:665:5\n1: main::main",
            "index out of bounds: the len is 3 but the index is 5",
        );
        one_trace(
            "thread 'main' panicked at 'boom', src/main.rs:2:5\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            "thread 'main' panicked at 'boom', src/main.rs:2:5",
        );
    }

    #[test]
    fn rustc_errors() {
        // a compiler error and its code excerpt; the widest line number starts its line
        let t = "   Compiling demo v0.1.0 (/tmp/demo)\n\
                 error[E0308]: mismatched types\n \
                 --> src/main.rs:2:18\n  \
                 |\n\
                 2 |     let x: i32 = \"a\";\n  \
                 |            ---   ^^^ expected `i32`, found `&str`\n\
                 \n\
                 For more information about this error, try `rustc --explain E0308`.";
        assert_eq!(recs(t), vec![(0, 0, 0), (1, 6, 1), (7, 7, 7)]);
        assert!(!gutter_line(b"12|x") && !gutter_line(b"1 |x") && gutter_line(b"12 |") && gutter_line(b"7   | y"));
    }

    #[test]
    fn dotnet() {
        one_trace(
            "Unhandled exception. System.InvalidOperationException: Sequence contains no elements\n   \
             at System.Linq.ThrowHelper.ThrowNoElementsException()\n   \
             at Program.Main(String[] args) in /app/Program.cs:line 10\n \
             ---> System.IO.IOException: inner\n   \
             --- End of inner exception stack trace ---",
            "Unhandled exception. System.InvalidOperationException: Sequence contains no elements",
        );
    }

    #[test]
    fn ruby() {
        one_trace(
            "/app/lib/foo.rb:10:in `bar': undefined method `baz' for nil:NilClass (NoMethodError)\n\
             \tfrom /app/lib/foo.rb:20:in `qux'\n\
             from /app/main.rb:5:in `<main>'",
            "/app/lib/foo.rb:10:in `bar': undefined method `baz' for nil:NilClass (NoMethodError)",
        );
    }

    #[test]
    fn php() {
        one_trace(
            "[18-Nov-2026 10:15:02 UTC] PHP Fatal error:  Uncaught Exception: boom in /var/www/index.php:3\n\
             Stack trace:\n\
             #0 /var/www/index.php(10): foo()\n\
             #1 {main}\n  \
             thrown in /var/www/index.php on line 3",
            "[18-Nov-2026 10:15:02 UTC] PHP Fatal error:  Uncaught Exception: boom in /var/www/index.php:3",
        );
    }

    #[test]
    fn dart() {
        one_trace(
            "Unhandled exception:\n\
             Exception: boom\n\
             #0      main (file:///app/bin/main.dart:5:3)\n\
             #1      _delayEntrypointInvocation.<anonymous closure> (dart:isolate-patch/isolate_patch.dart:297:19)\n\
             #2      _RawReceivePort._handleMessage (dart:isolate-patch/isolate_patch.dart:184:12)",
            "Exception: boom",
        );
        // a logged error and its stack, without the header
        one_trace("Exception: boom\n#0      main (file:///app/bin/main.dart:5:3)\n#1      run (file:///app/bin/run.dart:9:1)", "Exception: boom");
    }

    #[test]
    fn chrome_and_firefox() {
        one_trace(
            "Uncaught TypeError: Cannot read properties of undefined (reading 'x')\n    \
             at foo (http://localhost:3000/app.js:10:15)\n    \
             at HTMLButtonElement.<anonymous> (http://localhost:3000/app.js:20:5)",
            "Uncaught TypeError: Cannot read properties of undefined (reading 'x')",
        );
        one_trace(
            "TypeError: x is undefined\n\
             foo@http://localhost:3000/app.js:10:15\n\
             bar/<@http://localhost:3000/app.js:20:5\n\
             @http://localhost:3000/app.js:30:1\n\
             global code@http://localhost:3000/app.js:31:1",
            "TypeError: x is undefined",
        );
    }

    #[test]
    fn traces_inside_line_prefixes() {
        // GitHub Actions: the rules read the text after the time
        let gha = "2024-01-15T10:23:45.0000001Z Traceback (most recent call last):\n\
                   2024-01-15T10:23:45.0000002Z   File \"x.py\", line 1, in <module>\n\
                   2024-01-15T10:23:45.0000003Z ValueError: nope\n\
                   2024-01-15T10:23:45.0000004Z ##[error]Process completed with exit code 1.";
        assert_eq!(recs_in(Format::Gha, gha), vec![(0, 2, 2), (3, 3, 3)]);
        // CRI: P pieces join the next line; frames on their own lines join too
        let cri = "2026-11-18T10:15:02.1Z stdout P a long \n\
                   2026-11-18T10:15:02.1Z stdout F line\n\
                   2026-11-18T10:15:02.2Z stderr F java.lang.Exception: x\n\
                   2026-11-18T10:15:02.2Z stderr F \tat a.B.c(B.java:1)\n\
                   2026-11-18T10:15:02.3Z stdout F next";
        assert_eq!(recs_in(Format::Cri, cri), vec![(0, 1, 0), (2, 3, 2), (4, 4, 4)]);
        // Docker: a piece without its '\n' joins the next line
        let docker = r#"{"log":"part one ","stream":"stdout","time":"2026-11-18T10:15:02Z"}
{"log":"part two\n","stream":"stdout","time":"2026-11-18T10:15:02Z"}
{"log":"\tat a.B.c(B.java:1)\n","stream":"stdout","time":"2026-11-18T10:15:02Z"}
{"log":"other\n","stream":"stdout","time":"2026-11-18T10:15:03Z"}"#;
        assert_eq!(recs_in(Format::Docker, docker), vec![(0, 2, 0), (3, 3, 3)]);
        // syslog: the message is the whole line, so lines of other programs that start their
        // text with spaces stay records of their own
        let sys = "Nov 18 10:15:02 host kernel: BIOS map:\nNov 18 10:15:02 host secd[276]:  sync failure\nNov 18 10:15:03 host app[1]: ok";
        assert_eq!(recs_in(Format::Syslog, sys), vec![(0, 0, 0), (1, 1, 1), (2, 2, 2)]);
    }

    #[test]
    fn structured_events_are_records_of_their_own() {
        // JSON and logfmt lines carry their own time and fields: a message that starts with a
        // space or reads like a frame does not continue the line before
        let json = r#"{"time":"12-17 19:31:36.280","level":"I","tag":"A","msg":"start"}
{"time":"12-17 19:31:36.281","level":"I","tag":"B","msg":" getSeekBarProgress isAutoMode:true"}
{"time":"12-17 19:31:36.282","level":"E","tag":"C","msg":"at com.x.Y.z(Y.java:1)"}"#;
        assert_eq!(recs_in(Format::Json, json), vec![(0, 0, 0), (1, 1, 1), (2, 2, 2)]);
        let logfmt = "time=2026-11-18T10:15:02Z level=info msg=start a=1\ntime=2026-11-18T10:15:03Z level=info msg=\" indented\" a=2";
        assert_eq!(recs_in(Format::Logfmt, logfmt), vec![(0, 0, 0), (1, 1, 1)]);
        // raw lines after an event (a crash printed to stderr) still follow the record rules
        let mixed = "{\"level\":\"info\",\"msg\":\"serving\"}\nError: boom\n    at f (/a.js:1:2)";
        assert_eq!(recs_in(Format::Json, mixed), vec![(0, 0, 0), (1, 2, 1)]);
    }

    #[test]
    fn mysql_slow_entries() {
        let t = "/usr/sbin/mysqld, Version: 8.0.35 (MySQL Community Server - GPL). started with:\n\
                 Tcp port: 3306  Unix socket: /var/run/mysqld/mysqld.sock\n\
                 Time                 Id Command    Argument\n\
                 # Time: 2026-11-18T10:15:02.123456Z\n\
                 # User@Host: app[app] @ localhost []  Id:    12\n\
                 # Query_time: 2.000123  Lock_time: 0.000010 Rows_sent: 1  Rows_examined: 1000000\n\
                 use shop;\n\
                 SET timestamp=1795003302;\n\
                 SELECT *\n\
                 FROM orders\n\
                 WHERE total > 100;\n\
                 # User@Host: app[app] @ localhost []  Id:    12\n\
                 # Query_time: 1.5  Lock_time: 0.0 Rows_sent: 0  Rows_examined: 5\n\
                 SET timestamp=1795003303;\n\
                 UPDATE stock SET n = n - 1 WHERE id = 7;";
        assert_eq!(recs_in(Format::MysqlSlow, t), vec![(0, 2, 0), (3, 10, 8), (11, 14, 14)]);
    }

    #[test]
    fn crash_reports_are_one_record() {
        let ips = "{\"app_name\":\"MyApp\",\"bug_type\":\"309\"}\n{\n  \"exception\" : {\"type\":\"EXC_BREAKPOINT\"},\n  \"threads\" : []\n}";
        assert_eq!(recs_in(Format::Crash, ips), vec![(0, 4, 0)]);
        let tomb = "*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***\n\
                    Build fingerprint: 'google/x/y:14/AB/1:user/release-keys'\n\
                    Revision: '0'\n\
                    pid: 1234, tid: 1234, name: main  >>> com.example.app <<<\n\
                    signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0";
        assert_eq!(recs_in(Format::Crash, tomb), vec![(0, 4, 3)]);
    }

    #[test]
    fn limits_and_near_misses() {
        // a record ends after 1,000 lines
        let mut t = String::from("start");
        for _ in 0..2499 {
            t.push_str("\n  more");
        }
        assert_eq!(recs(&t), vec![(0, 999, 0), (1000, 1999, 1000), (2000, 2499, 2000)]);
        // blank lines stay with the record before them
        assert_eq!(recs("a\n\n\nb"), vec![(0, 2, 0), (3, 3, 3)]);
        // lines that only look like frames
        assert_eq!(recs("Results:\n1: alpha\n2: beta"), vec![(0, 0, 0), (1, 1, 1), (2, 2, 2)]);
        assert_eq!(recs("#1 [internal] load build definition\n#1 transferring 2B done\n#2 [internal] load .dockerignore"), vec![(0, 0, 0), (1, 1, 1), (2, 2, 2)]);
        assert_eq!(recs("done\nat least one test failed\nat 10:00 the job ran\nfrom the database"), vec![(0, 0, 0), (1, 1, 1), (2, 2, 2), (3, 3, 3)]);
        assert_eq!(recs("send mail to bob@example.com:25:1 now"), vec![(0, 0, 0)]);
        // JSON lines without a message text each start a record
        let lines: Vec<&[u8]> = vec![br#"{"a":1}"#, br#"{"b":2}"#];
        assert_eq!(split(&Profile::new(Format::Json), &lines, &clock()).len(), 2);
    }

    #[test]
    fn csv_rows_with_line_ends() {
        let t = "time,msg\n2026-11-18T10:15:02Z,\"two\nlines\"\n2026-11-18T10:15:03Z,one";
        let p = detect("x.csv", t.as_bytes());
        let lines: Vec<&[u8]> = t.lines().map(str::as_bytes).collect();
        let r: Vec<_> = split(&p, &lines, &clock()).iter().map(|r| (r.first, r.last)).collect();
        assert_eq!(r, vec![(0, 0), (1, 2), (3, 3)]);
    }

    #[test]
    fn steps_one_line_at_a_time() {
        let mut r = Records::new(Format::Plain);
        assert_eq!(r.push(Some(b"Traceback (most recent call last):"), false), Step { start: true, key: true });
        assert_eq!(r.push(Some(b"  File \"a.py\", line 1, in <module>"), false), Step { start: false, key: false });
        assert_eq!(r.push(Some(b"ValueError: x"), false), Step { start: false, key: true });
        assert!(r.is_trace());
        assert_eq!(r.len(), 3);
        assert_eq!(r.push(Some(b"next"), false), Step { start: true, key: true });
        assert!(!r.is_trace());
    }
}
