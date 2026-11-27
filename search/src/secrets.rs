//! Secret shapes: a pure scanner that replaces each secret value with a
//! marker, `[hidden: <shape>]`, and counts what it hid.
//!
//! Secrets are hidden before anything is written: `run` captures, agents' sessions and OS
//! exports; `add` only with --hide-secrets. The terminal shows the original; only what is
//! saved changes. These are our own matchers (no regex engine). The published list is
//! SECRETS.md; `secret = PREFIX CLASS MIN` lines in the config add shapes ([`Extra`]).
//!
//! The shapes ([`SHAPES`]):
//! - tokens with a known prefix, at the start of a word (the byte before is not an ASCII
//!   letter or digit): AWS key (AKIA or ASIA and exactly 16 of A-Z 0-9), GitHub (ghp_ gho_
//!   ghu_ ghs_ ghr_ and 36 or more letters and digits; github_pat_ and 22 or more letters,
//!   digits and '_'), GitLab (glpat- and 20 or more letters, digits, '_' and '-'), Slack
//!   (xoxa- xoxb- xoxp- xoxo- xoxs- xoxr- and 10 or more letters, digits and '-'), Stripe
//!   (sk_live_ sk_test_ rk_live_ rk_test_ and 16 or more letters and digits), Anthropic
//!   (sk-ant-) and OpenAI (sk-proj-, sk-) with 20 or more letters, digits, '_' and '-',
//!   Google API key (AIza and exactly 35 letters, digits, '_' and '-'), JWT (three base64url
//!   parts joined by '.', the first two starting with eyJ). The whole token is replaced;
//! - named values: a key whose name ends with password, passwd, pwd, passphrase, secret,
//!   client_secret, secret_key, token, api_key, apikey, access_key or private_key (any case;
//!   '_', '-' and '.' and digits at the end do not count: DB_PASSWORD, X-Api-Key, clientSecret,
//!   token2), then '=', ':', '=>' or ':=' (quotes and spaces allowed around): the value. A
//!   quoted value runs to its closing quote ("..." and '...' and JSON's escaped \"...\"); an
//!   unquoted one up to a space, a quote, '&', ';', ',', a backslash or a control byte. A
//!   value already hidden stays. In JSON (the key in double quotes, also as \" inside a JSON
//!   string), values that are not strings stay when they are null, true, false, an object or
//!   an array, and a number is replaced by the marker in quotes: the line stays valid JSON. A
//!   key named aws_secret_access_key with a value of exactly 40 of A-Z a-z 0-9 / + = gives
//!   "AWS secret"; a value that is one of the tokens above is named after that token;
//! - `Authorization: Bearer <value>` and `Authorization: Basic <value>` (any case): the value;
//! - `scheme://user:PASSWORD@host`: the password (also JSON's `scheme:\/\/...`);
//! - private keys, from `-----BEGIN ... PRIVATE KEY-----` to the matching `-----END ...-----`:
//!   - the matching END on the same line (a key in a JSON string, its line ends written \n):
//!     all of it becomes one marker; with no END, the rest of the line, or of its JSON string,
//!     does (a key cut short is still hidden);
//!   - the BEGIN marker alone on its line (or alone in its JSON string, as Docker writes one
//!     line per JSON object): the BEGIN marker becomes the marker, and the next lines are key
//!     lines up to the matching END line (or 1,000 lines): a line made only of base64 goes
//!     whole, and other lines (a log prefix, Docker's JSON around the text) lose every run of
//!     16 or more base64 bytes; the END line stays.
//!
//! [`Hider::line`] takes one line at a time and carries a private key from line to line.
//! [`Stream`] takes bytes as they come (a pty's reads) and keeps a part line until its '\n',
//! so a key split across two reads is still hidden.

/// The text a marker starts with: a marker is `[hidden: <shape>]`.
pub const MARKER: &[u8] = b"[hidden: ";

/// A part line longer than this, with no '\n' yet, is scanned as it is ([`Stream`]); a secret
/// across that cut may be missed.
pub const MAX_PART: usize = 1 << 20;

/// A private key block ends at its END line, or after this many lines.
pub const MAX_KEY_LINES: u32 = 1000;

/// The published shapes (SECRETS.md), as `memory secrets` lists them: the name in the marker,
/// and what is matched.
pub const SHAPES: &[(&str, &str)] = &[
    ("AWS key", "AKIA or ASIA, then exactly 16 of A-Z and 0-9"),
    ("AWS secret", "aws_secret_access_key (any case), '=' or ':', then exactly 40 of A-Z, a-z, 0-9, '/', '+' and '='"),
    ("GitHub token", "ghp_, gho_, ghu_, ghs_ or ghr_ and 36 or more letters and digits; github_pat_ and 22 or more letters, digits and '_'"),
    ("GitLab token", "glpat- and 20 or more letters, digits, '_' and '-'"),
    ("Slack token", "xoxa-, xoxb-, xoxp-, xoxo-, xoxs- or xoxr- and 10 or more letters, digits and '-'"),
    ("Stripe key", "sk_live_, sk_test_, rk_live_ or rk_test_ and 16 or more letters and digits"),
    ("Anthropic key", "sk-ant- and 20 or more letters, digits, '_' and '-'"),
    ("OpenAI key", "sk-proj- or sk- and 20 or more letters, digits, '_' and '-'"),
    ("Google API key", "AIza and exactly 35 letters, digits, '_' and '-'"),
    ("JWT", "three base64url parts joined by '.', the first two starting with eyJ"),
    ("private key", "from -----BEGIN ... PRIVATE KEY----- to the matching -----END ...-----, across lines"),
    ("authorization", "the value after Authorization: Bearer or Authorization: Basic"),
    (
        "password, token, ...",
        "the value after a key whose name ends with password, passwd, pwd, passphrase, secret, client_secret, secret_key, token, api_key, apikey, access_key or private_key, then '=', ':', '=>' or ':='; the marker names the key",
    ),
    ("URL password", "the password in scheme://user:password@host"),
    ("<prefix>", "extra shapes from the config: secret = PREFIX CLASS MIN (CLASS: alnum, base64 or hex)"),
];

/// The key names of named values, as compared (lowercase, without '_', '-' and '.'), and the
/// name the marker shows. The longest first: client_secret before secret.
const NAMES: [(&[u8], &str); 11] = [
    (b"clientsecret", "client_secret"),
    (b"passphrase", "passphrase"),
    (b"privatekey", "private_key"),
    (b"accesskey", "access_key"),
    (b"secretkey", "secret_key"),
    (b"password", "password"),
    (b"apikey", "api_key"),
    (b"passwd", "passwd"),
    (b"secret", "secret"),
    (b"token", "token"),
    (b"pwd", "pwd"),
];

// ---------------------------------------------------------------------------------------
// Extra shapes

/// The bytes an extra shape's value is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A-Z, a-z, 0-9
    Alnum,
    /// A-Z, a-z, 0-9, '+', '/', '=', '-', '_' (both base64 alphabets)
    Base64,
    /// 0-9, a-f, A-F
    Hex,
}

impl Class {
    fn has(self, b: u8) -> bool {
        match self {
            Class::Alnum => b.is_ascii_alphanumeric(),
            Class::Base64 => b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'),
            Class::Hex => b.is_ascii_hexdigit(),
        }
    }
}

/// An extra shape from the config, `secret = PREFIX CLASS MIN`: PREFIX at the start of a word,
/// then MIN or more bytes of CLASS. The whole token is replaced by `[hidden: PREFIX]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extra {
    pub prefix: String,
    pub class: Class,
    pub min: usize,
}

impl Extra {
    /// Reads "PREFIX CLASS MIN" (the config value after `secret =`). PREFIX: 1 to 64 printable
    /// ASCII bytes without spaces or brackets; CLASS: alnum, base64 or hex; MIN: 1 to 4096.
    pub fn parse(spec: &str) -> Result<Extra, String> {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        let [prefix, class, min] = parts.as_slice() else {
            return Err(format!("expected PREFIX CLASS MIN, found {spec:?}"));
        };
        if prefix.len() > 64 || !prefix.bytes().all(|b| b.is_ascii_graphic() && b != b'[' && b != b']') {
            return Err(format!("the prefix {prefix:?} must be 1 to 64 printable ASCII bytes without brackets"));
        }
        let class = match *class {
            "alnum" => Class::Alnum,
            "base64" => Class::Base64,
            "hex" => Class::Hex,
            other => return Err(format!("unknown class {other:?}: the classes are alnum, base64 and hex")),
        };
        let min = min.parse::<usize>().ok().filter(|m| (1..=4096).contains(m)).ok_or_else(|| format!("MIN must be a number from 1 to 4096, found {min:?}"))?;
        Ok(Extra { prefix: prefix.to_string(), class, min })
    }
}

