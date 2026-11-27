// IDs (format 2, phase B1; a lean port of final:ids, fb_ids/src/ids.rs). In an "ID column"
// (column mode bit 128, allowed only with header flag 32):
// - a new long ID (kind LONG_ID, cache code CK) that Java's java.util.Random made (HDFS block
//   IDs) is written as its step on the generator's loop from the column's last new one,
//   zigzag(step); any other new long ID takes cache code CK + 2 and its value as it is (one
//   number per code, so the header's walk over the numbers stays as it was). Repeats keep
//   their cache codes and their distance in the block's list of long IDs;
// - a UUID (kinds 100 / 101) is a number: 0 a new one (16 bytes), 1 a new version-4 one
//   (random: its 122 random bits), i + 2 entry i of the block's UUID dictionary (in order of
//   first use in the ID columns, line by line). The new ones' bytes end the text stream, after
//   every column's texts: varint(bytes of the 16-byte ones), those, then the 122-bit ones.
// The packer picks the ID columns per block, each column by its own bytes both ways
// (main.rs id_columns); the opener follows the header.
use super::*;

// ---------------------------------------------------------------- java.util.Random
// A 48-bit linear generator: seed = seed * 0x5DEECE66D + 0xB (mod 2^48); nextLong() =
// (next(32) << 32) + next(32). From one nextLong() value the full state follows (the 16
// hidden bits have at most 8 candidates), and every state has a place on the generator's
// single loop of 2^48 states.
const JR_A: u64 = 0x5DEECE66D;
const JR_C: u64 = 0xB;
const JR_M: u64 = (1 << 48) - 1;
const JR_M50: u64 = (1 << 50) - 1;

const fn inv64(x: u64) -> u64 {
    let mut y = x; // Newton: y = 1/x mod 2^64 (x odd)
    let mut i = 0;
    while i < 6 {
        y = y.wrapping_mul(2u64.wrapping_sub(x.wrapping_mul(y)));
        i += 1;
    }
    y
}
const JR_AINV: u64 = inv64(JR_A);

#[inline]
fn jr_step(s: u64) -> u64 {
    s.wrapping_mul(JR_A).wrapping_add(JR_C) & JR_M
}

/// The nextLong() whose first half left the generator in state s.
#[inline]
fn jr_long(s: u64) -> i64 {
    let hi = (s >> 16) as u32 as i32 as i64;
    let lo = (jr_step(s) >> 16) as u32 as i32 as i64;
    (hi << 32).wrapping_add(lo)
}

/// The state behind a nextLong() value, if java.util.Random made it.
fn jr_recover(v: i64) -> Option<u64> {
    let lo = v as i32 as i64;
    let hi = (v.wrapping_sub(lo) >> 32) as u32 as u64;
    let lo32 = lo as u32 as u64;
    let m: u64 = 1 << 48;
    let k = JR_A.wrapping_mul(hi << 16).wrapping_add(JR_C) & JR_M;
    let base0 = ((lo32 << 16) + m - k) & JR_M;
    for w in 0..8u64 {
        let base = base0 + w * m; // < 2^51: a * x for x < 2^16 is below 2^51
        let x = base.div_ceil(JR_A);
        if x >= 1 << 16 {
            break;
        }
        if JR_A * x < base + (1 << 16) {
            let s = (hi << 16) | x;
            if jr_long(s) == v {
                return Some(s);
            }
        }
    }
    None
}

/// The place of state s on the loop (steps from state 0): with y = (a-1)s + c (mod 2^50)
/// every step multiplies y by a, so the place is a 2-adic logarithm, found bit by bit.
fn jr_index(s: u64) -> u64 {
    // 1/a^(2^i) (mod 2^50), made by the compiler
    const INV_POW: [u64; 48] = {
        let mut t = [0u64; 48];
        t[0] = inv64(JR_A) & JR_M50;
        let mut i = 1;
        while i < 48 {
            t[i] = t[i - 1].wrapping_mul(t[i - 1]) & JR_M50;
            i += 1;
        }
        t
    };
    let y = (JR_A - 1).wrapping_mul(s).wrapping_add(JR_C) & JR_M50;
    let mut t = y.wrapping_mul(inv64(JR_C) & JR_M50) & JR_M50;
    let mut n = 0u64;
    for (i, ip) in INV_POW.iter().enumerate() {
        if (t >> (i + 2)) & 1 == 1 {
            t = t.wrapping_mul(*ip) & JR_M50;
            n |= 1 << i;
        }
    }
    n
}

