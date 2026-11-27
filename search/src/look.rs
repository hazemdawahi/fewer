//! `fewer look FILE... "QUESTION"`: pictures and scanned documents answered in words, for an
//! agent that has only a shell and so cannot see one. A Claude Code session with no
//! tools, no add-ons and a short prompt of its own gets the files in its first message and answers
//! the question from what they show, in words, in one step; the main session gets the words. (A
//! picture Claude Code reads itself stays in the conversation, read again at every later step.)

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::json::{self, Json};

pub const LOOK_HELP: &str = "\
fewer look FILE... \"QUESTION\" - pictures or documents, answered in words

Gives the files (pictures: png, jpg, gif, webp; documents: pdf) to a Claude Code session of one
step with no tools and no add-ons, which answers the question from what they show: the words and
numbers as shown, and what cannot be read said plainly. The files come first, then the question.
Its tokens are added to helpers.jsonl in the store (FEWER_HOME). claude is FEWER_CLAUDE_BIN,
else the claude on PATH.";

/// The kinds of file, by their ending, and how the API names them.
const KINDS: [(&str, &str); 6] = [("png", "image/png"), ("jpg", "image/jpeg"), ("jpeg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp"), ("pdf", "application/pdf")];
/// The largest picture and document Claude Code sends (bytes: a picture of 5 MB once in base64,
/// a PDF of 20 MB), all the files together in base64 (below the API's 32 MB a request), and the
/// most files at once.
const MAX_PICTURE: u64 = 3_932_160;
const MAX_DOCUMENT: u64 = 20 * 1024 * 1024;
const MAX_TOTAL: u64 = 30 * 1024 * 1024;
const MAX_FILES: usize = 20;
const LOOK_TIME: Duration = Duration::from_secs(300);
/// After Claude Code is stopped, the longest wait for what it wrote and for the message's writer.
const AFTER_STOP: Duration = Duration::from_secs(2);

const PROMPT: &str = "You answer one question about the pictures or documents in the message, from what they show. Give the words and numbers exactly as they are shown; say plainly what cannot be read or is not there. Answer briefly.";

