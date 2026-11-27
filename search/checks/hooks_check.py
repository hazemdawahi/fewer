"""hooks_check.py SEARCH PACK WORK: fewer's Claude Code hooks, on made-up hook input:
- `hook post-bash`: a long Bash output (a failing test run) comes back as Claude Code's
  updatedToolOutput, the tool's own result with its stdout replaced by `run --short`'s view and
  every other field as it was; the whole output is saved as a run, its secrets hidden, and
  `lines` reads it back; a short output, another tool, broken input: no output at all; only
  runs and logs are shortened: a file read, a search, output already cut by `| tail`, and a
  command with FEWER_FULL=1 are left whole, while a long log printed by `cat` is shortened;
- `hook start`: the start card as SessionStart's additionalContext; nothing for a folder with no
  memory;
- `hook precompact`: nothing while sessions are not saved; the short summary's instructions once
  they are.
Every process gets a made-up HOME and agents' folders (safe_env.py); WORK is a new scratch folder.
"""
import importlib.util, json, os, subprocess, sys

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
    f.write("dna = off\nsessions = off\n")
env = safe.env(FEWER_HOME=HOME, FEWER_PROJECT=PROJ, FEWER_TEST_NO_PACK="1", FEWER_PACK_BIN=PACK)
fails = []


def ok(cond, what):
    if not cond:
        fails.append(what)
        print("FAIL: " + what, flush=True)


def hook(which, data, cwd=PROJ, e=None):
    raw = data if isinstance(data, bytes) else json.dumps(data).encode()
    r = subprocess.run([SEARCH, "hook", which], input=raw, cwd=cwd, env=e or env, capture_output=True)
    return r.returncode, r.stdout.decode()


