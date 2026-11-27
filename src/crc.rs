//! CRC-32, the standard one (as in zlib, gzip and PNG): polynomial 0xEDB88320, start and
//! end with all bits flipped. Uses the processor's CRC instruction on ARM64 when it has one,
//! and carry-less multiplies (PCLMULQDQ) on x86_64 (crc_fold_x86; x86's own CRC32 instruction
//! is CRC-32C, another polynomial, so it is not used here).

static TABLES: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();

fn tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    for i in 0..256 {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        t[0][i] = c;
    }
    for i in 0..256 {
        for k in 1..8 {
            t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xff) as usize];
        }
    }
    t
}

/// Running CRC-32: start with `Crc::new()`, feed bytes with `update`, read `value()`.
#[derive(Clone, Copy, Default)]
pub struct Crc(u32);

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc_hw(c: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32b, __crc32d};
    let word = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap());
    // long data: three runs of the instruction side by side (each waits only for itself, so
    // the processor overlaps them), joined with crc32_combine
    if data.len() >= 1 << 14 {
        let k = data.len() / 24 * 8;
        let (a, rest) = data.split_at(k);
        let (b, c3) = rest.split_at(k);
        let (mut x, mut y, mut z) = (!c, !0u32, !0u32);
        for ((p, q), r) in a.chunks_exact(8).zip(b.chunks_exact(8)).zip(c3.chunks_exact(8)) {
            x = __crc32d(x, word(p));
            y = __crc32d(y, word(q));
            z = __crc32d(z, word(r));
        }
        let mut ch = c3[k..].chunks_exact(8);
        for w in &mut ch {
            z = __crc32d(z, word(w));
        }
        for &v in ch.remainder() {
            z = __crc32b(z, v);
        }
        return crc32_combine(crc32_combine(!x, !y, k as u64), !z, c3.len() as u64);
    }
    let mut c = !c;
    let mut ch = data.chunks_exact(8);
    for b in &mut ch {
        c = __crc32d(c, word(b));
    }
    for &b in ch.remainder() {
        c = __crc32b(c, b);
    }
    !c
}

/// a(x) times b(x) modulo the CRC polynomial, both in the CRC's bit order (bit 31 is x^0):
/// zlib's multmodp, always 32 steps.
const fn multmodp(a: u32, mut b: u32) -> u32 {
    let mut p = 0u32;
    let mut i = 0;
    while i < 32 {
        if a & (1 << (31 - i)) != 0 {
            p ^= b;
        }
        b = if b & 1 != 0 { (b >> 1) ^ 0xEDB8_8320 } else { b >> 1 };
        i += 1;
    }
    p
}

/// X2N[k] = x^(2^k) modulo the CRC polynomial (zlib's x2n_table), made at compile time.
const X2N: [u32; 32] = {
    let mut t = [0u32; 32];
    // x^1
    let mut p = 1u32 << 30;
    let mut k = 0;
    while k < 32 {
        t[k] = p;
        p = multmodp(p, p);
        k += 1;
    }
    t
};

/// The CRC-32 of A followed by B, from the CRC-32 of A, that of B and B's length (as zlib's
/// crc32_combine since 1.2.12: crc_a times x^(8 len_b) modulo the polynomial, that power made
/// from the precomputed X2N; the same value as the older 32 x 32 bit matrix method, at a
/// small part of its cost). (Only crc_hw, on ARM64, and the tests use it.)
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
fn crc32_combine(crc_a: u32, crc_b: u32, len_b: u64) -> u32 {
    if len_b == 0 {
        return crc_a;
    }
    // x^(8 len_b): the powers x^(2^(k+3)) for the bits k of len_b (x^0 to start)
    let (mut p, mut n, mut k) = (1u32 << 31, len_b, 3usize);
    while n != 0 {
        if n & 1 != 0 {
            p = multmodp(X2N[k & 31], p);
        }
        n >>= 1;
        k += 1;
    }
    multmodp(p, crc_a) ^ crc_b
}

