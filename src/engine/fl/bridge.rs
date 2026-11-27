//! The bridge between fewer-pack's adapter (../mod.rs) and the engine: the ONLY code that names
//! the engine's functions, types, settings and stream numbers; a newer engine drops in when it still has them (or when this file follows its changes).
//!
//! What the adapter gets from here:
//! - sample_runs / detect: the derive rules of a run of lines (from a sample, as the engine
//!   finds them for a file),
//! - pack_block / code_streams: a block of lines -> its streams -> their coded frames,
//! - decode / open_block: the frames -> the streams -> the block's lines,
//! - code / code_table / store / declared: the engine's coders for plain bytes, tables and
//!   lists,
//! - table_bank / Bank: the payload's table bank (fz's frequency tables that several blocks'
//!   column streams share, written once),
//! - check_block: the limits that keep the opener's time and memory in proportion to the
//!   block's size (the packer checks them too, so every block it writes passes),
//! - the settings below.
#![warn(clippy::all)]
#![allow(clippy::manual_is_multiple_of)]

use super::{MARK, Packed, derive, fz, open, ozip, read_ids, stored};

/// The coder level of the column streams and of plain blocks: the engine's DEFAULT_LEVEL (fz,
/// method 6: LZ with static rANS tables, greedy parse with buckets of 4 and a lazy step).
pub const LEVEL: u32 = 43;
/// The level of the pattern and word tables, the blocks' lists and the column header (the
/// engine's table_level: ozip method 5 with a deeper copy search, 28 below 2 MiB, 27 from
/// there). Every call of the coder passes one of these constants, so the compiler can leave
/// out the levels fewer-pack never uses.
const TABLE_LEVEL: u32 = 28;
const BIG_TABLE_LEVEL: u32 = 27;
const BIG_TABLE: usize = 2 << 20;
/// A run of up to ONE_MAX lines is one block; a longer run is cut into blocks of PER_BLOCK
/// lines (the engine's FL_ONEMAX and FL_BLOCK defaults).
pub const ONE_MAX: usize = 400_000;
pub const PER_BLOCK: usize = 100_000;
/// Streams of a packed block (Packed::streams, then the derive side stream): templates,
/// pattern ids, column header, kinds, numbers, texts, cache codes, differences, words, line
/// order, derived fields.
pub const STREAMS: usize = 11;
/// The streams that hold a block's own tables, entries each ended by '\n' (the pattern
/// table and the word table). The adapter keeps one table of each per payload; a block
/// then holds the list of the entries it uses.
pub const TABLES: [usize; 2] = [0, 8];
/// The pattern of each line: a varint per line, or one of the engine's other forms (core.rs
/// id_forms: the next new pattern, the next-pattern guess), whichever codes smallest.
pub const IDS: usize = 1;
/// The column header (coded like a table: the engine's code_streams).
pub const HEADER: usize = 2;
/// The frame methods fewer-pack writes: method 5 (code_table) for tables, the blocks' lists and
/// the column header, method 6 (code) for plain blocks and the other streams; either may
/// be stored (method 0). A reader takes no other method for each (decode_as).
pub const TABLE_METHOD: u8 = 5;
pub const COLUMN_METHOD: u8 = 6;
/// The derive side stream (derive.rs: empty when the block's lines were not rewritten).
pub const SIDE: usize = 10;
/// The texts stream: text values, whole tokens, hex bytes, UUIDs (a text value is a run of
/// its bytes).
pub const TEXTS: usize = 5;
/// The month and weekday names a value may be (kind NAMEK).
pub const NAMES: &[&[u8]] = &super::NAMES;

/// Names (prefixes) of the engine's developer switches read from the environment: FL_* in
/// the engine, CM_OZ* in ozip (CM_OZSTAT, CM_OZCHAIN, CM_OZSUFF), P1_* (earlier engines).
/// The engine's port turns every read of the environment in the engine into "not set"
/// (fl::no_env), so they have no effect; fewer-pack still clears them at start
/// (engine::clear_env_switches), in case an engine reads them another way.
pub const ENV_SWITCHES: [&str; 3] = ["FL_", "CM_OZ", "P1_"];

/// Limits checked by check_block.
/// Most value marks in one pattern.
pub const MAX_MARKS: usize = 1024;
/// Most columns in a block (the opener keeps about 250 bytes per column).
pub const MAX_COLS: u64 = 1 << 20;

/// Smallest block (bytes of its lines) whose derive checks run on a thread of their own while
/// its rewritten lines are packed (pack_block_as): below it the thread costs more than it saves.
const CHECKS_ASIDE: usize = 1 << 20;

/// A packed block (the engine's Packed, with the derive side stream as stream 10).
pub struct Block(Packed);

impl Block {
    pub fn lines(&self) -> usize {
        self.0.lines
    }
    pub fn streams(&self) -> &[Vec<u8>] {
        &self.0.streams
    }
    pub fn take_stream(&mut self, k: usize) -> Vec<u8> {
        std::mem::take(&mut self.0.streams[k])
    }
    /// The pattern ids in the other forms worth a try (core.rs id_forms; the adapter codes
    /// them after its choice of lines or plain bytes, which the plain form makes).
    pub fn take_alt(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.0.alt)
    }
}

/// The derive rules of a run of lines.
#[derive(Clone, Copy, Default)]
pub struct Rules(derive::Rules);

impl Rules {
    /// Whether the rules may rewrite a block's lines: then pack_block and patterns_max look
    /// for MK bytes in the block's text (a block with one is not rewritten), and the adapter
    /// does it once for both (mk_in).
    pub fn rewrite(&self) -> bool {
        self.0.bits != 0
    }

    /// fix3: these rules with the bgl lines in the old form (derive::Rules::old_bgl), for a
    /// block packed both ways (engine/mod.rs pack_lines); None unless bgl is on.
    pub fn old_bgl(&self) -> Option<Rules> {
        self.0.old_bgl().map(Rules)
    }
}

/// Whether text holds an MK byte (derive.rs: the rewrite's marker byte); for a block's text
/// (its lines, each followed by '\n'), the answer pack_block's rewrite looks for (`mk`).
pub fn mk_in(text: &[u8]) -> bool {
    derive::has_byte(text, derive::MK)
}

/// The lines derive::detect looks at in a run of n lines, as (first line, count): the engine
/// samples up to 4 runs of 2,048 lines spread over the file (derive.rs `sample`), and detect
/// given exactly those lines finds the same rules.
pub fn sample_runs(n: usize) -> Vec<(usize, usize)> {
    const RUN: usize = 2048;
    if n <= 4 * RUN {
        return vec![(0, n)];
    }
    (0..4).map(|k| (k * (n - RUN) / 3, RUN)).collect()
}

/// The derive rules that hold on a run of lines, from its sample (sample_runs).
pub fn detect(sample: &[&[u8]]) -> Rules {
    Rules(derive::detect(sample))
}

/// Pack one block: `lines` are slices of `data` (each without its '\n'), in order. The
/// engine may read up to 64 bytes past a line inside `data`. As the engine's pack_coded: the
/// block's lines are first rewritten without their derived fields when a rule of the run
/// applies (derive::Packer, a fresh state per block), and the side stream is stream 10. May
/// panic on input the engine cannot handle (the adapter then keeps the block as plain bytes).
/// The rewrite uses up to `threads` threads (forward_par: the same bytes as forward). `mk`:
/// mk_in of the lines' text when the caller knows it (else it is looked for).
/// A block whose distinct shapes are too big a part of it (core.rs, SHAPE_FLOOR and SHAPE_DIV)
/// or whose patterns would make too many columns (COLS_FLOOR and COL_BYTES) is declined:
/// lines() is then not the number of lines given, and the adapter keeps the block as plain
/// bytes.
pub fn pack_block(data: &[u8], lines: &[&[u8]], rules: &Rules, threads: usize, mk: Option<bool>) -> Block {
    pack_block_as(data, lines, rules, threads, false, mk)
}

