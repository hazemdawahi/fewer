//! A file's outline ("anything, not only logs", 2026-11-23): its functions, classes and other
//! definitions, or a Markdown file's headings, each with the range of lines it spans, so that
//! Claude reads the range it needs and not the whole file. A heuristic, by the shape of lines:
//! a definition is a line whose first word, after words like pub, export, async or static, is
//! fn, def, class, struct, enum, trait, impl, mod, interface, type, func, function, module,
//! macro_rules!, union or record (const and static too at the top level); in C-like files also
//! a line that opens a block after a call's parentheses (a method). An item spans its line to
//! the line before the next item as deep as it or less deep, or to the end.

/// An item: its first line (from 1), its last line, how deep it is (0: the top), its line.
#[derive(Debug, PartialEq, Eq)]
pub struct Item {
    pub line: u64,
    pub end: u64,
    pub depth: usize,
    pub text: String,
}

const MODIFIERS: [&str; 22] = [
    "pub", "pub(crate)", "pub(super)", "pub(self)", "export", "default", "async", "static", "public", "private", "protected", "internal", "abstract", "final", "override", "unsafe", "extern",
    "inline", "virtual", "sealed", "open", "data",
];
const DEFS: [&str; 19] = ["fn", "def", "class", "struct", "enum", "trait", "impl", "mod", "interface", "type", "func", "function", "module", "macro_rules!", "union", "record", "object", "protocol", "extension"];
const CONTROL: [&str; 14] = ["if", "for", "while", "switch", "catch", "return", "else", "try", "do", "case", "foreach", "using", "lock", "synchronized"];

/// What kind of text a file is, by its name: Markdown (headings), C-like (methods by their
/// shape too), other code, a csv or tsv table (its columns), or JSON lines (their fields).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Markdown,
    CLike,
    Code,
    Table(u8),
    JsonLines,
}

pub fn kind_of(name: &str) -> Kind {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "mdx" => Kind::Markdown,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "cs" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "go" | "swift" | "kt" | "kts" | "scala" | "php" | "dart" => Kind::CLike,
        "csv" => Kind::Table(b','),
        "tsv" => Kind::Table(b'\t'),
        "jsonl" | "ndjson" => Kind::JsonLines,
        _ => Kind::Code,
    }
}

/// The outline of a text.
pub fn outline(text: &[u8], kind: Kind) -> Vec<Item> {
    let lines: Vec<&[u8]> = {
        let mut v: Vec<&[u8]> = text.split(|&c| c == b'\n').collect();
        if v.last().is_some_and(|l| l.is_empty()) {
            v.pop();
        }
        v
    };
    // (line index, indent or heading level, text)
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    let mut fence = false;
    for (i, raw) in lines.iter().enumerate() {
        let l = String::from_utf8_lossy(raw.strip_suffix(b"\r").unwrap_or(raw));
        if kind == Kind::Markdown {
            let t = l.trim_start();
            if t.starts_with("```") || t.starts_with("~~~") {
                fence = !fence;
                continue;
            }
            let hashes = l.chars().take_while(|&c| c == '#').count();
            if !fence && (1..=6).contains(&hashes) && l[hashes..].starts_with(' ') {
                found.push((i, hashes - 1, l.trim_end().to_string()));
            }
            continue;
        }
        let indent: usize = l.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum();
        let t = l.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') || t.starts_with('*') || t.starts_with("/*") {
            continue;
        }
        if is_definition(t, indent, kind) {
            found.push((i, indent, l.trim_end().to_string()));
        }
    }
    // depths: Markdown by heading level; code by indent, in steps of the smallest indent used
    let unit = if kind == Kind::Markdown { 1 } else { found.iter().map(|f| f.1).filter(|&n| n > 0).min().unwrap_or(4).max(1) };
    let depth_of = |n: usize| if kind == Kind::Markdown { n } else { n.div_ceil(unit) };
    let mut items: Vec<Item> = found.iter().map(|(i, n, t)| Item { line: *i as u64 + 1, end: lines.len() as u64, depth: depth_of(*n), text: t.trim_start().to_string() }).collect();
    for k in 0..items.len() {
        let d = items[k].depth;
        if let Some(next) = items[k + 1..].iter().find(|x| x.depth <= d) {
            items[k].end = (next.line - 1).max(items[k].line);
        }
    }
    items
}

