//! `fewer add`: save files, folders and the system's logs into the
//! memory store, each file as one run of the current folder's project.
//!
//!   fewer add [--hide-secrets] [--force] PATH...
//!   fewer add --macos-log [--last 1h] [--predicate P]   this Mac's log
//!   fewer add --simulator [--last 1h] [--predicate P]   the booted iOS simulator's log
//!   fewer add FILE.logarchive                           a macOS log archive
//!   fewer add --journal [--since T]                     the systemd journal (Linux)
//!
//! A text is saved in these steps (the original file is never touched):
//! 1. what is new of it is copied to a snapshot in $FEWER_HOME/tmp/ (counted and hashed on the
//!    way); a whole file's snapshot gets the original's mode and modification time, so a file
//!    saved as it is gives the same archive as `fewer-pack FILE`, byte for byte;
//! 2. a run id is taken and the run recorded as packing, with its log type (formats::detect on
//!    the snapshot's first 64 KiB and the file's name);
//! 3. the fewer-pack binary packs the snapshot: the store's archives are fewer-pack's
//!    own, never made by a copy of the packer;
//! 4. its .idx is written (the same bytes as `fewer-pack index`);
//! 5. its catalog is built from the archive (the index pass, catalog.rs: keys, records, line
//!    times) and written, and merged into the project's key registry;
//! 6. the run is recorded as ready with its text bytes, lines and archive bytes, its log type,
//!    key rule and first and last line time, and the snapshot is removed;
//! 7. once every file is saved, the runs with dna_min bytes of text or more (16 MiB) get their
//!    DNA index from a detached pass (dna_build::after_save).
//!
//! What is saved, and how:
//! - **The same file again** (the same path, in the same project): its text is compared with
//!   the last run of that source, by the size and our 128-bit hash of what that run had read
//!   (`offset + size` bytes from the start; `hash`). Unchanged: nothing is saved ("already
//!   saved"). Grown with the same start: only the new part is saved, as a run that
//!   `continues` the last one, so nothing is counted twice. Otherwise (rewritten, rotated,
//!   shorter): a new full run.
//! - **Folders** are walked without following links. They give files named *.log, *.log.N,
//!   *.out, *.err, *.txt, *.json, *.jsonl, *.ndjson, *.csv, *.tsv, *.ips and *.crash (in any
//!   case), the same names ending in .gz, .bz2, .xz, .lz4 or .zst, and files without a dot
//!   inside a folder named log or logs (syslog; also rotated, syslog.1). .git, node_modules,
//!   target, hidden folders, hidden files and the store's own folder are passed over. The
//!   command says how many files it took, and how many it passed over and why.
//! - **Packed files** are told by their first bytes, whatever their name. gzip, bzip2, xz and
//!   lz4 files are read through the installed tool, a separate program that only reads
//!   (`gzip -dc FILE`, stdin /dev/null): the run keeps their content, the log lines, exactly,
//!   not the packed file. With the tool missing, or when it cannot read the file, the file is
//!   kept as it is (rNNNNNN.keep, a run of kind kept, with the reason). zstd files and zip and
//!   tar bundles (also tar bundles packed whole: .tgz, .tar.gz, ...) are kept as they are,
//!   read in a later version; 7z and rar archives are kept as they are. zstd is never run.
//! - **Not text** (NUL bytes, or under 90% text bytes in the first 64 KiB): not saved, recorded
//!   as a run of kind not-read with the reason, unless --force.
//! - **Secrets** are hidden in files only with --hide-secrets or `hide_secrets_in_add = on`
//!   (files added on purpose keep every byte), and always in OS exports.
//! - **OS exports**: the flag is the opt-in, with a time window (the last hour by default). The
//!   command's output is saved as a run of kind export, with the command in `cmd`; a log
//!   archive added again follows the rule of the same file again.
//!
//! A test switch, never set in normal use: FEWER_TEST_RUN_TAGS, a JSON object, gives every run
//! `add` saves of a file the tags `run` gives a command's run (kind, cmd, cwd, exit, signal,
//! branch, commit, dirty, start, end), in its first record and again in its
//! "ready" record, so the checks can make runs of a command with known tags before `run` is
//! there (checks/fixture_runs.py). Its `start` is also the save time the key registry gets.
//! With `agent`, `session` and `files` ([path, line] pairs, the files a session edited and the
//! line of each edit) it saves a made-up agent's session as a sessions sweep does (kind session): the
//! files go into the run's catalog, as the index pass of a session gives them,
//! not into runs.jsonl (checks/guard_check.py).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::answer::group;
use crate::catalog;
use crate::config;
use crate::formats;
use crate::json::Json;
use crate::secrets::{self, Extra, Hider, Stream};
use crate::store::{self, Project};

/// Our 128-bit hash of a byte stream (whether a file added again still
/// starts with what was saved): two 64-bit multiply-and-xor lanes over 8-byte little-endian
/// words (the last one padded with zeros), then the length mixed in.
#[derive(Clone)]
pub struct Hash128 {
    h1: u64,
    h2: u64,
    tail: [u8; 8],
    tlen: usize,
    total: u64,
}

fn fmix(mut z: u64) -> u64 {
    z = (z ^ z >> 30).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ z >> 27).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ z >> 31
}

impl Hash128 {
    pub fn new() -> Hash128 {
        Hash128 { h1: 0x243f_6a88_85a3_08d3, h2: 0x1319_8a2e_0370_7344, tail: [0; 8], tlen: 0, total: 0 }
    }

    #[inline]
    fn word(&mut self, w: u64) {
        self.h1 = (self.h1 ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.h1 ^= self.h1 >> 32;
        self.h2 = (self.h2 ^ w.rotate_left(29)).wrapping_mul(0xff51_afd7_ed55_8ccd);
        self.h2 ^= self.h2 >> 29;
    }

    pub fn update(&mut self, mut b: &[u8]) {
        self.total += b.len() as u64;
        if self.tlen > 0 {
            let k = (8 - self.tlen).min(b.len());
            self.tail[self.tlen..self.tlen + k].copy_from_slice(&b[..k]);
            self.tlen += k;
            b = &b[k..];
            if self.tlen < 8 {
                return;
            }
            let w = u64::from_le_bytes(self.tail);
            self.word(w);
            self.tlen = 0;
        }
        let mut chunks = b.chunks_exact(8);
        for c in &mut chunks {
            self.word(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let r = chunks.remainder();
        self.tail[..r.len()].copy_from_slice(r);
        self.tlen = r.len();
    }

    /// 32 hex digits.
    pub fn finish(mut self) -> String {
        if self.tlen > 0 {
            let mut t = [0u8; 8];
            t[..self.tlen].copy_from_slice(&self.tail[..self.tlen]);
            self.word(u64::from_le_bytes(t));
        }
        let a = fmix(self.h1 ^ self.total);
        let b = fmix(self.h2 ^ self.total.rotate_left(32) ^ a);
        format!("{a:016x}{b:016x}")
    }
}

/// The fewer-pack program that packs: $FEWER_PACK_BIN, else `fewer-pack` next to fewer, else
/// `fewer-pack` on PATH.
pub fn pack_bin() -> PathBuf {
    if let Some(b) = std::env::var_os("FEWER_PACK_BIN").filter(|b| !b.is_empty()) {
        return PathBuf::from(b);
    }
    if let Ok(me) = std::env::current_exe() {
        let near = me.with_file_name("fewer-pack");
        if near.is_file() {
            return near;
        }
    }
    PathBuf::from("fewer-pack")
}

const USAGE: &str = "usage: fewer add [--hide-secrets] [--force] PATH...
       fewer add --macos-log | --simulator [--last 1h] [--predicate P]
       fewer add FILE.logarchive | --journal [--since T]";

const HELP: &str = "\
fewer add - save logs into the memory store ($FEWER_HOME)

Usage:
  fewer add [--hide-secrets] [--force] PATH...
  fewer add --macos-log [--last 1h] [--predicate P]
  fewer add --simulator [--last 1h] [--predicate P]
  fewer add FILE.logarchive
  fewer add --journal [--since T]

Each file is saved as one run of the current folder's project, every byte kept. A file added
again saves only what is new: nothing when it is unchanged, its new end when it grew, all of
it when it was rewritten.

Folders are walked without following links. They give files named *.log, *.log.N, *.out,
*.err, *.txt, *.json, *.jsonl, *.ndjson, *.csv, *.tsv, *.ips and *.crash (also ending in .gz,
.bz2, .xz, .lz4 or .zst), and files without a dot in a folder named log or logs. .git,
node_modules, target and hidden folders and files are passed over.

gzip, bzip2, xz and lz4 files are read by the installed gzip, bzip2, xz or lz4 tool: the
saved copy keeps their content (the log lines) exactly, not the packed file. Without the tool
the file is kept as it is. zstd files and zip and tar bundles are kept as they are (read in a
later version). A file that is not text is not saved, unless --force is given.

Options:
  --hide-secrets     hide secrets (API keys, tokens, passwords, private keys: the shapes in
                     SECRETS.md) in the files too; `hide_secrets_in_add = on` in the store's
                     config does the same
  --force            save a file that is not text too
  --macos-log        this Mac's log: log show --info --debug --style ndjson --last 1h
  --simulator        the booted iOS simulator's log:
                     xcrun simctl spawn booted log show --style ndjson --last 1h
  FILE.logarchive    a log archive: log show --archive FILE --style ndjson
  --journal          the systemd journal (Linux): journalctl -o json --all --since=-1h
  --last N[s|m|h|d]  the time window of --macos-log and --simulator (default 1h), or of a log
                     archive (default: all of it); also `boot`
  --predicate P      log show's filter, for --macos-log, --simulator and log archives
  --since T          the start of --journal's window, as journalctl reads it (default -1h)

The system's logs are saved with secrets hidden, but they hold personal details (names, places,
networks, the apps you use) that hiding secrets does not catch: save them only when you mean
to, for the time you need. On Windows, export events in PowerShell, for example
  Get-WinEvent -LogName Application -MaxEvents 10000 | ForEach-Object { $_ | ConvertTo-Json -Compress -Depth 3 } > events.jsonl
and add the file.";

fn usage(msg: &str) -> i32 {
    eprintln!("fewer add: {msg}");
    eprintln!("{USAGE}");
    2
}

/// The fields FEWER_TEST_RUN_TAGS may set, each with the JSON type it must have.
const TEST_TAGS: [(&str, &str); 13] = [
    ("kind", "string"),
    ("cmd", "array of strings"),
    ("cwd", "string"),
    ("exit", "integer"),
    ("signal", "integer"),
    ("branch", "string"),
    ("commit", "string"),
    ("dirty", "boolean"),
    ("start", "integer"),
    ("end", "integer"),
    ("agent", "string"),
    ("session", "string"),
    ("files", "array of [path, line] pairs"),
];

/// The test switch FEWER_TEST_RUN_TAGS (the module's doc): the tags, checked, or None when it
/// is not set.
fn test_tags() -> Result<Option<Json>, String> {
    let Some(v) = std::env::var_os("FEWER_TEST_RUN_TAGS").filter(|v| !v.is_empty()) else { return Ok(None) };
    let bad = |why: String| format!("FEWER_TEST_RUN_TAGS (a test switch): {why}");
    let v = crate::json::parse(v.as_encoded_bytes()).map_err(|e| bad(format!("not JSON: {e}")))?;
    let Some(fields) = v.obj() else { return Err(bad("not a JSON object".into())) };
    for (k, x) in fields {
        let Some((_, want)) = TEST_TAGS.iter().find(|t| t.0 == k) else {
            return Err(bad(format!("unknown field '{k}'; the fields are {}", TEST_TAGS.map(|t| t.0).join(", "))));
        };
        let ok = match *want {
            "string" => x.str().is_some(),
            "integer" => x.int().is_some(),
            "boolean" => x.bool().is_some(),
            "array of strings" => x.arr().is_some_and(|a| a.iter().all(|s| s.str().is_some())),
            _ => edited_files(Some(x)).is_some(),
        };
        if !ok {
            return Err(bad(format!("field '{k}' must be a JSON {want}")));
        }
    }
    Ok(Some(v))
}

/// The test switch's edited files: [path, line] pairs, lines counted from 1.
fn edited_files(v: Option<&Json>) -> Option<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for p in v?.arr()? {
        match p.arr()? {
            [path, line] => out.push((path.str()?.to_string(), u64::try_from(line.int()?).ok().filter(|&l| l > 0)?)),
            _ => return None,
        }
    }
    Some(out)
}

/// A record with the test tags set over its fields (the edited files go to the catalog).
fn tagged(mut rec: Json, tags: Option<&Json>) -> Json {
    if let Some(f) = tags.and_then(Json::obj) {
        for (k, v) in f {
            if k != "files" {
                rec.set(k, v.clone());
            }
        }
    }
    rec
}

// ---------------------------------------------------------------- the command line

/// The system's log exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Os {
    /// this Mac's log: `log show --info --debug --style ndjson --last 1h`
    Mac,
    /// the booted iOS simulator's log: `xcrun simctl spawn booted log show --style ndjson`
    Simulator,
    /// the systemd journal (Linux): `journalctl -o json --all`
    Journal,
}

