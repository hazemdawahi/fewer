"""mcp_check.py SEARCH PACK WORK DATA: the MCP server's protocol (run with python3 -I).

SEARCH is the fewer program, PACK the fewer-pack program that packs, WORK a scratch folder
(made afresh: the store goes in WORK/home), DATA a folder of log copies (*_2k logs). Checks:
initialize with the four known protocol versions and an unknown one; initialized; ping;
tools/list (every tool, its schema and read-only annotations); every tool; -32700 (not JSON,
not UTF-8, nested too deep); -32600 (not an object, no jsonrpc, no method, a null id, an empty
batch, a 20 MiB line); -32601; -32602 (unknown tool, unknown field, wrong type, missing field);
a batch (answers in one array; nothing for a batch of notifications); a request cancelled
before it starts gets no answer; a damaged archive in the store (one error line for that run
inside the answer, the others answered; `lines` on it gives isError; the server goes on); the
end of stdin (exit 0); every stdout line is one JSON-RPC message.
"""
import importlib.util, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

SEARCH, PACK, WORK, DATA = (os.path.abspath(a) for a in sys.argv[1:5])
# (a folder that is there is cleared only when a check made it: mcpc.fresh_work)
mcpc.fresh_work(WORK)
# every process this check starts, the server too, gets a made-up HOME, agents' folders and
# claude, inside WORK (safe_env.py)
safe = safe_env.setup(WORK)
HOME, PROJ = WORK + "/home", WORK + "/proj"
os.makedirs(PROJ)
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ, FEWER_DEBUG_LOG=WORK + "/server.log")
fails = []


def ok(cond, what):
    if not cond:
        fails.append(what)
        print("FAILED:", what)


logs = [os.path.join(DATA, f) for f in ("HDFS_2k.log", "Zookeeper_2k.log", "Apache_2k.log", "Spark_2k.log")]
subprocess.run([SEARCH, "add"] + logs, check=True, env=env, cwd=PROJ, stdout=subprocess.DEVNULL)
pid = os.listdir(HOME + "/projects")[0]

