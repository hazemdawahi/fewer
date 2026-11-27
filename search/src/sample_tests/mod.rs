//! Tests on sample files, for formats.rs, times.rs, records.rs, msgkey.rs, secrets.rs and
//! fm.rs. They were built as the tests/ folder of agent 2's crate, where these modules were
//! made, and each file here is one of those, under the same name:
//! - samples.rs: each log type is recognized; lines get their times and levels; traces of
//!   eleven languages are one record each;
//! - no_panics.rs: random and changed lines never make a module panic;
//! - keys.rs: the stable message key on samples, and rule v1 frozen (5.1-5.2, 5.7);
//! - secrets.rs: secret hiding on random text and on samples (7.5);
//! - fm_samples.rs: the FM index on real logs and their token texts (6.1, 6.3).
//!
//! Two kinds of samples:
//! - made-up ones, written for the kinds with no public sample (Docker, CRI, nginx, GitHub
//!   Actions, logfmt, MySQL slow log, crash reports, agents' sessions, OS log exports, csv,
//!   tsv, colored terminal output, traces of eleven languages): tests/data/made_up, in the
//!   crate. Their content is invented. The tests that read them always run;
//! - copies of public logs, which stay out of the repository: loghub/ (the 16 Loghub `*_2k`
//!   logs), json_converted/ (the first 2,000 lines of four JSON logs), keys/ (the first 200
//!   lines of a GH Archive hour) and fm/ (two windows of the full Loghub Mac and Android
//!   logs, with many bytes >= 0x80), in the folder that FEWER_SAMPLES names.
//!   Without FEWER_SAMPLES, the tests and the parts of tests
//!   that read them are skipped, and say so on stderr (`cargo test -- --nocapture`).

mod fm_samples;
mod keys;
mod no_panics;
mod samples;
mod secrets;

use std::path::PathBuf;

/// The folder of the copies of public logs (FEWER_SAMPLES), when it is set.
fn public() -> Option<PathBuf> {
    std::env::var_os("FEWER_SAMPLES").filter(|d| !d.is_empty()).map(PathBuf::from)
}

/// True when the public samples can be read; else says on stderr that `what` was skipped.
fn have_public(what: &str) -> bool {
    if public().is_some() {
        return true;
    }
    eprintln!("{what}: skipped (set FEWER_SAMPLES to the folder of public samples to run it)");
    false
}

/// Where a sample is: "made_up/NAME" in tests/data, any other "FOLDER/NAME" in FEWER_SAMPLES.
fn path(rel: &str) -> PathBuf {
    if rel.starts_with("made_up/") {
        return PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data")).join(rel);
    }
    match public() {
        Some(d) => d.join(rel),
        None => panic!("{rel} is a public sample: set FEWER_SAMPLES"),
    }
}

/// A sample's bytes ("made_up/traces.log", "loghub/HDFS_2k.log"). A public one needs
/// FEWER_SAMPLES: check `have_public` first.
fn read(rel: &str) -> Vec<u8> {
    let p = path(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("cannot read sample {}: {e}", p.display()))
}

/// The files of sample folders ("made_up", "loghub", ...) as "FOLDER/NAME", each folder's
/// sorted by name; public folders only while FEWER_SAMPLES is set. Hidden files (.DS_Store)
/// are passed over.
fn files(folders: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for sub in folders {
        if *sub != "made_up" && public().is_none() {
            continue;
        }
        let dir = path(&format!("{sub}/"));
        let rd = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read the sample folder {}: {e}", dir.display()));
        let mut names: Vec<String> = rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
        names.sort();
        out.extend(names.into_iter().map(|n| format!("{sub}/{n}")));
    }
    out
}
