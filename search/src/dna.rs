//! The DNA index, in its shrunk form. A small file beside a run's archive,
//! rNNNNNN.dna, that lets a search leave unopened the blocks no line of which can hold the
//! text, also for a piece of a word, which the .idx's labels cannot rule out.
//!
//! **What goes in.** Per segment of the archive, the segment's distinct tokens (the .idx's
//! tokens: longest runs of ASCII letters, digits and bytes >= 0x80), folded: ASCII letters
//! lowercased, and every byte >= 0x80 as one symbol, 0x80 (which can only add maybes, never
//! lose a match). Rule 1 leaves out every token made only of 0-9 and a-f that holds a digit
//! (so every number) and every 0x token (0x followed only by 0-9 and a-f): the labels find
//! those. So "refused" and "blk" go in; "1117838570", "4ae9" and "0x7f3a" stay out. The kept
//! tokens, sorted, each followed by 0x01, are the text of an FM index that keeps no places
//! (fm.rs); with it, in a segment of more than one block, each token's blocks.
//!
//! **Which pieces of a needle are looked up** (rule 2, written as rule 1's complement): a
//! piece is a run of token bytes of the needle, folded, of 3 or more bytes, holding a byte
//! other than 0-9, a-f and x. Every token rule 1 leaves out is made of 0-9, a-f and x only,
//! and so is every piece of one, so a looked-up piece lies only in tokens that went in. Other
//! pieces are not looked up; a needle with none is searched as before the DNA index.
//!
//! **How a search uses it** (`Search`, called by find.rs where the .idx's skips are made). A
//! line that holds the needle holds each piece inside one of its tokens. In each segment, each
//! piece (at most 3, the longest first) is counted first: a piece no token holds rules out
//! every block, and a piece with over 4,096 places is not used. A used piece's places are
//! walked back (fm.rs's LF step, `back`) to the 0x01 before each, which names its token: the
//! tokens are distinct and sorted and 0x01 is below every token byte, so the rows of the token
//! starts sort in token order, and the row the 0x01 step lands on, less the first row of the
//! 0x01s, is the token's number (the rank of that 0x01 in the BWT). A walk that reaches the
//! text's first row (`Fm::text_row`) is in the first token; the last token's 0x01, which ends
//! the text, is never the 0x01 before a token. A step budget caps a piece's walks in a
//! segment: past it, that piece rules nothing out there. The blocks of the tokens found,
//! intersected over the pieces, are the only ones that may hold the needle. find.rs leaves the
//! others unopened only at a '\n' edge and only with a .idx that gives their line counts, and
//! checks the lines of every opened block with the same Matcher: the DNA index never decides a
//! line, it only leaves blocks unopened.
//!
//! **The file** (numbers are varints, as in the .idx and the .cat):
//!
//! ```text
//! dna  = "CMLD" 01 archive-bytes end-crc segments seg* CRC-32
//!          end-crc: the archive's last 4 bytes (its end record's CRC over every segment)
//! seg  = length                     the bytes of the rest of this segment's record
//!        raw packed crc             the archive's segment header (crc: 4 bytes)
//!        blocks (raw ends)*         each block's size, and 1 when its text ends with '\n'
//!        form                       0: not indexed (its tokens make over 16 MiB of text; the
//!                                      DNA index rules out nothing there); 1: one block;
//!                                      2: bitmaps; 3: lists
//!        [tokens text-length fm]    forms 1-3: the number of tokens, the length of their text,
//!                                      fm.rs's stored form with no places kept
//!        [token-blocks]             form 2: per token, ⌈blocks / 8⌉ bytes, bit b % 8 of byte
//!                                      b / 8 set when block b holds it;
//!                                   form 3: the place of every 32nd token's list (u32, from
//!                                      the lists' start), then per token its number of
//!                                      blocks, its first block and the gaps to the next ones
//! CRC-32 = crc.rs's CRC-32 of every byte before it (4 bytes, little-endian)
//! ```
//!
//! A segment of one block has no token-blocks; of bitmaps and lists, the smaller is written.
//! A .dna that was made for another archive (its length or end-crc), is damaged (its CRC, or
//! parts that do not fit), of another version, or whose segment differs from the archive's,
//! is not used, with a note in the debug log: so it can never cause a wrong skip.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::container::{get_varint, put_varint};
use crate::crc::crc32;
use crate::engine;
use crate::fm::Fm;

const MAGIC: &[u8; 4] = b"CMLD";
/// The version of the .dna file this program writes and reads.
pub const VERSION: u8 = 1;
/// The mark after each token in the index's text: below every token byte.
const SEP: u8 = 0x01;
/// The one symbol every byte >= 0x80 becomes.
const HIGH: u8 = 0x80;
/// Shorter pieces are not looked up.
pub const MIN_PIECE: usize = 3;
/// At most this many pieces of a needle are looked up, the longest first.
pub const MAX_PIECES: usize = 3;
/// A piece with more places than this in a segment is not used there (the
/// planner's "skip it for pieces that match many values").
pub const MAX_PLACES: usize = 4096;
/// The step budget of one piece in one segment: the LF steps all its walks back to their
/// tokens may take. Past it, the piece rules nothing out in that segment (never a skip on a
/// guess). 65,536 steps let the 4,096 places a piece may have lie 16 bytes from their token's
/// start on average (a token with its mark is 12.6 bytes on the 16 test logs, M, the study);
/// they cost about as much as opening one block (E). The stage 1 DNA test sets it from
/// measured lookups.
pub const STEP_BUDGET: usize = 1 << 16;
/// A segment whose kept tokens make more text than this is not indexed: building its index
/// takes about 5 bytes of memory per byte of that text. The 16 test logs hold at most about
/// 0.6 MB per segment (M, the study).
pub const MAX_TEXT: usize = 16 << 20;
/// Runs with less text than this get no DNA index unless the config's `dna_min` says
/// otherwise.
pub const DNA_MIN: u64 = 16 << 20;
/// What a build stopped by its caller gives (the idle builder: the load rose, or the server
/// ends): nothing is written or recorded.
pub const STOPPED: &str = "stopped before its end";

const FORM_NONE: u8 = 0;
const FORM_ONE: u8 = 1;
const FORM_BITMAPS: u8 = 2;
const FORM_LISTS: u8 = 3;
/// In form 3, the place of every LIST_STEP-th token's list is kept.
const LIST_STEP: usize = 32;

fn note(msg: &str) {
    crate::mcp::debug(msg)
}

// ---------------------------------------------------------------- tokens and pieces

/// Each byte as the index keeps it in a token: 0 for a byte that is not a token byte
/// (index.rs's `tok`: ASCII letters and digits, and bytes >= 0x80), else the byte folded:
/// ASCII letters lowercased, every byte >= 0x80 as HIGH.
const FOLD: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut c = 0;
    while c < 256 {
        let b = c as u8;
        t[c] = if b >= 0x80 {
            HIGH
        } else if b.is_ascii_alphanumeric() {
            b.to_ascii_lowercase()
        } else {
            0
        };
        c += 1;
    }
    t
};

/// Rule 1's bytes: 0-9 and a-f (tokens are folded, so lowercase).
fn hex(c: u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
}

/// Rule 1: whether a folded token is left out of the index: made only of 0-9 and a-f and
/// holding a digit (every number, hex ID, hash and UUID group), or 0x followed only by 0-9
/// and a-f. The .idx's labels find these.
pub fn left_out(t: &[u8]) -> bool {
    let number = t.iter().all(|&c| hex(c)) && t.iter().any(u8::is_ascii_digit);
    let hex_0x = t.starts_with(b"0x") && t[2..].iter().all(|&c| hex(c));
    number || hex_0x
}

/// Every byte a token that rule 1 leaves out can hold: rule 1's 0-9 and a-f, and the x of
/// 0x. Rule 2 is written with it, so that it is rule 1's complement.
fn left_out_byte(c: u8) -> bool {
    hex(c) || c == b'x'
}

/// Rule 2: whether a folded piece is looked up: it holds a byte that no token rule 1 leaves
/// out holds (one other than 0-9, a-f and x), so no such token can contain it. (`pieces`
/// also asks for MIN_PIECE bytes.)
pub fn looked_up(p: &[u8]) -> bool {
    p.iter().any(|&c| !left_out_byte(c))
}

