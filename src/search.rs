//! Line search for `fewer-pack grep`: the lines that contain a piece of text (optionally ignoring
//! ASCII case), found on all cores.

/// How common a byte is in log text (higher = more common); the search scans for the
/// needle's rarest byte and checks the rest around it.
fn commonness(c: u8) -> u32 {
    let c = c.to_ascii_lowercase();
    match c {
        b' ' => 100,
        b'e' => 90,
        b't' | b'a' | b'o' | b'i' | b'n' => 80,
        b's' | b'r' | b'h' | b'l' | b'd' | b'c' => 70,
        b'0'..=b'9' => 75,
        b'u' | b'm' | b'f' | b'p' | b'g' | b'w' | b'y' | b'b' => 50,
        b'.' | b':' | b'-' | b'_' | b'/' | b',' => 60,
        b'v' | b'k' => 30,
        b'x' | b'j' | b'q' | b'z' => 10,
        _ => 20,
    }
}

/// First i >= from with h[i] == a or h[i] == b (8 bytes at a time).
#[inline]
fn find2(h: &[u8], from: usize, a: u8, b: u8) -> Option<usize> {
    let (sa, sb) = (u64::from_ne_bytes([a; 8]), u64::from_ne_bytes([b; 8]));
    let mut i = from;
    while i + 8 <= h.len() {
        let w = u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
        let (x, y) = (w ^ sa, w ^ sb);
        let t = (x.wrapping_sub(0x0101_0101_0101_0101) & !x | y.wrapping_sub(0x0101_0101_0101_0101) & !y) & 0x8080_8080_8080_8080;
        if t != 0 {
            return Some(i + (t.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    (i..h.len()).find(|&k| h[k] == a || h[k] == b)
}

pub struct Needle {
    text: Vec<u8>,
    icase: bool,
    /// the rarest byte's place in the needle
    at: usize,
    /// the needle holds a line end: no line contains it (a match would run across lines)
    never: bool,
}

impl Needle {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn new(text: &[u8], icase: bool) -> Needle {
        let text: Vec<u8> = if icase { text.to_ascii_lowercase() } else { text.to_vec() };
        let at = (0..text.len()).min_by_key(|&i| commonness(text[i])).unwrap_or(0);
        let never = text.contains(&b'\n');
        Needle { text, icase, at, never }
    }

    #[inline]
    fn matches_at(&self, h: &[u8], s: usize) -> bool {
        let n = &self.text;
        if s + n.len() > h.len() {
            return false;
        }
        if self.icase {
            h[s..s + n.len()].iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b)
        } else {
            h[s..s + n.len()] == n[..]
        }
    }

    /// Start of the first match in h[from..], if any.
    fn find(&self, h: &[u8], from: usize) -> Option<usize> {
        if self.never {
            return None;
        }
        if self.text.is_empty() {
            return if from <= h.len() { Some(from) } else { None };
        }
        let c = self.text[self.at];
        let (a, b) = if self.icase { (c, c.to_ascii_uppercase()) } else { (c, c) };
        let mut i = from + self.at;
        while let Some(k) = find2(h, i, a, b) {
            let s = k - self.at;
            if self.matches_at(h, s) {
                return Some(s);
            }
            i = k + 1;
        }
        None
    }
}

/// A block of lines tested before it is opened (engine::Filter): whether a line made of the
/// block's patterns and values may hold the needle. A line is a pattern's fixed text with a
/// value in each place marked by a 00 byte, and a value is a run of the block's texts, one
/// of its words, a month or weekday name (engine::value_names), or a number, clock time, IP
/// address, hex number or UUID: digits, the letters a-f / A-F and "-.:, " only. The needle
/// (its first 63 bytes, which a line holding it holds too) is matched against each pattern
/// byte by byte as a set of positions (shift-and), a value place taking any piece of it that
/// a value may be: so "no" is exact (no line holds it) and "yes" means "open the block". A
/// piece of the needle in a text or word is only known by its bytes and pairs of bytes there
/// (all the needle in one value: found in the texts or words as it is).
pub struct Filter {
    /// the needle's first 63 bytes (lowercase with -i)
    n: Vec<u8>,
    icase: bool,
    /// per byte: bit j set when the needle's byte j is that byte (with -i: in either case)
    at: Vec<u64>,
    /// per start j: the end of the longest piece n[j..e] a number or a name may print
    fixed: Vec<usize>,
    /// the needle as it is searched in the texts and words
    nd: Needle,
    /// the needle's bytes and pairs of bytes (bit sets), and how many different ones
    want_b: [u64; 4],
    want_p: Vec<u64>,
    wants: usize,
    /// the needle holds a line end (no line holds it) / is empty (every line does)
    never: bool,
    always: bool,
}

impl Filter {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn new(text: &[u8], icase: bool, names: &[&[u8]]) -> Filter {
        let low = |c: u8| if icase { c.to_ascii_lowercase() } else { c };
        let n: Vec<u8> = text.iter().take(63).map(|&c| low(c)).collect();
        let mut at = vec![0u64; 256];
        for (c, m) in at.iter_mut().enumerate() {
            for (j, &x) in n.iter().enumerate() {
                if low(c as u8) == x {
                    *m |= 1 << j;
                }
            }
        }
        let fixed = (0..n.len())
            .map(|j| {
                let mut e = j;
                while e < n.len() && b"0123456789abcdefABCDEF-.:, ".contains(&n[e]) {
                    e += 1;
                }
                for name in names {
                    for s in 0..name.len() {
                        let l = n[j..].iter().zip(&name[s..]).take_while(|(x, y)| **x == low(**y)).count();
                        e = e.max(j + l);
                    }
                }
                e
            })
            .collect();
        let (mut want_b, mut want_p) = ([0u64; 4], vec![0u64; 1024]);
        for (j, &c) in n.iter().enumerate() {
            want_b[c as usize >> 6] |= 1 << (c & 63);
            if j + 1 < n.len() {
                let k = (c as usize) << 8 | n[j + 1] as usize;
                want_p[k >> 6] |= 1 << (k & 63);
            }
        }
        let wants = want_b.iter().chain(&want_p).map(|w| w.count_ones() as usize).sum();
        Filter { nd: Needle::new(&text[..n.len()], icase), n, icase, at, fixed, want_b, want_p, wants, never: text.contains(&b'\n'), always: text.is_empty() }
    }

    /// Whether a line of a block with these patterns (and these texts and words: without
    /// them, true only when a number or name piece is enough) may hold the needle.
    pub fn may(&self, pats: &[u8], tw: Option<(&[u8], &[u8])>) -> bool {
        if self.always || self.never {
            return self.always;
        }
        let (n, len) = (&self.n, self.n.len());
        let mut m = self.fixed.clone();
        if let Some((texts, words)) = tw {
            // which of the needle's bytes and pairs of bytes the texts and words hold (the scan
            // stops once it has found them all: nothing more could be learned)
            let mut pairs = vec![0u64; 1024];
            let mut bytes = [0u64; 4];
            let mut left = self.wants;
            'scan: for h in [texts, words] {
                let mut prev = 0usize;
                for &c in h {
                    let c = if self.icase { c.to_ascii_lowercase() } else { c } as usize;
                    let k = prev << 8 | c;
                    prev = c;
                    let (b, p) = (1u64 << (c & 63), 1u64 << (k & 63));
                    if self.want_b[c >> 6] & b & !bytes[c >> 6] != 0 {
                        bytes[c >> 6] |= b;
                        left -= 1;
                    }
                    if self.want_p[k >> 6] & p & !pairs[k >> 6] != 0 {
                        pairs[k >> 6] |= p;
                        left -= 1;
                    }
                    if left == 0 {
                        break 'scan;
                    }
                }
            }
            // (the pair of a text's first byte with 0 was set: it only adds a 00 pair)
            let byte = |j: usize| bytes[n[j] as usize >> 6] >> (n[j] & 63) & 1 == 1;
            let pair = |j: usize| {
                let k = (n[j] as usize) << 8 | n[j + 1] as usize;
                pairs[k >> 6] >> (k & 63) & 1 == 1
            };
            let mut t = vec![len; len + 1];
            for j in (0..len).rev() {
                t[j] = if !byte(j) { j } else if j + 1 < len && pair(j) { t[j + 1] } else { j + 1 };
            }
            if t[0] == len && self.nd.find(texts, 0).is_none() && self.nd.find(words, 0).is_none() {
                t[0] = len - 1;
            }
            for j in 0..len {
                m[j] = m[j].max(t[j]);
            }
        }
        // a value place: from position j on, any of j..=m[j]
        let vm: Vec<u64> = (0..len).map(|j| (u64::MAX >> (63 - m[j])) & !((1u64 << j) - 1)).collect();
        let goal = 1u64 << len;
        let mut r = 1u64;
        for &c in pats {
            if c == 0 {
                let (mut x, mut nr) = (r, 1u64);
                while x != 0 {
                    nr |= vm[x.trailing_zeros() as usize];
                    x &= x - 1;
                }
                r = nr;
            } else {
                r = (r & self.at[c as usize]) << 1 | 1;
            }
            if r & goal != 0 {
                return true;
            }
        }
        false
    }
}

/// Append every line of `text` that contains the needle to `out`, each as prefix + line +
/// '\n'. `text` holds whole lines (the last one may lack its '\n'). Returns the count.
pub fn grep_lines(text: &[u8], nd: &Needle, prefix: &[u8], out: &mut Vec<u8>) -> usize {
    let mut count = 0;
    let mut pos = 0;
    while pos < text.len() {
        let Some(m) = nd.find(text, pos) else { break };
        let ls = text[pos..m].iter().rposition(|&c| c == b'\n').map_or(pos, |k| pos + k + 1);
        let le = text[m..].iter().position(|&c| c == b'\n').map_or(text.len(), |k| m + k);
        if nd.text.is_empty() && m == text.len() {
            break;
        }
        out.extend_from_slice(prefix);
        out.extend_from_slice(&text[ls..le]);
        out.push(b'\n');
        count += 1;
        pos = le + 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(text: &str, needle: &str, icase: bool) -> String {
        let mut o = Vec::new();
        grep_lines(text.as_bytes(), &Needle::new(needle.as_bytes(), icase), b"", &mut o);
        String::from_utf8(o).unwrap()
    }

    /// search::Filter against brute force on random blocks: lines made of random patterns
    /// (fixed text and value places) and values of every source; whenever a line holds the
    /// needle, the filter must say "may" (with and without -i, short and long needles).
    #[test]
    fn filter_never_skips_a_match() {
        let names: &[&[u8]] = &[b"Jan", b"Monday", b"Sep"];
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let alpha = b"abcAB01.:- _qz";
        let (mut checked, mut skipped) = (0, 0);
        for round in 0..3000 {
            let rs = |rnd: &mut dyn FnMut(usize) -> usize, n: usize| -> Vec<u8> { (0..rnd(n)).map(|_| alpha[rnd(alpha.len())]).collect() };
            let texts = rs(&mut rnd, 12);
            let words: Vec<Vec<u8>> = (0..rnd(3)).map(|_| rs(&mut rnd, 5)).collect();
            let mut wtab = Vec::new();
            for w in &words {
                wtab.extend_from_slice(w);
                wtab.push(b'\n');
            }
            let (mut pats, mut lines) = (Vec::new(), Vec::new());
            for _ in 0..1 + rnd(3) {
                let pieces = rnd(4);
                let mut pat = rs(&mut rnd, 4);
                for _ in 0..pieces {
                    pat.push(0);
                    pat.extend(rs(&mut rnd, 3));
                }
                // a few lines of this pattern, each value from one source
                for _ in 0..3 {
                    let mut line = Vec::new();
                    for &c in &pat {
                        if c != 0 {
                            line.push(c);
                            continue;
                        }
                        match rnd(4) {
                            0 if !texts.is_empty() => {
                                let a = rnd(texts.len());
                                let b = a + rnd(texts.len() - a + 1);
                                line.extend_from_slice(&texts[a..b]);
                            }
                            1 if !words.is_empty() => line.extend_from_slice(&words[rnd(words.len())]),
                            2 => line.extend_from_slice(names[rnd(names.len())]),
                            _ => line.extend((0..1 + rnd(4)).map(|_| b"0123456789abcdefABCDEF-.:, "[rnd(27)])),
                        }
                    }
                    lines.push(line);
                }
                pats.extend_from_slice(&pat);
                pats.push(b'\n');
            }
            for _ in 0..20 {
                let icase = rnd(3) == 0;
                let l = &lines[rnd(lines.len())];
                let mut nd: Vec<u8> = if rnd(4) == 0 || l.is_empty() {
                    rs(&mut rnd, 6)
                } else {
                    let a = rnd(l.len());
                    l[a..(a + 1 + rnd(8)).min(l.len())].to_vec()
                };
                if round % 50 == 0 {
                    // a needle longer than the 63 bytes the filter looks at
                    nd = (0..70).map(|i| if i % 2 == 0 { b'a' } else { b'0' }).collect();
                }
                if icase {
                    nd = nd.iter().map(|&c| if rnd(2) == 0 { c.to_ascii_uppercase() } else { c }).collect();
                }
                let f = Filter::new(&nd, icase, names);
                let low = |v: &[u8]| if icase { v.to_ascii_lowercase() } else { v.to_vec() };
                let n2 = low(&nd);
                let holds = lines.iter().any(|ln| n2.is_empty() || low(ln).windows(n2.len()).any(|w| w == &n2[..]));
                let quick = f.may(&pats, None);
                let full = f.may(&pats, Some((&texts[..], &wtab[..])));
                assert!(!quick || full, "the quick answer yes must stay yes");
                assert!(!holds || full, "skipped a match: {:?} in {:?}", String::from_utf8_lossy(&nd), lines.iter().map(|l| String::from_utf8_lossy(l)).collect::<Vec<_>>());
                checked += 1;
                skipped += !full as usize;
            }
        }
        // the filter does say "no" (most random needles of 2+ bytes are in no line)
        assert!(skipped > checked / 10, "{skipped} of {checked}");
        let f = Filter::new(b"blk_77", false, names);
        assert!(f.may(b"x blk_\0 y\n", None) && !f.may(b"x blk \0 y\n", Some((b"ab", b"x\n"))));
        assert!(!Filter::new(b"a\nb", false, names).may(b"a\0b\n", Some((b"ab", b""))) && Filter::new(b"", true, names).may(b"\n", None));
    }

    #[test]
    fn finds_lines() {
        let t = "alpha error one\nbeta ok\nERROR two\nlast error";
        assert_eq!(g(t, "error", false), "alpha error one\nlast error\n");
        assert_eq!(g(t, "error", true), "alpha error one\nERROR two\nlast error\n");
        assert_eq!(g(t, "zzz", false), "");
        assert_eq!(g(t, "a", false), "alpha error one\nbeta ok\nlast error\n");
        assert_eq!(g("x\n\ny\n", "", false), "x\n\ny\n");
        assert_eq!(g("abc", "abcd", false), "");
        // a line never holds a line end: no match across two lines
        assert_eq!(g("alpha one\nbeta two\nalpha\n", "one\nbeta", false), "");
        assert_eq!(g("alpha one\nbeta two\nalpha\n", "alpha\n", true), "");
        assert_eq!(g("x\n\ny\n", "\n", false), "");
        let big: String = (0..200_000).map(|i| format!("line {i} {}\n", if i % 1000 == 7 { "needle" } else { "hay" })).collect();
        let nd = Needle::new(b"needle", false);
        let mut a = Vec::new();
        let mut b = Vec::new();
        assert_eq!(grep_lines(big.as_bytes(), &nd, b"p:", &mut a), 200);
        // searched in runs cut at line ends (as fewer-pack grep's jobs are), the same output
        let h = big.as_bytes();
        let (mut s, mut n) = (0, 0);
        for k in 1..=7 {
            let t = (k * h.len() / 7).max(s);
            let e = h[t..].iter().position(|&c| c == b'\n').map_or(h.len(), |p| t + p + 1);
            n += grep_lines(&h[s..e], &nd, b"p:", &mut b);
            s = e;
        }
        assert_eq!(n, 200);
        assert_eq!(a, b);
    }
}
