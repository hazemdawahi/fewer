"""catalog_check.py SEARCH PACK WORK DATA...: the catalog's acceptance checks (python3 -I).

DATA: folders or files of copies of logs whose records are one line each (the 16 Loghub
`*_2k` samples and the JSON samples; never the data folder itself: they are copied into
WORK first). WORK: a scratch folder the check makes (and clears only when a check made it).

1. keys: checks/msgkey_ref.py --check against `SEARCH keys` on every file: the same key for
   every line, and the same counts per key.
2. whole and pieces: each log saved whole, and cut into 15-minute pieces by its lines' times
   (read here in Python; at most 50 pieces), each piece saved as a run of its own: per key,
   the records of the pieces' catalogs add up to the whole run's (`keys --cat`), with 0
   differences; and the count tool by key gives the same table for the pieces as for the
   whole. By hour too, except where a piece learns another time column than the whole (each
   piece is a source of its own): those are printed as notes.
3. catalogs: the same bytes saved twice (the file, then a copy of it with the same name, mode
   and time: the file itself again is "already saved") give the same catalog bytes; `catalog` builds the
   same bytes again from the archive; a catalog is refused for another archive and when one
   byte is changed (`keys --cat` fails), and the tools then make it again from the run's
   bytes, say so, and give the same counts.
4. counts: `count` by key and by hour (without a text: records; with texts: lines) equals a
   count over the original lines here (keys by msgkey_ref.py, times read here), and so do the
   per-key and per-hour lines of search answers that do not show every line.
5. collapse: search answers with lines collapsed by key, followed through their cursors:
   each group's first line, count and last line equal the grouping here of the answer's
   matching lines, every shown line is the original's, and every match is covered once.
"""
import calendar, collections, importlib.util, json, os, random, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


mcpc = load("mcpc")
ref = load("msgkey_ref")
safe_env = load("safe_env")

SEARCH, PACK, WORK = (os.path.abspath(a) for a in sys.argv[1:4])
DATA = [os.path.abspath(a) for a in sys.argv[4:]]
if not DATA:
    raise SystemExit(__doc__)
mcpc.fresh_work(WORK)
# every process this check starts, the server too, gets a made-up HOME, agents' folders and
# claude, inside WORK (safe_env.py)
safe = safe_env.setup(WORK)
HOME, FILES = WORK + "/home", WORK + "/files"
os.makedirs(FILES)
rnd = random.Random(4)
failures = []
stats = collections.Counter()


def bad(msg):
    failures.append(msg)
    if len(failures) <= 30:
        print("FAIL:", msg, flush=True)


# ---------------------------------------------------------------- the files

srcs = []
for d in DATA:
    if os.path.isdir(d):
        srcs += [os.path.join(d, f) for f in sorted(os.listdir(d)) if not f.startswith(".")]
    else:
        srcs.append(d)
files = []
for s in srcs:
    dst = os.path.join(FILES, os.path.basename(s))
    shutil.copy2(s, dst)  # (the modification time too: years of dates written without one)
    files.append(dst)


def split_lines(b):
    return ref.split_lines(b)


# ---------------------------------------------------------------- times, read here

