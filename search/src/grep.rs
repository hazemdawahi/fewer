//! `fewer grep` and `fewer index`: fewer-pack's grep and index, with the same
//! arguments, the same output bytes and the same exit status. They prove
//! the shared pipeline, and later they replace fewer-pack's own grep and index.
//!
//! What fewer-pack's main.rs keeps to itself is copied here, line for line: its argument parser
//! (for these two commands), `open_archive`, `archive_in`, `parse_file_meta`, and the
//! `plan` closure of cmd_grep, which becomes `Planner` so the tools (find.rs) plan their
//! searches the same way. checks/same_as_grep.py runs both programs on the same archives.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::exit;

use crate::container::{self, ArchiveReader, Ent, Run};
use crate::folder::{self, show_name};
use crate::{MISMATCH, engine, index, search};

/// A job of the search: about this many bytes of lines (cmd_grep's JOB).
const JOB: usize = 1 << 20;

fn usage_error(msg: &str) -> ! {
    eprintln!("fewer: {msg}");
    eprintln!("Try 'fewer --help' for more information.");
    exit(2)
}

fn warn(m: &str) {
    eprintln!("fewer: warning: {m}");
}

/// Which command the options are for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Grep,
    Index,
}

impl Cmd {
    fn name(self) -> &'static str {
        match self {
            Cmd::Grep => "grep",
            Cmd::Index => "index",
        }
    }
}

pub struct Opts {
    pub threads: usize,
    pub icase: bool,
    pub paths: Vec<OsString>,
}

/// fewer-pack's `parse` for grep and index (main.rs): the same options, the same usage errors.
pub fn parse(args: Vec<OsString>, cmd: Cmd) -> Opts {
    let mut o = Opts { threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), icase: false, paths: Vec::new() };
    let mut i = 0;
    let mut only_paths = false;
    let num = |v: Option<&OsString>, what: &str| -> usize {
        let s = v.and_then(|s| s.to_str()).unwrap_or_else(|| usage_error(&format!("{what} needs a value")));
        match s.parse::<usize>() {
            Ok(n) if n >= 1 => n.min(1024),
            _ => usage_error(&format!("bad value for {what}: {s}")),
        }
    };
    let unknown = |opt: &str| -> ! { usage_error(&format!("unknown option {opt} for fewer {}", cmd.name())) };
    while i < args.len() {
        let a = &args[i];
        let s = a.as_bytes();
        i += 1;
        if only_paths || s == b"-" || !s.starts_with(b"-") {
            o.paths.push(a.clone());
            continue;
        }
        if s.starts_with(b"--") {
            // --name or --name=value
            let (name, inline) = match s.iter().position(|&c| c == b'=') {
                Some(p) => (&s[..p], Some(OsStr::from_bytes(&s[p + 1..]).to_os_string())),
                None => (s, None),
            };
            let shown = String::from_utf8_lossy(name).into_owned();
            let mut value = |what: &str| -> OsString {
                match &inline {
                    Some(v) => v.clone(),
                    None => {
                        let v = args.get(i).cloned().unwrap_or_else(|| usage_error(&format!("{what} needs a value")));
                        i += 1;
                        v
                    }
                }
            };
            let takes_value = name == b"--threads";
            if inline.is_some() && !takes_value {
                unknown(&String::from_utf8_lossy(s));
            }
            match name {
                b"--" => only_paths = true,
                b"--help" => {
                    println!("{}", crate::HELP);
                    exit(0)
                }
                b"--version" => {
                    println!("fewer {}", env!("CARGO_PKG_VERSION"));
                    exit(0)
                }
                b"--threads" => o.threads = num(Some(&value("--threads")), "--threads"),
                b"--ignore-case" if cmd == Cmd::Grep => o.icase = true,
                _ => unknown(&shown),
            }
            continue;
        }
        // short options, possibly joined: -i, -t4, -t 4
        let mut k = 1;
        while k < s.len() {
            match s[k] {
                b'h' => {
                    println!("{}", crate::HELP);
                    exit(0)
                }
                b'V' => {
                    println!("fewer {}", env!("CARGO_PKG_VERSION"));
                    exit(0)
                }
                b'i' if cmd == Cmd::Grep => o.icase = true,
                b't' => {
                    let rest = &s[k + 1..];
                    let val: OsString = if !rest.is_empty() {
                        OsStr::from_bytes(rest).to_os_string()
                    } else {
                        let v = args.get(i).cloned().unwrap_or_else(|| usage_error("-t needs a value"));
                        i += 1;
                        v
                    };
                    o.threads = num(Some(&val), "-t");
                    break;
                }
                c => unknown(&format!("-{}", c as char)),
            }
            k += 1;
        }
    }
    o
}

