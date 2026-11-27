//! fewer: the program beside fewer-pack that searches the developer's saved logs, answers
//! Claude's tools over MCP, and saves logs into the memory store.
//!
//!   fewer mcp                         the MCP server (JSON-RPC 2.0 on stdin/stdout)
//!   fewer install claude              register the MCP server with Claude Code (asks first)
//!   fewer add FILE...                 save files into the memory store
//!   fewer sessions sync               save agents' sessions now (when they are on)
//!   fewer memory [forget|pause|resume|limit|sessions|dna|secrets]
//!                                            see, forget, pause or limit what is saved
//!   fewer memory dna on|off|build     the DNA index: on (the default), off, or build now
//!   fewer keys --dump FILE            the stable message keys of a file, with counts
//!   fewer grep [-i] TEXT FILE.cmf...  fewer-pack's grep: the same output
//!   fewer index FILE.cmf...           fewer-pack's index: the same FILE.cmf.idx bytes
//!
//! It shares fewer-pack's archive, engine and search code: the files of ../src are used as they
//! are (through `#[path]`), never edited here, and their own unit tests run in this crate's
//! `cargo test` too. What those files keep private, or what lives in fewer-pack's main.rs, is
//! copied here, and a test checks each copy against the original.
#![allow(clippy::manual_is_multiple_of, clippy::chunks_exact_to_as_chunks, clippy::needless_range_loop)]

// The shared files are checked by clippy as fewer-pack checks them: for fewer-pack's rust-version,
// 1.85 (fewer's is 1.89, for File::lock). Without this, clippy suggests let chains
// and as_chunks (Rust 1.88) in the files shared with fewer.
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/batch.rs"]
mod batch;
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/container.rs"]
mod container;
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/crc.rs"]
mod crc;
// (its `mod fl;` and include!("core.rs") resolve in ../src/engine/)
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/engine/mod.rs"]
mod engine;
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/folder.rs"]
mod folder;
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/index.rs"]
mod index;
#[allow(dead_code)]
#[path = "../../src/portable.rs"]
mod portable;
#[allow(dead_code)]
#[clippy::msrv = "1.85"]
#[path = "../../src/search.rs"]
mod search;

mod add;
mod answer;
// A run's capture in raw/<pid>/: its writer, and the readers of a run in
// progress.
mod capture;
mod dev; // developer commands for checks/run_check.py
mod pack; // `fewer pack-pending`
#[cfg(unix)]
mod pty; // `run` on a pseudo-terminal
mod run; // `fewer run`
#[cfg(unix)]
mod sys_run; // the C calls `run` needs: pseudo-terminals, terminal modes, poll, signals
#[cfg(windows)]
#[path = "sys_run_win.rs"]
mod sys_run; // Windows: a background process away from the console; no terminal, no signals
mod tags; // a run's git tags
// The index pass, the catalogs (.cat) and the key registry.
mod catalog;
mod clean;
// The store's config file: read, and one key set with every other line kept.
mod config;
mod fetch;
mod find;
// The DNA index, in its shrunk form: the index itself
// (its tokens and pieces, its file, its build, the lookup find.rs makes), and when it is
// made (after saving, while idle, and now).
mod dna;
mod dna_build;
// The DNA index's FM core: SA-IS, BWT, wavelet matrix, count, LF steps.
// (locate, positions, with_rate and the parts' own constructors serve the tests and the
// stage 1 DNA test)
#[allow(dead_code)]
mod fm;
// Log types; each line's message, level and time fields.
// (Format::from_name serves the tests: add and pack-pending name a run's log type with
// formats::detect, directly and through catalog::index_run.)
#[allow(dead_code)]
mod formats;
mod grep;
// `install claude` and `uninstall claude`: fewer's MCP server in Claude Code.
mod install;
mod defaults; // where fewer finds things when a FEWER_ setting is not given
// Guard notes and the fix memory: moments, stops, moments.jsonl.
mod guard;
mod json;
mod mcp;
// `fewer memory`: see, forget, pause, limit, sessions, the DNA index
// and secrets; only the person changes the memory, from the command line.
mod memory;
// The stable message key, rule v1.
// (KeyCounts, count_lines and count_records serve the tests and the key reference check.)
#[allow(dead_code)]
mod msgkey;
// Multi-line records: stack traces, tracebacks, slow-log entries.
// (records::split serves the tests.)
#[allow(dead_code)]
mod records;
// Secret shapes and hiding: what is saved (`run`, `add`, pack-pending, sessions, the hook) and
// what is shown (`read`, `table`, `changes`, a short view's command line).
// (Counts::get and iter and Stream::pending serve the tests.)
#[allow(dead_code)]
mod secrets;
// Agents' sessions: `sessions sync` and the MCP server's sweep.
mod sessions;
mod store;
mod sys;
// The C calls the DNA index's builders need: getloadavg, background priority.
mod sys_idle;
// Times in answers and in `since`/`until`, and the log line time forms.
// (Arrivals is not called: the catalog walks a run's arrival marks itself; utc_text is for
// messages and tests.)
#[allow(dead_code)]
mod times;
mod tools;
mod view; // a readable view of agent sessions' lines (the token test of 2026-11-23)
mod short; // `run --short`: a short view of a command's output for an AI agent (2026-11-23)
mod outline; // a file's definitions or headings with their line ranges (2026-11-23)
mod hook; // Claude Code hooks: long Bash output shortened, the start card (2026-11-23)
mod ahead; // read-ahead: what a message names, read and run before Claude's first reply (2026-11-26)
mod savers; // the token savers in Claude Code's settings.json, added and removed (2026-11-23)
mod tokens; // how many tokens Claude Code spent, from the saved sessions (2026-11-23)
mod web; // `web "QUESTION"`: a page or a web question, the part that answers it (2026-11-25)
mod ondemand; // `skill`, `helper` and `workflow`: fewer's features on demand (2026-11-24)
mod changes; // `changes [REV]`: what a commit changed, in one call (2026-11-24)
mod read; // `read FILE...`: files whole, repeated blocks folded, nothing lost (2026-11-24)
mod table; // `table FILE...`: a table's columns, sums and sums by category in one call (2026-11-24)
mod look; // `look FILE... "QUESTION"`: pictures and documents answered in words (2026-11-25)
mod perms; // Claude Code's permission rules, kept by `read`, `table`, `look` and `run` (2026-11-26)
mod page; // a web page read by fewer itself for `web`: curl, then its text (2026-11-25)
mod route; // where to look for a question that names no page: free routes, no model (2026-11-25)

