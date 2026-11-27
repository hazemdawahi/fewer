//! The core of the DNA index: an FM index over a text of bytes, the kind of
//! index genome search tools use. It counts the places where a piece of text occurs (backward
//! search, in steps proportional to the piece's length) and gives each place (locate), without
//! reading the text, which it does not keep.
//!
//! How it works:
//! - **Rows.** The text gets an end mark after it, smaller than every byte (the end mark is
//!   not a byte, so 0x00 and 0xff are ordinary symbols). The text has n + 1 suffixes (the
//!   text from a place to its end, then the end mark); sorted, each is a row. Row 0 is the
//!   end mark alone, the suffix at place n.
//! - **The suffix array** gives the place of each row's suffix, in row order. `suffix_array`
//!   builds it with SA-IS (Nong, Zhang and Chan, 2009), in time and memory proportional to
//!   the text's length: our own code.
//! - **The BWT** (Burrows-Wheeler transform) is the byte before each row's suffix. The row
//!   of the whole text (place 0) has none: the end mark stands there.
//! - **Backward search.** The places where a piece occurs are the rows whose suffixes start
//!   with it, and those rows are next to each other. Taking the piece's bytes from its last
//!   to its first: the rows that start with byte c followed by the rows found so far (from
//!   `sp` to `ep`) are `first[c] + rank(c, sp)` to `first[c] + rank(c, ep)`, where
//!   `rank(c, r)` counts the c's in the BWT before row r and `first[c]` is the first row
//!   that starts with c (the counts table). Counting takes two ranks per byte of the piece.
//! - **The wavelet matrix** keeps the BWT and answers `rank` with one bit-table rank per
//!   level. Each level is a bit table of one bit of each symbol's code, with a rank table
//!   (the number of 1 bits before every 512 bits). Only the bytes the text holds get a code,
//!   in byte order, so the DNA index's text (letters, digits, 0x01 and one symbol for all
//!   bytes >= 0x80: 38 symbols) takes 6 levels; any text of bytes takes at most 8.
//! - **Locate.** The place of every 32nd position of the text (`SAMPLE`) is kept, with a bit
//!   table marking the rows that hold them. From any other row, LF steps (`back`: to the row
//!   of the suffix that starts one byte earlier) reach a marked row in at most 31 steps; its
//!   kept place plus the number of steps is the row's place.
//! - **No places kept** (`no_places`, the DNA index's shrunk form): no marks and no kept places, 2 of the 8 bits per byte less. Count and LF
//!   steps work as before; `locate` and `positions` give None. The DNA index finds a hit's
//!   token by LF steps back to the mark before it instead (dna.rs), and `text_row` says where
//!   such a walk ends: at the whole text's row, which has no byte before it.
//!
//! The empty piece occurs at every place from 0 to n (n + 1 places), as Rust's
//! `str::match_indices("")` gives.
//!
//! **The stored form** (`write`, `read`; numbers are little-endian):
//!
//! ```text
//! fm      = "CMFM" 01 n:u32 start-row:u32 rate:u32 symbols:u16
//!           symbol:u8*symbols                  the bytes the text holds, in byte order
//!           count:u32*symbols                  how often each one occurs
//!           level*                             one per bit of a code (none for one symbol)
//!           marks:bits(n + 1)                  the rows whose place is a multiple of rate
//!           place:u32*(n / rate + 1)           those rows' places, in row order
//! level   = zeros:u32 bits(n)                  one bit of the code of each BWT byte, in row
//!                                              order, without the start row's end mark
//! bits(k) = u64*(k / 64 rounded up)            bit i is bit i % 64 of word i / 64
//! ```
//!
//! start-row is the row of the whole text's suffix. Rate 0 means no places are kept: then
//! neither marks nor places follow the levels (a reader made before rate 0 refuses such an
//! index: an error, never a wrong answer). The rank tables are not stored: `read` makes them
//! again (one count per 64-bit word) and checks the parts against each other, so a damaged
//! index gives an error, or answers that never panic and always end. A CRC around the whole
//! file (dna.rs) catches the rest.

use std::ops::Range;

/// One place in `SAMPLE` is kept for locate (every 32nd suffix-array entry,
/// with a bit table marking where they are).
pub const SAMPLE: u32 = 32;
/// The longest text an index takes: places and rows are kept in 32 bits (a segment of an
/// archive holds at most 256 MiB).
pub const MAX_LEN: usize = u32::MAX as usize - 1;
/// The highest sample rate (locate takes up to rate - 1 steps).
pub const MAX_RATE: u32 = 1 << 16;

const MAGIC: [u8; 4] = *b"CMFM";
const VERSION: u8 = 1;
/// A free slot of the suffix array while it is built.
const EMPTY: u32 = u32::MAX;
/// In the table of codes: a byte the text does not hold.
const NO_CODE: u16 = u16::MAX;

// ------------------------------------------------------------------------------ bit tables

/// A bit table with a rank table (the number of 1 bits before every 512 bits), so `rank1`
/// counts at most 7 whole words and part of one.
#[derive(Clone, Debug)]
pub struct Bits {
    len: usize,
    words: Vec<u64>,
    /// `blocks[k]`: the 1 bits in `words[..8 * k]`.
    blocks: Vec<u32>,
}

impl Bits {
    /// The table of `len` bits held in `words` (bit i is bit i % 64 of word i / 64). None
    /// when there are not exactly `len / 64` words rounded up, a bit past `len` is set, or
    /// `len` is over `u32::MAX`.
    pub fn from_words(words: Vec<u64>, len: usize) -> Option<Bits> {
        if len > u32::MAX as usize || words.len() != len.div_ceil(64) {
            return None;
        }
        if !len.is_multiple_of(64) && words[len / 64] >> (len % 64) != 0 {
            return None;
        }
        let mut blocks = Vec::with_capacity(words.len() / 8 + 1);
        let mut ones = 0u32;
        for (i, w) in words.iter().enumerate() {
            if i.is_multiple_of(8) {
                blocks.push(ones);
            }
            ones += w.count_ones();
        }
        if words.len().is_multiple_of(8) {
            blocks.push(ones);
        }
        Some(Bits { len, words, blocks })
    }

    /// The number of bits.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The words that hold the bits (what `from_words` takes).
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Bit `i` (`i` below `len`).
    pub fn get(&self, i: usize) -> bool {
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    /// The number of 1 bits before bit `i` (`i` at most `len`).
    pub fn rank1(&self, i: usize) -> usize {
        let w = i / 64;
        let k = w / 8;
        let mut r = self.blocks[k] as usize;
        for x in &self.words[k * 8..w] {
            r += x.count_ones() as usize;
        }
        if !i.is_multiple_of(64) {
            r += (self.words[w] & ((1u64 << (i % 64)) - 1)).count_ones() as usize;
        }
        r
    }

    /// The number of 0 bits before bit `i` (`i` at most `len`).
    pub fn rank0(&self, i: usize) -> usize {
        i - self.rank1(i)
    }

    /// The number of 1 bits.
    pub fn ones(&self) -> usize {
        self.rank1(self.len)
    }
}

// --------------------------------------------------------------------------- suffix array

/// The suffix array of `text`: the place of every suffix, the suffixes in sorted order (a
/// suffix that is the start of a longer one comes first, as if an end mark smaller than
/// every byte followed the text). Built with SA-IS in time proportional to the text's
/// length. Besides the result (4 bytes per byte) it needs one bit per byte for the suffix
/// types (less in the recursion) and the bucket tables of one level at a time: 8 bytes per
/// symbol, where the recursion's symbols are the distinct LMS substrings (fewer than half
/// the bytes, and far fewer in texts that repeat).
///
/// Panics when `text` is longer than `MAX_LEN` (`Fm::new` checks this and returns an error).
pub fn suffix_array(text: &[u8]) -> Vec<u32> {
    assert!(text.len() <= MAX_LEN, "a text of {} bytes is too long for a suffix array", text.len());
    let mut sa = vec![0u32; text.len()];
    sais(text, 256, &mut sa);
    sa
}

/// A symbol of a text that SA-IS sorts: bytes at the top, names of LMS substrings below.
trait Sym: Copy {
    fn at(self) -> usize;
}

impl Sym for u8 {
    fn at(self) -> usize {
        self as usize
    }
}

impl Sym for u32 {
    fn at(self) -> usize {
        self as usize
    }
}

/// One bit per place: whether the suffix there is S-type.
struct Types(Vec<u64>);

impl Types {
    fn get(&self, i: usize) -> bool {
        (self.0[i / 64] >> (i % 64)) & 1 == 1
    }

