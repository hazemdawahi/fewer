//! ozip: our own general-purpose packer (no zstd code; ideas only).
//!
//! 1. "Copy from earlier" (LZ77): a hash table + chains find earlier repeats of the
//!    bytes ahead; the parse emits sequences (literal count, match length, distance).
//!    The last three distances are kept ("repeat offsets"): using one again is cheap.
//!    Lazy matching: before taking a match, check whether the next byte starts a longer one.
//! 2. "Short codes for common things" (rANS): literal bytes and the sequence codes are
//!    each coded with their own frequency table; a symbol of probability p costs about
//!    log2(1/p) bits. Numbers are split into a size code (coded) + extra bits (as is).
//! 3. Levels 20-25 (method 5): the fast adaptive coder, see its section at the end; levels
//!    26-28: the same coder with a deeper copy search (for small tables; opened as fast).
//!
//! Format: method byte (0 = stored, 1 = packed, 3-4 = range coder, 5 = fast range coder),
//! varint(original length), then streams.

const PB: u32 = 12; // probability bits: frequencies sum to 4096
const M: u32 = 1 << PB;
const RL: u32 = 1 << 23; // rANS state lower bound

pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub(crate) fn get_varint(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut s = 0;
    loop {
        let x = *b.get(*p)?;
        *p += 1;
        if s > 63 {
            return None;
        }
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return Some(v);
        }
        s += 7;
    }
}

// ---------------------------------------------------------------- rANS
/// Frequencies (summing to M) for the symbols that occur.
fn normalize(counts: &[u64]) -> Vec<u32> {
    let total: u64 = counts.iter().sum();
    let mut f: Vec<u32> = counts.iter().map(|&c| if c == 0 { 0 } else { ((c * M as u64) / total).max(1) as u32 }).collect();
    let mut sum: i64 = f.iter().map(|&x| x as i64).sum();
    // fix the rounding on the largest symbols
    while sum != M as i64 {
        let mut order: Vec<usize> = (0..f.len()).filter(|&i| f[i] > 0).collect();
        super::sort_stable_by(&mut order, |&a, &b| f[a] > f[b]);
        for &i in &order {
            if sum == M as i64 {
                break;
            }
            if sum < M as i64 {
                let add = (M as i64 - sum).min(f[i] as i64 + 1);
                f[i] += add as u32;
                sum += add;
            } else if f[i] > 1 {
                let sub = (sum - M as i64).min(f[i] as i64 - 1);
                f[i] -= sub as u32;
                sum -= sub;
            }
        }
    }
    f
}

fn write_table(out: &mut Vec<u8>, f: &[u32]) {
    let nz: Vec<usize> = (0..f.len()).filter(|&i| f[i] > 0).collect();
    put_varint(out, nz.len() as u64);
    let mut prev = 0usize;
    for &i in &nz {
        put_varint(out, (i - prev) as u64);
        put_varint(out, (f[i] - 1) as u64);
        prev = i;
    }
}

fn read_table(b: &[u8], p: &mut usize, alpha: usize) -> Option<Vec<u32>> {
    let nz = get_varint(b, p)? as usize;
    let mut f = vec![0u32; alpha];
    let mut s = 0usize;
    for _ in 0..nz {
        s += get_varint(b, p)? as usize;
        if s >= alpha {
            return None;
        }
        f[s] = get_varint(b, p)? as u32 + 1;
    }
    if f.iter().map(|&x| x as u64).sum::<u64>() != M as u64 && nz > 0 {
        return None;
    }
    Some(f)
}

/// One stream of symbols: table + rANS bytes.
fn rans_put(out: &mut Vec<u8>, syms: &[u16], alpha: usize) {
    put_varint(out, syms.len() as u64);
    if syms.is_empty() {
        return;
    }
    let mut counts = vec![0u64; alpha];
    for &s in syms {
        counts[s as usize] += 1;
    }
    let f = normalize(&counts);
    let mut cum = vec![0u32; alpha + 1];
    for i in 0..alpha {
        cum[i + 1] = cum[i] + f[i];
    }
    let t0 = out.len();
    write_table(out, &f);
    if stat_on() {
        STAT[11].fetch_add(out.len() - t0, std::sync::atomic::Ordering::Relaxed);
    }
    let mut buf: Vec<u8> = Vec::with_capacity(syms.len() / 2 + 8);
    let mut x: u32 = RL;
    for &s in syms.iter().rev() {
        let (fs, cs) = (f[s as usize], cum[s as usize]);
        let xmax = ((RL >> PB) << 8) * fs;
        while x >= xmax {
            buf.push(x as u8);
            x >>= 8;
        }
        x = ((x / fs) << PB) + (x % fs) + cs;
    }
    buf.extend_from_slice(&x.to_le_bytes());
    buf.reverse();
    put_varint(out, buf.len() as u64);
    out.extend_from_slice(&buf);
}

fn rans_get(b: &[u8], p: &mut usize, alpha: usize, max_n: usize) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    rans_get_into(b, p, alpha, max_n, &mut out, |s| s)?;
    Some(out)
}

/// The literal stream (symbols 0-255) as bytes.
fn rans_get_bytes(b: &[u8], p: &mut usize, max_n: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    rans_get_into(b, p, 256, max_n, &mut out, |s| s as u8)?;
    Some(out)
}

/// Decode one rANS stream into `out` (each symbol through `conv`). One table entry per slot
/// holds the symbol, its frequency and the slot's offset in it (one load per symbol).
#[inline(always)]
fn rans_get_into<T>(b: &[u8], p: &mut usize, alpha: usize, max_n: usize, out: &mut Vec<T>, conv: impl Fn(u16) -> T) -> Option<()> {
    let n = get_varint(b, p)? as usize;
    if n > max_n {
        return None;
    }
    if n == 0 {
        return Some(());
    }
    let f = read_table(b, p, alpha)?;
    // entry: symbol | frequency << 16 | (slot - start of the symbol's slots) << 32
    let mut lut = vec![0u64; M as usize];
    let mut cum = 0usize;
    for s in 0..alpha {
        for k in 0..f[s] as usize {
            lut[cum + k] = s as u64 | (f[s] as u64) << 16 | (k as u64) << 32;
        }
        cum += f[s] as usize;
    }
    let len = get_varint(b, p)? as usize;
    let data = b.get(*p..*p + len)?;
    *p += len;
    if data.len() < 4 {
        return None;
    }
    let mut x = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let mut q = 4;
    out.reserve(n);
    for _ in 0..n {
        let e = lut[(x & (M - 1)) as usize];
        out.push(conv(e as u16));
        x = (e >> 16 & 0xFFFF) as u32 * (x >> PB) + (e >> 32) as u32;
        while x < RL {
            x = (x << 8) | *data.get(q)? as u32;
            q += 1;
        }
    }
    Some(())
}

/// v -> (code, number of extra bits, extra bits). 0..15 are their own code; larger
/// values: code from the bit length and the next bit, the rest as extra bits.
#[inline]
fn bucket(v: u32) -> (u16, u32, u32) {
    if v < 16 {
        return (v as u16, 0, 0);
    }
    let b = 31 - v.leading_zeros(); // >= 4
    let code = 16 + (b - 4) * 2 + ((v >> (b - 1)) & 1);
    (code as u16, b - 1, v & ((1 << (b - 1)) - 1))
}

#[inline]
fn unbucket(code: u16, extra: &mut BitReader) -> Option<u32> {
    if code < 16 {
        return Some(code as u32);
    }
    let k = code as u32 - 16;
    let b = k / 2 + 4;
    if b > 31 {
        return None;
    }
    let hi = k & 1;
    Some((1 << b) | (hi << (b - 1)) | extra.read(b - 1)?)
}

const NB: usize = 16 + 28 * 2; // number of size codes

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc |= (v as u64) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

struct BitReader<'a> {
    b: &'a [u8],
    p: usize,
    acc: u64,
    n: u32,
}

impl BitReader<'_> {
    fn read(&mut self, bits: u32) -> Option<u32> {
        if bits == 0 {
            return Some(0);
        }
        while self.n < bits {
            self.acc |= (*self.b.get(self.p)? as u64) << self.n;
            self.p += 1;
            self.n += 8;
        }
        let v = (self.acc & ((1u64 << bits) - 1)) as u32;
        self.acc >>= bits;
        self.n -= bits;
        Some(v)
    }
}

// ---------------------------------------------------------------- LZ77 parse
const MIN_MATCH: usize = 4;

