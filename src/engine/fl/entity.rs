// Keyed (entity) modes, format 2 (B2): values remembered per entity (an IP, a pid, a session
// id), shared by every pattern that names the entity.
//
// A number slot of a pattern can be "keyed" by an earlier number slot of the same pattern
// (chosen per block on a sample of lines, see `plan`). The memory maps (key kind class, key
// value, value kind class) to that entity's last R different values (newest first) and its
// last new value. A column with keyed codes (mode bit 256) writes one code per number in the
// codes stream: KH + r = "the r-th recent value of this line's entity", KN (mode bit 512) = "a
// new value, written as a step from the entity's last new value", CK = a new number (as is,
// or a difference with mode bit 1); with mode bit 4 the column's cache codes are used as well.
// Example (Zookeeper): the port in "Accepted socket connection from /10.10.34.20:56370" is
// keyed by the IP before it; a later line naming the same IP codes its port as KH + 0.
// Packing (pass 2) and opening both look up and update the memory for every value of a keyed
// slot whose key is a number in that line, in line order, so both sides see the same memory.
// Header: flag FLAG, then the plan after the rules section (Plan::write / Plan::read).
// (Combined format 2: header flag 64 and mode bits 256 / 512, so that ID columns, ids.rs, keep
// flag 32 and mode bit 128; a column is never both keyed and an ID column, and cache code
// CK + 2 means a keyed hit only in a keyed column.)
// Slots with rules and slots of splitting columns are never keys or keyed (the plan skips
// them), so a keyed value is always in its own column.

use super::*;

/// Recent values per entity.
pub const R: usize = 4;
/// First keyed code (after the cache codes 0..=CK + 1): KH + r is a hit at rank r.
pub const KH: usize = CK + 2;
/// Keyed code: a new value as a step from the entity's last new value.
pub const KN: usize = KH + R;

/// Header flag (flags byte of stream 2): the block has keyed slots (plan after the rules).
pub const FLAG: u8 = 64;

/// Can a value of this kind be a key or a keyed value (numbers, padded numbers, IPs, hex
/// numbers; not long IDs, which have their own coder)? (The same answer before and after
/// fixed_width, which changes kind 0 to a padded kind.)
#[inline]
pub fn numeric(kind: u8) -> bool {
    kind == 0 || (3..=22).contains(&kind) || (SPN..=SPN + SPN_MAX).contains(&kind) || kind == 30 || (HEXNUM..HEXNUM + 18).contains(&kind)
}

/// The memory key of (key kind, key value, value kind); never 0 (0 marks an empty entry).
#[inline]
pub fn key_hash(kk: u8, key: u64, vk: u8) -> u64 {
    let c = (kclass(kk) as u64) << 8 | kclass(vk) as u64;
    let h = (key.wrapping_add(c.wrapping_mul(0xD6E8_FEB8_6659_FD93))).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h ^ (h >> 31)) | 1
}

/// One entity: its key hash, last new value, recent values (newest first) and their count;
/// one cache line.
#[derive(Clone, Copy, Default)]
#[repr(C, align(64))]
struct Ent {
    tag: u64,
    last: u64,
    rec: [u64; R],
    n: u64,
}

/// The memory: a direct-mapped table (a new entity takes over its entry; both sides do the
/// same, so it stays exact).
pub struct Mem {
    t: Vec<Ent>,
    mask: usize,
}

impl Mem {
    /// A memory of 2^bits entries.
    pub fn new(bits: u8) -> Mem {
        Mem { t: vec![Ent::default(); 1 << bits], mask: (1 << bits) - 1 }
    }
    /// The entry of key hash h, and whether it holds that entity.
    #[inline]
    pub fn find(&self, h: u64) -> (usize, bool) {
        let i = (h >> 32) as usize & self.mask;
        (i, self.t[i].tag == h)
    }
    /// The rank of v in a held entity's recent values (R: not there).
    #[inline]
    pub fn rank(&self, i: usize, v: u64) -> usize {
        let e = &self.t[i];
        e.rec[..e.n as usize].iter().position(|&x| x == v).unwrap_or(R)
    }
    /// The value at rank r of a held entity.
    #[inline]
    pub fn at(&self, i: usize, r: usize) -> u64 {
        self.t[i].rec[r]
    }
    /// How many recent values a held entity has.
    #[inline]
    pub fn count(&self, i: usize) -> usize {
        self.t[i].n as usize
    }
    /// The entity's last new value.
    #[inline]
    pub fn last(&self, i: usize) -> u64 {
        self.t[i].last
    }
    /// After value v (at rank r, R: new) of the entity with key hash h was coded: v moves to
    /// the front (a new value pushes the oldest out).
    #[inline]
    pub fn update(&mut self, i: usize, h: u64, held: bool, v: u64, r: usize) {
        let e = &mut self.t[i];
        if !held {
            e.tag = h;
            e.n = 0;
        }
        let mut k = if r < R {
            r
        } else {
            e.n = (e.n + 1).min(R as u64);
            e.last = v;
            R - 1
        };
        while k > 0 {
            e.rec[k] = e.rec[k - 1];
            k -= 1;
        }
        e.rec[0] = v;
    }
}