/// x^n modulo the CRC polynomial (bit 31 is x^0): the powers x^(2^k) of X2N for the bits k
/// of n (as crc32_combine makes x^(8 len_b)).
#[cfg_attr(not(all(target_arch = "aarch64", target_endian = "little")), allow(dead_code))]
const fn xpow(mut n: u64) -> u32 {
    let (mut p, mut k) = (1u32 << 31, 0usize);
    while n != 0 {
        if n & 1 != 0 {
            p = multmodp(X2N[k & 31], p);
        }
        n >>= 1;
        k += 1;
    }
    p
}

/// x^n modulo the polynomial as a 33-bit constant for crc_fold's carry-less products (the 32
/// bits in the CRC's order, shifted left by one: a product then lands where the bytes it is
/// folded onto are).
#[cfg_attr(not(all(target_arch = "aarch64", target_endian = "little")), allow(dead_code))]
const fn fold_k(n: u64) -> u64 {
    (xpow(n) as u64) << 1
}

/// The shortest data crc_fold takes (two rounds of its 16 vectors); shorter data goes to
/// crc_hw.
#[cfg_attr(not(all(target_arch = "aarch64", target_endian = "little")), allow(dead_code))]
const FOLD_MIN: usize = 512;

/// x(x) times x^D modulo the polynomial, as 128 bits, for k = (x^(D+32), x^(D-32)) (each
/// shifted left by one: fold_k), XORed with y: the low 8 bytes of x times k.0, the high 8
/// times k.1, as two carry-less 64 x 33 bit products.
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "aes")]
unsafe fn fold16(x: std::arch::aarch64::uint64x2_t, k: std::arch::aarch64::uint64x2_t, y: std::arch::aarch64::uint64x2_t) -> std::arch::aarch64::uint64x2_t {
    use std::arch::aarch64::*;
    let lo = vreinterpretq_u64_p128(vmull_p64(vgetq_lane_u64::<0>(x), vgetq_lane_u64::<0>(k)));
    let hi = vreinterpretq_u64_p128(vmull_high_p64(vreinterpretq_p64_u64(x), vreinterpretq_p64_u64(k)));
    veorq_u64(veorq_u64(lo, hi), y)
}

