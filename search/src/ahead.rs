//! Read-ahead (2026-11-26): what a message names, read and run by fewer's own tools before
//! Claude's first reply, and given to Claude with the message (a UserPromptSubmit hook's
//! `additionalContext`), so that a question about them is answered in the first reply. With fewer
//! a reply costs about 16,700 tokens (Claude Code's own tools and instructions, read again at each
//! one), and most jobs took two: one to fetch the facts, one to answer (the benchmark's tasks,
//! 2026-11-26: 7 of 9 answered in one reply with it, all of them right).
//!
//! What a message can name, and what is given for it:
//! - files and folders of the project: a path that is there (as written or in backticks), and
//!   "n01.py to n20.py" for the ones between: `fewer read`'s view (many files: its view of many),
//!   or a table's or a log's `fewer table` view;
//! - the last commit ("the last commit", "latest commit", HEAD, a commit's hash): `fewer changes`;
//! - a web page (an address the message asks about) or a web search ("search the web", "on the
//!   web"): `fewer web` with the message as its question, which holds to the session's web rules;
//! - a test, build or lint command in backticks that the message asks to run, when the session's
//!   rules let it run with no question asked (a Bash allow rule, no deny or ask rule; never in
//!   plan mode): `fewer run --short`;
//! - a failing Python test file ("... in tests/test_x.py fails"): the test file, the project's
//!   modules it imports, and its run (under the same rules).
//!
//! All within ROOM characters (Claude Code moves a hook's text past 10,000 characters to a file,
//! which costs a reply to read: 2.1.332, lDe=1e4). Nothing read past the session's Read rules and
//! the project's bounds; secrets hidden, as in every view of fewer's. Off: `ahead = off` in
//! fewer's config. On any trouble, nothing: Claude Code goes on as without it.

use std::ffi::OsString;
use std::io::Read;
use crate::portable::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::Json;

/// The most characters given (under Claude Code's 10,000, which it shares among the prompt's
/// hooks: room for another hook's few lines).
pub const ROOM: usize = 9_000;
/// How long a command may run, a web read take, and all of read-ahead (seconds; Claude Code
/// stops a hook at 60).
const RUN_SECS: u64 = 30;
const WEB_SECS: u64 = 40;
const ALL_SECS: u64 = 50;
/// The most files of a folder or a range read.
const MAX_FILES: usize = 300;

/// The line before what is given.
pub const HEAD: &str = "fewer, before your first reply: what your message names, read and run just now in this folder (exact and current; nothing left out unless a line says so; secrets hidden). Use it as your own reads and runs: no need to do them again.";

/// One thing to read or run ahead.
#[derive(Debug, PartialEq, Eq)]
enum Job {
    /// Files (as the message names them, relative to the folder), read in one view.
    Files(Vec<String>),
    /// A table or a log: `fewer table`.
    Table(String),
    /// A commit: `fewer changes REV`.
    Changes(String),
    /// The message, as `fewer web`'s question.
    Web,
    /// A command to run (its words).
    Run(Vec<String>),
}

