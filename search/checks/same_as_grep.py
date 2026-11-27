"""same_as_grep.py SEARCH PACK WORK DATA [N] [N_INDEXED]: `fewer grep` against
`fewer-pack grep`, and `fewer index` against `fewer-pack index` (run with python3 -I).

SEARCH is the fewer program, PACK the fewer-pack program (1.3.0), WORK a scratch folder
(made afresh), DATA a folder holding copies of the logs to use: the *_2k logs and the two
logs of several blocks (HPC.log and SSH.log). Built like the packer's own grep test:

- archives of each log, of edge files (CRLF, no final newline, empty lines, 00 bytes, a
  70 KB line, one line without a newline), a folder archive (files without a final newline,
  an empty file, blocks running across file ends) and joined archives (a line running from
  one archive into the next);
- N random queries (default 3000) drawn from the logs: pieces of random lines of 1 to 40
  bytes, some up to 120 (longer than the filter's 63), -i variants in random case, pieces made
  absent by an inserted letter, run on all cores or with -t 1, 2 or 3;
- then every archive is indexed (with fewer-pack index, and the bytes of fewer index are
  checked equal), and N_INDEXED more queries (default 1000) run with the indexes;
- stdout bytes and exit codes must be equal for every query.
"""
import importlib.util, os, random, shutil, subprocess, sys
from concurrent.futures import ThreadPoolExecutor

