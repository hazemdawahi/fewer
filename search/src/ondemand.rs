//! Features on demand with fewer on: helpers, workflows and skills keep working, with any number of
//! agents, and load only when a job needs them. The lean agent has one tool, Bash; these commands
//! give it the rest, at no cost until one is used:
//!   fewer skill NAME          a skill's instructions (its SKILL.md), found by its name
//!   fewer helper "JOB"        a helper: a Claude Code session of its own does JOB and its
//!                                    answer is printed (lean with fewer on, whose `agent` setting
//!                                    covers every session); as many at once as wanted
//!   fewer workflow "JOB"      a workflow: a session of fewer's workflow agent (the lean
//!                                    rules and the Workflow tool, turned on for it alone) runs
//!                                    one, every agent of it of the lean type
//! A helper runs `claude -p JOB --output-format json` in this folder, with this environment and
//! Claude Code's own settings (its permissions too); while fewer is in development, claude only
//! from FEWER_CLAUDE_BIN. FEWER_HELPER_SETTINGS (a file or JSON, as claude's --settings takes
//! them) is given to every helper and workflow session: scripts and tests that give their main
//! session settings with --settings (permission rules, say) give their helpers the same ones. Its tokens (every model's, its workflow agents' included) go into
//! helpers.jsonl in the store, one line a run, when a store is set: Claude Code does not save a
//! session started from inside one of its own (read from its code, 2.1.287:
//! CLAUDE_CODE_CHILD_SESSION), so this is where a report finds them.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::json::{self, Json};

pub const SKILL_HELP: &str = "\
fewer skill NAME [\"THE JOB\"] [--all] - print a skill's instructions (its SKILL.md)

Looks for the skill named NAME (its SKILL.md's `name:`, else its folder's name) in Claude Code's
skills folder (FEWER_CLAUDE_DIR/skills), the project's .claude/skills folders (from this folder
up to the project's root) and the plugins' skills (FEWER_CLAUDE_DIR/plugins; PLUGIN:NAME names
one plugin's). Prints the file's path, then the file. Given the job, a long skill (over 6,000
bytes) is printed in part: its start (to the end of its first section; past 4,000 bytes cut, said)
and its sections that match the job's words, whole; --all prints all of it. Claude Code's own built-in skills are not
files: type /NAME in Claude Code to use one.";

/// A skill longer than this (bytes) is printed in part when the job is given.
const SKILL_ALL: usize = 6_000;
/// How much of a long skill's sections past its start are printed for a job (bytes).
const SKILL_PART: usize = 3_000;
/// The most of a long skill's start printed (bytes).
const SKILL_START: usize = 4_000;

/// A long skill's parts for a job (a procedure's steps seldom repeat the
/// job's words, so a section is shown whole or not at all): its start (the text before its first
/// `## ` section, and that first section, most often its core rules; each cut at a line past
/// SKILL_START, said) and the sections after it that hold the most of the job's words, whole (the
/// best one whatever its length, the others up to SKILL_PART), with the other sections that hold
/// some of its words named (their headings and sizes). A `## ` line inside a fenced block (a
/// template in the skill) is not a section. None when no section after the first holds a word of
/// the job, or when the part would be most of the skill (then all of it is printed).
fn skill_part(text: &str, job: &str) -> Option<(String, Vec<String>)> {
    // the "## " lines outside fenced blocks
    let mut heads: Vec<usize> = Vec::new();
    let mut fence: Option<&str> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        match fence {
            Some(f) if t.starts_with(f) => fence = None,
            Some(_) => {}
            None if t.starts_with("```") || t.starts_with("~~~") => fence = Some(&t[..3]),
            None if line.starts_with("## ") && at > 0 => heads.push(at),
            None => {}
        }
        at += line.len();
    }
    if heads.len() < 2 {
        return None;
    }
    let cut = |t: &str| -> String {
        let (c, was) = crate::clean::cut_at(t, SKILL_START);
        if was { format!("{}\n… (cut here: all of it with --all)\n", c.trim_end()) } else { t.to_string() }
    };
    let start = format!("{}{}", cut(&text[..heads[0]]), cut(&text[heads[0]..heads[1]]));
    // the sections after the first, by how many of the job's words each holds (a word also
    // matching another's start, both of 4 letters or more: "report" in "reports")
    let words = crate::clean::words_of(job);
    let secs: Vec<&str> = (1..heads.len()).map(|k| &text[heads[k]..heads.get(k + 1).copied().unwrap_or(text.len())]).collect();
    let score = |sec: &str| {
        let low = sec.to_lowercase();
        let toks: Vec<&str> = low.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
        words.iter().filter(|w| toks.iter().any(|t| t == w || (t.len().min(w.len()) >= 4 && (t.starts_with(w.as_str()) || w.starts_with(*t))))).count()
    };
    let mut ranked: Vec<(usize, usize)> = secs.iter().enumerate().map(|(i, sec)| (score(sec), i)).filter(|(sc, _)| *sc > 0).collect();
    if ranked.is_empty() {
        return None;
    }
    ranked.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    // the best whatever its length; the others about as good (60% of its words or more) up to
    // SKILL_PART after it; any other that holds some of the job's words named
    let floor = (ranked[0].0 * 3).div_ceil(5).max(1);
    let mut chosen: Vec<usize> = vec![ranked[0].1];
    let mut used = 0;
    let mut named: Vec<usize> = Vec::new();
    for &(sc, i) in &ranked[1..] {
        if sc >= floor && used + secs[i].len() <= SKILL_PART {
            used += secs[i].len();
            chosen.push(i);
        } else {
            named.push(i);
        }
    }
    chosen.sort();
    // (the others named: the SKILL_NAMED with the most of the job's words, in their order, then
    // how many more: 200 sections named in one line cost as much as
    // the whole skill)
    // (and at most SKILL_NAMES_CHARS of names: eight long headings could still be many times the
    // part)
    let all = named.len();
    named.truncate(SKILL_NAMED);
    let name_of = |i: usize| format!("{} ({} bytes)", secs[i].lines().next().unwrap_or("").trim().chars().take(120).collect::<String>(), secs[i].len());
    let mut size = 0;
    named.retain(|&i| {
        size += name_of(i).len() + 2;
        size <= SKILL_NAMES_CHARS
    });
    let more = all - named.len();
    named.sort();
    let part = format!("{}…\n{}", start.trim_end().to_string() + "\n", chosen.iter().map(|&i| secs[i].trim_end()).collect::<Vec<_>>().join("\n…\n") + "\n");
    let mut others: Vec<String> = named.iter().map(|&i| name_of(i)).collect();
    if more > 0 {
        others.push(format!("and {more} more"));
    }
    // (most of it anyway, the names counted: all of it, which says the most)
    let listed: usize = others.iter().map(|o| o.len() + 2).sum();
    ((part.len() + listed) * 4 < text.len() * 3).then_some((part, others))
}

