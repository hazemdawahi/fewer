"""oracle.py SEARCH PACK WORK DATA [N]: the tools' answers against Python (python3 -I).

A store (WORK/home) is made with `fewer add` from copies of the *_2k logs and the two
logs of several blocks in DATA (HPC.log, SSH.log), and edge files written here: CRLF, no
final newline, 00 bytes, a 70 KB line, colors and '\\r' redraws, an empty file. Then N random
queries (default 500), each checked against Python's own reading of the original files:

- count: per run, the number of lines holding the text (with icase, whole) is Python's;
- search: every shown line's reference names the original's line at that number, shown as
  the server shows it; runs newest first, lines in order; the answer within max_tokens x 4
  bytes plus its last line; "all shown" only when everything was, else a "left out" line
  whose next call, followed to the end, gives every matching line once, in order;
- search with context: the lines around each hit are the original's;
- lines: the lines around a reference, raw and base64 are the original's exact bytes;
  expect says "same bytes" for the original and names the first differing byte of a copy
  with one byte changed;
- follow: whole-token matches per run, oldest first, equal to Python's.
"""
import base64, bisect, importlib.util, json, os, random, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

SEARCH, PACK, WORK, DATA = (os.path.abspath(a) for a in sys.argv[1:5])
N = int(sys.argv[5]) if len(sys.argv) > 5 else 500
rnd = random.Random(7)
# (a folder that is there is cleared only when a check made it: mcpc.fresh_work)
mcpc.fresh_work(WORK)
# every process this check starts, the server too, gets a made-up HOME, agents' folders and
# claude, inside WORK (safe_env.py)
safe = safe_env.setup(WORK)
HOME, PROJ, FILES = WORK + "/home", WORK + "/proj", WORK + "/files"
os.makedirs(PROJ)
os.makedirs(FILES)
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ)


def write(name, b):
    p = os.path.join(FILES, name)
    with open(p, "wb") as f:
        f.write(b)
    return p


lines = [b"%d %s user%d from 10.0.%d.%d port %d ok req-%05d" % (i, b"INFO" if i % 7 else b"WARN", i % 13, i % 256, i * 7 % 256, 40000 + i, i % 977) for i in range(30000)]
paths = [os.path.join(DATA, f) for f in sorted(os.listdir(DATA)) if f.endswith("_2k.log")]
paths += [os.path.join(DATA, f) for f in ("HPC.log", "SSH.log") if os.path.exists(os.path.join(DATA, f))]
paths += [
    write("crlf.log", b"\r\n".join(lines[:5000]) + b"\r\n"),
    write("no_final_nl.log", b"\n".join(lines[:4000])),
    write("nul.log", b"\n".join(l + (b"\0x" if i % 5 == 0 else b"") for i, l in enumerate(lines[:3000])) + b"\n"),
    write("long_line.log", b"\n".join(lines[:2000]) + b"\n" + b"z" * 70000 + b" tail req-00042\n" + b"\n".join(lines[2000:4000]) + b"\n"),
    write("color.log", b"".join(b"\x1b[3%dm%s\x1b[0m step %d\r%s done\r\n" % (i % 8, l[:20], i, l[:30]) if i % 3 else l + b"\n" for i, l in enumerate(lines[:3000]))),
    write("empty.log", b""),
]
# (--force: nul.log holds NUL bytes, which add saves only when asked)
out = subprocess.run([SEARCH, "add", "--force"] + paths, env=env, cwd=PROJ, capture_output=True, check=True).stdout.decode()
runs = {}  # run id -> file data
for line in out.splitlines():
    m = re.match(r"saved (.*) as (\S+)/r(\d+): ", line)
    runs[int(m.group(3))] = m.group(1)
    PID = m.group(2)
assert len(runs) == len(paths), out
order = sorted(runs, reverse=True)  # the scope's order: newest first

data, low, nls = {}, {}, {}
for rid, p in runs.items():
    b = open(p, "rb").read()
    data[rid], low[rid] = b, b.lower()
    nls[rid] = [m.start() for m in re.finditer(b"\n", b)]


