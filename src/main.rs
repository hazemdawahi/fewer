//! fewer-pack: a lossless compressor for log files. See README.md.
//!
//! fewer-pack FILE...           pack each FILE to FILE.cmf (the input is kept)
//! fewer-pack DIR               pack a folder to DIR.cmf
//! fewer-pack -d FILE.cmf...    unpack
//! fewer-pack list | cat | grep | index ...
//! fewer-pack add | run | memory ...   fewer's commands (search/): fewer-pack execs it
#![cfg_attr(fewer_small, feature(optimize_attribute))]

// newer slice helpers (as_chunks, is_multiple_of) are avoided so fewer-pack builds with Rust 1.85
#![allow(clippy::manual_is_multiple_of, clippy::chunks_exact_to_as_chunks, clippy::needless_range_loop)]

mod batch;
mod container;
mod crc;
mod engine;
mod folder;
mod index;
mod portable;
mod search;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufReader, IsTerminal, Read, Write};
use crate::portable::OsStrExt;
use crate::portable::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Instant;

use container::{ArchiveReader, ArchiveWriter, AtomicFile, FLAG_FOLDER};
use folder::show_name;

const SUFFIX: &str = ".cmf";

const HELP: &str = "\
fewer-pack - lossless compressor for log files

Usage:
  fewer-pack [options] FILE...        pack each FILE to FILE.cmf (the input is kept)
  fewer-pack [options] DIR            pack a folder (files, subfolders, links) to DIR.cmf
  fewer-pack -d [options] FILE.cmf... unpack (a file archive to FILE, a folder archive to DIR)
  fewer-pack [options] < in > out     pack standard input to standard output (-d: unpack)
  fewer-pack list FILE.cmf...         list what an archive holds
  fewer-pack cat FILE.cmf [PATH]      print the unpacked content (PATH: one file of a folder archive)
  fewer-pack grep [-i] TEXT FILE.cmf...  print the lines that contain TEXT
  fewer-pack index FILE.cmf...        write FILE.cmf.idx, which grep then uses to skip the
                                 blocks that cannot hold TEXT (words, IDs, numbers)

Options:
  -d, --decompress   unpack
  -c, --stdout       write to standard output (-o - does the same)
  -o, --output OUT   output name (one input only)
  -f, --force        overwrite outputs; read .cmf files, symlinks and other non-regular
                     files given by name (pipes, devices)
  --rm               delete each input after success, if it did not change meanwhile
                     (not with -c)
  -t, --threads N    threads to use (default: all cores)
  -q, --quiet        no summary line (warnings are still shown)
  -i                 (grep) ignore upper / lower case
  -V, --version      print the version
  -h, --help         print this help

A path of - means standard input. Exit status: 0 ok, 1 error, 2 wrong usage
(grep: 0 when some line matched, 1 when none did, 2 on an error).";

#[cfg_attr(fewer_small, optimize(size))]
fn usage_error(msg: &str) -> ! {
    eprintln!("fewer-pack: {msg}");
    eprintln!("Try 'fewer-pack --help' for more information.");
    exit(2)
}

/// Which command the options are for (each takes only its own options).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Main,
    List,
    Cat,
    Grep,
    Index,
}

impl Cmd {
    #[cfg_attr(fewer_small, optimize(size))]
    fn name(self) -> &'static str {
        match self {
            Cmd::Main => "fewer-pack",
            Cmd::List => "fewer-pack list",
            Cmd::Cat => "fewer-pack cat",
            Cmd::Grep => "fewer-pack grep",
            Cmd::Index => "fewer-pack index",
        }
    }
}

#[derive(Default)]
struct Opts {
    decompress: bool,
    stdout: bool,
    output: Option<PathBuf>,
    force: bool,
    rm: bool,
    threads: usize,
    quiet: bool,
    icase: bool,
    paths: Vec<OsString>,
}

#[cfg_attr(fewer_small, optimize(size))]
fn parse(args: Vec<OsString>, cmd: Cmd) -> Opts {
    let mut o = Opts { threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), ..Default::default() };
    let main = cmd == Cmd::Main;
    let mut i = 0;
    let mut only_paths = false;
    let num = |v: Option<&OsString>, what: &str| -> usize {
        let s = v.and_then(|s| s.to_str()).unwrap_or_else(|| usage_error(&format!("{what} needs a value")));
        match s.parse::<usize>() {
            Ok(n) if n >= 1 => n.min(1024),
            _ => usage_error(&format!("bad value for {what}: {s}")),
        }
    };
    let unknown = |opt: &str| -> ! {
        if main {
            usage_error(&format!("unknown option {opt}"))
        } else {
            usage_error(&format!("unknown option {opt} for {}", cmd.name()))
        }
    };
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
            let takes_value = matches!(name, b"--output" | b"--threads");
            if inline.is_some() && !takes_value {
                unknown(&String::from_utf8_lossy(s));
            }
            match name {
                b"--" => only_paths = true,
                b"--help" => {
                    println!("{HELP}");
                    exit(0)
                }
                b"--version" => {
                    println!("fewer-pack {}", env!("CARGO_PKG_VERSION"));
                    exit(0)
                }
                b"--threads" => o.threads = num(Some(&value("--threads")), "--threads"),
                b"--decompress" | b"--unpack" if main => o.decompress = true,
                b"--stdout" if main => o.stdout = true,
                b"--force" if main => o.force = true,
                b"--rm" if main => o.rm = true,
                b"--quiet" if main => o.quiet = true,
                b"--keep" if main => {}
                b"--output" if main => o.output = Some(PathBuf::from(value("--output"))),
                b"--ignore-case" if cmd == Cmd::Grep => o.icase = true,
                _ => unknown(&shown),
            }
            continue;
        }
        // short options, possibly joined: -dc, -t4, -o out
        let mut k = 1;
        while k < s.len() {
            match s[k] {
                b'h' => {
                    println!("{HELP}");
                    exit(0)
                }
                b'V' => {
                    println!("fewer-pack {}", env!("CARGO_PKG_VERSION"));
                    exit(0)
                }
                b'd' if main => o.decompress = true,
                b'c' if main => o.stdout = true,
                b'f' if main => o.force = true,
                b'q' if main => o.quiet = true,
                b'k' if main => {}
                b'i' if cmd == Cmd::Grep => o.icase = true,
                c @ (b'o' | b't') if main || c == b't' => {
                    let rest = &s[k + 1..];
                    let val: OsString = if !rest.is_empty() {
                        OsStr::from_bytes(rest).to_os_string()
                    } else {
                        let v = args.get(i).cloned().unwrap_or_else(|| usage_error(&format!("-{} needs a value", c as char)));
                        i += 1;
                        v
                    };
                    if c == b'o' {
                        o.output = Some(PathBuf::from(val));
                    } else {
                        o.threads = num(Some(&val), "-t");
                    }
                    break;
                }
                c => unknown(&format!("-{}", c as char)),
            }
            k += 1;
        }
    }
    // -o - is standard output
    if o.output.as_deref() == Some(Path::new("-")) {
        o.output = None;
        o.stdout = true;
    }
    o
}

