//! x86_64 versions of the engine's NEON fast paths, with exactly the results of the plain code:
//! - line ends: each_nl (the line loops: fewer-pack's each_line, the engine's split_lines) and
//!   nl_mask64 (front.rs newlines64),
//! - the splitters' byte classes: split_masks (split.rs masks64), front_masks, front_masks_a,
//!   front_masks_b (front.rs masks64, masks64a, masks64b),
//! - byte search: eq16 (derive.rs, the NEON nibble mask), has_byte, pair_any64 (derive.rs
//!   find_seq_inl's look at 64 places),
//! - twin lines: same_classes and skel_hash (front.rs), and the shape hash (shape_hash_crc).
//!
//! SSE2 is part of every x86_64 processor, so the SSE2 kernels need no check. SSE4.2 (its
//! CRC32-C instruction: the same one ARM's __crc32cd is, so the two hashes give ARM's values)
//! and AVX2 (32 bytes per compare in the long loops: each_nl, has_byte) are found at run time
//! by is_x86_feature_detected!, once (LEVEL). The hashes only place entries in tables; they
//! never change the output.
//!
//! Every path can be run on one machine (tests): build with --cfg fewer_simd="avx2" (the
//! default: everything the processor has), "sse42" (no AVX2), "sse2" (SSE2 only) or "scalar"
//! (the engine's plain code instead of these kernels: then this module is not built).
#![allow(unused_unsafe)]

use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU8, Ordering};

/// The levels; each holds the ones below it.
pub const SSE2: u8 = 1;
pub const SSE42: u8 = 2;
pub const AVX2: u8 = 3;

/// The highest level this build uses (cfg fewer_simd, for tests).
const CAP: u8 = if cfg!(fewer_simd = "sse2") {
    SSE2
} else if cfg!(fewer_simd = "sse42") {
    SSE42
} else {
    AVX2
};

/// The level found (0: not yet).
static LEVEL: AtomicU8 = AtomicU8::new(0);

/// This processor's level (at most CAP), found at the first call.
#[inline(always)]
pub fn level() -> u8 {
    match LEVEL.load(Ordering::Relaxed) {
        0 => detect(),
        l => l,
    }
}

#[cold]
#[inline(never)]
fn detect() -> u8 {
    let mut l = SSE2;
    if std::arch::is_x86_feature_detected!("sse4.2") {
        l = SSE42;
        if std::arch::is_x86_feature_detected!("avx2") {
            l = AVX2;
        }
    }
    let l = l.min(CAP);
    LEVEL.store(l, Ordering::Relaxed);
    l
}

// ---- SSE2 helpers (SSE2 is always there on x86_64: no check) ----
// (The unsafe blocks around SSE2 arithmetic, compares and movemasks touch no memory: older
// compilers mark those intrinsics unsafe, newer ones safe. Only the loads read memory; each
// says what must be readable.)

/// The 16 bytes at p. SAFETY: p[..16] must be readable (any alignment).
#[inline(always)]
unsafe fn ld(p: *const u8) -> __m128i {
    unsafe { _mm_loadu_si128(p as *const __m128i) }
}

/// The 64 bytes at p as 4 vectors. SAFETY: p[..64] must be readable.
#[inline(always)]
unsafe fn ld64(p: *const u8) -> [__m128i; 4] {
    unsafe { [ld(p), ld(p.add(16)), ld(p.add(32)), ld(p.add(48))] }
}

#[inline(always)]
fn splat(b: u8) -> __m128i {
    unsafe { _mm_set1_epi8(b as i8) }
}

/// 0xFF in each byte of x equal to b, else 0.
#[inline(always)]
fn eqb(x: __m128i, b: u8) -> __m128i {
    unsafe { _mm_cmpeq_epi8(x, splat(b)) }
}

/// 0xFF in each byte of x at most m (unsigned), else 0: min(x, m) == x.
#[inline(always)]
fn le(x: __m128i, m: u8) -> __m128i {
    unsafe { _mm_cmpeq_epi8(_mm_min_epu8(x, splat(m)), x) }
}

/// Digits: x - '0' < 10 (wrapping, unsigned), as the plain code tests it.
#[inline(always)]
fn digit(x: __m128i) -> __m128i {
    unsafe { le(_mm_sub_epi8(x, splat(b'0')), 9) }
}

/// Hex letters: (x | 0x20) - 'a' < 6.
#[inline(always)]
fn hexl(x: __m128i) -> __m128i {
    unsafe { le(_mm_sub_epi8(_mm_or_si128(x, splat(0x20)), splat(b'a')), 5) }
}

/// Capitals: x - 'A' < 26.
#[inline(always)]
fn cap(x: __m128i) -> __m128i {
    unsafe { le(_mm_sub_epi8(x, splat(b'A')), 25) }
}

/// Letters: (x | 0x20) - 'a' < 26.
#[inline(always)]
fn letter(x: __m128i) -> __m128i {
    unsafe { le(_mm_sub_epi8(_mm_or_si128(x, splat(0x20)), splat(b'a')), 25) }
}

/// Small letters: x - 'a' < 26.
#[inline(always)]
fn small(x: __m128i) -> __m128i {
    unsafe { le(_mm_sub_epi8(x, splat(b'a')), 25) }
}