/// UserPromptSubmit: what the message names, read and run, as the hook's JSON; None when it names
/// nothing fewer reads ahead, or read-ahead is off.
pub fn ahead(j: &Json) -> Option<String> {
    let prompt = j.get("prompt").and_then(Json::str)?.trim();
    // (a slash command is Claude Code's own; a very long message is mostly its own text)
    if prompt.is_empty() || prompt.starts_with('/') || prompt.len() > 20_000 {
        return None;
    }
    let home = crate::store::home().ok()?;
    if crate::config::get(&crate::config::read(&home), "ahead") == Some("off") {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let mode = j.get("permission_mode").and_then(Json::str).unwrap_or("default");
    let jobs = jobs_of(prompt, &cwd, &crate::store::project_root(&cwd));
    if jobs.is_empty() {
        return None;
    }
    let rules = crate::perms::Rules::load_session(&cwd);
    let text = give(prompt, &jobs, &cwd, &rules, mode);
    if text.is_empty() {
        return None;
    }
    let context = format!("{HEAD}\n{text}");
    Some(Json::o(vec![("hookSpecificOutput", Json::o(vec![("hookEventName", "UserPromptSubmit".into()), ("additionalContext", context.into())]))]).to_string())
}

/// What the jobs give, each after the command that gives it, within ROOM (with HEAD).
fn give(prompt: &str, jobs: &[Job], cwd: &Path, rules: &crate::perms::Rules, mode: &str) -> String {
    let start = Instant::now();
    let me = crate::savers::me();
    let mut out = String::new();
    for job in jobs {
        if start.elapsed() > Duration::from_secs(ALL_SECS) {
            break;
        }
        let room = ROOM.saturating_sub(HEAD.len() + 1 + out.len());
        if room < 400 {
            break;
        }
        let (label, body) = match job {
            Job::Files(files) => {
                // (a file the Read rules deny or ask about is not read: Claude reads it with Claude
                // Code's Read tool, which asks as usual)
                let mut notes = String::new();
                let mut ok: Vec<OsString> = Vec::new();
                for f in files {
                    match crate::perms::refused_read(f, &rules.read(Path::new(f), cwd)) {
                        Some(line) => notes.push_str(&format!("== {line}\n")),
                        None => ok.push(OsString::from(f)),
                    }
                }
                if ok.is_empty() {
                    continue;
                }
                let view = crate::read::ahead_view(&ok, room.saturating_sub(notes.len() + 200), &me);
                (format!("$ {me} read {}", names(files)), format!("{notes}{view}"))
            }
            Job::Table(f) => {
                if crate::perms::refused_read(f, &rules.read(Path::new(f), cwd)).is_some() {
                    continue;
                }
                match run_self(&["table", f.as_str()], RUN_SECS, cwd) {
                    Some((t, Some(0))) => (format!("$ {me} table {f}"), t),
                    _ => continue,
                }
            }
            Job::Changes(rev) => match run_self(&["changes", rev.as_str()], RUN_SECS, cwd) {
                Some((t, Some(0))) => (format!("$ {me} changes {rev}"), t),
                _ => continue,
            },
            Job::Web => match run_self(&["web", prompt], WEB_SECS, cwd) {
                // (refused by the session's web rules, or no answer: Claude reads the web itself)
                Some((t, Some(0))) => (format!("$ {me} web \"(your message)\""), t),
                _ => continue,
            },
            Job::Run(words) => {
                let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
                let allowed = rules.allows_command(&argv) || mode == "bypassPermissions" && rules.command(&argv) == crate::perms::Verdict::Free;
                if mode == "plan" || !allowed {
                    continue;
                }
                let mut args = vec!["run", "--short", "--"];
                args.extend(words.iter().map(String::as_str));
                match run_self(&args, RUN_SECS, cwd) {
                    Some((t, Some(_))) => (format!("$ {me} run --short -- {}", words.join(" ")), t),
                    Some((t, None)) => (format!("$ {me} run --short -- {}", words.join(" ")), format!("{t}(still running after {RUN_SECS} seconds: stopped; run it yourself)\n")),
                    None => continue,
                }
            }
        };
        let mut piece = format!("{label}\n{body}");
        if !piece.ends_with('\n') {
            piece.push('\n');
        }
        // (an output too long for the room left: named, for Claude to run)
        if piece.len() > room {
            let note = format!("{label}: left out, {} characters (run it if the job needs it)\n", group(piece.len()));
            if note.len() <= room {
                out.push_str(&note);
            }
            continue;
        }
        out.push_str(&piece);
    }
    out
}

/// Files as a command names them: up to five, then how many in all.
fn names(files: &[String]) -> String {
    if files.len() <= 5 {
        return files.join(" ");
    }
    format!("{} … ({} files)", files[..4].join(" "), files.len())
}

fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The jobs a message names, in the order it names them (files once each).
/// (`root`: the project's, whose bounds nothing is read past)
fn jobs_of(prompt: &str, cwd: &Path, root: &Path) -> Vec<Job> {
    let lower = prompt.to_lowercase();
    let ws = words(prompt);
    let mut jobs: Vec<Job> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let add_file = |f: String, files: &mut Vec<String>| {
        if !files.contains(&f) && files.len() < MAX_FILES {
            files.push(f);
        }
    };
    // commands to run, from the message's backticks
    for (at, span) in backticks(prompt) {
        if let Some(words) = command_to_run(prompt, at, span) {
            let job = Job::Run(words);
            if !jobs.contains(&job) {
                jobs.push(job);
            }
        }
    }
    // files and folders, and ranges of files ("n01.py to n20.py")
    let mut skip_next = 0;
    for (k, &(at, w)) in ws.iter().enumerate() {
        if skip_next > 0 {
            skip_next -= 1;
            continue;
        }
        if not_to_read(&lower, at) {
            continue;
        }
        if let (Some(&(_, mid)), Some(&(_, last))) = (ws.get(k + 1), ws.get(k + 2))
            && RANGE_WORDS.contains(&mid.to_lowercase().as_str())
            && let Some(between) = range(w, last)
        {
            let there: Vec<String> = between.into_iter().filter(|f| inside(f, cwd, root).is_some_and(|p| p.is_file())).collect();
            if there.len() >= 2 {
                for f in there {
                    add_file(f, &mut files);
                }
                skip_next = 2;
                continue;
            }
        }
        let Some(p) = path_named(w, cwd, root) else { continue };
        if p.is_dir() {
            for f in folder_files(w, cwd) {
                add_file(f, &mut files);
            }
        } else if is_table(w) {
            let job = Job::Table(w.to_string());
            if !jobs.contains(&job) {
                jobs.push(job);
            }
        } else {
            add_file(w.to_string(), &mut files);
        }
    }
    // a failing Python test: the modules it imports, and its run
    let failing = ["fail", "broken", "breaks", "does not pass", "doesn't pass", "not passing"].iter().any(|w| lower.contains(w));
    if failing {
        let tests: Vec<String> = files.iter().filter(|f| is_python_test(f)).cloned().collect();
        for t in tests.iter().take(2) {
            for m in python_imports(t, cwd, root) {
                add_file(m, &mut files);
            }
            if let Some(run) = python_test_run(t, cwd) {
                let job = Job::Run(run);
                if !jobs.contains(&job) {
                    jobs.push(job);
                }
            }
        }
    }
    if !files.is_empty() {
        // (the files first: what is read is what a command or a test is about)
        jobs.insert(0, Job::Files(files));
    }
    if let Some(rev) = commit_named(&lower, &ws, cwd) {
        jobs.push(Job::Changes(rev));
    }
    if web_asked(&lower, &ws) {
        jobs.push(Job::Web);
    }
    jobs
}

/// The words between two paths that name the files between them.
const RANGE_WORDS: [&str; 7] = ["to", "through", "thru", "until", "-", "–", ".."];

/// The message's words with where each starts, the punctuation around them taken off.
fn words(text: &str) -> Vec<(usize, &str)> {
    let mut raw = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                raw.push((s, &text[s..i]));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        raw.push((s, &text[s..]));
    }
    raw.into_iter()
        .map(|(s, w)| {
            let t = w.trim_start_matches(|c: char| "([{<\"'`*".contains(c));
            let at = s + (w.len() - t.len());
            (at, t.trim_end_matches(|c: char| ")]}>\"'`,:;!?.*".contains(c)))
        })
        .filter(|(_, t)| !t.is_empty())
        .collect()
}

