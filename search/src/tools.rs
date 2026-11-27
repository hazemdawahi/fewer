//! Claude's tools: their names, input schemas and handlers: logs, search, count (by run), lines,
//! follow and outline here; odd, errors, compare and quiet in keytools.rs; guard and fixes in
//! guardtools.rs; history in histtools.rs. Every tool only reads the store: nothing here writes
//! a file.
//!
//! The scope of a call (4.1): the server's project (or `project`), its runs of `kind`
//! (logs by default: command runs, added files and exports), narrowed by `runs`, `since`,
//! `until`, `command`, `branch` and `commit`. Search lists runs newest first; follow oldest
//! first. A cursor is a reference: go on in that run from that line, then the runs after it
//! in the same order.
//!
//! `since` and `until` pick the runs whose lines' times (from their catalogs: runs.jsonl's
//! `first` and `last`) meet the window; a run without line times by the time it was saved
//! (its start and end).
//!
//! The catalogs (catalog.rs) give `search` its lines collapsed by key and its counts
//! per key and per hour, `count` its tables by key, hour and level, and `logs` its time
//! ranges. A run without a catalog that fits gets one made from its bytes for the answer
//! (never written: no tool writes), and the answer says so.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::answer::{Answer, ShortRefs, group, line_ref, next_call, run_ref, shown_line};
use crate::catalog::{self, Catalog, Mark};
use crate::fetch;
use crate::find::{self, Hit, Job, Keep, LevelCtx, Query, Range, RunHits, RunOpts};
use crate::json::Json;
use crate::store::{self, Project, RunRec};
use crate::times;

// odd, errors, compare and quiet: a module of their own, inside this one so that it
// shares the scope, the arguments and the answers' helpers here
#[path = "keytools.rs"]
mod keytools;
// guard and fixes: the Guard notes and the fix memory, beside keytools
#[path = "guardtools.rs"]
mod guardtools;
// history (2026-11-23): the start card and what happened before, beside them
#[path = "histtools.rs"]
mod histtools;

// ---------------------------------------------------------------- definitions and schemas

#[derive(Clone, Copy)]
enum T {
    /// a string of min..=max characters
    Str(usize, usize),
    Bool,
    Int(i64, i64),
    Enum(&'static [&'static str]),
    /// an array of strings, at most n
    Strs(usize),
    /// an array of strings from a list, at most n
    Enums(&'static [&'static str], usize),
}

struct P {
    name: &'static str,
    t: T,
    desc: &'static str,
}

pub struct ToolDef {
    pub name: &'static str,
    title: &'static str,
    desc: &'static str,
    props: Vec<P>,
    required: &'static [&'static str],
}

const KINDS: &[&str] = &["logs", "runs", "sessions", "all"];

fn scope_props() -> Vec<P> {
    vec![
        // (described once, in the server's instructions: these come with most tools)
        P { name: "project", t: T::Str(1, 4096), desc: "" },
        P { name: "runs", t: T::Strs(100), desc: "" },
        P { name: "since", t: T::Str(1, 64), desc: "" },
        P { name: "until", t: T::Str(1, 64), desc: "" },
        P { name: "kind", t: T::Enum(KINDS), desc: "" },
        P { name: "command", t: T::Str(1, 1000), desc: "" },
        P { name: "branch", t: T::Str(1, 256), desc: "" },
        P { name: "commit", t: T::Str(1, 64), desc: "" },
    ]
}

fn cap_props() -> Vec<P> {
    vec![
        P { name: "max_tokens", t: T::Int(200, 20_000), desc: "" },
        P { name: "time_limit_ms", t: T::Int(100, 120_000), desc: "" },
    ]
}

fn with(mut own: Vec<P>, scope: bool) -> Vec<P> {
    if scope {
        own.extend(scope_props());
    } else {
        own.push(P { name: "project", t: T::Str(1, 4096), desc: "project of a ref without one" });
    }
    own.extend(cap_props());
    own
}

