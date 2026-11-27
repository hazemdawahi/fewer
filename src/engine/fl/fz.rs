//! fz: a fast coder for the engine's streams (method 6 of ozip's format), our own code.
//! The copies of ozip's fast parse (parse5: hash buckets, repeat distances, one lazy step),
//! coded with static tables instead of bit-by-bit learning:
//! - literals: rANS with 12-bit frequencies, cut into 1-8 runs (by count) decoded side by side; optionally
//!   2 tables chosen by the top bit of the literal before (in a stream of varints it says "this
//!   byte continues a number"),
//! - each copy as three codes (literal count, copy length, distance: a repeat or a new one)
//!   with one table each, three rANS states sharing one word stream, plus extra bits.
//! Tables are stored as the present symbols (gaps or a bitmap, whichever is shorter) and their
//! frequencies but the last. Decoding is table lookups and copies only.
//!
//! Format: 6, varint(n), varint(copies), varint(literals), literal part, copy part.
//! A table may instead be one byte, 0 or 1: the table of its slot in the unit's bank (Bank;
//! 0: the stream's own, 1: the one shared by all streams).

use super::ozip::{get_varint, next_reps, parse5_level, put_varint, Seq};

const PB: u32 = 12;
const PM: u32 = 1 << PB;
const RL: u32 = 1 << 16;
/// Literal runs decoded side by side: 1 for a few literals, up to 8 for many (each run adds
/// a state and a length to the header; more runs decode faster).
fn lanes_of(n: usize) -> usize {
    if n < 512 {
        1
    } else if n < 4096 {
        2
    } else if n < 65536 {
        4
    } else {
        8
    }
}

// ---------------------------------------------------------------- tables
/// Frequencies summing to PM (every symbol that occurs gets at least 1; no symbol gets PM, so
/// a lone symbol gets a partner with frequency 1).
fn normalize(counts: &[u32], f: &mut [u32]) {
    let total: u64 = counts.iter().map(|&c| c as u64).sum();
    if total == 0 {
        f.iter_mut().for_each(|x| *x = 0);
        return;
    }
    let mut sum = 0i64;
    for (x, &c) in f.iter_mut().zip(counts) {
        *x = if c == 0 { 0 } else { ((c as u64 * PM as u64) / total).max(1) as u32 };
        sum += *x as i64;
    }
    let nz = f.iter().filter(|&&x| x > 0).count();
    if nz == 1 {
        let i = f.iter().position(|&x| x > 0).unwrap();
        let j = if i + 1 < f.len() { i + 1 } else { i - 1 };
        f[i] = PM - 1;
        f[j] = 1;
        return;
    }
    // fix the rounding: the largest symbols give or take
    while sum != PM as i64 {
        let mut order: Vec<usize> = (0..f.len()).filter(|&i| f[i] > 0).collect();
        super::sort_stable_by(&mut order, |&a, &b| f[a] > f[b]);
        for &i in &order {
            if sum == PM as i64 {
                break;
            }
            if sum < PM as i64 {
                let add = (PM as i64 - sum).min(f[i] as i64 / 2 + 1);
                f[i] += add as u32;
                sum += add;
            } else if f[i] > 1 {
                let sub = (sum - PM as i64).min(f[i] as i64 / 2);
                f[i] -= sub as u32;
                sum -= sub;
            }
        }
    }
}

/// Bits of a table's symbols coded with frequencies f (counts c).
#[inline(never)]
fn cost_bits(c: &[u32], f: &[u32]) -> f64 {
    let mut b = 0f64;
    for (&n, &q) in c.iter().zip(f) {
        if n > 0 {
            b += n as f64 * (PB as f64 - (q as f64).log2());
        }
    }
    b
}

/// Bytes of v as a varint.
fn vlen(v: usize) -> usize {
    (64 - (v as u64 | 1).leading_zeros() as usize).div_ceil(7)
}

/// A table: varint(present << 1 | form), the present symbols (form 0: gaps as varints, the
/// first from 0; form 1: a bitmap of f.len() bits, whichever is shorter), then each present
/// symbol's frequency - 1 as a varint, except the last one's (the frequencies sum to PM).
/// At least 2 symbols are present (normalize).
fn write_table(out: &mut Vec<u8>, f: &[u32]) {
    let nz = f.iter().filter(|&&x| x > 0).count();
    let (mut gaps, mut prev) = (0usize, 0usize);
    for (i, &x) in f.iter().enumerate() {
        if x > 0 {
            gaps += vlen(i - prev);
            prev = i;
        }
    }
    let bm = f.len().div_ceil(8);
    let use_bm = bm < gaps;
    put_varint(out, (nz as u64) << 1 | use_bm as u64);
    if use_bm {
        let at = out.len();
        out.resize(at + bm, 0);
        for (i, &x) in f.iter().enumerate() {
            if x > 0 {
                out[at + i / 8] |= 1 << (i % 8);
            }
        }
    } else {
        let mut prev = 0usize;
        for (i, &x) in f.iter().enumerate() {
            if x > 0 {
                put_varint(out, (i - prev) as u64);
                prev = i;
            }
        }
    }
    let mut k = 0;
    for &x in f {
        if x > 0 {
            k += 1;
            if k < nz {
                put_varint(out, (x - 1) as u64);
            }
        }
    }
}

/// Read a table written by write_table into f (at most 256 symbols): None unless it is a
/// valid one (2 or more distinct symbols inside f, each with a frequency of 1 or more, summing
/// to PM).
fn read_table(b: &[u8], p: &mut usize, f: &mut [u32]) -> Option<()> {
    let h = get_varint(b, p)?;
    let nz = (h >> 1) as usize;
    if nz < 2 || nz > f.len() || f.len() > 256 {
        return None;
    }
    f.iter_mut().for_each(|x| *x = 0);
    let mut sy = [0usize; 256];
    if h & 1 == 1 {
        let bm = b.get(*p..*p + f.len().div_ceil(8))?;
        *p += bm.len();
        let mut c = 0usize;
        for (k, &byte) in bm.iter().enumerate() {
            for j in 0..8 {
                if byte >> j & 1 == 1 {
                    let i = k * 8 + j;
                    if i >= f.len() || c >= nz {
                        return None;
                    }
                    sy[c] = i;
                    c += 1;
                }
            }
        }
        if c != nz {
            return None;
        }
    } else {
        let mut s = 0usize;
        for k in 0..nz {
            let g = get_varint(b, p)?;
            // (strictly increasing: a gap of 1 or more after the first)
            if (k > 0 && g == 0) || g >= f.len() as u64 {
                return None;
            }
            s += g as usize;
            if s >= f.len() {
                return None;
            }
            sy[k] = s;
        }
    }
    let mut sum = 0u32;
    for &i in &sy[..nz - 1] {
        let v = get_varint(b, p)?;
        if v >= PM as u64 - 1 {
            return None;
        }
        f[i] = v as u32 + 1;
        sum += v as u32 + 1;
        if sum >= PM {
            return None;
        }
    }
    f[sy[nz - 1]] = PM - sum;
    Some(())
}

/// Decoding table: per slot, symbol | frequency << 8 | (slot - start) << 20.
fn dec_table(f: &[u32], t: &mut [u32]) {
    let mut c = 0usize;
    for (s, &x) in f.iter().enumerate() {
        for k in 0..x as usize {
            t[c + k] = s as u32 | x << 8 | (k as u32) << 20;
        }
        c += x as usize;
    }
}

/// Encoding table: per symbol, (frequency, start).
fn enc_table(f: &[u32]) -> Vec<(u32, u32)> {
    let mut c = 0u32;
    f.iter()
        .map(|&x| {
            let e = (x, c);
            c += x;
            e
        })
        .collect()
}

// ---------------------------------------------------------------- rANS steps
#[inline(always)]
fn enc_put(x: &mut u32, words: &mut Vec<u16>, (f, c): (u32, u32)) {
    if *x >= f << 20 {
        words.push(*x as u16);
        *x >>= 16;
    }
    *x = ((*x / f) << PB) + (*x % f) + c;
}

/// enc_put without a branch: the word is always stored at *w and the place moves on only
/// when it is written out (the same words and states). SAFETY: *w has room for one word.
#[inline(always)]
unsafe fn enc_put_bl(x: &mut u32, w: &mut *mut u16, (f, c): (u32, u32)) {
    let big = *x >= f << 20;
    unsafe {
        **w = *x as u16;
        *w = w.add(big as usize);
    }
    *x = if big { *x >> 16 } else { *x };
    *x = ((*x / f) << PB) + (*x % f) + c;
}

/// An encoding table entry with the division done by a multiply (the same states and words
/// as enc_put / enc_put_bl).
#[derive(Clone, Copy, Default)]
struct EncSym {
    /// ceil(2^44 / f)
    rcp: u64,
    /// f << 20: a state at or above it puts out a word first
    xmax: u32,
    /// the symbol's start
    c: u32,
    /// PM - f
    cmpl: u32,
}

impl EncSym {
    fn new((f, c): (u32, u32)) -> EncSym {
        if f == 0 {
            // (never coded)
            return EncSym::default();
        }
        EncSym { rcp: ((1u64 << 44) + f as u64 - 1) / f as u64, xmax: f << 20, c, cmpl: PM - f }
    }
}

/// enc_put_bl with x / f as a multiply: for 1 <= f < 2^12 and y < f << 20 (which holds after
/// the word is put out: y < 2^16 then), m = ceil(2^44 / f) = 2^44 / f + e (0 <= e < 1) gives
/// y m / 2^44 = y / f + y e / 2^44 with y e / 2^44 < f 2^20 / 2^44 <= 1 / f (as f^2 <= 2^24),
/// which is less than what y / f lacks to the next whole number, so (y m) >> 44 = y / f;
/// and y m < 2^64 - 2^44 / f + y <= 2^64. Then (y / f) << PB + y % f + c = y + c + q (PM - f).
/// SAFETY: *w has room for one word.
#[inline(always)]
unsafe fn enc_put_rcp(x: &mut u32, w: &mut *mut u16, s: &EncSym) {
    let big = *x >= s.xmax;
    unsafe {
        **w = *x as u16;
        *w = w.add(big as usize);
    }
    let y = if big { *x >> 16 } else { *x };
    let q = ((y as u64 * s.rcp) >> 44) as u32;
    *x = y + s.c + q * s.cmpl;
}

/// The words of a finished stream, in the order the decoder reads them: the final states
/// (first state first), then the words in reverse.
fn enc_finish(states: &[u32], mut words: Vec<u16>) -> Vec<u16> {
    for &x in states.iter().rev() {
        words.push(x as u16);
        words.push((x >> 16) as u16);
    }
    words.reverse();
    words
}

