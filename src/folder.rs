//! Folder archives: a file table plus the contents of every regular file, joined into one
//! stream in table order (so files share one pattern table: rotated logs pack better
//! together).
//!
//! Safety rules:
//! - Packing never follows symlinks inside the folder: a symlink is stored as a link.
//! - Unpacking goes into a new temporary folder next to the target, renamed into place at
//!   the end (removed on error). Every path is checked first: no absolute paths, no "." or
//!   ".." parts, no empty parts, no duplicates, at most MAX_PATH bytes, and every parent
//!   must be a folder from the table itself, so nothing can be written outside the target
//!   or through a link.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use crate::portable::{OsStrExt, OsStringExt};
use crate::portable::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::container::{self, get_varint, put_varint, temp_name, unzz, zz};
use crate::engine;

/// Longest path in a file table, in bytes (PATH_MAX on Linux: a longer path can be neither
/// packed nor unpacked). Paths are stored as "the first s bytes of the previous path, then
/// these bytes", so without a limit a tiny table could spell out huge paths.
pub const MAX_PATH: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// relative path with '/' between parts; empty for the folder itself
    pub path: Vec<u8>,
    /// permission bits (and sticky / set-id bits)
    pub mode: u32,
    /// modification time: seconds and nanoseconds since 1970 (UTC)
    pub mtime: (i64, u32),
    /// bytes (regular files)
    pub size: u64,
    /// where a symlink points (as stored, never followed)
    pub target: Vec<u8>,
    /// device and inode seen when scanning (packing only; not stored)
    pub id: (u64, u64),
}

fn mtime_of(md: &fs::Metadata) -> (i64, u32) {
    (md.mtime(), md.mtime_nsec().clamp(0, 999_999_999) as u32)
}

#[cfg_attr(fewer_small, optimize(size))]
fn join_rel(rel: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = rel.to_vec();
    if !p.is_empty() {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// A name as shown to people: one line, with control characters, backslashes and bytes
/// that are not UTF-8 written as escapes (\n, \t, \\, \xHH), as `ls -b` does. A name
/// from an archive can then never move the cursor or change the terminal.
#[cfg_attr(fewer_small, optimize(size))]
pub fn show_name(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if c.is_control() => {
                    let mut buf = [0u8; 4];
                    for x in c.encode_utf8(&mut buf).bytes() {
                        out.push_str(&format!("\\x{x:02x}"));
                    }
                }
                c => out.push(c),
            }
        }
        for x in chunk.invalid() {
            out.push_str(&format!("\\x{x:02x}"));
        }
    }
    out
}

#[cfg_attr(fewer_small, optimize(size))]
pub fn rel_to_path(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(std::ffi::OsStr::from_bytes(rel))
    }
}