#[cfg_attr(fewer_small, optimize(size))]
fn main() {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // add, run and memory are fewer's commands: it gets this command line and the
    // environment as they are, before the two calls below (a signal blocked here would stay
    // blocked through exec, in the commands that `run` starts). `fewer-pack ./run` packs a file
    // named run.
    if matches!(args.first().map(|a| a.as_bytes()), Some(b"add" | b"run" | b"memory")) {
        exec_search(&args);
    }
    // before any thread starts (the signal handler's thread is the first)
    engine::clear_env_switches();
    container::install_signal_handlers();
    let cmd = args.first().and_then(|a| a.to_str()).unwrap_or("").to_string();
    // a panic = "abort" build (a smaller build of the packer): a panic gives a clear error and the exit
    // status of an error, never a crash (container::install_panic_exit)
    #[cfg(panic = "abort")]
    container::install_panic_exit(if cmd == "grep" { 2 } else { 1 });
    let code = match cmd.as_str() {
        "list" | "cat" | "grep" | "index" => {
            args.remove(0);
            match cmd.as_str() {
                "list" => cmd_list(&parse(args, Cmd::List)),
                "cat" => cmd_cat(&parse(args, Cmd::Cat)),
                "index" => cmd_index(&parse(args, Cmd::Index)),
                _ => cmd_grep(&parse(args, Cmd::Grep)),
            }
        }
        _ => {
            let o = parse(args, Cmd::Main);
            if o.output.is_some() && o.paths.len() > 1 {
                usage_error("-o works with one input only");
            }
            if o.output.is_some() && o.stdout {
                usage_error("-o and -c cannot be used together");
            }
            if o.rm && o.stdout {
                warn("--rm does nothing with -c: the inputs are kept");
            } else if o.rm && (o.paths.is_empty() || o.paths.iter().all(|p| p == "-")) {
                warn("--rm does nothing for standard input");
            }
            run(&o)
        }
    };
    exit(code)
}

/// `fewer-pack add|run|memory ...`: become fewer with the same arguments, the one in this
/// program's folder (std::env::current_exe), else the first on PATH. execv and execvp are
/// called directly, not through std::process, to stay small. Returns only to end with the
/// error.
#[cfg_attr(fewer_small, optimize(size))]
fn exec_search(args: &[OsString]) -> ! {
    use std::ffi::{c_char, CString};
    unsafe extern "C" {
        fn execv(path: *const c_char, argv: *const *const c_char) -> i32;
        fn execvp(file: *const c_char, argv: *const *const c_char) -> i32;
    }
    const NAME: &str = "fewer";
    // (arguments and paths from the system never hold a 0 byte)
    let c = |b: &[u8]| CString::new(b).unwrap_or_default();
    let all: Vec<CString> = std::iter::once(c(NAME.as_bytes())).chain(args.iter().map(|a| c(a.as_bytes()))).collect();
    let mut argv: Vec<*const c_char> = all.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    if let Ok(me) = std::env::current_exe() {
        let next = c(me.with_file_name(NAME).as_os_str().as_bytes());
        // SAFETY: C strings and a null-terminated array of them, alive during the call;
        // execv returns only when it fails
        unsafe { execv(next.as_ptr(), argv.as_ptr()) };
    }
    // SAFETY: as above; execvp looks for the name in the folders of PATH
    unsafe { execvp(all[0].as_ptr(), argv.as_ptr()) };
    eprintln!("fewer-pack: add, run and memory need fewer, which was not found next to fewer-pack or on PATH");
    exit(2)
}

/// Pack or unpack every input; an error on one input does not stop the others.
#[cfg_attr(fewer_small, optimize(size))]
fn run(o: &Opts) -> i32 {
    if o.paths.is_empty() && io::stdin().is_terminal() && !o.force {
        usage_error("nothing to do: give files or folders, or send data to standard input");
    }
    let inputs: Vec<OsString> = if o.paths.is_empty() { vec![OsString::from("-")] } else { o.paths.clone() };
    if o.stdout && !o.decompress && inputs.len() > 1 {
        // joined file archives unpack as one; joined folder archives would not
        if let Some(d) = inputs.iter().find(|p| *p != "-" && fs::metadata(p).is_ok_and(|m| m.is_dir())) {
            usage_error(&format!("{} is a folder: with -c, pack a folder on its own (one archive per folder)", d.to_string_lossy()));
        }
    }
    // runs of inputs that may be worked on at the same time (batch.rs); the rest one by one
    let joins = batch_inputs(o, &inputs);
    let mut failed = false;
    let mut k = 0;
    while k < inputs.len() {
        let e = (k..inputs.len()).find(|&j| joins[j].is_none()).unwrap_or(inputs.len());
        if e > k + 1 {
            let part = &inputs[k..e];
            let sizes: Vec<u64> = joins[k..e].iter().map(|s| s.unwrap_or(0)).collect();
            // (a panic names the input that stopped: batch::on_panic)
            container::set_current(OsStr::new(""), !o.decompress);
            let one = |i: usize| if o.decompress { unpack_one(o, &part[i]) } else { pack_one(o, &part[i]) };
            failed |= batch::run(part, &sizes, o.threads, o.rm, &one);
            k = e;
            continue;
        }
        let inp = &inputs[k];
        k += 1;
        container::set_current(inp, !o.decompress);
        let r = if o.decompress { unpack_one(o, inp) } else { pack_one(o, inp) };
        if let Err(e) = r {
            let name = if inp == "-" { "(stdin)".to_string() } else { inp.to_string_lossy().into_owned() };
            eprintln!("fewer-pack: {name}: {e}");
            failed = true;
        }
    }
    failed as i32
}

/// Which inputs may be worked on at the same time as their neighbours (batch.rs), with their
/// sizes: regular files given by name (a symbolic link only with -f) and, when unpacking,
/// file archives. Anything else (standard input, folders, folder archives, names that
/// cannot be read now, or one thread) is worked on alone, in its place, as before; so is
/// every input when two inputs are the same file or name, or an output is an input or has
/// the name of another input or output (an input may need what another makes or removes).
fn batch_inputs(o: &Opts, inputs: &[OsString]) -> Vec<Option<u64>> {
    let mut out = vec![None; inputs.len()];
    if o.threads < 2 || inputs.len() < 2 || o.output.is_some() {
        return out;
    }
    let mut files = HashSet::new();
    let mut names = HashSet::new();
    let mut outputs = Vec::new();
    let mut clash = false;
    for (k, inp) in inputs.iter().enumerate() {
        let Some((size, ids, keys, dest)) = batch_input(o, inp) else { continue };
        for id in ids {
            clash |= !files.insert(id);
        }
        for key in keys {
            clash |= !names.insert(key);
        }
        outputs.extend(dest);
        out[k] = Some(size);
    }
    // an output that exists now must not be one of the inputs
    for d in &outputs {
        if let Ok(m) = fs::symlink_metadata(d) {
            clash |= files.contains(&(m.dev(), m.ino()));
        }
    }
    if clash {
        out.fill(None);
    }
    out
}

