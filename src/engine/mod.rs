//! engine: fewer-pack's log packer. One fixed setting: no levels, no modes.
//!
//! The rest of fewer-pack calls only these functions, so a better engine can replace this
//! folder as long as it keeps them:
//!
//!   pack(input, threads) -> payload        any bytes in (up to 4 GiB); never fails. The
//!                                          payload is at most 2 × input + 1 MiB (the
//!                                          container refuses to write a bigger segment).
//!   unpack_to(payload, threads, sink)      the exact bytes back, handed to `sink` a block
//!                                          at a time in order (each checked first), or
//!                                          Err(message) for a damaged payload; never
//!                                          panics, and its memory and time stay in
//!                                          proportion to the sizes the payload declares
//!   unpack(payload, threads) -> Ok(input)  the same, into one buffer
//!
//! A payload starts with an engine id byte (ENGINE_ID). An engine that writes a different
//! payload layout must use a new id; once archives have been given out, its `unpack` must
//! keep reading the older ids too (the archive format promises that).
//!
//! This file is the adapter between those functions and the engine in fl/ (the fast-lane
//! engine, copied in with mechanical edits only; fl/bridge.rs is the only code that reaches into
//! it).
//!
//! Packing:
//! 1. The input is cut into runs of lines, on all cores. Data that looks binary, lines over
//!    64 KiB and lines the engine is not made for (very many values, long runs of the hex
//!    letters a-f) go to plain blocks, and so do runs of lines that are not a log (log_like:
//!    text, code, tables, JSON documents); a run of up to 400,000 lines is one block, a longer
//!    run is cut every 100,000 lines (as the engine was measured), and a block of many
//!    different patterns with many values smaller still; a block whose lines would almost
//!    each be a pattern of their own (`explosive`, tried on 1,024 of its lines) is packed as
//!    plain bytes.
//! 2. The derive rules of each run of lines are found on a sample of it (as the engine finds
//!    them for a file). Every block is packed on its own core (the engine rewrites its lines
//!    without their derived fields when a rule applies, splits them into patterns and values
//!    and codes its streams); a block the engine cannot pack, or one that would break the
//!    opener's limits (fl::bridge::check_block, within_limits), or one that packs badly as
//!    lines, is kept as plain bytes (coded, or stored as they are when they look random).
//! 3. One pattern table and one word table for the payload: each block lists the entries it
//!    uses. Every stream, table and list is written with its length; every block with a
//!    CRC-32 of its text. With two lines blocks or more, a table bank: coder tables that
//!    several blocks' streams share, written once (table_bank).
//!
//! Opening checks every length and limit before it decodes anything, opens the blocks on all
//! cores into buffers used again for later blocks, hands them out in order, and runs the
//! engine's opener behind a guard: a damaged block gives an error (never a crash), and its
//! text must have the block's size and CRC.

mod fl;

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
#[cfg(not(panic = "abort"))]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::crc::crc32;
use fl::bridge as eng;
pub(crate) use fl::sort_stable_idx;

/// Payload format of this engine (first byte of every payload): 6 = format 2 of the final
/// engine with rules (fz coder, lean column headers, whole tokens, derived fields, number
/// slots given by rules of earlier slots; 11 streams per block), which adds: pattern ids in a
/// guessed form, ID columns (header flag 32), keyed slots (header flag 64), a table bank per
/// payload, and bgl / ctime fields the line printer computes. Engine 5
/// (format 1, fewer-pack 1.1.0) and the earlier ids are not read.
const ENGINE_ID: u8 = 6;

/// Receives each block in order: Some(its bytes), or None for a block that was skipped
/// (grep: it cannot match), with the block's size; returns false to stop.
pub type BlockSink<'a> = dyn FnMut(Option<&[u8]>, usize) -> Result<bool, String> + 'a;
/// Biggest payload this engine opens (a segment is at most 1 GiB).
const MAX_RAW: u64 = 1 << 32;
const KIND_LINES: u8 = 0;
const KIND_PLAIN: u8 = 1;

/// Lines longer than this are kept as plain bytes.
const LINE_MAX: usize = 64 << 10;
/// Lines longer than this are looked at closely (shorter ones cannot break the limits
/// below).
const LINE_LOOK: usize = 256;
/// A line goes to the engine with at most LINE_WORDS words (runs of digits, runs of letters:
/// every value is at least one) and words × length at most LINE_COST (the engine's work for a
/// new pattern grows with both), and no run of more than HEX_RUN of the letters a-f / A-F (the
/// engine's hex finder looks at such a run again from every letter in it).
const LINE_WORDS: usize = 1024;
const LINE_COST: usize = 16 << 20;
const HEX_RUN: usize = 256;
/// The first pass looks at the input in pieces of about this size, on all cores.
const PIECE: usize = 4 << 20;
/// A run of lines shorter than this between plain parts is kept plain too.
const MIN_RUN: usize = 64 << 10;
/// Most bytes in a block of lines.
const BLOCK_BYTES: usize = 64 << 20;
/// Most bytes in any block a reader accepts: a block of lines is cut after a line end (lines
/// are at most LINE_MAX bytes), so it may run up to a line past BLOCK_BYTES.
const MAX_BLOCK: usize = BLOCK_BYTES + 2 * LINE_MAX;
/// Most bytes in a plain block (big plain parts are coded on all cores: 4 MiB blocks pack
/// a 26 MB JSON file 17% faster on 14 cores than 16 MiB blocks, 1% bigger).
const PLAIN_BYTES: usize = 4 << 20;
/// A block's CRC is made on a core of its own (while the engine runs) from this size on, when
/// the block has more than one thread (pack_lines).
const CRC_ASIDE: usize = 1 << 20;
/// A block of lines whose estimated pattern work (see cost_parts) is above this is cut into
/// smaller blocks.
const COST_BUDGET: u64 = 64 << 20;
/// Most lines in a block a reader accepts.
const MAX_LINES: usize = if eng::ONE_MAX > eng::PER_BLOCK { eng::ONE_MAX } else { eng::PER_BLOCK };

// ---------------------------------------------------------------- small helpers

fn put_varint(o: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        o.push(v as u8 | 0x80);
        v >>= 7;
    }
    o.push(v as u8);
}

#[inline]
fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
#[inline]
fn unzz(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Fast hash for the table maps (multiply-rotate over 8 bytes at a time). A random start value
/// per run keeps crafted inputs from planning collisions; the output never depends on it.
#[derive(Clone, Copy)]
struct Fx(u64);
impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, b: &[u8]) {
        let mut h = self.0;
        let mut c = b.chunks_exact(8);
        for x in &mut c {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(x.try_into().unwrap())).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        for &x in c.remainder() {
            h = (h.rotate_left(5) ^ x as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        self.0 = h;
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}
#[derive(Clone, Copy)]
struct FxBuild(u64);
impl Default for FxBuild {
    // (not inlined: one copy for its every use)
    #[inline(never)]
    fn default() -> Self {
        static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        FxBuild(*SEED.get_or_init(|| std::collections::hash_map::RandomState::new().hash_one(0x9e37_79b9_u64)))
    }
}
impl BuildHasher for FxBuild {
    type Hasher = Fx;
    fn build_hasher(&self) -> Fx {
        Fx(self.0)
    }
}
type FxMap<K, V> = HashMap<K, V, FxBuild>;

/// Run f(i) for every i of `order`, in that order, on up to `threads` threads (one copy of
/// this code for every use: f is not generic).
#[cfg_attr(fewer_small, optimize(size))]
fn run_jobs(order: &[usize], threads: usize, f: &(dyn Fn(usize) + Sync)) {
    let t = threads.clamp(1, order.len().max(1));
    if t == 1 {
        order.iter().for_each(|&i| f(i));
        return;
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..t {
            s.spawn(|| {
                loop {
                    let q = next.fetch_add(1, Ordering::Relaxed);
                    if q >= order.len() {
                        break;
                    }
                    f(order[q]);
                }
            });
        }
    });
}

/// f(0..n) on up to `threads` threads, the jobs in `order`; results in job number order.
fn par_order<T: Send>(order: &[usize], threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let slots: Vec<Mutex<Option<T>>> = (0..order.len()).map(|_| Mutex::new(None)).collect();
    run_jobs(order, threads, &|i| *slots[i].lock().unwrap() = Some(f(i)));
    slots.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
}

/// f(0..n) on up to `threads` threads; results in order.
fn par_map<T: Send>(n: usize, threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let order: Vec<usize> = (0..n).collect();
    par_order(&order, threads, f)
}

/// par_map over jobs of the given sizes, biggest first (so the last ones to finish are
/// small); results in job order.
fn par_jobs<T: Send>(sizes: &[usize], threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    par_order(&biggest_first(sizes), threads, f)
}

fn biggest_first(sizes: &[usize]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    fl::sort_stable_idx(&mut order, &mut |&a, &b| sizes[a] > sizes[b]);
    order
}

/// How many threads to run `jobs` blocks on, given `threads`: all of them at once when there
/// are up to 1.5 times as many blocks as threads and `threads` is every core of the machine
/// (the system then shares the cores and the blocks end at about the same time, where a
/// second round would run the few blocks left over on few cores: Android's 15 blocks on 14
/// cores); else `threads`. With fewer threads than cores (-t), never more than `threads`.
fn workers(threads: usize, jobs: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if threads >= cores && jobs > threads && jobs <= threads + threads / 2 { jobs } else { threads }
}

/// First '\n' in b[from..to] (8 bytes at a time).
#[inline]
fn find_nl(b: &[u8], from: usize, to: usize) -> Option<usize> {
    let mut i = from;
    while i + 8 <= to {
        let x = u64::from_le_bytes(b[i..i + 8].try_into().unwrap()) ^ 0x0a0a_0a0a_0a0a_0a0a;
        let t = x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080;
        if t != 0 {
            return Some(i + (t.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    (i..to).find(|&k| b[k] == b'\n')
}

/// f(s, e) for every line input[s..e] of b[from..to] (e: the place of its '\n', or `to` for a
/// last line without one), in order: 64 bytes per step with NEON (an exact byte compare, one
/// bit per byte) or on x86_64 with SSE2 / AVX2, then 16 bytes per step, then (and on other
/// machines) find_nl.
#[inline(always)]
fn each_line(b: &[u8], from: usize, to: usize, mut f: impl FnMut(usize, usize)) {
    let (mut s, mut i) = (from, from);
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        assert!(to <= b.len());
        // SAFETY: NEON is always there on aarch64; every load is inside b[..to] (the 64-byte
        // loop loads b[i..i + 64] with i + 64 <= to, the 16-byte loop b[i..i + 16] with
        // i + 16 <= to); WEIGHTS is 16 bytes
        unsafe {
            let nl = vdupq_n_u8(b'\n');
            let p = b.as_ptr();
            // byte k of a 16-byte compare result kept as bit k % 8 of its byte
            const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            let wv = vld1q_u8(WEIGHTS.as_ptr());
            while i + 64 <= to {
                let q0 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i)), nl), wv);
                let q1 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i + 16)), nl), wv);
                let q2 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i + 32)), nl), wv);
                let q3 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i + 48)), nl), wv);
                // three rounds of pairwise adds (the weights of a byte's 8 inputs are distinct
                // bits, so each add is an OR): byte j of the low half = bits of bytes 8j..8j+7,
                // so bit k of the 64-bit mask = byte i + k is '\n'
                let r = vpaddq_u8(vpaddq_u8(q0, q1), vpaddq_u8(q2, q3));
                let mut m = vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(r, r)), 0);
                while m != 0 {
                    let e = i + m.trailing_zeros() as usize;
                    f(s, e);
                    s = e + 1;
                    m &= m - 1;
                }
                i += 64;
            }
            while i + 16 <= to {
                let eq = vceqq_u8(vld1q_u8(p.add(i)), nl);
                // 4 bits per byte (a narrowing shift), one kept: bit 4k + 3 = byte k is '\n'
                let mut m = vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(eq), 4)), 0) & 0x8888_8888_8888_8888;
                while m != 0 {
                    let e = i + (m.trailing_zeros() >> 2) as usize;
                    f(s, e);
                    s = e + 1;
                    m &= m - 1;
                }
                i += 16;
            }
        }
    }
    // x86_64: the same with SSE2 (64 bytes per step: AVX2 when the processor has it; then 16 per
    // step), fl/x86.rs
    #[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
    {
        assert!(to <= b.len());
        // SAFETY: each_nl reads b[k..k + 64] or b[k..k + 16] only while that end is <= to <=
        // b.len() (asserted), starting at k = i = from
        unsafe {
            fl::x86::each_nl(b.as_ptr(), &mut i, to, |e| {
                f(s, e);
                s = e + 1;
            })
        };
    }
    while let Some(e) = find_nl(b, i, to) {
        f(s, e);
        s = e + 1;
        i = s;
    }
    if s < to {
        f(s, to);
    }
}