/// Every entry under `root` (the folder itself first), depth first, names in byte order.
/// Symlinks are recorded as links; sockets, pipes and devices are skipped with a warning.
#[cfg_attr(fewer_small, optimize(size))]
pub fn scan(root: &Path, warn: &mut dyn FnMut(String)) -> Result<Vec<Entry>, String> {
    let md = fs::metadata(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    if !md.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let mut out = vec![Entry {
        kind: Kind::Dir,
        path: Vec::new(),
        mode: md.mode() & 0o7777,
        mtime: mtime_of(&md),
        size: 0,
        target: Vec::new(),
        id: (md.dev(), md.ino()),
    }];
    walk(root, &[], &mut out, warn)?;
    Ok(out)
}

#[cfg_attr(fewer_small, optimize(size))]
fn walk(dir: &Path, rel: &[u8], out: &mut Vec<Entry>, warn: &mut dyn FnMut(String)) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot read folder {}: {e}", dir.display()))?;
    let mut names: Vec<Vec<u8>> = Vec::new();
    for e in rd {
        let e = e.map_err(|e| format!("cannot read folder {}: {e}", dir.display()))?;
        names.push(e.file_name().into_vec());
    }
    let mut order: Vec<usize> = (0..names.len()).collect();
    crate::engine::sort_stable_idx(&mut order, &mut |&a, &b| names[a] < names[b]);
    for name in order.iter().map(|&k| &names[k]) {
        let p = dir.join(std::ffi::OsStr::from_bytes(name));
        let md = fs::symlink_metadata(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        let path = join_rel(rel, name);
        if path.len() > MAX_PATH {
            // a reader would refuse the table
            return Err(format!("{}: path too long for an archive (more than {MAX_PATH} bytes)", p.display()));
        }
        let ft = md.file_type();
        let mut e = Entry {
            kind: Kind::File,
            path,
            mode: md.mode() & 0o7777,
            mtime: mtime_of(&md),
            size: 0,
            target: Vec::new(),
            id: (md.dev(), md.ino()),
        };
        if ft.is_dir() {
            e.kind = Kind::Dir;
            let sub = e.path.clone();
            out.push(e);
            walk(&p, &sub, out, warn)?;
        } else if ft.is_file() {
            e.size = md.len();
            out.push(e);
        } else if ft.is_symlink() {
            e.kind = Kind::Symlink;
            e.target = fs::read_link(&p).map_err(|e| format!("cannot read link {}: {e}", p.display()))?.into_os_string().into_vec();
            out.push(e);
        } else {
            warn(format!("skipping {} (not a file, folder or link)", p.display()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- the file table

/// The table as bytes (paths front-coded: bytes shared with the previous path + the rest).
#[cfg_attr(fewer_small, optimize(size))]
pub fn encode_table(entries: &[Entry]) -> Vec<u8> {
    let mut o = Vec::new();
    put_varint(&mut o, entries.len() as u64);
    let mut prev: &[u8] = &[];
    for e in entries {
        o.push(match e.kind {
            Kind::Dir => 0,
            Kind::File => 1,
            Kind::Symlink => 2,
        });
        let shared = prev.iter().zip(&e.path).take_while(|(a, b)| a == b).count();
        put_varint(&mut o, shared as u64);
        put_varint(&mut o, (e.path.len() - shared) as u64);
        o.extend_from_slice(&e.path[shared..]);
        put_varint(&mut o, e.mode as u64);
        put_varint(&mut o, zz(e.mtime.0));
        put_varint(&mut o, e.mtime.1 as u64);
        match e.kind {
            Kind::File => put_varint(&mut o, e.size),
            Kind::Symlink => {
                put_varint(&mut o, e.target.len() as u64);
                o.extend_from_slice(&e.target);
            }
            Kind::Dir => {}
        }
        prev = &e.path;
    }
    o
}

/// Is this a safe relative path: parts separated by '/', none empty, ".", ".." or with a
/// zero byte, not starting with '/'.
#[cfg_attr(fewer_small, optimize(size))]
pub fn safe_path(p: &[u8]) -> bool {
    !p.is_empty() && !p.contains(&0) && p.split(|&c| c == b'/').all(|part| !part.is_empty() && part != b"." && part != b"..")
}

#[cfg_attr(fewer_small, optimize(size))]
fn parent_of(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&c| c == b'/') {
        Some(i) => &p[..i],
        None => &[],
    }
}

/// Read and check a table: the folder itself first, then entries whose parent folder came
/// earlier in the table, with safe and unique paths.
#[cfg_attr(fewer_small, optimize(size))]
pub fn decode_table(b: &[u8]) -> Result<Vec<Entry>, String> {
    const BAD: &str = "damaged archive (bad file table)";
    let mut p = 0;
    let n = get_varint(b, &mut p).ok_or(BAD)?;
    // an entry takes at least 6 bytes
    if n == 0 || n > b.len() as u64 / 6 {
        return Err(BAD.into());
    }
    let mut out: Vec<Entry> = Vec::with_capacity(n as usize);
    let mut dirs: HashSet<Vec<u8>> = HashSet::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut prev: Vec<u8> = Vec::new();
    for i in 0..n {
        let kind = match *b.get(p).ok_or(BAD)? {
            0 => Kind::Dir,
            1 => Kind::File,
            2 => Kind::Symlink,
            _ => return Err(BAD.into()),
        };
        p += 1;
        let shared = get_varint(b, &mut p).ok_or(BAD)? as usize;
        let rest = get_varint(b, &mut p).ok_or(BAD)?;
        if shared > prev.len() || rest > (b.len() - p) as u64 || shared + rest as usize > MAX_PATH {
            return Err(BAD.into());
        }
        let mut path = prev[..shared].to_vec();
        path.extend_from_slice(&b[p..p + rest as usize]);
        p += rest as usize;
        let mode = get_varint(b, &mut p).ok_or(BAD)?;
        let secs = unzz(get_varint(b, &mut p).ok_or(BAD)?);
        let nanos = get_varint(b, &mut p).ok_or(BAD)?;
        if mode > 0o7777 || nanos > 999_999_999 {
            return Err(BAD.into());
        }
        let mut e = Entry { kind, path, mode: mode as u32, mtime: (secs, nanos as u32), size: 0, target: Vec::new(), id: (0, 0) };
        match kind {
            Kind::File => e.size = get_varint(b, &mut p).ok_or(BAD)?,
            Kind::Symlink => {
                let tl = get_varint(b, &mut p).ok_or(BAD)?;
                if tl == 0 || tl > (b.len() - p) as u64 {
                    return Err(BAD.into());
                }
                e.target = b[p..p + tl as usize].to_vec();
                p += tl as usize;
                if e.target.contains(&0) {
                    return Err(BAD.into());
                }
            }
            Kind::Dir => {}
        }
        if i == 0 {
            if kind != Kind::Dir || !e.path.is_empty() {
                return Err(BAD.into());
            }
            dirs.insert(Vec::new());
        } else {
            if !safe_path(&e.path) {
                return Err(format!("unsafe path in archive: {}", show_name(&e.path)));
            }
            if !dirs.contains(parent_of(&e.path)) {
                return Err(format!("archive path has no parent folder in the archive: {}", show_name(&e.path)));
            }
            if !seen.insert(e.path.clone()) {
                return Err(format!("archive path appears twice: {}", show_name(&e.path)));
            }
            if kind == Kind::Dir {
                dirs.insert(e.path.clone());
            }
        }
        prev = e.path.clone();
        out.push(e);
    }
    if p != b.len() {
        return Err(BAD.into());
    }
    Ok(out)
}

/// The meta record of a folder archive: varint(table bytes) + the table packed by the engine.
#[cfg_attr(fewer_small, optimize(size))]
pub fn table_meta(entries: &[Entry], threads: usize) -> Vec<u8> {
    let t = encode_table(entries);
    let mut m = Vec::new();
    put_varint(&mut m, t.len() as u64);
    m.extend_from_slice(&engine::pack(&t, threads));
    m
}

#[cfg_attr(fewer_small, optimize(size))]
pub fn table_from_meta(meta: &[u8], threads: usize) -> Result<Vec<Entry>, String> {
    let mut p = 0;
    let n = get_varint(meta, &mut p).ok_or("damaged archive (bad file table)")?;
    let t = engine::unpack(&meta[p..], threads).map_err(|e| if e.starts_with("made by ") { e } else { format!("damaged archive (file table: {e})") })?;
    if t.len() as u64 != n {
        return Err("damaged archive (bad file table)".into());
    }
    decode_table(&t)
}

/// Total bytes of all regular files.
#[cfg_attr(fewer_small, optimize(size))]
pub fn total_size(entries: &[Entry]) -> u64 {
    entries.iter().filter(|e| e.kind == Kind::File).map(|e| e.size).sum()
}

// ---------------------------------------------------------------- reading the files as one stream

// open(2) flag "do not follow a symlink" (its value differs between systems; elsewhere the
// device and inode check after opening still catches a swapped file)
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm", target_arch = "powerpc64")))]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(any(
    target_os = "macos",
    target_os = "freebsd",
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "arm", target_arch = "powerpc64"))
)))]
const O_NOFOLLOW: i32 = 0;