/// CRC-32 with carry-less multiplies (PMULL, about 3 times crc_hw's speed), for data of at
/// least FOLD_MIN bytes: 16 vectors of 16 bytes, each moved 256 bytes forward per step (times
/// x^2048 modulo the polynomial: fold16) and XORed onto the data there; then the vectors are
/// folded into one, and the whole vectors after them, and that vector's CRC from 0 (two
/// CRC32X) is the CRC so far; the last 0-15 bytes go through the CRC instruction. c is the
/// CRC before data (as Crc keeps it). The same value as crc_hw and the tables (tests).
/// Little-endian only: the vector loads take the bytes in the CPU's order, and the folding
/// constants assume the first byte lowest (big-endian aarch64 uses crc_hw).
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "crc,aes")]
unsafe fn crc_fold(c: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::*;
    let n = data.len();
    assert!(n >= FOLD_MIN);
    let kn: [u64; 2] = const { [fold_k(2048 + 32), fold_k(2048 - 32)] };
    let k1: [u64; 2] = const { [fold_k(160), fold_k(96)] };
    let p = data.as_ptr();
    // the 16 bytes at data[i..]: every use below has i + 16 <= n
    let ld = |i: usize| {
        debug_assert!(i + 16 <= n);
        // SAFETY: i + 16 <= n = data.len() (see each use)
        unsafe { vld1q_u64(p.add(i) as *const u64) }
    };
    // (written out, not as a loop over an array: the 16 vectors stay in registers)
    // the first 256 bytes (n >= FOLD_MIN = 512), the register (the CRC so far, its bits
    // flipped) XORed onto the first 4
    let mut x0 = veorq_u64(ld(0), vsetq_lane_u64::<0>(!c as u64, vdupq_n_u64(0)));
    let (mut x1, mut x2, mut x3) = (ld(16), ld(32), ld(48));
    let (mut x4, mut x5, mut x6, mut x7) = (ld(64), ld(80), ld(96), ld(112));
    let (mut x8, mut x9, mut x10, mut x11) = (ld(128), ld(144), ld(160), ld(176));
    let (mut x12, mut x13, mut x14, mut x15) = (ld(192), ld(208), ld(224), ld(240));
    // SAFETY: kn has 2 u64s; the PMULL (aes) feature is on in this function
    unsafe {
        let k = vld1q_u64(kn.as_ptr());
        let mut i = 256;
        while i + 256 <= n {
            // (i + 240 + 16 <= n)
            x0 = fold16(x0, k, ld(i));
            x1 = fold16(x1, k, ld(i + 16));
            x2 = fold16(x2, k, ld(i + 32));
            x3 = fold16(x3, k, ld(i + 48));
            x4 = fold16(x4, k, ld(i + 64));
            x5 = fold16(x5, k, ld(i + 80));
            x6 = fold16(x6, k, ld(i + 96));
            x7 = fold16(x7, k, ld(i + 112));
            x8 = fold16(x8, k, ld(i + 128));
            x9 = fold16(x9, k, ld(i + 144));
            x10 = fold16(x10, k, ld(i + 160));
            x11 = fold16(x11, k, ld(i + 176));
            x12 = fold16(x12, k, ld(i + 192));
            x13 = fold16(x13, k, ld(i + 208));
            x14 = fold16(x14, k, ld(i + 224));
            x15 = fold16(x15, k, ld(i + 240));
            i += 256;
        }
        let k = vld1q_u64(k1.as_ptr());
        let mut a = x0;
        for x in [x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15] {
            a = fold16(a, k, x);
        }
        while i + 16 <= n {
            a = fold16(a, k, ld(i));
            i += 16;
        }
        let r = __crc32d(0, vgetq_lane_u64::<0>(a));
        let mut r = __crc32d(r, vgetq_lane_u64::<1>(a));
        let mut ch = data[i..].chunks_exact(8);
        for b in &mut ch {
            r = __crc32d(r, u64::from_le_bytes(b.try_into().unwrap()));
        }
        for &b in ch.remainder() {
            r = __crc32b(r, b);
        }
        !r
    }
}

/// x86_64: whether the processor has the carry-less multiply (PCLMULQDQ), found once (0: not
/// yet, 1: no, 2: yes). Builds with --cfg fewer_simd="scalar" or "sse2" (tests of every path)
/// use the tables.
#[cfg(target_arch = "x86_64")]
fn pclmul() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static S: AtomicU8 = AtomicU8::new(0);
    if cfg!(any(fewer_simd = "scalar", fewer_simd = "sse2")) {
        return false;
    }
    match S.load(Ordering::Relaxed) {
        0 => {
            let y = std::arch::is_x86_feature_detected!("pclmulqdq");
            S.store(if y { 2 } else { 1 }, Ordering::Relaxed);
            y
        }
        s => s == 2,
    }
}

/// The shortest data crc_fold_x86 takes (two rounds of its 4 vectors); shorter data goes to
/// the tables.
#[cfg(target_arch = "x86_64")]
const FOLD_MIN_X86: usize = 128;

/// fold16 with PCLMULQDQ: x times x^D modulo the polynomial, XORed with y, for k = (x^(D+32),
/// x^(D-32)) (fold_k): the low 8 bytes of x times k's low 8 (selector 0x00), the high 8 times
/// k's high 8 (0x11), as two carry-less 64 x 33 bit products.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "pclmulqdq")]
#[allow(unused_unsafe)]
unsafe fn fold16_x86(x: std::arch::x86_64::__m128i, k: std::arch::x86_64::__m128i, y: std::arch::x86_64::__m128i) -> std::arch::x86_64::__m128i {
    use std::arch::x86_64::*;
    unsafe { _mm_xor_si128(_mm_xor_si128(_mm_clmulepi64_si128::<0x00>(x, k), _mm_clmulepi64_si128::<0x11>(x, k)), y) }
}