/// pack_block; `alone`: the only block of lines of its input (the adapter's plan), which
/// no other block packs beside (core.rs, SHAPE_FLOOR_ALONE).
///
/// A rewrite that the opener's check (check_derived) would refuse, or that the opener would
/// not turn back into exactly these lines, is not used: a rule that misreads a line costs
/// size, never data. B5 (format 2): with bgl / ctime markers only, each rule checked every
/// derived part against the opener's own function of the text before its marker (derive.rs
/// bgl_fill, ctime_put), so when every marker is in the patterns' fixed text the line
/// printer puts the parts back (side stream marked, open::inline_ok) and the put-back is not
/// run here; otherwise the checks of derived_ok.
pub fn pack_block_as(data: &[u8], lines: &[&[u8]], rules: &Rules, threads: usize, alone: bool, mk: Option<bool>) -> Block {
    super::FLOOR.with(|f| f.set(if alone { super::SHAPE_FLOOR_ALONE } else { super::SHAPE_FLOOR }));
    if let Some(mut f) = forward(data, lines, rules, threads, mk) {
        let tsize = f.starts.last().map_or(0, |&e| e as usize);
        let size: usize = lines.iter().map(|l| l.len() + 1).sum();
        if tsize <= rewritten_max(size) {
            // B5: a rewrite whose markers the line printer puts back needs no check
            // (open::inline_ok on the packed block)
            let mut p = if derive::may_inline(&f.side) { Some(super::pack(&f.text, &f.lines(), threads)) } else { None };
            let inl = p.as_ref().is_some_and(|p| open::inline_ok(p, f.marks));
            // otherwise the checks; with threads to spare and a big block not packed yet, they
            // run on a thread of their own while the rewritten lines are packed (on this thread:
            // FLOOR and the tables hook are this thread's); that packing is kept only when they
            // pass, as without this. A panic in the checks goes on as it would without the
            // thread (unwind builds: the adapter keeps the block as plain bytes)
            let ok = inl
                || if p.is_none() && threads > 1 && size >= CHECKS_ASIDE {
                    let (ok, q) = std::thread::scope(|s| {
                        let c = s.spawn(|| derived_ok(data, &f, tsize, lines));
                        let q = super::pack(&f.text, &f.lines(), threads);
                        (c.join().unwrap_or_else(|e| std::panic::resume_unwind(e)), q)
                    });
                    p = Some(q);
                    ok
                } else {
                    derived_ok(data, &f, tsize, lines)
                };
            if ok {
                if inl {
                    derive::mark_inline(&mut f.side);
                }
                let mut p = match p.take() {
                    Some(p) => p,
                    None => super::pack(&f.text, &f.lines(), threads),
                };
                p.streams.push(f.side);
                return Block(p);
            }
        }
    }
    let mut p = super::pack(data, lines, threads);
    p.streams.push(Vec::new());
    Block(p)
}

/// The rules' rewrite of the lines (derive::Packer::forward_par), before any check.
fn forward(data: &[u8], lines: &[&[u8]], rules: &Rules, threads: usize, mk: Option<bool>) -> Option<derive::Fwd> {
    let pk = derive::Packer::new(rules.0);
    match mk {
        Some(m) => pk.mk_known(m),
        None => pk,
    }
    .forward_par(data, lines, threads)
}

/// Tests: the rules' rewrite of `lines` (its text, and its side stream as the put-back reads
/// it: never marked for the line printer), or None (no rule rewrites them).
#[cfg(test)]
pub fn rewritten(data: &[u8], lines: &[&[u8]], rules: &Rules) -> Option<(Vec<u8>, Vec<u8>)> {
    let f = forward(data, lines, rules, 1, None)?;
    let tsize = f.starts.last().map_or(0, |&e| e as usize);
    Some((f.text[..tsize].to_vec(), f.side))
}

/// The lines patterns_max packs: rewritten without their derived fields (Some) when a rule of
/// the run applies and the opener takes the rewrite back (derived_ok; pack_block_as may also
/// keep a rewrite that the line printer puts back, open::inline_ok).
fn rewrite(data: &[u8], lines: &[&[u8]], rules: &Rules, threads: usize, mk: Option<bool>) -> Option<derive::Fwd> {
    let f = forward(data, lines, rules, threads, mk)?;
    let tsize = f.starts.last().map_or(0, |&e| e as usize);
    let size: usize = lines.iter().map(|l| l.len() + 1).sum();
    (tsize <= rewritten_max(size) && derived_ok(data, &f, tsize, lines)).then_some(f)
}

/// At least the number of patterns pack_block(data, lines, rules, 1) makes (the entries of its
/// stream 0), from the first part of its work: the distinct shapes pass 1 finds in the lines
/// it packs, set up as core.rs pack_block sets it up. Its patterns are those shapes after
/// whole tokens and planned words merged some of them (tokens.rs pick_tokens: one merged
/// shape per shape; pack_block: at most one new pattern per merged shape), so never more.
/// May panic where pack_block does (the same work up to there). When `enough` or fewer is
/// all the caller needs to know, pass 1 may run on the first PREFIX lines only: their distinct
/// shapes plus one for each line after them is a bound too (pass 1 reads the lines in order,
/// so it does the same work on them first).
///
/// Also whether pack_block surely does not decline these lines (true; false: it may): core.rs
/// declines a block whose distinct shapes' text is over max(SHAPE_FLOOR or SHAPE_FLOOR_ALONE,
/// bytes / SHAPE_DIV), or whose templates make over max(COLS_FLOOR, bytes / COL_BYTES)
/// columns. Both stay within bounds while that text is at most max(COLS_FLOOR, bytes /
/// COL_BYTES) (the lower of the two limits): a template has one column at most per mark, a
/// mark stands for at least one byte of the shape it comes from (a value, a whole token or a
/// planned word: merge_shape and pick_tokens put one mark for a run of one byte or more), and
/// each template comes from its own shape, so the columns are at most that text's length. A
/// shape is at most its line's length (a value of one byte or more becomes one mark), so a
/// line after the first PREFIX adds at most its length and one byte to the text.
pub fn patterns_max(data: &[u8], lines: &[&[u8]], rules: &Rules, mk: Option<bool>, enough: usize) -> (usize, bool) {
    const PREFIX: usize = 256;
    let f = rewrite(data, lines, rules, 1, mk);
    let (d, l) = match &f {
        Some(f) => (&f.text[..], f.lines()),
        None => (data, lines.to_vec()),
    };
    let mut cfg = super::SplitCfg::get();
    if cfg.names && !super::env_on("FL_NONAMESAMPLE") {
        cfg.names = super::names_in_sample(d, &l, &cfg);
    }
    let bytes: usize = l.iter().map(|x| x.len() + 1).sum();
    let room = super::COLS_FLOOR.max(bytes / super::COL_BYTES);
    if l.len() > PREFIX {
        let p = super::pass1(d, &l[..PREFIX], &cfg, 1);
        let b = p.shapes.at.len() + (l.len() - PREFIX);
        let text = p.shapes.text.len() + l[PREFIX..].iter().map(|x| x.len() + 1).sum::<usize>();
        if b <= enough && text <= room {
            return (b, true);
        }
    }
    let p = super::pass1(d, &l, &cfg, 1);
    (p.shapes.at.len(), p.shapes.text.len() <= room)
}

/// Whether the opener takes the rewrite `f` (check_derived) and turns it back into exactly
/// `lines` (inverts). The lines lie one after the other in `data` (joined: lines_from cuts
/// them so), so the opener's output is compared in one piece. A rewrite whose side stream
/// holds only the bgl rule (the BGL logs) is checked in one pass that writes nothing
/// (derive::inverts_bgl: it makes check_derived's checks of that rule at the places
/// check_derived makes them, refuses where the opener panics, and compares what the opener
/// writes): the same answer as the two.
fn derived_ok(data: &[u8], f: &derive::Fwd, tsize: usize, lines: &[&[u8]]) -> bool {
    let text = &f.text[..tsize];
    let Ok(bits) = derived_bits(text, &f.side) else { return false };
    let exp = joined(data, lines);
    if let (RULE_BGL, Some(exp)) = (bits & !RULE_BGL0, exp) {
        // (a panic counts as no, as in inverts)
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| derive::inverts_bgl(&f.text, tsize, &f.side, exp)));
        return r.unwrap_or(false);
    }
    check_marks(text, bits).is_ok() && inverts(f, tsize, lines, exp)
}

/// `lines`, each followed by '\n' but the last, as one slice of `data`; None when they do not
/// lie that way in `data` (or there are none).
fn joined<'a>(data: &'a [u8], lines: &[&[u8]]) -> Option<&'a [u8]> {
    let base = data.as_ptr() as usize;
    let start = (lines.first()?.as_ptr() as usize).checked_sub(base)?;
    let mut e = start;
    for (j, l) in lines.iter().enumerate() {
        if l.as_ptr() as usize != base.wrapping_add(e) {
            return None;
        }
        e = e.checked_add(l.len())?;
        if j + 1 < lines.len() {
            if data.get(e) != Some(&b'\n') {
                return None;
            }
            e += 1;
        }
    }
    data.get(start..e)
}

/// Runs `g` (which packs one block on this thread) with `f` called once with the block's
/// pattern table (stream 0, its entries each ended by '\n') as soon as the engine has made it,
/// before the block's pass 2 (the engine's TMPL_HOOK). Only the engine's packing on this
/// thread calls it, and it only reads: what `g` packs is the same.
pub fn with_tables_hook<R>(f: fn(&[u8]), g: impl FnOnce() -> R) -> R {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            super::TMPL_HOOK.with(|h| h.set(None));
        }
    }
    super::TMPL_HOOK.with(|h| h.set(Some(f)));
    let _off = Off;
    g()
}

