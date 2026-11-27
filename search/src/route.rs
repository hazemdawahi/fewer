//! Where to look for a question that names no page:
//! routes that need no model and no key, tried before any search, each saying where its answer
//! came from, so that a question is answered in the one call that asks it:
//! - an address written without https:// ("docs.python.org/3/library/functions.html#zip"): read
//!   as if it had it (a host name with a path, or of three parts or more, so that a file's name,
//!   deploy.sh, is never one);
//! - a function, an option or a command of a documentation set DevDocs indexes (Python,
//!   JavaScript, Node.js, Rust, Go, git, bash, PostgreSQL and the others, the set named in the
//!   question): the set's index, downloaded at most once a week into the store and searched here,
//!   so that the question is not sent anywhere; the entry's page then read like any other page, at
//!   its entry (what its #name shows);
//! - a package's latest version, license and release from its registry's public record (PyPI,
//!   npm, crates.io), also when the question gives the package's page there: the facts, made here;
//! - a product's releases (a language, a database, an operating system, a framework: 489 of them)
//!   from endoflife.date: when each version came out, its latest patch, when its support ends;
//! - a question about a thing: Wikipedia's article on it, its introduction (Wikipedia's own
//!   summary) and its part that answers the question.
//!   (An error message has no free route: Stack Overflow, where most are answered, is refused by
//!   Anthropic's domain check, which WebFetch asks before it reads.)
//!
//! Everything is read through the page reader (page::read_page: an address WebFetch would read,
//! public addresses only, redirects within the host) with the caller's `check` of each host (the
//! user's rules for WebFetch, Anthropic's domain check).

use crate::json::Json;
use std::path::{Path, PathBuf};

/// How long a downloaded index is used before it is read again.
const INDEX_SECS: u64 = 7 * 24 * 3600;
/// DevDocs: the list of its documentation sets, and where each set's index and pages are.
pub const DEVDOCS_LIST: &str = "https://devdocs.io/docs.json";
pub const DEVDOCS_PAGES: &str = "https://documents.devdocs.io";

/// A host's check before it is read (the user's WebFetch rules, Anthropic's domain check).
pub type Check<'a> = &'a dyn Fn(&str) -> Result<(), String>;

// ------------------------------------------------------------------------------ an address

/// The top-level domains an address written without https:// is taken in.
const TLDS: &[&str] = &[
    "com", "org", "net", "io", "dev", "edu", "gov", "info", "co", "app", "ai", "rs", "sh", "me", "us", "uk", "de", "fr", "eu", "ca", "au", "jp", "cn", "in", "nl", "se", "ch", "it", "es", "ru", "br", "tech", "cloud", "page",
    "site", "blog", "xyz", "so", "gg", "tv", "ly", "to", "cc", "ws", "pl", "be", "at", "dk", "no", "fi", "nz", "kr", "tw", "sg", "hk", "id", "mx", "ar", "cz", "pt", "gr", "ie", "il", "za",
];

/// The first address in a question written without its https:// ("docs.python.org/3/x.html#y",
/// "github.com/rust-lang/rust"): a host name whose last part is a common top-level domain, with a
/// path after it, as https. (A host alone is a site named, not a page: "(doc.rust-lang.org
/// std::vec::Vec::retain)" read the site's first page, 2026-11-25; nor is "deploy.sh", a file.)
pub fn bare_address(task: &str) -> Option<String> {
    for raw in task.split_whitespace() {
        let t = raw.trim_start_matches(['(', '[', '<', '"', '\'', '`']);
        let edge = |c: char| matches!(c, ']' | '>' | '"' | '\'' | '`' | ',' | ';' | '!' | '?' | '.' | ':');
        let mut t = t.trim_end_matches(edge);
        // (a ')' ends the address only when it closes none of the address's own: Wikipedia's
        // "Rust_(programming_language)" keeps its ')', "(see docs.python.org/3/x.html)" loses it;
        // as first_url reads one)
        while t.ends_with(')') && t.matches(')').count() > t.matches('(').count() {
            t = t[..t.len() - 1].trim_end_matches(edge);
        }
        if t.contains("://") || t.contains('@') {
            continue;
        }
        let (host, path) = match t.find(['/', '#', '?']) {
            Some(i) => (&t[..i], &t[i..]),
            None => (t, ""),
        };
        // (an IP address with a port or a path, "10.0.0.1:8443/admin": an address too, so that it
        // is checked, and refused when it is not public)
        let (host_only, port) = host.rsplit_once(':').filter(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())).unwrap_or((host, ""));
        if host_only.parse::<std::net::Ipv4Addr>().is_ok() && (path.starts_with('/') || !port.is_empty()) {
            return Some(format!("https://{host}{path}"));
        }
        let labels: Vec<&str> = host.split('.').collect();
        let ok_label = |l: &str| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !l.starts_with('-') && !l.ends_with('-');
        if labels.len() < 2 || !labels.iter().all(|l| ok_label(l)) || labels.iter().all(|l| l.chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        let tld = labels[labels.len() - 1].to_ascii_lowercase();
        if !TLDS.contains(&tld.as_str()) || labels[..labels.len() - 1].iter().any(|l| l.chars().all(|c| c.is_ascii_digit())) && labels.len() == 2 {
            continue;
        }
        if path.starts_with('/') {
            return Some(format!("https://{host}{path}"));
        }
    }
    None
}

/// Every address in a question written without its https:// (bare_address's kind), as https.
pub fn bare_addresses(task: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in task.split_whitespace() {
        if let Some(u) = bare_address(raw)
            && !out.contains(&u)
        {
            out.push(u);
        }
    }
    out
}

// ------------------------------------------------------------------------- reading and keeping

/// A text from the web through the page reader.
fn web_text(url: &str, check: Check, accept: &str) -> Result<String, String> {
    use crate::page::{Gate, Page};
    let addr = match crate::page::gate(url) {
        Gate::Read(a) => a,
        Gate::Claude(why) | Gate::Refused(why) => return Err(why),
    };
    match crate::page::read_page(&addr, check, accept)? {
        Page::Text { body, .. } => Ok(body),
        Page::Moved { to, .. } => Err(format!("it redirects to {to}")),
    }
}

/// A JSON document from the web.
fn web_json(url: &str, check: Check) -> Result<Json, String> {
    crate::json::parse(web_text(url, check, "application/json")?.as_bytes())
}

