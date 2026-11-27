"""perms_check.py: fewer's own commands keep Claude Code's permission rules, so that permissions
work as usual with fewer on.

With fewer on Claude reads with `fewer read`, `fewer table` and `fewer look`, and runs tests and
builds with `fewer run`, all through Bash: Claude Code's Read rules never see those files, and its
Bash rules see `fewer run ...`, not the command inside. This checks, with the program itself, in a
made-up project whose .claude/settings.json and settings.local.json hold the rules (and an
organization's managed settings, made up too), that:
- a file a Read deny rule names is not read (read, table, look), and its line names the rule;
- one a Read ask rule names is not read either, and the line says to use Read, which asks;
- the other files of the same call are read as usual (read), and the exit status is 1;
- a command a Bash deny rule matches is not run by `fewer run` (exit 126, nothing run), nor one
  an ask rule matches; a wrapper (env, timeout) or a shell's -c script does not hide it;
- a command no rule names runs as before;
- a rule in the managed settings counts too; settings that are not JSON are passed over;
- in a Claude Code session (CLAUDECODE set: fewer's install lets fewer's reading commands run
  without asking), a file outside the project (a git repository's root, from a folder below too)
  is not read, nor one a link in the project points to outside it, and the line says to use
  Read, which asks; a folder `permissions.additionalDirectories` adds and a file a Read allow
  rule names are read; outside a session, as before;
- `fewer changes` leaves out a changed file a Read deny rule names, and says so; a commit of only
  that file shows its header alone.

  python3 -I perms_check.py FEWER WORK
"""
import importlib.util, json, os, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
FAILS, PASSES = [], [0]