/// An input that may join a batch: its weight (the bytes to pack; to unpack: the first
/// segment's, as unpacking holds about one segment's blocks), its file (and link), its name
/// and its output's name (as name_key), and its output.
type BatchInput = (u64, Vec<(u64, u64)>, Vec<(u64, u64, Vec<u8>)>, Option<PathBuf>);

fn batch_input(o: &Opts, inp: &OsStr) -> Option<BatchInput> {
    if inp == "-" {
        return None;
    }
    let path = Path::new(inp);
    let lmd = fs::symlink_metadata(path).ok()?;
    let link = lmd.file_type().is_symlink();
    if link && !o.force {
        return None;
    }
    let md = fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let mut weight = md.len();
    let dest = if o.decompress {
        // a file archive (a folder archive makes a folder)
        let mut r = BufReader::with_capacity(4096, fs::File::open(path).ok()?);
        let mut h = [0u8; 6];
        r.read_exact(&mut h).ok()?;
        if h[..4] != container::MAGIC[..] || h[5] & FLAG_FOLDER != 0 {
            return None;
        }
        let n = read_varint(&mut r)?;
        io::copy(&mut (&mut r).take(n.checked_add(4)?), &mut io::sink()).ok()?;
        weight = read_varint(&mut r)?;
        if o.stdout { None } else { Some(strip_suffix(path)?) }
    } else {
        if path.as_os_str().as_bytes().ends_with(SUFFIX.as_bytes()) && !o.force && !o.stdout {
            return None;
        }
        (!o.stdout).then(|| add_suffix(path))
    };
    let mut ids = vec![(md.dev(), md.ino())];
    if link {
        ids.push((lmd.dev(), lmd.ino()));
    }
    let mut keys = vec![name_key(path)?];
    if let Some(d) = &dest {
        keys.push(name_key(d)?);
    }
    Some((weight, ids, keys, dest))
}

/// A varint read from `r` (None: cut short or too long).
fn read_varint(r: &mut impl Read) -> Option<u64> {
    let mut v = 0u64;
    for s in (0..64).step_by(7) {
        let mut b = [0u8; 1];
        r.read_exact(&mut b).ok()?;
        v |= ((b[0] & 0x7f) as u64) << s;
        if b[0] < 0x80 {
            return Some(v);
        }
    }
    None
}

/// A name as (device, inode) of its folder and its last part.
fn name_key(p: &Path) -> Option<(u64, u64, Vec<u8>)> {
    let name = p.file_name()?.as_bytes().to_vec();
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let md = fs::metadata(dir).ok()?;
    Some((md.dev(), md.ino(), name))
}

#[cfg_attr(fewer_small, optimize(size))]
fn warn(m: &str) {
    batch::say(format!("fewer-pack: warning: {m}"));
}

// ---------------------------------------------------------------- output sinks

enum Sink {
    Std(io::BufWriter<io::StdoutLock<'static>>),
    /// standard output for one of several inputs worked on at once (in input order)
    Ordered(batch::Out),
    File(AtomicFile),
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Std(w) => w.write(b),
            Sink::Ordered(w) => w.write(b),
            Sink::File(f) => f.write(b),
        }
    }
    fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        match self {
            Sink::Std(w) => w.write_all(b),
            Sink::Ordered(w) => w.write_all(b),
            Sink::File(f) => f.write_all(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Std(w) => w.flush(),
            Sink::Ordered(w) => w.flush(),
            Sink::File(f) => f.flush(),
        }
    }
}

/// The mode of a new file when there is no other to copy: 0666 less the umask.
#[cfg_attr(fewer_small, optimize(size))]
fn default_mode() -> u32 {
    0o666 & !container::umask_bits()
}

impl Sink {
    #[cfg_attr(fewer_small, optimize(size))]
    fn stdout() -> Result<Sink, String> {
        if batch::in_batch() {
            return Ok(Sink::Ordered(batch::Out::new()));
        }
        Ok(Sink::Std(io::BufWriter::with_capacity(1 << 20, io::stdout().lock())))
    }

    /// An output file; forced to disk before it is finished when the input is deleted
    /// next (--rm).
    #[cfg_attr(fewer_small, optimize(size))]
    fn file(dest: &Path, o: &Opts) -> Result<Sink, String> {
        Ok(Sink::File(AtomicFile::create(dest, o.force, o.rm)?))
    }

    /// Finish the output; a file gets `mode` (and `mtime` when given). With several inputs
    /// at once, outputs are finished in input order (batch.rs).
    #[cfg_attr(fewer_small, optimize(size))]
    fn finish(self, mode: u32, mtime: Option<(i64, u32)>) -> Result<(), String> {
        match self {
            Sink::Std(mut w) => w.flush().map_err(|e| format!("cannot write output: {e}")),
            Sink::Ordered(w) => w.finish().map_err(|e| format!("cannot write output: {e}")),
            Sink::File(f) => batch::in_turn(|| f.finish(mode, mtime)),
        }
    }
}

fn mtime_of(md: &fs::Metadata) -> (i64, u32) {
    (md.mtime(), md.mtime_nsec().clamp(0, 999_999_999) as u32)
}

/// The meta record of a file archive: the file's mode and modification time.
#[cfg_attr(fewer_small, optimize(size))]
fn file_meta(md: &fs::Metadata) -> Vec<u8> {
    let mut m = Vec::new();
    let (s, ns) = mtime_of(md);
    container::put_varint(&mut m, (md.mode() & 0o7777) as u64);
    container::put_varint(&mut m, container::zz(s));
    container::put_varint(&mut m, ns as u64);
    m
}

/// A file's mode and modification time (seconds, nanoseconds).
type FileMeta = (u32, (i64, u32));

/// The meta record of a file archive: None when it is empty (packed from a stream), the mode
/// and modification time, or an error when it is anything else (never a default mode or
/// time in place of what a damaged record says).
#[cfg_attr(fewer_small, optimize(size))]
fn parse_file_meta(m: &[u8]) -> Result<Option<FileMeta>, String> {
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

#[cfg_attr(fewer_small, optimize(size))]
fn add_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(SUFFIX);
    PathBuf::from(s)
}

