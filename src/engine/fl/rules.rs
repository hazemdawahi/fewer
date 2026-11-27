// Rules ("store the rule, not the value"): a number slot of a pattern whose value follows from
// earlier slots of the same line:
// - equal to one, or k times one (Hadoop "kvi 26214396(104857584)": 4 x),
// - a sum of 2 or 3 of them with signs (Spark "total = boot + init + finish"),
// - one plus a fixed offset (an epoch number = the line's clock time in seconds + an offset),
// - the human size of a byte count (Proxifier "1228 bytes (1.19 KB)": whole-number steps of
//   1024 down to the unit below, then 3 significant digits, cut, not rounded),
// - near: one of the above plus a small difference (Spark "init" is "-boot" + 1 or 2; a
//   difference d with zz(d) of NEAR_BITS bits or more: the value is coded as before).
// A pattern's rules are chosen once per block on its first lines (no trials per line). Each
// value at a rule's slot then costs one coded yes/no, coded after the pattern ids with the same
// range coder: yes = the rule gives the value (a near rule: with its difference, coded right
// after) and the value is not stored; no = the value is coded as before. The rules sit in the
// block header (flag bit 2), after the flags byte.
use super::*;

/// v = m * x[src0] + sgn1 * x[src1] + sgn2 * x[src2] + c
pub const LIN: u8 = 0;
/// the whole part of x[src0] as a human size in unit 1024^m (1 = KB, 2 = MB, ...)
pub const HINT: u8 = 1;
/// the digits after its point (2 below 10, 1 below 100, none from 100 on)
pub const HFRAC: u8 = 2;
/// LIN plus a coded difference
pub const NEAR: u8 = 3;
/// A near rule gives a value only when zz(its difference) has fewer bits than this.
pub const NEAR_BITS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rule {
    pub slot: u16, // the slot it fills (place among the pattern's slots)
    pub op: u8,
    pub kind: u8,      // LIN / NEAR: the zero-padded kind its values may have (0: none)
    pub src: [u16; 3], // earlier slots
    pub sgn: [i8; 3],  // LIN / NEAR: signs of x[src1], x[src2] (0: no such term); sgn[0] unused
    pub m: u64,
    pub c: i64,
    pub always: bool, // it gave every value at its slot in the block: no answers coded (op bit 128)
}

/// A slot's value as a rule source when it is not a number or a clock time.
pub const NOT_NUM: u64 = u64::MAX;

/// A plain or zero-padded number (the kinds a rule can give).
#[inline]
pub fn is_num(kind: u8) -> bool {
    kind == 0 || (5..=22).contains(&kind)
}

#[inline]
fn pow10(n: u8) -> u64 {
    10u64.pow(n as u32)
}

/// A slot's value as a rule source: a number, or a clock time in whole seconds; else NOT_NUM.
#[inline]
pub fn src_val(kind: u8, v: u64) -> u64 {
    if is_num(kind) || (SPN..=SPN + SPN_MAX).contains(&kind) {
        // (space-padded numbers too: fixed_width may give a column's plain numbers the SPN
        // kind after pass 2, and the opener must see the same source values)
        v
    } else if (40..68).contains(&kind) {
        v / pow10((kind - 40) >> 2)
    } else if kind == 68 {
        v / 1000
    } else if (110..124).contains(&kind) {
        v / pow10((kind - 110) >> 1)
    } else {
        NOT_NUM
    }
}

/// The kind the line splitter gives v written zero-padded as `kind` (a padded kind only when
/// the padding adds a zero).
#[inline]
pub fn pad_kind(kind: u8, v: u64) -> u8 {
    if kind >= 5 && digits(v) < (kind - 3) as usize { kind } else { 0 }
}

/// A LIN / NEAR rule's value (with c, before a near rule's difference), or None when a source
/// is not a number.
#[inline]
pub fn base(r: &Rule, x: &[u64]) -> Option<i128> {
    let a = x[r.src[0] as usize];
    if a == NOT_NUM {
        return None;
    }
    let mut p = r.m as i128 * a as i128 + r.c as i128;
    for t in 1..3 {
        if r.sgn[t] != 0 {
            let b = x[r.src[t] as usize];
            if b == NOT_NUM {
                return None;
            }
            p += r.sgn[t] as i128 * b as i128;
        }
    }
    Some(p)
}

