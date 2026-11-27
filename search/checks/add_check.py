"""add_check.py SEARCH PACK WORK [DATA]: the acceptance check of `add`.
Run with python3 -I.

DATA holds copies of the Loghub *_2k logs (default: $FEWER_SAMPLES/loghub). The check copies
what it uses into WORK and adds only those copies, packed with the installed gzip, bzip2, xz
and lz4 (xz and lz4 from Homebrew here, linked into a test PATH; a tool that is not installed
is replaced by a stub that unpacks the check's own made-up format, and the check says so).
Every process the check starts gets the safe environment of safe_env.py: a made-up HOME and
agents' folders, and stores inside WORK.

1. folders: a tree with log names (also in upper case), rotated names, packed files, files
   without a dot in a logs folder, a binary file and a zstd file, and what is passed over: other
   names, a hidden file, links to a file and to a folder (never followed), a FIFO, .git,
   node_modules, target, a hidden folder and the store's own folder. The files taken are exactly
   the expected ones, each saved, kept or recorded as not read as it should be, the saved ones
   holding the file's text (`fewer-pack cat`), and the command's summary line is exactly the one
   that names every count and reason.
2. packed files: .gz, .bz2, .xz and .lz4 copies of *_2k logs, each read through its tool
   (`reader`), its archive and catalog byte for byte the same as the plain copy's (same bytes,
   mode and time), so its content is the unpacked original's; a file named .log holding gzip
   bytes is read by gzip; events.csv.gz is typed csv by its inner name.
3. missing tools (a PATH without them): each packed file kept as it is (rNNNNNN.keep holds its
   bytes, private), with the note "<tool> is not installed"; a damaged .gz kept with gzip's
   own words. The MCP tools: `logs` lists each kept file with its note, `lines` says it has no
   lines, `count` and `search` pass it over without a word.
4. bundles and other archives: .zst (zstd's magic bytes and junk), .zip, .tar, .tgz, .tar.gz, a
   tar packed with gzip under a plain .gz name, 7z and rar: kept with their notes; zstd is
   never run (a recording stub named zstd on the PATH stays unused).
5. not text: NUL bytes, and under 90% text bytes: not read (a not-read run, no archive); the
   same again: "already looked at"; --force saves it, byte for byte.
6. the same file again: unchanged ("already saved", no record added), grown (only the new part,
   `continues`, offset and size exact, its archive holds exactly the new bytes; `count` over
   the runs, without and with a text, equals the grown file's: nothing counted twice), grown
   again (a chain; "its last part is"), rewritten and shorter (a new full run, "saved whole
   again"), a .gz whose content grew (only the new part, read by gzip), a kept file again
   ("already kept") and a kept file read once its tool is there (compared with the text saved
   before it was kept).
7. --hide-secrets: secrets kept without it; with it, the archive's text equals `fewer
   hide` of the file and `hidden` its count; `hide_secrets_in_add = on` does the same; a new
   part that starts inside a private key is hidden as the whole file would be.
8. OS exports with stub programs on a test PATH: --macos-log, --simulator, FILE.logarchive and
   --journal run their system commands (with --last, --predicate and --since);
   each run is kind export with its cmd, cwd, reader and log type, secrets hidden; a log
   archive added again unchanged is "already saved"; a missing program, a failing command and
   an empty window save nothing.
9. this Mac's log, for real: `add --macos-log --last Ns` with the shortest window that gives
   lines (1s, 2s, 5s, ...), into a store of its own: kind export, format macos-log, lines, bytes
   and secrets hidden counted. Only the kind, the counts and pass or fail are printed, never a
   line of it; that store is then truncated and deleted.
"""
import collections, importlib.util, io, json, os, re, shutil, stat, subprocess, sys, tarfile, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

if len(sys.argv) not in (4, 5):
    raise SystemExit(__doc__)
SEARCH, PACK, WORK = (os.path.abspath(a) for a in sys.argv[1:4])
DATA = os.path.abspath(sys.argv[4]) if len(sys.argv) == 5 else (os.path.join(os.environ["FEWER_SAMPLES"], "loghub") if os.environ.get("FEWER_SAMPLES") else None)
if not DATA or not os.path.isdir(DATA):
    raise SystemExit("give DATA, a folder of copies of the Loghub *_2k logs (or set FEWER_SAMPLES)")
MADE_UP = os.path.normpath(os.path.join(HERE, "..", "tests", "data", "made_up"))
# (a folder that is there is cleared only when a check made it: mcpc.fresh_work)
mcpc.fresh_work(WORK)
safe = safe_env.setup(WORK)
SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"

fails = []
stats = collections.Counter()


def ok(cond, what):
    stats["checks"] += 1
    if not cond:
        fails.append(what)
        print("FAILED:", what, flush=True)
    return cond


def sample(name):
    return open(os.path.join(DATA, name), "rb").read()


def made_up(name):
    return open(os.path.join(MADE_UP, name), "rb").read()


