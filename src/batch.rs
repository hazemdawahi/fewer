//! Several inputs of one command worked on at the same time: `fewer-pack FILE...` and
//! `fewer-pack -d FILE.cmf...` (which inputs may join: `batch_inputs` in main.rs).
//!
//! A file of one block keeps only a few cores busy, so packing or opening many files one
//! after the other leaves most cores idle. Here each input is worked on by a job thread of
//! its own, with the engine's usual threads (the system shares the cores between them), so
//! every output has exactly the bytes it has when its input is worked on alone. What the
//! user sees stays in input order:
//! - each input's messages (summary, warnings, error) are printed once it is done, after
//!   those of every input before it (`say`);
//! - an output gets its name (or its bytes go to standard output) only after every input
//!   before it has done so or failed (`in_turn`); with --rm only after every input before it
//!   is completely done, so outputs and deletions happen in input order;
//! - with -c each input's bytes are held in memory until the inputs before it are written
//!   (HOLD_MAX bytes for all inputs together; past that the input waits) (`Out`);
//! - a panic (panic = "abort" builds: the engine stopped on damaged data) names the input
//!   that stopped and ends the program as one input at a time would have: the inputs
//!   before it complete and are reported, nothing after it is kept (`on_panic`).
//!
//! Bounded: at most WINDOW inputs started and not yet reported (each holds a temporary
//! output and a few open files); at most `threads` inputs running at once, holding at most
//! BYTES bytes of input together (an input of any size runs when nothing else does). The
//! biggest inputs start first, so the last ones to finish are small.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::io::{self, Write};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Inputs started and not yet reported, at most (container::TEMP_PATHS has room for all
/// their temporary outputs).
pub const WINDOW: usize = 24;
/// Input bytes of the inputs running at once (packing needs several times that in memory).
const BYTES: u64 = 512 << 20;
/// Bytes held in memory for standard output, for all inputs together.
const HOLD_MAX: usize = 64 << 20;
/// Bytes collected before they are written to standard output in an input's turn.
const OUT_CHUNK: usize = 1 << 20;

/// JOB of a thread that works on no input; COORD: the thread that reports and starts them.
const NONE: usize = usize::MAX;
const COORD: usize = usize::MAX - 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Queued,
    Running,
    /// waiting for the inputs before it (its turn) or for room to hold its output
    Parked,
    Done,
}

/// What an input printed, and its error.
type Report = (Vec<String>, Option<String>);

struct State {
    on: bool,
    /// (for the panic message)
    #[cfg_attr(not(panic = "abort"), allow(dead_code))]
    names: Vec<OsString>,
    sizes: Vec<u64>,
    st: Vec<St>,
    /// passed[i]: input i has named its output (or failed, or is done); the next may name its own
    passed: Vec<bool>,
    /// the first input that has not passed
    turn: usize,
    /// inputs reported so far (in order)
    printed: usize,
    /// --rm: an input passes only when it is completely done
    hold_turn: bool,
    running: usize,
    running_bytes: u64,
    /// bytes held for standard output
    held: usize,
    /// every change of state, for the panic hook
    changes: u64,
    panics: usize,
    reports: Vec<Option<Report>>,
}

static STATE: Mutex<State> = Mutex::new(State {
    on: false,
    names: Vec::new(),
    sizes: Vec::new(),
    st: Vec::new(),
    passed: Vec::new(),
    turn: 0,
    printed: 0,
    hold_turn: false,
    running: 0,
    running_bytes: 0,
    held: 0,
    changes: 0,
    panics: 0,
    reports: Vec::new(),
});
static CV: Condvar = Condvar::new();