#[inline]
fn hash4(d: &[u8], i: usize, bits: u32) -> usize {
    let v = u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    (v.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as usize
}

#[inline(always)]
fn match_len(d: &[u8], a: usize, b: usize, max: usize) -> usize {
    let mut n = 0;
    while n + 8 <= max {
        let x = u64::from_le_bytes(d[a + n..a + n + 8].try_into().unwrap()) ^ u64::from_le_bytes(d[b + n..b + n + 8].try_into().unwrap());
        if x != 0 {
            return n + (x.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    while n < max && d[a + n] == d[b + n] {
        n += 1;
    }
    n
}

/// A dictionary (the kit's sample text) with its match index built once and shared
/// read-only by every core: matches can point into it as if it came just before the data.
pub struct Dict {
    d: Vec<u8>,
    hb: u32,
    head: Vec<u32>,
    prev: Vec<u32>,
}

impl Dict {
    pub fn new(d: &[u8]) -> Dict {
        let hb = 20;
        let mut head = vec![u32::MAX; 1 << hb];
        let mut prev = vec![u32::MAX; d.len()];
        for i in 0..d.len().saturating_sub(MIN_MATCH) {
            let h = hash4(d, i, hb);
            prev[i] = head[h];
            head[h] = i as u32;
        }
        Dict { d: d.to_vec(), hb, head, prev }
    }
}

/// One step of the parse: `ll` literals, then a match of `ml` bytes (`rk` < 3: repeat
/// distance number `rk`; 3: the new distance `dist`).
#[derive(Clone, Copy)]
pub(crate) struct Seq {
    pub(crate) ll: u32,
    pub(crate) ml: u32,
    pub(crate) rk: u8,
    pub(crate) dist: u32,
}

/// Finds earlier repeats: hash chains over the data (and the dictionary's own chains).
struct Finder<'a> {
    data: &'a [u8],
    dict: Option<&'a Dict>,
    dl: usize,
    n: usize,
    end: usize,
    hb: u32,
    head: Vec<u32>,
    prev: Vec<u32>,
    chain_max: usize,
}

impl<'a> Finder<'a> {
    fn new(data: &'a [u8], dict: Option<&'a Dict>, chain_max: usize) -> Finder<'a> {
        let n = data.len();
        let hb: u32 = (usize::BITS - n.leading_zeros()).clamp(10, 20);
        Finder {
            data,
            dict,
            dl: dict.map_or(0, |x| x.d.len()),
            n,
            end: n.saturating_sub(MIN_MATCH),
            hb,
            head: vec![u32::MAX; 1 << hb],
            prev: vec![u32::MAX; n],
            chain_max,
        }
    }

    #[inline]
    fn insert(&mut self, i: usize) {
        if i < self.end {
            let h = hash4(self.data, i, self.hb);
            self.prev[i] = self.head[h];
            self.head[h] = i as u32;
        }
    }

    /// A match of data[i..] against the dictionary at j (it may not run past the dictionary's end).
    #[inline]
    fn dict_len(&self, x: &Dict, j: usize, i: usize) -> usize {
        let max = (self.n - i).min(x.d.len() - j);
        let mut l = 0;
        while l < max && x.d[j + l] == self.data[i + l] {
            l += 1;
        }
        l
    }

    /// Length of the match at distance r (a distance beyond i reaches into the dictionary).
    #[inline]
    fn rep_len(&self, r: usize, i: usize) -> usize {
        if r <= i {
            match_len(self.data, i, i - r, self.n - i)
        } else if let Some(x) = self.dict.filter(|_| r <= i + self.dl) {
            self.dict_len(x, self.dl + i - r, i)
        } else {
            0
        }
    }

    /// Best match at i: (length, distance, rep index or 3).
    fn find(&self, reps: &[usize; 3], i: usize) -> (usize, usize, usize) {
        let max = self.n - i;
        let mut best = (0usize, 0usize, 3usize);
        for (k, &r) in reps.iter().enumerate() {
            let l = self.rep_len(r, i);
            if l >= MIN_MATCH && l > best.0 {
                best = (l, r, k);
            }
        }
        if i >= self.end {
            return best;
        }
        if let Some(x) = self.dict {
            let mut c = x.head[hash4(self.data, i, x.hb)];
            let mut steps = 0;
            while c != u32::MAX && steps < self.chain_max {
                let j = c as usize;
                let l = self.dict_len(x, j, i);
                if l >= MIN_MATCH && l > best.0 + if best.2 < 3 { 1 } else { 0 } {
                    best = (l, self.dl - j + i, 3);
                    if l == max {
                        break;
                    }
                }
                c = x.prev[j];
                steps += 1;
            }
        }
        let mut c = self.head[hash4(self.data, i, self.hb)];
        let mut steps = 0;
        while c != u32::MAX && steps < self.chain_max {
            let j = c as usize;
            // a new distance must win by more than a repeat costs
            let need = (best.0 + if best.2 < 3 { 1 } else { 0 }).max(MIN_MATCH - 1);
            // it can only win if it also matches the byte at that length
            if need < max && self.data[j + need] != self.data[i + need] {
                c = self.prev[j];
                steps += 1;
                continue;
            }
            let l = match_len(self.data, i, j, max);
            if l >= MIN_MATCH && l > best.0 + if best.2 < 3 { 1 } else { 0 } {
                best = (l, i - j, 3);
                if l == max {
                    break;
                }
            }
            c = self.prev[j];
            steps += 1;
        }
        best
    }

    /// Every match at i that is longer than all closer ones, as (length, distance):
    /// the data first (closest first), then the dictionary.
    fn find_all(&self, i: usize, out: &mut Vec<(u32, u32)>, enough: usize) {
        out.clear();
        if i >= self.end {
            return;
        }
        let max = self.n - i;
        let mut best = MIN_MATCH - 1;
        let mut c = self.head[hash4(self.data, i, self.hb)];
        let mut steps = 0;
        while c != u32::MAX && steps < self.chain_max {
            let j = c as usize;
            // it can only be longer if it also matches the byte just past the best so far
            if self.data[j + best] != self.data[i + best] {
                c = self.prev[j];
                steps += 1;
                continue;
            }
            let l = match_len(self.data, i, j, max);
            if l > best {
                best = l;
                out.push((l as u32, (i - j) as u32));
                if l == max || l >= enough {
                    return;
                }
            }
            c = self.prev[j];
            steps += 1;
        }
        if let Some(x) = self.dict {
            let mut c = x.head[hash4(self.data, i, x.hb)];
            let mut steps = 0;
            while c != u32::MAX && steps < self.chain_max {
                let j = c as usize;
                let l = self.dict_len(x, j, i);
                if l > best {
                    best = l;
                    out.push((l as u32, (self.dl - j + i) as u32));
                    if l == max || l >= enough {
                        return;
                    }
                }
                c = x.prev[j];
                steps += 1;
            }
        }
    }
}

/// The greedy / lazy parse (levels up to 9).
fn lazy_parse(f: &mut Finder, lazy: bool) -> Vec<Seq> {
    let mut seqs = Vec::new();
    let mut reps: [usize; 3] = [1, 4, 8];
    let mut i = 0usize;
    let mut lit_start = 0usize;
    while i < f.end {
        let mut m = f.find(&reps, i);
        if m.0 < MIN_MATCH {
            f.insert(i);
            i += 1;
            continue;
        }
        f.insert(i);
        if lazy {
            // a longer match one byte later? take a literal now instead
            while i + 1 < f.end {
                let m2 = f.find(&reps, i + 1);
                if m2.0 > m.0 + 1 {
                    i += 1;
                    m = m2;
                    f.insert(i);
                } else {
                    break;
                }
            }
        }
        let (len, dist, rk) = m;
        seqs.push(Seq { ll: (i - lit_start) as u32, ml: len as u32, rk: rk as u8, dist: dist as u32 });
        reps = next_reps(reps, rk as u8, dist);
        for k in i + 1..i + len {
            f.insert(k);
        }
        i += len;
        lit_start = i;
    }
    seqs
}

#[inline]
pub(crate) fn next_reps(reps: [usize; 3], rk: u8, dist: usize) -> [usize; 3] {
    match rk {
        0 => reps,
        1 => [reps[1], reps[0], reps[2]],
        2 => [reps[2], reps[0], reps[1]],
        _ => [dist, reps[0], reps[1]],
    }
}

// ---------------------------------------------------------------- optimal parse
/// Bit costs (in 1/256 bit) of every symbol, from the counts of an earlier parse.
struct Prices {
    lit: Vec<i64>,
    ll: Vec<i64>,
    ml: Vec<i64>,
    of: Vec<i64>,
}

/// log2(x) in 1/256 bit, whole-number math only (the same result on every machine).
fn log2_256(x: u64) -> i64 {
    if x == 0 {
        return 0;
    }
    let b = 63 - x.leading_zeros() as u64;
    // y = x / 2^b as a 32-bit fraction in [1, 2); each squaring gives one more bit of the log
    let mut y: u128 = if b >= 32 { (x >> (b - 32)) as u128 } else { (x as u128) << (32 - b) };
    let mut frac = 0u64;
    for k in 0..8 {
        y = (y * y) >> 32;
        if y >= 1u128 << 33 {
            y >>= 1;
            frac |= 1 << (7 - k);
        }
    }
    (b * 256 + frac) as i64
}

fn cost_table(counts: &[u64]) -> Vec<i64> {
    let total = counts.iter().sum::<u64>() + counts.len() as u64;
    let t = log2_256(total);
    counts.iter().map(|&c| t - log2_256(c + 1)).collect()
}

/// Bits (in 1/256 bit) to code these counts with one frequency table, plus the table.
fn stream_cost(counts: &[u64]) -> i64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 8 * 256;
    }
    let t = log2_256(total);
    let mut bits = 0i64;
    let mut used = 0i64;
    for &c in counts {
        if c > 0 {
            bits += c as i64 * (t - log2_256(c));
            used += 1;
        }
    }
    bits + used * 16 * 256 + 4 * 8 * 256
}

/// Estimated packed size in bytes without writing anything: the parse at `level`,
/// then the cost of each stream from its counts. Used to choose between layouts.
pub fn estimate(data: &[u8], level: u32) -> usize {
    let n = data.len();
    if n < 16 {
        return n + 2;
    }
    let chain = match level {
        0..=1 => 4,
        2 => 16,
        _ => 64,
    };
    let seqs = lazy_parse(&mut Finder::new(data, None, chain), level >= 2);
    let (mut lit, mut ll, mut ml, mut of) = (vec![0u64; 256], vec![0u64; NB], vec![0u64; NB], vec![0u64; NB + 3]);
    let mut extra = 0i64;
    let mut p = 0usize;
    for s in &seqs {
        for &b in &data[p..p + s.ll as usize] {
            lit[b as usize] += 1;
        }
        let (c, nb, _) = bucket(s.ll);
        ll[c as usize] += 1;
        extra += nb as i64;
        let (c, nb, _) = bucket(s.ml - MIN_MATCH as u32);
        ml[c as usize] += 1;
        extra += nb as i64;
        if s.rk < 3 {
            of[s.rk as usize] += 1;
        } else {
            let (c, nb, _) = bucket(s.dist);
            of[3 + c as usize] += 1;
            extra += nb as i64;
        }
        p += (s.ll + s.ml) as usize;
    }
    for &b in &data[p..] {
        lit[b as usize] += 1;
    }
    let mut bits = stream_cost(&lit) + extra * 256;
    if !seqs.is_empty() {
        bits += stream_cost(&ll) + stream_cost(&ml) + stream_cost(&of);
    }
    ((bits / 256 / 8) as usize + 4).min(n + 2)
}

fn prices_of(data: &[u8], seqs: &[Seq]) -> Prices {
    let (mut lit, mut ll, mut ml, mut of) = (vec![0u64; 256], vec![0u64; NB], vec![0u64; NB], vec![0u64; NB + 3]);
    let mut p = 0usize;
    for s in seqs {
        for &b in &data[p..p + s.ll as usize] {
            lit[b as usize] += 1;
        }
        ll[bucket(s.ll).0 as usize] += 1;
        ml[bucket(s.ml - MIN_MATCH as u32).0 as usize] += 1;
        of[if s.rk < 3 { s.rk as usize } else { 3 + bucket(s.dist).0 as usize }] += 1;
        p += (s.ll + s.ml) as usize;
    }
    for &b in &data[p..] {
        lit[b as usize] += 1;
    }
    Prices { lit: cost_table(&lit), ll: cost_table(&ll), ml: cost_table(&ml), of: cost_table(&of) }
}

#[derive(Clone, Copy, Default)]
struct Node {
    price: i64,
    len: u32, // 0 = reached by a literal
    rk: u8,
    dist: u32,
    lit: u32, // literals since the last match
    reps: [usize; 3],
}

const OPT_W: usize = 4096; // positions per window of the optimal parse

/// A match at least this long is taken at once (tuning: CM_OZSUFF).
fn suff() -> usize {
    static V: crate::engine::fl::NoEnvOnce<usize> = crate::engine::fl::NoEnvOnce::new();
    *V.get_or_init(|| crate::engine::fl::no_env::var("CM_OZSUFF").ok().and_then(|v| v.parse().ok()).unwrap_or(192).clamp(8, 4096))
}

/// Hash-chain steps for the optimal parse (tuning: CM_OZCHAIN) and for its first, pricing pass (CM_OZCHAIN0).
fn opt_chains() -> (usize, usize) {
    static V: crate::engine::fl::NoEnvOnce<(usize, usize)> = crate::engine::fl::NoEnvOnce::new();
    *V.get_or_init(|| {
        let g = |k: &str, d: usize| crate::engine::fl::no_env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        (g("CM_OZCHAIN", 128), g("CM_OZCHAIN0", 256))
    })
}

/// Optimal parse: the cheapest path of literals and matches under the prices,
/// window by window (zstd's "btopt" idea, with our own hash chains and costs).
fn opt_parse(f: &mut Finder, pr: &Prices) -> Vec<Seq> {
    let data = f.data;
    let mut seqs = Vec::new();
    let mut reps: [usize; 3] = [1, 4, 8];
    let mut lit_start = 0usize;
    let mut i = 0usize;
    let suff = suff();
    let mut nodes = vec![Node::default(); OPT_W + suff + 2];
    let mut cands: Vec<(u32, u32)> = Vec::new();
    let llp = |v: u32| -> i64 {
        let (c, nb, _) = bucket(v);
        pr.ll[c as usize] + 256 * nb as i64
    };
    let mlp = |len: usize| -> i64 {
        let (c, nb, _) = bucket((len - MIN_MATCH) as u32);
        pr.ml[c as usize] + 256 * nb as i64
    };
    let ofp = |d: u32| -> i64 {
        let (c, nb, _) = bucket(d);
        pr.of[3 + c as usize] + 256 * nb as i64
    };
    let ll0 = llp(0);
    let mut path: Vec<(usize, usize, u8, u32)> = Vec::new();
    while i < f.end {
        let w = (f.end - i).min(OPT_W);
        for nd in nodes[..=w + suff].iter_mut() {
            nd.price = i64::MAX;
        }
        nodes[0] = Node { price: 0, len: 0, rk: 0, dist: 0, lit: (i - lit_start) as u32, reps };
        let mut stop = w;
        let mut forced: Option<(usize, u8, u32)> = None;
        for j in 0..w {
            let p = i + j;
            let nd = nodes[j];
            // a literal
            let lp = nd.price + pr.lit[data[p] as usize] + llp(nd.lit + 1) - llp(nd.lit);
            if lp < nodes[j + 1].price {
                nodes[j + 1] = Node { price: lp, len: 0, rk: 0, dist: 0, lit: nd.lit + 1, reps: nd.reps };
            }
            let base = nd.price + ll0;
            let mut longest = (0usize, 0u8, 0u32);
            // repeat distances
            for k in 0..3 {
                let r = nd.reps[k];
                let l = f.rep_len(r, p);
                if l < MIN_MATCH {
                    continue;
                }
                if l > longest.0 {
                    longest = (l, k as u8, r as u32);
                }
                if l >= suff {
                    continue;
                }
                let op = pr.of[k];
                for len in MIN_MATCH..=l {
                    let c = base + mlp(len) + op;
                    let t = &mut nodes[j + len];
                    if c < t.price {
                        *t = Node { price: c, len: len as u32, rk: k as u8, dist: r as u32, lit: 0, reps: next_reps(nd.reps, k as u8, r) };
                    }
                }
            }
            // new distances
            f.find_all(p, &mut cands, usize::MAX);
            let mut from = MIN_MATCH;
            for &(l, d) in &cands {
                let l = l as usize;
                if l > longest.0 + 1 || (l > longest.0 && longest.0 < suff) {
                    longest = (l, 3, d);
                }
                if l < suff {
                    let op = ofp(d);
                    for len in from..=l {
                        let c = base + mlp(len) + op;
                        let t = &mut nodes[j + len];
                        if c < t.price {
                            *t = Node { price: c, len: len as u32, rk: 3, dist: d, lit: 0, reps: next_reps(nd.reps, 3, d as usize) };
                        }
                    }
                }
                from = l + 1;
            }
            f.insert(p);
            if longest.0 >= suff {
                stop = j;
                forced = Some(longest);
                break;
            }
        }
        // walk back from `stop` along the cheapest path, then emit it in order
        path.clear();
        let mut j = stop;
        while j > 0 {
            let nd = nodes[j];
            if nd.len == 0 {
                j -= 1;
            } else {
                j -= nd.len as usize;
                path.push((i + j, nd.len as usize, nd.rk, nd.dist));
            }
        }
        for &(p, len, rk, dist) in path.iter().rev() {
            seqs.push(Seq { ll: (p - lit_start) as u32, ml: len as u32, rk, dist });
            reps = next_reps(reps, rk, dist as usize);
            lit_start = p + len;
        }
        debug_assert!(reps == nodes[stop].reps);
        i += stop;
        if let Some((len, rk, dist)) = forced {
            seqs.push(Seq { ll: (i - lit_start) as u32, ml: len as u32, rk, dist });
            reps = next_reps(reps, rk, dist as usize);
            for k in i + 1..i + len {
                f.insert(k);
            }
            i += len;
            lit_start = i;
        }
    }
    seqs
}