/// Every regular file's bytes, one after the other, exactly the sizes in the table (a file
/// that grew is read up to its size at scan time; one that shrank is an error).
pub struct FolderReader {
    root: PathBuf,
    files: Vec<(Vec<u8>, u64, (u64, u64))>,
    next: usize,
    cur: Option<(fs::File, u64)>,
    pub grew: Vec<Vec<u8>>,
}

impl FolderReader {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn new(root: &Path, entries: &[Entry]) -> FolderReader {
        let files = entries.iter().filter(|e| e.kind == Kind::File).map(|e| (e.path.clone(), e.size, e.id)).collect();
        FolderReader { root: root.to_path_buf(), files, next: 0, cur: None, grew: Vec::new() }
    }

    #[cfg_attr(fewer_small, optimize(size))]
    fn open_next(&mut self) -> io::Result<bool> {
        while self.next < self.files.len() {
            let (rel, size, id) = self.files[self.next].clone();
            self.next += 1;
            if size == 0 {
                continue;
            }
            let p = rel_to_path(&self.root, &rel);
            let f = fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(&p).map_err(|e| io::Error::new(e.kind(), format!("cannot read {}: {e}", p.display())))?;
            let md = f.metadata()?;
            if !md.is_file() || (md.dev(), md.ino()) != id {
                return Err(io::Error::other(format!("{} was replaced while packing", p.display())));
            }
            self.cur = Some((f, size));
            return Ok(true);
        }
        Ok(false)
    }
}