fn put_words(out: &mut Vec<u8>, w: &[u16]) {
    put_varint(out, w.len() as u64);
    #[cfg(target_endian = "little")]
    // SAFETY: the words' bytes in memory are their little-endian bytes, in order
    out.extend_from_slice(unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2) });
    #[cfg(not(target_endian = "little"))]
    for &x in w {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

/// A word stream being read.
struct Words<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Words<'a> {
    fn new(b: &'a [u8], p: &mut usize) -> Option<Words<'a>> {
        let n = get_varint(b, p)? as usize;
        let w = b.get(*p..*p + 2 * n)?;
        *p += 2 * n;
        Some(Words { b: w, p: 0 })
    }
    #[inline(always)]
    fn next(&mut self) -> u32 {
        // zeros past the end (a damaged stream decodes to garbage, never out of bounds)
        let v = match self.b.get(self.p..self.p + 2) {
            Some(x) => u16::from_le_bytes([x[0], x[1]]) as u32,
            None => 0,
        };
        self.p += 2;
        v
    }
    /// The next word without moving past it (0 past the end, as next).
    #[inline(always)]
    fn peek(&self) -> u32 {
        match self.b.get(self.p..self.p + 2) {
            Some(x) => u16::from_le_bytes([x[0], x[1]]) as u32,
            None => 0,
        }
    }
    /// The next 4 words as one number, the first in the low 16 bits (each 0 past the end, as
    /// peek).
    #[inline(always)]
    fn peek4(&self) -> u64 {
        match self.b.get(self.p..self.p + 8) {
            Some(x) => u64::from_le_bytes([x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]]),
            None => self.peek4_end(),
        }
    }
    /// peek4 near the end: word by word.
    #[cold]
    #[inline(never)]
    fn peek4_end(&self) -> u64 {
        let mut v = 0u64;
        for k in 0..4 {
            let q = self.p + 2 * k;
            if let Some(x) = self.b.get(q..q + 2) {
                v |= (u16::from_le_bytes([x[0], x[1]]) as u64) << (16 * k);
            }
        }
        v
    }
    #[inline(always)]
    fn state(&mut self) -> u32 {
        let hi = self.next();
        hi << 16 | self.next()
    }
}

#[inline(always)]
fn dec_step(x: &mut u32, e: u32, w: &mut Words) {
    // the same as `if x < RL { x = x << 16 | w.next() }`, without a branch on the data: the
    // word is read either way (peek) and taken, with its 2 bytes, only when x < RL
    let y = (e >> 8 & 0xFFF) * (*x >> PB) + (e >> 20);
    let need = y < RL;
    let v = w.peek();
    *x = if need { y << 16 | v } else { y };
    w.p += 2 * need as usize;
}

// ---------------------------------------------------------------- literals
/// Literal counts per context: the top bit of the literal before in the same run (the runs:
/// lits.chunks(q), q = n / lanes_of(n) rounded up), counted in 4 partial tables, so that a
/// repeated byte does not wait for its own count (the same sums).
fn lit_counts(lits: &[u8]) -> [[u32; 256]; 2] {
    let mut c2 = [[0u32; 256]; 2];
    let n = lits.len();
    if n == 0 {
        return c2;
    }
    let q = n.div_ceil(lanes_of(n));
    let mut c4 = [[0u32; 512]; 4];
    for r in lits.chunks(q) {
        let mut ctx = 0usize;
        let mut g = r.chunks_exact(4);
        for x in &mut g {
            c4[0][ctx | x[0] as usize] += 1;
            c4[1][((x[0] >> 7) as usize) << 8 | x[1] as usize] += 1;
            c4[2][((x[1] >> 7) as usize) << 8 | x[2] as usize] += 1;
            c4[3][((x[2] >> 7) as usize) << 8 | x[3] as usize] += 1;
            ctx = ((x[3] >> 7) as usize) << 8;
        }
        for &x in g.remainder() {
            c4[0][ctx | x as usize] += 1;
            ctx = ((x >> 7) as usize) << 8;
        }
    }
    for c in &c4 {
        for i in 0..512 {
            c2[i >> 8][i & 255] += c[i];
        }
    }
    c2
}

/// A table's bytes as write_table stores it, in bits.
#[inline(never)]
fn tbl_bits(f: &[u32]) -> f64 {
    let mut v = Vec::new();
    write_table(&mut v, f);
    8.0 * v.len() as f64
}

/// The literal tables (counts: cs[0] all, cs[1] and cs[2] by context; n literals): one table
/// for both contexts or one each, each its own (written) or the bank's for its slot (r),
/// whichever costs fewest bits: (mode: 1 one table, 2 two, 4 stored; bits; per table its
/// slot and header (choose; 2 also for no table); the own tables (slots 0, 1, 2); each slot's
/// bits, data and table).
fn lit_choice(n: usize, cs: &[[u32; 256]; SLOTS], r: Refs) -> (u8, f64, [(usize, u8); 2], [[u32; 256]; 3], [f64; 3]) {
    let mut f = [[0u32; 256]; 3];
    normalize(&cs[0], &mut f[0]);
    normalize(&cs[1], &mut f[1]);
    normalize(&cs[2], &mut f[2]);
    let (d1, t1, h1) = choose(&cs[0], &f[0], r, 0);
    let (da, ta, ha) = choose(&cs[1], &f[1], r, 1);
    let (db, tb, hb) = choose(&cs[2], &f[2], r, 2);
    let b1 = d1 + t1;
    let b2 = da + db + ta + tb;
    let st = 8.0 * n as f64;
    let c = [b1, da + ta, db + tb];
    if b1.min(b2) >= st {
        (4, st, [(0, 2); 2], f, c)
    } else if b2 < b1 {
        (2, b2, [(1, ha), (2, hb)], f, c)
    } else {
        (1, b1, [(0, h1), (0, 2)], f, c)
    }
}

/// Code the literals with tables chosen by lit_choice (mode, its tables hs, the own ones f):
/// mode 1 one table, 2 two tables (top bit of the literal before), 4 stored (fin writes
/// modes 0, none, and 3, one byte repeated). Adds the bank entries named to `used`.
fn put_literals(out: &mut Vec<u8>, lits: &[u8], (mode, hs, f): (u8, &[(usize, u8); 2], &[[u32; 256]; 3]), r: Refs, used: &mut u128) {
    let n = lits.len();
    out.push(mode);
    if mode == 4 {
        out.extend_from_slice(lits);
        return;
    }
    let nl = lanes_of(n);
    let q = n.div_ceil(nl);
    // (frequency, start) per context << 8 | literal, as EncSym
    let mut tab = [EncSym::default(); 512];
    for (i, &(s, h)) in hs[..mode as usize].iter().enumerate() {
        let t = put_ref(out, &f[s], r, s, h, used);
        for (e, &x) in tab[256 * i..256 * i + 256].iter_mut().zip(enc_table(t).iter()) {
            *e = EncSym::new(x);
        }
    }
    if mode == 1 {
        tab.copy_within(..256, 256);
    }
    match nl {
        1 => put_runs::<1>(out, lits, q, &tab),
        2 => put_runs::<2>(out, lits, q, &tab),
        4 => put_runs::<4>(out, lits, q, &tab),
        _ => put_runs::<8>(out, lits, q, &tab),
    }
}

/// The L runs (as lits.chunks(q), empty ones at the end), each coded from its end; coded side
/// by side (each run's steps in the same order as one run at a time: the same words).
fn put_runs<const L: usize>(out: &mut Vec<u8>, lits: &[u8], q: usize, tab: &[EncSym; 512]) {
    let n = lits.len();
    let runs: [&[u8]; L] = std::array::from_fn(|j| &lits[(j * q).min(n)..((j + 1) * q).min(n)]);
    let lens: [usize; L] = std::array::from_fn(|j| runs[j].len());
    let m = *lens.iter().min().unwrap();
    let mut xs = [RL; L];
    // (room for a word per literal and one more: the branch-free step always stores one)
    let mut ws: [Vec<u16>; L] = std::array::from_fn(|j| Vec::with_capacity(lens[j] + 4));
    let mut wp: [*mut u16; L] = std::array::from_fn(|j| ws[j].as_mut_ptr());
    #[inline(always)]
    fn sym(r: &[u8], i: usize) -> usize {
        (if i == 0 { 0 } else { ((r[i - 1] >> 7) as usize) << 8 }) | r[i] as usize
    }
    // where each run starts in lits
    let at: [usize; L] = std::array::from_fn(|j| (j * q).min(n));
    let lp = lits.as_ptr();
    // SAFETY: run j stores at most lens[j] + 1 words (one per literal, the last one unused)
    unsafe {
        // side by side while every run's literal i has one before it (i >= 1: the steps
        // k < m - 1 have i = lens[j] - 1 - k >= lens[j] - m + 1 >= 1); the literal and the one
        // before as one 16-bit read of lits[at + i - 1..=at + i], inside lits (at + i <
        // at + lens[j] <= n); the symbol (top bit of the one before) << 8 | literal is < 512
        let mm = m.saturating_sub(1);
        for k in 0..mm {
            for j in 0..L {
                let i = lens[j] - 1 - k;
                let x = u16::from_le((lp.add(at[j] + i - 1) as *const u16).read_unaligned()) as usize;
                enc_put_rcp(&mut xs[j], &mut wp[j], tab.get_unchecked((x & 0x80) << 1 | x >> 8));
            }
        }
        // the rest of each run, in the same order (each run's steps as one run at a time)
        for j in 0..L {
            for k in mm..lens[j] {
                let i = lens[j] - 1 - k;
                enc_put_rcp(&mut xs[j], &mut wp[j], &tab[sym(runs[j], i)]);
            }
        }
        for j in 0..L {
            ws[j].set_len(wp[j].offset_from(ws[j].as_ptr()) as usize);
        }
    }
    for j in 0..L {
        put_words(out, &enc_finish(&[xs[j]], std::mem::take(&mut ws[j])));
    }
}

/// Decode n literals into lits (with 16 spare bytes).
fn get_literals(b: &[u8], p: &mut usize, n: usize, lits: &mut Vec<u8>, r: Refs) -> Option<()> {
    lits.clear();
    lits.resize(n + 16, 0);
    let mode = *b.get(*p)?;
    *p += 1;
    match mode {
        0 => return if n == 0 { Some(()) } else { None },
        3 => {
            let c = *b.get(*p)?;
            *p += 1;
            lits[..n].fill(c);
            return Some(());
        }
        4 => {
            lits[..n].copy_from_slice(b.get(*p..*p + n)?);
            *p += n;
            return Some(());
        }
        1 | 2 => {}
        _ => return None,
    }
    let mut f = [0u32; 256];
    let mut t = vec![0u32; 2 * PM as usize];
    read_table_r(b, p, &mut f, r, mode as usize - 1)?;
    dec_table(&f, &mut t[..PM as usize]);
    if mode == 2 {
        read_table_r(b, p, &mut f, r, 2)?;
        dec_table(&f, &mut t[PM as usize..]);
    } else {
        t.copy_within(0..PM as usize, PM as usize);
    }
    let t: &[u32; 2 * PM as usize] = t[..].try_into().ok()?;
    match lanes_of(n) {
        1 => get_runs::<1>(t, b, p, n, lits),
        2 => get_runs::<2>(t, b, p, n, lits),
        4 => get_runs::<4>(t, b, p, n, lits),
        _ => get_runs::<8>(t, b, p, n, lits),
    }
}

/// Decode the L runs of put_runs (n literals) into lits[..n] (lits has n + 16 bytes).
fn get_runs<const L: usize>(t: &[u32; 2 * PM as usize], b: &[u8], p: &mut usize, n: usize, lits: &mut [u8]) -> Option<()> {
    let q = n.div_ceil(L);
    let mut ws: [Words; L] = {
        let mut v: Vec<Words> = Vec::with_capacity(L);
        for _ in 0..L {
            v.push(Words::new(b, p)?);
        }
        v.try_into().ok()?
    };
    let lens: [usize; L] = std::array::from_fn(|j| n.saturating_sub(j * q).min(q));
    let at: [usize; L] = std::array::from_fn(|j| (j * q).min(n));
    let mut x: [u32; L] = std::array::from_fn(|j| ws[j].state());
    let mut ctx = [0usize; L];
    let m = lens[L - 1];
    assert!(lits.len() >= n);
    let o = lits.as_mut_ptr();
    for i in 0..m {
        for j in 0..L {
            // (ctx < 2 and x & (PM - 1) < PM: inside t)
            let e = t[(ctx[j] << PB | (x[j] & (PM - 1)) as usize) & (2 * PM as usize - 1)];
            // SAFETY: at[j] + i < at[j] + lens[j] <= n <= lits.len() (i < m = the shortest run)
            unsafe { *o.add(at[j] + i) = e as u8 };
            ctx[j] = (e >> 7 & 1) as usize;
            dec_step(&mut x[j], e, &mut ws[j]);
        }
    }
    // the first runs may be longer
    for j in 0..L {
        for i in m..lens[j] {
            let e = t[(ctx[j] << PB | (x[j] & (PM - 1)) as usize) & (2 * PM as usize - 1)];
            lits[at[j] + i] = e as u8;
            ctx[j] = (e >> 7 & 1) as usize;
            dec_step(&mut x[j], e, &mut ws[j]);
        }
    }
    Some(())
}