/// A table's outline, a profile: its columns (from its first line), its rows, and each
/// column's values (Values::describe).
pub fn table(text: &[u8], sep: u8, values: bool) -> Vec<String> {
    let mut it = text.split(|&c| c == b'\n').filter(|l| !l.is_empty());
    let Some(head) = it.next() else { return Vec::new() };
    let head = head.strip_suffix(b"\r").unwrap_or(head);
    let cols: Vec<String> = crate::formats::split_row(head, sep).iter().map(|c| String::from_utf8_lossy(&crate::formats::unquote(c)).into_owned()).collect();
    let mut vals: Vec<Values> = (0..cols.len()).map(|_| Values::default()).collect();
    let mut rows = 0u64;
    for l in it.take(PROFILE_LINES) {
        let l = l.strip_suffix(b"\r").unwrap_or(l);
        rows += 1;
        if !values {
            continue;
        }
        let f = crate::formats::split_row(l, sep);
        for (i, v) in vals.iter_mut().enumerate() {
            v.add(&f.get(i).map(|c| String::from_utf8_lossy(&crate::formats::unquote(c)).into_owned()).unwrap_or_default());
        }
    }
    let mut out = vec![format!("a table: {} columns, {} rows after the header line", cols.len(), group(rows))];
    if values {
        for (i, (c, v)) in cols.iter().zip(&vals).enumerate() {
            out.push(format!("{} {c}: {}", i + 1, v.describe()));
        }
    } else {
        out.push(format!("columns: {}", cols.iter().enumerate().map(|(i, c)| format!("{} {c}", i + 1)).collect::<Vec<_>>().join(", ")));
    }
    out
}

/// The most lines a profile reads (a table's rows, JSON lines).
const PROFILE_LINES: usize = 1_000_000;
/// Different values a field's profile keeps count of.
const MAX_DISTINCT: usize = 100_000;
/// An object's fields whose values a JSON lines profile counts too (one level down).
const LABELS: [&str; 12] = ["name", "login", "title", "type", "status", "state", "level", "kind", "action", "event", "method", "code"];

/// The values of one field, for a profile.
#[derive(Default)]
struct Values {
    n: u64,
    empty: u64,
    counts: std::collections::HashMap<String, u64>,
    /// more than MAX_DISTINCT different values (the rest not counted one by one)
    over: bool,
}

impl Values {
    fn add(&mut self, v: &str) {
        self.n += 1;
        if v.is_empty() {
            self.empty += 1;
        } else if let Some(c) = self.counts.get_mut(v) {
            *c += 1;
        } else if self.counts.len() < MAX_DISTINCT {
            self.counts.insert(v.to_string(), 1);
        } else {
            self.over = true;
        }
    }

    /// "7 values: small_airport 15,752, heliport 8,232, closed 2,628"; "all different";
    /// "numbers -1266 to 29977"; "times from 2015-01-01T15:00:00Z to …"; "always true".
    fn describe(&self) -> String {
        let filled = self.n - self.empty;
        let distinct = self.counts.len();
        let mut s = if filled == 0 {
            "always empty".to_string()
        } else if distinct == 1 && !self.over {
            format!("always {}", cut(self.counts.keys().next().map_or("", String::as_str), 60))
        } else if let Some(r) = self.range().filter(|_| distinct > 20) {
            r
        } else if !self.over && distinct as u64 == filled {
            "all different".to_string()
        } else if self.counts.values().all(|&c| c <= 2) {
            format!("{} values, none more than twice", group(distinct as u64))
        } else {
            let mut v: Vec<(&String, &u64)> = self.counts.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let top: Vec<String> = v.iter().take(3).map(|(k, c)| format!("{} {}", cut(k, 40), group(**c))).collect();
            let more = if self.over { format!("over {}", group(MAX_DISTINCT as u64)) } else { group(distinct as u64) };
            format!("{more} values: {}", top.join(", "))
        };
        if self.empty > 0 && filled > 0 {
            s.push_str(&format!(" (empty in {})", group(self.empty)));
        }
        s
    }

    /// The range of numbers or of times (ISO-like dates), when every value is one.
    fn range(&self) -> Option<String> {
        let keys = || self.counts.keys().map(String::as_str);
        let num = |k: &str| k.parse::<f64>().ok().filter(|f| f.is_finite());
        if keys().all(|k| num(k).is_some()) {
            let lo = keys().min_by(|a, b| num(a).unwrap_or(0.0).total_cmp(&num(b).unwrap_or(0.0)))?;
            let hi = keys().max_by(|a, b| num(a).unwrap_or(0.0).total_cmp(&num(b).unwrap_or(0.0)))?;
            return Some(format!("numbers {lo} to {hi}"));
        }
        let iso = |k: &str| k.len() >= 10 && k.as_bytes()[..4].iter().all(u8::is_ascii_digit) && k.as_bytes()[4] == b'-' && k.as_bytes()[7] == b'-';
        if keys().all(iso) {
            return Some(format!("times from {} to {}", keys().min()?, keys().max()?));
        }
        None
    }
}