/// Appends the place of every '\n' of b[from..to], less `base` (base <= from, to - base <=
/// u32::MAX), to `out`, in order: 64 bytes per step with NEON (the mask of each_line). A step
/// writes its first line end whether it has one or not and counts it only if it has (most
/// steps of a log hold none or one, so there is no branch on where lines end); only a step
/// with more line ends goes on in a loop. Then (and on other machines) find_nl. Makes room
/// for one value per byte first: for stretches of at most SCAN bytes.
#[inline(always)]
fn find_ends(b: &[u8], from: usize, to: usize, base: usize, out: &mut Vec<u32>) {
    let mut i = from;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        assert!(to <= b.len() && base <= from && from <= to && to - base <= u32::MAX as usize);
        out.reserve(to - from + 1);
        let mut len = out.len();
        let w = out.as_mut_ptr();
        // SAFETY: NEON is always there on aarch64; every load is inside b[..to] (b[i..i + 64]
        // with i + 64 <= to); WEIGHTS is 16 bytes. Writes: with len0 the length on entry, the
        // capacity is at least len0 + (to - from) + 1 (reserve), and every write is at
        // w[len] with len = len0 + the count of '\n' in b[from..j] for a j < to (the step's
        // first value, written before the step's count is added: j = i < to; the others, one
        // after each counted '\n' of the step: j < i + 64 <= to), so below len0 + (to - from)
        // + 1. set_len(len): every value counted into len was written at its place (and a value
        // written but not counted lies at len, past the new length).
        unsafe {
            let nl = vdupq_n_u8(b'\n');
            let p = b.as_ptr();
            const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            let wv = vld1q_u8(WEIGHTS.as_ptr());
            while i + 64 <= to {
                let q0 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i)), nl), wv);
                let q1 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i + 16)), nl), wv);
                let q2 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i + 32)), nl), wv);
                let q3 = vandq_u8(vceqq_u8(vld1q_u8(p.add(i + 48)), nl), wv);
                let r = vpaddq_u8(vpaddq_u8(q0, q1), vpaddq_u8(q2, q3));
                let mut m = vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(r, r)), 0);
                // i - base < 2^32 (above); with no '\n' the value written is i - base + 64,
                // which is not counted (and may wrap: never read)
                let at = (i - base) as u32;
                w.add(len).write(at.wrapping_add(m.trailing_zeros()));
                len += (m != 0) as usize;
                m &= m.wrapping_sub(1);
                while m != 0 {
                    w.add(len).write(at + m.trailing_zeros());
                    len += 1;
                    m &= m - 1;
                }
                i += 64;
            }
            out.set_len(len);
        }
    }
    while let Some(e) = find_nl(b, i, to) {
        out.push((e - base) as u32);
        i = e + 1;
    }
}

thread_local! {
    /// Guarded calls running on this thread.
    static GUARDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Calls into the engine run behind this guard: a panic (the engine meeting input it does
/// not check itself, such as a damaged stream) becomes None instead of a crash, and its
/// message is not printed (the caller reports an error, or packs the block another way).
/// This is the panic = "unwind" build; a panic = "abort" build has no
/// unwinding: there a panic ends the program with a clear error instead (see
/// container::install_panic_exit), and this guard only runs `f`.
#[cfg(panic = "abort")]
#[inline(always)]
fn guarded<T>(f: impl FnOnce() -> T) -> Option<T> {
    Some(f())
}
#[cfg(not(panic = "abort"))]
fn guarded<T>(f: impl FnOnce() -> T) -> Option<T> {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if GUARDED.with(|g| g.get()) == 0 {
                prev(info);
            }
        }));
    });
    GUARDED.with(|g| g.set(g.get() + 1));
    let r = catch_unwind(AssertUnwindSafe(f));
    GUARDED.with(|g| g.set(g.get() - 1));
    r.ok()
}

/// Checked reading of a byte string.
struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}
const DAMAGED: &str = "damaged data";
impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, p: 0 }
    }
    #[inline]
    fn varint(&mut self) -> Result<u64, &'static str> {
        let mut v = 0u64;
        let mut s = 0;
        loop {
            let x = *self.b.get(self.p).ok_or(DAMAGED)?;
            self.p += 1;
            if s > 63 {
                return Err(DAMAGED);
            }
            v |= ((x & 0x7f) as u64) << s;
            if x < 0x80 {
                return Ok(v);
            }
            s += 7;
        }
    }
    fn byte(&mut self) -> Result<u8, &'static str> {
        let x = *self.b.get(self.p).ok_or(DAMAGED)?;
        self.p += 1;
        Ok(x)
    }
    fn take(&mut self, n: u64) -> Result<&'a [u8], &'static str> {
        if n > (self.b.len() - self.p) as u64 {
            return Err(DAMAGED);
        }
        let s = &self.b[self.p..self.p + n as usize];
        self.p += n as usize;
        Ok(s)
    }
    /// varint(m) then m bytes (m = 0: an empty stream, no frame).
    fn frame(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.varint()?;
        self.take(n)
    }
    fn done(&self) -> bool {
        self.p == self.b.len()
    }
}

// ---------------------------------------------------------------- what goes where

/// Binary data (control bytes other than tab, line ends and escape), judged from up to 4
/// samples of 4 KiB: kept as plain bytes.
fn looks_binary(t: &[u8]) -> bool {
    const W: usize = 4096;
    let k = if t.len() <= 4 * W { 1 } else { 4 };
    let (mut bad, mut seen) = (0usize, 0usize);
    for i in 0..k {
        let s = if k == 1 { t } else { &t[i * (t.len() - W) / (k - 1)..][..W] };
        for &c in s {
            bad += (c < 0x09 || (0x0e..0x20).contains(&c) && c != 0x1b || c == 0x7f) as usize;
        }
        seen += s.len();
    }
    bad * 64 > seen
}

/// Data that will not pack (already compressed, such as a .gz, or encrypted): close to 8
/// bits per byte and almost no repeats, judged from up to 8 samples of 64 KB.
fn looks_random(b: &[u8]) -> bool {
    const WIN: usize = 1 << 16;
    const WINS: usize = 8;
    if b.len() < 4096 {
        return false;
    }
    let mut hist = [0u64; 256];
    // where each 4-byte hash was seen last (position + 1, 0 = never)
    let mut last = vec![0u32; 1 << 14];
    let (mut repeats, mut seen) = (0usize, 0usize);
    let k = if b.len() <= WIN * WINS { 1 } else { WINS };
    for w in 0..k {
        let x = if k == 1 {
            b
        } else {
            let s = w * (b.len() - WIN) / (k - 1);
            &b[s..s + WIN]
        };
        last.fill(0);
        for i in 0..x.len() - 3 {
            let v = u32::from_le_bytes([x[i], x[i + 1], x[i + 2], x[i + 3]]);
            let h = (v.wrapping_mul(0x9E37_79B1) >> 18) as usize;
            let p = last[h] as usize;
            if p > 0 && x[p - 1..p + 3] == x[i..i + 4] {
                repeats += 1;
            }
            last[h] = (i + 1) as u32;
        }
        for &c in x {
            hist[c as usize] += 1;
        }
        seen += x.len();
    }
    let n = seen as f64;
    let bits: f64 = hist.iter().filter(|&&c| c > 0).map(|&c| -(c as f64 / n) * (c as f64 / n).log2()).sum();
    bits > 7.8 && repeats * 200 < seen
}

/// Byte classes for line_ok and cost_parts: 1 digit, 2 hex letter (a-f, A-F), 3 other
/// letter, 0 anything else.
static CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        t[i] = if c.is_ascii_digit() {
            1
        } else if matches!(c, b'a'..=b'f' | b'A'..=b'F') {
            2
        } else if c.is_ascii_alphabetic() {
            3
        } else {
            0
        };
        i += 1;
    }
    t
};

/// High bit of every byte of x that is a digit / a letter / one of a-f, A-F (8 bytes at a
/// time: the "has between" trick, exact per byte).
#[inline(always)]
fn class_masks(x: u64) -> (u64, u64, u64) {
    const L: u64 = !0 / 255;
    let between = |x: u64, m: u64, n: u64| {
        let t = x & (L * 127);
        (L * (127 + n)).wrapping_sub(t) & !x & (t + L * (127 - m)) & (L * 128)
    };
    let low = x | (L * 0x20);
    (between(x, b'0' as u64 - 1, b'9' as u64 + 1), between(low, b'a' as u64 - 1, b'z' as u64 + 1), between(low, b'a' as u64 - 1, b'f' as u64 + 1))
}

/// Whether a line (without its '\n') goes to the engine: see LINE_MAX, LINE_WORDS, LINE_COST
/// and HEX_RUN. Lines of up to LINE_LOOK bytes always do; longer ones are read 8 bytes at a
/// time (the words are only counted in lines over LINE_WORDS bytes: shorter ones cannot
/// have more words than bytes).
fn line_ok(l: &[u8]) -> bool {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let n = l.len();
    if n <= LINE_LOOK {
        return true;
    }
    if n > LINE_MAX {
        return false;
    }
    let count = n > LINE_WORDS;
    let (mut words, mut run, mut longest) = (0usize, 0usize, 0usize);
    let (mut pd, mut pa) = (0u64, 0u64); // the previous word's digit and letter masks
    let mut ch = l.chunks_exact(8);
    for w in &mut ch {
        let (d, a, h) = class_masks(u64::from_le_bytes(w.try_into().unwrap()));
        if count {
            // runs of digits and runs of letters that start in these 8 bytes
            words += (d & !(d << 8 | pd >> 56)).count_ones() as usize + (a & !(a << 8 | pa >> 56)).count_ones() as usize;
            (pd, pa) = (d, a);
        }
        // hex letter runs across words (a run inside 8 bytes is short anyway)
        if h == HIGH {
            run += 8;
        } else {
            longest = longest.max(run + (!h & HIGH).trailing_zeros() as usize / 8);
            run = (!h & HIGH).leading_zeros() as usize / 8;
        }
    }
    longest = longest.max(run);
    let mut prev = ((pd >> 63) as u8) | ((pa >> 63) as u8) << 1;
    for &c in ch.remainder() {
        let k = CLASS[c as usize];
        // 1: digit, 2: letter
        let g = if k == 1 { 1 } else { (k >= 2) as u8 * 2 };
        words += (count && g != 0 && g != prev) as usize;
        prev = g;
        run = if k == 2 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest <= HEX_RUN && words <= LINE_WORDS && words * n <= LINE_COST
}

/// A part of the input to pack: a run of lines (one or more blocks) or plain bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Part {
    a: usize,
    b: usize,
    lines: bool,
}