/// The store's folder for downloaded indexes.
fn index_dir(store: &Path) -> PathBuf {
    store.join("web").join("devdocs")
}

/// A downloaded index from the store when it is younger than INDEX_SECS, else read again and
/// kept (an older copy when the web fails).
fn cached(store: &Path, name: &str, url: &str, check: Check) -> Option<Json> {
    // (a name from the web, a set's slug, is a file of the index folder only: "../../.claude/
    // settings" would have read and written outside the store)
    if !plain_name(name) {
        return None;
    }
    let file = index_dir(store).join(name);
    let fresh = std::fs::metadata(&file).ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() < INDEX_SECS);
    if fresh && let Some(j) = std::fs::read(&file).ok().and_then(|b| crate::json::parse(&b).ok()) {
        return Some(j);
    }
    match web_text(url, check, "application/json") {
        Ok(text) => {
            let j = crate::json::parse(text.as_bytes()).ok()?;
            keep(&file, text.as_bytes());
            Some(j)
        }
        Err(_) => std::fs::read(&file).ok().and_then(|b| crate::json::parse(&b).ok()),
    }
}

/// Whether a name is a plain file name: letters, digits and "_.~-" only, at most 120 of them, not
/// starting with '.', no "..".
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 120 && !name.starts_with('.') && !name.contains("..") && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '~' | '-'))
}

/// A file written whole or not at all (a copy beside it, then renamed), only its owner reading it.
fn keep(file: &Path, bytes: &[u8]) {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let Some(dir) = file.parent() else { return };
    if std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(".{}.{}", file.file_name().and_then(|n| n.to_str()).unwrap_or("index"), std::process::id()));
    let written = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| std::io::Write::write_all(&mut f, bytes));
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, file);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ------------------------------------------------------------------------ documentation sets

/// A documentation set of DevDocs' list.
#[derive(Debug, Clone)]
pub struct Set {
    pub name: String,
    pub slug: String,
    alias: Option<String>,
    version: String,
}

impl Set {
    /// As a person names it: "Python 3.13", "Git".
    pub fn title(&self) -> String {
        if self.version.is_empty() { self.name.clone() } else { format!("{} {}", self.name, self.version) }
    }
}

