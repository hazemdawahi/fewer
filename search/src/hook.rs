//! Claude Code hooks: the tool doing the work by itself. Each reads the hook's JSON on
//! stdin and prints JSON for Claude Code, or nothing; on any trouble it prints nothing and exits
//! 0, so Claude Code goes on as without it.
//!
//! - `fewer hook post-bash` (PostToolUse, matcher Bash): after a command has run, as it
//!   always runs (the user's own approvals untouched: nothing here decides whether a command
//!   runs). Only a command that succeeds: for one that fails Claude Code calls PostToolUseFailure,
//!   which cannot replace the output, so failing runs are shortened by
//!   `fewer run --short` instead. A long output (over 40 lines or 4,000 bytes) is saved as a run of the project, its
//!   secrets hidden, and replaced for Claude by `run --short`'s view (`updatedToolOutput`: the
//!   tool's own result with its stdout replaced, its other fields as they were). A short output
//!   is left alone.
//! - `fewer hook start` (SessionStart): the start card (history with no input) added to
//!   a new, resumed, cleared or compacted session (`additionalContext`); nothing in a folder
//!   with no memory.
//! - `fewer hook precompact` (PreCompact): when fewer saves
//!   Claude Code's sessions (`sessions = on`), it asks for a short summary (Claude Code adds a
//!   PreCompact hook's output to its summary's instructions): the whole session is kept by fewer,
//!   and the card after the compaction gives back its commands, edits and last errors exactly.
//!   When sessions are not saved it prints nothing, and the summary is Claude Code's own.

use std::ffi::OsString;
use std::io::Read;

use crate::json::{self, Json};

/// The most of a hook's input read.
const INPUT_MAX: u64 = 256 << 20;
/// An output this small is left as it is (run --short shows it whole too).
const WHOLE_LINES: usize = 40;
const WHOLE_BYTES: usize = 4_000;

pub fn cmd_hook(args: Vec<OsString>) -> i32 {
    let which = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let mut input = Vec::new();
    if std::io::stdin().take(INPUT_MAX).read_to_end(&mut input).is_err() {
        return 0;
    }
    let out = match which {
        "post-bash" => post_bash(&input),
        "start" => start(&input),
        "precompact" => precompact(&input),
        _ => {
            eprintln!("usage: fewer hook post-bash|start|precompact  (the hook's JSON on stdin)");
            return 2;
        }
    };
    if let Some(o) = out {
        println!("{o}");
    }
    0
}

/// The hook's input, and its folder made the current one (the project is the folder's).
fn input(b: &[u8]) -> Option<Json> {
    let j = json::parse(b).ok()?;
    if let Some(cwd) = j.get("cwd").and_then(Json::str) {
        std::env::set_current_dir(cwd).ok()?;
    }
    Some(j)
}

