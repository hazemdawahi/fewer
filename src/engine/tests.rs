//! Unit tests of the engine adapter: round trips, damage, hostile payloads, limits.
use super::*;

fn rt(d: &[u8]) {
    for t in [1, 3] {
        let p = pack(d, t);
        assert!(p.len() <= 2 * d.len() + (1 << 20), "payload too big for {} bytes", d.len());
        let back = unpack(&p, t).unwrap_or_else(|e| panic!("unpack failed for {} bytes: {e}", d.len()));
        assert!(back == d, "round trip failed for {} bytes", d.len());
    }
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn varints(v: &[u64]) -> Vec<u8> {
    let mut o = Vec::new();
    for &x in v {
        put_varint(&mut o, x);
    }
    o
}

/// The kinds of the blocks of a payload (and the payload's table count).
fn block_kinds(p: &[u8]) -> (u64, Vec<u8>) {
    let mut r = Rd::new(p);
    r.byte().unwrap();
    r.varint().unwrap();
    let nb = r.varint().unwrap();
    let nt = r.varint().unwrap();
    for _ in 0..nt {
        r.frame().unwrap();
    }
    let mut kinds = Vec::new();
    for _ in 0..nb {
        r.varint().unwrap();
        let k = r.byte().unwrap();
        kinds.push(k);
        let nf = if k == KIND_LINES {
            r.byte().unwrap();
            r.varint().unwrap();
            r.varint().unwrap()
        } else {
            1
        };
        for _ in 0..nf {
            r.frame().unwrap();
        }
        r.take(4).unwrap();
    }
    assert!(r.done());
    (nt, kinds)
}

fn log_lines(n: u32) -> Vec<u8> {
    let mut d = Vec::new();
    for i in 0..n {
        d.extend_from_slice(
            format!(
                "2026-11-15 10:{:02}:{:02},{:03} INFO worker-{} took {} ms id={:08x} from 10.0.{}.{}\n",
                i / 60 % 60,
                i % 60,
                i % 1000,
                i % 7,
                i * 13 % 1000,
                i.wrapping_mul(2654435761),
                i % 3,
                i % 250
            )
            .as_bytes(),
        );
    }
    d
}

#[test]
fn edge_cases_round_trip() {
    rt(b"");
    rt(b"x");
    rt(b"\n");
    rt(b"\n\n\n");
    rt(b"no final newline 12:00:01");
    rt(b"a\r\nb 1\r\n\r\n");
    rt(b"nul \0 inside 12\nand 13\n\0\0\n");
    rt(b"t 99:59:59 x\nt 00:00:00.000001 x\nt 1:2:3\n9:05:03,5 y\n12:34:56.1234567 z\n");
    rt(b"ip 10.0.0.1 1.2.3.4.5 256.1.1.1 01.2.3.4 9.9.9.9\n");
    rt(b"u 6f1c2b9e-1d2a-4c3b-9e8f-0a1b2c3d4e5f 6F1C2B9E-1D2A-4C3B-9E8F-0A1B2C3D4E5F 6f1C2b9e-1d2a-4c3b-9e8f-0a1b2c3d4e5f\n");
    rt(b"h deadbeef01 DEADBEEF01 deadbeef012 0123456789abcdefAB 12345678901234567890 007 00 0\n");
    rt(b"  leading  and trailing  \n \n");
    rt(b"Jan Feb Mon Tuesday Junk 3 Mayday [Jan] -1234567890123456 x-123456789012345678\n");
    rt(&log_lines(250_000));
    let mut bin = Vec::new();
    let mut x = 99u64;
    for _ in 0..300_000 {
        bin.push(xorshift(&mut x) as u8);
    }
    rt(&bin);
    let mut long = vec![b'x'; 3_000_000];
    long.extend_from_slice(b" 12:00:00\nshort 1\n");
    rt(&long);
    let mut mixed = log_lines(3000);
    mixed.extend_from_slice(&bin[..100_000]);
    mixed.extend_from_slice(&log_lines(5000));
    rt(&mixed);
}

/// More than ONE_MAX lines: several blocks of lines sharing the payload's tables.
#[test]
fn many_blocks_round_trip() {
    let d = log_lines(450_000);
    let p = pack(&d, 4);
    let (nt, kinds) = block_kinds(&p);
    // (the tables, and the table bank of the blocks' column streams)
    assert_eq!(nt, eng::TABLES.len() as u64 + 1);
    assert!(kinds.len() >= 5 && kinds.iter().all(|&k| k == KIND_LINES), "{kinds:?}");
    assert!(unpack(&p, 4).unwrap() == d);
    assert!(unpack(&p, 1).unwrap() == d);
    // the same bytes with any number of threads
    assert!(pack(&d, 1) == p);
}

/// A payload of several lines blocks has a table bank (one more frame after its tables): it
/// opens exactly on 1 and 3 threads and is the same on 1 and 4; damage to the bank or anywhere
/// gives an error or the exact text, never a crash; without its bank, with an empty bank or
/// with one frame too many, the payload is refused.
#[test]
fn table_bank_round_trip_and_damage() {
    let d = log_lines(450_000);
    let p = pack(&d, 4);
    assert!(unpack(&p, 1).unwrap() == d && unpack(&p, 3).unwrap() == d);
    assert!(pack(&d, 1) == p);
    // where the table count and the bank are
    let mut r = Rd::new(&p);
    r.byte().unwrap();
    r.varint().unwrap();
    r.varint().unwrap();
    let nt_at = r.p;
    assert_eq!(r.varint().unwrap(), eng::TABLES.len() as u64 + 1, "no bank");
    for _ in 0..eng::TABLES.len() {
        r.frame().unwrap();
    }
    let at = r.p;
    let bank = r.frame().unwrap().to_vec();
    let end = r.p;
    assert!(eng::Bank::read(&bank).is_some());
    let mut x = 99u64;
    for k in 0..3000 {
        let mut g = p.clone();
        // two of three in or next to the bank, the rest anywhere
        let i = if k % 3 < 2 { at + (xorshift(&mut x) as usize) % (end - at + 2) } else { (xorshift(&mut x) as usize) % g.len() };
        match k % 4 {
            0 => g[i] ^= 1 << (k % 8),
            1 => g[i] = (x >> 20) as u8,
            2 => {
                g.remove(i);
            }
            _ => g.truncate(i),
        }
        if let Ok(back) = unpack(&g, 2) {
            assert!(back == d, "damage gave wrong output");
        }
    }
    // without the bank: its frames name tables that are not there
    let mut g = p[..nt_at].to_vec();
    put_varint(&mut g, eng::TABLES.len() as u64);
    g.extend_from_slice(&p[nt_at + 1..at]);
    g.extend_from_slice(&p[end..]);
    assert!(unpack(&g, 1).is_err());
    // an empty bank (no entry)
    let mut g = p[..at].to_vec();
    g.extend_from_slice(&[1, 0]);
    g.extend_from_slice(&p[end..]);
    assert!(unpack(&g, 1).is_err());
    // one frame too many
    let mut g = p[..nt_at].to_vec();
    put_varint(&mut g, eng::TABLES.len() as u64 + 2);
    g.extend_from_slice(&p[nt_at + 1..end]);
    g.extend_from_slice(&p[at..end]);
    g.extend_from_slice(&p[end..]);
    assert!(unpack(&g, 1).is_err());
}

/// Lines that are not a log (prose, tables, documents: few lines share a shape) are packed as
/// plain blocks without running the log engine; logs, JSON logs included, go to the engine.
#[test]
fn non_log_lines_go_plain() {
    const WORDS: [&str; 16] = ["the", "river", "quiet", "stone", "under", "light", "morning", "was", "and", "of", "to", "house", "walked", "long", "road", "where"];
    let mut x = 9u64;
    let mut prose = Vec::new();
    for _ in 0..4000 {
        for k in 0..4 + xorshift(&mut x) % 9 {
            if k > 0 {
                prose.push(b' ');
            }
            prose.extend_from_slice(WORDS[(xorshift(&mut x) % 16) as usize].as_bytes());
        }
        prose.extend_from_slice(b".\n");
    }
    let p = pack(&prose, 2);
    let (_, kinds) = block_kinds(&p);
    assert!(!kinds.is_empty() && kinds.iter().all(|&k| k == KIND_PLAIN), "{kinds:?}");
    assert!(unpack(&p, 2).unwrap() == prose);
    let mut json = Vec::new();
    for i in 0..4000u32 {
        json.extend_from_slice(format!("{{\"time\":\"2026-11-15 10:{:02}:{:02}\",\"level\":\"INFO\",\"msg\":\"worker {} took {} ms\"}}\n", i / 60 % 60, i % 60, i % 7, i * 13 % 1000).as_bytes());
    }
    for d in [log_lines(4000), json] {
        let p = pack(&d, 2);
        let (_, kinds) = block_kinds(&p);
        assert!(kinds == [KIND_LINES], "{kinds:?}");
        assert!(unpack(&p, 2).unwrap() == d);
    }
}

/// Opening hands the blocks out in order, one at a time (on 1 or several threads), and stops
/// when the sink says so.
#[test]
fn unpack_streams_blocks_in_order() {
    let d = log_lines(450_000);
    let p = pack(&d, 4);
    let (_, kinds) = block_kinds(&p);
    for t in [1, 3, 8] {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let raw = unpack_to(&p, t, &mut |b| {
            parts.push(b.to_vec());
            Ok(true)
        })
        .unwrap();
        assert_eq!(raw, d.len() as u64);
        assert_eq!(parts.len(), kinds.len(), "{t} threads");
        assert!(parts.concat() == d, "{t} threads");
        let mut seen = 0;
        unpack_to(&p, t, &mut |_| {
            seen += 1;
            Ok(seen < 2)
        })
        .unwrap();
        assert_eq!(seen, 2, "{t} threads: did not stop");
        let e = unpack_to(&p, t, &mut |_| Err("sink".into()));
        assert!(e.is_err());
    }
}

/// A search's filter (unpack_blocks): a block of lines left unopened (None) is whole lines
/// after a line end (or after `prev_nl` for the first); every other block comes as its text,
/// in order; a filter that says yes changes nothing.
#[test]
fn unpack_blocks_skips_only_whole_lines() {
    let d = log_lines(450_000);
    let p = pack(&d, 4);
    let no: Filter = &|_, _| false;
    let yes: Filter = &|_, _| true;
    for t in [1, 3, 8] {
        for (may, prev) in [(no, true), (no, false), (yes, true)] {
            let mut parts: Vec<(Option<Vec<u8>>, usize)> = Vec::new();
            let raw = unpack_blocks(&p, t, Some(may), prev, &mut |b, n| {
                parts.push((b.map(|x| x.to_vec()), n));
                Ok(true)
            })
            .unwrap();
            assert_eq!(raw, d.len() as u64);
            let mut at = 0;
            for (b, n) in &parts {
                match b {
                    Some(b) => assert!(b[..] == d[at..at + n], "{t} threads"),
                    None => assert!(d[at + n - 1] == b'\n' && if at == 0 { prev } else { d[at - 1] == b'\n' }),
                }
                at += n;
            }
            assert_eq!(at, d.len());
            let skipped = parts.iter().filter(|x| x.0.is_none()).count();
            assert!(if prev { (skipped > 0) == std::ptr::eq(may, no) } else { parts[0].0.is_some() }, "{t} threads: {skipped} skipped");
        }
    }
}

#[test]
fn random_lines_round_trip() {
    let mut x = 3u64;
    for _ in 0..60 {
        let mut d = Vec::new();
        for _ in 0..xorshift(&mut x) % 300 {
            for _ in 0..xorshift(&mut x) % 40 {
                d.push(b"7x .:-a0Jf"[(xorshift(&mut x) % 10) as usize]);
            }
            d.push(b'\n');
        }
        rt(&d);
    }
}

#[test]
fn damaged_payloads_never_panic() {
    let mut d = Vec::new();
    for i in 0..3000u32 {
        d.extend_from_slice(
            format!("Jan 1 10:00:{:02} host sshd[{}]: Accepted password for user{} from 10.0.{}.{} port {}\n", i % 60, 1000 + i % 9, i % 5, i % 3, i % 250, 40000 + i)
                .as_bytes(),
        );
    }
    let p = pack(&d, 2);
    let mut x = 7u64;
    for k in 0..3000 {
        let mut g = p.clone();
        let i = (xorshift(&mut x) as usize) % g.len();
        match k % 5 {
            0 => g[i] ^= 1 << (k % 8),
            1 => g.truncate(i),
            2 => g[i] = (x >> 20) as u8,
            3 => {
                g.remove(i);
            }
            _ => {
                let v = (x >> 24) as u8;
                for b in g[i..].iter_mut().take(8) {
                    *b = v;
                }
            }
        }
        if let Ok(back) = unpack(&g, 2) {
            assert!(back == d, "damage gave wrong output");
        }
    }
}

/// Lines the engine is not made for go to plain blocks, fast and in little memory.
#[test]
fn wide_lines_stay_cheap() {
    let t = std::time::Instant::now();
    // 40,000 numbers in one line; 8 MiB of short values without a line end; a long run of
    // hex letters; 800 different lines of 2,000 numbers
    let wide: Vec<u8> = (0..40_000).map(|i| format!("{i},")).collect::<String>().into_bytes();
    rt(&wide);
    rt(&b"ab 12 ".repeat(1_400_000));
    let mut h = b"abcdef".repeat(80_000);
    h.extend_from_slice(b"x1 end\n");
    rt(&h);
    let mut x = 5u64;
    let mut json = Vec::new();
    for _ in 0..800 {
        let user: String = (0..8).map(|_| (b'a' + (xorshift(&mut x) % 26) as u8) as char).collect();
        let nums: Vec<String> = (0..2000).map(|_| (xorshift(&mut x) % 1000).to_string()).collect();
        json.extend_from_slice(format!("{{\"level\":\"info\",\"user\":\"{user}\",\"latencies_ms\":[{}]}}\n", nums.join(",")).as_bytes());
    }
    rt(&json);
    // 3,000 different lines of 100 numbers: blocks are cut by their pattern work
    let mut mid = Vec::new();
    for _ in 0..3000 {
        let user: String = (0..8).map(|_| (b'a' + (xorshift(&mut x) % 26) as u8) as char).collect();
        let nums: Vec<String> = (0..100).map(|_| (xorshift(&mut x) % 1000).to_string()).collect();
        mid.extend_from_slice(format!("user {user} values {}\n", nums.join(" ")).as_bytes());
    }
    rt(&mid);
    assert!(t.elapsed().as_secs() < 60, "too slow: {:?}", t.elapsed());
}

#[test]
fn line_rules() {
    assert!(line_ok(&[b'a'; 200]));
    assert!(line_ok(&b"x 12 ".repeat(100)));
    assert!(!line_ok(&vec![b'x'; LINE_MAX + 1]));
    assert!(!line_ok(&b"1 ".repeat(1025)));
    assert!(!line_ok(&b"abcdef".repeat(50)));
    assert!(line_ok(&b"abcdeg".repeat(50)));
    let (_, kinds) = block_kinds(&pack(&b"1 ".repeat(5000), 1));
    assert_eq!(kinds, [KIND_PLAIN]);
}

/// Already-compressed data is stored as it is (fast), repeated data is still coded.
#[test]
fn random_data_is_stored() {
    let mut x = 9u64;
    let r: Vec<u8> = (0..3 << 20).map(|_| xorshift(&mut x) as u8).collect();
    assert!(looks_random(&r));
    let p = pack(&r, 2);
    assert!(p.len() < r.len() + 1000);
    assert!(unpack(&p, 2).unwrap() == r);
    assert!(!looks_random(&b"Jan 1 10:00:00 host sshd[12]: Accepted password for x\n".repeat(5000)));
    let mut rep = r[..4096].to_vec();
    for _ in 0..6 {
        let c = rep.clone();
        rep.extend_from_slice(&c);
    }
    assert!(!looks_random(&rep));
    assert!(pack(&rep, 1).len() < rep.len() / 10);
}

/// A payload with one plain block, built by hand (as the archive format writes it).
fn plain_payload(text: &[u8], frame: &[u8]) -> Vec<u8> {
    let mut p = vec![ENGINE_ID];
    put_varint(&mut p, text.len() as u64);
    put_varint(&mut p, 1);
    put_varint(&mut p, 0);
    put_varint(&mut p, text.len() as u64);
    p.push(KIND_PLAIN);
    put_varint(&mut p, frame.len() as u64);
    p.extend_from_slice(frame);
    p.extend_from_slice(&crc32(text).to_le_bytes());
    p
}

#[test]
fn hand_built_payloads() {
    assert_eq!(unpack(&[ENGINE_ID, 0, 0, 0], 1).unwrap(), b"");
    assert_eq!(pack(b"", 1), [ENGINE_ID, 0, 0, 0]);
    let text = b"hello 1\nworld 2\n";
    let mut stored = vec![0u8];
    put_varint(&mut stored, text.len() as u64);
    stored.extend_from_slice(text);
    assert_eq!(unpack(&plain_payload(text, &stored), 1).unwrap(), text);
    // a wrong CRC, a frame declaring more than the block, an unknown kind, bytes after the end
    let mut p = plain_payload(text, &stored);
    let n = p.len();
    p[n - 1] ^= 1;
    assert!(unpack(&p, 1).is_err());
    let big = eng::code(&[b'a'; 4096]);
    assert!(unpack(&plain_payload(text, &big), 1).is_err());
    let mut p = plain_payload(text, &stored);
    p[5] = 7;
    assert!(unpack(&p, 1).is_err());
    let mut p = plain_payload(text, &stored);
    p.push(0);
    assert!(unpack(&p, 1).is_err());
    assert!(unpack(&[1, 0, 0, 0], 1).unwrap_err().contains("made by an older fewer-pack"));
    // engine 5 (format 1, fewer-pack 1.1.0) is not read: format 2 replaced it
    assert!(unpack(&[5, 0, 0, 0], 1).unwrap_err().contains("made by an older fewer-pack (archive engine 5)"));
    assert!(unpack(&[9, 0, 0, 0], 1).unwrap_err().contains("made by a newer fewer-pack"));
}

/// Parsed lines payload, for building hostile ones: (header, tables, block header, frames).
struct Parts {
    tables: Vec<Vec<u8>>,
    braw: u64,
    flags: u8,
    lines: u64,
    frames: Vec<Vec<u8>>,
    crc: [u8; 4],
}

fn split_one_block(p: &[u8]) -> Parts {
    let mut r = Rd::new(p);
    r.byte().unwrap();
    r.varint().unwrap();
    assert_eq!(r.varint().unwrap(), 1);
    let nt = r.varint().unwrap();
    let tables = (0..nt).map(|_| r.frame().unwrap().to_vec()).collect();
    let braw = r.varint().unwrap();
    assert_eq!(r.byte().unwrap(), KIND_LINES);
    let flags = r.byte().unwrap();
    let lines = r.varint().unwrap();
    let ns = r.varint().unwrap();
    let frames = (0..ns).map(|_| r.frame().unwrap().to_vec()).collect();
    let c = r.take(4).unwrap();
    Parts { tables, braw, flags, lines, frames, crc: [c[0], c[1], c[2], c[3]] }
}

fn join_one_block(raw: u64, q: &Parts) -> Vec<u8> {
    let mut p = vec![ENGINE_ID];
    put_varint(&mut p, raw);
    put_varint(&mut p, 1);
    put_varint(&mut p, q.tables.len() as u64);
    for t in &q.tables {
        put_varint(&mut p, t.len() as u64);
        p.extend_from_slice(t);
    }
    put_varint(&mut p, q.braw);
    p.push(KIND_LINES);
    p.push(q.flags);
    put_varint(&mut p, q.lines);
    put_varint(&mut p, q.frames.len() as u64);
    for f in &q.frames {
        put_varint(&mut p, f.len() as u64);
        p.extend_from_slice(f);
    }
    p.extend_from_slice(&q.crc);
    p
}

/// Hostile payloads (their checksums would pass in an archive) must fail at once, without
/// memory or time that grows faster than the sizes they declare.
#[test]
fn hostile_payloads_are_refused_fast() {
    let t = std::time::Instant::now();
    let d = log_lines(2000);
    let good = pack(&d, 1);
    let q = split_one_block(&good);
    assert!(unpack(&join_one_block(d.len() as u64, &q), 1).unwrap() == d);
    // any stream replaced by a frame that declares far more than the block may hold
    let bomb = eng::code(&vec![0u8; 64 << 20]);
    for k in 0..q.frames.len() {
        let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
        h.frames[k] = bomb.clone();
        assert!(unpack(&join_one_block(d.len() as u64, &h), 1).is_err(), "stream {k}");
    }
    // tables declaring more than the payload may hold
    let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
    h.tables[0] = bomb.clone();
    assert!(unpack(&join_one_block(d.len() as u64, &h), 1).is_err());
    // garbage in every stream, one at a time (stored frames: no limit is broken): an error,
    // or the right text when the block does not use that stream
    let mut x = 11u64;
    for k in 0..q.frames.len() {
        for _ in 0..20 {
            let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
            let n = (xorshift(&mut x) % 300) as usize;
            let junk: Vec<u8> = (0..n).map(|_| xorshift(&mut x) as u8).collect();
            h.frames[k] = eng::store(&junk);
            if let Ok(back) = unpack(&join_one_block(d.len() as u64, &h), 1) {
                assert!(back == d, "stream {k}: wrong output");
            }
        }
    }
    // a line count far above what the patterns allow, with every other stream as it was
    let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
    h.lines = d.len() as u64;
    assert!(unpack(&join_one_block(d.len() as u64, &h), 1).is_err());
    // a block of 400,000 lines whose one pattern has 1,000 marks (lines x marks of work)
    let lines = 400_000u64;
    let mut pat = vec![0u8; 1000];
    pat.push(b'\n');
    let mut h = Parts {
        tables: vec![eng::store(&pat), Vec::new()],
        braw: lines,
        flags: 1,
        lines,
        frames: q.frames.clone(),
        crc: [0; 4],
    };
    h.frames[0] = eng::code(&varints(&[0]));
    for k in 1..h.frames.len() {
        if k != 8 {
            h.frames[k] = eng::store(&[0u8; 64]);
        }
    }
    assert!(unpack(&join_one_block(lines, &h), 2).is_err());
    assert!(t.elapsed().as_secs() < 20, "too slow: {:?}", t.elapsed());
}

#[test]
fn check_block_limits() {
    let s = |pat: &[u8], hdr: &[u8]| {
        let mut v = vec![Vec::new(); eng::STREAMS];
        v[0] = pat.to_vec();
        v[2] = hdr.to_vec();
        v
    };
    // one column (heads.rs reads and checks the rest of the header when opening)
    let hdr = varints(&[1, 0, 0]);
    assert!(eng::check_block(&s(b"a \0\n", &hdr), 1, 4).is_ok());
    // more columns than the opener keeps memory for; no header at all
    assert!(eng::check_block(&s(b"a \0\n", &varints(&[eng::MAX_COLS + 1, 0, 0])), 1, 4).is_err());
    assert!(eng::check_block(&s(b"a \0\n", b""), 1, 4).is_err());
    // a line order (lines grouped by source: this engine's setting never writes one)
    let mut v = s(b"a \0\n", &hdr);
    v[9] = varints(&[0, 2, 1, 1, 0, 1]);
    assert!(eng::check_block(&v, 2, 8).is_err());
    // header flags it never writes: pattern ids coded with TidModel (1), unknown bits (32: the
    // ID columns of format 2; 64: keyed slots, whose plan heads::read checks when the block is
    // opened)
    for f in [1, 128] {
        assert!(eng::check_block(&s(b"a \0\n", &varints(&[1, 0, f])), 1, 4).is_err(), "flag {f}");
    }
    for f in [2, 4, 8, 32] {
        assert!(eng::check_block(&s(b"a \0\n", &[varints(&[1, 0, f]), varints(&[0])].concat()), 1, 4).is_ok(), "flag {f}");
    }
    // no pattern, more patterns than lines, too many marks
    assert!(eng::check_block(&s(b"", &hdr), 1, 4).is_err());
    assert!(eng::check_block(&s(b"a\nb\n", &hdr), 1, 4).is_err());
    let mut many = vec![0u8; eng::MAX_MARKS + 1];
    many.push(b'\n');
    assert!(eng::check_block(&s(&many, &hdr), 1, 1 << 20).is_err());
    // derived fields: the rewritten text the side stream declares (rule bits, size) may be
    // at most 2 x size + 4 KiB, and not empty; the limits then follow its size
    let mut v = s(b"a \0\n", &hdr);
    v[10] = varints(&[2, 100]);
    assert!(eng::check_block(&v, 1, 4).is_ok());
    v[10] = varints(&[2, 2 * 4 + 4096]);
    assert!(eng::check_block(&v, 1, 4).is_ok());
    v[10] = varints(&[2, 2 * 4 + 4096 + 1]);
    assert!(eng::check_block(&v, 1, 4).is_err());
    v[10] = varints(&[2, 0]);
    assert!(eng::check_block(&v, 1, 4).is_err());
    v[10] = vec![0x80];
    assert!(eng::check_block(&v, 1, 4).is_err());
    let mut wide = vec![0u8; 900];
    wide.push(b'\n');
    let mut v = s(&wide, &hdr);
    assert!(eng::check_block(&v, 200, 1000).is_err());
    v[10] = varints(&[2, 3000]);
    assert!(eng::check_block(&v, 200, 1000).is_ok());
}

/// Lines in the formats of the derive rules fewer-pack's packer uses (bgl, spark, proxy, float,
/// oldnew, ctime), each in a run of its own, as the logs they come from write them.
fn derive_inputs() -> Vec<(&'static str, Vec<u8>)> {
    let mut x = 31u64;
    let mut out = Vec::new();
    // bgl: LABEL EPOCH DATE NODE TIMESTAMP NODE ...: EPOCH = the timestamp + 7 hours
    let mut d = Vec::new();
    for i in 0..4000u64 {
        let s = 15 * 3600 + 42 * 60 + 50 + i / 4;
        let node = format!("R{:02}-M{}-N{}-C:J{:02}-U{:02}", i % 64, i % 2, i % 16, i % 18, (i / 7) % 12);
        let label = if i % 9 == 0 { "APPREAD" } else { "-" };
        d.extend_from_slice(
            format!("{label} {} 2005.06.03 {node} 2005-06-03-{:02}.{:02}.{:02}.{:06} {node} RAS KERNEL INFO instruction cache parity error corrected\n", 1117813370 + s, s / 3600, s / 60 % 60, s % 60, xorshift(&mut x) % 1_000_000)
                .as_bytes(),
        );
    }
    out.push(("bgl", d));
    // spark: tasks assigned, running, finished (in a changing order)
    let mut d = Vec::new();
    let mut running: Vec<(u64, u64)> = Vec::new();
    for tid in 0..1500u64 {
        d.extend_from_slice(format!("15/09/01 18:16:43 INFO executor.CoarseGrainedExecutorBackend: Got assigned task {tid}\n").as_bytes());
        d.extend_from_slice(format!("15/09/01 18:16:43 INFO executor.Executor: Running task {}.0 in stage 0.0 (TID {tid})\n", tid + 69).as_bytes());
        running.push((tid + 69, tid));
        if running.len() > 3 {
            let (t, n) = running.remove((xorshift(&mut x) % 3) as usize);
            d.extend_from_slice(format!("15/09/01 18:16:44 INFO executor.Executor: Finished task {t}.0 in stage 0.0 (TID {n}). 1835 bytes result sent to driver\n").as_bytes());
        }
    }
    out.push(("spark", d));
    // proxy: open lines, and close lines whose lifetime follows from the two clock times
    let mut d = Vec::new();
    let apps = ["chrome.exe", "firefox.exe", "outlook.exe"];
    let mut open: Vec<(u64, usize)> = Vec::new();
    for i in 0..3000u64 {
        let t = 16 * 3600 + 49 * 60 + i / 2;
        let clock = format!("[10.30 {:02}:{:02}:{:02}]", t / 3600, t / 60 % 60, t % 60);
        if open.len() < 5 || xorshift(&mut x) % 2 == 0 {
            let a = (xorshift(&mut x) % 3) as usize;
            d.extend_from_slice(format!("{clock} {} - proxy.cse.cuhk.edu.hk:5070 open through proxy proxy.cse.cuhk.edu.hk:5070 HTTPS\n", apps[a]).as_bytes());
            open.push((t, a));
        } else {
            let (t0, a) = open.remove((xorshift(&mut x) % open.len() as u64) as usize);
            let life = if t == t0 { "<1 sec".to_string() } else { format!("{:02}:{:02}", (t - t0) / 60, (t - t0) % 60) };
            d.extend_from_slice(format!("{clock} {} - proxy.cse.cuhk.edu.hk:5070 close, {} bytes sent, {} bytes received, lifetime {life}\n", apps[a], i * 7, i * 13).as_bytes());
        }
    }
    out.push(("proxy", d));
    // float: "KEY is : F" at the line end, F printed as Java prints a float
    let mut d = Vec::new();
    for i in 0..3000u64 {
        let f = ((i % 997) as f32 + 1.0) / 1024.0 + (xorshift(&mut x) % 100) as f32 / 1e5;
        d.extend_from_slice(
            format!("2015-10-17 15:38:11,574 INFO [IPC Server handler {} on 47468] org.apache.hadoop.mapred.TaskAttemptListenerImpl: Progress of TaskAttempt attempt_1445062781478_0011_m_{:06}_0 is : {f}\n", i % 30, i % 40).as_bytes(),
        );
    }
    out.push(("float", d));
    // oldnew: "type=T,NEW,old=OLD", OLD mostly the last NEW of type T
    let mut d = Vec::new();
    let mut last = [0u64; 5];
    for i in 0..3000u64 {
        let t = (i % 5) as usize;
        let new = xorshift(&mut x) % 100_000;
        let old = if i % 7 == 0 { new / 2 } else { last[t] };
        last[t] = new;
        d.extend_from_slice(format!("20171223-22:19:58:{:03}|HiH_DataStatManager|30002312|new date =20171223, type={},{new}.0,old={old}.0\n", i % 1000, 40000 + t).as_bytes());
    }
    out.push(("oldnew", d));
    // ctime: a syslog line ending with a ctime() copy of its own clock time
    let mut d = Vec::new();
    for i in 0..3000u64 {
        let s = 3 * 3600 + 28 * 60 + i;
        let hms = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
        d.extend_from_slice(format!("Jun 11 {hms} combo ftpd[{}]: connection from 209.184.7.{} () at Sat Jun 11 {hms} 2005 \n", 4301 + i % 50, i % 250).as_bytes());
    }
    out.push(("ctime", d));
    out
}

/// Every rule gives a block with derived fields (its side stream is not empty) that opens to
/// the same bytes: a newer engine that changes the rules' markers or bits shows here (its
/// rewrites would fail check_derived and blocks would quietly pack without them).
#[test]
fn derived_blocks_round_trip() {
    for (name, d) in derive_inputs() {
        let p = pack(&d, 2);
        let q = split_one_block(&p);
        assert!(!q.frames[10].is_empty(), "{name}: no derived fields");
        assert!(unpack(&p, 2).unwrap() == d, "{name}: round trip");
        assert!(unpack(&p, 1).unwrap() == d, "{name}: round trip (1 thread)");
        assert!(pack(&d, 1) == p, "{name}: the same bytes on 1 thread");
        // the derived block is smaller than the same lines without their rules
        let lines = lines_of(&d, 0, d.len());
        let plain = eng::code_streams(&eng::pack_block(&d, &lines, &eng::Rules::default(), 1, None), 1);
        let derived = eng::code_streams(&eng::pack_block(&d, &lines, &eng::detect(&lines), 1, None), 1);
        let sum = |f: &[(Vec<u8>, Option<eng::Parsed>)]| f.iter().map(|x| x.0.len()).sum::<usize>();
        assert!(sum(&derived) < sum(&plain), "{name}: {} >= {}", sum(&derived), sum(&plain));
    }
}

/// The side stream of a rewritten text: rule bits, its size, (bgl) 2 offsets.
fn side_of(bits: u64, text: &[u8], extra: &[u64]) -> Vec<u8> {
    let mut v = vec![bits, text.len() as u64];
    v.extend_from_slice(extra);
    varints(&v)
}

/// check_derived takes what the packer writes and refuses texts that would make putting the
/// derived parts back slow or big.
#[test]
fn derived_text_check() {
    const MK: u8 = 1;
    let ok = |bits: u64, t: &[u8]| eng::check_derived(t, &side_of(bits, t, if bits & 2 != 0 { &[0, 0] } else { &[] })).is_ok();
    let line = |parts: &[&[u8]]| parts.concat();
    // legal forms (one rule per line)
    let g = line(&[b"x key is : ", &[MK, b'g'], b"-12\n"]);
    assert!(ok(64, &g));
    let t = line(&[b"Jun 11 03:28:22 h at ", &[MK, b't'], b"2005 \n"]);
    assert!(ok(256, &t));
    let o = line(&[b"[10.30 16:49:06] ", &[MK, b'o'], b"app - host:1 open through x\n"]);
    let c = line(&[b"[10.30 16:49:07] ", &[MK, b'c'], b"0 close, lifetime ", &[MK, b'l'], b"2\n"]);
    assert!(ok(32, &[o.clone(), c.clone()].concat()));
    let r = line(&[b"Running task ", &[MK, b'r'], b"1.0 in stage 0.0 (TID ", &[MK, b'u'], b"1)\n"]);
    assert!(ok(16, &r));
    let w = line(&[b"type=", &[MK, b'w'], b"5,10.0,old=", &[MK, b'x'], b"\n"]);
    assert!(ok(128, &w));
    // bgl (format 2, B5): the gap marker, then the fill marker after the timestamp
    let b = line(&[b"- ", &[MK, b'b'], b"R02-M1 2005-06-03-15.42.50.363779", &[MK, b'e'], b"RAS\n"]);
    assert!(ok(2, &b));
    assert!(ok(2, &line(&[b"- ", &[MK, b'B'], b"R02-M1 2005-06-03-15.42.50.363779", &[MK, b'E'], b"\n"])));
    // ... not the old form (no fill marker), not a fill marker away from the timestamp's end
    // or alone
    assert!(!ok(2, &line(&[b"- ", &[MK, b'b'], b"R02-M1 2005-06-03-15.42.50.363779 RAS\n"])));
    assert!(!ok(2, &line(&[b"- ", &[MK, b'b'], b"R02-M1 2005-06-03-15.42.50.36377", &[MK, b'e'], b"RAS\n"])));
    assert!(!ok(2, &line(&[b"- R02-M1 2005-06-03-15.42.50.363779", &[MK, b'e'], b"RAS\n"])));
    // the side stream: unknown or unused rule bits, a wrong size, bytes left over
    assert!(!ok(64 | 4, &g));
    assert!(eng::check_derived(&g, &side_of(64, b"x", &[])).is_err());
    assert!(eng::check_derived(&g, &side_of(64, &g, &[7])).is_err());
    // a marker of a rule that is off; an unknown letter; a sub-marker alone; at the end
    assert!(!ok(256, &g));
    assert!(!ok(64, &line(&[b"x ", &[MK, b'q'], b"1\n"])));
    assert!(!ok(16, &line(&[b"x ", &[MK, b'u'], b"1\n"])));
    assert!(!ok(64, &line(&[b"x key is : ", &[MK]])));
    // two float or ctime markers in one line, a float marker after another marker in its line
    assert!(!ok(64, &line(&[&g[..g.len() - 1], b" k is : ", &[MK, b'g'], b"1\n"])));
    assert!(!ok(256, &line(&[&t[..t.len() - 1], b" ", &[MK, b't'], b"2005\n"])));
    assert!(!ok(64 | 256, &line(&[b"Jun 11 03:28:22 h at ", &[MK, b't'], b"2005 k is : ", &[MK, b'g'], b"1\n"])));
    // a float marker without " is : " before it
    assert!(!ok(64, &line(&[b"x key is:", &[MK, b'g'], b"1\n"])));
    // an open line without " - " or its host's end; a marker inside an open line's host
    assert!(!ok(32, &line(&[b"[10.30 16:49:06] ", &[MK, b'o'], b"app open\n"])));
    assert!(!ok(32, &line(&[b"[10.30 16:49:06] ", &[MK, b'o'], b"app - host"])));
    assert!(!ok(32, &line(&[b"[10.30 16:49:06] ", &[MK, b'o'], b"app - ho", &[MK, b'o'], b"st open\n"])));
    // a close line naming an open line that is not there
    assert!(!ok(32, &line(&[&o, &line(&[b"[10.30 16:49:07] ", &[MK, b'c'], b"1 close\n"])])));
    assert!(!ok(32, &c));
    // spark: a task word over 24 bytes, a 'u' marker without the ' ' before it
    assert!(!ok(16, &line(&[b"Running task ", &[MK, b'r'], &[b'1'; 30], b" in stage 0.0 (TID 1)\n"])));
    assert!(!ok(16, &line(&[b"Running task ", &[MK, b'r'], b"1.0 in stage 0.0 (TID\n", &[MK, b'u'], b"1)\n"])));
    // more different oldnew types than MAX_KEYS
    let types = |n: usize| (0..n).flat_map(|k| line(&[b"type=", &[MK, b'w'], format!("{k},1,old=2\n").as_bytes()])).collect::<Vec<u8>>();
    assert!(!ok(128, &types(5000)));
    assert!(ok(128, &types(4000)));
}

/// The engine's streams for `text` (lines with '\n'; it may hold derive markers: the engine
/// then packs it as it is) with `side` as the derive side stream: a crafted block.
fn crafted_block(text: &[u8], side: Vec<u8>) -> (Vec<Vec<u8>>, usize) {
    let lines = lines_of(text, 0, text.len());
    let mut blk = eng::pack_block(text, &lines, &eng::Rules::default(), 1, None);
    let mut streams: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
    streams[10] = side;
    (streams, lines.len())
}

/// Open a crafted block as the adapter opens a block of `size` bytes.
fn open_crafted(streams: Vec<Vec<u8>>, lines: usize, size: usize) -> Result<Vec<u8>, &'static str> {
    eng::check_block(&streams, lines, size)?;
    let mut buf = vec![0u8; size + 64];
    let end = guarded(|| eng::open_block(streams, lines, &mut buf, size, &mut Vec::new())).ok_or("caught")??;
    Ok(buf[..end].to_vec())
}