/// The state n steps on from s (n < 0: back).
fn jr_jump(s: u64, n: i64) -> u64 {
    if (0..=8).contains(&n) {
        let mut s = s;
        for _ in 0..n {
            s = jr_step(s);
        }
        return s;
    }
    let (mut a, mut c) = if n >= 0 { (JR_A, JR_C) } else { (JR_AINV, JR_AINV.wrapping_mul(JR_C).wrapping_neg()) };
    let (mut ra, mut rc) = (1u64, 0u64);
    let mut k = n.unsigned_abs();
    while k > 0 {
        if k & 1 == 1 {
            rc = a.wrapping_mul(rc).wrapping_add(c);
            ra = a.wrapping_mul(ra);
        }
        c = a.wrapping_mul(c).wrapping_add(c);
        a = a.wrapping_mul(a);
        k >>= 1;
    }
    ra.wrapping_mul(s).wrapping_add(rc) & JR_M
}


/// The value (magnitude * 2 + sign, as the splitter gives it) of a nextLong() value.
#[inline]
fn signed_id(x: i64) -> u64 {
    (x.unsigned_abs() << 1) | (x < 0) as u64
}

/// Packing: a column's last new generator ID (its place on the loop and its state).
#[derive(Default)]
pub struct GenW {
    place: u64,
}

impl GenW {
    /// The step form of a new long ID v (zigzag(step)) when java.util.Random made it.
    pub fn form(&mut self, v: u64) -> Option<u64> {
        let m = (v >> 1) as i64; // below 2^63
        let x = if v & 1 == 1 { m.wrapping_neg() } else { m };
        let s2 = jr_recover(x)?;
        if signed_id(x) != v {
            return None; // ("-0": not a value the generator gives back)
        }
        // the step by the places on the loop (most new IDs are far from the last one: HDFS's
        // ids come out of order, so no search of the next few states first)
        let p2 = jr_index(s2);
        let dd = p2.wrapping_sub(self.place) & JR_M;
        let d = if dd >= 1 << 47 { dd as i64 - (1i64 << 48) } else { dd as i64 };
        self.place = p2;
        Some(zz(d))
    }
}

/// Opening: the new long ID of step form x after the column's last state *s (updated).
#[inline(never)]
pub fn gen_value(s: &mut u64, x: u64) -> u64 {
    let s2 = jr_jump(*s, unzz(x));
    *s = s2;
    signed_id(jr_long(s2))
}

/// A UUID's 32 hex digits (dashes left out) as one number.
pub fn uuid_bits(t: &[u8]) -> u128 {
    let mut x = 0u128;
    for &c in t {
        if c != b'-' {
            x = x << 4 | hexval(c) as u128;
        }
    }
    x
}

/// A version-4 UUID (random): version nibble 4, variant bits 10.
#[inline]
pub fn uuid_v4(x: u128) -> bool {
    (x >> 76) & 15 == 4 && (x >> 62) & 3 == 2
}

/// Packing: the new UUIDs' bytes (the text stream's end): 16 bytes each, and the 122 random
/// bits of each version-4 one.
#[derive(Default)]
pub struct Utail {
    a: Vec<u8>,
    b: Vec<u8>,
    acc: u128,
    n: u32,
}

impl Utail {
    /// A new UUID; returns its number (0, or 1 for version 4).
    pub fn push(&mut self, x: u128) -> u64 {
        if !uuid_v4(x) {
            self.a.extend_from_slice(&x.to_be_bytes());
            return 0;
        }
        // the 122 random bits (version and variant bits left out), in two parts
        let p = ((x >> 80) << 74) | (((x >> 64) & 0xFFF) << 62) | (x & ((1u128 << 62) - 1));
        for (v, k) in [((p >> 64) as u64, 58), (p as u64, 64)] {
            self.acc = (self.acc << k) | v as u128;
            self.n += k;
            while self.n >= 8 {
                self.n -= 8;
                self.b.push((self.acc >> self.n) as u8);
            }
        }
        1
    }
    pub fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.b.push((self.acc << (8 - self.n)) as u8);
        }
        let mut out = Vec::with_capacity(self.a.len() + self.b.len() + 4);
        put_varint(&mut out, self.a.len() as u64);
        out.extend_from_slice(&self.a);
        out.extend_from_slice(&self.b);
        out
    }
}

