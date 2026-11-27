// Fast opening: the same lines as unpack_block (main.rs, kept as the reference:
// FL_OLDOPEN=1), written straight into the final buffer. Every block takes the loop below
// (the earlier piece loop, unpack_lines_v1, is deleted: M7).
// - each pattern is cut once into pieces (Op): fixed text, then the value of a column whose
//   handler (H_*) is known per block: numbers as they are / differences / second differences /
//   cache codes, in decimal or of another kind, the clock column of a grouped block; anything
//   else (kinds stream, texts, words, hex) goes through value_any,
// - each column keeps the text of its last number (16 bytes, kept in a u128): about 3 numbers
//   in 4 repeat the column's last one, and a repeat is one 16-byte copy; new numbers are
//   printed 8 digits at a time (swar8), clock times and IPv4 addresses straight into a u128,
// - a line whose worst case (each piece's text + 16 bytes) fits in the buffer is written with
//   no bounds checks (w16::<false>); pieces, columns and slots are checked once per block,
// - the cache codes' recent list and order are updated without branches (update_fast),
// - slot ids: one lookup per slot by (earlier slot's id, piece) instead of hashing whole
//   prefixes; pattern ids: TidModel's opening side with its state in local variables;
//   grouped blocks: a reused scratch buffer and the merge written straight into the output
//   (group::ungroup_into, a heap with packed keys).
#![allow(dead_code)]
use super::*;
use split::wild_copy;

/// "00" "01" ... "99" as little-endian u16 (the tens digit first)
static D2: [u16; 100] = {
    let mut t = [0u16; 100];
    let mut i = 0;
    while i < 100 {
        t[i] = (b'0' + (i / 10) as u8) as u16 | ((b'0' + (i % 10) as u8) as u16) << 8;
        i += 1;
    }
    t
};

/// 0-255 in decimal: text (first byte lowest) | length << 24
static OCT: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let (a, b, c) = (b'0' + (i / 100) as u8, b'0' + (i / 10 % 10) as u8, b'0' + (i % 10) as u8);
        t[i] = if i >= 100 {
            a as u32 | (b as u32) << 8 | (c as u32) << 16 | 3 << 24
        } else if i >= 10 {
            b as u32 | (c as u32) << 8 | 2 << 24
        } else {
            c as u32 | 1 << 24
        };
        i += 1;
    }
    t
};

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

/// Digits of x in decimal (1 for 0).
#[inline(always)]
fn digit_count(x: u64) -> usize {
    let b = 64 - (x | 1).leading_zeros() as usize;
    let t = (b * 1233) >> 12; // about b * log10(2)
    (t + (t < 20 && x >= POW10[t]) as usize).max(1)
}

/// The 8 decimal digits of x < 10^8 (with leading zeros) as ASCII, the first digit in the
/// lowest byte: x is cut into 4 + 4 digits, each half into 2 + 2, each pair into 1 + 1, all
/// lanes at once (the divisions are multiply-shifts, exact in these ranges).
#[inline(always)]
fn swar8(x: u64) -> u64 {
    let hi = x / 10000;
    let v = hi | (x - hi * 10000) << 32;
    let a = ((v * 10486) >> 20) & 0x0000_007f_0000_007f;
    let v = a | (v - a * 100) << 16;
    let c = ((v * 103) >> 10) & 0x000f_000f_000f_000f;
    (c | (v - c * 10) << 8) | 0x3030_3030_3030_3030
}

/// Write x in decimal, zero-padded to `width` digits, at out[o..]; returns the end. Writes up
/// to 16 bytes past the end (`out` must have room).
#[inline(always)]
fn put_num(out: &mut [u8], o: usize, x: u64, width: usize) -> usize {
    let n = digit_count(x).max(width);
    if n <= 8 {
        let s = swar8(x) >> (8 * (8 - n));
        out[o..o + 8].copy_from_slice(&s.to_le_bytes());
    } else if n <= 16 {
        let hi = x / 100_000_000;
        let s = swar8(hi) >> (8 * (16 - n));
        out[o..o + 8].copy_from_slice(&s.to_le_bytes());
        out[o + n - 8..o + n].copy_from_slice(&swar8(x - hi * 100_000_000).to_le_bytes());
    } else {
        return put_num_long(out, o, x, n);
    }
    o + n
}

/// put_num for 17-20 digits (the first 1-4, then 16 as two times 8).
#[cold]
#[inline(never)]
fn put_num_long(out: &mut [u8], o: usize, x: u64, n: usize) -> usize {
    let hi = x / 10_000_000_000_000_000;
    let lo = x - hi * 10_000_000_000_000_000;
    let mid = lo / 100_000_000;
    let s = swar8(hi) >> (8 * (24 - n));
    out[o..o + 8].copy_from_slice(&s.to_le_bytes());
    let e = o + n - 16;
    out[e..e + 8].copy_from_slice(&swar8(mid).to_le_bytes());
    out[e + 8..e + 16].copy_from_slice(&swar8(lo - mid * 100_000_000).to_le_bytes());
    o + n
}

/// x in decimal, zero-padded to `width` digits, as text in a u128 (first byte lowest) and its
/// length; length 0 when it has more than 16 digits.
#[inline(always)]
fn num_text(x: u64, width: usize) -> (u128, usize) {
    let n = digit_count(x).max(width);
    if n <= 8 {
        ((swar8(x) >> (8 * (8 - n))) as u128, n)
    } else if n <= 16 {
        let hi = x / 100_000_000;
        let a = swar8(hi) >> (8 * (16 - n));
        let b = swar8(x - hi * 100_000_000);
        (a as u128 | (b as u128) << (8 * (n - 8)), n)
    } else {
        (0, 0)
    }
}

/// A varint at b[*p..] (8 bytes read at once when there is room: no branch on its length).
#[inline(always)]
fn varint(b: &[u8], p: &mut usize) -> u64 {
    if *p + 8 <= b.len() {
        let w = u64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
        let stop = !w & 0x8080_8080_8080_8080;
        if stop != 0 {
            *p += (stop.trailing_zeros() as usize + 1) >> 3;
            let w = w & (stop ^ (stop - 1)) & 0x7f7f_7f7f_7f7f_7f7f;
            let x = (w & 0x007f_007f_007f_007f) | ((w >> 1) & 0x3f80_3f80_3f80_3f80);
            let x = (x & 0x0000_3fff_0000_3fff) | ((x >> 2) & 0x0fff_c000_0fff_c000);
            return (x & 0x0fff_ffff) | ((x >> 4) & 0x00ff_ffff_f000_0000);
        }
    }
    get_varint(b, p)
}

/// Copy src[s..s + n] to out[o..] (16 bytes at a time while the source has room for it).
#[inline(always)]
fn put_bytes(out: &mut [u8], o: usize, src: &[u8], s: usize, n: usize) -> usize {
    if s + n + 16 <= src.len() {
        wild_copy(out, o, src, s, n);
    } else {
        out[o..o + n].copy_from_slice(&src[s..s + n]);
    }
    o + n
}

/// Copy fixed text tb[a..a + n] to out[o..] (tb has 16 bytes of room after its end).
#[inline(always)]
fn put_lit(out: &mut [u8], o: usize, tb: &[u8], a: usize, n: usize) -> usize {
    debug_assert!(a + n + 16 <= tb.len());
    // SAFETY: the caller checked a + n + 16 <= tb.len() for every piece
    let x = unsafe { (tb.as_ptr().add(a) as *const [u8; 16]).read_unaligned() };
    out[o..o + 16].copy_from_slice(&x);
    if n > 16 {
        wild_copy(out, o + 16, tb, a + 16, n - 16);
    }
    o + n
}

/// A column being opened: where it is in the kinds, numbers, texts and cache-code streams.
#[derive(Clone, Copy, Default)]
struct Cur {
    k: usize,
    n: usize,
    t: usize,
    h: usize,
    prev: i64,
    pd: i64,
    cache: u32, // index into the caches + 1 (0: not cache coded)
    same: u16,  // the kind of every value (256: kinds in the kinds stream)
    ns: u8,     // the stream of its numbers (4 or 7)
    delta: bool,
    dod: bool,
    dict: bool,
    split: bool,
    idc: bool, // an ID column (ids.rs, mode bit 128): new long IDs as steps, UUIDs as numbers
    gs: u64,   // its last new generator long ID's state (ids.rs)
    text: u128, // the text of its last number (first byte lowest), when tl > 0
    tl: u8,     // its length (0: none kept)
    hd: u8,  // how the line loop opens its values (H_*)
    w: u8,   // zero-padded width (kinds 3 + w)
    kc: u8,  // kind class of its values (one kind)
    keyed: bool, // keyed codes (entity.rs; mode bit 256)
    steps: bool, // ... with steps from the entity's last new value (mode bit 512)
}

/// State of the cache-coded columns (rare enough to sit outside the hot loop).
struct Cached<'a> {
    caches: Vec<Cache>,
    lists: Vec<Vec<u64>>,
    id_list: Vec<u64>, // long IDs in the order they first appeared
    codes: &'a [u8],
    use_ks: bool,
}

