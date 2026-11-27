//! Claude Code's permission rules, kept by fewer's own commands, so that permissions work as
//! usual. With fewer on, Claude reads files with `fewer read`, `fewer table`
//! and `fewer look`, and runs tests and builds with `fewer run`, all through Bash: Claude Code's
//! Read rules never see those files, and its Bash rules see `fewer run ...`, not the command
//! inside. So these commands check the developer's rules themselves, deny first:
//! - a file a Read deny rule names is not read; one a Read ask rule names is not read either:
//!   Claude reads it with Claude Code's Read tool, which asks the developer as usual;
//! - a command a Bash deny rule matches is not run by `fewer run`; one an ask rule matches is
//!   not run either: Claude runs it itself, without fewer run, so Claude Code asks as usual.
//!
//! The rules come from the settings Claude Code reads: the managed settings (FEWER_MANAGED_DIR,
//! else macOS /Library/Application Support/ClaudeCode, Linux /etc/claude-code), the user's (Claude Code's
//! folder: FEWER_CLAUDE_DIR, else its usual place), and the project's (.claude/settings.json and
//! settings.local.json, in the current folder and every folder above it). How a rule matches
//! follows Claude Code's documentation:
//! - Read(PATTERN): gitignore patterns. `//path` is absolute, `~/path` from the home folder,
//!   `/path` from the folder that holds the settings file's `.claude` (a project's root), `./path`
//!   and `path` from the current folder; a pattern with no slash but at its end matches at any
//!   depth; `*` takes no slash, `**` any number of folders, `?` one character. A rule that names
//!   a folder covers what is in it. Bare `Read` is every file.
//! - Bash(COMMAND): `cmd:*` and `cmd *` a command that is cmd or starts with cmd and a space;
//!   another `*` stands for any text; else the whole command. Bare `Bash` (or `Bash(*)`) is every
//!   command. A shell's `-c` script is checked part by part (&&, ||, ;, |, a new line), and so
//!   is a command after env, nohup, nice, time or timeout.
//!
//! In a Claude Code session (CLAUDECODE set) the reading commands also keep the project's bounds,
//! as Claude Code's Read does: fewer's install lets them run without asking (savers::READS),
//! so a file outside the project (its git root, else the current folder), the folders
//! `permissions.additionalDirectories` adds, and Claude Code's own folder for a session's files
//! (/tmp/claude-UID), that no Read allow rule names, is not read: Claude reads it with Claude
//! Code's Read tool, which asks the developer as usual. (A command cannot see the folder Claude
//! Code started in: a session started deep in a repository may read up to the repository's root.)
//!
//! Where it cannot tell (a pattern it cannot read), a rule is taken to match: for a deny or an
//! ask rule, refusing is the safe side. Settings it cannot read are passed over, as Claude Code
//! passes over a file that is not JSON.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::json::{self, Json};

/// What the rules say about one file or one command.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// No deny or ask rule names it.
    Free,
    /// A deny rule names it: the rule as written, and the file it is in.
    Deny(String, PathBuf),
    /// An ask rule names it (and no deny rule).
    Ask(String, PathBuf),
    /// (a Claude Code session) A file outside the project's bounds that no Read allow rule names:
    /// the project's root.
    Outside(PathBuf),
}

/// One rule: its tool, its pattern (None for a bare tool name), the file it came from, and the
/// folder `/path` patterns start from (the folder holding that file's `.claude`).
#[derive(Debug, Clone)]
struct Rule {
    text: String,
    tool: String,
    spec: Option<String>,
    file: PathBuf,
    root: PathBuf,
}

/// The deny and ask rules of every settings file Claude Code would read here, its Read allow
/// rules, and the project's bounds.
#[derive(Debug, Default)]
pub struct Rules {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
    allow: Vec<Rule>,
    /// The project's root first, then the folders the settings add and Claude Code's own.
    roots: Vec<PathBuf>,
    /// Whether a file outside them is refused: in a Claude Code session (CLAUDECODE).
    bounded: bool,
}

/// The managed settings files: managed-settings.json, then managed-settings.d/*.json by name, in
/// the organization's folder (FEWER_MANAGED_DIR, else the system's own: web.rs).
fn managed_files() -> Vec<PathBuf> {
    let dir = crate::web::managed_dir();
    let mut v = vec![dir.join("managed-settings.json")];
    if let Ok(rd) = std::fs::read_dir(dir.join("managed-settings.d")) {
        let mut more: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
            .collect();
        more.sort();
        v.extend(more);
    }
    v
}

