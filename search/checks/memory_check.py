"""memory_check.py SEARCH PACK WORK DATA: `fewer memory`
(python3 -I).

SEARCH is the fewer program, PACK the fewer-pack program that packs, WORK a scratch folder
(made afresh), DATA a folder of copies of the Loghub *_2k logs. Two projects get runs of every
kind (added logs, a command run twice through `run` and pack-pending, a made-up Claude Code
session saved by `sessions sync`). Then:
1. see: every number `memory` prints equals the files (runs by kind, text bytes from runs.jsonl,
   bytes on disk from the folders, the ratio, the oldest and newest run, the totals); --project
   and --runs;
2. forget in every form (a reference, a line reference, --session, --before, --sessions,
   --project, --all), without a terminal and without --yes (nothing goes), on a terminal
   answering n (nothing goes) and y, and with --yes: the runs' files are gone, runs.jsonl has
   no line of them (a session piece keeps only a "forgotten" record of where it was read
   from, and the sweep does not save it again, but does save what is new after it), keys.cat
   and moments.jsonl equal a fresh rebuild from the runs left (`rebuild-keys` on a copy), ids
   are not used again, a run still being written is refused, and unknown runs are said;
3. pause and resume, for the project and for a folder: the command still runs, with its exit
   status and output, and nothing is saved;
4. limit: none by default (nothing goes); --size and --months forget the oldest runs after the
   next save, never the newest, and say so in forgotten.log and in `memory`; --none;
5. sessions, secrets and dna, and bad arguments.
"""
import errno, hashlib, importlib.util, json, os, pty, re, select, shutil, signal, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

if len(sys.argv) != 5:
    raise SystemExit(__doc__)
SEARCH, PACK, WORK, DATA = (os.path.abspath(a) for a in sys.argv[1:5])
mcpc.fresh_work(WORK)
safe = safe_env.setup(WORK)
HOME, P1, P2, CLAUDE = WORK + "/home", WORK + "/proj1", WORK + "/proj2", WORK + "/claude"
for d in (P1, P2, P1 + "/sub"):
    os.makedirs(d)
ENV = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_CLAUDE_DIR=CLAUDE, GIT_CEILING_DIRECTORIES=WORK)
fails = []
stats = {"checks": 0}


def ok(cond, what):
    stats["checks"] += 1
    if not cond:
        fails.append(what)
        print("FAILED:", what, flush=True)
    return cond


def env_for(proj, **more):
    e = dict(ENV, FEWER_PROJECT=proj)
    e.update(more)
    return e


def search(args, proj=P1, cwd=None, **more):
    """fewer ARGS, stdin /dev/null (not a terminal): (exit status, stdout, stderr)."""
    r = subprocess.run([SEARCH] + args, env=env_for(proj, **more), cwd=cwd or proj, stdin=subprocess.DEVNULL, capture_output=True)
    return r.returncode, r.stdout.decode("utf-8", "surrogateescape"), r.stderr.decode("utf-8", "surrogateescape")


def code_of(st):
    return os.waitstatus_to_exitcode(st)


def tty(args, answer, proj=P1, timeout=60):
    """fewer ARGS on a terminal (Python's pty module), typing ANSWER at "[y/N] ":
    (exit status, output, whether it asked)."""
    env = safe.final(env_for(proj), [SEARCH] + args, cwd=proj)
    pid, fd = pty.fork()
    if pid == 0:
        try:
            os.chdir(proj)
            os.execve(SEARCH, [SEARCH] + args, env)
        finally:
            os._exit(127)
    out, asked, end = bytearray(), False, time.time() + timeout
    while True:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                b = os.read(fd, 65536)
            except OSError as e:
                if e.errno == errno.EIO:
                    break
                raise
            if not b:
                break
            out += b
        if not asked and b"[y/N] " in out:
            os.write(fd, answer.encode() + b"\n")
            asked = True
        if time.time() > end:
            os.kill(pid, signal.SIGKILL)
            ok(False, "timed out: %r" % (args,))
            break
    _, st = os.waitpid(pid, 0)
    os.close(fd)
    return code_of(st), bytes(out).replace(b"\r\n", b"\n").decode("utf-8", "surrogateescape"), asked


