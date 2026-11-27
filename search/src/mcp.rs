//! The MCP server: `fewer mcp`, JSON-RPC 2.0 on stdin and stdout, one
//! JSON message per line.
//!
//! - Stdout guard: first thing, a copy of stdout is kept for the messages and stdout itself
//!   is pointed at stderr, so a stray print or a child process can never break the stream.
//!   Messages go out through one locked writer, each on one line (our writer escapes every
//!   control byte).
//! - A reader thread reads lines of at most 16 MiB (a longer one is skipped to its '\n' and
//!   answered with -32600), parses them and queues them; it acts on
//!   notifications/cancelled at once. The main thread answers the messages one at a time,
//!   in order. At the end of stdin the server finishes and exits 0.
//! - A tool runs under catch_unwind: a panic becomes an isError answer and the server goes
//!   on.
//! - While sessions are on, a thread saves agents' sessions at the start and every 60 s
//!   (sessions::Sweeper): the server's only writes. It stops when serve ends.
//! - While the DNA index is on, a thread builds it for the big runs left without one, while
//!   the computer is idle (dna_build::IdleBuilder). With the sessions sweep,
//!   it is the server's only writer; no tool reaches either. Both stop when serve ends.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::json::{self, Json};
use crate::tools;

/// The protocol versions we know, oldest first; the newest is answered to any other.
const VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// Longest message line read.
const LINE_MAX: usize = 16 << 20;

const INSTRUCTIONS: &str = "fewer keeps this developer's logs and command outputs, exact and searchable.\n\
When debugging, check fewer first: errors (error groups), compare (what changed since a passing run), odd (new and rare lines), quiet (what stopped logging), guard (the latest moments: new keys, jumps, error bursts, failed runs, errors back), fixes (has this error happened before, and what came right before it stopped); search for a text, count it, read around a hit with lines, follow an ID across logs.\n\
Counts are exact; every line has a reference (fewer-3fa2/r42:1201) that lines expands or checks byte for byte.\n\
Run tests, builds and other commands with long output as `fewer run --short -- COMMAND`: the whole output is saved as a run, and you get a short view in its place (the exit status, each different error, failure and exception line with its line number, the warnings in brief, the last lines); read more of it with search, errors or lines on that run (from a shell: fewer tool lines r2:1-40). With fewer's token savers installed, a long output of a build, test, lint or install command that succeeds (or a log) also comes back as that view by itself; a command that fails comes back whole (Claude Code does not let a hook change it), so run tests and builds through fewer run --short. Put FEWER_FULL=1 in front of a command to get its output whole.\n\
At the start of a fresh session, or before re-reading what an earlier one did, call history (no input: the start card); start fresh sessions often, history keeps what matters.\n\
Before reading a big file whole, ask outline for its functions, classes or sections with their line ranges, then read only the range you need.\n\
In answers: a line like one shown just before it in the same run comes as ≈ N and its own words (· for each word as in line N, the same end left out), or = N for the same line (compact: false shows lines whole); a Claude Code session's lines read as who said or did what (lines with raw: true shows the JSON). Most tools take a scope: project, runs (r42, last, last:TEXT), since and until (2026-11-18 10:00, 15m, 2h, today), kind (logs by default; runs, sessions, all), command, branch, commit; and max_tokens (the answer's cap, default 3000) and time_limit_ms (default 10000).";

/// A message from the reader thread.
enum Msg {
    Value(Json),
    /// not UTF-8, not JSON or nested too deep: -32700
    ParseError(String),
    /// a line over LINE_MAX: -32600
    TooLong,
    End,
}

/// What the reader thread and the main thread share: the request being answered (its id's
/// JSON text and stop flag) and the ids cancelled before they started.
struct Shared {
    current: Mutex<Option<(String, Arc<AtomicBool>)>>,
    cancelled: Mutex<HashSet<String>>,
}

/// The debug log (FEWER_DEBUG_LOG), when set. Each line is written with one call: lines of
/// several threads and processes never mix.
pub fn debug(msg: &str) {
    if let Some(p) = std::env::var_os("FEWER_DEBUG_LOG").filter(|p| !p.is_empty())
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
    {
        let line = format!("{} [{}] {msg}\n", crate::times::fmt_local(crate::times::now_ms()), std::process::id());
        let _ = f.write_all(line.as_bytes());
    }
}

