"""The acceptance check of `run` through the real tools: "Search finds lines of a run in
progress, and their references still point at the same lines after packing", and the answers
say how such a run stands.

    python3 -I checks/run_mcp_check.py FEWER PACK CHECKS WORK

FEWER is fewer, PACK the fewer-pack that packs, CHECKS the folder of fewer's
checks/ (for its MCP client, mcpc.py, and safe_env.py), WORK a scratch folder.
Every process gets a made-up HOME, agents' folders and claude in WORK (safe_env.py).

1. `fewer run` starts a command that prints 150 numbered rows (every 7th holds the
   word "needle" and a word of its own, so no two share a message key), then waits.
2. While it waits, the MCP `search` tool finds "needle": every line of the capture so far
   that holds it, each with its reference, and says "r1 is still running" with the number of
   lines searched; `logs` shows the run's output size so far and "still running"; `lines`
   reads a line of the capture and says so too; `count` without a text names the run as not
   counted yet, and with a text counts its output so far.
3. The command ends (150 more rows), pack-pending packs the run.
4. Each reference found in step 2 is read back with the `lines` tool (base64, the exact
   bytes): the same line. A new search finds the same lines at the same references, plus
   the later ones, and no longer says the run is running.
5. A second run whose `run` is killed with -9 (its capture's lock gone, capture::writer_alive)
   is named as "ended without a status" by search and logs until pack-pending packs it, with
   that note.
"""
import base64
import importlib.util
import json
import os
import re
import signal
import subprocess
import sys
import time

ROWS = r'''
import sys, time
words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
         "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
         "uniform", "victor", "whiskey", "xray", "yankee", "zulu", "able", "baker", "dog", "easy",
         "fox", "george", "how", "item", "jig", "king", "love", "nan", "oboe", "peter", "queen", "roger", "sugar"]
for i in range(1, 301):
    if i % 7 == 0:
        sys.stdout.write("row %d needle %s\n" % (i, words[i // 7]))
    else:
        sys.stdout.write("row %d hay\n" % i)
    if i == 150:
        sys.stdout.write("half done\n")
        sys.stdout.flush()
        sys.stdin.readline()
'''

FAILS = []


def ok(cond, what):
    print(("ok:   " if cond else "FAIL: ") + what, flush=True)
    if not cond:
        FAILS.append(what)


def refs(text):
    """(reference, line number) pairs of an answer's lines: "<pid>/r<run>:<line>"."""
    return [(m.group(0), int(m.group(2))) for m in re.finditer(r"(?<![\w/-])((?:[a-z0-9-]+/)?r\d+):(\d+)\b", text)]


