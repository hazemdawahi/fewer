//! A `run`'s capture, in $FEWER_HOME/raw/<pid>/:
//!
//!   r000045.out    the command's output with secrets already hidden: exactly the bytes the
//!                  run's archive will hold, so a reference to line L of a run in progress
//!                  still names line L once the run is packed. Written as whole lines.
//!   r000045.err    pipes mode: the lines stderr wrote, one "first count" line per range
//!                  (lines counted from 1, as references count them; ranges that touch
//!                  may be written apart, and readers merge them)
//!   r000045.times  arrival marks, one "line ms" line each: from that line on, lines arrived
//!                  `ms` after the run's start (written when 100 ms have passed since the
//!                  last mark; output with times of its own does not need them)
//!
//! The writer (`run`) holds an exclusive lock on the .out file while it lives, so readers
//! and pack-pending can tell a run in progress from one whose writer is gone, even after a
//! kill -9 (the system drops the lock). A capture stops growing at 4 GiB: the
//! output still reaches the terminal, and the run's note says so.
//!
//! The readers: `scan` (find.rs: search a run in progress), `lines` (fetch.rs: lines by
//! number), `read_marks` and `read_errs` (the catalog's `Tags`), `writer_alive`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use crate::portable::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// A capture stops growing at this size.
pub const MAX_BYTES: u64 = 4 << 30;

/// An arrival mark is written when this many milliseconds have passed since the last one.
pub const MARK_MS: u64 = 100;

/// The runs of whole lines `scan` gives at a time: about this many bytes.
pub const SCAN_RUN: usize = 1 << 20;

/// The folder of a project's captures.
pub fn raw_dir(home: &Path, pid: &str) -> PathBuf {
    home.join("raw").join(pid)
}

/// A capture's file names: r000045.out, .err, .times (6 digits, as the store's files).
pub fn out_file(id: u64) -> String {
    format!("r{id:06}.out")
}
pub fn err_file(id: u64) -> String {
    format!("r{id:06}.err")
}
pub fn times_file(id: u64) -> String {
    format!("r{id:06}.times")
}

/// The run id of a capture's file name (r000045.out gives 45).
pub fn capture_id(name: &str) -> Option<u64> {
    let digits = name.strip_prefix('r')?.strip_suffix(".out")?;
    (digits.len() >= 6 && digits.bytes().all(|b| b.is_ascii_digit())).then(|| digits.parse().ok()).flatten().filter(|&i| i > 0)
}

fn count_nl(b: &[u8]) -> u64 {
    b.iter().filter(|&&c| c == b'\n').count() as u64
}

// ---------------------------------------------------------------- writing (`run`)

/// What a capture holds at its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub bytes: u64,
    /// lines as grep counts them: the '\n' bytes, and one more when the last line has none
    pub lines: u64,
    /// why saving stopped early ("saving stopped at 4 GiB", a write error), if it did
    pub note: Option<String>,
}

/// The writer of one capture. It is given whole lines (what a secrets::Stream lets out),
/// from stdout or stderr, in the order they came.
pub struct Writer {
    dir: PathBuf,
    id: u64,
    out: File,
    err: Option<File>,
    times: Option<File>,
    t0: Instant,
    last_mark: u64,
    last_err_flush: u64,
    bytes: u64,
    nl: u64,
    /// the last byte written is '\n', or nothing is written yet
    at_line_start: bool,
    /// the last bytes of the line not ended yet came from stderr
    part_from_err: bool,
    /// stderr lines not yet in the .err file: (first line, count)
    pend: Option<(u64, u64)>,
    max: u64,
    stopped: Option<String>,
}

impl Writer {
    /// A new capture for run `id` in `dir` (the project's raw folder), locked for this
    /// writer. `t0` is the run's start, from which arrival marks count.
    pub fn create(dir: &Path, id: u64, t0: Instant) -> io::Result<Writer> {
        fs::create_dir_all(dir)?;
        // (Windows: read access too, which its lock wants)
        let out = OpenOptions::new().read(cfg!(windows)).append(true).create_new(true).mode(0o600).open(dir.join(out_file(id)))?;
        crate::portable::lock(&out)?;
        Ok(Writer {
            dir: dir.to_path_buf(),
            id,
            out,
            err: None,
            times: None,
            t0,
            last_mark: 0,
            last_err_flush: 0,
            bytes: 0,
            nl: 0,
            at_line_start: true,
            part_from_err: false,
            pend: None,
            max: max_bytes(),
            stopped: None,
        })
    }

