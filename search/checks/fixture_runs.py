"""fixture_runs.py SEARCH PACK WORK [--via add|run] [--show]: the acceptance check
(python3 -I): the tools odd, errors, compare and quiet on six runs of a fake test command.

WORK is a scratch folder the check makes (and clears only when a check made it). It holds a
made-up project, proj/, whose fake test command, ./fake-test --all, runs six times (runs 1 to
6, ten minutes apart, on 2026-11-17 from 10:00 UTC). Each run also writes two log files,
proj/logs/worker.log and proj/logs/api.log (rewritten each time), saved with `add` after the
run. So the store holds 18 runs: the six command runs and six pieces of each log file. What
they hold, known by construction:
- messages that are new, gone and changed between the runs: "slow query" falls from 10 to 1
  from run 5 to run 6 (exactly 90%: told), "cleanup removed" from 9 to 1 (88.9%: not told),
  "cache warmed" and "flaky test_upload" are gone in run 6, "payment failed" is new in run 6,
  "memory at" is new in run 5 and grows, "retry" is rare (runs 2 and 6);
- a stack trace (a Java exception with its cause: one error record of 6 lines) in runs 4 and
  6, and other errors ("connection refused") in runs 2, 4 and 6; runs 4 and 6 fail (exit 1);
- worker.log goes quiet: its last piece is empty (it logged nothing);
- api.log logs only non-errors in its last piece (it logged no errors).

The check keeps its own model of every saved run (each record's lines, key, level and time;
keys by checks/msgkey_ref.py, records by construction) and checks:
1. the store: each run's catalog (`keys --cat`) holds the model's records per key, and each
   command run its tags;
2. every answer of the four tools, in full (a big max_tokens), equals the answer the model
   gives, line for line: every count, reference, time and shown line;
3. the cap and the "left out" line: with small max_tokens and limits, each answer stays within
   max_tokens x 4 bytes before its last line, ends with "all shown" or a "left out" line, and
   following the Next calls gives every row of the full answer once, in order (a row that
   does not fit even alone is shown cut, as search shows such a hit; an error group's record
   that does not fit names the `lines` call for the rest, which gives exactly the rest);
4. a run's catalog damaged and the key registry damaged: the answers are the same, and say
   which catalogs were made now; a run still being saved is named as not counted;
5. references: `lines` with `expect` says "same bytes" for every line the answers showed.

--via add (the default): each run of the fake command is saved with `add` and the test switch
FEWER_TEST_RUN_TAGS (kind run, the command, exit status, branch, commit, dirty, start, end), as
`run` would tag it. --via run: the command really runs under
`fewer run` in a git repository made in WORK (its tags come from git), then
`fewer pack-pending --wait`.
--show prints every answer.
"""
import calendar, collections, hashlib, importlib.util, json, os, random, re, subprocess, sys, time

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
if len(argv) != 3 or VIA not in ("add", "run"):
    raise SystemExit(__doc__)
SEARCH, PACK, WORK = (os.path.abspath(x) for x in argv)
mcpc.fresh_work(WORK)
# every process this check starts, the server too, gets a made-up HOME, agents' folders and
# claude, inside WORK (safe_env.py)
safe = load("safe_env").setup(WORK)
HOME, PROJ = WORK + "/home", WORK + "/proj"
os.makedirs(PROJ + "/logs")
os.makedirs(WORK + "/out")
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ)
for k in ("FEWER_TEST_RUN_TAGS", "FEWER_NOW", "FEWER_TEST_SLOW_BLOCK_MS"):
    env.pop(k, None)
failures = []
stats = collections.Counter()


def bad(msg):
    failures.append(msg)
    if len(failures) <= 40:
        print("FAIL:", msg, flush=True)


# ---------------------------------------------------------------- the fixture

DAY = calendar.timegm((2026, 11, 17, 10, 0, 0)) * 1000  # run 1 starts at 10:00 UTC
CMD = ["./fake-test", "--all"]
TESTS = ["test_login", "test_search", "test_checkout", "test_upload"]
# per run 1..6
EXIT = [0, 0, 0, 1, 0, 1]
SLOW = [2, 2, 3, 3, 10, 1]
WARM = [1, 1, 1, 1, 1, 0]
DB = [0, 2, 0, 1, 0, 3]
TRACE = [0, 0, 0, 1, 0, 1]
PAY = [0, 0, 0, 0, 0, 2]
RETRY = [0, 1, 0, 0, 0, 1]
FLAKY = [0, 0, 0, 0, 1, 0]
MEM = [0, 0, 0, 0, 4, 5]
CLEAN = [9, 9, 9, 9, 9, 1]
WORKER_PICK = [5, 5, 5, 5, 5, 0]
WORKER_FAIL = [0, 0, 1, 0, 1, 0]
API_GET = [4, 4, 4, 4, 4, 7]
API_ERR = [2, 1, 2, 1, 2, 0]
BRANCH = "main"
COMMITS = ["1111aaaa" * 5, "1111aaaa" * 5, "2222bbbb" * 5, "2222bbbb" * 5, "3333cccc" * 5, "4444dddd" * 5]
DIRTY = [False, False, False, False, False, True]
TRACE_MORE = [
    "\tat shop.Cart.checkout(Cart.java:42)",
    "\tat shop.CartTest.testCheckout(CartTest.java:17)",
    "Caused by: java.lang.NullPointerException: item is null",
    "\tat shop.Item.price(Item.java:9)",
    "\t... 2 more",
]