    /// An LMS place: an S-type suffix right after an L-type one ("leftmost S").
    fn lms(&self, i: usize) -> bool {
        i > 0 && self.get(i) && !self.get(i - 1)
    }
}

/// How often each symbol below `k` occurs in `s`.
fn symbol_counts<S: Sym>(s: &[S], k: usize) -> Vec<u32> {
    let mut counts = vec![0u32; k];
    for x in s {
        counts[x.at()] += 1;
    }
    counts
}

/// Sets `bkt` to where each symbol's bucket (the rows of suffixes starting with it) starts.
fn bucket_starts(counts: &[u32], bkt: &mut [u32]) {
    let mut sum = 0;
    for (b, &c) in bkt.iter_mut().zip(counts) {
        *b = sum;
        sum += c;
    }
}

/// Sets `bkt` to where each symbol's bucket ends (one past its last row).
fn bucket_ends(counts: &[u32], bkt: &mut [u32]) {
    let mut sum = 0;
    for (b, &c) in bkt.iter_mut().zip(counts) {
        sum += c;
        *b = sum;
    }
}

/// SA-IS: fills `sa` (as long as `s`) with the suffix array of `s`, whose symbols are below
/// `k`. A suffix is S-type when it is smaller than the suffix one place later, else L-type;
/// the last one is L-type (the end mark after it is smaller). Sorting the LMS suffixes is
/// enough: from them, one pass left to right places the L-type suffixes in order and one
/// pass right to left the S-type ones ("induced sorting"). The LMS suffixes are sorted by
/// first sorting the LMS substrings the same way, naming them in order, and, when two
/// names are equal, sorting the suffixes of the text of names by recursion (at most half as
/// long). The text of names and the recursion's result live in `sa` itself, and this
/// level's bucket tables are freed while the recursion runs.
fn sais<S: Sym>(s: &[S], k: usize, sa: &mut [u32]) {
    let n = s.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        sa[0] = 0;
        return;
    }
    let mut types = Types(vec![0; n.div_ceil(64)]);
    for i in (0..n - 1).rev() {
        let (a, b) = (s[i].at(), s[i + 1].at());
        if a < b || (a == b && types.get(i + 1)) {
            types.0[i / 64] |= 1 << (i % 64);
        }
    }
    let mut counts = symbol_counts(s, k);
    let mut bkt = vec![0u32; k];

    // 1. The LMS suffixes at the ends of their buckets, in any order; induced sorting then
    //    puts the LMS substrings (from one LMS place to the next, both included) in order.
    sa.fill(EMPTY);
    bucket_ends(&counts, &mut bkt);
    for (i, x) in s.iter().enumerate().skip(1) {
        if types.lms(i) {
            let c = x.at();
            bkt[c] -= 1;
            sa[bkt[c] as usize] = i as u32;
        }
    }
    induce(s, &types, &counts, &mut bkt, sa);

    // 2. The LMS places in that order, to the front of `sa`. With none, `sa` is complete.
    let mut m = 0;
    for i in 0..n {
        let p = sa[i];
        if p != EMPTY && types.lms(p as usize) {
            sa[m] = p;
            m += 1;
        }
    }
    if m == 0 {
        return;
    }

    // 3. Names: equal LMS substrings get the same name, in sorted order. LMS places are at
    //    least two apart, so the name of the one at p fits at m + p / 2; then the names, in
    //    text order, move to the end of `sa`.
    sa[m..].fill(EMPTY);
    let mut names = 0usize;
    let mut prev = usize::MAX;
    for i in 0..m {
        let p = sa[i] as usize;
        if prev == usize::MAX || !same_lms(s, &types, prev, p) {
            names += 1;
        }
        prev = p;
        sa[m + p / 2] = (names - 1) as u32;
    }
    let mut w = n;
    for j in (m..n).rev() {
        if sa[j] != EMPTY {
            w -= 1;
            sa[w] = sa[j];
        }
    }

    // 4. The LMS suffixes in order: already, when every name differs; else by recursion on
    //    the text of names (its suffixes sort as the LMS suffixes they start at).
    if names < m {
        drop(std::mem::take(&mut counts));
        drop(std::mem::take(&mut bkt));
        let (head, tail) = sa.split_at_mut(n - m);
        sais(&*tail, names, &mut head[..m]);
        let mut j = 0;
        for i in 1..n {
            if types.lms(i) {
                tail[j] = i as u32;
                j += 1;
            }
        }
        for x in &mut head[..m] {
            *x = tail[*x as usize];
        }
        counts = symbol_counts(s, k);
        bkt = vec![0u32; k];
    }

    // 5. The sorted LMS suffixes at the ends of their buckets, in order (each one moves to a
    //    slot at or after its own, so none is overwritten before it is read); then induced
    //    sorting places all the others.
    sa[m..].fill(EMPTY);
    bucket_ends(&counts, &mut bkt);
    for i in (0..m).rev() {
        let p = sa[i] as usize;
        sa[i] = EMPTY;
        let c = s[p].at();
        bkt[c] -= 1;
        sa[bkt[c] as usize] = p as u32;
    }
    induce(s, &types, &counts, &mut bkt, sa);
}

/// Induced sorting: from the LMS suffixes in `sa` (at the ends of their buckets), places
/// the L-type suffixes from the bucket starts, left to right (the suffix at n - 1 first: it
/// follows the end mark's), then the S-type suffixes from the bucket ends, right to left.
/// `bkt` is the bucket table, as long as `counts`.
fn induce<S: Sym>(s: &[S], types: &Types, counts: &[u32], bkt: &mut [u32], sa: &mut [u32]) {
    let n = s.len();
    bucket_starts(counts, bkt);
    let c = s[n - 1].at();
    sa[bkt[c] as usize] = (n - 1) as u32;
    bkt[c] += 1;
    for i in 0..n {
        let j = sa[i];
        if j != EMPTY && j > 0 && !types.get(j as usize - 1) {
            let p = j as usize - 1;
            let c = s[p].at();
            sa[bkt[c] as usize] = p as u32;
            bkt[c] += 1;
        }
    }
    bucket_ends(counts, bkt);
    for i in (0..n).rev() {
        let j = sa[i];
        if j != EMPTY && j > 0 && types.get(j as usize - 1) {
            let p = j as usize - 1;
            let c = s[p].at();
            bkt[c] -= 1;
            sa[bkt[c] as usize] = p as u32;
        }
    }
}

/// Whether the LMS substrings at `a` and `b` are equal: the same symbols and types up to
/// and including the next LMS place. One that reaches the end mark equals no other.
fn same_lms<S: Sym>(s: &[S], types: &Types, a: usize, b: usize) -> bool {
    let n = s.len();
    let mut d = 0;
    loop {
        if a + d == n || b + d == n {
            return false;
        }
        if s[a + d].at() != s[b + d].at() || types.get(a + d) != types.get(b + d) {
            return false;
        }
        // the types before were equal too, so b + d is an LMS place exactly when a + d is
        if d > 0 && types.lms(a + d) {
            return true;
        }
        d += 1;
    }
}

// ------------------------------------------------------------------------------------ BWT

