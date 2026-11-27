//! Agents' sessions: the conversations of Claude Code, Codex CLI, Aider, Cline
//! and Cursor, saved into the memory store with secrets always hidden, so Claude can search
//! them later (`kind: "sessions"`) and the fix memory can name the files a session edited.
//!
//! `fewer sessions sync` saves what is new now. The MCP server runs the same sweep at
//! its start and every 60 s while it runs ([`Sweeper`]). It is an opt-in: unless the store's
//! config says `sessions = on`, nothing of any agent is read, not even a folder listed; with
//! it on, `sessions.<agent> = off` leaves that agent's files unread.
//!
//! Where each agent keeps its sessions ([`Places`]): the folder set for it ([`SETTINGS`]), else
//! its own folder where it is (defaults.rs: ~/.claude or $CLAUDE_CONFIG_DIR, ~/.codex or
//! $CODEX_HOME, the editors' storage). In development (FEWER_DEV, the checks) only the setting,
//! so that no test, check or development run ever reads a developer's real sessions; an agent
//! without its folder is passed over with a note.
//! - claude: `$FEWER_CLAUDE_DIR` (Claude Code's folder, ~/.claude): every `projects/*/*.jsonl`,
//!   JSON lines, one file per session, named by its id; and its helpers' transcripts, every
//!   `agent-*.jsonl` up to 4 folders below the session's own folder `projects/*/<session id>/`
//!   (`subagents/` for the Agent tool, deeper for workflow runs), each a piece of that session
//!   (helpers can spend most of a session's tokens, and their work is the session's); other files there (workflow journals) are passed over;
//! - codex: `$FEWER_CODEX_DIR` (Codex's folder, ~/.codex): every `sessions/**/*.jsonl`, JSON
//!   lines;
//! - aider: `$FEWER_AIDER_DIR` (a project's root, where Aider keeps it): its
//!   `.aider.chat.history.md` (text);
//! - cline: `$FEWER_CLINE_DIR` (VS Code's `globalStorage/saoudrizwan.claude-dev`): every
//!   `tasks/*/api_conversation_history.json`, one JSON array per task;
//! - cursor: `$FEWER_CURSOR_DIR` (Cursor's `User/globalStorage`): `state.vscdb`, read by the
//!   installed `sqlite3`, read-only, with the query [`CURSOR_QUERY`].
//!
//! How a source is saved, one run (kind session) per new piece:
//! - JSON lines and text (claude, codex, aider) by the rule of 7.2: a source seen before saves
//!   only its new tail when it still starts with what was saved (the size and our 128-bit hash
//!   of the bytes read before), as a run that `continues` the piece before; a source that
//!   shrank or changed is saved whole again. Only whole lines are saved: a line still being
//!   written waits for the next sweep.
//! - Cline's task document is rewritten whole as the task grows, so as a file it would never
//!   keep its start: it is read as one element of its array per line (each element's exact
//!   bytes), and that text follows the same rule.
//! - Cursor's rows: sqlite3 prints one row per line; a row is saved the first time it is seen
//!   with these exact bytes (a row changed in place is saved again), so a database that
//!   changes saves only its new and changed rows. Our hash of each row saved is kept in the
//!   project's `cursor.seen`, with the database's stamp (its size and time) when last read.
//! - Secrets are hidden in every piece (secrets.rs) as in one pass over the whole source: the
//!   hider of a tail first reads the 1,000 lines before it, as far as a private key's lines
//!   are carried. offset, size and hash stay those of the source as read.
//! - A session's project: the git root of the working folder its lines name (Claude's `cwd`,
//!   Codex's `payload.cwd`), else that folder, as `run` finds a command's;
//!   Aider's: the project whose root is its folder; Cline's and Cursor's, whose files name no
//!   folder: a project whose root is the agent's own folder. A source keeps the project of its
//!   first piece.
//! - Each piece is packed by the fewer-pack binary from a snapshot in $FEWER_HOME/tmp (as `add`
//!   does), then its .idx, then its catalog (catalog::index_run), which also gets the files a
//!   Claude session edited: the `file_path` or `notebook_path` of each Edit, Write, MultiEdit
//!   and NotebookEdit call, with the line of the call.
//!
//! One sweep at a time (raw/sessions.lock). A session run that a stopped sweep left
//! "packing" is marked failed by the next sweep, which saves its part again.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::add::{self, Hash128};
use crate::json::{self, Json};
use crate::secrets::{self, Extra, Hider};
use crate::store::{self, Project, RunRec};
use crate::{catalog, config, grep};

/// The agents, in the order a sweep reads them (runs.jsonl's `agent`).
pub const AGENTS: [&str; 5] = ["claude", "codex", "aider", "cline", "cursor"];

/// The rows of Cursor's database that hold its chats.
pub const CURSOR_QUERY: &str = "SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%' OR key LIKE 'bubbleId:%'";

/// Claude Code's tools that edit a file, and the input field that names the file.
const EDIT_TOOLS: [(&str, &str); 4] = [("Edit", "file_path"), ("MultiEdit", "file_path"), ("Write", "file_path"), ("NotebookEdit", "notebook_path")];

/// A Cline task file is read whole; a bigger one is passed over with a note.
const DOC_MAX: u64 = 64 << 20;

/// How often the MCP server sweeps, in ms.
const EVERY_MS: u64 = 60_000;

/// The file of a Cursor project that keeps the rows saved.
pub const SEEN: &str = "cursor.seen";

/// The note of a session run whose sweep stopped before it was saved.
const STOPPED: &str = "saving stopped before it was done; the next sweep saves this part again";

const USAGE: &str = "usage: fewer sessions sync";

// ---------------------------------------------------------------- settings and places

/// Whether a setting says on or off (any case); None for anything else.
fn on_off(v: &str) -> Option<bool> {
    if v.eq_ignore_ascii_case("on") {
        Some(true)
    } else if v.eq_ignore_ascii_case("off") {
        Some(false)
    } else {
        None
    }
}

/// The agents whose sessions are saved: none unless `sessions = on`; then each
/// one unless `sessions.<agent> = off`. A value that is neither on nor off counts as off
/// (saving conversations is an opt-in), with a note.
pub fn agents_on(settings: &[(String, String)], notes: &mut Vec<String>) -> Vec<&'static str> {
    let mut on = |key: &str, unset: bool| match config::get(settings, key) {
        None => unset,
        Some(v) => on_off(v).unwrap_or_else(|| {
            notes.push(format!("the config says `{key} = {v}`: neither on nor off, so off"));
            false
        }),
    };
    if !on("sessions", false) {
        return Vec::new();
    }
    AGENTS.iter().copied().filter(|a| on(&format!("sessions.{a}"), true)).collect()
}

/// Each agent's setting, in the order of [`AGENTS`]: the folder its sessions are read from.
/// Unset, each is the agent's usual folder where there is one, except in development
/// (defaults.rs).
pub const SETTINGS: [(&str, &str); 5] = [("claude", "FEWER_CLAUDE_DIR"), ("codex", "FEWER_CODEX_DIR"), ("aider", "FEWER_AIDER_DIR"), ("cline", "FEWER_CLINE_DIR"), ("cursor", "FEWER_CURSOR_DIR")];

/// An agent's name as people know it, for messages.
pub fn agent_name(agent: &str) -> &'static str {
    match agent {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "aider" => "Aider",
        "cline" => "Cline",
        _ => "Cursor",
    }
}

/// What a sweep reads in an agent's folder (the module's doc), for messages.
pub fn read_in(agent: &str, dir: &Path) -> PathBuf {
    match agent {
        "claude" => dir.join("projects"),
        "codex" => dir.join("sessions"),
        "aider" => dir.join(".aider.chat.history.md"),
        "cline" => dir.join("tasks"),
        _ => dir.join("state.vscdb"),
    }
}

/// Where the agents keep their sessions: each agent's folder, or why it is not known.
#[derive(Clone, Debug)]
pub struct Places {
    /// Claude Code's data folder (its sessions are in projects/)
    pub claude: Result<PathBuf, String>,
    /// Codex's folder (its sessions are in sessions/)
    pub codex: Result<PathBuf, String>,
    /// the folder holding Aider's .aider.chat.history.md (a project's root)
    pub aider: Result<PathBuf, String>,
    /// Cline's folder in VS Code's globalStorage (its tasks are in tasks/)
    pub cline: Result<PathBuf, String>,
    /// Cursor's globalStorage folder (state.vscdb is in it)
    pub cursor: Result<PathBuf, String>,
}

impl Places {
    /// The places from the environment: each agent's own setting ([`SETTINGS`]), else its usual
    /// folder where it is (in development only the settings).
    pub fn from_env() -> Places {
        Places::with(&|k| crate::defaults::setting(k))
    }

    /// The places from the settings `get` gives.
    pub fn with(get: &dyn Fn(&str) -> Option<OsString>) -> Places {
        // a setting must name an absolute folder; without one the agent is passed over
        let set = |agent: &str| {
            let k = SETTINGS.iter().find(|s| s.0 == agent).map_or("?", |s| s.1);
            match get(k) {
                Some(v) => {
                    let p = PathBuf::from(v);
                    if p.is_absolute() { Ok(p) } else { Err(format!("{k} is not an absolute path")) }
                }
                None if crate::defaults::dev() => Err(format!("{k} is not set (in development, an agent's sessions are read only from the folder set for it)")),
                None => Err(format!("{k} is not set, and the agent's usual folder is not there")),
            }
        };
        Places { claude: set("claude"), codex: set("codex"), aider: set("aider"), cline: set("cline"), cursor: set("cursor") }
    }