    /// The path of the .out file.
    pub fn path(&self) -> PathBuf {
        self.dir.join(out_file(self.id))
    }

    /// Append `data` (whole lines, or the last part of a stream) that came from stdout, or
    /// from stderr when `from_err`.
    pub fn write(&mut self, from_err: bool, data: &[u8]) {
        if data.is_empty() || self.stopped.is_some() {
            return;
        }
        let mut data = data;
        if self.bytes + data.len() as u64 > self.max {
            // the whole lines that fit, then nothing more
            let room = (self.max - self.bytes) as usize;
            let cut = data[..room].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
            data = &data[..cut];
            self.stopped = Some(if self.max == MAX_BYTES { "saving stopped at 4 GiB".to_string() } else { format!("saving stopped at {} bytes", self.max) });
            if data.is_empty() {
                return;
            }
        }
        let ms = self.t0.elapsed().as_millis() as u64;
        if ms >= self.last_mark + MARK_MS {
            self.last_mark = ms;
            let mark = format!("{} {ms}\n", self.nl + 1);
            if !self.side(false, mark.as_bytes()) {
                return;
            }
        }
        if let Err(e) = self.out.write_all(data) {
            self.stop(&e);
            return;
        }
        let k = count_nl(data);
        if k > 0 {
            if from_err {
                self.add_err(self.nl + 1, k);
            } else {
                // a stdout line ended: the stderr lines before it are a range of their own
                self.flush_err();
            }
        }
        self.nl += k;
        self.bytes += data.len() as u64;
        self.at_line_start = data.ends_with(b"\n");
        if !self.at_line_start {
            self.part_from_err = from_err;
        }
    }

    /// Called now and then (the run's loop): stderr lines held back for 100 ms are written,
    /// so that readers of a run in progress see them.
    pub fn tick(&mut self) {
        let ms = self.t0.elapsed().as_millis() as u64;
        if self.pend.is_some() && ms >= self.last_err_flush + MARK_MS {
            self.flush_err();
        }
    }

    /// The end of the output: what the capture holds. The file stays open, and locked, until
    /// the writer is dropped, which `run` does once the run's end is recorded.
    pub fn finish(&mut self) -> Summary {
        if !self.at_line_start && self.part_from_err {
            self.add_err(self.nl + 1, 1);
        }
        self.flush_err();
        let part = !self.at_line_start && self.bytes > 0;
        Summary { bytes: self.bytes, lines: self.nl + part as u64, note: self.stopped.clone() }
    }

    fn add_err(&mut self, first: u64, count: u64) {
        match &mut self.pend {
            Some((f, c)) if *f + *c == first => *c += count,
            _ => {
                self.flush_err();
                self.pend = Some((first, count));
            }
        }
    }

    fn flush_err(&mut self) {
        if let Some((f, c)) = self.pend.take() {
            self.last_err_flush = self.t0.elapsed().as_millis() as u64;
            self.side(true, format!("{f} {c}\n").as_bytes());
        }
    }

    /// Append to the .err (`err`) or .times file; false when saving stopped.
    fn side(&mut self, err: bool, line: &[u8]) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        let name = if err { err_file(self.id) } else { times_file(self.id) };
        let slot = if err { &mut self.err } else { &mut self.times };
        if slot.is_none() {
            match OpenOptions::new().append(true).create(true).mode(0o600).open(self.dir.join(name)) {
                Ok(f) => *slot = Some(f),
                Err(e) => {
                    self.stop(&e);
                    return false;
                }
            }
        }
        if let Some(f) = slot
            && let Err(e) = f.write_all(line)
        {
            self.stop(&e);
            return false;
        }
        true
    }

    /// A write failed (a full disk): no more saving; the command goes on.
    fn stop(&mut self, e: &io::Error) {
        if self.stopped.is_none() {
            self.stopped = Some(format!("saving stopped: {e}"));
        }
    }
}