/// Keyed slots of a block: per pattern slot (numbered as in the header), how many slots back
/// its key is (0: not keyed); and the memory size.
#[derive(Default, Clone)]
pub struct Plan {
    pub dist: Vec<u16>,
    pub bits: u8,
}

impl Plan {
    pub fn any(&self) -> bool {
        self.dist.iter().any(|&d| d != 0)
    }
    /// Per pattern slot: is it the key of a later slot?
    pub fn keys(&self) -> Vec<bool> {
        let mut k = vec![false; self.dist.len()];
        for (s, &d) in self.dist.iter().enumerate() {
            if d != 0 {
                k[s - d as usize] = true;
            }
        }
        k
    }
    /// Written after the column headers: memory bits, count, then (slot step, distance) pairs.
    pub fn write(&self, hdr: &mut Vec<u8>) {
        hdr.push(self.bits);
        let n = self.dist.iter().filter(|&&d| d != 0).count();
        put_varint(hdr, n as u64);
        let mut prev = 0usize;
        for (s, &d) in self.dist.iter().enumerate() {
            if d != 0 {
                put_varint(hdr, (s - prev) as u64);
                put_varint(hdr, d as u64);
                prev = s;
            }
        }
    }
    /// Read what `write` wrote (first: each pattern's first slot, then the slot count). Every
    /// keyed slot's key must be an earlier slot of the same pattern; damaged input gives
    /// None, never a panic.
    pub fn read(hdr: &[u8], hp: &mut usize, first: &[u32]) -> Option<Plan> {
        let nslots = *first.last()? as usize;
        let bits = *hdr.get(*hp)?;
        *hp += 1;
        let n = heads::rd(hdr, hp)? as usize;
        if !(8..=14).contains(&bits) || n == 0 || n > nslots {
            return None;
        }
        let mut dist = vec![0u16; nslots];
        let (mut s, mut t) = (0usize, 0usize);
        for i in 0..n {
            let step = heads::rd(hdr, hp)? as usize;
            if i > 0 && step == 0 {
                return None;
            }
            s = s.checked_add(step)?;
            let d = heads::rd(hdr, hp)? as usize;
            if s >= nslots {
                return None;
            }
            // the pattern of slot s (slots rise, so the walk goes forward only)
            while first[t + 1] as usize <= s {
                t += 1;
            }
            if d == 0 || d > s - first[t] as usize || d > u16::MAX as usize {
                return None;
            }
            dist[s] = d as u16;
        }
        Some(Plan { dist, bits })
    }
}

/// Lines of a sample for `plan`: each line's pattern and its numeric values as (slot within
/// the pattern, kind, value).
#[derive(Default)]
pub struct Sample {
    pub lines: Vec<(u32, u32, u32)>, // pattern, start and end in vals
    pub vals: Vec<(u16, u8, u64)>,
}

/// The memory size (in bits) of a block of this many lines: about 4 entries per line, 2^8 to
/// 2^14. The packer always takes it; the opener refuses a bigger one (heads::read), so a
/// block's memory stays in proportion to its lines.
pub fn mem_bits(lines: usize) -> u8 {
    (usize::BITS - (lines * 4).max(1).leading_zeros()).clamp(8, 14) as u8
}

/// Bit length of x.
#[inline]
fn blen(x: u64) -> u32 {
    64 - x.leading_zeros()
}

#[inline]
fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Candidate keys tried per value slot (the nearest earlier slots whose values change).
const CANDS: usize = 2;