thread_local! {
    /// The input this thread works on (NONE outside a batch).
    static JOB: Cell<usize> = const { Cell::new(NONE) };
    /// Its messages so far.
    static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// How long it waited for the inputs before it.
    static PARKED: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

fn lock() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn job() -> Option<usize> {
    let j = JOB.with(|j| j.get());
    (j < COORD).then_some(j)
}

/// Whether this thread works on one input of a batch.
pub fn in_batch() -> bool {
    job().is_some()
}

/// Print a message line to standard error: now, or in a batch with the input's other
/// messages, once every input before it is reported.
pub fn say(line: String) {
    if in_batch() {
        LOG.with(|l| l.borrow_mut().push(line));
    } else {
        eprintln!("{line}");
    }
}

/// How long this input waited for the inputs before it (not counted in its time).
pub fn parked() -> Duration {
    PARKED.with(|p| p.get())
}

fn set(g: &mut State, i: usize, st: St) {
    let was = g.st[i];
    if was == st {
        return;
    }
    if was == St::Running {
        g.running -= 1;
        g.running_bytes -= g.sizes[i];
    }
    if st == St::Running {
        g.running += 1;
        g.running_bytes += g.sizes[i];
    }
    g.st[i] = st;
    g.changes += 1;
    CV.notify_all();
}

/// Wait (parked) until every input before input i has passed.
fn wait_turn(mut g: MutexGuard<'static, State>, i: usize) -> MutexGuard<'static, State> {
    if g.turn < i {
        let t0 = Instant::now();
        set(&mut g, i, St::Parked);
        while g.turn < i {
            g = CV.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        set(&mut g, i, St::Running);
        PARKED.with(|p| p.set(p.get() + t0.elapsed()));
    }
    g
}

fn pass(g: &mut State, i: usize) {
    if !g.passed[i] {
        g.passed[i] = true;
        while g.turn < g.passed.len() && g.passed[g.turn] {
            g.turn += 1;
        }
        g.changes += 1;
        CV.notify_all();
    }
}

/// Run `f` (it names an output) in this input's turn: once every input before it has named
/// its own or failed (with --rm: is done). Outside a batch it runs at once.
pub fn in_turn<T>(f: impl FnOnce() -> T) -> T {
    let Some(i) = job() else { return f() };
    drop(wait_turn(lock(), i));
    let r = f();
    let mut g = lock();
    if !g.hold_turn {
        pass(&mut g, i);
    }
    r
}

/// Standard output for an input of a batch (-c): its bytes are written out in its turn and
/// held in memory before it (see the module notes). Bytes still held when it is dropped
/// (the input failed) are written in its turn, as a buffered writer does.
pub struct Out {
    i: usize,
    buf: Vec<u8>,
}

impl Out {
    pub fn new() -> Out {
        Out { i: job().unwrap_or(0), buf: Vec::new() }
    }

    /// Write out the collected bytes (in this input's turn).
    fn write_out(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        // waiting for a slow reader is waiting, not work (see on_panic)
        let i = self.i;
        set(&mut lock(), i, St::Parked);
        let r = io::stdout().lock().write_all(&self.buf);
        let n = self.buf.len();
        self.buf = Vec::new();
        let mut g = lock();
        g.held -= n;
        set(&mut g, i, St::Running);
        r
    }

    /// All bytes written out and flushed, in this input's turn; then the next input's turn.
    pub fn finish(mut self) -> io::Result<()> {
        drop(wait_turn(lock(), self.i));
        let r = self.write_out().and_then(|_| io::stdout().lock().flush());
        let mut g = lock();
        if !g.hold_turn {
            pass(&mut g, self.i);
        }
        r
    }
}

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.write_all(b)?;
        Ok(b.len())
    }

    fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        self.buf.extend_from_slice(b);
        let mut g = lock();
        g.held += b.len();
        if g.turn >= self.i {
            drop(g);
            return if self.buf.len() >= OUT_CHUNK { self.write_out() } else { Ok(()) };
        }
        if g.held <= HOLD_MAX {
            return Ok(());
        }
        // no more room to hold it: wait for the turn
        drop(wait_turn(g, self.i));
        self.write_out()
    }

    fn flush(&mut self) -> io::Result<()> {
        if lock().turn < self.i {
            return Ok(());
        }
        self.write_out()?;
        io::stdout().lock().flush()
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            drop(wait_turn(lock(), self.i));
            let _ = self.write_out();
        }
    }
}

