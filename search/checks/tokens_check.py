"""tokens_check.py SEARCH PACK WORK: `fewer tokens`, end to end on made-up data: a made-up Claude Code session of the
project with its helpers' transcripts (the Agent tool's under subagents/, a workflow run's three
folders down, a workflow journal beside them), saved by `sessions sync`; then the report for the
project, split at the time in the store's config (`savers.on`, as install sets it) and at a
time given, and for every project; every number checked against Python's own count of the same
lines (each reply once by its message id), and fewer's own runs (helpers.jsonl: helpers of the
side their time falls in, counted with the session that started them); the card on top (the
percent, the day, about what it saved) and the table (chats, replies, tokens per reply, a chat's
first reply), in plain words: no UTC, no API prices. Then `memory --total` in plain words, and
`memory clear`. Every process gets a made-up HOME and agents' folders
(safe_env.py); WORK is a new scratch folder.
"""
import math, importlib.util, json, os, re, subprocess, sys

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
    f.write("dna = off\nsessions = on\nsavers.on = 2026-11-23T18:00:00\n")
# fewer's own runs (helper, workflow, look, web: Claude Code saves no session of them), one line
# a run in the store's helpers.jsonl: counted as helpers of the side their time falls in, together
# with the session that started them (`parent`, up through a run started from a run); a line
# without a parent (an older one) by its folder's git root with --here
MAIN = "7a0e5c11-0000-4000-8000-0000000070c3"  # the made-up session SID below, which is saved
RUNS = [
    {"time": "2026-11-22T11:00:00", "kind": "web", "ok": True, "turns": 2, "tokens": 9000, "output": 300, "cost_usd": 0.02, "cwd": PROJ, "input": 1000, "cache_write": 3000, "cache_read": 5000, "session": "w-1", "parent": MAIN},
    {"time": "2026-11-24T09:40:00", "kind": "helper", "ok": True, "turns": 3, "tokens": 12000, "output": 500, "cost_usd": 0.03, "cwd": PROJ, "input": 2000, "cache_write": 4000, "cache_read": 6000, "session": "h-outer", "parent": MAIN},
    {"time": "2026-11-24T09:42:00", "kind": "helper", "ok": True, "turns": 1, "tokens": 4000, "output": 50, "cwd": PROJ + "/sub", "session": "h-inner", "parent": "h-outer"},
    {"time": "2026-11-24T09:45:00", "kind": "workflow", "ok": True, "turns": 1, "tokens": 20000, "output": 800, "cwd": PROJ},
    {"time": "2026-11-24T09:50:00", "kind": "look", "ok": False, "turns": 2, "tokens": 7000, "output": 200, "cwd": WORK + "/elsewhere"},
    # started from a session that is not saved: not counted
    {"time": "2026-11-24T09:52:00", "kind": "helper", "ok": True, "turns": 4, "tokens": 8000, "output": 90, "cwd": PROJ, "session": "h-2", "parent": "never-saved"},
    # a run Claude Code saved as a session of its own (started outside it): counted once, as that
    {"time": "2026-11-24T09:55:00", "kind": "helper", "ok": True, "turns": 5, "tokens": 99999, "output": 1, "cwd": PROJ, "session": MAIN},
]
with open(HOME + "/helpers.jsonl", "w") as f:
    f.write("".join(json.dumps(x) + "\n" for x in RUNS))
# (git looks no higher than WORK: a run's project is found by its folder's git root)
env = safe.env(FEWER_HOME=HOME, FEWER_PROJECT=PROJ, FEWER_TEST_NO_PACK="1", FEWER_PACK_BIN=PACK, GIT_CEILING_DIRECTORIES=WORK)
fails = []


def pct_number(before, now):
    """fewer's percent of how much smaller `now` is (tokens.rs percent_number): whole percent, one
    decimal near 100 (never 100 while something is left), the size of the growth when it grew."""
    if before <= 0:
        return "0"
    p = 100.0 * (1.0 - now / before)
    if p < 0:
        return "%.0f" % -p
    whole = math.floor(p + 0.5)
    return "%.1f" % min(p, 99.9) if whole >= 100 and now > 0 else "%d" % whole


