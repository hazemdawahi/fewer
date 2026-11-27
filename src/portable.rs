//! The Mac/Linux-only parts of std that fewer-pack uses, under the same names on Windows, so the
//! rest of the code reads the same on every system. On Mac and Linux these are std's own traits.
//! On Windows:
//! - a name's bytes are the bytes Rust keeps for it (`as_encoded_bytes`, UTF-8 for any name
//!   that is valid Unicode); bytes from elsewhere that are not UTF-8 read as U+FFFD;
//! - a file's identity is its creation time (to the 100 ns) and its size, as Windows has no
//!   inode number in std: two files created in the same 100 ns with the same size would read as
//!   one;
//! - a file's mode is 0o755 for a folder, 0o444 when read-only, else 0o644; a mode set keeps
//!   only read-only or not; open and folder modes and open flags are not used.
//!
//! (Both programs use this file, each only some of it.)
#![allow(dead_code, unused_imports)]

#[cfg(unix)]
pub use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
pub use std::os::unix::process::{CommandExt, ExitStatusExt};

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use std::ffi::{OsStr, OsString};
    use std::fs::{DirBuilder, Metadata, OpenOptions, Permissions};
    use std::os::windows::fs::MetadataExt as WinMeta;

    pub trait OsStrExt {
        fn as_bytes(&self) -> &[u8];
        fn from_bytes(b: &[u8]) -> &Self;
    }

    impl OsStrExt for OsStr {
        fn as_bytes(&self) -> &[u8] {
            self.as_encoded_bytes()
        }
        fn from_bytes(b: &[u8]) -> &OsStr {
            match std::str::from_utf8(b) {
                Ok(s) => OsStr::new(s),
                Err(_) => OsStr::new("\u{fffd}"),
            }
        }
    }

    pub trait OsStringExt {
        fn into_vec(self) -> Vec<u8>;
        fn from_vec(v: Vec<u8>) -> Self;
    }

    impl OsStringExt for OsString {
        fn into_vec(self) -> Vec<u8> {
            self.into_encoded_bytes()
        }
        fn from_vec(v: Vec<u8>) -> OsString {
            match String::from_utf8(v) {
                Ok(s) => s.into(),
                Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned().into(),
            }
        }
    }

    /// Windows' FILETIME (100 ns since 1601) as Unix seconds and nanoseconds.
    fn unix_time(t: u64) -> (i64, i64) {
        let t = t as i64;
        (t / 10_000_000 - 11_644_473_600, (t % 10_000_000) * 100)
    }

    pub trait MetadataExt {
        fn dev(&self) -> u64;
        fn ino(&self) -> u64;
        fn mode(&self) -> u32;
        fn mtime(&self) -> i64;
        fn mtime_nsec(&self) -> i64;
        fn uid(&self) -> u32;
    }

    impl MetadataExt for Metadata {
        fn dev(&self) -> u64 {
            0
        }
        fn ino(&self) -> u64 {
            self.creation_time() ^ self.len().rotate_left(32)
        }
        fn mode(&self) -> u32 {
            if self.is_dir() {
                0o755
            } else if self.permissions().readonly() {
                0o444
            } else {
                0o644
            }
        }
        fn mtime(&self) -> i64 {
            unix_time(self.last_write_time()).0
        }
        fn mtime_nsec(&self) -> i64 {
            unix_time(self.last_write_time()).1
        }
        fn uid(&self) -> u32 {
            0
        }
    }

    pub trait PermissionsExt {
        fn from_mode(mode: u32) -> Self;
        fn mode(&self) -> u32;
        fn set_mode(&mut self, mode: u32);
    }

    impl PermissionsExt for Permissions {
        /// (Windows has no way to make a Permissions value from nothing: this program's own
        /// file's, then read-only or not as the mode says. A folder's would not do: setting
        /// its folder bit on a file fails, "The parameter is incorrect")
        fn from_mode(mode: u32) -> Permissions {
            static BASE: std::sync::OnceLock<Permissions> = std::sync::OnceLock::new();
            let mut p = BASE
                .get_or_init(|| {
                    std::env::current_exe().and_then(std::fs::metadata).map(|m| m.permissions()).unwrap_or_else(|_| {
                        let f = std::env::temp_dir().join(format!("fewer-mode-{}", std::process::id()));
                        let p = std::fs::write(&f, b"").and_then(|_| std::fs::metadata(&f)).map(|m| m.permissions()).expect("a file's permissions");
                        let _ = std::fs::remove_file(&f);
                        p
                    })
                })
                .clone();
            p.set_readonly(mode & 0o222 == 0);
            p
        }
        fn mode(&self) -> u32 {
            if self.readonly() { 0o444 } else { 0o644 }
        }
        fn set_mode(&mut self, mode: u32) {
            self.set_readonly(mode & 0o222 == 0);
        }
    }

    pub trait OpenOptionsExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
        fn custom_flags(&mut self, flags: i32) -> &mut Self;
    }

    impl OpenOptionsExt for OpenOptions {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
        fn custom_flags(&mut self, _flags: i32) -> &mut Self {
            self
        }
    }

    /// Starting programs: Windows has no code run in the child before it starts (`pre_exec`:
    /// a new session there; a child is not ended with its parent on Windows anyway), no
    /// process groups, and no `exec` (the program is run, waited for, and its code is ours).
    pub trait CommandExt {
        /// # Safety
        /// Nothing is run on Windows; the closure is dropped.
        unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
        where
            F: FnMut() -> std::io::Result<()> + Send + Sync + 'static;
        fn process_group(&mut self, pgroup: i32) -> &mut Self;
        fn exec(&mut self) -> std::io::Error;
    }

    impl CommandExt for std::process::Command {
        unsafe fn pre_exec<F>(&mut self, _f: F) -> &mut Self
        where
            F: FnMut() -> std::io::Result<()> + Send + Sync + 'static,
        {
            self
        }
        fn process_group(&mut self, _pgroup: i32) -> &mut Self {
            self
        }
        fn exec(&mut self) -> std::io::Error {
            match self.status() {
                Ok(st) => std::process::exit(st.code().unwrap_or(1)),
                Err(e) => e,
            }
        }
    }

    /// An exit status: Windows has no signals, and a raw status is the exit code itself.
    pub trait ExitStatusExt {
        fn signal(&self) -> Option<i32>;
        fn from_raw(raw: i32) -> Self;
    }

    impl ExitStatusExt for std::process::ExitStatus {
        fn signal(&self) -> Option<i32> {
            None
        }
        fn from_raw(raw: i32) -> std::process::ExitStatus {
            // (a Unix wait status holds the code in its second byte)
            <std::process::ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw((raw >> 8) as u32)
        }
    }

    pub trait DirBuilderExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl DirBuilderExt for DirBuilder {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }
}

