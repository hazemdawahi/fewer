"""readonly_check.py SEARCH PACK WORK DATA: every MCP tool leaves the memory store unchanged
(python3 -I).

SEARCH is the fewer program, PACK the fewer-pack program that packs, WORK a scratch folder
(made afresh), DATA a folder of copies of the Loghub *_2k logs. A store gets runs of every kind
the tools read: four added logs, a command run twice (exit 0, then 3) through `run` and
pack-pending, and a made-up Claude Code session saved by `sessions sync`. Then `dna = off` and
`sessions = off` go into its config (the server's idle builder and its sessions
sweep are the server's only writers, and both are off), and the state of every file and folder
of the store is taken: its type, size, SHA-256, modification time and inode. One server session
calls every tool, many times: the answers' cursors followed to the end, references read back
with `lines` (before, after, expect, raw, base64), every kind of scope, and calls that fail. The
server must exit 0, and the store must be the same, file by file, to the nanosecond.
"""
import hashlib, importlib.util, json, os, re, subprocess, sys

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
os.makedirs(PROJ)
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ, FEWER_CLAUDE_DIR=CLAUDE, GIT_CEILING_DIRECTORIES=WORK)
fails = []
calls = 0


def ok(cond, what):
    if not cond:
        fails.append(what)
        print("FAILED:", what, flush=True)
    return cond


def state(root):
    """Every file and folder under root: (kind, size, sha256, mtime_ns, inode, mode)."""
    out = {}
    for d, dirs, files in os.walk(root):
        for n in dirs + files:
            p = os.path.join(d, n)
            st = os.lstat(p)
            rel = os.path.relpath(p, root)
            if os.path.isdir(p) and not os.path.islink(p):
                out[rel] = ("dir", st.st_mtime_ns, st.st_ino, st.st_mode)
            else:
                h = hashlib.sha256(open(p, "rb").read()).hexdigest() if os.path.isfile(p) and not os.path.islink(p) else "link"
                out[rel] = ("file", st.st_size, h, st.st_mtime_ns, st.st_ino, st.st_mode)
    return out


# ---------------------------------------------------------------- the store

