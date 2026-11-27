//! `run --short`: what Claude reads of a command it ran through fewer (any command, not only
//! logs). The whole output is saved as a run, as
//! `run` always saves it; Claude gets a short view in its place: the exit status, the problem
//! lines of test runners and compilers (errors, failed tests, exceptions, panics; lines alike
//! counted together, in the order they first came, each with its reference), the warnings in
//! brief and the last lines. A test suite or a build that prints thousands of lines comes as a
//! few dozen, and nothing is cut out of the middle unseen: the rest is one call away (search,
//! errors or lines with the run). An output of a few lines is shown whole.

use crate::catalog::{self, Tags};
use crate::formats::Level;
use std::collections::HashMap;

/// An output this small is shown whole (lines, bytes).
const WHOLE_LINES: usize = 40;
const WHOLE_BYTES: usize = 4_000;
/// Different problem lines shown (past PROBLEMS_ALL of them, the first PROBLEMS_FIRST and the
/// rest by kind); warning kinds shown; last lines shown; bytes of a shown line.
const PROBLEMS_ALL: usize = 20;
const PROBLEMS_FIRST: usize = 15;
const REST_KINDS: usize = 3;
/// Summary lines shown (before the last lines).
const SUMMARIES: usize = 6;
/// Failed tests shown in full (each block of a test runner's report), and lines of each: Claude
/// reads what each failure says in the first call (a failing-tests job took a second call to
/// read them, 2026-11-24).
const FAILURES: usize = 5;
const FAILURE_LINES: usize = 14;
const WARN_KINDS: usize = 3;
const LAST_LINES: usize = 6;
const LINE_BYTES: usize = 200;