impl Read for FolderReader {
    #[cfg_attr(fewer_small, optimize(size))]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.cur.is_none() && !self.open_next()? {
            return Ok(0);
        }
        let (f, left) = self.cur.as_mut().unwrap();
        let want = (*left).min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            let rel = &self.files[self.next - 1].0;
            return Err(io::Error::other(format!("{} got shorter while packing; pack again", rel_to_path(&self.root, rel).display())));
        }
        *left -= n as u64;
        if *left == 0 {
            // a file that grew since the scan: its new bytes are not packed
            let mut one = [0u8; 1];
            if f.read(&mut one).map(|k| k > 0).unwrap_or(false) {
                self.grew.push(self.files[self.next - 1].0.clone());
            }
            self.cur = None;
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------- unpacking

#[cfg_attr(fewer_small, optimize(size))]
pub fn to_time(t: (i64, u32)) -> SystemTime {
    if t.0 >= 0 {
        UNIX_EPOCH + Duration::new(t.0 as u64, t.1)
    } else {
        UNIX_EPOCH - Duration::from_secs(t.0.unsigned_abs()) + Duration::from_nanos(t.1 as u64)
    }
}

/// A mode taken from an archive, as it is given to a file: without set-user and set-group
/// bits, and limited by the umask (as tar does for normal users).
#[cfg_attr(fewer_small, optimize(size))]
pub fn archive_mode(mode: u32) -> u32 {
    mode & 0o1777 & !container::umask_bits()
}

/// Set a file's (or folder's) modification time and permission bits through an open handle.
#[cfg_attr(fewer_small, optimize(size))]
pub fn set_mode_time(f: &fs::File, mode: u32, mtime: (i64, u32)) -> io::Result<()> {
    f.set_times(fs::FileTimes::new().set_modified(to_time(mtime)).set_accessed(SystemTime::now()))?;
    f.set_permissions(fs::Permissions::from_mode(mode & 0o1777))
}

/// Set a symlink's own modification time (not its target's).
#[cfg(all(target_pointer_width = "64", any(target_os = "macos", target_os = "linux")))]
fn set_link_time(p: &Path, t: (i64, u32)) {
    #[repr(C)]
    struct Timespec {
        sec: i64,
        nsec: i64,
    }
    unsafe extern "C" {
        fn utimensat(dirfd: i32, path: *const std::ffi::c_char, times: *const Timespec, flags: i32) -> i32;
    }
    #[cfg(target_os = "macos")]
    const AT: (i32, i32, i64) = (-2, 0x0020, -2); // AT_FDCWD, AT_SYMLINK_NOFOLLOW, UTIME_OMIT
    #[cfg(target_os = "linux")]
    const AT: (i32, i32, i64) = (-100, 0x100, (1 << 30) - 2);
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return };
    let ts = [Timespec { sec: 0, nsec: AT.2 }, Timespec { sec: t.0, nsec: t.1 as i64 }];
    // SAFETY: valid C string and a 2-element timespec array, as utimensat(2) expects
    unsafe {
        utimensat(AT.0, c.as_ptr(), ts.as_ptr(), AT.1);
    }
}
#[cfg(not(all(target_pointer_width = "64", any(target_os = "macos", target_os = "linux"))))]
fn set_link_time(_p: &Path, _t: (i64, u32)) {}

/// Writes a folder archive's stream back into files, in a temporary folder that is renamed
/// to `dest` only when everything is complete (removed on error and on Ctrl-C).
pub struct Extractor {
    tmp: PathBuf,
    dest: PathBuf,
    entries: Vec<Entry>,
    files: Vec<usize>,
    next: usize,
    cur: Option<(io::BufWriter<fs::File>, u64, usize)>,
    done: bool,
    /// force every file, folder and the new name to disk (-d --rm deletes the archive next)
    durable: bool,
}