/// Opening: the ID columns' UUIDs, the block's dictionary filled from the text stream's end
/// (Utail). A number past the dictionary, or bytes that run out, are damage (`bad`).
pub struct Udict<'a> {
    d: Vec<u128>,
    a: &'a [u8],
    ap: usize,
    b: &'a [u8],
    nb: usize, // bits read from b
    bad: bool,
}

impl<'a> Udict<'a> {
    /// From the text stream's end (None: its length does not fit).
    pub fn new(t: &'a [u8]) -> Option<Udict<'a>> {
        // (no ID columns: nothing; heads::read checked that their blocks have the length)
        let mut p = 0;
        let n = if t.is_empty() { 0 } else { heads::rd(t, &mut p)? as usize };
        let a = t.get(p..p.checked_add(n)?)?;
        if n % 16 != 0 {
            return None;
        }
        Some(Udict { d: Vec::new(), a, ap: 0, b: &t[p + n..], nb: 0, bad: false })
    }
    /// The UUID of number x.
    #[inline(never)]
    pub fn get(&mut self, x: u64) -> u128 {
        if x >= 2 {
            return match self.d.get(x as usize - 2) {
                Some(&u) => u,
                None => {
                    self.bad = true;
                    0
                }
            };
        }
        let u = if x == 0 {
            let b = self.a.get(self.ap..self.ap + 16);
            self.ap += 16;
            b.map_or(0, |b| u128::from_be_bytes(b.try_into().unwrap()))
        } else {
            // 122 bits from bit nb of b (zeros past its end), then the version and variant bits
            let (i, sh) = (self.nb >> 3, self.nb & 7);
            let mut w = [0u8; 17];
            let r = self.b.get(i..).unwrap_or(&[]);
            let k = r.len().min(17);
            w[..k].copy_from_slice(&r[..k]);
            let p = ((u128::from_be_bytes(w[..16].try_into().unwrap()) << sh) | (w[16] as u128) << sh >> 8) >> 6;
            self.nb += 122;
            ((p >> 74) << 80) | (4u128 << 76) | (((p >> 62) & 0xFFF) << 64) | (2u128 << 62) | (p & ((1u128 << 62) - 1))
        };
        self.d.push(u);
        u
    }
    /// After the block: every byte used (none past the ends), no damage.
    pub fn ok(&self) -> bool {
        !self.bad && self.ap == self.a.len() && self.nb.div_ceil(8) == self.b.len()
    }
}

/// Each UUID's number in order of first use (packing): a map from a 64-bit hash of it to its
/// number. The hash mixes all 128 bits with the run's random seed (two rounds of fmix64), so
/// crafted UUIDs can plan neither equal hashes nor hashes that crowd one place of the map:
/// packing time stays linear (the 1.2.0 fold let 90,000 crafted UUIDs walk 4 billion steps of
/// a chain). A UUID whose hash an earlier, different one holds (by chance: 2^-64 a pair) takes
/// a new number and the hash's place: always exact (the earlier one, met again, is new again),
/// at most a few bytes bigger.
#[derive(Default)]
pub struct Unum {
    map: FxMap<u64, u32>,
    vals: Vec<u128>,
}

impl Unum {
    /// x's number (new ones get the next).
    pub fn get(&mut self, x: u128) -> u32 {
        let n = self.vals.len() as u32;
        let mix = |v: u64| {
            let v = (v ^ v >> 33).wrapping_mul(0xff51_afd7_ed55_8ccd);
            let v = (v ^ v >> 33).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
            v ^ v >> 33
        };
        let e = self.map.entry(mix(mix(x as u64 ^ crate::engine::FxBuild::default().0) ^ (x >> 64) as u64)).or_insert(n);
        if self.vals.get(*e as usize) != Some(&x) {
            *e = n;
            self.vals.push(x);
        }
        *e
    }
    pub fn len(&self) -> usize {
        self.vals.len()
    }
}

// ---------------------------------------------------------------- refuter's checks (tests only)
#[cfg(test)]
mod hostile_tests {
    use super::*;
    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }
    fn uuid(x: &mut u64, v4: bool) -> u128 {
        let u = (rng(x) as u128) << 64 | rng(x) as u128;
        if v4 { u & !(0xF << 76) & !(3 << 62) | 4 << 76 | 2 << 62 } else { u }
    }