/// The most other sections a skill's part names, and the most characters of their names.
const SKILL_NAMED: usize = 8;
const SKILL_NAMES_CHARS: usize = 1_200;

pub const HELPER_HELP: &str = "\
fewer helper \"JOB\" [\"JOB\" ...] - helpers: a Claude Code session of its own for each job

Runs `claude -p JOB --output-format json` in this folder for each job, all at once, and prints
their answers in order (under a heading each when there are several). Each argument is a job
when every one holds a space; otherwise all the words are one job. With fewer on each helper is
fewer's worker (--agent fewer-worker: the lean rules for one job). A helper starts without your
CLAUDE.md files (each step stays small): the job is all it gets, so write any rule it must keep
into the job. Their tokens are added to helpers.jsonl in
the store (FEWER_HOME), for the tokens report. FEWER_HELPER_SETTINGS (a file or JSON) is given
to each as --settings. claude is FEWER_CLAUDE_BIN, else the claude on PATH.";

pub const WORKFLOW_HELP: &str = "\
fewer workflow \"JOB\" - a workflow, every agent of it lean

Runs a Claude Code session of fewer's workflow agent (fewer-lean-workflow: the lean rules and
the Workflow tool, turned on and allowed for this session alone: your yes to this command is
the yes to the workflow, and its agents' own steps still go through your permission rules)
that writes and runs one workflow for JOB, giving every workflow agent fewer's lean type, and
prints its answer. The workflow's session and its agents start without your CLAUDE.md files (each
step stays small): the job is all they get. Its tokens, its workflow agents' included, are added
to helpers.jsonl in the store (FEWER_HOME). claude is
FEWER_CLAUDE_BIN, else the claude on PATH.";

// ---------------------------------------------------------------------------- skills

/// A skill's name in its SKILL.md (`name:`), else its folder's name.
fn skill_name(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let front = text.strip_prefix("---\n").and_then(|r| r.split("\n---").next()).unwrap_or("");
    let named = front.lines().find_map(|l| l.strip_prefix("name:").map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())).filter(|n| !n.is_empty());
    named.or_else(|| file.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Every SKILL.md under `dir`, at most `depth` folders down, in name order (hidden folders and
/// links passed over).
fn skill_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(t) = e.file_type() else { continue };
        let p = e.path();
        if t.is_dir() && depth > 0 && !e.file_name().to_string_lossy().starts_with('.') {
            skill_files(&p, depth - 1, out);
        } else if t.is_file() && e.file_name() == "SKILL.md" {
            out.push(p);
        }
    }
}