/// The BWT of `text` from its suffix array: the byte before each row's suffix, in row order
/// (row 0 is the end mark's suffix, row r > 0 the suffix at `sa[r - 1]`). The row of the
/// whole text (place 0) holds the end mark, which is not a byte: it is left out, and its
/// row number is returned with the n bytes of the other rows.
pub fn bwt(text: &[u8], sa: &[u32]) -> (Vec<u8>, usize) {
    let n = text.len();
    assert_eq!(sa.len(), n, "a suffix array of another text");
    let mut out = Vec::with_capacity(n);
    let mut start_row = 0;
    if n > 0 {
        out.push(text[n - 1]);
    }
    for (i, &p) in sa.iter().enumerate() {
        if p == 0 {
            start_row = i + 1;
        } else {
            out.push(text[p as usize - 1]);
        }
    }
    (out, start_row)
}

// ------------------------------------------------------------------------- wavelet matrix

/// A sequence of codes below 2^levels (at most 8 levels), with access and rank in one
/// `Bits::rank1` per level.
///
/// Level 0 holds the top bit of each code, in the sequence's order. Then the codes are put
/// in a new order, those whose bit was 0 first, each group keeping its order, and level 1
/// holds their next bit; and so on down to the lowest bit. Following a code's bits through
/// the levels takes a position to its place at the next level, and the codes equal to c end
/// up as one run at the bottom, in their first order: `rank(c, i)` is where position i
/// lands, less where that run starts.
#[derive(Clone, Debug)]
pub struct WaveletMatrix {
    len: usize,
    levels: Vec<Bits>,
    /// The 0 bits of each level.
    zeros: Vec<usize>,
    /// Where each code's run starts at the bottom.
    start: Vec<usize>,
}

impl WaveletMatrix {
    /// The wavelet matrix of `codes`, which must be below 2^levels (`levels` at most 8).
    pub fn new(codes: &[u8], levels: u32) -> WaveletMatrix {
        WaveletMatrix::from_codes(codes.to_vec(), levels)
    }

    fn from_codes(mut cur: Vec<u8>, levels: u32) -> WaveletMatrix {
        assert!(levels <= 8, "a wavelet matrix of bytes has at most 8 levels");
        assert!(cur.iter().all(|&c| u32::from(c) < 1 << levels), "a code needs more than {levels} levels");
        let n = cur.len();
        let mut next = vec![0u8; n];
        let mut bits = Vec::with_capacity(levels as usize);
        let mut zeros = Vec::with_capacity(levels as usize);
        for d in 0..levels {
            let shift = levels - 1 - d;
            let mut words = vec![0u64; n.div_ceil(64)];
            let mut z = 0;
            for (i, &c) in cur.iter().enumerate() {
                if (c >> shift) & 1 == 1 {
                    words[i / 64] |= 1 << (i % 64);
                } else {
                    z += 1;
                }
            }
            let (mut a, mut b) = (0, z);
            for &c in &cur {
                if (c >> shift) & 1 == 0 {
                    next[a] = c;
                    a += 1;
                } else {
                    next[b] = c;
                    b += 1;
                }
            }
            std::mem::swap(&mut cur, &mut next);
            bits.push(Bits::from_words(words, n).expect("a wavelet matrix of at most u32::MAX codes"));
            zeros.push(z);
        }
        WaveletMatrix::from_levels(n, bits, zeros)
    }

    fn from_levels(len: usize, levels: Vec<Bits>, zeros: Vec<usize>) -> WaveletMatrix {
        let mut wm = WaveletMatrix { len, levels, zeros, start: Vec::new() };
        wm.start = (0..1usize << wm.levels.len()).map(|c| wm.walk(c, 0)).collect();
        wm
    }

    /// The number of codes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The number of levels (bits per code).
    pub fn levels(&self) -> usize {
        self.levels.len()
    }

    /// Where position `i` lands at the bottom, following code `c`'s bits.
    fn walk(&self, c: usize, mut i: usize) -> usize {
        let l = self.levels.len();
        for (d, bits) in self.levels.iter().enumerate() {
            i = if (c >> (l - 1 - d)) & 1 == 0 { bits.rank0(i) } else { self.zeros[d] + bits.rank1(i) };
        }
        i
    }

    /// The number of codes equal to `c` before position `i` (`i` past the end counts as the
    /// end).
    pub fn rank(&self, c: u8, i: usize) -> usize {
        let c = c as usize;
        if c >= self.start.len() {
            return 0;
        }
        self.walk(c, i.min(self.len)) - self.start[c]
    }

    /// The code at position `i` (below `len`), and the number of equal codes before it.
    pub fn access_rank(&self, mut i: usize) -> (u8, usize) {
        let mut c = 0;
        for (d, bits) in self.levels.iter().enumerate() {
            let bit = bits.get(i);
            c = (c << 1) | bit as usize;
            i = if bit { self.zeros[d] + bits.rank1(i) } else { bits.rank0(i) };
        }
        (c as u8, i - self.start[c])
    }

    /// The code at position `i` (below `len`).
    pub fn access(&self, i: usize) -> u8 {
        self.access_rank(i).0
    }
}

// ------------------------------------------------------------------------------- FM index

/// An FM index of a text of bytes: count and locate any piece (see the top of this file).
#[derive(Clone, Debug)]
pub struct Fm {
    /// The text's length: the rows are 0 to n.
    n: usize,
    /// The row of the whole text's suffix (place 0). Its BWT entry is the end mark, which the
    /// wavelet matrix leaves out: row r is position r of the matrix before this row, r - 1
    /// after it.
    start_row: usize,
    /// One place in `rate` is kept for locate.
    rate: u32,
    /// The bytes the text holds, in byte order: byte `symbols[c]` has code c.
    symbols: Vec<u8>,
    /// Each byte's code, or `NO_CODE`.
    code: [u16; 256],
    /// `first[c]`: the first row whose suffix starts with code c (row 0 is the end mark's);
    /// `first[symbols.len()]` is n + 1.
    first: Vec<usize>,
    /// The BWT's codes, in row order, without the start row's end mark.
    wm: WaveletMatrix,
    /// The rows whose place is a multiple of `rate` (no bits when no places are kept).
    marks: Bits,
    /// Those rows' places, in row order (none when no places are kept).
    places: Vec<u32>,
}

/// The number of levels for codes 0 to `symbols - 1`.
fn levels_for(symbols: usize) -> u32 {
    if symbols <= 1 { 0 } else { usize::BITS - (symbols - 1).leading_zeros() }
}

fn code_table(symbols: &[u8]) -> [u16; 256] {
    let mut code = [NO_CODE; 256];
    for (c, &b) in symbols.iter().enumerate() {
        code[b as usize] = c as u16;
    }
    code
}

/// `first` from the counts of the codes: row 0 is the end mark's, then each code's rows.
fn first_rows(counts: &[usize]) -> Vec<usize> {
    let mut first = Vec::with_capacity(counts.len() + 1);
    let mut row = 1;
    for &k in counts {
        first.push(row);
        row += k;
    }
    first.push(row);
    first
}

impl Fm {
    /// The FM index of `text`, keeping one place in `SAMPLE` (32) for locate. An error when
    /// the text is longer than `MAX_LEN`.
    pub fn new(text: &[u8]) -> Result<Fm, String> {
        Fm::with_rate(text, SAMPLE)
    }

    /// The FM index of `text`, keeping one place in `rate` (1 to `MAX_RATE`) for locate: a
    /// higher rate makes the index smaller and locate slower (up to rate - 1 steps).
    pub fn with_rate(text: &[u8], rate: u32) -> Result<Fm, String> {
        if text.len() > MAX_LEN {
            return Err(format!("a text of {} bytes is too long for an FM index (at most {MAX_LEN})", text.len()));
        }
        if rate == 0 || rate > MAX_RATE {
            return Err(format!("sample rate {rate}: it must be 1 to {MAX_RATE}"));
        }
        Fm::build(text, rate)
    }

    /// The FM index of `text` with no places kept (see the top of this file): it counts and
    /// steps back as any index, and `locate` gives None. An error when the text is longer than
    /// `MAX_LEN`.
    pub fn no_places(text: &[u8]) -> Result<Fm, String> {
        if text.len() > MAX_LEN {
            return Err(format!("a text of {} bytes is too long for an FM index (at most {MAX_LEN})", text.len()));
        }
        Fm::build(text, 0)
    }