spec = importlib.util.spec_from_file_location("safe_env", os.path.join(os.path.dirname(os.path.abspath(__file__)), "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

SEARCH, PACK, WORK, DATA = (os.path.abspath(a) for a in sys.argv[1:5])
N = int(sys.argv[5]) if len(sys.argv) > 5 else 3000
NI = int(sys.argv[6]) if len(sys.argv) > 6 else 1000
rnd = random.Random(29)
# A folder that is there is cleared only when a check made it (it holds the mark file, as in
# mcpc.fresh_work): a wrong argument, such as the read-only data folder, is never deleted.
MARK = ".fewer-check-scratch"
if os.path.lexists(WORK):
    if not os.path.isfile(os.path.join(WORK, MARK)):
        sys.exit("%s is already there and no check made it (it has no %s): give a new scratch folder" % (WORK, MARK))
    shutil.rmtree(WORK)
os.makedirs(WORK)
open(os.path.join(WORK, MARK), "w").close()
# every process this check starts gets a made-up HOME, agents' folders and claude, inside WORK
# (safe_env.py)
safe = safe_env.setup(WORK)
env = safe.env(LC_ALL="C")
os.makedirs(WORK + "/fold/sub")


def write(p, b):
    with open(p, "wb") as f:
        f.write(b)
    return p


def head(p, n):
    out = []
    with open(p, "rb") as f:
        for i, l in enumerate(f):
            if i >= n:
                break
            out.append(l)
    return b"".join(out)


ins = [(os.path.join(DATA, f), 1) for f in sorted(os.listdir(DATA)) if f.endswith("_2k.log")]
big = [os.path.join(DATA, f) for f in ("HPC.log", "SSH.log") if os.path.exists(os.path.join(DATA, f))]
ins += [(p, 2) for p in big]
lines = [b"%d %s user%d from 10.0.%d.%d port %d ok" % (i, b"INFO" if i % 7 else b"WARN", i % 13, i % 256, i * 7 % 256, 40000 + i) for i in range(30000)]
ins += [
    (write(WORK + "/crlf.log", b"\r\n".join(lines[:5000]) + b"\r\n"), 1),
    (write(WORK + "/no_final_nl.log", b"\n".join(lines[:4000])), 1),
    (write(WORK + "/empty_lines.log", b"\n\n".join(lines[:3000]) + b"\n\n"), 1),
    (write(WORK + "/nul.log", b"\n".join(l + (b"\0x" if i % 5 == 0 else b"") for i, l in enumerate(lines[:3000])) + b"\n"), 1),
    (write(WORK + "/long_line.log", b"\n".join(lines[:2000]) + b"\n" + b"z" * 70000 + b" tail\n" + b"\n".join(lines[2000:4000]) + b"\n"), 1),
    (write(WORK + "/one.log", b"one line without a newline"), 1),
]
arch = {}
for p, _ in ins:
    a = os.path.join(WORK, "a_" + os.path.basename(p) + ".cmf")
    subprocess.run([PACK, "-q", "-f", "-o", a, p], check=True)
    arch[p] = a

files = {
    "a.log": head(big[0], 150000) if big else b"\n".join(lines) + b"\n",
    "b_nonl.log": b"\n".join(lines[:5]),
    "c.log": b"",
    "sub/d.log": head(big[-1], 300000) if big else b"\n".join(lines[::-1]) + b"\n",
    "sub/e_crlf.log": b"\r\n".join(lines[100:300]) + b"\r\n",
    "z_nonl.log": b"last file, no newline",
}
for k, v in files.items():
    write(os.path.join(WORK, "fold", k), v)
FA = WORK + "/fold.cmf"
subprocess.run([PACK, "-q", "-f", "-o", FA, WORK + "/fold"], check=True)

# joined archives: the first one's text ends without a newline
J = write(WORK + "/j1.log", b"first\nprefix qq")
subprocess.run([PACK, "-q", "-f", "-o", WORK + "/j1.cmf", J], check=True)
jsrc = big[0] if big else ins[0][0]
JA = write(WORK + "/joined.cmf", open(WORK + "/j1.cmf", "rb").read() + open(arch[jsrc], "rb").read())
all_archives = list(arch.values()) + [FA, JA]


def piece(data):
    s = rnd.randrange(len(data))
    n = rnd.randint(1, 40) if rnd.random() < 0.9 else rnd.randint(41, 120)
    q = data[s:s + n].split(b"\n")[0].replace(b"\0", b"") or b"q"
    ic = rnd.random() < 0.2
    if ic:
        q = bytes((c ^ 32) if (65 <= c <= 90 or 97 <= c <= 122) and rnd.random() < 0.5 else c for c in q)
    elif rnd.random() < 0.12:
        k = rnd.randrange(len(q) + 1)
        q = q[:k] + bytes([rnd.choice(b"qzxjQZXJ_~^|")]) + q[k:]
    return q, ic


blobs = {}
for p, _ in ins:
    with open(p, "rb") as f:
        b = f.read()
    blobs[p] = b if len(b) < (8 << 20) else b"".join(b[k:k + 65536] for k in range(0, len(b), len(b) // 64))
fold_blob = b"".join(files.values())
weighted = [p for p, w in ins for _ in range(w)]


def tasks(n):
    t = []
    for _ in range(n):
        r = rnd.random()
        if r < 0.75:
            p = rnd.choice(weighted)
            q, ic = piece(blobs[p])
            t.append(([arch[p]], q, ic))
        elif r < 0.87:
            q, ic = piece(fold_blob)
            t.append(([FA], q, ic))
        elif r < 0.95:
            q, ic = piece(blobs[jsrc])
            t.append(([JA], q, ic))
        else:
            # several archives at once (names before the lines)
            ps = rnd.sample(list(arch.values()), 3)
            q, ic = piece(blobs[rnd.choice(weighted)])
            t.append((ps, q, ic))
    t += [([a], b"qzxj_nomatch_7719", False) for a in all_archives]
    t += [([FA], b"", False), ([JA], b"qq" + blobs[jsrc][:12].split(b"\n")[0], False), (all_archives[:4], b"ERROR", True)]
    return t


def check(t):
    ps, q, ic = t
    th = rnd.choice([None, None, "1", "2", "3"])
    args = (["-t", th] if th else []) + (["-i"] if ic else []) + ["--", q] + ps
    a = subprocess.run([SEARCH, "grep"] + args, capture_output=True, env=env)
    b = subprocess.run([PACK, "grep"] + args, capture_output=True, env=env)
    same = (a.returncode, a.stdout) == (b.returncode, b.stdout)
    return t, th, same, b.stdout.count(b"\n"), b.returncode


def run(ts, label):
    fails, nl, hits = 0, 0, 0
    with ThreadPoolExecutor(8) as ex:
        for (ps, q, ic), th, same, n, rc in ex.map(check, ts):
            nl += n
            hits += rc == 0
            if not same:
                fails += 1
                print("DIFFERENT:", [os.path.basename(p) for p in ps], q[:80], "-i" if ic else "", "-t %s" % th if th else "")
    print("%s: %d queries (%d with matches, %d lines), %d archives, %d different" % (label, len(ts), hits, nl, len(all_archives), fails))
    return fails


fails = run(tasks(N), "grep, no index")
# the index: fewer index writes the same bytes as fewer-pack index
idx_diff = 0
for a in all_archives:
    subprocess.run([PACK, "index", a], check=True)
    want = open(a + ".idx", "rb").read()
    os.remove(a + ".idx")
    subprocess.run([SEARCH, "index", a], check=True)
    got = open(a + ".idx", "rb").read()
    if got != want:
        idx_diff += 1
        print("INDEX DIFFERENT:", os.path.basename(a))
print("index: %d archives, %d different" % (len(all_archives), idx_diff))
fails += run(tasks(NI), "grep with every archive indexed") + idx_diff
# usage errors and a missing file: the same exit codes
for args in (["grep"], ["grep", "x"], ["grep", "-z", "x", FA], ["grep", "x", WORK + "/missing.cmf"], ["grep", "x", WORK + "/fold"], ["index", WORK + "/missing.cmf"]):
    a = subprocess.run([SEARCH] + args, capture_output=True)
    b = subprocess.run([PACK] + args, capture_output=True)
    if (a.returncode, a.stdout) != (b.returncode, b.stdout):
        fails += 1
        print("DIFFERENT exit/stdout:", args, a.returncode, b.returncode)
if fails:
    sys.exit(1)
print("same_as_grep: ALL IDENTICAL")