/// Work on inputs 0..names.len() with `work` (input i's whole job; Err: its error message)
/// several at a time, as the module notes say. `sizes`: their bytes. `hold_turn` (--rm):
/// an input names its output only when the inputs before it are done. Returns whether any
/// input failed.
pub fn run(names: &[OsString], sizes: &[u64], threads: usize, hold_turn: bool, work: &(dyn Fn(usize) -> Result<(), String> + Sync)) -> bool {
    let n = names.len();
    {
        let mut g = lock();
        *g = State {
            on: true,
            names: names.to_vec(),
            sizes: sizes.to_vec(),
            st: vec![St::Queued; n],
            passed: vec![false; n],
            turn: 0,
            printed: 0,
            hold_turn,
            running: 0,
            running_bytes: 0,
            held: 0,
            changes: 0,
            panics: 0,
            reports: (0..n).map(|_| None).collect(),
        };
    }
    JOB.with(|j| j.set(COORD));
    let most = threads.clamp(1, WINDOW);
    let mut failed = false;
    std::thread::scope(|s| {
        let mut printed = 0;
        let mut g = lock();
        while printed < n {
            // report what is done, in input order
            if g.st[printed] == St::Done {
                let (lines, err) = g.reports[printed].take().unwrap_or_default();
                drop(g);
                for l in &lines {
                    eprintln!("{l}");
                }
                if let Some(e) = err {
                    eprintln!("fewer-pack: {}: {e}", names[printed].to_string_lossy());
                    failed = true;
                }
                printed += 1;
                g = lock();
                g.printed = printed;
                g.changes += 1;
                CV.notify_all();
                continue;
            }
            // start the biggest input that may start (after a panic: every one that may, so
            // that the inputs before the one that stopped get done; see on_panic)
            let free = g.panics > 0 || g.running < most;
            let pick = if free {
                let room = if g.panics > 0 || g.running == 0 { u64::MAX } else { BYTES.saturating_sub(g.running_bytes) };
                (printed..n.min(printed + WINDOW)).filter(|&i| g.st[i] == St::Queued && g.sizes[i] <= room).max_by(|&a, &b| g.sizes[a].cmp(&g.sizes[b]).then(b.cmp(&a)))
            } else {
                None
            };
            match pick {
                Some(i) => {
                    set(&mut g, i, St::Running);
                    s.spawn(move || run_one(i, work));
                }
                None => g = CV.wait(g).unwrap_or_else(|e| e.into_inner()),
            }
        }
        g.on = false;
    });
    JOB.with(|j| j.set(NONE));
    failed
}

/// The job thread of input i.
fn run_one(i: usize, work: &(dyn Fn(usize) -> Result<(), String> + Sync)) {
    JOB.with(|j| j.set(i));
    LOG.with(|l| l.borrow_mut().clear());
    PARKED.with(|p| p.set(Duration::ZERO));
    // (panic = "unwind" builds: a panic still ends the input, so the batch is not left waiting)
    struct End(usize, Option<Result<(), String>>);
    impl Drop for End {
        fn drop(&mut self) {
            let err = match self.1.take() {
                Some(r) => r.err(),
                None => Some("internal error".to_string()),
            };
            let lines = LOG.with(|l| std::mem::take(&mut *l.borrow_mut()));
            let mut g = lock();
            pass(&mut g, self.0);
            g.reports[self.0] = Some((lines, err));
            set(&mut g, self.0, St::Done);
        }
    }
    let mut end = End(i, None);
    end.1 = Some(work(i));
}

/// The panic hook's part (panic = "abort" builds), called first. Outside a batch: None (the
/// hook names the current input as before). In a batch it waits until the input that
/// stopped is known: this thread's input, or (a thread of the engine's) the only input
/// still running once the others are done or wait for it (an input after it cannot name its
/// output before it does). Meanwhile the inputs before it go on and are reported. Then it
/// returns that input's name; the hook removes the temporary outputs (those of the inputs
/// after it) and ends the program, as one input at a time would have. Two inputs stopped
/// (two panics, neither on an input's own thread): the first of them, once nothing has
/// changed for a while. Some(empty): no input to name.
#[cfg(panic = "abort")]
pub fn on_panic() -> Option<OsString> {
    const STALL: Duration = Duration::from_secs(20);
    let me = JOB.with(|j| j.get());
    if me == COORD {
        return Some(OsString::new());
    }
    let t0 = Instant::now();
    let mut counted = false;
    let mut seen = (u64::MAX, Instant::now());
    loop {
        match STATE.try_lock() {
            Ok(mut g) => {
                if !g.on {
                    return None;
                }
                if !counted {
                    // from now on the inputs that may start do (the ones before it must end)
                    g.panics += 1;
                    counted = true;
                    CV.notify_all();
                }
                if g.changes != seen.0 {
                    seen = (g.changes, Instant::now());
                }
                let running: Vec<usize> = (0..g.st.len()).filter(|&i| g.st[i] == St::Running).collect();
                let stopped = if me < g.st.len() {
                    Some(me)
                } else if running.len() == 1 || (g.panics > 1 && !running.is_empty() && seen.1.elapsed() > STALL) {
                    Some(running[0])
                } else {
                    None
                };
                if let Some(c) = stopped {
                    if g.printed >= c {
                        return Some(g.names[c].clone());
                    }
                }
            }
            // this thread may hold the lock itself (a panic inside this module)
            Err(_) if !counted && t0.elapsed() > Duration::from_secs(2) => return Some(OsString::new()),
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