impl Cache {
    /// Cache::update without branches on the data (the same result).
    #[inline(always)]
    fn update_fast(&mut self, v: u64, p: usize, eq_t: bool, eq_k: bool) {
        let h = [self.hits[0] + (p == 0) as u32, self.hits[1] + eq_t as u32, self.hits[2] + eq_k as u32, 0];
        self.hits = [h[0], h[1], h[2]];
        // the same three compare-swaps of the specials' order: (0, 1), (1, 2), (0, 1)
        let [mut a, mut b, mut c] = self.ord;
        let sw = h[b as usize & 3] > h[a as usize & 3];
        (a, b) = if sw { (b, a) } else { (a, b) };
        let sw = h[c as usize & 3] > h[b as usize & 3];
        (b, c) = if sw { (c, b) } else { (b, c) };
        let sw = h[b as usize & 3] > h[a as usize & 3];
        (a, b) = if sw { (b, a) } else { (a, b) };
        self.ord = [a, b, c];
        // the recent list: v moves to the front (from place p, or new); p = 0 (v is m[0]
        // already: the most common case) leaves it as it is
        if p != 0 {
            let p = p.min(NM - 1);
            let m = &mut self.m;
            for i in (1..NM).rev() {
                m[i] = if i <= p { m[i - 1] } else { m[i] };
            }
            m[0] = v;
        }
    }
}

/// The next value of a cache-coded column (see put_nums).
#[inline(always)]
fn cached_value(cx: &mut Cached, cu: &mut Cur, nums: &[u8], kind: u8, t: u64, kl: u64) -> u64 {
    let ci = cu.cache as usize - 1;
    let code = cx.codes[cu.h] as usize;
    cu.h += 1;
    if code >= CK {
        return cached_new(cx, cu, nums, kind, t, kl, ci, code);
    }
    debug_assert!(ci < cx.caches.len());
    // SAFETY: cu.cache is 1 + the index of the cache unpack_lines made for this column (and
    // caches never shrink), so ci < cx.caches.len()
    let c = unsafe { cx.caches.get_unchecked_mut(ci) };
    // without branches on the code: codes 0-2 are the specials in their order (the previous
    // value, the slot's last value, the kind's last number), 3-8 the recent list
    let o = c.ord[code.min(2)];
    let vs = if o == 0 { c.m[0] } else { if o == 1 { t } else { kl } };
    let r = code.saturating_sub(2).min(NM - 1);
    let vr = c.m[r];
    let hi = code >= 3;
    let v = if hi { vr } else { vs };
    // (the previous value is m[0]: its place is 0 without a search)
    let p = if hi { r } else if o == 0 { 0 } else { c.pos(v) };
    c.update_fast(v, p, v == t, v == kl && cx.use_ks);
    v
}

/// cached_value in the unchecked line loop: code 0 with the previous value first in the order
/// (the most common case) without the general steps. update_fast with p = 0 adds 1 to
/// hits[0], which was the most (the order is sorted by hits after every update), so the order
/// keeps 0 first and only its places 1 and 2 may swap; the recent list stays as it is: the
/// same state as cached_value leaves.
#[inline(always)]
fn cached_value_hot(cx: &mut Cached, cu: &mut Cur, nums: &[u8], kind: u8, t: u64, kl: u64) -> u64 {
    let ci = cu.cache as usize - 1;
    if cx.codes.get(cu.h) == Some(&0) {
        debug_assert!(ci < cx.caches.len());
        // SAFETY: as in cached_value
        let c = unsafe { cx.caches.get_unchecked_mut(ci) };
        if c.ord[0] == 0 {
            cu.h += 1;
            let v = c.m[0];
            let h1 = c.hits[1] + (v == t) as u32;
            let h2 = c.hits[2] + (v == kl && cx.use_ks) as u32;
            c.hits = [c.hits[0] + 1, h1, h2];
            let (b, d) = (c.ord[1], c.ord[2]);
            let h = [c.hits[0], h1, h2, 0];
            if h[d as usize & 3] > h[b as usize & 3] {
                c.ord = [0, d, b];
            }
            return v;
        }
    }
    cached_value(cx, cu, nums, kind, t, kl)
}

/// cached_value for a value not among the candidates (codes CK and up).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn cached_new(cx: &mut Cached, cu: &mut Cur, nums: &[u8], kind: u8, t: u64, kl: u64, ci: usize, code: usize) -> u64 {
    let x = varint(nums, &mut cu.n);
    let c = &mut cx.caches[ci];
    let li = &mut cx.lists[ci];
    let v = if code == CK + 1 {
        if cu.dict { li[x as usize] } else { cx.id_list[cx.id_list.len() - 1 - x as usize] }
    } else if code == CK && cu.idc && kind == LONG_ID {
        // an ID column's new long ID: its generator step (ids.rs; code CK + 2: the value)
        ids::gen_value(&mut cu.gs, x)
    } else if cu.delta {
        c.m[0].wrapping_add(unzz(x) as u64)
    } else {
        x
    };
    if code != CK + 1 {
        // a new value (code CK; CK + 2: an ID column's long ID that is not a step)
        if kind == LONG_ID {
            cx.id_list.push(v);
        }
        if cu.dict {
            li.push(v);
        }
    }
    let p = c.pos(v);
    c.update_fast(v, p, v == t, v == kl && cx.use_ks);
    v
}

/// A damaged keyed code (a rank the entity does not have, a step without an entity, an
/// unknown code): refused (the block's opening is guarded).
#[cold]
#[inline(never)]
fn bad_key() -> ! {
    panic!("damaged keyed code")
}

/// The next value of a column with keyed codes (entity.rs, put_keyed) and its rank in the
/// entity `ent` (key hash, entry, held, looked up) of this line's key: R for any code but a hit.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn keyed_value(cx: &mut Cached, cu: &mut Cur, nums: &[u8], kind: u8, t: u64, kl: u64, mem: Option<&entity::Mem>, ent: (u64, usize, bool, bool)) -> (u64, usize) {
    let code = cx.codes[cu.h] as usize;
    if code < CK {
        // a cache code (keyed columns always have them: heads::read)
        let v = cached_value(cx, cu, nums, kind, t, kl);
        cu.prev = v as i64;
        return (v, entity::R);
    }
    cu.h += 1;
    let m = mem.filter(|_| ent.2);
    let (v, r) = match (code, m) {
        (entity::KH..entity::KN, Some(m)) if code - entity::KH < m.count(ent.1) => (m.at(ent.1, code - entity::KH), code - entity::KH),
        (entity::KN, Some(m)) if cu.steps => (m.last(ent.1).wrapping_add(unzz(varint(nums, &mut cu.n)) as u64), entity::R),
        (CK, _) => {
            let x = varint(nums, &mut cu.n);
            (if cu.delta { (cu.prev as u64).wrapping_add(unzz(x) as u64) } else { x }, entity::R)
        }
        _ => bad_key(),
    };
    let c = &mut cx.caches[cu.cache as usize - 1];
    let p = c.pos(v);
    c.update_fast(v, p, v == t, v == kl && cx.use_ks);
    cu.prev = v as i64;
    (v, r)
}

/// The column and slot number of a slot after a word of a splitting column (made on first
/// use, as packing numbers them).
#[cold]
#[inline(never)]
fn split_slot(sub: &mut FxMap<(u32, u64), u32>, sub_slot: &mut FxMap<(u32, u32), u32>, ncols: &mut u32, slot_last: &mut Vec<u64>, c: u32, ctx: u64, slot: usize) -> (usize, usize) {
    let ci = sub_col(sub, ncols, c, ctx);
    let ns = slot_last.len() as u32;
    let sid = *sub_slot.entry((ci, slot as u32)).or_insert(ns);
    if sid == ns {
        slot_last.push(0);
    }
    (ci as usize, sid as usize)
}

/// Each byte as two hex digits, the high one first (as a little-endian u16): lower case, then
/// upper case.
static HEX2: [[u16; 256]; 2] = {
    let mut t = [[0u16; 256]; 2];
    let mut i = 0;
    while i < 256 {
        let (lo, up) = (b"0123456789abcdef", b"0123456789ABCDEF");
        t[0][i] = lo[i >> 4] as u16 | (lo[i & 15] as u16) << 8;
        t[1][i] = up[i >> 4] as u16 | (up[i & 15] as u16) << 8;
        i += 1;
    }
    t
};

/// A UUID / hex bytes value (kinds 100-103) at out[o..]; returns the end. Two digits per
/// byte from HEX2 (upper case for odd kinds); a UUID (kinds 100-101: 16 bytes) is made in a
/// buffer with its dashes (8-4-4-4-12 digits) and written at once.
#[inline(never)]
fn put_hex(out: &mut [u8], mut o: usize, txt: &[u8], cu: &mut Cur, kind: u8) -> usize {
    let tab = &HEX2[(kind & 1) as usize];
    let n = if kind < 102 { 16 } else { get_varint(txt, &mut cu.t) as usize };
    let src = &txt[cu.t..cu.t + n];
    if kind < 102 {
        let mut b = [b'-'; 36];
        let mut w = 0;
        for (q, &x) in src.iter().enumerate() {
            // (a dash before bytes 4, 6, 8 and 10)
            w += matches!(q, 4 | 6 | 8 | 10) as usize;
            b[w..w + 2].copy_from_slice(&tab[x as usize].to_le_bytes());
            w += 2;
        }
        out[o..o + 36].copy_from_slice(&b);
        o += 36;
    } else {
        for (q, &x) in src.iter().enumerate() {
            out[o + 2 * q..o + 2 * q + 2].copy_from_slice(&tab[x as usize].to_le_bytes());
        }
        o += 2 * n;
    }
    cu.t += n;
    o
}

