// Fast line splitting (from the splitspeed idea), with the combined engine's rules: the same
// shape and values as split_line (main.rs, kept as the reference; FL_CHECK=1 compares them on
// every line), found faster:
// - each line is scanned 64 bytes at a time (NEON on ARM, plain code elsewhere) into bit masks
//   of digits, spaces, hex letters, dashes, capitals, letters and 0 bytes; the line is read in
//   place (with up to ROOM bytes after its end) and pieces are copied 16 bytes at a time,
// - words without a digit are copied as they are, except month / weekday names (found from
//   the capitals at word starts and a 3-letter key, then checked by name_at),
// - a word with a digit and no run of 8+ hex characters holding a letter and no UUID-like
//   dashes (most words) has only digit runs as values (some read as IPv4 addresses, clock
//   times or long IDs): found from the masks; other words go through the old per-character
//   rules (split_word); so do words that may hold a short hex number (5-7 hex digits between
//   signs, HEXS),
// - a number starting a word after two or more spaces takes the spaces beyond the first (SPN);
//   lines without two spaces in a row or a hex letter next to a digit skip those two rules.
use super::*;

/// Bytes that may be read after a line's end.
pub const ROOM: usize = 64;

/// Bit masks for 64 bytes (bit i = byte i): digits, spaces, hex letters (a-f, A-F), dashes,
/// capitals (A-Z), letters (a-z, A-Z), 0 bytes.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn masks64(b: &[u8]) -> [u64; 7] {
    use std::arch::aarch64::*;
    let b: &[u8; 64] = b[..64].try_into().unwrap();
    // SAFETY: NEON is always present on aarch64; the four loads stay inside the 64-byte array.
    unsafe {
        let p = b.as_ptr();
        let v = [vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(32)), vld1q_u8(p.add(48))];
        // one bit per byte: keep bit (i mod 8) of each 0/FF lane, then add neighbours 3 times
        let bits = |f: &dyn Fn(uint8x16_t) -> uint8x16_t| -> u64 {
            let m = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
            let (a, b, c, d) = (vandq_u8(f(v[0]), m), vandq_u8(f(v[1]), m), vandq_u8(f(v[2]), m), vandq_u8(f(v[3]), m));
            let s = vpaddq_u8(vpaddq_u8(a, b), vpaddq_u8(c, d));
            vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
        };
        let digits = bits(&|x| vcltq_u8(vsubq_u8(x, vdupq_n_u8(b'0')), vdupq_n_u8(10)));
        let spaces = bits(&|x| vceqq_u8(x, vdupq_n_u8(b' ')));
        let hexl = bits(&|x| vcltq_u8(vsubq_u8(vorrq_u8(x, vdupq_n_u8(0x20)), vdupq_n_u8(b'a')), vdupq_n_u8(6)));
        let dashes = bits(&|x| vceqq_u8(x, vdupq_n_u8(b'-')));
        let caps = bits(&|x| vcltq_u8(vsubq_u8(x, vdupq_n_u8(b'A')), vdupq_n_u8(26)));
        let letters = bits(&|x| vcltq_u8(vsubq_u8(vorrq_u8(x, vdupq_n_u8(0x20)), vdupq_n_u8(b'a')), vdupq_n_u8(26)));
        // 0 bytes are rare: their mask only when the smallest byte is 0
        let min = vminvq_u8(vminq_u8(vminq_u8(v[0], v[1]), vminq_u8(v[2], v[3])));
        let zeros = if min == 0 { bits(&|x| vceqq_u8(x, vdupq_n_u8(0))) } else { 0 };
        [digits, spaces, hexl, dashes, caps, letters, zeros]
    }
}

/// The same with SSE2 (x86_64, x86.rs).
#[cfg(all(target_arch = "x86_64", not(fewer_simd = "scalar")))]
#[inline(always)]
fn masks64(b: &[u8]) -> [u64; 7] {
    let b: &[u8; 64] = b[..64].try_into().unwrap();
    // SAFETY: the loads read the 64 bytes of b
    unsafe { super::x86::split_masks(b.as_ptr()) }
}

