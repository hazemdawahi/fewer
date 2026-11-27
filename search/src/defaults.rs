//! Where fewer finds things when a FEWER_ setting is not given: the memory
//! store ~/.fewer, Claude Code's folder ~/.claude (or $CLAUDE_CONFIG_DIR) and its config file
//! ~/.claude.json, the `claude` program on PATH, the system's opener for a page (`open`,
//! `xdg-open`), each agent's own folder where it is. In development (FEWER_DEV set, as the checks
//! set it, and every unit test) a setting comes from its variable only, so that no check or test
//! ever reads or writes the real home folder, or starts the real claude or a browser by itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Whether fewer runs in development: FEWER_DEV set, or a unit test.
pub fn dev() -> bool {
    cfg!(test) || var("FEWER_DEV").is_some()
}

fn var(k: &str) -> Option<OsString> {
    std::env::var_os(k).filter(|v| !v.is_empty())
}

/// A setting: its variable, else (not in development) its usual value on this machine.
pub fn setting(k: &str) -> Option<OsString> {
    var(k).or_else(|| if dev() { None } else { usual(k) })
}

/// The usual value of a setting (None: none known, or an agent's folder not there).
fn usual(k: &str) -> Option<OsString> {
    if k == "FEWER_CLAUDE_BIN" {
        // (a name: looked for on PATH, as a shell finds it)
        return Some("claude".into());
    }
    let home = var("HOME").map(PathBuf::from).filter(|h| h.is_absolute())?;
    let claude = var("CLAUDE_CONFIG_DIR").map(PathBuf::from).filter(|d| d.is_absolute());
    let p = match k {
        "FEWER_HOME" => home.join(".fewer"),
        "FEWER_CLAUDE_DIR" => claude.unwrap_or_else(|| home.join(".claude")),
        // (an agent's folder only where it is: the sessions question names the agents found)
        "FEWER_CODEX_DIR" => var("CODEX_HOME").map(PathBuf::from).filter(|d| d.is_absolute()).unwrap_or_else(|| home.join(".codex")),
        "FEWER_CLINE_DIR" => editor_storage(&home, "Code").join("saoudrizwan.claude-dev"),
        "FEWER_CURSOR_DIR" => editor_storage(&home, "Cursor"),
        "FEWER_AIDER_DIR" => {
            let root = crate::store::project_root(&std::env::current_dir().ok()?);
            return root.join(".aider.chat.history.md").is_file().then(|| root.into_os_string());
        }
        _ => return None,
    };
    if matches!(k, "FEWER_CODEX_DIR" | "FEWER_CLINE_DIR" | "FEWER_CURSOR_DIR") && !p.is_dir() {
        return None;
    }
    Some(p.into_os_string())
}

/// An editor's globalStorage folder (VS Code's, Cursor's), where its extensions keep their data.
fn editor_storage(home: &Path, app: &str) -> PathBuf {
    let base = if cfg!(target_os = "macos") { home.join("Library/Application Support") } else { var("XDG_CONFIG_HOME").map(PathBuf::from).filter(|d| d.is_absolute()).unwrap_or_else(|| home.join(".config")) };
    base.join(app).join("User").join("globalStorage")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_has_no_usual_places() {
        // (every unit test runs as development: no setting comes from the real home folder)
        assert!(dev());
        if std::env::var_os("FEWER_HOME").is_none() {
            assert_eq!(setting("FEWER_HOME"), None);
        }
        assert_eq!(setting("FEWER_NOT_A_SETTING"), None);
    }
}
