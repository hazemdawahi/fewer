"""guard_check.py SEARCH PACK WORK [--show]: the acceptance check (python3 -I) of the Guard
notes (`guard`) and the fix memory (`fixes`) on a made-up project in a temporary git repository.

WORK is a scratch folder the check makes (and clears only when a check made it). Its project,
proj/, is a git repository with four commits (fixed dates), and the store gets eight runs, all
saved with `add`; the command runs and the session carry the test switch FEWER_TEST_RUN_TAGS
(kind, command, exit or signal, branch, commit, dirty, start, end; and for the session its agent,
id and edited files), as `run` and a sessions sweep save them:
  r1  ./fake-app --test at commit 1: every key new; the error E1 "db: connection refused" (with
      two frames naming db.py and app.py: one error record of 3 lines); "slow query" 3 times;
      "retry upload" once
  r2  logs/worker.log, its lines from 08:00 to 09:21: jumps (a key at 2 a minute that logs 60 in
      a minute, 60 again the next minute (the same jump), then 25 four minutes later (a new one);
      a median of 2.5 met exactly; a key whose hour before holds one active minute 60 minutes
      before), and keys just short of a jump (24 against a median of 2.5; 25 against a median of
      3; an active minute 61 minutes before); error bursts (20 error records of two keys in a
      minute, after a minute with 1; 25 with a stack trace after 5 empty minutes), and minutes
      just short of one (19 errors; 22 after a minute with 19; the minute after a burst)
  r3  ./fake-app --test at commit 2, exit 1 (failed): E1 twice; "slow query" once (rare: 3
      before); a new error key E3 "test_upload failed"
  r4  a made-up Claude session, 10:15 to 10:22, that edited src/db.py (which commit 3 changes,
      and E1's record names) and notes.txt (neither): written straight into the store's format,
      a run of kind session with its edited files in its catalog (--via sessions
      saves it with `sessions sync` instead)
  r5  ./fake-app --test at commit 3: no E1, so E1 stopped here (commit 3 in between, and the
      session in that time); E3 again (rare)
  r6  ./fake-app --test at commit 3, ended by signal 15 (failed; cut short, so E3's absence does
      not stop it)
  r7  logs/worker.log again, 09:30 to 09:40: the worker's three error keys stopped (no commit
      recorded for a file)
  r8  ./fake-app --test at commit 4 with changes not committed (dirty), exit 1: E1 came back
      (rare too: 3 before); "retry upload" rare; a new error key "payment failed"; E3 stopped
      (commit 4 in between, no session in that time)

The check keeps its own model of every record (lines, key by checks/msgkey_ref.py, level, time)
and of the rules (guard.rs), and checks:
1. moments.jsonl: every moment equals the model's, field by field (time, kind, run, line, key,
   count, the number the rule compared, where the key was seen, the stop, exit or signal, note);
2. guard: the full answers of several scopes equal the model's, line for line;
3. fixes: by text, by a reference to the error's line and to a frame inside its record, by a
   pasted line, for a key with a commit, files and the session in between, one with no session
   in its time, one with no commit recorded, no match, and a key that is not an error key: line
   for line; the commits and changed files are git's own (read by this check from git);
4. no answer claims a cause: no word of cause, fixing or blame outside the quoted log lines and
   commit subjects;
5. the cap and the "left out" line: with small caps and limits, every answer stays within
   max_tokens x 4 bytes before its last line and ends "all shown" or "left out: ... Next: ...",
   and following the Next calls gives every row of the full answer once, in order (a row too
   long for the cap names the call that shows it whole, which gives exactly the full row);
6. references: every line the answers showed reads back with `lines` and `expect` as "same bytes";
7. the tools change no file of the store; with keys.cat damaged, moments.jsonl changed, or both
   gone, the answers are the same and say the notes were made now; and a second store saved with
   its keys.cat deleted before the last run has the same keys.cat and moments.jsonl bytes.
--show prints every answer. --via sessions saves r4 for real: the session file goes into a
made-up Claude Code folder in WORK (FEWER_CLAUDE_DIR, every other agent off, HOME in WORK), the
store's config says `sessions = on`, and `fewer sessions sync` saves it.
"""
import calendar, collections, hashlib, importlib.util, json, os, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


mcpc = load("mcpc")
ref = load("msgkey_ref")

argv, VIA, SHOW = [], "add", False
it = iter(sys.argv[1:])
for x in it:
    if x == "--show":
        SHOW = True
    elif x == "--via":
        VIA = next(it, "")
    elif x.startswith("--via="):
        VIA = x[6:]
    else:
        argv.append(x)
if len(argv) != 3 or VIA not in ("add", "sessions"):
    raise SystemExit(__doc__)
SEARCH, PACK, WORK = (os.path.abspath(x) for x in argv)
mcpc.fresh_work(WORK)
# every process this check starts, the server too, gets a made-up HOME, agents' folders and
# claude, inside WORK (safe_env.py)
safe = load("safe_env").setup(WORK)
PROJ = WORK + "/proj"
os.makedirs(PROJ + "/src")
os.makedirs(PROJ + "/logs")
os.makedirs(WORK + "/out")
# Every process the check starts, the MCP server too, gets made-up folders for the home and
# for every agent's sessions (safe_env.py, and here the made-up Claude Code folder whose
# session `--via sessions` saves): with `sessions = on` in a store's config, the server's
# sessions sweep reads the agents' folders.
FAKE_HOME, CLAUDE_DIR = WORK + "/fakehome", WORK + "/claude"
os.makedirs(FAKE_HOME)
os.makedirs(CLAUDE_DIR + "/projects/-proj")
BASE_ENV = safe.env(FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ, HOME=FAKE_HOME, FEWER_CLAUDE_DIR=CLAUDE_DIR, CLAUDE_CONFIG_DIR=CLAUDE_DIR, FEWER_CODEX_DIR=FAKE_HOME, CODEX_HOME=FAKE_HOME, FEWER_CLINE_DIR=FAKE_HOME, FEWER_CURSOR_DIR=FAKE_HOME)
for k in ("FEWER_TEST_RUN_TAGS", "FEWER_NOW", "FEWER_TEST_SLOW_BLOCK_MS", "FEWER_HOME"):
    BASE_ENV.pop(k, None)
failures = []
stats = collections.Counter()


def bad(msg):
    failures.append(msg)
    if len(failures) <= 40:
        print("FAIL:", msg, flush=True)


# ---------------------------------------------------------------- the git repository