// Tests on sample files (formats, times, records, keys, secrets, the FM index): made-up
// samples in tests/data; copies of public logs through FEWER_SAMPLES.
#[cfg(test)]
mod sample_tests;

/// index.rs refers to crate::MISMATCH, which lives in fewer-pack's main.rs: the same text here.
const MISMATCH: &str = "made for another archive, or before this one changed";

const HELP: &str = "\
fewer - 86% fewer tokens in Claude Code, with the same answers as without it

For you:
  fewer install                   put fewer into Claude Code (shows every change first, asks once)
  fewer update                    get the new version, build it, bring Claude Code's side up to it
  fewer uninstall                 take fewer out of Claude Code and remove it (asks before
                                  deleting what it saved)
  fewer on | off                  turn fewer on or off (install turns it on)
  fewer tokens                    the tokens fewer saved
  fewer memory                    what fewer saved, its size, and delete what you want

What Claude runs itself with fewer on (any AI agent with a shell can use them too):
  fewer read FILE...              files whole, every line numbered; a block repeated 3 times or
                                  more shown once with its numbers (nothing lost)
  fewer run --short -- COMMAND    a command run, its output in short (a test run's failures, a
                                  build's errors), all of it saved
  fewer tool lines rN:A-B         lines A to B of a saved output (`fewer tool` alone lists
                                  fewer's other tools: search, count, ...)
  fewer table FILE...             a CSV, TSV or key=value log in one call: columns, exact sums,
                                  values, and every number's sums by each category
  fewer changes [REV]             what a commit changed (HEAD by default): each changed function
                                  whole, the names it uses
  fewer web \"QUESTION\"            a web page or a web question, the part that answers it
  fewer look FILE... \"QUESTION\"   pictures or PDFs answered in words
  fewer skill NAME [\"JOB\"]        a skill's instructions, found by its name
  fewer helper \"JOB\"              a helper session does JOB; its answer (lean with fewer on)
  fewer workflow \"JOB\"            a workflow for JOB whose agents are all lean

fewer --help all: every command, the settings, and what goes on the network.";

/// The rest: `fewer --help all`.
const HELP_ALL: &str = "\
More commands:
  fewer install [claude] [--dry-run] [--yes]
                                  install for Claude Code (claude, the only agent for now):
                                  --dry-run shows the changes and makes none; --yes answers the
                                  question (for scripts and agents)
  fewer uninstall [claude] [--dry-run] [--yes]
                                  take fewer out of Claude Code, then remove its programs and
                                  its PATH line (asked in a terminal: delete what it saved too)
  fewer update                    the new version: git pull where install.sh built fewer, built
                                  and installed again, Claude Code's side brought up to it
  fewer on [--dry-run] | off [--dry-run]
                                  fewer on or off (a copy of the settings is kept each time)
  fewer tokens [--here] [--split TIME]
                                  the tokens fewer saved (--here: this project only)
  fewer mcp                       fewer's MCP server (Claude Code starts it; stdin/stdout)
  fewer add [--hide-secrets] [--force] PATH...
                                  save files and folders into the memory store ($FEWER_HOME); a
                                  file added again saves what is new
  fewer add --macos-log | --simulator | --journal | FILE.logarchive
                                  save the system's log (the last hour; add --help)
  fewer sessions sync             save Claude Code's new chats now (when they are saved)
  fewer pack-pending [--wait]     pack the finished runs (run starts it itself)
  fewer memory [--total] | [--runs] [--project P]
                                  what is saved and its size; in a terminal, delete what you want
                                  (--total: the totals only; --project, --runs: the details)
  fewer memory clear [--yes]      delete everything saved, the settings kept (asks first)
  fewer memory forget REF... | --session ID | --project P | --before DATE
      | --sessions | --all [--yes]
                                  forget runs (it says what will go and asks first)
  fewer memory pause [FOLDER] | resume [FOLDER]
                                  stop or start saving command output (`run`)
  fewer memory limit [--size SIZE] [--months N] | --none
                                  forget the oldest runs past a limit after each save
  fewer memory sessions [on|off] [AGENT]
                                  save Claude Code's chats (and other agents'), or not
  fewer memory dna [on|off|build]
                                  the DNA index: on (the default) or off, or build the missing
                                  ones now
  fewer memory secrets            the secret shapes hidden before saving
  fewer keys --dump FILE          each message key of FILE with its number of records
  fewer keys --lines FILE         each line's message key
  fewer keys --cat FILE.cat       a saved run's catalog: its keys and records
  fewer catalog [--name NAME] FILE.cmf [OUT.cat]
                                  build an archive's catalog again (FILE.cat)
  fewer grep [-i] [-t N] TEXT FILE.cmf...
                                  print the lines that contain TEXT (as fewer-pack grep)
  fewer index FILE.cmf...         write FILE.cmf.idx (as fewer-pack index)
  fewer -V | --version            print the version
  fewer -h | --help [all]         print the help (all: this too)

Settings (environment; each has a default):
  FEWER_HOME        the memory store's folder (default ~/.fewer)
  FEWER_PACK_BIN    the fewer-pack program that packs (default: next to fewer, else PATH)
  FEWER_PROJECT     the project's root folder (default: the git root of the current folder)
  FEWER_CLAUDE_DIR  Claude Code's folder (default ~/.claude, or $CLAUDE_CONFIG_DIR)
  FEWER_CLAUDE_BIN  the claude command, for install, helper, workflow, look and web (default:
                    the claude on PATH)
  FEWER_CODEX_DIR, FEWER_AIDER_DIR, FEWER_CLINE_DIR, FEWER_CURSOR_DIR
                    other agents' folders, for saving their sessions (default: each agent's own
                    folder, when it is there)
  FEWER_DEBUG_LOG   a file for the MCP server's debug log
  FEWER_DEV         development: the store, the agents' folders and claude only from
                    their variables (no defaults; the project and fewer-pack keep theirs)

Network (fewer looks host names up itself; curl reads; Claude Code calls its own service):
  web                     a page: read through curl, its host first asked about with Anthropic's
                          domain check (api.anthropic.com) as WebFetch does, unless
                          skipWebFetchPreflight is set; a question naming no page: its words to
                          Wikipedia and DuckDuckGo, a package's name to PyPI, npm or crates.io,
                          a product's to endoflife.date, DevDocs' indexes downloaded (kept a
                          week); else Claude Code's own search
  helper, workflow, look  run Claude Code
  update                  git pull, in the folder fewer was built from
Nothing else goes on the network, and fewer never signs in anywhere. Your add-ons (MCP servers)
and claude.ai's connectors are Claude Code's own: with fewer on Claude uses them as usual.";

/// `fewer tool NAME [JSON | -]`: one of fewer's MCP tools (search, count, lines, ...)
/// called from a command, its answer as the MCP server gives it; exit 1 when it is an error.
fn cmd_tool(args: Vec<std::ffi::OsString>) -> i32 {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let (name, input) = match words.as_slice() {
        [n] if n == "-h" || n == "--help" => {
            println!("fewer tool NAME [JSON | -] - one of fewer's tools (search, count, lines, errors, outline, history, ...) called from a command\nThe inputs are a JSON object, as the MCP tool takes them (- reads them from stdin; none: {{}}). Without a name: the tools' names.");
            return 0;
        }
        [] => {
            // (without a name: the tools' names, as the help says, release check)
            let ctx = tools::Ctx::new();
            let r = tools::call(&ctx, "", &json::Json::Obj(Vec::new()), &std::sync::atomic::AtomicBool::new(false));
            let e = r.err().unwrap_or_default();
            println!("{}", e.split_once("; ").map_or(e.as_str(), |(_, rest)| rest));
            return 0;
        }
        [n] => (n.clone(), "{}".to_string()),
        [n, i] if i == "-" => {
            let mut s = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s) {
                eprintln!("fewer tool: stdin: {e}");
                return 1;
            }
            (n.clone(), s)
        }
        [n, i] if i.trim_start().starts_with('{') => (n.clone(), i.clone()),
        // (a plain form: key=value words, a line reference alone, `tool lines r2:1-40`: no quotes
        // or braces for Claude Code to ask about, release check)
        [n, rest @ ..] if !rest.is_empty() => {
            let mut obj: Vec<(String, json::Json)> = Vec::new();
            for w in rest {
                let (k, v) = match w.split_once('=') {
                    Some((k, v)) if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => (k.to_string(), v.to_string()),
                    _ => ("ref".to_string(), w.clone()),
                };
                let val = match json::parse(v.as_bytes()) {
                    Ok(j @ (json::Json::Num(_) | json::Json::Bool(_))) => j,
                    _ => json::Json::from(v.as_str()),
                };
                obj.push((k, val));
            }
            (n.clone(), json::Json::Obj(obj).to_string())
        }
        _ => {
            eprintln!("usage: fewer tool NAME ['{{\"input\": \"value\"}}']");
            return 2;
        }
    };
    let args = match json::parse(input.trim().as_bytes()) {
        Ok(a @ json::Json::Obj(_)) => a,
        _ => {
            eprintln!("fewer tool: the inputs must be a JSON object, for example '{{\"ref\": \"r2:1-40\"}}'");
            return 2;
        }
    };
    let ctx = tools::Ctx::new();
    match tools::call(&ctx, &name, &args, &std::sync::atomic::AtomicBool::new(false)) {
        Ok((text, is_error)) => {
            println!("{text}");
            i32::from(is_error)
        }
        Err(e) => {
            eprintln!("fewer tool: {e}");
            2
        }
    }
}