/// The message's spans in backticks (one line each, not code fences), with where each starts.
fn backticks(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(a) = text[i..].find('`') {
        let a = i + a;
        let run = text[a..].bytes().take_while(|&c| c == b'`').count();
        if run > 1 {
            // (a fence or a double tick: its closing one, then on)
            let mark = &text[a..a + run];
            match text[a + run..].find(mark) {
                Some(b) => i = a + run + b + run,
                None => break,
            }
            continue;
        }
        let Some(b) = text[a + 1..].find('`') else { break };
        let span = &text[a + 1..a + 1 + b];
        if !span.contains('\n') && !span.trim().is_empty() && span.len() <= 300 {
            out.push((a + 1, span.trim()));
        }
        i = a + 1 + b + 1;
    }
    out
}

/// Whether the words just before `at` say not to read what comes (delete, remove, ignore...).
fn not_to_read(lower: &str, at: usize) -> bool {
    let from = lower[..at.min(lower.len())].char_indices().rev().nth(24).map_or(0, |(i, _)| i);
    let before = &lower[from..at.min(lower.len())];
    ["delete ", "remove ", "rm ", "rename ", "move ", "ignore ", "skip ", "don't read", "do not read", "without reading", "except ", "not "].iter().any(|w| before.contains(w))
}

