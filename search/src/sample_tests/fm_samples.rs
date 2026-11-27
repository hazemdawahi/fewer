//! The FM index (src/fm.rs) on real log samples: its counts and places equal a plain search.
//!
//! - On each file's own bytes: the count and the places of pieces of the file, and of
//!   pieces changed so that they occur nowhere, equal a plain search; also after the index
//!   is stored and read back.
//! - On the DNA index's text made from each file (the distinct tokens,
//!   lowercased, every byte >= 0x80 as one symbol, sorted, each followed by 0x01): the
//!   tokens that hold a piece (6.3), found through locate and the token starts, or by LF
//!   steps back to the 0x01 before each place, are the tokens a plain scan finds; the
//!   alphabet takes at most 6 levels.
//!
//! The samples (mod.rs): the made-up traces.log is in tests/data; the others are copies of
//! public logs, in FEWER_SAMPLES. Its fm/ holds two windows of whole lines, about 512 KiB
//! each, of the full Loghub Mac and Android logs (data/loghub_full/Mac/Mac.log from byte
//! 11,599,951 and data/loghub_unseen/Android.log from byte 32,374,919): real lines with
//! UTF-8 text, so thousands of bytes >= 0x80, and CRLF line ends. The random numbers come
//! from a fixed seed, so every run is the same.

use std::collections::BTreeSet;
use std::ops::Range;

use super::{have_public, read};
use crate::fm::{Fm, SAMPLE};

/// Real logs: Loghub `*_2k` logs (OpenStack's many IDs), JSON lines, the GH Archive head
/// (nested JSON with UTF-8 text), the two windows with many bytes >= 0x80, and a made-up
/// log of stack traces.
const FILES: &[&str] = &[
    "loghub/HDFS_2k.log",
    "loghub/Windows_2k.log",
    "loghub/Mac_2k.log",
    "loghub/OpenStack_2k.log",
    "json_converted/Android.jsonl",
    "keys/gharchive_head.jsonl",
    "fm/Mac_utf8.log",
    "fm/Android_utf8.log",
    "made_up/traces.log",
];

/// Places are checked one by one for pieces with at most this many (all of them are
/// counted); the unit tests check every place on random texts.
const PLACES_CHECKED: usize = 20_000;

/// FILES, less the public samples while FEWER_SAMPLES is not set.
fn files_to_check(test: &str) -> Vec<&'static str> {
    let public = have_public(&format!("{test}: the public samples"));
    FILES.iter().copied().filter(|f| public || f.starts_with("made_up/")).collect()
}

/// xorshift64*: a small fixed-seed generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Every place where `pattern` occurs, by looking at each place.
fn plain(text: &[u8], pattern: &[u8]) -> Vec<usize> {
    if pattern.is_empty() {
        return (0..=text.len()).collect();
    }
    if pattern.len() > text.len() {
        return Vec::new();
    }
    let first = pattern[0];
    (0..=text.len() - pattern.len())
        .filter(|&i| text[i] == first && &text[i..i + pattern.len()] == pattern)
        .collect()
}

/// The places of every piece, in one pass over the text: at each place, the pieces that
/// start with its byte are compared there. The empty piece occurs at every place.
fn plain_all(text: &[u8], pieces: &[Vec<u8>]) -> Vec<Vec<usize>> {
    let mut by_first: Vec<Vec<usize>> = vec![Vec::new(); 256];
    for (k, p) in pieces.iter().enumerate() {
        if let Some(&b) = p.first() {
            by_first[b as usize].push(k);
        }
    }
    let mut out: Vec<Vec<usize>> = pieces.iter().map(|p| if p.is_empty() { (0..=text.len()).collect() } else { Vec::new() }).collect();
    for (i, &b) in text.iter().enumerate() {
        for &k in &by_first[b as usize] {
            if text[i..].starts_with(&pieces[k]) {
                out[k].push(i);
            }
        }
    }
    out
}

/// Pieces of `text` at random places, of lengths from 1 to 128 bytes; the same pieces with
/// one byte changed (most then occur nowhere); and the start, the end, a piece running past
/// the end, the first line and the empty piece.
fn pieces(r: &mut Rng, text: &[u8]) -> Vec<Vec<u8>> {
    const LENS: &[usize] = &[1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32, 48, 64, 128];
    let n = text.len();
    let mut out = vec![
        Vec::new(),
        text[..20.min(n)].to_vec(),
        text[n.saturating_sub(20)..].to_vec(),
        [&text[n.saturating_sub(5)..], b"x"].concat(),
        text.split(|&b| b == b'\n').next().unwrap().to_vec(),
    ];
    for i in 0..180 {
        let len = LENS[r.below(LENS.len())].min(n);
        let s = r.below(n - len + 1);
        let mut p = text[s..s + len].to_vec();
        if i % 6 == 5 {
            let at = r.below(len);
            p[at] = [0x00, 0x01, 0x7f, 0xfe, p[at] ^ 0x20][r.below(5)];
        }
        out.push(p);
    }
    out
}