/// Whether derive::Opener (a fresh state, as open_block runs it) puts the rewritten text of
/// `f` (tsize bytes, then zeros) back into exactly `lines`, each followed by '\n' (`exp`:
/// the lines joined, compared in one piece). A panic in the opener counts as no.
fn inverts(f: &derive::Fwd, tsize: usize, lines: &[&[u8]], exp: Option<&[u8]>) -> bool {
    let size: usize = lines.iter().map(|l| l.len() + 1).sum();
    let mut out = vec![0u8; size + 64];
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| derive::Opener::default().inverse(&f.text, tsize, &f.side, &mut out, 0)));
    if r.ok() != Some(size) {
        return false;
    }
    if let Some(e) = exp {
        return same_long(&out[..size - 1], e) && out[size - 1] == b'\n';
    }
    let mut o = 0;
    for l in lines {
        if out[o..o + l.len()] != **l || out[o + l.len()] != b'\n' {
            return false;
        }
        o += l.len() + 1;
    }
    true
}

/// One stream coded the way the engine's code_streams codes it: a stream marked in
/// Packed::raw goes out stored, the column header with the table coder, every other one with
/// the column coder (LEVEL); TABLES streams come back empty (the adapter codes those itself).
fn code_stream(b: &Block, k: usize) -> (Vec<u8>, Option<Parsed>) {
    let s = &b.0.streams[k];
    // (a frame the engine's pack_block coded already for its keyed-slots check: the same bytes)
    if let Some(x) = b.0.pre.iter().find(|x| x.0 == k && x.1 == super::hash_bytes(s)) {
        return (x.2.clone(), x.3.clone().map(Parsed));
    }
    if TABLES.contains(&k) {
        (Vec::new(), None)
    } else if k < 32 && b.0.raw >> k & 1 == 1 {
        (stored(s), None)
    } else if k == HEADER {
        (code_table(s), None)
    } else {
        code_parsed(s)
    }
}

/// A column stream's parsed input (fz), kept for the payload's table bank (make_bank).
pub struct Parsed(fz::Prep);

/// code, with the parsed input when the frame is fz's (as code: stored when not smaller).
pub fn code_parsed(data: &[u8]) -> (Vec<u8>, Option<Parsed>) {
    if data.len() < 16 {
        return (stored(data), None);
    }
    let mut p = fz::prep_fast(data, LEVEL - 40);
    let f = fz::code(&mut p);
    let mut head = Vec::new();
    super::put_varint(&mut head, data.len() as u64);
    if f.len() > data.len() + head.len() { (stored(data), None) } else { (f, Some(Parsed(p))) }
}

/// A payload's table bank (fz::Bank): frequency tables written once, which the column
/// streams of its lines blocks may name instead of writing their own.
pub struct Bank(fz::Bank);

impl Bank {
    /// A bank as table_bank writes it, or None (damaged, or no entry at all).
    pub fn read(b: &[u8]) -> Option<Bank> {
        fz::Bank::read(b).map(Bank)
    }
}

/// The table bank of a payload's lines blocks (2 or more), from their column streams (each
/// frame, its stream number, its parsed input): a frame coded again with it replaces the frame
/// when that is smaller, and the bank is kept only when the frames save more than it takes;
/// else nothing changes (fz::bank_frames). The bank's bytes (empty for none).
pub fn table_bank(fr: &mut [(&mut Vec<u8>, usize, &Parsed)]) -> Vec<u8> {
    let mut v: Vec<(&mut Vec<u8>, usize, &fz::Prep)> = fr.iter_mut().map(|(f, k, p)| (&mut **f, *k, &p.0)).collect();
    fz::bank_frames(&mut v)
}

/// The coded frame of every stream (code_stream), on up to `threads` threads, biggest first,
/// with the parsed input of each frame fz coded (for the payload's table bank). Written here
/// instead of calling the engine's code_streams so that every call of the coder has a
/// constant level.
pub fn code_streams(b: &Block, threads: usize) -> Vec<(Vec<u8>, Option<Parsed>)> {
    let n = b.0.streams.len();
    let threads = threads.clamp(1, n);
    if threads == 1 {
        return (0..n).map(|k| code_stream(b, k)).collect();
    }
    let mut order: Vec<usize> = (0..n).collect();
    super::sort_stable_idx(&mut order, &mut |&x, &y| b.0.streams[x].len() > b.0.streams[y].len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out: Vec<std::sync::Mutex<(Vec<u8>, Option<Parsed>)>> = (0..n).map(|_| std::sync::Mutex::new((Vec::new(), None))).collect();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let q = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if q >= order.len() {
                        break;
                    }
                    let k = order[q];
                    *out[k].lock().unwrap() = code_stream(b, k);
                }
            });
        }
    });
    out.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

/// A frame from a coder, as ozip's compress_with makes it: stored when the data is shorter
/// than 16 bytes or the coded frame is not smaller.
fn frame_or_stored(data: &[u8], coder: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    if data.len() < 16 {
        return stored(data);
    }
    let out = coder();
    let mut head = Vec::new();
    super::put_varint(&mut head, data.len() as u64);
    if out.len() > data.len() + head.len() { stored(data) } else { out }
}

/// Bytes coded with the column coder (one frame): column streams and plain blocks. The same
/// bytes as ozip::compress_with(data, LEVEL, None) (levels 40-43: fz's fast parse, method 6),
/// called directly so that ozip's other levels are left out of fewer-pack.
pub fn code(data: &[u8]) -> Vec<u8> {
    frame_or_stored(data, || fz::compress_fast(data, LEVEL - 40))
}

/// Bytes coded with the table coder (one frame): tables, lists, column headers. The same
/// bytes as ozip::compress_with(data, TABLE_LEVEL or BIG_TABLE_LEVEL, None) (levels 20-28:
/// method 5).
pub fn code_table(data: &[u8]) -> Vec<u8> {
    if data.len() < BIG_TABLE {
        frame_or_stored(data, || ozip::rc5_encode(data, &ozip::parse5_level(data, TABLE_LEVEL)))
    } else {
        frame_or_stored(data, || ozip::rc5_encode(data, &ozip::parse5_level(data, BIG_TABLE_LEVEL)))
    }
}

/// Bytes as a stored frame (for data that will not pack).
pub fn store(data: &[u8]) -> Vec<u8> {
    stored(data)
}

