//! The token savers in Claude Code's settings.json: install adds them at the developer's
//! yes, uninstall takes them away. What is added:
//! - a PostToolUse hook on Bash, `<fewer> hook post-bash`: after a build, test, lint or
//!   install command (or a log) with a long output, Claude reads its short view and the whole
//!   output is saved (hook.rs); the command itself runs as always, with the user's own approvals;
//! - a SessionStart hook, `<fewer> hook start`: the start card in new, resumed, cleared
//!   and compacted sessions;
//! - `autoCompactWindow` (the setting Claude Code's /autocompact writes): long conversations
//!   compacted earlier. Claude Code compacts at the window less 33,000 tokens (20,000 kept for
//!   the reply, 13,000 for the summary; its code, version 2.1.287), and takes 100,000 at least.
//!
//! Nothing the developer set is changed: an `autoCompactWindow` already there is kept (and said
//! so); a hook of ours already there is not added again (one from fewer in another
//! place is pointed at this one). Every other key stays as it was, numbers as written.
//! Uninstall takes away our hooks (wherever their fewer is) and the window only while it
//! is still ours. In development the settings file is only the one in the folder set
//! in FEWER_CLAUDE_DIR, never ~/.claude by itself.

use std::fs;
use std::io::Write;
use crate::portable::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::json::{self, Json};
use crate::sessions::Places;

/// The window install sets: Claude Code then compacts past about WINDOW - 33,000 tokens. With
/// the full start, 150k was the best on the sessions simulated; with a
/// smaller start and short summaries, 120k has room
/// again between a compaction's floor and the next one.
pub const WINDOW: u64 = 120_000;
/// Windows earlier versions set, taken back by uninstall as ours too.
const OLD_WINDOWS: [u64; 1] = [150_000];
pub const WINDOW_KEY: &str = "autoCompactWindow";
/// How far below the window Claude Code compacts (20,000 for the reply, 13,000 the summary).
pub const BELOW: u64 = 33_000;

/// Our two hooks: (event, matcher, the argument after `hook`).
const HOOKS: [(&str, Option<&str>, &str); 3] = [("PostToolUse", Some("Bash"), "post-bash"), ("SessionStart", None, "start"), ("PreCompact", None, "precompact")];

/// A word as a shell reads it back: quoted when it needs it. On Windows a Windows path (a
/// drive or a `\` in it) is written with `/` in double quotes, which Claude Code's two shells
/// for hooks there, cmd and Git Bash, read alike (Windows names never hold a `"`).
fn quoted(w: &str) -> String {
    #[cfg(windows)]
    if w.contains(':') || w.contains('\\') {
        return format!("\"{}\"", w.replace('\\', "/"));
    }
    let safe = !w.is_empty() && w.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-+@%:,=".contains(&c));
    if safe { w.to_string() } else { format!("'{}'", w.replace('\'', "'\\''")) }
}

/// A hook command of ours: "[FEWER_HOME=<store>] <program> hook <which>". Claude Code runs a
/// hook with its own environment, which has no FEWER_HOME (in development the store is found
/// only through it), so the store install was given is named in the command itself.
pub fn command(home: Option<&str>, me: &str, which: &str) -> String {
    match home {
        Some(h) => format!("FEWER_HOME={} {} hook {which}", quoted(h), quoted(me)),
        None => format!("{} hook {which}", quoted(me)),
    }
}

/// Whether a hook command is ours for `which`: fewer (in any folder, after settings
/// such as FEWER_HOME=...) with `hook <which>` and nothing else.
fn ours(cmd: &str, which: &str) -> bool {
    let Some(rest) = cmd.strip_suffix(&format!(" hook {which}")) else { return false };
    match &crate::hook::pipelines(rest)[..] {
        [pipe] if pipe.len() == 1 => crate::hook::program(&pipe[0]).is_some_and(|(name, args)| matches!(name, "fewer" | "fewer.exe") && args.is_empty()),
        _ => false,
    }
}

fn field<'a>(o: &'a mut Vec<(String, Json)>, key: &str, empty: Json) -> &'a mut Json {
    let i = match o.iter().position(|(k, _)| k == key) {
        Some(i) => i,
        None => {
            o.push((key.to_string(), empty));
            o.len() - 1
        }
    };
    &mut o[i].1
}

/// Our hook in an entry of a hooks list, as its command's place: (index in "hooks", command).
fn our_hook<'a>(entry: &'a Json, which: &str) -> Option<(usize, &'a str)> {
    let Some(Json::Arr(h)) = entry.get("hooks") else { return None };
    h.iter().enumerate().find_map(|(i, x)| x.get("command").and_then(Json::str).filter(|c| ours(c, which)).map(|c| (i, c)))
}

/// Add the savers to `settings` (an object; anything else is an error, and nothing is changed),
/// the hooks naming this fewer (`me`) and the store (`home`, FEWER_HOME as install was
/// given it). What was done, one line each.
pub fn add(settings: &mut Json, home: Option<&str>, me: &str) -> Result<Vec<String>, String> {
    let mut after = settings.clone();
    let Json::Obj(top) = &mut after else { return Err("it does not hold a JSON object".into()) };
    let mut said = Vec::new();
    let Json::Obj(hooks) = field(top, "hooks", Json::Obj(Vec::new())) else { return Err("its \"hooks\" is not an object".into()) };
    for (event, matcher, which) in HOOKS {
        let cmd = command(home, me, which);
        let Json::Arr(list) = field(hooks, event, Json::Arr(Vec::new())) else { return Err(format!("its hooks.{event} is not a list")) };
        let found = list.iter().enumerate().find_map(|(e, entry)| our_hook(entry, which).map(|(i, c)| (e, i, c == cmd)));
        match found {
            Some((_, _, true)) => said.push(format!("{event} hook there already: {cmd}")),
            Some((e, i, false)) => {
                if let Some(Json::Arr(h)) = list[e].get("hooks").cloned().as_mut() {
                    h[i].set("command", Json::from(cmd.as_str()));
                    list[e].set("hooks", Json::Arr(h.clone()));
                }
                said.push(format!("{event} hook pointed at this fewer: {cmd}"));
            }
            None => {
                let mut entry = Vec::new();
                if let Some(m) = matcher {
                    entry.push(("matcher".to_string(), Json::from(m)));
                }
                entry.push(("hooks".to_string(), Json::Arr(vec![Json::o(vec![("type", "command".into()), ("command", cmd.as_str().into())])])));
                list.push(Json::Obj(entry));
                said.push(format!("{event} hook added: {cmd}"));
            }
        }
    }
    match top.iter().find(|(k, _)| k == WINDOW_KEY) {
        Some((_, v)) => said.push(format!("{WINDOW_KEY} kept as you set it: {v}")),
        None => {
            top.push((WINDOW_KEY.to_string(), Json::Num(WINDOW.to_string())));
            said.push(format!("{WINDOW_KEY} = {WINDOW} added (compacts past about {} tokens)", group(WINDOW - BELOW)));
        }
    }
    *settings = after;
    Ok(said)
}

/// Take the savers away: our hooks, and the window when `window_by_value` and the hooks were
/// there (an install from before fewer's record, which wrote WINDOW or an older value with
/// its hooks: a window at such a value set by the developer is kept otherwise; fewer's own
/// window goes by its record, remove_lean). Lists and objects left empty by it are removed.
/// What was done.
pub fn remove(settings: &mut Json, window_by_value: bool) -> Vec<String> {
    let mut said = Vec::new();
    let Json::Obj(top) = settings else { return said };
    if let Some(i) = top.iter().position(|(k, _)| k == "hooks")
        && let Json::Obj(hooks) = &mut top[i].1
    {
            for (event, _, which) in HOOKS {
                let Some(j) = hooks.iter().position(|(k, _)| k == event) else { continue };
                let Json::Arr(list) = &mut hooks[j].1 else { continue };
                for entry in list.iter_mut() {
                    let Some(Json::Arr(h)) = entry.get("hooks") else { continue };
                    let keep: Vec<Json> = h.iter().filter(|x| !x.get("command").and_then(Json::str).is_some_and(|c| ours(c, which))).cloned().collect();
                    if keep.len() < h.len() {
                        said.push(format!("{event} hook removed"));
                        entry.set("hooks", Json::Arr(keep));
                    }
                }
                // an entry left with no hook is removed, and a list left empty
                list.retain(|e| !matches!(e.get("hooks"), Some(Json::Arr(h)) if h.is_empty()));
                if list.is_empty() {
                    hooks.remove(j);
                }
            }
        if hooks.is_empty() {
            top.remove(i);
        }
    }
    let ours = |v: &Json| std::iter::once(WINDOW).chain(OLD_WINDOWS).any(|w| *v == Json::Num(w.to_string()));
    if window_by_value
        && !said.is_empty()
        && let Some(i) = top.iter().position(|(k, v)| k == WINDOW_KEY && ours(v))
    {
        top.remove(i);
        said.push(format!("{WINDOW_KEY} removed"));
    }
    said
}

/// Whether any of the savers is in `settings` (the tests').
#[cfg(test)]
pub fn present(settings: &Json) -> bool {
    let mut copy = settings.clone();
    !remove(&mut copy, true).is_empty()
}