def ok(cond, what):
    if cond:
        PASSES[0] += 1
    else:
        FAILS.append(what)
        print("FAIL: " + what, flush=True)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    fewer, work = (os.path.abspath(a) for a in sys.argv[1:3])
    if os.path.exists(work):
        shutil.rmtree(work)
    os.makedirs(work)
    spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
    safe_env = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(safe_env)
    safe = safe_env.setup(work)
    proj = os.path.join(work, "proj")
    sub = os.path.join(proj, "src")
    os.makedirs(sub)
    os.makedirs(os.path.join(proj, ".git"))
    os.makedirs(os.path.join(proj, ".claude"))
    files = {"main.py": "print('hello')\n", ".env": "API_KEY=sk-test-123\n", "notes.md": "private notes\n", "data.csv": "a,b\n1,2\n",
             "secret.csv": "k,v\nx,1\n", "src/app.py": "x = 1\n"}
    for k, v in files.items():
        open(os.path.join(proj, k), "w").write(v)
    json.dump({"permissions": {"deny": ["Read(./.env)", "Read(*.csv)", "Bash(rm:*)"], "allow": ["Bash(fewer:*)"]}}, open(os.path.join(proj, ".claude", "settings.json"), "w"))
    json.dump({"permissions": {"ask": ["Read(./notes.md)", "Bash(git push:*)"]}}, open(os.path.join(proj, ".claude", "settings.local.json"), "w"))
    managed = safe.env()["FEWER_MANAGED_DIR"]
    os.makedirs(managed, exist_ok=True)
    json.dump({"permissions": {"deny": ["Bash(curl:*)"]}}, open(os.path.join(managed, "managed-settings.json"), "w"))
    os.makedirs(os.path.join(managed, "managed-settings.d"), exist_ok=True)
    open(os.path.join(managed, "managed-settings.d", "50-broken.json"), "w").write("not json")
    store = os.path.join(work, "store")

    def run(args, cwd=proj):
        r = subprocess.run([fewer] + args, cwd=cwd, env=safe.env(FEWER_HOME=store), stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
        return r.returncode, r.stdout, r.stderr

    # read: denied and asked files out, each said, the rest read
    code, out, err = run(["read", "main.py", ".env", "notes.md"])
    ok(code == 1 and "== .env: not read: your permission rule Read(./.env) (in " in out and "denies reading it" in out, "read: the denied file said: %r %r" % (code, out))
    ok("== notes.md: not read: your permission rule Read(./notes.md) (in " in out and "read it with Claude Code's Read tool, which asks" in out, "read: the asked file said: %r" % out)
    ok("print('hello')" in out and "sk-test-123" not in out and "private notes" not in out, "read: main.py read, the others not: %r" % out)
    code, out, err = run(["read", ".env"])
    ok(code == 1 and "sk-test-123" not in out and "not read" in out, "read: only a denied file: %r %r" % (code, out))
    # (from a folder below, the project's rules still count; a pattern with no slash at any depth)
    code, out, err = run(["read", "../.env", "app.py"], cwd=sub)
    ok(code == 1 and "not read" in out and "x = 1" in out, "read: from a folder below: %r" % out)
    code, out, err = run(["read", "main.py"])
    ok(code == 0 and "not read" not in out, "read: a file no rule names: %r %r" % (code, out))
    # table: a CSV the rules deny at any depth
    code, out, err = run(["table", "data.csv"])
    ok(code == 1 and "== data.csv: not read: your permission rule Read(*.csv)" in out and "1" not in out.split("not read")[0], "table: denied: %r" % out)
    # look: refused before anything is sent to a model
    code, out, err = run(["look", ".env", "what is in it?"])
    ok(code == 1 and ".env: not read: your permission rule Read(./.env)" in out, "look: denied: %r %r" % (code, out))
    # run: denied and asked commands not run; others run
    marker = os.path.join(proj, "marker")
    open(marker, "w").write("x")
    code, out, err = run(["run", "--", "rm", marker])
    ok(code == 126 and os.path.exists(marker) and "fewer run: not run: your permission rule Bash(rm:*)" in err and "denies this command" in err, "run: rm denied: %r %r" % (code, err))
    code, out, err = run(["run", "--short", "--", "env", "A=1", "rm", marker])
    ok(code == 126 and os.path.exists(marker), "run: rm behind env denied: %r" % err)
    code, out, err = run(["run", "--", "timeout", "5", "rm", marker])
    ok(code == 126 and os.path.exists(marker), "run: rm behind timeout denied: %r" % err)
    code, out, err = run(["run", "--", "sh", "-c", "echo hi && rm " + marker])
    ok(code == 126 and os.path.exists(marker), "run: rm in a shell's script denied: %r" % err)
    code, out, err = run(["run", "--", "git", "push", "origin", "main"])
    ok(code == 126 and "says Claude Code asks you before this command; run it without fewer run" in err, "run: git push asked: %r" % err)
    code, out, err = run(["run", "--", "curl", "http://example.invalid"])
    ok(code == 126 and "Bash(curl:*)" in err and "managed-settings.json" in err, "run: the managed settings' deny: %r" % err)
    code, out, err = run(["run", "--", "echo", "fine"])
    ok(code == 0 and "fine" in out, "run: a command no rule names runs: %r %r" % (code, out + err))
    code, out, err = run(["run", "--", "rmdir", "--help"])
    ok("not run" not in err, "run: rmdir is not rm: %r" % err)
    bounds(fewer, work, safe, store)
    print("all passed (%d checks)" % PASSES[0] if not FAILS else "%d FAILED of %d" % (len(FAILS), PASSES[0] + len(FAILS)))
    return 1 if FAILS else 0


def bounds(fewer, work, safe, store):
    """The project's bounds in a Claude Code session, and `fewer changes` with a Read deny rule."""
    proj = os.path.join(work, "bproj")
    os.makedirs(os.path.join(proj, "src"))
    os.makedirs(os.path.join(proj, ".claude"))
    for d in ("shared", "docs", "else"):
        os.makedirs(os.path.join(work, d))
    put = lambda path, text: open(path, "w").write(text)
    put(os.path.join(proj, "main.py"), "print('inside')\n")
    put(os.path.join(proj, ".env"), "API_KEY=sk-test-456\n")
    put(os.path.join(work, "else", "far.txt"), "far away text\n")
    put(os.path.join(work, "else", "far.csv"), "k,v\nfar,1\n")
    put(os.path.join(work, "shared", "a.txt"), "shared text\n")
    put(os.path.join(work, "docs", "d.md"), "docs text\n")
    os.symlink(os.path.join(work, "else", "far.txt"), os.path.join(proj, "link.txt"))
    json.dump({"permissions": {"deny": ["Read(./.env)"], "additionalDirectories": ["../shared"], "allow": ["Read(/%s/docs/**)" % work]}}, open(os.path.join(proj, ".claude", "settings.json"), "w"))
    env = safe.env(FEWER_HOME=store)
    git = lambda *a: subprocess.run(["git", "-c", "user.email=t@example.invalid", "-c", "user.name=t", "-C", proj] + list(a), env=env, capture_output=True, text=True, timeout=60)
    ok(git("init", "-q").returncode == 0, "bounds: git init")

    def run(args, cwd=proj, claude=True):
        e = safe.env(FEWER_HOME=store, CLAUDECODE="1") if claude else env
        r = subprocess.run([fewer] + args, cwd=cwd, env=e, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
        return r.returncode, r.stdout, r.stderr

    far = os.path.join(work, "else", "far.txt")
    code, out, err = run(["read", "main.py", far])
    ok(code == 1 and "print('inside')" in out and "far away text" not in out and ": not read: it is outside this project (" in out
       and "read it with Claude Code's Read tool, which asks you first" in out, "bounds: a file outside the project: %r %r" % (code, out))
    code, out, err = run(["read", "../main.py"], cwd=os.path.join(proj, "src"))
    ok(code == 0 and "print('inside')" in out, "bounds: from a folder below, the repository's root counts: %r %r" % (code, out + err))
    code, out, err = run(["read", "link.txt"])
    ok(code == 1 and "far away text" not in out and "outside this project" in out, "bounds: a link to a file outside: %r" % out)
    code, out, err = run(["read", os.path.join(work, "shared", "a.txt"), os.path.join(work, "docs", "d.md")])
    ok(code == 0 and "shared text" in out and "docs text" in out, "bounds: an added folder and a Read allow rule: %r %r" % (code, out))
    code, out, err = run(["table", os.path.join(work, "else", "far.csv")])
    ok(code == 1 and "outside this project" in out and "far,1" not in out, "bounds: table outside: %r" % out)
    code, out, err = run(["read", far], claude=False)
    ok(code == 0 and "far away text" in out, "bounds: outside a Claude Code session, as before: %r %r" % (code, out))
    # changes: the denied file left out, said
    put(os.path.join(proj, "main.py"), "print('inside')\nprint('two')\n")
    put(os.path.join(proj, ".env"), "API_KEY=sk-test-789\n")
    ok(git("add", "-A").returncode == 0 and git("commit", "-qm", "both").returncode == 0, "bounds: a commit")
    code, out, err = run(["changes"])
    ok(code == 0 and "both" in out and "print('two')" in out and "sk-test-789" not in out and "sk-test-456" not in out
       and ".env: not shown: your permission rule Read(./.env) (in " in out, "changes: the denied file left out: %r %r" % (code, out + err))
    put(os.path.join(proj, ".env"), "API_KEY=sk-test-000\n")
    ok(git("commit", "-qam", "env only").returncode == 0, "bounds: a second commit")
    code, out, err = run(["changes"])
    ok(code == 0 and "env only" in out and "sk-test" not in out and ".env: not shown" in out, "changes: a commit of the denied file alone: %r %r" % (code, out + err))


if __name__ == "__main__":
    sys.exit(main())