/// The CRC register (its bits not flipped) after data, from c, by the tables (the plain loop of
/// Crc::update).
#[cfg(target_arch = "x86_64")]
fn table_raw(t: &[[u32; 256]; 8], mut c: u32, data: &[u8]) -> u32 {
    let mut ch = data.chunks_exact(8);
    for b in &mut ch {
        let lo = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) ^ c;
        let hi = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in ch.remainder() {
        c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

/// CRC-32 with carry-less multiplies (x86_64 PCLMULQDQ; crc_fold's method with 4 vectors
/// instead of 16, as x86 has 16 vector registers): 4 vectors of 16 bytes, each moved 64 bytes
/// forward per step (times x^512 modulo the polynomial: fold16_x86) and XORed onto the data
/// there; then the 4 are folded into one, and the whole vectors after them; that vector's CRC
/// from 0 (by the tables: x86 has no instruction for this polynomial) is the CRC so far, and
/// the last 0-15 bytes follow by the tables. c is the CRC before data (as Crc keeps it). The
/// same value as the tables (tests). x86_64 is little-endian: the loads take the first byte
/// lowest, as the folding constants assume.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "pclmulqdq")]
#[allow(unused_unsafe)]
unsafe fn crc_fold_x86(c: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::*;
    let n = data.len();
    assert!(n >= FOLD_MIN_X86);
    let k4: [u64; 2] = const { [fold_k(512 + 32), fold_k(512 - 32)] };
    let k1: [u64; 2] = const { [fold_k(160), fold_k(96)] };
    let p = data.as_ptr();
    // the 16 bytes at data[i..]: every use below has i + 16 <= n
    let ld = |i: usize| {
        debug_assert!(i + 16 <= n);
        // SAFETY: i + 16 <= n = data.len() (see each use); loadu takes any alignment
        unsafe { _mm_loadu_si128(p.add(i) as *const __m128i) }
    };
    // SAFETY: k4 and k1 hold 2 u64s (16 bytes) each; PCLMULQDQ is on in this function (the
    // caller checked the processor has it); the stores write v's 16 bytes
    unsafe {
        // the first 64 bytes (n >= FOLD_MIN_X86 = 128), the register (the CRC so far, its bits
        // flipped) XORed onto the first 4
        let mut x0 = _mm_xor_si128(ld(0), _mm_cvtsi32_si128(!c as i32));
        let (mut x1, mut x2, mut x3) = (ld(16), ld(32), ld(48));
        let k = _mm_loadu_si128(k4.as_ptr() as *const __m128i);
        let mut i = 64;
        while i + 64 <= n {
            // (i + 48 + 16 <= n)
            x0 = fold16_x86(x0, k, ld(i));
            x1 = fold16_x86(x1, k, ld(i + 16));
            x2 = fold16_x86(x2, k, ld(i + 32));
            x3 = fold16_x86(x3, k, ld(i + 48));
            i += 64;
        }
        let k = _mm_loadu_si128(k1.as_ptr() as *const __m128i);
        let mut a = fold16_x86(x0, k, x1);
        a = fold16_x86(a, k, x2);
        a = fold16_x86(a, k, x3);
        while i + 16 <= n {
            a = fold16_x86(a, k, ld(i));
            i += 16;
        }
        let mut v = [0u8; 16];
        _mm_storeu_si128(v.as_mut_ptr() as *mut __m128i, a);
        let t = TABLES.get_or_init(tables);
        !table_raw(t, table_raw(t, 0, &v), &data[i..])
    }
}

impl Crc {
    pub fn new() -> Crc {
        Crc(0)
    }

    pub fn value(&self) -> u32 {
        self.0
    }

    pub fn update(&mut self, data: &[u8]) {
        #[cfg(target_arch = "aarch64")]
        {
            if std::arch::is_aarch64_feature_detected!("crc") {
                #[cfg(target_endian = "little")]
                {
                    if data.len() >= FOLD_MIN && std::arch::is_aarch64_feature_detected!("aes") {
                        // SAFETY: the CPU has the CRC and carry-less multiply (aes)
                        // instructions (checked just above), and data has at least FOLD_MIN
                        // bytes
                        self.0 = unsafe { crc_fold(self.0, data) };
                        return;
                    }
                }
                // SAFETY: the CPU has the CRC instructions (checked just above)
                self.0 = unsafe { crc_hw(self.0, data) };
                return;
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            if data.len() >= FOLD_MIN_X86 && pclmul() {
                // SAFETY: the processor has PCLMULQDQ (checked just above), and data has at
                // least FOLD_MIN_X86 bytes
                self.0 = unsafe { crc_fold_x86(self.0, data) };
                return;
            }
        }
        let t = TABLES.get_or_init(tables);
        let mut c = !self.0;
        let mut ch = data.chunks_exact(8);
        for b in &mut ch {
            let lo = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) ^ c;
            let hi = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            c = t[7][(lo & 0xff) as usize]
                ^ t[6][((lo >> 8) & 0xff) as usize]
                ^ t[5][((lo >> 16) & 0xff) as usize]
                ^ t[4][(lo >> 24) as usize]
                ^ t[3][(hi & 0xff) as usize]
                ^ t[2][((hi >> 8) & 0xff) as usize]
                ^ t[1][((hi >> 16) & 0xff) as usize]
                ^ t[0][(hi >> 24) as usize];
        }
        for &b in ch.remainder() {
            c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = !c;
    }
}

/// CRC-32 of a whole byte string.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.update(data);
    c.value()
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value_and_pieces() {
        // the standard CRC-32 check value
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(super::crc32(b""), 0);
        // piece by piece gives the same as all at once
        let d: Vec<u8> = (0..100_003u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for cut in [0usize, 1, 7, 1000, 100_002] {
            let mut c = super::Crc::new();
            c.update(&d[..cut]);
            c.update(&d[cut..]);
            assert_eq!(c.value(), super::crc32(&d));
        }
        // the processor's instruction (three runs joined for long data) gives the table's CRC,
        // at every length around the switch and the joins
        let table = |d: &[u8]| {
            let t = super::tables();
            let mut c = !0u32;
            for &b in d {
                c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
            }
            !c
        };
        for n in [0usize, 1, 8, 23, 24, 25, (1 << 14) - 1, 1 << 14, (1 << 14) + 1, (1 << 14) + 23, 50_001, 100_003] {
            assert_eq!(super::crc32(&d[..n]), table(&d[..n]), "length {n}");
            let mut c = super::Crc::new();
            c.update(&d[..n / 3]);
            c.update(&d[n / 3..n]);
            assert_eq!(c.value(), table(&d[..n]), "length {n} in two pieces");
        }
        assert_eq!(super::crc32_combine(super::crc32(b"12345"), super::crc32(b"6789"), 4), 0xCBF4_3926);
    }

    #[test]
    fn fold_same_as_table() {
        // crc_fold (data of FOLD_MIN bytes and more) and crc_hw (less) give the table's CRC at
        // every length up to 3000 bytes, at several offsets and start values, and on long data
        let t = super::tables();
        let table = |c0: u32, d: &[u8]| {
            let mut c = !c0;
            for &b in d {
                c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
            }
            !c
        };
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let d: Vec<u8> = (0..(1usize << 21) + 64)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for off in [0usize, 1, 7, 8, 13] {
            for n in 0..3000usize {
                let c0 = if n % 3 == 0 { 0 } else { (n as u32 ^ off as u32).wrapping_mul(2_654_435_761) };
                let mut c = super::Crc(c0);
                c.update(&d[off..off + n]);
                assert_eq!(c.value(), table(c0, &d[off..off + n]), "length {n} offset {off}");
            }
        }
        for n in [4095usize, 4096, 4097, 65_536 + 255, 1 << 20, (1 << 21) + 3] {
            let mut c = super::Crc(0x1234_5678);
            c.update(&d[5..5 + n]);
            assert_eq!(c.value(), table(0x1234_5678, &d[5..5 + n]), "length {n}");
        }
    }

    #[test]
    fn folding_same_as_table() {
        // (from the shell2 patch, which made the same folding CRC) the folding code gives the
        // table's CRC: every length up to 4 KiB plus FOLD_MIN, from any start value, at every
        // alignment of the data, in pieces; crc_hw alone too (CPUs without PMULL)
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let d: Vec<u8> = (0..(1usize << 20) + 64).map(|_| (next() >> 24) as u8).collect();
        let t = super::tables();
        let table = |c: u32, d: &[u8]| {
            let mut c = !c;
            for &b in d {
                c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
            }
            !c
        };
        let upd = |c: u32, d: &[u8]| {
            let mut x = super::Crc(c);
            x.update(d);
            x.value()
        };
        for n in 0..4096 + super::FOLD_MIN {
            let off = n % 16;
            let x = &d[off..off + n];
            let c0 = if n % 3 == 0 { 0 } else { next() as u32 };
            assert_eq!(upd(c0, x), table(c0, x), "length {n}");
        }
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("crc") {
            for n in [0usize, 1, 9, 511, 512, 4097, (1 << 14) - 1, 1 << 14, (1 << 14) + 23, 100_003, 1 << 20] {
                let c0 = next() as u32;
                // SAFETY: the CPU has the CRC instructions (checked above)
                assert_eq!(unsafe { super::crc_hw(c0, &d[3..3 + n]) }, table(c0, &d[3..3 + n]), "crc_hw length {n}");
            }
        }
        for _ in 0..200 {
            let n = (next() % (1 << 20)) as usize;
            let off = (next() % 64) as usize;
            let x = &d[off..off + n];
            let cut = (next() as usize) % (n + 1);
            let want = table(0, x);
            assert_eq!(super::crc32(x), want, "length {n}");
            assert_eq!(upd(upd(0, &x[..cut]), &x[cut..]), want, "length {n} cut {cut}");
        }
        for b in [0u8, 0xff] {
            let z = vec![b; 100_003];
            assert_eq!(super::crc32(&z), table(0, &z));
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_fold_same_as_table() {
        // the constants are the known ones for this folding (Linux's crc32-pclmul_asm.S,
        // R1..R4: x^(512 + 32), x^(512 - 32), x^160, x^96 modulo the polynomial, bit-reflected,
        // shifted left by one)
        assert_eq!([super::fold_k(544), super::fold_k(480), super::fold_k(160), super::fold_k(96)], [0x1_5444_2bd4, 0x1_c6e4_1596, 0x1_7519_97d0, 0x0_ccaa_009e]);
        if !super::pclmul() {
            eprintln!("no PCLMULQDQ (or a build without it): the tables only");
            return;
        }
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let d: Vec<u8> = (0..(1usize << 20) + 64).map(|_| (next() >> 24) as u8).collect();
        let t = super::tables();
        let table = |c: u32, d: &[u8]| {
            let mut c = !c;
            for &b in d {
                c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
            }
            !c
        };
        // every length from FOLD_MIN_X86 to 4 KiB, at every alignment, from varied start values
        for n in super::FOLD_MIN_X86..4096 + 64 {
            let off = n % 16;
            let x = &d[off..off + n];
            let c0 = if n % 3 == 0 { 0 } else { next() as u32 };
            // SAFETY: the processor has PCLMULQDQ (pclmul), n >= FOLD_MIN_X86
            assert_eq!(unsafe { super::crc_fold_x86(c0, x) }, table(c0, x), "length {n}");
        }
        for n in [65_536 + 255, 1 << 20] {
            let c0 = next() as u32;
            // SAFETY: as above
            assert_eq!(unsafe { super::crc_fold_x86(c0, &d[3..3 + n]) }, table(c0, &d[3..3 + n]), "length {n}");
        }
        for b in [0u8, 0xff] {
            let z = vec![b; 100_003];
            // SAFETY: as above
            assert_eq!(unsafe { super::crc_fold_x86(0, &z) }, table(0, &z));
        }
    }

    /// The x86_64 fold at every alignment 0..64 and every length FOLD_MIN_X86..=1100, and
    /// Crc::update in two pieces at every alignment and every length 0..=300, against CRC-32
    /// one bit at a time.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_fold_every_alignment() {
        let bits = |c: u32, d: &[u8]| {
            let mut c = !c;
            for &b in d {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
                }
            }
            !c
        };
        let mut s = 0x0123_4567_89AB_CDEFu64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let d: Vec<u8> = (0..64 + 1100).map(|_| (next() >> 24) as u8).collect();
        for a in 0..64 {
            for n in 0..=300 {
                let x = &d[a..a + n];
                let cut = (next() as usize) % (n + 1);
                let mut c = super::Crc::new();
                c.update(&x[..cut]);
                c.update(&x[cut..]);
                assert_eq!(c.value(), bits(0, x), "a {a} n {n} cut {cut}");
            }
        }
        if !super::pclmul() {
            eprintln!("no PCLMULQDQ (or a build without it): the tables only");
            return;
        }
        for a in 0..64 {
            for n in super::FOLD_MIN_X86..=1100 {
                let c0 = next() as u32;
                let x = &d[a..a + n];
                // SAFETY: the processor has PCLMULQDQ (pclmul), n >= FOLD_MIN_X86
                assert_eq!(unsafe { super::crc_fold_x86(c0, x) }, bits(c0, x), "a {a} n {n}");
            }
        }
    }

    #[test]
    fn combine_same_as_matrix_method() {
        // the former method (zlib before 1.2.12): B's length in zero bits applied to crc_a as
        // powers of a 32 x 32 bit matrix
        fn combine_matrix(crc_a: u32, crc_b: u32, len_b: u64) -> u32 {
            fn times(m: &[u32; 32], mut v: u32) -> u32 {
                let mut s = 0;
                let mut i = 0;
                while v != 0 {
                    if v & 1 != 0 {
                        s ^= m[i];
                    }
                    v >>= 1;
                    i += 1;
                }
                s
            }
            fn square(m: &[u32; 32]) -> [u32; 32] {
                let mut sq = [0u32; 32];
                for (n, x) in sq.iter_mut().enumerate() {
                    *x = times(m, m[n]);
                }
                sq
            }
            if len_b == 0 {
                return crc_a;
            }
            // the operator for one zero bit, then for two and four
            let mut odd = [0u32; 32];
            odd[0] = 0xEDB8_8320;
            for (n, x) in odd.iter_mut().enumerate().skip(1) {
                *x = 1 << (n - 1);
            }
            let mut even = square(&odd);
            odd = square(&even);
            let (mut c, mut len) = (crc_a, len_b);
            loop {
                // one zero byte, then two, four, ...
                even = square(&odd);
                if len & 1 != 0 {
                    c = times(&even, c);
                }
                len >>= 1;
                if len == 0 {
                    break;
                }
                odd = square(&even);
                if len & 1 != 0 {
                    c = times(&odd, c);
                }
                len >>= 1;
                if len == 0 {
                    break;
                }
            }
            c ^ crc_b
        }
        // every length of up to 4096 bytes, then lengths of every size up to 2^63, with
        // varied CRCs: the same value as the matrix method
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for len in 0..4096u64 {
            let (a, b) = (next() as u32, next() as u32);
            assert_eq!(super::crc32_combine(a, b, len), combine_matrix(a, b, len), "length {len}");
        }
        for bits in 1..64u32 {
            for _ in 0..64 {
                let len = next() >> (64 - bits);
                let (a, b) = (next() as u32, next() as u32);
                assert_eq!(super::crc32_combine(a, b, len), combine_matrix(a, b, len), "length {len}");
            }
        }
        for len in [u64::MAX, u64::MAX - 1, 1 << 63, (1 << 32) - 1, 1 << 32] {
            assert_eq!(super::crc32_combine(0x1234_5678, 0x9abc_def0, len), combine_matrix(0x1234_5678, 0x9abc_def0, len), "length {len}");
        }
    }
}
