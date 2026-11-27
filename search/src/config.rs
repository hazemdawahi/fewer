//! The memory store's settings file, $FEWER_HOME/config: "key = value" lines,
//! '#' comments. It is read as store::read_settings reads it, and changed here one key at a
//! time by the commands the person runs: `install` (Claude Code's chats saved), `on` and `off`
//! (fewer's record), `memory sessions|dna|limit|pause|resume`. The MCP tools never change it.
//!
//! A change keeps every other line as it was, byte for byte (comments, order, line ends), and
//! is written as the store writes every file: to `config.tmp-<process id>`,
//! then renamed, so a reader (which never locks) sees the old file or the new one. Changes are
//! made one at a time, under the store's config lock, $FEWER_HOME/config.lock.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::store;

/// The config file of the store in `home`.
pub fn path(home: &Path) -> PathBuf {
    home.join("config")
}

/// The store's settings, in file order: none when there is no config file or it cannot be
/// read (bytes that are not UTF-8 read as U+FFFD).
pub fn read(home: &Path) -> Vec<(String, String)> {
    fs::read(path(home)).map(|b| store::read_settings(&String::from_utf8_lossy(&b))).unwrap_or_default()
}

/// A setting's value: its first line's, as the store's readers take it.
pub fn get<'a>(settings: &'a [(String, String)], key: &str) -> Option<&'a str> {
    settings.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Set `key = value` in the store's config, for a key that holds one value (not `pause` or
/// `secret`, which take a line each): the key's first line gets the new value, later lines
/// with the same key go, and a key not there yet is added at the end. Every other line stays
/// as it was. The store's folder is made when it is not there yet; a config that is a link
/// is changed where it points, and stays a link. Whether the file changed.
pub fn set(home: &Path, key: &str, value: &str) -> io::Result<bool> {
    change(home, |old| Some((edit(old, key, value), 1))).map(|n| n > 0)
}

/// Add a `key = value` line at the end, for a key that takes a line each (`pause`), unless a
/// line with that key and value is there already. Every other line stays as it was. Whether
/// the file changed.
pub fn add_line(home: &Path, key: &str, value: &str) -> io::Result<bool> {
    change(home, |old| {
        let s = store::read_settings(&String::from_utf8_lossy(old));
        if s.iter().any(|(k, v)| k == key && v == value) {
            return None;
        }
        let mut out = old.to_vec();
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(format!("{key} = {value}\n").as_bytes());
        Some((out, 1))
    })
    .map(|n| n > 0)
}

/// Remove every line of `key` whose value (as store::read_settings reads it) `matches`; every
/// other line stays as it was, byte for byte. How many lines went.
pub fn remove_lines(home: &Path, key: &str, matches: &dyn Fn(&str) -> bool) -> io::Result<usize> {
    change(home, |old| {
        let mut out = Vec::with_capacity(old.len());
        let mut n = 0;
        for l in old.split_inclusive(|&c| c == b'\n') {
            if line_key(l).as_deref() == Some(key) && matches(&line_value(l)) {
                n += 1;
            } else {
                out.extend_from_slice(l);
            }
        }
        (n > 0).then_some((out, n))
    })
}

/// Change the config under the store's config lock: `f` gets the file's bytes (empty when there
/// is none) and gives the new bytes and a count, or None to change nothing. The store's folder
/// is made when it is not there yet; a config that is a link is changed where it points, and
/// stays a link. The count, 0 when the file did not change.
fn change(home: &Path, f: impl FnOnce(&[u8]) -> Option<(Vec<u8>, usize)>) -> io::Result<usize> {
    crate::store::make_home(home)?;
    // (released when `lock` is closed, on every way out)
    let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(home.join("config.lock"))?;
    lock.lock()?;
    store::clear_leftovers(home);
    let mut file = path(home);
    if fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_symlink()) {
        file = fs::canonicalize(&file)?;
    }
    let old = match fs::read(&file) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    match f(old.as_deref().unwrap_or_default()) {
        Some((new, n)) if old.as_deref() != Some(&new[..]) => {
            write_keeping_mode(&file, &new)?;
            Ok(n)
        }
        _ => Ok(0),
    }
}

