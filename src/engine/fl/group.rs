// Group lines by source: inside a block, the lines of one source (a process, thread or
// component: one space-separated field) are stored together, so their columns are smoother.
// The original order is rebuilt by merging the groups on their clock times. The rule:
// next line = the group whose next line has the earliest time; on a tie, the group of the
// line before; then the lowest group number. Only lines where the rule is wrong cost bits:
// their rank among the waiting groups (0 = the rule was right).
//
// Order stream: varint(time column + 1, 0 = none), varint(groups), varint(size) per group,
// then one varint rank per line in the original order.

use super::{get_varint, put_varint, FxMap};

pub const MAX_GROUPS: usize = 4096;
/// A line without a clock time.
pub const NO_TIME: i64 = i64::MIN;

/// Field f of a line: the f-th run of bytes without spaces (empty when the line is shorter).
pub fn field(l: &[u8], f: usize) -> &[u8] {
    let n = l.len();
    let (mut i, mut k) = (0, 0);
    loop {
        while i < n && l[i] == b' ' {
            i += 1;
        }
        if i >= n {
            return &[];
        }
        let s = i;
        while i < n && l[i] != b' ' {
            i += 1;
        }
        if k == f {
            return &l[s..i];
        }
        k += 1;
    }
}

/// The group of each line (numbered by first appearance) and the size of each group, or
/// None when field f has fewer than 2 or more than MAX_GROUPS different values.
pub fn groups_of(lines: &[&[u8]], f: usize) -> Option<(Vec<u32>, Vec<u32>)> {
    let mut ids: FxMap<&[u8], u32> = FxMap::default();
    let mut gid = Vec::with_capacity(lines.len());
    let mut sizes: Vec<u32> = Vec::new();
    let mut prev: (&[u8], u32) = (&[], u32::MAX);
    for &l in lines {
        let k = field(l, f);
        // most lines come from the same source as the line before: no lookup then
        let g = if prev.1 != u32::MAX && k == prev.0 {
            prev.1
        } else {
            let n = ids.len() as u32;
            let g = *ids.entry(k).or_insert(n);
            if g == n {
                if sizes.len() >= MAX_GROUPS {
                    return None;
                }
                sizes.push(0);
            }
            g
        };
        sizes[g as usize] += 1;
        gid.push(g);
        prev = (k, g);
    }
    if sizes.len() < 2 {
        return None;
    }
    Some((gid, sizes))
}

/// The lines in group order (stable): perm[j] = the original number of the j-th line.
pub fn grouped_order(gid: &[u32], sizes: &[u32]) -> Vec<u32> {
    let mut at = starts(sizes);
    let mut perm = vec![0u32; gid.len()];
    for (i, &g) in gid.iter().enumerate() {
        perm[at[g as usize]] = i as u32;
        at[g as usize] += 1;
    }
    perm
}

/// Where each group starts in group order.
fn starts(sizes: &[u32]) -> Vec<usize> {
    let mut s = Vec::with_capacity(sizes.len());
    let mut a = 0usize;
    for &n in sizes {
        s.push(a);
        a += n as usize;
    }
    s
}

/// Times used by the merge (group order). A line without a time takes the time of the line
/// before it in its group (0 for a group's first line). A time more than 12 hours before the
/// block's first time is taken as the next day (the block crossed midnight); the block's
/// first line is the first line of group 0, so opening sees the same first time.
fn merge_times(times: &[i64], sizes: &[u32]) -> Vec<i64> {
    const DAY: i64 = 86_400_000_000;
    let n0 = sizes.first().map_or(0, |&n| n as usize);
    let t0 = times[..n0].iter().copied().find(|&t| t != NO_TIME);
    let mut out = Vec::with_capacity(times.len());
    let mut i = 0;
    for &n in sizes {
        let mut last = 0i64;
        for _ in 0..n {
            let t = times[i];
            if t != NO_TIME {
                last = match t0 {
                    Some(t0) if t < t0 - DAY / 2 => t + DAY,
                    _ => t,
                };
            }
            out.push(last);
            i += 1;
        }
    }
    out
}