GIT_ENV = dict(BASE_ENV, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid", GIT_COMMITTER_NAME="fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid")


def git(*a, date=None):
    e = dict(GIT_ENV)
    if date:
        e["GIT_AUTHOR_DATE"] = e["GIT_COMMITTER_DATE"] = date
    r = subprocess.run(["git"] + list(a), cwd=PROJ, env=e, capture_output=True)
    if r.returncode != 0:
        raise SystemExit("git %s failed: %s" % (" ".join(a), r.stderr.decode()))
    return r.stdout.decode().strip()


def write(path, text):
    with open(os.path.join(PROJ, path), "w") as f:
        f.write(text)


git("init", "-q", "-b", "main")
open(PROJ + "/.gitignore", "w").write("logs/\n")
write("src/app.py", "def main():\n    connect()\n")
write("src/db.py", "def connect():\n    open_socket()\n")
write("src/worker.py", "def run():\n    pass\n")
write("README.md", "fake app\n")
# (author and committer dates fixed: git log's dates are known)
COMMITS = []  # (full hash, short hash, iso date, subject, files changed from the one before)
for files, subject, date in (
    (None, "initial: app, db and worker", "2026-11-17T09:00:00+0000"),
    (["src/app.py"], "app: add the upload page", "2026-11-17T10:05:00+0000"),
    (["src/db.py"], "fix db connection retry", "2026-11-17T10:24:00+0000"),
    (["README.md"], "readme: notes on the worker", "2026-11-17T10:44:00+0000"),
):
    if files is None:
        git("add", "-A")
    else:
        for f in files:
            open(os.path.join(PROJ, f), "a").write("# changed: %s\n" % subject)
        git("add", "--", *files)
    git("commit", "-q", "-m", subject, date=date)
    h = git("rev-parse", "HEAD")
    COMMITS.append((h, git("rev-parse", "--short", h), date.replace("T", " ").replace("+0000", " +0000"), subject, files or []))

# ---------------------------------------------------------------- the runs


def ms_of(h, m, s=0):
    return calendar.timegm((2026, 11, 17, h, m, s)) * 1000


def iso(ms):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ms // 1000))


class Text:
    """A run's records in order: each (lines, level, time in ms)."""

    def __init__(self):
        self.recs = []

    def rec(self, t, level, msg, more=()):
        self.recs.append(([("%s %s %s" % (iso(t), level, msg)).encode()] + [m.encode() for m in more], level, t))


CMD = ["./fake-app", "--test"]
E1_FRAMES = ["\tat app.db.connect(db.py:12)", "\tat app.main(app.py:40)"]


def test_run(k, t0):
    w = Text()
    t = [t0 + 1000]

    def r(level, msg, more=()):
        w.rec(t[0], level, msg, more)
        t[0] += 3000

    r("INFO", "app starting")
    if k == 1:
        r("INFO", "test_login passed in 12 ms")
        r("ERROR", "db: connection refused to 10.0.0.1:5432", E1_FRAMES)
        for n in range(3):
            r("WARN", "slow query took %d ms" % (250 + n))
        r("WARN", "retry upload 1")
        r("INFO", "1 passed, 0 failed")
    elif k == 3:
        r("ERROR", "db: connection refused to 10.0.0.3:5432", E1_FRAMES)
        r("ERROR", "db: connection refused to 10.0.0.4:5432", E1_FRAMES)
        r("WARN", "slow query took 260 ms")
        r("ERROR", "test_upload failed: AssertionError: expected 200, got 500")
        r("INFO", "0 passed, 1 failed")
    elif k == 5:
        r("INFO", "test_login passed in 12 ms")
        r("WARN", "slow query took 270 ms")
        r("ERROR", "test_upload failed: AssertionError: expected 200, got 500")
        r("INFO", "1 passed, 0 failed")
    elif k == 6:
        r("INFO", "test_login passed in 13 ms")
    elif k == 8:
        r("ERROR", "db: connection refused to 10.0.0.8:5432", E1_FRAMES)
        r("WARN", "retry upload 2")
        r("ERROR", "payment failed: card declined for order 5520")
        r("INFO", "0 passed, 1 failed")
    return w.recs


W0 = ms_of(8, 0)


def worker_piece1():
    """r2: per minute (from 08:00), the records of each key; the jump and burst cases."""
    per = collections.defaultdict(list)  # minute -> [(level, msg, more)]

    def add(m, n, level, msg, more=()):
        for i in range(n):
            per[m].append((level, msg % (i,) if "%" in msg else msg, more))

    for m in range(60):
        add(m, 2, "INFO", "GET /api/items 200 in 4%d ms")
        add(m, 3, "INFO", "cache refresh took 1%d ms")
    add(70, 60, "INFO", "GET /api/items 200 in 5%d ms")  # a jump (median 2)
    add(71, 60, "INFO", "GET /api/items 200 in 6%d ms")  # the same jump (the minute before met the rule)
    add(75, 25, "INFO", "GET /api/items 200 in 7%d ms")  # a new jump (minute 74 did not meet it)
    add(65, 25, "INFO", "cache refresh took 2%d ms")  # not a jump: 25 < 10 x 3
    add(30, 2, "INFO", "queue depth %d")
    add(31, 3, "INFO", "queue depth 1%d")
    add(40, 25, "INFO", "queue depth 2%d")  # a jump: 2 x 25 = 10 x (2 + 3)
    add(30, 2, "INFO", "lock wait %d ms")
    add(31, 3, "INFO", "lock wait 1%d ms")
    add(40, 24, "INFO", "lock wait 2%d ms")  # not a jump: 2 x 24 < 10 x 5
    add(10, 5, "INFO", "sync batch %d done")
    add(70, 50, "INFO", "sync batch 1%d done")  # a jump: minute 10 is in the hour before (10 to 69)
    add(9, 5, "INFO", "flush batch %d done")
    add(70, 50, "INFO", "flush batch 1%d done")  # not a jump: minute 9 is not
    job = "worker: job 7%d failed: timeout"
    for m in (5, 20, 40):
        add(m, 1, "ERROR", job)
    add(50, 19, "ERROR", job)  # not a burst: 19
    add(52, 22, "ERROR", job)  # not a burst: minute 50 had 19
    add(60, 1, "ERROR", job)
    add(63, 1, "ERROR", job)
    add(66, 10, "ERROR", job)
    add(66, 10, "ERROR", "worker: disk full")  # a burst: 20 error records of two keys, minute 63 had 1
    add(80, 24, "ERROR", job)
    add(80, 1, "ERROR", "worker crashed: java.lang.IllegalStateException: pool closed", ("\tat app.worker.run(worker.py:3)", "\tat app.main(app.py:41)"))  # a burst: 25
    add(81, 25, "ERROR", job)  # not a burst: minute 80 was one
    w = Text()
    for m in sorted(per):
        for i, (level, msg, more) in enumerate(per[m]):
            w.rec(W0 + m * 60_000 + i * 500, level, msg, more)
    return w.recs


def worker_piece2():
    w = Text()
    for m in range(90, 100):
        w.rec(W0 + m * 60_000, "INFO", "GET /api/items 200 in 40 ms")
    return w.recs


SESSION_ID = "7f0c2a1e-5b6d-4c3a-9e8f-0123456789ab"


def session_lines():
    """r4: a made-up Claude Code session (10:15 to 10:22) that edits src/db.py and notes.txt."""
    def line(t, typ, content):
        return json.dumps({"type": typ, "sessionId": SESSION_ID, "cwd": PROJ, "message": {"role": typ, "content": content}, "timestamp": iso(t)[:-1] + ".000Z"}, separators=(",", ":")).encode()

    edit = lambda path, i: [{"type": "tool_use", "id": "toolu_%d" % i, "name": "Edit", "input": {"file_path": path, "old_string": "open_socket()", "new_string": "retry(open_socket)"}}]
    return [
        line(ms_of(10, 15), "user", "the db connection is refused in the tests"),
        line(ms_of(10, 16), "assistant", [{"type": "text", "text": "Let me look at db.py."}]),
        line(ms_of(10, 18), "assistant", edit(PROJ + "/src/db.py", 1)),
        line(ms_of(10, 19), "user", [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}]),
        line(ms_of(10, 21), "assistant", edit(PROJ + "/notes.txt", 2)),
        line(ms_of(10, 22), "user", [{"type": "tool_result", "tool_use_id": "toolu_2", "content": "ok"}]),
    ]