/// A number the rules can give (0 .. NOT_NUM - 1) as (kind, value), written as `kind`.
#[inline]
pub fn as_num(p: i128, kind: u8) -> Option<(u8, u64)> {
    if (0..NOT_NUM as i128).contains(&p) { Some((pad_kind(kind, p as u64), p as u64)) } else { None }
}

/// The (kind, value) an exact rule r gives for a line whose slot values so far are x (as
/// src_val), or None (a source is not a number, or the result is not a number).
#[inline]
pub fn predict(r: &Rule, x: &[u64]) -> Option<(u8, u64)> {
    if r.op == LIN {
        return as_num(base(r, x)?, r.kind);
    }
    let a = x[r.src[0] as usize];
    if a == NOT_NUM {
        return None;
    }
    let mut q = a;
    for _ in 1..r.m {
        q /= 1024;
    }
    let ip = q / 1024;
    if r.op == HINT {
        Some((0, ip))
    } else if ip >= 100 {
        None
    } else if ip >= 10 {
        Some((0, q * 10 / 1024 % 10))
    } else {
        let f = q * 100 / 1024 % 100;
        Some((if f < 10 { 5 } else { 0 }, f))
    }
}

/// The rules section: patterns with rules (count, then each as the difference of its id from
/// the one before), each with its rule count and rules.
pub fn put_rules(h: &mut Vec<u8>, rfirst: &[u32], rules: &[Rule]) {
    let np = rfirst.windows(2).filter(|w| w[1] > w[0]).count();
    put_varint(h, np as u64);
    let mut last = 0usize;
    for t in 0..rfirst.len() - 1 {
        let (a, b) = (rfirst[t] as usize, rfirst[t + 1] as usize);
        if a == b {
            continue;
        }
        put_varint(h, (t - last) as u64);
        last = t;
        put_varint(h, (b - a) as u64);
        for r in &rules[a..b] {
            put_varint(h, r.slot as u64);
            h.push(r.op | (r.always as u8) << 7);
            h.push(r.kind);
            put_varint(h, (r.slot - r.src[0]) as u64);
            put_varint(h, r.m);
            if r.op == LIN || r.op == NEAR {
                put_varint(h, zz(r.c));
                for t in 1..3 {
                    h.push(r.sgn[t] as u8);
                    if r.sgn[t] != 0 {
                        put_varint(h, (r.slot - r.src[t]) as u64);
                    }
                }
            }
        }
    }
}

/// Read the rules section (`patterns`: the block's pattern count): each pattern's first rule
/// (one more entry at the end) and the rules.
pub fn get_rules(h: &[u8], hp: &mut usize, patterns: usize) -> (Vec<u32>, Vec<Rule>) {
    let np = get_varint(h, hp) as usize;
    let mut rfirst = vec![0u32; patterns + 1];
    let mut rules = Vec::new();
    let mut t = 0usize;
    for _ in 0..np {
        let nt = t + get_varint(h, hp) as usize;
        // patterns between have no rules
        for e in &mut rfirst[t + 1..=nt] {
            *e = rules.len() as u32;
        }
        t = nt;
        let n = get_varint(h, hp) as usize;
        for _ in 0..n {
            let mut r = Rule { slot: get_varint(h, hp) as u16, op: h[*hp] & 127, always: h[*hp] >= 128, kind: h[*hp + 1], ..Default::default() };
            *hp += 2;
            r.src[0] = r.slot - get_varint(h, hp) as u16;
            r.m = get_varint(h, hp);
            if r.op == LIN || r.op == NEAR {
                r.c = unzz(get_varint(h, hp));
                for t in 1..3 {
                    r.sgn[t] = h[*hp] as i8;
                    *hp += 1;
                    if r.sgn[t] != 0 {
                        r.src[t] = r.slot - get_varint(h, hp) as u16;
                    }
                }
            }
            rules.push(r);
        }
        rfirst[t + 1] = rules.len() as u32;
    }
    for e in &mut rfirst[t + 1..] {
        *e = rules.len() as u32;
    }
    (rfirst, rules)
}

