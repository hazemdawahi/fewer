//! The engine: the fast-lane log packer (the final engine). Every file in this folder except
//! this one and bridge.rs is the engine's own code, copied in with
//! mechanical edits only (core.rs is the engine's main.rs without its benchmark main();
//! `crate::` became `super::` in its modules; its environment reads go to `no_env` below;
//! ozip's method-5 coder is visible to the bridge), so a newer engine replaces them as they
//! are. Only bridge.rs reaches into them.
#![allow(dead_code, unused, unused_unsafe, clippy::all, clippy::pedantic)]

include!("core.rs");

pub mod bridge;

/// The opener before M7 (ref's open.rs, unchanged), for the old-against-new tests only: it is
/// not in the program.
#[cfg(test)]
mod open_ref;

/// The engine's reads of the environment (its developer switches FL_*, CM_OZ*):
/// The engine's port turns `std::env::var(` and `std::env::var_os(` in its files into these,
/// which always answer "not set". fewer-pack runs the engine's default setting whatever the
/// environment holds, and the compiler leaves out the code that only a switch reaches.
pub(crate) mod no_env {
    #[inline(always)]
    pub fn var_os<K: AsRef<std::ffi::OsStr>>(_: K) -> Option<std::ffi::OsString> {
        None
    }
    #[inline(always)]
    pub fn var<K: AsRef<std::ffi::OsStr>>(_: K) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }
}

/// What the engine's cached switches become (`static S: OnceLock<T>` whose value is read from
/// the environment, such as `*S.get_or_init(|| !env_on("FL_NOWORDS"))`): the engine's port
/// gives them this type, which works the value out at each use instead of caching it. The
/// environment reads always answer "not set" (no_env), so the value is a constant the compiler
/// folds, and it leaves out the code that only another setting reaches (a OnceLock's value is
/// read at run time, which kept that code in the binary). The same values, so the same output.
pub(crate) struct NoEnvOnce<T>(std::marker::PhantomData<T>);

impl<T> NoEnvOnce<T> {
    pub(crate) const fn new() -> Self {
        NoEnvOnce(std::marker::PhantomData)
    }
    #[inline(always)]
    pub(crate) fn get_or_init(&self, f: impl FnOnce() -> T) -> NoEnvValue<T> {
        NoEnvValue(f())
    }
}

/// A value of NoEnvOnce::get_or_init (read through `*`, as a OnceLock's reference is).
pub(crate) struct NoEnvValue<T>(T);

impl<T> std::ops::Deref for NoEnvValue<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

/// The float of a text, for derive.rs fwd_float (the engine's port, edit 6: it replaces
/// `str::parse::<f32>` there). fwd_float keeps a float only when java_float writes it back as
/// the same text, and for every such text ('-'?, digits, '.', digits: at most 9 significant
/// digits, |f| in [1e-3, 1e7) or 0) this gives the float std's parser gives; texts of another
/// form get None (never java_float's text, so fwd_float refuses them either way). So the same
/// floats are kept, without std's general float parser (about 14 KB of tables and code).
/// Exact: m / 10^k in f64 (both exact, one correct rounding), then to f32; when that quotient is
/// exactly halfway between two f32s (the one case where rounding twice can differ from
/// rounding once), an exact integer comparison picks the side. Checked against std for all
/// 556,468,260 java_float texts (and in the tests below on a sample).
pub(crate) fn parse_f32(t: &[u8]) -> Option<f32> {
    const P10: [f64; 15] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14];
    let (neg, s) = match t.split_first() {
        Some((b'-', r)) => (true, r),
        _ => (false, t),
    };
    let dot = s.iter().position(|&c| c == b'.')?;
    let (ip, fp) = (&s[..dot], &s[dot + 1..]);
    if ip.is_empty() || fp.is_empty() || ip.len() + fp.len() > 15 {
        return None;
    }
    let mut m = 0u64;
    for &c in ip.iter().chain(fp) {
        if !c.is_ascii_digit() {
            return None;
        }
        m = m * 10 + (c - b'0') as u64;
    }
    let k = fp.len();
    let q = m as f64 / P10[k];
    let mut f = q as f32;
    let b = q.to_bits();
    let e = ((b >> 52) & 0x7ff) as i32 - 1075;
    if b & 0x1fff_ffff == 0x1000_0000 && (-70..0).contains(&e) {
        // q = qm * 2^e, halfway between the f32 `lo` and the next one up
        let qm = ((b & ((1u64 << 52) - 1)) | (1u64 << 52)) as u128;
        let lhs = (m as u128) << (-e) as u32;
        let rhs = qm * 10u128.pow(k as u32);
        let lo = f32::from_bits(((((b >> 52) & 0x7ff) as u32 - 896) << 23) | ((b >> 29) as u32 & 0x7f_ffff));
        if lhs > rhs {
            f = f32::from_bits(lo.to_bits() + 1);
        } else if lhs < rhs {
            f = lo;
        }
    }
    Some(if neg { -f } else { f })
}

