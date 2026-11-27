//! The fewer-pack archive container (format version 1).
//!
//!   archive = "CMLG" version flags meta segment* end
//!   meta    = varint(n) n bytes CRC-32          (file info, or the folder table)
//!   segment = varint(raw bytes > 0) varint(packed bytes) CRC-32 packed
//!   end     = varint(0) varint(segments) varint(total raw bytes) CRC-32
//!
//! - The input is cut into segments of at most 256 MiB, just after a line end when there is
//!   one, so memory stays bounded for any input size (stdin too). Each segment is packed on
//!   its own by the engine.
//! - Every CRC is checked before anything is decoded; the end record catches a cut-off file.
//! - Output files are written to a temporary file in the same folder, forced to disk, then
//!   renamed; on any error (or Ctrl-C) the temporary file is removed.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::crc::{Crc, crc32};
use crate::engine;

pub const MAGIC: &[u8; 4] = b"CMLG";
pub const VERSION: u8 = 1;
/// flags bit 0: a folder archive (the meta record holds the folder table)
pub const FLAG_FOLDER: u8 = 1;
/// Biggest segment this version writes.
pub const SEGMENT_MAX: usize = 256 << 20;
/// Biggest segment a reader accepts (room for later versions).
const READ_MAX: u64 = 1 << 30;

/// The sizes a reader accepts for a segment (`raw` original bytes packed into `packed`),
/// and so the only ones a writer may write.
fn segment_fits(raw: u64, packed: u64) -> bool {
    raw > 0 && raw <= READ_MAX && packed <= 2 * raw + (1 << 20)
}

pub fn put_varint(o: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        o.push(v as u8 | 0x80);
        v >>= 7;
    }
    o.push(v as u8);
}

/// A varint from a byte string (None if cut short or too long).
pub fn get_varint(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut s = 0;
    loop {
        let x = *b.get(*p)?;
        *p += 1;
        if s > 63 {
            return None;
        }
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return Some(v);
        }
        s += 7;
    }
}

pub fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
pub fn unzz(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

// ---------------------------------------------------------------- segments of the input

/// Cuts a byte stream into segments of at most `max` bytes, just after a line end when the
/// segment holds one.
pub struct Segmenter<R: Read> {
    r: R,
    carry: Vec<u8>,
    max: usize,
    eof: bool,
}

impl<R: Read> Segmenter<R> {
    /// `size`: the input's size when known (segments are then made about equal).
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn new(r: R, size: Option<u64>) -> Segmenter<R> {
        Segmenter { r, carry: Vec::new(), max: segment_max(size), eof: false }
    }

    pub fn next_segment(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buf = std::mem::take(&mut self.carry);
        if !self.eof && buf.len() < self.max {
            buf.reserve_exact(self.max - buf.len());
            let want = (self.max - buf.len()) as u64;
            let got = (&mut self.r).take(want).read_to_end(&mut buf)? as u64;
            if got < want {
                self.eof = true;
            }
        }
        if buf.is_empty() {
            return Ok(None);
        }
        // a full segment ends after its last line end (if it has one)
        let cut = if buf.len() >= self.max { buf.iter().rposition(|&b| b == b'\n').filter(|&p| p + 1 < buf.len()) } else { None };
        if let Some(p) = cut {
            self.carry = buf[p + 1..].to_vec();
            buf.truncate(p + 1);
        }
        Ok(Some(buf))
    }
}

// ---------------------------------------------------------------- mapped input files

unsafe extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, len: usize) -> i32;
    fn write(fd: i32, buf: *const std::ffi::c_void, n: usize) -> isize;
}

/// A whole regular file mapped read-only into memory (no copy; pages are read in by
/// whichever core touches them first).
pub struct Mapped {
    ptr: *mut std::ffi::c_void,
    len: usize,
}

// SAFETY: a read-only private mapping; nothing writes through it
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Mapped {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn open(f: &fs::File, len: u64) -> Option<Mapped> {
        use std::os::fd::AsRawFd;
        if len == 0 || len > isize::MAX as u64 {
            return None;
        }
        const PROT_READ: i32 = 1;
        const MAP_PRIVATE: i32 = 2;
        // SAFETY: maps `len` bytes of an open file read-only; checked for failure below
        let p = unsafe { mmap(std::ptr::null_mut(), len as usize, PROT_READ, MAP_PRIVATE, f.as_raw_fd(), 0) };
        if p as isize == -1 || p.is_null() {
            return None;
        }
        Some(Mapped { ptr: p, len: len as usize })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping covers exactly `len` readable bytes while self lives
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what open() mapped
        unsafe {
            munmap(self.ptr, self.len);
        }
    }
}

// ---------------------------------------------------------------- writing

/// Writes an archive: header and meta record, then segments, then the end record.
pub struct ArchiveWriter<W: Write> {
    w: W,
    crcs: Crc,
    segments: u64,
    total: u64,
    pub written: u64,
}

impl<W: Write> ArchiveWriter<W> {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn new(mut w: W, flags: u8, meta: &[u8]) -> io::Result<ArchiveWriter<W>> {
        if meta.len() as u64 > READ_MAX {
            // a reader would refuse it
            return Err(io::Error::other("the folder table is too big for one archive"));
        }
        let mut h = Vec::with_capacity(16 + meta.len());
        h.extend_from_slice(MAGIC);
        h.push(VERSION);
        h.push(flags);
        put_varint(&mut h, meta.len() as u64);
        h.extend_from_slice(meta);
        let c = crc32(&h);
        h.extend_from_slice(&c.to_le_bytes());
        w.write_all(&h)?;
        let mut crcs = Crc::new();
        crcs.update(&c.to_le_bytes());
        Ok(ArchiveWriter { w, crcs, segments: 0, total: 0, written: h.len() as u64 })
    }

    /// One segment: `raw` original bytes, packed into `payload` by the engine.
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn segment(&mut self, raw: u64, payload: &[u8]) -> io::Result<()> {
        if !segment_fits(raw, payload.len() as u64) {
            // never write what a reader would refuse (the engine promises smaller payloads)
            return Err(io::Error::other("internal error: a packed segment is outside the archive format's limits"));
        }
        let mut h = Vec::with_capacity(24);
        put_varint(&mut h, raw);
        put_varint(&mut h, payload.len() as u64);
        let mut c = Crc::new();
        c.update(&h);
        c.update(payload);
        h.extend_from_slice(&c.value().to_le_bytes());
        self.w.write_all(&h)?;
        self.w.write_all(payload)?;
        self.crcs.update(&c.value().to_le_bytes());
        self.segments += 1;
        self.total += raw;
        self.written += (h.len() + payload.len()) as u64;
        Ok(())
    }

    #[cfg_attr(fewer_small, optimize(size))]
    pub fn finish(mut self) -> io::Result<(W, u64)> {
        let mut e = Vec::with_capacity(32);
        put_varint(&mut e, 0);
        put_varint(&mut e, self.segments);
        put_varint(&mut e, self.total);
        e.extend_from_slice(&self.crcs.value().to_le_bytes());
        self.w.write_all(&e)?;
        self.w.flush()?;
        self.written += e.len() as u64;
        Ok((self.w, self.written))
    }
}

/// Biggest segment for an input of `size` bytes (when known): segments of about equal size.
#[cfg_attr(fewer_small, optimize(size))]
fn segment_max(size: Option<u64>) -> usize {
    match size {
        Some(n) if n > SEGMENT_MAX as u64 => {
            let k = n.div_ceil(SEGMENT_MAX as u64 - (2 << 20));
            (n.div_ceil(k) as usize + (1 << 20)).min(SEGMENT_MAX)
        }
        _ => SEGMENT_MAX,
    }
}

/// Pack bytes already in memory (a mapped file): segments cut after a line end, two
/// packed at the same time (so no core waits at the end of a segment).
pub fn pack_bytes<W: Write>(data: &[u8], aw: &mut ArchiveWriter<W>, threads: usize) -> Result<u64, String> {
    let max = segment_max(Some(data.len() as u64));
    let mut segs = Vec::new();
    let mut s = 0;
    while s < data.len() {
        let e = if data.len() - s <= max {
            data.len()
        } else {
            match data[s..s + max].iter().rposition(|&c| c == b'\n') {
                Some(p) => s + p + 1,
                None => s + max,
            }
        };
        segs.push((s, e));
        s = e;
    }
    std::thread::scope(|sc| {
        let mut busy = std::collections::VecDeque::new();
        for &(a, b) in &segs {
            if busy.len() == if threads > 1 { 2 } else { 1 } {
                let (n, h): (usize, std::thread::ScopedJoinHandle<Vec<u8>>) = busy.pop_front().unwrap();
                aw.segment(n as u64, &h.join().unwrap()).map_err(|e| format!("cannot write output: {e}"))?;
            }
            busy.push_back((b - a, sc.spawn(move || engine::pack(&data[a..b], threads))));
        }
        while let Some((n, h)) = busy.pop_front() {
            aw.segment(n as u64, &h.join().unwrap()).map_err(|e| format!("cannot write output: {e}"))?;
        }
        Ok::<(), String>(())
    })?;
    Ok(data.len() as u64)
}