/// Compress with an optional dictionary (the same dictionary is needed to open).
/// Levels 0-9: greedy / lazy parse with longer and longer searches, frequency tables + rANS;
/// 10-11: optimal parse (10: one pass priced by a lazy parse; 11: a second pass);
/// 12-13: the same parses coded with the adaptive range coder; 14: range coder with its
/// own optimal parse (the default for final packing); 20-25: the fast adaptive coder (method 5,
/// faster and faster search towards 20; with a dictionary: level 12); 26-28: method 5 with
/// buckets of 16-64 at every input size (small tables).
/// Method 5 at a table level (20-29) without its frame check: what compress_with gives when
/// it is smaller than the stored form (for the engine's real-bytes checks).
pub fn table_coded(data: &[u8], level: u32) -> Vec<u8> {
    rc5_encode(data, &parse5_level(data, level))
}

pub fn compress_with(data: &[u8], level: u32, dict: Option<&Dict>) -> Vec<u8> {
    let n = data.len();
    // the stored form (method 0) is made only when it is the answer (the fz and fast levels)
    let stored_of = || {
        let mut stored = Vec::with_capacity(n + 11);
        stored.push(0u8);
        put_varint(&mut stored, n as u64);
        stored.extend_from_slice(data);
        stored
    };
    let stored_len = 1 + n + (64 - (n as u64 | 1).leading_zeros() as usize).div_ceil(7);
    if n < 16 {
        return stored_of();
    }
    if level >= 40 && level < 50 && dict.is_none() {
        // fz with its own greedy parse (levels 40-43)
        let out = super::fz::compress_fast(data, level - 40);
        return if out.len() >= stored_len { stored_of() } else { out };
    }
    if (20..30).contains(&level) && dict.is_none() {
        let out = rc5_encode(data, &parse5_level(data, level));
        return if out.len() >= stored_len { stored_of() } else { out };
    }
    let stored = stored_of();
    if level >= 50 && dict.is_none() {
        // hybrid: small inputs (below FL_HYB bytes, 256 KB by default) with the adaptive coder
        // (level 22: smaller, and its slowness costs little there), larger ones with fz
        static HYB: crate::engine::fl::NoEnvOnce<usize> = crate::engine::fl::NoEnvOnce::new();
        let hyb = *HYB.get_or_init(|| crate::engine::fl::no_env::var("FL_HYB").ok().and_then(|x| x.parse().ok()).unwrap_or(256 << 10));
        return compress_with(data, if n < hyb { 22 } else { level - 10 }, None);
    }
    if level >= 40 && dict.is_none() {
        // fz with its own greedy parse (levels 40-43)
        let out = super::fz::compress_fast(data, level - 40);
        return if out.len() >= stored.len() { stored } else { out };
    }
    if level >= 30 && dict.is_none() {
        // fz: the fast parse with static tables (method 6)
        let out = super::fz::compress(data, level - 10);
        return if out.len() >= stored.len() { stored } else { out };
    }
    if level >= 20 && dict.is_none() {
        let out = rc5_encode(data, &parse5_level(data, level));
        return if out.len() >= stored.len() { stored } else { out };
    }
    let level = if level >= 20 { 12 } else { level };
    let (oc, oc0) = opt_chains();
    let chain_max: usize = match level {
        0..=1 => 4,
        2 => 16,
        3..=4 => 64,
        5..=6 => 256,
        7..=9 => 1024,
        _ => oc,
    };
    if level >= 14 {
        // the adaptive range coder with its own optimal parse
        let out = rc_opt(data, dict, oc);
        return if out.len() >= stored.len() { stored } else { out };
    }
    let mut seqs = lazy_parse(&mut Finder::new(data, dict, if level >= 10 { oc0 } else { chain_max }), level >= 2);
    if level == 10 || level == 11 || level == 13 {
        for _ in 0..if level == 11 { 2 } else { 1 } {
            let pr = prices_of(data, &seqs);
            seqs = opt_parse(&mut Finder::new(data, dict, chain_max), &pr);
        }
    }
    if level >= 12 {
        // the adaptive range coder (experiment: 12 = lazy parse, 13 = optimal parse)
        let out = rc_encode(data, dict.map_or(&[], |x| &x.d[..]), &seqs);
        return if out.len() >= stored.len() { stored } else { out };
    }
    // streams
    let mut lits: Vec<u16> = Vec::with_capacity(n / 4);
    let (mut llc, mut mlc, mut ofc) = (Vec::<u16>::new(), Vec::<u16>::new(), Vec::<u16>::new());
    let mut extra = BitWriter { out: Vec::new(), acc: 0, n: 0 };
    let mut p = 0usize;
    for s in &seqs {
        for &b in &data[p..p + s.ll as usize] {
            lits.push(b as u16);
        }
        let (c, nb, x) = bucket(s.ll);
        llc.push(c);
        extra.put(x, nb);
        let (c, nb, x) = bucket(s.ml - MIN_MATCH as u32);
        mlc.push(c);
        extra.put(x, nb);
        if s.rk < 3 {
            ofc.push(s.rk as u16);
        } else {
            let (c, nb, x) = bucket(s.dist);
            ofc.push(3 + c);
            extra.put(x, nb);
        }
        p += (s.ll + s.ml) as usize;
    }
    for &b in &data[p..] {
        lits.push(b as u16);
    }
    let mut out = vec![if dict.is_some() { 2u8 } else { 1u8 }];
    put_varint(&mut out, n as u64);
    put_varint(&mut out, (n - p) as u64); // literals after the last match
    let mut at = [0usize; 6];
    at[0] = out.len();
    rans_put(&mut out, &lits, 256);
    at[1] = out.len();
    rans_put(&mut out, &llc, NB);
    at[2] = out.len();
    rans_put(&mut out, &mlc, NB);
    at[3] = out.len();
    rans_put(&mut out, &ofc, NB + 3);
    at[4] = out.len();
    let ex = extra.finish();
    put_varint(&mut out, ex.len() as u64);
    out.extend_from_slice(&ex);
    at[5] = out.len();
    if stat_on() && level >= 10 {
        use std::sync::atomic::Ordering::Relaxed;
        let won = out.len() < stored.len();
        STAT[0].fetch_add(1, Relaxed);
        STAT[1].fetch_add(n, Relaxed);
        if won {
            STAT[2].fetch_add(at[1] - at[0], Relaxed);
            STAT[3].fetch_add(at[2] - at[1], Relaxed);
            STAT[4].fetch_add(at[3] - at[2], Relaxed);
            STAT[5].fetch_add(at[4] - at[3], Relaxed);
            STAT[6].fetch_add(at[5] - at[4], Relaxed);
            STAT[7].fetch_add(at[0], Relaxed);
            STAT[8].fetch_add(lits.len(), Relaxed);
            STAT[9].fetch_add(seqs.len(), Relaxed);
        } else {
            STAT[10].fetch_add(stored.len(), Relaxed);
        }
    }
    if out.len() >= stored.len() { stored } else { out }
}

/// Developer counters (CM_OZSTAT=1): where the packed bytes go.
static STAT: [std::sync::atomic::AtomicUsize; 12] = [const { std::sync::atomic::AtomicUsize::new(0) }; 12];

fn stat_on() -> bool {
    static ON: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *ON.get_or_init(|| crate::engine::fl::no_env::var_os("CM_OZSTAT").is_some())
}

pub fn print_stats() {
    if !stat_on() {
        return;
    }
    let g = |i: usize| STAT[i].load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("ozip frames {} (input {} bytes): literals {} B for {} literals | lengths-before {} B | match lengths {} B | distances {} B | extra bits {} B | headers {} B | {} sequences | stored frames {} B | frequency tables (inside the streams above, all levels) {} B",
              g(0), g(1), g(2), g(8), g(3), g(4), g(5), g(6), g(7), g(9), g(10), g(11));
}

