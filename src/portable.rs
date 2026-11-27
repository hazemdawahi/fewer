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

#[cfg(unix)]
pub use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

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
        /// (Windows has no way to make a Permissions value from nothing: one read from the
        /// temporary folder, then read-only or not as the mode says)
        fn from_mode(mode: u32) -> Permissions {
            let mut p = std::fs::metadata(std::env::temp_dir()).or_else(|_| std::fs::metadata(".")).map(|m| m.permissions()).expect("a folder's permissions");
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
pub fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link);
}