/// The size at which a capture stops growing: 4 GiB, or FEWER_TEST_CAPTURE_MAX (a test
/// switch: a small size, so checks can reach it; never set in normal use).
fn max_bytes() -> u64 {
    std::env::var("FEWER_TEST_CAPTURE_MAX").ok().and_then(|v| v.parse().ok()).filter(|&v| v > 0).unwrap_or(MAX_BYTES)
}

// ---------------------------------------------------------------- reading

/// Whether the capture at `path` is still being written: its writer holds its lock. A
/// capture that cannot be checked counts as being written (it is never packed half way).
pub fn writer_alive(path: &Path) -> bool {
    let Ok(f) = File::open(path) else { return false };
    match crate::portable::try_lock_shared(&f) {
        Ok(()) => {
            let _ = crate::portable::unlock(&f);
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => true,
    }
}

/// Pairs of numbers, one pair per line ("a b"), from a .err or .times file: whole lines only
/// (a line cut short by a crash is left out, and so is any line that is not two numbers).
fn read_pairs(path: &Path) -> Vec<(u64, u64)> {
    let Ok(b) = fs::read(path) else { return Vec::new() };
    let complete = match b.iter().rposition(|&c| c == b'\n') {
        Some(p) => &b[..p + 1],
        None => &b[..0],
    };
    let text = String::from_utf8_lossy(complete);
    text.lines()
        .filter_map(|l| {
            let (a, c) = l.split_once(' ')?;
            Some((a.parse().ok()?, c.parse().ok()?))
        })
        .collect()
}

/// A capture's arrival marks (its .times file): (line, ms since the run's start), lines
/// counted from 1 as references count them, in order. This is the catalog's
/// `Tags::arrivals`, with the run's start.
pub fn read_marks(path: &Path) -> Vec<(u64, u64)> {
    read_pairs(path)
}

/// A capture's stderr lines (its .err file): (first line, number of lines), lines counted
/// from 1, in order, ranges that touch merged. This is the catalog's `Tags::errs`.
pub fn read_errs(path: &Path) -> Vec<(u64, u64)> {
    let mut v = read_pairs(path);
    v.retain(|r| r.0 >= 1 && r.1 >= 1);
    v.sort();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (f, c) in v {
        match out.last_mut() {
            Some((lf, lc)) if *lf + *lc >= f => *lc = (*lc).max(f + c - *lf),
            _ => out.push((f, c)),
        }
    }
    out
}

/// What `scan` gives each run of whole lines to: (the number of its first line, its bytes);
/// Ok(false) stops the scan.
pub type Each<'a> = &'a mut dyn FnMut(u64, &[u8]) -> Result<bool, String>;

/// What `scan` covered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scanned {
    /// the '\n' bytes of the runs given to `each` (when it stopped: of those before the run
    /// it stopped at), so the next line to search is nl + 1
    pub nl: u64,
    /// the last run given ends without '\n' (a line still being written, or the capture's
    /// last line): it counts as a line, as grep counts it
    pub part: bool,
    /// `each` stopped the scan
    pub stopped: bool,
}