/// The sets of DevDocs' list, in its order (the newest version of a set first).
fn sets_of(list: &Json) -> Vec<Set> {
    list.arr()
        .unwrap_or(&[])
        .iter()
        .filter_map(|d| {
            // (a slug is a file's name and a part of an address: a plain one only)
            let slug = d.get("slug")?.str()?;
            if !plain_name(slug) || slug.len() > 100 {
                return None;
            }
            Some(Set {
                name: d.get("name")?.str()?.to_string(),
                slug: slug.to_string(),
                alias: d.get("alias").and_then(Json::str).filter(|a| !a.is_empty()).map(str::to_string),
                version: d.get("version").and_then(Json::str).unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Names of sets that are common words or a letter: taken only as written, a capital first, and
/// not as the question's first word ("Go" in "In Go, what does strings.Fields() split on").
const WORDS: &[&str] = &[
    "go", "c", "d", "r", "express", "async", "flask", "click", "chai", "fish", "yarn", "composer", "puppeteer", "chef", "crystal", "dart", "react", "julia", "nim", "love", "phaser", "jest", "mocha", "sinon", "gulp", "grunt",
    "bower", "salt", "falcon", "bottle", "pony", "q", "backbone.js", "underscore.js", "moment.js", "ember.js", "knockout.js", "meteor", "apache pig", "matplotlib", "requests",
];

/// Other names the sets go by.
const ALSO: &[(&str, &str)] = &[("golang", "Go"), ("nodejs", "Node.js"), ("node", "Node.js"), ("postgres", "PostgreSQL"), ("psql", "PostgreSQL"), ("ts", "TypeScript"), ("k8s", "Kubernetes"), ("sqlite3", "SQLite"), ("bash's", "Bash")];

/// The question's words as written, split at spaces and at punctuation other than what a name
/// holds (C++, C#, Node.js, a version 3.12).
fn tokens(task: &str) -> Vec<String> {
    task.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '?' | '!' | '(' | ')' | '[' | ']' | '"' | '`' | ':'))
        .map(|t| t.trim_matches(|c: char| matches!(c, '.' | '\'' | '-')).trim_end_matches("'s").trim_end_matches("’s").to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// The documentation sets a question names, in the order it names them: by name ("Python",
/// "Node.js", "PostgreSQL"), alias ("py", "js", "pg") or another name ("golang", "postgres"); a
/// version after the name picks that version's set ("Python 3.12", "PostgreSQL 16"), else the
/// newest.
pub fn named_sets(task: &str, sets: &[Set]) -> Vec<Set> {
    let toks = tokens(task);
    let mut out: Vec<Set> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let low = t.to_lowercase();
        let also = ALSO.iter().find(|(a, _)| *a == low).map(|(_, n)| n.to_lowercase());
        let found: Option<&Set> = sets.iter().find(|s| {
            let name = s.name.to_lowercase();
            let by_name = name == low && (!WORDS.contains(&name.as_str()) || (t.as_str() == s.name && i > 0));
            let by_alias = s.alias.as_deref().is_some_and(|a| a.to_lowercase() == low) && low.len() >= 2;
            let by_slug = s.slug.split('~').next() == Some(low.as_str()) && !WORDS.contains(&low.as_str()) && low.len() >= 3;
            by_name || by_alias || by_slug || also.as_deref() == Some(name.as_str())
        });
        let Some(first) = found else { continue };
        // (a version after the name: that version's set, when the list has it)
        let pick = toks
            .get(i + 1)
            .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()) || v.starts_with('v') && v[1..].chars().next().is_some_and(|c| c.is_ascii_digit()))
            .and_then(|v| {
                let v = v.trim_start_matches('v');
                sets.iter().find(|s| s.name == first.name && (s.version == v || s.version.starts_with(&format!("{v} ")) || s.slug.ends_with(&format!("~{v}")) || s.slug.ends_with(&format!("~{}", v.replace('.', "_")))))
            })
            .unwrap_or(first);
        if !out.iter().any(|s| s.name == pick.name) {
            out.push(pick.clone());
        }
    }
    out
}

/// What a question names as code, lower case, as an index names it: a call (zip(), fs.mkdir()),
/// a dotted or :: name (str.removeprefix, Vec::retain, Array.prototype.flat is array.flat), a name
/// in backticks, a word in capitals (COALESCE); then pairs of words ("git rebase") and words, each
/// with how it was found.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Code,
    Pair,
    Word,
}

/// Capitals that are not a function's name.
const CAPS: &[&str] = &["HTTP", "HTTPS", "JSON", "API", "SQL", "URL", "URI", "CSS", "HTML", "CLI", "GUI", "AWS", "YAML", "XML", "UTF", "ASCII", "TCP", "UDP", "DNS", "TLS", "SSL", "SSH", "CPU", "GPU", "RAM", "OS", "PDF", "CSV", "REST", "RFC", "ISO", "UUID", "JWT", "NULL", "TRUE", "FALSE", "AND", "NOT", "THE"];

fn norm_code(s: &str) -> String {
    let mut t = s.trim().trim_matches('`').to_lowercase();
    if let Some(i) = t.find('(') {
        t.truncate(i);
    }
    t.replace(".prototype.", ".").trim_end_matches(['.', ':']).to_string()
}

fn wanted(task: &str) -> Vec<(String, Kind)> {
    let mut out: Vec<(String, Kind)> = Vec::new();
    let push = |s: String, k: Kind, out: &mut Vec<(String, Kind)>| {
        if !s.is_empty() && !out.iter().any(|(x, _)| *x == s) {
            out.push((s, k));
        }
    };
    // backticks
    let mut rest = task;
    while let Some(a) = rest.find('`') {
        let Some(b) = rest[a + 1..].find('`') else { break };
        push(norm_code(&rest[a + 1..a + 1 + b]), Kind::Code, &mut out);
        rest = &rest[a + 2 + b..];
    }
    let raws: Vec<&str> = task.split_whitespace().collect();
    for (ri, raw) in raws.iter().enumerate() {
        // (a word before an option is a command: "set -o pipefail", "tar -x")
        if raws.get(ri + 1).is_some_and(|n| n.starts_with('-') && n.len() >= 2 && n.chars().nth(1).is_some_and(|c| c.is_ascii_alphabetic() || c == '-'))
            && raw.chars().all(|c| c.is_ascii_lowercase() || c == '-')
            && raw.len() >= 2
        {
            push(raw.to_string(), Kind::Code, &mut out);
        }
        let t = raw.trim_matches(|c: char| matches!(c, ',' | ';' | '?' | '!' | '"' | '\'' | '`' | '[' | ']')).trim_end_matches("'s").trim_end_matches("’s");
        let t = t.trim_end_matches(['.', ':']);
        let ident = |s: &str| s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
        let core = t.split('(').next().unwrap_or(t);
        let parts: Vec<&str> = core.split("::").flat_map(|p| p.split('.')).collect();
        let call = t.contains('(') && ident(parts.last().copied().unwrap_or(""));
        let dotted = parts.len() >= 2 && parts.iter().all(|p| ident(p)) && !core.starts_with("www.") && bare_address(core).is_none();
        if (call && parts.iter().all(|p| ident(p))) || dotted {
            push(norm_code(core), Kind::Code, &mut out);
        } else if t.len() >= 3 && t.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !CAPS.contains(&t) {
            push(t.to_lowercase(), Kind::Code, &mut out);
        }
    }
    let words: Vec<String> = task
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .map(|w| w.trim_matches('-').to_lowercase())
        .filter(|w| w.len() >= 2 && w.chars().next().is_some_and(char::is_alphabetic))
        .collect();
    // (pairs, "git rebase": kept by docs_page only when the first is the set's own name)
    for w in words.windows(2) {
        if !crate::clean::STOP.contains(&w[1].as_str()) {
            push(format!("{} {}", w[0], w[1]), Kind::Pair, &mut out);
        }
    }
    // (a word the question calls a builtin, a function, a header ...: "the trap builtin", "the
    // Retry-After header"; a word alone names no entry, "package" is no glossary's entry for it)
    for (i, w) in words.iter().enumerate() {
        if words.get(i + 1).is_some_and(|k| KINDS.contains(&k.as_str())) && w.len() >= 2 && !crate::clean::STOP.contains(&w.as_str()) && !COMMON.contains(&w.as_str()) {
            push(w.clone(), Kind::Word, &mut out);
        }
    }
    out
}

/// The kinds of things a question names a word as.
const KINDS: &[&str] = &[
    "builtin", "built-in", "command", "subcommand", "function", "method", "keyword", "operator", "statement", "option", "flag", "directive", "module", "attribute", "property", "header", "element", "macro", "trait",
    "type", "class", "struct", "enum", "hook", "decorator", "field", "parameter", "variable", "setting", "event", "selector", "pseudo-class", "unit", "constant", "exception", "tag", "instruction", "clause",
];

/// Words of a question that name no entry of a documentation set.
const COMMON: &[&str] = &["when", "where", "why", "who", "whom", "whose", "mean", "means", "meaning", "default", "return", "returns", "option", "options", "argument", "arguments", "function", "method", "class", "change", "changes", "version", "added", "used", "work", "works", "happen", "happens", "value", "values", "set", "get"];

/// An index entry's names as a question would write them: lower case, without its arguments or
/// "prototype." (fs.mkdir(path[, options]) is fs.mkdir; Array.prototype.flat is array.flat), and
/// the name after ": " (PostgreSQL's "Conditional Expressions: COALESCE" is coalesce too).
fn entry_names(name: &str) -> (String, Option<String>) {
    let full = norm_code(name);
    let after = full.rsplit_once(": ").map(|(_, a)| a.trim().to_string());
    (full, after)
}

/// How well an entry's names match one thing a question names (0: not at all).
fn score(want: &str, kind: Kind, full: &str, after: Option<&str>) -> u32 {
    let qualified = |w: &str| full.ends_with(&format!(".{w}")) || full.ends_with(&format!("::{w}"));
    match kind {
        Kind::Code if full == want => 100,
        Kind::Code if after == Some(want) => 90,
        Kind::Code if want.contains(['.', ':']) && qualified(want) => 80,
        Kind::Code if qualified(want) => 40,
        Kind::Pair if full == want => 70,
        // (a word: an entry named only so, "set" in bash; or a section's name after ": ")
        Kind::Word if full == want || after == Some(want) => 30,
        _ => 0,
    }
}

/// The best entry of an index for a question: (its name, its path), or None when nothing it
/// names is an entry. (Ties: the shorter name, then the first.)
fn best_entry(index: &Json, want: &[(String, Kind)]) -> Option<(String, String)> {
    let mut best: Option<(u32, usize, String, String)> = None;
    for e in index.get("entries").and_then(Json::arr).unwrap_or(&[]) {
        let (Some(name), Some(path)) = (e.get("name").and_then(Json::str), e.get("path").and_then(Json::str)) else { continue };
        let (full, after) = entry_names(name);
        let s = want.iter().map(|(w, k)| score(w, *k, &full, after.as_deref())).max().unwrap_or(0);
        if s > 0 && best.as_ref().is_none_or(|b| s > b.0 || s == b.0 && full.len() < b.1) {
            best = Some((s, full.len(), name.to_string(), path.to_string()));
        }
    }
    best.map(|(_, _, n, p)| (n, p))
}

/// A page of a documentation set found for a question: its address (at its entry), the set, the
/// entry.
#[derive(Debug)]
pub struct DocsHit {
    pub url: String,
    pub set: String,
    pub entry: String,
}

/// The page of a documentation set the question names, at the entry it asks about.
pub fn docs_page(task: &str, store: &Path, check: Check) -> Option<DocsHit> {
    let list = cached(store, "docs.json", DEVDOCS_LIST, check)?;
    let sets = named_sets(task, &sets_of(&list));
    if sets.is_empty() {
        return None;
    }
    let want = wanted(task);
    // (a set's own name is no entry of it: "git" in "git rebase" stays in the pair)
    for set in &sets {
        let own = |w: &str| w == set.name.to_lowercase() || set.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(w)) || set.slug.split('~').next() == Some(w);
        let want: Vec<(String, Kind)> = want
            .iter()
            .filter(|(w, k)| match k {
                Kind::Pair => w.split_once(' ').is_some_and(|(a, _)| own(a)),
                Kind::Word => !own(w),
                Kind::Code => true,
            })
            .cloned()
            .collect();
        let Some(index) = cached(store, &format!("{}.json", set.slug), &format!("{DEVDOCS_PAGES}/{}/index.json", set.slug), check) else { continue };
        let Some((entry, path)) = best_entry(&index, &want) else { continue };
        let (page, anchor) = path.split_once('#').map_or((path.as_str(), None), |(p, a)| (p, Some(a)));
        // (an entry's path, web data too, stays in its set: no "..", no other host)
        if page.contains("..") || page.starts_with('/') || page.contains("://") || page.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\') {
            continue;
        }
        let url = format!("{DEVDOCS_PAGES}/{}/{page}.html{}", set.slug, anchor.map_or(String::new(), |a| format!("#{a}")));
        return Some(DocsHit { url, set: set.title(), entry });
    }
    None
}