# (id, what, tags): the eight runs, in the order they are saved
RUNS = [
    (1, "test", 1, dict(kind="run", cmd=CMD, exit=0, commit=0, start=ms_of(10, 0), end=ms_of(10, 1))),
    (2, "worker", 1, dict(kind="add", start=ms_of(10, 6), end=ms_of(10, 6, 2))),
    (3, "test", 3, dict(kind="run", cmd=CMD, exit=1, commit=1, start=ms_of(10, 10), end=ms_of(10, 11))),
    (4, "session", 0, dict(kind="session", agent="claude", session=SESSION_ID, start=ms_of(10, 15), end=ms_of(10, 22))),
    (5, "test", 5, dict(kind="run", cmd=CMD, exit=0, commit=2, start=ms_of(10, 30), end=ms_of(10, 31))),
    (6, "test", 6, dict(kind="run", cmd=CMD, signal=15, commit=2, start=ms_of(10, 40), end=ms_of(10, 40, 20))),
    (7, "worker", 2, dict(kind="add", start=ms_of(10, 45), end=ms_of(10, 45, 2))),
    (8, "test", 8, dict(kind="run", cmd=CMD, exit=1, commit=3, dirty=True, start=ms_of(10, 50), end=ms_of(10, 51))),
]


def read_runs(pdir):
    runs = {}
    for line in open(os.path.join(pdir, "runs.jsonl"), "rb"):
        o = json.loads(line)
        runs.setdefault(o["id"], {}).update(o)
    return runs


def body_of(recs):
    return [l + b"\n" for lines, _, _ in recs for l in lines]


def saved_ids(out):
    ids = []
    for line in out.splitlines():
        m = re.match(r"saved (.*) as (\S+)/r(\d+): ", line)
        if m:
            ids.append((m.group(2), int(m.group(3))))
    return ids


