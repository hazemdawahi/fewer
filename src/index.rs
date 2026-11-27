//! The search index (`fewer-pack index FILE.cmf...`): FILE.cmf.idx next to an archive, so that
//! `fewer-pack grep` leaves the blocks that cannot hold the text unread and undecoded (a year of
//! daily archives is searched for an ID by opening only the blocks that may hold it).
//!
//! For every block of every segment it keeps the block's line count and an xor filter
//! (8-bit fingerprints, about 9.8 bits per key, 1 false "maybe" in 256) of the block's
//! tokens:
//!
//! - a token is a longest run of ASCII letters, digits and bytes >= 0x80; any other byte
//!   (space, punctuation, a line end) ends one. Tokens are compared in lowercase, so one
//!   index serves grep and grep -i.
//! - a token's keys: the token itself when shorter than K = 8 bytes (none for a number of
//!   fewer than 8 digits), else its first and its last 8 bytes.
//!
//! A line holding the text holds every run of token bytes of the text that has a non-token
//! byte before and after it (in the text) as a whole token; a run at the text's end with one
//! before it as the start of a token; a run at its start with one after it as the end of a
//! token. `needle_keys` takes the keys of exactly those, so every block with such a line has
//! them all in its filter: a block whose filter lacks one is skipped, and nothing is ever
//! missed. A text that is one run of token bytes (a piece of one token) has no keys and
//! skips nothing.
//!
//! Format (version 1; numbers are varints, CRCs 4 bytes little-endian):
//!
//!   index = "CMLI" 01 archive-bytes segments seg* CRC-32 (of all the bytes before it)
//!   seg   = raw packed CRC   (the archive's segment header: a segment is searched with the
//!                             index only when the archive has this very header there)
//!           blocks block*
//!   block = raw lines flags [seed (8 bytes) third fingerprint*(3 third)]
//!           lines: its '\n' bytes; flags 1: its text ends with '\n'; 2: a filter follows
//!
//! A block is skipped only after a '\n' (or at the start) and when its text ends with one
//! (or it is the archive's last), so no line runs into or out of it.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use crate::container::{ArchiveReader, get_varint, put_varint};
use crate::crc::crc32;
use crate::engine::unpack_blocks;

const MAGIC: &[u8; 5] = b"CMLI\x01";
const K: usize = 8;
const DAMAGED: &str = "damaged";

fn tok(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c >= 0x80
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ z >> 30).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ z >> 27).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ z >> 31
}

/// A key: a token or a piece of one, in lowercase (never 0).
fn hash(s: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &c in s {
        h = (h ^ c.to_ascii_lowercase() as u64).wrapping_mul(0x100_0000_01b3);
    }
    mix(h) | 1
}