/// Pack a whole stream into segments (a folder's files one after the other, or standard
/// input), reading the next segment while the last ones are packed: two at the same time
/// with several threads, as pack_bytes, so no core waits at the end of a segment. Segments
/// are written in order; a read error is reported once the segments before it are written.
/// Returns the bytes read.
#[cfg_attr(fewer_small, optimize(size))]
pub fn pack_stream<R: Read + Send, W: Write>(
    r: R,
    size: Option<u64>,
    aw: &mut ArchiveWriter<W>,
    threads: usize,
) -> Result<u64, String> {
    let mut seg = Segmenter::new(r, size);
    let mut total = 0u64;
    // the reader hands each segment over only once the one before it is taken (sync_channel(0))
    let ch = Chan::new(0);
    std::thread::scope(|s| {
        let (tx, rx) = (Tx(&ch), Rx(&ch));
        s.spawn(move || read_segments(&mut seg, tx));
        let most = if threads > 1 { 2 } else { 1 };
        let mut busy: std::collections::VecDeque<(u64, std::thread::ScopedJoinHandle<Vec<u8>>)> = std::collections::VecDeque::new();
        let mut write_first = |busy: &mut std::collections::VecDeque<(u64, std::thread::ScopedJoinHandle<Vec<u8>>)>| {
            let (n, h) = busy.pop_front().unwrap();
            aw.segment(n, &h.join().unwrap()).map_err(|e| format!("cannot write output: {e}"))
        };
        for item in rx {
            let data = match item {
                Ok((d, _, _)) => d,
                Err(e) => {
                    while !busy.is_empty() {
                        write_first(&mut busy)?;
                    }
                    return Err(e);
                }
            };
            if busy.len() == most {
                write_first(&mut busy)?;
            }
            total += data.len() as u64;
            busy.push_back((data.len() as u64, s.spawn(move || engine::pack(&data, threads))));
        }
        while !busy.is_empty() {
            write_first(&mut busy)?;
        }
        Ok(total)
    })
}

/// pack_stream's reader thread: every segment of the input, then the end of the channel.
#[cfg_attr(fewer_small, optimize(size))]
fn read_segments<R: Read>(seg: &mut Segmenter<R>, tx: Tx<'_>) {
    loop {
        match seg.next_segment() {
            Ok(Some(v)) => {
                if !tx.send(Ok((v, 0, 0))) {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                tx.send(Err(format!("cannot read input: {e}")));
                break;
            }
        }
    }
}

/// What a reader thread hands over: a segment's bytes (when opening: with its raw size and
/// number), or an error.
type Item = Result<(Vec<u8>, u64, u64), String>;

/// The channel from a reader thread (pack_stream, for_each_block) to the thread that packs or
/// opens what it reads, as std's sync_channel(cap) for cap 0 or 1: at most `cap` items wait in
/// it, and with cap 0 a send returns only once its item is taken. When the receiving end is
/// dropped (the receiver stopped early: an error, or cat or grep done) every send fails, so
/// the reader stops; when the sending end is dropped the receiver's loop ends after the item
/// left in it. (std's channel is about 10 KB of code for its three kinds of channel.)
struct Chan {
    st: Mutex<ChanState>,
    cv: Condvar,
    rendezvous: bool,
}

struct ChanState {
    item: Option<Item>,
    /// items received so far
    taken: u64,
    sending: bool,
    receiving: bool,
}

impl Chan {
    fn new(cap: usize) -> Chan {
        let st = ChanState { item: None, taken: 0, sending: true, receiving: true };
        Chan { st: Mutex::new(st), cv: Condvar::new(), rendezvous: cap == 0 }
    }
    fn lock(&self) -> MutexGuard<'_, ChanState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn wait<'a>(&self, g: MutexGuard<'a, ChanState>) -> MutexGuard<'a, ChanState> {
        self.cv.wait(g).unwrap_or_else(|e| e.into_inner())
    }
}

/// The sending end of a Chan (the reader thread's).
struct Tx<'a>(&'a Chan);

impl Tx<'_> {
    /// Hand `v` over; false when the receiver is gone (then the reader stops).
    fn send(&self, v: Item) -> bool {
        let c = self.0;
        let mut g = c.lock();
        while g.item.is_some() && g.receiving {
            g = c.wait(g);
        }
        if !g.receiving {
            return false;
        }
        g.item = Some(v);
        let n = g.taken + 1;
        c.cv.notify_all();
        while c.rendezvous && g.taken < n && g.receiving {
            g = c.wait(g);
        }
        !c.rendezvous || g.taken >= n
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        self.0.lock().sending = false;
        self.0.cv.notify_all();
    }
}

/// The receiving end of a Chan: the items, until the sending end is dropped.
struct Rx<'a>(&'a Chan);

impl Iterator for Rx<'_> {
    type Item = Item;
    fn next(&mut self) -> Option<Item> {
        let c = self.0;
        let mut g = c.lock();
        loop {
            if let Some(v) = g.item.take() {
                g.taken += 1;
                c.cv.notify_all();
                return Some(v);
            }
            if !g.sending {
                return None;
            }
            g = c.wait(g);
        }
    }
}

impl Drop for Rx<'_> {
    fn drop(&mut self) {
        let left = {
            let mut g = self.0.lock();
            g.receiving = false;
            g.item.take()
        };
        self.0.cv.notify_all();
        drop(left);
    }
}

// ---------------------------------------------------------------- reading

/// Reads an archive record by record, checking every CRC before handing anything out.
pub struct ArchiveReader<R: Read> {
    r: R,
    pub flags: u8,
    pub meta: Vec<u8>,
    crcs: Crc,
    segments: u64,
    total: u64,
    done: bool,
}

#[cfg_attr(fewer_small, optimize(size))]
fn read_varint_io(r: &mut impl Read) -> Result<Option<u64>, String> {
    let mut v = 0u64;
    let mut shift = 0;
    let mut first = true;
    loop {
        let mut b = [0u8; 1];
        match r.read(&mut b) {
            Ok(0) => {
                return if first { Ok(None) } else { Err("damaged archive (cut short)".into()) };
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read: {e}")),
        }
        first = false;
        if shift > 63 {
            return Err("damaged archive (bad number)".into());
        }
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] < 0x80 {
            return Ok(Some(v));
        }
        shift += 7;
    }
}

#[cfg_attr(fewer_small, optimize(size))]
fn need_varint(r: &mut impl Read) -> Result<u64, String> {
    read_varint_io(r)?.ok_or_else(|| "damaged archive (cut short)".to_string())
}

#[cfg_attr(fewer_small, optimize(size))]
fn read_exact_or(r: &mut impl Read, n: u64) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    let got = r.take(n).read_to_end(&mut v).map_err(|e| format!("cannot read: {e}"))?;
    if got as u64 != n {
        return Err("damaged archive (cut short)".into());
    }
    Ok(v)
}