/// A number of a special kind (IPv4, long ID, hex number, clock times with dots or with no
/// padding) at out[o..]; returns the end.
#[inline(never)]
fn put_special(out: &mut [u8], mut o: usize, kind: u8, v: u64) -> usize {
    if kind == 30 {
        for k in (0..4).rev() {
            o = put_num(out, o, (v >> (8 * k)) & 255, 0);
            out[o] = b'.';
            o += (k > 0) as usize;
        }
    } else if kind == LONG_ID {
        if v & 1 == 1 {
            out[o] = b'-';
            o += 1;
        }
        o = put_num(out, o, v >> 1, 0);
    } else if (HEXNUM..HEXNUM + 18).contains(&kind) {
        let t = kind - HEXNUM;
        let (w, digits) = ((t % 9 + 8) as usize, if t >= 9 { b"0123456789ABCDEF" } else { b"0123456789abcdef" });
        for q in (0..w).rev() {
            out[o] = digits[(v >> (4 * q)) as usize & 15];
            o += 1;
        }
    } else if (110..124).contains(&kind) {
        // H.MM.SS[.F]
        let t = kind - 110;
        let (hw, fd) = ((t & 1) + 1, (t >> 1) as usize);
        let p10 = POW10[fd];
        let secs = v / p10;
        o = put_num(out, o, secs / 3600, hw as usize);
        out[o] = b'.';
        o = put_num(out, o + 1, secs / 60 % 60, 2);
        out[o] = b'.';
        o = put_num(out, o + 1, secs % 60, 2);
        if fd > 0 {
            out[o] = b'.';
            o = put_num(out, o + 1, v % p10, fd);
        }
    } else if kind == 68 {
        // H:M:S:F, nothing zero-padded
        let secs = v / 1000;
        o = put_num(out, o, secs / 3600, 0);
        out[o] = b':';
        o = put_num(out, o + 1, secs / 60 % 60, 0);
        out[o] = b':';
        o = put_num(out, o + 1, secs % 60, 0);
        out[o] = b':';
        o = put_num(out, o + 1, v % 1000, 0);
    } else if kind == NAMEK {
        let t = NAMES[v as usize];
        out[o..o + t.len()].copy_from_slice(t);
        o += t.len();
    } else if (HEXS..HEXS + 6).contains(&kind) {
        let t = kind - HEXS;
        let (w, digits) = ((t % 3 + 5) as usize, if t >= 3 { b"0123456789ABCDEF" } else { b"0123456789abcdef" });
        for q in (0..w).rev() {
            out[o] = digits[(v >> (4 * q)) as usize & 15];
            o += 1;
        }
    } else if (SPN..=SPN + SPN_MAX).contains(&kind) {
        // spaces, then the number: w characters in all
        let w = (kind - SPN) as usize;
        let n = digit_count(v);
        out[o..o + w.max(n)].fill(b' ');
        o = put_num(out, o + w.saturating_sub(n), v, 0);
    } else {
        // a plain number, zero-padded for kinds 3 + w
        o = put_num(out, o, v, if kind >= 3 { (kind - 3) as usize } else { 0 });
    }
    o
}

/// slot_columns (main.rs) without hashing whole prefixes: the text before a slot is the text
/// before the slot before it, its MARK, and the piece between them, so its id is looked up by
/// (the earlier slot's id, the piece). Same ids: equal texts have equal pieces all the way.
#[inline(never)]
fn slot_columns_fast(tb: &[u8]) -> (Vec<u32>, Vec<u32>) {
    // the templates: up to the last '\n' (slot_columns skips what follows it)
    let end = tb.iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
    let marks = tb[..end].iter().filter(|&&c| c == MARK).count();
    let lines = tb[..end].iter().filter(|&&c| c == b'\n').count();
    let mut col_of: Vec<u32> = Vec::with_capacity(marks);
    let mut first: Vec<u32> = Vec::with_capacity(lines + 1);
    // key: a hash of the earlier slot's id and the piece; both are compared on a hit
    let mut ids: FxMap<u64, u32> = FxMap::with_capacity_and_hasher(marks, Default::default());
    let mut piece: Vec<(u32, u32, u32)> = Vec::with_capacity(marks); // per id: parent, piece in tb
    let (mut parent, mut a) = (u32::MAX, 0usize);
    if end > 0 {
        first.push(0);
    }
    for (pos, &c) in tb[..end].iter().enumerate() {
        if c == MARK {
            let n = ids.len() as u32;
            let key = piece_key(tb, a, pos, parent);
            let id = *ids.entry(key).or_insert(n);
            if id == n {
                piece.push((parent, a as u32, pos as u32));
            } else {
                let (pp, pa, pb) = piece[id as usize];
                if pp != parent || tb[pa as usize..pb as usize] != tb[a..pos] {
                    // two keys with one hash: the plain way
                    return slot_columns(tb);
                }
            }
            col_of.push(id);
            parent = id;
            a = pos + 1;
        } else if c == b'\n' {
            parent = u32::MAX;
            a = pos + 1;
            if a < end {
                first.push(col_of.len() as u32);
            }
        }
    }
    first.push(col_of.len() as u32);
    (col_of, first)
}

/// A hash of a slot's earlier slot id and the piece tb[a..b] between them.
#[inline]
fn piece_key(tb: &[u8], a: usize, b: usize, parent: u32) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = ((parent as u64) << 32 | (b - a) as u64).wrapping_mul(K);
    let mut p = a;
    while p + 8 <= b {
        h = (h ^ u64::from_le_bytes(tb[p..p + 8].try_into().unwrap())).wrapping_mul(K);
        h ^= h >> 29;
        p += 8;
    }
    if p < b {
        let mut t = 0u64;
        for (i, &c) in tb[p..b].iter().enumerate() {
            t |= (c as u64) << (8 * i);
        }
        h = (h ^ t).wrapping_mul(K);
        h ^= h >> 29;
    }
    h
}

/// line_tids (main.rs) with TidModel::step for opening written out: the model's state in
/// local variables, the last 4 ids kept at hand. The same ids.
#[inline(never)]
fn line_tids_fast(p: &Packed, patterns: usize) -> Vec<u32> {
    let s = &p.streams;
    let mut hp = 0;
    get_varint(&s[2], &mut hp);
    get_varint(&s[2], &mut hp);
    if s[2][hp] & 1 == 0 {
        return Vec::new(); // (unused: varint ids are read by line_tids)
    }
    const NO: usize = usize::MAX;
    let nb = usize::BITS - patterns.max(1).leading_zeros();
    let mut hist: Vec<u32> = Vec::with_capacity(p.lines);
    let mut table = vec![0u32; 1 << 16];
    let mut succ = vec![[u32::MAX; 4]; patterns + 1];
    let mut run = vec![0u8; patterns + 1];
    let (mut m, mut mlen, mut last, mut seen) = (NO, 0usize, 0usize, 0u32);
    let mut pm = [P_HALF; 32];
    let mut ph = [P_HALF; 64];
    let mut pnew = [P_HALF; 4];
    let mut ptree = vec![P_HALF; 2 << nb];
    let mut dec = ozip::RcDec::new(&s[1]);
    // the last 3 ids (h3 the newest): with the new id, the 4 the match finder hashes
    let (mut h1, mut h2, mut h3) = (0u32, 0u32, 0u32);
    let mut ctx = 0usize;
    for n in 1..=p.lines {
        let st = run[ctx] as usize;
        let mi = (m != NO) as usize;
        let guess = if mi == 1 { hist[m] } else { u32::MAX };
        let mut id = u32::MAX;
        let matched = mi == 1 && dec.bit(&mut pm[mlen.min(15) * 2 + last]) == 0;
        let l = succ[ctx];
        if matched {
            id = guess;
        } else {
            for (r, &x) in l.iter().enumerate() {
                if x == u32::MAX {
                    break;
                }
                if x == guess {
                    continue; // the match already said no
                }
                if dec.bit(&mut ph[((r * 4 + st) * 2 + last) * 2 + mi]) == 0 {
                    id = x;
                    break;
                }
            }
            if id == u32::MAX {
                if dec.bit(&mut pnew[st]) == 0 {
                    id = seen;
                } else {
                    let mut k = 1usize;
                    for _ in 0..nb {
                        k = k << 1 | dec.bit(&mut ptree[k]) as usize;
                    }
                    id = (k - (1 << nb)) as u32;
                }
            }
        }
        // the followers of the previous pattern: this id moves to the front
        // the first place of id in the list (4: not there), without branches
        let hit = (l[0] == id) as u32 | ((l[1] == id) as u32) << 1 | ((l[2] == id) as u32) << 2 | ((l[3] == id) as u32) << 3 | 16;
        let pos = hit.trailing_zeros().min(4) as usize;
        // move to front without a loop: the entries before pos move up one
        succ[ctx] = [id, if pos >= 1 { l[0] } else { l[1] }, if pos >= 2 { l[1] } else { l[2] }, if pos >= 3 { l[2] } else { l[3] }];
        run[ctx] = if pos == 0 { (st + 1).min(3) as u8 } else { 0 };
        last = (matched || pos == 0) as usize;
        seen += (id == seen) as u32;
        hist.push(id);
        ctx = id as usize + 1;
        if matched {
            m += 1;
            mlen += 1;
        } else {
            m = NO;
            mlen = 0;
        }
        let h0 = h1;
        (h1, h2, h3) = (h2, h3, id);
        if n >= 4 {
            let x = (h0 as u64) | (h1 as u64) << 16 | (h2 as u64) << 32 | (h3 as u64) << 48;
            let k = (x.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize;
            if m == NO {
                let q = table[k] as usize;
                if q >= 4 && hist[q - 4..q] == [h0, h1, h2, h3] {
                    m = q;
                }
            }
            table[k] = n as u32;
        }
    }
    hist
}

thread_local! {
    /// A grouped block's lines in group order, their ends and clock times (kept between blocks).
    static SCRATCH: std::cell::RefCell<(Vec<u8>, Vec<usize>, Vec<i64>)> = Default::default();
}

/// Rebuild a block's lines (`size` bytes, known from the block's header) into out[o0..];
/// returns the end. `out` must have room for them plus 64 bytes for wide copies.
pub fn unpack_into(p: &mut Packed, out: &mut [u8], o0: usize, size: usize) -> Result<usize, &'static str> {
    unpack_into_c(p, out, o0, size, None)
}