/// The same without NEON or SSE2.
#[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", not(fewer_simd = "scalar")))))]
#[inline(always)]
fn masks64(b: &[u8]) -> [u64; 7] {
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
#[inline]
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

/// Copy src[s..s + n] to dst[d..]: 16 bytes at a time, so up to 15 bytes more are read and
/// written (both buffers have room after their ends for this).
#[inline(always)]
pub fn wild_copy(dst: &mut [u8], d: usize, src: &[u8], s: usize, n: usize) {
    let mut p = 0;
    while p < n {
        dst[d + p..d + p + 16].copy_from_slice(&src[s + p..s + p + 16]);
        p += 16;
    }
}

/// The number written in the 1-8 digits l[a..a + w]; reads l[a..a + 8].
#[inline(always)]
fn digits8(l: &[u8], a: usize, w: usize) -> u64 {
    // the digits' values, the first in the lowest byte, moved up so that 8 - w zeros lead
    let x = u64::from_le_bytes(l[a..a + 8].try_into().unwrap()).wrapping_sub(0x3030_3030_3030_3030) << (8 * (8 - w));
    // neighbours to 2-digit numbers, then 4, then 8 (the known multiply trick)
    let x = x.wrapping_mul(10).wrapping_add(x >> 8);
    let m = 0x0000_00FF_0000_00FF;
    ((x & m).wrapping_mul(100 + (1_000_000 << 32)).wrapping_add((x >> 16 & m).wrapping_mul(1 + (10_000 << 32)))) >> 32
}

/// The number written in the 1-24 digits l[a..a + w] (wrapping like the old code above 2^64,
/// which only matters for digits that stay text); reads up to l[a + w + 8].
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

/// The bits of runs of 5+ set bits in x.
#[inline(always)]
fn runs5(x: u128) -> u128 {
    let r2 = x & x >> 1;
    let r5 = r2 & r2 >> 2 & x >> 4; // bit k: x has bits k..k+4
    let c2 = r5 | r5 << 1;
    c2 | c2 << 2 | r5 << 4
}

/// Whether a run of 5+ hex characters (x) not next to another letter (nh) may hold both a hex
/// letter (h) and a digit (d), so be a short hex number (HEXS, found by split_word): such runs
/// start and end away from other letters and have a letter next to a digit.
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

/// Positions of hex characters (x) with dashes 8, 13, 18 and 23 bytes later: where a UUID
/// could start.
#[inline(always)]
fn uuid_starts(x: u128, dash: u128) -> u128 {
    x & dash >> 8 & dash >> 13 & dash >> 18 & dash >> 23
}

/// Whether a word of len < 64 bytes (d, h, dash, nh: its digit, hex-letter, dash and other
/// letter masks) can hold neither a hex value nor a UUID (nor, with hex5, a short hex number),
/// so that its values are just its digit runs.
#[inline(always)]
fn only_numbers(len: usize, d: u64, h: u64, dash: u64, nh: u64, hex5: bool) -> bool {
    let x = d | h;
    if long_runs(x as u128) & h as u128 != 0 || (hex5 && hex5_maybe(x as u128, h as u128, d as u128, nh as u128)) {
        return false;
    }
    len < 36 || uuid_starts(x as u128, dash as u128) & ((1u64 << (len - 35)) - 1) as u128 == 0
}

/// Splits lines into shape + values; buffers are kept from line to line.
#[derive(Default)]
pub struct Splitter {
    // the line's masks, 64 bytes per word (one word more, always 0, at the end)
    dg: Vec<u64>,
    sp: Vec<u64>,
    hx: Vec<u64>,
    ds: Vec<u64>,
    cand: Vec<u64>, // capitals where a month / weekday name could start
    nh: Vec<u64>,   // letters that are not hex digits
    caps: Vec<u64>,
    lower: Vec<u64>,
    pad: Vec<u8>,   // a line copied with room after it (only near the end of the data)
    pub shape: Vec<u8>, // the last line's shape: shape[..shape_len] (and room after it)
    pub vals: Vec<Val>, // its values: vals[..nvals]
    pub shape_len: usize,
    pub nvals: usize,
}

impl Splitter {
    /// Split one line: data[at..at + n]. Reads up to ROOM bytes after the line (from data, or
    /// from a padded copy at its end).
    #[inline(always)]
    pub fn split(&mut self, data: &[u8], at: usize, n: usize, cfg: &SplitCfg) {
        // a shape is never longer than its line (a value of 1+ bytes becomes one mark)
        if self.shape.len() < n + ROOM {
            self.shape.resize(n + ROOM, 0);
            self.vals.resize(n + 1, (0, 0, 0, 0));
        }
        let l: &[u8] = if at + n + ROOM <= data.len() {
            &data[at..at + n + ROOM]
        } else {
            self.pad.clear();
            self.pad.extend_from_slice(&data[at..at + n]);
            self.pad.resize(n + ROOM, 0xFF);
            &self.pad
        };
        // the masks, 64 bytes per word; bits after the line end cleared
        let nw = n.div_ceil(64);
        if self.dg.len() < nw + 1 {
            for m in [&mut self.dg, &mut self.sp, &mut self.hx, &mut self.ds, &mut self.cand, &mut self.caps, &mut self.lower, &mut self.nh] {
                m.resize(nw + 1, 0);
            }
        }
        let mut zero = 0;
        // two spaces in a row anywhere (space-padded numbers), a hex letter next to a digit
        // anywhere (short hex numbers): else those rules are skipped for this line
        let (mut dsp, mut mix, mut top) = (0u64, 0u64, (0u64, 0u64, 0u64));
        for w in 0..nw {
            let [d, s, h, ds, cap, let_, z] = masks64(&l[64 * w..]);
            let keep = if w + 1 == nw && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 };
            let (d, s, h) = (d & keep, s & keep, h & keep);
            self.dg[w] = d;
            self.sp[w] = s;
            self.hx[w] = h;
            self.ds[w] = ds & keep;
            self.nh[w] = let_ & !h & keep;
            zero |= z & keep;
            dsp |= s & (s << 1 | top.0);
            mix |= h & (d << 1 | top.1) | d & (h << 1 | top.2);
            top = (s >> 63, d >> 63, h >> 63);
            if cfg.names {
                self.caps[w] = cap & keep;
                self.lower[w] = let_ & !cap & keep;
                // signs: neither digit, space nor letter (kept in cand for now)
                self.cand[w] = !(d | s | let_) & keep;
            }
        }
        (self.dg[nw], self.sp[nw], self.hx[nw], self.ds[nw], self.cand[nw], self.caps[nw], self.lower[nw], self.nh[nw]) = (0, 0, 0, 0, 0, 0, 0, 0);
        let cfg = &SplitCfg { spaced: cfg.spaced && dsp != 0, hex5: cfg.hex5 && mix != 0, ..*cfg };
        if zero != 0 {
            // raw line: template 0 = one text value
            self.shape[0] = MARK;
            self.vals[0] = (1, 0, 0, n as u32);
            (self.shape_len, self.nvals) = (1, 1);
            return;
        }
        // name candidates: a capital followed by two small letters, at a word start or right
        // after one sign that starts a word (the two bytes before the line count as spaces)
        let mut names = false;
        if cfg.names {
            let (mut psp, mut psg) = (3u64, 0u64); // the word before's last 2 space bits / last sign bit
            for w in 0..nw {
                let (s, sg) = (self.sp[w], self.cand[w]);
                let lw = self.lower[w] as u128 | (self.lower[w + 1] as u128) << 64;
                let low2 = (lw >> 1 & lw >> 2) as u64;
                let sp1 = s << 1 | psp >> 1;
                let sp2 = s << 2 | psp;
                let sg1 = sg << 1 | psg;
                let c = self.caps[w] & low2 & (sp1 | sg1 & sp2);
                self.cand[w] = c;
                names |= c != 0;
                psp = s >> 62;
                psg = sg >> 63;
            }
        }
        let (dg, sp, hx, ds, cand, nh) = (&self.dg[..nw + 1], &self.sp[..nw + 1], &self.hx[..nw + 1], &self.ds[..nw + 1], &self.cand[..nw + 1], &self.nh[..nw + 1]);
        let (shape, vals) = (&mut self.shape[..], &mut self.vals[..]);
        if only_numbers_line(dg, hx, ds, nh, nw, cfg.hex5) {
            (self.shape_len, self.nvals) = split_numbers_line(l, n, dg, sp, if names { Some(cand) } else { None }, shape, vals, cfg);
            return;
        }
        let (mut sl, mut nv) = (0, 0);
        let mut done = 0; // l[..done] is in the shape
        let mut cpos = 0; // the next name candidate is looked for from here
        loop {
            let d = next_one(dg, done);
            if names {
                if let Some(c) = next_one(cand, cpos.max(done)) {
                    if d.map_or(true, |d| c < d) {
                        cpos = c + 1;
                        if let Some((sl2, nv2, e)) = name_word(l, n, c, sp, dg, shape, sl, vals, nv, done) {
                            (sl, nv, done) = (sl2, nv2, e);
                        }
                        continue;
                    }
                }
            }
            let Some(p) = d else { break };
            // the word around the next digit: from after the space before it to the space after it
            let i = space_before(sp, p).map_or(0, |s| s + 1);
            let j = space_after(sp, p, n);
            wild_copy(shape, sl, l, done, i - done);
            sl += i - done;
            (sl, nv) = if j - i < 64 && only_numbers(j - i, bits_at(dg, i, j - i), bits_at(hx, i, j - i), bits_at(ds, i, j - i), bits_at(nh, i, j - i), cfg.hex5) {
                split_numbers(l, i, j, bits_at(dg, i, j - i), shape, sl, vals, nv, cfg)
            } else {
                split_word(l, i, j, shape, sl, vals, nv, cfg)
            };
            done = j;
        }
        wild_copy(shape, sl, l, done, n - done);
        (self.shape_len, self.nvals) = (sl + n - done, nv);
    }
}

