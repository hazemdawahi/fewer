//! A readable view of an agent session's lines (the token test on text that is not logs,
//! 2026-11-23): a Claude Code session is JSON lines whose first few hundred bytes are ids,
//! folders and versions, so a line cut at line_max showed none of what was said or run. The
//! view shows who said or did what instead: "user: …", "assistant: … | Bash: npm test",
//! "result (error): …", "summary: …", "system: …". It is a view, not the line's bytes: answers
//! say so, and `lines` with raw: true shows the JSON.

use crate::json::{self, Json};

/// A Claude Code session line as who said or did what, or None when it is not one of the
/// kinds above (then the line is shown as it is).
pub fn session_view(line: &[u8]) -> Option<String> {
    let start = line.iter().position(|&c| c == b'{')?;
    let j = json::parse(&line[start..]).ok()?;
    let kind = j.get("type").and_then(Json::str)?;
    let msg = j.get("message");
    let content = msg.and_then(|m| m.get("content"));
    let s = match kind {
        "summary" => format!("summary: {}", j.get("summary").and_then(Json::str)?),
        "user" => match content? {
            Json::Str(t) => format!("user: {t}"),
            Json::Arr(parts) => {
                let mut out = Vec::new();
                for p in parts {
                    match p.get("type").and_then(Json::str) {
                        Some("tool_result") => {
                            let error = p.get("is_error").and_then(Json::bool).unwrap_or(false);
                            out.push(format!("result{}: {}", if error { " (error)" } else { "" }, text_of(p.get("content"))));
                        }
                        Some("text") => out.push(format!("user: {}", p.get("text").and_then(Json::str).unwrap_or(""))),
                        Some("image") => out.push("user: [image]".into()),
                        _ => {}
                    }
                }
                if out.is_empty() {
                    return None;
                }
                out.join(" | ")
            }
            _ => return None,
        },
        "assistant" => {
            let mut out = Vec::new();
            for p in match content? {
                Json::Arr(parts) => parts.as_slice(),
                Json::Str(t) => return Some(one_line(&format!("assistant: {t}"))),
                _ => return None,
            } {
                match p.get("type").and_then(Json::str) {
                    Some("text") => out.push(p.get("text").and_then(Json::str).unwrap_or("").to_string()),
                    Some("tool_use") => out.push(tool_use(p)),
                    _ => {}
                }
            }
            if out.is_empty() {
                return None;
            }
            format!("assistant: {}", out.join(" | "))
        }
        "system" => match j.get("content") {
            Some(Json::Str(t)) => format!("system: {t}"),
            _ => format!("system: {}", j.get("subtype").and_then(Json::str).unwrap_or("")),
        },
        _ => return None,
    };
    Some(one_line(&s))
}

/// What a Claude Code session did, from its lines: the commands it ran (with whether each
/// failed), the files it edited (first edit first), and the time of its first and last line.
#[derive(Default, Debug, PartialEq)]
pub struct Facts {
    pub commands: Vec<(String, bool)>,
    pub files: Vec<String>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub replies: u64,
}

/// The facts of a session's lines (history's start card and `session`, 2026-11-23).
pub fn facts(text: &[u8]) -> Facts {
    let mut f = Facts::default();
    // tool_use id -> the command's index, for the results that say it failed
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for l in text.split(|&c| c == b'\n') {
        let Some(start) = l.iter().position(|&c| c == b'{') else { continue };
        let Ok(j) = json::parse(&l[start..]) else { continue };
        if let Some(t) = j.get("timestamp").and_then(Json::str) {
            if f.first.is_none() {
                f.first = Some(t.to_string());
            }
            f.last = Some(t.to_string());
        }
        let Some(Json::Arr(parts)) = j.get("message").and_then(|m| m.get("content")) else { continue };
        let assistant = j.get("type").and_then(Json::str) == Some("assistant");
        if assistant {
            f.replies += 1;
        }
        for p in parts {
            match p.get("type").and_then(Json::str) {
                Some("tool_use") => {
                    let name = p.get("name").and_then(Json::str).unwrap_or("");
                    let input = p.get("input");
                    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Json::str).map(str::to_string);
                    match name {
                        "Bash" => {
                            if let Some(c) = field("command") {
                                if let Some(id) = p.get("id").and_then(Json::str) {
                                    at.insert(id.to_string(), f.commands.len());
                                }
                                f.commands.push((one_line(&c), false));
                            }
                        }
                        "Edit" | "Write" | "MultiEdit" => {
                            if let Some(path) = field("file_path").filter(|x| !f.files.contains(x)) {
                                f.files.push(path);
                            }
                        }
                        "NotebookEdit" => {
                            if let Some(path) = field("notebook_path").filter(|x| !f.files.contains(x)) {
                                f.files.push(path);
                            }
                        }
                        _ => {}
                    }
                }
                Some("tool_result") if p.get("is_error").and_then(Json::bool) == Some(true) => {
                    if let Some(&i) = p.get("tool_use_id").and_then(Json::str).and_then(|id| at.get(id)) {
                        f.commands[i].1 = true;
                    }
                }
                _ => {}
            }
        }
    }
    f
}

