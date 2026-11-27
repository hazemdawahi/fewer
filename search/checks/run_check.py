"""Acceptance checks of `run` (a pseudo-terminal or pipes): its tags, secrets hidden while
saving, pack-pending, and the scan of a run in progress.

    python3 -I checks/run_check.py BIN PACK WORK

BIN is fewer (target/debug or target/release), PACK the fewer-pack binary that packs,
WORK a scratch folder (made fresh; an existing one is
cleared only when it holds the mark file a check leaves). Every store, project and git
repository is made in WORK; every process gets a made-up HOME, agents' folders and claude in
WORK (safe_env.py), and no git settings of the developer's. The copies of public logs of
check_real_logs come from the folder FEWER_SAMPLES names (its loghub/: Android_2k.log,
HDFS_2k.log, Mac_2k.log, OpenSSH_2k.log); without it, that check says it is skipped. No
network; nothing is left running (each check ends with `pack-pending --wait`, which waits
for any detached one).

The checks:
- pty: a command printing colors, '\\r' progress bars and "\\r\\n" in a terminal (Python's pty
  module gives BIN one) leaves a capture equal byte for byte to what reached the terminal;
  the window size reaches the command, also when it changes; Ctrl-C reaches the command;
- pipes: each stream's bytes equal the command's, and its stderr lines are listed;
- the exit status passes through (0, 1, 3; a signal gives 128 + n), in both modes;
- tags in a temporary git repository (branch, commit, dirty, detached) and none outside one;
- every secret shape is hidden in the capture, shown on the terminal and counted; near misses
  are kept; a secret cut by a pause in the output is still hidden;
- pack-pending's archive equals `PACK -q -o x.cmf` of the same capture and its .idx equals
  `PACK index`'s; the run is ready and its capture gone;
- a run whose process was killed with -9 is packed later, with its note;
- the lines of a run in progress found by the capture scan keep their numbers and bytes
  after packing;
- and: no FEWER_HOME, a paused project, a command not found, the 4 GiB stop (made small with
  a test switch), FL_ variables passed to the command, the detached pack-pending.
"""
import errno
import fcntl
import importlib.util
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time

MARK = ".laneb-check-scratch"
FAILS = []
PASSES = [0]
HERE = os.path.dirname(os.path.abspath(__file__))
# (read before safe_env.py makes this process's environment the safe one)
SAMPLES = os.environ.get("FEWER_SAMPLES")
SAMPLE_LOGS = ["Android_2k.log", "HDFS_2k.log", "Mac_2k.log", "OpenSSH_2k.log"]
_spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(safe_env)


def ok(cond, what):
    if cond:
        PASSES[0] += 1
    else:
        FAILS.append(what)
        print("FAIL:", what, flush=True)


def fresh(path):
    if os.path.exists(path):
        if not os.path.exists(os.path.join(path, MARK)):
            sys.exit("refusing to clear %s: it holds no %s (not made by this check)" % (path, MARK))
        shutil.rmtree(path)
    os.makedirs(path)
    open(os.path.join(path, MARK), "w").close()


class Ctx:
    def __init__(self, binary, fewer, work):
        self.bin = binary
        self.fewer = fewer
        self.work = work
        self.n = 0

    def store(self, name):
        """A new store and project folder: (home, project, env)."""
        self.n += 1
        base = os.path.join(self.work, "%02d-%s" % (self.n, name))
        home = os.path.join(base, "home")
        proj = os.path.join(base, "proj")
        fake = os.path.join(base, "fakehome")
        for d in (home, proj, fake):
            os.makedirs(d)
        env = {
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": fake,
            "TERM": "xterm",
            "LANG": "en_US.UTF-8",
            "FEWER_HOME": home,
            "FEWER_PACK_BIN": self.fewer,
            "FEWER_PROJECT": proj,
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "FEWER_TEST_CATALOG_LOG": os.path.join(base, "catalog.log"),
        }
        return home, proj, env


PY = sys.executable


def py(code):
    """A command running Python code (isolated mode)."""
    return [PY, "-I", "-c", code]


def runs(home):
    """Every run of the store, merged by id: {(pid, id): record}."""
    out = {}
    pdir = os.path.join(home, "projects")
    for pid in sorted(os.listdir(pdir)) if os.path.isdir(pdir) else []:
        path = os.path.join(pdir, pid, "runs.jsonl")
        if not os.path.exists(path):
            continue
        for line in open(path, "rb").read().split(b"\n"):
            if not line.strip():
                continue
            rec = json.loads(line)
            out.setdefault((pid, rec["id"]), {}).update(rec)
    return out


def last_run(home):
    """The run started last in the store (any project): (pid, id, record)."""
    rs = runs(home)
    k = max(rs, key=lambda k: (rs[k].get("start", 0), k[1]))
    return k[0], k[1], rs[k]


def capture(home, pid, rid, ext="out"):
    return os.path.join(home, "raw", pid, "r%06d.%s" % (rid, ext))


def archive(home, pid, rid):
    return os.path.join(home, "projects", pid, "r%06d.cmf" % rid)


def pairs(path):
    if not os.path.exists(path):
        return []
    v = []
    for line in open(path).read().split("\n")[:-1]:
        a, b = line.split(" ")
        v.append((int(a), int(b)))
    return v


def errs(path):
    v = []
    for f, c in sorted(pairs(path)):
        if v and v[-1][0] + v[-1][1] >= f:
            v[-1] = (v[-1][0], max(v[-1][1], f + c - v[-1][0]))
        else:
            v.append((f, c))
    return v


def run_pipes(ctx, cmd, env, cwd=None, timeout=60):
    return subprocess.run([ctx.bin, "run", "--"] + cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=cwd, timeout=timeout)