def line_bytes(rid, n):
    """Line n (1-based) of a run, with its '\\n' when it has one."""
    ns = nls[rid]
    s = ns[n - 2] + 1 if n > 1 else 0
    e = ns[n - 1] + 1 if n - 1 < len(ns) else len(data[rid])
    return data[rid][s:e]


def nlines(rid):
    d = data[rid]
    return len(nls[rid]) + (1 if d and not d.endswith(b"\n") else 0)


def is_word(c):
    return 65 <= c <= 90 or 97 <= c <= 122 or 48 <= c <= 57 or c in (95, 45) or c >= 128


def whole_in(line, q):
    p = line.find(q)
    while p >= 0:
        if not (p > 0 and is_word(line[p - 1])) and not (p + len(q) < len(line) and is_word(line[p + len(q)])):
            return True
        p = line.find(q, p + 1)
    return False


def matches(rid, q, icase, whole):
    """The line numbers of run rid holding q."""
    h = low[rid] if icase else data[rid]
    qq = q.lower() if icase else q
    ns, res = nls[rid], []
    p = h.find(qq)
    while p >= 0:
        li = bisect.bisect_left(ns, p)
        s = ns[li - 1] + 1 if li > 0 else 0
        e = ns[li] if li < len(ns) else len(h)
        if not whole or whole_in(h[s:e], qq):
            res.append(li + 1)
        if e >= len(h):
            break
        p = h.find(qq, e + 1)
    return res


def raw_escape(b):
    s = []
    for ch in b.decode("utf-8", "surrogateescape"):
        o = ord(ch)
        if 0xDC80 <= o <= 0xDCFF:
            s.append("\\x%02x" % (o - 0xDC00))
        elif ch == "\\":
            s.append("\\\\")
        elif ch == "\t":
            s.append("\\t")
        elif ch == "\n":
            s.append("\\n")
        elif ch == "\r":
            s.append("\\r")
        elif o < 0x20 or o == 0x7F:
            s.append("\\x%02x" % o)
        else:
            s.append(ch)
    return "".join(s)


def cut(t, line_max):
    tb = t.encode()
    if len(tb) <= line_max:
        return t
    k = line_max
    while k > 0 and (tb[k] & 0xC0) == 0x80:
        k -= 1
    return tb[:k].decode() + "…(+{:,} bytes)".format(len(tb) - k)


fails = []
stats = dict(count=0, search=0, follows=0, cursor_calls=0, lines=0, refs=0, context=0, expect=0)


def bad(what):
    fails.append(what)
    print("FAILED:", what[:400])


REF = re.compile(r"^(?:(\S+)/)?r(\d+):(\d+) (.*)$")


def check_size(text, max_tokens, what):
    last = text.split("\n")[-1]
    if len(text.encode()) > max_tokens * 4 + len(last.encode()) + 1:
        bad("%s: answer of %d bytes over the cap %d" % (what, len(text.encode()), max_tokens * 4))
    if not (last.startswith("all shown") or last.startswith("left out:")):
        bad("%s: last line %r" % (what, last))
    return last


def next_args(last):
    m = re.search(r"Next: (\w+) (\{.*\})$", last)
    return (m.group(1), json.loads(m.group(2))) if m else (None, None)


s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
s.initialize()