/// The groups that still have lines, as a binary min-heap on (time of the next line, group
/// number); pos says where each group sits in the heap (NONE once it has no lines left).
struct Heads {
    heap: Vec<u32>,
    pos: Vec<u32>,
    key: Vec<i64>,
    aux: std::collections::BinaryHeap<std::cmp::Reverse<(i64, u32, u32)>>,
}

const NONE: u32 = u32::MAX;

impl Heads {
    fn new(key: Vec<i64>) -> Heads {
        let g = key.len();
        let mut h = Heads { heap: (0..g as u32).collect(), pos: (0..g as u32).collect(), key, aux: Default::default() };
        for i in (0..g / 2).rev() {
            h.down(i);
        }
        h
    }
    #[inline]
    fn lt(&self, a: u32, b: u32) -> bool {
        let (ka, kb) = (self.key[a as usize], self.key[b as usize]);
        ka < kb || (ka == kb && a < b)
    }
    #[inline]
    fn swap(&mut self, i: usize, j: usize) {
        self.heap.swap(i, j);
        self.pos[self.heap[i] as usize] = i as u32;
        self.pos[self.heap[j] as usize] = j as u32;
    }
    fn up(&mut self, mut i: usize) {
        while i > 0 {
            let p = (i - 1) / 2;
            if !self.lt(self.heap[i], self.heap[p]) {
                break;
            }
            self.swap(i, p);
            i = p;
        }
    }
    fn down(&mut self, mut i: usize) {
        let n = self.heap.len();
        loop {
            let l = 2 * i + 1;
            if l >= n {
                break;
            }
            let c = if l + 1 < n && self.lt(self.heap[l + 1], self.heap[l]) { l + 1 } else { l };
            if !self.lt(self.heap[c], self.heap[i]) {
                break;
            }
            self.swap(i, c);
            i = c;
        }
    }
    /// Group g moved to its next line, with time t (None: g has no lines left).
    fn advance(&mut self, g: usize, t: Option<i64>) {
        let i = self.pos[g] as usize;
        match t {
            Some(t) => {
                let old = self.key[g];
                self.key[g] = t;
                if t < old {
                    self.up(i)
                } else {
                    self.down(i)
                }
            }
            None => {
                let last = self.heap.len() - 1;
                if i != last {
                    self.swap(i, last);
                }
                self.heap.pop();
                self.pos[g] = NONE;
                if i < self.heap.len() {
                    self.down(i);
                    self.up(i);
                }
            }
        }
    }
    /// The rule's pick: the earliest next line; on a tie, the group of the line before.
    #[inline]
    fn pick(&self, prev: usize) -> usize {
        let top = self.heap[0] as usize;
        if prev < self.pos.len() && prev != top && self.pos[prev] != NONE && self.key[prev] == self.key[top] {
            prev
        } else {
            top
        }
    }
    /// Visit the waiting groups in the rule's order (time, then the line before's group first
    /// among its time, then number) until f says stop. A best-first walk down the heap: a
    /// group at place r costs about r steps (most wrong guesses are place 1 or 2).
    fn walk(&mut self, prev: usize, mut f: impl FnMut(usize) -> bool) {
        use std::cmp::Reverse;
        let pw = prev < self.pos.len() && self.pos[prev] != NONE;
        let kp = if pw { self.key[prev] } else { 0 };
        let mut prev_done = !pw;
        self.aux.clear();
        if !self.heap.is_empty() {
            let g = self.heap[0];
            self.aux.push(Reverse((self.key[g as usize], g, 0)));
        }
        while let Some(Reverse((k, g, i))) = self.aux.pop() {
            if !prev_done && k >= kp {
                prev_done = true;
                if f(prev) {
                    return;
                }
            }
            for c in [2 * i as usize + 1, 2 * i as usize + 2] {
                if c < self.heap.len() {
                    let x = self.heap[c];
                    self.aux.push(Reverse((self.key[x as usize], x, c as u32)));
                }
            }
            if !(pw && g as usize == prev) && f(g as usize) {
                return;
            }
        }
    }
    /// The place of group a in the rule's order (0 = the rule's pick).
    fn rank(&mut self, a: usize, prev: usize) -> usize {
        let mut r = 0;
        self.walk(prev, |g| {
            if g == a {
                return true;
            }
            r += 1;
            false
        });
        r
    }
    /// The group at place r of the rule's order.
    fn nth(&mut self, r: usize, prev: usize) -> Option<usize> {
        let (mut k, mut pick) = (0, None);
        self.walk(prev, |g| {
            if k == r {
                pick = Some(g);
                return true;
            }
            k += 1;
            false
        });
        pick
    }
}

