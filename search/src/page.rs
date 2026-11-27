//! A web page read by fewer itself for `fewer web` (2026-11-25): fetched with curl (macOS's
//! own; no TLS of fewer's), turned into text (headings kept as #, list items as -, scripts and
//! styles left out, entities decoded), so that the parts a question is about can be chosen from
//! it with no model at all. (Claude Code's WebFetch reads a page through a small model of its own,
//! about 25,000-30,000 tokens for a long page, measured on RFC 9110; here the page costs only the
//! part shown.)
//!
//! It reads only what Claude Code's WebFetch would read, and less: an
//! address of at most 2,000 characters with no user name or password, http taken as https, a host
//! name with a dot in it (an IP address only in its usual four numbers), and only when every
//! address the name leads to is public (not this machine, a private or local network): curl is
//! then pinned to the address checked. A redirect is followed only within the host (www. or not,
//! the same port), as WebFetch does, and each host is checked again; a redirect elsewhere is said,
//! not followed. curl reads the address as it is (no ranges or sets: -g), https only. A file that
//! is not text (a picture, a PDF) is not shown. Anything fewer does not read itself (a host name in
//! another script) is left to Claude Code's WebFetch.

use std::io::Read;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The most of a page read (bytes, as WebFetch), and how long it may take.
const MAX_PAGE: u64 = 10 * 1024 * 1024;
const PAGE_SECS: u64 = 40;
/// The longest address read (characters), and the most redirects followed (as WebFetch).
const MAX_URL: usize = 2_000;
const MAX_REDIRECTS: usize = 10;
const AGENT: &str = "Mozilla/5.0 (fewer; a page read for Claude Code)";