/// An archive being read (from a file or standard input).
pub type Archive = ArchiveReader<Box<dyn container::Input>>;

/// An archive's bytes read through a 1 MB buffer (fewer-pack's archive_in).
pub fn archive_in(r: Box<dyn container::Src>) -> Box<dyn container::Input> {
    Box::new(BufReader::with_capacity(1 << 20, r))
}

/// An archive opened for reading, with the metadata of the open file (None for stdin):
/// fewer-pack's open_archive.
pub fn open_archive(inp: &OsStr, force: bool) -> Result<(Archive, Option<fs::Metadata>), String> {
    if inp == "-" {
        return Ok((ArchiveReader::open(archive_in(Box::new(io::stdin())))?, None));
    }
    let md = fs::metadata(inp).map_err(|e| format!("cannot read: {e}"))?;
    if md.is_dir() {
        return Err("is a folder, not an archive".into());
    }
    if !md.is_file() && !force {
        return Err("is not a regular file; skipped (use -f to read it)".into());
    }
    let f = fs::File::open(inp).map_err(|e| format!("cannot read: {e}"))?;
    let fmd = f.metadata().map_err(|e| format!("cannot read: {e}"))?;
    Ok((ArchiveReader::open(archive_in(Box::new(f)))?, Some(fmd)))
}

/// A file's mode and modification time (seconds, nanoseconds).
pub type FileMeta = (u32, (i64, u32));

/// The meta record of a file archive (fewer-pack's parse_file_meta): None when it is empty
/// (packed from a stream), the mode and modification time, or an error when it is anything
/// else.
pub fn parse_file_meta(m: &[u8]) -> Result<Option<FileMeta>, String> {
    if m.is_empty() {
        return Ok(None);
    }
    let bad = || "damaged archive (file information)".to_string();
    let mut p = 0;
    let mode = container::get_varint(m, &mut p).ok_or_else(bad)?;
    let s = container::unzz(container::get_varint(m, &mut p).ok_or_else(bad)?);
    let ns = container::get_varint(m, &mut p).ok_or_else(bad)?;
    if p != m.len() || mode > 0o7777 || ns > 999_999_999 {
        return Err(bad());
    }
    Ok(Some((mode as u32, (s, ns as u32))))
}

pub fn idx_path(inp: &OsStr) -> PathBuf {
    let mut p = inp.to_os_string();
    p.push(".idx");
    PathBuf::from(p)
}

/// Lines searched by the planner itself (a line carried from one block into the next) and
/// by the search's jobs: (whole lines, the file they belong to, the output) -> lines found.
pub type Lines<'a> = &'a (dyn Fn(&[u8], usize, &mut Vec<u8>) -> usize + Sync);
/// What the planner adds for a block left unopened (its number over the archive from 0,
/// its size): the tools' line counts (find.rs); nothing for grep.
pub type Skipped<'a> = &'a mut dyn FnMut(usize, usize, &mut Vec<Ent>) -> Result<(), String>;
/// The planner's own check before each block, given the block's number over the archive
/// (find.rs: the time limit); an error stops the search there, after everything before it
/// is written.
pub type Check<'a> = &'a dyn Fn(usize) -> Result<(), String>;

/// cmd_grep's `plan` closure (fewer-pack's main.rs) as a struct: it keeps what runs from block to
/// block (a line cut by a block's end, the file places of a folder archive) and cuts each
/// opened block into entries for container::search_archive: output it makes itself (the
/// carried line) and runs of whole lines of about JOB bytes for the pool.
pub struct Planner<'a> {
    /// the sizes of the archive's files in order (one file of u64::MAX for a file archive)
    files: Vec<u64>,
    fi: usize,
    left: u64,
    carry: Vec<u8>,
    lines: Lines<'a>,
    skipped: Option<Skipped<'a>>,
    check: Option<Check<'a>>,
    /// the next block's number over the archive
    block: usize,
}

