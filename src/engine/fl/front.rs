// Packing front end (packfront, 2026-11-15): a faster line splitter with exactly the shapes
// and values of split_line (main.rs, the reference; FL_CHECK=1 compares every line).
// - the line's masks (digits, spaces, hex letters, dashes, capitals, small letters) are made
//   64 bytes at a time with NEON (plain code elsewhere),
// - values are found by walking "events": digit-run starts and month / weekday name
//   candidates, in order, with no search per value; runs of 1-8 digits are read 8 bytes at
//   once; clock times and IPv4 addresses are tried only where they can start,
// - values are written straight into pass 1's list (no copy per line),
// - lines with a long hex run or a UUID take the per-word way (as in split.rs).
// Also: stream checksums (FL_SUMS=1) and a front-end bench (FL_FRONTBENCH=reps).
use super::*;

/// Bytes that may be read after a line's end.
pub const ROOM: usize = 64;

/// Masks of the 64 bytes at p (bit i = byte i): digits, spaces, hex letters (a-f, A-F),
/// dashes, capitals, small letters; and whether a 0 byte is among them.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> ([u64; 6], bool) {
    use std::arch::aarch64::*;
    unsafe {
        let v = [vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(32)), vld1q_u8(p.add(48))];
        let bm = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        #[inline(always)]
        unsafe fn mm(a: uint8x16_t, b: uint8x16_t, c: uint8x16_t, d: uint8x16_t, bm: uint8x16_t) -> u64 {
            unsafe {
                let s = vpaddq_u8(vpaddq_u8(vandq_u8(a, bm), vandq_u8(b, bm)), vpaddq_u8(vandq_u8(c, bm), vandq_u8(d, bm)));
                vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
            }
        }
        let (c0, c10, cs, c20, ca, c6, c26, cdash, c_cap) = (vdupq_n_u8(b'0'), vdupq_n_u8(10), vdupq_n_u8(b' '), vdupq_n_u8(0x20), vdupq_n_u8(b'a'), vdupq_n_u8(6), vdupq_n_u8(26), vdupq_n_u8(b'-'), vdupq_n_u8(b'A'));
        let dig = |x: uint8x16_t| vcltq_u8(vsubq_u8(x, c0), c10);
        let low = |x: uint8x16_t| vsubq_u8(vorrq_u8(x, c20), ca); // folded letter - 'a'
        let cap = |x: uint8x16_t| vcltq_u8(vsubq_u8(x, c_cap), c26);
        let (l0, l1, l2, l3) = (low(v[0]), low(v[1]), low(v[2]), low(v[3]));
        let digits = mm(dig(v[0]), dig(v[1]), dig(v[2]), dig(v[3]), bm);
        let spaces = mm(vceqq_u8(v[0], cs), vceqq_u8(v[1], cs), vceqq_u8(v[2], cs), vceqq_u8(v[3], cs), bm);
        let hexl = mm(vcltq_u8(l0, c6), vcltq_u8(l1, c6), vcltq_u8(l2, c6), vcltq_u8(l3, c6), bm);
        let dashes = mm(vceqq_u8(v[0], cdash), vceqq_u8(v[1], cdash), vceqq_u8(v[2], cdash), vceqq_u8(v[3], cdash), bm);
        let (k0, k1, k2, k3) = (cap(v[0]), cap(v[1]), cap(v[2]), cap(v[3]));
        let caps = mm(k0, k1, k2, k3, bm);
        // small letters: folded letters that are not capitals
        let sm = |l: uint8x16_t, k: uint8x16_t| vbicq_u8(vcltq_u8(l, c26), k);
        let small = mm(sm(l0, k0), sm(l1, k1), sm(l2, k2), sm(l3, k3), bm);
        let min = vminvq_u8(vminq_u8(vminq_u8(v[0], v[1]), vminq_u8(v[2], v[3])));
        ([digits, spaces, hexl, dashes, caps, small], min == 0)
    }
}

/// masks64's first four masks (digits, spaces, hex letters, dashes) and its 0-byte flag.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn masks64a(p: *const u8) -> ([u64; 4], bool) {
    use std::arch::aarch64::*;
    unsafe {
        let v = [vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(32)), vld1q_u8(p.add(48))];
        let bm = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        #[inline(always)]
        unsafe fn mm(a: uint8x16_t, b: uint8x16_t, c: uint8x16_t, d: uint8x16_t, bm: uint8x16_t) -> u64 {
            unsafe {
                let s = vpaddq_u8(vpaddq_u8(vandq_u8(a, bm), vandq_u8(b, bm)), vpaddq_u8(vandq_u8(c, bm), vandq_u8(d, bm)));
                vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
            }
        }
        let (c0, c10, cs, c20, ca, c6, cdash) = (vdupq_n_u8(b'0'), vdupq_n_u8(10), vdupq_n_u8(b' '), vdupq_n_u8(0x20), vdupq_n_u8(b'a'), vdupq_n_u8(6), vdupq_n_u8(b'-'));
        let dig = |x: uint8x16_t| vcltq_u8(vsubq_u8(x, c0), c10);
        let low = |x: uint8x16_t| vsubq_u8(vorrq_u8(x, c20), ca); // folded letter - 'a'
        let (l0, l1, l2, l3) = (low(v[0]), low(v[1]), low(v[2]), low(v[3]));
        let digits = mm(dig(v[0]), dig(v[1]), dig(v[2]), dig(v[3]), bm);
        let spaces = mm(vceqq_u8(v[0], cs), vceqq_u8(v[1], cs), vceqq_u8(v[2], cs), vceqq_u8(v[3], cs), bm);
        let hexl = mm(vcltq_u8(l0, c6), vcltq_u8(l1, c6), vcltq_u8(l2, c6), vcltq_u8(l3, c6), bm);
        let dashes = mm(vceqq_u8(v[0], cdash), vceqq_u8(v[1], cdash), vceqq_u8(v[2], cdash), vceqq_u8(v[3], cdash), bm);
        let min = vminvq_u8(vminq_u8(vminq_u8(v[0], v[1]), vminq_u8(v[2], v[3])));
        ([digits, spaces, hexl, dashes], min == 0)
    }
}

/// masks64's last two masks (capitals, small letters).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn masks64b(p: *const u8) -> [u64; 2] {
    use std::arch::aarch64::*;
    unsafe {
        let v = [vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(32)), vld1q_u8(p.add(48))];
        let bm = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        #[inline(always)]
        unsafe fn mm(a: uint8x16_t, b: uint8x16_t, c: uint8x16_t, d: uint8x16_t, bm: uint8x16_t) -> u64 {
            unsafe {
                let s = vpaddq_u8(vpaddq_u8(vandq_u8(a, bm), vandq_u8(b, bm)), vpaddq_u8(vandq_u8(c, bm), vandq_u8(d, bm)));
                vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
            }
        }
        let (c20, ca, c26, c_cap) = (vdupq_n_u8(0x20), vdupq_n_u8(b'a'), vdupq_n_u8(26), vdupq_n_u8(b'A'));
        let low = |x: uint8x16_t| vsubq_u8(vorrq_u8(x, c20), ca); // folded letter - 'a'
        let cap = |x: uint8x16_t| vcltq_u8(vsubq_u8(x, c_cap), c26);
        let (l0, l1, l2, l3) = (low(v[0]), low(v[1]), low(v[2]), low(v[3]));
        let (k0, k1, k2, k3) = (cap(v[0]), cap(v[1]), cap(v[2]), cap(v[3]));
        let caps = mm(k0, k1, k2, k3, bm);
        let sm = |l: uint8x16_t, k: uint8x16_t| vbicq_u8(vcltq_u8(l, c26), k);
        let small = mm(sm(l0, k0), sm(l1, k1), sm(l2, k2), sm(l3, k3), bm);
        [caps, small]
    }
}

/// masks64, masks64a and masks64b with SSE2 (x86_64, x86.rs).
/// SAFETY (all three): p[..64] must be readable.
#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> ([u64; 6], bool) {
    unsafe { super::x86::front_masks(p) }
}

#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[inline(always)]
unsafe fn masks64a(p: *const u8) -> ([u64; 4], bool) {
    unsafe { super::x86::front_masks_a(p) }
}

#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[inline(always)]
unsafe fn masks64b(p: *const u8) -> [u64; 2] {
    unsafe { super::x86::front_masks_b(p) }
}

#[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
#[inline(always)]
unsafe fn masks64a(p: *const u8) -> ([u64; 4], bool) {
    let ([d, s, h, ds, _, _], z) = unsafe { masks64(p) };
    ([d, s, h, ds], z)
}

#[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
#[inline(always)]
unsafe fn masks64b(p: *const u8) -> [u64; 2] {
    let ([_, _, _, _, c, l], _) = unsafe { masks64(p) };
    [c, l]
}

/// The same without NEON or SSE2.
#[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> ([u64; 6], bool) {
    let b = unsafe { std::slice::from_raw_parts(p, 64) };
    let mut m = [0u64; 6];
    let mut z = false;
    for (i, &c) in b.iter().enumerate() {
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

/// The 0-byte mask of 64 bytes (only asked for when there may be one).
#[inline(never)]
fn zeros64(b: &[u8]) -> u64 {
    let mut m = 0u64;
    for (i, &c) in b[..64].iter().enumerate() {
        m |= ((c == 0) as u64) << i;
    }
    m
}

/// Bits i..i+len of a mask (len < 64).
#[inline(always)]
fn bits_at(m: &[u64], i: usize, len: usize) -> u64 {
    let (w, s) = (i / 64, i % 64);
    let mut x = m[w] >> s;
    if s != 0 && w + 1 < m.len() {
        x |= m[w + 1] << (64 - s);
    }
    x & ((1u64 << len) - 1)
}

/// 32+ bits of m from bit i on (m has a 0 word after the line's words).
#[inline(always)]
fn bits_at32(m: &[u64], i: usize) -> u64 {
    let (w, s) = (i / 64, i % 64);
    let lo = m[w] >> s;
    if s > 32 { lo | m[w + 1] << (64 - s) } else { lo }
}

/// The last space before p, if any.
#[inline]
fn space_before(sp: &[u64], p: usize) -> Option<usize> {
    let mut w = p / 64;
    let mut m = sp[w] & ((1u64 << (p % 64)) - 1);
    loop {
        if m != 0 {
            return Some(w * 64 + 63 - m.leading_zeros() as usize);
        }
        if w == 0 {
            return None;
        }
        w -= 1;
        m = sp[w];
    }
}

/// The first space after p, or n (the line end).
#[inline(always)]
fn space_after(sp: &[u64], p: usize, n: usize) -> usize {
    let mut w = p / 64;
    let mut m = (!0u64 << (p % 64)) << 1 & sp[w];
    loop {
        if m != 0 {
            return (w * 64 + m.trailing_zeros() as usize).min(n);
        }
        w += 1;
        if w >= sp.len() {
            return n;
        }
        m = sp[w];
    }
}

/// The first set bit at or after p in m (None at the end).
#[inline(always)]
fn next_one(m: &[u64], p: usize) -> Option<usize> {
    let mut w = p / 64;
    if w >= m.len() {
        return None;
    }
    let mut x = m[w] & (!0u64 << (p % 64));
    loop {
        if x != 0 {
            return Some(w * 64 + x.trailing_zeros() as usize);
        }
        w += 1;
        if w >= m.len() {
            return None;
        }
        x = m[w];
    }
}

/// The first clear bit at or after p in m (m ends with a 0 word).
#[inline(always)]
fn next_zero(m: &[u64], p: usize) -> usize {
    let mut w = p / 64;
    let mut x = !m[w] & (!0u64 << (p % 64));
    while x == 0 {
        w += 1;
        x = !m[w];
    }
    w * 64 + x.trailing_zeros() as usize
}

/// Copy src[s..s + n] to dst[d..], 16 bytes at a time (up to 15 bytes more are read and
/// written: both buffers have room after their ends).
#[inline(always)]
unsafe fn copy16(dst: *mut u8, src: *const u8, n: usize) {
    let mut p = 0;
    while p < n {
        unsafe { (dst.add(p) as *mut [u8; 16]).write_unaligned((src.add(p) as *const [u8; 16]).read_unaligned()) };
        p += 16;
    }
}

/// copy16 that always copies the first 16 bytes (n may be 0).
#[inline(always)]
unsafe fn copy16f(dst: *mut u8, src: *const u8, n: usize) {
    unsafe {
        (dst as *mut [u8; 16]).write_unaligned((src as *const [u8; 16]).read_unaligned());
        let mut p = 16;
        while p < n {
            (dst.add(p) as *mut [u8; 16]).write_unaligned((src.add(p) as *const [u8; 16]).read_unaligned());
            p += 16;
        }
    }
}

/// The number written in the 1-8 digits l[a..a + w]; reads l[a..a + 8].
#[inline(always)]
fn digits8(l: &[u8], a: usize, w: usize) -> u64 {
    debug_assert!(a + 8 <= l.len() && (1..=8).contains(&w));
    let x = unsafe { (l.as_ptr().add(a) as *const u64).read_unaligned() };
    let x = u64::from_le(x).wrapping_sub(0x3030_3030_3030_3030) << (8 * (8 - w));
    let x = x.wrapping_mul(10).wrapping_add(x >> 8);
    let m = 0x0000_00FF_0000_00FF;
    ((x & m).wrapping_mul(100 + (1_000_000 << 32)).wrapping_add((x >> 16 & m).wrapping_mul(1 + (10_000 << 32)))) >> 32
}

/// The number written in the 1-24 digits l[a..a + w] (wrapping above 2^64 like split_line).
#[inline(always)]
fn parse_digits(l: &[u8], a: usize, w: usize) -> u64 {
    if w <= 8 {
        digits8(l, a, w)
    } else if w <= 16 {
        digits8(l, a, w - 8) * 100_000_000 + digits8(l, a + w - 8, 8)
    } else if w <= 24 {
        (digits8(l, a, w - 16).wrapping_mul(100_000_000).wrapping_add(digits8(l, a + w - 16, 8))).wrapping_mul(100_000_000).wrapping_add(digits8(l, a + w - 8, 8))
    } else {
        l[a..a + w].iter().fold(0u64, |v, &c| v.wrapping_mul(10).wrapping_add((c - b'0') as u64))
    }
}

/// A digit run l[a..e] read as a plain number, a long ID or text (it is not an IPv4 address or
/// a clock time); `sl` is the shape's end (a '-' just before a long ID is taken into it).
#[inline(always)]
fn digit_value(l: &[u8], a: usize, e: usize, sl: &mut usize, cfg: &SplitCfg) -> Val {
    let w = e - a;
    if w <= 8 && w <= cfg.maxw {
        // most numbers: 1-8 digits
        let v = digits8(l, a, w);
        let kind = ((w > 1) & (l[a] == b'0')) as u8 * (3 + w as u8);
        if kind == 0 {
            let p = spn_pad(l, a, w, cfg);
            if p > 0 {
                *sl -= p;
                return (SPN + (p + w) as u8, v, (a - p) as u32, e as u32);
            }
        }
        return (kind, v, a as u32, e as u32);
    }
    long_digit_value(l, a, e, sl, cfg)
}

/// Spaces before a plain number that start its value (space-padded numbers, SPN; see
/// space_pad): 0 when none or when the setting is off.
#[inline(always)]
fn spn_pad(l: &[u8], a: usize, w: usize, cfg: &SplitCfg) -> usize {
    if cfg.spaced && a >= 3 && l[a - 1] == b' ' && l[a - 2] == b' ' { space_pad(l, a, w) } else { 0 }
}

#[inline(never)]
fn long_digit_value(l: &[u8], a: usize, e: usize, sl: &mut usize, cfg: &SplitCfg) -> Val {
    let w = e - a;
    let v = if w <= cfg.maxw.max(19) { parse_digits(l, a, w) } else { 0 };
    if cfg.use_ids && (15..=19).contains(&w) && l[a] != b'0' && v < 1 << 63 {
        // a minus sign right before it (not after a letter or digit) is part of it
        let neg = a > 0 && l[a - 1] == b'-' && (a == 1 || !l[a - 2].is_ascii_alphanumeric());
        if neg {
            *sl -= 1;
        }
        return (LONG_ID, v << 1 | neg as u64, a as u32, e as u32);
    }
    if w > cfg.maxw {
        (1, 0, a as u32, e as u32)
    } else if w > 1 && l[a] == b'0' {
        (3 + w as u8, v, a as u32, e as u32)
    } else {
        // spaces before the word beyond its separating one: part of the number (they are the
        // shape's last bytes)
        let p = spn_pad(l, a, w, cfg);
        *sl -= p;
        (if p > 0 { SPN + (p + w) as u8 } else { 0 }, v, (a - p) as u32, e as u32)
    }
}

/// The 2-digit number at l[p..p + 2].
#[inline(always)]
fn two_digits(l: &[u8], p: usize) -> u64 {
    (l[p] - b'0') as u64 * 10 + (l[p + 1] - b'0') as u64
}

/// An IPv4 address, a dotted or a colon clock time starting at the digit run l[a..a + wd] (a
/// run start; next: the byte after the run, '.' with wd <= 3 or ':' with wd <= 2) in a line of
/// n bytes: (kind, value, end). `dm`: the digit mask from a on (32+ bits).
/// The word end j of split_line's rules is not needed: these forms hold no spaces, so a test
/// "p < j and l[p] is a digit or sign" is the same as "p < n and ...".
#[inline(always)]
fn smart_at(l: &[u8], a: usize, wd: usize, next: u8, n: usize, dm: u64, cfg: &SplitCfg) -> Option<(u8, u64, usize)> {
    if !cfg.use_smart {
        return None;
    }
    // all four start tests refuse a digit or the same sign before it
    if a > 0 && (l[a - 1] == next || l[a - 1].wrapping_sub(b'0') < 10) {
        return None;
    }
    if next == b':' {
        // HH:MM:SS or H:MM:SS: digits at hw+1, hw+2, hw+4, hw+5; none at hw+3 and hw+6
        let hw = wd;
        let want = ((1u64 << hw) - 1) | 3 << (hw + 1) | 3 << (hw + 4);
        if dm & ((1 << (hw + 7)) - 1) == want && l[a + hw + 3] == b':' {
            let h = if hw == 2 { two_digits(l, a) } else { (l[a] - b'0') as u64 };
            let (m, sec) = (two_digits(l, a + hw + 1), two_digits(l, a + hw + 4));
            if m >= 60 || sec >= 60 {
                return loose(l, a, n, cfg);
            }
            let mut p = a + hw + 6;
            if p < n && l[p] == b':' {
                return loose(l, a, n, cfg);
            }
            let mut v = (h * 60 + m) * 60 + sec;
            let mut kind = 40 + (hw as u8 - 1);
            if p + 1 < n && (l[p] == b'.' || l[p] == b',') && dm >> (p + 1 - a) & 1 == 1 {
                // 1-6 fraction digits (more: no fraction)
                let fd = (!(dm >> (p + 1 - a))).trailing_zeros() as usize;
                if fd <= 6 {
                    let f = digits8(l, p + 1, fd);
                    v = v * POW10[fd] + f;
                    kind += 2 * (l[p] == b',') as u8 + 4 * fd as u8;
                    p += 1 + fd;
                }
            }
            return Some((kind, v, p));
        }
        // not H:MM:SS: time_at says no
        return loose(l, a, n, cfg);
    }
    // IPv4: four parts of 1-3 digits (no leading 0, up to 255) with dots between
    let mut q = a;
    let mut ip = 0u64;
    let mut ok = true;
    for part in 0..4 {
        let w = (!(dm >> (q - a))).trailing_zeros() as usize;
        if w == 0 || w > 3 || (w > 1 && l[q] == b'0') {
            ok = false;
            break;
        }
        let x = digits8(l, q, w);
        if x > 255 {
            ok = false;
            break;
        }
        ip = ip << 8 | x;
        q += w;
        if part < 3 {
            if q >= n || l[q] != b'.' || q + 1 - a >= 32 {
                ok = false;
                break;
            }
            q += 1;
        }
    }
    if ok {
        // 1.2.3.4.5: a version, not an address
        if !(q + 1 < n && l[q] == b'.' && l[q + 1].wrapping_sub(b'0') < 10) {
            return Some((30, ip, q));
        }
    }
    if cfg.dotted && wd <= 2 {
        // H.MM.SS or HH.MM.SS (dotted_time_at): digits at hw+1, hw+2, hw+4, hw+5; none at hw+3,
        // hw+6 (3 digits first: never one)
        let hw = wd;
        let want = ((1u64 << hw) - 1) | 3 << (hw + 1) | 3 << (hw + 4);
        if dm & ((1 << (hw + 7)) - 1) == want && l[a + hw + 3] == b'.' {
            let h = if hw == 2 { two_digits(l, a) } else { (l[a] - b'0') as u64 };
            let (m, sec) = (two_digits(l, a + hw + 1), two_digits(l, a + hw + 4));
            if m >= 60 || sec >= 60 {
                return None;
            }
            let mut p = a + hw + 6;
            let mut v = (h * 60 + m) * 60 + sec;
            let mut kind = 110 + (hw as u8 - 1);
            if p + 1 < n && l[p] == b'.' && dm >> (p + 1 - a) & 1 == 1 {
                let fd = (!(dm >> (p + 1 - a))).trailing_zeros() as usize;
                if fd <= 6 {
                    v = v * POW10[fd] + digits8(l, p + 1, fd);
                    kind += 2 * fd as u8;
                    p += 1 + fd;
                }
            }
            if p < n && (l[p] == b'.' || l[p].wrapping_sub(b'0') < 10) {
                return None; // more dotted parts: not a time
            }
            return Some((kind, v, p));
        }
    }
    None
}

#[inline(never)]
fn loose(l: &[u8], a: usize, n: usize, cfg: &SplitCfg) -> Option<(u8, u64, usize)> {
    if cfg.loose {
        if let Some((e, v)) = loose_time_at(l, a, n) {
            return Some((68, v, e));
        }
    }
    None
}

/// 1, 10, 100, ... 10^19
static POW10: [u64; 20] = {
    let mut t = [1u64; 20];
    let mut i = 1;
    while i < 20 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// First 3 letters of the short month / weekday names as keys, at their NAME_SLOT places.
static NAME_KEYS: [u32; 32] = {
    let mut t = [0u32; 32];
    let mut k = 0;
    while k < 19 {
        let n = NAMES[k];
        let key = n[0] as u32 | (n[1] as u32) << 8 | (n[2] as u32) << 16;
        t[(key.wrapping_mul(NAME_MUL) >> 27) as usize] = key;
        k += 1;
    }
    t
};

/// A name candidate (a capital) at c whose 3 letters are a short name: when its word has no
/// digit and name_at finds a month or weekday name there, the text before it and a NAMEK value
/// go to the shape; returns the new ends and the end of the name.
#[inline(never)]
unsafe fn name_word(l: &[u8], n: usize, c: usize, sp: &[u64], dg: &[u64], sh: *mut u8, mut sl: usize, vp: *mut Val, nv: usize, done: usize) -> Option<(usize, usize, usize)> {
    let i = if c == 0 || l[c - 1] == b' ' { c } else { c - 1 };
    let j = space_after(sp, c, n);
    if i < done || next_one(dg, i).is_some_and(|x| x < j) {
        return None; // a word with a digit: no names there
    }
    let (p, q, x) = name_at(l, i, j)?;
    unsafe {
        copy16(sh.add(sl), l.as_ptr().add(done), p - done);
        sl += p - done;
        *sh.add(sl) = MARK;
        vp.add(nv).write((NAMEK, x, p as u32, q as u32));
    }
    Some((sl + 1, nv + 1, q))
}

/// Whether the 3 bytes at c (c + 3 <= n) are a short name's first letters.
#[inline(always)]
fn name_key(l: &[u8], c: usize, n: usize) -> bool {
    if c + 3 > n {
        return false;
    }
    let key = u32::from_le_bytes([l[c], l[c + 1], l[c + 2], 0]);
    NAME_KEYS[(key.wrapping_mul(NAME_MUL) >> 27) as usize] == key
}

/// Whether name_key holds at some place of the line l (kernels: names_in_sample's filter).
/// Front::split gives a NAMEK value only through name_word, which numbers_line and words_line
/// call only where name_key(l, c, n) holds; so a line where it holds nowhere has no NAMEK value.
/// The 19 keys start with a capital (NAMES), so only capitals are tried: 16 bytes per step.
pub(crate) fn may_have_name(l: &[u8]) -> bool {
    let n = l.len();
    let mut c = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        // SAFETY: 16 bytes at c with c + 16 <= n
        unsafe {
            let (ka, k26) = (vdupq_n_u8(b'A'), vdupq_n_u8(26));
            while c + 16 <= n {
                let m = vcltq_u8(vsubq_u8(vld1q_u8(l.as_ptr().add(c)), ka), k26);
                // a nibble per byte (0xF: a capital), one bit of it kept
                let mut b = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m)))) & 0x8888_8888_8888_8888;
                while b != 0 {
                    if name_key(l, c + b.trailing_zeros() as usize / 4, n) {
                        return true;
                    }
                    b &= b - 1;
                }
                c += 16;
            }
        }
    }
    while c < n {
        if l[c].wrapping_sub(b'A') < 26 && name_key(l, c, n) {
            return true;
        }
        c += 1;
    }
    false
}
const _: () = {
    // (may_have_name tries capitals only: every short name starts with one)
    let mut k = 0;
    while k < 19 {
        assert!(NAMES[k][0].wrapping_sub(b'A') < 26);
        k += 1;
    }
};