def load(checks, name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(checks, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def main():
    if len(sys.argv) != 5:
        sys.exit(__doc__)
    binary, fewer, checks, work = (os.path.abspath(a) for a in sys.argv[1:5])
    mcpc = load(checks, "mcpc")
    mcpc.fresh_work(work)
    # every process this check starts, the server too, gets a made-up HOME, agents' folders
    # and claude, inside WORK (safe_env.py)
    safe = load(checks, "safe_env").setup(work)
    home, proj = os.path.join(work, "home"), os.path.join(work, "proj")
    os.makedirs(home)
    os.makedirs(proj)
    env = safe.env(FEWER_HOME=home, FEWER_PACK_BIN=fewer, FEWER_PROJECT=proj)
    run = subprocess.Popen([binary, "run", "--", sys.executable, "-I", "-c", ROWS], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, env=dict(env, FEWER_TEST_NO_PACK="1"), cwd=proj)
    while run.stdout.readline() != b"half done\n":
        pass
    pid = os.listdir(os.path.join(home, "projects"))[0]
    # (the calls name no project: this project's runs come as "r1", the token test of
    # 2026-11-23; the expected texts are written with the project and taken out by `short`)
    short = lambda t: t.replace(pid + "/r", "r")
    cap = os.path.join(home, "raw", pid, "r000001.out")
    prefix = open(cap, "rb").read()
    want = [(i + 1, l) for i, l in enumerate(prefix.split(b"\n")) if b"needle" in l]
    srv = mcpc.Server(binary, env, proj, os.path.join(work, "mcp.err"))
    srv.initialize()
    text, err = srv.call("search", {"text": "needle", "limit": 500, "max_tokens": 20000})
    print(text)
    found = sorted(set(n for _, n in refs(text)))
    ok(not err and found == [n for n, _ in want], "search on a run in progress: every line holding the text, with its number (%d lines)" % len(want))
    lines_so_far = prefix.count(b"\n")
    ok(short("%s/r1 is still running: searched its output so far (%d lines)" % (pid, lines_so_far)) in text, "search says the run is still running, with the lines searched")
    # the shown text of each found line is the capture's line (a one-run count names its run in
    # its first line, with no row: the token tests of 2026-11-23)
    # (compact lines made whole again first: mcpc.expand)
    ok(all(("\nr1:%d %s\n" % (n, l.decode())) in "\n" + mcpc.expand(text) for n, l in want), "each line is shown as the capture has it, after its reference")
    # logs: the capture's size so far, and how the run stands
    text, err = srv.call("logs", {})
    print(text)
    size = os.path.getsize(cap)
    ok(not err and ("{:,} bytes of output so far, still running".format(size)) in text and "1 run (1 still running)" in text, "logs shows the run's output size so far and that it is still running")
    # lines: a line of the capture, and the run's state in the heading
    n0, l0 = want[0]
    text, err = srv.call("lines", {"ref": "%s/r1:%d" % (pid, n0), "before": 0, "after": 0})
    ok(not err and text.startswith(short("%s/r1:%d-%d of its output so far (" % (pid, n0, n0))) and "; r1 is still running\n" in text and ("> %d %s" % (n0, l0.decode()) in text or "%d %s" % (n0, l0.decode()) in text), "lines reads the capture and says the run is still running:\n%s" % text)
    # count: without a text the run is named, not counted (its catalog comes once it is saved);
    # with a text its output so far is counted
    text, err = srv.call("count", {})
    ok(not err and short("not counted: %s/r1 (still running: counted once it is saved)" % pid) in text, "count without a text names the running run:\n%s" % text)
    text, err = srv.call("count", {"text": "needle"})
    ok(not err and short("%s/r1 is still running: counted its output so far (%d lines)" % (pid, lines_so_far)) in text and (("\n" + short("%s/r1 %d " % (pid, len(want)))) in "\n" + text or text.startswith('%d lines hold "needle" in r1 (' % len(want))), "count with a text counts the running run's output so far:\n%s" % text)
    run.stdin.write(b"go on\n")
    run.stdin.flush()
    run.communicate(timeout=60)
    r = subprocess.run([binary, "pack-pending", "--wait"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    ok(r.returncode == 0 and not os.path.exists(cap), "packed: %s" % r.stdout.decode().strip())
    same = True
    for n, l in want:
        t, e = srv.call("lines", {"ref": "%s/r1:%d" % (pid, n), "before": 0, "after": 0, "base64": True})
        b64 = [l.split("): ", 1)[1] for l in t.split("\n") if l.startswith("base64 of lines ")]
        got = base64.b64decode(b64[0]) if b64 else b""
        if e or got.rstrip(b"\n") != l:
            same = False
            print("reference %s/r1:%d: %r against %r" % (pid, n, got, l))
    ok(same, "after packing, each reference found while running names the same bytes (lines, base64)")
    text2, err2 = srv.call("search", {"text": "needle", "limit": 500, "max_tokens": 20000})
    found2 = sorted(set(n for _, n in refs(text2)))
    ok(not err2 and found2[:len(found)] == found and len(found2) > len(found), "a new search: the same references, then the later lines")
    ok("running" not in text2, "after packing, the answer no longer says the run is running")
    # a run whose `run` was killed with -9: its capture's lock is gone
    k = subprocess.Popen([binary, "run", "--", "sh", "-c", 'echo "pid $$"; echo "needle killed"; exec sleep 30'], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, env=dict(env, FEWER_TEST_NO_PACK="1"), cwd=proj, start_new_session=True)
    child = int(k.stdout.readline().split()[1])
    while k.stdout.readline() != b"needle killed\n":
        pass
    # (the run and its command are one process group: both go)
    os.killpg(k.pid, signal.SIGKILL)
    k.wait()
    k.stdout.close()
    k.stderr.close()
    end = time.time() + 5
    while time.time() < end:
        try:
            os.kill(child, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    text, err = srv.call("search", {"text": "needle killed"})
    print(text)
    ok(not err and short("%s/r2 ended without a status (its `run` was stopped; the next `fewer pack-pending` packs it): searched its output (2 lines)" % pid) in text and short("%s/r2:2 needle killed" % pid) in text, "search names a run whose writer was killed as ended without a status")
    text, err = srv.call("logs", {})
    ok(not err and "bytes of output, ended without a status; the next `fewer pack-pending` packs it" in text, "logs names the killed run as ended without a status:\n%s" % text)
    r = subprocess.run([binary, "pack-pending", "--wait"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    text, err = srv.call("logs", {})
    ok(r.returncode == 0 and not err and "ready (ended without a status)" in text, "packed later with its note:\n%s" % text)
    srv.close()
    print("all passed" if not FAILS else "%d FAILED" % len(FAILS))
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
