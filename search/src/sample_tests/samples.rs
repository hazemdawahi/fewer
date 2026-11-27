//! The log types on sample files: each log type is
//! recognized; each line of the public samples gets its time, in UTC ms; the made-up traces of
//! eleven languages are one record each, with the right key line.
//!
//! The samples (mod.rs): copies of the 16 Loghub `*_2k` logs and of the first 2,000 lines of
//! each data/json_converted file (in FEWER_SAMPLES), and made-up files for the kinds with no
//! sample on disk (tests/data/made_up). Times are read in UTC (a fixed zone), with the file's
//! time set to 2026-11-18 12:00 UTC, so the results are the same on any computer.

use super::{files, have_public, read};
use crate::formats::{Fields, Format, Level, Msg, Profile, clean_line, detect};
use crate::records::{Records, split};
use crate::times::{Clock, Place, Stamp, Zone, utc_text};

/// 2026-11-18 12:00 UTC.
const FILE_MS: i64 = 1_795_003_200_000;

fn clock() -> Clock {
    Clock::new(Zone::Fixed(0), FILE_MS)
}

/// The clean lines of a file, without their line ends.
fn lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes.split_inclusive(|&b| b == b'\n').map(|l| clean_line(l).into_owned()).collect()
}

/// Every sample and the kind it must be given.
const EXPECTED: &[(&str, &str)] = &[
    ("loghub/Android_2k.log", "plain"),
    ("loghub/Apache_2k.log", "plain"),
    ("loghub/BGL_2k.log", "plain"),
    ("loghub/HDFS_2k.log", "plain"),
    ("loghub/HPC_2k.log", "plain"),
    ("loghub/Hadoop_2k.log", "plain"),
    ("loghub/HealthApp_2k.log", "plain"),
    ("loghub/Linux_2k.log", "syslog"),
    ("loghub/Mac_2k.log", "syslog"),
    ("loghub/OpenSSH_2k.log", "syslog"),
    ("loghub/OpenStack_2k.log", "plain"),
    ("loghub/Proxifier_2k.log", "plain"),
    ("loghub/Spark_2k.log", "plain"),
    ("loghub/Thunderbird_2k.log", "plain"),
    ("loghub/Windows_2k.log", "plain"),
    ("loghub/Zookeeper_2k.log", "plain"),
    ("json_converted/Android.jsonl", "json"),
    ("json_converted/Apache.jsonl", "json"),
    ("json_converted/HDFS.jsonl", "json"),
    ("json_converted/SSH.jsonl", "json"),
    ("made_up/3f4e2a1b9c8d-json.log", "docker"),
    ("made_up/cri-0.log", "cri"),
    ("made_up/nginx-access.log", "access"),
    ("made_up/nginx-error.log", "plain"),
    ("made_up/gha-job.txt", "gha"),
    ("made_up/app-logfmt.log", "logfmt"),
    ("made_up/mysql-slow.log", "mysql-slow"),
    ("made_up/MyApp-2026-11-18-101502.ips", "crash"),
    ("made_up/MyApp_2026-11-18-101502_mac.crash", "crash"),
    ("made_up/tombstone_03", "crash"),
    ("made_up/claude-session.jsonl", "claude-session"),
    ("made_up/rollout-2026-11-18T10-15-02-abc.jsonl", "codex-session"),
    ("made_up/macos-log.ndjson", "macos-log"),
    ("made_up/journal.json", "journal"),
    ("made_up/syslog-5424.log", "syslog"),
    ("made_up/cargo-test.out", "terminal"),
    ("made_up/events.csv", "csv"),
    ("made_up/events.tsv", "tsv"),
    ("made_up/kube-controller.log", "plain"),
    ("made_up/traces.log", "plain"),
];