def make_store(home, drop_registry_before=None):
    """Save the eight runs into the store at `home`; the model of each: id -> dict."""
    env = dict(BASE_ENV, FEWER_HOME=home)
    model, pid = {}, None
    for rid, what, k, tags in RUNS:
        tags = dict(tags)
        if "commit" in tags:
            tags["commit"] = COMMITS[tags["commit"]][0]
            tags["branch"] = "main"
        if what == "test":
            recs = test_run(k, tags["start"])
            path = WORK + "/out/run%d.log" % rid
            src = None
        elif what == "worker":
            recs = worker_piece1() if k == 1 else worker_piece2()
            path = PROJ + "/logs/worker.log"
            src = path
        else:
            lines = session_lines()
            recs = [([l], "", json.loads(l)["timestamp"]) for l in lines]
            recs = [(ls, lv, calendar.timegm(time.strptime(t[:19], "%Y-%m-%dT%H:%M:%S")) * 1000) for ls, lv, t in recs]
            path = WORK + "/out/session.jsonl"
            src = path
            tags["files"] = [[PROJ + "/src/db.py", 3], [PROJ + "/notes.txt", 5]]
        with open(path, "wb") as f:
            f.write(b"".join(body_of(recs)))
        if drop_registry_before == rid and pid:
            os.remove(os.path.join(home, "projects", pid, "keys.cat"))
        if what == "session" and VIA == "sessions":
            # saved for real by the sessions sweep, from the made-up Claude Code folder (every
            # process here has FEWER_CLAUDE_DIR and HOME in WORK: BASE_ENV)
            shutil.copy(path, "%s/projects/-proj/%s.jsonl" % (CLAUDE_DIR, SESSION_ID))
            with open(os.path.join(home, "config"), "a") as f:
                f.write("sessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n")
            r = subprocess.run([SEARCH, "sessions", "sync"], env=env, cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
            if b"unknown command sessions" in r.stderr:
                raise SystemExit("this fewer has no `sessions` command: use --via add")
            runs = read_runs(os.path.join(home, "projects", pid))
            if r.returncode != 0 or max(runs) != rid or runs[rid].get("kind") != "session" or runs[rid].get("state") != "ready":
                raise SystemExit("sessions sync did not save the session as r%d (%d): %s %s" % (rid, r.returncode, r.stdout.decode(), r.stderr.decode()))
            model[rid] = {"recs": recs, "body": body_of(recs), "tags": tags, "src": runs[rid].get("source"), "what": what}
            continue
        r = subprocess.run([SEARCH, "add", path], env=dict(env, FEWER_TEST_RUN_TAGS=json.dumps(tags)), cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
        got = saved_ids(r.stdout.decode())
        if r.returncode != 0 or len(got) != 1 or got[0][1] != rid:
            raise SystemExit("add %s failed (%d): %s %s" % (path, r.returncode, r.stdout.decode(), r.stderr.decode()))
        pid = got[0][0]
        model[rid] = {"recs": recs, "body": body_of(recs), "tags": tags, "src": src, "what": what}
    return pid, model


HOME = WORK + "/home"
PID, model = make_store(HOME)
PDIR = os.path.join(HOME, "projects", PID)
env = dict(BASE_ENV, FEWER_HOME=HOME)


RUNREC = read_runs(PDIR)
for rid in model:
    model[rid]["run"] = RUNREC[rid]
    if RUNREC[rid].get("state") != "ready":
        bad("r%d is %s" % (rid, RUNREC[rid].get("state")))

# ---------------------------------------------------------------- the model of the rules

KINDS = ["new", "rare", "jump", "burst", "failed", "back"]


def records_of(rid):
    """Each record: (first line, last line, key, error, minute, ms, run start)."""
    m, out, n = model[rid], [], 1
    for lines, level, t in m["recs"]:
        key = ref.line_key(lines[0] + b"\n")
        out.append((n, n + len(lines) - 1, key, level == "ERROR" or len(lines) > 1, t // 60_000))
        n += len(lines)
    return out


def source_of(rid):
    r = model[rid]["run"]
    if "cmd" in r:
        return "cmd " + json.dumps(r["cmd"], separators=(",", ":"))
    return "file " + r["source"] if "source" in r else None


def cut_short(rid):
    r = model[rid]["run"]
    n = r.get("note", "")
    return "signal" in r or "ended without a status" in n or "saving stopped" in n or n.startswith("could not start")


def failed(rid):
    r = model[rid]["run"]
    return "signal" in r or r.get("exit", 0) != 0


def records_text(n):
    return "1 record" if n == 1 else "%d records" % n


def median(two):
    return str(two // 2) if two % 2 == 0 else "%d.5" % (two // 2)


def note_text(n):
    k = n["kind"]
    if k == "new":
        return "first seen in the project: %s in this run" % records_text(n["count"])
    if k == "rare":
        return "%s in this run; seen before: %s in the project" % (records_text(n["count"]), records_text(n["before"]))
    if k == "jump":
        return "%s in one minute, 10 or more times its median per active minute in the hour before (%s)" % (records_text(n["count"]), median(n["before"]))
    if k == "burst":
        return "%d error records in one minute, after 5 minutes with fewer than 2 a minute (%d in all)" % (n["count"], n["before"])
    if k == "failed":
        return "the run ended with signal %d" % n["signal"] if "signal" in n else "the run ended with exit status %d" % n["exit"]
    return "%s in this run after it had stopped in run %d" % (records_text(n["count"]), n["stop"]["run"])


def index_moments(rid, recs):
    """jump and burst, as the index pass finds them (guard.rs's rules, written again)."""
    out = []
    per_key = collections.OrderedDict()  # key -> {minute: [records, first line]}
    errors = {}  # minute -> [records, first line, key]
    for first, last, key, err, minute in recs:
        d = per_key.setdefault(key, {})
        e = d.setdefault(minute, [0, first])
        e[0] += 1
        if err:
            e2 = errors.setdefault(minute, [0, first, key])
            e2[0] += 1
    for key, d in per_key.items():
        mins = sorted(d)
        met = None
        for i, m in enumerate(mins):
            n = d[m][0]
            before = [d[x][0] for x in mins[:i] if m - 60 <= x < m]
            if n < 20 or not before:
                continue
            before.sort()
            two = before[(len(before) - 1) // 2] + before[len(before) // 2]
            if 2 * n >= 10 * two:
                if met != m - 1:
                    out.append({"kind": "jump", "minute": m, "key": key, "count": n, "line": d[m][1], "before": two})
                met = m
    for m in sorted(errors):
        n, line, key = errors[m]
        if n < 20:
            continue
        before = [errors[x][0] for x in range(m - 5, m) if x in errors]
        if all(c < 2 for c in before):
            out.append({"kind": "burst", "minute": m, "key": key, "count": n, "line": line, "before": sum(before)})
    return out


def derive():
    """Every moment, and the registry with its stops, run by run in the order of ids."""
    reg = {}  # key -> dict(first, last, latest, runs, records, error, stops)
    notes = []
    for rid in sorted(model):
        recs = records_of(rid)
        r = model[rid]["run"]
        start = r["start"]
        seen = lambda minute, line: (minute * 60_000, rid, line)
        keys = collections.OrderedDict()  # key -> [records, first seen, last seen, error]
        for first, last, key, err, minute in recs:
            e = keys.setdefault(key, [0, seen(minute, first), None, False])
            e[0] += 1
            e[2] = seen(minute, first)
            e[3] |= err
        run_notes = []
        for key, (n, first, _, _) in keys.items():
            note = {"t": first[0], "run": rid, "line": first[2], "key": key.decode(), "count": n}
            k = reg.get(key)
            if k is None:
                run_notes.append(dict(note, kind="new"))
                continue
            if k["records"] <= 3:
                run_notes.append(dict(note, kind="rare", before=k["records"], seen={"t": k["first"][0], "run": k["first"][1], "line": k["first"][2]}))
            if k["stops"] and k["stops"][-1]["back"] is None:
                s = k["stops"][-1]
                s["back"] = first
                run_notes.append(dict(note, kind="back", seen={"t": s["last"][0], "run": s["last"][1], "line": s["last"][2]}, stop={"t": s["ms"], "run": s["run"]}))
        if source_of(rid) is not None and not cut_short(rid):
            first_ms = min(t for _, _, t in model[rid]["recs"])
            for key, k in reg.items():
                stopped = k["stops"] and k["stops"][-1]["back"] is None
                if not k["error"] or stopped or k["latest"][1] >= rid or key in keys:
                    continue
                if source_of(k["latest"][1]) == source_of(rid):
                    k["stops"].append({"last": k["latest"], "run": rid, "ms": first_ms, "back": None})
        if failed(rid):
            errs = sum(1 for x in recs if x[3])
            f = {"t": r.get("end", start), "kind": "failed", "run": rid, "line": len(model[rid]["body"]), "key": "", "count": errs}
            f.update({"signal": r["signal"]} if "signal" in r else {"exit": r["exit"]})
            run_notes.append(f)
        for m in index_moments(rid, recs):
            run_notes.append({"t": m["minute"] * 60_000, "kind": m["kind"], "run": rid, "line": m["line"], "key": m["key"].decode(), "count": m["count"], "before": m["before"]})
        run_notes.sort(key=lambda n: (n["line"], KINDS.index(n["kind"])))
        for n in run_notes:
            n["note"] = note_text(n)
        notes += run_notes
        for key, (n, first, last, err) in keys.items():
            k = reg.get(key)
            if k is None:
                reg[key] = {"first": first, "last": last, "latest": last, "runs": 1, "records": n, "error": err, "stops": []}
            else:
                k["first"] = min(k["first"], first)
                k["last"] = max(k["last"], last)
                if (last[1], last[2]) > (k["latest"][1], k["latest"][2]):
                    k["latest"] = last
                k["runs"] += 1
                k["records"] += n
                k["error"] |= err
    return reg, notes


REG, NOTES = derive()

# ---------------------------------------------------------------- 1. moments.jsonl

print("1. moments.jsonl: %d moments in the model (%s)" % (len(NOTES), ", ".join("%s %d" % (k, sum(1 for n in NOTES if n["kind"] == k)) for k in KINDS)), flush=True)
got = [json.loads(l) for l in open(os.path.join(PDIR, "moments.jsonl"), "rb") if l.strip()]
if len(got) != len(NOTES):
    bad("moments.jsonl holds %d moments, the model %d" % (len(got), len(NOTES)))
for i, (g, w) in enumerate(zip(got, NOTES)):
    if g != w:
        bad("moment %d differs:\n  got  %s\n  want %s" % (i + 1, json.dumps(g), json.dumps(w)))
    stats["moments compared"] += 1
# what the fixture was made to show, each at least once
want_kinds = {"new", "rare", "jump", "burst", "failed", "back"}
if {n["kind"] for n in NOTES} != want_kinds:
    bad("the model lacks kinds: %s" % (want_kinds - {n["kind"] for n in NOTES}))
jumps = [(n["run"], (n["t"] - W0) // 60_000, n["key"]) for n in NOTES if n["kind"] == "jump"]
bursts = [(n["run"], (n["t"] - W0) // 60_000, n["count"], n["before"]) for n in NOTES if n["kind"] == "burst"]
# (the worker's error key jumps too: 22 in minute 52, and 24 in minute 80, against a median of 1)
expect_jumps = [(2, 40, "<time> INFO queue depth <num>"), (2, 52, "<time> ERROR worker: job <num> failed: timeout"), (2, 70, "<time> INFO GET <path> <num> in <num> ms"), (2, 70, "<time> INFO sync batch <num> done"), (2, 75, "<time> INFO GET <path> <num> in <num> ms"), (2, 80, "<time> ERROR worker: job <num> failed: timeout")]
if sorted(jumps) != sorted(expect_jumps):
    bad("jumps %s, the fixture made %s" % (jumps, expect_jumps))
if sorted(bursts) != [(2, 66, 20, 1), (2, 80, 25, 0)]:
    bad("bursts %s" % bursts)
stops = {k.decode(): [(s["last"][1], s["run"], s["back"][1] if s["back"] else None) for s in v["stops"]] for k, v in REG.items() if v["stops"]}
expect_stops = {
    "<time> ERROR db: connection refused to <ip>:<num>": [(3, 5, 8)],
    "<time> ERROR test_upload failed: AssertionError: expected <num>, got <num>": [(5, 8, None)],
    "<time> ERROR worker: job <num> failed: timeout": [(2, 7, None)],
    "<time> ERROR worker: disk full": [(2, 7, None)],
    "<time> ERROR worker crashed: java.lang.IllegalStateException: pool closed": [(2, 7, None)],
}
if stops != expect_stops:
    bad("stops %s, the fixture made %s" % (stops, expect_stops))

# ---------------------------------------------------------------- the answers as the model gives them


def group(n):
    return "{:,}".format(n)


def plural(n, w):
    return "1 %s" % w if n == 1 else "%s %ss" % (group(n), w)


def local(ms):
    return time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(ms // 1000))


def minute_text(ms):
    return local(ms)[:16]


def minute_span(a, b):
    fa, fb = local(a * 60_000), local(b * 60_000)
    if a == b:
        return fa[:16]
    return "%s to %s" % (fa[:16], fb[11:16] if fa[:10] == fb[:10] else fb[:16])


def quoted(key):
    return json.dumps(key, ensure_ascii=False)


def lref(rid, line):
    return "%s/r%d:%d" % (PID, rid, line)


def rref(rid):
    return "%s/r%d" % (PID, rid)


def shown(rid, line):
    return "%s %s" % (lref(rid, line), mcpc.shown(model[rid]["body"][line - 1]))


def what(rid):
    r = model[rid]["run"]
    return " ".join(r["cmd"]) if "cmd" in r else r.get("source", "")


def at_commit(rid):
    r = model[rid]["run"]
    if "commit" not in r:
        return ""
    s = "%s@%s" % (r["branch"], r["commit"][:9]) if "branch" in r else "commit %s" % r["commit"][:9]
    return s + (" (dirty)" if r.get("dirty") else "")


def words(rid):
    r = model[rid]["run"]
    s = r["kind"] + (" " + what(rid) if what(rid) else "")
    if at_commit(rid):
        s += ", " + at_commit(rid)
    if "exit" in r:
        s += ", exit %d" % r["exit"]
    if "signal" in r:
        s += ", signal %d" % r["signal"]
    return s


TOK = set("abcdefghijklmnopqrstuvwxyz0123456789-")


def strip_pid(text):
    """The answer's references to this project's runs without the project, as the server shows
    them when a call names no project (the token test of 2026-11-23): only when every
    "<project>/r<digit>" in the answer is this project's; then each such "<project>/" is taken
    out."""
    cut = []
    for m in re.finditer(r"/r(?=\d)", text):
        p = st = m.start()
        while st > 0 and text[st - 1] in TOK:
            st -= 1
        if st == p:
            continue
        if text[st:p] != PID:
            return text
        cut.append((st, p + 1))
    out, last = [], 0
    for st, e in cut:
        out.append(text[last:st])
        last = e
    out.append(text[last:])
    return "".join(out)


def as_shown(a, want):
    """The model's answer (heading lines, section headings, rows) as the server shows it for
    the call `a`: this project's references without the project when `a` names no project."""
    if "project" in a:
        return want
    head, heads, rows = want
    return [strip_pid(l) for l in head], [strip_pid(h) for h in heads], [(sec, [strip_pid(l) for l in lines]) for sec, lines in rows]


def scope_desc(kind=None):
    return PID if kind in (None, "logs") else "%s, kind %s" % (PID, kind)


def note_rows(n):
    at = minute_text(n["t"])
    key = quoted(n["key"])
    rf = rref(n["run"])
    k = n["kind"]
    if k == "new":
        head = "%s new: %s: first seen in the project, %s in %s" % (at, key, plural(n["count"], "record"), rf)
    elif k == "rare":
        s = n["seen"]
        head = "%s rare: %s: %s in %s; %s before it in the project, the first %s at %s" % (at, key, plural(n["count"], "record"), rf, plural(n["before"], "record"), minute_text(s["t"]), lref(s["run"], s["line"]))
    elif k == "jump":
        head = "%s jump: %s: %s in this minute in %s; its median was %s a minute in the hour before (the minutes it logged in)" % (at, key, plural(n["count"], "record"), rf, median(n["before"]))
    elif k == "burst":
        head = "%s burst: %s in this minute in %s, after 5 minutes with fewer than 2 a minute (%s in them)" % (at, plural(n["count"], "error record"), rf, group(n["before"]))
    elif k == "failed":
        how = "signal %d" % n["signal"] if "signal" in n else "exit %d" % n["exit"]
        head = "%s failed: %s %s ended with %s (%s)" % (at, rf, what(n["run"]), how, plural(n["count"], "error record"))
    else:
        s, st = n["seen"], n["stop"]
        tags = ", " + at_commit(st["run"]) if at_commit(st["run"]) else ""
        head = "%s back: %s came back: %s in %s; it had stopped in %s (%s%s), last seen before at %s (%s). What came right before it stopped: fixes %s" % (
            at, key, plural(n["count"], "record"), rf, rref(st["run"]), minute_text(st["t"]), tags, lref(s["run"], s["line"]), minute_text(s["t"]), json.dumps({"ref": lref(n["run"], n["line"])}, separators=(",", ":")))
    return [head, "  " + shown(n["run"], n["line"])]


def want_guard(kinds=None, runs=None, kind=None, since=None, until=None):
    """The full guard answer: (heading lines, [], rows). With `runs`, `kind` only picks the
    runs (the calls name runs and no kind: the heading names no kind)."""
    ids = [rid for rid in model if (kind == "all" or kind == "sessions" and model[rid]["run"]["kind"] == "session" or kind in (None, "logs") and model[rid]["run"]["kind"] != "session")]
    if runs:
        ids = [rid for rid in ids if rid in runs]
    if since is not None or until is not None:
        # the scope: runs whose lines' times meet the window; then moments whose minute does
        def meets(rid):
            r = model[rid]["run"]
            return not (since is not None and r["last"] < since or until is not None and r["first"] > until)
        ids = [rid for rid in ids if meets(rid)]
    ns = [n for n in NOTES if n["run"] in ids and (not kinds or n["kind"] in kinds)]
    if since is not None:
        ns = [n for n in ns if n["t"] // 60_000 >= since // 60_000]
    if until is not None:
        ns = [n for n in ns if n["t"] // 60_000 <= until // 60_000]
    ns.sort(key=lambda n: (-n["t"], -n["run"], n["line"], KINDS.index(n["kind"])))
    per = ", ".join("%s %s" % (k, group(c)) for k in KINDS for c in [sum(1 for n in ns if n["kind"] == k)] if c) or "none"
    only = ", kinds " + ", ".join(kinds) if kinds else ""
    head = ["Guard notes, newest first: %s in %s (%s%s): %s" % (plural(len(ns), "moment"), plural(len(ids), "run"), scope_desc(None if runs else kind), only, per)]
    return head, [], [(0, note_rows(n)) for n in ns]


SECTION_GIT = {}


def git_between(a, b):
    """The check's own reading of git: (log lines, count, files) between two commits."""
    ia = [c[0] for c in COMMITS].index(a)
    ib = [c[0] for c in COMMITS].index(b)
    between = COMMITS[ia + 1: ib + 1]
    log = ["%s %s %s" % (c[1], c[2], c[3]) for c in reversed(between)]
    files = sorted({f for c in between for f in c[4]})
    return log, len(between), files


def sessions_between(lo, hi):
    out = []
    for rid in sorted(model):
        r = model[rid]["run"]
        if r["kind"] != "session":
            continue
        a, b = r.get("first", r["start"]), r.get("last", r.get("end", r["start"]))
        if a <= hi and b >= lo:
            out.append(rid)
    return out


def record_text(rid, key):
    """The key's first record in run rid (a whole stack trace): its bytes."""
    for first, last, k, err, minute in records_of(rid):
        if k == key:
            return b"".join(model[rid]["body"][first - 1:last])
    return b""


def names_file(text, name):
    i = 0
    word = lambda c: chr(c).isalnum() and c < 0x80 or c == ord("_") or c >= 0x80
    while True:
        p = text.find(name, i)
        if p < 0:
            return False
        s, e = p, p + len(name)
        if (s == 0 or not word(text[s - 1])) and (e == len(text) or not word(text[e])):
            return True
        i = s + 1


def stop_lines(key, k, j):
    s = k["stops"][j]
    last, run = s["last"], s["run"]
    out = ["stop %d of %d: last seen %s at %s (%s); not in %s, %s (%s)" % (j + 1, len(k["stops"]), minute_text(last[0]), lref(last[1], last[2]), words(last[1]), rref(run), minute_text(s["ms"]), words(run))]
    ra, rb = model[last[1]]["run"], model[run]["run"]
    changed = []
    if "commit" in ra and "commit" in rb:
        a, b = ra["commit"], rb["commit"]
        if a == b:
            out.append("  no commit between them: both at %s" % a[:9])
        else:
            log, count, files = git_between(a, b)
            out.append("  %s between %s and %s, newest first:" % (plural(count, "commit"), a[:9], b[:9]))
            out += ["    " + l for l in log]
            out.append("  files changed between them (%d): %s" % (len(files), ", ".join(files)) if files else "  no file changed between them")
            changed = files
    else:
        out.append("  commits between: not known: %s has no commit (saved outside a git repository, or an added file)" % rref(last[1] if "commit" not in ra else run))
    if rb.get("dirty"):
        out.append("  %s ran with changes not committed yet (dirty)" % rref(run))
    record = record_text(last[1], key)
    sess = sessions_between(last[0], s["ms"])
    window = minute_span(last[0] // 60_000, s["ms"] // 60_000)
    if not sess:
        out.append("  no saved session in that time (%s)" % window)
    else:
        out.append("  %s in that time (%s):" % (plural(len(sess), "saved session"), window))
    for rid in sess:
        r = model[rid]["run"]
        files = model[rid]["tags"]["files"]
        out.append("    %s %s session %s, %s; it edited %s:" % (rref(rid), r["agent"], r["session"], minute_span(r["first"] // 60_000, r["last"] // 60_000), plural(len(files), "file")))
        rows = []
        for i, (path, line) in enumerate(files):
            relp = path[len(PROJ) + 1:] if path.startswith(PROJ + "/") else path
            marks = []
            if relp in changed:
                marks.append("those commits changed it")
            if names_file(record, path.rsplit("/", 1)[-1].encode()):
                marks.append("the error's record names it")
            rows.append((not marks, i, "      %s at %s%s" % (relp, lref(rid, line), " (%s)" % "; ".join(marks) if marks else "")))
        out += [x[2] for x in sorted(rows)]
    if s["back"]:
        b = s["back"]
        out.append("  came back %s at %s (%s)" % (minute_text(b[0]), lref(b[1], b[2]), words(b[1])))
    return out


def now_lines(k):
    l = k["latest"]
    if k["stops"] and k["stops"][-1]["back"] is None:
        s = k["stops"][-1]
        head = "now: not seen since it stopped in %s (%s); last seen %s at %s" % (rref(s["run"]), minute_text(s["ms"]), minute_text(l[0]), lref(l[1], l[2]))
    else:
        head = "now: last seen %s at %s; it has not stopped since (no later run of the same command or file without it)" % (minute_text(l[0]), lref(l[1], l[2]))
    return [head, "  " + shown(l[1], l[2])]


def want_fixes(keys, head):
    """keys: the error keys covered, in answer order (the most recently seen first)."""
    heads, rows = [], []
    for i, key in enumerate(keys):
        k = REG[key]
        backs = sum(1 for s in k["stops"] if s["back"])
        heads.append("== %s: %s in %s, first seen %s at %s, last seen %s at %s; stopped %s, came back %s" % (
            quoted(key.decode()), plural(k["records"], "record"), plural(k["runs"], "run"), minute_text(k["first"][0]), lref(k["first"][1], k["first"][2]),
            minute_text(k["last"][0]), lref(k["last"][1], k["last"][2]), plural(len(k["stops"]), "time"), plural(backs, "time")))
        for j in range(len(k["stops"])):
            rows.append((i, stop_lines(key, k, j)))
        rows.append((i, now_lines(k)))
    return head, heads, rows


def error_keys_holding(text, icase=False):
    t = text.encode()
    alt = alt_key(text)
    hold = lambda k, n: (k.lower().find(n.lower()) if icase else k.find(n)) >= 0
    keys = [k for k, v in REG.items() if v["error"] and (hold(k, t) or alt is not None and hold(k, alt))]
    return sorted(keys, key=lambda k: (tuple(-x for x in REG[k]["last"]), k))


def alt_key(text):
    k = ref.line_key(text.encode())
    if k == text.encode():
        return None
    rest = k.decode("utf-8", "replace")
    for ph in ("<num>", "<ip>", "<time>", "<uuid>", "<url>", "<path>", "<str>", "<long>"):
        rest = rest.replace(ph, " ")
    return k if any(len(w) >= 3 for w in re.split(r"[^A-Za-z]", rest)) else None


def fixes_text_head(text, keys, icase=False):
    alt = alt_key(text)
    also = " (or, as keys write it, %s)" % quoted(alt.decode()) if alt is not None else ""
    if not keys:
        n = sum(1 for v in REG.values() if v["error"])
        return ["fixes for %s%s (%s): no error key holds it, of %s. Keys write values as <num>, <ip>, <path>, <time>: give words of the message, or a ref to one of its lines; errors lists the error keys" % (quoted(text), also, PID, plural(n, "error key"))]
    return ["fixes for %s%s (%s): %s %s it; each stop shows what came right before it, oldest first" % (quoted(text), also, PID, plural(len(keys), "error key"), "holds" if len(keys) == 1 else "hold")]


# ---------------------------------------------------------------- comparing answers

srv = None
shown_refs = set()
answers = []  # every answer text, for the cause check


def call(name, a):
    t, err = srv.call(name, a)
    stats["calls"] += 1
    answers.append((name, a, t))
    if SHOW:
        print("\n>>> %s %s\n%s" % (name, json.dumps(a), t), flush=True)
    return t, err


def note_refs(text):
    for m in re.finditer(r"(?<![a-z0-9/-])(?:%s/)?r(\d+):(\d+)" % re.escape(PID), text):
        shown_refs.add((int(m.group(1)), int(m.group(2))))


def flat(head, heads, rows, notes=()):
    """The full answer; its last line names the lines cut at line_max (400), as the tools do."""
    out, sec = list(head[:1]) + list(notes) + list(head[1:]), None
    for s, lines in rows:
        if s != sec and s < len(heads) and heads[s]:
            out.append(heads[s])
        sec = s
        out += lines
    cut = [l for l in out if re.search(r"%s/r\d+:\d+ .*…\(\+[\d,]+ bytes\)$" % re.escape(PID), l)]
    last = "all shown"
    if cut:
        first = re.search(r"%s/r\d+:\d+" % re.escape(PID), cut[0]).group(0)
        last += "; %s cut at 400 bytes (line_max; one whole: lines %s)" % (plural(len(cut), "line"), json.dumps({"ref": first, "line_max": 65536}, separators=(",", ":")))
    return "\n".join(out + [last])


def same_text(what, got, want):
    stats["answers compared"] += 1
    if got == want:
        return True
    g, w = got.split("\n"), want.split("\n")
    for i in range(max(len(g), len(w))):
        if i >= len(g) or i >= len(w) or g[i] != w[i]:
            bad("%s: line %d differs:\n  got  %r\n  want %r" % (what, i + 1, g[i] if i < len(g) else None, w[i] if i < len(w) else None))
            return False
    return False


def full(name, a, want, notes=()):
    head, heads, rows = want
    t, err = call(name, dict(a, max_tokens=20000, limit=500))
    if err:
        bad("%s %s: isError: %s" % (name, json.dumps(a), t[:300]))
        return
    want_text = flat(head, heads, rows, notes)
    if same_text("%s %s" % (name, json.dumps(a)), t, want_text if "project" in a else strip_pid(want_text)):
        note_refs(t)
        stats["rows compared"] += len(rows)


def cut_of(got, want):
    if got == want:
        return True
    m = re.match(r"^(.*)…\(\+([\d,]+) bytes\)$", got, re.S)
    if not m:
        return False
    keep, n = m.group(1).encode(), int(m.group(2).replace(",", ""))
    return want.encode().startswith(keep) and len(want.encode()) - len(keep) == n


ROW_START = {
    "guard": lambda l: re.match(r"^\d{4}-\d\d-\d\d \d\d:\d\d (new|rare|jump|burst|failed|back): ", l),
    "fixes": lambda l: re.match(r"^(stop \d+ of \d+: |now: )", l),
}
MORE = re.compile(r"^  \(\+([\d,]+) more lines: fixes (\{.*\})\)$")


def row_ok(name, got, want):
    """A row of a small answer against the full answer's: the same lines (the first ones maybe
    cut by the cap), or, for a row too long for the cap, its first lines and the call that shows
    it whole, which must give exactly the full row."""
    if len(got) == len(want) and all(cut_of(g, w) for g, w in zip(got, want)):
        return True
    m = MORE.match(got[-1]) if got else None
    if not m:
        return False
    n = int(m.group(1).replace(",", ""))
    if not all(cut_of(g, w) for g, w in zip(got[:-1], want)) or len(got) - 1 + n != len(want):
        return False
    t, err = call("fixes", json.loads(m.group(2)))
    lines = t.split("\n")
    start = [i for i, l in enumerate(lines) if ROW_START["fixes"](l)]
    stats["rows too long, shown whole by their call"] += 1
    return not err and start and lines[start[0]:start[0] + len(want)] == want


def follow(name, a, want, max_tokens, limit):
    head, heads, rows = as_shown(a, want)
    want_rows = [lines for _, lines in rows]
    got_rows, calls = [], 0
    args = dict(a, max_tokens=max_tokens, limit=limit)
    while True:
        calls += 1
        if calls > 300:
            bad("%s %s: more than 300 calls" % (name, json.dumps(a)))
            return
        t, err = call(name, args)
        if err:
            bad("%s %s: isError: %s" % (name, json.dumps(args), t[:300]))
            return
        lines = t.split("\n")
        last = lines[-1]
        size = len("\n".join(lines[:-1]).encode())
        if size > max_tokens * 4:
            bad("%s %s: %d bytes before its last line, over %d" % (name, json.dumps(args), size, max_tokens * 4))
        stats["small answers"] += 1
        if not (last.startswith("all shown") or last.startswith("left out: ")):
            bad("%s %s: last line %r" % (name, json.dumps(args), last))
            return
        body = lines[:-1]
        if len(body) < len(head) or not all(cut_of(g, w) for g, w in zip(body[:len(head)], head)):
            bad("%s %s: its heading %r is not %r" % (name, json.dumps(args), body[:len(head)], head))
            return
        cur = None
        for l in body[len(head):]:
            if ROW_START[name](l):
                cur = [l]
                got_rows.append(cur)
            elif cur is not None and l.startswith("  "):
                cur.append(l)
            elif not any(cut_of(l, h) for h in heads):
                bad("%s %s: a line that is no row and no heading: %r" % (name, json.dumps(args), l))
        if last.startswith("all shown"):
            break
        m = re.search(r"Next: %s (\{.*\})$" % name, last)
        if not m:
            bad("%s: no Next call in %r" % (name, last))
            return
        args = json.loads(m.group(1))
        if args.get("max_tokens") != max_tokens or args.get("limit") != limit:
            bad("%s: the Next call changed the cap or the limit: %s" % (name, m.group(1)))
    if len(got_rows) != len(want_rows) or not all(row_ok(name, g, w) for g, w in zip(got_rows, want_rows)):
        for k in range(max(len(got_rows), len(want_rows))):
            if k >= len(got_rows) or k >= len(want_rows) or not row_ok(name, got_rows[k], want_rows[k]):
                bad("%s %s (max_tokens %d, limit %d): row %d is %r, the full answer's %r" % (name, json.dumps(a), max_tokens, limit, k, got_rows[k] if k < len(got_rows) else None, want_rows[k] if k < len(want_rows) else None))
                break
    else:
        stats["cursors followed"] += 1


def check(name, a, want, caps=((200, 500), (400, 3), (20000, 1), (900, 7))):
    full(name, a, want)
    for mt, lim in caps:
        follow(name, a, want, mt, lim)


def store_state(home):
    out = {}
    for root, _, files in os.walk(home):
        for f in files:
            p = os.path.join(root, f)
            st = os.stat(p)
            out[os.path.relpath(p, home)] = (st.st_size, st.st_mtime_ns, hashlib.sha256(open(p, "rb").read()).hexdigest())
    return out


# (the server takes the project out of an answer's references only when every
# "<project>/r<digit>" in it is this project's: no fixture line looks like one, so every
# answer here has it taken out, and the model's lines can be compared one by one)
for rid in model:
    for l in model[rid]["body"]:
        if re.search(rb"[a-z0-9-]/r\d", l if isinstance(l, bytes) else l.encode()):
            bad("r%d: a fixture line looks like a run reference: %r" % (rid, l))
before_tools = store_state(HOME)
srv = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
srv.initialize()

# ---------------------------------------------------------------- 2. guard

print("2. guard", flush=True)
GUARD = (
    ({}, want_guard()),
    ({"kind": "all"}, want_guard(kind="all")),
    ({"kinds": ["burst", "jump"]}, want_guard(kinds=["burst", "jump"])),
    ({"kinds": ["back", "failed"]}, want_guard(kinds=["back", "failed"])),
    ({"runs": ["r8"]}, want_guard(runs=[8], kind="all")),
    ({"runs": ["r2", "r7"], "kinds": ["new", "rare"]}, want_guard(runs=[2, 7], kind="all", kinds=["new", "rare"])),
    ({"since": "2026-11-17T09:00:00Z", "until": "2026-11-17T09:15:00Z"}, want_guard(since=ms_of(9, 0), until=ms_of(9, 15))),
    ({"kind": "sessions"}, want_guard(kind="sessions")),
)
for a, want in GUARD:
    check("guard", a, want)

# ---------------------------------------------------------------- 3. fixes

print("3. fixes", flush=True)
E1 = b"<time> ERROR db: connection refused to <ip>:<num>"
E3 = b"<time> ERROR test_upload failed: AssertionError: expected <num>, got <num>"
JOB = b"<time> ERROR worker: job <num> failed: timeout"
e1_r8 = [r for r in records_of(8) if r[2] == E1][0]
e1_r3 = [r for r in records_of(3) if r[2] == E1][0]
FIXES = [
    ({"text": "connection refused"}, fixes_text_head("connection refused", [E1]), [E1]),
    ({"text": "test_upload failed"}, fixes_text_head("test_upload failed", [E3]), [E3]),
    ({"text": "worker"}, fixes_text_head("worker", error_keys_holding("worker")), error_keys_holding("worker")),
    ({"text": "PAYMENT FAILED", "icase": True}, fixes_text_head("PAYMENT FAILED", error_keys_holding("payment failed")), error_keys_holding("payment failed")),
    ({"text": "ERROR"}, fixes_text_head("ERROR", error_keys_holding("ERROR")), error_keys_holding("ERROR")),
    ({"text": "nothing like this"}, fixes_text_head("nothing like this", []), []),
    ({"text": "42"}, fixes_text_head("42", []), []),
]
pasted = model[8]["body"][e1_r8[0] - 1].rstrip(b"\n").decode()
FIXES.append(({"text": pasted}, fixes_text_head(pasted, [E1]), [E1]))
for a, head, keys in FIXES:
    if set(keys) != set(error_keys_holding(a["text"], a.get("icase", False))):
        bad("fixes %s: the model's keys %s" % (json.dumps(a), keys))
    check("fixes", a, want_fixes(keys, head))
# by reference: to the error's line, to a frame inside its record (the record's key), and to a
# line whose key is not an error key
for line, (r0, r1) in ((e1_r8[0], (e1_r8[0], e1_r8[1])), (e1_r3[0] + 2, (e1_r3[0], e1_r3[1]))):
    rid = 8 if line == e1_r8[0] else 3
    lines = "line %d" % r0 if r0 == r1 else "lines %d to %d" % (r0, r1)
    head = ["fixes for %s: its record (%s, level error) has an error key of %s; each stop shows what came right before it, oldest first" % (lref(rid, line), lines, PID)]
    check("fixes", {"ref": lref(rid, line)}, want_fixes([E1], head))
    # (a reference without its project: the server's project; the answer names it whole)
    check("fixes", {"ref": "r%d:%d" % (rid, line)}, want_fixes([E1], head))
INFO = b"<time> INFO app starting"
k = REG[INFO]
head = ["fixes for %s: its record (line 1) has the key %s (%s in %s, first seen %s at %s), which has no error record in %s; fixes follows error keys (level error, or multi-line error records): try errors" % (
    lref(5, 1), quoted(INFO.decode()), plural(k["records"], "record"), plural(k["runs"], "run"), minute_text(k["first"][0]), lref(k["first"][1], k["first"][2]), PID)]
full("fixes", {"ref": lref(5, 1)}, (head, [], []))
for a, want in (
    ({}, "give `text` (words of the error message) or `ref`"),
    ({"text": "x", "ref": "r1:1"}, "give `text` or `ref`, not both"),
    ({"ref": "r99:1"}, "no run r99 in %s" % PID),
    ({"ref": "r8:999"}, "%s is past the end of its run" % lref(8, 999)),
    ({"ref": "nope"}, "'nope' is not a line reference"),
):
    t, err = call("fixes", a)
    # (errors too name this project's runs without the project: the call names no project)
    if not err or strip_pid(want) not in t:
        bad("fixes %s should be an error saying %r: %s" % (json.dumps(a), want, t[:300]))
    stats["error answers"] += 1

# ---------------------------------------------------------------- 4. no cause claimed

print("4. no answer claims a cause", flush=True)
CLAIM = re.compile(r"\b(cause[sd]?|caused|because|due to|fix(e[sd]|ing)?|resolv\w*|solv\w*|blame\w*|culprit|root)\b", re.I)
REFLINE = re.compile(r"^\s*(?:%s/)?r\d+:\d+ " % re.escape(PID))
subjects = {"%s %s %s" % (c[1], c[2], c[3]) for c in COMMITS}
for name, a, t in answers:
    if name not in ("guard", "fixes"):
        continue
    for l in t.split("\n"):
        # quoted log lines (a reference, then the line) and commit subjects are data, not claims;
        # so are the Next calls and the fixes calls a back moment names
        text = l.strip()
        if REFLINE.match(l) or text in subjects or text.startswith("left out:"):
            continue
        text = re.sub(r"fixes \{.*", "", text)
        text = re.sub(r"\bfixes (for|follows)\b", "", text)
        text = re.sub(r'"[^"]*"', '""', text)
        m = CLAIM.search(text)
        if m:
            bad("%s %s claims a cause? %r in %r" % (name, json.dumps(a), m.group(0), l))
    stats["answers read for claims"] += 1

# ---------------------------------------------------------------- 6. references (5. ran in check())

print("5. references: %d lines shown, each checked with lines and expect" % len(shown_refs), flush=True)
for rid, line in sorted(shown_refs):
    if rid not in model or line > len(model[rid]["body"]):
        bad("reference r%d:%d is past the fixture's lines" % (rid, line))
        continue
    want = model[rid]["body"][line - 1].rstrip(b"\n").decode()
    t, err = srv.call("lines", {"ref": lref(rid, line), "before": 0, "after": 0, "expect": want})
    if err or "expect: same bytes" not in t:
        bad("lines %s with expect: %s" % (lref(rid, line), t[:200]))
    stats["references"] += 1

# ---------------------------------------------------------------- 7. read only, damaged files, a rebuild

print("6. the store unchanged by the tools; damaged and missing notes; a rebuild", flush=True)
after_tools = store_state(HOME)
if after_tools != before_tools:
    bad("the tools changed the store: %s" % sorted(set(after_tools.items()) ^ set(before_tools.items()))[:5])
reg_path, mom_path = os.path.join(PDIR, "keys.cat"), os.path.join(PDIR, "moments.jsonl")
keep_reg, keep_mom = open(reg_path, "rb").read(), open(mom_path, "rb").read()
for damage, why in (
    (lambda: open(reg_path, "wb").write(b"CMLK\x02 damaged"), "keys.cat is missing or damaged"),
    (lambda: open(mom_path, "ab").write(b"\n"), "moments.jsonl is missing, damaged, or not the one keys.cat was written with"),
    (lambda: (os.remove(reg_path), os.remove(mom_path)), "keys.cat is missing or damaged"),
):
    damage()
    full("guard", {}, want_guard(), ["moments made now from the runs' catalogs: %s (%s)" % (PID, why)])
    full("fixes", {"text": "connection refused"}, want_fixes([E1], fixes_text_head("connection refused", [E1])), ["stops made now from the runs' catalogs: %s (%s)" % (PID, why)])
    open(reg_path, "wb").write(keep_reg)
    open(mom_path, "wb").write(keep_mom)
code = srv.close()
if code != 0:
    bad("the server exited %d" % code)
# a second store, its keys.cat deleted before the last run is saved: the last merge makes both
# files again from every catalog, and they are the same bytes
pid2, _ = make_store(WORK + "/home2", drop_registry_before=8)
d2 = os.path.join(WORK + "/home2", "projects", pid2)
for f in ("keys.cat", "moments.jsonl"):
    if open(os.path.join(d2, f), "rb").read() != open(os.path.join(PDIR, f), "rb").read():
        bad("%s made again from the catalogs differs from the one merged run by run" % f)
    stats["files made again, the same bytes"] += 1

print("checked: %s" % ", ".join("%s %s" % (k, v) for k, v in sorted(stats.items())))
if failures:
    print("FAILED: %d checks" % len(failures))
    sys.exit(1)
print("ALL EXACT")