/// The order stream of a grouped block. gid: the group of each line (original order);
/// times: the clock time of each line in group order (NO_TIME when it has none).
pub fn order_stream(gid: &[u32], sizes: &[u32], tcol: Option<u32>, times: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(gid.len() + 2 * sizes.len() + 8);
    put_varint(&mut out, tcol.map_or(0, |c| c as u64 + 1));
    put_varint(&mut out, sizes.len() as u64);
    for &n in sizes {
        put_varint(&mut out, n as u64);
    }
    let mt = merge_times(times, sizes);
    let mut head = starts(sizes);
    let end: Vec<usize> = head.iter().zip(sizes).map(|(&s, &n)| s + n as usize).collect();
    let mut h = Heads::new(head.iter().map(|&s| mt[s]).collect());
    let mut prev = usize::MAX;
    for &a in gid {
        let a = a as usize;
        let r = if h.pick(prev) == a { 0 } else { h.rank(a, prev) };
        put_varint(&mut out, r as u64);
        head[a] += 1;
        h.advance(a, if head[a] < end[a] { Some(mt[head[a]]) } else { None });
        prev = a;
    }
    out
}

/// The time column named by an order stream (None: not grouped or no time column).
pub fn time_col(order: &[u8]) -> Option<u32> {
    if order.is_empty() {
        return None;
    }
    let mut p = 0;
    let c = get_varint(order, &mut p);
    if c == 0 { None } else { Some((c - 1) as u32) }
}

/// Opening: the lines in group order (text, with the end of each line and its clock time)
/// back in their original order.
pub fn ungroup(text: &[u8], ends: &[usize], times: &[i64], order: &[u8]) -> Option<Vec<u8>> {
    let mut p = 0;
    get_varint(order, &mut p);
    let g = get_varint(order, &mut p) as usize;
    if g > MAX_GROUPS {
        return None;
    }
    let sizes: Vec<u32> = (0..g).map(|_| get_varint(order, &mut p) as u32).collect();
    if sizes.iter().map(|&n| n as usize).sum::<usize>() != ends.len() || sizes.contains(&0) {
        return None;
    }
    let mt = merge_times(times, &sizes);
    let mut head = starts(&sizes);
    let end: Vec<usize> = head.iter().zip(&sizes).map(|(&s, &n)| s + n as usize).collect();
    let mut h = Heads::new(head.iter().map(|&s| mt[s]).collect());
    let mut out = Vec::with_capacity(text.len());
    let mut prev = usize::MAX;
    for _ in 0..ends.len() {
        let r = get_varint(order, &mut p) as usize;
        let a = if r == 0 { h.pick(prev) } else { h.nth(r, prev)? };
        let j = head[a];
        let s = if j == 0 { 0 } else { ends[j - 1] };
        out.extend_from_slice(&text[s..ends[j]]);
        head[a] += 1;
        h.advance(a, if head[a] < end[a] { Some(mt[head[a]]) } else { None });
        prev = a;
    }
    Some(out)
}