/// What `add` was asked to save, in the order given.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// a file, a folder, or a macOS log archive (a folder named *.logarchive)
    Path(PathBuf),
    /// an export of the system's log
    Os(Os),
}

/// The options of the command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Opts {
    /// hide secrets in files too (--hide-secrets; also the config's hide_secrets_in_add = on)
    hide: bool,
    /// save files that are not text too
    force: bool,
    /// log show's window and filter; journalctl's window
    last: Option<String>,
    predicate: Option<String>,
    since: Option<String>,
}

/// The window of --macos-log and --simulator, and the start of --journal's, when none is given.
const DEFAULT_LAST: &str = "1h";
const DEFAULT_SINCE: &str = "-1h";

/// A log archive: a folder whose name ends in .logarchive.
fn is_log_archive(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.as_bytes().to_ascii_lowercase().ends_with(b".logarchive"))
}

/// log show's --last: N, Ns, Nm, Nh or Nd, or `boot`.
fn last_ok(v: &str) -> bool {
    let d = v.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(v);
    v == "boot" || (!d.is_empty() && d.len() <= 9 && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Read the command line: what to save, in order, and the options. Err: the exit status, for
/// --help (0) and a usage error (2, said on stderr).
fn parse(args: Vec<OsString>) -> Result<(Vec<Item>, Opts), i32> {
    let mut items = Vec::new();
    let mut o = Opts::default();
    let mut only_paths = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let b = arg.as_bytes();
        if only_paths || b == b"-" || !b.starts_with(b"-") {
            items.push(Item::Path(PathBuf::from(arg)));
            continue;
        }
        let s = arg.to_string_lossy().into_owned();
        // --name=value, or --name value for the options that take one
        let (name, inline) = match s.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (s.clone(), None),
        };
        let takes = matches!(name.as_str(), "--last" | "--predicate" | "--since");
        if inline.is_some() && !takes {
            return Err(usage(&format!("{name} takes no value")));
        }
        let value = if takes {
            match inline.or_else(|| it.next().map(|v| v.to_string_lossy().into_owned())) {
                Some(v) => Some(v),
                None => return Err(usage(&format!("{name} needs a value"))),
            }
        } else {
            None
        };
        let mut export = |os: Os| {
            if !items.contains(&Item::Os(os)) {
                items.push(Item::Os(os));
            }
        };
        match name.as_str() {
            "--" => only_paths = true,
            "-h" | "--help" => {
                println!("{HELP}");
                return Err(0);
            }
            "--hide-secrets" => o.hide = true,
            "--force" => o.force = true,
            "--macos-log" => export(Os::Mac),
            "--simulator" => export(Os::Simulator),
            "--journal" => export(Os::Journal),
            "--last" => o.last = value,
            "--predicate" => o.predicate = value,
            "--since" => o.since = value,
            _ => return Err(usage(&format!("unknown option {s}"))),
        }
    }
    if items.is_empty() {
        return Err(usage("give the files or folders to save, or a log to export (--macos-log, --simulator, --journal)"));
    }
    let log_show = items.iter().any(|i| matches!(i, Item::Os(Os::Mac | Os::Simulator)) || matches!(i, Item::Path(p) if is_log_archive(p)));
    if let Some(l) = &o.last {
        if !last_ok(l) {
            return Err(usage(&format!("--last {l}: give a number with s, m, h or d after it (1h, 30m), or boot")));
        }
        if !log_show {
            return Err(usage("--last is for --macos-log, --simulator and log archives"));
        }
    }
    if o.predicate.is_some() && !log_show {
        return Err(usage("--predicate is for --macos-log, --simulator and log archives"));
    }
    if o.since.is_some() && !items.contains(&Item::Os(Os::Journal)) {
        return Err(usage("--since is for --journal"));
    }
    Ok((items, o))
}

/// An export's command, its program first, and how messages name what it reads.
fn export_of(os: Os, o: &Opts) -> (Vec<OsString>, String) {
    let last = o.last.as_deref().unwrap_or(DEFAULT_LAST);
    let mut argv: Vec<String> = match os {
        Os::Mac => ["log", "show", "--info", "--debug", "--style", "ndjson", "--last", last].map(String::from).to_vec(),
        Os::Simulator => ["xcrun", "simctl", "spawn", "booted", "log", "show", "--style", "ndjson", "--last", last].map(String::from).to_vec(),
        Os::Journal => vec!["journalctl".into(), "-o".into(), "json".into(), "--all".into(), format!("--since={}", o.since.as_deref().unwrap_or(DEFAULT_SINCE))],
    };
    if os != Os::Journal
        && let Some(p) = &o.predicate
    {
        argv.push("--predicate".into());
        argv.push(p.clone());
    }
    let what = match os {
        Os::Mac => format!("this Mac's log (the last {last})"),
        Os::Simulator => format!("the booted simulator's log (the last {last})"),
        Os::Journal => format!("the systemd journal (since {})", o.since.as_deref().unwrap_or(DEFAULT_SINCE)),
    };
    (argv.into_iter().map(OsString::from).collect(), what)
}

/// The command that exports a log archive: `log show --archive FILE --style
/// ndjson`, with --last and --predicate when they were given.
fn archive_export(path: &Path, o: &Opts) -> Vec<OsString> {
    let mut argv: Vec<OsString> = ["log", "show", "--archive"].map(OsString::from).to_vec();
    argv.push(path.as_os_str().to_os_string());
    argv.extend(["--style", "ndjson"].map(OsString::from));
    if let Some(l) = &o.last {
        argv.extend(["--last".into(), l.into()]);
    }
    if let Some(p) = &o.predicate {
        argv.extend(["--predicate".into(), p.into()]);
    }
    argv
}

/// Where an export's program comes from, for the message when it is missing.
fn program_home(prog: &str) -> &'static str {
    match prog {
        "log" => " (macOS's log)",
        "xcrun" => " (Xcode's command line tools, on macOS)",
        "journalctl" => " (systemd, on Linux)",
        _ => "",
    }
}

pub fn cmd_add(args: Vec<OsString>) -> i32 {
    let (paths, opts) = match parse(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let tags = match test_tags() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fewer add: {e}");
            return 2;
        }
    };
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fewer add: {e}");
            return 2;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fewer add: cannot read the current folder: {e}");
            return 1;
        }
    };
    let root = store::project_root(&cwd);
    let tmp = home.join("tmp");
    let project = match fs::create_dir_all(&tmp).and_then(|_| store::ensure_project(&home, &root)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fewer add: cannot open the memory store {}: {e}", home.display());
            return 1;
        }
    };
    let mut add = Adder::new(&project, &home, tmp.clone(), cwd, opts, tags.as_ref());
    store::clear_leftovers(&tmp);
    let home_real = fs::canonicalize(&home).unwrap_or(home.clone());
    let mut failed = false;
    // (the runs this command saves: from this id on)
    let first = crate::dna_build::next_id(&project);
    for p in &paths {
        if !add.item(p, &home_real) {
            failed = true;
        }
    }
    // a run with dna_min bytes of text or more gets its DNA index from a detached pass:
    // the save never waits for it
    crate::dna_build::after_save(&project, &crate::dna_build::runs_from(&project, first));
    // the memory's limit, after a save (memory.rs): the oldest runs past it are forgotten
    if let Some(m) = crate::memory::after_save(&home) {
        println!("{m}");
    }
    failed as i32
}