/// The first web address in a text (http or https), without the punctuation that ends a sentence
/// (a closing bracket only when the address has more of them than opening ones: Wikipedia's
/// Rust_(programming_language) keeps its own).
pub fn first_url(text: &str) -> Option<String> {
    let low = text.to_ascii_lowercase();
    // (the earlier of the two: an http address before an https one is the first)
    let at = match (low.find("https://"), low.find("http://")) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    let rest = &text[at..];
    let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`')).unwrap_or(rest.len());
    let mut url = &rest[..end];
    while let Some(last) = url.chars().last() {
        let cut = match last {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => url.matches(')').count() > url.matches('(').count(),
            ']' => url.matches(']').count() > url.matches('[').count(),
            _ => false,
        };
        if !cut {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    (url.len() > "https://".len()).then(|| url.to_string())
}

/// An address's #part as a page names its element: percent-decoded ('+' stays: a fragment is no
/// form, so '#C++11' names 'C++11'), its control characters and line ends taken out (it is
/// printed in fewer's own lines).
pub fn fragment_decode(a: &str) -> String {
    let raw = pct_decode(a).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| a.to_string());
    raw.chars().filter(|c| !c.is_control()).collect()
}

/// A web address fewer reads itself: https, a host name (lower case, no dot at its end), the
/// port when one is given, the path with the query; the #part kept apart (never sent).
#[derive(Clone, Debug, PartialEq)]
pub struct Addr {
    pub host: String,
    pub port: Option<u16>,
    pub rest: String,
    pub anchor: Option<String>,
}

impl Addr {
    /// As it is asked for (https, the host as checked).
    pub fn url(&self) -> String {
        format!("https://{}{}{}", self.host, self.port.map(|p| format!(":{p}")).unwrap_or_default(), self.rest)
    }
    fn port_or_443(&self) -> u16 {
        self.port.unwrap_or(443)
    }
}

/// What becomes of an address.
#[derive(Debug, PartialEq)]
pub enum Gate {
    /// read by fewer, once its host is checked
    Read(Addr),
    /// left to Claude Code's own WebFetch (why)
    Claude(String),
    /// not read (why)
    Refused(String),
}

/// %-escapes decoded (a host's; + stays +), or None when one is not two hex digits.
fn pct_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = |c: u8| (c as char).to_digit(16);
            let (Some(a), Some(c)) = (b.get(i + 1).and_then(|&c| h(c)), b.get(i + 2).and_then(|&c| h(c))) else { return None };
            out.push((a * 16 + c) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// An address as fewer reads it, or why not (as Claude Code's WebFetch, or more careful).
pub fn gate(url: &str) -> Gate {
    if url.chars().count() > MAX_URL {
        return Gate::Refused(format!("the address is longer than the {} characters Claude Code's WebFetch reads", group(MAX_URL as u64)));
    }
    let low = url.to_ascii_lowercase();
    let rest = if low.starts_with("https://") {
        &url[8..]
    } else if low.starts_with("http://") {
        &url[7..]
    } else {
        return Gate::Refused("not a web address (http or https)".into());
    };
    if url.contains('\\') || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Gate::Refused("the address holds a backslash, a space or a control character".into());
    }
    let (before, anchor) = match rest.split_once('#') {
        Some((a, b)) => (a, Some(b.to_string()).filter(|a| !a.is_empty())),
        None => (rest, None),
    };
    let split = before.find(['/', '?']).unwrap_or(before.len());
    let (authority, path) = before.split_at(split);
    if authority.contains('@') {
        return Gate::Refused("an address with a user name or password is not read (Claude Code's WebFetch refuses them too)".into());
    }
    if authority.starts_with('[') {
        return Gate::Refused("an address by IPv6 number is not read by fewer".into());
    }
    let (h, port) = match authority.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(n) if p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(n)),
            _ => return Gate::Refused("the address's port is not a number".into()),
        },
        None => (authority, None),
    };
    // the host as a browser takes it: %-escapes decoded, lower case, no dot at the end
    let Some(Ok(host)) = pct_decode(h).map(String::from_utf8) else { return Gate::Refused("the address's host name cannot be read".into()) };
    if !host.is_ascii() {
        return Gate::Claude("a host name in another script".into());
    }
    let host = host.to_ascii_lowercase().trim_end_matches('.').to_string();
    if host.is_empty() || host.len() > 253 || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') {
        return Gate::Refused("the address's host is not a host name".into());
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Gate::Refused("Claude Code's WebFetch cannot fetch localhost or other hostnames without a dot".into());
    }
    if labels.iter().any(|l| l.is_empty() || l.len() > 63) {
        return Gate::Refused("the address's host is not a host name".into());
    }
    // a host ending in a number is an IP address: only in its usual form (four numbers)
    let last = labels[labels.len() - 1];
    let numeric = last.bytes().all(|b| b.is_ascii_digit()) || (last.starts_with("0x") && last[2..].bytes().all(|b| b.is_ascii_hexdigit()));
    if numeric && host.parse::<std::net::Ipv4Addr>().is_err() {
        return Gate::Refused("an IP address written in an unusual form is not read".into());
    }
    let rest = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Gate::Read(Addr { host, port, rest, anchor })
}

/// Whether an address is on the public internet: not this machine, a private, shared or local
/// network, a reserved, documentation or multicast range (IPv6 that holds an IPv4 address by it).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            !(a.is_unspecified()
                || a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 240)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            if a.is_unspecified() || a.is_loopback() || a.is_multicast() {
                return false;
            }
            // IPv4 inside: mapped, compatible, NAT64, 6to4
            if let Some(v4) = a.to_ipv4() {
                return is_public(IpAddr::V4(v4));
            }
            // (NAT64's well-known prefix, its local-use one, 64:ff9b:1::/48, and SIIT's ::ffff:0:0/96:
            // an IPv4 address in the last 32 bits)
            let last32 = || std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
            if s[0] == 0x64 && s[1] == 0xff9b && (s[2..6] == [0, 0, 0, 0] || s[2] == 1) {
                return is_public(IpAddr::V4(last32()));
            }
            if s[0..4] == [0, 0, 0, 0] && s[4] == 0xffff && s[5] == 0 {
                return is_public(IpAddr::V4(last32()));
            }
            if s[0] == 0x2002 {
                return is_public(IpAddr::V4(std::net::Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8)));
            }
            !((s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 || (s[0] == 0x2001 && s[1] == 0x0db8) || (s[0] == 0x2001 && s[1] == 0))
        }
    }
}

/// The addresses a host name leads to, when every one of them is public (else why not).
pub fn public_address(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public(ip) { Ok(vec![ip]) } else { Err(format!("{host} is a local or private network address: fewer reads only public pages")) };
    }
    let mut all: Vec<IpAddr> = (host, port).to_socket_addrs().map_err(|e| format!("{host} could not be found ({e})"))?.map(|a| a.ip()).collect();
    all.dedup();
    match all.iter().find(|a| !is_public(**a)) {
        Some(bad) => Err(format!("{host} leads to a local or private network address ({bad}): fewer reads only public pages")),
        None if all.is_empty() => Err(format!("{host} could not be found")),
        None => Ok(all),
    }
}

/// How long curl may take to connect to one address before it tries the next.
const CONNECT_SECS: u64 = 10;

/// The environment's proxy settings, which curl would follow: a proxy looks the host's name up
/// itself, so the address checked would not be the one reached.
const PROXY_VARS: [&str; 8] = ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY", "no_proxy", "NO_PROXY"];

/// Whether this session's environment sets a proxy: then a page is left to Claude Code's own
/// WebFetch, which goes through it (fewer reads only straight to the address it checked).
pub fn proxy_in_env() -> bool {
    PROXY_VARS.iter().take(6).any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
}

/// One answer: its status, content type, where a redirect leads, its body (at most MAX_PAGE).
pub struct Fetched {
    pub code: u16,
    pub ctype: String,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

fn curl() -> &'static str {
    if std::path::Path::new("/usr/bin/curl").is_file() { "/usr/bin/curl" } else { "curl" }
}

/// One request to an address whose host is checked (no redirect followed), connected only to
/// `ips`, the addresses checked (curl tries the next when one does not connect within
/// CONNECT_SECS: 2026-11-25, a CDN's address that took no connection held a read 40 seconds).
fn fetch(addr: &Addr, ips: &[IpAddr], accept: &str) -> Result<Fetched, String> {
    let mut cmd = Command::new(curl());
    cmd.args(["-q", "-sS", "-g", "--proto", "=https", "--compressed", "--connect-timeout", &CONNECT_SECS.to_string(), "--max-time", &PAGE_SECS.to_string(), "--max-filesize", &MAX_PAGE.to_string()]);
    let pins: Vec<String> = ips
        .iter()
        .map(|ip| match ip {
            IpAddr::V4(v) => v.to_string(),
            IpAddr::V6(v) => format!("[{v}]"),
        })
        .collect();
    cmd.args(["--resolve", &format!("{}:{}:{}", addr.host, addr.port_or_443(), pins.join(","))]);
    // (no proxy, ever: the pin must hold; curl's own line, the status, on stderr, after the page,
    // which may be past MAX_PAGE)
    cmd.args(["--noproxy", "*", "-A", AGENT, "-H", &format!("Accept: {accept}"), "-w", "%{stderr}\n%{http_code}\t%{redirect_url}\t%{content_type}", "--", &addr.url()]);
    for v in PROXY_VARS {
        cmd.env_remove(v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
    // (both outputs read on threads of their own, so that neither waits on a full pipe)
    let mut err = child.stderr.take();
    let errs = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = e.by_ref().take(64 * 1024).read_to_end(&mut s);
            let _ = std::io::copy(e, &mut std::io::sink());
        }
        String::from_utf8_lossy(&s).into_owned()
    });
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let r = out.by_ref().take(MAX_PAGE + 1).read_to_end(&mut b);
        let _ = std::io::copy(&mut out, &mut std::io::sink());
        let _ = tx.send(r.map(|_| b));
    });
    let got = rx.recv_timeout(Duration::from_secs(PAGE_SECS + 5));
    if got.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let err = errs.join().unwrap_or_default();
    let out = match got {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(format!("the page could not be read: {e}")),
        Err(_) => return Err(format!("the page took more than {} seconds", PAGE_SECS + 5)),
    };
    // (stderr: curl's messages, then its own last line: STATUS, REDIRECT, CONTENT-TYPE)
    let (said, tail) = err.trim_end_matches('\n').rsplit_once('\n').unwrap_or(("", err.trim_end_matches('\n')));
    if out.len() as u64 > MAX_PAGE {
        return Err(format!("the page is more than {} bytes (WebFetch's limit too): not read", group(MAX_PAGE)));
    }
    if !status.success() {
        return Err(format!("curl could not read the page: {}", said.trim().chars().take(300).collect::<String>()));
    }
    let mut f = tail.splitn(3, '\t');
    let code: u16 = f.next().unwrap_or("").trim().parse().unwrap_or(0);
    let location = f.next().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string);
    let ctype = f.next().unwrap_or("").trim().to_lowercase();
    Ok(Fetched { code, ctype, location, body: out })
}

/// A page as fewer reads it: its text (from HTML, or as it came), or a redirect to another host.
pub enum Page {
    /// (the address read, its content type, its body as text, a note on how it was read)
    Text { addr: Addr, ctype: String, body: String, note: Option<String> },
    /// a redirect not followed: (from, to, status)
    Moved { from: String, to: String, code: u16 },
}

/// Whether a redirect stays where WebFetch follows it: the same host, www. or not, the same port.
fn same_site(a: &Addr, b: &Addr) -> bool {
    let bare = |h: &str| h.strip_prefix("www.").unwrap_or(h).to_string();
    a.port == b.port && bare(&a.host) == bare(&b.host)
}

/// The page at an address fewer reads: each host it goes to checked by `check` (the user's rules,
/// Anthropic's domain check) and its addresses public, redirects followed within the host.
pub fn read_page(addr: &Addr, check: &dyn Fn(&str) -> Result<(), String>, accept: &str) -> Result<Page, String> {
    let mut addr = addr.clone();
    for _ in 0..=MAX_REDIRECTS {
        check(&addr.host)?;
        let ips = public_address(&addr.host, addr.port_or_443())?;
        let f = fetch(&addr, &ips, accept)?;
        if matches!(f.code, 301 | 302 | 303 | 307 | 308) {
            let Some(to) = f.location else { return Err(format!("the page answered {} with no address to go to", f.code)) };
            match gate(&to) {
                Gate::Read(next) if to.to_ascii_lowercase().starts_with("https://") && same_site(&addr, &next) => {
                    addr = Addr { anchor: next.anchor.clone().or(addr.anchor.clone()), ..next };
                    continue;
                }
                _ => return Ok(Page::Moved { from: addr.url(), to, code: f.code }),
            }
        }
        if !(200..300).contains(&f.code) {
            return Err(format!("the page answered {}", f.code));
        }
        let kind = f.ctype.split(';').next().unwrap_or("").trim().to_string();
        let texty = kind.is_empty()
            || kind.starts_with("text/")
            || kind.ends_with("+xml")
            || kind.ends_with("+json")
            || matches!(kind.as_str(), "application/json" | "application/xml" | "application/javascript" | "application/x-javascript" | "application/ecmascript");
        let nul = f.body.iter().take(8192).any(|&b| b == 0);
        if !texty || nul {
            let what = if kind.is_empty() { "not text".to_string() } else { kind };
            return Err(format!(
                "the address is a file ({what}, {} bytes), not a page: not shown. A PDF or a picture: download it (curl -o FILE 'ADDRESS') and read it with {} look FILE \"question\"",
                group(f.body.len() as u64),
                crate::savers::me()
            ));
        }
        let (body, note) = as_text(&f.body, &f.ctype);
        return Ok(Page::Text { addr, ctype: f.ctype, body, note });
    }
    Err(format!("the page redirects more than {MAX_REDIRECTS} times"))
}

/// A page's bytes as text, by its character set (the content type's, else a <meta> in its first
/// 2 KB): UTF-8, or ISO-8859-1 / windows-1252 (the web's usual others); any other read as UTF-8,
/// said.
fn as_text(body: &[u8], ctype: &str) -> (String, Option<String>) {
    let (text, note) = as_text_raw(body, ctype);
    (clean(&text), note)
}

/// A page's text without what could act on a terminal or on fewer's own marks: control characters
/// but line ends and tabs (a form feed as a line end), the marks html_text_at puts in its text (as
/// U+FFFD), and a byte-order mark at its start (a BOM before
/// <title> lost the page's title).
fn clean(text: &str) -> String {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    if !text.chars().any(|c| (c.is_control() && !matches!(c, '\n' | '\t' | '\r')) || c == MARK_START || c == MARK_END) {
        return text.to_string();
    }
    text.chars()
        .filter_map(|c| match c {
            '\n' | '\t' | '\r' => Some(c),
            '\u{0C}' => Some('\n'),
            c if c == MARK_START || c == MARK_END => Some('\u{FFFD}'),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

fn as_text_raw(body: &[u8], ctype: &str) -> (String, Option<String>) {
    let from_type = ctype.split(';').skip(1).find_map(|p| p.trim().strip_prefix("charset=").map(|c| c.trim_matches(['"', '\'']).to_ascii_lowercase()));
    let from_meta = || {
        let head = String::from_utf8_lossy(&body[..body.len().min(2048)]).to_ascii_lowercase();
        let at = head.find("charset=")? + "charset=".len();
        let v: String = head[at..].trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        (!v.is_empty()).then_some(v)
    };
    let cs = from_type.or_else(from_meta).unwrap_or_else(|| "utf-8".into());
    match cs.as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => (String::from_utf8_lossy(body).into_owned(), None),
        "iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252" | "iso-8859-15" => (body.iter().map(|&b| cp1252(b)).collect(), None),
        other => match std::str::from_utf8(body) {
            Ok(t) => (t.to_string(), None),
            Err(_) => (String::from_utf8_lossy(body).into_owned(), Some(format!("its characters are in {other}, read as UTF-8: some may be wrong"))),
        },
    }
}

/// A windows-1252 byte (ISO-8859-1's superset, as browsers read both).
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    if (0x80..0xa0).contains(&b) { HIGH[(b - 0x80) as usize] } else { b as char }
}

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

/// Whether a page's body is HTML (by its content type, else by its first character).
pub fn is_html(ctype: &str, body: &str) -> bool {
    ctype.contains("html") || (ctype.is_empty() && body.trim_start().starts_with('<'))
}

/// One web result: where it came from, its title, its address, a few words of it.
pub struct Found {
    pub source: &'static str,
    pub title: String,
    pub url: String,
    pub note: String,
}

/// The hosts the free search asks (each a public API that needs no key): Wikipedia's and
/// DuckDuckGo's instant answers. (Not Stack Exchange's: Anthropic's domain check, which WebFetch
/// asks before it reads, refuses its sites and its API, probed 2026-11-25.)
pub const SEARCH_HOSTS: [&str; 2] = ["en.wikipedia.org", "api.duckduckgo.com"];

/// A public API's JSON answer, its host checked as a page's (`check`: the user's WebFetch rules,
/// Anthropic's domain check).
fn api(url: &str, check: &dyn Fn(&str) -> Result<(), String>) -> Option<crate::json::Json> {
    let Gate::Read(addr) = gate(url) else { return None };
    match read_page(&addr, check, "application/json").ok()? {
        Page::Text { body, .. } => crate::json::parse(body.as_bytes()).ok(),
        Page::Moved { .. } => None,
    }
}

/// A search with no model and no key (a free tool, no paid search): Wikipedia's articles for the question's words (their snippets) and DuckDuckGo's
/// instant answer for them (an abstract with its source), each through its own public API, its
/// host checked as a page's; a source that fails or is left out (`skip`) gives nothing.
pub fn free_search(query: &str, skip: &[&str], check: &dyn Fn(&str) -> Result<(), String>) -> Vec<Found> {
    use crate::json::Json;
    let q = crate::page::enc(query);
    let plain = |t: &str| html_text(t).split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = Vec::new();
    if !skip.contains(&"en.wikipedia.org")
        && let Some(j) = api(&format!("https://en.wikipedia.org/w/api.php?action=query&list=search&srlimit=3&srprop=snippet&format=json&formatversion=2&utf8=1&srsearch={q}"), check)
    {
        for r in j.get("query").and_then(|x| x.get("search")).and_then(Json::arr).unwrap_or(&[]) {
            let Some(t) = r.get("title").and_then(Json::str) else { continue };
            let url = format!("https://en.wikipedia.org/wiki/{}", crate::page::enc(&t.replace(' ', "_")));
            out.push(Found { source: "Wikipedia", title: t.to_string(), url, note: plain(r.get("snippet").and_then(Json::str).unwrap_or("")) });
        }
    }
    if !skip.contains(&"api.duckduckgo.com")
        && let Some(j) = api(&format!("https://api.duckduckgo.com/?format=json&no_html=1&skip_disambig=1&q={q}"), check)
        && let Some(a) = j.get("AbstractText").and_then(Json::str).filter(|a| !a.is_empty())
    {
        let url = j.get("AbstractURL").and_then(Json::str).unwrap_or("").to_string();
        out.push(Found { source: "DuckDuckGo", title: j.get("Heading").and_then(Json::str).unwrap_or("").to_string(), url, note: a.to_string() });
    }
    out
}

/// HTML as text: block elements on lines of their own, headings as # (h1) to ###### (h6), list
/// items as "- ", table cells separated by " | ", pre blocks kept as they are; scripts, styles,
/// templates and the head left out; entities decoded; spaces collapsed.
pub fn html_text(html: &str) -> String {
    html_text_at(html, None, None).0
}

/// Where a tag that starts at `i` ends (its '>', outside quoted attribute values), if it is one.
fn tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut after_eq = false;
    for (j, &c) in b.iter().enumerate().skip(i + 1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => {
                if c == b'>' {
                    return Some(j);
                }
                if (c == b'"' || c == b'\'') && after_eq {
                    quote = Some(c);
                }
                if c == b'=' {
                    after_eq = true;
                } else if !c.is_ascii_whitespace() {
                    after_eq = false;
                }
            }
        }
    }
    None
}

/// Where the end tag of `name` after `from` ends (past its '>'): `</name` then '>', a space or '/'.
fn close_of(lower: &str, from: usize, name: &str) -> Option<usize> {
    let close = format!("</{name}");
    let mut at = from;
    while let Some(e) = lower[at..].find(&close) {
        let after = at + e + close.len();
        if lower.as_bytes().get(after).is_none_or(|c| *c == b'>' || c.is_ascii_whitespace() || *c == b'/') {
            return Some(lower[after..].find('>').map_or(lower.len(), |g| after + g + 1));
        }
        at = after;
    }
    None
}

/// A link's address as an absolute one (against the page's own), or None for a link within the
/// page, a script, mail or data.
fn link_to(href: &str, base: &str) -> Option<String> {
    let h = href.trim();
    let low = h.to_ascii_lowercase();
    if h.is_empty() || h.starts_with('#') || ["javascript:", "mailto:", "data:", "tel:"].iter().any(|p| low.starts_with(p)) {
        return None;
    }
    let origin_end = base.find("://").map(|s| s + 3).and_then(|s| base[s..].find('/').map(|e| s + e)).unwrap_or(base.len());
    let abs = if low.starts_with("http://") || low.starts_with("https://") {
        h.to_string()
    } else if let Some(r) = h.strip_prefix("//") {
        format!("https://{r}")
    } else if h.starts_with('/') {
        format!("{}{h}", &base[..origin_end])
    } else {
        let page = base.split(['?', '#']).next().unwrap_or(base);
        let dir = page.rfind('/').filter(|&i| i >= origin_end).map_or(format!("{}/", &base[..origin_end]), |i| page[..=i].to_string());
        format!("{dir}{h}")
    };
    // (the page itself, at one of its parts)
    let bare = |u: &str| u.split('#').next().unwrap_or(u).to_string();
    (bare(&abs) != bare(base)).then_some(abs)
}

/// Marks (private-use characters) where an address's #part starts and ends in the text.
const MARK_START: char = '\u{E000}';
const MARK_END: char = '\u{E001}';

/// An attribute's value in a tag's text (`name` in lower case: id, name).
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let low = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = low[from..].find(name).map(|i| from + i) {
        from = i + name.len();
        let before_ok = i == 0 || low.as_bytes()[i - 1].is_ascii_whitespace();
        let rest = low[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let at = tag.len() - rest.len() + 1;
        let v = tag[at..].trim_start();
        return match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].find(q).map(|e| &v[1..1 + e]),
            _ => Some(v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("")),
        };
    }
    None
}

/// HTML as text (as `html_text`), and where the part an address's #anchor names lies in it (byte
/// range): from the element with that id (or name) to the end of what it heads: a definition
/// term (`dt`, as reference docs mark each function) to the next term of its list, a heading to
/// the next heading of its level or above, a section or other block to its own end tag, an
/// element inside a `summary` (as rustdoc marks each method: its signature in the summary, its
/// description after it) to the next summary or a heading of level 3 or above, any other element
/// to the next heading. With `links` (the page's address), each link's address
/// follows its text in brackets, on the same line (the lines as without them).
pub fn html_text_at(html: &str, anchor: Option<&str>, links: Option<&str>) -> (String, Option<(usize, usize)>) {
    // what the anchor's element is: 0 none yet, then its kind until its part ends
    enum Part {
        Wait,
        Term(usize),
        Heading(u8),
        Block(String, usize),
        Summary,
        Other,
        Done,
    }
    let mut part = if anchor.is_some() { Part::Wait } else { Part::Done };
    let mut dl_depth = 0usize;
    let mut summary_open = 0usize;
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let b = html.as_bytes();
    let mut i = 0;
    let mut pre = 0usize;
    // (whether the open <pre> is fenced: code of several lines between two fence lines, so that a
    // part of the page keeps it whole and its "# comment" lines are no headings; a <pre> of one
    // line, a signature, stays a line)
    let mut fenced = false;
    // the link being read: its address, and where its text starts in `out`
    let mut link: Option<(String, usize)> = None;
    let mut titled = false;
    // (once a tag has no end, the rest of the page is text: a tag looked for at each '<' after it
    // took time growing with the square of the page)
    let mut no_more_tags = false;
    while i < b.len() {
        // a tag: '<' then a letter, '/', '!' or '?', and its '>' (else the '<' is text)
        let tag_at = !no_more_tags && b[i] == b'<' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'));
        if tag_at && lower[i..].starts_with("<!--") {
            i = lower[i..].find("-->").map_or(b.len(), |e| i + e + 3);
            continue;
        }
        let found = tag_at.then(|| tag_end(b, i)).flatten();
        if tag_at && found.is_none() {
            no_more_tags = true;
        }
        if let Some(end) = found {
            let tag = &lower[i + 1..end];
            let closing = tag.starts_with('/');
            let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ':').collect();
            // ('/>' ends only svg's and math's elements: a browser reads <script/> as a script
            // that goes on)
            let self_closing = tag.trim_end().ends_with('/') && !["script", "style", "noscript", "template", "title"].contains(&name.as_str());
            let level = (name.len() == 2 && name.starts_with('h') && (b'1'..=b'6').contains(&name.as_bytes()[1])).then(|| name.as_bytes()[1] - b'0');
            // the anchor's part: where it ends (before this tag's own text), then where it starts
            match &mut part {
                Part::Term(d) if !closing && name == "dt" && dl_depth == *d => {
                    out.push(MARK_END);
                    part = Part::Done;
                }
                Part::Term(d) if closing && name == "dl" && dl_depth == *d => {
                    out.push(MARK_END);
                    part = Part::Done;
                }
                Part::Heading(n) if !closing && level.is_some_and(|l| l <= *n) => {
                    out.push(MARK_END);
                    part = Part::Done;
                }
                Part::Block(b, depth) if *b == name => {
                    if closing {
                        *depth -= 1;
                        if *depth == 0 {
                            out.push(MARK_END);
                            part = Part::Done;
                        }
                    } else {
                        *depth += 1;
                    }
                }
                Part::Summary if !closing && (name == "summary" || level.is_some_and(|l| l <= 3)) => {
                    out.push(MARK_END);
                    part = Part::Done;
                }
                Part::Other if !closing && level.is_some() => {
                    out.push(MARK_END);
                    part = Part::Done;
                }
                Part::Wait if !closing => {
                    let full = &html[i + 1..end];
                    if let Some(a) = anchor
                        && (attr(full, "id") == Some(a) || attr(full, "name") == Some(a))
                    {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push(MARK_START);
                        part = match (name.as_str(), level) {
                            ("dt", _) => Part::Term(dl_depth),
                            _ if summary_open > 0 => Part::Summary,
                            (_, Some(l)) => Part::Heading(l),
                            ("section" | "div" | "article" | "main" | "aside" | "li" | "dl" | "table", _) => Part::Block(name.clone(), 1),
                            _ => Part::Other,
                        };
                    }
                }
                _ => {}
            }
            if name == "dl" {
                if closing {
                    dl_depth = dl_depth.saturating_sub(1);
                } else {
                    dl_depth += 1;
                }
            }
            if name == "summary" && !self_closing {
                if closing {
                    summary_open = summary_open.saturating_sub(1);
                } else {
                    summary_open += 1;
                }
            }
            // what is never text: skipped to its end tag (the head's own parts are: its title, its
            // scripts and styles; a head left open must not take the page with it)
            if !closing && !self_closing && ["script", "style", "noscript", "template", "svg", "title", "math"].contains(&name.as_str()) {
                let after = close_of(&lower, end, &name).unwrap_or(b.len());
                // the page's title, once, as its first line (what the page is; some pages say it
                // nowhere else)
                if name == "title" && !titled && out.trim().is_empty() {
                    let t = lower[end + 1..after].rfind("</").map_or("", |c| &html[end + 1..end + 1 + c]);
                    let t = decode(t).split_whitespace().collect::<Vec<_>>().join(" ");
                    if !t.is_empty() {
                        out.push_str(&t);
                        out.push('\n');
                    }
                    titled = true;
                }
                i = after;
                continue;
            }
            if name == "a"
                && let Some(base) = links
            {
                // (the address on its text's last line, never a line of its own nor split by a line
                // end in it: the kept page's lines are the counted text's)
                if !closing {
                    link = attr(&html[i + 1..end], "href").and_then(|h| link_to(&decode(h), base)).map(|h| (h.chars().filter(|c| !c.is_control()).collect(), out.len()));
                } else if let Some((href, at)) = link.take() {
                    let text = out[at..].trim();
                    if !text.is_empty() && text != href {
                        let end_of_text = out.trim_end().len();
                        let tail = out.split_off(end_of_text);
                        out.push_str(&format!(" ({href})"));
                        out.push_str(&tail);
                    }
                }
            }
            match name.as_str() {
                "br" => out.push('\n'),
                "pre" => {
                    if closing {
                        pre = pre.saturating_sub(1);
                        out.push('\n');
                        if pre == 0 && fenced {
                            out.push_str(PRE_FENCE);
                            out.push('\n');
                            fenced = false;
                        }
                    } else {
                        out.push('\n');
                        if pre == 0 && !self_closing {
                            let close = close_of(&lower, end, "pre").unwrap_or(b.len());
                            let inner = &lower[end + 1..close.max(end + 1)];
                            if (inner.trim().contains('\n') || inner.contains("<br")) && !pre_is_prose(inner) {
                                out.push_str(PRE_FENCE);
                                out.push('\n');
                                fenced = true;
                            }
                        }
                        pre += 1;
                    }
                }
                h if h.len() == 2 && h.starts_with('h') && h.as_bytes()[1].is_ascii_digit() && (b'1'..=b'6').contains(&h.as_bytes()[1]) => {
                    out.push_str("\n\n");
                    if !closing {
                        out.push_str(&"#".repeat((h.as_bytes()[1] - b'0') as usize));
                        out.push(' ');
                    }
                }
                "li" if !closing => out.push_str("\n- "),
                "td" | "th" if !closing => out.push_str(" | "),
                "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside" | "table" | "tr" | "ul" | "ol" | "dl" | "dt" | "dd" | "blockquote" | "figure" | "figcaption" | "form" | "hr" => out.push('\n'),
                _ => {}
            }
            i = end + 1;
            continue;
        }
        // text up to the next tag (from a '<' that is text, past it; all the rest when no tag can
        // end any more)
        let from = if b[i] == b'<' { i + 1 } else { i };
        let next = if no_more_tags { b.len() } else { html[from..].find('<').map_or(b.len(), |e| from + e) };
        let text = decode(&html[i..next]);
        if pre > 0 {
            out.push_str(&text);
        } else {
            let mut last_space = out.ends_with([' ', '\n']);
            for c in text.chars() {
                if c.is_whitespace() {
                    if !last_space {
                        out.push(' ');
                        last_space = true;
                    }
                } else {
                    out.push(c);
                    last_space = false;
                }
            }
        }
        i = next;
    }
    if matches!(part, Part::Term(_) | Part::Heading(_) | Part::Block(..) | Part::Summary | Part::Other) {
        out.push(MARK_END);
    }
    // lines trimmed, at most one empty line in a row; the marks' places taken as whole lines
    let mut text = String::new();
    let mut empty = 0;
    let (mut start, mut stop): (Option<usize>, Option<usize>) = (None, None);
    for raw in out.lines() {
        if raw.contains(MARK_START) && start.is_none() {
            start = Some(text.len());
        }
        let ends_here = raw.contains(MARK_END) && stop.is_none();
        let l: String = raw.chars().filter(|&c| c != MARK_START && c != MARK_END).collect();
        // (a mark that ends the part before this line's text: the part stops before it)
        if ends_here && l.trim().is_empty() {
            stop = Some(text.len());
        }
        let l = l.trim_end();
        let l = if l.trim_start().starts_with('|') { l.trim_start() } else { l };
        if l.trim().is_empty() {
            empty += 1;
            if empty == 1 && !text.is_empty() {
                text.push('\n');
            }
            continue;
        }
        empty = 0;
        if ends_here && stop.is_none() {
            // the mark at the end of this line's text: the part takes the line
            let before_mark = raw.split(MARK_END).next().unwrap_or("");
            if before_mark.trim().is_empty() {
                stop = Some(text.len());
            }
        }
        text.push_str(if l.starts_with(' ') && !l.starts_with("  ") { l.trim_start() } else { l });
        text.push('\n');
        if ends_here && stop.is_none() {
            stop = Some(text.len());
        }
    }
    let range = match (start, stop) {
        (Some(a), Some(b)) if b > a => Some((a, b)),
        (Some(a), None) if anchor.is_some() => Some((a, text.len())),
        _ => None,
    };
    // (without the line ends at its edges)
    let range = range.map(|(mut a, mut b)| {
        while a < b && text.as_bytes()[a] == b'\n' {
            a += 1;
        }
        while b > a && text.as_bytes()[b - 1] == b'\n' {
            b -= 1;
        }
        (a, b)
    });
    (text, range)
}

/// Whether a <pre>'s text is prose set in fixed lines, not code: ten lines or more, nearly all of
/// it letters (an older RFC's pages as rfc-editor.org shows them, 55 lines each): its lines stay
/// the page's lines, not one block of code.
fn pre_is_prose(inner: &str) -> bool {
    let mut in_tag = false;
    let (mut lines, mut letters, mut other) = (1usize, 0usize, 0usize);
    for c in inner.trim().chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            '\n' => lines += 1,
            c if c.is_whitespace() => {}
            c if c.is_alphabetic() => letters += 1,
            _ => other += 1,
        }
    }
    lines >= 10 && letters * 100 > (letters + other) * 85
}

/// The fence line around a page's code of several lines (four marks: a line of three in the code,
/// Markdown's own, does not end it).
const PRE_FENCE: &str = "````";

/// HTML entities decoded (the common names, and numbers).
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        // (the ';' looked for in the next few bytes only: text with many '&' and no ';' stays quick)
        let end = after.bytes().take(12).position(|c| c == b';').filter(|&e| e <= 10);
        let ent = end.map(|e| &after[..e]);
        let ch: Option<String> = ent.and_then(|e| match e {
            "amp" => Some("&".into()),
            "lt" => Some("<".into()),
            "gt" => Some(">".into()),
            "quot" => Some("\"".into()),
            "apos" | "#39" => Some("'".into()),
            "nbsp" => Some(" ".into()),
            "mdash" => Some("—".into()),
            "ndash" => Some("–".into()),
            "hellip" => Some("…".into()),
            "rsquo" | "lsquo" => Some("'".into()),
            "ldquo" | "rdquo" => Some("\"".into()),
            "copy" => Some("©".into()),
            "reg" => Some("®".into()),
            "times" => Some("×".into()),
            "sect" => Some("§".into()),
            // (a number that is no character, or 0, as browsers read it: U+FFFD)
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().map(|n| char::from_u32(n).filter(|&c| c != '\0').unwrap_or('\u{FFFD}').to_string()),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().map(|n| char::from_u32(n).filter(|&c| c != '\0').unwrap_or('\u{FFFD}').to_string()),
            _ => None,
        });
        match (ch, end) {
            (Some(c), Some(e)) => {
                out.push_str(&clean(&c));
                rest = &after[e + 1..];
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Percent-encoding for a query or form value (RFC 3986's unreserved characters kept).
pub(crate) fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_as_text() {
        let h = "<html><head><title>T</title><style>p{color:red}</style></head><body><nav>Menu</nav>\
                 <h2 id=s1>15.5.17. 416 Range Not Satisfiable</h2><p>The <em>416</em> status code &amp; more&nbsp;text.</p>\
                 <script>var x = '<p>no</p>';</script><ul><li>one</li><li>two &#x2014; three</li></ul>\
                 <pre>  kept   as\n  is</pre><table><tr><td>a</td><td>b</td></tr></table><!-- hidden --></body></html>";
        let t = html_text(h);
        assert!(t.contains("## 15.5.17. 416 Range Not Satisfiable\n"), "{t}");
        assert!(t.contains("The 416 status code & more text."), "{t}");
        assert!(t.contains("- one\n- two — three\n"), "{t}");
        assert!(t.contains("\n````\n  kept   as\n  is\n````\n"), "{t}");
        // (a <pre> of one line stays a line: a signature; a "# comment" in code is fenced in)
        let t2 = html_text("<p>a</p><pre>pub fn retain(&amp;mut self)</pre><pre>x = 1\n# not a heading\ny = 2</pre><p>b</p>");
        assert!(t2.contains("\npub fn retain(&mut self)\n") && t2.contains("````\nx = 1\n# not a heading\ny = 2\n````"), "{t2}");
        // (prose in fixed lines, an older RFC's page: its lines, no fence)
        let rfc: String = (0..12).map(|i| format!("   The authorization server issues the code number {i} to the client here.\n")).collect();
        let t3 = html_text(&format!("<pre class=\"newpage\">{rfc}</pre>"));
        assert!(!t3.contains("````") && t3.contains("number 3 to the client"), "{t3}");
        assert!(t.contains("| a | b"), "{t}");
        assert!(!t.contains("no</p>") && !t.contains("color") && !t.contains("hidden") && t.starts_with("T\n") && !t.contains("\nT\n"), "{t}");
    }

    #[test]
    fn the_part_an_anchor_names() {
        // a definition term: to the next term of its list (a nested list's terms inside it)
        let h = "<h1>Functions</h1><dl><dt id=\"map\">map(f, it)</dt><dd><p>Map things.</p></dd></dl>\
                 <dl><dt id=\"zip\">zip(*iterables, strict=False)</dt><dd><p>Zip them.</p><dl><dt id=\"zip.inner\">inner()</dt><dd>x</dd></dl>\
                 <p>Changed in version 3.10: Added the strict argument.</p></dd><dt id=\"zz\">zz()</dt><dd>after</dd></dl>";
        let (t, r) = html_text_at(h, Some("zip"), None);
        let (a, b) = r.unwrap();
        let part = &t[a..b];
        assert!(part.starts_with("zip(*iterables, strict=False)") && part.contains("inner()") && part.contains("Changed in version 3.10") && !part.contains("zz()") && !part.contains("Map things"), "{part:?}");
        // a heading: to the next heading of its level or above
        let h = "<h2 id=\"a\">A</h2><p>one</p><h3>A.1</h3><p>two</p><h2 id=\"b\">B</h2><p>three</p>";
        let (t, r) = html_text_at(h, Some("a"), None);
        let (a, b) = r.unwrap();
        assert!(t[a..b].contains("one") && t[a..b].contains("two") && !t[a..b].contains("three"), "{:?}", &t[a..b]);
        // a section: to its own end; no such anchor: none; the text the same as html_text's
        let h = "<section id=\"s\"><p>in</p><section id=\"t\"><p>deeper</p></section><p>still in</p></section><p>out</p>";
        let (t, r) = html_text_at(h, Some("s"), None);
        let (a, b) = r.unwrap();
        assert!(t[a..b].contains("still in") && !t[a..b].contains("out"), "{:?}", &t[a..b]);
        assert_eq!(html_text_at(h, Some("nope"), None).1, None);
        assert_eq!(html_text_at(h, Some("s"), None).0, html_text(h));
        // an element inside a summary (rustdoc's methods, DevDocs' copy without its details): to
        // the next summary, its description with it (2026-11-25: retain's part was its signature)
        let h = "<h2>Methods</h2><summary><section id=\"method.retain\"><pre>pub fn retain(&amp;mut self, f: F)</pre></section></summary><div class=\"docblock\"><p>Retains only the elements specified by the predicate.</p><h5>Examples</h5><pre>v.retain(f)</pre></div>\
                 <summary><section id=\"method.retain_mut\"><pre>pub fn retain_mut()</pre></section></summary><div><p>Mutably.</p></div><h2>Traits</h2><p>later</p>";
        let (t, r) = html_text_at(h, Some("method.retain"), None);
        let (a, b) = r.unwrap();
        assert!(t[a..b].starts_with("pub fn retain(&mut self, f: F)") && t[a..b].contains("Retains only the elements") && t[a..b].contains("v.retain(f)") && !t[a..b].contains("retain_mut"), "{:?}", &t[a..b]);
        let (t, r) = html_text_at(h, Some("method.retain_mut"), None);
        let (a, b) = r.unwrap();
        assert!(t[a..b].contains("Mutably.") && !t[a..b].contains("later"), "{:?}", &t[a..b]);
    }

    #[test]
    fn the_address_in_a_question() {
        assert_eq!(first_url("Read RFC 9110 (https://www.rfc-editor.org/rfc/rfc9110) on the web: which code?").as_deref(), Some("https://www.rfc-editor.org/rfc/rfc9110"));
        assert_eq!(first_url("see https://a.example/x.html.").as_deref(), Some("https://a.example/x.html"));
        assert_eq!(first_url("no address here"), None);
        // a bracket of the address's own stays
        assert_eq!(first_url("Read https://en.wikipedia.org/wiki/Rust_(programming_language) and say").as_deref(), Some("https://en.wikipedia.org/wiki/Rust_(programming_language)"));
        assert_eq!(first_url("[x](https://example.com/a_(b))").as_deref(), Some("https://example.com/a_(b)"));
        assert_eq!(first_url("HTTPS://Example.COM/x").as_deref(), Some("HTTPS://Example.COM/x"));
        assert_eq!(decode("a &amp; b &#65;&#x42; &bogus; &"), "a & b AB &bogus; &");
        // a number that is no character, or 0: U+FFFD
        assert_eq!(decode("&#0;|&#xD800;|&#x110000;|&#65;"), "\u{FFFD}|\u{FFFD}|\u{FFFD}|A");
        // many '&' with no ';' near: quick, and kept
        let amps = "&& ".repeat(200_000);
        let t = std::time::Instant::now();
        assert_eq!(decode(&amps), amps);
        assert!(t.elapsed().as_millis() < 2_000, "{:?}", t.elapsed());
    }

    #[test]
    fn addresses_as_webfetch_takes_them() {
        let read = |u: &str| match gate(u) {
            Gate::Read(a) => a,
            g => panic!("{u}: {g:?}"),
        };
        let a = read("http://Example.COM./docs/x?y=[1-3]#Part");
        assert_eq!((a.host.as_str(), a.port, a.rest.as_str(), a.anchor.as_deref()), ("example.com", None, "/docs/x?y=[1-3]", Some("Part")));
        assert_eq!(a.url(), "https://example.com/docs/x?y=[1-3]");
        // %-escapes in the host decoded, as a browser (and curl) reads them
        assert_eq!(read("https://example%2ecom/").host, "example.com");
        assert_eq!(read("https://www.example.com:8443?q=1").url(), "https://www.example.com:8443/?q=1");
        assert_eq!(read("https://93.184.216.34/").host, "93.184.216.34");
        let refused = |u: &str| matches!(gate(u), Gate::Refused(_));
        for u in [
            "https://user:pw@example.com/",
            "https://example.com@evil.example/",
            "http://localhost:8080/x",
            "https://intranet/",
            "http://127.1/",
            "http://2130706433/",
            "http://0x7f.0.0.1/",
            "https://[::1]/",
            "https://exa mple.com/",
            "https://example.com\\@evil.example/",
            "https://example.com:http/",
            "ftp://example.com/",
            "https://%zz.com/",
            "https://a..b.com/",
            "https://under_score{x}.com/",
        ] {
            assert!(refused(u), "{u}: {:?}", gate(u));
        }
        assert!(refused(&format!("https://example.com/{}", "a".repeat(2_000))));
        assert!(matches!(gate("https://bücher.example/"), Gate::Claude(_)));
    }

    #[test]
    fn public_addresses_only() {
        let p = |s: &str| is_public(s.parse().unwrap());
        for s in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111", "2002:5db8:d822::1"] {
            assert!(p(s), "{s}");
        }
        for s in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "0.0.0.0", "100.64.0.1", "192.0.0.1", "198.18.0.1", "240.0.0.1", "255.255.255.255", "224.0.0.1", "192.0.2.1",
            "::1", "::", "fe80::1", "fc00::1", "fd12::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1", "64:ff9b::a9fe:a9fe", "2002:7f00:1::", "2001:db8::1", "2001:0:4136:e378::1", "ff02::1",
        ] {
            assert!(!p(s), "{s}");
        }
        assert!(public_address("127.0.0.1", 443).is_err());
        assert!(public_address("localhost", 443).is_err());
    }

    #[test]
    fn html_fewer_reads_as_a_browser() {
        // a head left open does not take the page
        assert!(html_text("<html><head><title>T</title><meta charset=utf-8><p>Body text here.</p></html>").contains("Body text here."));
        assert!(html_text("<head><title>T</title><body><header>Intro.</header><p>After header.</p>").contains("Intro."));
        // a self-closing svg; a '<' in text; a '>' inside an attribute's quotes; a tag never closed
        assert!(html_text("<p>first</p><svg width=10 height=10 /><p>second</p>").contains("second"));
        assert!(html_text("<p>if a < b and c > d then x</p>").contains("if a < b and c > d then x"));
        let t = html_text("<a title=\"a>b\" href=\"/x\">link text after</a>");
        assert!(t.contains("link text after") && !t.contains("href"), "{t}");
        assert!(html_text("<p>x <b unclosed").contains("x <b unclosed"));
        // a custom element is not the element its name starts with
        assert!(html_text("<p>a</p><title-bar>The zebra answer is 42.</title-bar><p>b</p>").contains("The zebra answer is 42."));
        // links: their addresses after their text, only when asked (same lines)
        let h = "<p>Download it from <a href=\"https://dl.example/tool-2.4.1.tar.gz\">this page</a>, see <a href=\"/docs/a.html\">docs</a>, <a href=\"#top\">top</a>, <a href=\"b.html\">b</a>.</p>";
        let (plain, _) = html_text_at(h, None, None);
        let (linked, _) = html_text_at(h, None, Some("https://site.example/guide/page.html"));
        assert!(plain.contains("Download it from this page, see docs, top, b."), "{plain}");
        assert!(linked.contains("this page (https://dl.example/tool-2.4.1.tar.gz), see docs (https://site.example/docs/a.html), top, b (https://site.example/guide/b.html)."), "{linked}");
        assert_eq!(plain.lines().count(), linked.lines().count());
    }

    #[test]
    fn character_sets() {
        let latin = b"Le caf\xe9 co\xfbte 3\x80";
        assert_eq!(as_text(latin, "text/html; charset=ISO-8859-1").0, "Le café coûte 3€");
        assert_eq!(as_text(b"<meta charset=\"windows-1252\"><p>caf\xe9", "text/html").0, "<meta charset=\"windows-1252\"><p>café");
        assert_eq!(as_text("café".as_bytes(), "text/html").0, "café");
        let (t, note) = as_text(b"\x83\x65", "text/html; charset=shift_jis");
        assert!(note.is_some_and(|n| n.contains("shift_jis")) && !t.is_empty());
    }

    #[test]
    fn hostile_pages_and_addresses() {
        // a megabyte of tags that never end, read in a moment
        let t = std::time::Instant::now();
        let page = format!("<p>start</p>{}", "<a href=x ".repeat(100_000));
        let text = html_text(&page);
        assert!(t.elapsed().as_secs_f64() < 10.0 && text.starts_with("start"), "{:?}", t.elapsed());
        // a script written '/>' still hides what it holds
        assert!(!html_text("<p>a</p><script src=x />alert(1)</script><p>b</p>").contains("alert"));
        // the earlier address
        assert_eq!(first_url("see http://a.example/x then https://b.example/y").as_deref(), Some("http://a.example/x"));
        // marks, control characters, escapes and a BOM out, the title kept
        let (t, _) = as_text("\u{feff}<html><head><title>The Title</title></head><body><p>A\u{1b}]0;x\u{7}B &#27;[2J C \u{e001} D</p></body></html>".as_bytes(), "text/html");
        let h = html_text(&t);
        assert!(h.starts_with("The Title") && !h.contains('\u{1b}') && !h.contains('\u{7}') && !h.contains('\u{e001}') && h.contains("D"), "{h:?}");
        // NAT64's local prefix and SIIT with a private IPv4 inside
        for a in ["64:ff9b:1::a00:1", "::ffff:0:a00:1", "64:ff9b::a00:1"] {
            assert!(!is_public(a.parse().unwrap()), "{a}");
        }
        assert!(is_public("64:ff9b:1::808:808".parse().unwrap()));
    }

    #[test]
    fn tag_ends_outside_quotes() {
        let b = b"<a title=\"x>y\" b='>' c=d>rest";
        assert_eq!(tag_end(b, 0), Some(b.len() - 5));
        assert_eq!(tag_end(b"<a b=\"never closed>", 0), None);
    }
}