/// The skill's file: the user's own first, then the project's, then the plugins' (`plugin:name`
/// names one plugin's: a folder of its path above `skills` is named so).
pub fn find_skill(name: &str, claude_dir: Option<&Path>, project_folders: &[PathBuf]) -> Option<PathBuf> {
    let (plugin, bare) = match name.split_once(':') {
        Some((p, n)) => (Some(p), n),
        None => (None, name),
    };
    let matches = |f: &Path| skill_name(f).is_some_and(|n| n == bare || n == name);
    if plugin.is_none() {
        let mut places = Vec::new();
        places.extend(claude_dir.map(|d| d.join("skills")));
        places.extend(project_folders.iter().map(|f| f.join(".claude").join("skills")));
        for place in places {
            let mut files = Vec::new();
            skill_files(&place, 4, &mut files);
            if let Some(f) = files.into_iter().find(|f| matches(f)) {
                return Some(f);
            }
        }
    }
    let mut files = Vec::new();
    skill_files(&claude_dir?.join("plugins"), 8, &mut files);
    files.into_iter().find(|f| {
        // .../<plugin>[/<version>]/skills/<skill>/SKILL.md
        let parts: Vec<String> = f.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let Some(k) = parts.iter().rposition(|p| p == "skills") else { return false };
        let in_plugin = plugin.is_none_or(|p| parts[..k].iter().rev().take(3).any(|x| x == p));
        in_plugin && matches(f)
    })
}

pub fn cmd_skill(args: Vec<OsString>) -> i32 {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if matches!(words.as_slice(), [h] if h == "-h" || h == "--help") {
        println!("{SKILL_HELP}");
        return 0;
    }
    let all = words.iter().any(|w| w == "--all");
    let rest: Vec<&String> = words.iter().filter(|w| *w != "--all").collect();
    let (name, job) = match rest.as_slice() {
        [n] if !n.starts_with('-') => ((*n).clone(), None),
        [n, j] if !n.starts_with('-') => ((*n).clone(), Some((*j).clone())),
        _ => {
            eprintln!("usage: fewer skill NAME [\"THE JOB\"] [--all]");
            return 2;
        }
    };
    let claude_dir = crate::defaults::setting("FEWER_CLAUDE_DIR").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = crate::store::project_root(&cwd);
    let mut folders = vec![cwd.clone()];
    if cwd.starts_with(&root) {
        folders.extend(cwd.ancestors().skip(1).take_while(|a| a.starts_with(&root)).map(Path::to_path_buf));
    }
    match find_skill(&name, claude_dir.as_deref(), &folders) {
        Some(f) => match std::fs::read_to_string(&f) {
            Ok(text) => {
                println!("skill {name}: {} (its other files are in that folder)", f.display());
                if let Some((part, others)) = job.filter(|_| !all && text.len() > SKILL_ALL).and_then(|j| skill_part(&text, &j)) {
                    let also = if others.is_empty() { String::new() } else { format!("; other sections with some of the job's words, not shown: {}", others.join(", ")) };
                    println!("(its start and the parts for this job: {} of {} bytes{also}; all of it: {} skill {name} --all)", part.len(), text.len(), crate::savers::me());
                    print!("{part}");
                    return 0;
                }
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
                0
            }
            Err(e) => {
                eprintln!("fewer skill: {}: {e}", f.display());
                1
            }
        },
        None => {
            let where_ = if claude_dir.is_some() { "" } else { " (FEWER_CLAUDE_DIR is not set, so only the project's skills were looked in)" };
            eprintln!("fewer skill: no skill named {name}{where_}. Claude Code's own built-in skills are not files: type /{name} in Claude Code to use one.");
            1
        }
    }
}

// ---------------------------------------------------------------------------- helpers

/// Every token a run's result counts: (all input read and written, output), over every model it
/// used (its workflow agents' and side calls' included: `modelUsage`).
pub fn tokens_of(r: &Json) -> (u64, u64) {
    let (mut input, mut output) = (0u64, 0u64);
    if let Some(Json::Obj(models)) = r.get("modelUsage") {
        for (_, m) in models {
            let n = |k: &str| m.get(k).and_then(Json::int).unwrap_or(0).max(0) as u64;
            input += n("inputTokens") + n("cacheReadInputTokens") + n("cacheCreationInputTokens");
            output += n("outputTokens");
        }
    }
    (input, output)
}

