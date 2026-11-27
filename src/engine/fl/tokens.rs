// Whole tokens: a word that mixes letters and values ("calvisitor-10-105-160-95",
// "(host-141.STSK.macomnet.net)", "rhost=218.188.2.4") can be coded as one value, a word of
// the block's word table (kind WORD, by its number: the opener needs nothing new), instead of
// as its numbers, so the shapes that differ only in that word become one pattern. Chosen per
// slot, once per block, before the word picker:
// - a slot is a word place: the shape's words before it and its place in the line. A word with
//   a value and a letter makes a slot; plain words (a letter, no value) at a slot join it,
// - a slot is looked at only when at least half its lines have a shape that merges with
//   another one (the shapes differ only in that word); then on a sample of lines: its uses,
//   its different whole words and the uses where the word changed,
// - it becomes one value when its different words are few (1 per TOKEN_RATE uses), change
//   seldom (1 use in TOKEN_CHANGE) and are paid by the shapes merged away (TOKEN_SHAPE bytes
//   each against TOKEN_WORD bytes + the length for each different word).
// The block's merged shapes then go on as the shapes (smap: shape -> merged shape).
use super::*;

/// A slot becomes one value when it has at least TOKEN_MIN sampled uses, at most one different
/// word per TOKEN_RATE uses, a changed word in at most one use per TOKEN_CHANGE, at least
/// TOKEN_MERGE percent of its lines in shapes that merge, and when the shapes merged away,
/// TOKEN_SHAPE bytes each, pay for its different words (as estimated for the whole block),
/// TOKEN_WORD bytes each plus their length.
const TOKEN_MIN: u32 = 16;
const TOKEN_RATE: u32 = 16;
const TOKEN_CHANGE: u32 = 8;
const TOKEN_MERGE: u64 = 50;
const TOKEN_SHAPE: u64 = 96;
const TOKEN_WORD: u64 = 6;

/// Lines looked at: all of a block up to SAMPLE_RUNS * SAMPLE_LEN lines, else SAMPLE_RUNS runs
/// of SAMPLE_LEN lines spread over it.
const SAMPLE_RUNS: usize = 8;
const SAMPLE_LEN: usize = 1024;

/// A word of a shape that becomes one value: where it is in the shape, the values in it, and
/// how to find it in a line from the values' ends.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Tok {
    off: u32,  // the word's start in the shape
    len: u32,  // its length in the shape
    j0: u32,   // the line's values before it
    nv: u32,   // values in it (0: a plain word)
    gap: u32,  // shape bytes from the end of value j0 - 1 (or the line start) to the word
    post: u32, // shape bytes after its last value
}

impl Tok {
    /// The word's start and end in a line with values lv.
    #[inline]
    pub fn span(&self, lv: &[Val]) -> (u32, u32) {
        let j0 = self.j0 as usize;
        let s = if j0 > 0 { lv[j0 - 1].3 + self.gap } else { self.gap };
        let e = if self.nv > 0 { lv[j0 + self.nv as usize - 1].3 + self.post } else { s + self.len };
        (s, e)
    }
    #[inline]
    pub fn j0(&self) -> usize {
        self.j0 as usize
    }
    #[inline]
    pub fn nv(&self) -> usize {
        self.nv as usize
    }
    /// the word's start and length in its shape (FL_SHADOW)
    #[allow(dead_code)]
    pub fn place(&self) -> (u32, u32) {
        (self.off, self.len)
    }
}

/// The chosen tokens of a block: per shape its words that become values, and the merged shapes.
pub struct Tokens {
    pub tidx: Vec<u32>, // shape r's tokens: toks[tidx[r]..tidx[r + 1]]
    pub toks: Vec<Tok>,
    pub smap: Vec<u32>, // shape -> merged shape
    pub merged: Shapes,
}

/// FL_NOTOKENS: no whole tokens (for tests; on by default).
pub fn tokens_on() -> bool {
    static S: crate::engine::fl::NoEnvOnce<bool> = crate::engine::fl::NoEnvOnce::new();
    *S.get_or_init(|| !env_on("FL_NOTOKENS"))
}