/// The inputs without some of them (compare picks its runs with a and b, not with runs).
fn without(mut props: Vec<P>, names: &[&str]) -> Vec<P> {
    props.retain(|p| !names.contains(&p.name));
    props
}

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "logs",
            title: "Saved logs",
            desc: "This project's saved logs, newest first: command runs and files (agents' sessions too with kind all), with lines, sizes and time ranges.",
            props: with(
                vec![
                    P { name: "limit", t: T::Int(1, 500), desc: "most runs listed; default 30" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "go on from this run (a reference from a previous answer)" },
                ],
                true,
            ),
            required: &[],
        },
        ToolDef {
            name: "search",
            title: "Search saved logs",
            desc: "Exact lines holding a text in saved logs, outputs and files; same-key lines collapsed (×N); exact counts; every line has a reference for `lines`. Use it first when debugging.",
            props: with(
                vec![
                    P { name: "text", t: T::Str(1, 1000), desc: "the text a line must hold (exact bytes, not a pattern)" },
                    P { name: "icase", t: T::Bool, desc: "ignore upper and lower case (ASCII)" },
                    P { name: "whole", t: T::Bool, desc: "only where the text is not part of a longer word or ID" },
                    P { name: "limit", t: T::Int(1, 500), desc: "most lines shown; default 10" },
                    P { name: "context", t: T::Int(0, 20), desc: "lines shown before and after each hit; default 0" },
                    P { name: "before", t: T::Int(0, 20), desc: "lines shown before each hit (in place of context's)" },
                    P { name: "after", t: T::Int(0, 20), desc: "lines shown after each hit (in place of context's)" },
                    P { name: "summary", t: T::Bool, desc: "counts per key and per hour too" },
                    P { name: "compact", t: T::Bool, desc: "default true" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "from a previous answer's last line" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                    P { name: "collapse", t: T::Bool, desc: "default true" },
                ],
                true,
            ),
            required: &["text"],
        },
        ToolDef {
            name: "count",
            title: "Count lines",
            desc: "Exact counts of the lines holding a text (or of all records), per run, key, hour, level, IP, field or path. Size a problem before reading lines.",
            props: with(
                vec![
                    P { name: "text", t: T::Str(1, 1000), desc: "omit to count all lines" },
                    P { name: "icase", t: T::Bool, desc: "ignore upper and lower case (ASCII)" },
                    P { name: "whole", t: T::Bool, desc: "only where the text is not part of a longer word or ID" },
                    P { name: "by", t: T::Enum(&["run", "key", "hour", "level", "ip", "field", "path"]), desc: "default run; ip, field, path: values in the matching lines" },
                    P { name: "field", t: T::Str(1, 200), desc: "csv column (name or number), JSON field (repo.name), name= value, or word number" },
                    P { name: "limit", t: T::Int(1, 500), desc: "rows, default 10 (24 by hour)" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "from a previous answer's last line" },
                ],
                true,
            ),
            required: &[],
        },
        ToolDef {
            name: "lines",
            title: "Exact lines",
            desc: "Exact lines at a reference (r42:1201, r42:1201-1210) with the lines around; expect checks a quoted line byte for byte.",
            props: with(
                vec![
                    P { name: "ref", t: T::Str(1, 300), desc: "r42:1201 or r42:1201-1210" },
                    P { name: "before", t: T::Int(0, 200), desc: "lines shown before; default 5" },
                    P { name: "after", t: T::Int(0, 200), desc: "lines shown after; default 5" },
                    P { name: "expect", t: T::Str(0, 1_000_000), desc: "the lines expected, joined by \\n" },
                    P { name: "raw", t: T::Bool, desc: "exact bytes, escapes shown" },
                    P { name: "base64", t: T::Bool, desc: "also give the referenced lines' exact bytes in base64" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 2000" },
                ],
                false,
            ),
            required: &["ref"],
        },
        ToolDef {
            name: "outline",
            title: "Outline a file",
            desc: "A file's definitions or headings, each with the lines it spans (and doc comments), a table's columns or JSON lines' fields: read only the range you need. A project file (path) or a saved run (ref).",
            props: with(
                vec![
                    P { name: "path", t: T::Str(1, 4096), desc: "a file in this project's folder" },
                    P { name: "ref", t: T::Str(1, 300), desc: "a saved run (r42) instead of a file" },
                    P { name: "depth", t: T::Int(0, 8), desc: "default 1 (with methods)" },
                    P { name: "line", t: T::Int(1, 1 << 40), desc: "the items around this line" },
                    P { name: "name", t: T::Str(1, 300), desc: "the items whose line holds this (def get_docstring)" },
                    P { name: "docs", t: T::Bool, desc: "doc comments too (on with line or name)" },
                    P { name: "values", t: T::Bool, desc: "each column's or field's values too" },
                    P { name: "limit", t: T::Int(1, 1000), desc: "most items shown; default 100" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "go on from item #N (given in a previous answer's last line)" },
                ],
                false,
            ),
            required: &[],
        },
        ToolDef {
            name: "history",
            title: "What happened before",
            desc: "With no input, the start card for a fresh session: what is failing now and since when, what passes again, the last runs, and what the last agent session ran (what failed) and edited. command: its runs; session (last or an id): what it did; file: the sessions that edited it and what ran after.",
            props: vec![
                P { name: "command", t: T::Str(1, 1000), desc: "the runs of commands holding this" },
                P { name: "session", t: T::Str(1, 200), desc: "last, or a session id (its start)" },
                P { name: "file", t: T::Str(1, 4096), desc: "the sessions that edited it" },
                P { name: "project", t: T::Str(1, 4096), desc: "" },
                P { name: "max_tokens", t: T::Int(200, 20_000), desc: "" },
            ],
            required: &[],
        },
        ToolDef {
            name: "follow",
            title: "Follow an ID",
            desc: "Every line holding an ID (request, trace, job) across saved logs, oldest first, with counts per run.",
            props: with(
                vec![
                    P { name: "id", t: T::Str(4, 200), desc: "the ID, exactly as logged" },
                    P { name: "limit", t: T::Int(1, 500), desc: "most lines shown; default 50" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "from a previous answer's last line" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                ],
                true,
            ),
            required: &["id"],
        },
        ToolDef {
            name: "odd",
            title: "Odd lines first",
            desc: "New and rare message keys first, with exact counts here and in the project, one line each: what is unusual.",
            props: with(
                vec![
                    P { name: "limit", t: T::Int(1, 500), desc: "most keys shown; default 20" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "go on from key #N (given in a previous answer's last line)" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                ],
                true,
            ),
            required: &[],
        },
        ToolDef {
            name: "errors",
            title: "Error groups",
            desc: "Error groups (keys at level error, stack traces): exact counts, first and last time, runs, the first record. Use it first when something failed.",
            props: with(
                vec![
                    P { name: "limit", t: T::Int(1, 500), desc: "most groups shown; default 5" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "go on from group #N (given in a previous answer's last line)" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                ],
                true,
            ),
            required: &[],
        },
        ToolDef {
            name: "compare",
            title: "Compare two runs",
            desc: "What changed between two runs (passing, then failing): keys new, gone and changed in b, with counts and a line each. Give a and b, or let it pick.",
            props: without(
                with(
                    vec![
                        P { name: "a", t: T::Str(1, 300), desc: "the run before (default: b's previous of the same command)" },
                        P { name: "b", t: T::Str(1, 300), desc: "the run after (default: the last)" },
                        P { name: "failing", t: T::Bool, desc: "b: the last failing run, a: the passing one before" },
                        P { name: "limit", t: T::Int(1, 500), desc: "most rows shown; default 30" },
                        P { name: "cursor", t: T::Str(1, 300), desc: "go on from row #N (given in a previous answer's last line)" },
                        P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                    ],
                    true,
                ),
                &["runs"],
            ),
            required: &[],
        },
        ToolDef {
            name: "quiet",
            title: "Gone quiet",
            desc: "Keys and sources whose counts fell to 0 or by 90% or more: against the run before (default) or the window before.",
            props: with(
                vec![
                    P { name: "against", t: T::Enum(&["previous", "window"]), desc: "previous (default) or window (since to until)" },
                    P { name: "limit", t: T::Int(1, 500), desc: "most rows shown; default 30" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "go on from row #N (given in a previous answer's last line)" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                ],
                true,
            ),
            required: &[],
        },
        ToolDef {
            name: "guard",
            title: "Guard notes",
            desc: "The latest moments, newest first: new and rare keys, jumps, error bursts, failed runs, errors back. Finds the minute to look at.",
            props: with(
                vec![
                    P { name: "kinds", t: T::Enums(&crate::guard::Kind::NAMES, 6), desc: "default all" },
                    P { name: "limit", t: T::Int(1, 500), desc: "most moments shown; default 20" },
                    P { name: "cursor", t: T::Str(1, 300), desc: "from a previous answer's last line" },
                    P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                ],
                true,
            ),
            required: &[],
        },
        ToolDef {
            name: "fixes",
            title: "Has this happened before?",
            desc: "Has this error happened before, and what came right before it stopped: runs without it, commits and files changed, agents' sessions and edits. It never names a cause.",
            props: without(
                with(
                    vec![
                        P { name: "text", t: T::Str(1, 1000), desc: "words of the error message, or a pasted line" },
                        P { name: "ref", t: T::Str(1, 300), desc: "a ref to one of its lines" },
                        P { name: "icase", t: T::Bool, desc: "ignore upper and lower case (ASCII) in text" },
                        P { name: "limit", t: T::Int(1, 500), desc: "rows, default 20" },
                        P { name: "cursor", t: T::Str(1, 300), desc: "go on from row #N (given in a previous answer's last line)" },
                        P { name: "line_max", t: T::Int(80, 65_536), desc: "longer lines are cut at this many bytes; default 400" },
                    ],
                    true,
                ),
                &["runs", "since", "until", "kind", "command", "branch", "commit"],
            ),
            required: &[],
        },
    ]
}

fn schema(d: &ToolDef) -> Json {
    let mut props = Vec::new();
    for p in &d.props {
        let mut o = match p.t {
            // (types and choices only: every bound is checked by the server, which says what is
            // wrong; bounds in every schema cost every reply's tokens, the measurement of
            // 2026-11-23)
            T::Str(..) => Json::o(vec![("type", "string".into())]),
            T::Bool => Json::o(vec![("type", "boolean".into())]),
            T::Int(..) => Json::o(vec![("type", "integer".into())]),
            T::Enum(v) => Json::o(vec![("type", "string".into()), ("enum", Json::Arr(v.iter().map(|&s| s.into()).collect()))]),
            T::Strs(_) => Json::o(vec![("type", "array".into()), ("items", Json::o(vec![("type", "string".into())]))]),
            T::Enums(v, _) => Json::o(vec![("type", "array".into()), ("items", Json::o(vec![("type", "string".into()), ("enum", Json::Arr(v.iter().map(|&s| s.into()).collect()))]))]),
        };
        if !p.desc.is_empty() {
            o.set("description", p.desc.into());
        }
        props.push((p.name.to_string(), o));
    }
    let mut s = Json::o(vec![("type", "object".into()), ("additionalProperties", false.into())]);
    if !d.required.is_empty() {
        s.set("required", Json::Arr(d.required.iter().map(|&r| r.into()).collect()));
    }
    s.set("properties", Json::Obj(props));
    s
}

/// The tools/list result.
pub fn list() -> Json {
    let tools = defs()
        .iter()
        .map(|d| {
            Json::o(vec![
                ("name", d.name.into()),
                ("title", d.title.into()),
                ("description", d.desc.into()),
                ("inputSchema", schema(d)),
                ("annotations", Json::o(vec![("readOnlyHint", true.into())])),
            ])
        })
        .collect();
    Json::o(vec![("tools", Json::Arr(tools))])
}

/// Check a call's arguments against the tool's schema: the reason when they break it.
fn check(d: &ToolDef, args: &Json) -> Result<(), String> {
    let fields = match args {
        Json::Obj(f) => f,
        _ => return Err("arguments must be an object".into()),
    };
    let names = || d.props.iter().map(|p| p.name).collect::<Vec<_>>().join(", ");
    for (k, v) in fields {
        let Some(p) = d.props.iter().find(|p| p.name == k) else {
            return Err(format!("unknown field '{k}'; the fields are {}", names()));
        };
        let ok = match (p.t, v) {
            (T::Str(min, max), Json::Str(s)) => {
                let n = s.chars().count();
                if n < min || n > max {
                    return Err(format!("field '{k}' must be a string of {min} to {max} characters"));
                }
                true
            }
            (T::Bool, Json::Bool(_)) => true,
            (T::Int(min, max), Json::Num(_)) => match v.int() {
                Some(i) if i >= min && i <= max => true,
                _ => return Err(format!("field '{k}' must be a whole number from {min} to {max}")),
            },
            (T::Enum(vals), Json::Str(s)) => {
                if !vals.contains(&s.as_str()) {
                    return Err(format!("field '{k}' must be one of {}", vals.join(", ")));
                }
                true
            }
            (T::Strs(n), Json::Arr(a)) => {
                if a.len() > n || a.iter().any(|x| x.str().is_none()) {
                    return Err(format!("field '{k}' must be an array of at most {n} strings"));
                }
                true
            }
            (T::Enums(vals, n), Json::Arr(a)) => {
                if a.len() > n || a.iter().any(|x| x.str().is_none_or(|s| !vals.contains(&s))) {
                    return Err(format!("field '{k}' must be an array of at most {n} of {}", vals.join(", ")));
                }
                true
            }
            _ => false,
        };
        if !ok {
            let want = match p.t {
                T::Str(..) | T::Enum(_) => "a string",
                T::Bool => "true or false",
                T::Int(..) => "a whole number",
                T::Strs(_) | T::Enums(..) => "an array of strings",
            };
            return Err(format!("field '{k}' must be {want}"));
        }
    }
    for r in d.required {
        if args.get(r).is_none() {
            return Err(format!("field '{r}' is required"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- calls

/// The server's context: the store and the project of the folder it runs in.
pub struct Ctx {
    pub home: Result<PathBuf, String>,
    pub root: PathBuf,
}

impl Ctx {
    pub fn new() -> Ctx {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Ctx { home: store::home(), root: store::project_root(&cwd) }
    }
}

/// A tool call: Ok((text, is_error)), or Err(reason) for a call that breaks the tool's
/// schema or names no tool (JSON-RPC -32602).
pub fn call(ctx: &Ctx, name: &str, args: &Json, stop: &AtomicBool) -> Result<(String, bool), String> {
    let defs = defs();
    let Some(d) = defs.iter().find(|d| d.name == name) else {
        return Err(format!("unknown tool '{name}'; the tools are {}", defs.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")));
    };
    let empty = Json::Obj(Vec::new());
    let args = if args.is_null() { &empty } else { args };
    check(d, args)?;
    let a = Args(args);
    // (a call about this folder's project, the default: its references without the project,
    // "r42:1201", which `lines` and the cursors read in this project; made so, not cut out
    // after, so that the caps and the "…(+N bytes)" of a cut line count what is shown: the
    // token test of 2026-11-23)
    let _short = ShortRefs::new(match (a.s("project"), &ctx.home) {
        (None, Ok(home)) => Some(store::pid_for(home, &ctx.root).0),
        _ => None,
    });
    let r = match name {
        "logs" => tool_logs(ctx, &a),
        "search" => tool_search(ctx, &a, stop, false),
        "follow" => tool_search(ctx, &a, stop, true),
        "count" => tool_count(ctx, &a, stop),
        "lines" => tool_lines(ctx, &a),
        "outline" => tool_outline(ctx, &a),
        "history" => histtools::tool_history(ctx, &a, stop),
        "odd" => keytools::tool_odd(ctx, &a, stop),
        "errors" => keytools::tool_errors(ctx, &a, stop),
        "compare" => keytools::tool_compare(ctx, &a, stop),
        "quiet" => keytools::tool_quiet(ctx, &a, stop),
        "guard" => guardtools::tool_guard(ctx, &a, stop),
        "fixes" => guardtools::tool_fixes(ctx, &a, stop),
        _ => Err("no such tool".into()),
    };
    Ok(match r {
        Ok(t) => (t, false),
        Err(e) => (e, true),
    })
}

/// A call's checked arguments.
struct Args<'a>(&'a Json);

impl Args<'_> {
    fn s(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Json::str)
    }
    fn b(&self, k: &str) -> bool {
        self.0.get(k).and_then(Json::bool).unwrap_or(false)
    }
    fn i(&self, k: &str, default: i64) -> i64 {
        self.0.get(k).and_then(Json::int).unwrap_or(default)
    }
    fn list(&self, k: &str) -> Vec<String> {
        self.0.get(k).and_then(Json::arr).map(|a| a.iter().filter_map(|x| x.str().map(str::to_string)).collect()).unwrap_or_default()
    }
    /// The same arguments with another cursor (the next call of a "left out" line).
    fn with_cursor(&self, cursor: &str) -> Json {
        let mut v = self.0.clone();
        v.remove("cursor");
        v.set("cursor", cursor.into());
        v
    }
}

// ---------------------------------------------------------------- the scope

/// The runs a call covers.
struct Scope {
    projects: Vec<Project>,
    /// (project index, run), in scope order: newest first
    runs: Vec<(usize, RunRec)>,
    /// what the answer's heading says about the scope ("fewer-3fa2, logs")
    desc: String,
}

fn kind_ok(kind: &str, run_kind: &str) -> bool {
    match kind {
        "runs" => run_kind == "run",
        "sessions" => run_kind == "session",
        "all" => true,
        _ => matches!(run_kind, "run" | "add" | "export" | "kept" | "not-read"),
    }
}

/// A run's command as text (its argv joined by spaces), else the file it saved.
fn what(r: &RunRec) -> String {
    let s = match r.0.get("cmd").and_then(Json::arr) {
        Some(c) => crate::secrets::shown_command(&c.iter().filter_map(Json::str).collect::<Vec<_>>().join(" ")),
        None => match r.s("cmd") {
            Some(c) => crate::secrets::shown_command(c),
            None => r.s("source").unwrap_or("").to_string(),
        },
    };
    // (an argument may hold a line end or other control bytes: shown as \xHH, as answers show
    // lines, so a heading stays one line)
    if s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) { crate::clean::escape(s.as_bytes(), false).0 } else { s }
}

fn no_logs_yet(ctx: &Ctx, pid: &str) -> String {
    format!(
        "no saved logs yet for this project ({pid}, {}). Save some with `fewer add FILE` (later `fewer add` and `fewer run`), or pass project \"*\" to look in every project.",
        ctx.root.display()
    )
}

/// The projects a call names.
fn projects(ctx: &Ctx, home: &Path, project: Option<&str>) -> Result<Vec<Project>, String> {
    let all = store::list_projects(home);
    // (at most 20 names: a store may hold hundreds, and each answer is read by Claude)
    let names = || match all.len() {
        0 => "none yet".to_string(),
        n if n <= 20 => all.iter().map(|p| p.pid.as_str()).collect::<Vec<_>>().join(", "),
        n => format!("{} and {} more (project \"*\" looks in all of them)", all.iter().take(20).map(|p| p.pid.as_str()).collect::<Vec<_>>().join(", "), n - 20),
    };
    match project {
        None => {
            let (pid, exists) = store::pid_for(home, &ctx.root);
            match all.into_iter().find(|p| p.pid == pid).filter(|_| exists) {
                Some(p) => Ok(vec![p]),
                None => Err(no_logs_yet(ctx, &pid)),
            }
        }
        Some("*") => {
            if all.is_empty() {
                return Err("no saved logs yet in any project. Save some with `fewer add FILE` (later `fewer add` and `fewer run`).".into());
            }
            Ok(all)
        }
        Some(p) => {
            if let Some(x) = all.iter().find(|x| x.pid == p) {
                return Ok(vec![x.clone()]);
            }
            if p.starts_with('/') {
                let root = PathBuf::from(p.trim_end_matches('/'));
                let root = if root.as_os_str().is_empty() { PathBuf::from("/") } else { root };
                let (pid, exists) = store::pid_for(home, &root);
                if let Some(x) = all.iter().find(|x| x.pid == pid).filter(|_| exists) {
                    return Ok(vec![x.clone()]);
                }
            }
            Err(format!("no project '{p}' in the memory store; its projects: {}", names()))
        }
    }
}

/// A run reference: (project id or None, run id).
fn parse_run_ref(s: &str) -> Option<(Option<&str>, u64)> {
    let (pid, r) = match s.rsplit_once('/') {
        Some((p, r)) => (Some(p), r),
        None => (None, s),
    };
    let id = r.strip_prefix('r')?.parse::<u64>().ok().filter(|&i| i > 0)?;
    Some((pid.filter(|p| !p.is_empty()), id))
}

/// A line reference: (project id or None, run id, first line, last line).
fn parse_line_ref(s: &str) -> Option<(Option<&str>, u64, u64, u64)> {
    let (run, lines) = s.trim().rsplit_once(':')?;
    let (pid, id) = parse_run_ref(run)?;
    let (a, b) = match lines.split_once('-') {
        Some((a, b)) => (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?),
        None => {
            let a = lines.trim().parse::<u64>().ok()?;
            (a, a)
        }
    };
    (a >= 1 && b >= a).then_some((pid, id, a, b))
}

fn select(ctx: &Ctx, a: &Args, newest_first: bool) -> Result<Scope, String> {
    let home = ctx.home.clone()?;
    let projects = projects(ctx, &home, a.s("project"))?;
    let now = times::now_ms();
    let since = a.s("since").map(|s| times::parse_when(s, now)).transpose()?;
    let until = a.s("until").map(|s| times::parse_when(s, now)).transpose()?;
    let refs = a.list("runs");
    // runs named by reference are taken whatever their kind, unless a kind is given
    let kind = a.s("kind").unwrap_or(if refs.is_empty() { "logs" } else { "all" });
    let mut runs: Vec<(usize, RunRec)> = Vec::new();
    for (pi, p) in projects.iter().enumerate() {
        for r in store::read_runs(&p.dir) {
            if !kind_ok(kind, r.kind()) {
                continue;
            }
            // the run's lines' times (its catalog's), else when it was saved
            let (start, end) = match (r.i("first"), r.i("last")) {
                (Some(a), Some(b)) => (a, b),
                _ => (r.i("start").unwrap_or(0), r.i("end").unwrap_or(i64::MAX)),
            };
            if since.is_some_and(|s| end < s) || until.is_some_and(|u| start > u) {
                continue;
            }
            if let Some(c) = a.s("command")
                && !what(&r).contains(c)
            {
                continue;
            }
            if let Some(b) = a.s("branch")
                && !r.s("branch").is_some_and(|x| x.starts_with(b))
            {
                continue;
            }
            if let Some(c) = a.s("commit")
                && !r.s("commit").is_some_and(|x| x.starts_with(c))
            {
                continue;
            }
            runs.push((pi, r));
        }
    }
    // newest first: by id in one project; by start time across projects
    if projects.len() == 1 {
        runs.sort_by_key(|x| std::cmp::Reverse(x.1.id()));
    } else {
        runs.sort_by(|x, y| {
            let (sx, sy) = (x.1.i("start").unwrap_or(0), y.1.i("start").unwrap_or(0));
            sy.cmp(&sx).then(projects[x.0].pid.cmp(&projects[y.0].pid)).then(y.1.id().cmp(&x.1.id()))
        });
    }
    if !refs.is_empty() {
        let mut keep = vec![false; runs.len()];
        for s in &refs {
            let k = if s == "last" {
                (!runs.is_empty()).then_some(0)
            } else if let Some(t) = s.strip_prefix("last:") {
                runs.iter().position(|(_, r)| what(r).contains(t))
            } else {
                let (pid, id) = parse_run_ref(s).ok_or_else(|| format!("'{s}' is not a run reference (r42, fewer-3fa2/r42, last, last:TEXT)"))?;
                runs.iter().position(|(pi, r)| r.id() == id && pid.is_none_or(|p| p == projects[*pi].pid))
            };
            match k {
                Some(k) => keep[k] = true,
                None => return Err(format!("no run {s} in this scope ({})", scope_names(&projects))),
            }
        }
        let mut k = 0;
        runs.retain(|_| {
            k += 1;
            keep[k - 1]
        });
    }
    if !newest_first {
        runs.reverse();
    }
    // (the kind only when the caller gave one: runs named by reference are taken whatever
    // their kind, and "kind all" in every heading was noise: the token test of 2026-11-23)
    let desc = format!("{}{}", scope_names(&projects), if kind == "logs" || a.s("kind").is_none() { String::new() } else { format!(", kind {kind}") });
    Ok(Scope { projects, runs, desc })
}

fn scope_names(projects: &[Project]) -> String {
    if projects.len() <= 3 { projects.iter().map(|p| p.pid.as_str()).collect::<Vec<_>>().join(", ") } else { format!("{} projects", projects.len()) }
}

/// A run's tags for a heading: "add /path/app.log, 2026-11-18 10:15:02, 1,204 lines".
fn tags(r: &RunRec) -> String {
    let mut s = r.kind().to_string();
    // (a file added by hand: its folder and name, not its whole path, and not the time it was
    // saved; a command's run keeps its command and time: the token test of 2026-11-23)
    let file = matches!(r.kind(), "add" | "kept" | "not-read") && r.0.get("cmd").is_none();
    let w = if file { short_path(&what(r)) } else { what(r) };
    if !w.is_empty() {
        s.push(' ');
        s.push_str(&w);
    }
    if let Some(st) = r.i("start").filter(|_| !file) {
        s.push_str(&format!(", {}", times::fmt_local(st)));
    }
    if let Some(e) = r.i("exit") {
        s.push_str(&format!(", exit {e}"));
    }
    if let Some(sig) = r.i("signal") {
        s.push_str(&format!(", signal {sig}"));
    }
    if let Some(b) = r.s("branch") {
        s.push_str(&format!(", {b}@{}", short_commit(r)));
    }
    if let Some(l) = r.i("lines") {
        s.push_str(&format!(", {} lines", group(l as u64)));
    }
    s
}

/// A path's last folder and name: "loghub_unseen/HDFS.log" (the whole path is in `logs`).
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').rsplitn(3, '/').collect();
    match parts.as_slice() {
        [name, dir, _] => format!("{dir}/{name}"),
        _ => p.to_string(),
    }
}

/// A run's commit cut to 9 characters ("?" when it has none). The cut never falls inside a
/// character: runs.jsonl is read as it is, so a commit there may hold any text.
fn short_commit(r: &RunRec) -> String {
    r.s("commit").map_or("?".to_string(), |c| c.chars().take(9).collect())
}

/// Where a run's text is: its archive; or, while it runs or waits to
/// be packed (also after a pack that failed: its capture is kept), its capture and the
/// capture's size now.
enum Text {
    Archive(PathBuf),
    Capture(PathBuf, u64),
}

/// How a run whose text is still its capture stands, as answers say it (step
/// 10: "r45 is still running").
enum Live {
    /// its `run` is still writing it
    Running,
    /// its `run` was stopped (killed) before it recorded the end: the capture's lock is gone
    /// (capture::writer_alive), and the next pack-pending packs it with the note "ended
    /// without a status"
    Killed,
    /// it ended and waits to be packed
    Ended,
    /// a pack failed and the next pack-pending tries again: the run's note
    Failed(String),
}

impl Live {
    fn of(r: &RunRec, capture: &Path) -> Live {
        match r.state() {
            "running" if crate::capture::writer_alive(capture) => Live::Running,
            "running" => Live::Killed,
            "packing" => Live::Ended,
            _ => Live::Failed(r.s("note").unwrap_or("no reason recorded").to_string()),
        }
    }

    /// After the run's reference, in a sentence: "r45 is still running".
    fn is(&self) -> String {
        match self {
            Live::Running => "is still running".into(),
            Live::Killed => "ended without a status (its `run` was stopped; the next `fewer pack-pending` packs it)".into(),
            Live::Ended => "has ended and waits to be packed".into(),
            Live::Failed(n) => format!("is not saved yet; the next `fewer pack-pending` tries again ({n})"),
        }
    }

    /// In a list of runs: "r45 (still running)".
    fn short(&self) -> String {
        match self {
            Live::Running => "still running".into(),
            Live::Killed => "ended without a status; the next `fewer pack-pending` packs it".into(),
            Live::Ended => "ended, waiting to be packed".into(),
            Live::Failed(n) => format!("saving failed: {n}"),
        }
    }
}

/// Why a run that is not ready is not counted by an answer made from catalogs (count without a
/// text, odd, errors, compare, quiet, guard, fixes): its state in words, and, for a run whose
/// text is still its capture, what comes once it is saved (`then`).
fn not_ready(p: &Project, r: &RunRec, then: &str) -> String {
    match searchable(p, r) {
        Err(why) => why,
        Ok(Text::Capture(cap, _)) => format!("{}: {then}", Live::of(r, &cap).short()),
        // (packed between reading runs.jsonl and now)
        Ok(Text::Archive(_)) => format!("being packed: {then}"),
    }
}

/// The store's folder of a project (projects/<pid> is in it).
fn home_of(p: &Project) -> &Path {
    p.dir.parent().and_then(Path::parent).unwrap_or(Path::new("/"))
}

/// How `logs` and `lines` name a file kept as it is, or not read: "kept as it
/// is, not read: bzip2 is not installed (12,345 bytes)", "not read: not text (...)".
fn no_log_text(r: &RunRec) -> String {
    let why = r.s("note").unwrap_or("no reason recorded");
    match (r.kind(), r.i("size")) {
        ("kept", Some(n)) => format!("kept as it is, not read: {why} ({} bytes)", group(n as u64)),
        ("kept", None) => format!("kept as it is, not read: {why}"),
        _ => format!("not read: {why}"),
    }
}

/// Whether a run can be searched now, and where its text is: Err(why) when it cannot.
fn searchable(p: &Project, r: &RunRec) -> Result<Text, String> {
    if crate::guard::no_log(r) {
        // (search and count pass such files over without a word: they hold no log lines)
        return Err(format!("{}; it has no lines to show", no_log_text(r)));
    }
    let archive = p.dir.join(store::run_file(r.id()));
    let capture = crate::capture::raw_dir(home_of(p), &p.pid).join(crate::capture::out_file(r.id()));
    match r.state() {
        "ready" => if archive.is_file() { Ok(Text::Archive(archive)) } else { Err("its archive is missing".into()) },
        "running" | "packing" | "failed" if capture.is_file() => Ok(Text::Capture(capture.clone(), std::fs::metadata(&capture).map(|m| m.len()).unwrap_or(0))),
        // (packed between reading runs.jsonl and now)
        "packing" if archive.is_file() => Ok(Text::Archive(archive)),
        "running" => Err("just started: no output saved yet".into()),
        "packing" => Err("being packed".into()),
        "failed" => Err(format!("saving failed: {}", r.s("note").unwrap_or("no reason recorded"))),
        s => Err(format!("{s}: {}", r.s("note").unwrap_or("not a log to search"))),
    }
}

// ---------------------------------------------------------------- logs

fn tool_logs(ctx: &Ctx, a: &Args) -> Result<String, String> {
    let sc = select(ctx, a, true)?;
    let limit = a.i("limit", 30) as usize;
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    let (mut lines, mut bytes, mut arch) = (0u64, 0u64, 0u64);
    let mut span: Option<(i64, i64)> = None;
    // runs whose text is still their capture: how each stands, and its size now
    let mut live: HashMap<usize, (Live, u64)> = HashMap::new();
    for (k, (pi, r)) in sc.runs.iter().enumerate() {
        lines += r.i("lines").unwrap_or(0) as u64;
        bytes += r.i("bytes").unwrap_or(0) as u64;
        arch += r.i("archive").unwrap_or(0) as u64;
        if let (Some(a), Some(b)) = (r.i("first"), r.i("last")) {
            span = Some(span.map_or((a, b), |(x, y)| (x.min(a), y.max(b))));
        }
        if r.state() != "ready"
            && let Ok(Text::Capture(cap, size)) = searchable(&sc.projects[*pi], r)
        {
            live.insert(k, (Live::of(r, &cap), size));
        }
    }
    let roots = if sc.projects.len() == 1 { format!(" ({})", sc.projects[0].root) } else { String::new() };
    let times = span.map(|(a, b)| format!(", lines from {}", ms_span(a, b))).unwrap_or_default();
    let running = live.values().filter(|l| matches!(l.0, Live::Running)).count() as u64;
    // (files kept as they are, and files not read, are runs too)
    let kept = sc.runs.iter().filter(|(_, r)| r.kind() == "kept").count() as u64;
    let not_read = sc.runs.iter().filter(|(_, r)| r.kind() == "not-read").count() as u64;
    let mut also = Vec::new();
    if running > 0 {
        also.push(format!("{} still running", group(running)));
    }
    if kept > 0 {
        also.push(format!("{} kept as {}, not read", group(kept), if kept == 1 { "it is" } else { "they are" }));
    }
    if not_read > 0 {
        also.push(format!("{} not read", group(not_read)));
    }
    let running = if also.is_empty() { String::new() } else { format!(" ({})", also.join(", ")) };
    ans.must(&format!(
        "{}{roots}: {}{running}, {}{times}, {} bytes of text in {} bytes of archives ({})",
        sc.desc,
        plural(sc.runs.len() as u64, "run"),
        plural(lines, "line"),
        group(bytes),
        group(arch),
        crate::answer::ratio(bytes, arch)
    ));
    let mut start = 0;
    if let Some(c) = a.s("cursor") {
        let (pid, id) = parse_run_ref(c).ok_or_else(|| format!("'{c}' is not a run reference"))?;
        start = sc.runs.iter().position(|(pi, r)| r.id() == id && pid.is_none_or(|p| p == sc.projects[*pi].pid)).ok_or_else(|| format!("the cursor's run {c} is not in this scope"))?;
    }
    let mut next = None;
    for (k, (pi, r)) in sc.runs.iter().enumerate().skip(start) {
        let p = &sc.projects[*pi];
        let mut s = format!("{} {}", run_ref(&p.pid, r.id()), r.kind());
        if let Some(st) = r.i("start") {
            s.push_str(&format!(" {}", times::fmt_local(st)));
            if let Some(e) = r.i("end") {
                s.push_str(&format!(" ({})", times::fmt_duration(e - st)));
            }
        }
        let w = what(r);
        if !w.is_empty() {
            s.push_str(&format!(" {w}"));
        }
        if let Some(e) = r.i("exit") {
            s.push_str(&format!(" exit {e}"));
        }
        if let Some(sig) = r.i("signal") {
            s.push_str(&format!(" signal {sig}"));
        }
        if let Some(b) = r.s("branch") {
            s.push_str(&format!(" {b}@{}", short_commit(r)));
        }
        s.push(':');
        if let Some(l) = r.i("lines") {
            s.push_str(&format!(" {} lines,", group(l as u64)));
        }
        if let (Some(a), Some(b)) = (r.i("first"), r.i("last")) {
            s.push_str(&format!(" times {},", ms_span(a, b)));
        }
        if let (Some(b), Some(ar)) = (r.i("bytes"), r.i("archive")) {
            s.push_str(&format!(" {} -> {} bytes,", group(b as u64), group(ar as u64)));
        }
        if let Some(f) = r.s("format") {
            s.push_str(&format!(" {f},"));
        }
        if let Some(h) = r.i("hidden").filter(|&h| h > 0) {
            s.push_str(&format!(" {} hidden,", plural(h as u64, "secret")));
        }
        match live.get(&k) {
            // its capture's size now (runs.jsonl has bytes and lines only from its end)
            Some((l, size)) => s.push_str(&format!(" {} bytes of output{}, {}", group(*size), if matches!(l, Live::Running) { " so far" } else { "" }, l.short())),
            // a file kept as it is, or not read: why
            None if crate::guard::no_log(r) && r.state() == "ready" => s.push_str(&format!(" {}", no_log_text(r))),
            None => {
                s.push_str(&format!(" {}", r.state()));
                if let Some(n) = r.s("note") {
                    s.push_str(&format!(" ({n})"));
                }
            }
        }
        // (k - start runs are shown so far)
        if k - start >= limit || !ans.line(&s) {
            next = Some(k);
            break;
        }
    }
    let last = match next {
        None => "all shown".to_string(),
        Some(k) => {
            let (pi, r) = &sc.runs[k];
            let c = run_ref(&sc.projects[*pi].pid, r.id());
            format!("left out: {} runs ({c} and older). Next: {}", group((sc.runs.len() - k) as u64), next_call("logs", &a.with_cursor(&c)))
        }
    };
    Ok(ans.finish(&last))
}

// ---------------------------------------------------------------- catalogs in answers

/// What a search notes of each match besides its line.
#[derive(Clone, Copy, Default)]
struct Want {
    /// (its key: Query::with_keys)
    /// its minute, from the run's catalog marks (counts per hour, time ranges)
    times: bool,
    /// its level, read with the run's log type (count by level)
    levels: bool,
}

/// Minutes (since 1970, UTC) shown local: "2026-11-17 09:12 to 11:40", the date again only
/// when it changes.
fn minute_span(a: i64, b: i64) -> String {
    let (fa, fb) = (times::fmt_local(a * 60_000), times::fmt_local(b * 60_000));
    if a == b {
        fa[..16].to_string()
    } else if fa[..10] == fb[..10] {
        format!("{} to {}", &fa[..16], &fb[11..16])
    } else {
        format!("{} to {}", &fa[..16], &fb[..16])
    }
}

/// A run's line times (Unix ms) shown local: "2026-11-17 09:12:03 to 11:40:59".
fn ms_span(a: i64, b: i64) -> String {
    let (fa, fb) = (times::fmt_local(a), times::fmt_local(b));
    if fa[..10] == fb[..10] { format!("{fa} to {}", &fb[11..]) } else { format!("{fa} to {fb}") }
}

/// An hour (since 1970, UTC) as answers show it: when it starts, local: "2026-11-18 10:00".
fn hour_text(h: i64) -> String {
    times::fmt_local(h * 3_600_000)[..16].to_string()
}

/// A key as answers show it: control bytes and bytes that are not UTF-8 as \xHH, cut at
/// `max` bytes.
/// A scope of one run: "r6 (add general/book.txt, 14,915 lines)", named in an answer's first
/// line in place of "in 1 of 1 run" and the run's own heading (the token tests of 2026-11-23);
/// "r6" alone when the call named its runs (the caller knows them).
fn one_run(sc: &Scope, a: &Args) -> Option<String> {
    match sc.runs.as_slice() {
        [(pi, r)] if a.0.get("runs").is_some() => Some(run_ref(&sc.projects[*pi].pid, r.id())),
        [(pi, r)] => Some(format!("{} ({})", run_ref(&sc.projects[*pi].pid, r.id()), tags(r))),
        _ => None,
    }
}

/// A scope of one run, its short name for a table's first line ("r6"): a table shows no run
/// heading to take the place of.
fn one_ref(sc: &Scope) -> Option<String> {
    match sc.runs.as_slice() {
        [(pi, r)] => Some(run_ref(&sc.projects[*pi].pid, r.id())),
        _ => None,
    }
}

/// A Claude Code session's run: its lines are shown as who said or did what (view.rs).
fn is_session(r: &RunRec) -> bool {
    r.s("format") == Some("claude-session") || (r.kind() == "session" && r.s("agent") == Some("claude"))
}

/// A line as answers show it: a session's line, read whole, as who said or did what
/// (view::session_view), cut at line_max; any other line as shown_line shows it.
fn view_line(r: &RunRec, bytes: &[u8], len: usize, line_max: usize) -> (String, bool, bool) {
    if bytes.len() >= len
        && is_session(r)
        && let Some(v) = crate::view::session_view(clean_end(bytes))
    {
        let (t, _, cut) = shown_line(v.as_bytes(), v.len(), line_max);
        return (t, false, cut);
    }
    shown_line(bytes, len, line_max)
}

fn key_text(k: &[u8], max: usize) -> String {
    let (s, _) = crate::clean::escape(k, false);
    let (t, n) = crate::clean::cut(&s, max);
    if n > 0 { format!("{t}…") } else { s }
}

/// The catalogs of some runs of a scope (scope indexes), whole: from their .cat files, or
/// made now from a run's bytes when it has none that fits (the tools never write one). Also
/// the runs whose catalog was made now, with why, and the runs that could not be read.
#[allow(clippy::type_complexity)]
fn load_catalogs(sc: &Scope, ks: &[usize], stop: &AtomicBool) -> (HashMap<usize, Catalog>, Vec<(usize, String)>, Vec<(usize, String)>) {
    let mut cats = HashMap::new();
    let (mut made, mut errs) = (Vec::new(), Vec::new());
    for &k in ks {
        let (pi, r) = &sc.runs[k];
        match catalog::of_run(&sc.projects[*pi].dir, r, true, &|| stop.load(Ordering::Relaxed)) {
            Ok((c, why)) => {
                if let Some(w) = why {
                    made.push((k, w));
                }
                cats.insert(k, c);
            }
            Err(e) => errs.push((k, e)),
        }
    }
    (cats, made, errs)
}

/// The line naming the runs whose catalog was made now for the answer.
fn made_line(sc: &Scope, made: &[(usize, String)]) -> Option<String> {
    if made.is_empty() {
        return None;
    }
    let s: Vec<String> = made.iter().take(10).map(|(k, why)| format!("r{} ({why})", sc.runs[*k].1.id())).collect();
    let more = if made.len() > 10 { format!(" and {} more", made.len() - 10) } else { String::new() };
    Some(format!("catalogs made now from the runs' bytes: {}{more}", s.join(", ")))
}

// ---------------------------------------------------------------- search and follow

/// A search of the scope's runs: what it found, in scope order.
struct Found {
    /// the runs searched: (scope index, matches), in order
    counts: Vec<(usize, u64)>,
    total: u64,
    /// the lines to show (with their run's scope index), in order (lines not collapsed)
    cand: Vec<(usize, Hit)>,
    /// the first match after the shown candidates: (scope index, line)
    after: Option<(usize, u64)>,
    /// where the search stopped (time limit or a stop): (scope index, next line)
    stopped: Option<(usize, u64)>,
    /// runs that could not be searched: (scope index, why)
    errors: Vec<(usize, String)>,
    /// runs of the scope that are not searched now (packing, failed, kept): (index, why)
    skipped: Vec<(usize, String)>,
    searched: usize,
    /// every run searched: (scope index, its start line, what it found besides `cand`)
    runs: Vec<(usize, u64, RunHits)>,
    /// runs whose catalog was made now for this answer: (scope index, why)
    made: Vec<(usize, String)>,
    /// the runs' marks, for a run searched again
    marks: HashMap<usize, Vec<Mark>>,
    /// runs whose text is still their capture (searched as it is now): (scope index, how it
    /// stands)
    live: Vec<(usize, Live)>,
}

#[allow(clippy::too_many_arguments)]
fn run_search(sc: &Scope, q: &Query, cursor: Option<&str>, limit: usize, keep: Keep, want: Want, time_limit: Duration, stop: &AtomicBool) -> Result<Found, String> {
    let deadline = Instant::now() + time_limit;
    // where the cursor starts
    let (mut first, mut from) = (0usize, 1u64);
    if let Some(c) = cursor {
        let (pid, id, line, _) = parse_line_ref(c).ok_or_else(|| format!("'{c}' is not a cursor (a reference such as fewer-3fa2/r42:118)"))?;
        first = sc.runs.iter().position(|(pi, r)| r.id() == id && pid.is_none_or(|p| p == sc.projects[*pi].pid)).ok_or_else(|| format!("the cursor's run {c} is not in this scope (was it forgotten, or the scope changed?)"))?;
        from = line;
    }
    let mut jobs = Vec::new();
    let mut idx = Vec::new();
    let mut f = Found {
        counts: Vec::new(),
        total: 0,
        cand: Vec::new(),
        after: None,
        stopped: None,
        errors: Vec::new(),
        skipped: Vec::new(),
        searched: 0,
        runs: Vec::new(),
        made: Vec::new(),
        marks: HashMap::new(),
        live: Vec::new(),
    };
    for (k, (pi, r)) in sc.runs.iter().enumerate().skip(first) {
        let p = &sc.projects[*pi];
        match searchable(p, r) {
            Ok(text) => {
                let (path, bytes, capture) = match text {
                    Text::Archive(path) => (path, r.i("bytes").unwrap_or(0) as u64, None),
                    Text::Capture(cap, size) => {
                        f.live.push((k, Live::of(r, &cap)));
                        (p.dir.join(store::run_file(r.id())), size, Some((cap, size)))
                    }
                };
                let mut job = Job { path, bytes, from: if k == first { from } else { 1 }, marks: None, level: None, capture };
                if want.times {
                    // each match's minute comes from the run's catalog marks (a run in
                    // progress: its catalog built from its capture)
                    let cat = match &job.capture {
                        Some((_, size)) => catalog::of_capture(home_of(p), &p.pid, r, *size).map(|c| (c, None)),
                        None => catalog::of_run(&p.dir, r, false, &|| stop.load(Ordering::Relaxed)),
                    };
                    match cat {
                        Ok((c, why)) => {
                            if let Some(w) = why {
                                f.made.push((k, w));
                            }
                            job.marks = Some(c.marks);
                        }
                        Err(e) => {
                            f.errors.push((k, e));
                            continue;
                        }
                    }
                }
                if want.levels {
                    // each match's level is read with the run's log type, as its catalog's
                    let prof = match &job.capture {
                        Some((cap, _)) => catalog::profile_of_capture(cap, &catalog::source_name(r)),
                        None => catalog::profile_of(&job.path, &catalog::source_name(r)),
                    };
                    match prof {
                        Ok((profile, file_ms)) => job.level = Some(LevelCtx { profile, zone: times::Zone::Local, file_ms }),
                        Err(e) => {
                            f.errors.push((k, e));
                            continue;
                        }
                    }
                }
                jobs.push(job);
                idx.push(k);
            }
            Err(why) => {
                if !matches!(r.kind(), "kept" | "not-read") {
                    f.skipped.push((k, why));
                }
            }
        }
    }
    let full = AtomicBool::new(false);
    let mut page_groups = 0usize;
    let handed = find::search_scope(&jobs, q, keep, deadline, stop, &full, &mut |j, mut h| {
        let k = idx[j];
        if let Some(e) = h.error.take() {
            f.errors.push((k, e));
            return true;
        }
        f.searched += 1;
        f.total += h.count;
        f.counts.push((k, h.count));
        if keep.groups > 0 {
            // lines collapsed by key: once the groups fill an answer, later runs keep no bytes
            page_groups += h.groups.len();
            if page_groups >= keep.groups || h.page_end.is_some() {
                full.store(true, Ordering::SeqCst);
            }
        } else if f.after.is_none() {
            let mut first_out: Option<u64> = None;
            for hit in std::mem::take(&mut h.kept) {
                if f.cand.len() < limit {
                    f.cand.push((k, hit));
                } else {
                    first_out = Some(hit.line);
                    break;
                }
            }
            if first_out.is_none() {
                first_out = h.first_unkept;
            }
            if let Some(l) = first_out {
                f.after = Some((k, l));
                full.store(true, Ordering::SeqCst);
            }
        }
        h.kept = Vec::new();
        let (stopped, lines) = (h.stopped, h.lines);
        f.runs.push((k, jobs[j].from, h));
        if stopped {
            f.stopped = Some((k, (lines + 1).max(jobs[j].from)));
            return false;
        }
        true
    });
    if f.stopped.is_none() && handed < jobs.len() {
        f.stopped = Some((idx[handed], jobs[handed].from));
    }
    for (j, job) in jobs.into_iter().enumerate() {
        if let Some(m) = job.marks {
            f.marks.insert(idx[j], m);
        }
    }
    f.errors.sort_by_key(|e| e.0);
    Ok(f)
}

/// A heading of the text searched: "timeout" (any case) (whole word).
fn what_text(q: &Query) -> String {
    let mut s = Json::Str(String::from_utf8_lossy(&q.text).into_owned()).to_string();
    if q.icase {
        s.push_str(" (any case)");
    }
    if q.whole {
        s.push_str(" (whole word)");
    }
    s
}

/// Per key over every searched run: (key, matching lines, scope index and line of its first
/// match), the most first, then in order of first appearance (the runs taken oldest first).
fn key_totals(f: &Found) -> Vec<(&[u8], u64, usize, u64)> {
    let mut at: HashMap<&[u8], usize> = HashMap::new();
    let mut v: Vec<(&[u8], u64, usize, u64)> = Vec::new();
    for (k, _, h) in f.runs.iter().rev() {
        for (i, key) in h.keys.iter().enumerate() {
            match at.get(&key[..]) {
                Some(&x) => v[x].1 += h.key_counts[i],
                None => {
                    at.insert(key, v.len());
                    v.push((key, h.key_counts[i], *k, h.key_first[i]));
                }
            }
        }
    }
    // (a stable sort: keys with the same count stay in order of first appearance)
    v.sort_by_key(|a| std::cmp::Reverse(a.1));
    v
}

/// Per hour over every searched run, in time order, and the matching lines without a clock
/// time.
fn hour_totals(f: &Found) -> (Vec<(i64, u64)>, u64) {
    let mut m: BTreeMap<i64, u64> = BTreeMap::new();
    let mut none = 0;
    for (_, _, h) in &f.runs {
        for (&hr, &n) in &h.hours {
            *m.entry(hr).or_insert(0) += n;
        }
        none += h.untimed;
    }
    (m.into_iter().collect(), none)
}

/// The count call that gives a whole table (`by` key or hour) of a search's matches.
fn count_call(a: &Args, by: &str) -> String {
    let mut v = a.0.clone();
    for k in ["limit", "context", "cursor", "line_max", "collapse"] {
        v.remove(k);
    }
    v.set("by", by.into());
    next_call("count", &v)
}

/// At most this many keys in a search's "per key" summary (the token test of 2026-11-23: every
/// key cost up to thousands of tokens nobody asked for); `count` by key gives them all.
const SUMMARY_KEYS: usize = 10;

/// "per key (3 keys): 30 "...", 5 "..."": the exact counts per key of every match, the most
/// first: at most SUMMARY_KEYS, as many as fit in `room` bytes.
fn per_key_line(f: &Found, a: &Args, room: usize) -> Option<String> {
    let keys = key_totals(f);
    if keys.len() < 2 {
        return None;
    }
    let mut s = format!("per key ({}):", plural(keys.len() as u64, "key"));
    let more = count_call(a, "key");
    let mut shown = 0;
    for (key, n, ..) in &keys {
        let item = format!(" {} {},", group(*n), Json::Str(key_text(key, 160)));
        if shown == SUMMARY_KEYS || (shown > 0 && s.len() + item.len() + more.len() + 20 > room) {
            break;
        }
        s.push_str(&item);
        shown += 1;
    }
    s.pop();
    if shown < keys.len() {
        s.push_str(&format!(" (+{} more: {more})", group((keys.len() - shown) as u64)));
    }
    Some(s)
}

/// At most this many hours listed one by one in a search's "per hour" summary; past them, the
/// span and the busiest hour (the token test of 2026-11-23); `count` by hour gives them all.
const SUMMARY_HOURS: usize = 6;

/// "per hour: 2026-11-18 10:00 12, 11:00 25; no time 3": the exact counts per hour of every
/// match, in time order, when there are at most SUMMARY_HOURS of them (and they fit in `room`
/// bytes); else "per hour: 523 hours, from 2017-12-10 06:00 to 2018-01-07 17:00, the most
/// 2,413 at 2017-12-10 13:00 (all: count ...)".
fn per_hour_line(f: &Found, a: &Args, room: usize) -> Option<String> {
    let (hours, none) = hour_totals(f);
    if hours.is_empty() || (hours.len() == 1 && none == 0) {
        return None;
    }
    let more = count_call(a, "hour");
    let mut s = String::from("per hour:");
    let listed = hours.len() <= SUMMARY_HOURS && {
        let mut day = String::new();
        let mut items = String::new();
        for &(h, n) in &hours {
            let t = hour_text(h);
            let label = if t[..10] == day[..] { t[11..].to_string() } else { t.clone() };
            items.push_str(&format!(" {label} {},", group(n)));
            day = t[..10].to_string();
        }
        items.pop();
        let fits = s.len() + items.len() + 40 <= room;
        if fits {
            s.push_str(&items);
        }
        fits
    };
    if !listed {
        let (first, last) = (hours[0].0, hours[hours.len() - 1].0);
        let &(peak, most) = hours.iter().max_by(|x, y| x.1.cmp(&y.1).then(y.0.cmp(&x.0))).expect("hours is not empty");
        s.push_str(&format!(
            " {}, from {} to {}, the most {} at {} (all: {more})",
            plural(hours.len() as u64, "hour"),
            hour_text(first),
            hour_text(last),
            group(most),
            hour_text(peak)
        ));
    }
    if none > 0 {
        s.push_str(&format!("; no time {}", group(none)));
    }
    Some(s)
}

/// The time range of the matches left out from (scope index k, line) on: run k's from that
/// line, and every later run's. None when none of them has a clock time.
fn left_span(f: &Found, k: usize, line: u64) -> Option<String> {
    let mut span: Option<(i64, i64)> = None;
    for (kk, from, h) in &f.runs {
        if *kk < k {
            continue;
        }
        if h.minutes_cut {
            return None;
        }
        let s = if *kk == k { h.minutes_from(line) } else { h.minutes_from(*from) };
        if let Some((x, y)) = s {
            span = Some(span.map_or((x, y), |(p, q)| (p.min(x), q.max(y))));
        }
    }
    span.map(|(x, y)| minute_span(x, y))
}

/// The heading, errors and runs not searched of a search answer.
fn search_head(sc: &Scope, a: &Args, q: &Query, f: &Found, follow: bool, ans: &mut Answer) {
    let runs_with = f.counts.iter().filter(|c| c.1 > 0).count();
    let from = a.s("cursor").map(|c| format!(", from {c}")).unwrap_or_default();
    let order = if follow { ", oldest first" } else { "" };
    let extra = [from.trim_start_matches(", "), order.trim_start_matches(", ")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
    let extra = if extra.is_empty() { String::new() } else { format!(" ({extra})") };
    let head = match (f.stopped, one_run(sc, a)) {
        (Some(_), _) => format!("at least {} {} {} in {} of {} searched so far ({}{from}{order}); stopped at the time limit", plural(f.total, "line"), holds(f.total), what_text(q), runs_with, plural(f.searched as u64, "run"), sc.desc),
        (None, Some(o)) if f.total == 0 => format!("no line holds {} in {o}{extra}", what_text(q)),
        (None, Some(o)) => format!("{} {} {} in {o}{extra}", plural(f.total, "line"), holds(f.total), what_text(q)),
        (None, None) if f.total == 0 => format!("no line holds {} in {} ({}{from})", what_text(q), plural(f.searched as u64, "run"), sc.desc),
        (None, None) => format!("{} {} {} in {} of {} ({}{from}{order})", plural(f.total, "line"), holds(f.total), what_text(q), runs_with, plural(f.searched as u64, "run"), sc.desc),
    };
    ans.must(&head);
    let rref = |k: usize| {
        let (pi, r) = &sc.runs[k];
        run_ref(&sc.projects[*pi].pid, r.id())
    };
    for (k, e) in f.errors.iter().take(20) {
        ans.must(&format!("{}: {e}", rref(*k)));
    }
    if f.errors.len() > 20 {
        ans.must(&format!("and {} more runs could not be read", f.errors.len() - 20));
    }
    if !f.skipped.is_empty() {
        let s: Vec<String> = f.skipped.iter().take(10).map(|(k, why)| format!("{} ({why})", rref(*k))).collect();
        let more = if f.skipped.len() > 10 { format!(" and {} more", f.skipped.len() - 10) } else { String::new() };
        ans.must(&format!("not searched: {}{more}", s.join(", ")));
    }
    for l in live_lines(sc, f, "searched") {
        ans.must(&l);
    }
    if let Some(m) = made_line(sc, &f.made) {
        ans.must(&m);
    }
}

/// A line for each run of a search that was read from its capture:
/// "fewer-3fa2/r45 is still running: searched its output so far (1,204 lines)"; at most 10,
/// then how many more. `did`: "searched" or "counted".
fn live_lines(sc: &Scope, f: &Found, did: &str) -> Vec<String> {
    let lines: HashMap<usize, u64> = f.runs.iter().map(|(k, _, h)| (*k, h.lines)).collect();
    let mut v = Vec::new();
    for (k, l) in f.live.iter().take(10) {
        let (pi, r) = &sc.runs[*k];
        let so_far = if matches!(l, Live::Running) { " so far" } else { "" };
        // (a run the time limit cut, or did not reach, is named in the left-out line)
        let what = match lines.get(k) {
            Some(n) if f.stopped.is_none_or(|s| s.0 != *k) => format!(": {did} its output{so_far} ({})", plural(*n, "line")),
            _ => String::new(),
        };
        v.push(format!("{} {}{what}", run_ref(&sc.projects[*pi].pid, r.id()), l.is()));
    }
    if f.live.len() > 10 {
        v.push(format!("and {} more runs not saved yet: {did} from their output so far", f.live.len() - 10));
    }
    v
}

/// The summaries of a search that shows not every match: exact counts per run, per key and
/// per hour, each within a share of the room left.
fn search_summaries(sc: &Scope, a: &Args, f: &Found, ans: &mut Answer) {
    let room = ans.room() / 5;
    let runs_with = f.counts.iter().filter(|c| c.1 > 0).count();
    if runs_with > 1 {
        let parts: Vec<String> = f.counts.iter().filter(|c| c.1 > 0).take(30).map(|&(k, n)| format!("r{} {}", sc.runs[k].1.id(), group(n))).collect();
        let more = if runs_with > 30 { format!(" (+{} more runs)", runs_with - 30) } else { String::new() };
        ans.line(&format!("per run: {}{more}", parts.join(", ")));
    }
    // (per key and per hour only when asked: the token test of 2026-11-23 found them the
    // largest part of the answers; count by key or by hour gives them)
    if !a.b("summary") {
        return;
    }
    if let Some(s) = per_key_line(f, a, room) {
        ans.line(&s);
    }
    if let Some(s) = per_hour_line(f, a, room) {
        ans.line(&s);
    }
}

fn tool_search(ctx: &Ctx, a: &Args, stop: &AtomicBool, follow: bool) -> Result<String, String> {
    let sc = select(ctx, a, !follow)?;
    let text = if follow { a.s("id").unwrap_or("") } else { a.s("text").unwrap_or("") };
    let mut q = Query::new(text.as_bytes(), !follow && a.b("icase"), follow || a.b("whole"));
    q.with_keys = !follow;
    let tool = if follow { "follow" } else { "search" };
    let limit = a.i("limit", if follow { 50 } else { 10 }) as usize;
    let context = if follow { 0 } else { a.i("context", 0) as u64 };
    // (before and after each hit: context's, unless asked on their own)
    let (before, after) = if follow { (0, 0) } else { (a.i("before", context as i64) as u64, a.i("after", context as i64) as u64) };
    let line_max = a.i("line_max", 400) as usize;
    let max_tokens = a.i("max_tokens", 3000) as usize;
    let time_limit = Duration::from_millis(a.i("time_limit_ms", 10_000) as u64);
    // lines with the same key are collapsed, unless `collapse` is false or lines around each
    // hit are asked for (then every matching line is shown on its own)
    // (a session's lines are not folded by key unless asked: two requests of the user share a
    // key; the token test of 2026-11-23)
    let sessions = sc.runs.iter().any(|(_, r)| is_session(r));
    let all_sessions = sessions && sc.runs.iter().all(|(_, r)| is_session(r));
    let collapse = !follow && before == 0 && after == 0 && a.0.get("collapse").and_then(Json::bool).unwrap_or(!all_sessions);
    let mut ans = Answer::new(max_tokens);
    // (a session's lines are read whole, to be shown as who said or did what)
    let keep_bytes = if sessions { (max_tokens * 4 + line_max).max(16 << 20) } else { max_tokens * 4 + line_max };
    let line_bytes = if sessions { 4 << 20 } else { 4 * line_max + 4096 };
    let keep = if collapse { Keep { lines: limit.max(64), bytes: keep_bytes, line_bytes, groups: limit } } else { Keep { lines: limit, bytes: keep_bytes, line_bytes, groups: 0 } };
    let want = Want { times: !follow, levels: false };
    let f = run_search(&sc, &q, a.s("cursor"), if collapse { 0 } else { limit }, keep, want, time_limit, stop)?;
    search_head(&sc, a, &q, &f, follow, &mut ans);
    if collapse {
        return render_collapsed(&sc, a, &q, &f, ans, keep, line_max, time_limit, stop);
    }
    let rref = |k: usize| {
        let (pi, r) = &sc.runs[k];
        run_ref(&sc.projects[*pi].pid, r.id())
    };
    // when not all will be shown: exact counts per run, per key and per hour, so the search
    // can be narrowed
    let all_kept = f.after.is_none() && f.stopped.is_none();
    if !all_kept {
        search_summaries(&sc, a, &f, &mut ans);
    }
    // the lines around the hits (context): fetched per run
    let mut around: HashMap<(usize, u64), Vec<u8>> = HashMap::new();
    if (before > 0 || after > 0) && !f.cand.is_empty() {
        let mut by_run: Vec<(usize, Vec<(u64, u64)>)> = Vec::new();
        for (k, h) in &f.cand {
            let r = (h.line.saturating_sub(before).max(1), h.line + after);
            match by_run.last_mut() {
                Some((kk, v)) if kk == k => v.push(r),
                _ => by_run.push((*k, vec![r])),
            }
        }
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        for (k, ranges) in by_run {
            let (pi, r) = &sc.runs[k];
            let got = match searchable(&sc.projects[*pi], r) {
                Ok(Text::Capture(cap, size)) => crate::capture::lines(&cap, size, &ranges),
                _ => fetch::fetch(&sc.projects[*pi].dir.join(store::run_file(r.id())), &ranges, threads),
            };
            if let Ok(v) = got {
                for (l, b) in v {
                    around.insert((k, l), b);
                }
            }
        }
    }
    // the lines, in order, while they fit
    let mut cut_lines = 0usize;
    let mut cut_ref: Option<String> = None;
    let mut shown = 0u64;
    let mut not_shown: Option<(usize, u64)> = None;
    let mut cur_run = usize::MAX;
    let mut last_shown: Option<(usize, u64)> = None;
    let counts: HashMap<usize, u64> = f.counts.iter().copied().collect();
    // a line like the one shown above it, as the words that changed (the token test of
    // 2026-11-23): `chain` is the last line shown, in its run
    let compact = a.0.get("compact").and_then(Json::bool).unwrap_or(true);
    let one = sc.runs.len() == 1;
    let mut shown_last: Option<(usize, Vec<(u64, String)>)> = None;
    let mut i = 0;
    while i < f.cand.len() {
        let (k, h) = &f.cand[i];
        let (pi, r) = &sc.runs[*k];
        let pid = &sc.projects[*pi].pid;
        let mut block: Vec<String> = Vec::new();
        let mut recent: Vec<(u64, String)> = shown_last.as_ref().filter(|x| x.0 == *k).map(|x| x.1.clone()).unwrap_or_default();
        let mut cleaned_ref = None;
        let mut cuts = 0;
        if *k != cur_run && !one {
            block.push(format!("== {} {}: {} matching", rref(*k), tags(r), group(counts.get(k).copied().unwrap_or(0))));
        }
        // the hits of this group: with context, hits whose lines touch are shown together
        let mut j = i + 1;
        if before == 0 && after == 0 {
            let (t, cl, cut) = view_line(r, &h.bytes, h.len, line_max);
            let rf = line_ref(pid, r.id(), h.line);
            if cl {
                cleaned_ref = Some(rf.clone());
            }
            cuts += cut as usize;
            let c = if compact { compact_line(&recent, &t) } else { None };
            block.push(format!("{rf} {}", c.as_deref().unwrap_or(&t)));
            remember(&mut recent, h.line, t);
        } else {
            let mut hi = h.line + after;
            while j < f.cand.len() && f.cand[j].0 == *k && f.cand[j].1.line <= hi + before + 1 {
                hi = f.cand[j].1.line + after;
                j += 1;
            }
            // (a match not shown yet is never shown as a context line: the next answer shows it)
            if let Some((ak, al)) = f.after
                && ak == *k
                && al <= hi
            {
                hi = al - 1;
            }
            let lo = h.line.saturating_sub(before).max(1);
            let hits: Vec<u64> = f.cand[i..j].iter().map(|c| c.1.line).collect();
            let start_skip = last_shown.filter(|ls| ls.0 == *k).map_or(lo, |ls| ls.1 + 1).max(lo);
            block.push(format!("{}:{}-{}", rref(*k), start_skip, hi));
            for l in start_skip..=hi {
                let hit = hits.contains(&l);
                let bytes = if hit { f.cand[i..j].iter().find(|c| c.1.line == l).map(|c| (c.1.bytes.clone(), c.1.len)) } else { around.get(&(*k, l)).map(|b| (clean_end(b).to_vec(), clean_end(b).len())) };
                let Some((b, len)) = bytes else { continue };
                let (t, cl, cut) = view_line(r, &b, len, line_max);
                if cl && cleaned_ref.is_none() {
                    cleaned_ref = Some(line_ref(pid, r.id(), l));
                }
                cuts += cut as usize;
                let c = if compact { compact_line(&recent, &t) } else { None };
                    block.push(format!("{}{l} {}", if hit { "> " } else { "  " }, c.as_deref().unwrap_or(&t)));
                remember(&mut recent, l, t);
            }
        }
        let size: usize = block.iter().map(|s| s.len() + 1).sum();
        if !ans.fits(size) {
            if shown == 0 {
                // nothing shown yet: the first hit alone, cut to the room left, so that
                // following cursors always moves on
                let (t, cl, _) = view_line(r, &h.bytes, h.len, line_max);
                let rf = line_ref(pid, r.id(), h.line);
                if cl {
                    ans.cleaned(&rf);
                }
                ans.must(&format!("{rf} {t}"));
                shown = 1;
                i += 1;
                if i < f.cand.len() {
                    not_shown = Some((f.cand[i].0, f.cand[i].1.line));
                }
            } else {
                not_shown = Some((*k, h.line));
            }
            break;
        }
        for s in &block {
            ans.line(s);
        }
        shown_last = Some((*k, recent));
        if let Some(c) = cleaned_ref {
            ans.cleaned(&c);
        }
        if cuts > 0 && cut_ref.is_none() {
            cut_ref = Some(line_ref(pid, r.id(), h.line));
        }
        cut_lines += cuts;
        shown += (j - i) as u64;
        last_shown = Some((*k, f.cand[j - 1].1.line + after));
        cur_run = *k;
        i = j;
    }
    // the last line: all shown, or what was left out and the next call
    let next = not_shown.or(f.after).or(f.stopped);
    let last = left_line(&sc, a, tool, &f, next, f.total - shown, follow, cut_lines, cut_ref.as_deref(), line_max);
    Ok(ans.finish(&last))
}

/// How many lines shown before a line are looked at for one like it.
const LIKE_WINDOW: usize = 8;

/// Keep a shown line (its number and text) among the last LIKE_WINDOW of its run.
fn remember(recent: &mut Vec<(u64, String)>, line: u64, text: String) {
    recent.push((line, text));
    if recent.len() > LIKE_WINDOW {
        recent.remove(0);
    }
}

/// A line against the last lines shown of its run: the one with the same number of words
/// (split at single spaces, at least 4) and the most of them in place, when at most a third
/// changed and it comes out a third shorter at the least: "≈ N" (that line's number) and its
/// words, "·" for each word line N has in that place, the words at the end that line N has
/// too left out (exact: line N with the words given put in); "= N" for the same line. Else None, and the line is shown whole (so is one with a "·").
fn compact_line(recent: &[(u64, String)], cur: &str) -> Option<String> {
    let b: Vec<&str> = cur.split(' ').collect();
    if b.len() < 4 || b.contains(&"·") {
        return None;
    }
    let mut best: Option<(usize, u64, Vec<&str>)> = None;
    for (line, prev) in recent.iter().rev() {
        let a: Vec<&str> = prev.split(' ').collect();
        if a.len() != b.len() {
            continue;
        }
        let same = a.iter().zip(&b).filter(|(x, y)| x == y).count();
        if best.as_ref().is_none_or(|x| same > x.0) {
            best = Some((same, *line, a));
        }
    }
    let (same, line, a) = best?;
    if same == b.len() {
        return Some(format!("= {line}"));
    }
    if (b.len() - same) * 3 > b.len() {
        return None;
    }
    let mut words: Vec<&str> = a.iter().zip(&b).map(|(x, y)| if x == y { "·" } else { *y }).collect();
    // the words at the end that line N has too are left out (not when the last word left
    // would be empty: the line would end in a space)
    while words.len() > 1 && words[words.len() - 1] == "·" && !words[words.len() - 2].is_empty() {
        words.pop();
    }
    let s = format!("≈ {line} {}", words.join(" "));
    (s.len() * 3 <= cur.len() * 2).then_some(s)
}

/// The last line of a search answer: "all shown", or what was left out (lines, runs, their
/// time range, what the time limit left unsearched) and the next call, which goes on from
/// `next` (scope index, line).
#[allow(clippy::too_many_arguments)]
fn left_line(sc: &Scope, a: &Args, tool: &str, f: &Found, next: Option<(usize, u64)>, found_not_shown: u64, follow: bool, cut_lines: usize, cut_ref: Option<&str>, line_max: usize) -> String {
    let short = |k: usize| format!("r{}", sc.runs[k].1.id());
    let cut_note = |s: &mut String| {
        if cut_lines > 0 {
            let call = Json::o(vec![("ref", cut_ref.unwrap_or_default().into()), ("line_max", Json::from(65_536u64))]);
            s.push_str(&format!("; {} lines cut at {} bytes (line_max; one whole: lines {call})", group(cut_lines as u64), group(line_max as u64)));
        }
    };
    let Some((k, line)) = next else {
        let mut s = "all shown".to_string();
        cut_note(&mut s);
        return s;
    };
    let c = line_ref(&sc.projects[sc.runs[k].0].pid, sc.runs[k].1.id(), line);
    let mut s = String::from("left out: ");
    if found_not_shown > 0 {
        let runs: Vec<String> = f.counts.iter().filter(|c| c.1 > 0 && c.0 >= k).map(|c| short(c.0)).collect();
        let list = if runs.len() > 8 { format!("{}, ... {}", runs[..6].join(", "), runs[runs.len() - 1]) } else { runs.join(", ") };
        // (the time range of what is left: only when it spans several runs)
        let span = if follow || runs.len() < 2 { None } else { left_span(f, k, line) }.map(|t| format!("; {t}")).unwrap_or_default();
        if sc.runs.len() == 1 {
            s.push_str(&plural(found_not_shown, "line"));
        } else {
            s.push_str(&format!("{} in {} ({list}{span})", plural(found_not_shown, "line"), plural(runs.len() as u64, "run")));
        }
    }
    if let Some((sk, sl)) = f.stopped {
        if found_not_shown > 0 {
            s.push_str(", and ");
        }
        let rest = sc.runs.len() - sk - 1;
        s.push_str(&format!(
            "not searched yet (time limit): {} from line {sl}{}",
            short(sk),
            if rest > 0 { format!(" and {} {} runs", rest, if follow { "newer" } else { "older" }) } else { String::new() }
        ));
    }
    cut_note(&mut s);
    s.push_str(&format!(". Next: {}", next_call(tool, &a.with_cursor(&c))));
    s
}

/// One run's collapsed lines in an answer: its heading, then per group its first line with
/// its reference and, for a group of several lines, how many have the same key, the last
/// one's reference and the ranges of their numbers.
struct Block {
    lines: Vec<String>,
    /// bytes of the heading, and of each group's lines (line ends included)
    head: usize,
    sizes: Vec<usize>,
    cleaned: Option<String>,
    cuts: usize,
    cut_ref: Option<String>,
    /// matching lines in its groups
    covered: u64,
}

impl Block {
    fn size(&self) -> usize {
        self.head + self.sizes.iter().sum::<usize>()
    }
}

/// "; 12-48 ms, 1.5s-3s": the ranges of a group's measured values that differ (at most 3):
/// numbers with a unit, written right after them ("250ms") or as the key's next word
/// ("<num> ms"). Numbers without a unit (IDs, counts, parts of times) are not ranged.
fn ranges_text(key: &[u8], ranges: &[Range]) -> String {
    let mut parts = Vec::new();
    for r in ranges {
        let ((lo, glued), (hi, _)) = (crate::msgkey::num_parts(&r.min), crate::msgkey::num_parts(&r.max));
        if !r.ok || find::dec_cmp(lo, hi).is_eq() {
            continue;
        }
        let next = key.get(r.at as usize + 5..).and_then(|a| a.strip_prefix(b" ")).map(|w| &w[..w.iter().position(|&c| c == b' ').unwrap_or(w.len())]).map(|w| w.strip_suffix(b",").or(w.strip_suffix(b".")).or(w.strip_suffix(b";")).unwrap_or(w)).filter(|w| crate::msgkey::is_unit(w));
        let s = match (glued.is_empty(), next) {
            (false, _) => format!("{}-{}", String::from_utf8_lossy(&r.min), String::from_utf8_lossy(&r.max)),
            (true, Some(u)) => format!("{}-{} {}", String::from_utf8_lossy(lo), String::from_utf8_lossy(hi), String::from_utf8_lossy(u)),
            (true, None) => continue,
        };
        if parts.len() == 3 {
            break;
        }
        parts.push(s);
    }
    if parts.is_empty() { String::new() } else { format!("; {}", parts.join(", ")) }
}

fn render_groups(sc: &Scope, k: usize, h: &RunHits, line_max: usize) -> Block {
    let (pi, r) = &sc.runs[k];
    let pid = &sc.projects[*pi].pid;
    // (a one-run answer names its run in its first line: no heading)
    let mut b = Block { head: 0, lines: Vec::new(), sizes: Vec::new(), cleaned: None, cuts: 0, cut_ref: None, covered: 0 };
    if sc.runs.len() != 1 {
        let heading = format!("== {} {}: {} matching", run_ref(pid, r.id()), tags(r), group(h.count));
        b.head = heading.len() + 1;
        b.lines.push(heading);
    }
    for g in &h.groups {
        let (t, cl, cut) = view_line(r, &g.first.bytes, g.first.len, line_max);
        let rf = line_ref(pid, r.id(), g.first.line);
        if cl && b.cleaned.is_none() {
            b.cleaned = Some(rf.clone());
        }
        if cut {
            b.cuts += 1;
            if b.cut_ref.is_none() {
                b.cut_ref = Some(rf.clone());
            }
        }
        let first = format!("{rf} {t}");
        let mut size = first.len() + 1;
        b.lines.push(first);
        if g.count > 1 {
            let more = format!("  ×{} with this key, to {}{}", group(g.count), line_ref(pid, r.id(), g.last), ranges_text(&h.keys[g.key as usize], &g.ranges));
            size += more.len() + 1;
            b.lines.push(more);
        }
        b.sizes.push(size);
        b.covered += g.count;
    }
    b
}

/// One run searched again with room for `groups` groups only (its first lines), so that the
/// groups shown hold exactly the lines before the next group's first line.
fn research(sc: &Scope, f: &Found, i: usize, q: &Query, keep: Keep, time_limit: Duration, stop: &AtomicBool) -> Result<RunHits, String> {
    let (k, from, _) = &f.runs[i];
    let (pi, r) = &sc.runs[*k];
    let path = sc.projects[*pi].dir.join(store::run_file(r.id()));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let o = RunOpts { from: *from, keep, threads, deadline: Instant::now() + time_limit, stop, marks: f.marks.get(k).map(|m| &m[..]), level: None };
    let mut h = find::search_run(&path, q, &o);
    match h.error.take() {
        Some(e) => Err(format!("{}: {e}", run_ref(&sc.projects[*pi].pid, r.id()))),
        None => Ok(h),
    }
}

/// A search answer with lines collapsed by key. An answer covers the matching lines from its
/// cursor up to its last line's "Next" cursor, each line once: shown, or counted in the group
/// of its key in its run ("×37 with this key, to r42:1890"). Runs are taken whole; the first
/// run of an answer that cannot show all its groups is searched again with room for fewer.
#[allow(clippy::too_many_arguments)]
fn render_collapsed(sc: &Scope, a: &Args, q: &Query, f: &Found, mut ans: Answer, keep: Keep, line_max: usize, time_limit: Duration, stop: &AtomicBool) -> Result<String, String> {
    let limit = keep.groups;
    // the runs taken, while their groups stay within the limit (the first run with matches
    // always: its groups are at most the limit)
    let mut take: Vec<usize> = Vec::new();
    let mut end: Option<(usize, u64)> = None;
    let mut n = 0;
    for (i, (k, _, h)) in f.runs.iter().enumerate() {
        if h.count == 0 {
            continue;
        }
        let first_line = h.groups.first().map(|g| g.first.line).or(h.first_unkept).or(h.key_first.first().copied()).unwrap_or(1);
        if n > 0 && (h.groups.is_empty() || n + h.groups.len() > limit) {
            end = Some((*k, first_line));
            break;
        }
        take.push(i);
        n += h.groups.len();
        if let Some(pe) = h.page_end {
            end = Some((*k, pe));
            break;
        }
    }
    if end.is_none() {
        end = f.stopped;
    }
    let mut blocks: Vec<(usize, Block)> = take.iter().map(|&i| (i, render_groups(sc, f.runs[i].0, &f.runs[i].2, line_max))).collect();
    let mut covered = 0u64;
    let (mut cut_lines, mut cut_ref) = (0usize, None::<String>);
    let mut show = |ans: &mut Answer, b: &Block, must: bool, covered: &mut u64| {
        for l in &b.lines {
            if must {
                ans.must(l);
            } else {
                ans.line(l);
            }
        }
        if let Some(c) = &b.cleaned {
            ans.cleaned(c);
        }
        cut_lines += b.cuts;
        if cut_ref.is_none() {
            cut_ref = b.cut_ref.clone();
        }
        *covered += b.covered;
    };
    if end.is_none() && take.iter().all(|&i| !f.runs[i].2.groups.is_empty()) && ans.fits(blocks.iter().map(|(_, b)| b.size()).sum()) {
        // every match fits: no summaries needed
        for (_, b) in &blocks {
            show(&mut ans, b, false, &mut covered);
        }
    } else {
        search_summaries(sc, a, f, &mut ans);
        for (bi, (i, b)) in blocks.drain(..).enumerate() {
            let (k, from, h) = &f.runs[i];
            if !h.groups.is_empty() && ans.fits(b.size()) {
                show(&mut ans, &b, false, &mut covered);
                continue;
            }
            let first_line = h.groups.first().map(|g| g.first.line).or(h.first_unkept).unwrap_or(*from);
            if bi > 0 {
                // a later run that does not fit is left whole for the next answer
                end = Some((*k, first_line));
                break;
            }
            // the first run cannot show all its groups: it is searched again with room for
            // as many as fit (at least one), so the groups shown hold exactly the lines
            // before the next group's first line
            let room = ans.room();
            let mut j = 0;
            let mut used = b.head;
            while j < b.sizes.len() && used + b.sizes[j] < room {
                used += b.sizes[j];
                j += 1;
            }
            let mut j = j.clamp(1, limit.max(1));
            let cut = loop {
                let h2 = research(sc, f, i, q, Keep { groups: j, ..keep }, time_limit, stop)?;
                let b2 = render_groups(sc, *k, &h2, line_max);
                let fits = ans.fits(b2.size());
                if fits || j == 1 {
                    show(&mut ans, &b2, !fits, &mut covered);
                    break match (h2.page_end, h2.stopped) {
                        (Some(pe), _) => Some((*k, pe)),
                        (None, true) => Some((*k, (h2.lines + 1).max(*from))),
                        // (its groups held all its lines: the next run goes on as usual)
                        (None, false) => None,
                    };
                }
                j -= 1;
            };
            if cut.is_some() {
                end = cut;
                break;
            }
        }
    }
    let last = left_line(sc, a, "search", f, end, f.total.saturating_sub(covered), false, cut_lines, cut_ref.as_deref(), line_max);
    Ok(ans.finish(&last))
}

/// "holds" or "hold".
fn holds(n: u64) -> &'static str {
    if n == 1 { "holds" } else { "hold" }
}

/// "1 line", "2 lines".
fn plural(n: u64, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{} {what}s", group(n)) }
}

/// A line's bytes without its line end (fetch gives them with it).
fn clean_end(b: &[u8]) -> &[u8] {
    b.strip_suffix(b"\n").unwrap_or(b)
}

// ---------------------------------------------------------------- count

fn tool_count(ctx: &Ctx, a: &Args, stop: &AtomicBool) -> Result<String, String> {
    let by = a.s("by").unwrap_or("run");
    if by != "run" {
        return count_table(ctx, a, stop, by);
    }
    let sc = select(ctx, a, true)?;
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    // the cursor: a run (counted from its start), or a line of a run (a count cut by the time
    // limit goes on from the line it reached, so every call moves on and no line is counted
    // twice across calls)
    let (mut start, mut from_line) = (0, 1u64);
    if let Some(c) = a.s("cursor") {
        let (pid, id, line) = match parse_line_ref(c) {
            Some((pid, id, line, _)) => (pid, id, line),
            None => {
                let (pid, id) = parse_run_ref(c).ok_or_else(|| format!("'{c}' is not a cursor (a run or line reference such as fewer-3fa2/r42 or fewer-3fa2/r42:118)"))?;
                (pid, id, 1)
            }
        };
        start = sc.runs.iter().position(|(pi, r)| r.id() == id && pid.is_none_or(|p| p == sc.projects[*pi].pid)).ok_or_else(|| format!("the cursor's run {c} is not in this scope"))?;
        from_line = line;
    }
    let from = a.s("cursor").map(|c| format!(", from {c}")).unwrap_or_default();
    // where a count goes on: a run's reference, with the line when it is past the first
    let place = |k: usize, line: u64| {
        let (pi, r) = &sc.runs[k];
        if line > 1 { line_ref(&sc.projects[*pi].pid, r.id(), line) } else { run_ref(&sc.projects[*pi].pid, r.id()) }
    };
    // (scope index, count) of every run counted, in order
    let mut rows: Vec<(usize, u64)> = Vec::new();
    let mut errors: Vec<(usize, String)> = Vec::new();
    // runs of the scope that cannot be counted now (packing, failed): named in the answer,
    // as search names them, so nothing is left out silently
    let mut skipped: Vec<(usize, String)> = Vec::new();
    // where a count cut by the time limit stopped: (scope index, the next line to count)
    let mut stopped: Option<(usize, u64)> = None;
    // runs counted from their captures (a text's count reads them as search does)
    let mut live: Vec<String> = Vec::new();
    let head = match a.s("text") {
        None => {
            let mut total = 0u64;
            for (k, (pi, r)) in sc.runs.iter().enumerate().skip(start) {
                if crate::guard::no_log(r) {
                    // (a file kept as it is, or not read: no lines to count)
                    continue;
                }
                if r.state() == "ready" {
                    let mut n = r.i("lines").unwrap_or(0) as u64;
                    if k == start {
                        n = n.saturating_sub(from_line - 1);
                    }
                    total += n;
                    rows.push((k, n));
                } else {
                    // (a run in progress too: its line count comes with its end, from
                    // runs.jsonl, as every other run's here)
                    skipped.push((k, not_ready(&sc.projects[*pi], r, "counted once it is saved")));
                }
            }
            format!("{} in {} ({}{from})", plural(total, "line"), plural(rows.len() as u64, "run"), sc.desc)
        }
        Some(t) => {
            let q = Query::new(t.as_bytes(), a.b("icase"), a.b("whole"));
            let time_limit = Duration::from_millis(a.i("time_limit_ms", 10_000) as u64);
            let sub = Scope { projects: sc.projects.clone(), runs: sc.runs[start..].to_vec(), desc: sc.desc.clone() };
            let cursor = (from_line > 1).then(|| place(start, from_line));
            let f = run_search(&sub, &q, cursor.as_deref(), 0, Keep::NONE, Want::default(), time_limit, stop)?;
            rows = f.counts.iter().filter(|c| c.1 > 0).map(|&(k, n)| (k + start, n)).collect();
            errors = f.errors.iter().map(|(k, e)| (k + start, e.clone())).collect();
            skipped = f.skipped.iter().map(|(k, e)| (k + start, e.clone())).collect();
            stopped = f.stopped.map(|(k, line)| (k + start, line));
            live = live_lines(&sub, &f, "counted");
            let runs_with = rows.len();
            match f.stopped {
                Some(_) => format!("at least {} {} {} in {} of {} searched so far ({}{from}); stopped at the time limit", plural(f.total, "line"), holds(f.total), what_text(&q), runs_with, plural(f.searched as u64, "run"), sc.desc),
                None => format!("{} {} {} in {} of {} ({}{from})", plural(f.total, "line"), holds(f.total), what_text(&q), runs_with, plural(f.searched as u64, "run"), sc.desc),
            }
        }
    };
    // (one run, counted whole: its count and name in the first line, no row; the token tests
    // of 2026-11-23)
    let head = match one_run(&sc, a) {
        Some(o) if stopped.is_none() && start == 0 && from_line == 1 && errors.is_empty() && skipped.is_empty() && rows.len() <= 1 => {
            let n = rows.first().map_or(0, |r| r.1);
            rows.clear();
            match a.s("text") {
                None => format!("{} in {o}", plural(n, "line")),
                Some(t) => {
                    let q = Query::new(t.as_bytes(), a.b("icase"), a.b("whole"));
                    if n == 0 { format!("no line holds {} in {o}", what_text(&q)) } else { format!("{} {} {} in {o}", plural(n, "line"), holds(n), what_text(&q)) }
                }
            }
        }
        _ => head,
    };
    ans.must(&head);
    let rref = |k: usize| {
        let (pi, r) = &sc.runs[k];
        run_ref(&sc.projects[*pi].pid, r.id())
    };
    for (k, e) in errors.iter().take(20) {
        ans.must(&format!("{}: {e}", rref(*k)));
    }
    if errors.len() > 20 {
        ans.must(&format!("and {} more runs could not be read", errors.len() - 20));
    }
    if !skipped.is_empty() {
        let s: Vec<String> = skipped.iter().take(10).map(|(k, why)| format!("{} ({why})", rref(*k))).collect();
        let more = if skipped.len() > 10 { format!(" and {} more", skipped.len() - 10) } else { String::new() };
        ans.must(&format!("not counted: {}{more}", s.join(", ")));
    }
    for l in &live {
        ans.must(l);
    }
    let mut next = None;
    for (n, &(k, c)) in rows.iter().enumerate() {
        let (_, r) = &sc.runs[k];
        // a row that covers only part of its run says which lines
        let first = if k == start { from_line } else { 1 };
        let part = match stopped {
            Some((sk, sl)) if sk == k => format!(" (lines {} to {} so far)", group(first), group(sl.saturating_sub(1))),
            _ if first > 1 => format!(" (from line {})", group(first)),
            _ => String::new(),
        };
        if !ans.line(&format!("{} {}{part}  {}", rref(k), group(c), tags(r))) {
            next = Some(n);
            break;
        }
    }
    let last = match (next, stopped) {
        (None, None) => "all shown".to_string(),
        (Some(n), _) => {
            let k = rows[n].0;
            let c = place(k, if k == start { from_line } else { 1 });
            format!("left out: {} runs ({} and after). Next: {}", rows.len() - n, rref(k), next_call("count", &a.with_cursor(&c)))
        }
        (None, Some((k, line))) => {
            let c = place(k, line);
            format!("left out: not counted yet (time limit): from {c} on. Next: {}", next_call("count", &a.with_cursor(&c)))
        }
    };
    Ok(ans.finish(&last))
}


/// `count` by key, hour or level: a table of exact counts, the biggest first (hours in time
/// order, levels from error down). Without a text, the runs' catalogs give records (a stack
/// trace is one record); with a text, a full scan gives the matching lines, each under its own
/// key, hour or level. "#40" as the cursor goes on from row 40.
/// `count by ip`: the matching lines read for their addresses, at most this many (and bytes).
const IP_LINES: usize = 2_000_000;
const IP_BYTES: usize = 512 << 20;

/// The IPv4 addresses in a line: four numbers of 1 to 3 digits, each at most 255, joined by
/// dots, not inside a longer run of digits, dots and letters.
fn ipv4s(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'.' || c == b'_';
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && word(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        let mut parts = 0;
        let mut ok = true;
        loop {
            let s = i;
            while i < b.len() && b[i].is_ascii_digit() && i - s < 4 {
                i += 1;
            }
            let n = i - s;
            if n == 0 || n > 3 || std::str::from_utf8(&b[s..i]).ok().and_then(|t| t.parse::<u16>().ok()).is_none_or(|v| v > 255) {
                ok = false;
                break;
            }
            parts += 1;
            if parts == 4 || i >= b.len() || b[i] != b'.' {
                break;
            }
            i += 1;
        }
        let end_ok = i >= b.len() || !(b[i].is_ascii_alphanumeric() || b[i] == b'_' || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)));
        if ok && parts == 4 && end_ok {
            out.push(&b[start..i]);
        } else {
            // (not an address: go past this run of digits and dots)
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            i = i.max(start + 1);
        }
    }
    out
}

/// `count by path`: the file paths in a line, each once. A path is a word (the line split at
/// spaces, quotes, brackets and , ; = | `) with a '/' or '\\', whose last part has a dot and
/// 1 to 10 letters or digits after it (src/main.rs, ../x/y.py, /usr/lib/a.so), without the
/// ":12" or ":12:5" of a line and column at its end; never a URL.
fn paths(b: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    for w in b.split(|&c| c.is_ascii_whitespace() || b"\"'()[]{}<>,;=`|".contains(&c)) {
        let mut w = w;
        for _ in 0..2 {
            if let Some(i) = w.iter().rposition(|&c| c == b':')
                && i + 1 < w.len()
                && w[i + 1..].iter().all(u8::is_ascii_digit)
            {
                w = &w[..i];
            }
        }
        while let Some((&last, rest)) = w.split_last() {
            if last == b'.' || last == b':' {
                w = rest;
            } else {
                break;
            }
        }
        if w.len() < 3 || w.len() > 400 || !w.iter().any(|&c| c == b'/' || c == b'\\') || w.windows(3).any(|x| x == b"://") {
            continue;
        }
        let name = &w[w.iter().rposition(|&c| c == b'/' || c == b'\\').map_or(0, |i| i + 1)..];
        let Some(dot) = name.iter().rposition(|&c| c == b'.') else { continue };
        let ext = &name[dot + 1..];
        if ext.is_empty() || ext.len() > 10 || !ext.iter().all(u8::is_ascii_alphanumeric) || ext.iter().all(u8::is_ascii_digit) {
            continue;
        }
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// How `count by field` reads one run's lines.
struct FieldOf {
    how: FieldHow,
    /// a csv or tsv file read by column name: its line 1 is the header, not a row
    header: bool,
}

enum FieldHow {
    /// a csv or tsv column: the separator, the column (from 0)
    Column(u8, usize),
    /// a JSON line's field: the parts of its path (repo.name)
    Json(Vec<String>),
    /// the value after name= or name: (name: value)
    Pair(Vec<u8>),
    /// a word by number (from 0), the line split at spaces
    Word(usize),
}

/// How run `k`'s lines give `spec`: by the run's format (a csv or tsv file's columns, JSON
/// lines' fields), else a name=value pair or a word.
fn field_of(sc: &Scope, k: usize, spec: &str) -> Result<FieldOf, String> {
    let (pi, r) = &sc.runs[k];
    let format = r.s("format").and_then(crate::formats::Format::from_name);
    let number = spec.parse::<usize>().ok().filter(|&n| n >= 1);
    let sep = match format {
        Some(crate::formats::Format::Csv) => Some(b','),
        Some(crate::formats::Format::Tsv) => Some(b'\t'),
        _ => None,
    };
    if let Some(sep) = sep {
        if let Some(n) = number {
            return Ok(FieldOf { how: FieldHow::Column(sep, n - 1), header: false });
        }
        // the column named in the header (line 1), any case
        let path = sc.projects[*pi].dir.join(store::run_file(r.id()));
        let got = crate::fetch::fetch(&path, &[(1, 1)], 1).map_err(|e| format!("its header cannot be read: {e}"))?;
        let head = got.into_iter().next().map(|(_, b)| b).unwrap_or_default();
        let cols: Vec<String> = crate::formats::split_row(clean_end(&head), sep).iter().map(|c| String::from_utf8_lossy(&crate::formats::unquote(c)).into_owned()).collect();
        return match cols.iter().position(|c| c.eq_ignore_ascii_case(spec)) {
            Some(i) => Ok(FieldOf { how: FieldHow::Column(sep, i), header: true }),
            None => Err(format!("no column {} in its header line (its columns: {})", Json::Str(spec.to_string()), key_text(cols.join(", ").as_bytes(), 300))),
        };
    }
    if format.is_some_and(crate::formats::Format::is_json) {
        if number.is_some() {
            return Err(format!("JSON lines: name a field (type, repo.name), not {spec}"));
        }
        return Ok(FieldOf { how: FieldHow::Json(spec.split('.').map(str::to_string).collect()), header: false });
    }
    Ok(match number {
        Some(n) => FieldOf { how: FieldHow::Word(n - 1), header: false },
        None => FieldOf { how: FieldHow::Pair(spec.as_bytes().to_vec()), header: false },
    })
}

impl FieldOf {
    /// The value a line gives, if any (a csv field without its quotes, a JSON string without
    /// its quotes, other JSON values as JSON).
    fn value(&self, line: &[u8]) -> Option<Vec<u8>> {
        let line = clean_end(line);
        match &self.how {
            FieldHow::Column(sep, i) => crate::formats::split_row(line, *sep).get(*i).map(|f| crate::formats::unquote(f).into_owned()),
            FieldHow::Word(i) => line.split(u8::is_ascii_whitespace).filter(|w| !w.is_empty()).nth(*i).map(<[u8]>::to_vec),
            FieldHow::Json(parts) => {
                let start = line.iter().position(|&c| c == b'{')?;
                let j = crate::json::parse(&line[start..]).ok()?;
                let mut v = &j;
                for p in parts {
                    v = v.get(p)?;
                }
                Some(match v {
                    Json::Str(s) => s.as_bytes().to_vec(),
                    other => other.to_string().into_bytes(),
                })
            }
            FieldHow::Pair(name) => pair_value(line, name),
        }
    }
}

/// The value after `name=` or `name:` (spaces after the colon skipped) in a line, where name
/// is a word of its own: up to a space or one of , ; & ) ] }, without quotes around it.
fn pair_value(line: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    let mut from = 0;
    while let Some(i) = line[from..].windows(name.len()).position(|w| w == name).map(|i| i + from) {
        from = i + 1;
        if i > 0 && (line[i - 1].is_ascii_alphanumeric() || line[i - 1] == b'_' || line[i - 1] == b'-' || line[i - 1] == b'.') {
            continue;
        }
        let mut j = i + name.len();
        if j >= line.len() || (line[j] != b'=' && line[j] != b':') {
            continue;
        }
        // (spaces after "name:" are skipped; "name= " is an empty value)
        let colon = line[j] == b':';
        j += 1;
        while colon && j < line.len() && line[j] == b' ' {
            j += 1;
        }
        let quoted = j < line.len() && (line[j] == b'"' || line[j] == b'\'');
        let v = if quoted {
            let q = line[j];
            let end = line[j + 1..].iter().position(|&c| c == q).map_or(line.len(), |e| j + 1 + e);
            &line[j + 1..end]
        } else {
            let end = line[j..].iter().position(|&c| c.is_ascii_whitespace() || b",;&)]}".contains(&c)).map_or(line.len(), |e| j + e);
            &line[j..end]
        };
        if !v.is_empty() {
            return Some(v.to_vec());
        }
    }
    None
}

/// A value as a table row's label: cut at 200 bytes, "(empty)" for an empty one.
fn value_label(v: &[u8]) -> String {
    if v.is_empty() {
        return "(empty)".into();
    }
    key_text(v, 200)
}

fn count_table(ctx: &Ctx, a: &Args, stop: &AtomicBool, by: &str) -> Result<String, String> {
    // (by ip, field or path: values read from the lines that hold the text, or from every line
    // when there is none; the token tests of 2026-11-23 asked which country, which event type,
    // which file)
    let line_values = matches!(by, "ip" | "field" | "path");
    if by == "field" && a.s("field").is_none() {
        return Err("count by field needs `field`: a column of a csv or tsv file (its name in the first line, or its number from 1), a field of JSON lines (type, repo.name), name for the value after name= or name: in a line, or a word's number".into());
    }
    let sc = select(ctx, a, true)?;
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    let mut skip = 0usize;
    let mut scan_cursor: Option<&str> = None;
    if let Some(c) = a.s("cursor") {
        if let Some(n) = c.strip_prefix('#') {
            skip = n.parse().map_err(|_| format!("'{c}' is not a cursor (#40 goes on from row 40 of the table)"))?;
        } else if a.s("text").is_some() || line_values {
            scan_cursor = Some(c);
        } else {
            return Err(format!("'{c}' is not a cursor of this table (#40 goes on from row 40)"));
        }
    }
    let pid_of = |k: usize| &sc.projects[sc.runs[k].0].pid;
    let rref = |k: usize| run_ref(pid_of(k), sc.runs[k].1.id());
    // rows: (label, count, what follows the label)
    let mut rows: Vec<(String, u64, String)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut stopped: Option<String> = None;
    let mut header_skipped = 0u64;
    let head = match a.s("text").or(if line_values { Some("") } else { None }) {
        None => {
            let mut ready = Vec::new();
            let mut skipped = Vec::new();
            for (k, (pi, r)) in sc.runs.iter().enumerate() {
                if crate::guard::no_log(r) {
                    // (a file kept as it is, or not read: no catalog)
                    continue;
                }
                if r.state() == "ready" {
                    ready.push(k);
                } else {
                    // (a run in progress too: its catalog is made once it is saved)
                    skipped.push((k, not_ready(&sc.projects[*pi], r, "counted once it is saved")));
                }
            }
            let (cats, made, errs) = load_catalogs(&sc, &ready, stop);
            for (k, e) in errs.iter().take(20) {
                notes.push(format!("{}: {e}", rref(*k)));
            }
            if !skipped.is_empty() {
                let s: Vec<String> = skipped.iter().take(10).map(|(k, why)| format!("{} ({why})", rref(*k))).collect();
                notes.push(format!("not counted: {}", s.join(", ")));
            }
            notes.extend(made_line(&sc, &made));
            let counted: Vec<usize> = ready.iter().copied().filter(|k| cats.contains_key(k)).collect();
            let (mut records, mut lines) = (0u64, 0u64);
            for k in &counted {
                records += cats[k].records;
                lines += cats[k].lines;
            }
            match by {
                "key" => {
                    // (text, records, lines, the first record's run and line: runs oldest first)
                    let mut at: HashMap<Vec<u8>, usize> = HashMap::new();
                    let mut v: Vec<(Vec<u8>, u64, u64, usize, u64)> = Vec::new();
                    for &k in counted.iter().rev() {
                        for (text, rec, lin, first, _) in cats[&k].by_text() {
                            match at.get(text) {
                                Some(&i) => {
                                    v[i].1 += rec;
                                    v[i].2 += lin;
                                }
                                None => {
                                    at.insert(text.to_vec(), v.len());
                                    v.push((text.to_vec(), rec, lin, k, first));
                                }
                            }
                        }
                    }
                    v.sort_by_key(|x| std::cmp::Reverse(x.1));
                    for (text, rec, lin, k, first) in v {
                        let more = if lin != rec { format!("{} lines; ", group(lin)) } else { String::new() };
                        rows.push((key_text(&text, 300), rec, format!("  ({more}first {})", line_ref(pid_of(k), sc.runs[k].1.id(), first))));
                    }
                }
                "hour" => {
                    let mut m: BTreeMap<i64, u64> = BTreeMap::new();
                    let mut none = 0u64;
                    for k in &counted {
                        let c = &cats[k];
                        let mut timed = 0;
                        for key in &c.keys {
                            for &(h, n) in &key.hours {
                                *m.entry(h).or_insert(0) += n;
                                timed += n;
                            }
                        }
                        none += c.records - timed;
                    }
                    rows.extend(m.into_iter().map(|(h, n)| (hour_text(h), n, String::new())));
                    if none > 0 {
                        rows.push(("no time".into(), none, String::new()));
                    }
                }
                _ => {
                    let mut lv = [(0u64, 0u64); 5];
                    for k in &counted {
                        for key in &cats[k].keys {
                            let e = &mut lv[catalog::level_code(key.level)];
                            e.0 += key.records;
                            e.1 += key.lines;
                        }
                    }
                    for c in (0..5).rev() {
                        let (rec, lin) = lv[c];
                        if rec > 0 {
                            let name = catalog::level_of(c).map_or("", |l| l.name());
                            let more = if lin != rec { format!("  ({} lines)", group(lin)) } else { String::new() };
                            rows.push((if name.is_empty() { "no level".into() } else { name.to_string() }, rec, more));
                        }
                    }
                }
            }
            match one_ref(&sc) {
                Some(o) if counted.len() == 1 => format!("{} ({}) in {o}, by {by}: {}", plural(records, "record"), plural(lines, "line"), plural(rows.len() as u64, "row")),
                _ => format!("{} ({}) in {} ({}), by {by}: {}", plural(records, "record"), plural(lines, "line"), plural(counted.len() as u64, "run"), sc.desc, plural(rows.len() as u64, "row")),
            }
        }
        Some(t) => {
            let mut q = Query::new(t.as_bytes(), a.b("icase"), a.b("whole"));
            q.with_keys = by == "key";
            let want = Want { times: by == "hour", levels: by == "level" };
            let time_limit = Duration::from_millis(a.i("time_limit_ms", 10_000) as u64);
            // (by ip, field or path: the matching lines are read, up to IP_LINES of them)
            let (keep_lines, keep) = if line_values { (IP_LINES, Keep { lines: IP_LINES, bytes: IP_BYTES, line_bytes: 1 << 16, groups: 0 }) } else { (0, Keep::NONE) };
            let f = run_search(&sc, &q, scan_cursor, keep_lines, keep, want, time_limit, stop)?;
            for (k, e) in f.errors.iter().take(20) {
                notes.push(format!("{}: {e}", rref(*k)));
            }
            if !f.skipped.is_empty() {
                let s: Vec<String> = f.skipped.iter().take(10).map(|(k, why)| format!("{} ({why})", rref(*k))).collect();
                notes.push(format!("not counted: {}", s.join(", ")));
            }
            notes.extend(live_lines(&sc, &f, "counted"));
            notes.extend(made_line(&sc, &f.made));
            match by {
                "key" => {
                    for (key, n, k, line) in key_totals(&f) {
                        rows.push((key_text(key, 300), n, format!("  (first {})", line_ref(pid_of(k), sc.runs[k].1.id(), line))));
                    }
                }
                "ip" => {
                    // each address once per line it is in; the most lines first, then by text
                    let mut m: HashMap<Vec<u8>, u64> = HashMap::new();
                    for (_, h) in &f.cand {
                        let mut seen: Vec<&[u8]> = Vec::new();
                        for ip in ipv4s(&h.bytes) {
                            if !seen.contains(&ip) {
                                seen.push(ip);
                                *m.entry(ip.to_vec()).or_default() += 1;
                            }
                        }
                    }
                    let mut v: Vec<(Vec<u8>, u64)> = m.into_iter().collect();
                    v.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
                    rows.extend(v.into_iter().map(|(ip, n)| (String::from_utf8_lossy(&ip).into_owned(), n, String::new())));
                    if (f.cand.len() as u64) < f.total {
                        notes.push(format!("the addresses of the first {} of these lines (count with since and until for the rest)", group(f.cand.len() as u64)));
                    }
                }
                "field" | "path" => {
                    let spec = a.s("field").unwrap_or("");
                    // (a table's header line is no row: the heading counts rows then)
                    let mut how: HashMap<usize, Result<FieldOf, String>> = HashMap::new();
                    let mut m: HashMap<Vec<u8>, u64> = HashMap::new();
                    let (mut without, mut header_rows) = (0u64, 0u64);
                    for (k, h) in &f.cand {
                        if by == "path" {
                            for p in paths(&h.bytes) {
                                *m.entry(p.to_vec()).or_default() += 1;
                            }
                            continue;
                        }
                        let fo = how.entry(*k).or_insert_with(|| field_of(&sc, *k, spec));
                        match fo {
                            Ok(fo) => {
                                if fo.header && h.line == 1 {
                                    header_rows += 1;
                                    continue;
                                }
                                match fo.value(&h.bytes) {
                                    Some(v) => *m.entry(v).or_default() += 1,
                                    None => without += 1,
                                }
                            }
                            Err(_) => without += 1,
                        }
                    }
                    let mut v: Vec<(Vec<u8>, u64)> = m.into_iter().collect();
                    v.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
                    rows.extend(v.into_iter().map(|(val, n)| (value_label(&val), n, String::new())));
                    let mut errs: Vec<(usize, String)> = how.into_iter().filter_map(|(k, r)| r.err().map(|e| (k, e))).collect();
                    errs.sort();
                    for (k, e) in errs.iter().take(10) {
                        notes.push(format!("{}: {e}", rref(*k)));
                    }
                    if without > 0 {
                        let what = if by == "path" { "no file path".to_string() } else { format!("no value of {}", Json::Str(spec.to_string())) };
                        notes.push(format!("{} with {what}", plural(without, "line")));
                    }
                    header_skipped = header_rows;
                    if (f.cand.len() as u64) < f.total {
                        notes.push(format!("the values of the first {} of these lines (count with since and until for the rest)", group(f.cand.len() as u64)));
                    }
                }
                "hour" => {
                    let (hours, none) = hour_totals(&f);
                    rows.extend(hours.into_iter().map(|(h, n)| (hour_text(h), n, String::new())));
                    if none > 0 {
                        rows.push(("no time".into(), none, String::new()));
                    }
                }
                _ => {
                    let mut lv = [0u64; 5];
                    for (_, _, h) in &f.runs {
                        for (c, n) in h.levels.iter().enumerate() {
                            lv[c] += n;
                        }
                    }
                    for c in (0..5).rev() {
                        if lv[c] > 0 {
                            let name = catalog::level_of(c).map_or("", |l| l.name());
                            rows.push((if name.is_empty() { "no level".into() } else { name.to_string() }, lv[c], String::new()));
                        }
                    }
                }
            }
            if let Some((k, line)) = f.stopped {
                stopped = Some(line_ref(pid_of(k), sc.runs[k].1.id(), line));
            }
            let runs_with = f.counts.iter().filter(|c| c.1 > 0).count();
            let from = scan_cursor.map(|c| format!(", from {c}")).unwrap_or_default();
            let least = if f.stopped.is_some() { "at least " } else { "" };
            let so_far = if f.stopped.is_some() { " searched so far" } else { "" };
            let by_text = if by == "field" { format!("field {}", Json::Str(a.s("field").unwrap_or("").to_string())) } else { by.to_string() };
            let counted_lines = if header_skipped > 0 { plural(f.total - header_skipped, "row") } else { plural(f.total, "line") };
            let from_paren = scan_cursor.map(|c| format!(" (from {c})")).unwrap_or_default();
            if let (Some(o), None) = (one_ref(&sc), f.stopped) {
                if q.text.is_empty() {
                    format!("{counted_lines} in {o}{from_paren}, by {by_text}: {}", plural(rows.len() as u64, "value"))
                } else {
                    format!("{} {} {} in {o}{from_paren}, by {by_text}: {}", plural(f.total, "line"), holds(f.total), what_text(&q), plural(rows.len() as u64, "row"))
                }
            } else if q.text.is_empty() {
                format!("{least}{} in {} of {}{so_far} ({}{from}), by {by_text}: {}", plural(f.total, "line"), runs_with, plural(f.searched as u64, "run"), sc.desc, plural(rows.len() as u64, "row"))
            } else {
                format!(
                    "{least}{} {} {} in {} of {}{so_far} ({}{from}), by {by_text}: {}",
                    plural(f.total, "line"),
                    holds(f.total),
                    what_text(&q),
                    runs_with,
                    plural(f.searched as u64, "run"),
                    sc.desc,
                    plural(rows.len() as u64, "row")
                )
            }
        }
    };
    ans.must(&head);
    for n in &notes {
        ans.must(n);
    }
    let width = rows.iter().map(|r| group(r.1).len()).max().unwrap_or(1);
    // (the first rows only, by default: the token test of 2026-11-23; the cursor gives the rest)
    let max_rows = a.i("limit", if by == "hour" { 24 } else { 10 }) as usize;
    let mut next_row = None;
    for (i, (label, n, more)) in rows.iter().enumerate().skip(skip) {
        if i - skip >= max_rows || !ans.line(&format!("{:>width$}  {label}{more}", group(*n))) {
            next_row = Some(i);
            break;
        }
    }
    let last = match (next_row, stopped) {
        (None, None) => "all shown".to_string(),
        (Some(i), None) => format!("left out: {} of {} rows (smaller counts); next: the same call with \"cursor\":\"#{i}\"", group((rows.len() - i) as u64), group(rows.len() as u64)),
        (rest, Some(c)) => {
            let rows_note = rest.map(|i| format!("{} rows not shown (a bigger max_tokens shows them), and ", group((rows.len() - i) as u64))).unwrap_or_default();
            format!("left out: {rows_note}not counted yet (time limit): from {c} on. Next: {}", next_call("count", &a.with_cursor(&c)))
        }
    };
    Ok(ans.finish(&last))
}

// ---------------------------------------------------------------- lines

/// Base64 (RFC 4648, with padding).
pub fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// One byte as an answer names it: 'x', or \xHH.
fn byte_name(b: u8) -> String {
    if (0x20..0x7f).contains(&b) && b != b'\'' { format!("'{}'", b as char) } else { format!("'\\x{b:02x}'") }
}

/// Files outline reads: at most this many bytes.
const OUTLINE_BYTES: u64 = 64 << 20;

/// `outline`: a file's definitions or headings with the lines each spans (outline.rs).
fn tool_outline(ctx: &Ctx, a: &Args) -> Result<String, String> {
    let (name, text, kind) = match (a.s("path"), a.s("ref")) {
        (Some(_), Some(_)) => return Err("give `path` or `ref`, not both".into()),
        (None, None) => return Err("give `path` (a file in this project's folder) or `ref` (a saved run, r42)".into()),
        (Some(p), None) => {
            // only files in this project's folder (its root, the folder the server serves)
            let root = std::fs::canonicalize(&ctx.root).map_err(|e| format!("this project's folder cannot be read: {e}"))?;
            let given = PathBuf::from(p);
            let path = std::fs::canonicalize(if given.is_absolute() { given } else { root.join(given) }).map_err(|e| format!("{p}: {e}"))?;
            if !path.starts_with(&root) {
                return Err(format!("{p} is not in this project's folder ({}); outline reads files there only", root.display()));
            }
            let meta = std::fs::metadata(&path).map_err(|e| format!("{p}: {e}"))?;
            if !meta.is_file() {
                return Err(format!("{p} is not a file"));
            }
            if meta.len() > OUTLINE_BYTES {
                return Err(format!("{p} has {} bytes, over {} (save it with `fewer add` and outline its run)", group(meta.len()), group(OUTLINE_BYTES)));
            }
            let text = std::fs::read(&path).map_err(|e| format!("{p}: {e}"))?;
            (path.strip_prefix(&root).unwrap_or(&path).display().to_string(), text, None)
        }
        (None, Some(rf)) => {
            let (pid, id) = parse_run_ref(rf).ok_or_else(|| format!("'{rf}' is not a run reference (r42, fewer-3fa2/r42)"))?;
            let home = ctx.home.clone()?;
            let projects = projects(ctx, &home, pid.or(a.s("project")))?;
            let p = projects.first().ok_or("no project")?;
            let runs = store::read_runs(&p.dir);
            let r = runs.iter().find(|r| r.id() == id).ok_or_else(|| format!("no run r{id} in {}", p.pid))?;
            let n = r.i("lines").unwrap_or(0) as u64;
            if r.state() != "ready" || n == 0 {
                return Err(format!("{} has no saved lines to outline yet", run_ref(&p.pid, id)));
            }
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            let got = fetch::fetch(&p.dir.join(store::run_file(id)), &[(1, n)], threads).map_err(|e| format!("{}: {e}", run_ref(&p.pid, id)))?;
            let mut text = Vec::new();
            for (_, l) in got {
                text.extend_from_slice(clean_end(&l));
                text.push(b'\n');
            }
            // (a run's format names a table or JSON lines whatever its file is called)
            let kind = match r.s("format") {
                Some("csv") => Some(crate::outline::Kind::Table(b',')),
                Some("tsv") => Some(crate::outline::Kind::Table(b'\t')),
                Some("json") => Some(crate::outline::Kind::JsonLines),
                _ => None,
            };
            let file = matches!(r.kind(), "add" | "kept" | "not-read") && r.0.get("cmd").is_none();
            (format!("{} ({})", run_ref(&p.pid, id), if file { short_path(&what(r)) } else { what(r) }), text, kind)
        }
    };
    let kind = kind.unwrap_or_else(|| crate::outline::kind_of(&name));
    let depth = a.i("depth", 1) as usize;
    let lines = text.split(|&c| c == b'\n').count() as u64 - u64::from(text.ends_with(b"\n"));
    // a table's columns, JSON lines' fields
    let fields = match kind {
        crate::outline::Kind::Table(sep) => Some(crate::outline::table(&text, sep, a.b("values"))),
        crate::outline::Kind::JsonLines => Some(crate::outline::json_fields(&text, a.b("values"))),
        _ => None,
    };
    if let Some(rows) = fields {
        let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
        ans.must(&format!("outline of {name} ({})", plural(lines, "line")));
        let mut shown = 0;
        for r in &rows {
            if !ans.line(r) {
                break;
            }
            shown += 1;
        }
        return Ok(ans.finish(if shown == rows.len() { "all shown" } else { "left out: the rest of its fields (a bigger max_tokens shows them)" }));
    }
    // (with `line`: the items that hold it, whatever their depth; with `name`: the items whose
    // line holds it; both show their doc comments, which say what an item does)
    let at = a.0.get("line").and_then(Json::int).map(|v| v as u64);
    let name_is = a.s("name");
    let items: Vec<crate::outline::Item> = crate::outline::outline(&text, kind)
        .into_iter()
        .filter(|i| match (at, name_is) {
            (Some(l), _) => i.line <= l && l <= i.end,
            (None, Some(n)) => i.text.contains(n),
            (None, None) => i.depth <= depth,
        })
        .collect();
    let with_docs = a.0.get("docs").and_then(Json::bool).unwrap_or(at.is_some() || name_is.is_some());
    let mut skip = 0usize;
    if let Some(c) = a.s("cursor") {
        skip = c.strip_prefix('#').and_then(|n| n.parse().ok()).ok_or_else(|| format!("'{c}' is not a cursor of an outline (#40 goes on from item 40)"))?;
    }
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    let what_items = if kind == crate::outline::Kind::Markdown { "heading" } else { "item" };
    ans.must(&format!("outline of {name} ({}): {} to depth {depth}, each with the lines it spans", plural(lines, "line"), plural(items.len() as u64, what_items)));
    if items.is_empty() {
        return Ok(ans.finish("no definitions or headings found (read its start with a small range, or search it)"));
    }
    let limit = a.i("limit", 100) as usize;
    let mut next = None;
    for (i, it) in items.iter().enumerate().skip(skip) {
        let (t, _) = crate::clean::cut(&it.text, 160);
        let mut row = format!("{}{}-{} {t}", "  ".repeat(it.depth), it.line, it.end);
        if with_docs {
            for d in crate::outline::docs(&text, it.line) {
                let (d, _) = crate::clean::cut(&d, 160);
                row.push_str(&format!("\n{}  {d}", "  ".repeat(it.depth)));
            }
        }
        if i - skip >= limit || !ans.line(&row) {
            next = Some(i);
            break;
        }
    }
    let last = match next {
        None => "all shown".to_string(),
        Some(i) => format!("left out: {} of {} {}s. Next: {}", group((items.len() - i) as u64), group(items.len() as u64), what_items, next_call("outline", &a.with_cursor(&format!("#{i}")))),
    };
    Ok(ans.finish(&last))
}

fn tool_lines(ctx: &Ctx, a: &Args) -> Result<String, String> {
    let home = ctx.home.clone()?;
    let rs = a.s("ref").unwrap_or("");
    let (pid, id, first, last) = parse_line_ref(rs).ok_or_else(|| format!("'{rs}' is not a line reference (fewer-3fa2/r42:1201 or fewer-3fa2/r42:1201-1210)"))?;
    let project = match pid {
        Some(p) => projects(ctx, &home, Some(p))?,
        None => projects(ctx, &home, a.s("project"))?,
    };
    let p = &project[0];
    let runs = store::read_runs(&p.dir);
    let r = runs.iter().find(|r| r.id() == id).ok_or_else(|| {
        // ids are never used twice: one below the project's next id that is not in its run
        // table was forgotten by the person (`memory forget`, or the limit they set)
        if id < next_id(&p.dir) {
            format!("{} was forgotten: the person removed it from the memory (`memory forget`, or the limit they set)", run_ref(&p.pid, id))
        } else {
            format!("no run r{id} in {} (its runs: {})", p.pid, run_span(&runs))
        }
    })?;
    let text = searchable(p, r).map_err(|e| format!("{}: {e}", run_ref(&p.pid, id)))?;
    let total = r.i("lines").map(|l| l as u64);
    if let Some(n) = total
        && first > n
    {
        return Err(format!("{} has {} lines; there is no line {first}", run_ref(&p.pid, id), group(n)));
    }
    // (5 lines of context around one line; none around a range, which is what was asked:
    // paging r2:1-40 then r2:41-80 shows each line once)
    let around = if last > first { 0 } else { 5 };
    let before = a.i("before", around) as u64;
    let after = a.i("after", around) as u64;
    let line_max = a.i("line_max", 2000) as usize;
    let lo = first.saturating_sub(before).max(1);
    let hi = last.saturating_add(after);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let got = match &text {
        Text::Archive(path) => fetch::fetch(path, &[(lo, hi)], threads),
        Text::Capture(cap, size) => crate::capture::lines(cap, *size, &[(lo, hi)]),
    }
    .map_err(|e| format!("{}: {e}", run_ref(&p.pid, id)))?;
    let shown_hi = got.last().map_or(lo, |l| l.0);
    let mut ans = Answer::new(a.i("max_tokens", 3000) as usize);
    // a run in progress: its output so far (its line count comes with its end)
    let of = match (&text, total) {
        (Text::Capture(..), None) => "its output so far".to_string(),
        (_, n) => format!("{} lines", n.map_or("?".into(), group)),
    };
    let mut head = format!("{}:{lo}-{shown_hi} of {of} ({})", run_ref(&p.pid, id), tags(r));
    if let Text::Capture(cap, _) = &text {
        head.push_str(&format!("; r{id} {}", Live::of(r, cap).is()));
    }
    ans.must(&head);
    let asked: Vec<&(u64, Vec<u8>)> = got.iter().filter(|l| l.0 >= first && l.0 <= last).collect();
    if asked.is_empty() {
        return Err(format!("{} has no line {first}", run_ref(&p.pid, id)));
    }
    if let Some(e) = a.s("expect") {
        let mut have = Vec::new();
        for (k, l) in asked.iter().enumerate() {
            if k > 0 {
                have.push(b'\n');
            }
            have.extend_from_slice(crate::clean::without_end(&l.1));
        }
        let want = e.as_bytes();
        let d = (0..want.len().max(have.len())).find(|&i| want.get(i) != have.get(i));
        let s = match d {
            None => "expect: same bytes".to_string(),
            Some(i) => format!(
                "expect: differs at byte {}: expected {}, found {}",
                i + 1,
                want.get(i).map_or("the end".into(), |&b| byte_name(b)),
                have.get(i).map_or("the end".into(), |&b| byte_name(b))
            ),
        };
        ans.must(&s);
    }
    if a.b("base64") {
        let mut raw = Vec::new();
        for l in &asked {
            raw.extend_from_slice(&l.1);
        }
        let b = base64(&raw);
        let s = format!("base64 of lines {first}-{} ({} bytes, line ends included): {b}", asked.last().map_or(first, |l| l.0), group(raw.len() as u64));
        if !ans.line(&s) {
            ans.must(&format!("base64 of lines {first}-{}: {} bytes, over the size cap: ask for fewer lines or a bigger max_tokens", asked.last().map_or(first, |l| l.0), group(raw.len() as u64)));
        }
    }
    // the asked lines first, then the lines around them, nearest first, while they fit
    let raw = a.b("raw");
    let render = |l: &(u64, Vec<u8>), mark: bool| -> (String, bool, bool) {
        let (t, cl, cut) = if raw {
            let (s, _) = crate::clean::escape(&l.1, true);
            let (t, n) = crate::clean::cut(&s, line_max);
            if n > 0 { (format!("{t}…(+{} bytes)", group(n as u64)), false, true) } else { (s, false, false) }
        } else {
            view_line(r, clean_end(&l.1), clean_end(&l.1).len(), line_max)
        };
        (format!("{}{} {t}", if mark { "> " } else { "  " }, l.0), cl, cut)
    };
    let mut keep = vec![false; got.len()];
    let mut used = 0usize;
    let room = |used: usize, ans: &Answer, n: usize| ans.fits(used + n);
    let order: Vec<usize> = {
        let ai: Vec<usize> = (0..got.len()).filter(|&i| got[i].0 >= first && got[i].0 <= last).collect();
        let (a0, a1) = (ai[0], *ai.last().unwrap());
        let mut o = ai.clone();
        let (mut b, mut f) = (a0 as isize - 1, a1 + 1);
        while b >= 0 || f < got.len() {
            if f < got.len() {
                o.push(f);
                f += 1;
            }
            if b >= 0 {
                o.push(b as usize);
                b -= 1;
            }
        }
        o
    };
    let mut rendered: Vec<Option<(String, bool, bool)>> = vec![None; got.len()];
    for &i in &order {
        let mark = got[i].0 >= first && got[i].0 <= last;
        let r = render(&got[i], mark && (first != last || before + after > 0));
        if !room(used, &ans, r.0.len() + 1) {
            break;
        }
        used += r.0.len() + 1;
        keep[i] = true;
        rendered[i] = Some(r);
    }
    let mut cut_lines = 0;
    for (i, r) in rendered.iter().enumerate() {
        if let Some((t, cl, cut)) = r {
            ans.line(t);
            if *cl {
                ans.cleaned(&line_ref(&p.pid, id, got[i].0));
            }
            cut_lines += *cut as usize;
        }
    }
    // left out: runs of lines not kept
    let mut gaps: Vec<(u64, u64)> = Vec::new();
    for (i, l) in got.iter().enumerate() {
        if !keep[i] {
            match gaps.last_mut() {
                Some(g) if g.1 + 1 == l.0 => g.1 = l.0,
                _ => gaps.push((l.0, l.0)),
            }
        }
    }
    let mut last_line = if gaps.is_empty() {
        "all shown".to_string()
    } else {
        let n: u64 = gaps.iter().map(|g| g.1 - g.0 + 1).sum();
        let list: Vec<String> = gaps.iter().map(|g| if g.0 == g.1 { g.0.to_string() } else { format!("{}-{}", g.0, g.1) }).collect();
        let g = gaps[0];
        let mut next = a.0.clone();
        next.set("ref", format!("{}:{}-{}", run_ref(&p.pid, id), g.0, g.1).into());
        next.set("before", Json::from(0u64));
        next.set("after", Json::from(0u64));
        next.remove("expect");
        format!("left out: {} lines over the size cap ({}). Next: {}", group(n), list.join(", "), next_call("lines", &next))
    };
    // (all of the asked lines shown but not the whole run: which, in a few words, as "all shown"
    // alone was read as the whole run)
    if gaps.is_empty() {
        let span = if lo == shown_hi { format!("line {lo}") } else { format!("lines {lo}-{shown_hi}") };
        match total {
            Some(n) if lo > 1 || shown_hi < n => last_line = format!("all shown ({span} of {})", group(n)),
            None => last_line = format!("all shown ({span} of its output so far)"),
            _ => {}
        }
    }
    if cut_lines > 0 {
        last_line.push_str(&format!("; {cut_lines} lines cut at {} bytes (line_max)", group(line_max as u64)));
    }
    Ok(ans.finish(&last_line))
}

fn run_span(runs: &[RunRec]) -> String {
    match (runs.first(), runs.last()) {
        (Some(a), Some(b)) => format!("r{} to r{}", a.id(), b.id()),
        _ => "none".into(),
    }
}

/// A project's next run id, from its `project` file (1 when it cannot be read).
fn next_id(dir: &Path) -> u64 {
    let text = std::fs::read_to_string(dir.join("project")).unwrap_or_default();
    store::read_settings(&text).iter().find(|(k, _)| k == "next").and_then(|(_, v)| v.parse().ok()).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compact line made whole again, as an answer's legend tells the reader to.
    fn rebuild(prev: &str, c: &str) -> String {
        if let Some(n) = c.strip_prefix("= ") {
            assert!(n.parse::<u64>().is_ok());
            return prev.to_string();
        }
        let words: Vec<&str> = c.splitn(3, ' ').nth(2).unwrap().split(' ').collect();
        let pw: Vec<&str> = prev.split(' ').collect();
        assert!(words.len() <= pw.len());
        pw.iter().enumerate().map(|(i, p)| match words.get(i) { Some(&"·") | None => *p, Some(w) => *w }).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn compact_lines_rebuild_exactly() {
        let prev = "081109 203639 169 INFO dfs.DataNode: Received block blk_1 of size 67108864 from /10.251.111.209";
        let recent = vec![(6971, prev.to_string())];
        // one word changed in the middle: the end that is the same is left out
        let cur = "081109 203639 173 INFO dfs.DataNode: Received block blk_1 of size 67108864 from /10.251.111.209";
        let c = compact_line(&recent, cur).unwrap();
        assert_eq!(c, "≈ 6971 · · 173");
        assert_eq!(rebuild(prev, &c), cur);
        // the last word changed
        let cur = "081109 203639 169 INFO dfs.DataNode: Received block blk_1 of size 67108864 from /10.251.203.149";
        let c = compact_line(&recent, cur).unwrap();
        assert!(c.ends_with(" /10.251.203.149"));
        assert_eq!(rebuild(prev, &c), cur);
        // the same line
        assert_eq!(compact_line(&recent, prev).as_deref(), Some("= 6971"));
        // an empty word (two spaces) in both lines: a "·" like any other
        let prev = "alpha bravo charlie delta echo foxtrot golf hotel  india juliet kilo lima";
        let cur = "alpha bravo charlie delta echo foxtrot golf HOTEL  india juliet kilo lima";
        let c = compact_line(&[(1, prev.to_string())], cur).unwrap();
        assert_eq!(c, "≈ 1 · · · · · · · HOTEL");
        assert_eq!(rebuild(prev, &c), cur);
        // a word that became empty: the "·" after it is kept, so the shown line does not end
        // in a space
        let prev2 = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
        let cur2 = "alpha bravo charlie delta echo foxtrot golf  india juliet kilo lima";
        let c = compact_line(&[(1, prev2.to_string())], cur2).unwrap();
        assert_eq!(c, "≈ 1 · · · · · · ·  ·");
        assert_eq!(rebuild(prev2, &c), cur2);
        // a line with a "·" of its own, or too many words changed: shown whole
        assert_eq!(compact_line(&[(1, prev.to_string())], "alpha bravo · delta echo foxtrot golf hotel  india juliet kilo lima"), None);
        assert_eq!(compact_line(&[(1, prev.to_string())], "A B C D E foxtrot golf hotel  india juliet kilo lima"), None);
    }

    #[test]
    fn this_projects_refs_lose_their_project() {
        assert_eq!(line_ref("proj-3ac9", 3, 2076), "proj-3ac9/r3:2076");
        {
            let _s = ShortRefs::new(Some("proj-3ac9".into()));
            assert_eq!(line_ref("proj-3ac9", 3, 2076), "r3:2076");
            assert_eq!(run_ref("proj-3ac9", 3), "r3");
            // another project's runs keep their project
            assert_eq!(line_ref("other-11aa", 2, 5), "other-11aa/r2:5");
        }
        assert_eq!(run_ref("proj-3ac9", 3), "proj-3ac9/r3");
    }

    #[test]
    fn file_paths_in_a_line() {
        let ps = |s: &str| paths(s.as_bytes()).iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect::<Vec<_>>();
        assert_eq!(ps("   --> src/../../src/engine/tests.rs:12:5"), ["src/../../src/engine/tests.rs"]);
        assert_eq!(ps("      at Object.<anonymous> (src/pages/__tests__/Login.test.tsx:40:7)"), ["src/pages/__tests__/Login.test.tsx"]);
        assert_eq!(ps(r#"  File "/usr/lib/python3.11/json/decoder.py", line 337, in decode"#), ["/usr/lib/python3.11/json/decoder.py"]);
        assert_eq!(ps("error[E0308]: mismatched types in src/main.rs and src/main.rs:3"), ["src/main.rs"]);
        // not paths: a URL, a fraction, a word with no folder, a version
        assert!(ps("see https://example.com/a/b.html 1/2.5 main.rs v1.2/3.4").is_empty());
    }

    #[test]
    fn field_values() {
        let v = |how: FieldHow, line: &str| FieldOf { how, header: false }.value(line.as_bytes()).map(|b| String::from_utf8_lossy(&b).into_owned());
        let row = r#"00AK,small_airport,"Lowell Field, the old one",450,NA,US,US-AK,Anchor Point,,,00AK,00AK,"59.947733, -151.692524""#;
        assert_eq!(v(FieldHow::Column(b',', 5), row).as_deref(), Some("US"));
        assert_eq!(v(FieldHow::Column(b',', 2), row).as_deref(), Some("Lowell Field, the old one"));
        assert_eq!(v(FieldHow::Column(b',', 40), row), None);
        let ev = r#"{"id":"1","type":"PushEvent","repo":{"name":"a/b","id":7},"public":true}"#;
        assert_eq!(v(FieldHow::Json(vec!["type".into()]), ev).as_deref(), Some("PushEvent"));
        assert_eq!(v(FieldHow::Json(vec!["repo".into(), "name".into()]), ev).as_deref(), Some("a/b"));
        assert_eq!(v(FieldHow::Json(vec!["repo".into(), "id".into()]), ev).as_deref(), Some("7"));
        assert_eq!(v(FieldHow::Json(vec!["public".into()]), ev).as_deref(), Some("true"));
        assert_eq!(v(FieldHow::Json(vec!["nope".into()]), ev), None);
        assert_eq!(v(FieldHow::Json(vec!["type".into()]), "not json"), None);
        let px = "[10.30 16:49:06] chrome.exe - proxy.cse.cuhk.edu.hk:5070 open through proxy";
        assert_eq!(v(FieldHow::Word(2), px).as_deref(), Some("chrome.exe"));
        let auth = "authentication failure; logname= uid=0 euid=0 tty=NODEVssh ruser= rhost=pc180-233.nttu.edu.tw  user=root";
        assert_eq!(v(FieldHow::Pair(b"rhost".to_vec()), auth).as_deref(), Some("pc180-233.nttu.edu.tw"));
        // (ruser= is empty: no value; "user" is not the end of "ruser")
        assert_eq!(v(FieldHow::Pair(b"ruser".to_vec()), auth), None);
        assert_eq!(v(FieldHow::Pair(b"user".to_vec()), auth).as_deref(), Some("root"));
        assert_eq!(v(FieldHow::Pair(b"status".to_vec()), "GET /v2/servers HTTP/1.1 status: 404 len: 1793").as_deref(), Some("404"));
        assert_eq!(v(FieldHow::Pair(b"msg".to_vec()), r#"level=info msg="cache warm" took=3ms"#).as_deref(), Some("cache warm"));
    }

    #[test]
    fn ip_addresses_in_a_line() {
        let ips = |s: &str| ipv4s(s.as_bytes()).iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect::<Vec<_>>();
        assert_eq!(ips("Failed password for root from 173.234.31.186 port 38926 ssh2"), ["173.234.31.186"]);
        assert_eq!(ips("src: /10.251.111.209:48129 dest: /10.251.111.209:50010"), ["10.251.111.209", "10.251.111.209"]);
        assert!(ips("version 1.2.3.4.5, 256.1.1.1, 1.2.3, a1.2.3.4, 1.2.3.4a, 12345.1.1.1").is_empty());
        assert_eq!(ips("(0.0.0.0) and 255.255.255.255."), ["0.0.0.0", "255.255.255.255"]);
    }

    #[test]
    fn refs_and_base64() {
        assert_eq!(parse_run_ref("r42"), Some((None, 42)));
        assert_eq!(parse_run_ref("fewer-3fa2/r42"), Some((Some("fewer-3fa2"), 42)));
        assert_eq!(parse_run_ref("r0"), None);
        assert_eq!(parse_run_ref("42"), None);
        assert_eq!(parse_line_ref("fewer-3fa2/r42:1201"), Some((Some("fewer-3fa2"), 42, 1201, 1201)));
        assert_eq!(parse_line_ref("r42:3-9"), Some((None, 42, 3, 9)));
        assert_eq!(parse_line_ref("r42:9-3"), None);
        assert_eq!(parse_line_ref("r42:0"), None);
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn schemas_and_checks() {
        let d = defs();
        let s = d.iter().find(|d| d.name == "search").unwrap();
        let ok = crate::json::parse(br#"{"text":"x","limit":5,"icase":true,"runs":["r1"]}"#).unwrap();
        assert!(check(s, &ok).is_ok());
        for (bad, why) in [
            (r#"{"txt":"x"}"#, "unknown field 'txt'"),
            (r#"{"text":""}"#, "field 'text' must be a string"),
            (r#"{"text":"x","limit":0}"#, "field 'limit' must be a whole number from 1 to 500"),
            (r#"{"text":"x","limit":"5"}"#, "field 'limit' must be a whole number"),
            (r#"{"text":"x","kind":"logz"}"#, "field 'kind' must be one of"),
            (r#"{"icase":true}"#, "field 'text' is required"),
            (r#"[1]"#, "arguments must be an object"),
        ] {
            let e = check(s, &crate::json::parse(bad.as_bytes()).unwrap()).unwrap_err();
            assert!(e.starts_with(why), "{bad}: {e}");
        }
        let l = list().to_string();
        assert!(l.contains("\"readOnlyHint\":true") && l.contains("\"additionalProperties\":false"));
    }
}
