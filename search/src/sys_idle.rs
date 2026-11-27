//! The C library calls the DNA index's builders need (section 1.1 lists them
//! for sys.rs, kept here in a file of their own so that sys.rs stays as it is): the 1-minute
//! load average (getloadavg), background priority for the calling thread, and that priority
//! read back for the debug log and the checks.
//!
//! - macOS: pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0), read back with
//!   pthread_get_qos_class_np. The thread then runs on the efficiency cores with its disk
//!   reads throttled, after every other kind of work.
//! - Linux: setpriority(PRIO_PROCESS, 0, 19), read back with getpriority. On Linux the nice
//!   value belongs to the calling thread, not to the whole process.
//! - Windows: SetThreadPriority(THREAD_PRIORITY_IDLE), read back with GetThreadPriority. Windows
//!   has no load average: the load is the share of time the cores were busy since the last
//!   reading (GetSystemTimes), times the number of cores.
//!
//! A test switch replaces getloadavg (never set in normal use): FEWER_TEST_LOAD names a file
//! that holds the 1-minute load to use, read at every check, so a check can change it while a
//! builder runs. A file that cannot be read as a number counts as a load too high to build.

use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::c_int;

#[cfg(unix)]
unsafe extern "C" {
    fn getloadavg(loads: *mut f64, n: c_int) -> c_int;
}

#[cfg(target_os = "macos")]
mod prio {
    use std::ffi::{c_int, c_uint, c_void};

    unsafe extern "C" {
        pub fn pthread_set_qos_class_self_np(class: c_uint, relative: c_int) -> c_int;
        pub fn pthread_get_qos_class_np(thread: *mut c_void, class: *mut c_uint, relative: *mut c_int) -> c_int;
        pub fn pthread_self() -> *mut c_void;
    }

    /// QOS_CLASS_BACKGROUND (sys/qos.h).
    pub const BACKGROUND: c_uint = 0x09;
}

#[cfg(all(unix, not(target_os = "macos")))]
mod prio {
    use std::ffi::{c_int, c_uint};

    unsafe extern "C" {
        pub fn setpriority(which: c_int, who: c_uint, prio: c_int) -> c_int;
        pub fn getpriority(which: c_int, who: c_uint) -> c_int;
    }

    pub const PRIO_PROCESS: c_int = 0;
    /// The lowest priority, nice 19.
    pub const BACKGROUND: c_int = 19;
}

#[cfg(windows)]
mod prio {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentThread() -> isize;
        pub fn SetThreadPriority(thread: isize, prio: i32) -> i32;
        pub fn GetThreadPriority(thread: isize) -> i32;
        pub fn GetSystemTimes(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    }

    /// THREAD_PRIORITY_IDLE.
    pub const BACKGROUND: i32 = -15;

    /// The time the cores were idle, and in all (kernel time counts the idle time too), in
    /// 100 ns units.
    fn times() -> Option<(u64, u64)> {
        let (mut idle, mut kernel, mut user) = (0u64, 0u64, 0u64);
        // SAFETY: GetSystemTimes writes three FILETIMEs (two 32-bit halves each) into the locals
        (unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } != 0).then_some((idle, kernel + user))
    }

    /// The share of time the cores were busy since the last reading (at least a second
    /// before; until then the last answer), times the number of cores. The first reading
    /// measures over 200 ms.
    pub fn load(cores: usize) -> Option<f64> {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        static LAST: Mutex<Option<(Instant, u64, u64, f64)>> = Mutex::new(None);
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let (i0, all0) = match *last {
            Some((t, _, _, l)) if t.elapsed() < Duration::from_secs(1) => return Some(l),
            Some((_, i, a, _)) => (i, a),
            None => {
                let first = times()?;
                std::thread::sleep(Duration::from_millis(200));
                first
            }
        };
        let (i1, all1) = times()?;
        let (di, da) = (i1.saturating_sub(i0), all1.saturating_sub(all0));
        let busy = if da == 0 { 0.0 } else { 1.0 - di as f64 / da as f64 };
        let l = busy.clamp(0.0, 1.0) * cores as f64;
        *last = Some((Instant::now(), i1, all1, l));
        Some(l)
    }
}

/// The share of the cores under which the 1-minute load counts as idle (25%).
pub const IDLE_SHARE: f64 = 0.25;

/// The 1-minute load average: the test switch's when FEWER_TEST_LOAD is set, else
/// getloadavg's. None when it cannot be read.
pub fn load() -> Option<f64> {
    load_from(std::env::var_os("FEWER_TEST_LOAD").filter(|f| !f.is_empty()).as_deref())
}