/// A run's input tokens by kind, over every model it used: (input, cache writes, cache reads).
fn input_split(r: &Json) -> (u64, u64, u64) {
    let (mut i, mut w, mut c) = (0u64, 0u64, 0u64);
    if let Some(Json::Obj(models)) = r.get("modelUsage") {
        for (_, m) in models {
            let n = |k: &str| m.get(k).and_then(Json::int).unwrap_or(0).max(0) as u64;
            i += n("inputTokens");
            w += n("cacheCreationInputTokens");
            c += n("cacheReadInputTokens");
        }
    }
    (i, w, c)
}

/// The id of the Claude Code session whose command started this run (CLAUDE_CODE_SESSION_ID,
/// which Claude Code gives its commands); "" outside one.
pub fn parent_session() -> String {
    std::env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|i| !i.is_empty() && i.len() <= 100 && i.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).unwrap_or_default()
}

/// The line helpers.jsonl gets for one run (the token card counts it: tokens.rs). The input's
/// split by kind, the run's own session and the session that started it (`parent`) came last
/// (2026-11-26): older lines have `tokens` alone. (A run started outside Claude Code is saved by
/// it as a session of its own, which the card counts: it leaves out the line of a session it
/// counted; a run started from a session counts with that session.)
pub fn usage_line(kind: &str, r: &Json, ok: bool, parent: &str) -> String {
    let (input, output) = tokens_of(r);
    let (fresh, written, read) = input_split(r);
    let num = |k: &str| r.get(k).filter(|v| matches!(v, Json::Num(_))).cloned().unwrap_or(Json::from(0i64));
    let (y, mo, d, h, mi, s) = crate::times::civil(crate::times::now_ms().div_euclid(1000));
    Json::o(vec![
        ("time", format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}").into()),
        ("kind", kind.into()),
        ("ok", Json::Bool(ok)),
        ("turns", num("num_turns")),
        ("tokens", Json::from(input)),
        ("output", Json::from(output)),
        ("cost_usd", num("total_cost_usd")),
        ("cwd", std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default().into()),
        ("input", Json::from(fresh)),
        ("cache_write", Json::from(written)),
        ("cache_read", Json::from(read)),
        ("session", r.get("session_id").and_then(Json::str).unwrap_or("").into()),
        ("parent", parent.into()),
    ])
    .to_string()
}

pub(crate) fn log_usage(line: &str) {
    let Ok(home) = crate::store::home() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("helpers.jsonl")) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// claude: FEWER_CLAUDE_BIN, else `claude` on PATH (in development only the variable).
pub(crate) fn claude() -> Result<PathBuf, String> {
    match crate::defaults::setting("FEWER_CLAUDE_BIN") {
        Some(v) => crate::install::find_claude(&v, std::env::var_os("PATH").as_deref()),
        None => Err("FEWER_CLAUDE_BIN is not set: in development (FEWER_DEV), fewer runs Claude Code's claude only from it".into()),
    }
}

/// The settings every helper and workflow session gets (FEWER_HELPER_SETTINGS: a file, or
/// JSON), as a JSON object, merged with `more` (whose keys win, env merged key by key).
pub fn helper_settings(given: Option<&str>, more: Option<&Json>) -> Result<Option<Json>, String> {
    let mut base = match given.map(str::trim).filter(|v| !v.is_empty()) {
        None => None,
        Some(v) => {
            let text = if v.starts_with('{') { v.as_bytes().to_vec() } else { std::fs::read(v).map_err(|e| format!("FEWER_HELPER_SETTINGS {v}: {e}"))? };
            let j = json::parse(&text).map_err(|e| format!("FEWER_HELPER_SETTINGS is not JSON ({e})"))?;
            if !matches!(j, Json::Obj(_)) {
                return Err("FEWER_HELPER_SETTINGS is not a JSON object".into());
            }
            Some(j)
        }
    };
    if let Some(Json::Obj(m)) = more {
        let b = base.get_or_insert_with(|| Json::Obj(Vec::new()));
        for (k, v) in m {
            match (b.get(k).cloned(), v) {
                (Some(Json::Obj(mut old)), Json::Obj(new)) if k == "env" => {
                    for (nk, nv) in new {
                        old.retain(|(ok, _)| ok != nk);
                        old.push((nk.clone(), nv.clone()));
                    }
                    b.set(k, Json::Obj(old));
                }
                _ => b.set(k, v.clone()),
            }
        }
    }
    Ok(base)
}

/// One helper's end: whether it did the job, what it answered, and why it failed (for stderr).
struct Done {
    ok: bool,
    answer: String,
    why: Option<String>,
}