/// Save one file as a run (or keep it as it is, or record it as not read), or tell that it is
/// saved already: what was done. `home_real` is the store's folder, resolved.
fn add_file(a: &mut Adder, path: &Path, home_real: &Path) -> Result<Done, String> {
    if path.as_os_str() == "-" {
        return Err("standard input is saved with `fewer run` (a later version)".into());
    }
    let start = a.start_ms();
    let md = fs::metadata(path).map_err(|e| format!("cannot read: {e}"))?;
    if !md.is_file() {
        return Err("is not a regular file; not saved".into());
    }
    let real = fs::canonicalize(path).map_err(|e| format!("cannot read: {e}"))?;
    if real.starts_with(home_real) {
        return Err("is inside the memory store; not saved".into());
    }
    let abs = std::path::absolute(path).unwrap_or(path.to_path_buf());
    let g = Given { shown: path.display().to_string(), path: path.to_path_buf(), source: abs.to_string_lossy().into_owned(), md };
    let head = read_head(path).map_err(|e| format!("cannot read: {e}"))?;
    let name = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match packed(&name, &head) {
        Packed::Later(what) => a.keep(&g, start, format!("{what}: read in a later version")),
        Packed::Other(what) => a.keep(&g, start, format!("{what}, which fewer does not read")),
        Packed::Tool(tool) => {
            let Some(prog) = find_program(tool) else {
                return a.keep(&g, start, format!("{tool} is not installed (not found on PATH)"));
            };
            // the tool's output, its content, into a file of ours first
            let raw = Temp(a.temp("raw"));
            match run_to_file(&prog, &[OsStr::new("-dc"), abs.as_os_str()], &raw.0) {
                Ok(_) => {}
                Err(RunFail::Program(why)) => return a.keep(&g, start, format!("{tool} could not read it: {why}")),
                Err(RunFail::Store(e)) => return Err(e),
            }
            let inner = read_head(&raw.0).map_err(|e| format!("cannot read what {tool} wrote: {e}"))?;
            if is_tar(&inner) {
                return a.keep(&g, start, format!("a tar bundle packed with {tool}: read in a later version"));
            }
            a.text(Text { shown: &g.shown, file: Some(&g), source: Some(g.source.clone()), path: &raw.0, temp: true, reader: Some(tool), cmd: None, head: inner, start })
        }
        Packed::No => a.text(Text { shown: &g.shown, file: Some(&g), source: Some(g.source.clone()), path: &g.path, temp: false, reader: None, cmd: None, head, start }),
    }
}

// ---------------------------------------------------------------- one add command

/// A file named on the command line or found in a folder.
struct Given {
    /// how messages name it: the path as given or found
    shown: String,
    path: PathBuf,
    /// the run's source: its absolute path
    source: String,
    md: Metadata,
}

/// A text to save as a run (its new part, when it was saved before).
struct Text<'t> {
    /// how messages name it
    shown: &'t str,
    /// the file it comes from (None: an export)
    file: Option<&'t Given>,
    /// the run's source: the file's absolute path, or the log archive an export reads; None
    /// for an export of the system's log now
    source: Option<String>,
    /// where the text is now: the file itself, or a file of ours a tool wrote (`temp`)
    path: &'t Path,
    temp: bool,
    /// the tool that read it: gzip, bzip2, xz or lz4, or an export's program
    reader: Option<&'t str>,
    /// an export's command: a run of kind export, secrets always hidden
    cmd: Option<Vec<String>>,
    /// its first 64 KiB
    head: Vec<u8>,
    /// when saving it began, Unix ms (the run's start)
    start: i64,
}

/// The last saved run of a source: what the same file added again is compared
/// with.
#[derive(Clone, Debug)]
struct Prev {
    id: u64,
    /// where the bytes it read end in the source (offset + size), and our hash of the source
    /// from its start to there (kept and not-read runs: the file's own bytes)
    end: u64,
    hash: String,
    /// the run it continues
    cont: Option<u64>,
    /// why it was kept or not read
    note: String,
}

/// What a run of this kind is compared with when its source comes again: the last run of the
/// same class. A text run (add, export) with the last text run, so a file kept for a while
/// (its tool missing) and read again later is still compared with the text saved before; a
/// kept file with the last kept copy; a file not read with the last one not read.
fn class_of(kind: &str) -> Option<&'static str> {
    match kind {
        "add" | "export" => Some("text"),
        "kept" => Some("kept"),
        "not-read" => Some("not-read"),
        _ => None,
    }
}

/// The last ready run of each class of each source of a project's files and log archives
/// (runs.jsonl is in id order: a later run replaces an earlier one).
fn last_runs(dir: &Path) -> HashMap<(String, &'static str), Prev> {
    let mut m = HashMap::new();
    for r in store::read_runs(dir) {
        let Some(class) = class_of(r.kind()).filter(|_| r.state() == "ready") else { continue };
        let (Some(src), Some(o), Some(s), Some(h)) = (r.s("source"), r.i("offset"), r.i("size"), r.s("hash")) else { continue };
        let (Ok(o), Ok(s)) = (u64::try_from(o), u64::try_from(s)) else { continue };
        let prev = Prev { id: r.id(), end: o + s, hash: h.to_string(), cont: r.i("continues").map(|c| c as u64), note: r.s("note").unwrap_or("").to_string() };
        m.insert((src.to_string(), class), prev);
    }
    m
}

/// What saving one file or export did, with the line that says so.
enum Done {
    Saved(String),
    /// nothing new: saved, kept or looked at before
    Same(String),
    Kept(String),
    NotRead(String),
    /// an export that gave no lines: nothing saved
    Empty(String),
}

impl Done {
    fn text(&self) -> &str {
        match self {
            Done::Saved(s) | Done::Same(s) | Done::Kept(s) | Done::NotRead(s) | Done::Empty(s) => s,
        }
    }
}

/// Print what saving one file or export did: whether it went well (a file kept as it is, or
/// not read, was dealt with; an error was not).
fn report(what: &str, r: &Result<Done, String>) -> bool {
    match r {
        Ok(d) => {
            println!("{}", d.text());
            true
        }
        Err(e) => {
            eprintln!("fewer add: {what}: {e}");
            false
        }
    }
}

/// A file of ours in $FEWER_HOME/tmp, removed when dropped (a leftover of a killed `add` is
/// cleared by the next one: its name ends with .tmp-<process id>).
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// One `add` command: the project its runs go to, its options, and what was saved before.
struct Adder<'a> {
    project: &'a Project,
    tmp: PathBuf,
    /// the folder `add` runs in (an export's `cwd`)
    cwd: PathBuf,
    opts: Opts,
    /// the store's config, for the extra secret shapes (read when secrets are hidden)
    settings: Vec<(String, String)>,
    extras: Option<Vec<Extra>>,
    /// the test switch's tags
    test: Option<&'a Json>,
    /// the last saved run of each class of each source (class_of), kept current as this
    /// command saves
    last: HashMap<(String, &'static str), Prev>,
    /// numbers this command's files in tmp/ apart
    seq: u64,
}