MON = {m: i + 1 for i, m in enumerate("Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split())}


def local_ms(y, mo, d, h, mi, s, ms=0):
    """Unix ms of a wall-clock time in this computer's zone (the C library's mktime)."""
    return int(time.mktime((y, mo, d, h, mi, s, 0, 0, -1))) * 1000 + ms


def file_year_month(path):
    t = time.localtime(os.stat(path).st_mtime)
    return t.tm_year, t.tm_mon


def reader(path):
    """A function giving a line's own time (Unix ms) or None, for the samples' forms."""
    base = os.path.basename(path)
    fy, fm = file_year_month(path)

    def no_year(mo):
        return fy - 1 if mo > fm else fy

    def iso(m):
        return local_ms(int(m[1]), int(m[2]), int(m[3]), int(m[4]), int(m[5]), int(m[6]), int(m[7] or 0))

    def unix_at(k):
        def f(line):
            t = line.split()
            if len(t) > k and re.fullmatch(rb"\d{10}", t[k]):
                return int(t[k]) * 1000
            return None
        return f

    def rx(p, conv):
        c = re.compile(p)

        def f(line):
            m = c.match(line)
            return conv([None] + [g.decode() if g is not None else None for g in m.groups()]) if m else None
        return f

    syslog = rx(rb"(\w{3}) +(\d{1,2}) (\d\d):(\d\d):(\d\d) ", lambda g: local_ms(no_year(MON[g[1]]), MON[g[1]], int(g[2]), int(g[3]), int(g[4]), int(g[5])) if g[1] in MON else None)
    iso_comma = rx(rb"(\d{4})-(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d),(\d{3})", iso)
    if base.endswith(".jsonl"):
        inner = {
            "Android": rx(rb"(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\.(\d{3})$", lambda g: local_ms(no_year(int(g[1])), int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]))),
            "Apache": rx(rb"\w{3} (\w{3}) (\d\d) (\d\d):(\d\d):(\d\d) (\d{4})$", lambda g: local_ms(int(g[6]), MON[g[1]], int(g[2]), int(g[3]), int(g[4]), int(g[5]))),
            "HDFS": rx(rb"(\d\d)(\d\d)(\d\d) (\d\d)(\d\d)(\d\d)$", lambda g: local_ms(2000 + int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]))),
            "SSH": rx(rb"(\w{3}) +(\d{1,2}) (\d\d):(\d\d):(\d\d)$", lambda g: local_ms(no_year(MON[g[1]]), MON[g[1]], int(g[2]), int(g[3]), int(g[4]), int(g[5]))),
        }[base.split(".")[0]]

        def f(line):
            try:
                t = json.loads(line).get("time")
            except ValueError:
                return None
            return inner(t.encode()) if isinstance(t, str) else None
        return f
    stem = base.split(".")[0].split("_")[0]
    return {
        "Android": rx(rb"(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\.(\d{3})", lambda g: local_ms(no_year(int(g[1])), int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]))),
        "Apache": rx(rb"\[\w{3} (\w{3}) (\d\d) (\d\d):(\d\d):(\d\d) (\d{4})\]", lambda g: local_ms(int(g[6]), MON[g[1]], int(g[2]), int(g[3]), int(g[4]), int(g[5]))),
        "BGL": unix_at(1),
        "HDFS": rx(rb"(\d\d)(\d\d)(\d\d) (\d\d)(\d\d)(\d\d) ", lambda g: local_ms(2000 + int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]))),
        "HPC": unix_at(4),
        "Hadoop": iso_comma,
        "HealthApp": rx(rb"(\d{4})(\d\d)(\d\d)-(\d{1,2}):(\d{1,2}):(\d{1,2}):(\d{1,3})\|", lambda g: local_ms(int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]), int(g[7]))),
        "Linux": syslog,
        "Mac": syslog,
        "OpenSSH": syslog,
        "SSH": syslog,
        "OpenStack": rx(rb"\S+ (\d{4})-(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\.(\d{3}) ", iso),
        "Proxifier": rx(rb"\[(\d\d)\.(\d\d) (\d\d):(\d\d):(\d\d)\]", lambda g: local_ms(no_year(int(g[1])), int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]))),
        "Spark": rx(rb"(\d\d)/(\d\d)/(\d\d) (\d\d):(\d\d):(\d\d) ", lambda g: local_ms(2000 + int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]))),
        "Thunderbird": unix_at(1),
        "Windows": rx(rb"(\d{4})-(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d),", lambda g: local_ms(int(g[1]), int(g[2]), int(g[3]), int(g[4]), int(g[5]), int(g[6]))),
        "Zookeeper": iso_comma,
    }[stem]


def line_times(path, lines):
    """Each line's time: its own, else the previous line's (None before the first)."""
    f = reader(path)
    out, last = [], None
    for l in lines:
        t = f(l.rstrip(b"\r\n"))
        if t is not None:
            last = t
        out.append(last)
    return out