/// fast_lines' end for damaged derived parts (B5; usize::MAX: damaged ID columns, ids.rs).
const E_BAD: usize = usize::MAX - 1;

/// unpack_into with B5's computed parts (comp: the block's derive settings, its side stream
/// marked R_INL): the markers in the patterns' fixed text are put in by the line printer.
pub fn unpack_into_c(p: &mut Packed, out: &mut [u8], o0: usize, size: usize, comp: Option<&derive::Comp>) -> Result<usize, &'static str> {
    let mut t = super::clock::Tick::now();
    let (col_of, first) = slot_columns(&p.streams[0]);
    super::pt(41, &mut t);
    let ids = line_tids(p, &col_of, &first)?;
    super::pt(42, &mut t);
    let hd = heads::read(&p.streams, &col_of, &first, &ids)?;
    super::pt(48, &mut t);
    ungroup(p, &hd, &col_of, &first, &ids)?;
    super::pt(43, &mut t);
    let grouped = p.streams.len() > 9 && !p.streams[9].is_empty();
    if grouped {
        // the lines come in group order: open them into a buffer (kept for the next block, so
        // its memory is not mapped again each time), then put them back in order
        let (mut tmp, mut ends, mut times) = SCRATCH.with(|x| std::mem::take(&mut *x.borrow_mut()));
        if tmp.len() < size + 64 {
            tmp.resize(size + 64, 0);
        }
        ends.clear();
        times.clear();
        let e = unpack_lines(p, &hd, &col_of, &first, &ids, &mut tmp, 0, Some((&mut ends, &mut times)), comp);
        super::pt(44, &mut t);
        let r = match e {
            Ok(e) if e == E_BAD => Err("damaged data (derived fields)"),
            Ok(e) if e <= size => group::ungroup_into(&tmp[..e], &ends, &times, &p.streams[9], out, o0).ok_or("damaged line order"),
            Ok(_) => Err("damaged line order"),
            Err(x) => Err(x),
        };
        SCRATCH.with(|x| *x.borrow_mut() = (tmp, ends, times));
        super::pt(45, &mut t);
        return r;
    }
    let e = unpack_lines(p, &hd, &col_of, &first, &ids, out, o0, None, comp)?;
    super::pt(44, &mut t);
    if e == usize::MAX {
        return Err("damaged ID columns");
    }
    if e == E_BAD {
        return Err("damaged data (derived fields)");
    }
    Ok(e)
}



/// How the line loop opens a value of a column (Cur::hd).
const H_ANY: u8 = 0; // any value (value_any): kinds stream, texts, words, the clock column
const H_NUM: u8 = 1; // numbers as they are, printed in decimal (kinds 0 and 3 + w)
const H_DNUM: u8 = 2; // ... written as differences
const H_DDNUM: u8 = 3; // ... written as second differences
const H_XNUM: u8 = 4; // numbers of another kind (clock times, IPv4, ...) as they are
const H_DXNUM: u8 = 5; // ... written as differences
const H_DDXNUM: u8 = 6; // ... written as second differences
const H_CNUM: u8 = 7; // cache-coded numbers in decimal (kinds 0 and 3 + w)
const H_CXNUM: u8 = 8; // cache-coded numbers of another kind
const H_TCOL: u8 = 9; // the clock column of a grouped block (one clock kind, not cache-coded)
const H_END: u8 = 255; // a pattern's last piece (its '\n'): no value
// B5: markers of derived parts in the fixed text (no value): bgl's gap (b, B: the offset)
// and fill (e: a space follows, E: none), ctime t
const H_CB: u8 = 248;
const H_CBB: u8 = 249;
const H_CE: u8 = 250;
const H_CEE: u8 = 251;
const H_CT: u8 = 252;

/// One piece of a pattern: fixed text tb[a..a + n], then the value of column `col` at pattern
/// slot `slot`; a pattern's last piece ends with its '\n' and has no value.
#[derive(Clone, Copy)]
struct Op {
    a: u32,
    n: u32,
    hd: u8, // the column's handler (H_*)
    way: u8, // 1: H_DNUM, 2: H_CNUM (checked before the others, apart from hd)
    col: u32,
    slot: u32,
}

/// Everything the line loop keeps per block.
struct St<'a> {
    cur: Vec<Cur>,
    kinds: &'a [u8],
    txt: &'a [u8],
    nums: Vec<u8>, // streams 4 and 7, one after the other
    words: Vec<&'a [u8]>,
    cx: Cached<'a>,
    rtext: Vec<[(u32, u32); 8]>, // the last 8 different texts of each column, newest first
    wrec: Vec<Recent8>,
    slot_last: Vec<u64>,
    kind_last: [u64; 256],
    tc: usize, // the clock column of a grouped block (usize::MAX: none)
    sub: FxMap<(u32, u64), u32>,
    sub_slot: FxMap<(u32, u32), u32>,
    ncols: u32,
    ud: ids::Udict<'a>,
    kp: Option<KOpen>,
    eslot: usize, // the pattern slot of the value value_any opens (keyed slots, entity.rs)
    // B5: where the line being written starts, a ctime marker's place in it (usize::MAX:
    // none), the bgl gap marker's place and offset, the bgl offsets and last epoch, and
    // whether derived parts were damaged
    ls: usize,
    pend: usize,
    gap: usize,
    k: usize,
    off: [i64; 2],
    bc: derive::BglCache,
    bad: bool,
}

/// Keyed slots (entity.rs) while opening: per pattern slot its key's distance (0: not
/// keyed) and whether it is a key, the memory, and the key values of the current line by
/// pattern slot (kind u8::MAX: none).
struct KOpen {
    dist: Vec<u16>,
    iskey: Vec<bool>,
    mem: entity::Mem,
    reg: Vec<(u8, u64)>,
}

/// A UUID (kind 100: lower case, 101: upper case) at out[o..]; returns the end.
#[inline(never)]
fn put_uuid(out: &mut [u8], o: usize, u: u128, kind: u8) -> usize {
    let tab = &HEX2[(kind & 1) as usize];
    let mut b = [b'-'; 36];
    let mut w = 0;
    for (q, &x) in u.to_be_bytes().iter().enumerate() {
        // (a dash before bytes 4, 6, 8 and 10)
        w += matches!(q, 4 | 6 | 8 | 10) as usize;
        b[w..w + 2].copy_from_slice(&tab[x as usize].to_le_bytes());
        w += 2;
    }
    out[o..o + 36].copy_from_slice(&b);
    o + 36
}

/// A clock time (kinds 40-67) at out[o..]; returns the end.
#[inline(always)]
fn put_time(out: &mut [u8], mut o: usize, kind: u8, v: u64) -> usize {
    let t = kind - 40;
    let (hw, comma, fd) = ((t & 1) + 1, (t >> 1) & 1, (t >> 2) as usize);
    let p10 = POW10[fd];
    let secs = v / p10;
    o = put_num(out, o, secs / 3600, hw as usize);
    out[o] = b':';
    o = put_num(out, o + 1, secs / 60 % 60, 2);
    out[o] = b':';
    o = put_num(out, o + 1, secs % 60, 2);
    if fd > 0 {
        out[o] = if comma == 1 { b',' } else { b'.' };
        o = put_num(out, o + 1, v % p10, fd);
    }
    o
}

/// A number of any kind but a word (kinds below 30 but 24 in decimal) at out[o..].
#[inline(always)]
fn put_numk(out: &mut [u8], o: usize, kind: u8, v: u64) -> usize {
    if kind < 30 && kind != LONG_ID {
        put_num(out, o, v, if kind >= 3 { (kind - 3) as usize } else { 0 })
    } else if (40..68).contains(&kind) {
        put_time(out, o, kind, v)
    } else {
        put_special(out, o, kind, v)
    }
}

/// The text of a clock time (kinds 40-67: H:MM:SS[.F], 110-123: H.MM.SS[.F]) below 100 hours
/// in a u128 (first byte lowest) and its length; length 0 for 100 hours and more.
#[inline(always)]
fn time_text(kind: u8, v: u64) -> (u128, usize) {
    let colon = kind < 110;
    let t = if colon { kind - 40 } else { kind - 110 };
    let (fd, fsep, sep) = if colon { ((t >> 2) as usize, if (t >> 1) & 1 == 1 { b',' } else { b'.' }, b':') } else { ((t >> 1) as usize, b'.', b'.') };
    let p10 = POW10[fd];
    let secs = v / p10;
    let h = secs / 3600;
    if h >= 100 {
        return (0, 0);
    }
    let (mm, ss) = ((secs / 60 % 60) as usize, (secs % 60) as usize);
    // the hours: 2 digits, or 1 when the width is 1 and h < 10
    let hl = if t & 1 == 0 && h < 10 { 1 } else { 2 };
    let ht = if hl == 1 { (b'0' + h as u8) as u128 } else { D2[h as usize] as u128 };
    let s = (sep as u128) << 8 * hl | (D2[mm] as u128) << 8 * (hl + 1) | (sep as u128) << 8 * (hl + 3) | (D2[ss] as u128) << 8 * (hl + 4);
    let n = hl + 6;
    if fd == 0 {
        return (ht | s, n);
    }
    let f = swar8(v % p10) >> (8 * (8 - fd));
    (ht | s | (fsep as u128) << 8 * n | (f as u128) << 8 * (n + 1), n + 1 + fd)
}