/// A block to pack: its part of the input, and (lines) the run of lines it comes from
/// (Plan::samples).
#[derive(Clone, Copy, Debug)]
struct Planned {
    part: Part,
    run: usize,
}

/// The blocks of the input, and for each run of lines the lines its derive rules are found
/// on (fl::bridge::sample_runs: as the engine samples a whole file).
struct Plan<'a> {
    blocks: Vec<Planned>,
    samples: Vec<Vec<&'a [u8]>>,
    /// Every '\n' of the pieces that are not binary data (so of every block of lines), in
    /// order, from the first pass; None when a piece has too many (DENSE) or the input has
    /// more than u32::MAX bytes (pack_part then finds the lines again: lines_of).
    ends: Option<Vec<u32>>,
}

/// A part found by the first pass, with (lines parts) its line count and where every
/// ANCHOR-th line starts (line number in the part, byte position), so that blocks can be cut
/// at exact line numbers without counting lines again.
struct Found {
    part: Part,
    count: usize,
    anchors: Vec<(usize, usize)>,
}

const ANCHOR: usize = 1024;

/// Add input[s..e] (a lines part with `count` lines and its anchors, or plain bytes) after
/// the parts found so far, joined to the last one when it is of the same kind.
fn add_part(parts: &mut Vec<Found>, s: usize, e: usize, lines: bool, count: usize, anchors: Vec<(usize, usize)>) {
    if s == e {
        return;
    }
    match parts.last_mut() {
        Some(f) if f.part.lines == lines && f.part.b == s => {
            f.part.b = e;
            let n = f.count;
            f.anchors.extend(anchors.into_iter().map(|(i, o)| (i + n, o)));
            f.count += count;
        }
        _ => parts.push(Found { part: Part { a: s, b: e, lines }, count, anchors }),
    }
}

/// The first pass reads a piece SCAN bytes at a time (a multiple of 64): the line ends of each
/// stretch are found first (find_ends), then its lines are looked at.
const SCAN: usize = 64 << 10;
/// A piece keeps its line ends for pack_part (lines_from) while it has at most one per DENSE
/// bytes (4 bytes each: at most an eighth of the piece's size).
const DENSE: usize = 32;

/// The parts of input[a..b] (a and b at line starts, or the input's ends): runs of lines the
/// engine takes, and plain bytes (binary data, lines it does not take); and the place of every
/// '\n' of input[a..b] less a, in order (none for binary data, which has no lines part), or
/// None when there are more than about one per DENSE bytes.
fn scan_piece(input: &[u8], a: usize, b: usize) -> (Vec<Found>, Option<Vec<u32>>) {
    let mut parts = Vec::new();
    if looks_binary(&input[a..b]) {
        add_part(&mut parts, a, b, false, 0, Vec::new());
        return (parts, Some(Vec::new()));
    }
    let mut r = Run { parts, run: a, count: 0, anchors: Vec::new() };
    let (mut ends, mut keep) = (Vec::new(), true);
    let (mut s, mut i) = (a, a);
    while i < b {
        let to = if b - i > SCAN { i + SCAN } else { b };
        let new = if keep {
            ends.len()
        } else {
            ends.clear();
            0
        };
        find_ends(input, i, to, a, &mut ends);
        for &e in &ends[new..] {
            let e = a + e as usize;
            r.line(input, b, s, e);
            s = e + 1;
        }
        if keep && ends.len() > (b - a) / DENSE + 64 {
            (ends, keep) = (Vec::with_capacity(SCAN + 1), false);
        }
        i = to;
    }
    if s < b {
        r.line(input, b, s, b);
    }
    add_part(&mut r.parts, r.run, b, true, r.count, r.anchors);
    (r.parts, keep.then_some(ends))
}

/// scan_piece's parts so far, and the lines part being made: input[run..], `count` lines.
struct Run {
    parts: Vec<Found>,
    run: usize,
    count: usize,
    anchors: Vec<(usize, usize)>,
}

impl Run {
    /// The next line input[s..e] (e: its '\n', or b, the piece's end). Lines of up to LINE_LOOK
    /// bytes (line_ok without a call) are counted here.
    #[inline(always)]
    fn line(&mut self, input: &[u8], b: usize, s: usize, e: usize) {
        if e - s <= LINE_LOOK || line_ok(&input[s..e]) {
            if self.count % ANCHOR == 0 {
                self.anchors.push((self.count, s));
            }
            self.count += 1;
        } else {
            self.plain(s, (e + 1).min(b));
        }
    }

    /// A line the engine does not take, input[s..next]: the lines part ends before it.
    #[cold]
    #[inline(never)]
    fn plain(&mut self, s: usize, next: usize) {
        add_part(&mut self.parts, self.run, s, true, self.count, std::mem::take(&mut self.anchors));
        add_part(&mut self.parts, s, next, false, 0, Vec::new());
        self.run = next;
        self.count = 0;
    }
}

/// Where line number `g` of a lines part starts.
fn line_start(input: &[u8], f: &Found, g: usize) -> usize {
    let k = f.anchors.partition_point(|&(i, _)| i <= g) - 1;
    let (mut i, mut o) = f.anchors[k];
    while i < g {
        o = find_nl(input, o, f.part.b).map_or(f.part.b, |p| p + 1);
        i += 1;
    }
    o
}

/// The lines derive::detect looks at in a run of lines (fl::bridge::sample_runs), found from
/// the run's anchors.
fn run_sample<'a>(input: &'a [u8], f: &Found) -> Vec<&'a [u8]> {
    let mut v = Vec::new();
    for (g, k) in eng::sample_runs(f.count) {
        let mut s = line_start(input, f, g);
        for _ in 0..k {
            if s >= f.part.b {
                break;
            }
            let e = find_nl(input, s, f.part.b).unwrap_or(f.part.b);
            v.push(&input[s..e]);
            s = e + 1;
        }
    }
    v
}

/// The blocks of the whole input. The first pass runs on all cores (pieces of about equal
/// size, cut after a line end); its parts are joined, short runs of lines between plain
/// parts become plain, and then:
/// - a run of up to ONE_MAX lines is one block, a longer run is cut every PER_BLOCK lines
///   (as the engine was measured), and a block over BLOCK_BYTES into blocks of about equal
///   size (cut after a line end),
/// - plain parts are cut into blocks of at most PLAIN_BYTES.
fn plan(input: &[u8], threads: usize) -> Plan<'_> {
    let n = input.len();
    if n == 0 {
        return Plan { blocks: Vec::new(), samples: Vec::new(), ends: None };
    }
    // pieces of a fixed size (not one per thread): the blocks never depend on the thread count
    let want = PIECE;
    let mut cuts = vec![0usize];
    while n - cuts.last().unwrap() > want {
        let t = cuts.last().unwrap() + want;
        match find_nl(input, t, (t + (1 << 20)).min(n)) {
            Some(p) if p + 1 < n => cuts.push(p + 1),
            _ => cuts.push(t),
        }
    }
    cuts.push(n);
    let pieces = par_map(cuts.len() - 1, threads, |i| scan_piece(input, cuts[i], cuts[i + 1]));
    // the line ends of all pieces, as places in the input (they fit in u32 when n does)
    let total: Option<usize> = pieces.iter().map(|(_, e)| e.as_ref().map(|e| e.len())).sum();
    let mut ends = total.filter(|_| n <= u32::MAX as usize).map(Vec::with_capacity);
    let mut all: Vec<Found> = Vec::new();
    for (k, (pc, e)) in pieces.into_iter().enumerate() {
        for f in pc {
            add_part(&mut all, f.part.a, f.part.b, f.part.lines, f.count, f.anchors);
        }
        if let (Some(all_ends), Some(e)) = (&mut ends, e) {
            // cuts[k] + x < n <= u32::MAX
            let at = cuts[k] as u32;
            all_ends.extend(e.iter().map(|&x| at + x));
        }
    }
    // short runs of lines next to plain parts go plain too
    if all.len() > 1 && all.iter().any(|f| f.part.lines && f.part.b - f.part.a < MIN_RUN) {
        let mut joined: Vec<Found> = Vec::with_capacity(all.len());
        for f in all {
            let lines = f.part.lines && f.part.b - f.part.a >= MIN_RUN;
            let anchors = if lines { f.anchors } else { Vec::new() };
            add_part(&mut joined, f.part.a, f.part.b, lines, f.count, anchors);
        }
        all = joined;
    }
    // blocks
    let mut out = Vec::new();
    let mut samples = Vec::new();
    for f in &all {
        let p = f.part;
        // a run of lines that is not a log (text, code, tables, JSON documents: lines that
        // rarely share a shape) is packed as plain bytes, as fast as the coder runs
        let sample = if p.lines { run_sample(input, f) } else { Vec::new() };
        if !p.lines || !log_like(&sample) {
            let k = (p.b - p.a).div_ceil(PLAIN_BYTES);
            out.extend((0..k).map(|i| Planned { part: Part { a: p.a + i * (p.b - p.a) / k, b: p.a + (i + 1) * (p.b - p.a) / k, lines: false }, run: usize::MAX }));
            continue;
        }
        let run = samples.len();
        samples.push(sample);
        let mut bounds = vec![p.a];
        if f.count > eng::ONE_MAX {
            bounds.extend((1..f.count.div_ceil(eng::PER_BLOCK)).map(|j| line_start(input, f, j * eng::PER_BLOCK)));
        }
        bounds.push(p.b);
        for w in bounds.windows(2) {
            let (a, b) = (w[0], w[1]);
            let k = (b - a).div_ceil(BLOCK_BYTES);
            let mut s = a;
            for i in 1..=k {
                let t = a + i * (b - a) / k;
                let e = if i == k { b } else { find_nl(input, t.max(s), b).map_or(b, |q| q + 1) };
                if e > s {
                    out.push(Planned { part: Part { a: s, b: e, lines: true }, run });
                    s = e;
                }
            }
        }
    }
    Plan { blocks: out, samples, ends }
}

/// Whether a run of lines is a log, from the sample its derive rules are found on: the lines
/// of a log share a few shapes (its messages, the line without its digits and hex letters),
/// those of text, code, tables and JSON documents rarely do. Up to 4 pieces of 256 lines
/// spread over the sample: a log when at most LOG_SHAPES of their lines have a shape not
/// seen before in their piece (measured: the 16 test logs 1% to 61%, Android the most;
/// prose, code, CSV, word lists and JSON documents 76% to 100%). Samples of fewer than
/// 256 lines always count as logs (the engine is cheap on them).
fn log_like(sample: &[&[u8]]) -> bool {
    const PIECE_LINES: usize = 256;
    const LOG_SHAPES: f64 = 0.7;
    let n = sample.len();
    if n < PIECE_LINES {
        return true;
    }
    let k = if n < 4 * PIECE_LINES { 1 } else { 4 };
    let (mut seen, mut new) = (0usize, 0usize);
    let mut shapes: Vec<u64> = Vec::with_capacity(PIECE_LINES);
    for i in 0..k {
        let a = i * n / k;
        let piece = &sample[a..(a + PIECE_LINES.max(n / k * (k == 1) as usize)).min(n)];
        shapes.clear();
        for l in piece {
            let mut h = 0x243f_6a88_85a3_08d3u64;
            for &c in &l[..l.len().min(256)] {
                if CLASS[c as usize] == 0 || CLASS[c as usize] == 3 {
                    h = (h.rotate_left(5) ^ c as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
                }
            }
            shapes.push(h);
        }
        seen += shapes.len();
        fl::sort_stable_by(&mut shapes, |a, b| a < b);
        shapes.dedup();
        new += shapes.len();
    }
    (new as f64) <= LOG_SHAPES * seen as f64
}

/// The lines of input[a..b] (b just after a '\n', or the input's end), without their '\n'.
fn lines_of(input: &[u8], a: usize, b: usize) -> Vec<&[u8]> {
    let mut v = Vec::with_capacity((b - a) / 64 + 16);
    each_line(input, a, b, |s, e| v.push(&input[s..e]));
    v
}

/// lines_of(input, a, b) from the line ends of the first pass (Plan::ends: every '\n' of
/// input[a..b] is in `ends`, in order), without reading the bytes again.
fn lines_from<'a>(input: &'a [u8], a: usize, b: usize, ends: &[u32]) -> Vec<&'a [u8]> {
    let lo = ends.partition_point(|&e| (e as usize) < a);
    let hi = lo + ends[lo..].partition_point(|&e| (e as usize) < b);
    let mut v = Vec::with_capacity(hi - lo + 1);
    let mut s = a;
    v.extend(ends[lo..hi].iter().map(|&e| {
        let l = &input[s..e as usize];
        s = e as usize + 1;
        l
    }));
    if s < b {
        v.push(&input[s..b]);
    }
    v
}