def write(path, data, mode=0o644, mtime=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    os.chmod(path, mode)
    if mtime is not None:
        os.utime(path, (mtime, mtime))
    return path


# ---------------------------------------------------------------- the tools

def real_tool(name):
    for d in SYSTEM_PATH.split(":") + safe.caller.get("PATH", "").split(":"):
        p = os.path.join(d, name)
        if d and os.path.isabs(d) and os.path.isfile(p) and os.access(p, os.X_OK):
            return p
    return None


TOOLS = WORK + "/tools"  # links to the real xz and lz4 (Homebrew), and the zstd recorder
EMPTY = WORK + "/empty-bin"  # a PATH with no program at all
os.makedirs(TOOLS)
os.makedirs(EMPTY)
PATH_ALL = TOOLS + ":" + SYSTEM_PATH
STUBBED = []
MAGIC = {"gzip": b"\x1f\x8b\x08", "bzip2": b"BZh91AY&SY", "xz": b"\xfd7zXZ\x00", "lz4": b"\x04\x22\x4d\x18"}
EXT = {"gzip": ".gz", "bzip2": ".bz2", "xz": ".xz", "lz4": ".lz4"}
REAL = {}
for t in ("gzip", "bzip2", "xz", "lz4"):
    p = real_tool(t)
    if p:
        REAL[t] = p
        if not p.startswith("/usr/bin/") and not p.startswith("/bin/"):
            os.symlink(p, os.path.join(TOOLS, t))
    else:
        # not installed here: a stub that "unpacks" the check's own format (the tool's magic
        # bytes, then the text), so the read path is still tested
        STUBBED.append(t)
        write(os.path.join(TOOLS, t), b'#!/bin/sh\n[ "$1" = "-dc" ] || exit 2\ntail -c +%d "$2"\n' % (len(MAGIC[t]) + 1), 0o755)
if STUBBED:
    print("not installed here, read through a stub on the test PATH: %s" % ", ".join(STUBBED), flush=True)
ZSTD_RAN = WORK + "/zstd-ran"
write(os.path.join(TOOLS, "zstd"), b"#!/bin/sh\ntouch '%s'\nexit 1\n" % ZSTD_RAN.encode(), 0o755)


def pack(tool, data, path, mtime=None):
    """`path`: `data` packed with the installed tool (or the stub's format)."""
    if tool in REAL:
        r = subprocess.run([REAL[tool], "-c"], input=data, capture_output=True)
        assert r.returncode == 0, (tool, r.stderr)
        b = r.stdout
    else:
        b = MAGIC[tool] + data
    return write(path, b, mtime=mtime)


# ---------------------------------------------------------------- the store

def env_for(home, proj, path=PATH_ALL, **more):
    return safe.env(FEWER_HOME=home, FEWER_PACK_BIN=PACK, FEWER_PROJECT=proj, PATH=path, **more)


def add(args, home, proj, path=PATH_ALL, **more):
    os.makedirs(proj, exist_ok=True)
    r = subprocess.run([SEARCH, "add"] + args, env=env_for(home, proj, path, **more), cwd=proj, stdin=subprocess.DEVNULL, capture_output=True)
    return r.returncode, r.stdout.decode(), r.stderr.decode()


def store(home):
    """(pid, project folder, runs merged by id) of a store's only project."""
    pids = os.listdir(os.path.join(home, "projects"))
    assert len(pids) == 1, pids
    pd = os.path.join(home, "projects", pids[0])
    runs = {}
    for line in open(os.path.join(pd, "runs.jsonl"), "rb"):
        o = json.loads(line)
        runs.setdefault(o["id"], {}).update(o)
    return pids[0], pd, runs


def records(home):
    pd = store(home)[1]
    return sum(1 for _ in open(os.path.join(pd, "runs.jsonl"), "rb"))


def cat(pd, rid):
    r = subprocess.run([PACK, "cat", os.path.join(pd, "r%06d.cmf" % rid)], capture_output=True)
    return r.stdout if r.returncode == 0 else None


def by_source(runs, path, kinds=None):
    """The runs of a source (its absolute path), oldest first."""
    return [r for _, r in sorted(runs.items()) if r.get("source") == os.path.abspath(path) and (kinds is None or r.get("kind") in kinds)]


def saved_line(out, path):
    """The `saved PATH as PID/rN: ...` line of a path: (rid, line) or (None, None)."""
    for line in out.splitlines():
        m = re.match(r"saved (.*) as (\S+)/r(\d+): ", line)
        if m and m.group(1) == path:
            return int(m.group(3)), line
    return None, None


def hide(data, *extras):
    r = subprocess.run([SEARCH, "hide"] + list(extras), input=data, capture_output=True)
    assert r.returncode == 0, r.stderr
    m = re.search(rb"hidden (\d+)", r.stderr)
    return r.stdout, int(m.group(1))


def lines_of(b):
    return b.count(b"\n") + (1 if b and not b.endswith(b"\n") else 0)


# ---------------------------------------------------------------- 1. folders

print("1. folders: files taken and passed over, the counts said", flush=True)
TREE = WORK + "/tree"
H1 = TREE + "/store"  # the store's own folder inside the walked folder: passed over
P1 = WORK + "/proj1"
plain = {
    "app.log": sample("HDFS_2k.log"),
    "app.log.1": sample("Zookeeper_2k.log"),
    "UPPER.LOG": b"2026-11-18 10:00:00 INFO upper case name\n",
    "service.out": sample("OpenSSH_2k.log"),
    "service.err": b"2026-11-18 10:00:01 ERROR made up\n",
    "notes.txt": b"made-up notes\nsecond line",
    "events.json": made_up("3f4e2a1b9c8d-json.log"),
    "events.jsonl": made_up("claude-session.jsonl"),
    "events.ndjson": made_up("macos-log.ndjson"),
    "table.csv": made_up("events.csv"),
    "table.tsv": made_up("events.tsv"),
    "MyApp-2026-11-18-101502.ips": made_up("MyApp-2026-11-18-101502.ips"),
    "MyApp_2026-11-18-101502_mac.crash": made_up("MyApp_2026-11-18-101502_mac.crash"),
    "logs/syslog": sample("Linux_2k.log"),
    "logs/messages.1": sample("Mac_2k.log"),
    "sub/deep/app.log": sample("BGL_2k.log"),
}
packed = {
    "app.log.2.gz": ("gzip", sample("Apache_2k.log")),
    "logs/kern.log.3.bz2": ("bzip2", sample("Spark_2k.log")),
    "sub/deep/trace.log.xz": ("xz", sample("Hadoop_2k.log")),
    "sub/deep/stream.log.lz4": ("lz4", sample("Proxifier_2k.log")),
}
for rel, data in plain.items():
    write(os.path.join(TREE, rel), data)
for rel, (tool, data) in packed.items():
    pack(tool, data, os.path.join(TREE, rel))
write(TREE + "/binary.log", b"head\x00\x01\x02 binary\n" * 10)
write(TREE + "/data.json.zst", b"\x28\xb5\x2f\xfd" + b"made-up zstd frame, never read")
# passed over
write(TREE + "/README.md", b"# readme\n")
write(TREE + "/image.png", b"\x89PNG\r\n\x1a\n" + bytes(range(256)))
write(TREE + "/run.sh", b"#!/bin/sh\necho hi\n", 0o755)
write(TREE + "/logs/x.y", b"in a logs folder, but with a dot\n")
write(TREE + "/.env.log", b"SECRET=never read\n")
os.symlink(TREE + "/app.log", TREE + "/link.log")
os.symlink(TREE + "/sub", TREE + "/linkdir")
os.mkfifo(TREE + "/pipe.log")
for rel in (".git/HEAD.log", "node_modules/pkg/x.log", "target/debug/y.log", ".cache/z.log"):
    write(os.path.join(TREE, rel), b"never taken\n")
code, out, err = add([TREE], H1, P1)
ok(code == 0 and not err, "the folder: exit %d, stderr %r" % (code, err[:300]))
taken = len(plain) + len(packed) + 2
want = ("folder %s: took %d files (%d saved, 1 kept as it is, 1 not read); passed over 8 files (4 not named as logs, 1 hidden, 2 links, not followed, 1 not a regular file) "
        "and 4 folders (.git, node_modules, target or hidden), the memory store's folder") % (TREE, taken, taken - 2)
ok(out.splitlines()[-1:] == [want], "the folder's summary:\n  got  %r\n  want %r" % (out.splitlines()[-1:], want))
pid1, pd1, runs1 = store(H1)
ok(len(runs1) == taken, "%d runs for %d files taken" % (len(runs1), taken))
for rel, data in plain.items():
    rs = by_source(runs1, os.path.join(TREE, rel))
    if ok(len(rs) == 1 and rs[0].get("kind") == "add" and rs[0].get("state") == "ready" and "reader" not in rs[0], "%s: one ready add run (%s)" % (rel, rs)):
        ok(cat(pd1, rs[0]["id"]) == data, "%s: its archive holds the file's bytes" % rel)
        stats["files compared"] += 1
for rel, (tool, data) in packed.items():
    rs = by_source(runs1, os.path.join(TREE, rel))
    if ok(len(rs) == 1 and rs[0].get("kind") == "add" and rs[0].get("reader") == tool, "%s: read by %s (%s)" % (rel, tool, rs)):
        ok(cat(pd1, rs[0]["id"]) == data, "%s: its archive holds the unpacked original" % rel)
        stats["files compared"] += 1
for rel, fmt in (("MyApp-2026-11-18-101502.ips", "crash"), ("MyApp_2026-11-18-101502_mac.crash", "crash"), ("table.csv", "csv"), ("table.tsv", "tsv"), ("events.ndjson", "macos-log"), ("events.jsonl", "claude-session")):
    rs = by_source(runs1, os.path.join(TREE, rel))
    ok(rs and rs[0].get("format") == fmt, "%s: format %s, not %s" % (rel, rs and rs[0].get("format"), fmt))
rs = by_source(runs1, TREE + "/binary.log")
ok(len(rs) == 1 and rs[0].get("kind") == "not-read" and "NUL bytes" in rs[0].get("note", "") and not os.path.exists(os.path.join(pd1, "r%06d.cmf" % rs[0]["id"])), "binary.log: not read, no archive (%s)" % rs)
rs = by_source(runs1, TREE + "/data.json.zst")
ok(len(rs) == 1 and rs[0].get("kind") == "kept" and rs[0].get("note") == "a zstd file: read in a later version", "data.json.zst: kept (%s)" % rs)
srcs = {r.get("source") for r in runs1.values()}
for rel in ("README.md", "image.png", "run.sh", "logs/x.y", ".env.log", "link.log", "pipe.log", ".git/HEAD.log", "node_modules/pkg/x.log", "target/debug/y.log", ".cache/z.log"):
    ok(os.path.join(TREE, rel) not in srcs, "%s was passed over" % rel)
ok(not any(s.startswith(H1) or "/linkdir/" in s for s in srcs), "nothing taken from the store's folder or through the link to a folder")
stats["folder runs"] += len(runs1)

# ---------------------------------------------------------------- 2. packed files

print("2. .gz, .bz2, .xz and .lz4 through the installed tools", flush=True)
H2, P2, D2 = WORK + "/home2", WORK + "/proj2", WORK + "/packed"
T0 = 1_790_000_000
pairs = [("HDFS_2k.log", "gzip"), ("Android_2k.log", "bzip2"), ("HealthApp_2k.log", "xz"), ("Windows_2k.log", "lz4"), ("Thunderbird_2k.log", "gzip"), ("OpenStack_2k.log", "xz")]
args = []
for name, tool in pairs:
    data = sample(name)
    write(os.path.join(D2, name), data, mtime=T0)
    pack(tool, data, os.path.join(D2, name + EXT[tool]), mtime=T0)
    args += [os.path.join(D2, name), os.path.join(D2, name + EXT[tool])]
pack("gzip", made_up("events.csv"), D2 + "/events.csv.gz")
pack("gzip", sample("Spark_2k.log"), D2 + "/looks-plain.log")
args += [D2 + "/events.csv.gz", D2 + "/looks-plain.log"]
code, out, err = add(args, H2, P2)
ok(code == 0 and not err, "packed files: exit %d, %r" % (code, err[:300]))
pid2, pd2, runs2 = store(H2)
for name, tool in pairs:
    a, b = by_source(runs2, os.path.join(D2, name)), by_source(runs2, os.path.join(D2, name + EXT[tool]))
    if not ok(len(a) == 1 and len(b) == 1 and b[0].get("reader") == tool and "reader" not in a[0], "%s%s: read by %s (%s)" % (name, EXT[tool], tool, b)):
        continue
    ra, rb = a[0]["id"], b[0]["id"]
    ok(cat(pd2, rb) == sample(name), "%s%s: its content is the unpacked original" % (name, EXT[tool]))
    same = all(open(os.path.join(pd2, "r%06d%s" % (ra, e)), "rb").read() == open(os.path.join(pd2, "r%06d%s" % (rb, e)), "rb").read() for e in (".cmf", ".cmf.idx", ".cat"))
    ok(same, "%s%s: archive, .idx and catalog the same bytes as the plain copy's" % (name, EXT[tool]))
    ok(b[0].get("format") == a[0].get("format") and b[0].get("lines") == a[0].get("lines") and b[0].get("size") == len(sample(name)), "%s%s: format, lines and size as the plain copy's" % (name, EXT[tool]))
    stats["packed files compared"] += 1
rs = by_source(runs2, D2 + "/events.csv.gz")
ok(rs and rs[0].get("format") == "csv" and rs[0].get("reader") == "gzip", "events.csv.gz: typed csv by its inner name (%s)" % rs)
rs = by_source(runs2, D2 + "/looks-plain.log")
ok(rs and rs[0].get("reader") == "gzip" and cat(pd2, rs[0]["id"]) == sample("Spark_2k.log"), "a .log holding gzip bytes is read by gzip (%s)" % rs)
ok(not os.listdir(H2 + "/tmp"), "no snapshot left in tmp: %s" % os.listdir(H2 + "/tmp"))

# ---------------------------------------------------------------- 3. missing tools

print("3. a PATH without the tools: kept as they are, with the note; the tools list them", flush=True)
H3, P3 = WORK + "/home3", WORK + "/proj3"
D3 = WORK + "/missing"
text_files = []
for name in ("Apache_2k.log", "Proxifier_2k.log"):
    text_files.append(write(os.path.join(D3, name), sample(name)))
missing = {}
for tool, name in (("gzip", "Zookeeper_2k.log"), ("bzip2", "Spark_2k.log"), ("xz", "Mac_2k.log"), ("lz4", "Linux_2k.log")):
    missing[tool] = pack(tool, sample(name), os.path.join(D3, name + EXT[tool]))
code, out, err = add(text_files + list(missing.values()), H3, P3, path=EMPTY)
ok(code == 0 and not err, "missing tools: exit %d, %r" % (code, err[:300]))
pid3, pd3, runs3 = store(H3)
for tool, path in missing.items():
    rs = by_source(runs3, path)
    note = "%s is not installed (not found on PATH)" % tool
    if ok(len(rs) == 1 and rs[0].get("kind") == "kept" and rs[0].get("note") == note and rs[0].get("state") == "ready", "%s: kept with its note (%s)" % (os.path.basename(path), rs)):
        keep = os.path.join(pd3, "r%06d.keep" % rs[0]["id"])
        ok(open(keep, "rb").read() == open(path, "rb").read() and stat.S_IMODE(os.stat(keep).st_mode) == 0o600, "%s: its .keep holds its bytes, private" % os.path.basename(path))
        ok(rs[0].get("size") == os.path.getsize(path) and ("kept %s as %s/r%d: " % (path, pid3, rs[0]["id"])) in out, "%s: size recorded, the command says kept" % os.path.basename(path))
        stats["kept for a missing tool"] += 1
# a damaged .gz: gzip's own words
broken = write(D3 + "/broken.log.gz", open(missing["gzip"], "rb").read()[:200])
code, out, err = add([broken], H3, P3)
rs = by_source(store(H3)[2], broken)
ok(code == 0 and rs and rs[0].get("kind") == "kept" and rs[0].get("note", "").startswith("gzip could not read it: ") and "unexpected end of file" in rs[0].get("note", ""), "a damaged .gz: kept with gzip's words (%s %r)" % (rs, err))
# the MCP tools over this store
s = mcpc.Server(SEARCH, env_for(H3, P3), P3, WORK + "/stderr3.txt")
s.initialize()
text, iserr = s.call("logs", {"limit": 50})
ok(not iserr and "(5 kept as they are, not read)" in text.splitlines()[0], "logs: the totals name the kept files: %s" % text.splitlines()[0])
for tool, path in missing.items():
    rid = by_source(runs3, path)[0]["id"]
    ok(re.search(r"^(?:%s/)?r%d kept .* %s: kept as it is, not read: %s is not installed \(not found on PATH\) \([\d,]+ bytes\)$" % (pid3, rid, re.escape(path), tool), text, re.M) is not None, "logs lists %s with its note" % os.path.basename(path))
text, iserr = s.call("lines", {"ref": "%s/r%d:1" % (pid3, by_source(runs3, missing["bzip2"])[0]["id"])})
ok(iserr and "kept as it is, not read: bzip2 is not installed" in text and "it has no lines to show" in text, "lines on a kept file: %s" % text[:200])
text, iserr = s.call("count", {})
want_lines = sum(lines_of(open(p, "rb").read()) for p in text_files)
ok(not iserr and text.startswith("%s lines in 2 runs" % format(want_lines, ",")) and "not counted" not in text, "count without a text passes kept files over: %s" % text.splitlines()[0])
text, iserr = s.call("count", {"by": "key"})
ok(not iserr and "kept as" not in text and "not counted" not in text and "cannot read" not in text, "count by key passes kept files over: %s" % text[:200])
text, iserr = s.call("search", {"text": "INFO"})
ok(not iserr and "not searched" not in text, "search passes kept files over: %s" % text[:200])
ok(s.close() == 0, "the server ends with 0")

# ---------------------------------------------------------------- 4. bundles and other archives

print("4. .zst, .zip and tar bundles: kept with their notes; zstd never runs", flush=True)
H4, P4, D4 = WORK + "/home4", WORK + "/proj4", WORK + "/bundles"
log = sample("HDFS_2k.log")
bio = io.BytesIO()
with zipfile.ZipFile(bio, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("app.log", log)
write(D4 + "/logs.zip", bio.getvalue())


def tar_bytes(mode):
    bio = io.BytesIO()
    with tarfile.open(fileobj=bio, mode=mode) as t:
        info = tarfile.TarInfo("app.log")
        info.size = len(log)
        t.addfile(info, io.BytesIO(log))
    return bio.getvalue()


write(D4 + "/logs.tar", tar_bytes("w"))
write(D4 + "/logs.tgz", tar_bytes("w:gz"))
write(D4 + "/logs.tar.gz", tar_bytes("w:gz"))
pack("gzip", tar_bytes("w"), D4 + "/bundle.gz")
write(D4 + "/app.log.zst", b"\x28\xb5\x2f\xfd\x04\x58" + b"made-up zstd frame")
write(D4 + "/old.7z", b"7z\xbc\xaf\x27\x1c" + b"\x00" * 30)
write(D4 + "/old.rar", b"Rar!\x1a\x07\x01\x00" + b"\x00" * 30)
notes = {
    "logs.zip": "a zip bundle: read in a later version",
    "logs.tar": "a tar bundle: read in a later version",
    "logs.tgz": "a tar bundle: read in a later version",
    "logs.tar.gz": "a tar bundle: read in a later version",
    "bundle.gz": "a tar bundle packed with gzip: read in a later version",
    "app.log.zst": "a zstd file: read in a later version",
    "old.7z": "a 7z archive, which fewer does not read",
    "old.rar": "a rar archive, which fewer does not read",
}
code, out, err = add([os.path.join(D4, n) for n in notes], H4, P4)
ok(code == 0 and not err, "bundles: exit %d, %r" % (code, err[:300]))
pid4, pd4, runs4 = store(H4)
for n, note in notes.items():
    rs = by_source(runs4, os.path.join(D4, n))
    ok(len(rs) == 1 and rs[0].get("kind") == "kept" and rs[0].get("note") == note and open(os.path.join(pd4, "r%06d.keep" % rs[0]["id"]), "rb").read() == open(os.path.join(D4, n), "rb").read(), "%s: kept, note %r (%s)" % (n, note, rs))
    stats["bundles kept"] += 1
ok(not os.path.exists(ZSTD_RAN), "zstd was never run")
code, out, err = add([D4 + "/app.log.zst", D4 + "/logs.zip"], H4, P4)
ok(code == 0 and out.count(": already kept as %s/r" % pid4) == 2 and len(store(H4)[2]) == len(notes), "kept files again: already kept, no new run: %r" % out)

# ---------------------------------------------------------------- 5. not text

print("5. a binary file is not read; --force saves it", flush=True)
H5, P5, D5 = WORK + "/home5", WORK + "/proj5", WORK + "/binary"
nul = write(D5 + "/nul.log", b"".join(b"line %d\x00 with a NUL\n" % i for i in range(100)))
low = write(D5 + "/control.log", b"".join(b"line %d \x01\x02\x03\x04\x05\x06\n" % i for i in range(300)))
code, out, err = add([nul, low], H5, P5)
pid5, pd5, runs5 = store(H5)
a, b = by_source(runs5, nul), by_source(runs5, low)
ok(code == 0 and len(a) == 1 and a[0].get("kind") == "not-read" and a[0].get("note") == "not text: NUL bytes in its first 64 KiB (the first at byte 6)", "nul.log: not read (%s)" % a)
ok(len(b) == 1 and b[0].get("kind") == "not-read" and re.fullmatch(r"not text: \d+% text bytes in its first 64 KiB, under 90%", b[0].get("note", "")), "control.log: not read (%s)" % b)
ok(not [f for f in os.listdir(pd5) if f.endswith(".cmf") or f.endswith(".keep")], "nothing of them saved")
ok("--force saves it all the same" in out, "the command says how to save it anyway")
code, out, err = add([nul], H5, P5)
ok(code == 0 and out.startswith("%s: already looked at as %s/r%d, not read: " % (nul, pid5, a[0]["id"])) and len(store(H5)[2]) == 2, "the same binary file again: already looked at, no new run: %r" % out)
code, out, err = add(["--force", nul, low], H5, P5)
runs5 = store(H5)[2]
for p in (nul, low):
    rs = by_source(runs5, p, ("add",))
    ok(len(rs) == 1 and cat(pd5, rs[0]["id"]) == open(p, "rb").read(), "--force: %s saved byte for byte (%s)" % (os.path.basename(p), rs))

# ---------------------------------------------------------------- 6. the same file again

print("6. the same file again: unchanged, grown, rewritten", flush=True)
H6, P6, D6 = WORK + "/home6", WORK + "/proj6", WORK + "/again"
# (each piece ends with a line end: a file that grows inside a line splits that line between
# two runs, a known limit)
X, Y, Z = (b if b.endswith(b"\n") else b + b"\n" for b in (sample("HDFS_2k.log"), sample("OpenSSH_2k.log"), sample("Zookeeper_2k.log")))
g = write(D6 + "/grows.log", X)
code, out, err = add([g], H6, P6)
ra, _ = saved_line(out, g)
ok(code == 0 and ra, "first save: %r %r" % (out, err))
n0 = records(H6)
code, out, err = add([g], H6, P6)
pid6, pd6, runs6 = store(H6)
ok(code == 0 and out.strip() == "%s: already saved as %s/r%d, unchanged" % (g, pid6, ra) and records(H6) == n0, "unchanged: %r" % out)
with open(g, "ab") as f:
    f.write(Y)
code, out, err = add([g], H6, P6)
rb, line = saved_line(out, g)
runs6 = store(H6)[2]
ok(code == 0 and rb and line.endswith("; its new part, after r%d" % ra), "grown: %r" % out)
r = runs6.get(rb, {})
ok(r.get("continues") == ra and r.get("offset") == len(X) and r.get("size") == len(Y) and r.get("bytes") == len(Y) and r.get("lines") == lines_of(Y), "grown: continues, offset, size, bytes, lines (%s)" % {k: r.get(k) for k in ("continues", "offset", "size", "bytes", "lines")})
ok(cat(pd6, rb) == Y, "grown: its archive holds exactly the new bytes")
with open(g, "ab") as f:
    f.write(Z)
code, out, err = add([g], H6, P6)
rc, line = saved_line(out, g)
r = store(H6)[2].get(rc, {})
ok(code == 0 and rc and r.get("continues") == rb and r.get("offset") == len(X) + len(Y) and cat(pd6, rc) == Z, "grown again: a chain (%s)" % {k: r.get(k) for k in ("continues", "offset", "size")})
code, out, err = add([g], H6, P6)
ok(out.strip() == "%s: already saved, unchanged (its last part is %s/r%d)" % (g, pid6, rc), "the chain unchanged: %r" % out)
# counts over the chain: the grown file's, nothing twice
s = mcpc.Server(SEARCH, env_for(H6, P6), P6, WORK + "/stderr6.txt")
s.initialize()
whole = X + Y + Z
refs = ["%s/r%d" % (pid6, i) for i in (ra, rb, rc)]
text, iserr = s.call("count", {"runs": refs})
ok(not iserr and text.startswith("%s lines in 3 runs" % format(lines_of(whole), ",")), "count over the chain: %s, the file has %d lines" % (text.splitlines()[0], lines_of(whole)))
for t in ("INFO", "sshd", "block", "Zookeeper"):
    want = sum(1 for l in whole.split(b"\n") if t.encode() in l)
    text, iserr = s.call("count", {"runs": refs, "text": t})
    m = re.match(r"([\d,]+) lines? hold", text)
    ok(not iserr and m and int(m.group(1).replace(",", "")) == want, "count %r over the chain: %s, the grown file holds it in %d lines" % (t, text.splitlines()[0], want))
    stats["chain counts"] += 1
ok(s.close() == 0, "the server ends with 0")
# rewritten (the same length, another start), then shorter
w = bytearray(open(g, "rb").read())
w[0:1] = b"X" if w[0:1] != b"X" else b"Y"
write(g, bytes(w))
code, out, err = add([g], H6, P6)
rd, line = saved_line(out, g)
r = store(H6)[2].get(rd, {})
ok(rd and line.endswith("; saved whole again: it no longer starts with what r%d read" % rc) and "continues" not in r and r.get("offset") == 0 and cat(pd6, rd) == bytes(w), "rewritten: a new full run (%r)" % line)
write(g, Y[:5000])
code, out, err = add([g], H6, P6)
re_, line = saved_line(out, g)
r = store(H6)[2].get(re_, {})
ok(re_ and "continues" not in r and r.get("size") == 5000 and cat(pd6, re_) == Y[:5000], "shorter: a new full run (%r)" % line)
# a .gz whose content grew: only the new part, read by gzip
gz = pack("gzip", X, D6 + "/rotated.log.gz")
code, out, err = add([gz], H6, P6)
r1, _ = saved_line(out, gz)
pack("gzip", X + Y, gz)
code, out, err = add([gz], H6, P6)
r2, line = saved_line(out, gz)
r = store(H6)[2].get(r2, {})
ok(r1 and r2 and r.get("continues") == r1 and r.get("reader") == "gzip" and r.get("offset") == len(X) and cat(pd6, r2) == Y, "a .gz whose content grew: only the new part (%r)" % line)
pack("gzip", X + Y, gz)  # packed again: other bytes, the same content
code, out, err = add([gz], H6, P6)
ok(out.strip() == "%s: already saved, unchanged (its last part is %s/r%d)" % (gz, pid6, r2), "a .gz packed again with the same content: unchanged: %r" % out)
# kept while its tool is missing, read once it is there: compared with the text saved before
xz = pack("xz", Z, D6 + "/later.log.xz")
code, out, err = add([xz], H6, P6, path=EMPTY)
kept = by_source(store(H6)[2], xz, ("kept",))
ok(len(kept) == 1, "xz missing: kept (%r)" % out)
code, out, err = add([xz], H6, P6)
r3, line = saved_line(out, xz)
ok(r3 and by_source(store(H6)[2], xz, ("add",))[0]["id"] == r3 and cat(pd6, r3) == Z, "xz there now: saved, read by xz (%r)" % out)
code, out, err = add([xz], H6, P6, path=EMPTY)
ok(out.strip() == "%s: already kept as %s/r%d (xz is not installed (not found on PATH))" % (xz, pid6, kept[0]["id"]), "kept again: already kept (%r)" % out)
code, out, err = add([xz], H6, P6)
ok(out.strip() == "%s: already saved as %s/r%d, unchanged" % (xz, pid6, r3), "read again: already saved, though it was kept since (%r)" % out)

# ---------------------------------------------------------------- 7. --hide-secrets

print("7. --hide-secrets", flush=True)
H7, P7, D7 = WORK + "/home7", WORK + "/proj7", WORK + "/secrets"
KEY = b"-----BEGIN RSA PRIVATE KEY-----\n" + b"".join(b"MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun%04d\n" % i for i in range(6)) + b"-----END RSA PRIVATE KEY-----\n"
SECRETS = (
    b"2026-11-18 10:00:00 INFO start\n"
    b"2026-11-18 10:00:01 INFO aws key AKIAIOSFODNN7EXAMPLE used\n"
    b"2026-11-18 10:00:02 WARN login password=hunter22 failed\n"
    b"2026-11-18 10:00:03 INFO Authorization: Bearer abcdef123456ghijklmnop\n"
    b"2026-11-18 10:00:04 INFO token ghp_0123456789abcdefghijklmnopqrstuvwxyzAB\n" + KEY + b"2026-11-18 10:00:05 INFO done\n"
)
want_text, want_n = hide(SECRETS)
ok(want_n >= 5 and b"hunter22" not in want_text and b"AKIAIOSFODNN7EXAMPLE" not in want_text and b"MIIE" not in want_text, "the reference hides %d secrets" % want_n)
plain7 = write(D7 + "/plain.log", SECRETS)
hidden7 = write(D7 + "/hidden.log", SECRETS)
code, out, err = add([plain7], H7, P7)
code2, out2, err2 = add(["--hide-secrets", hidden7], H7, P7)
pid7, pd7, runs7 = store(H7)
a, b = by_source(runs7, plain7)[0], by_source(runs7, hidden7)[0]
ok(cat(pd7, a["id"]) == SECRETS and "hidden" not in a, "without --hide-secrets every byte is kept")
ok(cat(pd7, b["id"]) == want_text and b.get("hidden") == want_n and b.get("size") == len(SECRETS) and b.get("bytes") == len(want_text), "--hide-secrets: hidden as `hide` hides them, %s counted (%s)" % (b.get("hidden"), {k: b.get(k) for k in ("size", "bytes")}))
ok(("; %d secrets hidden" % want_n) in out2, "the command says how many were hidden: %r" % out2)
with open(H7 + "/config", "a") as f:
    f.write("hide_secrets_in_add = on\n")
cfg7 = write(D7 + "/config-on.log", SECRETS)
code, out, err = add([cfg7], H7, P7)
r = by_source(store(H7)[2], cfg7)[0]
ok(cat(pd7, r["id"]) == want_text and r.get("hidden") == want_n, "hide_secrets_in_add = on hides them too")
# a new part that starts inside a private key: hidden as the whole file would be
# (the first part ends after the key's second line)
cut = KEY.index(b"\n", KEY.index(b"\n", KEY.index(b"MIIE")) + 1) + 1
pre = b"2026-11-18 10:01:00 INFO before the key\n" + KEY[:cut]
rest = KEY[cut:] + b"2026-11-18 10:01:01 INFO after the key password=swordfish9\n"
k = write(D7 + "/key.log", pre)
add(["--hide-secrets", k], H7, P7)
with open(k, "ab") as f:
    f.write(rest)
code, out, err = add(["--hide-secrets", k], H7, P7)
rk, line = saved_line(out, k)
whole_text, whole_n = hide(pre + rest)
pre_text, pre_n = hide(pre)
r = store(H7)[2].get(rk, {})
got = cat(pd7, rk) or b""
ok(rk and r.get("continues") and got == whole_text[len(pre_text):] and b"MIIE" not in got and b"swordfish9" not in got, "a new part inside a private key: hidden as the whole file would be (%r)" % got[:200])
ok(r.get("hidden") == whole_n - pre_n, "its own secrets counted: %s, want %d" % (r.get("hidden"), whole_n - pre_n))

# ---------------------------------------------------------------- 8. OS exports, with stubs

print("8. OS exports through stub programs on a test PATH", flush=True)
H8, P8, STUBS = WORK + "/home8", WORK + "/proj8", WORK + "/stubs"
STUB = b"""#!/bin/sh
# a stand-in for the system's export command: records its arguments, prints made-up lines
d=$(dirname "$0"); n=$(basename "$0")
for a in "$@"; do printf '%s\\n' "$a"; done > "$d/$n.args"
if [ -f "$d/$n.fail" ]; then cat "$d/$n.fail" >&2; exit 1; fi
cat "$d/$n.out"
"""
SECRET_LINE = b'{"eventMessage":"login password=hunter22 failed","messageType":"Error","timestamp":"2026-11-18 10:15:09.000000-0800","processID":7}\n'
for prog, out_name in (("log", "macos-log.ndjson"), ("xcrun", "macos-log.ndjson"), ("journalctl", "journal.json")):
    write(os.path.join(STUBS, prog), STUB, 0o755)
    write(os.path.join(STUBS, prog + ".out"), made_up(out_name) + (SECRET_LINE if prog != "journalctl" else b'{"MESSAGE":"token=abcdef0123456789 sent","PRIORITY":"4","__REALTIME_TIMESTAMP":"1795003309000000"}\n'))
PATH8 = STUBS + ":" + SYSTEM_PATH
ARCHIVE = WORK + "/exports/system_logs.logarchive"
os.makedirs(ARCHIVE)
cases = [
    (["--macos-log"], "log", ["show", "--info", "--debug", "--style", "ndjson", "--last", "1h"], "this Mac's log (the last 1h)", "macos-log", None),
    (["--simulator", "--last", "10m", "--predicate", 'subsystem == "com.example"'], "xcrun", ["simctl", "spawn", "booted", "log", "show", "--style", "ndjson", "--last", "10m", "--predicate", 'subsystem == "com.example"'], "the booted simulator's log (the last 10m)", "macos-log", None),
    ([ARCHIVE], "log", ["show", "--archive", ARCHIVE, "--style", "ndjson"], "the log archive " + ARCHIVE, "macos-log", ARCHIVE),
    (["--journal", "--since", "2026-11-18 10:00"], "journalctl", ["-o", "json", "--all", "--since=2026-11-18 10:00"], "the systemd journal (since 2026-11-18 10:00)", "journal", None),
]
for args, prog, want_args, label, fmt, source in cases:
    code, out, err = add(args, H8, P8, path=PATH8)
    got_args = open(os.path.join(STUBS, prog + ".args")).read().split("\n")[:-1]
    ok(code == 0 and got_args == want_args, "%s: ran %s %s (exit %d, %r)" % (args[0], prog, got_args, code, err[:200]))
    rid, line = saved_line(out, label)
    pid8, pd8, runs8 = store(H8)
    r = runs8.get(rid, {})
    want_text, want_n = hide(open(os.path.join(STUBS, prog + ".out"), "rb").read())
    ok(rid and r.get("kind") == "export" and r.get("cmd") == [prog] + want_args and r.get("cwd") == P8 and r.get("reader") == prog and r.get("format") == fmt, "%s: an export run with its command (%s)" % (args[0], {k: r.get(k) for k in ("kind", "cmd", "cwd", "reader", "format")}))
    ok(cat(pd8, rid) == want_text and r.get("hidden") == want_n == 1, "%s: its output saved, secrets hidden (%s)" % (args[0], r.get("hidden")))
    ok(r.get("source") == source and (source is None) == ("offset" not in r), "%s: source %s" % (args[0], r.get("source")))
    stats["exports"] += 1
n8 = records(H8)
code, out, err = add([ARCHIVE], H8, P8, path=PATH8)
ok(code == 0 and re.fullmatch(r"the log archive %s: already saved as %s/r\d+, unchanged" % (re.escape(ARCHIVE), pid8), out.strip()) and records(H8) == n8, "the log archive again: already saved (%r)" % out)
code, out, err = add(["--macos-log"], H8, P8, path=EMPTY)
ok(code == 1 and "log is not installed (not found on PATH) (macOS's log); nothing saved" in err and records(H8) == n8, "log missing: says so, saves nothing (%d %r)" % (code, err))
write(os.path.join(STUBS, "xcrun.fail"), b"No devices are booted.\n")
code, out, err = add(["--simulator"], H8, P8, path=PATH8)
ok(code == 1 and "failed: No devices are booted.; nothing saved" in err and records(H8) == n8, "a failing export saves nothing (%d %r)" % (code, err))
write(os.path.join(STUBS, "log.out"), b"")
code, out, err = add(["--macos-log", "--last", "1s"], H8, P8, path=PATH8)
ok(code == 0 and out.strip() == "this Mac's log (the last 1s): no lines in that window; nothing saved" and records(H8) == n8, "an empty window saves nothing (%r)" % out)
s = mcpc.Server(SEARCH, env_for(H8, P8, path=PATH8), P8, WORK + "/stderr8.txt")
s.initialize()
text, iserr = s.call("logs", {})
ok(not iserr and " export " in text and "xcrun simctl spawn booted log show --style ndjson --last 10m" in text and "journalctl -o json --all" in text, "logs lists the exports with their commands")
ok(s.close() == 0, "the server ends with 0")

# ---------------------------------------------------------------- 9. this Mac's log, for real

print("9. this Mac's log for real (log show): only the kind, counts and pass/fail are printed", flush=True)
H9, P9 = WORK + "/oslog/home", WORK + "/oslog/proj"
if not os.path.exists("/usr/bin/log"):
    print("   no /usr/bin/log here (not macOS): skipped")
else:
    window = rid = None
    for w in ("1s", "2s", "5s", "10s", "30s", "1m", "5m"):
        code, out, err = add(["--macos-log", "--last", w], H9, P9, path=SYSTEM_PATH)
        rid, _ = saved_line(out, "this Mac's log (the last %s)" % w)
        if rid:
            window = w
            break
        if code != 0 or "no lines in that window" not in out:
            ok(False, "add --macos-log --last %s: exit %d, %d bytes of output, %d of errors" % (w, code, len(out), len(err)))
            break
    if ok(rid is not None, "this Mac's log gave lines in some window up to 5m"):
        pid9, pd9, runs9 = store(H9)
        r = runs9[rid]
        body = cat(pd9, rid) or b""
        objects = sum(1 for l in body.split(b"\n") if l.startswith(b"{") and l.endswith(b"}"))
        good = (r.get("kind") == "export" and r.get("state") == "ready" and r.get("cmd") == ["log", "show", "--info", "--debug", "--style", "ndjson", "--last", window]
                and r.get("reader") == "log" and r.get("format") == "macos-log" and r.get("lines", 0) > 0 and r.get("lines") == lines_of(body)
                and isinstance(r.get("hidden"), int) and os.path.exists(os.path.join(pd9, "r%06d.cat" % rid)))
        ok(good, "this Mac's log saved as an export")
        print("   window %s: kind %s, format %s, %d lines (%d JSON objects), %d bytes, %d secrets hidden: %s" % (window, r.get("kind"), r.get("format"), r.get("lines", 0), objects, r.get("bytes", 0), r.get("hidden", -1), "pass" if good else "FAILED"), flush=True)
    # the real log's copy: truncated, then deleted
    for root, dirs, files in os.walk(WORK + "/oslog"):
        for f in files:
            p = os.path.join(root, f)
            if os.path.isfile(p) and not os.path.islink(p):
                os.truncate(p, 0)
    shutil.rmtree(WORK + "/oslog")
    ok(not os.path.exists(WORK + "/oslog"), "the store of the real log is truncated and deleted")

print("add_check: %d checks, %d failed; %s" % (stats["checks"], len(fails), ", ".join("%s %d" % kv for kv in sorted(stats.items()) if kv[0] != "checks")), flush=True)
if STUBBED:
    print("read through stubs (not installed here): %s" % ", ".join(STUBBED))
print("ALL PASSED" if not fails else "FAILED: %d" % len(fails))
sys.exit(1 if fails else 0)