/// Run one helper session to its end (its tokens logged).
/// What a helper's or a workflow's own session leaves out (read from Claude Code 2.1.287 and
/// measured, 2026-11-24): its title (a side call of ~900 tokens; these sessions are not saved
/// anyway), the commit lines' note (moved into the Bash tool's text), the token countdown, and the
/// CLAUDE.md files (~450 tokens a step: the main session writes the job, fewer's worker rules carry
/// what it needs). Background tasks stay on: a long command still goes on when it passes its time.
pub const SESSION_ENV: [(&str, &str); 4] = [
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
    ("CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT", "0"),
    ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off"),
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
];
/// A helper also leaves out the notes Claude Code adds to each step for an interactive session
/// (IDE context, add-ons' instructions, reminders); a workflow's session keeps them, as its
/// agents' results come to it.
pub const HELPER_ENV: [(&str, &str); 1] = [("CLAUDE_CODE_DISABLE_ATTACHMENTS", "1")];

fn run_one(kind: &str, claude: &Path, job: &str, extra: &[&str], settings: Option<&Json>) -> Done {
    let mut cmd = Command::new(claude);
    cmd.arg("-p").arg(job).args(["--output-format", "json"]).args(extra);
    cmd.envs(SESSION_ENV);
    if kind == "helper" {
        cmd.envs(HELPER_ENV);
    }
    if let Some(s) = settings {
        cmd.arg("--settings").arg(s.to_string());
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Done { ok: false, answer: String::new(), why: Some(format!("cannot run {}: {e}", claude.display())) },
    };
    let mut err_pipe = child.stderr.take();
    let errs = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(e) = err_pipe.as_mut() {
            let _ = e.read_to_end(&mut b);
        }
        b
    });
    let mut out = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut out);
    }
    let status = child.wait();
    let errs = errs.join().unwrap_or_default();
    let ok_exit = status.as_ref().is_ok_and(|s| s.success());
    // the result: the whole output, else its last line that is a JSON object
    let text = String::from_utf8_lossy(&out);
    let r = json::parse(text.trim().as_bytes()).ok().or_else(|| text.lines().rev().find_map(|l| json::parse(l.trim().as_bytes()).ok().filter(|v| v.get("type").is_some())));
    let Some(r) = r else {
        let said: String = String::from_utf8_lossy(&errs).lines().chain(text.lines()).filter(|l| !l.trim().is_empty()).take(8).collect::<Vec<_>>().join(" | ");
        let how = if ok_exit { "no result" } else { "it failed" };
        return Done { ok: false, answer: String::new(), why: Some(format!("the {kind} gave no answer ({how}){}", if said.is_empty() { String::new() } else { format!(": {said}") })) };
    };
    let failed = r.get("is_error").and_then(Json::bool) == Some(true) || !ok_exit;
    log_usage(&usage_line(kind, &r, !failed, &parent_session()));
    let answer = r.get("result").and_then(Json::str).unwrap_or("").to_string();
    let why = failed.then(|| format!("the {kind} ended with an error ({})", r.get("subtype").and_then(Json::str).unwrap_or("error")));
    Done { ok: !failed, answer, why }
}

/// Run the jobs, one session each, all at once; print their answers in order (each under a
/// heading when there are several); the exit status (0 when every one did its job).
fn run(kind: &str, jobs: &[String], extra: &[&str], more: Option<&Json>) -> i32 {
    let claude = match claude() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fewer {kind}: {e}");
            return 1;
        }
    };
    let settings = match helper_settings(std::env::var("FEWER_HELPER_SETTINGS").ok().as_deref(), more) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fewer {kind}: {e}");
            return 1;
        }
    };
    let done: Vec<Done> = std::thread::scope(|sc| {
        let handles: Vec<_> = jobs.iter().map(|job| sc.spawn(|| run_one(kind, &claude, job, extra, settings.as_ref()))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(Done { ok: false, answer: String::new(), why: Some("it stopped".into()) })).collect()
    });
    let several = jobs.len() > 1;
    for (i, d) in done.iter().enumerate() {
        if several {
            println!("== {kind} {} of {}{} ==", i + 1, jobs.len(), if d.ok { "" } else { " (failed)" });
        }
        if !d.answer.is_empty() || !several {
            println!("{}", d.answer);
        }
        if let Some(w) = &d.why {
            eprintln!("fewer {kind}{}: {w}", if several { format!(" {}", i + 1) } else { String::new() });
        }
    }
    if done.iter().all(|d| d.ok) { 0 } else { 1 }
}