#[cfg_attr(fewer_small, optimize(size))]
fn read_crc(r: &mut impl Read) -> Result<u32, String> {
    let c = read_exact_or(r, 4)?;
    Ok(u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
}

/// What the next 6 bytes of a stream are: Ok(None) at the very end of the input.
#[cfg_attr(fewer_small, optimize(size))]
fn read_header(r: &mut impl Read) -> Result<Option<[u8; 6]>, String> {
    let mut h = [0u8; 6];
    let mut got = 0;
    while got < 6 {
        match r.read(&mut h[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read: {e}")),
        }
    }
    if got == 0 {
        return Ok(None);
    }
    if got < 4 || &h[..4] != MAGIC {
        return Err("not a fewer-pack archive".into());
    }
    if got < 6 {
        return Err("damaged archive (cut short)".into());
    }
    if h[4] != VERSION {
        return Err(format!("archive format version {} is not supported by this fewer-pack (it reads version {VERSION})", h[4]));
    }
    Ok(Some(h))
}

impl<R: Read> ArchiveReader<R> {
    /// Read the header and the meta record.
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn open(mut r: R) -> Result<ArchiveReader<R>, String> {
        let h = read_header(&mut r)?.ok_or("empty input (not a fewer-pack archive)")?;
        Self::after_header(r, h)
    }

    #[cfg_attr(fewer_small, optimize(size))]
    fn after_header(mut r: R, h: [u8; 6]) -> Result<ArchiveReader<R>, String> {
        if h[5] & !FLAG_FOLDER != 0 {
            return Err("archive uses features this fewer-pack does not know (newer version?)".into());
        }
        let n = need_varint(&mut r)?;
        if n > READ_MAX {
            return Err("damaged archive (bad header)".into());
        }
        let meta = read_exact_or(&mut r, n)?;
        let c = read_crc(&mut r)?;
        let mut whole = Vec::with_capacity(16 + meta.len());
        whole.extend_from_slice(&h);
        put_varint(&mut whole, n);
        whole.extend_from_slice(&meta);
        if crc32(&whole) != c {
            return Err("damaged archive (header checksum mismatch)".into());
        }
        let mut crcs = Crc::new();
        crcs.update(&c.to_le_bytes());
        Ok(ArchiveReader { r, flags: h[5], meta, crcs, segments: 0, total: 0, done: false })
    }

    pub fn is_folder(&self) -> bool {
        self.flags & FLAG_FOLDER != 0
    }

    /// The next segment's header: Some(raw bytes) or None after a good end record.
    /// Then call `payload` or `skip` (exactly one of them).
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn next_header(&mut self) -> Result<Option<(u64, u64, u32)>, String> {
        if self.done {
            return Ok(None);
        }
        let raw = need_varint(&mut self.r)?;
        if raw == 0 {
            let segs = need_varint(&mut self.r)?;
            let total = need_varint(&mut self.r)?;
            let c = read_crc(&mut self.r)?;
            if segs != self.segments || total != self.total || c != self.crcs.value() {
                return Err("damaged archive (end record does not match the segments)".into());
            }
            self.done = true;
            return Ok(None);
        }
        let plen = need_varint(&mut self.r)?;
        let crc = read_crc(&mut self.r)?;
        if !segment_fits(raw, plen) {
            return Err(format!("damaged archive (segment {} has a bad size)", self.segments + 1));
        }
        Ok(Some((raw, plen, crc)))
    }

    /// The packed bytes of the segment whose header was just read (checked against its CRC).
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn payload(&mut self, hdr: (u64, u64, u32)) -> Result<Vec<u8>, String> {
        let (raw, plen, crc) = hdr;
        let p = read_exact_or(&mut self.r, plen)?;
        let mut h = Vec::with_capacity(20);
        put_varint(&mut h, raw);
        put_varint(&mut h, plen);
        let mut c = Crc::new();
        c.update(&h);
        c.update(&p);
        if c.value() != crc {
            return Err(format!("damaged archive (segment {} does not match its checksum)", self.segments + 1));
        }
        self.crcs.update(&crc.to_le_bytes());
        self.segments += 1;
        self.total += raw;
        Ok(p)
    }

    /// Skip the segment whose header was just read (its bytes are read but not decoded).
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn skip(&mut self, hdr: (u64, u64, u32)) -> Result<(), String> {
        let (raw, plen, crc) = hdr;
        let got = io::copy(&mut (&mut self.r).take(plen), &mut io::sink()).map_err(|e| format!("cannot read: {e}"))?;
        if got != plen {
            return Err("damaged archive (cut short)".into());
        }
        self.crcs.update(&crc.to_le_bytes());
        self.segments += 1;
        self.total += raw;
        Ok(())
    }

    /// skip, the bytes not even read when the input is a file (search_archive with an index).
    pub fn pass(&mut self, hdr: (u64, u64, u32)) -> Result<(), String> where R: Input {
        self.r.skip_n(hdr.1).map_err(|_| "damaged archive (cut short)".to_string())?;
        self.crcs.update(&hdr.2.to_le_bytes());
        self.segments += 1;
        self.total += hdr.0;
        Ok(())
    }

    /// After the end record: the next archive joined to this one (`cat a.cmf b.cmf`), or
    /// None at the end of the input.
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn next_member(self) -> Result<Option<ArchiveReader<R>>, String> {
        let mut r = self.r;
        match read_header(&mut r).map_err(|e| if e == "not a fewer-pack archive" { "unexpected bytes after the end of the archive".to_string() } else { e })? {
            None => Ok(None),
            Some(h) => Ok(Some(Self::after_header(r, h)?)),
        }
    }
}

/// What an archive is read from, under its buffer (main.rs `archive_in`): a file or standard
/// input, boxed so that both are one type to the buffer and the archive reader. `fwd` passes
/// over n bytes: a file seeks, anything else reads them.
pub trait Src: Read + Send {
    fn fwd(&mut self, n: u64) -> io::Result<()> {
        let k = io::copy(&mut (&mut *self).take(n), &mut io::sink())?;
        if k == n { Ok(()) } else { Err(io::ErrorKind::UnexpectedEof.into()) }
    }
}
impl Src for fs::File {
    fn fwd(&mut self, n: u64) -> io::Result<()> {
        io::Seek::seek(self, io::SeekFrom::Current(n as i64)).map(|_| ())
    }
}
impl Src for io::Stdin {}

/// An archive's input (ArchiveReader::pass): `skip_n` passes over n bytes, first those in the
/// buffer, then the rest in the source (a file is skipped through by seeking).
pub trait Input: Read + Send {
    fn skip_n(&mut self, n: u64) -> io::Result<()>;
}
impl Input for io::BufReader<Box<dyn Src>> {
    fn skip_n(&mut self, n: u64) -> io::Result<()> {
        let have = self.buffer().len();
        if n <= have as u64 {
            io::BufRead::consume(self, n as usize);
            return Ok(());
        }
        io::BufRead::consume(self, have);
        self.get_mut().fwd(n - have as u64)
    }
}
impl Input for Box<dyn Input> {
    fn skip_n(&mut self, n: u64) -> io::Result<()> {
        (**self).skip_n(n)
    }
}

/// Every segment of an archive (and of file archives joined after it), unpacked and
/// checked, handed to `f` in order, a block at a time (a segment's blocks are opened on all
/// cores into buffers used again for later blocks; see engine::unpack_to). The next segment
/// is read while one is opened. `f` returns false to stop early. Every block is checked (size
/// and CRC) before `f` sees it; a segment that does not unpack to its size is an error once
/// it ends.
#[cfg_attr(fewer_small, optimize(size))]
pub fn for_each_segment<R: Read + Send>(
    ar: ArchiveReader<R>,
    threads: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<bool, String>,
) -> Result<(), String> {
    for_each_block(ar, threads, None, &mut |b, _| f(b.unwrap_or_default()))
}

/// for_each_segment, and with a search's filter (engine::unpack_blocks): a block of lines
/// that cannot hold the text, between line ends, is handed to `f` as None and its size.
/// (`f` is not generic: one copy of this code for all its callers.) `fewer-pack grep` uses
/// search_archive, which opens and searches the blocks of all segments from one queue.
#[cfg_attr(fewer_small, optimize(size))]
pub fn for_each_block<R: Read + Send>(
    ar: ArchiveReader<R>,
    threads: usize,
    may: Option<engine::Filter>,
    f: &mut crate::engine::BlockSink<'_>,
) -> Result<(), String> {
    // whether the data so far ends with a line end (or nothing came yet)
    let mut last_nl = true;
    // the reader reads the next segment while one waits (sync_channel(1))
    let ch = Chan::new(1);
    std::thread::scope(|s| {
        let (tx, rx) = (Tx(&ch), Rx(&ch));
        s.spawn(move || read_payloads(ar, tx));
        for item in rx {
            let (p, raw, n) = item?;
            let (mut got, mut go) = (0u64, true);
            let mut ferr = None;
            let prev = last_nl;
            let r = engine::unpack_blocks(&p, threads, may, prev, &mut |b, size| {
                got += size as u64;
                last_nl = b.is_none_or(|t| t.last() == Some(&b'\n'));
                match f(b, size) {
                    Ok(g) => {
                        go = g;
                        Ok(g)
                    }
                    Err(e) => {
                        ferr = Some(e);
                        Err(String::new())
                    }
                }
            });
            if let Some(e) = ferr {
                return Err(e);
            }
            let total = r.map_err(|e| if e.starts_with("made by ") { e } else { format!("damaged archive (segment {n}: {e})") })?;
            if !go {
                break;
            }
            if total != raw || got != raw {
                return Err(format!("damaged archive (segment {n} unpacked to the wrong size)"));
            }
        }
        Ok(())
    })
}

/// A run of whole lines of a block's text to search on the pool (search_archive): its start
/// and end in the block, and the file it belongs to (folder archives; else 0).
pub type Run = (usize, usize, usize);

/// What a search's plan of a block gives (search_archive), written in this order: lines it
/// searched itself (their output and count), or runs of whole lines for the pool.
pub enum Ent {
    Out(Vec<u8>, usize),
    Runs(Vec<Run>),
}

/// A block of the archive in the search's queue (search_archive).
enum BlockSt {
    /// not opened yet
    Wait,
    /// being opened by a worker
    Busy,
    /// opened: its text
    Text(Arc<engine::BlockBuf>),
    /// left unopened by the filter
    Skip,
    /// the archive (or this block) is damaged here: the error, at its place in the order
    Fail(String),
}

struct Slot {
    seg: Option<Arc<engine::Seg>>,
    /// the block in its segment, its size, the segment's number (for errors)
    i: usize,
    raw: usize,
    n: u64,
    st: BlockSt,
}

struct Shared {
    /// the blocks not yet written out, in order: slots[0] is block number `base`
    slots: VecDeque<Slot>,
    base: usize,
    /// the next block to open
    next: usize,
    /// blocks being opened or holding their text (at most `window`: memory)
    inflight: usize,
    reader_done: bool,
    stop: bool,
    /// searches waiting for a thread: (job number, the block's text and size, its runs)
    jobs: VecDeque<(usize, Arc<engine::BlockBuf>, usize, Vec<Run>)>,
    /// the results of jobs `res_base`, `res_base + 1`, ... (None: not done yet)
    results: VecDeque<Option<(Vec<u8>, usize)>>,
    res_base: usize,
    /// bytes of output found and not yet written (at most about OUT_MAX: memory)
    pending: usize,
    pool: Vec<engine::BlockBuf>,
    /// the buffers derived fields are rewritten into, needed only while a block opens
    scratch: Vec<Vec<u8>>,
}

/// While this much output waits to be written, no further block is opened (search_archive).
const OUT_MAX: usize = 64 << 20;

fn seg_err(n: u64, e: String) -> String {
    if e.starts_with("made by ") { e } else { format!("damaged archive (segment {n}: {e})") }
}

/// search_archive's search of runs of an opened block's lines: (the block's text, the runs,
/// the output) -> the lines found.
pub type SearchRuns<'a> = &'a (dyn Fn(&[u8], &[Run], &mut Vec<u8>) -> usize + Sync);
/// search_archive's plan of the next block in order (its text, or None when it was skipped),
/// its size and the entries it makes.
pub type PlanBlock<'a> = &'a mut dyn FnMut(Option<&[u8]>, usize, &mut Vec<Ent>) -> Result<(), String>;
/// search_archive's writer of an entry's output, in order.
pub type WriteOut<'a> = &'a mut dyn FnMut(&[u8], usize) -> Result<(), String>;
/// search_archive's index (index::Index::skips): for segment g (from 0, over joined
/// archives) with this header, each block's size and whether it is left unopened; None:
/// not indexed as it is (the index is not asked again).
pub type IndexSkip<'a> = &'a (dyn Fn(usize, (u64, u64, u32)) -> Option<Vec<(usize, bool)>> + Sync);

/// One step of a search thread (search_archive): a waiting search of runs, else the next
/// block (opened with the filter); false when there is nothing to do now.
fn search_step<'a>(
    sh: &'a Mutex<Shared>,
    mut g: MutexGuard<'a, Shared>,
    window: usize,
    threads: usize,
    may: engine::Filter,
    search: SearchRuns<'_>,
) -> (MutexGuard<'a, Shared>, bool) {
    if let Some((id, buf, raw, runs)) = g.jobs.pop_front() {
        drop(g);
        let mut o = Vec::with_capacity(runs.iter().map(|r| r.1 - r.0).sum::<usize>().min(4 << 20));
        let n = search(buf.text(raw), &runs, &mut o);
        drop(buf);
        let mut g = sh.lock().unwrap();
        let k = id - g.res_base;
        g.pending += o.len();
        g.results[k] = Some((o, n));
        return (g, true);
    }
    // past the blocks the index left unopened (or this thread planned meanwhile)
    g.next = g.next.max(g.base);
    while g.slots.get(g.next - g.base).is_some_and(|s| matches!(s.st, BlockSt::Skip | BlockSt::Text(_))) {
        g.next += 1;
    }
    let k = g.next - g.base;
    if g.inflight >= window || g.pending > OUT_MAX || k >= g.slots.len() || !matches!(g.slots[k].st, BlockSt::Wait) {
        return (g, false);
    }
    let at = g.next;
    g.slots[k].st = BlockSt::Busy;
    g.next += 1;
    g.inflight += 1;
    // the threads a block's streams are decoded on: all of them for a lone block
    let waiting = g.slots.iter().skip(k).filter(|s| matches!(s.st, BlockSt::Wait)).count() + 1;
    let per = (threads / waiting).max(1);
    let mut buf = g.pool.pop().unwrap_or_default();
    buf.put_scratch(g.scratch.pop().unwrap_or_default());
    let (seg, i, n) = (g.slots[k].seg.clone().expect("a block to open has its segment"), g.slots[k].i, g.slots[k].n);
    drop(g);
    let r = seg.open(i, &mut buf, per, Some(may));
    drop(seg);
    let mut g = sh.lock().unwrap();
    let s = buf.take_scratch();
    g.scratch.push(s);
    let k = at - g.base;
    g.slots[k].st = match r {
        Ok(false) => BlockSt::Text(Arc::new(buf)),
        Ok(true) => {
            g.inflight -= 1;
            g.pool.push(buf);
            BlockSt::Skip
        }
        Err(e) => {
            g.inflight -= 1;
            g.pool.push(buf);
            BlockSt::Fail(seg_err(n, e))
        }
    };
    (g, true)
}

/// An entry of the output, in order (search_archive).
enum Wr {
    Out(Vec<u8>, usize),
    /// the next job's output (jobs are written in their order)
    Job,
    /// the block's entries are all written: its slot and text are let go
    Release,
    Fail(String),
}

/// Search an archive (and file archives joined after it) on all cores, its output in order.
///
/// One queue holds every block of every segment, read ahead by a reader thread (each
/// segment's CRC, header and tables are read there), so no thread waits for a segment to
/// end. `threads` threads take, first, a search of runs of lines (a job), else the next
/// block, which they open with the filter `may` first (engine::Seg::open: a block no line of
/// which can hold the text is left unopened, only its patterns, texts and words decoded). At
/// most `threads + 2` blocks are opened and not yet written (memory). This thread hands the
/// blocks out in order to `plan`, which keeps what runs from block to block (a line cut by a
/// block's end, the file places of a folder archive) and cuts each opened block into
/// entries: output it made itself and runs of whole lines, which go to the queue as jobs of
/// about 1 MiB, so a big block is searched on all cores too, while the next blocks open.
/// `write` gets the entries in order: the output is the same as on one thread. A skipped
/// block after text that does not end with '\n' is opened here after all (as
/// engine::unpack_blocks does). An error comes out at its place in the order, after all
/// before it was written, with the messages of for_each_block. With an index (`idx`), the
/// blocks it rules out are left unopened from the start, and a segment of only such blocks
/// is not decoded or checked at all (its header is: the index must have this very segment).
#[allow(clippy::too_many_arguments)]
pub fn search_archive<R: Input>(
    ar: ArchiveReader<R>,
    threads: usize,
    may: engine::Filter,
    idx: Option<IndexSkip<'_>>,
    plan: PlanBlock<'_>,
    search: SearchRuns<'_>,
    write: WriteOut<'_>,
) -> Result<(), String> {
    let threads = threads.max(1);
    let window = threads + 2;
    let sh = Mutex::new(Shared {
        slots: VecDeque::new(),
        base: 0,
        next: 0,
        inflight: 0,
        reader_done: false,
        stop: false,
        jobs: VecDeque::new(),
        results: VecDeque::new(),
        res_base: 0,
        pending: 0,
        pool: Vec::new(),
        scratch: Vec::new(),
    });
    // `work`: the workers and the reader wait on it (jobs, blocks, room); `main`: this thread
    let (work, main) = (Condvar::new(), Condvar::new());
    let (sh, work, main) = (&sh, &work, &main);
    std::thread::scope(|s| {
        // the reader: the segments in order, each read (its CRC checked), its header and
        // tables read, its blocks put in the queue; a few segments ahead at most
        s.spawn(move || {
            let fail = |e: String| {
                let mut g = sh.lock().unwrap();
                g.slots.push_back(Slot { seg: None, i: 0, raw: 0, n: 0, st: BlockSt::Fail(e) });
                g.reader_done = true;
                main.notify_one();
            };
            let (mut ar, mut idx, mut gn) = (Some(ar), idx, 0);
            while let Some(mut a) = ar.take() {
                loop {
                    {
                        let mut g = sh.lock().unwrap();
                        while !g.stop && g.slots.len() > 2 * window {
                            g = work.wait(g).unwrap();
                        }
                        if g.stop {
                            return;
                        }
                    }
                    let h = match a.next_header() {
                        Ok(Some(h)) => h,
                        Ok(None) => break,
                        Err(e) => return fail(e),
                    };
                    let sk = idx.and_then(|f| f(gn, h));
                    if sk.is_none() {
                        idx = None;
                    }
                    gn += 1;
                    if let Some(v) = sk.as_ref().filter(|v| v.iter().all(|b| b.1) && v.iter().map(|b| b.0 as u64).sum::<u64>() == h.0) {
                        if let Err(e) = a.pass(h) {
                            return fail(e);
                        }
                        let mut g = sh.lock().unwrap();
                        for &(raw, _) in v {
                            g.slots.push_back(Slot { seg: None, i: 0, raw, n: a.segments, st: BlockSt::Skip });
                        }
                        main.notify_one();
                        continue;
                    }
                    let p = match a.payload(h) {
                        Ok(p) => p,
                        Err(e) => return fail(e),
                    };
                    let n = a.segments;
                    let seg = match engine::Seg::new(p, threads) {
                        Ok(seg) => Arc::new(seg),
                        Err(e) => return fail(seg_err(n, e)),
                    };
                    let sk = sk.filter(|v| v.len() == seg.blocks());
                    let mut g = sh.lock().unwrap();
                    for i in 0..seg.blocks() {
                        let raw = seg.block_raw(i);
                        let st = if sk.as_ref().is_some_and(|v| v[i] == (raw, true)) { BlockSt::Skip } else { BlockSt::Wait };
                        g.slots.push_back(Slot { seg: Some(seg.clone()), i, raw, n, st });
                    }
                    if seg.raw() != h.0 {
                        g.slots.push_back(Slot { seg: None, i: 0, raw: 0, n, st: BlockSt::Fail(format!("damaged archive (segment {n} unpacked to the wrong size)")) });
                        g.reader_done = true;
                        work.notify_all();
                        main.notify_one();
                        return;
                    }
                    work.notify_all();
                    main.notify_one();
                }
                let folder = a.is_folder();
                match a.next_member() {
                    Ok(Some(n)) if !folder && !n.is_folder() => ar = Some(n),
                    Ok(Some(_)) => return fail("unexpected data after the end of the archive".into()),
                    Ok(None) => break,
                    Err(e) => return fail(e),
                }
            }
            let mut g = sh.lock().unwrap();
            g.reader_done = true;
            main.notify_one();
        });
        // the workers (with one thread, this thread does their work when it waits)
        let nworkers = if threads > 1 { threads } else { 0 };
        for _ in 0..nworkers {
            s.spawn(move || {
                let mut g = sh.lock().unwrap();
                while !g.stop {
                    let (g2, did) = search_step(sh, g, window, threads, may, search);
                    g = g2;
                    if did {
                        main.notify_one();
                    } else {
                        g = work.wait(g).unwrap();
                    }
                }
            });
        }
        let r = drive(sh, work, main, threads, window, may, plan, search, write);
        sh.lock().unwrap().stop = true;
        work.notify_all();
        r
    })
}

/// search_archive's own thread: plans the blocks in order and writes the entries in order.
#[allow(clippy::too_many_arguments)]
fn drive(
    sh: &Mutex<Shared>,
    work: &Condvar,
    main: &Condvar,
    threads: usize,
    window: usize,
    may: engine::Filter,
    plan: PlanBlock<'_>,
    search: SearchRuns<'_>,
    write: WriteOut<'_>,
) -> Result<(), String> {
    let mut q: VecDeque<Wr> = VecDeque::new();
    // the next block to plan; whether the text so far ends with '\n'; an error was planned
    let mut planned = 0usize;
    let mut last_nl = true;
    let mut failed = false;
    let mut ents: Vec<Ent> = Vec::new();
    let mut g = sh.lock().unwrap();
    loop {
        // 1. the entries that are ready, in order
        let mut wrote = false;
        loop {
            match q.front() {
                Some(Wr::Job) => {
                    let Some(r) = g.results.front_mut().and_then(|r| r.take()) else { break };
                    g.results.pop_front();
                    g.res_base += 1;
                    q.pop_front();
                    drop(g);
                    write(&r.0, r.1)?;
                    g = sh.lock().unwrap();
                    g.pending -= r.0.len();
                    if g.pending + r.0.len() > OUT_MAX {
                        work.notify_all();
                    }
                }
                Some(Wr::Out(..)) => {
                    let Some(Wr::Out(o, n)) = q.pop_front() else { unreachable!() };
                    drop(g);
                    write(&o, n)?;
                    g = sh.lock().unwrap();
                }
                Some(Wr::Release) => {
                    q.pop_front();
                    let slot = g.slots.pop_front().expect("a planned block");
                    g.base += 1;
                    if let BlockSt::Text(b) = slot.st {
                        g.inflight -= 1;
                        if let Ok(b) = Arc::try_unwrap(b) {
                            g.pool.push(b);
                        }
                    }
                    work.notify_all();
                }
                Some(Wr::Fail(_)) => {
                    let Some(Wr::Fail(e)) = q.pop_front() else { unreachable!() };
                    return Err(e);
                }
                None => break,
            }
            wrote = true;
        }
        // 2. the next block, when it is ready
        let k = planned - g.base;
        let mut planned_one = false;
        if !failed && k < g.slots.len() {
            let raw = g.slots[k].raw;
            let mut text = None;
            let mut fail = None;
            match &g.slots[k].st {
                BlockSt::Wait | BlockSt::Busy => {}
                BlockSt::Text(b) => text = Some(b.clone()),
                BlockSt::Fail(e) => fail = Some(e.clone()),
                BlockSt::Skip if !last_nl => {
                    // the line before goes on into it: open it after all
                    let (seg, i, n) = (g.slots[k].seg.clone().expect("a skipped block has its segment"), g.slots[k].i, g.slots[k].n);
                    drop(g);
                    let mut buf = engine::BlockBuf::default();
                    let r = seg.open(i, &mut buf, threads, None);
                    g = sh.lock().unwrap();
                    let k = planned - g.base;
                    g.slots[k].st = match r {
                        Ok(_) => {
                            g.inflight += 1;
                            BlockSt::Text(Arc::new(buf))
                        }
                        Err(e) => BlockSt::Fail(seg_err(n, e)),
                    };
                    continue;
                }
                BlockSt::Skip => {}
            }
            let ready = !matches!(g.slots[k].st, BlockSt::Wait | BlockSt::Busy);
            if let Some(e) = fail {
                q.push_back(Wr::Fail(e));
                failed = true;
                continue;
            }
            if ready {
                drop(g);
                ents.clear();
                let r = match &text {
                    Some(b) => {
                        last_nl = b.text(raw)[raw - 1] == b'\n';
                        plan(Some(b.text(raw)), raw, &mut ents)
                    }
                    None => plan(None, raw, &mut ents),
                };
                g = sh.lock().unwrap();
                for e in ents.drain(..) {
                    match e {
                        Ent::Out(o, n) => q.push_back(Wr::Out(o, n)),
                        Ent::Runs(runs) => {
                            let id = g.res_base + g.results.len();
                            g.results.push_back(None);
                            g.jobs.push_back((id, text.clone().expect("runs of an opened block"), raw, runs));
                            q.push_back(Wr::Job);
                        }
                    }
                }
                match r {
                    Ok(()) => {
                        q.push_back(Wr::Release);
                        planned += 1;
                    }
                    Err(e) => {
                        q.push_back(Wr::Fail(e));
                        failed = true;
                    }
                }
                work.notify_all();
                planned_one = true;
            }
        } else if !failed && g.reader_done && q.is_empty() {
            return Ok(());
        }
        if wrote || planned_one {
            continue;
        }
        // 3. nothing to do here now: with one thread, do a worker's step; else wait
        if threads == 1 {
            let (g2, did) = search_step(sh, g, window, threads, may, search);
            g = g2;
            if did {
                continue;
            }
        }
        g = main.wait(g).unwrap();
    }
}

/// for_each_block's reader thread: every segment's payload (with its size and number), of this
/// archive and the ones joined to it, then the end of the channel (or an error).
#[cfg_attr(fewer_small, optimize(size))]
fn read_payloads<R: Read>(ar: ArchiveReader<R>, tx: Tx<'_>) {
    let mut ar = Some(ar);
    while let Some(mut a) = ar.take() {
        loop {
            let h = match a.next_header() {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(e) => {
                    tx.send(Err(e));
                    return;
                }
            };
            let item = a.payload(h).map(|p| (p, h.0, a.segments));
            let failed = item.is_err();
            if !tx.send(item) || failed {
                return;
            }
        }
        let folder = a.is_folder();
        match a.next_member() {
            Ok(Some(n)) if !folder && !n.is_folder() => ar = Some(n),
            Ok(Some(_)) => {
                tx.send(Err("unexpected data after the end of the archive".into()));
                return;
            }
            Ok(None) => break,
            Err(e) => {
                tx.send(Err(e));
                return;
            }
        }
    }
}

/// Unpack every segment (and every joined archive) to `out`. Returns the bytes written.
#[cfg_attr(fewer_small, optimize(size))]
pub fn unpack_stream<R: Read + Send, W: Write>(ar: ArchiveReader<R>, out: &mut W, threads: usize) -> Result<u64, String> {
    let mut written = 0u64;
    for_each_segment(ar, threads, &mut |d| {
        out.write_all(d).map_err(|e| format!("cannot write output: {e}"))?;
        written += d.len() as u64;
        Ok(true)
    })?;
    out.flush().map_err(|e| format!("cannot write output: {e}"))?;
    Ok(written)
}

// ---------------------------------------------------------------- atomic output files

/// Temporary files to remove if the program is stopped by a signal (Ctrl-C): C strings,
/// so the SIGBUS handler can unlink them with async-signal-safe calls only. (That handler
/// cannot take the gate below, but it needs none: a mapped input is read only while its
/// output is still registered.) Room for one output of each input started at once
/// (batch::WINDOW) and more.
static TEMP_PATHS: [AtomicPtr<std::ffi::c_char>; 64] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 64];
const _: () = assert!(crate::batch::WINDOW + 8 <= 64);
/// Temporary folders (folder unpacking) to remove on Ctrl-C.
static TEMP_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// Held while a temporary folder gets new entries or is renamed into place, while a
/// temporary file name is dropped from TEMP_PATHS (and freed), and by the signal thread
/// while it cleans up, so they never race.
static GATE: Mutex<()> = Mutex::new(());
/// The process umask (read once at start).
static UMASK: AtomicU32 = AtomicU32::new(0o022);

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
type ModeT = u16;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd")))]
type ModeT = u32;