def pct_fewer(before, now):
    return "%s%% %s" % (pct_number(before, now), "more" if now > before > 0 else "fewer")


def ok(cond, what):
    if not cond:
        fails.append(what)
        print("FAIL: " + what, flush=True)


SID = "7a0e5c11-0000-4000-8000-0000000070c3"


def reply(mid, ts, side, i, w, r, out, workflow=False):
    content = [{"type": "text", "text": "ok"}]
    if workflow:
        content.append({"type": "tool_use", "id": "w" + mid, "name": "Workflow", "input": {"script": "x"}})
    return {"type": "assistant", "cwd": PROJ, "sessionId": SID, "isSidechain": side, "timestamp": ts, "uuid": "u" + mid,
            "message": {"id": mid, "role": "assistant", "content": content, "usage": {"input_tokens": i, "cache_creation_input_tokens": w, "cache_read_input_tokens": r, "output_tokens": out}}}


def user(ts, text, extra=None):
    o = {"type": "user", "cwd": PROJ, "sessionId": SID, "timestamp": ts, "uuid": "q" + ts, "message": {"role": "user", "content": text}}
    o.update(extra or {})
    return o


main = [user("2026-11-22T10:00:00Z", "start"),
        reply("m1", "2026-11-22T10:00:05Z", False, 3, 47000, 0, 900, workflow=True),
        reply("m1", "2026-11-22T10:00:05Z", False, 3, 47000, 0, 900),  # the same message again: once
        reply("m2", "2026-11-22T10:01:00Z", False, 2, 3000, 47003, 400),
        # resumed with fewer on: Claude Code names the agent the session runs with from here
        {"type": "agent-setting", "agentSetting": "fewer-lean", "sessionId": SID},
        user("2026-11-23T19:00:00Z", "summary", {"isCompactSummary": True}),
        reply("m3", "2026-11-23T19:00:10Z", False, 1, 20000, 26000, 300),
        reply("m4", "2026-11-23T19:01:00Z", False, 1, 1500, 46001, 200)]