    /// The index of `text`, keeping one place in `rate` (0: none).
    fn build(text: &[u8], rate: u32) -> Result<Fm, String> {
        let n = text.len();
        let mut seen = [0usize; 256];
        for &b in text {
            seen[b as usize] += 1;
        }
        let symbols: Vec<u8> = (0..=255u8).filter(|&b| seen[b as usize] > 0).collect();
        let counts: Vec<usize> = symbols.iter().map(|&b| seen[b as usize]).collect();
        let code = code_table(&symbols);

        let sa = suffix_array(text);
        // (no places kept: no marks either)
        let rows = if rate == 0 { 0 } else { n + 1 };
        let mut marks = vec![0u64; rows.div_ceil(64)];
        let mut places = Vec::with_capacity(if rate == 0 { 0 } else { n / rate as usize + 1 });
        for row in 0..rows {
            let place = if row == 0 { n } else { sa[row - 1] as usize };
            if place.is_multiple_of(rate as usize) {
                marks[row / 64] |= 1 << (row % 64);
                places.push(place as u32);
            }
        }
        let (mut codes, start_row) = bwt(text, &sa);
        drop(sa);
        for b in codes.iter_mut() {
            *b = code[*b as usize] as u8;
        }
        let wm = WaveletMatrix::from_codes(codes, levels_for(symbols.len()));
        let marks = Bits::from_words(marks, rows).expect("one bit per row");
        Ok(Fm { n, start_row, rate, first: first_rows(&counts), symbols, code, wm, marks, places })
    }

    /// The length of the indexed text.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// One place in `rate` is kept for locate (0: no places kept).
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The row of the whole text's suffix (place 0): LF steps back end there, since no byte
    /// comes before it (`back` gives None).
    pub fn text_row(&self) -> usize {
        self.start_row
    }

    /// The bytes the text holds, in byte order.
    pub fn symbols(&self) -> &[u8] {
        &self.symbols
    }

    /// The wavelet matrix's levels: the bits of a code (6 for the DNA index's 38 symbols).
    pub fn levels(&self) -> usize {
        self.wm.levels()
    }

    /// The number of rows before `row` whose BWT byte has code `c`.
    fn occ(&self, c: usize, row: usize) -> usize {
        self.wm.rank(c as u8, row - usize::from(row > self.start_row))
    }

    /// The rows whose suffixes start with `pattern`, found by backward search: one row per
    /// place where the pattern occurs (`locate` gives the place). Empty (0..0) when it does
    /// not occur; all n + 1 rows for the empty pattern.
    pub fn rows(&self, pattern: &[u8]) -> Range<usize> {
        let (mut sp, mut ep) = (0, self.n + 1);
        for &b in pattern.iter().rev() {
            let c = self.code[b as usize];
            if c == NO_CODE {
                return 0..0;
            }
            let c = c as usize;
            sp = self.first[c] + self.occ(c, sp);
            ep = self.first[c] + self.occ(c, ep);
            if sp >= ep {
                return 0..0;
            }
        }
        sp..ep
    }

    /// The number of places where `pattern` occurs (overlapping ones too), in two ranks per
    /// byte of the pattern.
    pub fn count(&self, pattern: &[u8]) -> usize {
        self.rows(pattern).len()
    }

    /// One LF step: the byte before row `row`'s suffix, and the row of the suffix that
    /// starts at that byte. None for the row of the whole text (place 0: nothing comes
    /// before it) and for rows past the last (n).
    pub fn back(&self, row: usize) -> Option<(u8, usize)> {
        if row > self.n || row == self.start_row {
            return None;
        }
        let (c, r) = self.wm.access_rank(row - usize::from(row > self.start_row));
        let c = c as usize;
        Some((*self.symbols.get(c)?, *self.first.get(c)? + r))
    }

    /// The place of row `row`'s suffix in the text (row 0 is the end mark's, at place n):
    /// LF steps to a row whose place is kept, at most rate - 1 of them. None for a row past
    /// the last, for an index that keeps no places, or when the index is damaged (no kept
    /// place within rate steps).
    pub fn locate(&self, row: usize) -> Option<usize> {
        if row > self.n || self.rate == 0 {
            return None;
        }
        let mut r = row;
        let mut steps = 0;
        loop {
            if self.marks.get(r) {
                let place = *self.places.get(self.marks.rank1(r))? as usize + steps;
                return (place <= self.n).then_some(place);
            }
            if steps + 1 >= self.rate as usize {
                return None;
            }
            r = self.back(r)?.1;
            steps += 1;
        }
    }

    /// Every place where `pattern` occurs, in increasing order. None only when the index is
    /// damaged or keeps no places (and the pattern occurs).
    pub fn positions(&self, pattern: &[u8]) -> Option<Vec<usize>> {
        let mut out = Vec::new();
        for row in self.rows(pattern) {
            out.push(self.locate(row)?);
        }
        out.sort_unstable();
        Some(out)
    }

    /// The number of bytes `write` adds.
    pub fn file_len(&self) -> usize {
        let n = self.n;
        // (an index that keeps no places has neither marks nor places)
        19 + 5 * self.symbols.len()
            + self.levels() * (4 + 8 * n.div_ceil(64))
            + 8 * self.marks.words().len()
            + 4 * self.places.len()
    }

    /// Adds the stored form (see the top of this file) to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.reserve(self.file_len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&(self.n as u32).to_le_bytes());
        out.extend_from_slice(&(self.start_row as u32).to_le_bytes());
        out.extend_from_slice(&self.rate.to_le_bytes());
        out.extend_from_slice(&(self.symbols.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.symbols);
        for c in 0..self.symbols.len() {
            out.extend_from_slice(&((self.first[c + 1] - self.first[c]) as u32).to_le_bytes());
        }
        for (bits, &z) in self.wm.levels.iter().zip(&self.wm.zeros) {
            out.extend_from_slice(&(z as u32).to_le_bytes());
            put_words(out, bits.words());
        }
        put_words(out, self.marks.words());
        for &p in &self.places {
            out.extend_from_slice(&p.to_le_bytes());
        }
    }

    /// An index in its stored form at the start of `bytes`, and the number of bytes it takes
    /// (what follows is left for the caller). An error when the bytes are not an index of
    /// this version, are cut short, or their parts do not fit together.
    pub fn read(bytes: &[u8]) -> Result<(Fm, usize), String> {
        let bad = |what: &str| format!("damaged FM index: {what}");
        let mut r = Input { bytes, at: 0 };
        if r.take(4)? != MAGIC {
            return Err(bad("no CMFM mark"));
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(format!("FM index version {version}: this program reads version {VERSION}"));
        }
        let n = r.u32()? as usize;
        let start_row = r.u32()? as usize;
        let rate = r.u32()?;
        let sigma = r.u16()? as usize;
        // (rate 0: no places kept)
        if n > MAX_LEN || rate > MAX_RATE || sigma > 256 || (sigma == 0) != (n == 0) {
            return Err(bad("its sizes"));
        }
        // place 0 is a row of its own, after row 0 (the end mark's) unless the text is empty
        if start_row > n || (n > 0 && start_row == 0) {
            return Err(bad("the row of the whole text"));
        }
        let symbols = r.take(sigma)?.to_vec();
        if symbols.windows(2).any(|w| w[0] >= w[1]) {
            return Err(bad("its symbols"));
        }
        let mut counts = Vec::with_capacity(sigma);
        for _ in 0..sigma {
            counts.push(r.u32()? as usize);
        }
        if counts.contains(&0) || counts.iter().sum::<usize>() != n {
            return Err(bad("its counts"));
        }
        let mut levels = Vec::new();
        let mut zeros = Vec::new();
        for _ in 0..levels_for(sigma) {
            let z = r.u32()? as usize;
            let bits = Bits::from_words(r.words(n.div_ceil(64))?, n).ok_or_else(|| bad("a level's last word"))?;
            if z > n || bits.ones() != n - z {
                return Err(bad("a level's zeros"));
            }
            levels.push(bits);
            zeros.push(z);
        }
        let wm = WaveletMatrix::from_levels(n, levels, zeros);
        // the marks and the kept places (none when no places are kept)
        let rows = if rate == 0 { 0 } else { n + 1 };
        let marks = Bits::from_words(r.words(rows.div_ceil(64))?, rows).ok_or_else(|| bad("the marks' last word"))?;
        let kept = if rate == 0 { 0 } else { n / rate as usize + 1 };
        if marks.ones() != kept {
            return Err(bad("the marks"));
        }
        let places: Vec<u32> = r.take(4 * kept)?.as_chunks::<4>().0.iter().map(|p| u32::from_le_bytes(*p)).collect();
        if places.iter().any(|&p| p as usize > n || !p.is_multiple_of(rate)) {
            return Err(bad("the kept places"));
        }
        // the matrix holds each code as often as the counts say: so every code is a symbol,
        // and backward search and LF steps stay within the rows
        if counts.iter().enumerate().any(|(c, &k)| wm.rank(c as u8, n) != k) {
            return Err(bad("the BWT does not fit its counts"));
        }
        let code = code_table(&symbols);
        let fm = Fm { n, start_row, rate, first: first_rows(&counts), symbols, code, wm, marks, places };
        Ok((fm, r.at))
    }
}