/// The bits of runs of 8+ set bits in x.
#[inline(always)]
fn long_runs(x: u128) -> u128 {
    let r2 = x & x >> 1;
    let r4 = r2 & r2 >> 2;
    let r8 = r4 & r4 >> 4; // bit k: x has bits k..k+8
    let c2 = r8 | r8 << 1;
    let c4 = c2 | c2 << 2;
    c4 | c4 << 4
}

/// Positions of hex characters (x) with dashes 8, 13, 18 and 23 bytes later: where a UUID
/// could start.
#[inline(always)]
fn uuid_starts(x: u128, dash: u128) -> u128 {
    x & dash >> 8 & dash >> 13 & dash >> 18 & dash >> 23
}

/// Whether a word of len < 64 bytes (d, h, dash: its digit, hex-letter and dash masks) can hold
/// neither a hex value nor a UUID, so that its values are just its digit runs.
#[inline(always)]
fn only_numbers(len: usize, d: u64, h: u64, dash: u64) -> bool {
    let x = d | h;
    if long_runs(x as u128) & h as u128 != 0 {
        return false;
    }
    len < 36 || uuid_starts(x as u128, dash as u128) & ((1u64 << (len - 35)) - 1) as u128 == 0
}

/// Bit k: x has bits k..k+4 set, spread over the run (split.rs runs5).
#[inline(always)]
fn runs5(x: u128) -> u128 {
    let r2 = x & x >> 1;
    let r5 = r2 & r2 >> 2 & x >> 4;
    let c2 = r5 | r5 << 1;
    c2 | c2 << 2 | r5 << 4
}

/// Whether a run of 5+ hex characters (x) not next to another letter (nh) may hold both a hex
/// letter (h) and a digit (d), so be a short hex number (HEXS; split.rs hex5_maybe).
#[inline(always)]
fn hex5_maybe(x: u128, h: u128, d: u128, nh: u128) -> bool {
    let mixed = h << 1 & d | d << 1 & h;
    if mixed == 0 {
        return false;
    }
    let c = runs5(x);
    let st = x & !(x << 1) & !(nh << 1);
    let en = x & !(x >> 1) & !(nh >> 1);
    st & c != 0 && en & c != 0 && c & mixed != 0
}

/// Whether some word of the line may hold a short hex number (the line then goes word by
/// word, where split_word_fast finds them); nh: letters that are not hex digits.
#[inline(never)]
fn hex5_line(dg: &[u64], hx: &[u64], caps: &[u64], lower: &[u64], nw: usize) -> bool {
    for w in 0..nw {
        let two = |m: &[u64]| m[w] as u128 | (m[w + 1] as u128) << 64;
        let (d, h) = (two(dg), two(hx));
        let nh = (two(caps) | two(lower)) & !h;
        if hex5_maybe(d | h, h, d, nh) {
            return true;
        }
    }
    false
}

/// Bits s.. of the 128-bit pair (hi, lo), low 64 of them (a funnel shift, one instruction).
#[inline(always)]
fn fsh(lo: u64, hi: u64, s: u32) -> u64 {
    lo >> s | hi << (64 - s)
}

/// Whether no word of the line can hold a hex value or a UUID (masks of nw words + a 0 word;
/// a few lines that could are sent the per-word way): no run of 8+ hex characters holds a hex
/// letter, and no hex character has dashes 8, 13, 18 and 23 bytes after it.
#[inline(always)]
fn only_numbers_line(dg: &[u64], hx: &[u64], ds: &[u64], nw: usize) -> bool {
    // backwards, so the next word's partial results are known: r8 bit k = hex characters at
    // k..k+8, h8 bit k = a hex letter in k..k+8
    let (mut xn, mut r2n, mut r4n, mut hn, mut h2n, mut h4n) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut dashes = 0;
    let mut w = nw;
    while w > 0 {
        w -= 1;
        let (h, x) = (hx[w], dg[w] | hx[w]);
        let r2 = x & fsh(x, xn, 1);
        let r4 = r2 & fsh(r2, r2n, 2);
        let r8 = r4 & fsh(r4, r4n, 4);
        let h2 = h | fsh(h, hn, 1);
        let h4 = h2 | fsh(h2, h2n, 2);
        let h8 = h4 | fsh(h4, h4n, 4);
        if r8 & h8 != 0 {
            return false;
        }
        (xn, r2n, r4n, hn, h2n, h4n) = (x, r2, r4, h, h2, h4);
        dashes += ds[w].count_ones();
    }
    if dashes >= 4 {
        for w in 0..nw {
            let (d0, d1) = (ds[w], ds[w + 1]);
            let x = dg[w] | hx[w];
            if x & fsh(d0, d1, 8) & fsh(d0, d1, 13) & fsh(d0, d1, 18) & fsh(d0, d1, 23) != 0 {
                return false;
            }
        }
    }
    true
}

/// Splits lines into shape + values; buffers are kept from line to line.
#[derive(Default)]
pub struct Front {
    // the line's masks, 64 bytes per word (one word more, always 0, at the end)
    dg: Vec<u64>,
    sp: Vec<u64>,
    hx: Vec<u64>,
    ds: Vec<u64>,
    caps: Vec<u64>,
    lower: Vec<u64>,
    cand: Vec<u64>, // capitals where a month / weekday name could start
    pad: Vec<u8>,   // a line copied with room after it (only near the end of the data)
    pub shape: Vec<u8>, // the last line's shape: shape[..len] (and room after it)
    /// The last line went through numbers_line (it can be a witness of the twin path)
    last_ok: bool,
}

impl Front {
    /// Split one line, data[at..at + n]: its values are appended to `out`; returns the shape's
    /// length (the shape is in self.shape). Reads up to ROOM bytes after the line.
    #[inline(never)]
    pub fn split(&mut self, data: &[u8], at: usize, n: usize, cfg: &SplitCfg, out: &mut Vec<Val>) -> usize {
        // a shape is never longer than its line (a value of 1+ bytes becomes one mark)
        if self.shape.len() < n + ROOM {
            self.shape.resize(n + ROOM, 0);
        }
        out.reserve(n + 1);
        let l: &[u8] = if at + n + ROOM <= data.len() {
            &data[at..at + n + ROOM]
        } else {
            self.pad.clear();
            self.pad.extend_from_slice(&data[at..at + n]);
            self.pad.resize(n + ROOM, 0xFF);
            &self.pad
        };
        let nw = n.div_ceil(64);
        if self.dg.len() < nw + 1 {
            for m in [&mut self.dg, &mut self.sp, &mut self.hx, &mut self.ds, &mut self.caps, &mut self.lower, &mut self.cand] {
                m.resize(nw + 1, 0);
            }
        }
        let (dg, sp, hx, ds, caps, lower, cand) = (&mut self.dg[..nw + 1], &mut self.sp[..nw + 1], &mut self.hx[..nw + 1], &mut self.ds[..nw + 1], &mut self.caps[..nw + 1], &mut self.lower[..nw + 1], &mut self.cand[..nw + 1]);
        let mut zero = false;
        // a hex letter next to a digit anywhere (else no short hex number can be in the line)
        let (mut mix, mut top) = (0u64, (0u64, 0u64));

        // (without month / weekday names, the capitals and small letters are made below, only
        // for a line that needs them: the same masks)
        let cl_now = cfg.names;
        for w in 0..nw {
            // SAFETY: l has ROOM >= 64 bytes after the line, so 64 bytes from 64 * w < n are there
            let keep = if w + 1 == nw && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 };
            let ([d, s, h, dsh], z) = if cl_now {
                let ([d, s, h, dsh, cp, lw], z) = unsafe { masks64(l.as_ptr().add(64 * w)) };
                caps[w] = cp & keep;
                lower[w] = lw & keep;
                ([d, s, h, dsh], z)
            } else {
                unsafe { masks64a(l.as_ptr().add(64 * w)) }
            };
            let (d, s, h) = (d & keep, s & keep, h & keep);
            mix |= h & (d << 1 | top.0) | d & (h << 1 | top.1);
            top = (d >> 63, h >> 63);
            dg[w] = d;
            sp[w] = s;
            hx[w] = h;
            ds[w] = dsh & keep;
            if z && zeros64(&l[64 * w..]) & keep != 0 {
                zero = true;
            }
        }
        for m in [&mut *dg, &mut *sp, &mut *hx, &mut *ds, &mut *caps, &mut *lower, &mut *cand] {
            m[nw] = 0;
        }

        let sh = self.shape.as_mut_ptr();
        let base = out.len();
        let vp = unsafe { out.as_mut_ptr().add(base) };
        self.last_ok = false;
        if zero {
            // raw line: template 0 = one text value
            unsafe {
                *sh = MARK;
                vp.write((1, 0, 0, n as u32));
                out.set_len(base + 1);
            }
            return 1;
        }
        // name candidates: a capital followed by two small letters, at a word start or right
        // after one sign that starts a word (the two bytes before the line count as spaces)
        let mut names = false;
        if cfg.names {
            let (mut psp, mut psg) = (3u64, 0u64); // the word before's last 2 space bits / last sign bit
            for w in 0..nw {
                let s = sp[w];
                let sg = !(dg[w] | s | caps[w] | lower[w]) & if w + 1 == nw && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 };
                let lw = lower[w] as u128 | (lower[w + 1] as u128) << 64;
                let low2 = (lw >> 1 & lw >> 2) as u64;
                let sp1 = s << 1 | psp >> 1;
                let sp2 = s << 2 | psp;
                let sg1 = sg << 1 | psg;
                let c = caps[w] & low2 & (sp1 | sg1 & sp2);
                cand[w] = c;
                names |= c != 0;
                psp = s >> 62;
                psg = sg >> 63;
            }
        }
        let on = only_numbers_line(dg, hx, ds, nw);
        if !cl_now && (!on || (cfg.hex5 && mix != 0)) {
            // the line needs them (hex5_line or words_line)
            for w in 0..nw {
                let keep = if w + 1 == nw && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 };
                let [cp, lw] = unsafe { masks64b(l.as_ptr().add(64 * w)) };
                caps[w] = cp & keep;
                lower[w] = lw & keep;
            }
        }
        let (dg, sp, hx, ds, cand, caps, lower) = (&dg[..], &sp[..], &hx[..], &ds[..], &cand[..], &caps[..], &lower[..]);
        let num_path = on && !(cfg.hex5 && mix != 0 && hex5_line(dg, hx, caps, lower, nw));
        self.last_ok = num_path;
        let (sl, nv) = unsafe {
            if num_path {
                if names {
                    numbers_line::<true>(l, n, nw, dg, sp, cand, sh, vp, cfg)
                } else {
                    numbers_line::<false>(l, n, nw, dg, sp, cand, sh, vp, cfg)
                }
            } else {
                words_line(l, n, dg, sp, hx, ds, &caps[..], &lower[..], if names { Some(cand) } else { None }, sh, vp, cfg)
            }
        };
        debug_assert!(nv <= n + 1 && sl <= n);
        unsafe { out.set_len(base + nv) };
        sl
    }
}

/// A line that passed only_numbers_line: its values are its digit runs, some read as IPv4
/// addresses, clock times or long IDs, and month / weekday names (cand: their candidates);
/// everything else is copied to the shape. Returns (shape length, number of values).
/// SAFETY: l has n + ROOM bytes; dg, sp, cand have nw + 1 words; sh has room for n + ROOM
/// bytes and vp for n + 1 values.
#[inline(never)]
unsafe fn numbers_line<const NAMES_ON: bool>(l: &[u8], n: usize, nw: usize, dg: &[u64], sp: &[u64], cand: &[u64], sh: *mut u8, vp: *mut Val, cfg: &SplitCfg) -> (usize, usize) {
    let (mut sl, mut nv) = (0usize, 0usize);
    let mut done = 0; // l[..done] is done
    let mut carry = 0u64; // the last bit of the word before (a digit there: no run starts at bit 0)
    let lp = l.as_ptr();
    let maxw8 = cfg.maxw.min(8);
    unsafe {
        for w in 0..nw {
            let d = *dg.get_unchecked(w);
            let starts = d & !(d << 1 | carry);
            carry = d >> 63;
            let mut ev = if NAMES_ON { starts | *cand.get_unchecked(w) } else { starts };
            // events before `done` (inside a value that ran on from the word before) are dropped
            if done > w * 64 {
                ev &= if done - w * 64 >= 64 { 0 } else { !0u64 << (done - w * 64) };
            }
            while ev != 0 {
                let b = ev.trailing_zeros() as usize;
                ev &= ev - 1;
                let p = w * 64 + b;
                if NAMES_ON && d >> b & 1 == 0 {
                    // a name candidate
                    if name_key(l, p, n) {
                        if let Some((sl2, nv2, e)) = name_word(l, n, p, sp, dg, sh, sl, vp, nv, done) {
                            (sl, nv, done) = (sl2, nv2, e);
                            ev &= if e - w * 64 >= 64 { 0 } else { !0u64 << (e - w * 64) };
                        }
                    }
                    continue;
                }
                // a digit run l[p..e]
                let r = (!(d >> b)).trailing_zeros() as usize;
                let e = if b + r < 64 { p + r } else { next_zero(dg, p) };
                copy16f(sh.add(sl), lp.add(done), p - done);
                sl += p - done;
                let wd = e - p;
                let next = if e < n { *lp.add(e) } else { b' ' };
                if (next == b'.' && wd <= 3) || (next == b':' && wd <= 2) {
                    if let Some((kind, x, end)) = smart_at(l, p, wd, next, n, bits_at32(dg, p), cfg) {
                        *sh.add(sl) = MARK;
                        vp.add(nv).write((kind, x, p as u32, end as u32));
                        sl += 1;
                        nv += 1;
                        done = end;
                        ev &= if end - w * 64 >= 64 { 0 } else { !0u64 << (end - w * 64) };
                        continue;
                    }
                }
                if wd <= maxw8 {
                    // most numbers: 1-8 digits
                    let x = digits8(l, p, wd);
                    let kind = ((wd > 1) & (*lp.add(p) == b'0')) as u8 * (3 + wd as u8);
                    let pad = if kind == 0 { spn_pad(l, p, wd, cfg) } else { 0 };
                    if pad > 0 {
                        // a space-padded number: its pad leaves the shape
                        sl -= pad;
                        *sh.add(sl) = MARK;
                        vp.add(nv).write((SPN + (pad + wd) as u8, x, (p - pad) as u32, e as u32));
                    } else {
                        *sh.add(sl) = MARK;
                        vp.add(nv).write((kind, x, p as u32, e as u32));
                    }
                } else {
                    let v = long_digit_value(l, p, e, &mut sl, cfg);
                    *sh.add(sl) = MARK;
                    vp.add(nv).write(v);
                }
                sl += 1;
                nv += 1;
                done = e;
            }
        }
        copy16f(sh.add(sl), lp.add(done), n - done);
    }
    (sl + n - done, nv)
}

/// A line that may hold a hex value or a UUID: word by word around each digit (and name
/// candidate), as split.rs does.
#[inline(never)]
unsafe fn words_line(l: &[u8], n: usize, dg: &[u64], sp: &[u64], hx: &[u64], ds: &[u64], caps: &[u64], lower: &[u64], cand: Option<&[u64]>, sh: *mut u8, vp: *mut Val, cfg: &SplitCfg) -> (usize, usize) {
    let (mut sl, mut nv) = (0, 0);
    let mut done = 0; // l[..done] is in the shape
    let mut cpos = 0; // the next name candidate is looked for from here
    let lp = l.as_ptr();
    loop {
        let d = next_one(dg, done);
        if let Some(cm) = cand {
            if let Some(c) = next_one(cm, cpos.max(done)) {
                if d.map_or(true, |d| c < d) {
                    cpos = c + 1;
                    if name_key(l, c, n) {
                        if let Some((sl2, nv2, e)) = unsafe { name_word(l, n, c, sp, dg, sh, sl, vp, nv, done) } {
                            (sl, nv, done) = (sl2, nv2, e);
                        }
                    }
                    continue;
                }
            }
        }
        let Some(p) = d else { break };
        // the word around the next digit: from after the space before it to the space after it
        let i = space_before(sp, p).map_or(0, |s| s + 1);
        let j = space_after(sp, p, n);
        unsafe { copy16(sh.add(sl), lp.add(done), i - done) };
        sl += i - done;
        (sl, nv) = if j - i < 64 && only_numbers(j - i, bits_at(dg, i, j - i), bits_at(hx, i, j - i), bits_at(ds, i, j - i)) && !(cfg.hex5 && {
            let (d, h) = (bits_at(dg, i, j - i) as u128, bits_at(hx, i, j - i) as u128);
            let nh = (bits_at(caps, i, j - i) as u128 | bits_at(lower, i, j - i) as u128) & !h;
            hex5_maybe(d | h, h, d, nh)
        }) {
            unsafe { split_numbers(l, i, j, bits_at(dg, i, j - i), dg, sh, sl, vp, nv, cfg) }
        } else {
            unsafe { split_word_fast(l, i, j, dg, hx, ds, caps, lower, sh, sl, vp, nv, cfg) }
        };
        done = j;
    }
    unsafe { copy16(sh.add(sl), lp.add(done), n - done) };
    (sl + n - done, nv)
}

/// A word l[i..j] shorter than 64 bytes that passed only_numbers: its values are its digit runs
/// (d: their mask from i on), some of them read as IPv4 addresses, clock times or long IDs.
#[inline(always)]
unsafe fn split_numbers(l: &[u8], i: usize, j: usize, d: u64, dg_all: &[u64], sh: *mut u8, mut sl: usize, vp: *mut Val, mut nv: usize, cfg: &SplitCfg) -> (usize, usize) {
    let mut k = i; // l[i..k] is done
    let mut dm = d;
    let lp = l.as_ptr();
    while dm != 0 {
        let a = i + dm.trailing_zeros() as usize;
        let e = a + (!(d >> (a - i))).trailing_zeros() as usize;
        unsafe { copy16(sh.add(sl), lp.add(k), a - k) };
        sl += a - k;
        let w = e - a;
        let next = if e < j { l[e] } else { b' ' };
        let mut v = None;
        if (next == b'.' && w <= 3) || (next == b':' && w <= 2) {
            if let Some((kind, x, end)) = smart_at(l, a, w, next, j, bits_at32(dg_all, a), cfg) {
                v = Some((kind, x, a as u32, end as u32));
            }
        }
        let v = match v {
            Some(v) => v,
            None => digit_value(l, a, e, &mut sl, cfg),
        };
        unsafe {
            *sh.add(sl) = MARK;
            vp.add(nv).write(v);
        }
        sl += 1;
        nv += 1;
        k = v.3 as usize;
        dm = if k - i >= 64 { 0 } else { d & (!0u64 << (k - i)) };
    }
    unsafe { copy16(sh.add(sl), lp.add(k), j - k) };
    (sl + j - k, nv)
}

/// Whether bit k of m is set.
#[inline(always)]
fn bit(m: &[u64], k: usize) -> bool {
    m[k / 64] >> (k % 64) & 1 == 1
}

/// Whether m has a set bit in a..b.
#[inline(always)]
fn any_in(m: &[u64], a: usize, b: usize) -> bool {
    if a >= b {
        return false;
    }
    let (wa, wb) = (a / 64, (b - 1) / 64);
    let (lo, hi) = (!0u64 << (a % 64), !0u64 >> (63 - (b - 1) % 64));
    if wa == wb {
        return m[wa] & lo & hi != 0;
    }
    if m[wa] & lo != 0 || m[wb] & hi != 0 {
        return true;
    }
    m[wa + 1..wb].iter().any(|&x| x != 0)
}

/// The first place at or after k where a or b has a set bit, or `lim` when there is none
/// before it.
#[inline(always)]
fn next_one2(a: &[u64], b: &[u64], k: usize, lim: usize) -> usize {
    let mut w = k / 64;
    let mut x = (a[w] | b[w]) & (!0u64 << (k % 64));
    loop {
        if x != 0 {
            return (w * 64 + x.trailing_zeros() as usize).min(lim);
        }
        w += 1;
        if w * 64 >= lim || w >= a.len() {
            return lim;
        }
        x = a[w] | b[w];
    }
}

/// The first place at or after k where neither a nor b has a set bit (they end with a 0 word).
#[inline(always)]
fn next_zero2(a: &[u64], b: &[u64], k: usize) -> usize {
    let mut w = k / 64;
    let mut x = !(a[w] | b[w]) & (!0u64 << (k % 64));
    while x == 0 {
        w += 1;
        x = !(a[w] | b[w]);
    }
    w * 64 + x.trailing_zeros() as usize
}

/// Dashes of a UUID (8-4-4-4-12) in its 36 bytes.
const UUID_DASHES: u64 = 1 << 8 | 1 << 13 | 1 << 18 | 1 << 23;