/// lines_of on up to `threads` threads: input[a..b] cut after line ends into pieces of at least
/// PAR_PIECE bytes, the lines of each piece found side by side, then joined in order (the same
/// lines: every cut is at a line start).
fn lines_of_par(input: &[u8], a: usize, b: usize, threads: usize) -> Vec<&[u8]> {
    let k = threads.min((b - a) / PAR_PIECE);
    if k <= 1 {
        return lines_of(input, a, b);
    }
    let mut cuts = vec![a];
    for i in 1..k {
        let last = *cuts.last().unwrap();
        match find_nl(input, (a + i * (b - a) / k).max(last), b) {
            Some(p) if p + 1 < b => cuts.push(p + 1),
            _ => break,
        }
    }
    cuts.push(b);
    let pieces = par_map(cuts.len() - 1, threads, |i| lines_of(input, cuts[i], cuts[i + 1]));
    let mut v = Vec::with_capacity(pieces.iter().map(|p| p.len()).sum());
    for p in &pieces {
        v.extend_from_slice(p);
    }
    v
}

/// Smallest piece of a block's text, or of its picked lines' count (cost_parts_par), that is
/// worth a thread of its own.
const PAR_PIECE: usize = 1 << 20;
const PAR_LINES: usize = 4096;

/// cost_parts on up to `threads` threads: the picked lines cut into runs of at least PAR_LINES,
/// each run's first lines with a new shape found side by side, then counted in line order once
/// per shape (the same estimate: a shape counts the line where it is first seen).
fn cost_parts_par(lines: &[&[u8]], threads: usize) -> usize {
    let step = cost_step(lines.len());
    let picked = lines.len().div_ceil(step);
    let k = threads.min(picked / PAR_LINES);
    if k <= 1 {
        return cost_parts(lines);
    }
    let firsts: Vec<Vec<(u64, u64)>> = par_map(k, threads, |c| {
        let mut seen: FxMap<u64, ()> = FxMap::default();
        let mut out = Vec::new();
        for j in c * picked / k..(c + 1) * picked / k {
            let l = lines[j * step];
            let st = cost_line(l);
            if st.runs > 0 && seen.insert(st.h, ()).is_none() {
                out.push((st.h, st.runs * l.len() as u64 / 2 * step as u64));
            }
        }
        out
    });
    let mut seen: FxMap<u64, ()> = FxMap::default();
    let mut est = 0u64;
    for (h, c) in firsts.into_iter().flatten() {
        if seen.insert(h, ()).is_none() {
            est += c;
        }
    }
    est.div_ceil(COST_BUDGET).max(1) as usize
}

/// Which lines cost_parts looks at: every step-th.
fn cost_step(n: usize) -> usize {
    match n {
        0..=4096 => 1,
        4097..=65536 => 8,
        _ => 32,
    }
}

/// Into how many blocks a block of lines should be cut so that each needs at most
/// COST_BUDGET of the engine's pattern work. The engine copies and hashes the pattern text
/// before every value of every new pattern, so a block of many different patterns with many
/// values costs about values × length / 2 per pattern. Estimated from every line of a block
/// of up to 4,096 lines, every 8th line up to 65,536 lines, and every 32nd line of bigger
/// blocks (at least 2,048 lines; every 8th cost 5% of the packing time): a line whose text
/// without digits and hex letters (about its pattern) was not seen yet counts its digit runs
/// × length / 2.
fn cost_parts(lines: &[&[u8]]) -> usize {
    let step = cost_step(lines.len());
    let mut seen: FxMap<u64, ()> = FxMap::default();
    let mut est = 0u64;
    for l in lines.iter().step_by(step) {
        let st = cost_line(l);
        if st.runs > 0 && seen.insert(st.h, ()).is_none() {
            est += st.runs * l.len() as u64 / 2 * step as u64;
        }
    }
    est.div_ceil(COST_BUDGET).max(1) as usize
}

/// cost_parts' reading of one line: CostState::byte over its bytes from CostState::NEW, 8 bytes
/// at a time (class_masks): the digit runs counted from the digit mask, and the hash run over
/// the bytes that are neither digits nor hex letters only, in order.
fn cost_line(l: &[u8]) -> CostState {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut st = CostState::NEW;
    let mut pd = 0u64; // the digit mask of the 8 bytes before
    let mut ch = l.chunks_exact(8);
    for w in &mut ch {
        let x = u64::from_le_bytes(w.try_into().unwrap());
        let (d, _, hex) = class_masks(x);
        st.runs += (d & !(d << 8 | pd >> 56)).count_ones() as u64;
        pd = d;
        let mut keep = !(d | hex) & HIGH;
        while keep != 0 {
            st.hash((x >> (keep.trailing_zeros() & !7)) as u8);
            keep &= keep - 1;
        }
    }
    // (CostState::byte only asks whether the byte before is a digit)
    st.prev = (pd >> 63) as u8;
    for &c in ch.remainder() {
        st.byte(c);
    }
    st
}

/// cost_parts' reading of one line: the hash of its text without digits and hex letters, its
/// digit runs, and the class of the byte before.
#[derive(Clone, Copy)]
struct CostState {
    h: u64,
    runs: u64,
    prev: u8,
}

impl CostState {
    const NEW: CostState = CostState { h: 0x243f_6a88_85a3_08d3, runs: 0, prev: 0 };

    #[inline(always)]
    fn byte(&mut self, c: u8) {
        let k = CLASS[c as usize];
        self.runs += (k == 1 && self.prev != 1) as u64;
        self.prev = k;
        if k != 1 && k != 2 {
            self.hash(c);
        }
    }