/// JSON lines' outline, a profile: each field in the order it first came, its kinds of value,
/// in how many lines it is (when not in all) and its values (Values::describe); an object's own
/// fields one level down, and the values of those of them that name or label things (LABELS).
pub fn json_fields(text: &[u8], values: bool) -> Vec<String> {
    use crate::json::Json;
    struct Field {
        kinds: Vec<&'static str>,
        lines: u64,
        values: Values,
        inner: Vec<String>,
        labels: Vec<(String, Values)>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashMap<String, Field> = std::collections::HashMap::new();
    let mut n = 0u64;
    for l in text.split(|&c| c == b'\n').filter(|l| !l.is_empty()).take(PROFILE_LINES) {
        let start = l.iter().position(|&c| c == b'{').unwrap_or(0);
        let end = l.iter().rposition(|&c| c == b'}').map_or(l.len(), |e| e + 1);
        let Ok(Json::Obj(fields)) = crate::json::parse(&l[start..end.max(start)]) else { continue };
        n += 1;
        for (k, v) in &fields {
            let f = seen.entry(k.clone()).or_insert_with(|| {
                order.push(k.clone());
                Field { kinds: Vec::new(), lines: 0, values: Values::default(), inner: Vec::new(), labels: Vec::new() }
            });
            f.lines += 1;
            let kind = kind_name(v);
            if !f.kinds.contains(&kind) {
                f.kinds.push(kind);
            }
            match v {
                Json::Obj(inner) => {
                    for (ik, iv) in inner {
                        if !f.inner.contains(ik) && f.inner.len() < 40 {
                            f.inner.push(ik.clone());
                        }
                        if LABELS.contains(&ik.as_str()) && matches!(iv, Json::Str(_) | Json::Num(_) | Json::Bool(_)) {
                            let at = match f.labels.iter().position(|x| &x.0 == ik) {
                                Some(i) => i,
                                None => {
                                    f.labels.push((ik.clone(), Values::default()));
                                    f.labels.len() - 1
                                }
                            };
                            f.labels[at].1.add(&scalar(iv));
                        }
                    }
                }
                Json::Arr(_) => {}
                other => f.values.add(&scalar(other)),
            }
        }
    }
    if n == 0 {
        return Vec::new();
    }
    let mut out = vec![format!("JSON lines: {} fields in {} lines", order.len(), group(n))];
    for k in &order {
        let f = &seen[k];
        let mut s = format!("{k}: {}", f.kinds.join(" or "));
        if f.lines < n {
            s.push_str(&format!(" (in {})", group(f.lines)));
        }
        if !f.inner.is_empty() {
            let shown: Vec<&str> = f.inner.iter().take(5).map(String::as_str).collect();
            s.push_str(&format!(" {{{}{}}}", shown.join(", "), if f.inner.len() > 5 { format!(", +{}", f.inner.len() - 5) } else { String::new() }));
        }
        if values && f.values.n > 0 {
            s.push_str(&format!(", {}", f.values.describe()));
        }
        out.push(s);
        if values {
            for (ik, v) in &f.labels {
                out.push(format!("  {k}.{ik}: {}", v.describe()));
            }
        }
    }
    out
}

/// A JSON scalar as text (a string without its quotes).
fn scalar(v: &crate::json::Json) -> String {
    match v {
        crate::json::Json::Str(s) => s.clone(),
        other => other.to_string(),
    }
}

fn group(n: u64) -> String {
    crate::answer::group(n)
}

/// The doc comment of the item at `line` (from 1): a Python docstring after it, or the
/// comment lines right before it (///, //, /** */, # ...), at most DOC_LINES lines;
/// attributes and decorators between them are passed over.
pub fn docs(text: &[u8], line: u64) -> Vec<String> {
    const DOC_LINES: usize = 12;
    const QUOTES: [&str; 4] = ["\"\"\"", "'''", "r\"\"\"", "u\"\"\""];
    let lines: Vec<String> = text.split(|&c| c == b'\n').map(|l| String::from_utf8_lossy(l.strip_suffix(b"\r").unwrap_or(l)).into_owned()).collect();
    let at = (line as usize).saturating_sub(1);
    // a docstring right after the line (Python)
    if let Some(i0) = (at + 1..lines.len()).find(|&i| !lines[i].trim().is_empty()) {
        let t = lines[i0].trim();
        if let Some(q) = QUOTES.into_iter().find(|q| t.starts_with(q)) {
            let close = &q[q.len() - 3..];
            let mut out = Vec::new();
            for (k, l) in lines.iter().enumerate().skip(i0).take(DOC_LINES) {
                let tl = l.trim();
                out.push(tl.to_string());
                let body = if k == i0 { &tl[q.len()..] } else { tl };
                if body.contains(close) {
                    break;
                }
            }
            return out;
        }
    }
    // comment lines right before it (Rust, C-like, shell)
    let mut out = Vec::new();
    let mut i = at;
    while i > 0 && out.len() < DOC_LINES {
        i -= 1;
        let t = lines[i].trim();
        if t.starts_with("#[") || t.starts_with('@') {
            continue;
        }
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') || t.starts_with("# ") || t == "#" {
            out.push(t.to_string());
        } else {
            break;
        }
    }
    out.reverse();
    out
}