# a failing test run's output: 300 passing lines, two failures, a secret, the summary
lines = ["test_case_%03d ... ok" % i for i in range(300)]
lines[120] = "test_case_120 ... FAIL"
lines[240] = "test_case_240 ... FAIL"
lines.append("AssertionError: 121 != 122 : total off by one")
lines.append("export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
lines.append("FAILED (failures=2)")
out = "\n".join(lines) + "\n"
resp = {"stdout": out, "stderr": "", "interrupted": False, "isImage": False}
event = {"session_id": "s1", "transcript_path": "/dev/null", "cwd": PROJ, "hook_event_name": "PostToolUse", "tool_name": "Bash",
         "tool_input": {"command": "python3 -m unittest -v", "description": "Run the tests"}, "tool_response": resp}
code, t = hook("post-bash", event)
ok(code == 0, "post-bash exits 0")
try:
    o = json.loads(t)
except ValueError:
    o = {}
h = o.get("hookSpecificOutput", {})
u = h.get("updatedToolOutput", {})
ok(h.get("hookEventName") == "PostToolUse", "post-bash names its event: %r" % t[:200])
ok(set(u) == set(resp) and u.get("interrupted") is False and u.get("isImage") is False and u.get("stderr") == "", "the tool's other fields are as they were: %r" % {k: v for k, v in u.items() if k != "stdout"})
view = u.get("stdout", "")
print(view)
ok(view.startswith("python3 -m unittest -v: done; 303 lines saved as r1\n"), "the view's first line: %r" % view.split("\n")[0])
# (2026-11-24: a line the last lines show is not listed among the problems too)
ok("\n  121 test_case_120 ... FAIL\n  241 test_case_240 ... FAIL\n" in view and "\n  301 AssertionError: 121 != 122 : total off by one\n" in view and "\n  303 FAILED (failures=2)" in view, "the view names the failures, the exception and the summary")
ok("wJalrXUtnFEMI" not in view, "the view hides the secret")
ok(len(view) < len(out) / 5, "the view is far smaller: %d bytes against %d" % (len(view), len(out)))
# the whole output is saved, secrets hidden; lines reads it back
subprocess.run([SEARCH, "pack-pending", "--wait"], cwd=PROJ, env=env, capture_output=True)
s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
s.initialize()
t2, err = s.call("lines", {"ref": "r1:121", "before": 0, "after": 0})
ok(not err and "\n  121 test_case_120 ... FAIL\n" in t2, "lines reads the saved output: %r" % t2[:200])
t3, err = s.call("search", {"text": "wJalrXUtnFEMI", "runs": ["r1"]})
ok(not err and t3.startswith("no line holds"), "the saved output holds no secret: %r" % t3[:200])
t4, err = s.call("search", {"text": "AWS_SECRET_ACCESS_KEY", "runs": ["r1"]})
ok(not err and "[hidden" in t4, "the secret's line is saved with it hidden: %r" % t4[:300])
s.close()
runs = [json.loads(l) for l in open(os.path.join(HOME, "projects", os.listdir(HOME + "/projects")[0], "runs.jsonl"))]
ok(any(r.get("via") == "claude-bash" for r in runs), "the run is marked as from Claude's Bash tool")
# an exit code, when the result has one, is kept and shown
code, t = hook("post-bash", dict(event, tool_response=dict(resp, exitCode=1)))
ok(json.loads(t)["hookSpecificOutput"]["updatedToolOutput"]["stdout"].startswith("python3 -m unittest -v: exit 1; 303 lines saved as r2"), "an exit code is shown: %r" % t[:200])
# left alone: a short output, another tool, broken input, no stdout field
for what, data in (
    ("a short output", dict(event, tool_response=dict(resp, stdout="ok\n"))),
    ("another tool", dict(event, tool_name="Read")),
    ("broken JSON", b"{not json"),
    ("no tool_response", {k: v for k, v in event.items() if k != "tool_response"}),
    ("an empty input", b""),
    # long, but what Claude asked to read: left whole
    ("a file read", dict(event, tool_input={"command": "cat notes.txt"}, tool_response=dict(resp, stdout="".join("note line %d about the design\n" % i for i in range(300))))),
    ("a search", dict(event, tool_input={"command": "rg -n test_case src"})),
    ("an output cut by tail", dict(event, tool_input={"command": "python3 -m unittest -v 2>&1 | tail -n 400"})),
    ("a command with FEWER_FULL=1", dict(event, tool_input={"command": "FEWER_FULL=1 python3 -m unittest -v"})),
):
    code, t = hook("post-bash", data)
    ok(code == 0 and t == "", "post-bash leaves %s alone: %r" % (what, t[:100]))
# a long log is shortened whatever printed it
log = "".join("2026-11-23 10:%02d:%02d INFO request %d served\n" % (i // 60 % 60, i % 60, i) for i in range(500)) + "2026-11-23 11:00:00 ERROR disk full on /data\n"
code, t = hook("post-bash", dict(event, tool_input={"command": "cat /var/log/app.log"}, tool_response=dict(resp, stdout=log)))
try:
    v = json.loads(t)["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
except (ValueError, KeyError):
    v = ""
ok(code == 0 and v.startswith("cat /var/log/app.log: done; 501 lines saved as r3") and "ERROR disk full on /data" in v, "a long log is shortened: %r" % t[:300])
# a secret in a setting before the program: shortened all the same, the secret hidden where the
# command is saved and shown
tok = "ghp_" + "abcdefghijklmnopqrstuvwxyz0123456789"
build = "".join("building part %d of the app\n" % i for i in range(120))
code, t = hook("post-bash", dict(event, tool_input={"command": "GITHUB_TOKEN=%s make all" % tok}, tool_response=dict(resp, stdout=build)))
try:
    v = json.loads(t)["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
except (ValueError, KeyError):
    v = ""
saved = "".join(open(os.path.join(dp, "runs.jsonl")).read() for dp in (os.path.join(HOME, "projects", d) for d in os.listdir(HOME + "/projects")) if os.path.exists(os.path.join(dp, "runs.jsonl")))
ok(code == 0 and " lines saved as r" in v and tok not in t and tok not in saved, "a secret setting before the program: shortened, the secret hidden: %r" % t[:300])
# an output past Claude Code's limit: stdout holds its first 30,000 characters, the whole is in
# the file Claude Code names (persistedOutputPath, Claude Code 2.1.287): all of it saved and summed up
tr = os.path.join(WORK, "session", "tool-results")
os.makedirs(tr)
whole = "".join("%d\n" % i for i in range(1, 100001))
open(os.path.join(tr, "b1.txt"), "w").write(whole)
big = dict(resp, stdout=whole[:30000], persistedOutputPath=os.path.join(tr, "b1.txt"), persistedOutputSize=len(whole))
code, t = hook("post-bash", dict(event, tool_input={"command": "make spam"}, tool_response=big))
try:
    v = json.loads(t)["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
except (ValueError, KeyError):
    v = ""
ok(code == 0 and ("100,000 lines saved" in v or "100000 lines saved" in v) and "100000" in v, "a long output Claude Code saved itself: all of it: %r" % v[:400])
# the start card
start = {"session_id": "s2", "transcript_path": "/dev/null", "cwd": PROJ, "hook_event_name": "SessionStart", "source": "startup"}
code, t = hook("start", start)
try:
    h = json.loads(t).get("hookSpecificOutput", {})
except ValueError:
    h = {}
ok(code == 0 and h.get("hookEventName") == "SessionStart" and "history of " in h.get("additionalContext", ""), "start adds the card: %r" % t[:200])
os.makedirs(WORK + "/empty")
code, t = hook("start", dict(start, cwd=WORK + "/empty"), cwd=WORK + "/empty", e=dict(env, FEWER_PROJECT=WORK + "/empty"))
ok(code == 0 and t == "", "start prints nothing for a folder with no memory: %r" % t[:200])
# precompact: nothing while sessions are not saved (the summary is Claude Code's own); a short
# summary asked for once they are (fewer keeps the whole session)
pre = {"session_id": "s3", "transcript_path": "/dev/null", "cwd": PROJ, "hook_event_name": "PreCompact", "trigger": "auto", "custom_instructions": ""}
code, t = hook("precompact", pre)
ok(code == 0 and t == "", "precompact says nothing while sessions are off: %r" % t[:200])
with open(HOME + "/config", "w") as f:
    f.write("dna = off\nsessions = on\n")
code, t = hook("precompact", pre)
ok(code == 0 and t.startswith("Keep this summary short, about 1,500 tokens") and "fewer keeps this whole session" in t, "precompact asks for a short summary when sessions are saved: %r" % t[:200])
code, t = hook("precompact", pre, e=safe_env.without(env, "FEWER_CLAUDE_DIR"))
ok(code == 0 and t == "", "precompact says nothing when Claude Code's folder is not known (no session is saved): %r" % t[:200])
code, t = hook("nonsense", start)
ok(code == 2, "an unknown hook is a usage error")
# a run and then fewer reading files, chained in one call: the run's part shortened (saved
# whole), fewer's file view kept whole after it
reads_view = "== calc.py (2 lines)\n1 def add(a, b):\n2     return a + b\n"
chained = dict(event, tool_input={"command": "python3 -m unittest -v 2>&1 || true; fewer read calc.py", "description": "Run the tests, read the code"},
               tool_response=dict(resp, stdout=out + reads_view))
code, t = hook("post-bash", chained)
cv = json.loads(t)["hookSpecificOutput"]["updatedToolOutput"]["stdout"] if t else ""
ok(cv.startswith("python3 -m unittest -v 2>&1 || true; fewer read calc.py: done; 303 lines saved as r") and cv.endswith(reads_view)
   and "test_case_120 ... FAIL" in cv and len(cv) < len(out) / 5, "a run chained with fewer read: the run shortened, the file whole: %r" % cv[-300:])
print("FAILED: %d" % len(fails) if fails else "ALL PASSED")
sys.exit(1 if fails else 0)