/// PostToolUse on Bash: a long output saved and replaced by its short view.
fn post_bash(b: &[u8]) -> Option<String> {
    let j = input(b)?;
    if j.get("tool_name").and_then(Json::str) != Some("Bash") {
        return None;
    }
    // (decided on the command as it was written; its secrets hidden before it is saved or
    // shown, as the output's are: a marker would read as the program's name)
    let raw = j.get("tool_input").and_then(|i| i.get("command")).and_then(Json::str)?;
    let Json::Obj(resp) = j.get("tool_response")? else { return None };
    let field = |k: &str| resp.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.str()).unwrap_or("");
    let (stdout, stderr) = (field("stdout"), field("stderr"));
    // (an output past Claude Code's limit (30,000 characters, 2.1.287): its stdout holds only the
    // start, and the whole output is in the file Claude Code names; read from there, so that what
    // is saved and summed up is all of it, not its first 30,000 characters)
    let whole = persisted(field("persistedOutputPath"));
    let mut text = whole.unwrap_or_else(|| stdout.to_string());
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(stderr);
    }
    // (a test or build run and then fewer reading files, in one call, as Claude may chain them
    // to save a step: the run's part shortened, the files' view kept whole after it)
    let (text, reads) = match run_then_reads(raw, &text) {
        Some((run, reads)) => (run.to_string(), Some(reads.to_string())),
        None => (text, None),
    };
    if text.lines().count() <= WHOLE_LINES && text.len() <= WHOLE_BYTES {
        return None;
    }
    if reads.is_none() && !worth_shortening(raw, text.as_bytes()) {
        return None;
    }
    let command = crate::secrets::hide_str(raw, &crate::secrets::store_extras());
    // (an exit code, when the tool's result gives one)
    let exit = ["exitCode", "exit_code", "returnCode"].iter().find_map(|k| resp.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.int()));
    let argv = vec![OsString::from("sh"), OsString::from("-c"), OsString::from(&command)];
    let (id, saved) = crate::run::save_output(&argv, text.as_bytes(), exit).ok()??;
    let status = match exit {
        Some(0) => "exit 0".to_string(),
        Some(e) => format!("exit {e}"),
        None => "done".to_string(),
    };
    let now = crate::times::now_ms();
    let tags = crate::catalog::Tags { name: command.clone(), file_ms: Some(now), zone: None, arrivals: None, errs: Vec::new(), files: Vec::new() };
    let mut view = crate::short::view(&command, &format!("r{id}"), &status, None, &saved, &tags, now);
    if let Some(reads) = reads {
        if !view.ends_with('\n') {
            view.push('\n');
        }
        view.push_str(&reads);
    }
    // the tool's own result, its stdout the view and its stderr empty (the view holds both)
    let mut out = resp.clone();
    for (k, v) in out.iter_mut() {
        match k.as_str() {
            "stdout" => *v = Json::Str(view.clone()),
            "stderr" => *v = Json::Str(String::new()),
            _ => {}
        }
    }
    Some(Json::o(vec![("hookSpecificOutput", Json::o(vec![("hookEventName", "PostToolUse".into()), ("updatedToolOutput", Json::Obj(out))]))]).to_string())
}

/// Programs whose output is a run's: any use of them builds, tests, lints or installs.
const RUNNERS: [&str; 44] = [
    "make", "gmake", "ninja", "cmake", "ctest", "meson", "pytest", "py.test", "tox", "nox", "jest", "vitest", "mocha", "tsc", "eslint", "ruff", "mypy", "pylint",
    "flake8", "rubocop", "rspec", "phpunit", "xcodebuild", "gradle", "gradlew", "mvn", "mvnw", "sbt", "ant", "rake", "bazel", "bazelisk", "playwright", "cypress",
    "journalctl", "ansible-playbook", "shellcheck", "golangci-lint", "swiftlint", "ktlint", "hadolint", "tflint", "docker-compose", "nextest",
];

/// Programs whose output is a run's with these subcommands (the first word not a flag).
const RUN_WITH: [(&[&str], &[&str]); 18] = [
    (&["cargo"], &["test", "build", "check", "clippy", "bench", "nextest", "install", "doc", "fmt", "update", "fetch"]),
    (&["npm", "pnpm", "yarn", "bun"], &["test", "t", "run", "run-script", "install", "i", "ci", "build", "lint", "add", "audit"]),
    (&["npx", "bunx", "pnpx"], &["jest", "vitest", "tsc", "eslint", "playwright", "prettier", "mocha", "cypress", "next", "vite", "webpack", "turbo", "nx"]),
    (&["go"], &["test", "build", "vet", "install", "generate", "mod"]),
    (&["swift"], &["build", "test"]),
    (&["dotnet"], &["build", "test", "restore", "publish"]),
    (&["mix"], &["test", "compile", "deps.get"]),
    (&["zig"], &["build", "test"]),
    (&["deno"], &["test", "lint", "check"]),
    (&["pip", "pip3", "uv", "poetry", "pipenv", "conda", "mamba"], &["install", "sync", "lock", "add", "update", "create"]),
    (&["python", "python3"], &["pytest", "unittest", "pip", "tox", "mypy", "ruff", "pylint", "flake8", "build", "compileall"]),
    (&["brew"], &["install", "upgrade", "update", "reinstall", "bundle"]),
    (&["apt", "apt-get", "dnf", "yum", "apk"], &["install", "update", "upgrade", "add"]),
    (&["docker", "podman"], &["build", "compose", "logs", "pull", "push", "buildx"]),
    (&["kubectl"], &["logs", "apply", "rollout"]),
    (&["terraform", "tofu"], &["plan", "apply", "init", "validate", "destroy"]),
    (&["helm", "bundle", "composer"], &["install", "upgrade", "lint", "update", "exec", "test"]),
    (&["stack", "cabal", "flutter", "dart", "lein", "rebar3"], &["build", "test", "install", "analyze", "pub", "compile"]),
];

