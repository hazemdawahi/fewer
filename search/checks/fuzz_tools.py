"""Fuzz every tool with arguments that pass its schema but hold odd values (non-ASCII text,
control bytes, huge numbers in range, odd references and times), on the store another check
made in WORK (WORK/home, its project root WORK/proj: oracle.py's, fixture_runs.py's or
guard_check.py's). A panic shows as "internal error in search" (caught by the server): none
may appear, and the server must answer every call.

    python3 -I checks/fuzz_tools.py SEARCH_BIN FEWER_PACK_BIN WORK [N]

FUZZ_DEEP=1: the scope fields hold valid values, so the odd texts, cursors and ranges reach the
search. Every process gets a made-up HOME, agents' folders and claude in WORK (safe_env.py).
"""
import importlib.util, json, os, random, subprocess, sys

SEARCH, PACK, W = (os.path.abspath(a) for a in sys.argv[1:4])
N = int(sys.argv[4]) if len(sys.argv) > 4 else 3000
HOME = os.path.join(W, "home")
# (read before safe_env.py makes this process's environment the safe one)
DEEP = os.environ.get("FUZZ_DEEP") == "1"
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(os.path.dirname(os.path.abspath(__file__)), "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)
safe = safe_env.setup(W)
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=os.path.join(W, "proj"))
p = subprocess.Popen([SEARCH, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, cwd=os.path.join(W, "proj"))
rnd = random.Random(20261118)
pid = os.listdir(os.path.join(HOME, "projects"))[0]

ATOMS = ["", "é", "€", "😀", "\u0000", "\n", "\r", "\t", "\\", "\"", ":", "-", "/", "r", "r1", "r2", "r0", "r-1", "r99999999999999999999",
         pid, pid + "/", "/", "*", "last", "last:", "last:é", ":1", ":0", ":-1", ":1-", ":5-2", ":18446744073709551615", ":1-18446744073709551615",
         "2026-11-18", "2026-11-1é", " 10:00", "T10:00:00", "Z", "+02:00", "+1é1", "15m", "99999999999d", "today", "yesterday", "now",
         "0x14ede63a5a7003b", "WARN", "INFO", "blk_", "\x1b[31m", "\xff", "a" * 300]


def rstr(lo, hi):
    s = "".join(rnd.choice(ATOMS) for _ in range(rnd.randint(1, 4)))
    if len(s) < lo:
        s += "x" * (lo - len(s))
    return s[:hi]


TOOLS = json.loads(subprocess.run([SEARCH, "mcp"], input=b'{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n', capture_output=True, env=env).stdout)["result"]["tools"]


# deep mode: the scope fields hold valid values, so the odd texts, cursors and ranges reach
# the search, the counting and the reading of lines
VALID = {
    "project": lambda: rnd.choice([pid, "*", os.path.join(W, "proj")]),
    "runs": lambda: rnd.sample(["r1", "r2", "r3", "r4", "r5", "last", "last:" + rnd.choice(["HDFS", "Linux", "log"])], rnd.randint(1, 3)),
    "since": lambda: rnd.choice(["3d", "2026-11-01", "2026-11-18 00:00", "yesterday", "2026-11-18T00:00:00Z"]),
    "until": lambda: rnd.choice(["now", "2030-01-01", "1h"]),
    "kind": lambda: rnd.choice(["logs", "all", "runs"]),
    "command": lambda: rnd.choice(["log", "HDFS", "data/"]),
    "branch": None,
    "commit": None,
}


def rargs(tool):
    a = {}
    for name, s in tool["inputSchema"]["properties"].items():
        if rnd.random() < 0.55 and name not in tool["inputSchema"].get("required", []):
            continue
        if DEEP and name in VALID:
            if VALID[name] is not None:
                a[name] = VALID[name]()
            continue
        t = s["type"]
        if t == "string" and "enum" in s:
            a[name] = rnd.choice(s["enum"])
        elif t == "string":
            a[name] = rstr(s.get("minLength", 0), min(s.get("maxLength", 50), 400))
            if name == "ref" and rnd.random() < 0.5:
                a[name] = rnd.choice(["", pid + "/"]) + "r" + str(rnd.randint(0, 7)) + ":" + rnd.choice(["1", "2-9", "0", "999999999", "5-2", "1-18446744073709551615", "é"])
            if name == "cursor" and rnd.random() < 0.5:
                a[name] = rnd.choice(["", pid + "/"]) + "r" + str(rnd.randint(0, 7)) + rnd.choice([":1", ":40000", ":0", "", ":é", ":99999999999"])
        elif t == "boolean":
            a[name] = rnd.random() < 0.5
        elif t == "integer":
            # (the published schema has no bounds since 2026-11-23: the server checks them, so
            # numbers inside and outside any bound are sent, the edges of 32 and 64 bits too)
            if "minimum" in s and "maximum" in s:
                a[name] = rnd.choice([s["minimum"], s["maximum"], rnd.randint(s["minimum"], s["maximum"])])
            else:
                a[name] = rnd.choice([0, 1, 2, 5, 40, 199, 200, 500, 501, 3000, 20000, 20001, 120000, -1, 2 ** 31, 2 ** 53, rnd.randint(1, 100000)])
        elif t == "array":
            a[name] = [rstr(0, 40) if rnd.random() < 0.5 else "r%d" % rnd.randint(0, 7) for _ in range(rnd.randint(0, 3))]
    # keep the calls short: a small time limit unless the call set one
    a.setdefault("time_limit_ms", 2000) if "time_limit_ms" in tool["inputSchema"]["properties"] else None
    return a


panics = errors = answers = rpc_errors = 0
for i in range(N):
    tool = rnd.choice(TOOLS)
    args = rargs(tool)
    msg = json.dumps({"jsonrpc": "2.0", "id": i + 10, "method": "tools/call", "params": {"name": tool["name"], "arguments": args}}, ensure_ascii=rnd.random() < 0.5)
    p.stdin.write(msg.encode() + b"\n")
    p.stdin.flush()
    line = p.stdout.readline()
    if not line:
        print("FAIL: the server ended at call %d: %s" % (i, msg))
        sys.exit(1)
    r = json.loads(line)
    if r.get("id") != i + 10:
        print("FAIL: answer to another id", r)
        sys.exit(1)
    if "error" in r:
        rpc_errors += 1
        continue
    text = r["result"]["content"][0]["text"]
    answers += 1
    errors += r["result"]["isError"]
    if "internal error" in text:
        panics += 1
        if panics <= 5:
            print("PANIC:", tool["name"], json.dumps(args, ensure_ascii=False)[:300], "->", text[:300])
p.stdin.close()
code = p.wait(timeout=60)
print("%d calls: %d answers (%d isError), %d JSON-RPC errors, %d panics; exit %d" % (N, answers, errors, rpc_errors, panics, code))
sys.exit(1 if panics or code else 0)