/// A signal set, big enough for every system (macOS uses 4 bytes, Linux 128).
#[repr(C, align(8))]
struct SigSet([u8; 128]);

unsafe extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
    fn unlink(path: *const std::ffi::c_char) -> i32;
    fn _exit(code: i32) -> !;
    fn umask(mask: ModeT) -> ModeT;
    fn sigemptyset(set: *mut SigSet) -> i32;
    fn sigaddset(set: *mut SigSet, sig: i32) -> i32;
    fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
    fn sigwait(set: *const SigSet, sig: *mut i32) -> i32;
}

#[cfg(target_os = "linux")]
const SIGBUS: i32 = 7;
#[cfg(not(target_os = "linux"))]
const SIGBUS: i32 = 10;
#[cfg(target_os = "linux")]
const SIG_BLOCK: i32 = 0;
#[cfg(not(target_os = "linux"))]
const SIG_BLOCK: i32 = 1;
/// SIGHUP, SIGINT, SIGTERM, SIGXCPU, SIGXFSZ (the same numbers on Linux and macOS)
const STOP_SIGNALS: [i32; 5] = [1, 2, 15, 24, 25];

#[cfg_attr(fewer_small, optimize(size))]
fn unlink_temp_files() {
    for p in &TEMP_PATHS {
        let q = p.load(Ordering::SeqCst);
        if !q.is_null() {
            // SAFETY: q is a C string leaked in register_temp; unlink is async-signal-safe
            unsafe {
                unlink(q);
            }
        }
    }
}