/// One bit per byte of four compare results (bytes 0 or 0xFF): bit i = byte i of a, b, c, d
/// in turn (pmovmskb gives byte k's top bit as bit k).
#[inline(always)]
fn bits64(a: __m128i, b: __m128i, c: __m128i, d: __m128i) -> u64 {
    unsafe {
        (_mm_movemask_epi8(a) as u32 & 0xFFFF) as u64
            | ((_mm_movemask_epi8(b) as u32 & 0xFFFF) as u64) << 16
            | ((_mm_movemask_epi8(c) as u32 & 0xFFFF) as u64) << 32
            | ((_mm_movemask_epi8(d) as u32 & 0xFFFF) as u64) << 48
    }
}

/// The mask of class f over the 64 bytes in v.
#[inline(always)]
fn class(v: &[__m128i; 4], f: impl Fn(__m128i) -> __m128i) -> u64 {
    bits64(f(v[0]), f(v[1]), f(v[2]), f(v[3]))
}

/// Whether any of the 64 bytes in v is b.
#[inline(always)]
fn any_eq(v: &[__m128i; 4], b: u8) -> bool {
    unsafe {
        let o = _mm_or_si128(_mm_or_si128(eqb(v[0], b), eqb(v[1], b)), _mm_or_si128(eqb(v[2], b), eqb(v[3], b)));
        _mm_movemask_epi8(o) != 0
    }
}

// ---- the splitters' masks (bit i = byte i of the 64 bytes at p) ----

/// split.rs masks64: digits, spaces, hex letters, dashes, capitals, letters, 0 bytes.
/// SAFETY: p[..64] must be readable.
#[inline(always)]
pub unsafe fn split_masks(p: *const u8) -> [u64; 7] {
    let v = unsafe { ld64(p) };
    [class(&v, digit), class(&v, |x| eqb(x, b' ')), class(&v, hexl), class(&v, |x| eqb(x, b'-')), class(&v, cap), class(&v, letter), class(&v, |x| eqb(x, 0))]
}

/// front.rs masks64: digits, spaces, hex letters, dashes, capitals, small letters; and whether
/// a 0 byte is among them. SAFETY: p[..64] must be readable.
#[inline(always)]
pub unsafe fn front_masks(p: *const u8) -> ([u64; 6], bool) {
    let v = unsafe { ld64(p) };
    ([class(&v, digit), class(&v, |x| eqb(x, b' ')), class(&v, hexl), class(&v, |x| eqb(x, b'-')), class(&v, cap), class(&v, small)], any_eq(&v, 0))
}

/// front.rs masks64a: front_masks's first four masks and its 0-byte flag.
/// SAFETY: p[..64] must be readable.
#[inline(always)]
pub unsafe fn front_masks_a(p: *const u8) -> ([u64; 4], bool) {
    let v = unsafe { ld64(p) };
    ([class(&v, digit), class(&v, |x| eqb(x, b' ')), class(&v, hexl), class(&v, |x| eqb(x, b'-'))], any_eq(&v, 0))
}

/// front.rs masks64b: front_masks's last two masks (capitals, small letters).
/// SAFETY: p[..64] must be readable.
#[inline(always)]
pub unsafe fn front_masks_b(p: *const u8) -> [u64; 2] {
    let v = unsafe { ld64(p) };
    [class(&v, cap), class(&v, small)]
}

/// front.rs newlines64: the '\n' mask of the 64 bytes at p (0 at once when there is none: most
/// 64-byte pieces hold at most one). SAFETY: p[..64] must be readable.
#[inline(always)]
pub unsafe fn nl_mask64(p: *const u8) -> u64 {
    let v = unsafe { ld64(p) };
    let (a, b, c, d) = (eqb(v[0], b'\n'), eqb(v[1], b'\n'), eqb(v[2], b'\n'), eqb(v[3], b'\n'));
    if unsafe { _mm_movemask_epi8(_mm_or_si128(_mm_or_si128(a, b), _mm_or_si128(c, d))) } == 0 {
        return 0;
    }
    bits64(a, b, c, d)
}

// ---- byte search (derive.rs) ----

/// derive.rs eq16: the bytes of the 16 at p equal to b as a nibble mask (bits 4k..4k + 3 set
/// when byte k is b), the NEON code's mask (its narrowing shift): each 16-bit lane of the
/// compare (bytes 2j, 2j + 1) shifted right by 4 holds byte 2j's result in the low nibble of
/// its low byte and byte 2j + 1's in the high nibble; packing the low bytes of the 8 lanes gives
/// nibble k = byte k. SAFETY: p[..16] must be readable.
#[inline(always)]
pub unsafe fn eq16(p: *const u8, b: u8) -> u64 {
    unsafe {
        let m = eqb(ld(p), b);
        let t = _mm_and_si128(_mm_srli_epi16::<4>(m), _mm_set1_epi16(0x00FF));
        _mm_cvtsi128_si64(_mm_packus_epi16(t, t)) as u64
    }
}

/// derive.rs has_byte: whether h holds byte b (64 bytes per step: AVX2 as two compares of 32,
/// else four of 16; the rest one by one).
pub fn has_byte(h: &[u8], b: u8) -> bool {
    let (p, n) = (h.as_ptr(), h.len());
    let mut i = 0;
    if level() >= AVX2 {
        // SAFETY: the processor has AVX2 (level); any64_avx2 reads p[i..i + 64] only while
        // i + 64 <= n = h.len()
        let (found, at) = unsafe { any64_avx2(p, n, b) };
        if found {
            return true;
        }
        i = at;
    }
    while i + 64 <= n {
        // SAFETY: the loads read h[i..i + 64], inside h (i + 64 <= n)
        if any_eq(&unsafe { ld64(p.add(i)) }, b) {
            return true;
        }
        i += 64;
    }
    h[i..].contains(&b)
}