/// The settings files Claude Code reads for a session in `cwd`, each with the folder its `/path`
/// patterns start from.
fn settings_files(cwd: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut v: Vec<(PathBuf, PathBuf)> = managed_files().into_iter().map(|f| (f, PathBuf::from("/"))).collect();
    if let Some(dir) = crate::defaults::setting("FEWER_CLAUDE_DIR") {
        let dir = PathBuf::from(dir);
        let root = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.clone());
        v.push((dir.join("settings.json"), root));
    }
    // (every folder from here up: Claude Code reads a project's settings in the folder it started
    // in, which a command cannot see, and a project need not be a git repository; a rule of a
    // folder further up can only refuse more, the safe side)
    for a in cwd.ancestors() {
        for name in ["settings.json", "settings.local.json"] {
            v.push((a.join(".claude").join(name), a.to_path_buf()));
        }
    }
    v
}

/// One `permissions` list of a settings file.
fn list(settings: &Json, key: &str) -> Vec<String> {
    match settings.get("permissions").and_then(|p| p.get(key)) {
        Some(Json::Arr(a)) => a.iter().filter_map(Json::str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// A rule as written: `Tool` or `Tool(spec)`.
fn parse_rule(text: &str, file: &Path, root: &Path) -> Option<Rule> {
    let t = text.trim();
    let (tool, spec) = match t.find('(') {
        Some(i) if t.ends_with(')') => (t[..i].trim().to_string(), Some(t[i + 1..t.len() - 1].trim().to_string())),
        Some(_) => return None,
        None => (t.to_string(), None),
    };
    if tool.is_empty() {
        return None;
    }
    Some(Rule { text: t.to_string(), tool, spec, file: file.to_path_buf(), root: root.to_path_buf() })
}

impl Rules {
    /// The rules for a session in `cwd`.
    pub fn load(cwd: &Path) -> Rules {
        let mut r = Rules { bounded: std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty()), ..Rules::default() };
        let home = crate::defaults::home_var().map(PathBuf::from);
        r.roots.push(crate::store::project_root(cwd));
        // (Claude Code's own folder for a session's files: a background command's output)
        #[cfg(unix)]
        if let Some(uid) = home.as_deref().and_then(|h| std::fs::metadata(h).ok()).map(|m| std::os::unix::fs::MetadataExt::uid(&m)) {
            r.roots.extend(["/tmp", "/private/tmp"].map(|t| PathBuf::from(format!("{t}/claude-{uid}"))));
        }
        let mut seen: Vec<PathBuf> = Vec::new();
        for (file, root) in settings_files(cwd) {
            // (the same file twice, a project in the home folder: once)
            let key = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let Ok(bytes) = std::fs::read(&file) else { continue };
            let Ok(s) = json::parse(&bytes) else { continue };
            for (key, into) in [("deny", &mut r.deny), ("ask", &mut r.ask), ("allow", &mut r.allow)] {
                into.extend(list(&s, key).iter().filter_map(|t| parse_rule(t, &file, &root)));
            }
            // (a folder as Claude Code reads it: absolute, from the home folder, else from the
            // folder the settings are for)
            for d in list(&s, "additionalDirectories") {
                let p = match (d.strip_prefix("~/"), &home) {
                    (Some(rest), Some(h)) => h.join(rest),
                    _ if d == "~" => home.clone().unwrap_or_default(),
                    _ => root.join(&d),
                };
                r.roots.push(normal(&p));
            }
        }
        // (each root as given and as it really is: a path is checked both ways)
        let real: Vec<PathBuf> = r.roots.iter().filter_map(|p| std::fs::canonicalize(p).ok()).collect();
        r.roots.extend(real);
        r
    }

    /// Rules given directly (tests).
    #[cfg(test)]
    fn of(deny: &[&str], ask: &[&str], root: &Path) -> Rules {
        let f = root.join(".claude").join("settings.json");
        Rules {
            deny: deny.iter().filter_map(|t| parse_rule(t, &f, root)).collect(),
            ask: ask.iter().filter_map(|t| parse_rule(t, &f, root)).collect(),
            ..Rules::default()
        }
    }

    /// The same, bounded to these roots with these Read allow rules (tests, Mac and Linux).
    #[cfg(all(test, unix))]
    fn bounded_to(mut self, roots: &[&Path], allow: &[&str]) -> Rules {
        let f = roots[0].join(".claude").join("settings.json");
        self.allow = allow.iter().filter_map(|t| parse_rule(t, &f, roots[0])).collect();
        self.roots = roots.iter().map(|p| p.to_path_buf()).collect();
        self.bounded = true;
        self
    }

    /// What the Read rules say about reading `path` (relative to `cwd` when not absolute).
    pub fn read(&self, path: &Path, cwd: &Path) -> Verdict {
        let abs = normal(&if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) });
        let real = std::fs::canonicalize(&abs).ok();
        let hit = |r: &Rule| r.tool == "Read" && [Some(&abs), real.as_ref()].into_iter().flatten().any(|p| read_matches(r.spec.as_deref(), p, cwd, &r.root));
        let v = verdict(&self.deny, &self.ask, hit);
        if v != Verdict::Free || !self.bounded {
            return v;
        }
        // (inside: where the file really is, so a link in the project to a file outside it is
        // outside; or a Read allow rule names it, as given or as it really is)
        let inside = real.as_ref().unwrap_or(&abs);
        // (a bare Read allow rule: every file, as Claude Code reads it)
        let allowed = |p: &PathBuf| self.allow.iter().any(|r| r.tool == "Read" && read_matches(r.spec.as_deref(), p, cwd, &r.root));
        if self.roots.iter().any(|r| inside.starts_with(r)) || [Some(&abs), real.as_ref()].into_iter().flatten().any(allowed) {
            Verdict::Free
        } else {
            Verdict::Outside(self.roots.first().cloned().unwrap_or_else(|| cwd.to_path_buf()))
        }
    }

    /// What the Bash rules say about running `argv` (as `fewer run` would, with no shell).
    pub fn command(&self, argv: &[OsString]) -> Verdict {
        let parts = command_parts(argv);
        let hit = |r: &Rule| r.tool == "Bash" && parts.iter().any(|c| bash_matches(r.spec.as_deref(), c));
        verdict(&self.deny, &self.ask, hit)
    }
}

