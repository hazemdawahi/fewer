//! The C library calls fewer needs that std does not offer: copying file descriptors
//! (the MCP server's stdout guard), local time (answers show local times; since/until read
//! them) and whether a process still runs (leftover temporary files).
//!
//! On Windows: local time through Windows' own zone rules (daylight saving as Windows has it
//! for each year), and whether a process runs through its exit code; copying descriptors is
//! not used there (the MCP server writes to standard output directly).

#[cfg(unix)]
use std::ffi::{c_int, c_long};

#[cfg(windows)]
pub use win::{alive, from_local, local};

#[cfg(unix)]
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

#[cfg(unix)]
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
#[cfg(all(unix, not(target_os = "macos")))]
const F_DUPFD_CLOEXEC: c_int = 1030;

/// A new descriptor for the same file as `fd`, closed in child processes.
#[cfg(unix)]
pub fn dup_cloexec(fd: i32) -> std::io::Result<i32> {
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) only makes a new descriptor
    let r = unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 0 as c_int) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r) }
}

/// Make descriptor `to` a copy of `from`.
#[cfg(unix)]
pub fn dup_to(from: i32, to: i32) -> std::io::Result<()> {
    // SAFETY: dup2 only changes which file the descriptor `to` names
    let r = unsafe { dup2(from, to) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Local calendar time of a Unix time in seconds: (year, month 1-12, day, hour, minute,
/// second, offset from UTC in seconds).
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists
    let r = unsafe { kill(pid as c_int, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM
}

#[cfg(windows)]
mod win {
    /// Windows' SYSTEMTIME.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SysTime {
        year: u16,
        month: u16,
        weekday: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        ms: u16,
    }

    /// Room for Windows' DYNAMIC_TIME_ZONE_INFORMATION (432 bytes), only passed along.
    #[repr(C, align(8))]
    struct Zone([u8; 512]);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDynamicTimeZoneInformation(z: *mut Zone) -> u32;
        fn SystemTimeToTzSpecificLocalTimeEx(z: *const Zone, utc: *const SysTime, local: *mut SysTime) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn GetExitCodeProcess(h: isize, code: *mut u32) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn GetLastError() -> u32;
    }

    /// Local calendar time of a Unix time in seconds: (year, month 1-12, day, hour, minute,
    /// second, offset from UTC in seconds); shown in UTC outside Windows' range (1601-30827).
    pub fn local(secs: i64) -> (i32, u32, u32, u32, u32, u32, i64) {
        let (y, mo, d, h, mi, s) = crate::times::civil(secs);
        let utc_shown = (y, mo, d, h, mi, s, 0);
        if !(1601..=30827).contains(&y) {
            return utc_shown;
        }
        let utc = SysTime { year: y as u16, month: mo as u16, day: d as u16, hour: h as u16, minute: mi as u16, second: s as u16, ..Default::default() };
        let mut zone = Zone([0; 512]);
        let mut out = SysTime::default();
        // SAFETY: both calls only write into the values passed; the zone buffer is larger
        // than what Windows writes
        let ok = unsafe { GetDynamicTimeZoneInformation(&mut zone) != u32::MAX && SystemTimeToTzSpecificLocalTimeEx(&zone, &utc, &mut out) != 0 };
        if !ok {
            return utc_shown;
        }
        let (ly, lmo, ld, lh, lmi, ls) = (out.year as i32, out.month as u32, out.day as u32, out.hour as u32, out.minute as u32, out.second as u32);
        let wall = crate::times::days_from_civil(ly, lmo, ld) * 86_400 + (lh * 3600 + lmi * 60 + ls) as i64;
        (ly, lmo, ld, lh, lmi, ls, wall - secs)
    }

    /// The Unix time of a local calendar time (fields out of range carry over, as mktime
    /// does): the instant whose local time it is, found from the zone's offset there.
    pub fn from_local(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
        let (y, mo) = (y + (mo as i32 - 1).div_euclid(12), (mo as i32 - 1).rem_euclid(12) as u32 + 1);
        let wall = crate::times::days_from_civil(y, mo, 1) * 86_400 + (d as i64 - 1) * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64;
        let first = wall - local(wall).6;
        Some(wall - local(first).6)
    }

    /// Whether a process with this id runs (or exists but is not ours to look at).
    pub fn alive(pid: u32) -> bool {
        const QUERY_LIMITED: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        const ACCESS_DENIED: u32 = 5;
        if pid == 0 {
            return false;
        }
        // SAFETY: a handle opened here is closed here; the exit code is written into `code`
        unsafe {
            let h = OpenProcess(QUERY_LIMITED, 0, pid);
            if h == 0 {
                return GetLastError() == ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            !ok || code == STILL_ACTIVE
        }
    }
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