/// Size (in bits) of a memory for about n values.
fn bits_for(n: usize) -> u8 {
    (usize::BITS - (2 * n).max(1).leading_zeros()).clamp(8, 11) as u8
}

/// Choose the keyed slots of a block from a sample: each varying number slot tries its CANDS
/// nearest earlier changing number slots as key on a shared memory, with a rough bit cost per
/// value (the column's own way: a repeat of its recent values, or a step from its last value;
/// keyed: a hit's rank, or a step from the entity's last new value). A slot is keyed by the
/// candidate that saves the most, counting the hits it gives other slots (a "feeder": the
/// first mention of an entity). (Whether a column then takes keyed codes, and whether the
/// block keeps them, is decided on the real numbers by put_nums and pack_block.)
/// Only the slots and columns met in the sample are numbered (big blocks have many patterns).
/// slot_base: each pattern's first slot (one more at the end); col: each slot's column.
#[inline(never)]
pub fn plan(s: &Sample, slot_base: &[u32], col: &[u32], ncols: usize, lines: usize) -> Plan {
    let nslots = col.len();
    let mut plan = Plan { dist: vec![0; nslots], bits: 10 };
    if s.lines.is_empty() {
        return plan;
    }
    // the slots met, numbered in order of first use; per slot: values met, changes from the
    // value before, the last value
    let mut sidx = vec![u32::MAX; nslots];
    let mut gsl: Vec<u32> = Vec::new();
    let mut st: Vec<(u32, u32, u64)> = Vec::new();
    // per slot: values equal to an earlier number of the same line (a copy: the coder finds
    // those already); a small table of the line's numbers (line number, value)
    let mut copies: Vec<u32> = Vec::new();
    let mut inline = [(u32::MAX, 0u64); 64];
    for (li, &(t, a, b)) in s.lines.iter().enumerate() {
        let sb = slot_base[t as usize] as usize;
        for &(k, _, v) in &s.vals[a as usize..b as usize] {
            let sl = sb + k as usize;
            let mut i = sidx[sl] as usize;
            if i == u32::MAX as usize {
                i = gsl.len();
                sidx[sl] = i as u32;
                gsl.push(sl as u32);
                st.push((0, 0, v));
                copies.push(0);
            }
            let e = &mut inline[(v.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize];
            if *e == (li as u32, v) {
                copies[i] += 1;
            } else {
                *e = (li as u32, v);
            }
            let x = &mut st[i];
            x.0 += 1;
            x.1 += (x.2 != v) as u32;
            x.2 = v;
        }
    }
    let n = gsl.len();
    // a value slot changes in at least 1/16 of its lines (and is not mostly a copy); a key
    // changes at least once
    let varies = |i: usize| st[i].1 >= 2 && st[i].1 * 16 >= st[i].0 && copies[i] * 2 < st[i].0;
    let is_key = |i: usize| st[i].1 >= 1;
    // candidates: per value slot met, the distances of up to CANDS nearest earlier keys
    let mut cand: Vec<[u16; CANDS]> = vec![[0; CANDS]; n];
    let mut tdone = vec![false; slot_base.len() - 1];
    let mut near: Vec<usize> = Vec::new();
    let mut any = false;
    for &(t, _, _) in &s.lines {
        if std::mem::replace(&mut tdone[t as usize], true) {
            continue;
        }
        near.clear();
        for sl in slot_base[t as usize] as usize..slot_base[t as usize + 1] as usize {
            let i = sidx[sl] as usize;
            if i == u32::MAX as usize {
                continue;
            }
            if varies(i) {
                for (j, &k) in near.iter().rev().take(CANDS).enumerate() {
                    cand[i][j] = (sl - k) as u16;
                    any = true;
                }
            }
            if is_key(i) {
                near.push(sl);
            }
        }
    }
    if !any {
        return plan;
    }
    // the columns met
    let mut cidx = vec![u32::MAX; ncols];
    let mut nc = 0usize;
    for &sl in &gsl {
        let c = &mut cidx[col[sl as usize] as usize];
        if *c == u32::MAX {
            *c = nc as u32;
            nc += 1;
        }
    }
    // the simulation: own cost per slot, savings per (slot, candidate) incl. what it fed;
    // values met per candidate, and values equal to the key itself (a copy in the same line:
    // the coder finds those already, it is not an entity's memory)
    let mut own = vec![0u64; n];
    let mut save = vec![[0i64; CANDS]; n];
    let mut met = vec![[0u32; CANDS]; n];
    let mut same = vec![[0u32; CANDS]; n];
    let mut colst: Vec<(u64, [u64; 4])> = vec![(0, [u64::MAX; 4]); nc];
    // memory: entries also keep who put each value there (slot * CANDS + j)
    #[derive(Clone, Copy, Default)]
    struct E {
        tag: u64,
        last: u64,
        rec: [u64; R],
        by: [u32; R],
        n: u8,
    }
    let mb = bits_for(s.vals.len() * CANDS);
    let mut mem = vec![E::default(); 1 << mb];
    let mmask = (1usize << mb) - 1;
    let mut reg: Vec<(u8, u64)> = Vec::new();
    for &(t, a, b) in &s.lines {
        let sb = slot_base[t as usize] as usize;
        reg.clear();
        reg.resize(slot_base[t as usize + 1] as usize - sb, (u8::MAX, 0));
        for &(k, kind, v) in &s.vals[a as usize..b as usize] {
            let k = k as usize;
            reg[k] = (kind, v);
            let sl = sb + k;
            let i = sidx[sl] as usize;
            if cand[i][0] == 0 {
                continue;
            }
            // the column's own way (a repeat of a recent value, or a step from its last value)
            let cs = &mut colst[cidx[col[sl] as usize] as usize];
            let oc = if v == cs.0 {
                1
            } else if cs.1.contains(&v) {
                3
            } else {
                2 + blen(zz(v.wrapping_sub(cs.0) as i64)).min(blen(v))
            } as i64;
            if !cs.1.contains(&v) {
                cs.1.copy_within(0..3, 1);
                cs.1[0] = v;
            }
            cs.0 = v;
            own[i] += oc as u64;
            for j in 0..CANDS {
                let d = cand[i][j] as usize;
                if d == 0 {
                    break;
                }
                let (kk, kv) = reg[k - d];
                if kk == u8::MAX {
                    continue;
                }
                met[i][j] += 1;
                same[i][j] += (kv == v) as u32;
                let h = key_hash(kk, kv, kind) ^ j as u64;
                let e = &mut mem[(h >> 32) as usize & mmask];
                let held = e.tag == h;
                let r = if held { e.rec[..e.n as usize].iter().position(|&x| x == v).unwrap_or(R) } else { R };
                let kc = if r < R {
                    1 + 2 * blen(r as u64) as i64
                } else if held {
                    oc.min(3 + blen(zz(v.wrapping_sub(e.last) as i64)) as i64)
                } else {
                    oc
                };
                let me = (i * CANDS + j) as u32;
                save[i][j] += oc - kc;
                if r < R && e.by[r] != me {
                    // the value was put there by another slot: count the hit for it too
                    let by = e.by[r] as usize;
                    save[by / CANDS][by % CANDS] += oc - kc;
                }
                if !held {
                    e.tag = h;
                    e.n = 0;
                }
                if r < R {
                    e.rec.copy_within(0..r, 1);
                    e.by.copy_within(0..r, 1);
                } else {
                    let m = (e.n as usize + 1).min(R);
                    e.rec.copy_within(0..m - 1, 1);
                    e.by.copy_within(0..m - 1, 1);
                    e.n = m as u8;
                    e.last = v;
                }
                e.rec[0] = v;
                e.by[0] = me;
            }
        }
    }
    // a slot is keyed by its best candidate when that saves at least 64 bits and 1/16 of its
    // own cost
    let mut dist = vec![0u16; n];
    let mut keyed = 0usize;
    for i in 0..n {
        let (mut best, mut bj) = (0i64, usize::MAX);
        for j in 0..CANDS {
            if cand[i][j] != 0 && save[i][j] > best && same[i][j] * 2 < met[i][j] {
                best = save[i][j];
                bj = j;
            }
        }
        if bj != usize::MAX && best >= 64 && best * 16 >= own[i] as i64 {
            dist[i] = cand[i][bj];
            keyed += 1;
        }
    }
    if keyed > 0 {
        for i in 0..n {
            plan.dist[gsl[i] as usize] = dist[i];
        }
        // memory size: about 4 entries per line of the block (at most 2^14)
        plan.bits = mem_bits(lines);
    }
    plan
}