print("1. a store with runs of every kind", flush=True)
logs = [os.path.join(DATA, f) for f in ("HDFS_2k.log", "Zookeeper_2k.log", "OpenSSH_2k.log", "Spark_2k.log")]
r = subprocess.run([SEARCH, "add"] + logs, env=env, cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
ok(r.returncode == 0, "add: %s" % r.stderr[-300:])
for want in (0, 3):
    r = subprocess.run([SEARCH, "run", "--", "sh", "-c", "echo 'INFO build step one'; echo 'ERROR build step two failed' >&2; exit $BUILD_EXIT"], env=dict(env, FEWER_TEST_NO_PACK="1", BUILD_EXIT=str(want)), cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
    ok(r.returncode == want, "run: exit %d passed through (%d)" % (want, r.returncode))
r = subprocess.run([SEARCH, "pack-pending", "--wait"], env=env, cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
ok(r.returncode == 0, "pack-pending: %s" % r.stderr[-300:])
SID = "0c0c0c0c-1111-4222-8333-444444444444"
os.makedirs(CLAUDE + "/projects/-proj")
session = [
    {"cwd": PROJ, "sessionId": SID, "type": "user", "message": {"role": "user", "content": "the build fails at step two"}, "timestamp": "2026-11-18T10:00:00.000Z"},
    {"cwd": PROJ, "sessionId": SID, "type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "name": "Edit", "input": {"file_path": PROJ + "/build.sh"}}]}, "timestamp": "2026-11-18T10:00:05.000Z"},
]
with open(CLAUDE + "/projects/-proj/%s.jsonl" % SID, "w") as f:
    f.write("".join(json.dumps(x) + "\n" for x in session))
with open(HOME + "/config", "w") as f:
    f.write("sessions = on\n")
r = subprocess.run([SEARCH, "sessions", "sync"], env=env, cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
ok(r.returncode == 0 and b"1 piece saved" in r.stdout, "sessions sync: %s" % r.stdout[-300:])
with open(HOME + "/config", "w") as f:
    f.write("dna = off\nsessions = off\n")
pid = os.listdir(HOME + "/projects")[0]
runs = {}
for line in open(HOME + "/projects/%s/runs.jsonl" % pid).read().split("\n"):
    if line.strip():
        x = json.loads(line)
        runs.setdefault(x["id"], {}).update(x)
kinds = sorted(set(x.get("kind") for x in runs.values()))
ok(kinds == ["add", "run", "session"] and all(x.get("state") == "ready" for x in runs.values()), "the store's runs: %s" % [(i, x.get("kind"), x.get("state")) for i, x in sorted(runs.items())])
cmd_runs = [i for i, x in sorted(runs.items()) if x.get("kind") == "run"]
before = state(HOME)
print("   %d runs (%s); %d files and folders in the store" % (len(runs), ", ".join(kinds), len(before)), flush=True)

# ---------------------------------------------------------------- every tool

print("2. every tool, many times, through one server session", flush=True)
s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
s.initialize()
REF = re.compile(r"(?<![\w/-])((?:%s/)?r\d+:\d+(?:-\d+)?)" % re.escape(pid))
# the last line of a shortened answer gives the next call, ready to send (answer.rs):
# `search {"text":"timeout","cursor":"..."}`
NEXT = re.compile(r"\b([a-z]+) (\{.*\})\s*$")
refs = set()


def call(name, args, walk=True):
    """Call a tool; follow the next calls its answers give to the end; keep the references."""
    global calls
    seen = 0
    while True:
        calls += 1
        try:
            text, err = s.call(name, args)
        except RuntimeError:
            return  # a call the schema refuses (-32602) is an answer too
        refs.update(REF.findall(text))
        last = text.rstrip("\n").split("\n")[-1]
        m = NEXT.search(last) if last.startswith("left out") else None
        seen += 1
        if not walk or not m or seen > 40:
            return
        try:
            name, args = m.group(1), json.loads(m.group(2))
        except ValueError:
            return


for args in ({}, {"kind": "all"}, {"kind": "sessions"}, {"project": "*"}, {"limit": 1}, {"since": "2005-01-01", "until": "2030-01-01"}):
    call("logs", args)
for args in (
    {"text": "INFO"},
    {"text": "error", "icase": True},
    {"text": "blk_", "limit": 2},
    {"text": "build step", "kind": "all"},
    {"text": "fails at step", "kind": "sessions"},
    {"text": "INFO", "context": 2, "limit": 2},
    {"text": "INFO", "collapse": False, "limit": 3},
    {"text": "Received block", "whole": True, "limit": 5},
    {"text": "a", "limit": 1, "max_tokens": 200},
    {"text": "no line holds this text at all"},
):
    call("search", args)
for args in ({}, {"text": "INFO"}, {"text": "e", "by": "key"}, {"by": "hour"}, {"by": "level"}, {"text": "build", "kind": "all"}, {"by": "key", "max_tokens": 200}):
    call("count", args)
for ref in sorted(refs)[:60]:
    for args in ({"ref": ref}, {"ref": ref, "before": 0, "after": 0, "raw": True}, {"ref": ref, "base64": True}, {"ref": ref, "expect": "not the text"}):
        call("lines", args, walk=False)
call("follow", {"id": "blk_-1608999687919862906"})
call("follow", {"id": "blk_-1608999687919862906", "limit": 1})
for args in ({}, {"runs": ["r1"]}, {"limit": 2}, {"kind": "all"}):
    call("odd", args)
for args in ({}, {"kind": "all"}, {"limit": 1}):
    call("errors", args)
call("compare", {"a": "r%d" % cmd_runs[0], "b": "r%d" % cmd_runs[1]})
call("compare", {"a": "r1", "b": "r2"})
call("quiet", {"against": "window", "since": "2005-06-03", "until": "2016-01-01"})
call("quiet", {})
for args in ({}, {"limit": 5}, {"kinds": ["failed", "back"]}, {"kinds": ["new"], "limit": 2}):
    call("guard", args)
for args in ({"text": "error", "icase": True, "limit": 3}, {"text": "build step two failed", "kind": "all"}):
    call("fixes", args)
# calls that fail: a run that is not there, a line past the end, a bad time, bad arguments
for name, args in (
    ("lines", {"ref": "%s/r999:1" % pid}),
    ("lines", {"ref": "%s/r1:999999" % pid}),
    ("search", {"text": "x", "since": "not a time"}),
    ("search", {"text": "x", "frobnicate": True}),
    ("count", {"by": "colour"}),
    ("logs", {"project": "no-such-project"}),
):
    call(name, args, walk=False)
code = s.close()
ok(code == 0, "the server exited %d" % code)
print("   %d calls, %d references read back" % (calls, min(len(refs), 60)), flush=True)

# ---------------------------------------------------------------- the store, again

print("3. the store, file by file", flush=True)
after = state(HOME)
for rel in sorted(set(before) | set(after)):
    if before.get(rel) != after.get(rel):
        ok(False, "changed by the tools: %s: %s -> %s" % (rel, before.get(rel), after.get(rel)))

print("readonly_check: %d calls; %d files and folders compared" % (calls, len(before)))
if fails:
    print("FAILED: %d" % len(fails))
    sys.exit(1)
print("ALL UNCHANGED")