def iso(ms):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ms // 1000))


class Writer:
    """Records written one after another: (lines, is an error, time in ms)."""

    def __init__(self, t0, step):
        self.t, self.step, self.recs = t0, step, []

    def rec(self, level, msg, more=()):
        self.recs.append(([("%s %s %s" % (iso(self.t), level, msg)).encode()] + [m.encode() for m in more], level == "ERROR" or bool(more), self.t))
        self.t += self.step


def test_output(k):
    i = k - 1
    w = Writer(DAY + i * 600_000, 7_000)
    w.rec("INFO", "session starts: run %d of the suite" % k)
    w.rec("INFO", "collected %d tests" % len(TESTS))
    for j, name in enumerate(TESTS):
        failed = name == "test_checkout" and EXIT[i]
        w.rec("INFO", "%s %s %d ms" % (name, "FAILED after" if failed else "passed in", 100 + 13 * j + k))
        if name == "test_search":
            for n in range(SLOW[i]):
                w.rec("WARN", "slow query took %d ms" % (250 + 10 * n + k))
            for n in range(DB[i]):
                w.rec("ERROR", "db: connection refused to 10.0.0.%d:5432" % (n + k))
        if name == "test_checkout":
            for n in range(TRACE[i]):
                w.rec("ERROR", "test_checkout crashed: java.lang.IllegalStateException: cart is empty", TRACE_MORE)
            for n in range(PAY[i]):
                w.rec("ERROR", "payment failed: card declined for order %d" % (5520 + n))
        if name == "test_upload":
            for n in range(RETRY[i]):
                w.rec("WARN", "retry %d of upload to s3" % (n + 2))
            for n in range(FLAKY[i]):
                w.rec("WARN", "flaky test_upload retried")
    for n in range(WARM[i]):
        w.rec("INFO", "cache warmed in %d ms" % (120 + k))
    for n in range(MEM[i]):
        w.rec("WARN", "memory at %d%% of the limit" % (80 + n))
    for n in range(CLEAN[i]):
        w.rec("INFO", "cleanup removed %d temp files" % (3 + n))
    w.rec("INFO", "%d passed, %d failed in %d s" % (len(TESTS) - EXIT[i], EXIT[i], 60 + 10 * k))
    return w.recs


def worker_log(k):
    i = k - 1
    w = Writer(DAY + i * 600_000 + 3_000, 31_000)
    for n in range(WORKER_PICK[i]):
        w.rec("INFO", "worker: picked job %d" % (4400 + 10 * k + n))
    for n in range(WORKER_FAIL[i]):
        w.rec("ERROR", "worker: job %d failed: timeout" % (4400 + 10 * k + n))
    return w.recs


def api_log(k):
    i = k - 1
    w = Writer(DAY + i * 600_000 + 5_000, 23_000)
    for n in range(API_GET[i]):
        w.rec("INFO", "api: GET /items 200 in %d ms" % (40 + n))
    for n in range(API_ERR[i]):
        w.rec("ERROR", "api: upstream timeout after %d ms" % (3000 + n))
    return w.recs


def text_of(recs):
    return b"".join(l + b"\n" for lines, _, _ in recs for l in lines)


# ---------------------------------------------------------------- the store

PID = None


def tool(*a, extra_env=None):
    r = subprocess.run([SEARCH] + list(a), env=dict(env, **(extra_env or {})), cwd=PROJ, stdin=subprocess.DEVNULL, capture_output=True)
    if r.returncode != 0:
        raise SystemExit("fewer %s failed (%d): %s" % (" ".join(a), r.returncode, r.stderr.decode()))
    return r.stdout.decode()


def saved_ids(out):
    global PID
    ids = []
    for line in out.splitlines():
        m = re.match(r"saved (.*) as (\S+)/r(\d+): ", line)
        if m:
            PID = m.group(2)
            ids.append(int(m.group(3)))
    return ids