/// The line ranges looked at.
fn sample_runs(n: usize) -> Vec<(usize, usize)> {
    if n <= SAMPLE_RUNS * SAMPLE_LEN {
        return vec![(0, n)];
    }
    (0..SAMPLE_RUNS)
        .map(|k| {
            let s = k * (n - SAMPLE_LEN) / (SAMPLE_RUNS - 1);
            (s, s + SAMPLE_LEN)
        })
        .collect()
}

/// A word of a shape with a letter: its place, and the hash of its slot.
#[derive(Clone, Copy)]
struct Word {
    tok: Tok,
    key: u64,  // its slot: the words before it, and its place
    full: u64, // the shape without it: the words before and after it
    r: u32,    // its shape
}

/// Choose the block's tokens (None: none chosen). `roff`: the shapes' starts in p1's text.
/// `cl`: cls64 of p1.shapes.text.
pub fn pick_tokens(p1: &Pass1, roff: &[u32], lines: &[&[u8]], cnt: &[u32], cl: &[[u64; 4]]) -> Option<Tokens> {
    let m = p1.rnv.len();
    let text = &p1.shapes.text;
    // the words with a letter of shapes that have a word with a value and a letter: words with
    // values (tw) and plain words (pw), each in shape order
    let (mut tw, mut pw): (Vec<Word>, Vec<Word>) = (Vec::new(), Vec::new());
    let mut wd: Vec<(Tok, bool, u64)> = Vec::new(); // every word: place, has a letter, hash
    let mut hs: Vec<u64> = Vec::new();
    for r in 0..m {
        let (b0, e) = (roff[r] as usize, roff[r + 1] as usize - 1);
        let s = &text[b0..e];
        wd.clear();
        let (mut i, mut marks, mut last_end) = (b0, 0u32, 0u32);
        let mut cand = false;
        loop {
            // a word: up to the next space (cls64's masks of the text, places in `text`). Fast
            // path: the word ends inside i's 64-byte mask word (or at the shape's end); else the
            // general walk.
            let (c, o) = (&cl[i >> 6], i & 63);
            let lim = (64 - o).min(e - i);
            let jl = ((c[CL_SP] >> o).trailing_zeros() as usize).min(lim);
            let (j, nv, lastm, letter) = if jl < lim || lim == e - i {
                let m = if jl == 0 { 0 } else { u64::MAX >> (64 - jl) };
                let mk = c[CL_MK] >> o & m;
                (i + jl, mk.count_ones(), i + 63 - (mk.leading_zeros() as usize).min(63), c[CL_AL] >> o & m != 0)
            } else {
                let j = cl_next(cl, i, e, |c| c[CL_SP]);
                let (nv, lastm) = cl_count_last(cl, i, j, CL_MK);
                (j, nv, lastm, cl_next(cl, i, j, |c| c[CL_AL]) < j)
            };
            let gap = (i - b0) as u32 - last_end;
            let post = if nv > 0 { (j - lastm - 1) as u32 } else { 0 };
            cand |= letter && nv > 0;
            wd.push((Tok { off: (i - b0) as u32, len: (j - i) as u32, j0: marks, nv, gap, post }, letter, 0));
            if nv > 0 {
                last_end = (lastm - b0) as u32 + 1;
            }
            marks += nv;
            if j >= e {
                break;
            }
            i = j + 1;
        }
        if cand {
            // a slot's hash: the words before it and its place; its hole's: also the words after
            let n = wd.len();
            for w in wd.iter_mut() {
                w.2 = hash_bytes(&s[w.0.off as usize..(w.0.off + w.0.len) as usize]);
            }
            hs.clear();
            hs.resize(n + 1, 0x1319_8a2e_0370_7344);
            for k in (0..n).rev() {
                hs[k] = mix(hs[k + 1], wd[k].2);
            }
            let mut h = 0x243f_6a88_85a3_08d3u64;
            for (k, w) in wd.iter().enumerate() {
                if w.1 {
                    let x = Word { tok: w.0, key: mix(h, k as u64), full: mix(mix(h, hs[k + 1].rotate_left(29)), k as u64), r: r as u32 };
                    if w.0.nv > 0 { tw.push(x) } else { pw.push(x) }
                }
                h = mix(h, w.2);
            }
        }
    }
    if tw.is_empty() {
        return None;
    }
    // slots: made by words with values; plain words join a slot that exists. ws: those words
    // in shape order (shape r's: ws[widx[r]..widx[r + 1]]), slot: the slot of each.
    let mut slots = Tab::new(tw.len());
    let mut ns = 0u32;
    let mut tslot: Vec<u32> = Vec::with_capacity(tw.len());
    for w in &tw {
        let x = slots.get_or(w.key, ns);
        ns += (x == ns) as u32;
        tslot.push(x);
    }
    let pslot: Vec<u32> = pw.iter().map(|w| slots.get(w.key).unwrap_or(u32::MAX)).collect();
    let mut ws: Vec<Word> = Vec::with_capacity(tw.len());
    let mut slot: Vec<u32> = Vec::with_capacity(tw.len());
    let mut widx: Vec<u32> = Vec::with_capacity(m + 1);
    let (mut a, mut b) = (0usize, 0usize);
    for r in 0..m as u32 {
        widx.push(ws.len() as u32);
        loop {
            while b < pw.len() && pw[b].r == r && pslot[b] == u32::MAX {
                b += 1;
            }
            let ta = a < tw.len() && tw[a].r == r;
            let tb = b < pw.len() && pw[b].r == r;
            if ta && (!tb || tw[a].tok.off < pw[b].tok.off) {
                ws.push(tw[a]);
                slot.push(tslot[a]);
                a += 1;
            } else if tb {
                ws.push(pw[b]);
                slot.push(pslot[b]);
                b += 1;
            } else {
                break;
            }
        }
    }
    widx.push(ws.len() as u32);
    // where the sample's runs start in the values (`cnt`: the lines of each shape, counted
    // by the caller)
    let runs = sample_runs(lines.len());
    let mut run_vi = Vec::with_capacity(runs.len());
    let (mut vi, mut li) = (0usize, 0usize);
    for &(a, _) in &runs {
        for &r in &p1.rid[li..a] {
            vi += p1.rnv[r as usize] as usize;
        }
        li = a;
        run_vi.push(vi);
    }
    // per slot: its lines, those whose shape merges with another one (same hole: the shapes
    // differ only in this word), its shapes and its holes
    let mut fh = Tab::new(ws.len());
    let mut fm: Vec<u32> = Vec::new();
    let mut fid = vec![0u32; ws.len()];
    let (mut nshape, mut nhole) = (vec![0u32; ns as usize], vec![0u32; ns as usize]);
    for (k, w) in ws.iter().enumerate() {
        if slot[k] != u32::MAX {
            let x = fh.get_or(mix(w.full, slot[k] as u64), fm.len() as u32);
            if x as usize == fm.len() {
                fm.push(0);
                nhole[slot[k] as usize] += 1;
            }
            fm[x as usize] += 1;
            fid[k] = x;
            nshape[slot[k] as usize] += 1;
        }
    }
    let (mut tot, mut mrg) = (vec![0u64; ns as usize], vec![0u64; ns as usize]);
    for (k, w) in ws.iter().enumerate() {
        if slot[k] != u32::MAX {
            tot[slot[k] as usize] += cnt[w.r as usize] as u64;
            if fm[fid[k] as usize] >= 2 {
                mrg[slot[k] as usize] += cnt[w.r as usize] as u64;
            }
        }
    }
    // only slots where at least half the lines have a shape that merges are looked at further
    let cand: Vec<bool> = (0..ns as usize).map(|h| nshape[h] > nhole[h] && mrg[h] * 100 >= tot[h] * TOKEN_MERGE).collect();
    if !cand.iter().any(|&c| c) {
        return None;
    }
    // the sample, per slot: uses, different words, uses with another word than the slot's last
    let (mut uses, mut diff, mut chg) = (vec![0u32; ns as usize], vec![0u32; ns as usize], vec![0u32; ns as usize]);
    let mut dlen = vec![0u64; ns as usize]; // the length of its different words
    let mut lastw = vec![0u64; ns as usize];
    let mut seen = Tab::new(1024);
    for (q, &(a, b)) in runs.iter().enumerate() {
        let mut vi = run_vi[q];
        for li in a..b {
            let r = p1.rid[li] as usize;
            let nv = p1.rnv[r] as usize;
            for k in widx[r] as usize..widx[r + 1] as usize {
                let h = slot[k] as usize;
                if h != u32::MAX as usize && cand[h] {
                    let (s, e) = ws[k].tok.span(&p1.vals[vi..vi + nv]);
                    let x = hash_bytes(&lines[li][s as usize..e as usize]);
                    let before = seen.used;
                    seen.get_or(mix(x, h as u64), 0);
                    if seen.used > before {
                        diff[h] += 1;
                        dlen[h] += (e - s) as u64;
                    }
                    uses[h] += 1;
                    chg[h] += (x != lastw[h]) as u32;
                    lastw[h] = x;
                }
            }
            vi += nv;
        }
    }
    // a slot becomes one value when its words are few for its uses and change little from use
    // to use, and the shapes merged away pay for its different words (their count and length
    // in the sample times lines / uses: estimated for the whole block)
    let conv: Vec<bool> = (0..ns as usize)
        .map(|h| {
            cand[h]
                && uses[h] >= TOKEN_MIN
                && diff[h] * TOKEN_RATE <= uses[h]
                && chg[h] * TOKEN_CHANGE <= uses[h]
                && (nshape[h] - nhole[h]) as u64 * TOKEN_SHAPE * uses[h] as u64 >= (diff[h] as u64 * TOKEN_WORD + dlen[h]) * tot[h]
        })
        .collect();
    if !conv.iter().any(|&c| c) {
        return None;
    }
    // the plan per shape and the merged shapes (numbered in order of first use, as shapes are)
    let mut tidx = Vec::with_capacity(m + 1);
    let mut toks = Vec::new();
    let mut smap = Vec::with_capacity(m);
    let mut merged = Shapes::default();
    let mut g: Vec<u8> = Vec::new();
    tidx.push(0u32);
    for r in 0..m {
        let s = &text[roff[r] as usize..roff[r + 1] as usize - 1];
        let st = toks.len();
        for k in widx[r] as usize..widx[r + 1] as usize {
            if slot[k] != u32::MAX && conv[slot[k] as usize] {
                toks.push(ws[k].tok);
            }
        }
        let t = if toks.len() > st {
            g.clear();
            let mut a = 0usize;
            for tk in &toks[st..] {
                g.extend_from_slice(&s[a..tk.off as usize]);
                g.push(MARK);
                a = (tk.off + tk.len) as usize;
            }
            g.extend_from_slice(&s[a..]);
            merged.find_or_add(&g).0
        } else {
            merged.find_or_add(s).0
        };
        smap.push(t);
        tidx.push(toks.len() as u32);
    }
    Some(Tokens { tidx, toks, smap, merged })
}

