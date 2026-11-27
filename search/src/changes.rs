//! `fewer changes [REV]`: what a commit changed, in one call. Its subject, author and date, then each file
//! with each changed function whole as it is after the commit (git's own function context, -W),
//! the lines taken out (-) and put in (+) marked, and the names those lines use that the file
//! defines at its top level ("TAX_RATE = 0.08 (line 2)"), one line each. A commit question took
//! Claude two calls with fewer on (the diff, then the constants it used: 8.1x); this is one.
//! A merge is shown against its first parent. Long output is cut at 400 lines, with the git
//! command for the rest. Read-only: git show, nothing else.

use std::ffi::{OsStr, OsString};
use std::time::Duration;

const MAX_LINES: usize = 400;
/// A changed function longer than this is shown as its changed lines with 3 lines around each.
const HUNK_LINES: usize = 60;
const NAMES: usize = 8;

const KEYWORDS: &[&str] = &["if", "else", "for", "while", "return", "def", "fn", "let", "const", "var", "in", "not", "and", "or", "is", "None", "True", "False", "self", "pub", "use", "import", "from", "as", "with", "try", "except", "match", "case", "class", "struct", "impl", "function", "export", "static", "new", "this", "null", "true", "false"];

fn git(root: &std::path::Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut a: Vec<&OsStr> = vec![OsStr::new("-C"), root.as_os_str()];
    a.extend(args.iter().map(OsStr::new));
    crate::store::run_quiet("git", &a, Duration::from_secs(20))
}

/// The top-level names a file defines: (name, line number, the line).
pub fn top_names(text: &str) -> Vec<(String, usize, String)> {
    let mut v = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if l.starts_with([' ', '\t', '#', '/', '*', '}', ')', ']']) || l.is_empty() {
            continue;
        }
        let mut rest = l;
        for kw in ["export ", "pub ", "const ", "let ", "var ", "static ", "final ", "readonly "] {
            if let Some(r) = rest.strip_prefix(kw) {
                rest = r.trim_start();
            }
        }
        let n: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if n.is_empty() || n.chars().next().is_some_and(|c| c.is_ascii_digit()) || KEYWORDS.contains(&n.as_str()) {
            continue;
        }
        let after = rest[n.len()..].trim_start();
        // NAME = value, NAME: Type = value (not ==, not a call)
        let after = if let Some(t) = after.strip_prefix(':') { t.split_once('=').map_or("", |(_, b)| b).trim_start().to_string() } else { after.strip_prefix('=').filter(|r| !r.starts_with('=')).map(|r| r.to_string()).unwrap_or_default() };
        if !after.is_empty() {
            v.push((n, i + 1, l.trim_end().to_string()));
        }
    }
    v
}