/// The config text with `key = value` set (see `set`); every other line kept byte for byte,
/// and the changed line keeps its own line end ("\n", "\r\n", or none at the end).
fn edit(text: &[u8], key: &str, value: &str) -> Vec<u8> {
    let line = format!("{key} = {value}");
    let mut out = Vec::with_capacity(text.len() + line.len() + 1);
    let mut done = false;
    for l in text.split_inclusive(|&c| c == b'\n') {
        if line_key(l).as_deref() != Some(key) {
            out.extend_from_slice(l);
        } else if !done {
            let body = l.strip_suffix(b"\n").map_or(l, |b| b.strip_suffix(b"\r").unwrap_or(b));
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(&l[body.len()..]);
            done = true;
        }
    }
    if !done {
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// A config line's key, exactly as store::read_settings reads it: the text before its first
/// '=', trimmed; none for an empty line, a comment or a line without '='.
fn line_key(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let t = text.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim().to_string())
}

/// A config line's value, exactly as store::read_settings reads it: the text after its first
/// '=', trimmed (empty for a line without one).
fn line_value(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    text.trim().split_once('=').map(|(_, v)| v.trim().to_string()).unwrap_or_default()
}

/// Write `bytes` as `path` through a temporary file in the same folder, then a rename. An
/// existing file's permission bits are kept (a config made private stays private); a new file
/// gets 0666 less the umask, as store::write_atomic makes files.
fn write_keeping_mode(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mode = fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o7777);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let r = (|| {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o666).open(&tmp)?;
        f.write_all(bytes)?;
        if let Some(m) = mode {
            f.set_permissions(fs::Permissions::from_mode(m))?;
        }
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for a test, inside this crate's target folder.
    fn tmpdir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp").join(format!("{}-config-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn ed(text: &[u8]) -> Vec<u8> {
        edit(text, "sessions", "on")
    }

    #[test]
    fn edits_one_key_and_keeps_the_rest() {
        // a new file; a key added at the end; the end of a last line without '\n' kept
        assert_eq!(ed(b""), b"sessions = on\n");
        assert_eq!(ed(b"dna = off\n"), b"dna = off\nsessions = on\n");
        assert_eq!(ed(b"dna = off"), b"dna = off\nsessions = on\n");
        // replaced in place: comments, order, blank lines and other keys kept
        let t = b"# my settings\n\ndna = off\nsessions = off\npause = /a\npause = /b\n";
        assert_eq!(ed(t), b"# my settings\n\ndna = off\nsessions = on\npause = /a\npause = /b\n");
        // later lines with the key go; the first keeps its place
        assert_eq!(ed(b"sessions = off\nx = 1\n  sessions=maybe\n"), b"sessions = on\nx = 1\n");
        // keys are whole: sessions.claude is another key; a comment is not a line of the key
        assert_eq!(ed(b"sessions.claude = off\n# sessions = on\n"), b"sessions.claude = off\n# sessions = on\nsessions = on\n");
        // line ends kept: "\r\n" stays "\r\n", a last line without '\n' stays without
        assert_eq!(ed(b"a = 1\r\nsessions = off\r\nb = 2"), b"a = 1\r\nsessions = on\r\nb = 2");
        assert_eq!(ed(b"a = 1\nsessions=off"), b"a = 1\nsessions = on");
        // bytes that are not UTF-8 kept as they are
        assert_eq!(ed(b"pause = /tmp/\xff\xfe\nsessions = off\n"), b"pause = /tmp/\xff\xfe\nsessions = on\n");
        // spaces and tabs around the key, and '=' inside other values
        assert_eq!(ed(b"\t sessions \t= off\nsecret = AB=C alnum 8\n"), b"sessions = on\nsecret = AB=C alnum 8\n");
        // nothing to change: the same bytes
        assert_eq!(ed(b"# x\nsessions = on\n"), b"# x\nsessions = on\n");
    }

    #[test]
    fn edits_agree_with_the_reader() {
        // random configs made of tricky lines: after the edit, store::read_settings gives the
        // new value for the key, and every other setting as before, in the same order
        let parts: [&[u8]; 16] = [
            b"sessions = off",
            b"  sessions=on  ",
            b"sessions",
            b"# sessions = off",
            b"sessions.claude = off",
            b"session = on",
            b"\xc2\xa0sessions = off",
            b"dna = on",
            b"pause = /a b/c",
            b"secret = sk_ alnum 16",
            b"",
            b"   ",
            b"=x",
            b"x = y = z",
            b"\xff\xfe = bytes",
            b"Sessions = on",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..3000 {
            let mut t = Vec::new();
            for _ in 0..next(7) {
                t.extend_from_slice(parts[next(parts.len())]);
                t.extend_from_slice([&b"\n"[..], b"\r\n", b"\n"][next(3)]);
            }
            if next(4) == 0 && !t.is_empty() {
                t.pop(); // a last line without its '\n'
            }
            let value = ["on", "off"][next(2)];
            let before = store::read_settings(&String::from_utf8_lossy(&t));
            let after_text = edit(&t, "sessions", value);
            let after = store::read_settings(&String::from_utf8_lossy(&after_text));
            assert_eq!(get(&after, "sessions"), Some(value), "{:?}", String::from_utf8_lossy(&t));
            assert_eq!(after.iter().filter(|(k, _)| k == "sessions").count(), 1);
            let others = |s: &[(String, String)]| s.iter().filter(|(k, _)| k != "sessions").cloned().collect::<Vec<_>>();
            assert_eq!(others(&after), others(&before), "{:?}", String::from_utf8_lossy(&t));
            // and setting it again changes nothing
            assert_eq!(edit(&after_text, "sessions", value), after_text);
        }
    }

    #[test]
    fn sets_on_disk() {
        let base = tmpdir("disk");
        let home = base.join("store");
        // a store that is not there yet: made, with the config and its lock
        assert!(set(&home, "sessions", "on").unwrap());
        assert_eq!(fs::read(path(&home)).unwrap(), b"sessions = on\n");
        assert!(home.join("config.lock").exists());
        assert_eq!(get(&read(&home), "sessions"), Some("on"));
        // the same again: nothing written
        let t0 = fs::metadata(path(&home)).unwrap().modified().unwrap();
        assert!(!set(&home, "sessions", "on").unwrap());
        assert_eq!(fs::metadata(path(&home)).unwrap().modified().unwrap(), t0);
        // a private config stays private; other lines stay
        fs::write(path(&home), b"# mine\nsessions = off\ndna = on\n").unwrap();
        fs::set_permissions(path(&home), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(set(&home, "sessions", "on").unwrap());
        assert_eq!(fs::read(path(&home)).unwrap(), b"# mine\nsessions = on\ndna = on\n");
        assert_eq!(fs::metadata(path(&home)).unwrap().permissions().mode() & 0o777, 0o600);
        // no temporary file is left; a leftover of a writer that is gone is cleared
        fs::write(home.join("config.tmp-999999999"), b"x").unwrap();
        assert!(set(&home, "dna", "off").unwrap());
        let mut names: Vec<String> = fs::read_dir(&home).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["config", "config.lock"]);
        // a config that is a link: changed where it points, and still a link
        let real = base.join("dotfiles-config");
        fs::write(&real, b"sessions = off\n").unwrap();
        fs::remove_file(path(&home)).unwrap();
        std::os::unix::fs::symlink(&real, path(&home)).unwrap();
        assert!(set(&home, "sessions", "on").unwrap());
        assert!(fs::symlink_metadata(path(&home)).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"sessions = on\n");
        // no config: no settings
        assert!(read(&base.join("none")).is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn lines_added_and_removed() {
        let base = tmpdir("lines");
        let home = base.join("store");
        // a key that takes a line each: added at the end, once
        assert!(add_line(&home, "pause", "/w/a").unwrap());
        assert!(!add_line(&home, "pause", "/w/a").unwrap());
        fs::write(path(&home), b"# mine\r\npause = /w/a\r\ndna = off\npause=/w/b").unwrap();
        assert!(add_line(&home, "pause", "/w/c").unwrap());
        assert_eq!(fs::read(path(&home)).unwrap(), b"# mine\r\npause = /w/a\r\ndna = off\npause=/w/b\npause = /w/c\n");
        // removed by value; other lines kept byte for byte, "\r\n" included
        assert_eq!(remove_lines(&home, "pause", &|v| v == "/w/b").unwrap(), 1);
        assert_eq!(fs::read(path(&home)).unwrap(), b"# mine\r\npause = /w/a\r\ndna = off\npause = /w/c\n");
        assert_eq!(remove_lines(&home, "pause", &|v| v == "/nothing").unwrap(), 0);
        assert_eq!(remove_lines(&home, "pause", &|_| true).unwrap(), 2);
        assert_eq!(fs::read(path(&home)).unwrap(), b"# mine\r\ndna = off\n");
        // the readers agree
        assert_eq!(read(&home), vec![("dna".to_string(), "off".to_string())]);
        fs::remove_dir_all(&base).unwrap();
    }
}