    /// One agent's folder, or why it is not known.
    pub fn of(&self, agent: &str) -> &Result<PathBuf, String> {
        match agent {
            "claude" => &self.claude,
            "codex" => &self.codex,
            "aider" => &self.aider,
            "cline" => &self.cline,
            _ => &self.cursor,
        }
    }
}

/// The config's extra secret shapes (`secret = PREFIX CLASS MIN`); a line that cannot be read
/// gets a note and is not used.
fn extra_secrets(settings: &[(String, String)], notes: &mut Vec<String>) -> Vec<Extra> {
    let mut v = Vec::new();
    for (k, val) in settings {
        if k == "secret" {
            match Extra::parse(val) {
                Ok(x) => v.push(x),
                Err(e) => notes.push(format!("config line \"secret = {val}\": {e}; this shape is not used")),
            }
        }
    }
    v
}

// ---------------------------------------------------------------- the sources

/// How a source's text is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// a file that grows by lines (JSON lines, text)
    Lines,
    /// a JSON array rewritten whole (Cline's task), read as one element per line
    Document,
    /// a database's rows, read by sqlite3 (Cursor)
    Rows,
}

/// A session source on disk.
#[derive(Clone, Debug)]
struct Source {
    agent: &'static str,
    shape: Shape,
    path: PathBuf,
    /// the session's id: the file's (claude, codex), the task's (cline); none for aider and
    /// cursor, whose one file holds many sessions
    session: Option<String>,
    /// the project's root, when where it was found says it (aider, cline, cursor)
    root: Option<PathBuf>,
}

fn is_uuid(b: &[u8]) -> bool {
    b.len() == 36 && b.iter().enumerate().all(|(i, &c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

/// A session's id from its file's name: the UUID the name ends with (Claude Code names a
/// session's file by its id; Codex ends its rollout files with it), else the name without
/// its extension.
fn session_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let b = stem.as_bytes();
    if b.len() >= 36 && is_uuid(&b[b.len() - 36..]) && (b.len() == 36 || b[b.len() - 37] == b'-') {
        return Some(stem[stem.len() - 36..].to_string());
    }
    Some(stem.to_string())
}

/// A folder's entries, sorted by name, with their kinds (links are not followed).
fn entries(dir: &Path) -> io::Result<Vec<(PathBuf, fs::FileType)>> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        v.push((e.path(), e.file_type()?));
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

fn has_ext(p: &Path, ext: &str) -> bool {
    p.extension().is_some_and(|e| e == ext)
}

fn hidden_name(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b"."))
}

/// What a search for sources found: the sources, and notes on what was passed over.
#[derive(Default)]
struct Found {
    sources: Vec<Source>,
    notes: Vec<String>,
    links: u64,
}

impl Found {
    fn file(&mut self, agent: &'static str, shape: Shape, path: PathBuf, t: &fs::FileType, session: Option<String>, root: Option<PathBuf>) {
        if t.is_symlink() {
            self.links += 1;
        } else if t.is_file() {
            self.sources.push(Source { agent, shape, path, session, root });
        }
    }
}

/// The files of a JSON lines agent: `ext` files in `dir` and its sub-folders down to `depth`
/// (hidden folders and links are passed over).
fn walk(agent: &'static str, dir: &Path, depth: u32, found: &mut Found) {
    let list = match entries(dir) {
        Ok(l) => l,
        Err(e) => {
            found.notes.push(format!("{agent}: cannot read {}: {e}", dir.display()));
            return;
        }
    };
    for (p, t) in list {
        if t.is_dir() {
            if depth > 0 && !hidden_name(&p) {
                walk(agent, &p, depth - 1, found);
            }
        } else if has_ext(&p, "jsonl") {
            let id = session_id(&p);
            found.file(agent, Shape::Lines, p, &t, id, None);
        }
    }
}

/// How many folders below a session's own folder its helpers' transcripts are looked for
/// (`subagents/agent-*.jsonl` is 1 folder down, a workflow run's agents 3; one more to spare).
const HELPER_DEPTH: u32 = 4;

/// The helpers' transcripts of the sessions in one of Claude Code's project folders: in each
/// sub-folder named by a session id, every `agent-*.jsonl` up to HELPER_DEPTH folders down,
/// each a piece of that session (its project from its own lines, as a session's).
fn helpers(agent: &'static str, project_dir: &Path, found: &mut Found) {
    // (the walk of the project folder has noted any trouble reading it)
    let Ok(list) = entries(project_dir) else { return };
    for (d, t) in list {
        let Some(name) = d.file_name().and_then(|n| n.to_str()) else { continue };
        if t.is_dir() && name.len() == 36 && is_uuid(name.as_bytes()) {
            helper_files(agent, &d, HELPER_DEPTH, name, found);
        }
    }
}

fn helper_files(agent: &'static str, dir: &Path, depth: u32, session: &str, found: &mut Found) {
    let list = match entries(dir) {
        Ok(l) => l,
        Err(e) => {
            found.notes.push(format!("{agent}: cannot read {}: {e}", dir.display()));
            return;
        }
    };
    for (p, t) in list {
        if t.is_dir() {
            if depth > 0 && !hidden_name(&p) {
                helper_files(agent, &p, depth - 1, session, found);
            }
        } else if has_ext(&p, "jsonl") && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agent-")) {
            found.file(agent, Shape::Lines, p, &t, Some(session.to_string()), None);
        }
    }
}

/// An agent's folder, or a note on why it is passed over.
fn folder(agent: &str, place: &Result<PathBuf, String>, sub: &str, found: &mut Found) -> Option<PathBuf> {
    match place {
        Ok(d) => {
            let d = if sub.is_empty() { d.clone() } else { d.join(sub) };
            if d.is_dir() {
                return Some(d);
            }
            found.notes.push(format!("{agent}: no folder {}; passed over", d.display()));
        }
        Err(e) => found.notes.push(format!("{agent}: {e}; passed over")),
    }
    None
}

/// The session sources of one agent, in order.
fn find(agent: &'static str, places: &Places) -> Found {
    let mut f = Found::default();
    match agent {
        "claude" => {
            if let Some(base) = folder(agent, &places.claude, "projects", &mut f) {
                // projects/<folder>/<session>.jsonl
                match entries(&base) {
                    Ok(list) => {
                        for (d, t) in list {
                            if t.is_dir() {
                                walk(agent, &d, 0, &mut f);
                                helpers(agent, &d, &mut f);
                            }
                        }
                    }
                    Err(e) => f.notes.push(format!("claude: cannot read {}: {e}", base.display())),
                }
            }
        }
        "codex" => {
            if let Some(base) = folder(agent, &places.codex, "sessions", &mut f) {
                // sessions/YYYY/MM/DD/rollout-...jsonl (older versions: sessions/rollout-...)
                walk(agent, &base, 6, &mut f);
            }
        }
        "aider" => {
            // its folder is a project's root: the history belongs to that project
            if let Some(root) = folder(agent, &places.aider, "", &mut f) {
                let p = root.join(".aider.chat.history.md");
                match fs::symlink_metadata(&p) {
                    Ok(md) => f.file(agent, Shape::Lines, p, &md.file_type(), None, Some(root.clone())),
                    Err(_) => f.notes.push(format!("aider: no {}; passed over", p.display())),
                }
            }
        }
        "cline" => {
            if let Some(base) = folder(agent, &places.cline, "tasks", &mut f) {
                let root = places.cline.clone().ok();
                match entries(&base) {
                    Ok(list) => {
                        for (d, t) in list {
                            let p = d.join("api_conversation_history.json");
                            if t.is_dir()
                                && let Ok(md) = fs::symlink_metadata(&p)
                            {
                                let task = d.file_name().map(|n| n.to_string_lossy().into_owned());
                                f.file(agent, Shape::Document, p, &md.file_type(), task, root.clone());
                            }
                        }
                    }
                    Err(e) => f.notes.push(format!("cline: cannot read {}: {e}", base.display())),
                }
            }
        }
        _ => {
            if let Some(base) = folder(agent, &places.cursor, "", &mut f) {
                let p = base.join("state.vscdb");
                match fs::symlink_metadata(&p) {
                    Ok(md) => f.file(agent, Shape::Rows, p, &md.file_type(), None, Some(base.clone())),
                    Err(_) => f.notes.push(format!("cursor: no {}; passed over", p.display())),
                }
            }
        }
    }
    if f.links > 0 {
        f.notes.push(format!("{agent}: {} passed over (only files are read)", plural(f.links, "link")));
    }
    f
}

// ---------------------------------------------------------------- what was saved before

/// The newest saved piece of a source, and its project.
#[derive(Clone)]
struct Last {
    project: Project,
    rec: RunRec,
}

/// The newest saved piece of every session source of the store (by start, then id: a source
/// keeps one project, so its pieces are in one runs.jsonl), and the session runs a stopped
/// sweep left "packing". A "forgotten" record (`memory forget` keeps one for each piece it forgets)
/// counts as saved, so what the person forgot is not saved again.
fn saved(projects: &[Project]) -> (HashMap<String, Last>, Vec<(Project, u64)>) {
    let mut last: HashMap<String, Last> = HashMap::new();
    let mut dead = Vec::new();
    for p in projects {
        for r in store::read_runs_all(&p.dir) {
            if r.kind() != "session" {
                continue;
            }
            match r.state() {
                "packing" => dead.push((p.clone(), r.id())),
                "ready" | "forgotten" => {
                    let Some(src) = r.s("source") else { continue };
                    if last.get(src).is_none_or(|l| (r.i("start"), r.id()) > (l.rec.i("start"), l.rec.id())) {
                        last.insert(src.to_string(), Last { project: p.clone(), rec: r });
                    }
                }
                _ => {}
            }
        }
    }
    (last, dead)
}