/// Crafted rewritten texts that would make putting the derived parts back take time or memory
/// far beyond the block's size (reading the output line back for every float or ctime marker,
/// open lines read again by every later open marker, the proxy list moved for every close)
/// are refused at once.
#[test]
fn hostile_derived_blocks_are_refused_fast() {
    const MK: u8 = 1;
    let t0 = std::time::Instant::now();
    // a correct rewrite opens (the check passes what the packer writes)
    let d = &derive_inputs()[3].1;
    let lines = lines_of(d, 0, d.len());
    let mut blk = eng::pack_block(d, &lines, &eng::detect(&lines), 1, None);
    let streams: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
    assert!(!streams[10].is_empty());
    assert!(open_crafted(streams, lines.len(), d.len()).unwrap() == *d);
    // (name, rule bits, rewritten text, the block's declared size: room for the output)
    let o_line = [b"[10.30 16:49:06] ".as_slice(), &[MK, b'o'], b"a - h open\n"].concat();
    let cases: Vec<(&str, u64, Vec<u8>, usize)> = vec![
        // lines of 1,000 float markers (the most values a line may have) and no space between
        // them: each marker would read the line back to its start for its key (1065353216:
        // the bits of 1.0; later lines repeat the keys, so their floats are the same)
        ("floats in one line", 64, (0..500u32).flat_map(|k| {
            let d: &[u8] = if k == 0 { b"1065353216" } else { b"0" };
            [format!("key{k} is : ").into_bytes(), [&[MK, b'g'][..], b"1065353216yyyyyy"].concat(), [&[MK, b'g'][..], d, b"yyyyyy"].concat().repeat(999), b"\n".to_vec()].concat()
        }).collect(), 0),
        // lines of 1,000 ctime markers: each reads the line back to its start for the header
        ("ctimes in one line", 256, [b"Jun 11 03:28:22 h".to_vec(), [b" at ".as_slice(), &[MK, b't'], b"2005"].concat().repeat(1000), b"\n".to_vec()].concat().repeat(700), 3),
        // open markers whose app and host run to the end of the text: each copies the rest
        ("open lines read again", 32, [[b"[10.30 16:49:06] ".as_slice(), &[MK, b'o'], b"app\n"].concat().repeat(40_000), b"x - host end\n".to_vec()].concat(), 0),
        // 4,096 open lines, then close lines that take the second oldest open each time
        ("proxy list moves", 32, {
            let mut v = o_line.repeat(4096);
            for _ in 0..40_000 {
                v.extend_from_slice(&o_line);
                v.extend_from_slice(&[b"[10.30 16:49:07] ".as_slice(), &[MK, b'c'], b"4094 x\n"].concat());
            }
            v
        }, 0),
        // oldnew types that all fall in one place of the opener's map (multiples of 2^20)
        ("oldnew types", 128, (0..100_000u64).flat_map(|k| [b"type=".as_slice(), &[MK, b'w'], format!("{},1,old=2\n", k << 20).as_bytes()].concat()).collect(), 0),
        // a rule fewer-pack never writes (hdfs: each 'i' marker looks ahead for the line's block id)
        ("hdfs markers", 4, [[b"a ".as_slice(), &[MK, b'i'], b"0 b\n"].concat().repeat(60_000), [b"c ".as_slice(), &[MK, b'k'], b"blk_1 d\n"].concat()].concat(), 0),
    ];
    for (name, bits, text, room) in cases {
        let size = text.len() * room.max(1);
        let (streams, lines) = crafted_block(&text, side_of(bits, &text, &[]));
        let t = std::time::Instant::now();
        let r = open_crafted(streams, lines, size);
        assert!(t.elapsed().as_secs_f64() < 2.0, "{name}: too slow: {:?}", t.elapsed());
        assert!(r.is_err(), "{name}: opened");
    }
    assert!(t0.elapsed().as_secs() < 20, "too slow: {:?}", t0.elapsed());
}