// --------------------------------------------------------------------------------- packages

/// A package registry a question names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Registry {
    PyPI,
    Npm,
    Crates,
}

/// The registry and the package a question asks about ("the requests package on PyPI", "react
/// on npm", "the serde crate"), when it asks for a package's facts (its version, license,
/// release).
pub fn package_named(task: &str) -> Option<(Registry, String)> {
    let low = task.to_lowercase();
    let asks = ["version", "latest", "newest", "current", "release", "released", "license", "licence", "requires", "require", "python versions", "node versions", "published"].iter().any(|w| low.contains(w));
    if !asks {
        return None;
    }
    let reg = if low.contains("pypi") || low.contains("pip install") || low.contains("python package") {
        Registry::PyPI
    } else if low.split(|c: char| !c.is_alphanumeric()).any(|w| w == "npm") || low.contains("node package") || low.contains("yarn add") {
        Registry::Npm
    } else if low.contains("crates.io") || low.split(|c: char| !c.is_alphanumeric()).any(|w| w == "crate") || low.contains("cargo add") {
        Registry::Crates
    } else {
        return None;
    };
    let valid = |n: &str| -> bool {
        !n.is_empty()
            && n.len() <= 214
            && match reg {
                Registry::PyPI => n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                Registry::Npm => n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '@' | '/')) && !n.starts_with('.'),
                Registry::Crates => n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
            }
    };
    // a name in backticks
    if let Some(a) = task.find('`')
        && let Some(b) = task[a + 1..].find('`')
    {
        let n = task[a + 1..a + 1 + b].trim();
        if valid(n) {
            return Some((reg, n.to_string()));
        }
    }
    let words: Vec<&str> = task.split_whitespace().map(|w| w.trim_matches(|c: char| matches!(c, ',' | ';' | '?' | '!' | '"' | '\'' | '(' | ')' | '.' | ':'))).collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let skip = ["the", "a", "an", "of", "on", "in", "for", "its", "this", "that", "python", "node", "npm", "pypi", "rust", "latest", "newest", "current", "version", "package", "crate", "library"];
    let ok = |i: usize| i < words.len() && !skip.contains(&lower[i].as_str()) && valid(words[i]) && words[i].chars().any(|c| c.is_ascii_alphabetic());
    for (i, w) in lower.iter().enumerate() {
        // "the requests package", "the serde crate", "react on npm", "package requests"
        let cand = match w.as_str() {
            "package" | "crate" | "library" | "module" if i > 0 && ok(i - 1) => Some(i - 1),
            "package" | "crate" if ok(i + 1) => Some(i + 1),
            "on" | "from" | "in" if lower.get(i + 1).is_some_and(|n| ["npm", "pypi", "crates.io"].contains(&n.as_str())) && i > 0 && ok(i - 1) => Some(i - 1),
            "of" if ok(i + 1) && i > 0 && ["version", "release", "license"].iter().any(|v| lower[i - 1].starts_with(v)) => Some(i + 1),
            _ => None,
        };
        if let Some(c) = cand {
            let n = if reg == Registry::Npm { words[c].to_lowercase() } else { words[c].to_string() };
            return Some((reg, n));
        }
    }
    None
}

