"""day.py SEARCH PACK WORK DATA: a scripted day of saving and asking (python3 -I).

SEARCH is the fewer program, PACK the fewer-pack program that packs, WORK a scratch folder
(made afresh), DATA a folder of copies of the Loghub *_2k logs. A made-up developer's day, in a
store of its own: two logs added in the morning; a build run twice through `run` (passing, then
failing with an error on stderr) and packed; a made-up Claude Code session saved by `sessions
sync`; one log grown and added again (only its new part is saved); one run forgotten; saving
paused for a command, then resumed. Then one MCP session asks every tool about the day, and
every count in the answers equals a Python count over each run's exact text (`fewer-pack -dc` of its
archive): per run, with and without a text, with and without -i (lines, as grep counts them),
the totals in the headings, and the lines `follow` gives through its cursors. The forgotten run
is in no answer and `lines` on it says it was forgotten; the paused command's output is nowhere;
the failing build is in `errors`, `compare` and `guard`; `memory` shows the day's runs.
"""
import importlib.util, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

if len(sys.argv) != 5:
    raise SystemExit(__doc__)
SEARCH, PACK, WORK, DATA = (os.path.abspath(a) for a in sys.argv[1:5])
mcpc.fresh_work(WORK)
safe = safe_env.setup(WORK)
HOME, PROJ, CLAUDE = WORK + "/home", WORK + "/proj", WORK + "/claude"
os.makedirs(PROJ + "/logs")
os.makedirs(HOME)
with open(HOME + "/config", "w") as f:
    f.write("sessions = on\ndna = off\n")
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ, FEWER_CLAUDE_DIR=CLAUDE, GIT_CEILING_DIRECTORIES=WORK)
fails = []
stats = {"checks": 0}


def ok(cond, what):
    stats["checks"] += 1
    if not cond:
        fails.append(what)
        print("FAILED:", what, flush=True)
    return cond