/// The pieces of `needle` the DNA index looks up (rule 2, in the module's doc): its runs of
/// token bytes, folded, of MIN_PIECE bytes or more that rule 2 lets through; each once and
/// none inside a longer one (it would rule out no more); the longest first (as in the needle
/// when as long); at most MAX_PIECES. Folding makes them serve searches with and without -i.
pub fn pieces(needle: &[u8]) -> Vec<Vec<u8>> {
    let mut runs: Vec<Vec<u8>> = Vec::new();
    for run in needle.split(|&c| FOLD[c as usize] == 0) {
        let p: Vec<u8> = run.iter().map(|&c| FOLD[c as usize]).collect();
        if p.len() >= MIN_PIECE && looked_up(&p) {
            runs.push(p);
        }
    }
    runs.sort_by_key(|p| std::cmp::Reverse(p.len()));
    let mut out: Vec<Vec<u8>> = Vec::new();
    for p in runs {
        if out.len() < MAX_PIECES && !out.iter().any(|q| q.windows(p.len()).any(|w| w == &p[..])) {
            out.push(p);
        }
    }
    out
}

/// Each token of `text` that rule 1 keeps, folded, given to `f` (`buf` is scratch).
fn kept_tokens(text: &[u8], buf: &mut Vec<u8>, f: &mut dyn FnMut(&[u8])) {
    let mut i = 0;
    while i < text.len() {
        if FOLD[text[i] as usize] == 0 {
            i += 1;
            continue;
        }
        buf.clear();
        while i < text.len() {
            let c = FOLD[text[i] as usize];
            if c == 0 {
                break;
            }
            buf.push(c);
            i += 1;
        }
        if !left_out(buf) {
            f(buf);
        }
    }
}

// ---------------------------------------------------------------- building

/// A segment's tokens and blocks, gathered block by block while its record is made.
#[derive(Default)]
struct Gather {
    /// each kept token's number, in order of first appearance
    ids: HashMap<Box<[u8]>, u32>,
    /// each token's blocks, in order (a token's number is its place here)
    blocks_of: Vec<Vec<u32>>,
    /// the length of the text the tokens make, each with its mark
    text_len: usize,
    /// over MAX_TEXT: the segment is not indexed
    too_big: bool,
    /// each block's size and whether its text ends with '\n'
    blocks: Vec<(u64, bool)>,
    buf: Vec<u8>,
}

impl Gather {
    /// The next block's text.
    fn block(&mut self, text: &[u8]) {
        let b = self.blocks.len() as u32;
        self.blocks.push((text.len() as u64, text.last() == Some(&b'\n')));
        if self.too_big {
            return;
        }
        let (ids, blocks_of, text_len, too_big) = (&mut self.ids, &mut self.blocks_of, &mut self.text_len, &mut self.too_big);
        kept_tokens(text, &mut self.buf, &mut |t| {
            if let Some(&id) = ids.get(t) {
                let v = &mut blocks_of[id as usize];
                if v.last() != Some(&b) {
                    v.push(b);
                }
            } else if !*too_big {
                *text_len += t.len() + 1;
                if *text_len > MAX_TEXT {
                    *too_big = true;
                } else {
                    ids.insert(t.into(), blocks_of.len() as u32);
                    blocks_of.push(vec![b]);
                }
            }
        });
        if self.too_big {
            self.ids = HashMap::new();
            self.blocks_of = Vec::new();
        }
    }

    /// The segment's record, with its length in front, added to `out`.
    fn finish(self, h: (u64, u64, u32), out: &mut Vec<u8>) -> Result<(), String> {
        let mut rec = Vec::new();
        put_varint(&mut rec, h.0);
        put_varint(&mut rec, h.1);
        rec.extend_from_slice(&h.2.to_le_bytes());
        put_varint(&mut rec, self.blocks.len() as u64);
        for &(raw, ends) in &self.blocks {
            put_varint(&mut rec, raw);
            rec.push(ends as u8);
        }
        let nb = self.blocks.len();
        if self.too_big || nb == 0 {
            rec.push(FORM_NONE);
        } else {
            // the tokens in byte order, each followed by its mark
            let mut order: Vec<(Box<[u8]>, u32)> = self.ids.into_iter().collect();
            order.sort_unstable();
            let mut text = Vec::with_capacity(self.text_len);
            for (t, _) in &order {
                text.extend_from_slice(t);
                text.push(SEP);
            }
            let fm = Fm::no_places(&text)?;
            let lists: Vec<&[u32]> = order.iter().map(|(_, id)| &self.blocks_of[*id as usize][..]).collect();
            let (form, tail) = if nb == 1 {
                (FORM_ONE, Vec::new())
            } else {
                let b = bitmaps(&lists, nb);
                match list_form(&lists) {
                    Some(l) if l.len() < b.len() => (FORM_LISTS, l),
                    _ => (FORM_BITMAPS, b),
                }
            };
            rec.push(form);
            put_varint(&mut rec, order.len() as u64);
            put_varint(&mut rec, text.len() as u64);
            fm.write(&mut rec);
            rec.extend(tail);
        }
        put_varint(out, rec.len() as u64);
        out.extend(rec);
        Ok(())
    }
}

/// Each token's blocks as a bitmap of ⌈nb / 8⌉ bytes: bit b % 8 of byte b / 8 for block b.
fn bitmaps(lists: &[&[u32]], nb: usize) -> Vec<u8> {
    let w = nb.div_ceil(8);
    let mut out = vec![0u8; lists.len() * w];
    for (t, l) in lists.iter().enumerate() {
        for &b in *l {
            out[t * w + b as usize / 8] |= 1 << (b % 8);
        }
    }
    out
}

/// Each token's blocks as a list: their number, the first block, then the gaps to the next
/// ones (varints); in front, the place of every LIST_STEP-th token's list (u32, little-endian,
/// from the lists' start). None when the lists pass u32's range.
fn list_form(lists: &[&[u32]]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut out = Vec::with_capacity(lists.len().div_ceil(LIST_STEP) * 4);
    for (t, l) in lists.iter().enumerate() {
        if t % LIST_STEP == 0 {
            out.extend_from_slice(&u32::try_from(body.len()).ok()?.to_le_bytes());
        }
        put_varint(&mut body, l.len() as u64);
        let mut prev = 0;
        for (i, &b) in l.iter().enumerate() {
            put_varint(&mut body, u64::from(if i == 0 { b } else { b - prev }));
            prev = b;
        }
    }
    out.extend(body);
    Some(out)
}

/// The DNA index of the archive file at `path` (and of file archives joined after it): the
/// .dna file's bytes. Its blocks are opened on `threads` threads. `go_on` is asked before
/// each segment and each block; when it says no, the build stops with Err(STOPPED).
pub fn build(path: &Path, threads: usize, go_on: &mut dyn FnMut() -> bool) -> Result<Vec<u8>, String> {
    let (len, end) = crate::catalog::archive_id(path)?;
    let (mut ar, _) = crate::grep::open_archive(path.as_os_str(), false)?;
    if ar.is_folder() {
        return Err("a folder archive (DNA indexes are made for file archives)".into());
    }
    let mut body = Vec::new();
    let mut n = 0u64;
    loop {
        while let Some(h) = ar.next_header()? {
            if !go_on() {
                return Err(STOPPED.into());
            }
            n += 1;
            let p = ar.payload(h)?;
            let mut g = Gather::default();
            let mut stopped = false;
            let r = engine::unpack_blocks(&p, threads, None, true, &mut |t, _| {
                g.block(t.unwrap_or_default());
                stopped = !go_on();
                Ok(!stopped)
            });
            if stopped {
                return Err(STOPPED.into());
            }
            if r.map_err(|e| format!("damaged archive (segment {n}: {e})"))? != h.0 {
                return Err(format!("damaged archive (segment {n} unpacked to the wrong size)"));
            }
            drop(p);
            g.finish(h, &mut body)?;
        }
        match ar.next_member()? {
            Some(next) if !next.is_folder() => ar = next,
            Some(_) => return Err("unexpected data after the end of the archive".into()),
            None => break,
        }
    }
    let mut o = MAGIC.to_vec();
    o.push(VERSION);
    put_varint(&mut o, len);
    o.extend_from_slice(&end);
    put_varint(&mut o, n);
    o.extend(body);
    let c = crc32(&o);
    o.extend_from_slice(&c.to_le_bytes());
    Ok(o)
}

