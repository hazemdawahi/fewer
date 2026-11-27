//! The stable message key (src/msgkey.rs, rule v1) on the sample files:
//! - a line's key does not depend on how the file is cut into pieces;
//! - where every record is one line (the 20 public samples), records and lines give the same
//!   counts, also when each piece is read as a source of its own (its own log type);
//! - rule v1's keys on the public samples are frozen: checks/msgkey_ref.py gives the same.
//!
//! The samples: mod.rs says where they are.

use super::{files, have_public, read};
use crate::msgkey::{KeyCounts, RULE, count_lines, count_records, line_key, line_keys};

const PUBLIC: &[&str] = &["loghub", "json_converted"];
const ALL: &[&str] = &["loghub", "json_converted", "made_up"];

/// A small random number generator (xorshift), so the cuts are the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Where each line starts.
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut s = vec![0];
    s.extend(bytes.iter().enumerate().filter(|&(i, &b)| b == b'\n' && i + 1 < bytes.len()).map(|(i, _)| i + 1));
    s
}

/// 1 to 12 random cuts at line starts (never at 0): the pieces' (start, end).
fn pieces(r: &mut Rng, bytes: &[u8]) -> Vec<(usize, usize)> {
    let starts = line_starts(bytes);
    if starts.len() < 2 {
        return vec![(0, bytes.len())];
    }
    let mut cuts: Vec<usize> = (0..1 + r.below(12)).map(|_| starts[1 + r.below(starts.len() - 1)]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    let mut a = 0;
    for c in cuts {
        out.push((a, c));
        a = c;
    }
    out.push((a, bytes.len()));
    out
}

#[test]
fn keys_do_not_depend_on_pieces() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    // 20 made-up files, and 20 public ones with FEWER_SAMPLES
    let least = if have_public("keys_do_not_depend_on_pieces: the public samples") { 40 } else { 20 };
    let paths = files(ALL);
    assert!(paths.len() >= least, "samples missing: {}", paths.len());
    for path in paths {
        let bytes = read(&path);
        let whole: Vec<Vec<u8>> = line_keys(&bytes).collect();
        let whole_counts = count_lines(&bytes);
        for _ in 0..5 {
            let mut keys = Vec::new();
            let mut counts = KeyCounts::new();
            for (a, b) in pieces(&mut r, &bytes) {
                keys.extend(line_keys(&bytes[a..b]));
                counts.add_all(&count_lines(&bytes[a..b]));
            }
            assert!(keys == whole, "{path}: the keys of the pieces differ");
            assert!(counts.dump() == whole_counts.dump(), "{path}: the counts of the pieces differ");
        }
    }
}

/// In the public samples every record is one line, so records and lines give the same counts,
/// and so do pieces read as sources of their own (each piece's log type named from its own
/// first 64 KiB).
#[test]
fn records_and_lines_agree_on_the_public_samples() {
    if !have_public("records_and_lines_agree_on_the_public_samples") {
        return;
    }
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    for path in files(PUBLIC) {
        let bytes = read(&path);
        let lines = count_lines(&bytes);
        assert!(count_records(&path, &bytes).dump() == lines.dump(), "{path}: records and lines differ");
        for _ in 0..3 {
            let mut counts = KeyCounts::new();
            for (a, b) in pieces(&mut r, &bytes) {
                counts.add_all(&count_records(&path, &bytes[a..b]));
            }
            assert!(counts.dump() == lines.dump(), "{path}: the records of the pieces differ");
        }
    }
}