/// Programs that cut an output down: after a `|`, the output is already what was asked for.
const FILTERS: [&str; 16] = ["head", "tail", "grep", "egrep", "rg", "sed", "awk", "cut", "sort", "uniq", "wc", "jq", "less", "more", "tr", "column"];

/// Words before the program itself: `sudo -E`, `time`, `env A=1`, `timeout 600`, `nice`.
const WRAPPERS: [&str; 7] = ["sudo", "time", "env", "nice", "nohup", "command", "exec"];

/// The command's pipelines, each a list of stages, each its words: quotes and backslashes are
/// read as the shell reads them; `;`, `&&`, `||`, `&` and new lines end a pipeline, `|` a stage.
pub(crate) fn pipelines(command: &str) -> Vec<Vec<Vec<String>>> {
    let (mut all, mut pipe, mut stage, mut word) = (Vec::new(), Vec::new(), Vec::new(), String::new());
    let mut quoted = false; // (a word started, even an empty one: '')
    let mut cs = command.chars().peekable();
    let end_word = |stage: &mut Vec<String>, word: &mut String, quoted: &mut bool| {
        if !word.is_empty() || *quoted {
            stage.push(std::mem::take(word));
        }
        *quoted = false;
    };
    while let Some(c) = cs.next() {
        match c {
            '\'' => {
                quoted = true;
                for d in cs.by_ref() {
                    if d == '\'' {
                        break;
                    }
                    word.push(d);
                }
            }
            '"' => {
                quoted = true;
                while let Some(d) = cs.next() {
                    match d {
                        '"' => break,
                        '\\' => word.extend(cs.next()),
                        _ => word.push(d),
                    }
                }
            }
            '\\' => word.extend(cs.next()),
            ' ' | '\t' => end_word(&mut stage, &mut word, &mut quoted),
            // `2>&1` and `&>`: a redirection, not the end of a pipeline
            '&' if word.ends_with('>') || cs.peek() == Some(&'>') => word.push(c),
            '|' | ';' | '&' | '\n' => {
                end_word(&mut stage, &mut word, &mut quoted);
                let twice = (c == '|' || c == '&') && cs.peek() == Some(&c);
                if twice {
                    cs.next();
                }
                pipe.push(std::mem::take(&mut stage));
                if c != '|' || twice {
                    all.push(std::mem::take(&mut pipe));
                }
            }
            _ => word.push(c),
        }
    }
    end_word(&mut stage, &mut word, &mut quoted);
    pipe.push(stage);
    all.push(pipe);
    for p in &mut all {
        p.retain(|s| !s.is_empty());
    }
    all.retain(|p| !p.is_empty());
    all
}

/// A stage's program (its file name) and the words after it, past `A=1` settings and wrappers.
pub(crate) fn program(stage: &[String]) -> Option<(&str, &[String])> {
    let mut i = 0;
    while i < stage.len() {
        let w = stage[i].as_str();
        let name = w.rsplit('/').next().unwrap_or(w);
        let setting = w.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
        if setting || w.starts_with('-') && i > 0 || WRAPPERS.contains(&name) {
            i += 1;
        } else if name == "timeout" {
            i += 2;
        } else {
            return Some((name, &stage[i + 1..]));
        }
    }
    None
}

/// Whether a stage runs a build, test, lint or install.
fn runs(stage: &[String]) -> bool {
    let Some((prog, rest)) = program(stage) else { return false };
    let prog = prog.strip_suffix(".exe").unwrap_or(prog);
    if RUNNERS.contains(&prog) {
        return true;
    }
    // the subcommand: the first word not a flag (`cargo +nightly test`, `npm --prefix x test`
    // reads x as it; good enough), or the module after python's -m
    let sub = if prog.starts_with("python") {
        rest.iter().position(|w| w == "-m").and_then(|i| rest.get(i + 1))
    } else {
        rest.iter().find(|w| !w.starts_with('-') && !w.starts_with('+'))
    };
    let prog = if prog.starts_with("python3.") { "python3" } else { prog };
    sub.is_some_and(|s| RUN_WITH.iter().any(|(ps, subs)| ps.contains(&prog) && subs.contains(&s.as_str())))
}