/// The coded answers: per value at a rule's slot a yes/no (0 = the rule gives it; one learning
/// probability per rule and its last answer), and for a near rule's yes its difference d as
/// zz(d): its bit length in unary (a learning probability per step, no end mark after the
/// longest), the bit below the top one (learning, per length) and the other bits as they are.
pub struct Flags {
    p: Vec<u16>,
    last: Vec<u8>,
    len: Vec<[u16; NEAR_BITS]>,
    hi: Vec<[u16; NEAR_BITS + 1]>,
}

impl Flags {
    pub fn new(rules: usize) -> Flags {
        Flags { p: vec![P_HALF; 2 * rules], last: vec![0; rules], len: Vec::new(), hi: Vec::new() }
    }
    #[inline]
    pub fn step<C: Bits>(&mut self, c: &mut C, ri: usize, miss: u32) -> u32 {
        let b = c.bit(&mut self.p[2 * ri + self.last[ri] as usize], miss);
        self.last[ri] = b as u8;
        b
    }
    /// A near rule's difference (`d` when packing): returns it.
    pub fn diff<C: Bits>(&mut self, c: &mut C, ri: usize, d: i64) -> i64 {
        if self.len.len() <= ri {
            self.len.resize(ri + 1, [P_HALF; NEAR_BITS]);
            self.hi.resize(ri + 1, [P_HALF; NEAR_BITS + 1]);
        }
        let u = zz(d);
        let n = 64 - u.leading_zeros() as usize;
        let mut k = 0;
        while k < NEAR_BITS && c.bit(&mut self.len[ri][k], (k < n) as u32) == 1 {
            k += 1;
        }
        if k == 0 {
            return 0;
        }
        let mut x = 1u64;
        if k >= 2 {
            x = x << 1 | c.bit(&mut self.hi[ri][k], (u >> (k - 2) & 1) as u32) as u64;
            for i in (0..k - 2).rev() {
                let mut p = P_HALF;
                x = x << 1 | c.bit(&mut p, (u >> i & 1) as u32) as u64;
            }
        }
        unzz(x)
    }
}

/// Patterns with fewer lines in the block get no rules; a pattern is screened on its first
/// SCREEN lines, and the candidates found are tried on its first SAMPLE lines; sums take their
/// terms from the NEAR_SRC nearest earlier number slots, the other rules their source from the
/// ONE_SRC nearest.
pub const MIN_LINES: u32 = 32;
pub const SCREEN: usize = 32;
pub const SAMPLE: usize = 128;
const NEAR_SRC: usize = 8;
const ONE_SRC: usize = 64;
/// A rule must save at least this many bits over the pattern's lines in the block; a slot
/// whose values cost below FREE bits per line now is left alone, and so is one whose values
/// cost below MIN_GAIN bits in all; near rules only for values that cost at least NEAR_DEAR
/// bits per line now.
const MIN_GAIN: f64 = 96.0;
const FREE: f64 = 0.3;
const NEAR_DEAR: f64 = 6.0;
/// A rule must also save at least this many bits per line of its pattern (FL_RGAIN): every
/// line of a pattern with rules costs the opener a slower line and an answer per rule.
fn line_gain() -> f64 {
    static G: crate::engine::fl::NoEnvOnce<f64> = crate::engine::fl::NoEnvOnce::new();
    *G.get_or_init(|| crate::engine::fl::no_env::var("FL_RGAIN").ok().and_then(|x| x.parse().ok()).unwrap_or(0.0))
}

/// FL_RNONEAR: no near rules (exact rules only).
fn no_near() -> bool {
    static G: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *G.get_or_init(|| crate::engine::fl::no_env::var_os("FL_RNONEAR").is_some())
}