/// A word that names a file or a folder of the project: a path (a `/` in it, or a name with an
/// extension) that is there, inside the project's bounds (where it really is).
fn path_named(w: &str, cwd: &Path, root: &Path) -> Option<PathBuf> {
    if w.contains("://") || w.starts_with('-') || w.starts_with('~') || w.len() > 400 {
        return None;
    }
    let ext = Path::new(w).extension().and_then(|e| e.to_str()).is_some_and(|e| !e.is_empty() && e.len() <= 10 && e.chars().all(|c| c.is_ascii_alphanumeric()) && e.chars().any(|c| c.is_ascii_alphabetic()));
    if !(w.contains('/') || w.contains('\\') || ext) {
        return None;
    }
    inside(w, cwd, root)
}

/// The path, when it is there and inside the project (where it really is).
fn inside(w: &str, cwd: &Path, root: &Path) -> Option<PathBuf> {
    let p = if Path::new(w).is_absolute() { PathBuf::from(w) } else { cwd.join(w) };
    let real = std::fs::canonicalize(&p).ok()?;
    let top = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    real.starts_with(&top).then_some(p)
}

/// A folder's files, as the message names the folder: the ones git holds (no ignored caches),
/// else the files in it to three levels down, past hidden ones and caches; MAX_FILES at most.
fn folder_files(dir: &str, cwd: &Path) -> Vec<String> {
    if let Ok(o) = Command::new("git").args(["ls-files", "-z", "--"]).arg(dir).current_dir(cwd).stdin(Stdio::null()).stderr(Stdio::null()).output()
        && o.status.success()
    {
        let list: Vec<String> = o.stdout.split(|&c| c == 0).filter(|s| !s.is_empty()).take(MAX_FILES).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        if !list.is_empty() {
            return list;
        }
    }
    let mut out = Vec::new();
    walk(&cwd.join(dir), Path::new(dir), 0, &mut out);
    out.sort();
    out.truncate(MAX_FILES);
    out
}

fn walk(at: &Path, shown: &Path, depth: usize, out: &mut Vec<String>) {
    const CACHES: [&str; 8] = ["node_modules", "target", "__pycache__", "dist", "build", "venv", ".venv", "vendor"];
    let Ok(rd) = std::fs::read_dir(at) else { return };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || CACHES.contains(&name.as_str()) {
            continue;
        }
        let p = shown.join(&name);
        match e.file_type() {
            Ok(t) if t.is_dir() && depth < 2 => walk(&e.path(), &p, depth + 1, out),
            Ok(t) if t.is_file() => out.push(p.to_string_lossy().into_owned()),
            _ => {}
        }
    }
}

/// A table or a log, by its name: `fewer table` gives its sums and counts.
fn is_table(w: &str) -> bool {
    let e = Path::new(w).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(e.as_str(), "csv" | "tsv" | "log" | "jsonl" | "ndjson")
}