/// The run as Claude sees it after `run --short`: `what` is the command, `run` its reference
/// (r12), `status` "exit 101" (or "signal 9"), `text` the saved output (secrets hidden).
pub fn view(what: &str, run: &str, status: &str, secs: Option<f64>, text: &[u8], tags: &Tags, file_ms: i64) -> String {
    let lines: Vec<&[u8]> = split_lines(text);
    let after = secs.map(|s| format!(" after {}", seconds(s))).unwrap_or_default();
    // (a command of several lines on one line, its line ends written \n, release check)
    let what = what.replace('\n', "\\n");
    let mut out = format!("{what}: {status}{after}; {} saved as {run}", plural(lines.len() as u64, "line"));
    if lines.len() <= WHOLE_LINES && text.len() <= WHOLE_BYTES {
        out.push_str(", all shown:");
        for l in &lines {
            out.push('\n');
            out.push_str(&shown(l, usize::MAX));
        }
        return out;
    }
    // the failed tests' own blocks, shown in full below; their problem lines are not listed twice,
    // nor the lines that only restate them (unittest's "test_x (m.C) ... ERROR"), nor those the
    // last lines show (2026-11-24: about 200 tokens of a failing-tests job)
    let blocks = failure_blocks(&lines);
    let shown_blocks: Vec<(usize, usize)> = blocks.iter().take(FAILURES).copied().collect();
    let inside = |i: usize| shown_blocks.iter().any(|&(a, b)| i >= a && i < b);
    let ids: Vec<Vec<u8>> = shown_blocks.iter().filter_map(|&(a, _)| head_id(lines[a])).collect();
    let restates = |l: &[u8]| ids.iter().any(|id| l.windows(id.len()).any(|w| w == id.as_slice()));
    let tail: Vec<usize> = {
        let mut v: Vec<usize> = (0..lines.len()).rev().filter(|&i| !lines[i].trim_ascii().is_empty() && !separator(lines[i])).take(LAST_LINES).collect();
        v.reverse();
        v
    };
    let in_tail = |i: usize| tail.contains(&i);
    // paths in the folder the command ran in, shown from there (the long prefix said once less)
    let base = std::env::current_dir().ok().and_then(|d| d.to_str().map(|s| format!("{}/", s.trim_end_matches('/')))).filter(|b| b.len() > 2);
    let shown = |l: &[u8], max: usize| -> String {
        let s = shown(l, max);
        match &base {
            Some(b) if s.contains(b.as_str()) => s.replace(b.as_str(), ""),
            _ => s,
        }
    };
    // the problem lines: each different one once (with how many times it came), in the order
    // they came; past PROBLEMS_ALL of them, the first PROBLEMS_FIRST and the rest by kind
    let mut seen: Vec<(usize, u64)> = Vec::new();
    let mut at: HashMap<&[u8], usize> = HashMap::new();
    let mut problems = 0u64;
    let mut covered = 0u64;
    for (i, l) in lines.iter().enumerate() {
        if problem(l) && (inside(i) || in_tail(i) || restates(l)) {
            problems += 1;
            covered += 1;
            continue;
        }
        if problem(l) {
            problems += 1;
            let k = *at.entry(l).or_insert_with(|| {
                seen.push((i, 0));
                seen.len() - 1
            });
            seen[k].1 += 1;
        }
    }
    if seen.is_empty() && covered == 0 {
        out.push_str("\nno error, failure or exception lines");
    } else if seen.is_empty() {
        out.push_str(&format!("\nproblems: {}, all shown below", plural(problems, "line")));
    } else {
        let below = if covered > 0 { format!(" ({} more shown below)", group(covered)) } else { String::new() };
        out.push_str(&format!("\nproblems: {}, {} different{below}, in the order they came:", plural(problems, "line"), group(seen.len() as u64)));
        let first = if seen.len() <= PROBLEMS_ALL { seen.len() } else { PROBLEMS_FIRST };
        for &(i, n) in &seen[..first] {
            let times = if n > 1 { format!("{} × ", group(n)) } else { String::new() };
            out.push_str(&format!("\n  {} {times}{}", i + 1, shown(lines[i], LINE_BYTES)));
        }
        if first < seen.len() {
            // the rest by message key, the most first
            let mut kinds: Vec<(u64, usize)> = Vec::new();
            let mut ki: HashMap<Vec<u8>, usize> = HashMap::new();
            let mut rest = 0u64;
            for &(i, n) in &seen[first..] {
                rest += n;
                let k = *ki.entry(crate::msgkey::line_key(lines[i])).or_insert_with(|| {
                    kinds.push((0, i));
                    kinds.len() - 1
                });
                kinds[k].0 += n;
            }
            kinds.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let top: Vec<String> = kinds.iter().take(REST_KINDS).map(|(n, i)| format!("{} × like {} {}", group(*n), i + 1, shown(lines[*i], 120))).collect();
            out.push_str(&format!("\n  and {} more in {}; the most: {}", plural(rest, "line"), plural(kinds.len() as u64, "kind"), top.join("; ")));
        }
    }
    // each failed test's block, whole (up to FAILURE_LINES: its first line, then its last lines,
    // where the error and its place are), separator lines left out
    if !shown_blocks.is_empty() {
        let more = if blocks.len() > FAILURES { format!(" (the first {FAILURES} of {})", blocks.len()) } else { String::new() };
        out.push_str(&format!("\nfailures in full{more}:"));
        for &(a, b) in &shown_blocks {
            let keep: Vec<usize> = (a..b).filter(|&i| !separator(lines[i]) && !lines[i].trim_ascii().is_empty() && lines[i].trim_ascii() != b"Traceback (most recent call last):").collect();
            let pick: Vec<Option<usize>> = if keep.len() <= FAILURE_LINES {
                keep.iter().map(|&i| Some(i)).collect()
            } else {
                std::iter::once(Some(keep[0])).chain(std::iter::once(None)).chain(keep[keep.len() - (FAILURE_LINES - 1)..].iter().map(|&i| Some(i))).collect()
            };
            for p in pick {
                match p {
                    Some(i) => out.push_str(&format!("\n  {} {}", i + 1, shown(lines[i], LINE_BYTES))),
                    None => out.push_str("\n  …"),
                }
            }
        }
    }
    // the last lines: the last LAST_LINES that are not empty (found above)
    let from = tail.first().copied().unwrap_or(lines.len());
    // the summary lines before the last lines (test results of each test binary, totals)
    let sums: Vec<usize> = (0..from).filter(|&i| summary(lines[i]) && !problem(lines[i])).collect();
    if !sums.is_empty() {
        out.push_str(&format!("\nsummary lines: {}", group(sums.len() as u64)));
        for &i in sums.iter().take(SUMMARIES) {
            out.push_str(&format!("\n  {} {}", i + 1, shown(lines[i], 160)));
        }
        if sums.len() > SUMMARIES {
            out.push_str(&format!("\n  and {} more", group((sums.len() - SUMMARIES) as u64)));
        }
    }
    let cat = catalog::build(&mut &text[..], tags, file_ms).ok();
    let by_level = |level: Level| -> Vec<&catalog::Key> {
        let mut v: Vec<&catalog::Key> = cat.iter().flat_map(|c| c.keys.iter()).filter(|k| k.level == level).collect();
        v.sort_by(|a, b| b.records.cmp(&a.records).then(a.first_line.cmp(&b.first_line)));
        v
    };
    let line_at = |n: u64| lines.get((n as usize).wrapping_sub(1)).copied().unwrap_or(b"");
    let warns = by_level(Level::Warn);
    if !warns.is_empty() {
        let records: u64 = warns.iter().map(|k| k.records).sum();
        let top: Vec<String> = warns.iter().take(WARN_KINDS).map(|k| format!("{} × {}", group(k.records), shown(line_at(k.first_line), 120))).collect();
        out.push_str(&format!("\nwarnings: {} in {}; the most: {}", plural(records, "record"), plural(warns.len() as u64, "kind"), top.join("; ")));
    }
    // (the lines the failures above showed are not shown again)
    let last: Vec<usize> = tail.iter().copied().filter(|&i| !inside(i)).collect();
    if !last.is_empty() {
        out.push_str("\nlast lines:");
        for i in last {
            out.push_str(&format!("\n  {} {}", i + 1, shown(lines[i], LINE_BYTES)));
        }
    }
    // (a form with no quotes or braces, which Claude Code would ask about, release check)
    out.push_str(&format!("\nthe whole output ({} lines): `{} tool lines {run}:1-40`, any A-B", lines.len(), me()));
    out
}