// ---------------------------------------------------------------------------------------
// Counts

/// How many values were hidden, per shape name, in order of the first one hidden.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    by: Vec<(String, u64)>,
}

impl Counts {
    fn add(&mut self, name: &str) {
        match self.by.iter_mut().find(|(n, _)| n == name) {
            Some((_, c)) => *c += 1,
            None => self.by.push((name.to_string(), 1)),
        }
    }

    /// `n` more values hidden under one shape name.
    pub fn add_n(&mut self, name: &str, n: u64) {
        if n > 0 {
            match self.by.iter_mut().find(|(x, _)| x == name) {
                Some((_, c)) => *c += n,
                None => self.by.push((name.to_string(), n)),
            }
        }
    }

    /// Every value hidden: the run's `hidden` count.
    pub fn total(&self) -> u64 {
        self.by.iter().map(|(_, c)| c).sum()
    }

    /// The values hidden under one shape name (as in the marker).
    pub fn get(&self, name: &str) -> u64 {
        self.by.iter().find(|(n, _)| n == name).map_or(0, |(_, c)| *c)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.by.iter().map(|(n, c)| (n.as_str(), *c))
    }
}

// ---------------------------------------------------------------------------------------
// The scanner

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Name {
    Fixed(&'static str),
    Extra(usize),
}

/// A value to hide: `line[start..end]`. `wrap` goes on both sides of the marker: quotes,
/// when the marker takes the place of a JSON number.
struct Hit {
    start: usize,
    end: usize,
    name: Name,
    wrap: &'static [u8],
}

impl Hit {
    fn new(start: usize, end: usize, name: Name) -> Hit {
        Hit { start, end, name, wrap: b"" }
    }
}

/// A private key carried from one line to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Block {
    None,
    /// A key inside a line that was cut before its end (a part of a very long line): hidden up
    /// to its END marker, its JSON string's end, or the line's end.
    InLine { end: Vec<u8> },
    /// Key lines after a BEGIN line, up to the END line: `end` is "-----END <label>-----";
    /// `long`: the line before held a long run of key material (a short line after it is the key's
    /// tail).
    Lines { end: Vec<u8>, lines: u32, long: bool },
}

/// Hides secrets one line at a time (see the module's doc).
#[derive(Clone, Debug)]
pub struct Hider {
    extra: Vec<Extra>,
    /// the first bytes of the token prefixes, built in and extra
    first: [bool; 256],
    block: Block,
    counts: Counts,
}

impl Default for Hider {
    fn default() -> Hider {
        Hider::new(Vec::new())
    }
}

impl Hider {
    pub fn new(extra: Vec<Extra>) -> Hider {
        let mut first = [false; 256];
        for b in *b"Agxsre" {
            first[b as usize] = true;
        }
        for x in &extra {
            first[x.prefix.as_bytes()[0] as usize] = true;
        }
        Hider { extra, first, block: Block::None, counts: Counts::default() }
    }