impl<'a> Planner<'a> {
    pub fn new(files: Vec<u64>, lines: Lines<'a>, skipped: Option<Skipped<'a>>, check: Option<Check<'a>>) -> Planner<'a> {
        let left = files.first().copied().unwrap_or(0);
        Planner { files, fi: 0, left, carry: Vec::new(), lines, skipped, check, block: 0 }
    }

    /// Plan the next block in order: its text, or None when it was left unopened.
    pub fn plan(&mut self, seg: Option<&[u8]>, size: usize, ents: &mut Vec<Ent>) -> Result<(), String> {
        let ordinal = self.block;
        if let Some(c) = self.check {
            c(ordinal)?;
        }
        self.block += 1;
        let files = &self.files;
        let Some(seg) = seg else {
            if let Some(f) = self.skipped.as_mut() {
                f(ordinal, size, ents)?;
            }
            // a block no line of which holds the text, after a line end and ending with one
            // (so nothing is carried over): only the file places move on
            let mut d = size as u64;
            while d > 0 {
                if self.fi >= files.len() {
                    return Err("damaged archive (more data than its file table lists)".into());
                }
                let take = self.left.min(d);
                d -= take;
                self.left -= take;
                if self.left == 0 {
                    self.fi += 1;
                    self.left = files.get(self.fi).copied().unwrap_or(0);
                }
            }
            return Ok(());
        };
        let mut runs: Vec<Run> = Vec::new();
        let mut bytes = 0usize;
        let mut d = 0usize;
        while d < seg.len() {
            if self.fi >= files.len() {
                if !runs.is_empty() {
                    ents.push(Ent::Runs(std::mem::take(&mut runs)));
                }
                return Err("damaged archive (more data than its file table lists)".into());
            }
            let (p0, p1) = (d, d + self.left.min((seg.len() - d) as u64) as usize);
            d = p1;
            self.left -= (p1 - p0) as u64;
            let file_ends = self.left == 0;
            // finish the line carried over from the block before (only a block's first part
            // can carry one: the others start a file)
            let mut b0 = p0;
            if !self.carry.is_empty() {
                let nl = seg[p0..p1].iter().position(|&c| c == b'\n');
                let cut = nl.map_or(p1, |p| p0 + p + 1);
                self.carry.extend_from_slice(&seg[p0..cut]);
                b0 = cut;
                if nl.is_some() || (b0 == p1 && file_ends) {
                    if !runs.is_empty() {
                        ents.push(Ent::Runs(std::mem::take(&mut runs)));
                        bytes = 0;
                    }
                    let mut b = Vec::new();
                    let n = (self.lines)(&self.carry, self.fi, &mut b);
                    ents.push(Ent::Out(b, n));
                    self.carry.clear();
                }
            }
            // keep an unfinished last line for the next block; the whole lines go to the
            // pool in runs of about JOB bytes, cut at line ends
            let whole = if file_ends { p1 } else { seg[b0..p1].iter().rposition(|&c| c == b'\n').map_or(b0, |p| b0 + p + 1) };
            let mut a = b0;
            while a < whole {
                let room = JOB - bytes;
                let e = if whole - a <= room { whole } else { seg[a + room..whole].iter().position(|&c| c == b'\n').map_or(whole, |p| a + room + p + 1) };
                runs.push((a, e, self.fi));
                bytes += e - a;
                a = e;
                if bytes >= JOB {
                    ents.push(Ent::Runs(std::mem::take(&mut runs)));
                    bytes = 0;
                }
            }
            self.carry.extend_from_slice(&seg[whole..p1]);
            if file_ends {
                self.fi += 1;
                self.left = files.get(self.fi).copied().unwrap_or(0);
            }
        }
        if !runs.is_empty() {
            ents.push(Ent::Runs(runs));
        }
        Ok(())
    }

    /// After the last block: the line still carried (the text did not end with '\n'), with
    /// its file.
    pub fn rest(&mut self) -> Option<(usize, Vec<u8>)> {
        if self.carry.is_empty() {
            return None;
        }
        Some((self.fi.min(self.files.len().saturating_sub(1)), std::mem::take(&mut self.carry)))
    }
}

/// fewer-pack grep, through the shared pipeline: the same output and exit status.
pub fn cmd_grep(o: &Opts) -> i32 {
    if o.paths.len() < 2 {
        usage_error("usage: fewer grep [-i] TEXT FILE.cmf...");
    }
    let nd = search::Needle::new(o.paths[0].as_bytes(), o.icase);
    // blocks of lines that cannot hold the text are not opened (search::Filter)
    let flt = search::Filter::new(o.paths[0].as_bytes(), o.icase, engine::value_names());
    let may = |p: &[u8], tw: Option<(&[u8], &[u8])>| flt.may(p, tw);
    // the keys every line holding the text holds (FILE.cmf.idx: index.rs)
    let keys = index::needle_keys(o.paths[0].as_bytes());
    let many = o.paths.len() > 2;
    let mut out = io::BufWriter::with_capacity(1 << 20, io::stdout().lock());
    let mut matched = 0usize;
    let mut failed = false;
    for inp in &o.paths[1..] {
        let r = (|| -> Result<(), String> {
            let (ar, fmd) = open_archive(inp, true)?;
            let ip = idx_path(inp);
            let ix = fmd.filter(|_| !keys.is_empty()).and_then(|m| index::load(&ip, m.len()));
            let skip = |g: usize, h: (u64, u64, u32)| {
                let r = ix.as_ref()?.skips(g, h, &keys);
                if r.is_none() {
                    warn(&format!("{}: {MISMATCH}; searching without it", ip.display()));
                }
                r
            };
            let aname = if many { format!("{}:", show_name(inp.as_bytes())) } else { String::new() };
            // file boundaries of a folder archive: lines never run across files
            let files: Vec<(u64, Vec<u8>)> = if ar.is_folder() {
                let entries = folder::table_from_meta(&ar.meta, o.threads)?;
                entries.iter().filter(|e| e.kind == folder::Kind::File && e.size > 0).map(|e| (e.size, e.path.clone())).collect()
            } else {
                parse_file_meta(&ar.meta)?;
                vec![(u64::MAX, Vec::new())]
            };
            if files.is_empty() {
                return Ok(()); // only folders, links and empty files
            }
            // what each file's matching lines start with
            let prefixes: Vec<Vec<u8>> = files
                .iter()
                .map(|f| {
                    let mut p = aname.as_bytes().to_vec();
                    if !f.1.is_empty() {
                        p.extend_from_slice(show_name(&f.1).as_bytes());
                        p.push(b':');
                    }
                    p
                })
                .collect();
            let lines = |text: &[u8], f: usize, out: &mut Vec<u8>| search::grep_lines(text, &nd, &prefixes[f], out);
            // the runs of whole lines planned below, searched on all cores
            let search = |text: &[u8], runs: &[Run], out: &mut Vec<u8>| -> usize { runs.iter().map(|&(a, b, f)| lines(&text[a..b], f, out)).sum() };
            let mut planner = Planner::new(files.iter().map(|f| f.0).collect(), &lines, None, None);
            let mut plan = |seg: Option<&[u8]>, size: usize, ents: &mut Vec<Ent>| planner.plan(seg, size, ents);
            let mut write = |b: &[u8], n: usize| -> Result<(), String> {
                matched += n;
                out.write_all(b).map_err(|e| format!("cannot write output: {e}"))
            };
            // (no thread is started for blocks when the index rules out all of them)
            let t = if ix.as_ref().is_some_and(|x| x.rules_out_all(&keys)) { 1 } else { o.threads };
            container::search_archive(ar, t, &may, ix.as_ref().map(|_| &skip as container::IndexSkip), &mut plan, &search, &mut write)?;
            if let Some((f, carry)) = planner.rest() {
                let mut buf = Vec::new();
                matched += lines(&carry, f, &mut buf);
                out.write_all(&buf).map_err(|e| format!("cannot write output: {e}"))?;
            }
            Ok(())
        })();
        // what this archive matched is written out before the next archive is read
        let _ = out.flush();
        if let Err(e) = r {
            eprintln!("fewer: {}: {e}", inp.to_string_lossy());
            failed = true;
        }
    }
    if let Err(e) = out.flush() {
        eprintln!("fewer: cannot write output: {e}");
        return 2;
    }
    // as grep: 2 on any error, else 0 when a line matched, 1 when none did
    if failed {
        2
    } else if matched > 0 {
        0
    } else {
        1
    }
}

/// fewer-pack index: FILE.cmf.idx for each archive (index.rs), replacing an older one; the same
/// bytes as `fewer-pack index`.
pub fn cmd_index(o: &Opts) -> i32 {
    if o.paths.is_empty() {
        usage_error("usage: fewer index FILE.cmf...");
    }
    let mut failed = false;
    for inp in &o.paths {
        let r = (|| -> Result<(), String> {
            if inp == "-" {
                return Err("an index is made for an archive file".into());
            }
            let (ar, fmd) = open_archive(inp, false)?;
            let d = index::build(ar, fmd.map_or(0, |m| m.len()), o.threads)?;
            crate::store::write_atomic(&idx_path(inp), &d).map_err(|e| format!("cannot write the index: {e}"))
        })();
        if let Err(e) = r {
            eprintln!("fewer: {}: {e}", inp.to_string_lossy());
            failed = true;
        }
    }
    failed as i32
}

/// The bytes `fewer-pack index` writes for the archive file at `path` (add.rs).
pub fn build_index(path: &Path, threads: usize) -> Result<Vec<u8>, String> {
    let (ar, fmd) = open_archive(path.as_os_str(), false)?;
    index::build(ar, fmd.map_or(0, |m| m.len()), threads)
}
