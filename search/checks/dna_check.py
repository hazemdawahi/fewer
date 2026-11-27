"""dna_check.py: the acceptance check of the DNA index (its shrunk design), run against a
fewer that has it.

    python3 -I checks/dna_check.py SEARCH PACK WORK [--queries N] [--hpc FILE] [--keep]

SEARCH is fewer, PACK the fewer-pack program that packs, WORK a new
scratch folder (a folder this check made before is cleared; any other is refused). --hpc names a
real multi-block log to add (default: data/loghub_unseen/HPC.log when it is there; it is only
read). Everything is written in WORK: the store, its config, the debug logs, the made-up logs;
HOME and the agents' folders point into WORK too. Without --keep, the made-up logs and the store
are deleted at the end (the debug log and the summary stay).

1. Settings and saving: on by default; `add` of a run under 16 MiB of text starts nothing; of one
   of 16 MiB or more, a detached pass in another process at background priority, which records
   dna_bytes and dna_build_ms; `run` (then pack-pending) the same; with `memory dna off` nothing
   is built and search uses no .dna (the debug log's find lines); `memory dna build` refuses
   while it is off and builds the missing ones when it is on.
2. The idle builder (the MCP server's): nothing while the load is high (FEWER_TEST_LOAD replaces
   getloadavg), at background priority; once the load is low, the biggest run without one.
3. Not used, with a note in the debug log, and answers unchanged: another archive's .dna, a stale
   one (its archive changed), a damaged one, one of another version; `memory dna build` makes a
   stale one again.
4. The same answers: N random queries (part of a word, part of an ID, hex-like, short, several
   words, absent, UTF-8; with and without -i) through the MCP tools search (cursors followed once),
   count and follow, over multi-block archives, a joined archive, a real log and runs without a
   DNA index: the same stdout bytes with the DNA index on, off, and on with a step budget of 40
   (FEWER_TEST_DNA_STEPS, on the first quarter). Per call, never more blocks opened with it (the
   debug log's find lines), and fewer on word pieces in all. A share of the counts equals fewer-pack
   grep's.
5. The size cap (dna_max, 2026-11-23), in a store of its own with the default cap (20%): the
   random-ID log, whose DNA index would take about 83% of its archive, gets none after saving: its
   run records dna_would_be, no dna_error; `memory dna` says so and `memory dna build` leaves it;
   with dna_max = none, `memory dna build` makes it. A 17 MiB `run` whose index is tiny (under
   1 MiB) keeps it, whatever its share. Parts 1 to 4 run with dna_max = none, so they build every
   index as before.
"""
import importlib.util
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mcpc)

MIB = 1 << 20
FAILS = []
NOTES = []


def check(cond, what):
    if not cond:
        FAILS.append(what)
        print("FAIL:", what, flush=True)
    return cond


def say(*a):
    print(*a, flush=True)


# ---------------------------------------------------------------- made-up logs

NAMES = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa"]
UTF = ["café", "naïve", "smörgås", "Ærø", "日本語", "über", "señal", "Δelta"]
B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"


def b62(r, n):
    return "".join(r.choice(B62) for _ in range(n))


