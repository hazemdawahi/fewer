// Derived fields: exact per-format rules found in real logs, as a text pass around the engine.
// Packing: a file's lines are rewritten, block by block, without the parts that follow from
// other parts of the line or from earlier lines; a 2-byte marker (MK + a letter) stands where
// the opener must put something back. Opening: the engine gives back the rewritten text and
// `Opener::inverse` puts the parts back, marker by marker (the text between markers is copied
// as it is). A rule is switched on per file when it holds on a sample of the file's lines
// (`detect`) and is checked on every value: a value it does not fit stays in the text as it is.
// The rules' state (lists, last values) goes on from block to block (the opener opens the blocks
// in order). A block holding an MK byte of its own is packed as it is.
//
// Rules (marker letters):
//  logcat  a        logcat threadtime: PID and TID are right-aligned in 5 columns ('%5d'), so
//                   the spaces before them follow from their digit counts
//  bgl     b B      'LABEL EPOCH DATE NODE TIMESTAMP NODE ...': EPOCH = the TIMESTAMP read as
//                   local time + a fixed offset (b: the most common on the sample, B: the second,
//                   as for summer time), DATE = its date, the second NODE = the first
//  hdfs    k h H i I q  an IP in a line with a block id that is already in that block's list of
//                   IPs (first-seen order, up to 8) is written as its place in the list (h; H: a
//                   new IP, added to the list; i / I: the same for the line's first IP when its
//                   block id comes later, so the opener looks ahead; k: the line's block id);
//                   'src: /IP:PORT' ports in 32768..60999 (Linux ephemeral ports, looking random)
//                   are stored flat, 15 bits each, in the side stream (q)
//  spark   s r u f  Spark task protocol: 'Running task' takes the TID of the last 'Got assigned
//                   task' (s: an assigned task; r: a running task, its TID left out when it is
//                   the last assigned one, u + k when it is the k-th newest one); 'Finished task'
//                   names a running task by its place in the list of running tasks (f; oldest
//                   first, entries skipped by 16 finished tasks are dropped)
//  proxy   o c l    Proxifier: a close line names its open line by its place in the list of
//                   pending opens (c; newest first); app and host come from that open, the
//                   lifetime from the two clock times plus a residual of -2..2 s (l); o: an open
//  float   g        'KEY is : F' at a line end, F a float32 printed shortest (as Java's
//                   Float.toString): the difference of its bits from the last float of KEY
//  oldnew  w x      'type=T,NEW,old=OLD': OLD is the last NEW of type T (w: such a field; x:
//                   OLD left out)
//  ctime   t        a syslog line ending with a ctime() copy of its own clock time: weekday and
//                   time follow from the header, only the year stays
//  rdns    n        a reverse-DNS name spelling an IPv4 of its line with dashes, then its domain
//                   ("dslb-084-057-172-016.pools..." for 84.57.172.16 before it, "191-210-223-
//                   172.user..." for a later [191.210.223.172]): the 4 numbers become a marker
//                   and 3 letters (the last IP before / the first after, in order / reversed,
//                   zero-padded / plain); it may share a line with ctime (rdns first)
// One letter per place where the following columns should stay shared: the engine shares a
// column between patterns with the same text before it (so Spark's two Running forms keep one
// letter and differ after it).
//
// Side stream of a block (empty: no rules): varint rule bits, varint size of the rewritten
// text, [bgl: 2 zigzag offsets], [ports: varint count, then 15 bits per port]. Format 2: bit
// 8 with bgl and without hdfs (the ports bit, R_BGL0) marks a block whose bgl lines are in
// the old form (no fill marker, fix3); it adds nothing to the side stream.
//
// Switches (testing only; the default is every rule on, detected per file): FL_NODERIVE=1 turns
// all off (output then as without this module); FL_DERIVE=name,name,... keeps only the named
// rules (names above, "hdfsip": hdfs without the flat ports); FL_DERIVE_DUMP=file writes the
// rewritten text; FL_DERIVE_BENCH=1 times and checks the two passes alone (2: the rewrite only).

use super::split::wild_copy;
use super::{get_varint, put_varint, unzz, zz, FxMap};

/// The marker byte (a block that has one of its own is not rewritten).
pub const MK: u8 = 1;

const R_LOGCAT: u32 = 1;
const R_BGL: u32 = 2;
const R_HDFS: u32 = 4;
const R_PORTS: u32 = 8; // with R_HDFS: flat client ports
const R_SPARK: u32 = 16;
const R_PROXY: u32 = 32;
const R_FLOAT: u32 = 64;
const R_OLDNEW: u32 = 128;
const R_CTIME: u32 = 256;
const R_RDNS: u32 = 512;
/// Format 2, per block (fix3): the ports bit without the hdfs bit, which no rules set (ports
/// go with hdfs), with bgl marks a block whose bgl lines are in the old form: no fill marker
/// (B5's form off for that block, so its line printer is not used either). The packer
/// writes it for a block it packed both ways when the old form is smaller (engine/mod.rs
/// pack_lines).
const R_BGL0: u32 = R_PORTS;

/// B5 (computed form, format 2): a bgl line's marker stands right after its timestamp (the
/// epoch and the date are put in front of the node from it, the second node after it: every
/// derived part follows from text written before the marker), and a block whose markers are
/// all bgl / ctime markers in the patterns' fixed text (never inside a value) has 0 as its
/// text size in the side stream: the line printer puts the derived parts in as it writes each
/// line (open.rs), with no put-back pass. FL_B5=0 (testing only, both sides) keeps the old
/// form and the put-back pass.
/// (Read where it is used, once per file or block: in fewer-pack, which reads no switch, it is a
/// constant, and the old form's code is left out.)
#[inline(always)]
pub fn b5_on() -> bool {
    crate::engine::fl::no_env::var_os("FL_B5").is_none_or(|v| v != "0")
}

/// Rule names for FL_DERIVE ("hdfsip": hdfs without the flat ports).
const NAMES: [(&str, u32); 10] = [("logcat", R_LOGCAT), ("bgl", R_BGL), ("hdfs", R_HDFS | R_PORTS), ("hdfsip", R_HDFS), ("spark", R_SPARK), ("proxy", R_PROXY), ("float", R_FLOAT), ("oldnew", R_OLDNEW), ("ctime", R_CTIME), ("rdns", R_RDNS)];

/// The rules on for a file and their settings.
#[derive(Clone, Copy, Default)]
pub struct Rules {
    pub bits: u32,
    bgl_off: [i64; 2],
    comp: bool, // B5's computed form (b5_on)
}

impl Rules {
    /// These rules with the bgl lines in the old form (fix3: R_BGL0 in the side stream), for
    /// a block packed both ways; None unless bgl is on, in B5's form, and hdfs off (R_BGL0 is
    /// its ports bit).
    pub fn old_bgl(&self) -> Option<Rules> {
        (self.comp && self.bits & (R_BGL | R_HDFS) == R_BGL).then_some(Rules { comp: false, ..*self })
    }

    /// The rule bits a block's side stream starts with: with R_BGL0 for bgl lines in the old
    /// form while B5 is on (old_bgl).
    fn side_bits(&self) -> u64 {
        (self.bits | if !self.comp && self.bits & (R_BGL | R_HDFS) == R_BGL && b5_on() { R_BGL0 } else { 0 }) as u64
    }
}

// ---------------------------------------------------------------------------------------
// small helpers

/// Nibble mask of the 16 bytes at h[i..] equal to b (4 bits per byte).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn eq16(h: &[u8], i: usize, b: u8) -> u64 {
    use std::arch::aarch64::*;
    let s = &h[i..i + 16];
    // SAFETY: NEON is always present on aarch64; the load reads the 16 bytes of s.
    unsafe {
        let m = vceqq_u8(vld1q_u8(s.as_ptr()), vdupq_n_u8(b));
        vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(m), 4)), 0)
    }
}

/// The same with SSE2 (x86_64, x86.rs): the same nibble mask.
#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[inline(always)]
fn eq16(h: &[u8], i: usize, b: u8) -> u64 {
    let s = &h[i..i + 16];
    // SAFETY: the load reads the 16 bytes of s
    unsafe { super::x86::eq16(s.as_ptr(), b) }
}

#[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
#[inline(always)]
fn eq16(h: &[u8], i: usize, b: u8) -> u64 {
    let mut m = 0u64;
    for (k, &c) in h[i..i + 16].iter().enumerate() {
        m |= ((c == b) as u64 * 15) << (4 * k);
    }
    m
}

/// The first place of byte b in h.
#[inline]
pub fn find_byte(h: &[u8], b: u8) -> Option<usize> {
    let n = h.len();
    let mut i = 0;
    while i + 16 <= n {
        let m = eq16(h, i, b);
        if m != 0 {
            return Some(i + (m.trailing_zeros() / 4) as usize);
        }
        i += 16;
    }
    h[i..].iter().position(|&c| c == b).map(|p| i + p)
}

/// find_byte, always inlined.
#[inline(always)]
fn find_byte_inl(h: &[u8], b: u8) -> Option<usize> {
    let n = h.len();
    let mut i = 0;
    while i + 16 <= n {
        let m = eq16(h, i, b);
        if m != 0 {
            return Some(i + (m.trailing_zeros() / 4) as usize);
        }
        i += 16;
    }
    h[i..].iter().position(|&c| c == b).map(|p| i + p)
}

/// The first place of byte b in h[i..end], or end; h must have 16 bytes after `end`.
#[inline(always)]
fn next_byte(h: &[u8], mut i: usize, end: usize, b: u8) -> usize {
    while i < end {
        let m = eq16(h, i, b);
        if m != 0 {
            return (i + (m.trailing_zeros() / 4) as usize).min(end);
        }
        i += 16;
    }
    end
}

/// Whether h holds byte b (64 bytes at a time: SSE2, or AVX2 when the processor has it; x86.rs).
#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
pub fn has_byte(h: &[u8], b: u8) -> bool {
    super::x86::has_byte(h, b)
}

/// Whether h holds byte b (128 bytes at a time: the compares ORed together, one test).
#[cfg(not(all(target_arch = "x86_64", not(fewer_simd = "scalar"))))]
pub fn has_byte(h: &[u8], b: u8) -> bool {
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: NEON is always present on aarch64; each step loads h[i..i + 128], i + 128 <= len
    unsafe {
        use std::arch::aarch64::*;
        let (p, v) = (h.as_ptr(), vdupq_n_u8(b));
        while i + 128 <= h.len() {
            let (x, y) = (vld1q_u8_x4(p.add(i)), vld1q_u8_x4(p.add(i + 64)));
            let e = |a, c| vorrq_u8(vceqq_u8(a, v), vceqq_u8(c, v));
            let m = vorrq_u8(vorrq_u8(e(x.0, x.1), e(x.2, x.3)), vorrq_u8(e(y.0, y.1), e(y.2, y.3)));
            if vmaxvq_u8(m) != 0 {
                return true;
            }
            i += 128;
        }
    }
    while i + 64 <= h.len() {
        if eq16(h, i, b) | eq16(h, i + 16, b) | eq16(h, i + 32, b) | eq16(h, i + 48, b) != 0 {
            return true;
        }
        i += 64;
    }
    h[i..].contains(&b)
}