/// The registry and the package of a registry's page or API address the question gives
/// (pypi.org/project/requests/, pypi.org/pypi/requests/json, www.npmjs.com/package/react,
/// registry.npmjs.org/react/latest, crates.io/crates/serde, crates.io/api/v1/crates/serde): its
/// record is read instead of the page (2026-11-25: npm's and crates.io's pages are programs that
/// show nothing without a browser, and three jobs took a step more each).
pub fn registry_address(url: &str) -> Option<(Registry, String)> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let dec = |s: &str| s.replace("%2F", "/").replace("%2f", "/").replace("%40", "@");
    match (host.to_ascii_lowercase().as_str(), parts.as_slice()) {
        ("pypi.org" | "www.pypi.org", ["project", name, ..]) | ("pypi.org" | "www.pypi.org", ["pypi", name, ..]) => Some((Registry::PyPI, name.to_string())),
        ("www.npmjs.com" | "npmjs.com", ["package", scope, name, ..]) if scope.starts_with('@') => Some((Registry::Npm, format!("{scope}/{name}"))),
        ("www.npmjs.com" | "npmjs.com", ["package", name, ..]) => Some((Registry::Npm, dec(name))),
        ("registry.npmjs.org", [scope, name, ..]) if scope.starts_with('@') => Some((Registry::Npm, format!("{scope}/{name}"))),
        ("registry.npmjs.org", [name, ..]) => Some((Registry::Npm, dec(name))),
        ("crates.io", ["crates", name, ..]) | ("crates.io", ["api", "v1", "crates", name, ..]) => Some((Registry::Crates, name.to_string())),
        _ => None,
    }
}

/// A package's facts from its registry's public record: (the text, where it came from).
pub fn package_facts(reg: Registry, name: &str, check: Check) -> Result<(String, String), String> {
    let enc = crate::page::enc;
    match reg {
        Registry::PyPI => {
            let url = format!("https://pypi.org/pypi/{}/json", enc(name));
            let j = web_json(&url, check)?;
            let info = j.get("info").ok_or("no info in PyPI's record")?;
            let s = |k: &str| info.get(k).and_then(Json::str).filter(|v| !v.trim().is_empty()).map(str::to_string);
            let version = s("version").ok_or("no version in PyPI's record")?;
            let uploaded = j.get("releases").and_then(|r| r.get(&version)).and_then(Json::arr).and_then(|files| files.iter().filter_map(|f| f.get("upload_time").and_then(Json::str)).min()).map(|t| t.chars().take(10).collect::<String>());
            let license = s("license_expression").or_else(|| s("license").filter(|l| l.len() <= 80));
            let mut facts = vec![format!("latest version {version}{}", uploaded.map_or(String::new(), |d| format!(", uploaded {d}")))];
            if let Some(r) = s("requires_python") {
                facts.push(format!("requires Python {r}"));
            }
            if let Some(l) = license {
                facts.push(format!("license {l}"));
            }
            if let Some(sum) = s("summary") {
                facts.push(format!("summary: {sum}"));
            }
            Ok((format!("PyPI's record of {name} (https://pypi.org/project/{name}/): {}.", facts.join("; ")), format!("PyPI's public record of {name}, read by fewer (no model)")))
        }
        Registry::Npm => {
            let url = format!("https://registry.npmjs.org/{}/latest", name.replace('/', "%2F"));
            let j = web_json(&url, check)?;
            let s = |k: &str| j.get(k).and_then(Json::str).filter(|v| !v.trim().is_empty()).map(str::to_string);
            let version = s("version").ok_or("no version in npm's record")?;
            let mut facts = vec![format!("latest version {version}")];
            if let Some(l) = s("license") {
                facts.push(format!("license {l}"));
            }
            if let Some(n) = j.get("engines").and_then(|e| e.get("node")).and_then(Json::str) {
                facts.push(format!("needs Node {n}"));
            }
            if let Some(d) = s("description") {
                facts.push(format!("description: {d}"));
            }
            Ok((format!("npm's record of {name} (https://www.npmjs.com/package/{name}): {}.", facts.join("; ")), format!("npm's public record of {name}, read by fewer (no model)")))
        }
        Registry::Crates => {
            let url = format!("https://crates.io/api/v1/crates/{}", enc(name));
            let j = web_json(&url, check)?;
            let c = j.get("crate").ok_or("no crate in crates.io's record")?;
            let s = |k: &str| c.get(k).and_then(Json::str).filter(|v| !v.trim().is_empty()).map(str::to_string);
            let version = s("max_stable_version").or_else(|| s("max_version")).ok_or("no version in crates.io's record")?;
            let latest = j.get("versions").and_then(Json::arr).and_then(|v| v.iter().find(|x| x.get("num").and_then(Json::str) == Some(version.as_str())));
            let created = latest.and_then(|v| v.get("created_at")).and_then(Json::str).map(|t| t.chars().take(10).collect::<String>());
            let mut facts = vec![format!("latest version {version}{}", created.map_or(String::new(), |d| format!(", published {d}")))];
            if let Some(l) = latest.and_then(|v| v.get("license")).and_then(Json::str) {
                facts.push(format!("license {l}"));
            }
            if let Some(r) = latest.and_then(|v| v.get("rust_version")).and_then(Json::str) {
                facts.push(format!("needs Rust {r}"));
            }
            if let Some(d) = s("description") {
                facts.push(format!("description: {}", d.trim()));
            }
            Ok((format!("crates.io's record of {name} (https://crates.io/crates/{name}): {}.", facts.join("; ")), format!("crates.io's public record of {name}, read by fewer (no model)")))
        }
    }
}

// ------------------------------------------------------------------- an article, an error

/// Whether a question is about an error message: a quoted message, or one written as a program
/// writes it ("TypeError: ...", "fatal: ...", "error[E0382]").
pub fn about_an_error(task: &str) -> bool {
    let low = task.to_lowercase();
    task.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '(' | ')'));
        (w.ends_with("Error:") || w.ends_with("Exception:") || w.starts_with("error[E")) && w.len() > 6
    }) || ["fatal:", "error:", "panicked at", "segmentation fault", "traceback (most recent call last)"].iter().any(|m| low.contains(m))
}

/// A question's words for matching titles: lower case, a dot kept inside (3.13), "'s" taken off.
fn title_words(t: &str) -> Vec<String> {
    t.replace(['’', '\''], " ")
        .split(|c: char| !(c.is_alphanumeric() || c == '.'))
        .map(|w| w.trim_matches('.').to_lowercase())
        .filter(|w| !w.is_empty() && w != "s")
        .collect()
}

/// Words a title may add to what the question names: "History of Python", "List of HTTP status
/// codes", "Rust version history".
const TITLE_EXTRA: &[&str] = &["history", "list", "timeline", "version", "versions", "release", "releases", "of"];