extern "C" fn on_sigbus(_sig: i32) {
    let m = b"fewer-pack: an input file got shorter while it was being read; nothing was written\n";
    // SAFETY: write(2), unlink(2) and _exit(2) are async-signal-safe
    unsafe {
        write(2, m.as_ptr() as *const std::ffi::c_void, m.len());
    }
    unlink_temp_files();
    unsafe { _exit(128 + SIGBUS) }
}

/// Make a folder tree writable and remove it.
#[cfg_attr(fewer_small, optimize(size))]
pub fn remove_tree(p: &Path) {
    #[cfg_attr(fewer_small, optimize(size))]
    fn open_up(p: &Path) {
        use crate::portable::PermissionsExt;
        if let Ok(md) = fs::symlink_metadata(p) {
            if md.is_dir() {
                let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
                if let Ok(rd) = fs::read_dir(p) {
                    for e in rd.flatten() {
                        open_up(&e.path());
                    }
                }
            }
        }
    }
    open_up(p);
    let _ = fs::remove_dir_all(p);
}

/// Ctrl-C and similar: a thread waits for the signal, removes the temporary outputs (files
/// and folders) and exits. SIGBUS (a mapped input file got shorter) removes temporary files.
/// A broken pipe exits quietly (`fewer-pack cat x.cmf | head`). Call this first, before any
/// thread starts (threads inherit the blocked signals).
#[cfg_attr(fewer_small, optimize(size))]
pub fn install_signal_handlers() {
    // SAFETY: plain libc calls on a set we own; the handler only uses async-signal-safe calls
    unsafe {
        let old = umask(0o022);
        umask(old);
        UMASK.store(old as u32 & 0o777, Ordering::SeqCst);
        signal(SIGBUS, on_sigbus as *const () as usize);
        signal(13, 0); // SIGPIPE: default action
        let mut set = SigSet([0; 128]);
        sigemptyset(&mut set);
        for sig in STOP_SIGNALS {
            sigaddset(&mut set, sig);
        }
        if pthread_sigmask(SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return;
        }
        std::thread::spawn(move || signal_thread(set));
    }
}