/// Read a capture's first `size` bytes (its size when the request began) for a search, in
/// order: in runs of whole lines of about 1 MiB, each given to `each(first, text)` with the
/// number of its first line (from 1). The last run may end without '\n'. No run holds lines
/// on both sides of line `from` (a cursor; 1 = from the start), so a run that ends before it
/// can be counted without being searched. `each` returns Ok(false) to stop there (the time
/// limit, a cancel). A capture that is gone was packed meanwhile: the error says so, and the
/// caller reads the run's archive instead.
pub fn scan(path: &Path, size: u64, from: u64, each: Each) -> Result<Scanned, String> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(GONE.into()),
        Err(e) => return Err(format!("cannot read its capture: {e}")),
    };
    let mut left = size;
    let mut buf: Vec<u8> = Vec::new();
    let mut first = 1u64;
    let mut s = Scanned::default();
    let mut want = SCAN_RUN;
    loop {
        // fill to `want` bytes, or to the end of what is scanned
        while buf.len() < want && left > 0 {
            let k = buf.len();
            let n = (want - k).min(left.min(usize::MAX as u64) as usize);
            buf.resize(k + n, 0);
            let got = match f.read(&mut buf[k..]) {
                Ok(g) => g,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => 0,
                Err(e) => return Err(format!("cannot read its capture: {e}")),
            };
            buf.truncate(k + got);
            if got == 0 {
                // (shorter than its size when the request began: it was cut, so stop here)
                left = 0;
                break;
            }
            left -= got as u64;
        }
        if buf.is_empty() {
            break;
        }
        let end = left == 0;
        let cut = match buf.iter().rposition(|&c| c == b'\n') {
            _ if end => buf.len(),
            Some(p) => p + 1,
            None => {
                // one line longer than a run: read on to its end
                want = buf.len() + SCAN_RUN;
                continue;
            }
        };
        want = SCAN_RUN;
        // the run is cut where line `from` starts, when it starts inside it
        let mut parts = [(0, cut), (cut, cut)];
        if first < from {
            let mut skip = from - first;
            if let Some(p) = buf[..cut].iter().position(|&c| {
                if c == b'\n' {
                    skip -= 1;
                }
                skip == 0
            }) && p + 1 < cut
            {
                parts = [(0, p + 1), (p + 1, cut)];
            }
        }
        for (a, b) in parts {
            if a == b {
                continue;
            }
            let text = &buf[a..b];
            if !each(first, text)? {
                s.stopped = true;
                s.nl = first - 1;
                return Ok(s);
            }
            first += count_nl(text);
            s.part = !text.ends_with(b"\n");
        }
        buf.drain(..cut);
    }
    s.nl = first - 1;
    Ok(s)
}

/// The error of a capture that is gone (packed meanwhile).
pub const GONE: &str = "its capture is gone (the run was packed meanwhile)";