def search_all(args, want):
    """Follow cursors from a first search call to the end; check every shown line."""
    shown = []
    calls = 0
    tool = "search"
    while True:
        text, err = s.call(tool, args)
        calls += 1
        if err:
            bad("search error %s: %s" % (args, text))
            return None
        last = check_size(text, args.get("max_tokens", 3000), "search %s" % json.dumps(args))
        # (similar lines as "≈ N"/"= N", made whole again: then checked as every line is; the
        # size is the answer's as sent)
        text = mcpc.expand(text)
        for line in text.split("\n")[:-1]:
            m = REF.match(line)
            if not m:
                continue
            rid, n, t = int(m.group(2)), int(m.group(3)), m.group(4)
            stats["refs"] += 1
            if m.group(1) not in (None, PID) or rid not in runs:
                bad("a reference to an unknown run: %s" % line)
                continue
            exp = mcpc.shown(line_bytes(rid, n), args.get("line_max", 400))
            if t != exp:
                bad("search %s: %s/r%d:%d shows %r, the original is %r" % (json.dumps(args)[:100], PID, rid, n, t[:120], exp[:120]))
            shown.append((rid, n))
        if last.startswith("all shown"):
            break
        tool, args = next_args(last)
        if tool != "search" or calls > 400:
            bad("bad next call: %s" % last)
            return None
    stats["cursor_calls"] += calls - 1
    if shown != want:
        bad("search %s: following cursors gave %d lines, want %d (first difference at %s)" % (json.dumps(args)[:120], len(shown), len(want), next(((a, b) for a, b in zip(shown, want) if a != b), None)))
    return calls


