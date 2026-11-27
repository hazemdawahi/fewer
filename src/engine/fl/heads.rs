// Leaner column headers (the coltable idea). A block's header (stream 2) used to give every
// column its mode, value count and byte lengths in the kinds, numbers, text and cache-code
// streams (9% of Android's output). Most of that the opener can work out itself:
// - value counts: from the pattern ids (decoded first) and the pattern table: a column gets
//   one value per line of each pattern with a slot in it. Only columns that a splitting
//   column's words can take values from store their count: columns made after such a word,
//   and the slots after a splitting column's slot in a pattern;
// - byte lengths: each column's values are read in order, so one quick pass over the kinds,
//   texts, cache codes and numbers finds where each column ends (varints are skipped 64 bytes
//   at a time); a grouped column's segments too (their counts follow from its group bytes);
// - a column with one value stores only its kind (its number is written as is).
// The header is written part by part (values of one sort side by side code better):
//   varint(columns), byte(grouped columns), byte(flags: 1 = pattern ids coded with TidModel,
//   2 = splitting columns listed, 4 = every column stores its count, 8 = no kind specials,
//   16 = rules, 32 = ID columns: mode bit 128, ids.rs; 64 = keyed slots: mode bits 256 and
//   512, entity.rs; other bits are refused), [varint(splitting columns), varint(column number
//   - previous) each], [the rules section (rules.rs; a value a rule gives is not in its
//   column: `minus`), varint(length of the rules' answers)], [the keyed plan (entity.rs)],
//   ... the parts below ..., [the rules' answers],
//   1. varint(count) of each column whose count the opener cannot work out,
//   2. byte(kind) of each one-value column (FULL: it has a full entry),
//   3. varint(mode) of each full entry with fewer than SMALL_COL values (a full entry: a
//      column with 2+ values, or with FULL),
//   4. varint(mode) of each full entry with SMALL_COL or more values,
//   5. byte(kind) of each full entry whose values have one kind (mode & 2),
//   6. the group of each pattern slot of each grouped column (mode & 16), in pattern order.
use super::*;

/// The escape byte of a one-value column: a full entry follows.
pub const FULL: u8 = 255;

/// What a value of each kind takes from the text stream: 0 a number (nothing: it is in the
/// numbers), 1 varint(length) + bytes (kind 1), 2 16 bytes (UUID, kinds 100-101), 3
/// varint(n) + n bytes (hex bytes, kinds 102-103), 4 nothing (repeats: 2, 71-77, 80-87).
pub static KIND_USE: [u8; 256] = {
    let mut t = [0u8; 256];
    t[1] = 1;
    t[100] = 2;
    t[101] = 2;
    t[102] = 3;
    t[103] = 3;
    t[2] = 4;
    let mut k = 71;
    while k <= 77 {
        t[k] = 4;
        k += 1;
    }
    k = 80;
    while k <= 87 {
        t[k] = 4;
        k += 1;
    }
    t
};

/// A column as its header gives it, and where its values start in the other streams.
#[derive(Clone, Copy, Default)]
pub struct ColHead {
    pub mode: u64,
    pub same: u16, // the kind of every value (256: kinds in the kinds stream)
    pub count: usize,
    pub k: usize, // start in the kinds
    pub n: usize, // start in its numbers stream (`ns`)
    pub t: usize, // start in the texts
    pub h: usize, // start in the cache codes
    pub ns: u8,   // the stream of its numbers: 4, or 7 (differences)
}

/// A block's column headers; per grouped column: (column, place of its group bytes in the
/// header, segment lengths in bytes); where the ID columns' new UUIDs start in the texts
/// (ids.rs Utail, to the end; the texts' end when the block has no ID columns).
pub struct Heads {
    pub cols: Vec<ColHead>,
    pub grouped: Vec<(usize, usize, [usize; GROUPS])>,
    pub tail: usize,
    pub plan: Option<entity::Plan>,
}

/// Per prefix column: true when its value count must be stored (a slot after the slot of a
/// splitting column in some pattern: a word there moves the line's later values to columns of
/// their own). `split`: the splitting prefix columns.
pub fn stored_counts(col_of: &[u32], first: &[u32], split: &[bool]) -> Vec<bool> {
    let mut st = vec![false; split.len()];
    if !split.iter().any(|&x| x) {
        return st;
    }
    for t in 0..first.len().saturating_sub(1) {
        let mut after = false;
        for &c in &col_of[first[t] as usize..first[t + 1] as usize] {
            let c = c as usize;
            if after {
                st[c] = true;
            }
            after |= split[c];
        }
    }
    st
}