/// The jobs given: each argument is a job when every one holds a space (`helper "job 1" "job
/// 2"`), else all the words are one job (`helper review the code`).
pub fn jobs_of(words: &[String]) -> Vec<String> {
    if words.len() > 1 && words.iter().all(|w| w.trim().contains(char::is_whitespace)) {
        words.iter().map(|w| w.trim().to_string()).collect()
    } else {
        vec![words.join(" ")]
    }
}

fn job_args(kind: &str, args: &[OsString], help: &str) -> Result<Vec<String>, i32> {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    match words.as_slice() {
        [h] if h == "-h" || h == "--help" => {
            println!("{help}");
            Err(0)
        }
        [] => {
            eprintln!("usage: fewer {kind} \"<the job>\"{}", if kind == "helper" { " [\"<another job>\" ...]" } else { "" });
            Err(2)
        }
        w => Ok(w.to_vec()),
    }
}

/// Whether an agent's file of this name is there for a session started here: in Claude Code's
/// folder (FEWER_CLAUDE_DIR) or a project's .claude/agents, from this folder up to its root
/// (claude stops with an error at an --agent it does not find).
fn agent_found(name: &str) -> bool {
    let file = format!("{name}.md");
    if crate::defaults::setting("FEWER_CLAUDE_DIR").is_some_and(|d| Path::new(&d).join("agents").join(&file).is_file()) {
        return true;
    }
    // both resolved (git names the root by its real path: /private/tmp for /tmp on macOS)
    let real = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let cwd = real(std::env::current_dir().unwrap_or_default());
    let root = real(crate::store::project_root(&cwd));
    cwd.ancestors().take_while(|a| a.starts_with(&root) || *a == cwd.as_path()).any(|a| a.join(".claude").join("agents").join(&file).is_file())
}

pub fn cmd_helper(args: Vec<OsString>) -> i32 {
    match job_args("helper", &args, HELPER_HELP) {
        // a helper is fewer's worker when fewer is on (the lean rules for one job, no add-ons:
        // their instructions would be re-read at every step of every helper); else the session's
        // own agent, with what it has
        Ok(words) => {
            let worker = agent_found(crate::savers::WORKER_AGENT);
            let extra: &[&str] = if worker { &["--agent", crate::savers::WORKER_AGENT, "--strict-mcp-config"] } else { &[] };
            run("helper", &jobs_of(&words), extra, None)
        }
        Err(code) => code,
    }
}