/// Each run of token bytes of `t`, and whether it starts `t` / ends `t`.
fn runs(t: &[u8], mut f: impl FnMut(&[u8], bool, bool)) {
    let mut i = 0;
    while i < t.len() {
        if !tok(t[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < t.len() && tok(t[i]) {
            i += 1;
        }
        f(&t[s..i], s == 0, i == t.len());
    }
}

/// A token's keys (none for a number shorter than K digits: too common to rule out much).
fn keys(r: &[u8], mut put: impl FnMut(u64)) {
    if r.len() < K {
        if !r.iter().all(u8::is_ascii_digit) {
            put(hash(r));
        }
    } else {
        put(hash(&r[..K]));
        put(hash(&r[r.len() - K..]));
    }
}

/// The keys that every line holding `needle` holds (see the top).
pub fn needle_keys(needle: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    runs(needle, |r, first, last| match (first, last) {
        (false, false) => keys(r, |h| out.push(h)),
        (false, true) if r.len() >= K => out.push(hash(&r[..K])),
        (true, false) if r.len() >= K => out.push(hash(&r[r.len() - K..])),
        _ => {}
    });
    out
}

type Fp = u8;

fn slots(h: u64, b: usize) -> [usize; 3] {
    let r = |x: u64| ((x as u32 as u64 * b as u64) >> 32) as usize;
    [r(h), b + r(h.rotate_left(21)), 2 * b + r(h.rotate_left(42))]
}

fn fpr(h: u64) -> Fp {
    (h ^ h >> 32) as Fp
}

/// An xor filter of distinct keys (Graf and Lemire): its seed and 3 * third fingerprints.
fn xor_build(keys: &[u64]) -> (u64, Vec<Fp>) {
    let b = (keys.len() + keys.len() / 4 + 32) / 3;
    let mut seed = 0u64;
    loop {
        seed = mix(seed.wrapping_add(0x9e37_79b9_7f4a_7c15));
        let (mut cnt, mut xs) = (vec![0u32; 3 * b], vec![0u64; 3 * b]);
        for &k in keys {
            let h = mix(k ^ seed);
            for s in slots(h, b) {
                cnt[s] += 1;
                xs[s] ^= h;
            }
        }
        // peel: a slot of one key gives that key its place, until none is left
        let mut q: Vec<usize> = (0..3 * b).filter(|&i| cnt[i] == 1).collect();
        let mut st = Vec::with_capacity(keys.len());
        while let Some(i) = q.pop() {
            if cnt[i] != 1 {
                continue;
            }
            let h = xs[i];
            st.push((h, i));
            for s in slots(h, b) {
                cnt[s] -= 1;
                xs[s] ^= h;
                if cnt[s] == 1 {
                    q.push(s);
                }
            }
        }
        if st.len() == keys.len() {
            let mut fp = vec![0; 3 * b];
            for &(h, i) in st.iter().rev() {
                let [x, y, z] = slots(h, b);
                fp[i] = fpr(h) ^ fp[x] ^ fp[y] ^ fp[z];
            }
            return (seed, fp);
        }
    }
}

fn has(seed: u64, fp: &[Fp], k: u64) -> bool {
    let h = mix(k ^ seed);
    let [x, y, z] = slots(h, fp.len() / 3);
    fpr(h) == fp[x] ^ fp[y] ^ fp[z]
}

/// Add key k (not 0) to the set t of n keys (open addressing, grown at half full).
fn add(t: &mut Vec<u64>, n: &mut usize, k: u64) {
    if 2 * *n >= t.len() {
        let old = std::mem::replace(t, vec![0; (2 * t.len()).max(1 << 12)]);
        *n = 0;
        for k in old.into_iter().filter(|&k| k != 0) {
            add(t, n, k);
        }
    }
    let m = t.len() - 1;
    let mut i = (k >> 1) as usize & m;
    while t[i] != k {
        if t[i] == 0 {
            t[i] = k;
            *n += 1;
            return;
        }
        i = (i + 1) & m;
    }
}

/// One block's record (its text t): its lines counted, the filter of its keys made.
fn block(t: &[u8], o: &mut Vec<u8>) {
    let ends = t.last() == Some(&b'\n');
    put_varint(o, t.len() as u64);
    put_varint(o, t.iter().filter(|&&c| c == b'\n').count() as u64);
    o.push(2 | ends as u8);
    // the distinct keys (a small table of recent keys first: most tokens repeat)
    let (mut seen, mut set, mut n) = (vec![0u64; 4096], Vec::new(), 0);
    runs(t, |r, _, _| {
        keys(r, |h| {
            let s = &mut seen[(h >> 52) as usize];
            if *s != h {
                *s = h;
                add(&mut set, &mut n, h);
            }
        })
    });
    set.retain(|&k| k != 0);
    let (seed, fp) = xor_build(&set);
    o.extend_from_slice(&seed.to_le_bytes());
    put_varint(o, fp.len() as u64 / 3);
    o.extend_from_slice(&fp);
}

/// The index of an archive file of `len` bytes (with the archives joined after it), its
/// blocks opened on `threads` threads.
#[cfg_attr(fewer_small, optimize(size))]
pub fn build<R: Read>(mut ar: ArchiveReader<R>, len: u64, threads: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut n = 0u64;
    loop {
        while let Some(h) = ar.next_header()? {
            n += 1;
            put_varint(&mut body, h.0);
            put_varint(&mut body, h.1);
            body.extend_from_slice(&h.2.to_le_bytes());
            let (mut nb, mut recs) = (0, Vec::new());
            let r = unpack_blocks(&ar.payload(h)?, threads, None, true, &mut |t, _| {
                nb += 1;
                block(t.unwrap_or_default(), &mut recs);
                Ok(true)
            });
            if r.map_err(|e| format!("damaged archive (segment {n}: {e})"))? != h.0 {
                return Err(format!("damaged archive (segment {n} unpacked to the wrong size)"));
            }
            put_varint(&mut body, nb);
            body.extend(recs);
        }
        let folder = ar.is_folder();
        match ar.next_member()? {
            Some(next) if !folder && !next.is_folder() => ar = next,
            Some(_) => return Err("unexpected data after the end of the archive".into()),
            None => break,
        }
    }
    let mut o = MAGIC.to_vec();
    put_varint(&mut o, len);
    put_varint(&mut o, n);
    o.extend(body);
    let c = crc32(&o);
    o.extend_from_slice(&c.to_le_bytes());
    Ok(o)
}

/// A block of an index: its size, whether its text ends with '\n', its filter.
type IdxBlock = (usize, bool, Option<(u64, Vec<Fp>)>);

/// An index read back (load).
pub struct Index {
    segs: Vec<((u64, u64, u32), Vec<IdxBlock>)>,
}

fn var(b: &[u8], p: &mut usize) -> Result<u64, &'static str> {
    get_varint(b, p).ok_or(DAMAGED)
}

fn take<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8], &'static str> {
    let r = b.get(*p..).and_then(|r| r.get(..n)).ok_or(DAMAGED)?;
    *p += n;
    Ok(r)
}