    #[inline(always)]
    fn hash(&mut self, c: u8) {
        self.h = (self.h.rotate_left(5) ^ c as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

// ---------------------------------------------------------------- settings

/// The engine reads developer switches from the environment (fl::bridge::ENV_SWITCHES);
/// some change how blocks are written or read without the payload saying so. fewer-pack clears
/// them first thing, so archives never depend on the environment. Call before any other
/// thread starts (main does).
#[cfg_attr(fewer_small, optimize(size))]
pub fn clear_env_switches() {
    for (k, _) in std::env::vars_os() {
        let b = k.as_encoded_bytes();
        if eng::ENV_SWITCHES.iter().any(|&p| b.starts_with(p.as_bytes())) {
            // SAFETY: called before any other thread runs (see above)
            unsafe { std::env::remove_var(&k) };
        }
    }
}

// ---------------------------------------------------------------- pack

/// A packed block.
struct BlockOut {
    a: usize,
    b: usize,
    crc: u32,
    body: Body,
}

enum Body {
    /// a frame holding the block's bytes
    Plain(Vec<u8>),
    /// lines: whether the text ends with '\n', the line count, the coded frame of every
    /// stream (TABLES streams: empty here), the block's own tables (TABLES streams) and the
    /// parsed input of each stream fz coded (for table_bank, which drops them)
    Lines { ends_nl: bool, lines: usize, frames: Vec<Vec<u8>>, tables: Vec<Vec<u8>>, parsed: Vec<Option<eng::Parsed>> },
}

/// Bytes coded with the engine's column coder; stored as they are should the coder ever
/// fail (packing never fails).
fn code_or_store(t: &[u8]) -> Vec<u8> {
    guarded(|| eng::code(t)).unwrap_or_else(|| eng::store(t))
}

/// A table or list coded with the engine's table coder (stored should it fail).
fn table_or_store(t: &[u8]) -> Vec<u8> {
    guarded(|| eng::code_table(t)).unwrap_or_else(|| eng::store(t))
}

/// The opener's limits for a block of `lines` lines whose text (with a final '\n') is `size`
/// bytes, as the frames declare them before anything is decoded: each stream (the pattern
/// ids: a varint per line; the column header), each list of table entries, all of a block's
/// streams and lists together, and each table rebuilt from the payload's table and the
/// block's list. They bound the memory and time of opening a block, whatever it declares, to
/// a few times its size (the 16 test logs use at most 0.18 x size per stream, 0.24 x size
/// all together); the packer keeps a block over one as plain bytes (within_limits).
fn stream_max(k: usize, lines: usize, size: usize) -> usize {
    if eng::TABLES.contains(&k) {
        2 * size + 16
    } else if k == eng::IDS {
        10 * lines + 16
    } else if k == eng::HEADER {
        size + 1024
    } else {
        4 * size + 1024
    }
}
fn streams_max(size: usize) -> u64 {
    6 * size as u64 + (64 << 10)
}
fn table_max(size: usize) -> usize {
    2 * size + 64
}

/// Whether a packed block's streams stay within the opener's limits (stream_max ...); its
/// lists are not made yet: each takes at most 10 bytes per entry.
fn within_limits(streams: &[Vec<u8>], lines: usize, size: usize) -> bool {
    let mut total = 0u64;
    for (k, s) in streams.iter().enumerate() {
        let n = if eng::TABLES.contains(&k) {
            let list = 10 * s.iter().filter(|&&c| c == b'\n').count();
            if s.len() > table_max(size) || list > stream_max(k, lines, size) {
                return false;
            }
            list
        } else if s.len() > stream_max(k, lines, size) {
            return false;
        } else {
            s.len()
        };
        total += n as u64;
    }
    total <= streams_max(size)
}

/// A block coded as plain bytes: stored as it is when it will not pack (fast for
/// compressed files), else coded.
fn plain_frame(text: &[u8]) -> Vec<u8> {
    if looks_random(text) { eng::store(text) } else { code_or_store(text) }
}

fn plain_block(input: &[u8], a: usize, b: usize) -> BlockOut {
    let t = &input[a..b];
    BlockOut { a, b, crc: crc32(t), body: Body::Plain(plain_frame(t)) }
}

/// input[a..b] as plain blocks of at most PLAIN_BYTES (cut as `plan` cuts plain parts),
/// coded on up to `threads` threads.
fn plain_blocks(input: &[u8], a: usize, b: usize, threads: usize) -> Vec<BlockOut> {
    let k = (b - a).div_ceil(PLAIN_BYTES).max(1);
    par_map(k, threads, |i| plain_block(input, a + i * (b - a) / k, a + (i + 1) * (b - a) / k))
}

/// Pack the lines input[a..b] as one block (as plain blocks when the engine cannot, when the
/// block breaks the opener's limits, or when that is smaller). `one`: the payload's only lines
/// block (its bytes are known exactly: lines_cost).
#[allow(clippy::too_many_arguments)]
fn pack_lines(input: &[u8], a: usize, b: usize, lines: &[&[u8]], rules: &eng::Rules, threads: usize, alone: bool, mk: Option<bool>, one: bool) -> Vec<BlockOut> {
    let text = &input[a..b];
    let ends_nl = text.last() == Some(&b'\n');
    let size = text.len() + !ends_nl as usize;
    // the block packed with the rules `r` (`hook`: the tables hook) and its frames settled:
    // an empty stream is written as no frame at all (length 0), and the pattern ids take
    // another form (format 2) where that codes smaller (the form kept goes to the table bank
    // with its parsed input, as the engine's code_streams does); with est, the block's
    // estimated bytes (its tables, coded once per payload, counted at a third; the pattern
    // ids in their plain form)
    let engine = |r: &eng::Rules, hook: fn(&[u8])| {
        guarded(|| {
            let mut blk = eng::with_tables_hook(hook, || eng::pack_block_as(input, lines, r, threads, alone, mk));
            if blk.lines() != lines.len() {
                return None;
            }
            // the opener's limits: with threads to spare and a big block, checked on a thread of
            // their own while the streams are coded (the frames are kept only when the block
            // passes)
            let ok = || eng::check_block(blk.streams(), lines.len(), size).is_ok() && within_limits(blk.streams(), lines.len(), size);
            let (ok, coded) = if threads > 1 && size >= CRC_ASIDE {
                std::thread::scope(|s| {
                    let c = s.spawn(ok);
                    let f = eng::code_streams(&blk, threads);
                    (c.join().unwrap_or(false), Some(f))
                })
            } else {
                (ok(), None)
            };
            if !ok {
                return None;
            }
            let (mut frames, mut parsed): (Vec<Vec<u8>>, Vec<Option<eng::Parsed>>) = coded.unwrap_or_else(|| eng::code_streams(&blk, threads)).into_iter().unzip();
            let tables: Vec<Vec<u8>> = eng::TABLES.iter().map(|&k| blk.take_stream(k)).collect();
            for (k, f) in frames.iter_mut().enumerate() {
                if eng::TABLES.contains(&k) || eng::declared(f) == Some(0) {
                    f.clear();
                }
            }
            let est = frames.iter().map(|f| f.len()).sum::<usize>() + tables.iter().map(|t| t.len()).sum::<usize>() / 3;
            for form in &blk.take_alt() {
                let c = eng::code_parsed(form);
                if c.0.len() < frames[eng::IDS].len() {
                    (frames[eng::IDS], parsed[eng::IDS]) = c;
                }
            }
            Some((frames, tables, parsed, est))
        })
        .flatten()
    };
    // the block's CRC: when the block has cores of its own (a file of few blocks) and is big,
    // on one of them while the engine runs (its one-thread phases leave the others idle);
    // the same value either way
    let (packed, crc) = if threads > 1 && text.len() >= CRC_ASIDE {
        std::thread::scope(|s| {
            let c = s.spawn(|| crc32(text));
            let p = engine(rules, early_hook);
            (p, c.join().unwrap_or_else(|_| crc32(text)))
        })
    } else {
        (engine(rules, early_hook), crc32(text))
    };
    let Some((mut frames, mut tables, mut parsed, mut est)) = packed else {
        return plain_blocks(input, a, b, threads);
    };
    // fix3: a block of bgl lines (B5's form, format 2) that is the payload's only lines block,
    // or any such block of an input of at most BOTH_MAX bytes, is packed in the old form too
    // (no fill marker; side stream bits 2 | 8), and the form of fewer bytes kept (lines_cost:
    // exact for the only lines block; ties keep B5's form). B5's fill marker makes every bgl
    // pattern a byte longer, which its side stream's saving (0 to 3 bytes) does not always
    // pay for. Not handed to the tables hook: the early table then no longer matches when the
    // old form is kept, and `pack` makes the table again (the same bytes).
    if one || input.len() <= BOTH_MAX {
        if let Some(o) = rules.old_bgl().and_then(|r| engine(&r, |_| ())) {
            if lines_cost(text.len(), lines.len(), &o.0, &o.1) < lines_cost(text.len(), lines.len(), &frames, &tables) {
                (frames, tables, parsed, est) = o;
            }
        }
    }
    // a block that packs badly as lines is kept as plain bytes when that is smaller (est)
    // (the estimate can be wrong either way: plain bytes only when they are smaller than the
    // block's exact bytes with its tables coded alone too; the payload's only lines block by
    // those exact bytes alone, and a small one always tried: its varints, CRC and coded tables
    // are much of it, which the estimate leaves out)
    if est * 6 > text.len() || (one && text.len() <= ONE_TRY) {
        let p = plain_blocks(input, a, b, threads);
        let plain: usize = p.iter().map(|o| if let Body::Plain(f) = &o.body { f.len() } else { 0 }).sum();
        if (one || plain < est) && plain_cost(&p) < lines_cost(text.len(), lines.len(), &frames, &tables) {
            return p;
        }
    }
    vec![BlockOut { a, b, crc, body: Body::Lines { ends_nl, lines: lines.len(), frames, tables, parsed } }]
}

/// The payload's only lines block is tried as plain bytes too up to this size (pack_lines).
const ONE_TRY: usize = 64 << 10;
/// A block of bgl lines that shares its payload with other lines blocks is packed in both
/// forms (pack_lines, fix3) when the input is at most this size (in a bigger input it keeps
/// B5's form: so the 16 test logs' BGL blocks, 20 in one payload, pack as before).
const BOTH_MAX: usize = 4 << 20;

/// Bytes of v as a varint.
fn varint_len(v: usize) -> usize {
    let mut o = Vec::new();
    put_varint(&mut o, v as u64);
    o.len()
}

/// The bytes plain blocks take in a payload (as `pack` writes them).
fn plain_cost(p: &[BlockOut]) -> usize {
    p.iter()
        .map(|o| if let Body::Plain(f) = &o.body { varint_len(o.b - o.a) + 1 + varint_len(f.len()) + f.len() + 4 } else { 0 })
        .sum()
}

/// The bytes a lines block of `len` bytes of text takes in a payload (as `pack` writes it) when it is
/// the payload's only lines block: its frames, and its tables and their lists coded as `pack`
/// codes them (exact for a payload of one block; with other lines blocks, their entries are
/// shared and cost less).
fn lines_cost(len: usize, lines: usize, frames: &[Vec<u8>], tables: &[Vec<u8>]) -> usize {
    let mut c = varint_len(len) + 2 + varint_len(lines) + varint_len(frames.len()) + 4;
    for (k, f) in frames.iter().enumerate() {
        if !eng::TABLES.contains(&k) {
            c += varint_len(f.len()) + f.len();
        }
    }
    for t in tables {
        let mut map: FxMap<&[u8], u32> = FxMap::default();
        let (mut table, mut list, mut prev) = (Vec::new(), Vec::new(), 0i64);
        for e in entries(t) {
            let n = map.len() as u32;
            let id = *map.entry(e).or_insert_with(|| {
                table.extend_from_slice(e);
                table.push(b'\n');
                n
            });
            put_varint(&mut list, zz(id as i64 - prev));
            prev = id as i64;
        }
        for d in [&table, &list] {
            let n = if d.is_empty() { 0 } else { table_or_store(d).len() };
            c += varint_len(n) + n;
        }
    }
    c
}

/// Blocks the engine is tried on first (explosive): at least PROBE_LINES lines and
/// PROBE_BYTES bytes; the engine packs PROBE_RUNS runs of PROBE_RUN lines spread over them.
const PROBE_LINES: usize = 4096;
const PROBE_BYTES: usize = 4 << 20;
const PROBE_RUNS: usize = 4;
const PROBE_RUN: usize = 256;

/// Whether the engine would make a pattern of almost every line of a block (lines that each
/// carry a different word or shape, such as "w123qhgjiab 482573 ..."): it then keeps the
/// columns of every pattern, 40 to 80 bytes of memory per byte of the block (4 GB for 64
/// MB), for an archive at most about 20% smaller than plain bytes (often not smaller at all),
/// so such a block is packed as plain bytes. Found by packing PROBE_RUNS runs of PROBE_RUN
/// lines spread over the block (the same lines whatever the thread count): more than 90% of
/// them their own pattern (the 16 test logs: at most 53%, Android; such shapes: 99.8% and
/// more).
fn explosive(input: &[u8], a: usize, b: usize, lines: &[&[u8]], rules: &eng::Rules, alone: bool, mk: Option<bool>) -> bool {
    let n = lines.len();
    if n < PROBE_LINES || b - a < PROBE_BYTES {
        return false;
    }
    let mut probe: Vec<&[u8]> = Vec::with_capacity(PROBE_RUNS * PROBE_RUN);
    for k in 0..PROBE_RUNS {
        let s = k * (n - PROBE_RUN) / (PROBE_RUNS - 1);
        probe.extend_from_slice(&lines[s..s + PROBE_RUN]);
    }
    let many = |p: usize| p * 10 > probe.len() * 9;
    // the patterns are at most pass 1's distinct shapes (eng::patterns_max, a part of the
    // packing): the rest of the packing runs only when those are many, or when pack_block's
    // memory guard might decline the probe (a panic: not explosive, as the packing's)
    match guarded(|| eng::patterns_max(input, &probe, rules, mk, probe.len() * 9 / 10)) {
        None => return false,
        Some((p, sure)) if sure && !many(p) => return false,
        Some(_) => {}
    }
    // a probe the engine declines (pack_block's memory guard: lines() is not the probe's) counts as
    // explosive
    let patterns = guarded(|| {
        let b = eng::pack_block_as(input, &probe, rules, 1, alone, mk);
        if b.lines() != probe.len() { usize::MAX } else { b.streams()[0].iter().filter(|&&c| c == b'\n').count() }
    });
    patterns.is_some_and(|p| p == usize::MAX || many(p))
}

/// Pack one part: plain bytes, or a block of lines (cut further when its pattern work would
/// be too big; plain when it looks binary or is explosive), with the derive rules of its run.
/// `alone`: the only part of lines of the input (no other block packs beside its blocks).
fn pack_part(input: &[u8], p: Part, rules: &eng::Rules, threads: usize, alone: bool, ends: Option<&[u32]>) -> Vec<BlockOut> {
    if !p.lines {
        return vec![plain_block(input, p.a, p.b)];
    }
    let lines = match ends {
        Some(e) => lines_from(input, p.a, p.b, e),
        None => lines_of_par(input, p.a, p.b, threads),
    };
    let k = cost_parts_par(&lines, threads).min(lines.len());
    let mut out = Vec::with_capacity(k);
    let mut a = p.a;
    for i in 0..k {
        let ls = &lines[i * lines.len() / k..(i + 1) * lines.len() / k];
        let b = if i + 1 == k { p.b } else { a + ls.iter().map(|l| l.len() + 1).sum::<usize>() };
        // whether the block's text holds an MK byte (the derive rewrite's marker): looked for
        // once, for the probe and the packing (the probe's lines span the whole block)
        let mk = rules.rewrite().then(|| eng::mk_in(&input[a..b]));
        if explosive(input, a, b, ls, rules, alone, mk) {
            out.extend(plain_blocks(input, a, b, threads));
        } else {
            out.extend(pack_lines(input, a, b, ls, rules, threads, alone, mk, alone && k == 1));
        }
        a = b;
    }
    out
}

/// The entries of a table: each ended by '\n'.
fn entries(t: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = t.strip_suffix(b"\n").unwrap_or(&[]);
    body.split(|&c| c == b'\n').take(if t.is_empty() { 0 } else { usize::MAX })
}

/// One table for the payload from the blocks' own tables (`own`, one per block in order, its
/// entries each ended by '\n'): every entry once, in order of first use; and each block's list
/// of its entries' numbers in the table (varints of the differences, zigzagged).
fn join_table<'a>(own: impl Iterator<Item = &'a [u8]>) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut j = Joiner::default();
    let lists = own.map(|t| j.add(t)).collect();
    (j.table, lists)
}

/// join_table one block at a time (in order): the table so far, and the entries in it.
#[derive(Default)]
struct Joiner<'a> {
    map: FxMap<&'a [u8], u32>,
    table: Vec<u8>,
}