impl<'a> Adder<'a> {
    fn new(project: &'a Project, home: &Path, tmp: PathBuf, cwd: PathBuf, mut opts: Opts, test: Option<&'a Json>) -> Adder<'a> {
        let settings = config::read(home);
        match config::get(&settings, "hide_secrets_in_add").map(str::to_ascii_lowercase).as_deref() {
            Some("on") => opts.hide = true,
            None | Some("off") => {}
            Some(v) => eprintln!("fewer add: config line \"hide_secrets_in_add = {v}\": neither on nor off; secrets are hidden in files only with --hide-secrets"),
        }
        let last = last_runs(&project.dir);
        Adder { project, tmp, cwd, opts, settings, extras: None, test, last, seq: 0 }
    }

    /// When saving begins: the test switch's start, else now.
    fn start_ms(&self) -> i64 {
        self.test.and_then(|t| t.get("start")).and_then(Json::int).unwrap_or_else(crate::times::now_ms)
    }

    /// A new name in tmp/ for a file of ours.
    fn temp(&mut self, what: &str) -> PathBuf {
        self.seq += 1;
        self.tmp.join(format!("{}-add-{}.{what}.tmp-{}", self.project.pid, self.seq, std::process::id()))
    }

    /// The config's extra secret shapes (`secret = PREFIX CLASS MIN`); a line that cannot be
    /// read is told on stderr, once, and not used.
    fn extras(&mut self) -> Vec<Extra> {
        if self.extras.is_none() {
            let mut v = Vec::new();
            for (k, val) in self.settings.iter().filter(|(k, _)| k == "secret") {
                match Extra::parse(val) {
                    Ok(x) => v.push(x),
                    Err(e) => eprintln!("fewer add: config line \"{k} = {val}\": {e}; this shape is not used"),
                }
            }
            self.extras = Some(v);
        }
        self.extras.clone().unwrap_or_default()
    }

    /// Save one item of the command line, saying what was done: whether all went well.
    fn item(&mut self, it: &Item, home_real: &Path) -> bool {
        match it {
            Item::Os(os) => {
                let (argv, what) = export_of(*os, &self.opts);
                let r = self.export(&what, &argv, None);
                report(&what, &r)
            }
            Item::Path(p) => match fs::metadata(p) {
                Ok(m) if m.is_dir() && is_log_archive(p) => {
                    let argv = archive_export(p, &self.opts);
                    let abs = std::path::absolute(p).unwrap_or(p.clone());
                    let r = self.export(&format!("the log archive {}", p.display()), &argv, Some(&abs));
                    report(&p.display().to_string(), &r)
                }
                Ok(m) if m.is_dir() => self.folder(p, home_real),
                _ => {
                    let r = add_file(self, p, home_real);
                    report(&p.display().to_string(), &r)
                }
            },
        }
    }

    /// Save the files a folder gives, each as one run, then say how many files
    /// it took, and how many it passed over and why: whether all went well.
    fn folder(&mut self, dir: &Path, home_real: &Path) -> bool {
        if fs::canonicalize(dir).is_ok_and(|r| r.starts_with(home_real)) {
            eprintln!("fewer add: {}: is inside the memory store; not saved", dir.display());
            return false;
        }
        let w = walk(dir, home_real);
        // saved, already saved, kept as they are, not read, failed
        let mut n = [0u64; 5];
        for f in &w.files {
            let r = add_file(self, f, home_real);
            let k = match &r {
                Ok(Done::Saved(_)) | Ok(Done::Empty(_)) => 0,
                Ok(Done::Same(_)) => 1,
                Ok(Done::Kept(_)) => 2,
                Ok(Done::NotRead(_)) => 3,
                Err(_) => 4,
            };
            n[k] += 1;
            report(&f.display().to_string(), &r);
        }
        for e in &w.errors {
            eprintln!("fewer add: cannot read the folder {e}");
        }
        println!("{}", w.summary(dir, &n));
        n[4] == 0 && w.errors.is_empty()
    }

    /// Save an OS export: its command's output, secrets hidden, as a run of kind
    /// export. `archive`: the log archive it reads, its source.
    fn export(&mut self, what: &str, argv: &[OsString], archive: Option<&Path>) -> Result<Done, String> {
        let prog = argv[0].to_string_lossy().into_owned();
        let Some(path) = find_program(&prog) else {
            return Err(format!("{prog} is not installed (not found on PATH){}; nothing saved", program_home(&prog)));
        };
        let start = crate::times::now_ms();
        let raw = Temp(self.temp("export"));
        let args: Vec<&OsStr> = argv[1..].iter().map(OsString::as_os_str).collect();
        let cmd: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        match run_to_file(&path, &args, &raw.0) {
            Ok(_) => {}
            Err(RunFail::Program(why)) => return Err(format!("{} failed: {why}; nothing saved", cmd.join(" "))),
            Err(RunFail::Store(e)) => return Err(format!("{e}; nothing saved")),
        }
        let head = read_head(&raw.0).map_err(|e| format!("cannot read what {prog} wrote: {e}"))?;
        let source = archive.map(|a| a.to_string_lossy().into_owned());
        self.text(Text { shown: what, file: None, source, path: &raw.0, temp: true, reader: Some(&prog), cmd: Some(cmd), head, start })
    }

    /// Keep a file as it is: its bytes copied to rNNNNNN.keep, a run of kind
    /// kept with the reason. The same bytes kept before: nothing new.
    fn keep(&mut self, g: &Given, start: i64, why: String) -> Result<Done, String> {
        let pid = self.project.pid.clone();
        if let Some(p) = self.last.get(&(g.source.clone(), "kept")).cloned()
            && hash_file(&g.path).map_err(|e| format!("cannot read: {e}"))? == (p.end, p.hash.clone())
        {
            return Ok(Done::Same(format!("{}: already kept as {pid}/r{} ({})", g.shown, p.id, p.note)));
        }
        let first = Json::o(vec![
            ("kind", "kept".into()),
            ("state", "packing".into()),
            ("start", Json::from(start)),
            ("source", g.source.as_str().into()),
            ("offset", Json::from(0u64)),
        ]);
        let id = store::new_run(self.project, first).map_err(|e| format!("cannot write the run table: {e}"))?;
        let keep = self.project.dir.join(keep_file(id));
        match copy_keep(&g.path, &keep, g.md.modified().ok()) {
            Ok((size, hash)) => {
                let f = Json::o(vec![
                    ("state", "ready".into()),
                    ("end", Json::from(crate::times::now_ms())),
                    ("size", Json::from(size)),
                    ("hash", hash.as_str().into()),
                    ("note", why.as_str().into()),
                ]);
                if let Err(e) = store::update_run(self.project, id, f) {
                    let _ = fs::remove_file(&keep);
                    return Err(format!("cannot write the run table: {e}"));
                }
                self.last.insert((g.source.clone(), "kept"), Prev { id, end: size, hash, cont: None, note: why.clone() });
                let hidden = if self.opts.hide { "; secrets in it are not hidden (it is not read)" } else { "" };
                Ok(Done::Kept(format!("kept {} as {pid}/r{id}: {} bytes as they are, not read: {why}{hidden}", g.shown, group(size))))
            }
            Err(e) => {
                let why = format!("cannot keep a copy in the store: {e}");
                let _ = store::update_run(self.project, id, Json::o(vec![("state", "failed".into()), ("end", Json::from(crate::times::now_ms())), ("note", why.as_str().into())]));
                Err(why)
            }
        }
    }

    /// Record a file that is not read (not text): a run of kind not-read with the
    /// reason; nothing of it is saved. The same bytes recorded before: nothing new.
    fn not_read(&mut self, g: &Given, start: i64, why: String) -> Result<Done, String> {
        let pid = self.project.pid.clone();
        let (size, hash) = hash_file(&g.path).map_err(|e| format!("cannot read: {e}"))?;
        if let Some(p) = self.last.get(&(g.source.clone(), "not-read")).filter(|p| p.end == size && p.hash == hash) {
            return Ok(Done::Same(format!("{}: already looked at as {pid}/r{}, not read: {}", g.shown, p.id, p.note)));
        }
        let rec = Json::o(vec![
            ("kind", "not-read".into()),
            ("state", "ready".into()),
            ("start", Json::from(start)),
            ("end", Json::from(crate::times::now_ms())),
            ("source", g.source.as_str().into()),
            ("offset", Json::from(0u64)),
            ("size", Json::from(size)),
            ("hash", hash.as_str().into()),
            ("note", why.as_str().into()),
        ]);
        let id = store::new_run(self.project, rec).map_err(|e| format!("cannot write the run table: {e}"))?;
        self.last.insert((g.source.clone(), "not-read"), Prev { id, end: size, hash, cont: None, note: why.clone() });
        Ok(Done::NotRead(format!("not read: {} (recorded as {pid}/r{id}): {why}; --force saves it all the same", g.shown)))
    }

    /// Save a text as a run, or its new part when its source was saved before:
    /// the snapshot, then packing, the .idx and the catalog.
    fn text(&mut self, t: Text) -> Result<Done, String> {
        let pid = self.project.pid.clone();
        let export = t.cmd.is_some();
        let hide = export || self.opts.hide;
        // 1. not text: not read, unless --force (an export is text by its command)
        if !export
            && !self.opts.force
            && let Some(why) = not_text(&t.head)
            && let Some(g) = t.file
        {
            let why = match t.reader {
                Some(r) => format!("not text, as {r} reads it: {why}"),
                None => format!("not text: {why}"),
            };
            return self.not_read(g, t.start, why);
        }
        let rd = |e: io::Error| format!("cannot read: {e}");
        let mut f = File::open(t.path).map_err(rd)?;
        // 2. the same source again: its new part starts where the last run's text ended, when
        //    the text still starts with what that run read; else it is saved whole again
        let prev = t.source.as_ref().and_then(|s| self.last.get(&(s.clone(), "text"))).cloned();
        let mut h = Hash128::new();
        let (mut from, mut cont, mut again) = (0u64, None, None);
        if let Some(p) = &prev {
            let got = crate::sessions::feed(&mut f, p.end, &mut |b| h.update(b)).map_err(rd)?;
            if got == p.end && h.clone().finish() == p.hash {
                from = p.end;
                cont = Some(p.id);
            } else {
                h = Hash128::new();
                again = Some(p.id);
                f.seek(SeekFrom::Start(0)).map_err(rd)?;
            }
        }
        // 3. the snapshot: the text from `from` on, secrets hidden when they are; a tool's
        //    output saved whole without hiding is the snapshot as it is
        let mut own = None;
        let c = if from == 0 && !hide && t.temp {
            copy_text(&mut f, &mut h, None, &mut io::sink()).map_err(rd)?
        } else {
            let snap = Temp(self.temp("snap"));
            let wr = |e: io::Error| format!("cannot read it or write its snapshot in the store: {e}");
            let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&snap.0).map_err(wr)?;
            let mut out = BufWriter::with_capacity(1 << 20, file);
            let mut stream = if hide { Some(Stream::new(Hider::new(self.extras()))) } else { None };
            // (the hider reads the lines before the new part, as a pass over the whole text would)
            let primed = match stream.as_mut() {
                Some(s) if from > 0 => prime(&mut f, from, s).map_err(rd)?,
                _ => 0,
            };
            f.seek(SeekFrom::Start(from)).map_err(rd)?;
            let mut c = copy_text(&mut f, &mut h, stream.as_mut(), &mut out).map_err(wr)?;
            out.flush().map_err(wr)?;
            c.hidden = stream.map(|s| s.counts().total() - primed);
            own = Some(snap);
            c
        };
        let snap: &Path = own.as_ref().map_or(t.path, |s| &s.0);
        // 4. nothing new
        if c.size == 0 && let Some(p) = prev.as_ref().filter(|_| cont.is_some()) {
            return Ok(Done::Same(match p.cont {
                Some(_) => format!("{}: already saved, unchanged (its last part is {pid}/r{})", t.shown, p.id),
                None => format!("{}: already saved as {pid}/r{}, unchanged", t.shown, p.id),
            }));
        }
        if c.size == 0 && export {
            return Ok(Done::Empty(format!("{}: no lines in that window; nothing saved", t.shown)));
        }
        // 5. the snapshot's time and mode: the original's (an export's: now, private)
        let (mtime, mode) = match t.file {
            Some(g) => (g.md.modified().unwrap_or_else(|_| SystemTime::now()), g.md.permissions()),
            None => (SystemTime::now(), Permissions::from_mode(0o600)),
        };
        let set = |e: io::Error| format!("cannot set its snapshot's time and mode: {e}");
        OpenOptions::new().write(true).open(snap).and_then(|s| s.set_modified(mtime)).map_err(set)?;
        fs::set_permissions(snap, mode).map_err(set)?;
        // 6. the run, with its log type (the name it goes by, and the snapshot's first 64 KiB)
        let name = match (&t.source, &t.cmd) {
            (Some(s), _) => inner_name(s, t.reader).to_string(),
            (None, Some(c)) => c.join(" "),
            (None, None) => String::new(),
        };
        let format = formats::detect(&name, &read_head(snap).map_err(rd)?).format;
        let kind = if export { "export" } else { "add" };
        let mut first = Json::o(vec![("kind", kind.into()), ("state", "packing".into()), ("start", Json::from(t.start))]);
        if let Some(c) = &t.cmd {
            first.set("cmd", Json::Arr(c.iter().map(|a| Json::from(a.as_str())).collect()));
            first.set("cwd", self.cwd.to_string_lossy().into_owned().into());
        }
        if let Some(s) = &t.source {
            first.set("source", s.as_str().into());
            first.set("offset", Json::from(from));
        }
        if let Some(c) = cont {
            first.set("continues", Json::from(c));
        }
        if let Some(r) = t.reader {
            first.set("reader", r.into());
        }
        first.set("format", format.name().into());
        let first = if export { first } else { tagged(first, self.test) };
        let id = store::new_run(self.project, first).map_err(|e| format!("cannot write the run table: {e}"))?;
        let archive = self.project.dir.join(store::run_file(id));
        let abytes = match pack(snap, &archive) {
            Ok(a) => a,
            Err(e) => {
                let _ = fs::remove_file(&archive);
                let _ = fs::remove_file(crate::grep::idx_path(archive.as_os_str()));
                let _ = store::update_run(self.project, id, Json::o(vec![("state", "failed".into()), ("end", Json::from(crate::times::now_ms())), ("note", e.as_str().into())]));
                return Err(e);
            }
        };
        let hash = h.finish();
        let mut fields = vec![("state", "ready".into()), ("end", Json::from(crate::times::now_ms()))];
        if t.source.is_some() {
            fields.push(("size", Json::from(c.size)));
            fields.push(("hash", hash.as_str().into()));
        }
        fields.push(("bytes", Json::from(c.bytes)));
        fields.push(("lines", Json::from(c.lines)));
        fields.push(("archive", Json::from(abytes)));
        if let Some(n) = c.hidden {
            fields.push(("hidden", Json::from(n)));
        }
        // the index pass: the run's catalog and the key registry. A run whose catalog could not
        // be made is still saved: the tools build its catalog when they need it
        let files = if export { Vec::new() } else { edited_files(self.test.and_then(|t| t.get("files"))).unwrap_or_default() };
        let tags = catalog::Tags { name, files, ..catalog::Tags::default() };
        let mut note = format!(", {}", format.name());
        match catalog::index_run(self.project, id, t.start, &tags) {
            Ok(ix) => {
                fields.extend(ix.fields());
                let span = match (ix.first, ix.last) {
                    (Some(a), Some(b)) => format!(", {} to {}", crate::times::fmt_local(a), crate::times::fmt_local(b)),
                    _ => String::new(),
                };
                note = format!(", {}, {} of {}{span}", ix.format, plural(ix.records, "record"), plural(ix.keys as u64, "key"));
            }
            Err(e) => eprintln!("fewer add: {}: its catalog was not made ({e}); the tools make it when they need it", t.shown),
        }
        let rec = Json::o(fields);
        let rec = if export { rec } else { tagged(rec, self.test) };
        store::update_run(self.project, id, rec).map_err(|e| format!("cannot write the run table: {e}"))?;
        if let Some(s) = t.source {
            self.last.insert((s, "text"), Prev { id, end: from + c.size, hash, cont, note: String::new() });
        }
        let mut m = format!("saved {} as {pid}/r{id}: {}, {} -> {} bytes{note}", t.shown, plural(c.lines, "line"), group(c.bytes), group(abytes));
        if let Some(r) = t.reader.filter(|_| !export) {
            m.push_str(&format!("; read by {r}"));
        }
        if let Some(n) = c.hidden {
            m.push_str(&format!("; {} hidden", plural(n, "secret")));
        }
        match (cont, again) {
            (Some(c), _) => m.push_str(&format!("; its new part, after r{c}")),
            (None, Some(a)) => m.push_str(&format!("; saved whole again: it no longer starts with what r{a} read")),
            _ => {}
        }
        Ok(Done::Saved(m))
    }
}