#[cfg_attr(fewer_small, optimize(size))]
fn parse(d: &[u8], len: u64) -> Result<Index, &'static str> {
    if d.len() < 9 || d[..4] != MAGIC[..4] {
        return Err("not a fewer-pack index");
    }
    if d[4] != MAGIC[4] {
        return Err("made by another version of fewer-pack");
    }
    let (d, c) = d.split_at(d.len() - 4);
    if crc32(d).to_le_bytes() != c {
        return Err(DAMAGED);
    }
    let p = &mut 5;
    if var(d, p)? != len {
        return Err(crate::MISMATCH);
    }
    let mut segs = Vec::new();
    for _ in 0..var(d, p)? {
        let hdr = (var(d, p)?, var(d, p)?, u32::from_le_bytes(take(d, p, 4)?.try_into().unwrap()));
        let mut blocks = Vec::new();
        for _ in 0..var(d, p)? {
            let (raw, _lines, flags) = (var(d, p)? as usize, var(d, p)?, take(d, p, 1)?[0]);
            let mut f = None;
            if flags & 2 != 0 {
                let seed = u64::from_le_bytes(take(d, p, 8)?.try_into().unwrap());
                let b = var(d, p)? as usize;
                if b == 0 {
                    return Err(DAMAGED);
                }
                f = Some((seed, take(d, p, b.checked_mul(3).ok_or(DAMAGED)?)?.to_vec()));
            }
            blocks.push((raw, flags & 1 != 0, f));
        }
        segs.push((hdr, blocks));
    }
    if *p != d.len() {
        return Err(DAMAGED);
    }
    Ok(Index { segs })
}

/// FILE.cmf.idx for an archive of `len` bytes; None when there is none (silently) or it is
/// damaged, of another version or for another archive (with a warning).
#[cfg_attr(fewer_small, optimize(size))]
pub fn load(path: &Path, len: u64) -> Option<Index> {
    let r = match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        // (a pipe or device is never read: that could wait forever)
        Ok(m) if !m.is_file() => Err("not a regular file".into()),
        _ => fs::read(path).map_err(|e| format!("cannot read: {e}")).and_then(|d| parse(&d, len).map_err(|e| e.to_string())),
    };
    r.map_err(|e| eprintln!("fewer-pack: warning: {}: {e}; searching without it", path.display())).ok()
}

impl Index {
    /// Whether no block of the archive can hold a text with these keys.
    pub fn rules_out_all(&self, keys: &[u64]) -> bool {
        (0..self.segs.len()).all(|g| self.skips(g, self.segs[g].0, keys).is_some_and(|v| v.iter().all(|b| b.1)))
    }