helper = [reply("h1", "2026-11-22T10:00:30Z", True, 4, 36000, 0, 700), reply("h2", "2026-11-22T10:00:40Z", True, 1, 2000, 36004, 300)]
flow = [reply("f1", "2026-11-23T19:00:30Z", True, 2, 30000, 0, 500)]
claude = env["FEWER_CLAUDE_DIR"]
folder = os.path.join(claude, "projects", "-made-up-proj")
for rel, ls in ((SID + ".jsonl", main), (SID + "/subagents/agent-a1.jsonl", helper), (SID + "/workflows/r1/agents/agent-f1.jsonl", flow)):
    p = os.path.join(folder, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as f:
        f.write("".join(json.dumps(o) + "\n" for o in ls))
with open(os.path.join(folder, SID, "workflows", "r1", "journal.jsonl"), "w") as f:
    f.write(json.dumps({"agent": "f1", "result": "done"}) + "\n")
r = subprocess.run([SEARCH, "sessions", "sync"], cwd=PROJ, env=env, capture_output=True)
ok(r.returncode == 0, "sessions sync: %r" % r.stderr[:300])
subprocess.run([SEARCH, "pack-pending", "--wait"], cwd=PROJ, env=env, capture_output=True)


def count(sessions, split=None, periods=None, here=False, by_mode=False):
    """Python's own count: [before, after], each {replies, helpers, tokens, output, starts, compactions, workflows,
    sessions}; `sessions`: each saved session's lines. `by_mode`: by how each session ran (from an agent-setting
    line naming fewer's agent on, with fewer: from the time of the first line after it; a session naming none,
    by when it started, in fewer's `periods`), else each line by its own time (from `split` on, or in `periods`).
    fewer's own runs (RUNS) too, as helpers with the session that started them (`parent`, up through a run started
    from a run), on its side at the run's time; one without a parent by its own time; `here`: only those under PROJ."""
    sides = [dict(replies=0, helpers=0, tokens=0, output=0, starts=[], compactions=0, workflows=0, sessions=0) for _ in range(2)]

    def in_time(ts):
        if periods is not None:
            return any(ts >= f and (u is None or ts < u) for f, u in periods)
        return bool(split) and ts >= split

    def mode_of(sess):
        switches, pending, start = [], None, None
        for o in sess:
            if o["type"] == "agent-setting":
                pending = o["agentSetting"].startswith("fewer-")
                continue
            ts = o.get("timestamp")
            if ts:
                if start is None or ts < start:
                    start = ts
                if pending is not None:
                    switches.append((ts, pending))
                    pending = None
        return sorted(switches), start

    def side_at(mode, ts):
        if not by_mode:
            return 1 if in_time(ts) else 0
        switches, start = mode
        if switches:
            on = False
            for at, lean in switches:
                if at <= ts:
                    on = lean
            return 1 if on else 0
        return 1 if start is not None and in_time(start) else 0

    modes = {}
    for sess in sessions:
        mode = mode_of(sess)
        modes[next(o["sessionId"] for o in sess if o.get("sessionId"))] = mode
        seen, started, used = set(), False, [False, False]
        for o in sess:
            if o["type"] == "agent-setting":
                continue
            t = sides[side_at(mode, o.get("timestamp", ""))]
            k = 0 if t is sides[0] else 1
            if o["type"] == "user" and o.get("isCompactSummary"):
                t["compactions"] += 1
            if o["type"] != "assistant":
                continue
            m = o["message"]
            if not o["isSidechain"]:
                t["workflows"] += sum(1 for p in m["content"] if p.get("type") == "tool_use" and p.get("name") == "Workflow")
            if m["id"] in seen:
                continue
            seen.add(m["id"])
            u = m["usage"]
            tok = u["input_tokens"] + u["cache_creation_input_tokens"] + u["cache_read_input_tokens"]
            t["replies"] += 1
            t["tokens"] += tok
            t["output"] += u["output_tokens"]
            if o["isSidechain"]:
                t["helpers"] += 1
            elif not started:
                started = True
                t["starts"].append(tok)
            used[k] = True
        for k in (0, 1):
            sides[k]["sessions"] += int(used[k])
    up = {x["session"]: x.get("parent", "") for x in RUNS if x.get("session")}
    for x in RUNS:
        if x.get("session") in modes:
            continue
        p = x.get("parent", "")
        while up.get(p):
            p = up[p]
        if p:
            if p not in modes:
                continue
            k = side_at(modes[p], x["time"])
        else:
            # (no parent: by the git root of its folder; these made-up folders are no repositories)
            if here and x["cwd"] != PROJ:
                continue
            k = 1 if in_time(x["time"]) else 0
        t = sides[k]
        t["replies"] += x["turns"]
        t["helpers"] += x["turns"]
        t["tokens"] += x["tokens"]
        t["output"] += x["output"]
        t["workflows"] += 1 if x["kind"] == "workflow" else 0
    return sides


def table(text):
    """The table's rows: {row name: [cells]}: the lines after its header (the line naming the
    columns), up to the empty line under it."""
    rows = {}
    lines = text.split("\n")
    start = next((i for i, l in enumerate(lines) if l.strip() in ("before fewer      with fewer", "all") or (l.startswith(" ") and "before" in l and "after" in l)), 0) + 1
    for l in lines[start:]:
        if not l.strip():
            break
        m = re.match(r"^(\S.*?)\s{2,}(.*)$", l)
        if m:
            rows.setdefault(m.group(1).strip(), []).append(m.group(2).split())
    return rows


def g(n):
    return "{:,}".format(n)


def rround(x):
    """Rust's round: halves away from zero."""
    return int(x + 0.5) if x >= 0 else -int(-x + 0.5)


def human(n):
    def one(v, unit):
        s = "%.1f" % v
        return (s[:-2] if s.endswith(".0") else s) + unit
    if n < 10_000:
        return g(n)
    if n < 999_500:
        return "%dK" % rround(n / 1e3)
    if n < 999_950_000:
        return one(n / 1e6, "M")
    return one(n / 1e9, "B")


every = main + helper + flow
for args, split, label in ((["tokens", "--here"], "2026-11-23T18:00:00", "split at savers.on"), (["tokens", "--here", "--split", "2026-11-22 10:00:35"], "2026-11-22T10:00:35", "split given"), (["tokens"], "2026-11-23T18:00:00", "everything")):
    r = subprocess.run([SEARCH] + args, cwd=PROJ, env=env, capture_output=True, text=True)
    print(r.stdout)
    ok(r.returncode == 0, "%s: exit %d %r" % (label, r.returncode, r.stderr[:200]))
    given = "--split" in args
    want = count([every], split=split if given else None, periods=None if given else [(split, None)], here="--here" in args, by_mode=not given)
    rows = table(r.stdout)
    got = lambda name, k: rows.get(name, [[]])[0][k] if rows.get(name) else None
    for k in (0, 1):
        w = want[k]
        ok(got("chats", k) == g(w["sessions"]), "%s: chats %s, want %s" % (label, got("chats", k), w["sessions"]))
        ok(got("replies", k) == g(w["replies"]), "%s: replies %s, want %s" % (label, got("replies", k), w["replies"]))
        ok(got("tokens per reply", k) == g(rround(w["tokens"] / max(w["replies"], 1))), "%s: tokens per reply %s, want %s" % (label, got("tokens per reply", k), w["tokens"] / max(w["replies"], 1)))
        starts = sorted(w["starts"])
        ok(rows.get("a chat's first reply", [[]])[0][k] == (g(starts[len(starts) // 2]) if starts else "-"), "%s: first reply %r, want %s" % (label, rows.get("a chat's first reply"), starts))
    b, a = want
    pbr, par = b["tokens"] / b["replies"], a["tokens"] / a["replies"]
    # the card on top: the percent, the day, and about what it saved
    saved = round((b["tokens"] / b["replies"] - a["tokens"] / a["replies"]) * a["replies"])
    ok(r.stdout.startswith("fewer: your Claude Code tokens, ") and ("  %s tokens per reply since " % pct_fewer(pbr, max(par, 1.0))) in r.stdout
       and ("  about %s tokens saved, in " % human(max(0, saved))) in r.stdout and "UTC" not in r.stdout and "API" not in r.stdout
       and r.stdout.rstrip("\n").endswith("Counted on this computer, from your own Claude Code chats."), "%s: the card: %r" % (label, r.stdout[:400]))
# the journal is not a transcript: not saved; the helpers' transcripts are pieces of the session
runs = [json.loads(l) for d in os.listdir(HOME + "/projects") for l in open(os.path.join(HOME, "projects", d, "runs.jsonl"))]
sessions = [x for x in runs if x.get("kind") == "session"]
ok(len(sessions) == 3 and all(x.get("session") == SID for x in sessions), "three pieces of one session: %r" % [(x.get("session"), os.path.basename(x.get("source", ""))) for x in sessions])
ok(not any("journal" in x.get("source", "") for x in sessions), "the journal is not saved")
# no saved session in another folder: the report says so
os.makedirs(WORK + "/empty")
r = subprocess.run([SEARCH, "tokens", "--here"], cwd=WORK + "/empty", env=dict(env, FEWER_PROJECT=WORK + "/empty"), capture_output=True, text=True)
ok(r.returncode == 0 and "no saved Claude Code chat" in r.stdout, "an empty project: %r" % r.stdout[-200:])
r = subprocess.run([SEARCH, "tokens", "--split", "yesterday"], cwd=PROJ, env=env, capture_output=True, text=True)
ok(r.returncode == 2 and "--split takes a UTC time" in r.stderr, "a bad --split is a usage error")

# the totals alone (/fewer-memory): no project's lines
r = subprocess.run([SEARCH, "memory", "--total"], cwd=PROJ, env=env, capture_output=True, text=True)
ok(r.returncode == 0 and r.stdout.startswith("fewer's memory, in ") and " outputs, files and chats, from " in r.stdout and "no size limit: nothing is deleted on its own" in r.stdout and "  1. " not in r.stdout, "memory --total: %r" % r.stdout[-400:])
r = subprocess.run([SEARCH, "memory", "clear", "--yes"], cwd=PROJ, env=env, capture_output=True, text=True)
ok(r.returncode == 0, "memory clear: %r" % r.stdout[-300:])
print("FAILED: %d" % len(fails) if fails else "ALL PASSED")
sys.exit(1 if fails else 0)