/// The groups that still have lines, for opening: like Heads, with each entry's key and
/// group packed in one number ((time << 12) | group: the same order as (time, group) while
/// times fit in 51 bits), so a comparison is one compare and a move one store.
struct Heap2 {
    heap: Vec<i64>,
    pos: Vec<u32>,
    aux: std::collections::BinaryHeap<std::cmp::Reverse<(i64, u32)>>,
}

const GB: u32 = 12; // bits for the group (MAX_GROUPS = 4096)

impl Heap2 {
    fn new(key: &[i64]) -> Heap2 {
        let g = key.len();
        let mut h = Heap2 { heap: key.iter().enumerate().map(|(i, &k)| k << GB | i as i64).collect(), pos: (0..g as u32).collect(), aux: Default::default() };
        for i in (0..g / 2).rev() {
            h.down(i);
        }
        h
    }
    /// Can (time, group) be packed: times within +-2^50.
    fn fits(t: i64) -> bool {
        (-(1i64 << 50)..(1i64 << 50)).contains(&t)
    }
    #[inline]
    fn up(&mut self, mut i: usize) {
        let x = self.heap[i];
        while i > 0 {
            let p = (i - 1) / 2;
            let y = self.heap[p];
            if x >= y {
                break;
            }
            self.heap[i] = y;
            self.pos[(y & 0xfff) as usize] = i as u32;
            i = p;
        }
        self.heap[i] = x;
        self.pos[(x & 0xfff) as usize] = i as u32;
    }
    #[inline]
    fn down(&mut self, mut i: usize) {
        let n = self.heap.len();
        let x = self.heap[i];
        loop {
            let l = 2 * i + 1;
            if l >= n {
                break;
            }
            let mut c = l;
            if l + 1 < n && self.heap[l + 1] < self.heap[l] {
                c = l + 1;
            }
            let y = self.heap[c];
            if y >= x {
                break;
            }
            self.heap[i] = y;
            self.pos[(y & 0xfff) as usize] = i as u32;
            i = c;
        }
        self.heap[i] = x;
        self.pos[(x & 0xfff) as usize] = i as u32;
    }
    /// Group g moved to its next line, with time t (None: g has no lines left).
    #[inline]
    fn advance(&mut self, g: usize, t: Option<i64>) {
        let i = self.pos[g] as usize;
        match t {
            Some(t) => {
                let old = self.heap[i];
                let x = t << GB | g as i64;
                self.heap[i] = x;
                if x < old {
                    self.up(i)
                } else if x > old {
                    self.down(i)
                }
            }
            None => {
                let last = self.heap.len() - 1;
                let y = self.heap[last];
                self.heap.pop();
                self.pos[g] = NONE;
                if i != last {
                    self.heap[i] = y;
                    self.pos[(y & 0xfff) as usize] = i as u32;
                    self.down(i);
                    self.up(self.pos[(y & 0xfff) as usize] as usize);
                }
            }
        }
    }
    #[inline]
    fn key(&self, g: usize) -> i64 {
        self.heap[self.pos[g] as usize] >> GB
    }
    /// Heads::pick.
    #[inline]
    fn pick(&self, prev: usize) -> usize {
        let top = (self.heap[0] & 0xfff) as usize;
        if prev < self.pos.len() && prev != top && self.pos[prev] != NONE && self.key(prev) == self.heap[0] >> GB {
            prev
        } else {
            top
        }
    }
    /// Heads::nth: the group at place r of the rule's order (a best-first walk down the heap).
    fn nth(&mut self, r: usize, prev: usize) -> Option<usize> {
        use std::cmp::Reverse;
        let pw = prev < self.pos.len() && self.pos[prev] != NONE;
        let kp = if pw { self.key(prev) } else { 0 };
        let mut prev_done = !pw;
        let mut k = 0;
        self.aux.clear();
        if !self.heap.is_empty() {
            self.aux.push(Reverse((self.heap[0], 0)));
        }
        while let Some(Reverse((x, i))) = self.aux.pop() {
            if !prev_done && x >> GB >= kp {
                prev_done = true;
                if k == r {
                    return Some(prev);
                }
                k += 1;
            }
            for c in [2 * i as usize + 1, 2 * i as usize + 2] {
                if c < self.heap.len() {
                    self.aux.push(Reverse((self.heap[c], c as u32)));
                }
            }
            let g = (x & 0xfff) as usize;
            if !(pw && g == prev) {
                if k == r {
                    return Some(g);
                }
                k += 1;
            }
        }
        None
    }
}