/// has_byte's AVX2 loop: (found, the place it stopped: n - place < 64 when not found).
/// SAFETY: p[..n] must be readable; the processor must have AVX2.
#[target_feature(enable = "avx2")]
unsafe fn any64_avx2(p: *const u8, n: usize, b: u8) -> (bool, usize) {
    unsafe {
        let v = _mm256_set1_epi8(b as i8);
        let mut i = 0;
        while i + 64 <= n {
            // (p[i..i + 64] is inside p[..n])
            let x = _mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i) as *const __m256i), v);
            let y = _mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i + 32) as *const __m256i), v);
            if _mm256_movemask_epi8(_mm256_or_si256(x, y)) != 0 {
                return (true, i);
            }
            i += 64;
        }
        (false, i)
    }
}

/// derive.rs find_seq_inl's look at 64 places: whether some o in 0..64 has p[o] == f and
/// p[k + o] == l. SAFETY: p[..k + 64] must be readable.
#[inline(always)]
pub unsafe fn pair_any64(p: *const u8, k: usize, f: u8, l: u8) -> bool {
    unsafe {
        let pair = |o: usize| _mm_and_si128(eqb(ld(p.add(o)), f), eqb(ld(p.add(k + o)), l));
        _mm_movemask_epi8(_mm_or_si128(_mm_or_si128(pair(0), pair(16)), _mm_or_si128(pair(32), pair(48)))) != 0
    }
}

// ---- line ends ----

/// The '\n's of p[*i..to], in order: g(e) for the one at e. 64 bytes per step (AVX2: two
/// compares of 32; else four of 16), then 16 per step; *i is left at the first byte not looked
/// at (to - *i < 16; the caller looks at the rest).
/// SAFETY: p[..to] must be readable and *i <= to.
#[inline(always)]
pub unsafe fn each_nl(p: *const u8, i: &mut usize, to: usize, mut g: impl FnMut(usize)) {
    if level() >= AVX2 {
        // SAFETY: the processor has AVX2 (level); the same bounds as here
        unsafe { each_nl_avx2(p, i, to, &mut g) };
    }
    let mut k = *i;
    while k + 64 <= to {
        // SAFETY: p[k..k + 64] is inside p[..to]
        let v = unsafe { ld64(p.add(k)) };
        let mut m = bits64(eqb(v[0], b'\n'), eqb(v[1], b'\n'), eqb(v[2], b'\n'), eqb(v[3], b'\n'));
        while m != 0 {
            g(k + m.trailing_zeros() as usize);
            m &= m - 1;
        }
        k += 64;
    }
    while k + 16 <= to {
        // SAFETY: p[k..k + 16] is inside p[..to]
        let mut m = unsafe { _mm_movemask_epi8(eqb(ld(p.add(k)), b'\n')) } as u32 & 0xFFFF;
        while m != 0 {
            g(k + m.trailing_zeros() as usize);
            m &= m - 1;
        }
        k += 16;
    }
    *i = k;
}

/// each_nl's AVX2 loop (64 bytes per step); leaves *i at the first byte not looked at
/// (to - *i < 64). SAFETY: p[..to] must be readable, *i <= to; the processor must have AVX2.
#[target_feature(enable = "avx2")]
unsafe fn each_nl_avx2(p: *const u8, i: &mut usize, to: usize, g: &mut impl FnMut(usize)) {
    unsafe {
        let nl = _mm256_set1_epi8(b'\n' as i8);
        let mut k = *i;
        while k + 64 <= to {
            // (p[k..k + 64] is inside p[..to])
            let a = _mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(k) as *const __m256i), nl);
            let b = _mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(k + 32) as *const __m256i), nl);
            let mut m = _mm256_movemask_epi8(a) as u32 as u64 | (_mm256_movemask_epi8(b) as u32 as u64) << 32;
            while m != 0 {
                g(k + m.trailing_zeros() as usize);
                m &= m - 1;
            }
            k += 64;
        }
        *i = k;
    }
}

// ---- twin lines and shapes (front.rs) ----

/// front.rs same_classes: whether the n bytes at a and at b have their digits at the same
/// places and the same other bytes. SAFETY: a[..n + 15] and b[..n + 15] must be readable (16
/// bytes are read at each p < n).
#[inline(always)]
pub unsafe fn same_classes(a: *const u8, b: *const u8, n: usize) -> bool {
    let mut p = 0;
    loop {
        // SAFETY: p < n (or p = 0 = n): a[p..p + 16] and b[p..p + 16] are inside a[..n + 15], b[..n + 15]
        let (x, y) = unsafe { (ld(a.add(p)), ld(b.add(p))) };
        let (dx, dy) = (digit(x), digit(y));
        // a byte is fine when both or neither are digits, and neither are or the two are equal
        // (all masks 0 or 0xFF bytes: pmovmskb reads only each byte's top bit, so the NEON
        // code's x ^ y, any bits, is an equality compare here)
        let ok = unsafe { _mm_andnot_si128(_mm_xor_si128(dx, dy), _mm_or_si128(dx, _mm_cmpeq_epi8(x, y))) };
        // the places that are not fine: a digit place differs, or a byte that is not a digit
        let mut m = !(unsafe { _mm_movemask_epi8(ok) } as u32) & 0xFFFF;
        if p + 16 > n {
            // the last bytes: only the n - p < 16 left count
            m &= (1u32 << (n - p)) - 1;
        }
        if m != 0 {
            return false;
        }
        p += 16;
        if p >= n {
            return true;
        }
    }
}

