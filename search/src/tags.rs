//! How a saved run is tagged with git: the branch, the commit and whether the
//! work tree is dirty, read before the command starts.
//!
//! - commit: `git -C <cwd> rev-parse --show-toplevel HEAD` (the root and the commit);
//! - branch: `git -C <cwd> rev-parse --abbrev-ref HEAD` ("HEAD" when detached), a call of its
//!   own because --abbrev-ref would also shorten any later argument;
//! - dirty: `git -C <cwd> status --porcelain --untracked-files=no` prints anything.
//!
//! Each with GIT_OPTIONAL_LOCKS=0, no input and a 2 s limit (store::run_quiet), the three at
//! once on their own threads. Outside a repository, or before the first commit, the missing
//! tags are left out: no commit means no branch and no dirty either (dirty says the work tree
//! differs from that commit).

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use crate::store;

/// A run's git tags; None: left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Git {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: Option<bool>,
}

const LIMIT: Duration = Duration::from_secs(2);

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut a: Vec<&OsStr> = vec![OsStr::new("-C"), cwd.as_os_str()];
    a.extend(args.iter().map(OsStr::new));
    store::run_quiet("git", &a, LIMIT).map(|o| String::from_utf8_lossy(&o).into_owned())
}

/// The commit of `rev-parse --show-toplevel HEAD`'s output: its last line, when it is a
/// commit id (40 hex digits, or 64 in a SHA-256 repository).
fn commit_of(out: &str) -> Option<String> {
    let c = out.lines().last()?.trim();
    ((c.len() == 40 || c.len() == 64) && c.bytes().all(|b| b.is_ascii_hexdigit())).then(|| c.to_string())
}

/// The tags of the folder `cwd`.
pub fn read(cwd: &Path) -> Git {
    let (commit, branch, status) = std::thread::scope(|s| {
        let c = s.spawn(|| git(cwd, &["rev-parse", "--show-toplevel", "HEAD"]));
        let b = s.spawn(|| git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]));
        let d = s.spawn(|| git(cwd, &["status", "--porcelain", "--untracked-files=no"]));
        (c.join().ok().flatten(), b.join().ok().flatten(), d.join().ok().flatten())
    });
    let Some(commit) = commit.as_deref().and_then(commit_of) else { return Git::default() };
    let branch = branch.map(|b| b.trim_end_matches(['\n', '\r']).to_string()).filter(|b| !b.is_empty() && !b.contains('\n'));
    Git { branch, commit: Some(commit), dirty: status.map(|s| !s.is_empty()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// git in a test repository, without the developer's own git settings.
    fn g(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn commit_lines() {
        let h = "a".repeat(40);
        assert_eq!(commit_of(&format!("/x/repo\n{h}\n")), Some(h.clone()));
        assert_eq!(commit_of(&format!("{h}\n")), Some(h));
        assert_eq!(commit_of("/x/repo\nHEAD\n"), None);
        assert_eq!(commit_of(""), None);
    }

    /// In a temporary repository: before the first commit nothing; then branch, commit and
    /// dirty; detached; outside a repository nothing.
    #[test]
    fn tags_in_a_temporary_repository() {
        // (`g` makes the repository without the developer's git settings; the tag calls read
        // it with the test's environment, as `run` would)
        let d = crate::capture::tests::tmpdir("git");
        let repo = d.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let outside = d.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        g(&repo, &["init", "-q"]);
        let first = read(&repo);
        assert_eq!(first, Git::default(), "before the first commit");
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        g(&repo, &["add", "a.txt"]);
        g(&repo, &["commit", "-q", "-m", "first"]);
        let head = g(&repo, &["rev-parse", "HEAD"]).trim().to_string();
        let t = read(&repo);
        assert_eq!(t, Git { branch: Some("main".into()), commit: Some(head.clone()), dirty: Some(false) });
        // an untracked file is not dirt; a changed tracked file is
        std::fs::write(repo.join("new.txt"), "x\n").unwrap();
        assert_eq!(read(&repo).dirty, Some(false));
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        assert_eq!(read(&repo).dirty, Some(true));
        // a subfolder of the repository gets the same tags
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        assert_eq!(read(&repo.join("sub")).commit, Some(head.clone()));
        // detached
        g(&repo, &["checkout", "-q", "--detach"]);
        assert_eq!(read(&repo).branch.as_deref(), Some("HEAD"));
        // outside a repository (the test folders are not inside one; checked first)
        let top = Command::new("git").arg("-C").arg(&outside).args(["rev-parse", "--show-toplevel"]).stderr(Stdio::null()).output().unwrap();
        if top.status.success() {
            eprintln!("skipped the outside check: {} is inside a repository", outside.display());
        } else {
            assert_eq!(read(&outside), Git::default());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }
}