/// The numbers of grouped columns are put back in line order by copying their segments; the
/// segments come from the column header (heads.rs) and must lie in the number streams: a
/// block whose number streams are cut short is refused, never read past.
#[test]
fn grouped_columns_stay_in_their_streams() {
    let d = &fuzz_inputs()[1];
    let lines = lines_of(d, 0, d.len());
    let mut blk = eng::pack_block(d, &lines, &eng::Rules::default(), 1, None);
    let streams: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
    // (lines are never grouped by source in this engine's setting: no line order)
    assert!(streams[9].is_empty(), "a line order");
    assert!(open_crafted(streams.clone(), lines.len(), d.len()).unwrap() == *d);
    for k in [4, 7] {
        for cut in [1, 2, 7, 100] {
            let mut s = streams.clone();
            let n = s[k].len();
            s[k].truncate(n.saturating_sub(cut));
            if let Ok(back) = open_crafted(s, lines.len(), d.len()) {
                assert!(back != *d || n == 0, "stream {k} cut by {cut}: opened");
            }
        }
    }
}

/// Lines whose number slots follow from earlier ones (a sum, k times, human sizes): the
/// engine's rules give them (column header flag 16, the rules section).
fn rules_input() -> Vec<u8> {
    let mut x = 5u64;
    let mut d = Vec::new();
    for i in 0..3000u64 {
        let (a, b) = (xorshift(&mut x) % 5000, xorshift(&mut x) % 900);
        let sz = 1024 + xorshift(&mut x) % 3_000_000;
        d.extend_from_slice(format!("10:{:02}:{:02} task {i} boot={a} init={b} total={} scaled={} sent {sz} bytes\n", i / 60 % 60, i % 60, a + b, 4 * a).as_bytes());
    }
    d
}

/// Where the rules section of a column header starts and ends (heads.rs layout: varint
/// columns, byte, byte flags, [splitting columns], [rules section, varint answers' length]).
fn rules_section(h: &[u8]) -> Option<(usize, usize)> {
    let mut r = Rd::new(h);
    r.varint().ok()?;
    r.byte().ok()?;
    let flags = r.byte().ok()?;
    if flags & 16 == 0 {
        return None;
    }
    if flags & 2 != 0 {
        let n = r.varint().ok()?;
        for _ in 0..n {
            r.varint().ok()?;
        }
    }
    let start = r.p;
    let np = r.varint().ok()?;
    for _ in 0..np {
        r.varint().ok()?;
        let n = r.varint().ok()?;
        for _ in 0..n {
            r.varint().ok()?;
            let op = r.byte().ok()? & 127;
            r.byte().ok()?;
            r.varint().ok()?;
            r.varint().ok()?;
            if op == 0 || op == 3 {
                r.varint().ok()?;
                for _ in 1..3 {
                    if r.byte().ok()? != 0 {
                        r.varint().ok()?;
                    }
                }
            }
        }
    }
    Some((start, r.p))
}

/// A rules section of one pattern (`t`) with the given rules, each (slot, op byte, kind,
/// slot - source, m, [c, sign, slot - source, sign, slot - source] for LIN / NEAR).
fn rules_bytes(t: u64, rules: &[Vec<u64>]) -> Vec<u8> {
    let mut o = varints(&[1, t, rules.len() as u64]);
    for r in rules {
        put_varint(&mut o, r[0]);
        o.push(r[1] as u8);
        o.push(r[2] as u8);
        put_varint(&mut o, r[3]);
        put_varint(&mut o, r[4]);
        if r[1] & 127 == 0 || r[1] & 127 == 3 {
            put_varint(&mut o, r[5]);
            o.push(r[6] as u8);
            if r[6] != 0 {
                put_varint(&mut o, r[7]);
            }
            o.push(r[8] as u8);
            if r[8] != 0 {
                put_varint(&mut o, r[9]);
            }
        }
    }
    o
}

/// The rules give values (the header has a rules section) and the block opens to the same
/// bytes on 1 and 2 threads; a newer engine whose rules section the check refuses shows here
/// (its blocks would quietly pack without rules, or as plain bytes).
#[test]
fn rules_blocks_round_trip() {
    let d = rules_input();
    let p = pack(&d, 2);
    let q = split_one_block(&p);
    let h = eng::decode(&q.frames[2], 1 << 24, None).unwrap();
    assert!(rules_section(&h).is_some(), "no rules section");
    assert!(unpack(&p, 2).unwrap() == d);
    assert!(unpack(&p, 1).unwrap() == d);
    assert!(pack(&d, 1) == p, "the same bytes on 1 thread");
}

/// Crafted rules sections (each would make the opener slow or big, or read past its data)
/// are refused before the opener runs, at once.
#[test]
fn hostile_rules_are_refused_fast() {
    let t0 = std::time::Instant::now();
    let d = rules_input();
    let lines = lines_of(&d, 0, d.len());
    let mut blk = eng::pack_block(&d, &lines, &eng::Rules::default(), 1, None);
    let streams: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
    let (a, b) = rules_section(&streams[2]).expect("no rules section");
    assert!(open_crafted(streams.clone(), lines.len(), d.len()).unwrap() == d);
    // the one pattern (every line has it) and its marks
    assert_eq!(streams[0].iter().filter(|&&c| c == b'\n').count(), 1);
    let marks = streams[0].iter().filter(|&&c| c == 0).count() as u64; // (the engine's MARK)
    let with = |sec: Vec<u8>| {
        let mut s = streams.clone();
        s[2] = [&streams[2][..a], &sec, &streams[2][b..]].concat();
        s
    };
    let lin = |slot: u64, src: u64| vec![slot, 0, 0, slot - src, 1, 0, 0, 0, 0, 0];
    // a correct section of our own (one rule: a slot equal to the slot before) passes the check
    assert!(eng::check_block(&with(rules_bytes(0, &[lin(3, 2)])), lines.len(), d.len()).is_ok());
    let cases: Vec<(&str, Vec<u8>, usize)> = vec![
        // a human size in unit 1024^(2^40): the opener divides 2^40 times per value
        ("huge unit", rules_bytes(0, &[vec![3, 1, 0, 1, 1 << 40]]), d.len()),
        ("unit 0", rules_bytes(0, &[vec![3, 2, 0, 1, 0]]), d.len()),
        // a zero-padded kind of 200 digits; an operation that does not exist
        ("kind 200", rules_bytes(0, &[vec![3, 0, 200, 1, 1, 0, 0, 0, 0, 0]]), d.len()),
        ("operation 9", rules_bytes(0, &[vec![3, 9, 0, 1, 1]]), d.len()),
        // slots past the pattern, sources not before the slot, slots out of order, signs
        ("slot past the pattern", rules_bytes(0, &[lin(marks, 0)]), d.len()),
        ("source after the slot", rules_bytes(0, &[vec![3, 0, 0, 0, 1, 0, 0, 0, 0, 0]]), d.len()),
        ("slots out of order", rules_bytes(0, &[lin(3, 2), lin(2, 1)]), d.len()),
        ("sign 7", rules_bytes(0, &[vec![3, 0, 0, 1, 1, 0, 7, 2, 0, 0]]), d.len()),
        // a pattern that does not exist; more patterns with rules than patterns
        ("pattern 5", rules_bytes(5, &[lin(3, 2)]), d.len()),
        ("2^40 patterns", [varints(&[1 << 40]), rules_bytes(0, &[lin(3, 2)])[1..].to_vec()].concat(), d.len()),
        // more rules than the pattern has marks
        ("rules past the marks", rules_bytes(0, &(0..marks + 1).map(|k| lin(k + 1, k)).collect::<Vec<_>>()), d.len()),
        // more answers than the text has bytes (the block declared far smaller)
        ("answers past the text", rules_bytes(0, &[lin(3, 2), lin(4, 3)]), lines.len()),
    ];
    for (name, sec, size) in cases {
        let s = with(sec);
        let t = std::time::Instant::now();
        assert!(eng::check_block(&s, lines.len(), size).is_err(), "{name}: passed the check");
        let r = open_crafted(s, lines.len(), size);
        assert!(t.elapsed().as_secs_f64() < 2.0, "{name}: too slow: {:?}", t.elapsed());
        assert!(r.is_err(), "{name}: opened");
    }
    assert!(t0.elapsed().as_secs() < 20, "too slow: {:?}", t0.elapsed());
}

/// Inputs that use every stream of the engine: numbers, times, addresses, hex, UUIDs, names,
/// texts, repeats, words that vary, lines grouped by source, cache-coded columns.
fn fuzz_inputs() -> Vec<Vec<u8>> {
    let mut x = 77u64;
    let mut grouped = Vec::new();
    for i in 0..6000u64 {
        let w = xorshift(&mut x) % 16;
        grouped.extend_from_slice(
            format!("2026-11-15 10:{:02}:{:02}.{:03} [w{w}] job {} step {} took {} ms user{} {}\n", i / 3600 % 60, i / 60 % 60, i % 1000, 1000 + w * 7, i / 16, xorshift(&mut x) % 500, xorshift(&mut x) % 9, ["ok", "fail", "retry", "skip", "done"][(xorshift(&mut x) % 5) as usize])
                .as_bytes(),
        );
    }
    let mut mixed = Vec::new();
    for i in 0..4000u64 {
        let uuid = format!("{:08x}-{:04x}-4{:03x}-9{:03x}-{:012x}", xorshift(&mut x) as u32, i % 65536, i % 4096, i % 4096, xorshift(&mut x) % (1 << 48));
        mixed.extend_from_slice(
            format!(
                "Jan {} 1{}:0{}:{:02} host{} sshd[{}]: {} for {} from 10.{}.{}.{} port {} id={:016x} req={} blk_{}{} at 15.42.{:02}.{:06} {}\n",
                1 + i % 28,
                i % 10,
                i % 10,
                i % 60,
                i % 3,
                2000 + i % 50,
                ["Accepted password", "Failed password", "Invalid user"][(i % 3) as usize],
                ["root", "admin", "test", "guest", "oracle", "mysql", "ubuntu"][(xorshift(&mut x) % 7) as usize],
                i % 4,
                i % 7,
                xorshift(&mut x) % 256,
                40000 + xorshift(&mut x) % 20000,
                xorshift(&mut x),
                uuid,
                if i % 2 == 0 { "-" } else { "" },
                1_000_000_000_000_000 + xorshift(&mut x) % 1_000_000_000_000,
                i % 60,
                i * 37 % 1_000_000,
                "x".repeat((i % 5) as usize)
            )
            .as_bytes(),
        );
    }
    let mut v = vec![log_lines(3000), grouped, mixed];
    // derived fields: bgl alone, and spark, proxy, float, oldnew and ctime lines in one block
    let mut di = derive_inputs().into_iter();
    v.push(di.next().unwrap().1);
    let rest: Vec<Vec<u8>> = di.map(|(_, d)| d).collect();
    let mut mixed_rules = Vec::new();
    for k in 0..400 {
        for r in &rest {
            let ls = lines_of(r, 0, r.len());
            for l in &ls[k * ls.len() / 400..(k + 1) * ls.len() / 400] {
                mixed_rules.extend_from_slice(l);
                mixed_rules.push(b'\n');
            }
        }
    }
    v.push(mixed_rules);
    // number slots given by rules (the column header's rules section and answers)
    v.push(rules_input());
    v
}

/// Fuzzing, run with: cargo test --release fuzz -- --ignored (FEWER_FUZZ=N rounds per input,
/// 3000 by default). Payloads whose streams are damaged after their frames are opened (stored
/// again, so the engine's opener reads the damage) and whose bytes are damaged: every result
/// must be an error or the exact text, never a crash, a hang or huge memory.
#[test]
#[ignore]
fn fuzz_streams() {
    let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let mut x = 0x5eed_u64;
    let (mut errs, mut oks) = (0usize, 0usize);
    let t0 = std::time::Instant::now();
    for d in fuzz_inputs() {
        let p = pack(&d, 2);
        assert!(unpack(&p, 2).unwrap() == d);
        let q = split_one_block(&p);
        let raw = d.len() as u64;
        eprintln!("fuzz input: {} bytes, frames {:?}", d.len(), q.frames.iter().map(|f| f.len()).collect::<Vec<_>>());
        // the streams as the opener sees them (table streams: the block's lists, unchanged)
        let opened: Vec<Vec<u8>> = q.frames.iter().map(|f| if f.is_empty() { Vec::new() } else { eng::decode(f, 64 << 20, None).unwrap() }).collect();
        let tables: Vec<Vec<u8>> = q.tables.iter().map(|f| if f.is_empty() { Vec::new() } else { eng::decode(f, 64 << 20, None).unwrap() }).collect();
        for round in 0..rounds {
            let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
            let pick = (xorshift(&mut x) % (opened.len() as u64 + 2)) as usize;
            let mut s = if pick < opened.len() { opened[pick].clone() } else { tables[pick - opened.len()].clone() };
            for _ in 0..1 + xorshift(&mut x) % 3 {
                let r = xorshift(&mut x);
                let i = if s.is_empty() { 0 } else { (r >> 8) as usize % s.len() };
                match r % 7 {
                    0 if !s.is_empty() => s[i] ^= 1 << ((r >> 40) % 8),
                    1 => s.truncate(i),
                    2 => s.insert(i, (r >> 32) as u8),
                    3 if !s.is_empty() => s[i] = 0xFF,
                    4 if !s.is_empty() => {
                        // a big varint
                        let v = varints(&[(r >> 16) % (1 << (r % 60))]);
                        s.splice(i..(i + 1).min(s.len()), v);
                    }
                    5 => {
                        let j = (r >> 20) as usize % (s.len() + 1);
                        let piece: Vec<u8> = s[i.min(j)..i.max(j)].to_vec();
                        s.splice(i..i, piece);
                    }
                    _ if !s.is_empty() => {
                        s.remove(i);
                    }
                    _ => {}
                }
            }
            let f = if s.is_empty() && round % 2 == 0 { Vec::new() } else { eng::store(&s) };
            if pick < opened.len() {
                h.frames[pick] = f;
            } else {
                h.tables[pick - opened.len()] = f;
            }
            let mut pay = join_one_block(raw, &h);
            if round % 5 == 4 {
                // and a damaged byte in the payload itself
                let i = (xorshift(&mut x) as usize) % pay.len();
                pay[i] ^= 1 << (x % 8);
            }
            let t = std::time::Instant::now();
            match unpack(&pay, 1 + round % 3) {
                Ok(back) => {
                    assert!(back == d, "fuzz: wrong output (stream {pick}, round {round})");
                    oks += 1;
                }
                Err(_) => errs += 1,
            }
            assert!(t.elapsed().as_secs() < 5, "fuzz: slow open (stream {pick}, round {round}): {:?}", t.elapsed());
        }
    }
    eprintln!("fuzz: {} payloads, {errs} refused, {oks} identical, {:?}", errs + oks, t0.elapsed());
}