/// The files between two names that differ in one number only ("calc/n01.py", "calc/n20.py"),
/// written as wide as the first (01): None when they are not such a pair, or too far apart.
fn range(a: &str, b: &str) -> Option<Vec<String>> {
    let (sa, na) = numbers(a);
    let (sb, nb) = numbers(b);
    if sa != sb || na.len() != nb.len() {
        return None;
    }
    let diff: Vec<usize> = (0..na.len()).filter(|&k| na[k].1 != nb[k].1).collect();
    let [k] = diff[..] else { return None };
    let (x, y): (u64, u64) = (na[k].1.parse().ok()?, nb[k].1.parse().ok()?);
    if y <= x || y - x >= MAX_FILES as u64 {
        return None;
    }
    let (at, num) = na[k];
    let width = num.len();
    Some((x..=y).map(|v| format!("{}{v:0width$}{}", &a[..at], &a[at + num.len()..])).collect())
}

/// A name's shape (its numbers as #) and its numbers with where each starts.
fn numbers(s: &str) -> (String, Vec<(usize, &str)>) {
    let b = s.as_bytes();
    let (mut shape, mut nums) = (String::new(), Vec::new());
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let j = i + b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            nums.push((i, &s[i..j]));
            shape.push('#');
            i = j;
        } else {
            let c = s[i..].chars().next().unwrap_or('?');
            shape.push(c);
            i += c.len_utf8();
        }
    }
    (shape, nums)
}

/// The commit a message names: the last one ("the last commit", HEAD), or one by its hash that
/// this repository holds.
fn commit_named(lower: &str, ws: &[(usize, &str)], cwd: &Path) -> Option<String> {
    let says = ["last commit", "latest commit", "most recent commit", "recent commit", "newest commit", "head commit"];
    if says.iter().any(|s| lower.contains(s)) {
        return Some("HEAD".into());
    }
    for &(_, w) in ws {
        if w == "HEAD" || (w.starts_with("HEAD~") || w.starts_with("HEAD^")) && w.len() <= 8 {
            return Some(w.to_string());
        }
        let hex = (7..=40).contains(&w.len()) && w.chars().all(|c| c.is_ascii_hexdigit()) && w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_alphabetic());
        if hex {
            let ok = Command::new("git").args(["rev-parse", "--verify", "--quiet", &format!("{w}^{{commit}}")]).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
            if ok {
                return Some(w.to_string());
            }
        }
    }
    None
}

/// Whether the message asks for the web: a search ("search the web"), or a page by its address
/// that it asks about (read, look at, what does it say...).
fn web_asked(lower: &str, ws: &[(usize, &str)]) -> bool {
    const SEARCH: [&str; 11] = ["search the web", "web search", "search online", "online search", "on the web", "search the internet", "on the internet", "look it up online", "look up online", "google it", "from the web"];
    const ABOUT: [&str; 13] = ["read", "look at", "fetch", "open", "visit", "summar", "according to", "page", "article", "what does", "what do", "says", "explain"];
    if SEARCH.iter().any(|s| lower.contains(s)) {
        return true;
    }
    let address = ws.iter().any(|(_, w)| w.starts_with("http://") || w.starts_with("https://"));
    address && ABOUT.iter().any(|s| lower.contains(s))
}

/// Programs read-ahead runs (tests, builds, lints, type checks), with the subcommands it runs
/// them with (none: any): never an install, a deploy, a publish, a script of the project's own.
const RUNNERS: [(&str, &[&str]); 30] = [
    ("pytest", &[]),
    ("py.test", &[]),
    ("jest", &[]),
    ("vitest", &[]),
    ("mocha", &[]),
    ("tsc", &[]),
    ("eslint", &[]),
    ("ruff", &["check"]),
    ("mypy", &[]),
    ("pylint", &[]),
    ("flake8", &[]),
    ("rubocop", &[]),
    ("rspec", &[]),
    ("phpunit", &[]),
    ("shellcheck", &[]),
    ("golangci-lint", &["run"]),
    ("cargo", &["test", "check", "clippy", "build"]),
    ("go", &["test", "vet", "build"]),
    ("npm", &["test", "t"]),
    ("pnpm", &["test", "t"]),
    ("yarn", &["test"]),
    ("bun", &["test"]),
    ("npx", &["jest", "vitest", "tsc", "eslint", "mocha"]),
    ("dotnet", &["test", "build"]),
    ("swift", &["test", "build"]),
    ("mix", &["test"]),
    ("zig", &["test"]),
    ("deno", &["test", "lint", "check"]),
    ("make", &["test", "check", "lint"]),
    ("python3", &[]),
];