/// The size a frame unpacks to, as it declares it (a frame is: method byte, varint(size), ...).
pub fn declared(frame: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for (i, &x) in frame.iter().skip(1).take(10).enumerate() {
        v |= ((x & 0x7f) as u64) << (7 * i);
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

/// The coded method of stream k of a lines block (TABLE_METHOD or COLUMN_METHOD).
pub fn stream_method(k: usize) -> u8 {
    if TABLES.contains(&k) || k == HEADER { TABLE_METHOD } else { COLUMN_METHOD }
}

/// Whether a frame is stored or coded with `method` (the one fewer-pack writes for its place).
pub fn method_is(frame: &[u8], method: u8) -> bool {
    matches!(frame.first(), Some(&m) if m == 0 || m == method)
}

/// Open a frame that must be stored or coded with `method` (method_is): the bytes, or None
/// when it is damaged, of another method, or declares more than `max` bytes (all checked
/// before anything is decoded, so memory stays within `max`).
pub fn decode_as(frame: &[u8], max: usize, method: u8, bank: Option<(&Bank, usize)>) -> Option<Vec<u8>> {
    if !method_is(frame, method) {
        return None;
    }
    decode(frame, max, bank)
}

/// Open a frame: the bytes, or None when it is damaged or declares more than `max` bytes
/// (checked before anything is decoded, so memory stays within `max`). Only the frames fewer-pack
/// writes open: method 0 (stored), 5 (code_table) and 6 (code); as ozip's decompress_with
/// opens them. A frame of stream k of a lines block may name tables of the payload's bank
/// (`bank`: the bank and k); without one such a frame is refused.
pub fn decode(frame: &[u8], max: usize, bank: Option<(&Bank, usize)>) -> Option<Vec<u8>> {
    let mut p = 1;
    let n = varint(frame, &mut p)?;
    if n > max as u64 {
        return None;
    }
    let (n, body) = (n as usize, &frame[p..]);
    let d = match frame[0] {
        0 => body.get(..n).map(|s| s.to_vec()),
        5 => ozip::rc5_decode(body, n),
        6 => fz::decompress_r(body, n, bank.map(|(b, k)| (&b.0, k))),
        _ => None,
    }?;
    (d.len() == n).then_some(d)
}

fn varint(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for s in (0..64).step_by(7) {
        let x = *b.get(*p)?;
        *p += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

const BAD: &str = "damaged data (block limits)";

/// Most bytes of a block's rewritten text (derived fields) for a block of `size` bytes: the
/// rewrite leaves parts out and adds 2-byte markers (real logs: at most 1.1 x size); the
/// packer does not use a longer rewrite (pack_block), so a reader refuses one.
fn rewritten_max(size: usize) -> usize {
    2 * size + 4096
}

/// The size of the text the engine rebuilds for a block of `size` bytes: the block's lines,
/// or, when the block has derived fields (side stream not empty), its rewritten lines, whose
/// size the side stream gives (derive.rs: varint(rule bits), varint(size), ...): at most
/// rewritten_max(size).
fn rebuilt_size(side: &[u8], size: usize) -> Result<usize, &'static str> {
    if side.is_empty() {
        return Ok(size);
    }
    let mut p = 0;
    varint(side, &mut p).ok_or(BAD)?;
    let t = varint(side, &mut p).ok_or(BAD)?;
    if t == 0 {
        // B5: lines printed with their derived parts (the side stream must read as that)
        return derive::inline_of(side).map(|_| size).ok_or(BAD);
    }
    if t > rewritten_max(size) as u64 {
        return Err(BAD);
    }
    Ok(t as usize)
}

/// The column header's flag bits fewer-pack's packer writes (heads.rs: 2 = splitting columns
/// listed, 4 = every column stores its count, 8 = no kind specials, 16 = rules, 32 = ID
/// columns, ids.rs; 64 = keyed slots, entity.rs: their plan is checked by heads::read). Never:
/// 1 (pattern ids coded with TidModel, off in this engine's setting) or any other bit.
const HEAD_FLAGS: u8 = 2 | 4 | 8 | 16 | 32 | super::entity::FLAG;
/// The column header's flag for the rules section (rules.rs).
const HEAD_RULES: u8 = 16;

/// The limits of a block of `lines` lines whose text is `size` bytes (each line counted with
/// its '\n'), from its streams before coding. They keep opening in proportion to the size of
/// the text the engine rebuilds (`size`, or the rewritten text's size for a block with
/// derived fields: rebuilt_size) whatever the streams hold:
/// - stream 0, the patterns (each ended by '\n'): at least one and at most `lines`, each with
///   at most MAX_MARKS marks, and the biggest mark count times the lines at most 64 x size
///   (+ 64 KiB): the work per line,
/// - stream 2, the column header: at most MAX_COLS columns (the opener's memory per column),
///   only the flag bits of HEAD_FLAGS, and its rules section (check_rules). The engine's own
///   reader (heads.rs `read`) checks the rest of the header as it reads it: at most one
///   column per header byte beyond the patterns' columns, every column's values inside the
///   kinds, texts, cache codes and numbers, and the segments of grouped columns inside the
///   number streams (4 and 7), one after the other: so the copy that puts grouped numbers
///   back in line order (`ungroup`) copies at most streams 4 + 7,
/// - stream 9, the line order: empty (this engine's setting never groups a block's lines by
///   source, so the opener's regrouping is never run on a payload),
/// - stream 10, the derive side stream: a rewritten text of at most 2 x size + 4 KiB
///   (rewritten_max; check_derived checks the text itself once it is rebuilt).
pub fn check_block(streams: &[Vec<u8>], lines: usize, size: usize) -> Result<(), &'static str> {
    if streams.len() != STREAMS {
        return Err(BAD);
    }
    let size = rebuilt_size(&streams[SIDE], size)?;
    let t = &streams[0];
    if t.last() != Some(&b'\n') {
        return Err(BAD);
    }
    // the marks of each pattern (at most one pattern per line)
    let marks = pattern_marks(t, lines)?;
    let kmax = marks.iter().copied().max().unwrap_or(0) as usize;
    let room = 64 * size as u64 + (64 << 10);
    if kmax > MAX_MARKS || (kmax as u64) * (lines as u64) > room {
        return Err(BAD);
    }
    let h = &streams[HEADER];
    let mut p = 0;
    let cols = varint(h, &mut p).ok_or(BAD)?;
    // (then a byte: the grouped column count, and a byte of flags)
    let flags = *h.get(p + 1).ok_or(BAD)?;
    p += 2;
    if cols > MAX_COLS || flags & !HEAD_FLAGS != 0 {
        return Err(BAD);
    }
    if flags & HEAD_RULES != 0 {
        // the splitting columns' list comes first
        if flags & 2 != 0 {
            let n = varint(h, &mut p).ok_or(BAD)?;
            if n > cols {
                return Err(BAD);
            }
            for _ in 0..n {
                varint(h, &mut p).ok_or(BAD)?;
            }
        }
        check_rules(h, &mut p, &marks, &streams[1], lines, size)?;
    }
    if !streams[9].is_empty() {
        return Err(BAD);
    }
    Ok(())
}

/// The mark count of each pattern of a template stream t (patterns ended by '\n'; what
/// follows the last '\n' is not a pattern), at most `lines` patterns (else an error): 16
/// bytes at a time, those without a '\n' by counting their marks at once (the same counts
/// as one byte at a time).
fn pattern_marks(t: &[u8], lines: usize) -> Result<Vec<u32>, &'static str> {
    let mut marks: Vec<u32> = Vec::new();
    let mut k = 0u32;
    let mut step = |c: u8, k: &mut u32| {
        if c == b'\n' {
            if marks.len() == lines {
                return Err(BAD);
            }
            marks.push(*k);
            *k = 0;
        } else if c == MARK {
            *k = k.saturating_add(1);
        }
        Ok(())
    };
    let mut ch = t.chunks_exact(16);
    for x in &mut ch {
        let (nl, mk) = x.iter().fold((0u32, 0u32), |(a, b), &c| (a + (c == b'\n') as u32, b + (c == MARK) as u32));
        if nl == 0 {
            k = k.saturating_add(mk);
        } else {
            for &c in x {
                step(c, &mut k)?;
            }
        }
    }
    for &c in ch.remainder() {
        step(c, &mut k)?;
    }
    Ok(marks)
}

/// The rules' operations (rules.rs): LIN (a sum of earlier slots), HINT and HFRAC (a human
/// size's whole part and fraction, in units 1024^m with m in 1..=MAX_UNIT), NEAR (LIN plus a
/// coded difference).
const RULE_LIN: u8 = 0;
const RULE_HINT: u8 = 1;
const RULE_HFRAC: u8 = 2;
const RULE_NEAR: u8 = 3;
const MAX_UNIT: u64 = 4;

/// The rules section of a block's column header (rules.rs `put_rules`, read by core.rs
/// `line_tids` and heads.rs `read`), read the way `get_rules` reads it, from h[*p..]; then the
/// length of the rules' answers. Every count is checked before the opener allocates by it:
/// - patterns with rules: at most the block's patterns, in increasing order, each with 1 to
///   its mark count rules,
/// - a rule: its slot inside its pattern and after the slot of the rule before; its sources
///   earlier slots; its operation one of the four; its kind a plain or zero-padded number
///   (0, 5..=22); a human size's unit in 1..=MAX_UNIT (the opener divides by 1024 once per
///   unit); signs of -1, 0 or 1,
/// - the answers: one per value at a rule's slot in the block's lines (decoded by the opener
///   into one byte each, a near difference into 8): at most `size`, since every value is at
///   least one byte of the rebuilt text; counted from the pattern of each line in stream 1
///   (core.rs read_ids: HEAD_FLAGS has no TidModel) only when `lines` x the most rules of a
///   pattern is over `size`,
/// - the answers' length inside the header.
fn check_rules(h: &[u8], p: &mut usize, marks: &[u32], ids: &[u8], lines: usize, size: usize) -> Result<(), &'static str> {
    const BAD: &str = "damaged data (rules)";
    let patterns = marks.len() as u64;
    let np = varint(h, p).ok_or(BAD)?;
    if np > patterns {
        return Err(BAD);
    }
    // (pattern, its rule count)
    let mut with: Vec<(usize, u64)> = Vec::with_capacity(np as usize);
    let mut t = 0u64;
    for i in 0..np {
        let dt = varint(h, p).ok_or(BAD)?;
        t = t.checked_add(dt).ok_or(BAD)?;
        if (i > 0 && dt == 0) || t >= patterns {
            return Err(BAD);
        }
        let ns = marks[t as usize] as u64;
        let n = varint(h, p).ok_or(BAD)?;
        if n == 0 || n > ns {
            return Err(BAD);
        }
        let mut last: Option<u64> = None;
        for _ in 0..n {
            let slot = varint(h, p).ok_or(BAD)?;
            if slot >= ns || last.is_some_and(|l| l >= slot) {
                return Err(BAD);
            }
            last = Some(slot);
            let (op, kind) = (*h.get(*p).ok_or(BAD)? & 127, *h.get(*p + 1).ok_or(BAD)?);
            *p += 2;
            if op > RULE_NEAR || !(kind == 0 || (5..=22).contains(&kind)) {
                return Err(BAD);
            }
            let d0 = varint(h, p).ok_or(BAD)?;
            let m = varint(h, p).ok_or(BAD)?;
            if d0 == 0 || d0 > slot || ((op == RULE_HINT || op == RULE_HFRAC) && !(1..=MAX_UNIT).contains(&m)) {
                return Err(BAD);
            }
            if op == RULE_LIN || op == RULE_NEAR {
                varint(h, p).ok_or(BAD)?;
                for _ in 1..3 {
                    let sgn = *h.get(*p).ok_or(BAD)?;
                    *p += 1;
                    if !matches!(sgn, 0 | 1 | 255) {
                        return Err(BAD);
                    }
                    if sgn != 0 {
                        let d = varint(h, p).ok_or(BAD)?;
                        if d == 0 || d > slot {
                            return Err(BAD);
                        }
                    }
                }
            }
        }
        with.push((t as usize, n));
    }
    let alen = varint(h, p).ok_or(BAD)?;
    if alen > (h.len() - *p) as u64 {
        return Err(BAD);
    }
    // at most `lines` x the most rules of a pattern: then no need to count (real blocks)
    let most = with.iter().map(|&(_, n)| n).max().unwrap_or(0);
    if (lines as u64).saturating_mul(most) <= size as u64 {
        return Ok(());
    }
    let mut per = vec![0u32; marks.len()];
    read_ids(ids, lines, &mut per, &mut Vec::with_capacity(lines)).ok_or(BAD)?;
    let answers = with.iter().fold(0u64, |a, &(t, n)| a.saturating_add((per[t] as u64).saturating_mul(n)));
    if answers > size as u64 {
        return Err(BAD);
    }
    Ok(())
}

