//! M4 (harness only): the column coder as it was at M0 (ref), kept for FL_SHADOW=code_col and
//! FL_COLFUZZ, which run it beside the new coder and compare every output. Nothing on the
//! default path calls it: the engine's port (edit 5) folds the shadow switch to "off", so the
//! product leaves it out. Verbatim copies of ref's EncCache, guess_forms, guess_cost, guess_two,
//! put_diffs, put_nums, code_col, code_col_own and fixed_width.
#![allow(dead_code)]
use super::*;

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

/// FL_DOD: a rough size of a number column in each form (0 as is, 1 differences, 2
/// differences of differences) as an LZ coder would see it, in tenths of a byte: a code equal
/// to the one before costs 2, one of the 3 before that 5, one seen earlier (small table) 10,
/// else 10 per varint byte. Looks only at the values it is given (the caller's sample).
fn guess_forms(vals: &[u64], set: &mut Vec<u64>, which: usize) -> [u64; 3] {
    // (each form has its own table part and recent list: only form `which` and form 2 are
    // worked out, the other's cost stays 0; the caller compares those two)
    let bits = (2 * vals.len()).next_power_of_two().trailing_zeros().clamp(4, 12);
    set.clear();
    set.resize(2 << bits, u64::MAX);
    let mut cost = [0u64; 3];
    // (one loop per form `which` (0 or 1, the caller's way), with fixed indices: the same
    // costs and table)
    debug_assert!(which < 2);
    (cost[which], cost[2]) = guess_two(vals, set, bits, which == 1);
    cost
}

/// The cost of one code in guess_forms: x against its form's recent list and table entry
/// (both updated).
#[inline(always)]
fn guess_cost(x: u64, l: &mut [u64; 4], set: &mut [u64], h: usize) -> u64 {
    let c = if l[0] == x {
        2
    } else if l[1] == x || l[2] == x || l[3] == x {
        5
    } else if set[h] == x {
        10
    } else {
        10 * (1 + (63 - (x | 1).leading_zeros() as u64) / 7)
    };
    if l[0] != x {
        *l = [x, l[0], l[1], l[2]];
    }
    set[h] = x;
    c
}