/// The exact candidates of a slot kept after screening (and the best near one).
const KEEP: usize = 2;

/// Rough bits of a slot's values coded as now, per line (the first: not known, taken as 0): a
/// repeat of the value before, the same step as before, one of the few values before it
/// (cache codes), or a difference.
fn now_bits(vals: &[u64], out: &mut Vec<f64>) {
    out.clear();
    let mut rec = [u64::MAX; 4];
    let mut step = 0i128;
    for (r, &v) in vals.iter().enumerate() {
        let st = v as i128 - rec[0] as i128;
        if r == 0 {
            out.push(0.0);
        } else if v == rec[0] {
            out.push(0.3);
        } else if st == step {
            out.push(0.1);
        } else if rec[1..].contains(&v) {
            out.push(2.5);
        } else {
            out.push(2.0 + (64 - v.abs_diff(rec[0]).leading_zeros()) as f64);
        }
        step = st;
        if v != rec[0] {
            let p = rec.iter().position(|&x| x == v).unwrap_or(3);
            rec.copy_within(0..p, 1);
            rec[0] = v;
        }
    }
}

/// Bits of `n` coded yes/no answers with `h` yes.
fn flag_bits(h: usize, n: usize) -> f64 {
    let p = h as f64 / n as f64;
    let e = if h == 0 || h == n { 0.0 } else { -(p * p.log2() + (1.0 - p) * (1.0 - p).log2()) };
    n as f64 * e.max(0.03)
}