/// The same test as only_numbers for a whole line (masks of nw words + a 0 word): true when no
/// word of it can hold a hex value, a UUID or (with hex5) a short hex number (a few lines that
/// could are sent the slow way).
#[inline(always)]
fn only_numbers_line(dg: &[u64], hx: &[u64], ds: &[u64], nh: &[u64], nw: usize, hex5: bool) -> bool {
    for w in 0..nw {
        // this word and the next one: runs and UUIDs may cross from one into the other
        let two = |m: &[u64]| m[w] as u128 | (m[w + 1] as u128) << 64;
        let (x, h) = (two(dg) | two(hx), two(hx));
        if long_runs(x) & h | uuid_starts(x, two(ds)) & u64::MAX as u128 != 0 || (hex5 && hex5_maybe(x, h, two(dg), two(nh))) {
            return false;
        }
    }
    true
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

/// A line that passed only_numbers_line: its values are its digit runs, some read as IPv4
/// addresses, clock times or long IDs, and month / weekday names (cand: their candidates);
/// everything else is copied to the shape. Same result as going word by word, without finding
/// the words. Returns (shape length, number of values).
#[inline(always)]
fn split_numbers_line(l: &[u8], n: usize, dg: &[u64], sp: &[u64], cand: Option<&[u64]>, shape: &mut [u8], vals: &mut [Val], cfg: &SplitCfg) -> (usize, usize) {
    let (mut sl, mut nv) = (0, 0);
    let mut done = 0; // l[..done] is done
    let mut cpos = 0;
    loop {
        let d = next_one(dg, done);
        if let Some(cm) = cand {
            if let Some(c) = next_one(cm, cpos.max(done)) {
                if d.map_or(true, |d| c < d) {
                    cpos = c + 1;
                    if let Some((sl2, nv2, e)) = name_word(l, n, c, sp, dg, shape, sl, vals, nv, done) {
                        (sl, nv, done) = (sl2, nv2, e);
                    }
                    continue;
                }
            }
        }
        let Some(a) = d else { break };
        let e = next_zero(dg, a);
        wild_copy(shape, sl, l, done, a - done);
        sl += a - done;
        let w = e - a;
        let next = if e < n { l[e] } else { b' ' };
        let smart = if (next == b'.' && w <= 3) || (next == b':' && w <= 2) { smart_at(l, a, w, next, space_after(sp, a, n), cfg) } else { None };
        let v = match smart {
            Some((kind, x, end)) => (kind, x, a as u32, end as u32),
            None => digit_value(l, a, e, &mut sl, cfg),
        };
        shape[sl] = MARK;
        sl += 1;
        vals[nv] = v;
        nv += 1;
        done = v.3 as usize;
    }
    wild_copy(shape, sl, l, done, n - done);
    (sl + n - done, nv)
}

/// A name candidate (a capital) at c: when its word has no digit and name_at finds a month or
/// weekday name there, the text before it and a NAMEK value go to the shape; returns the new
/// ends and the end of the name.
#[inline(never)]
fn name_word(l: &[u8], n: usize, c: usize, sp: &[u64], dg: &[u64], shape: &mut [u8], mut sl: usize, vals: &mut [Val], nv: usize, done: usize) -> Option<(usize, usize, usize)> {
    // the 3-letter key first: almost every capital fails here
    if c + 3 > n {
        return None;
    }
    let key = l[c] as u32 | (l[c + 1] as u32) << 8 | (l[c + 2] as u32) << 16;
    let k = NAME_SLOT[(key.wrapping_mul(NAME_MUL) >> 27) as usize] as usize;
    if k == 0 || NAMES[k - 1][..] != l[c..c + 3] {
        return None;
    }
    let i = if c == 0 || l[c - 1] == b' ' { c } else { c - 1 };
    let j = space_after(sp, c, n);
    if i < done || next_one(dg, i).is_some_and(|x| x < j) {
        return None; // a word with a digit: no names there
    }
    let (p, q, x) = name_at(l, i, j)?;
    wild_copy(shape, sl, l, done, p - done);
    sl += p - done;
    shape[sl] = MARK;
    vals[nv] = (NAMEK, x, p as u32, q as u32);
    Some((sl + 1, nv + 1, q))
}

/// A digit run l[a..e] read as a plain number, a long ID or text (it is not an IPv4 address or
/// a clock time); `sl` is the shape's end (a '-' just before a long ID is taken into it).
#[inline(always)]
fn digit_value(l: &[u8], a: usize, e: usize, sl: &mut usize, cfg: &SplitCfg) -> Val {
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
        let p = if cfg.spaced { space_pad(l, a, w) } else { 0 };
        *sl -= p;
        (if p > 0 { SPN + (p + w) as u8 } else { 0 }, v, (a - p) as u32, e as u32)
    }
}