/// A symbolic link at `link` naming `target` (on Windows a file link, which needs Developer
/// Mode or an administrator; the error says so when it fails).
pub fn symlink<P: AsRef<std::path::Path>, Q: AsRef<std::path::Path>>(target: P, link: Q) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link);
}

/// A lock on an open file that others see, and that keeps no one from reading or writing it.
/// Mac and Linux: std's (flock). Windows: std's lock covers the whole file, and reading a locked
/// file there fails, even in this program, so the lock is on one byte far past any data.
pub fn lock(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    return f.lock();
    #[cfg(windows)]
    return file_lock::take(f, true, false).map_err(|e| match e {
        std::fs::TryLockError::Error(e) => e,
        std::fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock.into(),
    });
}

/// A shared lock (as `lock`), now or not at all.
pub fn try_lock_shared(f: &std::fs::File) -> Result<(), std::fs::TryLockError> {
    #[cfg(unix)]
    return f.try_lock_shared();
    #[cfg(windows)]
    return file_lock::take(f, false, true);
}

/// Let go of a lock taken by `lock` or `try_lock_shared`.
pub fn unlock(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    return f.unlock();
    #[cfg(windows)]
    return file_lock::give_back(f);
}

#[cfg(windows)]
mod file_lock {
    use std::os::windows::io::{AsRawHandle, RawHandle};

    /// Windows' OVERLAPPED: here only where the locked byte is.
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: isize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(h: RawHandle, flags: u32, reserved: u32, len_low: u32, len_high: u32, ov: *mut Overlapped) -> i32;
        fn UnlockFileEx(h: RawHandle, reserved: u32, len_low: u32, len_high: u32, ov: *mut Overlapped) -> i32;
    }

    /// The byte locked: the last but one a file can have, never data.
    fn place() -> Overlapped {
        Overlapped { internal: 0, internal_high: 0, offset: 0xFFFF_FFFE, offset_high: 0xFFFF_FFFF, event: 0 }
    }

    pub fn take(f: &std::fs::File, exclusive: bool, now: bool) -> Result<(), std::fs::TryLockError> {
        const EXCLUSIVE: u32 = 2;
        const FAIL_IMMEDIATELY: u32 = 1;
        let flags = if exclusive { EXCLUSIVE } else { 0 } | if now { FAIL_IMMEDIATELY } else { 0 };
        let mut ov = place();
        // SAFETY: locks one byte of a file we hold open; `ov` outlives the call (the handle is
        // not opened for overlapped I/O, so the call ends with the lock taken or refused)
        if unsafe { LockFileEx(f.as_raw_handle(), flags, 0, 1, 0, &mut ov) } != 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        // (ERROR_LOCK_VIOLATION: another handle holds it)
        if e.raw_os_error() == Some(33) { Err(std::fs::TryLockError::WouldBlock) } else { Err(std::fs::TryLockError::Error(e)) }
    }

    pub fn give_back(f: &std::fs::File) -> std::io::Result<()> {
        let mut ov = place();
        // SAFETY: unlocks the byte `take` locked, on the same open file
        if unsafe { UnlockFileEx(f.as_raw_handle(), 0, 1, 0, &mut ov) } != 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }
}

/// The parent process's id (0 on Windows, where std has none: callers then stop looking up).
pub fn parent_id() -> u32 {
    #[cfg(unix)]
    return std::os::unix::process::parent_id();
    #[cfg(windows)]
    return 0;
}