// ---------------------------------------------------------------- copies
/// v -> (code, extra bit count, extra bits): 0..15 are their own code; larger values: code
/// from the bit length and the next bit, the rest as extra bits.
#[inline(always)]
fn bucket(v: u32) -> (u32, u32, u32) {
    if v < 16 {
        return (v, 0, 0);
    }
    let b = 31 - v.leading_zeros();
    (16 + (b - 4) * 2 + ((v >> (b - 1)) & 1), b - 1, v & ((1 << (b - 1)) - 1))
}

/// Base value and extra bit count of each code.
const fn code_bases() -> [(u32, u32); 80] {
    let mut t = [(0u32, 0u32); 80];
    let mut c = 0;
    while c < 80 {
        t[c] = if c < 16 {
            (c as u32, 0)
        } else {
            let k = c as u32 - 16;
            let b = k / 2 + 4;
            if b > 31 { (0, 0) } else { ((1u32 << b) | ((k & 1) << (b - 1)), b - 1) }
        };
        c += 1;
    }
    t
}
static BASES: [(u32, u32); 80] = code_bases();

/// Alphabet sizes: literal counts and copy lengths 72 codes, distances 3 repeats + 72 codes.
const NLL: usize = 72;
const NOF: usize = 75;

/// bucket() without a branch.
#[inline(always)]
fn bucket_bl(v: u32) -> (u32, u32, u32) {
    // (v | 16 has v's bit length when v >= 16, and 5 bits else: no shift by a negative amount)
    let b = 31 - (v | 16).leading_zeros();
    let big = v >= 16;
    let code = 16 + (b - 4) * 2 + ((v >> (b - 1)) & 1);
    let x = v & ((1 << (b - 1)) - 1);
    if big { (code, b - 1, x) } else { (v, 0, 0) }
}

struct BitR<'a> {
    b: &'a [u8],
    p: usize,
    acc: u64,
    n: u32,
}

impl BitR<'_> {
    #[inline(always)]
    fn get(&mut self, bits: u32) -> u32 {
        if self.n < 32 {
            let v = match self.b.get(self.p..self.p + 4) {
                Some(x) => u32::from_le_bytes([x[0], x[1], x[2], x[3]]),
                None => {
                    let mut t = [0u8; 4];
                    for (k, y) in t.iter_mut().enumerate() {
                        *y = self.b.get(self.p + k).copied().unwrap_or(0);
                    }
                    u32::from_le_bytes(t)
                }
            };
            self.acc |= (v as u64) << self.n;
            self.p += 4;
            self.n += 32;
        }
        let v = (self.acc & ((1u64 << bits) - 1)) as u32;
        self.acc >>= bits;
        self.n -= bits;
        v
    }
}

/// Pack data with fz (`level`: parse5's level, 20-25).
pub fn compress(data: &[u8], level: u32) -> Vec<u8> {
    let seqs = parse5_level(data, level);
    encode(data, &seqs)
}

fn encode(data: &[u8], seqs: &[Seq]) -> Vec<u8> {
    fin(&prep(data, seqs), None, &[]).0
}

/// The frame of a parsed input with its own tables (as compress_fast), their costs kept for
/// make_bank.
pub fn code(p: &mut Prep) -> Vec<u8> {
    let (f, e, own) = fin(p, None, &[]);
    (p.own, p.lit_end) = (own, e as usize);
    f
}

/// An input parsed for fz, before its frame is written (fin): its literals and their counts,
/// its copies' codes and their counts, and the copies' extra bits.
#[derive(Clone)]
pub struct Prep {
    n: usize,
    lits: Vec<u8>,
    /// the counts of each slot's symbols (SLOTS)
    cs: [[u32; 256]; SLOTS],
    codes: Vec<[u8; 3]>,
    eb: Vec<u8>,
    /// each slot's bits with its own table, and where the literal part ends in its frame
    /// (code; for make_bank and bank_frames)
    own: [f64; SLOTS],
    lit_end: usize,
}

/// A parsed input's literals, codes, counts and extra bits.
fn prep(data: &[u8], seqs: &[Seq]) -> Prep {
    let n = data.len();
    // (the literals: n minus the copies' bytes; room for 16-byte steps past the end)
    let nl = n - seqs.iter().map(|s| s.ml as usize).sum::<usize>();
    let mut lits: Vec<u8> = Vec::with_capacity(nl + 16);
    let mut codes: Vec<[u8; 3]> = Vec::with_capacity(seqs.len());
    // the extra bits: a 64-bit bit buffer whose low 4 bytes are always stored at pos, pos moving
    // on 4 when 32 bits are full (the bytes the old bit writer made, now with its state in
    // registers): a copy's three puts of at most 31 bits move pos on by at most 12, and each
    // store writes 4 bytes from pos
    let mut eb: Vec<u8> = Vec::with_capacity(12 * seqs.len() + 8);
    let (mut acc, mut nb, mut pos) = (0u64, 0u32, 0usize);
    let ep = eb.as_mut_ptr();
    let mut cs = [[0u32; 256]; SLOTS];
    let mut p = 0usize;
    let lp = lits.as_mut_ptr();
    let cp = codes.as_mut_ptr();
    let mut o = 0usize;
    for (q, s) in seqs.iter().enumerate() {
        let ll = s.ll as usize;
        assert!(p + ll <= n && o + ll <= nl);
        // SAFETY: the source has 16 bytes from p + k (k < ll, and k = 0 also when ll = 0)
        // when p + ll + 16 <= n, and lits has room for o + ll + 16 bytes; else an exact copy.
        // (Bytes written past o + ll are written again by the next copies or the tail, or lie
        // past nl.)
        unsafe {
            if p + ll + 16 <= n {
                std::ptr::copy_nonoverlapping(data.as_ptr().add(p), lp.add(o), 16);
                let mut k = 16;
                while k < ll {
                    std::ptr::copy_nonoverlapping(data.as_ptr().add(p + k), lp.add(o + k), 16);
                    k += 16;
                }
            } else {
                std::ptr::copy_nonoverlapping(data.as_ptr().add(p), lp.add(o), ll);
            }
        }
        o += ll;
        p += (s.ll + s.ml) as usize;
        let (a, n1, x1) = bucket_bl(s.ll);
        let (b, n2, x2) = bucket_bl(s.ml - 2);
        let (c, n3, x3) = bucket_bl(s.dist);
        // (a repeat puts no extra bits: 0 of them)
        let rep = s.rk < 3;
        let (c, n3, x3) = if rep { (s.rk as u32, 0, 0) } else { (3 + c, n3, x3) };
        // SAFETY: pos <= 12 q before copy q's puts (each moves it on by at most 4), so its stores
        // (4 bytes at pos) end at most at 12 q + 12 <= 12 seqs.len() < the capacity; extra bits
        // are below 2^k with k <= 30 (bucket_bl), so acc holds at most 31 + 30 bits
        for (x, k) in [(x1, n1), (x2, n2), (x3, n3)] {
            acc |= (x as u64) << nb;
            nb += k;
            unsafe { (ep.add(pos) as *mut [u8; 4]).write_unaligned((acc as u32).to_le_bytes()) };
            let full = nb >= 32;
            pos += 4 * full as usize;
            acc = if full { acc >> 32 } else { acc };
            nb -= 32 * full as u32;
        }
        cs[3][a as usize] += 1;
        cs[4][b as usize] += 1;
        cs[5][c as usize] += 1;
        // SAFETY: q < seqs.len() = the capacity of codes
        unsafe { cp.add(q).write([a as u8, b as u8, c as u8]) };
    }
    assert!(o + (n - p) == nl);
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr().add(p), lp.add(o), n - p);
        lits.set_len(nl);
        codes.set_len(seqs.len());
        // (the bytes before pos are written)
        eb.set_len(pos);
    }
    eb.extend_from_slice(&acc.to_le_bytes()[..nb.div_ceil(8) as usize]);
    let c2 = lit_counts(&lits);
    for i in 0..256 {
        cs[0][i] = c2[0][i] + c2[1][i];
    }
    (cs[1], cs[2]) = (c2[0], c2[1]);
    Prep { n, lits, cs, codes, eb, own: [0.0; SLOTS], lit_end: 0 }
}

/// A parsed input with fz's own greedy parse (compress_fast's levels).
pub fn prep_fast(data: &[u8], level: u32) -> Prep {
    prep(data, &parse_level(data, level))
}

/// A table at the coder's choice (choose): written (h = 2) or a reference to the bank's table
/// for slot s (h = 0 or 1, its bank entry added to `used`); the table the coder uses.
fn put_ref<'a>(out: &mut Vec<u8>, f: &'a [u32], r: Refs<'a>, s: usize, h: u8, used: &mut u128) -> &'a [u32] {
    if let (Some(t), Some(i)) = (bank_of(r, h, s), bank_key(r, h, s)) {
        out.push(h);
        *used |= 1 << i;
        t
    } else {
        write_table(out, f);
        f
    }
}

/// Whether slot s of a parsed input has a table: symbols to code, and for the literals at
/// least two different ones (else mode 0 or 3).
fn has(p: &Prep, s: usize) -> bool {
    p.cs[s].iter().any(|&x| x > 0) && (s > 2 || p.cs[0].iter().filter(|&&x| x > 0).count() > 1)
}