def tty_menu(args, pick, answer, proj=P1, timeout=60):
    """fewer ARGS on a terminal, typing PICK at the menu's question and ANSWER (when not None) at
    "[y/N] ": (exit status, output, whether it asked [y/N])."""
    env = safe.final(env_for(proj), [SEARCH] + args, cwd=proj)
    pid, fd = pty.fork()
    if pid == 0:
        try:
            os.chdir(proj)
            os.execve(SEARCH, [SEARCH] + args, env)
        finally:
            os._exit(127)
    out, picked, asked, end = bytearray(), False, False, time.time() + timeout
    while True:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                b = os.read(fd, 65536)
            except OSError as e:
                if e.errno == errno.EIO:
                    break
                raise
            if not b:
                break
            out += b
        if not picked and b"press Enter to leave: " in out:
            os.write(fd, pick.encode() + b"\n")
            picked = True
        if picked and answer is not None and not asked and b"[y/N] " in out:
            os.write(fd, answer.encode() + b"\n")
            asked = True
        if time.time() > end:
            os.kill(pid, signal.SIGKILL)
            ok(False, "timed out: %r" % (args,))
            break
    _, st = os.waitpid(pid, 0)
    os.close(fd)
    return code_of(st), bytes(out).replace(b"\r\n", b"\n").decode("utf-8", "surrogateescape"), asked


def state(root):
    """Every file and folder under root: what it is, its size, SHA-256 and modification time."""
    out = {}
    for d, dirs, files in os.walk(root):
        for n in dirs + files:
            p = os.path.join(d, n)
            st = os.lstat(p)
            rel = os.path.relpath(p, root)
            if os.path.isdir(p):
                out[rel] = ("dir",)
            else:
                out[rel] = ("file", st.st_size, hashlib.sha256(open(p, "rb").read()).hexdigest(), st.st_mtime_ns)
    return out


def pid_of(proj):
    for pid in os.listdir(HOME + "/projects"):
        for line in open(os.path.join(HOME, "projects", pid, "project")):
            if line.startswith("root = ") and line[7:].rstrip("\n") == proj:
                return pid
    raise SystemExit("no project for %s" % proj)


def runs_of(pid):
    """runs.jsonl merged by id, as store::read_runs does."""
    runs = {}
    path = os.path.join(HOME, "projects", pid, "runs.jsonl")
    if not os.path.exists(path):
        return runs
    data = open(path, "rb").read()
    data = data[: data.rfind(b"\n") + 1]
    for line in data.split(b"\n"):
        if not line:
            continue
        try:
            x = json.loads(line)
        except ValueError:
            continue
        if isinstance(x, dict) and isinstance(x.get("id"), int) and x["id"] > 0:
            runs.setdefault(x["id"], {}).update(x)
    return runs


def run_files(pid, rid):
    prefix = "r%06d." % rid
    out = []
    for d in (os.path.join(HOME, "projects", pid), os.path.join(HOME, "raw", pid)):
        if os.path.isdir(d):
            out += [os.path.join(d, n) for n in os.listdir(d) if n.startswith(prefix)]
    return out


def folder_bytes(d):
    n = 0
    for root, dirs, files in os.walk(d):
        for f in files:
            p = os.path.join(root, f)
            if os.path.isfile(p) and not os.path.islink(p):
                n += os.lstat(p).st_size
    return n


def disk(pid):
    return folder_bytes(os.path.join(HOME, "projects", pid)) + folder_bytes(os.path.join(HOME, "raw", pid))


def g(n):
    return "{:,}".format(n)


