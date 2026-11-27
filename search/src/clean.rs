//! How a line is shown in answers. The search always matches the exact
//! bytes; only the shown text is cleaned, and an answer says so whenever a shown line
//! differs from its bytes (`lines` with `raw` or `base64` gives them exactly).
//!
//! Shown text:
//! - the line end ('\n', and a '\r' before it) is dropped;
//! - color and cursor codes are set aside: ESC '[' up to a final byte 0x40-0x7E; ESC ']' up
//!   to BEL or ESC '\'; any other escape: ESC, its intermediate bytes 0x20-0x2F and one final
//!   byte (ESC ( B, the charset switch `tput sgr0` writes, goes whole);
//! - a line redrawn with '\r' (a progress bar) is shown at its final state: the last piece
//!   after a '\r' that is not empty;
//! - control bytes other than tab, and bytes that are not UTF-8, are shown as \xHH.

const ESC: u8 = 0x1b;

/// The line without its line end ('\n', and a '\r' before it).
pub fn without_end(line: &[u8]) -> &[u8] {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    l.strip_suffix(b"\r").unwrap_or(l)
}

/// The bytes with escape sequences set aside (and whether there were any).
pub fn strip_escapes(b: &[u8]) -> (Vec<u8>, bool) {
    if !b.contains(&ESC) {
        return (b.to_vec(), false);
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != ESC {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b.get(i) {
            Some(b'[') => {
                i += 1;
                while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                i += 1;
                while i < b.len() {
                    if b[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if b[i] == ESC && b.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(_) => {
                while i < b.len() && (0x20..=0x2f).contains(&b[i]) {
                    i += 1;
                }
                i += 1;
            }
            None => {}
        }
    }
    (out, true)
}

/// A line as answers show it, and whether it differs from the line's bytes (other than the
/// dropped line end).
pub fn shown(line: &[u8]) -> (String, bool) {
    let (b, mut changed) = strip_escapes(without_end(line));
    // a redraw's final state: the last piece after a '\r' that is not empty
    let mut piece: &[u8] = &b;
    if b.contains(&b'\r') {
        changed = true;
        piece = b.split(|&c| c == b'\r').rev().find(|p| !p.is_empty()).unwrap_or(&[]);
    }
    let (s, esc) = escape(piece, false);
    (s, changed || esc)
}

/// Bytes as text: UTF-8 kept, control bytes other than tab and bytes that are not UTF-8
/// written as \xHH (with `raw`: every control byte, tab and the line ends too, and the
/// backslash itself as \\, so the text gives back the exact bytes). Also whether anything
/// was written as an escape.
pub fn escape(b: &[u8], raw: bool) -> (String, bool) {
    let mut out = String::with_capacity(b.len());
    let mut esc = false;
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' if raw => out.push_str("\\\\"),
                '\t' if raw => out.push_str("\\t"),
                '\n' if raw => out.push_str("\\n"),
                '\r' if raw => out.push_str("\\r"),
                '\t' => out.push('\t'),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    out.push_str(&format!("\\x{:02x}", c as u32));
                    esc = true;
                }
                c => out.push(c),
            }
        }
        for &x in chunk.invalid() {
            out.push_str(&format!("\\x{x:02x}"));
            esc = true;
        }
    }
    (out, esc)
}

/// Text cut to at most `max` bytes (at a character boundary), and how many bytes were cut.
pub fn cut(s: &str, max: usize) -> (&str, usize) {
    if s.len() <= max {
        return (s, 0);
    }
    let mut k = max;
    while !s.is_char_boundary(k) {
        k -= 1;
    }
    (&s[..k], s.len() - k)
}

// (the question words and cut shared by `web`, `skill` and the page reader)

/// The text cut at `max` bytes (0: never), on a line end when one is in the last half: the
/// part printed, and whether it was cut.
pub fn cut_at(text: &str, max: usize) -> (&str, bool) {
    if max == 0 || text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = text[..end].rfind('\n')
        && nl >= end / 2
    {
        end = nl;
    }
    (&text[..end], true)
}

/// Small words a question's words leave out.
pub(crate) const STOP: &[&str] = &["the", "and", "for", "what", "which", "how", "does", "with", "from", "that", "this", "into", "are", "was", "can", "you", "your", "use", "using", "give", "page", "add-on", "find", "tell", "about", "its", "per", "via", "a", "an", "of", "in", "on", "to", "is", "be", "by", "or", "it", "do", "i", "me", "my"];

/// A question's words that count: lowercase letters and digits, the small ones left out.
pub(crate) fn words_of(q: &str) -> Vec<String> {
    let mut v: Vec<String> = q.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty() && !STOP.contains(w) && (w.len() > 1 || w.chars().all(|c| c.is_ascii_digit()))).map(str::to_string).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_lines() {
        assert_eq!(shown(b"plain line\n"), ("plain line".into(), false));
        assert_eq!(shown(b"crlf line\r\n"), ("crlf line".into(), false));
        assert_eq!(shown(b"\x1b[31mERROR\x1b[0m boom"), ("ERROR boom".into(), true));
        assert_eq!(shown(b"\x1b]0;title\x07after"), ("after".into(), true));
        assert_eq!(shown(b"\x1b]8;;http://x\x1b\\link"), ("link".into(), true));
        assert_eq!(shown(b"a\x1b(Bb"), ("ab".into(), true));
        assert_eq!(shown(b"cut\x1b[12"), ("cut".into(), true));
        assert_eq!(shown(b"10%\r50%\r100%\n"), ("100%".into(), true));
        assert_eq!(shown(b"done\r\r\n"), ("done".into(), true));
        assert_eq!(shown(b"tab\there"), ("tab\there".into(), false));
        assert_eq!(shown(b"nul\x00x\xffy"), ("nul\\x00x\\xffy".into(), true));
        assert_eq!(shown("é€😀".as_bytes()), ("é€😀".into(), false));
        assert_eq!(escape(b"a\\b\tc\r\n\x1b", true).0, "a\\\\b\\tc\\r\\n\\x1b");
        assert_eq!(cut("héllo", 2), ("h", 5));
        assert_eq!(cut("abc", 3), ("abc", 0));
    }
}