/// line_ok reads 8 bytes at a time: the same answers as byte by byte.
#[test]
fn line_ok_matches_bytes() {
    fn slow(l: &[u8]) -> bool {
        let n = l.len();
        if n <= LINE_LOOK {
            return true;
        }
        if n > LINE_MAX {
            return false;
        }
        let (mut words, mut run, mut longest, mut prev) = (0usize, 0usize, 0usize, 0u8);
        for &c in l {
            let k = CLASS[c as usize];
            let g = if k == 1 { 1 } else { (k >= 2) as u8 * 2 };
            words += (g != 0 && g != prev) as usize;
            prev = g;
            run = if k == 2 { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        longest <= HEX_RUN && words <= LINE_WORDS && words * n <= LINE_COST
    }
    let mut x = 21u64;
    let alphabets: [&[u8]; 5] = [b"abcdef", b"abcdef0", b"a1 ", b"aZ9 -.\xc3\xa9", b"0123456789abcdefghijklmnopqrstuvwxyz ABCDEF\xff\x00"];
    for i in 0..20_000 {
        let al = alphabets[i % alphabets.len()];
        let n = [200, 257, 300, 1000, 1025, 2000, 5000, 17000, 70000][(xorshift(&mut x) % 9) as usize] + (xorshift(&mut x) % 9) as usize;
        let mut l: Vec<u8> = (0..n).map(|_| al[(xorshift(&mut x) % al.len() as u64) as usize]).collect();
        if i % 3 == 0 {
            // a long run of hex letters somewhere
            let a = (xorshift(&mut x) as usize) % n;
            let len = (xorshift(&mut x) as usize) % 600;
            for (k, c) in l[a..].iter_mut().take(len).enumerate() {
                *c = b"abcdefABCDEF"[k % 12];
            }
        }
        assert_eq!(line_ok(&l), slow(&l), "line of {n} bytes from {:?}", String::from_utf8_lossy(al));
    }
}

/// each_line (NEON: 64 then 16 bytes per step) finds exactly the lines a byte-by-byte split
/// finds, for every density of line ends and every start and end around the step sizes.
#[test]
fn each_line_same_as_bytes() {
    let mut x = 0x1234_5678_9abc_def1u64;
    for round in 0..400u64 {
        let n = (xorshift(&mut x) % 700) as usize;
        // one '\n' in about every `gap` bytes (gap 1: almost all)
        let gap = [1u64, 2, 3, 16, 63, 64, 65, 200, 1 << 20][(round % 9) as usize];
        let d: Vec<u8> = (0..n).map(|_| if xorshift(&mut x) % gap == 0 { b'\n' } else { b'a' + (xorshift(&mut x) % 26) as u8 }).collect();
        for _ in 0..8 {
            let from = if n == 0 { 0 } else { (xorshift(&mut x) % (n as u64 + 1)) as usize };
            let to = from + (xorshift(&mut x) % ((n - from) as u64 + 1)) as usize;
            let mut got = Vec::new();
            each_line(&d, from, to, |s, e| got.push((s, e)));
            let mut want = Vec::new();
            let mut s = from;
            for k in from..to {
                if d[k] == b'\n' {
                    want.push((s, k));
                    s = k + 1;
                }
            }
            if s < to {
                want.push((s, to));
            }
            assert_eq!(got, want, "n {n} from {from} to {to}");
        }
    }
}

/// cost_parts (four lines side by side) gives what the line-by-line reading gave, for blocks
/// of every step size and lines of many lengths.
#[test]
fn cost_parts_same_as_line_by_line() {
    fn reference(lines: &[&[u8]]) -> usize {
        let step = match lines.len() {
            0..=4096 => 1,
            4097..=65536 => 8,
            _ => 32,
        };
        let mut seen: FxMap<u64, ()> = FxMap::default();
        let mut est = 0u64;
        for l in lines.iter().step_by(step) {
            let (mut h, mut runs, mut prev) = (0x243f_6a88_85a3_08d3u64, 0u64, 0u8);
            for &c in l.iter() {
                let k = CLASS[c as usize];
                runs += (k == 1 && prev != 1) as u64;
                prev = k;
                if k != 1 && k != 2 {
                    h = (h.rotate_left(5) ^ c as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
                }
            }
            if runs > 0 && seen.insert(h, ()).is_none() {
                est += runs * l.len() as u64 / 2 * step as u64;
            }
        }
        est.div_ceil(COST_BUDGET).max(1) as usize
    }
    let mut x = 0x0bad_5eed_1234_5678u64;
    let alphabet = b"0123456789abcdefxyzQ :=-_/.";
    for n in [0usize, 1, 2, 3, 4, 5, 7, 8, 9, 4095, 4096, 4097, 4099, 65535, 65536, 65537, 65539, 70001] {
        for wide in [6u64, 40, 300, 3000] {
            let text: Vec<Vec<u8>> = (0..n)
                .map(|_| {
                    let len = (xorshift(&mut x) % wide) as usize;
                    (0..len).map(|_| alphabet[(xorshift(&mut x) % alphabet.len() as u64) as usize]).collect()
                })
                .collect();
            let lines: Vec<&[u8]> = text.iter().map(|l| l.as_slice()).collect();
            assert_eq!(cost_parts(&lines), reference(&lines), "{n} lines of up to {wide} bytes");
        }
    }
    // many different patterns with many numbers: more than one part
    let text: Vec<Vec<u8>> = (0..20_000u64).map(|i| format!("p{} {}", "x".repeat((i % 3000) as usize), "7 ".repeat(200)).into_bytes()).collect();
    let lines: Vec<&[u8]> = text.iter().map(|l| l.as_slice()).collect();
    assert!(reference(&lines) > 1);
    assert_eq!(cost_parts(&lines), reference(&lines));
}

/// The small stable sorts (fl::sort_stable_by, fl::sort_stable_idx) order exactly as the
/// standard library's stable sort: many ties, every length up to 600 and some long ones.
#[test]
fn sort_stable_by_same_as_std() {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut lens: Vec<usize> = (0..=600).collect();
    lens.extend([1023, 1024, 1025, 4096, 5000, 70_000]);
    for &n in &lens {
        for ties in [1u64, 3, 17, 1 << 40] {
            let v: Vec<(u64, usize)> = (0..n).map(|i| (xorshift(&mut x) % ties, i)).collect();
            let (mut a, mut b) = (v.clone(), v.clone());
            a.sort_by_key(|p| std::cmp::Reverse(p.0));
            fl::sort_stable_by(&mut b, |p, q| p.0 > q.0);
            assert_eq!(a, b, "n {n} ties {ties}");
            let k: Vec<u64> = v.iter().map(|e| e.0).collect();
            let mut c: Vec<usize> = (0..n).collect();
            fl::sort_stable_idx(&mut c, &mut |&p, &q| k[p] > k[q]);
            assert!(c.iter().zip(&a).all(|(&i, e)| v[i] == *e));
            let f: Vec<(f64, u8)> = v.iter().map(|&(k, i)| (k as f64 / 3.0 - 2.0, (i % 5) as u8)).collect();
            let (mut a, mut b) = (f.clone(), f.clone());
            a.sort_by(|p, q| q.0.total_cmp(&p.0).then(p.1.cmp(&q.1)));
            fl::sort_stable_by(&mut b, |p, q| q.0.total_cmp(&p.0).then(p.1.cmp(&q.1)) == std::cmp::Ordering::Less);
            assert_eq!(a, b);
        }
        if n <= 2000 {
            let w: Vec<Vec<u8>> = (0..n).map(|_| (0..xorshift(&mut x) % 4).map(|_| (xorshift(&mut x) % 3) as u8).collect()).collect();
            let mut a = w.clone();
            a.sort();
            let mut c: Vec<usize> = (0..n).collect();
            fl::sort_stable_idx(&mut c, &mut |&p, &q| w[p] < w[q]);
            assert!(c.iter().zip(&a).all(|(&i, e)| w[i] == *e));
        }
    }
}

/// cost_line (8 bytes at a time) gives what CostState::byte gives over every byte, for any
/// bytes (digits, hex letters in both cases, other letters, bytes over 127) and lengths.
#[test]
fn cost_line_same_as_bytes() {
    let mut x = 0xc057_11e5_0000_0001u64;
    for round in 0..40_000u64 {
        let len = (xorshift(&mut x) % 90) as usize;
        let l: Vec<u8> = (0..len)
            .map(|_| {
                let r = xorshift(&mut x);
                match round % 3 {
                    0 => r as u8,
                    1 => b"09afAFgzGZ:/ \x80\xff`@"[(r % 17) as usize],
                    _ if r % 3 == 0 => b'0' + (r % 10) as u8,
                    _ => (r >> 8) as u8,
                }
            })
            .collect();
        let mut st = CostState::NEW;
        for &c in &l {
            st.byte(c);
        }
        let n = cost_line(&l);
        assert_eq!((n.h, n.runs, n.prev == 1), (st.h, st.runs, st.prev == 1), "{l:?}");
    }
}

/// find_ends (NEON: 64 bytes per step, the first line end of a step written whether there is
/// one or not) finds exactly the '\n' a byte-by-byte search finds, for every density of line
/// ends and every start, end and base around the step size.
#[test]
fn find_ends_same_as_bytes() {
    let mut x = 0x005e_ed0f_e4d5_1234_u64;
    for round in 0..600u64 {
        let n = (xorshift(&mut x) % 1500) as usize;
        let gap = [1u64, 2, 3, 5, 16, 31, 63, 64, 65, 127, 200, 1 << 20][(round % 12) as usize];
        let d: Vec<u8> = (0..n).map(|_| if xorshift(&mut x) % gap == 0 { b'\n' } else { b'a' + (xorshift(&mut x) % 26) as u8 }).collect();
        for _ in 0..8 {
            let from = if n == 0 { 0 } else { (xorshift(&mut x) % (n as u64 + 1)) as usize };
            let to = from + (xorshift(&mut x) % ((n - from) as u64 + 1)) as usize;
            let base = from - (xorshift(&mut x) % (from as u64 + 1)) as usize;
            // values already in the list stay, the new ones follow them
            let mut got = vec![7u32; (round % 3) as usize];
            find_ends(&d, from, to, base, &mut got);
            let mut want = vec![7u32; (round % 3) as usize];
            want.extend((from..to).filter(|&k| d[k] == b'\n').map(|k| (k - base) as u32));
            assert_eq!(got, want, "n {n} from {from} to {to} base {base}");
        }
    }
}

/// The first pass as it was (each_line over the piece): the reference for scan_piece.
fn scan_piece_reference(input: &[u8], a: usize, b: usize) -> Vec<Found> {
    let mut parts = Vec::new();
    if looks_binary(&input[a..b]) {
        add_part(&mut parts, a, b, false, 0, Vec::new());
        return parts;
    }
    let (mut run, mut count, mut anchors) = (a, 0usize, Vec::new());
    each_line(input, a, b, |s, e| {
        if line_ok(&input[s..e]) {
            if count % ANCHOR == 0 {
                anchors.push((count, s));
            }
            count += 1;
        } else {
            let next = (e + 1).min(b);
            add_part(&mut parts, run, s, true, count, std::mem::take(&mut anchors));
            add_part(&mut parts, s, next, false, 0, Vec::new());
            run = next;
            count = 0;
        }
    });
    add_part(&mut parts, run, b, true, count, anchors);
    parts
}

/// Inputs for the first-pass tests: log lines of many lengths (over LINE_LOOK, LINE_WORDS and
/// LINE_MAX too), hex runs, short and empty lines, CR, binary bytes, all across the SCAN and
/// 64-byte steps.
fn scan_inputs() -> Vec<Vec<u8>> {
    const LOG: &[u8] = b"2015-10-18 18:01:47,978 INFO [main] node-17: got 4096 bytes from 10.0.0.1";
    let mut x = 0xfeed_beef_0bad_c0deu64;
    let mut v = Vec::new();
    for kind in 0..14u64 {
        let size = [0usize, 1, 63, 64, 65, 4095, 70_000, 131_072, 140_001, 300_000][(kind % 10) as usize] + (kind as usize / 10) * 65_536;
        let mut d = Vec::with_capacity(size + 70_000);
        while d.len() < size {
            let r = xorshift(&mut x) % 100;
            let len = match kind % 5 {
                0 => (xorshift(&mut x) % 200) as usize,
                1 => (xorshift(&mut x) % 40) as usize,
                2 => [0usize, 1, 255, 256, 257, 1024, 1025, 3000][(xorshift(&mut x) % 8) as usize],
                3 => (xorshift(&mut x) % 600) as usize,
                _ => [8usize, 120, 66_000][(xorshift(&mut x) % 3) as usize],
            };
            for j in 0..len {
                let c = match r {
                    0..=4 => b"0123456789abcdef"[(xorshift(&mut x) % 16) as usize],
                    5..=7 => b"abcdefABCDEF"[(xorshift(&mut x) % 12) as usize],
                    8 if kind % 7 == 6 => (xorshift(&mut x) % 32) as u8,
                    9 => b"7 "[j % 2],
                    _ => LOG[j % LOG.len()],
                };
                d.push(c);
            }
            if r == 50 {
                d.push(b'\r');
            }
            d.push(b'\n');
        }
        v.push(d);
    }
    // a dense piece: more line ends than DENSE allows
    v.push(b"a\n".repeat(80_000));
    v.push(b"\n".repeat(140_000));
    v
}

/// scan_piece (line ends found first, then the lines looked at, SCAN bytes at a time) gives
/// the parts, counts and anchors the reference gives, and its line ends are every '\n' of the
/// piece (None only when they are dense).
#[test]
fn scan_piece_same_as_reference() {
    let mut x = 0x0123_4567_89ab_cdefu64;
    for d in scan_inputs() {
        let n = d.len();
        let mut ranges = vec![(0, n)];
        for _ in 0..6 {
            let a = (xorshift(&mut x) % (n as u64 + 1)) as usize;
            let b = a + (xorshift(&mut x) % ((n - a) as u64 + 1)) as usize;
            ranges.push((a, b));
        }
        for (a, b) in ranges {
            let want = scan_piece_reference(&d, a, b);
            let (got, ends) = scan_piece(&d, a, b);
            assert_eq!(got.len(), want.len(), "{n} bytes, {a}..{b}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!((g.part, g.count, &g.anchors), (w.part, w.count, &w.anchors), "{n} bytes, {a}..{b}");
            }
            let all: Vec<u32> = (a..b).filter(|&k| d[k] == b'\n').map(|k| (k - a) as u32).collect();
            match ends {
                Some(e) if looks_binary(&d[a..b]) => assert!(e.is_empty()),
                Some(e) => assert_eq!(e, all, "{n} bytes, {a}..{b}"),
                None => assert!(all.len() > (b - a) / DENSE + 64, "{n} bytes, {a}..{b}: dropped {} ends", all.len()),
            }
        }
    }
}

/// lines_from (from the first pass's line ends) gives exactly lines_of's lines, for any range,
/// and plan keeps every '\n' of the input (when it keeps them) whatever the thread count.
#[test]
fn lines_from_same_as_lines_of() {
    let mut x = 0x7777_1234_dead_0001u64;
    for d in scan_inputs() {
        let n = d.len();
        let all: Vec<u32> = (0..n).filter(|&k| d[k] == b'\n').map(|k| k as u32).collect();
        for _ in 0..20 {
            let a = (xorshift(&mut x) % (n as u64 + 1)) as usize;
            let b = a + (xorshift(&mut x) % ((n - a) as u64 + 1)) as usize;
            assert_eq!(lines_from(&d, a, b, &all), lines_of(&d, a, b), "{n} bytes, {a}..{b}");
        }
        assert_eq!(lines_from(&d, 0, n, &all), lines_of(&d, 0, n));
        for t in [1, 3] {
            let p = plan(&d, t);
            if let Some(e) = &p.ends {
                // binary pieces keep none: then every '\n' of the input outside them
                let binary = (0..n).step_by(PIECE).any(|s| looks_binary(&d[s..(s + PIECE).min(n)]));
                if !binary {
                    assert_eq!(e, &all, "{n} bytes, {t} threads");
                }
                for blk in p.blocks.iter().filter(|q| q.part.lines) {
                    assert_eq!(lines_from(&d, blk.part.a, blk.part.b, e), lines_of(&d, blk.part.a, blk.part.b));
                }
            }
        }
    }
}

/// A rewritten text damaged (1 to 3 changes: a byte from a small alphabet with the marker byte
/// and the markers' letters, a marker put in, bytes cut out, the text cut short, a byte after
/// a marker, a space after a marker taken out, a marker's letter, a line again or left out),
/// and its side stream: the text's size put right (3 in 4) or the stream damaged.
fn damage_rewrite(t0: &[u8], side0: &[u8], x: &mut u64) -> (Vec<u8>, Vec<u8>) {
    const BYTES: &[u8] = b"\x01\x01\x01  \n\nbBsfrocglwtxu0123456789-.:,()=";
    let pick = |x: &mut u64| BYTES[(xorshift(x) % BYTES.len() as u64) as usize];
    let mut t = t0.to_vec();
    let marks: Vec<usize> = (0..t.len()).filter(|&i| t[i] == 1).collect();
    for _ in 0..1 + xorshift(x) % 3 {
        let r = xorshift(x);
        let at = (xorshift(x) as usize) % (t.len() + 1);
        let mk = if marks.is_empty() { at } else { marks[(xorshift(x) as usize) % marks.len()] }.min(t.len().saturating_sub(1));
        match r % 9 {
            0 if at < t.len() => t[at] = pick(x),
            1 => {
                let l = pick(x);
                t.splice(at..at, [1u8, l]);
            }
            2 => {
                let e = (at + 1 + (xorshift(x) as usize) % 40).min(t.len());
                t.drain(at.min(e)..e);
            }
            3 => t.truncate(at),
            4 | 5 => {
                let q = mk + 1 + (xorshift(x) as usize) % 64;
                if q < t.len() {
                    t[q] = pick(x);
                }
            }
            6 => {
                if let Some(q) = t.get(mk..).and_then(|w| w.iter().position(|&c| c == b' ')) {
                    t.remove(mk + q);
                }
            }
            7 if mk + 1 < t.len() => t[mk + 1] = pick(x),
            _ => {
                let ls = t[..at].iter().rposition(|&c| c == b'\n').map_or(0, |q| q + 1);
                let le = t[at..].iter().position(|&c| c == b'\n').map_or(t.len(), |q| at + q + 1);
                if r & 512 == 0 {
                    let l = t[ls..le].to_vec();
                    t.splice(ls..ls, l);
                } else {
                    t.drain(ls..le);
                }
            }
        }
    }
    // the side stream: varint(bits), varint(text size), the rest
    let mut q = 0;
    let rd = |q: &mut usize| {
        let (mut v, mut sh) = (0u64, 0);
        while *q < side0.len() {
            let c = side0[*q];
            *q += 1;
            v |= ((c & 127) as u64) << sh;
            sh += 7;
            if c < 128 || sh > 63 {
                break;
            }
        }
        v
    };
    let bits = rd(&mut q);
    rd(&mut q);
    let mut side = varints(&[bits, t.len() as u64]);
    side.extend_from_slice(&side0[q..]);
    let r = xorshift(x);
    if r % 4 == 0 {
        match (r >> 8) % 5 {
            0 => side.push(r as u8),
            1 => {
                side.pop();
            }
            2 => side[0] = [2u8, 16, 18, 66, 3, 6, 0][(r >> 16) as usize % 7],
            3 => side = varints(&[bits, (t.len() as u64).wrapping_add([1, u64::MAX][(r >> 16) as usize % 2])]),
            _ => {
                let i = (r >> 16) as usize % side.len();
                side[i] ^= 1 << ((r >> 24) % 8);
            }
        }
    }
    (t, side)
}

/// M1: the packer's check of a derive rewrite (bridge.rs derived_ok: derive::inverts_bgl for a
/// bgl-only side stream, the opener's output compared in one piece for joined lines) answers
/// as the two steps before it (check_derived, then the opener into a zeroed buffer compared
/// line by line), on every rule's rewrite damaged at random (text and side stream), for the
/// lines as slices of the block (joined) and as copies (not joined), and for the text the
/// opener makes of the damaged rewrite (where the old answer is mostly yes), as it is and
/// with one bit changed.
/// Also check_derived itself (check_marks with find_mk, rfind and the repeated-key skip)
/// against check_marks before them, on every damaged rewrite.
fn derived_check_diff(rounds: u64, seed: u64) {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    for (name, d) in derive_inputs() {
        let lines = lines_of(&d, 0, d.len());
        let rules = eng::detect(&lines);
        let (t0, side0) = eng::rewrite_raw(&d, &lines, &rules).unwrap_or_else(|| panic!("{name}: no rewrite"));
        let copies: Vec<Vec<u8>> = lines.iter().map(|l| l.to_vec()).collect();
        let own: Vec<&[u8]> = copies.iter().map(|l| &l[..]).collect();
        assert_eq!(eng::check_answers(&d, &t0, &side0, &lines), (true, true), "{name}");
        assert_eq!(eng::check_answers(&d, &t0, &side0, &own), (true, true), "{name}");
        let (mut yes, mut no) = (0u64, 0u64);
        for round in 0..rounds {
            let (t, side) = damage_rewrite(&t0, &side0, &mut x);
            // check_derived with M1's faster searches against the one before them
            let (c1, c2) = eng::check_both(&t, &side);
            assert_eq!(c1, c2, "{name} round {round}: check_marks");
            for (k, ls) in [&lines, &own].into_iter().enumerate() {
                let (a, b) = eng::check_answers(&d, &t, &side, ls);
                assert_eq!(a, b, "{name} round {round} lines {k}: text {:?} side {side:?}", String::from_utf8_lossy(&t[..t.len().min(200)]));
            }
            if let Some(o) = eng::open_raw(&t, &side, 2 * d.len() + 4 * t.len() + 4096) {
                for flip in [false, true] {
                    let mut o = o.clone();
                    if flip && !o.is_empty() {
                        let q = (xorshift(&mut x) as usize) % o.len();
                        o[q] ^= 1 << (xorshift(&mut x) % 8);
                    }
                    let ol = lines_of(&o, 0, o.len());
                    let oc: Vec<Vec<u8>> = ol.iter().map(|l| l.to_vec()).collect();
                    let ocl: Vec<&[u8]> = oc.iter().map(|l| &l[..]).collect();
                    for (k, ls) in [&ol, &ocl].into_iter().enumerate() {
                        let (a, b) = eng::check_answers(&o, &t, &side, ls);
                        assert_eq!(a, b, "{name} round {round} opened (flip {flip}) lines {k}");
                        if a {
                            yes += 1;
                        } else {
                            no += 1;
                        }
                    }
                }
            }
        }
        eprintln!("derived check, {name}: {rounds} damaged rewrites, on the opened texts {yes} yes and {no} no, the same answers");
        assert!(rounds < 50 || (yes > 0 && no > 0), "{name}: {yes} yes, {no} no");
    }
}

#[test]
fn derived_check_same_answers() {
    derived_check_diff(300, 1);
}

/// FEWER_FUZZ=n (rounds per rule, default 20000) FEWER_SEED=s cargo test --release fuzz -- --ignored
#[test]
#[ignore]
fn fuzz_derived_check() {
    let n = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let seed = std::env::var("FEWER_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(12_345);
    derived_check_diff(n, seed);
}

/// The probe's shortcut (explosive): patterns_max is never below the patterns pack_block makes
/// (the full bound and the prefix bound), on the rule inputs, the fuzz inputs and lines of
/// random words and numbers (explosive ones), with and without their rules and their MK
/// answer, for whole blocks and probes of 4 runs.
#[test]
fn patterns_max_bounds_patterns() {
    let mut x = 5u64;
    let mut inputs: Vec<Vec<u8>> = derive_inputs().into_iter().map(|(_, d)| d).collect();
    inputs.extend(fuzz_inputs());
    let mut words = Vec::new();
    for _ in 0..3000 {
        for _ in 0..1 + xorshift(&mut x) % 8 {
            let w: Vec<u8> = (0..1 + xorshift(&mut x) % 9).map(|_| b"abcdefghijklmnopqrstuvwxyz0123456789_-./"[(xorshift(&mut x) % 40) as usize]).collect();
            words.extend_from_slice(&w);
            words.push(b' ');
        }
        words.extend_from_slice(format!("{}\n", xorshift(&mut x) % 100_000).as_bytes());
    }
    inputs.push(words);
    let mut probes = 0;
    for d in &inputs {
        let lines = lines_of(d, 0, d.len());
        let n = lines.len();
        let mut sets: Vec<Vec<&[u8]>> = vec![lines.clone()];
        if n >= 1024 {
            let mut probe = Vec::new();
            for k in 0..4 {
                let s = k * (n - 256) / 3;
                probe.extend_from_slice(&lines[s..s + 256]);
            }
            sets.push(probe);
        }
        for rules in [eng::Rules::default(), eng::detect(&lines)] {
            let mk = rules.rewrite().then(|| eng::mk_in(d));
            for ls in &sets {
                let blk = eng::pack_block(d, ls, &rules, 1, None);
                let p = blk.streams()[0].iter().filter(|&&c| c == b'\n').count();
                let (m, sure) = eng::patterns_max(d, ls, &rules, None, 0);
                assert!(p <= m, "{p} patterns, bound {m}");
                // sure: core.rs's memory guards do not decline the lines
                assert!(!sure || blk.lines() == ls.len(), "declined, but patterns_max was sure");
                // the prefix bound (asked for when it is enough)
                let (q, qsure) = eng::patterns_max(d, ls, &rules, None, usize::MAX);
                assert!(p <= q && (ls.len() > 256 || q == m), "{p} patterns, prefix bound {q}");
                assert!(!qsure || blk.lines() == ls.len(), "declined, but the prefix bound was sure");
                assert_eq!(eng::patterns_max(d, ls, &rules, mk, 0), (m, sure));
                assert!(eng::pack_block(d, ls, &rules, 1, mk).streams() == eng::pack_block(d, ls, &rules, 1, None).streams());
                probes += 1;
            }
        }
    }
    assert!(probes > 20);
}

/// mk_in (derive::has_byte, 128 bytes per step with NEON) finds an MK byte wherever it is, for
/// every length around the step sizes and every start.
#[test]
fn mk_in_same_as_bytes() {
    let mut x = 0x00c0_ffee_0000_0001u64;
    let buf: Vec<u8> = (0..1200).map(|_| (xorshift(&mut x) % 254) as u8 + 2).collect();
    for start in [0usize, 1, 7, 15, 16, 63, 64, 100] {
        for len in (0..400).chain([511, 512, 513, 1000]) {
            if start + len > buf.len() {
                continue;
            }
            let mut b = buf[start..start + len].to_vec();
            assert!(!eng::mk_in(&b));
            for at in [0, len / 3, len.saturating_sub(1), (xorshift(&mut x) as usize) % len.max(1)] {
                if at < len {
                    let keep = b[at];
                    b[at] = 1;
                    assert!(eng::mk_in(&b), "start {start} len {len} at {at}");
                    b[at] = keep;
                }
            }
        }
    }
}

/// M7: the blocks the old-against-new opener tests open: windows of 1 to 400 lines of every
/// fuzz input (all streams, rules, grouped columns, derived fields) and of the 16 *_2k logs
/// (FEWER_DATA, when there), each packed as one block. (streams, lines, size)
fn m7_blocks() -> Vec<(Vec<Vec<u8>>, usize, usize)> {
    let mut inputs = fuzz_inputs();
    // (the test data: FEWER_DATA, else ../data next to this folder)
    let dir = std::env::var("FEWER_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    if let Ok(rd) = std::fs::read_dir(format!("{dir}/loghub")) {
        let mut names: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with("_2k.log")).collect();
        names.sort();
        for p in names {
            if let Ok(d) = std::fs::read(&p) {
                inputs.push(d);
            }
        }
    }
    let mut x = 0x4d37_u64;
    let mut v = Vec::new();
    for d in &inputs {
        let all = lines_of(d, 0, d.len());
        let rules = eng::detect(&all[..all.len().min(4000)]);
        for _ in 0..24 {
            let n = 1 + (xorshift(&mut x) % 400) as usize;
            let a = (xorshift(&mut x) as usize) % all.len().saturating_sub(n).max(1);
            let ls = &all[a..(a + n).min(all.len())];
            let size: usize = ls.iter().map(|l| l.len() + 1).sum();
            let blk = eng::pack_block(d, ls, &rules, 1, None);
            v.push((blk.streams().to_vec(), ls.len(), size));
        }
    }
    v
}

/// One stream damaged 1-3 times (as fuzz_streams damages them).
fn m7_damage(s: &mut Vec<u8>, x: &mut u64) {
    for _ in 0..1 + xorshift(x) % 3 {
        let r = xorshift(x);
        let i = if s.is_empty() { 0 } else { (r >> 8) as usize % s.len() };
        match r % 8 {
            0 if !s.is_empty() => s[i] ^= 1 << ((r >> 40) % 8),
            1 => s.truncate(i),
            2 => s.insert(i, (r >> 32) as u8),
            3 if !s.is_empty() => s[i] = [0xFF, 0, 1, b'\n'][(r >> 44) as usize % 4],
            4 if !s.is_empty() => {
                let v = varints(&[(r >> 16) % (1 << (r % 60))]);
                s.splice(i..(i + 1).min(s.len()), v);
            }
            5 => {
                let j = (r >> 20) as usize % (s.len() + 1);
                let piece: Vec<u8> = s[i.min(j)..i.max(j)].to_vec();
                s.splice(i..i, piece);
            }
            6 if !s.is_empty() => s[i] = s[i].wrapping_add(1 + (r >> 50) as u8 % 3),
            _ if !s.is_empty() => {
                s.remove(i);
            }
            _ => {}
        }
    }
}

/// M7, old against new opener: blocks whose streams are damaged (CRC-correct, as an archive
/// could hold them) and pass check_block are opened by the opener before M7 (open_block_old:
/// ref's open.rs, check_derived, inverse) and by today's (open_block). Both must refuse (an
/// error, a panic, or a wrong length, which fewer-pack refuses), or both give the same bytes.
/// FEWER_DIFF=n damaged blocks (default 1,000,000), FEWER_SEED=s:
///   FEWER_DIFF=1000000 cargo test --release m7_old_new -- --ignored --nocapture
#[test]
#[ignore]
fn m7_old_new_opener() {
    let n: usize = std::env::var("FEWER_DIFF").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let seed: u64 = std::env::var("FEWER_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(m7_seed());
    // every block opens exactly with today's opener; the opener before M7 reads format 1 only
    // (engine id 5), so the blocks that use a part of format 2 (ID columns, keyed slots, the
    // pattern id forms, computed bgl / ctime parts) are left out: on the others both must open
    // exactly the same, before any damage
    let mut blocks = Vec::new();
    let mut format2 = 0usize;
    for (k, (s, lines, size)) in m7_blocks().into_iter().enumerate() {
        let (mut a, mut b) = (vec![0u8; size + 64], vec![0u8; size + 64]);
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        assert!(eng::check_block(&s, lines, size).is_ok(), "block {k}: check_block");
        assert_eq!(eng::open_block(s.clone(), lines, &mut a, size, &mut sa), Ok(size), "block {k}: new");
        let old = guarded(|| eng::open_block_old(s.clone(), lines, &mut b, size, &mut sb));
        if old != Some(Ok(size)) || a[..size] != b[..size] {
            format2 += 1;
            continue;
        }
        blocks.push((s, lines, size));
    }
    eprintln!("m7_old_new_opener: {} blocks in format 1, {format2} using format 2 left out", blocks.len());
    assert!(blocks.len() >= 8, "too few format 1 blocks: {}", blocks.len());
    let threads = 8;
    let per = n.div_ceil(threads);
    let t0 = std::time::Instant::now();
    let res: Vec<[usize; 5]> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|th| {
                let blocks = &blocks;
                sc.spawn(move || {
                    let mut x = (seed.wrapping_add(1).wrapping_mul(0xd6e8_feb8_6659_fd93) ^ (th as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1;
                    // [checked out by check_block, both refuse, both the same bytes, of those the
                    // input, read differently: the damage made a format 2 part, which only today's
                    // opener reads (a pattern id form, ID columns, keyed slots, computed parts)]
                    let mut c = [0usize; 5];
                    let (mut sa, mut sb) = (Vec::new(), Vec::new());
                    for i in 0..per.min(n.saturating_sub(th * per)) {
                        let (s0, lines, size) = &blocks[(xorshift(&mut x) as usize) % blocks.len()];
                        let mut s = s0.clone();
                        let k = (xorshift(&mut x) as usize) % s.len();
                        m7_damage(&mut s[k], &mut x);
                        if xorshift(&mut x) % 8 == 0 {
                            let k2 = (xorshift(&mut x) as usize) % s.len();
                            m7_damage(&mut s[k2], &mut x);
                        }
                        if eng::check_block(&s, *lines, *size).is_err() {
                            c[0] += 1;
                            continue;
                        }
                        let (mut a, mut b) = (vec![0u8; size + 64], vec![0u8; size + 64]);
                        let t = std::time::Instant::now();
                        let new = guarded(|| eng::open_block(s.clone(), *lines, &mut a, *size, &mut sa));
                        let old = guarded(|| eng::open_block_old(s.clone(), *lines, &mut b, *size, &mut sb));
                        assert!(t.elapsed().as_secs() < 5, "slow open (case {i}, thread {th})");
                        let got = |r: Option<Result<usize, &str>>, o: &[u8]| match r {
                            Some(Ok(e)) if e == *size => Some(o[..e].to_vec()),
                            _ => None,
                        };
                        let (gn, go) = (got(new, &a), got(old, &b));
                        if gn != go {
                            c[4] += 1;
                            continue;
                        }
                        if let Some(o) = go {
                            c[2] += 1;
                            let orig = {
                                let mut q = vec![0u8; size + 64];
                                eng::open_block(s0.clone(), *lines, &mut q, *size, &mut Vec::new()).unwrap();
                                q.truncate(*size);
                                q
                            };
                            c[3] += (o == orig) as usize;
                        } else {
                            c[1] += 1;
                        }
                    }
                    c
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut c = [0usize; 5];
    for r in res {
        for i in 0..5 {
            c[i] += r[i];
        }
    }
    eprintln!("m7 old/new opener: {n} damaged blocks ({} blocks, seed {seed}): {} refused by check_block; of the rest, both refuse {}, both the same bytes {} ({} of them the input), read differently (format 2) {}; no panic outside the guard, none slow; {:?}", blocks.len(), c[0], c[1], c[2], c[3], c[4], t0.elapsed());
    assert!(c[1] > 0 && c[2] > 0);
}

const fn m7_seed() -> u64 {
    0x5eed_0007
}

/// M7, the fused bgl check (derived_back, open side): rewrites of BGL lines, damaged (text and
/// side stream), give the same answer and bytes as check_derived then the inverse.
/// FEWER_FUZZ=n (default 20000) FEWER_SEED=s cargo test --release fuzz -- --ignored
#[test]
#[ignore]
fn m7_fused_bgl_fuzz() {
    let n: u64 = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let seed: u64 = std::env::var("FEWER_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(12_345);
    m7_fused_bgl(n, seed);
}

#[test]
fn m7_fused_bgl_same() {
    m7_fused_bgl(400, 0x9e37_79b9_7f4a_7c15);
}

fn m7_fused_bgl(n: u64, seed: u64) {
    fn put(v: &mut Vec<u8>, mut x: u64) {
        while x >= 128 {
            v.push(x as u8 | 128);
            x >>= 7;
        }
        v.push(x as u8);
    }
    fn padded(t: &[u8]) -> Vec<u8> {
        let mut v = t.to_vec();
        v.resize(t.len() + 128, 0);
        v
    }
    #[allow(clippy::type_complexity)]
    fn open_with(f: fn(&[u8], usize, &[u8], &mut [u8]) -> Result<usize, &'static str>, t: &[u8], side: &[u8], room: usize) -> Option<Vec<u8>> {
        let (text, mut out) = (padded(t), vec![0u8; room]);
        let o = guarded(|| f(&text, t.len(), side, &mut out))?.ok()?;
        // (an end past out: refused, fix3's room checks)
        out.get(..o).map(|v| v.to_vec())
    }
    let data = derive_inputs().into_iter().next().unwrap().1;
    let lines = lines_of(&data, 0, data.len());
    // the rewritten text and its side stream as the put-back reads them (format 2 marks the
    // side stream of a block whose markers are all in the patterns' fixed text for the line
    // printer instead, which this input is: not that one)
    let (t0, side0) = eng::rewritten(&data, &lines, &eng::detect(&lines)).expect("the bgl input has derived fields");
    let mut p = 0;
    let rd = |p: &mut usize| -> u64 {
        let (mut v, mut sh) = (0u64, 0);
        loop {
            let b = side0[*p];
            *p += 1;
            v |= ((b & 127) as u64) << sh;
            if b < 128 {
                return v;
            }
            sh += 7;
        }
    };
    let bits = rd(&mut p);
    let tsize = rd(&mut p) as usize;
    assert_eq!(bits, 2, "a bgl-only side stream");
    let offs = [rd(&mut p), rd(&mut p)];
    assert_eq!(tsize, t0.len());
    let size = data.len() + (data.last() != Some(&b'\n')) as usize;
    assert_eq!(open_with(eng::derived_back_old, &t0, &side0, size + 64).map(|v| v.len()), Some(size));
    assert_eq!(open_with(eng::derived_back_new, &t0, &side0, size + 64), open_with(eng::derived_back_old, &t0, &side0, size + 64));
    // fix3: the same lines in the old form (side stream bits 2 | 8: no fill marker), damaged
    // in every other round
    let (t1, side1) = eng::rewritten(&data, &lines, &eng::detect(&lines).old_bgl().expect("bgl rules")).unwrap();
    assert_eq!(side1[0], 2 | 8);
    assert_eq!(open_with(eng::derived_back_old, &t1, &side1, size + 64).map(|v| v.len()), Some(size));
    assert_eq!(open_with(eng::derived_back_new, &t1, &side1, size + 64), open_with(eng::derived_back_old, &t1, &side1, size + 64));
    let marks0: Vec<usize> = (0..t0.len()).filter(|&i| t0[i] == 1).collect();
    let marks1: Vec<usize> = (0..t1.len()).filter(|&i| t1[i] == 1).collect();
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let (mut same_ok, mut same_no) = (0u64, 0u64);
    const BYTES: &[u8] = b"\x01\x01  \n\nbB0123456789-.:xR";
    for round in 0..n {
        let (mut t, marks, bits0) = if round % 2 == 0 { (t0.clone(), &marks0, 2) } else { (t1.clone(), &marks1, 2 | 8) };
        for _ in 0..1 + xorshift(&mut x) % 3 {
            let r = xorshift(&mut x);
            let at = (xorshift(&mut x) as usize) % (t.len() + 1);
            let mk = marks[(xorshift(&mut x) as usize) % marks.len()].min(t.len().saturating_sub(1));
            match r % 9 {
                0 if at < t.len() => t[at] = BYTES[(xorshift(&mut x) as usize) % BYTES.len()],
                1 => {
                    let l = b"bBx eE"[(xorshift(&mut x) % 6) as usize];
                    t.splice(at..at, [1u8, l]);
                }
                2 => {
                    let e = (at + 1 + (xorshift(&mut x) as usize) % 40).min(t.len());
                    t.drain(at.min(e)..e);
                }
                3 => t.truncate(at),
                4 | 5 => {
                    let q = mk + 2 + (xorshift(&mut x) as usize) % 64;
                    if q < t.len() {
                        t[q] = BYTES[(xorshift(&mut x) as usize) % BYTES.len()];
                    }
                }
                6 => {
                    if let Some(q) = t.get(mk..).and_then(|w| w.iter().position(|&c| c == b' ')) {
                        t.remove(mk + q);
                    }
                }
                7 if mk + 1 < t.len() => t[mk + 1] ^= b'b' ^ b'B',
                _ => {
                    let ls = t[..at].iter().rposition(|&c| c == b'\n').map_or(0, |q| q + 1);
                    let le = t[at..].iter().position(|&c| c == b'\n').map_or(t.len(), |q| at + q + 1);
                    if r & 512 == 0 {
                        let l = t[ls..le].to_vec();
                        t.splice(ls..ls, l);
                    } else {
                        t.drain(ls..le);
                    }
                }
            }
        }
        let mut side = Vec::new();
        let r = xorshift(&mut x);
        if r % 4 == 0 {
            let bits = [2u64, 0, 2 | 16, 3, 6, 2 | 256, 2 | 8, 8, 8 | 16, 2 | 8 | 256, bits0 ^ 8][(r >> 8) as usize % 11];
            put(&mut side, bits);
            put(&mut side, (t.len() as u64).wrapping_add([0, 1, u64::MAX][(r >> 16) as usize % 3]));
            for k in 0..2 {
                put(&mut side, if r >> (20 + k) & 1 == 0 { offs[k] } else { xorshift(&mut x) >> (xorshift(&mut x) % 64) });
            }
            if r >> 30 & 3 == 0 {
                side.push(xorshift(&mut x) as u8);
            } else if r >> 30 & 3 == 1 {
                side.pop();
            }
        } else {
            put(&mut side, bits0);
            put(&mut side, t.len() as u64);
            put(&mut side, offs[0]);
            put(&mut side, offs[1]);
        }
        let room = if r >> 40 & 1 == 0 { size + 64 } else { 3 * t.len() + 4096 };
        let (old, new) = (open_with(eng::derived_back_old, &t, &side, room), open_with(eng::derived_back_new, &t, &side, room));
        assert_eq!(old, new, "fused bgl: old and new differ");
        if old.is_some() {
            same_ok += 1;
        } else {
            same_no += 1;
        }
    }
    eprintln!("m7 fused bgl: {n} damaged rewrites: both open {same_ok}, both refuse {same_no}; no difference");
    assert!(same_ok > 0 && same_no > 0);
}

/// explosive (pass 1's bound first, eng::patterns_max) answers as packing the probe in full
/// does (a probe that pack_block's memory guard declines counts as explosive): lines of 100
/// numbers whose separators make most lines a pattern of their own (more than COLS_FLOOR
/// columns in a probe whose lines are under 90% patterns of their own: declined), the same
/// with fewer numbers (not declined, not explosive), and a log-like input.
#[test]
fn explosive_same_as_full_probe() {
    let full = |input: &[u8], lines: &[&[u8]], rules: &eng::Rules, alone: bool| -> bool {
        let n = lines.len();
        if n < PROBE_LINES || input.len() < PROBE_BYTES {
            return false;
        }
        let mut probe: Vec<&[u8]> = Vec::new();
        for k in 0..PROBE_RUNS {
            let s = k * (n - PROBE_RUN) / (PROBE_RUNS - 1);
            probe.extend_from_slice(&lines[s..s + PROBE_RUN]);
        }
        let patterns = guarded(|| {
            let b = eng::pack_block_as(input, &probe, rules, 1, alone, None);
            if b.lines() != probe.len() { usize::MAX } else { b.streams()[0].iter().filter(|&&c| c == b'\n').count() }
        });
        patterns.is_some_and(|p| p == usize::MAX || p * 10 > probe.len() * 9)
    };
    let mut x = 0x5eed_0000_e791_0517u64;
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    for nums in [100usize, 12] {
        let mut d = Vec::new();
        let mut last = Vec::new();
        while d.len() < (5 << 20) {
            let i = d.len();
            let mut l = Vec::new();
            if i % 7 == 3 && !last.is_empty() {
                l = last.clone();
            } else {
                for j in 0..nums {
                    l.extend_from_slice(format!("{}", xorshift(&mut x) % 1000).as_bytes());
                    if j + 1 < nums {
                        l.push(if xorshift(&mut x) % 2 == 0 { b',' } else { b';' });
                    }
                }
            }
            d.extend_from_slice(&l);
            d.push(b'\n');
            last = l;
        }
        inputs.push(d);
    }
    let mut d = Vec::new();
    while d.len() < (5 << 20) {
        let k = xorshift(&mut x);
        d.extend_from_slice(format!("2026-11-15 12:{:02}:{:02} INFO worker-{} task {} done in {} ms\n", k % 60, (k >> 8) % 60, (k >> 16) % 9, (k >> 20) % 100_000, (k >> 40) % 5000).as_bytes());
    }
    inputs.push(d);
    let mut seen = [0usize; 2];
    for d in &inputs {
        let lines = lines_of(d, 0, d.len());
        let rules = eng::detect(&lines);
        let mk = rules.rewrite().then(|| eng::mk_in(d));
        for alone in [false, true] {
            let want = full(d, &lines, &rules, alone);
            assert_eq!(explosive(d, 0, d.len(), &lines, &rules, alone, mk), want);
            seen[want as usize] += 1;
        }
    }
    assert!(seen[0] > 0 && seen[1] > 0, "{seen:?}");
}

/// Stream 1 (the pattern ids) read in its three forms, written
/// independently of core.rs read_ids (refute_b3): None when it does not hold `lines` ids.
fn ref_read_ids(b: &[u8], lines: usize) -> Option<Vec<u32>> {
    let (form, mut p) = match b {
        [0x81, 0, ..] => (1u64, 2usize),
        [0x82, 0, ..] => (2, 2),
        _ => (0, 0),
    };
    let rd = |p: &mut usize| -> Option<u64> {
        let (mut v, mut s) = (0u64, 0u32);
        loop {
            let x = *b.get(*p)?;
            *p += 1;
            if s >= 64 {
                return None;
            }
            v |= ((x & 0x7f) as u64) << s;
            if x < 0x80 {
                return Some(v);
            }
            s += 7;
        }
    };
    let (mut follow, mut ids, mut seen, mut prev) = (HashMap::new(), Vec::new(), 0u32, None::<u32>);
    for _ in 0..lines {
        let c = rd(&mut p)?;
        let t = if c >= form {
            u32::try_from(c - form).ok()?
        } else if c + 1 == form {
            seen
        } else {
            *follow.get(&prev?)?
        };
        if let Some(q) = prev {
            follow.insert(q, t);
        }
        seen += (t == seen) as u32;
        prev = Some(t);
        ids.push(t);
    }
    Some(ids)
}

/// The codes of `ids` in form 0 (plain), 1 or 2, without the marker.
fn ref_id_codes(ids: &[u32], form: u64) -> Vec<u64> {
    let (mut follow, mut seen, mut prev, mut out) = (HashMap::new(), 0u32, None::<u32>, Vec::new());
    for &t in ids {
        let c = if form == 2 && prev.and_then(|q| follow.get(&q)) == Some(&t) {
            0
        } else if form > 0 && t == seen {
            form - 1
        } else {
            t as u64 + form
        };
        out.push(c);
        if let Some(q) = prev {
            follow.insert(q, t);
        }
        seen += (t == seen) as u32;
        prev = Some(t);
    }
    out
}

fn ref_id_stream(codes: &[u64], form: u64) -> Vec<u8> {
    let mut o = if form > 0 { vec![0x80 | form as u8, 0] } else { Vec::new() };
    for &c in codes {
        put_varint(&mut o, c);
    }
    o
}

/// The pattern-id forms of format 2 through the whole opener (refute_b3): a block's stream 1
/// replaced (as a stored frame) by the same ids in each form, written by the reference writer
/// above, opens exactly; replaced by hostile forms (a guess on the first line or with no
/// follower, the next new pattern past the count, ids past the count, unknown markers, cut
/// short, longer, huge varints, the marker swapped, random valid ids, damage) with crossed
/// line counts and pattern lists, it is refused or opens exactly: never a wrong text or a
/// slow open; every panic is counted by place (FEWER_FUZZ rounds per input, default 300).
#[test]
#[ignore]
fn fuzz_id_forms_open() {
    let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let dir = std::env::var("FEWER_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    let mut inputs: Vec<(String, Vec<u8>)> = Vec::new();
    for n in ["Android", "BGL", "Mac", "Proxifier", "Thunderbird", "Linux", "HPC", "Zookeeper", "Spark", "Hadoop", "HealthApp", "OpenSSH", "Apache", "HDFS", "OpenStack", "Windows"] {
        if let Ok(d) = std::fs::read(format!("{dir}/loghub/{n}_2k.log")) {
            inputs.push((n.into(), d));
        }
    }
    for (i, d) in fuzz_inputs().into_iter().enumerate() {
        inputs.push((format!("fuzz_input{i}"), d));
    }
    // a pack first installs guarded's hook; then every panic is counted by its place
    let _ = pack(b"a 1\nb 2\n", 1);
    static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let at = info.location().map_or("?".into(), |l| format!("{}:{}", l.file(), l.line()));
        PANICS.lock().unwrap().push(at);
    }));
    let mut x = 0xb3b3_u64;
    let (mut exact, mut refused, mut forms_seen) = (0usize, 0usize, [0usize; 3]);
    let t0 = std::time::Instant::now();
    for (name, d) in &inputs {
        let p = pack(d, 2);
        if block_kinds(&p).1 != [KIND_LINES] {
            eprintln!("{name}: not one lines block, skipped");
            continue;
        }
        let q = split_one_block(&p);
        let raw = d.len() as u64;
        let lines = q.lines as usize;
        let s1 = eng::decode(&q.frames[1], 64 << 20, None).unwrap();
        let form_used = match s1.as_slice() {
            [0x81, 0, ..] => 1,
            [0x82, 0, ..] => 2,
            _ => 0,
        };
        forms_seen[form_used] += 1;
        let ids = ref_read_ids(&s1, lines).expect("reference reader: the packer's stream 1");
        let list = eng::decode(&q.frames[0], 64 << 20, None).unwrap();
        let pats = list.iter().filter(|&&c| c < 0x80).count();
        assert!(ids.iter().all(|&t| (t as usize) < pats), "{name}: an id past the pattern count");
        let open = |frames1: Vec<u8>, lines_field: u64, list0: Option<Vec<u8>>, threads: usize| -> Result<(), String> {
            let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), lines: lines_field, ..q };
            h.frames[1] = eng::store(&frames1);
            if let Some(l) = list0 {
                h.frames[0] = eng::store(&l);
            }
            let pay = join_one_block(raw, &h);
            let t = std::time::Instant::now();
            let r = catch_unwind(AssertUnwindSafe(|| unpack(&pay, threads)));
            assert!(t.elapsed().as_secs() < 5, "{name}: slow open {:?}", t.elapsed());
            match r {
                Ok(Ok(back)) => {
                    assert!(back == *d, "{name}: WRONG text from a crafted stream 1");
                    Ok(())
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err("panic outside guarded".into()),
            }
        };
        // every form of the true ids opens exactly
        let codes: Vec<Vec<u64>> = (0..3).map(|f| ref_id_codes(&ids, f)).collect();
        for f in 0..3u64 {
            open(ref_id_stream(&codes[f as usize], f), q.lines, None, 1 + f as usize).unwrap_or_else(|e| panic!("{name}: form {f} of the true ids refused: {e}"));
        }
        // hostile forms, each refused
        let mut must_refuse: Vec<(String, Vec<u8>)> = Vec::new();
        let mut c2 = codes[2].clone();
        c2[0] = 0;
        must_refuse.push(("guess on line 0".into(), ref_id_stream(&c2, 2)));
        // a guess on the first line whose previous pattern has had no follower
        let mut followed = std::collections::HashSet::new();
        for i in 1..lines {
            if !followed.contains(&ids[i - 1]) {
                let mut c = codes[2].clone();
                c[i] = 0;
                must_refuse.push((format!("guess with no follower at line {i}"), ref_id_stream(&c, 2)));
                break;
            }
            followed.insert(ids[i - 1]);
        }
        // the next new pattern once every pattern is used
        let mut seen = 0u32;
        for i in 0..lines {
            if seen as usize == pats && i > 0 {
                for f in 1..3u64 {
                    let mut c = codes[f as usize].clone();
                    c[i] = f - 1;
                    must_refuse.push((format!("next new pattern past the count (form {f}, line {i})"), ref_id_stream(&c, f)));
                }
                break;
            }
            seen += (ids[i] == seen) as u32;
        }
        for f in 0..3u64 {
            let mut c = codes[f as usize].clone();
            let i = (xorshift(&mut x) as usize) % lines;
            c[i] = pats as u64 + f;
            must_refuse.push((format!("id = pattern count (form {f})"), ref_id_stream(&c, f)));
            c[i] = u64::MAX;
            must_refuse.push((format!("id u64::MAX (form {f})"), ref_id_stream(&c, f)));
            let mut s = ref_id_stream(&codes[f as usize], f);
            s.truncate(s.len() - 1);
            must_refuse.push((format!("last byte cut (form {f})"), s));
            // an 11-byte varint
            let mut s = ref_id_stream(&codes[f as usize][..lines / 2], f);
            s.extend_from_slice(&[0xff; 10]);
            s.push(1);
            s.extend_from_slice(&ref_id_stream(&codes[f as usize][lines / 2 + 1..], 0));
            must_refuse.push((format!("an 11-byte varint (form {f})"), s));
        }
        for m in [0x80u8, 0x83, 0x84, 0x8f, 0xc1, 0xff] {
            let mut s = vec![m, 0];
            s.extend_from_slice(&ref_id_stream(&codes[0], 0));
            must_refuse.push((format!("marker {m:#x}"), s));
        }
        for (what, s) in &must_refuse {
            match open(s.clone(), q.lines, None, 1 + (xorshift(&mut x) % 3) as usize) {
                Ok(()) => panic!("{name}: {what}: accepted"),
                Err(_) => refused += 1,
            }
        }
        // the marker swapped (another reading of valid codes): refused or exact
        for (f, g) in [(1u64, 2u64), (2, 1), (1, 0), (2, 0), (0, 1), (0, 2)] {
            let s = ref_id_stream(&codes[f as usize], g);
            match open(s, q.lines, None, 2) {
                Ok(()) => exact += 1,
                Err(_) => refused += 1,
            }
        }
        // random rounds: random valid or damaged forms, crossed line counts and pattern lists
        for round in 0..rounds {
            let f = xorshift(&mut x) % 3;
            let mut v = ids.clone();
            match xorshift(&mut x) % 4 {
                0 => {}
                1 => {
                    // some ids changed to other valid ids (crossed counts per pattern)
                    for _ in 0..1 + xorshift(&mut x) % 8 {
                        let i = (xorshift(&mut x) as usize) % lines;
                        v[i] = (xorshift(&mut x) % pats as u64) as u32;
                    }
                }
                2 => v = (0..lines).map(|_| (xorshift(&mut x) % pats as u64) as u32).collect(),
                _ => {
                    let i = (xorshift(&mut x) as usize) % lines;
                    let j = (xorshift(&mut x) as usize) % lines;
                    v.swap(i, j);
                }
            }
            let mut s = ref_id_stream(&ref_id_codes(&v, f), f);
            for _ in 0..xorshift(&mut x) % 3 {
                let r = xorshift(&mut x);
                let i = if s.is_empty() { 0 } else { (r >> 8) as usize % s.len() };
                match r % 7 {
                    0 if !s.is_empty() => s[i] ^= 1 << ((r >> 40) % 8),
                    1 => s.truncate(i.max(2)),
                    2 => s.insert(i, (r >> 32) as u8),
                    3 if !s.is_empty() => s[i] = ((r >> 32) % 3) as u8,
                    4 if !s.is_empty() => {
                        s.remove(i);
                    }
                    5 => s.extend_from_slice(&[(r >> 24) as u8, 0]),
                    _ => {}
                }
            }
            let r = xorshift(&mut x);
            let lf = match r % 6 {
                0 => q.lines + 1 + (r >> 8) % 3,
                1 => q.lines.saturating_sub(1 + (r >> 8) % 3).max(1),
                _ => q.lines,
            };
            let l0 = match (r >> 16) % 6 {
                0 if pats > 1 => {
                    // one pattern fewer: the list without its last entry
                    let mut l = list.clone();
                    l.pop();
                    while l.last().is_some_and(|&c| c >= 0x80) {
                        l.pop();
                    }
                    Some(l)
                }
                1 => {
                    // one pattern more (the next table entry, or one past the table)
                    let mut l = list.clone();
                    l.push(2);
                    Some(l)
                }
                _ => None,
            };
            match open(s, lf, l0, 1 + round % 3) {
                Ok(()) => exact += 1,
                Err(_) => refused += 1,
            }
        }
    }
    std::panic::set_hook(prev);
    let panics = PANICS.lock().unwrap().clone();
    let mut by: HashMap<String, usize> = HashMap::new();
    for p in &panics {
        *by.entry(p.clone()).or_default() += 1;
    }
    eprintln!("fuzz_id_forms_open: {} inputs (packer's forms plain/1/2: {forms_seen:?}), {exact} exact, {refused} refused, {} panics caught by guarded, {:?}", inputs.len(), panics.len(), t0.elapsed());
    let mut by: Vec<_> = by.into_iter().collect();
    by.sort();
    for (at, n) in &by {
        eprintln!("  panic at {at}: {n}");
    }
}

/// The pattern-id forms never turn a block that format 1 keeps as plain bytes into a bigger
/// lines block (refute_b3): 200 lines with a word of their own, cycled twice. Format 1 keeps
/// them as plain bytes (3,239 bytes in fewer-pack); choosing lines or plain bytes with the smallest
/// form made it a lines block of 3,605 bytes (+11%). The choice is made with the plain form.
#[test]
fn pattern_forms_never_turn_plain_into_lines() {
    let mut x = 1u64;
    let word = |x: &mut u64, n: u64| -> String {
        let v = xorshift(x);
        (0..(v % n + if n == 7 { 3 } else { 0 })).map(|j| (b'a' + ((v >> (4 * j)) % 10) as u8) as char).collect()
    };
    let mut pool = Vec::new();
    for _ in 0..200 {
        let w = word(&mut x, 7);
        let h = xorshift(&mut x) % 65536;
        let n = xorshift(&mut x) % 1_000_000;
        let t = word(&mut x, 13);
        pool.push(format!("INFO {w} {h:04x} {n} {t}\n"));
    }
    let d = pool.concat().repeat(2).into_bytes();
    for t in [1, 3] {
        let p = pack(&d, t);
        assert_eq!(block_kinds(&p).1, [KIND_PLAIN], "{t} threads");
        assert!(unpack(&p, t).unwrap() == d);
    }
}

// ---------------------------------------------------------------- ID columns (format 2, ids.rs)

/// Inputs whose blocks have ID columns: HDFS-like block ids made by java.util.Random (each
/// seen a few times, out of order, with a few long ids of other kinds), and OpenStack-like
/// UUIDs (random version-4 ones, repeats, time-based ones, upper case).
fn id_inputs() -> Vec<Vec<u8>> {
    let mut x = 991u64;
    let mut seed = 0x1234_5678_9abcu64;
    let mut next_long = || {
        let mut half = || {
            seed = (seed.wrapping_mul(0x5DEECE66D).wrapping_add(0xB)) & ((1 << 48) - 1);
            (seed >> 16) as u32 as i32 as i64
        };
        let hi = half();
        (hi << 32).wrapping_add(half())
    };
    let mut ids: Vec<i64> = Vec::new();
    let mut hdfs = Vec::new();
    for i in 0..6000u64 {
        if i % 3 == 0 || ids.is_empty() {
            ids.push(next_long());
        }
        let r = xorshift(&mut x);
        let id = if r % 9 == 0 { 1_000_000_000_000_000 + (r >> 8) % 1_000_000_000_000 } else { 0 };
        // (a few block ids that the generator did not make, in the same places)
        let b = if r % 13 == 5 { 4_000_000_000_000_000_000 + (r >> 4) as i64 % 1_000_000_000 } else { ids[ids.len() - 1 - (r >> 20) as usize % ids.len().min(40)] };
        let line = if id != 0 {
            format!("081109 2035{:02} {} INFO dfs.DataNode$PacketResponder: other id {id} of blk_{b} size {}\n", i % 60, 100 + i % 50, r % 70000)
        } else {
            format!("081109 2035{:02} {} INFO dfs.DataNode$PacketResponder: Received block blk_{b} of size {} from /10.250.{}.{}\n", i % 60, 100 + i % 50, r % 70000, r % 20, (r >> 9) % 256)
        };
        hdfs.extend_from_slice(line.as_bytes());
    }
    let mut uu: Vec<String> = Vec::new();
    let mut os = Vec::new();
    for i in 0..5000u64 {
        let (a, b) = (xorshift(&mut x), xorshift(&mut x));
        if i % 2 == 0 || uu.is_empty() {
            let v = (i % 7) != 0;
            let s = format!("{:08x}-{:04x}-{}{:03x}-{:x}{:03x}-{:012x}", a as u32, (a >> 32) as u16, if v { 4 } else { 1 }, (a >> 48) % 4096, 8 + (b % 4), (b >> 8) % 4096, (b >> 20) % (1 << 48));
            uu.push(if i % 11 == 0 { s.to_uppercase() } else { s });
        }
        let u = &uu[uu.len() - 1 - (b >> 30) as usize % uu.len().min(25)];
        os.extend_from_slice(format!("nova-compute.log 2017-05-16 00:{:02}:{:02}.{:03} 2931 INFO nova.compute.manager [req-{u} - - -] [instance: {}] VM Started (Lifecycle Event) took {}.{:02} seconds\n", i / 60 % 60, i % 60, i % 1000, uu[(a >> 40) as usize % uu.len()], b % 30, a % 100).as_bytes());
    }
    vec![hdfs, os]
}

/// The column header's flags byte (heads.rs layout: varint columns, byte, byte flags).
fn head_flags_at(h: &[u8]) -> usize {
    let mut r = Rd::new(h);
    r.varint().unwrap();
    r.p + 1
}

/// ID columns: the inputs use them (header flag 32), open exactly at 1 and 3 threads with the
/// same payload, and crafted damage to their parts (flags, the UUIDs at the end of the text
/// stream, a grouped ID column) is refused at once.
#[test]
fn id_columns_round_trip_and_refuse_damage() {
    for d in id_inputs() {
        let p = pack(&d, 1);
        assert!(pack(&d, 3) == p, "the same bytes on 3 threads");
        assert!(unpack(&p, 1).unwrap() == d && unpack(&p, 3).unwrap() == d);
        let lines = lines_of(&d, 0, d.len());
        let mut blk = eng::pack_block(&d, &lines, &eng::Rules::default(), 1, None);
        let streams: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
        let fa = head_flags_at(&streams[2]);
        assert!(streams[2][fa] & 32 != 0, "no ID columns");
        assert!(open_crafted(streams.clone(), lines.len(), d.len()).unwrap() == d);
        let mut cases: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
        for (name, f) in [("flag 32 cleared", streams[2][fa] & !32), ("flag 64 set", streams[2][fa] | 64)] {
            let mut s = streams.clone();
            s[2][fa] = f;
            cases.push((name.into(), s));
        }
        // the text stream's end (the ID columns' new UUIDs): cut short, or with bytes left over
        for k in [1usize, 2, 8, 15, 16, 17, 40] {
            if k <= streams[5].len() {
                let mut s = streams.clone();
                s[5].truncate(streams[5].len() - k);
                cases.push((format!("texts cut by {k}"), s));
            }
            let mut s = streams.clone();
            s[5].extend(std::iter::repeat_n(0xA5u8, k));
            cases.push((format!("texts {k} bytes longer"), s));
        }
        for (name, s) in cases {
            let t = std::time::Instant::now();
            let r = open_crafted(s, lines.len(), d.len());
            assert!(t.elapsed().as_secs_f64() < 2.0, "{name}: too slow");
            assert!(r.is_err(), "{name}: opened");
        }
    }
}

/// Fuzzing of the ID columns, run with: cargo test --release fuzz_ids -- --ignored
/// (FEWER_FUZZ=N rounds per input, 3000 by default): the streams ID columns use (header,
/// numbers, texts with the new UUIDs at their end, cache codes, differences) damaged after
/// their frames are opened: truncated, longer, bytes changed, big varints (huge numbers,
/// dictionary places past its end, crossed counts), pieces copied. Every result must be an
/// error or the exact text, never a crash, a hang or huge memory.
#[test]
#[ignore]
fn fuzz_ids() {
    let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let mut x = 0x1d5_u64;
    let (mut errs, mut oks) = (0usize, 0usize);
    let t0 = std::time::Instant::now();
    for d in id_inputs() {
        let p = pack(&d, 2);
        assert!(unpack(&p, 2).unwrap() == d);
        let q = split_one_block(&p);
        let raw = d.len() as u64;
        let opened: Vec<Vec<u8>> = q.frames.iter().map(|f| if f.is_empty() { Vec::new() } else { eng::decode(f, 64 << 20, None).unwrap() }).collect();
        assert!(opened[2][head_flags_at(&opened[2])] & 32 != 0, "no ID columns");
        for round in 0..rounds {
            let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
            let pick = [2usize, 4, 5, 5, 5, 6, 7][(xorshift(&mut x) % 7) as usize];
            let mut s = opened[pick].clone();
            for _ in 0..1 + xorshift(&mut x) % 3 {
                let r = xorshift(&mut x);
                // (texts: mostly at their end, where the new UUIDs are)
                let n = s.len();
                let i = if n == 0 { 0 } else if pick == 5 && r % 2 == 0 { n - 1 - (r >> 8) as usize % n.min(64) } else { (r >> 8) as usize % n };
                match (r >> 4) % 7 {
                    0 if n > 0 => s[i] ^= 1 << ((r >> 40) % 8),
                    1 => s.truncate(i),
                    2 => s.insert(i, (r >> 32) as u8),
                    3 if n > 0 => s[i] = 0xFF,
                    4 if n > 0 => {
                        let v = varints(&[(r >> 16) % (1 << (r % 60))]);
                        s.splice(i..(i + 1).min(n), v);
                    }
                    5 => {
                        let j = (r >> 20) as usize % (n + 1);
                        let piece: Vec<u8> = s[i.min(j)..i.max(j)].to_vec();
                        s.splice(i..i, piece);
                    }
                    _ => s.extend(std::iter::repeat_n((r >> 24) as u8, 1 + (r >> 50) as usize % 20)),
                }
            }
            h.frames[pick] = if s.is_empty() && round % 2 == 0 { Vec::new() } else { eng::store(&s) };
            let pay = join_one_block(raw, &h);
            let t = std::time::Instant::now();
            match unpack(&pay, 1 + round % 3) {
                Ok(back) => {
                    assert!(back == d, "fuzz_ids: wrong output (stream {pick}, round {round})");
                    oks += 1;
                }
                Err(_) => errs += 1,
            }
            assert!(t.elapsed().as_secs() < 5, "fuzz_ids: slow open (stream {pick}, round {round}): {:?}", t.elapsed());
        }
    }
    eprintln!("fuzz_ids: {} payloads, {errs} refused, {oks} identical, {:?}", errs + oks, t0.elapsed());
}

// ---------------------------------------------------------------- refuter's fuzzer of the ID columns

/// Inputs for refute_ids_fuzz: id_inputs(), lines with two UUID slots (both cases, version 4
/// and 1, the same UUID twice in a line) and long ids of the generator and of others (negative
/// ones too), and, with FEWER_DATA set, HDFS_2k, OpenStack_2k and 5000 lines of
/// openstack_abnormal.
fn refute_inputs() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = id_inputs().into_iter().enumerate().map(|(i, d)| (format!("id_inputs[{i}]"), d)).collect();
    let mut x = 0xfeed_beefu64;
    let mut seed = 0x0bad_cafe_f00du64;
    let mut next_long = || {
        let mut half = || {
            seed = (seed.wrapping_mul(0x5DEECE66D).wrapping_add(0xB)) & ((1 << 48) - 1);
            (seed >> 16) as u32 as i32 as i64
        };
        let hi = half();
        (hi << 32).wrapping_add(half())
    };
    let mut uu: Vec<String> = Vec::new();
    let mut ids: Vec<i64> = Vec::new();
    let mut d = Vec::new();
    for i in 0..4000u64 {
        let (a, b) = (xorshift(&mut x), xorshift(&mut x));
        if i % 3 == 0 || uu.is_empty() {
            let s = format!("{:08x}-{:04x}-{}{:03x}-{:x}{:03x}-{:012x}", a as u32, (a >> 32) as u16, if b % 5 == 0 { 1 } else { 4 }, (a >> 48) % 4096, 8 + (b % 4), (b >> 8) % 4096, (b >> 20) % (1 << 48));
            uu.push(if b % 6 == 0 { s.to_uppercase() } else { s });
        }
        if i % 2 == 0 || ids.is_empty() {
            ids.push(if b % 11 == 3 { -(1_000_000_000_000_000 + (b >> 9) as i64 % 1_000_000_000_000) } else { next_long() });
        }
        let u1 = &uu[uu.len() - 1 - (a >> 30) as usize % uu.len().min(9)];
        let u2 = if b % 4 == 0 { u1 } else { &uu[(b >> 33) as usize % uu.len()] };
        let id = ids[ids.len() - 1 - (b >> 40) as usize % ids.len().min(30)];
        let w = if a % 17 == 0 { "word".to_string() } else { u2.clone() };
        d.extend_from_slice(format!("2017-05-16 00:{:02}:{:02}.{:03} {} INFO req-{u1} ctx {w} blk_{id} took {}\n", i / 60 % 60, i % 60, i % 1000, 2900 + i % 7, a % 1000).as_bytes());
    }
    v.push(("two UUID slots".into(), d));
    if let Ok(dir) = std::env::var("FEWER_DATA") {
        for f in ["loghub/HDFS_2k.log", "loghub/OpenStack_2k.log"] {
            if let Ok(mut d) = std::fs::read(format!("{dir}/{f}")) {
                // (a block's lines each end with a newline)
                if d.last() != Some(&b'\n') {
                    d.push(b'\n');
                }
                v.push((f.into(), d));
            }
        }
        if let Ok(d) = std::fs::read(format!("{dir}/loghub_full/OpenStack/openstack_abnormal.log")) {
            let end = d.iter().enumerate().filter(|&(_, &c)| c == b'\n').nth(4999).map_or(d.len(), |(i, _)| i + 1);
            v.push(("openstack_abnormal 5000 lines".into(), d[..end].to_vec()));
        }
    }
    v
}

/// Every panic's place (file:line), recorded by a hook set after `guarded`'s own.
static PANIC_AT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The refuter's fuzzer of the ID columns, at the block level (no checksums in the way), run
/// with: FEWER_DATA=... cargo test --release refute_ids_fuzz -- --ignored --nocapture
/// --test-threads=1 (FEWER_FUZZ=N random rounds per input, 2000 by default). Structured
/// damage: every bit of the column header (flags, modes: unknown bits, 128 without 32, 128 +
/// 16), the UUID part's 16-byte length (crossed counts: +-1, +-16, huge), the UUID part cut,
/// longer, bits flipped, gone, or another input's; then random out-of-range numbers (dictionary
/// places past its end, huge steps) and cache codes in the number and code streams. Each case
/// must open to an error or some output, quickly; panics are counted by place, and none may
/// come from the ID code (ids.rs, put_uuid, the ID branches).
#[test]
#[ignore]
fn refute_ids_fuzz() {
    let _ = guarded(|| ());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info.location().map_or("?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
        PANIC_AT.lock().unwrap().push(at);
        prev(info);
    }));
    let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    let mut x = 0x5eed_0b1du64;
    let (mut total, mut refused, mut exact, mut other, mut panics) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut worst = std::time::Duration::ZERO;
    let mut tails: Vec<Vec<u8>> = Vec::new();
    let t0 = std::time::Instant::now();
    for (name, d) in refute_inputs() {
        let lines = lines_of(&d, 0, d.len());
        let mut blk = eng::pack_block(&d, &lines, &eng::Rules::default(), 1, None);
        let st: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
        let fa = head_flags_at(&st[2]);
        if st[2][fa] & 32 == 0 {
            eprintln!("refute_ids_fuzz: {name}: no ID columns (skipped)");
            continue;
        }
        assert!(open_crafted(st.clone(), lines.len(), d.len()).unwrap() == d, "{name}: round trip");
        let tail = eng::id_tail(&st, lines.len()).expect("tail");
        let mut q = tail;
        let n16 = super::Rd { b: &st[5], p: tail }.varint().unwrap();
        q += {
            let mut v = Vec::new();
            put_varint(&mut v, n16);
            v.len()
        };
        let mut cases: Vec<Vec<Vec<u8>>> = Vec::new();
        // 1. every bit of the column header, and each byte set to 0x80 | b, 0x90 | b, 0xFF, 0
        for i in 0..st[2].len() {
            for b in 0..8 {
                let mut s = st.clone();
                s[2][i] ^= 1 << b;
                cases.push(s);
            }
            for v in [st[2][i] | 0x80, st[2][i] | 0x90, 0xFF, 0] {
                let mut s = st.clone();
                s[2][i] = v;
                cases.push(s);
            }
        }
        // 2. the UUID part's 16-byte length: crossed counts
        let n5 = st[5].len() as u64;
        for m in [0, n16 + 1, n16.wrapping_sub(1), n16 + 16, n16.wrapping_sub(16), n16 + 32, n5, n5 - tail as u64, 1 << 31, 1 << 62, u64::MAX] {
            let mut s = st.clone();
            s[5].truncate(tail);
            put_varint(&mut s[5], m);
            s[5].extend_from_slice(&st[5][q..]);
            cases.push(s);
        }
        // 3. the UUID part cut at each place, longer, bits flipped (its first 160 and last 64 bytes)
        let places: Vec<usize> = (tail..st[5].len()).filter(|&i| i < tail + 160 || i + 64 >= st[5].len()).collect();
        for &i in &places {
            let mut s = st.clone();
            s[5].truncate(i);
            cases.push(s);
            for b in 0..8 {
                let mut s = st.clone();
                s[5][i] ^= 1 << b;
                cases.push(s);
            }
            let mut s = st.clone();
            s[5].insert(i, 0);
            cases.push(s);
        }
        for k in [1usize, 2, 15, 16, 17, 31, 32, 100] {
            let mut s = st.clone();
            s[5].extend(std::iter::repeat_n(0x5Au8, k));
            cases.push(s);
        }
        // 4. the UUID part gone (flag 32 kept, then cleared); another input's UUID part
        let mut s = st.clone();
        s[5].truncate(tail);
        cases.push(s.clone());
        s[2][fa] &= !32;
        cases.push(s);
        for t in &tails {
            let mut s = st.clone();
            s[5].truncate(tail);
            s[5].extend_from_slice(t);
            cases.push(s);
        }
        tails.push(st[5][tail..].to_vec());
        // 5. random: out-of-range numbers (dictionary places past its end, huge steps) as varints
        // in the number streams, cache codes 9-12 and 255 in the code stream
        for _ in 0..rounds {
            let mut s = st.clone();
            for _ in 0..1 + xorshift(&mut x) % 2 {
                let r = xorshift(&mut x);
                let k = [4usize, 4, 7, 6, 6][(r % 5) as usize];
                let n = s[k].len();
                if n == 0 {
                    continue;
                }
                let i = (r >> 8) as usize % n;
                if k == 6 {
                    s[k][i] = [9u8, 10, 11, 12, 255, 0, 8][(r >> 40) as usize % 7];
                } else {
                    let v = [2 + (r >> 40) % 4096, 1 << ((r >> 32) % 64), u64::MAX, (r >> 3) % 3, 1 << 47, (1 << 48) - 1][(r >> 50) as usize % 6];
                    // (replace the varint that starts at i, or the bytes up to its end)
                    let mut e = i;
                    while e < n && s[k][e] & 0x80 != 0 {
                        e += 1;
                    }
                    let e = (e + 1).min(n);
                    s[k].splice(i..e, varints(&[v]));
                }
            }
            cases.push(s);
        }
        let (mut c_ref, mut c_ex, mut c_oth, mut c_pan) = (0, 0, 0, 0);
        for s in cases {
            let before = PANIC_AT.lock().unwrap().len();
            let t = std::time::Instant::now();
            let r = open_crafted(s, lines.len(), d.len());
            let el = t.elapsed();
            worst = worst.max(el);
            assert!(el.as_secs_f64() < 3.0, "{name}: slow open {el:?}");
            total += 1;
            if PANIC_AT.lock().unwrap().len() > before {
                c_pan += 1;
            } else {
                match r {
                    Ok(b) if b == d => c_ex += 1,
                    Ok(_) => c_oth += 1,
                    Err(_) => c_ref += 1,
                }
            }
        }
        eprintln!("refute_ids_fuzz: {name}: {} lines, tail {} bytes; refused {c_ref}, exact {c_ex}, other output {c_oth}, panics {c_pan}", lines.len(), st[5].len() - tail);
        (refused, exact, other, panics) = (refused + c_ref, exact + c_ex, other + c_oth, panics + c_pan);
    }
    let mut by: std::collections::BTreeMap<String, usize> = Default::default();
    for p in PANIC_AT.lock().unwrap().iter() {
        *by.entry(p.clone()).or_default() += 1;
    }
    for (p, n) in &by {
        eprintln!("refute_ids_fuzz: panic at {p}: {n}");
    }
    eprintln!("refute_ids_fuzz: {total} cases: refused {refused}, exact {exact}, other output {other}, panics {panics}; slowest {worst:?}; {:?}", t0.elapsed());
    // no panic from the ID code
    for p in by.keys() {
        assert!(!p.contains("ids.rs"), "panic in ids.rs: {p}");
    }
}

/// The both-ways check (refuter's regression test): sequential time-based UUIDs, each 3 times,
/// look like a big gain to the ID columns' estimate (repeats cost 2 bytes instead of about 4),
/// but their real bytes are far bigger (the estimate alone made such a log 4.5 times bigger),
/// so the block must not use ID columns; one UUID kept for 1000 lines must open exactly.
#[test]
fn id_columns_only_when_smaller() {
    let mut x = 0x77u64;
    let (mut seq, mut runs) = (Vec::new(), Vec::new());
    let mut u = String::new();
    for i in 0..30000u64 {
        let ts = format!("2017-05-16 {:02}:{:02}:{:02}.{:03}", i / 3600000 % 24, i / 60000 % 60, i / 1000 % 60, i % 1000);
        seq.extend_from_slice(format!("{ts} INFO job 12345678-11e7-b0a1-8000-{:012x} step {}\n", i / 3, i % 5).as_bytes());
        if i % 1000 == 0 {
            let (a, b) = (xorshift(&mut x), xorshift(&mut x));
            u = format!("{:08x}-{:04x}-4{:03x}-{:x}{:03x}-{:012x}", a as u32, (a >> 32) as u16, (a >> 48) % 4096, 8 + b % 4, (b >> 8) % 4096, (b >> 20) % (1 << 48));
        }
        runs.extend_from_slice(format!("{ts} 2931 INFO nova.compute [req-{u} - - -] took {}.{:02} seconds\n", i % 7, i % 100).as_bytes());
    }
    for (k, d) in [seq, runs].into_iter().enumerate() {
        let lines = lines_of(&d, 0, d.len());
        let mut blk = eng::pack_block(&d, &lines, &eng::Rules::default(), 1, None);
        let streams: Vec<Vec<u8>> = (0..eng::STREAMS).map(|k| blk.take_stream(k)).collect();
        if k == 0 {
            assert!(streams[2][head_flags_at(&streams[2])] & 32 == 0, "ID columns where they do not pay");
        }
        assert!(open_crafted(streams, lines.len(), d.len()).unwrap() == d);
        rt(&d);
    }
}

/// A varint at b[*p..] (0 past the end).
fn gv(b: &[u8], p: &mut usize) -> u64 {
    let mut v = 0u64;
    for k in 0..10 {
        let Some(&x) = b.get(*p) else { return v };
        *p += 1;
        v |= ((x & 0x7f) as u64) << (7 * k);
        if x < 0x80 {
            break;
        }
    }
    v
}

/// B5 inputs: bgl lines with two clock offsets (b and B markers), labels and nodes of several
/// lengths, and syslog lines that end with a ctime() copy of their clock.
fn b5_inputs() -> Vec<Vec<u8>> {
    let mut x = 5u64;
    let mut bgl = Vec::new();
    for i in 0..3000u64 {
        let s = 15 * 3600 + 42 * 60 + 50 + i / 3;
        let off = if i % 5 == 0 { 6 * 3600 } else { 7 * 3600 };
        let node = format!("R{:02}-M{}-N{}{}", i % 64, i % 2, "x".repeat((i % 9) as usize), i % 16);
        let label = ["-", "APPREAD", "KERNDTLB", "L"][(i % 4) as usize];
        bgl.extend_from_slice(
            format!("{label} {} 2005.06.03 {node} 2005-06-03-{:02}.{:02}.{:02}.{:06} {node} RAS KERNEL INFO cache parity {}\n", 1117756800 + off + s, s / 3600, s / 60 % 60, s % 60, xorshift(&mut x) % 1_000_000, i % 7)
                .as_bytes(),
        );
    }
    let mut ct = Vec::new();
    for i in 0..3000u64 {
        let (d, s) = (1 + i / 400, 3 * 3600 + i * 7);
        // (1 June 2005 was a Wednesday)
        let wd = ["Wed", "Thu", "Fri", "Sat", "Sun", "Mon", "Tue"][((d - 1) % 7) as usize];
        let clock = format!("Jun {:>2} {:02}:{:02}:{:02}", d, s / 3600 % 24, s / 60 % 60, s % 60);
        ct.extend_from_slice(format!("{clock} combo ftpd[{}]: connection from 1.2.3.{} at {wd} {clock} 2005\n", 20000 + i % 300, i % 256).as_bytes());
    }
    vec![derive_inputs().swap_remove(0).1, bgl, ct]
}

/// fix3: the rule bits of the side stream of a payload of one lines block (None: another
/// payload, or a block without one).
fn side_bits_of(p: &[u8]) -> Option<u64> {
    let mut r = Rd::new(p);
    r.byte().ok()?;
    r.varint().ok()?;
    if r.varint().ok()? != 1 {
        return None;
    }
    let tables: Vec<&[u8]> = (0..r.varint().ok()?).map(|_| r.frame()).collect::<Result<_, _>>().ok()?;
    let _ = (tables, r.varint().ok()?);
    if r.byte().ok()? != KIND_LINES {
        return None;
    }
    r.byte().ok()?;
    r.varint().ok()?;
    let ns = r.varint().ok()?;
    let frames: Vec<&[u8]> = (0..ns).map(|_| r.frame()).collect::<Result<_, _>>().ok()?;
    let side = frames.get(10).filter(|f| !f.is_empty())?;
    let side = eng::decode(side, 64 << 20, None)?;
    let mut sp = 0;
    Some(gv(&side, &mut sp))
}

/// fix3: small bgl inputs whose block packs in the old form (side stream bits 2 | 8: no fill
/// marker, the packer keeping the form of fewer bytes): a line of the BGL log (the attack's
/// real_43) and the first lines of b5_inputs' bgl input, those of them that do.
fn old_bgl_inputs() -> Vec<Vec<u8>> {
    let mut v = vec![b"- 1120486433 2005.07.04 R10-M1-NA-C:J13-U01 2005-07-04-07.13.53.601691 R10-M1-NA-C:J13-U01 RAS KERNEL INFO generating core.9237\n".to_vec()];
    let bgl = b5_inputs().swap_remove(1);
    for k in [2, 3, 5, 8, 13, 21, 40, 80] {
        v.push(bgl.split_inclusive(|&c| c == b'\n').take(k).flatten().copied().collect());
    }
    v.retain(|d| side_bits_of(&pack(d, 1)) == Some(2 | 8));
    v
}

/// fix3: a block of bgl lines packed in the old form (side stream bits 2 | 8, no fill
/// marker) opens exactly and packs the same at 1 and 3 threads; its rewrite has no fill
/// marker and is shorter than B5's; check_derived takes the old form only with bit 8 (also
/// with other rules beside bgl) and B5's form only without it; 8 without bgl is refused.
#[test]
fn old_bgl_form() {
    let ins = old_bgl_inputs();
    assert!(!ins.is_empty(), "no input packs in the old form");
    for d in &ins {
        let p = pack(d, 1);
        assert!(unpack(&p, 1).unwrap() == *d && unpack(&p, 3).unwrap() == *d && pack(d, 3) == p, "{} bytes", d.len());
        // the rewrites in both forms (B5's: the rules' own)
        let lines = lines_of(d, 0, d.len());
        let rules = eng::detect(&lines);
        let old = rules.old_bgl().expect("bgl rules");
        let (tb, sb) = eng::rewritten(d, &lines, &rules).unwrap();
        let (to, so) = eng::rewritten(d, &lines, &old).unwrap();
        assert_eq!((sb[0], so[0]), (2, 2 | 8));
        assert!(to.len() < tb.len() && !to.windows(2).any(|w| w == [1, b'e'] || w == [1, b'E']));
        assert!(eng::check_derived(&to, &so).is_ok() && eng::check_derived(&tb, &sb).is_ok());
        let mut so2 = so.clone();
        so2[0] = 2;
        let mut sb2 = sb.clone();
        sb2[0] = 2 | 8;
        assert!(eng::check_derived(&to, &so2).is_err() && eng::check_derived(&tb, &sb2).is_err());
        for bits in [8u8, 8 | 16, 8 | 64] {
            let mut s = so.clone();
            s[0] = bits;
            assert!(eng::check_derived(&to, &s).is_err());
        }
        // the old form with another rule beside bgl (2 | 8 | 64) takes the general path
        let mut s3 = so.clone();
        s3[0] = 2 | 8 | 64;
        assert!(eng::check_derived(&to, &s3).is_ok());
        let size = d.len() + 64;
        assert_eq!(eng::open_raw(&to, &s3, size).as_deref(), Some(&d[..]));
        assert_eq!(eng::open_raw(&to, &so, size).as_deref(), Some(&d[..]));
    }
}

/// Fuzzing of B5's decoder (format 2: bgl and ctime parts put back by the line printer), run
/// with: cargo test --release b5_fuzz -- --ignored (FEWER_FUZZ=N rounds per input, 3000 by
/// default). Blocks are damaged where the new code reads: the side stream (its text size 0
/// or not, rule bits, the bgl offsets, cut short, grown), the markers in the patterns (the
/// letter changed, a marker taken out, put in, moved), the values (timestamps, years), and
/// any stream as fuzz_streams does. Every result must be an error or the exact text, in
/// time; no panic may come from derive.rs (bgl_fill, ctime_put, inline_of,
/// inverse after check_derived).
#[test]
#[ignore]
fn b5_fuzz() {
    use std::sync::Mutex;
    static AT: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let _ = guarded(|| ());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(l) = info.location() {
            AT.lock().unwrap().push(format!("{}:{}", l.file(), l.line()));
        }
        prev(info);
    }));
    let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let mut x = 0xb5_u64;
    let (mut errs, mut oks) = (0usize, 0usize);
    let t0 = std::time::Instant::now();
    // (fix3: and blocks of bgl lines in the old form, side stream bits 2 | 8)
    let olds = old_bgl_inputs();
    assert!(!olds.is_empty(), "b5_fuzz: no input packs in the old form");
    for d in b5_inputs().into_iter().chain(olds) {
        let p = pack(&d, 2);
        assert!(unpack(&p, 2).unwrap() == d && unpack(&p, 1).unwrap() == d && pack(&d, 1) == p && pack(&d, 3) == p);
        let q = split_one_block(&p);
        let opened: Vec<Vec<u8>> = q.frames.iter().map(|f| if f.is_empty() { Vec::new() } else { eng::decode(f, 64 << 20, None).unwrap() }).collect();
        // the block is printed with its derived parts: text size 0 in the side stream (or,
        // fix3, its bgl lines are in the old form: bits 2 | 8)
        let side = &opened[10];
        let mut sp = 0;
        let bits = gv(side, &mut sp);
        assert!(!side.is_empty() && (side[sp] == 0 || bits == 2 | 8), "not a B5 block");
        let raw = d.len() as u64;
        for round in 0..rounds {
            let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
            let r0 = xorshift(&mut x);
            let pick = match r0 % 8 {
                0 | 1 => 10,
                2 | 3 => 0,
                _ => (xorshift(&mut x) % 10) as usize,
            };
            let mut s = opened[pick].clone();
            for _ in 0..1 + xorshift(&mut x) % 2 {
                let r = xorshift(&mut x);
                let i = if s.is_empty() { 0 } else { (r >> 8) as usize % s.len() };
                let mks: Vec<usize> = s.iter().enumerate().filter(|e| *e.1 == 1).map(|e| e.0).collect();
                match (pick, r % 9) {
                    // the side stream: text size, rule bits, offsets
                    (10, 0) => {
                        let mut p = 0;
                        let bits = gv(&s, &mut p);
                        let a = p;
                        let _ = gv(&s, &mut p);
                        let v = [0, 1, (r >> 16) % 5000, d.len() as u64][(r >> 12) as usize % 4];
                        s.splice(a..p, varints(&[v]));
                        let _ = bits;
                    }
                    (10, 1) => {
                        let mut p = 0;
                        let bits = gv(&s, &mut p);
                        s.splice(0..p, varints(&[bits ^ (1 << ((r >> 16) % 13))]));
                    }
                    (10, 2) => {
                        let mut p = 0;
                        let _ = gv(&s, &mut p);
                        let _ = gv(&s, &mut p);
                        let o = [(r >> 20) as i64 % 100_000 - 50_000, (r >> 8) as i64, i64::MIN, i64::MAX, -(1i64 << 40)][(r >> 16) as usize % 5];
                        let z = ((o << 1) ^ (o >> 63)) as u64;
                        s.truncate(p);
                        s.extend_from_slice(&varints(&[z, if r & 1 == 0 { z } else { 0 }]));
                    }
                    // the markers in the patterns
                    (0, 0 | 1) if !mks.is_empty() => {
                        let m = mks[(r >> 16) as usize % mks.len()];
                        if m + 1 < s.len() {
                            s[m + 1] = [b'b', b'B', b'e', b'E', b't', b'\n', 0, 1, (r >> 40) as u8][(r >> 24) as usize % 9];
                        }
                    }
                    (0, 2) if !mks.is_empty() => {
                        let m = mks[(r >> 16) as usize % mks.len()];
                        s.drain(m..(m + 1 + (r >> 30) as usize % 2).min(s.len()));
                    }
                    (0, 3) => {
                        let l = b"bBeEt"[(r >> 24) as usize % 5];
                        s.splice(i..i, [1u8, l]);
                    }
                    (0, 4) if !mks.is_empty() => {
                        let m = mks[(r >> 16) as usize % mks.len()];
                        if m + 2 <= s.len() {
                            let mk: Vec<u8> = s.drain(m..m + 2).collect();
                            let j = (r >> 40) as usize % (s.len() + 1);
                            s.splice(j..j, mk);
                        }
                    }
                    // anything, as fuzz_streams
                    (_, k) => match k % 6 {
                        0 if !s.is_empty() => s[i] ^= 1 << ((r >> 40) % 8),
                        1 => s.truncate(i),
                        2 => s.insert(i, (r >> 32) as u8),
                        3 if !s.is_empty() => s[i] = [b'0', b'9', b' ', b'-', b'.', 0xFF][(r >> 44) as usize % 6],
                        4 => {
                            let v = varints(&[(r >> 16) % (1 << (r % 60))]);
                            s.splice(i..(i + 1).min(s.len()), v);
                        }
                        _ if !s.is_empty() => {
                            s.remove(i);
                        }
                        _ => {}
                    },
                }
            }
            h.frames[pick] = if s.is_empty() && round % 2 == 0 { Vec::new() } else { eng::store(&s) };
            let mut pay = join_one_block(raw, &h);
            if round % 7 == 6 {
                let i = (xorshift(&mut x) as usize) % pay.len();
                pay[i] ^= 1 << (x % 8);
            }
            let t = std::time::Instant::now();
            match unpack(&pay, [1, 3, 14][round % 3]) {
                Ok(back) => {
                    assert!(back == d, "b5_fuzz: wrong output (stream {pick}, round {round})");
                    oks += 1;
                }
                Err(_) => errs += 1,
            }
            assert!(t.elapsed().as_secs() < 5, "b5_fuzz: slow open (stream {pick}, round {round}): {:?}", t.elapsed());
        }
    }
    let at = AT.lock().unwrap().clone();
    let mut by: std::collections::BTreeMap<String, usize> = Default::default();
    for a in &at {
        *by.entry(a.clone()).or_default() += 1;
    }
    eprintln!("b5_fuzz: {} payloads, {errs} refused, {oks} identical, {:?}; panics caught by place: {:?}", errs + oks, t0.elapsed(), by);
    assert!(!at.iter().any(|a| a.contains("derive.rs")), "b5_fuzz: a panic in derive.rs: {by:?}");
}


/// The checker's fuzzer of B5's decoder (refute_b5_computed), meant for a debug build (debug
/// assertions and overflow checks on): a write past a line's budget on the unchecked paths
/// (w16, put_lit16, put_lit) then shows as a failed assertion instead of passing unseen.
/// cargo test refute_b5_fuzz -- --ignored --nocapture (FEWER_FUZZ=N rounds per input). Blocks
/// with B5 parts are damaged where format 2 reads: the side stream (rule bits of all 64, the
/// text size, offsets, non-canonical varints, cut short, grown), the markers in the patterns
/// (letters, pairs, crossed pairs, a marker at the end, a marker before a slot), the block's
/// declared sizes and line count (crossed with the streams, near the buffer's end), and the
/// value streams. Every result must be an error or the exact text, in time; it fails on a
/// failed debug assertion, an overflow, or any panic in derive.rs.
#[test]
#[ignore]
fn refute_b5_fuzz() {
    use std::sync::Mutex;
    static AT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let _ = guarded(|| ());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info.location().map_or(String::new(), |l| format!("{}:{}", l.file(), l.line()));
        let p = info.payload();
        let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
        AT.lock().unwrap().push((at, msg));
        let _ = &prev;
    }));
    let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    let mut x = 0x7e5_u64;
    let mut ins = b5_inputs();
    // bgl and ctime lines in one block (rule bits 2 | 256), and a short bgl block
    let mut both = Vec::new();
    let (b, c) = (&ins[1], &ins[2]);
    for (lb, lc) in b.split_inclusive(|&c| c == b'\n').zip(c.split_inclusive(|&c| c == b'\n')) {
        both.extend_from_slice(lb);
        both.extend_from_slice(lc);
    }
    let short: Vec<u8> = ins[1].split_inclusive(|&c| c == b'\n').take(40).flatten().copied().collect();
    ins.push(both);
    ins.push(short);
    // fix3: blocks of bgl lines in the old form (side stream bits 2 | 8)
    ins.extend(old_bgl_inputs());
    let (mut errs, mut oks, mut used) = (0usize, 0usize, 0usize);
    let t0 = std::time::Instant::now();
    for d in ins {
        let p = pack(&d, 2);
        assert!(unpack(&p, 1).unwrap() == d && unpack(&p, 3).unwrap() == d);
        let q = split_one_block(&p);
        let opened: Vec<Vec<u8>> = q.frames.iter().map(|f| if f.is_empty() { Vec::new() } else { eng::decode(f, 64 << 20, None).unwrap() }).collect();
        let side = &opened[10];
        let mut sp = 0;
        let bits0 = gv(side, &mut sp);
        if side.is_empty() || (side[sp] != 0 && bits0 != 2 | 8) {
            eprintln!("refute_b5_fuzz: input of {} bytes is not a B5 block (side {:?})", d.len(), &side[..side.len().min(12)]);
            continue;
        }
        used += 1;
        let raw = d.len() as u64;
        for round in 0..rounds {
            let mut h = Parts { frames: q.frames.clone(), tables: q.tables.clone(), ..q };
            let mut ss: Vec<Vec<u8>> = opened.clone();
            let mut raw2 = raw;
            for _ in 0..1 + xorshift(&mut x) % 3 {
                let r = xorshift(&mut x);
                let r2 = xorshift(&mut x);
                match r % 16 {
                    // the side stream
                    0 => {
                        let s = &mut ss[10];
                        let mut p = 0;
                        let _ = gv(s, &mut p);
                        let nb = [0, 2, 256, 258, 2 | 1 << 32, 256 | 1 << 40, 4, 16, 2 | 64, bits0 | 1 << 63, r2 >> 3, 2 | 8, 8, 8 | 256, 2 | 8 | 256, bits0 ^ 8][(r >> 8) as usize % 16];
                        s.splice(0..p, varints(&[nb]));
                    }
                    1 => {
                        let s = &mut ss[10];
                        let mut p = 0;
                        let _ = gv(s, &mut p);
                        let a = p;
                        let _ = gv(s, &mut p);
                        let v: Vec<u8> = match (r >> 8) % 5 {
                            0 => vec![0x80, 0x00],
                            1 => vec![0x80, 0x80, 0x00],
                            2 => varints(&[raw]),
                            3 => varints(&[1]),
                            _ => varints(&[r2 % (4 * raw + 9000)]),
                        };
                        s.splice(a..p, v);
                    }
                    2 => {
                        let s = &mut ss[10];
                        let mut p = 0;
                        let _ = gv(s, &mut p);
                        let _ = gv(s, &mut p);
                        let o = [i64::MIN, i64::MAX, -(1i64 << 40), 1i64 << 40, (r2 % 200_000) as i64 - 100_000, -1117813370, 0][(r >> 8) as usize % 7];
                        let z = ((o << 1) ^ (o >> 63)) as u64;
                        s.truncate(p);
                        match (r >> 12) % 4 {
                            0 => s.extend_from_slice(&varints(&[z, z])),
                            1 => s.extend_from_slice(&varints(&[z])),
                            2 => s.extend_from_slice(&varints(&[z, z, z])),
                            _ => s.extend_from_slice(&[0xff; 11]),
                        }
                    }
                    3 => {
                        let s = &mut ss[10];
                        let n = s.len();
                        s.truncate((r2 as usize) % (n + 1));
                    }
                    // the markers in the patterns
                    4..=8 => {
                        let s = &mut ss[0];
                        let mks: Vec<usize> = s.iter().enumerate().filter(|e| *e.1 == 1).map(|e| e.0).collect();
                        let i = if s.is_empty() { 0 } else { (r2 as usize) % s.len() };
                        let let_ = [b'b', b'B', b'e', b'E', b't', b'x', 0, 1, b'\n'][(r >> 8) as usize % 9];
                        match r % 16 {
                            4 => s.splice(i..i, [1u8, let_]).for_each(drop),
                            5 if !mks.is_empty() => {
                                // the letter of a marker into another letter, or the slot mark
                                let m = mks[(r >> 20) as usize % mks.len()];
                                if m + 1 < s.len() {
                                    s[m + 1] = let_;
                                }
                            }
                            6 => {
                                // a whole pair around a span of one pattern (maybe crossing
                                // values and the pattern's end), or ctime before / after it
                                let j = (i + (r >> 24) as usize % 120).min(s.len());
                                let (a, z) = if (r >> 40) & 1 == 0 { (b'b', b'e') } else { (b'B', b'E') };
                                s.splice(j..j, [1u8, z]).for_each(drop);
                                s.splice(i..i, [1u8, a]).for_each(drop);
                                if (r >> 41) & 1 == 1 {
                                    let k = (r2 >> 20) as usize % (s.len() + 1);
                                    s.splice(k..k, [1u8, b't']).for_each(drop);
                                }
                            }
                            7 if !mks.is_empty() => {
                                // a marker moved right before a slot mark, a '\n' or the end
                                let m = mks[(r >> 20) as usize % mks.len()];
                                let mk: Vec<u8> = s.drain(m..(m + 2).min(s.len())).collect();
                                let tgt = s.iter().enumerate().skip(i).find(|e| *e.1 == 0 || *e.1 == b'\n').map_or(s.len(), |e| e.0);
                                let tgt = if (r >> 30) & 1 == 0 { tgt } else { s.len() };
                                s.splice(tgt..tgt, mk).for_each(drop);
                            }
                            _ if !mks.is_empty() => {
                                // a marker's letter taken away, or the marker doubled
                                let m = mks[(r >> 20) as usize % mks.len()];
                                if (r >> 30) & 1 == 0 {
                                    if m + 1 < s.len() {
                                        s.remove(m + 1);
                                    }
                                } else {
                                    let mk: Vec<u8> = s[m..(m + 2).min(s.len())].to_vec();
                                    s.splice(m..m, mk).for_each(drop);
                                }
                            }
                            _ => {}
                        }
                    }
                    // the declared sizes and the line count
                    9 | 10 => {
                        let dv: i64 = [-1, 1, -16, 16, -64, 64, -80, 80, -21, 23, -(raw as i64) / 2, raw as i64, -(raw as i64) + 30][(r >> 8) as usize % 13];
                        let nv = (raw as i64 + dv).max(1) as u64;
                        if r % 16 == 9 {
                            raw2 = nv;
                            h.braw = nv;
                        } else {
                            h.braw = nv;
                        }
                    }
                    11 => {
                        let dl: i64 = [-1, 1, -2, 7, -(h.lines as i64) / 2, h.lines as i64][(r >> 8) as usize % 6];
                        h.lines = (h.lines as i64 + dl).max(1) as u64;
                    }
                    // the values: timestamps, years, nodes (texts, numbers, kinds)
                    _ => {
                        let k = [3, 4, 5, 6, 7, 1, 2, 8][(r >> 8) as usize % 8];
                        let s = &mut ss[k];
                        let i = if s.is_empty() { 0 } else { (r2 as usize) % s.len() };
                        match (r >> 12) % 5 {
                            0 if !s.is_empty() => s[i] ^= 1 << ((r >> 20) % 8),
                            1 => s.truncate(i),
                            2 => s.insert(i, (r >> 24) as u8),
                            3 if !s.is_empty() => s[i] = [b'0', b'9', b' ', b'-', b'.', 0xFF, 0x80][(r >> 28) as usize % 7],
                            _ => s.splice(i..i, varints(&[(r2 >> 8) % (1 << ((r >> 32) % 63))])).for_each(drop),
                        }
                    }
                }
            }
            for k in 0..ss.len() {
                if ss[k] != opened[k] {
                    h.frames[k] = if ss[k].is_empty() && round % 2 == 0 { Vec::new() } else { eng::store(&ss[k]) };
                }
            }
            let pay = join_one_block(raw2, &h);
            let t = std::time::Instant::now();
            match unpack(&pay, [1, 3, 14][round % 3]) {
                Ok(back) => {
                    assert!(back == d, "refute_b5_fuzz: wrong output (round {round})");
                    oks += 1;
                }
                Err(_) => errs += 1,
            }
            assert!(t.elapsed().as_secs() < 30, "refute_b5_fuzz: slow open (round {round}): {:?}", t.elapsed());
        }
    }
    let at = AT.lock().unwrap().clone();
    let mut by: std::collections::BTreeMap<String, usize> = Default::default();
    for (a, m) in &at {
        *by.entry(format!("{a} {}", m.chars().take(60).collect::<String>())).or_default() += 1;
    }
    eprintln!("refute_b5_fuzz: {used} B5 inputs, {} payloads, {errs} refused, {oks} identical, {:?}", errs + oks, t0.elapsed());
    for (k, v) in &by {
        eprintln!("  {v:6} {k}");
    }
    assert!(used >= 3, "refute_b5_fuzz: too few B5 inputs");
    // (overflows at three places of the base's own code are not counted: core.rs get_varint's
    // shift past 64 bits, open.rs put_num_long's 24 - n and cached_new's id_list[len - 1 - x];
    // a release build wraps them and the index checks after them stop the block; any other
    // overflow counts)
    let base_own = |k: &str| ["core.rs:199 ", "open.rs:109 ", "open.rs:263 "].iter().any(|s| k.contains(s));
    let bad: Vec<&String> = by.keys().filter(|k| k.contains("derive.rs") || k.contains("assertion failed") || (k.contains("overflow") && !base_own(k))).collect();
    assert!(bad.is_empty(), "refute_b5_fuzz: {bad:?}");
}