/// The lines in `ranges` (1-based, both ends included) of a capture's first `size` bytes, in
/// order, each with its exact bytes (its '\n' included when it has one): what fetch.rs's
/// `fetch` gives for an archive, for a run in progress. Lines past the end are not there.
pub fn lines(path: &Path, size: u64, ranges: &[(u64, u64)]) -> Result<Vec<(u64, Vec<u8>)>, String> {
    // the ranges sorted and merged
    let mut sorted: Vec<(u64, u64)> = ranges.iter().copied().filter(|r| r.0 <= r.1 && r.1 >= 1).map(|r| (r.0.max(1), r.1)).collect();
    sorted.sort();
    let mut want: Vec<(u64, u64)> = Vec::new();
    for r in sorted {
        match want.last_mut() {
            Some(l) if r.0 <= l.1.saturating_add(1) => l.1 = l.1.max(r.1),
            _ => want.push(r),
        }
    }
    let (Some(&(lo, _)), Some(&(_, hi))) = (want.first(), want.last()) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    let mut ri = 0;
    scan(path, size, lo, &mut |first, text| {
        if text.ends_with(b"\n") && first + count_nl(text) <= lo {
            // (wholly before the first wanted line)
            return Ok(true);
        }
        // a line never runs across two runs of `scan`
        let mut line = first;
        for l in text.split_inclusive(|&c| c == b'\n') {
            while ri < want.len() && want[ri].1 < line {
                ri += 1;
            }
            if ri < want.len() && want[ri].0 <= line {
                out.push((line, l.to_vec()));
            }
            line += 1;
            if line > hi {
                return Ok(false);
            }
        }
        Ok(true)
    })?;
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A fresh folder for a test, inside this crate's target folder (as store.rs's tests
    /// make theirs; never the home folder).
    pub fn tmpdir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-run-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rng(seed: u64) -> impl FnMut(usize) -> usize {
        let mut x = seed;
        move |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n.max(1) as u64) as usize
        }
    }

    #[test]
    fn names() {
        assert_eq!(out_file(42), "r000042.out");
        assert_eq!(capture_id("r000042.out"), Some(42));
        assert_eq!(capture_id("r1234567.out"), Some(1234567));
        for bad in ["r42.out", "r000042.err", "x000042.out", "r000000.out", "r00004a.out"] {
            assert_eq!(capture_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn writer_counts_lines_and_lists_stderr() {
        let d = tmpdir("writer");
        let mut w = Writer::create(&d, 7, Instant::now()).unwrap();
        assert!(writer_alive(&w.path()));
        w.write(false, b"out 1\nout 2\n");
        w.write(true, b"err 3\n");
        w.write(true, b"err 4\nerr 5\n");
        w.write(false, b"out 6\n");
        w.write(true, b"err 7\n");
        w.write(false, b"part ");
        w.write(true, b"ended by stderr 8\n");
        w.write(false, b"last, no end");
        let s = w.finish();
        assert_eq!(s, Summary { bytes: 77, lines: 9, note: None });
        assert_eq!(fs::read(d.join("r000007.out")).unwrap(), b"out 1\nout 2\nerr 3\nerr 4\nerr 5\nout 6\nerr 7\npart ended by stderr 8\nlast, no end");
        assert_eq!(read_errs(&d.join("r000007.err")), vec![(3, 3), (7, 2)]);
        assert!(writer_alive(&w.path()));
        drop(w);
        // The lock goes with the writer. Another test's child process can share the open
        // file for a moment, between its fork and its exec (the lock belongs to the open file),
        // so wait up to 2 s for the release.
        let t = Instant::now();
        while writer_alive(&d.join("r000007.out")) && t.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!writer_alive(&d.join("r000007.out")));
        assert!(!writer_alive(&d.join("r000099.out")));
        // a capture is never made twice
        assert!(Writer::create(&d, 7, Instant::now()).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_last_line_from_stderr_is_listed() {
        let d = tmpdir("errlast");
        let mut w = Writer::create(&d, 1, Instant::now()).unwrap();
        w.write(false, b"a\n");
        w.write(true, b"no end");
        assert_eq!(w.finish().lines, 2);
        assert_eq!(read_errs(&d.join(err_file(1))), vec![(2, 1)]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn arrival_marks_every_100_ms() {
        let d = tmpdir("marks");
        let t0 = Instant::now();
        let mut w = Writer::create(&d, 3, t0).unwrap();
        w.write(false, b"1\n2\n");
        std::thread::sleep(std::time::Duration::from_millis(120));
        w.write(false, b"3\n");
        w.write(false, b"4\n");
        std::thread::sleep(std::time::Duration::from_millis(120));
        w.write(false, b"5");
        w.write(false, b"\n");
        w.finish();
        let m = read_marks(&d.join(times_file(3)));
        assert_eq!(m.iter().map(|x| x.0).collect::<Vec<_>>(), vec![3, 5]);
        assert!(m[0].1 >= 100 && m[1].1 >= m[0].1 + 100, "{m:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_capture_stops_at_its_limit_on_a_line_end() {
        let d = tmpdir("limit");
        let mut w = Writer::create(&d, 2, Instant::now()).unwrap();
        w.max = 10;
        w.write(false, b"abc\ndef\n");
        w.write(false, b"gh\nij\n");
        w.write(false, b"more\n");
        let s = w.finish();
        assert_eq!((s.bytes, s.lines, s.note.as_deref()), (8, 2, Some("saving stopped at 10 bytes")));
        assert_eq!(fs::read(d.join(out_file(2))).unwrap(), b"abc\ndef\n");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pairs_files_ignore_a_cut_line() {
        let d = tmpdir("pairs");
        let p = d.join("x.err");
        fs::write(&p, "1 2\n3 1\n9 4\n10 1\nbad\n20 3\n21").unwrap();
        // (3 1 touches 1 2; 10 1 lies inside 9 4; "21" has no line end)
        assert_eq!(read_errs(&p), vec![(1, 3), (9, 4), (20, 3)]);
        assert_eq!(read_marks(&d.join("none")), vec![]);
        fs::remove_dir_all(&d).unwrap();
    }

    /// The lines `scan` gives equal the text's own lines, with their numbers, for any size,
    /// cursor and run length; no run holds lines on both sides of the cursor.
    #[test]
    fn scan_gives_every_line_once_with_its_number() {
        let d = tmpdir("scan");
        let mut rnd = rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..300 {
            let mut text = Vec::new();
            let n = rnd(60);
            for _ in 0..n {
                let len = match rnd(10) {
                    // (lines longer than a run of `scan`, in one round of 8)
                    0 if round % 8 == 0 => SCAN_RUN + rnd(3000),
                    1 => 0,
                    _ => rnd(200),
                };
                text.extend((0..len).map(|k| b'a' + (k % 26) as u8));
                if rnd(20) > 0 {
                    text.push(b'\n');
                }
            }
            let p = d.join("c.out");
            fs::write(&p, &text).unwrap();
            let size = if round % 3 == 0 { rnd(text.len() + 1) as u64 } else { text.len() as u64 };
            let all: Vec<&[u8]> = text[..size as usize].split_inclusive(|&c| c == b'\n').collect();
            let from = 1 + rnd(all.len() + 2) as u64;
            let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut joined = Vec::new();
            let s = scan(&p, size, from, &mut |first, t| {
                let nl = count_nl(t);
                // a run ends before the cursor's line, or starts at it or after it
                assert!(first >= from || first + nl < from || (first + nl == from && t.ends_with(b"\n")), "round {round}: {first} {nl} {from}");
                for (i, l) in t.split_inclusive(|&c| c == b'\n').enumerate() {
                    got.push((first + i as u64, l.to_vec()));
                }
                joined.extend_from_slice(t);
                Ok(true)
            })
            .unwrap();
            assert_eq!(joined, &text[..size as usize], "round {round}");
            // line numbers: whole lines are never split between runs
            let want: Vec<(u64, Vec<u8>)> = all.iter().enumerate().map(|(i, l)| (i as u64 + 1, l.to_vec())).collect();
            assert_eq!(got, want, "round {round}");
            assert_eq!(s.nl, count_nl(&text[..size as usize]));
            assert_eq!(s.part, size > 0 && text[size as usize - 1] != b'\n');
            assert!(!s.stopped);
            // lines by number: every range against the text's own lines
            let a = 1 + rnd(all.len() + 2) as u64;
            let b = a + rnd(5) as u64;
            let got = lines(&p, size, &[(a, b), (b + 2, b + 3)]).unwrap();
            let want: Vec<(u64, Vec<u8>)> = all.iter().enumerate().map(|(i, l)| (i as u64 + 1, l.to_vec())).filter(|(n, _)| (*n >= a && *n <= b) || (*n >= b + 2 && *n <= b + 3)).collect();
            assert_eq!(got, want, "round {round}: {a}-{b}");
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn scan_stops_when_asked_and_knows_a_gone_capture() {
        let d = tmpdir("scanstop");
        let p = d.join("c.out");
        let line = vec![b'x'; 1000];
        let mut text = Vec::new();
        for _ in 0..3000 {
            text.extend_from_slice(&line);
            text.push(b'\n');
        }
        fs::write(&p, &text).unwrap();
        let mut calls = 0;
        let s = scan(&p, text.len() as u64, 1, &mut |_, _| {
            calls += 1;
            Ok(calls < 2)
        })
        .unwrap();
        assert!(s.stopped);
        // the first run was given whole: its lines are covered
        assert!(s.nl > 0 && s.nl < 3000, "{s:?}");
        fs::remove_file(&p).unwrap();
        assert_eq!(scan(&p, 10, 1, &mut |_, _| Ok(true)).unwrap_err(), GONE);
        fs::remove_dir_all(&d).unwrap();
    }

    /// A capture read while it grows: the lines found in a prefix keep their numbers and
    /// their bytes (whole lines) in the full capture.
    #[test]
    fn lines_found_while_growing_keep_their_numbers() {
        let d = tmpdir("grow");
        let mut w = Writer::create(&d, 5, Instant::now()).unwrap();
        let p = w.path();
        let mut seen: Vec<(u64, Vec<u8>)> = Vec::new();
        for k in 0..50u64 {
            w.write(k % 3 == 0, format!("line {k} needle {}\n", "z".repeat(k as usize)).as_bytes());
            let size = fs::metadata(&p).unwrap().len();
            scan(&p, size, 1, &mut |first, t| {
                for (i, l) in t.split_inclusive(|&c| c == b'\n').enumerate() {
                    if l.windows(6).any(|x| x == b"needle") {
                        seen.push((first + i as u64, l.to_vec()));
                    }
                }
                Ok(true)
            })
            .unwrap();
        }
        w.finish();
        drop(w);
        let all = fs::read(&p).unwrap();
        let lines: Vec<&[u8]> = all.split_inclusive(|&c| c == b'\n').collect();
        assert_eq!(seen.len(), (1..=50).sum::<usize>());
        for (n, l) in seen {
            assert_eq!(lines[n as usize - 1], &l[..]);
        }
        fs::remove_dir_all(&d).unwrap();
    }
}