pub fn decompress_with(b: &[u8], dict: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut p = 1;
    let method = *b.first()?;
    let n = get_varint(b, &mut p)? as usize;
    if method == 0 {
        return b.get(p..p + n).map(|s| s.to_vec());
    }
    if n > (1usize << 40) {
        return None;
    }
    if method == 3 || method == 4 {
        let d: &[u8] = if method == 4 { dict? } else { &[] };
        return rc_decode(b.get(p..)?, n, d);
    }
    if method == 5 {
        return rc5_decode(b.get(p..)?, n);
    }
    if method == 6 {
        return super::fz::decompress(b.get(p..)?, n);
    }
    let dict: &[u8] = match method {
        1 => &[],
        2 => dict?,
        _ => return None,
    };
    let tail = get_varint(b, &mut p)? as usize;
    let lits = rans_get_bytes(b, &mut p, n)?;
    let llc = rans_get(b, &mut p, NB, n)?;
    let mlc = rans_get(b, &mut p, NB, n)?;
    let ofc = rans_get(b, &mut p, NB + 3, n)?;
    let el = get_varint(b, &mut p)? as usize;
    let mut ex = BitReader { b: b.get(p..p + el)?, p: 0, acc: 0, n: 0 };
    if llc.len() != mlc.len() || llc.len() != ofc.len() {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut reps: [usize; 3] = [1, 4, 8];
    let mut lp = 0usize;
    for k in 0..llc.len() {
        let ll = unbucket(llc[k], &mut ex)? as usize;
        let ml = unbucket(mlc[k], &mut ex)? as usize + MIN_MATCH;
        let o = ofc[k];
        let dist = if o < 3 {
            let r = reps[o as usize];
            match o {
                1 => reps = [r, reps[0], reps[2]],
                2 => reps = [r, reps[0], reps[1]],
                _ => {}
            }
            r
        } else {
            let d = unbucket(o - 3, &mut ex)? as usize;
            reps = [d, reps[0], reps[1]];
            d
        };
        out.extend_from_slice(lits.get(lp..lp + ll)?);
        lp += ll;
        if dist == 0 || out.len() + ml > n {
            return None;
        }
        if dist > out.len() {
            // from the dictionary (the match ends inside it)
            let back = dist - out.len();
            if back > dict.len() || dict.len() - back + ml > dict.len() {
                return None;
            }
            let st = dict.len() - back;
            out.extend_from_slice(&dict[st..st + ml]);
            continue;
        }
        let from = out.len() - dist;
        if dist >= ml {
            out.extend_from_within(from..from + ml);
        } else {
            for q in 0..ml {
                let c = out[from + q];
                out.push(c);
            }
        }
    }
    if lits.len() - lp != tail {
        return None;
    }
    out.extend_from_slice(&lits[lp..]);
    if out.len() != n {
        return None;
    }
    Some(out)
}

// ================================================================ method 3/4: adaptive range coder
// The same copies and literals as above, coded bit by bit with probabilities that learn as
// they go (LZMA's ideas, our own code): no frequency tables to store, literals coded with
// the previous byte as context, and the literal right after a copy coded against the byte
// the copy would have continued with. Method 4 = with the kit's dictionary.

const RC_TOP: u32 = 1 << 24;
const PBITS: u32 = 11;
const PONE: u16 = 1 << PBITS;
const PMOVE: u32 = 5;
pub(crate) const P_INIT: u16 = PONE / 2;
/// Longest copy in one step (longer copies continue with "repeat the last distance").
pub(crate) const LEN_MAX: usize = 273;
const NSTATES: usize = 12;

pub(crate) struct RcEnc {
    low: u64,
    range: u32,
    cache: u8,
    csize: u64,
    out: Vec<u8>,
}

impl RcEnc {
    pub(crate) fn new() -> RcEnc {
        RcEnc { low: 0, range: u32::MAX, cache: 0, csize: 1, out: Vec::new() }
    }
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut t = self.cache;
            loop {
                self.out.push(t.wrapping_add(carry));
                t = 0xFF;
                self.csize -= 1;
                if self.csize == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.csize += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }
    #[inline(always)]
    pub(crate) fn bit(&mut self, p: &mut u16, b: u32) {
        // no branch on the bit: mask is all ones for a 1
        let pv = *p as u32;
        let bound = (self.range >> PBITS) * pv;
        let mask = 0u32.wrapping_sub(b);
        self.low += (bound & mask) as u64;
        self.range = (bound & !mask) | (self.range.wrapping_sub(bound) & mask);
        *p = prob_next(pv, mask);
        while self.range < RC_TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }
    pub(crate) fn direct(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.range >>= 1;
            if (v >> i) & 1 == 1 {
                self.low += self.range as u64;
            }
            while self.range < RC_TOP {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }
    #[inline(always)]
    fn tree(&mut self, probs: &mut [u16], nb: u32, v: u32) {
        let mut m = 1usize;
        for i in (0..nb).rev() {
            let b = (v >> i) & 1;
            self.bit(&mut probs[m], b);
            m = (m << 1) | b as usize;
        }
    }
    #[inline(always)]
    fn rev_tree(&mut self, probs: &mut [u16], nb: u32, mut v: u32) {
        let mut m = 1usize;
        for _ in 0..nb {
            let b = v & 1;
            v >>= 1;
            self.bit(&mut probs[m], b);
            m = (m << 1) | b as usize;
        }
    }
    /// The bytes; the first byte the coder makes is always 0 and is left out.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        debug_assert!(self.out[0] == 0);
        self.out.remove(0);
        self.out
    }
}

/// The probability after coding a bit (mask all ones for a 1): 1/32 of the way towards 0
/// (a 1 bit) or towards PONE (a 0 bit), without a branch; the same values as before.
#[inline(always)]
fn prob_next(pv: u32, mask: u32) -> u16 {
    let t = ((PONE as u32 - pv) & !mask) | (pv & mask);
    let d = t >> PMOVE;
    (pv.wrapping_add((d ^ mask).wrapping_sub(mask))) as u16
}

pub(crate) struct RcDec<'a> {
    b: &'a [u8],
    p: usize,
    range: u32,
    code: u32,
}

impl<'a> RcDec<'a> {
    pub(crate) fn new(b: &'a [u8]) -> RcDec<'a> {
        let mut d = RcDec { b, p: 0, range: u32::MAX, code: 0 };
        for _ in 0..4 {
            d.code = (d.code << 8) | d.next() as u32;
        }
        d
    }
    #[inline]
    fn next(&mut self) -> u8 {
        let x = self.b.get(self.p).copied().unwrap_or(0);
        self.p += 1;
        x
    }
    #[inline(always)]
    pub(crate) fn bit(&mut self, p: &mut u16) -> u32 {
        // no branch on the bit: mask is all ones for a 1
        let pv = *p as u32;
        let bound = (self.range >> PBITS) * pv;
        let b = (self.code >= bound) as u32;
        let mask = 0u32.wrapping_sub(b);
        self.code -= bound & mask;
        self.range = (bound & !mask) | (self.range.wrapping_sub(bound) & mask);
        *p = prob_next(pv, mask);
        if self.range < RC_TOP {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next() as u32;
        }
        b
    }
    pub(crate) fn direct(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            self.range >>= 1;
            let b = if self.code >= self.range {
                self.code -= self.range;
                1
            } else {
                0
            };
            v = (v << 1) | b;
            if self.range < RC_TOP {
                self.range <<= 8;
                self.code = (self.code << 8) | self.next() as u32;
            }
        }
        v
    }
    #[inline(always)]
    fn tree(&mut self, probs: &mut [u16], nb: u32) -> u32 {
        let mut m = 1usize;
        for _ in 0..nb {
            m = (m << 1) | self.bit(&mut probs[m]) as usize;
        }
        (m - (1 << nb)) as u32
    }
    #[inline(always)]
    fn rev_tree(&mut self, probs: &mut [u16], nb: u32) -> u32 {
        let mut m = 1usize;
        let mut v = 0u32;
        for i in 0..nb {
            let b = self.bit(&mut probs[m]);
            m = (m << 1) | b as usize;
            v |= b << i;
        }
        v
    }
}

#[derive(Clone)]
struct LenModel {
    choice: u16,
    choice2: u16,
    low: [[u16; 8]; 4],
    mid: [[u16; 8]; 4],
    high: [u16; 256],
}

impl LenModel {
    fn new() -> LenModel {
        LenModel { choice: P_INIT, choice2: P_INIT, low: [[P_INIT; 8]; 4], mid: [[P_INIT; 8]; 4], high: [P_INIT; 256] }
    }
    #[inline(always)]
    fn enc(&mut self, rc: &mut RcEnc, len: usize, ps: usize) {
        let l = (len - 2) as u32;
        if l < 8 {
            rc.bit(&mut self.choice, 0);
            rc.tree(&mut self.low[ps], 3, l);
        } else if l < 16 {
            rc.bit(&mut self.choice, 1);
            rc.bit(&mut self.choice2, 0);
            rc.tree(&mut self.mid[ps], 3, l - 8);
        } else {
            rc.bit(&mut self.choice, 1);
            rc.bit(&mut self.choice2, 1);
            rc.tree(&mut self.high, 8, l - 16);
        }
    }
    #[inline(always)]
    fn dec(&mut self, rc: &mut RcDec, ps: usize) -> usize {
        2 + if rc.bit(&mut self.choice) == 0 {
            rc.tree(&mut self.low[ps], 3) as usize
        } else if rc.bit(&mut self.choice2) == 0 {
            8 + rc.tree(&mut self.mid[ps], 3) as usize
        } else {
            16 + rc.tree(&mut self.high, 8) as usize
        }
    }
}

/// Every adaptive probability of the coder.
#[derive(Clone)]
struct Model {
    lc: u32,
    pb_mask: usize,
    is_match: [[u16; 4]; NSTATES],
    is_rep: [u16; NSTATES],
    is_rep0: [u16; NSTATES],
    is_rep1: [u16; NSTATES],
    is_rep2: [u16; NSTATES],
    is_rep0_long: [[u16; 4]; NSTATES],
    lit: Vec<[u16; 0x300]>,
    len: LenModel,
    rep_len: LenModel,
    slot: [[u16; 64]; 4],
    special: [[u16; 32]; 10],
    align: [u16; 16],
}

impl Model {
    fn new(lc: u32, pb: u32) -> Model {
        Model {
            lc,
            pb_mask: (1 << pb) - 1,
            is_match: [[P_INIT; 4]; NSTATES],
            is_rep: [P_INIT; NSTATES],
            is_rep0: [P_INIT; NSTATES],
            is_rep1: [P_INIT; NSTATES],
            is_rep2: [P_INIT; NSTATES],
            is_rep0_long: [[P_INIT; 4]; NSTATES],
            lit: vec![[P_INIT; 0x300]; 1 << lc],
            len: LenModel::new(),
            rep_len: LenModel::new(),
            slot: [[P_INIT; 64]; 4],
            special: [[P_INIT; 32]; 10],
            align: [P_INIT; 16],
        }
    }
}

#[inline]
fn st_lit(s: usize) -> usize {
    if s < 4 { 0 } else if s < 10 { s - 3 } else { s - 6 }
}
#[inline]
fn st_match(s: usize) -> usize {
    if s < 7 { 7 } else { 10 }
}
#[inline]
fn st_rep(s: usize) -> usize {
    if s < 7 { 8 } else { 11 }
}
#[inline]
fn st_short(s: usize) -> usize {
    if s < 7 { 9 } else { 11 }
}

#[inline]
fn dist_slot(d0: u32) -> u32 {
    if d0 < 4 {
        return d0;
    }
    let b = 31 - d0.leading_zeros();
    2 * b + ((d0 >> (b - 1)) & 1)
}

/// The byte `back` bytes before position `pos` of the data (reaching into the dictionary).
#[inline]
fn byte_back(data: &[u8], dict: &[u8], pos: usize, back: usize) -> u8 {
    if back <= pos {
        data[pos - back]
    } else {
        let k = back - pos;
        if k <= dict.len() { dict[dict.len() - k] } else { 0 }
    }
}

struct RcWriter<'a> {
    rc: RcEnc,
    m: Model,
    state: usize,
    reps: [usize; 4],
    data: &'a [u8],
    dict: &'a [u8],
}

impl RcWriter<'_> {
    fn literal(&mut self, pos: usize) {
        let ps = pos & self.m.pb_mask;
        self.rc.bit(&mut self.m.is_match[self.state][ps], 0);
        let prev = byte_back(self.data, self.dict, pos, 1) as usize;
        let probs = &mut self.m.lit[prev >> (8 - self.m.lc)];
        let c = self.data[pos] as u32;
        if self.state < 7 {
            let mut m = 1usize;
            for i in (0..8).rev() {
                let b = (c >> i) & 1;
                self.rc.bit(&mut probs[m], b);
                m = (m << 1) | b as usize;
            }
        } else {
            let mut mb = byte_back(self.data, self.dict, pos, self.reps[0]) as u32;
            let mut offs = 0x100u32;
            let mut sym = 1u32;
            for i in (0..8).rev() {
                mb <<= 1;
                let mbit = mb & offs;
                let b = (c >> i) & 1;
                self.rc.bit(&mut probs[(offs + mbit + sym) as usize], b);
                sym = (sym << 1) | b;
                offs &= if b == 0 { !mbit } else { mbit };
            }
        }
        self.state = st_lit(self.state);
    }

    /// A copy of `len` (2..=LEN_MAX) bytes from `dist` back, at `pos`.
    fn copy(&mut self, pos: usize, len: usize, dist: usize) {
        let ps = pos & self.m.pb_mask;
        let s = self.state;
        self.rc.bit(&mut self.m.is_match[s][ps], 1);
        if let Some(k) = self.reps.iter().position(|&r| r == dist) {
            self.rc.bit(&mut self.m.is_rep[s], 1);
            if k == 0 {
                self.rc.bit(&mut self.m.is_rep0[s], 0);
                self.rc.bit(&mut self.m.is_rep0_long[s][ps], 1);
            } else {
                self.rc.bit(&mut self.m.is_rep0[s], 1);
                if k == 1 {
                    self.rc.bit(&mut self.m.is_rep1[s], 0);
                } else {
                    self.rc.bit(&mut self.m.is_rep1[s], 1);
                    self.rc.bit(&mut self.m.is_rep2[s], (k - 2) as u32);
                }
                let r = self.reps[k];
                for j in (1..=k).rev() {
                    self.reps[j] = self.reps[j - 1];
                }
                self.reps[0] = r;
            }
            self.m.rep_len.enc(&mut self.rc, len, ps);
            self.state = st_rep(s);
        } else {
            self.rc.bit(&mut self.m.is_rep[s], 0);
            self.m.len.enc(&mut self.rc, len, ps);
            let d0 = (dist - 1) as u32;
            let slot = dist_slot(d0);
            self.rc.tree(&mut self.m.slot[(len - 2).min(3)], 6, slot);
            if slot >= 4 {
                let footer = (slot >> 1) - 1;
                let red = d0 - ((2 | (slot & 1)) << footer);
                if slot < 14 {
                    self.rc.rev_tree(&mut self.m.special[slot as usize - 4], footer, red);
                } else {
                    self.rc.direct(red >> 4, footer - 4);
                    self.rc.rev_tree(&mut self.m.align, 4, red & 15);
                }
            }
            self.reps = [dist, self.reps[0], self.reps[1], self.reps[2]];
            self.state = st_match(s);
        }
    }

    /// One byte repeated from the last distance ("short repeat").
    fn short_rep(&mut self, pos: usize) {
        let ps = pos & self.m.pb_mask;
        let s = self.state;
        self.rc.bit(&mut self.m.is_match[s][ps], 1);
        self.rc.bit(&mut self.m.is_rep[s], 1);
        self.rc.bit(&mut self.m.is_rep0[s], 0);
        self.rc.bit(&mut self.m.is_rep0_long[s][ps], 0);
        self.state = st_short(s);
    }

    /// A long copy, cut into steps of at most LEN_MAX (the later steps repeat the distance).
    fn long_copy(&mut self, mut pos: usize, mut len: usize, dist: usize) {
        while len > 0 {
            let mut take = len.min(LEN_MAX);
            if len - take == 1 {
                take -= 1;
            }
            self.copy(pos, take, dist);
            pos += take;
            len -= take;
        }
    }
}