/// Python's modules read-ahead runs with -m.
const PYTHON_MODULES: [&str; 8] = ["pytest", "unittest", "mypy", "ruff", "pylint", "flake8", "compileall", "doctest"];

/// The command in a message's backticks at `at`, when the message asks to run it ("run", "with",
/// and no "don't" before it) and it is a test, build or lint run: its words (no pipes, no
/// redirections but 2>&1, no second command).
fn command_to_run(prompt: &str, at: usize, span: &str) -> Option<Vec<String>> {
    let lower = prompt.to_lowercase();
    let from = lower[..at.min(lower.len())].char_indices().rev().nth(60).map_or(0, |(i, _)| i);
    let before = &lower[from..at.min(lower.len())];
    // (the sentence the span is in)
    let before = before.rsplit(['.', '\n', '?', '!']).next().unwrap_or(before);
    if ["don't", "do not", "dont", "never", "without", "instead of", "not ", "avoid", "skip"].iter().any(|w| before.contains(w)) {
        return None;
    }
    let after: String = lower[(at + span.len()).min(lower.len())..].chars().take(40).collect();
    let asked = ["run", "with", "execute", "using"].iter().any(|w| before.contains(w)) || ["fail", "error", "output", "print", "say"].iter().any(|w| after.contains(w));
    if !asked {
        return None;
    }
    let all = crate::hook::pipelines(span);
    let [pipe] = &all[..] else { return None };
    let [stage] = &pipe[..] else { return None };
    let words: Vec<String> = stage.iter().filter(|w| *w != "2>&1").cloned().collect();
    if words.iter().any(|w| w.contains('>') || w.contains('<') || w.contains('$') || w.contains('`')) {
        return None;
    }
    let first = words.first()?;
    // (a program named by its own path, or with settings before it: not read ahead)
    if first.contains('/') || first.contains('=') {
        return None;
    }
    let name = first.strip_suffix(".exe").unwrap_or(first);
    let name = if name.starts_with("python3.") || name == "python" { "python3" } else { name };
    let (_, subs) = RUNNERS.iter().find(|(p, _)| *p == name)?;
    let rest = &words[1..];
    if name == "python3" {
        let m = rest.iter().position(|w| w == "-m").and_then(|i| rest.get(i + 1))?;
        if !PYTHON_MODULES.contains(&m.as_str()) {
            return None;
        }
    } else if !subs.is_empty() {
        let sub = rest.iter().find(|w| !w.starts_with('-'))?;
        if !subs.contains(&sub.as_str()) {
            return None;
        }
    }
    Some(words)
}

/// A Python test file, by its name (test_x.py, x_test.py, or a .py file under tests/ or test/).
fn is_python_test(f: &str) -> bool {
    let p = Path::new(f);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py") || p.components().any(|c| matches!(c.as_os_str().to_str(), Some("tests" | "test"))))
}