def gen_phases(path, n, seed):
    """A log of n lines whose words change from region to region (so blocks differ), with hex
    IDs, 0x values, numbers, IDs with letters past f, UTF-8 words and messages."""
    r = random.Random(seed)
    out = []
    for i in range(n):
        phase = NAMES[(i * 8) // n]
        level = ["INFO", "INFO", "INFO", "WARN", "ERROR", "DEBUG"][r.randrange(6)]
        extra = ""
        k = r.randrange(10)
        if k == 0:
            extra = " conn=0x%x" % r.getrandbits(24)
        elif k == 1:
            extra = " session=%s" % b62(r, 12)
        elif k == 2 and (i * 8) // n in (2, 3, 6):
            extra = " note=%s" % UTF[(i * 8) // n % len(UTF)]
        elif k == 3:
            extra = " msg=Connection refused by peer%d" % r.randrange(40)
        out.append("2026-11-18 10:%02d:%02d %s worker%d %s%dq request id=%x took %dms user=u%s%s\n" % (
            i // 60 % 60, i % 60, level, i % 9, phase, i % 50, r.getrandbits(40), r.randint(1, 900), r.choice(["kim", "lee", "zoe", "max"]), extra))
    with open(path, "w", encoding="utf-8") as f:
        f.write("".join(out))


def gen_ids(path, n, seed):
    """A log of trace and span IDs with letters past f, a few very long tokens (walks back of
    hundreds of steps), and op words by region."""
    r = random.Random(seed)
    ops = ["fetch", "store", "render", "commit", "rollback", "query"]
    out = []
    for i in range(n):
        op = ops[(i * 6) // n]
        line = "%d op=%s%d " % (1763460000 + i // 10, op, i % 30)
        if i % 10 == 0:
            line += "trace=%s span=%s " % (b62(r, 20), b62(r, 10))
        if i % 1500 == 7:
            line += "blob=%s " % b62(r, r.randint(300, 2500))
        line += "status=%s\n" % r.choice(["ok", "retry", "failed", "slow"])
        out.append(line)
    with open(path, "w") as f:
        f.write("".join(out))


def gen_plain(path, size, seed):
    """Text that is not a log: paragraphs of made-up words in lines of every length."""
    r = random.Random(seed)
    vocab = ["".join(r.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(r.randint(2, 11))) for _ in range(4000)]
    vocab += UTF
    out, total = [], 0
    while total < size:
        words = [r.choice(vocab) for _ in range(r.randint(1, 40))]
        if r.randrange(5) == 0:
            words.insert(r.randrange(len(words)), str(r.getrandbits(30)))
        line = " ".join(words) + ("\n" if r.randrange(9) else "\n\n")
        out.append(line)
        total += len(line.encode())
    with open(path, "w", encoding="utf-8") as f:
        f.write("".join(out))


def gen_small(path, size, seed, words):
    r = random.Random(seed)
    out, total, i = [], 0, 0
    while total < size:
        line = "%06d step %s%d done in %d ms code=%x\n" % (i, r.choice(words), i % 20, r.randint(1, 99), r.getrandbits(20))
        out.append(line)
        total += len(line)
        i += 1
    with open(path, "w") as f:
        f.write("".join(out))


# ---------------------------------------------------------------- the store

class Store:
    def __init__(self, search, fewer, work, name):
        self.search, self.fewer = search, fewer
        self.dir = os.path.join(work, name)
        self.home = os.path.join(self.dir, "home")
        self.proj = os.path.join(self.dir, "proj")
        self.fake = os.path.join(self.dir, "fakehome")
        self.log = os.path.join(self.dir, "debug.log")
        self.loadf = os.path.join(self.dir, "load")
        for d in [self.home, self.proj, self.fake]:
            os.makedirs(d)

    def env(self, **extra):
        e = {k: v for k, v in os.environ.items() if not k.startswith("FEWER_")}
        e.update(HOME=self.fake, FEWER_HOME=self.home, FEWER_PACK_BIN=self.fewer, FEWER_PROJECT=self.proj, FEWER_DEBUG_LOG=self.log,
                 FEWER_CLAUDE_DIR=os.path.join(self.fake, "claude"), FEWER_CODEX_DIR=os.path.join(self.fake, "codex"),
                 FEWER_CLINE_DIR=os.path.join(self.fake, "cline"), FEWER_CURSOR_DIR=os.path.join(self.fake, "cursor"),
                 FEWER_CLAUDE_BIN=os.path.join(self.fake, "no-claude"))
        e.update({k: str(v) for k, v in extra.items()})
        return e

    def cmd(self, args, ok=True, **extra):
        p = subprocess.run([self.search] + args, cwd=self.proj, env=self.env(**extra), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if ok and p.returncode != 0:
            raise SystemExit("%s failed (%d): %s %s" % (" ".join(args), p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")))
        return p

    def pid(self):
        return os.listdir(os.path.join(self.home, "projects"))[0]

    def pdir(self):
        return os.path.join(self.home, "projects", self.pid())

    def runs(self):
        out = {}
        with open(os.path.join(self.pdir(), "runs.jsonl"), "rb") as f:
            for line in f.read().split(b"\n"):
                if not line.strip():
                    continue
                try:
                    v = json.loads(line)
                except ValueError:
                    continue
                out.setdefault(v["id"], {}).update(v)
        return out

    def archive(self, rid):
        return os.path.join(self.pdir(), "r%06d.cmf" % rid)

    def dna(self, rid):
        return os.path.join(self.pdir(), "r%06d.dna" % rid)

    def add(self, path, **extra):
        before = set(self.runs()) if os.path.isdir(os.path.join(self.home, "projects")) else set()
        p = subprocess.Popen([self.search, "add", path], cwd=self.proj, env=self.env(**extra), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        out, err = p.communicate()
        if p.returncode != 0:
            raise SystemExit("add %s failed: %s" % (path, err.decode(errors="replace")))
        new = sorted(set(self.runs()) - before)
        return new[-1], p.pid

    def log_lines(self):
        try:
            with open(self.log, "rb") as f:
                return f.read().decode(errors="replace").splitlines()
        except FileNotFoundError:
            return []

    def server(self, **extra):
        s = mcpc.Server(self.search, self.env(**extra), self.proj, os.path.join(self.dir, "server-stderr.txt"))
        s.initialize()
        return s


def wait_for(pred, timeout, step=0.2):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(step)
    return pred()


def find_lines(lines):
    """The debug log's find lines: (archive, blocks, opened, by the .idx, by the DNA index)."""
    out = []
    for l in lines:
        m = re.search(r"\] find: (.*): (\d+) blocks: (\d+) opened, (\d+) left unopened by the \.idx, (\d+) more by the DNA index$", l)
        if m:
            out.append((m.group(1), int(m.group(2)), int(m.group(3)), int(m.group(4)), int(m.group(5))))
    return out


def per_call(lines):
    """The blocks opened and left unopened by the DNA index in each tool call, from the debug log:
    find lines up to the call's own "tools/call" line."""
    calls, opened, by_dna = [], 0, 0
    for l in lines:
        f = find_lines([l])
        if f:
            opened += f[0][2]
            by_dna += f[0][4]
        elif "] tools/call " in l:
            calls.append((opened, by_dna))
            opened, by_dna = 0, 0
    return calls


# ---------------------------------------------------------------- part 1: settings and saving

def part1(st, data):
    say("== 1. settings and saving")
    p = st.cmd(["memory", "dna"])
    check("The DNA index is on (the default)" in p.stdout.decode(), "memory dna says on by default: %r" % p.stdout.decode())
    # a run under 16 MiB: nothing starts
    r1, _ = st.add(data["small"])
    time.sleep(3)
    check(not os.path.exists(st.dna(r1)) and "dna_bytes" not in st.runs()[r1], "a run under 16 MiB gets no DNA index")
    check(not any("dna pass for %s/r%d" % (st.pid(), r1) in l for l in st.log_lines()), "no pass starts for a run under 16 MiB")
    # a run of 16 MiB or more: a detached pass, in another process, at background priority
    r2, add_pid = st.add(data["phases"])
    check(wait_for(lambda: os.path.exists(st.dna(r2)) and "dna_bytes" in st.runs()[r2], 120), "the detached pass builds the DNA index of a 16 MiB run")
    rec = st.runs()[r2]
    check(rec.get("dna_bytes") == os.path.getsize(st.dna(r2)), "dna_bytes is the file's size")
    check(isinstance(rec.get("dna_build_ms"), int) and rec["dna_build_ms"] >= 0, "dna_build_ms is recorded")
    starts = [l for l in st.log_lines() if "dna pass for %s/r%d: started" % (st.pid(), r2) in l]
    check(len(starts) == 1, "one pass for r%d: %r" % (r2, starts))
    if starts:
        pass_pid = int(re.search(r"\[(\d+)\]", starts[0]).group(1))
        check(pass_pid != add_pid, "the pass is another process than add (%d, %d)" % (pass_pid, add_pid))
        check("at background priority" in starts[0], "the pass runs at background priority: %s" % starts[0])
        NOTES.append("pass after add: " + starts[0].split("] ", 1)[1])
    # off: nothing is built, and search uses no .dna
    p = st.cmd(["memory", "dna", "off"])
    check("off" in p.stdout.decode() and "dna = off" in open(os.path.join(st.home, "config")).read(), "memory dna off")
    r3, _ = st.add(data["ids"])
    time.sleep(3)
    check(not os.path.exists(st.dna(r3)) and "dna_bytes" not in st.runs()[r3], "with the DNA index off, a 16 MiB run gets none")
    p = st.cmd(["memory", "dna", "build"], ok=False)
    check(p.returncode == 1 and "off" in p.stdout.decode(), "memory dna build refuses while off: %r" % p.stdout.decode())
    n0 = len(st.log_lines())
    s = st.server()
    s.call("count", {"text": "olf", "time_limit_ms": 120000})
    s.close()
    fl = [f for f in find_lines(st.log_lines()[n0:]) if f[0].endswith("r%06d.cmf" % r2)]
    check(len(fl) == 1 and fl[0][4] == 0 and fl[0][2] > 0, "with the DNA index off, search uses no .dna: %r" % fl)
    check(not any("] dna: " in l for l in st.log_lines()[n0:]), "with it off, no .dna is read")
    # on: build the missing one now
    st.cmd(["memory", "dna", "on"])
    p = st.cmd(["memory", "dna"])
    check("1 run of that size without one yet" in p.stdout.decode(), "memory dna counts the run without one: %r" % p.stdout.decode())
    p = st.cmd(["memory", "dna", "build"])
    check("built %s/r%d" % (st.pid(), r3) in p.stdout.decode() and os.path.exists(st.dna(r3)), "memory dna build builds it: %r" % p.stdout.decode())
    check(st.runs()[r3].get("dna_bytes") == os.path.getsize(st.dna(r3)), "memory dna build records it")
    n0 = len(st.log_lines())
    s = st.server()
    s.call("count", {"text": "olf", "time_limit_ms": 120000})
    s.close()
    fl = [f for f in find_lines(st.log_lines()[n0:]) if f[0].endswith("r%06d.cmf" % r2)]
    check(len(fl) == 1 and fl[0][4] > 0, "with it on, the DNA index leaves blocks unopened: %r" % fl)
    # through `run` and pack-pending: 17 MiB gets one, 2 MiB none
    for mib, wants in [(17, True), (2, False)]:
        before = set(st.runs())
        prog = "import sys\nw=sys.stdout.write\ni=0\nn=0\nwhile n < %d:\n    s='%%07d run output line of step%%dq for uzoe\\n' %% (i, i %% 13)\n    w(s)\n    n += len(s)\n    i += 1\n" % (mib * MIB)
        subprocess.run([st.search, "run", "--", sys.executable, "-I", "-c", prog], cwd=st.proj, env=st.env(), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
        new = sorted(set(st.runs()) - before)
        check(len(new) == 1, "run made one run")
        rid = new[-1]
        check(wait_for(lambda: st.runs()[rid].get("state") == "ready", 120), "pack-pending packed r%d" % rid)
        if wants:
            check(wait_for(lambda: os.path.exists(st.dna(rid)) and "dna_bytes" in st.runs()[rid], 120), "a 17 MiB run's DNA index comes from the pass pack-pending starts")
        else:
            time.sleep(3)
            check(not os.path.exists(st.dna(rid)), "a 2 MiB run gets none")
    return r1, r2, r3


# ---------------------------------------------------------------- part 2: the idle builder

def part2(st, data):
    say("== 2. the idle builder")
    st.cmd(["memory", "dna", "off"])
    rid, _ = st.add(data["hpc"])
    st.cmd(["memory", "dna", "on"])
    check(not os.path.exists(st.dna(rid)), "a run saved while off has none")
    with open(st.loadf, "w") as f:
        f.write("99\n")
    n0 = len(st.log_lines())
    s = st.server(FEWER_TEST_LOAD=st.loadf, FEWER_TEST_DNA_IDLE_MS=300)
    time.sleep(3)
    check(not os.path.exists(st.dna(rid)), "while the load is high the idle builder builds nothing")
    lines = st.log_lines()[n0:]
    started = [l for l in lines if "dna idle builder: started" in l]
    check(len(started) == 1 and "at background priority" in started[0], "the idle builder runs at background priority: %r" % started)
    check(any("dna idle builder: not idle (1-minute load 99.00" in l for l in lines), "it says it waits for idle")
    with open(st.loadf, "w") as f:
        f.write("0\n")
    check(wait_for(lambda: os.path.exists(st.dna(rid)) and "dna_bytes" in st.runs()[rid], 120), "once the load is low, the idle builder builds the run without one")
    s.close()
    lines = st.log_lines()[n0:]
    check(any("dna idle builder: built %s/r%d" % (st.pid(), rid) in l for l in lines), "it says what it built")
    if started:
        NOTES.append("idle builder: " + started[0].split("] ", 1)[1])
    return rid


# ---------------------------------------------------------------- part 3: files not used

def part3(st, data, r2, r3):
    say("== 3. files that do not fit are not used")
    # a run whose archive will change: its DNA index made first (dna_min lowered for it)
    with open(os.path.join(st.home, "config"), "a") as f:
        f.write("dna_min = 1MiB\n")
    rj, _ = st.add(data["join1"])
    check(wait_for(lambda: os.path.exists(st.dna(rj)), 120), "the joined run's first archive gets its DNA index")
    text = open(os.path.join(st.home, "config")).read().replace("dna_min = 1MiB\n", "")
    with open(os.path.join(st.home, "config"), "w") as f:
        f.write(text)
    # join a second archive to it (cat a.cmf b.cmf), its .idx made again, its record updated
    second = os.path.join(st.dir, "join2.cmf")
    subprocess.run([st.fewer, "-q", "-o", second, data["join2"]], check=True)
    with open(st.archive(rj), "ab") as f, open(second, "rb") as g:
        f.write(g.read())
    os.remove(second)
    st.cmd(["index", st.archive(rj)])
    cat = os.path.join(st.pdir(), "r%06d.cat" % rj)
    if os.path.exists(cat):
        os.remove(cat)
    b1, b2 = open(data["join1"], "rb").read(), open(data["join2"], "rb").read()
    rec = {"id": rj, "bytes": len(b1) + len(b2), "lines": (b1 + b2).count(b"\n"), "archive": os.path.getsize(st.archive(rj))}
    with open(os.path.join(st.pdir(), "runs.jsonl"), "a") as f:
        f.write(json.dumps(rec) + "\n")
    queries = [("count", {"text": "ovember", "time_limit_ms": 120000}), ("count", {"text": "olf", "icase": True, "time_limit_ms": 120000}),
               ("search", {"text": "eflat", "limit": 7, "time_limit_ms": 120000}), ("search", {"text": "ilo3", "limit": 5, "time_limit_ms": 120000}),
               ("count", {"text": "ender2", "time_limit_ms": 120000}), ("follow", {"id": "uzoe", "limit": 3, "time_limit_ms": 120000})]

    def answers():
        n0 = len(st.log_lines())
        s = st.server(FEWER_TEST_LOAD=st.loadf)
        out = [s.call(t, a) for t, a in queries]
        s.close()
        return out, st.log_lines()[n0:]

    # the stale one: its archive changed
    with open(st.loadf, "w") as f:
        f.write("99\n")
    st.cmd(["memory", "dna", "off"])
    base, _ = answers()
    st.cmd(["memory", "dna", "on"])
    got, lines = answers()
    check(got == base, "a stale .dna: the same answers")
    check(any("r%06d.dna: not used: made for another archive, or before this one changed" % rj in l for l in lines), "a stale .dna is not used, with a note")
    good = open(st.dna(r2), "rb").read()
    cases = [("another archive's", open(st.dna(r3), "rb").read(), "made for another archive"),
             ("a damaged", good[:len(good) // 2] + bytes([good[len(good) // 2] ^ 0x20]) + good[len(good) // 2 + 1:], "damaged (its checksum does not match)"),
             ("a cut", good[:len(good) - 9], "damaged"),
             ("another version's", good[:4] + bytes([9]) + good[5:], "made by another version (DNA index version 9")]
    for what, bad, note in cases:
        with open(st.dna(r2), "wb") as f:
            f.write(bad)
        got, lines = answers()
        check(got == base, "%s .dna: the same answers" % what)
        check(any(("r%06d.dna: not used: " % r2) in l and note in l for l in lines), "%s .dna is not used, with a note (%s)" % (what, note))
    with open(st.dna(r2), "wb") as f:
        f.write(good)
    got, lines = answers()
    check(got == base, "the good .dna back: the same answers")
    fl = [f for f in find_lines(lines) if f[0].endswith("r%06d.cmf" % r2)]
    check(any(f[4] > 0 for f in fl), "the good .dna back: blocks left unopened again")
    p = st.cmd(["memory", "dna", "build"])
    check("built %s/r%d" % (st.pid(), rj) in p.stdout.decode(), "memory dna build makes the stale one again: %r" % p.stdout.decode())
    return rj


# ---------------------------------------------------------------- part 4: the same answers

TOK = re.compile(rb"[A-Za-z0-9\x80-\xff]+")


def token_pool(paths, r, per=4000):
    words, ids, hexes, lines = [], [], [], []
    for p in paths:
        with open(p, "rb") as f:
            data = f.read()
        ls = data.split(b"\n")
        for _ in range(per):
            l = ls[r.randrange(len(ls))]
            if not l:
                continue
            lines.append(l)
            for t in TOK.findall(l):
                if re.fullmatch(rb"[0-9a-fA-F]+", t):
                    hexes.append(t)
                elif re.fullmatch(rb"[A-Za-z\x80-\xff]+", t):
                    words.append(t)
                else:
                    ids.append(t)
    return words, ids, hexes, lines


def utf8_piece(r, b):
    """A piece of b made of whole UTF-8 characters (so a JSON string can carry it)."""
    s = b.decode("utf-8", errors="ignore")
    if not s:
        return "x"
    a = r.randrange(len(s))
    return s[a:a + r.randint(1, 8)] or s


def make_queries(r, pool, n):
    words, ids, hexes, lines = pool
    out = []
    kinds = ["word", "word", "word", "id", "id", "hex", "short", "multi", "absent", "utf"]
    for i in range(n):
        kind = kinds[i % len(kinds)]
        if kind == "word":
            w = r.choice(words)
            a = r.randrange(max(1, len(w) - 2))
            q = utf8_piece(r, w[a:a + r.randint(3, 10)]) if len(w) >= 3 else w.decode("utf-8", "ignore")
        elif kind == "id":
            t = r.choice(ids)
            a = r.randrange(len(t))
            q = utf8_piece(r, t[a:a + r.randint(3, 14)])
            if r.randrange(4) == 0:
                q = r.choice(["=", "_", "id=", "u", "op="]) + q
        elif kind == "hex":
            q = r.choice([r.choice(hexes)[: r.randint(2, 8)].decode(), "0x%x" % r.getrandbits(12), str(r.getrandbits(r.randint(4, 30))), "%x" % r.getrandbits(16)])
        elif kind == "short":
            q = utf8_piece(r, r.choice(words + ids))[: r.randint(1, 2)]
        elif kind == "multi":
            l = r.choice(lines)
            a = r.randrange(len(l))
            q = utf8_piece(r, l[a:a + r.randint(5, 25)])
        elif kind == "absent":
            q = "".join(r.choice("ghjkmnpqrstuvwyz") for _ in range(r.randint(4, 9)))
        else:
            q = r.choice(UTF)
            q = q[: r.randint(1, len(q))]
        q = q.replace("\n", " ").strip("\r") or "zq"
        icase = r.randrange(3) == 0
        if icase:
            q = "".join(c.upper() if r.randrange(2) else c for c in q)
        out.append((kind, q, icase))
    return out


def calls_for(r, queries):
    """The tool calls: search (some with collapse off or context), count (some by key), follow."""
    out = []
    for i, (kind, q, icase) in enumerate(queries):
        k = i % 5
        if k == 4 and 4 <= len(q) <= 200:
            out.append((kind, q, icase, "follow", {"id": q, "limit": r.randint(1, 40), "time_limit_ms": 120000}))
        elif k in (0, 1):
            a = {"text": q, "icase": icase, "limit": r.randint(1, 40), "time_limit_ms": 120000}
            if r.randrange(3) == 0:
                a["collapse"] = False
            if r.randrange(10) == 0:
                a["context"] = 1
            out.append((kind, q, icase, "search", a))
        else:
            a = {"text": q, "icase": icase, "time_limit_ms": 120000}
            if r.randrange(6) == 0:
                a["by"] = "key"
            out.append((kind, q, icase, "count", a))
    return out


NEXT = re.compile(r"Next: search (\{.*\})\s*$")


def session(st, calls, **extra):
    """Every call in one server session, the search cursors followed once (with the same long
    time limit): the stdout lines, the first answers, and each call's blocks (opened, left
    unopened by the DNA index), its cursor's call included."""
    n0 = len(st.log_lines())
    s = st.server(FEWER_TEST_LOAD=st.loadf, **extra)
    texts, made = [], []
    for _, _, _, tool, args in calls:
        text, _ = s.call(tool, args)
        texts.append(text)
        made.append(1)
        m = NEXT.search(text) if tool == "search" else None
        if m:
            try:
                nxt = json.loads(m.group(1))
            except ValueError:
                nxt = None
            if isinstance(nxt, dict):
                nxt["time_limit_ms"] = 120000
                s.call("search", nxt)
                made[-1] += 1
    s.close()
    each = per_call(st.log_lines()[n0:])
    blocks, k = [], 0
    for n in made:
        part = each[k:k + n]
        blocks.append((sum(x[0] for x in part), sum(x[1] for x in part)))
        k += n
    check(k == len(each), "one tools/call line per call in the debug log (%d, %d)" % (k, len(each)))
    return s.lines, texts, blocks


def grep_count(st, q, icase, archives):
    args = [st.fewer, "grep"] + (["-i"] if icase else []) + ["--", q] + archives
    p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return p.stdout.count(b"\n")


def part4(st, data, nq, seed):
    say("== 4. the same answers with and without the DNA index (%d queries)" % nq)
    with open(st.loadf, "w") as f:
        f.write("99\n")
    st.cmd(["memory", "dna", "on"])
    p = st.cmd(["memory", "dna", "build"])
    say(p.stdout.decode().strip().splitlines()[-1])
    st.cmd(["memory", "dna"])
    r = random.Random(seed)
    pool = token_pool([data[k] for k in ["phases", "ids", "plain", "hpc", "join1", "join2", "small"] if data.get(k)], r)
    queries = make_queries(r, pool, nq)
    calls = calls_for(r, queries)
    t0 = time.time()
    a_lines, a_texts, a_blocks = session(st, calls)
    say("  with the DNA index: %d calls, %.0f s" % (len(a_blocks), time.time() - t0))
    st.cmd(["memory", "dna", "off"])
    b_lines, b_texts, b_blocks = session(st, calls)
    say("  without it: %d calls" % len(b_blocks))
    st.cmd(["memory", "dna", "on"])
    quarter = calls[: max(1, nq // 4)]
    c_lines, c_texts, c_blocks = session(st, quarter, FEWER_TEST_DNA_STEPS=40)
    say("  with a step budget of 40: %d calls" % len(c_blocks))
    # the same stdout bytes
    check(len(a_lines) == len(b_lines), "as many stdout lines on and off (%d, %d)" % (len(a_lines), len(b_lines)))
    diff = [i for i, (x, y) in enumerate(zip(a_lines, b_lines)) if x != y]
    check(not diff, "the same stdout bytes on and off: %d lines differ, first %r" % (len(diff), diff[:3]))
    diff_c = [i for i, (x, y) in enumerate(zip(c_lines, b_lines[: len(c_lines)])) if x != y]
    check(len(c_lines) <= len(b_lines) and not diff_c, "the same stdout bytes with a tiny step budget: %d lines differ" % len(diff_c))
    # never more blocks opened, fewer on word pieces
    check(len(a_blocks) == len(b_blocks), "as many calls in both debug logs (%d, %d)" % (len(a_blocks), len(b_blocks)))
    more = [i for i, (a, b) in enumerate(zip(a_blocks, b_blocks)) if a[0] > b[0]]
    check(not more, "never more blocks opened with the DNA index: %d calls opened more, first %r" % (len(more), more[:3]))
    more_c = [i for i, (c, b) in enumerate(zip(c_blocks, b_blocks)) if c[0] > b[0]]
    check(not more_c, "never more blocks opened with a tiny budget: %d calls" % len(more_c))
    worse = [i for i, (a, c) in enumerate(zip(a_blocks, c_blocks)) if a[0] > c[0]]
    check(not worse, "a smaller budget never opens fewer blocks: %d calls" % len(worse))
    # per kind: blocks opened on and off (calls carry their query's kind; follow-ups count with their call)
    by_kind = {}
    for (kind, _, _, tool, _), a, b in zip(calls, a_blocks, b_blocks):
        k = by_kind.setdefault(kind, [0, 0, 0, 0, 0])
        k[0] += a[0]
        k[1] += b[0]
        k[2] += 1
        k[3] += a[0] < b[0]
        k[4] += a[1]
    # (a block the engine's own filter leaves unopened is not "opened" either, but the filter
    # decodes its patterns, texts and words first; a block the DNA index leaves unopened is
    # not decoded at all)
    for kind in sorted(by_kind):
        a, b, n, fewer, dna_out = by_kind[kind]
        say("  %-6s %4d calls: %6d blocks opened with the DNA index, %6d without (%d calls opened fewer); %d left unopened by it" % (kind, n, a, b, fewer, dna_out))
    w = by_kind.get("word", [0, 0, 0, 0, 0])
    check(w[0] < w[1], "fewer blocks opened on word pieces: %d against %d" % (w[0], w[1]))
    tot_a, tot_b = sum(x[0] for x in a_blocks), sum(x[0] for x in b_blocks)
    say("  all: %d blocks opened with the DNA index, %d without; %d left unopened by it" % (tot_a, tot_b, sum(x[1] for x in a_blocks)))
    NOTES.append("equivalence: %d queries, %d calls with cursors; blocks opened %d with the DNA index against %d without; word pieces %d against %d" % (nq, len(a_blocks), tot_a, tot_b, w[0], w[1]))
    # the counts are grep's (a share of them, against fewer-pack grep on the store's archives)
    archives = sorted(os.path.join(st.pdir(), f) for f in os.listdir(st.pdir()) if f.endswith(".cmf"))
    n_or = 0
    for (kind, q, icase, tool, args), text in zip(calls, a_texts):
        if tool != "count" or "by" in args or n_or >= max(20, nq // 25):
            continue
        m = re.match(r"([\d,]+) lines? holds?", text)
        if not check(m is not None, "a count answer starts with its total: %r" % text[:80]):
            continue
        want = grep_count(st, q, icase, archives)
        check(int(m.group(1).replace(",", "")) == want, "count %r (icase %s) is fewer-pack grep's %d: %s" % (q, icase, want, text.splitlines()[0]))
        n_or += 1
    say("  %d counts equal fewer-pack grep's" % n_or)
    return len(a_blocks)


# ---------------------------------------------------------------- part 5: the size cap

def part5(search, fewer, work, data):
    say("== 5. the size cap (dna_max)")
    st = Store(search, fewer, work, "store-cap")
    rid, _ = st.add(data["ids"])
    check(wait_for(lambda: "dna_would_be" in st.runs()[rid], 120), "the pass records the size the random-ID log's index would take")
    rec = st.runs()[rid]
    would = rec.get("dna_would_be", 0)
    check(would > MIB and would * 100 > 20 * rec.get("archive", 1), "it is over 20%% of the archive and over 1 MiB: %d of %d" % (would, rec.get("archive", 0)))
    check(not os.path.exists(st.dna(rid)) and "dna_bytes" not in rec and "dna_error" not in rec, "over the cap: no DNA index kept, and no error recorded")
    out = st.cmd(["memory", "dna"]).stdout.decode()
    check("1 run over that size: no DNA index kept for it." in out and "more than 20% of its run's archive" in out, "memory dna says so: %r" % out)
    out = st.cmd(["memory", "dna", "build"]).stdout.decode()
    check("but 1 run over the size cap" in out and not os.path.exists(st.dna(rid)), "memory dna build leaves it: %r" % out)
    # a run whose index is tiny keeps it, whatever its share (the 1 MiB floor)
    before = set(st.runs())
    prog = "import sys\nw=sys.stdout.write\ni=0\nn=0\nwhile n < %d:\n    s='%%07d run output line of step%%dq for uzoe\\n' %% (i, i %% 13)\n    w(s)\n    n += len(s)\n    i += 1\n" % (17 * MIB)
    subprocess.run([st.search, "run", "--", sys.executable, "-I", "-c", prog], cwd=st.proj, env=st.env(), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    tiny = sorted(set(st.runs()) - before)[-1]
    check(wait_for(lambda: os.path.exists(st.dna(tiny)) and "dna_bytes" in st.runs()[tiny], 120), "a 17 MiB run with a tiny index keeps it")
    t = st.runs()[tiny]
    say("  under the cap: r%d's index %d bytes, %.2f%% of its %d-byte archive (kept: under 1 MiB)" % (tiny, t.get("dna_bytes", 0), 100.0 * t.get("dna_bytes", 0) / max(1, t.get("archive", 1)), t.get("archive", 0)))
    # the cap lifted: built
    with open(os.path.join(st.home, "config"), "a") as f:
        f.write("dna_max = none\n")
    out = st.cmd(["memory", "dna", "build"]).stdout.decode()
    check(os.path.exists(st.dna(rid)) and st.runs()[rid].get("dna_bytes") == os.path.getsize(st.dna(rid)), "with dna_max = none, memory dna build makes it: %r" % out)
    say("  over the cap: r%d's index would take %d bytes, %.2f%% of its %d-byte archive: none kept until dna_max = none" % (rid, would, 100.0 * would / max(1, rec.get("archive", 1)), rec.get("archive", 0)))
    shutil.rmtree(st.home, ignore_errors=True)


def main():
    a = sys.argv[1:]
    if len(a) < 3:
        sys.exit(__doc__)
    search, fewer, work = (os.path.abspath(x) for x in a[:3])
    nq, keep = 2000, False
    # (loghub's HPC.log, when the developer has it: --hpc FILE or FEWER_HPC_LOG; the check that
    # needs it is passed over without it)
    hpc = os.environ.get("FEWER_HPC_LOG", "")
    rest = a[3:]
    while rest:
        o = rest.pop(0)
        if o == "--queries":
            nq = int(rest.pop(0))
        elif o == "--hpc":
            hpc = rest.pop(0)
        elif o == "--keep":
            keep = True
        else:
            sys.exit("unknown option %s" % o)
    mcpc.fresh_work(work)
    t0 = time.time()
    dd = os.path.join(work, "data")
    os.makedirs(dd)
    data = {"phases": os.path.join(dd, "phases.log"), "ids": os.path.join(dd, "ids.log"), "plain": os.path.join(dd, "plain.txt"),
            "join1": os.path.join(dd, "join1.log"), "join2": os.path.join(dd, "join2.log"), "small": os.path.join(dd, "small.log")}
    gen_phases(data["phases"], 520000, 1)
    gen_ids(data["ids"], 450000, 2)
    gen_plain(data["plain"], 18 * MIB, 3)
    gen_small(data["join1"], 9 * MIB, 4, ["kilo", "lima", "mike"])
    gen_small(data["join2"], 9 * MIB, 5, ["november", "oscar", "papa"])
    gen_small(data["small"], 15 * MIB + MIB // 2, 6, ["quebec", "romeo"])
    if os.path.isfile(hpc):
        data["hpc"] = hpc
    else:
        data["hpc"] = os.path.join(dd, "hpc-made-up.log")
        gen_phases(data["hpc"], 300000, 7)
        say("(no %s: a made-up log stands in for it)" % hpc)
    say("made-up logs in %s (%.0f s)" % (dd, time.time() - t0))
    st = Store(search, fewer, work, "store")
    # (parts 1 to 4 build every index, whatever its size: part 5 checks the cap)
    with open(os.path.join(st.home, "config"), "w") as f:
        f.write("dna_max = none\n")
    r1, r2, r3 = part1(st, data)
    part2(st, data)
    rp, _ = st.add(data["plain"])
    check(wait_for(lambda: os.path.exists(st.dna(rp)), 120), "the plain text run gets its DNA index")
    part3(st, data, r2, r3)
    part4(st, data, nq, 11)
    part5(search, fewer, work, data)
    # sizes and build times in the runs
    runs = st.runs()
    rows = []
    for rid in sorted(runs):
        rec = runs[rid]
        if "dna_bytes" in rec:
            check(rec["dna_bytes"] == os.path.getsize(st.dna(rid)) and isinstance(rec.get("dna_build_ms"), int), "r%d's record holds its DNA index's size and build time" % rid)
            rows.append("r%d %s: %d bytes of text, archive %d, DNA index %d (%.2f%%), built in %d ms" % (rid, rec.get("kind"), rec.get("bytes", 0), rec.get("archive", 0), rec["dna_bytes"], 100.0 * rec["dna_bytes"] / max(1, rec.get("archive", 1)), rec["dna_build_ms"]))
    for row in rows:
        say("  " + row)
    if not keep:
        shutil.rmtree(dd, ignore_errors=True)
        shutil.rmtree(st.home, ignore_errors=True)
    with open(os.path.join(work, "summary.txt"), "w") as f:
        f.write("\n".join(NOTES + rows + ["FAILS: %d" % len(FAILS)] + FAILS) + "\n")
    say("\n".join(NOTES))
    say("%s in %.0f s" % ("ALL PASSED" if not FAILS else "%d FAILED" % len(FAILS), time.time() - t0))
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