/// The frame of a parsed input (after the method byte's 6), its tables the bank's (r) where
/// that is cheaper (estimated); the bank entries it names (bit k * SLOTS + slot; with r =
/// None: where its literal part ends); each slot's bits as chosen (with r = None: with its own
/// table). With a bank: no frame (empty) when no table would use it (it would come out as
/// without the bank), and the literal part or the copy part that uses none is taken as it is
/// from the frame without the bank (`base`, as code made it: the same bytes).
pub fn fin(p: &Prep, r: Refs, base: &[u8]) -> (Vec<u8>, u128, [f64; SLOTS]) {
    let n = p.lits.len();
    let mut cost = [0.0; SLOTS];
    // the tables first: the literals' (lit_choice), the copies' (choose)
    let lc = has(p, 0).then(|| lit_choice(n, &p.cs, r));
    let mut cf = [[0u32; NOF]; 3];
    let mut ch = [2u8; 3];
    if !p.codes.is_empty() {
        for j in 0..3 {
            let (c, f) = (&p.cs[3 + j][..ALPHA[3 + j]], &mut cf[j][..ALPHA[3 + j]]);
            normalize(c, f);
            let (d, t, h) = choose(c, f, r, 3 + j);
            (cost[3 + j], ch[j]) = (d + t, h);
        }
    }
    if r.is_some() && !lc.as_ref().is_some_and(|c| c.2.iter().any(|x| x.1 < 2)) && !ch.iter().any(|&h| h < 2) {
        return (Vec::new(), 0, cost);
    }
    // (room for the usual result, so that it is not copied as it grows: literal words take
    // at most about a byte per literal, a copy's three codes and extra bits about 4 bytes)
    let mut out = Vec::with_capacity(n + 4 * p.codes.len() + 2048);
    out.push(6u8);
    put_varint(&mut out, p.n as u64);
    put_varint(&mut out, p.codes.len() as u64);
    put_varint(&mut out, n as u64);
    let mut used = 0u128;
    let (at, cut) = (out.len(), r.is_some() && p.lit_end <= base.len());
    match &lc {
        _ if cut && !lc.as_ref().is_some_and(|c| c.2.iter().any(|x| x.1 < 2)) => out.extend_from_slice(&base[at..p.lit_end]),
        Some((mode, _, hs, f, c)) => {
            cost[..3].copy_from_slice(c);
            put_literals(&mut out, &p.lits, (*mode, hs, f), r, &mut used);
        }
        // (no literals: mode 0; one byte repeated: mode 3)
        None if n == 0 => out.push(0),
        None => out.extend_from_slice(&[3, p.lits[0]]),
    }
    if r.is_none() {
        used = out.len() as u128;
    } else if cut && !ch.iter().any(|&h| h < 2) {
        out.extend_from_slice(&base[p.lit_end..]);
        return (out, used, cost);
    }
    if !p.codes.is_empty() {
        let mut ts: [Vec<EncSym>; 3] = Default::default();
        for j in 0..3 {
            let t = put_ref(&mut out, &cf[j][..ALPHA[3 + j]], r, 3 + j, ch[j], &mut used);
            ts[j] = enc_table(t).into_iter().map(EncSym::new).collect();
        }
        let [tl, tm, to] = &ts;
        let (mut xl, mut xm, mut xo) = (RL, RL, RL);
        // (room for a word per code and one more: see enc_put_bl)
        let mut w: Vec<u16> = Vec::with_capacity(p.codes.len() * 3 + 8);
        let mut q = w.as_mut_ptr();
        // SAFETY: at most 3 words per copy are stored
        unsafe {
            for c in p.codes.iter().rev() {
                enc_put_rcp(&mut xo, &mut q, &to[c[2] as usize]);
                enc_put_rcp(&mut xm, &mut q, &tm[c[1] as usize]);
                enc_put_rcp(&mut xl, &mut q, &tl[c[0] as usize]);
            }
            w.set_len(q.offset_from(w.as_ptr()) as usize);
        }
        put_words(&mut out, &enc_finish(&[xl, xm, xo], w));
        put_varint(&mut out, p.eb.len() as u64);
        out.extend_from_slice(&p.eb);
    }
    (out, used, cost)
}

/// Open fz bytes (after the method byte): the n original bytes.
pub fn decompress(b: &[u8], n: usize) -> Option<Vec<u8>> {
    decompress_r(b, n, None)
}

/// decompress, with the unit's bank for the frame's stream (r).
pub fn decompress_r(b: &[u8], n: usize, r: Refs) -> Option<Vec<u8>> {
    let mut p = 0usize;
    let nseq = get_varint(b, &mut p)? as usize;
    let nlit = get_varint(b, &mut p)? as usize;
    if nlit > n || nseq > n {
        return None;
    }
    let mut lits = Vec::new();
    get_literals(b, &mut p, nlit, &mut lits, r)?;
    let mut out: Vec<u8> = vec![0; n + 32];
    if nseq == 0 {
        if nlit != n {
            return None;
        }
        out.truncate(n);
        out.copy_from_slice(&lits[..n]);
        return Some(out);
    }
    let (mut fl, mut fm, mut fo) = ([0u32; NLL], [0u32; NLL], [0u32; NOF]);
    read_table_r(b, &mut p, &mut fl, r, 3)?;
    read_table_r(b, &mut p, &mut fm, r, 4)?;
    read_table_r(b, &mut p, &mut fo, r, 5)?;
    let mut t = vec![0u32; 3 * PM as usize];
    dec_table(&fl, &mut t[..PM as usize]);
    dec_table(&fm, &mut t[PM as usize..2 * PM as usize]);
    dec_table(&fo, &mut t[2 * PM as usize..]);
    let (tl, rest) = t.split_at(PM as usize);
    let (tm, to) = rest.split_at(PM as usize);
    let mut w = Words::new(b, &mut p)?;
    let (mut xl, mut xm, mut xo) = (w.state(), w.state(), w.state());
    let ne = get_varint(b, &mut p)? as usize;
    let mut br = BitR { b: b.get(p..p + ne)?, p: 0, acc: 0, n: 0 };
    let mut reps = [1usize, 4, 8];
    let (mut op, mut lp) = (0usize, 0usize);
    for _ in 0..nseq {
        let el = tl[(xl & (PM - 1)) as usize];
        let em = tm[(xm & (PM - 1)) as usize];
        let eo = to[(xo & (PM - 1)) as usize];
        // dec_step for the three states in turn, its word reads made from one read of 4
        // words: a state that needs a word takes the next one (the same words, in the same
        // order, as three dec_steps)
        let yl = (el >> 8 & 0xFFF) * (xl >> PB) + (el >> 20);
        let ym = (em >> 8 & 0xFFF) * (xm >> PB) + (em >> 20);
        let yo = (eo >> 8 & 0xFFF) * (xo >> PB) + (eo >> 20);
        let (nl, nm, no) = ((yl < RL) as u32, (ym < RL) as u32, (yo < RL) as u32);
        let q = w.peek4();
        let qm = q >> (16 * nl);
        let qo = qm >> (16 * nm);
        xl = if nl != 0 { yl << 16 | (q as u32 & 0xFFFF) } else { yl };
        xm = if nm != 0 { ym << 16 | (qm as u32 & 0xFFFF) } else { ym };
        xo = if no != 0 { yo << 16 | (qo as u32 & 0xFFFF) } else { yo };
        w.p += 2 * (nl + nm + no) as usize;
        let (bl, nl) = BASES[(el & 0xFF) as usize];
        let ll = (bl + br.get(nl)) as usize;
        let (bm, nm) = BASES[(em & 0xFF) as usize];
        let ml = (bm + br.get(nm)) as usize + 2;
        let oc = (eo & 0xFF) as usize;
        let dist = if oc < 3 {
            let d = reps[oc];
            reps = next_reps(reps, oc as u8, d);
            d
        } else {
            let (bo, no) = BASES[oc - 3];
            let d = (bo + br.get(no)) as usize;
            reps = [d, reps[0], reps[1]];
            d
        };
        // the literals, 16 bytes at a time (both buffers have room)
        if lp + ll > nlit || op + ll + ml > n {
            return None;
        }
        let mut k = 0;
        while k < ll {
            out[op + k..op + k + 16].copy_from_slice(&lits[lp + k..lp + k + 16]);
            k += 16;
        }
        op += ll;
        lp += ll;
        if dist > op || dist == 0 {
            return None;
        }
        let src = op - dist;
        if dist >= 16 {
            let mut k = 0;
            while k < ml {
                out.copy_within(src + k..src + k + 16, op + k);
                k += 16;
            }
        } else if dist >= 8 {
            let mut k = 0;
            while k < ml {
                out.copy_within(src + k..src + k + 8, op + k);
                k += 8;
            }
        } else if ml <= 8 {
            for k in 0..ml {
                out[op + k] = out[src + k];
            }
        } else {
            // a run of period dist (1-7): out[op + k] = out[src + k % dist] for every k, so the
            // pattern's 16 bytes go at every multiple of `step` (a multiple of dist); the last
            // store ends before op + ml + 16 <= n + 16 (out has n + 32 bytes), and what it puts
            // past op + ml is written again by the next sequences (or cut off at n)
            let mut pat = [0u8; 16];
            for (j, x) in pat.iter_mut().enumerate() {
                *x = out[src + j % dist];
            }
            let step = 16 - 16 % dist;
            let mut k = 0;
            while k < ml {
                out[op + k..op + k + 16].copy_from_slice(&pat);
                k += step;
            }
        }
        op += ml;
    }
    // the literals after the last copy
    let rest = nlit - lp;
    if op + rest != n {
        return None;
    }
    out[op..n].copy_from_slice(&lits[lp..nlit]);
    out.truncate(n);
    Some(out)
}

/// Developer statistics (FZ_STATS=1): estimated bits of each part, summed over calls.
pub static ST: [std::sync::atomic::AtomicU64; 40] = [const { std::sync::atomic::AtomicU64::new(0) }; 40];
fn ent(c: &[u32]) -> f64 {
    let n: u64 = c.iter().map(|&x| x as u64).sum();
    c.iter().filter(|&&x| x > 0).map(|&x| x as f64 * (n as f64 / x as f64).log2()).sum()
}
fn stats(data: &[u8], seqs: &[Seq], lits: &[u8], cl: &[u32], cm: &[u32], co: &[u32], extra: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut c1 = [0u32; 256];
    let mut c2 = [[0u32; 256]; 2];
    let mut cb = [[0u32; 256]; 2];
    let mut co1 = vec![[0u32; 256]; 256];
    let mut prev = 0u8;
    for &x in lits {
        c1[x as usize] += 1;
        c2[(prev >> 7) as usize][x as usize] += 1;
        co1[prev as usize][x as usize] += 1;
        prev = x;
    }
    // context: the byte before in the data
    let mut p = 0usize;
    for s in seqs {
        for q in p..p + s.ll as usize {
            let pb = if q > 0 { data[q - 1] } else { 0 };
            cb[(pb >> 7) as usize][data[q] as usize] += 1;
        }
        p += (s.ll + s.ml) as usize;
    }
    for q in p..data.len() {
        let pb = if q > 0 { data[q - 1] } else { 0 };
        cb[(pb >> 7) as usize][data[q] as usize] += 1;
    }
    // other literal contexts: classes of the literal before
    let mut cx = vec![[[0u32; 256]; 16]; 6];
    let mut prev = 0u8;
    for &x in lits {
        let cls = [(prev >> 6) as usize, (prev >> 5) as usize, (prev >> 4) as usize, if prev == 0 { 0 } else if prev < 16 { 1 } else if prev < 128 { 2 } else { 3 }, if prev < 128 { (prev.min(15) >> 2) as usize } else { 4 + (prev >> 5) as usize }, (prev as usize * 7) >> 7];
        for k in 0..6 {
            cx[k][cls[k]][x as usize] += 1;
        }
        prev = x;
    }
    for k in 0..6 {
        let used = cx[k].iter().filter(|c| c.iter().any(|&y| y > 0)).count();
        ST[12 + k].fetch_add((cx[k].iter().map(|c| ent(c)).sum::<f64>() + used as f64 * 8.0 * 200.0) as u64, Relaxed);
    }
    // order-1 with the K most common literals before as their own contexts (the rest share
    // one), tables at 1.5 bytes per symbol present
    {
        let mut pc = [0u32; 256];
        for &x in lits { pc[x as usize] += 1; }
        let mut ord: Vec<usize> = (0..256).collect();
        ord.sort_by_key(|&i| std::cmp::Reverse(pc[i]));
        for (slot, kk) in [4usize, 8, 16, 32].iter().enumerate() {
            let mut cls = [*kk; 256];
            for (r, &i) in ord[..*kk].iter().enumerate() { cls[i] = r; }
            let mut cc = vec![[0u32; 256]; kk + 1];
            let mut prev = 0u8;
            for &x in lits { cc[cls[prev as usize]][x as usize] += 1; prev = x; }
            let tb: f64 = cc.iter().map(|c| 12.0 * c.iter().filter(|&&y| y > 0).count() as f64).sum();
            ST[24 + slot].fetch_add((cc.iter().map(|c| ent(c)).sum::<f64>() + tb) as u64, Relaxed);
        }
    }
    // 2 contexts, tables per segment of S literals (+ table cost: 2.5 bytes per symbol)
    for (slot, seg) in [16384usize, 65536, 262144].iter().enumerate() {
        let mut tot = 0f64;
        for ch in lits.chunks(*seg) {
            let mut c2 = [[0u32; 256]; 2];
            let mut prev = 0u8;
            for &x in ch { c2[(prev >> 7) as usize][x as usize] += 1; prev = x; }
            tot += ent(&c2[0]) + ent(&c2[1]) + 20.0 * (c2[0].iter().filter(|&&y| y > 0).count() + c2[1].iter().filter(|&&y| y > 0).count()) as f64;
        }
        ST[28 + slot].fetch_add(tot as u64, Relaxed);
    }
    // extra bits by kind: ll, ml, distance
    let (mut el, mut em, mut ed) = (0u64, 0u64, 0u64);
    for s in seqs {
        el += bucket(s.ll).1 as u64;
        em += bucket(s.ml - 2).1 as u64;
        if s.rk == 3 {
            ed += bucket(s.dist).1 as u64;
        }
    }
    ST[18].fetch_add(el, Relaxed);
    ST[19].fetch_add(em, Relaxed);
    ST[20].fetch_add(ed, Relaxed);
    let v = [
        lits.len() as f64 * 8.0,
        ent(&c1),
        ent(&c2[0]) + ent(&c2[1]),
        ent(&cb[0]) + ent(&cb[1]),
        co1.iter().map(|c| ent(c)).sum::<f64>(),
        ent(cl),
        ent(cm),
        ent(co),
        extra as f64 * 8.0,
        seqs.len() as f64 * 8.0,
    ];
    for (i, x) in v.iter().enumerate() {
        ST[i].fetch_add(*x as u64, Relaxed);
    }
}
pub fn print_stats() {
    use std::sync::atomic::Ordering::Relaxed;
    let g = |i: usize| ST[i].load(Relaxed) as f64 / 8.0;
    eprintln!("FZ_STATS bytes: literals {:.0} raw | H0 {:.0} | H(top bit of prev literal) {:.0} | H(top bit of prev data byte) {:.0} | H(prev literal byte) {:.0} | LL {:.0} ML {:.0} OF {:.0} extra {:.0} | copies {:.0}",
        g(0) / 8.0 * 8.0, g(1), g(2), g(3), g(4), g(5), g(6), g(7), g(8), g(9) / 8.0 * 8.0 / 8.0);
    eprintln!("FZ_STATS literal contexts (+200 B table each): prev>>6 {:.0} | prev>>5 {:.0} | prev>>4 {:.0} | 0/<16/<128/rest {:.0} | 8 classes {:.0} | 7 classes {:.0} || extra bits: ll {:.0} B ml {:.0} B dist {:.0} B",
        g(12), g(13), g(14), g(15), g(16), g(17), g(18), g(19), g(20));
 eprintln!("FZ_STATS literal tables per segment of 16K {:.0} 64K {:.0} 256K {:.0} literals", g(28), g(29), g(30));
 eprintln!("FZ_STATS order-1 top-K contexts (with tables): K=4 {:.0} K=8 {:.0} K=16 {:.0} K=32 {:.0}", g(24), g(25), g(26), g(27));
    eprintln!("FZ_STATS coded literal bytes {} for {} literals; {} copies", ST[21].load(std::sync::atomic::Ordering::Relaxed), ST[22].load(std::sync::atomic::Ordering::Relaxed), ST[23].load(std::sync::atomic::Ordering::Relaxed));
}