def run_pty(ctx, cmd, env, cwd=None, size=None, interact=None, timeout=60, stderr_file=None, argv=None, stdin_file=None):
    """BIN run on a terminal of Python's pty module: (wait status, every byte that reached
    the terminal). `interact(fd, out)` is called after each read. `stderr_file`: BIN's
    stderr goes there instead of the terminal; `stdin_file`: BIN's stdin comes from there.
    `argv`: run this program instead (sh -c)."""
    pid, fd = pty.fork()
    if pid == 0:
        try:
            if size:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", size[0], size[1], 0, 0))
            if cwd:
                os.chdir(cwd)
            if stderr_file:
                f = os.open(stderr_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
                os.dup2(f, 2)
            if stdin_file:
                f = os.open(stdin_file, os.O_RDONLY)
                os.dup2(f, 0)
            if argv:
                os.execve(argv[0], argv, env)
            os.execve(ctx.bin, [ctx.bin, "run", "--"] + cmd, env)
        finally:
            os._exit(127)
    out = bytearray()
    end = time.time() + timeout
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
        if interact:
            interact(fd, out)
        if time.time() > end:
            os.kill(pid, signal.SIGKILL)
            ok(False, "pty run timed out: %r" % (cmd,))
            break
    _, st = os.waitpid(pid, 0)
    os.close(fd)
    return st, bytes(out)


def code_of(st):
    return os.waitstatus_to_exitcode(st) if hasattr(os, "waitstatus_to_exitcode") else (os.WEXITSTATUS(st) if os.WIFEXITED(st) else -os.WTERMSIG(st))


def pack_wait(ctx, env):
    return subprocess.run([ctx.bin, "pack-pending", "--wait"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=120)


def hide(ctx, data):
    r = subprocess.run([ctx.bin, "hide"], input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    return r.stdout, int(r.stderr.decode().split()[-1])


def pack_archive(ctx, src, dst):
    subprocess.run([ctx.fewer, "-q", "-o", dst, src], check=True, timeout=120)
    subprocess.run([ctx.fewer, "index", dst], check=True, timeout=120)


def same_file(a, b):
    return open(a, "rb").read() == open(b, "rb").read()


def check_packed(ctx, env, home, pid, rid, cap_copy, label):
    """After pack-pending: the archive equals PACK's of the capture (made from the capture
    before packing, at cap_copy + '.cmf'), the .idx equals PACK index's, the run is ready
    and its capture files are gone."""
    r = pack_wait(ctx, env)
    ok(r.returncode == 0, "%s: pack-pending --wait exits 0 (%s)" % (label, r.stderr.decode().strip()))
    a = archive(home, pid, rid)
    ok(os.path.exists(a) and same_file(a, cap_copy + ".cmf"), "%s: the archive equals `fewer-pack -q -o x.cmf` of the capture" % label)
    ok(os.path.exists(a + ".idx") and same_file(a + ".idx", cap_copy + ".cmf.idx"), "%s: the .idx equals `fewer-pack index`'s" % label)
    dec = subprocess.run([ctx.fewer, "-d", "-c", a], stdout=subprocess.PIPE, timeout=60).stdout
    ok(dec == open(cap_copy, "rb").read(), "%s: the archive opens to the capture's bytes" % label)
    rec = runs(home)[(pid, rid)]
    ok(rec.get("state") == "ready", "%s: state ready (%s)" % (label, rec.get("state")))
    ok(rec.get("archive") == os.path.getsize(a), "%s: archive bytes recorded" % label)
    data = open(cap_copy, "rb").read()
    lines = data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)
    ok(rec.get("bytes") == len(data) and rec.get("lines") == lines, "%s: bytes and lines recorded (%s %s, want %d %d)" % (label, rec.get("bytes"), rec.get("lines"), len(data), lines))
    for ext in ("out", "err", "times"):
        ok(not os.path.exists(capture(home, pid, rid, ext)), "%s: capture .%s gone" % (label, ext))
    return rec


def keep_capture(ctx, home, pid, rid, name):
    """A copy of a run's capture (mode and time kept) and PACK's archive and index of the
    capture itself, made before pack-pending."""
    src = capture(home, pid, rid)
    dst = os.path.join(ctx.work, name)
    shutil.copy2(src, dst)
    pack_archive(ctx, src, dst + ".cmf")
    return dst


# ---------------------------------------------------------------- the checks

TERMINAL = r'''
import sys, time
w = sys.stdout.write
w("\x1b[32mgreen\x1b[0m and \x1b[1;31mred bold\x1b[0m\n")
for i in range(0, 101, 5):
    w("\rprogress %3d%% [%-20s]" % (i, "#" * (i // 5)))
    sys.stdout.flush()
    time.sleep(0.01)
w("\n")
w("explicit cr lf\r\n")
w("tab\there, utf-8 \xe9 ✓, a bell \x07 and a NUL-free line\n")
sys.stdout.flush()
time.sleep(0.15)
w("spinner |\b/\b-\b\\\bdone\n")
w("\x1b[2K\x1b[1Gcleared line\n")
w("no line end at the end")
'''


def check_pty_terminal_bytes(ctx):
    home, proj, env = ctx.store("pty-bytes")
    env["FEWER_TEST_NO_PACK"] = "1"
    st, out = run_pty(ctx, py(TERMINAL), env, cwd=proj)
    ok(code_of(st) == 0, "pty: exit 0 (%s)" % code_of(st))
    pid, rid, rec = last_run(home)
    cap = open(capture(home, pid, rid), "rb").read()
    ok(len(out) > 300 and b"\r\n" in out and b"\x1b[32m" in out and b"\rprogress  50%" in out, "pty: the terminal got colors, \\r redraws and \\r\\n")
    ok(cap == out, "pty: the capture equals what reached the terminal, byte for byte (%d and %d bytes)" % (len(cap), len(out)))
    ok(rec.get("tty") is True and rec.get("exit") == 0 and rec.get("hidden") == 0 and rec.get("state") == "packing", "pty: the run's record (%s)" % rec)
    ok(b"explicit cr lf\r\r\n" in out, "pty: the terminal's own \\n -> \\r\\n is kept (an explicit \\r\\n arrives as \\r\\r\\n)")
    copy = keep_capture(ctx, home, pid, rid, "pty-bytes.out")
    check_packed(ctx, env, home, pid, rid, copy, "pty")


WINSIZE = r'''
import os, signal, sys
def show(*a):
    c, r = os.get_terminal_size(1)
    sys.stdout.write("size %dx%d\n" % (c, r))
    sys.stdout.flush()
signal.signal(signal.SIGWINCH, show)
show()
sys.stdin.readline()
sys.stdout.write("bye\n")
'''


def check_pty_window_size(ctx):
    home, proj, env = ctx.store("pty-size")
    state = {"step": 0}

    def interact(fd, out):
        if state["step"] == 0 and b"size 100x40" in out:
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 132, 0, 0))
            state["step"] = 1
        elif state["step"] == 1 and b"size 132x50" in out:
            os.write(fd, b"q\r")
            state["step"] = 2

    st, out = run_pty(ctx, py(WINSIZE), env, cwd=proj, size=(40, 100), interact=interact, timeout=30)
    ok(b"size 100x40" in out, "pty: the command starts with the terminal's size")
    ok(b"size 132x50" in out, "pty: a new window size reaches the command")
    ok(code_of(st) == 0 and b"bye" in out, "pty: keys reach the command (it read a line)")
    pack_wait(ctx, env)