#[test]
fn counts_and_places_equal_a_plain_search_on_real_logs() {
    let mut r = Rng(11);
    for name in files_to_check("counts_and_places_equal_a_plain_search_on_real_logs") {
        let text = read(name);
        let fm = Fm::new(&text).unwrap();
        assert_eq!((fm.len(), fm.rate()), (text.len(), SAMPLE));
        let mut stored = Vec::new();
        fm.write(&mut stored);
        let (back, used) = Fm::read(&stored).unwrap();
        assert_eq!(used, stored.len());
        let mut placed = 0;
        let pieces = pieces(&mut r, &text);
        let wanted = plain_all(&text, &pieces);
        for (i, (p, want)) in pieces.iter().zip(wanted).enumerate() {
            if i % 37 == 0 {
                assert_eq!(want, plain(&text, p), "the two plain searches agree");
            }
            assert_eq!(fm.count(p), want.len(), "{name}: count of {:?}", String::from_utf8_lossy(p));
            if want.len() <= PLACES_CHECKED {
                assert_eq!(fm.positions(p).as_ref(), Some(&want), "{name}: places of {:?}", String::from_utf8_lossy(p));
                placed += want.len();
            }
            if i % 10 == 0 {
                assert_eq!(back.count(p), want.len(), "{name}, read back: count of {:?}", String::from_utf8_lossy(p));
                if want.len() <= PLACES_CHECKED {
                    assert_eq!(back.positions(p), Some(want), "{name}, read back");
                }
            }
        }
        assert!(placed > 1000, "{name}: only {placed} places checked");
    }
}

fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80
}

/// A byte as the DNA index keeps it: lowercase, and every byte >= 0x80 as one symbol.
fn fold(b: u8) -> u8 {
    if b >= 0x80 { 0x80 } else { b.to_ascii_lowercase() }
}

/// The DNA index's text of `bytes`: each longest run of ASCII letters,
/// digits and bytes >= 0x80, folded; digit-only runs shorter than 8 left out; the distinct
/// ones sorted, each followed by 0x01. Also the tokens and where each starts in the text.
fn token_text(bytes: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, Vec<usize>) {
    let mut set = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if !token_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < bytes.len() && token_byte(bytes[i]) {
            i += 1;
        }
        let t: Vec<u8> = bytes[s..i].iter().map(|&b| fold(b)).collect();
        if t.len() >= 8 || !t.iter().all(u8::is_ascii_digit) {
            set.insert(t);
        }
    }
    let tokens: Vec<Vec<u8>> = set.into_iter().collect();
    let mut text = Vec::new();
    let mut starts = Vec::with_capacity(tokens.len());
    for t in &tokens {
        starts.push(text.len());
        text.extend_from_slice(t);
        text.push(0x01);
    }
    (text, tokens, starts)
}

/// A piece the DNA index may look up: 3 or more bytes holding a letter or
/// a byte >= 0x80, or 8 or more digits.
fn usable(piece: &[u8]) -> bool {
    piece.len() >= 3 && (piece.iter().any(|&b| b.is_ascii_alphabetic() || b >= 0x80) || piece.len() >= 8)
}

/// The token of each place: through locate and the token starts.
fn tokens_by_place(fm: &Fm, rows: Range<usize>, starts: &[usize]) -> BTreeSet<usize> {
    rows.map(|row| starts.partition_point(|&s| s <= fm.locate(row).unwrap()) - 1).collect()
}

/// The token of each place without locate: LF steps back to the 0x01 before the place. The
/// rows of suffixes that start with 0x01 are in token order (the 0x01 after the last token,
/// then the one before token 1, token 2, ...), so that row, less the first 0x01 row, is the
/// token's number; reaching the text's start means token 0.
fn tokens_by_walk(fm: &Fm, rows: Range<usize>, sep_first: usize) -> BTreeSet<usize> {
    rows.map(|mut row| loop {
        match fm.back(row) {
            None => break 0,
            Some((0x01, sep)) => break sep - sep_first,
            Some((_, next)) => row = next,
        }
    })
    .collect()
}

#[test]
fn tokens_holding_a_piece_on_real_logs() {
    let mut r = Rng(12);
    let dna_alphabet: BTreeSet<u8> = b"\x01\x80abcdefghijklmnopqrstuvwxyz0123456789".iter().copied().collect();
    for name in files_to_check("tokens_holding_a_piece_on_real_logs") {
        let (text, tokens, starts) = token_text(&read(name));
        let fm = Fm::new(&text).unwrap();
        assert!(fm.levels() <= 6, "{name}: {} levels", fm.levels());
        assert!(fm.symbols().iter().all(|b| dna_alphabet.contains(b)), "{name}: a symbol outside the DNA alphabet");
        let mut stored = Vec::new();
        fm.write(&mut stored);
        let (back, _) = Fm::read(&stored).unwrap();
        let sep_first = fm.rows(b"\x01").start;
        assert_eq!(fm.count(b"\x01"), tokens.len());

        let mut looked_up = 0;
        let mut tries = 0;
        while looked_up < 150 && tries < 10_000 {
            tries += 1;
            let piece: Vec<u8> = if looked_up % 8 == 7 {
                // most likely held by no token
                (0..5 + r.below(4)).map(|_| b"qxzjkvw"[r.below(7)]).collect()
            } else {
                let t = &tokens[r.below(tokens.len())];
                if t.len() < 3 {
                    continue;
                }
                let len = 3 + r.below(t.len().min(16) - 2);
                let s = r.below(t.len() - len + 1);
                t[s..s + len].to_vec()
            };
            if !usable(&piece) {
                continue;
            }
            looked_up += 1;
            let want: BTreeSet<usize> = tokens.iter().enumerate().filter(|(_, t)| !plain(t, &piece).is_empty()).map(|(k, _)| k).collect();
            let rows = fm.rows(&piece);
            assert_eq!(rows.len(), plain(&text, &piece).len(), "{name}: count of {:?}", String::from_utf8_lossy(&piece));
            assert_eq!(back.rows(&piece), rows, "{name}: read back");
            assert_eq!(tokens_by_place(&fm, rows.clone(), &starts), want, "{name}: tokens of {:?} by place", String::from_utf8_lossy(&piece));
            assert_eq!(tokens_by_walk(&fm, rows, sep_first), want, "{name}: tokens of {:?} by walking back", String::from_utf8_lossy(&piece));
        }
        assert_eq!(looked_up, 150, "{name}: too few usable pieces");
    }
}