/// Candidate rules for slot k from two lines (values x0, x1; wanted (kind, value) w0, w1):
/// exact rules that give both, and near rules (an offset c taken from the first line) whose
/// difference is small on the second. Single sources from `one`, sums from `near` (each sum
/// starts with a term with sign +).
#[allow(clippy::too_many_arguments)]
fn candidates(x0: &[u64], w0: (u8, u64), x1: &[u64], w1: (u8, u64), one: &[usize], near: &[usize], k: usize, pkind: &[u8], own: &[bool], out: &mut Vec<Rule>) {
    let kind = pkind[k];
    let (v0, v1) = (w0.1 as i128, w1.1 as i128);
    // equal to an earlier slot: only when the value's column holds (nearly) only this
    // pattern's values and the earlier slot's column does not; when both do, the two columns
    // are copies the coder finds already; when the value's column holds others' values too,
    // taking some out breaks such a copy. Sums are not tried then (they would only be the same
    // rule in disguise).
    if let Some(&a) = one.iter().find(|&&a| x0[a] as i128 == v0 && x1[a] as i128 == v1) {
        if own[k] && !own[a] {
            // written like the earlier slot when the sample showed no padding here
            let kind = if kind == 0 { pkind[a] } else { kind };
            out.push(Rule { slot: k as u16, op: LIN, kind, m: 1, src: [a as u16, 0, 0], ..Default::default() });
        }
        return;
    }
    let base_r = Rule { slot: k as u16, op: LIN, kind, m: 1, ..Default::default() };
    let fits = |r: &Rule| predict(r, x0) == Some(w0) && predict(r, x1) == Some(w1);
    // a near rule's difference: small, and small next to the value (at most 1/16 of it)
    let small = |d: i128, v: i128| d.abs() < 1 << (NEAR_BITS - 2) && d.abs() * 16 <= v.abs().max(16);
    for &a in one {
        if x0[a] == NOT_NUM || x1[a] == NOT_NUM {
            continue;
        }
        let (a0, a1) = (x0[a] as i128, x1[a] as i128);
        let ra = Rule { src: [a as u16, 0, 0], ..base_r };
        // k times
        if a0 > 0 && v0 % a0 == 0 && v0 / a0 <= 1 << 20 && v0 > a0 && v1 == v0 / a0 * a1 {
            out.push(Rule { m: (v0 / a0) as u64, ..ra });
        }
        // plus an offset: the same on both lines, or near
        let d = v0 - a0;
        if d.abs() < 1 << 40 {
            if d != 0 && d == v1 - a1 {
                out.push(Rule { c: d as i64, ..ra });
            } else if d != v1 - a1 && small(v1 - a1 - d, v1) {
                out.push(Rule { op: NEAR, c: d as i64, ..ra });
            }
        }
        // human size (a byte count of at least 1 KB, a value below 1024)
        if a0 >= 1024 && v0 < 1024 {
            for u in 1..=4u64 {
                for op in [HINT, HFRAC] {
                    let r = Rule { op, kind: 0, m: u, ..ra };
                    if fits(&r) {
                        out.push(r);
                    }
                }
            }
        }
    }
    // sums of 2 and 3 with signs: exact on both lines, or exact on the first and near on the
    // second
    let mut push = |r: Rule| {
        if fits(&r) {
            out.push(r);
        } else if let Some(b) = base(&r, x1)
            && small(v1 - b, v1)
        {
            out.push(Rule { op: NEAR, ..r });
        }
    };
    for (ia, &a) in near.iter().enumerate() {
        if x0[a] == NOT_NUM {
            continue;
        }
        let xa = x0[a] as i128;
        for (ib, &b) in near.iter().enumerate().skip(ia + 1) {
            if x0[b] == NOT_NUM {
                continue;
            }
            let xb = x0[b] as i128;
            for (sa, sb) in [(1i128, 1i128), (1, -1), (-1, 1)] {
                if sa * xa + sb * xb == v0 {
                    let (p, q, s) = if sa > 0 { (a, b, sb) } else { (b, a, sa) };
                    push(Rule { src: [p as u16, q as u16, 0], sgn: [1, s as i8, 0], ..base_r });
                }
            }
            for &c in &near[ib + 1..] {
                if x0[c] == NOT_NUM {
                    continue;
                }
                let xc = x0[c] as i128;
                for s in 1..8 {
                    // signs of a, b, c: bits of s (1 = +), not all -
                    let sg = [(s & 1) as i128 * 2 - 1, (s >> 1 & 1) as i128 * 2 - 1, (s >> 2 & 1) as i128 * 2 - 1];
                    if sg[0] * xa + sg[1] * xb + sg[2] * xc == v0 {
                        let t = [(a, sg[0]), (b, sg[1]), (c, sg[2])];
                        let f = t.iter().position(|e| e.1 > 0).unwrap();
                        let (g, h) = match f {
                            0 => (t[1], t[2]),
                            1 => (t[0], t[2]),
                            _ => (t[0], t[1]),
                        };
                        push(Rule { src: [t[f].0 as u16, g.0 as u16, h.0 as u16], sgn: [1, g.1 as i8, h.1 as i8], ..base_r });
                    }
                }
            }
        }
    }
}

/// Bits rule r would save on the sample's first `rows` lines: the bits of the values it gives
/// as now, less its answers and (near) differences.
fn gain(r: &Rule, x: &[u64], kv: &[(u8, u64)], ns: usize, k: usize, now: &[f64], rows: usize) -> f64 {
    let (mut hits, mut saved) = (0usize, 0f64);
    // near: the differences' bit lengths, to price them as coded (learning)
    let mut len = [0u32; 65];
    let mut mant = 0f64;
    for i in 0..rows {
        let row = &x[i * ns..(i + 1) * ns];
        let want = kv[i * ns + k];
        if r.op == NEAR {
            if let Some(d) = applies(r, row, want.0, want.1) {
                let n = 64 - zz(d).leading_zeros() as usize;
                len[n] += 1;
                mant += n.saturating_sub(2) as f64;
                hits += 1;
                saved += now[i];
            }
        } else if predict(r, row) == Some(want) {
            hits += 1;
            saved += now[i];
        }
    }
    let mut cost = flag_bits(hits, rows) + mant;
    for &c in &len {
        if c > 0 {
            // the length in unary with learning probabilities: about its entropy; the bit
            // below the top one (learning) taken as half a bit
            cost += c as f64 * ((hits as f64 / c as f64).log2() + 0.5);
        }
    }
    saved - cost
}