    /// What was hidden so far.
    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    /// Hides the secrets of one line: its bytes up to and including '\n' (the last line may
    /// have none). Appends the line, with each secret value replaced by its marker, to `out`.
    pub fn line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let has_end = line.ends_with(b"\n");
        let content = content_end(line);
        let mut from = 0;
        match std::mem::replace(&mut self.block, Block::None) {
            Block::None => {}
            Block::InLine { end } => {
                // the rest of a key whose marker is already written
                let (stop, closed) = in_line_stop(line, 0, content, &end);
                if !closed && !has_end {
                    self.block = Block::InLine { end };
                }
                from = stop;
            }
            Block::Lines { end, lines, long } => {
                if let Some(q) = find(&line[..content], &end) {
                    scrub(&line[..q], out);
                    from = q;
                } else if begin_marker_in(&line[..content]) {
                    // a new key starts: the one before had no END line
                } else if let Some(longest) = key_line(&line[..content], long) {
                    // (a key's line: one of base64 alone, under its indentation, quotes and commas
                    // too, whatever its letters, a key's last line may be a few letters; other lines lose their runs of key material)
                    if !is_base64(quoted_text(&line[..content])) {
                        scrub(line, out);
                    }
                    if lines + 1 < MAX_KEY_LINES {
                        self.block = Block::Lines { end, lines: lines + 1, long: longest >= 40 };
                    }
                    return;
                }
                // (else a line that is no key's: the BEGIN marker started none, as in code that
                // names it, "if (s.startsWith(header)) {"; the line read as any other)
            }
        }
        self.scan(line, from, content, has_end, out);
    }

    /// Hides the secrets of a whole text, line by line.
    pub fn text(&mut self, text: &[u8], out: &mut Vec<u8>) {
        for l in text.split_inclusive(|&b| b == b'\n') {
            self.line(l, out);
        }
    }

    /// One pass over `line[from..content]`, copying it to `out` with each hit replaced.
    fn scan(&mut self, line: &[u8], from: usize, content: usize, has_end: bool, out: &mut Vec<u8>) {
        let mut last = from;
        let mut i = from;
        while i < content {
            let b = line[i];
            let hit = match b {
                b'=' | b':' => self.separator_hit(line, i, content),
                b'-' if line[i..content].starts_with(b"-----BEGIN ") => self.begin_hit(line, i, content, has_end),
                _ if self.first[b as usize] && (i == 0 || !line[i - 1].is_ascii_alphanumeric()) => {
                    self.token_at(line, i, content).map(|(name, end)| Hit::new(i, end, name))
                }
                _ => None,
            };
            match hit {
                Some(h) => {
                    out.extend_from_slice(&line[last..h.start]);
                    out.extend_from_slice(h.wrap);
                    self.marker(h.name, out);
                    out.extend_from_slice(h.wrap);
                    last = h.end;
                    i = h.end.max(i + 1);
                }
                None => i += 1,
            }
        }
        out.extend_from_slice(&line[last..]);
    }

    /// Writes `[hidden: <name>]` and counts it.
    fn marker(&mut self, name: Name, out: &mut Vec<u8>) {
        let text: &str = match name {
            Name::Fixed(s) => s,
            Name::Extra(k) => &self.extra[k].prefix,
        };
        out.extend_from_slice(MARKER);
        out.extend_from_slice(text.as_bytes());
        out.push(b']');
        self.counts.add(text);
    }

    /// A token with a known prefix (or an extra shape's) at `line[i]`: its name and end.
    fn token_at(&self, line: &[u8], i: usize, end: usize) -> Option<(Name, usize)> {
        let t = &line[i..end];
        let run = |from: usize, class: fn(u8) -> bool| t.get(from..).map_or(0, |r| r.iter().take_while(|&&b| class(b)).count());
        let alnum: fn(u8) -> bool = |b| b.is_ascii_alphanumeric();
        let upper_digit: fn(u8) -> bool = |b| b.is_ascii_uppercase() || b.is_ascii_digit();
        let url64: fn(u8) -> bool = |b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        let word: fn(u8) -> bool = |b| b.is_ascii_alphanumeric() || b == b'_';
        let dashed: fn(u8) -> bool = |b| b.is_ascii_alphanumeric() || b == b'-';
        let at_least = |p: usize, class: fn(u8) -> bool, min: usize, name: &'static str| -> Option<(Name, usize)> {
            let n = run(p, class);
            (n >= min).then_some((Name::Fixed(name), i + p + n))
        };
        let found = match *t.first()? {
            b'A' if t.starts_with(b"AKIA") || t.starts_with(b"ASIA") => {
                (run(4, upper_digit) == 16 && !t.get(20).is_some_and(u8::is_ascii_alphanumeric)).then_some((Name::Fixed("AWS key"), i + 20))
            }
            b'A' if t.starts_with(b"AIza") => (run(4, url64) == 35).then_some((Name::Fixed("Google API key"), i + 39)),
            b'g' if t.len() > 4 && t.starts_with(b"gh") && matches!(t[2], b'p' | b'o' | b'u' | b's' | b'r') && t[3] == b'_' => at_least(4, alnum, 36, "GitHub token"),
            b'g' if t.starts_with(b"github_pat_") => at_least(11, word, 22, "GitHub token"),
            b'g' if t.starts_with(b"glpat-") => at_least(6, url64, 20, "GitLab token"),
            b'x' if t.len() > 5 && t.starts_with(b"xox") && matches!(t[3], b'a' | b'b' | b'p' | b'o' | b's' | b'r') && t[4] == b'-' => at_least(5, dashed, 10, "Slack token"),
            b's' | b'r' if [&b"sk_live_"[..], b"sk_test_", b"rk_live_", b"rk_test_"].iter().any(|p| t.starts_with(p)) => at_least(8, alnum, 16, "Stripe key"),
            b's' if t.starts_with(b"sk-ant-") => at_least(7, url64, 20, "Anthropic key"),
            b's' if t.starts_with(b"sk-proj-") => at_least(8, url64, 20, "OpenAI key"),
            b's' if t.starts_with(b"sk-") => at_least(3, url64, 20, "OpenAI key"),
            b'e' if t.starts_with(b"eyJ") => jwt_end(t).map(|e| (Name::Fixed("JWT"), i + e)),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
        for (k, x) in self.extra.iter().enumerate() {
            if t.starts_with(x.prefix.as_bytes()) {
                let p = x.prefix.len();
                let n = t[p..].iter().take_while(|&&b| x.class.has(b)).count();
                if n >= x.min {
                    return Some((Name::Extra(k), i + p + n));
                }
            }
        }
        None
    }

    /// At a '=' or ':': a named value, an Authorization header, or a URL's password.
    fn separator_hit(&self, line: &[u8], i: usize, end: usize) -> Option<Hit> {
        let at = |k: usize| if k < end { line[k] } else { 0 };
        let prev = if i > 0 { line[i - 1] } else { 0 };
        if line[i] == b':' {
            let rest = &line[i + 1..end];
            if rest.starts_with(b"//") || rest.starts_with(b"\\/\\/") {
                return self.url_hit(line, i, end);
            }
            if at(i + 1) == b':' || prev == b':' {
                return None; // a::b
            }
        } else if at(i + 1) == b'=' || matches!(prev, b'=' | b'!' | b'<' | b'>') {
            return None; // ==, !=, <=, >=
        }
        let sep = if (line[i] == b'=' && at(i + 1) == b'>') || (line[i] == b':' && at(i + 1) == b'=') { 2 } else { 1 };
        let (key, quote) = key_before(line, i)?;
        let key = normalize(key);
        if key.ends_with(b"authorization") {
            return auth_hit(line, i + sep, end);
        }
        let &(_, name) = NAMES.iter().find(|(n, _)| key.ends_with(n))?;
        let (a, mut z, quoted) = value_span(line, i + sep, end)?;
        let mut wrap: &'static [u8] = b"";
        if !quoted && (quote == b"\"" || quote == b"\\\"") {
            // JSON (the key in double quotes): null, true, false, objects and arrays are not
            // secrets; a number is replaced by the marker in quotes, so the JSON stays valid
            let v = &line[a..z];
            if v.first().is_some_and(|&b| b == b'{' || b == b'[') {
                return None;
            }
            if let Some((n, literal)) = json_scalar(v) {
                if literal {
                    return None;
                }
                z = a + n;
                wrap = quote;
            }
        }
        let v = &line[a..z];
        if v.is_empty() || line[a..end].starts_with(MARKER) {
            return None;
        }
        let name = if key.ends_with(b"awssecretaccesskey") && v.len() == 40 && v.iter().all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'+' | b'=')) {
            Name::Fixed("AWS secret")
        } else {
            self.token_at(line, a, z).map_or(Name::Fixed(name), |(n, _)| n)
        };
        Some(Hit { start: a, end: z, name, wrap })
    }

    /// `scheme://user:PASSWORD@host` with the ':' after the scheme at `line[i]`.
    fn url_hit(&self, line: &[u8], i: usize, end: usize) -> Option<Hit> {
        let mut s = i;
        while s > 0 && (line[s - 1].is_ascii_alphanumeric() || matches!(line[s - 1], b'+' | b'-' | b'.')) {
            s -= 1;
        }
        if s == i || !line[s].is_ascii_alphabetic() {
            return None;
        }
        let start = if line[i + 1..end].starts_with(b"//") { i + 3 } else { i + 5 };
        let mut j = start;
        while j < end && line[j] > b' ' && line[j] != 0x7f && !matches!(line[j], b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' | b'`' | b'\\') {
            j += 1;
        }
        let authority = &line[start..j];
        let at = authority.iter().rposition(|&b| b == b'@')?;
        let colon = authority[..at].iter().position(|&b| b == b':')?;
        let (a, z) = (start + colon + 1, start + at);
        if a >= z || line[a..end].starts_with(MARKER) {
            return None;
        }
        let name = self.token_at(line, a, z).map_or(Name::Fixed("URL password"), |(n, _)| n);
        Some(Hit::new(a, z, name))
    }

    /// A private key's BEGIN marker at `line[i]` (see the module's doc).
    fn begin_hit(&mut self, line: &[u8], i: usize, content: usize, has_end: bool) -> Option<Hit> {
        let (len, end_marker) = begin_marker(&line[i..content])?;
        let me = i + len;
        let name = Name::Fixed("private key");
        // (its END on the same line, after strings and commas too, a key's lines as one line of
        // JSON: all of it one marker)
        if let Some(q) = find(&line[me..content], &end_marker) {
            return Some(Hit::new(i, me + q + end_marker.len(), name));
        }
        if rest_is_empty(line, me, content) {
            self.block = Block::Lines { end: end_marker, lines: 0, long: false };
            return Some(Hit::new(i, me, name));
        }
        let (stop, closed) = in_line_stop(line, me, content, &end_marker);
        if !closed && !has_end {
            self.block = Block::InLine { end: end_marker };
        }
        Some(Hit::new(i, stop, name))
    }
}

/// The end of a line's text: before its '\n' and a '\r' before that.
fn content_end(l: &[u8]) -> usize {
    let mut e = l.len();
    if e > 0 && l[e - 1] == b'\n' {
        e -= 1;
        if e > 0 && l[e - 1] == b'\r' {
            e -= 1;
        }
    }
    e
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w == n)
}

/// The key name before a separator at `line[s]`: spaces, then a closing quote (" ' or \"),
/// maybe inside brackets (`env["TOKEN"] = ...`), then a run of letters, digits, '_', '-' and
/// '.' (its last 128 bytes). Also the closing quote as written ("" when none).
fn key_before(line: &[u8], s: usize) -> Option<(&[u8], &'static [u8])> {
    let mut j = s;
    while j > 0 && (line[j - 1] == b' ' || line[j - 1] == b'\t') {
        j -= 1;
    }
    if j > 1 && line[j - 1] == b']' && (line[j - 2] == b'"' || line[j - 2] == b'\'') {
        j -= 1;
    }
    let mut quote: &'static [u8] = b"";
    if j > 0 && (line[j - 1] == b'"' || line[j - 1] == b'\'') {
        j -= 1;
        quote = if line[j] == b'"' { b"\"" } else { b"'" };
        if line[j] == b'"' && j > 0 && line[j - 1] == b'\\' {
            j -= 1;
            quote = b"\\\"";
        }
    }
    let e = j;
    let low = e.saturating_sub(128);
    while j > low && (line[j - 1].is_ascii_alphanumeric() || matches!(line[j - 1], b'_' | b'-' | b'.')) {
        j -= 1;
    }
    (j < e).then(|| (&line[j..e], quote))
}

/// A JSON value other than a string at the start of an unquoted value, filling it or followed
/// by '}' or ']': null, true or false (a literal), or a number. Its length, and whether it is a
/// literal.
fn json_scalar(v: &[u8]) -> Option<(usize, bool)> {
    let digits = |from: usize| v.get(from..).map_or(0, |r| r.iter().take_while(|b| b.is_ascii_digit()).count());
    let (n, literal) = if v.starts_with(b"null") || v.starts_with(b"true") {
        (4, true)
    } else if v.starts_with(b"false") {
        (5, true)
    } else {
        let mut i = usize::from(v.first() == Some(&b'-'));
        let d = digits(i);
        if d == 0 {
            return None;
        }
        i += d;
        if v.get(i) == Some(&b'.') {
            let f = digits(i + 1);
            if f == 0 {
                return None;
            }
            i += 1 + f;
        }
        if matches!(v.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(v.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            let e = digits(j);
            if e == 0 {
                return None;
            }
            i = j + e;
        }
        (i, false)
    };
    (n == v.len() || matches!(v[n], b'}' | b']')).then_some((n, literal))
}

/// A key name as compared: lowercase, without '_', '-' and '.', and without digits at its end.
fn normalize(key: &[u8]) -> Vec<u8> {
    let mut n: Vec<u8> = key.iter().filter(|&&b| !matches!(b, b'_' | b'-' | b'.')).map(u8::to_ascii_lowercase).collect();
    while n.last().is_some_and(u8::is_ascii_digit) {
        n.pop();
    }
    n
}

/// The bytes that end an unquoted value.
fn stops(b: u8) -> bool {
    b <= b' ' || b == 0x7f || matches!(b, b'"' | b'\'' | b'&' | b';' | b',' | b'\\')
}

fn unquoted_end(line: &[u8], k: usize, end: usize) -> usize {
    let mut j = k;
    while j < end && !stops(line[j]) {
        j += 1;
    }
    j
}

/// A value after a separator ending at `k`: (start, end, quoted), the quotes not included.
fn value_span(line: &[u8], mut k: usize, end: usize) -> Option<(usize, usize, bool)> {
    while k < end && (line[k] == b' ' || line[k] == b'\t') {
        k += 1;
    }
    if k >= end {
        return None;
    }
    match line[k] {
        b'"' => {
            // to the closing quote (backslash escapes), else to the end of the line
            let mut j = k + 1;
            while j < end {
                match line[j] {
                    b'\\' => j += 2,
                    b'"' => return Some((k + 1, j, true)),
                    _ => j += 1,
                }
            }
            Some((k + 1, end, true))
        }
        b'\'' => match line[k + 1..end].iter().position(|&b| b == b'\'') {
            Some(p) => Some((k + 1, k + 1 + p, true)),
            None => Some((k + 1, unquoted_end(line, k + 1, end), true)),
        },
        b'\\' if k + 1 < end && line[k + 1] == b'"' => {
            // JSON's \"...\" inside a JSON string: to the next \" or to the string's end
            let mut j = k + 2;
            while j < end {
                if line[j] == b'"' || (line[j] == b'\\' && j + 1 < end && line[j + 1] == b'"') {
                    return Some((k + 2, j, true));
                }
                j += if line[j] == b'\\' { 2 } else { 1 };
            }
            Some((k + 2, end, true))
        }
        _ => Some((k, unquoted_end(line, k, end), false)),
    }
}

/// The value of `Authorization: Bearer <value>` or `Basic <value>`, the separator ending at `k`.
fn auth_hit(line: &[u8], mut k: usize, end: usize) -> Option<Hit> {
    while k < end && (line[k] == b' ' || line[k] == b'\t') {
        k += 1;
    }
    if k < end && (line[k] == b'"' || line[k] == b'\'') {
        k += 1;
    } else if line[k..end].starts_with(b"\\\"") {
        k += 2;
    }
    let rest = &line[k..end];
    let n = if rest.len() > 6 && rest[..6].eq_ignore_ascii_case(b"bearer") {
        6
    } else if rest.len() > 5 && rest[..5].eq_ignore_ascii_case(b"basic") {
        5
    } else {
        return None;
    };
    let mut a = k + n;
    if line[a] != b' ' {
        return None;
    }
    while a < end && line[a] == b' ' {
        a += 1;
    }
    let z = unquoted_end(line, a, end);
    if z == a || line[a..end].starts_with(MARKER) {
        return None;
    }
    Some(Hit::new(a, z, Name::Fixed("authorization")))
}

/// A JWT at the start of `t`: eyJ..., '.', eyJ..., '.', and a third part (maybe empty), of
/// base64url bytes. Its end.
fn jwt_end(t: &[u8]) -> Option<usize> {
    let url64 = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-';
    let part = |from: usize| t.get(from..).map_or(0, |r| r.iter().take_while(|b| url64(b)).count());
    let p1 = part(0);
    if p1 < 4 || t.get(p1) != Some(&b'.') || !t[p1 + 1..].starts_with(b"eyJ") {
        return None;
    }
    let p2 = part(p1 + 1);
    let e2 = p1 + 1 + p2;
    if p2 < 4 || t.get(e2) != Some(&b'.') {
        return None;
    }
    Some(e2 + 1 + part(e2 + 1))
}

/// "-----BEGIN <label>-----" at the start of `t`, the label made of A-Z, 0-9 and spaces (at
/// most 64 bytes) and ending with "PRIVATE KEY" or "PRIVATE KEY BLOCK": the marker's length
/// and the matching "-----END <label>-----".
fn begin_marker(t: &[u8]) -> Option<(usize, Vec<u8>)> {
    let rest = t.strip_prefix(b"-----BEGIN ")?;
    let n = rest.iter().take(65).position(|&b| b == b'-')?;
    let label = &rest[..n];
    if !rest[n..].starts_with(b"-----") || !label.iter().all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b' ') {
        return None;
    }
    if !(label.ends_with(b"PRIVATE KEY") || label.ends_with(b"PRIVATE KEY BLOCK")) {
        return None;
    }
    let mut end = b"-----END ".to_vec();
    end.extend_from_slice(label);
    end.extend_from_slice(b"-----");
    Some((11 + n + 5, end))
}

/// A private key's BEGIN marker anywhere in `t`.
fn begin_marker_in(t: &[u8]) -> bool {
    let mut from = 0;
    while let Some(p) = find(&t[from..], b"-----BEGIN ") {
        if begin_marker(&t[from + p..]).is_some() {
            return true;
        }
        from += p + 1;
    }
    false
}

/// After a BEGIN marker: only spaces, tabs, '\r' and escaped line ends (\r, \n written in a
/// JSON string) up to the line's end or the end of its JSON string.
fn rest_is_empty(line: &[u8], mut j: usize, end: usize) -> bool {
    let mut escaped_end = false;
    loop {
        while j < end && matches!(line[j], b' ' | b'\t' | b'\r') {
            j += 1;
        }
        if line[j..end].starts_with(b"\\n") || line[j..end].starts_with(b"\\r") {
            j += 2;
            escaped_end = true;
        } else {
            break;
        }
    }
    // (a quote's end, a string that holds the marker alone, starts the key's lines: in code that
    // writes a key a line at a time, "-----BEGIN ...-----", then its lines; in code that only names
    // the marker the block ends at its first line that is no key's)
    let _ = escaped_end;
    j >= end || matches!(line[j], b'"' | b'\'')
}

/// A line's text without what holds it in code or in a list: spaces, a list's "- ", quotes,
/// commas, brackets, and escaped line ends at its end ("\\n").
fn quoted_text(t: &[u8]) -> &[u8] {
    let mut t = t.trim_ascii();
    t = t.strip_prefix(b"- ").unwrap_or(t);
    let edge = |b: u8| matches!(b, b'"' | b'\'' | b',' | b';' | b'[' | b']' | b'(' | b')' | b'{' | b'}' | b'`') || b.is_ascii_whitespace();
    loop {
        let before = t.len();
        while let Some((&f, rest)) = t.split_first() {
            if edge(f) { t = rest } else { break }
        }
        while let Some((&l, rest)) = t.split_last() {
            if edge(l) { t = rest } else { break }
        }
        for e in [&b"\\n"[..], b"\\r"] {
            if let Some(rest) = t.strip_suffix(e) {
                t = rest;
            }
        }
        if t.len() == before {
            return t;
        }
    }
}

/// Whether a line inside a key's block is the key's: blank, a header ("Proc-Type: 4,ENCRYPTED",
/// "Version: ..."), base64 alone once its quotes and commas are taken off, one holding a run of
/// key material of 16 bytes or more or a whole string of base64 (a key's line in a log's prefix, in
/// Docker's JSON, after an echo), or, after a line with a long run, one whose last word is base64
/// (a key's tail in a log). Its longest run of key material, or None when it is no key's line.
fn key_line(t: &[u8], after_long: bool) -> Option<usize> {
    let text = quoted_text(t);
    if t.trim_ascii().is_empty() {
        return Some(0);
    }
    let header = t.iter().position(|&b| b == b':').is_some_and(|c| c > 0 && t[c + 1..].starts_with(b" ") && t[..c].trim_ascii_start().iter().all(|&b| b.is_ascii_alphanumeric() || b == b'-'));
    if header {
        return Some(0);
    }
    if is_base64(text) {
        return Some(text.len());
    }
    let mut longest = 0;
    let mut whole_string = false;
    let mut i = 0;
    while i < t.len() {
        if !base64_byte(t[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < t.len() && base64_byte(t[j]) {
            j += 1;
        }
        if key_material(&t[i..j]) {
            longest = longest.max(j - i);
        }
        let q = if i > 0 { t[i - 1] } else { 0 };
        whole_string |= matches!(q, b'"' | b'\'') && (t[j..].starts_with(&[q]) || t[j..].starts_with(b"\\n") || t[j..].starts_with(b"\\r"));
        i = j;
    }
    let last_word = t.split(|b| b.is_ascii_whitespace()).rfind(|w| !w.is_empty()).map(quoted_text).is_some_and(|w| !w.is_empty() && is_base64(w));
    (longest >= 16 || whole_string || (after_long && last_word)).then_some(longest)
}

/// Where a key inside a line stops: after its END marker, or at the end of its JSON string
/// (a '"' that is not escaped), or at the line's end; and whether the key was closed there.
fn in_line_stop(line: &[u8], from: usize, end: usize, end_marker: &[u8]) -> (usize, bool) {
    let mut j = from;
    while j < end {
        if line[j] == b'-' && line[j..end].starts_with(end_marker) {
            return (j + end_marker.len(), true);
        }
        match line[j] {
            b'\\' => j += 2,
            b'"' => return (j, true),
            _ => j += 1,
        }
    }
    (end, false)
}

fn base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')
}

/// A key line made only of base64 bytes.
fn is_base64(t: &[u8]) -> bool {
    !t.is_empty() && t.iter().all(|&b| base64_byte(b))
}

/// Whether a run of base64 bytes is key material, not a word, a name in code or a number: random
/// base64 holds '+' or '/', or about as many capitals as small letters (each more than a sixth of
/// its letters), or is hex of 16 or more with digits and letters (an IV); a word or a name has
/// few capitals (Troubleshooting, loadConfigurationValue2024), a constant no small letters, a
/// number no letters (after a BEGIN marker named in code, the code's names were once taken for
/// a key's).
fn key_material(run: &[u8]) -> bool {
    if run.iter().any(|&b| b == b'+' || b == b'/') {
        return true;
    }
    let letters = run.iter().filter(|b| b.is_ascii_alphabetic()).count();
    let capitals = run.iter().filter(|b| b.is_ascii_uppercase()).count();
    // (hex of 16 or more, digits and letters both: an IV, a key in hex)
    let hex = run.len() >= 16 && run.iter().all(u8::is_ascii_hexdigit) && letters > 0 && letters < run.len();
    hex || (letters > 0 && capitals * 6 > letters && (letters - capitals) * 6 > letters)
}

/// Copies `t` without its runs of 16 or more base64 bytes that are key material (inside a
/// wrapped line). A run right after a backslash keeps its first byte: it is an escape's letter.
fn scrub(t: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < t.len() {
        if !base64_byte(t[i]) {
            out.push(t[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < t.len() && base64_byte(t[j]) {
            j += 1;
        }
        // (a run that is a JSON string's whole text, a key's line as Docker writes it, goes however
        // short: "ab3+Xyz=\n" before its closing quote)
        let q = if i > 0 { t[i - 1] } else { 0 };
        let whole_string = matches!(q, b'"' | b'\'') && (t[j..].starts_with(b"\\n") || t[j..].starts_with(b"\\r\\n") || t[j..].starts_with(&[q]));
        if whole_string || (j - i >= 16 && key_material(&t[i..j])) {
            let keep = usize::from(i > 0 && t[i - 1] == b'\\');
            out.extend_from_slice(&t[i..i + keep]);
        } else {
            out.extend_from_slice(&t[i..j]);
        }
        i = j;
    }
}

// ---------------------------------------------------------------------------------------
// Streams and whole texts

/// Hides secrets in bytes that come in pieces of any size (a command's output as it is
/// read): whole lines are scanned as they complete; a part line is kept until its '\n' (or
/// until it is [`MAX_PART`] long), so a secret split across two reads is still hidden.
#[derive(Clone, Debug, Default)]
pub struct Stream {
    hider: Hider,
    part: Vec<u8>,
}

impl Stream {
    pub fn new(hider: Hider) -> Stream {
        Stream { hider, part: Vec::new() }
    }

    /// Takes the next bytes; appends what can be written now to `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        let mut rest = bytes;
        while let Some(p) = rest.iter().position(|&b| b == b'\n') {
            let (line, tail) = rest.split_at(p + 1);
            if self.part.is_empty() {
                self.hider.line(line, out);
            } else {
                self.part.extend_from_slice(line);
                let whole = std::mem::take(&mut self.part);
                self.hider.line(&whole, out);
            }
            rest = tail;
        }
        self.part.extend_from_slice(rest);
        if self.part.len() >= MAX_PART {
            let piece = std::mem::take(&mut self.part);
            self.hider.line(&piece, out);
        }
    }

    /// At the end of the bytes: the part line kept, scanned as the last line.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.part.is_empty() {
            let last = std::mem::take(&mut self.part);
            self.hider.line(&last, out);
        }
    }

    /// The bytes kept until their line ends.
    pub fn pending(&self) -> usize {
        self.part.len()
    }

    pub fn counts(&self) -> &Counts {
        self.hider.counts()
    }
}

/// Hides the secrets of a whole text: the text with each value replaced, and the counts.
pub fn hide(text: &[u8], extra: &[Extra]) -> (Vec<u8>, Counts) {
    let mut h = Hider::new(extra.to_vec());
    let mut out = Vec::with_capacity(text.len());
    h.text(text, &mut out);
    (out, h.counts)
}

/// A short text (a command line, a cell, a name) with its secrets hidden.
pub fn hide_str(s: &str, extra: &[Extra]) -> String {
    let (h, c) = hide(s.as_bytes(), extra);
    if c.total() == 0 { s.to_string() } else { String::from_utf8_lossy(&h).into_owned() }
}

/// The extra shapes of a store's config (`secret = PREFIX CLASS MIN` lines); a line that does
/// not read as one is passed over (`run` says so).
pub fn config_extras(config: &[(String, String)]) -> Vec<Extra> {
    config.iter().filter(|(k, _)| k == "secret").filter_map(|(_, v)| Extra::parse(v).ok()).collect()
}

/// The extra shapes of this store's config (none when the store is not known).
pub fn store_extras() -> Vec<Extra> {
    crate::store::home().map(|h| config_extras(&crate::config::read(&h))).unwrap_or_default()
}

/// A command line's arguments with their secrets hidden: each argument alone, then the line as
/// one text, for a shape that spans arguments ("password = x", "Authorization: Bearer x"); when
/// that finds more, the whole hidden line as one argument (shown the same, joined by spaces).
pub fn hide_args(args: &[String], extra: &[Extra]) -> Vec<String> {
    let each: Vec<String> = args.iter().map(|a| hide_str(a, extra)).collect();
    let line = each.join(" ");
    let whole = hide_str(&line, extra);
    if whole == line { each } else { vec![whole] }
}

/// A saved command line as shown (its arguments joined by spaces), hidden again with this
/// store's shapes: a run saved by an earlier version may hold a secret spread over arguments.
pub fn shown_command(line: &str) -> String {
    static EXTRAS: std::sync::OnceLock<Vec<Extra>> = std::sync::OnceLock::new();
    hide_str(line, EXTRAS.get_or_init(store_extras))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_secret_over_several_arguments() {
        let a = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let h = hide_args(&a(&["echo", "password", "=", "Hunter2SPLITsecret3"]), &[]);
        assert!(!h.join(" ").contains("Hunter2SPLITsecret3") && h.join(" ").starts_with("echo password = [hidden"), "{h:?}");
        let h = hide_args(&a(&["curl", "-H", "Authorization:", "Bearer", "abcRUNsplitSECRET1", "https://x.invalid"]), &[]);
        assert!(!h.join(" ").contains("abcRUNsplitSECRET1"), "{h:?}");
        // nothing spread: each argument as it is
        assert_eq!(hide_args(&a(&["cargo", "test", "--release"]), &[]), a(&["cargo", "test", "--release"]));
        assert!(!shown_command("echo password = Hunter2SPLITsecret3").contains("Hunter2"));
    }

    use super::*;

    /// A made-up secret: the prefix, then `body` repeated to `n` bytes. Put together at run
    /// time, so that no secret-like text sits in the source.
    fn fake(prefix: &str, body: &str, n: usize) -> String {
        let mut b = String::new();
        while b.len() < n {
            b.push_str(body);
        }
        b.truncate(n);
        format!("{prefix}{b}")
    }

    fn h(text: &str) -> (String, Counts) {
        let (out, c) = hide(text.as_bytes(), &[]);
        (String::from_utf8(out).unwrap(), c)
    }

    fn hidden(text: &str) -> String {
        h(text).0
    }

    #[test]
    fn tokens_are_hidden() {
        let cases: Vec<(String, &str)> = vec![
            (fake("AKIA", "IOSFODNN7EXAMPLE", 16), "AWS key"),
            (fake("ASIA", "Q7Z2X9Y8W7V6U5T4", 16), "AWS key"),
            (fake("ghp_", "A1b2C3d4E5f6G7h8", 36), "GitHub token"),
            (fake("ghs_", "Zz09Yy18Xx27", 40), "GitHub token"),
            (fake("github_pat_", "11ABCDEFG0aBcDeFgHiJkL_mNoPqRsTuVwXyZ0123", 49), "GitHub token"),
            (fake("glpat-", "AbCdEfGhIjKlMnOpQrSt_-", 22), "GitLab token"),
            (fake("xoxb-", "1234567890-AbCdEfGh", 25), "Slack token"),
            (fake("sk_live_", "AbCdEfGhIj0123456789", 24), "Stripe key"),
            (fake("rk_test_", "AbCdEfGhIj0123456789", 24), "Stripe key"),
            (fake("sk-ant-", "api03-AbCd_EfGh-IjKl0123", 43), "Anthropic key"),
            (fake("sk-proj-", "AbCd_EfGh-IjKl0123", 42), "OpenAI key"),
            (fake("sk-", "AbCdEfGhIjKlMnOpQrSt0123", 48), "OpenAI key"),
            (fake("AIza", "SyAbCdEfGh_IjKl-MnOp0123456789", 35), "Google API key"),
            (format!("{}.{}.{}", fake("eyJ", "hbGciOiJIUzI1NiJ9", 17), fake("eyJ", "zdWIiOiIxMjM0NTY3ODkwIn0", 24), "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"), "JWT"),
        ];
        for (secret, name) in cases {
            let line = format!("key: x {secret} y\n");
            let (out, c) = h(&line);
            assert_eq!(out, format!("key: x [hidden: {name}] y\n"), "{secret}");
            assert_eq!((c.total(), c.get(name)), (1, 1), "{secret}");
            // at the start and at the end of a line, and in quotes
            assert_eq!(hidden(&secret), format!("[hidden: {name}]"));
            assert_eq!(hidden(&format!("[\"{secret}\"]")), format!("[\"[hidden: {name}]\"]"));
        }
    }

    #[test]
    fn near_misses_are_kept() {
        let kept = [
            fake("AKIA", "IOSFODNN7EXAMPLE", 15),                 // too short
            fake("AKIA", "IOSFODNN7EXAMPLEX", 17),                // too long
            fake("AKIB", "IOSFODNN7EXAMPLE", 16),                 // wrong prefix
            format!("x{}", fake("AKIA", "IOSFODNN7EXAMPLE", 16)), // inside a word
            fake("AKIA", "IOSFODNN7EXAMPLE", 16) + "abc",         // goes on with letters
            fake("ghp_", "A1b2C3d4E5f6G7h8", 35),                 // too short
            fake("ghx_", "A1b2C3d4E5f6G7h8", 36),                 // wrong prefix
            fake("github_pat_", "11ABCDEFG0aBcDeFgHiJkL", 21),    // too short
            fake("glpat-", "AbCdEfGhIjKlMnOpQrSt", 19),           // too short
            fake("xoxb-", "1234567890", 9),                       // too short
            fake("xoxz-", "1234567890-AbCdEfGh", 25),             // wrong prefix
            fake("sk_live_", "AbCdEfGhIj012345", 15),             // too short
            fake("pk_live_", "AbCdEfGhIj0123456789", 24),         // publishable keys are not secret
            fake("sk-", "AbCdEfGhIjKlMnOpQrS", 19),               // too short
            fake("sk-ant-", "AbCdEfGhIjKlMn0", 16),               // too short
            "task-1234567890abcdefghijkl".to_string(),            // sk- inside a word
            fake("AIza", "SyAbCdEfGh_IjKl-MnOp0123456789", 34),   // too short
            fake("AIza", "SyAbCdEfGh_IjKl-MnOp0123456789", 36),   // too long
            "eyJhbGciOi.eyJzdWIiOi".to_string(),          // two parts
            "eyJhbGciOi.notjwt.abc".to_string(),          // the second part is not eyJ
        ];
        for k in kept {
            let line = format!("value {k} end\n");
            let (out, c) = h(&line);
            assert_eq!((out.as_str(), c.total()), (line.as_str(), 0), "{k}");
        }
        for line in [
            "passwordless=true login",
            "password_hash=abc123 salt=x",
            "max_tokens: 100, input_tokens=5 token_count: 7 tokens=9",
            "if password == null or token != x",
            "use crate::token::Token; a::b",
            "Enter password: ",
            "password=",
            "\"password\": null, \"token\": true, \"secret\": false",
            "Authorization: Digest username=\"x\"",
            "Authorization: token",
            "-----BEGIN PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----",
            "https://example.com:8080/path?q=1",
            "https://user@example.com/x",
            "mailto:user:x@example.com",
            "The secret garden",
        ] {
            let (out, c) = h(line);
            assert_eq!((out.as_str(), c.total()), (line, 0), "{line}");
        }
    }

    #[test]
    fn named_values() {
        let cases = [
            ("password=hunter2 user=bob", "password=[hidden: password] user=bob"),
            ("DB_PASSWORD=hunter2", "DB_PASSWORD=[hidden: password]"),
            ("passwd: hunter2", "passwd: [hidden: passwd]"),
            ("PWD=/home/bob", "PWD=[hidden: pwd]"),
            ("db.passphrase = 'correct horse battery'", "db.passphrase = '[hidden: passphrase]'"),
            ("export API_KEY=abc123; next", "export API_KEY=[hidden: api_key]; next"),
            ("X-Api-Key: abc123", "X-Api-Key: [hidden: api_key]"),
            ("apikey=abc&page=2", "apikey=[hidden: api_key]&page=2"),
            ("?token=abc123&x=1", "?token=[hidden: token]&x=1"),
            ("AWS_SESSION_TOKEN=FwoGZXIvYXdzEJr", "AWS_SESSION_TOKEN=[hidden: token]"),
            ("client_secret: s3cr3t", "client_secret: [hidden: client_secret]"),
            ("clientSecret=s3cr3t", "clientSecret=[hidden: client_secret]"),
            ("SECRET_KEY='django-insecure-x'", "SECRET_KEY='[hidden: secret_key]'"),
            ("access_key = AKIDX", "access_key = [hidden: access_key]"),
            ("private_key: /etc/key.pem", "private_key: [hidden: private_key]"),
            ("token2=abc", "token2=[hidden: token]"),
            ("{\"password\": \"a b, c\", \"user\": \"bob\"}", "{\"password\": \"[hidden: password]\", \"user\": \"bob\"}"),
            ("{\"token\":\"a\\\"b\"}", "{\"token\":\"[hidden: token]\"}"),
            ("{'password': 'x y'}", "{'password': '[hidden: password]'}"),
            ("$db = ['password' => 'x y'];", "$db = ['password' => '[hidden: password]'];"),
            ("password := hunter2", "password := [hidden: password]"),
            ("Password: hunter2", "Password: [hidden: password]"),
            ("token=abc\\nnext=1", "token=[hidden: token]\\nnext=1"),
            ("--password=hunter2 --user=x", "--password=[hidden: password] --user=x"),
            ("config[\"password\"] = \"a b\"", "config[\"password\"] = \"[hidden: password]\""),
            ("ENV['API_TOKEN'] = 'xyz'", "ENV['API_TOKEN'] = '[hidden: token]'"),
            ("a[0] = 1, items[i]=2", "a[0] = 1, items[i]=2"),
        ];
        for (line, want) in cases {
            assert_eq!(hidden(line), want, "{line}");
        }
        // JSON inside a JSON string (an agent's tool result): \"...\"
        let s = r#"{"content":"cat .env\nPASSWORD=\"a b\"\nTOKEN=xyz\n{\"secret\": \"q r\"}"}"#;
        assert_eq!(hidden(s), r#"{"content":"cat .env\nPASSWORD=\"[hidden: password]\"\nTOKEN=[hidden: token]\n{\"secret\": \"[hidden: secret]\"}"}"#);
        // a token as a named value keeps the token's name
        let t = fake("ghp_", "A1b2C3d4E5f6G7h8", 36);
        assert_eq!(hidden(&format!("GITHUB_TOKEN={t}")), "GITHUB_TOKEN=[hidden: GitHub token]");
        // the AWS secret
        let aws = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(hidden(&format!("aws_secret_access_key = {aws}")), "aws_secret_access_key = [hidden: AWS secret]");
        assert_eq!(hidden(&format!("{{\"AWS_SECRET_ACCESS_KEY\": \"{aws}\"}}")), "{\"AWS_SECRET_ACCESS_KEY\": \"[hidden: AWS secret]\"}");
        assert_eq!(hidden("aws_secret_access_key=short"), "aws_secret_access_key=[hidden: access_key]");
    }

    #[test]
    fn json_values_that_are_not_strings() {
        let cases = [
            (r#"{"pwd":1234}"#, r#"{"pwd":"[hidden: pwd]"}"#),
            (r#"{"pwd": -12.5e3, "x": 1}"#, r#"{"pwd": "[hidden: pwd]", "x": 1}"#),
            (r#"{"token":null}"#, r#"{"token":null}"#),
            (r#"{"secret":true,"password":false}"#, r#"{"secret":true,"password":false}"#),
            (r#"{"volumes":[{"secret":{"secretName":"db"}}],"tokens":[1]}"#, r#"{"volumes":[{"secret":{"secretName":"db"}}],"tokens":[1]}"#),
            (r#"{"token":[1,2]}"#, r#"{"token":[1,2]}"#),
            (r#"{"c":"{\"pwd\": 1234, \"token\": null}"}"#, r#"{"c":"{\"pwd\": \"[hidden: pwd]\", \"token\": null}"}"#),
            // not JSON: the value as written
            ("PASSWORD=1234", "PASSWORD=[hidden: password]"),
            ("password: [x]y", "password: [hidden: password]"),
            ("token=null", "token=[hidden: token]"),
            ("{'pwd': 1234, 'x': 1}", "{'pwd': [hidden: pwd], 'x': 1}"),
        ];
        for (line, want) in cases {
            assert_eq!(hidden(line), want, "{line}");
            if line.starts_with('{') && !line.starts_with("{'") {
                assert!(crate::json::parse(hidden(line).as_bytes()).is_ok(), "{line}");
            }
        }
    }

    #[test]
    fn authorization_and_urls() {
        let jwt = format!("{}.{}.sig", fake("eyJ", "hbGciOiJIUzI1NiJ9", 17), fake("eyJ", "zdWIiOiIxMjM0NTY3ODkwIn0", 24));
        let cases = [
            ("Authorization: Bearer abc.def-ghi".to_string(), "Authorization: Bearer [hidden: authorization]".to_string()),
            ("authorization: basic dXNlcjpwYXNz".into(), "authorization: basic [hidden: authorization]".into()),
            ("curl -H 'Authorization: Bearer xyz' https://x".into(), "curl -H 'Authorization: Bearer [hidden: authorization]' https://x".into()),
            ("{\"Authorization\": \"Bearer xyz\"}".into(), "{\"Authorization\": \"Bearer [hidden: authorization]\"}".into()),
            (format!("Proxy-Authorization: Bearer {jwt}"), "Proxy-Authorization: Bearer [hidden: authorization]".into()),
            ("postgres://app:pa55w0rd@db:5432/app".into(), "postgres://app:[hidden: URL password]@db:5432/app".into()),
            ("redis://:s3cret@cache:6379".into(), "redis://:[hidden: URL password]@cache:6379".into()),
            ("mongodb+srv://u:p@ss@h1,h2/db".into(), "mongodb+srv://u:[hidden: URL password]@h1,h2/db".into()),
            ("{\"url\":\"https:\\/\\/u:pw@host\\/x\"}".into(), "{\"url\":\"https:\\/\\/u:[hidden: URL password]@host\\/x\"}".into()),
        ];
        for (line, want) in cases {
            assert_eq!(hidden(&line), want, "{line}");
        }
        let t = fake("ghp_", "A1b2C3d4E5f6G7h8", 36);
        assert_eq!(hidden(&format!("https://x:{t}@github.com/o/r.git")), "https://x:[hidden: GitHub token]@github.com/o/r.git");
        assert_eq!(hidden(&format!("https://{t}@github.com/o/r.git")), "https://[hidden: GitHub token]@github.com/o/r.git");
    }

    fn pem(label: &str, lines: usize) -> String {
        let mut s = format!("-----BEGIN {label}-----\n");
        for k in 0..lines {
            s.push_str(&fake("MIIE", &format!("owIBAAKCAQEA{k}xYz+/9"), 60));
            s.push('\n');
        }
        s.push_str("Xw==\n");
        s.push_str(&format!("-----END {label}-----\n"));
        s
    }

    #[test]
    fn private_keys() {
        // a plain key: the block becomes one marker; the END line stays
        let text = format!("before\n{}after password=x\n", pem("RSA PRIVATE KEY", 3));
        let (out, c) = h(&text);
        assert_eq!(out, "before\n[hidden: private key]\n-----END RSA PRIVATE KEY-----\nafter password=[hidden: password]\n");
        assert_eq!((c.get("private key"), c.total()), (1, 2));
        for label in ["PRIVATE KEY", "EC PRIVATE KEY", "OPENSSH PRIVATE KEY", "ENCRYPTED PRIVATE KEY", "PGP PRIVATE KEY BLOCK"] {
            let (out, c) = h(&pem(label, 2));
            assert_eq!(out, format!("[hidden: private key]\n-----END {label}-----\n"), "{label}");
            assert_eq!(c.total(), 1);
        }
        // encrypted PEM headers and a PGP armor: the headers stay, the material goes
        let enc = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,0123456789ABCDEF0123456789ABCDEF\n\nMIIEowIBAAKCAQEAxYz0123456789\n-----END RSA PRIVATE KEY-----\n";
        assert_eq!(hidden(enc), "[hidden: private key]\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,\n\n-----END RSA PRIVATE KEY-----\n");
        // in a JSON string on one line (an agent's tool result)
        let one = r#"{"content":"-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0123456789\n-----END RSA PRIVATE KEY-----\n","x":1}"#;
        assert_eq!(hidden(one), r#"{"content":"[hidden: private key]\n","x":1}"#);
        // cut short in a JSON string: hidden to the string's end
        let cut = r#"{"content":"-----BEGIN PRIVATE KEY-----\nMIIEowIBAAKCAQEA0123456789 (truncated)","x":1}"#;
        assert_eq!(hidden(cut), r#"{"content":"[hidden: private key]","x":1}"#);
        // CRI lines: a prefix on every line
        let mut cri = String::new();
        for l in pem("RSA PRIVATE KEY", 2).lines() {
            cri.push_str(&format!("2026-11-18T10:15:02Z stdout F {l}\n"));
        }
        let out = hidden(&cri);
        assert!(!out.contains("MIIE") && out.contains("[hidden: private key]") && out.contains("-----END RSA PRIVATE KEY-----"), "{out}");
        // Docker: one JSON object per line; the JSON stays valid
        let mut docker = String::new();
        for l in pem("RSA PRIVATE KEY", 2).lines() {
            docker.push_str(&format!("{{\"log\":\"{l}\\n\",\"stream\":\"stdout\",\"time\":\"2026-11-18T10:15:02Z\"}}\n"));
        }
        let out = hidden(&docker);
        assert!(!out.contains("MIIE"), "{out}");
        for l in out.lines() {
            assert!(crate::json::parse(l.as_bytes()).is_ok(), "{l}");
        }
        // a header alone in a sentence: the rest of the line goes, the next lines stay
        let msg = "error: expected -----BEGIN PRIVATE KEY----- at the start\nnext line abcdefghijklmnopqrstuvwxyz\n";
        assert_eq!(hidden(msg), "error: expected [hidden: private key]\nnext line abcdefghijklmnopqrstuvwxyz\n");
        // a BEGIN line with no END: the key lines end after MAX_KEY_LINES lines
        let mut long = String::from("-----BEGIN PRIVATE KEY-----\n");
        for _ in 0..MAX_KEY_LINES + 5 {
            long.push_str("QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=\n");
        }
        let out = hidden(&long);
        assert_eq!(out.lines().count(), 1 + 5, "{}", &out[..200.min(out.len())]);
        // a BEGIN line alone with code under it and no END: the names of the code stay, but a line
        // made only of base64 goes, a word or a key's last line
        let code = "HEADER = (\n-----BEGIN RSA PRIVATE KEY-----\nTroubleshooting\nconfigurationSettingsValue = load()\nprint(configurationSettingsValue)\nreturn processIncomingRequestPayload(event)\n";
        let out = hidden(code);
        assert!(out.ends_with("configurationSettingsValue = load()\nprint(configurationSettingsValue)\nreturn processIncomingRequestPayload(event)\n") && !out.contains("Troubleshooting"), "{out}");
        // a key's short last line, a word's letters: hidden; under indentation and in
        // Docker's JSON, however short
        let k = format!("-----BEGIN RSA PRIVATE KEY-----\n{}\nabcd\n-----END RSA PRIVATE KEY-----\n", "MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun");
        assert!(!hidden(&k).contains("abcd"), "{}", hidden(&k));
        let yaml = "key: |\n  -----BEGIN RSA PRIVATE KEY-----\n  MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n  ab3+Xyz=\n  -----END RSA PRIVATE KEY-----\n";
        assert!(!hidden(yaml).contains("ab3+Xyz"), "{}", hidden(yaml));
        let docker = "{\"log\":\"-----BEGIN RSA PRIVATE KEY-----\\n\"}\n{\"log\":\"MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\\n\"}\n{\"log\":\"ab3+Xyz=\\n\"}\n{\"log\":\"-----END RSA PRIVATE KEY-----\\n\"}\n";
        assert!(!hidden(docker).contains("ab3+Xyz"), "{}", hidden(docker));
        // a key written a line at a time in code, once shown whole: a
        // Go slice, echo lines, a JSON or YAML list, single quotes, a one-line JSON array
        let body = ["MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun", "VTLw7onLRnrq0/IzW7yWR7QkrmBL7jTKEn5u+qKhbwKfBstIs+bMY2Zkp18gnTxK", "Qmf8tx"];
        let (b, e) = ("-----BEGIN RSA PRIVATE KEY-----", "-----END RSA PRIVATE KEY-----");
        let lines = |f: &dyn Fn(&str) -> String| -> String { std::iter::once(b).chain(body.iter().copied()).chain(std::iter::once(e)).map(f).collect() };
        let forms = [
            format!("var k = strings.Join([]string{{\n{}}}, \"\\n\")\n", lines(&|l| format!("\t\"{l}\",\n"))),
            lines(&|l| format!("echo \"{l}\" >> key.pem\n")),
            format!("{{\n  \"key\": [\n{}  ]\n}}\n", lines(&|l| format!("    \"{l}\",\n"))),
            format!("key:\n{}", lines(&|l| format!("    - \"{l}\"\n"))),
            format!("const key = [\n{}].join('\\n');\n", lines(&|l| format!("  '{l}',\n"))),
            format!("{{\"key\":[{}]}}\n", std::iter::once(b).chain(body.iter().copied()).chain(std::iter::once(e)).map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(",")),
        ];
        for f in &forms {
            let out = hidden(f);
            assert!(body.iter().all(|l| !out.contains(l)), "{f}\n=>\n{out}");
        }
        // code that only names the marker: its lines after it stay
        let code = "const header = \"-----BEGIN PRIVATE KEY-----\";\nif (s.startsWith(header)) {\n  return parseConfigurationValue2024(s);\n}\n";
        assert!(hidden(code).ends_with("if (s.startsWith(header)) {\n  return parseConfigurationValue2024(s);\n}\n"), "{}", hidden(code));
        let named = "const rsaHeader = \"-----BEGIN RSA PRIVATE KEY-----\"\nconst next = loadConfigurationValue2024(path)\n";
        let out = hidden(named);
        assert!(out.ends_with("\nconst next = loadConfigurationValue2024(path)\n") && !out.contains("BEGIN RSA"), "{out}");
        // key material, and what is not
        assert!(key_material(b"MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun") && key_material(b"VTLw7onLRnrq0/IzW7yWR7Qk"));
        assert!(!key_material(b"Troubleshooting") && !key_material(b"configurationSettingsValue") && !key_material(b"processIncomingRequestPayload"));
        assert!(!key_material(b"loadConfigurationValue2024") && !key_material(b"XMLHTTPREQUESTHANDLER") && !key_material(b"1700000000000123"));
    }

    #[test]
    fn extra_shapes() {
        let x = Extra::parse("acme_ alnum 20").unwrap();
        let tok = fake("acme_", "Zz09Yy18Xx27Ww36", 24);
        let (out, c) = hide(format!("key {tok} end\n").as_bytes(), std::slice::from_ref(&x));
        assert_eq!(String::from_utf8(out).unwrap(), "key [hidden: acme_] end\n");
        assert_eq!(c.get("acme_"), 1);
        let (out, _) = hide(b"key acme_short end\n", &[x]);
        assert_eq!(out, b"key acme_short end\n");
        assert_eq!(Extra::parse("abc hex 8").unwrap().class, Class::Hex);
        assert_eq!(Extra::parse("abc= base64 8").unwrap().min, 8);
        for bad in ["", "abc alnum", "abc words 8", "abc alnum 0", "abc alnum x", "a[b alnum 8", "abc alnum 8 more"] {
            assert!(Extra::parse(bad).is_err(), "{bad}");
        }
    }

    /// A text with every kind of secret, for the stream and count tests.
    fn mixed() -> String {
        format!(
            "start {} and {}\npassword=hunter2 token: abc\n{}{{\"msg\":\"Authorization: Bearer xyz\",\"api_key\":\"k\"}}\npostgres://a:b@c/d\nend\n",
            fake("AKIA", "IOSFODNN7EXAMPLE", 16),
            fake("ghp_", "A1b2C3d4E5f6G7h8", 36),
            pem("EC PRIVATE KEY", 2)
        )
    }

    #[test]
    fn counts_are_right() {
        let (_, c) = h(&mixed());
        let want = [("AWS key", 1), ("GitHub token", 1), ("password", 1), ("token", 1), ("private key", 1), ("authorization", 1), ("api_key", 1), ("URL password", 1)];
        for (name, n) in want {
            assert_eq!(c.get(name), n, "{name}");
        }
        assert_eq!(c.total(), 8);
        assert_eq!(c.iter().count(), 8);
    }

    #[test]
    fn a_secret_split_across_reads_is_still_hidden() {
        let text = mixed();
        let (whole, counts) = hide(text.as_bytes(), &[]);
        let b = text.as_bytes();
        // every cut into two reads
        for cut in 0..=b.len() {
            let mut s = Stream::default();
            let mut out = Vec::new();
            s.push(&b[..cut], &mut out);
            s.push(&b[cut..], &mut out);
            s.finish(&mut out);
            assert!(out == whole, "cut at {cut}");
            assert_eq!(s.counts(), &counts);
        }
        // one byte at a time
        let mut s = Stream::default();
        let mut out = Vec::new();
        for byte in b {
            s.push(std::slice::from_ref(byte), &mut out);
        }
        assert_eq!(s.pending(), 0);
        s.finish(&mut out);
        assert!(out == whole);
    }

    #[test]
    fn hiding_twice_changes_nothing_more() {
        let (once, c1) = h(&mixed());
        let (twice, c2) = h(&once);
        assert_eq!(twice, once);
        assert_eq!((c1.total(), c2.total()), (8, 0));
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        let mut s = 99u64;
        for _ in 0..3000 {
            let mut line = Vec::new();
            for _ in 0..(s % 90) {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                line.push(b"abc XYZ 019 :=/-_.\"'\\{}[],;&@\t\r\x1b\xff"[(s >> 40) as usize % 33]);
            }
            let (out, c) = hide(&line, &[]);
            if c.total() == 0 {
                assert_eq!(out, line);
            }
        }
        // a long line with no line end goes out whole
        let big = vec![b'a'; MAX_PART + 10];
        let mut st = Stream::default();
        let mut out = Vec::new();
        st.push(&big, &mut out);
        assert_eq!(out.len(), MAX_PART + 10);
        st.finish(&mut out);
        assert_eq!(out.len(), MAX_PART + 10);
    }
}