/// split.rs split_word with the line's masks (digits, hex letters, dashes, capitals, small letters):
/// the same per-character rules, with each test done on the masks and the bytes that start
/// nothing copied in one go.
#[inline(never)]
unsafe fn split_word_fast(l: &[u8], i: usize, j: usize, dg: &[u64], hx: &[u64], ds: &[u64], caps: &[u64], lower: &[u64], sh: *mut u8, mut sl: usize, vp: *mut Val, mut nv: usize, cfg: &SplitCfg) -> (usize, usize) {
    let lp = l.as_ptr();
    let mut k = i;
    let push = |sl: &mut usize, nv: &mut usize, v: Val| unsafe {
        *sh.add(*sl) = MARK;
        *sl += 1;
        vp.add(*nv).write(v);
        *nv += 1;
    };
    while k < j {
        let (isdig, ishex) = (bit(dg, k), bit(hx, k));
        if !isdig && !ishex {
            // up to the next hex character: copied as they are
            let q = next_one2(dg, hx, k, j);
            unsafe { copy16(sh.add(sl), lp.add(k), q - k) };
            sl += q - k;
            k = q;
            continue;
        }
        let r = if isdig { next_zero(dg, k) - k } else { 0 }; // the digit run from k
        if isdig {
            // an address or a clock time starts with 1-3 digits and then '.' or ':'
            let nx = if r < 4 && k + r < j { l[k + r] } else { 0 };
            if (nx == b'.' && r <= 3) || (nx == b':' && r <= 2) {
                if let Some((kind, v, end)) = smart_at(l, k, r, nx, j, bits_at32(dg, k), cfg) {
                    push(&mut sl, &mut nv, (kind, v, k as u32, end as u32));
                    k = end;
                    continue;
                }
            }
        }
        // a UUID or a long hex run (8+ hex characters with a digit) inside the word
        if j - k >= 36 {
            let x36 = bits_at(dg, k, 36) | bits_at(hx, k, 36);
            let d36 = bits_at(ds, k, 36);
            if x36 | UUID_DASHES == (1u64 << 36) - 1 && d36 & UUID_DASHES == UUID_DASHES {
                let (up, lo) = (bits_at(caps, k, 36) != 0, bits_at(lower, k, 36) != 0);
                if !(up && lo) {
                    push(&mut sl, &mut nv, (100 + up as u8, 0, k as u32, (k + 36) as u32));
                    k += 36;
                    continue;
                }
            }
        }
        let hr = next_zero2(dg, hx, k) - k;
        if hr >= 8 && (isdig || any_in(dg, k, k + hr)) && (ishex || any_in(hx, k, k + hr)) {
            let r2 = k + hr;
            let (up, lo) = (any_in(caps, k, r2), any_in(lower, k, r2));
            let v = if cfg.hexnum && hr <= 16 && !(up && lo) {
                // 8-16 hex digits in one case: a number (kinds HEXNUM..: width and case)
                let v = l[k..r2].iter().fold(0u64, |a, &c| a << 4 | hexval(c) as u64);
                (HEXNUM + (hr - 8) as u8 + 9 * up as u8, v, k as u32, r2 as u32)
            } else {
                let hk = if up && lo { 1 } else if hr % 2 == 0 { 102 + up as u8 } else { 1 };
                (hk, 0, k as u32, r2 as u32)
            };
            push(&mut sl, &mut nv, v);
            k = r2;
            continue;
        }
        if cfg.hex5 && (5..8).contains(&hr) && (isdig || any_in(dg, k, k + hr)) && (ishex || any_in(hx, k, k + hr)) && (k == i || !l[k - 1].is_ascii_alphanumeric()) && (k + hr == j || !l[k + hr].is_ascii_alphanumeric()) && hex_case(&l[k..k + hr]) < 2 {
            // 5-7 hex digits alone between signs: a short hex number (HEXS)
            let r2 = k + hr;
            let v = l[k..r2].iter().fold(0u64, |a, &c| a << 4 | hexval(c) as u64);
            push(&mut sl, &mut nv, (HEXS + (hr - 5) as u8 + 3 * hex_case(&l[k..r2]), v, k as u32, r2 as u32));
            k = r2;
            continue;
        }
        if isdig {
            let v = digit_value(l, k, k + r, &mut sl, cfg);
            push(&mut sl, &mut nv, v);
            k += r;
        } else {
            unsafe { *sh.add(sl) = l[k] };
            sl += 1;
            k += 1;
        }
    }
    (sl, nv)
}

/// A fast hash of a shape: two CRC32-C lanes over 8-byte words (the ends read as overlapping
/// words), mixed. Only used inside one Shapes table (pass 1 of a block or a part of it).
#[inline(always)]
fn shape_hash(b: &[u8]) -> u64 {
    #[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
    if super::x86::level() >= super::x86::SSE42 {
        // SAFETY: the processor has SSE4.2 (level): the CRC32-C instruction, as on ARM
        return unsafe { super::x86::shape_hash_crc(b) };
    }
    let n = b.len();
    let rd = |p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
    let rd4 = |p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
    #[cfg(target_arch = "aarch64")]
    let (c8, c4) = (|h: u32, x: u64| unsafe { std::arch::aarch64::__crc32cd(h, x) }, |h: u32, x: u32| unsafe { std::arch::aarch64::__crc32cw(h, x) });
    #[cfg(not(target_arch = "aarch64"))]
    let (c8, c4) = (|h: u32, x: u64| ((h as u64 ^ x).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32 ^ (x as u32).rotate_left(7), |h: u32, x: u32| (h ^ x).wrapping_mul(0x9E37_79B9).rotate_left(13));
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

/// a == b for byte strings of the same length n (16 bytes per step, the end read as an
/// overlapping word; with NEON 32 bytes per step from 32 bytes on, the end read as an
/// overlapping 32-byte block: every read stays inside both slices).
#[inline(always)]
fn same(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    if n >= 32 {
        use std::arch::aarch64::*;
        // SAFETY: every 32-byte read starts at p with p + 32 <= n, or at n - 32 (n >= 32), so it
        // is inside a and inside b (both n bytes long)
        unsafe {
            let (pa, pb) = (a.as_ptr(), b.as_ptr());
            let eq32 = |p: usize| vminvq_u8(vandq_u8(vceqq_u8(vld1q_u8(pa.add(p)), vld1q_u8(pb.add(p))), vceqq_u8(vld1q_u8(pa.add(p + 16)), vld1q_u8(pb.add(p + 16))))) == 0xFF;
            let mut p = 0;
            while p + 32 < n {
                if !eq32(p) {
                    return false;
                }
                p += 32;
            }
            return eq32(n - 32);
        }
    }
    let r8 = |x: &[u8], p: usize| u64::from_le_bytes(x[p..p + 8].try_into().unwrap());
    if n >= 16 {
        let mut p = 0;
        while p + 16 < n {
            if (r8(a, p) ^ r8(b, p)) | (r8(a, p + 8) ^ r8(b, p + 8)) != 0 {
                return false;
            }
            p += 16;
        }
        (r8(a, n - 16) ^ r8(b, n - 16)) | (r8(a, n - 8) ^ r8(b, n - 8)) == 0
    } else if n >= 8 {
        (r8(a, 0) ^ r8(b, 0)) | (r8(a, n - 8) ^ r8(b, n - 8)) == 0
    } else {
        a == b
    }
}

/// The distinct shapes of pass 1 by hash: each slot holds what a probe needs (hash tag, shape
/// number + 1, place and length of its text), so a lookup reads one slot and the text.
#[derive(Clone, Copy, Default)]
struct Slot {
    tag: u32,
    id: u32, // shape number + 1 (0: empty)
    off: u32,
    len: u32,
}

#[derive(Default)]
struct ShapeTab {
    slots: Vec<Slot>,
}

impl ShapeTab {
    /// The shape's number in `t` (its text and place added there when new), and whether it is new.
    #[inline(always)]
    fn find_or_add(&mut self, t: &mut Shapes, shape: &[u8]) -> (u32, bool) {
        let h = shape_hash(shape);
        if self.slots.len() < 2 * t.at.len() + 2 {
            let size = (self.slots.len() * 2).max(1024);
            let mut slots = vec![Slot::default(); size];
            for (k, &(a, n)) in t.at.iter().enumerate() {
                let h = shape_hash(&t.text[a as usize..(a + n) as usize]);
                let mut s = h as usize & (size - 1);
                while slots[s].id != 0 {
                    s = (s + 1) & (size - 1);
                }
                slots[s] = Slot { tag: (h >> 32) as u32, id: k as u32 + 1, off: a, len: n };
            }
            self.slots = slots;
        }
        let mask = self.slots.len() - 1;
        let (tag, n) = ((h >> 32) as u32, shape.len() as u32);
        let mut s = h as usize & mask;
        loop {
            let e = self.slots[s];
            if e.id == 0 {
                break;
            }
            if e.tag == tag && e.len == n && same(&t.text[e.off as usize..(e.off + n) as usize], shape) {
                return (e.id - 1, false);
            }
            s = (s + 1) & mask;
        }
        let k = t.at.len() as u32;
        let a = t.text.len() as u32;
        self.slots[s] = Slot { tag, id: k + 1, off: a, len: n };
        t.at.push((a, n));
        t.text.extend_from_slice(shape);
        t.text.push(b'\n');
        (k, true)
    }
}

// ---- Pass 1, twin first (M2): a line with the byte classes of a line split before ----
//
// The splitter's masks (digits, spaces, hex letters, dashes, capitals, small letters, name
// candidates) and all its line-level choices (raw line, numbers_line or words_line, names or
// not, its events) depend only on where the digits are and on the other bytes. A line of the
// same length as an earlier line (its witness) with digits at the same places and the same
// other bytes (a twin) goes the same way. When the witness went through numbers_line, every
// digit of it lies inside one of its values, and its shape is the text around the values'
// places (less the pads of space-padded numbers and the minus of a negative long ID). So the
// twin's values are the witness's, except for values whose reading may have looked at a changed
// digit; those are read again with numbers_line's own steps. When every value read again keeps
// its place (start and end) and long-ID-ness, numbers_line would make the same events, values
// and shape on the twin: the twin takes the witness's shape number and values (the re-read
// ones replaced). Otherwise the line takes the full path.
// What a value's reading may look at (its window, worked out once per witness from the
// witness's line and value row: its program, build_prog):
// - a plain number of 1-8 digits with no clock / address form possible and no pad possible
//   (a P0 entry): its digits only; read again on every twin with no test (digits8's SWAR, two
//   values at a time with NEON) with its leading-zero kind;
// - a run where smart_at may find a clock / IPv4 / dotted / loose time (next byte '.' with 1-3
//   digits or ':' with 1-2, and the form possible on classes): the 16 bytes from its start (the
//   longest form, "HH:MM:SS.ffffff", has 15); the digit mask smart_at gets there (bits_at32) is
//   kept in the program;
// - a longer number, or one after two spaces (a pad possible): its digits;
//   (these are GEN entries: compared with the witness's bytes, read again only when one differs:
//   an IPv4 address, clock, dotted or loose time of the witness by its parts in place; a
//   space-padded number with its leading-zero kind; a long number by long_digit_value; any other
//   by rerun, numbers_line's own event step)
// - month / weekday names and text values (runs over the longest number): never read again.
// Witnesses sit in a table of 8K buckets of 2 by a hash of their classes (skel_key: length and
// three 16-byte pieces with digits read as '0'), the most recent first; a twin becomes its
// bucket's witness. The path taken never changes the output, so the table, its key and the
// windows that turn the path off where it seldom helps do not either. FL_NOFAST turns the
// twin path off; FL_CHECK compares every line (twin or not) with split_line; FL_FUZZ2 fuzzes
// it with lines that share their witness's classes, FL_FUZZ3 with digits changed at the edges
// of the value kinds.

/// Longest twin line (bytes; positions in a program fit in u16).
const TW_MAX: usize = 4096;

/// A witness: a line data[at..at + n] (with ROOM bytes after it in the data) that went through
/// numbers_line, or a twin of one; its shape number, its values (vals[row..row + nv]) and its
/// program (np0 P0 entries in the P0Pair records p0..p0 + np0.div_ceil(2), GEN entries g..g + ng;
/// np0 == NOPROG: not made yet).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Wit {
    at: u32,
    n: u16, // 0: an empty way
    nv: u16,
    r: u32,
    row: u32,
    p0: u32,
    g: u32,
    np0: u16,
    ng: u16,
    _pad: u32,
}
const _: () = assert!(std::mem::size_of::<Wit>() == 32);
const NOPROG: u16 = u16::MAX;

/// A plain number of 1-8 digits at p (no clock / address try, no pad possible): value idx of
/// the line; sh = 8 * (8 - its width), lzk = its kind with a leading zero (0 for one digit).
#[derive(Clone, Copy)]
struct P0 {
    idx: u8,
    sh: u8,
    lzk: u8,
    p: u16,
    e: u16,
}

/// Two P0 entries as the twin path reads them (kernels): one 16-byte load gives both shifts
/// (sh: 8 * (8 - width), for vshlq_u64), and per entry two words that are stored as they are
/// into the value's Val (layout (u8, u64, u32, u32) = value at 0, place at 8, kind at 12, end
/// at 16, checked by VAL_RAW): k1 = p | lzk << 32 (the 8 bytes at offset 8 when the first digit
/// is '0'; else the lzk byte is cleared) and eo = e | (idx * size_of::<Val>()) << 32 (the 8 bytes at offset 16:
/// the end, then the value's byte offset in the row, which lands in the Val's padding).
/// A program's last pair holds one entry when its count is odd (the other is all zero).
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct P0Pair {
    sh: [i64; 2],
    k1: [u64; 2],
    eo: [u64; 2],
}
const _: () = assert!(std::mem::size_of::<P0Pair>() == 48);

/// Whether Val is laid out as P0Pair's raw stores expect: value at 0, place at 8, kind at 12,
/// end at 16, 24 bytes (rustc's layout of (u8, u64, u32, u32) on every 64-bit target today;
/// when it is not, the field-by-field stores run), on a little-endian target (k1 and eo are
/// stored as native u64s: place and end in their low halves).
const VAL_RAW: bool = cfg!(target_endian = "little") && std::mem::offset_of!(Val, 1) == 0 && std::mem::offset_of!(Val, 2) == 8 && std::mem::offset_of!(Val, 0) == 12 && std::mem::offset_of!(Val, 3) == 16 && std::mem::size_of::<Val>() == 24 && std::mem::align_of::<Val>() == 8;

/// P0Pair's k1 with the kind byte cleared (the first digit is not '0').
const K1_NOLZ: u64 = 0xFFFF_FF00_FFFF_FFFF;

/// Store one P0 value: x, and the place, kind and end of entry (k1, eo) whose first digit is
/// first (kind lzk when '0'), into out[idx].
/// SAFETY: (eo >> 32) / size_of::<Val>() = idx < out's room (the entry's value index), and out is 8-aligned.
#[inline(always)]
unsafe fn p0_put(out: *mut Val, k1: u64, eo: u64, first: u8, x: u64) {
    unsafe {
        p0_put_pke(out, k1, eo, first);
        if VAL_RAW {
            *((out as *mut u8).add((eo >> 32) as usize) as *mut u64) = x;
        } else {
            (*out.add((eo >> 32) as usize / std::mem::size_of::<Val>())).1 = x;
        }
    }
}

/// p0_put_pke with the first digit as loaded by a 32-bit byte load (its upper bits 0).
/// SAFETY: p0_put's.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn p0_put_pke32(out: *mut Val, k1: u64, eo: u64, first: u32) {
    unsafe {
        if VAL_RAW {
            let o = (out as *mut u8).add((eo >> 32) as usize) as *mut u64;
            *o.add(1) = if first == b'0' as u32 { k1 } else { k1 & K1_NOLZ };
            *o.add(2) = eo;
        } else {
            p0_put_pke(out, k1, eo, first as u8);
        }
    }
}

/// p0_put without the value: the place, kind and end (the value is stored by the caller).
/// SAFETY: p0_put's.
#[inline(always)]
unsafe fn p0_put_pke(out: *mut Val, k1: u64, eo: u64, first: u8) {
    unsafe {
        let off = (eo >> 32) as usize;
        if VAL_RAW {
            // two 8-byte stores (one stp): place + kind (+ 3 zero padding bytes); end (+ 4
            // padding bytes, which hold off)
            let o = (out as *mut u8).add(off) as *mut u64;
            *o.add(1) = if first == b'0' { k1 } else { k1 & K1_NOLZ };
            *o.add(2) = eo;
        } else {
            let kind = if first == b'0' { (k1 >> 32) as u8 } else { 0 };
            // (spare room of the values: written through the pointer, never borrowed)
            let v = out.add(off / std::mem::size_of::<Val>());
            ((*v).0, (*v).2, (*v).3) = (kind, k1 as u32, eo as u32);
        }
    }
}

/// Any other value with digits: its digit run p..e, the masks of the bytes p..p + 24 its
/// reading may look at, how it is read again (spec) and the digit mask numbers_line gives
/// smart_at there (dm: bits_at32 of the line's digits at p).
/// (kernels: 64 bytes, the masks first with a fourth all-zero one, so the window test is two
/// 16-byte loads of masks and a 32-byte compare; off: the value's byte offset in a row, idx * size_of::<Val>())
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Gen {
    m: [u64; 4],
    idx: u16,
    p: u16,
    e: u16,
    spec: u8,
    k: u8, // SP_CLOCK: its kind
    off: u32,
    dm: u64,
    x: u64, // SP_IP: part j at p + (x >> 16j & 255), x >> (16j + 8) & 255 digits; SP_CLOCK: hours' digits | fraction digits << 8
}
/// How a GEN value is read again: numbers_line's step (rerun); a long number (long_digit_value);
/// an IPv4 address of the witness (its parts read in place: when one has a leading zero or is
/// over 255, rerun); a clock time or a dotted time (no IPv4 form) of the witness (minutes or
/// seconds over 59: rerun).
const SP_GEN: u8 = 0;
const SP_LONG: u8 = 1;
const SP_IP: u8 = 2;
const SP_CLOCK: u8 = 3;
const SP_DOT: u8 = 4;
/// A loose clock time of the witness (H:M:S:F, kind 68): its parts read in place (a leading
/// zero or a part over its limit: rerun).
const SP_LOOSE: u8 = 5;
/// A plain number of 1-8 digits after two spaces (k: its pad without a leading zero): read again
/// with its leading-zero kind; when that moves its start (a pad comes or goes): refused.
const SP_SPN: u8 = 6;

/// Witnesses are kept in a table of 1 << WT_BITS buckets of 2, by skel_key.
const WT_BITS: u32 = 13;