// ---------------------------------------------------------------- reading

/// A run's .dna file, read and checked against its archive: its CRC holds, its version is
/// this program's, and it was made for this very archive (its length and last 4 bytes).
#[derive(Debug)]
pub struct Dna {
    bytes: Vec<u8>,
    /// each segment's record in `bytes` (after its length)
    segs: Vec<Range<usize>>,
}

impl Dna {
    /// The DNA index in `d`, for the archive of `len` bytes whose last 4 bytes are `end`.
    pub fn read(d: Vec<u8>, len: u64, end: [u8; 4]) -> Result<Dna, String> {
        if d.len() < 9 || d[..4] != MAGIC[..] {
            return Err("not a DNA index".into());
        }
        if d[4] != VERSION {
            return Err(format!("made by another version (DNA index version {}; this program reads version {VERSION})", d[4]));
        }
        let body = d.len() - 4;
        if crc32(&d[..body]).to_le_bytes() != d[body..] {
            return Err("damaged (its checksum does not match)".into());
        }
        let b = &d[..body];
        let damaged = || "damaged".to_string();
        let p = &mut 5;
        let alen = get_varint(b, p).ok_or_else(damaged)?;
        let aend: [u8; 4] = b.get(*p..*p + 4).ok_or_else(damaged)?.try_into().unwrap();
        *p += 4;
        if alen != len || aend != end {
            return Err(crate::MISMATCH.to_string());
        }
        let n = get_varint(b, p).ok_or_else(damaged)?;
        let mut segs = Vec::new();
        for _ in 0..n {
            let k = get_varint(b, p).ok_or_else(damaged)?;
            let e = usize::try_from(k).ok().and_then(|k| p.checked_add(k)).filter(|&e| e <= body).ok_or_else(damaged)?;
            segs.push(*p..e);
            *p = e;
        }
        if *p != body {
            return Err(damaged());
        }
        Ok(Dna { bytes: d, segs })
    }

    /// Segment g's part (from 0, over joined archives, as the .idx counts them).
    pub fn segment(&self, g: usize) -> Result<Seg<'_>, String> {
        let r = self.segs.get(g).ok_or("it has fewer segments than the archive")?;
        Seg::parse(&self.bytes[r.clone()])
    }

    /// What each segment holds, for the size tables (`dna-file`) and the DNA
    /// test: its blocks, its form, its tokens, its text's bytes, its FM index's bytes, its
    /// token-blocks' bytes.
    #[allow(dead_code)]
    pub fn info(&self) -> Result<Vec<SegInfo>, String> {
        let mut out = Vec::new();
        for g in 0..self.segs.len() {
            let s = self.segment(g)?;
            let (form, tokens, text, fm, lists) = match &s.index {
                None => (FORM_NONE, 0, 0, 0, 0),
                Some(ix) => {
                    let (form, lists) = match &ix.lists {
                        TokenBlocks::One => (FORM_ONE, 0),
                        TokenBlocks::Bitmaps(b) => (FORM_BITMAPS, b.len()),
                        TokenBlocks::Lists(o, l) => (FORM_LISTS, 4 * o.len() + l.len()),
                    };
                    (form, ix.tokens, ix.fm.len(), ix.fm.file_len(), lists)
                }
            };
            out.push(SegInfo { blocks: s.blocks.len(), form, tokens, text, fm, lists, record: self.segs[g].len() });
        }
        Ok(out)
    }
}

/// What a segment's part of a DNA index holds (`Dna::info`).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegInfo {
    pub blocks: usize,
    /// 0: not indexed, 1: one block, 2: bitmaps, 3: lists
    pub form: u8,
    pub tokens: usize,
    /// the bytes of the tokens' text (each with its mark)
    pub text: usize,
    /// the bytes of its FM index
    pub fm: usize,
    /// the bytes of its token-blocks
    pub lists: usize,
    /// the bytes of its whole record
    pub record: usize,
}

/// One segment's part of a DNA index.
pub struct Seg<'a> {
    /// the archive's segment header it was made from
    pub header: (u64, u64, u32),
    /// each block's size and whether its text ends with '\n'
    pub blocks: Vec<(u64, bool)>,
    /// None: not indexed (the DNA index rules out nothing in this segment)
    index: Option<SegIndex<'a>>,
}

struct SegIndex<'a> {
    tokens: usize,
    fm: Fm,
    /// the first row of the suffixes that start with SEP
    sep: usize,
    lists: TokenBlocks<'a>,
}