impl Extractor {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn new(dest: &Path, entries: Vec<Entry>, durable: bool) -> Result<Extractor, String> {
        let tmp = temp_name(dest, "dir");
        {
            let _g = container::gate();
            // private to the owner until it is complete
            fs::DirBuilder::new().mode(0o700).create(&tmp).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
            container::register_temp_dir(&tmp);
        }
        let files = (0..entries.len()).filter(|&i| entries[i].kind == Kind::File).collect();
        let x = Extractor { tmp, dest: dest.to_path_buf(), entries, files, next: 0, cur: None, done: false, durable };
        // folders first (the table lists every parent before its children). The gate is
        // taken after `x`, so on an error it is let go before `x` is dropped (which takes it).
        let _g = container::gate();
        for e in &x.entries[1..] {
            if e.kind == Kind::Dir {
                let p = rel_to_path(&x.tmp, &e.path);
                fs::create_dir(&p).map_err(|er| format!("cannot create {}: {er}", show_name(&e.path)))?;
            }
        }
        drop(_g);
        Ok(x)
    }

    #[cfg_attr(fewer_small, optimize(size))]
    fn close_cur(&mut self) -> Result<(), String> {
        if let Some((w, _, i)) = self.cur.take() {
            let e = &self.entries[i];
            let f = w.into_inner().map_err(|er| format!("cannot write {}: {}", show_name(&e.path), er.error()))?;
            set_mode_time(&f, archive_mode(e.mode), e.mtime).map_err(|er| format!("cannot set times of {}: {er}", show_name(&e.path)))?;
            // while the file is still open for writing (its mode may not let us open it again)
            if self.durable {
                f.sync_all().map_err(|er| format!("cannot force {} to disk: {er}", show_name(&e.path)))?;
            }
        }
        Ok(())
    }

    #[cfg_attr(fewer_small, optimize(size))]
    fn open_next(&mut self) -> Result<bool, String> {
        while self.next < self.files.len() {
            let i = self.files[self.next];
            self.next += 1;
            let e = &self.entries[i];
            let p = rel_to_path(&self.tmp, &e.path);
            let f = {
                let _g = container::gate();
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(O_NOFOLLOW)
                    .open(&p)
                    .map_err(|er| format!("cannot create {}: {er}", show_name(&e.path)))?
            };
            self.cur = Some((io::BufWriter::with_capacity(1 << 18, f), e.size, i));
            if e.size > 0 {
                return Ok(true);
            }
            self.close_cur()?;
        }
        Ok(false)
    }

    /// The next bytes of the stream.
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn write(&mut self, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            if self.cur.is_none() && !self.open_next()? {
                return Err("damaged archive (more data than its file table lists)".into());
            }
            let (w, left, i) = self.cur.as_mut().unwrap();
            let take = (*left).min(data.len() as u64) as usize;
            w.write_all(&data[..take]).map_err(|e| format!("cannot write {}: {e}", show_name(&self.entries[*i].path)))?;
            *left -= take as u64;
            data = &data[take..];
            if *left == 0 {
                self.close_cur()?;
            }
        }
        Ok(())
    }

    /// Check that every file is complete, make the links, set folder times, and put the
    /// folder in place. An existing file (not a folder) of that name is replaced only when
    /// `replace` is set, and only now that the folder is complete.
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn finish(mut self, replace: bool) -> Result<(), String> {
        if self.cur.is_some() {
            return Err("damaged archive (data ends inside a file)".into());
        }
        if self.open_next()? {
            return Err("damaged archive (data ends before the last file)".into());
        }
        let _g = container::gate();
        for e in &self.entries {
            if e.kind == Kind::Symlink {
                let p = rel_to_path(&self.tmp, &e.path);
                crate::portable::symlink(Path::new(std::ffi::OsStr::from_bytes(&e.target)), &p).map_err(|er| format!("cannot create link {}: {er}", show_name(&e.path)))?;
                set_link_time(&p, e.mtime);
            }
        }
        // folders last, deepest first (writing inside a folder changes its time)
        for e in self.entries.iter().rev() {
            if e.kind == Kind::Dir {
                let p = rel_to_path(&self.tmp, &e.path);
                let f = fs::File::open(&p).map_err(|er| format!("cannot open {}: {er}", show_name(&e.path)))?;
                let mode = if e.path.is_empty() { 0o700 } else { archive_mode(e.mode) };
                set_mode_time(&f, mode, e.mtime).map_err(|er| format!("cannot set times of {}: {er}", show_name(&e.path)))?;
                if self.durable {
                    f.sync_all().map_err(|er| format!("cannot force {} to disk: {er}", show_name(&e.path)))?;
                }
            }
        }
        let rename = |to: &Path| fs::rename(&self.tmp, to).map_err(|e| format!("cannot rename to {}: {e}", self.dest.display()));
        match fs::symlink_metadata(&self.dest) {
            Err(_) => rename(&self.dest)?,
            Ok(md) if md.is_dir() => return Err(format!("{} appeared while unpacking; not replacing it", self.dest.display())),
            Ok(_) if !replace => return Err(format!("{} already exists (use -f to overwrite)", self.dest.display())),
            Ok(_) => {
                // move the old file aside, put the folder in its place, then delete the old file
                let old = temp_name(&self.dest, "old");
                fs::rename(&self.dest, &old).map_err(|e| format!("cannot replace {}: {e}", self.dest.display()))?;
                if let Err(e) = rename(&self.dest) {
                    let _ = fs::rename(&old, &self.dest);
                    return Err(e);
                }
                let _ = fs::remove_file(&old);
            }
        }
        self.done = true;
        container::unregister_temp_dir(&self.tmp);
        if self.durable {
            // the folder's new name too, before the archive is deleted
            let parent = self.dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
            fs::File::open(parent).and_then(|d| d.sync_all()).map_err(|e| format!("cannot force {} to disk: {e}", self.dest.display()))?;
        }
        // the folder itself: its own mode (it was kept private until now) and time
        let root = &self.entries[0];
        if let Ok(f) = fs::File::open(&self.dest) {
            let _ = set_mode_time(&f, archive_mode(root.mode), root.mtime);
        }
        Ok(())
    }
}