/// The project's own modules a Python file imports (`from calc.pricing import x`, `import calc.m`),
/// as files there: three at most.
fn python_imports(f: &str, cwd: &Path, root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(cwd.join(f)) else { return Vec::new() };
    let mut out = Vec::new();
    for l in text.lines() {
        let module = if let Some(r) = l.strip_prefix("from ") {
            r.split_whitespace().next()
        } else if let Some(r) = l.strip_prefix("import ") {
            r.split([',', ' ']).next()
        } else {
            None
        };
        let Some(m) = module.filter(|m| !m.starts_with('.') && m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')) else { continue };
        let base = m.replace('.', "/");
        for cand in [format!("{base}.py"), format!("{base}/__init__.py")] {
            if inside(&cand, cwd, root).is_some_and(|p| p.is_file()) && !out.contains(&cand) {
                out.push(cand);
                break;
            }
        }
        if out.len() >= 3 {
            break;
        }
    }
    out
}

/// A Python test file's run: unittest when it uses it (its module, verbose), else pytest.
fn python_test_run(f: &str, cwd: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(cwd.join(f)).ok()?;
    let python = if cfg!(windows) { "python" } else { "python3" };
    if text.contains("unittest") {
        let module = f.strip_suffix(".py")?.replace(['/', '\\'], ".");
        if !module.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            return None;
        }
        Some(vec![python.into(), "-m".into(), "unittest".into(), "-v".into(), module])
    } else if text.contains("def test_") {
        Some(vec![python.into(), "-m".into(), "pytest".into(), "-q".into(), f.to_string()])
    } else {
        None
    }
}