/// pick_tokens before M6 (a byte loop per word, TabOld): only FL_SHADOW's old decisions use it.
#[allow(dead_code)]
pub fn pick_tokens_old(p1: &Pass1, roff: &[u32], lines: &[&[u8]], cnt: &[u32]) -> Option<Tokens> {
    let m = p1.rnv.len();
    let text = &p1.shapes.text;
    // the words with a letter of shapes that have a word with a value and a letter: words with
    // values (tw) and plain words (pw), each in shape order
    let (mut tw, mut pw): (Vec<Word>, Vec<Word>) = (Vec::new(), Vec::new());
    let mut wd: Vec<(Tok, bool, u64)> = Vec::new(); // every word: place, has a letter, hash
    let mut hs: Vec<u64> = Vec::new();
    for r in 0..m {
        let s = &text[roff[r] as usize..roff[r + 1] as usize - 1];
        wd.clear();
        let (mut i, mut marks, mut last_end) = (0usize, 0u32, 0u32);
        let mut cand = false;
        loop {
            let mut j = i;
            let (mut nv, mut letter, mut lastm) = (0u32, false, 0usize);
            while j < s.len() && s[j] != b' ' {
                if s[j] == MARK {
                    lastm = j;
                    nv += 1;
                } else {
                    letter |= s[j].is_ascii_alphabetic();
                }
                j += 1;
            }
            let gap = i as u32 - last_end;
            let post = if nv > 0 { (j - lastm - 1) as u32 } else { 0 };
            cand |= letter && nv > 0;
            wd.push((Tok { off: i as u32, len: (j - i) as u32, j0: marks, nv, gap, post }, letter, 0));
            if nv > 0 {
                last_end = lastm as u32 + 1;
            }
            marks += nv;
            if j >= s.len() {
                break;
            }
            i = j + 1;
        }
        if cand {
            // a slot's hash: the words before it and its place; its hole's: also the words after
            let n = wd.len();
            for w in wd.iter_mut() {
                w.2 = hash_bytes(&s[w.0.off as usize..(w.0.off + w.0.len) as usize]);
            }
            hs.clear();
            hs.resize(n + 1, 0x1319_8a2e_0370_7344);
            for k in (0..n).rev() {
                hs[k] = mix(hs[k + 1], wd[k].2);
            }
            let mut h = 0x243f_6a88_85a3_08d3u64;
            for (k, w) in wd.iter().enumerate() {
                if w.1 {
                    let x = Word { tok: w.0, key: mix(h, k as u64), full: mix(mix(h, hs[k + 1].rotate_left(29)), k as u64), r: r as u32 };
                    if w.0.nv > 0 { tw.push(x) } else { pw.push(x) }
                }
                h = mix(h, w.2);
            }
        }
    }
    if tw.is_empty() {
        return None;
    }
    // slots: made by words with values; plain words join a slot that exists. ws: those words
    // in shape order (shape r's: ws[widx[r]..widx[r + 1]]), slot: the slot of each.
    let mut slots = TabOld::new(tw.len());
    let mut ns = 0u32;
    let mut tslot: Vec<u32> = Vec::with_capacity(tw.len());
    for w in &tw {
        let x = slots.get_or(w.key, ns);
        ns += (x == ns) as u32;
        tslot.push(x);
    }
    let pslot: Vec<u32> = pw.iter().map(|w| slots.get(w.key).unwrap_or(u32::MAX)).collect();
    let mut ws: Vec<Word> = Vec::with_capacity(tw.len());
    let mut slot: Vec<u32> = Vec::with_capacity(tw.len());
    let mut widx: Vec<u32> = Vec::with_capacity(m + 1);
    let (mut a, mut b) = (0usize, 0usize);
    for r in 0..m as u32 {
        widx.push(ws.len() as u32);
        loop {
            while b < pw.len() && pw[b].r == r && pslot[b] == u32::MAX {
                b += 1;
            }
            let ta = a < tw.len() && tw[a].r == r;
            let tb = b < pw.len() && pw[b].r == r;
            if ta && (!tb || tw[a].tok.off < pw[b].tok.off) {
                ws.push(tw[a]);
                slot.push(tslot[a]);
                a += 1;
            } else if tb {
                ws.push(pw[b]);
                slot.push(pslot[b]);
                b += 1;
            } else {
                break;
            }
        }
    }
    widx.push(ws.len() as u32);
    // where the sample's runs start in the values (`cnt`: the lines of each shape, counted
    // by the caller)
    let runs = sample_runs(lines.len());
    let mut run_vi = Vec::with_capacity(runs.len());
    let (mut vi, mut li) = (0usize, 0usize);
    for &(a, _) in &runs {
        for &r in &p1.rid[li..a] {
            vi += p1.rnv[r as usize] as usize;
        }
        li = a;
        run_vi.push(vi);
    }
    // per slot: its lines, those whose shape merges with another one (same hole: the shapes
    // differ only in this word), its shapes and its holes
    let mut fh = TabOld::new(ws.len());
    let mut fm: Vec<u32> = Vec::new();
    let mut fid = vec![0u32; ws.len()];
    let (mut nshape, mut nhole) = (vec![0u32; ns as usize], vec![0u32; ns as usize]);
    for (k, w) in ws.iter().enumerate() {
        if slot[k] != u32::MAX {
            let x = fh.get_or(mix(w.full, slot[k] as u64), fm.len() as u32);
            if x as usize == fm.len() {
                fm.push(0);
                nhole[slot[k] as usize] += 1;
            }
            fm[x as usize] += 1;
            fid[k] = x;
            nshape[slot[k] as usize] += 1;
        }
    }
    let (mut tot, mut mrg) = (vec![0u64; ns as usize], vec![0u64; ns as usize]);
    for (k, w) in ws.iter().enumerate() {
        if slot[k] != u32::MAX {
            tot[slot[k] as usize] += cnt[w.r as usize] as u64;
            if fm[fid[k] as usize] >= 2 {
                mrg[slot[k] as usize] += cnt[w.r as usize] as u64;
            }
        }
    }
    // only slots where at least half the lines have a shape that merges are looked at further
    let cand: Vec<bool> = (0..ns as usize).map(|h| nshape[h] > nhole[h] && mrg[h] * 100 >= tot[h] * TOKEN_MERGE).collect();
    if !cand.iter().any(|&c| c) {
        return None;
    }
    // the sample, per slot: uses, different words, uses with another word than the slot's last
    let (mut uses, mut diff, mut chg) = (vec![0u32; ns as usize], vec![0u32; ns as usize], vec![0u32; ns as usize]);
    let mut dlen = vec![0u64; ns as usize]; // the length of its different words
    let mut lastw = vec![0u64; ns as usize];
    let mut seen = TabOld::new(1024);
    for (q, &(a, b)) in runs.iter().enumerate() {
        let mut vi = run_vi[q];
        for li in a..b {
            let r = p1.rid[li] as usize;
            let nv = p1.rnv[r] as usize;
            for k in widx[r] as usize..widx[r + 1] as usize {
                let h = slot[k] as usize;
                if h != u32::MAX as usize && cand[h] {
                    let (s, e) = ws[k].tok.span(&p1.vals[vi..vi + nv]);
                    let x = hash_bytes(&lines[li][s as usize..e as usize]);
                    let before = seen.used;
                    seen.get_or(mix(x, h as u64), 0);
                    if seen.used > before {
                        diff[h] += 1;
                        dlen[h] += (e - s) as u64;
                    }
                    uses[h] += 1;
                    chg[h] += (x != lastw[h]) as u32;
                    lastw[h] = x;
                }
            }
            vi += nv;
        }
    }
    // a slot becomes one value when its words are few for its uses and change little from use
    // to use, and the shapes merged away pay for its different words (their count and length
    // in the sample times lines / uses: estimated for the whole block)
    let conv: Vec<bool> = (0..ns as usize)
        .map(|h| {
            cand[h]
                && uses[h] >= TOKEN_MIN
                && diff[h] * TOKEN_RATE <= uses[h]
                && chg[h] * TOKEN_CHANGE <= uses[h]
                && (nshape[h] - nhole[h]) as u64 * TOKEN_SHAPE * uses[h] as u64 >= (diff[h] as u64 * TOKEN_WORD + dlen[h]) * tot[h]
        })
        .collect();
    if !conv.iter().any(|&c| c) {
        return None;
    }
    // the plan per shape and the merged shapes (numbered in order of first use, as shapes are)
    let mut tidx = Vec::with_capacity(m + 1);
    let mut toks = Vec::new();
    let mut smap = Vec::with_capacity(m);
    let mut merged = Shapes::default();
    let mut g: Vec<u8> = Vec::new();
    tidx.push(0u32);
    for r in 0..m {
        let s = &text[roff[r] as usize..roff[r + 1] as usize - 1];
        let st = toks.len();
        for k in widx[r] as usize..widx[r + 1] as usize {
            if slot[k] != u32::MAX && conv[slot[k] as usize] {
                toks.push(ws[k].tok);
            }
        }
        let t = if toks.len() > st {
            g.clear();
            let mut a = 0usize;
            for tk in &toks[st..] {
                g.extend_from_slice(&s[a..tk.off as usize]);
                g.push(MARK);
                a = (tk.off + tk.len) as usize;
            }
            g.extend_from_slice(&s[a..]);
            merged.find_or_add(&g).0
        } else {
            merged.find_or_add(s).0
        };
        smap.push(t);
        tidx.push(toks.len() as u32);
    }
    Some(Tokens { tidx, toks, smap, merged })
}