s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
# initialize: the four versions we know are answered as asked, any other with our newest
for v in ("2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2099-01-01"):
    r = s.request("initialize", {"protocolVersion": v, "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}})
    res = r.get("result", {})
    ok(res.get("protocolVersion") == (v if v != "2099-01-01" else "2025-11-25"), "initialize %s -> %s" % (v, res.get("protocolVersion")))
    ok(res.get("capabilities") == {"tools": {"listChanged": False}}, "capabilities")
    ok(res.get("serverInfo", {}).get("name") == "fewer" and res.get("instructions"), "serverInfo and instructions")
s.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
ok(s.request("ping").get("result") == {}, "ping")

# tools/list
tl = s.request("tools/list")["result"]["tools"]
names = [t["name"] for t in tl]
ok(names == ["logs", "search", "count", "lines", "outline", "history", "follow", "odd", "errors", "compare", "quiet", "guard", "fixes"], "tool names %s" % names)
for t in tl:
    sch = t["inputSchema"]
    ok(sch.get("type") == "object" and sch.get("additionalProperties") is False and "properties" in sch, "schema of %s" % t["name"])
    # (the read-only hint alone since 2026-11-23: the others only matter for tools that change
    # things, and every token of the list is read again by every reply)
    ok(t["annotations"] == {"readOnlyHint": True}, "annotations of %s" % t["name"])
    ok(t.get("description") and t.get("title"), "description and title of %s" % t["name"])

# outline (2026-11-23): a file in the project's folder, with the lines each item spans; a file
# outside the folder is refused, whatever the path
with open(PROJ + "/shop.py", "w") as f:
    f.write("import os\n\nclass Cart:\n    def total(self):\n        return 1\n\ndef main():\n    pass\n")
r = s.request("tools/call", {"name": "outline", "arguments": {"path": "shop.py"}})["result"]
t = r["content"][0]["text"]
ok(not r.get("isError") and "\n3-6 class Cart:\n  4-6 def total(self):\n7-8 def main():\nall shown" in t, "outline of a file in the project's folder: %r" % t)
for bad_path in ("/etc/hosts", "../outside.txt", HOME + "/x"):
    r = s.request("tools/call", {"name": "outline", "arguments": {"path": bad_path}})["result"]
    ok(r.get("isError"), "outline refuses %s: %r" % (bad_path, r["content"][0]["text"][:200]))
os.remove(PROJ + "/shop.py")

# every tool
for name, args in (
    ("logs", {}),
    ("search", {"text": "INFO", "limit": 3}),
    ("count", {"text": "error", "icase": True}),
    ("count", {}),
    ("lines", {"ref": "%s/r1:10" % pid, "before": 1, "after": 1}),
    ("follow", {"id": "blk_-1608999687919862906"}),
    ("odd", {"runs": ["r4"]}),
    ("errors", {}),
    ("compare", {"a": "r1", "b": "r2"}),
    ("quiet", {"against": "window", "since": "2005-06-03", "until": "2016-01-01"}),
    ("guard", {"limit": 5}),
    ("guard", {"kinds": ["failed", "back"]}),
    ("fixes", {"text": "error", "icase": True, "limit": 3}),
):
    text, err = s.call(name, args)
    ok(not err and text.strip(), "tool %s %s" % (name, args))
    last = text.rstrip("\n").split("\n")[-1]
    ok(last.startswith("all shown") or last.startswith("left out:"), "last line of %s: %s" % (name, last))

# saved runs and sessions too, and every tool over them: two finished `run`s of one
# command (exit 0, then exit 3), a run whose `run` was killed (its capture waits for
# pack-pending, its writer gone), and a made-up Claude Code session saved by `sessions sync`
import signal
CLAUDE, SID = WORK + "/claude", "0b0b0b0b-1111-4222-8333-444444444444"
envb = dict(env, FEWER_CLAUDE_DIR=CLAUDE, GIT_CEILING_DIRECTORIES=WORK)
for want in (0, 3):
    # (one command, the same argv: its exit status comes from the environment)
    r = subprocess.run([SEARCH, "run", "--", "sh", "-c", "echo 'INFO build step one'; echo 'ERROR build step two failed' >&2; exit $BUILD_EXIT"], env=dict(envb, FEWER_TEST_NO_PACK="1", BUILD_EXIT=str(want)), cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
    ok(r.returncode == want, "run: exit %d passed through (%d)" % (want, r.returncode))
r = subprocess.run([SEARCH, "pack-pending", "--wait"], env=envb, cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
ok(r.returncode == 0, "pack-pending: %s" % r.stderr[-200:])
k = subprocess.Popen([SEARCH, "run", "--", "sh", "-c", "echo 'INFO killed run alive'; exec sleep 30"], env=dict(envb, FEWER_TEST_NO_PACK="1"), cwd=PROJ, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
k.stdout.readline()
# (the run and its command are one process group: both go)
os.killpg(k.pid, signal.SIGKILL)
k.wait()
k.stdout.close()
k.stderr.close()
os.makedirs(CLAUDE + "/projects/-proj")
session = [
    {"cwd": PROJ, "sessionId": SID, "type": "user", "message": {"role": "user", "content": "the build fails at step two"}, "timestamp": "2026-11-18T10:00:00.000Z"},
    {"cwd": PROJ, "sessionId": SID, "type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "name": "Edit", "input": {"file_path": PROJ + "/build.sh"}}]}, "timestamp": "2026-11-18T10:00:05.000Z"},
]
with open(CLAUDE + "/projects/-proj/%s.jsonl" % SID, "w") as f:
    f.write("".join(json.dumps(x) + "\n" for x in session))
with open(HOME + "/config", "w") as f:
    f.write("sessions = on\n")
r = subprocess.run([SEARCH, "sessions", "sync"], env=envb, cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
ok(r.returncode == 0 and b"sessions: 1 piece saved" in r.stdout, "sessions sync saved the session: %s" % r.stdout[-300:])
srid = max(int(json.loads(l)["id"]) for l in open(HOME + "/projects/%s/runs.jsonl" % pid).read().split("\n") if l.strip())
for name, args, want in (
    ("logs", {"kind": "all"}, "ended without a status; the next `fewer pack-pending` packs it"),
    # (the calls name no project: this project's runs as "r42", the token test of 2026-11-23)
    ("logs", {"kind": "sessions"}, "r%d session" % srid),
    ("search", {"text": "build step", "kind": "all"}, "exit 3"),
    ("search", {"text": "killed run alive"}, "ended without a status (its `run` was stopped; the next `fewer pack-pending` packs it): searched its output (1 line)"),
    ("search", {"text": "fails at step two", "kind": "sessions"}, "1 line holds"),
    ("count", {"text": "build", "kind": "all"}, "ended without a status"),
    ("count", {"kind": "all"}, "(ended without a status; the next `fewer pack-pending` packs it: counted once it is saved)"),
    ("count", {"by": "key", "kind": "all"}, "not counted: r%d" % (srid - 1)),
    ("lines", {"ref": "%s/r%d:1" % (pid, srid)}, "the build fails at step two"),
    ("follow", {"id": SID, "kind": "all"}, "r%d:1" % srid),
    ("odd", {"kind": "all"}, "not counted: r%d (ended without a status" % (srid - 1)),
    # (limit 20: errors shows the 5 biggest groups by default since the token test of
    # 2026-11-23, and the build's one record is the 7th of 7)
    ("errors", {"kind": "all", "limit": 20}, "ERROR build step two failed"),
    ("compare", {"command": "build step", "failing": True}, "exit 3"),
    ("quiet", {"kind": "runs"}, ""),
    ("guard", {"kind": "all"}, "not in these notes yet: r%d (ended without a status" % (srid - 1)),
    ("fixes", {"text": "build step two failed"}, "build step two failed"),
):
    text, err = s.call(name, args)
    last = text.rstrip("\n").split("\n")[-1]
    ok(not err and want in text and (last.startswith("all shown") or last.startswith("left out:")), "runs: tool %s %s: wanted %r in\n%s" % (name, args, want, text[:1500]))

# a tool problem is an isError answer, not a protocol error
text, err = s.call("lines", {"ref": "%s/r99:1" % pid})
ok(err and "no run r99" in text, "lines on a missing run: %s" % text)
# count by key, hour and level come from the message catalogs
for by in ("key", "hour", "level"):
    text, err = s.call("count", {"by": by})
    last = text.split("\n")[-1]
    ok(not err and (last == "all shown" or last.startswith("left out:")), "count by %s: %s" % (by, text[:200]))
text, err = s.call("count", {"by": "key", "cursor": "#x"})
ok(err, "count by key with a bad cursor is an error: %s" % text[:200])
# fixes needs a text or a ref; a kind guard does not know is -32602
text, err = s.call("fixes", {})
ok(err and "give `text`" in text, "fixes without text or ref: %s" % text[:200])
r = s.request("tools/call", {"name": "guard", "arguments": {"kinds": ["nope"]}})
ok("error" in r and r["error"]["code"] == -32602 and "kinds" in r["error"]["message"], "guard with an unknown kind: %s" % r)


def expect_error(raw, code, what):
    s.send_raw(raw)
    r = s.recv()
    ok("error" in r and r["error"]["code"] == code, "%s -> %s" % (what, r))
    return r


# -32700: not JSON, not UTF-8, nested too deep
expect_error(b"this is not json\n", -32700, "not JSON")
expect_error(b'{"jsonrpc":"2.0","id":1,"method":"ping","x":"\xff"}\n', -32700, "not UTF-8")
expect_error(("[" * 65 + "]" * 65 + "\n").encode(), -32700, "nested 65 deep")
# -32600
expect_error(b'"a string"\n', -32600, "not an object")
expect_error(b'{"id":7,"method":"ping"}\n', -32600, "no jsonrpc")
expect_error(b'{"jsonrpc":"2.0","id":7}\n', -32600, "no method")
expect_error(b'{"jsonrpc":"2.0","id":null,"method":"ping"}\n', -32600, "a null id")
expect_error(b"[]\n", -32600, "an empty batch")
r = expect_error(b'{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"search","arguments":{"text":"' + b"x" * (20 << 20) + b'"}}}\n', -32600, "a 20 MiB line")
ok(r.get("id") is None, "the 20 MiB line's answer has id null")
ok(s.request("ping").get("result") == {}, "ping after the 20 MiB line")
# -32601
expect_error(b'{"jsonrpc":"2.0","id":9,"method":"resources/list"}\n', -32601, "unknown method")
# -32602
for args, what in (
    ({"name": "nope", "arguments": {}}, "unknown tool"),
    ({"name": "search", "arguments": {"txt": "x"}}, "unknown field"),
    ({"name": "search", "arguments": {"text": "x", "limit": "5"}}, "wrong type"),
    ({"name": "search", "arguments": {"icase": True}}, "missing field"),
    ({"name": "search", "arguments": {"text": "x", "limit": 501}}, "over the maximum"),
    ({"arguments": {}}, "no tool name"),
):
    r = s.request("tools/call", args)
    ok("error" in r and r["error"]["code"] == -32602, "-32602 for %s: %s" % (what, r))
r = s.request("tools/call", {"name": "search", "arguments": {"txt": "x"}})
ok("unknown field 'txt'; the fields are text" in r["error"]["message"], "the reason names the fields")

# a batch: answers in order, in one array; notifications get none
s.send([{"jsonrpc": "2.0", "id": "a", "method": "ping"}, {"jsonrpc": "2.0", "method": "notifications/initialized"}, {"jsonrpc": "2.0", "id": "b", "method": "tools/list"}, {"jsonrpc": "2.0", "id": "c", "method": "nope"}])
r = s.recv()
ok(isinstance(r, list) and [x.get("id") for x in r] == ["a", "b", "c"] and "error" in r[2], "batch answers %s" % (r if not isinstance(r, list) else [x.get("id") for x in r]))
s.send([{"jsonrpc": "2.0", "method": "notifications/initialized"}])
ok(s.request("ping").get("result") == {}, "a batch of notifications gets no answer")
# a request cancelled before it starts gets no answer
s.send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 4242, "reason": "test"}})
s.send({"jsonrpc": "2.0", "id": 4242, "method": "ping"})
r = s.request("ping")
ok(r.get("result") == {}, "the cancelled request got no answer, the next one did")

# a request cancelled while it runs: it stops and gets no answer; the next one does
code = s.close()
ok(code == 0, "exit status at the end of stdin: %s" % code)
lines_so_far = s.lines
subprocess.run([SEARCH, "add", os.path.join(DATA, "HPC.log")], check=True, env=env, cwd=PROJ, stdout=subprocess.DEVNULL)
s = mcpc.Server(SEARCH, dict(env, FEWER_TEST_SLOW_BLOCK_MS="300"), PROJ, WORK + "/stderr_slow.txt")
s.initialize()
s.send({"jsonrpc": "2.0", "id": "long", "method": "tools/call", "params": {"name": "search", "arguments": {"text": "zzz_never", "time_limit_ms": 60000}}})
import time
time.sleep(0.5)
s.send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "long", "reason": "test"}})
t0 = time.time()
r = s.request("ping")
ok(r.get("result") == {} and time.time() - t0 < 5, "a cancelled running request gets no answer, and the next one comes at once")
# a time limit: the answer says how far it got and gives the next call
text, err = s.call("search", {"text": "zzz_never", "time_limit_ms": 100})
ok(not err and text.startswith("at least 0 lines") and "Next: search" in text, "a search cut by the time limit: %s" % text[:300])
code = s.close()
ok(code == 0, "exit status after the slow server: %s" % code)
earlier_lines = lines_so_far + s.lines
s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
s.initialize()

# a damaged archive in the store: one error line for it, the other runs still answered
runs = sorted(f for f in os.listdir(HOME + "/projects/" + pid) if f.endswith(".cmf"))
bad = HOME + "/projects/%s/%s" % (pid, runs[1])
b = bytearray(open(bad, "rb").read())
b[len(b) // 2] ^= 0x55
open(bad, "wb").write(bytes(b))
text, err = s.call("search", {"text": "INFO"})
# (this project's runs as "r2": the calls name no project)
ok(not err and re.search(r"(?<![\w/-])(?:%s/)?r2: damaged archive" % re.escape(pid), text) is not None, "search with a damaged run: %s" % text[:300])
ok(re.search(r"^== (?:%s/)?r[34] " % re.escape(pid), text, re.M) is not None or "per run:" in text, "the other runs are answered")
text, err = s.call("count", {"text": "INFO"})
ok(not err and re.search(r"(?<![\w/-])(?:%s/)?r2: damaged archive" % re.escape(pid), text) is not None, "count with a damaged run")
text, err = s.call("lines", {"ref": "%s/r2:5" % pid})
ok(err and "damaged archive" in text, "lines on the damaged run: %s" % text)
ok(s.request("ping").get("result") == {}, "the server goes on after a damaged archive")

# the end of stdin: exit 0; every stdout line is one JSON-RPC message
code = s.close()
ok(code == 0, "exit status at the end of stdin: %s" % code)
bad_lines = 0
s.lines = earlier_lines + s.lines
for line in s.lines:
    try:
        m = json.loads(line)
        items = m if isinstance(m, list) else [m]
        if not items or not all(isinstance(x, dict) and x.get("jsonrpc") == "2.0" and ("result" in x or "error" in x) for x in items):
            bad_lines += 1
    except Exception:
        bad_lines += 1
ok(bad_lines == 0 and line.endswith(b"\n"), "every stdout line is one JSON-RPC message (%d bad of %d)" % (bad_lines, len(s.lines)))

# a server started with no store: a tool says what to do (isError), the server goes on
env2 = safe.env(FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ, FEWER_DEBUG_LOG=WORK + "/server.log", FEWER_HOME=None)
s2 = mcpc.Server(SEARCH, env2, PROJ, WORK + "/stderr2.txt")
s2.initialize()
text, err = s2.call("search", {"text": "x"})
ok(err and "FEWER_HOME" in text, "no store: %s" % text)
ok(s2.close() == 0, "exit 0")

print("mcp_check: %d stdout lines, %d failed checks" % (len(s.lines), len(fails)))
if fails:
    sys.exit(1)
print("mcp_check: ALL PASSED")
