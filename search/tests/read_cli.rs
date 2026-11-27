//! `fewer read` end to end: the secret shapes of the store's config are hidden in a view as
//! in a run's output. The store lives in the target folder's tmp/
//! (CARGO_TARGET_TMPDIR), never the home folder; the command gets an environment built from
//! nothing, with HOME a folder there.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_fewer");

#[test]
fn the_configs_secret_shapes_are_hidden() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("read-{}-config", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (home, store) = (base.join("home"), base.join("store"));
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&store).unwrap();
    // (a byte that is not UTF-8 before it, a Latin-1 note: the shapes still read, R5-1)
    fs::write(store.join("config"), b"# caf\xe9 notes\nsecret = acme_ alnum 20\n").unwrap();
    let value = "acme_Q7wErTy9uIoP1aSdF2gH";
    let file = base.join("app.env");
    fs::write(&file, format!("NAME=demo\nORDER_REF={value}\n")).unwrap();
    let read = |with_store: bool| {
        let mut c = Command::new(BIN);
        c.env_clear().env("FEWER_DEV", "1").env("HOME", &home).env("PATH", "/usr/bin:/bin").arg("read").arg(&file);
        if with_store {
            c.env("FEWER_HOME", &store);
        }
        let o = c.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    };
    let v = read(true);
    assert!(!v.contains(value) && v.contains("2 ORDER_REF=[hidden: acme_]\n") && v.contains("(2 lines, 1 secret hidden)"), "{v}");
    // without a store, only the built-in shapes (this one is none of them)
    assert!(read(false).contains(value));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn several_files_with_a_line_cut_get_the_outline() {
    // R5-5: several files read at 10,000 characters, a line of one cut in that view: the outline,
    // which names --max, not the cut view
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("read-{}-outline", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("home")).unwrap();
    let (a, b) = (base.join("a.json"), base.join("b.txt"));
    fs::write(&a, format!("{{\"k\":\"{}\"}}\n", "v".repeat(9_900))).unwrap();
    fs::write(&b, "small\n").unwrap();
    let o = Command::new(BIN).env_clear().env("FEWER_DEV", "1").env("HOME", base.join("home")).env("PATH", "/usr/bin:/bin").arg("read").arg(&a).arg(&b).output().unwrap();
    let v = String::from_utf8(o.stdout).unwrap();
    assert!(v.starts_with("2 files,") && v.contains("--max 28000") && !v.contains("shown up to character"), "{v}");
    let _ = fs::remove_dir_all(&base);
}