#[cfg_attr(fewer_small, optimize(size))]
fn strip_suffix(p: &Path) -> Option<PathBuf> {
    let b = p.as_os_str().as_bytes();
    let base = b.strip_suffix(SUFFIX.as_bytes())?;
    if base.is_empty() || base.ends_with(b"/") {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(base)))
}

/// A folder name without trailing '/' (but "/" stays "/").
#[cfg_attr(fewer_small, optimize(size))]
fn trim_slashes(p: &Path) -> PathBuf {
    let b = p.as_os_str().as_bytes();
    let n = b.len() - b.iter().rev().take_while(|&&c| c == b'/').count();
    if n == 0 && !b.is_empty() { PathBuf::from("/") } else { PathBuf::from(OsStr::from_bytes(&b[..n])) }
}

/// Refuse an output that is the input itself (same device and inode, as zstd does).
#[cfg_attr(fewer_small, optimize(size))]
fn not_the_input(dest: &Path, input: &[&fs::Metadata]) -> Result<(), String> {
    if let Ok(d) = fs::symlink_metadata(dest) {
        if input.iter().any(|m| (m.dev(), m.ino()) == (d.dev(), d.ino())) {
            return Err(format!("the output {} is the input itself; not overwriting it", dest.display()));
        }
    }
    Ok(())
}

/// Is the file at `path` still exactly the one that was read (`read` = metadata of the open
/// file; `link` = the path's own metadata then, when it was a symlink)?
#[cfg_attr(fewer_small, optimize(size))]
fn still_same(path: &Path, read: &fs::Metadata, link: Option<&fs::Metadata>) -> bool {
    let id = |m: &fs::Metadata| (m.dev(), m.ino());
    let Ok(now_l) = fs::symlink_metadata(path) else { return false };
    if let Some(l) = link {
        if !now_l.file_type().is_symlink() || id(&now_l) != id(l) {
            return false;
        }
    }
    let Ok(now) = fs::metadata(path) else { return false };
    now.is_file() && id(&now) == id(read) && now.len() == read.len() && mtime_of(&now) == mtime_of(read)
}

#[cfg_attr(fewer_small, optimize(size))]
fn summary(o: &Opts, name: &str, inb: u64, outb: u64, t0: Instant) {
    if o.quiet {
        return;
    }
    // (several inputs at once: not counting the wait for the inputs before it)
    let secs = t0.elapsed().saturating_sub(batch::parked()).as_secs_f64();
    let ratio = if outb > 0 { inb as f64 / outb as f64 } else { 0.0 };
    batch::say(format!(
        "fewer-pack: {name}: {} -> {} bytes ({}x) in {} s ({} MB/s)",
        group(inb),
        group(outb),
        fixed(ratio, 2),
        fixed(secs, 2),
        fixed(inb as f64 / secs.max(1e-9) / 1e6, 0)
    ));
}

/// `x` with `d` decimals (d <= 2), the text of format!("{x:.d$}"): the exact binary value
/// rounded half to even. For summary()'s numbers (a ratio, seconds, MB/s: finite, not negative,
/// times 10^d below 2^64; larger ones would print as 2^64 - 1), so std's fixed-precision float
/// printer is not in the program (the shortest one stays: derive.rs uses it). Checked against
/// std (tests: fixed_same_as_std).
#[cfg_attr(fewer_small, optimize(size))]
fn fixed(x: f64, d: u32) -> String {
    let b = x.to_bits();
    let (be, frac) = ((b >> 52 & 0x7ff) as i32, b & ((1 << 52) - 1));
    // |x| = m * 2^e, and x * 10^d = v * 2^e (v below 2^60)
    let (m, e) = if be == 0 { (frac, -1074) } else { (frac | 1 << 52, be - 1075) };
    let v = m * 10u64.pow(d);
    let mut n = if e >= 0 {
        v.saturating_mul(1 << e.min(63))
    } else if e < -60 {
        0
    } else {
        let s = -e as u32;
        let (q, r, half) = (v >> s, v & ((1 << s) - 1), 1u64 << (s - 1));
        q + (r > half || (r == half && q & 1 == 1)) as u64
    };
    let mut t = Vec::with_capacity(24);
    let mut k = 0;
    loop {
        t.push(b'0' + (n % 10) as u8);
        n /= 10;
        k += 1;
        if k == d {
            t.push(b'.');
        }
        if n == 0 && k > d {
            break;
        }
    }
    if b >> 63 == 1 {
        t.push(b'-');
    }
    t.iter().rev().map(|&c| c as char).collect()
}

#[cfg_attr(fewer_small, optimize(size))]
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg_attr(fewer_small, optimize(size))]
fn refuse_terminal(o: &Opts, packing: bool) -> Result<(), String> {
    if packing && !o.force && io::stdout().is_terminal() {
        return Err("not writing packed data to a terminal (redirect it, or use -f)".into());
    }
    Ok(())
}

// ---------------------------------------------------------------- pack

