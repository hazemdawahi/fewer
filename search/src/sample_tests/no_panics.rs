//! No input makes formats.rs, times.rs, records.rs, msgkey.rs or secrets.rs panic: random
//! bytes, and the sample files' lines cut and changed at random places, go through every
//! function, with every kind of source. The random numbers come from a fixed seed, so every
//! run is the same.

use super::{files, have_public, read};
use crate::formats::{Format, Profile, clean_line, detect, level_word, text_level};
use crate::json;
use crate::records::{Records, split};
use crate::times::{self, Clock, Place, Zone};
use crate::{msgkey, secrets};

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

/// Bytes that log lines and time forms are made of, and some that they are not.
const ALPHABET: &[u8] = b"0123456789012345678901234567890123456789 :-/.,[](){}<>\"\\=@#^|*'`+_TZzWEIFADV\tabcdefNovDecMonSunauditpanic\x1b\r\x00\xff\xc3\xa9";

fn random_line(r: &mut Rng) -> Vec<u8> {
    let n = r.below(120);
    (0..n).map(|_| ALPHABET[r.below(ALPHABET.len())]).collect()
}

fn sample_lines() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for f in files(&["loghub", "json_converted", "made_up"]) {
        let bytes = read(&f);
        // a few lines of each file is enough
        out.extend(bytes.split(|&b| b == b'\n').take(25).map(|l| l.to_vec()));
    }
    out
}

/// A copy of `l` cut, or with bytes changed, put in or taken out.
fn mutate(r: &mut Rng, l: &[u8]) -> Vec<u8> {
    let mut v = l.to_vec();
    for _ in 0..1 + r.below(4) {
        match r.below(4) {
            0 if !v.is_empty() => {
                let i = r.below(v.len());
                v[i] = ALPHABET[r.below(ALPHABET.len())];
            }
            1 => {
                let i = r.below(v.len() + 1);
                v.insert(i, ALPHABET[r.below(ALPHABET.len())]);
            }
            2 if !v.is_empty() => {
                let i = r.below(v.len());
                v.remove(i);
            }
            _ => {
                let i = r.below(v.len() + 1);
                v.truncate(i);
            }
        }
    }
    v
}

fn exercise(line: &[u8], profiles: &[Profile], c: &Clock) {
    let cl = clean_line(line);
    for p in profiles {
        let f = p.fields(&cl, c);
        let mut rec = Records::new(p.format);
        rec.push_fields(&f);
        rec.push(Some(&cl[..]), f.partial);
        rec.push(f.body(), false);
    }
    for i in 0..=line.len() {
        let _ = times::read_at(line, i, c);
    }
    for k in 0..times::MAX_TOKENS as u8 {
        let _ = times::line_time(line, Place(k), c);
    }
    let _ = times::number_time(line);
    let _ = times::text_time(line, c);
    let _ = times::pri_end(line, 0);
    let _ = json::parse(line);
    let _ = text_level(line);
    let _ = level_word(line);
    let _ = msgkey::line_key(line);
    let _ = msgkey::text_key(line);
    let (hidden, counts) = secrets::hide(line, &[]);
    if counts.total() == 0 {
        assert!(hidden == line, "{:?}", String::from_utf8_lossy(line));
    }
}

#[test]
fn random_and_changed_lines() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let clocks = [Clock::new(Zone::Fixed(0), 1_795_003_200_000), Clock::new(Zone::Fixed(-720), 0), Clock::local(1_795_003_200_000)];
    let mut profiles: Vec<Profile> = Format::ALL.iter().map(|&f| Profile::new(f)).collect();
    profiles.push(Profile { place: Place(3), ..Profile::new(Format::Access) });
    profiles.push(Profile { place: Place(7), ..Profile::new(Format::Plain) });
    profiles.push(Profile { sep: b'\t', time_col: Some(0), level_col: Some(9), ..Profile::new(Format::Tsv) });
    let samples = sample_lines();
    // the made-up samples give about 180 lines, the public ones about 500 more
    let least = if have_public("random_and_changed_lines: lines of the public samples") { 500 } else { 150 };
    assert!(samples.len() > least, "the samples are missing");
    for round in 0..3000 {
        let c = &clocks[round % clocks.len()];
        let line = if round % 3 == 0 {
            random_line(&mut r)
        } else {
            let k = r.below(samples.len());
            mutate(&mut r, &samples[k])
        };
        exercise(&line, &profiles, c);
    }
    // whole sources: detection and records over random mixes of changed lines
    let names = ["x.log", "a.ips", "b.crash", "tombstone_01", "t.csv", "t.tsv", "slow.log", ""];
    for round in 0..300 {
        let n = r.below(40);
        let mut head = Vec::new();
        let mut ls = Vec::new();
        for _ in 0..n {
            let k = r.below(samples.len());
            let l = mutate(&mut r, &samples[k]);
            head.extend_from_slice(&l);
            head.push(b'\n');
            ls.push(clean_line(&l).into_owned());
        }
        let p = detect(names[round % names.len()], &head);
        let refs: Vec<&[u8]> = ls.iter().map(|l| &l[..]).collect();
        let recs = split(&p, &refs, &clocks[0]);
        // records cover every line once, in order
        let mut next = 0;
        for rec in &recs {
            assert_eq!(rec.first, next);
            assert!(rec.first <= rec.key && rec.key <= rec.last);
            next = rec.last + 1;
        }
        assert_eq!(next, ls.len());
    }
}