fn plural(n: u64, what: &str) -> String {
    format!("{} {what}{}", group(n), if n == 1 { "" } else { "s" })
}

/// A kept file's name: r000042.keep.
pub fn keep_file(id: u64) -> String {
    format!("r{id:06}.keep")
}

/// Pack a snapshot with the fewer-pack binary on every core, then write the
/// archive's .idx (the same bytes as `fewer-pack index`): the archive's size.
fn pack(snap: &Path, archive: &Path) -> Result<u64, String> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let out = Command::new(pack_bin())
        .arg("-q")
        .arg("-t")
        .arg(threads.to_string())
        .arg("-o")
        .arg(archive)
        .arg(snap)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run fewer-pack ({}): {e} (set FEWER_PACK_BIN to the fewer-pack program)", pack_bin().display()))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // (a fewer-pack that says nothing: its exit status is the reason)
        return Err(format!("fewer-pack could not pack it: {}", if why.is_empty() { out.status.to_string() } else { why }));
    }
    let idx = crate::grep::build_index(archive, threads)?;
    store::write_atomic(&crate::grep::idx_path(archive.as_os_str()), &idx).map_err(|e| format!("cannot write the index: {e}"))?;
    fs::metadata(archive).map(|m| m.len()).map_err(|e| format!("cannot read its archive: {e}"))
}

// ---------------------------------------------------------------- reading and copying

/// The bytes looked at to tell packed files and text: the first 64 KiB.
const HEAD: usize = formats::SAMPLE_BYTES;

/// A file's first 64 KiB (all of a shorter one).
fn read_head(path: &Path) -> io::Result<Vec<u8>> {
    let mut b = Vec::new();
    File::open(path)?.take(HEAD as u64).read_to_end(&mut b)?;
    Ok(b)
}

/// What a text's copy holds.
#[derive(Debug, Default)]
struct Copied {
    /// the source's bytes taken
    size: u64,
    /// the copy's bytes and lines (as grep counts them: '\n' bytes, and one more for a last
    /// line without one)
    bytes: u64,
    lines: u64,
    /// the secrets hidden in it, when they were hidden
    hidden: Option<u64>,
}

/// A writer that counts what it writes.
struct Counted<'w> {
    w: &'w mut dyn Write,
    bytes: u64,
    nl: u64,
    last: u8,
}

impl Counted<'_> {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        if let Some(&l) = b.last() {
            self.last = l;
        }
        self.nl += b.iter().filter(|&&x| x == b'\n').count() as u64;
        self.bytes += b.len() as u64;
        self.w.write_all(b)
    }
}

/// Copy `f` from where it is to its end into `out`, hashing the source's bytes into `h`; with
/// a secrets stream, secrets hidden (it keeps a part line until its '\n').
fn copy_text(f: &mut File, h: &mut Hash128, mut stream: Option<&mut Stream>, out: &mut dyn Write) -> io::Result<Copied> {
    let mut o = Counted { w: out, bytes: 0, nl: 0, last: b'\n' };
    let mut buf = vec![0u8; 1 << 20];
    let mut hidden = Vec::new();
    let mut size = 0u64;
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let d = &buf[..n];
        h.update(d);
        size += n as u64;
        match stream.as_deref_mut() {
            None => o.put(d)?,
            Some(s) => {
                hidden.clear();
                s.push(d, &mut hidden);
                o.put(&hidden)?;
            }
        }
    }
    if let Some(s) = stream {
        hidden.clear();
        s.finish(&mut hidden);
        o.put(&hidden)?;
    }
    Ok(Copied { size, bytes: o.bytes, lines: o.nl + (o.bytes > 0 && o.last != b'\n') as u64, hidden: None })
}

/// The secrets stream reads the lines before `from` (up to secrets::MAX_KEY_LINES of them, as
/// far as a private key's lines are carried), as a pass over the whole text would have: the
/// values it hid there (not counted for the new part).
fn prime(f: &mut File, from: u64, s: &mut Stream) -> io::Result<u64> {
    let start = crate::sessions::lines_back(f, from, secrets::MAX_KEY_LINES)?;
    f.seek(SeekFrom::Start(start))?;
    let mut scratch = Vec::new();
    // (whole lines only: a part line before `from`, when the last run ended inside a line,
    // is not given to it)
    crate::sessions::each_line(f, from - start, &mut |l| {
        scratch.clear();
        s.push(l, &mut scratch);
        Ok(())
    })?;
    Ok(s.counts().total())
}

/// A file's size and our hash of its bytes.
fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let mut h = Hash128::new();
    let c = copy_text(&mut File::open(path)?, &mut h, None, &mut io::sink())?;
    Ok((c.size, h.finish()))
}