fn base64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= c.len() { A[((n >> shift) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// The first message: each file as a picture or a document, then the question.
fn message(files: &[PathBuf], question: &str) -> Result<Json, String> {
    if files.len() > MAX_FILES {
        return Err(format!("{} files: {MAX_FILES} at most at once", files.len()));
    }
    let mut content = Vec::new();
    let mut total: u64 = 0;
    for f in files {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        let Some((_, mime)) = KINDS.iter().find(|(e, _)| *e == ext) else {
            return Err(format!("{}: not a picture or a PDF (png, jpg, gif, webp, pdf); read text with fewer read", f.display()));
        };
        let size = std::fs::metadata(f).map_err(|e| format!("{}: {e}", f.display()))?.len();
        let (kind, max) = if *mime == "application/pdf" { ("document", MAX_DOCUMENT) } else { ("image", MAX_PICTURE) };
        if size > max {
            return Err(format!("{}: {size} bytes, past the {max} bytes Claude Code sends for one {kind}", f.display()));
        }
        total += size.div_ceil(3) * 4;
        if total > MAX_TOTAL {
            return Err(format!("the files together are past the {MAX_TOTAL} bytes one message can carry: give fewer at once"));
        }
        let bytes = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        content.push(Json::o(vec![("type", kind.into()), ("source", Json::o(vec![("type", "base64".into()), ("media_type", (*mime).into()), ("data", base64(&bytes).into())]))]));
    }
    content.push(Json::o(vec![("type", "text".into()), ("text", question.into())]));
    Ok(Json::o(vec![("type", "user".into()), ("message", Json::o(vec![("role", "user".into()), ("content", Json::Arr(content))]))]))
}

/// The model the user chose (Claude Code's settings file in FEWER_CLAUDE_DIR), if any.
fn user_model() -> Option<String> {
    let d = crate::defaults::setting("FEWER_CLAUDE_DIR")?;
    let j = json::parse(&std::fs::read(Path::new(&d).join("settings.json")).ok()?).ok()?;
    j.get("model").and_then(Json::str).map(str::to_string)
}

/// The answer, from one step of a session that sees the files.
fn look(files: &[PathBuf], question: &str) -> Result<String, String> {
    let msg = message(files, question)?;
    let claude = crate::ondemand::claude()?;
    let mut cmd = Command::new(&claude);
    cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--max-turns", "1", "--setting-sources", "", "--no-session-persistence"]);
    cmd.args(["--tools", "", "--strict-mcp-config", "--system-prompt", PROMPT]);
    if let Some(m) = user_model() {
        cmd.args(["--model", &m]);
    }
    cmd.envs(crate::ondemand::SESSION_ENV).envs(crate::ondemand::HELPER_ENV);
    cmd.env("ENABLE_CLAUDEAI_MCP_SERVERS", "false").env("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS", "1");
    let dir = crate::store::home().ok().filter(|h| crate::store::make_home(h).is_ok()).unwrap_or_else(std::env::temp_dir);
    // (in a group of its own: at the deadline the whole group is stopped, a program it started
    // too, which would otherwise keep the pipes open)
    cmd.current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut p = cmd.spawn().map_err(|e| format!("cannot run {}: {e}", claude.display()))?;
    // (its output read from the start, so that a big message cannot wait on a full pipe)
    // (what Claude Code writes on stderr kept as it comes, at most 64 KB, so that after the stop
    // what came is said even when a program it left holds the pipe open; and its end signalled,
    // so that no wait is longer than AFTER_STOP)
    let mut err = p.stderr.take();
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let (etx, erx) = mpsc::channel::<()>();
    let eb = err_buf.clone();
    std::thread::spawn(move || {
        if let Some(e) = err.as_mut() {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = e.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut b) = eb.lock() {
                    let room = (64 * 1024usize).saturating_sub(b.len());
                    b.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let _ = etx.send(());
    });
    let out = p.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // (the message written on a thread of its own: a session that does not read it cannot hold
    // `look` past its time)
    let mut stdin = p.stdin.take().ok_or("no stdin")?;
    let text = msg.to_string();
    let (wtx, wrx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = writeln!(stdin, "{text}").and_then(|()| stdin.flush());
        drop(stdin);
        let _ = wtx.send(r);
    });
    let deadline = Instant::now() + LOOK_TIME;
    let mut result = None;
    // (Claude Code's end watched too: once it has ended, what it wrote comes within AFTER_STOP, and
    // a program it left holding its output keeps look no longer)
    let mut ended: Option<Instant> = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let wait = left.min(Duration::from_millis(250));
        match rx.recv_timeout(wait) {
            Ok(line) => {
                if let Ok(m) = json::parse(line.as_bytes())
                    && m.get("type").and_then(Json::str) == Some("result")
                {
                    result = Some(m);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if ended.is_none() && matches!(p.try_wait(), Ok(Some(_))) {
            ended = Some(Instant::now());
        }
        if ended.is_some_and(|t| t.elapsed() >= AFTER_STOP) {
            break;
        }
    }
    let late = result.is_none() && Instant::now() >= deadline;
    crate::sys_run::send_signal(-(p.id() as i32), crate::sys_run::SIGKILL);
    let _ = p.kill();
    let _ = p.wait();
    let _ = erx.recv_timeout(AFTER_STOP);
    let errs: Vec<u8> = err_buf.lock().map(|b| b.clone()).unwrap_or_default();
    let wrote = wrx.recv_timeout(AFTER_STOP).unwrap_or_else(|_| Err(std::io::Error::other("the message was not taken")));
    if late {
        return Err(format!("Claude Code gave no answer within {} seconds", LOOK_TIME.as_secs()));
    }
    let Some(r) = result else {
        // (what Claude Code said first: a refused message's broken pipe only when it said
        // nothing: "Invalid API key" was hidden behind it)
        // (each line cut: a crash of a bundle prints its whole source line)
        let said: String = String::from_utf8_lossy(&errs).lines().filter(|l| !l.trim().is_empty()).take(4).map(|l| l.chars().take(300).collect::<String>()).collect::<Vec<_>>().join(" | ");
        if said.is_empty() {
            wrote.map_err(|e| format!("could not give the files to Claude Code: {e}"))?;
        }
        return Err(format!("Claude Code gave no answer{}", if said.is_empty() { String::new() } else { format!(": {said}") }));
    };
    let text = r.get("result").and_then(Json::str).unwrap_or("").trim().to_string();
    let ok = r.get("is_error").and_then(Json::bool) != Some(true) && !text.is_empty();
    crate::ondemand::log_usage(&crate::ondemand::usage_line("look", &r, ok, &crate::ondemand::parent_session()));
    if ok { Ok(text) } else { Err(format!("Claude Code said: {}", if text.is_empty() { r.get("subtype").and_then(Json::str).unwrap_or("no answer").to_string() } else { text })) }
}

pub fn cmd_look(args: Vec<OsString>) -> i32 {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if matches!(words.as_slice(), [h] if h == "-h" || h == "--help") {
        println!("{LOOK_HELP}");
        return 0;
    }
    // the files first (each an existing file), then the question
    let n = words.iter().take_while(|w| Path::new(w).is_file()).count();
    let (files, rest) = words.split_at(n);
    if files.is_empty() || rest.is_empty() {
        eprintln!("usage: fewer look FILE... \"QUESTION\"");
        return 2;
    }
    let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    // (Claude Code's Read rules, which never see a file read through Bash: perms.rs)
    let cwd = std::env::current_dir().unwrap_or_default();
    let rules = crate::perms::Rules::load(&cwd);
    let refused: Vec<String> = files.iter().filter_map(|f| crate::perms::refused_read(&f.display().to_string(), &rules.read(f, &cwd))).collect();
    if !refused.is_empty() {
        for line in &refused {
            println!("{line}");
        }
        return 1;
    }
    match look(&files, &rest.join(" ")) {
        Ok(a) => {
            println!("{a}");
            0
        }
        Err(e) => {
            eprintln!("fewer look: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_as_the_api_reads_it() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn files_then_the_question() {
        let d = std::env::temp_dir().join(format!("fewer-look-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let png = d.join("a.png");
        std::fs::write(&png, b"\x89PNG").unwrap();
        let m = message(std::slice::from_ref(&png), "what does it say").unwrap();
        let c = m.get("message").and_then(|x| x.get("content")).and_then(Json::arr).unwrap();
        assert_eq!(c[0].get("source").and_then(|s| s.get("media_type")).and_then(Json::str), Some("image/png"));
        assert_eq!(c[0].get("source").and_then(|s| s.get("data")).and_then(Json::str), Some("iVBORw=="));
        assert_eq!(c[1].get("text").and_then(Json::str), Some("what does it say"));
        let txt = d.join("a.txt");
        std::fs::write(&txt, "x").unwrap();
        assert!(message(&[txt], "q").unwrap_err().contains("not a picture or a PDF"));
        // a picture past what Claude Code sends; pictures past one message together
        let big = d.join("big.png");
        std::fs::write(&big, vec![0u8; MAX_PICTURE as usize + 1]).unwrap();
        assert!(message(std::slice::from_ref(&big), "q").err().is_some_and(|e| e.contains("bytes Claude Code sends for one image")));
        let ok = d.join("ok.png");
        std::fs::write(&ok, vec![0u8; MAX_PICTURE as usize]).unwrap();
        // (6 of the largest are 30 MB in base64, what one message carries; 7 are past it)
        assert!(message(&vec![ok.clone(); 7], "q").err().is_some_and(|e| e.contains("one message can carry")));
        assert!(message(&vec![ok; 6], "q").is_ok());
        let _ = std::fs::remove_dir_all(d);
    }
}