impl Drop for Extractor {
    #[cfg_attr(fewer_small, optimize(size))]
    fn drop(&mut self) {
        if !self.done {
            self.cur = None;
            let _g = container::gate();
            container::remove_tree(&self.tmp);
            container::unregister_temp_dir(&self.tmp);
        }
    }
}

// ---------------------------------------------------------------- --rm for a folder

#[cfg(unix)]
unsafe extern "C" {
    fn fchdir(fd: i32) -> i32;
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))))]
const O_NONBLOCK: i32 = 0o4000;

/// Is this entry (just read with lstat) still exactly what was packed?
#[cfg(unix)]
#[cfg_attr(fewer_small, optimize(size))]
fn unchanged(e: &Entry, md: &fs::Metadata) -> bool {
    let same_id = (md.dev(), md.ino()) == e.id;
    match e.kind {
        Kind::File => md.file_type().is_file() && same_id && md.len() == e.size && mtime_of(md) == e.mtime,
        Kind::Symlink => md.file_type().is_symlink() && same_id,
        Kind::Dir => md.file_type().is_dir() && same_id,
    }
}

/// Open a folder by its name in the current folder (never through a link, never blocking
/// on a pipe) and check it is the folder that was scanned.
#[cfg(unix)]
#[cfg_attr(fewer_small, optimize(size))]
fn open_scanned_dir(name: &Path, e: &Entry, follow: bool) -> Result<fs::File, String> {
    let flags = O_NONBLOCK | if follow { 0 } else { O_NOFOLLOW };
    let f = fs::OpenOptions::new().read(true).custom_flags(flags).open(name).map_err(|er| {
        if !follow && fs::symlink_metadata(name).is_ok_and(|m| m.file_type().is_symlink()) {
            "it changed while packing (it is a symbolic link now; not followed)".to_string()
        } else {
            er.to_string()
        }
    })?;
    let md = f.metadata().map_err(|er| er.to_string())?;
    if !md.is_dir() || (md.dev(), md.ino()) != e.id {
        return Err("it changed while packing".into());
    }
    Ok(f)
}

/// --rm for a folder: delete exactly what was packed, and nothing that changed since.
///
/// Each folder is entered through an open handle that is checked to be the folder that was
/// scanned (same device and inode), and every entry is removed by its plain name inside it,
/// only if it is still what was packed: same device and inode, and for files the same size
/// and modification time. So a link swapped in for a folder is never followed, and a file
/// that grew, was rewritten or was rotated is kept. Folders are removed once empty.
/// Returns what was kept, and why (empty when everything was removed).
///
/// On Windows nothing is removed yet (it has no way to enter a folder by an open handle):
/// the folder is kept, and the message says so.
#[cfg(windows)]
pub fn remove_packed(root: &Path, _entries: &[Entry]) -> Vec<String> {
    vec![format!("kept {}: --rm for a folder is not on Windows yet; the archive is complete", root.display())]
}