/// Copy a file to `dst` (written as <dst>.tmp-<process id>, then renamed),
/// private (mode 0600: it may hold secrets, which are not hidden in a file kept as it is), with
/// the original's modification time: its size and our hash of its bytes.
fn copy_keep(src: &Path, dst: &Path, mtime: Option<SystemTime>) -> io::Result<(u64, String)> {
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let r = (|| {
        let mut out = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        let mut h = Hash128::new();
        let c = {
            let mut w = BufWriter::with_capacity(1 << 20, &mut out);
            let c = copy_text(&mut File::open(src)?, &mut h, None, &mut w)?;
            w.flush()?;
            c
        };
        if let Some(t) = mtime {
            out.set_modified(t)?;
        }
        out.sync_all()?;
        fs::rename(&tmp, dst)?;
        Ok((c.size, h.finish()))
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

// ---------------------------------------------------------------- packed files and text

/// The installed tools that read packed files, each a separate program that
/// only reads (`<tool> -dc FILE`, its output read through a pipe), with the ending of the
/// files it reads.
pub const TOOLS: [(&str, &str); 4] = [("gzip", ".gz"), ("bzip2", ".bz2"), ("xz", ".xz"), ("lz4", ".lz4")];

/// The name a packed file's text goes by: its path without the ending of the tool that read it
/// (app.ips.gz read by gzip: app.ips), so its log type is named as the unpacked file's would be
/// (formats::detect reads .ips, .crash, .csv and .tsv names). catalog::source_name gives a
/// run's catalogs the same name.
pub fn inner_name<'s>(source: &'s str, reader: Option<&str>) -> &'s str {
    if let Some(r) = reader
        && let Some((_, end)) = TOOLS.iter().find(|t| t.0 == r)
        && source.len() > end.len()
        && source.is_char_boundary(source.len() - end.len())
        && source[source.len() - end.len()..].eq_ignore_ascii_case(end)
    {
        return &source[..source.len() - end.len()];
    }
    source
}

/// What a file's first bytes say it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packed {
    /// not packed: read as it is
    No,
    /// read through this installed tool
    Tool(&'static str),
    /// kept as it is, read in a later version: what it is
    Later(&'static str),
    /// kept as it is: another program's archive, which fewer does not read
    Other(&'static str),
}

/// Names of tar bundles packed whole (their bytes start as the packer's).
const TAR_PACKED: [&str; 11] = [".tgz", ".tbz", ".tbz2", ".txz", ".tlz4", ".tzst", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.lz4", ".tar.zst"];

/// The tool whose files start with these bytes: gzip (1f 8b 08), bzip2 ("BZh", the block
/// size, then a block's or the end's magic), xz (fd "7zXZ" 00), lz4 (a frame, or the legacy
/// format).
fn tool_of(h: &[u8]) -> Option<&'static str> {
    if h.starts_with(&[0x1f, 0x8b, 0x08]) {
        Some("gzip")
    } else if h.len() >= 10 && h.starts_with(b"BZh") && (b'1'..=b'9').contains(&h[3]) && (h[4..10] == [0x31, 0x41, 0x59, 0x26, 0x53, 0x59] || h[4..10] == [0x17, 0x72, 0x45, 0x38, 0x50, 0x90]) {
        Some("bzip2")
    } else if h.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Some("xz")
    } else if h.starts_with(&[0x04, 0x22, 0x4d, 0x18]) || h.starts_with(&[0x02, 0x21, 0x4c, 0x18]) {
        Some("lz4")
    } else {
        None
    }
}

/// A tar bundle: "ustar" at byte 257 (POSIX and GNU tar).
fn is_tar(h: &[u8]) -> bool {
    h.len() >= 262 && &h[257..262] == b"ustar"
}

/// What a file is, from its first bytes and, for tar bundles packed whole, its name.
fn packed(name: &str, h: &[u8]) -> Packed {
    let zstd = h.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);
    let tool = tool_of(h);
    let lower = name.to_ascii_lowercase();
    if (tool.is_some() || zstd) && TAR_PACKED.iter().any(|e| lower.ends_with(e)) {
        Packed::Later("a tar bundle")
    } else if zstd {
        Packed::Later("a zstd file")
    } else if h.starts_with(b"PK\x03\x04") || h.starts_with(b"PK\x05\x06") || h.starts_with(b"PK\x07\x08") {
        Packed::Later("a zip bundle")
    } else if is_tar(h) {
        Packed::Later("a tar bundle")
    } else if h.starts_with(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]) {
        Packed::Other("a 7z archive")
    } else if h.starts_with(b"Rar!\x1a\x07") {
        Packed::Other("a rar archive")
    } else if let Some(t) = tool {
        Packed::Tool(t)
    } else {
        Packed::No
    }
}

/// A text byte: printable ASCII, tab, the line ends, form feed, and backspace, bell and escape
/// (terminal output), and every byte from 0x80 (UTF-8 and other 8-bit text).
fn text_byte(b: u8) -> bool {
    (0x20..0x7f).contains(&b) || b >= 0x80 || matches!(b, b'\t' | b'\n' | b'\r' | 0x0c | 0x08 | 0x07 | 0x1b)
}

/// Why a text's first 64 KiB say it is not text: NUL bytes, or under 90% text
/// bytes. None: it is text.
fn not_text(head: &[u8]) -> Option<String> {
    let head = &head[..head.len().min(HEAD)];
    if let Some(p) = head.iter().position(|&b| b == 0) {
        return Some(format!("NUL bytes in its first 64 KiB (the first at byte {})", group(p as u64)));
    }
    let text = head.iter().filter(|&&b| text_byte(b)).count();
    if text * 10 < head.len() * 9 {
        return Some(format!("{}% text bytes in its first 64 KiB, under 90%", text * 100 / head.len()));
    }
    None
}

/// An installed program, looked for in PATH's absolute folders (one in the current folder is
/// never run): a file with an execute bit.
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Why a program that reads gave no output to save.
#[derive(Debug, PartialEq, Eq)]
enum RunFail {
    /// the program could not be run, or it failed: in its own words (the first line it wrote on
    /// stderr), else its exit status
    Program(String),
    /// the store could not take its output (a full disk): an error of ours, not the file's
    Store(String),
}

/// Run a program that only reads (a packed file's tool, an export's command) with stdin from
/// /dev/null and its output written to `out` (a new file, mode 0600): the bytes it wrote.
fn run_to_file(prog: &Path, args: &[&OsStr], out: &Path) -> Result<u64, RunFail> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunFail::Program(format!("cannot run {}: {e}", prog.display())))?;
    let mut err = child.stderr.take().expect("stderr is piped");
    let errs = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut err).take(64 << 10).read_to_end(&mut b);
        // (the rest is read and dropped: the program never waits on a full pipe)
        let _ = io::copy(&mut err, &mut io::sink());
        b
    });
    let mut so = child.stdout.take().expect("stdout is piped");
    let wrote = (|| -> io::Result<u64> {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(out)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut n = 0u64;
        loop {
            let k = match so.read(&mut buf) {
                Ok(0) => return Ok(n),
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            f.write_all(&buf[..k])?;
            n += k as u64;
        }
    })();
    // (closed first: a program still writing gets a broken pipe and ends)
    drop(so);
    let status = child.wait();
    let errs = errs.join().unwrap_or_default();
    let n = wrote.map_err(|e| RunFail::Store(format!("cannot write its output into the store: {e}")))?;
    match status {
        Ok(s) if s.success() => Ok(n),
        Ok(s) => {
            let text = String::from_utf8_lossy(&errs);
            Err(RunFail::Program(match text.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(l) if l.chars().count() > 300 => format!("{}...", l.chars().take(300).collect::<String>()),
                Some(l) => l.to_string(),
                None => s.to_string(),
            }))
        }
        Err(e) => Err(RunFail::Program(format!("cannot wait for {}: {e}", prog.display()))),
    }
}

// ---------------------------------------------------------------- folders

/// The file names a folder gives, by their last ending (in any case).
const LOG_ENDINGS: [&str; 11] = ["log", "out", "err", "txt", "json", "jsonl", "ndjson", "csv", "tsv", "ips", "crash"];

/// The endings of packed files: a folder gives the log names ending in one of these too.
const PACKED_ENDINGS: [&str; 5] = [".gz", ".bz2", ".xz", ".lz4", ".zst"];

/// Folders a walk passes over, besides hidden ones.
const PASSED_FOLDERS: [&str; 3] = [".git", "node_modules", "target"];

/// Whether a folder gives a file of this name: *.log, *.log.N, *.out, *.err,
/// *.txt, *.json, *.jsonl, *.ndjson, *.csv, *.tsv, *.ips and *.crash, the same names ending in
/// .gz, .bz2, .xz, .lz4 or .zst, and in a folder named log or logs (`in_logs`) names without a
/// dot, also rotated (syslog, syslog.1, messages.2.gz). Any case.
fn log_name(name: &str, in_logs: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    let base = PACKED_ENDINGS.iter().find_map(|e| lower.strip_suffix(e)).unwrap_or(&lower);
    // a rotated name: <name>.N
    if let Some((stem, n)) = base.rsplit_once('.')
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
    {
        return stem.ends_with(".log") || (in_logs && !stem.is_empty() && !stem.contains('.'));
    }
    match base.rsplit_once('.') {
        Some((stem, end)) => !stem.is_empty() && LOG_ENDINGS.contains(&end),
        None => in_logs && !base.is_empty(),
    }
}

/// What walking a folder found.
#[derive(Debug, Default)]
struct Walk {
    /// the files it gives, in order: each folder's files by name, then its folders by name
    files: Vec<PathBuf>,
    /// files passed over: not named as logs, hidden, links (never followed), not regular files
    not_logs: u64,
    hidden: u64,
    links: u64,
    special: u64,
    /// folders passed over: .git, node_modules, target and hidden folders; the store's folder
    folders: u64,
    store: bool,
    /// folders that could not be read, and why
    errors: Vec<String>,
}