fn verdict(deny: &[Rule], ask: &[Rule], hit: impl Fn(&Rule) -> bool) -> Verdict {
    if let Some(r) = deny.iter().find(|r| hit(r)) {
        return Verdict::Deny(r.text.clone(), r.file.clone());
    }
    if let Some(r) = ask.iter().find(|r| hit(r)) {
        return Verdict::Ask(r.text.clone(), r.file.clone());
    }
    Verdict::Free
}

/// A path with its `.` and `..` worked out (no file system).
fn normal(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether a Read rule's pattern covers `path` (absolute, normal).
fn read_matches(spec: Option<&str>, path: &Path, cwd: &Path, root: &Path) -> bool {
    let Some(s) = spec.map(str::trim) else { return true };
    if s.is_empty() || s == "*" || s == "**" {
        return true;
    }
    let home = crate::defaults::home_var().map(PathBuf::from);
    let (bases, pat, anchored): (Vec<PathBuf>, &str, bool) = if let Some(p) = s.strip_prefix("//") {
        (vec![PathBuf::from("/")], p, true)
    } else if let Some(p) = s.strip_prefix("~/") {
        match home {
            Some(h) => (vec![h], p, true),
            None => return true,
        }
    } else if let Some(p) = s.strip_prefix('/') {
        // (relative to the settings file: its project's root, or the folder holding it, as the
        // documentation can be read either way; both, refusing being the safe side)
        (vec![root.to_path_buf(), root.join(".claude")], p, true)
    } else if let Some(p) = s.strip_prefix("./") {
        (vec![cwd.to_path_buf(), root.to_path_buf()], p, true)
    } else {
        // (gitignore: a slash before the end anchors the pattern; with none it matches at any depth)
        let anchored = s.trim_end_matches('/').contains('/');
        (vec![cwd.to_path_buf(), root.to_path_buf()], s, anchored)
    };
    let pat = pat.trim_end_matches('/');
    if pat.is_empty() {
        return bases.iter().any(|b| path.starts_with(b));
    }
    let pat_segs: Vec<&str> = pat.split('/').filter(|x| !x.is_empty()).collect();
    bases.iter().any(|b| {
        let Ok(rel) = path.strip_prefix(b) else { return false };
        let segs: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        // (a rule that names a folder covers what is in it: every leading part of the path)
        (1..=segs.len()).any(|n| {
            let head: Vec<&str> = segs[..n].iter().map(String::as_str).collect();
            if anchored { segs_match(&pat_segs, &head) } else { (0..head.len()).any(|k| segs_match(&pat_segs, &head[k..])) }
        })
    })
}

/// Path segments against pattern segments: `**` takes any number of segments, the others match
/// one segment each (`*` and `?` within it).
fn segs_match(p: &[&str], t: &[&str]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some(&"**") => (0..=t.len()).any(|k| segs_match(&p[1..], &t[k..])),
        Some(first) => !t.is_empty() && glob(first.as_bytes(), t[0].as_bytes(), true) && segs_match(&p[1..], &t[1..]),
    }
}

