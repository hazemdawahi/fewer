//! The C library calls fewer needs that std does not offer: copying file descriptors
//! (the MCP server's stdout guard), local time (answers show local times; since/until read
//! them) and whether a process still runs (leftover temporary files).

use std::ffi::{c_int, c_long};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: usize,
}

unsafe extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn dup2(from: c_int, to: c_int) -> c_int;
    fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    fn mktime(tm: *mut Tm) -> i64;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn tzset();
}

#[cfg(target_os = "macos")]
const F_DUPFD_CLOEXEC: c_int = 67;
#[cfg(not(target_os = "macos"))]
const F_DUPFD_CLOEXEC: c_int = 1030;

/// A new descriptor for the same file as `fd`, closed in child processes.
pub fn dup_cloexec(fd: i32) -> std::io::Result<i32> {
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) only makes a new descriptor
    let r = unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 0 as c_int) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r) }
}

/// Make descriptor `to` a copy of `from`.
pub fn dup_to(from: i32, to: i32) -> std::io::Result<()> {
    // SAFETY: dup2 only changes which file the descriptor `to` names
    let r = unsafe { dup2(from, to) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Local calendar time of a Unix time in seconds: (year, month 1-12, day, hour, minute,
/// second, offset from UTC in seconds).
pub fn local(secs: i64) -> (i32, u32, u32, u32, u32, u32, i64) {
    let mut tm = Tm::default();
    // SAFETY: localtime_r writes only into `tm`
    let ok = unsafe {
        tzset();
        !localtime_r(&secs, &mut tm).is_null()
    };
    if !ok {
        // (out of the C library's range: shown in UTC)
        let (y, mo, d, h, mi, s) = crate::times::civil(secs);
        return (y, mo, d, h, mi, s, 0);
    }
    (tm.tm_year + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32, tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32, tm.tm_gmtoff)
}

/// The Unix time of a local calendar time (None when the C library cannot place it).
pub fn from_local(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    let mut tm = Tm { tm_year: y - 1900, tm_mon: mo as c_int - 1, tm_mday: d as c_int, tm_hour: h as c_int, tm_min: mi as c_int, tm_sec: s as c_int, tm_isdst: -1, ..Default::default() };
    // SAFETY: mktime reads and normalizes `tm` only
    let t = unsafe {
        tzset();
        mktime(&mut tm)
    };
    (t != -1 || (y, mo, d, h, mi, s) == (1969, 12, 31, 23, 59, 59)).then_some(t)
}

/// Whether a process with this id runs (or exists but is not ours to signal).
pub fn alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists
    let r = unsafe { kill(pid as c_int, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_round_trips() {
        for &t in &[0i64, 1_700_000_000, 1_763_460_000, 1_234_567_890, 1_900_000_000] {
            let (y, mo, d, h, mi, s, off) = local(t);
            assert_eq!(from_local(y, mo, d, h, mi, s), Some(t), "{t}");
            // the offset agrees with the UTC calendar
            let u = crate::times::civil(t + off);
            assert_eq!((u.0, u.1, u.2, u.3, u.4, u.5), (y, mo, d, h, mi, s));
        }
        assert!(alive(std::process::id()));
        assert!(!alive(0));
    }
}