/// Rebuild a block's lines, each followed by '\n', into out[..size] from its streams (as
/// pack_block made them, TABLES streams included); returns where they end. `out` must have
/// size + 64 bytes. As the engine's open_unit_par: a block with derived fields is opened into
/// `scratch` first (its rewritten lines; grown as needed, kept for the next block), then
/// derive::Opener puts the derived parts back (a fresh state per block). May panic on damaged
/// streams (the adapter catches it and reports an error); call check_block first.
pub fn open_block(mut streams: Vec<Vec<u8>>, lines: usize, out: &mut [u8], size: usize, scratch: &mut Vec<u8>) -> Result<usize, &'static str> {
    let side = std::mem::take(&mut streams[SIDE]);
    let mut p = Packed { streams, lines, raw: 0, alt: Vec::new(), pre: Vec::new(), mk: 0 };
    if side.is_empty() {
        return open::unpack_into(&mut p, out, 0, size);
    }
    if let Some(cp) = derive::inline_of(&side) {
        // B5: the line printer puts the derived parts back
        return open::unpack_into_c(&mut p, out, 0, size, Some(&cp));
    }
    let tsize = rebuilt_size(&side, size)?;
    if scratch.len() < tsize + 128 {
        scratch.resize(tsize + 128, 0);
    }
    let text = &mut scratch[..tsize + 128];
    let e = open::unpack_into(&mut p, text, 0, tsize)?;
    if e != tsize {
        return Err(BAD);
    }
    // (the opener writes up to 64 bytes past the end: derive reads 16 zeros there)
    text[e..e + 64].fill(0);
    derived_back(text, e, &side, out)
}

/// Put the derived parts of the rewritten text text[..e] (zeros after it) back into out:
/// check_derived, then derive::Opener::inverse (a fresh state); for a side stream that holds
/// only the bgl rule, in one pass: inverse_bgl makes check_derived's checks of that rule
/// itself, at the places check_derived makes them, so it refuses every text check_derived
/// refuses, and otherwise writes what inverse writes (the fusedderive patch, open side).
fn derived_back(text: &[u8], e: usize, side: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    let bits = derived_bits(&text[..e], side)?;
    if bits & !RULE_BGL0 == RULE_BGL {
        return derive::Opener::default().inverse_bgl(text, e, side, out, 0).ok_or(BAD_DERIVED);
    }
    check_marks(&text[..e], bits)?;
    Ok(derive::Opener::default().inverse(text, e, side, out, 0))
}

/// open_block as it was before M7 (tests only): ref's opener (open_ref), then check_derived
/// and derive::Opener::inverse for a block with derived fields.
#[cfg(test)]
pub fn open_block_old(mut streams: Vec<Vec<u8>>, lines: usize, out: &mut [u8], size: usize, scratch: &mut Vec<u8>) -> Result<usize, &'static str> {
    let side = std::mem::take(&mut streams[SIDE]);
    let mut p = Packed { streams, lines, raw: 0, alt: Vec::new(), pre: Vec::new(), mk: 0 };
    if side.is_empty() {
        return super::open_ref::unpack_into(&mut p, out, 0, size);
    }
    let tsize = rebuilt_size(&side, size)?;
    if scratch.len() < tsize + 128 {
        scratch.resize(tsize + 128, 0);
    }
    let text = &mut scratch[..tsize + 128];
    let e = super::open_ref::unpack_into(&mut p, text, 0, tsize)?;
    if e != tsize {
        return Err(BAD);
    }
    text[e..e + 64].fill(0);
    check_derived(&text[..e], &side)?;
    Ok(derive::Opener::default().inverse(text, e, &side, out, 0))
}

/// derived_back as it was before M7 (tests only): check_derived, then the inverse.
#[cfg(test)]
pub fn derived_back_old(text: &[u8], e: usize, side: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    check_derived(&text[..e], side)?;
    Ok(derive::Opener::default().inverse(text, e, side, out, 0))
}

/// derived_back (tests only).
#[cfg(test)]
pub fn derived_back_new(text: &[u8], e: usize, side: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    derived_back(text, e, side, out)
}

// ---------------------------------------------------------------- derived fields

/// derive.rs's rule bits that fewer-pack's packer writes (its default rules: bgl 2, spark 16,
/// proxy 32, float 64, oldnew 128, ctime 256; never logcat 1, hdfs 4, rdns 512, and 8, hdfs's
/// ports bit, only as RULE_BGL0).
const RULE_BGL: u64 = 2;
/// Format 2 (fix3): 8 with bgl (hdfs's ports bit without hdfs: derive.rs R_BGL0) marks a
/// block whose bgl lines are in the old form (no fill marker); 8 without bgl is refused.
const RULE_BGL0: u64 = 8;
const RULE_SPARK: u64 = 16;
const RULE_PROXY: u64 = 32;
const RULE_FLOAT: u64 = 64;
const RULE_OLDNEW: u64 = 128;
const RULE_CTIME: u64 = 256;
const RULES: u64 = RULE_BGL | RULE_SPARK | RULE_PROXY | RULE_FLOAT | RULE_OLDNEW | RULE_CTIME;
/// Most different keys of the float rule (and types of the oldnew rule) in a block: the
/// opener keeps them in maps with a fixed hash, so their number bounds the work a crafted
/// set of colliding keys can cause.
const MAX_KEYS: usize = 4096;
/// The proxy rule keeps up to 4,096 open connections in a list (derive.rs PROXY_PENDING)
/// and takes one out of its middle for each close line: the entries moved, added up over a
/// block, may be at most 4 per byte of rewritten text (+ 1 Mi).
const PENDING: usize = 4096;

/// Check a block's rewritten text (derive.rs: markers MK + a letter where the opener puts a
/// derived part back) and its side stream before derive::Opener::inverse runs on them, so
/// that putting the parts back takes time and memory in proportion to the text, whatever the
/// text holds. The packer checks its own rewrites too (pack_block), so every block it writes
/// passes. Each marker is followed the way `inverse` reads it (the same bytes, the same
/// order: where its handler stops reading is where the next marker is looked for):
/// - the side stream: rule bits among RULES (and RULE_BGL0 with RULE_BGL), the text's size,
///   the 2 bgl offsets with bgl, nothing more; every marker's letter belongs to a rule that
///   is on,
/// - b B (bgl): a node then ' ' then a 26-byte timestamp, then the fill marker MK e or MK E
///   (format 2; none with RULE_BGL0), inside the text,
/// - s, f, c, g, t: digits; l: one byte; o: nothing (the open line's app and host are read
///   from the text after it: " - " then a space must come before the next marker, so no
///   text is read twice),
/// - r (spark): two words of at most 24 bytes each (the task and the stage), then ')' or
///   ' ' MK 'u' digits or digits,
/// - w (oldnew): digits, ',' NEW ',old=' and maybe MK 'x',
/// - g and t read the output line back to its start (the float's key, the syslog header): the
///   text from the line's '\n' (which no marker may have read: it goes to the output as it
///   is) to the marker must hold no other marker, so the output there is that text,
/// - at most MAX_KEYS different float keys and oldnew types,
/// - the proxy list's moves (PENDING) and its close lines naming open lines that exist.
pub fn check_derived(text: &[u8], side: &[u8]) -> Result<(), &'static str> {
    check_marks(text, derived_bits(text, side)?)
}

const BAD_DERIVED: &str = "damaged data (derived fields)";

/// The rule bits of a block's side stream, checked (check_derived, the side stream).
fn derived_bits(text: &[u8], side: &[u8]) -> Result<u64, &'static str> {
    const BAD: &str = BAD_DERIVED;
    let mut p = 0;
    let bits = varint(side, &mut p).ok_or(BAD)?;
    let tsize = varint(side, &mut p).ok_or(BAD)?;
    if bits & !(RULES | RULE_BGL0) != 0 || bits & (RULE_BGL | RULE_BGL0) == RULE_BGL0 || tsize != text.len() as u64 {
        return Err(BAD);
    }
    if bits & RULE_BGL != 0 {
        varint(side, &mut p).ok_or(BAD)?;
        varint(side, &mut p).ok_or(BAD)?;
    }
    if p != side.len() {
        return Err(BAD);
    }
    Ok(bits)
}