    /// Unum: numbers in order of first use (against a plain search), for random UUIDs with
    /// repeats and for 60,000 crafted ones that 1.2.0's fold put in one chain (hi ^ lo rotated
    /// by 29 the same: 1.8 billion chain steps then; now a moment).
    #[test]
    fn unum_numbers_and_crafted_folds() {
        let mut x = 0x0bad_5eed_1234_5678u64;
        let mut u = Unum::default();
        let mut seen: Vec<u128> = Vec::new();
        for _ in 0..3000 {
            let r = rng(&mut x);
            let v = if !seen.is_empty() && r % 3 == 0 { seen[(r >> 8) as usize % seen.len()] } else { uuid(&mut x, r % 2 == 0) };
            let want = seen.iter().position(|&s| s == v).unwrap_or(seen.len());
            if want == seen.len() {
                seen.push(v);
            }
            assert_eq!(u.get(v) as usize, want);
        }
        assert_eq!(u.len(), seen.len());
        let mut u = Unum::default();
        let t = std::time::Instant::now();
        for i in 0..60_000u64 {
            let hi = rng(&mut x);
            let lo = (0x0123_4567_89ab_cdefu64 ^ hi).rotate_right(29);
            let v = (hi as u128) << 64 | lo as u128;
            assert_eq!(u.get(v) as u64, i);
            assert_eq!(u.get(v) as u64, i);
        }
        assert!(t.elapsed().as_secs() < 20, "crafted UUIDs: {:?}", t.elapsed());
    }