fn words(l: &str) -> impl Iterator<Item = &str> {
    l.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

/// The view of one commit's `git show -W` output, and each changed file's text after it.
pub fn view(show: &str, file_after: &dyn Fn(&str) -> Option<String>) -> String {
    let (head, diff) = show.split_once('\u{1}').unwrap_or(("", show));
    let f: Vec<&str> = head.split('\0').collect();
    let (hash, author, date, subject, body) = (f.first().copied().unwrap_or(""), f.get(1).copied().unwrap_or(""), f.get(2).copied().unwrap_or(""), f.get(3).copied().unwrap_or(""), f.get(4).copied().unwrap_or("").trim());
    let mut out: Vec<String> = Vec::new();
    // the files: path, hunks (header, lines)
    struct File {
        path: String,
        hunks: Vec<(String, Vec<String>)>,
        other: Vec<String>,
    }
    let mut files: Vec<File> = Vec::new();
    for l in diff.lines() {
        if let Some(r) = l.strip_prefix("diff --git ") {
            let path = r.rsplit_once(" b/").map_or(r, |(_, b)| b).to_string();
            files.push(File { path, hunks: Vec::new(), other: Vec::new() });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if l.starts_with("@@") {
            file.hunks.push((l.to_string(), Vec::new()));
        } else if let Some((_, lines)) = file.hunks.last_mut() {
            if l.starts_with([' ', '+', '-']) || l.starts_with('\\') {
                lines.push(l.to_string());
            }
        } else if l.starts_with("Binary files") || l.starts_with("new file") || l.starts_with("deleted file") || l.starts_with("rename ") || l.starts_with("similarity") {
            file.other.push(l.to_string());
        }
    }
    let (mut add, mut del) = (0, 0);
    for f in &files {
        for (_, ls) in &f.hunks {
            add += ls.iter().filter(|l| l.starts_with('+')).count();
            del += ls.iter().filter(|l| l.starts_with('-')).count();
        }
    }
    out.push(format!("commit {hash} \"{subject}\" by {author}, {date}: {} file{}, +{add} -{del}", files.len(), if files.len() == 1 { "" } else { "s" }));
    for l in body.lines().filter(|l| !l.trim().is_empty()).take(3) {
        out.push(format!("  {}", l.trim()));
    }
    for f in &files {
        out.push(format!("{}:{}", f.path, if f.other.is_empty() { String::new() } else { format!(" {}", f.other.join("; ")) }));
        let mut changed_words: Vec<String> = Vec::new();
        for (h, lines) in &f.hunks {
            // "@@ -10,3 +10,4 @@ def tax(price):" -> where, and the new side's first line
            let ctx = h.rsplit_once("@@").map(|(_, c)| c.trim()).unwrap_or("");
            let new_start: usize = h.split_whitespace().find(|w| w.starts_with('+')).and_then(|w| w[1..].split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(1);
            let _ = ctx;
            let title = block_title(lines);
            out.push(format!("  in {title}"));
            // the lines, numbered on the new side; a long function: only its changed lines, 3 around
            let mut n = new_start;
            let mut marks: Vec<(Option<usize>, &String)> = lines.iter().map(|l| {
                let num = if l.starts_with('-') { None } else { let k = n; n += 1; Some(k) };
                (num, l)
            }).collect();
            // git's context around the function, left out: blank lines before it and after the
            // last change, and the next function's first line; a change at the top level (a
            // decorator, a definition, a constant): the lines before it, which belong to what came
            // before; a changed decorator: the definition under it shown, to its end
            let first_change = marks.iter().position(|(_, l)| l.starts_with(['+', '-'])).unwrap_or(0);
            let last_change = marks.iter().rposition(|(_, l)| l.starts_with(['+', '-'])).unwrap_or(0);
            let top_level = |t: &str| !t.trim().is_empty() && !t.starts_with([' ', '\t']) && !t.starts_with(['}', ')', ']']);
            let lead = match marks.get(first_change) {
                Some((_, l)) if top_level(&l[1..]) => first_change,
                _ => marks[..first_change].iter().take_while(|(_, l)| l[1..].trim().is_empty()).count(),
            };
            let mut decorated = marks.get(last_change).is_some_and(|(_, l)| decorator(&l[1..]));
            let mut end = marks.len();
            for (i, (_, l)) in marks.iter().enumerate().skip(last_change + 1) {
                let t = &l[1..];
                let top_def = !t.starts_with([' ', '\t']) && def_of(t).is_some();
                if decorated && (decorator(t) || top_def) {
                    // (more decorators, then the definition they decorate: kept)
                    decorated = !top_def;
                    continue;
                }
                if t.trim().is_empty() || top_def {
                    end = i;
                    break;
                }
            }
            marks.truncate(end);
            marks.drain(..lead);
            let near: Vec<bool> = if marks.len() <= HUNK_LINES {
                vec![true; marks.len()]
            } else {
                let ch: Vec<usize> = marks.iter().enumerate().filter(|(_, (_, l))| l.starts_with(['+', '-'])).map(|(i, _)| i).collect();
                (0..marks.len()).map(|i| ch.iter().any(|&c| c.abs_diff(i) <= 3)).collect()
            };
            let mut gap = false;
            for (i, (num, l)) in marks.iter().enumerate() {
                if !near[i] {
                    if !gap {
                        out.push("    …".into());
                        gap = true;
                    }
                    continue;
                }
                gap = false;
                let (sign, text) = l.split_at(1);
                if sign == "+" || sign == "-" {
                    changed_words.extend(words(text).map(str::to_string));
                }
                let num = num.map(|k| format!("{k:>4}")).unwrap_or_else(|| "    ".into());
                out.push(format!("  {sign}{num} {text}"));
            }
        }
        // the top-level names the changed lines use, as the file defines them after the commit
        if let Some(text) = file_after(&f.path) {
            let defs = top_names(&text);
            let mut used: Vec<String> = Vec::new();
            for (name, line, src) in &defs {
                if changed_words.iter().any(|w| w == name) && !used.iter().any(|u| u.starts_with(&format!("{name} "))) {
                    // (a name the changed lines define themselves is not looked up)
                    let defined_here = f.hunks.iter().flat_map(|(_, ls)| ls.iter()).any(|l| l.starts_with('+') && l[1..].trim_end() == src.as_str());
                    if !defined_here {
                        used.push(format!("{name} (line {line}): {}", src.trim()));
                    }
                }
                if used.len() >= NAMES {
                    break;
                }
            }
            if !used.is_empty() {
                out.push(format!("  names these lines use, defined in the file: {}", used.join("; ")));
            }
        }
    }
    if out.len() > MAX_LINES {
        let more = out.len() - MAX_LINES;
        out.truncate(MAX_LINES);
        out.push(format!("… {more} more lines: git show {hash} -- FILE"));
    }
    out.join("\n")
}

/// The words that start a definition (after its indentation and its qualifiers).
const DEFS: [&str; 15] = ["def ", "fn ", "function ", "function*", "func ", "class ", "impl ", "impl<", "mod ", "trait ", "enum ", "struct ", "interface ", "module ", "namespace "];
/// Words that may come before a definition's word: `pub(crate) async fn`, `export default class`.
const QUALIFIERS: [&str; 8] = ["pub ", "export ", "default ", "async ", "unsafe ", "extern ", "abstract ", "const fn "];

/// A decorator's or an attribute's line: `@cache`, `#[test]`.
fn decorator(t: &str) -> bool {
    let u = t.trim_start();
    u.starts_with('@') || u.starts_with("#[")
}

/// A line that starts a definition: (its key, how it is shown), else None. Its qualifiers are
/// passed over (pub, pub(crate), export, default, async, ...); a JavaScript function given to a
/// name counts (`const gamma = (c) => {`, `export const f = async function () {`).
fn def_of(t: &str) -> Option<(String, String)> {
    let u = t.trim_start();
    let mut rest = u;
    loop {
        if let Some(r) = rest.strip_prefix("pub(").and_then(|r| r.split_once(") ")) {
            rest = r.1;
        } else if let Some(q) = QUALIFIERS.iter().find(|q| rest.starts_with(**q)) {
            // (`const fn`: the fn stays)
            rest = if *q == "const fn " { &rest["const ".len()..] } else { &rest[q.len()..] };
        } else {
            break;
        }
    }
    let shown = u.trim_end().trim_end_matches(['{', ':']).trim().to_string();
    if DEFS.iter().any(|k| rest.starts_with(k)) {
        // (an impl named whole: `impl Display for Money` and `impl Money` are two)
        let key = if rest.starts_with("impl") { shown.clone() } else { shown.split(['(', '<', '{', ':']).next().unwrap_or(&shown).trim().to_string() };
        return Some((key, shown));
    }
    for k in ["const ", "let ", "var "] {
        let Some(r) = rest.strip_prefix(k) else { continue };
        let (name, value) = r.split_once('=')?;
        let v = value.trim_start();
        let named = name.trim().split(':').next().unwrap_or("").trim();
        let ident = !named.is_empty() && named.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
        let function = v.starts_with("function") || v.starts_with("async ") || v.starts_with("async(") || ((v.starts_with('(') || v.split_whitespace().next().is_some_and(|w| w.chars().all(|c| c.is_alphanumeric() || c == '_'))) && v.contains("=>"));
        return (ident && function && !v.starts_with("=")).then(|| (format!("{k}{named}"), shown));
    }
    None
}

/// What a block is named by: each definition a changed line is in, in order. A definition's
/// lines are the ones indented past it (and its closing brace); a decorator's or an attribute's
/// change counts for the definition under it; a definition changed (its old line and its new)
/// is named once, as it is now; blank lines do not count. A changed line in no definition of
/// the block: at the top level, "(top level)"; indented, in what git took for its function
/// (`git show -W` starts the block there): the block's first line, when it is at the top level
/// (git's own header names the function before the block, not this one).
fn block_title(lines: &[String]) -> String {
    let indent = |t: &str| t.len() - t.trim_start().len();
    // (key, as shown) of each definition named
    let mut owners: Vec<(String, String)> = Vec::new();
    let name = |owners: &mut Vec<(String, String)>, key: &str, shown: &str, removed: bool| {
        match owners.iter_mut().find(|o| o.0 == key) {
            Some(o) if !removed => o.1 = shown.to_string(),
            Some(_) => {}
            None => owners.push((key.to_string(), shown.to_string())),
        }
    };
    let first = lines.iter().map(|l| l.get(1..).unwrap_or("")).find(|t| !t.trim().is_empty());
    let header = first.filter(|t| !t.starts_with([' ', '\t'])).map(|t| t.trim_end().trim_end_matches(['{', ':']).trim().to_string()).unwrap_or_default();
    // the definition the lines are in: (its indentation, key, as shown)
    let mut current: Option<(usize, String, String)> = None;
    let mut seen_def = false;
    // a changed decorator line waiting for its definition
    let mut decorated = false;
    for l in lines {
        let (sign, t) = l.split_at(1.min(l.len()));
        let changed = sign == "+" || sign == "-";
        if t.trim().is_empty() {
            continue;
        }
        if let Some((key, shown)) = def_of(t) {
            if changed || decorated {
                name(&mut owners, &key, &shown, sign == "-");
            }
            current = Some((indent(t), key, shown));
            seen_def = true;
            decorated = false;
            continue;
        }
        if decorator(t) {
            decorated |= changed;
            continue;
        }
        // a line at the definition's indentation or less ends it, but for its closing bracket
        let closing = t.trim_start().starts_with(['}', ')', ']']);
        let ends = current.as_ref().is_some_and(|c| indent(t) <= c.0);
        if ends && !closing {
            current = None;
        }
        if changed {
            match &current {
                Some((_, key, shown)) => name(&mut owners, key, shown, false),
                None if !seen_def && indent(t) > 0 && !header.is_empty() => name(&mut owners, &header, &header, false),
                None => name(&mut owners, "(top level)", "(top level)", false),
            }
        }
        if ends && closing {
            current = None;
        }
    }
    if owners.is_empty() {
        return "(top level)".into();
    }
    owners.into_iter().map(|o| o.1).collect::<Vec<_>>().join(", ")
}

pub fn cmd_changes(args: Vec<OsString>) -> i32 {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let rev = match words.as_slice() {
        [] => "HEAD".to_string(),
        [h] if h == "-h" || h == "--help" => {
            println!("fewer changes [REV] - what a commit changed (HEAD by default), in one call: each changed function whole as it is after it, the lines taken out (-) and put in (+), and the top-level names those lines use (a constant's value), from git show");
            return 0;
        }
        [r] if !r.starts_with('-') => r.clone(),
        _ => {
            eprintln!("usage: fewer changes [REV]");
            return 2;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = crate::store::project_root(&cwd);
    let fmt = "--format=%h%x00%an%x00%ad%x00%s%x00%b%x01";
    // (Claude Code's Read rules, which never see what git shows: a file they deny or ask about is
    // left out, each said at the end; perms.rs)
    let rules = crate::perms::Rules::load(&cwd);
    let names = git(&root, &["show", "--no-color", "--no-renames", "--diff-merges=first-parent", "--name-only", "-z", "--format=", &rev]).unwrap_or_default();
    let mut refused = Vec::new();
    let mut left_out = Vec::new();
    for p in names.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let p = String::from_utf8_lossy(p).into_owned();
        if let Some(line) = crate::perms::refused(&p, &rules.read(&root.join(&p), &cwd), "not shown") {
            refused.push(line);
            left_out.push(format!(":(top,exclude,literal){p}"));
        }
    }
    let mut args = vec!["show", "--no-color", "--no-ext-diff", "--no-textconv", "--diff-merges=first-parent", "-W", "--date=short", fmt, &rev];
    if !left_out.is_empty() {
        args.extend(["--", ":/"]);
        args.extend(left_out.iter().map(String::as_str));
    }
    let Some(mut show) = git(&root, &args) else {
        eprintln!("fewer changes: git show {rev} did not work in {} (not a git repository, or no such commit)", root.display());
        return 1;
    };
    // (a commit of left-out files alone: git shows nothing, so its header alone)
    if show.is_empty() && !left_out.is_empty() {
        show = git(&root, &["show", "--no-color", "-s", "--date=short", fmt, &rev]).unwrap_or_default();
    }
    let extras = crate::secrets::store_extras();
    // (secrets hidden in all of it, as `read` hides them: each diff line's text before the view
    // is made, every line kept, so that a private key's lines are known as one; then the view,
    // a constant's value included)
    let show = hide_diff(&String::from_utf8_lossy(&show), &extras);
    let after = |path: &str| git(&root, &["show", &format!("{rev}:{path}")]).map(|b| String::from_utf8_lossy(&b).into_owned());
    println!("{}", crate::secrets::hide_str(&view(&show, &after), &extras));
    for line in &refused {
        println!("{line}");
    }
    0
}

/// git show's text with its secrets hidden line by line, each line kept (a private key's lines
/// left empty, as `read` shows them): each diff line's text without its sign (+, - or a space).
fn hide_diff(show: &str, extras: &[crate::secrets::Extra]) -> String {
    let mut h = crate::secrets::Hider::new(extras.to_vec());
    let mut out = String::with_capacity(show.len());
    for l in show.split_inclusive('\n') {
        let signed = l.starts_with(['+', '-', ' ']) && !l.starts_with("+++ ") && !l.starts_with("--- ");
        let (sign, body) = if signed { l.split_at(1) } else { ("", l) };
        let mut o = Vec::new();
        h.line(body.as_bytes(), &mut o);
        out.push_str(sign);
        if o.is_empty() && body.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str(&String::from_utf8_lossy(&o));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_in_one_view() {
        let show = "6b45442\0t\x002026-11-23\0retry longer; lower tax on big orders\0\x01\n\
diff --git a/calc/pricing.py b/calc/pricing.py\nindex 1..2 100644\n--- a/calc/pricing.py\n+++ b/calc/pricing.py\n\
@@ -9,2 +9,3 @@ def apply_discount(price, pct):\n \n def tax(price):\n-    return round(price * TAX_RATE, 2)\n+    rate = TAX_RATE if price < 1000 else 0.05\n+    return round(price * rate, 2)\n\
diff --git a/calc/upload.py b/calc/upload.py\nindex 3..4 100644\n--- a/calc/upload.py\n+++ b/calc/upload.py\n\
@@ -8,2 +8,2 @@ MAX_UPLOAD_RETRIES = int(os.environ.get(\"CALC_UPLOAD_RETRIES\", \"7\"))\n def backoff(attempt):\n-    return min(2 ** attempt, 30)\n+    return min(2 ** attempt, 60)\n";
        let pricing = "\"\"\"Prices.\"\"\"\nTAX_RATE = 0.08\n\n\ndef apply_discount(price, pct):\n    return price\n\n\ndef tax(price):\n    rate = TAX_RATE if price < 1000 else 0.05\n    return round(price * rate, 2)\n";
        let after = |p: &str| (p == "calc/pricing.py").then(|| pricing.to_string());
        let v = view(show, &after);
        assert!(v.starts_with("commit 6b45442 \"retry longer; lower tax on big orders\" by t, 2026-11-23: 2 files, +3 -2\ncalc/pricing.py:\n  in def tax(price)\n"), "{v}");
        assert!(v.contains("  -         return round(price * TAX_RATE, 2)\n  +  11     rate = TAX_RATE if price < 1000 else 0.05\n  +  12     return round(price * rate, 2)\n"), "{v}");
        assert!(v.contains("  names these lines use, defined in the file: TAX_RATE (line 2): TAX_RATE = 0.08\n"), "{v}");
        assert!(v.ends_with("calc/upload.py:\n  in def backoff(attempt)\n      8 def backoff(attempt):\n  -         return min(2 ** attempt, 30)\n  +   9     return min(2 ** attempt, 60)"), "{v}");
        assert!(v.contains("  in def tax(price)\n     10 def tax(price):\n"), "blank lines before the function left out: {v}");
        let n = top_names("A = 1\nconst B: u32 = 2;\nexport const C = 3\nif x == 1:\n    D = 4\ndef f(): pass\nE: int = 5\n");
        let names: Vec<&str> = n.iter().map(|(a, _, _)| a.as_str()).collect();
        assert_eq!(names, ["A", "B", "C", "E"]);
    }

    #[test]
    fn a_block_named_by_the_functions_it_changes() {
        // one hunk of git's -W over three functions, the changes in the last two: named by those,
        // not by the first function the hunk holds
        let show = "1a2b3c4\0t\x002026-11-25\0div\0\x01\n\
diff --git a/calc.py b/calc.py\nindex 1..2 100644\n--- a/calc.py\n+++ b/calc.py\n\
@@ -1,5 +1,8 @@\n def add(a, b):\n     return a + b\n \n def mul(a, b):\n-    return a * b\n+    result = a * b\n+    return result\n+\n+def div(a, b):\n+    return a / b\n";
        let v = view(show, &|_: &str| None);
        assert!(v.contains("calc.py:\n  in def mul(a, b), def div(a, b)\n"), "{v}");
        let lines = |t: &str| t.lines().map(str::to_string).collect::<Vec<_>>();
        // a top-level change after a function: not the function's
        assert_eq!(block_title(&lines(" def add(a, b):\n     return a + b\n \n-LIMIT = 1\n+LIMIT = 2")), "(top level)");
        // an added decorator: the function under it's, not the one before it
        assert_eq!(block_title(&lines(" def a():\n     pass\n \n+@cache\n def b():\n     return 1")), "def b()");
        // a definition changed: named once, as it is now
        assert_eq!(block_title(&lines("-def f(a):\n+def f(a, b):\n     return a")), "def f(a, b)");
        // a changed line above the block's first definition: indented, in the block's first line
        // (git -W starts it at its function); at the top level, the top level
        assert_eq!(block_title(&lines(" impl Shop {\n     x = 1\n-    y = 2\n+    y = 3")), "impl Shop");
        assert_eq!(block_title(&lines("-import os\n+import sys\n def g():\n     pass")), "(top level)");
        // JavaScript: an exported function, a function given to a name, after a class
        assert_eq!(block_title(&lines(" export function alpha(a) {\n-  return a;\n+  return a + 1;\n }")), "export function alpha(a)");
        assert_eq!(block_title(&lines(" class Box {\n   x = 1;\n }\n \n const gamma = (c) => {\n-  return c;\n+  return c * 2;\n };")), "const gamma = (c) =>");
        assert_eq!(block_title(&lines(" export const load = async (url) => {\n-  a();\n+  b();\n };")), "export const load = async (url) =>");
        // Rust: an attribute changed counts for the function under it, in its module
        assert_eq!(block_title(&lines(" mod tests {\n     use super::*;\n \n-    #[ignore]\n+    #[should_panic]\n     fn t1() {}\n }")), "fn t1() {}");
        assert_eq!(block_title(&lines(" pub(crate) async fn go(x: u8) -> u8 {\n-    x\n+    x + 1\n }")), "pub(crate) async fn go(x: u8) -> u8");
        assert!(def_of("static API_TOKEN: &str = \"x\";").is_none() && def_of("const LIMIT = 5;").is_none() && def_of("let x = a => b;").is_some());
        // a function whose closing brace changed, then the next one: each named
        assert_eq!(block_title(&lines(" fn a() {\n-    x();\n+    y();\n }\n fn b() {\n+    z();\n }")), "fn a(), fn b()");
    }

    #[test]
    fn a_private_key_in_a_diff_is_hidden_whole() {
        let key = "MIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/ygWyF8PbnGy0AHB7MaGLJ2aBdE6ULrKr\n9y5vxw==\n";
        let show = format!("h\0t\x002026-11-25\0key\0\x01\ndiff --git a/key.pem b/key.pem\nnew file mode 100644\n--- /dev/null\n+++ b/key.pem\n@@ -0,0 +1,4 @@\n+-----BEGIN RSA PRIVATE KEY-----\n{}+-----END RSA PRIVATE KEY-----\n", key.lines().map(|l| format!("+{l}\n")).collect::<String>());
        let hidden = hide_diff(&show, &[]);
        assert!(!hidden.contains("9y5vxw==") && !hidden.contains("MIIEow"), "{hidden}");
        assert_eq!(hidden.lines().count(), show.lines().count(), "{hidden}");
    }

    #[test]
    fn an_added_decorator_shows_its_function() {
        // git -W's block over the function before and the decorated one: the decorated one shown
        // whole, the one before left out
        let show = "1a2b3c4\0t\x002026-11-25\0cache area\0\x01\n\
diff --git a/geo.py b/geo.py\nindex 1..2 100644\n--- a/geo.py\n+++ b/geo.py\n\
@@ -17,6 +17,7 @@ import math\n def div(a, b):\n     return a / b\n \n \n+@functools.lru_cache(maxsize=None)\n def area(r):\n     return math.pi * r * r\n";
        let v = view(show, &|_: &str| None);
        assert!(v.contains("geo.py:\n  in def area(r)\n  +  21 @functools.lru_cache(maxsize=None)\n     22 def area(r):\n     23     return math.pi * r * r"), "{v}");
        assert!(!v.contains("return a / b"), "{v}");
    }
}