/// A key of a line's classes: its length and its first, middle and last 16 bytes with every
/// digit read as one class (aarch64: same_classes' class of each byte; others: '0'), so lines
/// with the same classes have the same key. lp has 16+ bytes after n.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn skel_key(lp: *const u8, n: usize) -> usize {
    use std::arch::aarch64::*;
    unsafe {
        // kernels: each byte as its class (same_classes' map: digits 0, every other byte value its
        // own; 2 operations, was 3); any function of the classes is a valid key
        let (k, c9) = (vdupq_n_u8(0u8.wrapping_sub(b'0')), vdupq_n_u8(9));
        let mid = n.max(16) / 2 - 8; // (the middle 16 bytes; the first ones when n < 16)
        let (x, y, z) = (vld1q_u8(lp), vld1q_u8(lp.add(n.max(16) - 16)), vld1q_u8(lp.add(mid)));
        let mut nx = vqsubq_u8(vaddq_u8(x, k), c9);
        let mut ny = vqsubq_u8(vaddq_u8(y, k), c9);
        let mut nz = vqsubq_u8(vaddq_u8(z, k), c9);
        if n < 16 {
            // (all three are the first 16 bytes: only the line's n count)
            let idx = vld1q_u8([0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
            let keep = vcltq_u8(idx, vdupq_n_u8(n as u8));
            nx = vandq_u8(nx, keep);
            ny = vandq_u8(ny, keep);
            nz = vandq_u8(nz, keep);
        }
        // the three folded into one (the last two turned by 5 and 11 bytes first), then two CRC lanes
        let v = veorq_u8(veorq_u8(nx, vextq_u8::<5>(ny, ny)), vextq_u8::<11>(nz, nz));
        let a = vreinterpretq_u64_u8(v);
        let h1 = __crc32cd(n as u32, vgetq_lane_u64(a, 0));
        let h2 = __crc32cd(0x9E37_79B9, vgetq_lane_u64(a, 1));
        let h = ((h1 as u64) << 32 | h2 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (h >> (64 - WT_BITS)) as usize
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
unsafe fn skel_key(lp: *const u8, n: usize) -> usize {
    #[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
    if super::x86::level() >= super::x86::SSE42 {
        // SAFETY: lp[..max(n, 16)] is readable (as above); the processor has SSE4.2 (level)
        return unsafe { (super::x86::skel_hash(lp, n) >> (64 - WT_BITS)) as usize };
    }
    let b = unsafe { std::slice::from_raw_parts(lp, n.max(16)) };
    let mut h = n as u64 ^ 0x9E37_79B9_7F4A_7C15;
    for &c in b[..n.min(16)].iter().chain(b[n.max(16) - 16..n].iter()) {
        let c = if c.wrapping_sub(b'0') < 10 { b'0' } else { c };
        h = (h ^ c as u64).wrapping_mul(0x100_0000_01b3);
    }
    (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - WT_BITS)) as usize
}

/// Whether the n bytes at a and at b have their digits at the same places and the same other
/// bytes (both have 15+ bytes of room after n).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn same_classes(a: *const u8, b: *const u8, n: usize) -> bool {
    use std::arch::aarch64::*;
    unsafe {
        // kernels: a byte's class in 2 operations: (c - '0') wrapping, then minus 9 saturating:
        // 0 for the 10 digits, and 1..=246 one to one for the 246 other byte values. So two bytes
        // are equal or both digits exactly when their classes are equal: a difference is
        // class(x) ^ class(y) != 0 (5 operations per 16 bytes instead of 6), and 64 bytes' ORed
        // differences are tested at once (umaxv; was and + shl + cmlt + and + uminv + bics)
        let (k, c9) = (vdupq_n_u8(0u8.wrapping_sub(b'0')), vdupq_n_u8(9));
        let cl = |x: uint8x16_t| vqsubq_u8(vaddq_u8(x, k), c9);
        let dif = |p: usize| veorq_u8(cl(vld1q_u8(a.add(p))), cl(vld1q_u8(b.add(p))));
        let any = |x: uint8x16_t| vmaxvq_u32(vreinterpretq_u32_u8(x)) != 0;
        let mut p = 0;
        while p + 64 < n {
            if any(vorrq_u8(vorrq_u8(dif(p), dif(p + 16)), vorrq_u8(dif(p + 32), dif(p + 48)))) {
                return false;
            }
            p += 64;
        }
        let mut acc = vdupq_n_u8(0);
        while p + 16 < n {
            acc = vorrq_u8(acc, dif(p));
            p += 16;
        }
        // the last 1-16 bytes: only the n - p left count
        let idx = vld1q_u8([0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
        acc = vorrq_u8(acc, vandq_u8(dif(p), vcltq_u8(idx, vdupq_n_u8((n - p) as u8))));
        !any(acc)
    }
}

/// same_classes as it was (16 bytes per step; it reads n + 15 bytes at each), for its test.
#[cfg(all(test, target_arch = "aarch64"))]
unsafe fn same_classes_ref(a: *const u8, b: *const u8, n: usize) -> bool {
    use std::arch::aarch64::*;
    unsafe {
        let (c0, c10) = (vdupq_n_u8(b'0'), vdupq_n_u8(10));
        let mut p = 0;
        loop {
            let (x, y) = (vld1q_u8(a.add(p)), vld1q_u8(b.add(p)));
            let (dx, dy) = (vcltq_u8(vsubq_u8(x, c0), c10), vcltq_u8(vsubq_u8(y, c0), c10));
            let mut bad = vorrq_u8(veorq_u8(dx, dy), vbicq_u8(veorq_u8(x, y), dx));
            if p + 16 > n {
                let idx = vld1q_u8([0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
                bad = vandq_u8(bad, vcltq_u8(idx, vdupq_n_u8((n - p) as u8)));
            }
            if vmaxvq_u8(bad) != 0 {
                return false;
            }
            p += 16;
            if p >= n {
                return true;
            }
        }
    }
}

/// The same with SSE2 (x86_64, x86.rs). SAFETY: as above (15+ bytes of room after n at each).
#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[inline(always)]
unsafe fn same_classes(a: *const u8, b: *const u8, n: usize) -> bool {
    unsafe { super::x86::same_classes(a, b, n) }
}

#[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
#[inline(always)]
unsafe fn same_classes(a: *const u8, b: *const u8, n: usize) -> bool {
    let (x, y) = unsafe { (std::slice::from_raw_parts(a, n), std::slice::from_raw_parts(b, n)) };
    x.iter().zip(y).all(|(&c, &d)| {
        let (dc, dd) = (c.wrapping_sub(b'0') < 10, d.wrapping_sub(b'0') < 10);
        dc == dd && (dc || c == d)
    })
}

fn fast_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NOFAST"))
}

/// Pass 1 with the new splitter (FL_CHECK=1: every line is compared with split_line).
pub fn pass1_fast(data: &[u8], lines: &[&[u8]], cfg: &SplitCfg) -> Pass1 {
    pass1_fast_at(data, lines, cfg, env_on("FL_CHECK"))
}

/// Compare a line's shape and values with split_line's (FL_CHECK, FL_FUZZ2); exits 3 when
/// they differ.
fn check_line(l: &[u8], cfg: &SplitCfg, sh: &[u8], vals: &[Val], fast: bool) {
    let (mut shape, mut cvals): (Vec<u8>, Vec<Val>) = (Vec::new(), Vec::new());
    split_line(l, cfg, &mut shape, &mut cvals);
    if shape[..] != sh[..] || cvals[..] != vals[..] {
        eprintln!("SPLIT DIFFERS ({}): line {:?}\n old {:?} {:?}\n new {:?} {:?}", if fast { "fast path" } else { "splitter" }, String::from_utf8_lossy(l), String::from_utf8_lossy(&shape), cvals, String::from_utf8_lossy(sh), vals);
        std::process::exit(3);
    }
}

/// The program of a witness (line lr[..n], with ROOM bytes after it; values row): its P0 and
/// GEN entries appended to p0s and gens. None: no program (over 255 values, a value without
/// digits that is not a name, or a window over 24 bytes).
/// (kernels: out of line and cold: once per witness, and kept out of twin_run's registers)
#[cold]
#[inline(never)]
fn build_prog(lr: &[u8], n: usize, row: &[Val], cfg: &SplitCfg, p0t: &mut Vec<P0>, p0s: &mut Vec<P0Pair>, gens: &mut Vec<Gen>) -> Option<(u32, u16, u32, u16)> {
    if row.len() > 255 {
        return None;
    }
    // (kernels: the P0 entries are made in p0t, then packed two by two into p0s)
    p0t.clear();
    let (a0, g0) = (p0s.len(), gens.len());
    let maxw8 = cfg.maxw.min(8);
    let dig = |c: u8| c.wrapping_sub(b'0') < 10;
    // the line's digit mask as Front::split makes it: nw words (no bits past n) and a 0 word
    let nw = n.div_ceil(64);
    let mut dgw = [0u64; TW_MAX / 64 + 1];
    for w in 0..nw {
        let keep = if w + 1 == nw && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 };
        // SAFETY: lr has n + ROOM bytes (ROOM = 64), so 64 bytes from 64 * w < n are there
        dgw[w] = unsafe { masks64a(lr.as_ptr().add(64 * w)) }.0[0] & keep;
    }
    let dg = &dgw[..nw + 1];
    let fail = |_p0t: &mut Vec<P0>, gens: &mut Vec<Gen>| {
        gens.truncate(g0);
        None
    };
    for (i, v) in row.iter().enumerate() {
        if v.0 == NAMEK {
            // the same on every twin: copied
            gens.push(Gen { idx: i as u16, p: 0, e: 0, spec: SP_GEN, k: 0, m: [0; 4], off: (i * std::mem::size_of::<Val>()) as u32, dm: 0, x: 0 });
            continue;
        }
        // the digit run: after the pad spaces of a space-padded number
        let p = match next_one(dg, v.2 as usize) {
            Some(p) if p < v.3 as usize => p,
            _ => return fail(p0t, gens),
        };
        let e = next_zero(dg, p);
        let wd = e - p;
        let next = if e < n { lr[e] } else { b' ' };
        // smart_at may give a value here on some twin (its tests on classes only); dm: the digit
        // mask numbers_line gives it (bits_at32)
        let mut smart = cfg.use_smart && ((next == b'.' && wd <= 3) || (next == b':' && wd <= 2)) && !(p > 0 && (lr[p - 1] == next || dig(lr[p - 1])));
        let dm = if smart { bits_at32(dg, p) } else { 0 };
        let run = |q: usize| (!(dm >> (q - p))).trailing_zeros() as usize;
        let (mut ip4, mut dot) = (false, false);
        if smart && next == b'.' {
            // the IPv4 form: four parts of 1-3 digits with dots between, no fifth part
            let mut q = p;
            ip4 = true;
            for part in 0..4 {
                let w = (!(dm >> (q - p))).trailing_zeros() as usize;
                if w == 0 || w > 3 {
                    ip4 = false;
                    break;
                }
                q += w;
                if part < 3 {
                    if q >= n || lr[q] != b'.' || q + 1 - p >= 32 {
                        ip4 = false;
                        break;
                    }
                    q += 1;
                }
            }
            if ip4 && q + 1 < n && lr[q] == b'.' && dig(lr[q + 1]) {
                ip4 = false;
            }
            // the dotted time form H.MM.SS / HH.MM.SS (+ fraction), no more parts after it
            let hw = wd;
            if cfg.dotted && hw <= 2 && dm & ((1 << (hw + 7)) - 1) == (((1u64 << hw) - 1) | 3 << (hw + 1) | 3 << (hw + 4)) && lr[p + hw + 3] == b'.' {
                let mut q = p + hw + 6;
                if q + 1 < n && lr[q] == b'.' && dm >> (q + 1 - p) & 1 == 1 {
                    let fd = (!(dm >> (q + 1 - p))).trailing_zeros() as usize;
                    if fd <= 6 {
                        q += 1 + fd;
                    }
                }
                dot = !(q < n && (lr[q] == b'.' || dig(lr[q])));
            }
            smart = ip4 || dot;
        }
        // a pad is possible only after two spaces (spn_pad)
        let pad = cfg.spaced && p >= 3 && lr[p - 1] == b' ' && lr[p - 2] == b' ';
        if !smart && wd <= maxw8 && !pad {
            p0t.push(P0 { idx: i as u8, sh: (8 * (8 - wd)) as u8, lzk: if wd > 1 { 3 + wd as u8 } else { 0 }, p: p as u16, e: e as u16 });
            continue;
        }
        if !smart && wd > cfg.maxw.max(19) {
            // text (kind 1, value 0) on every twin: copied
            gens.push(Gen { idx: i as u16, p: 0, e: 0, spec: SP_GEN, k: 0, m: [0; 4], off: (i * std::mem::size_of::<Val>()) as u32, dm: 0, x: 0 });
            continue;
        }
        let win = if smart { 16.min(n - p) } else { wd };
        if win > 24 {
            return fail(p0t, gens);
        }
        let lane = |o: usize| if win >= o + 8 { !0u64 } else if win > o { (1u64 << (8 * (win - o))) - 1 } else { 0 };
        let (mut spec, mut k, mut x) = (SP_GEN, 0u8, 0u64);
        if !smart && wd > maxw8 {
            spec = if pad { SP_GEN } else { SP_LONG };
        } else if !smart {
            // a plain number after two spaces: its pad when it has no leading zero (space_pad's
            // test on classes; the leading-zero test is the twin's)
            spec = SP_SPN;
            let mut a = p - 2;
            while a > 0 && lr[a - 1] == b' ' {
                a -= 1;
            }
            let pw = p - a - 1;
            k = if a == 0 || pw + wd > SPN_MAX as usize { 0 } else { pw as u8 };
        } else if v.0 == 30 {
            // an IPv4 address: its four parts' places and widths
            let mut q = p;
            spec = SP_IP;
            for j in 0..4 {
                let w = run(q);
                x |= ((q - p) as u64 | (w as u64) << 8) << (16 * j);
                q += w + 1;
            }
        } else if (40..68).contains(&v.0) {
            spec = SP_CLOCK;
            k = v.0;
            x = wd as u64 | (((v.0 - 40) >> 2) as u64) << 8;
        } else if v.0 == 68 && next == b':' && cfg.loose {
            // a loose time: its four parts' places and widths (at most 2, 2, 2, 3 digits)
            let mut q = p;
            spec = SP_LOOSE;
            for j in 0..4 {
                let w = run(q).min(if j == 3 { 3 } else { 2 });
                x |= ((q - p) as u64 | (w as u64) << 8) << (16 * j);
                q += w + 1;
            }
        } else if (110..124).contains(&v.0) && !ip4 {
            // a dotted time where no twin can be an IPv4 address (smart_at tries that first)
            spec = SP_DOT;
            k = v.0;
            x = wd as u64 | (((v.0 - 110 - (wd as u8 - 1)) >> 1) as u64) << 8;
        }
        gens.push(Gen { idx: i as u16, p: p as u16, e: e as u16, spec, k, m: [lane(0), lane(8), lane(16), 0], off: (i * std::mem::size_of::<Val>()) as u32, dm, x });
    }
    for pr in p0t.chunks(2) {
        let mut q = P0Pair::default();
        for (j, e) in pr.iter().enumerate() {
            q.sh[j] = e.sh as i64;
            q.k1[j] = e.p as u64 | (e.lzk as u64) << 32;
            q.eo[j] = e.e as u64 | ((e.idx as usize * std::mem::size_of::<Val>()) as u64) << 32;
        }
        p0s.push(q);
    }
    Some((a0 as u32, p0t.len() as u16, g0 as u32, (gens.len() - g0) as u16))
}

/// numbers_line's value for the digit run l[p..e] of a line of n bytes (dm: bits_at32 of its
/// digit mask at p), as numbers_line works it out (its event step, without the shape).
#[inline(never)]
fn rerun(l: &[u8], n: usize, p: usize, e: usize, dm: u64, cfg: &SplitCfg) -> Val {
    let wd = e - p;
    let next = if e < n { l[e] } else { b' ' };
    if (next == b'.' && wd <= 3) || (next == b':' && wd <= 2) {
        if let Some((kind, x, end)) = smart_at(l, p, wd, next, n, dm, cfg) {
            return (kind, x, p as u32, end as u32);
        }
    }
    if wd <= cfg.maxw.min(8) {
        let x = digits8(l, p, wd);
        let kind = ((wd > 1) & (l[p] == b'0')) as u8 * (3 + wd as u8);
        let pad = if kind == 0 { spn_pad(l, p, wd, cfg) } else { 0 };
        if pad > 0 { (SPN + (pad + wd) as u8, x, (p - pad) as u32, e as u32) } else { (kind, x, p as u32, e as u32) }
    } else {
        let mut sl = n; // (long_digit_value lowers the shape's end: not needed here)
        long_digit_value(l, p, e, &mut sl, cfg)
    }
}

/// The P0 values of a twin (line at lp, 8+ bytes readable from each entry's p) into out: each
/// entry's 1-8 digits read as digits8 does (two at a time with NEON), its kind (leading zero or
/// not) and its place (kernels: from P0Pair records, two values per step, each Val written by
/// one 8-byte store of the value and one stp of place + kind and end).
/// SAFETY: every entry has p + 8 <= the readable bytes at lp (build_prog: p < n, ROOM >= 8 bytes
/// after the line) and an idx below out's room (a value index of the witness: < nv); pr holds
/// np0.div_ceil(2) records (build_prog pushed them together).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn p0_vals(lp: *const u8, out: *mut Val, pr: *const P0Pair, np0: usize) {
    use std::arch::aarch64::*;
    unsafe {
        let c0 = vdupq_n_u8(b'0');
        let m10 = vreinterpretq_u8_u16(vdupq_n_u16(10 | 1 << 8)); // bytes 10, 1, 10, 1, ...
        let m100 = vreinterpretq_u16_u32(vdupq_n_u32(100 | 1 << 16));
        let m10k = vreinterpretq_u32_u64(vdupq_n_u64(10_000 | 1 << 32));
        let mut r = pr;
        let end = pr.add(np0 / 2);
        while r < end {
            let q = &*r;
            let sh = vld1q_s64(q.sh.as_ptr());
            let (ka, kb, ea, eb) = (q.k1[0], q.k1[1], q.eo[0], q.eo[1]);
            // the 8 bytes at each place and its first digit, with the place (the low 32 bits of k1:
            // p < n <= TW_MAX) as a register offset of every load (inline asm: the compiler took the
            // first digits out of the 8-byte loads' register with a copy and a lane move, then
            // needed the place added in a register of its own)
            let (da, db): (uint8x8_t, uint8x8_t);
            let (fa, fb): (u32, u32);
            std::arch::asm!(
                "ldr {da:d}, [{lp}, {ka:w}, uxtw]",
                "ldr {db:d}, [{lp}, {kb:w}, uxtw]",
                "ldrb {fa:w}, [{lp}, {ka:w}, uxtw]",
                "ldrb {fb:w}, [{lp}, {kb:w}, uxtw]",
                lp = in(reg) lp, ka = in(reg) ka, kb = in(reg) kb,
                da = out(vreg) da, db = out(vreg) db, fa = out(reg) fa, fb = out(reg) fb,
                options(pure, readonly, nostack, preserves_flags));
            let x = vcombine_u8(da, db);
            let x = vreinterpretq_u8_u64(vshlq_u64(vreinterpretq_u64_u8(vsubq_u8(x, c0)), sh));
            let x = vpaddlq_u8(vmulq_u8(x, m10));
            let x = vpaddlq_u16(vmulq_u16(x, m100));
            let x = vpaddlq_u32(vmulq_u32(x, m10k));
            p0_put_pke32(out, ka, ea, fa);
            p0_put_pke32(out, kb, eb, fb);
            // the two values straight from their lanes (st1 of lane 1: no lane move)
            if VAL_RAW {
                let (oa, ob) = ((out as *mut u8).add((ea >> 32) as usize), (out as *mut u8).add((eb >> 32) as usize));
                vst1q_lane_u64::<0>(oa as *mut u64, x);
                std::arch::asm!("st1 {{{x:v}.d}}[1], [{ob}]", x = in(vreg) x, ob = in(reg) ob, options(nostack, preserves_flags));
            } else {
                (*out.add((ea >> 32) as usize / std::mem::size_of::<Val>())).1 = vgetq_lane_u64::<0>(x);
                (*out.add((eb >> 32) as usize / std::mem::size_of::<Val>())).1 = vgetq_lane_u64::<1>(x);
            }
            r = r.add(1);
        }
        if np0 & 1 == 1 {
            let q = &*end;
            let (k, e) = (q.k1[0], q.eo[0]);
            let p = lp.add(k as u32 as usize);
            let x = u64::from_le((p as *const u64).read_unaligned()).wrapping_sub(0x3030_3030_3030_3030) << q.sh[0];
            let x = x.wrapping_mul(10).wrapping_add(x >> 8);
            let m = 0x0000_00FF_0000_00FF;
            p0_put(out, k, e, *p, ((x & m).wrapping_mul(100 + (1_000_000 << 32)).wrapping_add((x >> 16 & m).wrapping_mul(1 + (10_000 << 32)))) >> 32);
        }
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
unsafe fn p0_vals(lp: *const u8, out: *mut Val, pr: *const P0Pair, np0: usize) {
    unsafe {
        for i in 0..np0 {
            let q = &*pr.add(i / 2);
            let (k, e, sh) = (q.k1[i & 1], q.eo[i & 1], q.sh[i & 1]);
            let p = lp.add(k as u32 as usize);
            let x = u64::from_le((p as *const u64).read_unaligned()).wrapping_sub(0x3030_3030_3030_3030) << sh;
            let x = x.wrapping_mul(10).wrapping_add(x >> 8);
            let m = 0x0000_00FF_0000_00FF;
            p0_put(out, k, e, *p, ((x & m).wrapping_mul(100 + (1_000_000 << 32)).wrapping_add((x >> 16 & m).wrapping_mul(1 + (10_000 << 32)))) >> 32);
        }
    }
}

/// Whether no byte of a GEN entry's window differs between the twin (lp) and its witness (wl):
/// the 32 bytes from p of each, masked by g.m (m[3] == 0).
/// SAFETY: p < n (build_prog's; p = 0 for names and text), so p + 32 <= n + ROOM bytes at both.
#[inline(always)]
unsafe fn gen_same(lp: *const u8, wl: *const u8, p: usize, g: &Gen) -> bool {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        let (a, b) = (vld1q_u64(lp.add(p) as *const u64), vld1q_u64(lp.add(p + 16) as *const u64));
        let (c, d) = (vld1q_u64(wl.add(p) as *const u64), vld1q_u64(wl.add(p + 16) as *const u64));
        let (m0, m1) = (vld1q_u64(g.m.as_ptr()), vld1q_u64(g.m.as_ptr().add(2)));
        let x = vorrq_u64(vandq_u64(veorq_u64(a, c), m0), vandq_u64(veorq_u64(b, d), m1));
        vmaxvq_u32(vreinterpretq_u32_u64(x)) == 0
    }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe {
        let rd = |b: *const u8, o: usize| (b.add(p + o) as *const u64).read_unaligned();
        (rd(lp, 0) ^ rd(wl, 0)) & g.m[0] | (rd(lp, 8) ^ rd(wl, 8)) & g.m[1] | (rd(lp, 16) ^ rd(wl, 16)) & g.m[2] == 0
    }
}

/// The values of a twin (line lr[..n], ROOM bytes after it) of a witness (line wl, same length
/// and classes; values wrow[..nv]) into out[..nv]: the P0 entries read again, the GEN entries
/// read again where a byte of their window differs (else the witness's, as names and text are).
/// False: a value read again moved (or changed long-ID-ness): the twin's shape may differ (out is
/// then partly written).
/// SAFETY: wl has n + ROOM bytes; wrow has nv values and out room for nv, apart; the entries
/// name each value index below nv once, with p + 32 <= n + ROOM (build_prog's: p < n, ROOM =
/// 64; gen_same reads 32 bytes from p).
#[inline(always)]
unsafe fn twin_vals(lp: *const u8, wl: *const u8, n: usize, out: *mut Val, wrow: *const Val, p0: *const P0Pair, np0: usize, gens: *const Gen, ng: usize, cfg: &SplitCfg) -> bool {
    unsafe {
        let lr = std::slice::from_raw_parts(lp, n + ROOM);
        let gens = std::slice::from_raw_parts(gens, ng);
        p0_vals(lp, out, p0, np0);
        for g in gens {
            let p = g.p as usize;
            // (names and text values have no window: m == 0, so they are always copied)
            if gen_same(lp, wl, p, g) {
                // kernels: the witness's value as an untyped 24-byte copy (one 16-byte and one
                // 8-byte load and store; the typed copy was 4 loads and 8 stores), at the byte
                // offset kept in the entry. SAFETY: both rows have room for idx < nv (twin_vals'
                // contract), and off = idx * size_of::<Val>()
                let off = g.off as usize;
                std::ptr::copy_nonoverlapping((wrow as *const u8).add(off), (out as *mut u8).add(off), std::mem::size_of::<Val>());
                continue;
            }
            let w = *wrow.add(g.idx as usize);
            let o = out.add(g.idx as usize); // (spare room of the values: written through the pointer, never borrowed)
            match g.spec {
                SP_IP => {
                    let (mut ip, mut bad) = (0u64, false);
                    for j in 0..4 {
                        let (q, w) = (p + (g.x >> (16 * j) & 255) as usize, (g.x >> (16 * j + 8) & 255) as usize);
                        let y = digits8(lr, q, w);
                        bad |= (w > 1) & (*lp.add(q) == b'0') | (y > 255);
                        ip = ip << 8 | y;
                    }
                    if !bad {
                        o.write((30, ip, w.2, w.3));
                        continue;
                    }
                }
                SP_CLOCK | SP_DOT => {
                    let (hw, fd) = ((g.x & 255) as usize, (g.x >> 8) as usize);
                    let h = if hw == 2 { two_digits(lr, p) } else { (*lp.add(p) - b'0') as u64 };
                    let (mi, se) = (two_digits(lr, p + hw + 1), two_digits(lr, p + hw + 4));
                    if mi < 60 && se < 60 {
                        let mut v = (h * 60 + mi) * 60 + se;
                        if fd > 0 {
                            v = v * POW10[fd] + digits8(lr, p + hw + 7, fd);
                        }
                        o.write((g.k, v, w.2, w.3));
                        continue;
                    }
                }
                SP_SPN => {
                    let e = g.e as usize;
                    let (wd, pad) = (e - p, g.k as usize);
                    let x = digits8(lr, p, wd);
                    let v = if wd > 1 && *lp.add(p) == b'0' {
                        (3 + wd as u8, x, p as u32, e as u32)
                    } else if pad > 0 {
                        (SPN + (pad + wd) as u8, x, (p - pad) as u32, e as u32)
                    } else {
                        (0, x, p as u32, e as u32)
                    };
                    if v.2 != w.2 {
                        return false;
                    }
                    o.write(v);
                    continue;
                }
                SP_LOOSE => {
                    let (mut v, mut bad) = (0u64, false);
                    for (j, (lim, mul)) in [(100u64, 1u64), (60, 60), (60, 60), (1000, 1000)].into_iter().enumerate() {
                        let (q, wd) = (p + (g.x >> (16 * j) & 255) as usize, (g.x >> (16 * j + 8) & 255) as usize);
                        let y = digits8(lr, q, wd);
                        bad |= (wd > 1) & (*lp.add(q) == b'0') | (y >= lim);
                        v = v * mul + y;
                    }
                    if !bad {
                        o.write((68, v, w.2, w.3));
                        continue;
                    }
                }
                _ => {}
            }
            let v = if g.spec == SP_LONG {
                let mut sl = n;
                long_digit_value(lr, p, g.e as usize, &mut sl, cfg)
            } else {
                rerun(lr, n, p, g.e as usize, g.dm, cfg)
            };
            if v.2 != w.2 || v.3 != w.3 || (v.0 == LONG_ID) != (w.0 == LONG_ID) {
                return false;
            }
            o.write(v);
        }
    }
    true
}

/// The witness table and the programs of one pass 1, and its counts (FL_FASTSTAT: lines that
/// were not twins, their bytes, refused, programs; counted off the twins' path).
struct Twins {
    wt: Vec<[Wit; 2]>,
    p0s: Vec<P0Pair>,
    p0t: Vec<P0>,
    gens: Vec<Gen>,
    st: [usize; 5],
}

/// The twin path for the lines lines[0..] (kernels: one call does a whole run of twins, so the
/// pass-1 loop, the call and the saved registers are paid once per run, not once per line).
/// lines[0] is tried as twin_line tried a line: it must have 1 <= n <= TW_MAX and
/// at + n + ROOM <= data.len() < 2^32 (the caller's test; pass1_fast_at takes the twin path
/// only for data under 4 GiB, as a witness's place and value row are kept as u32); when it is
/// not a twin the answer is
/// (0, Err(key << 2 | way), 0), way being the way of a witness with the line's classes whose twin
/// moved a value (2: none), with the program made on the way as before.
/// Then lines[1..] are taken while they are twins: each one's values are appended to vals and
/// its shape number to rid, as for lines[0]. The run stops before a line that is not tried
/// (too long, too near the end), whose witness has no program yet, or whose twin moved a value
/// (refused): nothing of it is kept, and the caller tries it again as a first line (so a
/// program is made, and a refusal counted, exactly where twin_line did). It stops at a line
/// whose bucket has no witness with its classes (no other effect) with that line's
/// key << 2 | 2 in the answer, for the caller to take as that line's miss.
/// Answer: (k, stop, bytes): k lines were twins (rid has k more entries); stop as above
/// (usize::MAX: none); bytes: the k lines' n + 1 summed.
#[inline(never)]
fn twin_run(tw: &mut Twins, data: &[u8], lines: &[&[u8]], vals: &mut Vec<Val>, rid: &mut Vec<u32>, cfg: &SplitCfg) -> (usize, usize, usize) {
    // kernels: the lengths of vals and rid live in registers for the run (set at the end); rid
    // has room for every line of the block (pass1_fast_at made it so; reserve checks it once)
    let nl = lines.len();
    rid.reserve(nl);
    let rp = rid.as_mut_ptr();
    let mut rn = rid.len();
    let (mut vb, mut vn, mut vcap) = (vals.as_mut_ptr(), vals.len(), vals.capacity());
    let mut bytes = 0;
    let mut k = 0;
    let stop = loop {
        if k == nl {
            break usize::MAX;
        }
        let l = lines[k];
        let at = l.as_ptr() as usize - data.as_ptr() as usize;
        let n = l.len();
        let first = k == 0;
        if !first && !(n.wrapping_sub(1) < TW_MAX && at + n + ROOM <= data.len()) {
            break usize::MAX;
        }
        // SAFETY: (all reads below) the line has ROOM bytes after it in data (the test above, or
        // the caller's for lines[0]), and a witness is kept only when its own at + n + ROOM <=
        // data.len(); skel_key reads max(n, 16) bytes, same_classes n + 15 at each; key < 1 <<
        // WT_BITS = wt.len(); vb[..vn] are vals' values (vn <= vcap = its capacity)
        unsafe {
            let lp = data.as_ptr().add(at);
            let key = skel_key(lp, n);
            let bk = &mut *tw.wt.as_mut_ptr().add(key);
            let way = if bk[0].n as usize == n && same_classes(lp, data.as_ptr().add(bk[0].at as usize), n) {
                0
            } else if bk[1].n as usize == n && same_classes(lp, data.as_ptr().add(bk[1].at as usize), n) {
                1
            } else {
                break key << 2 | 2;
            };
            let w = &mut bk[way];
            let (nv, row) = (w.nv as usize, w.row as usize);
            let wl = data.as_ptr().add(w.at as usize);
            if w.np0 == NOPROG {
                if !first {
                    break usize::MAX;
                }
                // (k == 0: vals is as the caller gave it, vn == vals.len())
                let x = build_prog(std::slice::from_raw_parts(wl, n + ROOM), n, &vals[row..row + nv], cfg, &mut tw.p0t, &mut tw.p0s, &mut tw.gens);
                let Some(x) = x else { break key << 2 | way };
                (w.p0, w.np0, w.g, w.ng) = x;
                tw.st[3] += 1;
            }
            if vcap - vn < nv {
                vals.set_len(vn);
                vals.reserve(nv);
                (vb, vcap) = (vals.as_mut_ptr(), vals.capacity());
            }
            // (the witness's values vb[row..row + nv] were written before this line's: row + nv
            // <= vn; nv more fit; the program was made from the witness's line, and its entries
            // lie in p0s and gens: build_prog's bounds)
            if !twin_vals(lp, wl, n, vb.add(vn), vb.add(row), tw.p0s.as_ptr().add(w.p0 as usize), w.np0 as usize, tw.gens.as_ptr().add(w.g as usize), w.ng as usize, cfg) {
                if !first {
                    break usize::MAX;
                }
                tw.st[2] += 1;
                break key << 2 | way;
            }
            let r = w.r;
            // the twin is its bucket's witness now (same classes, values and program)
            w.at = at as u32;
            w.row = vn as u32;
            vn += nv;
            if way == 1 {
                bk.swap(0, 1);
            }
            // (rn < rid's capacity: one entry per line, room for nl more)
            *rp.add(rn) = r;
            rn += 1;
            bytes += n + 1;
        }
        k += 1;
    };
    // SAFETY: vb[..vn] and rp[..rn] were written (the lines' values and shape numbers)
    unsafe {
        vals.set_len(vn);
        rid.set_len(rn);
    }
    (k, stop, bytes)
}

fn pass1_fast_at(data: &[u8], lines: &[&[u8]], cfg: &SplitCfg, check: bool) -> Pass1 {
    // room for the values: 8 per line, or one per 8 bytes of text when that is more (lines
    // full of numbers, as in BGL, have 18 per line: growing the vector would copy it twice)
    let span = match (lines.first(), lines.last()) {
        (Some(a), Some(b)) => (b.as_ptr() as usize + b.len()).saturating_sub(a.as_ptr() as usize),
        _ => 0,
    };
    let mut p = Pass1 { shapes: Shapes::default(), rnv: Vec::new(), rid: Vec::with_capacity(lines.len()), vals: vals_buffer((lines.len() * 8).max(span / 8)) };
    let mut f = Front::default();
    let mut tab = ShapeTab::default();
    // the twin path keeps a witness's place, value row and program (p0, g) as u32 (Wit): a line
    // reaches it only with at + n + ROOM <= data.len(), a line has at most n + 1 values, and a
    // witness gets at most one program, of one entry per value, so all fit when the data is
    // under 4 GiB (bigger data: the full path only, the same output)
    let fast = fast_on() && data.len() < u32::MAX as usize;
    // witnesses by the key of their classes, 2 per key, the most recent first
    let mut tw = Twins { wt: if fast { vec![[Wit::default(); 2]; 1 << WT_BITS] } else { Vec::new() }, p0s: Vec::new(), p0t: Vec::new(), gens: Vec::new(), st: [0; 5] };
    let stat = env_on("FL_FASTSTAT");
    // windows of 8192 lines: after one where under a fifth of the lines were twins, the next 7
    // do not try (nor keep witnesses); then one tries again (same output either way). Twins are
    // the window's lines less its misses (tw.st[0] counts every line that is not a twin).
    let (mut win_skip, mut mark_li, mut mark_miss) = (0usize, 0usize, 0usize);
    let mut fast_now = fast;
    // kernels: a line that twin_run stopped at with no witness of its classes in its bucket:
    // that line's key << 2 | 2 (it was tried; usize::MAX: none)
    let mut pend = usize::MAX;
    let nl = lines.len();
    let mut li = 0;
    while li < nl {
        let l = lines[li];
        if li & 8191 == 8191 {
            let hits = (li - mark_li) - (tw.st[0] - mark_miss);
            if win_skip > 0 {
                win_skip -= 1;
            } else if hits * 5 < 8192 {
                win_skip = 7;
            }
            fast_now = fast && win_skip == 0;
            (mark_li, mark_miss) = (li, tw.st[0]);
        }
        let v0 = p.vals.len();
        let at = l.as_ptr() as usize - data.as_ptr() as usize;
        let n = l.len();
        let mut key = usize::MAX;
        let mut way = 2; // the way of the bucket whose witness has the line's classes (2: none)
        if pend != usize::MAX {
            // (tried in twin_run: a miss; after its first line a run never reaches a window's first
            // line (li & 8191 == 8191), so this line is in the same window, and fast_now holds)
            (key, way) = (pend >> 2, pend & 3);
            pend = usize::MAX;
        } else if fast_now && n.wrapping_sub(1) < TW_MAX && at + n + ROOM <= data.len() {
            // this line, then the next ones while they are twins, up to the line before the next
            // window's first line ((li + 1) | 8191)
            let m = nl.min((li + 1) | 8191) - li;
            let (k, stop, _) = twin_run(&mut tw, data, &lines[li..li + m], &mut p.vals, &mut p.rid, cfg);
            if k > 0 {
                if check {
                    // (every line of the run, each with its shape's value count)
                    let mut v = v0;
                    for j in 0..k {
                        let r = p.rid[p.rid.len() - k + j] as usize;
                        let nvj = p.rnv[r] as usize;
                        let (a, len) = p.shapes.at[r];
                        let sh = p.shapes.text[a as usize..(a + len) as usize].to_vec();
                        check_line(lines[li + j], cfg, &sh, &p.vals[v..v + nvj], true);
                        v += nvj;
                    }
                    assert_eq!(v, p.vals.len(), "twin run: value counts");
                }
                li += k;
                pend = stop;
                continue;
            }
            (key, way) = (stop >> 2, stop & 3);
        }
        tw.st[0] += 1;
        tw.st[1] += n + 1;
        let sl = f.split(data, at, n, cfg, &mut p.vals);
        let sh = &f.shape[..sl];
        if check {
            check_line(l, cfg, sh, &p.vals[v0..], false);
        }
        let (r, new) = tab.find_or_add(&mut p.shapes, sh);
        let nv = p.vals.len() - v0;
        if new {
            p.rnv.push(nv as u32);
        }
        if key != usize::MAX && f.last_ok && at + n + ROOM <= u32::MAX as usize && v0 + nv <= u32::MAX as usize && nv < NOPROG as usize {
            let nw = Wit { at: at as u32, n: n as u16, nv: nv as u16, r, row: v0 as u32, p0: 0, g: 0, np0: NOPROG, ng: 0, _pad: 0 };
            let bk = &mut tw.wt[key];
            if way < 2 {
                // (a witness of the same classes whose twin moved a value: this line takes its place)
                bk[way] = nw;
            } else {
                bk[1] = bk[0];
                bk[0] = nw;
            }
        }
        p.rid.push(r);
        li += 1;
    }
    if stat {
        let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        eprintln!("FASTSTAT lines {} misses {} twins {} refused {} programs {} shapes {} twin_bytes {}", lines.len(), tw.st[0], lines.len() - tw.st[0], tw.st[2], tw.st[3], p.shapes.at.len(), bytes - tw.st[1]);
    }
    p
}

/// 64-bit FNV-1a-like hash of bytes (for checksums only).
pub fn sum64(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ b.len() as u64;
    let mut c = b.chunks_exact(8);
    for x in &mut c {
        h = (h ^ u64::from_le_bytes(x.try_into().unwrap())).wrapping_mul(0x100_0000_01b3).rotate_left(23);
    }
    for &x in c.remainder() {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// FL_SUMS: one line per unit with a checksum of every coded stream (all blocks, in order),
/// the coded file tables and the block lists.
pub fn print_sums(name: &str, uc: &[u8], wuc: &[u8], idlists: &[Vec<u8>], widlists: &[Vec<u8>], coded: &[&Vec<Vec<u8>>]) {
    if !env_on("FL_SUMS") {
        return;
    }
    let mut hs = [0u64; 10];
    for c in coded {
        for (k, s) in c.iter().enumerate() {
            hs[k] = (hs[k].rotate_left(7) ^ sum64(s)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    let mut hl = 0u64;
    for l in idlists.iter().chain(widlists) {
        hl = (hl.rotate_left(7) ^ sum64(l)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    let mut s = format!("SUMS {} tbl {:016x} wtbl {:016x} lists {:016x}", name, sum64(uc), sum64(wuc), hl);
    for h in hs {
        s += &format!(" {:016x}", h);
    }
    eprintln!("{}", s);
}

fn cpu_now() -> u64 {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(id: u32) -> u64;
    }
    unsafe { clock_gettime_nsec_np(16) }
}

/// FL_FRONTBENCH=reps: pass 1 (split + shape lookup) of each file as one block, `reps` times;
/// prints the best CPU time per file and in total.
pub fn bench(files: &[String]) {
    let reps: usize = crate::engine::fl::no_env::var("FL_FRONTBENCH").ok().and_then(|x| x.parse().ok()).unwrap_or(3);
    let cfg = SplitCfg::get();
    let (mut tot_b, mut tot_lines_t, mut tot_split_t, mut tot_p1_t) = (0usize, 0f64, 0f64, 0f64);
    for f in files {
        let data = std::fs::read(f).unwrap();
        let (mut tl, mut ts, mut tp) = (f64::MAX, f64::MAX, f64::MAX);
        let mut lines: Vec<&[u8]> = Vec::new();
        let mut chk = 0u64;
        for _ in 0..reps {
            let t = cpu_now();
            let body = data.strip_suffix(b"\n").unwrap_or(&data);
            lines = if env_on("FL_OLDLINES") { body.split(|&c| c == b'\n').collect() } else { lines_of(body) };
            tl = tl.min((cpu_now() - t) as f64 * 1e-9);
            if env_on("FL_CHECK") {
                let ref_lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
                assert!(ref_lines.len() == lines.len() && ref_lines.iter().zip(&lines).all(|(a, b)| a.as_ptr() == b.as_ptr() && a.len() == b.len()), "lines differ");
            }
            let t = cpu_now();
            let mut x = 0usize;
            if env_on("FL_SPLIT1") {
                let mut sp = split::Splitter::default();
                for &l in &lines {
                    sp.split(&data, l.as_ptr() as usize - data.as_ptr() as usize, l.len(), &cfg);
                    x += sp.shape_len + sp.nvals;
                }
            } else {
                let mut f = Front::default();
                let mut out: Vec<Val> = Vec::with_capacity(1 << 16);
                for &l in &lines {
                    out.clear();
                    x += f.split(&data, l.as_ptr() as usize - data.as_ptr() as usize, l.len(), &cfg, &mut out);
                    x += out.len();
                }
            }
            ts = ts.min((cpu_now() - t) as f64 * 1e-9);
            chk = chk.wrapping_add(x as u64);
            let t = cpu_now();
            let p = pass1_seq(&data, &lines, &cfg);
            tp = tp.min((cpu_now() - t) as f64 * 1e-9);
            chk = chk.wrapping_add(p.rid.len() as u64 + p.vals.len() as u64);
        }
        let n = data.len() as f64 * 1e-6;
        println!("{:<24} lines {:6.0} MB/s | split only {:6.0} MB/s | pass1 {:6.0} MB/s | {} lines ({})", std::path::Path::new(f).file_name().unwrap().to_string_lossy(), n / tl, n / ts, n / tp, lines.len(), chk);
        tot_b += data.len();
        tot_lines_t += tl;
        tot_split_t += ts;
        tot_p1_t += tp;
    }
    let n = tot_b as f64 * 1e-6;
    println!("FRONT TOTAL {:.1} MB | lines {:.0} MB/s | split only {:.0} MB/s | pass1 {:.0} MB/s", n, n / tot_lines_t, n / tot_split_t, n / tot_p1_t);
}

/// FL_FUZZ=lines: random lines of tricky tokens, each split by Front and by split_line (the
/// reference) and compared; exits 3 at the first difference.
pub fn fuzz() {
    let total: usize = crate::engine::fl::no_env::var("FL_FUZZ").ok().and_then(|x| x.parse().ok()).unwrap_or(1_000_000);
    let seed: u64 = crate::engine::fl::no_env::var("FL_SEED").ok().and_then(|x| x.parse().ok()).unwrap_or(1);
    let mut st = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rnd = move |n: u64| -> u64 {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st % n.max(1)
    };
    let toks: &[&[u8]] = &[
        b"Jan", b"January", b"Mon", b"Monday", b"Junk", b"[Jan", b"(Monday", b"Mayday", b"May", b"Sept", b"Sep", b"Dec,", b"-Fri", b"Sat]", b"Thu1", b"Wednesday", b"Tue",
        b"0", b"00", b"007", b"255", b"256", b"1.2.3.4", b"10.0.0.1", b"1.2.3.4.5", b"01.2.3.4", b"1.2.3", b"300.1.1.1", b"1.2.3.4:80", b"999.999.999.999", b"1.2.3.04",
        b"12:34:56", b"1:23:45", b"12:34:56.789", b"12:34:56,123456", b"12:34:56.1234567", b"12:60:00", b"23:59:59:", b"1:2:3:4", b"22:15:29:606", b"0:0:0:190", b"1:02:3:4", b"12:34",
        b"12.34.56", b"15.42.50.675872", b"1.23.45.6", b"12.34.56.1234567", b"12.34.56.78.9", b"2005.06.03",
        b"deadbeef", b"DEADBEEF", b"DeadBeef", b"deadbeef1", b"0xdeadbeef12", b"abcdef0123456789", b"ABCDEF0123456789A", b"abc123", b"a1b2c3d4", b"12345678ab", b"113d3a99c3da401fbd62cc2caa5b96d2",
        b"38101a0b-2096-447d-96ea-a692162415ae", b"38101A0B-2096-447D-96EA-A692162415AE", b"38101a0b-2096-447D-96ea-a692162415ae", b"req-38101a0b-2096-447d-96ea-a692162415ae", b"38101a0b-2096-447d-96ea-a69216241", b"12345678-1234-1234-1234-123456789012",
        b"blk_-1608999687919862906", b"blk_38865049064139660", b"-1608999687919862906", b"x-1608999687919862906", b"12345678901234567890", b"1234567890123456789012345", b"0123456789012345",
        b"-", b"--", b"=", b"/", b":", b".", b",", b"(", b")", b"[", b"]", b"\t", b"\xc3\xa9", b"\xff", b"\x00", b"a", b"Z", b"word", b"INFO", b"x", b"_", b"e5", b"E5",
    ];
    let cfg = SplitCfg::get();
    let mut f = Front::default();
    let (mut shape, mut cvals): (Vec<u8>, Vec<Val>) = (Vec::new(), Vec::new());
    let mut out: Vec<Val> = Vec::new();
    let mut done = 0usize;
    while done < total {
        // a block of lines
        let mut data: Vec<u8> = Vec::new();
        let nl = if rnd(8) == 0 { 1 } else { 1 + rnd(200) as usize };
        for _ in 0..nl {
            let ntok = rnd(40) as usize;
            for t in 0..ntok {
                if t > 0 || rnd(4) == 0 {
                    let sep: &[u8] = match rnd(10) {
                        0 => b"",
                        1 => b"  ",
                        2 => b"-",
                        3 => b":",
                        4 => b".",
                        5 => b"=",
                        _ => b" ",
                    };
                    data.extend_from_slice(sep);
                }
                match rnd(8) {
                    0 => {
                        // random digits / hex
                        let len = 1 + rnd(30) as usize;
                        let alpha: &[u8] = if rnd(2) == 0 { b"0123456789" } else { b"0123456789abcdefABCDEF-" };
                        for _ in 0..len {
                            data.push(alpha[rnd(alpha.len() as u64) as usize]);
                        }
                    }
                    1 => {
                        // random bytes
                        let len = 1 + rnd(6) as usize;
                        for _ in 0..len {
                            let c = rnd(256) as u8;
                            data.push(if c == b'\n' { b'?' } else { c });
                        }
                    }
                    _ => data.extend_from_slice(toks[rnd(toks.len() as u64) as usize]),
                }
            }
            data.push(b'\n');
        }
        if rnd(2) == 0 {
            data.pop(); // sometimes no '\n' at the end
        }
        let body = data.strip_suffix(b"\n").unwrap_or(&data);
        let lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
        let fast = lines_of(body);
        if fast.len() != lines.len() || fast.iter().zip(&lines).any(|(a, b)| a.as_ptr() != b.as_ptr() || a.len() != b.len()) {
            eprintln!("FUZZ LINES DIFFER (seed {})", seed);
            std::process::exit(3);
        }
        for &l in &lines {
            out.clear();
            let at = l.as_ptr() as usize - data.as_ptr() as usize;
            let sl = f.split(&data, at, l.len(), &cfg, &mut out);
            shape.clear();
            cvals.clear();
            split_line(l, &cfg, &mut shape, &mut cvals);
            if shape[..] != f.shape[..sl] || cvals[..] != out[..] {
                eprintln!("FUZZ DIFFERS (seed {} after {} lines): line {:?}\n old {:?} {:?}\n new {:?} {:?}", seed, done, String::from_utf8_lossy(l), String::from_utf8_lossy(&shape), cvals, String::from_utf8_lossy(&f.shape[..sl]), out);
                std::process::exit(3);
            }
            done += 1;
        }
    }
    println!("FUZZ OK {} lines (seed {})", done, seed);
}

/// FL_FUZZ2=lines: pass 1 (with its fast path) on blocks of random lines, many of them made
/// from an earlier line by changing its digits (same classes, other values: IPv4 parts over
/// 255, leading zeros, clock times out of range, long IDs, pads...); FL_CHECK's comparison of
/// every line with split_line; exits 3 at the first difference.
pub fn fuzz2() {
    let total: usize = crate::engine::fl::no_env::var("FL_FUZZ2").ok().and_then(|x| x.parse().ok()).unwrap_or(1_000_000);
    let seed: u64 = crate::engine::fl::no_env::var("FL_SEED").ok().and_then(|x| x.parse().ok()).unwrap_or(1);
    let mut st = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rnd = move |n: u64| -> u64 {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st % n.max(1)
    };
    let toks: &[&[u8]] = &[
        b"Jan", b"January", b"Mon", b"Monday", b"Junk", b"[Jan", b"(Monday", b"Mayday", b"May", b"Sept", b"Dec,", b"-Fri", b"Sat]", b"Thu1",
        b"0", b"00", b"007", b"255", b"256", b"1.2.3.4", b"10.0.0.1", b"1.2.3.4.5", b"01.2.3.4", b"1.2.3", b"300.1.1.1", b"1.2.3.4:80", b"192.168.100.200",
        b"12:34:56", b"1:23:45", b"12:34:56.789", b"12:34:56,123456", b"12:34:56.1234567", b"12:60:00", b"23:59:59:", b"1:2:3:4", b"22:15:29:606", b"0:0:0:190", b"12:34",
        b"12.34.56", b"15.42.50.675872", b"1.23.45.6", b"12.34.56.1234567", b"2005.06.03", b"2005-06-03-15.42.50.675872", b"R02-M1-N0-C:J12-U11",
        b"deadbeef", b"abc123", b"x12", b"blk_-1608999687919862906", b"blk_38865049064139660", b"-1608999687919862906", b" -9223372036854775807", b"18446744073709551615",
        b"12345678901234567890", b"1234567890123456789012345", b"0123456789012345", b"  12", b"   7", b"  0", b"  1702  2395", b"a-1-2-3-4", b"--", b"-",
        b"=", b"/", b":", b".", b",", b"(", b")", b"[", b"]", b"\t", b"\xc3\xa9", b"a", b"Z", b"word", b"INFO", b"x", b"_", b"G", b"q",
    ];
    let mut cfg = SplitCfg::get();
    if crate::engine::fl::no_env::var_os("FL_FUZZ2_NONAMES").is_some() {
        cfg.names = false;
    }
    let mut done = 0usize;
    while done < total {
        // a block: some base lines, then lines made from them (digits changed), mixed
        let nb = 1 + rnd(12) as usize;
        let mut bases: Vec<Vec<u8>> = Vec::new();
        for _ in 0..nb {
            let mut b: Vec<u8> = Vec::new();
            let ntok = 1 + rnd(24) as usize;
            for t in 0..ntok {
                if t > 0 || rnd(4) == 0 {
                    let sep: &[u8] = match rnd(10) {
                        0 => b"",
                        1 => b"  ",
                        2 => b"-",
                        3 => b":",
                        4 => b".",
                        5 => b"   ",
                        _ => b" ",
                    };
                    b.extend_from_slice(sep);
                }
                if rnd(5) == 0 {
                    let len = 1 + rnd(22) as usize;
                    for _ in 0..len {
                        b.push(b'0' + rnd(10) as u8);
                    }
                } else {
                    b.extend_from_slice(toks[rnd(toks.len() as u64) as usize]);
                }
            }
            bases.push(b);
        }
        let mut data: Vec<u8> = Vec::new();
        let big = rnd(16) == 0;
        let nl = 1 + rnd(if big { 30000 } else { 3000 }) as usize;
        for _ in 0..nl {
            let mut l = bases[rnd(nb as u64) as usize].clone();
            match rnd(10) {
                0 => {}
                1 => {
                    // one byte changed
                    if !l.is_empty() {
                        let k = rnd(l.len() as u64) as usize;
                        l[k] = toks[rnd(toks.len() as u64) as usize].first().copied().unwrap_or(b'x');
                    }
                }
                _ => {
                    // digits changed (each with some chance; a leading zero or a 9 now and then)
                    let pz = 1 + rnd(4);
                    for c in l.iter_mut() {
                        if c.is_ascii_digit() && rnd(pz) == 0 {
                            *c = match rnd(8) {
                                0 => b'0',
                                1 => b'9',
                                _ => b'0' + rnd(10) as u8,
                            };
                        }
                    }
                }
            }
            if l.contains(&b'\n') {
                continue;
            }
            data.extend_from_slice(&l);
            data.push(b'\n');
        }
        if rnd(2) == 0 {
            data.pop();
        }
        // room after the data: most lines take the fast path (it needs ROOM bytes after a line)
        let body_len = data.strip_suffix(b"\n").unwrap_or(&data).len();
        let pad = if rnd(4) == 0 { 0 } else { ROOM + 16 };
        data.extend(std::iter::repeat_n(b'z', pad));
        let body = &data[..body_len];
        let lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
        let p = pass1_fast_at(&data, &lines, &cfg, true);
        assert_eq!(p.rid.len(), lines.len());
        done += lines.len();
    }
    println!("FUZZ2 OK {} lines (seed {}, names {})", done, seed, cfg.names);
}

/// FL_FUZZ3=lines: pass 1 on blocks of twins whose digits change at the edges of the value kinds
/// (a leading 0, 2^63 for long IDs, octet 255/256, minute and second 59/60, H:M:S:F limits, pads
/// of space-padded numbers, runs next to hex letters), checked line by line against split_line
/// as FL_FUZZ2 does; exits 3 at the first difference.
pub fn fuzz3() {
    let total: usize = crate::engine::fl::no_env::var("FL_FUZZ3").ok().and_then(|x| x.parse().ok()).unwrap_or(1_000_000);
    let seed: u64 = crate::engine::fl::no_env::var("FL_SEED").ok().and_then(|x| x.parse().ok()).unwrap_or(1);
    let mut st = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rnd = move |n: u64| -> u64 {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st % n.max(1)
    };
    let toks: &[&[u8]] = &[
        b"10.0.0.255", b"192.168.1.1", b"1.2.3.4:8080", b"/10.251.73.220:50010", b"255.255.255.255", b"1.2.3.4.5", b"v1.2.3",
        b"12:34:56", b"1:23:45", b"12:59:59.999999", b"00:00:00,000", b"23:59:59:999", b"22:15:29:606", b"0:0:0:190", b"9:59:59:99",
        b"12.34.56", b"15.42.50.675872", b"2005-06-03-15.42.50.675872", b"12.34.56.789", b"1.23.45",
        b"blk_-1608999687919862906", b"blk_9223372036854775807", b"-9223372036854775808", b"123456789012345", b"1234567890123456789",
        b"0x1f2e", b"ab12cd", b"12ab", b"A1B2", b"  12", b"   7", b"  0", b"  1702  2395", b"(  42)", b"=  3",
        b"Jan", b"Mon", b"[Dec", b"2015-10-05", b"20171223-22:15:29:606", b"081109", b"203518", b"00042", b"0", b"007",
        b"=", b"/", b":", b".", b",", b"-", b"[", b"]", b"x", b"INFO", b"G",
    ];
    // edge values of a run of w digits
    let edge = |w: usize, k: u64| -> Vec<u8> {
        let mut v: Vec<u8> = match (w, k % 8) {
            (1, _) => vec![[b'0', b'9', b'1', b'5'][(k / 8 % 4) as usize]],
            (2, 0) => b"59".to_vec(),
            (2, 1) => b"60".to_vec(),
            (2, 2) => b"00".to_vec(),
            (2, 3) => b"09".to_vec(),
            (2, 4) => b"99".to_vec(),
            (3, 0) => b"255".to_vec(),
            (3, 1) => b"256".to_vec(),
            (3, 2) => b"000".to_vec(),
            (3, 3) => b"099".to_vec(),
            (3, 4) => b"999".to_vec(),
            (19, 0) => b"9223372036854775807".to_vec(),
            (19, 1) => b"9223372036854775808".to_vec(),
            (19, 2) => b"0223372036854775807".to_vec(),
            (19, 3) => b"9999999999999999999".to_vec(),
            (_, 5) => std::iter::once(b'0').chain(std::iter::repeat_n(b'9', w - 1)).collect(),
            (_, 6) => std::iter::once(b'1').chain(std::iter::repeat_n(b'0', w - 1)).collect(),
            _ => std::iter::repeat_n(b'9', w).collect(),
        };
        v.truncate(w);
        v
    };
    let mut cfg = SplitCfg::get();
    if crate::engine::fl::no_env::var_os("FL_FUZZ2_NONAMES").is_some() {
        cfg.names = false;
    }
    let mut done = 0usize;
    while done < total {
        let nb = 1 + rnd(8) as usize;
        let mut bases: Vec<Vec<u8>> = Vec::new();
        for _ in 0..nb {
            let mut b: Vec<u8> = Vec::new();
            let ntok = 1 + rnd(16) as usize;
            for t in 0..ntok {
                if t > 0 || rnd(4) == 0 {
                    let sep: &[u8] = match rnd(8) {
                        0 => b"",
                        1 => b"  ",
                        2 => b"-",
                        3 => b":",
                        4 => b".",
                        _ => b" ",
                    };
                    b.extend_from_slice(sep);
                }
                b.extend_from_slice(toks[rnd(toks.len() as u64) as usize]);
            }
            bases.push(b);
        }
        let mut data: Vec<u8> = Vec::new();
        let nl = 1 + rnd(4000) as usize;
        for _ in 0..nl {
            let mut l = bases[rnd(nb as u64) as usize].clone();
            if rnd(8) != 0 {
                // each digit run: kept, or set to an edge value of its width, or one digit changed
                let mut p = 0;
                while p < l.len() {
                    if !l[p].is_ascii_digit() {
                        p += 1;
                        continue;
                    }
                    let mut e = p;
                    while e < l.len() && l[e].is_ascii_digit() {
                        e += 1;
                    }
                    match rnd(4) {
                        0 => {}
                        1 => {
                            let k = p + rnd((e - p) as u64) as usize;
                            l[k] = b'0' + rnd(10) as u8;
                        }
                        _ => {
                            let v = edge(e - p, rnd(64));
                            l[p..e].copy_from_slice(&v);
                        }
                    }
                    p = e;
                }
            }
            data.extend_from_slice(&l);
            data.push(b'\n');
        }
        if rnd(2) == 0 {
            data.pop();
        }
        let body_len = data.strip_suffix(b"\n").unwrap_or(&data).len();
        let pad = if rnd(4) == 0 { 0 } else { ROOM + 16 };
        data.extend(std::iter::repeat_n(b'z', pad));
        let body = &data[..body_len];
        let lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
        let p = pass1_fast_at(&data, &lines, &cfg, true);
        assert_eq!(p.rid.len(), lines.len());
        done += lines.len();
    }
    println!("FUZZ3 OK {} lines (seed {}, names {})", done, seed, cfg.names);
}

/// Kind flags for pass 2: a number written the plain way (not text, hex text, a long ID or a
/// whole token), of kind class 0 (kclass).
const K_FAST: u8 = 1;
const K_CLASS0: u8 = 2;
static KFLAGS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut k = 0;
    while k < 256 {
        let kind = k as u8;
        let fast = kind != 1 && !(kind >= 100 && kind <= 103) && kind != LONG_ID && kind != TOKK;
        let class0 = kind <= 22 || (kind >= SPN && kind <= SPN + SPN_MAX);
        t[k] = (fast as u8) * K_FAST | (class0 as u8) * K_CLASS0;
        k += 1;
    }
    t
};

/// A base column's write cursors in pass 2, in 32 bytes (per value one load pair and one load,
/// one store pair and one store; two columns' cursors share a cache line): the places of its next
/// kind, number and slot in the buffers reserved for all its values. Bit 0 of `v` (its places are
/// 8-byte aligned) says the column has a list of long-ID places, so each of its values takes the
/// slow way: the number place is `vp()`, never `v` itself. pass2_tok's array of them starts with
/// HOT_PAD records that hold the kind flags (plain_run reads both from one base register).
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Hot {
    k: *mut u8,
    v: *mut u64,
    s: *mut u32,
}
impl Hot {
    /// the column's next number place
    #[inline(always)]
    fn vp(&self) -> *mut u64 {
        self.v.map_addr(|a| a & !1)
    }
    /// the column has a list of long-ID places
    #[inline(always)]
    fn ids(&self) -> bool {
        self.v.addr() & 1 != 0
    }
}
const _: () = assert!(std::mem::size_of::<Hot>() == 32);
/// Records in front of pass 2's cursors that hold the 256 kind flags (KFLAGS)
const HOT_PAD: usize = 8;
const _: () = assert!(HOT_PAD * std::mem::size_of::<Hot>() == 256);

/// The values k..n of a line whose slots are ps[..n] (the plan of its template: column and slot
/// number of each) into their base columns, until one needs the slow way (text, hex text, a long
/// ID, a column with long IDs). Returns its place (n: none) and l0, the last number of kind
/// class 0 (kind_last[0], kept in a register). A leaf function (not inlined): the loop has the
/// scratch registers to itself, and one copy serves every caller.
/// SAFETY: k <= n; ps and lv hold n entries; every ps[..n].col is a base column (an index into
/// hot); each column's buffers have room for all the values it gets (reserved by pass2_tok).
#[inline(never)]
unsafe fn line_vals<const KS: bool>(ps: *const PlanSlot, lv: *const Val, n: usize, k: usize, hot: *mut Hot, kl: *mut u64, mut l0: u64) -> (usize, u64) {
    unsafe {
        let (mut p, mut q, pe) = (ps.add(k), lv.add(k), ps.add(n));
        while p < pe {
            let PlanSlot { col, slot, .. } = *p;
            let (kind, v, _, _) = *q;
            let fk = *KFLAGS.get_unchecked(kind as usize);
            let h = hot.add(col as usize);
            let Hot { k: kp, v: vp, s: sp } = *h;
            if fk & K_FAST == 0 || vp.addr() & 1 != 0 {
                break;
            }
            // (the cursors move on first: their adds fold into the stores)
            *kp = kind;
            *vp = v;
            *h = Hot { k: kp.add(1), v: vp.add(1), s: sp.add(1) };
            let same = if fk & K_CLASS0 != 0 {
                let same = l0 == v;
                l0 = v;
                same
            } else {
                let x = kl.add(kind as usize);
                let same = *x == v;
                *x = v;
                same
            };
            *sp = slot | ((same & KS) as u32) << 31;
            p = p.add(1);
            q = q.add(1);
        }
        (p.offset_from(ps) as usize, l0)
    }
}

/// The next slot of a plain line (its column and slot number) and its value (kind, number); p
/// and q move on to the next. On aarch64 both moves are folded into the loads (post-index).
/// SAFETY: p and q point at a slot and a value.
#[inline(always)]
unsafe fn next_val(p: &mut *const PlanSlot, q: &mut *const Val) -> (usize, u32, usize, u64) {
    #[cfg(target_arch = "aarch64")]
    {
        // (the layouts the loads below read; checked when compiling)
        const _: () = assert!(std::mem::offset_of!(PlanSlot, col) == 0 && std::mem::offset_of!(PlanSlot, slot) == 4 && std::mem::size_of::<PlanSlot>() == 12);
        const _: () = assert!(std::mem::offset_of!(Val, 1) == 0 && std::mem::offset_of!(Val, 0) == 12 && std::mem::size_of::<Val>() == 24);
        // (col and kind are loaded into 32-bit registers, which clears their upper halves: they
        // come out as usize with no extension)
        let (col, slot, kind, v): (usize, u32, usize, u64);
        // SAFETY: reads the 12-byte slot at p and the 24-byte value at q (the caller's condition)
        unsafe {
            std::arch::asm!(
                "ldp {c:w}, {s:w}, [{p}], #12",
                "ldrb {k:w}, [{q}, #12]",
                "ldr {v}, [{q}], #24",
                p = inout(reg) *p, q = inout(reg) *q, c = out(reg) col, s = out(reg) slot, k = out(reg) kind, v = out(reg) v,
                options(nostack, readonly, preserves_flags)
            );
        }
        (col, slot, kind, v)
    }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe {
        let PlanSlot { col, slot, .. } = **p;
        let (kind, v, _, _) = **q;
        *p = p.add(1);
        *q = q.add(1);
        (col as usize, slot, kind as usize, v)
    }
}

/// A long ID, or another number of a column with long IDs, into base column c through its
/// cursors h: what put_value_slow does for these (its kind, its place in the column's long-ID
/// list, the number and its slot word), with fewer arguments. Returns l0 (kind_last[0], the
/// last number of kind class 0, which the caller keeps in a register).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn put_id_val<const KS: bool>(c: &mut Col, h: &mut Hot, kind: u8, v: u64, slot: u32, mut l0: u64, kind_last: &mut [u64; 256], id_map: &mut IdMap, ci: u32) -> u64 {
    // SAFETY (the stores through h): reserved for every value of the column
    unsafe {
        *h.k = kind;
        h.k = h.k.add(1);
    }
    if kind == LONG_ID {
        // 0: a new long ID (added to the list); d + 1: seen, d places from the newest
        if c.ids.is_empty() {
            // (the column's numbers so far get no place; room for all it can get)
            let nv = unsafe { h.vp().offset_from(c.vals.as_ptr()) as usize };
            c.ids.reserve(c.vals.capacity());
            c.ids.resize(nv, u32::MAX);
        }
        let n = id_map.map.len() as u32;
        let e = *id_map.map.entry(v).or_insert(n);
        c.ids.push(if e == n { 0 } else { n - e });
        if e == n {
            // (a new one: an ID column may write it as a generator step, ids.rs; a repeat keeps
            // its cache code or list distance either way, so id_plan and id_apply skip it)
            id_map.seq.push(IdVal { col: ci, kind, v: v as u128, pos: c.ids.len() as u32 - 1, npos: 0, sl: slot });
        }
        h.v = h.v.map_addr(|a| a | 1);
    } else {
        // (a column with long IDs: its list is not empty)
        c.ids.push(u32::MAX);
    }
    let kc = kclass(kind);
    let x = if kc == 0 { &mut l0 } else { &mut kind_last[kc] };
    let ks = *x == v && KS;
    *x = v;
    unsafe {
        *h.vp() = v;
        *h.s = slot | (ks as u32) << 31;
        h.v = h.v.add(1);
        h.s = h.s.add(1);
    }
    l0
}

/// A column's cursors at hc + 256 (hc: the base of the cursor array with its HOT_PAD records in
/// front, plus col * 32), in two loads (aarch64: one load pair and one load, the 256 folded into
/// their offsets).
/// SAFETY: hc + 256 points at a Hot.
#[inline(always)]
unsafe fn load_hot_pad(hc: *const u8) -> Hot {
    #[cfg(target_arch = "aarch64")]
    {
        const _: () = assert!(HOT_PAD * 32 == 256 && std::mem::offset_of!(Hot, k) == 0 && std::mem::offset_of!(Hot, v) == 8 && std::mem::offset_of!(Hot, s) == 16);
        let (k, v, s): (*mut u8, *mut u64, *mut u32);
        // SAFETY: reads the 24 bytes of cursors at hc + 256 (the caller's condition)
        unsafe {
            std::arch::asm!(
                "ldp {k}, {v}, [{h}, #256]",
                "ldr {s}, [{h}, #272]",
                h = in(reg) hc, k = out(reg) k, v = out(reg) v, s = out(reg) s,
                options(nostack, readonly, preserves_flags)
            );
        }
        Hot { k, v, s }
    }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe {
        *hc.add(HOT_PAD * 32).cast::<Hot>()
    }
}

/// A pass-1 shape's line on pass 2's plain way (no whole tokens; not a grouped block): its
/// template's plan slots (plan.slots[c0..c0 + nc]) and its template id as written for it (tv:
/// the id's varint bytes, little end first, tl of them; TidModel: tv is the id). `sp` 0: a
/// value per slot; 1: also its template's rules, rules[a..b]; 2: planned words (none in a
/// splitting column, no rules), words[a..b] (slot, word number), and a value per other slot.
/// nc == u32::MAX: the general way. (32 bytes: one address shift.)
#[derive(Clone, Copy)]
struct FastLine {
    c0: u32,
    nc: u32,
    tv: u32,
    tl: u32,
    sp: u32,
    a: u32,
    b: u32,
    nv: u32,
}

/// What a plain line with rules writes besides its columns (pass2_tok's RuleLists): the
/// answers (rule << 1 | not given), the near rules' differences, the rule sources (the line's
/// values so far) and the values given per prefix column.
struct RuleOut<'r> {
    rules: &'r [super::rules::Rule],
    flag_list: &'r mut Vec<u32>,
    near_list: &'r mut Vec<i64>,
    xs: &'r mut [u64],
    rminus: &'r mut [u32],
}

/// The values k..lim of a plain line (ps, lv) into their columns: line_vals, and a value that
/// needs more through put_value_slow (kind_last[0] kept in step with l0). Returns lim, or k
/// when k >= lim (nothing written: as the old loop's `while k < lim`).
/// SAFETY: as line_vals (lim <= ps.len() <= lv.len())
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn vals_to<const KS: bool>(ps: &[PlanSlot], lv: &[Val], mut k: usize, lim: usize, li: usize, l: &[u8], cols: &mut [Col], hot: &mut [Hot], l0: &mut u64, kind_last: &mut [u64; 256], id_map: &mut IdMap, clock_vals: &mut Vec<(u32, u32, u64, i64)>) -> usize {
    if k >= lim {
        return k;
    }
    let (ps, lv) = (&ps[..lim], &lv[..lim]);
    while k < lim {
        // SAFETY: k < lim; ps and lv hold lim entries (the caller's conditions)
        (k, *l0) = unsafe { line_vals::<KS>(ps.as_ptr(), lv.as_ptr(), lim, k, hot.as_mut_ptr(), kind_last.as_mut_ptr(), *l0) };
        if k == lim {
            break;
        }
        let (kind, v, s, e) = lv[k];
        kind_last[0] = *l0;
        put_value_slow(cols, hot, true, ps[k].col, ps[k].slot, li, l, kind, v, s, e, KS, false, kind_last, id_map, clock_vals);
        *l0 = kind_last[0];
        k += 1;
    }
    lim
}

/// A plain line whose template has rules (rules[r0..r1], in slot order): the fast way between
/// the rules' slots, and at each rule's slot one yes/no (when the rule gives the value it is not
/// stored); the same writes as pass2_tok's general way makes for it.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn rule_line<const KS: bool>(ps: &[PlanSlot], lv: &[Val], r0: usize, r1: usize, li: usize, l: &[u8], cols: &mut [Col], hot: &mut [Hot], l0: u64, kind_last: &mut [u64; 256], id_map: &mut IdMap, clock_vals: &mut Vec<(u32, u32, u64, i64)>, ro: &mut RuleOut) -> u64 {
    let (mut l0v, l0) = (l0, &mut 0u64);
    *l0 = l0v;
    let n = ps.len();
    let lv = &lv[..n];
    let (mut k, mut ri) = (0usize, r0);
    loop {
        let lim = if ri < r1 { ro.rules[ri].slot as usize + 1 } else { n };
        if ri < r1 {
            let (r, t) = (&ro.rules[ri], lim - 1);
            // SAFETY: t < n = ps.len() = lv.len()
            k = unsafe { vals_to::<KS>(ps, lv, k, t, li, l, cols, hot, l0, kind_last, id_map, clock_vals) };
            for i in 0..3 {
                if i == 0 || r.sgn[i] != 0 {
                    let q = r.src[i] as usize;
                    ro.xs[q] = super::rules::src_val(lv[q].0, lv[q].1);
                }
            }
            let (kind, v, _, _) = lv[t];
            let given = super::rules::applies(r, ro.xs, kind, v);
            ro.flag_list.push((ri as u32) << 1 | given.is_none() as u32);
            ri += 1;
            if let Some(d) = given {
                if r.op == super::rules::NEAR {
                    ro.near_list.push(d);
                }
                // still the last number of its kind (as without the rule)
                let kc = kclass(kind);
                if kc == 0 {
                    *l0 = v;
                } else {
                    kind_last[kc] = v;
                }
                ro.rminus[ps[t].col as usize] += 1;
                k = lim;
                if k >= n {
                    break;
                }
                continue;
            }
        }
        // the slots up to lim (the rule's slot when it did not give the value)
        // SAFETY: lim <= n
        k = unsafe { vals_to::<KS>(ps, lv, k, lim, li, l, cols, hot, l0, kind_last, id_map, clock_vals) };
        if k >= n {
            break;
        }
    }
    l0v = *l0;
    l0v
}

/// A plain line with planned words (words: each one's slot, in slot order, and its number in
/// the word table; none in a splitting column) and no rules: the values between the words the
/// fast way, each word as the general way writes it (the k-th recent word of its column, or its
/// number). Returns l0.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn word_line<const KS: bool>(ps: &[PlanSlot], lv: &[Val], words: &[(u32, u32)], li: usize, l: &[u8], cols: &mut [Col], hot: &mut [Hot], l0: u64, kind_last: &mut [u64; 256], id_map: &mut IdMap, clock_vals: &mut Vec<(u32, u32, u64, i64)>) -> u64 {
    let mut l0v = l0;
    let (mut k, mut j) = (0usize, 0usize);
    for &(kw, id) in words {
        let kw = kw as usize;
        // the values of slots k..kw
        let n = kw - k;
        // SAFETY: n entries of each sub-slice (bounds-checked slices)
        unsafe { vals_to::<KS>(&ps[k..kw], &lv[j..j + n], 0, n, li, l, cols, hot, &mut l0v, kind_last, id_map, clock_vals) };
        j += n;
        // the word at slot kw: the k-th recent word of the column, or its number in the word table
        let PlanSlot { col, slot, .. } = ps[kw];
        let c = &mut cols[col as usize];
        match c.wrecent.find(id) {
            Some(x) => {
                push_kind(c, hot, true, col, WORD + 1 + x as u8);
                c.wrecent.hit(x);
            }
            None => {
                push_kind(c, hot, true, col, WORD);
                if !c.ids.is_empty() {
                    c.ids.push(u32::MAX);
                }
                let v = id as u64;
                let kc = kclass(WORD);
                let ks = kind_last[kc] == v && KS;
                kind_last[kc] = v;
                push_num(c, hot, true, col, v, slot | (ks as u32) << 31);
                c.wrecent.push(id);
            }
        }
        k = kw + 1;
    }
    let n = ps.len() - k;
    // SAFETY: as above
    unsafe { vals_to::<KS>(&ps[k..], &lv[j..j + n], 0, n, li, l, cols, hot, &mut l0v, kind_last, id_map, clock_vals) };
    l0v
}

/// The keyed slots' memory work (entity.rs) of a plain line (no words, no rules) whose values
/// the plain way has written, in slot order: the same lookups, updates and entries as the
/// loop in pass2_tok (a keyed value's place is the last value of its column, as a pattern's
/// slots have columns of their own).
#[inline(never)]
fn keyed_walk(ps: &[PlanSlot], lv: &[Val], tid: usize, cols: &mut [Col], hot: &[Hot], kp: &mut KeyedPass) {
    let KeyedPass { inv, mem, reg, looks, .. } = kp;
    for &(k, d, key) in &inv[tid] {
        let (k, d) = (k as usize, d as usize);
        let (kind, v, _, _) = lv[k];
        if !super::entity::numeric(kind) {
            reg[k].0 = u8::MAX;
            continue;
        }
        if d != 0 && reg[k - d].0 != u8::MAX {
            let (kk, kv) = reg[k - d];
            let h = super::entity::key_hash(kk, kv, kind);
            let (i, held) = mem.find(h);
            let rk = if held { mem.rank(i, v) } else { super::entity::R };
            *looks += 1;
            if held {
                let ci = ps[k].col as usize;
                // SAFETY: the cursor lies in the column's values (a base column of a plain line)
                let at = unsafe { hot[ci].vp().offset_from(cols[ci].vals.as_ptr()) } as u32 - 1;
                cols[ci].ke.push((at, rk as u8, mem.last(i)));
            }
            mem.update(i, h, held, v, rk);
        }
        if key {
            reg[k] = (kind, v);
        }
    }
}

/// pass2_tok's lines li.. while they take the plain way (groupscatter: a function of its own,
/// so the loop state stays in registers): the same writes in the same order as the general way
/// makes for them. Returns the next line and the place of its first value.
/// tids: room for 5 bytes per line and 8 more was reserved by pass2_tok (not TidModel).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn plain_run<const KS: bool>(lines: &[&[u8]], rid: &[u32], vals: &[Val], fl: &[FastLine], slots: &[PlanSlot], words: &[(u32, u32)], mut li: usize, vi: usize, cols: &mut [Col], hot_all: &mut [Hot], kind_last: &mut [u64; 256], tidm: bool, tids: &mut Vec<u8>, tid_list: &mut Vec<u32>, id_map: &mut IdMap, clock_vals: &mut Vec<(u32, u32, u64, i64)>, ro: &mut RuleOut, mut kp: Option<&mut KeyedPass>) -> (usize, usize) {
    let mut l0 = kind_last[0];
    // (the tids' write place: the varint bytes of a line go in one 4-byte store)
    let (tb, mut tn) = (tids.as_mut_ptr(), tids.len());
    debug_assert!(tidm || tids.capacity() >= tn + 5 * (rid.len() - li) + 8);
    // SAFETY (the unchecked reads and pointers of this loop; pass2_tok checks each bound once a
    // block): every rid entry is below fl.len() (pass 1's shape numbers; pack_block's cnt1
    // counted them with checked indexes); lines.len() == rid.len(); the lines' value counts add
    // up to vals.len(), and a plain shape's nc is its value count, so a plain line's values
    // qv[..nc] lie in vals; its plan slots slots[c0..c0 + nc] lie in slots (pass2_tok checks
    // c0 + nc <= slots.len() for every plain shape)
    let mut qv = unsafe { vals.as_ptr().add(vi) };
    let s0 = slots.as_ptr();
    while li < rid.len() {
        let f = unsafe { *fl.get_unchecked(*rid.get_unchecked(li) as usize) };
        if f.nc == u32::MAX {
            break;
        }
        if tidm {
            tid_list.push(f.tv);
        } else {
            // SAFETY: room reserved (5 bytes per line and 8 more); tl <= 4 (to_le: the varint
            // bytes in their order on every target; no instruction on little-endian ones)
            unsafe { tb.add(tn).cast::<u32>().write_unaligned(f.tv.to_le()) };
            tn += f.tl as usize;
        }
        let nc = f.nc as usize;
        let mut p = unsafe { s0.add(f.c0 as usize) };
        if f.sp == 1 || f.sp == 2 {
            // a template with rules, or a line with words: one line at a time (nv values)
            let (ps, lv) = unsafe { (std::slice::from_raw_parts(p, nc), std::slice::from_raw_parts(qv, f.nv as usize)) };
            let l = unsafe { *lines.get_unchecked(li) };
            l0 = if f.sp == 1 {
                rule_line::<KS>(ps, lv, f.a as usize, f.b as usize, li, l, cols, &mut hot_all[HOT_PAD..], l0, kind_last, id_map, clock_vals, ro)
            } else {
                word_line::<KS>(ps, lv, &words[f.a as usize..f.b as usize], li, l, cols, &mut hot_all[HOT_PAD..], l0, kind_last, id_map, clock_vals)
            };
            qv = unsafe { qv.add(f.nv as usize) };
            li += 1;
            continue;
        }
        let mut q = qv;
        // SAFETY: p, q walk the line's plan slots and values (nc of each); a plain line's
        // columns are base columns (no words, so no columns made on the way), whose buffers were
        // reserved for all their values; hb (the base of hot_all: the kind flags at hb + kind, a
        // column's cursors at hb + 256 + col * 32) is taken again after each call that gets the
        // cursors (they move the same cursors). The loop is line_vals' (inlined here: the run
        // keeps its registers)
        unsafe {
            let pe = p.add(nc);
            let mut hb = hot_all.as_mut_ptr().cast::<u8>();
            while p < pe {
                let (col, slot, kind, v) = next_val(&mut p, &mut q);
                let fk = *hb.add(kind);
                let hc = hb.add(col << 5);
                let Hot { k: kp, v: vp, s: sp } = load_hot_pad(hc);
                if fk & K_FAST == 0 || vp.addr() & 1 != 0 {
                    if fk & K_FAST != 0 || kind == LONG_ID as usize {
                        // a long ID, or a number of a column with long IDs (HDFS block IDs)
                        l0 = put_id_val::<KS>(&mut cols[col], &mut hot_all[HOT_PAD + col], kind as u8, v, slot, l0, kind_last, id_map, col as u32);
                    } else {
                        // the slow way (text, hex text); the line's text is read only here
                        // (black_box: not hoisted out to every line)
                        let (_, _, s, e) = *q.sub(1);
                        let l = *lines.get_unchecked(std::hint::black_box(li));
                        kind_last[0] = l0;
                        put_value_slow(cols, &mut hot_all[HOT_PAD..], true, col as u32, slot, li, l, kind as u8, v, s, e, KS, false, kind_last, id_map, clock_vals);
                        l0 = kind_last[0];
                    }
                    hb = hot_all.as_mut_ptr().cast::<u8>();
                    continue;
                }
                // the slot word (its "same as the last number of its kind class" bit), then the
                // three stores and the cursors (their adds fold into the stores)
                let w = if fk & K_CLASS0 != 0 {
                    let w = slot | ((l0 == v && KS) as u32) << 31;
                    l0 = v;
                    w
                } else {
                    let x = kind_last.get_unchecked_mut(kind);
                    let w = slot | ((*x == v && KS) as u32) << 31;
                    *x = v;
                    w
                };
                *kp = kind as u8;
                *vp = v;
                *sp = w;
                *hc.add(HOT_PAD * 32).cast::<Hot>() = Hot { k: kp.add(1), v: vp.add(1), s: sp.add(1) };
            }
        }
        if f.sp == 3 {
            // a pattern with keyed slots (entity.rs; sp 3 only with a keyed pass): their memory
            // work after the line's values
            // SAFETY: the line's plan slots and values, nc of each (as above)
            let (ps, lv) = unsafe { (std::slice::from_raw_parts(s0.add(f.c0 as usize), nc), std::slice::from_raw_parts(qv, nc)) };
            keyed_walk(ps, lv, f.a as usize, cols, &hot_all[HOT_PAD..], kp.as_deref_mut().unwrap());
        }
        qv = q;
        li += 1;
    }
    if !tidm {
        // SAFETY: tn bytes were written (in the room reserved)
        unsafe { tids.set_len(tn) };
    }
    kind_last[0] = l0;
    (li, unsafe { qv.offset_from(vals.as_ptr()) as usize })
}

/// Pass 2 of pack_block, driven by the block's plan (main.rs `Plan`: per template, the column
/// and slot number of each slot): template ids, then each value into its column, with the same
/// results as the old loop (main.rs `pass2_old`; FL_SHADOW=pass2 compares them). Base columns
/// (cols[..cols.len()] on entry) are written through 32-byte cursors into buffers reserved up
/// front; columns made on the way (after a word of a splitting column) use the plain code.
/// Lines of plain shapes (no words, tokens or rules) take plain_run; the rest the general way.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub fn pass2_tok<'a>(lines: &[&'a [u8]], rid: &[u32], rnv: &[u32], cnt1: &[u32], vals: &[Val], final_of: &[u32], col_of: &[Vec<u32>], tplan: &Plan, pidx: &[u32], plan: &[Run], wid: &[u32], nslots: u32, cols: &mut Vec<Col>, tidm: bool, use_ks: bool, want_times: bool, tids: &mut Vec<u8>, tid_list: &mut Vec<u32>, kind_last: &mut [u64; 256], id_map: &mut IdMap, sub: &mut FxMap<(u32, u64), u32>, sub_slot: &mut FxMap<(u32, u32), u32>, clock_vals: &mut Vec<(u32, u32, u64, i64)>, tk: Option<&super::tokens::Tokens>, wmap: &mut FxMap<&'a [u8], u32>, words_out: &mut Vec<u8>, per_tmpl: &mut [u32], rl: &RuleLists, mut kp: Option<&mut KeyedPass>) {
    let nbase = cols.len();
    let RuleLists { rfirst, rules, flag_list, near_list, xs, rminus } = rl;
    let (mut flag_list, mut near_list, mut xs, mut rminus) = (flag_list.borrow_mut(), near_list.borrow_mut(), xs.borrow_mut(), rminus.borrow_mut());
    // values per base column: lines per template times its slots in the column (an upper
    // bound when splitting columns send some to columns of their own)
    let mut tcount = vec![0usize; col_of.len()];
    let smap = |r: u32| -> usize {
        match tk {
            Some(t) => t.smap[r as usize] as usize,
            None => r as usize,
        }
    };
    // (from the lines of each pass-1 shape, cnt1; and the bounds plain_run relies on: see there)
    let mut nv_all = 0usize;
    for (r0, &n) in cnt1.iter().enumerate() {
        tcount[final_of[smap(r0 as u32)] as usize] += n as usize;
        nv_all += n as usize * rnv[r0] as usize;
    }
    assert!(nv_all == vals.len() && lines.len() == rid.len() && cnt1.len() == rnv.len() && cnt1.iter().map(|&n| n as usize).sum::<usize>() == rid.len());
    let mut ccount = vec![0usize; nbase];
    for (t, cs) in col_of.iter().enumerate() {
        for &c in cs {
            ccount[c as usize] += tcount[t];
        }
    }
    // the cursors, after HOT_PAD records that hold the 256 kind flags (plain_run reads both from
    // one base register); every use but plain_run's takes hot[HOT_PAD..]
    let mut hot: Vec<Hot> = Vec::with_capacity(HOT_PAD + nbase);
    hot.resize(HOT_PAD, Hot { k: std::ptr::null_mut(), v: std::ptr::null_mut(), s: std::ptr::null_mut() });
    // SAFETY: the HOT_PAD records are 256 bytes (asserted), written as bytes and only read as bytes
    unsafe { std::ptr::copy_nonoverlapping(KFLAGS.as_ptr(), hot.as_mut_ptr().cast::<u8>(), 256) };
    for (c, &n) in cols.iter_mut().zip(&ccount) {
        c.kinds.reserve(n);
        c.vals.reserve(n);
        c.slots.reserve(n);
        // SAFETY: each place is the end of the column's values so far (len <= capacity)
        let (k, v, s) = unsafe { (c.kinds.as_mut_ptr().add(c.kinds.len()), c.vals.as_mut_ptr().add(c.vals.len()), c.slots.as_mut_ptr().add(c.slots.len())) };
        let v = if c.ids.is_empty() { v } else { v.map_addr(|a| a | 1) };
        hot.push(Hot { k, v, s });
    }
    // lines of each pattern (the old loop added them one line at a time)
    for (p, &n) in per_tmpl.iter_mut().zip(&tcount) {
        *p += n as u32;
    }
    // per merged shape: its template and words
    #[derive(Clone, Copy)]
    struct Shape {
        tid: u32,
        p0: u32,
        p1: u32,
    }
    let shapes: Vec<Shape> = (0..final_of.len()).map(|r| Shape { tid: final_of[r], p0: pidx[r], p1: pidx[r + 1] }).collect();
    // per pass-1 shape (as numbered in rid): the plain way's template and plan slots, or
    // nc = u32::MAX (a grouped block's lines all take the general way: plain_run keeps no times)
    // (the planned words of plain lines with words: slot, word number)
    let mut words: Vec<(u32, u32)> = Vec::new();
    let fl: Vec<FastLine> = (0..rnv.len()).map(|r0| {
        let (r, tok) = match tk {
            Some(t) => (t.smap[r0] as usize, t.tidx[r0] < t.tidx[r0 + 1]),
            None => (r0, false),
        };
        let sh = shapes[r];
        let t = sh.tid as usize;
        let (c0, nc) = (tplan.at[t], tplan.at[t + 1] - tplan.at[t]);
        let (p0, p1) = (sh.p0 as usize, sh.p1 as usize);
        let nw = (p1 - p0) as u32;
        let rules_t = rfirst[t] < rfirst[t + 1];
        // words: in slot order, below nc, none in a splitting column; and not with rules
        let w0 = words.len() as u32;
        let mut words_ok = nw == 0 || !rules_t;
        if nw > 0 && words_ok {
            let mut next = 0u32;
            for q in p0..p1 {
                let kw = plan[q].k0 as u32 + (q - p0) as u32;
                if kw < next || kw >= nc || col_of[t].get(kw as usize).is_none_or(|&c| cols[c as usize].split) {
                    words_ok = false;
                    break;
                }
                words.push((kw, wid[q]));
                next = kw + 1;
            }
            if !words_ok {
                words.truncate(w0 as usize);
            }
        }
        // (a plain line's id in at most 4 varint bytes: below 2^28)
        // (a pattern with keyed slots, entity.rs: its lines without words and rules stay on
        // the plain way, keyed_line doing their memory work; the others take the loop below)
        let keyed = kp.as_ref().is_some_and(|k| !k.inv[t].is_empty());
        let fast = !want_times && words_ok && !tok && !(keyed && (nw > 0 || rules_t)) && nw + rnv[r0] == nc && nc as usize == col_of[t].len() && sh.tid < 1 << 28 && (c0 + nc) as usize <= tplan.slots.len();
        let (mut tv, mut tl) = (sh.tid, 0u32);
        if !tidm {
            let mut b = [0u8; 10];
            // SAFETY: room for 10 bytes
            tl = unsafe { super::varint_at(b.as_mut_ptr(), sh.tid as u64).offset_from(b.as_ptr()) } as u32;
            tv = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let (sp, a, b) = if keyed { (3, t as u32, 0) } else if nw > 0 { (2, w0, words.len() as u32) } else if rules_t { (1, rfirst[t], rfirst[t + 1]) } else { (0, 0, 0) };
        FastLine { c0, nc: if fast { nc } else { u32::MAX }, tv, tl, sp, a, b, nv: rnv[r0] }
    }).collect();
    if !tidm {
        // the plain way writes the ids' varints without capacity checks (at most 5 bytes a line)
        tids.reserve(5 * lines.len() + 8);
    }
    if tidm {
        tid_list.reserve(lines.len());
    }
    let mut vi = 0usize;
    let mut last0 = kind_last[0]; // the last number of kind class 0 (kept out of memory)
    // whole tokens: the line's values with each token's values replaced by one (kind TOKK, its
    // number in the word table); the last word at each token place saves most lookups
    let mut lv2: Vec<Val> = Vec::new();
    let mut tlast: Vec<(&[u8], u32)> = vec![(&[][..], 0); tk.map_or(0, |t| t.toks.len())];
    let mut li = 0usize;
    while li < lines.len() {
        if fl[rid[li] as usize].nc != u32::MAX {
            // a run of lines on the plain way
            kind_last[0] = last0;
            let mut ro = RuleOut { rules, flag_list: &mut flag_list, near_list: &mut near_list, xs: &mut xs[..], rminus: &mut rminus[..] };
            (li, vi) = if use_ks {
                plain_run::<true>(lines, rid, vals, &fl, &tplan.slots, &words, li, vi, cols, &mut hot[..], kind_last, tidm, tids, tid_list, id_map, clock_vals, &mut ro, kp.as_deref_mut())
            } else {
                plain_run::<false>(lines, rid, vals, &fl, &tplan.slots, &words, li, vi, cols, &mut hot[..], kind_last, tidm, tids, tid_list, id_map, clock_vals, &mut ro, kp.as_deref_mut())
            };
            last0 = kind_last[0];
            continue;
        }
        let (cur, l) = (li, lines[li]);
        li += 1;
        let li = cur;
        let mut hot = &mut hot[HOT_PAD..]; // (the columns' cursors)
        let r0 = rid[li] as usize;
        let nvl = rnv[r0] as usize;
        let lv0 = &vals[vi..vi + nvl];
        vi += nvl;
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
        let sh = shapes[r];
        let tid = sh.tid;
        if tidm {
            tid_list.push(tid);
        } else {
            // (the same bytes as put_varint, no capacity check per byte)
            super::put_varints(tids, 5, |p| unsafe { super::varint_at(p, tid as u64) });
        }
        let cs = &col_of[tid as usize][..];
        let ps = tplan.tmpl(tid as usize); // (column and slot number of each slot)
        // the next planned word (its slot: values before it + words before it), the next value
        let (p0, p1) = (sh.p0 as usize, sh.p1 as usize);
        // this pattern's rules (in slot order): its lines take the loop with words
        let (mut ri, re) = (rfirst[tid as usize] as usize, rfirst[tid as usize + 1] as usize);
        // a pattern with keyed slots (entity.rs): its lines take the loop below (never the
        // plain way: fl marks them), with the keyed slots' memory work in slot order
        let kl = kp.as_ref().is_some_and(|k| !k.inv[tid as usize].is_empty());
        // (every line not on the plain way takes the loop below: lines with whole tokens, words
        // in a splitting column, words with rules, and every line of a grouped block, which
        // keeps its clock times through put_value; the loop makes the old pass 2's writes)
        let sb = ps.first().map_or(0, |p| p.slot);
        kind_last[0] = last0;
        let (mut q, mut j, mut ctx) = (p0, 0usize, 0u64);
        // the slots (one copy: the plain way takes most lines now; the rules' checks are made
        // only for lines of patterns with rules)
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
                if $R && kl {
                    // a key slot's value of this line: none yet
                    let kp = kp.as_deref_mut().unwrap();
                    if kp.iskey[sb as usize + k] {
                        kp.reg[k].0 = u8::MAX;
                    }
                }
                if $R {
                    if ri < re && rules[ri].slot as usize == k {
                        // a rule's slot: one yes/no; when the rule gives the value it is not stored
                        let given = if word { None } else { super::rules::applies(&rules[ri], &xs, lv[j].0, lv[j].1) };
                        flag_list.push((ri as u32) << 1 | given.is_none() as u32);
                        ri += 1;
                        if let Some(d) = given {
                            if rules[ri - 1].op == super::rules::NEAR {
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
                    xs[k] = if word { super::rules::NOT_NUM } else { super::rules::src_val(lv[j].0, lv[j].1) };
                }
                if word {
                    // a word: the k-th recent word of the column, or its number in the word table
                    let id = wid[q];
                    q += 1;
                    let base = (ci as usize) < nbase;
                    let c = &mut cols[ci as usize];
                    if c.split {
                        ctx = id as u64 + 1;
                    }
                    match c.wrecent.find(id) {
                        Some(x) => {
                            push_kind(c, &mut hot, base, ci, WORD + 1 + x as u8);
                            c.wrecent.hit(x);
                        }
                        None => {
                            push_kind(c, &mut hot, base, ci, WORD);
                            if !c.ids.is_empty() {
                                c.ids.push(u32::MAX);
                            }
                            let v = id as u64;
                            let kc = kclass(WORD);
                            let ks = kind_last[kc] == v && use_ks;
                            kind_last[kc] = v;
                            push_num(c, &mut hot, base, ci, v, sl | (ks as u32) << 31);
                            c.wrecent.push(id);
                        }
                    }
                    continue;
                }
                let (kind, v, s, e) = lv[j];
                j += 1;
                if kind == TOKK {
                    // a whole token: a word, always by its number (cache codes find repeats)
                    let base = (ci as usize) < nbase;
                    let c = &mut cols[ci as usize];
                    if c.split {
                        ctx = v + 1;
                    }
                    push_kind(c, &mut hot, base, ci, WORD);
                    if !c.ids.is_empty() {
                        c.ids.push(u32::MAX);
                    }
                    let kc = kclass(WORD);
                    let ks = kind_last[kc] == v && use_ks;
                    kind_last[kc] = v;
                    push_num(c, &mut hot, base, ci, v, sl | (ks as u32) << 31);
                    c.wrecent.push(v as u32);
                    continue;
                }
                if $R && kl && super::entity::numeric(kind) {
                    // a keyed slot whose key is a number in this line: the value's rank in its
                    // entity's recent values (kept with its place in the column when the
                    // entity is known); a key slot's value is kept for the line's later slots
                    let kp = kp.as_deref_mut().unwrap();
                    let d = kp.plan.dist[sb as usize + k] as usize;
                    if d != 0 && kp.reg[k - d].0 != u8::MAX {
                        let (kk, kv) = kp.reg[k - d];
                        let h = super::entity::key_hash(kk, kv, kind);
                        let (i, held) = kp.mem.find(h);
                        let rk = if held { kp.mem.rank(i, v) } else { super::entity::R };
                        kp.looks += 1;
                        if held {
                            let at = if (ci as usize) < nbase { unsafe { hot[ci as usize].vp().offset_from(cols[ci as usize].vals.as_ptr()) as u32 } } else { cols[ci as usize].vals.len() as u32 };
                            cols[ci as usize].ke.push((at, rk as u8, kp.mem.last(i)));
                        }
                        kp.mem.update(i, h, held, v, rk);
                    }
                    if kp.iskey[sb as usize + k] {
                        kp.reg[k] = (kind, v);
                    }
                }
                put_value(cols, &mut hot, ci, sl, li, l, kind, v, s, e, use_ks, want_times, kind_last, id_map, clock_vals);
            }
            };
        }
        let has_r = ri < re || kl;
        slots!(has_r);
        last0 = kind_last[0];
    }
    kind_last[0] = last0;
    // the cursors' lengths into the base columns
    for (c, h) in cols.iter_mut().zip(&hot[HOT_PAD..]) {
        // SAFETY: the cursors moved over the entries written (in the capacity reserved above)
        unsafe {
            c.kinds.set_len(h.k.offset_from(c.kinds.as_ptr()) as usize);
            c.vals.set_len(h.vp().offset_from(c.vals.as_ptr()) as usize);
            c.slots.set_len(h.s.offset_from(c.slots.as_ptr()) as usize);
        }
    }
}

#[inline(always)]
fn push_kind(c: &mut Col, hot: &mut [Hot], base: bool, ci: u32, kind: u8) {
    if base {
        let h = &mut hot[ci as usize];
        // SAFETY: reserved for every value of the column
        unsafe {
            *h.k = kind;
            h.k = h.k.add(1);
        }
    } else {
        c.kinds.push(kind);
    }
}

#[inline(always)]
fn push_num(c: &mut Col, hot: &mut [Hot], base: bool, ci: u32, v: u64, slot: u32) {
    if base {
        let h = &mut hot[ci as usize];
        // SAFETY: reserved for every value of the column (vp: the place without the ids bit;
        // adding 8 keeps the bit)
        unsafe {
            *h.vp() = v;
            *h.s = slot;
            h.v = h.v.add(1);
            h.s = h.s.add(1);
        }
    } else {
        c.vals.push(v);
        c.slots.push(slot);
    }
}

/// One value (not a word) into column ci at pattern slot sl.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn put_value(cols: &mut [Col], hot: &mut [Hot], ci: u32, sl: u32, li: usize, l: &[u8], kind: u8, v: u64, s: u32, e: u32, use_ks: bool, want_times: bool, kind_last: &mut [u64; 256], id_map: &mut IdMap, clock_vals: &mut Vec<(u32, u32, u64, i64)>) {
    let base = (ci as usize) < hot.len();
    if base && kind != 1 && !(100..=103).contains(&kind) && kind != LONG_ID && !hot[ci as usize].ids() {
        let h = &mut hot[ci as usize];
        let kc = kclass(kind);
        let ks = kind_last[kc] == v && use_ks;
        kind_last[kc] = v;
        // SAFETY: reserved for every value of the column
        unsafe {
            *h.k = kind;
            *h.v = v; // (not an ids column: no bit in v)
            *h.s = sl | (ks as u32) << 31;
            h.k = h.k.add(1);
            h.v = h.v.add(1);
            h.s = h.s.add(1);
        }
        if want_times && (40..68).contains(&kind) {
            clock_vals.push((li as u32, ci, v, micros(kind, v)));
        }
        return;
    }
    put_value_slow(cols, hot, base, ci, sl, li, l, kind, v, s, e, use_ks, want_times, kind_last, id_map, clock_vals);
}

/// Hex text t (kinds 100-103) into txt, as pass 2 has always written it: a UUID (kinds 100,
/// 101) as its 16 bytes without the dashes, other hex text (102, 103) as its length / 2 (a
/// varint) and its bytes; each byte is hexval(hi) << 4 | hexval(lo). The NEON way computes that
/// same function of every byte value, 16 output bytes at a time; a UUID takes it only with its
/// dashes exactly at 8, 13, 18 and 23 of its first 36 bytes (else the loop, which then decides).
#[inline(never)]
fn put_hex(txt: &mut Vec<u8>, kind: u8, t: &[u8]) {
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn pairs(h0: std::arch::aarch64::uint8x16_t, h1: std::arch::aarch64::uint8x16_t) -> [u8; 16] {
        use std::arch::aarch64::*;
        unsafe {
            // hexval: c - '0' up to '9', else (c | 0x20) - 'a' + 10 (bytes, wrapping)
            let hv = |c: uint8x16_t| vbslq_u8(vcleq_u8(c, vdupq_n_u8(b'9')), vsubq_u8(c, vdupq_n_u8(b'0')), vsubq_u8(vorrq_u8(c, vdupq_n_u8(0x20)), vdupq_n_u8(b'a' - 10)));
            let (v0, v1) = (hv(h0), hv(h1));
            let mut o = [0u8; 16];
            vst1q_u8(o.as_mut_ptr(), vorrq_u8(vshlq_n_u8::<4>(vuzp1q_u8(v0, v1)), vuzp2q_u8(v0, v1)));
            o
        }
    }
    if kind < 102 {
        #[cfg(target_arch = "aarch64")]
        if t.len() >= 36 {
            use std::arch::aarch64::*;
            // SAFETY: reads t[0..36] (loads at 0, 16 and 20)
            unsafe {
                let p = t.as_ptr();
                let (a, b, c) = (vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(20)));
                let dash = vdupq_n_u8(b'-');
                let ea = vld1q_u8([0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0].as_ptr());
                let eb = vld1q_u8([0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0].as_ptr());
                let mc = vld1q_u8([0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255].as_ptr());
                let bad = vorrq_u8(vorrq_u8(veorq_u8(vceqq_u8(a, dash), ea), veorq_u8(vceqq_u8(b, dash), eb)), vandq_u8(vceqq_u8(c, dash), mc));
                if vmaxvq_u8(bad) == 0 {
                    // the 32 digits in order: t[0..8] t[9..13] t[14..18] | t[19..23] t[24..36]
                    let i0 = vld1q_u8([0, 1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 12, 14, 15, 16, 17].as_ptr());
                    let i1 = vld1q_u8([3, 4, 5, 6, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31].as_ptr());
                    let o = pairs(vqtbl2q_u8(uint8x16x2_t(a, b), i0), vqtbl2q_u8(uint8x16x2_t(b, c), i1));
                    txt.extend_from_slice(&o);
                    return;
                }
            }
        }
        let mut k = 0;
        while k < 36 {
            if t[k] == b'-' {
                k += 1;
                continue;
            }
            txt.push(hexval(t[k]) << 4 | hexval(t[k + 1]));
            k += 2;
        }
    } else {
        put_varint(txt, (t.len() / 2) as u64);
        let mut t = t;
        #[cfg(target_arch = "aarch64")]
        while t.len() >= 32 {
            use std::arch::aarch64::*;
            // SAFETY: reads t[0..32]
            let o = unsafe { pairs(vld1q_u8(t.as_ptr()), vld1q_u8(t.as_ptr().add(16))) };
            txt.extend_from_slice(&o);
            t = &t[32..];
        }
        for q in t.chunks_exact(2) {
            txt.push(hexval(q[0]) << 4 | hexval(q[1]));
        }
    }
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn put_value_slow(cols: &mut [Col], hot: &mut [Hot], base: bool, ci: u32, sl: u32, li: usize, l: &[u8], kind: u8, v: u64, s: u32, e: u32, use_ks: bool, want_times: bool, kind_last: &mut [u64; 256], id_map: &mut IdMap, clock_vals: &mut Vec<(u32, u32, u64, i64)>) {
    let c = &mut cols[ci as usize];
    if (100..=103).contains(&kind) {
        let t = &l[s as usize..e as usize];
        push_kind(c, hot, base, ci, kind);
        if kind < 102 {
            // (an ID column may take it as a number: ids.rs; its place in the texts and among
            // the column's numbers)
            let nv = if base { unsafe { hot[ci as usize].vp().offset_from(c.vals.as_ptr()) as usize } } else { c.vals.len() };
            id_map.seq.push(IdVal { col: ci, kind, v: super::ids::uuid_bits(t), pos: c.txt.len() as u32, npos: nv as u32, sl });
        }
        put_hex(&mut c.txt, kind, t);
    } else if kind == 1 {
        let t = &l[s as usize..e as usize];
        let hit = c.recent.iter().position(|&(a, b)| &c.txt[a as usize..b as usize] == t);
        match hit {
            Some(0) => push_kind(c, hot, base, ci, 2),
            Some(k) => {
                push_kind(c, hot, base, ci, 70 + k as u8);
                let r = c.recent.remove(k);
                c.recent.insert(0, r);
            }
            None => {
                push_kind(c, hot, base, ci, 1);
                put_varint(&mut c.txt, t.len() as u64);
                let a = c.txt.len() as u32;
                c.txt.extend_from_slice(t);
                c.recent.insert(0, (a, c.txt.len() as u32));
                c.recent.truncate(8);
            }
        }
    } else {
        push_kind(c, hot, base, ci, kind);
        if kind == LONG_ID {
            // 0: a new long ID (added to the list); d + 1: seen, d places from the newest
            if c.ids.is_empty() {
                // (a base column's numbers so far: its cursor's distance from the buffer start)
                let nv = if base { unsafe { hot[ci as usize].vp().offset_from(c.vals.as_ptr()) as usize } } else { c.vals.len() };
                c.ids.resize(nv, u32::MAX);
            }
            let n = id_map.map.len() as u32;
            let e = *id_map.map.entry(v).or_insert(n);
            c.ids.push(if e == n { 0 } else { n - e });
            if e == n {
                // (a new one: an ID column may write it as a generator step, ids.rs; a repeat
                // keeps its cache code or list distance either way, so id_plan and id_apply
                // skip it)
                id_map.seq.push(IdVal { col: ci, kind, v: v as u128, pos: c.ids.len() as u32 - 1, npos: 0, sl });
            }
            if base {
                let h = &mut hot[ci as usize];
                h.v = h.v.map_addr(|a| a | 1);
            }
        } else if !c.ids.is_empty() {
            c.ids.push(u32::MAX);
        }
        let kc = kclass(kind);
        let ks = kind_last[kc] == v && use_ks;
        kind_last[kc] = v;
        push_num(c, hot, base, ci, v, sl | (ks as u32) << 31);
        if want_times && (40..68).contains(&kind) {
            clock_vals.push((li as u32, ci, v, micros(kind, v)));
        }
    }
}

/// Open addressing from 64-bit hashes to entry numbers (an entry's bytes are checked by the
/// caller): `find` gives the entries with the hash in turn.
#[derive(Default)]
struct HashIdx {
    slots: Vec<u32>, // entry + 1 (0: empty)
    hash: Vec<u64>,  // per entry
}

impl HashIdx {
    #[inline(always)]
    fn grow(&mut self) {
        if self.slots.len() < 2 * self.hash.len() + 2 {
            let size = (self.slots.len() * 2).max(256);
            self.slots = vec![0; size];
            for (k, &h) in self.hash.iter().enumerate() {
                let mut s = h as usize & (size - 1);
                while self.slots[s] != 0 {
                    s = (s + 1) & (size - 1);
                }
                self.slots[s] = k as u32 + 1;
            }
        }
    }
    /// The entry with hash h for which same(entry) holds, or the slot to add it at.
    #[inline(always)]
    fn find(&mut self, h: u64, mut same: impl FnMut(usize) -> bool) -> Result<usize, usize> {
        self.grow();
        let mask = self.slots.len() - 1;
        let mut s = h as usize & mask;
        loop {
            let id = self.slots[s];
            if id == 0 {
                return Err(s);
            }
            let k = (id - 1) as usize;
            if self.hash[k] == h && same(k) {
                return Ok(k);
            }
            s = (s + 1) & mask;
        }
    }
    /// Add entry number hash.len() with hash h at slot s (from find).
    #[inline(always)]
    fn add(&mut self, s: usize, h: u64) {
        self.slots[s] = self.hash.len() as u32 + 1;
        self.hash.push(h);
    }
}

/// The templates of a block (pack_block's loop, without an allocation per template or slot):
/// each distinct shape, with its planned words merged into marks, gets the number of its
/// template (numbered in order of first use; text in tmpl_out); each slot of a new template
/// gets a column, shared by the slots whose template text before them is the same (`shared`).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub fn templates(rbuf: &[u8], roff: &[u32], plan: &[Run], pidx: &[u32], shared: bool, tmpl_out: &mut Vec<u8>, col_of: &mut Vec<Vec<u32>>, slot_base: &mut Vec<u32>, nslots: &mut u32, cols: &mut Vec<Col>, final_of: &mut Vec<u32>) {
    let m = roff.len() - 1;
    let mut tidx = HashIdx::default();
    let mut tstart: Vec<(u32, u32)> = Vec::new(); // per template: start and length in tmpl_out
    // shared columns by the template text before the slot: (template that has it, length, column)
    let mut pidx_h = HashIdx::default();
    let mut pref: Vec<(u32, u32, u32)> = Vec::new();
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
        let h = shape_hash(s);
        let found = {
            let (tstart, tmpl) = (&tstart, &*tmpl_out);
            tidx.find(h, |k| {
                let (a, n) = tstart[k];
                same(&tmpl[a as usize..(a + n) as usize], s)
            })
        };
        let tid = match found {
            Ok(k) => k as u32,
            Err(slot) => {
                let t = tstart.len();
                tidx.add(slot, h);
                let a = tmpl_out.len() as u32;
                tmpl_out.extend_from_slice(s);
                tmpl_out.push(b'\n');
                tstart.push((a, s.len() as u32));
                let mut ids = Vec::new();
                for (pos, &c) in s.iter().enumerate() {
                    if c != MARK {
                        continue;
                    }
                    let id = if shared {
                        let p = &s[..pos];
                        let ph = shape_hash(p);
                        let found = {
                            let (pref, tmpl) = (&pref, &*tmpl_out);
                            pidx_h.find(ph, |k| {
                                let (o, n, _) = pref[k];
                                n as usize == pos && same(&tmpl[o as usize..o as usize + pos], p)
                            })
                        };
                        match found {
                            Ok(k) => pref[k].2,
                            Err(slot) => {
                                pidx_h.add(slot, ph);
                                cols.push(Col::default());
                                let c = (cols.len() - 1) as u32;
                                pref.push((a, pos as u32, c));
                                c
                            }
                        }
                    } else {
                        cols.push(Col::default());
                        (cols.len() - 1) as u32
                    };
                    ids.push(id);
                }
                slot_base.push(*nslots);
                *nslots += ids.len() as u32;
                col_of.push(ids);
                t as u32
            }
        };
        final_of.push(tid);
    }
}

/// The lines of `body` split at every '\n' (as body.split(|&c| c == b'\n'): an empty body is one
/// empty line), 64 bytes per step.
pub fn lines_of(body: &[u8]) -> Vec<&[u8]> {
    let n = body.len();
    let mut out: Vec<&[u8]> = Vec::with_capacity(n / 64 + 16);
    let mut start = 0;
    let mut p = 0;
    while p + 64 <= n {
        // SAFETY: p + 64 <= n
        let mut m = unsafe { newlines64(body.as_ptr().add(p)) };
        while m != 0 {
            let e = p + m.trailing_zeros() as usize;
            out.push(&body[start..e]);
            start = e + 1;
            m &= m - 1;
        }
        p += 64;
    }
    for (k, &c) in body[p..].iter().enumerate() {
        if c == b'\n' {
            out.push(&body[start..p + k]);
            start = p + k + 1;
        }
    }
    out.push(&body[start..]);
    out
}

/// The '\n' mask of the 64 bytes at p.
#[inline(always)]
unsafe fn newlines64(p: *const u8) -> u64 {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        let nl = vdupq_n_u8(b'\n');
        let bm = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        let (a, b, c, d) = (vceqq_u8(vld1q_u8(p), nl), vceqq_u8(vld1q_u8(p.add(16)), nl), vceqq_u8(vld1q_u8(p.add(32)), nl), vceqq_u8(vld1q_u8(p.add(48)), nl));
        // most 64-byte pieces hold at most one '\n': skip the mask when there is none
        if vmaxvq_u8(vorrq_u8(vorrq_u8(a, b), vorrq_u8(c, d))) == 0 {
            return 0;
        }
        let s = vpaddq_u8(vpaddq_u8(vandq_u8(a, bm), vandq_u8(b, bm)), vpaddq_u8(vandq_u8(c, bm), vandq_u8(d, bm)));
        vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
    }
    #[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
    // SAFETY: p[..64] is readable (the caller's promise)
    unsafe {
        super::x86::nl_mask64(p)
    }
    #[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
    {
        let b = unsafe { std::slice::from_raw_parts(p, 64) };
        let mut m = 0u64;
        for (i, &c) in b.iter().enumerate() {
            m |= ((c == b'\n') as u64) << i;
        }
        m
    }
}

