// fastlane, final engine (2026-11-15): a speed-first log packer, one setting (level 43).
// One pass per line: the line's "shape" (its fixed text, values replaced by a mark) is
// looked up in a table; values go to columns (numbers as differences, the rest as text);
// no trials, no allocation per word, blocks on all cores, a fast coder on the columns.
// Every block is unpacked again (from the coded bytes only) and compared with the input.
//
// Merged one idea at a time (see FINAL_NOTES.md; checkpoints in final_ckpt/):
// - coder: fz (fz.rs, method 6: LZ with static rANS tables); the setting is level 43 (fz's
//   greedy parse with buckets of 4 and a lazy step); the pattern / word tables and the column
//   header use ozip levels 27-28 (method 5),
// - numbers: cache codes, column lists, long IDs with sign, dotted / loose clock times, hex
//   numbers, fixed widths, difference columns in their own stream, grouped by slot (the
//   grouping estimate on a sample),
// - knobs: month / weekday names as values (NAMEK, looked for only when a sample has one),
//   pattern ids as varints through the coder (FL_TIDM=1: TidModel, +1.6 average but 12%
//   slower opening), second differences when an LZ-like guess finds them
//   clearly smaller, one block per file of up to 400K lines (FL_ONEMAX), else 100K lines
//   (FL_BLOCK),
// - rare words: varying words become values (pick_words), a word table per file, splitting
//   word columns,
// - group: lines of one source together in a block, merged back by clock time (group.rs;
//   off in the product setting: FL_GROUP=auto turns it on, +0.15 average for ~3% speed),
// - front end: lines found with NEON (all cores for big inputs), the fast splitter front.rs
//   (FL_CHECK=1 compares every line with split_line), parallel pass 1, the columns coded side
//   by side on spare threads,
// - coltable: lean column headers (heads.rs), columns numbered from the pieces between slots,
//   the blocks' lists into the tables as one stream (Lists), an opener that returns errors on
//   damaged input (open_unit; FL_DAMAGE=n tests it),
// - tokens: a word mixing letters and values can be one value (tokens.rs); 5-7 hex digits
//   alone between signs are a number (HEXS); spaces padding a number belong to it (SPN);
//   a block keeps its picked words only when they merge away enough shapes (WORDS_PAY),
// - derive: derived fields (derive.rs: BGL epoch, Spark task ids, Proxifier closes, Hadoop
//   floats, old/new pairs, ctime copies) rewritten away per block, put back when opening,
// - opener: the fast line loop of open.rs (ops per pattern, handlers per column), blocks
//   opened side by side on all cores,
// - kits: kit pattern and word tables known to both sides (FL_KIT=1: the 30% / 70% kit test),
// - rules (rules.rs): number slots that follow from earlier slots of the line (equal, k x, sums
//   with signs, clock + offset, human sizes; near: plus a small coded difference) are not
//   stored: one coded yes/no per value (FL_NORULES=1: off; FL_RDUMP=1: print the rules).
// The pattern and word tables are coded once per file; each block keeps lists into them.
#[allow(dead_code)]
mod ozip;
#[allow(dead_code)]
mod fz;
mod group;
mod heads;
mod open;
mod rules;
mod split;
mod tokens;
mod oldcol;
mod ids;
#[allow(dead_code)]
mod front;
#[allow(dead_code)]
mod derive;
mod entity;
// x86_64 versions of the NEON fast paths (SSE2, SSE4.2, AVX2: x86.rs)
#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[allow(dead_code)]
pub mod x86;

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::Instant;

/// Timer for the per-stage speeds: wall clock, or with FL_CPUTIME=1 this thread's CPU time
/// (steadier when other programs share the machine).
mod clock {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(id: u32) -> u64;
    }
    fn cpu() -> bool {
        static C: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
        *C.get_or_init(|| crate::engine::fl::no_env::var_os("FL_CPUTIME").is_some())
    }
    /// FL_PROF=1 (harness, scoreboard): each tick also reads this thread's instructions and
    /// cycles (exact hardware counts; macOS only, zero elsewhere)
    #[inline(always)]
    pub fn cnt() -> bool {
        static C: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
        *C.get_or_init(|| crate::engine::fl::no_env::var_os("FL_PROF").is_some())
    }
    /// (instructions, cycles) of this thread
    #[cfg(target_vendor = "apple")]
    pub fn counts() -> (u64, u64) {
        unsafe extern "C" {
            fn thread_selfcounts(kind: i32, buf: *mut u64, n: usize) -> i32;
        }
        let mut b = [0u64; 2];
        // kind 1: instructions, cycles
        if unsafe { thread_selfcounts(1, b.as_mut_ptr(), 16) } != 0 {
            return (0, 0);
        }
        (b[0], b[1])
    }
    #[cfg(not(target_vendor = "apple"))]
    pub fn counts() -> (u64, u64) {
        (0, 0)
    }
    /// (instructions, cycles) of the whole process: all its threads, running or ended
    #[cfg(target_vendor = "apple")]
    pub fn proc_counts() -> (u64, u64) {
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buf: *mut u64) -> i32;
            fn getpid() -> i32;
        }
        let mut b = [0u64; 40];
        // flavor 4: rusage_info_v4 (296 bytes); ri_instructions and ri_cycles are words 31, 32
        if unsafe { proc_pid_rusage(getpid(), 4, b.as_mut_ptr()) } != 0 {
            return (0, 0);
        }
        (b[31], b[32])
    }
    #[cfg(not(target_vendor = "apple"))]
    pub fn proc_counts() -> (u64, u64) {
        (0, 0)
    }
    /// (nanoseconds, this thread's instructions, its cycles; both 0 without FL_PROF)
    #[derive(Clone, Copy)]
    pub struct Tick(u64, pub u64, pub u64);
    impl Tick {
        pub fn now() -> Tick {
            let (i, c) = if cnt() { counts() } else { (0, 0) };
            // 16: this thread's CPU time; 4: the raw monotonic clock
            Tick(unsafe { clock_gettime_nsec_np(if cpu() { 16 } else { 4 }) }, i, c)
        }
        pub fn elapsed(&self) -> std::time::Duration {
            Tick::now() - *self
        }
    }
    impl std::ops::Sub for Tick {
        type Output = std::time::Duration;
        fn sub(self, o: Tick) -> std::time::Duration {
            std::time::Duration::from_nanos(self.0.saturating_sub(o.0))
        }
    }
}
use clock::Tick;

thread_local! {
    /// A hook the caller may set on its thread (fewer-pack's adapter; the harness never does): called
    /// once per block with the block's pattern table (stream 0) as soon as that is final, after
    /// the template loop and before pass 2, so that the file's pattern table can be joined and
    /// coded on another core while the block is still packed. It only reads the table: the
    /// block's bytes do not depend on it.
    pub(crate) static TMPL_HOOK: std::cell::Cell<Option<fn(&[u8])>> = const { std::cell::Cell::new(None) };
}

#[derive(Default)]
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
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}
type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

/// Hasher of the opener's slot map (slot_columns), whose keys are (column, hash of a piece):
/// each number is mixed in with one multiply (Fx takes a u32 a byte at a time). The map only
/// numbers its keys in order of first use, so its hasher does not change any number.
#[derive(Default)]
struct SlotHasher(u64);
impl Hasher for SlotHasher {
    fn finish(&self) -> u64 {
        self.0 ^ self.0 >> 29
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.write_u64(x as u64);
        }
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.write_u64(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(26) ^ i).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

const MARK: u8 = 0;
// byte classes: 0 other, 1 digit, 2 space, 3 hex letter, 4 other letter
static CLS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        t[i] = if c.is_ascii_digit() {
            1
        } else if c == b' ' {
            2
        } else if (c >= b'a' && c <= b'f') || (c >= b'A' && c <= b'F') {
            3
        } else if c.is_ascii_alphabetic() {
            4
        } else {
            0
        };
        i += 1;
    }
    t
};

fn put_varint(o: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        o.push(v as u8 | 0x80);
        v >>= 7;
    }
    o.push(v as u8);
}
/// put_varint's bytes at p (room for 10 bytes there); returns the end (pack speed: no
/// capacity check per byte; the same bytes).
#[inline(always)]
unsafe fn varint_at(mut p: *mut u8, mut v: u64) -> *mut u8 {
    unsafe {
        while v >= 0x80 {
            *p = v as u8 | 0x80;
            p = p.add(1);
            v >>= 7;
        }
        *p = v as u8;
        p.add(1)
    }
}
/// Append varints to `o` through f(write), which gets the write position and returns the end
/// (at most `room` bytes are written).
#[inline(always)]
fn put_varints(o: &mut Vec<u8>, room: usize, f: impl FnOnce(*mut u8) -> *mut u8) {
    o.reserve(room);
    unsafe {
        let b = o.as_mut_ptr();
        let e = f(b.add(o.len()));
        o.set_len(e.offset_from(b) as usize);
    }
}
fn get_varint(b: &[u8], p: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut s = 0;
    loop {
        let x = b[*p];
        *p += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return v;
        }
        s += 7;
    }
}
#[inline]
fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
#[inline]
fn unzz(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[derive(Default, Clone)]
struct Col {
    // an ID column (ids.rs, mode bit 128): the step forms of its new long IDs, in order
    idc: bool,
    steps: Vec<u64>,
    kinds: Vec<u8>,
    vals: Vec<u64>,
    txt: Vec<u8>,
    // the last 8 different texts of this column (start, end in txt), newest first:
    // a repeat is stored as "the k-th recent one" (kinds 2 and 71-77)
    recent: Vec<(u32, u32)>,
    // the pattern slot of each number (its "last value at this slot" is a candidate)
    slots: Vec<u32>,
    // per number (only in columns with long IDs): see LONG_ID handling in pack_block
    ids: Vec<u32>,
    // the last 8 different words of this column (words turned into values, kinds 79-87)
    wrecent: Recent8,
    // a word column whose words choose the columns of the line's later slots (mode bit 64)
    split: bool,
    // the numbers of known entities (entity.rs): (place in vals, rank in the entity's recent
    // values (entity::R: a new value), the entity's last new value)
    ke: Vec<(u32, u8, u64)>,
}


impl Col {
    /// A copy of the column as pass 2 left it (pack_block codes a block both ways).
    fn copy(&self) -> Col {
        Col { idc: self.idc, steps: self.steps.clone(), kinds: self.kinds.clone(), vals: self.vals.clone(), txt: self.txt.clone(), recent: Vec::new(), slots: self.slots.clone(), ids: self.ids.clone(), wrecent: self.wrecent, split: self.split, ke: self.ke.clone() }
    }
}

/// Pass 2's long IDs: each one's place in the block's list (in order of first use); and every
/// long ID and UUID value in line order (ids.rs).
#[derive(Default)]
struct IdMap {
    map: FxMap<u64, u32>,
    seq: Vec<IdVal>,
}

/// A long ID or UUID value of pass 2: its column, kind, value (a UUID's bits), place (a long
/// ID: among the column's numbers; a UUID: in its texts), the column's numbers before it and
/// its pattern slot (a UUID).
#[derive(Clone, Copy)]
struct IdVal {
    col: u32,
    kind: u8,
    v: u128,
    pos: u32,
    npos: u32,
    sl: u32,
}

/// Cache codes for a number column (mode bit 4): each value is one varint s. s < CK: the
/// value is a candidate: first three "specials" (the column's previous value, the last value
/// at the same pattern slot, the last number of the same kind anywhere before it; the one
/// right most often so far comes first), then the column's earlier different values, newest
/// first. s >= CK: a new value; s - CK is its difference from the previous value (mode bit 1)
/// or the value itself.
const CK: usize = 9;
const NM: usize = CK - 2; // recent list length; codes 3.. are m[1..]

struct Cache {
    m: [u64; NM],  // recent different values, newest first (m[0] = the previous value)
    hits: [u32; 3], // how often each special was right
    ord: [u8; 3],   // the specials, most often right first
}

/// Kind class for "the last number of the same kind": plain numbers (padded with zeros or
/// spaces or not) are one.
#[inline]
fn kclass(kind: u8) -> usize {
    if kind <= 22 || (SPN..=SPN + SPN_MAX).contains(&kind) { 0 } else { kind as usize }
}

/// Kind of a long ID (15-19 digits, below 2^63, maybe with a minus sign: HDFS block IDs):
/// value = magnitude * 2 + sign. Long IDs of a block go to one list in the order they first
/// appear; a repeat in any column can be coded as its distance from the newest entry.
const LONG_ID: u8 = 24;

/// Kinds HEXNUM + (w - 8) (+ 9 in upper case): w hex digits (8-16) as a number.
const HEXNUM: u8 = 132;

/// Kinds HEXS + (w - 5) (+ 3 in upper case): a run of w = 5-7 hex digits with a letter and a
/// digit, alone in its word or between signs ("{9f4ef63 "), as a number.
const HEXS: u8 = 150;

/// Kinds SPN + w: a number right-aligned with spaces to w characters (w <= SPN_MAX): the
/// spaces after a word's separating space ("Jun  9", "811  1702") belong to the number.
const SPN: u8 = 160;
const SPN_MAX: u8 = 24;

impl Cache {
    fn new() -> Cache {
        Cache { m: [0; NM], hits: [0; 3], ord: [0, 1, 2] }
    }
    /// Where v is in the recent list (NM: not there).
    #[inline]
    fn pos(&self, v: u64) -> usize {
        let mut p = NM;
        for k in (0..NM).rev() {
            if self.m[k] == v {
                p = k;
            }
        }
        p
    }
    /// The code of v (CK: not a candidate) and its place in the recent list; t = the last
    /// value at v's slot, ks = v equals the last number of its kind. (put_nums uses
    /// EncCache::code, the same codes.)
    #[allow(dead_code)]
    #[inline]
    fn code(&self, v: u64, t: u64, ks: bool) -> (usize, usize) {
        let p = self.pos(v);
        let eq = [p == 0, v == t, ks];
        for i in 0..3 {
            if eq[self.ord[i] as usize] {
                return (i, p);
            }
        }
        (if p < NM { p + 2 } else { CK }, p)
    }
    /// The value of code s < CK and its place in the recent list (k = the kind's last number).
    #[inline]
    fn value(&self, s: usize, t: u64, k: u64) -> (u64, usize) {
        if s >= 3 {
            return (self.m[s - 2], s - 2);
        }
        match self.ord[s] {
            0 => (self.m[0], 0),
            1 => (t, self.pos(t)),
            _ => (k, self.pos(k)),
        }
    }
    /// After value v (at place p in the recent list) was coded.
    #[inline]
    fn update(&mut self, v: u64, p: usize, eq_t: bool, eq_k: bool) {
        self.hits[0] += (p == 0) as u32;
        self.hits[1] += eq_t as u32;
        self.hits[2] += eq_k as u32;
        let h = &self.hits;
        let o = &mut self.ord;
        for (a, b) in [(0, 1), (1, 2), (0, 1)] {
            if h[o[b] as usize] > h[o[a] as usize] {
                o.swap(a, b);
            }
        }
        let mut i = p.min(NM - 1);
        while i > 0 {
            self.m[i] = self.m[i - 1];
            i -= 1;
        }
        self.m[0] = v;
    }
}

/// The orders of the 3 specials (most often right first): EncCache keeps the index of one.
const PERMS: [[u8; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];

/// RMASK[q * 8 + e]: the specials that are right (bit k of e: special k) in the order PERMS[q]
/// (bit i: the i-th special of the order); 64 entries, so that an index masked with 63 needs
/// no bounds check (q < 6: the index is below 48).
const RMASK: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut q = 0;
    while q < 6 {
        let mut e = 0;
        while e < 8 {
            let mut r = 0u8;
            let mut i = 0;
            while i < 3 {
                r |= ((e >> PERMS[q][i]) & 1) << i;
                i += 1;
            }
            t[q * 8 + e as usize] = r;
            e += 1;
        }
        q += 1;
    }
    t
};

/// put_nums' copy of Cache: the same codes and the same updates (pack speed; the opener keeps
/// Cache). The order of the specials is PERMS[q]; of their hits only the differences in that
/// order are kept (d01: first minus second, d12: second minus third), as Cache::update's
/// compare-and-swaps only compare hits: while both are >= 0 the order is sorted and those
/// compare-and-swaps change nothing. The recent list moves with fixed indices only, each
/// entry chosen by "v is among the entries before it".
#[derive(Clone, Copy)]
struct EncCache {
    m: [u64; NM],
    d01: i64,
    d12: i64,
    q: usize,
}

impl EncCache {
    fn new() -> EncCache {
        EncCache { m: [0; NM], d01: 0, d12: 0, q: 0 }
    }
    /// Cache::code then Cache::update (eq_t = v == t, eq_k = ks): the code of v (CK: not a
    /// candidate).
    #[inline(always)]
    fn step(&mut self, v: u64, t: u64, ks: bool) -> usize {
        let m = self.m;
        let e = [m[0] == v, m[1] == v, m[2] == v, m[3] == v, m[4] == v, m[5] == v, m[6] == v];
        // s_k: v is in m[..k]
        let s1 = e[0];
        let s2 = s1 | e[1];
        let s3 = s2 | e[2];
        let s4 = s3 | e[3];
        let s5 = s4 | e[4];
        let s6 = s5 | e[5];
        let s7 = s6 | e[6];
        // the specials that are right, by rank: the code is the first of them
        let r = RMASK[(self.q * 8 + (e[0] as usize | ((v == t) as usize) << 1 | (ks as usize) << 2)) & 63] as u32;
        let s = (r | 8).trailing_zeros() as usize;
        let s = if s < 3 {
            s
        } else {
            // the first place p of v (NM: not there): s_k is false exactly for k <= p
            let p = NM - (s1 as usize + s2 as usize + s3 as usize + s4 as usize + s5 as usize + s6 as usize + s7 as usize);
            if p < NM { p + 2 } else { CK }
        };
        let (b0, b1, b2) = ((r & 1) as i64, ((r >> 1) & 1) as i64, (r >> 2) as i64);
        self.d01 += b0 - b1;
        self.d12 += b1 - b2;
        if (self.d01 | self.d12) < 0 {
            (self.d01, self.d12, self.q) = reorder(self.d01, self.d12, self.q);
        }
        // m[1..=min(p, NM - 1)] = old m[0..], m[0] = v: entry k moves down when v is not in
        // m[..k] (k <= p)
        self.m = [
            v,
            if s1 { m[1] } else { m[0] },
            if s2 { m[2] } else { m[1] },
            if s3 { m[3] } else { m[2] },
            if s4 { m[4] } else { m[3] },
            if s5 { m[5] } else { m[4] },
            if s6 { m[6] } else { m[5] },
        ];
        s
    }
}

/// Cache::update's compare-and-swaps on EncCache's order q, from the differences of the hits
/// (the hits less the third's: compares are the same); returns the new differences and order.
/// (By value: the cache state stays in registers.)
#[cold]
#[inline(never)]
fn reorder(d01: i64, d12: i64, q: usize) -> (i64, i64, usize) {
    let mut h = [d01 + d12, d12, 0];
    let mut o = PERMS[q];
    for (a, b) in [(0, 1), (1, 2), (0, 1)] {
        if h[b] > h[a] {
            h.swap(a, b);
            o.swap(a, b);
        }
    }
    (h[0] - h[1], h[1] - h[2], PERMS.iter().position(|&x| x == o).unwrap_or(0))
}

/// Bit-length counts of numbers: a rough size in bits when coded as an entropy-coded bit
/// length plus the bits below the top one. Used to choose how a column is written.
struct Est {
    h: [u32; 65],
}

impl Est {
    fn new() -> Est {
        Est { h: [0; 65] }
    }
    #[inline]
    fn add(&mut self, x: u64) {
        self.h[(64 - x.leading_zeros()) as usize] += 1;
    }
    /// Est::bits of the counts by leading zeros z (z[j]: numbers with 64 - j bits): the same
    /// terms added in the same order (bit length 0 first), so the same number.
    fn bits_z(z: &[u32; 65]) -> f64 {
        let n: u32 = z.iter().sum();
        let mut e = 0f64;
        let mut add = |b: usize, c: u32| {
            if c > 0 {
                e += c as f64 * ((n as f64 / c as f64).log2() + b.saturating_sub(1) as f64);
            }
        };
        add(0, z[64]);
        // bit lengths 1-64 (z[63] down to z[0]) in 16 blocks of 4; an empty block adds nothing
        for k in (0..16).rev() {
            let q = &z[4 * k..4 * k + 4];
            if q[0] | q[1] | q[2] | q[3] != 0 {
                for j in (0..4).rev() {
                    add(64 - (4 * k + j), q[j]);
                }
            }
        }
        e
    }
    fn bits(&self) -> f64 {
        let n: u32 = self.h.iter().sum();
        let mut e = 0f64;
        for (b, &c) in self.h.iter().enumerate() {
            if c > 0 {
                e += c as f64 * ((n as f64 / c as f64).log2() + b.saturating_sub(1) as f64);
            }
        }
        e
    }
}

fn env_on(k: &str) -> bool {
    crate::engine::fl::no_env::var_os(k).is_some()
}

/// Differences in their own stream (stream 7) unless FL_ONENUMS is set.
fn split_nums_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_ONENUMS"))
}

/// `slots` entries: the pattern slot, plus this bit when the number equals the last number of
/// its kind class.
const SLOT: u32 = (1 << 31) - 1;

/// A column written as differences with at least GROUP_MIN numbers (all numbers) is cut into
/// GROUPS segments: each pattern slot goes to the segment of its typical difference size (see
/// group_of), so differences of a like size sit together and the coder learns each group's
/// sizes (mode bit 16; the group of each slot is in the header). The opener puts the numbers
/// back in line order before it rebuilds the lines (ungroup).
const GROUP_MIN: usize = 4096;
const GROUPS: usize = 12;
const GROUP_GAIN: f64 = 0.95; // grouped only when the estimate drops below this share

/// The group of a slot from the sum and count of its differences' bit lengths: its mean bit
/// length in steps of 1.25 bits (FL_GSTEP: the step in quarter bits, 5 by default).
fn group_of(sum: u64, cnt: u32) -> usize {
    static STEP: crate::engine::fl::NoEnvOnce<u64> = crate::engine::fl::NoEnvOnce::new();
    let step = *STEP.get_or_init(|| crate::engine::fl::no_env::var("FL_GSTEP").ok().and_then(|x| x.parse().ok()).unwrap_or(5));
    ((sum * 4 / cnt.max(1) as u64 / step) as usize).min(GROUPS - 1)
}

/// How many numbers of a column the choice of its way looks at (in EST_CHUNKS runs).
const EST_N: usize = 4096;
const EST_CHUNKS: usize = 8;

/// Columns with fewer numbers are written as before (differences or values by byte count).
const SMALL_COL: usize = 16;

/// FL_DOD: a rough size of a number column in each form (0 as is, 1 differences, 2
/// differences of differences) as an LZ coder would see it, in tenths of a byte: a code equal
/// to the one before costs 2, one of the 3 before that 5, one seen earlier (small table) 10,
/// else 10 per varint byte. Looks only at the values it is given (the caller's sample).
/// (M4: the same costs as ref's; the table is the first 2 << bits entries of an 8192-entry
/// array, so no index needs a check, and the loop is made once per form.)
fn guess_forms(vals: &[u64], set: &mut Vec<u64>, which: usize) -> [u64; 3] {
    // (each form has its own table part and recent list: only form `which` and form 2 are
    // worked out, the other's cost stays 0; the caller compares those two)
    let bits = (2 * vals.len()).next_power_of_two().trailing_zeros().clamp(4, 12);
    if set.len() < 8192 {
        set.resize(8192, 0);
    }
    let tab: &mut [u64; 8192] = (&mut set[..8192]).try_into().unwrap();
    tab[..2 << bits].fill(u64::MAX);
    let mut cost = [0u64; 3];
    debug_assert!(which < 2);
    (cost[which], cost[2]) = if which == 1 { guess_two::<true>(vals, tab, bits) } else { guess_two::<false>(vals, tab, bits) };
    cost
}

/// 10 * the varint bytes of a number x with (x | 1).leading_zeros() = j: 10 * (1 + (63 - j) / 7).
const VB10: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut j = 0;
    while j < 64 {
        t[j] = (10 * (1 + (63 - j) / 7)) as u8;
        j += 1;
    }
    t
};

/// The cost of one code in guess_forms: x against its form's recent list and table entry
/// (both updated). (h < 8192: the mask changes nothing.)
#[inline(always)]
fn guess_cost(x: u64, l: &mut [u64; 4], set: &mut [u64; 8192], h: usize) -> u64 {
    // (x equal to the form's last code: cost 2, and nothing changes: the list keeps x first,
    // and x's table entry already holds x, from that code, or u64::MAX at the start)
    if l[0] == x {
        return 2;
    }
    let h = h & 8191;
    let c = if l[1] == x || l[2] == x || l[3] == x {
        5
    } else if set[h] == x {
        10
    } else {
        VB10[(x | 1).leading_zeros() as usize] as u64
    };
    *l = [x, l[0], l[1], l[2]];
    set[h] = x;
    c
}

/// guess_forms for form DIFF (false: values, true: differences) and form 2: their costs.
#[inline(never)]
fn guess_two<const DIFF: bool>(vals: &[u64], set: &mut [u64; 8192], bits: u32) -> (u64, u64) {
    let (mut la, mut lb) = ([u64::MAX; 4], [u64::MAX; 4]);
    let (mut ca, mut cb) = (0u64, 0u64);
    let (mut prev, mut pd) = (0i64, 0i64);
    let half = 1usize << bits;
    let sh = 64 - bits;
    for &v in vals {
        let d = (v as i64).wrapping_sub(prev);
        let xa = if DIFF { zz(d) } else { v };
        let xb = zz(d.wrapping_sub(pd));
        prev = v as i64;
        pd = d;
        ca += guess_cost(xa, &mut la, set, (xa.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> sh) as usize);
        cb += guess_cost(xb, &mut lb, set, half + (xb.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> sh) as usize);
    }
    (ca, cb)
}

/// The differences of vals (zigzag varints) at p; returns the end.
/// (A repeat of the number before is difference 0: one zero byte.)
#[inline(always)]
unsafe fn put_diffs(mut p: *mut u8, vals: &[u64]) -> *mut u8 {
    let mut prev = 0u64;
    for &v in vals {
        if v == prev {
            unsafe { // SAFETY: the caller gives room for 10 bytes per number; a repeat writes 1
                *p = 0;
                p = p.add(1);
            }
            continue;
        }
        p = unsafe { varint_at(p, zz((v as i64).wrapping_sub(prev as i64))) }; // SAFETY: at most 10 bytes, within the caller's room of 10 per number
        prev = v;
    }
    p
}

/// Where a column's numbers and cache codes are written: the numbers into `a`, or (`b` given)
/// those of the ways with differences (mode bits 1 and 32) into `b`; the codes into `codes`.
/// One thread codes the columns in order straight into the block's streams (no buffer per
/// column); the threads of the parallel column phase give each column buffers of its own.
struct Sink<'a> {
    a: &'a mut Vec<u8>,
    b: Option<&'a mut Vec<u8>>,
    codes: &'a mut Vec<u8>,
}

impl Sink<'_> {
    /// The buffer for the numbers of a column written in mode `mode`, and the codes' buffer.
    #[inline]
    fn parts(&mut self, mode: u64) -> (&mut Vec<u8>, &mut Vec<u8>) {
        match (mode & 33 != 0, self.b.as_deref_mut()) {
            (true, Some(b)) => (b, &mut *self.codes),
            _ => (&mut *self.a, &mut *self.codes),
        }
    }
}

/// The cache codes of vals in order (EncCache::step on each; `slot_last` read and updated as
/// the opener does), one byte each into `out` (as long as vals); the cache state carries in c.
/// (M4: the serial chain in a loop of its own; the counts and the numbers follow in a second
/// pass over the codes.)
#[inline(never)]
fn chain_codes(c: &mut EncCache, vals: &[u64], slots: &[u32], slot_last: &mut [u64], out: &mut [u8]) {
    let n = vals.len().min(slots.len()).min(out.len());
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    {
        // every slot inside slot_last (else the loop below, which stops as ref's does)
        let top = slots[..n].iter().fold(0u32, |a, &s| a.max(s & SLOT)) as usize;
        // (the packed hit differences need d01 + d12 + n < 2^30, see chain_asm)
        if n >= 1 && top < slot_last.len() && c.d01 >= 0 && c.d12 >= 0 && (c.d01 + c.d12) as u64 + (n as u64) < 1 << 30 {
            i = n;
            // SAFETY: n values; vals, slots and out hold n entries; every slot index is below
            // slot_last.len() and the hit bound holds (both checked above); the table reads are
            // dd[q * 8 + e] and code[q * 8 + e] (below 48) and nq[q * 8 + w] (below 48), with
            // q < 6 always (EncCache::new, reorder and nq give 0-5)
            unsafe { chain_asm(c, vals.as_ptr(), slots.as_ptr(), slot_last.as_mut_ptr(), out.as_mut_ptr(), n) };
        }
    }
    let mut m = *c;
    for ((&v, &sl), o) in vals[i..n].iter().zip(&slots[i..n]).zip(out[i..n].iter_mut()) {
        let ks = sl > SLOT;
        let sl = (sl & SLOT) as usize;
        let t = slot_last[sl];
        *o = m.step(v, t, ks) as u8;
        slot_last[sl] = v;
    }
    *c = m;
}

/// chain_asm's tables, by q * 8 + e (e: bit 0 the column's previous value, bit 1 the slot's
/// last value, bit 2 the kind's last number are right), as EncCache::step reads RMASK: `dd` the
/// change of the packed hit differences (see chain_asm), `code` the code if a special is
/// right (3: none); nq[q * 8 + w]: the order after reorder's compare-and-swaps w (bit k: the
/// k-th one swapped).
#[repr(C, align(64))]
struct ChainTab {
    dd: [u64; 48],
    code: [u8; 48],
    nq: [u8; 48],
}
static CHAIN_TAB: ChainTab = {
    let mut dd = [0u64; 48];
    let mut code = [0u8; 48];
    let mut nq = [0u8; 48];
    let mut i = 0;
    while i < 48 {
        let r = RMASK[i] as u32;
        code[i] = (r | 8).trailing_zeros() as u8;
        let (b0, b1, b2) = ((r & 1) as i64, ((r >> 1) & 1) as i64, (r >> 2) as i64);
        // u01 -= b0 - b1, u12 -= b1 - b2 (each half on its own: no carry, see chain_asm)
        dd[i] = ((b1 - b0) << 32).wrapping_add(b2 - b1) as u64;
        let mut o = PERMS[i / 8];
        let w = i % 8;
        if w & 1 != 0 {
            o = [o[1], o[0], o[2]];
        }
        if w & 2 != 0 {
            o = [o[0], o[2], o[1]];
        }
        if w & 4 != 0 {
            o = [o[1], o[0], o[2]];
        }
        let mut k = 0;
        while k < 6 {
            if PERMS[k][0] == o[0] && PERMS[k][1] == o[1] && PERMS[k][2] == o[2] {
                nq[i] = k as u8;
            }
            k += 1;
        }
        i += 1;
    }
    ChainTab { dd, code, nq }
};

/// EncCache::step on n values (codes to out; slot_last read and written as there). The recent
/// list lives in one of two sets of registers, X (m0..m6) or Y (va, n1..n6): a step that moves
/// the list writes the other set (no copies); a repeat of the previous value (v == the list's
/// first entry, 61% of the values on the 16 logs, in runs) leaves the list as it is, so it only
/// looks up its code (a special is right: below 3) and the hit changes, and stays in its set.
/// One moving step: the list compare chain (ccmp) gives each entry's move (csel) and the first
/// place of v (cinc); the tables give the special's code and the hit changes; reorder runs as
/// there. The two hit differences are kept in one register as u01 << 32 | u12 with u = 2^31 - 1
/// - d: d < 0 exactly when bit 31 or 63 is set, and with d01 + d12 + n < 2^30 (the caller's
/// check; each step moves d01 + d12 by at most 1) every u stays in (2^30, 2^31], so adding a
/// change never carries from one half to the other.
/// SAFETY: the caller checks that vals, slots and out hold n >= 1 entries, that every slot
/// (& SLOT) is below slot_last's length, and the bound above.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
unsafe fn chain_asm(c: &mut EncCache, vp: *const u64, sp: *const u32, lp: *mut u64, op: *mut u8, n: usize) {
    const K: i64 = (1 << 31) - 1;
    let [mut m0, mut m1, mut m2, mut m3, mut m4, mut m5, mut m6] = c.m;
    let mut dd = ((K - c.d01) as u64) << 32 | (K - c.d12) as u64;
    let mut q8 = (c.q * 8) as u64;
    let tp = &CHAIN_TAB as *const ChainTab as *const u8;
    let tc = unsafe { tp.add(384) }; // SAFETY: CHAIN_TAB is dd (384 bytes), code (48), nq (48): offset 384 is code, inside it
    // one step: the list (o0, o1..o6) and value register v -> the list (v, n1..n6); rp: the
    // repeat label, r: the reorder label (both forward)
    macro_rules! step {
        ($o0:literal, $o1:literal, $o2:literal, $o3:literal, $o4:literal, $o5:literal, $o6:literal, $v:literal, $n1:literal, $n2:literal, $n3:literal, $n4:literal, $n5:literal, $n6:literal, $rp:literal, $r:literal) => {
            concat!(
                "ldr ", $v, ", [{vp}], #8\n",
                "ldr {a:w}, [{sp}], #4\n",
                "lsr {p:w}, {a:w}, #31\n",
                "and {a}, {a}, #0x7fffffff\n",
                "ldr {w}, [{lp}, {a}, lsl #3]\n",
                "str ", $v, ", [{lp}, {a}, lsl #3]\n",
                "cmp ", $v, ", {w}\n",
                "cset {w}, eq\n",
                "add {a}, {q8}, {p}, lsl #2\n",
                "add {a}, {a}, {w}, lsl #1\n",
                "cmp ", $o0, ", ", $v, "\n",
                "b.eq ", $rp, "\n",
                "cset {p}, ne\n",
                "csel ", $n1, ", ", $o1, ", ", $o0, ", eq\n",
                "ccmp ", $o1, ", ", $v, ", #4, ne\n",
                "csel ", $n2, ", ", $o2, ", ", $o1, ", eq\n",
                "cinc {p}, {p}, ne\n",
                "ccmp ", $o2, ", ", $v, ", #4, ne\n",
                "csel ", $n3, ", ", $o3, ", ", $o2, ", eq\n",
                "cinc {p}, {p}, ne\n",
                "ccmp ", $o3, ", ", $v, ", #4, ne\n",
                "csel ", $n4, ", ", $o4, ", ", $o3, ", eq\n",
                "cinc {p}, {p}, ne\n",
                "ccmp ", $o4, ", ", $v, ", #4, ne\n",
                "csel ", $n5, ", ", $o5, ", ", $o4, ", eq\n",
                "cinc {p}, {p}, ne\n",
                "ccmp ", $o5, ", ", $v, ", #4, ne\n",
                "csel ", $n6, ", ", $o6, ", ", $o5, ", eq\n",
                "cinc {p}, {p}, ne\n",
                "ccmp ", $o6, ", ", $v, ", #4, ne\n",
                "cinc {p}, {p}, ne\n",
                "ldr {w}, [{tp}, {a}, lsl #3]\n",
                "add {dd}, {dd}, {w}\n",
                "ldrb {w:w}, [{tc}, {a}]\n",
                "add {p}, {p}, #2\n",
                "cmp {w}, #3\n",
                "csel {w}, {p}, {w}, eq\n",
                "strb {w:w}, [{op}], #1\n",
                "tst {dd}, #0x8000000080000000\n",
                "b.ne ", $r, "\n",
            )
        };
    }
    // a repeat (bit 0 of e: the column's previous value is right, so the code is a special's):
    // the code and the hit changes only; r: the reorder label (forward)
    macro_rules! rept {
        ($r:literal) => {
            concat!(
                "add {a}, {a}, #1\n",
                "ldr {w}, [{tp}, {a}, lsl #3]\n",
                "add {dd}, {dd}, {w}\n",
                "ldrb {w:w}, [{tc}, {a}]\n",
                "strb {w:w}, [{op}], #1\n",
                "tst {dd}, #0x8000000080000000\n",
                "b.ne ", $r, "\n",
            )
        };
    }
    // reorder (as fn reorder) on the unpacked differences, with the dead list's registers t1..t6
    // as scratch; b: the return label (backward)
    macro_rules! reord {
        ($t1:literal, $t2:literal, $t3:literal, $t3w:literal, $t4:literal, $t4w:literal, $t5:literal, $t6:literal, $b:literal) => {
            concat!(
                "lsr ", $t1, ", {dd}, #32\n",
                "mov ", $t2, ", #0x7fffffff\n",
                "sub ", $t1, ", ", $t2, ", ", $t1, "\n",
                "mov ", $t3w, ", {dd:w}\n",
                "sub ", $t2, ", ", $t2, ", ", $t3, "\n",
                "add ", $t3, ", ", $t1, ", ", $t2, "\n",
                "cmp ", $t2, ", ", $t3, "\n",
                "cset ", $t4, ", gt\n",
                "csel ", $t5, ", ", $t2, ", ", $t3, ", gt\n",
                "csel ", $t6, ", ", $t3, ", ", $t2, ", gt\n",
                "cmp ", $t6, ", #0\n",
                "cset ", $t1, ", lt\n",
                "orr ", $t4, ", ", $t4, ", ", $t1, ", lsl #1\n",
                "csel ", $t2, ", ", $t6, ", xzr, lt\n",
                "csel ", $t6, ", xzr, ", $t6, ", lt\n",
                "cmp ", $t6, ", ", $t5, "\n",
                "cset ", $t1, ", gt\n",
                "orr ", $t4, ", ", $t4, ", ", $t1, ", lsl #2\n",
                "csel ", $t1, ", ", $t6, ", ", $t5, ", gt\n",
                "csel ", $t6, ", ", $t5, ", ", $t6, ", gt\n",
                "sub ", $t1, ", ", $t1, ", ", $t6, "\n",
                "sub ", $t6, ", ", $t6, ", ", $t2, "\n",
                "mov ", $t2, ", #0x7fffffff\n",
                "sub ", $t1, ", ", $t2, ", ", $t1, "\n",
                "sub ", $t6, ", ", $t2, ", ", $t6, "\n",
                "orr {dd}, ", $t6, ", ", $t1, ", lsl #32\n",
                "add ", $t4, ", {q8}, ", $t4, "\n",
                "add ", $t4, ", ", $t4, ", #48\n",
                "ldrb ", $t4w, ", [{tc}, ", $t4, "]\n",
                "lsl {q8}, ", $t4, ", #3\n",
                "b ", $b, "\n",
            )
        };
    }
    unsafe { // SAFETY: the bounds of fn chain_asm's SAFETY note, checked by chain_codes
        core::arch::asm!(
            // the list in X: a moving step to Y (2), or a repeat that stays in X (22)
            "2:",
            step!("{m0}", "{m1}", "{m2}", "{m3}", "{m4}", "{m5}", "{m6}", "{va}", "{n1}", "{n2}", "{n3}", "{n4}", "{n5}", "{n6}", "22f", "6f"),
            "3:",
            "subs {cnt}, {cnt}, #1",
            "b.eq 9f",
            // the list in Y: a moving step to X (5), or a repeat that stays in Y (25)
            "5:",
            step!("{va}", "{n1}", "{n2}", "{n3}", "{n4}", "{n5}", "{n6}", "{m0}", "{m1}", "{m2}", "{m3}", "{m4}", "{m5}", "{m6}", "25f", "7f"),
            "4:",
            "subs {cnt}, {cnt}, #1",
            "b.ne 2b",
            "b 8f",
            "22:",
            rept!("24f"),
            "23:",
            "subs {cnt}, {cnt}, #1",
            "b.eq 8f",
            "b 2b",
            "25:",
            rept!("27f"),
            "26:",
            "subs {cnt}, {cnt}, #1",
            "b.eq 9f",
            "b 5b",
            // reorders: after a move to Y (X is free), after a move to X (Y is free), after a
            // repeat in X (Y is free), after a repeat in Y (X is free)
            "6:",
            reord!("{m1}", "{m2}", "{m3}", "{m3:w}", "{m4}", "{m4:w}", "{m5}", "{m6}", "3b"),
            "7:",
            reord!("{n1}", "{n2}", "{n3}", "{n3:w}", "{n4}", "{n4:w}", "{n5}", "{n6}", "4b"),
            "24:",
            reord!("{n1}", "{n2}", "{n3}", "{n3:w}", "{n4}", "{n4:w}", "{n5}", "{n6}", "23b"),
            "27:",
            reord!("{m1}", "{m2}", "{m3}", "{m3:w}", "{m4}", "{m4:w}", "{m5}", "{m6}", "26b"),
            // the end with the list in Y: into X
            "9:",
            "mov {m0}, {va}",
            "mov {m1}, {n1}",
            "mov {m2}, {n2}",
            "mov {m3}, {n3}",
            "mov {m4}, {n4}",
            "mov {m5}, {n5}",
            "mov {m6}, {n6}",
            "8:",
            vp = inout(reg) vp => _,
            sp = inout(reg) sp => _,
            lp = in(reg) lp,
            op = inout(reg) op => _,
            cnt = inout(reg) n => _,
            tp = in(reg) tp,
            tc = in(reg) tc,
            m0 = inout(reg) m0,
            m1 = inout(reg) m1,
            m2 = inout(reg) m2,
            m3 = inout(reg) m3,
            m4 = inout(reg) m4,
            m5 = inout(reg) m5,
            m6 = inout(reg) m6,
            va = out(reg) _,
            n1 = out(reg) _,
            n2 = out(reg) _,
            n3 = out(reg) _,
            n4 = out(reg) _,
            n5 = out(reg) _,
            n6 = out(reg) _,
            dd = inout(reg) dd,
            q8 = inout(reg) q8,
            a = out(reg) _,
            p = out(reg) _,
            w = out(reg) _,
            options(nostack),
        );
    }
    c.m = [m0, m1, m2, m3, m4, m5, m6];
    (c.d01, c.d12, c.q) = (K - (dd >> 32) as i64, K - (dd & 0xffff_ffff) as i64, (q8 / 8) as usize);
}

/// put_nums' estimate counts, by leading zeros (z[k][j]: numbers with 64 - j bits): 0 values,
/// 1 differences, 2-3 the same for cache misses, 4-5 for misses not in the column's list, 6 the
/// list places.
struct EstZ {
    z: [[u32; 65]; 7],
}

/// The estimate's second pass over one run (vals, their codes from chain_codes, the number
/// before the run): the counts, and the column's list (a miss found there becomes code CK + 1
/// and its place goes to dix; a miss not there is added).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn est_run(vals: &[u64], mut prev: u64, codes: &mut [u8], ez: &mut EstZ, table: &mut [(u64, u32)], tb: u32, nd: &mut u32, dix: &mut Vec<u32>, use_dict: bool) {
    let mut k = *nd;
    // (a value equal to the one before it, after the run's first, is a cache hit: the chain's
    // previous value is that one, a special is right; it only adds to the counts of its value
    // and of difference 0, so a run of them is added at once)
    let mut reps = 0u32;
    for (i, (&v, s)) in vals.iter().zip(codes.iter_mut()).enumerate() {
        if v == prev && i > 0 {
            reps += 1;
            continue;
        }
        if reps > 0 {
            ez.z[0][prev.leading_zeros() as usize] += reps;
            ez.z[1][64] += reps;
            reps = 0;
        }
        let d = zz((v as i64).wrapping_sub(prev as i64));
        prev = v;
        let (lv, ld) = (v.leading_zeros() as usize, d.leading_zeros() as usize);
        ez.z[0][lv] += 1;
        ez.z[1][ld] += 1;
        if *s == CK as u8 {
            ez.z[2][lv] += 1;
            ez.z[3][ld] += 1;
            let e = &mut table[(v.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - tb)) as usize];
            if use_dict && e.0 == v {
                ez.z[6][(e.1 as u64).leading_zeros() as usize] += 1;
                dix.push(e.1);
                *s = CK as u8 + 1;
            } else {
                *e = (v, k);
                k += 1;
                ez.z[4][lv] += 1;
                ez.z[5][ld] += 1;
            }
        }
    }
    if reps > 0 {
        ez.z[0][prev.leading_zeros() as usize] += reps;
        ez.z[1][64] += reps;
    }
    *nd = k;
}

/// How many of the codes are 0, 1, .. CK + 1 (16 codes at a time; the same counts as one by
/// one).
fn code_counts(codes: &[u8]) -> [u32; CK + 2] {
    let mut h = [0u32; CK + 2];
    let mut ch = codes.chunks_exact(16);
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        // (byte counters, added into h every 255 rounds before they could wrap)
        let mut acc = [unsafe { vdupq_n_u8(0) }; CK + 2]; // SAFETY: NEON is part of every aarch64 target; registers only
        let mut r = 0;
        for x in &mut ch {
            // SAFETY: x holds 16 bytes
            let v = unsafe { vld1q_u8(x.as_ptr()) }; // SAFETY: x is a chunk of exactly 16 bytes
            for (k, a) in acc.iter_mut().enumerate() {
                *a = unsafe { vsubq_u8(*a, vceqq_u8(v, vdupq_n_u8(k as u8))) }; // SAFETY: NEON (every aarch64 target); registers only
            }
            r += 1;
            if r == 255 {
                for (k, a) in acc.iter_mut().enumerate() {
                    h[k] += unsafe { vaddlvq_u8(*a) } as u32; // SAFETY: NEON (every aarch64 target); registers only
                    *a = unsafe { vdupq_n_u8(0) }; // SAFETY: NEON (every aarch64 target); registers only
                }
                r = 0;
            }
        }
        for (k, a) in acc.iter().enumerate() {
            h[k] += unsafe { vaddlvq_u8(*a) } as u32; // SAFETY: NEON (every aarch64 target); registers only
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    for x in &mut ch {
        for &b in x {
            if (b as usize) < CK + 2 {
                h[b as usize] += 1;
            }
        }
    }
    for &b in ch.remainder() {
        if (b as usize) < CK + 2 {
            h[b as usize] += 1;
        }
    }
    h
}

/// The numbers of a column coded from a fresh start with cache codes `cs` (from chain_codes),
/// at o (room for 10 bytes per number), in order: each new value (code CK) as is or as its
/// difference from the number before it (delta), or with the column's list (dict) a value
/// found there as its place (its code becomes CK + 1); returns the end. The codes are looked
/// at 8 at a time: a word with no CK in it holds only cache hits, which write nothing.
/// SAFETY: room for 10 bytes per number at o.
#[inline(never)]
unsafe fn cache_nums(vals: &[u64], cs: &mut [u8], table: &mut [(u64, u32)], tb: u32, delta: bool, dict: bool, mut o: *mut u8) -> *mut u8 {
    let m = vals.len().min(cs.len());
    let mut nd = 0u32;
    // (the end, and whether the value was in the list: its code becomes CK + 1)
    let mut one = |j: usize, o: *mut u8| -> (*mut u8, bool) {
        // a new value (as is or as its difference), or a list entry (its place)
        let v = vals[j];
        let prev = if j > 0 { vals[j - 1] } else { 0 };
        let x = if delta { zz((v as i64).wrapping_sub(prev as i64)) } else { v };
        if dict {
            let e = &mut table[(v.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - tb)) as usize];
            if e.0 == v {
                return (unsafe { varint_at(o, e.1 as u64) }, true); // SAFETY: one varint of at most 10 bytes per number: the room of fn cache_nums
            }
            *e = (v, nd);
            nd += 1;
        }
        (unsafe { varint_at(o, x) }, false) // SAFETY: one varint of at most 10 bytes per number: the room of fn cache_nums
    };
    const ONES: u64 = 0x0101_0101_0101_0101;
    let mut i = 0;
    while i + 8 <= m {
        // (a byte equal to CK in the word: the exact zero-byte test on w ^ CK * ONES)
        let y = u64::from_le_bytes(cs[i..i + 8].try_into().unwrap()) ^ (CK as u64 * ONES);
        if y.wrapping_sub(ONES) & !y & (ONES << 7) != 0 {
            for j in i..i + 8 {
                if cs[j] == CK as u8 {
                    let (e, hit) = one(j, o);
                    o = e;
                    if hit {
                        cs[j] = CK as u8 + 1;
                    }
                }
            }
        }
        i += 8;
    }
    for j in i..m {
        if cs[j] == CK as u8 {
            let (e, hit) = one(j, o);
            o = e;
            if hit {
                cs[j] = CK as u8 + 1;
            }
        }
    }
    o
}

/// Write a column's numbers into `out` (cache codes into `codes`, one byte per number: CK = a
/// new value written in `out`, CK + 1 = an entry of a list, its place written in `out`);
/// returns the mode bits (1: differences, 4: cache codes, 8: the column's own list of values).
/// `slot_last` holds the last number at each pattern slot (updated here, as the opener does).
/// (M4: the same choices and bytes as ref's put_nums, kept in oldcol.rs for FL_SHADOW; the
/// cache chain runs alone first, then the counts or the numbers in a pass over its codes.)
#[inline(never)]
fn put_nums(vals: &[u64], slots: &[u32], ids: &[u32], steps: &[u64], slot_last: &mut [u64], syms: &mut Vec<u8>, dix: &mut Vec<u32>, table: &mut Vec<(u64, u32)>, sink: &mut Sink, defer1: bool, gset: &mut Vec<u64>, ek: &[u8], el: &[u64]) -> u64 {
    static NOCACHE: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    let use_cache = !*NOCACHE.get_or_init(|| env_on("FL_NOCACHE"));
    static NODICT: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    let use_dict = !*NODICT.get_or_init(|| env_on("FL_NODICT"));
    let mut mode;
    if !ids.is_empty() {
        // a column with long IDs: always cache codes; a cache miss on a long ID seen before is
        // code CK + 1 with its distance in the list, a new long ID is code CK (raw value; in an
        // ID column its step form, ids.rs, or code CK + 2 and the value)
        let m = vals.len().min(slots.len());
        let (vals, ids) = (&vals[..m], &ids[..m]);
        let (out, codes) = sink.parts(4);
        let st = codes.len();
        codes.resize(st + m, 0);
        let cs = &mut codes[st..];
        chain_codes(&mut EncCache::new(), vals, slots, slot_last, cs);
        let mut gi = 0;
        put_varints(out, 10 * m + 16, |mut o| unsafe { // SAFETY: room for 10 bytes per number; one varint (at most 10) per number
            for ((&v, &id), s) in vals.iter().zip(ids).zip(cs.iter_mut()) {
                if id == 0 {
                    // (an ID column: a generator long ID's step form, or code CK + 2 and the value)
                    let f = steps.get(gi).copied().unwrap_or(u64::MAX);
                    gi += !steps.is_empty() as usize;
                    if f == u64::MAX {
                        o = varint_at(o, v);
                        *s = if steps.is_empty() { CK as u8 } else { CK as u8 + 2 };
                    } else {
                        o = varint_at(o, f);
                        *s = CK as u8;
                    }
                } else if (*s as usize) < CK {
                } else if id != u32::MAX {
                    o = varint_at(o, (id - 1) as u64);
                    *s = CK as u8 + 1;
                } else {
                    o = varint_at(o, v);
                }
            }
            o
        });
        return 4;
    }
    if vals.len() < SMALL_COL || !use_cache {
        // as differences or as they are, whichever takes fewer bytes
        let (mut dl, mut rl, mut prev) = (0usize, 0usize, 0i64);
        for &v in vals {
            dl += 1 + (63 - (zz((v as i64).wrapping_sub(prev)) | 1).leading_zeros() as usize) / 7;
            rl += 1 + (63 - (v | 1).leading_zeros() as usize) / 7;
            prev = v as i64;
        }
        mode = (dl <= rl && vals.len() > 1) as u64; // one number: as is
    } else {
        // Estimate six ways (on EST_N numbers in EST_CHUNKS runs spread over the column):
        // values, differences, and cache codes with either for new values, with or without
        // the column's list of earlier values (code CK + 1 = a list entry, its place in the
        // nums; a direct-mapped table finds them: value -> entry; mode bit 8).
        let mut ez = EstZ { z: [[0; 65]; 7] };
        let mut c = EncCache::new();
        syms.clear();
        dix.clear();
        let tb = (vals.len().next_power_of_two().clamp(64, 4096)).trailing_zeros();
        table.clear();
        table.resize(1 << tb, (u64::MAX, u32::MAX));
        let mut nd = 0u32;
        // the numbers looked at: all, or EST_CHUNKS runs spread over the column
        let n = vals.len();
        static ESTN: crate::engine::fl::NoEnvOnce<usize> = crate::engine::fl::NoEnvOnce::new();
        let est_n = *ESTN.get_or_init(|| crate::engine::fl::no_env::var("FL_ESTN").ok().and_then(|x| x.parse().ok()).unwrap_or(EST_N));
        let (chunks, clen) = if n <= est_n { (1, n) } else { (EST_CHUNKS, est_n / EST_CHUNKS) };
        let run = |k: usize| if chunks == 1 { 0 } else { k * (n - clen) / (chunks - 1) };
        // the codes of the runs (one cache chain through them all), then the counts run by run
        syms.resize(chunks * clen, 0);
        for k in 0..chunks {
            let st = run(k);
            chain_codes(&mut c, &vals[st..st + clen], &slots[st..st + clen], slot_last, &mut syms[k * clen..(k + 1) * clen]);
        }
        for k in 0..chunks {
            let st = run(k);
            est_run(&vals[st..st + clen], if st > 0 { vals[st - 1] } else { 0 }, &mut syms[k * clen..(k + 1) * clen], &mut ez, table, tb, &mut nd, dix, use_dict);
        }
        let pre = chunks * clen;
        let bz = |k: usize| Est::bits_z(&ez.z[k]);
        let hits = code_counts(syms);
        // the codes go to their own stream: their cost is their entropy
        let n = pre as f64;
        let ent = |h: &[u32]| -> f64 { h.iter().filter(|&&c| c > 0).map(|&c| c as f64 * (n / c as f64).log2()).sum() };
        // codes without the list: CK and CK + 1 are one code
        let mut h2 = hits;
        h2[CK] += h2[CK + 1];
        h2[CK + 1] = 0;
        let hc = ent(&h2);
        let hcd = ent(&hits);
        // cache codes must win by a margin (what the estimate cannot see: copies of whole
        // runs of numbers favour the plain ways)
        static MARGIN: crate::engine::fl::NoEnvOnce<f64> = crate::engine::fl::NoEnvOnce::new();
        let margin = *MARGIN.get_or_init(|| crate::engine::fl::no_env::var("FL_CMARGIN").ok().and_then(|x| x.parse().ok()).unwrap_or(0.85));
        // (z: 0 values, 1 differences, 2-3 misses, 4-5 misses not in the list, 6 list places)
        let mut best = bz(0);
        mode = 0;
        let dl = hcd + bz(6);
        for (e, m) in [(bz(1), 1u64), ((hc + bz(2)) / margin, 4), ((hc + bz(3)) / margin, 5), ((dl + bz(4)) / margin, 12), ((dl + bz(5)) / margin, 13)] {
            if e < best {
                best = e;
                mode = m;
            }
        }
        if !ek.is_empty() {
            // keyed codes (entity.rs; the column's slots are 0 again after their estimate)
            for &sl in slots {
                slot_last[(sl & SLOT) as usize] = 0;
            }
            mode = keyed_mode(vals, slots, ek, el, slot_last, chunks, clen, best, mode, margin);
        }
        if mode & 256 != 0 {
            put_keyed(vals, slots, ek, el, slot_last, sink, mode);
            return mode;
        }
        if mode & 4 != 0 && chunks == 1 {
            // all numbers were looked at: the codes are ready (a list hit is CK without the list)
            if mode & 8 == 0 {
                syms.iter_mut().for_each(|x| *x = (*x).min(CK as u8));
            }
        } else if mode & 4 != 0 {
            // code the whole column from a fresh start (the opener's view): the codes, then the
            // numbers of the new values and list entries in order (the same bytes as ref)
            let dict = mode & 8 != 0;
            let delta = mode & 1 != 0;
            let m = vals.len();
            let sl0 = &mut slot_last[..];
            for &sl in slots {
                sl0[(sl & SLOT) as usize] = 0;
            }
            // (the list starts empty again: cache_nums counts its places from 0)
            table.iter_mut().for_each(|e| *e = (u64::MAX, u32::MAX));
            let (out, codes) = sink.parts(mode);
            let st = codes.len();
            codes.resize(st + m, 0);
            let cs = &mut codes[st..];
            chain_codes(&mut EncCache::new(), vals, slots, slot_last, cs);
            put_varints(out, 10 * m + 16, |o| unsafe { cache_nums(vals, cs, table, tb, delta, dict, o) }); // SAFETY: put_varints reserved 10 bytes per number, as cache_nums needs
            return mode;
        }
    }
    if mode & !1 == 0 && vals.len() >= 64 && dod_on() {
        // second differences (mode bit 32) when an LZ-like size guess on the first 4096 values
        // finds them at least 10% smaller than the chosen way (as is / differences)
        let g = guess_forms(&vals[..vals.len().min(4096)], gset, mode as usize);
        static DM: crate::engine::fl::NoEnvOnce<(u64, u64)> = crate::engine::fl::NoEnvOnce::new();
        let dm = *DM.get_or_init(|| {
            let e = |k: &str, d: u64| crate::engine::fl::no_env::var(k).ok().and_then(|x| x.parse().ok()).unwrap_or(d);
            (e("FL_DODM0", 150), e("FL_DODM1", 110))
        });
        if g[2] * if mode == 0 { dm.0 } else { dm.1 } < g[mode as usize] * 100 {
            mode = 32;
        }
    }
    if mode == 1 && defer1 {
        // plain differences: the caller writes them (or groups them)
        return 1;
    }
    let delta = mode & 1 != 0;
    let dict = mode & 8 != 0;
    // (one loop per way; at most 10 bytes per number)
    let (out, codes) = sink.parts(mode);
    put_varints(out, 10 * vals.len() + 16, |mut p| unsafe {
        match mode {
            0 => {
                for &v in vals {
                    p = varint_at(p, v);
                }
            }
            1 => p = put_diffs(p, vals),
            32 => {
                let (mut prev, mut pd) = (0u64, 0i64);
                for &v in vals {
                    let ds = (v as i64).wrapping_sub(prev as i64);
                    p = varint_at(p, zz(ds.wrapping_sub(pd)));
                    pd = ds;
                    prev = v;
                }
            }
            _ => {
                // cache codes: a new value as is or as its difference, a list entry as its place
                let mut prev = 0u64;
                let mut q = 0;
                for (&v, &s) in vals.iter().zip(syms.iter()) {
                    let s = s as usize;
                    if s == CK || (s == CK + 1 && !dict) {
                        p = varint_at(p, if delta { zz((v as i64).wrapping_sub(prev as i64)) } else { v });
                    } else if s == CK + 1 {
                        p = varint_at(p, dix[q] as u64);
                        q += 1;
                    }
                    prev = v;
                }
            }
        }
        p
    });
    if mode & 4 != 0 {
        let st = codes.len();
        codes.extend_from_slice(syms);
        if !dict {
            for x in &mut codes[st..] {
                *x = (*x).min(CK as u8);
            }
        }
    }
    mode
}

/// put_nums' choice of keyed codes (entity.rs) on the numbers it looks at (`chunks` runs of
/// `clen`, as put_nums): per number its rank in its entity (ek: R = a new value of a known
/// entity, R + 1 = no entity) and the entity's last new value (el). Two ways, with steps from
/// the entity's last new value (mode bit 512: KN) or without, each with the column's cache
/// codes (mode bit 4) and new numbers as they are or as differences (mode bit 1); a way is
/// taken when it beats the plain estimate `best` with put_nums' margin and by at least 1/4 bit
/// per number (on columns that cost almost nothing the coder's copies beat any estimate).
/// Returns the mode (`mode` when no keyed way is better). slot_last: the column's slots are 0
/// again after it.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn keyed_mode(vals: &[u64], slots: &[u32], ek: &[u8], el: &[u64], slot_last: &mut [u64], chunks: usize, clen: usize, mut best: f64, mut mode: u64, margin: f64) -> u64 {
    let mut kh = [[0u32; entity::KN + 1]; 2];
    let mut ke: [(Est, Est); 2] = [(Est::new(), Est::new()), (Est::new(), Est::new())];
    let mut kn = Est::new();
    let mut c = EncCache::new();
    let n = vals.len();
    let mut cnt = 0usize;
    for k in 0..chunks {
        let st = if chunks == 1 { 0 } else { k * (n - clen) / (chunks - 1) };
        let mut prev = if st > 0 { vals[st - 1] } else { 0 };
        for i in st..st + clen {
            let (v, sl) = (vals[i], slots[i]);
            let d = zz((v as i64).wrapping_sub(prev as i64));
            let s = c.step(v, slot_last[(sl & SLOT) as usize], sl > SLOT);
            slot_last[(sl & SLOT) as usize] = v;
            prev = v;
            cnt += 1;
            let e = ek[i] as usize;
            for w in 0..2 {
                let x = if e < entity::R {
                    entity::KH + e
                } else if s < CK {
                    s
                } else if e == entity::R && w == 1 {
                    entity::KN
                } else {
                    ke[w].0.add(v);
                    ke[w].1.add(d);
                    CK
                };
                kh[w][x] += 1;
            }
            if e == entity::R {
                kn.add(zz(v.wrapping_sub(el[i]) as i64));
            }
        }
    }
    for &sl in slots {
        slot_last[(sl & SLOT) as usize] = 0;
    }
    let n = cnt as f64;
    let ent = |h: &[u32]| -> f64 { h.iter().filter(|&&c| c > 0).map(|&c| c as f64 * (n / c as f64).log2()).sum() };
    let need = best - 0.25 * n;
    for w in 0..2 {
        let base = ent(&kh[w]) + if w == 1 { kn.bits() } else { 0.0 };
        let m = 260 | (w as u64) << 9;
        for (e, m) in [((base + ke[w].0.bits()) / margin, m), ((base + ke[w].1.bits()) / margin, m | 1)] {
            if e < best && e <= need {
                best = e;
                mode = m;
            }
        }
    }
    mode
}

/// A column in a keyed mode (put_nums, mode bits 256 and 4), coded from a fresh start (the
/// opener's view): one code per number (KH + r: rank r of its entity; a cache code; with mode
/// bit 512 KN: a step from its entity's last new value, in the numbers; else CK: a new value
/// as is or (mode bit 1) as its difference from the column's previous number).
#[cold]
#[inline(never)]
fn put_keyed(vals: &[u64], slots: &[u32], ek: &[u8], el: &[u64], slot_last: &mut [u64], sink: &mut Sink, mode: u64) {
    for &sl in slots {
        slot_last[(sl & SLOT) as usize] = 0;
    }
    let mut c = EncCache::new();
    let (out, codes) = sink.parts(mode);
    let mut prev = 0u64;
    for (i, (&v, &sl)) in vals.iter().zip(slots).enumerate() {
        let ks = sl > SLOT;
        let sl = (sl & SLOT) as usize;
        let s = c.step(v, slot_last[sl], ks);
        let e = ek[i] as usize;
        let code = if e < entity::R {
            entity::KH + e
        } else if s < CK {
            s
        } else if e == entity::R && mode & 512 != 0 {
            put_varint(out, zz(v.wrapping_sub(el[i]) as i64));
            entity::KN
        } else {
            put_varint(out, if mode & 1 != 0 { zz(v.wrapping_sub(prev) as i64) } else { v });
            CK
        };
        codes.push(code as u8);
        slot_last[sl] = v;
        prev = v;
    }
}

/// value kinds: 0 number, 1 text, 2 same text as last in column, 3+w number zero-padded to
/// width w (w <= 19), 24 long ID (LONG_ID), 30 IPv4, 40-67 clock time, 68 clock time with no
/// padding (H:M:S:F), 71-77 one of the column's recent texts, 100-103 UUID / hex bytes,
/// 110-123 clock time with dots, 132-149 hex number (HEXNUM), 150-155 short hex number (HEXS),
/// 162-184 space-padded number (SPN)
struct Packed {
    streams: Vec<Vec<u8>>, // templates, tids, colhdr, kinds, nums, txt, cache codes, differences
    lines: usize,
    raw: u32, // bit k: stream k is already coded (kept as is, not passed through ozip)
    alt: Vec<Vec<u8>>, // stream 1 in other forms (id_forms): the one that codes smallest is kept
    // frames pack_block coded already (keyed slots' check): (stream, hash_bytes of the stream,
    // its frame at the product's level), used by code_streams when the stream still has that hash
    pre: Vec<(usize, u64, Vec<u8>, Option<fz::Prep>)>,
    mk: u64,  // packing (B5): the derive markers in the lines' patterns (per line, added up)
}

#[allow(dead_code)]
fn hexlike(t: &[u8]) -> bool {
    if t.len() < 6 {
        return false;
    }
    let (mut hl, mut d) = (false, false);
    for &c in t {
        match CLS[c as usize] {
            1 => d = true,
            3 => hl = true,
            _ if c == b'-' => {}
            _ => return false,
        }
    }
    hl && d
}

/// 100 / 101: UUID (8-4-4-4-12) in lower / upper case; 102 / 103: even-length hex in lower /
/// upper case; 1: anything else (kept as text).
fn hex_kind(t: &[u8]) -> u8 {
    let lower = t.iter().all(|&c| !c.is_ascii_uppercase());
    let upper = t.iter().all(|&c| !c.is_ascii_lowercase());
    if !lower && !upper {
        return 1;
    }
    let base = if lower { 0 } else { 1 };
    if t.len() == 36 && t.iter().enumerate().all(|(i, &c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() }) {
        return 100 + base;
    }
    if t.len() % 2 == 0 && t.iter().all(|c| c.is_ascii_hexdigit()) {
        return 102 + base;
    }
    1
}

/// An IPv4 address (four numbers 0-255, no leading zeros) at l[k..]: (end, value).
fn ipv4_at(l: &[u8], k: usize, j: usize) -> Option<(usize, u64)> {
    if k > 0 && (CLS[l[k - 1] as usize] == 1 || l[k - 1] == b'.') {
        return None;
    }
    let mut p = k;
    let mut v = 0u64;
    for part in 0..4 {
        let s = p;
        let mut x = 0u64;
        while p < j && CLS[l[p] as usize] == 1 && p - s < 3 {
            x = x * 10 + (l[p] - b'0') as u64;
            p += 1;
        }
        let w = p - s;
        if w == 0 || x > 255 || (w > 1 && l[s] == b'0') || (p < j && CLS[l[p] as usize] == 1) {
            return None;
        }
        v = v << 8 | x;
        if part < 3 {
            if p >= j || l[p] != b'.' {
                return None;
            }
            p += 1;
        }
    }
    if p < j && (l[p] == b'.' && p + 1 < j && CLS[l[p + 1] as usize] == 1) {
        return None; // 1.2.3.4.5: a version, not an address
    }
    Some((p, v))
}

/// A clock time H:MM:SS or HH:MM:SS, optionally with . or , and 1-6 fraction digits, at l[k..]:
/// (end, kind, value). kind = 40 + (hour width - 1) + 2 * (comma) + 4 * fraction digits.
fn time_at(l: &[u8], k: usize, j: usize) -> Option<(usize, u8, u64)> {
    if k > 0 && (CLS[l[k - 1] as usize] == 1 || l[k - 1] == b':') {
        return None;
    }
    let d = |p: usize| p < j && CLS[l[p] as usize] == 1;
    let hw = if d(k) && d(k + 1) { 2 } else if d(k) { 1 } else { return None };
    let mut p = k + hw;
    let h = if hw == 2 { (l[k] - b'0') as u64 * 10 + (l[k + 1] - b'0') as u64 } else { (l[k] - b'0') as u64 };
    let two = |p: &mut usize| -> Option<u64> {
        if *p < j && l[*p] == b':' && d(*p + 1) && d(*p + 2) && !d(*p + 3) {
            let x = (l[*p + 1] - b'0') as u64 * 10 + (l[*p + 2] - b'0') as u64;
            *p += 3;
            if x < 60 { Some(x) } else { None }
        } else {
            None
        }
    };
    let m = two(&mut p)?;
    let sec = two(&mut p)?;
    if p < j && l[p] == b':' {
        return None;
    }
    let mut v = (h * 60 + m) * 60 + sec;
    let mut kind = 40 + (hw as u8 - 1);
    if p + 1 < j && (l[p] == b'.' || l[p] == b',') && d(p + 1) {
        let comma = l[p] == b',';
        let s = p + 1;
        let mut q = s;
        let mut f = 0u64;
        while d(q) && q - s < 6 {
            f = f * 10 + (l[q] - b'0') as u64;
            q += 1;
        }
        if !d(q) {
            let fd = (q - s) as u64;
            v = v * 10u64.pow(fd as u32) + f;
            kind += 2 * comma as u8 + 4 * fd as u8;
            p = q;
        }
    }
    Some((p, kind, v))
}

/// A clock time with dots, H.MM.SS or HH.MM.SS, optionally with .F (1-6 fraction digits) (BGL:
/// "15.42.50.675872") at l[k..]: (end, kind, value). kind = 110 + (hour width - 1) + 2 * digits.
fn dotted_time_at(l: &[u8], k: usize, j: usize) -> Option<(usize, u8, u64)> {
    if k > 0 && (CLS[l[k - 1] as usize] == 1 || l[k - 1] == b'.') {
        return None;
    }
    let d = |p: usize| p < j && CLS[l[p] as usize] == 1;
    let hw = if d(k) && d(k + 1) { 2 } else if d(k) { 1 } else { return None };
    let mut p = k + hw;
    let h = if hw == 2 { (l[k] - b'0') as u64 * 10 + (l[k + 1] - b'0') as u64 } else { (l[k] - b'0') as u64 };
    let two = |p: &mut usize| -> Option<u64> {
        if *p < j && l[*p] == b'.' && d(*p + 1) && d(*p + 2) && !d(*p + 3) {
            let x = (l[*p + 1] - b'0') as u64 * 10 + (l[*p + 2] - b'0') as u64;
            *p += 3;
            if x < 60 { Some(x) } else { None }
        } else {
            None
        }
    };
    let m = two(&mut p)?;
    let sec = two(&mut p)?;
    let mut v = (h * 60 + m) * 60 + sec;
    let mut kind = 110 + (hw as u8 - 1);
    if p + 1 < j && l[p] == b'.' && d(p + 1) {
        let s = p + 1;
        let mut q = s;
        let mut f = 0u64;
        while d(q) && q - s < 6 {
            f = f * 10 + (l[q] - b'0') as u64;
            q += 1;
        }
        if !d(q) {
            let fd = (q - s) as u64;
            v = v * 10u64.pow(fd as u32) + f;
            kind += 2 * fd as u8;
            p = q;
        }
    }
    if p < j && (l[p] == b'.' || CLS[l[p] as usize] == 1) {
        return None; // more dotted parts: not a time
    }
    Some((p, kind, v))
}

/// A clock time H:M:S:F with no part zero-padded (HealthApp: "22:15:29:606", "0:0:0:190")
/// at l[k..]: (end, milliseconds). H < 100, M and S < 60, F < 1000. Kind 68.
fn loose_time_at(l: &[u8], k: usize, j: usize) -> Option<(usize, u64)> {
    if k > 0 && (CLS[l[k - 1] as usize] == 1 || l[k - 1] == b':') {
        return None;
    }
    let mut p = k;
    let mut v = 0u64;
    for (part, &(maxw, lim, mul)) in [(2, 100, 1), (2, 60, 60), (2, 60, 60), (3, 1000, 1000)].iter().enumerate() {
        if part > 0 {
            if p >= j || l[p] != b':' {
                return None;
            }
            p += 1;
        }
        let s = p;
        let mut x = 0u64;
        while p < j && CLS[l[p] as usize] == 1 && p - s < maxw {
            x = x * 10 + (l[p] - b'0') as u64;
            p += 1;
        }
        let w = p - s;
        if w == 0 || (w > 1 && l[s] == b'0') || x >= lim || (p < j && CLS[l[p] as usize] == 1) {
            return None;
        }
        v = v * mul + x;
    }
    if p < j && l[p] == b':' {
        return None;
    }
    Some((p, v))
}

#[inline]
fn hexval(c: u8) -> u8 {
    if c <= b'9' { c - b'0' } else { (c | 0x20) - b'a' + 10 }
}

/// Case of hex digits: 0 no capitals, 1 no small letters, 2 both.
#[inline]
fn hex_case(t: &[u8]) -> u8 {
    let up = t.iter().any(|c| c.is_ascii_uppercase());
    let low = t.iter().any(|c| c.is_ascii_lowercase());
    if up && low { 2 } else { up as u8 }
}

/// The pad of a number of w digits (no leading zero) starting a word at l[s]: the spaces
/// before it beyond one, when there are 1+ such spaces after some text and the padded width
/// stays within SPN_MAX (else 0).
#[inline]
fn space_pad(l: &[u8], s: usize, w: usize) -> usize {
    if s < 3 || l[s - 1] != b' ' || l[s - 2] != b' ' || (w > 1 && l[s] == b'0') {
        return 0;
    }
    let mut a = s - 2;
    while a > 0 && l[a - 1] == b' ' {
        a -= 1;
    }
    let p = s - a - 1;
    if a == 0 || p + w > SPN_MAX as usize { 0 } else { p }
}

/// Knobs from the cheap-knobs idea, on by default (switches turn them off for tests):
/// FL_NONAMES  month and weekday names stay pattern text (else: values of kind NAMEK);
/// FL_TIDM     pattern ids coded with TidModel (else, the default: varints through the coder);
/// FL_NODOD    no second differences for number columns.
fn names_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NONAMES"))
}
fn tidm_on() -> bool {
    // the product setting codes pattern ids as varints through the coder (TidModel's range
    // coder: 1.6 more average ratio, 12% slower opening); FL_TIDM=1 turns TidModel on
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| env_on("FL_TIDM") && !env_on("FL_NOTIDM"))
}
/// FL_NOWORDS: no word picker (varying words stay pattern text).
fn words_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NOWORDS"))
}
fn dod_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NODOD"))
}
/// FL_NORULES: no rules (rules.rs).
fn rules_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NORULES"))
}

/// Value kind of a month or weekday name: its place in NAMES (a number).
const NAMEK: u8 = 90;

/// Month and weekday names (kind 80, value = place here): short names, then full names.
const NAMES: [&[u8]; 38] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
    b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun",
    b"January", b"February", b"March", b"April", b"May", b"June", b"July", b"August", b"September", b"October", b"November", b"December",
    b"Monday", b"Tuesday", b"Wednesday", b"Thursday", b"Friday", b"Saturday", b"Sunday",
];

/// A quick test for names: the first 3 letters as a key into a small table (no misses for the
/// 19 short names: NAME_MUL was searched so that they all land on different slots).
const NAME_MUL: u32 = 0xc69d_4bd9;
static NAME_SLOT: [u8; 32] = {
    let mut t = [0u8; 32];
    let mut k = 0;
    while k < 19 {
        let n = NAMES[k];
        let key = n[0] as u32 | (n[1] as u32) << 8 | (n[2] as u32) << 16;
        t[(key.wrapping_mul(NAME_MUL) >> 27) as usize] = k as u8 + 1;
        k += 1;
    }
    t
};

/// A month or weekday name at the start of the word l[i..j] (no digits in it), or right after
/// one opening sign such as '[': (start, end, place in NAMES). The name must not run on into
/// more letters ("Junk" is not "Jun").
#[inline]
fn name_at(l: &[u8], i: usize, j: usize) -> Option<(usize, usize, u64)> {
    if j - i < 3 {
        return None;
    }
    let p = i + (j - i > 3 && !l[i].is_ascii_alphanumeric()) as usize;
    let key = l[p] as u32 | (l[p + 1] as u32) << 8 | (l[p + 2] as u32) << 16;
    let k = NAME_SLOT[(key.wrapping_mul(NAME_MUL) >> 27) as usize] as usize;
    if k == 0 || NAMES[k - 1][..] != l[p..p + 3] {
        return None;
    }
    let k = k - 1;
    let mut q = p + 3;
    while q < j && l[q].is_ascii_lowercase() {
        q += 1;
    }
    if q < j && l[q].is_ascii_alphabetic() {
        return None;
    }
    if q == p + 3 {
        Some((p, q, k as u64))
    } else if NAMES[19 + k] == &l[p..q] {
        Some((p, q, 19 + k as u64))
    } else {
        None
    }
}

/// One coded yes/no: packing passes the bit and gets it back, opening reads it from the coded
/// bytes; so the same model code runs on both sides.
trait Bits {
    fn bit(&mut self, p: &mut u16, b: u32) -> u32;
}
impl Bits for ozip::RcEnc {
    #[inline]
    fn bit(&mut self, p: &mut u16, b: u32) -> u32 {
        ozip::RcEnc::bit(self, p, b);
        b
    }
}
impl Bits for ozip::RcDec<'_> {
    #[inline]
    fn bit(&mut self, p: &mut u16, _b: u32) -> u32 {
        ozip::RcDec::bit(self, p)
    }
}

const NONE: usize = usize::MAX;
const P_HALF: u16 = 1024; // probability 1/2 (ozip's range coder: 11 bits)

/// FL_TIDM: the pattern ids of a block, coded with small learning models. Guesses, in order:
/// 1. "match": the id that came after the last time the same 4 ids came (it goes on while
///    it keeps being right);
/// 2. the 4 patterns that most recently followed the previous line's pattern;
/// 3. "a pattern not used before in the block" (ids are numbered in order of first use);
/// 4. else the id itself, bit by bit.
/// Each guess costs one yes/no, coded with a probability that learns (ozip's range coder).
struct TidModel {
    hist: Vec<u32>,      // the ids so far
    table: Vec<u32>,     // hash of 4 ids -> place in hist right after them (0 = none)
    m: usize,            // match: hist[m] is the guess, or NONE
    mlen: usize,         // times in a row the match was right
    succ: Vec<[u32; 4]>, // per previous id (+1; 0 = block start): the ids that followed it
    run: Vec<u8>,        // per previous id: times in a row its first follower was right (0-3)
    last: usize,         // 1 if the line before was guessed by the match or first follower
    seen: u32,           // ids used so far (the next new pattern gets this id)
    nb: u32,             // bits of an id
    pm: [u16; 32],       // probabilities: match right? (by run length, last)
    ph: [u16; 64],       // follower r right? (by r, run, last, match there)
    pnew: [u16; 4],      // a new pattern? (by run)
    ptree: Vec<u16>,     // the id's bits
}

impl TidModel {
    fn new(patterns: usize) -> TidModel {
        let nb = usize::BITS - patterns.max(1).leading_zeros();
        TidModel {
            hist: Vec::new(),
            table: vec![0; 1 << 16],
            m: NONE,
            mlen: 0,
            succ: vec![[u32::MAX; 4]; patterns + 1],
            run: vec![0; patterns + 1],
            last: 0,
            seen: 0,
            nb,
            pm: [P_HALF; 32],
            ph: [P_HALF; 64],
            pnew: [P_HALF; 4],
            ptree: vec![P_HALF; 2 << nb],
        }
    }

    /// Code one id: `t` is the id when packing (unused when opening); returns the id.
    fn step<C: Bits>(&mut self, c: &mut C, t: u32) -> u32 {
        let ctx = self.hist.last().map_or(0, |&x| x as usize + 1);
        let st = self.run[ctx] as usize;
        let mi = (self.m != NONE) as usize;
        let guess = if mi == 1 { self.hist[self.m] } else { u32::MAX };
        let mut id = u32::MAX;
        let matched = mi == 1 && c.bit(&mut self.pm[self.mlen.min(15) * 2 + self.last], (t != guess) as u32) == 0;
        if matched {
            id = guess;
        } else {
            for (r, &x) in self.succ[ctx].iter().enumerate() {
                if x == u32::MAX {
                    break;
                }
                if x == guess {
                    continue; // the match already said no
                }
                if c.bit(&mut self.ph[((r * 4 + st) * 2 + self.last) * 2 + mi], (t != x) as u32) == 0 {
                    id = x;
                    break;
                }
            }
            if id == u32::MAX {
                if c.bit(&mut self.pnew[st], (t != self.seen) as u32) == 0 {
                    id = self.seen;
                } else {
                    let mut k = 1usize;
                    for i in (0..self.nb).rev() {
                        let b = c.bit(&mut self.ptree[k], (t >> i) & 1);
                        k = k << 1 | b as usize;
                    }
                    id = (k - (1 << self.nb)) as u32;
                }
            }
        }
        // the followers of the previous pattern: this id moves to the front
        let l = &mut self.succ[ctx];
        let pos = l.iter().position(|&x| x == id);
        let r = pos.unwrap_or(3);
        l.copy_within(0..r, 1);
        l[0] = id;
        self.run[ctx] = if pos == Some(0) { (st + 1).min(3) as u8 } else { 0 };
        self.last = (matched || pos == Some(0)) as usize;
        if id == self.seen {
            self.seen += 1;
        }
        self.hist.push(id);
        // the match goes on, or look for the last time the last 4 ids came
        if matched {
            self.m += 1;
            self.mlen += 1;
        } else {
            self.m = NONE;
            self.mlen = 0;
        }
        let n = self.hist.len();
        if n >= 4 {
            let h = &self.hist[n - 4..];
            let x = (h[0] as u64) | (h[1] as u64) << 16 | (h[2] as u64) << 32 | (h[3] as u64) << 48;
            let k = (x.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize;
            if self.m == NONE {
                let p = self.table[k] as usize;
                if p >= 4 && self.hist[p - 4..p] == self.hist[n - 4..] {
                    self.m = p;
                }
            }
            self.table[k] = n as u32;
        }
        id
    }
}

/// ozip's "stored" form, for a stream that is already coded.
fn stored(s: &[u8]) -> Vec<u8> {
    let mut o = vec![0u8];
    put_varint(&mut o, s.len() as u64);
    o.extend_from_slice(s);
    o
}


/// Line splitting settings (env switches, read once per block).
#[derive(Clone, Copy)]
struct SplitCfg {
    use_smart: bool,
    loose: bool,
    dotted: bool,
    hexnum: bool,
    maxw: usize,
    use_ids: bool,
    names: bool,
    hex5: bool,
    spaced: bool,
}

impl SplitCfg {
    fn get() -> SplitCfg {
        SplitCfg {
            use_smart: crate::engine::fl::no_env::var_os("FL_NOSMART").is_none(),
            loose: crate::engine::fl::no_env::var_os("FL_NOLOOSE").is_none(),
            dotted: crate::engine::fl::no_env::var_os("FL_NODOTTED").is_none(),
            hexnum: crate::engine::fl::no_env::var_os("FL_NOHEXNUM").is_none(),
            // digit runs longer than this stay text (19 digits always fit in 64 bits)
            maxw: crate::engine::fl::no_env::var("FL_MAXW").ok().and_then(|x| x.parse().ok()).unwrap_or(19),
            use_ids: crate::engine::fl::no_env::var_os("FL_NOIDS").is_none(),
            names: names_on(),
            hex5: crate::engine::fl::no_env::var_os("FL_NOHEX5").is_none(),
            spaced: crate::engine::fl::no_env::var_os("FL_NOSPACED").is_none(),
        }
    }
}

/// One value of a line: (kind, number, start, end in the line).
type Val = (u8, u64, u32, u32);

// ---------------------------------------------------------------- frozen interfaces (DESIGN.md 7b)
// Fixed in M0 (2026-11-15) so the redesign's milestones code against the same types. Each has a
// stand-in built from today's data; the default path does not use them yet (the bytes are ref's).
// FL_SHADOW (mod shadow) checks every stand-in against today's data on every block.

/// Val16 (M2, then M3 and M6): a value's number, its span in the line and its kind, read only
/// through these accessors. Today's 24-byte `Val` is the stand-in; `Val16` is the 16-byte row M2
/// brings (end:24 and kind:8 in one u32: a block whose values end past 16 MB keeps the 24-byte path).
#[allow(dead_code)]
trait ValAcc {
    fn val(&self) -> u64;
    fn span(&self) -> (u32, u32);
    fn kind(&self) -> u8;
}
impl ValAcc for Val {
    #[inline(always)]
    fn val(&self) -> u64 {
        self.1
    }
    #[inline(always)]
    fn span(&self) -> (u32, u32) {
        (self.2, self.3)
    }
    #[inline(always)]
    fn kind(&self) -> u8 {
        self.0
    }
}
/// The 16-byte value row (M2): value, start, end | kind << 24.
#[allow(dead_code)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C)]
struct Val16 {
    v: u64,
    start: u32,
    ek: u32,
}
const _: () = assert!(std::mem::size_of::<Val16>() == 16);
/// The largest end a Val16 holds.
const VAL16_END_MAX: u32 = (1 << 24) - 1;
#[allow(dead_code)]
impl Val16 {
    /// None when the value ends past VAL16_END_MAX (that block keeps the 24-byte path)
    #[inline(always)]
    fn from_val(x: &Val) -> Option<Val16> {
        if x.3 > VAL16_END_MAX {
            return None;
        }
        Some(Val16 { v: x.1, start: x.2, ek: x.3 | (x.0 as u32) << 24 })
    }
}
impl ValAcc for Val16 {
    #[inline(always)]
    fn val(&self) -> u64 {
        self.v
    }
    #[inline(always)]
    fn span(&self) -> (u32, u32) {
        (self.start, self.ek & VAL16_END_MAX)
    }
    #[inline(always)]
    fn kind(&self) -> u8 {
        (self.ek >> 24) as u8
    }
}

/// A line's shape (its fixed text, values replaced by MARK) and its values, both appended.
fn split_line(l: &[u8], cfg: &SplitCfg, shape: &mut Vec<u8>, vals: &mut Vec<Val>) {
    let SplitCfg { use_smart, loose, dotted, hexnum, maxw, use_ids, names, hex5, spaced } = *cfg;
    if l.contains(&MARK) {
        // raw line: template 0 = one text value
        shape.push(MARK);
        vals.push((1, 0, 0, l.len() as u32));
    } else {
        let n = l.len();
        let mut i = 0;
        while i <= n {
            let mut j = i;
            let mut has_d = false;
            while j < n && l[j] != b' ' {
                has_d |= CLS[l[j] as usize] == 1;
                j += 1;
            }
            let tok = &l[i..j];
            if !has_d {
                match if names { name_at(l, i, j) } else { None } {
                    Some((p, q, x)) => {
                        shape.extend_from_slice(&l[i..p]);
                        shape.push(MARK);
                        vals.push((NAMEK, x, p as u32, q as u32));
                        shape.extend_from_slice(&l[q..j]);
                    }
                    None => shape.extend_from_slice(tok),
                }
            } else {
                let mut k = i;
                while k < j {
                    let ck = CLS[l[k] as usize];
                    // an address or a clock time starts with 1-3 digits and then '.' or ':'
                    let mut e = k;
                    while e < j && e - k < 4 && CLS[l[e] as usize] == 1 {
                        e += 1;
                    }
                    let nx = if e < j && e - k < 4 { l[e] } else { 0 };
                    if ck == 1 && use_smart && nx == b'.' {
                        if let Some((e, v)) = ipv4_at(l, k, j) {
                            shape.push(MARK);
                            vals.push((30, v, k as u32, e as u32));
                            k = e;
                            continue;
                        }
                        if dotted {
                            if let Some((e, kind, v)) = dotted_time_at(l, k, j) {
                                shape.push(MARK);
                                vals.push((kind, v, k as u32, e as u32));
                                k = e;
                                continue;
                            }
                        }
                    }
                    if ck == 1 && use_smart && nx == b':' {
                        if let Some((e, kind, v)) = time_at(l, k, j) {
                            shape.push(MARK);
                            vals.push((kind, v, k as u32, e as u32));
                            k = e;
                            continue;
                        }
                        if loose {
                            if let Some((e, v)) = loose_time_at(l, k, j) {
                                shape.push(MARK);
                                vals.push((68, v, k as u32, e as u32));
                                k = e;
                                continue;
                            }
                        }
                    }
                    if ck == 1 || ck == 3 {
                        // a UUID or a long hex run (8+ hex characters with a digit) inside the word
                        let r = if j - k >= 36 && hex_kind(&l[k..k + 36]) >= 100 && hex_kind(&l[k..k + 36]) <= 101 {
                            k + 36
                        } else {
                            let mut r = k;
                            let (mut d, mut h) = (false, false);
                            while r < j && matches!(CLS[l[r] as usize], 1 | 3) {
                                d |= CLS[l[r] as usize] == 1;
                                h |= CLS[l[r] as usize] == 3;
                                r += 1;
                            }
                            if r - k >= 8 && d && h {
                                r
                            } else if hex5 && r - k >= 5 && d && h && (k == i || !l[k - 1].is_ascii_alphanumeric()) && (r == j || !l[r].is_ascii_alphanumeric()) && hex_case(&l[k..r]) < 2 {
                                // 5-7 hex digits alone between signs: a short hex number
                                shape.push(MARK);
                                let v = l[k..r].iter().fold(0u64, |a, &c| a << 4 | hexval(c) as u64);
                                vals.push((HEXS + (r - k - 5) as u8 + 3 * hex_case(&l[k..r]), v, k as u32, r as u32));
                                k = r;
                                continue;
                            } else {
                                k
                            }
                        };
                        if r > k {
                            shape.push(MARK);
                            let hk = hex_kind(&l[k..r]);
                            let upper = l[k..r].iter().any(|c| c.is_ascii_uppercase());
                            let lower = l[k..r].iter().any(|c| c.is_ascii_lowercase());
                            if hexnum && r - k <= 16 && !(upper && lower) && hk != 100 && hk != 101 {
                                // 8-16 hex digits in one case: a number (kinds HEXNUM..: width and case)
                                let v = l[k..r].iter().fold(0u64, |a, &c| a << 4 | hexval(c) as u64);
                                vals.push((HEXNUM + (r - k - 8) as u8 + 9 * upper as u8, v, k as u32, r as u32));
                            } else {
                                vals.push((hk, 0, k as u32, r as u32));
                            }
                            k = r;
                            continue;
                        }
                    }
                    if ck == 1 {
                        let s = k;
                        let mut v = 0u64;
                        while k < j && CLS[l[k] as usize] == 1 {
                            v = v.wrapping_mul(10).wrapping_add((l[k] - b'0') as u64);
                            k += 1;
                        }
                        let w = k - s;
                        if use_ids && (15..=19).contains(&w) && l[s] != b'0' && v < 1 << 63 {
                            // a minus sign right before it (not after a letter or digit) is part of it
                            let neg = s > i && l[s - 1] == b'-' && (s - 1 == i || !l[s - 2].is_ascii_alphanumeric());
                            if neg {
                                shape.pop();
                            }
                            shape.push(MARK);
                            vals.push((LONG_ID, v << 1 | neg as u64, s as u32, k as u32));
                            continue;
                        }
                        if w > maxw {
                            shape.push(MARK);
                            vals.push((1, 0, s as u32, k as u32));
                        } else if w > 1 && l[s] == b'0' {
                            shape.push(MARK);
                            vals.push((3 + w as u8, v, s as u32, k as u32));
                        } else {
                            // spaces before the word beyond its separating one: part of the number
                            let p = if spaced && s == i { space_pad(l, s, w) } else { 0 };
                            shape.truncate(shape.len() - p);
                            shape.push(MARK);
                            vals.push((if p > 0 { SPN + (p + w) as u8 } else { 0 }, v, (s - p) as u32, k as u32));
                        }
                    } else {
                        shape.push(l[k]);
                        k += 1;
                    }
                }
            }
            if j < n {
                shape.push(b' ');
            }
            i = j + 1;
        }
    }
}

/// The last 8 different words of a column, newest first (move to front).
#[derive(Default, Clone, Copy)]
struct Recent8 {
    w: [u32; 8],
    n: u8,
}

impl Recent8 {
    #[inline]
    fn find(&self, id: u32) -> Option<usize> {
        self.w[..self.n as usize].iter().position(|&w| w == id)
    }
    /// Move the k-th word to the front and return it.
    #[inline]
    fn hit(&mut self, k: usize) -> u32 {
        if k > 0 {
            self.w[..=k & 7].rotate_right(1);
        }
        self.w[0]
    }
    /// A new word in front (the oldest of 8 drops out).
    #[inline]
    fn push(&mut self, id: u32) {
        self.n = (self.n + 1).min(8);
        self.w[..self.n as usize].rotate_right(1);
        self.w[0] = id;
    }
}

/// Value kind of a word turned into a value: its number in the block's word table (in nums);
/// kinds WORD + 1 + k: the k-th recent word of the column (k < 8).
const WORD: u8 = 79;

/// Packing only: a whole token (tokens.rs), coded as a WORD.
const TOKK: u8 = 255;

#[inline]
fn hash_bytes(b: &[u8]) -> u64 {
    // Fx's write (8 bytes per step, then the last 0-7 bytes one at a time), the last bytes
    // unrolled: the same value
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let st = |h: u64, x: u64| (h.rotate_left(5) ^ x).wrapping_mul(K);
    let mut h = 0x2545_f491_4f6c_dd1du64;
    let mut c = b.chunks_exact(8);
    for x in &mut c {
        h = st(h, u64::from_le_bytes(x.try_into().unwrap()));
    }
    let mut r = c.remainder();
    if r.len() >= 4 {
        h = st(st(st(st(h, r[0] as u64), r[1] as u64), r[2] as u64), r[3] as u64);
        r = &r[4..];
    }
    if r.len() >= 2 {
        h = st(st(h, r[0] as u64), r[1] as u64);
        r = &r[2..];
    }
    if let Some(&x) = r.first() {
        h = st(h, x as u64);
    }
    h ^ b.len() as u64
}

/// A run of text in a shape that becomes one word value: its offset and length in the shape,
/// the values before it in the shape, and a hash of its text.
#[derive(Clone, Copy)]
struct Run {
    off: u32,
    len: u32,
    k0: u32,
    h: u64,
}

/// Settings of the word picker.
struct Pick {
    kmin: u32,     // different words a place needs before it becomes a value
    ctx: usize,    // other fixed words the shape must keep besides it
    rounds: usize, // merge rounds
    smin: f64,     // the words of a place must be candidates in this many shapes on average
}

const PICK: Pick = Pick { kmin: 4, ctx: 1, rounds: 4, smin: 3.0 };

/// PICK, with FL_KMIN / FL_CTX / FL_ROUNDS / FL_SMIN for tests.
fn pick_cfg() -> Pick {
    let e = |k: &str, d: f64| crate::engine::fl::no_env::var(k).ok().and_then(|x| x.parse().ok()).unwrap_or(d);
    Pick { kmin: e("FL_KMIN", PICK.kmin as f64) as u32, ctx: e("FL_CTX", PICK.ctx as f64) as usize, rounds: e("FL_ROUNDS", PICK.rounds as f64) as usize, smin: e("FL_SMIN", PICK.smin) }
}

/// A small open-addressing table from 64-bit hashes to numbers (grows at half full). A slot
/// holds its key and its number in 16 bytes, so a probe reads one cache line; `prefetch` starts
/// that read early. The numbers given out are those of TabOld (the same keys in the same order
/// get the same numbers; only where they sit differs).
struct Tab {
    e: Vec<[u64; 2]>, // (key, number); key 0: empty
    used: usize,
}

impl Tab {
    fn new(n: usize) -> Tab {
        let cap = (n * 2).next_power_of_two().max(64);
        Tab { e: vec![[0; 2]; cap], used: 0 }
    }
    /// Start loading the home slot of hash `k` (a hint only).
    #[inline(always)]
    fn prefetch(&self, k: u64) {
        prefetch(self.e.as_ptr().wrapping_add((k.max(1) >> 17) as usize & (self.e.len() - 1)));
    }
    /// The number stored for hash `k`, if any.
    fn get(&self, k: u64) -> Option<u32> {
        let k = k.max(1);
        let mask = self.e.len() - 1;
        let mut i = (k >> 17) as usize & mask;
        loop {
            let x = self.e[i];
            if x[0] == k {
                return Some(x[1] as u32);
            }
            if x[0] == 0 {
                return None;
            }
            i = (i + 1) & mask;
        }
    }
    /// The number stored for hash `k`, or `new` (stored now) when `k` is not there yet.
    #[inline]
    fn get_or(&mut self, k: u64, new: u32) -> u32 {
        if self.used * 2 >= self.e.len() {
            self.grow();
        }
        let k = k.max(1); // 0 marks an empty slot
        let mask = self.e.len() - 1;
        let mut i = (k >> 17) as usize & mask;
        loop {
            let x = self.e[i];
            if x[0] == k {
                return x[1] as u32;
            }
            if x[0] == 0 {
                self.e[i] = [k, new as u64];
                self.used += 1;
                return new;
            }
            i = (i + 1) & mask;
        }
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let mut t = Tab::new(self.e.len());
        for x in &self.e {
            if x[0] != 0 {
                t.get_or(x[0], x[1] as u32);
            }
        }
        *self = t;
    }
}

/// Start loading the cache line at p (PRFM: a hint that reads nothing the program sees and
/// never faults, so p may point anywhere).
#[inline(always)]
fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: PRFM only hints the cache; it cannot fault and changes no memory or flags.
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = p;
}

/// Byte classes of a text, 64 bytes per entry (bit i of entry k: byte 64k + i): spaces, marks
/// (0 bytes), ASCII letters, bytes 0x80 and up (index CL_SP, CL_MK, CL_AL, CL_HI). One NEON
/// pass over the text; the bytes past its end are in none of the classes.
const CL_SP: usize = 0;
const CL_MK: usize = 1;
const CL_AL: usize = 2;
const CL_HI: usize = 3;
fn cls64(t: &[u8], out: &mut Vec<[u64; 4]>) {
    out.clear();
    out.resize(t.len().div_ceil(64), [0; 4]);
    let mut p = 0;
    for x in out.iter_mut() {
        if p + 64 <= t.len() {
            // SAFETY: the 64 bytes at p are inside t
            *x = unsafe { cls_at(t.as_ptr().add(p)) };
        } else {
            let mut b = [b'!'; 64];
            b[..t.len() - p].copy_from_slice(&t[p..]);
            // SAFETY: b has 64 bytes
            *x = unsafe { cls_at(b.as_ptr()) };
        }
        p += 64;
    }
}

/// cls64's four masks of the 64 bytes at p (all must be readable).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn cls_at(p: *const u8) -> [u64; 4] {
    use std::arch::aarch64::*;
    unsafe {
        let v = [vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(32)), vld1q_u8(p.add(48))];
        let bm = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        #[inline(always)]
        unsafe fn mm(x: [uint8x16_t; 4], bm: uint8x16_t) -> u64 {
            unsafe {
                let s = vpaddq_u8(vpaddq_u8(vandq_u8(x[0], bm), vandq_u8(x[1], bm)), vpaddq_u8(vandq_u8(x[2], bm), vandq_u8(x[3], bm)));
                vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
            }
        }
        let (cs, c20, ca, c26) = (vdupq_n_u8(b' '), vdupq_n_u8(0x20), vdupq_n_u8(b'a'), vdupq_n_u8(26));
        let sp = mm(v.map(|x| vceqq_u8(x, cs)), bm);
        let mk = mm(v.map(|x| vceqzq_u8(x)), bm);
        // ASCII letters: the byte with bit 5 set, minus 'a', below 26
        let al = mm(v.map(|x| vcltq_u8(vsubq_u8(vorrq_u8(x, c20), ca), c26)), bm);
        let hi = mm(v.map(|x| vcltzq_s8(vreinterpretq_s8_u8(x))), bm);
        [sp, mk, al, hi]
    }
}
#[cfg(not(target_arch = "aarch64"))]
unsafe fn cls_at(p: *const u8) -> [u64; 4] {
    let b = unsafe { std::slice::from_raw_parts(p, 64) };
    let mut m = [0u64; 4];
    for (i, &c) in b.iter().enumerate() {
        m[CL_SP] |= ((c == b' ') as u64) << i;
        m[CL_MK] |= ((c == MARK) as u64) << i;
        m[CL_AL] |= (c.is_ascii_alphabetic() as u64) << i;
        m[CL_HI] |= ((c >= 0x80) as u64) << i;
    }
    m
}

/// The first place in [a, e) whose bit in f(class masks) is set, else e.
#[inline(always)]
fn cl_next(cl: &[[u64; 4]], a: usize, e: usize, f: impl Fn(&[u64; 4]) -> u64) -> usize {
    if a >= e {
        return e;
    }
    let mut w = a >> 6;
    let mut x = f(&cl[w]) & (!0u64 << (a & 63));
    loop {
        if x != 0 {
            return (w * 64 + x.trailing_zeros() as usize).min(e);
        }
        w += 1;
        if w * 64 >= e {
            return e;
        }
        x = f(&cl[w]);
    }
}

/// The bits of class k for the l bytes at a (l < 64; bit p: byte a + p).
#[inline(always)]
fn cl_bits(cl: &[[u64; 4]], a: usize, l: usize, k: usize) -> u64 {
    let (w, o) = (a >> 6, a & 63);
    let mut x = cl[w][k] >> o;
    if o + l > 64 {
        x |= cl[w + 1][k] << (64 - o);
    }
    x & ((1u64 << l) - 1)
}

/// The bytes of class k in [a, b): how many, and the place of the last one (when any).
#[inline(always)]
fn cl_count_last(cl: &[[u64; 4]], a: usize, b: usize, k: usize) -> (u32, usize) {
    let (mut n, mut last) = (0u32, 0usize);
    if a >= b {
        return (0, 0);
    }
    let mut w = a >> 6;
    let mut x = cl[w][k] & (!0u64 << (a & 63));
    loop {
        let end = (w + 1) * 64;
        if end >= b {
            x &= !0u64 >> (end - b);
        }
        if x != 0 {
            n += x.count_ones();
            last = w * 64 + 63 - x.leading_zeros() as usize;
        }
        if end >= b {
            return (n, last);
        }
        w += 1;
        x = cl[w][k];
    }
}

/// The table before M6 (keys and values in two arrays): only FL_SHADOW's old decisions use it.
struct TabOld {
    keys: Vec<u64>,
    vals: Vec<u32>,
    used: usize,
}

impl TabOld {
    fn new(n: usize) -> TabOld {
        let cap = (n * 2).next_power_of_two().max(64);
        TabOld { keys: vec![0; cap], vals: vec![0; cap], used: 0 }
    }
    /// The number stored for hash `k`, if any.
    fn get(&self, k: u64) -> Option<u32> {
        let k = k.max(1);
        let mask = self.keys.len() - 1;
        let mut i = (k >> 17) as usize & mask;
        loop {
            if self.keys[i] == k {
                return Some(self.vals[i]);
            }
            if self.keys[i] == 0 {
                return None;
            }
            i = (i + 1) & mask;
        }
    }
    /// The number stored for hash `k`, or `new` (stored now) when `k` is not there yet.
    fn get_or(&mut self, k: u64, new: u32) -> u32 {
        if self.used * 2 >= self.keys.len() {
            let mut t = TabOld::new(self.keys.len());
            for (i, &k) in self.keys.iter().enumerate() {
                if k != 0 {
                    t.get_or(k, self.vals[i]);
                }
            }
            *self = t;
        }
        let k = k.max(1); // 0 marks an empty slot
        let mask = self.keys.len() - 1;
        let mut i = (k >> 17) as usize & mask;
        loop {
            if self.keys[i] == k {
                return self.vals[i];
            }
            if self.keys[i] == 0 {
                self.keys[i] = k;
                self.vals[i] = new;
                self.used += 1;
                return new;
            }
            i = (i + 1) & mask;
        }
    }
}

/// heads::slot_cols' map for packing: (column of the slot before, hash of the piece) -> column,
/// an open-addressing table with the whole key (the same numbers: each new key gets the count
/// so far, as with the map the opener uses).
struct PieceMap {
    e: Vec<(u64, u32, u32)>, // (piece hash, previous column + 1, column + 1); 0 in the last: empty
    used: usize,
}

impl PieceMap {
    /// A map with room for about n keys without growing.
    fn new(n: usize) -> PieceMap {
        PieceMap { e: vec![(0, 0, 0); (2 * n + 2).next_power_of_two().max(1024)], used: 0 }
    }
    fn len(&self) -> usize {
        self.used
    }
    /// The column of key (prev, h), numbered len() when new.
    #[inline]
    fn get_or_add(&mut self, prev: u32, h: u64) -> u32 {
        if self.used * 2 >= self.e.len() {
            let n2 = self.e.len() * 2;
            let old = std::mem::replace(&mut self.e, vec![(0, 0, 0); n2]);
            let mask = self.e.len() - 1;
            let shift = 64 - self.e.len().trailing_zeros();
            for x in old {
                if x.2 != 0 {
                    let mut i = Self::slot(x.1, x.0, shift) & mask;
                    while self.e[i].2 != 0 {
                        i = (i + 1) & mask;
                    }
                    self.e[i] = x;
                }
            }
        }
        let mask = self.e.len() - 1;
        let p1 = prev.wrapping_add(1);
        let mut i = Self::slot(p1, h, 64 - self.e.len().trailing_zeros()) & mask;
        loop {
            let x = self.e[i];
            if x.2 == 0 {
                self.used += 1;
                self.e[i] = (h, p1, self.used as u32);
                return self.used as u32 - 1;
            }
            if x.0 == h && x.1 == p1 {
                return x.2 - 1;
            }
            i = (i + 1) & mask;
        }
    }
    /// The home place of key (p1, h) in a table of 2^(64 - shift) entries: the top bits of
    /// the product, so that every place of the table can be a home (a fixed shift would leave
    /// the places above 2^24 without keys of their own: in a block with millions of slot keys
    /// the probes then run through one long cluster). Only speed depends on it: the keys are
    /// compared in full. (shift: 1..=54, as the table has 1024 entries or more.)
    #[inline]
    fn slot(p1: u32, h: u64, shift: u32) -> usize {
        ((h ^ (p1 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0x517c_c1b7_2722_0a95) >> shift) as usize
    }
}

/// heads::slot_cols with a PieceMap (packing): the same columns.
#[inline]
fn slot_cols_pack(t: &[u8], map: &mut PieceMap, out: &mut Vec<u32>) {
    let (mut prev, mut a) = (u32::MAX, 0);
    for (pos, &c) in t.iter().enumerate() {
        if c == MARK {
            let id = map.get_or_add(prev, hash_bytes(&t[a..pos]));
            out.push(id);
            prev = id;
            a = pos + 1;
        }
    }
}

/// The places of the marks (0 bytes) of t, in order (NEON: 16 bytes per step).
fn mark_places(t: &[u8], out: &mut Vec<u32>) {
    out.clear();
    let mut p = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        while p + 16 <= t.len() {
            // SAFETY: the 16 bytes at p are inside t. shrn by 4 gives 4 bits per byte; bit 3 of
            // each nibble is kept, so a set bit 4i + 3 means byte i is a mark.
            let mut x = unsafe {
                let z = vceqzq_u8(vld1q_u8(t.as_ptr().add(p)));
                vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(z), 4)), 0) & 0x8888_8888_8888_8888
            };
            while x != 0 {
                out.push((p + (x.trailing_zeros() >> 2) as usize) as u32);
                x &= x - 1;
            }
            p += 16;
        }
    }
    for (i, &c) in t[p..].iter().enumerate() {
        if c == MARK {
            out.push((p + i) as u32);
        }
    }
}

/// The places (from a) of the bytes of class k in [a, b), in order (cls64's masks).
fn cl_places(cl: &[[u64; 4]], a: usize, b: usize, k: usize, out: &mut Vec<u32>) {
    out.clear();
    if a >= b {
        return;
    }
    let mut w = a >> 6;
    let mut x = cl[w][k] & (!0u64 << (a & 63));
    loop {
        let end = (w + 1) * 64;
        if end >= b {
            x &= !0u64 >> (end - b);
        }
        while x != 0 {
            out.push((w * 64 + x.trailing_zeros() as usize - a) as u32);
            x &= x - 1;
        }
        if end >= b {
            return;
        }
        w += 1;
        x = cl[w][k];
    }
}

/// slot_cols_pack from the places of the marks of t (mark_places): the same columns.
#[inline]
fn slot_cols_at(t: &[u8], at: &[u32], map: &mut PieceMap, out: &mut Vec<u32>) {
    let (mut prev, mut a) = (u32::MAX, 0usize);
    for &pos in at {
        let pos = pos as usize;
        let id = map.get_or_add(prev, hash_bytes(&t[a..pos]));
        out.push(id);
        prev = id;
        a = pos + 1;
    }
}

#[inline]
fn mix(h: u64, x: u64) -> u64 {
    (h.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// Words that vary become values. Works on the block's distinct shapes only (cheap). A shape is
/// cut into runs of text (between spaces and value marks), each with the spaces and marks after
/// it. A run with a letter becomes a value when at least `kmin` different runs fill its place
/// while the rest of the shape stays the same ("one hole"), and those runs also appear in other
/// shapes (spread: a weekday or a user name does, a state name that goes with one message does
/// not). Repeated for a few rounds, so a shape with several varying words loses them one by
/// one. Returns, per shape (flat list with start offsets), the runs to turn into word values.
fn pick_words(rbuf: &[u8], roff: &[u32], cl: &[[u64; 4]], pk: &Pick) -> (Vec<u32>, Vec<Run>) {
    let m = roff.len() - 1;
    // runs of all shapes; `sh`: hash of the spaces and marks after the run
    struct Piece {
        sh: u64,
        cand: bool,
        run: Run,
    }
    const HMARK: u64 = 0x6a09_e667_f3bc_c908;
    const SEP0: u64 = 0x243f_6a88_85a3_08d3;
    // the hash of a run of spaces and marks (mix byte by byte from SEP0) for runs of up to 6
    // bytes, by length l and marks (bit p: byte p is a mark): septab[2^l - 1 + marks]
    let mut septab = [0u64; 127];
    for l in 0..=6usize {
        for bits in 0..1usize << l {
            let mut h = SEP0;
            for q in 0..l {
                h = mix(h, if bits >> q & 1 == 1 { MARK as u64 } else { b' ' as u64 });
            }
            septab[(1 << l) - 1 + bits] = h;
        }
    }
    // the run of spaces and marks [a, b): its hash and its marks
    let sep = |a: usize, b: usize| -> (u64, u32) {
        let l = b - a;
        if l <= 6 {
            let bits = cl_bits(cl, a, l, CL_MK);
            (septab[(1 << l) - 1 + bits as usize], bits.count_ones())
        } else {
            let (mut h, mut k) = (SEP0, 0u32);
            for &c in &rbuf[a..b] {
                h = mix(h, c as u64);
                k += (c == MARK) as u32;
            }
            (h, k)
        }
    };
    let is_sep = |c: &[u64; 4]| c[CL_SP] | c[CL_MK];
    let not_sep = |c: &[u64; 4]| !(c[CL_SP] | c[CL_MK]);
    let mut ps: Vec<Piece> = Vec::with_capacity(rbuf.len() / 8);
    let mut poff: Vec<u32> = Vec::with_capacity(m + 1);
    let mut lead: Vec<u64> = Vec::with_capacity(m); // the spaces and marks before the first run
    poff.push(0);
    for j in 0..m {
        let (s, e) = (roff[j] as usize, roff[j + 1] as usize - 1);
        let mut i = cl_next(cl, s, e, not_sep);
        let (lh, mut k0) = sep(s, i);
        lead.push(lh);
        while i < e {
            // a run: up to the next space or mark; a letter in it makes it a candidate. Fast
            // path: the run and the spaces and marks after it end inside i's 64-byte mask word
            // (or at the shape's end); else the general walk.
            let (c, o) = (&cl[i >> 6], i & 63);
            let lim = (64 - o).min(e - i); // the shape's bytes in this mask word, from i
            let sm = (c[CL_SP] | c[CL_MK]) >> o;
            let zl = (sm.trailing_zeros() as usize).min(lim); // >= 1: byte i is not a separator
            let nl = if zl < lim { ((!(sm >> zl)).trailing_zeros() as usize).min(lim - zl) } else { 0 };
            let (z, i2, letter) = if zl + nl < lim || lim == e - i {
                (i + zl, i + zl + nl, (c[CL_AL] | c[CL_HI]) >> o & u64::MAX >> (64 - zl) != 0)
            } else {
                let z = cl_next(cl, i, e, is_sep);
                (z, cl_next(cl, z, e, not_sep), cl_next(cl, i, z, |c| c[CL_AL] | c[CL_HI]) < z)
            };
            let run = Run { off: (i - s) as u32, len: (z - i) as u32, k0, h: hash_bytes(&rbuf[i..z]) };
            i = i2;
            let (sh, k) = sep(z, i);
            k0 += k;
            ps.push(Piece { sh, cand: letter, run });
        }
        poff.push(ps.len() as u32);
    }
    // spread of a word: in how many distinct shapes it is a candidate
    let cands = ps.iter().filter(|p| p.cand).count();
    let mut widx = Tab::new(cands / 4);
    let mut wof: Vec<u32> = vec![0; ps.len()];
    let mut spread: Vec<u32> = Vec::new();
    for t in 0..ps.len() {
        if let Some(q) = ps.get(t + 8) {
            widx.prefetch(q.run.h);
        }
        let p = &ps[t];
        if p.cand {
            let w = widx.get_or(p.run.h, spread.len() as u32);
            if w as usize == spread.len() {
                spread.push(0);
            }
            spread[w as usize] += 1;
            wof[t] = w;
        }
    }
    // the rounds read compact copies: each run's hash (HMARK once it is a value), the hash of the
    // spaces and marks after it, whether it is a candidate not yet a value (open), and per shape
    // how many of its runs are open
    let mut rh: Vec<u64> = ps.iter().map(|p| p.run.h).collect();
    let shv: Vec<u64> = ps.iter().map(|p| p.sh).collect();
    let mut open: Vec<bool> = ps.iter().map(|p| p.cand).collect();
    let mut nopen: Vec<u32> = (0..m).map(|j| open[poff[j] as usize..poff[j + 1] as usize].iter().filter(|&&o| o).count() as u32).collect();
    // per hole (the shape with one run left out): how many distinct shapes fill it (they differ
    // in that run, so this counts different runs) and the sum of their runs' spread
    let mut hn: Vec<u32> = Vec::new();
    let mut hsp: Vec<u64> = Vec::new();
    let mut hidx = Tab::new(cands);
    // the hole of each candidate run not yet a value (u32::MAX: none, or no longer a candidate)
    let mut at: Vec<u32> = vec![u32::MAX; ps.len()];
    let mut conv = vec![false; ps.len()];
    let mut dirty = vec![true; m]; // shapes whose holes must be (re)computed
    let mut forms = Tab::new(m); // shapes as merged so far, counted once
    let mut pre: Vec<u64> = Vec::new();
    let mut keys: Vec<(u64, u32)> = Vec::new();
    for _round in 0..pk.rounds {
        for j in 0..m {
            if !dirty[j] {
                continue;
            }
            dirty[j] = false;
            let (a, b) = (poff[j] as usize, poff[j + 1] as usize);
            // a hole needs context: at least ctx other fixed words in the shape
            if (nopen[j] as usize) < pk.ctx + 1 {
                at[a..b].fill(u32::MAX);
                continue;
            }
            // hash of the runs before each position, then walk back with the hash after it
            pre.clear();
            let mut h = lead[j] ^ (b - a) as u64;
            for t in a..b {
                pre.push(h);
                h = mix(mix(h, rh[t]), shv[t]);
            }
            // two shapes can become the same after merging: count that form once
            let before = forms.used;
            forms.get_or(h, 0);
            let first = forms.used > before;
            // the holes' keys first (prefetched), then the lookups in the same order
            keys.clear();
            let mut sfx = 0x1319_8a2e_0370_7344u64;
            for t in (a..b).rev() {
                if open[t] {
                    let key = mix(mix(pre[t - a] ^ sfx.rotate_left(29), shv[t]), (t - a) as u64);
                    hidx.prefetch(key);
                    keys.push((key, t as u32));
                }
                sfx = mix(mix(sfx, rh[t]), shv[t]);
            }
            for &(key, t) in &keys {
                let hi = hidx.get_or(key, hn.len() as u32);
                if hi as usize == hn.len() {
                    hn.push(0);
                    hsp.push(0);
                }
                if first {
                    hn[hi as usize] += 1;
                    hsp[hi as usize] += spread[wof[t as usize] as usize] as u64;
                }
                at[t as usize] = hi;
            }
        }
        // turn a run into a value when its hole has kmin different runs that vary on their own
        // (their spread); `at` is set only for candidates that are not values yet
        let ok = |hi: usize| hn[hi] >= pk.kmin && hsp[hi] as f64 >= pk.smin * hn[hi] as f64;
        let mut changed = false;
        for j in 0..m {
            for t in poff[j] as usize..poff[j + 1] as usize {
                let x = at[t];
                if x != u32::MAX && ok(x as usize) {
                    conv[t] = true;
                    open[t] = false;
                    rh[t] = HMARK;
                    nopen[j] -= 1;
                    at[t] = u32::MAX;
                    dirty[j] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut pidx = Vec::with_capacity(m + 1);
    let mut plan = Vec::new();
    pidx.push(0u32);
    for j in 0..m {
        for t in poff[j] as usize..poff[j + 1] as usize {
            if conv[t] {
                plan.push(ps[t].run);
            }
        }
        pidx.push(plan.len() as u32);
    }
    (pidx, plan)
}

/// pick_words before M6 (a byte loop per run, TabOld): only FL_SHADOW's old decisions use it.
#[allow(dead_code)]
fn pick_words_old(rbuf: &[u8], roff: &[u32], pk: &Pick) -> (Vec<u32>, Vec<Run>) {
    let m = roff.len() - 1;
    // runs of all shapes; `sh`: hash of the spaces and marks after the run
    struct Piece {
        sh: u64,
        cand: bool,
        run: Run,
    }
    const HMARK: u64 = 0x6a09_e667_f3bc_c908;
    let mut ps: Vec<Piece> = Vec::with_capacity(rbuf.len() / 8);
    let mut poff: Vec<u32> = Vec::with_capacity(m + 1);
    let mut lead: Vec<u64> = Vec::with_capacity(m); // the spaces and marks before the first run
    poff.push(0);
    for j in 0..m {
        let (s, e) = (roff[j] as usize, roff[j + 1] as usize - 1);
        let (mut i, mut k0) = (s, 0u32);
        let sep = |i: &mut usize, k0: &mut u32| {
            let mut h = 0x243f_6a88_85a3_08d3u64;
            while *i < e && (rbuf[*i] == MARK || rbuf[*i] == b' ') {
                h = mix(h, rbuf[*i] as u64);
                *k0 += (rbuf[*i] == MARK) as u32;
                *i += 1;
            }
            h
        };
        lead.push(sep(&mut i, &mut k0));
        while i < e {
            let (mut z, mut letter) = (i, false);
            while z < e && rbuf[z] != MARK && rbuf[z] != b' ' {
                letter |= CLS[rbuf[z] as usize] >= 3 || rbuf[z] >= 0x80;
                z += 1;
            }
            let run = Run { off: (i - s) as u32, len: (z - i) as u32, k0, h: hash_bytes(&rbuf[i..z]) };
            i = z;
            let sh = sep(&mut i, &mut k0);
            ps.push(Piece { sh, cand: letter, run });
        }
        poff.push(ps.len() as u32);
    }
    // spread of a word: in how many distinct shapes it is a candidate
    let cands = ps.iter().filter(|p| p.cand).count();
    let mut widx = TabOld::new(cands / 4);
    let mut wof: Vec<u32> = vec![0; ps.len()];
    let mut spread: Vec<u32> = Vec::new();
    for (t, p) in ps.iter().enumerate() {
        if p.cand {
            let w = widx.get_or(p.run.h, spread.len() as u32);
            if w as usize == spread.len() {
                spread.push(0);
            }
            spread[w as usize] += 1;
            wof[t] = w;
        }
    }
    // per hole (the shape with one run left out): how many distinct shapes fill it (they differ
    // in that run, so this counts different runs) and the sum of their runs' spread
    let mut hn: Vec<u32> = Vec::new();
    let mut hsp: Vec<u64> = Vec::new();
    let mut hidx = TabOld::new(cands);
    let mut at: Vec<u32> = vec![u32::MAX; ps.len()]; // the hole of each candidate run
    let mut conv = vec![false; ps.len()];
    let mut dirty = vec![true; m]; // shapes whose holes must be (re)computed
    let mut forms = TabOld::new(m); // shapes as merged so far, counted once
    let mut pre: Vec<u64> = Vec::new();
    for _round in 0..pk.rounds {
        for j in 0..m {
            if !dirty[j] {
                continue;
            }
            dirty[j] = false;
            let (a, b) = (poff[j] as usize, poff[j + 1] as usize);
            // a hole needs context: at least ctx other fixed words in the shape
            if (a..b).filter(|&t| ps[t].cand && !conv[t]).count() < pk.ctx + 1 {
                at[a..b].fill(u32::MAX);
                continue;
            }
            // hash of the runs before each position, then walk back with the hash after it
            pre.clear();
            let mut h = lead[j] ^ (b - a) as u64;
            for t in a..b {
                pre.push(h);
                h = mix(mix(h, if conv[t] { HMARK } else { ps[t].run.h }), ps[t].sh);
            }
            // two shapes can become the same after merging: count that form once
            let before = forms.used;
            forms.get_or(h, 0);
            let first = forms.used > before;
            let mut sfx = 0x1319_8a2e_0370_7344u64;
            for t in (a..b).rev() {
                if ps[t].cand && !conv[t] {
                    let key = mix(mix(pre[t - a] ^ sfx.rotate_left(29), ps[t].sh), (t - a) as u64);
                    let hi = hidx.get_or(key, hn.len() as u32);
                    if hi as usize == hn.len() {
                        hn.push(0);
                        hsp.push(0);
                    }
                    if first {
                        hn[hi as usize] += 1;
                        hsp[hi as usize] += spread[wof[t] as usize] as u64;
                    }
                    at[t] = hi;
                }
                sfx = mix(mix(sfx, if conv[t] { HMARK } else { ps[t].run.h }), ps[t].sh);
            }
        }
        // turn a run into a value when its hole has kmin different runs that vary on their own
        // (their spread)
        let ok = |hi: usize| hn[hi] >= pk.kmin && hsp[hi] as f64 >= pk.smin * hn[hi] as f64;
        let mut changed = false;
        for j in 0..m {
            for t in poff[j] as usize..poff[j + 1] as usize {
                if ps[t].cand && !conv[t] && at[t] != u32::MAX && ok(at[t] as usize) {
                    conv[t] = true;
                    dirty[j] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut pidx = Vec::with_capacity(m + 1);
    let mut plan = Vec::new();
    pidx.push(0u32);
    for j in 0..m {
        for t in poff[j] as usize..poff[j + 1] as usize {
            if conv[t] {
                plan.push(ps[t].run);
            }
        }
        pidx.push(plan.len() as u32);
    }
    (pidx, plan)
}

/// The word values of a block pay when they merge away at least one shape per WORDS_PAY
/// values written (else the block keeps its words as pattern text).
const WORDS_PAY: u64 = 50;

/// Whether the block's word values pay (see WORDS_PAY); cnt: the lines of each shape. The merged
/// forms are counted once each (Shapes: distinct texts).
fn words_pay(rbuf: &[u8], roff: &[u32], pidx: &[u32], plan: &[Run], cnt: &[u32]) -> bool {
    let mut merged = Shapes::default();
    let (mut shapes, mut uses) = (0u64, 0u64);
    let mut g = Vec::new();
    for r in 0..roff.len() - 1 {
        let pl = &plan[pidx[r] as usize..pidx[r + 1] as usize];
        if pl.is_empty() {
            continue;
        }
        shapes += 1;
        uses += pl.len() as u64 * cnt[r] as u64;
        merge_shape(&rbuf[roff[r] as usize..roff[r + 1] as usize - 1], pl, &mut g);
        merged.find_or_add(&g);
    }
    (shapes - merged.at.len() as u64) * WORDS_PAY >= uses
}

/// words_pay before M6 (an FxMap of the merged texts): only FL_SHADOW's old decisions use it.
#[allow(dead_code)]
fn words_pay_old(rbuf: &[u8], roff: &[u32], pidx: &[u32], plan: &[Run], cnt: &[u32]) -> bool {
    let mut merged: FxMap<&[u8], ()> = FxMap::default();
    let mut text: Vec<u8> = Vec::new();
    let mut at: Vec<(usize, usize)> = Vec::new();
    let (mut shapes, mut uses) = (0u64, 0u64);
    let mut g = Vec::new();
    for r in 0..roff.len() - 1 {
        let pl = &plan[pidx[r] as usize..pidx[r + 1] as usize];
        if pl.is_empty() {
            continue;
        }
        shapes += 1;
        uses += pl.len() as u64 * cnt[r] as u64;
        merge_shape(&rbuf[roff[r] as usize..roff[r + 1] as usize - 1], pl, &mut g);
        at.push((text.len(), g.len()));
        text.extend_from_slice(&g);
    }
    for &(a, n) in &at {
        merged.entry(&text[a..a + n]).or_insert(());
    }
    (shapes - merged.len() as u64) * WORDS_PAY >= uses
}

/// The shape with the planned runs replaced by one mark each.
fn merge_shape(s: &[u8], plan: &[Run], out: &mut Vec<u8>) {
    out.clear();
    let mut a = 0;
    for r in plan {
        out.extend_from_slice(&s[a..r.off as usize]);
        out.push(MARK);
        a = (r.off + r.len) as usize;
    }
    out.extend_from_slice(&s[a..]);
}


/// The column of a slot after a word of a splitting column: one per (slot column, word),
/// numbered on first use like packing does.
#[cold]
#[inline(never)]
fn sub_col(sub: &mut FxMap<(u32, u64), u32>, ncols: &mut u32, c: u32, ctx: u64) -> u32 {
    *sub.entry((c, ctx)).or_insert_with(|| {
        *ncols += 1;
        *ncols - 1
    })
}


/// A clock time value (kinds 40-67) in microseconds, so times with different fractions compare.
#[inline]
fn micros(kind: u8, v: u64) -> i64 {
    let fd = ((kind - 40) >> 2) as u32;
    (v as i64).wrapping_mul(10i64.pow(6 - fd.min(6)))
}

/// Pack a block. With `want_times` (a grouped block), also return the clock column (the
/// column with the most clock times and changes, among columns in at least half the lines)
/// and each line's first time in it (group::NO_TIME when it has none).
/// A fast hash of a byte string (16 bytes per step; the ends read as overlapping words).
#[inline]
fn hash16(b: &[u8]) -> u64 {
    const K1: u64 = 0x9E37_79B9_7F4A_7C15;
    const K2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    let n = b.len();
    let rd = |p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
    let (mut h1, mut h2) = (n as u64 ^ K2, K1);
    if n >= 16 {
        let mut p = 0;
        while p + 16 < n {
            h1 = (h1 ^ rd(p)).wrapping_mul(K1).rotate_left(29);
            h2 = (h2 ^ rd(p + 8)).wrapping_mul(K2).rotate_left(31);
            p += 16;
        }
        h1 = (h1 ^ rd(n - 16)).wrapping_mul(K1).rotate_left(29);
        h2 = (h2 ^ rd(n - 8)).wrapping_mul(K2).rotate_left(31);
    } else if n >= 8 {
        h1 = (h1 ^ rd(0)).wrapping_mul(K1).rotate_left(29);
        h2 = (h2 ^ rd(n - 8)).wrapping_mul(K2).rotate_left(31);
    } else if n >= 4 {
        let r4 = |p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as u64;
        h1 = (h1 ^ (r4(0) << 32 | r4(n - 4))).wrapping_mul(K1).rotate_left(29);
    } else if n > 0 {
        h1 = (h1 ^ (b[0] as u64) << 16 ^ (b[n / 2] as u64) << 8 ^ b[n - 1] as u64).wrapping_mul(K1).rotate_left(29);
    }
    let h = (h1 ^ h2.rotate_left(17)).wrapping_mul(K2);
    h ^ (h >> 29)
}

/// Distinct shapes, each followed by '\n' in `text`, found by hash (no allocation per line).
#[derive(Default)]
struct Shapes {
    text: Vec<u8>,
    at: Vec<(u32, u32)>, // start and length in `text`
    hash: Vec<u64>,
    slots: Vec<u32>, // open addressing: shape number + 1 (0: empty)
}

impl Shapes {
    /// The shape's number, and whether it is new.
    #[inline]
    fn find_or_add(&mut self, shape: &[u8]) -> (u32, bool) {
        let h = hash16(shape);
        if self.slots.len() < 2 * self.at.len() + 2 {
            let size = (self.slots.len() * 2).max(1024);
            self.slots = vec![0; size];
            for (t, &h) in self.hash.iter().enumerate() {
                let mut s = h as usize & (size - 1);
                while self.slots[s] != 0 {
                    s = (s + 1) & (size - 1);
                }
                self.slots[s] = t as u32 + 1;
            }
        }
        let mask = self.slots.len() - 1;
        let mut s = h as usize & mask;
        loop {
            let id = self.slots[s];
            if id == 0 {
                break;
            }
            let t = (id - 1) as usize;
            if self.hash[t] == h {
                let (a, n) = self.at[t];
                if &self.text[a as usize..(a + n) as usize] == shape {
                    return (t as u32, false);
                }
            }
            s = (s + 1) & mask;
        }
        let t = self.at.len();
        self.slots[s] = t as u32 + 1;
        self.at.push((self.text.len() as u32, shape.len() as u32));
        self.hash.push(h);
        self.text.extend_from_slice(shape);
        self.text.push(b'\n');
        (t as u32, true)
    }
}

/// Pass 1 of a block (or of a part of it): the distinct shapes in order of first use, the
/// number of values of each, each line's shape and all values in line order.
struct Pass1 {
    shapes: Shapes,
    rnv: Vec<u32>,
    rid: Vec<u32>,
    vals: Vec<Val>,
}

/// LineRec (M2, then M3 and M6; frozen interface, DESIGN.md 7b): a line's shape id and where its
/// values start in the block's value rows. The stand-in derives it from today's Pass1 (rid, and
/// rnv per shape); M2's pass 1 writes it directly.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct LineRec {
    rid: u32,
    row: u32,
}
/// The stand-in: each line's LineRec from today's Pass1.
fn line_recs(p: &Pass1) -> Vec<LineRec> {
    let mut at = 0u32;
    p.rid
        .iter()
        .map(|&r| {
            let l = LineRec { rid: r, row: at };
            at += p.rnv[r as usize];
            l
        })
        .collect()
}

thread_local! {
    /// The values buffer of this thread's last pass 1 (kept for its next block: its pages are
    /// already mapped, so filling it again costs no page faults)
    static VALS_POOL: std::cell::RefCell<Vec<Val>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// An empty values buffer with room for `cap` values: this thread's kept one when it is big
/// enough, else a new one.
fn vals_buffer(cap: usize) -> Vec<Val> {
    let mut v = VALS_POOL.try_with(|c| c.try_borrow_mut().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default()).unwrap_or_default();
    v.clear();
    if v.capacity() < cap {
        v = Vec::with_capacity(cap);
    }
    v
}

impl Drop for Pass1 {
    /// The values buffer goes back to this thread's pool (the bigger of the two is kept).
    fn drop(&mut self) {
        let v = std::mem::take(&mut self.vals);
        let _ = VALS_POOL.try_with(|c| {
            if let Ok(mut b) = c.try_borrow_mut() {
                if v.capacity() > b.capacity() {
                    *b = v;
                }
            }
        });
    }
}

/// Pass 1 on this core (FL_OLDSPLIT: the old line splitter; FL_CHECK: both, compared).
fn pass1_seq(data: &[u8], lines: &[&[u8]], cfg: &SplitCfg) -> Pass1 {
    if !env_on("FL_OLDSPLIT") && !env_on("FL_SPLIT1") {
        // the fast front end (front.rs): the same shapes and values as split_line (lines for
        // the space-padded or short hex numbers go to split.rs's splitter)
        return front::pass1_fast(data, lines, cfg);
    }
    let mut p = Pass1 { shapes: Shapes::default(), rnv: Vec::new(), rid: Vec::with_capacity(lines.len()), vals: Vec::with_capacity(lines.len() * 8) };
    let mut sp = split::Splitter::default();
    let (old, check) = (env_on("FL_OLDSPLIT"), env_on("FL_CHECK"));
    let mut shape: Vec<u8> = Vec::new();
    let mut cvals: Vec<Val> = Vec::new();
    for &l in lines {
        let (sh, nv): (&[u8], usize) = if old {
            shape.clear();
            let v0 = p.vals.len();
            split_line(l, cfg, &mut shape, &mut p.vals);
            (&shape, p.vals.len() - v0)
        } else {
            sp.split(data, l.as_ptr() as usize - data.as_ptr() as usize, l.len(), cfg);
            p.vals.extend_from_slice(&sp.vals[..sp.nvals]);
            if check {
                shape.clear();
                cvals.clear();
                split_line(l, cfg, &mut shape, &mut cvals);
                if shape[..] != sp.shape[..sp.shape_len] || cvals[..] != sp.vals[..sp.nvals] {
                    eprintln!("SPLIT DIFFERS: line {:?}\n old {:?} {:?}\n new {:?} {:?}", String::from_utf8_lossy(l), String::from_utf8_lossy(&shape), cvals, String::from_utf8_lossy(&sp.shape[..sp.shape_len]), &sp.vals[..sp.nvals]);
                    std::process::exit(3);
                }
            }
            (&sp.shape[..sp.shape_len], sp.nvals)
        };
        let (r, new) = p.shapes.find_or_add(sh);
        if new {
            p.rnv.push(nv as u32);
        }
        p.rid.push(r);
    }
    p
}

/// Pass 1 on `threads` cores, with the same result as pass1_seq: the lines are split in
/// parts at the same time; then, in line order, each part's shapes get their block numbers
/// (new ones in order of first use, as one pass would give them).
fn pass1(data: &[u8], lines: &[&[u8]], cfg: &SplitCfg, threads: usize) -> Pass1 {
    if threads <= 1 || lines.len() < 4096 {
        return pass1_seq(data, lines, cfg);
    }
    let size = lines.len().div_ceil(threads);
    let parts: Vec<Pass1> = std::thread::scope(|sc| {
        let hs: Vec<_> = lines.chunks(size).map(|c| sc.spawn(move || pass1_seq(data, c, cfg))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    // the parts' shapes numbered in line order (one core: a part has few shapes)
    let (nl, nv): (usize, usize) = (parts.iter().map(|q| q.rid.len()).sum(), parts.iter().map(|q| q.vals.len()).sum());
    let mut p = Pass1 { shapes: Shapes::default(), rnv: Vec::new(), rid: vec![0; nl], vals: vals_buffer(nv) };
    let mut maps: Vec<Vec<u32>> = Vec::with_capacity(parts.len());
    for q in &parts {
        let mut map = Vec::with_capacity(q.shapes.at.len());
        for (k, &(a, n)) in q.shapes.at.iter().enumerate() {
            let (r, new) = p.shapes.find_or_add(&q.shapes.text[a as usize..(a + n) as usize]);
            if new {
                p.rnv.push(q.rnv[k]);
            }
            map.push(r);
        }
        maps.push(map);
    }
    // then each part's shape numbers and values put in their places, all parts side by side
    // (the same rows as one after the other: the parts tile rid and vals in order)
    {
        let (mut rid, mut vals) = (&mut p.rid[..], &mut p.vals.spare_capacity_mut()[..nv]);
        std::thread::scope(|sc| {
            for (q, map) in parts.iter().zip(&maps) {
                let (r, rest) = std::mem::take(&mut rid).split_at_mut(q.rid.len());
                rid = rest;
                let (v, rest) = std::mem::take(&mut vals).split_at_mut(q.vals.len());
                vals = rest;
                sc.spawn(move || {
                    for (d, &x) in r.iter_mut().zip(&q.rid) {
                        *d = map[x as usize];
                    }
                    for (d, &x) in v.iter_mut().zip(&q.vals) {
                        d.write(x);
                    }
                });
            }
        });
    }
    // SAFETY: vals_buffer gave a capacity of at least nv; the parts' slices above tile
    // spare_capacity_mut()[..nv] exactly (their lengths sum to nv) and each thread wrote every
    // element of its slice (zip of two slices of the same length) before the scope ended
    unsafe { p.vals.set_len(nv) };
    p
}

/// Code a block's streams, on up to `threads` cores (the biggest streams first); with each
/// stream coded by fz (levels 40-49), its parsed input (for the unit's table bank: pack_coded).
fn code_streams(p: &Packed, level: u32, threads: usize) -> Vec<(Vec<u8>, Option<fz::Prep>)> {
    // a stream (or a form of stream 1) as ozip::compress_with codes it at these levels: fz's
    // frame (with its parsed input, for the bank), or stored when not smaller
    let fzc = |s: &[u8]| -> (Vec<u8>, Option<fz::Prep>) {
        if (40..50).contains(&level) && s.len() >= 16 {
            let mut pr = fz::prep_fast(s, level - 40);
            let f = fz::code(&mut pr);
            if f.len() >= 1 + s.len() + varint_len(s.len() as u64) { (stored(s), None) } else { (f, Some(pr)) }
        } else {
            (ozip::compress_with(s, level, None), None)
        }
    };
    let code = |k: usize| -> (Vec<u8>, Option<fz::Prep>) {
        let s = &p.streams[k];
        let t = Tick::now();
        PBYTES[k].fetch_add(s.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let pre = if level == 43 { p.pre.iter().find(|x| x.0 == k && x.1 == hash_bytes(s)) } else { None };
        let r = if k == 0 || k == 8 {
            (Vec::new(), None) // the block's tables: coded once per file (file_table)
        } else if let Some(x) = pre {
            (x.2.clone(), x.3.clone()) // coded by pack_block already (the same bytes)
        } else if p.raw >> k & 1 == 1 {
            (stored(s), None)
        } else if k == 2 {
            // the column header: small and repetitive, like the tables (table_level)
            (ozip::compress_with(s, table_level(level, s.len()), None), None)
        } else if k == 1 && !p.alt.is_empty() {
            // (the pattern ids: the form that codes smallest, the plain form on a tie)
            let mut r = fzc(s);
            for a in &p.alt {
                let c = fzc(a);
                if c.0.len() < r.0.len() {
                    r = c;
                }
            }
            r
        } else {
            fzc(s)
        };
        prof_add(12 + k, t);
        r
    };
    if let Some(d) = crate::engine::fl::no_env::var_os("FL_DUMP") {
        // developer: write each raw stream of each block to FL_DUMP + "_b<block>_s<k>.bin"
        static NB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let b = NB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for (k, s) in p.streams.iter().enumerate() {
            let mut f = d.clone();
            f.push(format!("_b{}_s{}.bin", b, k));
            std::fs::write(f, s).unwrap();
        }
    }
    if threads <= 1 {
        return (0..p.streams.len()).map(code).collect();
    }
    let mut order: Vec<usize> = (0..p.streams.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(p.streams[i].len()));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out: Vec<std::sync::Mutex<(Vec<u8>, Option<fz::Prep>)>> = p.streams.iter().map(|_| std::sync::Mutex::new((Vec::new(), None))).collect();
    std::thread::scope(|sc| {
        for _ in 0..threads.min(p.streams.len()) {
            sc.spawn(|| loop {
                let q = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if q >= order.len() {
                    break;
                }
                let i = order[q];
                *out[i].lock().unwrap() = code(i);
            });
        }
    });
    out.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

/// The rules of a block's patterns (rules.rs). One scan over the lines collects the first lines
/// of each pattern with at least rules::MIN_LINES lines (the most common patterns first, within
/// a budget): at rules::SCREEN lines it is screened; with candidates it goes on to
/// rules::SAMPLE lines, where they are tried. Lines are seen as pass 2 sees them: merged shapes
/// (tokens: a token is one value, not a number), planned words (not numbers). Returns each
/// pattern's first rule (one more entry at the end) and the rules.
#[allow(clippy::too_many_arguments)]
/// `count`: the lines of each pattern; `ccount`: the values in each column (a pattern's slot
/// "owns" its column when nearly all are its).
#[allow(clippy::too_many_arguments)]
fn block_rules(rid: &[u32], rnv: &[u32], vals: &[Val], tk: Option<&tokens::Tokens>, final_of: &[u32], pidx: &[u32], plan: &[Run], col_of: &[Vec<u32>], count: &[u32], ccount: &[u32]) -> (Vec<u32>, Vec<rules::Rule>) {
    let np = col_of.len();
    let smap = |r: u32| -> usize {
        match tk {
            Some(t) => t.smap[r as usize] as usize,
            None => r as usize,
        }
    };
    // the patterns looked at: the most common first, while their screening lines stay within
    // a budget (a share of the block's values)
    let mut order: Vec<u32> = (0..np as u32).filter(|&t| count[t as usize] >= rules::MIN_LINES && (2..=u16::MAX as usize).contains(&col_of[t as usize].len())).collect();
    if order.is_empty() {
        return (vec![0; np + 1], Vec::new());
    }
    order.sort_unstable_by_key(|&t| std::cmp::Reverse(count[t as usize]));
    let budget = vals.len() / 32 + (1 << 16);
    let mut n = 0usize;
    let mut take = 0usize;
    for &t in &order {
        let size = (count[t as usize] as usize).min(rules::SCREEN) * col_of[t as usize].len();
        if n + size > budget {
            break;
        }
        n += size;
        take += 1;
    }
    order.truncate(take);
    // per pattern: its place in `order` (or none)
    let mut at = vec![u32::MAX; np];
    for (a, &t) in order.iter().enumerate() {
        at[t as usize] = a as u32;
    }
    // each pattern's lines so far (its screening lines in one buffer, more in its own) and
    // where it is: 0 screening, 1 trying candidates, 2 done
    let mut buf = vec![(0u8, 0u64); n];
    let mut place: Vec<(usize, usize)> = Vec::with_capacity(order.len());
    let mut m = 0usize;
    for &t in &order {
        place.push((m, m));
        m += (count[t as usize] as usize).min(rules::SCREEN) * col_of[t as usize].len();
    }
    let mut more: Vec<Vec<(u8, u64)>> = vec![Vec::new(); order.len()];
    let mut state = vec![0u8; order.len()];
    let mut crange = vec![(0u32, 0u32); order.len()]; // each pattern's candidates in `found`
    let mut found = Vec::new();
    let mut w = rules::Scratch::default();
    let mut rules = Vec::new();
    let mut chosen: Vec<(u32, usize, usize)> = Vec::new(); // (pattern, its rules from, to)
    let mut own: Vec<bool> = Vec::new();
    // values still wanted; past half the block the scan stops at nearly none (a rare
    // pattern's last lines can come late)
    let mut left = n;
    let stop = n / 16;
    let half = rid.len() / 2;
    let mut vi = 0usize;
    // a pattern's sample is complete: screen it, or try its candidates
    let mut step = |a: usize, kv: &[(u8, u64)], state: &mut u8, left: &mut usize, w: &mut rules::Scratch, found: &mut Vec<rules::Rule>, rules: &mut Vec<rules::Rule>| {
        let t = order[a] as usize;
        let ns = col_of[t].len();
        if *state == 0 {
            own.clear();
            own.extend(col_of[t].iter().map(|&c| count[t] as u64 * 10 >= ccount[c as usize] as u64 * 9));
            let c0 = found.len() as u32;
            rules::screen(kv, ns, count[t], &own, found, w);
            crange[a] = (c0, found.len() as u32);
            let full = kv.len() / ns >= (count[t] as usize).min(rules::SAMPLE);
            if found.len() as u32 == c0 {
                *state = 2;
                return;
            }
            if !full {
                // go on to SAMPLE lines
                *state = 1;
                *left += ((count[t] as usize).min(rules::SAMPLE) - kv.len() / ns) * ns;
                return;
            }
        }
        let r0 = rules.len();
        rules::choose(kv, ns, count[t], &found[crange[a].0 as usize..crange[a].1 as usize], rules, w);
        if rules.len() > r0 {
            chosen.push((t as u32, r0, rules.len()));
        }
        *state = 2;
    };
    let mut lvm: Vec<(u8, u64)> = Vec::new(); // a line's values with its tokens merged
    // per pass-1 shape: its pattern's place in `order` (or none)
    let sa: Vec<u32> = (0..rnv.len()).map(|r0| at[final_of[smap(r0 as u32)] as usize]).collect();
    // the scan stops after an eighth of the block (FL_RSCAN: lines = block / this): patterns
    // whose lines did not all come are looked at with the lines there are
    static RS: crate::engine::fl::NoEnvOnce<usize> = crate::engine::fl::NoEnvOnce::new();
    let cap = rid.len() / (*RS.get_or_init(|| crate::engine::fl::no_env::var("FL_RSCAN").ok().and_then(|x| x.parse().ok()).unwrap_or(8))).max(1);
    for (li, &r0) in rid.iter().enumerate() {
        if (left <= stop && (left == 0 || li >= half)) || li >= cap {
            break;
        }
        let nv = rnv[r0 as usize] as usize;
        let j0 = vi;
        vi += nv;
        let a = sa[r0 as usize];
        if a == u32::MAX || state[a as usize] == 2 {
            continue;
        }
        let r = smap(r0);
        let a = a as usize;
        let t = order[a] as usize;
        let ns = col_of[t].len();
        // the line's values as pass 2 sees them (a token is one value, not a number); a plain
        // line (no token, no planned word) gives slot k the line's value k, without a copy
        let lv0 = &vals[j0..j0 + nv];
        let (p0, p1) = (pidx[r] as usize, pidx[r + 1] as usize);
        let toks = matches!(tk, Some(tt) if tt.tidx[r0 as usize] < tt.tidx[r0 as usize + 1]);
        let plain = !toks && p0 == p1;
        lvm.clear();
        match tk {
            _ if plain => {}
            Some(tt) if toks => {
                let mut j = 0;
                for x in tt.tidx[r0 as usize] as usize..tt.tidx[r0 as usize + 1] as usize {
                    let tok = &tt.toks[x];
                    lvm.extend(lv0[j..tok.j0()].iter().map(|v| (v.0, v.1)));
                    lvm.push((TOKK, 0));
                    j = tok.j0() + tok.nv();
                }
                lvm.extend(lv0[j..].iter().map(|v| (v.0, v.1)));
            }
            _ => lvm.extend(lv0.iter().map(|v| (v.0, v.1))),
        }
        // the line's slot values (a word is not a number)
        let (mut q, mut j) = (p0, 0usize);
        let mut slot = |k: usize| {
            if plain {
                lv0.get(k).map_or((WORD, 0), |v| (v.0, v.1))
            } else if q < p1 && plan[q].k0 as usize + q - p0 == k {
                q += 1;
                (WORD, 0)
            } else {
                j += 1;
                lvm.get(j - 1).copied().unwrap_or((WORD, 0))
            }
        };
        left = left.saturating_sub(ns);
        if state[a] == 0 {
            let e = place[a].1;
            for k in 0..ns {
                buf[e + k] = slot(k);
            }
            place[a].1 += ns;
            if place[a].1 - place[a].0 == (count[t] as usize).min(rules::SCREEN) * ns {
                let mut st = 0;
                let (p, e) = place[a];
                step(a, &buf[p..e], &mut st, &mut left, &mut w, &mut found, &mut rules);
                if st == 1 {
                    more[a].extend_from_slice(&buf[p..e]);
                }
                state[a] = st;
            }
        } else {
            for k in 0..ns {
                let v = slot(k);
                more[a].push(v);
            }
            if more[a].len() == (count[t] as usize).min(rules::SAMPLE) * ns {
                let mut st = 1;
                let kv = std::mem::take(&mut more[a]);
                step(a, &kv, &mut st, &mut left, &mut w, &mut found, &mut rules);
                state[a] = st;
            }
        }
    }
    // the patterns whose lines did not all come: with the lines there are
    for a in 0..order.len() {
        let mut st = state[a];
        if st == 0 && place[a].1 > place[a].0 {
            let (p, e) = place[a];
            step(a, &buf[p..e], &mut st, &mut left, &mut w, &mut found, &mut rules);
            if st == 1 {
                more[a].extend_from_slice(&buf[p..e]);
            }
        }
        if st == 1 {
            let kv = std::mem::take(&mut more[a]);
            step(a, &kv, &mut st, &mut left, &mut w, &mut found, &mut rules);
        }
    }
    // in pattern order: a pattern is chosen at most once (its state is then 2 and it is never
    // stepped again), so the entry of pattern t is found by index (no sort needed)
    let mut of_t = vec![u32::MAX; np];
    for (i, e) in chosen.iter().enumerate() {
        of_t[e.0 as usize] = i as u32;
    }
    let mut sorted = Vec::with_capacity(rules.len());
    let mut rfirst = vec![0u32; np + 1];
    for t in 0..np {
        rfirst[t] = sorted.len() as u32;
        if let Some(e) = chosen.get(of_t[t] as usize) {
            sorted.extend_from_slice(&rules[e.1..e.2]);
        }
    }
    rfirst[np] = sorted.len() as u32;
    (rfirst, sorted)
}

/// block_rules before M6 (its own counts, the sort of the chosen rules): only FL_SHADOW's old
/// decisions use it.
#[allow(dead_code, clippy::too_many_arguments)]
fn block_rules_old(rid: &[u32], rnv: &[u32], vals: &[Val], tk: Option<&tokens::Tokens>, final_of: &[u32], pidx: &[u32], plan: &[Run], col_of: &[Vec<u32>], per_shape: &[u32]) -> (Vec<u32>, Vec<rules::Rule>) {
    let np = col_of.len();
    let smap = |r: u32| -> usize {
        match tk {
            Some(t) => t.smap[r as usize] as usize,
            None => r as usize,
        }
    };
    // the lines of each pattern (from the lines of each shape)
    let mut count = vec![0u32; np];
    for (r, &n) in per_shape.iter().enumerate() {
        count[final_of[r] as usize] += n;
    }
    // the patterns looked at: the most common first, while their screening lines stay within
    // a budget (a share of the block's values)
    let mut order: Vec<u32> = (0..np as u32).filter(|&t| count[t as usize] >= rules::MIN_LINES && (2..=u16::MAX as usize).contains(&col_of[t as usize].len())).collect();
    if order.is_empty() {
        return (vec![0; np + 1], Vec::new());
    }
    order.sort_unstable_by_key(|&t| std::cmp::Reverse(count[t as usize]));
    let budget = vals.len() / 32 + (1 << 16);
    let mut n = 0usize;
    let mut take = 0usize;
    for &t in &order {
        let size = (count[t as usize] as usize).min(rules::SCREEN) * col_of[t as usize].len();
        if n + size > budget {
            break;
        }
        n += size;
        take += 1;
    }
    order.truncate(take);
    // the values in each column (a pattern's slot "owns" its column when nearly all are its)
    let mut ccount = vec![0u32; col_of.iter().flatten().map(|&c| c as usize + 1).max().unwrap_or(0)];
    for (t, cs) in col_of.iter().enumerate() {
        for &c in cs {
            ccount[c as usize] += count[t];
        }
    }
    // per pattern: its place in `order` (or none)
    let mut at = vec![u32::MAX; np];
    for (a, &t) in order.iter().enumerate() {
        at[t as usize] = a as u32;
    }
    // each pattern's lines so far (its screening lines in one buffer, more in its own) and
    // where it is: 0 screening, 1 trying candidates, 2 done
    let mut buf = vec![(0u8, 0u64); n];
    let mut place: Vec<(usize, usize)> = Vec::with_capacity(order.len());
    let mut m = 0usize;
    for &t in &order {
        place.push((m, m));
        m += (count[t as usize] as usize).min(rules::SCREEN) * col_of[t as usize].len();
    }
    let mut more: Vec<Vec<(u8, u64)>> = vec![Vec::new(); order.len()];
    let mut state = vec![0u8; order.len()];
    let mut crange = vec![(0u32, 0u32); order.len()]; // each pattern's candidates in `found`
    let mut found = Vec::new();
    let mut w = rules::Scratch::default();
    let mut rules = Vec::new();
    let mut chosen: Vec<(u32, usize, usize)> = Vec::new(); // (pattern, its rules from, to)
    let mut own: Vec<bool> = Vec::new();
    // values still wanted; past half the block the scan stops at nearly none (a rare
    // pattern's last lines can come late)
    let mut left = n;
    let stop = n / 16;
    let half = rid.len() / 2;
    let mut vi = 0usize;
    // a pattern's sample is complete: screen it, or try its candidates
    let mut step = |a: usize, kv: &[(u8, u64)], state: &mut u8, left: &mut usize, w: &mut rules::Scratch, found: &mut Vec<rules::Rule>, rules: &mut Vec<rules::Rule>| {
        let t = order[a] as usize;
        let ns = col_of[t].len();
        if *state == 0 {
            own.clear();
            own.extend(col_of[t].iter().map(|&c| count[t] as u64 * 10 >= ccount[c as usize] as u64 * 9));
            let c0 = found.len() as u32;
            rules::screen(kv, ns, count[t], &own, found, w);
            crange[a] = (c0, found.len() as u32);
            let full = kv.len() / ns >= (count[t] as usize).min(rules::SAMPLE);
            if found.len() as u32 == c0 {
                *state = 2;
                return;
            }
            if !full {
                // go on to SAMPLE lines
                *state = 1;
                *left += ((count[t] as usize).min(rules::SAMPLE) - kv.len() / ns) * ns;
                return;
            }
        }
        let r0 = rules.len();
        rules::choose(kv, ns, count[t], &found[crange[a].0 as usize..crange[a].1 as usize], rules, w);
        if rules.len() > r0 {
            chosen.push((t as u32, r0, rules.len()));
        }
        *state = 2;
    };
    let mut lvm: Vec<(u8, u64)> = Vec::new(); // a line's values with its tokens merged
    // per pass-1 shape: its pattern's place in `order` (or none)
    let sa: Vec<u32> = (0..rnv.len()).map(|r0| at[final_of[smap(r0 as u32)] as usize]).collect();
    // the scan stops after an eighth of the block (FL_RSCAN: lines = block / this): patterns
    // whose lines did not all come are looked at with the lines there are
    static RS: crate::engine::fl::NoEnvOnce<usize> = crate::engine::fl::NoEnvOnce::new();
    let cap = rid.len() / (*RS.get_or_init(|| crate::engine::fl::no_env::var("FL_RSCAN").ok().and_then(|x| x.parse().ok()).unwrap_or(8))).max(1);
    for (li, &r0) in rid.iter().enumerate() {
        if (left <= stop && (left == 0 || li >= half)) || li >= cap {
            break;
        }
        let nv = rnv[r0 as usize] as usize;
        let j0 = vi;
        vi += nv;
        let a = sa[r0 as usize];
        if a == u32::MAX || state[a as usize] == 2 {
            continue;
        }
        let r = smap(r0);
        let a = a as usize;
        let t = order[a] as usize;
        let ns = col_of[t].len();
        // the line's values as pass 2 sees them (a token is one value, not a number)
        lvm.clear();
        let lv0 = &vals[j0..j0 + nv];
        match tk {
            Some(tt) if tt.tidx[r0 as usize] < tt.tidx[r0 as usize + 1] => {
                let mut j = 0;
                for x in tt.tidx[r0 as usize] as usize..tt.tidx[r0 as usize + 1] as usize {
                    let tok = &tt.toks[x];
                    lvm.extend(lv0[j..tok.j0()].iter().map(|v| (v.0, v.1)));
                    lvm.push((TOKK, 0));
                    j = tok.j0() + tok.nv();
                }
                lvm.extend(lv0[j..].iter().map(|v| (v.0, v.1)));
            }
            _ => lvm.extend(lv0.iter().map(|v| (v.0, v.1))),
        }
        // the line's slot values (a word is not a number)
        let (p0, p1) = (pidx[r] as usize, pidx[r + 1] as usize);
        let (mut q, mut j) = (p0, 0usize);
        let mut slot = |k: usize| {
            if q < p1 && plan[q].k0 as usize + q - p0 == k {
                q += 1;
                (WORD, 0)
            } else {
                j += 1;
                lvm.get(j - 1).copied().unwrap_or((WORD, 0))
            }
        };
        left = left.saturating_sub(ns);
        if state[a] == 0 {
            let e = place[a].1;
            for k in 0..ns {
                buf[e + k] = slot(k);
            }
            place[a].1 += ns;
            if place[a].1 - place[a].0 == (count[t] as usize).min(rules::SCREEN) * ns {
                let mut st = 0;
                let (p, e) = place[a];
                step(a, &buf[p..e], &mut st, &mut left, &mut w, &mut found, &mut rules);
                if st == 1 {
                    more[a].extend_from_slice(&buf[p..e]);
                }
                state[a] = st;
            }
        } else {
            for k in 0..ns {
                let v = slot(k);
                more[a].push(v);
            }
            if more[a].len() == (count[t] as usize).min(rules::SAMPLE) * ns {
                let mut st = 1;
                let kv = std::mem::take(&mut more[a]);
                step(a, &kv, &mut st, &mut left, &mut w, &mut found, &mut rules);
                state[a] = st;
            }
        }
    }
    // the patterns whose lines did not all come: with the lines there are
    for a in 0..order.len() {
        let mut st = state[a];
        if st == 0 && place[a].1 > place[a].0 {
            let (p, e) = place[a];
            step(a, &buf[p..e], &mut st, &mut left, &mut w, &mut found, &mut rules);
            if st == 1 {
                more[a].extend_from_slice(&buf[p..e]);
            }
        }
        if st == 1 {
            let kv = std::mem::take(&mut more[a]);
            step(a, &kv, &mut st, &mut left, &mut w, &mut found, &mut rules);
        }
    }
    // in pattern order
    chosen.sort_unstable_by_key(|e| e.0);
    let mut sorted = Vec::with_capacity(rules.len());
    let mut rfirst = vec![0u32; np + 1];
    let mut ci = 0;
    for t in 0..np {
        rfirst[t] = sorted.len() as u32;
        if ci < chosen.len() && chosen[ci].0 as usize == t {
            sorted.extend_from_slice(&rules[chosen[ci].1..chosen[ci].2]);
            ci += 1;
        }
    }
    rfirst[np] = sorted.len() as u32;
    (rfirst, sorted)
}

static PROF: [std::sync::atomic::AtomicU64; 64] = [const { std::sync::atomic::AtomicU64::new(0) }; 64];
/// FL_PROF: instructions and cycles of each slot (the thread that ran it; slots 60-62: the whole
/// process, pack_coded, open_unit and open_unit_par)
static PROFI: [std::sync::atomic::AtomicU64; 64] = [const { std::sync::atomic::AtomicU64::new(0) }; 64];
static PROFC: [std::sync::atomic::AtomicU64; 64] = [const { std::sync::atomic::AtomicU64::new(0) }; 64];
#[inline(always)]
fn prof_cnt(k: usize, a: Tick, b: Tick) {
    // (a constant "off" in the product: port edit 5 folds clock::cnt, and this is left out)
    if clock::cnt() {
        PROFI[k].fetch_add(b.1.wrapping_sub(a.1), std::sync::atomic::Ordering::Relaxed);
        PROFC[k].fetch_add(b.2.wrapping_sub(a.2), std::sync::atomic::Ordering::Relaxed);
    }
}
/// FL_PROF: add the whole process's instructions and cycles since `a` (from clock::proc_counts)
fn prof_proc(k: usize, a: (u64, u64)) {
    if clock::cnt() {
        let b = clock::proc_counts();
        PROFI[k].fetch_add(b.0.wrapping_sub(a.0), std::sync::atomic::Ordering::Relaxed);
        PROFC[k].fetch_add(b.1.wrapping_sub(a.1), std::sync::atomic::Ordering::Relaxed);
    }
}
/// Raw (before the coder) bytes of each stream, summed over all blocks.
static PBYTES: [std::sync::atomic::AtomicU64; 16] = [const { std::sync::atomic::AtomicU64::new(0) }; 16];
fn prof_add(k: usize, t: Tick) {
    let now = Tick::now();
    PROF[k].fetch_add((now - t).as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    prof_cnt(k, t, now);
}
/// Add the time since *t to slot k and restart *t.
#[inline]
pub(crate) fn pt(k: usize, t: &mut Tick) {
    let now = Tick::now();
    PROF[k].fetch_add((now - *t).as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    prof_cnt(k, *t, now);
    *t = now;
}
const PROF_NAMES: [(usize, &str); 37] = [(30, "table bank"), (49, "rules chosen"), (50, "keyed plan"), (24, "derive rewrite"), (28, "open: decode streams"), (47, "open: derive put back"),
    (23, "line split (main)"), (9, "choose_field"), (10, "groups_of+perm"), (0, "pass1 split+lookup"), (1, "pick_words"), (2, "templates+cols"), (3, "split stats"), (4, "pass2"), (5, "clock col"), (6, "tidm encode"), (7, "put_nums+fixw"), (8, "group est+hdr"), (11, "order_stream"),
    (12, "code s0"), (13, "code s1 tids"), (14, "code s2 hdr"), (15, "code s3 kinds"), (16, "code s4 nums"), (17, "code s5 txt"), (18, "code s6 codes"), (19, "code s7 diffs"), (20, "code s8"), (21, "code s9 order"), (22, "file tables"),
    (46, "open: tables"), (40, "open: rebuild_table"), (41, "open: slot_columns"), (42, "open: line_tids"), (43, "open: ungroup nums"), (44, "open: unpack_lines"), (45, "open: ungroup lines"),
];

/// FL_NAMESAMPLE=1: month / weekday names are looked for only when a sample of the block
/// (4 runs of 256 lines) has one (any split is exact; this only saves the search).
fn names_in_sample(data: &[u8], lines: &[&[u8]], cfg: &SplitCfg) -> bool {
    let n = lines.len();
    // each line's values as pass 1 makes them (front.rs's splitter: the same as split_line's,
    // FL_CHECK)
    let mut f = front::Front::default();
    let mut vals: Vec<Val> = Vec::new();
    let runs = if n <= 1024 { vec![(0, n)] } else { (0..4).map(|k| (k * (n - 256) / 3, 256)).collect() };
    for (a, len) in runs {
        for &l in &lines[a..a + len] {
            // (kernels: a line where no short name's key is found cannot give a NAMEK value:
            // front::may_have_name; the answer is the same, without its split)
            if !front::may_have_name(l) {
                continue;
            }
            vals.clear();
            f.split(data, l.as_ptr() as usize - data.as_ptr() as usize, l.len(), cfg, &mut vals);
            if vals.iter().any(|v| v.0 == NAMEK) {
                return true;
            }
        }
    }
    false
}

/// What coding a column needs from its block.
struct ColCtx {
    // the slots of each prefix column in pattern order: cslots[cslot_at[c]..cslot_at[c + 1]]
    // (made only when some column may be grouped)
    cslot_at: Vec<u32>,
    cslots: Vec<u32>,
    prefix_cols: usize,
    any_split: bool,
    group_on: bool,
    gfull: bool,
    fixw: bool,
    nslots: usize,
}

/// One thread's working space for coding columns (slots are column-owned, so the per-slot
/// arrays need no clearing between columns).
struct ColScratch {
    slot_last: Vec<u64>,
    gsum: Vec<u64>,
    gcnt: Vec<u32>,
    gfin: Vec<u8>,
    syms: Vec<u8>,
    dix: Vec<u32>,
    table: Vec<(u64, u32)>,
    segs: Vec<Vec<u8>>,
    gset: Vec<u64>,
}

impl ColScratch {
    fn new(nslots: usize) -> ColScratch {
        ColScratch { slot_last: vec![0; nslots], gsum: vec![0; nslots], gcnt: vec![0; nslots], gfin: vec![0; nslots], syms: Vec::new(), dix: Vec::new(), table: Vec::new(), segs: vec![Vec::new(); GROUPS], gset: Vec::new() }
    }
}

/// A coded column: its put_nums mode, whether it is grouped, its numbers and cache codes,
/// and (grouped) the group of each of its slots in pattern order.
struct ColOut {
    nm: u64,
    grouped: bool,
    tmp: Vec<u8>,
    codes: Vec<u8>,
    groups: Vec<u8>,
}

/// ColBuf (M3, then M4; frozen interface, DESIGN.md 7b): one column's kinds, values, slots and
/// ids as pass 2 leaves them, plus all_kinds_equal and kind_runs. The stand-in is today's Col
/// (the two facts computed on demand); M3 records them while it writes, in an arena.
/// `code_col(ctx, ci, &mut ColBuf, ..) -> ColOut` keeps its signature (M4).
type ColBuf = Col;
#[allow(dead_code)]
impl Col {
    /// every value of the column has the same kind
    fn all_kinds_equal(&self) -> bool {
        self.kinds.windows(2).all(|w| w[0] == w[1])
    }
    /// the number of runs of equal kinds
    fn kind_runs(&self) -> usize {
        if self.kinds.is_empty() { 0 } else { 1 + self.kinds.windows(2).filter(|w| w[0] != w[1]).count() }
    }
}

/// Plan (M6, then M3; frozen interface, DESIGN.md 7b): per final template, per slot: its column,
/// its slot number, a kind class and flags; `plain` when no slot of the template has a flag
/// (M3's tight pass-2 loop takes those templates).
#[allow(dead_code)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct PlanSlot {
    col: u32,
    slot: u32,
    kclass: u8,
    flags: u8,
}
#[allow(dead_code)]
const PS_WORD: u8 = 1; // a planned word (pick_words) becomes this slot's value
#[allow(dead_code)]
const PS_TOKEN: u8 = 2; // whole tokens (M6 sets it; the stand-in does not know them)
#[allow(dead_code)]
const PS_RULE: u8 = 4; // a rule of the block fills this slot
#[allow(dead_code)]
const PS_SPLIT: u8 = 8; // the slot's column is a splitting word column
#[allow(dead_code)]
const PS_ID: u8 = 16; // long IDs (M6; per value today)
#[derive(Default, Debug)]
struct Plan {
    at: Vec<u32>, // template t: slots[at[t]..at[t + 1]]
    slots: Vec<PlanSlot>,
    plain: Vec<bool>,
}
impl Plan {
    fn tmpl(&self, t: usize) -> &[PlanSlot] {
        &self.slots[self.at[t] as usize..self.at[t + 1] as usize]
    }
}
/// The stand-in Plan builder (M3 uses it until M6's lands): today's template loop (col_of,
/// slot_base), the planned words, the block's rules and the splitting columns. Kind classes are
/// only known in pass 2 (0 here); tokens and long IDs are per value today (M6 adds them).
#[allow(clippy::too_many_arguments)]
fn plan_standin(col_of: &[Vec<u32>], slot_base: &[u32], final_of: &[u32], pidx: &[u32], plan: &[Run], cols: &[Col], rfirst: &[u32], rules: &[rules::Rule]) -> Plan {
    let mut p = Plan { at: Vec::with_capacity(col_of.len() + 1), slots: Vec::new(), plain: Vec::with_capacity(col_of.len()) };
    for (t, ids) in col_of.iter().enumerate() {
        p.at.push(p.slots.len() as u32);
        for (k, &c) in ids.iter().enumerate() {
            let split = cols.get(c as usize).is_some_and(|x| x.split);
            p.slots.push(PlanSlot { col: c, slot: slot_base[t] + k as u32, kclass: 0, flags: if split { PS_SPLIT } else { 0 } });
        }
    }
    p.at.push(p.slots.len() as u32);
    // planned words: run q of merged shape r is slot k0 + (q - first run) of its template
    for (r, &t) in final_of.iter().enumerate() {
        let k = pidx[r] as usize;
        for q in k..pidx[r + 1] as usize {
            let s = p.at[t as usize] as usize + plan[q].k0 as usize + q - k;
            if s < p.at[t as usize + 1] as usize {
                p.slots[s].flags |= PS_WORD;
            }
        }
    }
    // rules: the slot each fills (rules of template t: rules[rfirst[t]..rfirst[t + 1]])
    for t in 0..col_of.len().min(rfirst.len().saturating_sub(1)) {
        for rl in &rules[rfirst[t] as usize..rfirst[t + 1] as usize] {
            let s = p.at[t] as usize + rl.slot as usize;
            if s < p.at[t + 1] as usize {
                p.slots[s].flags |= PS_RULE;
            }
        }
    }
    for t in 0..col_of.len() {
        let plain = p.tmpl(t).iter().all(|s| s.flags == 0);
        p.plain.push(plain);
    }
    p
}

/// The Plan builder's flags (M6; the template loop gives each slot its column and number):
/// PS_WORD where a planned word sits, PS_SPLIT on slots of splitting columns, PS_RULE where a
/// rule fills the slot, PS_TOKEN where a whole token of any of the template's pass-1 shapes
/// sits; `plain`: no flag in the template. Kind classes and long IDs are known per value in
/// pass 2: kclass stays 0 and PS_ID unset.
#[allow(clippy::too_many_arguments)]
fn plan_flags(p: &mut Plan, final_of: &[u32], pidx: &[u32], plan: &[Run], cols: &[Col], rfirst: &[u32], rules: &[rules::Rule], tk: Option<&tokens::Tokens>) {
    let nt = p.at.len() - 1;
    for x in p.slots.iter_mut() {
        if cols.get(x.col as usize).is_some_and(|c| c.split) {
            x.flags |= PS_SPLIT;
        }
    }
    // planned words: run q of merged shape r is slot k0 + (q - its first run) of its template
    if !plan.is_empty() {
        for (r, &t) in final_of.iter().enumerate() {
            let (k, a, b) = (pidx[r] as usize, p.at[t as usize] as usize, p.at[t as usize + 1] as usize);
            for q in k..pidx[r + 1] as usize {
                let x = a + plan[q].k0 as usize + q - k;
                if x < b {
                    p.slots[x].flags |= PS_WORD;
                }
            }
        }
    }
    // rules of template t: rules[rfirst[t]..rfirst[t + 1]]
    for t in 0..nt.min(rfirst.len().saturating_sub(1)) {
        for rl in &rules[rfirst[t] as usize..rfirst[t + 1] as usize] {
            let x = p.at[t] as usize + rl.slot as usize;
            if x < p.at[t + 1] as usize {
                p.slots[x].flags |= PS_RULE;
            }
        }
    }
    // tokens: the e-th token of a pass-1 shape is mark j0 + e - (the values of the tokens before
    // it) of the merged shape; the template has one more slot before it per planned run before it
    if let Some(tk) = tk {
        for r0 in 0..tk.smap.len() {
            let (x0, x1) = (tk.tidx[r0] as usize, tk.tidx[r0 + 1] as usize);
            if x0 == x1 {
                continue;
            }
            let r = tk.smap[r0] as usize;
            let (t, runs) = (final_of[r] as usize, &plan[pidx[r] as usize..pidx[r + 1] as usize]);
            let (mut gone, mut q) = (0usize, 0usize);
            for (e, tok) in tk.toks[x0..x1].iter().enumerate() {
                let ms = tok.j0() + e - gone;
                gone += tok.nv();
                while q < runs.len() && runs[q].k0 as usize <= ms {
                    q += 1;
                }
                let x = p.at[t] as usize + ms + q;
                if x < p.at[t + 1] as usize {
                    p.slots[x].flags |= PS_TOKEN;
                }
            }
        }
    }
    p.plain = (0..nt).map(|t| p.tmpl(t).iter().all(|x| x.flags == 0)).collect();
}

/// FL_SHADOW (harness only): run an old and a new version of a stage side by side on every block
/// and compare their outputs. FL_SHADOW=all, or a comma list of stages: val16, linerec, plan,
/// code_col (M0's stand-ins; a milestone adds its own stage the same way:
/// `if shadow::on("name") { shadow::same("name", blk, "what", &old, &new); }`). A difference prints
/// "SHADOW DIFF stage block what ..." and stops the run (exit 4), or with FL_SHADOW_ALL=1 is
/// counted and the run goes on. The end of the run prints one line per stage: checks, items
/// compared, differences. The switch is a cached OnceLock: the engine's port (edit 5) folds it
/// to "off", so the product leaves all of this out.
mod shadow {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    #[inline(always)]
    pub fn any() -> bool {
        static ON: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
        *ON.get_or_init(|| crate::engine::fl::no_env::var_os("FL_SHADOW").is_some())
    }
    /// stage `name` is checked in this run (reads the list: call once per block or column phase)
    pub fn on(name: &str) -> bool {
        any() && crate::engine::fl::no_env::var("FL_SHADOW").is_ok_and(|v| v.split(',').any(|x| x.trim() == "all" || x.trim() == name))
    }
    static BLOCK: AtomicU64 = AtomicU64::new(0);
    /// a number for this block (in the order blocks start)
    pub fn next_block() -> u64 {
        BLOCK.fetch_add(1, Relaxed)
    }
    type Stat = (&'static str, u64, u64, u64);
    static STATS: std::sync::Mutex<Vec<Stat>> = std::sync::Mutex::new(Vec::new());
    /// Compare a stage's old and new outputs (equal slices pass); the first difference is printed.
    pub fn same<T: PartialEq + std::fmt::Debug>(stage: &'static str, blk: u64, what: &str, old: &[T], new: &[T]) -> bool {
        let n = old.len().max(new.len());
        let first = if old == new { None } else { (0..n).find(|&i| old.get(i) != new.get(i)) };
        {
            let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
            match s.iter_mut().find(|e| e.0 == stage) {
                Some(e) => {
                    e.1 += 1;
                    e.2 += n as u64;
                    e.3 += first.is_some() as u64;
                }
                None => s.push((stage, 1, n as u64, first.is_some() as u64)),
            }
        }
        if let Some(i) = first {
            eprintln!("SHADOW DIFF {} block {} {}: lengths {} / {}, first difference at {}: old {:?} new {:?}", stage, blk, what, old.len(), new.len(), i, old.get(i), new.get(i));
            if crate::engine::fl::no_env::var_os("FL_SHADOW_ALL").is_none() {
                std::process::exit(4);
            }
            return false;
        }
        true
    }
    /// One line per stage checked (end of the run).
    pub fn report() {
        if !any() {
            return;
        }
        let s = STATS.lock().unwrap_or_else(|e| e.into_inner());
        if s.is_empty() {
            eprintln!("SHADOW nothing checked (stages: val16, linerec, plan, code_col, all)");
        }
        for e in s.iter() {
            eprintln!("SHADOW {} checks {} items {} differences {}", e.0, e.1, e.2, e.3);
        }
    }
}

/// FL_SHADOW=code_col: the new column coder (M4: the default code_col; the old one, ref's, is
/// oldcol::code_col_own).
fn code_col_new(x: &ColCtx, ci: usize, c: &mut ColBuf, may_group: bool, sc: &mut ColScratch) -> ColOut {
    code_col_own(x, ci, c, may_group, sc)
}
/// FL_SHADOW=code_col: old and new coder on two copies of column ci, each with its own scratch
/// (reused across the block's columns, as the column phase does); every output compared.
fn shadow_code_col(x: &ColCtx, ci: usize, c: &Col, may_group: bool, blk: u64, sc: &mut (ColScratch, ColScratch)) {
    let (mut a, mut b) = (c.clone(), c.clone());
    let oa = oldcol::code_col_own(x, ci, &mut a, may_group, &mut sc.0);
    let ob = code_col_new(x, ci, &mut b, may_group, &mut sc.1);
    shadow::same("code_col", blk, &format!("column {} mode, grouped", ci), &[(oa.nm, oa.grouped)], &[(ob.nm, ob.grouped)]);
    shadow::same("code_col", blk, &format!("column {} numbers", ci), &oa.tmp, &ob.tmp);
    shadow::same("code_col", blk, &format!("column {} codes", ci), &oa.codes, &ob.codes);
    shadow::same("code_col", blk, &format!("column {} groups", ci), &oa.groups, &ob.groups);
    shadow::same("code_col", blk, &format!("column {} kinds after", ci), &a.kinds, &b.kinds);
    shadow::same("code_col", blk, &format!("column {} values after", ci), &a.vals, &b.vals);
}

/// Code column ci (put_nums, then the grouping by slot when it pays); `may_group`: the block's
/// cap on grouped columns is not reached. (Not inlined: one copy for both column phases.)
#[inline(never)]
fn code_col(x: &ColCtx, ci: usize, c: &mut Col, may_group: bool, sc: &mut ColScratch, sink: &mut Sink) -> ColOut {
    if x.fixw {
        fixed_width(c);
    }
    // a big column of differences: its numbers in GROUPS segments by pattern (see GROUP_MIN)
    let may = c.vals.len() >= GROUP_MIN && c.kinds.len() == c.vals.len() && x.group_on && may_group && ci < x.prefix_cols && !x.any_split && !c.idc;
    // (a column that may be grouped and is written as plain differences: written below)
    // keyed values (entity.rs): per number its rank (R + 1: no entity) and its entity's last
    // new value (columns of long IDs keep their own way)
    let (mut ek, mut el) = (Vec::new(), Vec::new());
    if !c.ke.is_empty() && c.ids.is_empty() {
        ek = vec![entity::R as u8 + 1; c.vals.len()];
        el = vec![0u64; c.vals.len()];
        for &(i, r, last) in &c.ke {
            ek[i as usize] = r;
            el[i as usize] = last;
        }
    }
    let nm = put_nums(&c.vals, &c.slots, &c.ids, &c.steps, &mut sc.slot_last, &mut sc.syms, &mut sc.dix, &mut sc.table, sink, may, &mut sc.gset, &ek, &el);
    let (gsum, gcnt, gfin) = (&mut sc.gsum, &mut sc.gcnt, &mut sc.gfin);
    let mut groups = Vec::new();
    let mut grouped = nm == 1 && may;
    // the column's slots in pattern order (ColCtx::cslots)
    let cslots: &[u32] = if grouped { &x.cslots[x.cslot_at[ci] as usize..x.cslot_at[ci + 1] as usize] } else { &[] };
    if grouped {
        // the slots' typical sizes and the estimate on a sample (FL_GFULL=1: all numbers):
        // up to 8 runs of 4096 numbers spread over the column
        let n = c.vals.len();
        let (chunks, clen) = if n <= 32768 || x.gfull { (1, n) } else { (8, 4096) };
        let runs = |k: usize| -> (usize, u64) {
            let st = if chunks == 1 { 0 } else { k * (n - clen) / (chunks - 1) };
            (st, if st > 0 { c.vals[st - 1] } else { 0 })
        };
        // The estimate below (each slot's group from its sampled differences' mean bit
        // length; bits of the groups apart against together) is worked out in full only when
        // the sampled slots fall in two groups or more. With one group the groups' counts are
        // all the counts, so "apart" equals "together" (A) and the column is grouped exactly
        // when A > GROUP_GAIN * A fails, that is when A = 0: every sampled difference has the
        // same bit length, at most 1 (zigzag value 0 or 1; any other mix of counts gives A > 0).
        // `zo`/`za`: OR and AND of the sampled zigzag differences (A = 0: zo <= 1, za == zo).
        let (mut zo, mut za) = (0u64, u64::MAX);
        let mut one = true; // one group so far
        if cslots.len() == 1 {
            // one slot: one group
            for k in 0..chunks {
                let (st, mut prev) = runs(k);
                for &v in &c.vals[st..st + clen] {
                    let z = zz((v as i64).wrapping_sub(prev as i64));
                    zo |= z;
                    za &= z;
                    prev = v;
                }
            }
        } else {
            for k in 0..chunks {
                let (st, prev) = runs(k);
                group_sums(&c.vals[st..st + clen], &c.slots[st..st + clen], prev, gsum, gcnt, &mut zo, &mut za);
            }
            // each slot's group (slots not in the sample: group 0); one group among the
            // sampled slots?
            let mut g0 = u8::MAX;
            for &sl in cslots {
                let sl = sl as usize;
                gfin[sl] = if gcnt[sl] > 0 {
                    let g = group_of(gsum[sl], gcnt[sl]) as u8;
                    if g0 == u8::MAX {
                        g0 = g;
                    }
                    one &= g == g0;
                    g
                } else {
                    0
                };
            }
        }
        let a_zero = zo <= 1 && za == zo;
        if one && !a_zero {
            grouped = false;
        } else {
            if cslots.len() == 1 {
                // (the rare case A = 0 of one slot: the full way, from its sums)
                let sl = cslots[0] as usize;
                gsum[sl] += zo * (chunks * clen) as u64; // every difference's bit length is zo
                gcnt[sl] += (chunks * clen) as u32;
                gfin[sl] = group_of(gsum[sl], gcnt[sl]) as u8;
            }
            // only when the groups' sizes differ enough (estimated bits of the groups apart
            // against together): a split also cuts runs that copies would have covered
            let mut ge: Vec<Est> = (0..GROUPS).map(|_| Est::new()).collect();
            let mut all = Est::new();
            // (counted in 2 partial tables taken in turn, so that a repeated count does not
            // wait for itself; the same sums)
            let mut h2 = Box::new([[[0u32; 65]; GROUPS]; 2]);
            for k in 0..chunks {
                let (st, prev) = runs(k);
                group_counts(&c.vals[st..st + clen], &c.slots[st..st + clen], prev, gfin, &mut h2);
            }
            for (g, e) in ge.iter_mut().enumerate() {
                for t in h2.iter() {
                    for (a, &b) in e.h.iter_mut().zip(&t[g]) {
                        *a += b;
                    }
                }
            }
            for e in &ge {
                for (a, &b) in all.h.iter_mut().zip(&e.h) {
                    *a += b;
                }
            }
            let apart: f64 = ge.iter().map(|e| e.bits()).sum();
            if apart > GROUP_GAIN * all.bits() {
                grouped = false;
            }
        }
        if !grouped {
            // (the sums go back to 0 for the next column: all the column's slots)
            for &sl in cslots {
                gsum[sl as usize] = 0;
                gcnt[sl as usize] = 0;
            }
        }
    }
    if grouped {
        // the differences straight into their segments (each with room for 10 bytes per
        // number), then the segments one after the other
        let n = c.vals.len();
        let segs = &mut sc.segs;
        let mut ends: [*mut u8; GROUPS] = [std::ptr::null_mut(); GROUPS];
        let mut bases: [*mut u8; GROUPS] = [std::ptr::null_mut(); GROUPS];
        for (g, seg) in segs.iter_mut().enumerate() {
            seg.clear();
            if seg.capacity() < 10 * n + 16 {
                // (a new buffer, not a grown one: growing copies the old room)
                *seg = Vec::with_capacity(10 * n + 16);
            }
            bases[g] = seg.as_mut_ptr();
            ends[g] = bases[g];
        }
        // SAFETY: each segment has room for 10 bytes per number of the column
        unsafe { put_grouped(&c.vals, &c.slots, gfin, &mut ends) };
        let (out, _) = sink.parts(1);
        for (g, seg) in segs.iter_mut().enumerate() {
            unsafe { seg.set_len(ends[g].offset_from(bases[g]) as usize) };
            out.extend_from_slice(seg);
        }
        for &sl in cslots {
            let sl = sl as usize;
            groups.push(gfin[sl]);
            gsum[sl] = 0;
            gcnt[sl] = 0;
        }
    } else if nm == 1 && may {
        // plain differences (put_nums left them to this place)
        let (out, _) = sink.parts(1);
        put_varints(out, 10 * c.vals.len() + 16, |p| unsafe { put_diffs(p, &c.vals) });
    }
    ColOut { nm, grouped, tmp: Vec::new(), codes: Vec::new(), groups }
}

/// The grouping estimate's first pass over one run (prev: the number before it): OR and AND
/// of the zigzag differences into zo, za, and per slot the sum of their bit lengths and their
/// count added to gsum, gcnt. (A run of numbers at one slot is summed in registers, then
/// added: the same sums; bit lengths as 64 per number less the leading zeros.)
#[inline(never)]
fn group_sums(vals: &[u64], slots: &[u32], mut prev: u64, gsum: &mut [u64], gcnt: &mut [u32], zo: &mut u64, za: &mut u64) {
    let (mut rs, mut rlz, mut rcnt) = (usize::MAX, 0u64, 0u32);
    let (mut o, mut a) = (*zo, *za);
    for (&v, &sl) in vals.iter().zip(slots) {
        let sl = (sl & SLOT) as usize;
        let z = zz((v as i64).wrapping_sub(prev as i64));
        o |= z;
        a &= z;
        if sl != rs {
            if rs != usize::MAX {
                gsum[rs] += 64 * rcnt as u64 - rlz;
                gcnt[rs] += rcnt;
            }
            (rs, rlz, rcnt) = (sl, 0, 0);
        }
        rlz += z.leading_zeros() as u64;
        rcnt += 1;
        prev = v;
    }
    if rs != usize::MAX {
        gsum[rs] += 64 * rcnt as u64 - rlz;
        gcnt[rs] += rcnt;
    }
    (*zo, *za) = (o, a);
}

/// The grouping estimate's second pass over one run (prev: the number before it): the count of
/// each difference's bit length in its slot's group, h[t][g][b], two numbers per round into the
/// two partial tables t (the sums are the same however they are split).
#[inline(never)]
fn group_counts(vals: &[u64], slots: &[u32], mut prev: u64, gfin: &[u8], h: &mut [[[u32; 65]; GROUPS]; 2]) {
    let n = vals.len().min(slots.len());
    let (vals, slots) = (&vals[..n], &slots[..n]);
    let mut i = 0;
    while i + 2 <= n {
        let (v0, v1) = (vals[i], vals[i + 1]);
        let g0 = gfin[(slots[i] & SLOT) as usize] as usize;
        let g1 = gfin[(slots[i + 1] & SLOT) as usize] as usize;
        let b0 = 64 - zz((v0 as i64).wrapping_sub(prev as i64)).leading_zeros();
        let b1 = 64 - zz((v1 as i64).wrapping_sub(v0 as i64)).leading_zeros();
        h[0][g0][b0 as usize] += 1;
        h[1][g1][b1 as usize] += 1;
        prev = v1;
        i += 2;
    }
    if i < n {
        let g = gfin[(slots[i] & SLOT) as usize] as usize;
        h[0][g][(64 - zz((vals[i] as i64).wrapping_sub(prev as i64)).leading_zeros()) as usize] += 1;
    }
}

/// The differences of a grouped column into the segments of their slots' groups (gfin), each
/// written at ends[g] (moved on). (A loop of its own: the write places stay in registers.)
/// SAFETY: every ends[g] has room for 10 bytes per number.
#[inline(never)]
unsafe fn put_grouped(vals: &[u64], slots: &[u32], gfin: &[u8], ends: &mut [*mut u8; GROUPS]) {
    // (the write place of the current group stays in a register while the group repeats)
    let mut prev = 0u64;
    let (mut cg, mut cp) = (0usize, ends[0]);
    for (&v, &sl) in vals.iter().zip(slots) {
        let g = gfin[(sl & SLOT) as usize] as usize;
        if g != cg {
            ends[cg] = cp;
            cg = g;
            cp = ends[g];
        }
        if v == prev {
            // (difference 0: one zero byte)
            unsafe { // SAFETY: each segment has room for 10 bytes per number; a repeat writes 1
                *cp = 0;
                cp = cp.add(1);
            }
            continue;
        }
        cp = unsafe { varint_at(cp, zz((v as i64).wrapping_sub(prev as i64))) }; // SAFETY: at most 10 bytes, within the segment's room of 10 per number
        prev = v;
    }
    ends[cg] = cp;
}

/// code_col into buffers of the column's own (the parallel column phase), in the ColOut.
fn code_col_own(x: &ColCtx, ci: usize, c: &mut Col, may_group: bool, sc: &mut ColScratch) -> ColOut {
    // (room for the longest varint per number and a code each: no regrowth; pages that are
    // not written are not used)
    let mut tmp = Vec::with_capacity(10 * c.vals.len() + 16);
    let mut codes = Vec::with_capacity(c.vals.len() + 16);
    let mut o = code_col(x, ci, c, may_group, sc, &mut Sink { a: &mut tmp, b: None, codes: &mut codes });
    o.tmp = tmp;
    o.codes = codes;
    o
}

/// Format 2 (engine id 5): a block's pattern ids (stream 1) in the other forms worth a try;
/// the coder keeps the form that codes smallest (code_streams; the plain form on a tie), so a
/// form is used only where it makes the block smaller. `tids`: the plain form, a varint per
/// line. A form's stream starts with the bytes 0x80 + form, 0 (a varint with a needless 0
/// byte, which the plain form never holds), then a varint per line:
/// - form 1: 0 = the next new pattern, else the pattern id + 1;
/// - form 2: 0 = the pattern that last followed the previous line's pattern (the next-pattern
///   guess), 1 = the next new pattern, else the pattern id + 2.
///
/// The next new pattern is `seen`, the count of lines so far whose pattern was the next new
/// one (ids are numbered in order of first use, so: the patterns used so far). Each try is
/// one more coding of the stream, so it is made only where it pays for its time (measured
/// on the 16 logs): form 1 when new patterns are at least 1 line in 256 (below that it wins
/// at most a few hundred bytes a block, SSH and BGL, and loses on Spark, HPC and Hadoop);
/// both forms on blocks of up to 32K lines (form 2 on bigger blocks paid only on BGL, 0.07%
/// of its size, for about 0.4% more packing time over the 16 logs).
/// FL_NOB3=1: neither (the plain form only, engine id 4's bytes).
fn id_forms(tids: &[u8], lines: usize, patterns: usize) -> Vec<Vec<u8>> {
    let small = lines <= 1 << 15;
    let mut ids = Vec::with_capacity(lines);
    if !(small || patterns * 256 >= lines) || env_on("FL_NOB3") || read_ids(tids, lines, &mut vec![0; patterns], &mut ids).is_none() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(2);
    let mut last = vec![u32::MAX; patterns];
    for form in 1..=1 + small as u32 {
        let mut o = Vec::with_capacity(lines + lines / 2 + 8);
        o.extend_from_slice(&[0x80 | form as u8, 0]);
        last.fill(u32::MAX);
        let (mut prev, mut seen) = (usize::MAX, 0u32);
        for &t in &ids {
            let c = if form == 2 && last.get(prev) == Some(&t) {
                0
            } else if t == seen {
                form - 1
            } else {
                t + form
            };
            put_varint(&mut o, c as u64);
            if let Some(l) = last.get_mut(prev) {
                *l = t;
            }
            seen += (t == seen) as u32;
            prev = t as usize;
        }
        // (a form at most 5 bytes per pattern longer than the plain form: fewer-pack's opener limits
        // a block's streams all together, and its packer checks that limit with the plain form
        // and 10 bytes per pattern for the block's pattern list, which takes at most 5)
        if o.len() <= tids.len() + 5 * patterns {
            out.push(o);
        }
    }
    out
}

/// The pattern of each line of a block from its stream 1 `b` (the plain form or a form of
/// id_forms) into `out`, and the lines of each pattern counted in `per` (one entry per
/// pattern). None when damaged: fewer than `lines` varints, a pattern id at or past the
/// pattern count, a guess with no previous line or no pattern that followed the previous
/// line's pattern yet, or an unknown form (0x80 or 0x83-0xff before a 0 byte at the start).
fn read_ids(b: &[u8], lines: usize, per: &mut [u32], out: &mut Vec<u32>) -> Option<()> {
    // (the plain form is form 0: every varint is the id itself)
    let (form, mut p) = match b {
        [0x81, 0, ..] => (1u64, 2),
        [0x82, 0, ..] => (2, 2),
        [0x80..=0xff, 0, ..] => return None,
        _ => (0, 0),
    };
    let mut last = vec![u32::MAX; if form == 2 { per.len() } else { 0 }];
    let (mut prev, mut seen) = (usize::MAX, 0u64);
    for _ in 0..lines {
        let c = heads::rd(b, &mut p)?;
        let t = if c >= form {
            c - form
        } else if c + 1 == form {
            seen
        } else {
            // form 2, c = 0: the guess
            match *last.get(prev)? {
                u32::MAX => return None,
                g => g as u64,
            }
        };
        *per.get_mut(usize::try_from(t).ok()?)? += 1;
        if let Some(l) = last.get_mut(prev) {
            *l = t as u32;
        }
        seen += (t == seen) as u64;
        prev = t as usize;
        out.push(t as u32);
    }
    Some(())
}

/// The pattern ids coded with TidModel.
fn code_tids(tid_list: &[u32], patterns: usize) -> Vec<u8> {
    let mut enc = ozip::RcEnc::new();
    let mut tm = TidModel::new(patterns);
    for &t in tid_list {
        tm.step(&mut enc, t);
    }
    enc.finish()
}

/// A rule that gave every value at its slot in the block is marked `always`: its yes/no
/// answers are not coded.
fn mark_always(rules: &mut [rules::Rule], flag_list: &[u32]) {
    let mut no = vec![false; rules.len()];
    for &f in flag_list {
        no[(f >> 1) as usize] |= f & 1 != 0;
    }
    for (r, &n) in rules.iter_mut().zip(&no) {
        r.always = !n;
    }
}

/// The rules' answers, rule after rule, each rule's in line order (a near rule's yes with its
/// difference), range coded (rules::Flags) with a coder of their own; they end the column
/// header (stream 2), and the rules section gives their length. (Rule after rule: the opener
/// decodes them without a pass over the lines of patterns without rules.)
fn code_answers(enc: &mut ozip::RcEnc, rules: &[rules::Rule], flag_list: &[u32], near_list: &[i64]) {
    if rules.is_empty() {
        return;
    }
    // each rule's answers (miss, near difference), in line order
    let mut per: Vec<Vec<(u32, i64)>> = vec![Vec::new(); rules.len()];
    let mut nl = near_list.iter();
    for &f in flag_list {
        let ri = (f >> 1) as usize;
        let d = if f & 1 == 0 && rules[ri].op == rules::NEAR { *nl.next().unwrap() } else { 0 };
        per[ri].push((f & 1, d));
    }
    let mut fl = rules::Flags::new(rules.len());
    for (ri, a) in per.iter().enumerate() {
        let r = &rules[ri];
        for &(miss, d) in a {
            let yes = if r.always { true } else { fl.step(enc, ri, miss) == 0 };
            if yes && r.op == rules::NEAR {
                fl.diff(enc, ri, d);
            }
        }
    }
}

/// FL_SHADOW: M0's stand-in checks of one block (val16, linerec, plan); the block's number.
#[allow(clippy::too_many_arguments)]
fn shadow_block(p1: &Pass1, col_of: &[Vec<u32>], slot_base: &[u32], final_of: &[u32], pidx: &[u32], plan: &[Run], cols: &[Col], rfirst: &[u32], rules: &[rules::Rule]) -> u64 {
    let blk = shadow::next_block();
    if shadow::on("val16") {
        // the 16-byte rows give every value's kind, number and span back (lines under 16 MB)
        let v16: Vec<Option<Val16>> = p1.vals.iter().map(Val16::from_val).collect();
        if v16.iter().all(|x| x.is_some()) {
            let a: Vec<(u8, u64, (u32, u32))> = p1.vals.iter().map(|x| (x.kind(), x.val(), x.span())).collect();
            let b: Vec<(u8, u64, (u32, u32))> = v16.iter().flatten().map(|x| (x.kind(), x.val(), x.span())).collect();
            shadow::same("val16", blk, "kind, value, span of every value", &a, &b);
        } else {
            eprintln!("SHADOW val16 block {}: a value ends past 16 MB (this block keeps the 24-byte path)", blk);
        }
    }
    if shadow::on("linerec") {
        // today's walk: line i's values follow line i-1's, rnv[rid[i]] of them
        let mut old = Vec::with_capacity(p1.rid.len());
        let mut at = 0u32;
        for &r in &p1.rid {
            old.push((r, at));
            at += p1.rnv[r as usize];
        }
        let new: Vec<(u32, u32)> = line_recs(p1).iter().map(|l| (l.rid, l.row)).collect();
        shadow::same("linerec", blk, "shape, value row of every line", &old, &new);
        shadow::same("linerec", blk, "values in all rows", &[at as usize], &[p1.vals.len()]);
    }
    if shadow::on("plan") {
        let pl = plan_standin(col_of, slot_base, final_of, pidx, plan, cols, rfirst, rules);
        // old: today's template loop; new: the Plan's slots
        let mut old = Vec::new();
        for (t, ids) in col_of.iter().enumerate() {
            for (k, &c) in ids.iter().enumerate() {
                old.push((c, slot_base[t] + k as u32));
            }
        }
        let new: Vec<(u32, u32)> = pl.slots.iter().map(|s| (s.col, s.slot)).collect();
        shadow::same("plan", blk, "column, slot number of every template slot", &old, &new);
        // flags: words and rules as today's lists give them
        let words = pl.slots.iter().filter(|s| s.flags & PS_WORD != 0).count();
        let rl = pl.slots.iter().filter(|s| s.flags & PS_RULE != 0).count();
        let mut wslots = std::collections::HashSet::new();
        for (r, &t) in final_of.iter().enumerate() {
            for q in pidx[r] as usize..pidx[r + 1] as usize {
                wslots.insert((t, plan[q].k0 as usize + q - pidx[r] as usize));
            }
        }
        let rslots: std::collections::HashSet<(usize, u16)> = (0..col_of.len().min(rfirst.len().saturating_sub(1))).flat_map(|t| rules[rfirst[t] as usize..rfirst[t + 1] as usize].iter().map(move |x| (t, x.slot))).collect();
        shadow::same("plan", blk, "word slots, rule slots", &[words, rl], &[wslots.len(), rslots.len()]);
    }
    blk
}

/// FL_SHADOW (M6, harness only): the block's decisions as made before M6 (pick_tokens_old,
/// pick_words_old, the template map of boxed texts, slot_cols_pack, block_rules' own counts).
struct DecOld {
    tk: Option<tokens::Tokens>,
    pidx: Vec<u32>,
    plan: Vec<Run>,
    wid: Vec<u32>,
    words_out: Vec<u8>,
    tmpl_out: Vec<u8>,
    col_of: Vec<Vec<u32>>,
    slot_base: Vec<u32>,
    final_of: Vec<u32>,
    cols: Vec<Col>,
    rfirst: Vec<u32>,
    rules: Vec<rules::Rule>,
}
#[allow(unused_variables, unused_mut, unused_assignments)]
fn decisions_old(p1: &Pass1, lines: &[&[u8]]) -> DecOld {
    let shared = crate::engine::fl::no_env::var_os("FL_NOSHARE").is_none();
    let (rnv, rid, vals) = (&p1.rnv, &p1.rid, &p1.vals);
    let roff: Vec<u32> = std::iter::once(0).chain(p1.shapes.at.iter().map(|&(a, n)| a + n + 1)).collect();
    let cnt1: Vec<u32> = {
        let m1 = p1.rnv.len();
        let mut c2 = vec![[0u32; 2]; m1];
        let mut ch = rid.chunks_exact(2);
        for x in &mut ch {
            c2[x[0] as usize][0] += 1;
            c2[x[1] as usize][1] += 1;
        }
        for &r in ch.remainder() {
            c2[r as usize][0] += 1;
        }
        c2.iter().map(|x| x[0] + x[1]).collect()
    };
    let tk = if tokens::tokens_on() { tokens::pick_tokens_old(p1, &roff, lines, &cnt1) } else { None };
    let (rbuf, roff) = match &tk {
        Some(t) => (&t.merged.text, std::iter::once(0).chain(t.merged.at.iter().map(|&(a, n)| a + n + 1)).collect()),
        None => (&p1.shapes.text, roff),
    };
    let smap = |r: u32| -> usize {
        match &tk {
            Some(t) => t.smap[r as usize] as usize,
            None => r as usize,
        }
    };
    let m = roff.len() - 1;
    // the lines of each (merged) shape
    let per_shape: Vec<u32> = match &tk {
        Some(t) => {
            let mut c = vec![0u32; m];
            for (r, &n) in cnt1.iter().enumerate() {
                c[t.smap[r] as usize] += n;
            }
            c
        }
        None => cnt1,
    };
    // varying words become values: shapes that differ only there merge into one template
    let (mut pidx, mut plan) = if words_on() { pick_words_old(rbuf, &roff, &pick_cfg()) } else { (vec![0; m + 1], Vec::new()) };
    if !plan.is_empty() {
        if !words_pay_old(rbuf, &roff, &pidx, &plan, &per_shape) {
            (pidx, plan) = (vec![0; m + 1], Vec::new());
        }
    }
    // the block's word table: those words in order of first use (shapes are numbered so),
    // and the word number of each planned run (its text is part of the shape)
    let mut wmap: FxMap<&[u8], u32> = FxMap::default();
    let mut words_out: Vec<u8> = Vec::new();
    let mut wid: Vec<u32> = Vec::with_capacity(plan.len());
    for r in 0..m {
        let s = &rbuf[roff[r] as usize..];
        for run in &plan[pidx[r] as usize..pidx[r + 1] as usize] {
            let t = &s[run.off as usize..(run.off + run.len) as usize];
            let n = wmap.len() as u32;
            wid.push(*wmap.entry(t).or_insert_with(|| {
                words_out.extend_from_slice(t);
                words_out.push(b'\n');
                n
            }));
        }
    }
    // the templates, numbered in order of first use; each slot gets a column
    let mut tmap: FxMap<Box<[u8]>, u32> = FxMap::default();
    let mut tmpl_out: Vec<u8> = Vec::new();
    let mut col_of: Vec<Vec<u32>> = Vec::new();
    let mut slot_base: Vec<u32> = Vec::new(); // the first pattern slot of each pattern
    let mut nslots = 0u32;
    let mut cols: Vec<Col> = Vec::new();
    // shared columns: a slot whose template text before it is the same in two templates
    // (the line header: time, host, pid) uses one column, so values stay in line order
    // (room for every slot of the shapes: the marks in their text, and one per planned word;
    // at most 2^19 at first, 16 MB: a block with more slots grows the map as needed)
    let mut by_piece = PieceMap::new((rbuf.iter().filter(|&&c| c == MARK).count() + plan.len()).min((1 << 19) - 1));
    let mut final_of: Vec<u32> = Vec::with_capacity(m);
    let mut g: Vec<u8> = Vec::new();
    for r in 0..m {
        let s = &rbuf[roff[r] as usize..roff[r + 1] as usize - 1];
        let pl = &plan[pidx[r] as usize..pidx[r + 1] as usize];
        let s: &[u8] = if pl.is_empty() {
            s
        } else {
            merge_shape(s, pl, &mut g);
            &g
        };
        let tid = match tmap.get(s) {
            Some(&t) => t,
            None => {
                let t = col_of.len() as u32;
                tmap.insert(s.to_vec().into_boxed_slice(), t);
                tmpl_out.extend_from_slice(s);
                tmpl_out.push(b'\n');
                let mut ids = Vec::new();
                if shared {
                    slot_cols_pack(s, &mut by_piece, &mut ids);
                    cols.resize_with(by_piece.len(), Col::default);
                } else {
                    for _ in s.iter().filter(|&&c| c == MARK) {
                        cols.push(Col::default());
                        ids.push((cols.len() - 1) as u32);
                    }
                }
                slot_base.push(nslots);
                nslots += ids.len() as u32;
                col_of.push(ids);
                t
            }
        };
        final_of.push(tid);
    }
    let prefix_cols = cols.len();
    // a word column "splits" when it is used 200+ times with 2-16 different words that change
    // at least every other use: then each word gets its own columns for the line's later slots,
    // so the numbers after it (like a countdown per job) are not mixed in one column
    let mut any_split = false;
    if !plan.is_empty() {
        // per column: uses, changes, last word, different words (up to 17)
        let mut st: Vec<(u32, u32, u32, Vec<u32>)> = vec![(0, 0, u32::MAX, Vec::new()); cols.len()];
        for &r in rid.iter() {
            let r = smap(r);
            let k = pidx[r] as usize;
            for q in k..pidx[r + 1] as usize {
                let e = &mut st[col_of[final_of[r] as usize][plan[q].k0 as usize + q - k] as usize];
                e.0 += 1;
                if e.2 != wid[q] {
                    e.1 += 1;
                    e.2 = wid[q];
                    if e.3.len() <= 64 && !e.3.contains(&wid[q]) {
                        e.3.push(wid[q]);
                    }
                }
            }
        }
        static SP: crate::engine::fl::NoEnvOnce<(u32, usize, u32)> = crate::engine::fl::NoEnvOnce::new();
        let (smin, smax, schg) = *SP.get_or_init(|| {
            let e = |k: &str, d: u32| crate::engine::fl::no_env::var(k).ok().and_then(|x| x.parse().ok()).unwrap_or(d);
            (e("FL_SPLITMIN", 200), e("FL_SPLITMAX", 16) as usize, e("FL_SPLITCHG", 2))
        });
        for (c, e) in cols.iter_mut().zip(&st) {
            c.split = e.0 >= smin && (2..=smax).contains(&e.3.len()) && e.1 * schg >= e.0;
            any_split |= c.split;
        }
    }
    // each column's value count (lines per shape -> slots of its template), so that pass 2
    // writes into vectors of the right size (no growing)
    // rules (rules.rs; their yes/no answers are range coded at the end of the column header)
    let (rfirst, rules) = if rules_on() { block_rules_old(rid, rnv, vals, tk.as_ref(), &final_of, &pidx, &plan, &col_of, &per_shape) } else { (vec![0; col_of.len() + 1], Vec::new()) };
    DecOld { tk, pidx, plan, wid, words_out, tmpl_out, col_of, slot_base, final_of, cols, rfirst, rules }
}

/// FL_SHADOW (M6): this block's decisions against decisions_old, item by item (stages tokens,
/// pidx, wid, tmpl_out, col_of, final_of, rules), and the Plan builder's Plan against
/// plan_standin of the old outputs (stage plan; the token slots against their places in the
/// merged text).
#[allow(clippy::too_many_arguments)]
fn shadow_m6(blk: u64, p1: &Pass1, lines: &[&[u8]], tk: Option<&tokens::Tokens>, pidx: &[u32], plan: &[Run], wid: &[u32], words_out: &[u8], tmpl_out: &[u8], col_of: &[Vec<u32>], slot_base: &[u32], final_of: &[u32], cols: &[Col], rfirst: &[u32], rules: &[rules::Rule], pl6: &Plan) {
    if !["plan", "pidx", "wid", "tmpl_out", "col_of", "final_of", "rules", "tokens"].iter().any(|n| shadow::on(n)) {
        return;
    }
    let o = decisions_old(p1, lines);
    if shadow::on("tokens") {
        type TkKey = (Vec<u32>, Vec<tokens::Tok>, Vec<u32>, Vec<u8>);
        let key = |t: Option<&tokens::Tokens>| -> TkKey {
            match t {
                Some(t) => (t.tidx.clone(), t.toks.clone(), t.smap.clone(), t.merged.text.clone()),
                None => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            }
        };
        let (a, b) = (key(o.tk.as_ref()), key(tk));
        shadow::same("tokens", blk, "tokens chosen", &[o.tk.is_some()], &[tk.is_some()]);
        shadow::same("tokens", blk, "tidx", &a.0, &b.0);
        shadow::same("tokens", blk, "toks", &a.1, &b.1);
        shadow::same("tokens", blk, "smap", &a.2, &b.2);
        shadow::same("tokens", blk, "merged shapes", &a.3, &b.3);
    }
    let rk = |x: &[Run]| x.iter().map(|r| (r.off, r.len, r.k0, r.h)).collect::<Vec<_>>();
    if shadow::on("pidx") {
        shadow::same("pidx", blk, "pidx", &o.pidx, pidx);
        shadow::same("pidx", blk, "planned runs", &rk(&o.plan), &rk(plan));
    }
    if shadow::on("wid") {
        shadow::same("wid", blk, "wid", &o.wid, wid);
        shadow::same("wid", blk, "word table", &o.words_out, words_out);
    }
    if shadow::on("tmpl_out") {
        shadow::same("tmpl_out", blk, "template text", &o.tmpl_out, tmpl_out);
    }
    if shadow::on("col_of") {
        let flat = |x: &[Vec<u32>]| x.iter().flat_map(|v| std::iter::once(u32::MAX - v.len() as u32).chain(v.iter().copied())).collect::<Vec<u32>>();
        shadow::same("col_of", blk, "col_of", &flat(&o.col_of), &flat(col_of));
        shadow::same("col_of", blk, "slot_base", &o.slot_base, slot_base);
        let sp = |c: &[Col]| c.iter().map(|c| c.split).collect::<Vec<bool>>();
        shadow::same("col_of", blk, "columns and their split flags", &sp(&o.cols), &sp(cols));
    }
    if shadow::on("final_of") {
        shadow::same("final_of", blk, "final_of", &o.final_of, final_of);
    }
    if shadow::on("rules") {
        shadow::same("rules", blk, "rfirst", &o.rfirst, rfirst);
        shadow::same("rules", blk, "rules", &o.rules, rules);
    }
    if shadow::on("plan") {
        let st = plan_standin(&o.col_of, &o.slot_base, &o.final_of, &o.pidx, &o.plan, &o.cols, &o.rfirst, &o.rules);
        shadow::same("plan", blk, "M6 Plan: template starts", &st.at, &pl6.at);
        let strip = |x: &[PlanSlot]| x.iter().map(|s| PlanSlot { flags: s.flags & !PS_TOKEN, ..*s }).collect::<Vec<_>>();
        shadow::same("plan", blk, "M6 Plan: column, slot number, word / rule / split flags of every slot", &st.slots, &strip(&pl6.slots));
        // token slots, from the places of the tokens' marks in the merged text
        let mut want: Vec<(u32, u32)> = Vec::new();
        if let Some(t) = tk {
            for r0 in 0..t.smap.len() {
                let (x0, x1) = (t.tidx[r0] as usize, t.tidx[r0 + 1] as usize);
                let r = t.smap[r0] as usize;
                let (a, n) = t.merged.at[r];
                let mt = &t.merged.text[a as usize..(a + n) as usize];
                let tt = final_of[r];
                let mut cut = 0usize;
                for tok in &t.toks[x0..x1] {
                    let (off, len) = tok.place();
                    let pos = off as usize - cut;
                    cut += len as usize - 1;
                    let ms = mt[..pos].iter().filter(|&&c| c == MARK).count();
                    let runs = plan[pidx[r] as usize..pidx[r + 1] as usize].iter().filter(|x| (x.off as usize) < pos).count();
                    want.push((tt, (ms + runs) as u32));
                }
            }
        }
        want.sort_unstable();
        want.dedup();
        let mut got: Vec<(u32, u32)> = Vec::new();
        for tt in 0..pl6.at.len() - 1 {
            for (k, x) in pl6.tmpl(tt).iter().enumerate() {
                if x.flags & PS_TOKEN != 0 {
                    got.push((tt as u32, k as u32));
                }
            }
        }
        shadow::same("plan", blk, "M6 Plan: token slots (template, slot)", &want, &got);
        let plain: Vec<bool> = (0..pl6.at.len() - 1).map(|tt| st.plain[tt] && !pl6.tmpl(tt).iter().any(|x| x.flags & PS_TOKEN != 0)).collect();
        shadow::same("plan", blk, "M6 Plan: plain templates", &plain, &pl6.plain);
    }
}

/// The old pass 2 (the loop pack_block had before front.rs's cursors): the reference that
/// FL_SHADOW=pass2 compares the plan-driven pass 2 with, and FL_OLDPASS2=1's pass 2. Harness
/// only: the product leaves it out (both switches are constant "off" there).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn pass2_old<'a>(lines: &[&'a [u8]], rid: &[u32], rnv: &[u32], vals: &[Val], tk: Option<&tokens::Tokens>, final_of: &[u32], col_of: &[Vec<u32>], slot_base: &[u32], pidx: &[u32], plan: &[Run], wid: &[u32], nslots: u32, rfirst: &[u32], rules: &[rules::Rule], use_ks: bool, want_times: bool, tidm: bool, cols: &mut Vec<Col>, tids: &mut Vec<u8>, tid_list: &mut Vec<u32>, kind_last: &mut [u64; 256], id_map: &mut IdMap, sub: &mut FxMap<(u32, u64), u32>, sub_slot: &mut FxMap<(u32, u32), u32>, clock_vals: &mut Vec<(u32, u32, u64, i64)>, per_tmpl: &mut [u32], wmap: &mut FxMap<&'a [u8], u32>, words_out: &mut Vec<u8>, flag_list: &mut Vec<u32>, near_list: &mut Vec<i64>, xs: &mut [u64], rminus: &mut [u32]) {
    let mut vi = 0usize;
    // whole tokens: the line's values with each token's values replaced by one (kind TOKK, its
    // number in the word table); the last word at each token place saves most lookups
    let mut lv2: Vec<Val> = Vec::new();
    let mut tlast: Vec<(&[u8], u32)> = vec![(&[][..], 0); tk.map_or(0, |t| t.toks.len())];
    for (li, &l) in lines.iter().enumerate() {
        let r0 = rid[li] as usize;
        let lv0 = &vals[vi..vi + rnv[r0] as usize];
        vi += rnv[r0] as usize;
        let (r, lv): (usize, &[Val]) = match tk {
            Some(t) if t.tidx[r0] < t.tidx[r0 + 1] => {
                lv2.clear();
                let mut j = 0;
                for x in t.tidx[r0] as usize..t.tidx[r0 + 1] as usize {
                    let tok = &t.toks[x];
                    lv2.extend_from_slice(&lv0[j..tok.j0()]);
                    let (s, e) = tok.span(lv0);
                    let w = &l[s as usize..e as usize];
                    let id = if tlast[x].0 == w {
                        tlast[x].1
                    } else {
                        let n = wmap.len() as u32;
                        let id = *wmap.entry(w).or_insert_with(|| {
                            words_out.extend_from_slice(w);
                            words_out.push(b'\n');
                            n
                        });
                        tlast[x] = (w, id);
                        id
                    };
                    lv2.push((TOKK, id as u64, s, e));
                    j = tok.j0() + tok.nv();
                }
                lv2.extend_from_slice(&lv0[j..]);
                (t.smap[r0] as usize, &lv2[..])
            }
            Some(t) => (t.smap[r0] as usize, lv0),
            None => (r0, lv0),
        };
        let tid = final_of[r];
        per_tmpl[tid as usize] += 1;
        if tidm {
            tid_list.push(tid);
        } else {
            put_varint(tids, tid as u64);
        }
        let cs = &col_of[tid as usize];
        let sb = slot_base[tid as usize];
        // the next planned word (its slot: values before it + words before it), the next value
        let (p0, p1) = (pidx[r] as usize, pidx[r + 1] as usize);
        let (mut q, mut j, mut ctx) = (p0, 0usize, 0u64);
        // this pattern's rules (in slot order)
        let (mut ri, re) = (rfirst[tid as usize] as usize, rfirst[tid as usize + 1] as usize);
        let has_r = ri < re;
        // the slots (two copies: lines of patterns with rules, and the rest without their checks)
        macro_rules! slots {
            ($R:expr) => {
            for k in 0..cs.len() {
                let (ci, sl) = if ctx == 0 {
                    (cs[k], sb + k as u32)
                } else {
                    let n = cols.len() as u32;
                    let ci = *sub.entry((cs[k], ctx)).or_insert(n);
                    if ci == n {
                        let split = cols[cs[k] as usize].split;
                        cols.push(Col { split, ..Default::default() });
                    }
                    let ns = nslots + sub_slot.len() as u32;
                    (ci, *sub_slot.entry((ci, sb + k as u32)).or_insert(ns))
                };
                let word = q < p1 && plan[q].k0 as usize + q - p0 == k;
                if $R {
                    if ri < re && rules[ri].slot as usize == k {
                        // a rule's slot: one yes/no; when the rule gives the value it is not stored
                        let given = if word { None } else { rules::applies(&rules[ri], &xs, lv[j].0, lv[j].1) };
                        flag_list.push((ri as u32) << 1 | given.is_none() as u32);
                        ri += 1;
                        if let Some(d) = given {
                            if rules[ri - 1].op == rules::NEAR {
                                near_list.push(d);
                            }
                            // still the last number of its kind (as without the rule)
                            kind_last[kclass(lv[j].0)] = lv[j].1;
                            xs[k] = lv[j].1;
                            rminus[cs[k] as usize] += 1;
                            j += 1;
                            continue;
                        }
                    }
                    xs[k] = if word { rules::NOT_NUM } else { rules::src_val(lv[j].0, lv[j].1) };
                }
                let c = &mut cols[ci as usize];
                if word {
                    // a word: the k-th recent word of the column, or its number in the word table
                    let id = wid[q];
                    q += 1;
                    if c.split {
                        ctx = id as u64 + 1;
                    }
                    match c.wrecent.find(id) {
                        Some(x) => {
                            c.kinds.push(WORD + 1 + x as u8);
                            c.wrecent.hit(x);
                        }
                        None => {
                            c.kinds.push(WORD);
                            if !c.ids.is_empty() {
                                c.ids.push(u32::MAX);
                            }
                            let v = id as u64;
                            c.vals.push(v);
                            let kc = kclass(WORD);
                            let ks = kind_last[kc] == v && use_ks;
                            kind_last[kc] = v;
                            c.slots.push(sl | (ks as u32) << 31);
                            c.wrecent.push(id);
                        }
                    }
                    continue;
                }
                let (kind, v, s, e) = lv[j];
                j += 1;
                if kind == TOKK {
                    // a whole token: a word, always by its number (cache codes find repeats)
                    if c.split {
                        ctx = v + 1;
                    }
                    c.kinds.push(WORD);
                    if !c.ids.is_empty() {
                        c.ids.push(u32::MAX);
                    }
                    c.vals.push(v);
                    let kc = kclass(WORD);
                    let ks = kind_last[kc] == v && use_ks;
                    kind_last[kc] = v;
                    c.slots.push(sl | (ks as u32) << 31);
                    c.wrecent.push(v as u32);
                    continue;
                }
                if (100..=103).contains(&kind) {
                    let t = &l[s as usize..e as usize];
                    c.kinds.push(kind);
                    if kind < 102 {
                        let mut k = 0;
                        while k < 36 {
                            if t[k] == b'-' {
                                k += 1;
                                continue;
                            }
                            c.txt.push(hexval(t[k]) << 4 | hexval(t[k + 1]));
                            k += 2;
                        }
                    } else {
                        put_varint(&mut c.txt, (t.len() / 2) as u64);
                        for q in t.chunks_exact(2) {
                            c.txt.push(hexval(q[0]) << 4 | hexval(q[1]));
                        }
                    }
                } else if kind == 1 {
                    let t = &l[s as usize..e as usize];
                    let hit = c.recent.iter().position(|&(a, b)| &c.txt[a as usize..b as usize] == t);
                    match hit {
                        Some(0) => c.kinds.push(2),
                        Some(k) => {
                            c.kinds.push(70 + k as u8);
                            let r = c.recent.remove(k);
                            c.recent.insert(0, r);
                        }
                        None => {
                            c.kinds.push(1);
                            put_varint(&mut c.txt, t.len() as u64);
                            let a = c.txt.len() as u32;
                            c.txt.extend_from_slice(t);
                            c.recent.insert(0, (a, c.txt.len() as u32));
                            c.recent.truncate(8);
                        }
                    }
                } else {
                    c.kinds.push(kind);
                    if kind == LONG_ID {
                        // 0: a new long ID (added to the list); d + 1: seen, d places from the newest
                        if c.ids.is_empty() {
                            c.ids.resize(c.vals.len(), u32::MAX);
                        }
                        let n = id_map.map.len() as u32;
                        let e = *id_map.map.entry(v).or_insert(n);
                        c.ids.push(if e == n { 0 } else { n - e });
                    } else if !c.ids.is_empty() {
                        c.ids.push(u32::MAX);
                    }
                    c.vals.push(v);
                    let kc = kclass(kind);
                    let ks = kind_last[kc] == v && use_ks;
                    kind_last[kc] = v;
                    c.slots.push(sl | (ks as u32) << 31);
                    if want_times && (40..68).contains(&kind) {
                        clock_vals.push((li as u32, ci, v, micros(kind, v)));
                    }
                }
            }
            };
        }
        if has_r {
            slots!(true)
        } else {
            slots!(false)
        }
    }
}

/// The memory guard of pack_block: a block may make at most COLS_FLOOR columns, or one per
/// COL_BYTES bytes of its lines when that is more. The real logs tested (the 16 logs, the
/// 2k logs, data/general, 405 files of loghub_unseen/raw, the edge cases) make at most 21,036
/// columns in a block and use at most 11% of this budget (Android: 20,476 columns in 11.8 MB,
/// one per 578 bytes); lines with a different word before 10 numbers make one per 9 to 12.
const COLS_FLOOR: usize = 1 << 16;
const COL_BYTES: usize = 64;
/// The first memory guard of pack_block, right after pass 1: a block whose distinct shapes
/// (pass 1's shape text) take more than SHAPE_FLOOR bytes and more than 1/SHAPE_DIV of the
/// block's bytes is declined before the token and word pickers, the templates and their
/// columns run: they spend 30 to 40 bytes of memory per byte of distinct shape (lines that
/// each carry their own word before a long shared text: 2.1 GB for 64 MB, for a result then
/// dropped for plain bytes; enwik8's XML: 1.9 GB on all cores, now 7% bigger as plain bytes).
/// Decided from the block's lines alone (the same whatever the thread count). The 16 test
/// logs reach at most 9.4% (Android: 1.10 MB of shapes in 11.7 MB); real logs of a code
/// editor's extension (rotated at 5 MiB) reach 22% to 27% with 1.1 to 1.4 MB of shapes (as
/// plain bytes twice as big), IDE thread dumps 50% with 0.7 to 1.2 MB (a single block:
/// SHAPE_FLOOR_ALONE). Lines with their own word reach 37% to 97%; below a third (31%: 1.5
/// GB for 256 MB on all cores) they stay within 12 bytes per byte and 300 MB.
const SHAPE_FLOOR: usize = 1 << 20;
const SHAPE_DIV: usize = 3;
/// SHAPE_FLOOR for the only block of lines of its input (bridge::pack_block_as): no other
/// block packs at the same time, so up to 4 MiB of shapes (at most 170 MB measured) stay
/// within the memory goal (a 2.4 MB thread dump: 1.17 MB of shapes, 49% of its bytes, 32 MB
/// of memory, 10% smaller than as plain bytes).
const SHAPE_FLOOR_ALONE: usize = 4 << 20;
thread_local! {
    /// The shape floor of the blocks packed on this thread (bridge::pack_block_as sets it).
    static FLOOR: std::cell::Cell<usize> = const { std::cell::Cell::new(SHAPE_FLOOR) };
}

/// A block pack_block declines (lines usize::MAX): the adapter packs it as plain bytes.
fn declined() -> (Packed, Option<u32>, Vec<i64>) {
    (Packed { streams: vec![Vec::new(); 9], lines: usize::MAX, raw: 0, alt: Vec::new(), pre: Vec::new(), mk: 0 }, None, Vec::new())
}

/// FL_NOKEYED: no keyed slots (entity.rs).
fn keyed_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NOKEYED"))
}

/// The sample for entity::plan: up to 4 runs of 512 lines spread over the block, each line's
/// pattern and its number values with their slots, as pass 2 sees them (a planned word is not
/// a number; lines with whole tokens are left out, and so are slots with rules: never keys
/// or keyed).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn entity_sample(rid: &[u32], rnv: &[u32], vals: &[Val], tk: Option<&tokens::Tokens>, final_of: &[u32], pidx: &[u32], plan: &[Run], col_of: &[Vec<u32>], rfirst: &[u32], rules: &[rules::Rule]) -> entity::Sample {
    const RUN: usize = 512;
    let n = rid.len();
    let mut s = entity::Sample::default();
    let (mut vi, mut li) = (0usize, 0usize);
    for j in 0..4 {
        let (a, b) = if n <= 4 * RUN { (0, if j == 0 { n } else { 0 }) } else { (j * (n - RUN) / 3, j * (n - RUN) / 3 + RUN) };
        while li < a {
            vi += rnv[rid[li] as usize] as usize;
            li += 1;
        }
        while li < b {
            let r0 = rid[li] as usize;
            let lv = &vals[vi..vi + rnv[r0] as usize];
            vi += lv.len();
            li += 1;
            let r = match tk {
                Some(tt) if tt.tidx[r0] < tt.tidx[r0 + 1] => continue,
                Some(tt) => tt.smap[r0] as usize,
                None => r0,
            };
            let t = final_of[r] as usize;
            let st = s.vals.len() as u32;
            let (p0, p1) = (pidx[r] as usize, pidx[r + 1] as usize);
            let (mut q, mut j) = (p0, 0usize);
            let mut ri = rfirst[t] as usize;
            for k in 0..col_of[t].len().min(u16::MAX as usize) {
                if q < p1 && plan[q].k0 as usize + q - p0 == k {
                    q += 1;
                    continue;
                }
                let (kind, v, _, _) = lv[j];
                j += 1;
                while ri < rfirst[t + 1] as usize && (rules[ri].slot as usize) < k {
                    ri += 1;
                }
                if entity::numeric(kind) && !(ri < rfirst[t + 1] as usize && rules[ri].slot as usize == k) {
                    s.vals.push((k as u16, kind, v));
                }
            }
            s.lines.push((t as u32, st, s.vals.len() as u32));
        }
    }
    s
}

/// A stream's frame as code_streams codes it at the product's level (43: fz's fast parse; the
/// column header with the table coder at table_level), stored when that is not smaller: for
/// pack_block's real-bytes checks; with an fz frame, its parsed input (the unit's table bank
/// may code it again: table_bank). (The coders are called as the product calls them, so that
/// no other level is linked in.)
fn frame(header: bool, s: &[u8]) -> (Vec<u8>, Option<fz::Prep>) {
    let n = s.len();
    if n >= 16 {
        if header {
            let c = ozip::table_coded(s, table_level(43, n));
            if c.len() < 1 + n + varint_len(n as u64) {
                return (c, None);
            }
        } else {
            // (as fz::compress_fast(s, 3): the same frame)
            let mut pr = fz::prep_fast(s, 3);
            let c = fz::code(&mut pr);
            if c.len() < 1 + n + varint_len(n as u64) {
                return (c, Some(pr));
            }
        }
    }
    (stored(s), None)
}

fn pack_block(data: &[u8], lines: &[&[u8]], want_times: bool, threads: usize) -> (Packed, Option<u32>, Vec<i64>) {
    let t_a = Tick::now();
    let mut cfg = SplitCfg::get();
    if cfg.names && !env_on("FL_NONAMESAMPLE") {
        cfg.names = names_in_sample(data, lines, &cfg);
    }
    let use_ks = crate::engine::fl::no_env::var_os("FL_NOKS").is_none();
    let shared = crate::engine::fl::no_env::var_os("FL_NOSHARE").is_none();
    let tidm = tidm_on();
    // pass 1: each line's shape and values; distinct shapes numbered in order of first use
    let p1 = pass1(data, lines, &cfg, threads);
    let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
    if p1.shapes.text.len() > FLOOR.with(|f| f.get()).max(bytes / SHAPE_DIV) {
        return declined();
    }
    let (rnv, rid, vals) = (&p1.rnv, &p1.rid, &p1.vals);
    let roff: Vec<u32> = std::iter::once(0).chain(p1.shapes.at.iter().map(|&(a, n)| a + n + 1)).collect();
    prof_add(0, t_a);
    let mut t_a = Tick::now();
    // whole tokens: chosen words become one value each; the shapes from here on are the merged
    // ones (shape r of a line: smap[rid])
    // the lines of each pass-1 shape: one pass over the lines; the counts per merged shape and
    // per pattern below are sums of these (in two partial tables taken in turn, so that a
    // repeated shape does not wait for itself; the same sums)
    let cnt1: Vec<u32> = {
        let m1 = p1.rnv.len();
        let mut c2 = vec![[0u32; 2]; m1];
        let mut ch = rid.chunks_exact(4);
        for x in &mut ch {
            c2[x[0] as usize][0] += 1;
            c2[x[1] as usize][1] += 1;
            c2[x[2] as usize][0] += 1;
            c2[x[3] as usize][1] += 1;
        }
        for (k, &r) in ch.remainder().iter().enumerate() {
            c2[r as usize][k & 1] += 1;
        }
        c2.iter().map(|x| x[0] + x[1]).collect()
    };
    // the byte classes of the shapes' text, 64 bytes per mask (pick_tokens; pick_words too when
    // no token merges shapes)
    let mut cl: Vec<[u64; 4]> = Vec::new();
    cls64(&p1.shapes.text, &mut cl);
    let tk = if tokens::tokens_on() { tokens::pick_tokens(&p1, &roff, lines, &cnt1, &cl) } else { None };
    let (rbuf, roff) = match &tk {
        Some(t) => {
            cls64(&t.merged.text, &mut cl);
            (&t.merged.text, std::iter::once(0).chain(t.merged.at.iter().map(|&(a, n)| a + n + 1)).collect())
        }
        None => (&p1.shapes.text, roff),
    };
    let smap = |r: u32| -> usize {
        match &tk {
            Some(t) => t.smap[r as usize] as usize,
            None => r as usize,
        }
    };
    let m = roff.len() - 1;
    // the lines of each (merged) shape
    let per_shape: Vec<u32> = match &tk {
        Some(t) => {
            let mut c = vec![0u32; m];
            for (r, &n) in cnt1.iter().enumerate() {
                c[t.smap[r] as usize] += n;
            }
            c
        }
        None => cnt1.clone(),
    };
    // varying words become values: shapes that differ only there merge into one template
    let (mut pidx, mut plan) = if words_on() { pick_words(&rbuf, &roff, &cl, &pick_cfg()) } else { (vec![0; m + 1], Vec::new()) };
    if !plan.is_empty() {
        if !words_pay(rbuf, &roff, &pidx, &plan, &per_shape) {
            (pidx, plan) = (vec![0; m + 1], Vec::new());
        }
    }
    pt(1, &mut t_a);
    // the block's word table: those words in order of first use (shapes are numbered so),
    // and the word number of each planned run (its text is part of the shape)
    let mut wmap: FxMap<&[u8], u32> = FxMap::default();
    let mut words_out: Vec<u8> = Vec::new();
    let mut wid: Vec<u32> = Vec::with_capacity(plan.len());
    for r in 0..m {
        let s = &rbuf[roff[r] as usize..];
        for run in &plan[pidx[r] as usize..pidx[r + 1] as usize] {
            let t = &s[run.off as usize..(run.off + run.len) as usize];
            let n = wmap.len() as u32;
            wid.push(*wmap.entry(t).or_insert_with(|| {
                words_out.extend_from_slice(t);
                words_out.push(b'\n');
                n
            }));
        }
    }
    // the templates, numbered in order of first use (each one's text and a '\n' make the block's
    // template stream); each slot gets a column, and the Plan (M6) a slot
    let mut tm = Shapes::default();
    let mut col_of: Vec<Vec<u32>> = Vec::new();
    let mut slot_base: Vec<u32> = Vec::new(); // the first pattern slot of each pattern
    let mut nslots = 0u32;
    let mut cols: Vec<Col> = Vec::new();
    // shared columns: a slot whose template text before it is the same in two templates
    // (the line header: time, host, pid) uses one column, so values stay in line order
    // (room for every slot of the shapes: the marks in their text, and one per planned word;
    // at most 2^19 at first, 16 MB: a block with more slots grows the map as needed)
    let nmarks: usize = cl.iter().map(|c| c[CL_MK].count_ones() as usize).sum();
    let mut by_piece = PieceMap::new((nmarks + plan.len()).min((1 << 19) - 1));
    let mut final_of: Vec<u32> = Vec::with_capacity(m);
    let mut g: Vec<u8> = Vec::new();
    let mut mpos: Vec<u32> = Vec::new();
    let mut pl6 = Plan { at: Vec::new(), slots: Vec::new(), plain: Vec::new() };
    // memory guard: lines that each carry their own text before values (a different word or
    // shape on most lines) make a column of every slot, about 400 bytes of memory each (100K
    // lines with a unique word and 50 numbers: 5M columns, 2.8 GB); a block whose templates
    // would make more than COLS_FLOOR columns and one per COL_BYTES bytes of the block is
    // declined (lines usize::MAX: the adapter packs it as plain bytes). Decided from the
    // block's lines alone (the same whatever the thread count).
    let col_budget = COLS_FLOOR.max(bytes / COL_BYTES);
    for r in 0..m {
        let s = &rbuf[roff[r] as usize..roff[r + 1] as usize - 1];
        let pl = &plan[pidx[r] as usize..pidx[r + 1] as usize];
        let s: &[u8] = if pl.is_empty() {
            s
        } else {
            merge_shape(s, pl, &mut g);
            &g
        };
        let (tid, new) = tm.find_or_add(s);
        if new {
            if pl.is_empty() {
                cl_places(&cl, roff[r] as usize, roff[r + 1] as usize - 1, CL_MK, &mut mpos);
            } else {
                mark_places(s, &mut mpos);
            }
            let mut ids = Vec::with_capacity(mpos.len());
            if shared {
                slot_cols_at(s, &mpos, &mut by_piece, &mut ids);
                cols.resize_with(by_piece.len(), Col::default);
            } else {
                for _ in 0..mpos.len() {
                    cols.push(Col::default());
                    ids.push((cols.len() - 1) as u32);
                }
            }
            if cols.len() > col_budget {
                return declined();
            }
            pl6.at.push(nslots);
            pl6.slots.extend(ids.iter().enumerate().map(|(k, &c)| PlanSlot { col: c, slot: nslots + k as u32, kclass: 0, flags: 0 }));
            slot_base.push(nslots);
            nslots += ids.len() as u32;
            col_of.push(ids);
        }
        final_of.push(tid);
    }
    pl6.at.push(nslots);
    let tmpl_out = tm.text;
    let prefix_cols = cols.len();
    // the pattern table is final here (stream 0 is not touched again): hand it to the hook
    if let Some(f) = TMPL_HOOK.with(|h| h.get()) {
        f(&tmpl_out);
    }
    pt(2, &mut t_a);
    // a word column "splits" when it is used 200+ times with 2-16 different words that change
    // at least every other use: then each word gets its own columns for the line's later slots,
    // so the numbers after it (like a countdown per job) are not mixed in one column
    let mut any_split = false;
    if !plan.is_empty() {
        // per column: uses, changes, last word, different words (up to 17)
        let mut st: Vec<(u32, u32, u32, Vec<u32>)> = vec![(0, 0, u32::MAX, Vec::new()); cols.len()];
        // (a line whose shape has no planned word changes nothing here)
        let has: Vec<bool> = (0..p1.rnv.len()).map(|r0| pidx[smap(r0 as u32)] < pidx[smap(r0 as u32) + 1]).collect();
        for &r in rid.iter() {
            if !has[r as usize] {
                continue;
            }
            let r = smap(r);
            let k = pidx[r] as usize;
            for q in k..pidx[r + 1] as usize {
                let e = &mut st[col_of[final_of[r] as usize][plan[q].k0 as usize + q - k] as usize];
                e.0 += 1;
                if e.2 != wid[q] {
                    e.1 += 1;
                    e.2 = wid[q];
                    if e.3.len() <= 64 && !e.3.contains(&wid[q]) {
                        e.3.push(wid[q]);
                    }
                }
            }
        }
        static SP: crate::engine::fl::NoEnvOnce<(u32, usize, u32)> = crate::engine::fl::NoEnvOnce::new();
        let (smin, smax, schg) = *SP.get_or_init(|| {
            let e = |k: &str, d: u32| crate::engine::fl::no_env::var(k).ok().and_then(|x| x.parse().ok()).unwrap_or(d);
            (e("FL_SPLITMIN", 200), e("FL_SPLITMAX", 16) as usize, e("FL_SPLITCHG", 2))
        });
        for (c, e) in cols.iter_mut().zip(&st) {
            c.split = e.0 >= smin && (2..=smax).contains(&e.3.len()) && e.1 * schg >= e.0;
            any_split |= c.split;
        }
    }
    // each column's value count (lines per shape -> slots of its template), so that pass 2
    // writes into vectors of the right size (no growing)
    // (the lines of each template and the values of each column are counted once, for this and
    // for the rules)
    let mut tcount = vec![0u32; col_of.len()];
    for (r, &n) in per_shape.iter().enumerate() {
        tcount[final_of[r] as usize] += n;
    }
    let mut ccount = vec![0u32; cols.len()];
    for (t, cs) in col_of.iter().enumerate() {
        for &c in cs {
            ccount[c as usize] += tcount[t];
        }
    }
    if !env_on("FL_NOPRESIZE") {
        // (the vectors are new and empty: with_capacity gives the capacity reserve_exact gave)
        for (c, &n) in cols.iter_mut().zip(&ccount) {
            c.kinds = Vec::with_capacity(n as usize);
            c.vals = Vec::with_capacity(n as usize);
            c.slots = Vec::with_capacity(n as usize);
        }
    }
    pt(3, &mut t_a);
    // rules (rules.rs; their yes/no answers are range coded at the end of the column header)
    let (rfirst, mut rules) = if rules_on() { block_rules(rid, rnv, vals, tk.as_ref(), &final_of, &pidx, &plan, &col_of, &tcount, &ccount) } else { (vec![0; col_of.len() + 1], Vec::new()) };
    let mut flag_list: Vec<u32> = Vec::new(); // per value at a rule's slot: rule << 1 | (not given)
    let mut near_list: Vec<i64> = Vec::new(); // the differences of the near rules that gave values
    let mut xs = vec![rules::NOT_NUM; col_of.iter().map(|c| c.len()).max().unwrap_or(0)]; // the line's values so far, as rule sources
    let mut rminus = vec![0u32; if rules.is_empty() { 0 } else { prefix_cols }]; // values the rules gave, per prefix column
    // the Plan's flags (M6): planned words, splitting columns, rules, tokens
    plan_flags(&mut pl6, &final_of, &pidx, &plan, &cols, &rfirst, &rules, tk.as_ref());
    pt(49, &mut t_a);
    // keyed slots (entity.rs), chosen on a sample of lines (not with the old pass 2, nor in a
    // block whose fixed text has a piece the fast opener does not take)
    let long_piece = tmpl_out.len() >= u32::MAX as usize || tmpl_out.split(|&c| c == MARK || c == b'\n').any(|x| x.len() >= u16::MAX as usize);
    let mut kp = if keyed_on() && !env_on("FL_OLDPASS2") && !long_piece {
        let mut sbase = slot_base.clone();
        sbase.push(nslots);
        let slot_col: Vec<u32> = col_of.iter().flatten().copied().collect();
        let smp = entity_sample(rid, rnv, vals, tk.as_ref(), &final_of, &pidx, &plan, &col_of, &rfirst, &rules);
        let ep = entity::plan(&smp, &sbase, &slot_col, cols.len(), lines.len());
        if env_on("FL_KDUMP") {
            eprintln!("KPLAN lines {} keyed slots {}", lines.len(), ep.dist.iter().filter(|&&d| d != 0).count());
        }
        if ep.any() { Some(front::KeyedPass::new(ep, &slot_base, nslots)) } else { None }
    } else {
        None
    };
    pt(50, &mut t_a);
    // FL_SHADOW (harness): the frozen interfaces' stand-ins against today's data, every block
    let sh_blk = if shadow::any() { shadow_block(&p1, &col_of, &slot_base, &final_of, &pidx, &plan, &cols, &rfirst, &rules) } else { 0 };
    if shadow::any() {
        shadow_m6(sh_blk, &p1, lines, tk.as_ref(), &pidx, &plan, &wid, &words_out, &tmpl_out, &col_of, &slot_base, &final_of, &cols, &rfirst, &rules, &pl6);
    }
    // pass 2: template ids, values into their columns
    let mut tids: Vec<u8> = Vec::with_capacity(lines.len() * 2);
    let mut tid_list: Vec<u32> = Vec::new(); // TidModel: the ids, coded after the block
    let mut kind_last = [0u64; 256]; // the last number of each kind class
    let mut id_map = IdMap::default(); // long ID -> its place in the list; ID values (ids.rs)
    // after a word of a splitting column, the line's later slots use a column per word (made
    // on first use), and each (column, pattern slot) of those its own slot number
    let mut sub: FxMap<(u32, u64), u32> = FxMap::default();
    let mut sub_slot: FxMap<(u32, u32), u32> = FxMap::default();
    let mut clock_vals: Vec<(u32, u32, u64, i64)> = Vec::new(); // grouped: (line, column, value, micros)
    let mut per_tmpl = vec![0u32; col_of.len()]; // lines of each pattern
    // FL_SHADOW=pass2 (harness): the old loop runs on a copy of everything pass 2 writes, and
    // every output is compared below (shadow::any() is a constant "off" in the product)
    let sh_p2 = if shadow::any() && shadow::on("pass2") { Some((cols.clone(), wmap.clone(), words_out.clone(), flag_list.clone(), near_list.clone(), xs.clone(), rminus.clone())) } else { None };
    if env_on("FL_OLDPASS2") {
        // FL_OLDPASS2=1: the old loop (the reference of FL_SHADOW=pass2)
        pass2_old(lines, rid, rnv, vals, tk.as_ref(), &final_of, &col_of, &slot_base, &pidx, &plan, &wid, nslots, &rfirst, &rules, use_ks, want_times, tidm, &mut cols, &mut tids, &mut tid_list, &mut kind_last, &mut id_map, &mut sub, &mut sub_slot, &mut clock_vals, &mut per_tmpl, &mut wmap, &mut words_out, &mut flag_list, &mut near_list, &mut xs, &mut rminus);
    } else {
        // the plan-driven pass 2 (front.rs: write cursors into reserved column buffers); it reads
        // the column and slot number of each template slot from M6's Plan (pl6), and takes words,
        // tokens and rules from their own lists
        front::pass2_tok(lines, rid, rnv, &cnt1, vals, &final_of, &col_of, &pl6, &pidx, &plan, &wid, nslots, &mut cols, tidm, use_ks, want_times, &mut tids, &mut tid_list, &mut kind_last, &mut id_map, &mut sub, &mut sub_slot, &mut clock_vals, tk.as_ref(), &mut wmap, &mut words_out, &mut per_tmpl, &front::RuleLists { rfirst: &rfirst, rules: &rules, flag_list: std::cell::RefCell::new(&mut flag_list), near_list: std::cell::RefCell::new(&mut near_list), xs: std::cell::RefCell::new(&mut xs), rminus: std::cell::RefCell::new(&mut rminus) }, kp.as_mut());
    }
    if let Some((mut o_cols, mut o_wmap, mut o_words, mut o_flags, mut o_near, mut o_xs, mut o_rminus)) = sh_p2 {
        let (mut o_tids, mut o_tid_list, mut o_kl, mut o_ids, mut o_sub, mut o_subs, mut o_clock, mut o_pt) = (Vec::new(), Vec::new(), [0u64; 256], IdMap::default(), FxMap::default(), FxMap::default(), Vec::new(), vec![0u32; col_of.len()]);
        pass2_old(lines, rid, rnv, vals, tk.as_ref(), &final_of, &col_of, &slot_base, &pidx, &plan, &wid, nslots, &rfirst, &rules, use_ks, want_times, tidm, &mut o_cols, &mut o_tids, &mut o_tid_list, &mut o_kl, &mut o_ids, &mut o_sub, &mut o_subs, &mut o_clock, &mut o_pt, &mut o_wmap, &mut o_words, &mut o_flags, &mut o_near, &mut o_xs, &mut o_rminus);
        let b = sh_blk;
        let st = "pass2";
        shadow::same(st, b, "column count", &[o_cols.len()], &[cols.len()]);
        for (ci, (o, n)) in o_cols.iter().zip(&cols).enumerate() {
            shadow::same(st, b, &format!("column {} kinds", ci), &o.kinds, &n.kinds);
            shadow::same(st, b, &format!("column {} values", ci), &o.vals, &n.vals);
            shadow::same(st, b, &format!("column {} slots", ci), &o.slots, &n.slots);
            shadow::same(st, b, &format!("column {} texts", ci), &o.txt, &n.txt);
            shadow::same(st, b, &format!("column {} recent texts", ci), &o.recent, &n.recent);
            shadow::same(st, b, &format!("column {} long-ID places", ci), &o.ids, &n.ids);
            shadow::same(st, b, &format!("column {} recent words, split", ci), &[(o.wrecent.w, o.wrecent.n, o.split)], &[(n.wrecent.w, n.wrecent.n, n.split)]);
        }
        shadow::same(st, b, "tids", &o_tids, &tids);
        shadow::same(st, b, "tid_list", &o_tid_list, &tid_list);
        shadow::same(st, b, "kind_last", &o_kl, &kind_last);
        let sorted = |m: &FxMap<u64, u32>| -> Vec<(u64, u32)> { let mut v: Vec<(u64, u32)> = m.iter().map(|(&a, &b)| (a, b)).collect(); v.sort_unstable(); v };
        shadow::same(st, b, "id_map", &sorted(&o_ids.map), &sorted(&id_map.map));
        let mut s1: Vec<((u32, u64), u32)> = o_sub.iter().map(|(&a, &b)| (a, b)).collect();
        let mut s2: Vec<((u32, u64), u32)> = sub.iter().map(|(&a, &b)| (a, b)).collect();
        s1.sort_unstable();
        s2.sort_unstable();
        shadow::same(st, b, "sub", &s1, &s2);
        let mut s1: Vec<((u32, u32), u32)> = o_subs.iter().map(|(&a, &b)| (a, b)).collect();
        let mut s2: Vec<((u32, u32), u32)> = sub_slot.iter().map(|(&a, &b)| (a, b)).collect();
        s1.sort_unstable();
        s2.sort_unstable();
        shadow::same(st, b, "sub_slot", &s1, &s2);
        shadow::same(st, b, "clock_vals", &o_clock, &clock_vals);
        shadow::same(st, b, "per_tmpl", &o_pt, &per_tmpl);
        let mut s1: Vec<(&[u8], u32)> = o_wmap.iter().map(|(&a, &b)| (a, b)).collect();
        let mut s2: Vec<(&[u8], u32)> = wmap.iter().map(|(&a, &b)| (a, b)).collect();
        s1.sort_unstable();
        s2.sort_unstable();
        shadow::same(st, b, "word table", &s1, &s2);
        shadow::same(st, b, "words_out", &o_words, &words_out);
        shadow::same(st, b, "flag_list", &o_flags, &flag_list);
        shadow::same(st, b, "near_list", &o_near, &near_list);
        // (xs is pass 2's scratch for the rules' sources, not an output: the old loop also keeps
        // the values of other slots there, so it is not compared)
        let _ = &o_xs;
        shadow::same(st, b, "rminus", &o_rminus, &rminus);
    }
    pt(4, &mut t_a);
    let (mut tcol, mut times) = (None, Vec::new());
    if want_times {
        // the clock column: most clock times + most changes (a file name's time rarely
        // changes); only a column in at least half the lines can be the lines' clock
        let mut score = vec![(0usize, u64::MAX); prefix_cols];
        for &(_, ci, v, _) in &clock_vals {
            if (ci as usize) < prefix_cols {
                let e = &mut score[ci as usize];
                e.0 += 1 + (v != e.1) as usize;
                e.1 = v;
            }
        }
        let mut best = 0usize;
        for (ci, e) in score.iter().enumerate() {
            if cols[ci].kinds.len() * 2 >= lines.len() && e.0 > best {
                best = e.0;
                tcol = Some(ci as u32);
            }
        }
        times = vec![group::NO_TIME; lines.len()];
        if let Some(t) = tcol {
            for &(li, ci, _, x) in &clock_vals {
                if ci == t && times[li as usize] == group::NO_TIME {
                    times[li as usize] = x;
                }
            }
        }
    }
    let nslots = nslots + sub_slot.len() as u32;
    // ID columns (ids.rs): their new long IDs as generator steps, their UUIDs as numbers (the
    // new UUIDs' bytes end the text stream)
    let id_plan = if ids_on() && !id_map.seq.is_empty() { id_plan(&cols, &id_map, lines.len()) } else { None };
    if env_on("FL_RDUMP") && !rules.is_empty() {
        // the rules, their patterns and how often each gave the value
        let mut hits = vec![(0u32, 0u32); rules.len()];
        for &f in &flag_list {
            let e = &mut hits[(f >> 1) as usize];
            e.0 += (f & 1 == 0) as u32;
            e.1 += 1;
        }
        let tl: Vec<&[u8]> = tmpl_out.split(|&c| c == b'\n').collect();
        for t in 0..col_of.len() {
            for ri in rfirst[t] as usize..rfirst[t + 1] as usize {
                eprintln!("RULE {:?} hits {}/{} | {}", rules[ri], hits[ri].0, hits[ri].1, String::from_utf8_lossy(&tl[t][..tl[t].len().min(160)]).replace('\0', "@"));
            }
        }
    }
    pt(5, &mut t_a);
    if env_on("FL_WDUMP") {
        eprintln!("WORDS {} split {}: {}", wmap.len(), cols.iter().filter(|c| c.split).count(), String::from_utf8_lossy(&words_out[..words_out.len().min(600)]).replace('\n', " | "));
        for r in 0..m.min(40) {
            if pidx[r] < pidx[r + 1] {
                eprintln!("  shape {}: {}", r, String::from_utf8_lossy(&rbuf[roff[r] as usize..roff[r + 1] as usize - 1]).replace('\0', "@"));
            }
        }
    }
    let mut hdr = Vec::new();
    let split_nums = split_nums_on();
    put_varint(&mut hdr, cols.len() as u64);
    let ng_at = hdr.len();
    hdr.push(0); // how many grouped columns (set below; one byte while below 128)
    // value counts the opener works out from the pattern ids (heads.rs); the splitting columns
    // are listed first, as the counts of the slots after theirs are stored
    let flat: Vec<u32> = col_of.iter().flatten().copied().collect();
    let mut first: Vec<u32> = Vec::with_capacity(col_of.len() + 1);
    first.push(0);
    for ids in &col_of {
        first.push(first[first.len() - 1] + ids.len() as u32);
    }
    let split_of: Vec<bool> = cols[..prefix_cols].iter().map(|c| c.split).collect();
    let stored = heads::stored_counts(&flat, &first, &split_of);
    let derived = heads::derived_counts(&flat, &first, &per_tmpl, prefix_cols);
    // (a count the rule gets wrong would be a bug: then every column stores its count)
    let all_counts = (0..prefix_cols).any(|c| !stored[c] && derived[c] - rminus.get(c).map_or(0, |&x| x as usize) != cols[c].kinds.len());
    let nsplit = split_of.iter().filter(|&&x| x).count();
    // flags: 1 = pattern ids coded with TidModel, 2 = splitting columns listed, 4 = all counts,
    // 8 = no kind specials, 16 = rules (their section follows the splitting columns), 32 = ID
    // columns (ids.rs), 64 = keyed slots (entity.rs; their plan follows the rules section)
    let flags_at = hdr.len();
    hdr.push(tidm as u8 | ((nsplit > 0) as u8) << 1 | (all_counts as u8) << 2 | (!use_ks as u8) << 3 | (!rules.is_empty() as u8) << 4);
    if nsplit > 0 {
        put_varint(&mut hdr, nsplit as u64);
        let mut prev = 0;
        for (c, _) in split_of.iter().enumerate().filter(|x| *x.1) {
            put_varint(&mut hdr, (c - prev) as u64);
            prev = c;
        }
    }
    let mut answers = Vec::new();
    if !rules.is_empty() {
        mark_always(&mut rules, &flag_list);
        rules::put_rules(&mut hdr, &rfirst, &rules);
        let mut enc = ozip::RcEnc::new();
        code_answers(&mut enc, &rules, &flag_list, &near_list);
        answers = enc.finish();
        put_varint(&mut hdr, answers.len() as u64);
    }
    pt(6, &mut t_a);
    // the columns: each coded on its own (put_nums, grouping) - side by side when the block has
    // spare threads - then put in the streams and the header in column order (col_phase)
    if tidm {
        tids = code_tids(&tid_list, col_of.len());
    }
    let group_on = !env_on("FL_NOGROUP");
    let gfull = env_on("FL_GFULL");
    let fixw = crate::engine::fl::no_env::var_os("FL_NOFIXW").is_none();
    let cand = cols.iter().enumerate().filter(|(ci, c)| c.vals.len() >= GROUP_MIN && *ci < prefix_cols).count();
    // (the slots of each column, for the grouping estimate: one pass over the patterns)
    let (mut cslot_at, mut cslots) = (Vec::new(), Vec::new());
    if cand > 0 && group_on && !any_split {
        cslot_at = vec![0u32; prefix_cols + 1];
        for ids in &col_of {
            for &id in ids {
                cslot_at[id as usize + 1] += 1;
            }
        }
        for c in 0..prefix_cols {
            cslot_at[c + 1] += cslot_at[c];
        }
        let mut at = cslot_at.clone();
        cslots = vec![0u32; cslot_at[prefix_cols] as usize];
        for (t, ids) in col_of.iter().enumerate() {
            for (k, &id) in ids.iter().enumerate() {
                cslots[at[id as usize] as usize] = slot_base[t] + k as u32;
                at[id as usize] += 1;
            }
        }
    }
    let ctx = ColCtx { cslot_at, cslots, prefix_cols, any_split, group_on, gfull, fixw, nslots: nslots as usize };
    // keyed slots (entity.rs) and ID columns (ids.rs): each kept only where it makes the block
    // smaller by real bytes (both_ways); a block with neither: the column phase as before
    let mut pre = Vec::new();
    let ([kinds, nums, txt, codes, nums2], hdr) = if kp.is_none() && id_plan.is_none() {
        (col_phase(&mut cols, &ctx, threads, split_nums, all_counts, &stored, &mut hdr, ng_at, &answers, None, sh_blk), hdr)
    } else {
        both_ways(&mut cols, &ctx, threads, split_nums, all_counts, &stored, hdr, flags_at, ng_at, &answers, kp.as_ref(), id_plan, &id_map, use_ks, lines.len(), &mut pre)
    };
    pt(7, &mut t_a);
    pt(8, &mut t_a);
    let alt = if tidm { Vec::new() } else { id_forms(&tids, lines.len(), col_of.len()) };
    // B5: the derive markers in the patterns' fixed text, line by line (the packer's check
    // that no marker went into a value)
    let (mut mk, mut t, mut c) = (0u64, 0usize, 0u64);
    for &b in &tmpl_out {
        if b == derive::MK {
            c += 1;
        } else if b == b'\n' {
            mk += c * per_tmpl.get(t).map_or(0, |&n| n as u64);
            (t, c) = (t + 1, 0);
        }
    }
    (Packed { streams: vec![tmpl_out, tids, hdr, kinds, nums, txt, codes, nums2, words_out], lines: lines.len(), raw: if tidm { 2 } else { 0 }, alt, pre, mk }, tcol, times)
}

/// The column phase of pack_block: each column coded (put_nums, grouping; side by side when
/// the block has spare threads), then the streams (kinds, numbers, texts, cache codes,
/// differences) and the header's parts in column order; `tail`: the ID columns' new UUIDs
/// (ids.rs), after the texts.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn col_phase(cols: &mut [Col], ctx: &ColCtx, threads: usize, split_nums: bool, all_counts: bool, stored: &[bool], hdr: &mut Vec<u8>, ng_at: usize, answers: &[u8], tail: Option<&[u8]>, sh_blk: u64) -> [Vec<u8>; 5] {
    let (kinds, mut nums, txt, mut codes, mut nums2) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let prefix_cols = ctx.prefix_cols;
    let nc = cols.len();
    // (more than 126 columns that could be grouped: one at a time, with the cap on the count)
    let cand = cols.iter().enumerate().filter(|(ci, c)| c.vals.len() >= GROUP_MIN && *ci < prefix_cols).count();
    // (each thread's per-slot working space is about 21 bytes per slot: at most ~256 MB in all)
    let col_threads = threads.min(nc).min(((256usize << 20) / (21 * ctx.nslots.max(1))).max(1));
    let par = col_threads > 1 && cand < 127;
    let mut outs: Vec<ColOut> = Vec::with_capacity(nc);
    let sh_cc = shadow::any() && shadow::on("code_col");
    if par {
        let mut order: Vec<usize> = (0..nc).collect();
        sort_stable_idx(&mut order, &mut |&a, &b| cols[a].vals.len() + cols[a].kinds.len() > cols[b].vals.len() + cols[b].kinds.len());
        let slots: Vec<std::sync::Mutex<(Option<&mut Col>, Option<ColOut>)>> = cols.iter_mut().map(|c| std::sync::Mutex::new((Some(c), None))).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for _ in 0..col_threads {
                sc.spawn(|| {
                    let mut sc = ColScratch::new(ctx.nslots);
                    // (shadow::any() again inside the thread: a constant "off" in the product)
                    let mut shs = if shadow::any() && sh_cc { Some((ColScratch::new(ctx.nslots), ColScratch::new(ctx.nslots))) } else { None };
                    loop {
                        let q = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if q >= nc {
                            break;
                        }
                        let ci = order[q];
                        let mut g = slots[ci].lock().unwrap();
                        let c = g.0.take().unwrap();
                        if shadow::any() {
                            if let Some(s) = shs.as_mut() {
                                // (an ID column, ids.rs: the old coder has no step forms)
                                if !c.idc {
                                    shadow_code_col(ctx, ci, c, true, sh_blk, s);
                                }
                            }
                        }
                        let o = code_col_own(ctx, ci, c, true, &mut sc);
                        g.1 = Some(o);
                    }
                });
            }
        });
        outs.extend(slots.into_iter().map(|m| m.into_inner().unwrap().1.unwrap()));
    } else {
        let mut sc = ColScratch::new(ctx.nslots);
        let mut ng = 0usize;
        // (one thread: the columns in order straight into the streams; the header loop below
        // then adds no numbers or codes, as each ColOut has none of its own)
        let mut sink = Sink { a: &mut nums, b: if split_nums { Some(&mut nums2) } else { None }, codes: &mut codes };
        let mut shs = if shadow::any() && sh_cc { Some((ColScratch::new(ctx.nslots), ColScratch::new(ctx.nslots))) } else { None };
        for (ci, c) in cols.iter_mut().enumerate() {
            if shadow::any() {
                if let Some(s) = shs.as_mut() {
                    if !c.idc {
                        shadow_code_col(ctx, ci, c, ng < 127, sh_blk, s);
                    }
                }
            }
            let o = code_col(ctx, ci, c, ng < 127, &mut sc, &mut sink);
            ng += o.grouped as usize;
            outs.push(o);
        }
    }
    let outs: Vec<&ColOut> = outs.iter().collect();
    let cr: Vec<&Col> = cols.iter().collect();
    assemble(&cr, &outs, [kinds, nums, txt, codes, nums2], prefix_cols, split_nums, all_counts, stored, hdr, ng_at, answers, tail)
}

/// The column phase of a block with keyed slots (entity.rs) or planned ID columns (ids.rs).
/// 1. Every column coded on its own (code_col_own: the same bytes as col_phase; side by side
///    when the block has spare threads; with 127 or more columns that could be grouped, one at
///    a time in order with the cap on the count).
/// 2. Keyed codes (entity.rs): the keyed columns are coded again without them, and the streams
///    they change (header, numbers, codes, differences) are coded both ways; keyed codes are
///    kept only when they save at least half a bit per memory lookup (a tie: without). The
///    winner's frames go with the block (`pre`), so code_streams does not code them again.
/// 3. ID columns (ids.rs; never a keyed column): coded the ID way (their columns as pass 2
///    left them, the plan applied; a fresh scratch, as when opening), and kept when the
///    streams that differ code smaller (real bytes; the estimate alone made some logs bigger).
/// Returns the streams (kinds, numbers, texts, cache codes, differences) and the header.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn both_ways(cols: &mut [Col], ctx: &ColCtx, threads: usize, split_nums: bool, all_counts: bool, stored: &[bool], mut hdr: Vec<u8>, flags_at: usize, ng_at: usize, answers: &[u8], kp: Option<&front::KeyedPass>, id_plan: Option<IdPlan>, id_map: &IdMap, use_ks: bool, lines: usize, pre: &mut Vec<(usize, u64, Vec<u8>, Option<fz::Prep>)>) -> ([Vec<u8>; 5], Vec<u8>) {
    let n = cols.len();
    let prefix_cols = ctx.prefix_cols;
    // (the planned ID columns as pass 2 left them: the ID way starts from these)
    let mut pre_id: Vec<Option<Col>> = match &id_plan {
        Some(p) => cols.iter().zip(&p.idc).map(|(c, &on)| on.then(|| c.copy())).collect(),
        None => Vec::new(),
    };
    // 1. every column on its own
    let cand = cols.iter().enumerate().filter(|(ci, c)| c.vals.len() >= GROUP_MIN && *ci < prefix_cols).count();
    let col_threads = threads.min(n).min(((256usize << 20) / (21 * ctx.nslots.max(1))).max(1));
    let mut outs: Vec<ColOut> = Vec::with_capacity(n);
    if col_threads > 1 && cand < 127 {
        let mut order: Vec<usize> = (0..n).collect();
        // (the job order only: each column's output goes to its own slot; c8's small sort)
        sort_stable_idx(&mut order, &mut |&a, &b| cols[a].vals.len() + cols[a].kinds.len() > cols[b].vals.len() + cols[b].kinds.len());
        let slots: Vec<std::sync::Mutex<(Option<&mut Col>, Option<ColOut>)>> = cols.iter_mut().map(|c| std::sync::Mutex::new((Some(c), None))).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for _ in 0..col_threads {
                sc.spawn(|| {
                    let mut sc = ColScratch::new(ctx.nslots);
                    loop {
                        let q = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if q >= n {
                            break;
                        }
                        let ci = order[q];
                        let mut g = slots[ci].lock().unwrap();
                        let c = g.0.take().unwrap();
                        g.1 = Some(code_col_own(ctx, ci, c, true, &mut sc));
                    }
                });
            }
        });
        outs.extend(slots.into_iter().map(|m| m.into_inner().unwrap().1.unwrap()));
    } else {
        let mut sc = ColScratch::new(ctx.nslots);
        let mut ng = 0usize;
        for (ci, c) in cols.iter_mut().enumerate() {
            let o = code_col_own(ctx, ci, c, ng < 127, &mut sc);
            ng += o.grouped as usize;
            outs.push(o);
        }
    }
    let none = || -> [Vec<u8>; 5] { Default::default() };
    // 2. keyed codes only where they make the block smaller
    if let Some(k) = kp.filter(|_| outs.iter().any(|o| o.nm & 256 != 0)) {
        let mut sc = ColScratch::new(ctx.nslots);
        let mut alt: Vec<(usize, ColOut)> = Vec::new();
        for (ci, o) in outs.iter().enumerate() {
            if o.nm & 256 != 0 {
                let c = &mut cols[ci];
                let ke = std::mem::take(&mut c.ke);
                alt.push((ci, code_col_own(ctx, ci, c, true, &mut sc)));
                c.ke = ke;
            }
        }
        let mut plan = Vec::new();
        k.plan.write(&mut plan);
        // a variant's streams 2, 4, 6 and 7 as the block will have them, and their frames
        let frames = |cols: &[Col], outs: &[ColOut], keyed: bool| -> (usize, Vec<(usize, u64, Vec<u8>, Option<fz::Prep>)>) {
            let mut h = hdr.clone();
            if keyed {
                h[flags_at] |= entity::FLAG;
                h.extend_from_slice(&plan);
            }
            let cr: Vec<&Col> = cols.iter().collect();
            let or: Vec<&ColOut> = outs.iter().collect();
            let [_, nums, _, codes, nums2] = assemble(&cr, &or, none(), prefix_cols, split_nums, all_counts, stored, &mut h, ng_at, answers, None);
            let f: Vec<(usize, u64, Vec<u8>, Option<fz::Prep>)> = [(2, &h), (4, &nums), (6, &codes), (7, &nums2)].into_iter().map(|(k, s)| {
                let (fr, pr) = frame(k == 2, s);
                (k, hash_bytes(s), fr, pr)
            }).collect();
            // (an empty stream is no frame at all in the block, as in the container)
            (f.iter().zip([&h, &nums, &codes, &nums2]).map(|(x, s)| if s.is_empty() { 0 } else { x.2.len() }).sum(), f)
        };
        let (with, fk) = frames(cols, &outs, true);
        for (ci, o) in &mut alt {
            std::mem::swap(&mut outs[*ci], o);
        }
        let (without, fp) = frames(cols, &outs, false);
        if env_on("FL_KDUMP") {
            eprintln!("KEYED lines {} keyed slots {} lookups {} columns {} bytes {} -> {} ({:.3} bits per lookup)", lines, k.plan.dist.iter().filter(|&&d| d != 0).count(), k.looks, alt.len(), without, with, (without as f64 - with as f64) * 8.0 / k.looks.max(1) as f64);
        }
        // (and by at least half a bit per memory lookup: both sides pay for every lookup)
        if with < without && (without - with) * 16 >= k.looks {
            for (ci, o) in &mut alt {
                std::mem::swap(&mut outs[*ci], o);
            }
            hdr[flags_at] |= entity::FLAG;
            hdr.extend_from_slice(&plan);
            *pre = fk;
        } else {
            // (no keyed codes: a column coded again below must not take them either)
            for (ci, _) in &alt {
                cols[*ci].ke.clear();
            }
            *pre = fp;
        }
    }
    // 3. ID columns only where they make the block smaller (never a keyed column)
    let plan = id_plan.map(|mut p| {
        for (on, o) in p.idc.iter_mut().zip(&outs) {
            *on &= o.nm & 256 == 0;
        }
        p
    });
    let Some(plan) = plan.filter(|p| p.idc.iter().any(|&x| x)) else {
        let cr: Vec<&Col> = cols.iter().collect();
        let or: Vec<&ColOut> = outs.iter().collect();
        let st = assemble(&cr, &or, none(), prefix_cols, split_nums, all_counts, stored, &mut hdr, ng_at, answers, None);
        return (st, hdr);
    };
    // the ID columns as pass 2 left them, the plan applied (the other entries: placeholders)
    let mut vb: Vec<Col> = (0..n).map(|ci| if plan.idc[ci] { pre_id[ci].take().unwrap() } else { Col::default() }).collect();
    let tail = id_apply(&mut vb, id_map, &plan, use_ks);
    for (c, &on) in vb.iter_mut().zip(&plan.idc) {
        if on {
            c.ke.clear(); // (an ID column has no keyed codes: its places change)
        }
    }
    let ob: Vec<Option<ColOut>> = if cand >= 127 {
        // (the cap on grouped columns depends on the order: every column again, in order)
        let mut sc = ColScratch::new(ctx.nslots);
        let mut ng = 0usize;
        (0..n).map(|ci| {
            let c = if plan.idc[ci] { &mut vb[ci] } else { &mut cols[ci] };
            let o = code_col_own(ctx, ci, c, ng < 127, &mut sc);
            ng += o.grouped as usize;
            Some(o)
        }).collect()
    } else {
        // the ID columns the ID way (a fresh scratch: their slots start from nothing again, as
        // when opening)
        let mut sc = ColScratch::new(ctx.nslots);
        (0..n).map(|ci| plan.idc[ci].then(|| code_col_own(ctx, ci, &mut vb[ci], true, &mut sc))).collect()
    };
    let ca: Vec<&Col> = cols.iter().collect();
    let cb: Vec<&Col> = (0..n).map(|ci| if plan.idc[ci] { &vb[ci] } else { &cols[ci] }).collect();
    let ra: Vec<&ColOut> = outs.iter().collect();
    let rb: Vec<&ColOut> = ob.iter().zip(&outs).map(|(b, a)| b.as_ref().unwrap_or(a)).collect();
    let mut hb = hdr.clone();
    hb[flags_at] |= 32;
    let sa = assemble(&ca, &ra, none(), prefix_cols, split_nums, all_counts, stored, &mut hdr, ng_at, answers, None);
    let sb = assemble(&cb, &rb, none(), prefix_cols, split_nums, all_counts, stored, &mut hb, ng_at, answers, Some(&tail));
    // the streams that differ, coded both ways (as code_streams codes them: coded_size's
    // sizes; an empty stream is no frame at all in the block, as in the container); the
    // winner's frames go with the block (`pre`), so they are not coded again
    let (fa, fb) = (frame(true, &hdr), frame(true, &hb));
    let (mut za, mut zb) = (fa.0.len(), fb.0.len());
    let (mut pa, mut pb) = (vec![(2, hash_bytes(&hdr), fa.0, fa.1)], vec![(2, hash_bytes(&hb), fb.0, fb.1)]);
    for k in 0..5 {
        if sa[k] != sb[k] {
            let (x, y) = (frame(false, &sa[k]), frame(false, &sb[k]));
            za += if sa[k].is_empty() { 0 } else { x.0.len() };
            zb += if sb[k].is_empty() { 0 } else { y.0.len() };
            pa.push((k + 3, hash_bytes(&sa[k]), x.0, x.1));
            pb.push((k + 3, hash_bytes(&sb[k]), y.0, y.1));
        }
    }
    let won = zb < za;
    // (first: they are looked up in order, and they hold the block's final streams)
    pre.splice(0..0, if won { pb } else { pa });
    if won { (sb, hb) } else { (sa, hdr) }
}

/// The streams (kinds, numbers, texts, cache codes, differences; numbers and codes already
/// written by col_phase's one-thread way come in `st`) and the header's parts of coded columns,
/// in column order; `tail`: the ID columns' new UUIDs (ids.rs), after the texts.
#[allow(clippy::too_many_arguments)]
fn assemble(cols: &[&Col], outs: &[&ColOut], st: [Vec<u8>; 5], prefix_cols: usize, split_nums: bool, all_counts: bool, stored: &[bool], hdr: &mut Vec<u8>, ng_at: usize, answers: &[u8], tail: Option<&[u8]>) -> [Vec<u8>; 5] {
    let [mut kinds, mut nums, mut txt, mut codes, mut nums2] = st;
    // the header's parts, one after the other (heads.rs): stored counts, kinds of one-value
    // columns, modes of small and of other columns, kinds, group bytes
    let mut part: [Vec<u8>; 6] = Default::default();
    let mut ng = 0usize;
    for (ci, (&c, &o)) in cols.iter().zip(outs).enumerate() {
        let (nm, grouped) = (o.nm, o.grouped);
        // kinds: left out when every value has the same kind
        let same = c.kinds.is_empty() || all_eq(&c.kinds, c.kinds[0]);
        let mode = nm | ((same as u64) << 1) | ((grouped as u64) << 4) | ((c.split as u64) << 6) | ((c.idc as u64) << 7);
        ng += grouped as usize;
        // the column's entry (see heads.rs)
        let cnt = c.kinds.len();
        if all_counts || ci >= prefix_cols || stored[ci] {
            put_varint(&mut part[0], cnt as u64);
        }
        if !same {
            kinds.extend_from_slice(&c.kinds);
        }
        if cnt == 1 && mode == 2 {
            part[1].push(c.kinds[0]); // one value, a number as is: only its kind
        } else if cnt > 0 {
            if cnt == 1 {
                part[1].push(heads::FULL);
            }
            put_varint(&mut part[if cnt < SMALL_COL { 2 } else { 3 }], mode);
            if same {
                part[4].push(c.kinds[0]);
            }
        }
        if grouped {
            // the group of each slot (in pattern order)
            part[5].extend_from_slice(&o.groups);
        }
        codes.extend_from_slice(&o.codes);
        // numbers written as differences get their own stream (their own statistics)
        if nm & 33 != 0 && split_nums {
            nums2.extend_from_slice(&o.tmp);
        } else {
            nums.extend_from_slice(&o.tmp);
        }
        txt.extend_from_slice(&c.txt);
    }
    if let Some(t) = tail {
        txt.extend_from_slice(t); // the ID columns' new UUIDs (ids.rs)
    }
    hdr[ng_at] = ng as u8;
    for x in &part {
        hdr.extend_from_slice(x);
    }
    hdr.extend_from_slice(answers); // the rules' answers (their length is in the rules section)
    [kinds, nums, txt, codes, nums2]
}

/// FL_NOIDS2=1: no ID columns (ids.rs).
fn ids_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NOIDS2"))
}

/// The ID columns of a block (ids.rs), from an estimate of each column's own bytes both
/// ways (in 1/8 bytes): the old way takes a new long ID's value, 16 bytes per new UUID and
/// about 4 per repeat (which the coder finds in the texts); the ID way a new long ID's step
/// form and a UUID's number (with its 16 bytes or 122 bits when new). A column becomes one
/// when it saves 4 bytes and 1/32 of its bytes; the block takes them when they save a byte per
/// 64 lines (else not: too little for the time). pack_block then codes the block both ways.
struct IdPlan {
    idc: Vec<bool>,
    forms: Vec<u64>, // per value of the IdMap: a new long ID's step form, a UUID's first number
    nu: usize,       // different UUIDs
}

fn id_plan(cols: &[Col], m: &IdMap, lines: usize) -> Option<IdPlan> {
    let nc = cols.len();
    let (mut old, mut new) = (vec![0u64; nc], vec![0u64; nc]);
    let mut gw: Vec<ids::GenW> = (0..nc).map(|_| ids::GenW::default()).collect();
    let mut forms: Vec<u64> = Vec::with_capacity(m.seq.len());
    let mut seen = ids::Unum::default();
    for x in &m.seq {
        let c = x.col as usize;
        let mut f = 0;
        if x.kind == LONG_ID {
            if cols[c].ids[x.pos as usize] == 0 {
                let v = x.v as u64;
                f = gw[c].form(v).unwrap_or(u64::MAX);
                old[c] += 8 * varint_len(v) as u64;
                new[c] += 8 * varint_len(if f == u64::MAX { v } else { f }) as u64;
            }
        } else {
            let n = seen.len() as u32;
            f = seen.get(x.v) as u64;
            let (o, w) = if f != n as u64 { (32, 16) } else if ids::uuid_v4(x.v) { (128, 124) } else { (128, 130) };
            old[c] += o;
            new[c] += w;
        }
        forms.push(f);
    }
    let idc: Vec<bool> = (0..nc).map(|c| new[c] + 32 <= old[c] && (old[c] - new[c]) * 32 >= old[c]).collect();
    let (o, n) = (0..nc).filter(|&c| idc[c]).fold((0, 0), |(o, n), c| (o + old[c], n + new[c]));
    if o == 0 || (o - n) / 8 < (lines / 64).max(4) as u64 {
        return None;
    }
    Some(IdPlan { idc, forms, nu: seen.len() })
}

/// The ID columns of the plan get their step forms and their UUIDs as numbers (0 / 1: a new
/// one, i + 2: dictionary entry i; kinds stay 100 / 101); returns the new UUIDs' bytes (the
/// text stream's end, ids.rs Utail).
fn id_apply(cols: &mut [Col], m: &IdMap, plan: &IdPlan, use_ks: bool) -> Vec<u8> {
    let idc = &plan.idc;
    let mut dict = vec![u32::MAX; plan.nu]; // a UUID's first number -> its place
    let mut nd = 0u32;
    let mut tail = ids::Utail::default();
    let mut last = [0u64; 2]; // the last number of kinds 100 and 101 (the opener's kind_last)
    // per column: its UUIDs' (numbers before it, number, slot with its "same as the kind's
    // last" bit), and their places in its texts
    let mut ins: Vec<Vec<(u32, u64, u32)>> = vec![Vec::new(); cols.len()];
    let mut gone: Vec<Vec<u32>> = vec![Vec::new(); cols.len()];
    for (x, &f) in m.seq.iter().zip(&plan.forms) {
        let c = x.col as usize;
        if !idc[c] {
            continue;
        }
        if x.kind == LONG_ID {
            if cols[c].ids[x.pos as usize] == 0 {
                cols[c].steps.push(f);
            }
        } else {
            let e = &mut dict[f as usize];
            let num = if *e == u32::MAX {
                *e = nd;
                nd += 1;
                tail.push(x.v)
            } else {
                *e as u64 + 2
            };
            let k = (x.kind - 100) as usize;
            let ks = use_ks && last[k] == num;
            last[k] = num;
            ins[c].push((x.npos, num, x.sl | (ks as u32) << 31));
            gone[c].push(x.pos);
        }
    }
    for (c, &on) in idc.iter().enumerate() {
        cols[c].idc = on;
    }
    for ((col, add), g) in cols.iter_mut().zip(&ins).zip(&gone) {
        if add.is_empty() {
            continue;
        }
        let n = col.vals.len() + add.len();
        let has_ids = !col.ids.is_empty();
        let (mut v2, mut s2) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let mut i2 = Vec::with_capacity(if has_ids { n } else { 0 });
        let mut q = 0;
        for i in 0..=col.vals.len() {
            while q < add.len() && add[q].0 as usize == i {
                v2.push(add[q].1);
                s2.push(add[q].2);
                if has_ids {
                    i2.push(u32::MAX);
                }
                q += 1;
            }
            if i < col.vals.len() {
                v2.push(col.vals[i]);
                s2.push(col.slots[i]);
                if has_ids {
                    i2.push(col.ids[i]);
                }
            }
        }
        col.vals = v2;
        col.slots = s2;
        col.ids = i2;
        // its texts without the UUIDs' 16 bytes
        let mut t2 = Vec::with_capacity(col.txt.len() - 16 * g.len());
        let mut tp = 0;
        for &a in g {
            t2.extend_from_slice(&col.txt[tp..a as usize]);
            tp = a as usize + 16;
        }
        t2.extend_from_slice(&col.txt[tp..]);
        col.txt = t2;
    }
    tail.finish()
}

/// Grouping setting FL_GROUP: unset or "off" = never (the product setting), "auto" = decide
/// per block, a number = always group on that field (when it has 2..MAX_GROUPS values).
fn group_mode() -> Option<Option<usize>> {
    static M: crate::engine::fl::NoEnvOnce<Option<Option<usize>>> = crate::engine::fl::NoEnvOnce::new();
    // the product setting: no grouping by source (0.15 average ratio for about 3% speed);
    // FL_GROUP=auto decides per block, FL_GROUP=<field> always groups on that field
    *M.get_or_init(|| match crate::engine::fl::no_env::var("FL_GROUP") {
        Err(_) => Some(None),
        Ok(v) if v == "auto" => None,
        Ok(v) => Some(v.parse().ok()),
    })
}

/// The field to group a block on, or None (decided per block by a cheap look at a sample).
fn choose_field(lines: &[&[u8]]) -> Option<usize> {
    match group_mode() {
        Some(f) => f,
        None => group::choose_field(&group::sample(lines)),
    }
}

/// Pack a block, grouped by field f when given (and the field has 2..MAX_GROUPS values).
/// Stream 9 is the line order (empty: not grouped).
fn pack_with(data: &[u8], lines: &[&[u8]], f: Option<usize>, threads: usize) -> Packed {
    let mut tg = Tick::now();
    if let Some((gid, sizes)) = f.and_then(|f| group::groups_of(lines, f)) {
        let perm = group::grouped_order(&gid, &sizes);
        let gl: Vec<&[u8]> = perm.iter().map(|&i| lines[i as usize]).collect();
        pt(10, &mut tg);
        let (mut p, tcol, times) = pack_block(data, &gl, true, threads);
        let mut tg = Tick::now();
        p.streams.push(group::order_stream(&gid, &sizes, tcol, &times));
        pt(11, &mut tg);
        if env_on("FL_GDBG") {
            let nt = times.iter().filter(|&&t| t == group::NO_TIME).count();
            let o = &p.streams[9];
            let mut q = 0;
            get_varint(o, &mut q);
            let g = get_varint(o, &mut q);
            for _ in 0..g {
                get_varint(o, &mut q);
            }
            let mut zeros = 0;
            let mut nz = 0;
            while q < o.len() {
                if get_varint(o, &mut q) == 0 { zeros += 1 } else { nz += 1 }
            }
            eprintln!("grouped: field {:?} groups {} tcol {:?} no-time {} of {} order {} zeros {} nonzero {} gid0 {:?} times {:?}", f, sizes.len(), tcol, nt, times.len(), p.streams[9].len(), zeros, nz, &gid[..8], &times[..8.min(times.len())]);
        }
        return p;
    }
    pt(10, &mut tg);
    let (mut p, _, _) = pack_block(data, lines, false, threads);
    p.streams.push(Vec::new());
    p
}

fn pack(data: &[u8], lines: &[&[u8]], threads: usize) -> Packed {
    let mut t = Tick::now();
    let f = choose_field(lines);
    pt(9, &mut t);
    pack_with(data, lines, f, threads)
}

/// Whether every byte of b is x (16 at a time).
#[inline]
fn all_eq(b: &[u8], x: u8) -> bool {
    let xx = x as u128 * (u128::MAX / 255);
    let mut ch = b.chunks_exact(16);
    for c in &mut ch {
        if u128::from_le_bytes(c.try_into().unwrap()) != xx {
            return false;
        }
    }
    ch.remainder().iter().all(|&k| k == x)
}

/// Number of decimal digits of x.
fn digits(mut x: u64) -> usize {
    let mut n = 1;
    while x >= 10 {
        x /= 10;
        n += 1;
    }
    n
}

/// A column of w-digit numbers, some with leading zeros (kind 3 + w) and some without (kind
/// 0): all become kind 3 + w ("zero-padded to w digits" writes them the same), so the column
/// needs no kinds. The same for numbers padded with spaces to w (kind SPN + w).
/// (M4: ref's decision, in whole-array scans: all kinds the same -> nothing to do; else the
/// first nonzero kind wk must be a padded kind and every kind 0 or wk (then a 0 is there), and
/// every number of kind 0 must have w digits.)
#[inline(never)]
fn fixed_width(c: &mut Col) {
    let ks = &c.kinds;
    let k0 = match ks.first() {
        Some(&k) => k,
        None => return,
    };
    if all_eq(ks, k0) {
        return;
    }
    let wk = match ks.iter().find(|&&k| k != 0) {
        Some(&k) => k,
        None => return,
    };
    if !((5..=21).contains(&wk) || (SPN + 2..=SPN + SPN_MAX).contains(&wk)) || !ks.iter().fold(true, |a, &k| a & ((k == 0) | (k == wk))) {
        return;
    }
    let w = if wk >= SPN { (wk - SPN) as usize } else { (wk - 3) as usize };
    // digits(v) == w exactly when lo <= v <= hi (w >= 2 here; a u64 has at most 20 digits; a
    // kind 0 is there, so w > 20 changes nothing)
    if w > 20 {
        return;
    }
    let lo = 10u64.pow(w as u32 - 1);
    let hi = if w == 20 { u64::MAX } else { 10u64.pow(w as u32) - 1 };
    if ks.iter().zip(&c.vals).fold(false, |a, (&k, &v)| a | ((k == 0) & ((v < lo) | (v > hi)))) {
        return;
    }
    c.kinds.iter_mut().for_each(|k| *k = wk);
}

/// Grouped columns (mode bit 16, see GROUP_MIN): put their numbers back in line order, in
/// place in their stream (7, or 4 with FL_ONENUMS), before the lines are rebuilt.
#[inline(never)]
fn ungroup(p: &mut Packed, hd: &heads::Heads, col_of: &[u32], first: &[u32], ids: &Ids) -> Result<(), &'static str> {
    const E: &str = "damaged grouped column";
    if hd.grouped.is_empty() {
        return Ok(());
    }
    let s = &p.streams;
    let nc = hd.cols.len();
    let ns = if split_nums_on() { 7 } else { 4 };
    // (column, start, group bytes, lengths)
    let grouped: Vec<(usize, usize, usize, [usize; GROUPS])> = hd.grouped.iter().map(|&(c, gb, seg)| (c, hd.cols[c].n, gb, seg)).collect();
    // per slot of a grouped column: (which grouped column + 1, its group)
    let mut gcol = vec![0u32; nc];
    for (i, g) in grouped.iter().enumerate() {
        gcol[g.0] = i as u32 + 1;
    }
    // per pattern: its slots in grouped columns, as (grouped column, group, slot in the
    // pattern), from tslot[t]
    let mut tslot: Vec<u32> = Vec::with_capacity(first.len());
    let mut slot_g: Vec<(u32, u8, u16)> = Vec::new();
    let mut gbp: Vec<usize> = grouped.iter().map(|g| g.2).collect();
    for t in 0..first.len() - 1 {
        tslot.push(slot_g.len() as u32);
        for (k, &c) in col_of[first[t] as usize..first[t + 1] as usize].iter().enumerate() {
            let gi = gcol[c as usize];
            if gi > 0 {
                let g = s[2][gbp[gi as usize - 1]];
                gbp[gi as usize - 1] += 1;
                if g as usize >= GROUPS {
                    return Err(E);
                }
                slot_g.push((gi - 1, g, k as u16));
            }
        }
    }
    tslot.push(slot_g.len() as u32);
    // read cursors of each segment (in a copy of the column's bytes), write cursors in place
    let src: Vec<Vec<u8>> = grouped.iter().map(|g| s[ns][g.1..g.1 + g.3.iter().sum::<usize>()].to_vec()).collect();
    let mut rd: Vec<[usize; GROUPS]> = grouped
        .iter()
        .map(|g| {
            let mut at = [0usize; GROUPS];
            let mut o = 0;
            for (x, &l) in at.iter_mut().zip(&g.3) {
                *x = o;
                o += l;
            }
            at
        })
        .collect();
    // each segment's end: a copy must not run into the next segment
    let ends: Vec<[usize; GROUPS]> = rd.iter().zip(&grouped).map(|(at, g)| std::array::from_fn(|k| at[k] + g.3[k])).collect();
    let mut wr: Vec<usize> = grouped.iter().map(|g| g.1).collect();
    let out = &mut p.streams[ns];
    let mut rc = RuleCur::new(ids);
    let mut given: Vec<bool> = Vec::new(); // the line's rules: whether each gave its value
    // the rules at slots of grouped columns (only their answers matter here), per pattern too
    let mut rg = vec![false; ids.rules.len()];
    let mut tg = vec![false; first.len() - 1];
    for t in 0..ids.rfirst.len().saturating_sub(1) {
        for ri in ids.rfirst[t] as usize..ids.rfirst[t + 1] as usize {
            let c = col_of[first[t] as usize + ids.rules[ri].slot as usize] as usize;
            rg[ri] = gcol[c] > 0;
            tg[t] |= rg[ri];
        }
    }
    for &tid in &ids.tid {
        let tid = tid as usize;
        let (rs, re) = if tg[tid] { (ids.rfirst[tid] as usize, ids.rfirst[tid + 1] as usize) } else { (0, 0) };
        let mut ri = rs;
        given.clear();
        for i in rs..re {
            given.push(rg[i] && rc.take(ids, i).is_some());
        }
        for &(gi, g, k) in &slot_g[tslot[tid] as usize..tslot[tid + 1] as usize] {
            // a value a rule gave is not in its column
            if ri < re {
                while ri < re && ids.rules[ri].slot < k {
                    ri += 1;
                }
                if ri < re && ids.rules[ri].slot == k && given[ri - rs] {
                    continue;
                }
            }
            let gi = gi as usize;
            // copy one varint
            let r = &mut rd[gi][g as usize];
            let end = ends[gi][g as usize];
            loop {
                if *r >= end {
                    return Err(E);
                }
                let x = src[gi][*r];
                *r += 1;
                out[wr[gi]] = x;
                wr[gi] += 1;
                if x < 0x80 {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Write x in decimal, zero-padded to `width` digits.
fn push_num(out: &mut Vec<u8>, mut x: u64, width: usize) {
    let mut b = [0u8; 24];
    let mut q = b.len();
    loop {
        q -= 1;
        b[q] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    while b.len() - q < width {
        q -= 1;
        b[q] = b'0';
    }
    out.extend_from_slice(&b[q..]);
}

/// Pattern slots -> column ids, as packing numbered them (shared by the pattern text before
/// the slot), and each pattern's first slot (one more entry at the end: the slot count).
#[inline(never)]
fn slot_columns(tmpl_stream: &[u8]) -> (Vec<u32>, Vec<u32>) {
    // every slot (a MARK) gets an entry of col_of and adds at most one key to the map: both
    // made big enough at once (the map up to 1M keys; past that it grows as it fills)
    let marks = tmpl_stream.iter().filter(|&&c| c == MARK).count();
    let mut col_of: Vec<u32> = Vec::with_capacity(marks);
    let mut first: Vec<u32> = vec![0];
    let shared = crate::engine::fl::no_env::var_os("FL_NOSHARE").is_none();
    let mut map: HashMap<(u32, u64), u32, BuildHasherDefault<SlotHasher>> = HashMap::with_capacity_and_hasher(if shared { marks.min(1 << 20) } else { 0 }, Default::default());
    // the patterns, each ended by '\n' (text after the last one is not a pattern)
    let mut a = 0;
    while let Some(e) = tmpl_stream[a..].iter().position(|&c| c == b'\n') {
        let t = &tmpl_stream[a..a + e];
        if shared {
            heads::slot_cols(t, &mut map, &mut col_of);
        } else {
            let n = col_of.len() as u32;
            col_of.extend(n..n + t.iter().filter(|&&c| c == MARK).count() as u32);
        }
        first.push(col_of.len() as u32);
        a += e + 1;
    }
    (col_of, first)
}

/// What the opener reads before the columns: each line's pattern id, the lines of each
/// pattern, and with rules (header flag 16) each pattern's first rule (one more entry at the
/// end), the rules, the yes/no of each value at a rule's slot in line order (0: the rule gives
/// it), the near rules' differences in order and the values the rules gave per prefix column.
pub(crate) struct Ids {
    pub tid: Vec<u32>,
    pub per: Vec<u32>,
    pub rfirst: Vec<u32>,
    pub rules: Vec<rules::Rule>,
    pub flags: Vec<u8>,
    pub near: Vec<i64>,
    pub minus: Vec<u32>,
    pub yes: Vec<u32>, // per rule: the values it gave
    pub foff: Vec<u32>, // per rule: its first answer in flags
    pub noff: Vec<u32>, // per rule: its first difference in near
}

/// The rules' answers (see code_answers), rule after rule: rule ri's yes/no in
/// flags[foff[ri]..] (0: the rule gives the value; none stored for `always` rules), its near
/// differences in near[noff[ri]..]; returns how many values each rule gave.
#[allow(clippy::too_many_arguments)]
fn answers(dec: &mut ozip::RcDec, per: &[u32], rfirst: &[u32], rules: &[rules::Rule], flags: &mut Vec<u8>, near: &mut Vec<i64>, foff: &mut Vec<u32>, noff: &mut Vec<u32>) -> Vec<u32> {
    let mut f = rules::Flags::new(rules.len());
    let mut yes = vec![0u32; rules.len()];
    for t in 0..per.len() {
        for ri in rfirst[t] as usize..rfirst[t + 1] as usize {
            let r = &rules[ri];
            foff.push(flags.len() as u32);
            noff.push(near.len() as u32);
            if r.always && r.op != rules::NEAR {
                yes[ri] += per[t]; // (nothing coded)
                continue;
            }
            for _ in 0..per[t] {
                let x = if r.always { 0 } else { f.step(dec, ri, 0) };
                if !r.always {
                    flags.push(x as u8);
                }
                yes[ri] += (x == 0) as u32;
                if x == 0 && r.op == rules::NEAR {
                    near.push(f.diff(dec, ri, 0));
                }
            }
        }
    }
    yes
}

/// Where an opener is in each rule's answers (Ids: rule after rule).
pub(crate) struct RuleCur {
    f: Vec<u32>,
    n: Vec<u32>,
}

impl RuleCur {
    pub fn new(ids: &Ids) -> RuleCur {
        RuleCur { f: ids.foff.clone(), n: ids.noff.clone() }
    }
    /// The next value of rule ri's slot: Some(its near difference, 0 for exact rules) when the
    /// rule gives it, None when the value is in its column.
    #[inline]
    pub fn take(&mut self, ids: &Ids, ri: usize) -> Option<i64> {
        let r = &ids.rules[ri];
        if !r.always {
            let i = self.f[ri] as usize;
            self.f[ri] += 1;
            if ids.flags[i] != 0 {
                return None;
            }
        }
        if r.op == rules::NEAR {
            let i = self.n[ri] as usize;
            self.n[ri] += 1;
            Some(ids.near[i])
        } else {
            Some(0)
        }
    }
}

/// The pattern id of each line (varints, or coded with TidModel: header flag 1), the number
/// of lines of each pattern; then the rules' answers (coded after the ids).
#[inline(never)]
fn line_tids(p: &Packed, col_of: &[u32], first: &[u32]) -> Result<Ids, &'static str> {
    const E: &str = "damaged pattern ids";
    let patterns = first.len() - 1;
    let s = &p.streams;
    let mut hp = 0;
    heads::rd(&s[2], &mut hp).ok_or(E)?;
    hp += 1; // the grouped column count
    let fl = *s[2].get(hp).ok_or(E)?;
    hp += 1;
    let tidm = fl & 1 != 0;
    let (rfirst, rules) = if fl & 16 != 0 {
        // the rules section follows the splitting columns' list
        if fl & 2 != 0 {
            let n = heads::rd(&s[2], &mut hp).ok_or(E)? as usize;
            for _ in 0..n {
                heads::rd(&s[2], &mut hp).ok_or(E)?;
            }
        }
        let h = &s[2];
        let (x, q) = std::panic::catch_unwind(move || {
            let mut q = hp;
            let x = rules::get_rules(h, &mut q, patterns);
            (x, q)
        })
        .map_err(|_| E)?;
        hp = q;
        // every rule: a slot of its pattern after its sources, in slot order
        if x.0.len() != patterns + 1 || x.0[patterns] as usize != x.1.len() {
            return Err(E);
        }
        for t in 0..patterns {
            let (a, b) = (x.0[t] as usize, x.0[t + 1] as usize);
            if a > b || b > x.1.len() {
                return Err(E);
            }
            let ns = (first[t + 1] - first[t]) as usize;
            let mut last = None;
            for r in &x.1[a..b] {
                if r.slot as usize >= ns || r.src[0] >= r.slot || (1..3).any(|i| r.sgn[i] != 0 && r.src[i] >= r.slot) || last.is_some_and(|l| l >= r.slot) {
                    return Err(E);
                }
                last = Some(r.slot);
            }
        }
        x
    } else {
        (Vec::new(), Vec::new())
    };
    // the rules' answers: the header's last `alen` bytes
    let alen = if fl & 16 != 0 { heads::rd(&s[2], &mut hp).ok_or(E)? as usize } else { 0 };
    if alen > s[2].len().saturating_sub(hp) {
        return Err(E);
    }
    let atext = &s[2][s[2].len() - alen..];
    let mut tid_of = Vec::with_capacity(p.lines);
    let mut per = vec![0u32; patterns];
    let (mut flags, mut near) = (Vec::new(), Vec::new());
    let mut minus = Vec::new();
    let mut yes = Vec::new();
    let (mut foff, mut noff) = (Vec::new(), Vec::new());
    if tidm {
        let mut tm = TidModel::new(patterns);
        let mut dec = ozip::RcDec::new(&s[1]);
        for _ in 0..p.lines {
            let t = tm.step(&mut dec, 0);
            *per.get_mut(t as usize).ok_or(E)? += 1; // (an id past the table stops here)
            tid_of.push(t);
        }
    } else {
        read_ids(&s[1], p.lines, &mut per, &mut tid_of).ok_or(E)?;
    }
    if !rules.is_empty() {
        let nprefix = col_of.iter().copied().max().map_or(0, |x| x as usize + 1);
        minus = vec![0u32; nprefix];
        let mut dec = ozip::RcDec::new(atext);
        yes = answers(&mut dec, &per, &rfirst, &rules, &mut flags, &mut near, &mut foff, &mut noff);
        // the values the rules gave, per column (a rule's slot has one column)
        for t in 0..patterns {
            for ri in rfirst[t] as usize..rfirst[t + 1] as usize {
                let c = *col_of.get(first[t] as usize + rules[ri].slot as usize).ok_or(E)?;
                minus[c as usize] += yes[ri];
            }
        }
    }
    let rfirst = if rfirst.is_empty() { vec![0u32; patterns + 1] } else { rfirst };
    Ok(Ids { tid: tid_of, per, rfirst, rules, flags, near, minus, yes, foff, noff })
}

#[inline(never)]
fn unpack_block(p: &mut Packed) -> Vec<u8> {
    let (col_of, first) = slot_columns(&p.streams[0]);
    let ids = line_tids(p, &col_of, &first).expect("pattern ids");
    let tid_of = &ids.tid;
    let hd = heads::read(&p.streams, &col_of, &first, &ids).expect("column header");
    // (keyed slots, entity.rs: only the fast opener)
    assert!(hd.plan.is_none(), "keyed block");
    ungroup(p, &hd, &col_of, &first, &ids).expect("grouped column");
    let s = &p.streams;
    let tmpls: Vec<&[u8]> = s[0].split(|&c| c == b'\n').collect();
    let words = table_entries(&s[8]);
    // a grouped block: note each line's end and clock time, for putting the lines back in order
    let grouped = s.len() > 9 && !s[9].is_empty();
    let tc = if grouped { group::time_col(&s[9]).map_or(u32::MAX as usize, |c| c as usize) } else { u32::MAX as usize };
    let (mut ends, mut times): (Vec<usize>, Vec<i64>) = if grouped { (Vec::with_capacity(p.lines), Vec::with_capacity(p.lines)) } else { (Vec::new(), Vec::new()) };
    let nc = hd.cols.len();
    struct Cur {
        k: usize,
        n: usize,
        t: usize,
        prev: i64,
        pd: i64, // the last difference (second differences, mode bit 32)
        recent: Vec<(usize, usize)>,
        delta: bool,
        dod: bool,
        same: Option<u8>,
        cache: u32, // index into `caches` + 1 (0: not cache coded)
        h: usize,   // place in the cache codes
        ns: usize,  // the stream of its numbers (4 or 7)
        dict: bool, // new values go to the column's list (`lists`, at the cache's index)
        split: bool, // its words choose the columns of the line's later slots (mode bit 64)
    }
    let mut lists: Vec<Vec<u64>> = Vec::new();
    let mut caches: Vec<Cache> = Vec::new();
    let mut cur = Vec::with_capacity(nc);
    for e in &hd.cols {
        let mode = e.mode;
        let same = if e.same < 256 { Some(e.same as u8) } else { None };
        let cache = if mode & 4 != 0 {
            caches.push(Cache::new());
            lists.push(Vec::new());
            caches.len() as u32
        } else {
            0
        };
        cur.push(Cur { k: e.k, n: e.n, t: e.t, prev: 0, pd: 0, recent: Vec::new(), delta: mode & 1 != 0, dod: mode & 32 != 0, same, cache, h: e.h, dict: mode & 8 != 0, ns: e.ns as usize, split: mode & 64 != 0 });
    }
    let mut out = Vec::new();
    let mut nb = [0u8; 24];
    let mut slot_last = vec![0u64; col_of.len()];
    let mut kind_last = [0u64; 256];
    let use_ks = { let mut q = 0; get_varint(&s[2], &mut q); s[2].get(q + 1).map_or(true, |f| f & 8 == 0) };
    let mut id_list: Vec<u64> = Vec::new(); // long IDs in the order they first appeared
    // the recent words of each column; columns and slots made on first use after a word of a
    // splitting column (same order as packing)
    let mut wrec: Vec<Recent8> = vec![Recent8::default(); nc];
    let mut sub: FxMap<(u32, u64), u32> = FxMap::default();
    let mut sub_slot: FxMap<(u32, u32), u32> = FxMap::default();
    let mut ncols = (col_of.iter().copied().max().map_or(0, |x| x + 1)) as u32;
    // rules: the line's values so far (as rule sources) and the next answer
    let mut xs = vec![rules::NOT_NUM; first.windows(2).map(|w| (w[1] - w[0]) as usize).max().unwrap_or(0)];
    let mut rc = RuleCur::new(&ids);
    let rl = &ids.rules;
    for &tid in tid_of {
        let tid = tid as usize;
        let mut slot = first[tid] as usize;
        let s0 = slot;
        let (mut ri, re) = (ids.rfirst[tid] as usize, ids.rfirst[tid + 1] as usize);
        let has_r = ri < re;
        let mut ctx = 0u64;
        let mut lt = group::NO_TIME;
        for &c in tmpls[tid] {
            if c != MARK {
                out.push(c);
                continue;
            }
            let (ci, sid) = if ctx == 0 {
                (col_of[slot] as usize, slot)
            } else {
                let ci = sub_col(&mut sub, &mut ncols, col_of[slot], ctx);
                let ns = slot_last.len() as u32;
                let sid = *sub_slot.entry((ci, slot as u32)).or_insert(ns);
                if sid == ns {
                    slot_last.push(0);
                }
                (ci as usize, sid as usize)
            };
            let k = slot - s0;
            slot += 1;
            if ri < re && rl[ri].slot as usize == k {
                // a rule's slot: when its answer is yes, the rule gives the value
                ri += 1;
                if let Some(d) = rc.take(&ids, ri - 1) {
                    let r = &rl[ri - 1];
                    let (pk, v) = rules::give(r, &xs, d);
                    push_num(&mut out, v, if pk >= 5 { (pk - 3) as usize } else { 0 });
                    kind_last[kclass(pk)] = v;
                    xs[k] = v;
                    continue;
                }
            }
            let cu = &mut cur[ci];
            let kind = match cu.same {
                Some(k) => k,
                None => {
                    cu.k += 1;
                    s[3][cu.k - 1]
                }
            };
            if has_r {
                xs[k] = rules::NOT_NUM;
            }
            match kind {
                1 => {
                    let len = get_varint(&s[5], &mut cu.t) as usize;
                    out.extend_from_slice(&s[5][cu.t..cu.t + len]);
                    cu.recent.insert(0, (cu.t, cu.t + len));
                    cu.recent.truncate(8);
                    cu.t += len;
                }
                2 => out.extend_from_slice(&s[5][cu.recent[0].0..cu.recent[0].1]),
                80..=87 => {
                    let w = wrec[ci].hit((kind - 80) as usize);
                    out.extend_from_slice(words[w as usize]);
                    if cu.split {
                        ctx = w as u64 + 1;
                    }
                }
                71..=77 => {
                    let r = cu.recent.remove((kind - 70) as usize);
                    cu.recent.insert(0, r);
                    out.extend_from_slice(&s[5][r.0..r.1]);
                }
                100..=103 => {
                    let digits: &[u8; 16] = if kind & 1 == 0 { b"0123456789abcdef" } else { b"0123456789ABCDEF" };
                    let n = if kind < 102 { 16 } else { get_varint(&s[5], &mut cu.t) as usize };
                    for q in 0..n {
                        if kind < 102 && matches!(q, 4 | 6 | 8 | 10) {
                            out.push(b'-');
                        }
                        let b = s[5][cu.t + q];
                        out.push(digits[(b >> 4) as usize]);
                        out.push(digits[(b & 15) as usize]);
                    }
                    cu.t += n;
                }
                _ => {
                    let kc = kclass(kind);
                    let v = if cu.cache == 0 {
                        let x = get_varint(&s[cu.ns], &mut cu.n);
                        let v = if cu.dod {
                            cu.pd = cu.pd.wrapping_add(unzz(x));
                            cu.prev.wrapping_add(cu.pd)
                        } else if cu.delta {
                            cu.prev.wrapping_add(unzz(x))
                        } else {
                            x as i64
                        };
                        cu.prev = v;
                        v
                    } else {
                        let c = &mut caches[cu.cache as usize - 1];
                        let (t, kl) = (slot_last[sid], kind_last[kc]);
                        let code = s[6][cu.h] as usize;
                        cu.h += 1;
                        let (v, p) = if code < CK {
                            c.value(code, t, kl)
                        } else {
                            let x = get_varint(&s[cu.ns], &mut cu.n);
                            let li = &mut lists[cu.cache as usize - 1];
                            let v = if code == CK + 1 {
                                if cu.dict { li[x as usize] } else { id_list[id_list.len() - 1 - x as usize] }
                            } else if cu.delta {
                                c.m[0].wrapping_add(unzz(x) as u64)
                            } else {
                                x
                            };
                            if code == CK {
                                if kind == LONG_ID {
                                    id_list.push(v);
                                }
                                if cu.dict {
                                    li.push(v);
                                }
                            }
                            (v, c.pos(v))
                        };
                        c.update(v, p, v == t, v == kl && use_ks);
                        slot_last[sid] = v;
                        v as i64
                    };
                    kind_last[kc] = v as u64;
                    if has_r {
                        xs[k] = rules::src_val(kind, v as u64);
                    }
                    if kind == WORD {
                        out.extend_from_slice(words[v as usize]);
                        wrec[ci].push(v as u32);
                        if cu.split {
                            ctx = v as u64 + 1;
                        }
                        continue;
                    }
                    if kind == NAMEK {
                        out.extend_from_slice(NAMES[v as usize]);
                        continue;
                    }
                    if kind == 30 {
                        for k in (0..4).rev() {
                            push_num(&mut out, (v as u64 >> (8 * k)) & 255, 0);
                            if k > 0 {
                                out.push(b'.');
                            }
                        }
                        continue;
                    }
                    if kind == LONG_ID {
                        if v & 1 == 1 {
                            out.push(b'-');
                        }
                        push_num(&mut out, v as u64 >> 1, 0);
                        continue;
                    }
                    if (HEXNUM..HEXNUM + 18).contains(&kind) {
                        let t = kind - HEXNUM;
                        let (w, digits) = ((t % 9 + 8) as usize, if t >= 9 { b"0123456789ABCDEF" } else { b"0123456789abcdef" });
                        for q in (0..w).rev() {
                            out.push(digits[(v as u64 >> (4 * q)) as usize & 15]);
                        }
                        continue;
                    }
                    if (HEXS..HEXS + 6).contains(&kind) {
                        let t = kind - HEXS;
                        let (w, digits) = ((t % 3 + 5) as usize, if t >= 3 { b"0123456789ABCDEF" } else { b"0123456789abcdef" });
                        for q in (0..w).rev() {
                            out.push(digits[(v as u64 >> (4 * q)) as usize & 15]);
                        }
                        continue;
                    }
                    if (SPN..=SPN + SPN_MAX).contains(&kind) {
                        let w = (kind - SPN) as usize;
                        for _ in digits(v as u64)..w {
                            out.push(b' ');
                        }
                        push_num(&mut out, v as u64, 0);
                        continue;
                    }
                    if (110..124).contains(&kind) {
                        // H.MM.SS[.F]
                        let t = kind - 110;
                        let (hw, fd) = ((t & 1) + 1, (t >> 1) as u32);
                        let p10 = 10u64.pow(fd);
                        let secs = v as u64 / p10;
                        push_num(&mut out, secs / 3600, hw as usize);
                        out.push(b'.');
                        push_num(&mut out, secs / 60 % 60, 2);
                        out.push(b'.');
                        push_num(&mut out, secs % 60, 2);
                        if fd > 0 {
                            out.push(b'.');
                            push_num(&mut out, v as u64 % p10, fd as usize);
                        }
                        continue;
                    }
                    if kind == 68 {
                        // H:M:S:F, nothing zero-padded
                        let secs = v as u64 / 1000;
                        push_num(&mut out, secs / 3600, 0);
                        out.push(b':');
                        push_num(&mut out, secs / 60 % 60, 0);
                        out.push(b':');
                        push_num(&mut out, secs % 60, 0);
                        out.push(b':');
                        push_num(&mut out, v as u64 % 1000, 0);
                        continue;
                    }
                    if (40..68).contains(&kind) {
                        if ci == tc && lt == group::NO_TIME {
                            lt = micros(kind, v as u64);
                        }
                        let t = kind - 40;
                        let (hw, comma, fd) = ((t & 1) + 1, (t >> 1) & 1, (t >> 2) as u32);
                        let p10 = 10u64.pow(fd);
                        let secs = v as u64 / p10;
                        push_num(&mut out, secs / 3600, hw as usize);
                        out.push(b':');
                        push_num(&mut out, secs / 60 % 60, 2);
                        out.push(b':');
                        push_num(&mut out, secs % 60, 2);
                        if fd > 0 {
                            out.push(if comma == 1 { b',' } else { b'.' });
                            push_num(&mut out, v as u64 % p10, fd as usize);
                        }
                        continue;
                    }
                    let mut x = v as u64;
                    let mut q = nb.len();
                    loop {
                        q -= 1;
                        nb[q] = b'0' + (x % 10) as u8;
                        x /= 10;
                        if x == 0 {
                            break;
                        }
                    }
                    if kind >= 3 {
                        let w = (kind - 3) as usize;
                        while nb.len() - q < w {
                            q -= 1;
                            nb[q] = b'0';
                        }
                    }
                    out.extend_from_slice(&nb[q..]);
                }
            }
        }
        out.push(b'\n');
        if grouped {
            ends.push(out.len());
            times.push(lt);
        }
    }
    if grouped {
        return group::ungroup(&out, &ends, &times, &s[9]).expect("line order");
    }
    out
}

/// Bytes of x as a varint.
fn varint_len(x: u64) -> usize {
    1 + (63 - (x | 1).leading_zeros() as usize) / 7
}

/// The coder level of a file's pattern or word table of n bytes: the fast coder with a deeper
/// copy search (ozip levels 27-28: buckets of 32-64; opened as fast as level 22). Tables are
/// small and full of near repeats, so the search pays; very large ones get the lighter one.
fn table_level(level: u32, n: usize) -> u32 {
    if level < 20 {
        level
    } else if n < 2 << 20 {
        28
    } else {
        27
    }
}

/// One table for the whole file from the blocks' tables in stream k (entries ended by '\n'),
/// and the blocks' lists of entry numbers as one stream (Lists).
fn file_table<'a>(all: &'a [(Packed, Vec<Vec<u8>>, f64, f64)], k: usize, kit: &'a [Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let mut gmap: FxMap<&[u8], u32> = FxMap::default();
    // the kit's entries are numbered first; they are not in the table
    for e in kit {
        let n = gmap.len() as u32;
        gmap.entry(&e[..]).or_insert(n);
    }
    let mut union: Vec<u8> = Vec::new();
    let mut lists = Lists::new(gmap.len());
    let mut out = Vec::new();
    let mut ids = Vec::new();
    for (p, _, _, _) in all {
        let tbl = &p.streams[k];
        ids.clear();
        // the entries: each ends with '\n' (an entry may be empty: the shape of an empty line)
        let body = tbl.strip_suffix(b"\n").unwrap_or(&[]);
        for t in body.split(|&c| c == b'\n').take(if tbl.is_empty() { 0 } else { usize::MAX }) {
            let n = gmap.len() as u32;
            ids.push(*gmap.entry(t).or_insert_with(|| {
                union.extend_from_slice(t);
                union.push(b'\n');
                n
            }));
        }
        lists.put(&ids, &mut out);
    }
    (union, out)
}

/// The blocks' lists of entry numbers in a file's table, one block after the other: per block
/// varint(entries), then per entry a varint: 0 = a new entry (the next number), 1 = the entry
/// that followed the entry before it the last time (in an earlier block; the first entry of
/// a block follows "the block start"), else zz(its number - the last number coded so) + 2.
struct Lists {
    next: Vec<u32>, // per entry + 1 (0: the block start): the entry after it last time
    seen: u32,      // entries so far: the next new number
}

impl Lists {
    fn new(seen: usize) -> Lists {
        Lists { next: vec![u32::MAX; seen + 1], seen: seen as u32 }
    }
    fn put(&mut self, ids: &[u32], out: &mut Vec<u8>) {
        put_varint(out, ids.len() as u64);
        let (mut last, mut old) = (0usize, 0i64);
        for &id in ids {
            if id == self.seen {
                put_varint(out, 0);
                self.seen += 1;
                self.next.push(u32::MAX);
            } else if self.next[last] == id {
                put_varint(out, 1);
            } else {
                put_varint(out, zz(id as i64 - old) + 2);
                old = id as i64;
            }
            self.next[last] = id;
            last = id as usize + 1;
        }
    }
    /// The next block's list (`max`: entries in the table, kit included).
    fn get(&mut self, b: &[u8], p: &mut usize, max: usize) -> Option<Vec<u32>> {
        let n = heads::rd(b, p)? as usize;
        if n > b.len() {
            return None;
        }
        let mut ids = Vec::with_capacity(n);
        let (mut last, mut old) = (0usize, 0i64);
        for _ in 0..n {
            let x = heads::rd(b, p)?;
            let id = match x {
                0 => {
                    self.seen += 1;
                    self.next.push(u32::MAX);
                    self.seen - 1
                }
                1 => self.next[last],
                _ => {
                    old = old.wrapping_add(unzz(x - 2));
                    old as u32
                }
            };
            if id as usize >= max.min(self.seen as usize) || (x > 1 && old != id as i64) {
                return None;
            }
            self.next[last] = id;
            last = id as usize + 1;
            ids.push(id);
        }
        Some(ids)
    }
}

/// The entries of a table (each ended by '\n').
fn table_entries(t: &[u8]) -> Vec<&[u8]> {
    match t.strip_suffix(b"\n") {
        Some(b) => b.split(|&c| c == b'\n').collect(),
        None => Vec::new(),
    }
}

/// A block's own table from the file's table entries and the block's list.
fn rebuild_table(entries: &[&[u8]], ids: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.iter().map(|&i| entries[i as usize].len() + 1).sum());
    for &i in ids {
        out.extend_from_slice(entries[i as usize]);
        out.push(b'\n');
    }
    out
}

/// Starter kit (learned in advance from earlier lines of the same source, known to both
/// sides, not counted in the output): pattern and word tables. A unit's file tables then hold
/// only the entries that are not in the kit, and the blocks' lists number kit entries first.
#[derive(Default)]
struct Kit {
    pats: Vec<Vec<u8>>,
    words: Vec<Vec<u8>>,
}

impl Kit {
    fn bytes(&self) -> usize {
        self.pats.iter().chain(&self.words).map(|e| e.len() + 1).sum()
    }
}

/// Learn a kit from lines: the pattern and word tables of their blocks, in order of first use.
fn learn_kit(data: &[u8], lines: &[&[u8]], one_max: usize) -> Kit {
    let per_block = if lines.len() <= one_max { lines.len().max(1) } else { block_lines() };
    let mut kit = Kit::default();
    let (mut ps, mut ws): (FxMap<Vec<u8>, ()>, FxMap<Vec<u8>, ()>) = (FxMap::default(), FxMap::default());
    for b in lines.chunks(per_block) {
        let p = pack(data, b, 1);
        for (k, set, list) in [(0, &mut ps, &mut kit.pats), (8, &mut ws, &mut kit.words)] {
            for e in table_entries(&p.streams[k]) {
                if set.insert(e.to_vec(), ()).is_none() {
                    list.push(e.to_vec());
                }
            }
        }
    }
    kit
}

/// Lines per block for files above FL_ONEMAX lines (FL_BLOCK, 100K by default).
fn block_lines() -> usize {
    static B: crate::engine::fl::NoEnvOnce<usize> = crate::engine::fl::NoEnvOnce::new();
    *B.get_or_init(|| crate::engine::fl::no_env::var("FL_BLOCK").ok().and_then(|x| x.parse().ok()).unwrap_or(100_000))
}

/// What packing and opening a unit (a file, or a piece of one) gave.
#[derive(Default)]
struct Unit {
    out: usize,
    sbytes: [usize; 11],
    ts: f64,
    tcd: f64,
    topen: f64,
    wall: f64,
    owall: f64, // all cores: open wall clock (threads > 1)
    nblocks: usize,
    ngrouped: usize,
    exact: bool,
}

/// A packed unit, as counted: 1 flag byte (whether it ends with a newline); the unit's tables
/// (pattern table, pattern lists, word table, word lists), each with a 3-byte length; per
/// block: varint(lines), varint(bytes), its coded streams 1-7 with a 3-byte length each, and
/// stream 9 (line order) with a 3-byte length, or one flag byte when the block is not grouped.
/// (Streams 0 and 8 are the tables' parts.)
struct CodedUnit {
    nl: bool,
    derive: bool, // some blocks have derived fields (stream 10; derive.rs)
    tables: [Vec<u8>; 4],
    blocks: Vec<(usize, usize, Vec<Vec<u8>>)>,
    /// the unit's table bank (fz::Bank::write), empty for none (a bit of the flag byte says
    /// which), with a 3-byte length
    bank: Vec<u8>,
}

impl CodedUnit {
    fn bytes(&self) -> usize {
        let mut n = 1 + self.tables.iter().map(|t| t.len() + 3).sum::<usize>();
        n += if self.bank.is_empty() { 0 } else { self.bank.len() + 3 };
        for (lines, size, coded) in &self.blocks {
            n += varint_len(*lines as u64) + varint_len(*size as u64);
            n += (1..=7).map(|k| coded[k].len() + 3).sum::<usize>();
            n += if coded[9].is_empty() { 1 } else { coded[9].len() + 3 };
            if self.derive {
                // derived fields: the side stream, or a flag byte when the block has none
                n += if coded[10].is_empty() { 1 } else { coded[10].len() + 3 };
            }
        }
        n
    }
}

/// Undo the coder on a stream of at most `max` bytes (a damaged length must not ask for a
/// huge buffer).
fn decode_max(c: &[u8], max: usize) -> Result<Vec<u8>, &'static str> {
    decode_max_r(c, max, None)
}

/// decode_max, an fz frame with the unit's bank for its stream (r).
fn decode_max_r(c: &[u8], max: usize, r: fz::Refs) -> Result<Vec<u8>, &'static str> {
    let mut p = 1;
    let n = heads::rd(c, &mut p).ok_or("damaged stream")? as usize;
    if n > max {
        return Err("damaged stream length");
    }
    if c[0] == 6 {
        return fz::decompress_r(&c[p..], n, r).ok_or("damaged stream");
    }
    ozip::decompress_with(c, None).ok_or("damaged stream")
}

/// A unit's bank, read: None when it has none.
fn unit_bank(u: &CodedUnit) -> Result<Option<fz::Bank>, &'static str> {
    if u.bank.is_empty() {
        return Ok(None);
    }
    fz::Bank::read(&u.bank).ok_or("damaged bank").map(Some)
}

/// Open a unit from its coded bytes only (plus the kit) into `out` (cleared first). Damaged
/// input gives an error: the new parts (tables, lists, column headers, sizes) are checked as
/// they are read; the rest of a block's opening runs under catch_unwind, so an index past a
/// damaged stream's end also comes back as an error.
fn open_unit(u: &CodedUnit, kit: Option<&Kit>, out: &mut Vec<u8>, timing: &mut f64) -> Result<(), &'static str> {
    out.clear();
    let to = Tick::now();
    // the sizes first: they bound every buffer
    let mut total = 0usize;
    for (lines, size, coded) in &u.blocks {
        if *lines == 0 || lines > size || *size > 1 << 36 || coded.len() != 11 {
            return Err("damaged block size");
        }
        total = total.checked_add(*size).ok_or("damaged block size")?;
    }
    let max = 16 * total + (1 << 20);
    let none: Vec<Vec<u8>> = Vec::new();
    let (kp, kw) = kit.map_or((&none, &none), |k| (&k.pats, &k.words));
    let ut = decode_max(&u.tables[0], max)?;
    let lt = decode_max(&u.tables[1], max)?;
    let wut = decode_max(&u.tables[2], max)?;
    let wlt = decode_max(&u.tables[3], max)?;
    let bank = unit_bank(u)?;
    let entries: Vec<&[u8]> = kp.iter().map(|e| &e[..]).chain(table_entries(&ut)).collect();
    let wentries: Vec<&[u8]> = kw.iter().map(|e| &e[..]).chain(table_entries(&wut)).collect();
    let (mut tl, mut wl) = (Lists::new(kp.len()), Lists::new(kw.len()));
    let old_open = env_on("FL_OLDOPEN");
    let mut dtmp: Vec<u8> = Vec::new(); // a rewritten block's text (derived fields)
    let (mut tp, mut wp) = (0, 0);
    *out = vec![0u8; total + 64]; // (zeroed pages come from the system as they are used)
    let mut o = 0;
    *timing += to.elapsed().as_secs_f64();
    for (lines, size, coded) in &u.blocks {
        let to = Tick::now();
        let ids = tl.get(&lt, &mut tp, entries.len()).ok_or("damaged pattern lists")?;
        let wids = wl.get(&wlt, &mut wp, wentries.len()).ok_or("damaged word lists")?;
        let mut streams = Vec::with_capacity(10);
        for (k, c) in coded.iter().enumerate() {
            streams.push(match k {
                0 => rebuild_table(&entries, &ids),
                8 => rebuild_table(&wentries, &wids),
                9 | 10 if c.is_empty() => Vec::new(),
                _ => decode_max_r(c, 16 * size + 4096, bank.as_ref().map(|b| (b, k)))?,
            });
        }
        prof_add(28, to);
        let tu = Tick::now();
        let mut q = Packed { streams, lines: *lines, raw: 0, alt: Vec::new(), pre: Vec::new(), mk: 0 };
        let (o0, size) = (o, *size);
        let side = std::mem::take(&mut q.streams[10]);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(cp) = derive::inline_of(&side) {
                // B5: derived parts put in by the line printer
                return open::unpack_into_c(&mut q, out, o0, size, Some(&cp));
            }
            if !side.is_empty() {
                // derived fields: open the rewritten text, then put the derived parts back
                let tsize = derive::text_size(&side);
                if tsize > 16 * size + 4096 {
                    return Err("damaged derive stream");
                }
                if dtmp.len() < tsize + 128 {
                    dtmp.resize(tsize + 128, 0);
                }
                let e = if old_open {
                    let b = unpack_block(&mut q);
                    if b.len() > tsize {
                        return Err("damaged block");
                    }
                    dtmp[..b.len()].copy_from_slice(&b);
                    b.len()
                } else {
                    open::unpack_into(&mut q, &mut dtmp, 0, tsize)?
                };
                dtmp[e..e + 64].fill(0);
                let ti = Tick::now();
                let o = derive::Opener::default().inverse_fused(&dtmp, e, &side, out, o0).ok_or("damaged derive stream")?;
                prof_add(7 + 40, ti);
                return Ok(o);
            }
            if old_open {
                let b = unpack_block(&mut q);
                if b.len() > size {
                    return Err("damaged block");
                }
                out[o0..o0 + b.len()].copy_from_slice(&b);
                Ok(o0 + b.len())
            } else {
                open::unpack_into(&mut q, out, o0, size)
            }
        }));
        o = match r {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("damaged block (caught)"),
        };
        if o - o0 != size {
            return Err("damaged block: wrong size");
        }
        prof_add(29, tu);
        *timing += to.elapsed().as_secs_f64();
    }
    if tp != lt.len() || wp != wlt.len() {
        return Err("damaged lists: bytes left");
    }
    out.truncate(o);
    if !u.nl {
        out.pop();
    }
    Ok(())
}

/// Open a unit on `threads` cores: the tables and the blocks' lists first (one core), then the
/// blocks side by side, each into its own buffer and then copied to its place in the output.
fn open_unit_par(u: &CodedUnit, kit: Option<&Kit>, threads: usize) -> Result<Vec<u8>, &'static str> {
    let mut total = 0usize;
    for (lines, size, coded) in &u.blocks {
        if *lines == 0 || lines > size || *size > 1 << 36 || coded.len() != 11 {
            return Err("damaged block size");
        }
        total = total.checked_add(*size).ok_or("damaged block size")?;
    }
    let max = 16 * total + (1 << 20);
    let none: Vec<Vec<u8>> = Vec::new();
    let (kp, kw) = kit.map_or((&none, &none), |k| (&k.pats, &k.words));
    let ut = decode_max(&u.tables[0], max)?;
    let lt = decode_max(&u.tables[1], max)?;
    let wut = decode_max(&u.tables[2], max)?;
    let wlt = decode_max(&u.tables[3], max)?;
    let bank = unit_bank(u)?;
    let entries: Vec<&[u8]> = kp.iter().map(|e| &e[..]).chain(table_entries(&ut)).collect();
    let wentries: Vec<&[u8]> = kw.iter().map(|e| &e[..]).chain(table_entries(&wut)).collect();
    let (mut tl, mut wl) = (Lists::new(kp.len()), Lists::new(kw.len()));
    let (mut tp, mut wp) = (0, 0);
    let mut ids = Vec::with_capacity(u.blocks.len());
    for _ in &u.blocks {
        let a = tl.get(&lt, &mut tp, entries.len()).ok_or("damaged pattern lists")?;
        let b = wl.get(&wlt, &mut wp, wentries.len()).ok_or("damaged word lists")?;
        ids.push((a, b));
    }
    if tp != lt.len() || wp != wlt.len() {
        return Err("damaged lists: bytes left");
    }
    let mut out = vec![0u8; total];
    let mut places: Vec<std::sync::Mutex<&mut [u8]>> = Vec::with_capacity(u.blocks.len());
    let mut rest: &mut [u8] = &mut out[..];
    for (_, size, _) in &u.blocks {
        let (a, b) = rest.split_at_mut(*size);
        places.push(std::sync::Mutex::new(a));
        rest = b;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<&'static str>> = std::sync::Mutex::new(None);
    std::thread::scope(|sc| {
        for _ in 0..threads.min(u.blocks.len()) {
            sc.spawn(|| {
                let mut buf: Vec<u8> = Vec::new();
                let mut buf2: Vec<u8> = Vec::new();
                loop {
                    let bi = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if bi >= u.blocks.len() {
                        break;
                    }
                    let (lines, size, coded) = &u.blocks[bi];
                    let r = (|| -> Result<(), &'static str> {
                        let mut streams = Vec::with_capacity(10);
                        for (k, c) in coded.iter().enumerate() {
                            streams.push(match k {
                                0 => rebuild_table(&entries, &ids[bi].0),
                                8 => rebuild_table(&wentries, &ids[bi].1),
                                9 | 10 if c.is_empty() => Vec::new(),
                                _ => decode_max_r(c, 16 * size + 4096, bank.as_ref().map(|b| (b, k)))?,
                            });
                        }
                        let mut q = Packed { streams, lines: *lines, raw: 0, alt: Vec::new(), pre: Vec::new(), mk: 0 };
                        let side = std::mem::take(&mut q.streams[10]);
                        if let Some(cp) = derive::inline_of(&side) {
                            // B5: derived parts put in by the line printer
                            buf.clear();
                            buf.resize(*size + 128, 0);
                            let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open::unpack_into_c(&mut q, &mut buf, 0, *size, Some(&cp))));
                            match e {
                                Ok(Ok(e)) if e == *size => {
                                    places[bi].lock().unwrap().copy_from_slice(&buf[..e]);
                                    return Ok(());
                                }
                                Ok(Ok(_)) => return Err("damaged block: wrong size"),
                                Ok(Err(e)) => return Err(e),
                                Err(_) => return Err("damaged block (caught)"),
                            }
                        }
                        let tsize = if side.is_empty() { *size } else { derive::text_size(&side) };
                        if tsize > 16 * size + 4096 {
                            return Err("damaged derive stream");
                        }
                        buf.clear();
                        buf.resize(tsize + 128, 0);
                        let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<usize, &'static str> {
                            let e = open::unpack_into(&mut q, &mut buf, 0, tsize)?;
                            if side.is_empty() {
                                return Ok(e);
                            }
                            // derived fields: put the derived parts back (a fresh rules state per block)
                            buf[e..e + 64].fill(0);
                            buf2.clear();
                            buf2.resize(size + 64, 0);
                            let o = derive::Opener::default().inverse_fused(&buf, e, &side, &mut buf2, 0).ok_or("damaged derive stream")?;
                            if o != *size {
                                return Err("damaged block: wrong size");
                            }
                            std::mem::swap(&mut buf, &mut buf2);
                            Ok(o)
                        }));
                        match e {
                            Ok(Ok(e)) if e == *size => {
                                places[bi].lock().unwrap().copy_from_slice(&buf[..e]);
                                Ok(())
                            }
                            Ok(Ok(_)) => Err("damaged block: wrong size"),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err("damaged block (caught)"),
                        }
                    })();
                    if let Err(e) = r {
                        *failed.lock().unwrap() = Some(e);
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    drop(places);
    if !u.nl {
        out.pop();
    }
    Ok(out)
}


/// Pack a unit (its lines) in blocks, `threads` at a time: the coded unit, and per block the
/// time spent splitting and coding.
fn pack_coded(data: &[u8], lines: &[&[u8]], nl: bool, level: u32, threads: usize, one_max: usize, kit: Option<&Kit>, u: &mut Unit) -> CodedUnit {
    let per_block = if lines.len() <= one_max { lines.len().max(1) } else { block_lines() };
    let blocks: Vec<&[&[u8]]> = lines.chunks(per_block).collect();
    // timing: all blocks, `threads` at a time
    let t0 = Instant::now();
    // derived fields (derive.rs): the rules that hold on a sample of the unit's lines; each
    // block's lines are rewritten without their derived parts by the thread that packs it (the
    // rules' state starts afresh in every block, so blocks pack and open side by side); the
    // side stream of each block is its stream 10
    let td = Tick::now();
    let rules = derive::detect(lines);
    u.ts += td.elapsed().as_secs_f64();
    let any_derive = std::sync::atomic::AtomicBool::new(false);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<(Packed, Vec<Vec<u8>>, f64, f64)>>> = (0..blocks.len()).map(|_| std::sync::Mutex::new(None)).collect();
    let preps: Vec<std::sync::Mutex<Vec<Option<fz::Prep>>>> = (0..blocks.len()).map(|_| std::sync::Mutex::new(Vec::new())).collect();
    // fewer blocks than threads: the spare threads help with each block (the per-core
    // speeds printed are then not per core: take those from runs with 1 thread)
    let per = (threads / blocks.len().max(1)).max(1);
    let work = || loop {
        let b = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if b >= blocks.len() {
            break;
        }
        // the last blocks (fewer left than threads): the spare threads help them (same bytes)
        let left = blocks.len() - b;
        let per = if left < threads { (threads / left).max(per) } else { per };
        let ta = Tick::now();
        let fw = derive::Packer::new(rules).forward_par(data, blocks[b], per);
        prof_add(24, ta);
        if fw.is_some() {
            any_derive.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let mut p = match &fw {
            Some(f) => pack(&f.text, &f.lines(), per),
            None => pack(data, blocks[b], per),
        };
        p.streams.push(fw.as_ref().map_or(Vec::new(), |f| f.side.clone()));
        // B5: a block of bgl / ctime markers in the patterns' fixed text: the line printer
        // puts the derived parts in (no put-back pass)
        if let Some(f) = &fw {
            if derive::may_inline(&p.streams[10]) && open::inline_ok(&p, f.marks) {
                derive::mark_inline(&mut p.streams[10]);
            }
            if crate::engine::fl::no_env::var_os("FL_B5DBG").is_some() {
                eprintln!("B5DBG block {} inline {} marks {} in patterns {}", b, derive::inline_of(&p.streams[10]).is_some(), f.marks, p.mk);
            }
        }
        let tb = Tick::now();
        // (streams 0 and 8, the block's own pattern and word tables, are not coded:
        // the file's tables and the blocks' lists of entries in them are, below)
        let (coded, pr): (Vec<Vec<u8>>, Vec<Option<fz::Prep>>) = code_streams(&p, level, per).into_iter().unzip();
        let tc = Tick::now();
        *results[b].lock().unwrap() = Some((p, coded, (tb - ta).as_secs_f64(), (tc - tb).as_secs_f64()));
        *preps[b].lock().unwrap() = pr;
    };
    std::thread::scope(|sc| {
        for _ in 0..threads.min(blocks.len()) {
            sc.spawn(work);
        }
    });
    u.wall = t0.elapsed().as_secs_f64();
    let any_derive = any_derive.into_inner();
    // one pattern table for the unit: each block keeps only the list of the patterns it uses
    // (as numbers into the table); the table is packed once, and the opener rebuilds each
    // block's own table from these two; the same for the words turned into values (stream 8)
    let tg = Tick::now();
    let all: Vec<(Packed, Vec<Vec<u8>>, f64, f64)> = results.iter().map(|r| r.lock().unwrap().take().unwrap()).collect();
    let none: Vec<Vec<u8>> = Vec::new();
    let (kp, kw) = kit.map_or((&none, &none), |k| (&k.pats, &k.words));
    let (union, lists) = file_table(&all, 0, kp);
    let (wunion, wlists) = file_table(&all, 8, kw);
    let raw = [union, lists, wunion, wlists];
    let tables: [Vec<u8>; 4] = if threads > 1 {
        // all cores: the four tables side by side (the pattern table is the big one)
        let mut t: [Vec<u8>; 4] = Default::default();
        std::thread::scope(|sc| {
            for (o, r) in t.iter_mut().zip(&raw) {
                sc.spawn(move || *o = ozip::compress_with(r, table_level(level, r.len()), None));
            }
        });
        t
    } else {
        raw.map(|t| ozip::compress_with(&t, table_level(level, t.len()), None))
    };
    u.sbytes[0] += tables[0].len() + tables[1].len();
    u.sbytes[8] += tables[2].len() + tables[3].len();
    u.ts += tg.elapsed().as_secs_f64();
    prof_add(22, tg);
    u.wall = t0.elapsed().as_secs_f64(); // (wall clock: the blocks and the tables)
    let mut all = all;
    let tk = Tick::now();
    let preps: Vec<Vec<Option<fz::Prep>>> = preps.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let bank = if all.len() > 1 && !env_on("FL_NOBANK") { table_bank(&mut all, &preps) } else { Vec::new() };
    drop(preps);
    u.tcd += tk.elapsed().as_secs_f64();
    prof_add(30, tk);
    let mut cu = CodedUnit { nl, derive: any_derive, tables, blocks: Vec::with_capacity(all.len()), bank };
    for (bi, (p, mut coded, a1, a2)) in all.into_iter().enumerate() {
        u.ts += a1;
        u.tcd += a2;
        u.nblocks += 1;
        u.ngrouped += !p.streams[9].is_empty() as usize;
        if p.streams[9].is_empty() {
            coded[9].clear();
        }
        if p.streams[10].is_empty() {
            coded[10].clear();
        }
        for k in 1..=10 {
            u.sbytes[k] += if k == 8 { 0 } else { coded[k].len() };
        }
        let bsize = blocks[bi].iter().map(|l| l.len() + 1).sum::<usize>();
        cu.blocks.push((p.lines, bsize, coded));
    }
    cu
}

/// The unit's table bank (fz::bank_frames) from its blocks' fz streams: their frames replaced
/// where coding them again with it is smaller. Returns the bank's bytes (empty for none).
fn table_bank(all: &mut [(Packed, Vec<Vec<u8>>, f64, f64)], preps: &[Vec<Option<fz::Prep>>]) -> Vec<u8> {
    let mut fr: Vec<(&mut Vec<u8>, usize, &fz::Prep)> = Vec::new();
    for (a, ps) in all.iter_mut().zip(preps) {
        for (k, (f, p)) in a.1.iter_mut().zip(ps).enumerate() {
            if let Some(p) = p {
                fr.push((f, k, p));
            }
        }
    }
    fz::bank_frames(&mut fr)
}

/// Pack a unit (its lines; `expect`: the bytes they are, which opening must give back), then
/// open it from the coded bytes only (plus the kit), one core.
fn pack_unit(data: &[u8], lines: &[&[u8]], expect: &[u8], level: u32, threads: usize, one_max: usize, kit: Option<&Kit>) -> Unit {
    let mut u = Unit::default();
    let pc = if clock::cnt() { clock::proc_counts() } else { (0, 0) };
    let cu = pack_coded(data, lines, expect.ends_with(b"\n"), level, threads, one_max, kit, &mut u);
    u.out = cu.bytes();
    prof_proc(60, pc);
    if env_on("FL_HASH") {
        // developer: a checksum (FNV-1a 64) of every packed stream with its length, in order
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |b: &[u8]| {
            for &c in (b.len() as u64).to_le_bytes().iter().chain(b.iter()) {
                h = (h ^ c as u64).wrapping_mul(0x0100_0000_01b3);
            }
        };
        eat(&[cu.nl as u8, cu.derive as u8]);
        for t in &cu.tables {
            eat(t);
        }
        if !cu.bank.is_empty() {
            eat(&cu.bank);
        }
        for (lines, size, coded) in &cu.blocks {
            eat(&(*lines as u64).to_le_bytes());
            eat(&(*size as u64).to_le_bytes());
            for c in coded {
                eat(c);
            }
        }
        eprintln!("HASH {} {:016x}", u.out, h);
    }
    let mut back = Vec::new();
    let pc = if clock::cnt() { clock::proc_counts() } else { (0, 0) };
    let r = open_unit(&cu, kit, &mut back, &mut u.topen);
    prof_proc(61, pc);
    if let Err(e) = r {
        eprintln!("open error: {}", e);
    }
    u.exact = r.is_ok() && back == expect;
    drop(back);
    if threads > 1 {
        // all cores: open again, the blocks side by side (wall clock), and check it too
        let tw = Instant::now();
        let pc = if clock::cnt() { clock::proc_counts() } else { (0, 0) };
        let r2 = open_unit_par(&cu, kit, threads);
        prof_proc(62, pc);
        u.owall = tw.elapsed().as_secs_f64();
        match r2 {
            Ok(b) => u.exact &= b == expect,
            Err(e) => {
                eprintln!("all-core open error: {}", e);
                u.exact = false;
            }
        }
    }
    if let Some(n) = crate::engine::fl::no_env::var("FL_DAMAGE").ok().and_then(|x| x.parse::<usize>().ok()) {
        damage_test(&cu, kit, expect, n);
    }
    u
}

/// FL_DAMAGE=n (test): open the packed unit n times, each time with one damage (a byte changed
/// in one of its streams or tables, a stream cut short, a block's line count or size off):
/// the opener must give an error or some bytes, never crash. Prints how often each happened.
fn damage_test(cu: &CodedUnit, kit: Option<&Kit>, expect: &[u8], n: usize) {
    let prev = std::panic::take_hook();
    // (where the guarded index errors happen: printed at the end)
    static WHERE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    std::panic::set_hook(Box::new(|info| {
        if let Some(l) = info.location() {
            WHERE.lock().unwrap().push(format!("{}:{}", l.file(), l.line()));
        }
    }));
    let (mut err, mut caught, mut same, mut diff) = (0usize, 0usize, 0usize, 0usize);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ expect.len() as u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut msgs: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for _ in 0..n {
        let mut d = CodedUnit { nl: cu.nl, derive: cu.derive, tables: cu.tables.clone(), blocks: cu.blocks.clone(), bank: cu.bank.clone() };
        // pick a piece: a table, or a block's stream (or its line count or size)
        let nb = d.blocks.len();
        // (pick 4: the table bank)
        let pick = rnd() as usize % (5 + nb * 9);
        let how = rnd() % 8;
        let (b, k) = (pick.saturating_sub(5) / 9, pick.saturating_sub(5) % 9 + 1);
        if how == 7 && pick >= 5 {
            let e = &mut d.blocks[b];
            let delta = (rnd() % 5) as usize + 1;
            if rnd() % 2 == 0 {
                e.0 = e.0.saturating_sub(delta).max(1);
            } else {
                e.1 += delta;
            }
        } else {
            let piece: &mut Vec<u8> = if pick < 4 {
                &mut d.tables[pick]
            } else if pick == 4 {
                &mut d.bank
            } else {
                &mut d.blocks[b].2[k]
            };
            if !piece.is_empty() {
                let at = rnd() as usize % piece.len();
                match how {
                    0..=4 => piece[at] ^= 1 << (rnd() % 8), // one bit
                    5 => piece[at] = rnd() as u8,          // one byte
                    _ => piece.truncate(at),                // cut short
                }
            }
        }
        let mut back = Vec::new();
        let mut t = 0f64;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_unit(&d, kit, &mut back, &mut t)));
        match r {
            Ok(Ok(())) => {
                if back == expect { same += 1 } else { diff += 1 }
            }
            Ok(Err(e)) => {
                if e.contains("caught") { caught += 1 } else { err += 1 }
                *msgs.entry(e).or_insert(0) += 1;
            }
            Err(_) => {
                eprintln!("DAMAGE: the opener panicked outside its guard");
                std::process::exit(5);
            }
        }
    }
    std::panic::set_hook(prev);
    let mut wh: std::collections::BTreeMap<String, usize> = Default::default();
    for w in WHERE.lock().unwrap().drain(..) {
        *wh.entry(w).or_insert(0) += 1;
    }
    eprintln!("DAMAGE index errors at {:?}", wh);
    eprintln!("DAMAGE {} tries: error {} (caught index errors {}), opened same {}, opened other bytes {} | {:?}", n, err + caught, caught, same, diff, msgs);
}

/// FL_KFUZZ=n FILES (test of the keyed decoders, entity.rs): each file is packed as one block
/// (its first 20,000 lines); a block with keyed slots is opened n times, each time with one
/// damage aimed at keyed coding: the plan cut short or grown, its memory size, slot count, a
/// slot step or a distance changed (a key outside its pattern, counts that cross), the keyed
/// flag cleared or set, a column mode given keyed bits (with a list, groups or second
/// differences, steps without keyed codes), a keyed code changed (a rank the entity does not
/// have, a step without an entity, an unknown code), the line count off. The opener must
/// refuse (an error, or a guarded panic of a check) or give bytes: never crash or hang.
fn kfuzz(files: &[String]) {
    let n: usize = crate::engine::fl::no_env::var("FL_KFUZZ").ok().and_then(|x| x.parse().ok()).unwrap_or(1000);
    for f in files {
        let data = std::fs::read(f).unwrap();
        let body = data.strip_suffix(b"\n").unwrap_or(&data);
        let mut lines: Vec<&[u8]> = body.split(|&c| c == b'\n').take(20_000).collect();
        lines.retain(|l| l.len() < 60_000);
        let size: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut buf = Vec::with_capacity(size + 64);
        for l in &lines {
            buf.extend_from_slice(l);
            buf.push(b'\n');
        }
        buf.extend_from_slice(&[0u8; 64]);
        let lines: Vec<&[u8]> = { let b = &buf[..size]; b.strip_suffix(b"\n").unwrap_or(b).split(|&c| c == b'\n').collect() };
        let p = pack(&buf, &lines, 1);
        // where the plan is: after the rules section (as heads::read finds it)
        let h = &p.streams[2];
        let mut q = 0;
        heads::rd(h, &mut q);
        let flags = h[q + 1];
        q += 2;
        if flags & entity::FLAG == 0 {
            eprintln!("KFUZZ {}: no keyed slots", f);
            continue;
        }
        if flags & 2 != 0 {
            let k = heads::rd(h, &mut q).unwrap();
            for _ in 0..k {
                heads::rd(h, &mut q);
            }
        }
        if flags & 16 != 0 {
            let (col_of, first) = slot_columns(&p.streams[0]);
            let _ = col_of;
            rules::get_rules(h, &mut q, first.len() - 1);
            heads::rd(h, &mut q);
        }
        let pa = q;
        let (_, first) = slot_columns(&p.streams[0]);
        let mut pe = pa;
        entity::Plan::read(h, &mut pe, &first).unwrap();
        let mut seed = 0x2545_F491_4F6C_DD1Du64 ^ size as u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let expect = &buf[..size];
        let prev = std::panic::take_hook();
        static AT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        std::panic::set_hook(Box::new(|info| {
            if let Some(l) = info.location() {
                AT.lock().unwrap().push(format!("{}:{}", l.file(), l.line()));
            }
        }));
        let (mut same, mut other, mut err, mut caught) = (0usize, 0usize, 0usize, 0usize);
        let mut kinds = [0usize; 10];
        for _ in 0..n {
            let mut d = Packed { streams: p.streams.clone(), lines: p.lines, raw: p.raw, alt: Vec::new(), pre: Vec::new(), mk: p.mk };
            let how = (rnd() % 10) as usize;
            kinds[how] += 1;
            let h = &mut d.streams[2];
            let at = pa + rnd() as usize % (pe - pa);
            match how {
                // the plan cut short (the rest of the header moves up)
                0 => {
                    let k = 1 + rnd() as usize % (pe - at);
                    h.drain(at..at + k);
                }
                // a byte of the plan changed (memory size, count, step or distance)
                1 => h[at] = rnd() as u8,
                // a varint grown inside the plan (oversized counts, steps, distances)
                2 => {
                    let x = [0xff, 0xff, 0xff, 0x7f];
                    h.splice(at..at, x[..1 + rnd() as usize % 4].iter().copied());
                }
                // the memory size out of range, or the slot count past the slots
                3 => h[pa] = [0, 7, 15, 64, 255][rnd() as usize % 5],
                // the keyed flag cleared
                4 => h[pa - (pa - 0).min(pa)] ^= 0,
                // a byte after the plan (column modes, kinds) given keyed or other bits
                5 => {
                    if h.len() > pe {
                        let k = pe + rnd() as usize % (h.len() - pe);
                        h[k] |= [0x80u8, 0x08, 0x10, 0x20, 0x40, 0x01][rnd() as usize % 6];
                    }
                }
                // keyed codes: ranks past the entity's values, steps, unknown codes
                6 | 7 => {
                    let c = &mut d.streams[6];
                    if !c.is_empty() {
                        for _ in 0..1 + rnd() % 8 {
                            let k = rnd() as usize % c.len();
                            c[k] = (CK - 1 + rnd() as usize % (entity::KN - CK + 4)) as u8;
                        }
                    }
                }
                // the numbers cut short
                8 => {
                    let s = &mut d.streams[if rnd() % 2 == 0 { 4 } else { 7 }];
                    let k = rnd() as usize % (s.len() + 1);
                    s.truncate(k);
                }
                // the line count off
                _ => d.lines = (d.lines as i64 + [-3i64, -1, 1, 5][rnd() as usize % 4]).max(1) as usize,
            }
            if how == 4 {
                // (the flags byte: keyed flag cleared, or an unknown bit set)
                let mut q = 0;
                heads::rd(&d.streams[2], &mut q);
                d.streams[2][q + 1] ^= if rnd() % 2 == 0 { entity::FLAG } else { 128 };
            }
            let mut out = vec![0u8; size + 64];
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open::unpack_into(&mut d, &mut out, 0, size)));
            match r {
                Ok(Ok(e)) => {
                    if e == size && &out[..e] == expect { same += 1 } else { other += 1 }
                }
                Ok(Err(_)) => err += 1,
                Err(_) => caught += 1,
            }
        }
        std::panic::set_hook(prev);
        let mut wh: std::collections::BTreeMap<String, usize> = Default::default();
        for w in AT.lock().unwrap().drain(..) {
            *wh.entry(w).or_insert(0) += 1;
        }
        eprintln!("KFUZZ panics at {:?}", wh);
        eprintln!("KFUZZ {}: {} tries {:?}: refused {} (errors), stopped by a check or index {} (guarded panics), opened same {}, opened other bytes {}", f.rsplit('/').next().unwrap_or(f), n, kinds, err, caught, same, other);
    }
}

/// FL_KIT=1: the starter-kit test (as in the kits idea): a kit is learned from the first 30%
/// of each log's lines; the other 70% is packed whole (blocks as usual), in 10,000-line pieces
/// and in 1,000-line pieces, each piece packed alone, without and with the kit.
fn kit_test(level: u32, threads: usize, one_max: usize, files: &[String]) {
    let cases = [("whole", 0usize), ("p10k", 10_000), ("p1k", 1_000)];
    // per case and side: ratios per log, total in and out, exact count
    let mut ratios = vec![vec![Vec::new(); 2]; 3];
    let mut tot = vec![[(0usize, 0usize); 2]; 3];
    let mut nexact = vec![[0usize; 2]; 3];
    let (mut kbytes, mut tl) = (0usize, 0f64);
    for f in files {
        let data = std::fs::read(f).unwrap();
        let body = data.strip_suffix(b"\n").unwrap_or(&data);
        let lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
        let n0 = lines.len() * 3 / 10;
        let t = Instant::now();
        let kit = learn_kit(&data, &lines[..n0], one_max);
        tl += t.elapsed().as_secs_f64();
        kbytes += kit.bytes();
        let test = &lines[n0..];
        let mut row = format!("{:<24} kit {} patterns {} words {} B |", std::path::Path::new(f).file_name().unwrap().to_string_lossy(), kit.pats.len(), kit.words.len(), kit.bytes());
        for (ci, &(name, size)) in cases.iter().enumerate() {
            let pieces: Vec<&[&[u8]]> = if size == 0 { vec![test] } else { test.chunks(size).collect() };
            for side in 0..2 {
                let (mut i, mut o, mut ex) = (0usize, 0usize, true);
                for pc in &pieces {
                    let mut expect = Vec::with_capacity(pc.iter().map(|l| l.len() + 1).sum());
                    for l in pc.iter() {
                        expect.extend_from_slice(l);
                        expect.push(b'\n');
                    }
                    let u = pack_unit(&data, pc, &expect, level, threads, one_max, if side == 1 { Some(&kit) } else { None });
                    i += expect.len();
                    o += u.out;
                    ex &= u.exact;
                }
                ratios[ci][side].push(i as f64 / o as f64);
                tot[ci][side].0 += i;
                tot[ci][side].1 += o;
                nexact[ci][side] += ex as usize;
                row += &format!(" {} {} {:.2}x{}", name, if side == 1 { "kit" } else { "nokit" }, i as f64 / o as f64, if ex { "" } else { " NOT EXACT" });
            }
        }
        println!("{}", row);
    }
    for (ci, &(name, _)) in cases.iter().enumerate() {
        for side in 0..2 {
            let r = &ratios[ci][side];
            println!(
                "KIT {:5} {:5}: average {:.2}x per log, total {:.2}x, exact {}/{}",
                name,
                if side == 1 { "kit" } else { "nokit" },
                r.iter().sum::<f64>() / r.len() as f64,
                tot[ci][side].0 as f64 / tot[ci][side].1 as f64,
                nexact[ci][side],
                r.len()
            );
        }
    }
    println!("KIT bytes (not counted in the output) {} for {} logs; learning {:.2} s", kbytes, files.len(), tl);
}

/// split_lines on `threads` cores (the same lines): the buffer in parts that end after a '\n';
/// each part's line ends are counted, then each part writes its lines into its place.
fn split_lines_par(b: &[u8], threads: usize) -> Vec<&[u8]> {
    let want = threads.min(16);
    // part k: b[cut[k]..cut[k + 1]], each but the last ending right after a '\n'
    let mut cut = vec![0usize];
    for k in 1..want {
        let mut c = (b.len() * k / want).max(*cut.last().unwrap());
        while c < b.len() && b[c] != b'\n' {
            c += 1;
        }
        if c >= b.len() {
            break;
        }
        if c + 1 > *cut.last().unwrap() {
            cut.push(c + 1);
        }
    }
    cut.push(b.len());
    let parts = cut.len() - 1;
    let mut lines_of: Vec<Vec<&[u8]>> = (0..parts).map(|_| Vec::new()).collect();
    std::thread::scope(|sc| {
        for (k, out) in lines_of.iter_mut().enumerate() {
            let (a, e) = (cut[k], cut[k + 1]);
            sc.spawn(move || {
                let mut v = split_lines(&b[a..e]);
                if k + 1 < parts {
                    v.pop(); // (the empty piece after the part's last '\n')
                }
                *out = v;
            });
        }
    });
    let total: usize = lines_of.iter().map(|v| v.len()).sum();
    let mut all: Vec<&[u8]> = Vec::with_capacity(total);
    for v in &lines_of {
        all.extend_from_slice(v);
    }
    all
}

/// The lines of a buffer (without their '\n'): 16 bytes per step with NEON (an exact byte
/// compare; x86_64: 64 then 16 bytes per step with SSE2 or AVX2, x86.rs), else 8 bytes per step
/// with an exact zero-byte test.
fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::with_capacity(b.len() / 64 + 16);
    let (mut s, mut i) = (0usize, 0usize);
    let n = b.len();
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        // SAFETY: NEON is always there on aarch64; loads stay inside b (i + 16 <= n).
        unsafe {
            let nl = vdupq_n_u8(b'\n');
            let p = b.as_ptr();
            while i + 16 <= n {
                let eq = vceqq_u8(vld1q_u8(p.add(i)), nl);
                // 4 bits per byte (narrowing shift), one kept: bit 4k + 3 = byte k is '\n'
                let mut m = vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(eq), 4)), 0) & 0x8888_8888_8888_8888;
                while m != 0 {
                    let e = i + (m.trailing_zeros() >> 2) as usize;
                    lines.push(&b[s..e]);
                    s = e + 1;
                    m &= m - 1;
                }
                i += 16;
            }
        }
    }
    #[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
    {
        // SAFETY: each_nl reads b[k..k + 64] or b[k..k + 16] only while that end is <= n =
        // b.len(), from k = i = 0
        unsafe {
            x86::each_nl(b.as_ptr(), &mut i, n, |e| {
                lines.push(&b[s..e]);
                s = e + 1;
            })
        };
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        const LO: u64 = 0x0101_0101_0101_0101;
        const HI: u64 = 0x8080_8080_8080_8080;
        while i + 8 <= n {
            let x = u64::from_le_bytes(b[i..i + 8].try_into().unwrap()) ^ (LO * b'\n' as u64);
            // exact zero-byte test (no borrow between bytes: a 0x0B after a newline is not one)
            let mut z = !(((x & !HI).wrapping_add(!HI)) | x | !HI);
            while z != 0 {
                let p = i + (z.trailing_zeros() / 8) as usize;
                lines.push(&b[s..p]);
                s = p + 1;
                z &= z - 1;
            }
            i += 8;
        }
    }
    for p in i..n {
        if b[p] == b'\n' {
            lines.push(&b[s..p]);
            s = p + 1;
        }
    }
    lines.push(&b[s..]);
    lines
}

/// The product setting: fz level 43 (see the notes at the top).
const DEFAULT_LEVEL: u32 = 43;


/// A stable sort in little code: `less(a, b)` says a goes before b (as sort_by's comparison
/// giving Less). A stable sort's result is fixed by the comparison, so this orders exactly as
/// `sort_by` / `sort_by_key` do, in a few hundred bytes per use instead of the standard
/// library's stable or unstable sort (about 4-8 KB of code for each element type and
/// comparison). Insertion sort on runs of 16, then merges through one scratch copy.
pub fn sort_stable_by<T: Copy, F: FnMut(&T, &T) -> bool>(v: &mut [T], mut less: F) {
    const RUN: usize = 16;
    let n = v.len();
    let mut s = 0;
    while s < n {
        let e = (s + RUN).min(n);
        for i in s + 1..e {
            let x = v[i];
            let mut j = i;
            while j > s && less(&x, &v[j - 1]) {
                v[j] = v[j - 1];
                j -= 1;
            }
            v[j] = x;
        }
        s = e;
    }
    if n <= RUN {
        return;
    }
    let mut buf = v.to_vec();
    let mut in_buf = false;
    let mut w = RUN;
    while w < n {
        let (a, b): (&[T], &mut [T]) = if in_buf { (&buf[..], &mut v[..]) } else { (&v[..], &mut buf[..]) };
        let mut s = 0;
        while s < n {
            let m = (s + w).min(n);
            let e = (s + 2 * w).min(n);
            let (mut i, mut j, mut k) = (s, m, s);
            while i < m && j < e {
                if less(&a[j], &a[i]) {
                    b[k] = a[j];
                    j += 1;
                } else {
                    b[k] = a[i];
                    i += 1;
                }
                k += 1;
            }
            b[k..k + m - i].copy_from_slice(&a[i..m]);
            b[j..e].copy_from_slice(&a[j..e]);
            s = e;
        }
        in_buf = !in_buf;
        w *= 2;
    }
    if in_buf {
        v.copy_from_slice(&buf);
    }
}

/// sort_stable_by for the sorts off the hot paths (job and thread orders, folder names): one
/// shared copy of the code, the comparison called through a reference.
#[inline(never)]
pub fn sort_stable_idx(v: &mut [usize], less: &mut dyn FnMut(&usize, &usize) -> bool) {
    sort_stable_by(v, less);
}