/// 1234567 as "1,234,567".
fn group(n: u64) -> String {
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

/// The text of `v` as people write settings.json: two spaces a level, one field a line.
pub fn pretty(v: &Json) -> String {
    fn go(v: &Json, depth: usize, out: &mut String) {
        let pad = |n: usize, out: &mut String| out.extend(std::iter::repeat_n(' ', 2 * n));
        match v {
            Json::Arr(items) if !items.is_empty() => {
                out.push('[');
                for (k, x) in items.iter().enumerate() {
                    out.push_str(if k > 0 { ",\n" } else { "\n" });
                    pad(depth + 1, out);
                    go(x, depth + 1, out);
                }
                out.push('\n');
                pad(depth, out);
                out.push(']');
            }
            Json::Obj(fields) if !fields.is_empty() => {
                out.push('{');
                for (k, (key, x)) in fields.iter().enumerate() {
                    out.push_str(if k > 0 { ",\n" } else { "\n" });
                    pad(depth + 1, out);
                    json::write_str(key, out);
                    out.push_str(": ");
                    go(x, depth + 1, out);
                }
                out.push('\n');
                pad(depth, out);
                out.push('}');
            }
            _ => json::write(v, out),
        }
    }
    let mut out = String::new();
    go(v, 0, &mut out);
    out.push('\n');
    out
}

/// Claude Code's user settings file. In development only the one in the folder set in
/// FEWER_CLAUDE_DIR: never ~/.claude by itself.
pub fn settings_file(places: &Places) -> Result<PathBuf, String> {
    places.claude.as_ref().map(|d| d.join("settings.json")).map_err(|why| format!("Claude Code's folder is not known ({why})"))
}

/// The file's bytes (None when there is no file) and its settings ({} for no file).
pub fn read(file: &Path) -> Result<(Option<Vec<u8>>, Json), String> {
    match fs::read(file) {
        Ok(b) if b.iter().all(u8::is_ascii_whitespace) => Ok((Some(b), Json::Obj(Vec::new()))),
        Ok(b) => {
            let j = json::parse(&b).map_err(|e| format!("it is not JSON fewer can read ({e}), so it is left as it is"))?;
            Ok((Some(b), j))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, Json::Obj(Vec::new()))),
        Err(e) => Err(e.to_string()),
    }
}

/// Write `new` over the file read as `was`: refused when the file changed since; the old file
/// kept next to it first (its name returned); the new one written whole, then put in place.
pub fn write(file: &Path, was: Option<&[u8]>, new: &Json) -> Result<Option<PathBuf>, String> {
    let now = fs::read(file).ok();
    if now.as_deref() != was {
        return Err("it changed while fewer was reading it; nothing was written, run this again".into());
    }
    let dir = file.parent().ok_or("it has no folder")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mode = fs::metadata(file).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let backup = match was {
        Some(b) => {
            // (named in local time, as answers show times)
            let (y, mo, d, h, mi, s, _) = crate::sys::local(crate::times::now_ms().div_euclid(1000));
            let mut name = format!("settings.json.before-fewer-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}");
            let mut n = 1;
            while dir.join(&name).exists() {
                n += 1;
                name = format!("settings.json.before-fewer-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{n}");
            }
            let p = dir.join(name);
            put(&p, b, mode)?;
            Some(p)
        }
        None => None,
    };
    // (a settings.json that is a link, as dotfiles keep it: the file it names is written, and the
    // link stays; the copy above is kept beside the link)
    let target = match fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(file).map_err(|e| format!("it is a link fewer cannot follow ({e})"))?,
        _ => file.to_path_buf(),
    };
    let tdir = target.parent().ok_or("it has no folder")?;
    let tmp = tdir.join(format!(".settings.json.fewer-{}", std::process::id()));
    put(&tmp, pretty(new).as_bytes(), mode)?;
    fs::rename(&tmp, &target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(backup)
}