/// Walk a folder without following links: the files it gives, and what it
/// passed over. `home_real` is the store's folder, resolved: passed over.
fn walk(root: &Path, home_real: &Path) -> Walk {
    let mut w = Walk::default();
    // (each folder once, by device and inode: a folder seen twice is not walked again)
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(m) = fs::metadata(&dir)
            && !seen.insert((m.dev(), m.ino()))
        {
            continue;
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                w.errors.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let mut entries = Vec::new();
        for e in rd {
            match e.and_then(|e| Ok((e.file_name(), e.file_type()?))) {
                Ok(x) => entries.push(x),
                Err(e) => w.errors.push(format!("{}: {e}", dir.display())),
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let in_logs = dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("log") || n.eq_ignore_ascii_case("logs"));
        let mut dirs = Vec::new();
        for (name, ft) in entries {
            let p = dir.join(&name);
            let hidden = name.as_bytes().first() == Some(&b'.');
            if ft.is_symlink() {
                w.links += 1;
            } else if ft.is_dir() {
                if hidden || PASSED_FOLDERS.iter().any(|f| name.as_bytes() == f.as_bytes()) {
                    w.folders += 1;
                } else if fs::canonicalize(&p).is_ok_and(|r| r.starts_with(home_real)) {
                    w.store = true;
                } else {
                    dirs.push(p);
                }
            } else if ft.is_file() {
                if hidden {
                    w.hidden += 1;
                } else if log_name(&name.to_string_lossy(), in_logs) {
                    w.files.push(p);
                } else {
                    w.not_logs += 1;
                }
            } else {
                w.special += 1;
            }
        }
        stack.extend(dirs.into_iter().rev());
    }
    w
}

impl Walk {
    /// The command's word on a folder: "folder logs: took 12 files (10 saved, 1 kept as it is,
    /// 1 not read); passed over 5 files (3 not named as logs, 1 hidden, 1 link, not followed)
    /// and 2 folders (.git, node_modules, target or hidden)". `done`: the files saved, already
    /// saved, kept as they are, not read, and failed.
    fn summary(&self, dir: &Path, done: &[u64; 5]) -> String {
        let mut s = format!("folder {}: took {}", dir.display(), plural(self.files.len() as u64, "file"));
        let what = ["saved", "already saved", "kept as it is", "not read", "failed"];
        let parts: Vec<String> = done.iter().zip(what).filter(|x| *x.0 > 0).map(|(n, w)| format!("{} {}", group(*n), if *n > 1 && w == "kept as it is" { "kept as they are" } else { w })).collect();
        if !parts.is_empty() {
            s.push_str(&format!(" ({})", parts.join(", ")));
        }
        let passed = self.not_logs + self.hidden + self.links + self.special;
        s.push_str(&format!("; passed over {}", plural(passed, "file")));
        let why: Vec<String> = [
            (self.not_logs, "not named as logs", "not named as logs"),
            (self.hidden, "hidden", "hidden"),
            (self.links, "link, not followed", "links, not followed"),
            (self.special, "not a regular file", "not regular files"),
        ]
        .iter()
        .filter(|x| x.0 > 0)
        .map(|&(n, one, many)| format!("{} {}", group(n), if n == 1 { one } else { many }))
        .collect();
        if !why.is_empty() {
            s.push_str(&format!(" ({})", why.join(", ")));
        }
        let mut folders = Vec::new();
        if self.folders > 0 {
            folders.push(format!("{} (.git, node_modules, target or hidden)", plural(self.folders, "folder")));
        }
        if self.store {
            folders.push("the memory store's folder".to_string());
        }
        if !self.errors.is_empty() {
            folders.push(format!("{} that could not be read", plural(self.errors.len() as u64, "folder or entry")));
        }
        if !folders.is_empty() {
            s.push_str(&format!(" and {}", folders.join(", ")));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_streams_the_same() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut whole = Hash128::new();
        whole.update(&data);
        let w = whole.finish();
        for cut in [0, 1, 7, 8, 9, 500, 999] {
            let mut h = Hash128::new();
            h.update(&data[..cut]);
            h.update(&data[cut..]);
            assert_eq!(h.finish(), w);
        }
        let mut a = Hash128::new();
        a.update(b"abc");
        let mut b = Hash128::new();
        b.update(b"abc\0");
        assert_ne!(a.finish(), b.finish());
        assert_eq!(w.len(), 32);
        // a copy of a hash in the middle gives the prefix's hash, and the hash goes on
        let mut h = Hash128::new();
        h.update(&data[..333]);
        let mut p = Hash128::new();
        p.update(&data[..333]);
        assert_eq!(h.clone().finish(), p.finish());
        h.update(&data[333..]);
        assert_eq!(h.finish(), w);
    }

    #[test]
    fn names_a_folder_gives() {
        for (name, in_logs, want) in [
            ("app.log", false, true),
            ("APP.LOG", false, true),
            ("app.log.1", false, true),
            ("app.log.12.gz", false, true),
            ("app.log.gz", false, true),
            ("events.jsonl.zst", false, true),
            ("data.ndjson", false, true),
            ("t.csv", false, true),
            ("t.tsv.xz", false, true),
            ("x.out", false, true),
            ("x.err.lz4", false, true),
            ("notes.txt.bz2", false, true),
            ("App-2026.ips", false, true),
            ("App.crash", false, true),
            ("README.md", false, false),
            ("image.png", false, false),
            ("app.json.1", false, false),
            ("syslog", false, false),
            ("syslog", true, true),
            ("syslog.1", true, true),
            ("messages.2.gz", true, true),
            ("x.y.1", true, false),
            ("archive.tar.gz", false, false),
            ("a.log.zip", false, false),
            ("1", true, true),
            (".log", false, false),
        ] {
            assert_eq!(log_name(name, in_logs), want, "{name} (in a log folder: {in_logs})");
        }
    }

    #[test]
    fn packed_files_by_their_first_bytes() {
        let gz = [0x1f, 0x8b, 0x08, 0, 0, 0];
        let bz = *b"BZh91AY&SY\x00\x00";
        let bz_empty = [b'B', b'Z', b'h', b'9', 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, 0, 0, 0, 0];
        let xz = [0xfd, b'7', b'z', b'X', b'Z', 0, 0];
        let lz4 = [0x04, 0x22, 0x4d, 0x18, 0x64];
        let zst = [0x28, 0xb5, 0x2f, 0xfd, 0x04];
        assert_eq!(packed("a.log.gz", &gz), Packed::Tool("gzip"));
        assert_eq!(packed("a.log", &gz), Packed::Tool("gzip"));
        assert_eq!(packed("a.bz2", &bz), Packed::Tool("bzip2"));
        assert_eq!(packed("a.bz2", &bz_empty), Packed::Tool("bzip2"));
        assert_eq!(packed("a.xz", &xz), Packed::Tool("xz"));
        assert_eq!(packed("a.lz4", &lz4), Packed::Tool("lz4"));
        assert_eq!(packed("a.lz4", &[0x02, 0x21, 0x4c, 0x18, 1]), Packed::Tool("lz4"));
        assert_eq!(packed("a.log.zst", &zst), Packed::Later("a zstd file"));
        assert_eq!(packed("x.tgz", &gz), Packed::Later("a tar bundle"));
        assert_eq!(packed("x.TAR.GZ", &gz), Packed::Later("a tar bundle"));
        assert_eq!(packed("x.tar.zst", &zst), Packed::Later("a tar bundle"));
        assert_eq!(packed("x.zip", b"PK\x03\x04rest"), Packed::Later("a zip bundle"));
        let mut tar = vec![b'a'; 600];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(packed("x.tar", &tar), Packed::Later("a tar bundle"));
        assert_eq!(packed("x.7z", &[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0]), Packed::Other("a 7z archive"));
        assert_eq!(packed("x.rar", b"Rar!\x1a\x07\x01\x00"), Packed::Other("a rar archive"));
        // text, whatever its name; and near misses
        assert_eq!(packed("a.log.gz", b"plain text\n"), Packed::No);
        assert_eq!(packed("notes.tar", b"a tar of notes\n"), Packed::No);
        assert_eq!(packed("a.bz2", b"BZh9 is not bzip2\n"), Packed::No);
        assert_eq!(packed("a", b""), Packed::No);
        assert_eq!(packed("a", &[0x1f, 0x8b]), Packed::No);
    }

    #[test]
    fn text_or_not() {
        assert_eq!(not_text(b""), None);
        assert_eq!(not_text(b"2026-11-18 INFO started\n"), None);
        assert_eq!(not_text("ünïcödé 日本 ok\n".as_bytes()), None);
        assert_eq!(not_text(b"\x1b[31mred\x1b[0m\r50%\r100%\x07\x08\x0c\n"), None);
        assert!(not_text(b"text\0more").is_some_and(|w| w.contains("NUL bytes") && w.contains("byte 4")));
        let mut b = vec![b'a'; 89];
        b.extend([0x01; 11]);
        assert!(not_text(&b).is_some_and(|w| w.contains("89% text bytes")));
        b.truncate(90);
        b.extend([0x01; 10]);
        b[89] = b'a';
        assert_eq!(not_text(&b), None, "90% is text");
    }

    #[test]
    fn inner_names() {
        assert_eq!(inner_name("/x/App.ips.gz", Some("gzip")), "/x/App.ips");
        assert_eq!(inner_name("/x/app.log.GZ", Some("gzip")), "/x/app.log");
        assert_eq!(inner_name("/x/a.csv.bz2", Some("bzip2")), "/x/a.csv");
        assert_eq!(inner_name("/x/a.tsv.xz", Some("xz")), "/x/a.tsv");
        assert_eq!(inner_name("/x/a.log.lz4", Some("lz4")), "/x/a.log");
        // read by a tool whose ending the name does not have, or by no tool
        assert_eq!(inner_name("/x/app.log", Some("gzip")), "/x/app.log");
        assert_eq!(inner_name("/x/a.gz", Some("bzip2")), "/x/a.gz");
        assert_eq!(inner_name("/x/a.gz", None), "/x/a.gz");
        assert_eq!(inner_name("/x/t.json", Some("json")), "/x/t.json");
        assert_eq!(inner_name(".gz", Some("gzip")), ".gz");
        assert_eq!(inner_name("/x/é.gz", Some("gzip")), "/x/é");
        // a catalog made again names the log type by the same name
        let rec = |j: &str| store::RunRec(crate::json::parse(j.as_bytes()).unwrap());
        assert_eq!(catalog::source_name(&rec(r#"{"id":1,"source":"/x/events.csv.gz","reader":"gzip"}"#)), "/x/events.csv");
        assert_eq!(catalog::source_name(&rec(r#"{"id":1,"source":"/x/events.csv.gz"}"#)), "/x/events.csv.gz");
        assert_eq!(catalog::source_name(&rec(r#"{"id":1,"kind":"export","cmd":["log","show","--last","1h"]}"#)), "log show --last 1h");
    }

    #[test]
    fn command_lines() {
        let a = |v: &[&str]| parse(v.iter().map(OsString::from).collect());
        let (items, o) = a(&["--hide-secrets", "x.log", "--", "--force"]).unwrap();
        assert_eq!(items, vec![Item::Path("x.log".into()), Item::Path("--force".into())]);
        assert!(o.hide && !o.force);
        let (items, o) = a(&["--macos-log", "--last", "5m", "--simulator", "--macos-log", "--predicate=process == \"x\""]).unwrap();
        assert_eq!(items, vec![Item::Os(Os::Mac), Item::Os(Os::Simulator)]);
        assert_eq!((o.last.as_deref(), o.predicate.as_deref()), (Some("5m"), Some("process == \"x\"")));
        let (argv, what) = export_of(Os::Mac, &o);
        assert_eq!(argv, ["log", "show", "--info", "--debug", "--style", "ndjson", "--last", "5m", "--predicate", "process == \"x\""].map(OsString::from));
        assert_eq!(what, "this Mac's log (the last 5m)");
        let (argv, _) = export_of(Os::Simulator, &Opts::default());
        assert_eq!(argv, ["xcrun", "simctl", "spawn", "booted", "log", "show", "--style", "ndjson", "--last", "1h"].map(OsString::from));
        let (items, o) = a(&["--journal", "--since", "2026-11-18 10:00"]).unwrap();
        assert_eq!(items, vec![Item::Os(Os::Journal)]);
        assert_eq!(export_of(Os::Journal, &o).0, ["journalctl", "-o", "json", "--all", "--since=2026-11-18 10:00"].map(OsString::from));
        assert_eq!(export_of(Os::Journal, &Opts::default()).0[4], "--since=-1h");
        let (items, o) = a(&["sys.logarchive", "--last", "boot"]).unwrap();
        assert_eq!(items, vec![Item::Path("sys.logarchive".into())]);
        assert_eq!(archive_export(Path::new("/x/sys.logarchive"), &o), ["log", "show", "--archive", "/x/sys.logarchive", "--style", "ndjson", "--last", "boot"].map(OsString::from));
        // usage errors: 2; help: 0
        for bad in [&[][..], &["--nope"], &["x.log", "--last", "1h"], &["--macos-log", "--last", "1w"], &["--macos-log", "--last"], &["--since", "1h", "x.log"], &["--force=yes", "x"], &["--predicate", "p", "x.log"]] {
            assert_eq!(a(bad).err(), Some(2), "{bad:?}");
        }
        assert_eq!(a(&["--help"]).err(), Some(0));
        for (l, ok) in [("1h", true), ("30", true), ("15s", true), ("2d", true), ("boot", true), ("h", false), ("1w", false), ("-1h", false), ("", false)] {
            assert_eq!(last_ok(l), ok, "{l}");
        }
    }

    #[test]
    fn walks_folders_without_links() {
        use std::os::unix::fs::symlink;
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-walk", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let store = d.join("store");
        for f in ["a.log", "b.txt", "README.md", ".hidden.log", "sub/c.log.1.gz", "sub/logs/syslog", "sub/logs/syslog.2", "sub/logs/x.y", "node_modules/m.log", "target/t.log", ".git/g.log", ".cache/h.log", "store/projects/p/r.log", "z/deep/d.jsonl"] {
            let p = d.join("w").join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "x\n").unwrap();
        }
        fs::rename(d.join("w").join("store"), &store).unwrap();
        symlink(&store, d.join("w").join("store-link")).unwrap();
        symlink(d.join("w").join("a.log"), d.join("w").join("link.log")).unwrap();
        symlink(d.join("w").join("sub"), d.join("w").join("sub-link")).unwrap();
        let store_real = fs::canonicalize(&store).unwrap();
        let w = walk(&d.join("w"), &store_real);
        let got: Vec<String> = w.files.iter().map(|p| p.strip_prefix(d.join("w")).unwrap().display().to_string()).collect();
        assert_eq!(got, ["a.log", "b.txt", "sub/c.log.1.gz", "sub/logs/syslog", "sub/logs/syslog.2", "z/deep/d.jsonl"]);
        assert_eq!((w.not_logs, w.hidden, w.links, w.special, w.folders, w.store), (2, 1, 3, 0, 4, false));
        assert!(w.errors.is_empty());
        assert_eq!(
            w.summary(Path::new("w"), &[4, 1, 0, 1, 0]),
            "folder w: took 6 files (4 saved, 1 already saved, 1 not read); passed over 6 files (2 not named as logs, 1 hidden, 3 links, not followed) and 4 folders (.git, node_modules, target or hidden)"
        );
        // the store's folder inside the walked folder is passed over
        fs::rename(&store, d.join("w").join("store")).unwrap();
        let w = walk(&d.join("w"), &fs::canonicalize(d.join("w").join("store")).unwrap());
        assert!(w.store && !w.files.iter().any(|p| p.ends_with("r.log")));
        assert!(w.summary(Path::new("w"), &[0; 5]).ends_with("and 4 folders (.git, node_modules, target or hidden), the memory store's folder"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn copies_count_hash_and_hide() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-copy", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gunVTLw\n-----END RSA PRIVATE KEY-----\n";
        let text = format!("a password=hunter22 here\n{key}tail AKIAABCDEFGHIJKLMNOP end");
        fs::write(d.join("t"), &text).unwrap();
        // a plain copy keeps every byte; its lines count as grep counts them
        let mut out = Vec::new();
        let mut h = Hash128::new();
        let c = copy_text(&mut File::open(d.join("t")).unwrap(), &mut h, None, &mut out).unwrap();
        assert_eq!(out, text.as_bytes());
        assert_eq!((c.size, c.bytes, c.lines), (text.len() as u64, text.len() as u64, 5));
        assert_eq!(hash_file(&d.join("t")).unwrap(), (text.len() as u64, h.finish()));
        // hidden: as secrets::hide hides the whole text
        let (want, counts) = secrets::hide(text.as_bytes(), &[]);
        let mut s = Stream::new(Hider::default());
        let mut out = Vec::new();
        let c = copy_text(&mut File::open(d.join("t")).unwrap(), &mut Hash128::new(), Some(&mut s), &mut out).unwrap();
        assert_eq!(out, want);
        assert_eq!((c.size, c.bytes, s.counts().total()), (text.len() as u64, want.len() as u64, counts.total()));
        assert!(counts.total() >= 3);
        // a new part that starts inside the key (at its first key line): primed with the lines
        // before it, its key lines are hidden as in the whole text's pass, and only its own
        // secrets count; without priming, the key line would stay
        let from = text.find("MIIE").unwrap() as u64;
        let mut f = File::open(d.join("t")).unwrap();
        f.seek(SeekFrom::Start(from)).unwrap();
        let mut out = Vec::new();
        copy_text(&mut f, &mut Hash128::new(), Some(&mut Stream::new(Hider::default())), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("MIIE"));
        let mut f = File::open(d.join("t")).unwrap();
        let mut s = Stream::new(Hider::default());
        let primed = prime(&mut f, from, &mut s).unwrap();
        f.seek(SeekFrom::Start(from)).unwrap();
        let mut out = Vec::new();
        copy_text(&mut f, &mut Hash128::new(), Some(&mut s), &mut out).unwrap();
        let tail = String::from_utf8(out).unwrap();
        assert!(!tail.contains("MIIE") && !tail.contains("AKIA"), "{tail}");
        assert!(s.counts().total() - primed >= 1);
        // copy_keep: the same bytes, renamed into place, the time kept
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(copy_keep(&d.join("t"), &d.join("k.keep"), Some(t)).unwrap(), hash_file(&d.join("t")).unwrap());
        assert_eq!(fs::read(d.join("k.keep")).unwrap(), text.as_bytes());
        assert_eq!(fs::metadata(d.join("k.keep")).unwrap().modified().unwrap(), t);
        assert_eq!(fs::metadata(d.join("k.keep")).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn programs_that_read() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-prog", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let sh = Path::new("/bin/sh");
        // its output into the file; stdin is /dev/null
        let n = run_to_file(sh, &[OsStr::new("-c"), OsStr::new("printf 'a\\nb\\n'; cat; echo done")], &d.join("o1")).unwrap();
        assert_eq!(fs::read(d.join("o1")).unwrap(), b"a\nb\ndone\n");
        assert_eq!(n, 9);
        // a failure: its stderr's first line, else its status
        let e = run_to_file(sh, &[OsStr::new("-c"), OsStr::new("echo part; echo; echo '  gz: bad data  ' >&2; echo more >&2; exit 1")], &d.join("o2")).unwrap_err();
        assert_eq!(e, RunFail::Program("gz: bad data".into()));
        let e = run_to_file(sh, &[OsStr::new("-c"), OsStr::new("exit 3")], &d.join("o3")).unwrap_err();
        assert!(matches!(&e, RunFail::Program(w) if w.contains('3')), "{e:?}");
        assert!(matches!(run_to_file(&d.join("nothing-here"), &[], &d.join("o4")), Err(RunFail::Program(w)) if w.starts_with("cannot run")));
        // the store cannot take the output (its file is there already): an error of ours
        assert!(matches!(run_to_file(sh, &[OsStr::new("-c"), OsStr::new("echo x")], &d.join("o1")), Err(RunFail::Store(w)) if w.starts_with("cannot write its output into the store")));
        fs::remove_dir_all(&d).unwrap();
    }
}
