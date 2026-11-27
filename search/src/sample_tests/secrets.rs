//! Secret hiding (src/secrets.rs) on random and real text:
//! - random lines built from the shapes' own pieces never panic, and the stream, fed in
//!   random pieces, writes exactly what hiding the whole text writes;
//! - on the samples, hiding changes only the lines with a hit, and hiding again changes
//!   nothing;
//! - an agent's session line full of secrets is still one valid JSON object after hiding.

use super::{files, have_public, read};
use crate::json;
use crate::secrets::{Extra, Hider, Stream, hide};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces of the shapes and of the text around them.
const PIECES: &[&str] = &[
    "AKIA", "ASIA", "AIza", "ghp_", "gho_", "github_pat_", "glpat-", "xoxb-", "sk_live_", "rk_test_", "sk-", "sk-ant-", "sk-proj-",
    "eyJ", "eyJhbGciOiJIUzI1NiJ9", ".", "-----BEGIN ", "-----END ", "RSA ", "PRIVATE KEY", " BLOCK", "-----", "password", "PASSWD",
    "pwd", "token", "Token", "api_key", "X-Api-Key", "client_secret", "aws_secret_access_key", "Authorization", "Bearer ", "basic ",
    "=", "==", "=>", ":=", ":", "::", "://", ":\\/\\/", "@", "&", ";", ",", " ", "  ", "\t", "\"", "'", "\\\"", "\\", "\\n", "\r",
    "\x1b[31m", "{", "}", "[", "]", "null", "true", "[hidden: ", "]", "https", "postgres", "user", "pa55", "host", "/",
    "ABCDEFGHIJKLMNOP", "0123456789", "abcdefghijklmnopqrstuvwxyz", "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=", "Xw==", "\u{e9}",
];

fn random_text(r: &mut Rng, lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..lines {
        for _ in 0..r.below(14) {
            if r.below(10) == 0 {
                out.push(r.next() as u8);
            } else {
                out.extend_from_slice(PIECES[r.below(PIECES.len())].as_bytes());
            }
        }
        if r.below(8) > 0 {
            out.push(b'\n');
        }
    }
    out
}

#[test]
fn random_text_never_panics_and_streams_equal_the_whole() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    let extra = vec![Extra::parse("acme_ alnum 8").unwrap(), Extra::parse("-x base64 4").unwrap()];
    for _ in 0..1500 {
        let lines = 1 + r.below(12);
        let text = random_text(&mut r, lines);
        let (whole, counts) = hide(&text, &extra);
        // the stream, fed in random pieces
        let mut s = Stream::new(Hider::new(extra.clone()));
        let mut out = Vec::new();
        let mut i = 0;
        while i < text.len() {
            let n = 1 + r.below(16);
            let e = (i + n).min(text.len());
            s.push(&text[i..e], &mut out);
            i = e;
        }
        s.finish(&mut out);
        assert!(out == whole, "{:?}", String::from_utf8_lossy(&text));
        assert_eq!(s.counts(), &counts);
        // the line ends are all kept, unless a private key's lines went
        if counts.get("private key") == 0 {
            assert_eq!(whole.iter().filter(|&&b| b == b'\n').count(), text.iter().filter(|&&b| b == b'\n').count());
        }
    }
}

#[test]
fn samples_change_only_where_a_secret_was_hidden() {
    let public = have_public("samples_change_only_where_a_secret_was_hidden: the public samples");
    // 20 made-up files, and 21 public ones with FEWER_SAMPLES
    let paths = files(&["loghub", "json_converted", "made_up", "keys"]);
    assert!(paths.len() >= if public { 40 } else { 20 });
    let mut total = 0;
    for path in paths {
        let text = read(&path);
        let mut h = Hider::default();
        for line in text.split_inclusive(|&b| b == b'\n') {
            let before = h.counts().total();
            let mut out = Vec::new();
            h.line(line, &mut out);
            if h.counts().total() == before {
                assert!(out == line, "{path}: {:?}", String::from_utf8_lossy(line));
            } else {
                assert!(out.windows(9).any(|w| w == b"[hidden: "));
            }
        }
        total += h.counts().total();
        let (once, _) = hide(&text, &[]);
        let (twice, again) = hide(&once, &[]);
        assert!(twice == once && again.total() == 0, "{path}: hiding twice changed more");
    }
    // the samples hold no secrets; the name list does catch a few "token" fields of the
    // public ones (Android's window tokens, a Hadoop and a macOS message): 13 values. The
    // made-up samples give none.
    assert_eq!(total, if public { 13 } else { 0 });
}

#[test]
fn a_session_line_stays_valid_json() {
    let key = format!("-----BEGIN OPENSSH PRIVATE KEY-----\\n{}\\n-----END OPENSSH PRIVATE KEY-----\\n", "b3BlbnNzaC1rZXktdjEAAAAABG5vbmU".repeat(3));
    let env = "DB_PASSWORD=hunter2\\nAPI_KEY=\\\"k e y\\\"\\nexport GITHUB_TOKEN=abc\\\\def";
    let line = format!(
        r#"{{"type":"user","sessionId":"s1","message":{{"role":"user","content":[{{"type":"tool_result","content":"{env}\n{key}curl -H \"Authorization: Bearer xyz\" https://u:pw@h/x\n{{\"password\": \"a\\\"b\", \"token\": null}}"}}]}}}}"#
    );
    assert!(json::parse(line.as_bytes()).is_ok(), "the made-up line must be JSON: {line}");
    let (out, c) = hide(line.as_bytes(), &[]);
    let out = String::from_utf8(out).unwrap();
    assert!(json::parse(out.as_bytes()).is_ok(), "{out}");
    for gone in ["hunter2", "k e y", "abc", "b3BlbnNz", "xyz", ":pw@", "a\\\\\\\"b"] {
        assert!(!out.contains(gone), "{gone} is still in {out}");
    }
    assert_eq!((c.get("password"), c.get("api_key"), c.get("token"), c.get("private key"), c.get("authorization"), c.get("URL password")), (2, 1, 1, 1, 1, 1));
}

/// SECRETS.md, the published list, names every shape the code hides.
#[test]
fn the_published_list_names_every_shape() {
    use crate::secrets::SHAPES;
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/SECRETS.md")).unwrap();
    for (name, _) in SHAPES {
        if name.starts_with('<') || name.contains(',') {
            continue; // the named values and the extra shapes are described, not one name
        }
        assert!(doc.contains(&format!("| `{name}` |")), "SECRETS.md does not list {name}");
    }
    for name in ["password", "passwd", "pwd", "passphrase", "secret", "client_secret", "secret_key", "token", "api_key", "apikey", "access_key", "private_key"] {
        assert!(doc.contains(&format!("`{name}`")), "SECRETS.md does not name {name}");
    }
    assert!(doc.contains("secret = PREFIX CLASS MIN"));
}