for qi in range(N):
    rid = rnd.choice(order[:-1])  # not the empty file
    d = data[rid]
    if not d:
        continue
    n = rnd.randint(1, nlines(rid))
    l = line_bytes(rid, n).rstrip(b"\n")
    if len(l) < 3:
        l = b"user"
    a = rnd.randrange(max(1, len(l) - 3))
    k = rnd.randint(3, 40) if rnd.random() < 0.9 else rnd.randint(41, 120)
    q = l[a:a + k].replace(b"\0", b"").replace(b"\r", b"").replace(b"\x1b", b"") or b"user"
    icase = rnd.random() < 0.2
    whole = rnd.random() < 0.15
    if icase:
        q = bytes((c ^ 32) if (65 <= c <= 90 or 97 <= c <= 122) and rnd.random() < 0.5 else c for c in q)
    if rnd.random() < 0.1:
        j = rnd.randrange(len(q) + 1)
        q = q[:j] + b"qzxj" + q[j:]
    try:
        qs = q.decode()
    except UnicodeDecodeError:
        continue
    want = {r: matches(r, q, icase, whole) for r in order}
    total = sum(len(v) for v in want.values())
    base = {"text": qs}
    if icase:
        base["icase"] = True
    if whole:
        base["whole"] = True
    # count: exact per run
    text, err = s.call("count", dict(base, max_tokens=20000))
    stats["count"] += 1
    got = {}
    for line in text.split("\n"):
        m = re.match(r"^(?:(\S+)/)?r(\d+) ([\d,]+)  ", line)
        if m:
            got[int(m.group(2))] = int(m.group(3).replace(",", ""))
    exp = {r: len(v) for r, v in want.items() if v}
    if err or got != exp:
        bad("count %s: got %s, want %s" % (json.dumps(base), got, exp))
    head = text.split("\n")[0]
    if total == 0:
        if not head.startswith("no line holds") and not head.startswith("0 lines"):
            if got:
                bad("count %s: heading %r" % (json.dumps(base), head))
    elif not head.startswith("{:,} line".format(total)):
        bad("count %s: heading %r, want %d lines" % (json.dumps(base), head, total))
    # search, followed to the end (not for huge results)
    if qi % 3 == 0 and total <= 3000:
        stats["search"] += 1
        flat = [(r, ln) for r in order for ln in want[r]]
        # (every matching line on its own: lines with the same key are not collapsed)
        if total <= 300:
            args = dict(base, limit=rnd.randint(1, 60), max_tokens=rnd.randint(200, 3000), collapse=False)
        else:
            args = dict(base, limit=500, max_tokens=20000, collapse=False)
        if rnd.random() < 0.3:
            args["line_max"] = rnd.randint(80, 600)
        search_all(args, flat)
    # search with context: the lines around each hit are the original's
    if qi % 7 == 1 and 0 < total <= 3000:
        stats["context"] += 1
        c = rnd.randint(1, 4)
        text, err = s.call("search", dict(base, context=c, limit=10, max_tokens=8000))
        text = mcpc.expand(text)
        cur = None
        for line in text.split("\n")[:-1]:
            m = re.match(r"^(?:(\S+)/)?r(\d+):(\d+)-(\d+)$", line)
            if m:
                cur = int(m.group(2))
                continue
            m = re.match(r"^(> |  )(\d+) (.*)$", line)
            if m and cur is not None:
                ln = int(m.group(2))
                if m.group(3) != mcpc.shown(line_bytes(cur, ln)):
                    bad("context %s: r%d:%d shows %r" % (json.dumps(base), cur, ln, m.group(3)[:100]))
                if (m.group(1) == "> ") != (ln in want[cur]):
                    # a "> " line is a hit; a context line may hold the text only before the first hit
                    if m.group(1) == "> ":
                        bad("context %s: r%d:%d marked as a hit but holds no match" % (json.dumps(base), cur, ln))
    # lines, raw, base64, expect around one hit
    if qi % 5 == 2 and total > 0:
        stats["lines"] += 1
        r = rnd.choice([r for r in order if want[r]])
        ln = rnd.choice(want[r])
        b, af = rnd.randint(0, 6), rnd.randint(0, 6)
        ref = "%s/r%d:%d" % (PID, r, ln)
        text, err = s.call("lines", {"ref": ref, "before": b, "after": af, "max_tokens": 20000, "line_max": 65536})
        rows = [re.match(r"^(> |  )(\d+) (.*)$", x) for x in text.split("\n")[1:-1]]
        rows = [x for x in rows if x]
        want_rows = list(range(max(1, ln - b), min(nlines(r), ln + af) + 1))
        if err or [int(x.group(2)) for x in rows] != want_rows:
            bad("lines %s: rows %s, want %s" % (ref, [int(x.group(2)) for x in rows], want_rows))
        for x in rows:
            if x.group(3) != mcpc.shown(line_bytes(r, int(x.group(2))), 65536):
                bad("lines %s: line %s shows %r" % (ref, x.group(2), x.group(3)[:100]))
        # raw and base64: the exact bytes
        text, err = s.call("lines", {"ref": "%s/r%d:%d-%d" % (PID, r, max(1, ln - 1), ln), "before": 0, "after": 0, "raw": True, "base64": True, "max_tokens": 20000, "line_max": 65536})
        m = re.search(r"^base64 of lines (\d+)-(\d+) \(([\d,]+) bytes, line ends included\): (\S*)$", text, re.M)
        exact = b"".join(line_bytes(r, x) for x in range(max(1, ln - 1), ln + 1))
        if not m or base64.b64decode(m.group(4)) != exact:
            bad("base64 of %s differs" % ref)
        for x in re.finditer(r"^(> |  )(\d+) (.*)$", text, re.M):
            if x.group(3) != cut(raw_escape(line_bytes(r, int(x.group(2)))), 65536):
                bad("raw %s line %s: %r" % (ref, x.group(2), x.group(3)[:100]))
        # expect: same bytes, and the first differing byte of a changed copy
        orig = line_bytes(r, ln)
        orig = orig[:-1] if orig.endswith(b"\n") else orig
        orig = orig[:-1] if orig.endswith(b"\r") else orig
        try:
            o = orig.decode()
        except UnicodeDecodeError:
            o = None
        if o is not None and len(orig) > 0:
            stats["expect"] += 1
            text, err = s.call("lines", {"ref": ref, "before": 0, "after": 0, "expect": o, "max_tokens": 20000, "line_max": 65536})
            if "expect: same bytes" not in text:
                bad("expect %s: %s" % (ref, text[:200]))
            j = rnd.randrange(len(orig))
            if orig[j] < 0x80:
                ch = b"#" if orig[j] != ord("#") else b"%"
                changed = orig[:j] + ch + orig[j + 1:]
                text, err = s.call("lines", {"ref": ref, "before": 0, "after": 0, "expect": changed.decode(), "max_tokens": 20000, "line_max": 65536})
                if "expect: differs at byte %d:" % (j + 1) not in text:
                    bad("expect %s changed at byte %d: %s" % (ref, j + 1, text[:300]))