    /// Utail -> Udict: the same UUIDs back (new ones and dictionary places), every byte used.
    #[test]
    fn udict_round_trip() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..2000 {
            let n = (rng(&mut x) % 300) as usize;
            let mut t = Utail::default();
            let mut seen: Vec<u128> = Vec::new();
            let mut nums = Vec::new();
            for _ in 0..n {
                let r = rng(&mut x);
                if !seen.is_empty() && r % 3 == 0 {
                    let i = (r >> 8) as usize % seen.len();
                    nums.push((i + 2) as u64);
                } else {
                    let u = uuid(&mut x, r % 4 != 1 || round % 7 == 0);
                    nums.push(t.push(u));
                    seen.push(u);
                }
            }
            let tail = t.finish();
            let mut d = Udict::new(&tail).expect("tail");
            let mut k = 0;
            for &v in &nums {
                let u = d.get(v);
                if v < 2 {
                    assert_eq!(u, seen[k], "round {round}");
                    k += 1;
                } else {
                    assert_eq!(u, seen[v as usize - 2]);
                }
            }
            assert!(d.ok(), "round {round}");
        }
    }

    /// Hostile tails and numbers: never a panic; numbers past the dictionary, bytes that run out
    /// or bytes left over are reported by ok(); the dictionary grows by at most one per call.
    #[test]
    fn udict_hostile() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        // every tail of 0-2 bytes, then random and damaged valid ones
        let mut tails: Vec<Vec<u8>> = vec![Vec::new()];
        for a in 0..=255u8 {
            tails.push(vec![a]);
            for b in [0u8, 1, 15, 16, 17, 0x7F, 0x80, 0xFF] {
                tails.push(vec![a, b]);
            }
        }
        for _ in 0..3000 {
            let mut t = Utail::default();
            let n = rng(&mut x) % 12;
            for _ in 0..n {
                let r = rng(&mut x);
                t.push(uuid(&mut x, r % 2 == 0));
            }
            let mut v = t.finish();
            let r = rng(&mut x);
            match r % 6 {
                0 => v.truncate((r >> 8) as usize % (v.len() + 1)),
                1 => v.extend(std::iter::repeat_n((r >> 16) as u8, 1 + (r >> 24) as usize % 20)),
                2 => {
                    // crossed counts: the 16-byte part's length changed
                    let mut p = 0;
                    let m = heads::rd(&v, &mut p).unwrap();
                    let m2 = [m.wrapping_add(16), m.wrapping_sub(16), m + 1, m.saturating_sub(1), 1 << 40, u64::MAX, 0][(r >> 8) as usize % 7];
                    let mut w = Vec::new();
                    put_varint(&mut w, m2);
                    w.extend_from_slice(&v[p..]);
                    v = w;
                }
                3 if !v.is_empty() => {
                    let i = (r >> 8) as usize % v.len();
                    v[i] ^= 1 << ((r >> 40) % 8);
                }
                _ => {}
            }
            tails.push(v);
        }
        let mut accepted = 0;
        for t in &tails {
            let Some(mut d) = Udict::new(t) else { continue };
            accepted += 1;
            let calls = (rng(&mut x) % 40) as usize;
            let mut bad_rank = false;
            for _ in 0..calls {
                let r = rng(&mut x);
                let v = match r % 6 {
                    0 => 0,
                    1 => 1,
                    2 => 2 + d.d.len() as u64, // just past the dictionary
                    3 => [u64::MAX, u64::MAX - 1, 1 << 63, 1 << 32][(r >> 8) as usize % 4],
                    _ if d.d.is_empty() => 0,
                    _ => 2 + (r >> 8) % d.d.len() as u64,
                };
                if v >= 2 && v - 2 >= d.d.len() as u64 {
                    bad_rank = true;
                }
                let before = d.d.len();
                let _ = d.get(v);
                assert!(d.d.len() <= before + 1);
            }
            if bad_rank {
                assert!(!d.ok(), "a number past the dictionary passed");
            }
            // bytes past the ends are never "used exactly"
            if d.ap > d.a.len() || d.nb.div_ceil(8) > d.b.len() {
                assert!(!d.ok());
            }
        }
        assert!(accepted > 1000);
        // the 16-byte part: its length must be a multiple of 16 and fit
        assert!(Udict::new(&[1, 0]).is_none());
        assert!(Udict::new(&[16, 0, 0]).is_none());
        assert!(Udict::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]).is_none());
    }

    /// The generator: any step form opens without a panic and quickly; real nextLong() values
    /// written as steps (any order, repeats) open back exactly; jr_recover never claims a value
    /// it did not find.
    #[test]
    fn generator_forms() {
        let mut x = 0x1234_5678_9abc_def1u64;
        for _ in 0..20000 {
            let mut s = rng(&mut x) & JR_M;
            let f = match rng(&mut x) % 4 {
                0 => u64::MAX,
                1 => u64::MAX - 1,
                2 => rng(&mut x) >> (rng(&mut x) % 64),
                _ => rng(&mut x),
            };
            let v = gen_value(&mut s, f);
            assert!(s <= JR_M);
            let _ = v;
            let _ = jr_recover(rng(&mut x) as i64);
            let w = rng(&mut x) as i64;
            if let Some(st) = jr_recover(w) {
                assert_eq!(jr_long(st), w);
            }
        }
        for seed in [0u64, 1, 0x5DEECE66D, JR_M, 0xdead_beef] {
            let mut st = seed & JR_M;
            let mut vals = Vec::new();
            for _ in 0..500 {
                st = jr_step(st);
                let hi = jr_long(st);
                st = jr_step(st);
                vals.push(hi);
            }
            // out of order, with repeats
            let mut order: Vec<usize> = (0..vals.len()).collect();
            for i in (1..order.len()).rev() {
                let j = rng(&mut x) as usize % (i + 1);
                order.swap(i, j);
            }
            let mut w = GenW::default();
            let mut s = 0u64;
            for &i in &order {
                let v = signed_id(vals[i]);
                if let Some(f) = w.form(v) {
                    assert_eq!(gen_value(&mut s, f), v);
                }
            }
        }
        assert_eq!(jr_index(0), 0);
        for _ in 0..1000 {
            let s = rng(&mut x) & JR_M;
            let n = (rng(&mut x) % (1 << 20)) as i64 - (1 << 19);
            assert_eq!(jr_jump(jr_jump(s, n), -n), s);
            assert_eq!(jr_index(jr_jump(s, n)).wrapping_sub(jr_index(s)) & JR_M, (n as u64) & JR_M);
        }
    }
}