const RC_LC: u32 = 3;
const RC_PB: u32 = 2;

/// Code a parse with the adaptive range coder (method 3, or 4 with a dictionary).
fn rc_encode(data: &[u8], dict: &[u8], seqs: &[Seq]) -> Vec<u8> {
    let mut w = RcWriter { rc: RcEnc::new(), m: Model::new(RC_LC, RC_PB), state: 0, reps: [1, 4, 8, 16], data, dict };
    let mut zreps: [usize; 3] = [1, 4, 8];
    let mut p = 0usize;
    for s in seqs {
        for q in p..p + s.ll as usize {
            w.literal(q);
        }
        p += s.ll as usize;
        let dist = if s.rk < 3 { zreps[s.rk as usize] } else { s.dist as usize };
        zreps = next_reps(zreps, s.rk, s.dist as usize);
        w.long_copy(p, s.ml as usize, dist);
        p += s.ml as usize;
    }
    for q in p..data.len() {
        w.literal(q);
    }
    let mut out = vec![if dict.is_empty() { 3u8 } else { 4u8 }];
    put_varint(&mut out, data.len() as u64);
    out.push((RC_LC | (RC_PB << 4)) as u8);
    out.extend_from_slice(&w.rc.finish());
    out
}

fn rc_decode(b: &[u8], n: usize, dict: &[u8]) -> Option<Vec<u8>> {
    let params = *b.first()?;
    let (lc, pb) = ((params & 15) as u32, (params >> 4) as u32);
    if lc > 8 || pb > 2 {
        return None;
    }
    let mut m = Model::new(lc, pb);
    let mut rc = RcDec::new(&b[1..]);
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut state = 0usize;
    let mut reps: [usize; 4] = [1, 4, 8, 16];
    let dl = dict.len();
    while out.len() < n {
        let pos = out.len();
        let ps = pos & m.pb_mask;
        if rc.bit(&mut m.is_match[state][ps]) == 0 {
            let prev = if pos > 0 { out[pos - 1] } else if dl > 0 { dict[dl - 1] } else { 0 } as usize;
            let probs = &mut m.lit[prev >> (8 - lc)];
            let mut sym = 1u32;
            if state < 7 {
                while sym < 0x100 {
                    sym = (sym << 1) | rc.bit(&mut probs[sym as usize]);
                }
            } else {
                let back = reps[0];
                let mut mb = if back <= pos { out[pos - back] } else if back - pos <= dl { dict[dl - (back - pos)] } else { 0 } as u32;
                let mut offs = 0x100u32;
                while sym < 0x100 {
                    mb <<= 1;
                    let mbit = mb & offs;
                    let bit = rc.bit(&mut probs[(offs + mbit + sym) as usize]);
                    sym = (sym << 1) | bit;
                    offs &= if bit == 0 { !mbit } else { mbit };
                }
            }
            out.push(sym as u8);
            state = st_lit(state);
            continue;
        }
        let len;
        if rc.bit(&mut m.is_rep[state]) == 1 {
            if rc.bit(&mut m.is_rep0[state]) == 0 {
                if rc.bit(&mut m.is_rep0_long[state][ps]) == 0 {
                    // one byte from the last distance
                    let back = reps[0];
                    let c = if back <= pos { out[pos - back] } else if back - pos <= dl { dict[dl - (back - pos)] } else { return None };
                    out.push(c);
                    state = st_short(state);
                    continue;
                }
            } else {
                let k = if rc.bit(&mut m.is_rep1[state]) == 0 { 1 } else { 2 + rc.bit(&mut m.is_rep2[state]) as usize };
                let r = reps[k];
                for j in (1..=k).rev() {
                    reps[j] = reps[j - 1];
                }
                reps[0] = r;
            }
            len = m.rep_len.dec(&mut rc, ps);
            state = st_rep(state);
        } else {
            len = m.len.dec(&mut rc, ps);
            let slot = rc.tree(&mut m.slot[(len - 2).min(3)], 6);
            let d0 = if slot < 4 {
                slot
            } else {
                let footer = (slot >> 1) - 1;
                let base = (2 | (slot & 1)) << footer;
                if slot < 14 {
                    base + rc.rev_tree(&mut m.special[slot as usize - 4], footer)
                } else {
                    let hi = rc.direct(footer - 4);
                    base.wrapping_add(hi << 4).wrapping_add(rc.rev_tree(&mut m.align, 4))
                }
            };
            reps = [d0 as usize + 1, reps[0], reps[1], reps[2]];
            state = st_match(state);
        }
        let dist = reps[0];
        if pos + len > n || dist > pos + dl {
            return None;
        }
        if dist > pos {
            // starts in the dictionary (and may run on into the data)
            for q in 0..len {
                let back = dist;
                let c = if back <= pos + q { out[pos + q - back] } else { dict[dl - (back - pos - q)] };
                out.push(c);
            }
        } else if dist >= len {
            out.extend_from_within(pos - dist..pos - dist + len);
        } else {
            for q in 0..len {
                let c = out[pos - dist + q];
                out.push(c);
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- optimal parse for the range coder
/// Cost (1/256 bit) of a bit whose probability of 0 is p/2048.
fn bit_price_table() -> &'static [i64; 128] {
    static T: std::sync::OnceLock<[i64; 128]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0i64; 128];
        for (i, x) in t.iter_mut().enumerate() {
            *x = log2_256(2048) - log2_256(i as u64 * 16 + 8);
        }
        t
    })
}

#[inline]
fn bp(t: &[i64; 128], p: u16, b: u32) -> i64 {
    let q = if b == 0 { p } else { PONE - p };
    t[(q >> 4) as usize]
}

fn tree_price(t: &[i64; 128], probs: &[u16], nb: u32, v: u32) -> i64 {
    let mut m = 1usize;
    let mut c = 0;
    for i in (0..nb).rev() {
        let b = (v >> i) & 1;
        c += bp(t, probs[m], b);
        m = (m << 1) | b as usize;
    }
    c
}

fn rev_tree_price(t: &[i64; 128], probs: &[u16], nb: u32, mut v: u32) -> i64 {
    let mut m = 1usize;
    let mut c = 0;
    for _ in 0..nb {
        let b = v & 1;
        v >>= 1;
        c += bp(t, probs[m], b);
        m = (m << 1) | b as usize;
    }
    c
}

fn len_price(t: &[i64; 128], lm: &LenModel, len: usize, ps: usize) -> i64 {
    let l = (len - 2) as u32;
    if l < 8 {
        bp(t, lm.choice, 0) + tree_price(t, &lm.low[ps], 3, l)
    } else if l < 16 {
        bp(t, lm.choice, 1) + bp(t, lm.choice2, 0) + tree_price(t, &lm.mid[ps], 3, l - 8)
    } else {
        bp(t, lm.choice, 1) + bp(t, lm.choice2, 1) + tree_price(t, &lm.high, 8, l - 16)
    }
}

/// Price tables taken from the coder's probabilities (refreshed now and then while parsing).
struct RcPrices {
    len: Vec<[i64; LEN_MAX + 1]>,
    rep_len: Vec<[i64; LEN_MAX + 1]>,
    slot: [[i64; 64]; 4],
    near: [[i64; 128]; 4], // whole distance price for distances 1..=128
    align: [i64; 16],
}

fn rc_prices(t: &[i64; 128], m: &Model) -> RcPrices {
    let nps = m.pb_mask + 1;
    let mut len = vec![[0i64; LEN_MAX + 1]; nps];
    let mut rep_len = vec![[0i64; LEN_MAX + 1]; nps];
    for ps in 0..nps {
        for l in 2..=LEN_MAX {
            len[ps][l] = len_price(t, &m.len, l, ps);
            rep_len[ps][l] = len_price(t, &m.rep_len, l, ps);
        }
    }
    let mut slot = [[0i64; 64]; 4];
    let mut near = [[0i64; 128]; 4];
    for ls in 0..4 {
        for s in 0..64u32 {
            slot[ls][s as usize] = tree_price(t, &m.slot[ls], 6, s);
        }
        for d0 in 0..128u32 {
            let s = dist_slot(d0);
            let mut c = slot[ls][s as usize];
            if s >= 4 {
                let footer = (s >> 1) - 1;
                c += rev_tree_price(t, &m.special[s as usize - 4], footer, d0 - ((2 | (s & 1)) << footer));
            }
            near[ls][d0 as usize] = c;
        }
    }
    let mut align = [0i64; 16];
    for (v, a) in align.iter_mut().enumerate() {
        *a = rev_tree_price(t, &m.align, 4, v as u32);
    }
    RcPrices { len, rep_len, slot, near, align }
}

impl RcPrices {
    #[inline]
    fn dist(&self, dist: usize, len: usize) -> i64 {
        let ls = (len - 2).min(3);
        let d0 = (dist - 1) as u32;
        if d0 < 128 {
            return self.near[ls][d0 as usize];
        }
        let s = dist_slot(d0);
        let footer = (s >> 1) - 1;
        let red = d0 - ((2 | (s & 1)) << footer);
        self.slot[ls][s as usize] + 256 * (footer as i64 - 4) + self.align[(red & 15) as usize]
    }
}

fn lit_price(t: &[i64; 128], probs: &[u16; 0x300], c: u32, matched: Option<u32>) -> i64 {
    let mut cost = 0;
    match matched {
        None => {
            let mut m = 1usize;
            for i in (0..8).rev() {
                let b = (c >> i) & 1;
                cost += bp(t, probs[m], b);
                m = (m << 1) | b as usize;
            }
        }
        Some(mut mb) => {
            let mut offs = 0x100u32;
            let mut sym = 1u32;
            for i in (0..8).rev() {
                mb <<= 1;
                let mbit = mb & offs;
                let b = (c >> i) & 1;
                cost += bp(t, probs[(offs + mbit + sym) as usize], b);
                sym = (sym << 1) | b;
                offs &= if b == 0 { !mbit } else { mbit };
            }
        }
    }
    cost
}

#[derive(Clone, Copy, Default)]
struct RNode {
    price: i64,
    len: u32,
    kind: u8, // 0 literal, 1 short repeat, 2 copy
    dist: u32,
    state: u8,
    reps: [u32; 4],
}