fn put_words(out: &mut Vec<u8>, words: &[u64]) {
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
}

/// Reads the stored form from the front.
struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn take(&mut self, k: usize) -> Result<&'a [u8], String> {
        if self.bytes.len() - self.at < k {
            return Err("damaged FM index: cut short".to_string());
        }
        let s = &self.bytes[self.at..self.at + k];
        self.at += k;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn words(&mut self, k: usize) -> Result<Vec<u64>, String> {
        Ok(self.take(k * 8)?.as_chunks::<8>().0.iter().map(|w| u64::from_le_bytes(*w)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// xorshift64*: a small fixed-seed generator, so every run is the same.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn random_text(r: &mut Rng, alphabet: &[u8], n: usize) -> Vec<u8> {
        (0..n).map(|_| alphabet[r.below(alphabet.len())]).collect()
    }

    /// The suffix array by sorting the suffixes themselves.
    fn plain_sa(text: &[u8]) -> Vec<u32> {
        let mut sa: Vec<u32> = (0..text.len() as u32).collect();
        sa.sort_by(|&a, &b| text[a as usize..].cmp(&text[b as usize..]));
        sa
    }

    /// Burkhardt and Kärkkäinen's check, in linear time: `sa` holds every place once, and
    /// each two neighbours are in order by their first byte, then by the order of the
    /// suffixes one byte later (the empty suffix first). This is enough for `sa` to be the
    /// suffix array.
    fn right_sa(text: &[u8], sa: &[u32]) -> bool {
        let n = text.len();
        if sa.len() != n {
            return false;
        }
        let mut rank = vec![usize::MAX; n + 1];
        rank[n] = 0;
        for (i, &p) in sa.iter().enumerate() {
            let p = p as usize;
            if p >= n || rank[p] != usize::MAX {
                return false;
            }
            rank[p] = i + 1;
        }
        sa.windows(2).all(|w| {
            let (a, b) = (w[0] as usize, w[1] as usize);
            (text[a], rank[a + 1]) < (text[b], rank[b + 1])
        })
    }

    /// Every place where `pattern` occurs, by looking at each place.
    fn plain(text: &[u8], pattern: &[u8]) -> Vec<usize> {
        if pattern.len() > text.len() {
            return Vec::new();
        }
        (0..=text.len() - pattern.len()).filter(|&i| &text[i..i + pattern.len()] == pattern).collect()
    }

    fn fibonacci(n: usize) -> Vec<u8> {
        let (mut a, mut b) = (b"b".to_vec(), b"a".to_vec());
        while b.len() < n {
            let c = [b.as_slice(), a.as_slice()].concat();
            a = b;
            b = c;
        }
        b.truncate(n);
        b
    }

    fn thue_morse(n: usize) -> Vec<u8> {
        (0..n).map(|i: usize| if i.count_ones().is_multiple_of(2) { b'a' } else { b'b' }).collect()
    }

    /// Texts that are hard for suffix sorting: long repeats, runs, many recursion levels.
    fn special_texts() -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = [&b"banana"[..], b"mississippi", b"abracadabra", b"aaaaaaaaaab", b"baaaaaaaaaa", b"abababababa", b"\x01\x01\x01", b"\xff\x00\xff\x00\xff"]
            .iter()
            .map(|t| t.to_vec())
            .collect();
        for n in [2, 3, 5, 8, 13, 21, 34, 55, 100, 233, 610] {
            out.push(fibonacci(n));
            out.push(thue_morse(n));
            out.push(vec![b'x'; n]);
            out.push(b"ab".iter().cycle().take(n).copied().collect());
            out.push(b"aab".iter().cycle().take(n).copied().collect());
            out.push(b"dcba".iter().cycle().take(n).copied().collect());
            out.push(b"abcabd".iter().cycle().take(n).copied().collect());
        }
        out.push((0..=255u8).collect());
        out.push((0..=255u8).rev().collect());
        out.push((0..=255u8).chain(0..=255u8).collect());
        out
    }

    /// The alphabets of the random texts: 0x01 (the DNA index's separator), 0x00, high bytes,
    /// one symbol, and all 256 bytes.
    const ALPHABETS: &[&[u8]] = &[
        b"\x01ab",
        b"\x00\x01\x02\x03",
        b"\x01abc7\x80\xc3\xa9\xff",
        b"a",
        b"\x01abcdefghijklmnopqrstuvwxyz0123456789\x80",
    ];

    fn all_bytes() -> Vec<u8> {
        (0..=255u8).collect()
    }

    #[test]
    fn bit_tables_rank_like_counting() {
        let mut r = Rng(7);
        for len in [0usize, 1, 2, 63, 64, 65, 127, 128, 129, 511, 512, 513, 1023, 1024, 1025, 4097] {
            for density in [0, 1, 2, 4] {
                let bits: Vec<bool> = (0..len).map(|_| density > 0 && r.below(density) == 0).collect();
                let mut words = vec![0u64; len.div_ceil(64)];
                for (i, &b) in bits.iter().enumerate() {
                    if b {
                        words[i / 64] |= 1 << (i % 64);
                    }
                }
                let t = Bits::from_words(words, len).unwrap();
                assert_eq!(t.len(), len);
                let mut ones = 0;
                for (i, &b) in bits.iter().enumerate() {
                    assert_eq!(t.rank1(i), ones, "rank1({i}) of {len} bits");
                    assert_eq!(t.rank0(i), i - ones);
                    assert_eq!(t.get(i), b);
                    ones += usize::from(b);
                }
                assert_eq!((t.rank1(len), t.ones()), (ones, ones));
            }
        }
    }

    #[test]
    fn bit_tables_refuse_wrong_words() {
        assert!(Bits::from_words(vec![], 0).is_some());
        assert!(Bits::from_words(vec![0], 0).is_none());
        assert!(Bits::from_words(vec![], 1).is_none());
        assert!(Bits::from_words(vec![1], 1).is_some());
        assert!(Bits::from_words(vec![2], 1).is_none(), "a bit past the end");
        assert!(Bits::from_words(vec![u64::MAX], 64).is_some());
        assert!(Bits::from_words(vec![u64::MAX, 0], 64).is_none());
        assert!(Bits::from_words(vec![0, 1 << 5], 70).is_some());
        assert!(Bits::from_words(vec![0, 1 << 6], 70).is_none());
    }

    #[test]
    fn suffix_arrays_equal_a_plain_sort() {
        let mut r = Rng(1);
        let all = all_bytes();
        let mut alphabets: Vec<&[u8]> = ALPHABETS.to_vec();
        alphabets.push(b"ab");
        alphabets.push(b"abc");
        alphabets.push(&all);
        for alphabet in alphabets {
            for n in 0..=70 {
                for _ in 0..12 {
                    let t = random_text(&mut r, alphabet, n);
                    assert_eq!(suffix_array(&t), plain_sa(&t), "text {t:?}");
                }
            }
            for n in [100, 255, 256, 257, 1000, 3000] {
                let t = random_text(&mut r, alphabet, n);
                assert_eq!(suffix_array(&t), plain_sa(&t), "a random text of {n} bytes");
            }
        }
        for t in special_texts() {
            assert_eq!(suffix_array(&t), plain_sa(&t), "text {t:?}");
        }
    }

    #[test]
    fn long_suffix_arrays_pass_the_linear_check() {
        let mut r = Rng(2);
        let mut texts = vec![
            random_text(&mut r, b"ab", 200_000),
            random_text(&mut r, b"\x01abcdefghijklmnopqrstuvwxyz0123456789\x80", 200_000),
            random_text(&mut r, &all_bytes(), 100_000),
            fibonacci(150_000),
            thue_morse(150_000),
            vec![0u8; 100_000],
            b"abcab".iter().cycle().take(100_001).copied().collect(),
        ];
        // a log-like text: lines that repeat with small changes
        let mut log = Vec::new();
        for i in 0..20_000 {
            log.extend_from_slice(format!("2026-11-18 10:{:02}:{:02} INFO block blk_{} served to /10.0.{}.{}\n", i % 60, i % 37, i * 7919 % 100_003, i % 13, i % 251).as_bytes());
        }
        texts.push(log);
        for t in &texts {
            let sa = suffix_array(t);
            assert!(right_sa(t, &sa), "a text of {} bytes", t.len());
        }
        // the check itself catches a wrong order
        let t = &texts[0];
        let mut sa = suffix_array(t);
        sa.swap(10, 11);
        assert!(!right_sa(t, &sa));
    }

    #[test]
    fn the_bwt_is_the_byte_before_each_sorted_suffix() {
        let mut r = Rng(3);
        for n in 0..60 {
            let t = random_text(&mut r, b"\x00\x01ab\xff", n);
            let (b, start_row) = bwt(&t, &suffix_array(&t));
            // rows: the end mark alone, then the suffixes in order; the byte before each, with
            // None for the end mark before the whole text
            let mut rows = vec![n];
            rows.extend(plain_sa(&t).iter().map(|&p| p as usize));
            let want: Vec<Option<u8>> = rows.iter().map(|&p| if p == 0 { None } else { Some(t[p - 1]) }).collect();
            let mut got: Vec<Option<u8>> = b.iter().map(|&x| Some(x)).collect();
            got.insert(start_row, None);
            assert_eq!(got, want, "text {t:?}");
        }
        assert_eq!(bwt(b"banana", &suffix_array(b"banana")), (b"annbaa".to_vec(), 4));
    }

    #[test]
    fn wavelet_matrix_access_and_rank() {
        let mut r = Rng(4);
        for levels in 0..=8u32 {
            for len in [0, 1, 2, 63, 64, 65, 511, 512, 513, 1500] {
                let top = 1usize << levels;
                // a few codes only, sometimes, so that some codes are missing
                let used = if r.below(2) == 0 { top } else { 1 + r.below(top.min(5)) };
                let codes: Vec<u8> = (0..len).map(|_| r.below(used) as u8).collect();
                let wm = WaveletMatrix::new(&codes, levels);
                assert_eq!((wm.len(), wm.levels()), (len, levels as usize));
                let mut seen = vec![0usize; top];
                for (i, &c) in codes.iter().enumerate() {
                    assert_eq!(wm.access_rank(i), (c, seen[c as usize]), "position {i}");
                    assert_eq!(wm.access(i), c);
                    if i % 7 == 0 || len < 100 {
                        for (x, &k) in seen.iter().enumerate() {
                            assert_eq!(wm.rank(x as u8, i), k, "rank of {x} at {i}");
                        }
                    }
                    seen[c as usize] += 1;
                }
                for (x, &k) in seen.iter().enumerate() {
                    assert_eq!(wm.rank(x as u8, len), k);
                    assert_eq!(wm.rank(x as u8, len + 5), k, "past the end counts as the end");
                }
                if levels < 8 {
                    assert_eq!(wm.rank(top as u8, len), 0, "a code above the levels");
                }
            }
        }
    }

    /// Checks `fm`'s count and places for `pattern` against a plain search of `text`.
    fn check(fm: &Fm, text: &[u8], pattern: &[u8]) {
        let want = plain(text, pattern);
        assert_eq!(fm.count(pattern), want.len(), "count of {pattern:?} in a text of {} bytes", text.len());
        assert_eq!(fm.positions(pattern), Some(want), "places of {pattern:?} in a text of {} bytes", text.len());
    }

    /// Pieces of `text` and random patterns over `alphabet`, with the empty pattern, the
    /// text itself and the text with a byte more.
    fn patterns(r: &mut Rng, text: &[u8], alphabet: &[u8], k: usize) -> Vec<Vec<u8>> {
        let mut out = vec![Vec::new(), text.to_vec(), [text, &alphabet[..1]].concat()];
        if let Some(missing) = (0..=255u8).find(|b| !alphabet.contains(b)) {
            out.push(vec![missing]);
            if !text.is_empty() {
                out.push([&text[..1], &[missing]].concat());
            }
        }
        for _ in 0..k {
            if !text.is_empty() && r.below(3) > 0 {
                let s = r.below(text.len());
                let len = 1 + r.below((text.len() - s).min(12));
                out.push(text[s..s + len].to_vec());
            } else {
                let len = 1 + r.below(5);
                out.push(random_text(r, alphabet, len));
            }
        }
        out
    }

    #[test]
    fn counts_and_places_equal_a_plain_search() {
        let mut r = Rng(5);
        let all = all_bytes();
        let mut alphabets: Vec<&[u8]> = ALPHABETS.to_vec();
        alphabets.push(&all);
        for alphabet in alphabets {
            let mut lengths: Vec<usize> = (0..=48).collect();
            lengths.extend([64, 65, 100, 513, 1000, 4097]);
            for n in lengths {
                let text = random_text(&mut r, alphabet, n);
                for rate in [1, 2, 3, 7, 32] {
                    let fm = Fm::with_rate(&text, rate).unwrap();
                    assert_eq!(fm.len(), n);
                    let k = if n > 500 { 60 } else { 15 };
                    for p in patterns(&mut r, &text, alphabet, k) {
                        check(&fm, &text, &p);
                    }
                }
            }
        }
    }

    #[test]
    fn edge_cases() {
        // an empty text: only the empty pattern, once, at place 0
        let fm = Fm::new(b"").unwrap();
        assert!(fm.is_empty());
        assert_eq!((fm.count(b""), fm.positions(b"")), (1, Some(vec![0])));
        for p in [&b"a"[..], b"\x01", b"\x00", b"\xff", b"abc"] {
            assert_eq!((fm.count(p), fm.positions(p)), (0, Some(vec![])));
        }
        assert_eq!((fm.levels(), fm.symbols()), (0, &[][..]));
        assert_eq!((fm.back(0), fm.locate(0), fm.locate(1)), (None, Some(0), None));

        // one byte, each kind
        for b in [0x00, 0x01, b'a', 0x80, 0xff] {
            let fm = Fm::new(&[b]).unwrap();
            assert_eq!(fm.positions(&[b]), Some(vec![0]));
            assert_eq!(fm.positions(b""), Some(vec![0, 1]));
            assert_eq!(fm.count(&[b, b]), 0, "a pattern longer than the text");
            assert_eq!(fm.count(&[b ^ 1]), 0);
            assert_eq!(fm.levels(), 0);
        }

        // a pattern longer than the text, whose start is the whole text
        let fm = Fm::new(b"abc").unwrap();
        assert_eq!(fm.count(b"abcd"), 0);
        assert_eq!(fm.count(b"abcabc"), 0);
        assert_eq!(fm.count(b"zabc"), 0);
        check(&fm, b"abc", b"abc");

        // repeated bytes: overlapping places, n - k + 1 of them
        for byte in [b'a', 0x01, 0x00, 0xff] {
            for n in [1, 2, 31, 32, 33, 64, 1000] {
                let text = vec![byte; n];
                for rate in [1, 3, 32] {
                    let fm = Fm::with_rate(&text, rate).unwrap();
                    for k in [1, 2, 3, n / 2, n - 1, n, n + 1] {
                        let p = vec![byte; k];
                        let want: Vec<usize> = if k <= n { (0..=n - k).collect() } else { vec![] };
                        assert_eq!(fm.count(&p), want.len(), "{k} of {n} {byte:#x}");
                        assert_eq!(fm.positions(&p), Some(want));
                    }
                }
            }
        }

        // two symbols, repeated in runs
        let text: Vec<u8> = b"aaaabbbbaaaabbbb\x01".repeat(40);
        let fm = Fm::with_rate(&text, 5).unwrap();
        for p in [&b"ab"[..], b"ba", b"aaaa", b"bbbb\x01a", b"\x01", b"aaaaa", b"b\x01aaaab"] {
            check(&fm, &text, p);
        }

        // too long a text, and wrong rates
        assert!(Fm::with_rate(b"abc", 0).is_err());
        assert!(Fm::with_rate(b"abc", MAX_RATE + 1).is_err());
        assert!(Fm::with_rate(b"abc", MAX_RATE).is_ok());
    }

    #[test]
    fn levels_follow_the_alphabet() {
        assert_eq!([0, 1, 2, 3, 4, 5, 37, 38, 64, 65, 256].map(levels_for), [0, 0, 1, 2, 2, 3, 6, 6, 6, 7, 8]);
        let dna = b"\x01abcdefghijklmnopqrstuvwxyz0123456789\x80";
        assert_eq!(Fm::new(dna).unwrap().levels(), 6, "the DNA index's 38 symbols");
        assert_eq!(Fm::new(&all_bytes()).unwrap().levels(), 8);
    }

    #[test]
    fn back_steps_read_the_text_backwards() {
        let mut r = Rng(6);
        for alphabet in ALPHABETS {
            for n in [0, 1, 2, 3, 10, 100, 1000] {
                let text = random_text(&mut r, alphabet, n);
                let fm = Fm::new(&text).unwrap();
                // from the end mark's row (place n), each LF step gives the byte before
                let mut row = 0;
                let mut got = Vec::new();
                while let Some((b, next)) = fm.back(row) {
                    got.push(b);
                    row = next;
                    assert!(got.len() <= n);
                }
                got.reverse();
                assert_eq!(got, text);
                assert_eq!(fm.locate(row), Some(0), "the walk ends at the whole text's row");
                assert_eq!(fm.back(n + 1), None);
                // and every row's place, through the walk, equals locate's
                let mut places = vec![usize::MAX; n + 1];
                let mut row = 0;
                for p in (0..=n).rev() {
                    places[row] = p;
                    row = fm.back(row).map_or(row, |x| x.1);
                }
                for (row, &p) in places.iter().enumerate() {
                    assert_eq!(fm.locate(row), Some(p));
                }
            }
        }
    }

    #[test]
    fn stored_index_gives_the_same_answers() {
        let mut r = Rng(8);
        for alphabet in ALPHABETS {
            for n in [0, 1, 2, 63, 64, 65, 300, 2000] {
                let text = random_text(&mut r, alphabet, n);
                for rate in [1, 5, 32] {
                    let fm = Fm::with_rate(&text, rate).unwrap();
                    let mut stored = b"before".to_vec();
                    fm.write(&mut stored);
                    assert_eq!(stored.len(), 6 + fm.file_len());
                    stored.extend_from_slice(b"after");
                    let (back, used) = Fm::read(&stored[6..]).unwrap();
                    assert_eq!(used, fm.file_len());
                    assert_eq!(&stored[6 + used..], b"after");
                    assert_eq!((back.len(), back.rate(), back.levels(), back.symbols()), (n, rate, fm.levels(), fm.symbols()));
                    for p in patterns(&mut r, &text, alphabet, 20) {
                        check(&back, &text, &p);
                    }
                    let mut again = Vec::new();
                    back.write(&mut again);
                    assert_eq!(again, stored[6..6 + used], "written again, the same bytes");
                }
            }
        }
        // two indexes one after the other, as a file of segments holds them
        let (a, b) = (Fm::new(b"first text").unwrap(), Fm::new(b"\x01second\x01").unwrap());
        let mut both = Vec::new();
        a.write(&mut both);
        b.write(&mut both);
        let (a2, used) = Fm::read(&both).unwrap();
        let (b2, used2) = Fm::read(&both[used..]).unwrap();
        assert_eq!(used + used2, both.len());
        assert_eq!((a2.count(b"t"), b2.count(b"\x01")), (3, 2));
    }

    #[test]
    fn damaged_index_gives_an_error_or_safe_answers() {
        let mut r = Rng(9);
        let text = random_text(&mut r, b"\x01abcde\x80\xff", 700);
        let fm = Fm::with_rate(&text, 4).unwrap();
        let mut good = Vec::new();
        fm.write(&mut good);
        for cut in 0..good.len() {
            assert!(Fm::read(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut version = good.clone();
        version[4] = 2;
        assert!(Fm::read(&version).unwrap_err().contains("version 2"));
        let pieces: Vec<Vec<u8>> = patterns(&mut r, &text, b"\x01abcde\x80\xff", 30);
        let mut read_ok = 0;
        for round in 0..4000 {
            let mut bad = good.clone();
            for _ in 0..1 + round % 3 {
                let at = r.below(bad.len());
                bad[at] ^= 1 << r.below(8);
            }
            if let Ok((damaged, _)) = Fm::read(&bad) {
                read_ok += 1;
                // any answer will do, as long as none panics or loops
                for p in &pieces {
                    let rows = damaged.rows(p);
                    assert!(rows.end <= damaged.len() + 1);
                    let _ = damaged.positions(p);
                }
                for row in 0..=damaged.len() + 1 {
                    let _ = (damaged.back(row), damaged.locate(row));
                }
            }
        }
        // some changes pass the checks (a flipped place in a kept sample, say): they must still be safe
        assert!(read_ok > 0);
    }

    /// The DNA index's use: a text of distinct tokens, sorted, each
    /// followed by 0x01. The tokens holding a piece are found from its places (each place's
    /// token by the token starts), or with no kept places at all, by LF steps back to the
    /// 0x01 before the place: the rows of 0x01 suffixes are in token order, so that row
    /// gives the token's number.
    #[test]
    fn token_numbers_by_locate_and_by_walking_back() {
        let mut r = Rng(10);
        let letters = b"abcdefghijklmnopqrstuvwxyz0123456789\x80";
        for round in 0..30 {
            let mut set = BTreeSet::new();
            for _ in 0..1 + r.below(300) {
                let len = 1 + r.below(if round % 2 == 0 { 6 } else { 20 });
                let used = 2 + r.below(letters.len() - 1);
                set.insert(random_text(&mut r, &letters[..used], len));
            }
            let tokens: Vec<Vec<u8>> = set.into_iter().collect();
            let mut text = Vec::new();
            let mut starts = Vec::new();
            for t in &tokens {
                starts.push(text.len());
                text.extend_from_slice(t);
                text.push(0x01);
            }
            let fm = Fm::new(&text).unwrap();
            let sep_rows = fm.rows(b"\x01");
            assert_eq!(sep_rows.len(), tokens.len());
            for _ in 0..40 {
                let t = &tokens[r.below(tokens.len())];
                let s = r.below(t.len());
                let piece = &t[s..s + 1 + r.below(t.len() - s)];
                let want: BTreeSet<usize> = tokens.iter().enumerate().filter(|(_, t)| !plain(t, piece).is_empty()).map(|(k, _)| k).collect();
                let rows = fm.rows(piece);
                assert_eq!(rows.len(), plain(&text, piece).len());
                let by_place: BTreeSet<usize> = rows.clone().map(|row| starts.partition_point(|&s| s <= fm.locate(row).unwrap()) - 1).collect();
                let by_walk: BTreeSet<usize> = rows
                    .map(|mut row| loop {
                        match fm.back(row) {
                            None => break 0,
                            Some((0x01, sep)) => break sep - sep_rows.start,
                            Some((_, next)) => row = next,
                        }
                    })
                    .collect();
                assert_eq!(by_place, want, "piece {piece:?}");
                assert_eq!(by_walk, want, "piece {piece:?}");
            }
        }
    }

    /// The DNA index's text of `tokens` (distinct and sorted): each token followed by 0x01;
    /// and each token's place in it.
    fn token_text(tokens: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
        let mut text = Vec::new();
        let mut starts = Vec::new();
        for t in tokens {
            starts.push(text.len());
            text.extend_from_slice(t);
            text.push(0x01);
        }
        (text, starts)
    }

    /// The number of the token holding row `row`'s place, by LF steps back to the 0x01 before
    /// it, with no place kept: the rows of the suffixes that start with 0x01 are in token
    /// order (the tokens are distinct and sorted, and 0x01 is below every token byte), so the
    /// row the 0x01 step lands on, less the first of those rows (`sep_first`), is the token's
    /// number. The first token has no 0x01 before it: the walk ends at `text_row`.
    fn walk_to_token(fm: &Fm, sep_first: usize, mut row: usize) -> usize {
        loop {
            match fm.back(row) {
                None => {
                    assert_eq!(row, fm.text_row(), "a walk ends only at the whole text's row");
                    return 0;
                }
                Some((0x01, sep)) => return sep - sep_first,
                Some((_, next)) => row = next,
            }
        }
    }

    /// With no places kept (the DNA index's form): counts equal a plain search and an index
    /// with places, `locate` and `positions` give None, and the token found by stepping back
    /// from each place equals the token a plain scan finds there. Random sets of distinct
    /// tokens over letters, digits and high bytes (0x80, 0xc3, 0xa9, 0xff), sorted, each
    /// followed by 0x01 (one to a few tokens too); pieces of random tokens, the first and the
    /// last token whole and in part, the text's first and last bytes (the last token and the
    /// 0x01 that ends the text), pieces holding 0x01, one-byte tokens, and absent pieces.
    #[test]
    fn no_places_counts_and_tokens_by_walking_back() {
        let mut r = Rng(12);
        let letters = b"abcdefghijklmnopqrstuvwxyz0123456789\x80\xc3\xa9\xff";
        let mut walked = 0;
        for round in 0..80 {
            let mut set = BTreeSet::new();
            let k = if round < 6 { 1 + round } else { 1 + r.below(400) };
            for _ in 0..k {
                let len = 1 + r.below(if round % 3 == 0 { 3 } else { 24 });
                let used = 2 + r.below(letters.len() - 1);
                set.insert(random_text(&mut r, &letters[..used], len));
            }
            let tokens: Vec<Vec<u8>> = set.into_iter().collect();
            let (text, starts) = token_text(&tokens);
            let fm = Fm::no_places(&text).unwrap();
            let with = Fm::new(&text).unwrap();
            assert_eq!((fm.rate(), fm.len(), fm.levels()), (0, text.len(), with.levels()));
            let sep = fm.rows(b"\x01");
            assert_eq!(sep.len(), tokens.len());
            let (first, last) = (&tokens[0], &tokens[tokens.len() - 1]);
            let mut ps: Vec<Vec<u8>> = vec![
                first.clone(),
                last.clone(),
                first[..1].to_vec(),
                last[last.len() - 1..].to_vec(),
                [last.as_slice(), b"\x01"].concat(),
                text[..text.len().min(5)].to_vec(),
                b"\x01".to_vec(),
                b"zzzzzzzzzzzzzzzzzzzzzzzzz".to_vec(),
                b"\x01\x01".to_vec(),
            ];
            for _ in 0..60 {
                let t = &tokens[r.below(tokens.len())];
                let s = r.below(t.len());
                let mut p = t[s..s + 1 + r.below(t.len() - s)].to_vec();
                match r.below(8) {
                    // across the 0x01 into the next token
                    0 if s + p.len() == t.len() => p.push(0x01),
                    // most likely absent
                    1 => p.push(letters[r.below(letters.len())]),
                    _ => {}
                }
                ps.push(p);
            }
            for p in &ps {
                let places = plain(&text, p);
                let rows = fm.rows(p);
                assert_eq!(rows.len(), places.len(), "count of {p:?}");
                assert_eq!(with.count(p), places.len());
                let mut want: Vec<usize> = places.iter().map(|&pl| starts.partition_point(|&s| s <= pl) - 1).collect();
                let mut got: Vec<usize> = rows.clone().map(|row| walk_to_token(&fm, sep.start, row)).collect();
                want.sort_unstable();
                got.sort_unstable();
                assert_eq!(got, want, "tokens holding {p:?}");
                walked += got.len();
                if !places.is_empty() {
                    assert_eq!(fm.positions(p), None, "no places are kept");
                }
                for row in rows {
                    assert_eq!(fm.locate(row), None);
                }
            }
        }
        assert!(walked > 10_000, "{walked} places walked");
        // the empty text: no tokens, every piece absent
        let fm = Fm::no_places(b"").unwrap();
        assert_eq!((fm.count(b"a"), fm.count(b"\x01"), fm.locate(0), fm.back(0)), (0, 0, None, None));
        assert!(Fm::no_places(b"x").is_ok());
    }

    /// The stored form with no places kept: neither marks nor places, `file_len` says so, and
    /// the index read back gives the same counts and walks; written again, the same bytes. A
    /// damaged one gives an error or answers that never panic.
    #[test]
    fn stored_index_with_no_places() {
        let mut r = Rng(13);
        for n in [0, 1, 2, 63, 64, 65, 700, 3000] {
            let text = random_text(&mut r, b"\x01abcde\x80\xff", n);
            let fm = Fm::no_places(&text).unwrap();
            let with = Fm::new(&text).unwrap();
            let mut stored = Vec::new();
            fm.write(&mut stored);
            assert_eq!(stored.len(), fm.file_len());
            // the index with places holds the marks and the places more
            assert_eq!(with.file_len() - fm.file_len(), 8 * (n + 1).div_ceil(64) + 4 * (n / SAMPLE as usize + 1));
            stored.extend_from_slice(b"after");
            let (back, used) = Fm::read(&stored).unwrap();
            assert_eq!((used, back.rate(), back.text_row()), (fm.file_len(), 0, fm.text_row()));
            for p in patterns(&mut r, &text, b"\x01abcde\x80\xff", 30) {
                assert_eq!(back.count(&p), plain(&text, &p).len());
            }
            for row in 0..=n {
                assert_eq!(back.back(row), fm.back(row));
            }
            let mut again = Vec::new();
            back.write(&mut again);
            assert_eq!(again, stored[..used], "written again, the same bytes");
        }
        let text = random_text(&mut r, b"\x01abcde\x80\xff", 900);
        let mut good = Vec::new();
        Fm::no_places(&text).unwrap().write(&mut good);
        for cut in 0..good.len() {
            assert!(Fm::read(&good[..cut]).is_err(), "cut at {cut}");
        }
        for round in 0..3000 {
            let mut bad = good.clone();
            for _ in 0..1 + round % 3 {
                let at = r.below(bad.len());
                bad[at] ^= 1 << r.below(8);
            }
            if let Ok((damaged, _)) = Fm::read(&bad) {
                for p in [&b"ab"[..], b"\x01", b"cde\x01a", b"\x80\xff"] {
                    let _ = damaged.rows(p);
                }
                for row in 0..=damaged.len() + 1 {
                    let _ = (damaged.back(row), damaged.locate(row));
                }
            }
        }
    }
}