/// Simpler rules first when they do as well: exact, fewer terms, no offset, no factor.
fn plainness(r: &Rule) -> (bool, u8, bool, bool, u8) {
    (r.op == NEAR, (r.sgn[1] != 0) as u8 + (r.sgn[2] != 0) as u8, r.c != 0, r.m != 1, r.op)
}

/// Work space for screen and choose (kept between patterns).
#[derive(Default)]
pub struct Scratch {
    x: Vec<u64>,
    cands: Vec<Rule>,
    srcok: Vec<bool>,
    pkind: Vec<u8>,
    tv: Vec<u64>,
    nb: Vec<f64>,
    now: Vec<f64>,
    tgts: Vec<usize>,
    top: Vec<(f64, Rule)>,
    one: Vec<usize>,
}

/// The source values of a sample (see src_val).
fn sources(kv: &[(u8, u64)], x: &mut Vec<u64>) {
    x.clear();
    x.extend(kv.iter().map(|&(k, v)| src_val(k, v)));
}

/// Screen one pattern on its first lines: `kv` holds `ns` slot values (kind, value) per line;
/// `count` is the pattern's line count in the block; `own` whether its slot's column holds
/// (nearly) only its values (see candidates). Appends, per slot, the KEEP best exact candidates
/// and the best near one that would save bits on these lines (in slot order).
pub fn screen(kv: &[(u8, u64)], ns: usize, count: u32, own: &[bool], out: &mut Vec<Rule>, w: &mut Scratch) {
    let rows = kv.len() / ns;
    if rows < 2 || ns < 2 {
        return;
    }
    // targets: a number on at least half the lines, not nearly free already, and with enough
    // to gain over the block (their bits as now, one after the other in `now`)
    w.now.clear();
    w.tgts.clear();
    for k in 1..ns {
        if (0..rows).filter(|&r| is_num(kv[r * ns + k].0)).count() * 2 < rows {
            continue;
        }
        w.tv.clear();
        w.tv.extend((0..rows).map(|r| kv[r * ns + k].1));
        now_bits(&w.tv, &mut w.nb);
        let total: f64 = w.nb.iter().sum();
        if total < rows as f64 * FREE || total * (count as f64 / rows as f64) < MIN_GAIN {
            continue;
        }
        w.now.extend_from_slice(&w.nb);
        w.tgts.push(k);
    }
    if w.tgts.is_empty() {
        return;
    }
    sources(kv, &mut w.x);
    let x = &w.x[..];
    // per slot: a source (a number or a clock time on at least half the lines, and not always
    // the same), its most common padded kind
    w.srcok.clear();
    w.pkind.clear();
    for k in 0..ns {
        let (mut nsrc, mut varies) = (0usize, false);
        let mut pk = [0u32; 23];
        for r in 0..rows {
            let (kd, xv) = (kv[r * ns + k].0, x[r * ns + k]);
            nsrc += (xv != NOT_NUM) as usize;
            varies |= xv != x[k];
            if (5..=22).contains(&kd) {
                pk[kd as usize] += 1;
            }
        }
        w.srcok.push(nsrc * 2 >= rows && varies);
        w.pkind.push((5..23).max_by_key(|&w| pk[w]).filter(|&w| pk[w] > 0).unwrap_or(0) as u8);
    }
    for (i, &k) in w.tgts.iter().enumerate() {
        let now = &w.now[i * rows..(i + 1) * rows];
        // the sources before slot k (the nearest ONE_SRC; sums: the nearest NEAR_SRC)
        w.one.clear();
        w.one.extend((0..k).rev().filter(|&j| w.srcok[j]).take(ONE_SRC));
        w.one.reverse();
        if w.one.is_empty() {
            continue;
        }
        let one = &w.one[..];
        let near = &one[one.len().saturating_sub(NEAR_SRC)..];
        let at = |r: usize| kv[r * ns + k];
        // candidates from two pairs of lines where the value is a number, not 0 and new
        w.cands.clear();
        let mut probe = [0usize; 4];
        let mut g = 0;
        for r in 0..rows {
            let v = at(r);
            if is_num(v.0) && v.1 != 0 && (r == 0 || at(r - 1).1 != v.1) {
                probe[g] = r;
                g += 1;
                if g == 4 {
                    break;
                }
            }
        }
        for p in probe[..g].chunks_exact(2) {
            let row = |r: usize| &x[r * ns..(r + 1) * ns];
            candidates(row(p[0]), at(p[0]), row(p[1]), at(p[1]), one, near, k, &w.pkind, own, &mut w.cands);
        }
        if w.cands.is_empty() {
            continue;
        }
        super::sort_stable_by(&mut w.cands, |a, b| (a.op, a.src, a.sgn, a.m, a.c) < (b.op, b.src, b.sgn, b.m, b.c));
        w.cands.dedup();
        // the KEEP exact rules and the near rule that save the most bits on these lines
        w.top.clear();
        for c in w.cands.iter() {
            let g = gain(c, x, kv, ns, k, now, rows);
            if g > 0.0 {
                w.top.push((g, *c));
            }
        }
        super::sort_stable_by(&mut w.top, |a, b| b.0.total_cmp(&a.0).then(plainness(&a.1).cmp(&plainness(&b.1))) == std::cmp::Ordering::Less);
        out.extend(w.top.iter().filter(|e| e.1.op != NEAR).take(KEEP).map(|e| e.1));
        out.extend(w.top.iter().filter(|e| e.1.op == NEAR).take(1).map(|e| e.1));
    }
}