/// The test a failure head names, as other lines of the run name it: unittest's "FAIL: test_x
/// (m.C)" gives "test_x (m.C)" (its status line "test_x (m.C) ... FAIL"), cargo's "---- t::x
/// stdout ----" gives "t::x", Go's "--- FAIL: TestX (0.01s)" gives "TestX", pytest's "___ C.test_x
/// ___" gives "test_x" (its summary says "C::test_x"), Jest's "● a › b" gives "a › b".
fn head_id(l: &[u8]) -> Option<Vec<u8>> {
    let t = l.trim_ascii();
    let id: &[u8] = if let Some(r) = t.strip_prefix(b"FAIL: ").or_else(|| t.strip_prefix(b"ERROR: ")) {
        r
    } else if let Some(r) = t.strip_prefix(b"---- ").and_then(|r| r.strip_suffix(b" stdout ----")) {
        r
    } else if let Some(r) = t.strip_prefix(b"--- FAIL: ") {
        r.split(|&c| c == b' ').next().unwrap_or(r)
    } else if let Some(r) = t.strip_prefix("● ".as_bytes()) {
        r
    } else if t.starts_with(b"___") {
        let name = t.trim_ascii().iter().copied().skip_while(|&c| c == b'_' || c == b' ').collect::<Vec<u8>>();
        let name: Vec<u8> = name.iter().rev().copied().skip_while(|&c| c == b'_' || c == b' ').collect::<Vec<u8>>().into_iter().rev().collect();
        let last = name.rsplit(|&c| c == b'.').next().unwrap_or(&name).to_vec();
        return (last.len() >= 4).then_some(last);
    } else {
        return None;
    };
    let id = id.trim_ascii();
    (id.len() >= 4).then(|| id.to_vec())
}

/// How a command names this fewer: `fewer` when PATH finds this program, else its
/// whole path (an agent with Bash alone reads more of a run with `fewer tool`).
fn me() -> String {
    crate::savers::me()
}

/// A line of ==== or ---- (a test runner's separator), 10 characters or more.
fn separator(l: &[u8]) -> bool {
    let t = l.trim_ascii();
    t.len() >= 10 && (t.iter().all(|&c| c == b'=') || t.iter().all(|&c| c == b'-'))
}