/// The text of an IPv4 address (kind 30) in a u128 and its length.
#[inline(always)]
fn ipv4_text(v: u64) -> (u128, usize) {
    let (a, b, c, d) = (OCT[(v >> 24) as usize & 255], OCT[(v >> 16) as usize & 255], OCT[(v >> 8) as usize & 255], OCT[v as usize & 255]);
    let (la, lb, lc, ld) = ((a >> 24) as usize, (b >> 24) as usize, (c >> 24) as usize, (d >> 24) as usize);
    let mut x = (a & 0xff_ffff) as u128 | (b'.' as u128) << 8 * la;
    let mut n = la + 1;
    x |= ((b & 0xff_ffff) as u128 | (b'.' as u128) << 8 * lb) << 8 * n;
    n += lb + 1;
    x |= ((c & 0xff_ffff) as u128 | (b'.' as u128) << 8 * lc) << 8 * n;
    n += lc + 1;
    x |= ((d & 0xff_ffff) as u128) << 8 * n;
    (x, n + ld)
}

/// The text of a number of any kind but a word (as put_numk writes it) in a u128 and its
/// length; length 0 when longer than 16 bytes.
#[inline(always)]
fn kind_text(kind: u8, v: u64) -> (u128, usize) {
    if kind == LONG_ID {
        return (0, 0); // mostly longer than 16 bytes: written straight away
    }
    if (40..68).contains(&kind) || (110..124).contains(&kind) {
        let (x, n) = time_text(kind, v);
        if n > 0 {
            return (x, n);
        }
    } else if kind == 30 && v < 1 << 32 {
        return ipv4_text(v);
    }
    kind_text_slow(kind, v)
}

#[inline(never)]
fn kind_text_slow(kind: u8, v: u64) -> (u128, usize) {
    if (SPN..=SPN + SPN_MAX).contains(&kind) && kind - SPN <= 16 {
        // a space-padded number of w <= 16 characters: spaces, then the digits (the text
        // put_special writes, with the same zeros after it)
        let (t, n) = num_text(v, 0);
        if n > 0 {
            let pad = ((kind - SPN) as usize).saturating_sub(n);
            let sp = u128::from_le_bytes([b' '; 16]) & ((1u128 << (8 * pad)) - 1);
            return (sp | t << (8 * pad), pad + n);
        }
    }
    let mut b = [0u8; 64];
    let n = put_numk(&mut b, 0, kind, v);
    if n <= 16 { (u128::from_le_bytes(b[..16].try_into().unwrap()), n) } else { (0, 0) }
}

/// A long ID (kind LONG_ID: magnitude * 2 + sign) at out[o..]; returns the end.
#[inline(never)]
fn put_long_id(out: &mut [u8], mut o: usize, v: u64) -> usize {
    if v & 1 == 1 {
        out[o] = b'-';
        o += 1;
    }
    let x = v >> 1;
    let n = digit_count(x);
    if n <= 16 {
        return put_num(out, o, x, 0);
    }
    // 17-19 digits: the first n - 16, then 8 + 8
    let hi = x / 10_000_000_000_000_000;
    let lo = x - hi * 10_000_000_000_000_000;
    let mid = lo / 100_000_000;
    out[o..o + 8].copy_from_slice(&(swar8(hi) >> (8 * (24 - n))).to_le_bytes());
    let e = o + n - 16;
    out[e..e + 8].copy_from_slice(&swar8(mid).to_le_bytes());
    out[e + 8..e + 16].copy_from_slice(&swar8(lo - mid * 100_000_000).to_le_bytes());
    o + n
}

/// put_numk out of the line loop (numbers longer than 16 bytes).
#[cold]
#[inline(never)]
fn put_numk_cold(out: &mut [u8], o: usize, kind: u8, v: u64) -> usize {
    put_numk(out, o, kind, v)
}

/// One value of column ci at slot sid, of any kind, at out[o..]; returns the end. Sets *ctx
/// after a word of a splitting column, and *lt at the first clock time of the clock column.
/// With X: also the value as a rule source (rules::src_val; NOT_NUM for texts and words).
#[inline(never)]
fn value_any<const X: bool>(st: &mut St, ci: usize, sid: usize, out: &mut [u8], mut o: usize, ctx: &mut u64, lt: &mut i64) -> (usize, u64) {
    let mut xv = rules::NOT_NUM;
    // keyed slots (entity.rs; st.eslot: the pattern slot): a key slot has no number of this
    // line until one is opened below
    let g = st.eslot;
    if let Some(kp) = &mut st.kp {
        if kp.iskey[g] {
            kp.reg[g].0 = u8::MAX;
        }
    }
    let cu = &mut st.cur[ci];
    let kind = if cu.same < 256 {
        cu.same as u8
    } else {
        cu.k += 1;
        st.kinds[cu.k - 1]
    };
    let txt = st.txt;
    match kind {
        1 => {
            let len = get_varint(txt, &mut cu.t) as usize;
            o = put_bytes(out, o, txt, cu.t, len);
            let r = &mut st.rtext[ci];
            r.copy_within(0..7, 1);
            r[0] = (cu.t as u32, (cu.t + len) as u32);
            cu.t += len;
        }
        2 | 71..=77 => {
            let k = if kind == 2 { 0 } else { (kind - 70) as usize };
            let r = &mut st.rtext[ci];
            let x = r[k];
            r.copy_within(0..k, 1);
            r[0] = x;
            o = put_bytes(out, o, txt, x.0 as usize, (x.1 - x.0) as usize);
        }
        80..=87 => {
            let w = st.wrec[ci].hit((kind - 80) as usize);
            let t = st.words[w as usize];
            out[o..o + t.len()].copy_from_slice(t);
            o += t.len();
            if cu.split {
                *ctx = w as u64 + 1;
            }
        }
        100..=103 if !cu.idc || kind > 101 => o = put_hex(out, o, txt, cu, kind),
        _ => {
            let kc = kclass(kind);
            let nums = &st.nums[..];
            // a keyed slot (entity.rs) whose key is a number in this line: its entity
            let mut ent = (0u64, 0usize, false, false);
            if let Some(kp) = &st.kp {
                let d = kp.dist[g] as usize;
                if d != 0 && kp.reg[g - d].0 != u8::MAX && entity::numeric(kind) {
                    let (kk, kv) = kp.reg[g - d];
                    let h = entity::key_hash(kk, kv, kind);
                    let (i, held) = kp.mem.find(h);
                    ent = (h, i, held, true);
                }
            }
            let mut rank = usize::MAX;
            let v = if cu.keyed {
                let (v, r) = keyed_value(&mut st.cx, cu, nums, kind, st.slot_last[sid], st.kind_last[kc], st.kp.as_ref().map(|k| &k.mem), ent);
                st.slot_last[sid] = v;
                rank = r;
                v
            } else if cu.cache == 0 {
                let x = varint(nums, &mut cu.n);
                let v = if cu.dod {
                    cu.pd = cu.pd.wrapping_add(unzz(x));
                    cu.prev.wrapping_add(cu.pd)
                } else if cu.delta {
                    cu.prev.wrapping_add(unzz(x))
                } else {
                    x as i64
                };
                cu.prev = v;
                v as u64
            } else {
                let v = cached_value(&mut st.cx, cu, nums, kind, st.slot_last[sid], st.kind_last[kc]);
                st.slot_last[sid] = v;
                v
            };
            if let Some(kp) = &mut st.kp {
                if ent.3 {
                    let r = if rank != usize::MAX { rank } else if ent.2 { kp.mem.rank(ent.1, v) } else { entity::R };
                    kp.mem.update(ent.1, ent.0, ent.2, v, r);
                }
                if kp.iskey[g] && entity::numeric(kind) {
                    kp.reg[g] = (kind, v);
                }
            }
            st.kind_last[kc] = v;
            cu.tl = 0;
            if X {
                xv = rules::src_val(kind, v);
            }
            if kind == WORD {
                let t = st.words[v as usize];
                out[o..o + t.len()].copy_from_slice(t);
                o += t.len();
                st.wrec[ci].push(v as u32);
                if cu.split {
                    *ctx = v + 1;
                }
            } else if kind == 100 || kind == 101 {
                // an ID column's UUID: its place in the dictionary (ids.rs)
                let u = st.ud.get(v);
                o = put_uuid(out, o, u, kind);
            } else {
                if (40..68).contains(&kind) && ci == st.tc && *lt == group::NO_TIME {
                    *lt = micros(kind, v);
                }
                o = put_numk(out, o, kind, v);
            }
        }
    }
    (o, xv)
}

/// The rest of a line after a word of a splitting column: the columns of its later slots
/// depend on the word (split_slot).
#[cold]
#[inline(never)]
fn rest_split(st: &mut St, ops: &[Op], tb: &[u8], out: &mut [u8], mut o: usize, mut ctx: u64, lt: &mut i64) -> usize {
    for op in ops {
        o = put_lit(out, o, tb, op.a as usize, op.n as usize);
        if op.hd >= H_CB {
            o = comp_op(st, op.hd, out, o);
            continue;
        }
        st.eslot = op.slot as usize;
        let (ci, sid) = if ctx == 0 { (op.col as usize, op.slot as usize) } else { split_slot(&mut st.sub, &mut st.sub_slot, &mut st.ncols, &mut st.slot_last, op.col, ctx, op.slot as usize) };
        o = value_any::<false>(st, ci, sid, out, o, &mut ctx, lt).0;
    }
    o
}