/// One of fewer's own commands, run in `cwd` for at most `secs` seconds: (its output, its exit
/// code; None when it was stopped at the time), or None when it could not start. In a group of its
/// own, so that at the time the program it started is stopped too.
fn run_self(args: &[&str], secs: u64, cwd: &Path) -> Option<(String, Option<i32>)> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    // (the project's bounds kept, as for Claude's own reads in a session)
    if std::env::var_os("CLAUDECODE").is_none() {
        cmd.env("CLAUDECODE", "1");
    }
    let mut p = cmd.spawn().ok()?;
    let mut so = p.stdout.take()?;
    let mut se = p.stderr.take()?;
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let code = loop {
        match p.try_wait() {
            Ok(Some(st)) => break Some(st.code().unwrap_or(1)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                crate::sys_run::send_signal(-(p.id() as i32), crate::sys_run::SIGKILL);
                let _ = p.kill();
                let _ = p.wait();
                break None;
            }
        }
    };
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let e = err.join().unwrap_or_default();
    // (what it said on stderr after, for a command's own errors; not for fewer's refusals)
    if code.is_some_and(|c| c != 0) && !e.is_empty() && args.first() == Some(&"run") {
        text.push_str(&String::from_utf8_lossy(&e));
    }
    Some((text, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up project in a folder of its own (`tag`: each test's own, as tests run at once).
    fn project(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fewer-ahead-{tag}-{}-{}", std::process::id(), crate::times::now_ms()));
        for (f, t) in [
            ("calc/n01.py", "def a():\n    return 1\n"),
            ("calc/n02.py", "def b():\n    return 2\n"),
            ("calc/n03.py", "def c():\n    return 3\n"),
            ("calc/pricing.py", "TAX = 0.08\n"),
            ("data/sales.csv", "region,amount\neast,1\n"),
            ("tests/test_pricing.py", "import unittest\nfrom calc.pricing import TAX\n\nclass T(unittest.TestCase):\n    def test_tax(self):\n        self.assertEqual(TAX, 0.08)\n"),
            ("notes.md", "# notes\n"),
        ] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        }
        d
    }

    #[test]
    fn what_a_message_names() {
        let d = project("names");
        let j = |p: &str| jobs_of(p, &d, &d);
        // a file, a table, a folder, a range
        assert_eq!(j("What does calc/pricing.py set TAX to?"), [Job::Files(vec!["calc/pricing.py".into()])]);
        assert_eq!(j("In data/sales.csv, the total for each region?"), [Job::Table("data/sales.csv".into())]);
        assert_eq!(j("Read the modules in calc/ and say what each returns."), [Job::Files(vec!["calc/n01.py".into(), "calc/n02.py".into(), "calc/n03.py".into(), "calc/pricing.py".into()])]);
        assert_eq!(j("Review calc/n01.py to calc/n03.py one at a time."), [Job::Files(vec!["calc/n01.py".into(), "calc/n02.py".into(), "calc/n03.py".into()])]);
        // (in backticks, with punctuation around; a word that is a folder's name but no path: not read)
        assert_eq!(j("Look at `notes.md`, (calc/n02.py)."), [Job::Files(vec!["notes.md".into(), "calc/n02.py".into()])]);
        assert_eq!(j("Which tests fail in tests? Version 1.0 please."), []);
        // not what is there, outside the project, or to delete
        assert_eq!(j("Write calc/new.py and ../outside.py"), []);
        assert_eq!(j("Delete calc/n01.py please"), []);
        // a failing test: its file, the module it imports, its run
        assert_eq!(j("The test test_tax in tests/test_pricing.py fails. Fix it."), [
            Job::Files(vec!["tests/test_pricing.py".into(), "calc/pricing.py".into()]),
            Job::Run(vec![if cfg!(windows) { "python" } else { "python3" }.into(), "-m".into(), "unittest".into(), "-v".into(), "tests.test_pricing".into()]),
        ]);
        // the web: a search, or an address it asks about (not one only mentioned)
        assert_eq!(j("Search the web: when did Rust 1.0 come out?"), [Job::Web]);
        assert_eq!(j("Read RFC 9110 (https://www.rfc-editor.org/rfc/rfc9110): which code is 416?"), [Job::Web]);
        assert_eq!(j("Our API lives at https://api.example.com, fix the client"), []);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_command_is_run_only_when_asked_and_a_test_or_build() {
        let c = |p: &str| backticks(p).into_iter().find_map(|(at, s)| command_to_run(p, at, s));
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(c("Run the test suite with `python3 -m unittest discover -s tests -v`. Which fail?"), Some(v("python3 -m unittest discover -s tests -v")));
        assert_eq!(c("Why does `cargo test --lib` fail?"), Some(v("cargo test --lib")));
        assert_eq!(c("run `npm test 2>&1`"), Some(v("npm test")));
        // not asked to run, told not to, not a test or build, or more than one command
        assert_eq!(c("I like `cargo test`."), None);
        assert_eq!(c("Don't run `cargo test`, just read the code."), None);
        assert_eq!(c("Run `rm -rf build`"), None);
        assert_eq!(c("Run `npm run deploy`"), None);
        assert_eq!(c("Run `cargo install fewer`"), None);
        assert_eq!(c("Run `python3 deploy.py`"), None);
        assert_eq!(c("Run `cargo test && git push`"), None);
        assert_eq!(c("Run `cargo test > out.txt`"), None);
        assert_eq!(c("Run `./run_tests.sh`"), None);
        assert_eq!(c("Run ```cargo test```"), None);
    }

    #[test]
    fn ranges_and_words() {
        assert_eq!(range("calc/n01.py", "calc/n03.py"), Some(vec!["calc/n01.py".into(), "calc/n02.py".into(), "calc/n03.py".into()]));
        assert_eq!(range("a9.txt", "a11.txt"), Some(vec!["a9.txt".into(), "a10.txt".into(), "a11.txt".into()]));
        assert_eq!(range("v1/a01.py", "v2/a03.py"), None);
        assert_eq!(range("a03.py", "a01.py"), None);
        assert_eq!(range("a.py", "b.py"), None);
        let w: Vec<&str> = words("Read (calc/n01.py), then `x.rs`. Done!").into_iter().map(|(_, w)| w).collect();
        assert_eq!(w, ["Read", "calc/n01.py", "then", "x.rs", "Done"]);
    }

    #[test]
    fn the_commit_a_message_names() {
        let d = project("commit");
        let c = |p: &str| commit_named(&p.to_lowercase(), &words(p), &d);
        assert_eq!(c("What did the last commit change?"), Some("HEAD".into()));
        assert_eq!(c("Explain HEAD~2 for me"), Some("HEAD~2".into()));
        // (a hash this folder's repository does not hold: not one; no repository here)
        assert_eq!(c("What is deadbeef1?"), None);
        assert_eq!(c("Commit the work"), None);
        let _ = std::fs::remove_dir_all(d);
    }
}