/// The names and numbers of a question, for a search of few words: its words that start with a
/// capital (not its first word) or a digit ("Git", "Python 3.13", "HTTP 418").
pub fn names_and_numbers(task: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in task.split_whitespace().enumerate() {
        let w = raw.trim_matches(|c: char| !c.is_alphanumeric()).trim_end_matches("'s").trim_end_matches("’s");
        let named = i > 0 && w.chars().next().is_some_and(char::is_uppercase);
        let number = w.chars().next().is_some_and(|c| c.is_ascii_digit());
        if (named || number) && !w.is_empty() && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out.join(" ")
}

/// Wikipedia's articles on what a question asks about (at most two, best first): of its search's
/// first five results, those whose title the question names (its words, without what the
/// brackets add, and with "History of", "List of": "Rust" of "Rust (programming language)",
/// "History of Python"), else the first whose snippet holds a number the question holds ("418");
/// the search made again with the question's names and numbers when its words find none
/// (2026-11-25: "Git's first release" found the band "The Gits" and GitHub, "Git" found Git).
pub fn wikipedia_articles(task: &str, query: &str, check: Check) -> Vec<(String, String)> {
    let asked = title_words(task);
    let numbers: Vec<&String> = asked.iter().filter(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit())).collect();
    let search = |q: &str| -> Vec<(String, String)> {
        let url = format!("https://en.wikipedia.org/w/api.php?action=query&list=search&srlimit=5&srprop=snippet&format=json&formatversion=2&utf8=1&srsearch={}", crate::page::enc(q));
        let Ok(j) = web_json(&url, check) else { return Vec::new() };
        j.get("query")
            .and_then(|x| x.get("search"))
            .and_then(Json::arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(|r| Some((r.get("title")?.str()?.to_string(), crate::page::html_text(r.get("snippet").and_then(Json::str).unwrap_or("")).to_lowercase())))
            .collect()
    };
    let named = |t: &str| {
        let main = t.split(" (").next().unwrap_or(t);
        let tw: Vec<String> = title_words(main).into_iter().filter(|w| !crate::clean::STOP.contains(&w.as_str()) && !TITLE_EXTRA.contains(&w.as_str())).collect();
        !tw.is_empty() && tw.iter().all(|w| asked.contains(w))
    };
    let pick = |found: &[(String, String)]| -> Vec<String> {
        let mut out: Vec<String> = found.iter().filter(|(t, _)| named(t)).map(|(t, _)| t.clone()).take(2).collect();
        if out.is_empty()
            && let Some((t, _)) = found.iter().find(|(_, s)| numbers.iter().any(|n| title_words(s).contains(n)))
        {
            out.push(t.clone());
        }
        out
    };
    let mut titles = pick(&search(query));
    if titles.is_empty() {
        let few = names_and_numbers(task);
        if !few.is_empty() && few != query {
            titles = pick(&search(&few));
        }
    }
    titles.into_iter().map(|t| (format!("https://en.wikipedia.org/wiki/{}", crate::page::enc(&t.replace(' ', "_"))), t)).collect()
}

/// The most of an article's introduction shown (bytes).
const INTRO: usize = 1_500;

/// Wikipedia's introduction of an article (its text before its first section: what the article
/// is about, and most often who made it and when), at most INTRO bytes; None when it has none
/// (2026-11-25: the teapot's RFC 2324 and Git's Linus Torvalds are said there, and three jobs read
/// the kept article again for them).
pub fn wikipedia_intro(title: &str, check: Check) -> Option<String> {
    let url = format!("https://en.wikipedia.org/w/api.php?action=query&prop=extracts&exintro=1&explaintext=1&format=json&formatversion=2&redirects=1&titles={}", crate::page::enc(title));
    let j = web_json(&url, check).ok()?;
    let text = j.get("query")?.get("pages")?.arr()?.first()?.get("extract")?.str()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let (cut, more) = crate::clean::cut_at(&text, INTRO);
    Some(if more { format!("{} …", cut.trim_end()) } else { text })
}

// ------------------------------------------------------------------------- product releases

/// Other names of endoflife.date's products.
const PRODUCT_ALSO: &[(&str, &str)] = &[("node", "nodejs"), ("node.js", "nodejs"), ("postgres", "postgresql"), ("k8s", "kubernetes"), ("golang", "go"), ("macos", "macos"), ("dotnet", "dotnet"), (".net", "dotnet"), ("java", "oracle-jdk"), ("ubuntu", "ubuntu"), ("windows", "windows")];

/// Whether a question asks about a product's releases: when a version came out, the latest one,
/// when support ends (not about its first release or who made it: a history's question, endoflife
/// keeps only the versions still listed; 2026-11-25, "In what year was Git first released" got
/// git's newest releases).
fn asks_release(task: &str) -> bool {
    let low = task.to_lowercase();
    let words: Vec<&str> = low.split(|c: char| !c.is_alphanumeric()).collect();
    let history = words.iter().any(|w| ["first", "initial", "initially", "originally", "created", "invented", "wrote", "founded"].contains(w));
    !history && (["released", "release date", "come out", "came out", "end of life", "supported until", "support end", "support until", "still supported"].iter().any(|w| low.contains(w)) || asks_newest(&low) || words.iter().any(|w| ["release", "eol", "lts"].contains(w)))
}

/// Whether a question asks for the newest version: "latest version", "current release".
fn asks_newest(low: &str) -> bool {
    ["latest", "newest", "current", "most recent"].iter().any(|w| low.contains(w))
}

/// The product a question names, of endoflife.date's list, and the version it names after it (a
/// cycle: "Python 3.13", "Node.js 22", "PostgreSQL 16").
pub fn product_named(task: &str, products: &[String]) -> Option<(String, Option<String>)> {
    let toks = tokens(task);
    for (i, t) in toks.iter().enumerate() {
        let low = t.to_lowercase();
        let name = PRODUCT_ALSO.iter().find(|(a, _)| *a == low).map(|(_, p)| p.to_string()).unwrap_or(low.clone());
        // (a product's name that is a common word, "go", only written with a capital, not first)
        if !products.contains(&name) || (WORDS.contains(&low.as_str()) && (i == 0 || t.chars().next().is_some_and(char::is_lowercase))) {
            continue;
        }
        let version = toks.get(i + 1).map(|v| v.trim_start_matches(['v', 'V']).to_string()).filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()));
        return Some((name, version));
    }
    None
}