pub fn cmd_workflow(args: Vec<OsString>) -> i32 {
    match job_args("workflow", &args, WORKFLOW_HELP) {
        // the Workflow tool, turned on for this session alone (fewer turns it off in the
        // settings: its description would be re-read at every step of every session)
        Ok(words) => {
            if !agent_found(crate::savers::WORKFLOW_AGENT) {
                eprintln!("fewer workflow: a workflow needs fewer's workflow agent ({}.md), which is not there: fewer on", crate::savers::WORKFLOW_AGENT);
                return 1;
            }
            let on = json::parse(br#"{"env":{"CLAUDE_CODE_DISABLE_WORKFLOWS":"0"}}"#).unwrap_or(Json::Null);
            // the Workflow tool allowed in it: a session started with -p cannot show the script
            // for review, and the yes to `fewer workflow` is the yes to the workflow (its
            // agents' own steps still go through the developer's permission rules)
            // (no add-ons in a workflow's session either: its agents are fewer's workers)
            run("workflow", &[words.join(" ")], &["--agent", crate::savers::WORKFLOW_AGENT, "--allowedTools", "Workflow", "--strict-mcp-config"], Some(&on))
        }
        Err(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_skill_in_part() {
        let mut t = String::from("---\nname: reports\ndescription: house style\n---\n\n# Reports\n\nEvery report is Markdown under out/.\n\n## General rules\n\n- The first line is a level-2 heading.\n- One blank line between blocks.\n");
        for (title, word) in [("Invoices", "invoice"), ("Log summaries", "log"), ("Incident reports", "incident"), ("Release notes", "release"), ("Sales by region", "region"), ("Customer lists", "customer"), ("Weekly status", "status")] {
            t += &format!("\n## {title}\n\n");
            for i in 0..12 {
                t += &format!("- Rule {i} of the {word} report: write the {word} lines in the order given, one per line, and so on.\n");
            }
        }
        assert!(t.len() > SKILL_ALL, "{}", t.len());
        let (p, _) = skill_part(&t, "write the sales by region report for data/sales.csv").unwrap();
        assert!(p.starts_with("---\nname: reports\n") && p.contains("## General rules\n\n- The first line is a level-2 heading.\n- One blank line between blocks.\n…\n## Sales by region"), "{p}");
        assert!(p.contains("- Rule 11 of the region report") && !p.contains("## Invoices") && !p.contains("## Customer lists"), "{p}");
        // nothing past the start matches: all of it is printed
        assert!(skill_part(&t, "zebra").is_none());
        // a procedure whose steps do not repeat the job's words: its sections whole
        let mut r = String::from("---\nname: ship\n---\n\n# Ship\n\n## Rules\n\nAlways ask first.\n");
        for title in ["Release", "Rollback", "Hotfix", "Audit", "Archive", "Backup"] {
            r += &format!("\n## {title}\n\n");
            for i in 0..20 {
                r += &format!("{i}. Step {i}: run the tool with the flags given here, then check it.\n");
            }
        }
        let (p, _) = skill_part(&r, "how do I do a release and a rollback").unwrap();
        assert!(p.contains("## Rules\n\nAlways ask first.") && p.contains("19. Step 19") && p.contains("## Release") && p.contains("## Rollback") && !p.contains("## Hotfix"), "{p}");
        assert_eq!(p.matches("19. Step 19").count(), 2, "{p}");
        // a first section past 4,000 bytes: cut at a line, said, never left out
        let mut big = String::from("# Big\n\n## Instructions\n\n");
        big += &"Follow this rule — always, in every report you write.\n".repeat(120);
        big += "\n## Zebra appendix\n\nThe zebra lines go last.\n";
        big += &"\n## Filler\n\nNothing here at all for anyone.\n".repeat(40);
        let (p, _) = skill_part(&big, "the zebra appendix").unwrap();
        assert!(p.contains("## Instructions") && p.contains("(cut here: all of it with --all)") && p.contains("The zebra lines go last."), "{}", &p[..300]);
        // (no slice inside a character: an em dash at byte 4,000)
        let mut dash = "x".repeat(3_999) + "—";
        dash += &"\n## A\n\nalpha\n".repeat(400);
        let _ = skill_part(&format!("## First\n{dash}"), "alpha");
        // a "## " inside a fenced template is not a section; a best section of 3,000 bytes
        // or more leaves room for the others, and the matching ones left out are named
        let mut f = String::from("# Reports\n\n## Rules\n\nKeep it short.\n\n## Weekly status\n\nUse this template for the weekly status:\n\n```markdown\n## Status\n\nWhat moved this week.\n\n## Risks\n\nWhat could slip.\n```\n\nEnd every weekly status with a date.\n");
        f += &"\n## Filler\n\nNothing at all for anyone here, just text to make it long.\n".repeat(60);
        f += &format!("\n## Long procedure\n\n{}", "Step: run the weekly tool, then check the weekly output.\n".repeat(70));
        f += "\n## Charts\n\nA weekly chart is a table, never an image.\n";
        let (p, others) = skill_part(&f, "write the weekly status").unwrap();
        assert!(p.contains("## Weekly status\n\nUse this template") && p.contains("## Risks\n\nWhat could slip.\n```\n\nEnd every weekly status with a date."), "{p}");
        assert!(!others.is_empty() || p.contains("## Long procedure"), "{others:?}");
        let (_, others) = skill_part(&f, "the weekly chart procedure").unwrap();
        assert!(others.iter().all(|o| o.starts_with("## ")), "{others:?}");
        // 200 sections with the job's words: 8 named and how many more, the names counted in what
        // is shown
        let mut big = String::from("# Reports\n\n## Start\n\nRead this first.\n");
        for k in 0..200 {
            big += &format!("\n## A section about the weekly report, part number {k}\n\nSee the wiki.\n");
        }
        let (p, others) = skill_part(&big, "write the weekly report").unwrap();
        assert!(others.len() == SKILL_NAMED + 1 && others.last().is_some_and(|o| o.starts_with("and ") && o.ends_with(" more")), "{others:?}");
        assert!((p.len() + others.iter().map(|o| o.len() + 2).sum::<usize>()) * 4 < big.len() * 3);
    }

    #[test]
    fn skills_found_by_name() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-skills", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let put = |rel: &str, text: &str| {
            let p = d.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put("claude/skills/gstack/verify/SKILL.md", "---\nname: verify\ndescription: checks\n---\nrun the checks\n");
        put("claude/skills/plain/SKILL.md", "no front matter\n");
        put("claude/plugins/cache/mkt/anthropic-skills/1.0/skills/docx/SKILL.md", "---\nname: docx\n---\nword files\n");
        put("claude/plugins/cache/mkt/other/2.0/skills/docx/SKILL.md", "---\nname: docx\n---\nother docx\n");
        put("repo/.claude/skills/deploy/SKILL.md", "---\nname: deploy\n---\nship it\n");
        put("repo/.claude/skills/verify/SKILL.md", "---\nname: verify\n---\nthe project's verify\n");
        let claude = d.join("claude");
        let folders = [d.join("repo").join("sub"), d.join("repo")];
        let found = |n: &str| find_skill(n, Some(&claude), &folders).map(|p| p.strip_prefix(&d).unwrap().display().to_string().replace('\\', "/"));
        // the user's own first
        assert_eq!(found("verify").as_deref(), Some("claude/skills/gstack/verify/SKILL.md"));
        assert_eq!(found("plain").as_deref(), Some("claude/skills/plain/SKILL.md"));
        assert_eq!(found("deploy").as_deref(), Some("repo/.claude/skills/deploy/SKILL.md"));
        assert_eq!(found("anthropic-skills:docx").as_deref(), Some("claude/plugins/cache/mkt/anthropic-skills/1.0/skills/docx/SKILL.md"));
        assert_eq!(found("other:docx").as_deref(), Some("claude/plugins/cache/mkt/other/2.0/skills/docx/SKILL.md"));
        assert_eq!(found("nope:docx"), None);
        assert_eq!(found("claude-api"), None);
        // without Claude Code's folder: only the project's
        assert_eq!(find_skill("verify", None, &folders).map(|p| p.ends_with("repo/.claude/skills/verify/SKILL.md")), Some(true));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn helpers_get_the_settings_they_are_given() {
        let on = json::parse(br#"{"env":{"CLAUDE_CODE_DISABLE_WORKFLOWS":"0"}}"#).unwrap();
        assert_eq!(helper_settings(None, None), Ok(None));
        assert_eq!(helper_settings(Some("  "), Some(&on)).unwrap().unwrap().to_string(), r#"{"env":{"CLAUDE_CODE_DISABLE_WORKFLOWS":"0"}}"#);
        let given = r#"{"permissions":{"allow":["Bash(python3:*)"]},"env":{"A":"1","CLAUDE_CODE_DISABLE_WORKFLOWS":"1"}}"#;
        assert_eq!(helper_settings(Some(given), Some(&on)).unwrap().unwrap().to_string(), r#"{"permissions":{"allow":["Bash(python3:*)"]},"env":{"A":"1","CLAUDE_CODE_DISABLE_WORKFLOWS":"0"}}"#);
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-hs.json", std::process::id()));
        std::fs::create_dir_all(d.parent().unwrap()).unwrap();
        std::fs::write(&d, given).unwrap();
        assert_eq!(helper_settings(d.to_str(), None).unwrap().unwrap().to_string(), json::parse(given.as_bytes()).unwrap().to_string());
        std::fs::remove_file(&d).unwrap();
        assert!(helper_settings(Some("/no/such/file"), None).unwrap_err().contains("FEWER_HELPER_SETTINGS /no/such/file"));
        assert!(helper_settings(Some("[1]"), None).unwrap_err().contains("not JSON") || helper_settings(Some("{]"), None).is_err());
    }

    #[test]
    fn jobs_split_only_when_each_is_a_sentence() {
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(jobs_of(&w(&["review n01-n10", "review n11-n20"])), ["review n01-n10", "review n11-n20"]);
        assert_eq!(jobs_of(&w(&["review", "the", "code"])), ["review the code"]);
        assert_eq!(jobs_of(&w(&["review the code"])), ["review the code"]);
        assert_eq!(jobs_of(&w(&["review all", "now"])), ["review all now"]);
    }

    #[test]
    fn a_runs_tokens() {
        let r = json::parse(br#"{"type":"result","num_turns":3,"total_cost_usd":0.05,"session_id":"s-1","modelUsage":{"a":{"inputTokens":4,"outputTokens":100,"cacheReadInputTokens":5000,"cacheCreationInputTokens":2000},"b":{"inputTokens":900,"outputTokens":10}}}"#).unwrap();
        assert_eq!(tokens_of(&r), (7904, 110));
        let line = usage_line("helper", &r, true, "p-1");
        assert!(line.contains(r#""kind":"helper","ok":true,"turns":3,"tokens":7904,"output":110,"cost_usd":0.05"#), "{line}");
        // the input by kind, over every model, at the end of the line (904 + 2,000 + 5,000), and the
        // run's session
        assert!(line.ends_with(r#","input":904,"cache_write":2000,"cache_read":5000,"session":"s-1","parent":"p-1"}"#), "{line}");
    }
}
