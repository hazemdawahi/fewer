//! Lines by number from a run's archive: the `lines` tool and the lines
//! around a search hit.
//!
//! With the run's .idx, its per-block '\n' counts give the blocks that hold the wanted
//! lines: earlier segments are passed by seeking (ArchiveReader::pass), only the segments
//! that hold them are read (each checked against its CRC), and only the blocks that hold
//! them are opened (engine::Seg). Without a .idx that fits (a segment header that differs),
//! the archive is opened from the start until the wanted lines are passed. Either way every
//! byte comes from the decoded archive, checked by the engine, and lines are counted by
//! their '\n' exactly as references count them.

use std::path::Path;

use crate::engine;
use crate::find::IdxTable;
use crate::grep;

/// Wanted line ranges (sorted, merged) and the lines collected so far.
struct Collector {
    ranges: Vec<(u64, u64)>,
    /// the range the current line may be in
    ri: usize,
    /// the line number at the current place
    line: u64,
    /// the current line's bytes so far, when it is wanted
    part: Vec<u8>,
    out: Vec<(u64, Vec<u8>)>,
}

impl Collector {
    fn new(mut ranges: Vec<(u64, u64)>) -> Collector {
        ranges.retain(|r| r.0 <= r.1 && r.1 >= 1);
        for r in ranges.iter_mut() {
            r.0 = r.0.max(1);
        }
        ranges.sort();
        let mut m: Vec<(u64, u64)> = Vec::new();
        for r in ranges {
            match m.last_mut() {
                Some(l) if r.0 <= l.1.saturating_add(1) => l.1 = l.1.max(r.1),
                _ => m.push(r),
            }
        }
        Collector { ranges: m, ri: 0, line: 1, part: Vec::new(), out: Vec::new() }
    }

    fn wanted(&mut self, line: u64) -> bool {
        while self.ri < self.ranges.len() && self.ranges[self.ri].1 < line {
            self.ri += 1;
        }
        self.ri < self.ranges.len() && self.ranges[self.ri].0 <= line
    }

    /// The last wanted line.
    fn last(&self) -> u64 {
        self.ranges.last().map_or(0, |r| r.1)
    }

    fn done(&self) -> bool {
        self.line > self.last()
    }

    /// The next text starts at line `line` (blocks before it were not opened).
    fn jump(&mut self, line: u64) {
        self.part.clear();
        self.line = line;
    }

    /// The next bytes of the text, in order.
    fn feed(&mut self, mut t: &[u8]) {
        while !t.is_empty() && !self.done() {
            let nl = t.iter().position(|&c| c == b'\n');
            let end = nl.map_or(t.len(), |p| p + 1);
            let line = self.line;
            if self.wanted(line) {
                self.part.extend_from_slice(&t[..end]);
                if nl.is_some() {
                    let b = std::mem::take(&mut self.part);
                    self.out.push((line, b));
                }
            }
            if nl.is_some() {
                self.line += 1;
            }
            t = &t[end..];
        }
    }

    /// The end of the text: a last line without '\n'.
    fn finish(&mut self) {
        if !self.part.is_empty() {
            let line = self.line;
            if self.wanted(line) {
                let b = std::mem::take(&mut self.part);
                self.out.push((line, b));
            }
        }
    }
}

/// The lines in `ranges` (1-based, both ends included) of the run whose archive is at
/// `path`, in order, each with its exact bytes (its '\n' included when it has one). Lines
/// past the end are not there.
pub fn fetch(path: &Path, ranges: &[(u64, u64)], threads: usize) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let c = Collector::new(ranges.to_vec());
    if c.ranges.is_empty() {
        return Ok(Vec::new());
    }
    match fetch_indexed(path, Collector::new(ranges.to_vec()), threads) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => fetch_plain(path, c, threads),
        Err(e) => Err(e),
    }
}

/// Lines with their numbers.
type Lines = Vec<(u64, Vec<u8>)>;