/// The column of each slot of pattern `t` (appended to `out`): patterns with the same text
/// before a slot share its column, numbered in order of first use (`map.len()` is the next
/// number). The text before slot k is the text before slot k - 1, a mark and the piece
/// between them, so a slot is looked up by (column of the slot before it, hash of that piece):
/// no long texts are hashed or compared. Packing and opening run this same function on the
/// same pattern text, so they agree on every number.
#[inline]
pub fn slot_cols<S: std::hash::BuildHasher>(t: &[u8], map: &mut HashMap<(u32, u64), u32, S>, out: &mut Vec<u32>) {
    let (mut prev, mut a) = (u32::MAX, 0);
    for (pos, &c) in t.iter().enumerate() {
        if c == MARK {
            let n = map.len() as u32;
            let id = *map.entry((prev, hash_bytes(&t[a..pos]))).or_insert(n);
            out.push(id);
            prev = id;
            a = pos + 1;
        }
    }
}

/// Value count of each prefix column from the lines of each pattern (`per_tmpl`).
pub fn derived_counts(col_of: &[u32], first: &[u32], per_tmpl: &[u32], nprefix: usize) -> Vec<usize> {
    let mut n = vec![0usize; nprefix];
    for (t, &k) in per_tmpl.iter().enumerate() {
        for &c in &col_of[first[t] as usize..first[t + 1] as usize] {
            n[c as usize] += k as usize;
        }
    }
    n
}

/// A varint at b[*p..] (None: past the end, or longer than 10 bytes).
#[inline]
pub fn rd(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut s = 0;
    loop {
        let x = *b.get(*p)?;
        *p += 1;
        if s > 63 {
            return None;
        }
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return Some(v);
        }
        s += 7;
    }
}

/// The place right after `n` varints starting at b[p..] (None: the stream ends first): a byte
/// below 0x80 ends a varint; the ends are counted 64 bytes at a time, then 8.
#[inline]
pub fn skip_varints(b: &[u8], mut p: usize, mut n: usize) -> Option<usize> {
    if n == 0 {
        return Some(p);
    }
    // 64 bytes at a time while all their ends are needed (a count per byte lane, summed by a
    // multiply: at most 64)
    while p + 64 <= b.len() {
        let mut acc = 0u64;
        for k in 0..8 {
            let w = u64::from_le_bytes(b[p + 8 * k..p + 8 * k + 8].try_into().unwrap());
            acc += (!w >> 7) & 0x0101_0101_0101_0101;
        }
        let c = (acc.wrapping_mul(0x0101_0101_0101_0101) >> 56) as usize;
        if c >= n {
            break;
        }
        n -= c;
        p += 64;
    }
    while p + 8 <= b.len() {
        let w = u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
        let mut ends = !w & 0x8080_8080_8080_8080;
        let c = ends.count_ones() as usize;
        if c < n {
            n -= c;
            p += 8;
            continue;
        }
        // the n-th end is in this word
        for _ in 1..n {
            ends &= ends - 1;
        }
        return Some(p + ends.trailing_zeros() as usize / 8 + 1);
    }
    while p < b.len() {
        n -= (b[p] < 0x80) as usize;
        p += 1;
        if n == 0 {
            return Some(p);
        }
    }
    None
}

/// How many of b[a..a + n] are at least x (None: past the end).
#[inline]
fn count_ge(b: &[u8], a: usize, n: usize, x: u8) -> Option<usize> {
    Some(b.get(a..a.checked_add(n)?)?.iter().filter(|&&c| c >= x).count())
}

/// The end of one text item of kind use u (see KIND_USE) at txt[p..].
#[inline]
fn skip_text(txt: &[u8], mut p: usize, u: u8) -> Option<usize> {
    let e = match u {
        1 | 3 => {
            let len = rd(txt, &mut p)? as usize;
            p.checked_add(len)?
        }
        2 => p + 16,
        _ => p,
    };
    if e > txt.len() {
        return None;
    }
    Some(e)
}