/// A line of a pattern with rules (`ops`: its pieces but the last; rules[ri..re]: the
/// pattern's rules): the rules' slots by their answers, the other values by their handlers
/// (each value is kept in xs as a rule source: after a fast handler the column's `prev` is the
/// value), or value_any.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn line_rules(st: &mut St, ops: &[Op], need: &[u64], tb: &[u8], out: &mut [u8], mut o: usize, lt: &mut i64, ids: &Ids, rat: &mut RuleCur, xs: &mut [u64], mut ri: usize, re: usize) -> usize {
    let mut ctx = 0u64;
    // writes without checks while the line's worst case (need, as in the line loop) fits
    let mut checked = o + need[0] as usize > out.len();
    // k: the slot (B5 markers are pieces without one), j: the piece
    let mut k = usize::MAX;
    for (j, op) in ops.iter().enumerate() {
        if op.hd >= H_CB {
            o = put_lit(out, o, tb, op.a as usize, op.n as usize);
            o = comp_op(st, op.hd, out, o);
            checked |= o + need[j + 1] as usize > out.len();
            continue;
        }
        k = k.wrapping_add(1);
        // (keyed slots, entity.rs: value_any's pattern slot)
        st.eslot = op.slot as usize;
        if ri < re && ids.rules[ri].slot as usize == k {
            ri += 1;
            if let Some(d) = rat.take(ids, ri - 1) {
                // the rule gives the value (columns after a splitting word are still made)
                o = put_lit(out, o, tb, op.a as usize, op.n as usize);
                if ctx != 0 {
                    split_slot(&mut st.sub, &mut st.sub_slot, &mut st.ncols, &mut st.slot_last, op.col, ctx, op.slot as usize);
                }
                let r = &ids.rules[ri - 1];
                let (pk, v) = rules::give(r, xs, d);
                xs[k] = v;
                st.kind_last[kclass(pk)] = v;
                o = put_num(out, o, v, if pk >= 5 { (pk - 3) as usize } else { 0 });
                checked |= o + need[j + 1] as usize > out.len();
                continue;
            }
        }
        if ctx == 0 && op.hd != H_ANY {
            let (o2, _, unb) = if checked { piece::<true>(st, *op, tb, out, o, lt) } else { piece::<false>(st, *op, tb, out, o, lt) };
            o = o2;
            if unb {
                // written by a checked way: maybe more than the piece's 16 bytes
                checked |= o + need[j + 1] as usize > out.len();
            }
            // SAFETY: every piece's column is below st.cur.len() (checked when made)
            let cu = unsafe { st.cur.get_unchecked(op.col as usize) };
            xs[k] = rules::src_val(cu.same as u8, cu.prev as u64);
            continue;
        }
        o = put_lit(out, o, tb, op.a as usize, op.n as usize);
        let (ci, sid) = if ctx == 0 { (op.col as usize, op.slot as usize) } else { split_slot(&mut st.sub, &mut st.sub_slot, &mut st.ncols, &mut st.slot_last, op.col, ctx, op.slot as usize) };
        let (o2, x) = value_any::<true>(st, ci, sid, out, o, &mut ctx, lt);
        o = o2;
        xs[k] = x;
        checked |= o + need[j + 1] as usize > out.len();
    }
    o
}

#[inline(never)]
fn unpack_lines(p: &Packed, heads: &heads::Heads, col_of: &[u32], first: &[u32], ids: &Ids, out: &mut [u8], o: usize, grp: Option<(&mut Vec<usize>, &mut Vec<i64>)>, comp: Option<&derive::Comp>) -> Result<usize, &'static str> {
    const E: &str = "damaged templates";
    let tid_of = &ids.tid[..];
    let s = &p.streams;
    let tc = if grp.is_some() { group::time_col(&s[9]).map_or(usize::MAX, |c| c as usize) } else { usize::MAX };
    // the columns, from the header (heads.rs)
    let nc = heads.cols.len();
    let mut cx = Cached { caches: Vec::new(), lists: Vec::new(), id_list: Vec::new(), codes: &s[6], use_ks: { let mut q = 0; get_varint(&s[2], &mut q); s[2].get(q + 1).map_or(true, |f| f & 8 == 0) } };
    // numbers: streams 4 and 7 in one buffer (stream 7's columns start after stream 4)
    let n4 = s[4].len();
    let mut cur: Vec<Cur> = Vec::with_capacity(nc);
    for (ci, e) in heads.cols.iter().enumerate() {
        let (mode, same) = (e.mode, e.same);
        let cache = if mode & 4 != 0 {
            cx.caches.push(Cache::new());
            cx.lists.push(Vec::new());
            cx.caches.len() as u32
        } else {
            0
        };
        let ns = e.ns;
        // the handler: one kind of number and not the clock column: a fast way
        let k = same as u8;
        let hd = if mode & 256 != 0 {
            H_ANY
        } else if ci == tc && same < 256 && (40..68).contains(&k) && cache == 0 {
            H_TCOL
        } else if same == 256 || ci == tc || matches!(k, 1 | 2 | 71..=77 | 80..=87 | 100..=103) || k == WORD {
            H_ANY
        } else if cache != 0 {
            if k < 30 && k != LONG_ID { H_CNUM } else { H_CXNUM }
        } else {
            let form = if mode & 32 != 0 { 2 } else if mode & 1 != 0 { 1 } else { 0 };
            form + if k < 30 && k != LONG_ID { H_NUM } else { H_XNUM }
        };
        cur.push(Cur {
            k: e.k,
            n: if ns == 7 { n4 + e.n } else { e.n },
            t: e.t,
            h: e.h,
            prev: 0,
            pd: 0,
            cache,
            same,
            ns,
            delta: mode & 1 != 0,
            dod: mode & 32 != 0,
            dict: mode & 8 != 0,
            split: mode & 64 != 0,
            idc: mode & 128 != 0,
            gs: 0,
            text: 0,
            tl: 0,
            hd,
            w: if (3..30).contains(&k) { k - 3 } else { 0 },
            kc: kclass(k) as u8,
            keyed: mode & 256 != 0,
            steps: mode & 512 != 0,
        });
    }
    // the templates: up to the last '\n' (slot_columns, which made col_of and first, skips
    // what follows it); pieces are kept as 32-bit places and lengths
    let tend = s[0].iter().rposition(|&c| c == b'\n').map_or(0, |q| q + 1);
    if tend >= u32::MAX as usize - 32 {
        return Err(E);
    }
    let mut nums = Vec::with_capacity(s[4].len() + s[7].len() + 16);
    nums.extend_from_slice(&s[4]);
    nums.extend_from_slice(&s[7]);
    // each pattern as pieces (Op): fixed text, then a value; the template text gets 16 bytes
    // of room so that pieces are always copied 16 bytes at a time
    let mut tbp = Vec::with_capacity(s[0].len() + 16);
    tbp.extend_from_slice(&s[0]);
    tbp.extend_from_slice(&[0u8; 16]);
    let tb = &tbp[..];
    // keyed slots (entity.rs): key and keyed slots open through value_any (H_ANY), which keeps
    // the line's key values and runs the memory
    let kp = heads.plan.as_ref().map(|pl| KOpen { iskey: pl.keys(), mem: entity::Mem::new(pl.bits), reg: vec![(u8::MAX, 0); pl.dist.len()], dist: pl.dist.clone() });
    let mut ops: Vec<Op> = Vec::with_capacity(col_of.len() + first.len());
    let mut pfirst: Vec<u32> = Vec::with_capacity(first.len());
    pfirst.push(0);
    let (mut a, mut slot) = (0usize, 0usize);
    // B5, per pattern: a bgl gap marker waiting for its fill marker, a ctime marker seen
    let (mut gap_open, mut ct_seen) = (false, false);
    for (pos, &c) in s[0][..tend].iter().enumerate() {
        // (MARK, MK and '\n' are below 11; a B5 marker's letter, always above, is passed
        // over here)
        if c > b'\n' {
            continue;
        }
        if c == derive::MK {
            if let Some(cp) = comp {
                // B5: a marker of derived parts ends a piece of fixed text, like a value
                // (each pattern: gap and fill markers in pairs, at most one ctime marker)
                let hd = match s[0].get(pos + 1) {
                    Some(b'b') if cp.bgl && !gap_open => H_CB,
                    Some(b'B') if cp.bgl && !gap_open => H_CBB,
                    Some(b'e') if cp.bgl && gap_open => H_CE,
                    Some(b'E') if cp.bgl && gap_open => H_CEE,
                    Some(b't') if cp.ctime && !ct_seen => H_CT,
                    _ => return Err("damaged data (derived fields)"),
                };
                gap_open = hd == H_CB || hd == H_CBB;
                ct_seen |= hd == H_CT;
                ops.push(Op { a: a as u32, n: (pos - a) as u32, hd, way: 0, col: 0, slot: 0 });
                a = pos + 2;
                continue;
            }
        }
        if c == MARK || c == b'\n' {
            let n = pos - a + (c == b'\n') as usize;

            if c == MARK {
                // (a slot outside col_of or a column outside the header: refused below)
                let col = col_of.get(slot).copied().unwrap_or(u32::MAX);
                let keyed = kp.as_ref().is_some_and(|k| k.dist.get(slot).is_some_and(|&d| d != 0) || k.iskey.get(slot) == Some(&true));
                let hd = if keyed { H_ANY } else { cur.get(col as usize).map_or(H_ANY, |c| c.hd) };
                let way = if hd == H_DNUM { 1 } else if hd == H_CNUM { 2 } else { 0 };
                ops.push(Op { a: a as u32, n: n as u32, hd, way, col, slot: slot as u32 });
                slot += 1;
            } else {
                if gap_open {
                    return Err("damaged data (derived fields)");
                }
                ct_seen = false;
                ops.push(Op { a: a as u32, n: n as u32, hd: H_END, way: 0, col: 0, slot: 0 });
                pfirst.push(ops.len() as u32);
            }
            a = pos + 1;
        }
    }
    let Some(ud) = ids::Udict::new(&s[5][heads.tail..]) else { return Ok(usize::MAX) };
    let mut st = St {
        ud,
        cur,
        kinds: &s[3],
        txt: &s[5],
        nums,
        words: table_entries(&s[8]),
        cx,
        rtext: vec![[(0, 0); 8]; nc],
        wrec: vec![Recent8::default(); nc],
        slot_last: vec![0u64; col_of.len()],
        kind_last: [0u64; 256],
        tc,
        sub: FxMap::default(),
        sub_slot: FxMap::default(),
        ncols: col_of.iter().copied().max().map_or(0, |x| x + 1),
        kp,
        eslot: 0,
        ls: 0,
        pend: usize::MAX,
        gap: 0,
        k: 0,
        off: comp.map_or([0; 2], |c| c.off),
        bc: derive::BglCache::default(),
        bad: false,
    };
    // checked once here, so that the line loop reads ops, columns, slots and the template
    // text without bounds checks: every value piece's column and slot exist, every piece's
    // text (+ 16 bytes) is inside tbp, and every pattern ends with a piece; and the pattern
    // ids: one entry of pfirst per pattern of `first` (line_tids refuses an id past them) and
    // of rfirst. (heads::read makes a column for each one col_of names, and col_of has a
    // slot for each MARK before the last '\n': a valid block always passes.)
    let (ncur, nslot) = (st.cur.len(), st.slot_last.len());
    if ops.iter().any(|op| (op.hd != H_END && (op.col as usize >= ncur || op.slot as usize >= nslot)) || op.a as usize + op.n as usize + 16 > tb.len())
        || pfirst.windows(2).any(|w| w[0] >= w[1])
        || *pfirst.last().unwrap() as usize != ops.len()
        || pfirst.len() != first.len()
        || ids.rfirst.len() != pfirst.len()
        || tid_of.iter().any(|&t| t as usize + 1 >= pfirst.len())
    {
        return Err(E);
    }
    // the most each piece can write (its text, 16 bytes of value) up to its line's end, plus
    // 16: a line whose worst case fits in `out` is written without bounds checks. (u64: a
    // pattern's pieces are parts of s[0][..tend], so a sum is below tend + 32 * ops.len() + 32,
    // under 2^38: it neither overflows nor overflows o + need)
    let mut need = vec![0u64; ops.len()];
    for k in (0..ops.len()).rev() {
        // (a B5 marker puts in at most 23 + 64 bytes, a ctime marker 20)
        let w = ops[k].n as u64 + if ops[k].hd >= H_CB && ops[k].hd != H_END { 128 } else { 16 };
        need[k] = if ops[k].hd == H_END { w + 16 } else { w + need[k + 1] };
    }
    // rules: patterns with rules open their lines in line_rules (the line's values so far as
    // rule sources, the next answer)
    let has_rules = !ids.rules.is_empty();
    let mut xs = vec![rules::NOT_NUM; if has_rules { first.windows(2).map(|w| (w[1] - w[0]) as usize).max().unwrap_or(0) } else { 0 }];
    let mut rat = RuleCur::new(ids);
    Ok(fast_lines(&mut st, tid_of, &pfirst, &ops, &need, tb, out, o, grp, ids, &mut rat, &mut xs))
}

