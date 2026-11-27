"""e2e_grep.py SEARCH PACK WORK DATA: one MCP session over a pipe against `fewer-pack grep`
(python3 -I).

Copies of logs in DATA (HDFS_2k.log, Zookeeper_2k.log, OpenSSH_2k.log, HPC.log, which has
several blocks) and two edge files (CRLF line ends, 00 bytes) are saved with `fewer
add`, which packs them with PACK. One server session gets initialize, initialized,
tools/list and search calls (following each answer's cursor to the end). For each text and
run, the lines the search returns must equal `PACK grep TEXT ARCHIVE` on the same archive,
byte for byte: their exact bytes come from `lines` with base64, and, where a log has no
bytes the answers clean (CR, escapes, control bytes), the shown text plus '\\n' too. The
counts in the answers equal grep's line counts.
"""
import base64, importlib.util, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)

SEARCH, PACK, WORK, DATA = (os.path.abspath(a) for a in sys.argv[1:5])
# (a folder that is there is cleared only when a check made it: mcpc.fresh_work)
mcpc.fresh_work(WORK)
# every process this check starts, the server too, gets a made-up HOME, agents' folders and
# claude, inside WORK (safe_env.py)
safe = safe_env.setup(WORK)
HOME, PROJ = WORK + "/home", WORK + "/proj"
os.makedirs(PROJ)
env = safe.env(FEWER_HOME=HOME, FEWER_PACK_BIN=PACK, FEWER_PROJECT=PROJ)
lines = [b"%d %s user%d from 10.0.%d.%d port %d ok" % (i, b"INFO" if i % 7 else b"WARN", i % 13, i % 256, i * 7 % 256, 40000 + i) for i in range(5000)]
open(WORK + "/crlf.log", "wb").write(b"\r\n".join(lines) + b"\r\n")
open(WORK + "/nul.log", "wb").write(b"\n".join(l + (b"\0x" if i % 5 == 0 else b"") for i, l in enumerate(lines)) + b"\n")
files = [os.path.join(DATA, f) for f in ("HDFS_2k.log", "Zookeeper_2k.log", "OpenSSH_2k.log", "HPC.log")] + [WORK + "/crlf.log", WORK + "/nul.log"]
# (--force: nul.log holds NUL bytes, which add saves only when asked)
out = subprocess.run([SEARCH, "add", "--force"] + files, env=env, cwd=PROJ, capture_output=True, check=True).stdout.decode()
runs = {}
for line in out.splitlines():
    m = re.match(r"saved (.*) as (\S+)/r(\d+): ", line)
    runs[int(m.group(3))] = m.group(1)
    PID = m.group(2)
pdir = os.path.join(HOME, "projects", PID)
archive = {r: os.path.join(pdir, "r%06d.cmf" % r) for r in runs}

texts = ["Exception", "PacketResponder 1", "Failed password for root", "session opened", "port 4000", "WARN user1 ", "State change", "Received block", "node-246", "blk_-1608999687919862906", "no_such_text_31337"]
s = mcpc.Server(SEARCH, env, PROJ, WORK + "/stderr.txt")
init = s.initialize("2025-06-18")
assert init["result"]["protocolVersion"] == "2025-06-18"
tl = s.request("tools/list")["result"]["tools"]
assert "search" in [t["name"] for t in tl]
fails, compared, exact_lines = 0, 0, 0
REF = re.compile(r"^(?:(\S+)/)?r(\d+):(\d+) (.*)$")
for t in texts:
    # every line the search gives, per run, following the cursors
    got_refs = {r: [] for r in runs}
    got_text = {r: [] for r in runs}
    args = {"text": t, "limit": 500, "max_tokens": 20000, "line_max": 65536, "collapse": False}
    head_total = None
    while True:
        text, err = s.call("search", args)
        # (similar lines come as "≈ N"/"= N" by default: made whole again here, so the shown
        # text below is checked against grep exactly)
        text = mcpc.expand(text)
        if head_total is None:
            # (after the "shown cleaned" note, when there is one)
            m = re.search(r"^([\d,]+) lines? holds? ", text, re.M)
            head_total = int(m.group(1).replace(",", "")) if m else 0
        for line in text.split("\n")[:-1]:
            m = REF.match(line)
            if m:
                got_refs[int(m.group(2))].append(int(m.group(3)))
                got_text[int(m.group(2))].append(m.group(4))
        last = text.split("\n")[-1]
        if last.startswith("all shown"):
            break
        args = json.loads(re.search(r"Next: search (\{.*\})$", last).group(1))
    total = 0
    for r in runs:
        g = subprocess.run([PACK, "grep", "--", t, archive[r]], capture_output=True)
        want = g.stdout
        total += want.count(b"\n")
        # the exact bytes of every returned line, by reference
        exact = b""
        refs = got_refs[r]
        for k in range(0, len(refs), 1):
            text2, err2 = s.call("lines", {"ref": "%s/r%d:%d" % (PID, r, refs[k]), "before": 0, "after": 0, "base64": True, "max_tokens": 20000, "line_max": 65536})
            m = re.search(r"^base64 of lines \d+-\d+ \([\d,]+ bytes, line ends included\): (\S*)$", text2, re.M)
            b = base64.b64decode(m.group(1))
            exact += b if b.endswith(b"\n") else b + b"\n"
            exact_lines += 1
        compared += 1
        if exact != want:
            fails += 1
            print("DIFFERENT exact bytes: %r in r%d (%s): %d lines, grep %d" % (t, r, os.path.basename(runs[r]), len(refs), want.count(b"\n")))
        raw = open(runs[r], "rb").read()
        if not re.search(rb"[\x00-\x08\x0b-\x1f\x7f]", raw):
            shown = "".join(x + "\n" for x in got_text[r]).encode()
            if shown != want:
                fails += 1
                print("DIFFERENT shown text: %r in r%d" % (t, r))
    if head_total != total:
        fails += 1
        print("DIFFERENT count for %r: the answer says %d, grep %d" % (t, head_total, total))
code = s.close()
print("e2e_grep: %d texts x %d runs compared with fewer-pack grep, %d lines checked byte for byte by reference, %d different, exit %d" % (len(texts), len(runs), exact_lines, fails, code))
if fails or code != 0:
    sys.exit(1)
print("e2e_grep: ALL IDENTICAL")