/// Where the text a piece read ends, the hash of the source up to there, and the piece's id:
/// (offset + size, hash, id).
fn chain_end(r: &RunRec) -> Option<(u64, String, u64)> {
    let end = (r.i("offset")? as u64).checked_add(r.i("size")? as u64)?;
    Some((end, r.s("hash")?.to_string(), r.id()))
}

/// A file's stamp: its size and modification time, to the nanosecond. A source whose stamp is
/// the one recorded with its last piece is not read again.
fn stamp_of(md: &Metadata) -> String {
    format!("{} {}.{:09}", md.len(), md.mtime(), md.mtime_nsec())
}

// ---------------------------------------------------------------- reading lines

/// Each whole line of the next `len` bytes of `r` (with its '\n'), in order; the bytes after
/// the last '\n' are returned (a line not ended yet). (`add` uses it too.)
pub fn each_line(r: &mut dyn Read, len: u64, f: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<Vec<u8>> {
    let mut r = r.take(len);
    let mut buf = vec![0u8; 1 << 20];
    let mut part: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let mut d = &buf[..n];
        while let Some(p) = d.iter().position(|&c| c == b'\n') {
            if part.is_empty() {
                f(&d[..=p])?;
            } else {
                part.extend_from_slice(&d[..=p]);
                let l = std::mem::take(&mut part);
                f(&l)?;
            }
            d = &d[p + 1..];
        }
        part.extend_from_slice(d);
    }
    Ok(part)
}

/// Where the `n` lines before `end` start (`end` starts a line): just after the (n + 1)-th
/// '\n' before `end`, or 0 when there are fewer.
fn lines_back_in(text: &[u8], end: usize, n: u32) -> usize {
    let mut seen = 0;
    for i in (0..end).rev() {
        if text[i] == b'\n' {
            seen += 1;
            if seen > n {
                return i + 1;
            }
        }
    }
    0
}

/// [`lines_back_in`] for a file, read backwards from `end`. (`add` uses it too.)
pub fn lines_back(f: &mut File, end: u64, n: u32) -> io::Result<u64> {
    let mut seen = 0u32;
    let mut pos = end;
    let mut buf = vec![0u8; 1 << 16];
    while pos > 0 {
        let k = pos.min(buf.len() as u64) as usize;
        pos -= k as u64;
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut buf[..k])?;
        for i in (0..k).rev() {
            if buf[i] == b'\n' {
                seen += 1;
                if seen > n {
                    return Ok(pos + i as u64 + 1);
                }
            }
        }
    }
    Ok(0)
}

/// A line's JSON object (a session line), without its line end.
fn json_line(line: &[u8]) -> Option<Json> {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    let l = l.strip_suffix(b"\r").unwrap_or(l);
    json::parse(l).ok().filter(|v| v.obj().is_some())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The working folder a session line names: Claude Code's `cwd`, Codex's `payload.cwd`
/// (session_meta and turn_context lines); an absolute path only.
fn line_cwd(line: &[u8]) -> Option<String> {
    if !contains(line, b"\"cwd\"") {
        return None;
    }
    let o = json_line(line)?;
    let cwd = o.get("cwd").and_then(Json::str).or_else(|| o.get("payload").and_then(|p| p.get("cwd")).and_then(Json::str))?;
    cwd.starts_with('/').then(|| cwd.to_string())
}

/// The files a Claude Code session line edits: the `file_path` or `notebook_path` of each
/// Edit, Write, MultiEdit and NotebookEdit call among its message's content blocks.
fn line_edits(line: &[u8]) -> Vec<String> {
    if !contains(line, b"\"tool_use\"") {
        return Vec::new();
    }
    let Some(o) = json_line(line) else { return Vec::new() };
    let Some(blocks) = o.get("message").and_then(|m| m.get("content")).and_then(Json::arr) else { return Vec::new() };
    let mut v = Vec::new();
    for b in blocks {
        if b.get("type").and_then(Json::str) != Some("tool_use") {
            continue;
        }
        let name = b.get("name").and_then(Json::str).unwrap_or("");
        if let Some((_, field)) = EDIT_TOOLS.iter().find(|(t, _)| *t == name)
            && let Some(path) = b.get("input").and_then(|i| i.get(field)).and_then(Json::str)
        {
            v.push(path.to_string());
        }
    }
    v
}

/// The exact bytes of each element of a JSON array, in order (`doc` holds one array and white
/// space around it, as json::parse has checked); None when it is not an array.
fn array_items(doc: &[u8]) -> Option<Vec<&[u8]>> {
    let ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r');
    let skip = |mut i: usize| {
        while i < doc.len() && ws(doc[i]) {
            i += 1;
        }
        i
    };
    let mut i = skip(0);
    if doc.get(i) != Some(&b'[') {
        return None;
    }
    i = skip(i + 1);
    let mut items = Vec::new();
    if doc.get(i) == Some(&b']') {
        return Some(items);
    }
    loop {
        i = skip(i);
        // one element: up to the ',' or ']' outside its strings and brackets
        let start = i;
        let (mut depth, mut in_str, mut esc) = (0usize, false, false);
        while i < doc.len() {
            let c = doc[i];
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' if depth == 0 => break,
                    b']' | b'}' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
            }
            i += 1;
        }
        let mut end = i;
        while end > start && ws(doc[end - 1]) {
            end -= 1;
        }
        items.push(&doc[start..end]);
        match doc.get(i)? {
            b',' => i += 1,
            _ => return Some(items),
        }
    }
}

/// A row of sqlite3's JSON output: the line without the array's '[' before the first row and
/// the ',' or ']' after each row.
fn row_of(line: &[u8]) -> &[u8] {
    let mut r = line.strip_suffix(b"\n").unwrap_or(line);
    r = r.strip_prefix(b"[").unwrap_or(r);
    r.strip_suffix(b",").or_else(|| r.strip_suffix(b"]")).unwrap_or(r)
}

// ---------------------------------------------------------------- a piece

/// A piece being written to its snapshot: whole lines of the source as read, secrets hidden,
/// hashed and counted on the way.
struct Piece {
    snap: PathBuf,
    out: BufWriter<File>,
    hider: Hider,
    /// values hidden while the hider read the lines before the piece (not counted)
    primed: u64,
    /// the source as read, from its start to the piece's end
    hash: Hash128,
    /// the source's bytes taken into the piece
    size: u64,
    /// the snapshot's bytes and lines
    bytes: u64,
    lines: u64,
    /// values hidden by hand (rows hidden one by one)
    hidden: u64,
    /// list the files a Claude session edits
    edits: bool,
    files: Vec<(String, u64)>,
    /// look for the working folder the lines name
    want_cwd: bool,
    cwd: Option<String>,
    line: Vec<u8>,
}

impl Piece {
    /// A new snapshot in the store's tmp folder (removed when the piece is dropped unsaved).
    fn new(tmp: &Path, seq: u64, hider: Hider) -> io::Result<Piece> {
        let snap = tmp.join(format!("sessions-{seq}.snap.tmp-{}", std::process::id()));
        let f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&snap)?;
        Ok(Piece {
            snap,
            out: BufWriter::with_capacity(1 << 20, f),
            hider,
            primed: 0,
            hash: Hash128::new(),
            size: 0,
            bytes: 0,
            lines: 0,
            hidden: 0,
            edits: false,
            files: Vec::new(),
            want_cwd: false,
            cwd: None,
            line: Vec::new(),
        })
    }

    /// The hider reads the lines before the piece, as a pass over the whole source would have.
    fn prime(&mut self, line: &[u8]) {
        self.line.clear();
        self.hider.line(line, &mut self.line);
        self.primed = self.hider.counts().total();
    }

    /// One whole line of the source (with its '\n'): hashed, hidden, written.
    fn take(&mut self, line: &[u8]) -> io::Result<()> {
        self.hash.update(line);
        self.size += line.len() as u64;
        self.line.clear();
        self.hider.line(line, &mut self.line);
        self.write_line()
    }

    /// A line already hidden (a Cursor row): written as it is.
    fn take_hidden(&mut self, row: &[u8]) -> io::Result<()> {
        self.line.clear();
        self.line.extend_from_slice(row);
        self.line.push(b'\n');
        self.write_line()
    }

    fn write_line(&mut self) -> io::Result<()> {
        self.lines += 1;
        if self.want_cwd && self.cwd.is_none() {
            self.cwd = line_cwd(&self.line);
        }
        if self.edits {
            for f in line_edits(&self.line) {
                self.files.push((f, self.lines));
            }
        }
        self.bytes += self.line.len() as u64;
        self.out.write_all(&self.line)
    }

    fn hidden(&self) -> u64 {
        self.hider.counts().total() - self.primed + self.hidden
    }

    /// The snapshot written out, with the source's modification time (the archive records
    /// it; the catalog reads dates without a year by it).
    fn close(&mut self, mtime: std::time::SystemTime) -> io::Result<()> {
        self.out.flush()?;
        self.out.get_ref().set_modified(mtime)
    }
}

impl Drop for Piece {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.snap);
    }
}

/// What saving a source did.
enum Done {
    Saved(String),
    Unchanged,
    /// passed over for now, and why
    Waiting(String),
}

// ---------------------------------------------------------------- the sweep