/// A segment's token-blocks.
enum TokenBlocks<'a> {
    /// a segment of one block: none
    One,
    /// ⌈blocks / 8⌉ bytes per token
    Bitmaps(&'a [u8]),
    /// the place of every LIST_STEP-th token's list, and the lists
    Lists(Vec<u32>, &'a [u8]),
}

impl<'a> Seg<'a> {
    fn parse(d: &'a [u8]) -> Result<Seg<'a>, String> {
        let bad = |what: &str| format!("damaged ({what})");
        let p = &mut 0usize;
        let var = |p: &mut usize| get_varint(d, p).ok_or_else(|| bad("cut short"));
        let byte = |p: &mut usize| -> Result<u8, String> {
            let b = *d.get(*p).ok_or_else(|| bad("cut short"))?;
            *p += 1;
            Ok(b)
        };
        let raw = var(p)?;
        let packed = var(p)?;
        let crc = u32::from_le_bytes(d.get(*p..*p + 4).ok_or_else(|| bad("cut short"))?.try_into().unwrap());
        *p += 4;
        let nb = var(p)?;
        if nb == 0 || nb > raw {
            return Err(bad("its blocks"));
        }
        let mut blocks = Vec::new();
        let mut sum = 0u64;
        for _ in 0..nb {
            let braw = var(p)?;
            let ends = byte(p)?;
            if braw == 0 || ends > 1 {
                return Err(bad("its blocks"));
            }
            sum = sum.checked_add(braw).ok_or_else(|| bad("its blocks"))?;
            blocks.push((braw, ends == 1));
        }
        if sum != raw {
            return Err(bad("its blocks"));
        }
        let nb = blocks.len();
        let form = byte(p)?;
        let index = match form {
            FORM_NONE if *p == d.len() => None,
            FORM_ONE | FORM_BITMAPS | FORM_LISTS if (form == FORM_ONE) == (nb == 1) => {
                let tokens = usize::try_from(var(p)?).map_err(|_| bad("its tokens"))?;
                let text = usize::try_from(var(p)?).map_err(|_| bad("its tokens"))?;
                let (fm, used) = Fm::read(&d[*p..])?;
                *p += used;
                if fm.rate() != 0 || fm.len() != text || fm.count(&[SEP]) != tokens {
                    return Err(bad("its tokens"));
                }
                let sep = fm.rows(&[SEP]).start;
                let rest = &d[*p..];
                let lists = match form {
                    FORM_ONE if rest.is_empty() => TokenBlocks::One,
                    FORM_BITMAPS if Some(rest.len()) == tokens.checked_mul(nb.div_ceil(8)) => TokenBlocks::Bitmaps(rest),
                    FORM_LISTS if rest.len() >= 4 * tokens.div_ceil(LIST_STEP) => {
                        let (head, lists) = rest.split_at(4 * tokens.div_ceil(LIST_STEP));
                        let offsets: Vec<u32> = head.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
                        // every list read once: where the offsets say, its blocks in order and
                        // below the segment's, the last one ending the record
                        let mut q = 0usize;
                        for t in 0..tokens {
                            if t % LIST_STEP == 0 && offsets[t / LIST_STEP] as usize != q {
                                return Err(bad("its lists"));
                            }
                            let k = get_varint(lists, &mut q).filter(|&k| k >= 1 && k <= nb as u64).ok_or_else(|| bad("its lists"))?;
                            let mut b = 0u64;
                            for i in 0..k {
                                let gap = get_varint(lists, &mut q).filter(|&g| i == 0 || g > 0).ok_or_else(|| bad("its lists"))?;
                                b = b.checked_add(gap).filter(|&b| b < nb as u64).ok_or_else(|| bad("its lists"))?;
                            }
                        }
                        if q != lists.len() {
                            return Err(bad("its lists"));
                        }
                        TokenBlocks::Lists(offsets, lists)
                    }
                    _ => return Err(bad("its token-blocks")),
                };
                Some(SegIndex { tokens, fm, sep, lists })
            }
            _ => return Err(bad("its form")),
        };
        Ok(Seg { header: (raw, packed, crc), blocks, index })
    }

    /// Whether this segment is indexed (a segment whose tokens are too many is not).
    #[cfg(test)]
    pub fn indexed(&self) -> bool {
        self.index.is_some()
    }

    /// For each block of this segment, whether no token of it holds one of `pieces` (folded,
    /// as `pieces` gives them; at most MAX_PIECES are looked at), so that no line wholly
    /// inside it can hold a needle with those pieces. A piece no token of the segment holds
    /// rules out every block. A piece with over MAX_PLACES places, or whose walks back to
    /// their tokens would take over `budget` LF steps, rules nothing out; so does any piece in
    /// a segment that is not indexed, or of one block that holds it.
    pub fn rule_out(&self, pieces: &[Vec<u8>], budget: usize) -> Vec<bool> {
        let nb = self.blocks.len();
        let Some(ix) = &self.index else { return vec![false; nb] };
        // count first
        let mut used: Vec<Range<usize>> = Vec::new();
        for p in pieces.iter().take(MAX_PIECES) {
            let rows = ix.fm.rows(p);
            if rows.is_empty() {
                return vec![true; nb];
            }
            if rows.len() <= MAX_PLACES {
                used.push(rows);
            }
        }
        if matches!(ix.lists, TokenBlocks::One) || used.is_empty() {
            return vec![false; nb];
        }
        // the fewest places first (the cheapest walks); the blocks that may hold every piece
        used.sort_by_key(|r| r.len());
        let mut may: Option<Vec<u8>> = None;
        for rows in used {
            let Some(set) = ix.blocks_holding(rows, nb, budget) else { continue };
            let m = match may.take() {
                None => set,
                Some(m) => m.iter().zip(&set).map(|(a, b)| a & b).collect(),
            };
            let none = m.iter().all(|&x| x == 0);
            may = Some(m);
            if none {
                break;
            }
        }
        match may {
            None => vec![false; nb],
            Some(m) => (0..nb).map(|b| m[b / 8] >> (b % 8) & 1 == 0).collect(),
        }
    }
}

impl SegIndex<'_> {
    /// The blocks of the tokens that hold the places at `rows` (a piece's), as a bitmap of
    /// ⌈nb / 8⌉ bytes; None when the walks would pass `budget` LF steps or the index is
    /// damaged.
    fn blocks_holding(&self, rows: Range<usize>, nb: usize, budget: usize) -> Option<Vec<u8>> {
        let mut set = vec![0u8; nb.div_ceil(8)];
        let mut steps = 0usize;
        for row in rows {
            let t = self.token_of(row, budget, &mut steps)?;
            self.add_blocks(t, nb, &mut set)?;
        }
        Some(set)
    }

    /// The number of the token holding row `row`'s place: LF steps back to the mark before it,
    /// each counted in `steps`. The first token has no mark before it: its walk ends at the
    /// text's row. None past `budget` steps, or on a damaged index.
    fn token_of(&self, mut row: usize, budget: usize, steps: &mut usize) -> Option<usize> {
        loop {
            if *steps >= budget {
                return None;
            }
            *steps += 1;
            match self.fm.back(row) {
                None => return (row == self.fm.text_row() && self.tokens > 0).then_some(0),
                Some((SEP, sep_row)) => return sep_row.checked_sub(self.sep).filter(|&t| t < self.tokens),
                Some((_, next)) => row = next,
            }
        }
    }

    /// Adds token t's blocks to `set`. None on a damaged index.
    fn add_blocks(&self, t: usize, nb: usize, set: &mut [u8]) -> Option<()> {
        match &self.lists {
            TokenBlocks::One => set[0] |= 1,
            TokenBlocks::Bitmaps(b) => {
                let w = nb.div_ceil(8);
                for (s, x) in set.iter_mut().zip(b.get(t * w..(t + 1) * w)?) {
                    *s |= x;
                }
            }
            TokenBlocks::Lists(offsets, d) => {
                let mut p = *offsets.get(t / LIST_STEP)? as usize;
                for _ in 0..t % LIST_STEP {
                    let k = get_varint(d, &mut p)?;
                    for _ in 0..k {
                        get_varint(d, &mut p)?;
                    }
                }
                let k = get_varint(d, &mut p)?;
                if k == 0 || k > nb as u64 {
                    return None;
                }
                let mut b = 0u64;
                for i in 0..k {
                    let gap = get_varint(d, &mut p)?;
                    if i > 0 && gap == 0 {
                        return None;
                    }
                    b = b.checked_add(gap).filter(|&b| b < nb as u64)?;
                    set[b as usize / 8] |= 1 << (b % 8);
                }
            }
        }
        Some(())
    }
}

/// Whether the .dna at `path` says it was made by this version for the archive of `len`
/// bytes whose last 4 bytes are `end`: its first bytes only (`Dna::read` checks the whole).
pub fn made_for(path: &Path, len: u64, end: [u8; 4]) -> bool {
    let mut head = Vec::with_capacity(24);
    if fs::File::open(path).and_then(|f| f.take(24).read_to_end(&mut head)).is_err() || head.len() < 10 || head[..4] != MAGIC[..] || head[4] != VERSION {
        return false;
    }
    let p = &mut 5;
    get_varint(&head, p) == Some(len) && head.get(*p..*p + 4) == Some(&end[..])
}

// ---------------------------------------------------------------- where, and whether it is on

/// The .dna file of an archive: rNNNNNN.cmf gives rNNNNNN.dna (another name gets ".dna").
pub fn dna_path(archive: &Path) -> PathBuf {
    if archive.extension().is_some_and(|e| e == "cmf") {
        archive.with_extension("dna")
    } else {
        let mut p = archive.as_os_str().to_os_string();
        p.push(".dna");
        PathBuf::from(p)
    }
}

/// The store folder of a run's archive, `<home>/projects/<pid>/rNNNNNN.cmf`;
/// None for an archive outside a store.
pub fn store_home(archive: &Path) -> Option<PathBuf> {
    let projects = archive.parent()?.parent()?;
    if projects.file_name()? != "projects" {
        return None;
    }
    projects.parent().map(Path::to_path_buf)
}

/// The store's DNA settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// on unless the config says `dna = off`
    pub on: bool,
    /// runs with at least this many bytes of text get one (`dna_min`, 16 MiB unless set)
    pub min: u64,
}

/// A size as the config writes it: "16MiB", "16 MiB", "20MB", "1GiB" or bytes ("1048576").
pub fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
    let n: u64 = v[..digits].parse().ok()?;
    let k: u64 = match v[digits..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1000,
        "kib" => 1 << 10,
        "mb" => 1_000_000,
        "mib" => 1 << 20,
        "gb" => 1_000_000_000,
        "gib" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(k)
}

/// The settings in the config's lines (the first line of a key counts, as the store reads it).
fn settings_of(lines: &[(String, String)]) -> Settings {
    let on = crate::config::get(lines, "dna").is_none_or(|v| !v.trim().eq_ignore_ascii_case("off"));
    let min = crate::config::get(lines, "dna_min").and_then(parse_size).unwrap_or(DNA_MIN);
    Settings { on, min }
}

/// The DNA settings of the store at `home`. The config is read again only when it changed:
/// a stat per call (its size, time and inode: `config::set` writes a new file each time).
pub fn settings(home: &Path) -> Settings {
    type Stamp = Option<(u64, i64, i64, u64)>;
    static LAST: Mutex<Option<(PathBuf, Stamp, Settings)>> = Mutex::new(None);
    let stamp: Stamp = fs::metadata(crate::config::path(home)).ok().map(|m| (m.len(), m.mtime(), m.mtime_nsec(), m.ino()));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((h, st, s)) = last.as_ref()
        && h == home
        && *st == stamp
    {
        return *s;
    }
    let s = settings_of(&crate::config::read(home));
    *last = Some((home.to_path_buf(), stamp, s));
    s
}

// ---------------------------------------------------------------- searching