#[cfg(unix)]
#[cfg_attr(fewer_small, optimize(size))]
pub fn remove_packed(root: &Path, entries: &[Entry]) -> Vec<String> {
    use std::os::fd::AsRawFd;
    let mut kept = Vec::new();
    // the entries inside each folder
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut dirs: std::collections::HashMap<&[u8], usize> = std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            match dirs.get(parent_of(&e.path)) {
                Some(&d) => kids[d].push(i),
                None => kept.push(format!("kept {}: not found in the scan", show_name(&e.path))),
            }
        }
        if e.kind == Kind::Dir {
            dirs.insert(&e.path, i);
        }
    }
    let home = match fs::File::open(".") {
        Ok(h) => h,
        Err(er) => {
            kept.push(format!("kept {}: cannot open the current folder: {er}", root.display()));
            return kept;
        }
    };
    match open_scanned_dir(root, &entries[0], true) {
        Ok(top) => clear_dir(&top, 0, entries, &kids, root, &mut kept),
        Err(er) => kept.push(format!("kept {}: {er}", root.display())),
    }
    // SAFETY: fchdir on a folder handle we hold open
    if unsafe { fchdir(home.as_raw_fd()) } != 0 {
        // later inputs given by relative names would be looked up in the wrong folder
        for k in &kept {
            eprintln!("fewer-pack: {k}");
        }
        eprintln!("fewer-pack: cannot go back to the current folder; stopping");
        std::process::exit(1);
    }
    if kept.is_empty() {
        if let Err(er) = fs::remove_dir(root) {
            kept.push(format!("kept {}: {er}", root.display()));
        }
    }
    kept
}

/// Remove what was packed inside the folder `dir` (entry `d`), deepest first.
#[cfg(unix)]
#[cfg_attr(fewer_small, optimize(size))]
fn clear_dir(dir: &fs::File, d: usize, entries: &[Entry], kids: &[Vec<usize>], root: &Path, kept: &mut Vec<String>) {
    use std::os::fd::AsRawFd;
    let shown = |e: &Entry| show_name(&rel_to_path(root, &e.path).into_os_string().into_vec());
    for &k in kids[d].iter().rev() {
        // SAFETY: fchdir on a folder handle we hold open
        if unsafe { fchdir(dir.as_raw_fd()) } != 0 {
            kept.push(format!("kept {}: cannot enter its folder", shown(&entries[d])));
            return;
        }
        let e = &entries[k];
        let name = Path::new(std::ffi::OsStr::from_bytes(e.path.rsplit(|&c| c == b'/').next().unwrap_or(&e.path)));
        if e.kind == Kind::Dir {
            match open_scanned_dir(name, e, false) {
                Ok(sub) => {
                    let before = kept.len();
                    clear_dir(&sub, k, entries, kids, root, kept);
                    // SAFETY: as above
                    if unsafe { fchdir(dir.as_raw_fd()) } != 0 {
                        kept.push(format!("kept {}: cannot enter its folder", shown(e)));
                        return;
                    }
                    if kept.len() == before {
                        if let Err(er) = fs::remove_dir(name) {
                            kept.push(format!("kept {}: {er}", shown(e)));
                        }
                    }
                }
                Err(er) => kept.push(format!("kept {}: {er}", shown(e))),
            }
            continue;
        }
        match fs::symlink_metadata(name) {
            Ok(md) if unchanged(e, &md) => {
                if let Err(er) = fs::remove_file(name) {
                    kept.push(format!("kept {}: {er}", shown(e)));
                }
            }
            Ok(_) => kept.push(format!("kept {}: it changed while packing", shown(e))),
            Err(er) => kept.push(format!("kept {}: {er}", shown(e))),
        }
    }
}