impl<'a> Joiner<'a> {
    /// The next block's own table: its entries go into the table (the new ones), and its list.
    fn add(&mut self, own: &'a [u8]) -> Vec<u8> {
        let (mut l, mut prev) = (Vec::new(), 0i64);
        for e in entries(own) {
            let n = self.map.len() as u32;
            let table = &mut self.table;
            let id = *self.map.entry(e).or_insert_with(|| {
                table.extend_from_slice(e);
                table.push(b'\n');
                n
            });
            put_varint(&mut l, zz(id as i64 - prev));
            prev = id as i64;
        }
        l
    }
}

/// The payload's pattern table (table 0: the blocks' stream 0), joined and coded on a core of
/// its own while the blocks are still packed. The engine hands each block's pattern table out
/// before the block's pass 2 (fl::bridge::with_tables_hook); a thread takes them part by part
/// in order, joins them (join_table) and codes the table. On many cores this was the last step
/// and ran on one core (Android: a 3.8 MB table, about 40% of its packing time on 14 cores).
/// `pack` uses the result only when the blocks of lines came out with exactly these tables,
/// one block per part (else it makes the table again after the blocks, as without this), so
/// the payload's bytes never depend on it.
struct Early {
    parts: Vec<(Mutex<EarlyPart>, std::sync::Condvar)>,
}

#[derive(Default)]
struct EarlyPart {
    /// the pattern tables the part's blocks handed out, in order
    tables: Vec<Vec<u8>>,
    /// the part is packed: no more tables come
    done: bool,
}

/// What the early join made: the tables it joined (one per part that handed one out, in
/// order), the coded table, and the list of each of those blocks.
type EarlyTable = (Vec<Vec<u8>>, Vec<u8>, Vec<Vec<u8>>);

thread_local! {
    /// The Early of the payload being packed and the part this thread packs (set per part).
    static EARLY: std::cell::RefCell<Option<(std::sync::Arc<Early>, usize)>> = const { std::cell::RefCell::new(None) };
}

/// fl's tables hook (pack_lines): hands the block's pattern table to the part's Early.
fn early_hook(t: &[u8]) {
    EARLY.with(|e| {
        if let Some((e, i)) = &*e.borrow() {
            let (m, c) = &e.parts[*i];
            m.lock().unwrap_or_else(|p| p.into_inner()).tables.push(t.to_vec());
            c.notify_all();
        }
    });
}

/// While alive: this thread packs part `i` of `e`; when dropped (also on a panic), the part is
/// done.
struct InPart<'a>(&'a Early, usize);

impl Drop for InPart<'_> {
    fn drop(&mut self) {
        EARLY.with(|e| *e.borrow_mut() = None);
        let (m, c) = &self.0.parts[self.1];
        m.lock().unwrap_or_else(|p| p.into_inner()).done = true;
        c.notify_all();
    }
}

/// When dropped: every part of the Early is done.
struct AllDone<'a>(&'a Early);

impl Drop for AllDone<'_> {
    fn drop(&mut self) {
        for (m, c) in &self.0.parts {
            m.lock().unwrap_or_else(|p| p.into_inner()).done = true;
            c.notify_all();
        }
    }
}

impl Early {
    fn new(n: usize) -> std::sync::Arc<Early> {
        std::sync::Arc::new(Early { parts: (0..n).map(|_| (Mutex::new(EarlyPart::default()), std::sync::Condvar::new())).collect() })
    }

    fn enter(self: &std::sync::Arc<Early>, i: usize) -> InPart<'_> {
        EARLY.with(|e| *e.borrow_mut() = Some((self.clone(), i)));
        InPart(self, i)
    }

    /// Takes each part in order (its first table, or its end without one; waiting for it),
    /// joins each table as it comes (so that little is left to join when the last one comes),
    /// then codes the table.
    fn table(&self) -> EarlyTable {
        // (each part's table is kept in a slot of its own, set once: the joiner's entries point
        // into the slots set before while later ones are set)
        let slots: Vec<std::sync::OnceLock<Vec<u8>>> = self.parts.iter().map(|_| std::sync::OnceLock::new()).collect();
        let mut j = Joiner::default();
        let mut lists = Vec::new();
        for ((m, c), slot) in self.parts.iter().zip(&slots) {
            let t = {
                let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
                while g.tables.is_empty() && !g.done {
                    g = c.wait(g).unwrap_or_else(|p| p.into_inner());
                }
                g.tables.first_mut().map(std::mem::take)
            };
            if let Some(t) = t {
                let _ = slot.set(t);
                if let Some(t) = slot.get() {
                    lists.push(j.add(t));
                }
            }
        }
        let frame = if j.table.is_empty() { Vec::new() } else { table_or_store(&j.table) };
        drop(j);
        (slots.into_iter().filter_map(|s| s.into_inner()).collect(), frame, lists)
    }
}

/// Pack any bytes into one payload.
pub fn pack(input: &[u8], threads: usize) -> Vec<u8> {
    let threads = threads.max(1);
    // 1. the parts and the derive rules of each run of lines, then every block on all cores
    //    (biggest first; a part's helpers share the cores when there are fewer parts than
    //    cores)
    let plan = plan(input, threads);
    let rules: Vec<eng::Rules> = par_map(plan.samples.len(), threads, |r| guarded(|| eng::detect(&plan.samples[r])).unwrap_or_default());
    let parts = &plan.blocks;
    let per = (threads / parts.len().max(1)).max(1);
    // a single part of lines packs with no other block beside it (more room for its shapes)
    let alone = parts.iter().filter(|p| p.part.lines).count() == 1;
    let sizes: Vec<usize> = parts.iter().map(|p| p.part.b - p.part.a).collect();
    // the payload's pattern table is joined and coded meanwhile, on a thread of its own (Early)
    let early = Early::new(parts.len());
    let (mut blocks, early0): (Vec<BlockOut>, Option<EarlyTable>) = std::thread::scope(|s| {
        // (with one thread, none: -t 1 packs on one core; nor for a small input, whose table costs
        // less to code at the end than a thread of its own)
        let co = (threads > 1 && input.len() >= CRC_ASIDE && parts.iter().any(|p| p.part.lines)).then(|| s.spawn(|| early.table()));
        let on = co.is_some();
        // every part counts as done once the jobs end, even when one panicked (unwind builds): the
        // early join never waits for a part that will not come
        let _all = AllDone(&early);
        let blocks = par_jobs(&sizes, workers(threads, parts.len()), |i| {
            let p = parts[i];
            // (the blocks hand their tables out only while the early join runs: no copies else)
            let _in = on.then(|| early.enter(i));
            pack_part(input, p.part, rules.get(p.run).unwrap_or(&eng::Rules::default()), per, alone, plan.ends.as_deref())
        })
        .into_iter()
        .flatten()
        .collect();
        (blocks, co.and_then(|h| h.join().ok()))
    });
    drop(plan);

    // 1b. the table bank (two lines blocks or more)
    let bank = table_bank(&mut blocks);

    // 2. one table of each kind for the payload: each block lists the entries it uses
    let nt = eng::TABLES.len();
    let any_lines = blocks.iter().any(|b| matches!(b.body, Body::Lines { .. }));
    let own = |t: usize| {
        blocks.iter().map(move |b| match &b.body {
            Body::Lines { tables, .. } => &tables[t][..],
            Body::Plain(_) => &[][..],
        })
    };
    // the early pattern table holds when the blocks of lines, in order, have exactly the
    // tables it joined
    let mut early0 = early0.filter(|(used, _, _)| {
        nt > 0 && eng::TABLES[0] == 0 && blocks.iter().filter(|b| matches!(b.body, Body::Lines { .. })).count() == used.len() && {
            let mut u = used.iter();
            blocks.iter().all(|b| match &b.body {
                Body::Lines { tables, .. } => u.next().is_some_and(|x| *x == tables[0]),
                Body::Plain(_) => true,
            })
        }
    });
    let mut tables: Vec<Vec<u8>> = vec![Vec::new(); nt];
    let mut lists: Vec<Vec<Vec<u8>>> = vec![vec![Vec::new(); nt]; blocks.len()];
    let mut early_frame = None;
    for t in 0..nt {
        let l: Vec<Vec<u8>> = match early0.take().filter(|_| t == 0) {
            Some((_, frame, el)) => {
                early_frame = Some(frame);
                // the lists of the blocks of lines; a plain block's stays empty
                let mut el = el.into_iter();
                blocks.iter().map(|b| if matches!(b.body, Body::Lines { .. }) { el.next().unwrap_or_default() } else { Vec::new() }).collect()
            }
            None => {
                let (table, l) = join_table(own(t));
                tables[t] = table;
                l
            }
        };
        for (bi, x) in l.into_iter().enumerate() {
            lists[bi][t] = x;
        }
    }
    // the tables and the lists, coded on all cores (the pattern table already is when the early
    // one holds)
    let mut jobs: Vec<(usize, usize)> = (0..nt).filter(|&t| !(t == 0 && early_frame.is_some())).map(|t| (usize::MAX, t)).collect();
    for (bi, b) in blocks.iter().enumerate() {
        if matches!(b.body, Body::Lines { .. }) {
            jobs.extend((0..nt).map(|t| (bi, t)));
        }
    }
    let data_of = |(bi, t): (usize, usize)| -> &[u8] { if bi == usize::MAX { &tables[t] } else { &lists[bi][t] } };
    let jsizes: Vec<usize> = jobs.iter().map(|&j| data_of(j).len()).collect();
    let coded: Vec<Vec<u8>> = par_jobs(&jsizes, threads, |i| {
        let d = data_of(jobs[i]);
        if d.is_empty() { Vec::new() } else { table_or_store(d) }
    });
    let mut coded = coded.into_iter();
    let table_frames: Vec<Vec<u8>> = (0..nt)
        .map(|t| {
            let e = if t == 0 { early_frame.take() } else { None };
            e.unwrap_or_else(|| coded.next().unwrap_or_default())
        })
        .collect();
    let mut list_frames: Vec<Vec<Vec<u8>>> = Vec::with_capacity(blocks.len());
    for b in &blocks {
        list_frames.push(if matches!(b.body, Body::Lines { .. }) { coded.by_ref().take(nt).collect() } else { Vec::new() });
    }

    // 3. the payload
    let total: usize = blocks
        .iter()
        .map(|b| match &b.body {
            Body::Plain(f) => f.len() + 16,
            Body::Lines { frames, .. } => frames.iter().map(|f| f.len() + 5).sum::<usize>() + 24,
        })
        .sum::<usize>()
        + list_frames.iter().flatten().map(|f| f.len() + 5).sum::<usize>()
        + table_frames.iter().map(|f| f.len() + 5).sum::<usize>()
        + 32;
    let mut out = Vec::with_capacity(total);
    out.push(ENGINE_ID);
    put_varint(&mut out, input.len() as u64);
    put_varint(&mut out, blocks.len() as u64);
    if any_lines {
        // (the table bank, when there is one, as one more frame after the tables)
        put_varint(&mut out, (nt + !bank.is_empty() as usize) as u64);
        for f in table_frames.iter().chain((!bank.is_empty()).then_some(&bank)) {
            put_varint(&mut out, f.len() as u64);
            out.extend_from_slice(f);
        }
    } else {
        put_varint(&mut out, 0);
    }
    for (bi, b) in blocks.iter().enumerate() {
        put_varint(&mut out, (b.b - b.a) as u64);
        match &b.body {
            Body::Plain(f) => {
                out.push(KIND_PLAIN);
                put_varint(&mut out, f.len() as u64);
                out.extend_from_slice(f);
            }
            Body::Lines { ends_nl, lines, frames, .. } => {
                out.push(KIND_LINES);
                out.push(*ends_nl as u8);
                put_varint(&mut out, *lines as u64);
                put_varint(&mut out, frames.len() as u64);
                for (k, f) in frames.iter().enumerate() {
                    let f = match eng::TABLES.iter().position(|&t| t == k) {
                        Some(t) => &list_frames[bi][t],
                        None => f,
                    };
                    put_varint(&mut out, f.len() as u64);
                    out.extend_from_slice(f);
                }
            }
        }
        out.extend_from_slice(&b.crc.to_le_bytes());
    }
    out
}