fn kind_name(v: &crate::json::Json) -> &'static str {
    use crate::json::Json;
    match v {
        Json::Null => "null",
        Json::Bool(_) => "true/false",
        Json::Num(_) => "number",
        Json::Str(_) => "text",
        Json::Arr(_) => "list",
        Json::Obj(_) => "object",
    }
}

fn cut(s: &str, max: usize) -> String {
    let (t, n) = crate::clean::cut(s, max);
    if n > 0 { format!("{t}…") } else { t.to_string() }
}

/// Whether a line (trimmed) defines something: its first word after modifiers is one of DEFS
/// (or const or static at the top level), or, in a C-like file, it opens a block after a call's
/// parentheses and is no control statement.
fn is_definition(t: &str, indent: usize, kind: Kind) -> bool {
    let words: Vec<&str> = t.split_whitespace().collect();
    let mut k = 0;
    while k < words.len() && (MODIFIERS.contains(&words[k]) || (words[k] == "const" && words.get(k + 1) == Some(&"fn"))) {
        k += 1;
    }
    let Some(first) = words.get(k) else { return false };
    // (generics and a call's parentheses are not part of the word: impl<T>, class A(B):)
    let first = first.split(['<', '(']).next().unwrap_or("").trim_end_matches(['{', ':']);
    if DEFS.contains(&first) {
        // (a word, not a call or an assignment: "type = 3", "class=..." are no definitions)
        return words.get(k + 1).is_some_and(|w| !w.starts_with('=') && !w.starts_with('('));
    }
    if indent == 0 && (first == "const" || first == "static") && k + 1 < words.len() {
        return true;
    }
    if kind == Kind::CLike && indent <= 8 {
        let opens = t.ends_with('{') || t.ends_with(") {");
        let call = t.contains('(') && t.find('(') < t.rfind(')');
        let ctl = CONTROL.contains(&first);
        let arrow_or_assign = t.contains(" = ") && !t.contains("=>");
        return opens && call && !ctl && !arrow_or_assign && !t.ends_with(';') && !first.starts_with('.') && !first.starts_with(')') && !first.starts_with('}');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(items: &[Item]) -> Vec<(u64, u64, usize, &str)> {
        items.iter().map(|i| (i.line, i.end, i.depth, i.text.as_str())).collect()
    }

    #[test]
    fn rust_python_markdown_and_c_like() {
        let rs = "use std::io;\n\npub struct A {\n    x: u8,\n}\n\nimpl<T> A {\n    pub fn new() -> A {\n        A { x: 0 }\n    }\n\n    fn get(&self) -> u8 {\n        self.x\n    }\n}\n\nconst N: usize = 3;\n\nfn main() {\n    let a = A::new();\n}\n";
        assert_eq!(
            lines_of(&outline(rs.as_bytes(), Kind::Code)),
            [
                (3, 6, 0, "pub struct A {"),
                (7, 16, 0, "impl<T> A {"),
                (8, 11, 1, "pub fn new() -> A {"),
                (12, 16, 1, "fn get(&self) -> u8 {"),
                (17, 18, 0, "const N: usize = 3;"),
                (19, 21, 0, "fn main() {"),
            ]
        );
        let py = "import os\n\nclass Cart:\n    def total(self):\n        return 1\n\n    async def pay(self):\n        pass\n\ndef main():\n    # def not_this():\n    type = 3\n";
        assert_eq!(lines_of(&outline(py.as_bytes(), Kind::Code)), [(3, 9, 0, "class Cart:"), (4, 6, 1, "def total(self):"), (7, 9, 1, "async def pay(self):"), (10, 12, 0, "def main():")]);
        let md = "# Plan\n\ntext\n\n## Stage 1\n\n```\n# not a heading\n```\n\n## Stage 2\nmore\n";
        assert_eq!(lines_of(&outline(md.as_bytes(), Kind::Markdown)), [(1, 12, 0, "# Plan"), (5, 10, 1, "## Stage 1"), (11, 12, 1, "## Stage 2")]);
        let ts = "export class Login {\n  submit(email: string): void {\n    if (email) {\n      send(email);\n    }\n  }\n}\nexport function render(p: Props) {\n  return 1;\n}\n";
        assert_eq!(lines_of(&outline(ts.as_bytes(), Kind::CLike)), [(1, 7, 0, "export class Login {"), (2, 7, 1, "submit(email: string): void {"), (8, 10, 0, "export function render(p: Props) {")]);
        assert_eq!(kind_of("docs/PLAN.md"), Kind::Markdown);
        assert_eq!(kind_of("src/a.tsx"), Kind::CLike);
        assert_eq!(kind_of("src/a.rs"), Kind::Code);
        assert_eq!(kind_of("data/t.csv"), Kind::Table(b','));
        assert_eq!(kind_of("e.jsonl"), Kind::JsonLines);
    }

    #[test]
    fn tables_and_json_lines() {
        let csv = "ident,type,\"name, long\",elevation_ft\n00A,heliport,\"Total, RF\",11\n00AA,small_airport,Aero,3435\n00AB,heliport,,-5\n";
        assert_eq!(
            table(csv.as_bytes(), b',', true),
            ["a table: 4 columns, 3 rows after the header line", "1 ident: all different", "2 type: 2 values, none more than twice", "3 name, long: all different (empty in 1)", "4 elevation_ft: all different"]
        );
        assert_eq!(table(csv.as_bytes(), b',', false), ["a table: 4 columns, 3 rows after the header line", "columns: 1 ident, 2 type, 3 name, long, 4 elevation_ft"]);
        let jl = "{\"id\":\"1\",\"type\":\"PushEvent\",\"repo\":{\"id\":7,\"name\":\"a/b\"},\"public\":true,\"at\":\"2015-01-01T15:00:00Z\"}\n{\"id\":\"2\",\"type\":\"WatchEvent\",\"repo\":{\"id\":8,\"name\":\"a/b\",\"url\":\"u\"},\"public\":true,\"org\":null,\"at\":\"2015-01-01T15:00:09Z\"}\n";
        assert_eq!(
            json_fields(jl.as_bytes(), true),
            [
                "JSON lines: 6 fields in 2 lines",
                "id: text, all different",
                "type: text, all different",
                "repo: object {id, name, url}",
                "  repo.name: always a/b",
                "public: true/false, always true",
                "at: text, all different",
                "org: null (in 1), always null",
            ]
        );
        // many different numbers or times: their range
        let mut nums = String::from("n,t\n");
        for i in 0..30 {
            nums.push_str(&format!("{},2015-01-01T15:00:{:02}Z\n", i * 7 - 3, i));
        }
        assert_eq!(table(nums.as_bytes(), b',', true), ["a table: 2 columns, 30 rows after the header line", "1 n: numbers -3 to 200", "2 t: times from 2015-01-01T15:00:00Z to 2015-01-01T15:00:29Z"]);
        assert_eq!(json_fields(jl.as_bytes(), false)[3], "repo: object {id, name, url}");
    }

    #[test]
    fn doc_comments() {
        let py = "def f(x):\n    \"\"\"\n    Return x.\n    \"\"\"\n    return x\n";
        assert_eq!(docs(py.as_bytes(), 1), ["\"\"\"", "Return x.", "\"\"\""]);
        let one = "def g():\n    \"\"\"Return 1.\"\"\"\n    return 1\n";
        assert_eq!(docs(one.as_bytes(), 1), ["\"\"\"Return 1.\"\"\""]);
        let rs = "/// Adds one.\n/// Never fails.\n#[inline]\npub fn f() {}\n";
        assert_eq!(docs(rs.as_bytes(), 4), ["/// Adds one.", "/// Never fails."]);
        assert!(docs(b"x = 1\ndef g():\n    pass\n", 2).is_empty());
    }
}