/// What a sweep did.
#[derive(Debug, Default)]
pub struct Report {
    /// sessions are off: nothing was read
    pub off: bool,
    /// another sweep held the lock (the server's sweep does not wait)
    pub busy: bool,
    /// stopped before the end (the server is ending)
    pub stopped: bool,
    /// a line per piece saved
    pub saved: Vec<String>,
    /// sources with nothing new
    pub unchanged: u64,
    /// sources and agents passed over, and why
    pub notes: Vec<String>,
    /// sources that could not be saved, and why
    pub failed: Vec<String>,
}

/// One sweep's state.
struct Sweep<'a> {
    home: &'a Path,
    tmp: PathBuf,
    extras: Vec<Extra>,
    last: HashMap<String, Last>,
    /// working folder -> project root
    roots: HashMap<String, PathBuf>,
    seq: u64,
    report: Report,
}

/// `sessions sync` started detached (a new session, its stdio /dev/null, not waited for): how
/// the hooks save Claude Code's chats, as fewer leaves fewer's MCP server (whose sweeper saves
/// them otherwise) out of Claude Code's start; one sweep runs at a time (raw/sessions.lock).
pub fn sync_detached() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let mut c = std::process::Command::new(exe);
    c.args(["sessions", "sync"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: new_session makes one async-signal-safe call (setsid)
    unsafe { c.pre_exec(crate::sys_run::new_session) };
    c.spawn().map(|_| ())
}

/// One sweep over the agents that are on: every new piece saved (the module's doc). `wait`:
/// wait for a sweep already running (the command), or pass (the server). `stop` is asked
/// between sources.
pub fn sweep(home: &Path, places: &Places, wait: bool, stop: &AtomicBool) -> Report {
    let mut report = Report::default();
    // 1. nothing is read while sessions are off
    let settings = config::read(home);
    let agents = agents_on(&settings, &mut report.notes);
    if agents.is_empty() {
        report.off = true;
        return report;
    }
    // 2. one sweep at a time
    let raw = home.join("raw");
    let lock = crate::store::make_home(home).and_then(|_| fs::create_dir_all(&raw)).and_then(|_| OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(raw.join("sessions.lock")));
    let lock = match lock {
        Ok(f) => f,
        Err(e) => {
            report.failed.push(format!("cannot open {}: {e}", raw.join("sessions.lock").display()));
            return report;
        }
    };
    if wait {
        if let Err(e) = lock.lock() {
            report.failed.push(format!("cannot lock {}: {e}", raw.join("sessions.lock").display()));
            return report;
        }
    } else if lock.try_lock().is_err() {
        report.busy = true;
        return report;
    }
    let tmp = home.join("tmp");
    if let Err(e) = fs::create_dir_all(&tmp) {
        report.failed.push(format!("cannot make {}: {e}", tmp.display()));
        return report;
    }
    store::clear_leftovers(&tmp);
    let extras = extra_secrets(&settings, &mut report.notes);
    // 3. what was saved before; runs a stopped sweep left half saved
    let projects = store::list_projects(home);
    let (last, dead) = saved(&projects);
    for (p, id) in dead {
        for f in [store::run_file(id), catalog::cat_file(id)] {
            let _ = fs::remove_file(p.dir.join(&f));
        }
        let _ = fs::remove_file(grep::idx_path(p.dir.join(store::run_file(id)).as_os_str()));
        let f = Json::o(vec![("state", "failed".into()), ("note", STOPPED.into())]);
        if let Err(e) = store::update_run(&p, id, f) {
            report.failed.push(format!("{}/r{id}: cannot write the run table: {e}", p.pid));
        }
    }
    let mut s = Sweep { home, tmp, extras, last, roots: HashMap::new(), seq: 0, report };
    // 4. each agent's sources
    'agents: for agent in agents {
        let found = find(agent, places);
        s.report.notes.extend(found.notes);
        for src in &found.sources {
            if stop.load(Ordering::Relaxed) {
                s.report.stopped = true;
                break 'agents;
            }
            s.save(src);
        }
    }
    drop(lock);
    // the memory's limit, after a save (memory.rs; no store lock is held now)
    if !s.report.saved.is_empty()
        && let Some(m) = crate::memory::after_save(home)
    {
        s.report.notes.push(m);
    }
    s.report
}