/// CRC32-C of the 8 bytes x after h (the SSE4.2 instruction; ARM's __crc32cd).
#[inline]
#[target_feature(enable = "sse4.2")]
unsafe fn c8(h: u32, x: u64) -> u32 {
    unsafe { _mm_crc32_u64(h as u64, x) as u32 }
}

/// CRC32-C of the 4 bytes x after h (ARM's __crc32cw).
#[inline]
#[target_feature(enable = "sse4.2")]
unsafe fn c4(h: u32, x: u32) -> u32 {
    unsafe { _mm_crc32_u32(h, x) }
}

/// front.rs skel_key's hash, as the NEON code makes it (the same value): the line's length and
/// its first, middle and last 16 bytes with every digit read as '0' (when n < 16, only the
/// line's n bytes count), two CRC32-C lanes, mixed; the caller keeps the top bits.
/// SAFETY: lp[..max(n, 16)] must be readable; the processor must have SSE4.2.
#[target_feature(enable = "sse4.2")]
pub unsafe fn skel_hash(lp: *const u8, n: usize) -> u64 {
    unsafe {
        let mid = n.max(16) / 2 - 8; // (the middle 16 bytes; the first ones when n < 16)
        // (mid + 16 <= max(n, 16) and max(n, 16) - 16 + 16 = max(n, 16): inside lp[..max(n, 16)])
        let (x, y, z) = (ld(lp), ld(lp.add(n.max(16) - 16)), ld(lp.add(mid)));
        let c0 = splat(b'0');
        // digits become '0' (the NEON bit select)
        let norm = |v: __m128i| {
            let d = digit(v);
            _mm_or_si128(_mm_and_si128(d, c0), _mm_andnot_si128(d, v))
        };
        let (mut nx, mut ny, mut nz) = (norm(x), norm(y), norm(z));
        if n < 16 {
            // (all three are the first 16 bytes: only the line's n count)
            let idx = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
            let keep = _mm_cmplt_epi8(idx, _mm_set1_epi8(n as i8));
            nx = _mm_and_si128(nx, keep);
            ny = _mm_and_si128(ny, keep);
            nz = _mm_and_si128(nz, keep);
        }
        let lo = |v: __m128i| _mm_cvtsi128_si64(v) as u64;
        let hi = |v: __m128i| _mm_cvtsi128_si64(_mm_unpackhi_epi64(v, v)) as u64;
        let h1 = c8(c8(c8(n as u32, lo(nx)), hi(nx)), lo(nz));
        let h2 = c8(c8(c8(0x9E37_79B9, lo(ny)), hi(ny)), hi(nz));
        ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
}

/// front.rs shape_hash with the CRC32-C instruction, as on ARM (the same value): two CRC32-C
/// lanes over 8-byte words (the ends read as overlapping words), mixed.
/// SAFETY: the processor must have SSE4.2.
#[target_feature(enable = "sse4.2")]
pub unsafe fn shape_hash_crc(b: &[u8]) -> u64 {
    unsafe {
        let n = b.len();
        let rd = |p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
        let rd4 = |p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
        let (mut h1, mut h2) = (n as u32, 0x9E37_79B9u32);
        if n >= 16 {
            let mut p = 0;
            while p + 16 < n {
                h1 = c8(h1, rd(p));
                h2 = c8(h2, rd(p + 8));
                p += 16;
            }
            h1 = c8(h1, rd(n - 16));
            h2 = c8(h2, rd(n - 8));
        } else if n >= 8 {
            h1 = c8(h1, rd(0));
            h2 = c8(h2, rd(n - 8));
        } else if n >= 4 {
            h1 = c4(h1, rd4(0));
            h2 = c4(h2, rd4(n - 4));
        } else if n > 0 {
            h1 = c4(h1, b[0] as u32 | (b[n / 2] as u32) << 8 | (b[n - 1] as u32) << 16);
        }
        let h = ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^ h >> 29
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test bytes: every byte value at every place of a 64-byte window (and the classes'
    /// edges: '0' - 1, '9' + 1, '@', '[', '`', '{', 0x20 and 0xA0 folds), then random.
    fn samples() -> Vec<[u8; 80]> {
        let mut v = Vec::new();
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for b in 0..=255u8 {
            for at in [0usize, 1, 15, 16, 31, 32, 47, 48, 63] {
                let mut x = [b'x'; 80];
                x[at] = b;
                v.push(x);
                let mut y = [0u8; 80];
                for (k, c) in y.iter_mut().enumerate() {
                    *c = (k as u8).wrapping_mul(37).wrapping_add(b);
                }
                y[at] = b;
                v.push(y);
            }
        }
        let edges = b"/09:@AFGZ[`afgz{ -\n\0\x7f\x80\xa0\xc1\xdaxyz";
        for _ in 0..20_000 {
            let mut x = [0u8; 80];
            for c in x.iter_mut() {
                let r = next();
                *c = if r & 3 == 0 { (r >> 8) as u8 } else { edges[(r >> 16) as usize % edges.len()] };
            }
            v.push(x);
        }
        v
    }

    fn mask(b: &[u8], f: impl Fn(u8) -> bool) -> u64 {
        b[..64].iter().enumerate().fold(0u64, |m, (i, &c)| m | (f(c) as u64) << i)
    }

    #[test]
    fn masks_same_as_plain_code() {
        for x in samples() {
            let b = &x[..];
            let dig = mask(b, |c| c.wrapping_sub(b'0') < 10);
            let sp = mask(b, |c| c == b' ');
            let hx = mask(b, |c| (c | 0x20).wrapping_sub(b'a') < 6);
            let ds = mask(b, |c| c == b'-');
            let cp = mask(b, |c| c.wrapping_sub(b'A') < 26);
            let lt = mask(b, |c| (c | 0x20).wrapping_sub(b'a') < 26);
            let sm = mask(b, |c| c.wrapping_sub(b'a') < 26);
            let z = mask(b, |c| c == 0);
            let nl = mask(b, |c| c == b'\n');
            // SAFETY: x has 80 >= 64 bytes
            unsafe {
                assert_eq!(split_masks(b.as_ptr()), [dig, sp, hx, ds, cp, lt, z], "{b:?}");
                assert_eq!(front_masks(b.as_ptr()), ([dig, sp, hx, ds, cp, sm], z != 0), "{b:?}");
                assert_eq!(front_masks_a(b.as_ptr()), ([dig, sp, hx, ds], z != 0));
                assert_eq!(front_masks_b(b.as_ptr()), [cp, sm]);
                assert_eq!(nl_mask64(b.as_ptr()), nl);
            }
            for i in [0usize, 5, 16, 48, 64] {
                for c in [b'x', b'\n', b' ', 0, 0xFF, b[i]] {
                    let want = (0..16).fold(0u64, |m, k| m | ((b[i + k] == c) as u64 * 15) << (4 * k));
                    // SAFETY: i + 16 <= 80
                    assert_eq!(unsafe { eq16(b.as_ptr().add(i), c) }, want);
                }
            }
        }
    }

    #[test]
    fn search_same_as_plain_code() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let h: Vec<u8> = (0..5000).map(|_| b"ab \n-x"[(next() % 6) as usize]).collect();
        for n in (0..700).chain([4095, 4096, 4097, 5000]) {
            for b in [b'a', b'q', b'\n', 0] {
                for off in [0usize, 1, 33] {
                    let x = &h[off.min(n)..n];
                    let mut y = x.to_vec();
                    // a byte b once, at places that test the loops' ends
                    let at = (next() as usize) % (y.len() + 1);
                    if at < y.len() && b == b'q' {
                        y[at] = b'q';
                    }
                    assert_eq!(has_byte(x, b), x.contains(&b), "n {n} b {b}");
                    assert_eq!(has_byte(&y, b), y.contains(&b), "n {n} b {b} at {at}");
                }
            }
        }
        for _ in 0..20_000 {
            let k = 1 + (next() % 9) as usize;
            let i = (next() % 200) as usize;
            let (f, l) = (b"ab \n-x"[(next() % 6) as usize], b"ab \n-x"[(next() % 6) as usize]);
            let want = (0..64).any(|o| h[i + o] == f && h[i + k + o] == l);
            // SAFETY: i + k + 64 <= 200 + 10 + 64 < h.len()
            assert_eq!(unsafe { pair_any64(h.as_ptr().add(i), k, f, l) }, want);
        }
    }

    #[test]
    fn line_ends_same_as_plain_code() {
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for dense in [2u64, 7, 61, 1000] {
            let h: Vec<u8> = (0..3000).map(|_| if next() % dense == 0 { b'\n' } else { b'\n' ^ (1 + (next() % 255) as u8) }).collect();
            for from in [0usize, 1, 15, 64, 100] {
                for to in [from, from + 1, from + 15, from + 16, from + 63, from + 64, from + 65, 1000, 2999, 3000] {
                    let mut got = Vec::new();
                    let mut i = from;
                    // SAFETY: to <= h.len() and from <= to
                    unsafe { each_nl(h.as_ptr(), &mut i, to, |e| got.push(e)) };
                    assert!(i <= to && to - i < 16 && i >= from);
                    let want: Vec<usize> = (from..i).filter(|&e| h[e] == b'\n').collect();
                    assert_eq!(got, want, "from {from} to {to}");
                }
            }
        }
    }

    #[test]
    fn twins_same_as_plain_code() {
        let mut s = 0x0F0F_1234_5555_9999u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let base: Vec<u8> = (0..400).map(|_| b"0123456789ab:/ -"[(next() % 16) as usize]).collect();
        for n in 0..300usize {
            for _ in 0..20 {
                let mut a = base.clone();
                let mut b = base.clone();
                // a few changes: a digit for another digit (same classes), or any byte
                for _ in 0..(next() % 3) {
                    let at = (next() as usize) % 320;
                    let c = b"0123456789ab:/ -"[(next() % 16) as usize];
                    if next() % 2 == 0 { a[at] = c } else { b[at] = c }
                }
                let want = a[..n].iter().zip(&b[..n]).all(|(&c, &d)| {
                    let (dc, dd) = (c.wrapping_sub(b'0') < 10, d.wrapping_sub(b'0') < 10);
                    dc == dd && (dc || c == d)
                });
                // SAFETY: a and b have 400 >= n + 15 bytes
                assert_eq!(unsafe { same_classes(a.as_ptr(), b.as_ptr(), n) }, want, "n {n}");
            }
        }
    }

    /// CRC32-C, one bit at a time (the instruction's register: no flips).
    fn crc32c_bits(mut c: u32, data: &[u8]) -> u32 {
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            }
        }
        c
    }

    #[test]
    fn hashes_use_crc32c() {
        if level() < SSE42 {
            return;
        }
        // the standard check value of CRC-32C (flipped at the start and the end)
        // SAFETY: the processor has SSE4.2 (level)
        let one = |c: u32, x: u64| unsafe { c8(c, x) };
        assert_eq!(!crc32c_bits(!0, b"123456789"), 0xE306_9283);
        let mut s = 7u64;
        for _ in 0..1000 {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let c = (s >> 32) as u32;
            assert_eq!(one(c, s), crc32c_bits(c, &s.to_le_bytes()));
            // SAFETY: as above
            assert_eq!(unsafe { c4(c, s as u32) }, crc32c_bits(c, &(s as u32).to_le_bytes()));
        }
        // skel_hash and shape_hash_crc as the ARM code computes them (written out with the
        // bitwise CRC32-C)
        let line: Vec<u8> = b"2026-11-15 14:30:01,123 INFO dfs.DataNode: Receiving block blk_-1608999687919862906 src: /10.250.19.102:54106 dest: /10.250.19.102:50010".iter().copied().chain([0u8; 64]).collect();
        for n in 0..140usize {
            let norm: Vec<u8> = line.iter().map(|&c| if c.wrapping_sub(b'0') < 10 { b'0' } else { c }).collect();
            let m = n.max(16);
            let mid = m / 2 - 8;
            let cut = |a: usize| -> [u8; 16] {
                let mut v: [u8; 16] = norm[a..a + 16].try_into().unwrap();
                if n < 16 {
                    for c in v[n..].iter_mut() {
                        *c = 0;
                    }
                }
                v
            };
            let (x, y, z) = (cut(0), cut(m - 16), cut(mid));
            let h1 = crc32c_bits(crc32c_bits(n as u32, &x), &z[..8]);
            let h2 = crc32c_bits(crc32c_bits(0x9E37_79B9, &y), &z[8..]);
            let want = ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            // SAFETY: line has n + 64 >= max(n, 16) bytes; the processor has SSE4.2
            assert_eq!(unsafe { skel_hash(line.as_ptr(), n) }, want, "n {n}");
            let b = &line[..n];
            let w = |p: usize| &b[p..p + 8];
            let (mut h1, mut h2) = (n as u32, 0x9E37_79B9u32);
            if n >= 16 {
                let mut p = 0;
                while p + 16 < n {
                    h1 = crc32c_bits(h1, w(p));
                    h2 = crc32c_bits(h2, w(p + 8));
                    p += 16;
                }
                h1 = crc32c_bits(h1, w(n - 16));
                h2 = crc32c_bits(h2, w(n - 8));
            } else if n >= 8 {
                h1 = crc32c_bits(h1, w(0));
                h2 = crc32c_bits(h2, w(n - 8));
            } else if n >= 4 {
                h1 = crc32c_bits(h1, &b[..4]);
                h2 = crc32c_bits(h2, &b[n - 4..n]);
            } else if n > 0 {
                h1 = crc32c_bits(h1, &[b[0], b[n / 2], b[n - 1], 0]);
            }
            let h = ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            // SAFETY: the processor has SSE4.2
            assert_eq!(unsafe { shape_hash_crc(b) }, h ^ h >> 29, "n {n}");
        }
    }

    #[test]
    fn level_found() {
        let l = level();
        assert!((SSE2..=CAP).contains(&l));
        if CAP >= AVX2 && std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("sse4.2") {
            assert_eq!(l, AVX2);
        }
        eprintln!("x86 level {l} (1 SSE2, 2 SSE4.2, 3 AVX2), cap {CAP}");
    }
}