/// B5 regression (refute_b5_computed): bgl lines whose node or rest starts with hex digits
/// (the splitter reads a marker's letter and them as one hex run, so the letter lands in a
/// value) must still open: such blocks take the old, self-checked path (inline_ok), and a
/// rest of hex digits keeps its first space (MK E). The builder's B5 packed these into
/// archives its opener refused.
#[test]
fn b5_hex_runs_round_trip() {
    let mut x = 77u64;
    for form in 0..3u64 {
        let mut d = Vec::new();
        for i in 0..3000u64 {
            let s = 15 * 3600 + 42 * 60 + 50 + i / 3;
            let r = xorshift(&mut x);
            let node = match form {
                0 => format!("{:08}", r % 100_000_000),
                1 => format!("cafe{:04}", r % 10_000),
                _ => format!("R{:02}-M1-N0", r % 64),
            };
            let rest = if form == 2 { format!("{:08} ok {i}", r % 100_000_000) } else { "RAS KERNEL INFO cache parity".to_string() };
            d.extend_from_slice(format!("- {} 2005.06.03 {node} 2005-06-03-{:02}.{:02}.{:02}.{:06} {node} {rest}\n", 1117813370 + s, s / 3600, s / 60 % 60, s % 60, (r >> 20) % 1_000_000).as_bytes());
        }
        for t in [1, 3] {
            let p = pack(&d, t);
            assert!(unpack(&p, t).ok().as_ref() == Some(&d), "form {form}, {t} threads: the archive does not open");
        }
    }
}