/// The step budget: STEP_BUDGET, or the test switch FEWER_TEST_DNA_STEPS (checks/dna_check.py
/// sets a small one so that the budget's path is taken; never set in normal use).
fn budget() -> usize {
    static B: OnceLock<usize> = OnceLock::new();
    *B.get_or_init(|| std::env::var("FEWER_TEST_DNA_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(STEP_BUDGET))
}

/// A .dna bigger than this is not read (a real one is about 2.5% of its archive).
const READ_MAX: u64 = 1 << 30;

/// A run's DNA index as one search of the run uses it (find.rs). Nothing is read until a
/// segment has a block that the .idx leaves open and that may be left unopened, and nothing
/// at all when the needle has no piece to look up, or the store has the DNA index off.
pub struct Search<'a> {
    archive: &'a Path,
    pieces: &'a [Vec<u8>],
    dna: OnceLock<Option<Dna>>,
    /// blocks this search left unopened because of the DNA index (find.rs's debug line)
    pub ruled_out: AtomicUsize,
}

impl<'a> Search<'a> {
    /// For a search of the archive at `archive` for a needle with these pieces (`pieces`).
    pub fn new(archive: &'a Path, pieces: &'a [Vec<u8>]) -> Search<'a> {
        Search { archive, pieces, dna: OnceLock::new(), ruled_out: AtomicUsize::new(0) }
    }

    /// Segment g of the archive (its header `h`), whose blocks the .idx gives as (size, ends
    /// with '\n', may be left unopened: Index::skips's '\n' edge rule), and `skip`, the
    /// verdicts so far (the .idx's and a cursor's): sets the blocks the DNA index rules out.
    pub fn skips(&self, g: usize, h: (u64, u64, u32), blocks: &[(u64, bool, bool)], skip: &mut [(usize, bool)]) {
        if self.pieces.is_empty() || blocks.len() != skip.len() || !skip.iter().zip(blocks).any(|(s, b)| !s.1 && b.2) {
            return;
        }
        let Some(dna) = self.dna.get_or_init(|| self.open()) else { return };
        let seg = match dna.segment(g) {
            Ok(s) => s,
            Err(e) => {
                note(&format!("dna: {}: segment {} not used: {e}", dna_path(self.archive).display(), g + 1));
                return;
            }
        };
        if seg.header != h || seg.blocks.len() != blocks.len() || seg.blocks.iter().zip(blocks).any(|(a, b)| a.0 != b.0 || a.1 != b.1) {
            note(&format!("dna: {}: segment {} not used: it does not fit the archive's", dna_path(self.archive).display(), g + 1));
            return;
        }
        let out = seg.rule_out(self.pieces, budget());
        for ((s, b), o) in skip.iter_mut().zip(blocks).zip(out) {
            if o && b.2 && !s.1 {
                s.1 = true;
                self.ruled_out.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The run's .dna, when the store has the DNA index on and the file fits the archive.
    fn open(&self) -> Option<Dna> {
        let home = store_home(self.archive)?;
        if !settings(&home).on {
            return None;
        }
        let path = dna_path(self.archive);
        let unused = |why: String| note(&format!("dna: {}: not used: {why}", path.display()));
        match fs::metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                unused(format!("cannot read it: {e}"));
                return None;
            }
            // (a pipe or a device is never read: that could wait forever)
            Ok(m) if !m.is_file() => {
                unused("not a regular file".into());
                return None;
            }
            Ok(m) if m.len() > READ_MAX => {
                unused("too big to be a DNA index".into());
                return None;
            }
            Ok(_) => {}
        }
        let r = fs::read(&path).map_err(|e| format!("cannot read it: {e}"));
        match r.and_then(|d| crate::catalog::archive_id(self.archive).and_then(|(len, end)| Dna::read(d, len, end))) {
            Ok(d) => Some(d),
            Err(e) => {
                unused(e);
                None
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// xorshift64*: a small fixed-seed generator, so every run is the same.
    pub struct Rng(pub u64);

    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        pub fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        pub fn pick<'b, T>(&mut self, v: &'b [T]) -> &'b T {
            &v[self.below(v.len())]
        }
    }

    /// Every piece (substring) of `t` of one byte or more.
    fn every_piece(t: &[u8]) -> Vec<&[u8]> {
        (0..t.len()).flat_map(|s| (s + 1..=t.len()).map(move |e| &t[s..e])).collect()
    }

    /// The rule's examples (ANSWER.md): "refused" and "blk" go in; "1117838570", "4ae9" and
    /// "0x7f3a" stay out; "deadbeef" (no digit) goes in, but its pieces are not looked up.
    #[test]
    fn rule_1_examples() {
        for t in [&b"refused"[..], b"blk", b"deadbeef", b"added", b"x", b"0xg1", b"\x80\x80", b"user42x", b"0xx1"] {
            assert!(!left_out(t), "{:?} goes in", String::from_utf8_lossy(t));
        }
        for t in [&b"1117838570"[..], b"4ae9", b"0x7f3a", b"0", b"7", b"0x", b"0xdeadbeef", b"8c1f0e6a", b"a1"] {
            assert!(left_out(t), "{:?} stays out", String::from_utf8_lossy(t));
        }
        assert!(looked_up(b"refused") && looked_up(b"blk") && looked_up(b"\x80\x80\x80") && looked_up(b"xyz"));
        for p in [&b"dead"[..], b"0x7f", b"4ae9", b"1117838570", b"x7f", b"xx", b"fax", b"abc"] {
            assert!(!looked_up(p), "{:?} is not looked up", String::from_utf8_lossy(p));
        }
    }

    /// Rule 2 is rule 1's complement: every token rule 1 leaves out, and every piece of one,
    /// fails rule 2, so a piece rule 2 looks up is never inside a token that was left out.
    /// Every token of up to 6 bytes over the boundary bytes ('0', '1', '9', 'a', 'f', 'g',
    /// 'x', 'y', 'z', 0x80), then long tokens of the kinds rule 1 is for.
    #[test]
    fn rule_2_is_rule_1s_complement() {
        let alphabet = b"019afgxyz\x80";
        let mut tokens: Vec<Vec<u8>> = vec![Vec::new()];
        let mut short: Vec<Vec<u8>> = Vec::new();
        let (mut out, mut checked) = (0usize, 0usize);
        for len in 1..=6 {
            let mut next = Vec::new();
            for t in &tokens {
                for &c in alphabet {
                    let mut u = t.clone();
                    u.push(c);
                    if left_out(&u) {
                        out += 1;
                        for p in every_piece(&u) {
                            assert!(!looked_up(p), "{:?}, a piece of {:?}, which rule 1 leaves out", p, u);
                            checked += 1;
                        }
                    }
                    next.push(u);
                }
            }
            tokens = next;
            if len <= 4 {
                short.extend(tokens.iter().cloned());
            }
        }
        // and long ones: numbers, hex IDs, UUID groups, hashes, 0x values
        let mut r = Rng(1);
        let digits = b"0123456789";
        let hexd = b"0123456789abcdef";
        for i in 0..20_000 {
            let len = 1 + r.below(64);
            let mut t: Vec<u8> = (0..len).map(|_| if i % 2 == 0 { *r.pick(digits) } else { *r.pick(hexd) }).collect();
            if i % 3 == 0 {
                t.splice(0..0, *b"0x");
            }
            if !left_out(&t) {
                continue;
            }
            out += 1;
            for p in every_piece(&t) {
                assert!(!looked_up(p), "{p:?} of {t:?}");
                checked += 1;
            }
        }
        assert!(out > 30_000 && checked > 1_000_000, "{out} tokens, {checked} pieces");
        // said the other way: a token holding a looked-up piece is never left out
        for t in &short {
            if every_piece(t).iter().any(|p| looked_up(p)) {
                assert!(!left_out(t), "{t:?}");
            }
        }
    }

    #[test]
    fn pieces_of_needles() {
        let s = |v: Vec<Vec<u8>>| v.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>();
        assert_eq!(s(pieces(b"Connection refused")), ["connection", "refused"]);
        // folded; numbers, hex and 0x left out; short ones left out; high bytes as 0x80
        assert_eq!(s(pieces(b"ERROR blk_-1608999687919862906 0x7f3a deadbeef at")), ["error", "blk"]);
        assert_eq!(pieces("café crème".as_bytes()), vec![b"cr\x80\x80me".to_vec(), b"caf\x80\x80".to_vec()]);
        // part of a word, part of an ID
        assert_eq!(s(pieces(b"efused")), ["efused"]);
        assert_eq!(s(pieces(b"req-4fa2c9 user7x")), ["user7x", "req"]);
        // each once, none inside a longer one, at most 3, the longest first
        assert_eq!(s(pieces(b"error ERROR rror errors")), ["errors"]);
        assert_eq!(s(pieces(b"ggg hhhh iiiii jjjjjj kkkkkkk")), ["kkkkkkk", "jjjjjj", "iiiii"]);
        assert_eq!(s(pieces(b"xyz abq")), ["xyz", "abq"], "as long: in the needle's order");
        // nothing to look up
        for n in [&b""[..], b"ab", b"4ae9", b"0x7f3a", b"1117838570", b"dead beef", b"- _ :", b"xx"] {
            assert!(pieces(n).is_empty(), "{:?}", String::from_utf8_lossy(n));
        }
    }

    #[test]
    fn tokens_of_a_text() {
        let mut got = Vec::new();
        kept_tokens(b"081109 INFO dfs.DataNode: blk_-123 0x7F3A Caf\xc3\xa9\nabc4ae9 ABC4AEG 4AE9 r2d2 Added", &mut Vec::new(), &mut |t| got.push(t.to_vec()));
        // (abc4ae9 is hex with a digit: left out; abc4aeg holds a g)
        let want: Vec<&[u8]> = vec![b"info", b"dfs", b"datanode", b"blk", b"caf\x80\x80", b"abc4aeg", b"r2d2", b"added"];
        assert_eq!(got, want);
    }

    #[test]
    fn settings_of_the_config() {
        let lines = |t: &str| crate::store::read_settings(t);
        assert_eq!(settings_of(&lines("")), Settings { on: true, min: 16 << 20 }, "on by default");
        assert!(!settings_of(&lines("dna = off\n")).on);
        assert!(!settings_of(&lines("dna=OFF\ndna = on\n")).on, "the first line counts");
        assert!(settings_of(&lines("dna = on\n")).on);
        assert!(settings_of(&lines("dna = maybe\n")).on, "only off turns it off");
        assert_eq!(settings_of(&lines("dna_min = 1MiB\n")).min, 1 << 20);
        assert_eq!(settings_of(&lines("dna_min = nonsense\n")).min, 16 << 20);
        for (v, n) in [("16MiB", 16 << 20), ("16 MiB", 16 << 20), ("20MB", 20_000_000), ("1GiB", 1 << 30), ("12345", 12345), ("7 b", 7), (" 2 kib ", 2048)] {
            assert_eq!(parse_size(v), Some(n), "{v}");
        }
        for v in ["", "MiB", "16 MiBs", "-1", "1.5MiB", "99999999999999999999"] {
            assert_eq!(parse_size(v), None, "{v}");
        }
    }

    #[test]
    fn paths() {
        assert_eq!(dna_path(Path::new("/h/projects/p-1a2b/r000042.cmf")), Path::new("/h/projects/p-1a2b/r000042.dna"));
        assert_eq!(dna_path(Path::new("/t/x.log.cmf")), Path::new("/t/x.log.dna"));
        assert_eq!(dna_path(Path::new("/t/archive")), Path::new("/t/archive.dna"));
        assert_eq!(store_home(Path::new("/h/projects/p-1a2b/r000042.cmf")), Some(PathBuf::from("/h")));
        assert_eq!(store_home(Path::new("/t/x.cmf")), None);
        assert_eq!(store_home(Path::new("x.cmf")), None);
    }

    // ------------------------------------------------ segments made from texts, no archive

    /// One segment's record made from these blocks' texts (header made up), and read back.
    pub fn record_of(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut g = Gather::default();
        for b in blocks {
            g.block(b);
        }
        let raw: u64 = blocks.iter().map(|b| b.len() as u64).sum();
        let mut out = Vec::new();
        g.finish((raw, 1, 7), &mut out).unwrap();
        let p = &mut 0;
        let k = get_varint(&out, p).unwrap() as usize;
        assert_eq!(*p + k, out.len());
        out[*p..].to_vec()
    }

    /// The blocks a needle's pieces rule out, by a plain scan of the blocks' tokens (what
    /// `rule_out` must give when no piece passes the budget or the place cap).
    fn plain_rule_out(blocks: &[Vec<u8>], pieces: &[Vec<u8>]) -> Vec<bool> {
        let toks: Vec<BTreeSet<Vec<u8>>> = blocks
            .iter()
            .map(|b| {
                let mut s = BTreeSet::new();
                kept_tokens(b, &mut Vec::new(), &mut |t| {
                    s.insert(t.to_vec());
                });
                s
            })
            .collect();
        let holds = |b: usize, p: &[u8]| toks[b].iter().any(|t| t.windows(p.len()).any(|w| w == p));
        let any_holds = |p: &[u8]| (0..blocks.len()).any(|b| holds(b, p));
        (0..blocks.len())
            .map(|b| {
                pieces.iter().take(MAX_PIECES).any(|p| !any_holds(p)) || (blocks.len() > 1 && pieces.iter().take(MAX_PIECES).any(|p| !holds(b, p)))
            })
            .collect()
    }

    /// Whether some line wholly inside the block text holds the needle, without case (the
    /// DNA index folds case, so a block it rules out holds the needle in no case).
    fn holds_needle(block: &[u8], needle: &[u8]) -> bool {
        let n = needle.to_ascii_lowercase();
        block.split(|&c| c == b'\n').any(|l| n.is_empty() || l.to_ascii_lowercase().windows(n.len()).any(|w| w == &n[..]))
    }

    /// Random log-like blocks: words that change from block to block, IDs, numbers, hex,
    /// 0x values, high bytes.
    pub fn random_blocks(r: &mut Rng, nb: usize, lines: usize) -> Vec<Vec<u8>> {
        let common = ["info", "error", "connection", "refused", "server", "blk", "user", "request", "timeout", "Caf\u{e9}"];
        let mut out = Vec::new();
        for b in 0..nb {
            let own: Vec<String> = (0..4).map(|k| format!("w{}{}x{}", ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"][(b + k) % 6], b, k)).collect();
            let mut t = String::new();
            for _ in 0..lines {
                let words = 2 + r.below(6);
                for w in 0..words {
                    if w > 0 {
                        t.push(' ');
                    }
                    match r.below(9) {
                        0 => t.push_str(&format!("{}", r.next() % 10_000_000_000)),
                        1 => t.push_str(&format!("{:x}", r.next() >> 16)),
                        2 => t.push_str(&format!("0x{:x}", r.next() % 0xffff)),
                        3 | 4 => t.push_str(r.pick(&own)),
                        5 => t.push_str(&format!("id{}{}", ["q", "z", "k"][r.below(3)], r.next() % 1000)),
                        _ => t.push_str(r.pick(&common)),
                    }
                }
                t.push('\n');
            }
            out.push(t.into_bytes());
        }
        out
    }

    /// Needles of every kind: pieces of lines (part of a word, part of an ID, several words),
    /// in other case, hex-like, short, with high bytes, absent.
    pub fn random_needles(r: &mut Rng, blocks: &[Vec<u8>], k: usize) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = vec![b"refused".to_vec(), b"onnect".to_vec(), b"wbravo1x".to_vec(), b"Caf\xc3\xa9".to_vec(), b"nowhere at all".to_vec(), b"0x1f".to_vec(), b"zz".to_vec()];
        for _ in 0..k {
            let b = r.pick(blocks);
            let s = r.below(b.len());
            let len = 1 + r.below(24);
            let mut n = b[s..(s + len).min(b.len())].to_vec();
            if n.contains(&b'\n') {
                n.truncate(n.iter().position(|&c| c == b'\n').unwrap());
            }
            match r.below(5) {
                0 => n.make_ascii_uppercase(),
                1 => n.push(b'q'),
                _ => {}
            }
            out.push(n);
        }
        out
    }

    #[test]
    fn segments_rule_out_exactly_the_blocks_without_a_piece() {
        let mut r = Rng(5);
        let (mut ruled, mut total) = (0, 0);
        for round in 0..40 {
            let nb = [1, 2, 3, 7, 9, 17, 70][round % 7];
            let lines = 1 + r.below(60);
            let blocks = random_blocks(&mut r, nb, lines);
            let rec = record_of(&blocks);
            let seg = Seg::parse(&rec).unwrap();
            assert!(seg.indexed());
            assert_eq!(seg.blocks.len(), nb);
            for needle in random_needles(&mut r, &blocks, 60) {
                let ps = pieces(&needle);
                let got = seg.rule_out(&ps, usize::MAX);
                // never a block where a line holds the needle
                for (b, &out) in got.iter().enumerate() {
                    assert!(!(out && holds_needle(&blocks[b], &needle)), "block {b} holds {:?}", String::from_utf8_lossy(&needle));
                }
                // exactly the blocks a plain scan of the tokens rules out (no piece here has
                // over MAX_PLACES places)
                assert_eq!(got, plain_rule_out(&blocks, &ps), "{:?} in {nb} blocks", String::from_utf8_lossy(&needle));
                ruled += got.iter().filter(|&&x| x).count();
                total += nb;
            }
        }
        assert!(ruled > total / 4, "{ruled} of {total} blocks ruled out");
    }

    /// The step budget and the place cap: a piece whose walks would pass the budget, or that
    /// has over MAX_PLACES places, rules nothing out; with room, it rules out exactly.
    #[test]
    fn step_budget_and_place_cap() {
        // long tokens: a piece far from its token's start takes many steps
        let long = |c: char, n: usize| -> String { std::iter::repeat_n(c, n).collect() };
        let blocks: Vec<Vec<u8>> = vec![
            format!("{}needle {}\n", long('q', 3000), long('z', 10)).into_bytes(),
            format!("{}other\n", long('q', 3000)).into_bytes(),
            b"plain words here\n".to_vec(),
        ];
        let seg_rec = record_of(&blocks);
        let seg = Seg::parse(&seg_rec).unwrap();
        let p = pieces(b"needle");
        assert_eq!(seg.rule_out(&p, usize::MAX), vec![false, true, true]);
        // the place of "needle" is 3,000 bytes into its token: 3,001 steps
        assert_eq!(seg.rule_out(&p, 3001), vec![false, true, true]);
        assert_eq!(seg.rule_out(&p, 3000), vec![false; 3], "past the budget: nothing ruled out");
        // a piece no token holds needs no walk at all
        assert_eq!(seg.rule_out(&pieces(b"absent"), 0), vec![true; 3]);
        // one piece over the budget, another within it: the other still rules out
        let two = pieces(b"qneedle zzz");
        assert_eq!(seg.rule_out(&two, 50), vec![false, true, true], "zzz: few steps; qneedle: many");
        // over MAX_PLACES places: not used
        let mut many = String::new();
        for i in 0..MAX_PLACES + 1 {
            many.push_str(&format!("tok{i}q "));
        }
        let blocks = vec![format!("{many}\n").into_bytes(), b"nothing\n".to_vec()];
        let seg_rec = record_of(&blocks);
        let seg = Seg::parse(&seg_rec).unwrap();
        assert_eq!(seg.rule_out(&pieces(b"tok"), usize::MAX), vec![false, false], "4,097 places: not used");
        assert_eq!(seg.rule_out(&pieces(b"tok4096q"), usize::MAX), vec![false, true]);
    }

    /// The first token and the last, and tokens that are pieces of each other: walked to the
    /// right token in every form.
    #[test]
    fn first_and_last_tokens() {
        // (letters past f: hex letters alone are not looked up)
        let blocks: Vec<Vec<u8>> = vec![b"ggg gggh zzz\n".to_vec(), b"ggh zzzz\n".to_vec(), b"mmm\n".to_vec()];
        let rec = record_of(&blocks);
        let seg = Seg::parse(&rec).unwrap();
        for (needle, want) in [
            (&b"ggg"[..], vec![false, true, true]),
            (b"ggh", vec![false, false, true]),
            (b"gggh", vec![false, true, true]),
            (b"zzz", vec![false, false, true]),
            (b"zzzz", vec![true, false, true]),
            (b"mmm", vec![true, true, false]),
        ] {
            assert_eq!(seg.rule_out(&pieces(needle), usize::MAX), want, "{:?}", String::from_utf8_lossy(needle));
        }
    }

    /// Both forms of token-blocks, bitmaps and lists, give the same answers, and the smaller
    /// one is written.
    #[test]
    fn bitmaps_and_lists_agree() {
        let mut r = Rng(9);
        let mut forms = BTreeSet::new();
        for nb in [2, 8, 9, 64, 130, 300] {
            let blocks = random_blocks(&mut r, nb, 3);
            let mut g = Gather::default();
            for b in &blocks {
                g.block(b);
            }
            // the tokens in the record's order: by their bytes
            let mut order: Vec<(&Box<[u8]>, &u32)> = g.ids.iter().collect();
            order.sort_unstable();
            let lists: Vec<&[u32]> = order.iter().map(|(_, id)| &g.blocks_of[**id as usize][..]).collect();
            let (bm, ls) = (bitmaps(&lists, nb), list_form(&lists).unwrap());
            let rec = record_of(&blocks);
            let seg = Seg::parse(&rec).unwrap();
            let written = match seg.index.as_ref().unwrap().lists {
                TokenBlocks::Bitmaps(_) => FORM_BITMAPS,
                TokenBlocks::Lists(..) => FORM_LISTS,
                TokenBlocks::One => FORM_ONE,
            };
            assert_eq!(written, if ls.len() < bm.len() { FORM_LISTS } else { FORM_BITMAPS }, "{nb} blocks");
            forms.insert(written);
            // the other form, put in its place, answers the same
            let ix = seg.index.as_ref().unwrap();
            let other = SegIndex {
                tokens: ix.tokens,
                fm: ix.fm.clone(),
                sep: ix.sep,
                lists: if written == FORM_LISTS {
                    TokenBlocks::Bitmaps(&bm)
                } else {
                    let k = 4 * ix.tokens.div_ceil(LIST_STEP);
                    TokenBlocks::Lists(ls[..k].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect(), &ls[k..])
                },
            };
            let seg2 = Seg { header: seg.header, blocks: seg.blocks.clone(), index: Some(other) };
            for needle in random_needles(&mut r, &blocks, 40) {
                let ps = pieces(&needle);
                assert_eq!(seg.rule_out(&ps, usize::MAX), seg2.rule_out(&ps, usize::MAX));
            }
        }
        assert_eq!(forms.len(), 2, "both forms were written");
    }

    /// A segment whose tokens make too much text is not indexed, and rules nothing out.
    #[test]
    fn too_many_tokens_are_not_indexed() {
        let mut g = Gather::default();
        let mut t = String::new();
        let mut i = 0u64;
        while t.len() < MAX_TEXT + (1 << 20) {
            t.push_str(&format!("tok{i}qz "));
            i += 1;
        }
        t.push('\n');
        g.block(t.as_bytes());
        g.block(b"refused\n");
        assert!(g.too_big && g.ids.is_empty());
        let mut out = Vec::new();
        g.finish((t.len() as u64 + 8, 1, 2), &mut out).unwrap();
        let p = &mut 0;
        get_varint(&out, p).unwrap();
        let seg = Seg::parse(&out[*p..]).unwrap();
        assert!(!seg.indexed());
        assert_eq!(seg.rule_out(&pieces(b"refused"), usize::MAX), vec![false, false]);
        assert_eq!(seg.rule_out(&pieces(b"absent"), usize::MAX), vec![false, false]);
    }

    /// A fresh folder for a test, inside this crate's target folder.
    fn tmp(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-dna-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// An archive of these texts, one segment each (engine::pack, as fewer-pack packs a segment).
    fn archive_of(texts: &[Vec<u8>]) -> Vec<u8> {
        let mut aw = crate::container::ArchiveWriter::new(Vec::new(), 0, &[]).unwrap();
        for t in texts {
            aw.segment(t.len() as u64, &engine::pack(t, 1)).unwrap();
        }
        aw.finish().unwrap().0
    }

    /// The segment headers of an archive.
    fn headers(archive: &Path) -> Vec<(u64, u64, u32)> {
        let (mut ar, _) = crate::grep::open_archive(archive.as_os_str(), false).unwrap();
        let mut hs = Vec::new();
        while let Some(h) = ar.next_header().unwrap() {
            ar.skip(h).unwrap();
            hs.push(h);
        }
        hs
    }

    /// A store's archive of several segments with its DNA index beside it: `Search::skips`
    /// leaves unopened only blocks that hold no line with the needle, and only at an edge; and
    /// nothing when the store has the DNA index off, when the .dna is another archive's,
    /// damaged, of another version or missing, or when a segment does not fit.
    #[test]
    fn search_uses_only_a_fitting_index_when_it_is_on() {
        let home = tmp("search");
        let dir = home.join("projects").join("app-1a2b");
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("r000001.cmf");
        let mut r = Rng(21);
        let texts = random_blocks(&mut r, 6, 40);
        fs::write(&archive, archive_of(&texts)).unwrap();
        let good = build(&archive, 2, &mut || true).unwrap();
        fs::write(dna_path(&archive), &good).unwrap();
        let hs = headers(&archive);
        assert_eq!(hs.len(), 6);
        let blocks = |g: usize| (texts[g].len() as u64, texts[g].ends_with(b"\n"));
        let ask = |needle: &[u8]| -> Vec<bool> {
            let ps = pieces(needle);
            let s = Search::new(&archive, &ps);
            let out: Vec<bool> = hs
                .iter()
                .enumerate()
                .map(|(g, &h)| {
                    let (n, ends) = blocks(g);
                    let mut v = vec![(n as usize, false)];
                    s.skips(g, h, &[(n, ends, true)], &mut v);
                    v[0].1
                })
                .collect();
            assert_eq!(s.ruled_out.load(Ordering::Relaxed), out.iter().filter(|&&x| x).count());
            out
        };
        // exact on random needles; and a word of one segment rules out the others
        let mut ruled = 0;
        for needle in random_needles(&mut r, &texts, 200) {
            for (g, skip) in ask(&needle).into_iter().enumerate() {
                assert!(!(skip && holds_needle(&texts[g], &needle)), "segment {g} holds {:?}", String::from_utf8_lossy(&needle));
                ruled += skip as usize;
            }
        }
        assert!(ruled > 100, "{ruled}");
        let one = vec![true, false, true, true, true, true];
        assert_eq!(ask(b"wbravo1x0"), one);
        assert_eq!(ask(b"WBRAVO1X0"), one, "-i: the index folds case");
        assert_eq!(ask(b"wbravo1x0 says"), vec![true; 6], "no segment holds \"says\"");
        assert_eq!(ask(b"4ae9"), vec![false; 6], "no piece to look up");
        // not at an edge, or already left unopened by the .idx: not ruled out (or counted) again
        let ps = pieces(b"wbravo1x0");
        let s = Search::new(&archive, &ps);
        let (n, ends) = blocks(0);
        let mut v = vec![(n as usize, false)];
        s.skips(0, hs[0], &[(n, ends, false)], &mut v);
        assert!(!v[0].1);
        let mut v = vec![(n as usize, true)];
        s.skips(0, hs[0], &[(n, ends, true)], &mut v);
        assert_eq!(s.ruled_out.load(Ordering::Relaxed), 0);
        // a segment that does not fit: another header, other block sizes
        let mut v = vec![(n as usize, false)];
        s.skips(0, hs[1], &[(n, ends, true)], &mut v);
        assert!(!v[0].1);
        s.skips(0, hs[0], &[(n + 1, ends, true)], &mut v);
        assert!(!v[0].1);
        // the DNA index off in the store's config: nothing; on again: as before
        fs::write(home.join("config"), "# mine\ndna = off\n").unwrap();
        assert_eq!(ask(b"wbravo1x0"), vec![false; 6]);
        fs::write(home.join("config"), "# mine\ndna = on\n").unwrap();
        assert_eq!(ask(b"wbravo1x0"), one);
        // another archive's DNA index, a damaged one, another version, none: nothing
        let other = dir.join("r000002.cmf");
        fs::write(&other, archive_of(&texts[..5])).unwrap();
        fs::write(dna_path(&archive), build(&other, 1, &mut || true).unwrap()).unwrap();
        assert_eq!(ask(b"wbravo1x0"), vec![false; 6]);
        for at in [5, good.len() / 2, good.len() - 1] {
            let mut bad = good.clone();
            bad[at] ^= 0x10;
            fs::write(dna_path(&archive), &bad).unwrap();
            assert_eq!(ask(b"wbravo1x0"), vec![false; 6], "a byte changed at {at}");
        }
        let mut v2 = good.clone();
        v2[4] = 2;
        fs::write(dna_path(&archive), &v2).unwrap();
        assert_eq!(ask(b"wbravo1x0"), vec![false; 6]);
        fs::remove_file(dna_path(&archive)).unwrap();
        assert_eq!(ask(b"wbravo1x0"), vec![false; 6]);
        // an archive outside a store is never searched with a DNA index
        let lone = home.join("lone.cmf");
        fs::copy(&archive, &lone).unwrap();
        fs::write(dna_path(&lone), &good).unwrap();
        let ps = pieces(b"wbravo1x0");
        let s = Search::new(&lone, &ps);
        let mut v = vec![(n as usize, false)];
        s.skips(0, hs[0], &[(n, ends, true)], &mut v);
        assert!(!v[0].1);
        fs::remove_dir_all(&home).unwrap();
    }

    /// The file: its segments are the archive's, joined archives included; read back it
    /// gives the blocks the archive has; refused for another archive, a cut, any changed
    /// byte and another version; and its building stops when asked to.
    #[test]
    fn the_file_fits_its_archive_only() {
        let home = tmp("file");
        let mut r = Rng(23);
        let texts = random_blocks(&mut r, 4, 30);
        let (a, b) = (home.join("a.cmf"), home.join("ab.cmf"));
        let one = archive_of(&texts[..2]);
        fs::write(&a, &one).unwrap();
        // joined archives: `cat a.cmf b.cmf`
        fs::write(&b, [one.clone(), archive_of(&texts[2..])].concat()).unwrap();
        for (path, n) in [(&a, 2), (&b, 4)] {
            let d = build(path, 1, &mut || true).unwrap();
            let (len, end) = crate::catalog::archive_id(path).unwrap();
            let dna = Dna::read(d.clone(), len, end).unwrap();
            let info = dna.info().unwrap();
            assert_eq!(info.len(), n);
            for (g, h) in headers(path).into_iter().enumerate() {
                let s = dna.segment(g).unwrap();
                assert_eq!((s.header, s.blocks.clone()), (h, vec![(texts[g].len() as u64, true)]));
                assert_eq!(info[g].form, FORM_ONE);
            }
            assert!(Dna::read(d.clone(), len + 1, end).unwrap_err().contains("another archive"));
            assert!(Dna::read(d.clone(), len, [0, 0, 0, 0]).unwrap_err().contains("another archive"));
            for cut in [0, 4, 9, d.len() / 2, d.len() - 1] {
                assert!(Dna::read(d[..cut].to_vec(), len, end).is_err(), "cut at {cut}");
            }
            for at in 0..d.len() {
                let mut bad = d.clone();
                bad[at] ^= 1 << (at % 8);
                assert!(Dna::read(bad, len, end).is_err(), "a bit changed at {at}");
            }
            let mut v2 = d.clone();
            v2[4] = 9;
            assert!(Dna::read(v2, len, end).unwrap_err().contains("version 9"));
            assert!(made_for(&{
                let f = home.join("x.dna");
                fs::write(&f, &d).unwrap();
                f
            }, len, end));
        }
        // stopped between segments: no file
        let mut asked = 0;
        assert_eq!(build(&b, 1, &mut || {
            asked += 1;
            asked <= 2
        })
        .unwrap_err(), STOPPED);
        fs::remove_dir_all(&home).unwrap();
    }

    /// A damaged segment record is refused or, when its parts still fit, answers without a
    /// panic and always ends (the file's CRC refuses it before it is ever used).
    #[test]
    fn damaged_segments_are_refused_or_safe() {
        let mut r = Rng(11);
        let blocks = random_blocks(&mut r, 9, 20);
        let good = record_of(&blocks);
        for cut in 0..good.len() {
            assert!(Seg::parse(&good[..cut]).is_err(), "cut at {cut}");
        }
        let ps = pieces(b"error refused wbravo1x");
        for round in 0..3000 {
            let mut bad = good.clone();
            for _ in 0..1 + round % 3 {
                let at = r.below(bad.len());
                bad[at] ^= 1 << r.below(8);
            }
            if let Ok(seg) = Seg::parse(&bad) {
                let _ = seg.rule_out(&ps, 10_000);
            }
        }
    }
}