/// The load from the test switch's file when there is one, else from getloadavg.
fn load_from(test_file: Option<&OsStr>) -> Option<f64> {
    if let Some(f) = test_file {
        return std::fs::read_to_string(f).ok()?.trim().parse::<f64>().ok().filter(|l| l.is_finite() && *l >= 0.0);
    }
    #[cfg(windows)]
    return prio::load(cores());
    #[cfg(unix)]
    {
        let mut l = [0f64; 3];
        // SAFETY: getloadavg writes at most 1 number into `l`
        let n = unsafe { getloadavg(l.as_mut_ptr(), 1) };
        (n >= 1).then_some(l[0])
    }
}

/// The number of cores.
pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Whether a load counts as idle on this many cores: under IDLE_SHARE of them. A load that
/// could not be read does not.
fn idle_with(load: Option<f64>, cores: usize) -> bool {
    load.is_some_and(|l| l < cores as f64 * IDLE_SHARE)
}

/// Whether the computer is idle enough to build, with the load and the
/// cores it was told from, for the log.
pub fn idle() -> (bool, Option<f64>, usize) {
    let (l, c) = (load(), cores());
    (idle_with(l, c), l, c)
}

/// Run the calling thread at background priority.
pub fn background() -> Result<(), String> {
    // (macOS gives the error number back; Linux gives -1 and sets errno)
    #[cfg(target_os = "macos")]
    // SAFETY: changes only the calling thread's QoS class
    let r = match unsafe { prio::pthread_set_qos_class_self_np(prio::BACKGROUND, 0) } {
        0 => Ok(()),
        e => Err(std::io::Error::from_raw_os_error(e)),
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    // SAFETY: changes only the calling thread's nice value (Linux)
    let r = match unsafe { prio::setpriority(prio::PRIO_PROCESS, 0, prio::BACKGROUND) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    };
    #[cfg(windows)]
    // SAFETY: changes only the calling thread's priority
    let r = match unsafe { prio::SetThreadPriority(prio::GetCurrentThread(), prio::BACKGROUND) } {
        0 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    };
    r.map_err(|e| format!("cannot lower its priority: {e}"))
}

/// The calling thread's priority as the system tells it, for the debug log: "background
/// priority (QoS class 9)" or "background priority (nice 19)" when it is background.
pub fn priority() -> String {
    let (bg, what) = read_priority();
    format!("{} priority ({what})", if bg { "background" } else { "normal" })
}

/// Whether the calling thread runs at background priority.
#[cfg(test)]
pub fn is_background() -> bool {
    read_priority().0
}

#[cfg(target_os = "macos")]
fn read_priority() -> (bool, String) {
    let (mut class, mut rel) = (0, 0);
    // SAFETY: reads the calling thread's QoS class into the two locals
    let r = unsafe { prio::pthread_get_qos_class_np(prio::pthread_self(), &mut class, &mut rel) };
    if r != 0 {
        return (false, "QoS class unknown".into());
    }
    (class == prio::BACKGROUND, format!("QoS class {class}"))
}

#[cfg(windows)]
fn read_priority() -> (bool, String) {
    // SAFETY: reads the calling thread's priority
    let n = unsafe { prio::GetThreadPriority(prio::GetCurrentThread()) };
    (n == prio::BACKGROUND, format!("thread priority {n}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_priority() -> (bool, String) {
    // SAFETY: reads the calling thread's nice value
    let n = unsafe { prio::getpriority(prio::PRIO_PROCESS, 0) };
    (n >= prio::BACKGROUND, format!("nice {n}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_is_a_quarter_of_the_cores() {
        assert!(idle_with(Some(0.0), 1));
        assert!(idle_with(Some(3.49), 14));
        assert!(!idle_with(Some(3.5), 14), "25% of 14 cores is 3.5");
        assert!(!idle_with(Some(99.0), 14));
        assert!(!idle_with(None, 14), "a load that cannot be read is not idle");
        // the real load can be read on this computer
        assert!(load_from(None).is_some_and(|l| l >= 0.0));
    }

    #[test]
    fn the_test_switch_replaces_the_load() {
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-load", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("load");
        std::fs::write(&f, "0.25\n").unwrap();
        assert_eq!(load_from(Some(f.as_os_str())), Some(0.25));
        std::fs::write(&f, "99").unwrap();
        assert_eq!(load_from(Some(f.as_os_str())), Some(99.0));
        // not a number, a negative one, or no file: no load, so not idle
        for bad in ["high", "-1", "", "NaN"] {
            std::fs::write(&f, bad).unwrap();
            assert_eq!(load_from(Some(f.as_os_str())), None, "{bad:?}");
        }
        assert_eq!(load_from(Some(d.join("none").as_os_str())), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn background_priority_is_the_calling_threads() {
        // a new thread starts at normal priority, goes to background, and says so; the thread
        // that started it keeps its own
        let before = is_background();
        let (bg, text) = std::thread::spawn(|| {
            let was = is_background();
            background().unwrap();
            (!was && is_background(), priority())
        })
        .join()
        .unwrap();
        assert!(bg, "{text}");
        assert!(text.starts_with("background priority ("), "{text}");
        assert_eq!(is_background(), before);
    }
}