#[test]
fn every_sample_gets_its_kind() {
    let public = have_public("every_sample_gets_its_kind: the public samples");
    let mut wrong = Vec::new();
    for (path, want) in EXPECTED {
        if !public && !path.starts_with("made_up/") {
            continue;
        }
        let got = detect(path, &read(path)).format.name();
        if got != *want {
            wrong.push(format!("{path}: {got}, expected {want}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    // each log type is on the list
    for f in Format::ALL {
        assert!(EXPECTED.iter().any(|(_, k)| *k == f.name()), "no sample of {}", f.name());
    }
    // and every sample file is on the list
    for name in files(&["loghub", "json_converted", "made_up"]) {
        assert!(EXPECTED.iter().any(|(p, _)| *p == name), "{name} is not in EXPECTED");
    }
}

/// The 16 Loghub logs: every line has a time of its own, and the first line's time is right.
#[test]
fn loghub_lines_get_their_times() {
    if !have_public("loghub_lines_get_their_times") {
        return;
    }
    let c = clock();
    let first: &[(&str, i64, u8)] = &[
        ("Android_2k.log", 1_773_764_018_811, 0),   // 03-17 16:13:38.811, the file's year
        ("Apache_2k.log", 1_133_671_664_000, 0),    // [Sun Dec 04 04:47:44 2005]
        ("BGL_2k.log", 1_117_838_570_000, 1),       // Unix seconds in the second column
        ("HDFS_2k.log", 1_226_262_975_000, 0),      // 081109 203615
        ("HPC_2k.log", 1_077_804_742_000, 4),       // Unix seconds in the fifth column
        ("Hadoop_2k.log", 1_445_191_307_978, 0),    // 2015-10-18 18:01:47,978
        ("HealthApp_2k.log", 1_514_067_329_606, 0), // 20171223-22:15:29:606
        ("Linux_2k.log", 1_781_450_161_000, 0),     // Jun 14 15:16:01, the file's year
        ("Mac_2k.log", 1_782_896_455_000, 0),       // Jul  1 09:00:55
        ("OpenSSH_2k.log", 1_765_349_746_000, 0),   // Dec 10 06:55:46: later than November, so 2025
        ("OpenStack_2k.log", 1_494_892_800_008, 1), // after the file name: 2017-05-16 00:00:00.008
        ("Proxifier_2k.log", 1_793_378_946_000, 0), // [10.30 16:49:06]
        ("Spark_2k.log", 1_497_039_040_000, 0),     // 17/06/09 20:10:40
        ("Thunderbird_2k.log", 1_131_566_461_000, 1),
        ("Windows_2k.log", 1_475_037_030_000, 0), // 2016-09-28 04:30:30,
        ("Zookeeper_2k.log", 1_438_191_704_747, 0), // 2015-07-29 17:41:44,747
    ];
    for &(name, ms, place) in first {
        let bytes = read(&format!("loghub/{name}"));
        let p = detect(name, &bytes);
        assert_eq!(p.place, Place(place), "{name}");
        let ls = lines(&bytes);
        assert_eq!(ls.len(), 2000, "{name}");
        let mut untimed = 0;
        for (i, l) in ls.iter().enumerate() {
            let f = p.fields(l, &c);
            if i == 0 {
                assert_eq!(f.time, Some(Stamp::Utc(ms)), "{name}: {}", f.time.and_then(|t| t.utc_ms()).map_or("none".into(), utc_text));
            }
            if f.time.is_none() {
                untimed += 1;
            }
        }
        assert_eq!(untimed, 0, "{name}: lines without a time");
    }
}

#[test]
fn loghub_levels() {
    if !have_public("loghub_levels") {
        return;
    }
    let c = clock();
    // (file, lines with a level, the level of the first line)
    let cases: &[(&str, usize, Level)] = &[
        ("Android_2k.log", 2000, Level::Debug), // one-letter levels after the pid columns
        ("Apache_2k.log", 2000, Level::Info),   // [notice]
        ("BGL_2k.log", 2000, Level::Info),      // RAS KERNEL INFO
        ("HDFS_2k.log", 2000, Level::Info),
        ("Hadoop_2k.log", 2000, Level::Info),
        ("OpenStack_2k.log", 2000, Level::Info),
        ("Spark_2k.log", 2000, Level::Info),
        ("Windows_2k.log", 2000, Level::Info),
        ("Zookeeper_2k.log", 2000, Level::Info),
        ("HealthApp_2k.log", 0, Level::None), // no level written
    ];
    for &(name, n, l0) in cases {
        let bytes = read(&format!("loghub/{name}"));
        let p = detect(name, &bytes);
        let fs: Vec<Level> = lines(&bytes).iter().map(|l| p.fields(l, &c).level).collect();
        assert_eq!(fs.iter().filter(|l| **l != Level::None).count(), n, "{name}");
        assert_eq!(fs[0], l0, "{name}");
    }
    let bytes = read("loghub/Apache_2k.log");
    let p = detect("Apache_2k.log", &bytes);
    assert_eq!(p.fields(b"[Sun Dec 04 04:47:44 2005] [error] mod_jk child workerEnv in error state 6", &c).level, Level::Error);
}

/// data/json_converted: every line's message, time and (when written) level.
#[test]
fn json_converted_fields() {
    if !have_public("json_converted_fields") {
        return;
    }
    let c = clock();
    let cases: &[(&str, i64, bool)] = &[
        ("Android.jsonl", 1_765_999_896_263, true), // "12-17 19:31:36.263": December, so 2025
        ("Apache.jsonl", 1_118_297_224_000, true),  // "Thu Jun 09 06:07:04 2005"
        ("HDFS.jsonl", 1_226_262_918_000, true),    // "081109 203518"
        ("SSH.jsonl", 1_765_349_746_000, false),    // "Dec 10 06:55:46", no level field
    ];
    for &(name, ms, levels) in cases {
        let bytes = read(&format!("json_converted/{name}"));
        let p = detect(name, &bytes);
        for (i, l) in lines(&bytes).iter().enumerate() {
            let f = p.fields(l, &c);
            assert!(matches!(f.msg, Msg::Text(_)), "{name}:{}", i + 1);
            assert!(f.time.is_some(), "{name}:{}", i + 1);
            assert_eq!(f.level_text.is_some(), levels, "{name}:{}", i + 1);
            if levels {
                assert_ne!(f.level, Level::None, "{name}:{}", i + 1);
            }
            if i == 0 {
                assert_eq!(f.time, Some(Stamp::Utc(ms)), "{name}");
            }
        }
    }
}

/// The fields of every line of a made-up sample.
fn made_up(name: &str) -> (Profile, Vec<Vec<u8>>) {
    let bytes = read(&format!("made_up/{name}"));
    (detect(name, &bytes), lines(&bytes))
}

fn all_fields<'a>(p: &Profile, ls: &'a [Vec<u8>], c: &Clock) -> Vec<Fields<'a>> {
    ls.iter().map(|l| p.fields(l, c)).collect()
}

fn text(f: &Fields) -> String {
    match &f.msg {
        Msg::Text(t) => String::from_utf8_lossy(t).into_owned(),
        other => format!("{other:?}"),
    }
}

fn utc(f: &Fields) -> Option<i64> {
    f.time.and_then(|t| t.utc_ms())
}

#[test]
fn made_up_container_and_ci_logs() {
    let c = clock();
    let (p, ls) = made_up("3f4e2a1b9c8d-json.log");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((text(&fs[0]).as_str(), utc(&fs[0])), ("Server starting on port 8080", Some(1_794_996_902_123)));
    assert_eq!((text(&fs[1]).as_str(), fs[1].level, fs[1].stderr), ("WARN cache miss for key user:42", Level::Warn, true));
    assert_eq!((text(&fs[2]).as_str(), fs[2].level), ("db connect failed", Level::Error));

    let (p, ls) = made_up("cri-0.log");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((text(&fs[0]).as_str(), utc(&fs[0])), ("Starting app version 1.4.2", Some(1_794_996_902_123)));
    assert!(fs[2].partial && !fs[3].partial);
    assert_eq!((text(&fs[4]).as_str(), fs[4].level), ("request served", Level::Info));

    let (p, ls) = made_up("nginx-access.log");
    let fs = all_fields(&p, &ls, &c);
    let levels: Vec<Level> = fs.iter().map(|f| f.level).collect();
    assert_eq!(levels, [Level::Info, Level::Warn, Level::Error, Level::Info]);
    assert_eq!(utc(&fs[0]), Some(1_794_996_902_000));

    let (p, ls) = made_up("nginx-error.log");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((fs[2].level, utc(&fs[2])), (Level::Error, Some(1_794_996_904_000)));

    let (p, ls) = made_up("gha-job.txt");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((text(&fs[0]).as_str(), utc(&fs[0])), ("##[group]Run actions/checkout@v4", Some(1_794_996_902_123)));
    assert_eq!(text(&fs[4]), "npm test"); // escape codes out
    assert_eq!((fs[5].level, fs[6].level), (Level::Error, Level::Error));

    let (p, ls) = made_up("kube-controller.log");
    let fs = all_fields(&p, &ls, &c);
    let levels: Vec<Level> = fs.iter().map(|f| f.level).collect();
    assert_eq!(levels, [Level::Info, Level::Warn, Level::Error]);
    assert_eq!(utc(&fs[0]), Some(1_794_996_902_123));
}

#[test]
fn made_up_app_and_database_logs() {
    let c = clock();
    let (p, ls) = made_up("app-logfmt.log");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((text(&fs[0]).as_str(), fs[0].level, utc(&fs[0])), ("server started", Level::Info, Some(1_794_996_902_000)));
    assert_eq!((text(&fs[2]).as_str(), fs[2].level, utc(&fs[2])), ("payment failed", Level::Error, Some(1_794_996_904_500)));

    let (p, ls) = made_up("mysql-slow.log");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!(utc(&fs[3]), Some(1_794_996_902_123)); // # Time:
    assert_eq!(utc(&fs[7]), Some(1_795_003_302_000)); // SET timestamp=
    let refs: Vec<&[u8]> = ls.iter().map(|l| &l[..]).collect();
    let r: Vec<(usize, usize, usize)> = split(&p, &refs, &c).iter().map(|r| (r.first, r.last, r.key)).collect();
    assert_eq!(r, vec![(0, 2, 0), (3, 10, 8), (11, 14, 14)]);

    let (p, ls) = made_up("events.csv");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((fs[1].level, utc(&fs[1])), (Level::Info, Some(1_794_996_902_000)));
    assert_eq!(fs[2].level, Level::Error);
    let (p, ls) = made_up("events.tsv");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((fs[2].level, fs[2].level_text.as_deref(), utc(&fs[2])), (Level::Error, Some("ERROR"), Some(1_794_996_903_000)));

    let (p, ls) = made_up("cargo-test.out");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!(text(&fs[3]), "Downloading 100%"); // a redraw at its final state
    assert_eq!(text(&fs[6]), "test b ... FAILED");
}

#[test]
fn made_up_os_logs_sessions_and_crashes() {
    let c = clock();
    let (p, ls) = made_up("macos-log.ndjson");
    let fs = all_fields(&p, &ls, &c);
    let levels: Vec<Level> = fs.iter().map(|f| f.level).collect();
    assert_eq!(levels, [Level::Info, Level::Error, Level::Error, Level::Debug]);
    assert_eq!((text(&fs[0]).as_str(), utc(&fs[0])), ("Network reachability changed", Some(1_795_025_702_123)));

    let (p, ls) = made_up("journal.json");
    let fs = all_fields(&p, &ls, &c);
    let levels: Vec<Level> = fs.iter().map(|f| f.level).collect();
    assert_eq!(levels, [Level::Info, Level::Error, Level::Warn]);
    assert_eq!(utc(&fs[0]), Some(1_795_003_302_123));

    let (p, ls) = made_up("syslog-5424.log");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!((fs[0].level, utc(&fs[0])), (Level::Error, Some(1_794_996_902_003))); // <34>: severity 2
    assert_eq!(utc(&fs[2]), Some(1_794_993_304_500)); // +01:00

    let (p, ls) = made_up("claude-session.jsonl");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!(fs[0].msg, Msg::Session(vec!["summary".into()]));
    assert_eq!(fs[2].msg, Msg::Session(vec!["assistant".into(), "assistant".into(), "text".into(), "Bash".into()]));
    assert_eq!(fs[3].msg, Msg::Session(vec!["user".into(), "user".into(), "tool_result".into()]));
    assert_eq!(utc(&fs[1]), Some(1_794_996_902_123));

    let (p, ls) = made_up("rollout-2026-11-18T10-15-02-abc.jsonl");
    let fs = all_fields(&p, &ls, &c);
    assert_eq!(fs[1].msg, Msg::Session(vec!["response_item".into(), "message".into(), "user".into(), "input_text".into()]));

    // a crash report is one error record, its time from the report
    for (name, ms, key) in [("MyApp-2026-11-18-101502.ips", 1_794_993_302_000, 0), ("MyApp_2026-11-18-101502_mac.crash", 1_794_993_302_123, 0), ("tombstone_03", 1_794_993_302_123, 7)] {
        let (p, ls) = made_up(name);
        let refs: Vec<&[u8]> = ls.iter().map(|l| &l[..]).collect();
        let r = split(&p, &refs, &c);
        assert_eq!(r.len(), 1, "{name}");
        assert_eq!((r[0].first, r[0].last, r[0].key, r[0].trace), (0, ls.len() - 1, key, true), "{name}");
        let fs = all_fields(&p, &ls, &c);
        assert!(fs.iter().all(|f| f.level == Level::Error), "{name}");
        assert_eq!(fs.iter().find_map(utc), Some(ms), "{name}");
    }
    let (p, ls) = made_up("MyApp-2026-11-18-101502.ips");
    assert_eq!(text(&p.fields(&ls[0], &c)), "MyApp");
}

/// traces.log: a trace of each of eleven languages, between ordinary log lines. Each trace is
/// one record, an error record, keyed by the line that names the error.
#[test]
fn eleven_languages_one_record_each() {
    let c = clock();
    let (p, ls) = made_up("traces.log");
    let refs: Vec<&[u8]> = ls.iter().map(|l| &l[..]).collect();
    let recs = split(&p, &refs, &c);
    let expect: &[(&str, &str)] = &[
        ("java", "Exception in thread \"main\" java.lang.IllegalStateException: boom"),
        ("python", "ValueError: bad value"),
        ("go", "panic: runtime error: invalid memory address or nil pointer dereference"),
        ("node", "Error: boom"),
        ("rust", "called `Option::unwrap()` on a `None` value"),
        ("dotnet", "Unhandled exception. System.InvalidOperationException: Sequence contains no elements"),
        ("ruby", "/app/lib/foo.rb:10:in `bar': undefined method `baz' for nil:NilClass (NoMethodError)"),
        ("php", "PHP Fatal error:  Uncaught Exception: boom in /var/www/index.php:3"),
        ("dart", "Exception: boom"),
        ("chrome", "Uncaught TypeError: Cannot read properties of undefined (reading 'x')"),
        ("firefox", "TypeError: x is undefined"),
    ];
    // records alternate: the "step N: <language>" line, then its trace; the last line ends it
    assert_eq!(recs.len(), expect.len() * 2 + 1, "{recs:#?}");
    for (i, (lang, key)) in expect.iter().enumerate() {
        let step = recs[2 * i];
        let trace = recs[2 * i + 1];
        assert_eq!(step.first, step.last, "{lang}");
        assert!(String::from_utf8_lossy(&ls[step.first]).ends_with(lang), "{lang}");
        assert!(!step.trace, "{lang}");
        assert!(trace.trace, "{lang}: not an error record");
        assert_eq!(String::from_utf8_lossy(&ls[trace.key]), *key, "{lang}");
        // the trace runs up to the next step line
        let next = recs[2 * i + 2].first;
        assert_eq!(trace.last + 1, next, "{lang}");
    }
    // line by line, the same answer as split
    let mut r = Records::new(p.format);
    let starts: Vec<usize> = ls.iter().enumerate().filter(|(_, l)| r.push_fields(&p.fields(l, &c)).start).map(|(i, _)| i).collect();
    assert_eq!(starts, recs.iter().map(|r| r.first).collect::<Vec<_>>());
}

/// The index-pass loop of catalog.rs, run on a made-up log: every line gets a
/// time (its own, else the previous line's, else its arrival time), and each record is counted
/// once, under its key line.
#[test]
fn catalog_style_pass() {
    use crate::records;
    use crate::times;
    let bytes = read("made_up/traces.log");
    let source_name = "traces.log";
    let first_64_kib = &bytes[..bytes.len().min(64 * 1024)];
    let profile = crate::formats::detect(source_name, first_64_kib);
    let clock = times::Clock::new(times::Zone::Fixed(0), FILE_MS); // Clock::local(file_ms) in the product
    let marks: Option<Vec<(u64, u64)>> = Some(vec![(0, 0), (40, 2_000)]);
    let run_start_ms = FILE_MS;
    let mut recs = records::Records::new(profile.format);
    let mut carry = times::Carry::new();
    let mut arrivals = marks.as_deref().map(|m| times::Arrivals::new(run_start_ms, m));
    let mut counted: Vec<(usize, usize)> = Vec::new(); // (first line, key line) of each record
    let mut timed = 0;
    for (n, raw) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
        let clean = clean_line(raw);
        let f = profile.fields(&clean, &clock);
        let time = carry.next(f.time, arrivals.as_mut().map(|a| a.at(n as u64)));
        if time.is_some() {
            timed += 1;
        }
        let step = recs.push_fields(&f);
        if step.start {
            counted.push((n, n));
        }
        if step.key {
            counted.last_mut().unwrap().1 = n;
        }
    }
    let n_lines = bytes.split_inclusive(|&b| b == b'\n').count();
    assert_eq!(timed, n_lines);
    let refs = lines(&bytes);
    let r: Vec<&[u8]> = refs.iter().map(|l| &l[..]).collect();
    let by_split: Vec<(usize, usize)> = split(&profile, &r, &clock).iter().map(|x| (x.first, x.key)).collect();
    assert_eq!(counted, by_split);
    assert_eq!(counted.len(), 23);
}