def git(*a):
    genv = dict(env, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid", GIT_COMMITTER_NAME="fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid")
    r = subprocess.run(["git"] + list(a), cwd=PROJ, env=genv, capture_output=True)
    if r.returncode != 0:
        raise SystemExit("git %s failed: %s" % (" ".join(a), r.stderr.decode()))
    return r.stdout.decode().strip()


def read_runs():
    runs = {}
    for line in open(os.path.join(HOME, "projects", PID, "runs.jsonl"), "rb"):
        o = json.loads(line)
        runs.setdefault(o["id"], {}).update(o)
    return runs


# the model of every saved run: id -> {"recs": [(first, last, key, error, ms)], "body": its
# lines, "label": its command or file as answers name it, "src": its source, "run": its record}
model = {}


def remember(rid, recs, label, src):
    body, out, n = [], [], 1
    for ls, err, t in recs:
        out.append((n, n + len(ls) - 1, ref.line_key(ls[0] + b"\n"), err, t))
        body += [l + b"\n" for l in ls]
        n += len(ls)
    model[rid] = {"recs": out, "body": body, "label": label, "src": src}


if VIA == "run":
    git("init", "-q", "-b", BRANCH)
    open(PROJ + "/fake-test", "w").write('#!/bin/sh\ncat "$FAKE_OUT"\nexit "$FAKE_EXIT"\n')
    os.chmod(PROJ + "/fake-test", 0o755)
    open(PROJ + "/.gitignore", "w").write("logs/\n")
    git("add", "fake-test", ".gitignore")
    git("commit", "-q", "-m", "fake test command")
test_ids = []
for k in range(1, 7):
    recs = test_output(k)
    out = WORK + "/out/run%d.log" % k
    open(out, "wb").write(text_of(recs))
    if VIA == "add":
        st = DAY + (k - 1) * 600_000
        tags = {"kind": "run", "cmd": CMD, "cwd": PROJ, "exit": EXIT[k - 1], "branch": BRANCH, "commit": COMMITS[k - 1], "dirty": DIRTY[k - 1], "start": st, "end": st + (60 + 10 * k) * 1000}
        rid = saved_ids(tool("add", out, extra_env={"FEWER_TEST_RUN_TAGS": json.dumps(tags)}))[0]
    else:
        # a new commit where the fixture's commit changes; run 6 runs with a changed file
        if k == 1 or COMMITS[k - 1] != COMMITS[k - 2]:
            open(PROJ + "/version.txt", "w").write("v%d\n" % k)
            git("add", "version.txt")
            git("commit", "-q", "-m", "version %d" % k)
        if DIRTY[k - 1]:
            open(PROJ + "/version.txt", "a").write("changed\n")
        r = subprocess.run([SEARCH, "run", "--"] + CMD, cwd=PROJ, env=dict(env, FAKE_OUT=out, FAKE_EXIT=str(EXIT[k - 1]), FEWER_TEST_NO_PACK="1"), stdin=subprocess.DEVNULL, capture_output=True)
        if b"unknown command run" in r.stderr:
            raise SystemExit("this fewer has no `run` command: use --via add")
        if r.returncode != EXIT[k - 1] or r.stdout != text_of(recs):
            bad("run %d: exit %d, its output %s the command's" % (k, r.returncode, "is" if r.stdout == text_of(recs) else "is not"))
        tool("pack-pending", "--wait")
        PID = os.listdir(HOME + "/projects")[0]
        rid = max(read_runs())
    remember(rid, recs, " ".join(CMD), "cmd " + json.dumps(CMD))
    test_ids.append(rid)
    wrecs, arecs = worker_log(k), api_log(k)
    open(PROJ + "/logs/worker.log", "wb").write(text_of(wrecs))
    open(PROJ + "/logs/api.log", "wb").write(text_of(arecs))
    wid, aid = saved_ids(tool("add", PROJ + "/logs/worker.log", PROJ + "/logs/api.log"))
    remember(wid, wrecs, PROJ + "/logs/worker.log", "file " + PROJ + "/logs/worker.log")
    remember(aid, arecs, PROJ + "/logs/api.log", "file " + PROJ + "/logs/api.log")
PDIR = os.path.join(HOME, "projects", PID)
runs = read_runs()
if sorted(runs) != sorted(model):
    raise SystemExit("the store's runs %s are not the fixture's %s" % (sorted(runs), sorted(model)))
for rid, r in runs.items():
    model[rid]["run"] = r

# ---------------------------------------------------------------- 1. the store

print("1. the store: %d runs; catalogs and tags as the fixture's" % len(runs), flush=True)
for rid in sorted(model):
    m = model[rid]
    if m["run"].get("state") != "ready":
        bad("r%d is %s" % (rid, m["run"].get("state")))
        continue
    want = collections.Counter()
    for _, _, key, _, _ in m["recs"]:
        want[ref.write_key(key)] += 1
    r = subprocess.run([SEARCH, "keys", "--cat", os.path.join(PDIR, "r%06d.cat" % rid)], capture_output=True)
    got = collections.Counter()
    for line in r.stdout.split(b"\n"):
        if line:
            n, k = line.split(b"\t", 1)
            got[k] += int(n)
    if r.returncode != 0 or got != want:
        bad("r%d: its catalog's records per key %s are not the fixture's %s (%s)" % (rid, dict(got), dict(want), r.stderr.decode()))
    if m["run"].get("lines") != len(m["body"]):
        bad("r%d: %s lines, the fixture's %d" % (rid, m["run"].get("lines"), len(m["body"])))
    stats["runs"] += 1
for n, rid in enumerate(test_ids):
    r = model[rid]["run"]
    if r.get("kind") != "run" or r.get("cmd") != CMD or r.get("exit") != EXIT[n] or r.get("branch") != BRANCH:
        bad("r%d: tags %s" % (rid, {k: r.get(k) for k in ("kind", "cmd", "exit", "branch", "commit")}))
    if VIA == "add" and (r.get("commit") != COMMITS[n] or r.get("dirty") != DIRTY[n] or r.get("start") != DAY + n * 600_000):
        bad("r%d: commit %s, dirty %s, start %s" % (rid, r.get("commit"), r.get("dirty"), r.get("start")))
if VIA == "run":
    commits = [model[r]["run"].get("commit") for r in test_ids]
    dirty = [bool(model[r]["run"].get("dirty")) for r in test_ids]
    if not (commits[0] == commits[1] and commits[2] == commits[3] and len(set(commits)) == 4 and dirty == DIRTY):
        bad("the runs' commits %s and dirty %s are not the fixture's" % (commits, dirty))

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


def duration(ms):
    ms = max(ms, 0)
    if ms < 10_000:
        return "%d.%d s" % (ms // 1000, ms % 1000 // 100)
    if ms < 60_000:
        return "%d s" % (ms // 1000)
    if ms < 3_600_000:
        return "%d min %d s" % (ms // 60_000, ms // 1000 % 60)
    return "%d h %d min" % (ms // 3_600_000, ms // 60_000 % 60)


def key_label(k, mx=300):
    """A key as the tables show it (clean.rs escape, cut at mx bytes with an ellipsis)."""
    s = []
    for ch in k.decode("utf-8", "surrogateescape"):
        o = ord(ch)
        if 0xDC80 <= o <= 0xDCFF:
            s.append("\\x%02x" % (o - 0xDC00))
        elif (o < 0x20 and ch != "\t") or o == 0x7F:
            s.append("\\x%02x" % o)
        else:
            s.append(ch)
    t = "".join(s).encode()
    if len(t) <= mx:
        return t.decode()
    k2 = mx
    while k2 > 0 and (t[k2] & 0xC0) == 0x80:
        k2 -= 1
    return t[:k2].decode() + "…"


def lref(rid, line):
    return "%s/r%d:%d" % (PID, rid, line)


def shown(rid, line):
    return "%s %s" % (lref(rid, line), mcpc.shown(model[rid]["body"][line - 1]))


def rec_ms(rec):
    # a record's time as the catalogs keep it: its minute (every fixture record has a time)
    return rec[4] // 60_000 * 60_000


def run_keys(rid):
    """Per key of a run: [records, its first record's (ms, run, line), error records]."""
    d = {}
    for rec in model[rid]["recs"]:
        first, last, key, err, t = rec
        if key not in d:
            d[key] = [0, (rec_ms(rec), rid, first), 0]
        d[key][0] += 1
        d[key][2] += err
    return d


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


def run_line(rid):
    m, r = model[rid], model[rid]["run"]
    s = "%s/r%d %s %s" % (PID, rid, r["kind"], m["label"])
    if "start" in r:
        s += ", " + local(r["start"])
        if "end" in r:
            s += ", " + duration(r["end"] - r["start"])
    if "exit" in r:
        s += ", exit %d" % r["exit"]
    if "branch" in r:
        s += ", %s@%s" % (r["branch"], r.get("commit", "?")[:9])
    if r.get("dirty"):
        s += " (dirty)"
    return s + ": %s in %s" % (plural(len(m["recs"]), "record"), plural(len(m["body"]), "line"))


def want_errors(ids, desc):
    groups = {}
    for rid in sorted(ids, reverse=True):
        for rec in model[rid]["recs"]:
            first, last, key, err, t = rec
            if not err:
                continue
            g = groups.setdefault(key, {"n": 0, "runs": collections.OrderedDict(), "first": None, "last": None})
            g["n"] += 1
            g["runs"][rid] = g["runs"].get(rid, 0) + 1
            s = (rec_ms(rec), rid, first)
            if g["first"] is None or s < g["first"][0]:
                g["first"] = (s, (first, last))
            if g["last"] is None or s > g["last"]:
                g["last"] = s
    order = sorted(groups.items(), key=lambda kv: (-kv[1]["n"], kv[1]["first"][0], kv[0]))
    errors = sum(g["n"] for g in groups.values())
    with_errors = len({rid for g in groups.values() for rid in g["runs"]})
    if groups:
        head = ["%s (%s) in %s of %s (%s), most records first" % (plural(len(groups), "error group"), plural(errors, "error record"), group(with_errors), plural(len(ids), "run"), desc)]
    else:
        head = ["no error records in %s (%s): none of their %s is at level error or in a multi-line error record" % (plural(len(ids), "run"), desc, plural(sum(len(model[r]["recs"]) for r in ids), "record"))]
    rows = []
    for key, g in order:
        (fms, frun, fline), (r0, r1) = g["first"]
        lms, lrun, lline = g["last"]
        rs = list(g["runs"].items())
        rl = ", ".join("r%d %s" % (r, group(n)) for r, n in rs[:10]) + (" (+%s more runs)" % group(len(rs) - 10) if len(rs) > 10 else "")
        # (the runs and their counts only when the answer counts more than one run)
        runs_part = "; in %s: %s" % (plural(len(rs), "run"), rl) if len(ids) > 1 else ""
        row = ["%s %s" % (plural(g["n"], "error record"), json.dumps(key_label(key), ensure_ascii=False)), "  first %s %s, last %s %s%s" % (minute_text(fms), lref(frun, fline), minute_text(lms), lref(lrun, lline), runs_part)]
        if r0 == r1:
            row.append("  first record: " + shown(frun, r0))
        else:
            row.append("  first record, %s/r%d:%d-%d (%s lines):" % (PID, frun, r0, r1, group(r1 - r0 + 1)))
            for l in range(r0, min(r1, r0 + 39) + 1):
                row.append("  %d %s" % (l, mcpc.shown(model[frun]["body"][l - 1])))
        rows.append((0, row))
    return head, [], rows


def want_odd(ids, desc):
    proj, here = {}, {}
    for d, rids in ((proj, model), (here, ids)):
        for rid in rids:
            for key, (n, first, _) in run_keys(rid).items():
                e = d.setdefault(key, [0, first])
                e[0] += n
                e[1] = min(e[1], first)
    keys = []
    for key, (n, sample) in here.items():
        pn, pfirst = proj[key]
        keys.append((pfirst[1] in ids, pn, n, pfirst, key, sample))
    keys.sort(key=lambda x: (not x[0], x[1], x[2], x[3], x[4]))
    head = ["odd lines first: %s in %s (%s): %s new (first seen in these runs), then the rarest in the project" % (plural(len(keys), "message key"), plural(len(ids), "run"), desc, group(sum(1 for x in keys if x[0])))]
    rows = []
    for new, pn, n, pfirst, key, sample in keys:
        counts = "%s in these runs, %s in the project" % (plural(n, "record"), group(pn))
        second = "  new: %s; first seen here, %s" % (counts, minute_text(pfirst[0])) if new else "  %s; first seen %s at %s" % (counts, minute_text(pfirst[0]), lref(pfirst[1], pfirst[2]))
        rows.append((0, [shown(sample[1], sample[2]), second]))
    return head, [], rows


def want_compare(a, b):
    ka, kb = run_keys(a), run_keys(b)
    new = sorted(((0, kb[k][0], kb[k][1][2]) for k in kb if k not in ka), key=lambda x: x[2])
    gone = sorted(((ka[k][0], 0, ka[k][1][2]) for k in ka if k not in kb), key=lambda x: x[2])
    changed = sorted(((ka[k][0], kb[k][0], kb[k][1][2]) for k in kb if k in ka and ka[k][0] != kb[k][0]), key=lambda x: (-abs(x[0] - x[1]), x[2]))
    same = sum(1 for k in kb if k in ka and ka[k][0] == kb[k][0])
    head = [
        "compare a = %s/r%d with b = %s/r%d, by message key (records; a stack trace is one): %s new in b, %s gone from b, %s changed, %s the same" % (PID, a, PID, b, group(len(new)), group(len(gone)), group(len(changed)), group(same)),
        "a: " + run_line(a),
        "b: " + run_line(b),
    ]
    heads = [
        "new in b (%s, %s), each with its first line in b:" % (plural(len(new), "key"), plural(sum(x[1] for x in new), "record")),
        "gone from b (%s, %s), each with its first line in a:" % (plural(len(gone), "key"), plural(sum(x[0] for x in gone), "record")),
        "changed (%s), the largest change first, each with its first line in b:" % plural(len(changed), "key"),
    ]
    rows = []
    for sec, v in ((0, new), (1, gone), (2, changed)):
        for na, nb, line in v:
            rows.append((sec, ["%s -> %s  %s" % (group(na), group(nb), shown(a if sec == 1 else b, line))]))
    return head, heads, rows


class Side:
    def __init__(self):
        self.keys, self.records, self.errors, self.runs = {}, 0, 0, []

    def add(self, key, n, errors, first):
        if n == 0:
            return
        e = self.keys.setdefault(key, [0, first])
        e[0] += n
        e[1] = min(e[1], first)
        self.records += n
        self.errors += errors
        if first[1] not in self.runs:
            self.runs.append(first[1])


def status(b, a):
    before = "before: %s, %s" % (plural(b.records, "record"), plural(b.errors, "error"))
    if a.records == 0:
        return "logged nothing (0 records; %s)" % before
    if b.errors > 0 and a.errors == 0:
        return "logged no errors (%s, none an error; %s)" % (plural(a.records, "record"), before)
    if b.records > 0 and a.records * 10 <= b.records:
        return "fell by 90%% or more (%s, %s; %s)" % (plural(a.records, "record"), plural(a.errors, "error"), before)
    return "%s, %s (%s)" % (plural(a.records, "record"), plural(a.errors, "error"), before)


def quiet_keys(b, a):
    """Keys that fell to 0 or by 90% or more, the biggest fall first."""
    v = []
    for key, (nb, first) in b.keys.items():
        na = a.keys.get(key, [0])[0]
        if nb > 0 and na * 10 <= nb:
            v.append((nb - na, first, key, nb, na))
    v.sort(key=lambda x: (-x[0], x[1], x[2]))
    return v


def want_quiet(head, compared):
    """compared: [(label, sides, before Side, after Side)], newest source first."""
    withk = [c for c in compared if quiet_keys(c[2], c[3])]
    without = [c for c in compared if not quiet_keys(c[2], c[3])]
    lines = ["%s: %s compared, %s with keys that fell" % (head, plural(len(compared), "source"), group(len(withk)))]
    if without:
        lines.append("nothing fell: " + ", ".join("%s (%s)" % (c[0], status(c[2], c[3])) for c in without[:10]) + (" and %d more" % (len(without) - 10) if len(without) > 10 else ""))
    heads, rows = [], []
    for si, (label, sides, b, a) in enumerate(withk):
        heads.append("== %s: %s: %s" % (label, sides, status(b, a)))
        for _, first, key, nb, na in quiet_keys(b, a):
            rows.append((si, ["  %s -> %s  %s" % (group(nb), group(na), shown(first[1], first[2]))]))
    return lines, heads, rows


def sources():
    """source -> its runs' ids, oldest first; newest source first."""
    s = collections.OrderedDict()
    for rid in sorted(model):
        s.setdefault(model[rid]["src"], []).append(rid)
    return sorted(s.items(), key=lambda kv: -kv[1][-1])


def want_quiet_previous():
    compared = []
    for src, ids in sources():
        after, before = ids[-1], ids[-2]
        b, a = Side(), Side()
        for side, rid in ((b, before), (a, after)):
            for key, (n, first, errs) in run_keys(rid).items():
                side.add(key, n, errs, first)
        compared.append((model[after]["label"], "%s/r%d against %s/r%d" % (PID, after, PID, before), b, a))
    return want_quiet("quiet: keys that fell to 0 or by 90% or more, each source's last run against the run before it (" + scope_desc() + ")", compared)


def want_quiet_window(wa, wb):
    """The window [wa, wb) in minutes against the same length before it."""
    pa, pb = wa - (wb - wa), wa
    found = []
    for src, ids in sources():
        b, a = Side(), Side()
        newest = None
        for rid in ids:
            for rec in model[rid]["recs"]:
                first, last, key, err, t = rec
                m = t // 60_000
                for side, lo, hi in ((b, pa, pb), (a, wa, wb)):
                    if lo <= m < hi:
                        side.add(key, 1, int(err), (rec_ms(rec), rid, first))
                        newest = max(newest or 0, rid)
        if newest is None:
            continue
        runs_text = lambda s: ", ".join("r%d" % r for r in s.runs) if s.runs else "no run"
        found.append((newest, (model[newest]["label"], "the window (%s) against the window before (%s)" % (runs_text(a), runs_text(b)), b, a)))
    found.sort(key=lambda x: -x[0])
    n = wb - wa
    len_text = plural(n // 60, "hour") if n % 60 == 0 else plural(n, "minute")
    head = "quiet: keys that fell to 0 or by 90%% or more in %s against the %s before it (%s) (%s)" % (minute_span(wa, wb), len_text, minute_span(pa, pb), scope_desc())
    return want_quiet(head, [c for _, c in found])


# ---------------------------------------------------------------- comparing answers


def store_state():
    """Every file of the store: its size, time and SHA-256 (the tools must change none)."""
    out = {}
    for root, _, files in os.walk(HOME):
        for f in files:
            p = os.path.join(root, f)
            st = os.stat(p)
            out[os.path.relpath(p, HOME)] = (st.st_size, st.st_mtime_ns, hashlib.sha256(open(p, "rb").read()).hexdigest())
    return out


before_tools = store_state()
srv = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
srv.initialize()
shown_refs = set()


def call(name, a):
    t, err = srv.call(name, a)
    stats["calls"] += 1
    if SHOW:
        print("\n>>> %s %s\n%s" % (name, json.dumps(a), t), flush=True)
    return t, err


def note_refs(text):
    for m in re.finditer(r"(?<![a-z0-9/-])(?:%s/)?r(\d+):(\d+)" % re.escape(PID), text):
        shown_refs.add((int(m.group(1)), int(m.group(2))))


def flat(head, heads, rows, notes=(), at=1):
    """The full answer: heading lines with the notes after the first `at` of them, then each
    section's heading before its rows."""
    out, sec = list(head[:at]) + list(notes) + list(head[at:]), None
    for s, lines in rows:
        if s != sec and s < len(heads) and heads[s]:
            out.append(heads[s])
        sec = s
        out += lines
    return "\n".join(out + ["all shown"])


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
    head, heads, rows = as_shown(a, want)
    notes = [strip_pid(n) for n in notes] if "project" not in a else notes
    t, err = call(name, dict(a, max_tokens=20000, limit=500))
    if err:
        bad("%s %s: isError: %s" % (name, json.dumps(a), t[:300]))
        return
    if same_text("%s %s" % (name, json.dumps(a)), t, flat(head, heads, rows, notes, 3 if name == "compare" else 1)):
        note_refs(t)
        stats["rows compared"] += len(rows)


def cut_of(got, want):
    """`got` is `want` whole, or cut by the cap: its first bytes and "…(+N bytes)"."""
    if got == want:
        return True
    m = re.match(r"^(.*)…\(\+([\d,]+) bytes\)$", got, re.S)
    if not m:
        return False
    keep, n = m.group(1).encode(), int(m.group(2).replace(",", ""))
    return want.encode().startswith(keep) and len(want.encode()) - len(keep) == n


ROW_START = {
    "errors": lambda l: re.match(r"^[\d,]+ error records? \"", l),
    "odd": lambda l: re.match(r"^(?:%s/)?r\d+:\d+ " % re.escape(PID), l),
    "compare": lambda l: re.match(r"^[\d,]+ -> [\d,]+  ", l),
    "quiet": lambda l: re.match(r"^  [\d,]+ -> [\d,]+  ", l),
}
MORE = re.compile(r'^  \(\+([\d,]+) more lines of this record: lines (\{.*\})\)$')


def row_ok(name, got, want):
    """A row of a small answer against the full answer's: the same lines, the first ones maybe
    cut by the cap; an error group whose record did not fit shows the lines that did, then the
    `lines` call that gives the rest, which must give exactly the rest."""
    if len(got) == len(want) and all(cut_of(g, w) for g, w in zip(got, want)):
        return True
    if name != "errors" or not got or not MORE.match(got[-1]):
        return False
    m = MORE.match(got[-1])
    head = 3
    shown_lines = got[head:-1]
    if not all(cut_of(g, w) for g, w in zip(got[:head], want[:head])) or shown_lines != want[head:head + len(shown_lines)]:
        return False
    rest = want[head + len(shown_lines):]
    call_args = json.loads(m.group(2))
    t, err = call("lines", dict(call_args, max_tokens=20000))
    if err:
        return False
    got_rest = []
    for l in t.split("\n")[1:-1]:
        x = re.match(r"^(?:> |  )(\d+) (.*)$", l)
        if x:
            got_rest.append("  %s %s" % (x.group(1), x.group(2)))
    stats["records cut, rest read by lines"] += 1
    return int(m.group(1).replace(",", "")) == len(rest) and got_rest == rest


def follow(name, a, want, max_tokens, limit):
    """Follow the Next calls with a small cap: each answer within the cap, and the rows of all
    answers are the full answer's rows, each once, in order."""
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
        # then section headings (maybe cut) and rows, each row from its first line to the next
        cur = None
        for l in body[len(head):]:
            if ROW_START[name](l):
                cur = [l]
                got_rows.append(cur)
            elif cur is not None and (name in ("errors", "odd")) and not any(cut_of(l, h) for h in heads):
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
        # (a window is named in the Next call as it was read: in whole minutes, UTC)
        if name == "quiet" and a.get("against") == "window" and (args.get("since"), args.get("until")) != (a["since"], a["until"]):
            bad("quiet: the Next call names another window: %s, asked %s to %s" % (m.group(1), a["since"], a["until"]))
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


ALL = sorted(model)
T = test_ids
WID = [r for r in sorted(model) if model[r]["src"].endswith("worker.log")]
AID = [r for r in sorted(model) if model[r]["src"].endswith("api.log")]
# (the server takes the project out of an answer's references only when every
# "<project>/r<digit>" in it is this project's: no fixture line looks like one, so every
# answer here has it taken out, and the model's lines can be compared one by one)
for rid in model:
    for l in model[rid]["body"]:
        if re.search(rb"[a-z0-9-]/r\d", l):
            bad("r%d: a fixture line looks like a run reference: %r" % (rid, l))

# ---------------------------------------------------------------- 2. errors

print("2. errors", flush=True)
ERRORS = (
    ({}, ALL, scope_desc()),
    ({"runs": ["r%d" % T[3]]}, [T[3]], scope_desc()),
    ({"command": "fake-test"}, T, scope_desc()),
    ({"runs": ["r%d" % T[0]]}, [T[0]], scope_desc()),
    ({"runs": ["r%d" % AID[5], "r%d" % WID[5]]}, [AID[5], WID[5]], scope_desc()),
    ({"since": "2026-11-17T10:40:00Z", "until": "2026-11-17T10:49:00Z"}, [T[4], WID[4], AID[4]], scope_desc()),
)
for a, ids, desc in ERRORS:
    check("errors", a, want_errors(ids, desc))
# the default: the 5 groups with the most records (the token test of 2026-11-23)
t, err = call("errors", {"max_tokens": 20000})
n_groups = len(want_errors(ALL, scope_desc())[2])
got_groups = sum(1 for l in t.split("\n") if ROW_START["errors"](l))
if err or got_groups != min(5, n_groups):
    bad("errors with the default limit: %d groups shown, of %d (want %d)" % (got_groups, n_groups, min(5, n_groups)))

# ---------------------------------------------------------------- 3. odd

print("3. odd", flush=True)
ODD = (
    ({"runs": ["r%d" % T[5]]}, [T[5]], scope_desc()),
    ({"runs": ["r%d" % T[4]]}, [T[4]], scope_desc()),
    ({"runs": ["last:fake-test"]}, [T[5]], scope_desc()),
    ({}, ALL, scope_desc()),
    ({"command": "fake-test"}, T, scope_desc()),
    ({"runs": ["r%d" % T[1], "r%d" % T[3]]}, [T[1], T[3]], scope_desc()),
    ({"runs": ["r%d" % AID[5]]}, [AID[5]], scope_desc()),
)
for a, ids, desc in ODD:
    check("odd", a, want_odd(set(ids), desc))

# ---------------------------------------------------------------- 4. compare

print("4. compare", flush=True)
COMPARE = (
    ({"command": "fake-test", "failing": True}, (T[4], T[5])),
    ({"command": "fake-test"}, (T[4], T[5])),
    ({"a": "r%d" % T[2], "b": "r%d" % T[3]}, (T[2], T[3])),
    ({"b": "r%d" % T[3], "failing": True}, (T[2], T[3])),
    ({"b": "r%d" % T[3]}, (T[2], T[3])),
    ({"a": "r%d" % T[0]}, (T[0], T[5])),
    ({}, (AID[4], AID[5])),
    ({"a": "%s/r%d" % (PID, WID[4]), "b": "r%d" % WID[5]}, (WID[4], WID[5])),
    ({"a": "r%d" % T[5], "b": "r%d" % T[0]}, (T[5], T[0])),
    ({"command": "fake-test", "failing": True, "since": "2026-11-17T10:00:00Z", "until": "2026-11-17T10:35:00Z"}, (T[2], T[3])),
)
for a, (ra, rb) in COMPARE:
    check("compare", a, want_compare(ra, rb))
for a, want in (
    ({"a": "r%d" % T[0], "b": "r%d" % T[0]}, "a and b are the same run"),
    ({"b": "r%d" % T[0]}, "no run of ./fake-test --all before %s/r%d to compare it with: give a" % (PID, T[0])),
    ({"b": "r%d" % T[0], "failing": True}, "no passing run of ./fake-test --all before"),
    ({"b": "r99"}, "no run r99 in %s" % PID),
    ({"b": "x"}, "'x' is not a run reference"),
    ({"command": "fake-test", "failing": True, "until": "2026-11-17T10:20:00Z"}, "no failing run (an exit status other than 0, or a signal) in this scope"),
    ({"a": "r%d" % T[5]}, "no run to compare in this scope (%s) after a, of its command or file; give b" % PID),
):
    t, err = call("compare", a)
    # (errors too name this project's runs without the project: the call names no project)
    if not err or strip_pid(want) not in t:
        bad("compare %s should be an error saying %r: %s" % (json.dumps(a), want, t[:300]))
    stats["error answers"] += 1

# ---------------------------------------------------------------- 5. quiet

print("5. quiet", flush=True)
check("quiet", {}, want_quiet_previous())
check("quiet", {"against": "previous"}, want_quiet_previous())
base = DAY // 60_000
windows = [(base + 50, base + 60), (base + 40, base + 50), (base + 52, base + 57), (base + 30, base + 60), (base, base + 120), (base - 60, base)]
rnd = random.Random(5)
for _ in range(30):
    lo = base - 20 + rnd.randrange(100)
    windows.append((lo, lo + 1 + rnd.randrange(30)))
for wa, wb in windows:
    a = {"against": "window", "since": iso(wa * 60_000), "until": iso(wb * 60_000)}
    want = want_quiet_window(wa, wb)
    full("quiet", a, want)
    follow("quiet", a, want, 200, 2)
    stats["windows"] += 1
# a window given in seconds counts whole minutes: from since's minute to until's
t, err = call("quiet", {"against": "window", "since": iso((base + 50) * 60_000 + 20_000), "until": iso((base + 59) * 60_000 + 1_000)})
same_text("quiet in whole minutes", t, strip_pid(flat(*want_quiet_window(base + 50, base + 60))))
t, err = call("quiet", {"against": "window", "since": "2026-11-17T11:00:00Z", "until": "2026-11-17T10:00:00Z"})
if not err or "the window is empty" not in t:
    bad("quiet with since after until: %s" % t[:200])
after_tools = store_state()
if after_tools != before_tools:
    bad("the tools changed the store: %s" % sorted(set(after_tools.items()) ^ set(before_tools.items()))[:5])
print("   the store's %d files are unchanged after %d tool calls" % (len(after_tools), stats["calls"]), flush=True)

# ---------------------------------------------------------------- 6. damaged catalogs, a run in progress

print("6. a damaged catalog and key registry, and a run still being saved", flush=True)
cat6 = os.path.join(PDIR, "r%06d.cat" % T[5])
keep = open(cat6, "rb").read()
reg = os.path.join(PDIR, "keys.cat")
keep_reg = open(reg, "rb").read()
open(cat6, "wb").write(keep[:-9] + b"\x00" * 5 + keep[-4:])
open(reg, "wb").write(b"CMLK\x01 damaged")
made = ["catalogs made now from the runs' bytes: r%d (damaged catalog (it does not match its checksum))" % T[5]]
for name, a, want in (
    ("errors", {}, want_errors(ALL, scope_desc())),
    ("odd", {"runs": ["r%d" % T[5]]}, want_odd({T[5]}, scope_desc())),
    ("odd", {"runs": ["r%d" % T[4]]}, None),
    ("compare", {"command": "fake-test", "failing": True}, want_compare(T[4], T[5])),
    ("quiet", {}, want_quiet_previous()),
):
    if want is None:
        # (the registry made now from every run's catalog counts r6 too)
        want, notes = want_odd({T[4]}, scope_desc()), made
    else:
        notes = made
    full(name, a, want, notes)
open(cat6, "wb").write(keep)
open(reg, "wb").write(keep_reg)
# a run still being saved: named as not counted, and the answers otherwise the same
with open(os.path.join(PDIR, "runs.jsonl"), "a") as f:
    f.write(json.dumps({"id": 99, "kind": "run", "state": "packing", "start": DAY + 3_600_000, "cmd": CMD}) + "\n")
for name, a, want, notes in (
    ("errors", {}, want_errors(ALL, scope_desc()), ["not counted: r99 (being packed)"]),
    ("odd", {"command": "fake-test"}, want_odd(set(T), scope_desc()), ["not counted: r99 (being packed)"]),
):
    full(name, a, want, notes)
t, err = call("compare", {"command": "fake-test"})
if err or "newer runs of it not compared (not saved yet): r99 (packing)" not in t:
    bad("compare with a run being packed: %s" % t[:600])
t, err = call("quiet", {})
if err or strip_pid("against %s/r%d (newer, not saved yet: r99 (packing)):" % (PID, T[4])) not in t:
    bad("quiet with a run being packed: %s" % t[:800])
data = open(os.path.join(PDIR, "runs.jsonl"), "rb").read()
open(os.path.join(PDIR, "runs.jsonl"), "wb").write(data[: data.rindex(b"{")])

# ---------------------------------------------------------------- 7. references

print("7. references: %d lines shown, each checked with lines and expect" % len(shown_refs), flush=True)
for rid, line in sorted(shown_refs):
    if rid not in model or line > len(model[rid]["body"]):
        bad("reference r%d:%d is past the fixture's lines" % (rid, line))
        continue
    want = model[rid]["body"][line - 1].rstrip(b"\n").decode()
    t, err = srv.call("lines", {"ref": lref(rid, line), "before": 0, "after": 0, "expect": want})
    if err or "expect: same bytes" not in t:
        bad("lines %s with expect: %s" % (lref(rid, line), t[:200]))
    stats["references"] += 1

code = srv.close()
if code != 0:
    bad("the server exited %d" % code)
print("checked: %s" % ", ".join("%s %s" % (k, v) for k, v in sorted(stats.items())))
if failures:
    print("FAILED: %d checks" % len(failures))
    sys.exit(1)
print("ALL EXACT")