/// The stdout guard: messages go to a copy of stdout; stdout itself becomes stderr (so a
/// stray print, ours or a program's, never breaks the messages). The copy, or the exit
/// status after saying why not.
#[cfg(unix)]
fn guard_stdout() -> Result<File, i32> {
    use std::os::fd::FromRawFd;
    let out_fd = match crate::sys::dup_cloexec(1) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("fewer mcp: cannot keep stdout: {e}");
            return Err(1);
        }
    };
    if let Err(e) = crate::sys::dup_to(2, 1) {
        eprintln!("fewer mcp: cannot guard stdout: {e}");
        return Err(1);
    }
    // SAFETY: out_fd is a new descriptor that only this File owns
    Ok(unsafe { File::from_raw_fd(out_fd) })
}

/// The stdout guard on Windows: the standard output handle is kept for the messages, and
/// the standard error handle takes its place (for our prints and the programs we start).
#[cfg(windows)]
fn guard_stdout() -> Result<File, i32> {
    use std::os::windows::io::{FromRawHandle, RawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> RawHandle;
        fn SetStdHandle(which: u32, h: RawHandle) -> i32;
    }
    const STD_OUTPUT: u32 = -11i32 as u32;
    const STD_ERROR: u32 = -12i32 as u32;
    // SAFETY: reads and sets this process's standard handles; the File takes the output
    // handle, which nothing else uses from here on
    unsafe {
        let out = GetStdHandle(STD_OUTPUT);
        if out.is_null() || out as isize == -1 {
            eprintln!("fewer mcp: cannot keep stdout: {}", io::Error::last_os_error());
            return Err(1);
        }
        if SetStdHandle(STD_OUTPUT, GetStdHandle(STD_ERROR)) == 0 {
            eprintln!("fewer mcp: cannot guard stdout: {}", io::Error::last_os_error());
            return Err(1);
        }
        Ok(File::from_raw_handle(out))
    }
}

pub fn serve() -> i32 {
    let out = match guard_stdout() {
        Ok(f) => Mutex::new(BufWriter::with_capacity(1 << 16, f)),
        Err(code) => return code,
    };
    let ctx = tools::Ctx::new();
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let home = match &ctx.home {
        Ok(h) => h.display().to_string(),
        Err(e) => format!("(none: {e})"),
    };
    debug(&format!("start: cwd {cwd}, project root {}, store {home}", ctx.root.display()));
    // the sessions sweep, stopped when it is dropped at the end of serve
    let _sweeper = ctx.home.as_ref().ok().map(|h| crate::sessions::Sweeper::start(h.clone(), Box::new(debug)));
    // the DNA index's idle builder, stopped when it is dropped at the end of serve
    let _dna = ctx.home.as_ref().ok().map(|h| crate::dna_build::IdleBuilder::start(h.clone()));
    let shared = Arc::new(Shared { current: Mutex::new(None), cancelled: Mutex::new(HashSet::new()) });
    let (tx, rx) = mpsc::channel::<Msg>();
    let sh = shared.clone();
    std::thread::spawn(move || reader(tx, sh));
    for msg in rx {
        let reply = match msg {
            Msg::End => break,
            Msg::TooLong => Some(error(Json::Null, -32600, "the message is longer than 16 MiB")),
            Msg::ParseError(e) => Some(error(Json::Null, -32700, &format!("parse error: {e}"))),
            Msg::Value(Json::Arr(items)) => {
                if items.is_empty() {
                    Some(error(Json::Null, -32600, "an empty batch"))
                } else {
                    let replies: Vec<Json> = items.iter().filter_map(|m| handle(&ctx, m, &shared)).collect();
                    (!replies.is_empty()).then_some(Json::Arr(replies))
                }
            }
            Msg::Value(v) => handle(&ctx, &v, &shared),
        };
        if let Some(r) = reply
            && send(&out, &r).is_err()
        {
            debug("stdout closed: ending");
            return 0;
        }
    }
    debug("end of stdin: ending");
    0
}

fn send(out: &Mutex<BufWriter<File>>, v: &Json) -> io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
    w.write_all(s.as_bytes())?;
    w.flush()
}

/// One line from `r`: Ok(Some(true)) a line in `buf` (without its '\n'), Ok(Some(false)) a
/// line over `max` (skipped), Ok(None) the end.
fn read_line(r: &mut impl BufRead, max: usize, buf: &mut Vec<u8>) -> io::Result<Option<bool>> {
    buf.clear();
    let mut over = false;
    let mut any = false;
    loop {
        let avail = match r.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(if !any { None } else { Some(!over) });
        }
        any = true;
        let (take, done) = match avail.iter().position(|&c| c == b'\n') {
            Some(i) => (i, true),
            None => (avail.len(), false),
        };
        if !over {
            if buf.len() + take > max {
                over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&avail[..take]);
            }
        }
        r.consume(take + done as usize);
        if done {
            return Ok(Some(!over));
        }
    }
}