// ---------------------------------------------------------------- the fast parse
// (the parse's reads are all inside d: i < n - 8 for 8-byte reads at i, earlier positions for
// the others, and mlen reads below a + max = n; unchecked reads, the same values)
#[inline(always)]
fn rd32(d: &[u8], i: usize) -> u32 {
    debug_assert!(i + 4 <= d.len());
    u32::from_le(unsafe { (d.as_ptr().add(i) as *const u32).read_unaligned() })
}
#[inline(always)]
fn rd64(d: &[u8], i: usize) -> u64 {
    debug_assert!(i + 8 <= d.len());
    u64::from_le(unsafe { (d.as_ptr().add(i) as *const u64).read_unaligned() })
}
#[inline(always)]
fn mlen(d: &[u8], a: usize, b: usize, max: usize) -> usize {
    debug_assert!(a + max <= d.len() && b < a);
    let mut n = 0;
    while n + 8 <= max {
        let x = rd64(d, a + n) ^ rd64(d, b + n);
        if x != 0 {
            return n + (x.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    while n < max && unsafe { *d.get_unchecked(a + n) == *d.get_unchecked(b + n) } {
        n += 1;
    }
    n
}

/// The fast parse's hash table, kept per thread from one call to the next, so that it is
/// neither allocated nor cleared per stream (a fresh 2 MB table per stream cost a zeroing
/// pass, or page faults, of up to 32 bytes per input byte). An entry holds position + 1 +
/// `base`; every entry <= `base` (left by an earlier call, or never written) is empty, just
/// as a 0 was in a fresh table. Each call takes the base it finds and leaves base + n behind
/// (before it starts, so that nothing it writes can ever look present to a later call).
struct FastTab {
    t: Vec<u32>,
    base: u32,
}
thread_local! {
    static FAST_TAB: std::cell::RefCell<FastTab> = const { std::cell::RefCell::new(FastTab { t: Vec::new(), base: 0 }) };
}

/// The fast parse's literal scan for W = 4 on little-endian aarch64 (its loads are rd64 and
/// rd32), written out so that all its values stay in registers (the compiler keeps several of
/// them in memory when the copy search shares the loop): the same steps as the loop in
/// parse_fast_on. From place i while i < lim: put i into
/// its slot, then if a repeat (r0, r1) or an entry of the slot as it was (written by this
/// call: > base) starts with the 4 bytes at i, stop there; else step on by
/// 1 + (i - anchor) >> skip. Returns (i, the 8 bytes at i, the slot as it was); i >= lim when
/// nothing was found.
/// SAFETY: d holds lim + 8 bytes; tab is the slot table of 1 << (64 - sh) slots of 4 entries;
/// every entry > base is a place in d plus b1 = base + 1; r0, r1 >= 1; anchor <= i.
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn scan4(d: *const u8, i: usize, lim: usize, tab: *mut u32, sh: u32, b1: usize, base: u32, r0: usize, r1: usize, anchor: usize, skip: u32) -> (usize, u64, [u32; 4]) {
    let (mut i, v): (usize, u64) = (i, 0);
    let mut v = v;
    let (s0, s1, s2, s3): (u32, u32, u32, u32);
    // (v << 24) * K = v * (K << 24) (mod 2^64)
    let k: u64 = 0x9E37_79B1_85EB_CA87u64.wrapping_shl(24);
    unsafe {
        std::arch::asm!(
            "2:",
            "cmp {i}, {lim}",
            "b.hs 9f",
            "ldr {v}, [{d}, {i}]",
            "mul {t0}, {v}, {k}",
            "lsr {t0}, {t0}, {sh}",
            "add {te}, {tab}, {t0}, lsl #4",
            "ldp {s0:w}, {s1:w}, [{te}]",
            "ldp {s2:w}, {s3:w}, [{te}, #8]",
            "add {t0}, {i}, {b1}",
            "stp {t0:w}, {s0:w}, [{te}]",
            "stp {s1:w}, {s2:w}, [{te}, #8]",
            // repeat 0 and 1: i >= r and the 4 bytes at i - r are these
            "subs {t0}, {i}, {r0}",
            "csel {t0}, {t0}, xzr, hs",
            "ldr {t1:w}, [{d}, {t0}]",
            "ccmp {t1:w}, {v:w}, #0, hs",
            "b.eq 9f",
            "subs {t0}, {i}, {r1}",
            "csel {t0}, {t0}, xzr, hs",
            "ldr {t1:w}, [{d}, {t0}]",
            "ccmp {t1:w}, {v:w}, #0, hs",
            "b.eq 9f",
            // the slot's entries: > base and the 4 bytes at entry - b1 are these (an ldp of w
            // registers clears their top halves: {sK:x} is the entry)
            "cmp {s0:w}, {base:w}",
            "sub {t0}, {s0:x}, {b1}",
            "csel {t0}, {t0}, xzr, hi",
            "ldr {t1:w}, [{d}, {t0}]",
            "ccmp {t1:w}, {v:w}, #0, hi",
            "b.eq 9f",
            "cmp {s1:w}, {base:w}",
            "sub {t0}, {s1:x}, {b1}",
            "csel {t0}, {t0}, xzr, hi",
            "ldr {t1:w}, [{d}, {t0}]",
            "ccmp {t1:w}, {v:w}, #0, hi",
            "b.eq 9f",
            "cmp {s2:w}, {base:w}",
            "sub {t0}, {s2:x}, {b1}",
            "csel {t0}, {t0}, xzr, hi",
            "ldr {t1:w}, [{d}, {t0}]",
            "ccmp {t1:w}, {v:w}, #0, hi",
            "b.eq 9f",
            "cmp {s3:w}, {base:w}",
            "sub {t0}, {s3:x}, {b1}",
            "csel {t0}, {t0}, xzr, hi",
            "ldr {t1:w}, [{d}, {t0}]",
            "ccmp {t1:w}, {v:w}, #0, hi",
            "b.eq 9f",
            // a literal: step on
            "sub {t0}, {i}, {anchor}",
            "lsr {t0}, {t0}, {skip}",
            "add {i}, {i}, {t0}",
            "add {i}, {i}, #1",
            "b 2b",
            "9:",
            i = inout(reg) i,
            v = inout(reg) v,
            s0 = out(reg) s0,
            s1 = out(reg) s1,
            s2 = out(reg) s2,
            s3 = out(reg) s3,
            t0 = out(reg) _,
            t1 = out(reg) _,
            te = out(reg) _,
            d = in(reg) d,
            lim = in(reg) lim,
            k = in(reg) k,
            sh = in(reg) sh as u64,
            tab = in(reg) tab,
            b1 = in(reg) b1,
            base = in(reg) base as u64,
            r0 = in(reg) r0,
            r1 = in(reg) r1,
            anchor = in(reg) anchor,
            skip = in(reg) skip as u64,
            options(nostack)
        );
    }
    (i, v, [s0, s1, s2, s3])
}

/// Put place p into its slot for W = 4 on aarch64 (the same steps as slot_scan with key()):
/// the slot of the 8 bytes at p, its entries moved one on and p + b1 put first.
/// SAFETY: p + 8 <= the length of d; tab is the slot table of 1 << (64 - sh) slots of 4.
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline(always)]
unsafe fn insert4(d: *const u8, p: usize, tab: *mut u32, sh: u32, b1: usize) {
    // (v << 24) * K = v * (K << 24) (mod 2^64)
    let k: u64 = 0x9E37_79B1_85EB_CA87u64.wrapping_shl(24);
    unsafe {
        std::arch::asm!(
            "ldr {t0}, [{d}, {p}]",
            "mul {t0}, {t0}, {k}",
            "lsr {t0}, {t0}, {sh}",
            "add {te}, {tab}, {t0}, lsl #4",
            "ldp {s0:w}, {s1:w}, [{te}]",
            "ldr {s2:w}, [{te}, #8]",
            "add {t0}, {p}, {b1}",
            "stp {t0:w}, {s0:w}, [{te}]",
            "stp {s1:w}, {s2:w}, [{te}, #8]",
            t0 = out(reg) _,
            te = out(reg) _,
            s0 = out(reg) _,
            s1 = out(reg) _,
            s2 = out(reg) _,
            d = in(reg) d,
            p = in(reg) p,
            k = in(reg) k,
            sh = in(reg) sh as u64,
            tab = in(reg) tab,
            b1 = in(reg) b1,
            options(nostack, preserves_flags)
        );
    }
}

/// Read slot h and put position entry p at its front (the oldest falls out); returns the slot
/// as it was. (h < tab.len(): key() shifts by 64 - bits)
#[inline(always)]
fn slot_scan<const W: usize>(tab: &mut [[u32; W]], h: usize, p: u32) -> [u32; W] {
    let te = unsafe { tab.get_unchecked_mut(h) };
    let slot = *te;
    let mut e = [0u32; W];
    e[0] = p;
    e[1..].copy_from_slice(&slot[..W - 1]);
    *te = e;
    slot
}

/// A greedy parse for speed (FZ parse levels 0-9): one hash table of 5-byte keys with W
/// positions per slot, the last two distances checked first, literal runs searched more and
/// more sparsely (skip: 1 + run >> SKIP), copies extended backwards, a few positions inside a
/// copy added. lazy: look one byte further before a short new-distance copy.
pub(crate) fn parse_fast<const W: usize>(d: &[u8], skip: u32, lazy: bool) -> Vec<Seq> {
    let n = d.len();
    if n < 16 {
        return Vec::new();
    }
    match FAST_TAB.try_with(|c| c.try_borrow_mut().ok().map(|mut ft| parse_fast_on::<W>(d, skip, lazy, &mut ft))) {
        Ok(Some(s)) => s,
        // (no thread-local table here: a fresh one)
        _ => parse_fast_on::<W>(d, skip, lazy, &mut FastTab { t: Vec::new(), base: 0 }),
    }
}

/// parse_fast on the table ft (one copy of the parse per W, whichever table it gets).
#[inline(never)]
fn parse_fast_on<const W: usize>(d: &[u8], skip: u32, lazy: bool, ft: &mut FastTab) -> Vec<Seq> {
    let n = d.len();
    assert!(n >= 16);
    let bits = (usize::BITS - n.leading_zeros()).clamp(10, 17);
    let need = W << bits;
    if ft.t.len() < need {
        ft.t = vec![0u32; need.max(4 << 17)];
    }
    // every entry is <= ft.base; this call stores values below base + n (its positions are
    // below n), which must fit in u32: else start over from a cleared table and base 0 (where
    // "<= 0" is exactly the old "== 0")
    if ft.base as u64 + n as u64 >= u32::MAX as u64 {
        ft.t.fill(0);
        ft.base = 0;
    }
    let base = ft.base;
    // (a huge n leaves u32::MAX behind, so that the next call clears the table)
    ft.base = (base as u64 + n as u64).min(u32::MAX as u64) as u32;
    let (tab, _) = ft.t[..need].as_chunks_mut::<W>();
    let mut seqs = Vec::with_capacity(n / 16);
    let sh = 64 - bits;
    assert!(tab.len() == 1 << bits);
    // an entry e > base is the position e - base - 1 of this call
    let b1 = base as usize + 1;
    let key = |v: u64| ((v << 24).wrapping_mul(0x9E37_79B1_85EB_CA87) >> sh) as usize;
    let lim = n - 8;
    let mut reps = [1usize, 4, 8];
    let (mut i, mut anchor) = (0usize, 0usize);
    while i < lim {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        let (v, slot) = if W == 4 {
            // (the literal scan up to a place where something starts with these 4 bytes)
            // SAFETY: as scan4 asks: lim = n - 8; tab has 1 << bits slots of 4; entries > base
            // were written by this call as place + b1; reps are >= 1; anchor <= i
            let (j, v, s4) = unsafe { scan4(d.as_ptr(), i, lim, tab.as_mut_ptr() as *mut u32, sh, b1, base, reps[0], reps[1], anchor, skip) };
            i = j;
            if i >= lim {
                break;
            }
            (v, unsafe { std::mem::transmute_copy::<[u32; 4], [u32; W]>(&s4) })
        } else {
            let v = rd64(d, i);
            (v, slot_scan(tab, key(v), (i + b1) as u32))
        };
        #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
        let (v, slot) = {
            let v = rd64(d, i);
            (v, slot_scan(tab, key(v), (i + b1) as u32))
        };
        let cur = v as u32;
        if W != 4 || cfg!(not(all(target_arch = "aarch64", target_endian = "little"))) {
            // nothing to find here (the common case in a literal run): no repeat and no slot
            // entry starts with these 4 bytes, tested without a branch per candidate (an empty
            // entry or a repeat too far back reads d[0..4], a valid place, and its answer is
            // masked off)
            let mut any = false;
            for k in 0..2 {
                let r = reps[k];
                let ok = r <= i;
                any |= ok & (rd32(d, if ok { i - r } else { 0 }) == cur);
            }
            for &j in slot.iter() {
                let ok = j > base;
                any |= ok & (rd32(d, if ok { j as usize - b1 } else { 0 }) == cur);
            }
            if !any {
                i += 1 + ((i - anchor) >> skip);
                continue;
            }
        }
        // the last distance, then the one before
        let mut best = (0usize, 0usize, 3u8); // (len, dist, rk)
        for k in 0..2 {
            let r = reps[k];
            if r <= i && rd32(d, i - r) == cur {
                let len = 4 + mlen(d, i + 4, i + 4 - r, n - i - 4);
                if len > best.0 {
                    best = (len, r, k as u8);
                }
            }
        }
        if best.0 < 8 {
            for &j in slot.iter() {
                if j <= base {
                    break;
                }
                let j = j as usize - b1;
                if rd32(d, j) == cur {
                    let len = 4 + mlen(d, i + 4, j + 4, n - i - 4);
                    // a new distance must be longer than a repeat to win
                    if len > best.0 + (best.2 < 3) as usize {
                        best = (len, i - j, 3);
                    }
                }
            }
        }
        if best.0 == 0 {
            i += 1 + ((i - anchor) >> skip);
            continue;
        }
        let (mut len, mut dist, mut rk) = best;
        if lazy && rk == 3 && len < 12 && i + 1 < lim {
            // one byte further: a longer copy there makes this byte a literal
            let v2 = rd64(d, i + 1);
            let h2 = key(v2);
            let mut b2 = (0usize, 0usize);
            for &j in unsafe { tab.get_unchecked(h2) }.iter() {
                if j <= base {
                    break;
                }
                let j = j as usize - b1;
                if j < i + 1 && rd32(d, j) == v2 as u32 {
                    let l2 = 4 + mlen(d, i + 5, j + 4, n - i - 5);
                    if l2 > b2.0 {
                        b2 = (l2, i + 1 - j);
                    }
                }
            }
            if b2.0 > len + 1 {
                i += 1;
                (len, dist, rk) = (b2.0, b2.1, 3);
            }
        }
        // extend backwards into the literals
        while i > anchor && i > dist && d[i - 1] == d[i - 1 - dist] {
            i -= 1;
            len += 1;
        }
        let rk = if rk < 3 && reps[rk as usize] == dist { rk } else { 3 };
        seqs.push(Seq { ll: (i - anchor) as u32, ml: len as u32, rk, dist: dist as u32 });
        reps = next_reps(reps, rk, dist);
        // a few positions inside the copy
        let e = i + len;
        for p in [i + 1, i + 2, e.saturating_sub(2), e.saturating_sub(1)] {
            #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
            if W == 4 {
                // (a copy is at least 4 long, so i < p < e for these four)
                debug_assert!(len >= 4 && p > i && p < e);
                if p < lim {
                    // SAFETY: p < lim = n - 8; tab has 1 << bits slots of 4
                    unsafe { insert4(d.as_ptr(), p, tab.as_mut_ptr() as *mut u32, sh, b1) };
                }
                continue;
            }
            if p > i && p < e && p < lim {
                let h = key(rd64(d, p));
                let te = unsafe { tab.get_unchecked_mut(h) };
                let s = *te;
                let mut x = [0u32; W];
                x[0] = (p + b1) as u32;
                x[1..].copy_from_slice(&s[..W - 1]);
                *te = x;
            }
        }
        i = e;
        anchor = i;
    }
    seqs
}

/// Pack with the fast parse: level 0-3 = (bucket size 1 or 2, lazy or not).
pub fn compress_fast(data: &[u8], level: u32) -> Vec<u8> {
    encode(data, &parse_level(data, level))
}

/// compress_fast's parse.
fn parse_level(data: &[u8], level: u32) -> Vec<Seq> {
    match level {
        0 => parse_fast::<1>(data, 5, false),
        1 => parse_fast::<2>(data, 5, false),
        2 => parse_fast::<2>(data, 6, true),
        _ => parse_fast::<4>(data, 7, true),
    }
}

// ---------------------------------------------------------------- the table bank
/// A frame's table slots: the one literal table, the two literal tables (by context), the
/// three copy tables (literal count, copy length, distance); and their alphabets.
const SLOTS: usize = 6;
const ALPHA: [usize; SLOTS] = [256, 256, 256, NLL, NLL, NOF];
/// Streams 1 to BANK_STREAMS - 1 may have tables of their own in a bank; stream 0's tables are
/// shared by all streams.
pub const BANK_STREAMS: usize = 16;
const NBANK: usize = BANK_STREAMS * SLOTS;

/// A unit's bank of tables, written once for all its blocks: entry k * SLOTS + s is stream
/// k's table for slot s (k = 0: the one shared by every stream). Written as varint(entries),
/// then per entry, in order, its number (one byte) and the table (write_table).
#[derive(Clone)]
pub struct Bank(Vec<Option<Vec<u32>>>);

/// A frame's view of a bank: the bank and the frame's stream.
pub type Refs<'a> = Option<(&'a Bank, usize)>;

/// The bank entry a table header h (0: the stream's, 1: the shared one) of slot s names.
fn bank_key(r: Refs, h: u8, s: usize) -> Option<usize> {
    let (_, k) = r?;
    let i = if h == 0 { k } else if h == 1 { 0 } else { return None } * SLOTS + s;
    (i < NBANK).then_some(i)
}

fn bank_of<'a>(r: Refs<'a>, h: u8, s: usize) -> Option<&'a [u32]> {
    r?.0.0[bank_key(r, h, s)?].as_deref()
}