def local(ms):
    return time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(ms // 1000))


KINDS = [("run", "command run", "command runs"), ("add", "file", "files"), ("session", "session piece", "session pieces"), ("export", "export", "exports"), ("kept", "file kept as it is", "files kept as they are"), ("not-read", "file not read", "files not read")]


def expected_block(pid):
    """The lines `memory` should print for a project (memory.rs `see`)."""
    runs = [x for x in runs_of(pid).values() if x.get("state") != "forgotten"]
    root = [l[7:].rstrip("\n") for l in open(os.path.join(HOME, "projects", pid, "project")) if l.startswith("root = ")][0]
    paused = os.path.exists(os.path.join(HOME, "projects", pid, "paused"))
    lines = ["%s: %s%s" % (pid, root, " (saving paused)" if paused else "")]
    by = {}
    for x in runs:
        by[x.get("kind")] = by.get(x.get("kind"), 0) + 1
    parts = ["%s %s" % (g(by[k]), one if by[k] == 1 else many) for k, one, many in KINDS if k in by]
    parts += ["%s %s" % (g(n), k) for k, n in sorted(by.items()) if k not in [x[0] for x in KINDS]]
    t = ", ".join(parts)
    live = sum(1 for x in runs if x.get("state") in ("running", "packing"))
    failed = sum(1 for x in runs if x.get("state") == "failed")
    more = ([("%s in progress" % g(live))] if live else []) + ([("%s failed" % g(failed))] if failed else [])
    if more:
        t += "; " + ", ".join(more)
    lines.append("  runs: %s (%s)" % (g(len(runs)), t) if runs else "  runs: 0")
    text = sum(max(int(x.get("bytes", 0)), 0) for x in runs if x.get("state") == "ready" and "archive" in x)
    d = disk(pid)
    lines.append("  text: %s bytes; on disk: %s bytes%s" % (g(text), g(d), (" (%.1fx)" % (text / d)) if text and d else ""))
    starts = [x["start"] for x in runs if isinstance(x.get("start"), int)]
    if starts:
        lines.append("  from %s to %s" % (local(min(starts)), local(max(starts))))
    return lines, len(runs), text, d


def check_see(what, args=(), pids=None):
    code, out, err = search(["memory"] + list(args))
    ok(code == 0, "%s: memory exit %d %s" % (what, code, err))
    pids = pids if pids is not None else sorted(os.listdir(HOME + "/projects"))
    n = t = d = 0
    for pid in pids:
        want, rn, rt, rd = expected_block(pid)
        n, t, d = n + rn, t + rt, d + rd
        i = out.find("\n" + want[0] + "\n")
        if not ok(i >= 0, "%s: no block for %s in:\n%s" % (what, pid, out)):
            continue
        got = out[i + 1 :].split("\n")[: len(want)]
        ok(got == want, "%s: %s's lines\n  got  %s\n  want %s" % (what, pid, got, want))
    if "--project" not in args:
        projects = len(os.listdir(HOME + "/projects"))
        line = "In all: %s %s, %s; text: %s bytes; on disk: %s bytes%s" % (g(projects), "project" if projects == 1 else "projects", "1 run" if n == 1 else "%s runs" % g(n), g(t), g(d), (" (%.1fx)" % (t / d)) if t and d else "")
        ok(("\n" + line + "\n") in out, "%s: the totals line %r in:\n%s" % (what, line, out))
    stats["see compared"] = stats.get("see compared", 0) + 1
    return out


def fresh_rebuild_equal(pid, what):
    """keys.cat and moments.jsonl equal those `rebuild-keys` makes again on a copy."""
    src = os.path.join(HOME, "projects", pid)
    copy = WORK + "/rebuild/" + pid
    if os.path.exists(copy):
        shutil.rmtree(copy)
    shutil.copytree(src, copy)
    for f in ("keys.cat", "moments.jsonl"):
        if os.path.exists(os.path.join(copy, f)):
            os.remove(os.path.join(copy, f))
    r = subprocess.run([SEARCH, "rebuild-keys", copy], env=ENV, stdin=subprocess.DEVNULL, capture_output=True)
    ok(r.returncode == 0, "%s: rebuild-keys: %s" % (what, r.stderr))
    for f in ("keys.cat", "moments.jsonl"):
        a = open(os.path.join(src, f), "rb").read() if os.path.exists(os.path.join(src, f)) else None
        b = open(os.path.join(copy, f), "rb").read() if os.path.exists(os.path.join(copy, f)) else None
        ok(a is not None and a == b, "%s: %s is not a fresh rebuild (%s and %s bytes)" % (what, f, None if a is None else len(a), None if b is None else len(b)))
    stats["fresh rebuilds compared"] = stats.get("fresh rebuilds compared", 0) + 1


def forgotten_ok(pid, ids, what, pieces=()):
    """The runs' files are gone, runs.jsonl has no line of them (a session piece: only its
    "forgotten" record), and keys.cat and moments.jsonl are a fresh rebuild."""
    runs = runs_of(pid)
    raw = open(os.path.join(HOME, "projects", pid, "runs.jsonl"), "rb").read().split(b"\n")
    for rid in ids:
        ok(run_files(pid, rid) == [], "%s: r%d's files are still there: %s" % (what, rid, run_files(pid, rid)))
        lines = [json.loads(l) for l in raw if l.strip() and json.loads(l).get("id") == rid]
        if rid in pieces:
            ok(len(lines) == 1 and lines[0].get("state") == "forgotten", "%s: r%d's record is not one forgotten mark: %s" % (what, rid, lines))
            ok(set(lines[0]) <= {"id", "kind", "state", "source", "offset", "size", "hash", "stamp", "start", "agent", "session", "reader", "forgotten"}, "%s: r%d's mark keeps more than where it was read from: %s" % (what, rid, sorted(lines[0])))
        else:
            ok(lines == [] and rid not in runs, "%s: runs.jsonl still has r%d: %s" % (what, rid, lines))
    fresh_rebuild_equal(pid, what)
    stats["runs forgotten"] = stats.get("runs forgotten", 0) + len(ids)


def ids(pid, kind=None, state=None):
    return sorted(i for i, x in runs_of(pid).items() if (kind is None or x.get("kind") == kind) and (state is None or x.get("state") == state) and x.get("state") != "forgotten")


def save_session(lines):
    path = CLAUDE + "/projects/-proj1/%s.jsonl" % SID
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        f.write("".join(json.dumps(x) + "\n" for x in lines))
    return search(["sessions", "sync"])


# ---------------------------------------------------------------- the store

print("0. two projects with runs of every kind", flush=True)
os.makedirs(HOME)
with open(HOME + "/config", "w") as f:
    f.write("dna = off\nsessions = on\n")
code, out, err = search(["add", DATA + "/HDFS_2k.log", DATA + "/Zookeeper_2k.log"])
ok(code == 0, "add in proj1: %s" % err)
for want in (0, 3):
    code, out, err = search(["run", "--", "sh", "-c", "echo 'INFO step one'; echo 'ERROR step two failed' >&2; exit $E"], FEWER_TEST_NO_PACK="1", E=str(want))
    ok(code == want, "run exit %d -> %d" % (want, code))
code, out, err = search(["pack-pending", "--wait"])
ok(code == 0, "pack-pending: %s" % err)
SID = "0d0d0d0d-1111-4222-8333-444444444444"
code, out, err = save_session([
    {"cwd": P1, "sessionId": SID, "type": "user", "message": {"role": "user", "content": "why does step two fail"}, "timestamp": "2026-11-18T10:00:00.000Z"},
    {"cwd": P1, "sessionId": SID, "type": "assistant", "message": {"role": "assistant", "content": "it reads a missing file"}, "timestamp": "2026-11-18T10:00:05.000Z"},
])
ok(code == 0 and "1 piece saved" in out, "sessions sync: %s %s" % (out, err))
code, out, err = search(["add", DATA + "/OpenSSH_2k.log", DATA + "/Spark_2k.log", DATA + "/Apache_2k.log"], proj=P2)
ok(code == 0, "add in proj2: %s" % err)
time.sleep(1.3)
code, out, err = search(["add", DATA + "/Linux_2k.log"], proj=P2)
ok(code == 0, "add in proj2 (later): %s" % err)
A, B = pid_of(P1), pid_of(P2)
print("   %s: %s; %s: %s" % (A, [(i, x.get("kind")) for i, x in sorted(runs_of(A).items())], B, [(i, x.get("kind")) for i, x in sorted(runs_of(B).items())]), flush=True)

# ---------------------------------------------------------------- 1. see

print("1. see: its numbers equal the files", flush=True)
out = check_see("see", ["--runs"])
ok("Limit: none: nothing is ever deleted on its own." in out, "see: no limit by default")
# (the DNA index's setting: `memory dna`, no longer in the overview, which ends with how to clear)
ok("The DNA index is off" not in out and "To delete what is saved: fewer memory clear (it asks first)." in out, "see: the overview's last line")
code, dna_out, err = search(["memory", "dna"])
ok(code == 0 and "The DNA index is off" in dna_out, "memory dna: the DNA index's setting: %r %r" % (dna_out[:200], err))
ok("Sessions: saved for Claude Code, Codex, Aider, Cline, Cursor." in out, "see: the sessions setting (sessions = on: every agent whose own setting is not off): %s" % out)
out = check_see("see --project", ["--project", B], [B])
ok(A not in out and "In all" not in out, "see --project: only that project")
out = check_see("see --project folder", ["--project", P2], [B])
code, out, err = search(["memory", "--runs"])
ok(code == 0 and all(("%s/r%d" % (p, i)) in out for p in (A, B) for i in runs_of(p)), "see --runs lists every run: %s" % out[-500:])
code, out, err = search(["memory", "--project", WORK + "/nowhere"])
ok(code == 1 and "no project" in err, "see --project unknown: %d %s" % (code, err))
# the overview, for people: each folder by size, the totals in plain words; no menu without a terminal
code, out, err = search(["memory"])
ok(code == 0 and out.startswith("fewer's memory, in ") and "\n   1. " in out and "\n   2. " in out and " outputs, files and chats, from 2 folders\n" in out and " packed into " in out
   and "  no size limit: nothing is deleted on its own\n" in out and out.rstrip("\n").endswith("To see each folder and delete what you want: fewer memory, in a terminal. More: fewer memory --help")
   and "Delete something?" not in out, "memory, the overview: %r" % out)
code, out, err = search(["memory", "--total"])
ok(code == 0 and "   1. " not in out and " outputs, files and chats, from 2 folders\n" in out, "memory --total: %r" % out)
# the menu, in a terminal: Enter leaves; a number not on the list deletes nothing; a folder's
# number asks before forgetting it (no: nothing goes)
before_menu = (ids(A), ids(B))
for pick, answer, want_code, want in (("", None, 0, None), ("9", None, 1, "Nothing deleted: type a number from 1 to 2, or \"all\"."), ("1", "n", 0, "[y/N] n")):
    code, out, asked = tty_menu(["memory"], pick, answer)
    ok(code == want_code and 'Delete something? Type a number, "all", or press Enter to leave: ' in out and (want is None or want in out) and asked == (answer is not None),
       "memory menu %r: %d %r %r" % (pick, code, asked, out[-300:]))
ok((ids(A), ids(B)) == before_menu, "the menu deleted nothing it was not told to")

# ---------------------------------------------------------------- 2. forget

print("2. forget in every form", flush=True)
first_add = ids(A, "add")[0]
before = state(HOME)
code, out, err = search(["memory", "forget", "%s/r%d" % (A, first_add)])
ok(code == 1 and "Not forgotten" in out and "To forget: 1 run" in out, "no terminal, no --yes: %d %s %s" % (code, out, err))
ok(state(HOME) == before, "no terminal, no --yes: the store is unchanged")
code, out, asked = tty(["memory", "forget", "%s/r%d" % (A, first_add)], "n")
ok(code == 0 and asked and "Nothing was forgotten." in out, "a terminal, answered n: %d %s" % (code, out))
ok(state(HOME) == before, "answered n: the store is unchanged")
next_before = int([l.split("=")[1] for l in open(os.path.join(HOME, "projects", A, "project")) if l.startswith("next")][0])
code, out, asked = tty(["memory", "forget", "%s/r%d" % (A, first_add)], "y")
ok(code == 0 and asked and "Forgot 1 run" in out, "a terminal, answered y: %d %s" % (code, out))
forgotten_ok(A, [first_add], "forget REF")
check_see("see after forget REF", ["--runs"])
# a line reference names its whole run
second_add = ids(A, "add")[0]
code, out, err = search(["memory", "forget", "--yes", "%s/r%d:5" % (A, second_add)])
ok(code == 0 and "the whole run %s/r%d is forgotten" % (A, second_add) in out, "a line reference: %s %s" % (out, err))
forgotten_ok(A, [second_add], "forget a line reference")
# unknown and forgotten runs are said, and nothing goes
code, out, err = search(["memory", "forget", "--yes", "%s/r999" % A, "%s/r%d" % (A, first_add)])
ok(code == 1 and "no run r999" in err and "(it was forgotten before)" in err and "nothing was forgotten" in err, "unknown runs: %d %s" % (code, err))
# a run still being written is refused; once it ended and is packed, it goes
k = subprocess.Popen([SEARCH, "run", "--", "sh", "-c", "echo 'INFO still running'; exec sleep 30"], env=env_for(P1, FEWER_TEST_NO_PACK="1"), cwd=P1, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
k.stdout.readline()
live = ids(A, "run", "running")
ok(len(live) == 1, "the run in progress: %s" % live)
code, out, err = search(["memory", "forget", "--yes", "%s/r%d" % (A, live[0])])
ok(code == 1 and "still being written" in err, "a run being written is refused: %d %s %s" % (code, out, err))
ok(run_files(A, live[0]) != [], "the run being written keeps its capture")
os.killpg(k.pid, signal.SIGKILL)
k.wait()
k.stdout.close()
k.stderr.close()
code, out, err = search(["memory", "forget", "--yes", "%s/r%d" % (A, live[0])])
ok(code == 0 and "Forgot 1 run" in out, "the run, its writer gone, is forgotten before it is packed: %s %s" % (out, err))
forgotten_ok(A, [live[0]], "forget a run whose writer is gone")
# --session: the piece leaves a mark; the sweep does not save it again, but saves what is new
piece = ids(A, "session")
code, out, err = search(["memory", "forget", "--yes", "--session", SID])
ok(code == 0 and "Forgot 1 run" in out and "keeps a record of where it was read from" in out, "--session: %s %s" % (out, err))
forgotten_ok(A, piece, "forget --session", pieces=piece)
code, out, err = search(["sessions", "sync"])
ok(code == 0 and "0 pieces saved" in out and ids(A, "session") == [], "the forgotten piece is not saved again: %s" % out)
code, out, err = save_session([{"cwd": P1, "sessionId": SID, "type": "user", "message": {"role": "user", "content": "and now step three"}, "timestamp": "2026-11-18T10:01:00.000Z"}])
new = ids(A, "session")
mark = runs_of(A)[piece[0]]
ok(code == 0 and "1 piece saved" in out and len(new) == 1, "what is new after it is saved: %s" % out)
if new:
    x = runs_of(A)[new[0]]
    ok(x.get("offset") == mark["offset"] + mark["size"] and x.get("continues") == piece[0], "the new piece starts where the forgotten one ended: %s %s" % (x, mark))
check_see("see after forget --session", ["--runs"])
# --before, narrowed by --project: proj2's first three files go, the later one stays
later = ids(B, "add")[-1]
edge = local(runs_of(B)[later]["start"] // 1000 * 1000)
early = [i for i in ids(B) if i != later]
code, out, err = search(["memory", "forget", "--yes", "--project", B, "--before", edge])
ok(code == 0 and "Forgot %d runs" % len(early) in out, "--before %s: %s %s" % (edge, out, err))
forgotten_ok(B, early, "forget --before")
ok(ids(B) == [later], "--before kept the later run: %s" % ids(B))
# --sessions
pieces = ids(A, "session")
code, out, err = search(["memory", "forget", "--yes", "--sessions"])
ok(code == 0 and ids(A, "session") == [], "--sessions: %s %s" % (out, err))
forgotten_ok(A, pieces, "forget --sessions", pieces=pieces)
# --project: every run of it; the project stays, with its next id
code, out, err = search(["memory", "forget", "--yes", "--project", B])
ok(code == 0 and ids(B) == [], "--project: %s %s" % (out, err))
forgotten_ok(B, [later], "forget --project")
ok(os.path.exists(os.path.join(HOME, "projects", B, "project")), "the project's folder and id counter stay")
check_see("see after forget --project", ["--runs"])
# --all; then ids are not used again
gone = ids(A)
code, out, err = search(["memory", "forget", "--yes", "--all"])
ok(code == 0 and ids(A) == [] and ids(B) == [], "--all: %s %s" % (out, err))
forgotten_ok(A, gone, "forget --all", pieces=new)
code, out, err = search(["add", DATA + "/HPC_2k.log"])
top = max(runs_of(A))
ok(code == 0 and top >= next_before and all(i < top for i in runs_of(A) if i != top), "ids are not used again: the new run is r%d (next was %d before the forgets)" % (top, next_before))
code, out, err = search(["memory", "forget", "--yes", "--all"])
ok(code == 0, "forget --all again")
code, out, err = search(["memory", "forget", "--yes", "--all"])
ok(code == 0 and "Nothing to forget." in out, "nothing left: %s" % out)
code, out, err = search(["memory", "forget", "--all", "--sessions"])
ok(code == 2, "two kinds of selection are refused: %d" % code)

# ---------------------------------------------------------------- 3. pause and resume

print("3. pause and resume", flush=True)


def runs_count():
    return sum(len(runs_of(p)) for p in os.listdir(HOME + "/projects"))


def run_here(cwd, status, text):
    n = runs_count()
    code, out, err = search(["run", "--", "sh", "-c", "echo %s; exit %d" % (text, status)], cwd=cwd, FEWER_TEST_NO_PACK="1")
    ok(code == status and text in out, "run in %s: exit %d and its output (%d %r)" % (cwd, status, code, out))
    return runs_count() - n


code, out, err = search(["memory", "pause"])
ok(code == 0 and os.path.exists(os.path.join(HOME, "projects", A, "paused")) and "Saving is paused for %s" % A in out, "pause: %s %s" % (out, err))
ok(run_here(P1, 5, "paused-output") == 0 and not os.listdir(os.path.join(HOME, "raw", A)), "paused: the command ran and nothing was saved")
check_see("see while paused", ["--runs"])
code, out, err = search(["memory", "resume"])
ok(code == 0 and not os.path.exists(os.path.join(HOME, "projects", A, "paused")), "resume: %s" % out)
ok(run_here(P1, 0, "saved-output") == 1, "resumed: the run is saved")
code, out, err = search(["memory", "pause", P1 + "/sub"])
ok(code == 0 and "pause = %s/sub" % P1 in open(HOME + "/config").read(), "pause FOLDER: the config line: %s" % open(HOME + "/config").read())
code, out, err = search(["memory", "pause", P1 + "/sub/"])
ok(code == 0 and "already paused" in out and open(HOME + "/config").read().count("pause =") == 1, "pause FOLDER twice: one line")
ok(run_here(P1 + "/sub", 2, "under-paused-folder") == 0, "under the paused folder: nothing saved")
ok(run_here(P1, 0, "beside-paused-folder") == 1, "beside the paused folder: saved")
code, out, err = search(["memory", "--runs"])
ok("Saving paused under %s/sub (config)." % P1 in out, "see shows the paused folder")
code, out, err = search(["memory"])
ok(("  saving paused under %s/sub\n" % P1) in out, "the overview shows the paused folder: %r" % out)
code, out, err = search(["memory", "resume", P1 + "/sub"])
ok(code == 0 and "pause =" not in open(HOME + "/config").read(), "resume FOLDER: the line is gone")
ok(run_here(P1 + "/sub", 0, "resumed-folder") == 1, "after resume FOLDER: saved")
code, out, err = search(["pack-pending", "--wait"])

# ---------------------------------------------------------------- 4. limit

print("4. limit", flush=True)
n = runs_count()
code, out, err = search(["add", DATA + "/HealthApp_2k.log"])
ok(code == 0 and runs_count() == n + 1 and not os.path.exists(HOME + "/forgotten.log"), "no limit: nothing goes")
code, out, err = search(["memory", "limit", "--size", "20XB"])
ok(code == 2, "a size that cannot be read: %d" % code)
code, out, err = search(["memory", "limit", "--size", "1B"])
ok(code == 0 and "limit_size = 1B" in open(HOME + "/config").read() and "Past it now" in out, "limit --size: %s %s" % (out, err))
alive = sorted((p, i) for p in os.listdir(HOME + "/projects") for i in ids(p))
ok(len(alive) > 1, "runs before the save: %s" % alive)
code, out, err = search(["add", DATA + "/Proxifier_2k.log"])
newest = max(ids(A))
left = sorted((p, i) for p in os.listdir(HOME + "/projects") for i in ids(p))
ok(code == 0 and left == [(A, newest)], "after the next save only the newest run is left: %s" % left)
log = open(HOME + "/forgotten.log").read() if os.path.exists(HOME + "/forgotten.log") else ""
ok("limit at most 1 bytes on disk: forgot %d runs" % len(alive) in log, "forgotten.log: %r" % log)
for p, i in alive:
    ok("%s/r%d" % (p, i) in log, "forgotten.log names %s/r%d" % (p, i))
    if (p, i) != (A, newest):
        forgotten_ok(p, [i], "the limit")
code, out, err = search(["memory", "--runs"])
ok("Forgotten by the limit" in out and "Limit: at most 1 bytes on disk" in out, "see shows the limit and what it forgot: %s" % out[-600:])
code, out, err = search(["memory", "limit", "--none"])
ok(code == 0 and "Limit: none" in out, "limit --none: %s" % out)
code, out, err = search(["add", DATA + "/Thunderbird_2k.log"])
ok(len(ids(A)) == 2, "with no limit nothing goes: %s" % ids(A))
# months, with the clock moved on
code, out, err = search(["memory", "limit", "--months", "1"])
ok(code == 0 and "limit_months = 1" in open(HOME + "/config").read(), "limit --months: %s" % out)
code, out, err = search(["add", DATA + "/Windows_2k.log"], FEWER_NOW=str(int(time.time() * 1000) + 40 * 86_400_000))
ok(code == 0 and len(ids(A)) == 1, "a month later, after the next save, only the newest run is left: %s" % ids(A))
code, out, err = search(["memory", "limit", "--none"])
check_see("see at the end", ["--runs"])

# ---------------------------------------------------------------- 5. the rest

print("5. sessions, secrets, dna, bad arguments", flush=True)
code, out, err = search(["memory", "sessions", "off"])
ok(code == 0 and "Sessions: off" in out and "sessions = off" in open(HOME + "/config").read(), "sessions off: %s" % out)
code, out, err = search(["memory", "sessions", "on", "codex"])
ok(code == 0 and "sessions.codex = on" in open(HOME + "/config").read(), "sessions on codex: %s" % out)
code, out, err = search(["memory", "sessions", "on", "nobody"])
ok(code == 2, "sessions for an unknown agent: %d" % code)
with open(HOME + "/config", "a") as f:
    f.write("secret = acme_ alnum 20\nsecret = bad\n")
code, out, err = search(["memory", "secrets"])
ok(code == 0 and "- AWS key:" in out and "- secret = acme_ alnum 20\n" in out and "secret = bad (not used:" in out, "secrets: %s" % out)
code, out, err = search(["memory", "dna"])
ok(code == 0 and "The DNA index is off" in out, "dna: %s" % out)
for bad in (["memory", "frobnicate"], ["memory", "forget"], ["memory", "pause", "a", "b"], ["memory", "limit", "--months", "0"], ["memory", "--runs", "--project"]):
    code, out, err = search(bad)
    ok(code == 2, "%s: exit %d" % (bad, code))

print("memory_check: %s" % ", ".join("%s %d" % kv for kv in sorted(stats.items())))
if fails:
    print("FAILED: %d" % len(fails))
    sys.exit(1)
print("ALL PASSED")