/// The line loop of unpack_lines, in a function of its own: without the block's set-up code
/// around it, the loop keeps its state (out, the place in it, the template text, the pieces)
/// in registers instead of reloading it from the stack for every piece (3.6% fewer
/// instructions for opening). The values of H_DNUM columns (55% of all values; about 3 in 4
/// repeat the column's last one) are opened here before the other handlers are looked at;
/// lines with rules, lines that may not fit in `out` and what is left of a line after a long
/// value go to functions out of the loop. The same bytes as piece.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn fast_lines(st: &mut St, tid_of: &[u32], pfirst: &[u32], ops: &[Op], need: &[u64], tb: &[u8], out: &mut [u8], mut o: usize, mut grp: Option<(&mut Vec<usize>, &mut Vec<i64>)>, ids: &Ids, rat: &mut RuleCur, xs: &mut [u64]) -> usize {
    let has_rules = !ids.rules.is_empty();
    debug_assert!(need.len() == ops.len() && *pfirst.last().unwrap() as usize == ops.len());
    debug_assert!(ids.rfirst.len() == pfirst.len() && tid_of.iter().all(|&t| (t as usize) + 1 < pfirst.len()));
    for &tid in tid_of {
        let t = tid as usize;
        // SAFETY: t + 1 < pfirst.len() = ids.rfirst.len() for every id (checked in unpack_lines)
        let (b, e) = unsafe { (*pfirst.get_unchecked(t) as usize, *pfirst.get_unchecked(t + 1) as usize - 1) };
        let mut lt = group::NO_TIME;
        let mut k = b;
        st.ls = o;
        if has_rules && unsafe { ids.rfirst.get_unchecked(t) != ids.rfirst.get_unchecked(t + 1) } {
            let (ri, re) = (ids.rfirst[t] as usize, ids.rfirst[t + 1] as usize);
            o = line_rules(st, &ops[b..e], &need[b..=e], tb, out, o, &mut lt, ids, rat, xs, ri, re);
            k = e;
        }
        // SAFETY (all get_unchecked below): b <= k <= e < ops.len() = need.len(): pfirst
        // rises and ends at ops.len() (checked in unpack_lines)
        if o + unsafe { *need.get_unchecked(b) } as usize <= out.len() {
            while k < e {
                let op = unsafe { *ops.get_unchecked(k) };
                k += 1;
                o = put_lit16::<false>(out, o, tb, op.a as usize, op.n as usize);
                if op.way == 1 {
                    // H_DNUM, as value::<false> opens it: a difference of 0 repeats the
                    // column's text
                    // SAFETY: every piece's column is below st.cur.len() (checked when made)
                    let cu = unsafe { st.cur.get_unchecked_mut(op.col as usize) };
                    let nums = &st.nums[..];
                    if cu.tl == 0 || nums[cu.n] != 0 {
                        let x = varint(nums, &mut cu.n);
                        let v = cu.prev.wrapping_add(unzz(x)) as u64;
                        cu.prev = v as i64;
                        st.kind_last[cu.kc as usize] = v;
                        let (t, l) = num_text(v, cu.w as usize);
                        cu.text = t;
                        cu.tl = l as u8;
                        if l == 0 {
                            // more than 16 digits: written by a checked way, maybe more than
                            // the piece's 16 bytes (as emit! in value)
                            let kd = cu.same as u8;
                            o = if kd == LONG_ID { put_long_id(out, o, v) } else { put_numk_cold(out, o, kd, v) };
                            if o + unsafe { *need.get_unchecked(k) } as usize > out.len() {
                                break;
                            }
                            continue;
                        }
                    } else {
                        cu.n += 1;
                        st.kind_last[cu.kc as usize] = cu.prev as u64;
                    }
                    w16::<false>(out, o, cu.text.to_le_bytes());
                    o += cu.tl as usize;
                    continue;
                }
                let (o2, ctx, unb) = value::<false, true>(st, op, out, o, &mut lt);
                o = o2;
                if unb {
                    // written by a checked way: maybe more than the piece's 16 bytes
                    if ctx != 0 {
                        o = rest_split(st, &ops[k..e], tb, out, o, ctx, &mut lt);
                        k = e;
                    }
                    if o + unsafe { *need.get_unchecked(k) } as usize > out.len() {
                        break;
                    }
                }
            }
        }
        if k < e {
            // with bounds checks: a line that may not fit (the buffer's end), or what is left
            // of a line after a long value
            o = pieces_checked(st, &ops[k..e], tb, out, o, &mut lt);
        }
        let op = unsafe { *ops.get_unchecked(e) };
        o = if o + op.n as usize + 32 <= out.len() { put_lit16::<false>(out, o, tb, op.a as usize, op.n as usize) } else { put_lit16::<true>(out, o, tb, op.a as usize, op.n as usize) };
        if st.pend != usize::MAX {
            // B5: a ctime marker: its parts go in front of the year once the line is written
            match derive::ctime_put(out, st.ls, st.pend, o) {
                Some(e) => o = e,
                None => st.bad = true,
            }
            st.pend = usize::MAX;
        }
        if let Some((ends, times)) = &mut grp {
            ends.push(o);
            times.push(lt);
        }
    }
    if st.bad {
        return E_BAD;
    }
    if !st.ud.ok() {
        return usize::MAX; // (damaged UUIDs of ID columns)
    }
    o
}

/// The pieces `ops` of a line with bounds checks (see fast_lines).
#[inline(never)]
fn pieces_checked(st: &mut St, ops: &[Op], tb: &[u8], out: &mut [u8], mut o: usize, lt: &mut i64) -> usize {
    let mut k = 0;
    while k < ops.len() {
        let op = ops[k];
        k += 1;
        let (o2, ctx, unb) = piece::<true>(st, op, tb, out, o, lt);
        o = o2;
        if unb && ctx != 0 {
            o = rest_split(st, &ops[k..], tb, out, o, ctx, lt);
            k = ops.len();
        }
    }
    o
}