/// Where a failed test's own report starts: unittest's "FAIL: name" and "ERROR: name", pytest's
/// "____ name ____", cargo's "---- name stdout ----", Go's "--- FAIL: name", Jest's "● name".
fn failure_head(l: &[u8]) -> bool {
    let t = l.trim_ascii();
    let pytest = t.len() > 10 && t.starts_with(b"___") && t.ends_with(b"___") && t.iter().any(|c| c.is_ascii_alphanumeric());
    t.starts_with(b"FAIL: ") || t.starts_with(b"ERROR: ") || pytest || (t.starts_with(b"---- ") && t.ends_with(b" stdout ----")) || t.starts_with(b"--- FAIL: ") || t.starts_with("● ".as_bytes())
}

/// Where a failed test's report ends (not taken in): a ==== line (pytest's "=== short test
/// summary info ===" too), unittest's "Ran N tests",
/// cargo's "failures:" and "test result:", Jest's "Test Suites:", Go's package FAIL or ok.
fn failure_end(l: &[u8]) -> bool {
    let t = l.trim_ascii();
    t.starts_with(b"==========") || (t.len() > 6 && t.starts_with(b"===") && t.ends_with(b"===")) || (t.starts_with(b"Ran ") && t.get(4).is_some_and(u8::is_ascii_digit)) || t == b"failures:" || t.starts_with(b"test result:") || t.starts_with(b"Test Suites:") || t.starts_with(b"FAIL\t") || t == b"FAIL" || t.starts_with(b"ok  ") || t.starts_with(b"ok \t")
}

/// The failed tests' own blocks: (first line, end), in the order they came.
fn failure_blocks(lines: &[&[u8]]) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if failure_head(lines[i]) {
            let mut j = i + 1;
            while j < lines.len() && !failure_head(lines[j]) && !failure_end(lines[j]) {
                j += 1;
            }
            v.push((i, j));
            i = j;
        } else {
            i += 1;
        }
    }
    v
}