/// endoflife.date's record of a product's releases, for a question: the version it names (when
/// it came out, its latest patch, when support ends), else the newest three: (the text, where it
/// came from).
pub fn releases(task: &str, store: &Path, check: Check) -> Option<(String, String)> {
    if !asks_release(task) {
        return None;
    }
    let list = cached(store, "endoflife-all.json", "https://endoflife.date/api/all.json", check)?;
    let products: Vec<String> = list.arr()?.iter().filter_map(|p| p.str().map(str::to_string)).collect();
    let (product, version) = product_named(task, &products)?;
    let cycles = web_json(&format!("https://endoflife.date/api/{}.json", crate::page::enc(&product)), check).ok()?;
    let cycles = cycles.arr()?;
    let say = |c: &Json| -> Option<String> {
        let s = |k: &str| c.get(k).and_then(|v| v.str().map(str::to_string).or_else(|| v.bool().map(|b| b.to_string())));
        let mut facts = vec![format!("{} released {}", s("cycle")?, s("releaseDate").unwrap_or_else(|| "?".into()))];
        if let Some(l) = s("latest") {
            facts.push(format!("its latest {l}{}", s("latestReleaseDate").map_or(String::new(), |d| format!(" ({d})"))));
        }
        if let Some(sup) = s("support").filter(|v| v != "false" && v != "true") {
            facts.push(format!("active support until {sup}"));
        }
        match s("eol").as_deref() {
            Some("false") => facts.push("no end of life set".into()),
            Some("true") => facts.push("past its end of life".into()),
            Some(d) => facts.push(format!("end of life {d}")),
            None => {}
        }
        Some(facts.join(", "))
    };
    // (a version named: its release line, none when it is not one, "Rust 2024" is an edition;
    // none named: the newest three, only when the newest is asked for)
    let mut lines: Vec<String> = Vec::new();
    match &version {
        Some(v) => {
            let c = cycles.iter().find(|c| c.get("cycle").and_then(Json::str).is_some_and(|x| x == v || v.starts_with(&format!("{x}."))))?;
            lines.push(say(c)?);
        }
        None if asks_newest(&task.to_lowercase()) => lines.push(format!("the newest: {}", cycles.iter().take(3).filter_map(say).collect::<Vec<_>>().join("; "))),
        None => return None,
    }
    Some((format!("endoflife.date's record of {product} (https://endoflife.date/{product}): {}.", lines.join("; ")), format!("endoflife.date's public record of {product}, read by fewer (no model)")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sets() -> Vec<Set> {
        let list = crate::json::parse(
            br#"[{"name":"Python","slug":"python~3.14","alias":"py","version":"3.14"},{"name":"Python","slug":"python~3.13","alias":"py","version":"3.13"},
                 {"name":"Go","slug":"go","alias":null,"version":null},{"name":"Git","slug":"git","version":null},{"name":"Bash","slug":"bash","version":null},
                 {"name":"JavaScript","slug":"javascript","alias":"js","version":null},{"name":"Node.js","slug":"node","version":""},
                 {"name":"PostgreSQL","slug":"postgresql~17","alias":"pg","version":"17"},{"name":"PostgreSQL","slug":"postgresql~16","alias":"pg","version":"16"},
                 {"name":"React","slug":"react","version":""},{"name":"Rust","slug":"rust","alias":"rs","version":null}]"#,
        )
        .unwrap();
        sets_of(&list)
    }

    #[test]
    fn web_names_stay_in_the_store() {
        // (a slug from DevDocs' list never leaves the index folder)
        assert!(plain_name("python~3.14.json") && plain_name("docs.json") && plain_name("postgresql~17.json"));
        for bad in ["../../../.claude/settings.json", "..json", ".hidden.json", "a/b.json", "a\\b.json", "", "x y.json"] {
            assert!(!plain_name(bad), "{bad}");
        }
        let list = crate::json::parse(br#"[{"name":"Python","slug":"../../../.claude/settings","version":"3.14"},{"name":"Go","slug":"go","version":null}]"#).unwrap();
        let sets = sets_of(&list);
        assert_eq!(sets.iter().map(|s| s.slug.as_str()).collect::<Vec<_>>(), ["go"]);
        let store = std::env::temp_dir().join(format!("fewer-c1-{}", std::process::id()));
        assert!(cached(&store, "../../../outside.json", "https://documents.devdocs.io/x/index.json", &|_: &str| Err("no network in tests".to_string())).is_none());
        assert!(!store.join("web").exists() || !store.parent().unwrap().join("outside.json").exists());
    }

    #[test]
    fn addresses_written_without_https() {
        // a balanced ')' is the address's own; an unbalanced one ends it
        assert_eq!(bare_address("en.wikipedia.org/wiki/Rust_(programming_language) When was Rust 1.0 released?").as_deref(), Some("https://en.wikipedia.org/wiki/Rust_(programming_language)"));
        assert_eq!(bare_address("(see docs.python.org/3/library/functions.html#zip).").as_deref(), Some("https://docs.python.org/3/library/functions.html#zip"));
        assert_eq!(bare_address("(en.wikipedia.org/wiki/Rust_(programming_language))").as_deref(), Some("https://en.wikipedia.org/wiki/Rust_(programming_language)"));
        assert_eq!(bare_address("see docs.python.org/3/library/functions.html#zip, PEP 618").as_deref(), Some("https://docs.python.org/3/library/functions.html#zip"));
        assert_eq!(bare_address("(github.com/rust-lang/rust)").as_deref(), Some("https://github.com/rust-lang/rust"));
        assert_eq!(bare_address("on docs.rs/serde").as_deref(), Some("https://docs.rs/serde"));
        assert_eq!(bare_address("read 10.0.0.1:8443/admin").as_deref(), Some("https://10.0.0.1:8443/admin"));
        assert_eq!(bare_address("see 192.168.1.1/x").as_deref(), Some("https://192.168.1.1/x"));
        for no in ["what is at www.example.com?", "(doc.rust-lang.org std::vec::Vec::retain)", "what does deploy.sh do", "main.rs and setup.py", "Node.js and Vue.js", "os.path.join", "v1.2.3", "e.g. this", "example.com", "a@b.com/x", "version 10.0.0.1", "https://x.org/y"] {
            assert_eq!(bare_address(no), None, "{no}");
        }
    }

    #[test]
    fn the_sets_a_question_names() {
        let s = sets();
        let names = |q: &str| named_sets(q, &s).iter().map(|x| x.slug.clone()).collect::<Vec<_>>();
        assert_eq!(names("In Python, what does zip() raise?"), ["python~3.14"]);
        assert_eq!(names("In Python 3.13, what does zip() raise?"), ["python~3.13"]);
        assert_eq!(names("In Go, what does strings.Fields() split on?"), ["go"]);
        assert_eq!(names("Go to the docs: how does it go?"), Vec::<String>::new());
        assert_eq!(names("golang strings.Fields"), ["go"]);
        assert_eq!(names("what does git rebase --autosquash do"), ["git"]);
        assert_eq!(names("In postgres 16, what does COALESCE return"), ["postgresql~16"]);
        assert_eq!(names("node fs.mkdir recursive"), ["node"]);
        assert_eq!(names("how do components react to props in React"), ["react"]);
        assert_eq!(names("js Array.prototype.flat() and bash set"), ["javascript", "bash"]);
    }

    #[test]
    fn entries_as_a_question_names_them() {
        let idx = crate::json::parse(
            br#"{"entries":[{"name":"zip()","path":"library/functions#zip"},{"name":"zipfile","path":"library/zipfile"},{"name":"str.removeprefix()","path":"library/stdtypes#str.removeprefix"},
              {"name":"bytes.removeprefix()","path":"library/stdtypes#bytes.removeprefix"},{"name":"Array.flat","path":"global_objects/array/flat"},
              {"name":"fs.mkdir()","path":"fs#fsmkdirpath-options-callback"},{"name":"std::vec::Vec::retain","path":"std/vec/struct.vec#method.retain"},
              {"name":"std::bstr::ByteString::retain","path":"std/bstr/struct.bytestring#method.retain"},{"name":"Conditional Expressions: COALESCE","path":"functions-conditional#FUNCTIONS-COALESCE-NVL-IFNULL"},
              {"name":"git rebase","path":"git-rebase"},{"name":"set","path":"the-set-builtin#index-set"},{"name":"strings.Fields()","path":"strings/index#Fields"}]}"#,
        )
        .unwrap();
        let best = |q: &str| best_entry(&idx, &wanted(q)).map(|(n, _)| n);
        assert_eq!(best("what exception does zip() raise").as_deref(), Some("zip()"));
        assert_eq!(best("what does str.removeprefix() return").as_deref(), Some("str.removeprefix()"));
        assert_eq!(best("What is the default depth of Array.prototype.flat()?").as_deref(), Some("Array.flat"));
        assert_eq!(best("fs.mkdir with recursive: true").as_deref(), Some("fs.mkdir()"));
        assert_eq!(best("in which order does Vec::retain visit").as_deref(), Some("std::vec::Vec::retain"));
        assert_eq!(best("what does COALESCE() return when all are null").as_deref(), Some("Conditional Expressions: COALESCE"));
        assert_eq!(best("what does git rebase --autosquash do").as_deref(), Some("git rebase"));
        assert_eq!(best("what does set -o pipefail change").as_deref(), Some("set"));
        assert_eq!(best("what does strings.Fields() split on").as_deref(), Some("strings.Fields()"));
        assert_eq!(best("how fast is a zebra"), None);
    }

    #[test]
    fn packages_a_question_names() {
        assert_eq!(package_named("What is the latest version of the requests package on PyPI?"), Some((Registry::PyPI, "requests".into())));
        assert_eq!(package_named("What is the latest version of react on npm, and its license?"), Some((Registry::Npm, "react".into())));
        assert_eq!(package_named("What is the newest version of the serde crate on crates.io?"), Some((Registry::Crates, "serde".into())));
        assert_eq!(package_named("latest version of `@types/node` on npm"), Some((Registry::Npm, "@types/node".into())));
        assert_eq!(package_named("what does the requests package do"), None);
        assert_eq!(package_named("What is the latest version of Kubernetes?"), None);
    }

    #[test]
    fn registry_addresses() {
        assert_eq!(registry_address("https://pypi.org/pypi/requests/json"), Some((Registry::PyPI, "requests".into())));
        assert_eq!(registry_address("https://pypi.org/project/requests/2.34.2/"), Some((Registry::PyPI, "requests".into())));
        assert_eq!(registry_address("https://www.npmjs.com/package/react"), Some((Registry::Npm, "react".into())));
        assert_eq!(registry_address("https://www.npmjs.com/package/@types/node"), Some((Registry::Npm, "@types/node".into())));
        assert_eq!(registry_address("https://registry.npmjs.org/react/latest"), Some((Registry::Npm, "react".into())));
        assert_eq!(registry_address("https://crates.io/crates/serde"), Some((Registry::Crates, "serde".into())));
        assert_eq!(registry_address("https://crates.io/api/v1/crates/serde"), Some((Registry::Crates, "serde".into())));
        assert_eq!(registry_address("https://docs.rs/serde"), None);
        assert_eq!(registry_address("https://pypi.org/"), None);
    }

    #[test]
    fn products_and_releases() {
        let products: Vec<String> = ["python", "nodejs", "go", "postgresql", "rust", "kubernetes"].iter().map(|s| s.to_string()).collect();
        assert_eq!(product_named("When was Python 3.13 released?", &products), Some(("python".into(), Some("3.13".into()))));
        assert_eq!(product_named("Is Node.js 22 still supported?", &products), Some(("nodejs".into(), Some("22".into()))));
        assert_eq!(product_named("When does postgres 16 reach end of life?", &products), Some(("postgresql".into(), Some("16".into()))));
        assert_eq!(product_named("When was Go 1.22 released?", &products), Some(("go".into(), Some("1.22".into()))));
        assert_eq!(product_named("go and see when it was released", &products), None);
        assert!(asks_release("When was Python 3.13 released?") && asks_release("What is the latest version of Kubernetes?") && !asks_release("What does zip() raise?"));
        assert!(!asks_release("In what year was Git first released, and who wrote it?"));
    }

    #[test]
    fn names_for_a_short_search() {
        assert_eq!(names_and_numbers("In what year did Git's first release come out, and who wrote it?"), "Git");
        assert_eq!(names_and_numbers("When was Python 3.13 released?"), "Python 3.13");
        assert_eq!(names_and_numbers("What does HTTP status code 418 mean, and which RFC first defined it?"), "HTTP 418 RFC");
        assert_eq!(title_words("Git's first release (2005)"), ["git", "first", "release", "2005"]);
    }

    #[test]
    fn error_questions() {
        assert!(about_an_error("What does Python's 'RuntimeError: dictionary changed size during iteration' mean?"));
        assert!(about_an_error("git says fatal: refusing to merge unrelated histories"));
        assert!(about_an_error("rustc error[E0382]: borrow of moved value"));
        assert!(!about_an_error("In what year was Rust 1.0 released?"));
    }
}