/// Read a block's column headers and find where each column's values start. `col_of`, `first`:
/// the pattern table's slots (slot_columns); `ids`: each line's pattern, the lines of each
/// pattern, and the rules (line_tids): per prefix column the values rules gave (`minus`), and
/// per rule how many (`yes`: a grouped column's segments lose them). Damaged input gives an
/// error, never a panic.
pub fn read(s: &[Vec<u8>], col_of: &[u32], first: &[u32], ids: &Ids) -> Result<Heads, &'static str> {
    const E: &str = "damaged column header";
    let (tid_of, per, minus) = (&ids.tid[..], &ids.per[..], &ids.minus[..]);
    let h = &s[2];
    let mut p = 0;
    let nc = rd(h, &mut p).ok_or(E)? as usize;
    let flags = *h.get(p + 1).ok_or(E)?;
    p += 2; // (the grouped column count, then the flags)
    // ID columns (flag 32): mode bit 128 is allowed only then (ids.rs); keyed slots (flag 64,
    // entity::FLAG): a plan; other bits are refused
    if flags & !(63 | entity::FLAG) != 0 {
        return Err(E);
    }
    let nprefix = col_of.iter().copied().max().map_or(0, |x| x as usize + 1);
    if nc < nprefix || nc > h.len() + nprefix {
        return Err(E);
    }
    let mut split = vec![false; nprefix];
    if flags & 2 != 0 {
        let n = rd(h, &mut p).ok_or(E)? as usize;
        let mut c = 0usize;
        for i in 0..n {
            c = c.checked_add(rd(h, &mut p).ok_or(E)? as usize).ok_or(E)?;
            if c >= nprefix || (i > 0 && c == 0) {
                return Err(E);
            }
            split[c] = true;
        }
    }
    // the rules section (line_tids has read and checked it already), then the length of the
    // rules' answers, which end the header
    let mut hend = h.len();
    if flags & 16 != 0 {
        let mut q = p;
        rules::get_rules(h, &mut q, first.len().saturating_sub(1));
        p = q;
        let alen = rd(h, &mut p).ok_or(E)? as usize;
        hend = h.len().checked_sub(alen).ok_or(E)?;
    }
    let h = &h[..hend];
    // (the keyed plan; a memory bigger than the packer makes for the block's lines is refused)
    let plan = if flags & entity::FLAG != 0 { Some(entity::Plan::read(h, &mut p, first).filter(|pl| pl.bits <= entity::mem_bits(tid_of.len())).ok_or(E)?) } else { None };
    let all = flags & 4 != 0;
    let stored = stored_counts(col_of, first, &split);
    let mut derived = if all { Vec::new() } else { derived_counts(col_of, first, per, nprefix) };
    if !all && !minus.is_empty() {
        if minus.len() != nprefix {
            return Err(E);
        }
        for (d, &m) in derived.iter_mut().zip(minus) {
            *d = d.checked_sub(m as usize).ok_or(E)?;
        }
    }
    let mut cols: Vec<ColHead> = Vec::with_capacity(nc);
    // part 1: the stored counts
    for ci in 0..nc {
        let count = if all || ci >= nprefix || stored[ci] { rd(h, &mut p).ok_or(E)? as usize } else { derived[ci] };
        if count > tid_of.len() {
            return Err(E);
        }
        cols.push(ColHead { mode: 2, same: 0, count, ..Default::default() });
    }
    // part 2: the kind of each one-value column (FULL: a full entry)
    let mut full: Vec<bool> = cols.iter().map(|e| e.count >= 2).collect();
    for (e, f) in cols.iter_mut().zip(full.iter_mut()) {
        if e.count == 1 {
            let k = *h.get(p).ok_or(E)?;
            p += 1;
            *f = k == FULL;
            e.same = k as u16;
        }
    }
    // parts 3-4: modes of the full entries (small columns first); part 5: their kinds
    for big in [false, true] {
        for (e, &f) in cols.iter_mut().zip(&full) {
            if f && (e.count >= SMALL_COL) == big {
                e.mode = rd(h, &mut p).ok_or(E)?;
                // an ID column (128) only with flag 32, never grouped (ids.rs); keyed codes
                // (256, always with cache codes 4; 512: steps) only with a plan, and not with
                // a list (8), groups (16), second differences (32), splitting (64) or an ID
                // column (128)
                let m = e.mode;
                if m >= 1024 || (m & 128 != 0 && (flags & 32 == 0 || m & 16 != 0)) || (m & 256 != 0 && (plan.is_none() || m & 252 != 4)) || (m & 512 != 0 && m & 256 == 0) {
                    return Err(E);
                }
            }
        }
    }
    for (e, &f) in cols.iter_mut().zip(&full) {
        if f {
            e.same = if e.mode & 2 != 0 {
                p += 1;
                *h.get(p - 1).ok_or(E)? as u16
            } else {
                256
            };
        }
    }
    // part 6: the group of each slot of the grouped columns (prefix columns of differences),
    // one column after the other; each segment's count of numbers follows from the lines of
    // each pattern (turned into byte lengths below)
    let mut grouped: Vec<(usize, usize, [usize; GROUPS])> = Vec::new();
    let mut gi_of: Vec<u32> = Vec::new();
    if cols.iter().any(|e| e.mode & 16 != 0) {
        gi_of = vec![u32::MAX; nc];
        for (ci, e) in cols.iter().enumerate() {
            if e.mode & 16 != 0 {
                if ci >= nprefix || e.mode & 4 != 0 {
                    return Err(E);
                }
                gi_of[ci] = grouped.len() as u32;
                grouped.push((ci, 0, [0; GROUPS]));
            }
        }
        let mut nb = vec![0usize; grouped.len()];
        for &c in col_of {
            if gi_of[c as usize] != u32::MAX {
                nb[gi_of[c as usize] as usize] += 1;
            }
        }
        for (g, &n) in grouped.iter_mut().zip(&nb) {
            g.1 = p;
            p += n;
        }
        if p > h.len() {
            return Err(E);
        }
        let mut at: Vec<usize> = grouped.iter().map(|g| g.1).collect();
        for t in 0..per.len() {
            // (the values this pattern's rules gave are not in their columns)
            let (mut ri, re) = if ids.rules.is_empty() { (0, 0) } else { (ids.rfirst[t] as usize, ids.rfirst[t + 1] as usize) };
            for (k, &c) in col_of[first[t] as usize..first[t + 1] as usize].iter().enumerate() {
                let gi = gi_of[c as usize];
                let mut n = per[t] as usize;
                while ri < re && (ids.rules[ri].slot as usize) < k {
                    ri += 1;
                }
                if ri < re && ids.rules[ri].slot as usize == k {
                    n = n.checked_sub(ids.yes[ri] as usize).ok_or(E)?;
                }
                if gi != u32::MAX {
                    let g = h[at[gi as usize]] as usize;
                    at[gi as usize] += 1;
                    *grouped[gi as usize].2.get_mut(g).ok_or(E)? += n;
                }
            }
        }
    }
    if p != h.len() {
        return Err(E);
    }
    let split_nums = split_nums_on();
    for e in cols.iter_mut() {
        e.ns = if e.mode & 33 != 0 && split_nums { 7 } else { 4 };
    }
    // where each column starts: after the columns before it
    let (kinds, txt, codes) = (&s[3], &s[5], &s[6]);
    let (mut ko, mut to, mut ho, mut no) = (0usize, 0usize, 0usize, [0usize; 2]);
    for (ci, e) in cols.iter_mut().enumerate() {
        let w = (e.ns == 7) as usize;
        let nums = &s[if w == 1 { 7 } else { 4 }];
        e.k = ko;
        e.t = to;
        e.h = ho;
        e.n = no[w];
        let ks: &[u8] = if e.same == 256 {
            ko = ko.checked_add(e.count).ok_or(E)?;
            kinds.get(e.k..ko).ok_or(E)?
        } else {
            &[]
        };
        // its numbers and text items (an ID column's UUIDs are numbers)
        let mut nnum = 0usize;
        let mut q = to;
        let use_of = |k: u8| if e.mode & 128 != 0 && (k == 100 || k == 101) { 0 } else { KIND_USE[k as usize] };
        if e.same < 256 {
            let u = use_of(e.same as u8);
            if u == 0 {
                nnum = e.count;
            } else if u != 4 {
                for _ in 0..e.count {
                    q = skip_text(txt, q, u).ok_or(E)?;
                }
            }
        } else {
            for &k in ks {
                let u = use_of(k);
                nnum += (u == 0) as usize;
                if u != 0 && u != 4 {
                    q = skip_text(txt, q, u).ok_or(E)?;
                }
            }
        }
        let nh = if e.mode & 260 != 0 { nnum } else { 0 };
        let end = if e.mode & 16 != 0 {
            // a grouped column: segment by segment (counts to byte lengths)
            let g = &mut grouped[gi_of[ci] as usize].2;
            if g.iter().sum::<usize>() != nnum || q != to {
                return Err(E);
            }
            let mut x = e.n;
            for len in g.iter_mut() {
                let y = skip_varints(nums, x, *len).ok_or(E)?;
                (*len, x) = (y - x, y);
            }
            x
        } else {
            let nvar = if e.mode & 256 != 0 {
                // keyed: numbers for new values (CK) and steps (KN) only
                codes.get(ho..ho.checked_add(nnum).ok_or(E)?).ok_or(E)?.iter().filter(|&&c| c as usize == CK || c as usize == entity::KN).count()
            } else if e.mode & 4 != 0 {
                count_ge(codes, ho, nnum, CK as u8).ok_or(E)?
            } else {
                nnum
            };
            skip_varints(nums, e.n, nvar).ok_or(E)?
        };
        to = q;
        ho = ho.checked_add(nh).ok_or(E)?;
        no[w] = end;
        if ho > codes.len() {
            return Err(E);
        }
    }
    // the ID columns' new UUIDs: the rest of the texts (ids.rs Utail: at least its length)
    let tail = if flags & 32 != 0 { to } else { txt.len() };
    if tail > txt.len() || (flags & 32 != 0 && tail == txt.len()) {
        return Err(E);
    }
    Ok(Heads { cols, grouped, tail, plan })
}