/// Keyed slots in pass 2 (entity.rs): the plan, per pattern slot whether it is a key, per
/// pattern whether it has keyed slots, the memory, and the line's key values by slot.
pub struct KeyedPass {
    pub plan: super::entity::Plan,
    iskey: Vec<bool>,
    // per pattern: its key and keyed slots (slot, key distance, is a key), in slot order
    inv: Vec<Vec<(u16, u16, bool)>>,
    mem: super::entity::Mem,
    reg: Vec<(u8, u64)>,
    // memory lookups in the block
    pub looks: usize,
}

impl KeyedPass {
    pub fn new(plan: super::entity::Plan, slot_base: &[u32], nslots: u32) -> KeyedPass {
        let np = slot_base.len();
        let end = |t: usize| if t + 1 < np { slot_base[t + 1] as usize } else { nslots as usize };
        let iskey = plan.keys();
        let inv = (0..np).map(|t| {
            let b = slot_base[t] as usize;
            (b..end(t)).filter(|&g| plan.dist[g] != 0 || iskey[g]).map(|g| ((g - b) as u16, plan.dist[g], iskey[g])).collect()
        }).collect();
        let most = (0..np).map(|t| end(t) - slot_base[t] as usize).max().unwrap_or(0);
        KeyedPass { iskey, inv, mem: super::entity::Mem::new(plan.bits), reg: vec![(u8::MAX, 0); most], plan, looks: 0 }
    }
}