/// The table for counts c in slot s: the own one f, or the bank's (r) when that costs fewer
/// bits with its 1-byte reference: (data bits, table bits, header: 0 or 1 the bank's, 2 own).
/// Empty counts have no own table.
fn choose(c: &[u32], f: &[u32], r: Refs, s: usize) -> (f64, f64, u8) {
    let mut best = if c.iter().any(|&x| x > 0) { (cost_bits(c, f), tbl_bits(f), 2) } else { (f64::INFINITY, 0.0, 2) };
    for h in 0..2 {
        if let Some(t) = bank_of(r, h, s) {
            let d = cost_bits(c, t);
            if d + 8.0 < best.0 + best.1 {
                best = (d, 8.0, h);
            }
        }
    }
    best
}

/// read_table, or a 1-byte reference to the bank's table for slot s (r: an error when there
/// is no such table).
fn read_table_r(b: &[u8], p: &mut usize, f: &mut [u32], r: Refs, s: usize) -> Option<()> {
    let h = *b.get(*p)?;
    if h < 2 {
        let t = bank_of(r, h, s)?;
        if t.len() != f.len() {
            return None;
        }
        f.copy_from_slice(t);
        *p += 1;
        return Some(());
    }
    read_table(b, p, f)
}

impl Bank {
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|t| t.is_none())
    }
    /// The bank's entries whose bit is in `used`, as Bank::read reads them.
    pub fn write(&self, used: u128) -> Vec<u8> {
        let mut o = Vec::new();
        let n = (0..NBANK).filter(|&i| used >> i & 1 == 1 && self.0[i].is_some()).count();
        put_varint(&mut o, n as u64);
        for (i, t) in self.0.iter().enumerate() {
            if let (Some(f), 1) = (t, used >> i & 1) {
                o.push(i as u8);
                write_table(&mut o, f);
            }
        }
        o
    }
    /// A bank as `write` makes it: None unless it has 1 to NBANK entries, their numbers below
    /// NBANK and going up, each table valid for its slot's alphabet, and nothing is left over.
    pub fn read(b: &[u8]) -> Option<Bank> {
        let mut p = 0;
        let n = get_varint(b, &mut p)?;
        if n == 0 || n > NBANK as u64 {
            return None;
        }
        let mut t = vec![None; NBANK];
        let mut next = 0;
        for _ in 0..n {
            let i = *b.get(p)? as usize;
            p += 1;
            if i < next || i >= NBANK {
                return None;
            }
            next = i + 1;
            let mut f = vec![0u32; ALPHA[i % SLOTS]];
            read_table(b, &mut p, &mut f)?;
            t[i] = Some(f);
        }
        (p == b.len()).then_some(Bank(t))
    }
}

