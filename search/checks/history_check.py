"""history_check.py SEARCH PACK WORK: the start card and the history tool, end to end on
made-up data: a project's command runs
(one failing twice, then another command passing), a made-up Claude Code session of the
project saved by `sessions sync`, then `history` with no input (the card), with command, session
and file, and `fewer card` (the same card, for a SessionStart hook). Every process gets
a made-up HOME and agents' folders (safe_env.py); WORK is a new scratch folder.
"""
import calendar, importlib.util, json, os, subprocess, sys, time

if len(sys.argv) != 4:
    raise SystemExit(__doc__)
SEARCH, PACK, WORK = (os.path.abspath(a) for a in sys.argv[1:4])
HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)
mcpc.fresh_work(WORK)
safe = safe_env.setup(WORK)
PROJ, HOME = WORK + "/proj", WORK + "/store"
os.makedirs(PROJ)
os.makedirs(HOME)
with open(HOME + "/config", "w") as f:
    f.write("dna = off\nsessions = on\n")
env = safe.env(FEWER_HOME=HOME, FEWER_PROJECT=PROJ, FEWER_TEST_NO_PACK="1", FEWER_PACK_BIN=PACK)
fails = []


def ok(cond, what):
    if not cond:
        fails.append(what)
        print("FAIL: " + what, flush=True)


def run(*argv):
    return subprocess.run([SEARCH, "run", "--"] + list(argv), cwd=PROJ, env=env, capture_output=True).returncode


TEST = "echo 'test_a ... ok'; echo 'FAIL: test_b'; exit 1"
ok(run("sh", "-c", "echo building; exit 0") == 0, "a passing run")
ok(run("sh", "-c", TEST) == 1 and run("sh", "-c", TEST) == 1, "a failing run, twice")
ok(run("sh", "-c", "echo lint ok; exit 0") == 0, "another passing run")

# a made-up Claude Code session of this project: a failing test, a failing build, a failing
# lint, each fixed; three files edited
SID = "5e55a1d0-0000-4000-8000-00000000c0de"
lines = []
n = [0]


def line(typ, content, extra=None):
    n[0] += 1
    o = {"type": typ, "cwd": PROJ, "sessionId": SID, "timestamp": "2026-11-20T14:%02d:00Z" % n[0], "uuid": "u%d" % n[0], "message": {"role": typ, "content": content}}
    o.update(extra or {})
    lines.append(json.dumps(o))


line("user", "the login page forgets the email")
for i, (cmd, bad) in enumerate([("npm test", True), ("npm run build", True), ("npm run build", False), ("npm test", False), ("npm run lint", True), ("npm run lint", False)]):
    line("assistant", [{"type": "tool_use", "id": "b%d" % i, "name": "Bash", "input": {"command": cmd}}])
    r = {"type": "tool_result", "tool_use_id": "b%d" % i, "content": "FAIL" if bad else "ok"}
    if bad:
        r["is_error"] = True
    line("user", [r])
for i, path in enumerate(["src/pages/Login.tsx", "src/pages/Login.test.tsx", "src/pages/Login.tsx"]):
    line("assistant", [{"type": "tool_use", "id": "e%d" % i, "name": "Edit" if i != 1 else "Write", "input": {"file_path": PROJ + "/" + path}}])
    line("user", [{"type": "tool_result", "tool_use_id": "e%d" % i, "content": "updated"}])
claude = env["FEWER_CLAUDE_DIR"]
os.makedirs(claude + "/projects/x", exist_ok=True)
with open(claude + "/projects/x/%s.jsonl" % SID, "w") as f:
    f.write("\n".join(lines) + "\n")
r = subprocess.run([SEARCH, "sessions", "sync"], cwd=PROJ, env=env, capture_output=True)
ok(r.returncode == 0 and b"1 piece saved" in r.stdout, "sessions sync saved the session: %r" % r.stdout[-300:])
ok(run("sh", "-c", "echo 'test_b fixed'; exit 0") == 0, "a passing run after the session")
subprocess.run([SEARCH, "pack-pending", "--wait"], cwd=PROJ, env=env, capture_output=True)

s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
s.initialize()
t, err = s.call("history", {})
print(t)
ok(not err and t.split("\n")[0].startswith("history of ") and "5 of commands, 1 of sessions" in t.split("\n")[0], "the card's first line: %r" % t.split("\n")[0])
ok("failing now (1):\n  sh -c %s: r3 " % TEST in t and "failing since r2; never passed here" in t, "the card names the failing command, since when, and that it never passed")
ok("\nlast runs: r6 " in t, "the card's last runs start with the newest")
def local_minute(utc):
    # (Claude Code writes UTC; fewer shows times in local time, as the check's processes read it)
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(calendar.timegm(time.strptime(utc, "%Y-%m-%dT%H:%M:%SZ"))))


when = "%s to %s" % (local_minute("2026-11-20T14:01:00Z"), local_minute("2026-11-20T14:%02d:00Z" % n[0])[11:])
ok(("last session: claude 5e55a1d0 (r5), %s, " % when) in t and ": 6 commands (3 failed), 2 files edited" in t, "the card's last session (%s): %r" % (when, [l for l in t.split("\n") if "session" in l][:1]))
ok("  failed in it: npm test · npm run build · npm run lint" in t, "the card names the session's failed commands")
ok("  edited: pages/Login.tsx, pages/Login.test.tsx" in t, "the card names the files the session edited")
ok(t.endswith("all shown"), "the card ends with all shown")
t, err = s.call("history", {"command": "test_b"})
ok(not err and t.startswith('3 runs of commands holding "test_b" (the newest first): 2 failed') and t.index("  r6 ") < t.index("  r3 ") < t.index("  r2 "), "history command: %r" % t[:200])
t, err = s.call("history", {"session": "last"})
ok(not err and "commands, in order:\n  failed npm test\n  failed npm run build\n  ran npm run build\n  ran npm test\n  failed npm run lint\n  ran npm run lint\n" in t, "history session last: %r" % t[:400])
t, err = s.call("history", {"session": "5e55"})
ok(not err and t.startswith("session: claude 5e55a1d0 (r5)"), "history session by the start of its id")
t, err = s.call("history", {"session": "ffff"})
ok(err and "no session" in t, "history of an unknown session is an error: %r" % t)
t, err = s.call("history", {"file": "Login.tsx"})
ok(not err and t.startswith("pages/Login.tsx: claude 5e55a1d0 (r5)") and "\n  then: r6 " in t, "history file: %r" % t[:300])
t, err = s.call("history", {"file": "nowhere.rs"})
ok(not err and t.startswith("no session of the newest 1 edited a file holding"), "history of a file no session edited")
code = s.close()
ok(code == 0, "the server exited 0")
r = subprocess.run([SEARCH, "card"], cwd=PROJ, env=env, capture_output=True)
ok(r.returncode == 0 and r.stdout.decode().startswith("history of ") and "failing now (1):" in r.stdout.decode(), "fewer card prints the card")
r = subprocess.run([SEARCH, "card"], cwd=WORK, env=dict(env, FEWER_PROJECT=WORK), capture_output=True)
ok(r.returncode == 0 and r.stdout == b"", "card in a folder with no memory prints nothing: %r" % r.stdout[:200])
print("FAILED: %d" % len(fails) if fails else "ALL PASSED")
sys.exit(1 if fails else 0)