/// The signal thread: on a stop signal, remove the temporary files and folders, and exit.
#[cfg_attr(fewer_small, optimize(size))]
fn signal_thread(set: SigSet) -> ! {
    loop {
        let mut sig = 0;
        // SAFETY: sigwait on a set we own
        if unsafe { sigwait(&set, &mut sig) } != 0 {
            continue;
        }
        let _g = GATE.lock().unwrap_or_else(|e| e.into_inner());
        unlink_temp_files();
        let dirs = TEMP_DIRS.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for d in dirs {
            remove_tree(&d);
        }
        // SAFETY: _exit(2); nothing runs after it
        unsafe { _exit(128 + sig) };
    }
}

/// The input being worked on, for the message of a panic = "abort" build (see below).
static CURRENT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Whether the program packs (else it opens archives: unpack, list, cat, grep).
static PACKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Name the input being worked on (for the error message of a panic, see install_panic_exit).
#[cfg_attr(fewer_small, optimize(size))]
pub fn set_current(name: &std::ffi::OsStr, packing: bool) {
    use crate::portable::OsStrExt;
    PACKING.store(packing, Ordering::SeqCst);
    if let Ok(mut c) = CURRENT.lock() {
        c.clear();
        c.extend_from_slice(if name == "-" { b"(stdin)" } else { name.as_bytes() });
    }
}

/// A panic = "abort" build (no unwinding, so the engine's opener
/// cannot run behind catch_unwind): a panic, which only damaged input the engine does not
/// check itself (or a bug) can cause, ends the program with a clear error instead of a crash:
/// "fewer-pack: NAME: ..." on standard error, the temporary outputs removed as on Ctrl-C (so no
/// output is left behind), and exit status `code` (1; grep: 2). The inputs after it are not
/// worked on (several inputs at once: not kept; see batch::on_panic). Call this once, at the
/// start.
#[cfg(panic = "abort")]
#[cfg_attr(fewer_small, optimize(size))]
pub fn install_panic_exit(code: i32) {
    std::panic::set_hook(Box::new(move |_| panic_exit(code)));
}