/// The first MK byte in t (the marker byte of the derive rewrite): 64 bytes per step (the
/// compares ORed, one test), then derive::find_byte from the step that holds one.
fn find_mk(t: &[u8]) -> Option<usize> {
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: NEON is always present on aarch64; each step loads t[i..i + 64], i + 64 <= len
    unsafe {
        use std::arch::aarch64::*;
        let (p, v) = (t.as_ptr(), vdupq_n_u8(derive::MK));
        while i + 64 <= t.len() {
            let x = vld1q_u8_x4(p.add(i));
            let m = vorrq_u8(vorrq_u8(vceqq_u8(x.0, v), vceqq_u8(x.1, v)), vorrq_u8(vceqq_u8(x.2, v), vceqq_u8(x.3, v)));
            if vmaxvq_u8(m) != 0 {
                break;
            }
            i += 64;
        }
    }
    derive::find_byte(&t[i..], derive::MK).map(|q| i + q)
}

/// The last place of byte c in t: 16 bytes per step from the end (an exact byte compare).
fn rfind(t: &[u8], c: u8) -> Option<usize> {
    let mut e = t.len();
    let cc = u128::from_le_bytes([c; 16]);
    while e >= 16 {
        let x = u128::from_le_bytes(t[e - 16..e].try_into().unwrap()) ^ cc;
        // high bit of each byte that is zero (no carry between bytes: each byte's low 7 bits
        // plus 0x7f stays below 0x100)
        const L: u128 = u128::MAX / 255;
        let z = !(((x & (L * 0x7f)) + L * 0x7f) | x) & (L * 0x80);
        if z != 0 {
            return Some(e - 16 + (127 - z.leading_zeros() as usize) / 8);
        }
        e -= 16;
    }
    t[..e].iter().rposition(|&x| x == c)
}

/// a == b, 64 bytes per step (the differences ORed, one test), then the rest.
fn same_long(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: NEON is always present on aarch64; each step loads a[i..i + 64] and b[i..i + 64],
    // i + 64 <= len (both have len bytes)
    unsafe {
        use std::arch::aarch64::*;
        let (p, q) = (a.as_ptr(), b.as_ptr());
        while i + 64 <= a.len() {
            let (x, y) = (vld1q_u8_x4(p.add(i)), vld1q_u8_x4(q.add(i)));
            let d = vorrq_u8(vorrq_u8(veorq_u8(x.0, y.0), veorq_u8(x.1, y.1)), vorrq_u8(veorq_u8(x.2, y.2), veorq_u8(x.3, y.3)));
            if vmaxvq_u8(d) != 0 {
                return false;
            }
            i += 64;
        }
    }
    a[i..] == b[i..]
}

/// check_derived's checks of the text, for a side stream of these rule bits (derived_bits).
fn check_marks(text: &[u8], bits: u64) -> Result<(), &'static str> {
    const BAD: &str = BAD_DERIVED;
    let on = |r: u64| bits & r != 0;
    let n = text.len();
    let digits_end = |mut i: usize| {
        while i < n && text[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    // the first `c` in text[a..a + max] (None: not there)
    let find = |a: usize, max: usize, c: u8| text.get(a..n.min(a.saturating_add(max)))?.iter().position(|&x| x == c).map(|q| a + q);
    let budget = 4 * n as u64 + (1 << 20);
    let (mut pending, mut moves) = (0usize, 0u64);
    let mut keys: std::collections::HashSet<&[u8]> = Default::default();
    let mut last_key: Option<&[u8]> = None;
    let mut types: std::collections::HashSet<u64> = Default::default();
    // i: where the opener looks for the next marker (the end of what the last one read)
    let mut i = 0usize;
    // the end of what the last marker read, and the least place the next marker may be at
    let (mut last_end, mut next_min) = (0usize, 0usize);
    while let Some(q) = find_mk(&text[i..]) {
        let m = i + q;
        if m < next_min {
            return Err(BAD);
        }
        let c = *text.get(m + 1).ok_or(BAD)?;
        let a = m + 2;
        i = match c {
            b'b' | b'B' if on(RULE_BGL) => {
                // (format 2, B5) the fill marker e or E right after the timestamp; none in a
                // block of bgl lines in the old form (RULE_BGL0, fix3)
                let ne = find(a, usize::MAX, b' ').ok_or(BAD)?;
                let f = 2 * !on(RULE_BGL0) as usize;
                if ne + 27 + f > n || (f > 0 && (text[ne + 27] != derive::MK || !matches!(text[ne + 28], b'e' | b'E'))) {
                    return Err(BAD);
                }
                ne + 27 + f
            }
            b's' | b'f' if on(RULE_SPARK) => digits_end(a),
            b'r' if on(RULE_SPARK) => {
                let xe = find(a, 25, b' ').ok_or(BAD)?;
                let ss = xe + 10;
                let se = find(ss, 25, b' ').ok_or(BAD)?;
                if se + 6 >= n {
                    return Err(BAD);
                }
                if text[se + 5] == b')' {
                    se + 5
                } else if text[se + 6] == derive::MK {
                    // " (TID " MK 'u' k: the k-th newest assigned task (the ' ' before the
                    // marker is read too: it must not be a line end)
                    if text[se + 5] != b' ' || text.get(se + 7) != Some(&b'u') {
                        return Err(BAD);
                    }
                    digits_end(se + 8)
                } else {
                    digits_end(se + 6)
                }
            }
            b'o' if on(RULE_PROXY) => {
                // app " - " host ' ': read again by no other marker
                let dash = text[a..].windows(3).position(|w| w == b" - ").ok_or(BAD)? + a;
                next_min = find(dash + 3, usize::MAX, b' ').ok_or(BAD)?;
                if pending == PENDING {
                    moves += PENDING as u64 - 1;
                } else {
                    pending += 1;
                }
                a
            }
            b'c' if on(RULE_PROXY) => {
                let e = digits_end(a);
                // its place in the list, newest first (more than 6 digits: past the list)
                if e - a > 6 {
                    return Err(BAD);
                }
                let r = text[a..e].iter().fold(0usize, |v, &d| v * 10 + (d - b'0') as usize);
                if r >= pending {
                    return Err(BAD);
                }
                pending -= 1;
                moves += r as u64;
                e
            }
            b'l' if on(RULE_PROXY) => a + 1,
            b'g' | b't' if on(if c == b'g' { RULE_FLOAT } else { RULE_CTIME }) => {
                // the line back to its start: its '\n' after what the last marker read
                let ls = rfind(&text[last_end..m], b'\n').map(|x| last_end + x + 1);
                let ls = match ls {
                    Some(s) => s,
                    None if last_end == 0 => 0,
                    None => return Err(BAD),
                };
                if c == b'g' {
                    // KEY " is : " F: the key is the word before " is : "
                    if m < ls + 6 || &text[m - 6..m] != b" is : " {
                        return Err(BAD);
                    }
                    let ke = m - 6;
                    let ks = rfind(&text[ls..ke], b' ').map_or(ls, |x| ls + x + 1);
                    // (a key like the last one is in the set already)
                    if last_key != Some(&text[ks..ke]) {
                        keys.insert(&text[ks..ke]);
                        last_key = Some(&text[ks..ke]);
                    }
                    digits_end(a + (text.get(a) == Some(&b'-')) as usize)
                } else {
                    digits_end(a)
                }
            }
            b'w' if on(RULE_OLDNEW) => {
                let te = digits_end(a);
                // the type as the opener reads it (wrapping past 19 digits)
                types.insert(text[a..te].iter().fold(0u64, |v, &d| v.wrapping_mul(10).wrapping_add((d - b'0') as u64)));
                let ne = find(te + 1, usize::MAX, b',').ok_or(BAD)?;
                let os = ne + 5;
                if text.get(os) == Some(&derive::MK) && text.get(os + 1) == Some(&b'x') { os + 2 } else { os }
            }
            _ => return Err(BAD),
        };
        if i > n || moves > budget || keys.len() > MAX_KEYS || types.len() > MAX_KEYS {
            return Err(BAD);
        }
        last_end = i;
        next_min = next_min.max(i);
    }
    Ok(())
}

/// Tests (M1's checks of the packer's derive check): the rewrite of `lines` by `rules`, its
/// text and side stream, unchecked.
#[cfg(test)]
pub(crate) fn rewrite_raw(data: &[u8], lines: &[&[u8]], rules: &Rules) -> Option<(Vec<u8>, Vec<u8>)> {
    let f = derive::Packer::new(rules.0).forward_par(data, lines, 1)?;
    let t = *f.starts.last()? as usize;
    Some((f.text[..t].to_vec(), f.side))
}

/// Tests: the opener's output for a rewritten text and side stream (check_derived, then the
/// opener; None where it refuses or panics), with `room` bytes of output.
#[cfg(test)]
pub(crate) fn open_raw(text: &[u8], side: &[u8], room: usize) -> Option<Vec<u8>> {
    check_derived(text, side).ok()?;
    let mut t = text.to_vec();
    t.resize(text.len() + 128, 0);
    let mut out = vec![0u8; room];
    let o = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| derive::Opener::default().inverse(&t, text.len(), side, &mut out, 0))).ok()?;
    out.truncate(o);
    Some(out)
}