impl Sweep<'_> {
    /// Save what is new in one source.
    fn save(&mut self, src: &Source) {
        let what = match &src.session {
            Some(id) => format!("{} session {id} ({})", src.agent, src.path.display()),
            None => format!("{} {}", src.agent, src.path.display()),
        };
        let Some(key) = src.path.to_str().map(str::to_string) else {
            self.report.notes.push(format!("{what}: its path is not UTF-8; passed over"));
            return;
        };
        let prev = self.last.get(&key).cloned();
        let r = match src.shape {
            Shape::Lines => self.save_lines(src, &key, prev),
            Shape::Document => self.save_document(src, &key, prev),
            Shape::Rows => self.save_rows(src, &key, prev),
        };
        match r {
            Ok(Done::Saved(m)) => self.report.saved.push(format!("{what}: {m}")),
            Ok(Done::Unchanged) => self.report.unchanged += 1,
            Ok(Done::Waiting(why)) => self.report.notes.push(format!("{what}: {why}")),
            Err(e) => self.report.failed.push(format!("{what}: {e}")),
        }
    }

    fn piece(&mut self) -> Result<Piece, String> {
        self.seq += 1;
        Piece::new(&self.tmp, self.seq, Hider::new(self.extras.clone())).map_err(|e| format!("cannot write a snapshot in {}: {e}", self.tmp.display()))
    }

    /// The project of a source's new piece: its earlier pieces' project; else where it was
    /// found; else from the working folder its lines name. None: no folder named yet.
    fn project_for(&mut self, src: &Source, prev: Option<&Last>, cwd: Option<&str>) -> Result<Option<Project>, String> {
        if let Some(p) = prev {
            return Ok(Some(p.project.clone()));
        }
        let root = match (&src.root, cwd) {
            (Some(r), _) => r.clone(),
            (None, Some(c)) => match self.roots.get(c) {
                Some(r) => r.clone(),
                None => {
                    let r = git_root(c);
                    self.roots.insert(c.to_string(), r.clone());
                    r
                }
            },
            (None, None) => return Ok(None),
        };
        store::ensure_project(self.home, &root).map(Some).map_err(|e| format!("cannot open the store's project for {}: {e}", root.display()))
    }

    /// claude, codex, aider: a file that grows by lines.
    fn save_lines(&mut self, src: &Source, key: &str, prev: Option<Last>) -> Result<Done, String> {
        let md = fs::symlink_metadata(&src.path).map_err(|e| format!("cannot read: {e}"))?;
        let stamp = stamp_of(&md);
        if prev.as_ref().is_some_and(|p| p.rec.s("stamp") == Some(stamp.as_str())) {
            return Ok(Done::Unchanged);
        }
        let mtime = md.modified().map_err(|e| format!("cannot read its time: {e}"))?;
        let len = md.len();
        let mut f = File::open(&src.path).map_err(|e| format!("cannot read: {e}"))?;
        let rd = |e: io::Error| format!("cannot read: {e}");
        let mut piece = self.piece()?;
        // 1. where the new part starts: after what was saved, when the source still starts
        //    with it (7.2); else from the start
        let mut from = 0;
        let mut cont = None;
        if let Some((end, want, id)) = prev.as_ref().and_then(|p| chain_end(&p.rec))
            && end <= len
        {
            let mut check = Hash128::new();
            let got = feed(&mut f, end, &mut |b| {
                check.update(b);
                piece.hash.update(b);
            })
            .map_err(rd)?;
            if got == end && check.finish() == want {
                from = end;
                cont = Some(id);
            } else {
                piece.hash = Hash128::new();
            }
        }
        // 2. the hider reads the lines before the new part
        if from > 0 {
            let start = lines_back(&mut f, from, secrets::MAX_KEY_LINES).map_err(rd)?;
            f.seek(SeekFrom::Start(start)).map_err(rd)?;
            each_line(&mut f, from - start, &mut |l| {
                piece.prime(l);
                Ok(())
            })
            .map_err(rd)?;
        }
        // 3. the new whole lines
        f.seek(SeekFrom::Start(from)).map_err(rd)?;
        piece.edits = src.agent == "claude";
        piece.want_cwd = prev.is_none() && src.root.is_none();
        let part = each_line(&mut f, len - from, &mut |l| piece.take(l)).map_err(|e| format!("cannot read it or write its snapshot: {e}"))?;
        if piece.lines == 0 {
            return Ok(if part.is_empty() { Done::Unchanged } else { Done::Waiting(format!("its last line ({} bytes) has no line end yet; saved when it has", part.len())) });
        }
        let cwd = piece.cwd.clone();
        let Some(project) = self.project_for(src, prev.as_ref(), cwd.as_deref())? else {
            return Ok(Done::Waiting("no line names its working folder (cwd) yet; saved when one does".into()));
        };
        piece.close(mtime).map_err(|e| format!("cannot write its snapshot: {e}"))?;
        let again = if cont.is_none() { prev.as_ref().map(|p| p.rec.id()) } else { None };
        self.commit(src, key, &project, piece, Link { offset: Some(from), cont, again, stamp: &stamp, reader: "" })
    }

    /// cline: a JSON array rewritten whole, read as one element per line.
    fn save_document(&mut self, src: &Source, key: &str, prev: Option<Last>) -> Result<Done, String> {
        let md = fs::symlink_metadata(&src.path).map_err(|e| format!("cannot read: {e}"))?;
        let stamp = stamp_of(&md);
        if prev.as_ref().is_some_and(|p| p.rec.s("stamp") == Some(stamp.as_str())) {
            return Ok(Done::Unchanged);
        }
        if md.len() > DOC_MAX {
            return Ok(Done::Waiting(format!("{} bytes: task files over {} MiB are saved in a later version", md.len(), DOC_MAX >> 20)));
        }
        let mtime = md.modified().map_err(|e| format!("cannot read its time: {e}"))?;
        let doc = fs::read(&src.path).map_err(|e| format!("cannot read: {e}"))?;
        // (Cline rewrites the file as the task grows: one cut short is read again next time)
        let items = match json::parse(&doc) {
            Ok(Json::Arr(_)) => array_items(&doc).ok_or("not a JSON array")?,
            Ok(_) => return Ok(Done::Waiting("not a JSON array; passed over".into())),
            Err(e) => return Ok(Done::Waiting(format!("not whole JSON now ({e}); read again next time"))),
        };
        let mut text = Vec::with_capacity(doc.len() + items.len());
        for it in items {
            text.extend_from_slice(it);
            text.push(b'\n');
        }
        let mut piece = self.piece()?;
        let mut from = 0usize;
        let mut cont = None;
        if let Some((end, want, id)) = prev.as_ref().and_then(|p| chain_end(&p.rec))
            && end <= text.len() as u64
        {
            let mut check = Hash128::new();
            check.update(&text[..end as usize]);
            if check.finish() == want {
                from = end as usize;
                cont = Some(id);
                piece.hash.update(&text[..from]);
                for l in text[lines_back_in(&text, from, secrets::MAX_KEY_LINES)..from].split_inclusive(|&c| c == b'\n') {
                    piece.prime(l);
                }
            }
        }
        for l in text[from..].split_inclusive(|&c| c == b'\n') {
            piece.take(l).map_err(|e| format!("cannot write its snapshot: {e}"))?;
        }
        if piece.lines == 0 {
            return Ok(Done::Unchanged);
        }
        let Some(project) = self.project_for(src, prev.as_ref(), None)? else { return Err("no project".into()) };
        piece.close(mtime).map_err(|e| format!("cannot write its snapshot: {e}"))?;
        let again = if cont.is_none() { prev.as_ref().map(|p| p.rec.id()) } else { None };
        self.commit(src, key, &project, piece, Link { offset: Some(from as u64), cont, again, stamp: &stamp, reader: "json" })
    }

    /// cursor: the rows of its database, through sqlite3.
    fn save_rows(&mut self, src: &Source, key: &str, prev: Option<Last>) -> Result<Done, String> {
        let md = fs::symlink_metadata(&src.path).map_err(|e| format!("cannot read: {e}"))?;
        // (Cursor writes through the database's -wal file first)
        let wal = fs::metadata(format!("{key}-wal")).ok();
        let stamp = format!("{} {}", stamp_of(&md), wal.as_ref().map_or("-".to_string(), stamp_of));
        let mtime = wal.as_ref().and_then(|w| w.modified().ok()).into_iter().chain(md.modified().ok()).max().ok_or("cannot read its time")?;
        // the project: where its rows were saved before, else the database's folder
        let dir = match &prev {
            Some(p) => p.project.dir.clone(),
            None => {
                let root = src.root.clone().ok_or("no project")?;
                self.home.join("projects").join(store::pid_for(self.home, &root).0)
            }
        };
        let mut seen = Seen::read(&dir.join(SEEN));
        if seen.stamp == stamp {
            return Ok(Done::Unchanged);
        }
        let mut piece = self.piece()?;
        let mut child = match Command::new("sqlite3")
            .args(["-readonly", "-init", "/dev/null", "-cmd", ".timeout 2000", "-json"])
            .arg(&src.path)
            .arg(CURSOR_QUERY)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Done::Waiting("sqlite3 is not installed (Cursor's chats are read with it); passed over".into())),
            Err(e) => return Err(format!("cannot run sqlite3: {e}")),
        };
        let mut err = child.stderr.take().expect("piped");
        let errs = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = err.read_to_end(&mut b);
            b
        });
        let mut new: Vec<String> = Vec::new();
        let mut fresh: HashSet<String> = HashSet::new();
        let mut bad = None;
        let mut out = BufReader::with_capacity(1 << 20, child.stdout.take().expect("piped"));
        let mut line = Vec::new();
        let mut n = 0u64;
        let read = loop {
            line.clear();
            match out.read_until(b'\n', &mut line) {
                Ok(0) => break Ok(()),
                Ok(_) => {}
                Err(e) => break Err(e),
            }
            n += 1;
            let row = row_of(&line);
            if bad.is_some() || !json::parse(row).is_ok_and(|v| v.obj().is_some()) {
                bad.get_or_insert(n);
                continue;
            }
            // each row hidden on its own, so a row's saved bytes depend on the row alone
            let (hidden, counts) = secrets::hide(row, &self.extras);
            let mut h = Hash128::new();
            h.update(&hidden);
            let h = h.finish();
            if !seen.rows.contains(&h) && fresh.insert(h.clone()) {
                piece.hidden += counts.total();
                if let Err(e) = piece.take_hidden(&hidden) {
                    break Err(e);
                }
                new.push(h);
            }
        };
        drop(out);
        let status = child.wait().map_err(|e| format!("cannot wait for sqlite3: {e}"))?;
        let errs = errs.join().unwrap_or_default();
        read.map_err(|e| format!("cannot read sqlite3's output or write its snapshot: {e}"))?;
        if !status.success() {
            let why = String::from_utf8_lossy(&errs).trim().to_string();
            return Err(format!("sqlite3 could not read it: {}", if why.is_empty() { status.to_string() } else { why }));
        }
        if let Some(l) = bad {
            return Err(format!("sqlite3's output line {l} is not a row as expected; nothing saved"));
        }
        if new.is_empty() {
            // (nothing new: the stamp is kept, so the database is not read again until it changes)
            if dir.is_dir() {
                seen.stamp = stamp;
                seen.write(&dir.join(SEEN), key).map_err(|e| format!("cannot write {}: {e}", dir.join(SEEN).display()))?;
            }
            return Ok(Done::Unchanged);
        }
        let Some(project) = self.project_for(src, prev.as_ref(), None)? else { return Err("no project".into()) };
        piece.close(mtime).map_err(|e| format!("cannot write its snapshot: {e}"))?;
        let msg = self.commit(src, key, &project, piece, Link { offset: None, cont: prev.as_ref().map(|p| p.rec.id()), again: None, stamp: &stamp, reader: "sqlite3" })?;
        // the rows saved; written after the run is ready (a sweep stopped between saves the
        // rows again, never loses one)
        seen.rows.extend(new);
        seen.stamp = stamp;
        seen.write(&project.dir.join(SEEN), key).map_err(|e| format!("saved, but cannot write {}: {e}", project.dir.join(SEEN).display()))?;
        Ok(msg)
    }


    /// Save a piece as a run of `project`, as `add` saves a file: the run recorded as packing,
    /// its snapshot packed by fewer-pack, its .idx, its catalog (with the files a Claude session
    /// edited), then ready.
    fn commit(&mut self, src: &Source, key: &str, project: &Project, mut piece: Piece, link: Link) -> Result<Done, String> {
        let start = crate::times::now_ms();
        let mut first = Json::o(vec![("kind", "session".into()), ("state", "packing".into()), ("start", Json::from(start)), ("agent", src.agent.into())]);
        if let Some(s) = &src.session {
            first.set("session", s.as_str().into());
        }
        first.set("source", key.into());
        if let Some(o) = link.offset {
            first.set("offset", Json::from(o));
        }
        if let Some(c) = link.cont {
            first.set("continues", Json::from(c));
        }
        if !link.reader.is_empty() {
            first.set("reader", link.reader.into());
        }
        let id = store::new_run(project, first).map_err(|e| format!("cannot write the run table: {e}"))?;
        let archive = project.dir.join(store::run_file(id));
        let abytes = match pack(&archive, &piece.snap) {
            Ok(a) => a,
            Err(e) => return Err(failed_run(project, id, e)),
        };
        let mut f = Json::o(vec![("state", "ready".into()), ("end", Json::from(crate::times::now_ms()))]);
        if link.offset.is_some() {
            // (what the next sweep checks: the source as read, from its start to this piece's end)
            f.set("size", Json::from(piece.size));
            f.set("hash", std::mem::replace(&mut piece.hash, Hash128::new()).finish().into());
        }
        f.set("stamp", link.stamp.into());
        f.set("bytes", Json::from(piece.bytes));
        f.set("lines", Json::from(piece.lines));
        f.set("archive", Json::from(abytes));
        f.set("hidden", Json::from(piece.hidden()));
        // the index pass: the catalog and the key registry; a run without a catalog is still
        // saved (the tools build a missing one when they need it)
        let tags = catalog::Tags { name: key.to_string(), files: piece.files.clone(), ..catalog::Tags::default() };
        let mut note = String::new();
        match catalog::index_run(project, id, start, &tags) {
            Ok(ix) => {
                for (k, v) in ix.fields() {
                    f.set(k, v);
                }
            }
            Err(e) => {
                note = format!("no catalog: {e}");
                f.set("note", note.as_str().into());
            }
        }
        if let Err(e) = store::update_run(project, id, f) {
            return Err(failed_run(project, id, format!("cannot write the run table: {e}")));
        }
        let how = match (link.cont, link.again) {
            (Some(c), _) if link.offset.is_some() => format!("its new part saved as {}/r{id} (continues r{c})", project.pid),
            (Some(c), _) => format!("its new and changed rows saved as {}/r{id} (after r{c})", project.pid),
            (None, Some(a)) => format!("saved whole again as {}/r{id} (it no longer starts as r{a} ends)", project.pid),
            (None, None) => format!("saved as {}/r{id}", project.pid),
        };
        let mut m = format!("{how}: {}, {} -> {abytes} bytes", plural(piece.lines, "line"), piece.bytes);
        if piece.hidden() > 0 {
            m.push_str(&format!(", {} hidden", plural(piece.hidden(), "secret")));
        }
        if !piece.files.is_empty() {
            m.push_str(&format!(", {} listed", plural(piece.files.len() as u64, "file edit")));
        }
        if !note.is_empty() {
            m.push_str(&format!(" ({note})"));
        }
        Ok(Done::Saved(m))
    }
}