/// ungroup, writing the lines straight into out[o..] (returns the end) instead of a new
/// buffer.
#[inline(never)]
pub fn ungroup_into(text: &[u8], ends: &[usize], times: &[i64], order: &[u8], out: &mut [u8], mut o: usize) -> Option<usize> {
    let mut p = 0;
    get_varint(order, &mut p);
    let g = get_varint(order, &mut p) as usize;
    if g > MAX_GROUPS {
        return None;
    }
    let sizes: Vec<u32> = (0..g).map(|_| get_varint(order, &mut p) as u32).collect();
    if sizes.iter().map(|&n| n as usize).sum::<usize>() != ends.len() || sizes.contains(&0) {
        return None;
    }
    let mt = merge_times(times, &sizes);
    if !mt.iter().all(|&t| Heap2::fits(t)) {
        // times too far apart to pack: the plain way
        let back = ungroup(text, ends, times, order)?;
        out[o..o + back.len()].copy_from_slice(&back);
        return Some(o + back.len());
    }
    let mut head = starts(&sizes);
    let end: Vec<usize> = head.iter().zip(&sizes).map(|(&s, &n)| s + n as usize).collect();
    let mut h = Heap2::new(&head.iter().map(|&s| mt[s]).collect::<Vec<i64>>());
    let mut prev = usize::MAX;
    for _ in 0..ends.len() {
        let r = get_varint(order, &mut p) as usize;
        let a = if r == 0 { h.pick(prev) } else { h.nth(r, prev)? };
        let j = head[a];
        let s = if j == 0 { 0 } else { ends[j - 1] };
        let n = ends[j] - s;
        out[o..o + n].copy_from_slice(&text[s..ends[j]]);
        o += n;
        head[a] += 1;
        h.advance(a, if head[a] < end[a] { Some(mt[head[a]]) } else { None });
        prev = a;
    }
    Some(o)
}

// ---- the cheap look at a block: is grouping worth it, and on which field? ----

/// The look uses 4 runs of CHUNK lines spread over the block (the start alone can mislead).
const CHUNK: usize = 512;
/// A field with fewer values is a category (like a log level), not a source.
const MIN_GROUPS: usize = 8;

/// The first clock time of a line (H:MM:SS[.frac] in its first 64 bytes), in microseconds.
/// Only for the cheap look; the packed order uses the clock column of the packer.
fn text_time(l: &[u8]) -> i64 {
    let n = l.len().min(64);
    for k in 0..n {
        if l[k].is_ascii_digit() && (k == 0 || !(l[k - 1].is_ascii_digit() || l[k - 1] == b':')) {
            let mut j = k;
            while j < l.len() && l[j] != b' ' {
                j += 1;
            }
            if let Some((_, kind, v)) = super::time_at(l, k, j) {
                return super::micros(kind, v);
            }
        }
    }
    NO_TIME
}

/// How many lines the merge rule gets wrong (times in the original order).
fn misses(gid: &[u32], sizes: &[u32], times: &[i64]) -> usize {
    let perm = grouped_order(gid, sizes);
    let gt: Vec<i64> = perm.iter().map(|&i| times[i as usize]).collect();
    let mt = merge_times(&gt, sizes);
    let mut head = starts(sizes);
    let end: Vec<usize> = head.iter().zip(sizes).map(|(&s, &n)| s + n as usize).collect();
    let mut h = Heads::new(head.iter().map(|&s| mt[s]).collect());
    let (mut prev, mut m) = (usize::MAX, 0);
    for &a in gid {
        let a = a as usize;
        m += (h.pick(prev) != a) as usize;
        head[a] += 1;
        h.advance(a, if head[a] < end[a] { Some(mt[head[a]]) } else { None });
        prev = a;
    }
    m
}

