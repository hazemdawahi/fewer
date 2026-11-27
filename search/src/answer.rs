//! Answers: plain text built in order of importance under a size cap, every
//! shown line with its reference, and a last line that says "all shown" or what was left
//! out and the next call that gets it.
//!
//! - The cap is max_tokens × 4 bytes for everything but the last line. A line that would
//!   pass it is not added; the answer then says what was left out.
//! - References: `<pid>/r<run>:<line>` or `<pid>/r<run>:<first>-<last>`, lines counted from
//!   1 by the '\n' in the run's exact bytes.
//! - When a shown line differs from its bytes (escape codes set aside, a redraw, odd bytes
//!   as \xHH), the answer's first line says so and how to get the exact bytes.

use crate::clean;
use crate::json::Json;

/// Room kept inside the cap for the first line that says lines were shown cleaned.
const NOTE_ROOM: usize = 200;
/// The room that note needs in its short form ("shown cleaned; exact bytes: lines {...}"), used
/// when a line that must be there took some of NOTE_ROOM.
const SHORT_NOTE_ROOM: usize = 80;

pub struct Answer {
    cap: usize,
    /// max_tokens × 4 bytes: everything but the last line stays under it
    hard: usize,
    out: String,
    /// a line did not fit
    pub full: bool,
    /// the reference of the first shown line that differs from its bytes
    cleaned: Option<String>,
}

impl Answer {
    pub fn new(max_tokens: usize) -> Answer {
        Answer { cap: (max_tokens * 4).saturating_sub(NOTE_ROOM).max(200), hard: (max_tokens * 4).max(400), out: String::new(), full: false, cleaned: None }
    }

    /// Whether `n` more bytes (and a line end) fit.
    pub fn fits(&self, n: usize) -> bool {
        self.out.len() + n < self.cap
    }

    /// The bytes left under the cap.
    pub fn room(&self) -> usize {
        self.cap.saturating_sub(self.out.len())
    }

    /// Add a line when it fits (else note that the answer is full).
    pub fn line(&mut self, s: &str) -> bool {
        if !self.fits(s.len()) {
            self.full = true;
            return false;
        }
        self.out.push_str(s);
        self.out.push('\n');
        true
    }

    /// Add a line that must be there (a heading, an error), cut to the room left, its "…(+N
    /// bytes)" counted in it: at least 80 bytes while the answer stays under max_tokens × 4 with
    /// room for the cleaned note's short form (a long project path once took a heading 3 bytes
    /// past it).
    pub fn must(&mut self, s: &str) {
        let floor = 80.min(self.hard.saturating_sub(SHORT_NOTE_ROOM).saturating_sub(self.out.len() + 1));
        let room = self.cap.saturating_sub(self.out.len() + 1).max(floor);
        let tail = format!("…(+{} bytes)", group(s.len() as u64)).len();
        if s.len() > room && room < tail {
            // (no room even for its "…(+N bytes)": left out, the answer full)
            self.full = true;
            return;
        }
        if s.len() <= room {
            self.out.push_str(s);
        } else {
            let (t, n) = clean::cut(s, room.saturating_sub(tail));
            self.out.push_str(t);
            if n > 0 {
                self.out.push_str(&format!("…(+{} bytes)", group(n as u64)));
            }
        }
        self.out.push('\n');
    }

    /// Note that the line with this reference was shown cleaned.
    pub fn cleaned(&mut self, r: &str) {
        if self.cleaned.is_none() {
            self.cleaned = Some(r.to_string());
        }
    }

    /// The answer's text with its last line.
    pub fn finish(self, last: &str) -> String {
        let mut s = String::with_capacity(self.out.len() + last.len() + NOTE_ROOM);
        if let Some(r) = &self.cleaned {
            let call = Json::o(vec![("ref", r.as_str().into()), ("raw", true.into())]);
            let note = format!("shown cleaned (color codes set aside, redraws at their final state, odd bytes as \\xHH); exact bytes: lines {call}\n");
            // (the short form when a line that must be there took the long one's room)
            if note.len() + self.out.len() <= self.hard {
                s.push_str(&note);
            } else {
                s.push_str(&format!("shown cleaned; exact bytes: lines {call}\n"));
            }
        }
        s.push_str(&self.out);
        s.push_str(last);
        s
    }
}

/// A number with thousands separators: 1,234,567.
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

thread_local! {
    /// The project whose references an answer shows without it: the server's own project,
    /// while a tool call that names no project is answered (all its runs are that project's).
    static SHORT_FOR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// While the value lives, the references to `pid`'s runs that this thread makes leave the
/// project out: "r42:1201", which `lines`, the cursors and every tool read in the server's
/// project (the token test of 2026-11-23). None: whole references.
pub struct ShortRefs;

impl ShortRefs {
    pub fn new(pid: Option<String>) -> ShortRefs {
        SHORT_FOR.with(|s| *s.borrow_mut() = pid);
        ShortRefs
    }
}

impl Drop for ShortRefs {
    fn drop(&mut self) {
        SHORT_FOR.with(|s| *s.borrow_mut() = None);
    }
}

fn short(pid: &str) -> bool {
    SHORT_FOR.with(|s| s.borrow().as_deref() == Some(pid))
}

/// A run's reference: fewer-3fa2/r42 (r42 while ShortRefs names fewer-3fa2).
pub fn run_ref(pid: &str, run: u64) -> String {
    if short(pid) { format!("r{run}") } else { format!("{pid}/r{run}") }
}

/// A line's reference: fewer-3fa2/r42:1201 (r42:1201 while ShortRefs names fewer-3fa2).
pub fn line_ref(pid: &str, run: u64, line: u64) -> String {
    format!("{}:{line}", run_ref(pid, run))
}

/// A shown line: its cleaned text, cut at `line_max` bytes with "…(+N bytes)". `bytes` may
/// be the first part of a longer line of `len` bytes. Also whether it differs from its bytes
/// and whether it was cut.
pub fn shown_line(bytes: &[u8], len: usize, line_max: usize) -> (String, bool, bool) {
    let (s, changed) = clean::shown(bytes);
    let (t, cut) = clean::cut(&s, line_max);
    let more = cut + len.saturating_sub(bytes.len());
    if more == 0 {
        return (s, changed, false);
    }
    (format!("{t}…(+{} bytes)", group(more as u64)), changed, true)
}

/// A ratio with one decimal: 46.8x.
pub fn ratio(a: u64, b: u64) -> String {
    if b == 0 {
        return "-".into();
    }
    let r = (a as f64 / b as f64 * 10.0).round() / 10.0;
    format!("{r:.1}x")
}

/// The next call, ready to send: `search {"text":"timeout","cursor":"..."}`.
pub fn next_call(tool: &str, args: &Json) -> String {
    format!("{tool} {args}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_lines() {
        assert_eq!(group(0), "0");
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(group(100), "100");
        let mut a = Answer::new(200);
        let cap = a.cap;
        let mut n = 0;
        while a.line("0123456789") {
            n += 1;
        }
        assert!(a.full && a.out.len() <= cap && n == cap / 11);
        assert_eq!(shown_line(b"abcdef", 6, 3), ("abc…(+3 bytes)".into(), false, true));
        assert_eq!(shown_line(b"abc", 10, 400), ("abc…(+7 bytes)".into(), false, true));
        assert_eq!(shown_line(b"\x1b[1mab\r\n", 8, 400), ("ab".into(), true, false));
        assert_eq!(ratio(468, 10), "46.8x");
    }
}