/// Every kernel against the engine's plain code (the versions without NEON or SSE, copied
/// here) and against ARM's hash values, at every alignment 0..64 and every length 0..=300, on
/// four kinds of random bytes (any byte, class edges, many '\n', digits and separators).
#[cfg(test)]
mod every_alignment {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut s = self.0;
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            self.0 = s;
            s
        }

        fn byte(&mut self, kind: u32) -> u8 {
            const EDGES: &[u8] = b"/09:@AFGZ[`afgz{ -\n\0\x7f\x80\xa0\xc1\xda\xe0\xfa\xff\x0b\x8a\x2a";
            let r = self.next();
            match kind {
                0 => (r >> 24) as u8,
                1 => EDGES[(r >> 20) as usize % EDGES.len()],
                2 => {
                    if r % 5 == 0 {
                        b'\n'
                    } else {
                        (r >> 24) as u8
                    }
                }
                _ => b"0123456789ab:/ -"[(r >> 30) as usize % 16],
            }
        }
    }

    // ---- the plain code: split.rs masks64, front.rs masks64 and newlines64, derive.rs eq16,
    // front.rs same_classes (their versions without NEON) ----

    fn plain_split(b: &[u8]) -> [u64; 7] {
        let mut m = [0u64; 7];
        for (i, &c) in b[..64].iter().enumerate() {
            m[0] |= ((c.wrapping_sub(b'0') < 10) as u64) << i;
            m[1] |= ((c == b' ') as u64) << i;
            m[2] |= (((c | 0x20).wrapping_sub(b'a') < 6) as u64) << i;
            m[3] |= ((c == b'-') as u64) << i;
            m[4] |= ((c.wrapping_sub(b'A') < 26) as u64) << i;
            m[5] |= (((c | 0x20).wrapping_sub(b'a') < 26) as u64) << i;
            m[6] |= ((c == 0) as u64) << i;
        }
        m
    }

    fn plain_front(b: &[u8]) -> ([u64; 6], bool) {
        let mut m = [0u64; 6];
        let mut z = false;
        for (i, &c) in b[..64].iter().enumerate() {
            m[0] |= ((c.wrapping_sub(b'0') < 10) as u64) << i;
            m[1] |= ((c == b' ') as u64) << i;
            m[2] |= (((c | 0x20).wrapping_sub(b'a') < 6) as u64) << i;
            m[3] |= ((c == b'-') as u64) << i;
            m[4] |= ((c.wrapping_sub(b'A') < 26) as u64) << i;
            m[5] |= ((c.wrapping_sub(b'a') < 26) as u64) << i;
            z |= c == 0;
        }
        (m, z)
    }

    fn plain_nl(b: &[u8]) -> u64 {
        let mut m = 0u64;
        for (i, &c) in b[..64].iter().enumerate() {
            m |= ((c == b'\n') as u64) << i;
        }
        m
    }

    fn plain_eq16(h: &[u8], i: usize, b: u8) -> u64 {
        let mut m = 0u64;
        for (k, &c) in h[i..i + 16].iter().enumerate() {
            m |= ((c == b) as u64 * 15) << (4 * k);
        }
        m
    }

    fn plain_same_classes(x: &[u8], y: &[u8]) -> bool {
        x.iter().zip(y).all(|(&c, &d)| {
            let (dc, dd) = (c.wrapping_sub(b'0') < 10, d.wrapping_sub(b'0') < 10);
            dc == dd && (dc || c == d)
        })
    }

    // ---- ARM's hashes (front.rs skel_key before its top bits, shape_hash: __crc32cd and
    // __crc32cw), written out with a bitwise CRC32-C ----

    fn crc32c(mut c: u32, data: &[u8]) -> u32 {
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            }
        }
        c
    }

    fn arm_skel(lp: &[u8], n: usize) -> u64 {
        let m = n.max(16);
        let cut = |a: usize| -> [u8; 16] {
            let mut v = [0u8; 16];
            for (k, x) in v.iter_mut().enumerate() {
                let c = lp[a + k];
                *x = if n < 16 && k >= n {
                    0
                } else if c.wrapping_sub(b'0') < 10 {
                    b'0'
                } else {
                    c
                };
            }
            v
        };
        let (x, y, z) = (cut(0), cut(m - 16), cut(m / 2 - 8));
        let h1 = crc32c(crc32c(n as u32, &x), &z[..8]);
        let h2 = crc32c(crc32c(0x9E37_79B9, &y), &z[8..]);
        ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    fn arm_shape(b: &[u8]) -> u64 {
        let n = b.len();
        let w = |p: usize| &b[p..p + 8];
        let (mut h1, mut h2) = (n as u32, 0x9E37_79B9u32);
        if n >= 16 {
            let mut p = 0;
            while p + 16 < n {
                h1 = crc32c(h1, w(p));
                h2 = crc32c(h2, w(p + 8));
                p += 16;
            }
            h1 = crc32c(h1, w(n - 16));
            h2 = crc32c(h2, w(n - 8));
        } else if n >= 8 {
            h1 = crc32c(h1, w(0));
            h2 = crc32c(h2, w(n - 8));
        } else if n >= 4 {
            h1 = crc32c(h1, &b[..4]);
            h2 = crc32c(h2, &b[n - 4..n]);
        } else if n > 0 {
            h1 = crc32c(h1, &[b[0], b[n / 2], b[n - 1], 0]);
        }
        let h = ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^ h >> 29
    }

    #[test]
    fn kernels_same_as_plain_code() {
        let mut r = Rng(0x0DDB_A11C_AFE5_EED5);
        let mut cases = 0u64;
        for kind in 0..4u32 {
            for _ in 0..2 {
                // 64 alignments + 300 bytes + 80 bytes of room (the kernels' reads after n)
                let buf: Vec<u8> = (0..64 + 300 + 80).map(|_| r.byte(kind)).collect();
                for a in 0..64usize {
                    let w = &buf[a..a + 64];
                    // SAFETY: w has 64 bytes
                    unsafe {
                        assert_eq!(split_masks(w.as_ptr()), plain_split(w), "kind {kind} a {a}");
                        let (fm, fz) = plain_front(w);
                        assert_eq!(front_masks(w.as_ptr()), (fm, fz));
                        assert_eq!(front_masks_a(w.as_ptr()), ([fm[0], fm[1], fm[2], fm[3]], fz));
                        assert_eq!(front_masks_b(w.as_ptr()), [fm[4], fm[5]]);
                        assert_eq!(nl_mask64(w.as_ptr()), plain_nl(w));
                    }
                    for i in 0..48 {
                        for c in [w[i], w[i + 7], b'\n', 0, 0xFF, b' '] {
                            // SAFETY: i + 16 <= 64
                            assert_eq!(unsafe { eq16(w.as_ptr().add(i), c) }, plain_eq16(w, i, c));
                        }
                    }
                    for k in 1..=16usize {
                        let h = &buf[a..];
                        let (f, l) = (h[(r.next() % 64) as usize], h[k + (r.next() % 64) as usize]);
                        let want = (0..64).any(|o| h[o] == f && h[k + o] == l);
                        // SAFETY: k + 64 <= 80 <= h.len()
                        assert_eq!(unsafe { pair_any64(h.as_ptr(), k, f, l) }, want, "a {a} k {k}");
                    }
                    for n in 0..=300usize {
                        let h = &buf[a..a + n];
                        for from in [0usize, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 200] {
                            if from > n {
                                break;
                            }
                            let mut got = Vec::new();
                            let mut i = from;
                            // SAFETY: h[..n] is readable and from <= n
                            unsafe { each_nl(h.as_ptr(), &mut i, n, |e| got.push(e)) };
                            assert!(i >= from && i <= n && n - i < 16, "a {a} n {n} from {from} i {i}");
                            let want: Vec<usize> = (from..i).filter(|&e| h[e] == b'\n').collect();
                            assert_eq!(got, want, "kind {kind} a {a} n {n} from {from}");
                        }
                        for b in [b'\n', 0, 0xFF, b' ', b'q', buf[a + n / 2], buf[a + n]] {
                            assert_eq!(has_byte(h, b), h.contains(&b), "a {a} n {n} b {b}");
                        }
                        if level() >= SSE42 {
                            // SAFETY: the processor has SSE4.2; buf has 80 bytes after a + n
                            // (skel_hash reads max(n, 16))
                            unsafe {
                                assert_eq!(shape_hash_crc(h), arm_shape(h), "a {a} n {n}");
                                assert_eq!(skel_hash(h.as_ptr(), n), arm_skel(&buf[a..], n), "a {a} n {n}");
                            }
                        }
                        cases += 1;
                    }
                }
            }
        }
        eprintln!("{cases} (alignment, length) cases, x86 level {}", level());
    }

    /// One '\n' (or byte b) at every place of every length at every alignment, with the same
    /// byte just before and just after the bytes looked at (never reported).
    #[test]
    fn one_byte_at_every_place() {
        let mut buf = vec![b'x'; 64 + 300 + 64];
        for a in 1..65usize {
            for n in 0..=300usize {
                buf[a - 1] = b'\n';
                buf[a + n] = b'\n';
                assert!(!has_byte(&buf[a..a + n], b'\n'), "a {a} n {n}");
                let mut i = 0;
                let mut got = Vec::new();
                // SAFETY: buf[a..a + n] is readable
                unsafe { each_nl(buf[a..].as_ptr(), &mut i, n, |e| got.push(e)) };
                assert!(got.is_empty() && i <= n && n - i < 16);
                for p in 0..n {
                    buf[a + p] = b'\n';
                    assert!(has_byte(&buf[a..a + n], b'\n'), "a {a} n {n} p {p}");
                    let mut i = 0;
                    let mut got = Vec::new();
                    // SAFETY: as above
                    unsafe { each_nl(buf[a..].as_ptr(), &mut i, n, |e| got.push(e)) };
                    assert_eq!(got, if p < i { vec![p] } else { vec![] }, "a {a} n {n} p {p}");
                    buf[a + p] = b'x';
                }
                buf[a - 1] = b'x';
                buf[a + n] = b'x';
            }
        }
    }

    /// same_classes at every alignment of both lines and every length, against the plain
    /// code: digits changed for digits, one byte changed (top bit only, one low bit, any), and
    /// changes after n (never looked at).
    #[test]
    fn same_classes_same_as_plain_code() {
        let mut r = Rng(0x5A5A_1234_8765_4321);
        for kind in [1u32, 3, 0] {
            let x: Vec<u8> = (0..64 + 300 + 80).map(|_| r.byte(kind)).collect();
            for a in 0..64usize {
                let b = (a * 7 + 3) % 64;
                for n in 0..=300usize {
                    for t in 0..7 {
                        let mut y = vec![b'x'; 64 + 300 + 80];
                        y[b..b + n + 16].copy_from_slice(&x[a..a + n + 16]);
                        let p = b + (r.next() as usize) % (n + 16);
                        match t {
                            0 => {}
                            1 => {
                                for c in y[b..b + n].iter_mut() {
                                    if c.wrapping_sub(b'0') < 10 {
                                        *c = b'0' + (r.next() % 10) as u8;
                                    }
                                }
                            }
                            2 => y[p] ^= 0x80,
                            3 => y[p] ^= 1 << (r.next() % 7),
                            4 => y[p] = r.byte(1),
                            5 => y[b + n + (r.next() as usize) % 16] ^= 0x41,
                            _ => {
                                let q = b + (r.next() as usize) % (n + 1);
                                y[q] = b'0' + (r.next() % 10) as u8;
                            }
                        }
                        let want = plain_same_classes(&x[a..a + n], &y[b..b + n]);
                        // SAFETY: both have 16+ bytes after n
                        let got = unsafe { same_classes(x[a..].as_ptr(), y[b..].as_ptr(), n) };
                        assert_eq!(got, want, "kind {kind} a {a} b {b} n {n} t {t}");
                        // and the other way round
                        let got = unsafe { same_classes(y[b..].as_ptr(), x[a..].as_ptr(), n) };
                        assert_eq!(got, want, "kind {kind} a {a} b {b} n {n} t {t} (swapped)");
                    }
                }
            }
        }
    }
}