/// The payload's table bank (eng::table_bank), from the column streams of its lines blocks when
/// there are two or more (their frames replaced where it makes them smaller). Returns the
/// bank's bytes (empty for none). The parsed inputs are dropped.
fn table_bank(blocks: &mut [BlockOut]) -> Vec<u8> {
    let n = blocks.iter().filter(|b| matches!(b.body, Body::Lines { .. })).count();
    let mut fr: Vec<(&mut Vec<u8>, usize, &eng::Parsed)> = Vec::new();
    for b in blocks.iter_mut() {
        if let Body::Lines { frames, parsed, .. } = &mut b.body {
            for (k, (f, p)) in frames.iter_mut().zip(parsed.iter()).enumerate() {
                if let Some(p) = p {
                    fr.push((f, k, p));
                }
            }
        }
    }
    let bank = if n > 1 { guarded(|| eng::table_bank(&mut fr)).unwrap_or_default() } else { Vec::new() };
    drop(fr);
    for b in blocks.iter_mut() {
        if let Body::Lines { parsed, .. } = &mut b.body {
            *parsed = Vec::new();
        }
    }
    bank
}

// ---------------------------------------------------------------- unpack

/// A block as found in the payload (not decoded yet).
struct BlockRef<'a> {
    raw: usize,
    kind: u8,
    ends_nl: bool,
    lines: usize,
    frames: Vec<&'a [u8]>,
    crc: u32,
}

/// A payload table, opened: entry i is bytes[starts[i]..starts[i + 1] - 1].
struct Table {
    bytes: Vec<u8>,
    starts: Vec<u32>,
}

impl Table {
    fn open(t: Vec<u8>) -> Result<Table, &'static str> {
        if t.last().is_some_and(|&c| c != b'\n') || t.len() >= u32::MAX as usize {
            return Err(DAMAGED);
        }
        // entries are all different, so at most one is empty (and there are at most about
        // half as many entries as bytes)
        let mut starts = vec![0u32];
        let mut empty = 0;
        let mut s = 0;
        while s < t.len() {
            let e = find_nl(&t, s, t.len()).unwrap_or(t.len());
            empty += (e == s) as usize;
            if empty > 1 {
                return Err(DAMAGED);
            }
            s = e + 1;
            starts.push(s as u32);
        }
        Ok(Table { bytes: t, starts })
    }
    fn len(&self) -> usize {
        self.starts.len() - 1
    }
    /// A block's own table from its list of entries (as differences), at most `max` bytes.
    fn rebuild(&self, list: &[u8], max: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::new();
        let mut r = Rd::new(list);
        let mut id = 0i64;
        while !r.done() {
            id = id.checked_add(unzz(r.varint()?)).ok_or(DAMAGED)?;
            if id < 0 || id as u64 >= self.len() as u64 {
                return Err(DAMAGED);
            }
            let (s, e) = (self.starts[id as usize] as usize, self.starts[id as usize + 1] as usize);
            if out.len() + e - s > max {
                return Err(DAMAGED);
            }
            out.extend_from_slice(&self.bytes[s..e]);
        }
        Ok(out)
    }
}

/// Open a payload made by `pack`: the exact original bytes, or an error.
pub fn unpack(payload: &[u8], threads: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    unpack_to(payload, threads, &mut |b| {
        out.extend_from_slice(b);
        Ok(true)
    })?;
    Ok(out)
}

/// Open a payload made by `pack`, handing its bytes to `sink` in order, a block at a time
/// (each block checked against its size and CRC first). Blocks are opened into buffers that
/// are used again for later blocks, so opening does not touch fresh memory for every byte;
/// with several threads, a few blocks ahead are opened at the same time. `sink` returns
/// false to stop. Returns the payload's size (the bytes handed out when it ran to the end).
pub fn unpack_to(payload: &[u8], threads: usize, sink: &mut dyn FnMut(&[u8]) -> Result<bool, String>) -> Result<u64, String> {
    unpack_blocks(payload, threads, None, true, &mut |t, _| sink(t.unwrap_or_default()))
}

/// A search's test of a block of lines before it is opened (fewer-pack grep): given the block's
/// patterns (each ended by '\n', a value's place marked by a 00 byte), and maybe its texts
/// and its words (stream 5 and the block's word table), whether a line of the block may hold
/// what is looked for. Without the texts and words, false only means "ask again with them".
pub type Filter<'a> = &'a (dyn Fn(&[u8], Option<(&[u8], &[u8])>) -> bool + Sync);

/// The month and weekday names a value may be (the other values are texts, words, or
/// numbers printed with digits, the letters a-f / A-F and "-.:, ").
pub fn value_names() -> &'static [&'static [u8]] {
    eng::NAMES
}

/// unpack_to, and with a filter (fewer-pack grep): a block of lines without derived fields whose
/// text ends with '\n', after a '\n' (`prev_nl`: the payload starts after one), and that the
/// filter says cannot hold the text, is not opened: `sink` gets None and its size. Every
/// other block is opened and checked as by unpack_to (Some(text)).
pub fn unpack_blocks(
    payload: &[u8],
    threads: usize,
    may: Option<Filter>,
    prev_nl: bool,
    sink: &mut BlockSink<'_>,
) -> Result<u64, String> {
    let threads = threads.max(1);
    let (raw, blocks, tframes, tmax, bank) = parse_payload(payload)?;
    let tabs = open_tables(&tframes, tmax, threads)?;
    // 2. the blocks in order, each checked against its size and CRC, handed to the sink
    open_blocks(&blocks, &tabs, bank.as_ref(), threads, may, prev_nl, sink)?;
    Ok(raw as u64)
}

/// A payload's header read and checked: its size, its blocks, its table frames and their
/// limit, and its table bank (unpack_blocks and Seg).
#[allow(clippy::type_complexity)]
fn parse_payload(payload: &[u8]) -> Result<(usize, Vec<BlockRef<'_>>, Vec<&[u8]>, usize, Option<eng::Bank>), String> {
    let mut r = Rd::new(payload);
    let id = r.byte()?;
    if id != ENGINE_ID {
        // only this format is read (engine 5 and older are not); say plainly which version
        // made it
        return Err(if id < ENGINE_ID {
            format!("made by an older fewer-pack (archive engine {id}); this fewer-pack {} reads engine {ENGINE_ID} only", env!("CARGO_PKG_VERSION"))
        } else {
            format!("made by a newer fewer-pack (archive engine {id}); this fewer-pack {} reads engine {ENGINE_ID}", env!("CARGO_PKG_VERSION"))
        });
    }
    let raw = r.varint()?;
    let nblocks = r.varint()?;
    let ntables = r.varint()?;
    let nt = eng::TABLES.len();
    if raw > MAX_RAW || nblocks > raw || (ntables != 0 && ntables != nt as u64 && ntables != nt as u64 + 1) {
        return Err(DAMAGED.into());
    }
    let raw = raw as usize;
    let mut tframes = Vec::with_capacity(nt + 1);
    for _ in 0..ntables {
        tframes.push(r.frame()?);
    }
    // the table bank: one more frame after the tables (every entry checked as it is read)
    let bank = match tframes.len() > nt {
        true => Some(tframes.pop().and_then(|f| guarded(|| eng::Bank::read(f)).flatten()).ok_or(DAMAGED)?),
        false => None,
    };
    // the tables hold at most the payload's text plus a '\n' per entry: refused before any
    // is opened when they declare more all together
    let tmax = 4 * raw + 1024;
    let mut declared = 0u64;
    for f in &tframes {
        if !f.is_empty() {
            if !eng::method_is(f, eng::TABLE_METHOD) {
                return Err(DAMAGED.into());
            }
            declared = declared.saturating_add(eng::declared(f).ok_or(DAMAGED)?);
        }
    }
    if declared > tmax as u64 {
        return Err(DAMAGED.into());
    }
    // a block takes at least 6 bytes of the payload
    let mut blocks: Vec<BlockRef> = Vec::with_capacity(nblocks.min(payload.len() as u64 / 6) as usize);
    let mut sum = 0usize;
    for _ in 0..nblocks {
        let braw = r.varint()?;
        if braw == 0 || braw > (raw - sum) as u64 || braw > MAX_BLOCK as u64 {
            return Err(DAMAGED.into());
        }
        let braw = braw as usize;
        sum += braw;
        let kind = r.byte()?;
        let (ends_nl, lines, nframes) = match kind {
            KIND_LINES => {
                let flags = r.byte()?;
                let lines = r.varint()?;
                let ns = r.varint()?;
                if ntables == 0 || flags > 1 || lines == 0 || lines > braw as u64 || lines > MAX_LINES as u64 || ns != eng::STREAMS as u64 {
                    return Err(DAMAGED.into());
                }
                (flags == 1, lines as usize, eng::STREAMS)
            }
            KIND_PLAIN => (false, 0, 1),
            _ => return Err(DAMAGED.into()),
        };
        let mut frames = Vec::with_capacity(nframes);
        for _ in 0..nframes {
            frames.push(r.frame()?);
        }
        // (the frame methods are checked here too, before anything is decoded)
        let methods_ok = match kind {
            KIND_PLAIN => !frames[0].is_empty() && eng::method_is(frames[0], eng::COLUMN_METHOD),
            _ => frames.iter().enumerate().all(|(k, f)| f.is_empty() || eng::method_is(f, eng::stream_method(k))),
        };
        if !methods_ok {
            return Err(DAMAGED.into());
        }
        let c = r.take(4)?;
        let crc = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        blocks.push(BlockRef { raw: braw, kind, ends_nl, lines, frames, crc });
    }
    if sum != raw || !r.done() {
        return Err(DAMAGED.into());
    }
    Ok((raw, blocks, tframes, tmax, bank))
}

/// 1. the payload's tables, opened on up to `threads` threads
fn open_tables(tframes: &[&[u8]], tmax: usize, threads: usize) -> Result<Vec<Table>, String> {
    let opened: Vec<Result<Table, &str>> = par_map(tframes.len(), threads, |i| {
        let f = tframes[i];
        let t = if f.is_empty() { Vec::new() } else { guarded(|| eng::decode_as(f, tmax, eng::TABLE_METHOD, None)).flatten().ok_or(DAMAGED)? };
        Table::open(t)
    });
    let mut tabs = Vec::with_capacity(opened.len());
    for t in opened {
        tabs.push(t?);
    }
    Ok(tabs)
}

/// A payload (one segment) read for a search whose blocks are opened one by one on any
/// thread (fewer-pack grep: container::search_archive): its bytes, its checked block headers
/// (frames as places in the bytes) and its opened tables.
pub struct Seg {
    payload: Vec<u8>,
    raw: u64,
    blocks: Vec<SegBlock>,
    tabs: Vec<Table>,
    bank: Option<eng::Bank>,
}