#[cfg_attr(fewer_small, optimize(size))]
fn pack_one(o: &Opts, inp: &OsStr) -> Result<(), String> {
    let t0 = Instant::now();
    if inp == "-" {
        if io::stdin().is_terminal() && !o.force {
            return Err("nothing to pack: standard input is a terminal (give a file, or use -f)".into());
        }
        let sink = match &o.output {
            Some(p) => Sink::file(p, o)?,
            None => {
                refuse_terminal(o, true)?;
                Sink::stdout()?
            }
        };
        let (inb, outb) = pack_to(sink, Src::Stream(Box::new(io::stdin()), None), 0, &[], o.threads, default_mode(), None)?;
        summary(o, "(stdin)", inb, outb, t0);
        return Ok(());
    }
    let path = Path::new(inp);
    let lmd = fs::symlink_metadata(path).map_err(|e| format!("cannot read: {e}"))?;
    let link = lmd.file_type().is_symlink();
    if link && !o.force {
        return Err("is a symbolic link; skipped (use -f to follow it)".into());
    }
    let md = fs::metadata(path).map_err(|e| format!("cannot read: {e}"))?;
    if md.is_dir() {
        // (a file replaced by a folder meanwhile: never packed beside other inputs, as --rm
        // of a folder changes the current folder)
        if batch::in_batch() {
            return Err("changed while opening it; try again".into());
        }
        return pack_folder(o, path, t0);
    }
    if !md.is_file() && !o.force {
        // a pipe would block until something writes to it
        return Err("is not a regular file; skipped (use -f to read it)".into());
    }
    if path.as_os_str().as_bytes().ends_with(SUFFIX.as_bytes()) && !o.force && !o.stdout {
        return Err(format!("already has the {SUFFIX} suffix (use -d to unpack, or -f to pack it again)"));
    }
    let dest = o.output.clone().unwrap_or_else(|| add_suffix(path));
    let sink = if o.stdout {
        refuse_terminal(o, true)?;
        Sink::stdout()?
    } else {
        container::check_output(&dest, o.force)?;
        not_the_input(&dest, &[&lmd, &md])?;
        Sink::file(&dest, o)?
    };
    let f = fs::File::open(path).map_err(|e| format!("cannot read: {e}"))?;
    // what is read is the open file (the name may change meanwhile)
    let fmd = f.metadata().map_err(|e| format!("cannot read: {e}"))?;
    if fmd.is_dir() {
        return Err("changed while opening it; try again".into());
    }
    let mode = fmd.mode() & 0o7777;
    let mtime = mtime_of(&fmd);
    // a regular file is mapped into memory (no copy); anything else is read as a stream
    let map = if fmd.is_file() { container::Mapped::open(&f, fmd.len()) } else { None };
    let (sink, inb, outb) = match &map {
        Some(m) => pack_into(sink, Src::Bytes(m.bytes()), 0, &file_meta(&fmd), o.threads)?,
        None => {
            let size = if fmd.is_file() { Some(fmd.len()) } else { None };
            pack_into(sink, Src::Stream(Box::new(f), size), 0, &file_meta(&fmd), o.threads)?
        }
    };
    // the input is not needed any more (with several inputs at once the output may wait for
    // its turn to be named)
    drop(map);
    sink.finish(mode, Some(mtime))?;
    summary(o, &inp.to_string_lossy(), inb, outb, t0);
    if o.rm && !o.stdout {
        // only what was packed is deleted: not a file that grew, was rewritten or replaced
        if !fmd.is_file() {
            return Err("packed, but kept: not a regular file".into());
        }
        if !still_same(path, &fmd, link.then_some(&lmd)) {
            return Err("packed, but kept: it changed while packing (the archive holds it as it was before)".into());
        }
        let again = fs::File::open(path).map_err(|e| format!("packed, but kept: cannot read it again: {e}"))?;
        let changed = || !still_same(path, &fmd, link.then_some(&lmd));
        verify_archive(&dest, Box::new(again), o.threads, &changed)?;
        if changed() {
            return Err("packed, but kept: it changed while the archive was checked (the archive holds it as it was before)".into());
        }
        fs::remove_file(path).map_err(|e| format!("packed, but cannot delete the input: {e}"))?;
    }
    Ok(())
}

/// What to pack: bytes in memory, or a stream (with its size when known).
enum Src<'a> {
    Bytes(&'a [u8]),
    Stream(Box<dyn Read + Send + 'a>, Option<u64>),
}

/// Pack into `sink`, which as a file gets `mode` (and `mtime`): (bytes in, bytes out).
#[cfg_attr(fewer_small, optimize(size))]
fn pack_to(sink: Sink, src: Src, flags: u8, meta: &[u8], threads: usize, mode: u32, mtime: Option<(i64, u32)>) -> Result<(u64, u64), String> {
    let (sink, inb, outb) = pack_into(sink, src, flags, meta, threads)?;
    sink.finish(mode, mtime)?;
    Ok((inb, outb))
}

/// pack_to without finishing the output: (the output, bytes in, bytes out).
#[cfg_attr(fewer_small, optimize(size))]
fn pack_into(mut sink: Sink, src: Src, flags: u8, meta: &[u8], threads: usize) -> Result<(Sink, u64, u64), String> {
    let mut aw = ArchiveWriter::new(&mut sink, flags, meta).map_err(|e| format!("cannot write output: {e}"))?;
    let inb = match src {
        Src::Bytes(b) => container::pack_bytes(b, &mut aw, threads)?,
        Src::Stream(r, size) => container::pack_stream(r, size, &mut aw, threads)?,
    };
    let (_, outb) = aw.finish().map_err(|e| format!("cannot write output: {e}"))?;
    Ok((sink, inb, outb))
}

/// --rm: before anything is deleted, the new archive is opened again and compared with the
/// input byte for byte (`input` reads it again). On any difference or error the archive is
/// removed and the input kept; a Ctrl-C or a panic meanwhile does the same.
#[cfg_attr(fewer_small, optimize(size))]
fn verify_archive(archive: &Path, input: Box<dyn Read + Send + '_>, threads: usize, changed: &dyn Fn() -> bool) -> Result<(), String> {
    let p = container::probation(archive);
    let r = same_bytes(archive, input, threads);
    let r = match r {
        Ok(()) => Ok(()),
        // an input that cannot be read again (it changed after it was packed) is not a fault
        // of the archive: keep both, delete nothing
        Err((true, e)) => Err(format!("packed, but kept: an input changed after it was packed ({e}); nothing was deleted")),
        Err((false, _)) if changed() => Err("packed, but kept: the input changed while the archive was checked; nothing was deleted".into()),
        Err((false, e)) => {
            let _ = fs::remove_file(archive);
            Err(format!("packed, but the archive did not open to the exact input ({e}); the archive was removed and the input kept (please report this)"))
        }
    };
    p.release();
    r
}

/// Ok, or (whether reading the input again failed, the message).
#[cfg_attr(fewer_small, optimize(size))]
fn same_bytes(archive: &Path, input: Box<dyn Read + Send + '_>, threads: usize) -> Result<(), (bool, String)> {
    let f = fs::File::open(archive).map_err(|e| (false, e.to_string()))?;
    let ar = ArchiveReader::open(archive_in(Box::new(f))).map_err(|e| (false, e))?;
    let mut same = SameAs { r: buffered(input), buf: Vec::new(), input_error: None };
    if let Err(e) = container::unpack_stream(ar, &mut same, threads) {
        return Err(match same.input_error {
            Some(ie) => (true, ie),
            None => (false, e),
        });
    }
    match same.r.read(&mut [0u8; 1]) {
        Ok(0) => Ok(()),
        Ok(_) => Err((false, "the archive holds less".into())),
        Err(e) => Err((true, e.to_string())),
    }
}

/// A writer that only checks that what it is given equals what `r` reads.
struct SameAs<'a> {
    r: Box<dyn Read + Send + 'a>,
    buf: Vec<u8>,
    /// reading the input failed (other than at its end): it changed after it was packed
    input_error: Option<String>,
}