# follow: whole-token matches per run, oldest first
ids = set()
for r in order:
    for m in re.finditer(rb"(?:blk_-?\d{6,}|req-\d{5}|\b\d+\.\d+\.\d+\.\d+\b|[A-Za-z]+\d{4,})", data[r][:300000]):
        ids.add(m.group(0))
ids = sorted(ids)
rnd.shuffle(ids)
for idb in ids[:60]:
    if len(idb) < 4 or len(idb) > 200:
        continue
    want = {r: matches(r, idb, False, True) for r in order}
    total = sum(len(v) for v in want.values())
    if total > 3000:
        continue
    stats["follows"] += 1
    flat = [(r, ln) for r in sorted(order) for ln in want[r]]
    args = {"id": idb.decode(), "limit": rnd.choice([5, 50, 500]), "max_tokens": rnd.choice([400, 3000, 20000])}
    shown, calls, tool = [], 0, "follow"
    heads = {}
    while True:
        text, err = s.call(tool, args)
        calls += 1
        last = check_size(text, args.get("max_tokens", 3000), "follow %s" % args["id"])
        # (similar lines as "≈ N"/"= N", made whole again: then checked as every line is)
        text = mcpc.expand(text)
        for line in text.split("\n")[:-1]:
            m = REF.match(line)
            if m:
                rr, ln = int(m.group(2)), int(m.group(3))
                if m.group(4) != mcpc.shown(line_bytes(rr, ln)):
                    bad("follow %s: r%d:%d shows %r" % (args["id"], rr, ln, m.group(4)[:80]))
                shown.append((rr, ln))
            h = re.match(r"^== (?:\S+/)?r(\d+) .*: ([\d,]+) matching$", line)
            if h:
                heads[int(h.group(1))] = int(h.group(2).replace(",", ""))
        if last.startswith("all shown"):
            break
        tool, args = next_args(last)
        if tool != "follow" or calls > 200:
            bad("follow: bad next call %s" % last)
            break
    if shown != flat:
        bad("follow %s: %d lines, want %d" % (idb, len(shown), len(flat)))
    for r, c in heads.items():
        # (a heading after a cursor counts the lines from the cursor on)
        if c > len(want[r]):
            bad("follow %s: r%d heading says %d, Python %d" % (idb, r, c, len(want[r])))

# logs: following its cursor lists every run once, newest first
listed, args, calls = [], {"limit": 7, "max_tokens": 400}, 0
while True:
    text, err = s.call("logs", args)
    calls += 1
    last = check_size(text, args["max_tokens"], "logs")
    listed += [int(m.group(1)) for m in re.finditer(r"^(?:\S+/)?r(\d+) ", text, re.M)]
    if last.startswith("all shown") or calls > 50:
        break
    _, args = next_args(last)
if listed != order:
    bad("logs listed %s, want %s" % (listed, order))
# count without a text: every run's lines
text, err = s.call("count", {"max_tokens": 20000})
got = {int(m.group(2)): int(m.group(3).replace(",", "")) for m in re.finditer(r"^(?:(\S+)/)?r(\d+) ([\d,]+)  ", text, re.M)}
if got != {r: nlines(r) for r in order}:
    bad("count of all lines: %s" % got)

code = s.close()
if code != 0:
    bad("exit status %s" % code)