fn reader(tx: mpsc::Sender<Msg>, sh: Arc<Shared>) {
    let stdin = io::stdin();
    let mut r = io::BufReader::with_capacity(1 << 20, stdin.lock());
    let mut buf = Vec::new();
    loop {
        let m = match read_line(&mut r, LINE_MAX, &mut buf) {
            Ok(None) | Err(_) => {
                let _ = tx.send(Msg::End);
                return;
            }
            Ok(Some(false)) => Msg::TooLong,
            Ok(Some(true)) => {
                if buf.iter().all(|c| c.is_ascii_whitespace()) {
                    continue;
                }
                if let Err(e) = std::str::from_utf8(&buf) {
                    Msg::ParseError(format!("not UTF-8 at byte {}", e.valid_up_to()))
                } else {
                    match json::parse(&buf) {
                        Err(e) => Msg::ParseError(e),
                        Ok(v) => {
                            if v.get("method").and_then(Json::str) == Some("notifications/cancelled") && v.get("id").is_none() {
                                cancel(&v, &sh);
                                continue;
                            }
                            Msg::Value(v)
                        }
                    }
                }
            }
        };
        if tx.send(m).is_err() {
            return;
        }
    }
}

/// notifications/cancelled: the request in progress stops (and gets no answer); a request
/// still waiting is dropped when its turn comes.
fn cancel(v: &Json, sh: &Shared) {
    let Some(id) = v.get("params").and_then(|p| p.get("requestId")) else { return };
    let key = id.to_string();
    debug(&format!("cancelled: {key}"));
    if let Some((cur, flag)) = sh.current.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && *cur == key
    {
        flag.store(true, Ordering::SeqCst);
        return;
    }
    let mut c = sh.cancelled.lock().unwrap_or_else(|e| e.into_inner());
    if c.len() > 10_000 {
        c.clear();
    }
    c.insert(key);
}

fn error(id: Json, code: i64, msg: &str) -> Json {
    Json::o(vec![("jsonrpc", "2.0".into()), ("id", id), ("error", Json::o(vec![("code", Json::from(code)), ("message", msg.into())]))])
}

fn result(id: Json, r: Json) -> Json {
    Json::o(vec![("jsonrpc", "2.0".into()), ("id", id), ("result", r)])
}