def check_pty_restores_terminal(ctx):
    home, proj, env = ctx.store("pty-stty")
    env["FEWER_TEST_NO_PACK"] = "1"
    script = 'stty -g; "$0" run -- sh -c "echo inner; exit 5"; echo "status $?"; stty -g'
    st, out = run_pty(ctx, None, env, cwd=proj, argv=["/bin/sh", "-c", script, ctx.bin])
    lines = out.split(b"\r\n")
    ok(len(lines) >= 4 and lines[1] == b"inner" and lines[2] == b"status 5", "pty: run inside a shell on a terminal (%r)" % lines[:3])
    ok(len(lines) >= 4 and lines[0] == lines[3] and lines[0].count(b":") > 5, "pty: the terminal's settings are exactly as before (stty -g)")
    pack_wait(ctx, env)


STDERR_TO_FILE = r'''
import sys, time
for i in range(5):
    sys.stdout.write("out %d\n" % i)
    sys.stdout.flush()
    sys.stderr.write("err %d \x1b[31mred\x1b[0m\n" % i)
    sys.stderr.flush()
    time.sleep(0.01)
sys.stderr.write("password=hunter2 on stderr\n")
'''


def check_pty_stderr_redirected(ctx):
    home, proj, env = ctx.store("pty-stderr")
    env["FEWER_TEST_NO_PACK"] = "1"
    errf = os.path.join(ctx.work, "pty-stderr.txt")
    st, out = run_pty(ctx, py(STDERR_TO_FILE), env, cwd=proj, stderr_file=errf)
    direct = subprocess.run(py(STDERR_TO_FILE), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    ok(code_of(st) == 0, "pty, stderr to a file: exit 0")
    ok(out == direct.stdout.replace(b"\n", b"\r\n"), "pty, stderr to a file: the terminal shows stdout only (%r)" % out[:80])
    ok(open(errf, "rb").read() == direct.stderr, "pty, stderr to a file: the file gets the command's stderr unchanged")
    pid, rid, rec = last_run(home)
    cap = open(capture(home, pid, rid), "rb").read()
    lines = [l + b"\n" for l in cap.split(b"\n")[:-1]]
    listed = set()
    for f, c in errs(capture(home, pid, rid, "err")):
        listed.update(range(f, f + c))
    want_err, _ = hide(ctx, direct.stderr)
    ok(b"".join(l for i, l in enumerate(lines) if i + 1 in listed) == want_err, "pty, stderr to a file: stderr's lines are captured (secrets hidden) and listed")
    ok(b"".join(l for i, l in enumerate(lines) if i + 1 not in listed) == out, "pty, stderr to a file: the other lines are what reached the terminal")
    ok(rec.get("hidden") == 1, "pty, stderr to a file: the stderr secret is counted")
    pack_wait(ctx, env)


def check_pty_stdin_and_background(ctx):
    home, proj, env = ctx.store("pty-stdin")
    env["FEWER_TEST_NO_PACK"] = "1"
    src = os.path.join(ctx.work, "pty-stdin.txt")
    open(src, "wb").write(b"first\nsecond\nno end")
    # stdin a file, stdout a terminal: the command reads the file to its end, as without run
    st, out = run_pty(ctx, ["cat"], env, cwd=proj, stdin_file=src, timeout=30)
    pid, rid, rec = last_run(home)
    ok(code_of(st) == 0 and out == b"first\r\nsecond\r\nno end", "pty, stdin a file: the command reads the file and ends; the screen gets the command's terminal's bytes (%r)" % out)
    ok(open(capture(home, pid, rid), "rb").read() == out and rec.get("tty") is True, "pty, stdin a file: the capture is what reached the terminal")
    # run in the background of its terminal (a job of a shell with job control): it neither
    # changes the terminal nor stops
    script = 'set -m; stty -g; "$0" run -- sh -c "echo from the background" & wait $!; echo "status $?"; stty -g'
    st, out = run_pty(ctx, None, env, cwd=proj, argv=["/bin/sh", "-c", script, ctx.bin], timeout=30)
    modes = [l for l in out.replace(b"\r", b"").split(b"\n") if l.count(b":") > 5]
    ok(code_of(st) == 0 and b"from the background" in out and b"status 0" in out, "pty, in the background: the command runs, run is not stopped (%r)" % out[-60:])
    ok(len(modes) == 2 and modes[0] == modes[1], "pty, in the background: the terminal's settings are untouched (stty -g)")
    ok(last_run(home)[2].get("exit") == 0, "pty, in the background: the run is saved")
    pack_wait(ctx, env)


def check_pty_ctrl_c(ctx):
    home, proj, env = ctx.store("pty-ctrl-c")
    sent = {"done": False}

    def interact(fd, out):
        if not sent["done"] and b"ready" in out:
            os.write(fd, b"\x03")
            sent["done"] = True

    st, out = run_pty(ctx, ["sh", "-c", "echo ready; exec sleep 30"], env, cwd=proj, interact=interact, timeout=30)
    pid, rid, rec = last_run(home)
    ok(code_of(st) == 130, "pty: Ctrl-C ends the command and run exits 130 (%s)" % code_of(st))
    ok(rec.get("signal") == 2 and "exit" not in rec and "end" in rec, "pty: the run records signal 2 (%s)" % rec)
    pack_wait(ctx, env)


def check_exit_codes(ctx):
    home, proj, env = ctx.store("exit")
    cases = [(["sh", "-c", "exit 0"], 0, ("exit", 0)), (["sh", "-c", "exit 1"], 1, ("exit", 1)), (["sh", "-c", "exit 3"], 3, ("exit", 3)),
             (["sh", "-c", "kill -TERM $$"], 143, ("signal", 15)), (py("import os; os.kill(os.getpid(), 9)"), 137, ("signal", 9))]
    for cmd, want, (field, val) in cases:
        r = run_pipes(ctx, cmd, env, cwd=proj)
        rec = last_run(home)[2]
        ok(r.returncode == want, "pipes: %s exits %d (%d)" % (cmd[-1], want, r.returncode))
        ok(rec.get(field) == val, "pipes: %s records %s %s (%s)" % (cmd[-1], field, val, rec))
        st, _ = run_pty(ctx, cmd, env, cwd=proj)
        rec = last_run(home)[2]
        ok(code_of(st) == want, "pty: %s exits %d (%d)" % (cmd[-1], want, code_of(st)))
        ok(rec.get(field) == val and rec.get("tty") is True, "pty: %s records %s %s (%s)" % (cmd[-1], field, val, rec))
    pack_wait(ctx, env)


STREAMS = r'''
import os, sys, time
o, e = sys.stdout.buffer, sys.stderr.buffer
def put(f, b):
    f.write(b)
    f.flush()
put(o, b"out 1\n")
put(e, b"err 1\nerr 2\n")
put(o, b"out 2 \xff\xfe bytes that are not utf-8\n" + b"x" * 70000 + b"\n")
put(e, b"err 3 \x1b[31mred\x1b[0m\n")
for i in range(200):
    put(o if i % 3 else e, b"line %d of %s\n" % (i, b"stderr" if i % 3 == 0 else b"stdout"))
put(o, b"redraw 1\rredraw 2\r\n")
time.sleep(0.2)
put(e, b"err after a pause\n")
put(o, b"the end, no line end")
'''


def check_pipes_streams(ctx):
    home, proj, env = ctx.store("pipes")
    env["FEWER_TEST_NO_PACK"] = "1"
    direct = subprocess.run(py(STREAMS), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    r = run_pipes(ctx, py(STREAMS), env, cwd=proj)
    ok(r.returncode == 0, "pipes: exit 0")
    ok(r.stdout == direct.stdout, "pipes: our stdout equals the command's stdout, byte for byte (%d bytes)" % len(direct.stdout))
    ok(r.stderr == direct.stderr, "pipes: our stderr equals the command's stderr, byte for byte (%d bytes)" % len(direct.stderr))
    pid, rid, rec = last_run(home)
    cap = open(capture(home, pid, rid), "rb").read()
    lines = cap.split(b"\n")
    lines = [l + b"\n" for l in lines[:-1]] + ([lines[-1]] if lines[-1] else [])
    err_lines = set()
    for f, c in errs(capture(home, pid, rid, "err")):
        err_lines.update(range(f, f + c))
    got_err = b"".join(l for i, l in enumerate(lines) if i + 1 in err_lines)
    got_out = b"".join(l for i, l in enumerate(lines) if i + 1 not in err_lines)
    ok(got_err == direct.stderr, "pipes: the capture's lines listed in .err are stderr's bytes, in order")
    ok(got_out == direct.stdout, "pipes: the capture's other lines are stdout's bytes, in order")
    ok(rec.get("tty") is False and rec.get("lines") == len(lines) and rec.get("bytes") == len(cap), "pipes: the run's record (%s)" % {k: rec.get(k) for k in ("tty", "lines", "bytes")})
    marks = pairs(capture(home, pid, rid, "times"))
    ok(len(marks) >= 1 and all(m[1] >= 100 for m in marks), "pipes: arrival marks written after a pause (%s)" % marks)
    copy = keep_capture(ctx, home, pid, rid, "pipes.out")
    rec = check_packed(ctx, env, home, pid, rid, copy, "pipes")
    if os.path.exists(env["FEWER_TEST_CATALOG_LOG"]):
        # the catalog stand-in logs what it is given
        cat = [json.loads(l) for l in open(env["FEWER_TEST_CATALOG_LOG"]).read().split("\n") if l]
        ok(cat and cat[-1]["errs"] == [list(x) for x in errs_from(lines, err_lines)] and cat[-1]["arrivals"] == [list(m) for m in marks], "pipes: the catalog is given the capture's stderr lines and arrival marks")
    else:
        # fewer's own catalog.rs: the run's catalog is written
        ok(os.path.exists(archive(home, pid, rid)[:-4] + ".cat") and rec.get("rule") == 1, "pipes: the run's catalog is written (%s)" % {k: rec.get(k) for k in ("format", "rule")})


def errs_from(lines, err_lines):
    v = []
    for i in range(1, len(lines) + 1):
        if i in err_lines:
            if v and v[-1][0] + v[-1][1] == i:
                v[-1] = (v[-1][0], v[-1][1] + 1)
            else:
                v.append((i, 1))
    return v


def check_pipes_ctrl_c(ctx):
    home, proj, env = ctx.store("pipes-ctrl-c")
    for sig, want in ((signal.SIGINT, 130), (signal.SIGTERM, 143)):
        p = subprocess.Popen([ctx.bin, "run", "--", "sh", "-c", "echo ready; exec sleep 30"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=proj, start_new_session=True)
        line = p.stdout.readline()
        # what a terminal does on Ctrl-C: the signal to the whole foreground group
        os.killpg(p.pid, sig)
        out, err = p.communicate(timeout=30)
        rec = last_run(home)[2]
        ok(line == b"ready\n" and p.returncode == want, "pipes: %s to the group reaches the command; run exits %d (%s)" % (sig.name, want, p.returncode))
        ok(rec.get("signal") == int(sig) and "end" in rec and rec.get("state") == "packing", "pipes: run survives %s and records the end (%s)" % (sig.name, rec))
    pack_wait(ctx, env)


def git(repo, *args):
    env = {"PATH": "/usr/bin:/bin", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "HOME": repo}
    return subprocess.run(["git", "-C", repo, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"] + list(args), env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout.decode().strip()


def check_tags(ctx):
    home, proj, env = ctx.store("tags")
    repo = os.path.join(ctx.work, "repo")
    os.makedirs(repo)
    del env["FEWER_PROJECT"]
    env["GIT_CEILING_DIRECTORIES"] = ctx.work
    git(repo, "init", "-q")
    run_pipes(ctx, ["true"], env, cwd=repo)
    rec = last_run(home)[2]
    ok(not any(k in rec for k in ("branch", "commit", "dirty")), "tags: none before the first commit (%s)" % rec)
    open(os.path.join(repo, "a.txt"), "w").write("one\n")
    git(repo, "add", "a.txt")
    git(repo, "commit", "-q", "-m", "first")
    head = git(repo, "rev-parse", "HEAD")
    run_pipes(ctx, ["true"], env, cwd=repo)
    pid, rid, rec = last_run(home)
    ok(rec.get("branch") == "main" and rec.get("commit") == head and rec.get("dirty") is False, "tags: branch, commit and clean in a repository (%s)" % rec)
    ok(os.path.realpath(rec.get("cwd", "")) == os.path.realpath(repo), "tags: cwd recorded")
    root = open(os.path.join(home, "projects", pid, "project")).read().split("\n")[0]
    ok(root in ("root = " + os.path.realpath(repo), "root = " + repo), "tags: the project is the repository's root (%s)" % root)
    open(os.path.join(repo, "untracked.txt"), "w").write("x\n")
    run_pipes(ctx, ["true"], env, cwd=repo)
    ok(last_run(home)[2].get("dirty") is False, "tags: an untracked file is not dirty")
    open(os.path.join(repo, "a.txt"), "w").write("two\n")
    st, _ = run_pty(ctx, ["true"], env, cwd=repo)
    ok(last_run(home)[2].get("dirty") is True, "tags: a changed tracked file is dirty (pty mode too)")
    os.makedirs(os.path.join(repo, "sub"))
    run_pipes(ctx, ["true"], env, cwd=os.path.join(repo, "sub"))
    ok(last_run(home)[2].get("commit") == head, "tags: from a subfolder")
    git(repo, "checkout", "-q", "--detach")
    run_pipes(ctx, ["true"], env, cwd=repo)
    ok(last_run(home)[2].get("branch") == "HEAD", "tags: detached HEAD gives branch HEAD")
    git(repo, "checkout", "-q", "-b", "feature/x")
    run_pipes(ctx, ["true"], env, cwd=repo)
    ok(last_run(home)[2].get("branch") == "feature/x", "tags: a branch name with a slash")
    outside = os.path.join(ctx.work, "outside")
    os.makedirs(outside)
    env["FEWER_PROJECT"] = outside
    run_pipes(ctx, ["true"], env, cwd=outside)
    rec = last_run(home)[2]
    ok(not any(k in rec for k in ("branch", "commit", "dirty")), "tags: none outside a repository (%s)" % rec)
    pack_wait(ctx, env)


def fake(prefix, body, n):
    b = ""
    while len(b) < n:
        b += body
    return prefix + b[:n]


def secret_text():
    """Made-up secrets of every shape (built here at run time), the near misses, and the
    number of values that must be hidden."""
    jwt = "%s.%s.%s" % (fake("eyJ", "hbGciOiJIUzI1NiJ9", 17), fake("eyJ", "zdWIiOiIxMjM0NTY3ODkwIn0", 24), "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c")
    lines = [
        "aws key " + fake("AKIA", "IOSFODNN7EXAMPLE", 16) + " end",
        "aws_secret_access_key = " + "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "github " + fake("ghp_", "A1b2C3d4E5f6G7h8", 36),
        "fine-grained " + fake("github_pat_", "11ABCDEFG0aBcDeFgHiJkL_mNoPqRsTuVwXyZ0123", 49),
        "gitlab " + fake("glpat-", "AbCdEfGhIjKlMnOpQrSt_-", 22),
        "slack " + fake("xoxb-", "1234567890-AbCdEfGh", 25),
        "stripe " + fake("sk_live_", "AbCdEfGhIj0123456789", 24),
        "anthropic " + fake("sk-ant-", "api03-AbCd_EfGh-IjKl0123", 43),
        "openai " + fake("sk-proj-", "AbCd_EfGh-IjKl0123", 42),
        "google " + fake("AIza", "SyAbCdEfGh_IjKl-MnOp0123456789", 35),
        "jwt " + jwt,
        "Authorization: Bearer abc.def-ghi",
        "password=hunter2 user=bob",
        "postgres://app:pa55w0rd@db:5432/app",
        "-----BEGIN RSA PRIVATE KEY-----",
        fake("MIIE", "owIBAAKCAQEAxYz+/9", 60),
        fake("MIIE", "owIBAAKCAQEAxYz+/8", 60),
        "-----END RSA PRIVATE KEY-----",
    ]
    hidden = 15
    near = [
        "near " + fake("AKIA", "IOSFODNN7EXAMPLE", 15),
        "near " + fake("ghp_", "A1b2C3d4E5f6G7h8", 35),
        "near " + fake("pk_live_", "AbCdEfGhIj0123456789", 24),
        "passwordless=true login",
        "max_tokens: 100, input_tokens=5",
        "task-1234567890abcdefghijkl",
        "-----BEGIN PUBLIC KEY-----",
    ]
    return lines, near, hidden


def secrets_cmd(lines, near):
    # the secrets are written one line at a time, and one is cut by a pause longer than
    # the 100 ms rule: "pass", then "word=..."
    text = "\n".join(lines + near) + "\n"
    code = "import sys, time\nw = sys.stdout\nfor l in %r.splitlines(True):\n    w.write(l)\n    w.flush()\nw.write('pass')\nw.flush()\ntime.sleep(0.3)\nw.write('word=after-a-pause tail\\n')\nw.flush()\n" % text
    return py(code)


def check_secrets(ctx):
    lines, near, n = secret_text()
    for mode in ("pipes", "pty"):
        home, proj, env = ctx.store("secrets-" + mode)
        env["FEWER_TEST_NO_PACK"] = "1"
        if mode == "pipes":
            r = run_pipes(ctx, secrets_cmd(lines, near), env, cwd=proj)
            shown, code = r.stdout, r.returncode
        else:
            st, shown = run_pty(ctx, secrets_cmd(lines, near), env, cwd=proj)
            code = code_of(st)
        pid, rid, rec = last_run(home)
        cap = open(capture(home, pid, rid), "rb").read()
        want, count = hide(ctx, shown)
        ok(code == 0, "%s secrets: exit 0" % mode)
        ok(all(l.encode() in shown for l in lines + near) and b"password=after-a-pause" in shown, "%s secrets: the terminal shows every original" % mode)
        ok(cap == want, "%s secrets: the capture is the output with secrets hidden" % mode)
        ok(count == n + 1 and rec.get("hidden") == n + 1, "%s secrets: %d values hidden and counted (%s, %s)" % (mode, n + 1, count, rec.get("hidden")))
        ok(not any(s in cap for s in [b"hunter2", b"pa55w0rd", b"AKIAIOSFODNN7EXAMPLE end", b"MIIEowIBAAKC", b"after-a-pause", b"abc.def-ghi"]), "%s secrets: no secret value is in the capture" % mode)
        ok(all(l.encode() in cap for l in near), "%s secrets: near misses are kept" % mode)
        for name in ["AWS key", "AWS secret", "GitHub token", "GitLab token", "Slack token", "Stripe key", "Anthropic key", "OpenAI key", "Google API key", "JWT", "authorization", "password", "URL password", "private key"]:
            ok(("[hidden: %s]" % name).encode() in cap, "%s secrets: marker %s" % (mode, name))
        pack_wait(ctx, env)


def check_kill9(ctx):
    home, proj, env = ctx.store("kill9")
    p = subprocess.Popen([ctx.bin, "run", "--", "sh", "-c", 'echo "pid $$"; i=0; while true; do i=$((i+1)); echo "line $i"; sleep 0.02; done'], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=proj, start_new_session=True)
    child = int(p.stdout.readline().split()[1])
    seen = b""
    while b"line 30\n" not in seen:
        seen += p.stdout.readline()
    os.kill(p.pid, signal.SIGKILL)
    p.wait()
    p.stdout.close()
    p.stderr.close()
    # the command dies of SIGPIPE at its next line; make sure it is gone
    end = time.time() + 5
    while time.time() < end:
        try:
            os.kill(child, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    else:
        os.kill(child, signal.SIGKILL)
    pid, rid, rec = last_run(home)
    ok(rec.get("state") == "running" and "end" not in rec, "kill -9: the run is left running, without an end (%s)" % rec)
    copy = keep_capture(ctx, home, pid, rid, "kill9.out")
    data = open(copy, "rb").read()
    ok(data.startswith(b"pid %d\nline 1\n" % child) and data.endswith(b"\n"), "kill -9: the capture holds whole lines")
    rec = check_packed(ctx, env, home, pid, rid, copy, "kill -9")
    ok(rec.get("note") == "ended without a status" and "end" in rec, "kill -9: packed later with its note (%s)" % rec.get("note"))
    ok("exit" not in rec and "signal" not in rec, "kill -9: no status is made up")


def check_kill9_pty(ctx):
    home, proj, env = ctx.store("kill9-pty")
    state = {"done": False}

    def interact(fd, out):
        if not state["done"] and b"line 20\r\n" in out:
            # the run's record names its writer: BIN, the pty child
            rs = runs(home)
            if rs:
                rec = list(rs.values())[-1]
                os.kill(rec["writer"], signal.SIGKILL)
                state["done"] = True

    st, out = run_pty(ctx, ["sh", "-c", 'i=0; while [ $i -lt 2000 ]; do i=$((i+1)); echo "line $i"; sleep 0.01; done'], env, cwd=proj, interact=interact, timeout=60)
    ok(state["done"] and os.WIFSIGNALED(st) and os.WTERMSIG(st) == 9, "kill -9 (pty): run was killed")
    # the command got SIGHUP when the pseudo-terminal closed
    pid, rid, rec = last_run(home)
    end = time.time() + 5
    while time.time() < end and capture_grows(home, pid, rid):
        pass
    copy = keep_capture(ctx, home, pid, rid, "kill9-pty.out")
    rec = check_packed(ctx, env, home, pid, rid, copy, "kill -9 (pty)")
    ok(rec.get("note") == "ended without a status" and rec.get("tty") is True, "kill -9 (pty): packed later with its note")


def capture_grows(home, pid, rid):
    a = os.path.getsize(capture(home, pid, rid))
    time.sleep(0.3)
    return os.path.getsize(capture(home, pid, rid)) != a


IN_PROGRESS = r'''
import sys, time
for i in range(1, 301):
    sys.stdout.write("row %d %s\n" % (i, "needle here" if i % 7 == 0 else "hay"))
    if i % 25 == 0:
        sys.stdout.flush()
        time.sleep(0.05)
    if i == 150:
        sys.stdout.write("half done\n")
        sys.stdout.flush()
        sys.stdin.readline()
'''


def check_in_progress(ctx):
    home, proj, env = ctx.store("in-progress")
    env["FEWER_TEST_NO_PACK"] = "1"
    p = subprocess.Popen([ctx.bin, "run", "--"] + py(IN_PROGRESS), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=proj)
    while p.stdout.readline() != b"half done\n":
        pass
    pid, rid, rec = last_run(home)
    cap = capture(home, pid, rid)
    ok(rec.get("state") == "running", "in progress: the run is running")
    size = os.path.getsize(cap)
    r = subprocess.run([ctx.bin, "scan-capture", cap, str(size), "needle"], stdout=subprocess.PIPE, timeout=60)
    found = [l.split(b"\t", 1) for l in r.stdout.split(b"\n") if l and not l.startswith(b"#")]
    prefix = open(cap, "rb").read()[:size].split(b"\n")
    want = [(i + 1, l) for i, l in enumerate(prefix) if b"needle" in l]
    ok([(int(a), b) for a, b in found] == want and len(want) >= 20, "in progress: the scan finds every line holding the text, with its number (%d)" % len(want))
    # a cursor: from line 100 on
    r2 = subprocess.run([ctx.bin, "scan-capture", cap, str(size), "needle", "100"], stdout=subprocess.PIPE, timeout=60)
    found2 = [int(l.split(b"\t", 1)[0]) for l in r2.stdout.split(b"\n") if l and not l.startswith(b"#")]
    ok(found2 == [n for n, _ in want if n >= 100], "in progress: from a cursor line on")
    p.stdin.write(b"go on\n")
    p.stdin.flush()
    p.communicate(timeout=60)
    copy = keep_capture(ctx, home, pid, rid, "in-progress.out")
    check_packed(ctx, env, home, pid, rid, copy, "in progress")
    dec = subprocess.run([ctx.fewer, "-d", "-c", archive(home, pid, rid)], stdout=subprocess.PIPE, timeout=60).stdout.split(b"\n")
    ok(all(dec[n - 1] == b for n, b in want), "in progress: after packing, every reference found in progress names the same line")


def check_real_logs(ctx):
    """Copies of public logs (samples/, from data/loghub, CRLF line ends) through `run`: the
    terminal gets them unchanged, the capture holds them with secrets hidden as secrets::hide
    hides them, and the archive is fewer-pack's own of the capture."""
    if not SAMPLES:
        print("check_real_logs: skipped (FEWER_SAMPLES is not set: the copies of public logs stay out of the repository)", flush=True)
        return
    sdir = os.path.join(SAMPLES, "loghub")
    names = [n for n in SAMPLE_LOGS if os.path.isfile(os.path.join(sdir, n))]
    ok(names == SAMPLE_LOGS, "real logs: the samples are there (%s)" % names)
    for i, name in enumerate(names):
        home, proj, env = ctx.store("real-" + name)
        env["FEWER_TEST_NO_PACK"] = "1"
        src = os.path.join(sdir, name)
        orig = open(src, "rb").read()
        if i == 0:
            st, shown = run_pty(ctx, ["cat", src], env, cwd=proj)
            mode, code = "pty", code_of(st)
            # (a terminal turns "\n" into "\r\n"; macOS's terminal driver may also write a
            # '\r' twice when its output queue fills between the two: what reached the
            # terminal is compared without '\r', and the capture with what reached it)
            ok(shown.replace(b"\r", b"") == orig.replace(b"\r", b"") and shown.count(b"\n") == orig.count(b"\n"), "real logs (%s, pty): the terminal gets the file's text" % name)
        else:
            r = run_pipes(ctx, ["cat", src], env, cwd=proj)
            mode, code, shown = "pipes", r.returncode, r.stdout
            ok(shown == orig, "real logs (%s, pipes): stdout is the file, byte for byte" % name)
        pid, rid, rec = last_run(home)
        want, count = hide(ctx, shown)
        cap = open(capture(home, pid, rid), "rb").read()
        ok(code == 0 and cap == want and rec.get("hidden") == count, "real logs (%s, %s): the capture is the output with %d values hidden" % (name, mode, count))
        copy = keep_capture(ctx, home, pid, rid, "real-" + name)
        check_packed(ctx, env, home, pid, rid, copy, "real logs (%s, %s)" % (name, mode))


def check_misc(ctx):
    # no FEWER_HOME: the command still runs, nothing is saved
    home, proj, env = ctx.store("misc")
    e2 = safe_env.without(env, "FEWER_HOME")
    r = run_pipes(ctx, ["sh", "-c", "echo hi; exit 4"], e2, cwd=proj)
    ok(r.returncode == 4 and r.stdout == b"hi\n" and b"not saving" in r.stderr, "no FEWER_HOME: the command runs, with a warning")
    ok(not os.listdir(home), "no FEWER_HOME: nothing saved")
    # a command not found
    r = run_pipes(ctx, ["no-such-command-xyz"], env, cwd=proj)
    rec = last_run(home)[2]
    ok(r.returncode == 127 and rec.get("exit") == 127 and "could not start" in rec.get("note", ""), "not found: exit 127, recorded (%s)" % rec.get("note"))
    # FL_ variables reach the command (run comes before the engine's switch clearing)
    e3 = dict(env, FL_TEST_KEEP="kept", P1_X="also")
    r = run_pipes(ctx, ["sh", "-c", 'echo "$FL_TEST_KEEP $P1_X"'], e3, cwd=proj)
    ok(r.stdout == b"kept also\n", "FL_ and P1_ variables reach the command (%r)" % r.stdout)
    # the 4 GiB stop, made small
    e4 = dict(env, FEWER_TEST_CAPTURE_MAX="100", FEWER_TEST_NO_PACK="1")
    r = run_pipes(ctx, py("import sys\nfor i in range(50): print('line %02d of fifty' % i)"), e4, cwd=proj)
    pid, rid, rec = last_run(home)
    ok(r.stdout.count(b"\n") == 50, "capture limit: the output still reaches the terminal")
    ok(rec.get("note") == "saving stopped at 100 bytes" and os.path.getsize(capture(home, pid, rid)) <= 100 and open(capture(home, pid, rid), "rb").read().endswith(b"\n"), "capture limit: saving stops on a line end, with its note")
    # a paused project: the command runs, nothing more is saved
    pack_wait(ctx, env)
    before = runs(home)
    open(os.path.join(home, "projects", pid, "paused"), "w").close()
    r = run_pipes(ctx, ["sh", "-c", "echo paused run; exit 2"], env, cwd=proj)
    ok(r.returncode == 2 and r.stdout == b"paused run\n" and runs(home) == before, "paused: the command runs and nothing is saved")
    os.remove(os.path.join(home, "projects", pid, "paused"))
    # a folder paused in the config
    open(os.path.join(home, "config"), "w").write("pause = %s\n" % proj)
    r = run_pipes(ctx, ["true"], env, cwd=proj)
    ok(r.returncode == 0 and runs(home) == before, "paused folder: nothing is saved")
    os.remove(os.path.join(home, "config"))
    # the detached pack-pending: run starts it; pack-pending --wait waits for it
    r = run_pipes(ctx, ["sh", "-c", "echo packed by the detached one"], env, cwd=proj)
    pack_wait(ctx, env)
    pid, rid, rec = last_run(home)
    ok(rec.get("state") == "ready" and not os.path.exists(capture(home, pid, rid)), "detached pack-pending packed the run (%s)" % rec.get("state"))
    # extra secret shapes from the config
    open(os.path.join(home, "config"), "w").write("secret = acme_ alnum 20\nsecret = broken line\n")
    tok = fake("acme_", "Zz09Yy18Xx27Ww36", 24)
    e5 = dict(env, FEWER_TEST_NO_PACK="1")
    r = run_pipes(ctx, ["sh", "-c", "echo key %s end" % tok], e5, cwd=proj)
    pid, rid, rec = last_run(home)
    ok(open(capture(home, pid, rid), "rb").read() == b"key [hidden: acme_] end\n" and rec.get("hidden") == 1 and b"broken line" in r.stderr, "config: an extra secret shape is hidden; a bad line is told")
    os.remove(os.path.join(home, "config"))
    pack_wait(ctx, env)


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    binary, fewer, work = (os.path.abspath(a) for a in sys.argv[1:4])
    fresh(work)
    # every process this check starts gets a made-up HOME, agents' folders and claude, inside
    # WORK (safe_env.py)
    safe = safe_env.setup(work)
    ctx = Ctx(binary, fewer, work)
    checks = [check_pty_terminal_bytes, check_pty_window_size, check_pty_restores_terminal, check_pty_stderr_redirected, check_pty_stdin_and_background, check_pty_ctrl_c, check_exit_codes, check_pipes_streams, check_pipes_ctrl_c, check_tags, check_secrets, check_kill9, check_kill9_pty, check_in_progress, check_real_logs, check_misc]
    only = safe.caller.get("CHECK_ONLY")
    for c in checks:
        if only and only not in c.__name__:
            continue
        n0, f0 = PASSES[0], len(FAILS)
        t = time.time()
        try:
            c(ctx)
        except Exception as e:
            ok(False, "%s raised %r" % (c.__name__, e))
        print("%-28s %3d passed, %d failed (%.1f s)" % (c.__name__, PASSES[0] - n0, len(FAILS) - f0, time.time() - t), flush=True)
    print("all passed (%d checks)" % PASSES[0] if not FAILS else "%d FAILED of %d" % (len(FAILS), PASSES[0] + len(FAILS)))
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