/// A tool call as its name and what it was given: the command, the file, the pattern.
fn tool_use(p: &Json) -> String {
    let name = p.get("name").and_then(Json::str).unwrap_or("tool");
    let inp = p.get("input");
    let field = |k: &str| inp.and_then(|i| i.get(k)).and_then(Json::str);
    let arg = match name {
        "Bash" => field("command").map(str::to_string),
        "Read" | "Write" | "Edit" | "MultiEdit" => field("file_path").map(str::to_string),
        "NotebookEdit" => field("notebook_path").map(str::to_string),
        "Grep" => field("pattern").map(|p| match field("path") {
            Some(path) => format!("{p} in {path}"),
            None => p.to_string(),
        }),
        "Glob" => field("pattern").map(str::to_string),
        "WebFetch" => field("url").map(str::to_string),
        "WebSearch" => field("query").map(str::to_string),
        "Task" | "Agent" => field("description").map(str::to_string),
        _ => None,
    };
    let arg = arg.unwrap_or_else(|| inp.map(Json::to_string).unwrap_or_default());
    format!("{name}: {arg}")
}

/// A tool result's content as text: a string, or the text parts of a list.
fn text_of(c: Option<&Json>) -> String {
    match c {
        Some(Json::Str(t)) => t.clone(),
        Some(Json::Arr(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Json::str) {
                Some("text") => p.get("text").and_then(Json::str).unwrap_or("").to_string(),
                Some("image") => "[image]".to_string(),
                _ => String::new(),
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// One line: each line end written as \n, tabs as spaces.
fn one_line(s: &str) -> String {
    s.replace("\r\n", "\\n").replace(['\n', '\r'], "\\n").replace('\t', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(o: &str) -> Vec<u8> {
        o.as_bytes().to_vec()
    }

    #[test]
    fn who_said_or_did_what() {
        let pre = r#"{"parentUuid":null,"isSidechain":false,"userType":"external","cwd":"/w","sessionId":"s","version":"2.0.31","gitBranch":"main","#;
        let user = line(&format!(r#"{pre}"message":{{"role":"user","content":"why does the\nlogin fail?"}},"type":"user","uuid":"u1"}}"#));
        assert_eq!(session_view(&user).as_deref(), Some("user: why does the\\nlogin fail?"));
        let asst = line(&format!(
            r#"{pre}"message":{{"id":"m","type":"message","role":"assistant","model":"x","content":[{{"type":"thinking","thinking":"hm","signature":"s"}},{{"type":"text","text":"Let me run it."}},{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"npm test","description":"Run"}}}}],"usage":{{"input_tokens":3}}}},"type":"assistant"}}"#
        ));
        assert_eq!(session_view(&asst).as_deref(), Some("assistant: Let me run it. | Bash: npm test"));
        let res = line(&format!(r#"{pre}"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"FAIL\tx\n2 failed","is_error":true}}]}},"type":"user","toolUseResult":{{"stdout":"FAIL"}}}}"#));
        assert_eq!(session_view(&res).as_deref(), Some("result (error): FAIL x\\n2 failed"));
        let parts = line(&format!(r#"{pre}"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t2","content":[{{"type":"text","text":"ok"}},{{"type":"image"}}]}}]}},"type":"user"}}"#));
        assert_eq!(session_view(&parts).as_deref(), Some("result: ok [image]"));
        let edit = line(&format!(r#"{pre}"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t3","name":"Edit","input":{{"file_path":"/w/src/a.rs","old_string":"a","new_string":"b"}}}}]}},"type":"assistant"}}"#));
        assert_eq!(session_view(&edit).as_deref(), Some("assistant: Edit: /w/src/a.rs"));
        let other = line(&format!(r#"{pre}"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t4","name":"Mystery","input":{{"k":1}}}}]}},"type":"assistant"}}"#));
        assert_eq!(session_view(&other).as_deref(), Some(r#"assistant: Mystery: {"k":1}"#));
        assert_eq!(session_view(br#"{"type":"summary","summary":"Fix the flaky test","leafUuid":"x"}"#).as_deref(), Some("summary: Fix the flaky test"));
        assert_eq!(session_view(br#"{"type":"system","subtype":"informational","content":"Compacted"}"#).as_deref(), Some("system: Compacted"));
        // not a session line, or one with nothing to show: None (shown as it is)
        assert_eq!(session_view(b"plain text"), None);
        assert_eq!(session_view(br#"{"type":"file-history-snapshot","snapshot":{}}"#), None);
        assert_eq!(session_view(br#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"x"}]}}"#), None);
    }

    #[test]
    fn what_a_session_did() {
        let t = concat!(
            r#"{"type":"user","timestamp":"2026-11-20T14:00:01Z","message":{"role":"user","content":"fix it"}}"#, "\n",
            r#"{"type":"assistant","timestamp":"2026-11-20T14:00:02Z","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"npm test"}}]}}"#, "\n",
            r#"{"type":"user","timestamp":"2026-11-20T14:00:09Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"FAIL","is_error":true}]}}"#, "\n",
            r#"{"type":"assistant","timestamp":"2026-11-20T14:01:00Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/w/src/a.ts"}},{"type":"tool_use","id":"t3","name":"Write","input":{"file_path":"/w/src/a.ts"}}]}}"#, "\n",
            r#"{"type":"assistant","timestamp":"2026-11-20T14:02:00Z","message":{"content":[{"type":"tool_use","id":"t4","name":"Bash","input":{"command":"npm test\n  --watch=false"}}]}}"#, "\n",
            r#"{"type":"user","timestamp":"2026-11-20T14:02:30Z","message":{"content":[{"type":"tool_result","tool_use_id":"t4","content":"ok"}]}}"#, "\n",
        );
        let f = facts(t.as_bytes());
        assert_eq!(f.commands, [("npm test".to_string(), true), ("npm test\\n  --watch=false".to_string(), false)]);
        assert_eq!(f.files, ["/w/src/a.ts"]);
        assert_eq!((f.first.as_deref(), f.last.as_deref(), f.replies), (Some("2026-11-20T14:00:01Z"), Some("2026-11-20T14:02:30Z"), 3));
    }
}