/// The answer to one message (None for a notification or a response).
fn handle(ctx: &tools::Ctx, v: &Json, sh: &Shared) -> Option<Json> {
    if !matches!(v, Json::Obj(_)) {
        return Some(error(Json::Null, -32600, "a message must be a JSON object"));
    }
    let id = v.get("id");
    // an id must be a string or a number (MCP: never null)
    let good_id = id.filter(|i| matches!(i, Json::Str(_) | Json::Num(_))).cloned();
    if v.get("jsonrpc").and_then(Json::str) != Some("2.0") {
        return Some(error(good_id.unwrap_or(Json::Null), -32600, "not JSON-RPC 2.0 (\"jsonrpc\": \"2.0\" is missing)"));
    }
    let Some(method) = v.get("method").and_then(Json::str) else {
        // a response from the client (we send no requests): nothing to answer
        if id.is_some() && (v.get("result").is_some() || v.get("error").is_some()) {
            return None;
        }
        return Some(error(good_id.unwrap_or(Json::Null), -32600, "the message has no method"));
    };
    if id.is_none() {
        // a notification: notifications/initialized and the others need nothing
        debug(&format!("notification {method}"));
        return None;
    }
    let Some(id) = good_id else {
        return Some(error(Json::Null, -32600, "the id must be a string or a number"));
    };
    let key = id.to_string();
    if sh.cancelled.lock().unwrap_or_else(|e| e.into_inner()).remove(&key) {
        debug(&format!("{method} {key}: cancelled before it started"));
        return None;
    }
    let params = v.get("params");
    match method {
        "initialize" => {
            let asked = params.and_then(|p| p.get("protocolVersion")).and_then(Json::str).unwrap_or("");
            let ver = if VERSIONS.contains(&asked) { asked } else { VERSIONS[VERSIONS.len() - 1] };
            debug(&format!("initialize: client asked {asked:?}, answered {ver}"));
            Some(result(
                id,
                Json::o(vec![
                    ("protocolVersion", ver.into()),
                    ("capabilities", Json::o(vec![("tools", Json::o(vec![("listChanged", false.into())]))])),
                    ("serverInfo", Json::o(vec![("name", "fewer".into()), ("version", env!("CARGO_PKG_VERSION").into())])),
                    ("instructions", INSTRUCTIONS.into()),
                ]),
            ))
        }
        "ping" => Some(result(id, Json::Obj(Vec::new()))),
        "tools/list" => Some(result(id, tools::list())),
        "tools/call" => {
            let Some(name) = params.and_then(|p| p.get("name")).and_then(Json::str) else {
                return Some(error(id, -32602, "params.name (the tool's name) is required"));
            };
            let null = Json::Null;
            let args = params.and_then(|p| p.get("arguments")).unwrap_or(&null);
            let stop = Arc::new(AtomicBool::new(false));
            *sh.current.lock().unwrap_or_else(|e| e.into_inner()) = Some((key.clone(), stop.clone()));
            let t0 = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tools::call(ctx, name, args, &stop)));
            *sh.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
            debug(&format!("tools/call {name} {args}: {} ms", t0.elapsed().as_millis()));
            if stop.load(Ordering::SeqCst) {
                debug(&format!("tools/call {name} {key}: cancelled; no answer"));
                return None;
            }
            let content = |text: String, is_error: bool| {
                Json::o(vec![("content", Json::Arr(vec![Json::o(vec![("type", "text".into()), ("text", text.into())])])), ("isError", is_error.into())])
            };
            Some(match r {
                Ok(Ok((text, is_error))) => result(id, content(text, is_error)),
                Ok(Err(reason)) => error(id, -32602, &reason),
                Err(p) => {
                    let m = crate::find::panic_text(&p);
                    debug(&format!("tools/call {name}: panic: {m}"));
                    result(id, content(format!("internal error in search: {m}"), true))
                }
            })
        }
        _ => Some(error(id, -32601, &format!("method not found: {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lines_with_a_limit() {
        let data = b"{\"a\":1}\nxxxxxxxxxxxxxxxxxxxx\n\nlast".to_vec();
        let mut r = io::BufReader::with_capacity(4, &data[..]);
        let mut buf = Vec::new();
        assert_eq!(read_line(&mut r, 10, &mut buf).unwrap(), Some(true));
        assert_eq!(buf, b"{\"a\":1}");
        assert_eq!(read_line(&mut r, 10, &mut buf).unwrap(), Some(false));
        assert_eq!(read_line(&mut r, 10, &mut buf).unwrap(), Some(true));
        assert_eq!(buf, b"");
        assert_eq!(read_line(&mut r, 10, &mut buf).unwrap(), Some(true));
        assert_eq!(buf, b"last");
        assert_eq!(read_line(&mut r, 10, &mut buf).unwrap(), None);
    }

    #[test]
    fn answers_messages() {
        let ctx = tools::Ctx { home: Err("FEWER_HOME is not set".into()), root: "/nowhere".into() };
        let sh = Shared { current: Mutex::new(None), cancelled: Mutex::new(HashSet::new()) };
        let ask = |s: &str| handle(&ctx, &json::parse(s.as_bytes()).unwrap(), &sh).map(|v| v.to_string());
        let init = ask(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).unwrap();
        assert!(init.contains("\"protocolVersion\":\"2025-06-18\"") && init.contains("\"listChanged\":false"));
        let init = ask(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#).unwrap();
        assert!(init.contains("\"protocolVersion\":\"2025-11-25\""));
        assert_eq!(ask(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        assert_eq!(ask(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#).unwrap(), r#"{"jsonrpc":"2.0","id":"p","result":{}}"#);
        assert!(ask(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#).unwrap().contains("-32601"));
        assert!(ask(r#"{"id":3,"method":"ping"}"#).unwrap().contains("-32600"));
        assert!(ask(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap().contains("-32600"));
        assert!(ask(r#"{"jsonrpc":"2.0","id":4}"#).unwrap().contains("-32600"));
        assert_eq!(ask(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#), None);
        assert!(ask(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#).unwrap().contains("-32602"));
        assert!(ask(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search","arguments":{"txt":"x"}}}"#).unwrap().contains("unknown field 'txt'"));
        let r = ask(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search","arguments":{"text":"x"}}}"#).unwrap();
        assert!(r.contains("\"isError\":true") && r.contains("FEWER_HOME"), "{r}");
        let l = ask(r#"{"jsonrpc":"2.0","id":8,"method":"tools/list"}"#).unwrap();
        assert!(l.contains("\"name\":\"search\"") && l.contains("\"inputSchema\""));
    }
}