/// B5 (packing): whether the line printer can put a block's derived parts in (R_INL): all
/// `marks` markers of the rewritten lines are in the patterns' fixed text (none went into a
/// value: a text, a word, a token), each with its letter right after it in the fixed text (a
/// letter cut into a value, as the splitter does with "b" + a node of hex digits, would make
/// unpack_lines refuse the block), gap and fill markers in pairs and at most one ctime marker
/// per pattern (unpack_lines' own rules), and the fast line loop takes the block (the pieces
/// of fixed text, cut at markers too, below 64 KiB; a slot and a column).
#[inline(never)]
pub fn inline_ok(p: &Packed, marks: usize) -> bool {
    let s = &p.streams;
    if p.mk != marks as u64 || s[0].len() >= u32::MAX as usize {
        return false;
    }
    let t = &s[0];
    let (mut run, mut gap, mut ct, mut slots, mut i) = (0usize, false, false, 0usize, 0usize);
    while i < t.len() {
        let c = t[i];
        i += 1;
        if c == derive::MK {
            match t.get(i) {
                Some(b'b' | b'B') if !gap => gap = true,
                Some(b'e' | b'E') if gap => gap = false,
                // (stricter than unpack_lines, which takes a t inside a gap / fill pair and
                // then refuses the fill, or leaves the gap unfilled: never written here)
                Some(b't') if !ct && !gap => ct = true,
                _ => return false,
            }
            (i, run) = (i + 1, 0);
        } else if c == MARK || c == b'\n' {
            if c == b'\n' {
                if gap {
                    return false;
                }
                ct = false;
            }
            slots += (c == MARK) as usize;
            run = 0;
        } else {
            run += 1;
            if run > 60_000 {
                return false;
            }
        }
    }
    // (the column count, the header's first varint, written canonically: 0 only as a 0 byte)
    slots > 0 && s[2].first().is_some_and(|&c| c != 0)
}

/// B5: a marker of derived parts in a line being written (out[st.ls..o]): a bgl gap marker
/// leaves a hole, its fill marker (later in the same pattern) fills it and puts the second
/// node in; a ctime
/// marker's parts go in when the line is written (unpack_lines). Damaged parts set st.bad
/// (the block is refused) and write nothing.
#[inline(never)]
fn comp_op(st: &mut St, hd: u8, out: &mut [u8], o: usize) -> usize {
    match hd {
        H_CB | H_CBB => {
            // a hole for EPOCH ' ' DATE ' ', filled by the fill marker
            (st.gap, st.k) = (o, (hd == H_CBB) as usize);
            o + derive::BGL_HOLE
        }
        H_CT => {
            st.pend = o;
            o
        }
        _ => match derive::bgl_fill(out, st.gap, derive::BGL_HOLE, o, st.k, hd == H_CE, &st.off, &mut st.bc) {
            Some(e) => e,
            None => {
                st.bad = true;
                o
            }
        },
    }
}

/// 16 bytes at out[o..]: with a bounds check (C), or without one (the caller's line budget
/// holds them).
#[inline(always)]
fn w16<const C: bool>(out: &mut [u8], o: usize, x: [u8; 16]) {
    if C {
        out[o..o + 16].copy_from_slice(&x);
    } else {
        debug_assert!(o + 16 <= out.len());
        // SAFETY: the line's budget (need) keeps o + 16 <= out.len()
        unsafe { (out.as_mut_ptr().add(o) as *mut [u8; 16]).write_unaligned(x) }
    }
}

/// Fixed text tb[a..a + n] at out[o..], 16 bytes at a time (tb has room after each piece:
/// checked once per block).
#[inline(always)]
fn put_lit16<const C: bool>(out: &mut [u8], o: usize, tb: &[u8], a: usize, n: usize) -> usize {
    debug_assert!(a + n + 16 <= tb.len());
    // SAFETY: a + n + 16 <= tb.len() for every piece (checked when the pieces were made)
    let rd = |p: usize| unsafe { (tb.as_ptr().add(p) as *const [u8; 16]).read_unaligned() };
    w16::<C>(out, o, rd(a));
    let mut p = 16;
    while p < n {
        w16::<C>(out, o + p, rd(a + p));
        p += 16;
    }
    o + n
}

/// One piece of a line: its fixed text, then its value. Returns the end, the word choosing
/// the columns of the line's later slots (0: none), and whether the value was written by a
/// checked way that may go past the piece's 16 bytes.
#[inline(always)]
fn piece<const C: bool>(st: &mut St, op: Op, tb: &[u8], out: &mut [u8], o: usize, lt: &mut i64) -> (usize, u64, bool) {
    let o = put_lit16::<C>(out, o, tb, op.a as usize, op.n as usize);
    value::<C, false>(st, op, out, o, lt)
}

/// The value of a piece whose fixed text is written (see piece). H: cache codes by
/// cached_value_hot (only fast_lines' own loop: the hot path is not copied elsewhere).
#[inline(always)]
fn value<const C: bool, const H: bool>(st: &mut St, op: Op, out: &mut [u8], mut o: usize, lt: &mut i64) -> (usize, u64, bool) {
    let ci = op.col as usize;
    // a number's text: kept in its column (repeats copy it), or written straight away when
    // longer than 16 bytes
    macro_rules! emit {
        ($cu:expr, $v:expr, $plain:expr) => {{
            let (t, l) = if $plain { num_text($v, $cu.w as usize) } else { kind_text($cu.same as u8, $v) };
            $cu.text = t;
            $cu.tl = l as u8;
            if l == 0 {
                let k = $cu.same as u8;
                return (if k == LONG_ID { put_long_id(out, o, $v) } else { put_numk_cold(out, o, k, $v) }, 0, true);
            }
        }};
    }
    macro_rules! delta {
        ($plain:expr) => {{
            // SAFETY: every piece's column is below st.cur.len() (checked when made)
            let cu = unsafe { st.cur.get_unchecked_mut(ci) };
            let nums = &st.nums[..];
            if cu.tl == 0 || nums[cu.n] != 0 {
                let x = varint(nums, &mut cu.n);
                let v = cu.prev.wrapping_add(unzz(x)) as u64;
                cu.prev = v as i64;
                st.kind_last[cu.kc as usize] = v;
                emit!(cu, v, $plain);
            } else {
                cu.n += 1;
                st.kind_last[cu.kc as usize] = cu.prev as u64;
            }
            w16::<C>(out, o, cu.text.to_le_bytes());
            o += cu.tl as usize;
        }};
    }
    macro_rules! raw {
        ($plain:expr) => {{
            // SAFETY: as above
            let cu = unsafe { st.cur.get_unchecked_mut(ci) };
            let v = varint(&st.nums, &mut cu.n);
            st.kind_last[cu.kc as usize] = v;
            if cu.tl == 0 || v != cu.prev as u64 {
                cu.prev = v as i64;
                emit!(cu, v, $plain);
            }
            w16::<C>(out, o, cu.text.to_le_bytes());
            o += cu.tl as usize;
        }};
    }
    macro_rules! dod {
        ($plain:expr) => {{
            // SAFETY: as above
            let cu = unsafe { st.cur.get_unchecked_mut(ci) };
            let x = varint(&st.nums, &mut cu.n);
            cu.pd = cu.pd.wrapping_add(unzz(x));
            let v = cu.prev.wrapping_add(cu.pd) as u64;
            st.kind_last[cu.kc as usize] = v;
            if cu.tl == 0 || cu.pd != 0 {
                cu.prev = v as i64;
                emit!(cu, v, $plain);
            }
            w16::<C>(out, o, cu.text.to_le_bytes());
            o += cu.tl as usize;
        }};
    }
    macro_rules! cached {
        ($plain:expr) => {{
            // SAFETY: as above
            let cu = unsafe { st.cur.get_unchecked_mut(ci) };
            let (kind, kc, sid) = (cu.same as u8, cu.kc as usize, op.slot as usize);
            // SAFETY: every piece's slot is below st.slot_last.len(), which only grows
            let sl = unsafe { st.slot_last.get_unchecked_mut(sid) };
            let v = if H { cached_value_hot(&mut st.cx, cu, &st.nums, kind, *sl, st.kind_last[kc]) } else { cached_value(&mut st.cx, cu, &st.nums, kind, *sl, st.kind_last[kc]) };
            *sl = v;
            st.kind_last[kc] = v;
            if cu.tl == 0 || v != cu.prev as u64 {
                cu.prev = v as i64;
                emit!(cu, v, $plain);
            }
            w16::<C>(out, o, cu.text.to_le_bytes());
            o += cu.tl as usize;
        }};
    }
    if op.way == 1 {
        delta!(true);
        return (o, 0, false);
    }
    if op.way == 2 {
        cached!(true);
        return (o, 0, false);
    }
    match op.hd {
        H_DXNUM => delta!(false),
        H_CXNUM => cached!(false),
        H_NUM => raw!(true),
        H_XNUM => raw!(false),
        H_DDNUM => dod!(true),
        H_DDXNUM => dod!(false),
        H_TCOL => {
            // SAFETY: as above
            let cu = unsafe { st.cur.get_unchecked_mut(ci) };
            let x = varint(&st.nums, &mut cu.n);
            let v = if cu.dod {
                cu.pd = cu.pd.wrapping_add(unzz(x));
                cu.prev.wrapping_add(cu.pd)
            } else if cu.delta {
                cu.prev.wrapping_add(unzz(x))
            } else {
                x as i64
            } as u64;
            let kind = cu.same as u8;
            st.kind_last[cu.kc as usize] = v;
            if *lt == group::NO_TIME {
                *lt = micros(kind, v);
            }
            // prev is the last value in all three ways (and the text's value when tl > 0)
            let same_text = cu.tl != 0 && v == cu.prev as u64;
            cu.prev = v as i64;
            if !same_text {
                let (t, l) = kind_text(kind, v);
                cu.text = t;
                cu.tl = l as u8;
                if l == 0 {
                    return (put_numk_cold(out, o, kind, v), 0, true);
                }
            }
            w16::<C>(out, o, cu.text.to_le_bytes());
            o += cu.tl as usize;
        }
        _ => {
            if op.hd >= H_CB {
                return (comp_op(st, op.hd, out, o), 0, true);
            }
            let mut ctx = 0u64;
            st.eslot = op.slot as usize;
            let o = value_any::<false>(st, ci, op.slot as usize, out, o, &mut ctx, lt).0;
            return (o, ctx, true);
        }
    }
    (o, 0, false)
}