/// With the .idx: None when there is no .idx that fits.
fn fetch_indexed(path: &Path, mut c: Collector, threads: usize) -> Result<Option<Lines>, String> {
    let (mut ar, fmd) = grep::open_archive(path.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (the tools read file archives)".into());
    }
    grep::parse_file_meta(&ar.meta)?;
    let len = fmd.map_or(0, |m| m.len());
    let Ok(tab) = IdxTable::load(&grep::idx_path(path.as_os_str()), len) else { return Ok(None) };
    // each block's first line, and the last line it holds a byte of
    let mut first = Vec::with_capacity(tab.blocks.len());
    let mut lines_before = 0u64;
    for b in &tab.blocks {
        first.push(lines_before + 1);
        lines_before += b.nl;
    }
    let holds = |k: usize, c: &Collector| {
        let b = tab.blocks[k];
        let (lo, hi) = (first[k], first[k] + b.nl - b.ends as u64);
        c.ranges.iter().any(|r| r.0 <= hi && r.1 >= lo)
    };
    let mut g = 0usize;
    // whether the block before was opened (then the text goes on without a jump, also from
    // one segment into the next)
    let mut opened_last = false;
    'members: loop {
        while let Some(h) = ar.next_header()? {
            let Some(&(hdr, b0, nb)) = tab.segs.get(g) else { return Ok(None) };
            if hdr != h {
                return Ok(None);
            }
            g += 1;
            if c.done() {
                break 'members;
            }
            if !(b0..b0 + nb).any(|k| holds(k, &c)) {
                ar.pass(h)?;
                opened_last = false;
                continue;
            }
            let p = ar.payload(h)?;
            let n = g;
            let seg = engine::Seg::new(p, threads).map_err(|e| seg_err(n, e))?;
            if seg.blocks() != nb {
                return Ok(None);
            }
            let mut buf = engine::BlockBuf::default();
            for i in 0..nb {
                let k = b0 + i;
                if !holds(k, &c) {
                    opened_last = false;
                    continue;
                }
                if !opened_last {
                    c.jump(first[k]);
                }
                let raw = seg.block_raw(i);
                if raw as u64 != tab.blocks[k].raw {
                    return Ok(None);
                }
                seg.open(i, &mut buf, threads, None).map_err(|e| seg_err(n, e))?;
                c.feed(buf.text(raw));
                opened_last = true;
                if c.done() {
                    break 'members;
                }
            }
        }
        let folder = ar.is_folder();
        match ar.next_member()? {
            Some(next) if !folder && !next.is_folder() => ar = next,
            Some(_) => return Err("unexpected data after the end of the archive".into()),
            None => break,
        }
    }
    if g >= tab.segs.len() {
        c.finish();
    }
    Ok(Some(c.out))
}

fn seg_err(n: usize, e: String) -> String {
    if e.starts_with("made by ") { e } else { format!("damaged archive (segment {n}: {e})") }
}

/// Without a .idx: the archive opened from the start.
fn fetch_plain(path: &Path, mut c: Collector, threads: usize) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let (ar, _) = grep::open_archive(path.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (the tools read file archives)".into());
    }
    grep::parse_file_meta(&ar.meta)?;
    crate::container::for_each_segment(ar, threads, &mut |d| {
        c.feed(d);
        Ok(!c.done())
    })?;
    if !c.done() {
        c.finish();
    }
    Ok(c.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_takes_the_wanted_lines() {
        let text = b"one\ntwo\nthree\nfour\nfive";
        for cut in 0..text.len() {
            let mut c = Collector::new(vec![(2, 3), (5, 9), (3, 3)]);
            c.feed(&text[..cut]);
            c.feed(&text[cut..]);
            c.finish();
            assert_eq!(c.out, vec![(2, b"two\n".to_vec()), (3, b"three\n".to_vec()), (5, b"five".to_vec())]);
        }
        let mut c = Collector::new(vec![(1, 1)]);
        c.feed(b"\n\nx");
        assert!(c.done());
        assert_eq!(c.out, vec![(1, b"\n".to_vec())]);
    }
}