/// Tests: the packer's answer for the rewrite (text, side) of `lines` (slices of `data`), as
/// it was before M1 (check_derived, then the opener into a zeroed buffer, compared line by
/// line; a panic is a no) and derived_ok's.
#[cfg(test)]
pub(crate) fn check_answers(data: &[u8], text: &[u8], side: &[u8], lines: &[&[u8]]) -> (bool, bool) {
    let mut t = text.to_vec();
    t.resize(text.len() + super::split::ROOM, 0);
    let f = derive::Fwd { text: t, starts: Vec::new(), side: side.to_vec(), marks: 0 };
    let old = check_derived(text, side).is_ok() && {
        let size: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut out = vec![0u8; size + 64];
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| derive::Opener::default().inverse(&f.text, text.len(), side, &mut out, 0)));
        let mut o = 0;
        r.ok() == Some(size)
            && lines.iter().all(|l| {
                let same = out[o..o + l.len()] == **l && out[o + l.len()] == b'\n';
                o += l.len() + 1;
                same
            })
    };
    (old, derived_ok(data, &f, text.len(), lines))
}

/// Tests: check_marks before M1's faster searches (find_mk, rfind, the repeated-key skip), kept
/// to compare with.
#[cfg(test)]
fn check_marks_ref(text: &[u8], bits: u64) -> Result<(), &'static str> {
    const BAD: &str = BAD_DERIVED;
    let on = |r: u64| bits & r != 0;
    let n = text.len();
    let digits_end = |mut i: usize| {
        while i < n && text[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    // the first `c` in text[a..a + max] (None: not there)
    let find = |a: usize, max: usize, c: u8| text.get(a..n.min(a.saturating_add(max)))?.iter().position(|&x| x == c).map(|q| a + q);
    let budget = 4 * n as u64 + (1 << 20);
    let (mut pending, mut moves) = (0usize, 0u64);
    let mut keys: std::collections::HashSet<&[u8]> = Default::default();
    let mut types: std::collections::HashSet<u64> = Default::default();
    // i: where the opener looks for the next marker (the end of what the last one read)
    let mut i = 0usize;
    // the end of what the last marker read, and the least place the next marker may be at
    let (mut last_end, mut next_min) = (0usize, 0usize);
    while let Some(q) = derive::find_byte(&text[i..], derive::MK) {
        let m = i + q;
        if m < next_min {
            return Err(BAD);
        }
        let c = *text.get(m + 1).ok_or(BAD)?;
        let a = m + 2;
        i = match c {
            b'b' | b'B' if on(RULE_BGL) => {
                // (format 2, B5) the fill marker e or E right after the timestamp; none in a
                // block of bgl lines in the old form (RULE_BGL0, fix3)
                let ne = find(a, usize::MAX, b' ').ok_or(BAD)?;
                let f = 2 * !on(RULE_BGL0) as usize;
                if ne + 27 + f > n || (f > 0 && (text[ne + 27] != derive::MK || !matches!(text[ne + 28], b'e' | b'E'))) {
                    return Err(BAD);
                }
                ne + 27 + f
            }
            b's' | b'f' if on(RULE_SPARK) => digits_end(a),
            b'r' if on(RULE_SPARK) => {
                let xe = find(a, 25, b' ').ok_or(BAD)?;
                let ss = xe + 10;
                let se = find(ss, 25, b' ').ok_or(BAD)?;
                if se + 6 >= n {
                    return Err(BAD);
                }
                if text[se + 5] == b')' {
                    se + 5
                } else if text[se + 6] == derive::MK {
                    // " (TID " MK 'u' k: the k-th newest assigned task (the ' ' before the
                    // marker is read too: it must not be a line end)
                    if text[se + 5] != b' ' || text.get(se + 7) != Some(&b'u') {
                        return Err(BAD);
                    }
                    digits_end(se + 8)
                } else {
                    digits_end(se + 6)
                }
            }
            b'o' if on(RULE_PROXY) => {
                // app " - " host ' ': read again by no other marker
                let dash = text[a..].windows(3).position(|w| w == b" - ").ok_or(BAD)? + a;
                next_min = find(dash + 3, usize::MAX, b' ').ok_or(BAD)?;
                if pending == PENDING {
                    moves += PENDING as u64 - 1;
                } else {
                    pending += 1;
                }
                a
            }
            b'c' if on(RULE_PROXY) => {
                let e = digits_end(a);
                // its place in the list, newest first (more than 6 digits: past the list)
                if e - a > 6 {
                    return Err(BAD);
                }
                let r = text[a..e].iter().fold(0usize, |v, &d| v * 10 + (d - b'0') as usize);
                if r >= pending {
                    return Err(BAD);
                }
                pending -= 1;
                moves += r as u64;
                e
            }
            b'l' if on(RULE_PROXY) => a + 1,
            b'g' | b't' if on(if c == b'g' { RULE_FLOAT } else { RULE_CTIME }) => {
                // the line back to its start: its '\n' after what the last marker read
                let ls = text[last_end..m].iter().rposition(|&x| x == b'\n').map(|x| last_end + x + 1);
                let ls = match ls {
                    Some(s) => s,
                    None if last_end == 0 => 0,
                    None => return Err(BAD),
                };
                if c == b'g' {
                    // KEY " is : " F: the key is the word before " is : "
                    if m < ls + 6 || &text[m - 6..m] != b" is : " {
                        return Err(BAD);
                    }
                    let ke = m - 6;
                    let ks = text[ls..ke].iter().rposition(|&x| x == b' ').map_or(ls, |x| ls + x + 1);
                    keys.insert(&text[ks..ke]);
                    digits_end(a + (text.get(a) == Some(&b'-')) as usize)
                } else {
                    digits_end(a)
                }
            }
            b'w' if on(RULE_OLDNEW) => {
                let te = digits_end(a);
                // the type as the opener reads it (wrapping past 19 digits)
                types.insert(text[a..te].iter().fold(0u64, |v, &d| v.wrapping_mul(10).wrapping_add((d - b'0') as u64)));
                let ne = find(te + 1, usize::MAX, b',').ok_or(BAD)?;
                let os = ne + 5;
                if text.get(os) == Some(&derive::MK) && text.get(os + 1) == Some(&b'x') { os + 2 } else { os }
            }
            _ => return Err(BAD),
        };
        if i > n || moves > budget || keys.len() > MAX_KEYS || types.len() > MAX_KEYS {
            return Err(BAD);
        }
        last_end = i;
        next_min = next_min.max(i);
    }
    Ok(())
}

/// Tests: check_derived's answer with check_marks and with check_marks_ref, for a text and side
/// stream.
#[cfg(test)]
pub(crate) fn check_both(text: &[u8], side: &[u8]) -> (Result<(), &'static str>, Result<(), &'static str>) {
    match derived_bits(text, side) {
        Ok(bits) => (check_marks(text, bits), check_marks_ref(text, bits)),
        Err(e) => (Err(e), Err(e)),
    }
}

/// Tests only: where a block's ID columns' new UUIDs start in its texts (heads.rs `tail`), or
/// None when its header does not read.
#[cfg(test)]
pub fn id_tail(streams: &[Vec<u8>], lines: usize) -> Option<usize> {
    let p = Packed { streams: streams.to_vec(), lines, raw: 0, alt: Vec::new(), pre: Vec::new(), mk: 0 };
    let (col_of, first) = super::slot_columns(&p.streams[0]);
    let ids = super::line_tids(&p, &col_of, &first).ok()?;
    super::heads::read(&p.streams, &col_of, &first, &ids).ok().map(|h| h.tail)
}

#[cfg(test)]
mod tests {
    /// find_mk, rfind and same_long give what the plain searches and compare give, for every
    /// length around their step sizes and every place.
    #[test]
    fn fast_searches_same_as_plain() {
        let mut x = 0x5ea4_c4e5_0000_0001u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..3000 {
            let len = (rnd() % 300) as usize;
            let dense = round % 4;
            let mut t: Vec<u8> = (0..len).map(|_| if rnd() % (4 + dense * 40) == 0 { [1u8, b'\n', b' '][(rnd() % 3) as usize] } else { b"ab \n\x01xyz0123456789"[(rnd() % 16) as usize] | 0x20 }).collect();
            if round % 7 == 0 && len > 0 {
                let k = (rnd() as usize) % len;
                t[k] = super::derive::MK;
            }
            assert_eq!(super::find_mk(&t), t.iter().position(|&c| c == super::derive::MK), "{t:?}");
            for c in [b'\n', b' ', 1u8, 0x2a] {
                assert_eq!(super::rfind(&t, c), t.iter().rposition(|&b| b == c), "{t:?} {c}");
            }
            let mut u = t.clone();
            assert!(super::same_long(&t, &u));
            if len > 0 {
                let k = (rnd() as usize) % len;
                u[k] ^= 1 << (rnd() % 8);
                assert_eq!(super::same_long(&t, &u), t == u);
                assert!(!super::same_long(&t, &u[..len - 1]));
            }
        }
    }

    /// pattern_marks gives what one byte at a time gives (counts, and the error past `lines`
    /// patterns), on random template streams.
    #[test]
    fn pattern_marks_same_as_byte_steps() {
        let mut x = 0x0123_4567_89AB_CDEFu64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..20_000 {
            let n = (next() % 300) as usize;
            let nl_rate = 1 + next() % 40;
            let t: Vec<u8> = (0..n)
                .map(|_| match next() % (nl_rate + 6) {
                    0 => b'\n',
                    1 | 2 => super::MARK,
                    _ => b'a' + (next() % 26) as u8,
                })
                .collect();
            let lines = if round % 3 == 0 { (next() % 8) as usize } else { usize::MAX };
            let mut want: Result<Vec<u32>, &str> = Ok(Vec::new());
            let mut k = 0u32;
            for &c in &t {
                if c == b'\n' {
                    let m = want.as_mut().unwrap();
                    if m.len() == lines {
                        want = Err(super::BAD);
                        break;
                    }
                    m.push(k);
                    k = 0;
                } else if c == super::MARK {
                    k = k.saturating_add(1);
                }
            }
            assert_eq!(super::pattern_marks(&t, lines), want, "{t:?}");
        }
    }

    fn xs(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    /// A random block's pattern ids (numbered in order of first use, as the packer numbers
    /// them; with `odd`, any order): n lines, at most `p` patterns, in runs, cycles and noise.
    fn rand_ids(x: &mut u64, n: usize, p: u32, odd: bool) -> Vec<u32> {
        let mut v: Vec<u32> = Vec::with_capacity(n);
        let mut seen = 0u32;
        while v.len() < n {
            let r = xs(x);
            let t = match r % 5 {
                0 if seen < p => seen,
                1 if v.len() >= 4 => v[v.len() - 1 - (r >> 8) as usize % 4],
                2 if !v.is_empty() => {
                    // a cycle through earlier lines
                    let k = (r >> 8) as usize % v.len().min(16) + 1;
                    let a = v.len() - k;
                    for i in 0..k.min(n - v.len()) {
                        let t = v[a + i];
                        v.push(t);
                    }
                    continue;
                }
                _ if odd => (r >> 20) as u32 % p,
                _ => (r >> 20) as u32 % seen.max(1),
            };
            seen += (t == seen) as u32;
            v.push(t);
        }
        v
    }

    fn plain(ids: &[u32]) -> Vec<u8> {
        let mut b = Vec::new();
        for &t in ids {
            super::super::put_varint(&mut b, t as u64);
        }
        b
    }

    /// read_ids on `b` must not panic; when it accepts, the ids are below the pattern count,
    /// `lines` of them, counted in per.
    fn check_read(b: &[u8], lines: usize, patterns: usize) -> Option<Vec<u32>> {
        let mut per = vec![0u32; patterns];
        let mut out = Vec::new();
        super::read_ids(b, lines, &mut per, &mut out)?;
        assert_eq!(out.len(), lines);
        let mut want = vec![0u32; patterns];
        for &t in &out {
            want[t as usize] += 1;
        }
        assert_eq!(per, want);
        Some(out)
    }

    /// The pattern-id forms of core.rs (id_forms, read_ids): every form gives the ids back,
    /// and the cases the format refuses are refused.
    #[test]
    fn pattern_id_forms() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..3000 {
            let n = 1 + (xs(&mut x) % 3000) as usize;
            let p = 1 + (xs(&mut x) % 400) as u32;
            let ids = rand_ids(&mut x, n, p, round % 4 == 0);
            let pl = plain(&ids);
            let forms = super::super::id_forms(&pl, n, p as usize);
            // (blocks of up to 32K lines: both forms are tried)
            assert_eq!(forms.len(), 2);
            assert_eq!(check_read(&pl, n, p as usize).as_deref(), Some(&ids[..]));
            for (k, f) in forms.iter().enumerate() {
                assert_eq!(&f[..2], &[0x81 + k as u8, 0]);
                assert_eq!(check_read(f, n, p as usize).as_deref(), Some(&ids[..]), "form {}", k + 1);
                // one line fewer is fine (the rest is not read); one more line is refused
                assert!(check_read(f, n - 1, p as usize).is_some());
                assert!(check_read(f, n + 1, p as usize).is_none());
            }
        }
        // form 2: a guess on the first line, or before the previous pattern had a follower
        assert!(check_read(&[0x82, 0, 0], 1, 4).is_none());
        assert!(check_read(&[0x82, 0, 3, 0], 2, 4).is_none());
        assert_eq!(check_read(&[0x82, 0, 3, 4, 3, 0], 4, 4), Some(vec![1, 2, 1, 2]));
        // the next new pattern: 0, 1, 2, ... and never at or past the pattern count
        assert_eq!(check_read(&[0x81, 0, 0, 0, 1, 0], 4, 3), Some(vec![0, 1, 0, 2]));
        assert!(check_read(&[0x81, 0, 0, 0, 0], 3, 2).is_none());
        assert!(check_read(&[0x82, 0, 1, 1, 1], 3, 2).is_none());
        // an id at or past the pattern count; unknown forms; a plain stream as before
        assert!(check_read(&[0x81, 0, 4], 1, 3).is_none());
        assert!(check_read(&[0x82, 0, 5], 1, 3).is_none());
        for f in [0x80u8, 0x83, 0x90, 0xff] {
            assert!(check_read(&[f, 0, 1, 1], 1, 300).is_none());
        }
        assert_eq!(check_read(&[0x80, 1, 0x81, 1], 2, 300), Some(vec![128, 129]));
        assert!(check_read(&[], 1, 3).is_none());
        assert!(check_read(&[0xff; 12], 1, 3).is_none());
    }

    /// Fuzz of read_ids (FEWER_FUZZ rounds, default 3000): each form of random pattern ids,
    /// damaged (bits flipped, cut short, bytes put in or set to 0 or 1, big varints, the
    /// marker changed) and read with the right, a smaller or a bigger line or pattern count:
    /// never a panic, and every stream it accepts gives ids below the pattern count.
    #[test]
    #[ignore]
    fn fuzz_pattern_ids() {
        let rounds: usize = std::env::var("FEWER_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
        let mut x = 0x1234_5678_9abc_def1u64;
        let (mut ok, mut refused) = (0usize, 0usize);
        for round in 0..rounds {
            let n = 1 + (xs(&mut x) % 2000) as usize;
            let p = 1 + (xs(&mut x) % 300) as u32;
            let ids = rand_ids(&mut x, n, p, round % 3 == 0);
            let pl = plain(&ids);
            let mut all = super::super::id_forms(&pl, n, p as usize);
            all.push(pl);
            for f in &all {
                for _ in 0..8 {
                    let mut s = f.clone();
                    for _ in 0..1 + xs(&mut x) % 4 {
                        let r = xs(&mut x);
                        let i = if s.is_empty() { 0 } else { (r >> 8) as usize % s.len() };
                        match r % 8 {
                            0 if !s.is_empty() => s[i] ^= 1 << ((r >> 40) % 8),
                            1 => s.truncate(i),
                            2 => s.insert(i, (r >> 32) as u8),
                            3 if !s.is_empty() => s[i] = ((r >> 32) % 2) as u8,
                            4 => {
                                let mut v = Vec::new();
                                super::super::put_varint(&mut v, (r >> 16) % (1 << (r % 60)));
                                s.splice(i..(i + 1).min(s.len()), v);
                            }
                            5 if s.len() >= 2 => s[0] = 0x80 | (r >> 32) as u8,
                            _ => {}
                        }
                    }
                    let r = xs(&mut x);
                    let lines = match r % 4 {
                        0 => n,
                        1 => n.saturating_sub((r >> 8) as usize % 5),
                        2 => n + (r >> 8) as usize % 5,
                        _ => (r >> 8) as usize % (2 * n + 2),
                    };
                    let pats = match (r >> 32) % 3 {
                        0 => p as usize,
                        1 => 1 + (r >> 40) as usize % p as usize,
                        _ => p as usize + (r >> 40) as usize % 4,
                    };
                    match check_read(&s, lines, pats) {
                        Some(_) => ok += 1,
                        None => refused += 1,
                    }
                }
            }
        }
        eprintln!("fuzz_pattern_ids: {rounds} rounds, {ok} accepted, {refused} refused");
    }
}