fn plural(n: u64, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// How a piece follows the source's earlier pieces.
struct Link<'s> {
    /// where the piece starts in the source as read (none for Cursor's rows)
    offset: Option<u64>,
    /// the piece it continues
    cont: Option<u64>,
    /// the source's newest piece, when this one does not continue it (saved whole again)
    again: Option<u64>,
    /// the source's stamp when it was read
    stamp: &'s str,
    /// the tool that read the source: our JSON reader (cline), sqlite3 (cursor), or none
    reader: &'s str,
}

/// A run that could not be saved: its files removed, recorded as failed with the reason; the
/// reason, for the report. The next sweep saves its part again (it is not a saved piece).
fn failed_run(project: &Project, id: u64, why: String) -> String {
    let archive = project.dir.join(store::run_file(id));
    let _ = fs::remove_file(grep::idx_path(archive.as_os_str()));
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_file(project.dir.join(catalog::cat_file(id)));
    let _ = store::update_run(project, id, Json::o(vec![("state", "failed".into()), ("end", Json::from(crate::times::now_ms())), ("note", why.as_str().into())]));
    format!("{}/r{id}: {why}", project.pid)
}

/// Feed the next `n` bytes of `r` to `f`, in pieces; how many there were. (`add` uses it too.)
pub fn feed(r: &mut dyn Read, n: u64, f: &mut dyn FnMut(&[u8])) -> io::Result<u64> {
    let mut r = r.take(n);
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0;
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(got),
            Ok(k) => {
                f(&buf[..k]);
                got += k as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// A session's project root from its working folder: the folder's git root, as `run` and
/// `add` find theirs, else the folder as the session names it.
pub(crate) fn git_root(cwd: &str) -> PathBuf {
    let p = Path::new(cwd);
    if p.is_dir() {
        let args = [OsStr::new("-C"), p.as_os_str(), OsStr::new("rev-parse"), OsStr::new("--show-toplevel")];
        if let Some(out) = store::run_quiet("git", &args, Duration::from_secs(2)) {
            let s = String::from_utf8_lossy(&out).trim_end_matches(['\n', '\r']).to_string();
            if !s.is_empty() && Path::new(&s).is_absolute() {
                return PathBuf::from(s);
            }
        }
    }
    p.to_path_buf()
}

/// Pack a snapshot into an archive with the fewer-pack binary on half the cores
/// (saving sessions is background work), then write its .idx (the same bytes as `fewer
/// index`); the archive's size.
fn pack(archive: &Path, snap: &Path) -> Result<u64, String> {
    let t = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) / 2).max(1);
    let out = Command::new(add::pack_bin())
        .arg("-q")
        .arg("-t")
        .arg(t.to_string())
        .arg("-o")
        .arg(archive)
        .arg(snap)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run fewer-pack ({}): {e} (set FEWER_PACK_BIN to the fewer-pack program)", add::pack_bin().display()))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("fewer-pack could not pack it: {}", if why.is_empty() { out.status.to_string() } else { why }));
    }
    let ix = grep::build_index(archive, t)?;
    store::write_atomic(&grep::idx_path(archive.as_os_str()), &ix).map_err(|e| format!("cannot write its index: {e}"))?;
    fs::metadata(archive).map(|m| m.len()).map_err(|e| format!("cannot read its archive: {e}"))
}

/// The rows of a Cursor database saved so far (a project's cursor.seen): the database's stamp
/// when it was last read, and our hash of each row as saved (secrets hidden).
#[derive(Debug, Default, PartialEq)]
struct Seen {
    stamp: String,
    rows: HashSet<String>,
}

impl Seen {
    /// The file as `write` wrote it; a missing or damaged one reads as empty (every row is
    /// then saved again: nothing is lost).
    fn read(path: &Path) -> Seen {
        let Ok(t) = fs::read_to_string(path) else { return Seen::default() };
        let mut lines = t.lines();
        let _what = lines.next();
        let Some(stamp) = lines.next().and_then(|l| l.strip_prefix("stamp = ")) else { return Seen::default() };
        let rows = lines.filter(|l| l.len() == 32 && l.bytes().all(|c| c.is_ascii_hexdigit())).map(str::to_string).collect();
        Seen { stamp: stamp.to_string(), rows }
    }

    fn write(&self, path: &Path, source: &str) -> io::Result<()> {
        let mut rows: Vec<&String> = self.rows.iter().collect();
        rows.sort();
        let mut t = format!("# the rows of {source:?} saved by fewer: our hash of each row as saved\nstamp = {}\n", self.stamp);
        for r in rows {
            t.push_str(r);
            t.push('\n');
        }
        store::write_atomic(path, t.as_bytes())
    }
}

// ---------------------------------------------------------------- the server's sweep