# ---------------------------------------------------------------- shown as the tools show it


def key_label(k, mx):
    """A key as the count tables show it (clean.rs escape, cut at mx bytes with an ellipsis)."""
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


def hour_label(h):
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(h * 3600))


# ---------------------------------------------------------------- the store

PROJ = WORK + "/proj"
os.makedirs(PROJ)
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ)


def add(paths):
    out = subprocess.run([SEARCH, "add"] + paths, env=env, cwd=PROJ, capture_output=True)
    if out.returncode != 0:
        raise SystemExit("add failed: %s" % out.stderr.decode())
    ids = []
    for line in out.stdout.decode().splitlines():
        m = re.match(r"saved (.*) as (\S+)/r(\d+): ", line)
        ids.append(int(m.group(3)))
        global PID
        PID = m.group(2)
    return ids


def pdir():
    return os.path.join(HOME, "projects", PID)


def cat_path(rid):
    return os.path.join(pdir(), "r%06d.cat" % rid)


def keys_cat(rid):
    r = subprocess.run([SEARCH, "keys", "--cat", cat_path(rid)], capture_output=True)
    return r.returncode, r.stdout, r.stderr.decode()


def summed(rows):
    """Rows of a table summed by label (two keys may share the label shown, their first bytes)."""
    c = collections.Counter()
    for k, n in rows or []:
        c[k] += n
    return c


notes = []


def dump_counts(b):
    c = collections.Counter()
    for line in b.split(b"\n"):
        if line:
            n, k = line.split(b"\t", 1)
            c[k] += int(n)
    return c


# ---------------------------------------------------------------- 1. keys against the reference

print("1. keys: msgkey_ref.py --check against `fewer keys`", flush=True)
r = subprocess.run([sys.executable, "-I", os.path.join(HERE, "msgkey_ref.py"), "--check", "--bin", SEARCH + " keys"] + files, capture_output=True)
print(r.stdout.decode().strip().split("\n")[-1])
if r.returncode != 0:
    bad("msgkey_ref.py --check: %s" % r.stdout.decode()[-2000:])
stats["files"] = len(files)

# ---------------------------------------------------------------- the runs

whole = dict(zip(files, add(files)))  # file -> run id
data = {}
for f in files:
    b = open(f, "rb").read()
    lines = split_lines(b)
    data[f] = (lines, [ref.line_key(l) for l in lines], line_times(f, lines))

# ---------------------------------------------------------------- 2. whole and pieces

print("2. whole and 15-minute pieces", flush=True)
srv = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
srv.initialize()


def table(args):
    """Every row of a count table, through its "#N" cursors: [(label, count)]."""
    rows = []
    # (500 rows an answer: a count table shows 10 by default since the token test of
    # 2026-11-23, 24 by hour)
    args = dict(args, max_tokens=20000, limit=500)
    for _ in range(200):
        t, err = srv.call("count", args)
        if err:
            bad("count %s: %s" % (json.dumps(args), t[:300]))
            return None
        lines = t.split("\n")
        for l in lines[1:-1]:
            m = re.match(r"^ *([\d,]+)  (.*?)(  \((?:[\d,]+ lines(?:; first \S+)?|first \S+)\))?$", l)
            if m:
                rows.append((m.group(2), int(m.group(1).replace(",", ""))))
        last = lines[-1]
        if last.startswith("all shown"):
            return rows
        # (a table's next rows: the same call with the row cursor, since 2026-11-23; a count
        # stopped by the time limit gives its whole next call)
        c = re.search(r'next: the same call with "cursor":"(#\d+)"$', last)
        m = re.search(r"Next: count (\{.*\})$", last)
        if c:
            args = dict(args, cursor=c.group(1))
        elif m:
            args = json.loads(m.group(1))
        else:
            bad("count %s: last line %r" % (json.dumps(args), last))
            return None
    bad("count %s: too many calls" % json.dumps(args))
    return None