/// A command's entry in `--help all`: its line(s) and what it does.
fn entry_help(cmd: &str) -> String {
    let lines: Vec<&str> = HELP_ALL.lines().collect();
    let mut out: Vec<String> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with(&format!("  fewer {cmd} ")) || l.trim_end() == format!("  fewer {cmd}") {
            out.push(l.trim().to_string());
            out.extend(lines[i + 1..].iter().take_while(|x| x.starts_with("    ")).map(|x| format!("    {}", x.trim())));
        }
    }
    out.join("\n")
}

fn main() {
    // (output cut short, as `| head` does: a quiet end, as other command-line tools have, not a
    // panic's message; Rust's println! panics on a closed pipe)
    let usual = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.payload().downcast_ref::<String>().map(String::as_str).or_else(|| info.payload().downcast_ref::<&str>().copied()).unwrap_or("");
        if msg.starts_with("failed printing to std") && msg.contains("Broken pipe") {
            std::process::exit(0);
        }
        usual(info);
    }));
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let cmd = args.first().and_then(|a| a.to_str()).unwrap_or("").to_string();
    let rest = if args.is_empty() { Vec::new() } else { args[1..].to_vec() };
    if cmd == "run" {
        // before engine::clear_env_switches(): the command gets the environment as given
        std::process::exit(run::cmd_run(rest));
    }
    // before any other thread starts: the engine's developer switches never apply
    engine::clear_env_switches();
    // (`COMMAND --help` for a command with no help of its own: its entry in --help all)
    if matches!(cmd.as_str(), "catalog" | "grep" | "index" | "keys" | "card") && rest.len() == 1 && (rest[0] == "-h" || rest[0] == "--help") {
        match cmd.as_str() {
            "card" => println!("fewer card - the start card of a fresh session (the history tool with no input): what is failing now, what passes again, the last runs, what the last session did. The SessionStart hook adds it."),
            c => println!("{}", entry_help(c)),
        }
        std::process::exit(0);
    }
    let code = match cmd.as_str() {
        // `mcp` alone is fewer's MCP server (as Claude Code runs it); add-ons are not in this version
        "mcp" if rest.is_empty() => mcp::serve(),
        "mcp" if rest.len() == 1 && (rest[0] == "-h" || rest[0] == "--help") => {
            println!("usage: fewer mcp\n\nfewer's own MCP server, on stdin and stdout: its tools (search, count, lines, errors, outline,\nhistory, ...) only read what fewer saved. Claude Code starts it (`fewer install` registers it);\na person does not run it. `fewer tool` calls the same tools from a command.");
            0
        }
        "mcp" => {
            eprintln!("fewer mcp: fewer has no commands for add-ons: with fewer on Claude uses your add-ons (MCP servers) and claude.ai's connectors as usual, through Claude Code. `fewer mcp` alone is fewer's own MCP server.");
            2
        }
        "skill" => ondemand::cmd_skill(rest),
        "look" => look::cmd_look(rest),
        "web" => web::cmd_web(rest),
        "helper" => ondemand::cmd_helper(rest),
        "workflow" => ondemand::cmd_workflow(rest),
        // fewer on or off
        "on" | "off" => install::cmd_tenx(std::iter::once(std::ffi::OsString::from(cmd.as_str())).chain(rest).collect()),
        "sessions" => sessions::cmd_sessions(rest),
        // a developer command for checks/sessions_check.py: the server's sweep alone
        "sweep-serve" => dev::sweep_serve(rest),
        "install" => install::cmd_install(rest),
        "uninstall" => install::cmd_uninstall(rest),
        "update" => install::cmd_update(rest),
        "grep" => grep::cmd_grep(&grep::parse(rest, grep::Cmd::Grep)),
        "index" => grep::cmd_index(&grep::parse(rest, grep::Cmd::Index)),
        "add" => add::cmd_add(rest),
        "memory" => memory::cmd_memory(rest),
        // the DNA index's pass after saving: add and pack-pending start it, detached
        "dna-pass" => dna_build::cmd_dna_pass(rest),
        "pack-pending" => pack::cmd_pack_pending(rest),
        // developer commands for checks/run_check.py
        "hide" => dev::hide(rest),
        "scan-capture" => dev::scan_capture(rest),
        "capture-lines" => dev::capture_lines(rest),
        "rebuild-keys" => dev::rebuild_keys(rest),
        "short-sizes" => dev::short_sizes(rest),
        "hook" => hook::cmd_hook(rest),
        "tokens" => tokens::cmd_tokens(rest),
        // the start card of a fresh session (history with no input), for a SessionStart hook
        "card" => {
            let ctx = tools::Ctx::new();
            match tools::call(&ctx, "history", &json::Json::Obj(Vec::new()), &std::sync::atomic::AtomicBool::new(false)) {
                Ok((t, false)) => {
                    println!("{t}");
                    0
                }
                // (no memory for this folder yet: nothing to add to a fresh session)
                _ => 0,
            }
        }
        // fewer's own tools from a command: fewer's agents have Bash, not MCP tools
        "tool" => cmd_tool(rest),
        "changes" => changes::cmd_changes(rest),
        "read" => read::cmd_read(rest),
        "table" => table::cmd_table(rest),
        "keys" => catalog::cmd_keys(rest),
        "catalog" => catalog::cmd_catalog(rest),
        // a developer command for checks/json_check.py: each line of stdin parsed and
        // written back compact, or "ERROR <reason>"
        "json-check" => json::cmd_check(),
        "-V" | "--version" => {
            println!("fewer {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "-h" | "--help" | "help" => {
            if rest.first().is_some_and(|r| r == "all") {
                println!("{HELP}\n\n{HELP_ALL}");
            } else {
                println!("{HELP}");
            }
            0
        }
        "" => {
            eprintln!("{HELP}");
            2
        }
        _ => {
            eprintln!("fewer: unknown command {cmd}");
            eprintln!("Try 'fewer --help' for more information.");
            2
        }
    };
    std::process::exit(code)
}