/// Choose a pattern's rules among its screened candidates (`cands`, in slot order) on its
/// first lines (`kv`, `ns`, `count` as in screen): per slot the one that saves the most bits,
/// when that is at least MIN_GAIN over the pattern's lines in the block. Appends to `out`.
pub fn choose(kv: &[(u8, u64)], ns: usize, count: u32, cands: &[Rule], out: &mut Vec<Rule>, w: &mut Scratch) {
    let rows = kv.len() / ns;
    sources(kv, &mut w.x);
    let x = &w.x[..];
    for same in cands.chunk_by(|a, b| a.slot == b.slot) {
        let k = same[0].slot as usize;
        w.tv.clear();
        w.tv.extend((0..rows).map(|r| kv[r * ns + k].1));
        now_bits(&w.tv, &mut w.now);
        // near rules only for values that are dear now
        let nearok = w.now.iter().sum::<f64>() >= rows as f64 * NEAR_DEAR && !no_near();
        let mut best: Option<(f64, Rule)> = None;
        for c in same {
            if c.op == NEAR && !nearok {
                continue;
            }
            let g = gain(c, x, kv, ns, k, &w.now, rows);
            if best.is_none_or(|b| g > b.0) {
                best = Some((g, *c));
            }
        }
        if let Some((g, r)) = best
            && g * count as f64 / rows as f64 >= MIN_GAIN
            && g >= rows as f64 * line_gain()
        {
            out.push(r);
        }
    }
}

/// Whether rule r gives a line's value (kind, v) at its slot (x: the line's values so far):
/// Some(the near rule's difference, 0 for exact rules), or None (the value is coded as before).
#[inline]
pub fn applies(r: &Rule, x: &[u64], kind: u8, v: u64) -> Option<i64> {
    if r.op == NEAR {
        let d = v as i128 - base(r, x)?;
        if is_num(kind) && pad_kind(r.kind, v) == kind && d.abs() < 1 << (NEAR_BITS - 1) && zz(d as i64) < 1 << NEAR_BITS { Some(d as i64) } else { None }
    } else if predict(r, x) == Some((kind, v)) {
        Some(0)
    } else {
        None
    }
}

/// The (kind, value) rule r gives with difference d (0 for exact rules).
#[inline]
pub fn give(r: &Rule, x: &[u64], d: i64) -> (u8, u64) {
    if r.op == NEAR { as_num(base(r, x).expect("rule") + d as i128, r.kind).expect("rule") } else { predict(r, x).expect("rule") }
}