/// A new file with these bytes and this mode, on disk before it returns.
fn put(p: &Path, b: &[u8], mode: u32) -> Result<(), String> {
    let mut f = fs::OpenOptions::new().write(true).create_new(true).open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    f.set_permissions(fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    f.write_all(b).and_then(|()| f.sync_all()).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_developers_own_agent_keeps_their_add_ons() {
        let own = json::parse(br#"{"agent":"my-agent"}"#).unwrap();
        let plan = lean_plan(&own, Some("/s"));
        assert!(!plan.iter().any(|p| ["agent", "deny-own"].contains(&p.as_str())), "{plan:?}");
        assert!(plan.iter().any(|p| p == "env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE"), "{plan:?}");
        // (no agent of their own: all of it; every add-on stays on but fewer's own)
        let plan = lean_plan(&Json::Obj(Vec::new()), Some("/s"));
        assert!(["agent", "deny-own"].iter().all(|k| plan.iter().any(|p| p == k)), "{plan:?}");
        assert!(!plan.iter().any(|p| ["connectors", "deny-web", "allow-none"].contains(&p.as_str()) || p.contains("MCP")), "{plan:?}");
    }

    #[test]
    fn the_lean_agent_keeps_claude_codes_file_tools_and_the_add_ons() {
        // (the main agent names what it leaves out: then Claude Code gives it the add-ons' tools,
        // which a `tools` list takes only by server name; the probes of 2026-11-26, 2.1.332)
        let main = lean_agent_text("fewer");
        assert!(main.contains("\ndisallowedTools: Agent, Task, ") && !main.contains("\ntools:"), "{main}");
        for kept in ["Bash", "Read", "Edit", "Write", "ToolSearch", "ListMcpResourcesTool", "ReadMcpResourceTool", "McpTaskCheck"] {
            assert!(!LEAN_LEAVES_OUT.split(", ").any(|t| t == kept), "{kept} is left out");
        }
        for out in ["Glob", "Grep", "WebFetch", "WebSearch", "Skill", "Workflow", "NotebookEdit", "AskUserQuestion", "TaskCreate", "mcp__fewer"] {
            assert!(LEAN_LEAVES_OUT.split(", ").any(|t| t == out), "{out} is not left out");
        }
        assert!(!main.contains("mcp__*"), "every add-on but fewer's own stays: {main}");
        // (every file change through Edit and Write: rewind and the permission rules cover it)
        assert!(main.contains("Change files only with Edit and Write, never with python3 or the shell"), "{main}");
        assert!(main.contains("ToolSearch loads them") && !main.contains("not with fewer on"), "{main}");
        let worker = worker_agent_text("fewer");
        assert!(worker.contains("\ntools: Bash, Read, Edit, Write\nomitClaudeMd: true\n---\n"), "{worker}");
        assert!(workflow_agent_text("fewer").contains("\ntools: Bash, Read, Edit, Write, Workflow, Skill\n---\n"));
        // (fewer's own MCP server, the one add-on fewer keeps off: its registered name)
        assert_eq!(OWN_SERVER, crate::install::NAME);
    }

    #[test]
    fn an_update_takes_back_what_1_0_0_turned_off() {
        // an early version's record and settings: the add-ons off in four ways
        let v1 = "lean:agent,lean:env:ENABLE_CLAUDEAI_MCP_SERVERS,lean:env:CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS,lean:env:CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH,lean:env:CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT,lean:env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE,lean:env:CLAUDE_CODE_TOTAL_TOKENS_REMINDER,lean:env:FEWER_HOME,lean:connectors,lean:deny-web,lean:allow-none,lean:window";
        let (old, kept) = retired_parts(v1);
        assert_eq!(old, "lean:env:ENABLE_CLAUDEAI_MCP_SERVERS,lean:env:CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS,lean:env:CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH,lean:connectors,lean:deny-web,lean:allow-none");
        assert_eq!(kept, "lean:agent,lean:env:CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT,lean:env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE,lean:env:CLAUDE_CODE_TOTAL_TOKENS_REMINDER,lean:env:FEWER_HOME,lean:window");
        let mut s = json::parse(br#"{"model":"opus","env":{"ENABLE_CLAUDEAI_MCP_SERVERS":"false","CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS":"1","CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH":"1","CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT":"0","CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1","CLAUDE_CODE_TOTAL_TOKENS_REMINDER":"off","FEWER_HOME":"/w/store"},"agent":"fewer-lean","disableClaudeAiConnectors":true,"deniedMcpServers":[{"serverUrl":"https://*"}],"allowedMcpServers":[{"serverName":"fewer-10x-none"}],"autoCompactWindow":100000}"#).unwrap();
        assert!(hides_own_server(&s));
        let said = remove_lean(&mut s, &old);
        assert_eq!(said.len(), 6, "{said:?}");
        // then this version's plan: only fewer's own server off
        assert_eq!(lean_plan(&s, Some("/w/store")), ["deny-own"]);
        let rec = add_lean(&mut s, &["deny-own".to_string()], Some("/w/store"), false).unwrap();
        assert_eq!(rec, "lean:deny-own");
        assert_eq!(s.to_string(), r#"{"model":"opus","env":{"CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT":"0","CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1","CLAUDE_CODE_TOTAL_TOKENS_REMINDER":"off","FEWER_HOME":"/w/store"},"agent":"fewer-lean","autoCompactWindow":100000,"deniedMcpServers":[{"serverName":"fewer"}]}"#);
        assert!(hides_own_server(&s) && lean_plan(&s, Some("/w/store")).is_empty());
        // a value the developer changed since an early version wrote it is theirs: kept
        let mut t = json::parse(br#"{"env":{"CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH":"500"}}"#).unwrap();
        assert!(remove_lean(&mut t, "lean:env:CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH").is_empty());
        // nothing retired in a record of this version
        assert_eq!(retired_parts("lean:agent,lean:deny-own"), (String::new(), "lean:agent,lean:deny-own".to_string()));
    }

    use super::*;

    const ME: &str = "/opt/fewer/fewer";

    #[test]
    fn a_linked_settings_file_stays_a_link() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-link", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("dotfiles")).unwrap();
        fs::create_dir_all(d.join("claude")).unwrap();
        fs::write(d.join("dotfiles").join("settings.json"), "{}").unwrap();
        let link = d.join("claude").join("settings.json");
        crate::portable::symlink(d.join("dotfiles").join("settings.json"), &link).unwrap();
        let copy = write(&link, Some(b"{}"), &json::parse(br#"{"a":1}"#).unwrap()).unwrap().unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(d.join("dotfiles").join("settings.json")).unwrap(), "{\n  \"a\": 1\n}\n");
        assert_eq!(copy.parent(), Some(d.join("claude").as_path()));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn added_once_kept_apart_removed_cleanly() {
        let theirs = r#"{"model":"opus","cleanupPeriodDays":30.0,"hooks":{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"fmt"}]}]}}"#;
        let mut s = json::parse(theirs.as_bytes()).unwrap();
        let said = add(&mut s, None, ME).unwrap();
        assert_eq!(said, [
            "PostToolUse hook added: /opt/fewer/fewer hook post-bash",
            "SessionStart hook added: /opt/fewer/fewer hook start",
            "PreCompact hook added: /opt/fewer/fewer hook precompact",
            "autoCompactWindow = 120000 added (compacts past about 87,000 tokens)",
        ]);
        let t = s.to_string();
        assert!(t.contains(r#""matcher":"Edit""#) && t.contains(r#""cleanupPeriodDays":30.0"#), "{t}");
        assert!(t.contains(r#"{"matcher":"Bash","hooks":[{"type":"command","command":"/opt/fewer/fewer hook post-bash"}]}"#), "{t}");
        assert!(present(&s));
        // a second install adds nothing again
        let again = add(&mut s, None, ME).unwrap();
        assert!(again[0].starts_with("PostToolUse hook there already") && again[1].starts_with("SessionStart hook there already"), "{again:?}");
        assert!(again[2].starts_with("PreCompact hook there already"), "{again:?}");
        assert_eq!(again[3], "autoCompactWindow kept as you set it: 120000");
        assert_eq!(s.to_string(), t);
        // fewer moved: our hooks follow it, nothing is added twice
        let moved = add(&mut s, None, "/new place/fewer").unwrap();
        assert_eq!(moved[0], "PostToolUse hook pointed at this fewer: '/new place/fewer' hook post-bash");
        assert_eq!(s.to_string().matches("hook post-bash").count(), 1);
        // the store named in the command (Claude Code's own environment has no FEWER_HOME)
        let with_home = add(&mut s, Some("/w/my store"), ME).unwrap();
        assert_eq!(with_home[1], "SessionStart hook pointed at this fewer: FEWER_HOME='/w/my store' /opt/fewer/fewer hook start");
        assert_eq!(crate::hook::pipelines("FEWER_HOME='/w/my store' /opt/fewer/fewer hook start"), [vec![vec!["FEWER_HOME=/w/my store", "/opt/fewer/fewer", "hook", "start"]]]);
        // uninstall: ours away (wherever fewer is), the developer's own hook kept
        let gone = remove(&mut s, true);
        assert_eq!(gone, ["PostToolUse hook removed", "SessionStart hook removed", "PreCompact hook removed", "autoCompactWindow removed"]);
        assert_eq!(s.to_string(), theirs);
        assert!(!present(&s));
        // a window the developer set is theirs: kept by add and by remove
        let mut w = json::parse(br#"{"autoCompactWindow":300000}"#).unwrap();
        assert_eq!(add(&mut w, None, ME).unwrap()[3], "autoCompactWindow kept as you set it: 300000");
        // a window an earlier version set (150000) with its hooks is taken back as ours; the same
        // value alone, or under fewer's record (window_by_value false), is the developer's
        let mut old = Json::Obj(Vec::new());
        add(&mut old, None, ME).unwrap();
        if let Json::Obj(top) = &mut old {
            top.iter_mut().filter(|(k, _)| k == WINDOW_KEY).for_each(|(_, v)| *v = Json::Num("150000".into()));
        }
        let mut kept = old.clone();
        assert_eq!(remove(&mut old, true).last().map(String::as_str), Some("autoCompactWindow removed"));
        assert!(!remove(&mut kept, false).iter().any(|x| x.starts_with(WINDOW_KEY)));
        assert_eq!(kept.to_string(), r#"{"autoCompactWindow":150000}"#);
        for v in ["120000", "150000"] {
            let mut alone = json::parse(format!(r#"{{"autoCompactWindow":{v}}}"#).as_bytes()).unwrap();
            assert!(remove(&mut alone, true).is_empty(), "{v}");
        }
        remove(&mut w, true);
        assert_eq!(w.to_string(), r#"{"autoCompactWindow":300000}"#);
        // an empty settings file: everything added, then everything removed
        let mut e = Json::Obj(Vec::new());
        add(&mut e, None, "/a b/fewer").unwrap();
        assert!(e.to_string().contains(r#""command":"'/a b/fewer' hook start""#), "{e}");
        remove(&mut e, true);
        assert_eq!(e.to_string(), "{}");
        // not an object, or hooks of another shape: an error, and nothing changed
        for bad in ["[]", r#"{"hooks":[]}"#, r#"{"hooks":{"PostToolUse":{}}}"#] {
            let mut b = json::parse(bad.as_bytes()).unwrap();
            assert!(add(&mut b, None, ME).is_err(), "{bad}");
            assert_eq!(b.to_string(), bad);
        }
        assert!(ours("fewer hook start", "start") && ours("'/x y/fewer' hook start", "start") && ours("FEWER_HOME='/a b' /x/fewer hook start", "start"));
        assert!(!ours("/x/fewerer hook start", "start") && !ours("/x/fewer hook post-bash", "start"));
        assert!(!ours("echo x; /x/fewer hook start", "start") && !ours("/x/fewer --v hook start", "start"));
    }

    #[test]
    fn written_readably_with_a_copy_kept() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-savers", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let file = d.join("settings.json");
        // no file: written new, no copy
        let (was, mut j) = read(&file).unwrap();
        assert_eq!((was.as_deref(), j.to_string().as_str()), (None, "{}"));
        add(&mut j, None, ME).unwrap();
        assert_eq!(write(&file, None, &j).unwrap(), None);
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("{\n  \"hooks\": {\n    \"PostToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n"), "{text}");
        assert!(text.ends_with("  \"autoCompactWindow\": 120000\n}\n"), "{text}");
        assert_eq!(json::parse(text.as_bytes()).unwrap(), j);
        #[cfg(unix)]
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // a file there: the old one kept next to it, its mode kept
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let (was, mut j) = read(&file).unwrap();
        remove(&mut j, true);
        let copy = write(&file, was.as_deref(), &j).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&copy).unwrap(), text);
        assert!(copy.file_name().unwrap().to_str().unwrap().starts_with("settings.json.before-fewer-"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "{}\n");
        #[cfg(unix)]
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
        // changed since it was read: nothing written
        let (was, j) = read(&file).unwrap();
        fs::write(&file, "{\"theirs\": 1}\n").unwrap();
        assert!(write(&file, was.as_deref(), &j).unwrap_err().contains("changed while fewer was reading it"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"theirs\": 1}\n");
        // not JSON: left as it is
        fs::write(&file, "{ // a comment\n}").unwrap();
        assert!(read(&file).unwrap_err().contains("left as it is"));
        // nothing but our files in the folder (no temporary file left)
        let mut names: Vec<String> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        fs::remove_dir_all(&d).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// A smaller start (a quality check: a smaller start used 3.8 times
// fewer tokens with the same answers). Every reply re-reads Claude Code's start; these three
// parts of it are turned off or shortened at the developer's yes, and only what fewer put there
// is taken back by uninstall (the store's config `savers.start` lists it):
// - workflows (multi-agent runs; ultracode needs them): env CLAUDE_CODE_DISABLE_WORKFLOWS = 1;
// - the Artifact tool: enableArtifact = false;
// - the skills in Claude Code's folder (skills/**/SKILL.md) listed by name only, without their
//   descriptions: skillOverrides.<name> = "name-only" (each still works, /name still types;
//   Claude Code does not apply overrides to plugins' skills, so those are left as they are).

/// A skill found in Claude Code's folder: its name and the length of its description.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: usize,
}

/// The skills in `<claude_dir>/skills`, up to 4 folders down: each SKILL.md's `name:` (else
/// its folder's name) and the length of its `description:`; sorted by name, each name once.
pub fn skills_found(claude_dir: &Path) -> Vec<Skill> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<Skill>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(t) = e.file_type() else { continue };
            let p = e.path();
            if t.is_dir() && depth > 0 && !e.file_name().to_string_lossy().starts_with('.') {
                walk(&p, depth - 1, out);
            } else if t.is_file() && e.file_name() == "SKILL.md" {
                let Ok(text) = fs::read_to_string(&p) else { continue };
                let front = text.strip_prefix("---\n").and_then(|r| r.split("\n---").next()).unwrap_or("");
                let field = |k: &str| front.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string()));
                let folder = p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let name = field("name:").filter(|n| !n.is_empty()).unwrap_or(folder);
                let description = field("description:").map_or(0, |d| d.len());
                if !name.is_empty() {
                    out.push(Skill { name, description });
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&claude_dir.join("skills"), 4, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

pub const WORKFLOWS_ENV: &str = "CLAUDE_CODE_DISABLE_WORKFLOWS";

/// What a smaller start would change in `settings` now (parts already as the developer set
/// them are left out): (workflows off, Artifact off, the skills to list by name only).
pub fn start_plan(settings: &Json, skills: &[Skill]) -> (bool, bool, Vec<String>) {
    let env_has = settings.get("env").and_then(|e| e.get(WORKFLOWS_ENV)).is_some();
    let artifact_set = settings.get("enableArtifact").is_some();
    let overrides = settings.get("skillOverrides");
    let names = skills.iter().filter(|s| overrides.and_then(|o| o.get(&s.name)).is_none()).map(|s| s.name.clone()).collect();
    (!env_has, !artifact_set, names)
}

/// Make the start smaller as planned; the record of what was put there (for `savers.start`).
pub fn add_start(settings: &mut Json, workflows: bool, artifact: bool, skills: &[String]) -> Result<String, String> {
    let mut after = settings.clone();
    let Json::Obj(top) = &mut after else { return Err("it does not hold a JSON object".into()) };
    let mut record = Vec::new();
    if workflows {
        let Json::Obj(env) = field(top, "env", Json::Obj(Vec::new())) else { return Err("its \"env\" is not an object".into()) };
        env.push((WORKFLOWS_ENV.to_string(), Json::from("1")));
        record.push("workflows".to_string());
    }
    if artifact {
        top.push(("enableArtifact".to_string(), Json::Bool(false)));
        record.push("artifact".to_string());
    }
    if !skills.is_empty() {
        let Json::Obj(o) = field(top, "skillOverrides", Json::Obj(Vec::new())) else { return Err("its \"skillOverrides\" is not an object".into()) };
        for n in skills {
            o.push((n.clone(), Json::from("name-only")));
            record.push(format!("skill:{n}"));
        }
    }
    *settings = after;
    Ok(record.join(","))
}

/// Take back what `record` says fewer put there, while it is still as fewer set it; objects left
/// empty by it are removed. What was taken back, one line each.
pub fn remove_start(settings: &mut Json, record: &str) -> Vec<String> {
    let mut said = Vec::new();
    let Json::Obj(top) = settings else { return said };
    let (mut skills, mut n_skills) = (Vec::new(), 0);
    for part in record.split(',').filter(|p| !p.is_empty()) {
        match part {
            "workflows" => {
                if let Some(i) = top.iter().position(|(k, _)| k == "env")
                    && let Json::Obj(env) = &mut top[i].1
                    && let Some(j) = env.iter().position(|(k, v)| k == WORKFLOWS_ENV && v.str() == Some("1"))
                {
                    env.remove(j);
                    said.push(format!("{WORKFLOWS_ENV} removed"));
                    if env.is_empty() {
                        top.remove(i);
                    }
                }
            }
            "artifact" => {
                if let Some(i) = top.iter().position(|(k, v)| k == "enableArtifact" && *v == Json::Bool(false)) {
                    top.remove(i);
                    said.push("enableArtifact removed".into());
                }
            }
            p => {
                if let Some(n) = p.strip_prefix("skill:") {
                    skills.push(n.to_string());
                }
            }
        }
    }
    if !skills.is_empty()
        && let Some(i) = top.iter().position(|(k, _)| k == "skillOverrides")
        && let Json::Obj(o) = &mut top[i].1
    {
        o.retain(|(k, v)| {
            let ours = skills.contains(k) && v.str() == Some("name-only");
            n_skills += usize::from(ours);
            !ours
        });
        if o.is_empty() {
            top.remove(i);
        }
    }
    if n_skills > 0 {
        said.push(format!("{n_skills} skills listed in full again"));
    }
    said
}

#[cfg(test)]
mod start_tests {
    use super::*;

    #[test]
    fn a_smaller_start_added_and_taken_back() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-start", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let put = |rel: &str, text: &str| {
            let p = d.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        };
        put("skills/verify/SKILL.md", "---\nname: verify\ndescription: Use when coding: run the checks.\n---\nbody\n");
        put("skills/synced/abc/docx/SKILL.md", "---\nname: \"docx\"\ndescription: Word files.\n---\n");
        put("skills/plain/SKILL.md", "no front matter\n");
        put("skills/a/b/c/d/e/deep/SKILL.md", "---\nname: too-deep\n---\n");
        put("skills/.hidden/x/SKILL.md", "---\nname: hidden\n---\n");
        let skills = skills_found(&d);
        assert_eq!(skills, [
            Skill { name: "docx".into(), description: 11 },
            Skill { name: "plain".into(), description: 0 },
            Skill { name: "verify".into(), description: 32 },
        ]);
        // the developer's own override and env value stay theirs
        let theirs = r#"{"env":{"A":"1"},"skillOverrides":{"verify":"off"}}"#;
        let mut s = json::parse(theirs.as_bytes()).unwrap();
        let (wf, art, names) = start_plan(&s, &skills);
        assert_eq!((wf, art, names.clone()), (true, true, vec!["docx".to_string(), "plain".to_string()]));
        let record = add_start(&mut s, wf, art, &names).unwrap();
        assert_eq!(record, "workflows,artifact,skill:docx,skill:plain");
        assert_eq!(s.to_string(), r#"{"env":{"A":"1","CLAUDE_CODE_DISABLE_WORKFLOWS":"1"},"skillOverrides":{"verify":"off","docx":"name-only","plain":"name-only"},"enableArtifact":false}"#);
        // nothing left to do a second time
        assert_eq!(start_plan(&s, &skills), (false, false, Vec::new()));
        // a skill the developer changed since is theirs now: kept
        if let Json::Obj(top) = &mut s
            && let Some((_, Json::Obj(o))) = top.iter_mut().find(|(k, _)| k == "skillOverrides")
        {
            o[2].1 = Json::from("off");
        }
        let said = remove_start(&mut s, &record);
        assert_eq!(said, ["CLAUDE_CODE_DISABLE_WORKFLOWS removed", "enableArtifact removed", "1 skills listed in full again"]);
        assert_eq!(s.to_string(), r#"{"env":{"A":"1"},"skillOverrides":{"verify":"off","plain":"off"}}"#);
        // an empty start: everything added, everything taken back
        let mut e = Json::Obj(Vec::new());
        let r = add_start(&mut e, true, true, &["x".to_string()]).unwrap();
        remove_start(&mut e, &r);
        assert_eq!(e.to_string(), "{}");
        fs::remove_dir_all(&d).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// fewer's lean agents (measured with Claude Code 2.1.332: 87% fewer tokens on nine jobs against
// Claude Code as it is, every answer kept, no less safe). Claude Code's `agent` setting makes fewer's lean
// agent the main session's agent: short rules with the safety rules (they replace Claude Code's
// own instructions, which is what an agent's prompt does), a question in one call plus the
// answer, tests and builds run as Claude Code runs any command (` 2>&1 || true` at the end, so that
// a failing run ends well and fewer's PostToolUse hook may shorten it: Claude Code lets a hook
// replace only a successful command's output, 2.1.332), each one under the user's own Bash rules
// as usual, with no wrapper around it. It keeps what people rely on: Claude Code's own Read, Edit
// and Write tools, through which every
// file change goes (rewind undoes it, the developer's Read, Edit and Write rules apply, each change
// is shown as usual), and the developer's add-ons (MCP servers) and claude.ai's connectors as
// normal. The agent names the tools it leaves out (`disallowedTools`) rather than the ones it
// keeps: only then does Claude Code give it the add-ons' tools (a `tools` list takes an add-on
// only by its server's name, never mcp__*; read from its code and probed, 2.1.332), and
// ToolSearch loads one when a job needs it. Every feature stays, at almost no cost a step,
// loaded only when a job needs it (helpers, workflows and skills all keep working, with any
// number of agents):
// - skills: `fewer skill NAME` prints one's instructions;
// - helpers: `fewer helper "<job>"`, a lean Claude Code session of its own, as many at once as
//   wanted, with no add-ons (--strict-mcp-config: their instructions would be re-read at every
//   step of every helper, and a helper's job is files and commands);
// - workflows: `fewer workflow "<job>"`, a session of fewer's second agent,
//   fewer-lean-workflow, the only one with the Workflow tool, whose scripts give every agent()
//   fewer-worker as its type: Claude Code's own workflow agents get every tool and a long start,
//   and nothing but a script's agentType changes that (read from its code, 2.1.287).
// Only fewer's own MCP server is off with fewer on (a `deniedMcpServers` entry): its tools are
// fewer's commands, which the lean agents run, and its instructions would cost every step about
// 600 tokens. CLAUDE.md files stay: the developer's own rules keep applying.

pub const LEAN_AGENT: &str = "fewer-lean";
/// The agent of a workflow's own session (`fewer workflow`).
pub const WORKFLOW_AGENT: &str = "fewer-lean-workflow";
/// The agent of helpers and workflow agents: the lean rules for one job, without the features
/// (a helper starts no skill, helper or workflow of its own).
pub const WORKER_AGENT: &str = "fewer-worker";
/// How the main session's agent starts its rules, and how a worker's start.
const MAIN_WHO: &str = "You are Claude Code, working in the user's repository.";
const WORKER_WHO: &str = "You are a helper of Claude Code doing one job in the user's repository; answer only what the job asks.";
/// The built-in tools the main session's agent leaves out (its `disallowedTools`): everything but
/// Bash, Read, Edit, Write, ToolSearch and the add-ons' own (their resources, their background
/// tasks). Claude Code's tool names in 2.1.287 and 2.1.332, in -p and in its editors' sessions; a
/// name a version does not have is passed over. fewer's own MCP server's tools too, should its
/// server run all the same (`deny_own` below keeps it off).
pub const LEAN_LEAVES_OUT: &str = "Agent, Task, AskUserQuestion, BashOutput, KillShell, CronCreate, CronDelete, CronList, DesignSync, \
EnterPlanMode, ExitPlanMode, EnterWorktree, ExitWorktree, Glob, Grep, LS, LSP, ListAgents, Monitor, MultiEdit, NotebookEdit, NotebookRead, \
PowerShell, PushNotification, RemoteTrigger, ReportFindings, ScheduleWakeup, SendMessage, SendUserFile, Skill, SlashCommand, TaskCreate, \
TaskGet, TaskList, TaskOutput, TaskStop, TaskUpdate, TodoWrite, WebFetch, WebSearch, Workflow, Artifact, ArtifactCheck, ArtifactComments, \
ArtifactData, ReadNotifications, FetchInboxMessage, SendToProject, SendToProjectContext, mcp__fewer";
/// The tools of a worker (a helper's or a workflow agent's): no add-ons (`fewer helper` starts
/// its session with none).
pub const WORKER_TOOLS: &str = "Bash, Read, Edit, Write";
/// The environment of fewer (only where the developer set nothing). Read from Claude Code
/// 2.1.287 and measured (2026-11-24): the commit lines' note moved into the Bash tool's text
/// (ATTRIBUTION_ANNOUNCEMENT=0: ~180 tokens a step, the lines still written), no session-title
/// side call (~900 tokens a session; the session list shows the first message instead), no token
/// countdown note after each step.
pub const LEAN_ENV: [(&str, &str); 3] = [
    ("CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT", "0"),
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
    ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off"),
];
/// What early versions (before 1.0.0) set, still taken back as fewer's (at fewer's own values):
/// CLAUDE.md files off, and add-ons off (claude.ai's connectors, plugins' MCP servers, add-ons'
/// instructions cut to one character).
const OLD_LEAN_ENV: [(&str, &str); 4] = [
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
    ("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS", "1"),
    ("CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH", "1"),
];
/// The parts an early version of fewer wrote that this version takes back on its own when it
/// finds them (`fewer install`, `fewer on`): the add-ons off, in each of the ways it had.
pub const RETIRED: [&str; 7] = [
    "lean:connectors",
    "lean:deny-web",
    "lean:allow-none",
    "lean:env:ENABLE_CLAUDEAI_MCP_SERVERS",
    "lean:env:CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS",
    "lean:env:CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH",
    "lean:env:CLAUDE_CODE_DISABLE_CLAUDE_MDS",
];
/// An early version's entry in `deniedMcpServers`: Claude Code's connections to web add-ons off. Taken
/// back, never written now.
pub const DENY_WEB: &str = "https://*";
/// An early version's `allowedMcpServers` list whose one entry names no add-on, so that Claude Code
/// connected none of them. Taken back, never written now.
pub const ALLOW_NONE: &str = "fewer-10x-none";
/// The name fewer's own MCP server is registered under (install's `claude mcp add`): the one
/// add-on fewer keeps off.
pub const OWN_SERVER: &str = "fewer";
/// The entry's names in early versions (under the tool's earlier name, cmlog-search), taken out but
/// never written: Claude Code's settings check takes only letters, digits, '-' and '_' in a
/// serverName, and might warn about or skip a file that holds another.
const OLD_ALLOW_NONE: &[&str] = &["cmlog-10x: add-ons through cmlog-search", "cmlog-10x-none"];

/// Whether an entry of `allowedMcpServers` is fewer's own (its name now or before).
pub(crate) fn is_allow_none(e: &Json) -> bool {
    *e == allow_none() || OLD_ALLOW_NONE.iter().any(|n| *e == Json::Obj(vec![("serverName".to_string(), Json::from(*n))]))
}
/// The window with the lean start: the floor after a compaction is small, so 100k has room.
pub const LEAN_WINDOW: u64 = 100_000;
/// The first line of an agent's file after its front matter: fewer's mark, so only a file of
/// fewer's is ever replaced or removed.
const LEAN_MARK: &str = "<!-- fewer's lean mode: written by fewer install; uninstall removes it -->";

/// The lean rules every lean agent has; `c` is how a command names fewer (its name
/// when the one on PATH is this one, else its whole path: Claude Code's permission rules match
/// a command as it is written, so a plain name can be allowed once).
pub fn core_rules(c: &str, who: &str) -> String {
    format!(
        "{who} Check facts with commands; never guess. \
Change only what was asked (fix code, not tests); look before you change; never delete data, force-push or reset unless asked; \
never print secrets. Every step re-reads everything, so: for a question, one Bash call that gets every fact at once (a command, \
or a small python3 program printing only the facts), then answer. Read only the files a job needs, whole: `{c} read FILE...` (each \
file's line count, a repeated block once, nothing lost; one file shows a folder's style), several in one call, also when the user \
asks to go through them one at a time (read them together, then answer about each in turn); never print long files; list a \
project's files with git ls-files (find and ls -R list its ignored caches too), a folder's with ls, not ls -l. A CSV, TSV or log question: start \
with `{c} table FILE` (columns, sums and counts by category: usually the answer). One python3 program rather than a chain of \
shell commands, a heredoc alone in its command and each command under 10,000 characters (Claude Code refuses the rest: write a \
long file in parts); in it never write a {{ with a quote before its }} ({{'a': 1}}, f\"{{', '.join(x)}}\"): Claude Code asks about such a command (in -p refuses it), heredocs included, so use dict(...) and set([...]), keep quotes out of an f-string's braces (sep = ', ' then f\"{{sep.join(x)}}\"); and no $(...) in a command (asked about too). Tests and builds: the command itself, ending in ` 2>&1 || true` (a long output \
comes back short, each failure in full, all of it saved: `{c} tool lines` reads more); a commit's changes: `{c} changes [REV]`. Change files only with Edit and Write, never with python3 or the shell, so the \
user's rewind and permission rules cover every change. New code: Write each file once, its tests run in the same step. A change: \
Edit with an exact piece of text you have seen, never a whole file again; no Read first (Claude Code lets you edit a file you may read; \
only if Edit says to read it first, Read just the lines you change, with offset and limit). A fix: one step running the failing \
test and printing the code it uses, one with the Edit and the test run again, then answer. Answer briefly: exact names, numbers, \
paths, errors."
    )
}

/// Every feature, on demand (the main session's and helpers' agent).
pub fn feature_rules(c: &str) -> String {
    format!(
        " Only when the user asks, or a job cannot be done without it (otherwise never look for them): a skill: \
`{c} skill NAME \"the job\"` (its instructions for the job, to follow; `--all`: all of it); a picture or a scanned PDF: \
`{c} look FILE... \"question\"` (you cannot see one); the web: `{c} web \"question, or a page's address and question\"`; \
an add-on (MCP server) or a claude.ai connector: its tools, as usual (ToolSearch loads them); \
helpers (asked for, or a very large job): `{c} helper \"<job 1>\" \"<job 2>\"` (one per job, all at once; write each job \
as the facts to find); a workflow (asked for): `{c} workflow \"<the job>\"`. Run helpers and workflows in the foreground \
with Bash's longest timeout."
    )
}

/// What the workflow agent adds.
pub fn workflow_rules(c: &str) -> String {
    format!(
        " This session runs a workflow for the job: write one Workflow script for it, and in the script give \
every agent() call the option agentType: 'fewer-worker' (fewer's lean helper type), with as many agents as the job needs. Write each \
agent's prompt as the facts it must find; when the job says to read files whole, tell the agent to read all of them with one \
`{c} read FILE...`, never to print them. Give agent() no schema: ask each agent for a short plain answer (a schema costs every \
agent a step more), and combine the answers yourself, in the script's code or in your answer: never with one more agent (each agent is a session of its own); check a fact again yourself only when the agents' answers disagree or one is missing. Load no skill (not workflow-authoring either): a \
script is `export const meta = {{ name: 'x', description: 'y' }}` (plain literals only), then its code; \
`await agent(prompt, {{ agentType: 'fewer-worker', label: 'x' }})` returns an agent's answer; \
`await parallel(items.map(i => () => agent(...)))` runs them at once; `return` what the workflow gives back. When the workflow \
ends, answer with its result: exact names, numbers, paths, errors."
    )
}

/// How a printed command names this fewer, as the lean rules do: `fewer` when PATH
/// finds this program, else its whole path (a read-on line naming a
/// program that is not there cost a step).
pub fn me() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| p.to_str().map(str::to_string)).unwrap_or_else(|| "fewer".into());
    command_word(&exe, std::env::var_os("PATH").as_deref())
}

/// How the rules name fewer (`me`): `fewer` when that name on `path` is this
/// program, else its whole path, quoted when it needs it.
pub fn command_word(me: &str, path: Option<&std::ffi::OsStr>) -> String {
    let same = |p: &Path| fs::canonicalize(p).ok().zip(fs::canonicalize(me).ok()).is_some_and(|(a, b)| a == b);
    let on_path = path.into_iter().flat_map(std::env::split_paths).filter(|d| d.is_absolute()).map(|d| d.join(format!("fewer{}", std::env::consts::EXE_SUFFIX))).find(|p| p.is_file());
    if on_path.is_some_and(|p| same(&p)) { "fewer".to_string() } else { quoted(me) }
}

/// An agent's file: front matter (`tools`: its lines naming tools; `more`: lines of its own),
/// fewer's mark, then its rules.
fn agent_text(name: &str, description: &str, tools: &str, more: &str, rules: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n{tools}{more}---\n{LEAN_MARK}\n{rules}\n")
}

/// The lean agent's file (the main session's): every tool but those it leaves out, so the
/// developer's add-ons come as they do without fewer.
pub fn lean_agent_text(c: &str) -> String {
    agent_text(LEAN_AGENT, "fewer", &format!("disallowedTools: {LEAN_LEAVES_OUT}\n"), "", &format!("{}{}", core_rules(c, MAIN_WHO), feature_rules(c)))
}

/// The workflow agent's file.
pub fn workflow_agent_text(c: &str) -> String {
    // (with the Skill tool in its list, Claude Code gives the Workflow tool its short description,
    // 3,480 characters instead of 20,256 that every step of the workflow's session re-read; the
    // rules say what a script needs and to load no skill)
    agent_text(WORKFLOW_AGENT, "fewer: a workflow", "tools: Bash, Read, Edit, Write, Workflow, Skill\n", "", &format!("{}{}", core_rules(c, MAIN_WHO), workflow_rules(c)))
}

/// The worker's file (helpers' and workflow agents'). A workflow agent leaves out the CLAUDE.md
/// files (omitClaudeMd: ~450 tokens a step; the main session writes the job, and fewer's rules carry
/// what it needs); a helper, a session of its own, leaves them out through its environment
/// (ondemand.rs).
pub fn worker_agent_text(c: &str) -> String {
    agent_text(WORKER_AGENT, "fewer: one job", &format!("tools: {WORKER_TOOLS}\n"), "omitClaudeMd: true\n", &core_rules(c, WORKER_WHO))
}

/// The lean agent's file in Claude Code's folder.
pub fn lean_agent_file(claude_dir: &Path) -> PathBuf {
    claude_dir.join("agents").join(format!("{LEAN_AGENT}.md"))
}

/// The workflow agent's file in Claude Code's folder.
pub fn workflow_agent_file(claude_dir: &Path) -> PathBuf {
    claude_dir.join("agents").join(format!("{WORKFLOW_AGENT}.md"))
}

/// The worker's file in Claude Code's folder.
pub fn worker_agent_file(claude_dir: &Path) -> PathBuf {
    claude_dir.join("agents").join(format!("{WORKER_AGENT}.md"))
}

/// Each agent's file and its text as this version writes it.
fn agent_files(claude_dir: &Path, c: &str) -> [(PathBuf, String); 3] {
    [(lean_agent_file(claude_dir), lean_agent_text(c)), (workflow_agent_file(claude_dir), workflow_agent_text(c)), (worker_agent_file(claude_dir), worker_agent_text(c))]
}

/// Whether every agent's file is there as this version writes it.
pub fn agents_there(claude_dir: &Path, c: &str) -> bool {
    agent_files(claude_dir, c).iter().all(|(p, t)| fs::read_to_string(p).is_ok_and(|a| a == *t))
}

/// Write the agents' files (a file there that is not fewer's is left alone: an error, and
/// nothing is written).
pub fn write_lean_agent(claude_dir: &Path, c: &str) -> Result<(), String> {
    let files = agent_files(claude_dir, c);
    for (p, _) in &files {
        if let Ok(old) = fs::read_to_string(p)
            && !old.contains(LEAN_MARK)
        {
            return Err(format!("{} is someone else's agent; it is left as it is", p.display()));
        }
    }
    for (p, text) in &files {
        fs::create_dir_all(p.parent().ok_or("no folder")?).map_err(|e| e.to_string())?;
        let tmp = p.with_extension(format!("md.fewer-{}", std::process::id()));
        put(&tmp, text.as_bytes(), 0o644)?;
        fs::rename(&tmp, p).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e.to_string()
        })?;
    }
    Ok(())
}

/// fewer's mark in its slash commands' files (a file there without it is someone else's).
const SLASH_MARK: &str = "<!-- fewer's slash command: fewer uninstall takes it out -->";

/// fewer's slash commands in Claude Code (its commands/ folder): the name, what it does, the
/// words after fewer it runs. Install writes them, uninstall takes them out (off and on keep them,
/// so /fewer-on is there when fewer is off). Only the person runs them: Claude Code does not
/// list them to the model (disable-model-invocation), so a chat's start stays the same size.
pub const SLASH: [(&str, &str, &str); 4] = [
    ("fewer-tokens", "fewer: how many tokens you saved", "tokens"),
    ("fewer-off", "fewer: turn it off (new and reopened chats; an open one keeps its setup)", "off"),
    ("fewer-on", "fewer: turn it on (new and reopened chats; an open one keeps its setup)", "on"),
    ("fewer-memory", "fewer: what fewer saved, and the room it takes", "memory --total"),
];

/// A slash command's file: Claude Code runs the command first (the ! line; allowed exactly as it
/// is written, as a hook's command is: the store named when install was given one), then Claude
/// shows its answer as it is.
pub fn slash_text(home: Option<&str>, me: &str, description: &str, words: &str) -> String {
    let run = match home {
        Some(h) => format!("FEWER_HOME={} {} {words}", quoted(h), quoted(me)),
        None => format!("{} {words}", quoted(me)),
    };
    format!(
        "---\ndescription: {description}\nallowed-tools: Bash({run})\ndisable-model-invocation: true\n---\n{SLASH_MARK}\n!`{run}`\n\nThe lines above are what `fewer {words}` said. Show them to the user as they are, in a code block, and add nothing.\n"
    )
}

/// Each slash command's file and its text as this version writes it.
pub fn slash_files(claude_dir: &Path, home: Option<&str>, me: &str) -> Vec<(PathBuf, String)> {
    SLASH.iter().map(|(name, what, words)| (claude_dir.join("commands").join(format!("{name}.md")), slash_text(home, me, what, words))).collect()
}

/// Write the slash commands' files (one there that is not fewer's is left alone, and said).
pub fn write_slash(claude_dir: &Path, home: Option<&str>, me: &str) -> Result<(), String> {
    let files = slash_files(claude_dir, home, me);
    for (p, _) in &files {
        if let Ok(old) = fs::read_to_string(p)
            && !old.contains(SLASH_MARK)
        {
            return Err(format!("{} is someone else's command; it is left as it is", p.display()));
        }
    }
    for (p, text) in &files {
        fs::create_dir_all(p.parent().ok_or("no folder")?).map_err(|e| e.to_string())?;
        let tmp = p.with_extension(format!("md.fewer-{}", std::process::id()));
        put(&tmp, text.as_bytes(), 0o644)?;
        fs::rename(&tmp, p).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e.to_string()
        })?;
    }
    Ok(())
}

/// Remove the slash commands' files, each only when it is fewer's: whether any was removed.
pub fn remove_slash(claude_dir: &Path) -> bool {
    let mut any = false;
    for (name, _, _) in SLASH {
        let p = claude_dir.join("commands").join(format!("{name}.md"));
        if let Ok(t) = fs::read_to_string(&p)
            && t.contains(SLASH_MARK)
        {
            any |= fs::remove_file(&p).is_ok();
        }
    }
    // (the folder too, when nothing else is in it)
    let _ = fs::remove_dir(claude_dir.join("commands"));
    any
}

/// Remove the agents' files, each only when it is fewer's: whether any was removed.
pub fn remove_lean_agent(claude_dir: &Path) -> bool {
    let mut any = false;
    for p in [lean_agent_file(claude_dir), workflow_agent_file(claude_dir), worker_agent_file(claude_dir)] {
        if let Ok(t) = fs::read_to_string(&p)
            && t.contains(LEAN_MARK)
        {
            any |= fs::remove_file(&p).is_ok();
        }
    }
    any
}

/// What fewer would set in `settings` now (what the developer set is left out): the agent,
/// the environment's keys, and FEWER_HOME (the store `fewer run` needs inside Claude's commands, when install was
/// given one).
pub fn lean_plan(settings: &Json, home: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    // (the developer's own agent stays the main one: the lean agent is not used, so fewer's own
    // MCP server stays on too, as their agent may use it)
    let own_agent = settings.get("agent").is_some_and(|a| a.str() != Some(LEAN_AGENT));
    if settings.get("agent").is_none() {
        v.push("agent".to_string());
    }
    let env = settings.get("env");
    for (k, _) in LEAN_ENV {
        if env.and_then(|e| e.get(k)).is_none() {
            v.push(format!("env:{k}"));
        }
    }
    if home.is_some() && env.and_then(|e| e.get("FEWER_HOME")).is_none() {
        v.push("env:FEWER_HOME".to_string());
    }
    if own_agent {
        return v;
    }
    // fewer's own MCP server off (its tools are fewer's commands), unless the developer's deny
    // list is not a list, or names it already
    match settings.get("deniedMcpServers") {
        None => v.push("deny-own".to_string()),
        Some(Json::Arr(a)) if !a.contains(&deny_own()) => v.push("deny-own".to_string()),
        _ => {}
    }
    v
}

/// fewer's reading commands, allowed with fewer on as Claude Code's own Read is: they only read,
/// and keep the user's Read deny and ask rules and the project's bounds themselves (perms.rs), so
/// they ask nothing Claude Code's Read would not ask.
pub const READS: [&str; 6] = ["read", "table", "look", "changes", "tool", "skill"];

/// The allow rules for them, fewer named as the rules name it (`word`).
pub fn read_rules(word: &str) -> Vec<String> {
    READS.iter().map(|c| format!("Bash({word} {c}:*)")).collect()
}

/// The reads' rules fewer would add to `permissions.allow` now: those not there yet (none with
/// the developer's own agent, or with `permissions` or its `allow` not as Claude Code reads them).
pub fn reads_plan(settings: &Json, word: &str) -> Vec<String> {
    // (a path with a comma or a quote: the record is comma-separated, and the rule would not name
    // the command as Claude writes it; Claude Code asks then, as before)
    if settings.get("agent").is_some_and(|a| a.str() != Some(LEAN_AGENT)) || word.contains([',', '\'', '"', '(', ')']) {
        return Vec::new();
    }
    let have: Vec<String> = match settings.get("permissions") {
        None => Vec::new(),
        Some(Json::Obj(_)) => match settings.get("permissions").and_then(|p| p.get("allow")) {
            None => Vec::new(),
            Some(Json::Arr(a)) => a.iter().filter_map(Json::str).map(str::to_string).collect(),
            Some(_) => return Vec::new(),
        },
        Some(_) => return Vec::new(),
    };
    read_rules(word).into_iter().filter(|r| !have.contains(r)).map(|r| format!("allow:{r}")).collect()
}

/// The parts of `record` (savers.lean) that this version no longer writes (RETIRED), and the
/// rest: (retired, kept), each a record.
pub fn retired_parts(record: &str) -> (String, String) {
    let (old, kept): (Vec<&str>, Vec<&str>) = record.split(',').filter(|p| !p.is_empty()).partition(|p| RETIRED.contains(p));
    (old.join(","), kept.join(","))
}

/// Whether fewer keeps fewer's own MCP server out of sight in these settings: an early version's allow
/// list (Claude Code then runs no MCP server at all) or this version's deny entry. Either way
/// `claude mcp get fewer` says there is none and `claude mcp add fewer` is refused as "blocked by
/// enterprise policy", while `claude mcp remove` still works (2.1.287, 2.1.332).
pub fn hides_own_server(settings: &Json) -> bool {
    matches!(settings.get("allowedMcpServers"), Some(Json::Arr(a)) if a.iter().any(is_allow_none))
        || matches!(settings.get("deniedMcpServers"), Some(Json::Arr(a)) if a.contains(&deny_own()))
}

/// Whether settings hold what only fewer writes: its agent as the main one, its entry
/// in `allowedMcpServers` (an early version), or FEWER_HOME in `env`.
pub fn marked(settings: &Json) -> bool {
    settings.get("agent").and_then(Json::str) == Some(LEAN_AGENT)
        || matches!(settings.get("allowedMcpServers"), Some(Json::Arr(a)) if a.iter().any(is_allow_none))
        || settings.get("env").and_then(|e| e.get("FEWER_HOME")).is_some()
}

/// The records of everything fewer may have written (the smaller start's and the lean
/// mode's), for settings whose record is gone: each part is taken back only at fewer's own value
/// (the reads' rules naming fewer by its name, or as `word` names it now).
pub fn full_records(word: &str) -> (String, String) {
    let mut lean: Vec<String> = vec!["lean:agent".into()];
    lean.extend(LEAN_ENV.iter().chain(OLD_LEAN_ENV.iter()).map(|(k, _)| format!("lean:env:{k}")));
    lean.extend(["lean:env:FEWER_HOME", "lean:connectors", "lean:deny-web", "lean:allow-none", "lean:deny-own", "lean:window"].map(String::from));
    for w in if word == "fewer" { vec!["fewer"] } else { vec!["fewer", word] } {
        lean.extend(read_rules(w).into_iter().map(|r| format!("lean:allow:{r}")));
    }
    ("workflows,artifact".to_string(), lean.join(","))
}

/// fewer's entry in `deniedMcpServers`: fewer's own MCP server.
pub fn deny_own() -> Json {
    Json::Obj(vec![("serverName".to_string(), Json::from(OWN_SERVER))])
}

/// fewer's one entry in `allowedMcpServers`.
pub fn allow_none() -> Json {
    Json::Obj(vec![("serverName".to_string(), Json::from(ALLOW_NONE))])
}

/// fewer's entry in `deniedMcpServers`.
pub fn deny_web() -> Json {
    Json::Obj(vec![("serverUrl".to_string(), Json::from(DENY_WEB))])
}

/// Set fewer as planned; the window goes to LEAN_WINDOW when it is fewer's own (or unset),
/// never when the developer set one before (`theirs`: kept as it is, even at a value fewer
/// once wrote). The record of what was put there.
pub fn add_lean(settings: &mut Json, plan: &[String], home: Option<&str>, theirs: bool) -> Result<String, String> {
    let mut after = settings.clone();
    let Json::Obj(top) = &mut after else { return Err("it does not hold a JSON object".into()) };
    let mut record = Vec::new();
    for part in plan {
        if part == "agent" {
            top.push(("agent".to_string(), Json::from(LEAN_AGENT)));
            record.push("lean:agent".to_string());
        } else if let Some(k) = part.strip_prefix("env:") {
            let v = if k == "FEWER_HOME" { home.unwrap_or("").to_string() } else { LEAN_ENV.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()).unwrap_or_default() };
            let Json::Obj(env) = field(top, "env", Json::Obj(Vec::new())) else { return Err("its \"env\" is not an object".into()) };
            env.push((k.to_string(), Json::from(v.as_str())));
            record.push(format!("lean:env:{k}"));
        } else if part == "connectors" {
            top.push(("disableClaudeAiConnectors".to_string(), Json::Bool(true)));
            record.push("lean:connectors".to_string());
        } else if part == "deny-web" {
            let Json::Arr(list) = field(top, "deniedMcpServers", Json::Arr(Vec::new())) else { return Err("its \"deniedMcpServers\" is not a list".into()) };
            list.push(deny_web());
            record.push("lean:deny-web".to_string());
        } else if part == "allow-none" {
            top.push(("allowedMcpServers".to_string(), Json::Arr(vec![allow_none()])));
            record.push("lean:allow-none".to_string());
        } else if part == "deny-own" {
            let Json::Arr(list) = field(top, "deniedMcpServers", Json::Arr(Vec::new())) else { return Err("its \"deniedMcpServers\" is not a list".into()) };
            list.push(deny_own());
            record.push("lean:deny-own".to_string());
        } else if let Some(rule) = part.strip_prefix("allow:") {
            let Json::Obj(perms) = field(top, "permissions", Json::Obj(Vec::new())) else { return Err("its \"permissions\" is not an object".into()) };
            let Json::Arr(list) = field(perms, "allow", Json::Arr(Vec::new())) else { return Err("its \"permissions.allow\" is not a list".into()) };
            list.push(Json::from(rule));
            record.push(format!("lean:allow:{rule}"));
        }
    }
    // the window: fewer's own (or none) becomes the lean one; the developer's stays
    let ours = |v: &Json| !theirs && std::iter::once(WINDOW).chain(OLD_WINDOWS).any(|w| *v == Json::Num(w.to_string()));
    match top.iter().position(|(k, _)| k == WINDOW_KEY) {
        Some(i) if ours(&top[i].1) => {
            top[i].1 = Json::Num(LEAN_WINDOW.to_string());
            record.push("lean:window".to_string());
        }
        None => {
            top.push((WINDOW_KEY.to_string(), Json::Num(LEAN_WINDOW.to_string())));
            record.push("lean:window".to_string());
        }
        _ => {}
    }
    *settings = after;
    Ok(record.join(","))
}

/// Take back what the record says fewer put there, while it is still as fewer set it.
pub fn remove_lean(settings: &mut Json, record: &str) -> Vec<String> {
    let mut said = Vec::new();
    let Json::Obj(top) = settings else { return said };
    for part in record.split(',').filter_map(|p| p.strip_prefix("lean:")) {
        match part {
            "agent" => {
                if let Some(i) = top.iter().position(|(k, v)| k == "agent" && v.str() == Some(LEAN_AGENT)) {
                    top.remove(i);
                    said.push("agent removed".into());
                }
            }
            "window" => {
                if let Some(i) = top.iter().position(|(k, v)| k == WINDOW_KEY && *v == Json::Num(LEAN_WINDOW.to_string())) {
                    top.remove(i);
                    said.push(format!("{WINDOW_KEY} removed"));
                }
            }
            "connectors" => {
                if let Some(i) = top.iter().position(|(k, v)| k == "disableClaudeAiConnectors" && *v == Json::Bool(true)) {
                    top.remove(i);
                    said.push("disableClaudeAiConnectors removed".into());
                }
            }
            "allow-none" => {
                // (fewer's entry alone taken out: add-ons the developer allowed while fewer was
                // on stay allowed, and the list, now theirs, is said)
                if let Some(i) = top.iter().position(|(k, _)| k == "allowedMcpServers")
                    && let Json::Arr(list) = &mut top[i].1
                    && list.iter().any(is_allow_none)
                {
                    // (every copy of it: a list merged twice holds it twice)
                    list.retain(|e| !is_allow_none(e));
                    if list.is_empty() {
                        top.remove(i);
                        said.push("allowedMcpServers removed".into());
                    } else {
                        said.push(format!("allowedMcpServers: fewer's entry removed; the {} add-on(s) you allowed while fewer was on stay on the list, so Claude Code connects only those", list.len()));
                    }
                }
            }
            "deny-web" | "deny-own" => {
                let (entry, shown) = if part == "deny-web" { (deny_web(), DENY_WEB) } else { (deny_own(), OWN_SERVER) };
                if let Some(i) = top.iter().position(|(k, _)| k == "deniedMcpServers")
                    && let Json::Arr(list) = &mut top[i].1
                    && let Some(j) = list.iter().position(|e| *e == entry)
                {
                    list.remove(j);
                    said.push(format!("deniedMcpServers {shown} removed"));
                    if list.is_empty() {
                        top.remove(i);
                    }
                }
            }
            p if p.starts_with("allow:") => {
                let rule = &p["allow:".len()..];
                if let Some(i) = top.iter().position(|(k, _)| k == "permissions")
                    && let Json::Obj(perms) = &mut top[i].1
                    && let Some(j) = perms.iter().position(|(k, _)| k == "allow")
                    && let Json::Arr(list) = &mut perms[j].1
                    && let Some(x) = list.iter().position(|e| e.str() == Some(rule))
                {
                    list.remove(x);
                    said.push(format!("permissions.allow {rule} removed"));
                    if list.is_empty() {
                        perms.remove(j);
                    }
                    if perms.is_empty() {
                        top.remove(i);
                    }
                }
            }
            p => {
                if let Some(k) = p.strip_prefix("env:")
                    && let Some(i) = top.iter().position(|(n, _)| n == "env")
                    && let Json::Obj(env) = &mut top[i].1
                {
                    let want = LEAN_ENV.iter().chain(OLD_LEAN_ENV.iter()).find(|(n, _)| *n == k).map(|(_, v)| *v);
                    if let Some(j) = env.iter().position(|(n, v)| n == k && (want.is_none() || v.str() == want)) {
                        env.remove(j);
                        said.push(format!("{k} removed"));
                    }
                    if env.is_empty() {
                        top.remove(i);
                    }
                }
            }
        }
    }
    said
}

#[cfg(test)]
mod lean_tests {
    use super::*;

    #[test]
    fn the_slash_commands_written_and_taken_out() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-slash", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        write_slash(&d, None, "/opt/my tools/fewer").unwrap();
        let t = fs::read_to_string(d.join("commands").join("fewer-tokens.md")).unwrap();
        // only the person runs it (Claude is not told about it); it runs exactly this command, then
        // Claude shows the answer as it is
        assert_eq!(t, "---\ndescription: fewer: how many tokens you saved\nallowed-tools: Bash('/opt/my tools/fewer' tokens)\ndisable-model-invocation: true\n---\n<!-- fewer's slash command: fewer uninstall takes it out -->\n!`'/opt/my tools/fewer' tokens`\n\nThe lines above are what `fewer tokens` said. Show them to the user as they are, in a code block, and add nothing.\n");
        let names: Vec<String> = fs::read_dir(d.join("commands")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        assert_eq!(names, ["fewer-memory.md", "fewer-off.md", "fewer-on.md", "fewer-tokens.md"]);
        assert!(fs::read_to_string(d.join("commands").join("fewer-memory.md")).unwrap().contains("!`'/opt/my tools/fewer' memory --total`"));
        // the store named, as a hook's command names it
        write_slash(&d, Some("/w/store"), "/opt/fewer").unwrap();
        assert!(fs::read_to_string(d.join("commands").join("fewer-off.md")).unwrap().contains("allowed-tools: Bash(FEWER_HOME=/w/store /opt/fewer off)\n"));
        // someone else's command of the same name: left alone, said, nothing written
        fs::write(d.join("commands").join("fewer-on.md"), "mine").unwrap();
        assert!(write_slash(&d, None, "/opt/fewer").unwrap_err().contains("fewer-on.md is someone else's command"));
        // taken out: ours only; the folder stays while something else is in it
        assert!(remove_slash(&d));
        assert_eq!(fs::read_to_string(d.join("commands").join("fewer-on.md")).unwrap(), "mine");
        fs::remove_file(d.join("commands").join("fewer-on.md")).unwrap();
        write_slash(&d, None, "/opt/fewer").unwrap();
        assert!(remove_slash(&d) && !d.join("commands").exists() && !remove_slash(&d));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_lean_mode_added_and_taken_back() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-lean", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        // the agents' files: written, rewritten, removed; someone else's is left alone
        write_lean_agent(&d, "fewer").unwrap();
        let text = fs::read_to_string(lean_agent_file(&d)).unwrap();
        assert!(text.starts_with("---\nname: fewer-lean\n") && text.contains(&format!("disallowedTools: {LEAN_LEAVES_OUT}\n---\n")) && text.contains("ending in ` 2>&1 || true`") && text.contains("an add-on (MCP server) or a claude.ai connector: its tools, as usual (ToolSearch loads them)") && !text.contains("mcp list") && text.contains("`fewer helper \"<job 1>\" \"<job 2>\"`"), "{text}");
        let wf = fs::read_to_string(workflow_agent_file(&d)).unwrap();
        assert!(wf.starts_with("---\nname: fewer-lean-workflow\n") && wf.contains("tools: Bash, Read, Edit, Write, Workflow, Skill\n") && wf.contains("agentType: 'fewer-worker'") && wf.contains("Load no skill"), "{wf}");
        let wk = fs::read_to_string(worker_agent_file(&d)).unwrap();
        assert!(wk.starts_with("---\nname: fewer-worker\n") && wk.contains("tools: Bash, Read, Edit, Write\nomitClaudeMd: true\n---\n") && wk.contains("You are a helper of Claude Code doing one job") && !wk.contains("mcp ask") && wk.contains("`fewer changes [REV]`") && wk.contains("`fewer read FILE...`") && wk.contains("never write a { with a quote before its } ({'a': 1}, f\"{', '.join(x)}\"): Claude Code asks about such a command") && wk.contains("keep quotes out of an f-string's braces (sep = ', ' then f\"{sep.join(x)}\")") && wk.contains("no $(...) in a command"), "{wk}");
        assert!(wf.contains("tell the agent to read all of them with one `fewer read FILE...`"), "{wf}");
        assert!(agents_there(&d, "fewer") && !agents_there(&d, "/opt/x/fewer"));
        write_lean_agent(&d, "'/opt/my tools/fewer'").unwrap();
        assert!(fs::read_to_string(lean_agent_file(&d)).unwrap().contains("`'/opt/my tools/fewer' changes [REV]`"));
        assert!(remove_lean_agent(&d) && !lean_agent_file(&d).exists() && !workflow_agent_file(&d).exists() && !worker_agent_file(&d).exists());
        fs::write(lean_agent_file(&d), "---\nname: fewer-lean\n---\nmine\n").unwrap();
        assert!(write_lean_agent(&d, "fewer").unwrap_err().contains("someone else's agent"));
        assert!(!workflow_agent_file(&d).exists(), "nothing written when one file is someone else's");
        assert!(!remove_lean_agent(&d) && lean_agent_file(&d).exists());
        // how the rules name fewer: its name when PATH finds this one, else its path
        let bin = d.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let me = bin.join(format!("fewer{}", std::env::consts::EXE_SUFFIX));
        fs::write(&me, "#!/bin/sh\n").unwrap();
        let me = me.to_str().unwrap();
        assert_eq!(command_word(me, Some(bin.as_os_str())), "fewer");
        // (Windows: its path with / in double quotes)
        let me_path = if cfg!(windows) { format!("\"{}\"", me.replace('\\', "/")) } else { me.to_string() };
        assert_eq!(command_word(me, Some(std::ffi::OsStr::new("/nowhere"))), me_path);
        assert_eq!(command_word("/opt/my tools/fewer", None), "'/opt/my tools/fewer'");
        // the settings: the developer's own values stay; ours come back out
        let theirs = r#"{"env":{"ENABLE_CLAUDEAI_MCP_SERVERS":"true"},"autoCompactWindow":120000}"#;
        let mut s = json::parse(theirs.as_bytes()).unwrap();
        let plan = lean_plan(&s, Some("/w/store"));
        // (the developer's add-ons and claude.ai's connectors stay as they are: fewer's own server
        // alone is kept off)
        assert_eq!(plan, ["agent", "env:CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT", "env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "env:CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "env:FEWER_HOME", "deny-own"]);
        let record = add_lean(&mut s, &plan, Some("/w/store"), false).unwrap();
        assert_eq!(record, "lean:agent,lean:env:CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT,lean:env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE,lean:env:CLAUDE_CODE_TOTAL_TOKENS_REMINDER,lean:env:FEWER_HOME,lean:deny-own,lean:window");
        assert_eq!(s.to_string(), r#"{"env":{"ENABLE_CLAUDEAI_MCP_SERVERS":"true","CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT":"0","CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1","CLAUDE_CODE_TOTAL_TOKENS_REMINDER":"off","FEWER_HOME":"/w/store"},"autoCompactWindow":100000,"agent":"fewer-lean","deniedMcpServers":[{"serverName":"fewer"}]}"#);
        assert!(lean_plan(&s, Some("/w/store")).is_empty() && hides_own_server(&s));
        let said = remove_lean(&mut s, &record);
        assert_eq!(said.len(), 7, "{said:?}");
        assert_eq!(s.to_string(), r#"{"env":{"ENABLE_CLAUDEAI_MCP_SERVERS":"true"}}"#);
        // the developer's own deny list: ours added at its end and taken back alone
        let mut d2 = json::parse(br#"{"deniedMcpServers":[{"serverName":"x"}]}"#).unwrap();
        let plan = lean_plan(&d2, None);
        assert_eq!(plan, ["agent", "env:CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT", "env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "env:CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "deny-own"]);
        let record = add_lean(&mut d2, &plan, None, false).unwrap();
        assert_eq!(d2.to_string(), r#"{"deniedMcpServers":[{"serverName":"x"},{"serverName":"fewer"}],"agent":"fewer-lean","env":{"CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT":"0","CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1","CLAUDE_CODE_TOTAL_TOKENS_REMINDER":"off"},"autoCompactWindow":100000}"#);
        let said = remove_lean(&mut d2, &record);
        assert!(said.contains(&"deniedMcpServers fewer removed".to_string()), "{said:?}");
        assert_eq!(d2.to_string(), r#"{"deniedMcpServers":[{"serverName":"x"}]}"#);
        // a deny list that is not a list, or one that names fewer already: left alone
        let odd = json::parse(br#"{"deniedMcpServers":"x","disableClaudeAiConnectors":false,"allowedMcpServers":[{"serverName":"mine"}]}"#).unwrap();
        assert!(!lean_plan(&odd, None).iter().any(|p| p == "deny-own" || p == "deny-web" || p == "connectors" || p == "allow-none"));
        let named = json::parse(br#"{"deniedMcpServers":[{"serverName":"fewer"}]}"#).unwrap();
        assert!(!lean_plan(&named, None).iter().any(|p| p == "deny-own"));
        // an add-on the developer allowed while fewer was on: fewer's entry alone taken out,
        // the list said
        let mut d3 = json::parse(br#"{"model":"opus","allowedMcpServers":[{"serverName":"cmlog-10x: add-ons through cmlog-search"},{"serverName":"github"}]}"#).unwrap();
        let said = remove_lean(&mut d3, "lean:allow-none");
        assert_eq!(d3.to_string(), r#"{"model":"opus","allowedMcpServers":[{"serverName":"github"}]}"#);
        assert!(said.len() == 1 && said[0].contains("stay on the list"), "{said:?}");
        // fewer's entry twice (a list merged twice): both taken out, nothing left said as the
        // developer's
        let mut d4 = json::parse(br#"{"allowedMcpServers":[{"serverName":"cmlog-10x: add-ons through cmlog-search"},{"serverName":"cmlog-10x: add-ons through cmlog-search"}]}"#).unwrap();
        assert_eq!(remove_lean(&mut d4, "lean:allow-none"), ["allowedMcpServers removed"]);
        assert_eq!(d4.to_string(), "{}");
        // an earlier version's CLAUDE.md switch is taken back too
        let mut old = json::parse(br#"{"env":{"CLAUDE_CODE_DISABLE_CLAUDE_MDS":"1"}}"#).unwrap();
        assert_eq!(remove_lean(&mut old, "lean:env:CLAUDE_CODE_DISABLE_CLAUDE_MDS"), ["CLAUDE_CODE_DISABLE_CLAUDE_MDS removed"]);
        assert_eq!(old.to_string(), "{}");
        // a window the developer set is theirs
        let mut w = json::parse(br#"{"autoCompactWindow":300000}"#).unwrap();
        let r = add_lean(&mut w, &[], None, false).unwrap();
        // the developer's own window, even at a value fewer once wrote: kept, not recorded
        let mut mine = json::parse(br#"{"autoCompactWindow":120000}"#).unwrap();
        assert_eq!(add_lean(&mut mine, &[], None, true).unwrap(), "");
        assert_eq!(mine.to_string(), r#"{"autoCompactWindow":120000}"#);
        assert_eq!((r.as_str(), w.to_string().as_str()), ("", r#"{"autoCompactWindow":300000}"#));
        fs::remove_dir_all(&d).unwrap();
    }
}