/// Optimal parse priced by the range coder's own probabilities, coded as it goes
/// (window by window; prices are refreshed every few hundred steps).
fn rc_opt(data: &[u8], dict: Option<&Dict>, chain: usize) -> Vec<u8> {
    let n = data.len();
    let t = bit_price_table();
    let dictb: &[u8] = dict.map_or(&[], |x| &x.d[..]);
    let mut f = Finder::new(data, dict, chain);
    let mut w = RcWriter { rc: RcEnc::new(), m: Model::new(RC_LC, RC_PB), state: 0, reps: [1, 4, 8, 16], data, dict: dictb };
    let suff = suff().min(LEN_MAX);
    let mut nodes = vec![RNode::default(); OPT_W + LEN_MAX + 2];
    let mut cands: Vec<(u32, u32)> = Vec::new();
    let mut path: Vec<(usize, RNode)> = Vec::new();
    let mut pr = rc_prices(t, &w.m);
    let mut since = 0usize;
    let mut i = 0usize;
    while i < n {
        if since >= 256 {
            pr = rc_prices(t, &w.m);
            since = 0;
        }
        let wl = (n - i).min(OPT_W);
        for nd in nodes[..=wl + LEN_MAX].iter_mut() {
            nd.price = i64::MAX;
        }
        nodes[0] = RNode { price: 0, len: 0, kind: 0, dist: 0, state: w.state as u8, reps: w.reps.map(|r| r as u32) };
        let mut stop = wl;
        let mut forced: Option<(usize, usize)> = None; // (len, dist)
        for j in 0..wl {
            let p = i + j;
            let nd = nodes[j];
            let st = nd.state as usize;
            let ps = p & w.m.pb_mask;
            let m = &w.m;
            let r0 = nd.reps[0] as usize;
            // a literal
            let prev = byte_back(data, dictb, p, 1) as usize;
            let mbyte = byte_back(data, dictb, p, r0);
            let probs = &m.lit[prev >> (8 - m.lc)];
            let c = nd.price + bp(t, m.is_match[st][ps], 0)
                + lit_price(t, probs, data[p] as u32, if st < 7 { None } else { Some(mbyte as u32) });
            if c < nodes[j + 1].price {
                nodes[j + 1] = RNode { price: c, len: 1, kind: 0, dist: 0, state: st_lit(st) as u8, reps: nd.reps };
            }
            let m1 = nd.price + bp(t, m.is_match[st][ps], 1);
            let rep_base = m1 + bp(t, m.is_rep[st], 1);
            // one byte from the last distance
            if data[p] == mbyte && r0 <= p + dictb.len() {
                let c = rep_base + bp(t, m.is_rep0[st], 0) + bp(t, m.is_rep0_long[st][ps], 0);
                if c < nodes[j + 1].price {
                    nodes[j + 1] = RNode { price: c, len: 1, kind: 1, dist: r0 as u32, state: st_short(st) as u8, reps: nd.reps };
                }
            }
            let mut longest = (0usize, 0usize);
            // repeat distances
            for k in 0..4 {
                let r = nd.reps[k] as usize;
                let l = f.rep_len(r, p);
                if l < 2 {
                    continue;
                }
                if l > longest.0 {
                    longest = (l, r);
                }
                if l >= suff {
                    continue;
                }
                let sel = if k == 0 {
                    bp(t, m.is_rep0[st], 0) + bp(t, m.is_rep0_long[st][ps], 1)
                } else if k == 1 {
                    bp(t, m.is_rep0[st], 1) + bp(t, m.is_rep1[st], 0)
                } else {
                    bp(t, m.is_rep0[st], 1) + bp(t, m.is_rep1[st], 1) + bp(t, m.is_rep2[st], (k - 2) as u32)
                };
                let mut reps2 = nd.reps;
                reps2.copy_within(0..k, 1);
                reps2[0] = r as u32;
                let base = rep_base + sel;
                let lp = &pr.rep_len[ps];
                for len in 2..=l {
                    let c = base + lp[len];
                    let tn = &mut nodes[j + len];
                    if c < tn.price {
                        *tn = RNode { price: c, len: len as u32, kind: 2, dist: r as u32, state: st_rep(st) as u8, reps: reps2 };
                    }
                }
            }
            // new distances (not needed when a repeat is already long enough)
            if longest.0 >= suff {
                cands.clear();
            } else {
                f.find_all(p, &mut cands, suff);
            }
            let base = m1 + bp(t, m.is_rep[st], 0);
            let lp = &pr.len[ps];
            let mut from = MIN_MATCH;
            for &(l, d) in &cands {
                let (l, d) = (l as usize, d as usize);
                if l > longest.0 + 1 {
                    longest = (l, d);
                }
                if nd.reps.contains(&(d as u32)) {
                    from = from.max(l + 1);
                    continue;
                }
                if l < suff {
                    let reps2 = [d as u32, nd.reps[0], nd.reps[1], nd.reps[2]];
                    for len in from..=l {
                        let c = base + lp[len] + pr.dist(d, len);
                        let tn = &mut nodes[j + len];
                        if c < tn.price {
                            *tn = RNode { price: c, len: len as u32, kind: 2, dist: d as u32, state: st_match(st) as u8, reps: reps2 };
                        }
                    }
                }
                from = from.max(l + 1);
            }
            f.insert(p);
            if longest.0 >= suff {
                stop = j;
                forced = Some(longest);
                break;
            }
        }
        // walk back from `stop` along the cheapest path, then code it in order
        path.clear();
        let mut j = stop;
        while j > 0 {
            let nd = nodes[j];
            j -= nd.len as usize;
            path.push((i + j, nd));
        }
        for &(p, nd) in path.iter().rev() {
            match nd.kind {
                0 => w.literal(p),
                1 => w.short_rep(p),
                _ => w.copy(p, nd.len as usize, nd.dist as usize),
            }
        }
        since += path.len();
        i += stop;
        if let Some((len, dist)) = forced {
            w.long_copy(i, len, dist);
            since += 1 + len / LEN_MAX;
            for k in i + 1..i + len {
                f.insert(k);
            }
            i += len;
        }
    }
    let mut out = vec![if dictb.is_empty() { 3u8 } else { 4u8 }];
    put_varint(&mut out, n as u64);
    out.push((RC_LC | (RC_PB << 4)) as u8);
    out.extend_from_slice(&w.rc.finish());
    out
}

// ================================================================ method 5: the fast adaptive coder
// The same idea as method 3 (copies and literals coded bit by bit with probabilities that
// learn as they go), arranged for speed:
// - the parse has no hash chains: one table of 8-byte keys (long copies) and one of 4-byte
//   keys with small buckets (short copies); copies are compared by a score that counts the
//   cost of the distance; one lazy step;
// - four independent range coders (literal-or-copy choices and lengths; distances; literals
//   at even places; literals at odd places), so the processor works on four chains at once;
// - 64-bit range coders with no branches per bit: they write out bytes only every few bits;
// - literals use the top bit of the byte before as context (in the numbers stream it says
//   "this byte continues a number"); no position bits; plain distance bits in one step.

// ---- the parse
/// A copy candidate: length, distance, repeat index (3 = a new distance) and score.
#[derive(Clone, Copy)]
struct Cand {
    len: usize,
    dist: usize,
    rk: usize,
    score: i32,
}

const NO_CAND: Cand = Cand { len: 0, dist: 0, rk: 3, score: i32::MIN };

/// About the bits a copy saves: 3 per byte, minus the bit length of a new distance
/// (a repeated distance is nearly free).
#[inline(always)]
fn cand_score(len: usize, dist: usize, rk: usize) -> i32 {
    3 * len as i32 - if rk < 3 { 0 } else { (usize::BITS - dist.leading_zeros()) as i32 }
}

#[inline(always)]
fn rd32(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(d[i..i + 4].try_into().unwrap())
}

#[inline(always)]
fn rd64(d: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(d[i..i + 8].try_into().unwrap())
}

/// Put a position at the front of a bucket (the oldest one falls out).
#[inline(always)]
fn push_slot<const W: usize>(e: &mut [u32; W], v: u32) {
    for k in (1..W).rev() {
        e[k] = e[k - 1];
    }
    e[0] = v;
}

/// Finds copies: an 8-byte-key table with one position per slot (long copies) and a 4-byte-key
/// table with buckets of the last W positions, newest first (short copies). Slots hold
/// position + 1 (0 = empty). A search reads at most 1 + W earlier positions.
///
/// A bucket is a ring: a new position is written over the oldest one at `head - 1` (no shift
/// of the other W - 1), and the bucket read newest first is short[b][head..W] then
/// short[b][..head]: the same entries in the same order as a bucket shifted at each add.
struct Scan<'a, const W: usize> {
    d: &'a [u8],
    long: Vec<u32>,
    short: Vec<[u32; W]>,
    head: Vec<u8>,
    sh_l: u32,
    sh_s: u32,
}

/// The 4 bytes at i (i + 4 <= d.len(), checked in debug builds).
#[inline(always)]
fn rd32u(d: &[u8], i: usize) -> u32 {
    debug_assert!(i + 4 <= d.len());
    u32::from_le(unsafe { (d.as_ptr().add(i) as *const u32).read_unaligned() })
}

/// The 8 bytes at i (i + 8 <= d.len(), checked in debug builds).
#[inline(always)]
fn rd64u(d: &[u8], i: usize) -> u64 {
    debug_assert!(i + 8 <= d.len());
    u64::from_le(unsafe { (d.as_ptr().add(i) as *const u64).read_unaligned() })
}