/// An IPv4 address, a dotted or a colon clock time starting at the digit run l[a..a + w] (next:
/// the byte after the run) in the word ending at j: (kind, value, end).
#[inline(always)]
fn smart_at(l: &[u8], a: usize, w: usize, next: u8, j: usize, cfg: &SplitCfg) -> Option<(u8, u64, usize)> {
    if !cfg.use_smart {
        return None;
    }
    if next == b'.' && w <= 3 {
        if let Some((e, v)) = ipv4_at(l, a, j) {
            return Some((30, v, e));
        }
        if cfg.dotted {
            if let Some((e, kind, v)) = dotted_time_at(l, a, j) {
                return Some((kind, v, e));
            }
        }
    } else if next == b':' && w <= 2 {
        if let Some((e, kind, v)) = time_at(l, a, j) {
            return Some((kind, v, e));
        }
        if cfg.loose {
            if let Some((e, v)) = loose_time_at(l, a, j) {
                return Some((68, v, e));
            }
        }
    }
    None
}

/// A word l[i..j] shorter than 64 bytes that passed only_numbers: its values are its digit runs
/// (d: their mask from i on), some of them read as IPv4 addresses, clock times or long IDs.
/// Same result as split_word, found faster.
#[inline(always)]
fn split_numbers(l: &[u8], i: usize, j: usize, d: u64, shape: &mut [u8], mut sl: usize, vals: &mut [Val], mut nv: usize, cfg: &SplitCfg) -> (usize, usize) {
    let mut k = i; // l[i..k] is done
    let mut dm = d;
    while dm != 0 {
        let a = i + dm.trailing_zeros() as usize;
        let e = a + (!(d >> (a - i))).trailing_zeros() as usize;
        wild_copy(shape, sl, l, k, a - k);
        sl += a - k;
        let w = e - a;
        let next = if e < j { l[e] } else { b' ' };
        let v = match smart_at(l, a, w, next, j, cfg) {
            Some((kind, x, end)) => (kind, x, a as u32, end as u32),
            None => digit_value(l, a, e, &mut sl, cfg),
        };
        shape[sl] = MARK;
        sl += 1;
        vals[nv] = v;
        nv += 1;
        k = v.3 as usize;
        dm = if k - i >= 64 { 0 } else { d & (!0u64 << (k - i)) };
    }
    wild_copy(shape, sl, l, k, j - k);
    (sl + j - k, nv)
}