/// FNV-1a, 64 bits.
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| (h ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Rule v1's keys on the 20 public samples: the number of distinct keys, and an FNV-1a hash
/// of the dump ("<lines>\t<key>" per key). checks/msgkey_ref.py gives the same dumps. If this
/// test fails, the rule changed: that needs a new rule version (msgkey::RULE), which rebuilds
/// every catalog.
#[test]
fn rule_v1_is_frozen() {
    assert_eq!(RULE, 1);
    if !have_public("rule_v1_is_frozen") {
        return;
    }
    let expected: &[(&str, usize, u64)] = &[
        ("loghub/Android_2k.log", 197, 0x7bfffdc1f9b8da0d),
        ("loghub/Apache_2k.log", 6, 0xd710361b3216c802),
        ("loghub/BGL_2k.log", 152, 0xca278a6cae006704),
        ("loghub/HDFS_2k.log", 17, 0x497640e7352592a6),
        ("loghub/HPC_2k.log", 64, 0x7d461428d078b04b),
        ("loghub/Hadoop_2k.log", 130, 0x690c1a361bb88d11),
        ("loghub/HealthApp_2k.log", 75, 0x38b15671bcd9c566),
        ("loghub/Linux_2k.log", 132, 0x6609f62e7c17a591),
        ("loghub/Mac_2k.log", 476, 0x4826403d4455c0b0),
        ("loghub/OpenSSH_2k.log", 183, 0xf1d610e8391a5a7a),
        ("loghub/OpenStack_2k.log", 44, 0x2cec26ff2161d8f5),
        ("loghub/Proxifier_2k.log", 117, 0xde6e72d236daba15),
        ("loghub/Spark_2k.log", 39, 0x9aea429cfcb9c6c1),
        ("loghub/Thunderbird_2k.log", 185, 0x8194798b86795d79),
        ("loghub/Windows_2k.log", 58, 0x6137e86e5637188a),
        ("loghub/Zookeeper_2k.log", 59, 0x56e79ce1a1609cc7),
        ("json_converted/Android.jsonl", 776, 0x39d916cd920807c6),
        ("json_converted/Apache.jsonl", 33, 0xcb37aec5191d0f37),
        ("json_converted/HDFS.jsonl", 14, 0x677a5ce2e6fdcf44),
        ("json_converted/SSH.jsonl", 183, 0x5241edb58f721337),
    ];
    let mut wrong = Vec::new();
    for &(path, n, hash) in expected {
        let c = count_lines(&read(path));
        if c.len() != n || fnv(&c.dump()) != hash {
            wrong.push(format!("{path}: {} keys, hash {:#018x}", c.len(), fnv(&c.dump())));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// What the rule makes of some sample lines.
#[test]
fn keys_of_sample_lines() {
    let first = |path: &str| line_key(read(path).split_inclusive(|&b| b == b'\n').next().unwrap());
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    if have_public("keys_of_sample_lines: the public samples") {
        assert_eq!(
            s(first("loghub/HDFS_2k.log")),
            "<num> <num> <num> INFO dfs.DataNode$PacketResponder: PacketResponder <num> for block blk_<num> terminating"
        );
        // syslog and ctime stamps are one <time>: one key across months and weekdays
        let linux = count_lines(&read("loghub/Linux_2k.log"));
        assert_eq!(linux.get(b"<time> combo sshd(pam_unix)[<num>]: check pass; user unknown"), 116);
        let apache = count_lines(&read("loghub/Apache_2k.log"));
        assert_eq!(apache.get(b"[<time>] [error] mod_jk child workerEnv in error state <num>"), 539);
    }
    // colored terminal output: the codes are not in the keys
    for k in line_keys(&read("made_up/cargo-test.out")) {
        assert!(!k.contains(&0x1b), "{}", String::from_utf8_lossy(&k));
    }
    // Docker (colors cleaned; JSON inside read as JSON), sessions, CRI
    let docker = count_lines(&read("made_up/3f4e2a1b9c8d-json.log"));
    assert_eq!(docker.get(b"WARN cache miss for key <num>"), 1);
    assert_eq!(docker.get(b"{error} db connect failed"), 1);
    for k in line_keys(&read("made_up/claude-session.jsonl")).skip(1) {
        assert!(k.starts_with(b"{session "), "{}", String::from_utf8_lossy(&k));
    }
    assert_eq!(s(first("made_up/cri-0.log")), "Starting app version <num>");
}
