"""mcpc.py: a small MCP client for the checks (stdin/stdout, one JSON message per line).

Loaded by path (python3 -I does not put the script's folder on the import path):

    spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
"""
import json, os, re, shutil, subprocess

# the mark a check leaves in its scratch folder
MARK = ".fewer-check-scratch"


def fresh_work(work):
    """Make `work` a new, empty scratch folder for a check, with the mark file in it. A
    folder that is already there is deleted only when it holds the mark (a check made it):
    a wrong argument, such as the read-only data folder or a home folder, is never deleted."""
    if os.path.lexists(work):
        if not os.path.isfile(os.path.join(work, MARK)):
            raise SystemExit("%s is already there and no check made it (it has no %s): give a new scratch folder" % (work, MARK))
        shutil.rmtree(work)
    os.makedirs(work)
    open(os.path.join(work, MARK), "w").close()


class Server:
    def __init__(self, binary, env, cwd, stderr_path):
        self.err = open(stderr_path, "ab")
        self.p = subprocess.Popen([binary, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.err, env=env, cwd=cwd)
        self.next_id = 1000
        self.lines = []  # every stdout line read, raw

    def send_raw(self, b):
        self.p.stdin.write(b)
        self.p.stdin.flush()

    def send(self, obj):
        self.send_raw(json.dumps(obj, ensure_ascii=False).encode() + b"\n")

    def recv(self):
        line = self.p.stdout.readline()
        if not line:
            raise EOFError("the server closed its stdout")
        self.lines.append(line)
        return json.loads(line)

    def request(self, method, params=None):
        self.next_id += 1
        m = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            m["params"] = params
        self.send(m)
        r = self.recv()
        assert r.get("id") == self.next_id, ("answer to another request", r)
        return r

    def call(self, name, args):
        """(text, is_error) of a tool call; raises on a JSON-RPC error."""
        r = self.request("tools/call", {"name": name, "arguments": args})
        if "error" in r:
            raise RuntimeError("tools/call %s %s: %s" % (name, json.dumps(args), r["error"]))
        res = r["result"]
        return res["content"][0]["text"], res["isError"]

    def initialize(self, version="2025-06-18"):
        r = self.request("initialize", {"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "checks", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return r

    def close(self, timeout=60):
        self.p.stdin.close()
        rest = self.p.stdout.read()
        if rest:
            self.lines += rest.splitlines(keepends=True)
        code = self.p.wait(timeout=timeout)
        self.err.close()
        return code


COMPACT_LINE = re.compile(r"^((?:\S+/)?r\d+:\d+ |> \d+ |  \d+ )(.*)$")


def expand(text):
    """An answer's compact lines made whole again, as its legend tells the reader (tools.rs,
    compact_line, the token test of 2026-11-23): "≈ N" and words, "·" for line N's word in
    that place, the words at the end that line N has too left out; "= N" for line N again. N is
    a line shown before it in the same run of the answer (since the last "== " heading). A line
    that names no line shown before is left as it is (a log line that looks like one)."""
    out, seen = [], {}
    for l in text.split("\n"):
        if l.startswith("== "):
            seen = {}
        m = COMPACT_LINE.match(l)
        if m:
            num = int(re.search(r"(\d+) $", m.group(1)).group(1))
            body = m.group(2)
            e = re.match(r"^= (\d+)$", body)
            c = re.match(r"^≈ (\d+) (.*)$", body)
            if e and int(e.group(1)) in seen:
                body = seen[int(e.group(1))]
            elif c and int(c.group(1)) in seen:
                words, pw = c.group(2).split(" "), seen[int(c.group(1))].split(" ")
                if len(words) <= len(pw):
                    words += ["·"] * (len(pw) - len(words))
                    body = " ".join(p if w == "·" else w for p, w in zip(pw, words))
            seen[num] = body
            l = m.group(1) + body
        out.append(l)
    return "\n".join(out)


def shown(line, line_max=400):
    """How the server shows a line (clean.rs and answer.rs's shown_line): the line end dropped,
    escape sequences set aside, a redraw at its final state, control bytes but tab and bytes
    that are not UTF-8 as \\xHH, cut at line_max bytes with "…(+N bytes)"."""
    b = line[:-1] if line.endswith(b"\n") else line
    b = b[:-1] if b.endswith(b"\r") else b
    out, i = bytearray(), 0
    while i < len(b):
        c = b[i]
        if c != 0x1B:
            out.append(c)
            i += 1
            continue
        i += 1
        if i >= len(b):
            break
        if b[i] == ord("["):
            i += 1
            while i < len(b) and not (0x40 <= b[i] <= 0x7E):
                i += 1
            i += 1
        elif b[i] == ord("]"):
            i += 1
            while i < len(b):
                if b[i] == 7:
                    i += 1
                    break
                if b[i] == 0x1B and i + 1 < len(b) and b[i + 1] == ord("\\"):
                    i += 2
                    break
                i += 1
        else:
            while i < len(b) and 0x20 <= b[i] <= 0x2F:
                i += 1
            i += 1
    b = bytes(out)
    if b"\r" in b:
        pieces = [p for p in b.split(b"\r") if p]
        b = pieces[-1] if pieces else b""
    s = []
    k = 0
    while k < len(b):
        # decode the longest valid UTF-8 run, escape the rest byte by byte
        for n in (4, 3, 2, 1):
            try:
                ch = b[k:k + n].decode("utf-8")
            except UnicodeDecodeError:
                continue
            if len(ch) == 1:
                break
        else:
            ch = None
        if ch is None or (n == 1 and b[k] >= 0x80):
            s.append("\\x%02x" % b[k])
            k += 1
            continue
        o = ord(ch)
        if (o < 0x20 and ch != "\t") or o == 0x7F:
            s.append("\\x%02x" % o)
        else:
            s.append(ch)
        k += n
    t = "".join(s)
    tb = t.encode()
    if len(tb) <= line_max:
        return t
    k = line_max
    while k > 0 and (tb[k] & 0xC0) == 0x80:
        k -= 1
    return tb[:k].decode() + "…(+{:,} bytes)".format(len(tb) - k)