/// A text against a pattern whose `*` stands for any text (with `one_segment`, never a '/') and
/// `?` for one character.
fn glob(p: &[u8], t: &[u8], one_segment: bool) -> bool {
    let (mut i, mut j, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while j < t.len() {
        if i < p.len() && (p[i] == t[j] || (p[i] == b'?' && !(one_segment && t[j] == b'/'))) && p[i] != b'*' {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(si) = star
            && !(one_segment && t[mark] == b'/')
        {
            i = si + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == b'*')
}

/// Whether a Bash rule's pattern covers one simple command.
fn bash_matches(spec: Option<&str>, cmd: &str) -> bool {
    let Some(s) = spec.map(str::trim) else { return true };
    let cmd = cmd.trim();
    if s.is_empty() || s == "*" {
        return true;
    }
    // (the legacy `cmd:*` and `cmd *`: the command itself, or it with more words)
    let prefix = s.strip_suffix(":*").or_else(|| s.strip_suffix(" *"));
    if let Some(p) = prefix
        && !p.contains('*')
    {
        let p = p.trim_end();
        return cmd == p || cmd.starts_with(&format!("{p} "));
    }
    if s.contains('*') {
        let flat = s.replace(":*", " *");
        // (`x *` at the end also takes x alone)
        if let Some(head) = flat.strip_suffix(" *")
            && glob(head.as_bytes(), cmd.as_bytes(), false)
        {
            return true;
        }
        return glob(flat.as_bytes(), cmd.as_bytes(), false);
    }
    cmd == s
}

/// The simple commands `argv` runs, as Claude Code would check them: the whole command line,
/// the command after a wrapper (env and its NAME=value words, nohup, nice, time, timeout and
/// their options), and each part of a shell's `-c` script.
fn command_parts(argv: &[OsString]) -> Vec<String> {
    let words: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let mut out = vec![join(&words)];
    let mut w: &[String] = &words;
    loop {
        let Some(first) = w.first() else { break };
        let name = first.rsplit('/').next().unwrap_or(first);
        let skip = match name {
            "env" => 1 + w[1..].iter().take_while(|x| x.starts_with('-') || x.contains('=')).count(),
            "nohup" | "time" => 1,
            "nice" => 1 + if w.get(1).is_some_and(|x| x == "-n") { 2 } else { usize::from(w.get(1).is_some_and(|x| x.starts_with('-'))) },
            "timeout" => {
                // (its options, -s and -k with a value of their own, then the duration)
                let mut i = 1;
                while w.get(i).is_some_and(|x| x.starts_with('-')) {
                    i += if ["-s", "--signal", "-k", "--kill-after"].contains(&w[i].as_str()) { 2 } else { 1 };
                }
                i + 1
            }
            _ => 0,
        };
        if skip == 0 || skip >= w.len() {
            break;
        }
        w = &w[skip..];
        out.push(join(w));
    }
    // a shell's -c script, part by part
    if let Some(first) = w.first() {
        let name = first.rsplit('/').next().unwrap_or(first);
        if ["sh", "bash", "zsh", "dash", "ksh", "fish"].contains(&name)
            && let Some(i) = w.iter().position(|x| x == "-c" || (x.starts_with('-') && !x.starts_with("--") && x.contains('c')))
            && let Some(script) = w.get(i + 1)
        {
            for part in script.split(['\n', ';', '|', '&']) {
                let p = part.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// Words as one command line (a word with a space or a quote is quoted, as a shell would need it).
fn join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| if w.is_empty() || w.chars().any(|c| c.is_whitespace() || "'\"\\$`".contains(c)) { format!("'{}'", w.replace('\'', "'\\''")) } else { w.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A path with the home folder shown as ~.
pub fn tilde(p: &Path) -> String {
    match crate::defaults::home_var().map(PathBuf::from) {
        Some(h) if p.starts_with(&h) => format!("~/{}", p.strip_prefix(&h).map(|r| r.display().to_string()).unwrap_or_default()),
        _ => p.display().to_string(),
    }
}

/// The line a refused read prints (the file as given).
pub fn refused_read(shown: &str, v: &Verdict) -> Option<String> {
    refused(shown, v, "not read")
}

/// The line a refused file prints, `not` saying what was not done with it.
pub fn refused(shown: &str, v: &Verdict, not: &str) -> Option<String> {
    match v {
        Verdict::Free => None,
        Verdict::Deny(rule, file) => Some(format!("{shown}: {not}: your permission rule {rule} (in {}) denies reading it", tilde(file))),
        Verdict::Ask(rule, file) => Some(format!(
            "{shown}: {not}: your permission rule {rule} (in {}) says Claude Code asks you first; read it with Claude Code's Read tool, which asks",
            tilde(file)
        )),
        Verdict::Outside(root) => Some(format!(
            "{shown}: {not}: it is outside this project ({}); read it with Claude Code's Read tool, which asks you first, as it does for files outside the project",
            tilde(root)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn read_rules_follow_claude_codes_patterns() {
        let root = Path::new("/w/proj");
        let cwd = root;
        let r = Rules::of(&["Read(./.env)", "Read(./secrets/**)", "Read(*.pem)", "Read(//etc/shadow)", "Read(/config/prod.yml)"], &["Read(./notes/private.md)"], root);
        let v = |p: &str| r.read(Path::new(p), cwd);
        assert!(matches!(v(".env"), Verdict::Deny(ref t, _) if t == "Read(./.env)"));
        assert_eq!(v("src/.env"), Verdict::Free, "./ anchors the pattern to the folder");
        assert!(matches!(v("secrets/a/b.txt"), Verdict::Deny(..)));
        assert!(matches!(v("deep/down/key.pem"), Verdict::Deny(..)), "no slash: any depth");
        assert!(matches!(v("/etc/shadow"), Verdict::Deny(..)));
        assert!(matches!(v("config/prod.yml"), Verdict::Deny(..)));
        assert!(matches!(v("notes/private.md"), Verdict::Ask(..)));
        assert_eq!(v("src/main.rs"), Verdict::Free);
        assert!(matches!(v("./sub/../.env"), Verdict::Deny(..)), "dots worked out");
        // a folder named by a rule covers what is in it
        let f = Rules::of(&["Read(./build)"], &[], root);
        assert!(matches!(f.read(Path::new("build/out.txt"), cwd), Verdict::Deny(..)));
        // a bare Read rule: every file; deny wins over ask
        let all = Rules::of(&["Read"], &["Read(./x)"], root);
        assert!(matches!(all.read(Path::new("x"), cwd), Verdict::Deny(..)));
        // other tools' rules are not Read's
        let other = Rules::of(&["Edit(./.env)", "Bash(cat:*)"], &[], root);
        assert_eq!(other.read(Path::new(".env"), cwd), Verdict::Free);
        // ** in the middle, ? for one character
        let g = Rules::of(&["Read(./src/**/test_?.py)"], &[], root);
        assert!(matches!(g.read(Path::new("src/a/b/test_1.py"), cwd), Verdict::Deny(..)));
        assert_eq!(g.read(Path::new("src/test_10.py"), cwd), Verdict::Free);
    }

    // (Mac and Linux: absolute paths written /w/proj)
    #[cfg(unix)]
    #[test]
    fn reads_keep_the_projects_bounds_in_a_claude_code_session() {
        let root = Path::new("/w/proj");
        let more = Path::new("/w/shared");
        let r = Rules::of(&["Read(./.env)"], &[], root).bounded_to(&[root, more], &["Read(~/notes/**)", "Read(//opt/docs/*.md)"]);
        let v = |p: &str| r.read(Path::new(p), root);
        assert_eq!(v("src/main.rs"), Verdict::Free);
        assert_eq!(v("/w/shared/a.txt"), Verdict::Free, "a folder the settings add");
        assert!(matches!(v(".env"), Verdict::Deny(..)), "deny first");
        assert_eq!(v("/etc/hosts"), Verdict::Outside(root.to_path_buf()));
        assert_eq!(v("../other/x.rs"), Verdict::Outside(root.to_path_buf()), "dots worked out");
        assert_eq!(v("/w/project2/x.rs"), Verdict::Outside(root.to_path_buf()), "a folder, not a prefix of its name");
        assert_eq!(v("/opt/docs/a.md"), Verdict::Free, "a Read allow rule names it");
        assert_eq!(v("/opt/docs/a.txt"), Verdict::Outside(root.to_path_buf()));
        let all = Rules::of(&[], &[], root).bounded_to(&[root], &["Read"]);
        assert_eq!(all.read(Path::new("/etc/hosts"), root), Verdict::Free, "a bare Read allow rule: every file");
        let line = refused_read("/etc/hosts", &v("/etc/hosts")).unwrap();
        assert!(line.starts_with("/etc/hosts: not read: it is outside this project (/w/proj); read it with Claude Code's Read tool"), "{line}");
        // (outside a Claude Code session: no bounds, as before)
        assert_eq!(Rules::of(&[], &[], root).read(Path::new("/etc/hosts"), root), Verdict::Free);
    }

    #[test]
    fn bash_rules_follow_claude_codes_patterns() {
        let root = Path::new("/w/proj");
        let r = Rules::of(&["Bash(rm:*)", "Bash(git push *)", "Bash(curl http://evil.example/*)"], &["Bash(npm publish)"], root);
        let v = |a: &[&str]| r.command(&os(a));
        assert!(matches!(v(&["rm", "-rf", "build"]), Verdict::Deny(ref t, _) if t == "Bash(rm:*)"));
        assert!(matches!(v(&["rm"]), Verdict::Deny(..)));
        assert_eq!(v(&["rmdir", "x"]), Verdict::Free, "a word boundary after the prefix");
        assert!(matches!(v(&["git", "push", "origin", "main"]), Verdict::Deny(..)));
        assert!(matches!(v(&["git", "push"]), Verdict::Deny(..)), "`x *` takes x alone");
        assert_eq!(v(&["git", "pushx"]), Verdict::Free);
        assert!(matches!(v(&["curl", "http://evil.example/a/b"]), Verdict::Deny(..)));
        assert!(matches!(v(&["npm", "publish"]), Verdict::Ask(..)));
        assert_eq!(v(&["npm", "publish", "--dry-run"]), Verdict::Free, "an exact rule is the whole command");
        assert_eq!(v(&["python3", "-m", "pytest"]), Verdict::Free);
        // through wrappers and a shell's script
        assert!(matches!(v(&["env", "A=1", "rm", "x"]), Verdict::Deny(..)));
        assert!(matches!(v(&["timeout", "-s", "KILL", "30", "rm", "x"]), Verdict::Deny(..)));
        assert!(matches!(v(&["nice", "-n", "5", "git", "push"]), Verdict::Deny(..)));
        assert!(matches!(v(&["sh", "-c", "make && rm -rf /tmp/x"]), Verdict::Deny(..)));
        assert!(matches!(v(&["/bin/bash", "-lc", "echo hi; npm publish"]), Verdict::Ask(..)));
        assert_eq!(v(&["sh", "-c", "make test"]), Verdict::Free);
        // a bare Bash deny: every command
        let all = Rules::of(&["Bash"], &[], root);
        assert!(matches!(all.command(&os(&["ls"])), Verdict::Deny(..)));
        // a word with a space is quoted, as Claude Code sees the command line
        let q = Rules::of(&["Bash(echo 'a b')"], &[], root);
        assert!(matches!(q.command(&os(&["echo", "a b"])), Verdict::Deny(..)));
    }

    #[test]
    fn rules_come_from_the_settings_files_claude_code_reads() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-perms", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let proj = d.join("proj");
        let sub = proj.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::create_dir_all(proj.join(".claude")).unwrap();
        std::fs::write(proj.join(".claude").join("settings.json"), r#"{"permissions":{"deny":["Read(./.env)"],"allow":["Bash(ls:*)"]}}"#).unwrap();
        std::fs::write(proj.join(".claude").join("settings.local.json"), r#"{"permissions":{"ask":["Bash(git push:*)"]}}"#).unwrap();
        // (a file that is not JSON is passed over)
        std::fs::create_dir_all(sub.join(".claude")).unwrap();
        std::fs::write(sub.join(".claude").join("settings.json"), "not json").unwrap();
        let r = Rules::load(&sub);
        // (from a folder deep in the project: its root's rules, the paths from there)
        assert!(matches!(r.read(&proj.join(".env"), &sub), Verdict::Deny(..)));
        assert!(matches!(r.command(&os(&["git", "push"])), Verdict::Ask(..)));
        assert_eq!(r.command(&os(&["ls", "-la"])), Verdict::Free, "allow rules are Claude Code's own business");
        let line = refused_read(".env", &r.read(&proj.join(".env"), &proj)).unwrap();
        assert!(line.starts_with(".env: not read: your permission rule Read(./.env) (in ") && line.ends_with("denies reading it"), "{line}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