impl Write for SameAs<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.resize(b.len(), 0);
        if let Err(e) = self.r.read_exact(&mut self.buf) {
            if e.kind() != io::ErrorKind::UnexpectedEof {
                self.input_error = Some(e.to_string());
            }
            return Err(e);
        }
        if self.buf[..] != *b {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "different bytes"));
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg_attr(fewer_small, optimize(size))]
fn pack_folder(o: &Opts, dir: &Path, t0: Instant) -> Result<(), String> {
    let mut warn_scan = |m: String| warn(&m);
    let entries = folder::scan(dir, &mut warn_scan)?;
    let mut archive = None;
    let sink = if o.stdout {
        refuse_terminal(o, true)?;
        Sink::stdout()?
    } else {
        let dest = match &o.output {
            Some(p) => p.clone(),
            None => {
                let b = dir.as_os_str().as_bytes();
                let trimmed = &b[..b.len() - b.iter().rev().take_while(|&&c| c == b'/').count()];
                let last = trimmed.rsplit(|&c| c == b'/').next().unwrap_or(b"");
                if trimmed.is_empty() || last == b"." || last == b".." {
                    return Err("give an output name with -o (or use -c) to pack this folder".into());
                }
                add_suffix(Path::new(OsStr::from_bytes(trimmed)))
            }
        };
        container::check_output(&dest, o.force)?;
        // never pack the archive into itself
        if let Ok(dmd) = fs::metadata(dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))) {
            if entries.iter().any(|e| e.kind == folder::Kind::Dir && e.id == (dmd.dev(), dmd.ino())) {
                return Err(match o.output {
                    Some(_) => format!("the output {} is inside the folder being packed", dest.display()),
                    None => format!("the output {} would be inside the folder; use -o to put it elsewhere", dest.display()),
                });
            }
        }
        let s = Sink::file(&dest, o)?;
        archive = Some(dest);
        s
    };
    let meta = folder::table_meta(&entries, o.threads);
    let total = folder::total_size(&entries);
    // the archive is no more open to others than the folder itself
    let mode = default_mode() & entries[0].mode & 0o666;
    let mut reader = folder::FolderReader::new(dir, &entries);
    let (inb, outb) = pack_to(sink, Src::Stream(Box::new(&mut reader), Some(total)), FLAG_FOLDER, &meta, o.threads, mode, None)?;
    if inb != total {
        return Err("files changed while packing; pack again".into());
    }
    for g in &reader.grew {
        warn(&format!("{} grew while packing; only its first bytes were packed", show_name(g)));
    }
    let files = entries.iter().filter(|e| e.kind == folder::Kind::File).count();
    summary(o, &format!("{} ({files} files)", dir.display()), inb, outb, t0);
    if o.rm && !o.stdout {
        if let Some(a) = &archive {
            verify_archive(a, Box::new(&mut folder::FolderReader::new(dir, &entries)), o.threads, &|| false)?;
        }
        let kept = folder::remove_packed(dir, &entries);
        if !kept.is_empty() {
            for k in &kept {
                eprintln!("fewer-pack: {k}");
            }
            return Err(format!("packed, but {} entries were kept (see above)", kept.len()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- unpack

/// An archive being read (from a file or standard input).
type Archive = ArchiveReader<Box<dyn container::Input>>;

/// An archive's bytes read through a 1 MB buffer. The source is boxed as well, so all of them
/// (a file, standard input) are one type to the buffer and to the archive reader: one copy of
/// their code (the source is called once per megabyte). A file is passed over by seeking
/// where grep's index rules a whole segment out (container::Input).
#[cfg_attr(fewer_small, optimize(size))]
fn archive_in(r: Box<dyn container::Src>) -> Box<dyn container::Input> {
    Box::new(BufReader::with_capacity(1 << 20, r))
}

/// The input that pack's check reads again (a file or a folder's files), through a 1 MB buffer.
#[cfg_attr(fewer_small, optimize(size))]
fn buffered<'a>(r: Box<dyn Read + Send + 'a>) -> Box<dyn Read + Send + 'a> {
    Box::new(BufReader::with_capacity(1 << 20, r))
}

/// An archive opened for reading, with the metadata of the open file (None for stdin).
#[cfg_attr(fewer_small, optimize(size))]
fn open_archive(inp: &OsStr, force: bool) -> Result<(Archive, Option<fs::Metadata>), String> {
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

#[cfg_attr(fewer_small, optimize(size))]
fn unpack_one(o: &Opts, inp: &OsStr) -> Result<(), String> {
    if inp == "-" && io::stdin().is_terminal() && !o.force {
        return Err("nothing to unpack: standard input is a terminal".into());
    }
    let lmd = if inp == "-" { None } else { fs::symlink_metadata(inp).ok() };
    let (ar, fmd) = open_archive(inp, o.force)?;
    let to_stdout = o.stdout || (inp == "-" && o.output.is_none());
    let dest = |folder: bool| -> Result<PathBuf, String> {
        let d = match &o.output {
            Some(p) => p.clone(),
            None => strip_suffix(Path::new(inp)).ok_or(format!("unknown suffix (expected {SUFFIX}); use -o NAME or -c"))?,
        };
        let d = if folder { trim_slashes(&d) } else { d };
        let input: Vec<&fs::Metadata> = lmd.iter().chain(fmd.iter()).collect();
        not_the_input(&d, &input)?;
        Ok(d)
    };
    if ar.is_folder() && !to_stdout {
        let dest = dest(true)?;
        let entries = folder::table_from_meta(&ar.meta, o.threads)?;
        unpack_folder(o, ar, entries, &dest)?;
    } else {
        // a damaged meta record is refused before anything is written
        let keep = if ar.is_folder() {
            folder::table_from_meta(&ar.meta, o.threads)?;
            None
        } else {
            parse_file_meta(&ar.meta)?
        };
        let keep = if to_stdout { None } else { keep };
        let mut sink = if to_stdout {
            Sink::stdout()?
        } else {
            let d = dest(false)?;
            Sink::file(&d, o)?
        };
        container::unpack_stream(ar, &mut sink, o.threads)?;
        let mode = keep.map_or(default_mode(), |k| folder::archive_mode(k.0));
        sink.finish(mode, keep.map(|k| k.1))?;
    }
    // --rm: not when the output only went to standard output (as zstd), and only the
    // archive that was read (not one that changed or was replaced meanwhile)
    if o.rm && !to_stdout && inp != "-" {
        let link = lmd.as_ref().filter(|m| m.file_type().is_symlink());
        match &fmd {
            Some(f) if still_same(Path::new(inp), f, link) => {
                fs::remove_file(inp).map_err(|e| format!("unpacked, but cannot delete the archive: {e}"))?;
            }
            _ => return Err("unpacked, but kept the archive: it changed while unpacking".into()),
        }
    }
    Ok(())
}

#[cfg_attr(fewer_small, optimize(size))]
fn unpack_folder<R: Read + Send>(o: &Opts, ar: ArchiveReader<R>, entries: Vec<folder::Entry>, dest: &Path) -> Result<(), String> {
    // an existing file is replaced (with -f) only once the new folder is complete
    if let Ok(md) = fs::symlink_metadata(dest) {
        if md.is_dir() {
            return Err(format!("{} already exists; remove it first or use -o to choose another name", dest.display()));
        }
        if !o.force {
            return Err(format!("{} already exists (use -f to overwrite)", dest.display()));
        }
    }
    // --rm: every file, folder and the new name forced to disk before the archive is deleted
    let mut ex = folder::Extractor::new(dest, entries, o.rm)?;
    container::for_each_segment(ar, o.threads, &mut |d| ex.write(d).map(|_| true))?;
    ex.finish(o.force)
}

// ---------------------------------------------------------------- list, cat, grep

#[cfg_attr(fewer_small, optimize(size))]
fn cmd_list(o: &Opts) -> i32 {
    if o.paths.is_empty() {
        usage_error("list needs an archive");
    }
    let many = o.paths.len() > 1;
    let mut failed = false;
    let mut out = io::stdout().lock();
    for (n, inp) in o.paths.iter().enumerate() {
        container::set_current(inp, false);
        let r = (|| -> Result<(), String> {
            let (mut ar, _) = open_archive(inp, true)?;
            let mut lines = String::new();
            if many {
                // several archives: a heading for each, as ls does for folders
                if n > 0 {
                    lines.push('\n');
                }
                lines += &format!("{}:\n", show_name(inp.as_bytes()));
            }
            if ar.is_folder() {
                let entries = folder::table_from_meta(&ar.meta, o.threads)?;
                for e in &entries {
                    let mut name = if e.path.is_empty() { ".".to_string() } else { show_name(&e.path) };
                    if e.kind == folder::Kind::Dir {
                        name.push('/');
                    }
                    if e.kind == folder::Kind::Symlink {
                        name = format!("{name} -> {}", show_name(&e.target));
                    }
                    lines += &format!("{} {:>14}  {}  {name}\n", folder::mode_string(e.kind, e.mode), e.size, folder::date_string(e.mtime.0));
                }
                // the archive must end here, as unpacking checks
                while let Some(h) = ar.next_header()? {
                    ar.skip(h)?;
                }
                if ar.next_member()?.is_some() {
                    return Err("unexpected data after the end of the archive".into());
                }
            } else {
                // joined file archives unpack as one: their total
                let (mode, date) = match parse_file_meta(&ar.meta)? {
                    Some((m, t)) => (folder::mode_string(folder::Kind::File, m), folder::date_string(t.0)),
                    None => ("-?????????".to_string(), "????-??-?? ??:??".to_string()),
                };
                let mut total = 0u64;
                loop {
                    while let Some(h) = ar.next_header()? {
                        total += h.0;
                        ar.skip(h)?;
                    }
                    match ar.next_member()? {
                        None => break,
                        Some(next) if !next.is_folder() => ar = next,
                        Some(_) => return Err("unexpected data after the end of the archive".into()),
                    }
                }
                let name = if inp == "-" {
                    "(stdin)".to_string()
                } else {
                    let p = Path::new(inp);
                    show_name(strip_suffix(p).as_deref().unwrap_or(p).as_os_str().as_bytes())
                };
                lines += &format!("{mode} {total:>14}  {date}  {name}\n");
            }
            out.write_all(lines.as_bytes()).map_err(|e| format!("cannot write output: {e}"))
        })();
        if let Err(e) = r {
            let _ = out.flush();
            eprintln!("fewer-pack: {}: {e}", inp.to_string_lossy());
            failed = true;
        }
    }
    failed as i32
}

#[cfg_attr(fewer_small, optimize(size))]
fn cmd_cat(o: &Opts) -> i32 {
    if o.paths.is_empty() || o.paths.len() > 2 {
        usage_error("usage: fewer-pack cat FILE.cmf [PATH]");
    }
    let inp = &o.paths[0];
    container::set_current(inp, false);
    let r = (|| -> Result<(), String> {
        let (ar, _) = open_archive(inp, true)?;
        let mut sink = Sink::stdout()?;
        let Some(want) = o.paths.get(1) else {
            if ar.is_folder() {
                folder::table_from_meta(&ar.meta, o.threads)?;
            } else {
                parse_file_meta(&ar.meta)?;
            }
            container::unpack_stream(ar, &mut sink, o.threads)?;
            return sink.finish(0, None);
        };
        if !ar.is_folder() {
            return Err("not a folder archive (leave out the PATH)".into());
        }
        let entries = folder::table_from_meta(&ar.meta, o.threads)?;
        let mut key = want.as_bytes();
        while let Some(k) = key.strip_prefix(b"./") {
            key = k;
        }
        while let Some(k) = key.strip_suffix(b"/") {
            key = k;
        }
        let mut off = 0u64;
        let mut found = None;
        for e in &entries {
            if e.path == key {
                found = Some(e);
                break;
            }
            if e.kind == folder::Kind::File {
                off += e.size;
            }
        }
        let e = found.ok_or(format!("{} is not in the archive", show_name(want.as_bytes())))?;
        if e.kind != folder::Kind::File {
            return Err(format!("{} is not a file", show_name(want.as_bytes())));
        }
        let (start, end) = (off, off + e.size);
        let mut ar = ar;
        let mut pos = 0u64;
        while pos < end {
            let Some(h) = ar.next_header()? else { return Err("damaged archive (data ends early)".into()) };
            if pos + h.0 <= start {
                ar.skip(h)?;
                pos += h.0;
                continue;
            }
            let p = ar.payload(h)?;
            let d = engine::unpack(&p, o.threads).map_err(|e| format!("damaged archive ({e})"))?;
            if d.len() as u64 != h.0 {
                return Err("damaged archive (segment unpacked to the wrong size)".into());
            }
            let a = start.saturating_sub(pos) as usize;
            let b = (end - pos).min(h.0) as usize;
            sink.write_all(&d[a..b]).map_err(|e| format!("cannot write output: {e}"))?;
            pos += h.0;
        }
        sink.finish(0, None)
    })();
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fewer-pack: {}: {e}", inp.to_string_lossy());
            1
        }
    }
}

#[cfg_attr(fewer_small, optimize(size))]
fn cmd_grep(o: &Opts) -> i32 {
    if o.paths.len() < 2 {
        usage_error("usage: fewer-pack grep [-i] TEXT FILE.cmf...");
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
        container::set_current(inp, false);
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
            // the runs of whole lines planned below, searched on all cores
            let search = |text: &[u8], runs: &[container::Run], out: &mut Vec<u8>| -> usize {
                runs.iter().map(|&(a, b, f)| search::grep_lines(&text[a..b], &nd, &prefixes[f], out)).sum()
            };
            // a job of the search: about this many bytes of lines
            const JOB: usize = 1 << 20;
            let mut fi = 0usize;
            let mut left = files[0].0;
            let mut carry: Vec<u8> = Vec::new();
            let mut plan = |seg: Option<&[u8]>, size: usize, ents: &mut Vec<container::Ent>| -> Result<(), String> {
                let Some(seg) = seg else {
                    // a block no line of which holds the text, after a line end and ending
                    // with one (so nothing is carried over): only the file places move on
                    let mut d = size as u64;
                    while d > 0 {
                        if fi >= files.len() {
                            return Err("damaged archive (more data than its file table lists)".into());
                        }
                        let take = left.min(d);
                        d -= take;
                        left -= take;
                        if left == 0 {
                            fi += 1;
                            left = files.get(fi).map_or(0, |f| f.0);
                        }
                    }
                    return Ok(());
                };
                let mut runs: Vec<container::Run> = Vec::new();
                let mut bytes = 0usize;
                let mut d = 0usize;
                while d < seg.len() {
                    if fi >= files.len() {
                        if !runs.is_empty() {
                            ents.push(container::Ent::Runs(std::mem::take(&mut runs)));
                        }
                        return Err("damaged archive (more data than its file table lists)".into());
                    }
                    let (p0, p1) = (d, d + left.min((seg.len() - d) as u64) as usize);
                    d = p1;
                    left -= (p1 - p0) as u64;
                    let file_ends = left == 0;
                    // finish the line carried over from the block before (only a block's first
                    // part can carry one: the others start a file)
                    let mut b0 = p0;
                    if !carry.is_empty() {
                        let nl = seg[p0..p1].iter().position(|&c| c == b'\n');
                        let cut = nl.map_or(p1, |p| p0 + p + 1);
                        carry.extend_from_slice(&seg[p0..cut]);
                        b0 = cut;
                        if nl.is_some() || (b0 == p1 && file_ends) {
                            if !runs.is_empty() {
                                ents.push(container::Ent::Runs(std::mem::take(&mut runs)));
                                bytes = 0;
                            }
                            let mut b = Vec::new();
                            let n = search::grep_lines(&carry, &nd, &prefixes[fi], &mut b);
                            ents.push(container::Ent::Out(b, n));
                            carry.clear();
                        }
                    }
                    // keep an unfinished last line for the next block; the whole lines go to
                    // the pool in runs of about JOB bytes, cut at line ends
                    let whole = if file_ends { p1 } else { seg[b0..p1].iter().rposition(|&c| c == b'\n').map_or(b0, |p| b0 + p + 1) };
                    let mut a = b0;
                    while a < whole {
                        let room = JOB - bytes;
                        let e = if whole - a <= room { whole } else { seg[a + room..whole].iter().position(|&c| c == b'\n').map_or(whole, |p| a + room + p + 1) };
                        runs.push((a, e, fi));
                        bytes += e - a;
                        a = e;
                        if bytes >= JOB {
                            ents.push(container::Ent::Runs(std::mem::take(&mut runs)));
                            bytes = 0;
                        }
                    }
                    carry.extend_from_slice(&seg[whole..p1]);
                    if file_ends {
                        fi += 1;
                        left = files.get(fi).map_or(0, |f| f.0);
                    }
                }
                if !runs.is_empty() {
                    ents.push(container::Ent::Runs(runs));
                }
                Ok(())
            };
            let mut write = |b: &[u8], n: usize| -> Result<(), String> {
                matched += n;
                out.write_all(b).map_err(|e| format!("cannot write output: {e}"))
            };
            // (no thread is started for blocks when the index rules out all of them)
            let t = if ix.as_ref().is_some_and(|x| x.rules_out_all(&keys)) { 1 } else { o.threads };
            container::search_archive(ar, t, &may, ix.as_ref().map(|_| &skip as container::IndexSkip), &mut plan, &search, &mut write)?;
            if !carry.is_empty() {
                let mut buf = Vec::new();
                matched += search::grep_lines(&carry, &nd, &prefixes[fi.min(files.len() - 1)], &mut buf);
                out.write_all(&buf).map_err(|e| format!("cannot write output: {e}"))?;
            }
            Ok(())
        })();
        // what this archive matched is written out before the next archive is read (a later
        // archive that stops the engine ends the program without flushing)
        let _ = out.flush();
        if let Err(e) = r {
            eprintln!("fewer-pack: {}: {e}", inp.to_string_lossy());
            failed = true;
        }
    }
    if let Err(e) = out.flush() {
        eprintln!("fewer-pack: cannot write output: {e}");
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

const MISMATCH: &str = "made for another archive, or before this one changed";

fn idx_path(inp: &OsStr) -> PathBuf {
    let mut p = inp.to_os_string();
    p.push(".idx");
    PathBuf::from(p)
}

/// fewer-pack index: FILE.cmf.idx for each archive (index.rs), replacing an older one.
#[cfg_attr(fewer_small, optimize(size))]
fn cmd_index(o: &Opts) -> i32 {
    if o.paths.is_empty() {
        usage_error("usage: fewer-pack index FILE.cmf...");
    }
    let mut failed = false;
    for inp in &o.paths {
        container::set_current(inp, false);
        let r = (|| -> Result<(), String> {
            if inp == "-" {
                return Err("an index is made for an archive file".into());
            }
            let (ar, fmd) = open_archive(inp, false)?;
            let d = index::build(ar, fmd.map_or(0, |m| m.len()), o.threads)?;
            let mut f = AtomicFile::create(&idx_path(inp), true, false)?;
            f.write_all(&d).map_err(|e| format!("cannot write the index: {e}"))?;
            f.finish(default_mode(), None)
        })();
        if let Err(e) = r {
            eprintln!("fewer-pack: {}: {e}", inp.to_string_lossy());
            failed = true;
        }
    }
    failed as i32
}

#[cfg(test)]
mod tests {
    use super::fixed;

    /// fixed() writes what format!("{x:.d$}") writes, for the numbers summary() prints.
    #[test]
    fn fixed_same_as_std() {
        let mut xs: Vec<f64> = vec![0.0, -0.0, 0.005, 0.015, 0.025, 0.125, 0.375, 0.5, 1.5, 2.5, 1e-9, 1e-300, 5e-324, 9.995, 99.995, 1e15, 4503599627370495.5, 1e17];
        // ratios, MB/s and seconds as summary() makes them, and halfway cases (k / 2^j)
        let mut r = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..300_000 {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let a = r >> (r % 50);
            let b = (r.rotate_left(17) >> (r % 61)).max(1);
            xs.push(a as f64 / b as f64);
            xs.push(a as f64 / (b as f64 * 1e-9).max(1e-9) / 1e6);
            xs.push((r % 1_000_000_000_000) as f64 / 1e9);
            xs.push((r % (1 << 40)) as f64 / (1u64 << (r % 40)) as f64);
            xs.push(f64::from_bits(r >> 2) % 1e18);
        }
        let mut n = 0;
        for &x in &xs {
            for d in 0..=2u32 {
                // (x * 10^d below 2^64, as for every number summary() prints)
                if x.abs() * 10f64.powi(d as i32) < 1.8e19 {
                    assert_eq!(fixed(x, d), format!("{x:.*}", d as usize), "{x:e} {d}");
                    n += 1;
                }
            }
        }
        assert!(n > 3_000_000, "{n}");
    }
}