#[cfg(test)]
mod parse_f32_tests {
    use super::parse_f32;

    /// java_float's text (derive.rs): Display, plus ".0" when it has no '.'
    fn java_text(f: f32) -> Option<String> {
        let a = f.abs();
        if !f.is_finite() || (a != 0.0 && !(1e-3..1e7).contains(&a)) {
            return None;
        }
        let s = format!("{f}");
        Some(if s.contains('.') { s } else { s + ".0" })
    }

    #[test]
    fn parse_f32_same_as_std() {
        // java_float's texts: every 251st bit pattern, the binade edges and their neighbours
        let mut n = 0;
        let mut bits: Vec<u32> = (0..u32::MAX / 251).map(|i| i * 251).collect();
        for e in 0..256u32 {
            for d in [0u32, 1, 2, 0x7f_fffe, 0x7f_ffff, 0x40_0000] {
                bits.push((e << 23) | d);
                bits.push(0x8000_0000 | (e << 23) | d);
            }
        }
        for b in bits {
            let f = f32::from_bits(b);
            let Some(t) = java_text(f) else { continue };
            n += 1;
            assert_eq!(parse_f32(t.as_bytes()).map(f32::to_bits), Some(b), "{t}");
            assert_eq!(t.parse::<f32>().map(f32::to_bits), Ok(b), "{t}");
        }
        assert!(n > 2_000_000, "{n}");
        // other texts of digits, '.' and '-': fwd_float keeps the same ones with the same float
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let alpha = b"0123456789.-";
        for _ in 0..2_000_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = 1 + (x % 14) as usize;
            let mut r = x;
            let t: Vec<u8> = (0..len)
                .map(|i| {
                    r = r.rotate_left(7) ^ (r >> 3).wrapping_mul(0x2545_f491_4f6c_dd1d);
                    if i > 0 && r % 5 != 0 { b'0' + (r % 10) as u8 } else { alpha[(r % 12) as usize] }
                })
                .collect();
            let keep = |f: Option<f32>| f.filter(|&f| java_text(f).is_some_and(|j| j.as_bytes() == t)).map(f32::to_bits);
            let std = std::str::from_utf8(&t).unwrap().parse::<f32>().ok();
            assert_eq!(keep(parse_f32(&t)), keep(std), "{}", String::from_utf8_lossy(&t));
        }
        for t in ["0.0", "-0.0", "0.001", "9999999.0", "1.0E7", "1e3", ".5", "5.", "-", ".", "", "--1.0", "1.0.0", "0.0000000000001"] {
            let std = t.parse::<f32>().ok().filter(|&f| java_text(f).as_deref() == Some(t)).map(f32::to_bits);
            let ours = parse_f32(t.as_bytes()).filter(|&f| java_text(f).as_deref() == Some(t)).map(f32::to_bits);
            assert_eq!(ours, std, "{t}");
        }
    }
}

/// The engine's stage timer calls clock_gettime_nsec_np, which only macOS has: the same clock
/// for other systems (only the timing changes, never the output).
#[cfg(not(target_vendor = "apple"))]
#[unsafe(no_mangle)]
extern "C" fn clock_gettime_nsec_np(_id: u32) -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}