/// The first place of `needle` (2+ bytes) in h: places where its first and last bytes are,
/// 16 at a time, then the bytes between.
#[inline]
fn find_seq(h: &[u8], needle: &[u8]) -> Option<usize> {
    let k = needle.len() - 1;
    let n = h.len();
    let mut i = 0;
    while i + k + 16 <= n {
        let mut m = eq16(h, i, needle[0]) & eq16(h, i + k, needle[k]);
        while m != 0 {
            let t = m.trailing_zeros();
            let p = i + (t / 4) as usize;
            if h[p + 1..p + k] == needle[1..k] {
                return Some(p);
            }
            m &= !(15u64 << (t & !3));
        }
        i += 16;
    }
    while i + k < n {
        if h[i] == needle[0] && h[i + 1..=i + k] == needle[1..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// find_seq, always inlined (for a needle known where it is called); on aarch64, 64 places
/// at a time are first looked at as one (the places where the needle's first and last bytes
/// are, OR-ed, one horizontal max), and only 64 places with such a pair go through the
/// 16-place steps of find_seq. The same first place.
#[inline(always)]
fn find_seq_inl(h: &[u8], needle: &[u8]) -> Option<usize> {
    let k = needle.len() - 1;
    let n = h.len();
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let p = h.as_ptr();
        while i + k + 64 <= n {
            // SAFETY: NEON is always present on aarch64; the loads read h[i..i + 64] and
            // h[i + k..i + k + 64], inside h (i + k + 64 <= n)
            let any = unsafe {
                let (f, l) = (vdupq_n_u8(needle[0]), vdupq_n_u8(needle[k]));
                let pair = |o: usize| vandq_u8(vceqq_u8(vld1q_u8(p.add(i + o)), f), vceqq_u8(vld1q_u8(p.add(i + k + o)), l));
                vmaxvq_u8(vorrq_u8(vorrq_u8(pair(0), pair(16)), vorrq_u8(pair(32), pair(48))))
            };
            if any != 0 {
                for o in [0, 16, 32, 48] {
                    let mut m = eq16(h, i + o, needle[0]) & eq16(h, i + o + k, needle[k]);
                    while m != 0 {
                        let t = m.trailing_zeros();
                        let q = i + o + (t / 4) as usize;
                        // (byte by byte: the needles are short)
                        if h[q + 1..q + k].iter().zip(&needle[1..k]).all(|(a, b)| a == b) {
                            return Some(q);
                        }
                        m &= !(15u64 << (t & !3));
                    }
                }
            }
            i += 64;
        }
    }
    #[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
    {
        // the same with SSE2 (x86.rs): 64 places at a time looked at as one
        let p = h.as_ptr();
        while i + k + 64 <= n {
            // SAFETY: pair_any64 reads h[i..i + k + 64], inside h (i + k + 64 <= n)
            if unsafe { super::x86::pair_any64(p.add(i), k, needle[0], needle[k]) } {
                for o in [0, 16, 32, 48] {
                    let mut m = eq16(h, i + o, needle[0]) & eq16(h, i + o + k, needle[k]);
                    while m != 0 {
                        let t = m.trailing_zeros();
                        let q = i + o + (t / 4) as usize;
                        // (byte by byte: the needles are short)
                        if h[q + 1..q + k].iter().zip(&needle[1..k]).all(|(a, b)| a == b) {
                            return Some(q);
                        }
                        m &= !(15u64 << (t & !3));
                    }
                }
            }
            i += 64;
        }
    }
    while i + k + 16 <= n {
        let mut m = eq16(h, i, needle[0]) & eq16(h, i + k, needle[k]);
        while m != 0 {
            let t = m.trailing_zeros();
            let p = i + (t / 4) as usize;
            if h[p + 1..p + k] == needle[1..k] {
                return Some(p);
            }
            m &= !(15u64 << (t & !3));
        }
        i += 16;
    }
    while i + k < n {
        if h[i] == needle[0] && h[i + 1..=i + k] == needle[1..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline(always)]
fn is_digit(c: u8) -> bool {
    c.wrapping_sub(b'0') < 10
}

/// The digits starting at b[i]: (end, value); end == i when there are none (value wraps past 19).
#[inline]
fn digits(b: &[u8], i: usize) -> (usize, u64) {
    let mut j = i;
    let mut v = 0u64;
    while j < b.len() && is_digit(b[j]) {
        v = v.wrapping_mul(10).wrapping_add((b[j] - b'0') as u64);
        j += 1;
    }
    (j, v)
}

/// The end of the digit run at l[i..] (8 bytes at a time).
#[inline]
fn digit_end(l: &[u8], mut i: usize) -> usize {
    while i + 8 <= l.len() {
        let x = u64::from_le_bytes(l[i..i + 8].try_into().unwrap());
        // a byte below '0' or above '9' gets its top bit set (the lowest such byte is exact)
        let nd = (x.wrapping_sub(0x3030_3030_3030_3030) | x.wrapping_add(0x4646_4646_4646_4646)) & 0x8080_8080_8080_8080;
        if nd != 0 {
            return i + (nd.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < l.len() && is_digit(l[i]) {
        i += 1;
    }
    i
}

/// A number written plainly: 1-19 digits, no leading zero (except "0").
#[inline]
fn plain_num(t: &[u8]) -> Option<u64> {
    if t.is_empty() || t.len() > 19 || (t.len() > 1 && t[0] == b'0') || !t.iter().all(|&c| is_digit(c)) {
        return None;
    }
    Some(t.iter().fold(0u64, |v, &c| v * 10 + (c - b'0') as u64))
}

/// "00" "01" ... "99"
static DIGITS2: [u8; 200] = {
    let mut t = [0u8; 200];
    let mut i = 0;
    while i < 100 {
        t[2 * i] = b'0' + (i / 10) as u8;
        t[2 * i + 1] = b'0' + (i % 10) as u8;
        i += 1;
    }
    t
};

/// x in decimal at out[o..] (out needs 20 bytes of room); returns the end.
#[inline]
fn put_u64(out: &mut [u8], o: usize, mut x: u64) -> usize {
    let mut b = [0u8; 20];
    let mut q = 20;
    while x >= 100 {
        let r = (x % 100) as usize * 2;
        x /= 100;
        q -= 2;
        b[q..q + 2].copy_from_slice(&DIGITS2[r..r + 2]);
    }
    if x >= 10 {
        q -= 2;
        b[q..q + 2].copy_from_slice(&DIGITS2[x as usize * 2..x as usize * 2 + 2]);
    } else {
        q -= 1;
        b[q] = b'0' + x as u8;
    }
    out[o..o + 20 - q].copy_from_slice(&b[q..]);
    o + 20 - q
}

/// The 8 decimal digits of x < 10^8 (leading zeros kept) as ASCII, first digit in the lowest
/// byte (4 + 4, 2 + 2, 1 + 1 digits in all lanes at once; the divisions are multiply-shifts).
#[inline(always)]
fn swar8(x: u64) -> u64 {
    let hi = x / 10000;
    let v = hi | (x - hi * 10000) << 32;
    let a = ((v * 10486) >> 20) & 0x0000_007f_0000_007f;
    let v = a | (v - a * 100) << 16;
    let c = ((v * 103) >> 10) & 0x000f_000f_000f_000f;
    (c | (v - c * 10) << 8) | 0x3030_3030_3030_3030
}

/// Decimal digits of x < 10^8 (1 for 0).
#[inline(always)]
fn len8(x: u64) -> usize {
    1 + (x >= 10) as usize + (x >= 100) as usize + (x >= 1000) as usize + (x >= 10_000) as usize
        + (x >= 100_000) as usize + (x >= 1_000_000) as usize + (x >= 10_000_000) as usize
}

/// put_u64 with 8-digit SWAR stores, for the opener's put-back: writes up to 8 bytes past a
/// short number's end (the put-back's out has 64 bytes of room after the block; a damaged
/// block only meets a bounds check). The same digits as put_u64.
#[inline(always)]
fn put_dec(out: &mut [u8], o: usize, x: u64) -> usize {
    if x < 100_000_000 {
        let n = len8(x);
        out[o..o + 8].copy_from_slice(&(swar8(x) >> (8 * (8 - n))).to_le_bytes());
        o + n
    } else if x < 10_000_000_000_000_000 {
        let hi = x / 100_000_000;
        let n = len8(hi);
        out[o..o + 8].copy_from_slice(&(swar8(hi) >> (8 * (8 - n))).to_le_bytes());
        out[o + n..o + n + 8].copy_from_slice(&swar8(x - hi * 100_000_000).to_le_bytes());
        o + n + 8
    } else {
        put_u64(out, o, x)
    }
}

fn push_u64(v: &mut Vec<u8>, x: u64) {
    let mut b = [0u8; 20];
    let e = put_u64(&mut b, 0, x);
    v.extend_from_slice(&b[..e]);
}

fn push_i64(v: &mut Vec<u8>, x: i64) {
    if x < 0 {
        v.push(b'-');
    }
    push_u64(v, x.unsigned_abs());
}

#[inline(always)]
fn put(out: &mut [u8], o: usize, s: &[u8]) -> usize {
    out[o..o + s.len()].copy_from_slice(s);
    o + s.len()
}

/// Copy src to dst[..src.len()] with a few fixed-size moves (the range's two ends covered by
/// overlapping 16-, 8- or 4-byte moves; no call for short copies). Reads only src and writes
/// only dst[..src.len()]. SAFETY: dst has room for src.len() bytes and does not overlap src.
#[inline(always)]
unsafe fn copy_short(dst: *mut u8, src: &[u8]) {
    let (n, s) = (src.len(), src.as_ptr());
    unsafe {
        if n >= 16 {
            let mut k = 0;
            while k + 16 < n {
                (dst.add(k) as *mut [u8; 16]).write_unaligned((s.add(k) as *const [u8; 16]).read_unaligned());
                k += 16;
            }
            (dst.add(n - 16) as *mut [u8; 16]).write_unaligned((s.add(n - 16) as *const [u8; 16]).read_unaligned());
        } else if n >= 8 {
            (dst as *mut [u8; 8]).write_unaligned((s as *const [u8; 8]).read_unaligned());
            (dst.add(n - 8) as *mut [u8; 8]).write_unaligned((s.add(n - 8) as *const [u8; 8]).read_unaligned());
        } else if n >= 4 {
            (dst as *mut [u8; 4]).write_unaligned((s as *const [u8; 4]).read_unaligned());
            (dst.add(n - 4) as *mut [u8; 4]).write_unaligned((s.add(n - 4) as *const [u8; 4]).read_unaligned());
        } else {
            for k in 0..n {
                *dst.add(k) = *s.add(k);
            }
        }
    }
}

/// out.extend_from_slice of each part in turn, with one reserve and copy_short.
#[inline(always)]
fn put_parts<const K: usize>(out: &mut Vec<u8>, parts: [&[u8]; K]) {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    out.reserve(len);
    // SAFETY: out has room for len more bytes (reserved just above), all written, in order,
    // before the length is set; the parts are not in out
    unsafe {
        let mut o = out.as_mut_ptr().add(out.len());
        for p in parts {
            copy_short(o, p);
            o = o.add(p.len());
        }
        out.set_len(out.len() + len);
    }
}

/// a == b for slices of the same length, compared like copy_short (no call for short ones).
#[inline(always)]
fn eq_short(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    let r16 = |x: &[u8], i: usize| u128::from_le_bytes(x[i..i + 16].try_into().unwrap());
    let r8 = |x: &[u8], i: usize| u64::from_le_bytes(x[i..i + 8].try_into().unwrap());
    if n >= 16 {
        let mut k = 0;
        while k + 16 < n {
            if r16(a, k) != r16(b, k) {
                return false;
            }
            k += 16;
        }
        r16(a, n - 16) == r16(b, n - 16)
    } else if n >= 8 {
        r8(a, 0) == r8(b, 0) && r8(a, n - 8) == r8(b, n - 8)
    } else {
        a == b
    }
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The 2-digit number at t[i..i + 2] (None unless both are digits).
#[inline(always)]
fn d2(t: &[u8], i: usize) -> Option<i64> {
    if is_digit(t[i]) && is_digit(t[i + 1]) { Some(((t[i] - b'0') * 10 + (t[i + 1] - b'0')) as i64) } else { None }
}

/// Does t start with the layout p ('0': a digit, other bytes: themselves)?
#[inline(always)]
fn layout(t: &[u8], p: &[u8]) -> bool {
    t.len() >= p.len() && p.iter().zip(t).all(|(&p, &c)| if p == b'0' { is_digit(c) } else { c == p })
}

const MONTHS: [&[u8; 3]; 12] = [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"];
const WDAYS: [&[u8; 3]; 7] = [b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];

// ---------------------------------------------------------------------------------------
// logcat: "MM-DD HH:MM:SS.mmm" + ' ' + pad + PID + ' ' + pad + TID + ' '

/// A right-aligned '%5d' field at l[i..]: spaces then digits; (start, end of the digits) if
/// the spaces are exactly what '%5d' writes.
#[inline]
fn pad5(l: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut a = i;
    while a < l.len() && l[a] == b' ' {
        a += 1;
    }
    let (e, _) = digits(l, a);
    let w = e - a;
    if w == 0 || (w > 1 && l[a] == b'0') || a - i != 5usize.saturating_sub(w) || e >= l.len() || l[e] != b' ' {
        return None;
    }
    Some((a, e))
}

fn fwd_logcat(l: &[u8], out: &mut Vec<u8>) -> bool {
    if l.len() < 24 || !layout(l, b"00-00 00:00:00.000 ") {
        return false;
    }
    let Some((pa, pe)) = pad5(l, 19) else { return false };
    let Some((ta, _)) = pad5(l, pe + 1) else { return false };
    out.extend_from_slice(&l[..19]);
    out.extend_from_slice(&[MK, b'a']);
    out.extend_from_slice(&l[pa..pe + 1]);
    out.extend_from_slice(&l[ta..]);
    true
}

// ---------------------------------------------------------------------------------------
// bgl: LABEL EPOCH DATE NODE TIMESTAMP NODE ...

const BGL_TS: &[u8; 26] = b"0000-00-00-00.00.00.000000";

/// Days of the date in a timestamp's first 10 bytes ("YYYY-MM-DD"), checked.
fn ts_days(t: &[u8]) -> Option<i64> {
    let y = d2(t, 0)? * 100 + d2(t, 2)?;
    let (mo, d) = (d2(t, 5)?, d2(t, 8)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d))
}

/// Seconds of the day in a timestamp ("HH.MM.SS" at t[11..19]), checked.
#[inline(always)]
fn ts_secs(t: &[u8]) -> Option<i64> {
    let (h, mi, s) = (d2(t, 11)?, d2(t, 14)?, d2(t, 17)?);
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(h * 3600 + mi * 60 + s)
}

/// The days of the last date seen (both sides look up each line's date here first).
#[derive(Default)]
struct DayCache {
    date: [u8; 10],
    days: i64,
}

impl DayCache {
    #[inline(always)]
    fn get(&mut self, t: &[u8]) -> Option<i64> {
        if !eq_short(&t[..10], &self.date) {
            self.days = ts_days(t)?;
            self.date.copy_from_slice(&t[..10]);
        }
        Some(self.days)
    }
}

/// Parts of a BGL line: (label end, node start, node end, timestamp end, end of the second
/// node, epoch value, timestamp seconds) when the layout fits.
fn bgl_parts(l: &[u8], dc: &mut DayCache) -> Option<(usize, usize, usize, usize, usize, i64, i64)> {
    let a = l.iter().take(48).position(|&c| c == b' ')?; // label end
    if a == 0 {
        return None;
    }
    // (pack speed, the same answers: the epoch's end found 8 bytes at a time and its 9-11
    // digits read 8 at once; the node's end by find_byte; the timestamp layout without a
    // branch per byte)
    let e = digit_end(l, a + 1);
    let w = e - (a + 1);
    if !(9..=11).contains(&w) || l[a + 1] == b'0' || e >= l.len() || l[e] != b' ' {
        return None;
    }
    let ep = {
        let mut v = digits8_swar(&l[a + 1..a + 9]);
        for &c in &l[a + 9..e] {
            v = v * 10 + (c - b'0') as u64;
        }
        v
    };
    let ds = e + 1; // DATE
    let ns = ds + 11; // NODE
    if ns >= l.len() || l[ds + 10] != b' ' {
        return None;
    }
    let ne = ns + find_byte_inl(&l[ns..(ns + 64).min(l.len())], b' ')?;
    let ts = ne + 1;
    let te = ts + 26;
    if ne == ns || te >= l.len() || l[te] != b' ' || !bgl_ts_layout(&l[ts..te]) {
        return None;
    }
    let n2e = te + 1 + (ne - ns);
    if n2e > l.len() || !eq_short(&l[te + 1..n2e], &l[ns..ne]) || (n2e < l.len() && l[n2e] != b' ') {
        return None;
    }
    // DATE = YYYY.MM.DD of the timestamp
    let (t, d) = (&l[ts..te], &l[ds..ds + 10]);
    if d[..4] != t[..4] || d[4] != b'.' || d[5..7] != t[5..7] || d[7] != b'.' || d[8..10] != t[8..10] {
        return None;
    }
    let secs = dc.get(t)? * 86_400 + ts_secs(t)?;
    Some((a, ns, ne, te, n2e, ep as i64, secs))
}

/// The value of 8 digits (b has exactly 8 bytes, all digits): two digits, then four, then
/// eight per step.
#[inline(always)]
fn digits8_swar(b: &[u8]) -> u64 {
    let x = u64::from_le_bytes(b.try_into().unwrap()).wrapping_sub(0x3030_3030_3030_3030);
    let x = (x.wrapping_mul(10).wrapping_add(x >> 8)) & 0x00FF_00FF_00FF_00FF;
    let x = (x.wrapping_mul(100).wrapping_add(x >> 16)) & 0x0000_FFFF_0000_FFFF;
    (x.wrapping_mul(10000).wrapping_add(x >> 32)) & 0xFFFF_FFFF
}

/// For BGL_TS[off..off + 16]: per byte, the byte to xor with (b'0' where a digit goes, else the
/// byte itself) and 0x80 minus the bound the result must stay under (10 for a digit: c ^ b'0'
/// < 10 exactly when c is a digit; 1 for the others: c ^ p < 1 exactly when c == p).
const fn ts_masks(off: usize) -> (u128, u128) {
    let (mut x, mut k) = (0u128, 0u128);
    let mut i = 0;
    while i < 16 {
        let c = BGL_TS[off + i];
        let (xb, lb) = if c == b'0' { (b'0', 10u8) } else { (c, 1u8) };
        x |= (xb as u128) << (8 * i);
        k |= ((0x80 - lb) as u128) << (8 * i);
        i += 1;
    }
    (x, k)
}
const TS_A: (u128, u128) = ts_masks(0);
const TS_B: (u128, u128) = ts_masks(10);

/// layout(t, BGL_TS) for t of 26 bytes, every byte checked: bytes 0-15 and 10-25 as two
/// 16-byte words. Per byte y = c ^ x must be under its bound L (1 or 10): (y & 0x7F) + 0x80 - L
/// has its top bit set exactly when (y & 0x7F) >= L (no carry: at most 0x7F + 0x7F), and y's
/// own top bit covers y >= 0x80.
#[inline(always)]
fn bgl_ts_layout(t: &[u8]) -> bool {
    let t: &[u8; 26] = t.try_into().unwrap();
    const M7: u128 = u128::MAX / 255 * 0x7F;
    const M8: u128 = u128::MAX / 255 * 0x80;
    let bad = |w: u128, (x, k): (u128, u128)| {
        let y = w ^ x;
        (((y & M7) + k) | y) & M8
    };
    let a = u128::from_le_bytes(t[..16].try_into().unwrap());
    let b = u128::from_le_bytes(t[10..].try_into().unwrap());
    bad(a, TS_A) | bad(b, TS_B) == 0
}

fn fwd_bgl(l: &[u8], off: [i64; 2], dc: &mut DayCache, comp: bool, out: &mut Vec<u8>) -> bool {
    let Some((a, ns, _ne, te, n2e, ep, secs)) = bgl_parts(l, dc) else { return false };
    let Some(k) = off.iter().position(|&o| secs + o == ep) else { return false };
    // l[..a + 1], MK, 'b' or 'B', NODE ' ' TIMESTAMP (l[ns..te]), then the rest (l[n2e..]);
    // B5 (comp): the fill marker MK e and the rest without its first space, or MK E and the
    // rest with it (the line's end, or a rest starting with hex digits that hold a digit: the
    // splitter would read e and them as one hex run). The fill marker puts EPOCH ' ' DATE ' '
    // in at the first marker and ' ' NODE (+ ' ' for e) after the timestamp (bgl_fill).
    let (fill, p3) = if !comp {
        (None, &l[n2e..])
    } else {
        let hex = l.get(n2e + 1..).unwrap_or(&[]).iter().take_while(|c| c.is_ascii_hexdigit()).any(|c| c.is_ascii_digit());
        if n2e < l.len() && !hex { (Some(b'e'), &l[n2e + 1..]) } else { (Some(b'E'), &l[n2e..]) }
    };
    let (p1, p2) = (&l[..a + 1], &l[ns..te]);
    let len = p1.len() + 2 + p2.len() + 2 * fill.is_some() as usize + p3.len();
    out.reserve(len + 1);
    // SAFETY: out has room for len more bytes (reserved just above), written in order before
    // the length is set; l and out do not overlap
    unsafe {
        let o = out.as_mut_ptr().add(out.len());
        copy_short(o, p1);
        *o.add(p1.len()) = MK;
        *o.add(p1.len() + 1) = if k == 0 { b'b' } else { b'B' };
        copy_short(o.add(p1.len() + 2), p2);
        let mut q = p1.len() + 2 + p2.len();
        if let Some(c) = fill {
            *o.add(q) = MK;
            *o.add(q + 1) = c;
            q += 2;
        }
        copy_short(o.add(q), p3);
        out.set_len(out.len() + len);
    }
    true
}


// ---------------------------------------------------------------------------------------
// hdfs: per block id, its list of IPs

/// An IPv4 address (4 numbers 0-255, no leading zeros, no digit or '.' around) at l[k..]:
/// (end, value).
#[inline]
fn ipv4_at(l: &[u8], k: usize) -> Option<(usize, u32)> {
    if k > 0 && (is_digit(l[k - 1]) || l[k - 1] == b'.') {
        return None;
    }
    let mut p = k;
    let mut v = 0u32;
    for part in 0..4 {
        let s = p;
        let mut x = 0u32;
        while p < l.len() && is_digit(l[p]) && p - s < 3 {
            x = x * 10 + (l[p] - b'0') as u32;
            p += 1;
        }
        let w = p - s;
        if w == 0 || x > 255 || (w > 1 && l[s] == b'0') || (p < l.len() && is_digit(l[p])) {
            return None;
        }
        v = v << 8 | x;
        if part < 3 {
            if p >= l.len() || l[p] != b'.' {
                return None;
            }
            p += 1;
        }
    }
    if p + 1 < l.len() && l[p] == b'.' && is_digit(l[p + 1]) {
        return None;
    }
    Some((p, v))
}

/// Each byte value as text: length, then up to 3 digits.
static OCTET: [[u8; 4]; 256] = {
    let mut t = [[0u8; 4]; 256];
    let mut i = 0;
    while i < 256 {
        if i >= 100 {
            t[i] = [3, b'0' + (i / 100) as u8, b'0' + (i / 10 % 10) as u8, b'0' + (i % 10) as u8];
        } else if i >= 10 {
            t[i] = [2, b'0' + (i / 10) as u8, b'0' + (i % 10) as u8, 0];
        } else {
            t[i] = [1, b'0' + i as u8, 0, 0];
        }
        i += 1;
    }
    t
};

/// An IPv4 address at out[o..] (out needs 16 bytes of room); returns the end.
#[inline]
fn put_ipv4(out: &mut [u8], o: usize, v: u32) -> usize {
    let mut b = [0u8; 16];
    let mut n = 0;
    for k in (0..4).rev() {
        let t = &OCTET[((v >> (8 * k)) & 255) as usize];
        b[n..n + 3].copy_from_slice(&t[1..4]);
        n += t[0] as usize;
        b[n] = b'.';
        n += 1;
    }
    out[o..o + 16].copy_from_slice(&b);
    o + n - 1
}

/// IPs of the blocks: block id -> place in `lists` (each: up to 8 IPs and their count); the
/// recently used ids also in a small direct-mapped table (most lines name a recent block).
struct BlockIps {
    map: FxMap<u64, u32>,
    lists: Vec<([u32; 8], u8)>,
    recent: Vec<(u64, u32)>,
}

impl Default for BlockIps {
    fn default() -> BlockIps {
        BlockIps { map: FxMap::default(), lists: Vec::new(), recent: vec![(0, u32::MAX); 1024] }
    }
}

impl BlockIps {
    #[inline]
    fn get(&mut self, id: u64) -> usize {
        let r = &mut self.recent[(id >> 54) as usize];
        if r.0 == id && r.1 != u32::MAX {
            return r.1 as usize;
        }
        let n = self.lists.len() as u32;
        let k = *self.map.entry(id).or_insert(n);
        if k == n {
            self.lists.push(([0; 8], 0));
        }
        *r = (id, k);
        k as usize
    }
}

/// "blk_" + optional '-' + digits at l[p..]: (end, key: a hash of the id's text; both sides
/// use the same keys).
#[inline]
fn blk_at(l: &[u8], p: usize) -> Option<(usize, u64)> {
    if l.len() < p + 5 || &l[p..p + 4] != b"blk_" {
        return None;
    }
    let s = p + 4 + (l[p + 4] == b'-') as usize;
    let e = digit_end(l, s);
    if e == s || e - s > 19 {
        return None;
    }
    Some((e, key_hash(&l[p + 4..e])))
}

const PORT_LO: u64 = 32768;
const PORT_HI: u64 = 60999;

struct HdfsFwd {
    ips: BlockIps,
    ports: bool,
    port_vals: Vec<u16>,
    hits: usize,
}

fn fwd_hdfs(l: &[u8], st: &mut HdfsFwd, out: &mut Vec<u8>) -> bool {
    // the line's block: its first "blk_" with a number
    let mut from = 0;
    let (bp, key) = loop {
        let p = from + match find_seq(&l[from..], b"blk_") {
            Some(p) => p,
            None => return false,
        };
        if let Some((_, k)) = blk_at(l, p) {
            break (p, k);
        }
        from = p + 1;
    };
    let li = st.ips.get(key);
    let before = st.ips.lists[li].1 as usize; // places 0..before are older than this line
    let start = out.len();
    let mut last = 0usize; // l[..last] is written
    let mut marks = false;
    let mut ahead = true; // the next IP marker is the line's first one
    // copy l[last..to], with the block id's marker when the block id is in it
    let copy = |out: &mut Vec<u8>, last: usize, to: usize| {
        if last <= bp && bp < to {
            out.extend_from_slice(&l[last..bp]);
            out.extend_from_slice(&[MK, b'k']);
            out.extend_from_slice(&l[bp..to]);
        } else {
            out.extend_from_slice(&l[last..to]);
        }
    };
    // every IPv4 in the line, found from its dots (16 bytes at a time)
    let n = l.len();
    let mut k = 0; // l[..k] is looked at
    let mut base = 0;
    let mut m = 0u64;
    loop {
        while m == 0 {
            if base >= n {
                break;
            }
            m = if base + 16 <= n { eq16(l, base, b'.') } else { l[base..].iter().enumerate().fold(0u64, |a, (j, &c)| a | ((c == b'.') as u64 * 15) << (4 * j)) };
            base += 16;
        }
        if m == 0 {
            break;
        }
        let t = m.trailing_zeros();
        m &= !(15u64 << (t & !3));
        let d = base - 16 + (t / 4) as usize;
        if d < k || d == 0 || !is_digit(l[d - 1]) || d + 1 >= n || !is_digit(l[d + 1]) {
            continue;
        }
        let mut a = d - 1;
        while a > 0 && is_digit(l[a - 1]) && d - a < 3 {
            a -= 1;
        }
        let Some((e, ip)) = ipv4_at(l, a) else {
            continue;
        };
        let lst = &mut st.ips.lists[li];
        let at = lst.0[..lst.1 as usize].iter().position(|&x| x == ip);
        let code = match at {
            Some(p) if p < before => Some(p),
            Some(_) => None, // first seen earlier in this line: stays as it is
            None if (lst.1 as usize) < 8 => {
                lst.0[lst.1 as usize] = ip;
                lst.1 += 1;
                Some(8) // new: the opener adds it to the list
            }
            None => None,
        };
        if let Some(p) = code {
            marks = true;
            copy(out, last, a);
            // the line's first IP marker before its block id: i / I (the opener looks ahead
            // for the block id); h / I: a place in the list, H / I: a new IP (its own letter:
            // as pattern text this costs less than a value kind)
            let look = ahead && a < bp;
            ahead = false;
            if p == 8 {
                out.extend_from_slice(&[MK, if look { b'I' } else { b'H' }]);
                out.extend_from_slice(&l[a..e]);
            } else {
                out.extend_from_slice(&[MK, if look { b'i' } else { b'h' }, b'0' + p as u8]);
            }
            last = e;
        }
        // a client port after "src: /IP:" goes flat to the side stream
        if st.ports && a >= 6 && &l[a - 6..a] == b"src: /" && e < l.len() && l[e] == b':' {
            let (pe, pv) = digits(l, e + 1);
            if (PORT_LO..=PORT_HI).contains(&pv) && pe - (e + 1) == 5 {
                marks = true;
                copy(out, last, e + 1);
                out.extend_from_slice(&[MK, b'q']);
                st.port_vals.push((pv - PORT_LO) as u16);
                last = pe;
            }
        }
        k = e;
    }
    if !marks {
        out.truncate(start);
        return false;
    }
    copy(out, last, l.len());
    st.hits += 1;
    true
}

// ---------------------------------------------------------------------------------------
// spark: task protocol

/// A short token (task or stage number such as "582.0").
#[derive(Clone, Copy, PartialEq, Eq)]
struct Tok {
    b: [u8; 24],
    n: u8,
}

impl Tok {
    fn new(t: &[u8]) -> Option<Tok> {
        if t.len() > 24 {
            return None;
        }
        let mut b = [0u8; 24];
        b[..t.len()].copy_from_slice(t);
        Some(Tok { b, n: t.len() as u8 })
    }
    fn get(&self) -> &[u8] {
        &self.b[..self.n as usize]
    }
}

#[derive(Clone, Copy)]
struct Task {
    x: Tok,
    s: Tok,
    n: u64,
    skips: u32,
}

#[derive(Default)]
struct Spark {
    assigned: Vec<u64>, // newest last
    running: Vec<Task>, // oldest first
}

const SPARK_ASSIGNED: usize = 16;
const SPARK_RUNNING: usize = 32;
const SPARK_SKIPS: u32 = 16;

impl Spark {
    fn assign(&mut self, n: u64) {
        if self.assigned.len() == SPARK_ASSIGNED {
            self.assigned.remove(0);
        }
        self.assigned.push(n);
    }
    fn unassign(&mut self, n: u64) {
        if let Some(p) = self.assigned.iter().rposition(|&x| x == n) {
            self.assigned.remove(p);
        }
    }
    fn run(&mut self, t: Task) {
        if self.running.len() == SPARK_RUNNING {
            self.running.remove(0);
        }
        self.running.push(t);
    }
    /// Task i finished: the ones before it were skipped once more; long-skipped ones go.
    fn finish(&mut self, i: usize) -> Task {
        for t in &mut self.running[..i] {
            t.skips += 1;
        }
        let t = self.running.remove(i);
        self.running.retain(|t| t.skips <= SPARK_SKIPS);
        t
    }
}

/// "X in stage S (TID N)" at l[i..] (X and S: digits '.' digits): (X, S, start of N, end of N
/// (')' follows), N).
fn spark_task(l: &[u8], i: usize) -> Option<(Tok, Tok, usize, usize, u64)> {
    let dotted = |p: usize| -> Option<usize> {
        let (e, _) = digits(l, p);
        if e == p || e >= l.len() || l[e] != b'.' {
            return None;
        }
        let (e2, _) = digits(l, e + 1);
        if e2 == e + 1 { None } else { Some(e2) }
    };
    let xe = dotted(i)?;
    const IN: &[u8] = b" in stage ";
    if l.len() < xe + IN.len() || &l[xe..xe + IN.len()] != IN {
        return None;
    }
    let ss = xe + IN.len();
    let se = dotted(ss)?;
    const TID: &[u8] = b" (TID ";
    if l.len() < se + TID.len() || &l[se..se + TID.len()] != TID {
        return None;
    }
    let ns = se + TID.len();
    let (ne, n) = digits(l, ns);
    if ne >= l.len() || l[ne] != b')' {
        return None;
    }
    plain_num(&l[ns..ne])?;
    Some((Tok::new(&l[i..xe])?, Tok::new(&l[ss..se])?, ns, ne, n))
}

fn fwd_spark(l: &[u8], st: &mut Spark, hits: &mut usize, out: &mut Vec<u8>) -> bool {
    let Some(p) = find_seq(l, b"task ") else { return false };
    fwd_spark_at(l, p, st, hits, out)
}

/// fwd_spark when the line's first "task " is known to be at p.
#[inline(never)]
fn fwd_spark_at(l: &[u8], p: usize, st: &mut Spark, hits: &mut usize, out: &mut Vec<u8>) -> bool {
    let q = p + 5;
    let ends = |w: &[u8]| p >= w.len() && &l[p - w.len()..p] == w;
    if ends(b"Got assigned ") {
        // the number up to the line end (or '\r')
        let (e, n) = digits(l, q);
        if e == q || plain_num(&l[q..e]).is_none() || !(e == l.len() || (e + 1 == l.len() && l[e] == b'\r')) {
            return false;
        }
        st.assign(n);
        put_parts(out, [&l[..q], &[MK, b's'], &l[q..]]);
        return true;
    }
    if ends(b"Running ") {
        let Some((x, s, ns, ne, n)) = spark_task(l, q) else { return false };
        // its place among the assigned tasks, newest first
        let rank = st.assigned.iter().rposition(|&x| x == n).map(|p| st.assigned.len() - 1 - p);
        st.unassign(n);
        st.run(Task { x, s, n, skips: 0 });
        match rank {
            Some(0) => {
                // up to "(TID", then from its ')'
                put_parts(out, [&l[..q], &[MK, b'r'], &l[q..ns - 1], &l[ne..]]);
                *hits += 1;
            }
            Some(k) => {
                // up to "(TID ", the rank, then from its ')'
                put_parts(out, [&l[..q], &[MK, b'r'], &l[q..ns], &[MK, b'u']]);
                push_u64(out, k as u64);
                put_parts(out, [&l[ne..]]);
                *hits += 1;
            }
            None => put_parts(out, [&l[..q], &[MK, b'r'], &l[q..]]),
        }
        return true;
    }
    if ends(b"Finished ") {
        let Some((x, s, _ns, ne, n)) = spark_task(l, q) else { return false };
        // the opener reads the place's digits up to the first non-digit: a digit right after
        // ')' would be read as part of it, so such a line stays as it is
        if l.get(ne + 1).is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        let Some(i) = st.running.iter().position(|t| t.n == n && t.x == x && t.s == s) else { return false };
        st.finish(i);
        put_parts(out, [&l[..q], &[MK, b'f']]);
        push_u64(out, i as u64);
        put_parts(out, [&l[ne + 1..]]);
        *hits += 1;
        return true;
    }
    false
}

// ---------------------------------------------------------------------------------------
// proxy: Proxifier connections

struct Open {
    t: i64,
    app: Vec<u8>,
    host: Vec<u8>,
}

const PROXY_PENDING: usize = 4096;

#[derive(Default)]
struct Proxy {
    pending: Vec<Open>, // newest last
    base: i64,          // opener: close time - open time of the last close line
}

impl Proxy {
    fn open(&mut self, o: Open) {
        if self.pending.len() == PROXY_PENDING {
            self.pending.remove(0);
        }
        self.pending.push(o);
    }
}

/// The clock of "[MM.DD HH:MM:SS] " (17 bytes) as seconds of a 31-day-month year.
fn proxy_time(l: &[u8]) -> Option<i64> {
    if !layout(l, b"[00.00 00:00:00] ") {
        return None;
    }
    Some(((d2(l, 1)? * 31 + d2(l, 4)?) * 24 + d2(l, 7)?) * 3600 + d2(l, 10)? * 60 + d2(l, 13)?)
}

/// App and host after the clock: (end of app, start of host, end of host).
fn proxy_app_host(l: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    let a = from + find_seq(&l[from..], b" - ")?;
    let hs = a + 3;
    let he = hs + find_byte(&l[hs..], b' ')?;
    if a == from || he == hs { None } else { Some((a, hs, he)) }
}

/// A lifetime as Proxifier writes it: "<1 sec", "MM:SS" or "HH:MM:SS" (out needs 32 bytes).
fn life(l: i64, out: &mut [u8], o: usize) -> usize {
    // at least 2 digits
    let two = |out: &mut [u8], o: usize, x: i64| -> usize {
        if x < 10 {
            out[o] = b'0';
            out[o + 1] = b'0' + x as u8;
            o + 2
        } else {
            put_u64(out, o, x as u64)
        }
    };
    if l == 0 {
        return put(out, o, b"<1 sec");
    }
    let mut o = o;
    if l >= 3600 {
        o = two(out, o, l / 3600);
        out[o] = b':';
        o += 1;
    }
    o = two(out, o, l / 60 % 60);
    out[o] = b':';
    two(out, o + 1, l % 60)
}

/// Seconds of a lifetime text that `life` writes back the same.
fn parse_life(t: &[u8]) -> Option<i64> {
    let v = if t == b"<1 sec" {
        0
    } else {
        let mut parts = [0i64; 3];
        let mut n = 0;
        for p in t.split(|&c| c == b':') {
            if n == 3 || p.len() < 2 || p.len() > 6 || !p.iter().all(|&c| is_digit(c)) {
                return None;
            }
            parts[n] = p.iter().fold(0i64, |v, &c| v * 10 + (c - b'0') as i64);
            n += 1;
        }
        match n {
            2 => parts[0] * 60 + parts[1],
            3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
            _ => return None,
        }
    };
    let mut b = [0u8; 48];
    let e = life(v, &mut b, 0);
    if &b[..e] == t { Some(v) } else { None }
}

fn fwd_proxy(l: &[u8], st: &mut Proxy, hits: &mut usize, out: &mut Vec<u8>) -> bool {
    let Some(t) = proxy_time(l) else { return false };
    let Some((a, hs, he)) = proxy_app_host(l, 17) else { return false };
    let rest = &l[he..];
    if rest.starts_with(b" open through ") {
        st.open(Open { t, app: l[17..a].to_vec(), host: l[hs..he].to_vec() });
        out.extend_from_slice(&l[..17]);
        out.extend_from_slice(&[MK, b'o']);
        out.extend_from_slice(&l[17..]);
        return true;
    }
    if !rest.starts_with(b" close, ") {
        return false;
    }
    // the lifetime at the line end
    let end = if l.ends_with(b"\r") { l.len() - 1 } else { l.len() };
    let Some(lp) = find_seq(&l[he..end], b"lifetime ") else { return false };
    let ls = he + lp + 9;
    let Some(lv) = parse_life(&l[ls..end]) else { return false };
    let (app, host) = (&l[17..a], &l[hs..he]);
    let mut best: Option<(usize, i64)> = None;
    for k in (0..st.pending.len()).rev() {
        let e = &st.pending[k];
        if e.app == app && e.host == host {
            let r = (t - e.t) - lv;
            if r.abs() <= 2 && best.is_none_or(|b| r.abs() < b.1.abs()) {
                best = Some((k, r));
            }
            if r == 0 {
                break;
            }
        }
    }
    let Some((k, r)) = best else { return false };
    let rank = st.pending.len() - 1 - k;
    st.pending.remove(k);
    out.extend_from_slice(&l[..17]);
    out.extend_from_slice(&[MK, b'c']);
    push_u64(out, rank as u64);
    out.extend_from_slice(&l[he..ls]);
    out.extend_from_slice(&[MK, b'l', b'0' + (r + 2) as u8]);
    out.extend_from_slice(&l[end..]);
    *hits += 1;
    true
}

// ---------------------------------------------------------------------------------------
// float: "KEY is : F" at the line end

/// F written as Java's Float.toString writes it, for 1e-3 <= |F| < 1e7 and 0: the shortest
/// digits that read back as the same float, at least one digit after the point (out needs 64
/// bytes); None outside that range.
fn java_float(bits: u32, out: &mut [u8], o: usize) -> Option<usize> {
    use std::io::Write;
    let f = f32::from_bits(bits);
    let a = f.abs();
    if !f.is_finite() || (a != 0.0 && !(1e-3..1e7).contains(&a)) {
        return None;
    }
    let mut b = [0u8; 64];
    let mut c = std::io::Cursor::new(&mut b[..]);
    write!(c, "{}", f).ok()?;
    let n = c.position() as usize;
    let o2 = put(out, o, &b[..n]);
    Some(if b[..n].contains(&b'.') { o2 } else { put(out, o2, b".0") })
}

#[derive(Default)]
struct Floats {
    last: FxMap<u64, u32>,
}

#[inline]
fn key_hash(k: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = super::Fx::default();
    h.write(k);
    h.finish() ^ k.len() as u64
}

/// The float of a line: (start of KEY, start of F, end of F (before a '\r')).
fn float_parts(l: &[u8]) -> Option<(usize, usize, usize)> {
    let end = if l.ends_with(b"\r") { l.len() - 1 } else { l.len() };
    let from = end.saturating_sub(24);
    let sp = from + l[from..end].iter().rposition(|&c| c == b' ')?;
    let fs = sp + 1;
    if fs >= end || sp < 6 || &l[sp - 5..sp + 1] != b" is : " {
        return None;
    }
    let ke = sp - 5;
    let ks = l[..ke].iter().rposition(|&c| c == b' ').map_or(0, |p| p + 1);
    if ks == ke {
        return None;
    }
    Some((ks, fs, end))
}

fn fwd_float(l: &[u8], st: &mut Floats, out: &mut Vec<u8>) -> bool {
    let Some((ks, fs, end)) = float_parts(l) else { return false };
    let t = &l[fs..end];
    if !t.iter().all(|&c| is_digit(c) || c == b'.' || c == b'-') {
        return false;
    }
    let Ok(f) = crate::engine::fl::parse_f32(t).ok_or(()) else { return false };
    let bits = f.to_bits();
    let mut b = [0u8; 64];
    match java_float(bits, &mut b, 0) {
        Some(e) if &b[..e] == t => {}
        _ => return false,
    }
    let key = key_hash(&l[ks..fs - 6]);
    let last = st.last.insert(key, bits).unwrap_or(0);
    out.extend_from_slice(&l[..fs]);
    out.extend_from_slice(&[MK, b'g']);
    push_i64(out, bits as i64 - last as i64);
    out.extend_from_slice(&l[end..]);
    true
}

// ---------------------------------------------------------------------------------------
// oldnew: "type=T,NEW,old=OLD"

#[derive(Default)]
struct OldNew {
    last: FxMap<u64, Vec<u8>>,
}

/// (start of T, end of T, end of NEW, start of OLD, end of OLD (before a '\r'), T).
fn oldnew_parts(l: &[u8]) -> Option<(usize, usize, usize, usize, usize, u64)> {
    let p = find_seq(l, b"type=")? + 5;
    let (te, tv) = digits(l, p);
    if te == p || te - p > 18 || te >= l.len() || l[te] != b',' {
        return None;
    }
    let ne = te + 1 + find_byte(&l[te + 1..], b',')?;
    if l.len() < ne + 5 || &l[ne..ne + 5] != b",old=" {
        return None;
    }
    let end = if l.ends_with(b"\r") { l.len() - 1 } else { l.len() };
    let os = ne + 5;
    if os > end {
        return None;
    }
    Some((p, te, ne, os, end, tv))
}

fn fwd_oldnew(l: &[u8], st: &mut OldNew, hits: &mut usize, out: &mut Vec<u8>) -> bool {
    let Some((p, te, ne, os, end, tv)) = oldnew_parts(l) else { return false };
    let new = &l[te + 1..ne];
    let same = st.last.get(&tv).is_some_and(|x| x[..] == l[os..end]);
    out.extend_from_slice(&l[..p]);
    out.extend_from_slice(&[MK, b'w']);
    if same {
        out.extend_from_slice(&l[p..os]);
        out.extend_from_slice(&[MK, b'x']);
        out.extend_from_slice(&l[end..]);
        *hits += 1;
    } else {
        out.extend_from_slice(&l[p..]);
    }
    let e = st.last.entry(tv).or_default();
    e.clear();
    e.extend_from_slice(new);
    true
}

// ---------------------------------------------------------------------------------------
// ctime: "Mmm dd hh:mm:ss ..." ending with "Www Mmm dd hh:mm:ss yyyy" (+ spaces / '\r')

fn month_of(t: &[u8]) -> Option<i64> {
    MONTHS.iter().position(|m| &m[..] == t).map(|m| m as i64 + 1)
}

/// The weekday name of the syslog header h ("Mmm dd hh:mm:ss") in year y.
fn weekday(h: &[u8], y: i64) -> Option<&'static [u8; 3]> {
    let mo = month_of(&h[..3])?;
    let d = if h[4] == b' ' { (h[5] as i64) - b'0' as i64 } else { d2(h, 4)? };
    if !(1..=31).contains(&d) {
        return None;
    }
    Some(WDAYS[(days_from_civil(y, mo, d) + 3).rem_euclid(7) as usize])
}

/// A syslog header "Mmm dd hh:mm:ss " (dd: " d" or "dd") at the line start?
fn syslog_hdr(l: &[u8]) -> bool {
    l.len() > 16 && month_of(&l[..3]).is_some() && l[3] == b' ' && (l[4] == b' ' || is_digit(l[4])) && layout(&l[5..], b"0 00:00:00 ")
}

fn fwd_ctime(l: &[u8], out: &mut Vec<u8>) -> bool {
    if !syslog_hdr(l) {
        return false;
    }
    let mut e = l.len();
    while e > 0 && (l[e - 1] == b' ' || l[e - 1] == b'\r') {
        e -= 1;
    }
    if e < 16 + 25 {
        return false;
    }
    let s = e - 24; // "Www Mmm dd hh:mm:ss yyyy"
    if l[s - 1] != b' ' || l[s + 3] != b' ' || l[s + 4..s + 19] != l[..15] || l[s + 19] != b' ' || !l[s + 20..e].iter().all(|&c| is_digit(c)) || l[s + 20] == b'0' {
        return false;
    }
    let y = l[s + 20..e].iter().fold(0i64, |v, &c| v * 10 + (c - b'0') as i64);
    if weekday(l, y).map(|w| &w[..]) != Some(&l[s..s + 3]) {
        return false;
    }
    out.extend_from_slice(&l[..s]);
    out.extend_from_slice(&[MK, b't']);
    out.extend_from_slice(&l[s + 20..]);
    true
}

// ---------------------------------------------------------------------------------------
// rdns: "a-b-c-d" in a host name = the octets of an IPv4 of the same line

/// Every IPv4 in l[a..z]: (start, end, value).
fn ipv4s(l: &[u8], a: usize, z: usize, f: &mut impl FnMut(usize, usize, u32) -> bool) {
    let mut k = a;
    while k < z {
        if is_digit(l[k]) && (k == 0 || !(is_digit(l[k - 1]) || l[k - 1] == b'.')) {
            if let Some((e, v)) = ipv4_at(&l[..z], k) {
                if !f(k, e, v) {
                    return;
                }
                k = e;
                continue;
            }
        }
        k += 1;
    }
}

/// The 4 dash-separated numbers of 1-3 digits at l[a..]: (end, values, all 3 digits, all plain).
fn dash4(l: &[u8], a: usize) -> Option<(usize, [u32; 4], bool, bool)> {
    let (mut p, mut v, mut pad, mut plain) = (a, [0u32; 4], true, true);
    for (j, x) in v.iter_mut().enumerate() {
        if j > 0 {
            if p >= l.len() || l[p] != b'-' {
                return None;
            }
            p += 1;
        }
        let e = digit_end(l, p);
        if e == p || e - p > 3 {
            return None;
        }
        *x = l[p..e].iter().fold(0u32, |a, &c| a * 10 + (c - b'0') as u32);
        if *x > 255 {
            return None;
        }
        pad &= e - p == 3;
        plain &= e - p == 1 || l[p] != b'0';
        p = e;
    }
    // not part of a longer run of numbers
    if p < l.len() && (is_digit(l[p]) || (l[p] == b'-' && p + 1 < l.len() && is_digit(l[p + 1]))) {
        return None;
    }
    Some((p, v, pad, plain))
}

fn fwd_rdns(l: &[u8], out: &mut Vec<u8>) -> bool {
    let n = l.len();
    let mut from = 0;
    // a '-' between digits starts the search
    while let Some(d) = find_byte(&l[from..], b'-') {
        let d = from + d;
        from = d + 1;
        if d == 0 || d + 1 >= n || !is_digit(l[d - 1]) || !is_digit(l[d + 1]) {
            continue;
        }
        let mut a = d - 1;
        while a > 0 && is_digit(l[a - 1]) {
            a -= 1;
        }
        if a > 0 && (l[a - 1] == b'-' || l[a - 1] == b'.') && a > 1 && is_digit(l[a - 2]) {
            continue;
        }
        let Some((e, v, pad, plain)) = dash4(l, a) else { continue };
        // a reverse-DNS name goes on with its domain ("....pools.arcor-ip.net"); a bare
        // host name (a machine named after its address) is left to the engine
        if !(pad || plain) || e + 1 >= n || l[e] != b'.' || !l[e + 1].is_ascii_alphabetic() {
            continue;
        }
        // the last IPv4 before the name, else the first after it
        let mut before = None;
        ipv4s(l, 0, a, &mut |_, _, ip| {
            before = Some(ip);
            true
        });
        let mut after = None;
        ipv4s(l, e, n, &mut |_, _, ip| {
            after = Some(ip);
            false
        });
        let ov = |ip: u32| [ip >> 24, ip >> 16 & 255, ip >> 8 & 255, ip & 255];
        let rv = |x: [u32; 4]| [x[3], x[2], x[1], x[0]];
        let mut code = None;
        for (pos, ip) in [(b'b', before), (b'a', after)] {
            if let Some(ip) = ip {
                if v == ov(ip) {
                    code = Some((pos, b'f'));
                } else if v == rv(ov(ip)) {
                    code = Some((pos, b'r'));
                }
                if code.is_some() {
                    break;
                }
            }
        }
        let Some((pos, ord)) = code else { continue };
        out.extend_from_slice(&l[..a]);
        out.extend_from_slice(&[MK, b'n', pos, ord, if pad { b'p' } else { b'u' }]);
        out.extend_from_slice(&l[e..]);
        return true;
    }
    false
}

/// The host-name form of an IPv4 (rdns letters: order, padding) at out[o..]; returns the end.
fn put_dash4(out: &mut [u8], mut o: usize, ip: u32, ord: u8, pad: u8) -> usize {
    let mut x = [ip >> 24, ip >> 16 & 255, ip >> 8 & 255, ip & 255];
    if ord == b'r' {
        x.reverse();
    }
    for (j, &v) in x.iter().enumerate() {
        if j > 0 {
            out[o] = b'-';
            o += 1;
        }
        if pad == b'p' {
            out[o..o + 3].copy_from_slice(&[b'0' + (v / 100) as u8, b'0' + (v / 10 % 10) as u8, b'0' + (v % 10) as u8]);
            o += 3;
        } else {
            o = put_u64(out, o, v as u64);
        }
    }
    o
}

// ---------------------------------------------------------------------------------------
// packing

/// A block's lines rewritten: the text (each line ends with '\n', then ROOM spare bytes),
/// where each line starts, and the side stream.
pub struct Fwd {
    pub text: Vec<u8>,
    pub starts: Vec<u32>,
    pub side: Vec<u8>,
    pub marks: usize, // B5: the bgl and ctime markers in the text
}

impl Fwd {
    pub fn lines(&self) -> Vec<&[u8]> {
        self.starts.windows(2).map(|w| &self.text[w[0] as usize..w[1] as usize - 1]).collect()
    }
}

/// Rule bits allowed by the switches.
fn allowed() -> u32 {
    static A: crate::engine::fl::NoEnvOnce<u32> = crate::engine::fl::NoEnvOnce::new();
    *A.get_or_init(|| {
        if crate::engine::fl::no_env::var_os("FL_NODERIVE").is_some() {
            return 0;
        }
        match crate::engine::fl::no_env::var("FL_DERIVE") {
            Ok(v) => v.split(',').filter_map(|n| NAMES.iter().find(|x| x.0 == n.trim()).map(|x| x.1)).fold(0, |a, b| a | b),
            // the default: every rule but hdfs (its first-seen IP lists rewrite a whole file on one
            // core, which caps all-core packing), logcat (the engine's padded numbers do it) and
            // rdns (SSH +0.3% for a quarter of its all-core packing speed)
            Err(_) => !(R_HDFS | R_PORTS | R_LOGCAT | R_RDNS),
        }
    })
}

/// Per-rule state of the packing side.
struct Fstate {
    days: DayCache,
    hdfs: HdfsFwd,
    spark: Spark,
    proxy: Proxy,
    floats: Floats,
    oldnew: OldNew,
    tmp: Vec<u8>,
    hits: [usize; 10], // per rule bit place: lines with a derived value
    mk: usize,         // B5: the bgl and ctime markers written
}

impl Fstate {
    fn new(r: &Rules) -> Fstate {
        Fstate {
            days: DayCache::default(),
            hdfs: HdfsFwd { ips: BlockIps::default(), ports: r.bits & R_PORTS != 0, port_vals: Vec::new(), hits: 0 },
            spark: Spark::default(),
            proxy: Proxy::default(),
            floats: Floats::default(),
            oldnew: OldNew::default(),
            tmp: Vec::new(),
            hits: [0; 10],
            mk: 0,
        }
    }

    /// line() as a call (for rules few lines get to).
    #[inline(never)]
    fn line_cold(&mut self, l: &[u8], r: &Rules, out: &mut Vec<u8>) -> bool {
        self.line(l, r, out)
    }

    /// Rewrite one line into `out` (true), or leave it (false: nothing written, no state
    /// changed).
    #[inline]
    fn line(&mut self, l: &[u8], r: &Rules, out: &mut Vec<u8>) -> bool {
        let b = r.bits;
        if b & R_RDNS != 0 {
            let mut t = std::mem::take(&mut self.tmp);
            t.clear();
            if fwd_rdns(l, &mut t) {
                self.hits[9] += 1;
                if !(b & R_CTIME != 0 && fwd_ctime(&t, out)) {
                    out.extend_from_slice(&t);
                }
                self.tmp = t;
                return true;
            }
            self.tmp = t;
        }
        if b & R_LOGCAT != 0 && fwd_logcat(l, out) {
            self.hits[0] += 1;
            return true;
        }
        if b & R_BGL != 0 && fwd_bgl(l, r.bgl_off, &mut self.days, r.comp, out) {
            self.hits[1] += 1;
            self.mk += 1 + r.comp as usize;
            return true;
        }
        if b & R_HDFS != 0 && fwd_hdfs(l, &mut self.hdfs, out) {
            self.hits[2] = self.hdfs.hits;
            return true;
        }
        if b & R_SPARK != 0 && fwd_spark(l, &mut self.spark, &mut self.hits[4], out) {
            return true;
        }
        if b & R_PROXY != 0 && fwd_proxy(l, &mut self.proxy, &mut self.hits[5], out) {
            return true;
        }
        if b & R_FLOAT != 0 && fwd_float(l, &mut self.floats, out) {
            self.hits[6] += 1;
            return true;
        }
        if b & R_OLDNEW != 0 && fwd_oldnew(l, &mut self.oldnew, &mut self.hits[7], out) {
            return true;
        }
        if b & R_CTIME != 0 && fwd_ctime(l, out) {
            self.hits[8] += 1;
            self.mk += 1;
            return true;
        }
        false
    }
}

/// The lines detection looks at: up to 4 runs of 2048 lines spread over the file.
fn sample<'a>(lines: &[&'a [u8]]) -> Vec<&'a [u8]> {
    const RUN: usize = 2048;
    let n = lines.len();
    if n <= 4 * RUN {
        return lines.to_vec();
    }
    let mut s = Vec::with_capacity(4 * RUN);
    for k in 0..4 {
        let a = k * (n - RUN) / 3;
        s.extend_from_slice(&lines[a..a + RUN]);
    }
    s
}

/// The rules worth using on a file, found on a sample of its lines.
pub fn detect(lines: &[&[u8]]) -> Rules {
    let allow = allowed();
    if allow == 0 || lines.is_empty() {
        return Rules::default();
    }
    let s = sample(lines);
    let n = s.len();
    // the BGL clock offsets: the two most common EPOCH - TIMESTAMP on the sample
    let mut bgl_off = [0i64; 2];
    if allow & R_BGL != 0 {
        let mut offs: FxMap<i64, usize> = FxMap::default();
        let mut dc = DayCache::default();
        for l in &s {
            if let Some((_, _, _, _, _, ep, secs)) = bgl_parts(l, &mut dc) {
                *offs.entry(ep - secs).or_default() += 1;
            }
        }
        let mut v: Vec<(usize, i64)> = offs.into_iter().map(|(o, c)| (c, o)).collect();
        super::sort_stable_by(&mut v, |a, b| (std::cmp::Reverse(a.0), a.1) < (std::cmp::Reverse(b.0), b.1));
        if let Some(&(_, o)) = v.first() {
            bgl_off = [o, v.get(1).map_or(o, |x| x.1)];
        }
    }
    let all = Rules { bits: allow & !R_PORTS, bgl_off, comp: false };
    let mut st = Fstate::new(&all);
    let mut out = Vec::new();
    let (mut ports, mut srcs) = (0usize, 0usize);
    for l in &s {
        out.clear();
        st.line(l, &all, &mut out);
        // client ports: how many are in the ephemeral range
        if allow & R_PORTS != 0 {
            if let Some(p) = find_seq(l, b"src: /") {
                if let Some((e, _)) = ipv4_at(l, p + 6) {
                    if e < l.len() && l[e] == b':' {
                        let (pe, pv) = digits(l, e + 1);
                        srcs += 1;
                        ports += ((PORT_LO..=PORT_HI).contains(&pv) && pe - (e + 1) == 5) as usize;
                    }
                }
            }
        }
    }
    let h = &st.hits;
    let mut bits = 0;
    // header rules must fit most lines; the others a few per thousand
    if h[0] * 10 >= n * 3 {
        bits |= R_LOGCAT;
    }
    if h[1] * 10 >= n * 3 {
        bits |= R_BGL;
    }
    if h[2] * 100 >= n && h[2] >= 8 {
        bits |= R_HDFS;
        if srcs >= 8 && ports * 10 >= srcs * 9 {
            bits |= R_PORTS;
        }
    }
    if h[4] * 200 >= n && h[4] >= 8 {
        bits |= R_SPARK;
    }
    if h[5] * 100 >= n && h[5] >= 8 {
        bits |= R_PROXY;
    }
    if h[6] * 1000 >= n && h[6] >= 4 {
        bits |= R_FLOAT;
    }
    if h[7] * 1000 >= n && h[7] >= 4 {
        bits |= R_OLDNEW;
    }
    if h[8] * 200 >= n && h[8] >= 4 {
        bits |= R_CTIME;
    }
    if h[9] * 1000 >= n && h[9] >= 4 {
        bits |= R_RDNS;
    }
    Rules { bits: bits & allow, bgl_off, comp: b5_on() }
}

// ---------------------------------------------------------------------------------------
// B5: computed parts, put in by the line printer (or by `inverse` in the old way)

/// The settings of a block whose markers the line printer puts back (text size 0).
#[derive(Clone, Copy, Default)]
pub struct Comp {
    pub bgl: bool,
    pub ctime: bool,
    pub off: [i64; 2],
}

/// The last bgl text put in (bgl_fill): the timestamp up to its seconds and the offset, and
/// EPOCH ' ' DATE ' ' (its length n; 0: none).
#[derive(Default)]
pub struct BglCache {
    days: DayCache,
    key: (u128, u32, u8),
    ins: [u8; 32],
    n: usize,
}

/// Whether a block of rules `side` (its side stream) may be printed with its derived parts:
/// B5 on, and no rule on but bgl and ctime.
pub fn may_inline(side: &[u8]) -> bool {
    let mut p = 0;
    let bits = get_varint(side, &mut p) as u32;
    b5_on() && bits & !(R_BGL | R_CTIME) == 0
}

/// Mark a side stream (may_inline) for the line printer: its text size becomes 0.
pub fn mark_inline(side: &mut Vec<u8>) {
    let mut p = 0;
    get_varint(side, &mut p);
    let a = p;
    get_varint(side, &mut p);
    let mut v = Vec::with_capacity(side.len());
    v.extend_from_slice(&side[..a]);
    v.push(0);
    v.extend_from_slice(&side[p..]);
    *side = v;
}

/// The line printer's settings of a block marked for it (None: not marked, or a side stream
/// that does not read as one).
#[inline(never)]
pub fn inline_of(side: &[u8]) -> Option<Comp> {
    let mut p = 0;
    // (the bits as read, all 64: a bit past 32 is refused, not cut off)
    let bits = varint_checked(side, &mut p)?;
    let (bgl, ctime) = (bits & R_BGL as u64 != 0, bits & R_CTIME as u64 != 0);
    if !b5_on() || bits & !((R_BGL | R_CTIME) as u64) != 0 || bits == 0 || varint_checked(side, &mut p)? != 0 {
        return None;
    }
    let mut off = [0i64; 2];
    if bgl {
        off = [unzz(varint_checked(side, &mut p)?), unzz(varint_checked(side, &mut p)?)];
    }
    (p == side.len()).then_some(Comp { bgl, ctime, off })
}

/// A varint at b[*p..] (None past the end or over 64 bits).
fn varint_checked(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for s in (0..64).step_by(7) {
        let x = *b.get(*p)?;
        *p += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

/// The bytes the line printer leaves for EPOCH ' ' DATE ' ' at a bgl gap marker (a 10-digit
/// epoch); bgl_fill moves the rest when they differ.
pub const BGL_HOLE: usize = 22;

/// B5: a bgl line's fill marker at out[o] (sp: e, a space follows; E: the line ends), its
/// gap marker's hole at out[gap..gap + res] (k: 0 for b, 1 for B): out[gap + res..o] is NODE
/// ' ' TIMESTAMP (written). EPOCH ' ' DATE ' ' goes in the hole (the node and the timestamp
/// move when it is not that size), then ' ' NODE (and ' ') after the timestamp. Returns the
/// new end, or None (damaged data): a node of 1-63 bytes, the timestamp's layout and values
/// (checked when its seconds differ from the last one's: equal ones were checked), an epoch
/// of 9-11 digits, room in out.
#[inline(never)]
pub fn bgl_fill(out: &mut [u8], gap: usize, res: usize, o: usize, k: usize, sp: bool, off: &[i64; 2], c: &mut BglCache) -> Option<usize> {
    let te = o;
    if te < gap + res + 28 || te > out.len() {
        return None;
    }
    let ts = te - 26;
    let t: [u8; 26] = out[ts..te].try_into().unwrap();
    let key = (u128::from_le_bytes(t[..16].try_into().unwrap()), u32::from_le_bytes(t[15..19].try_into().unwrap()), k as u8);
    if key != c.key || c.n == 0 {
        if !bgl_ts_layout(&t) {
            return None;
        }
        let secs = c.days.get(&t)? * 86_400 + ts_secs(&t)?;
        let ep = secs.checked_add(*off.get(k)?)?;
        if !(100_000_000..100_000_000_000).contains(&ep) {
            return None;
        }
        let mut b = [0u8; 32];
        let w = put_u64(&mut b, 0, ep as u64);
        b[w] = b' ';
        b[w + 1..w + 5].copy_from_slice(&t[..4]);
        b[w + 5] = b'.';
        b[w + 6..w + 8].copy_from_slice(&t[5..7]);
        b[w + 8] = b'.';
        b[w + 9..w + 11].copy_from_slice(&t[8..10]);
        b[w + 11] = b' ';
        c.ins = b;
        c.n = w + 12;
        c.key = key;
    }
    let (ns, ne) = (gap + res, ts - 1);
    if out[ne] != b' ' || ne - ns > 63 {
        return None;
    }
    let (n, nl) = (c.n, ne - ns);
    if n == res && te + 80 <= out.len() {
        // the hole fits (n >= 21: two 16-byte stores fill it); ' ' NODE copied 16 bytes at a
        // time from before the timestamp (reads end before te - 11, writes before te + 66)
        // SAFETY: gap + n <= te and te + 80 <= out.len(), checked above
        unsafe {
            let p = out.as_mut_ptr();
            let ins = c.ins;
            (p.add(gap) as *mut [u8; 16]).write_unaligned(ins[..16].try_into().unwrap());
            (p.add(gap + n - 16) as *mut [u8; 16]).write_unaligned(ins[n - 16..n].try_into().unwrap());
            *p.add(te) = b' ';
            let mut q = 0;
            while q < nl {
                let x = (p.add(ns + q) as *const [u8; 16]).read_unaligned();
                (p.add(te + 1 + q) as *mut [u8; 16]).write_unaligned(x);
                q += 16;
            }
            *p.add(te + 1 + nl) = b' ';
        }
        return Some(te + 1 + nl + sp as usize);
    }
    // another epoch width (or near the buffer's end): move NODE ' ' TIMESTAMP
    let te2 = gap + n + (te - ns);
    let end = te2 + 1 + nl + sp as usize;
    if end > out.len() {
        return None;
    }
    out.copy_within(ns..te, gap + n);
    out[gap..gap + n].copy_from_slice(&c.ins[..n]);
    out[te2] = b' ';
    out.copy_within(gap + n..gap + n + nl, te2 + 1);
    if sp {
        out[end - 1] = b' ';
    }
    Some(end)
}

/// A ctime marker at out[p] when its line is written (out[ls..o], '\n' included): the year's
/// 4 digits at out[p..]; "Www Mmm dd hh:mm:ss " goes in before them (from the line's
/// syslog header and the year). Returns the new end, or None (damaged data).
#[inline(never)]
pub fn ctime_put(out: &mut [u8], ls: usize, p: usize, o: usize) -> Option<usize> {
    if p < ls + 17 || p + 4 > o || o + 20 > out.len() {
        return None;
    }
    let yd = &out[p..p + 4];
    if !yd.iter().all(|&c| is_digit(c)) || yd[0] == b'0' || !syslog_hdr(&out[ls..p]) {
        return None;
    }
    let y = yd.iter().fold(0i64, |v, &c| v * 10 + (c - b'0') as i64);
    let wd = *weekday(&out[ls..ls + 15], y)?;
    out.copy_within(p..o, p + 20);
    out[p..p + 3].copy_from_slice(&wd);
    out[p + 3] = b' ';
    out.copy_within(ls..ls + 15, p + 4);
    out[p + 19] = b' ';
    Some(o + 20)
}

/// The rewrite of `lines` (slices of `data`, in order) put after `text`, with each line's
/// start in text (as u32) put after `starts`; returns the number of lines rewritten. A line
/// no rule rewrites is copied with its '\n' (a '\n' is added after the file's last line when
/// it has none). The same text and starts as a line-by-line copy, but runs of such lines that
/// follow each other in `data` are copied at once (data[ra..rz], not yet in text), and when
/// the only rule on is one that needs a certain piece of text in a line, a line without it is
/// not offered to the rules (it is left as it is either way):
/// - spark: fwd_spark looks for the line's first "task " and returns false without one;
/// - oldnew: oldnew_parts looks for "type=" and fwd_oldnew returns false without one;
/// - float: float_parts needs " is : " right before the line's last word;
/// and each returns false with nothing written and no state changed.
#[inline(never)]
fn rewrite_lines(st: &mut Fstate, r: &Rules, data: &[u8], lines: &[&[u8]], text: &mut Vec<u8>, starts: &mut Vec<u32>) -> usize {
    // (spark alone: its rule called with the place of the "task " found; bgl alone: its rule
    // inline, as line() runs it; the others: line() as one shared call)
    match r.bits {
        R_SPARK => rewrite_lines_with(data, lines, text, starts, Some((5, find_task as Finder)), |l, p, out| match p {
            Some(p) => fwd_spark_at(l, p, &mut st.spark, &mut st.hits[4], out),
            None => fwd_spark(l, &mut st.spark, &mut st.hits[4], out),
        }),
        R_BGL => rewrite_lines_with(data, lines, text, starts, None, |l, _, out| {
            // (as line(): the markers counted for B5)
            let ok = fwd_bgl(l, r.bgl_off, &mut st.days, r.comp, out);
            st.hits[1] += ok as usize;
            st.mk += (1 + r.comp as usize) * ok as usize;
            ok
        }),
        b => {
            let needle = match b {
                R_OLDNEW => Some((5, find_type as Finder)),
                R_FLOAT => Some((6, find_is as Finder)),
                _ => None,
            };
            rewrite_lines_with(data, lines, text, starts, needle, |l, _, out| st.line_cold(l, r, out))
        }
    }
}

/// The first place of a rule's needle in h (find_seq_inl).
type Finder = fn(&[u8]) -> Option<usize>;
#[inline(never)]
fn find_task(h: &[u8]) -> Option<usize> {
    // (spark's own copy: its logs have a "task " every few lines)
    find_seq_inl(h, b"task ")
}
fn find_type(h: &[u8]) -> Option<usize> {
    find_needle(h, b"type=")
}
fn find_is(h: &[u8]) -> Option<usize> {
    find_needle(h, b" is : ")
}
#[inline(never)]
fn find_needle(h: &[u8], needle: &[u8]) -> Option<usize> {
    find_seq_inl(h, needle)
}

/// The loop of rewrite_lines: `needle` (its length and its search), when the rules can only
/// rewrite a line that holds it; `rule(line, place of the line's first needle when known,
/// out)`: the rules.
#[inline(always)]
fn rewrite_lines_with(data: &[u8], lines: &[&[u8]], text: &mut Vec<u8>, starts: &mut Vec<u32>, needle: Option<(usize, Finder)>, mut rule: impl FnMut(&[u8], Option<usize>, &mut Vec<u8>) -> bool) -> usize {
    let off = |l: &[u8]| l.as_ptr() as usize - data.as_ptr() as usize;
    let mut changed = 0usize;
    let (mut ra, mut rz) = (0usize, 0usize);
    // text's length with the run
    let mut tl = text.len();
    let z = lines.last().map_or(0, |l| off(l) + l.len());
    // nt: the first needle in data[from..z] (z: none there); a line [s, e) with from <= s <=
    // nt holds one exactly when nt + k <= e (none starts in [s, nt), and one starting at nt or
    // later that ends past e cannot be inside the line). Lines come in order; one that does
    // not (s < from, or e > z) is offered to the rules, as without a needle.
    let (mut nt, mut from) = (0usize, usize::MAX);
    for l in lines {
        starts.push(tl as u32);
        let s = off(l);
        let e = s + l.len();
        // (offer, the place of the line's first needle when known)
        let (offer, at) = match &needle {
            Some((k, find)) => {
                if s <= z && (from == usize::MAX || nt < s) {
                    nt = find(&data[s..z]).map_or(z, |p| s + p);
                    from = s;
                }
                if s < from || e > z { (true, None) } else { (nt + k <= e, Some(nt - s)) }
            }
            None => (true, None),
        };
        if offer {
            put_run(text, &data[ra..rz]);
            ra = rz;
            let ok = rule(l, at, text);
            if ok {
                changed += 1;
                text.push(b'\n');
            }
            tl = text.len();
            if ok {
                continue;
            }
        }
        // the line and its '\n' (the file's last line may have none)
        if e < data.len() {
            if s != rz {
                put_run(text, &data[ra..rz]);
                ra = s;
            }
            rz = e + 1;
        } else {
            put_run(text, &data[ra..rz]);
            text.extend_from_slice(l);
            text.push(b'\n');
            ra = rz;
        }
        tl += l.len() + 1;
    }
    put_run(text, &data[ra..rz]);
    debug_assert!(tl == text.len());
    changed
}

/// text.extend_from_slice(run) for a run that may be empty.
#[inline(always)]
fn put_run(text: &mut Vec<u8>, run: &[u8]) {
    if !run.is_empty() {
        text.extend_from_slice(run);
    }
}

/// Packing side: the rules of a file and their state, carried from block to block (blocks are
/// rewritten in order; the opener opens them in the same order).
pub struct Packer {
    r: Rules,
    st: Fstate,
    mk: Option<bool>,
}

impl Packer {
    pub fn new(r: Rules) -> Packer {
        Packer { r, st: Fstate::new(&r), mk: None }
    }

    /// For a caller that already knows whether the lines' text (from the first line's start
    /// to the last line's end in `data`) holds an MK byte (has_byte): forward and forward_par
    /// then take that answer instead of looking again.
    pub fn mk_known(mut self, found: bool) -> Packer {
        self.mk = Some(found);
        self
    }

    /// Rewrite a block's lines (slices of `data`, in order). None: no rule on, a line holds an
    /// MK byte, or nothing changed; the state is then as before.
    pub fn forward(&mut self, data: &[u8], lines: &[&[u8]]) -> Option<Fwd> {
        let (r, st) = (&self.r, &mut self.st);
        let (first, last) = (lines.first()?, lines.last()?);
        let off = |l: &[u8]| l.as_ptr() as usize - data.as_ptr() as usize;
        let (a, z) = (off(first), off(last) + last.len());
        if r.bits == 0 || self.mk.unwrap_or_else(|| has_byte(&data[a..z], MK)) {
            return None;
        }
        let mut text = Vec::with_capacity(z - a + 1 + lines.len() / 4 + super::split::ROOM);
        let mut starts = Vec::with_capacity(lines.len() + 1);
        st.hdfs.port_vals.clear();
        let mk0 = st.mk;
        let changed = rewrite_lines(st, r, data, lines, &mut text, &mut starts);
        if changed == 0 {
            return None;
        }
        starts.push(text.len() as u32);
        let tsize = text.len();
        text.resize(tsize + super::split::ROOM, 0);
        let mut side = Vec::new();
        put_varint(&mut side, r.side_bits());
        put_varint(&mut side, tsize as u64);
        if r.bits & R_BGL != 0 {
            put_varint(&mut side, zz(r.bgl_off[0]));
            put_varint(&mut side, zz(r.bgl_off[1]));
        }
        if r.bits & R_PORTS != 0 {
            let v = &st.hdfs.port_vals;
            put_varint(&mut side, v.len() as u64);
            // 15 bits per port, packed
            let (mut acc, mut nb) = (0u64, 0u32);
            for &p in v {
                acc |= (p as u64) << nb;
                nb += 15;
                while nb >= 8 {
                    side.push(acc as u8);
                    acc >>= 8;
                    nb -= 8;
                }
            }
            if nb > 0 {
                side.push(acc as u8);
            }
        }
        Some(Fwd { text, starts, side, marks: st.mk - mk0 })
    }

    /// `forward` on up to `threads` threads (a fresh Packer only): the same Fwd, byte for byte.
    /// The block's lines are cut into chunks rewritten side by side. Allowed only when every
    /// rule on is one whose answer (rewrite or not) does not depend on the state, and whose
    /// state is either none (ctime, logcat, rdns), a pure memo (bgl's DayCache: its date is
    /// checked to be digits before use, so a fresh cache answers as a warm one), or a map of
    /// "the last value per key", overwritten by every line the rule rewrites, whatever the
    /// state held (float, oldnew). A chunk's start state is then the maps' last values from
    /// all earlier chunks: phase A finds each chunk's own last values from a fresh start (its
    /// rewrite thrown away), the starts are merged in order (later chunks win), and phase B
    /// rewrites every chunk from its true start state. spark, proxy and hdfs keep the serial
    /// path.
    pub fn forward_par(&mut self, data: &[u8], lines: &[&[u8]], threads: usize) -> Option<Fwd> {
        const MIN_LINES: usize = 8192;
        let r = self.r;
        let c = threads.min(lines.len() / MIN_LINES);
        if c < 2 || r.bits & (R_HDFS | R_PORTS | R_SPARK | R_PROXY) != 0 {
            return self.forward(data, lines);
        }
        let (first, last) = (lines.first()?, lines.last()?);
        let off = |l: &[u8]| l.as_ptr() as usize - data.as_ptr() as usize;
        let (a, z) = (off(first), off(last) + last.len());
        if r.bits == 0 || self.mk.unwrap_or_else(|| has_byte(&data[a..z], MK)) {
            return None;
        }
        let chunks: Vec<&[&[u8]]> = lines.chunks(lines.len().div_ceil(c)).collect();
        let stateful = r.bits & (R_FLOAT | R_OLDNEW) != 0;
        type Ends = (FxMap<u64, u32>, FxMap<u64, Vec<u8>>);
        // phase A: the last values per key of each chunk but the last, from a fresh start
        let ends: Vec<Ends> = if stateful {
            std::thread::scope(|sc| {
                let hs: Vec<_> = chunks[..chunks.len() - 1]
                    .iter()
                    .map(|ch| {
                        sc.spawn(move || {
                            let mut st = Fstate::new(&r);
                            let mut out = Vec::new();
                            for l in ch.iter() {
                                out.clear();
                                st.line(l, &r, &mut out);
                            }
                            (std::mem::take(&mut st.floats.last), std::mem::take(&mut st.oldnew.last))
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            })
        } else {
            Vec::new()
        };
        // many keys (each chunk's start state is a copy of all keys before it): the serial
        // pass is cheaper; self's state is untouched here, so it starts fresh (the same bytes).
        // A float map (u64 -> u32) copies in bulk; an oldnew map copies each value apart.
        let nf: usize = ends.iter().map(|e| e.0.len()).sum();
        let no: usize = ends.iter().map(|e| e.1.len()).sum();
        // and long oldnew values (up to a line each): every chunk's start state holds a copy of
        // at most all of them, so the copies are kept under half the block's bytes (a crafted
        // 62 MB log of 60 KB values took 1.2 GB here at 14 threads, against 0.23 GB serial)
        let ob: usize = ends.iter().map(|e| e.1.values().map(|v| v.len()).sum::<usize>()).sum();
        if nf.saturating_mul(chunks.len()) > lines.len().saturating_mul(8)
            || no.saturating_mul(chunks.len()) > lines.len() / 2
            || ob.saturating_mul(chunks.len()) > (z - a) / 2
        {
            return self.forward(data, lines);
        }
        // each chunk's start state: the ends of all earlier chunks, later ones winning (the
        // same overwrites the serial pass makes, in the same order per key)
        let mut sts: Vec<Fstate> = Vec::with_capacity(chunks.len());
        let mut cur: Ends = (FxMap::default(), FxMap::default());
        for k in 0..chunks.len() {
            if k > 0 && stateful {
                for (&key, &v) in &ends[k - 1].0 {
                    cur.0.insert(key, v);
                }
                for (&key, v) in &ends[k - 1].1 {
                    let e = cur.1.entry(key).or_default();
                    e.clear();
                    e.extend_from_slice(v);
                }
            }
            let mut st = Fstate::new(&r);
            if stateful {
                st.floats.last = cur.0.clone();
                st.oldnew.last = cur.1.clone();
            }
            sts.push(st);
        }
        // phase B: each chunk rewritten from its start state (as in forward)
        let parts: Vec<(Vec<u8>, Vec<u32>, usize, usize)> = std::thread::scope(|sc| {
            let hs: Vec<_> = chunks
                .iter()
                .zip(sts)
                .map(|(ch, mut st)| {
                    sc.spawn(move || {
                        let (ca, cz) = (off(ch[0]), off(ch[ch.len() - 1]) + ch[ch.len() - 1].len());
                        let mut text = Vec::with_capacity(cz - ca + 1 + ch.len() / 4);
                        let mut starts = Vec::with_capacity(ch.len());
                        let changed = rewrite_lines(&mut st, &r, data, ch, &mut text, &mut starts);
                        (text, starts, changed, st.mk)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        if parts.iter().all(|p| p.2 == 0) {
            return None;
        }
        let marks: usize = parts.iter().map(|p| p.3).sum();
        // the chunks put together: the starts shifted by the bytes before each chunk (mod 2^32,
        // as the serial pass's `as u32` gives), the texts copied side by side
        let tsize: usize = parts.iter().map(|p| p.0.len()).sum();
        let mut starts = Vec::with_capacity(lines.len() + 1);
        let mut base = 0usize;
        for (t, s, _, _) in &parts {
            starts.extend(s.iter().map(|&x| x.wrapping_add(base as u32)));
            base += t.len();
        }
        starts.push(tsize as u32);
        let mut text = vec![0u8; tsize + super::split::ROOM];
        std::thread::scope(|sc| {
            let mut rest: &mut [u8] = &mut text[..tsize];
            for (t, _, _, _) in &parts {
                let (a, b) = std::mem::take(&mut rest).split_at_mut(t.len());
                rest = b;
                sc.spawn(move || a.copy_from_slice(t));
            }
        });
        drop(parts);
        let mut side = Vec::new();
        put_varint(&mut side, r.side_bits());
        put_varint(&mut side, tsize as u64);
        if r.bits & R_BGL != 0 {
            put_varint(&mut side, zz(r.bgl_off[0]));
            put_varint(&mut side, zz(r.bgl_off[1]));
        }
        // (R_PORTS is off here: no port list)
        Some(Fwd { text, starts, side, marks })
    }
}

// ---------------------------------------------------------------------------------------
// opening

/// The size of the rewritten text (from the side stream).
pub fn text_size(side: &[u8]) -> usize {
    let mut p = 0;
    get_varint(side, &mut p);
    get_varint(side, &mut p) as usize
}

/// Reads the flat ports.
struct Bits<'a> {
    b: &'a [u8],
    p: usize,
    acc: u64,
    nb: u32,
}

impl Bits<'_> {
    #[inline]
    fn take15(&mut self) -> u64 {
        while self.nb < 15 {
            self.acc |= (self.b[self.p] as u64) << self.nb;
            self.p += 1;
            self.nb += 8;
        }
        let v = self.acc & 0x7fff;
        self.acc >>= 15;
        self.nb -= 15;
        v
    }
}

/// The IP lists of the blocks, and which list the line being opened uses (hdfs).
#[derive(Default)]
struct HdfsInv {
    ips: BlockIps,
    cur: usize,  // the list of the last block id read
    kpos: usize, // where that block id's marker is in the input
}

/// Opening side: the rules' state, carried from block to block like the packer's.
#[derive(Default)]
pub struct Opener {
    days: DayCache,
    hd: HdfsInv,
    spark: Spark,
    proxy: Proxy,
    floats: Floats,
    oldnew: OldNew,
}

impl Opener {
    /// Put a block's derived parts back: inp[..n] is its rewritten text (with 16+ zero bytes
    /// after it), the original goes to out[o..] (with 64 bytes of room after it); returns the
    /// end.
    pub fn inverse(&mut self, inp: &[u8], n: usize, side: &[u8], out: &mut [u8], o: usize) -> usize {
        self.hd.cur = usize::MAX;
        self.hd.kpos = usize::MAX;
        inverse(inp, n, side, out, o, self)
    }

    /// `inverse`, or `inverse_bgl` (its checks fused in) for a side stream that holds only the
    /// bgl rule: None where inverse_bgl refuses the text. The harness opens with it (M7), so
    /// every corpus run checks inverse_bgl's bytes against the input.
    pub fn inverse_fused(&mut self, inp: &[u8], n: usize, side: &[u8], out: &mut [u8], o: usize) -> Option<usize> {
        if bgl_side(side).is_some() {
            return self.inverse_bgl(inp, n, side, out, o);
        }
        Some(self.inverse(inp, n, side, out, o))
    }

    /// `inverse` for a side stream that holds only the bgl rule (bits 2, the text's size, the
    /// two offsets), with fewer-pack's safety check of the text (check_derived) made in the same
    /// pass: None where that check refuses the text (bgl_mark) and where `inverse` panics (a
    /// timestamp that is no date and time); otherwise the same bytes and end as `inverse`.
    /// Reads inp[..n] only (next_byte and copy_run read up to 16 bytes past n without using
    /// them: inp needs 16 bytes after n, out 64 after the end, as for `inverse`).
    pub fn inverse_bgl(&mut self, inp: &[u8], n: usize, side: &[u8], out: &mut [u8], mut o: usize) -> Option<usize> {
        let (off, b5) = bgl_side(side)?;
        // the epoch text kept as `inverse` keeps it: the last line's timestamp up to its
        // seconds and its letter, the text of its epoch and the text's length (0: none kept)
        let mut epoch: ((u128, u32, u8), [u8; 16], usize) = ((0, 0, 0), [0; 16], 0);
        let mut i = 0;
        loop {
            let k;
            // (fix3: every write inside out, as `inverse` checks it for such blocks, its
            // `room`: a damaged text is refused)
            if o + n.saturating_sub(i) + 16 > out.len() {
                return None;
            }
            (k, o) = copy_to_mark(inp, i, n, out, o);
            if k >= n {
                return Some(o);
            }
            // NODE ' ' TIMESTAMP -> EPOCH DATE NODE TIMESTAMP NODE (as `inverse`)
            let ne = bgl_node(inp, n, k)?;
            if o + 2 * (ne - k) + 80 > out.len() {
                return None;
            }
            let code = inp[k + 1];
            i = k + 2;
            let ts = &inp[ne + 1..ne + 27];
            let key = (u128::from_le_bytes(ts[..16].try_into().unwrap()), u32::from_le_bytes(ts[15..19].try_into().unwrap()), code);
            if key == epoch.0 && epoch.2 > 0 {
                // (the same 19 timestamp bytes as a line whose date and time were good)
                out[o..o + 16].copy_from_slice(&epoch.1);
                o += epoch.2;
            } else {
                let secs = bgl_secs(ts, &mut self.days)?;
                let e = put_dec(out, o, (secs + off[(code == b'B') as usize]) as u64);
                epoch.2 = 0;
                if e - o <= 16 {
                    epoch = (key, out[o..o + 16].try_into().unwrap(), e - o);
                }
                o = e;
            }
            o = bgl_date(out, o, ts);
            o = copy_run(out, o, inp, i, ne + 27 - i);
            out[o] = b' ';
            o = copy_run(out, o + 1, inp, i, ne - i);
            i = ne + 27;
            if b5 {
                // B5's form (format 2): the fill marker after the timestamp (e: a space
                // follows, E: none), as `inverse` reads it (None where `inverse` panics)
                if i + 1 >= n || inp[i] != MK || (inp[i + 1] != b'e' && inp[i + 1] != b'E') {
                    return None;
                }
                out[o] = b' ';
                o += (inp[i + 1] == b'e') as usize;
                i += 2;
            }
        }
    }
}

/// The two bgl offsets of a side stream that holds only the bgl rule (in B5's form, or in
/// the old form: R_BGL0, fix3), read as `inverse` reads them (fewer-pack checks the side stream's
/// form first), and whether the lines have the fill marker (B5's form); None for other rules.
fn bgl_side(side: &[u8]) -> Option<([i64; 2], bool)> {
    let mut sp = 0;
    let bits = get_varint(side, &mut sp) as u32;
    if bits & !R_BGL0 != R_BGL {
        return None;
    }
    get_varint(side, &mut sp);
    Some(([unzz(get_varint(side, &mut sp)), unzz(get_varint(side, &mut sp))], b5_on() && bits == R_BGL))
}

/// The bgl marker at inp[k] (k < n) checked as fewer-pack's check_derived checks it with only the
/// bgl rule on: its letter inside the text and 'b' or 'B', then the node's ' ' and the 26-byte
/// timestamp after it inside inp[..n]. The node's end, or None.
#[inline(always)]
fn bgl_node(inp: &[u8], n: usize, k: usize) -> Option<usize> {
    if k + 1 >= n || (inp[k + 1] != b'b' && inp[k + 1] != b'B') {
        return None;
    }
    let ne = next_byte(inp, k + 2, n, b' ');
    if ne + 27 > n {
        return None;
    }
    Some(ne)
}

/// The seconds of a bgl timestamp (its date, then its time, as `inverse` reads them; None
/// where `inverse` panics).
#[inline(always)]
fn bgl_secs(ts: &[u8], days: &mut DayCache) -> Option<i64> {
    Some(days.get(ts)? * 86_400 + ts_secs(ts)?)
}

/// ' ' DATE ' ' of a bgl line (as `inverse` writes them) to out[o..]; returns the end.
#[inline(always)]
fn bgl_date(out: &mut [u8], o: usize, ts: &[u8]) -> usize {
    out[o] = b' ';
    out[o + 1..o + 5].copy_from_slice(&ts[..4]);
    out[o + 5] = b'.';
    out[o + 6..o + 8].copy_from_slice(&ts[5..7]);
    out[o + 8] = b'.';
    out[o + 9..o + 11].copy_from_slice(&ts[8..10]);
    out[o + 11] = b' ';
    o + 12
}

/// Whether `inverse` (a fresh state) puts inp[..n], the rewritten text of a block whose side
/// stream holds only the bgl rule, back into exactly `exp` then one '\n' (the block's lines
/// one after the other, each followed by '\n'), and fewer-pack's check_derived takes the text: what
/// `inverse`'s 'b'/'B' arm writes, compared as it goes instead of written out (the same bytes
/// at the same places; no output buffer), with check_derived's checks of each marker made
/// where it makes them (its letter inside the text, then the node's ' ' and the 26-byte
/// timestamp after it inside inp[..n]), and refused where `inverse` panics (a timestamp
/// that is no date and time). inp needs 16 bytes after n.
pub fn inverts_bgl(inp: &[u8], n: usize, side: &[u8], exp: &[u8]) -> bool {
    let Some((off, b5)) = bgl_side(side) else { return false };
    let size = exp.len() + 1;
    // whether the output's bytes from o on are b (exp, then the '\n' after it at size - 1)
    #[inline(always)]
    fn at(exp: &[u8], o: usize, b: &[u8]) -> bool {
        let (e, size) = (o + b.len(), exp.len() + 1);
        if e < size {
            same(&exp[o..e], b)
        } else if e == size {
            b.split_last().map_or(true, |(&l, h)| l == b'\n' && same(&exp[o..e - 1], h))
        } else {
            false
        }
    }
    let mut days = DayCache::default();
    let (mut i, mut o) = (0, 0);
    // the last marker's head (EPOCH ' ' DATE ' ', hl bytes; 0: none yet) and what it is made
    // of: the timestamp up to its seconds and the marker's letter (`inverse`'s epoch key)
    let (mut head, mut hl, mut key) = ([0u8; 64], 0usize, (0u128, 0u32, 0u8));
    loop {
        let k = next_byte(inp, i, n, MK);
        if !at(exp, o, &inp[i..k]) {
            return false;
        }
        o += k - i;
        if k >= n {
            return o == size;
        }
        // the marker's checks (the timestamp's date and time only for a new key: the same
        // key gives the same answer and the same head)
        if k + 1 >= n || (inp[k + 1] != b'b' && inp[k + 1] != b'B') {
            return false;
        }
        let ne = next_byte(inp, k + 2, n, b' ');
        if ne + 27 > n {
            return false;
        }
        let ts = &inp[ne + 1..ne + 27];
        let kk = (u128::from_le_bytes(ts[..16].try_into().unwrap()), u32::from_le_bytes(ts[15..19].try_into().unwrap()), inp[k + 1]);
        if hl == 0 || kk != key {
            let Some(d) = days.get(ts) else { return false };
            let Some(t) = ts_secs(ts) else { return false };
            hl = bgl_head(&mut head, 0, d * 86_400 + t + off[(kk.2 == b'B') as usize], ts);
            key = kk;
        }
        i = k + 2;
        if !at(exp, o, &head[..hl]) || !at(exp, o + hl, &inp[i..ne + 27]) {
            return false;
        }
        o += hl + ne + 27 - i;
        if !at(exp, o, b" ") || !at(exp, o + 1, &inp[i..ne]) {
            return false;
        }
        o += 1 + ne - i;
        i = ne + 27;
        if b5 {
            // B5's form (format 2): the fill marker after the timestamp (e: a space follows,
            // E: none), as `inverse` reads it
            if i + 1 >= n || inp[i] != MK || (inp[i + 1] != b'e' && inp[i + 1] != b'E') {
                return false;
            }
            if inp[i + 1] == b'e' {
                if !at(exp, o, b" ") {
                    return false;
                }
                o += 1;
            }
            i += 2;
        }
    }
}

/// x == y for short pieces, without a library call: 16 bytes at a time (the last 16
/// overlapping), else 8 (two overlapping), else byte by byte.
#[inline(always)]
fn same(x: &[u8], y: &[u8]) -> bool {
    let n = x.len();
    if n != y.len() {
        return false;
    }
    if n >= 16 {
        let w = |s: &[u8], at: usize| u128::from_le_bytes(s[at..at + 16].try_into().unwrap());
        let mut k = 0;
        while k + 16 < n {
            if w(x, k) != w(y, k) {
                return false;
            }
            k += 16;
        }
        return w(x, n - 16) == w(y, n - 16);
    }
    if n >= 8 {
        let w = |s: &[u8], at: usize| u64::from_le_bytes(s[at..at + 8].try_into().unwrap());
        return w(x, 0) == w(y, 0) && w(x, n - 8) == w(y, n - 8);
    }
    x.iter().zip(y).all(|(a, b)| a == b)
}

/// EPOCH ' ' DATE ' ' of a bgl line (as `inverse` writes them) to out[o..]; returns the end.
#[inline(always)]
fn bgl_head(out: &mut [u8], o: usize, epoch: i64, ts: &[u8]) -> usize {
    let o = put_dec(out, o, epoch as u64);
    bgl_date(out, o, ts)
}

/// Copy src[s..s + len] to out[o..] (16 bytes at a time: both have room after the ends).
#[inline(always)]
fn copy_run(out: &mut [u8], o: usize, src: &[u8], s: usize, len: usize) -> usize {
    wild_copy(out, o, src, s, len);
    o + len
}

/// next_byte(inp, i, end, MK) and copy_run of the text before it, in one pass: each 16 bytes
/// are read once, tested for MK and copied; the same bytes are written as copy_run writes
/// (whole 16-byte pieces from i, up to the one holding the marker or end). Returns the
/// marker's place (or end) and the end in out.
#[inline(always)]
fn copy_to_mark(inp: &[u8], mut i: usize, end: usize, out: &mut [u8], mut o: usize) -> (usize, usize) {
    if i > end {
        // (damaged input only: as the two steps do it)
        let k = next_byte(inp, i, end, MK);
        return (k, copy_run(out, o, inp, i, k.wrapping_sub(i)));
    }
    while i < end {
        let x: [u8; 16] = inp[i..i + 16].try_into().unwrap();
        let m = eq16(&x, 0, MK);
        if m != 0 {
            let k = (i + (m.trailing_zeros() / 4) as usize).min(end);
            if k > i {
                out[o..o + 16].copy_from_slice(&x);
            }
            return (k, o + (k - i));
        }
        out[o..o + 16].copy_from_slice(&x);
        i += 16;
        o += 16;
    }
    // (i went past end by i - end: o did too)
    (end, o - (i - end))
}

fn inverse(inp: &[u8], n: usize, side: &[u8], out: &mut [u8], mut o: usize, op: &mut Opener) -> usize {
    let mut sp = 0;
    let bits = get_varint(side, &mut sp) as u32;
    get_varint(side, &mut sp);
    let mut bgl_off = [0i64; 2];
    if bits & R_BGL != 0 {
        bgl_off = [unzz(get_varint(side, &mut sp)), unzz(get_varint(side, &mut sp))];
    }
    let mut ports = Bits { b: side, p: sp, acc: 0, nb: 0 };
    // (the ports bit without hdfs: R_BGL0, bgl lines in the old form, no port list)
    let old = bits & (R_HDFS | R_BGL0) == R_BGL0;
    if bits & R_PORTS != 0 && !old {
        get_varint(side, &mut ports.p);
    }
    let Opener { days, hd, spark, proxy, floats, oldnew } = op;
    let b5 = b5_on() && !old;
    // fix3: a block of bgl lines alone (either form) or in the old form keeps every write of
    // its copies and its bgl markers inside out (as inverse_bgl; a damaged text gives an end
    // past out, which every caller refuses): the copy up to the next marker writes at most
    // n - i + 15 bytes, a bgl marker at most 2 x (its node + 2) + 75; a block that opens
    // passes (its lines' remaining output is at least their remaining text, but for spark's
    // markers, which the packer's own check of such a block then refuses the same way)
    let room = bits & R_BGL != 0 && (old || bits == R_BGL);
    // bgl: the last line's timestamp up to its seconds and its letter (b or B), the text of
    // its epoch (16 bytes) and the text's length (0: none kept)
    let mut epoch: ((u128, u32, u8), [u8; 16], usize) = ((0, 0, 0), [0; 16], 0);
    let mut i = 0;
    loop {
        let k;
        if room && o + n.saturating_sub(i) + 16 > out.len() {
            return usize::MAX;
        }
        (k, o) = copy_to_mark(inp, i, n, out, o);
        if k >= n {
            return o;
        }
        let code = inp[k + 1];
        i = k + 2;
        match code {
            b'a' => {
                // PID and TID, each right-aligned to 5 columns
                for f in 0..2 {
                    let (e, _) = digits(inp, i);
                    let w = e - i;
                    if w < 5 {
                        out[o..o + 8].copy_from_slice(b"        ");
                        o += 5 - w;
                    }
                    o = copy_run(out, o, inp, i, w);
                    i = e;
                    if f == 0 {
                        out[o] = b' ';
                        o += 1;
                        i += 1;
                    }
                }
            }
            b'b' | b'B' => {
                // NODE ' ' TIMESTAMP -> EPOCH DATE NODE TIMESTAMP NODE
                let ne = match next_byte(inp, i, n, b' ') {
                    q if q < n => q,
                    _ => i + inp[i..].iter().position(|&c| c == b' ').expect("bgl node"),
                };
                if room && o + 2 * (ne - k) + 80 > out.len() {
                    return usize::MAX;
                }
                let ts = &inp[ne + 1..ne + 27];
                // the timestamp up to its seconds and the letter: the same as the line before's
                // (most lines) give the same epoch, so its text is written again
                let key = (u128::from_le_bytes(ts[..16].try_into().unwrap()), u32::from_le_bytes(ts[15..19].try_into().unwrap()), code);
                if key == epoch.0 && epoch.2 > 0 {
                    out[o..o + 16].copy_from_slice(&epoch.1);
                    o += epoch.2;
                } else {
                    // (a timestamp that is no date and time: an end past out, which every
                    // caller refuses, fix3; check_marks does not read the dates)
                    let Some(secs) = bgl_secs(ts, days) else { return usize::MAX };
                    let e = put_dec(out, o, (secs + bgl_off[(code == b'B') as usize]) as u64);
                    epoch.2 = 0;
                    if e - o <= 16 {
                        epoch = (key, out[o..o + 16].try_into().unwrap(), e - o);
                    }
                    o = e;
                }
                out[o] = b' ';
                out[o + 1..o + 5].copy_from_slice(&ts[..4]);
                out[o + 5] = b'.';
                out[o + 6..o + 8].copy_from_slice(&ts[5..7]);
                out[o + 8] = b'.';
                out[o + 9..o + 11].copy_from_slice(&ts[8..10]);
                out[o + 11] = b' ';
                o = copy_run(out, o + 12, inp, i, ne + 27 - i);
                out[o] = b' ';
                o = copy_run(out, o + 1, inp, i, ne - i);
                i = ne + 27;
                if b5 {
                    // B5's form: the fill marker after the timestamp (e: a space follows)
                    let f = inp[i + 1];
                    assert!(inp[i] == MK && (f == b'e' || f == b'E'), "bgl fill marker");
                    out[o] = b' ';
                    o += (f == b'e') as usize;
                    i += 2;
                }
            }
            b'k' => {
                let (e, key) = blk_at(inp, i).expect("block id");
                if hd.kpos != k {
                    hd.cur = hd.ips.get(key);
                    hd.kpos = k;
                }
                o = copy_run(out, o, inp, i, e - i);
                i = e;
            }
            b'q' => o = put_dec(out, o, PORT_LO + ports.take15()),
            b'h' | b'H' | b'i' | b'I' => {
                if code == b'i' || code == b'I' {
                    // the line's block id comes later in the line: its marker is the next 'k'
                    let mut q = i;
                    loop {
                        let m = next_byte(inp, q, n, MK);
                        assert!(m < n, "block marker");
                        if inp[m + 1] == b'k' {
                            let (_, key) = blk_at(inp, m + 2).expect("block id");
                            hd.cur = hd.ips.get(key);
                            hd.kpos = m;
                            break;
                        }
                        q = m + 2;
                    }
                }
                let lst = &mut hd.ips.lists[hd.cur];
                if code == b'h' || code == b'i' {
                    o = put_ipv4(out, o, lst.0[(inp[i] - b'0') as usize]);
                    i += 1;
                } else {
                    let (e, ip) = ipv4_at(inp, i).expect("new ip");
                    lst.0[lst.1 as usize] = ip;
                    lst.1 += 1;
                    o = copy_run(out, o, inp, i, e - i);
                    i = e;
                }
            }
            b's' => {
                let (e, v) = digits(inp, i);
                spark.assign(v);
                o = copy_run(out, o, inp, i, e - i);
                i = e;
            }
            b'r' => {
                // X " in stage " S " (TID" [" " N] ")": no N = the last assigned task
                let xe = i + inp[i..].iter().position(|&c| c == b' ').expect("task");
                let x = Tok::new(&inp[i..xe]).expect("task");
                let ss = xe + 10;
                let se = ss + inp[ss..].iter().position(|&c| c == b' ').expect("stage");
                let s = Tok::new(&inp[ss..se]).expect("stage");
                let v = if inp[se + 5] == b')' || inp[se + 6] == MK {
                    // the newest assigned task, or the k-th newest (" (TID " MK 'u' k)
                    let k = if inp[se + 5] == b')' { 0 } else { digits(inp, se + 8).1 as usize };
                    let v = spark.assigned[spark.assigned.len() - 1 - k];
                    o = copy_run(out, o, inp, i, se + 5 - i); // ... " (TID"
                    out[o] = b' ';
                    o = put_dec(out, o + 1, v);
                    i = if k == 0 && inp[se + 5] == b')' { se + 5 } else { digit_end(inp, se + 8) };
                    v
                } else {
                    let (ne, v) = digits(inp, se + 6);
                    o = copy_run(out, o, inp, i, ne - i);
                    i = ne;
                    v
                };
                spark.unassign(v);
                spark.run(Task { x, s, n: v, skips: 0 });
            }
            b'f' => {
                let (e, r) = digits(inp, i);
                let t = spark.finish(r as usize);
                o = put(out, o, t.x.get());
                o = put(out, o, b" in stage ");
                o = put(out, o, t.s.get());
                o = put(out, o, b" (TID ");
                o = put_dec(out, o, t.n);
                out[o] = b')';
                o += 1;
                i = e;
            }
            b'o' => {
                // an open: noted (its text goes on as it is)
                let t = proxy_time(&out[o - 17..o]).expect("proxy clock");
                let (a, hs, he) = proxy_app_host(&inp[..n], i).expect("proxy app");
                proxy.open(Open { t, app: inp[i..a].to_vec(), host: inp[hs..he].to_vec() });
            }
            b'c' => {
                let (e, r) = digits(inp, i);
                let t = proxy_time(&out[o - 17..o]).expect("proxy clock");
                let op = proxy.pending.remove(proxy.pending.len() - 1 - r as usize);
                o = put(out, o, &op.app);
                o = put(out, o, b" - ");
                o = put(out, o, &op.host);
                proxy.base = t - op.t;
                i = e;
            }
            b'l' => {
                let r = (inp[i] - b'0') as i64 - 2;
                o = life(proxy.base - r, out, o);
                i += 1;
            }
            b'g' => {
                // KEY is the word before " is : " (already written)
                let ke = o - 6;
                let ks = out[..ke].iter().rposition(|&c| c == b' ' || c == b'\n').map_or(0, |p| p + 1);
                let key = key_hash(&out[ks..ke]);
                let neg = inp[i] == b'-';
                let (e, d) = digits(inp, i + neg as usize);
                let last = floats.last.get(&key).copied().unwrap_or(0);
                let fb = (last as i64 + if neg { -(d as i64) } else { d as i64 }) as u32;
                floats.last.insert(key, fb);
                o = java_float(fb, out, o).expect("float");
                i = e;
            }
            b'w' => {
                // T ',' NEW ",old=" then OLD, or a marker: OLD = the last NEW of type T
                let (te, tv) = digits(inp, i);
                let ne = te + 1 + inp[te + 1..].iter().position(|&c| c == b',').expect("new");
                let os = ne + 5;
                o = copy_run(out, o, inp, i, os - i);
                let e = oldnew.last.entry(tv).or_default();
                i = os;
                if inp[os] == MK && inp[os + 1] == b'x' {
                    o = put(out, o, e);
                    i = os + 2;
                }
                e.clear();
                e.extend_from_slice(&inp[te + 1..ne]);
            }
            b't' => {
                // the year; weekday and time from the line's header
                let (e, y) = digits(inp, i);
                let ls = out[..o].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
                let h: [u8; 15] = out[ls..ls + 15].try_into().unwrap();
                o = put(out, o, weekday(&h, y as i64).expect("weekday"));
                out[o] = b' ';
                o = put(out, o + 1, &h);
                out[o] = b' ';
                o = copy_run(out, o + 1, inp, i, e - i);
                i = e;
            }
            b'n' => {
                // the IP: the last one before in the line (written), or the first after it
                let (pos, ord, pad) = (inp[i], inp[i + 1], inp[i + 2]);
                i += 3;
                let mut ip = None;
                if pos == b'b' {
                    let ls = out[..o].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
                    ipv4s(out, ls, o, &mut |_, _, v| {
                        ip = Some(v);
                        true
                    });
                } else {
                    let le = next_byte(inp, i, n, b'\n');
                    ipv4s(inp, i, le, &mut |_, _, v| {
                        ip = Some(v);
                        false
                    });
                }
                o = put_dash4(out, o, ip.expect("rdns ip"), ord, pad);
            }
            _ => panic!("derive: unknown marker {}", code),
        }
    }
}

/// FL_DERIVE_BENCH=1 (testing): only the derive passes on each file, in 100K-line blocks
/// (one block up to `one_max` lines), timed (thread CPU time with FL_CPUTIME), and checked;
/// FL_DERIVE_BENCH=2: the rewrite only, 10 times (for a profiler).
pub fn bench(files: &[String], one_max: usize) {
    use super::clock::Tick;
    let (mut tf, mut ti, mut tin) = (0f64, 0f64, 0usize);
    for f in files {
        let data = std::fs::read(f).unwrap();
        let body = data.strip_suffix(b"\n").unwrap_or(&data);
        let lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
        let per = if lines.len() <= one_max { lines.len().max(1) } else { 100_000 };
        let t0 = Tick::now();
        let r = detect(&lines);
        let td = t0.elapsed().as_secs_f64();
        if crate::engine::fl::no_env::var("FL_DERIVE_BENCH").as_deref() == Ok("2") {
            // profiling: the rewrite only, 10 times
            for _ in 0..10 {
                let mut pk = Packer::new(r);
                let n: usize = lines.chunks(per).map(|b| pk.forward(&data, b).map_or(0, |x| x.starts.len())).sum();
                std::hint::black_box(n);
            }
            continue;
        }
        let mut pk = Packer::new(r);
        let fws: Vec<Option<Fwd>> = lines.chunks(per).map(|b| pk.forward(&data, b)).collect();
        let t1 = t0.elapsed().as_secs_f64();
        let mut back = vec![0u8; data.len() + 64];
        let mut bo = 0;
        let mut op = Opener::default();
        let t2 = Tick::now();
        for (b, fw) in lines.chunks(per).zip(&fws) {
            match fw {
                Some(x) => {
                    let n = *x.starts.last().unwrap() as usize;
                    bo = op.inverse(&x.text, n, &x.side, &mut back, bo);
                }
                None => {
                    for l in b {
                        back[bo..bo + l.len()].copy_from_slice(l);
                        bo += l.len();
                        back[bo] = b'\n';
                        bo += 1;
                    }
                }
            }
        }
        let t3 = t2.elapsed().as_secs_f64();
        back.truncate(bo);
        if !data.ends_with(b"\n") {
            back.pop();
        }
        let tsz: usize = fws.iter().map(|x| x.as_ref().map_or(0, |x| *x.starts.last().unwrap() as usize)).sum();
        println!("{:<24} rules {:4} detect {:.4}s rewrite {:.4}s ({:.0} MB/s) put back {:.4}s ({:.0} MB/s) | rewritten {} of {} B | {}", std::path::Path::new(f).file_name().unwrap().to_string_lossy(), r.bits, td, t1 - td, data.len() as f64 / 1e6 / (t1 - td), t3, data.len() as f64 / 1e6 / t3, tsz, data.len(), if back == data { "EXACT" } else { "NOT EXACT" });
        tf += t1;
        ti += t3;
        tin += data.len();
    }
    println!("TOTAL {:.1} MB: rewrite {:.3}s, put back {:.3}s", tin as f64 / 1e6, tf, ti);
}