    /// The archive's segment number g (from 0, over joined archives), with header h: for
    /// each block its size and whether no line of it can hold a text with these keys (and
    /// the text before it ends with '\n'); None when the index has not this very segment.
    pub fn skips(&self, g: usize, h: (u64, u64, u32), keys: &[u64]) -> Option<Vec<(usize, bool)>> {
        let (hdr, blocks) = self.segs.get(g)?;
        if *hdr != h {
            return None;
        }
        let mut prev = g == 0 || self.segs[g - 1].1.last().is_some_and(|b| b.1);
        let last = g + 1 == self.segs.len();
        let v = blocks.iter().enumerate().map(|(i, (raw, ends, f))| {
            let edge = prev && (*ends || last && i + 1 == blocks.len());
            let skip = edge && f.as_ref().is_some_and(|(seed, fp)| keys.iter().any(|&k| !has(*seed, fp, k)));
            prev = *ends;
            (*raw, skip)
        });
        Some(v.collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Random lines of tokens and separators in blocks: whenever a line of a block holds the
    /// needle (with and without -i), every needle key is in the block's filter.
    #[test]
    fn index_never_skips_a_match() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let alpha = b"aAbB0123456789xyzXYZ \t.-_:/=\xc3\xa9";
        let (mut checked, mut skipped) = (0, 0);
        for _ in 0..300 {
            let lines: Vec<Vec<u8>> = (0..1 + rnd(20)).map(|_| (0..rnd(60)).map(|_| alpha[rnd(alpha.len())]).collect()).collect();
            let text: Vec<u8> = lines.iter().flat_map(|l| l.iter().copied().chain(*b"\n")).collect();
            let (mut ks, mut n) = (Vec::new(), 0);
            runs(&text, |r, _, _| keys(r, |h| add(&mut ks, &mut n, h)));
            ks.retain(|&k| k != 0);
            assert_eq!(ks.len(), n);
            let (seed, fp) = xor_build(&ks);
            for &k in &ks {
                assert!(has(seed, &fp, k));
            }
            for _ in 0..50 {
                let l = &lines[rnd(lines.len())];
                let mut nd: Vec<u8> = if rnd(3) == 0 || l.is_empty() {
                    (0..1 + rnd(20)).map(|_| alpha[rnd(alpha.len())]).collect()
                } else {
                    let a = rnd(l.len());
                    l[a..(a + 1 + rnd(30)).min(l.len())].to_vec()
                };
                let icase = rnd(2) == 0;
                if icase {
                    nd = nd.iter().map(|&c| if rnd(2) == 0 { c.to_ascii_uppercase() } else { c }).collect();
                }
                let low = |v: &[u8]| if icase { v.to_ascii_lowercase() } else { v.to_vec() };
                let n2 = low(&nd);
                let holds = lines.iter().any(|ln| low(ln).windows(n2.len()).any(|w| w == &n2[..]));
                let may = needle_keys(&nd).iter().all(|&k| has(seed, &fp, k));
                assert!(!holds || may, "skipped a match: {:?}", String::from_utf8_lossy(&nd));
                checked += 1;
                skipped += !may as usize;
            }
        }
        assert!(skipped > checked / 20, "{skipped} of {checked}");
        // a token's start at the text's end, its end at the start, a whole one inside
        assert_eq!(needle_keys(b"blk_-1608999687919862906"), vec![hash(b"16089996")]);
        assert_eq!(needle_keys(b"user-1608999687919862906 x"), vec![hash(b"16089996"), hash(b"19862906")]);
        assert_eq!(needle_keys(b"ERROR 1608999687919862906"), vec![hash(b"16089996")]);
        assert_eq!(needle_keys(b"1608999687919862906 x"), vec![hash(b"19862906")]);
        assert!(needle_keys(b"1608999687919862906").is_empty() && needle_keys(b"").is_empty());
        assert_eq!(needle_keys(b"a 1234567 b x"), vec![hash(b"b")]);
    }
}