/// The sample for the cheap look: 4 runs of CHUNK lines spread over the block.
pub fn sample<'a>(lines: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let n = lines.len();
    if n <= 4 * CHUNK {
        return lines.to_vec();
    }
    let mut s = Vec::with_capacity(4 * CHUNK);
    for k in 0..4 {
        let a = k * (n - CHUNK) / 3;
        s.extend_from_slice(&lines[a..a + CHUNK]);
    }
    s
}

/// The field worth grouping the sample on, or None. A field qualifies when its values
/// interleave (it changes in at least 20% of the lines and at least twice as often as it has
/// values), it has MIN_GROUPS..MAX_GROUPS values, and the merge on clock times gets fewer
/// than 15% of the lines wrong.
pub fn choose_field(s: &[&[u8]]) -> Option<usize> {
    // too few clock times: the merge has nothing to go on (a quick check on a few lines first)
    if s.iter().step_by(64).filter(|l| text_time(l) != NO_TIME).count() * 2 < s.len().div_ceil(64) {
        return None;
    }
    // one pass: how often each of the fields 0-5 changes from line to line
    let mut changes = [0usize; 6];
    let mut prev: [&[u8]; 6] = [&[]; 6];
    for &l in s {
        let (n, mut i) = (l.len(), 0);
        for f in 0..6 {
            while i < n && l[i] == b' ' {
                i += 1;
            }
            let a = i;
            while i < n && l[i] != b' ' {
                i += 1;
            }
            changes[f] += (l[a..i] != *prev[f]) as usize;
            prev[f] = &l[a..i];
        }
    }
    let mut times: Vec<i64> = Vec::new();
    // (field, group of each line, misses) of each field that qualifies
    let mut ok: Vec<(usize, Vec<u32>, usize)> = Vec::new();
    for f in 0..6 {
        if changes[f] * 5 < s.len() {
            continue;
        }
        let Some((gid, sizes)) = groups_of(s, f) else { continue };
        if sizes.len() < MIN_GROUPS || changes[f] < 2 * sizes.len() {
            continue;
        }
        if times.is_empty() {
            times = s.iter().map(|l| text_time(l)).collect();
        }
        let m = misses(&gid, &sizes, &times);
        if m * 100 < s.len() * 15 {
            ok.push((f, gid, m));
        }
    }
    // the field the merge gets wrong least; then a finer source inside it (a thread inside a
    // process) as long as one refines it
    let mut best = (0..ok.len()).min_by_key(|&i| ok[i].2)?;
    loop {
        let finer = (0..ok.len()).filter(|&i| i != best && refines(&ok[i].1, &ok[best].1)).min_by_key(|&i| ok[i].2);
        match finer {
            Some(i) => best = i,
            None => break,
        }
    }
    Some(ok[best].0)
}

/// Does grouping a split groups b further (each a group within one b group, for 99% of
/// the lines)? A field with the same groups does not count.
fn refines(a: &[u32], b: &[u32]) -> bool {
    let na = a.iter().max().map_or(0, |&x| x as usize + 1);
    let nb = b.iter().max().map_or(0, |&x| x as usize + 1);
    if na <= nb {
        return false;
    }
    let mut owner = vec![u32::MAX; na];
    let mut off = 0usize;
    for (&x, &y) in a.iter().zip(b) {
        let o = &mut owner[x as usize];
        if *o == u32::MAX {
            *o = y;
        }
        off += (*o != y) as usize;
    }
    off * 100 <= a.len()
}