/// The panic hook of install_panic_exit.
#[cfg(panic = "abort")]
#[cfg_attr(fewer_small, optimize(size))]
fn panic_exit(code: i32) -> ! {
    // several inputs at once: the inputs before the one that stopped are completed and
    // reported first, and it is named (batch::on_panic); only the first panic reports
    static ENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let stopped = crate::batch::on_panic();
    if ENDING.swap(true, Ordering::SeqCst) {
        loop {
            std::thread::park();
        }
    }
    // wait (a little) for a Ctrl-C clean-up or a folder's rename in progress, as the
    // signal thread does (held until _exit)
    let _gate = (0..200).find_map(|_| {
        let g = GATE.try_lock().ok();
        if g.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        g
    });
    unlink_temp_files();
    if let Ok(d) = TEMP_DIRS.try_lock() {
        for p in d.iter() {
            remove_tree(p);
        }
    }
    let mut m = b"fewer-pack: ".to_vec();
    if let Some(name) = stopped {
        use crate::portable::OsStrExt;
        if !name.is_empty() {
            m.extend_from_slice(name.as_bytes());
            m.extend_from_slice(b": ");
        }
    } else if let Ok(c) = CURRENT.try_lock() {
        if !c.is_empty() {
            m.extend_from_slice(&c);
            m.extend_from_slice(b": ");
        }
    }
    m.extend_from_slice(if PACKING.load(Ordering::SeqCst) {
        b"internal error while packing; no archive was written\n"
    } else {
        b"damaged archive (the engine stopped on its data)\n"
    });
    // SAFETY: write(2) and _exit(2) on our own buffer; nothing runs after _exit
    unsafe {
        write(2, m.as_ptr() as *const std::ffi::c_void, m.len());
        _exit(code)
    }
}

/// The process umask.
#[cfg_attr(fewer_small, optimize(size))]
pub fn umask_bits() -> u32 {
    UMASK.load(Ordering::SeqCst)
}

/// Hold this while adding entries to a temporary folder, or renaming it into place.
pub fn gate() -> std::sync::MutexGuard<'static, ()> {
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg_attr(fewer_small, optimize(size))]
pub fn register_temp_dir(p: &Path) {
    TEMP_DIRS.lock().unwrap_or_else(|e| e.into_inner()).push(p.to_path_buf());
}

#[cfg_attr(fewer_small, optimize(size))]
pub fn unregister_temp_dir(p: &Path) {
    TEMP_DIRS.lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d != p);
}

/// An output that already has its real name but is still on probation (--rm checks it before
/// deleting the input): until `release`, Ctrl-C or a panic removes it like a temporary file.
pub struct Probation(Option<usize>);

#[cfg_attr(fewer_small, optimize(size))]
pub fn probation(p: &Path) -> Probation {
    Probation(register_temp(p))
}

impl Probation {
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn release(mut self) {
        unregister_temp(self.0.take());
    }
}

#[cfg_attr(fewer_small, optimize(size))]
fn register_temp(p: &Path) -> Option<usize> {
    use crate::portable::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let raw = c.into_raw();
    for (i, slot) in TEMP_PATHS.iter().enumerate() {
        if slot.compare_exchange(std::ptr::null_mut(), raw, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return Some(i);
        }
    }
    // SAFETY: raw came from CString::into_raw just above and was not stored
    drop(unsafe { std::ffi::CString::from_raw(raw) });
    None
}

#[cfg_attr(fewer_small, optimize(size))]
fn unregister_temp(i: Option<usize>) {
    if let Some(i) = i {
        // the signal thread may be unlinking this name right now: never free it under it
        let _g = gate();
        let q = TEMP_PATHS[i].swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !q.is_null() {
            // SAFETY: q came from CString::into_raw in register_temp
            drop(unsafe { std::ffi::CString::from_raw(q) });
        }
    }
}

/// A short temporary name in `dest`'s folder (short, so any legal final name works).
#[cfg_attr(fewer_small, optimize(size))]
pub fn temp_name(dest: &Path, tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let tmp = format!(".fewer-{tag}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(tmp),
        _ => PathBuf::from(tmp),
    }
}

/// Check an output name before anything is written: not empty, not ending in '/', not an
/// existing folder, and (without `force`) not an existing file.
#[cfg_attr(fewer_small, optimize(size))]
pub fn check_output(dest: &Path, force: bool) -> Result<(), String> {
    use crate::portable::OsStrExt;
    let b = dest.as_os_str().as_bytes();
    if b.is_empty() {
        return Err("the output name is empty".into());
    }
    if b.ends_with(b"/") {
        return Err(format!("{}: an output file name cannot end with '/'", dest.display()));
    }
    match fs::symlink_metadata(dest) {
        Ok(md) if md.is_dir() => Err(format!("{} is a folder; use -o to choose another name", dest.display())),
        Ok(_) if !force => Err(format!("{} already exists (use -f to overwrite)", dest.display())),
        _ => Ok(()),
    }
}

/// A file written under a temporary name (private to the owner while it is written) and
/// given its real name only when complete.
pub struct AtomicFile {
    file: Option<io::BufWriter<fs::File>>,
    tmp: PathBuf,
    dest: PathBuf,
    force: bool,
    durable: bool,
    slot: Option<usize>,
}

impl AtomicFile {
    /// Fails if `dest` is a folder, or exists and `force` is off. With `durable`, the file
    /// (and its new name) is forced to disk before `finish` returns: needed when the input
    /// is deleted next (--rm); without it the output is left to the system to write, as
    /// zstd and gzip do (forcing every output to disk made opening to a file 1.3-3 times
    /// slower than zstd -d).
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn create(dest: &Path, force: bool, durable: bool) -> Result<AtomicFile, String> {
        use crate::portable::OpenOptionsExt;
        check_output(dest, force)?;
        let tmp = temp_name(dest, "tmp");
        // created and registered under the gate, as a temporary folder is (folder.rs): a Ctrl-C
        // in between would otherwise leave an empty temporary file behind
        let (f, slot) = {
            let _g = gate();
            let f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
            (f, register_temp(&tmp))
        };
        Ok(AtomicFile { file: Some(io::BufWriter::with_capacity(1 << 20, f)), tmp, dest: dest.to_path_buf(), force, durable, slot })
    }

    /// Flush, set the mode (and time), force to disk (when durable), and only then give the
    /// file its real name; without `force` an output that appeared meanwhile is never
    /// replaced. On any error the temporary file is removed.
    #[cfg_attr(fewer_small, optimize(size))]
    pub fn finish(self, mode: u32, mtime: Option<(i64, u32)>) -> Result<(), String> {
        self.finish_synced(mode, mtime).map(|_| ())
    }

    /// finish, saying whether the file was forced to disk.
    #[cfg_attr(fewer_small, optimize(size))]
    fn finish_synced(mut self, mode: u32, mtime: Option<(i64, u32)>) -> Result<bool, String> {
        let w = self.file.take().unwrap();
        let r = self.complete(w, mode, mtime);
        if r.is_err() {
            let _ = fs::remove_file(&self.tmp);
        }
        unregister_temp(self.slot.take());
        r.map(|_| self.durable)
    }

    #[cfg_attr(fewer_small, optimize(size))]
    fn complete(&self, w: io::BufWriter<fs::File>, mode: u32, mtime: Option<(i64, u32)>) -> Result<(), String> {
        use crate::portable::PermissionsExt;
        let what = |e: io::Error| format!("cannot write {}: {e}", self.dest.display());
        let f = w.into_inner().map_err(|e| what(e.into_error()))?;
        if let Some(t) = mtime {
            f.set_times(fs::FileTimes::new().set_modified(crate::folder::to_time(t)).set_accessed(std::time::SystemTime::now())).map_err(what)?;
        }
        f.set_permissions(fs::Permissions::from_mode(mode & 0o1777)).map_err(what)?;
        if self.durable {
            f.sync_all().map_err(what)?;
        }
        drop(f);
        if self.force {
            fs::rename(&self.tmp, &self.dest).map_err(|e| format!("cannot write {}: {e}", self.dest.display()))?;
        } else {
            // a new name that cannot replace a file made in the meantime: a hard link
            // (fails if the name exists), else (file systems without links) a checked rename
            match fs::hard_link(&self.tmp, &self.dest) {
                Ok(()) => {
                    let _ = fs::remove_file(&self.tmp);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(format!("{} already exists (use -f to overwrite)", self.dest.display()));
                }
                Err(_) => {
                    if fs::symlink_metadata(&self.dest).is_ok() {
                        return Err(format!("{} already exists (use -f to overwrite)", self.dest.display()));
                    }
                    fs::rename(&self.tmp, &self.dest).map_err(|e| format!("cannot write {}: {e}", self.dest.display()))?;
                }
            }
        }
        // make the new name itself durable
        if self.durable {
            let dir = self.dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
            if let Ok(d) = fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
}

impl Write for AtomicFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.file.as_mut().unwrap().write(b)
    }
    fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        self.file.as_mut().unwrap().write_all(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().unwrap().flush()
    }
}