/// A line test runners and compilers write for a problem: an error, a failed test, an
/// exception, a panic. Its start (spaces left out): error / Error / ERROR (with ":" or "[",
/// or a space), FAIL, FAILED, FAILURE, [ERROR], npm ERR!, panic:, fatal:, E (pytest's "E  "),
/// ✕, ✗, ●, assertion failed, assertion `; or within it: ": error", ": fatal error", " error TS" with digits, "panicked at",
/// " ... FAIL", " ... FAILED", " ... ERROR", "--- FAIL", "Traceback (most recent call last)",
/// "Exception in thread", "BUILD FAILURE", "Error 1" (make); or a line that is an exception:
/// a name ending in Error or Exception, then ':' or the end (AssertionError: 1 != 2); or a log
/// line whose level is an error, written in capitals, after its time or thread (log_error).
pub fn problem(l: &[u8]) -> bool {
    let t = l.trim_ascii_start();
    if log_error(t) {
        return true;
    }
    const STARTS: [&[u8]; 20] = [
        b"error:", b"error[", b"Error:", b"ERROR:", b"ERROR ", b"FAIL ", b"FAIL:", b"FAIL\t", b"FAILED", b"FAILURE", b"[ERROR]", b"npm ERR!", b"panic:", b"fatal:",
        "✕".as_bytes(), "✗".as_bytes(), "●".as_bytes(), b"E   ", b"assertion failed", b"assertion `",
    ];
    if STARTS.iter().any(|p| t.starts_with(p)) || t == b"FAIL" {
        return true;
    }
    const WITHIN: [&[u8]; 13] = [
        b": error", b": fatal error", b"panicked at", b" ... FAIL", b" ... FAILED", b" ... ERROR", b"--- FAIL", b"Traceback (most recent call last)", b"Exception in thread", b"BUILD FAILURE", b"] Error 1", b" error TS", b"  error  ",
    ];
    // (each as a word: ": errors come back" is not one)
    let word_at = |p: &[u8]| (0..(t.len() + 1).saturating_sub(p.len())).any(|i| &t[i..i + p.len()] == p && !t.get(i + p.len()).is_some_and(u8::is_ascii_alphabetic));
    if WITHIN.iter().any(|p| word_at(p)) {
        return true;
    }
    // an exception line: Name(Error|Exception) then ':' or the end
    let name_end = t.iter().position(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')).unwrap_or(t.len());
    let name = &t[..name_end];
    (name.ends_with(b"Error") || name.ends_with(b"Exception")) && name.len() > 5 && name[0].is_ascii_alphabetic() && (name_end == t.len() || t[name_end] == b':')
}

/// A log line whose level is an error, as the errors tool reads levels (formats::text_level):
/// the first level word among its first 6 words is ERROR, FATAL, CRITICAL, SEVERE, ... written
/// in capitals ("2026-11-25 10:00:01 ERROR payment failed", "10:00:01.123 [main] ERROR
/// com.app.Main"; not "handle the error", nor "warning: ... ERROR").
fn log_error(t: &[u8]) -> bool {
    let w = &t[..t.len().min(160)];
    let free = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'/');
    let (mut i, mut words) = (0, 0);
    while i < w.len() && words < 6 {
        if !w[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let a = i;
        while i < w.len() && w[i].is_ascii_alphabetic() {
            i += 1;
        }
        let before = if a == 0 { b' ' } else { w[a - 1] };
        let after = w.get(i).copied().unwrap_or(b' ');
        if free(before) && free(after) && after != b'=' {
            words += 1;
            if let Some(l) = crate::formats::level_word(&w[a..i]) {
                return l == crate::formats::Level::Error && w[a..i].iter().all(u8::is_ascii_uppercase);
            }
        }
    }
    false
}

/// A line test runners and build tools write as a summary: "test result: ok. 286 passed",
/// "Tests: 2 failed, 39 passed", "Ran 120 tests in 0.2s", "==== 3 failed, 10 passed ====",
/// "OK", "BUILD SUCCESS", "Finished …", "Done in 3.2s", "ok  pkg 0.01s" (Go); or a short line
/// with a number right before passed, failed, tests, errors or warnings (not "running 286
/// tests").
pub fn summary(l: &[u8]) -> bool {
    let t = l.trim_ascii();
    const STARTS: [&[u8]; 13] = [b"test result:", b"Tests:", b"Test Suites:", b"Ran ", b"OK", b"FAILED (", b"BUILD SUCCESS", b"BUILD FAILED", b"Finished ", b"Done in", b"Executed ", b"ok \t", b"ok  "];
    if STARTS.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    let words = [&b" passed"[..], b" failed", b" tests", b" errors", b" warnings"];
    // (not cargo's "running 286 tests" before each test binary)
    // (each word whole and right after a number: "286 passed", "9 warnings"; not "tests/a.rs",
    // nor "the tests use it" in a line of code a compiler shows)
    let counted = |w: &[u8]| {
        (0..(t.len() + 1).saturating_sub(w.len())).any(|i| {
            &t[i..i + w.len()] == w && !t.get(i + w.len()).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'/' || *c == b'_') && i > 0 && t[i - 1].is_ascii_digit()
        })
    };
    !t.starts_with(b"running ") && t.len() < 300 && words.iter().any(|w| counted(w))
}

/// The lines of a text, without their line ends (a last line without '\n' counts).
fn split_lines(text: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = text.split(|&c| c == b'\n').collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v.into_iter().map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect()
}

/// A line as shown: escape sequences and control bytes as escapes (clean.rs), cut at `max`
/// bytes with "…".
fn shown(l: &[u8], max: usize) -> String {
    let (s, _) = crate::clean::escape(l, false);
    if max == usize::MAX {
        return s;
    }
    let (t, n) = crate::clean::cut(&s, max);
    if n > 0 { format!("{t}…") } else { s }
}

fn seconds(s: f64) -> String {
    if s < 10.0 { format!("{s:.1} s") } else { format!("{} s", s.round() as u64) }
}

fn group(n: u64) -> String {
    crate::answer::group(n)
}