/// A word with a digit, l[i..j]: exactly the per-character rules of split_line (IPv4, clock
/// times, UUID or hex run, long ID, number).
#[inline(never)]
fn split_word(l: &[u8], i: usize, j: usize, shape: &mut [u8], mut sl: usize, vals: &mut [Val], mut nv: usize, cfg: &SplitCfg) -> (usize, usize) {
    let mut k = i;
    while k < j {
        let ck = CLS[l[k] as usize];
        if ck == 1 {
            let mut e = k;
            while e < j && e - k < 4 && CLS[l[e] as usize] == 1 {
                e += 1;
            }
            let nx = if e < j && e - k < 4 { l[e] } else { 0 };
            if let Some((kind, v, end)) = smart_at(l, k, e - k, nx, j, cfg) {
                shape[sl] = MARK;
                sl += 1;
                vals[nv] = (kind, v, k as u32, end as u32);
                nv += 1;
                k = end;
                continue;
            }
        }
        if ck == 1 || ck == 3 {
            // a UUID or a long hex run (8+ hex characters with a digit) inside the word
            let r = if j - k >= 36 && (100..=101).contains(&hex_kind(&l[k..k + 36])) {
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
                } else if cfg.hex5 && r - k >= 5 && d && h && (k == i || !l[k - 1].is_ascii_alphanumeric()) && (r == j || !l[r].is_ascii_alphanumeric()) && hex_case(&l[k..r]) < 2 {
                    // 5-7 hex digits alone between signs: a short hex number
                    shape[sl] = MARK;
                    sl += 1;
                    let v = l[k..r].iter().fold(0u64, |a, &c| a << 4 | hexval(c) as u64);
                    vals[nv] = (HEXS + (r - k - 5) as u8 + 3 * hex_case(&l[k..r]), v, k as u32, r as u32);
                    nv += 1;
                    k = r;
                    continue;
                } else {
                    k
                }
            };
            if r > k {
                shape[sl] = MARK;
                sl += 1;
                let hk = hex_kind(&l[k..r]);
                let upper = l[k..r].iter().any(|c| c.is_ascii_uppercase());
                let lower = l[k..r].iter().any(|c| c.is_ascii_lowercase());
                vals[nv] = if cfg.hexnum && r - k <= 16 && !(upper && lower) && hk != 100 && hk != 101 {
                    // 8-16 hex digits in one case: a number (kinds HEXNUM..: width and case)
                    let v = l[k..r].iter().fold(0u64, |a, &c| a << 4 | hexval(c) as u64);
                    (HEXNUM + (r - k - 8) as u8 + 9 * upper as u8, v, k as u32, r as u32)
                } else {
                    (hk, 0, k as u32, r as u32)
                };
                nv += 1;
                k = r;
                continue;
            }
        }
        if ck == 1 {
            let s = k;
            while k < j && CLS[l[k] as usize] == 1 {
                k += 1;
            }
            let v = digit_value(l, s, k, &mut sl, cfg);
            shape[sl] = MARK;
            sl += 1;
            vals[nv] = v;
            nv += 1;
        } else {
            shape[sl] = l[k];
            sl += 1;
            k += 1;
        }
    }
    (sl, nv)
}