/// The bank for a unit's frames (stream, parsed input after `code`): per slot, a table for each
/// stream and one shared by all, from the frames' summed counts. An entry stays while the frames
/// whose table it would replace (where it costs fewer bits than their own, each slot on its
/// own) save more bits than it costs; the first 2 rounds also make each entry the table of
/// those frames. (Estimates: frames are coded again and kept by their real bytes afterwards.)
pub fn make_bank(fr: &[(usize, &Prep)]) -> Bank {
    let mut sum = vec![[0u64; 256]; NBANK];
    let mut b = Bank(vec![None; NBANK]);
    let mut lg = vec![[0f32; 256]; NBANK];
    for it in 0..7 {
        // round 0: the summed counts; then each entry's bits per symbol, the frames that would
        // use it (their gain and counts), and the entries that do not pay dropped
        let mut gain = [0f64; NBANK];
        for i in 0..NBANK {
            if let Some(f) = &b.0[i] {
                for j in 0..f.len() {
                    lg[i][j] = PB as f32 - (f[j] as f32).log2();
                }
            }
        }
        if it > 0 {
            sum.iter_mut().for_each(|x| *x = [0; 256]);
        }
        for &(k, p) in fr {
            for s in 0..SLOTS {
                if !has(p, s) || k >= BANK_STREAMS {
                    continue;
                }
                let mut best = (p.own[s], NBANK);
                for i in [k * SLOTS + s, s] {
                    if it == 0 {
                        add_counts(&mut sum[i], &p.cs[s]);
                    } else if b.0[i].is_some() {
                        let mut x = 8.0;
                        for j in 0..256 {
                            if p.cs[s][j] > 0 {
                                x += p.cs[s][j] as f64 * lg[i][j] as f64;
                            }
                        }
                        if x < best.0 {
                            best = (x, i);
                        }
                    }
                }
                if best.1 < NBANK {
                    gain[best.1] += p.own[s] - best.0;
                    add_counts(&mut sum[best.1], &p.cs[s]);
                }
            }
        }
        let mut drop = false;
        for i in 0..NBANK {
            if it > 0 && b.0[i].as_ref().is_some_and(|f| gain[i] <= tbl_bits(f) + 8.0) {
                b.0[i] = None;
                drop = true;
            } else if it < 3 && (it == 0 || b.0[i].is_some()) {
                b.0[i] = table_of(&sum[i][..ALPHA[i % SLOTS]]);
            }
        }
        if !drop && it >= 3 {
            break;
        }
    }
    b
}

/// A unit's table bank and its frames (each frame, its stream and its parsed input after
/// `code`): the bank (make_bank), then every frame coded again with it, taken only when that
/// is smaller (real bytes); the bank (the entries those frames name) is kept only when they
/// save more than its bytes and 10 more (its length), else no frame changes. The bank's bytes
/// (empty for none).
pub fn bank_frames(fr: &mut [(&mut Vec<u8>, usize, &Prep)]) -> Vec<u8> {
    let b = make_bank(&fr.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>());
    if b.is_empty() {
        return Vec::new();
    }
    let (mut saved, mut used, mut new) = (0, 0u128, Vec::with_capacity(fr.len()));
    for (f, k, p) in fr.iter() {
        let (g, u, _) = fin(p, Some((&b, *k)), f);
        let take = !g.is_empty() && g.len() < f.len();
        if take {
            saved += f.len() - g.len();
            used |= u;
        }
        new.push(take.then_some(g));
    }
    let bytes = b.write(used);
    if saved <= bytes.len() + 10 {
        return Vec::new();
    }
    for ((f, _, _), g) in fr.iter_mut().zip(new) {
        if let Some(g) = g {
            **f = g;
        }
    }
    bytes
}

fn add_counts(a: &mut [u64; 256], c: &[u32; 256]) {
    for (a, &x) in a.iter_mut().zip(c) {
        *a += x as u64;
    }
}

/// The table of summed counts c (scaled to 32 bits), None when all are 0.
fn table_of(c: &[u64]) -> Option<Vec<u32>> {
    let m = *c.iter().max()?;
    let sh = (64 - m.leading_zeros()).saturating_sub(31);
    let c: Vec<u32> = c.iter().map(|&x| ((x >> sh) as u32).max((x > 0) as u32)).collect();
    let mut f = vec![0u32; c.len()];
    normalize(&c, &mut f);
    (m > 0).then_some(f)
}