/// `ls -l`-style lines for `fewer-pack list`.
#[cfg_attr(fewer_small, optimize(size))]
pub fn mode_string(kind: Kind, mode: u32) -> String {
    let mut s = String::with_capacity(10);
    s.push(match kind {
        Kind::Dir => 'd',
        Kind::File => '-',
        Kind::Symlink => 'l',
    });
    let bits = [(0o400, 'r'), (0o200, 'w'), (0o100, 'x'), (0o040, 'r'), (0o020, 'w'), (0o010, 'x'), (0o004, 'r'), (0o002, 'w'), (0o001, 'x')];
    for (k, (b, c)) in bits.iter().enumerate() {
        let mut ch = if mode & b != 0 { *c } else { '-' };
        if k == 2 && mode & 0o4000 != 0 {
            ch = if ch == 'x' { 's' } else { 'S' };
        }
        if k == 5 && mode & 0o2000 != 0 {
            ch = if ch == 'x' { 's' } else { 'S' };
        }
        if k == 8 && mode & 0o1000 != 0 {
            ch = if ch == 'x' { 't' } else { 'T' };
        }
        s.push(ch);
    }
    s
}

/// "YYYY-MM-DD HH:MM" in UTC.
#[cfg_attr(fewer_small, optimize(size))]
pub fn date_string(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil date from days since 1970-01-01 (H. Hinnant's method)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: Kind, path: &str) -> Entry {
        Entry { kind, path: path.as_bytes().to_vec(), mode: 0o644, mtime: (1_700_000_000, 5), size: 3, target: b"x".to_vec(), id: (0, 0) }
    }

    #[test]
    fn table_round_trip() {
        let mut v = vec![e(Kind::Dir, ""), e(Kind::Dir, "a"), e(Kind::File, "a/x.log"), e(Kind::File, "a/x.log.1"), e(Kind::Symlink, "a/y"), e(Kind::File, "b c ü.log")];
        v[0].size = 0;
        v[1].size = 0;
        v[4].size = 0;
        v[0].target.clear();
        v[1].target.clear();
        v[2].target.clear();
        v[3].target.clear();
        v[5].target.clear();
        v[3].mtime = (-5, 999_999_999);
        let t = encode_table(&v);
        assert_eq!(decode_table(&t).unwrap(), v);
        for cut in 0..t.len() {
            assert!(decode_table(&t[..cut]).is_err());
        }
    }

    #[test]
    fn unsafe_tables_are_refused() {
        let root = e(Kind::Dir, "");
        for bad in ["../x", "a/../../x", "/etc/passwd", "a//b", "./a", "a/.", ""] {
            let mut r = root.clone();
            r.size = 0;
            let t = encode_table(&[r.clone(), e(Kind::File, bad)]);
            assert!(decode_table(&t).is_err(), "{bad} accepted");
        }
        // a file under a link or under a file
        let mut r = root.clone();
        r.size = 0;
        let t = encode_table(&[r.clone(), e(Kind::Symlink, "l"), e(Kind::File, "l/x")]);
        assert!(decode_table(&t).is_err());
        let t = encode_table(&[r.clone(), e(Kind::File, "f"), e(Kind::File, "f/x")]);
        assert!(decode_table(&t).is_err());
        // a parent listed after its child, a duplicate, the folder itself not first
        let t = encode_table(&[r.clone(), e(Kind::File, "d/x"), e(Kind::Dir, "d")]);
        assert!(decode_table(&t).is_err());
        let t = encode_table(&[r.clone(), e(Kind::File, "x"), e(Kind::File, "x")]);
        assert!(decode_table(&t).is_err());
        let t = encode_table(&[e(Kind::File, "x")]);
        assert!(decode_table(&t).is_err());
        // paths longer than MAX_PATH (front coding once let a 599-byte archive spell out
        // 600 paths of 1 MiB each)
        let long = "a".repeat(MAX_PATH);
        assert!(decode_table(&encode_table(&[r.clone(), e(Kind::File, &long)])).is_ok());
        let t = encode_table(&[r.clone(), e(Kind::File, &long), e(Kind::File, &(long.clone() + "b"))]);
        assert!(decode_table(&t).is_err());
    }

    #[test]
    fn safe_paths() {
        assert!(safe_path(b"a"));
        assert!(safe_path(b"a/b.log"));
        assert!(safe_path(b"..a/b.."));
        assert!(!safe_path(b"a\0b"));
        assert!(!safe_path(b"/a"));
        assert!(!safe_path(b"a/"));
    }

    #[test]
    fn dates_and_modes() {
        assert_eq!(date_string(0), "1970-01-01 00:00");
        assert_eq!(date_string(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(date_string(-1), "1969-12-31 23:59");
        assert_eq!(mode_string(Kind::File, 0o644), "-rw-r--r--");
        assert_eq!(mode_string(Kind::Dir, 0o1777), "drwxrwxrwt");
    }
}