for f in files:
    lines, keys, times = data[f]
    base, ext = os.path.splitext(os.path.basename(f))
    cuts, prev = [], None
    for i, t in enumerate(times):
        w = None if t is None else t // 900_000
        if i > 0 and w is not None and prev is not None and w != prev:
            cuts.append(i)
        if w is not None:
            prev = w
    cuts = cuts[:49]
    pieces = []
    for n, (a, b) in enumerate(zip([0] + cuts, cuts + [len(lines)])):
        p = os.path.join(FILES, "pieces", "%s.p%03d%s" % (base, n, ext))
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "wb").write(b"".join(lines[a:b]))
        st = os.stat(f)
        os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))
        pieces.append(p)
    ids = add(pieces)
    stats["pieces"] += len(ids)
    code, wd, err = keys_cat(whole[f])
    if code != 0:
        bad("keys --cat of %s: %s" % (f, err))
        continue
    total = collections.Counter()
    for rid in ids:
        code, d, err = keys_cat(rid)
        if code != 0:
            bad("keys --cat of a piece: %s" % err)
        total.update(dump_counts(d))
    want = dump_counts(wd)
    diff = sum(abs(total[k] - want[k]) for k in set(total) | set(want))
    if diff:
        bad("%s: the pieces' key counts differ from the whole's by %d" % (os.path.basename(f), diff))
    stats["piece_differences"] += diff
    for by in ("key", "hour"):
        tw = table({"by": by, "runs": ["r%d" % whole[f]]})
        tp = table({"by": by, "runs": ["r%d" % i for i in ids]})
        if tw is None or tp is None:
            continue
        cw, cp = summed(tw), summed(tp)
        if cw != cp:
            if by == "key":
                bad("%s: count by key differs between the whole and the pieces" % os.path.basename(f))
            else:
                # each piece is a source of its own and learns its own time column from its own
                # first lines (formats::detect, times::learn_place): where a few lines write
                # their time in another column, a piece of them reads it and the whole does not
                d = sum(abs(cw[k] - cp[k]) for k in set(cw) | set(cp))
                notes.append("%s: by hour, the pieces differ from the whole in %d records (a piece learns its own time column)" % (os.path.basename(f), d // 2))
    print("  %-22s %3d pieces, %4d keys, %d differences" % (os.path.basename(f), len(ids), len(want), diff), flush=True)

# ---------------------------------------------------------------- 3. catalogs: the same bytes, refused for others

print("3. catalogs built twice, and refused for another archive", flush=True)
f0, f1 = files[0], files[1]
# (the same file added again is "already saved": its bytes are saved a second
# time from a copy with the same name, mode and time in another folder)
copy = os.path.join(FILES, "again", os.path.basename(f0))
os.makedirs(os.path.dirname(copy), exist_ok=True)
shutil.copy2(f0, copy)
again = add([copy])[0]
a = open(cat_path(whole[f0]), "rb").read()
if open(cat_path(again), "rb").read() != a:
    bad("the same file saved twice gives different catalogs")
runs_json = {}
for line in open(os.path.join(pdir(), "runs.jsonl"), "rb"):
    o = json.loads(line)
    runs_json.setdefault(o["id"], {}).update(o)
out = WORK + "/again.cat"
r = subprocess.run([SEARCH, "catalog", "--name", runs_json[whole[f0]]["source"], os.path.join(pdir(), "r%06d.cmf" % whole[f0]), out], capture_output=True)
if r.returncode != 0 or open(out, "rb").read() != a:
    bad("`catalog` does not build the same bytes again: %s" % r.stderr.decode())
stats["catalogs_same"] += 2
before = table({"by": "key", "runs": ["r%d" % whole[f1]]})
keep = open(cat_path(whole[f1]), "rb").read()
open(cat_path(whole[f1]), "wb").write(a)  # another archive's catalog
code, _, err = keys_cat(whole[f1])
if code == 0 or "another archive" not in err:
    bad("a catalog of another archive is not refused: %s" % err)
t, _ = srv.call("count", {"by": "key", "runs": ["r%d" % whole[f1]], "max_tokens": 20000})
if "catalogs made now" not in t or "another archive" not in t:
    bad("the tools do not say they made the catalog now: %s" % t[:300])
if table({"by": "key", "runs": ["r%d" % whole[f1]]}) != before:
    bad("counts from a catalog made now differ")
d = bytearray(keep)
d[len(d) // 2] ^= 0x20
open(cat_path(whole[f1]), "wb").write(bytes(d))
code, _, err = keys_cat(whole[f1])
if code == 0 or "damaged" not in err:
    bad("a damaged catalog is not refused: %s" % err)
if table({"by": "key", "runs": ["r%d" % whole[f1]]}) != before:
    bad("counts with a damaged catalog differ")
open(cat_path(whole[f1]), "wb").write(keep)
stats["refused"] += 2

# ---------------------------------------------------------------- 4. counts against Python


def py_counts(f, sel):
    """(per key label, per hour label, no time) over the lines whose index is in sel."""
    lines, keys, times = data[f]
    kc = collections.Counter()
    hc = collections.Counter()
    none = 0
    for i in sel:
        kc[key_label(keys[i], 300)] += 1
        if times[i] is None:
            none += 1
        else:
            hc[times[i] // 3_600_000] += 1
    hours = [(hour_label(h), n) for h, n in sorted(hc.items())]
    if none:
        hours.append(("no time", none))
    return kc, hours


print("4. counts by key and by hour against a count of the original lines", flush=True)
for f in files:
    lines, keys, times = data[f]
    rid = whole[f]
    kc, hours = py_counts(f, range(len(lines)))
    tk = table({"by": "key", "runs": ["r%d" % rid]})
    if tk is None or summed(tk) != kc or len(tk) != len(set(keys)):
        bad("%s: count by key differs from Python's" % os.path.basename(f))
    th = table({"by": "hour", "runs": ["r%d" % rid]})
    if th != hours:
        bad("%s: count by hour differs from Python's (%s against %s)" % (os.path.basename(f), (th or [])[:3], hours[:3]))
    stats["tables"] += 2
    # with texts: matching lines
    texts = [b"INFO", b"error", b"block", b"user"]
    for _ in range(4):
        l = lines[rnd.randrange(len(lines))].rstrip(b"\r\n")
        a0 = rnd.randrange(max(1, len(l) - 4))
        texts.append(l[a0:a0 + rnd.randint(3, 12)])
    for q in texts:
        try:
            qs = q.decode()
        except UnicodeDecodeError:
            continue
        if not qs or "\n" in qs:
            continue
        sel = [i for i, l in enumerate(lines) if q in l]
        kc, hours = py_counts(f, sel)
        tk = table({"by": "key", "text": qs, "runs": ["r%d" % rid]})
        th = table({"by": "hour", "text": qs, "runs": ["r%d" % rid]})
        if tk is None or summed(tk) != kc:
            bad("%s: count by key of %r differs" % (os.path.basename(f), qs))
        if th != hours:
            bad("%s: count by hour of %r differs: %s against %s" % (os.path.basename(f), qs, (th or [])[:3], hours[:3]))
        stats["text_tables"] += 2
        # the search answer's per-key and per-hour lines, when it shows not every line
        # (summary: true: since the token test of 2026-11-23 a search answer has them only when
        # asked)
        t, _ = srv.call("search", {"text": qs, "runs": ["r%d" % rid], "limit": 1, "max_tokens": 20000, "summary": True})
        if "left out" in t.split("\n")[-1] and not re.search(r"^per (hour|key)", t, re.M):
            bad("%s: search %r with summary: true has no per key or per hour line" % (os.path.basename(f), qs))
        m = re.search(r"^per hour: (.*)$", t, re.M)
        if m:
            body, _, none = m.group(1).partition("; no time ")
            # (more than 6 hours, since 2026-11-23: their number, the span and the busiest
            # hour, the earliest of a tie; else every hour with its count)
            short = re.match(r"^([\d,]+) hours?, from (\S+ \S+) to (\S+ \S+), the most ([\d,]+) at (\S+ \S+) \(all: .*\)$", body)
            timed = [h for h in hours if h[0] != "no time"]
            if short:
                peak = max(enumerate(timed), key=lambda x: (x[1][1], -x[0]))[1]
                got = (int(short.group(1).replace(",", "")), short.group(2), short.group(3), int(short.group(4).replace(",", "")), short.group(5))
                want = (len(timed), timed[0][0], timed[-1][0], peak[1], peak[0]) if timed else None
                if got != want:
                    bad("%s: search %r per hour (short): %s against %s" % (os.path.basename(f), qs, got, want))
                got = list(timed)
            else:
                got, day = [], ""
                for item in body.split(", "):
                    mm = re.match(r"^(?:(\d{4}-\d\d-\d\d) )?(\d\d:\d\d) ([\d,]+)$", item)
                    if mm.group(1):
                        day = mm.group(1)
                    got.append(("%s %s" % (day, mm.group(2)), int(mm.group(3).replace(",", ""))))
            if none:
                got.append(("no time", int(none.replace(",", ""))))
            if got != hours:
                bad("%s: search %r per hour: %s against %s" % (os.path.basename(f), qs, got[:3], hours[:3]))
            stats["search_per_hour"] += 1
        m = re.search(r"^per key \(([\d,]+) keys?\): (.*?)( \(\+[\d,]+ more: .*\))?$", t, re.M)
        if m:
            items = re.findall(r'([\d,]+) ("(?:[^"\\]|\\.)*")', m.group(2))
            shown = [(json.loads(k), int(n.replace(",", ""))) for n, k in items]
            # Python: per key (whole keys: two keys may share their first 160 bytes), the most
            # first, then in order of first appearance; then shown as the answer shows them
            order, cnt = [], collections.Counter()
            for i in sel:
                k = keys[i]
                if k not in cnt:
                    order.append(k)
                cnt[k] += 1
            want = [(key_label(k, 160), cnt[k]) for k in sorted(order, key=lambda k: -cnt[k])]
            if int(m.group(1).replace(",", "")) != len(order) or shown != want[:len(shown)]:
                diff = next((i for i, (x, y) in enumerate(zip(shown, want)) if x != y), None)
                bad("%s: search %r per key: item %s: %s against %s" % (os.path.basename(f), qs, diff, shown[diff:diff + 1] if diff is not None else shown[:1], want[diff:diff + 1] if diff is not None else want[:1]))
            stats["search_per_key"] += 1

# ---------------------------------------------------------------- 5. lines collapsed by key

print("5. search with lines collapsed by key, through its cursors", flush=True)
REF = re.compile(r"^(?:(\S+)/)?r(\d+):(\d+) (.*)$")
GROUP = re.compile(r"^  ×([\d,]+) with this key, to (?:(\S+)/)?r(\d+):(\d+)(; .*)?$")
order_runs = sorted(whole.values(), reverse=True)  # the scope's order: newest first
by_run = {whole[f]: f for f in files}


def matches(rid, q):
    lines = data[by_run[rid]][0]
    return [i + 1 for i, l in enumerate(lines) if q in l]


def collapse_checks(srv, n, extra):
    for qi in range(n):
        one_collapse(srv, qi, extra)


def one_collapse(srv, qi, extra):
    if True:
        f = rnd.choice(files)
        lines = data[f][0]
        l = lines[rnd.randrange(len(lines))].rstrip(b"\r\n")
        a0 = rnd.randrange(max(1, len(l) - 4))
        q = l[a0:a0 + rnd.randint(3, 10)] if qi % 4 else rnd.choice([b"INFO", b"error", b"1", b"e"])
        try:
            qs = q.decode()
        except UnicodeDecodeError:
            return
        runs = ["r%d" % whole[g] for g in rnd.sample(files, min(len(files), rnd.randint(1, 4)))]
        scope = [r for r in order_runs if "r%d" % r in runs]
        flat = [(r, ln) for r in scope for ln in matches(r, q)]
        if len(flat) > 20000:
            return
        args = dict({"text": qs, "runs": runs, "limit": rnd.randint(1, 40), "max_tokens": rnd.choice([400, 1500, 3000, 8000])}, **extra)
        pos = 0  # matches covered so far (an index into flat)
        for call in range(3000):
            t, err = srv.call("search", args)
            if err:
                bad("search %s: %s" % (json.dumps(args), t[:200]))
                break
            mcpc_last = t.split("\n")[-1]
            body = t.split("\n")[:-1]
            if "stopped at the time limit" in t.split("\n")[0] or "stopped at the time limit" in "".join(body[:2]):
                stats["collapse_stopped_answers"] += 1
            if len("\n".join(body).encode()) > args["max_tokens"] * 4 + 400:
                bad("search %s: the answer is over the size cap" % json.dumps(args))
            groups = []  # (run, first line, count, last line)
            for line in body:
                m = REF.match(line)
                if m:
                    rid, ln = int(m.group(2)), int(m.group(3))
                    if m.group(4) != mcpc.shown(data[by_run[rid]][0][ln - 1]) and "…(+" not in m.group(4):
                        bad("search %s: r%d:%d shows %r" % (json.dumps(args), rid, ln, m.group(4)[:80]))
                    groups.append([rid, ln, 1, ln])
                    continue
                m = GROUP.match(line)
                if m and groups:
                    groups[-1][2] = int(m.group(1).replace(",", ""))
                    groups[-1][3] = int(m.group(4))
            # the next cursor: where this answer's range ends
            if mcpc_last.startswith("all shown"):
                end = len(flat)
                nxt = None
            else:
                m = re.search(r"Next: search (\{.*\})$", mcpc_last)
                nxt = json.loads(m.group(1))
                cm = re.match(r"^(?:\S+/)?r(\d+):(\d+)$", nxt["cursor"])
                cr, cl = int(cm.group(1)), int(cm.group(2))
                end = next((i for i, (r, ln) in enumerate(flat) if scope.index(r) > scope.index(cr) or (r == cr and ln >= cl)), len(flat))
            # the groups here: the answer's matches by run and key, in order of first appearance
            want = []
            seen = {}
            for r, ln in flat[pos:end]:
                k = (r, data[by_run[r]][1][ln - 1])
                if k in seen:
                    g = want[seen[k]]
                    g[2] += 1
                    g[3] = ln
                else:
                    seen[k] = len(want)
                    want.append([r, ln, 1, ln])
            if groups != want:
                bad("search %s: groups %s, want %s" % (json.dumps(args)[:150], groups[:3], want[:3]))
                break
            stats["groups"] += len(groups)
            pos = end
            if nxt is None:
                break
            if nxt == args:
                bad("search %s: the same cursor again" % json.dumps(args))
                break
            args = nxt
            stats["collapse_cursor_calls"] += 1
        if pos != len(flat):
            bad("search %r: following cursors covered %d of %d matches" % (qs, pos, len(flat)))
        stats["collapse_searches"] += 1
        stats["collapse_matches"] += len(flat)

collapse_checks(srv, 60, {})
# under the time limit (every block made slow by the test switch, the shortest limit): the
# answers cut by it still cover every match once, in exact groups
slow = mcpc.Server(SEARCH, dict(env, FEWER_TEST_SLOW_BLOCK_MS="30"), PROJ, WORK + "/stderr_slow.txt")
slow.initialize()
collapse_checks(slow, 20, {"time_limit_ms": 100})
if slow.close() != 0:
    bad("the slow server's exit status")

code = srv.close()
if code != 0:
    bad("the server's exit status: %s" % code)
print("stats:", dict(stats))
for n in notes:
    print("note:", n)
if failures:
    print("%d failed checks" % len(failures))
    sys.exit(1)
print("all passed")