#[cfg(test)]
mod format_tests {
    use super::*;
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }
    /// The literal part of a frame, as fin writes it (no bank).
    fn lits_part(lits: &[u8]) -> Vec<u8> {
        let mut pr = prep(lits, &[]);
        let f = code(&mut pr);
        // (6, varint(n), varint(0 copies), varint(literals), then the literal part)
        let mut p = 1;
        for _ in 0..3 {
            get_varint(&f, &mut p).unwrap();
        }
        f[p..].to_vec()
    }
    fn mk(r: &mut Rng, n: usize, kind: usize) -> Vec<u8> {
        if kind == 7 {
            // varints of skewed numbers: after a byte with its top bit set the next one differs
            let mut v = Vec::with_capacity(n + 10);
            while v.len() < n {
                let mut x = (r.next() % 5000) >> (r.next() % 12);
                while x >= 0x80 {
                    v.push(x as u8 | 0x80);
                    x >>= 7;
                }
                v.push(x as u8);
            }
            v.truncate(n);
            return v;
        }
        (0..n)
            .map(|i| match kind {
                0 => (r.next() % 256) as u8,                                  // uniform: stored
                1 => [b'a', b'b'][(r.next() % 2) as usize],                   // 2 symbols
                2 => (r.next().trailing_zeros() as u8).wrapping_mul(37),     // skewed, spread
                3 => if r.next() % 3 == 0 { 0x80 | (r.next() % 128) as u8 } else { (r.next() % 20) as u8 }, // top-bit contexts
                4 => (r.next() % 256).min(r.next() % 256) as u8,              // all 256, skewed
                5 => if i % 1000 == 999 { 1 } else { 0 },                     // lone symbol + rare
                _ => 7,                                                        // one byte
            })
            .collect()
    }
    /// Data with copies, in a few styles (like a block's column streams).
    fn col(r: &mut Rng, n: usize, style: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 32);
        while v.len() < n {
            let x = r.next() % (50 + 400 * style);
            match style % 3 {
                0 => v.extend_from_slice(format!("k{} v={}\n", x % 7, x).as_bytes()),
                1 => {
                    let mut y = x;
                    while y >= 0x80 {
                        v.push(y as u8 | 0x80);
                        y >>= 7;
                    }
                    v.push(y as u8);
                }
                _ => v.extend_from_slice(&[(x % 13) as u8, b' ', (x % 251) as u8]),
            }
        }
        v.truncate(n);
        v
    }
    /// A whole fz frame (method byte 6 first) opened with a bank view.
    fn open_frame(f: &[u8], r: Refs) -> Option<Vec<u8>> {
        let mut p = 1;
        let n = get_varint(f, &mut p)? as usize;
        if *f.first()? != 6 || n > 1 << 24 {
            return None;
        }
        decompress_r(f.get(p..)?, n, r)
    }
    /// A bank written raw: entries `keys`, each a flat table of its slot's alphabet.
    fn raw_bank(keys: &[usize]) -> Vec<u8> {
        let mut o = Vec::new();
        put_varint(&mut o, keys.len() as u64);
        for &i in keys {
            o.push(i as u8);
            let mut f = vec![0u32; ALPHA[i % SLOTS]];
            normalize(&vec![1u32; f.len()], &mut f);
            write_table(&mut o, &f);
        }
        o
    }
    /// The table bank: frames that name it open exactly with it and not without it; its
    /// reader and the frames' references refuse hostile bytes (cut short, counts too big or 0,
    /// entries out of order, repeated or out of range, tables outside their alphabet, header
    /// values other than 0 and 1 below 4, references without a bank or to a missing entry)
    /// and never panic on damage (FZ_FUZZ rounds, 20000 by default).
    #[test]
    fn bank_round_trips_and_refuses_damage() {
        let rounds: usize = crate::engine::fl::no_env::var("FZ_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(20000);
        let mut r = Rng(0xB4B4_2026);
        let mut fr = Vec::new();
        for b in 0..8u64 {
            for k in [1usize, 3, 4, 6, 7] {
                let n = 200 + (r.next() % 20000) as usize;
                let d = col(&mut r, n, k as u64 + b % 2);
                let mut p = prep_fast(&d, 3);
                let f0 = code(&mut p);
                assert_eq!(open_frame(&f0, None).as_deref(), Some(&d[..]));
                fr.push((k, p, d, f0));
            }
        }
        let frk: Vec<(usize, &Prep)> = fr.iter().map(|(k, p, _, _)| (*k, p)).collect();
        let bank = make_bank(&frk);
        assert!(!bank.is_empty());
        let mut used = 0u128;
        let mut frames = Vec::new();
        for (k, p, d, f0) in &fr {
            let (f, u, _) = fin(p, Some((&bank, *k)), f0);
            used |= u;
            // (a frame that would name no entry comes out empty: it is the frame without the bank)
            assert_eq!(f.is_empty(), u == 0);
            frames.push((*k, if f.is_empty() { f0.clone() } else { f }, d));
        }
        assert!(used != 0);
        let bytes = bank.write(used);
        let back = Bank::read(&bytes).expect("bank");
        assert_eq!(back.write(u128::MAX), bytes);
        let mut named = 0;
        for (k, f, d) in &frames {
            assert_eq!(open_frame(f, Some((&back, *k))).as_deref(), Some(&d[..]));
            if open_frame(f, None).is_none() && open_frame(f, Some((&Bank(vec![None; NBANK]), *k))).is_none() {
                named += 1;
            }
        }
        assert!(named > 0, "no frame names the bank");
        // the bank's reader
        for i in 0..bytes.len() {
            assert!(Bank::read(&bytes[..i]).is_none(), "cut at {}", i);
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(Bank::read(&extra).is_none());
        for c in [0u64, NBANK as u64 + 1, 1 << 40] {
            let mut b = Vec::new();
            put_varint(&mut b, c);
            b.extend_from_slice(&bytes[1..]);
            assert!(Bank::read(&b).is_none(), "count {}", c);
        }
        assert!(Bank::read(&raw_bank(&[3, 9, 95])).is_some());
        assert!(Bank::read(&raw_bank(&[9, 3])).is_none(), "crossed");
        assert!(Bank::read(&raw_bank(&[9, 9])).is_none(), "repeated");
        assert!(Bank::read(&raw_bank(&[96])).is_none(), "out of range");
        assert!(Bank::read(&raw_bank(&[255])).is_none(), "out of range");
        // a 256-symbol table where the slot's alphabet has 72
        let mut wide = raw_bank(&[0]);
        wide[1] = 3;
        assert!(Bank::read(&wide).is_none(), "alphabet");
        // table headers: 0 and 1 name the bank (the stream's entry, the shared one), 2 and 3 are refused
        let mut f = [0u32; 256];
        let one = Bank::read(&raw_bank(&[0, 7 * SLOTS])).unwrap();
        for (h, k, ok) in [(0u8, 7, true), (1, 7, true), (0, 4, false), (2, 7, false), (3, 7, false)] {
            let mut q = 0;
            assert_eq!(read_table_r(&[h, 0, 0], &mut q, &mut f, Some((&one, k)), 0).is_some(), ok, "header {} stream {}", h, k);
            let mut q = 0;
            assert!(read_table_r(&[h, 0, 0], &mut q, &mut f, None, 0).is_none() || h > 1);
        }
        // damage: banks and frames, never a panic; a bank read is valid table by table
        for _ in 0..rounds {
            let mut b = bytes.clone();
            for _ in 0..1 + r.next() % 3 {
                let i = r.next() as usize % b.len();
                match r.next() % 3 {
                    0 => b[i] ^= 1 << (r.next() % 8),
                    1 => b[i] = r.next() as u8,
                    _ => b.truncate(i.max(1)),
                }
            }
            let res = std::panic::catch_unwind(|| {
                if let Some(x) = Bank::read(&b) {
                    for (i, t) in x.0.iter().enumerate() {
                        if let Some(t) = t {
                            assert!(t.len() == ALPHA[i % SLOTS] && t.iter().sum::<u32>() == PM && t.iter().filter(|&&q| q > 0).count() >= 2);
                        }
                    }
                }
            });
            assert!(res.is_ok(), "bank damage");
            let (k, f, _) = &frames[r.next() as usize % frames.len()];
            let mut g = f.clone();
            for _ in 0..1 + r.next() % 3 {
                let i = r.next() as usize % g.len();
                match r.next() % 3 {
                    0 => g[i] ^= 1 << (r.next() % 8),
                    1 => g[i] = r.next() as u8,
                    _ => g.truncate(i.max(1)),
                }
            }
            let kk = if r.next() % 4 == 0 { r.next() as usize % 20 } else { *k };
            let res = std::panic::catch_unwind(|| {
                let _ = open_frame(&g, Some((&back, kk)));
                let _ = open_frame(&g, None);
            });
            assert!(res.is_ok(), "frame damage");
        }
    }
    #[test]
    fn literals_round_trip_at_lane_bounds() {
        let mut r = Rng(0x9E3779B97F4A7C15);
        let sizes = [1usize, 2, 3, 4, 7, 8, 9, 15, 16, 17, 255, 256, 257, 510, 511, 512, 513, 514, 1000, 4094, 4095, 4096, 4097, 4098, 8191, 65534, 65535, 65536, 65537, 65538, 65543, 100000, 300001];
        let mut modes = [0usize; 8];
        for &n in &sizes {
            for kind in 0..8 {
                let lits = mk(&mut r, n, kind);
                let mut out = vec![0xAAu8; 3];
                out.extend_from_slice(&lits_part(&lits));
                modes[out[3] as usize] += 1;
                out.extend_from_slice(&[0x55; 5]);
                let mut p = 3usize;
                let mut v = Vec::new();
                get_literals(&out, &mut p, n, &mut v, None).expect("decode");
                assert_eq!(&v[..n], &lits[..], "n {} kind {}", n, kind);
                assert_eq!(p, out.len() - 5, "end n {} kind {}", n, kind);
            }
        }
        eprintln!("modes {:?}", modes);
        assert!(modes[1] > 0 && modes[2] > 0 && modes[3] > 0 && modes[4] > 0);
    }
    #[test]
    fn tables_round_trip() {
        let mut r = Rng(12345);
        let mut forms = [0usize; 2];
        for len in [256usize, 72, 75, 2, 9] {
            for _ in 0..3000 {
                let nzw = 2 + (r.next() as usize % (len - 1));
                let mut c = vec![0u32; len];
                for _ in 0..nzw {
                    let i = r.next() as usize % len;
                    c[i] += 1 + (r.next() % if r.next() % 2 == 0 { 3 } else { 5000 }) as u32;
                }
                let mut f = vec![0u32; len];
                normalize(&c, &mut f);
                assert_eq!(f.iter().sum::<u32>(), PM);
                let mut out = Vec::new();
                write_table(&mut out, &f);
                forms[(out[0] & 1) as usize] += 1;
                let mut g = vec![0u32; len];
                let mut p = 0;
                read_table(&out, &mut p, &mut g).expect("table");
                assert_eq!(f, g);
                assert_eq!(p, out.len());
            }
        }
        eprintln!("forms {:?}", forms);
        assert!(forms[0] > 0 && forms[1] > 0);
    }
    #[test]
    fn damaged_never_panics() {
        let mut r = Rng(777);
        let mut tries = 0usize;
        for &n in &[3usize, 600, 5000, 70000] {
            for kind in [1usize, 2, 3, 4, 7] {
                let lits = mk(&mut r, n, kind);
                // whole fz frames too
                let frame = compress_fast(&lits, 3);
                let out = lits_part(&lits);
                for _ in 0..1500 {
                    for src in [&out, &frame] {
                        let mut d = src.clone();
                        let k = 1 + r.next() as usize % 4;
                        for _ in 0..k {
                            let i = r.next() as usize % d.len();
                            match r.next() % 3 {
                                0 => d[i] ^= 1 << (r.next() % 8),
                                1 => d[i] = r.next() as u8,
                                _ => d.truncate(i),
                            }
                            if d.is_empty() {
                                break;
                            }
                        }
                        let res = std::panic::catch_unwind(|| {
                            let mut p = 0usize;
                            let mut v = Vec::new();
                            let _ = get_literals(&d, &mut p, n, &mut v, None);
                            if d.len() > 1 {
                                let mut q = 1usize;
                                let _ = get_varint(&d, &mut q).map(|m| decompress(&d[q.min(d.len())..], (m as usize).min(1 << 20)));
                            }
                        });
                        assert!(res.is_ok(), "panic on damaged input n {} kind {}", n, kind);
                        tries += 1;
                    }
                }
            }
        }
        eprintln!("damaged tries {}", tries);
    }

    /// Where the tables of a whole fz frame (method byte first) start, walked as decompress_r
    /// reads it with the bank view r: the literal tables, then the copy tables.
    fn table_spots(f: &[u8], r: Refs) -> Option<Vec<(usize, usize)>> {
        let mut p = 1;
        get_varint(f, &mut p)?;
        let nseq = get_varint(f, &mut p)? as usize;
        let nlit = get_varint(f, &mut p)? as usize;
        let mode = *f.get(p)?;
        p += 1;
        let mut v = Vec::new();
        let mut t = [0u32; 256];
        match mode {
            1 | 2 => {
                for s in if mode == 1 { 0..1 } else { 1..3 } {
                    v.push((p, s));
                    read_table_r(f, &mut p, &mut t, r, s)?;
                }
                for _ in 0..lanes_of(nlit) {
                    Words::new(f, &mut p)?;
                }
            }
            3 => p += 1,
            4 => p += nlit,
            _ => {}
        }
        if nseq > 0 {
            for s in 3..6 {
                v.push((p, s));
                read_table_r(f, &mut p, &mut t[..ALPHA[s]], r, s)?;
            }
        }
        Some(v)
    }
    /// The checker's own bank test (refute_b4_bank): every byte of a real bank set to each of the
    /// 256 values (Bank::read never panics, and every bank it accepts holds only valid tables of
    /// its slots' alphabets, which the frames then open with, without a panic); every table header
    /// of every frame set to each of the 256 values, opened with its bank, with the bank of another
    /// stream, with an empty bank and with none (no panic; 0 and 1 refused without a bank); and
    /// frames that name the bank are refused without it.
    #[test]
    fn bank_every_byte_and_header() {
        let mut r = Rng(0x5EED_B4B4_0BAD_F00D);
        // column-like streams: varint numbers, tokens, ids (3 kinds, 6 streams, 6 blocks)
        let mut fr = Vec::new();
        for blk in 0..6u64 {
            for k in [1usize, 3, 4, 5, 7, 9] {
                let n = 3000 + (r.next() % 30000) as usize;
                let mut d = Vec::with_capacity(n + 64);
                while d.len() < n {
                    let x = r.next();
                    match (k + blk as usize) % 3 {
                        0 => {
                            let mut y = (x % (1000 + 5000 * k as u64)) as u64;
                            while y >= 0x80 {
                                d.push(y as u8 | 0x80);
                                y >>= 7;
                            }
                            d.push(y as u8);
                        }
                        1 => d.extend_from_slice(format!("blk_{} ", x % 977).as_bytes()),
                        _ => d.extend_from_slice(&[b'a' + (x % 26) as u8, (x >> 8) as u8 & 0x3f, b'\n']),
                    }
                }
                d.truncate(n);
                let mut p = prep_fast(&d, 3);
                let f0 = code(&mut p);
                fr.push((k, p, d, f0));
            }
        }
        let bank = make_bank(&fr.iter().map(|(k, p, _, _)| (*k, p)).collect::<Vec<_>>());
        let mut used = 0u128;
        let mut named = Vec::new();
        for (k, p, d, f0) in &fr {
            let (f, u, _) = fin(p, Some((&bank, *k)), f0);
            if !f.is_empty() {
                used |= u;
                named.push((*k, f, d.clone()));
            }
        }
        assert!(!named.is_empty(), "no frame names the bank");
        let bytes = bank.write(used);
        let back = Bank::read(&bytes).expect("bank");
        let open = |f: &[u8], rr: Refs| -> Option<Vec<u8>> {
            let mut p = 1;
            let n = get_varint(f, &mut p)? as usize;
            decompress_r(f.get(p..)?, n, rr)
        };
        let empty = Bank(vec![None; NBANK]);
        for (k, f, d) in &named {
            assert_eq!(open(f, Some((&back, *k))).as_deref(), Some(&d[..]));
            assert!(open(f, None).is_none() && open(f, Some((&empty, *k))).is_none(), "a frame naming the bank opened without it");
        }
        // every byte of the bank, every value
        let mut accepted = 0usize;
        for i in 0..bytes.len() {
            for v in 0..=255u8 {
                let mut b = bytes.clone();
                b[i] = v;
                if let Some(x) = Bank::read(&b) {
                    accepted += 1;
                    for (e, t) in x.0.iter().enumerate() {
                        if let Some(t) = t {
                            assert!(t.len() == ALPHA[e % SLOTS] && t.iter().sum::<u32>() == PM && t.iter().filter(|&&q| q > 0).count() >= 2, "entry {} after byte {}={}", e, i, v);
                        }
                    }
                    // (opened with the changed bank: no panic; one frame in 8 to keep it short)
                    if (i * 256 + v as usize) % 8 == 0 {
                        for (k, f, _) in &named {
                            let _ = open(f, Some((&x, *k)));
                        }
                    }
                }
            }
        }
        // every table header of every frame, every value, with every kind of bank view
        let mut spots = 0usize;
        for (k, f, _) in &named {
            let s = table_spots(f, Some((&back, *k))).expect("walk");
            assert!(s.iter().any(|&(p, _)| f[p] < 2), "a named frame without a reference");
            for &(p, slot) in &s {
                spots += 1;
                for v in 0..=255u8 {
                    let mut g = f.clone();
                    g[p] = v;
                    for rr in [Some((&back, *k)), Some((&back, (*k + 2) % BANK_STREAMS)), Some((&back, 0)), Some((&back, 15)), Some((&empty, *k)), None] {
                        let res = open(&g, rr);
                        if v < 2 && rr.is_none() {
                            assert!(res.is_none(), "reference {} without a bank at slot {}", v, slot);
                        }
                    }
                }
            }
        }
        eprintln!("bank bytes {} (accepted changed banks {}), frames {}, table headers {}", bytes.len(), accepted, named.len(), spots);
    }
}