/// match_len for the fast parse (b < a, a + max <= d.len(): reads inside d; the same answer).
#[inline(always)]
fn match_len_u(d: &[u8], a: usize, b: usize, max: usize) -> usize {
    debug_assert!(b < a && a + max <= d.len());
    let mut n = 0;
    while n + 8 <= max {
        let x = rd64u(d, a + n) ^ rd64u(d, b + n);
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

impl<'a, const W: usize> Scan<'a, W> {
    fn new(d: &'a [u8]) -> Self {
        // about one slot per input byte, at most 2^20
        let bits = (usize::BITS - d.len().leading_zeros()).clamp(10, 20);
        let bs = bits - W.trailing_zeros();
        // (a slot holds position + 1 as a u32 and 0 means empty: with d.len() <= u32::MAX every
        // position put in (below d.len() - 8) gives a non-zero entry, which the bucket rings'
        // empty test (e[W - 1] == 0) and best()'s unchecked reads at entry - 1 rely on)
        assert!(W.is_power_of_two() && W <= 256 && d.len() >= 16 && d.len() <= u32::MAX as usize);
        Scan { d, long: vec![0; 1 << bits], short: vec![[0; W]; 1 << bs], head: vec![0; 1 << bs], sh_l: 64 - bits, sh_s: 32 - bs }
    }
    /// The 8 bytes at i and their slots in the two tables (i <= d.len() - 8).
    #[inline(always)]
    fn keys(&self, i: usize) -> (u64, usize, usize) {
        let v = rd64u(self.d, i);
        (v, (v.wrapping_mul(0x9E37_79B1_85EB_CA87) >> self.sh_l) as usize, ((v as u32).wrapping_mul(0x9E37_79B1) >> self.sh_s) as usize)
    }
    #[inline(always)]
    fn add(&mut self, i: usize, a: usize, b: usize) {
        // (a < long.len() and b < short.len(): keys() shifts by sh_l, sh_s)
        unsafe {
            *self.long.get_unchecked_mut(a) = i as u32 + 1;
            let h = self.head.get_unchecked_mut(b);
            let k = (*h as usize).wrapping_sub(1) & (W - 1);
            *h = k as u8;
            *self.short.get_unchecked_mut(b).get_unchecked_mut(k) = i as u32 + 1;
        }
    }
    /// The best copy at i by score: the repeated distances (a copy at the last distance may be
    /// 2-3 bytes long), the long-key slot, then the short-key bucket (i < d.len() - 8; every
    /// position in the tables is below i).
    #[inline(always)]
    fn best(&self, reps: &[usize; 3], i: usize, v: u64, a: usize, b: usize) -> Cand {
        let d = self.d;
        let n = d.len();
        let cur = v as u32;
        let mut best = NO_CAND;
        let take = |len: usize, dist: usize, rk: usize, best: &mut Cand| {
            let score = cand_score(len, dist, rk);
            if score > best.score {
                *best = Cand { len, dist, rk, score };
            }
        };
        for (k, &r) in reps.iter().enumerate() {
            if r > i {
                continue;
            }
            let x = cur ^ rd32u(d, i - r);
            if x == 0 {
                take(4 + match_len_u(d, i + 4, i + 4 - r, n - i - 4), r, k, &mut best);
            } else if k == 0 && x & 0xFFFF == 0 {
                take((x.trailing_zeros() / 8) as usize, r, 0, &mut best);
            }
        }
        let j = unsafe { *self.long.get_unchecked(a) } as usize;
        if j != 0 && rd64u(d, j - 1) == v {
            take(8 + match_len_u(d, i + 8, j + 7, n - i - 8), i + 1 - j, 3, &mut best);
        }
        // (b < short.len(); head < W)
        let e = unsafe { self.short.get_unchecked(b) };
        let h = unsafe { *self.head.get_unchecked(b) } as usize;
        let cand = |j: u32, best: &mut Cand| {
            let j = j as usize - 1;
            // a farther copy must be longer to win: it must also match the byte at the best
            // length (j < i, so j + need < i + need < n)
            let need = best.len.max(3);
            if rd32u(d, j) != cur || (need >= 4 && (need >= n - i || unsafe { *d.get_unchecked(j + need) != *d.get_unchecked(i + need) })) {
                return;
            }
            take(4 + match_len_u(d, i + 4, j + 4, n - i - 4), i - j, 3, best);
        };
        // newest first: e[h..], then e[..h]. The first position put in a bucket goes to
        // e[W - 1]: while that is 0 the bucket is empty; else e[h..] is full and e[..h] is
        // all empty (0) or all full
        if e[W - 1] == 0 {
            return best;
        }
        for &j in unsafe { e.get_unchecked(h..) } {
            cand(j, &mut best);
        }
        if e[0] != 0 {
            for &j in unsafe { e.get_unchecked(..h) } {
                cand(j, &mut best);
            }
        }
        best
    }
}

/// Positions added to the tables at each end of a long copy (all of a short one).
const P5_INS: usize = 16;
/// Inputs smaller than this get buckets of 8 (a deeper search is cheap there, and the small
/// streams of a log gain the most).
const P5_SMALL: usize = 256 << 10;

/// The parse of the fast coder at `level` (20-28): bucket size and lazy step. Levels 26-28
/// (buckets of 16-64 at every input size) are for small tables that pay for a deeper search.
pub(crate) fn parse5_level(d: &[u8], level: u32) -> Vec<Seq> {
    // (bucket size for inputs from P5_SMALL on, lazy step only after copies shorter than this)
    let (w, lazy_max) = match level {
        20 => (2, 0),
        21 => (2, 6),
        22 => (2, 24),
        23 => (2, usize::MAX),
        24 => (4, usize::MAX),
        26 => (16, usize::MAX),
        27 => (32, usize::MAX),
        28 => (64, usize::MAX),
        _ => (8, usize::MAX),
    };
    match if d.len() < P5_SMALL { w.max(8) } else { w } {
        2 => parse5::<2>(d, lazy_max),
        4 => parse5::<4>(d, lazy_max),
        16 => parse5::<16>(d, lazy_max),
        32 => parse5::<32>(d, lazy_max),
        64 => parse5::<64>(d, lazy_max),
        _ => parse5::<8>(d, lazy_max),
    }
}

/// Greedy or lazy parse: before taking a copy shorter than `lazy_max`, look one byte further;
/// a better copy there (by more than 2 points) makes this byte a literal.
fn parse5<const W: usize>(d: &[u8], lazy_max: usize) -> Vec<Seq> {
    let n = d.len();
    let mut seqs = Vec::new();
    if n < 16 {
        return seqs;
    }
    let mut f: Scan<W> = Scan::new(d);
    let lim = n - 8;
    let mut reps = [1usize, 4, 8];
    let (mut i, mut lit_start) = (0usize, 0usize);
    while i < lim {
        let (v, a, b) = f.keys(i);
        let mut m = f.best(&reps, i, v, a, b);
        f.add(i, a, b);
        if m.len == 0 {
            i += 1;
            continue;
        }
        while m.len < lazy_max && i + 1 < lim {
            let (v2, a2, b2) = f.keys(i + 1);
            let m2 = f.best(&reps, i + 1, v2, a2, b2);
            if m2.len == 0 || m2.score <= m.score.saturating_add(2) {
                break;
            }
            i += 1;
            m = m2;
            f.add(i, a2, b2);
        }
        seqs.push(Seq { ll: (i - lit_start) as u32, ml: m.len as u32, rk: m.rk as u8, dist: m.dist as u32 });
        reps = next_reps(reps, m.rk as u8, m.dist);
        // positions inside the copy: all of a short one, the first and last P5_INS of a long one
        let e = (i + m.len).min(lim);
        let (x1, x2) = if m.len <= 2 * P5_INS { (e, e) } else { ((i + 1 + P5_INS).min(e), e.saturating_sub(P5_INS).max(i + 1 + P5_INS)) };
        for k in (i + 1..x1).chain(x2..e) {
            let (_, a, b) = f.keys(k);
            f.add(k, a, b);
        }
        i += m.len;
        lit_start = i;
    }
    seqs
}

// ---- the 64-bit range coder
// low, range and code are 64-bit; a bit is coded without branches; the settled top bytes are
// written out only at "norm" points, at most 8 bits apart (a bit shrinks the range by at most
// 6 bits, so it stays far above 2^11 in between). A carry goes into the bytes written.

/// Encoder state; the output buffer is passed in, so that this state can stay in registers.
#[derive(Clone, Copy)]
struct Enc64 {
    low: u64,
    range: u64,
    pos: usize,
}

impl Enc64 {
    fn new() -> Enc64 {
        Enc64 { low: 0, range: u64::MAX, pos: 0 }
    }
    /// Add a carry into the bytes already written.
    #[cold]
    #[inline(never)]
    fn carry(out: &mut [u8], pos: usize) {
        let mut i = pos;
        while i > 0 {
            i -= 1;
            out[i] = out[i].wrapping_add(1);
            if out[i] != 0 {
                return;
            }
        }
    }
    #[inline(always)]
    fn bit(&mut self, out: &mut [u8], p: &mut u16, b: u32) {
        let pv = *p as u64;
        let bound = (self.range >> PBITS) * pv;
        let bm = 0u64.wrapping_sub(b as u64);
        let (low, c) = self.low.overflowing_add(bound & bm);
        self.low = low;
        if c {
            Enc64::carry(out, self.pos);
        }
        // the range becomes bound for a 0, range - bound for a 1
        self.range = bound.wrapping_add(self.range.wrapping_sub(bound).wrapping_sub(bound) & bm);
        *p = prob_next(pv as u32, bm as u32);
    }
    /// Write out the settled top bytes, so that the range is at least 2^56 again.
    #[inline(always)]
    fn norm(&mut self, out: &mut Vec<u8>) {
        if self.pos + 16 > out.len() {
            out.resize(out.len() * 2 + 64, 0);
        }
        let k = (self.range.leading_zeros() >> 3) as usize;
        out[self.pos..self.pos + 8].copy_from_slice(&self.low.to_be_bytes());
        self.pos += k;
        self.low <<= 8 * k;
        self.range <<= 8 * k;
    }
    /// `n` (at most 16) equally likely bits in one step (right after a norm).
    #[inline(always)]
    fn direct(&mut self, out: &mut [u8], v: u32, n: u32) {
        let r = self.range >> n;
        let (low, c) = self.low.overflowing_add(v as u64 * r);
        self.low = low;
        if c {
            Enc64::carry(out, self.pos);
        }
        self.range = r;
    }
    /// `nb` bits of v, high bit first, down a tree of probabilities.
    #[inline(always)]
    fn tree(&mut self, out: &mut [u8], probs: &mut [u16], nb: u32, v: u32) {
        let mut m = 1usize;
        for i in (0..nb).rev() {
            let b = (v >> i) & 1;
            self.bit(out, &mut probs[m], b);
            m = (m << 1) | b as usize;
        }
    }
    /// `nb` bits of v, low bit first.
    #[inline(always)]
    fn rev_tree(&mut self, out: &mut [u8], probs: &mut [u16], nb: u32, mut v: u32) {
        let mut m = 1usize;
        for _ in 0..nb {
            let b = v & 1;
            v >>= 1;
            self.bit(out, &mut probs[m], b);
            m = (m << 1) | b as usize;
        }
    }
    /// A literal: 8 bits down its tree, then a norm.
    #[inline(always)]
    fn byte(&mut self, out: &mut Vec<u8>, probs: &mut [u16; 0x100], c: u32) {
        let mut m = 1usize;
        for i in (0..8).rev() {
            let b = (c >> i) & 1;
            self.bit(out, &mut probs[m & 0xFF], b);
            m = (m << 1) | b as usize;
        }
        self.norm(out);
    }
    /// A copy length 2..=LEN_MAX: at most 5 bits, or 2 bits, a norm and 8 bits.
    #[inline(always)]
    fn len(&mut self, out: &mut Vec<u8>, lm: &mut LenModel, len: usize) {
        let l = (len - 2) as u32;
        if l < 8 {
            self.bit(out, &mut lm.choice, 0);
            self.tree(out, &mut lm.low[0], 3, l);
        } else if l < 16 {
            self.bit(out, &mut lm.choice, 1);
            self.bit(out, &mut lm.choice2, 0);
            self.tree(out, &mut lm.mid[0], 3, l - 8);
        } else {
            self.bit(out, &mut lm.choice, 1);
            self.bit(out, &mut lm.choice2, 1);
            self.norm(out);
            self.tree(out, &mut lm.high, 8, l - 16);
        }
    }
    /// The bytes. The decoder reads zeros past the end, so the coder ends on a value inside
    /// its last range with as many zero bytes at the end as possible, and leaves them out.
    fn finish(mut self, mut out: Vec<u8>) -> Vec<u8> {
        self.norm(&mut out);
        // the range is now at least 2^56: low rounded up to a multiple of 2^56 is inside it
        let (v, c) = self.low.overflowing_add((1 << 56) - 1);
        if c {
            Enc64::carry(&mut out, self.pos);
        }
        out.truncate(self.pos);
        out.extend_from_slice(&(v & !((1u64 << 56) - 1)).to_be_bytes());
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }
}

/// Decoder state: `code` is the coded value minus the encoder's low.
struct Dec64<'a> {
    b: &'a [u8],
    p: usize,
    code: u64,
    range: u64,
}

impl<'a> Dec64<'a> {
    fn new(b: &'a [u8]) -> Dec64<'a> {
        let mut d = Dec64 { b, p: 0, code: 0, range: u64::MAX };
        d.code = d.peek();
        d.p = 8;
        d
    }
    /// The next 8 input bytes, big-endian (zeros past the end).
    #[inline(always)]
    fn peek(&self) -> u64 {
        match self.b.get(self.p..self.p + 8) {
            Some(x) => u64::from_be_bytes(x.try_into().unwrap()),
            None => {
                let mut t = [0u8; 8];
                for (k, x) in t.iter_mut().enumerate() {
                    *x = self.b.get(self.p + k).copied().unwrap_or(0);
                }
                u64::from_be_bytes(t)
            }
        }
    }
    #[inline(always)]
    fn bit(&mut self, p: &mut u16) -> u32 {
        let pv = *p as u32;
        let bound = (self.range >> PBITS) * pv as u64;
        // the bit is hard to guess: selects, not branches
        let one = self.code >= bound;
        self.code = std::hint::select_unpredictable(one, self.code.wrapping_sub(bound), self.code);
        self.range = std::hint::select_unpredictable(one, self.range.wrapping_sub(bound), bound);
        *p = std::hint::select_unpredictable(one, pv - (pv >> PMOVE), pv + ((PONE as u32 - pv) >> PMOVE)) as u16;
        one as u32
    }
    #[inline(always)]
    fn norm(&mut self) {
        let k = self.range.leading_zeros() >> 3;
        let nxt = self.peek();
        // (nxt >> 1) >> (63 - 8k) is the top 8k bits of nxt, also for k = 0
        self.code = (self.code << (8 * k)) | ((nxt >> 1) >> (63 - 8 * k));
        self.range <<= 8 * k;
        self.p += k as usize;
    }
    #[inline(always)]
    fn direct(&mut self, n: u32) -> u32 {
        let r = self.range >> n;
        let v = (self.code / r).min((1 << n) - 1);
        self.code -= v * r;
        self.range = r;
        v as u32
    }
    #[inline(always)]
    fn tree(&mut self, probs: &mut [u16], nb: u32) -> u32 {
        let mut m = 1usize;
        for _ in 0..nb {
            m = (m << 1) | self.bit(&mut probs[m]) as usize;
        }
        (m - (1 << nb)) as u32
    }
    #[inline(always)]
    fn rev_tree(&mut self, probs: &mut [u16], nb: u32) -> u32 {
        let mut m = 1usize;
        let mut v = 0u32;
        for i in 0..nb {
            let b = self.bit(&mut probs[m]);
            m = (m << 1) | b as usize;
            v |= b << i;
        }
        v
    }
    /// A literal, and its top `lc` bits (known after its first `lc` bits: the next literal's
    /// context, so that it need not wait for the whole byte).
    #[inline(always)]
    fn byte(&mut self, probs: &mut [u16; 0x100], lc: u32) -> (u8, usize) {
        let mut m = 1usize;
        let mut top = 0;
        for i in 0..8 {
            m = (m << 1) | self.bit(&mut probs[m & 0xFF]) as usize;
            if i + 1 == lc {
                top = m - (1 << lc);
            }
        }
        self.norm();
        (m as u8, top)
    }
    #[inline(always)]
    fn len(&mut self, lm: &mut LenModel) -> usize {
        2 + if self.bit(&mut lm.choice) == 0 {
            self.tree(&mut lm.low[0], 3) as usize
        } else if self.bit(&mut lm.choice2) == 0 {
            8 + self.tree(&mut lm.mid[0], 3) as usize
        } else {
            self.norm();
            16 + self.tree(&mut lm.high, 8) as usize
        }
    }
}

// ---- the model and the two directions
/// Literal context bits: the top bit of the byte before.
const LC5: u32 = 1;

/// The probabilities of the fast coder.
struct Model5 {
    is_match: [u16; NSTATES],
    is_rep: [u16; NSTATES],
    is_rep0: [u16; NSTATES],
    is_rep1: [u16; NSTATES],
    is_rep2: [u16; NSTATES],
    is_rep0_long: [u16; NSTATES],
    lit: Vec<[u16; 0x100]>,
    len: LenModel,
    rep_len: LenModel,
    slot: [[u16; 64]; 4],
    special: [[u16; 32]; 10],
    align: [u16; 16],
}

impl Model5 {
    fn new(lc: u32) -> Model5 {
        Model5 {
            is_match: [P_INIT; NSTATES],
            is_rep: [P_INIT; NSTATES],
            is_rep0: [P_INIT; NSTATES],
            is_rep1: [P_INIT; NSTATES],
            is_rep2: [P_INIT; NSTATES],
            is_rep0_long: [P_INIT; NSTATES],
            lit: vec![[P_INIT; 0x100]; 1 << lc],
            len: LenModel::new(),
            rep_len: LenModel::new(),
            slot: [[P_INIT; 64]; 4],
            special: [[P_INIT; 32]; 10],
            align: [P_INIT; 16],
        }
    }
}

/// Code a parse with the fast coder: method 5, varint(length), the literal context bits,
/// varint(bytes) of the first three coders, then the four coders' bytes.
pub(crate) fn rc5_encode(data: &[u8], seqs: &[Seq]) -> Vec<u8> {
    let lc = LC5;
    let sh = 8 - lc;
    let mut m = Model5::new(lc);
    // c: literal-or-copy choices and copy lengths; dc: distances; l0 / l1: literals, taking turns
    let (mut c, mut dc, mut l0, mut l1) = (Enc64::new(), Enc64::new(), Enc64::new(), Enc64::new());
    let cap = data.len() / 4 + 64;
    let (mut oc, mut od, mut o0, mut o1) = (vec![0u8; cap], vec![0u8; cap / 2], vec![0u8; cap], vec![0u8; cap]);
    let mut state = 0usize;
    let mut reps: [usize; 4] = [1, 4, 8, 16];
    let mut odd = false;
    let mut zreps: [usize; 3] = [1, 4, 8];
    let mut p = 0usize;
    let mut k = 0;
    loop {
        // the literals before the next copy (or the last ones)
        let (ll, s) = if k < seqs.len() { (seqs[k].ll as usize, Some(seqs[k])) } else { (data.len() - p, None) };
        for q in p..p + ll {
            c.bit(&mut oc, &mut m.is_match[state], 0);
            c.norm(&mut oc);
            state = st_lit(state);
            let ctx = if q > 0 { ((data[q - 1] as u32) >> sh) as usize } else { 0 };
            if odd {
                l1.byte(&mut o1, &mut m.lit[ctx], data[q] as u32);
            } else {
                l0.byte(&mut o0, &mut m.lit[ctx], data[q] as u32);
            }
            odd = !odd;
        }
        p += ll;
        let Some(s) = s else { break };
        let dist = if s.rk < 3 { zreps[s.rk as usize] } else { s.dist as usize };
        zreps = next_reps(zreps, s.rk, s.dist as usize);
        let mut left = s.ml as usize;
        while left > 0 {
            // long copies go in steps of at most LEN_MAX (the later steps repeat the distance)
            let mut len = left.min(LEN_MAX);
            if left - len == 1 {
                len -= 1;
            }
            left -= len;
            let st = state;
            c.bit(&mut oc, &mut m.is_match[st], 1);
            if let Some(k) = reps.iter().position(|&r| r == dist) {
                c.bit(&mut oc, &mut m.is_rep[st], 1);
                if k == 0 {
                    c.bit(&mut oc, &mut m.is_rep0[st], 0);
                    c.bit(&mut oc, &mut m.is_rep0_long[st], 1);
                } else {
                    c.bit(&mut oc, &mut m.is_rep0[st], 1);
                    if k == 1 {
                        c.bit(&mut oc, &mut m.is_rep1[st], 0);
                    } else {
                        c.bit(&mut oc, &mut m.is_rep1[st], 1);
                        c.bit(&mut oc, &mut m.is_rep2[st], (k - 2) as u32);
                    }
                    // repeat k moves to the front
                    let r = reps[k];
                    for j in (1..=k).rev() {
                        reps[j] = reps[j - 1];
                    }
                    reps[0] = r;
                }
                c.norm(&mut oc);
                c.len(&mut oc, &mut m.rep_len, len);
                c.norm(&mut oc);
                state = st_rep(st);
            } else {
                c.bit(&mut oc, &mut m.is_rep[st], 0);
                c.norm(&mut oc);
                c.len(&mut oc, &mut m.len, len);
                c.norm(&mut oc);
                let d0 = (dist - 1) as u32;
                let slot = dist_slot(d0);
                dc.tree(&mut od, &mut m.slot[(len - 2).min(3)], 6, slot);
                dc.norm(&mut od);
                if slot >= 4 {
                    let footer = (slot >> 1) - 1;
                    let red = d0 - ((2 | (slot & 1)) << footer);
                    if slot < 14 {
                        dc.rev_tree(&mut od, &mut m.special[slot as usize - 4], footer, red);
                    } else {
                        // the plain bits above the 4 aligned ones, at most 16 per step
                        let (mut v, mut nb) = (red >> 4, footer - 4);
                        while nb > 16 {
                            nb -= 16;
                            dc.direct(&mut od, (v >> nb) & 0xFFFF, 16);
                            dc.norm(&mut od);
                            v &= (1 << nb) - 1;
                        }
                        dc.direct(&mut od, v, nb);
                        dc.norm(&mut od);
                        dc.rev_tree(&mut od, &mut m.align, 4, red & 15);
                    }
                    dc.norm(&mut od);
                }
                reps = [dist, reps[0], reps[1], reps[2]];
                state = st_match(st);
            }
        }
        p += s.ml as usize;
        k += 1;
    }
    let (bc, bd, b0, b1) = (c.finish(oc), dc.finish(od), l0.finish(o0), l1.finish(o1));
    let mut out = vec![5u8];
    put_varint(&mut out, data.len() as u64);
    out.push(lc as u8);
    put_varint(&mut out, bc.len() as u64);
    put_varint(&mut out, bd.len() as u64);
    put_varint(&mut out, b0.len() as u64);
    for x in [&bc, &bd, &b0, &b1] {
        out.extend_from_slice(x);
    }
    out
}

pub(crate) fn rc5_decode(b: &[u8], n: usize) -> Option<Vec<u8>> {
    let lc = *b.first()? as u32;
    if lc > 8 {
        return None;
    }
    let sh = 8 - lc;
    let mut p = 1;
    let nc = get_varint(b, &mut p)? as usize;
    let nd = get_varint(b, &mut p)? as usize;
    let n0 = get_varint(b, &mut p)? as usize;
    let bc = b.get(p..p + nc)?;
    let bd = b.get(p + nc..p + nc + nd)?;
    let b0 = b.get(p + nc + nd..p + nc + nd + n0)?;
    let b1 = b.get(p + nc + nd + n0..)?;
    let mut m = Model5::new(lc);
    let (mut rc, mut dc, mut l0, mut l1) = (Dec64::new(bc), Dec64::new(bd), Dec64::new(b0), Dec64::new(b1));
    let mut odd = false;
    // the output with 32 spare bytes at the end, so that copies can move 16 bytes at a time
    let mut out: Vec<u8> = vec![0; n + 32];
    let mut pos = 0usize;
    let mut state = 0usize;
    let mut reps: [usize; 4] = [1, 4, 8, 16];
    let mut ctx = 0usize; // the next literal's context: the top lc bits of the byte before
    while pos < n {
        if rc.bit(&mut m.is_match[state]) == 0 {
            rc.norm();
            let (c, top) = if odd { l1.byte(&mut m.lit[ctx], lc) } else { l0.byte(&mut m.lit[ctx], lc) };
            out[pos] = c;
            ctx = top;
            pos += 1;
            odd = !odd;
            state = st_lit(state);
            continue;
        }
        let len;
        if rc.bit(&mut m.is_rep[state]) == 1 {
            if rc.bit(&mut m.is_rep0[state]) == 0 {
                if rc.bit(&mut m.is_rep0_long[state]) == 0 {
                    return None; // one-byte repeats are not used by this coder
                }
            } else {
                // repeat 1, 2 or 3 moves to the front
                reps = if rc.bit(&mut m.is_rep1[state]) == 0 {
                    [reps[1], reps[0], reps[2], reps[3]]
                } else if rc.bit(&mut m.is_rep2[state]) == 0 {
                    [reps[2], reps[0], reps[1], reps[3]]
                } else {
                    [reps[3], reps[0], reps[1], reps[2]]
                };
            }
            rc.norm();
            len = rc.len(&mut m.rep_len);
            rc.norm();
            state = st_rep(state);
        } else {
            rc.norm();
            len = rc.len(&mut m.len);
            rc.norm();
            let slot = dc.tree(&mut m.slot[(len - 2).min(3)], 6);
            dc.norm();
            let d0 = if slot < 4 {
                slot
            } else {
                let footer = (slot >> 1) - 1;
                let base = (2 | (slot & 1)) << footer;
                let d = if slot < 14 {
                    base + dc.rev_tree(&mut m.special[slot as usize - 4], footer)
                } else {
                    let mut nb = footer - 4;
                    let mut hi = 0u32;
                    while nb > 16 {
                        nb -= 16;
                        hi = (hi << 16) | dc.direct(16);
                        dc.norm();
                    }
                    hi = (hi << nb) | dc.direct(nb);
                    dc.norm();
                    base.wrapping_add(hi << 4).wrapping_add(dc.rev_tree(&mut m.align, 4))
                };
                dc.norm();
                d
            };
            reps = [d0 as usize + 1, reps[0], reps[1], reps[2]];
            state = st_match(state);
        }
        let dist = reps[0];
        if pos + len > n || dist > pos {
            return None;
        }
        let src = pos - dist;
        if dist >= 16 {
            // 16 bytes at a time (may write up to 15 bytes past the copy: spare room)
            let mut k = 0;
            while k < len {
                out.copy_within(src + k..src + k + 16, pos + k);
                k += 16;
            }
        } else {
            for q in 0..len {
                out[pos + q] = out[src + q];
            }
        }
        pos += len;
        ctx = ((out[pos - 1] as u32) >> sh) as usize;
    }
    out.truncate(n);
    Some(out)
}