/// Whether most lines read as a log's: a time near the start (12:34:56) or a level word
/// (INFO, WARN, ERROR, DEBUG, ...) in the first 80 bytes; over the first 400 lines that are
/// not empty, at least 3 in 5.
fn log_like(text: &[u8]) -> bool {
    const LEVELS: [&[u8]; 10] = [b"INFO", b"WARN", b"WARNING", b"ERROR", b"DEBUG", b"TRACE", b"FATAL", b"CRITICAL", b"NOTICE", b"ERR"];
    let word_at = |l: &[u8], i: usize, w: &[u8]| {
        l[i..].starts_with(w) && (i == 0 || !l[i - 1].is_ascii_alphanumeric()) && l.get(i + w.len()).is_none_or(|c| !c.is_ascii_alphanumeric())
    };
    let (mut seen, mut like) = (0, 0);
    for l in text.split(|&c| c == b'\n').filter(|l| !l.trim_ascii().is_empty()).take(400) {
        seen += 1;
        let head = &l[..l.len().min(80)];
        let time = head.windows(8).take(32).any(|w| w[2] == b':' && w[5] == b':' && [0, 1, 3, 4, 6, 7].iter().all(|&i| w[i].is_ascii_digit()));
        let level = (0..head.len()).any(|i| LEVELS.iter().any(|w| word_at(head, i, w)));
        like += usize::from(time || level);
    }
    seen > 0 && like * 5 >= seen * 3
}

/// Whether a long output is one the short view serves: a build, test, lint or install run (the
/// view keeps its problems, warnings, summary lines and end, and every line stays one call
/// away), or a log. Left whole, as Claude Code shows it: a file, a diff, search results, any
/// output already cut down by a filter after `|`, fewer's own output, and any command with
/// FEWER_FULL=1 in it.
/// The whole output of a Bash call that Claude Code saved to a file of its own (a tool result
/// past its limit: `persistedOutputPath`, in a session's tool-results/ folder), when it can be
/// read as text and is not larger than fewer saves at once.
fn persisted(path: &str) -> Option<String> {
    const MAX: u64 = 1 << 30;
    let p = std::path::Path::new(path);
    if path.is_empty() || !p.is_absolute() || p.parent()?.file_name()? != "tool-results" {
        return None;
    }
    std::fs::metadata(p).ok().filter(|m| m.is_file() && m.len() <= MAX)?;
    let b = std::fs::read(p).ok()?;
    Some(String::from_utf8_lossy(&b).into_owned())
}

pub fn worth_shortening(command: &str, text: &[u8]) -> bool {
    worth(&pipelines(command), text)
}

/// worth_shortening, for a command's pipelines.
fn worth(all: &[Vec<Vec<String>>], text: &[u8]) -> bool {
    let words = || all.iter().flatten().flatten();
    if words().any(|w| w == "FEWER_FULL=1") {
        return false;
    }
    if all.iter().flatten().any(|s| program(s).is_some_and(|(p, _)| p == "fewer-pack" || p == "fewer")) {
        return false;
    }
    if all.iter().any(|p| p.iter().skip(1).any(|s| program(s).is_some_and(|(p, _)| FILTERS.contains(&p)))) {
        return false;
    }
    all.iter().flatten().any(|s| runs(s)) || log_like(text)
}

/// A command whose last pipelines are fewer reading files (`fewer read`, `fewer table`) after
/// a build or test run that is worth shortening (`python3 -m pytest 2>&1 || true; fewer read
/// a.py b.py`): its output split where fewer's first file header ("== FILE", a file the read
/// names) starts a line: (the run's part, fewer's part). None for any other command.
pub fn run_then_reads<'a>(command: &str, text: &'a str) -> Option<(&'a str, &'a str)> {
    let all = pipelines(command);
    let is_read = |p: &Vec<Vec<String>>| {
        p.len() == 1 && program(&p[0]).is_some_and(|(prog, rest)| prog == "fewer" && rest.first().is_some_and(|s| s == "read" || s == "table"))
    };
    let k = all.iter().rposition(|p| !is_read(p))? + 1;
    if k >= all.len() {
        return None;
    }
    let (_, rest) = program(&all[k][0])?;
    for f in rest.iter().skip(1).filter(|w| !w.starts_with('-')) {
        let head = format!("== {f}");
        let at = if text.starts_with(&head) { Some(0) } else { text.find(&format!("\n{head}")).map(|i| i + 1) };
        if let Some(i) = at {
            let (run, reads) = text.split_at(i);
            return worth(&all[..k], run.as_bytes()).then_some((run, reads));
        }
    }
    None
}