/// guess_forms for form `diff` (false: values, true: differences) and form 2: their costs.
#[inline(never)]
fn guess_two(vals: &[u64], set: &mut [u64], bits: u32, diff: bool) -> (u64, u64) {
    let (mut la, mut lb) = ([u64::MAX; 4], [u64::MAX; 4]);
    let (mut ca, mut cb) = (0u64, 0u64);
    let (mut prev, mut pd) = (0i64, 0i64);
    let half = 1usize << bits;
    for &v in vals {
        let d = (v as i64).wrapping_sub(prev);
        let xa = if diff { zz(d) } else { v };
        let xb = zz(d.wrapping_sub(pd));
        prev = v as i64;
        pd = d;
        ca += guess_cost(xa, &mut la, set, (xa.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize);
        cb += guess_cost(xb, &mut lb, set, half + (xb.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize);
    }
    (ca, cb)
}

/// The differences of vals (zigzag varints) at p; returns the end.
#[inline(always)]
unsafe fn put_diffs(mut p: *mut u8, vals: &[u64]) -> *mut u8 {
    let mut prev = 0u64;
    for &v in vals {
        p = unsafe { varint_at(p, zz((v as i64).wrapping_sub(prev as i64))) };
        prev = v;
    }
    p
}

/// Write a column's numbers into `out` (cache codes into `codes`, one byte per number: CK = a
/// new value written in `out`, CK + 1 = an entry of a list, its place written in `out`);
/// returns the mode bits (1: differences, 4: cache codes, 8: the column's own list of values).
/// `slot_last` holds the last number at each pattern slot (updated here, as the opener does).
#[inline(never)]
fn put_nums(vals: &[u64], slots: &[u32], ids: &[u32], slot_last: &mut [u64], syms: &mut Vec<u8>, dix: &mut Vec<u32>, table: &mut Vec<(u64, u32)>, sink: &mut Sink, defer1: bool, gset: &mut Vec<u64>) -> u64 {
    static NOCACHE: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    let use_cache = !*NOCACHE.get_or_init(|| env_on("FL_NOCACHE"));
    static NODICT: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    let use_dict = !*NODICT.get_or_init(|| env_on("FL_NODICT"));
    let mut mode;
    if !ids.is_empty() {
        // a column with long IDs: always cache codes; a cache miss on a long ID seen before is
        // code CK + 1 with its distance in the list, a new long ID is code CK (raw value)
        let mut c = EncCache::new();
        let (out, codes) = sink.parts(4);
        for (i, (&v, &sl)) in vals.iter().zip(slots).enumerate() {
            let ks = sl > SLOT;
            let sl = sl & SLOT;
            let t = slot_last[sl as usize];
            let s = c.step(v, t, ks);
            let id = ids[i];
            let s = if id == 0 {
                put_varint(out, v);
                CK
            } else if s < CK {
                s
            } else if id != u32::MAX {
                put_varint(out, (id - 1) as u64);
                CK + 1
            } else {
                put_varint(out, v);
                CK
            };
            codes.push(s as u8);
            slot_last[sl as usize] = v;
        }
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
        let (mut er, mut ed, mut mr, mut md) = (Est::new(), Est::new(), Est::new(), Est::new());
        let (mut dr, mut dd, mut di) = (Est::new(), Est::new(), Est::new());
        let mut hits = [0u32; CK + 2];
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
        let mut pre = 0;
        for k in 0..chunks {
            let st = if chunks == 1 { 0 } else { k * (n - clen) / (chunks - 1) };
            let mut prev = if st > 0 { vals[st - 1] } else { 0 };
            for (&v, &sl) in vals[st..st + clen].iter().zip(&slots[st..st + clen]) {
                let d = zz((v as i64).wrapping_sub(prev as i64));
                er.add(v);
                ed.add(d);
                let ks = sl > SLOT;
                let sl = sl & SLOT;
                let t = slot_last[sl as usize];
                let s = c.step(v, t, ks);
                if s == CK {
                    mr.add(v);
                    md.add(d);
                    let e = &mut table[(v.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - tb)) as usize];
                    if use_dict && e.0 == v {
                        // in the list: code CK + 1 (without the list it stays CK)
                        di.add(e.1 as u64);
                        dix.push(e.1);
                        syms.push(CK as u8 + 1);
                        hits[CK + 1] += 1;
                    } else {
                        *e = (v, nd);
                        nd += 1;
                        dr.add(v);
                        dd.add(d);
                        syms.push(CK as u8);
                        hits[CK] += 1;
                    }
                } else {
                    syms.push(s as u8);
                    hits[s] += 1;
                }
                slot_last[sl as usize] = v;
                prev = v;
                pre += 1;
            }
        }
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
        let mut best = er.bits();
        mode = 0;
        let dl = hcd + di.bits();
        for (e, m) in [(ed.bits(), 1u64), ((hc + mr.bits()) / margin, 4), ((hc + md.bits()) / margin, 5), ((dl + dr.bits()) / margin, 12), ((dl + dd.bits()) / margin, 13)] {
            if e < best {
                best = e;
                mode = m;
            }
        }
        if mode & 4 != 0 && chunks == 1 {
            // all numbers were looked at: the codes are ready (a list hit is CK without the list)
            if mode & 8 == 0 {
                syms.iter_mut().for_each(|x| *x = (*x).min(CK as u8));
            }
        } else if mode & 4 != 0 {
            // code the whole column from a fresh start (the opener's view), each number and
            // code written as it is coded (the same bytes as coding first and writing after)
            let dict = mode & 8 != 0;
            let delta = mode & 1 != 0;
            for &sl in slots {
                slot_last[(sl & SLOT) as usize] = 0;
            }
            c = EncCache::new();
            table.iter_mut().for_each(|e| *e = (u64::MAX, u32::MAX));
            nd = 0;
            let (out, codes) = sink.parts(mode);
            codes.reserve(vals.len());
            put_varints(out, 10 * vals.len() + 16, |mut o| unsafe {
                let mut prev = 0u64;
                for (&v, &sl) in vals.iter().zip(slots) {
                    let ks = sl > SLOT;
                    let sl = sl & SLOT;
                    let t = slot_last[sl as usize];
                    let s = c.step(v, t, ks);
                    let code = if s == CK {
                        // a new value (as is or as its difference), or a list entry (its place)
                        let x = if delta { zz((v as i64).wrapping_sub(prev as i64)) } else { v };
                        if dict {
                            let e = &mut table[(v.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - tb)) as usize];
                            if e.0 == v {
                                o = varint_at(o, e.1 as u64);
                                CK + 1
                            } else {
                                *e = (v, nd);
                                nd += 1;
                                o = varint_at(o, x);
                                CK
                            }
                        } else {
                            o = varint_at(o, x);
                            CK
                        }
                    } else {
                        s
                    };
                    codes.push(code as u8);
                    slot_last[sl as usize] = v;
                    prev = v;
                }
                o
            });
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

/// Code column ci (put_nums, then the grouping by slot when it pays); `may_group`: the block's
/// cap on grouped columns is not reached. (Not inlined: one copy for both column phases.)
#[inline(never)]
fn code_col(x: &ColCtx, ci: usize, c: &mut Col, may_group: bool, sc: &mut ColScratch, sink: &mut Sink) -> ColOut {
    if x.fixw {
        fixed_width(c);
    }
    // a big column of differences: its numbers in GROUPS segments by pattern (see GROUP_MIN)
    let may = c.vals.len() >= GROUP_MIN && c.kinds.len() == c.vals.len() && x.group_on && may_group && ci < x.prefix_cols && !x.any_split;
    // (a column that may be grouped and is written as plain differences: written below)
    let nm = put_nums(&c.vals, &c.slots, &c.ids, &mut sc.slot_last, &mut sc.syms, &mut sc.dix, &mut sc.table, sink, may, &mut sc.gset);
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
            // (a run of numbers at one slot is summed in registers, then added: the same sums)
            let (mut rs, mut rsum, mut rcnt) = (usize::MAX, 0u64, 0u32);
            for k in 0..chunks {
                let (st, mut prev) = runs(k);
                for (&v, &sl) in c.vals[st..st + clen].iter().zip(&c.slots[st..st + clen]) {
                    let sl = (sl & SLOT) as usize;
                    let z = zz((v as i64).wrapping_sub(prev as i64));
                    zo |= z;
                    za &= z;
                    let b = 64 - z.leading_zeros();
                    if sl != rs {
                        if rs != usize::MAX {
                            gsum[rs] += rsum;
                            gcnt[rs] += rcnt;
                        }
                        (rs, rsum, rcnt) = (sl, 0, 0);
                    }
                    rsum += b as u64;
                    rcnt += 1;
                    prev = v;
                }
            }
            if rs != usize::MAX {
                gsum[rs] += rsum;
                gcnt[rs] += rcnt;
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
            // (counted in 4 partial tables taken in turn, so that a repeated count does not
            // wait for itself; the same sums)
            let mut h4 = vec![[0u32; 65]; 4 * GROUPS];
            let mut q = 0;
            for k in 0..chunks {
                let (st, mut prev) = runs(k);
                for (&v, &sl) in c.vals[st..st + clen].iter().zip(&c.slots[st..st + clen]) {
                    let b = 64 - zz((v as i64).wrapping_sub(prev as i64)).leading_zeros();
                    prev = v;
                    let g = gfin[(sl & SLOT) as usize] as usize;
                    h4[(q & 3) * GROUPS + g][b as usize] += 1;
                    q += 1;
                }
            }
            for (g, e) in ge.iter_mut().enumerate() {
                for j in 0..4 {
                    for (a, &b) in e.h.iter_mut().zip(&h4[j * GROUPS + g]) {
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
        // (the write place of the current group stays in a register while the group repeats)
        let mut prev = 0u64;
        let (mut cg, mut cp) = (0usize, ends[0]);
        for (&v, &sl) in c.vals.iter().zip(&c.slots) {
            let g = gfin[(sl & SLOT) as usize] as usize;
            if g != cg {
                ends[cg] = cp;
                cg = g;
                cp = ends[g];
            }
            cp = unsafe { varint_at(cp, zz((v as i64).wrapping_sub(prev as i64))) };
            prev = v;
        }
        ends[cg] = cp;
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

/// code_col into buffers of the column's own (the parallel column phase), in the ColOut.
pub(super) fn code_col_own(x: &ColCtx, ci: usize, c: &mut Col, may_group: bool, sc: &mut ColScratch) -> ColOut {
    // (room for the longest varint per number and a code each: no regrowth; pages that are
    // not written are not used)
    let mut tmp = Vec::with_capacity(10 * c.vals.len() + 16);
    let mut codes = Vec::with_capacity(c.vals.len() + 16);
    let mut o = code_col(x, ci, c, may_group, sc, &mut Sink { a: &mut tmp, b: None, codes: &mut codes });
    o.tmp = tmp;
    o.codes = codes;
    o
}

/// A column of w-digit numbers, some with leading zeros (kind 3 + w) and some without (kind
/// 0): all become kind 3 + w ("zero-padded to w digits" writes them the same), so the column
/// needs no kinds. The same for numbers padded with spaces to w (kind SPN + w).
#[inline(never)]
fn fixed_width(c: &mut Col) {
    let k0 = match c.kinds.first() {
        Some(&k) => k,
        None => return,
    };
    let mut wk = 0u8; // the padded kind seen (3 + w or SPN + w)
    // (16 kinds at a time: a run of 0s, or of the padded kind already seen, changes nothing)
    let mut ch = c.kinds.chunks_exact(16);
    for x in &mut ch {
        let v = u128::from_le_bytes(x.try_into().unwrap());
        if v == 0 || (wk != 0 && v == wk as u128 * (u128::MAX / 255)) {
            continue;
        }
        for &k in x {
            if k != 0 {
                if !((5..=21).contains(&k) || (SPN + 2..=SPN + SPN_MAX).contains(&k)) || (wk != 0 && k != wk) {
                    return;
                }
                wk = k;
            }
        }
    }
    for &k in ch.remainder() {
        if k != 0 {
            if !((5..=21).contains(&k) || (SPN + 2..=SPN + SPN_MAX).contains(&k)) || (wk != 0 && k != wk) {
                return;
            }
            wk = k;
        }
    }
    if wk == 0 || (k0 == wk && all_eq(&c.kinds, wk)) {
        return;
    }
    let w = if wk >= SPN { (wk - SPN) as usize } else { (wk - 3) as usize };
    // digits(v) == w exactly when lo <= v <= hi (w >= 2 here; a u64 has at most 20 digits)
    if w > 20 {
        if c.kinds.contains(&0) {
            return;
        }
    } else {
        let lo = 10u64.pow(w as u32 - 1);
        let hi = if w == 20 { u64::MAX } else { 10u64.pow(w as u32) - 1 };
        if c.kinds.iter().zip(&c.vals).any(|(&k, &v)| k == 0 && !(lo..=hi).contains(&v)) {
            return;
        }
    }
    c.kinds.iter_mut().for_each(|k| *k = wk);
}

/// FL_COLFUZZ=n (harness only): n random columns, each coded by ref's coder (above) and the new
/// one (super::code_col_own) from the same start, on its own copy and scratch; every output is
/// compared: mode, grouped, numbers, codes, groups, the kinds and values after, and the per-slot
/// state (slot_last, the group sums, counts and groups). Columns: 0-104K numbers; values
/// constant, counters, small sets, any u64 (0, 1, 2^63 - 1, 2^63, u64::MAX ...), per-slot
/// counters, recent repeats, ramps; 1-40 slots in runs or at random; kind-same bits at 0-90%;
/// long-ID columns (ids 0, u32::MAX, places); kinds all 0, all padded, 0 and padded (w-digit
/// numbers or not), mixed, longer than the values; the grouping switches. Returns true when
/// every column is the same.
pub(super) fn fuzz(cols: u64, threads: usize, seed: u64) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    const NSLOT: usize = 64;
    struct R(u64);
    impl R {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }
        fn pct(&mut self, p: u64) -> bool {
            self.below(100) < p
        }
    }
    const EDGE: [u64; 12] = [0, 1, 2, 127, 128, 16383, 16384, (1 << 32) - 1, (1 << 63) - 1, 1 << 63, u64::MAX - 1, u64::MAX];
    fn any_u64(r: &mut R) -> u64 {
        match r.below(4) {
            0 => EDGE[r.below(12) as usize],
            1 => r.next(),
            _ => r.next() >> r.below(64),
        }
    }
    fn mkcol(r: &mut R) -> (ColCtx, Col, bool, bool) {
        // (grp: a big column of per-slot counters with steps of different sizes, set up so it
        // may be grouped)
        let grp = r.pct(6);
        let n = match if grp { 99 } else { r.below(100) } {
            0..=29 => r.below(17),
            30..=59 => 17 + r.below(300),
            60..=86 => 300 + r.below(4600),
            87..=98 => 4096 + r.below(30000),
            _ if grp => 4096 + r.below(30000),
            _ => 34000 + r.below(70000),
        } as usize;
        let ns = if grp { 2 + r.below(12) as usize } else if r.pct(30) { 1 } else { 1 + r.below(40) as usize };
        let base = r.below(16) as usize;
        let ty = if grp { 10 } else { r.below(11) };
        let set: Vec<u64> = (0..1 + r.below(20)).map(|_| if r.pct(50) { r.below(1000) } else { any_u64(r) }).collect();
        let mut per: Vec<u64> = (0..ns).map(|_| r.below(1 << 40)).collect();
        let sb = r.below(20);
        let step = r.below(1 << sb);
        let a0 = any_u64(r);
        let bb = r.below(30);
        let b0 = r.below(1 << bb);
        let ksp = if grp { 0 } else { [0, 10, 50, 90][r.below(4) as usize] };
        let runs = r.pct(60);
        let mut c = Col::default();
        let (mut cur, mut sl) = (a0, base);
        let mut hist: Vec<u64> = Vec::with_capacity(n);
        // fixed-width columns: w-digit numbers
        let fw = !grp && r.pct(25);
        let w = 2 + r.below(18) as u32;
        let (lo, hi) = (10u64.pow(w - 1), 10u64.pow(w) - 1);
        for i in 0..n {
            if !runs || r.pct(35) {
                sl = base + r.below(ns as u64) as usize;
            }
            let v = if fw && r.pct(95) {
                lo + r.below(hi - lo + 1)
            } else {
                match ty {
                    0 => a0,
                    1 => {
                        cur = cur.wrapping_add(r.below(step + 1));
                        cur
                    }
                    2 => set[(r.below(set.len() as u64) * r.below(set.len() as u64) / set.len().max(1) as u64) as usize],
                    3 => any_u64(r),
                    4 => {
                        let k = sl - base;
                        per[k] = per[k].wrapping_add(r.below(step + 1));
                        per[k]
                    }
                    5 => {
                        if !hist.is_empty() && r.pct(70) {
                            hist[hist.len() - 1 - r.below(hist.len().min(12) as u64) as usize]
                        } else {
                            any_u64(r) >> r.below(48)
                        }
                    }
                    6 => {
                        let x = a0.wrapping_add(b0.wrapping_mul(i as u64));
                        if r.pct(5) { x ^ r.below(16) } else { x }
                    }
                    7 => EDGE[r.below(12) as usize],
                    8 => set[(i / (1 + r.below(3) as usize)) % set.len()],
                    _ => {
                        // per-slot counters whose steps differ in size by slot (grouping)
                        let k = sl - base;
                        per[k] = per[k].wrapping_add(r.below(1 + (step | 1) << (3 * (k % 12))));
                        per[k]
                    }
                }
            };
            hist.push(v);
            c.vals.push(v);
            c.slots.push(sl as u32 | (r.pct(ksp) as u32) << 31);
        }
        if !grp && r.pct(5) && n > 0 {
            c.ids = (0..n).map(|_| match r.below(3) { 0 => 0, 1 => u32::MAX, _ => 1 + r.below(60) as u32 }).collect();
        }
        let wk = if r.pct(50) { 3 + w as u8 } else { SPN + w as u8 + r.below(3) as u8 };
        c.kinds = match if grp { 0 } else { r.below(10) } {
            0..=3 => vec![0; n],
            4 => vec![wk; n],
            5..=6 => (0..n).map(|_| if r.pct(50) { 0 } else { wk }).collect(),
            7 => (0..n).map(|_| [0u8, 1, 2, 24, 30, 40, 71, 100, 132, wk][r.below(10) as usize]).collect(),
            8 => {
                let mut k: Vec<u8> = (0..n).map(|_| if r.pct(80) { 0 } else { wk }).collect();
                k.extend((0..1 + r.below(5)).map(|_| 1u8));
                k
            }
            _ => (0..n).map(|_| if r.pct(97) { 0 } else { (5 + r.below(17)) as u8 }).collect(),
        };
        let mut cslots: Vec<u32> = (base..base + ns).map(|x| x as u32).collect();
        if r.pct(50) {
            cslots.reverse();
        }
        let ctx = ColCtx { cslot_at: vec![0, ns as u32], cslots, prefix_cols: if grp || r.pct(90) { 1 } else { 0 }, any_split: !grp && r.pct(5), group_on: grp || r.pct(90), gfull: r.pct(10), fixw: r.pct(90), nslots: NSLOT };
        (ctx, c, grp || r.pct(90), r.pct(30))
    }
    let next = AtomicU64::new(0);
    let bad = AtomicU64::new(0);
    let nv = AtomicU64::new(0);
    let modes = [const { AtomicU64::new(0) }; 64];
    let grouped = AtomicU64::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads.max(1) {
            sc.spawn(|| {
                let (mut sa, mut sb) = (ColScratch::new(NSLOT), ColScratch::new(NSLOT));
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= cols {
                        break;
                    }
                    let mut r = R(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) ^ i);
                    let (ctx, col, may_group, junk) = mkcol(&mut r);
                    // the same per-slot start in both (0, or old values when `junk`)
                    for k in 0..NSLOT {
                        let x = if junk { r.next() >> r.below(64) } else { 0 };
                        sa.slot_last[k] = x;
                        sb.slot_last[k] = x;
                    }
                    let (mut a, mut b) = (col.clone(), col.clone());
                    let oa = code_col_own(&ctx, 0, &mut a, may_group, &mut sa);
                    let ob = super::code_col_own(&ctx, 0, &mut b, may_group, &mut sb);
                    let same = oa.nm == ob.nm && oa.grouped == ob.grouped && oa.tmp == ob.tmp && oa.codes == ob.codes && oa.groups == ob.groups && a.kinds == b.kinds && a.vals == b.vals && sa.slot_last == sb.slot_last && sa.gsum == sb.gsum && sa.gcnt == sb.gcnt && sa.gfin == sb.gfin;
                    nv.fetch_add(col.vals.len() as u64, Relaxed);
                    modes[(oa.nm & 63) as usize].fetch_add(1, Relaxed);
                    grouped.fetch_add(oa.grouped as u64, Relaxed);
                    if !same {
                        let k = bad.fetch_add(1, Relaxed);
                        if k < 10 {
                            eprintln!("COLFUZZ DIFF column {} (seed {}): n {} ids {} mode {} / {} grouped {} / {} numbers {} / {} codes {} / {} kinds same {} slots same {}", i, seed, col.vals.len(), col.ids.len(), oa.nm, ob.nm, oa.grouped, ob.grouped, oa.tmp.len(), ob.tmp.len(), oa.codes.len(), ob.codes.len(), a.kinds == b.kinds, sa.slot_last == sb.slot_last);
                        }
                    }
                }
            });
        }
    });
    let ms: Vec<String> = (0..64).filter_map(|m| {
        let x = modes[m].load(Relaxed);
        if x > 0 { Some(format!("{}:{}", m, x)) } else { None }
    }).collect();
    let b = bad.load(Relaxed);
    eprintln!("COLFUZZ columns {} values {} differences {} (seed {}; modes {}; grouped {})", cols, nv.load(Relaxed), b, seed, ms.join(" "), grouped.load(Relaxed));
    b == 0
}