# the time limit: every block made slow (a test switch), the shortest limit; following the
# cursors still gives every matching line once, in order, and an answer cut by the time
# limit says "at least"
s = mcpc.Server(SEARCH, dict(env, FEWER_TEST_SLOW_BLOCK_MS="30"), PROJ, WORK + "/stderr_slow.txt")
s.initialize()
stopped_answers = 0
for q in (b"session opened", b"Failed password", b"unix", b"node-", b"req-00042", b"State change"):
    want = {r: matches(r, q, False, False) for r in order}
    flat = [(r, ln) for r in order for ln in want[r]]
    if len(flat) > 30000:
        continue
    args = {"text": q.decode(), "limit": 500, "max_tokens": 20000, "time_limit_ms": 100, "collapse": False}
    shown, calls = [], 0
    while True:
        text, err = s.call("search", args)
        calls += 1
        if text.startswith("at least"):
            stopped_answers += 1
        last = check_size(text, 20000, "slow search")
        text = mcpc.expand(text)
        for line in text.split("\n")[:-1]:
            m = REF.match(line)
            if m:
                rr, ln = int(m.group(2)), int(m.group(3))
                if m.group(4) != mcpc.shown(line_bytes(rr, ln)):
                    bad("slow search %s: r%d:%d shows %r" % (q, rr, ln, m.group(4)[:80]))
                shown.append((rr, ln))
        if last.startswith("all shown"):
            break
        _, nargs = next_args(last)
        if nargs is None or nargs == args or calls > 2000:
            bad("slow search %s: no progress: %s" % (q, last))
            break
        args = nargs
    stats["cursor_calls"] += calls - 1
    if shown != flat:
        bad("slow search %s: %d lines, want %d" % (q, len(shown), len(flat)))
# a cursor inside a job whose first matches lie before it (they spend the job's kept bytes),
# with the time limit cutting a later block of the run: each next call still moves on and
# shows the next lines (it once handed back the same cursor forever, showing nothing)
for name, q in (("SSH.log", b"sshd"), ("HPC.log", b"node-")):
    rid = next((r for r, p in runs.items() if os.path.basename(p) == name), None)
    if rid is None:
        continue
    want = matches(rid, q, False, False)
    args = {"text": q.decode(), "runs": ["r%d" % rid], "limit": 20, "max_tokens": 20000, "time_limit_ms": 100, "collapse": False}
    got = []
    for _ in range(4):
        text, err = s.call("search", args)
        if text.startswith("at least"):
            stopped_answers += 1
        last = check_size(text, 20000, "slow dense search")
        text = mcpc.expand(text)
        got += [int(m.group(3)) for m in (REF.match(l) for l in text.split("\n")[:-1]) if m]
        _, nargs = next_args(last)
        if last.startswith("all shown"):
            break
        if nargs is None or nargs == args:
            bad("slow dense search %s in %s: no progress: %s" % (q, name, last[:200]))
            break
        args = nargs
    if got != want[:len(got)] or len(got) < min(len(want), 80):
        bad("slow dense search %s in %s: shown %d lines %r..., want %r..." % (q, name, len(got), got[:3], want[:3]))
# count cut by the time limit, followed through its cursors: it ends, and its rows add up to
# the exact count (a cut count once went back to the start of its run: on a run longer than
# the limit it never ended, and it counted lines twice)
for q in (b"session opened", b"node-"):
    want = sum(len(matches(r, q, False, False)) for r in order)
    args = {"text": q.decode(), "time_limit_ms": 100}
    total, calls = 0, 0
    while True:
        text, err = s.call("count", args)
        calls += 1
        if text.startswith("at least"):
            stopped_answers += 1
        last = check_size(text, 3000, "slow count")
        for line in text.split("\n")[1:-1]:
            m = re.match(r"^(?:\S+/)?r\d+ ([\d,]+)", line)
            if m:
                total += int(m.group(1).replace(",", ""))
        if last.startswith("all shown"):
            break
        _, nargs = next_args(last)
        if nargs is None or nargs == args or calls > 500:
            bad("slow count %s: no progress: %s" % (q, last[:200]))
            break
        args = nargs
    if total != want:
        bad("slow count %s: rows add up to %d in %d calls, want %d" % (q, total, calls, want))
if stopped_answers == 0:
    bad("no answer was cut by the time limit")
stats["stopped_answers"] = stopped_answers
code = s.close()
if code != 0:
    bad("exit status %s" % code)
print("oracle: %d runs; %s; %d failed checks" % (len(runs), ", ".join("%s %d" % kv for kv in stats.items()), len(fails)))
if fails:
    sys.exit(1)
print("oracle: ALL EXACT")