fn plural(n: u64, w: &str) -> String {
    format!("{} {w}{}", group(n), if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn each_failure_in_full() {
        let mut text = String::new();
        for k in 0..60 {
            text.push_str(&format!("test_case_{k:03} (test_many.TestMany) ... ok\n"));
        }
        text.push_str("test_parse_amount (test_parse.TestParse) ... ERROR\ntest_discount_rounding (test_pricing.TestPricing) ... FAIL\n\n");
        text.push_str("======================================================================\nERROR: test_parse_amount (test_parse.TestParse)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File \"tests/test_parse.py\", line 7, in test_parse_amount\n    self.assertEqual(parse_amount(\"1,234.50\"), 1234.5)\nValueError: could not convert string to float: '1,234.50'\n\n");
        text.push_str("======================================================================\nFAIL: test_discount_rounding (test_pricing.TestPricing)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File \"tests/test_pricing.py\", line 7, in test_discount_rounding\n    self.assertEqual(apply_discount(2.30, 50), 1.15)\nAssertionError: 1.14 != 1.15\n\n----------------------------------------------------------------------\nRan 62 tests in 0.002s\n\nFAILED (failures=1, errors=1)\n");
        let v = view("python3 -m unittest", "r1", "exit 1", None, text.as_bytes(), &Tags::default(), 0);
        let full = v.split("\nfailures in full:").nth(1).unwrap_or("").split("\nlast ").next().unwrap_or("");
        assert!(full.contains("\n  65 ERROR: test_parse_amount (test_parse.TestParse)\n  68   File \"tests/test_parse.py\", line 7, in test_parse_amount\n  69     self.assertEqual(parse_amount(\"1,234.50\"), 1234.5)\n  70 ValueError: could not convert string to float: '1,234.50'\n  73 FAIL: test_discount_rounding"), "{v}");
        assert!(full.contains("AssertionError: 1.14 != 1.15") && !full.contains("Ran 62"), "{v}");
        // the problem lines inside them are not listed twice, nor the status lines that restate
        // them, nor the last lines; the last lines the failures showed are not shown again, nor
        // separators; the traceback's first line is left out
        assert!(v.contains("\nproblems: 9 lines, all shown below\nfailures in full:\n"), "{v}");
        assert!(v.contains("\nlast lines:\n  81 Ran 62 tests in 0.002s\n  83 FAILED (failures=1, errors=1)\n") && !v.contains("Traceback"), "{v}");
        // pytest's and cargo's heads; a long block keeps its first line and its end
        let py: Vec<&[u8]> = vec![b"____ test_a ____", b"x", b"E   assert 1 == 2", b"t.py:3: AssertionError", b"=== short test summary info ==="];
        assert_eq!(failure_blocks(&py), [(0, 4)]);
        let cargo: Vec<&[u8]> = vec![b"---- tests::a stdout ----", b"thread 'tests::a' panicked at src/lib.rs:3:5:", b"", b"failures:", b"    tests::a"];
        assert_eq!(failure_blocks(&cargo), [(0, 3)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> Tags {
        Tags { name: "cargo test".into(), file_ms: Some(1_763_900_000_000), zone: None, arrivals: None, errs: Vec::new(), files: Vec::new() }
    }

    #[test]
    fn a_small_output_is_shown_whole() {
        let v = view("ls", "r3", "exit 0", Some(0.04), b"a.txt\nb.txt\n", &tags(), 1_763_900_000_000);
        assert_eq!(v, "ls: exit 0 after 0.0 s; 2 lines saved as r3, all shown:\na.txt\nb.txt");
    }

    #[test]
    fn a_long_output_comes_as_errors_warnings_and_its_end() {
        let mut t = String::new();
        for i in 0..300 {
            t.push_str(&format!("test tests::case_{i} ... ok\n"));
        }
        t.push_str("thread 'tests::bad' panicked at src/lib.rs:12:5:\nassertion failed: x == 2\n");
        for i in 0..3 {
            t.push_str(&format!("error: could not compile `app{i}` (lib) due to 2 previous errors\n"));
        }
        t.push_str("warning: unused variable: `y`\n");
        t.push_str("test result: FAILED. 300 passed; 1 failed; 0 ignored\n");
        let v = view("cargo test", "r12", "exit 101", Some(41.4), t.as_bytes(), &tags(), 1_763_900_000_000);
        assert!(v.starts_with("cargo test: exit 101 after 41 s; 307 lines saved as r12\nproblems: 5 lines, 1 different (4 more shown below), in the order they came:"), "{v}");
        assert!(v.contains("\n  303 error: could not compile `app0` (lib) due to 2 previous errors\n  304 error: could not compile `app1`"), "{v}");
        assert!(v.contains("\nwarnings: 1 record in 1 kind; the most: 1 × warning: unused variable: `y`"), "{v}");
        assert!(v.contains("\n  307 test result: FAILED. 300 passed; 1 failed; 0 ignored"), "{v}");
        assert!(v.contains("\n  301 thread 'tests::bad' panicked at src/lib.rs:12:5:"), "{v}");
        assert!(v.ends_with(&format!("\nlast lines:\n  302 assertion failed: x == 2\n  303 error: could not compile `app0` (lib) due to 2 previous errors\n  304 error: could not compile `app1` (lib) due to 2 previous errors\n  305 error: could not compile `app2` (lib) due to 2 previous errors\n  306 warning: unused variable: `y`\n  307 test result: FAILED. 300 passed; 1 failed; 0 ignored\nthe whole output (307 lines): `{} tool lines r12:1-40`, any A-B", me())), "{v}");
        assert!(v.len() < 3_000, "{} bytes", v.len());
    }

    #[test]
    fn summary_lines() {
        for l in ["test result: ok. 286 passed; 0 failed; 10 ignored", "Tests:       2 failed, 39 passed, 41 total", "Ran 1200 tests in 0.031s", "OK", "======= 3 failed, 10 passed in 0.12s =======", "warning: `app` (lib) generated 12 warnings", "    Finished `release` profile [optimized] target(s) in 4.25s", "ok  \tgithub.com/x/y\t0.012s"] {
            assert!(summary(l.as_bytes()), "{l}");
        }
        for l in ["test tools::x ... ok", "running 286 tests", "the tests passed before", "     Running tests/add_cli.rs (target/release/deps/add_cli-890cbeb3)", "OKAY"] {
            assert_eq!(summary(l.as_bytes()), l == "OKAY", "{l}");
        }
    }

    #[test]
    fn problem_lines() {
        for l in [
            "error[E0308]: mismatched types",
            "error: could not compile `app`",
            "src/a.ts(42,7): error TS2322: Type 'string' is not assignable",
            "src/a.c:12:5: error: expected ';'",
            "thread 'main' panicked at src/main.rs:3:5:",
            "test tools::tests::x ... FAILED",
            "test_case_017 (__main__.Cart) ... FAIL",
            "FAIL: test_case_017 (__main__.Cart)",
            "ERROR: test_case_091 (__main__.Cart)",
            "Traceback (most recent call last):",
            "AssertionError: 59 != 60 : total off by one",
            "KeyError: 'price_91'",
            "java.lang.IllegalStateException: closed",
            "--- FAIL: TestParse (0.00s)",
            "FAIL\tgithub.com/x/y\t0.01s",
            "  ✕ keeps the email (31 ms)",
            "E       assert 2 == 3",
            "npm ERR! code ELIFECYCLE",
            "[ERROR] BUILD FAILURE",
            "  88:5   error  'user' is assigned a value but never used  no-unused-vars",
            "make: *** [all] Error 1",
            "assertion `left == right` failed",
            "java.lang.NullPointerException",
            // a log's error level after its time or thread
            "2026-11-25 10:00:01 ERROR payment failed: connection refused",
            "[2026-11-25T10:00:01Z] ERROR: db connection refused",
            "10:00:01.123 [main] ERROR com.app.Main - Failed to start",
            "line 150 ERROR: test_divide failed",
            "2026-11-25T10:00:02Z FATAL out of memory",
        ] {
            assert!(problem(l.as_bytes()), "{l}");
        }
        for l in ["2026-11-25 10:00:01 INFO served ERROR page", "x = handle(error)", "warning: ERROR is deprecated", "2026-11-25 10:00:01 error budget left: 3"] {
            assert!(!problem(l.as_bytes()), "{l}");
        }
        for l in [
            "test tools::tests::x ... ok",
            "help: ... or use `try_from` and handle the error accordingly",
            "101 -             && let Err(e) = write_all_fd(1, data)",
            "warning: unused variable: `y`",
            "0 errors, 2 warnings",
            "    = note: the error is reported here",
            "Errors: none",
            "class MyError(Exception):",
            "6 | //! Every call is a thin wrapper: errors come back as io::Error, and",
        ] {
            assert!(!problem(l.as_bytes()), "{l}");
        }
    }
}
