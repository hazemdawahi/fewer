//! Developer commands for the checks (checks/run_check.py and checks/sessions_check.py), as
//! json-check is for checks/json_check.py. Not for people: they are not in the help.
//!
//!   scan-capture FILE SIZE TEXT [FROM]   each line of FILE's first SIZE bytes that holds
//!                                        TEXT, from line FROM on: "<line>\t<bytes>" (the
//!                                        line without its '\n'), through capture::scan, as
//!                                        find.rs searches a run in progress; then
//!                                        "# nl <n> part <0|1>"
//!   capture-lines FILE SIZE FIRST LAST   lines FIRST to LAST of FILE's first SIZE bytes,
//!                                        exact bytes (capture::lines, as fetch.rs would)
//!   hide ["PREFIX CLASS MIN"...]         stdin with secrets hidden (secrets::hide) to stdout,
//!                                        with the extra shapes given (as the config's
//!                                        `secret = ...` lines); the count to stderr: "hidden <n>"
//!   sweep-serve MS                       the MCP server's sessions sweep (sessions::Sweeper)
//!                                        for MS ms, as `fewer mcp` runs it, its log on
//!                                        stderr; then stopped (waiting for the piece being
//!                                        saved)
//!   rebuild-keys DIR                     a project folder's keys.cat and moments.jsonl made
//!                                        again from its runs' catalogs, under its lock, as
//!                                        `memory forget` makes them (checks/memory_check.py)

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::{capture, secrets, sessions, store};

fn num(a: Option<&OsString>) -> Option<u64> {
    a?.to_str()?.parse().ok()
}

fn usage() -> i32 {
    eprintln!("usage: scan-capture FILE SIZE TEXT [FROM] | capture-lines FILE SIZE FIRST LAST | hide [\"PREFIX CLASS MIN\"...] | sweep-serve MS | rebuild-keys DIR");
    2
}

/// `rebuild-keys DIR`: DIR's keys.cat and moments.jsonl made again from its runs' catalogs,
/// under its project lock (guard::rebuild_locked, as `memory forget` does).
/// `short-sizes [--commands]`: the size of `run --short`'s view of many outputs, for the
/// measurement on real sessions:
/// stdin holds records, each an 8-byte little-endian length and that many bytes of an output
/// (with --commands, each output after a record of its command); stdout gets one line per
/// output, "<bytes of the output> <bytes of its view>" (with --commands, then 1 when the Bash
/// hook would shorten it, else 0). Nothing is written but the numbers.
pub fn short_sizes(a: Vec<OsString>) -> i32 {
    let commands = match a.iter().map(|x| x.to_str()).collect::<Vec<_>>()[..] {
        [] => false,
        [Some("--commands")] => true,
        _ => {
            eprintln!("usage: fewer short-sizes [--commands] < records");
            return 2;
        }
    };
    let tags = crate::catalog::Tags { name: "cmd".into(), file_ms: Some(0), zone: None, arrivals: None, errs: Vec::new(), files: Vec::new() };
    let mut input = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    // one record, or None at the end of input
    let record = |input: &mut dyn std::io::Read| -> Result<Option<Vec<u8>>, String> {
        let mut len = [0u8; 8];
        match input.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
        let mut b = vec![0u8; u64::from_le_bytes(len) as usize];
        input.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(Some(b))
    };
    loop {
        let command = if commands {
            match record(&mut input) {
                Ok(Some(c)) => Some(c),
                Ok(None) => return 0,
                Err(e) => {
                    eprintln!("fewer short-sizes: {e}");
                    return 1;
                }
            }
        } else {
            None
        };
        let b = match record(&mut input) {
            Ok(Some(b)) => b,
            Ok(None) if command.is_none() => return 0,
            Ok(None) => {
                eprintln!("fewer short-sizes: a command without its output");
                return 1;
            }
            Err(e) => {
                eprintln!("fewer short-sizes: {e}");
                return 1;
            }
        };
        let v = crate::short::view("cmd", "r1", "exit 0", Some(1.0), &b, &tags, 0);
        let line = match &command {
            Some(c) => format!("{} {} {}", b.len(), v.len(), u8::from(crate::hook::worth_shortening(&String::from_utf8_lossy(c), &b))),
            None => format!("{} {}", b.len(), v.len()),
        };
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            return 1;
        }
    }
}

pub fn rebuild_keys(a: Vec<OsString>) -> i32 {
    let (Some(dir), None) = (a.first().map(PathBuf::from), a.get(1)) else { return usage() };
    let r = store::Lock::take(&dir).and_then(|_l| crate::guard::rebuild_locked(&dir));
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("rebuild-keys: {}: {e}", dir.display());
            1
        }
    }
}

pub fn scan_capture(a: Vec<OsString>) -> i32 {
    let (Some(f), Some(size), Some(text)) = (a.first(), num(a.get(1)), a.get(2)) else { return usage() };
    let from = num(a.get(3)).unwrap_or(1);
    let needle = text.as_encoded_bytes().to_vec();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let r = capture::scan(&PathBuf::from(f), size, from, &mut |first, t| {
        for (i, l) in t.split_inclusive(|&c| c == b'\n').enumerate() {
            let line = first + i as u64;
            let body = l.strip_suffix(b"\n").unwrap_or(l);
            if line >= from && (needle.is_empty() || body.windows(needle.len()).any(|w| w == needle)) {
                let _ = write!(out, "{line}\t");
                let _ = out.write_all(body);
                let _ = out.write_all(b"\n");
            }
        }
        Ok(true)
    });
    match r {
        Ok(s) => {
            let _ = writeln!(out, "# nl {} part {}", s.nl, s.part as u8);
            0
        }
        Err(e) => {
            eprintln!("scan-capture: {e}");
            1
        }
    }
}

pub fn capture_lines(a: Vec<OsString>) -> i32 {
    let (Some(f), Some(size), Some(lo), Some(hi)) = (a.first(), num(a.get(1)), num(a.get(2)), num(a.get(3))) else { return usage() };
    match capture::lines(&PathBuf::from(f), size, &[(lo, hi)]) {
        Ok(v) => {
            let mut out = std::io::stdout().lock();
            for (_, l) in v {
                let _ = out.write_all(&l);
            }
            0
        }
        Err(e) => {
            eprintln!("capture-lines: {e}");
            1
        }
    }
}

pub fn hide(a: Vec<OsString>) -> i32 {
    // extra shapes, as the config's `secret = PREFIX CLASS MIN` lines give them
    let mut extras = Vec::new();
    for spec in &a {
        match spec.to_str().map(secrets::Extra::parse) {
            Some(Ok(x)) => extras.push(x),
            _ => return usage(),
        }
    }
    let mut text = Vec::new();
    if std::io::stdin().read_to_end(&mut text).is_err() {
        return 1;
    }
    let (out, counts) = secrets::hide(&text, &extras);
    let _ = std::io::stdout().write_all(&out);
    eprintln!("hidden {}", counts.total());
    0
}

pub fn sweep_serve(a: Vec<OsString>) -> i32 {
    let Some(ms) = num(a.first()) else {
        eprintln!("usage: sweep-serve MS");
        return 2;
    };
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("sweep-serve: {e}");
            return 2;
        }
    };
    let s = sessions::Sweeper::start(home, Box::new(|m| eprintln!("{m}")));
    std::thread::sleep(std::time::Duration::from_millis(ms));
    s.stop(std::time::Duration::from_secs(60));
    0
}