struct SegBlock {
    raw: usize,
    kind: u8,
    ends_nl: bool,
    lines: usize,
    frames: Vec<(usize, usize)>,
    crc: u32,
}

/// The text a block is opened into (Seg::open), used again for later blocks.
#[derive(Default)]
pub struct BlockBuf(Bufs);

impl BlockBuf {
    /// The opened block's text (its `raw` bytes).
    pub fn text(&self, raw: usize) -> &[u8] {
        &self.0.text[..raw]
    }
    /// The buffer a block with derived fields is rewritten into while it is opened (not
    /// needed once it is open: it can serve the next block opened elsewhere).
    pub fn take_scratch(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0.scratch)
    }
    pub fn put_scratch(&mut self, s: Vec<u8>) {
        self.0.scratch = s;
    }
}

impl Seg {
    /// Read and check a payload's header and open its tables (on up to `threads` threads);
    /// the blocks are opened later, each by Seg::open.
    pub fn new(payload: Vec<u8>, threads: usize) -> Result<Seg, String> {
        let base = payload.as_ptr() as usize;
        let (raw, blocks, tframes, tmax, bank) = parse_payload(&payload)?;
        let tabs = open_tables(&tframes, tmax, threads.max(1))?;
        let blocks: Vec<SegBlock> = blocks
            .iter()
            .map(|b| SegBlock {
                raw: b.raw,
                kind: b.kind,
                ends_nl: b.ends_nl,
                lines: b.lines,
                frames: b.frames.iter().map(|f| (f.as_ptr() as usize - base, f.len())).collect(),
                crc: b.crc,
            })
            .collect();
        Ok(Seg { raw: raw as u64, blocks, tabs, bank, payload })
    }
    /// The size of the text the payload holds.
    pub fn raw(&self) -> u64 {
        self.raw
    }
    pub fn blocks(&self) -> usize {
        self.blocks.len()
    }
    pub fn block_raw(&self, i: usize) -> usize {
        self.blocks[i].raw
    }
    /// Open block i into `buf` (checked against its size and CRC), on up to `threads`
    /// threads; or, with a filter, leave a block of lines that cannot hold the text unopened
    /// (Ok(true), as unpack_blocks does: its text ends with '\n').
    pub fn open(&self, i: usize, buf: &mut BlockBuf, threads: usize, may: Option<Filter>) -> Result<bool, String> {
        let h = &self.blocks[i];
        let b = BlockRef {
            raw: h.raw,
            kind: h.kind,
            ends_nl: h.ends_nl,
            lines: h.lines,
            frames: h.frames.iter().map(|&(a, n)| &self.payload[a..a + n]).collect(),
            crc: h.crc,
        };
        // (a panic outside the engine's guards becomes an error here too, as in open_blocks)
        match guarded(|| open_block(&b, &self.tabs, self.bank.as_ref(), &mut buf.0, threads.max(1), may)) {
            Some(Ok(s)) => {
                buf.0.skipped = s;
                Ok(s)
            }
            Some(Err(e)) => Err(format!("{e} (block {})", i + 1)),
            None => Err(format!("{DAMAGED} (block {})", i + 1)),
        }
    }
}

/// Buffers a thread opens blocks into, used again for its next block: the block's text, and
/// the rewritten text of a block with derived fields; `skipped`: the block was left unopened
/// (the filter of unpack_blocks).
#[derive(Default)]
struct Bufs {
    text: Vec<u8>,
    scratch: Vec<u8>,
    skipped: bool,
}

/// Open the blocks in order and hand each one's text to `sink`. One thread: block after
/// block into the same buffers. Several: the threads (`workers`) take the blocks in order,
/// at most `window` (their number + 2) blocks ahead of the one being handed out (memory:
/// about that many blocks), and this thread hands them out in order as they are ready; a
/// block's buffers go back to a pool for a later block. A file of one block gives that block
/// all the threads (its streams are decoded side by side).
#[allow(clippy::too_many_arguments)]
fn open_blocks(
    blocks: &[BlockRef],
    tabs: &[Table],
    bank: Option<&eng::Bank>,
    threads: usize,
    may: Option<Filter>,
    prev_nl: bool,
    sink: &mut BlockSink<'_>,
) -> Result<(), String> {
    let n = blocks.len();
    let per = (threads / n.max(1)).max(1);
    let open = |i: usize, bufs: &mut Bufs, may: Option<Filter>| {
        bufs.skipped = open_block(&blocks[i], tabs, bank, bufs, per, may).map_err(|e| format!("{e} (block {})", i + 1))?;
        Ok::<(), String>(())
    };
    // a block left unopened is handed out as None only after a '\n' (else the line before it
    // goes on into it: then it is opened here after all); its own text ends with one
    let mut last_nl = prev_nl;
    let mut hand = |i: usize, bufs: &mut Bufs| -> Result<bool, String> {
        if bufs.skipped && !last_nl {
            open(i, bufs, None)?;
        }
        let raw = blocks[i].raw;
        if bufs.skipped {
            last_nl = true;
            return sink(None, raw);
        }
        last_nl = bufs.text[raw - 1] == b'\n';
        sink(Some(&bufs.text[..raw]), raw)
    };
    if threads == 1 || n <= 1 {
        let mut bufs = Bufs::default();
        for i in 0..n {
            open(i, &mut bufs, may)?;
            if !hand(i, &mut bufs)? {
                break;
            }
        }
        return Ok(());
    }
    let w = workers(threads, n);
    let window = w + 2;
    struct State {
        /// blocks handed out so far
        done: usize,
        /// opened blocks waiting to be handed out (by block number)
        ready: Vec<Option<Result<Bufs, String>>>,
        /// buffers of blocks handed out
        pool: Vec<Bufs>,
        stop: bool,
    }
    let st = Mutex::new(State { done: 0, ready: (0..n).map(|_| None).collect(), pool: Vec::new(), stop: false });
    let cv = std::sync::Condvar::new();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..w.min(n) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        return;
                    }
                    let mut bufs = {
                        let mut g = st.lock().unwrap();
                        while !g.stop && i >= g.done + window {
                            g = cv.wait(g).unwrap();
                        }
                        if g.stop {
                            return;
                        }
                        g.pool.pop().unwrap_or_default()
                    };
                    // (a panic outside the engine's guards becomes an error here too: this
                    // thread must still hand in its block, or the writer would wait for it)
                    let r = match guarded(|| open(i, &mut bufs, may)) {
                        Some(Ok(())) => Ok(bufs),
                        Some(Err(e)) => Err(e),
                        None => Err(format!("{DAMAGED} (block {})", i + 1)),
                    };
                    st.lock().unwrap().ready[i] = Some(r);
                    cv.notify_all();
                }
            });
        }
        let mut result = Ok(());
        for i in 0..n {
            let r = {
                let mut g = st.lock().unwrap();
                loop {
                    if let Some(r) = g.ready[i].take() {
                        break r;
                    }
                    g = cv.wait(g).unwrap();
                }
            };
            let go = match r {
                Ok(mut bufs) => {
                    let go = hand(i, &mut bufs);
                    let mut g = st.lock().unwrap();
                    g.pool.push(bufs);
                    g.done = i + 1;
                    go
                }
                Err(e) => Err(e),
            };
            match go {
                Ok(true) => cv.notify_all(),
                Ok(false) => break,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        st.lock().unwrap().stop = true;
        cv.notify_all();
        result
    })
}

/// Open one block into bufs.text[..b.raw]; or, with a filter, leave a block of lines that
/// cannot hold the text unopened (Ok(true): see unpack_blocks).
fn open_block(b: &BlockRef, tabs: &[Table], bank: Option<&eng::Bank>, bufs: &mut Bufs, threads: usize, may: Option<Filter>) -> Result<bool, &'static str> {
    const BAD_CRC: &str = "damaged data (a block does not match its checksum)";
    if b.kind == KIND_PLAIN {
        let t = guarded(|| eng::decode_as(b.frames[0], b.raw, eng::COLUMN_METHOD, None)).flatten().ok_or(DAMAGED)?;
        if t.len() != b.raw || crc32(&t) != b.crc {
            return Err(BAD_CRC);
        }
        bufs.text = t;
        return Ok(false);
    }
    // the text the engine rebuilds: every line with its '\n'
    let size = b.raw + !b.ends_nl as usize;
    // the streams: the table streams from the lists, the others decoded; each within a
    // limit that follows from the block's size, and all together too (checked first)
    // a block's list names different entries of its table (as differences of entry numbers
    // below the table's length: at most 5 bytes each), so it is also at most 5 bytes per
    // entry of the payload's table
    let max = |k: usize| match eng::TABLES.iter().position(|&t| t == k) {
        Some(t) => stream_max(k, b.lines, size).min(5 * tabs[t].len() + 16),
        None => stream_max(k, b.lines, size),
    };
    let mut declared = 0u64;
    for (k, f) in b.frames.iter().enumerate() {
        if !f.is_empty() {
            let n = eng::declared(f).ok_or(DAMAGED)?;
            if n > max(k) as u64 || !eng::method_is(f, eng::stream_method(k)) {
                return Err(DAMAGED);
            }
            declared += n;
        }
    }
    if declared > streams_max(size) {
        return Err(DAMAGED);
    }
    let dec = |k: usize| {
        let f = b.frames[k];
        let d = if f.is_empty() { Vec::new() } else { guarded(|| eng::decode_as(f, max(k), eng::stream_method(k), bank.map(|x| (x, k)))).flatten().ok_or(DAMAGED)? };
        match eng::TABLES.iter().position(|&t| t == k) {
            Some(t) => tabs[t].rebuild(&d, table_max(size)),
            None => Ok(d),
        }
    };
    let mut streams: Vec<Vec<u8>> = vec![Vec::new(); b.frames.len()];
    let mut done = [false; eng::STREAMS];
    if let Some(may) = may {
        // the patterns and the derive side stream first: a block without derived fields
        // whose text ends with '\n' is not opened when no line can hold the text (asked first
        // without the texts and words, then with them)
        let (pats, side) = (dec(0)?, dec(eng::SIDE)?);
        if side.is_empty() && b.ends_nl && !may(&pats, None) {
            let (texts, words) = (dec(eng::TEXTS)?, dec(eng::TABLES[1])?);
            if !may(&pats, Some((&texts, &words))) {
                return Ok(true);
            }
            (streams[eng::TEXTS], streams[eng::TABLES[1]]) = (texts, words);
            (done[eng::TEXTS], done[eng::TABLES[1]]) = (true, true);
        }
        (streams[0], streams[eng::SIDE]) = (pats, side);
        (done[0], done[eng::SIDE]) = (true, true);
    }
    let rest: Vec<usize> = (0..streams.len()).filter(|&k| !done[k]).collect();
    let sizes: Vec<usize> = rest.iter().map(|&k| b.frames[k].len()).collect();
    let decoded: Vec<Result<Vec<u8>, &str>> = par_jobs(&sizes, threads, |q| dec(rest[q]));
    for (q, d) in decoded.into_iter().enumerate() {
        streams[rest[q]] = d?;
    }
    eng::check_block(&streams, b.lines, size)?;
    // the buffer of the last block opened here, grown when this one needs more (the engine
    // writes every byte of the text, and up to 64 past it)
    let buf = &mut bufs.text;
    if buf.len() < size + 64 {
        buf.resize(size + 64, 0);
    }
    let scratch = &mut bufs.scratch;
    let end = guarded(|| eng::open_block(streams, b.lines, &mut buf[..size + 64], size, scratch)).ok_or(DAMAGED)??;
    if end != size || (!b.ends_nl && buf[size - 1] != b'\n') {
        return Err(BAD_CRC);
    }
    if crc32(&buf[..b.raw]) != b.crc {
        return Err(BAD_CRC);
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