/// The rules of a block (rules.rs) for pass2_tok: each pattern's first rule, the rules, and
/// what pass 2 fills in: the answers (rule << 1 | not given), the near rules' differences, the
/// line's values so far (rule sources) and the values given per prefix column.
pub struct RuleLists<'r> {
    pub rfirst: &'r [u32],
    pub rules: &'r [super::rules::Rule],
    pub flag_list: std::cell::RefCell<&'r mut Vec<u32>>,
    pub near_list: std::cell::RefCell<&'r mut Vec<i64>>,
    pub xs: std::cell::RefCell<&'r mut Vec<u64>>,
    pub rminus: std::cell::RefCell<&'r mut Vec<u32>>,
}

/// The hardware paths give exactly the old answers (hotloops, 2026-11-15; aarch64 only: the
/// other CPUs keep the old code).
#[cfg(all(test, target_arch = "aarch64"))]
mod hw_tests {
    use super::*;

    fn rng(st: &mut u64) -> u64 {
        *st ^= *st << 13;
        *st ^= *st >> 7;
        *st ^= *st << 17;
        *st
    }

    /// same_classes (32 bytes per step) against the 16-byte version, for every length up to 300
    /// with lines that differ in one byte (also past n), in one digit place, or only in digits.
    #[test]
    fn classes_same() {
        let mut st = 0x2545_F491_4F6C_DD1Du64;
        let alpha = b"0123456789 :.-/aZ\x00\xFF";
        let (mut yes, mut no) = (0usize, 0usize);
        for round in 0..300_000usize {
            let n = 1 + (rng(&mut st) % 300) as usize;
            let mut a: Vec<u8> = (0..n + 64).map(|_| alpha[(rng(&mut st) % alpha.len() as u64) as usize]).collect();
            let mut b: Vec<u8> = a.iter().map(|&c| if c.is_ascii_digit() { b'0' + (rng(&mut st) % 10) as u8 } else { c }).collect();
            match round % 4 {
                0 => {}
                1 => {
                    let i = (rng(&mut st) % (n as u64 + 40)) as usize;
                    b[i] = alpha[(rng(&mut st) % alpha.len() as u64) as usize];
                }
                2 => {
                    let i = (rng(&mut st) % n as u64) as usize;
                    b[i] = if b[i].is_ascii_digit() { b':' } else { b'5' };
                }
                _ => {
                    let i = (rng(&mut st) % n as u64) as usize;
                    a[i] = b'0' + (a[i] % 10);
                    b[i] = if rng(&mut st) % 2 == 0 { b'9' } else { b'/' };
                }
            }
            // SAFETY: both have n + 64 bytes (same_classes reads at most n + 15, the old one too)
            let (x, y) = unsafe { (same_classes(a.as_ptr(), b.as_ptr(), n), same_classes_ref(a.as_ptr(), b.as_ptr(), n)) };
            assert_eq!(x, y, "n {} a {:?} b {:?}", n, a, b);
            if x { yes += 1 } else { no += 1 }
        }
        assert!(yes > 50_000 && no > 50_000, "{} {}", yes, no);
    }
}

/// same (NEON from 32 bytes on) against a plain slice compare (hotloops, 2026-11-15).
#[cfg(test)]
mod same_tests {
    use super::*;
    #[test]
    fn same_is_eq() {
        let mut st = 0x1234_5678_9ABC_DEF1u64;
        let mut rng = || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        for round in 0..300_000usize {
            let n = (rng() % 200) as usize;
            let a: Vec<u8> = (0..n).map(|_| (rng() % 4) as u8 + b'a').collect();
            let mut b = a.clone();
            if round % 2 == 1 && n > 0 {
                let i = (rng() % n as u64) as usize;
                b[i] = b[i].wrapping_add(1 + (rng() % 3) as u8);
            }
            // a shorter or longer b sometimes
            if round % 7 == 3 {
                b.push(b'a');
            }
            assert_eq!(same(&a, &b), a == b, "{:?} {:?}", a, b);
        }
    }
}