/// The MCP server's sweep: at the server's start, then every 60 s, on a
/// thread of its own, while sessions are on (the config is read again each time). It never
/// waits for another sweep: the next turn comes soon. Dropping it stops it (waiting at most
/// 2 s for the piece being saved), so every way out of the server stops it.
pub struct Sweeper {
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Sweeper {
    /// `log` gets each piece saved and each note (once), for the server's debug log; failures
    /// also go to stderr, once each.
    pub fn start(home: PathBuf, log: Box<dyn Fn(&str) + Send>) -> Sweeper {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        // (a test switch, never set in normal use: the time between sweeps in ms)
        let every = std::env::var("FEWER_TEST_SWEEP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(EVERY_MS);
        let (s, w) = (stop.clone(), wake.clone());
        let thread = std::thread::spawn(move || {
            let places = Places::from_env();
            let mut told: HashSet<String> = HashSet::new();
            loop {
                let r = sweep(&home, &places, false, &s);
                for m in &r.saved {
                    log(&format!("sessions: {m}"));
                }
                for n in &r.notes {
                    if told.insert(n.clone()) {
                        log(&format!("sessions: {n}"));
                    }
                }
                for e in &r.failed {
                    if told.insert(e.clone()) {
                        log(&format!("sessions: not saved: {e}"));
                        eprintln!("fewer: sessions: not saved: {e}");
                    }
                }
                let (m, cv) = &*w;
                let mut stopped = m.lock().unwrap_or_else(|e| e.into_inner());
                let until = Instant::now() + Duration::from_millis(every);
                while !*stopped {
                    let now = Instant::now();
                    if now >= until {
                        break;
                    }
                    stopped = cv.wait_timeout(stopped, until - now).unwrap_or_else(|e| e.into_inner()).0;
                }
                if *stopped {
                    return;
                }
            }
        });
        Sweeper { stop, wake, thread: Some(thread) }
    }

    /// Stop: no new source is started, and the piece being saved is waited for, at most
    /// `wait` (a piece cut off is marked failed by the next sweep and saved again).
    pub fn stop(mut self, wait: Duration) {
        self.halt(wait);
    }

    fn halt(&mut self, wait: Duration) {
        self.stop.store(true, Ordering::Relaxed);
        let (m, cv) = &*self.wake;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            let until = Instant::now() + wait;
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.halt(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------- `fewer sessions`

/// `fewer sessions sync`: one sweep now, waiting for one already running. Exit 0 when
/// nothing failed (sessions off, nothing new and agents passed over included), 1 when a source
/// could not be saved, 2 for usage errors and no store.
pub fn cmd_sessions(args: Vec<OsString>) -> i32 {
    let a: Vec<&str> = args.iter().map(|a| a.to_str().unwrap_or("?")).collect();
    match a.as_slice() {
        ["sync"] => sync(),
        ["-h" | "--help"] => {
            println!(
                "{USAGE}\n\nSave the new part of Claude Code's, Codex's, Aider's, Cline's and Cursor's sessions into the\nmemory store, secrets hidden. Nothing is read unless the store's config says `sessions = on`;\n`sessions.<agent> = off` leaves one agent out. The MCP server does the same at its start and\nevery 60 s.\n\nEach agent's sessions are read from its own folder: FEWER_CLAUDE_DIR (Claude Code, default\n~/.claude), FEWER_CODEX_DIR (Codex, ~/.codex), FEWER_AIDER_DIR (the project root holding\n.aider.chat.history.md), FEWER_CLINE_DIR (Cline, in VS Code's globalStorage) and\nFEWER_CURSOR_DIR (Cursor's globalStorage). An agent whose folder is not there is passed over."
            );
            0
        }
        _ => {
            eprintln!("fewer sessions: unknown arguments\n{USAGE}");
            2
        }
    }
}

fn sync() -> i32 {
    let home = match store::home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fewer sessions: {e}");
            return 2;
        }
    };
    let r = sweep(&home, &Places::from_env(), true, &AtomicBool::new(false));
    for n in &r.notes {
        println!("{n}");
    }
    if r.off {
        println!("sessions are off: nothing was read. To save agents' sessions, set `sessions = on` in {} (install claude asks).", config::path(&home).display());
        return 0;
    }
    for m in &r.saved {
        println!("{m}");
    }
    for e in &r.failed {
        eprintln!("fewer sessions: not saved: {e}");
    }
    println!("sessions: {} saved, {} unchanged, {} failed", plural(r.saved.len() as u64, "piece"), plural(r.unchanged as u64, "source"), r.failed.len());
    (!r.failed.is_empty()) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for a test, inside this crate's target folder (never the home folder).
    fn tmpdir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-sessions-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rng(seed: u64) -> impl FnMut(usize) -> usize {
        let mut x = seed;
        move |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n.max(1) as u64) as usize
        }
    }

    #[test]
    fn agents_follow_the_config() {
        let s = |t: &str| store::read_settings(t);
        let mut n = Vec::new();
        assert!(agents_on(&s(""), &mut n).is_empty());
        assert!(agents_on(&s("# nothing\nsessions = off\n"), &mut n).is_empty());
        assert_eq!(agents_on(&s("sessions = on\n"), &mut n), AGENTS.to_vec());
        assert_eq!(agents_on(&s("sessions = ON\nsessions.codex = off\nsessions.cursor = Off\nsessions.claude = on\n"), &mut n), vec!["claude", "aider", "cline"]);
        // only the agent named
        assert_eq!(agents_on(&s("sessions.claude = on\n"), &mut n), Vec::<&str>::new());
        assert!(n.is_empty());
        // a value that is neither on nor off counts as off, with a note
        assert!(agents_on(&s("sessions = yes\n"), &mut n).is_empty());
        assert_eq!(agents_on(&s("sessions = on\nsessions.aider = maybe\n"), &mut n), vec!["claude", "codex", "cline", "cursor"]);
        assert_eq!(n.len(), 2, "{n:?}");
        // the first line of a key wins, as the store reads it
        assert!(agents_on(&s("sessions = off\nsessions = on\n"), &mut n).is_empty());
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k: &str| pairs.iter().find(|(a, _)| *a == k).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn helpers_transcripts_are_pieces_of_their_session() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-helpers", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let sid = "0f7c2a1e-3b4d-4e5f-8a9b-0c1d2e3f4a5b";
        let put = |rel: &str| {
            let p = d.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"{}\n").unwrap();
        };
        put(&format!("{sid}.jsonl")); // the session itself (the walk's, not helpers')
        put(&format!("{sid}/subagents/agent-a1.jsonl")); // the Agent tool's helper
        put(&format!("{sid}/workflows/run1/agents/agent-b2.jsonl")); // a workflow run's agent, 3 down
        put(&format!("{sid}/w/x/y/z/agent-c3.jsonl")); // 4 down: still looked at
        put(&format!("{sid}/v/w/x/y/z/agent-d4.jsonl")); // 5 down: too deep
        put(&format!("{sid}/workflows/run1/journal.jsonl")); // a workflow's journal: not a transcript
        put(&format!("{sid}/subagents/notes.jsonl")); // not an agent's transcript
        put("notes/agent-e5.jsonl"); // not under a session's folder
        put(&format!("{sid}/.hidden/agent-f6.jsonl")); // a hidden folder
        let mut f = Found::default();
        helpers("claude", &d, &mut f);
        let mut got: Vec<(String, Option<String>)> = f.sources.iter().map(|s| (s.path.strip_prefix(&d).unwrap().display().to_string(), s.session.clone())).collect();
        got.sort();
        let want = |rel: &str| (format!("{sid}/{rel}"), Some(sid.to_string()));
        assert_eq!(got, [want("subagents/agent-a1.jsonl"), want("w/x/y/z/agent-c3.jsonl"), want("workflows/run1/agents/agent-b2.jsonl")]);
        assert!(f.sources.iter().all(|s| s.root.is_none() && s.agent == "claude"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn places_only_from_their_settings() {
        let ok = |p: &str| Ok(PathBuf::from(p));
        // in development: nothing from HOME, the agents' own settings or the per-OS folders
        let p = Places::with(&env_of(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/x"), ("CLAUDE_CONFIG_DIR", "/c"), ("CODEX_HOME", "/d")]));
        for (agent, setting) in SETTINGS {
            let e = p.of(agent).clone().unwrap_err();
            assert!(e.starts_with(&format!("{setting} is not set")), "{agent}: {e}");
        }
        // each agent's own setting, and only that
        let e = [("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "/c"), ("CODEX_HOME", "/d"), ("FEWER_CLAUDE_DIR", "/t/a"), ("FEWER_CODEX_DIR", "/t/b"), ("FEWER_AIDER_DIR", "/w/app"), ("FEWER_CLINE_DIR", "/t/c"), ("FEWER_CURSOR_DIR", "/t/d")];
        let p = Places::with(&env_of(&e));
        assert_eq!((p.claude, p.codex, p.aider, p.cline, p.cursor), (ok("/t/a"), ok("/t/b"), ok("/w/app"), ok("/t/c"), ok("/t/d")));
        // a relative setting: never a folder relative to the current one
        let p = Places::with(&env_of(&[("FEWER_CLAUDE_DIR", "rel"), ("FEWER_AIDER_DIR", "./app")]));
        assert_eq!(p.claude, Err("FEWER_CLAUDE_DIR is not an absolute path".into()));
        assert_eq!(p.aider, Err("FEWER_AIDER_DIR is not an absolute path".into()));
        assert_eq!(AGENTS.to_vec(), SETTINGS.iter().map(|s| s.0).collect::<Vec<_>>());
    }

    #[test]
    fn sessions_on_without_settings_read_nothing() {
        // a made-up home holding every agent's folder, each with a session, and a project root
        // with Aider's history: with sessions on and no folder set, nothing is read or saved
        let d = tmpdir("no-settings");
        let (home, fake) = (d.join("store"), d.join("fakehome"));
        let sid = "11111111-2222-4333-8444-555555555555";
        let claude = fake.join(".claude").join("projects").join("-w-app");
        let codex = fake.join(".codex").join("sessions").join("2026").join("11").join("18");
        let cursor = fake.join("Library").join("Application Support").join("Cursor").join("User").join("globalStorage");
        for dir in [&claude, &codex, &cursor, &fake.join("app")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(claude.join(format!("{sid}.jsonl")), "{\"cwd\":\"/w/app\",\"type\":\"user\"}\n").unwrap();
        fs::write(codex.join(format!("rollout-2026-11-18T10-00-00-{sid}.jsonl")), "{\"type\":\"session_meta\"}\n").unwrap();
        fs::write(cursor.join("state.vscdb"), "not read").unwrap();
        fs::write(fake.join("app").join(".aider.chat.history.md"), "#### hi\n").unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config"), "sessions = on\n").unwrap();
        // (the agents' own settings point at the made-up folders too: still not used)
        let f = fake.display().to_string();
        let env = [("HOME", f.clone()), ("CLAUDE_CONFIG_DIR", format!("{f}/.claude")), ("CODEX_HOME", format!("{f}/.codex")), ("XDG_CONFIG_HOME", f.clone())];
        let get = |k: &str| env.iter().find(|(a, _)| *a == k).map(|(_, v)| OsString::from(v));
        let r = sweep(&home, &Places::with(&get), true, &AtomicBool::new(false));
        assert!(!r.off && r.saved.is_empty() && r.failed.is_empty() && r.unchanged == 0, "{r:?}");
        for (agent, setting) in SETTINGS {
            assert!(r.notes.iter().any(|n| n.starts_with(&format!("{agent}: {setting} is not set")) && n.ends_with("passed over")), "{agent}: {:?}", r.notes);
        }
        assert_eq!(r.notes.len(), 5, "{:?}", r.notes);
        assert!(!home.join("projects").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn session_ids_from_file_names() {
        let id = |p: &str| session_id(Path::new(p)).unwrap();
        assert_eq!(id("/p/11111111-2222-4333-8444-555555555555.jsonl"), "11111111-2222-4333-8444-555555555555");
        assert_eq!(id("/s/2026/11/18/rollout-2026-11-18T10-15-02-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl"), "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b");
        assert_eq!(id("/s/rollout-2026-11-18T10-15-02-abc.jsonl"), "rollout-2026-11-18T10-15-02-abc");
        assert_eq!(id("/p/agent-1a2b3c4d.jsonl"), "agent-1a2b3c4d");
        // a UUID glued to other text is not taken alone
        assert_eq!(id("/p/x11111111-2222-4333-8444-555555555555.jsonl"), "x11111111-2222-4333-8444-555555555555");
        assert!(is_uuid(b"ABCDEF01-2222-4333-8444-555555555555") && !is_uuid(b"ABCDEF01-2222-4333-8444-55555555555g"));
    }

    /// A random JSON value, nested at most `depth` deep.
    fn random_json(r: &mut impl FnMut(usize) -> usize, depth: u32) -> Json {
        let strs = ["", "a", "x ] y", "q \" , { [", "back\\slash", "é ü", "tab\there", "line\nend"];
        match r(if depth == 0 { 4 } else { 6 }) {
            0 => Json::Null,
            1 => Json::Bool(r(2) == 0),
            2 => Json::Num(["0", "-1", "3.25", "1e9", "-0"][r(5)].to_string()),
            3 => Json::Str(strs[r(strs.len())].to_string()),
            4 => Json::Arr((0..r(4)).map(|_| random_json(r, depth - 1)).collect()),
            _ => Json::Obj((0..r(4)).map(|i| (format!("k{i}{}", strs[r(3)]), random_json(r, depth - 1))).collect()),
        }
    }

    #[test]
    fn array_items_keep_each_elements_bytes() {
        let docs = [
            "[]",
            " [ ] \n",
            "[1]",
            r#"[{"role":"user","content":[{"type":"text","text":"a ] b , c } \" ["}]},{"role":"assistant","content":"x"}]"#,
            "[\n  {\n    \"a\": [1, 2]\n  },\n  \"s\\\\\",\n  null\n]\n",
            r#"["é", 1.50, -0, true, {"k":{}}, [[]], "\"\\"]"#,
        ];
        let check = |d: &[u8]| {
            let v = json::parse(d).unwrap();
            let items = array_items(d).unwrap();
            let want = v.arr().unwrap();
            assert_eq!(items.len(), want.len(), "{}", String::from_utf8_lossy(d));
            for (it, w) in items.iter().zip(want) {
                assert_eq!(&json::parse(it).unwrap(), w, "{}", String::from_utf8_lossy(d));
                // the element's own bytes: no white space around it
                assert!(!it.is_empty() && !it[0].is_ascii_whitespace() && !it[it.len() - 1].is_ascii_whitespace());
            }
        };
        for d in docs {
            check(d.as_bytes());
        }
        // random arrays, written compact and with white space between the parts
        let mut r = rng(7);
        for _ in 0..500 {
            let n = r(6);
            let vals: Vec<Json> = (0..n).map(|_| random_json(&mut r, 3)).collect();
            let mut compact = String::new();
            json::write(&Json::Arr(vals.clone()), &mut compact);
            check(compact.as_bytes());
            let ws = ["", " ", "\n", "\r\n  ", "\t"];
            let mut spaced = format!("{}[{}", ws[r(5)], ws[r(5)]);
            for (i, v) in vals.iter().enumerate() {
                if i > 0 {
                    spaced.push_str(&format!("{},{}", ws[r(5)], ws[r(5)]));
                }
                json::write(v, &mut spaced);
            }
            spaced.push_str(&format!("{}]{}", ws[r(5)], ws[r(5)]));
            check(spaced.as_bytes());
        }
        assert!(array_items(br#"{"a":[1]}"#).is_none());
        assert!(array_items(b"  \"[1]\"").is_none());
    }

    #[test]
    fn lines_back_finds_where_lines_start() {
        let text = b"a\nbb\n\nccc\nd\n";
        assert_eq!(lines_back_in(text, text.len(), 0), text.len());
        assert_eq!(lines_back_in(text, text.len(), 1), 10);
        assert_eq!(lines_back_in(text, text.len(), 2), 6);
        assert_eq!(lines_back_in(text, text.len(), 3), 5);
        assert_eq!(lines_back_in(text, text.len(), 5), 0);
        assert_eq!(lines_back_in(text, text.len(), 9), 0);
        assert_eq!(lines_back_in(text, 5, 1), 2);
        assert_eq!(lines_back_in(text, 0, 3), 0);
        // the file's version agrees, also across its reads of 64 KiB
        let d = tmpdir("back");
        let mut big = Vec::new();
        let mut starts = vec![0];
        for i in 0..20_000 {
            big.extend_from_slice(format!("line {i} {}\n", "x".repeat(i % 50)).as_bytes());
            starts.push(big.len());
        }
        fs::write(d.join("f"), &big).unwrap();
        let mut f = File::open(d.join("f")).unwrap();
        let mut r = rng(3);
        for _ in 0..200 {
            let end = starts[r(starts.len())];
            let n = [0, 1, 2, 999, 1000, 5000, 30000][r(7)];
            assert_eq!(lines_back(&mut f, end as u64, n).unwrap(), lines_back_in(&big, end, n) as u64, "end {end} n {n}");
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_tail_is_hidden_as_the_whole_text_would_be() {
        // a private key's lines (pure base64), a named value and a token: every cut at a line
        // start hides the tail exactly as one pass over the whole text does
        let body: String = (0..40).map(|i| format!("{}{}\n", "QUJD".repeat(15), ["", "x"][i % 2])).collect();
        let text = format!("start\n-----BEGIN RSA PRIVATE KEY-----\n{body}-----END RSA PRIVATE KEY-----\npassword=hunter2x\nlast line\n");
        let text = text.as_bytes();
        let whole: Vec<Vec<u8>> = {
            let mut h = Hider::new(Vec::new());
            text.split_inclusive(|&c| c == b'\n')
                .map(|l| {
                    let mut o = Vec::new();
                    h.line(l, &mut o);
                    o
                })
                .collect()
        };
        let mut unprimed_differs = false;
        let mut at = 0;
        for (k, l) in text.split_inclusive(|&c| c == b'\n').enumerate() {
            let cut = at;
            at += l.len();
            for primed in [true, false] {
                let mut h = Hider::new(Vec::new());
                let mut sink = Vec::new();
                if primed {
                    for l in text[lines_back_in(text, cut, secrets::MAX_KEY_LINES)..cut].split_inclusive(|&c| c == b'\n') {
                        sink.clear();
                        h.line(l, &mut sink);
                    }
                }
                let before = h.counts().total();
                let mut out = Vec::new();
                for l in text[cut..].split_inclusive(|&c| c == b'\n') {
                    h.line(l, &mut out);
                }
                let want: Vec<u8> = whole[k..].concat();
                if primed {
                    assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&want), "cut at line {k}");
                    let markers = |b: &[u8]| b.windows(secrets::MARKER.len()).filter(|w| *w == secrets::MARKER).count() as u64;
                    assert_eq!(h.counts().total() - before, markers(&want), "cut at line {k}");
                } else if out != want {
                    unprimed_differs = true;
                }
            }
        }
        // (without the lines before, a cut inside the key would leave its lines in view)
        assert!(unprimed_differs);
    }

    #[test]
    fn session_lines_name_their_folder_and_edits() {
        let l = br#"{"type":"assistant","cwd":"/w/app","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/app/src/a.rs","old_string":"x","new_string":"y"}},{"type":"tool_use","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","name":"NotebookEdit","input":{"notebook_path":"/w/app/n.ipynb"}},{"type":"tool_use","name":"Write","input":{"file_path":"/w/app/b.rs","content":"..."}},{"type":"tool_use","name":"MultiEdit","input":{"file_path":"/w/app/c.rs","edits":[]}}]}}
"#;
        assert_eq!(line_edits(l), vec!["/w/app/src/a.rs", "/w/app/n.ipynb", "/w/app/b.rs", "/w/app/c.rs"]);
        assert_eq!(line_cwd(l).as_deref(), Some("/w/app"));
        // Codex names its folder in session_meta and turn_context lines
        assert_eq!(line_cwd(br#"{"timestamp":"t","type":"session_meta","payload":{"id":"x","cwd":"/w/codex"}}"#).as_deref(), Some("/w/codex"));
        assert_eq!(line_cwd(b"{\"type\":\"turn_context\",\"payload\":{\"cwd\":\"/w/c2\"}}\r\n").as_deref(), Some("/w/c2"));
        // not a folder: relative, not a string, not JSON, a word in a text
        for bad in [&br#"{"cwd":"rel/x"}"#[..], br#"{"cwd":7}"#, br#"{"cwd":"/x""#, br#"{"text":"the \"cwd\" is /x"}"#] {
            assert_eq!(line_cwd(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        // no edits: a tool result naming Edit, a block that is not a call, a call without its path
        for none in [
            &br#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Edit done: {\"type\":\"tool_use\"}"}]}}"#[..],
            br#"{"type":"assistant","message":{"content":[{"type":"text","name":"Edit","input":{"file_path":"/x"}}]}}"#,
            br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{}}]}}"#,
            br#"{"type":"assistant","message":{"content":"tool_use"}}"#,
        ] {
            assert!(line_edits(none).is_empty(), "{}", String::from_utf8_lossy(none));
        }
    }

    #[test]
    fn rows_of_sqlite_json_output() {
        assert_eq!(row_of(b"[{\"key\":\"a\",\"value\":\"x\"},\n"), b"{\"key\":\"a\",\"value\":\"x\"}");
        assert_eq!(row_of(b"{\"key\":\"b\",\"value\":\"]\"},\n"), b"{\"key\":\"b\",\"value\":\"]\"}");
        assert_eq!(row_of(b"{\"key\":\"c\",\"value\":null}]\n"), b"{\"key\":\"c\",\"value\":null}");
        assert_eq!(row_of(b"[{\"key\":\"d\",\"value\":\"[\"}]\n"), b"{\"key\":\"d\",\"value\":\"[\"}");
    }

    #[test]
    fn the_seen_file_reads_back() {
        let d = tmpdir("seen");
        let p = d.join(SEEN);
        assert_eq!(Seen::read(&p), Seen::default());
        let s = Seen { stamp: "20480 1763485200.000000001 -".into(), rows: ["0123456789abcdef0123456789abcdef".to_string(), "f".repeat(32)].into_iter().collect() };
        s.write(&p, "/x/Cursor/User/globalStorage/state.vscdb").unwrap();
        assert_eq!(Seen::read(&p), s);
        let t = fs::read_to_string(&p).unwrap();
        assert!(t.starts_with("# the rows of \"/x/Cursor/User/globalStorage/state.vscdb\" saved by fewer"), "{t}");
        // damaged: read as empty (every row saved again; none lost)
        fs::write(&p, "stamp = x\n").unwrap();
        assert_eq!(Seen::read(&p), Seen::default());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn each_line_gives_whole_lines_and_the_rest() {
        let text = b"one\ntwo\r\n\nthree";
        let mut got = Vec::new();
        let rest = each_line(&mut &text[..], text.len() as u64, &mut |l| {
            got.push(l.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![b"one\n".to_vec(), b"two\r\n".to_vec(), b"\n".to_vec()]);
        assert_eq!(rest, b"three");
        // only `len` bytes are read
        let mut n = 0;
        let rest = each_line(&mut &text[..], 6, &mut |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!((n, rest), (1, b"tw".to_vec()));
    }
}