def sh(args, **more):
    r = subprocess.run([SEARCH] + args, env=dict(env, **more), cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
    return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")


# ---------------------------------------------------------------- the day

print("1. the day", flush=True)
for n in ("HDFS_2k.log", "Zookeeper_2k.log"):
    shutil.copy(os.path.join(DATA, n), PROJ + "/logs/" + n)
code, out, err = sh(["add", "logs/HDFS_2k.log", "logs/Zookeeper_2k.log"])
ok(code == 0, "morning add: %s" % err)
BUILD = "echo 'INFO compiling 12 files'; echo 'INFO tests: 40 passed'; [ \"$FAIL\" = 1 ] && { echo 'ERROR test login_timeout failed: expected 200, got 500' >&2; exit 1; }; exit 0"
for fail in ("0", "1"):
    code, out, err = sh(["run", "--", "sh", "-c", BUILD], FEWER_TEST_NO_PACK="1", FAIL=fail)
    ok(code == int(fail), "build run (FAIL=%s): exit %d" % (fail, code))
code, out, err = sh(["pack-pending", "--wait"])
ok(code == 0, "pack-pending: %s" % err)
SID = "0e0e0e0e-1111-4222-8333-444444444444"
os.makedirs(CLAUDE + "/projects/-proj")
with open(CLAUDE + "/projects/-proj/%s.jsonl" % SID, "w") as f:
    for i, (who, text) in enumerate((("user", "the login_timeout test fails in CI"), ("assistant", "the handler returns 500 when the session store is slow"), ("user", "raise the store's timeout"))):
        f.write(json.dumps({"cwd": PROJ, "sessionId": SID, "type": who, "message": {"role": who, "content": text}, "timestamp": "2026-11-18T1%d:00:00.000Z" % i}) + "\n")
code, out, err = sh(["sessions", "sync"])
ok(code == 0 and "1 piece saved" in out, "sessions sync: %s %s" % (out, err))
with open(PROJ + "/logs/Zookeeper_2k.log", "ab") as f:
    for i in range(50):
        f.write(b"2015-07-29 19:%02d:00,000 - INFO  [day.py:1] - grown line %d of the afternoon\n" % (i % 60, i))
code, out, err = sh(["add", "logs/Zookeeper_2k.log"])
ok(code == 0 and "its new part, after r" in out, "the grown log: only its new part: %s %s" % (out, err))
pid = os.listdir(HOME + "/projects")[0]


def runs():
    r = {}
    for line in open(os.path.join(HOME, "projects", pid, "runs.jsonl"), "rb").read().split(b"\n"):
        if line.strip():
            x = json.loads(line)
            r.setdefault(x["id"], {}).update(x)
    return r


hdfs = [i for i, x in runs().items() if x.get("source", "").endswith("HDFS_2k.log")][0]
code, out, err = sh(["memory", "forget", "--yes", "%s/r%d" % (pid, hdfs)])
ok(code == 0 and "Forgot 1 run" in out, "forget the HDFS run: %s %s" % (out, err))
code, out, err = sh(["memory", "pause"])
code, out, err = sh(["run", "--", "sh", "-c", "echo 'PAUSED-OUTPUT-NOT-SAVED'"], FEWER_TEST_NO_PACK="1")
ok(code == 0 and "PAUSED-OUTPUT-NOT-SAVED" in out, "the paused run still ran")
code, out, err = sh(["memory", "resume"])
live = {i: x for i, x in runs().items() if x.get("state") != "forgotten"}
ok(all(x.get("state") == "ready" for x in live.values()) and len(live) == 5, "the day's runs: %s" % [(i, x.get("kind"), x.get("state")) for i, x in sorted(live.items())])
builds = sorted(i for i, x in live.items() if x.get("kind") == "run")

# the exact text of every run, from its archive
TEXT = {}
for i in live:
    r = subprocess.run([PACK, "-dc", os.path.join(HOME, "projects", pid, "r%06d.cmf" % i)], env=env, stdin=subprocess.DEVNULL, capture_output=True)
    ok(r.returncode == 0, "fewer-pack -dc r%d" % i)
    TEXT[i] = r.stdout
ok(all(b"PAUSED-OUTPUT-NOT-SAVED" not in t for t in TEXT.values()), "the paused command's output is in no run")


def lines_of(i):
    t = TEXT[i]
    return t[:-1].split(b"\n") if t.endswith(b"\n") else (t.split(b"\n") if t else [])


def holds(i, q, icase=False):
    q = q.encode()
    if icase:
        q = q.lower()
        return [n + 1 for n, l in enumerate(lines_of(i)) if q in l.lower()]
    return [n + 1 for n, l in enumerate(lines_of(i)) if q in l]


# ---------------------------------------------------------------- every tool

print("2. every tool, and every count against Python's", flush=True)
s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
s.initialize()
ROW = re.compile(r"^(?:(\S+)/)?r(\d+) ([\d,]+)  ", re.M)
NEXT = re.compile(r"\b([a-z]+) (\{.*\})\s*$")
REF = re.compile(r"^(?:(\S+)/)?r(\d+):(\d+) ", re.M)


def all_text(name, args):
    """Every answer of a call and the next calls its answers give."""
    texts = []
    for _ in range(200):
        text, err = s.call(name, args)
        ok(not err or name == "lines", "%s %s: an error answer: %s" % (name, args, text[:200]))
        texts.append(text)
        last = text.rstrip("\n").split("\n")[-1]
        m = NEXT.search(last) if last.startswith("left out") else None
        if not m:
            return texts
        name, args = m.group(1), json.loads(m.group(2))
    ok(False, "%s: too many next calls" % name)
    return texts


text, _ = s.call("logs", {"kind": "all", "max_tokens": 20000})
shown = {int(m.group(1)) for m in re.finditer(r"^(?:%s/)?r(\d+) " % re.escape(pid), text, re.M)}
ok(shown == set(live), "logs lists the day's runs: %s, want %s" % (sorted(shown), sorted(live)))
for q, icase in (("INFO", False), ("info", True), ("ERROR", False), ("login_timeout", False), ("passed", False), ("grown line", False), ("blk_", False), ("session", True), ("a", False), ("no line holds this", False)):
    want = {i: len(holds(i, q, icase)) for i in live}
    want = {i: n for i, n in want.items() if n}
    args = {"text": q, "kind": "all", "max_tokens": 20000}
    if icase:
        args["icase"] = True
    text, err = s.call("count", args)
    got = {int(m.group(2)): int(m.group(3).replace(",", "")) for m in ROW.finditer(text)}
    ok(not err and got == want, "count %r%s: got %s, want %s" % (q, " -i" if icase else "", got, want))
    total = sum(want.values())
    head = text.split("\n")[0]
    ok(head.startswith("{:,} line".format(total)) if total else (head.startswith("no line holds") or head.startswith("0 lines")), "count %r: heading %r, want %d lines" % (q, head, total))
    stats["counts compared"] = stats.get("counts compared", 0) + 1
    # search, every matching line through the cursors: the same lines as Python's
    if total and total <= 600:
        got_lines = []
        for t in all_text("search", dict(args, collapse=False, limit=200)):
            got_lines += [(int(m.group(2)), int(m.group(3))) for m in REF.finditer(t) if m.group(1) in (None, pid)]
        flat = sorted((i, n) for i in live for n in holds(i, q, icase))
        ok(sorted(got_lines) == flat, "search %r: %d lines, want %d" % (q, len(got_lines), len(flat)))
        stats["searches compared"] = stats.get("searches compared", 0) + 1
text, err = s.call("count", {"kind": "all", "max_tokens": 20000})
got = {int(m.group(2)): int(m.group(3).replace(",", "")) for m in ROW.finditer(text)}
ok(got == {i: len(lines_of(i)) for i in live}, "count without a text: %s, want %s" % (got, {i: len(lines_of(i)) for i in live}))
# follow an ID of the grown log
zk = [i for i, x in live.items() if x.get("source", "").endswith("Zookeeper_2k.log") and not x.get("continues")][0]
tok = re.search(rb"0x[0-9a-f]{10,}", TEXT[zk]).group(0).decode()
# (a whole token: not part of a longer word or ID: letters, digits, _ and - and bytes from 0x80 on; oracle.py is_word)
want = sorted((i, n) for i in live for n, l in enumerate(lines_of(i), 1) if re.search(rb"(?<![A-Za-z0-9_\x2d\x80-\xff])" + re.escape(tok.encode()) + rb"(?![A-Za-z0-9_\x2d\x80-\xff])", l))
got_lines = []
for t in all_text("follow", {"id": tok, "limit": 200}):
    got_lines += [(int(m.group(2)), int(m.group(3))) for m in REF.finditer(t) if m.group(1) in (None, pid)]
ok(sorted(got_lines) == want, "follow %s: %d lines, want %d" % (tok, len(got_lines), len(want)))
# the failing build, in errors, compare and guard; the session, the fix memory
text = "".join(all_text("errors", {"kind": "all"}))
ok("login_timeout" in text, "errors names the failing test")
text = "".join(all_text("compare", {"a": "r%d" % builds[0], "b": "r%d" % builds[1]}))
ok("login_timeout" in text, "compare names what is new in the failing build")
text = "".join(all_text("guard", {"kinds": ["failed"]}))
ok(re.search(r"(?<![\w/-])(?:%s/)?r%d\b" % (re.escape(pid), builds[1]), text) is not None, "guard notes the failed run")
for name, args in (("odd", {}), ("quiet", {}), ("fixes", {"text": "login_timeout"}), ("search", {"text": "timeout", "kind": "sessions"})):
    all_text(name, args)
# the forgotten run: in no answer, and `lines` says it was forgotten
text, err = s.call("lines", {"ref": "%s/r%d:1" % (pid, hdfs)})
ok(err and "was forgotten" in text, "lines on the forgotten run: %r" % text)
text, err = s.call("count", {"text": "blk_", "kind": "all"})
ok("r%d" % hdfs not in text, "the forgotten run is in no count")
code = s.close()
ok(code == 0, "the server exited %d" % code)

# ---------------------------------------------------------------- memory

print("3. memory shows the day", flush=True)
code, out, err = sh(["memory", "--runs"])
ok(code == 0 and "  runs: 5 (2 command runs, 2 files, 1 session piece)" in out, "memory --runs: %s" % out)
code, out, err = sh(["memory"])
ok(code == 0 and "  5 outputs, files and chats, from 1 folder\n" in out, "memory, the overview: %s" % out)

print("day: %s" % ", ".join("%s %d" % kv for kv in sorted(stats.items())))
if fails:
    print("FAILED: %d" % len(fails))
    sys.exit(1)
print("ALL EXACT")