/// What a compaction is asked to keep when fewer has the whole session.
pub const SHORT_SUMMARY: &str = "Keep this summary short, about 1,500 tokens: the task and what was asked, the decisions made and why, what is done and what is next, and the exact names needed to go on (files, functions, commands, error messages, ids). Leave out file contents and long outputs: fewer keeps this whole session word for word, its card after the compaction gives back the commands run (failed or passed), the files edited and the last errors exactly, and fewer's history and search tools find anything else.";

/// PreCompact: the short summary's instructions, when fewer saves the sessions.
fn precompact(b: &[u8]) -> Option<String> {
    input(b)?;
    let home = crate::store::home().ok()?;
    let cfg = crate::config::read(&home);
    // (saved only when sessions are on for Claude Code and its folder is known: saved now, in the
    // background)
    let on = crate::sessions::agents_on(&cfg, &mut Vec::new()).contains(&"claude") && crate::sessions::Places::from_env().claude.is_ok();
    if on {
        let _ = crate::sessions::sync_detached();
    }
    on.then(|| SHORT_SUMMARY.to_string())
}

/// Where each session started, by its id (store/sessions-where/<id>): Claude Code reads a
/// project's settings in the folder a session starts in, and tells the commands it runs only the
/// session's id (CLAUDE_CODE_SESSION_ID), so `web`'s helper finds the session's own rules and hooks here.
pub fn note_session_folder(j: &Json) {
    let Some(id) = j.get("session_id").and_then(Json::str).filter(|i| !i.is_empty() && i.len() <= 100 && i.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) else { return };
    // (the project's folder Claude Code gives its hooks, else the session's folder)
    let Some(folder) = std::env::var("CLAUDE_PROJECT_DIR").ok().filter(|d| !d.is_empty()).or_else(|| j.get("cwd").and_then(Json::str).map(str::to_string)) else { return };
    let Ok(home) = crate::store::home() else { return };
    let dir = home.join("sessions-where");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(id), folder);
        // (sessions noted more than 30 days ago: gone)
        let old = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(SESSIONS_KEPT_DAYS * 86_400));
        if let (Ok(rd), Some(old)) = (std::fs::read_dir(&dir), old) {
            for e in rd.filter_map(Result::ok) {
                if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < old) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// How long a session's folder stays noted (days).
const SESSIONS_KEPT_DAYS: u64 = 30;

/// SessionStart: the start card as context for the new session.
fn start(b: &[u8]) -> Option<String> {
    let j = input(b)?;
    note_session_folder(&j);
    // (Claude Code's chats saved in the background when sessions are on: the last session's whole
    // chat, for the token card and this card)
    if let Ok(home) = crate::store::home()
        && crate::sessions::agents_on(&crate::config::read(&home), &mut Vec::new()).contains(&"claude")
    {
        let _ = crate::sessions::sync_detached();
    }
    let ctx = crate::tools::Ctx::new();
    let (card, err) = crate::tools::call(&ctx, "history", &Json::Obj(Vec::new()), &std::sync::atomic::AtomicBool::new(false)).ok()?;
    if err {
        return None;
    }
    let context = format!("fewer's start card (this project's memory; history, search, outline and the other fewer tools give more):\n{card}");
    Some(Json::o(vec![("hookSpecificOutput", Json::o(vec![("hookEventName", "SessionStart".into()), ("additionalContext", context.into())]))]).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_read_as_the_shell_reads_them() {
        let p = pipelines(r#"cd /w && A=1 cargo test 2>&1 | grep -E "test result|FAILED"; echo 'a b' x\ y"#);
        assert_eq!(p, [
            vec![vec!["cd", "/w"]],
            vec![vec!["A=1", "cargo", "test", "2>&1"], vec!["grep", "-E", "test result|FAILED"]],
            vec![vec!["echo", "a b", "x y"]],
        ]);
        assert_eq!(pipelines("make &> out.txt & wait || true"), [vec![vec!["make", "&>", "out.txt"]], vec![vec!["wait"]], vec![vec!["true"]]]);
        assert_eq!(program(&["sudo".into(), "-E".into(), "/usr/bin/make".into(), "-j8".into()]).map(|p| p.0), Some("make"));
        assert_eq!(program(&["timeout".into(), "600".into(), "env".into(), "X=1".into(), "go".into(), "test".into()]).map(|p| p.0), Some("go"));
    }

    #[test]
    fn a_run_then_fewers_reads_splits_where_the_reads_start() {
        let run = (1..=150).map(|i| format!("test_add_{i:03} (tests.test_calc.TestCalc) ... ok\n")).collect::<String>() + "FAILED (failures=1)\n";
        let reads = "== calc.py (7 lines)\n1 def add(a, b):\n== tests/test_calc.py (3 lines)\n1 import unittest\n";
        let text = format!("{run}{reads}");
        let c = "python3 -m pytest -q 2>&1 || python3 -m unittest -v 2>&1 || true; fewer read calc.py tests/test_calc.py";
        assert_eq!(run_then_reads(c, &text), Some((run.as_str(), reads)));
        // (two reads in a row; a read's options before its files)
        let c2 = "cargo test 2>&1 || true; fewer read --max 3000 calc.py; fewer table data.csv";
        assert_eq!(run_then_reads(c2, &text).map(|(a, _)| a.len()), Some(run.len()));
        // not split: no run before the reads, reads not last, a run already cut down, no header
        assert_eq!(run_then_reads("fewer read calc.py", &text), None);
        assert_eq!(run_then_reads("ls; fewer read calc.py", &text), None);
        assert_eq!(run_then_reads("fewer read calc.py; cargo test", &text), None);
        assert_eq!(run_then_reads("cargo test 2>&1 | tail -5; fewer read calc.py", &text), None);
        assert_eq!(run_then_reads("cargo test; fewer read other.py", &text), None);
    }

    #[test]
    fn runs_and_logs_are_shortened_files_and_searches_are_not() {
        let code = "fn main() {\n    println!(\"hi\");\n}\n".repeat(100);
        let log = (0..300).map(|i| format!("2026-11-23 12:00:{:02} INFO served request {i}\n", i % 60)).collect::<String>();
        let syslog = (0..300).map(|i| format!("Nov 23 12:00:{:02} host sshd[{i}]: Accepted key\n", i % 60)).collect::<String>();
        for c in [
            "cargo test",
            "cd /w && cargo test --release 2>&1",
            "cargo +nightly clippy --all-targets",
            "python3 -m pytest -q",
            "FEWER_HOME=/x python3 -I -m unittest -v",
            "sudo -E make -j8",
            "npx vitest run",
            "env FOO=1 go test ./...",
            "timeout 600 cargo test",
            "./gradlew build",
            "npm --silent test",
            "docker compose logs web",
        ] {
            assert!(worth_shortening(c, code.as_bytes()), "{c}");
        }
        for c in [
            "cat src/main.rs",
            "git diff",
            "sed -n 1,400p src/main.rs",
            "rg -n fn src",
            "cargo test 2>&1 | tail -30",
            "cargo test 2>&1 | grep -E \"test result|FAILED\"",
            "python3 -I checks/x.py out",
            "fewer run --short -- cargo test",
            "/opt/fewer/fewer sessions sync",
            "FEWER_FULL=1 cargo test",
            "ls -la",
        ] {
            assert!(!worth_shortening(c, code.as_bytes()), "{c}");
        }
        // a log is shortened whatever printed it, unless a filter cut it down already
        assert!(worth_shortening("cat /var/log/app.log", log.as_bytes()));
        assert!(worth_shortening("tail -n 500 /var/log/system.log", syslog.as_bytes()));
        assert!(!worth_shortening("cat /var/log/app.log | grep -v DEBUG", log.as_bytes()));
        assert!(!log_like(b"-rw-r--r--  1 me  staff  123 Nov 23 12:34 a.txt\n"));
        assert!(log_like(b"[ERROR] it broke\n[WARN] careful\nplain\n"));
    }
}