impl Drop for AtomicFile {
    #[cfg_attr(fewer_small, optimize(size))]
    fn drop(&mut self) {
        if self.file.take().is_some() {
            // not finished: never leave a half-written file behind
            let _ = fs::remove_file(&self.tmp);
            unregister_temp(self.slot.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chan does what sync_channel(0) and sync_channel(1) did for the reader threads.
    #[test]
    fn chan_like_sync_channel() {
        for cap in [0usize, 1] {
            // every item in order, then the end; with cap 0 an item is taken before its send returns
            let ch = Chan::new(cap);
            let got: Vec<Item> = std::thread::scope(|s| {
                let (tx, rx) = (Tx(&ch), Rx(&ch));
                let c = &ch;
                s.spawn(move || {
                    for i in 0..2000u64 {
                        assert!(tx.send(Ok((vec![i as u8], i, i + 1))));
                        if cap == 0 {
                            assert!(c.lock().taken > i);
                        }
                    }
                    assert!(tx.send(Err("end".into())));
                });
                rx.collect()
            });
            assert_eq!(got.len(), 2001);
            for (i, g) in got[..2000].iter().enumerate() {
                assert_eq!(g, &Ok((vec![i as u8], i as u64, i as u64 + 1)));
            }
            assert_eq!(got[2000], Err("end".to_string()));
            // the receiver stops early: the reader's next send fails, nothing waits forever
            for stop in [0usize, 1, 5] {
                let ch = Chan::new(cap);
                let sent = std::thread::scope(|s| {
                    let (tx, mut rx) = (Tx(&ch), Rx(&ch));
                    let h = s.spawn(move || {
                        let mut n = 0u64;
                        while tx.send(Ok((vec![0; 1000], n, 0))) {
                            n += 1;
                        }
                        n
                    });
                    for _ in 0..stop {
                        assert!(rx.next().unwrap().is_ok());
                    }
                    drop(rx);
                    h.join().unwrap()
                });
                // cap 0: exactly the items taken; cap 1: one more may have been accepted
                assert!(sent == stop as u64 || (cap == 1 && sent == stop as u64 + 1), "cap {cap} stop {stop} sent {sent}");
            }
        }
    }

    fn pack_bytes(data: &[u8], meta: &[u8], flags: u8) -> Vec<u8> {
        let mut aw = ArchiveWriter::new(Vec::new(), flags, meta).unwrap();
        pack_stream(data, Some(data.len() as u64), &mut aw, 2).unwrap();
        aw.finish().unwrap().0
    }

    /// An output is forced to disk only when it is made durable (--rm deletes the input
    /// next); otherwise it is left to the system, as zstd does (forcing every output made
    /// opening to a file slower than zstd -d).
    #[test]
    fn outputs_forced_to_disk_only_when_durable() {
        let dir = std::env::temp_dir().join(format!("fewer_durable_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for durable in [false, true] {
            let dest = dir.join(format!("out_{durable}"));
            let _ = fs::remove_file(&dest);
            let mut f = AtomicFile::create(&dest, false, durable).unwrap();
            f.write_all(b"some bytes\n").unwrap();
            assert_eq!(f.finish_synced(0o640, Some((1_700_000_000, 5))).unwrap(), durable);
            assert_eq!(fs::read(&dest).unwrap(), b"some bytes\n");
            let md = fs::metadata(&dest).unwrap();
            use crate::portable::{MetadataExt, PermissionsExt};
            assert_eq!(md.permissions().mode() & 0o777, 0o640);
            assert_eq!((md.mtime(), md.mtime_nsec()), (1_700_000_000, 5));
            fs::remove_file(&dest).unwrap();
        }
        let _ = fs::remove_dir(&dir);
    }

    fn unpack_bytes(a: &[u8]) -> Result<Vec<u8>, String> {
        let ar = ArchiveReader::open(a)?;
        let mut out = Vec::new();
        unpack_stream(ar, &mut out, 2)?;
        Ok(out)
    }

    #[test]
    fn varints() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            let mut p = 0;
            assert_eq!(get_varint(&b, &mut p), Some(v));
            assert_eq!(p, b.len());
            assert_eq!(get_varint(&b[..b.len() - 1], &mut 0), None);
        }
        for v in [0i64, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(unzz(zz(v)), v);
        }
    }

    #[test]
    fn round_trip_and_meta() {
        let data = b"line 1\nline 2 10.0.0.1\nno end".to_vec();
        let a = pack_bytes(&data, b"meta!", 0);
        assert_eq!(&a[..4], MAGIC);
        assert_eq!(a[4], VERSION);
        let ar = ArchiveReader::open(&a[..]).unwrap();
        assert_eq!(ar.meta, b"meta!");
        assert!(!ar.is_folder());
        assert_eq!(unpack_bytes(&a).unwrap(), data);
        // empty input: header, meta and end record only
        let e = pack_bytes(b"", b"", 0);
        assert_eq!(unpack_bytes(&e).unwrap(), b"");
        // two archives joined
        let mut j = a.clone();
        j.extend_from_slice(&e);
        j.extend_from_slice(&a);
        let mut both = data.clone();
        both.extend_from_slice(&data);
        assert_eq!(unpack_bytes(&j).unwrap(), both);
    }

    #[test]
    fn every_damage_is_caught() {
        let mut data = Vec::new();
        for i in 0..2000 {
            data.extend_from_slice(format!("10:00:{:02} worker {} done in {} ms\n", i % 60, i % 4, i * 7 % 900).as_bytes());
        }
        let a = pack_bytes(&data, b"", 0);
        // every cut-off point and a flip of every bit in the first 64 bytes and the last 64
        for cut in 0..a.len() {
            assert!(unpack_bytes(&a[..cut]).is_err(), "cut at {cut} not caught");
        }
        let mut places: Vec<usize> = (0..64.min(a.len())).collect();
        places.extend(a.len().saturating_sub(64)..a.len());
        places.extend((0..a.len()).step_by(7));
        for i in places {
            for bit in 0..8 {
                let mut b = a.clone();
                b[i] ^= 1 << bit;
                if let Ok(d) = unpack_bytes(&b) {
                    assert_eq!(d, data, "flip at {i} bit {bit} gave wrong output");
                }
            }
        }
        // trailing bytes after the end record
        let mut t = a.clone();
        t.push(b'x');
        assert!(unpack_bytes(&t).is_err());
    }

    #[test]
    fn writer_refuses_what_readers_refuse() {
        let mut aw = ArchiveWriter::new(Vec::new(), 0, b"").unwrap();
        assert!(aw.segment(10, &vec![0u8; (1 << 20) + 21]).is_err());
        assert!(aw.segment(0, b"x").is_err());
        assert!(aw.segment(10, &[0u8; 20]).is_ok());
    }

    #[test]
    fn segments_cut_after_line_ends() {
        let mut s = Segmenter { r: &b"aaa\nbbbb\ncc"[..], carry: Vec::new(), max: 6, eof: false };
        let mut got = Vec::new();
        while let Some(x) = s.next_segment().unwrap() {
            got.push(x);
        }
        assert_eq!(got, vec![b"aaa\n".to_vec(), b"bbbb\n".to_vec(), b"cc".to_vec()]);
        // a line longer than a segment is cut at the size limit
        let mut s = Segmenter { r: &b"0123456789\n"[..], carry: Vec::new(), max: 4, eof: false };
        let mut all = Vec::new();
        while let Some(x) = s.next_segment().unwrap() {
            assert!(x.len() <= 4);
            all.extend_from_slice(&x);
        }
        assert_eq!(all, b"0123456789\n");
        // equal segments for a known size
        let s = Segmenter::new(&b""[..], Some(600 << 20));
        assert!(s.max <= SEGMENT_MAX && s.max > 200 << 20);
    }
}
