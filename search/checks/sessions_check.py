"""Acceptance checks of agents' sessions saved into the memory
store, on made-up sessions only.

    python3 -I checks/sessions_check.py BIN PACK WORK

BIN is fewer; PACK the
fewer-pack binary that packs; WORK a scratch folder (made fresh;
an existing one is cleared only when it holds the mark file a check leaves). Every session
file, Cursor database, store, project and git repository is made up in WORK. The program's
environment is built from nothing, and safe_env.py checks every process start: HOME is a
folder in WORK, the agents' folders (FEWER_CLAUDE_DIR, FEWER_CODEX_DIR, FEWER_CLINE_DIR,
FEWER_CURSOR_DIR, and FEWER_AIDER_DIR: the project alpha's root) are in WORK, and Claude
Code's and Codex's own settings are not passed, so the real ~/.claude, ~/.codex, Cursor and
VS Code folders are never named, never read. No network; nothing is left running.

The checks:
- Claude sessions in two projects (one a git repository whose session started in a subfolder)
  are saved once, then only their new tails, as runs that continue the piece before; a line
  still being written waits; a file that changed before its end is saved whole again; an
  unchanged source is not saved again; a session's pieces, put together, are the session file
  with its secrets hidden; offset, size and hash are those of the file as read (our hash,
  computed again here); two syncs at once save a tail once;
- a secret inside a tool result (and in the tool's result copy) is hidden and counted;
- keys per message type and tool (with the real catalog: `BIN keys --cat`, against the rule
  of msgkey.rs written again here), and the files the session edited (Edit, Write,
  MultiEdit, NotebookEdit), each with its line (the .cat file read here; with the
  catalog stand-in, the files it was given);
- Codex, Aider and Cline fixtures, with their tails; Aider's history read only from the folder
  FEWER_AIDER_DIR names; Cline's task file read as one element of its array per
  line, the element's exact bytes (Python's own JSON decoder finds them); a task file cut
  short is not saved;
- Cursor through sqlite3 (/usr/bin/sqlite3, behind a recorder) on a made-up state.vscdb: its
  rows saved as sqlite3 prints them, secrets hidden; then only the new and changed rows; an
  unchanged database is not read again; reading never changes the database; sqlite3 missing
  or failing;
- with sessions off nothing is read: the agents' folders are made unreadable and the sqlite3
  recorder notes any call; nothing is listed, run or saved; agents turned off one by one too;
- the MCP server's sweep (`BIN sweep-serve`, and `BIN mcp` when BIN has it): at its start and
  again after the interval; nothing with sessions off;
- a run left "packing" by a stopped sweep is marked failed and its part saved again;
- the real home: no path of it outside WORK appears in any output, and HOME (in WORK) stays
  empty.
"""
import collections
import hashlib
import importlib.util
import json
import os
import pwd
import shutil
import subprocess
import sys
import threading
import time

MARK = ".laneb-check-scratch"
FAILS = []
PASSES = [0]
_spec = importlib.util.spec_from_file_location("safe_env", os.path.join(os.path.dirname(os.path.abspath(__file__)), "safe_env.py"))
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
        for root, dirs, _ in os.walk(path):
            for d in dirs:
                os.chmod(os.path.join(root, d), 0o755)
        shutil.rmtree(path)
    os.makedirs(path)
    open(os.path.join(path, MARK), "w").close()


# ---------------------------------------------------------------- our 128-bit hash (add.rs)

M64 = (1 << 64) - 1


def fmix(z):
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return z ^ (z >> 31)


def hash128(b):
    h1, h2 = 0x243F6A8885A308D3, 0x13198A2E03707344
    n = len(b)
    pad = b + b"\0" * ((-n) % 8)
    for i in range(0, len(pad), 8):
        w = int.from_bytes(pad[i:i + 8], "little")
        h1 = ((h1 ^ w) * 0x9E3779B97F4A7C15) & M64
        h1 ^= h1 >> 32
        r = ((w << 29) | (w >> 35)) & M64
        h2 = ((h2 ^ r) * 0xFF51AFD7ED558CCD) & M64
        h2 ^= h2 >> 29
    a = fmix(h1 ^ n)
    c = fmix(h2 ^ (((n << 32) | (n >> 32)) & M64) ^ a)
    return "%016x%016x" % (a, c)


# ---------------------------------------------------------------- made-up secrets


def fake(prefix, body, n):
    """A made-up secret: the prefix, then `body` repeated to n bytes (built at run time, so no
    secret-like text sits in this file)."""
    return prefix + (body * n)[:n]


AWS = fake("AKIA", "QZ7X", 16)
GH = fake("ghp_", "aB3d", 36)
SLACK = fake("xoxb-", "12ab-", 24)
OPENAI = fake("sk-proj-", "Ab1_", 40)
SECRETS = [AWS, GH, SLACK, OPENAI, "hunter2x-not-real", fake("acme_", "Zq9", 24)]

# ---------------------------------------------------------------- the store


def runs(home):
    """Every run of the store, merged by id: {(pid, id): record}."""
    out = {}
    pdir = os.path.join(home, "projects")
    for pid in sorted(os.listdir(pdir)) if os.path.isdir(pdir) else []:
        path = os.path.join(pdir, pid, "runs.jsonl")
        if not os.path.exists(path):
            continue
        for line in open(path, "rb").read().split(b"\n"):
            if line.strip():
                rec = json.loads(line)
                out.setdefault((pid, rec["id"]), {}).update(rec)
    return out


def pieces(home, source):
    """The ready runs of one source, oldest first: [(pid, id, record)]."""
    v = [(k[0], k[1], r) for k, r in runs(home).items() if r.get("source") == source and r.get("state") == "ready"]
    return sorted(v, key=lambda x: x[1])


def project_root(home, pid):
    for line in open(os.path.join(home, "projects", pid, "project")).read().split("\n"):
        if line.startswith("root = "):
            return line[7:]
    return None


def archive(home, pid, rid):
    return os.path.join(home, "projects", pid, "r%06d.cmf" % rid)


def tree(path):
    """Every file under a folder: {relative path: (size, sha256)}."""
    out = {}
    for root, _, files in os.walk(path):
        for f in files:
            p = os.path.join(root, f)
            b = open(p, "rb").read()
            out[os.path.relpath(p, path)] = (len(b), hashlib.sha256(b).hexdigest())
    return out


def varint(d, p):
    v, s = 0, 0
    while True:
        x = d[p]
        p += 1
        v |= (x & 0x7F) << s
        if x < 0x80:
            return v, p
        s += 7


def cat_files(path):
    """The edited files of a .cat file (catalog.rs's encode, formats 01 and 02:
    02 adds `before` to each moment): [[path, line]]."""
    d = open(path, "rb").read()
    version = d[4] if d[:4] == b"CMLC" else None
    if version not in (1, 2):
        raise ValueError("%s: a catalog format this check does not know (%r): update cat_files" % (path, d[:5]))
    p = 5
    for _ in range(2):  # rule, archive bytes
        _, p = varint(d, p)
    p += 4  # end crc
    for _ in range(5):  # lines, records, bytes, first-ms, last-ms
        _, p = varint(d, p)
    n, p = varint(d, p)
    p += n  # format
    _, p = varint(d, p)  # zone
    nm, p = varint(d, p)
    for _ in range(2 * nm):
        _, p = varint(d, p)
    nk, p = varint(d, p)
    for _ in range(nk):
        n, p = varint(d, p)
        p += n
        _, p = varint(d, p)  # level
        krec, p = varint(d, p)
        for _ in range(5):  # lines, first line, last - first, first minute, last minute
            _, p = varint(d, p)
        nh, p = varint(d, p)
        for _ in range(2 * nh):
            _, p = varint(d, p)
        for _ in range(krec if krec <= 16 else 12):
            _, p = varint(d, p)
        for _ in range(2):
            _, p = varint(d, p)
    nmo, p = varint(d, p)
    for _ in range(nmo):
        p += 1
        for _ in range(4 if version == 1 else 5):
            _, p = varint(d, p)
    ne, p = varint(d, p)
    for _ in range(2 * ne):
        _, p = varint(d, p)
    nf, p = varint(d, p)
    files = []
    for _ in range(nf):
        n, p = varint(d, p)
        f = d[p:p + n].decode()
        p += n
        line, p = varint(d, p)
        files.append([f, line])
    assert p == len(d) - 4, "the .cat file has bytes after its files"
    return files


# ---------------------------------------------------------------- the session key


def session_key(o):
    """The key of a session's JSON line, as msgkey.rs's rule v1 makes it:
    `{session ...}` for Claude Code's and Codex's lines; the field names for an object with no
    message field (the only other lines here)."""
    if not ("sessionId" in o and "type" in o) and not (("payload" in o and "type" in o and "timestamp" in o) or "record_type" in o):
        assert not any(k in o for k in ("msg", "message", "log", "MESSAGE", "eventMessage", "event", "text", "error", "err"))
        return "{" + ",".join(o.keys()) + "}"
    parts = []

    def put(v):
        if isinstance(v, str):
            parts.append(v)

    if "sessionId" in o and "type" in o:
        put(o.get("type"))
        m = o.get("message")
        put(m.get("role") if isinstance(m, dict) else None)
        if isinstance(m, dict) and isinstance(m.get("content"), list):
            blocks = m["content"]
            put(blocks[0].get("type") if blocks and isinstance(blocks[0], dict) else None)
            put(next((b["name"] for b in blocks if isinstance(b, dict) and isinstance(b.get("name"), str)), None))
    else:
        put(o.get("type"))
        body = o.get("payload") if "payload" in o else o
        if "payload" in o:
            put(body.get("type"))
        put(body.get("role"))
        if isinstance(body.get("content"), list):
            c = body["content"]
            put(c[0].get("type") if c and isinstance(c[0], dict) else None)
        put(body.get("name"))
    return "{session" + "".join(" " + p for p in parts) + "}"


EDITS = {"Edit": "file_path", "MultiEdit": "file_path", "Write": "file_path", "NotebookEdit": "notebook_path"}


def edits_of(lines):
    """The files a piece's lines edit: [[path, line in the piece]]."""
    out = []
    for n, l in enumerate(lines, 1):
        try:
            o = json.loads(l)
        except ValueError:
            continue
        m = o.get("message") if isinstance(o, dict) else None
        c = m.get("content") if isinstance(m, dict) else None
        for b in c if isinstance(c, list) else []:
            if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("name") in EDITS:
                path = (b.get("input") or {}).get(EDITS[b["name"]])
                if isinstance(path, str):
                    out.append([path, n])
    return out


# ---------------------------------------------------------------- a case


class Ctx:
    def __init__(self, binary, fewer, work):
        self.bin, self.fewer, self.work = binary, fewer, work
        self.n = 0
        self.real_home = pwd.getpwuid(os.getuid()).pw_dir
        h = subprocess.run([binary, "--help"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env={"PATH": "/usr/bin:/bin"}).stdout
        self.has_mcp = b"fewer mcp" in h
        self.real_catalog = None

    def case(self, name):
        self.n += 1
        return Case(self, os.path.join(self.work, "%02d-%s" % (self.n, name)))

    def hide(self, data, extras=()):
        """`BIN hide`: secrets::hide over the bytes, as one text, with the extra shapes given
        (the config's `secret = PREFIX CLASS MIN` values)."""
        p = subprocess.run([self.bin, "hide"] + list(extras), input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env={"PATH": "/usr/bin:/bin"}, timeout=60)
        assert p.returncode == 0, p.stderr
        return p.stdout

    def decode(self, path):
        p = subprocess.run([self.fewer, "-d", "-c", path], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        return p.stdout if p.returncode == 0 else None


class Case:
    def __init__(self, ctx, base):
        self.ctx, self.base = ctx, base
        self.home = os.path.join(base, "home")
        self.fakehome = os.path.join(base, "fakehome")
        self.agents = os.path.join(base, "agents")
        self.claude = os.path.join(self.agents, "claude")
        self.codex = os.path.join(self.agents, "codex")
        self.cline = os.path.join(self.agents, "cline")
        self.cursor = os.path.join(self.agents, "cursor")
        self.proj = os.path.join(base, "proj")
        self.alpha = os.path.join(self.proj, "alpha")
        self.beta = os.path.join(self.proj, "beta")
        self.catlog = os.path.join(base, "catalog.log")
        self.bin_dir = os.path.join(base, "bin")
        self.calls = os.path.join(base, "sqlite3-calls")
        self.outputs = []
        self.extras = []
        for d in (self.home, self.fakehome, self.agents, self.alpha, self.beta, self.bin_dir):
            os.makedirs(d)
        # a recorder in place of sqlite3: notes each call, then runs the system's
        rec = os.path.join(self.bin_dir, "sqlite3")
        with open(rec, "w") as f:
            f.write('#!/bin/sh\nfor a in "$@"; do printf "%%s\\t" "$a" >> "%s"; done\necho >> "%s"\nexec /usr/bin/sqlite3 "$@"\n' % (self.calls, self.calls))
        os.chmod(rec, 0o755)

    def env(self, **extra):
        e = {
            "PATH": self.bin_dir + ":/usr/bin:/bin",
            "HOME": self.fakehome,
            "LANG": "en_US.UTF-8",
            "FEWER_HOME": self.home,
            "FEWER_PACK_BIN": self.ctx.fewer,
            "FEWER_CLAUDE_DIR": self.claude,
            "FEWER_CODEX_DIR": self.codex,
            "FEWER_AIDER_DIR": self.alpha,
            "FEWER_CLINE_DIR": self.cline,
            "FEWER_CURSOR_DIR": self.cursor,
            "FEWER_TEST_CATALOG_LOG": self.catlog,
            "GIT_CEILING_DIRECTORIES": self.base,
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        e.update(extra)
        # (a setting removed on purpose stays removed: safe_env.py)
        return safe_env.without({k: v for k, v in e.items() if v is not None}, *[k for k, v in extra.items() if v is None])

    def config(self, text):
        with open(os.path.join(self.home, "config"), "w") as f:
            f.write(text)
        # the extra secret shapes the program reads from it
        self.extras = [l.split("=", 1)[1].strip() for l in text.split("\n") if l.split("=", 1)[0].strip() == "secret"]

    def sync(self, cwd=None, **extra):
        p = subprocess.run([self.ctx.bin, "sessions", "sync"], cwd=cwd or self.alpha, env=self.env(**extra), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=300)
        out, err = p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")
        self.outputs.append(out + err)
        return p.returncode, out, err

    def sqlite_calls(self):
        if not os.path.exists(self.calls):
            return []
        return [l.rstrip("\t").split("\t") for l in open(self.calls).read().split("\n") if l]

    def catalog_files(self, pid, rid):
        """The files a run's catalog holds: the .cat file's (real catalog), else what the
        stand-in was given."""
        cat = os.path.join(self.home, "projects", pid, "r%06d.cat" % rid)
        if os.path.exists(cat):
            if self.ctx.real_catalog is None:
                self.ctx.real_catalog = True
            return cat_files(cat)
        self.ctx.real_catalog = False
        for line in open(self.catlog).read().split("\n") if os.path.exists(self.catlog) else []:
            if line:
                rec = json.loads(line)
                if rec["pid"] == pid and rec["id"] == rid:
                    return rec.get("files")
        return None

    def keys(self, pid, rid):
        """{key: records} of a run's catalog (`BIN keys --cat`), or None without a real catalog."""
        cat = os.path.join(self.home, "projects", pid, "r%06d.cat" % rid)
        if not os.path.exists(cat):
            return None
        p = subprocess.run([self.ctx.bin, "keys", "--cat", cat], stdout=subprocess.PIPE, stderr=subprocess.PIPE, env={"PATH": "/usr/bin:/bin"}, timeout=60)
        if p.returncode != 0:
            return {"error": p.stderr.decode()}
        out = {}
        for line in p.stdout.decode().split("\n"):
            if line:
                n, k = line.split("\t", 1)
                out[k] = out.get(k, 0) + int(n)
        return out

    def check_home_untouched(self, label):
        ok(os.listdir(self.fakehome) == [], "%s: HOME (a folder in WORK) stays empty" % label)
        real = self.ctx.real_home.rstrip("/") + "/"
        for o in self.outputs:
            ok(real not in o.replace(self.ctx.work, ""), "%s: no path of the real home outside WORK in the output: %r" % (label, o[:300]))


def lines_of(b):
    """The lines of bytes, each with its '\\n' (the last may have none)."""
    v = [x + b"\n" for x in b.split(b"\n")]
    v[-1] = v[-1][:-1]
    return v if v[-1] else v[:-1]


def write(path, data, mode="wb"):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, mode) as f:
        f.write(data)


def line(o):
    return (json.dumps(o, separators=(",", ":"), ensure_ascii=False) + "\n").encode()


def claude_line(cwd, sid, typ, msg, n, extra=None):
    o = {"parentUuid": None, "isSidechain": False, "userType": "external", "cwd": cwd, "sessionId": sid, "version": "2.0.0", "gitBranch": "main", "type": typ}
    if msg is not None:
        o["message"] = msg
    o["uuid"] = "00000000-0000-4000-8000-%012d" % n
    o["timestamp"] = "2026-11-18T10:%02d:%02d.000Z" % (n // 60 % 60, n % 60)
    if extra:
        o.update(extra)
    return line(o)


def tool_use(name, inp, i):
    return {"type": "tool_use", "id": "toolu_%02d" % i, "name": name, "input": inp}


SID_A = "11111111-2222-4333-8444-555555555555"
SID_B = "22222222-3333-4444-8555-666666666666"
SID_C = "33333333-4444-4555-8666-777777777777"


def session_a(cwd):
    """A Claude Code session: a summary, a question, a test run whose result holds secrets (also
    in the tool's result copy), edits through every editing tool, a system line."""
    leak = "export AWS_ACCESS_KEY_ID=%s\nGITHUB_TOKEN=%s\nDB_PASSWORD=%s\ntest result: FAILED" % (AWS, GH, SECRETS[4])
    return [
        line({"type": "summary", "summary": "Fix the flaky test", "leafUuid": "00000000-0000-4000-8000-000000000009"}),
        claude_line(cwd, SID_A, "user", {"role": "user", "content": "why does the test fail?"}, 1),
        claude_line(cwd, SID_A, "assistant", {"role": "assistant", "content": [{"type": "text", "text": "Let me run it."}, tool_use("Bash", {"command": "cargo test"}, 1)]}, 2),
        claude_line(cwd, SID_A, "user", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_01", "content": leak, "is_error": True}]}, 3, {"toolUseResult": {"stdout": leak, "stderr": "", "interrupted": False}}),
        claude_line(cwd, SID_A, "assistant", {"role": "assistant", "content": [tool_use("Edit", {"file_path": cwd + "/src/lib.rs", "old_string": "a", "new_string": "b"}, 2)]}, 4),
        claude_line(cwd, SID_A, "user", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_02", "content": "The file has been updated."}]}, 5),
        claude_line(cwd, SID_A, "assistant", {"role": "assistant", "content": [{"type": "text", "text": "Now the notebook and two files."}, tool_use("NotebookEdit", {"notebook_path": cwd + "/nb/a.ipynb", "new_source": "x"}, 3), tool_use("Write", {"file_path": cwd + "/notes.md", "content": "n"}, 4), tool_use("MultiEdit", {"file_path": cwd + "/src/main.rs", "edits": [{"old_string": "c", "new_string": "d"}]}, 5)]}, 6),
        claude_line(cwd, SID_A, "system", None, 7, {"subtype": "informational", "content": "Compacted", "level": "info"}),
    ]


def session_a_more(cwd):
    return [
        claude_line(cwd, SID_A, "assistant", {"role": "assistant", "content": [tool_use("Edit", {"file_path": cwd + "/src/more.rs", "old_string": "e", "new_string": "f"}, 6)]}, 8),
        claude_line(cwd, SID_A, "user", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_06", "content": "posted with SLACK_TOKEN=%s" % SLACK}]}, 9),
    ]


def short_session(cwd, sid, n0, edit):
    return [
        claude_line(cwd, sid, "user", {"role": "user", "content": "add a flag"}, n0),
        claude_line(cwd, sid, "assistant", {"role": "assistant", "content": [{"type": "text", "text": "Done."}, tool_use("Write", {"file_path": edit, "content": "flag"}, n0)]}, n0 + 1),
        claude_line(cwd, sid, "user", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_%02d" % n0, "content": "ok"}]}, n0 + 2),
    ]


def git(repo, *args):
    env = {"PATH": "/usr/bin:/bin", "HOME": repo, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
    subprocess.run(["git", "-C", repo] + list(args), env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)


def check_piece(case, pid, rid, rec, source_bytes, offset, piece_lines, label, agent, session=None, files=True):
    """A saved piece against the source as read: its fields, its archive's text (the lines with
    secrets hidden, as one pass over the source hides them) and its .idx."""
    text = b"".join(piece_lines)
    upto = offset + len(text)
    # hidden as one pass over the whole source up to here hides them
    whole = case.ctx.hide(source_bytes[:upto], case.extras)
    k0 = source_bytes[:offset].count(b"\n")
    want = b"".join(lines_of(whole)[k0:])
    ok(rec.get("kind") == "session" and rec.get("agent") == agent and rec.get("state") == "ready", "%s: a ready session run of agent %s (%s)" % (label, agent, {k: rec.get(k) for k in ("kind", "agent", "state", "note")}))
    if session is not None:
        ok(rec.get("session") == session, "%s: session id %s (%s)" % (label, session, rec.get("session")))
    ok(rec.get("offset") == offset and rec.get("size") == len(text), "%s: offset %d and size %d (%s, %s)" % (label, offset, len(text), rec.get("offset"), rec.get("size")))
    ok(rec.get("hash") == hash128(source_bytes[:upto]), "%s: hash of the source as read up to the piece's end" % label)
    got = case.ctx.decode(archive(case.home, pid, rid))
    ok(got == want, "%s: the archive holds the piece's lines with secrets hidden (%d bytes, want %d)" % (label, len(got or b""), len(want)))
    ok(rec.get("bytes") == len(want) and rec.get("lines") == len(piece_lines), "%s: bytes and lines recorded (%s %s)" % (label, rec.get("bytes"), rec.get("lines")))
    markers = want.count(b"[hidden: ")
    ok(rec.get("hidden") == markers, "%s: hidden counts the %d values hidden (%s)" % (label, markers, rec.get("hidden")))
    for s in SECRETS:
        ok(s.encode() not in (got or b""), "%s: secret %s... not in the archive" % (label, s[:6]))
    ok(rec.get("archive") == os.path.getsize(archive(case.home, pid, rid)), "%s: archive size recorded" % label)
    # the .idx: the same bytes as `fewer-pack index`
    tmp = os.path.join(case.base, "idx-check.cmf")
    shutil.copyfile(archive(case.home, pid, rid), tmp)
    subprocess.run([case.ctx.fewer, "index", tmp], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
    ok(open(tmp + ".idx", "rb").read() == open(archive(case.home, pid, rid) + ".idx", "rb").read(), "%s: the .idx equals fewer-pack index's" % label)
    os.remove(tmp)
    os.remove(tmp + ".idx")
    # the files a Claude session edited, with their lines; none for the other agents
    want_files = edits_of([l.decode() for l in lines_of(want)]) if files else []
    ok(case.catalog_files(pid, rid) == want_files, "%s: the catalog lists the files edited, with their lines: %s (want %s)" % (label, case.catalog_files(pid, rid), want_files))
    return want


def check_keys(case, pid, rid, hidden_text, label):
    keys = case.keys(pid, rid)
    if keys is None:
        return
    want = collections.Counter(session_key(json.loads(l)) for l in lines_of(hidden_text))
    ok(keys == dict(want), "%s: keys per message type and tool: %s (want %s)" % (label, keys, dict(want)))


# ---------------------------------------------------------------- the checks


def check_claude(ctx):
    c = ctx.case("claude")
    c.config("# saving\nsessions = on\n")
    os.makedirs(os.path.join(c.beta, "src"))
    git(c.beta, "init", "-q")
    a = session_a(c.alpha)
    b = short_session(c.alpha, SID_B, 20, c.alpha + "/flag.rs")
    cc = short_session(c.beta + "/src", SID_C, 30, c.beta + "/src/x.rs")
    fa = os.path.join(c.claude, "projects", "-w-alpha", SID_A + ".jsonl")
    fb = os.path.join(c.claude, "projects", "-w-alpha", SID_B + ".jsonl")
    fc = os.path.join(c.claude, "projects", "-w-beta-src", SID_C + ".jsonl")
    write(fa, b"".join(a))
    write(fb, b"".join(b))
    write(fc, b"".join(cc))
    # a link is passed over; a helper's transcript under the session's own folder is saved as a
    # piece of that session, a workflow's journal next to it is not
    os.symlink(fb, os.path.join(c.claude, "projects", "-w-alpha", "link.jsonl"))
    fh = os.path.join(c.claude, "projects", "-w-alpha", SID_A, "subagents", "agent-1.jsonl")
    write(fh, b"".join(b))
    write(os.path.join(c.claude, "projects", "-w-alpha", SID_A, "workflows", "r1", "journal.jsonl"), b'{"agent": 1}\n')
    sums = {f: hashlib.sha256(open(f, "rb").read()).hexdigest() for f in (fa, fb, fc)}
    code, out, err = c.sync()
    ok(code == 0, "claude: sync exits 0 (%d: %s)" % (code, err))
    ok("claude: 1 link passed over (only files are read)" in out, "claude: the link is named as passed over (%s)" % out)
    rs = runs(c.home)
    ok(len(rs) == 4, "claude: three sessions and a helper's transcript, four runs (%d)" % len(rs))
    ph = pieces(c.home, fh)
    ok(len(ph) == 1 and ph[0][2].get("session") == SID_A, "claude: the helper's transcript is a piece of session A (%r)" % [(x[0], x[2].get("session")) for x in ph])
    pa, pb, pc = pieces(c.home, fa), pieces(c.home, fb), pieces(c.home, fc)
    ok(len(pa) == len(pb) == len(pc) == 1, "claude: one piece each")
    pid_a, pid_c = pa[0][0], pc[0][0]
    ok(pb[0][0] == pid_a and pid_c != pid_a, "claude: two projects (%s, %s)" % (pid_a, pid_c))
    ok(project_root(c.home, pid_a) == c.alpha, "claude: alpha's project root is its cwd (%s)" % project_root(c.home, pid_a))
    ok(project_root(c.home, pid_c) == c.beta, "claude: a session started in beta/src is in beta's project (its git root: %s)" % project_root(c.home, pid_c))
    src_a = open(fa, "rb").read()
    want_a = check_piece(c, pid_a, pa[0][1], pa[0][2], src_a, 0, a, "claude A", "claude", SID_A)
    check_keys(c, pid_a, pa[0][1], want_a, "claude A")
    ok(pa[0][2].get("hidden", 0) >= 6, "claude A: the tool result's secrets are hidden twice (result and copy): %s" % pa[0][2].get("hidden"))
    for s in (b"[hidden: AWS key]", b"[hidden: GitHub token]", b"[hidden: password]"):
        ok(s in want_a, "claude A: marker %s" % s.decode())
    check_piece(c, pid_a, pb[0][1], pb[0][2], open(fb, "rb").read(), 0, b, "claude B", "claude", SID_B)
    want_c = check_piece(c, pid_c, pc[0][1], pc[0][2], open(fc, "rb").read(), 0, cc, "claude C", "claude", SID_C)
    check_keys(c, pid_c, pc[0][1], want_c, "claude C")
    ok(sorted(r.get("source") for r in rs.values()) == sorted([fa, fb, fc, fh]), "claude: the link and the workflow's journal are not saved")
    # unchanged: nothing saved, runs.jsonl as it was
    before = tree(c.home)
    code, out, _ = c.sync()
    ok(code == 0 and "0 pieces saved" in out and tree(c.home) == before, "claude: an unchanged sync saves nothing and changes nothing")
    # grown, with a line still being written
    more = session_a_more(c.alpha)
    part = claude_line(c.alpha, SID_A, "user", {"role": "user", "content": "thanks"}, 10)
    write(fa, b"".join(more) + part[:25], "ab")
    code, out, _ = c.sync()
    pa = pieces(c.home, fa)
    ok(code == 0 and len(pa) == 2, "claude: the grown session gives one more piece (%d)" % len(pa))
    ok(pa[1][2].get("continues") == pa[0][1], "claude: the tail continues the first piece")
    src_a = open(fa, "rb").read()
    tail_want = check_piece(c, pid_a, pa[1][1], pa[1][2], src_a, len(b"".join(a)), more, "claude A tail", "claude", SID_A)
    ok(b"[hidden: Slack token]" in tail_want, "claude A tail: the Slack token is hidden")
    ok(len(pieces(c.home, fb)) == 1 and len(pieces(c.home, fc)) == 1, "claude: the other sessions are not saved again")
    # the line being written is saved when it ends
    write(fa, part[25:], "ab")
    c.sync()
    pa = pieces(c.home, fa)
    ok(len(pa) == 3 and pa[2][2].get("continues") == pa[1][1], "claude: the finished line is saved next, continuing the tail")
    src_a = open(fa, "rb").read()
    check_piece(c, pid_a, pa[2][1], pa[2][2], src_a, len(b"".join(a + more)), [part], "claude A last line", "claude", SID_A)
    # the pieces put together are the session with its secrets hidden
    joined = b"".join(ctx.decode(archive(c.home, p[0], p[1])) for p in pa)
    ok(joined == ctx.hide(src_a), "claude: the pieces put together equal the whole session hidden in one pass")
    # two syncs at once save a new tail once
    write(fa, claude_line(c.alpha, SID_A, "user", {"role": "user", "content": "once"}, 11), "ab")
    ts = [threading.Thread(target=c.sync) for _ in range(2)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    ok(len(pieces(c.home, fa)) == 4, "claude: two syncs at once save the new line once (%d pieces)" % len(pieces(c.home, fa)))
    # touched only: read, nothing new, nothing saved
    os.utime(fa, None)
    before = len(runs(c.home))
    c.sync()
    ok(len(runs(c.home)) == before, "claude: a session only touched saves nothing")
    # changed before its end: saved whole again, in the same project
    write(fa, b"".join(a[:3]))
    c.sync()
    pa = pieces(c.home, fa)
    ok(len(pa) == 5 and pa[4][0] == pid_a and "continues" not in pa[4][2], "claude: a session that changed before its end is saved whole again, same project")
    check_piece(c, pid_a, pa[4][1], pa[4][2], open(fa, "rb").read(), 0, a[:3], "claude A again", "claude", SID_A)
    # the agent's files are never changed
    ok(all(hashlib.sha256(open(f, "rb").read()).hexdigest() == sums[f] for f in (fb, fc)), "claude: the session files are not changed by saving")
    c.check_home_untouched("claude")


def check_stopped_sweep(ctx):
    c = ctx.case("stopped")
    c.config("sessions = on\n")
    a = session_a(c.alpha)
    fa = os.path.join(c.claude, "projects", "-w-alpha", SID_A + ".jsonl")
    write(fa, b"".join(a))
    c.sync()
    (pid, rid, rec), = pieces(c.home, fa)
    # a sweep killed while it saved the next piece left this behind
    dead = rid + 1
    rj = os.path.join(c.home, "projects", pid, "runs.jsonl")
    with open(rj, "a") as f:
        f.write(json.dumps({"id": dead, "kind": "session", "state": "packing", "start": rec["start"] + 1, "agent": "claude", "source": fa, "offset": rec["size"], "continues": rid}) + "\n")
    write(archive(c.home, pid, dead), b"half an archive")
    more = session_a_more(c.alpha)
    write(fa, b"".join(more), "ab")
    code, out, err = c.sync()
    rs = runs(c.home)
    ok(rs[(pid, dead)].get("state") == "failed" and "stopped before it was done" in rs[(pid, dead)].get("note", ""), "stopped: the run left packing is marked failed with its note (%s)" % rs[(pid, dead)])
    ok(not os.path.exists(archive(c.home, pid, dead)), "stopped: its half archive is removed")
    pa = pieces(c.home, fa)
    ok(len(pa) == 2 and pa[1][2].get("continues") == rid and pa[1][1] > dead, "stopped: its part is saved again, continuing the last ready piece")
    check_piece(c, pid, pa[1][1], pa[1][2], open(fa, "rb").read(), len(b"".join(a)), more, "stopped tail", "claude", SID_A)
    c.check_home_untouched("stopped")


def codex_lines(cwd, sid):
    return [
        line({"timestamp": "2026-11-18T10:15:02.000Z", "type": "session_meta", "payload": {"id": sid, "timestamp": "2026-11-18T10:15:02.000Z", "cwd": cwd, "originator": "codex_cli_rs", "cli_version": "0.50.0"}}),
        line({"timestamp": "2026-11-18T10:15:03.000Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run the tests with %s" % ACME}]}}),
        line({"timestamp": "2026-11-18T10:15:04.000Z", "type": "turn_context", "payload": {"cwd": cwd, "approval_policy": "on-request", "model": "gpt-5-codex"}}),
        line({"timestamp": "2026-11-18T10:15:05.000Z", "type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"command\":[\"cargo\",\"test\"]}", "call_id": "call_1"}}),
        line({"timestamp": "2026-11-18T10:15:09.000Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "call_1", "output": "OPENAI_API_KEY=%s test result: FAILED" % OPENAI}}),
        line({"timestamp": "2026-11-18T10:15:09.500Z", "type": "event_msg", "payload": {"type": "token_count", "info": None}}),
    ]


ACME = fake("acme_", "Zq9", 24)


def check_codex(ctx):
    c = ctx.case("codex")
    # (an extra secret shape from the config applies to sessions too)
    c.config("sessions = on\nsecret = acme_ alnum 20\n")
    sid = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    f = os.path.join(c.codex, "sessions", "2026", "11", "18", "rollout-2026-11-18T10-15-02-%s.jsonl" % sid)
    old = os.path.join(c.codex, "sessions", "rollout-2025-05-01-old.jsonl")
    lines = codex_lines(c.alpha, sid)
    write(f, b"".join(lines))
    # an old Codex session file that names no folder waits (nothing guessed)
    write(old, line({"id": "old", "timestamp": "2025-05-01T10:00:00Z", "instructions": None}) + line({"record_type": "state"}))
    code, out, _ = c.sync()
    p = pieces(c.home, f)
    ok(code == 0 and len(p) == 1, "codex: the rollout file is saved (%d)" % len(p))
    pid, rid, rec = p[0]
    ok(project_root(c.home, pid) == c.alpha, "codex: its project comes from session_meta's cwd")
    want = check_piece(c, pid, rid, rec, open(f, "rb").read(), 0, lines, "codex", "codex", sid, files=False)
    ok(b"[hidden: OpenAI key]" in want, "codex: the key in the tool output is hidden")
    ok(b"[hidden: acme_]" in want and ACME.encode() not in want, "codex: the config's extra secret shape is hidden too")
    check_keys(c, pid, rid, want, "codex")
    ok(pieces(c.home, old) == [] and "names its working folder" in out, "codex: a session naming no folder is passed over with a note")
    more = [line({"timestamp": "2026-11-18T10:16:00.000Z", "type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fixed."}]}})]
    write(f, b"".join(more), "ab")
    c.sync()
    p = pieces(c.home, f)
    ok(len(p) == 2 and p[1][2].get("continues") == rid, "codex: the new tail is saved, continuing")
    check_piece(c, pid, p[1][1], p[1][2], open(f, "rb").read(), len(b"".join(lines)), more, "codex tail", "codex", sid, files=False)
    # usage
    for args, code in ((["sessions"], 2), (["sessions", "sync", "x"], 2), (["sessions", "--help"], 0)):
        p = subprocess.run([ctx.bin] + args, env=c.env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        ok(p.returncode == code, "usage: %s exits %d (%d)" % (" ".join(args), code, p.returncode))
    p = subprocess.run([ctx.bin, "sessions", "sync"], env=c.env(FEWER_HOME=None), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    ok(p.returncode == 2 and b"FEWER_HOME" in p.stderr, "usage: no store, exit 2 (%s)" % p.stderr)
    c.check_home_untouched("codex")


def check_aider(ctx):
    c = ctx.case("aider")
    c.config("sessions = on\n")
    f = os.path.join(c.alpha, ".aider.chat.history.md")
    text = ("# aider chat started at 2026-11-18 10:00:00\n\n> /usr/local/bin/aider --model sonnet\n> Aider v0.86.0\n\n"
            "#### fix the failing test\n\nThe test reads `OPENAI_API_KEY=%s` from the environment.\n\n> Applied edit to src/lib.rs\n" % OPENAI).encode()
    write(f, text)
    code, out, _ = c.sync()
    p = pieces(c.home, f)
    ok(code == 0 and len(p) == 1, "aider: the history is saved (%d: %s)" % (len(p), out))
    pid, rid, rec = p[0]
    ok(project_root(c.home, pid) == c.alpha and "session" not in rec, "aider: in the project it was found in, with no session id")
    check_piece(c, pid, rid, rec, text, 0, lines_of(text), "aider", "aider", files=False)
    more = b"\n# aider chat started at 2026-11-18 11:00:00\n\n#### and the docs\n\nDone.\n"
    write(f, more, "ab")
    c.sync()
    p = pieces(c.home, f)
    ok(len(p) == 2 and p[1][2].get("continues") == rid, "aider: the new chat is saved as a tail")
    check_piece(c, pid, p[1][1], p[1][2], open(f, "rb").read(), len(text), lines_of(more), "aider tail", "aider", files=False)
    # in development, only the folder FEWER_AIDER_DIR names is read: a history in another project's
    # root (beta: a Claude session's project, made in the same sync) is not, even unreadable
    fb = os.path.join(c.beta, ".aider.chat.history.md")
    write(fb, b"#### hello\n")
    write(os.path.join(c.claude, "projects", "-w-beta", SID_B + ".jsonl"), b"".join(short_session(c.beta, SID_B, 40, c.beta + "/y.rs")))
    os.chmod(fb, 0)
    try:
        code, out, err = c.sync()
    finally:
        os.chmod(fb, 0o644)
    ok(code == 0 and pieces(c.home, fb) == [] and "cannot read" not in out + err, "aider: a history outside FEWER_AIDER_DIR is not read (%s)" % out)
    # named by FEWER_AIDER_DIR: saved, in the project whose root that folder is
    code, out, _ = c.sync(FEWER_AIDER_DIR=c.beta)
    pb = pieces(c.home, fb)
    ok(code == 0 and len(pb) == 1 and project_root(c.home, pb[0][0]) == c.beta, "aider: the history FEWER_AIDER_DIR names is saved, in its project (%s)" % out)
    # no FEWER_AIDER_DIR: passed over with a note
    code, out, _ = c.sync(FEWER_AIDER_DIR=None)
    ok(code == 0 and "aider: FEWER_AIDER_DIR is not set (in development, an agent's sessions are read only from the folder set for it); passed over" in out, "aider: without FEWER_AIDER_DIR, passed over with a note (%s)" % out)
    c.check_home_untouched("aider")


def items_of(doc):
    """The exact bytes of each element of a JSON array, found with Python's own JSON decoder."""
    dec = json.JSONDecoder()
    s = doc.decode()
    i = 0

    def skip(i):
        while i < len(s) and s[i] in " \t\r\n":
            i += 1
        return i

    i = skip(i)
    assert s[i] == "["
    i = skip(i + 1)
    out = []
    if s[i] == "]":
        return out
    while True:
        _, end = dec.raw_decode(s, i)
        out.append(s[i:end].encode())
        i = skip(end)
        if s[i] == "]":
            return out
        i = skip(i + 1)


def check_cline(ctx):
    c = ctx.case("cline")
    c.config("sessions = on\n")
    task = "1763485200000"
    f = os.path.join(c.cline, "tasks", task, "api_conversation_history.json")
    msgs = [
        {"role": "user", "content": [{"type": "text", "text": "<task>\nfix the failing test\n</task>"}, {"type": "text", "text": "<environment_details>\n# Current Working Directory (/w/app) Files\n</environment_details>"}]},
        {"role": "assistant", "content": [{"type": "text", "text": "<thinking>read it</thinking>\n<read_file>\n<path>src/lib.rs</path>\n</read_file>"}]},
        {"role": "user", "content": [{"type": "text", "text": "[read_file for 'src/lib.rs'] Result:\nconst TOKEN: &str = \"%s\"; // é ] , {" % GH}]},
    ]
    doc = json.dumps(msgs, separators=(",", ":")).encode()
    write(f, doc)
    # a second task, written with white space (each element on several lines)
    f2 = os.path.join(c.cline, "tasks", "1763485300000", "api_conversation_history.json")
    doc2 = json.dumps(msgs[:2], indent=2, ensure_ascii=False).encode()
    write(f2, doc2)
    code, out, _ = c.sync()
    p, p2 = pieces(c.home, f), pieces(c.home, f2)
    ok(code == 0 and len(p) == 1 and len(p2) == 1, "cline: both tasks are saved (%s)" % out)
    pid, rid, rec = p[0]
    ok(project_root(c.home, pid) == c.cline and rec.get("reader") == "json" and rec.get("session") == task, "cline: the agent's folder as project, read by our JSON reader, the task as session (%s)" % {k: rec.get(k) for k in ("reader", "session")})
    text = b"".join(x + b"\n" for x in items_of(doc))
    want = check_piece(c, pid, rid, rec, text, 0, lines_of(text), "cline", "cline", task, files=False)
    ok(b"[hidden: GitHub token]" in want, "cline: the token in the tool result is hidden")
    text2 = b"".join(x + b"\n" for x in items_of(doc2))
    check_piece(c, p2[0][0], p2[0][1], p2[0][2], text2, 0, lines_of(text2), "cline indented", "cline", "1763485300000", files=False)
    # the task grows: Cline writes the whole file again; only the new messages are saved
    msgs2 = msgs + [{"role": "assistant", "content": [{"type": "text", "text": "<replace_in_file><path>src/lib.rs</path></replace_in_file>"}]}, {"role": "user", "content": [{"type": "text", "text": "ok"}]}]
    doc_b = json.dumps(msgs2, separators=(",", ":")).encode()
    # cut short while it is written: not saved, nothing failed
    write(f, doc_b[: len(doc_b) - 30])
    code, out, _ = c.sync()
    ok(code == 0 and len(pieces(c.home, f)) == 1 and "not whole JSON" in out, "cline: a task file cut short is not saved (%s)" % out)
    write(f, doc_b)
    c.sync()
    p = pieces(c.home, f)
    ok(len(p) == 2 and p[1][2].get("continues") == rid, "cline: the grown task saves only its new messages")
    text_b = b"".join(x + b"\n" for x in items_of(doc_b))
    check_piece(c, pid, p[1][1], p[1][2], text_b, len(text), lines_of(text_b[len(text):]), "cline tail", "cline", task, files=False)
    # an earlier message changed: the task is saved whole again
    msgs3 = [msgs2[0], {"role": "assistant", "content": [{"type": "text", "text": "changed"}]}] + msgs2[2:]
    write(f, json.dumps(msgs3, separators=(",", ":")).encode())
    c.sync()
    p = pieces(c.home, f)
    ok(len(p) == 3 and p[2][2].get("offset") == 0 and "continues" not in p[2][2], "cline: a task whose earlier message changed is saved whole again")
    c.check_home_untouched("cline")


CURSOR_QUERY = "SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%' OR key LIKE 'bubbleId:%'"


def sqlite(db, sql):
    subprocess.run(["/usr/bin/sqlite3", "-init", "/dev/null", db], input=sql.encode(), stdout=subprocess.DEVNULL, check=True, env={"PATH": "/usr/bin:/bin", "HOME": os.path.dirname(db)})


def q(s):
    return "'" + s.replace("'", "''") + "'"


def cursor_rows(db):
    """The rows sqlite3 prints for the query, without the array's punctuation."""
    p = subprocess.run(["/usr/bin/sqlite3", "-readonly", "-init", "/dev/null", "-json", db, CURSOR_QUERY], stdout=subprocess.PIPE, check=True, env={"PATH": "/usr/bin:/bin", "HOME": os.path.dirname(db)})
    rows = []
    for l in p.stdout.split(b"\n"):
        if l:
            l = l[1:] if l.startswith(b"[") else l
            rows.append(l[:-1] if l.endswith(b",") or l.endswith(b"]") else l)
    return rows


def check_cursor(ctx):
    c = ctx.case("cursor")
    c.config("sessions = on\n")
    os.makedirs(c.cursor)
    db = os.path.join(c.cursor, "state.vscdb")
    sqlite(db, "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);"
               "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);"
               "INSERT INTO ItemTable VALUES ('workbench.panel', '{}');"
               "INSERT INTO cursorDiskKV VALUES ('composerData:c1', %s);"
               "INSERT INTO cursorDiskKV VALUES ('bubbleId:c1:b1', %s);"
               "INSERT INTO cursorDiskKV VALUES ('bubbleId:c1:b2', CAST(%s AS BLOB));"
               "INSERT INTO cursorDiskKV VALUES ('checkpointId:c1:x', '{}');"
               % (q(json.dumps({"composerId": "c1", "name": "Fix tests", "fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]})),
                  q(json.dumps({"type": 1, "bubbleId": "b1", "text": "why does it fail? SLACK_TOKEN=%s" % SLACK})),
                  q(json.dumps({"type": 2, "bubbleId": "b2", "text": "Because of é and a tab\there"}, ensure_ascii=False))))
    db_sum = hashlib.sha256(open(db, "rb").read()).hexdigest()
    code, out, err = c.sync()
    p = pieces(c.home, db)
    ok(code == 0 and len(p) == 1, "cursor: its rows are saved (%d: %s %s)" % (len(p), out, err))
    calls = c.sqlite_calls()
    ok(calls == [["-readonly", "-init", "/dev/null", "-cmd", ".timeout 2000", "-json", db, CURSOR_QUERY]], "cursor: sqlite3 is run once, read-only, with CURSOR_QUERY: %s" % calls)
    ok(hashlib.sha256(open(db, "rb").read()).hexdigest() == db_sum, "cursor: reading does not change the database")
    pid, rid, rec = p[0]
    ok(project_root(c.home, pid) == c.cursor and rec.get("reader") == "sqlite3" and "offset" not in rec and "session" not in rec, "cursor: the agent's folder as project, read by sqlite3 (%s)" % rec)
    rows = cursor_rows(db)
    ok(len(rows) == 3, "cursor: three rows match the query (%d)" % len(rows))
    hidden = [ctx.hide(r + b"\n")[:-1] for r in rows]
    got = ctx.decode(archive(c.home, pid, rid))
    ok(got == b"".join(h + b"\n" for h in hidden), "cursor: the archive holds the rows as sqlite3 prints them, secrets hidden")
    ok(b"[hidden: Slack token]" in got and SLACK.encode() not in got and rec.get("hidden") == 1, "cursor: the Slack token is hidden and counted")
    ok(rec.get("lines") == 3 and rec.get("bytes") == len(got), "cursor: lines and bytes recorded")
    seen = open(os.path.join(c.home, "projects", pid, "cursor.seen")).read().split("\n")
    ok(sorted(x for x in seen[2:] if x) == sorted(hash128(h) for h in hidden) and seen[1].startswith("stamp = "), "cursor: cursor.seen holds our hash of each row saved")
    # unchanged: sqlite3 is not run again
    c.sync()
    ok(len(c.sqlite_calls()) == 1 and len(pieces(c.home, db)) == 1, "cursor: an unchanged database is not read again")
    # a row changed in place and a new row: only those two are saved
    sqlite(db, "INSERT INTO cursorDiskKV VALUES ('composerData:c1', %s); INSERT INTO cursorDiskKV VALUES ('bubbleId:c1:b3', %s);"
           % (q(json.dumps({"composerId": "c1", "name": "Fix tests, done"})), q(json.dumps({"type": 2, "bubbleId": "b3", "text": "Fixed."}))))
    db_sum = hashlib.sha256(open(db, "rb").read()).hexdigest()
    c.sync()
    p = pieces(c.home, db)
    ok(len(p) == 2 and p[1][2].get("continues") == rid, "cursor: the new and changed rows are saved, after the first piece")
    rows2 = cursor_rows(db)
    new = [r for r in rows2 if r not in rows]
    got2 = ctx.decode(archive(c.home, p[1][0], p[1][1]))
    ok(len(new) == 2 and got2 == b"".join(ctx.hide(r + b"\n") for r in new), "cursor: exactly the changed composer row and the new bubble (%d rows)" % len(new))
    ok(hashlib.sha256(open(db, "rb").read()).hexdigest() == db_sum, "cursor: reading again does not change the database")
    # touched only: read again, nothing new saved
    os.utime(db, None)
    c.sync()
    ok(len(c.sqlite_calls()) == 3 and len(pieces(c.home, db)) == 2, "cursor: a touched database is read, and nothing new is saved")
    # sqlite3 missing: passed over with a note
    os.utime(db, None)
    code, out, _ = c.sync(PATH="/bin")
    ok(code == 0 and "sqlite3 is not installed" in out, "cursor: without sqlite3, passed over with a note (%s)" % out)
    # not Cursor's database: sqlite3's error, nothing saved, exit 1
    os.utime(db, None)
    sqlite(db, "DROP TABLE cursorDiskKV;")
    code, out, err = c.sync()
    ok(code == 1 and "sqlite3 could not read it" in err and len(pieces(c.home, db)) == 2, "cursor: sqlite3 failing is reported, nothing saved (%d %s)" % (code, err))
    c.check_home_untouched("cursor")


def lock_out(*dirs):
    for d in dirs:
        os.makedirs(d, exist_ok=True)
        os.chmod(d, 0)


def let_in(*dirs):
    for d in dirs:
        os.chmod(d, 0o755)


def fill_all(c):
    """A session for every agent."""
    write(os.path.join(c.claude, "projects", "-w-alpha", SID_A + ".jsonl"), b"".join(session_a(c.alpha)))
    write(os.path.join(c.codex, "sessions", "2026", "11", "18", "rollout-x-%s.jsonl" % SID_C), b"".join(codex_lines(c.alpha, SID_C)))
    write(os.path.join(c.alpha, ".aider.chat.history.md"), b"#### hi\n")
    write(os.path.join(c.cline, "tasks", "1", "api_conversation_history.json"), b'[{"role":"user","content":"hi"}]')
    os.makedirs(c.cursor, exist_ok=True)
    sqlite(os.path.join(c.cursor, "state.vscdb"), "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB); INSERT INTO cursorDiskKV VALUES ('bubbleId:a:1', 'x');")


def check_off(ctx):
    c = ctx.case("off")
    fill_all(c)
    locked = [os.path.join(c.claude, "projects"), os.path.join(c.codex, "sessions"), os.path.join(c.cline, "tasks"), c.cursor]
    aider = os.path.join(c.alpha, ".aider.chat.history.md")
    try:
        for config in [None, "sessions = off\n", "sessions = OFF\nsessions.claude = on\n", "sessions = yes\n"]:
            if config is not None:
                c.config(config)
            os.chmod(aider, 0)
            lock_out(*locked)
            before = tree(c.home)
            code, out, err = c.sync()
            let_in(*locked)
            os.chmod(aider, 0o644)
            label = "off (%r)" % config
            ok(code == 0 and "sessions are off: nothing was read" in out, "%s: sync says so, exit 0 (%d %s %s)" % (label, code, out, err))
            ok("cannot read" not in out + err and "passed over" not in out, "%s: no agent folder was even listed (%s)" % (label, out))
            ok(c.sqlite_calls() == [], "%s: sqlite3 was never run" % label)
            ok(tree(c.home) == before and not os.path.exists(os.path.join(c.home, "projects")), "%s: nothing written in the store" % label)
        ok("neither on nor off" in out, "off: `sessions = yes` is read as off, with a note")
        # sessions on, every agent but claude off: only claude's folder is read
        c.config("sessions = on\nsessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n")
        os.chmod(aider, 0)
        lock_out(*locked[1:])
        code, out, err = c.sync()
        let_in(*locked[1:])
        os.chmod(aider, 0o644)
        ok(code == 0 and len(runs(c.home)) == 1 and list(runs(c.home).values())[0].get("agent") == "claude", "agents off: only Claude's session is saved (%s)" % out)
        ok(not any(a in out + err for a in ("codex:", "aider:", "cline:", "cursor:")) and c.sqlite_calls() == [], "agents off: their folders and sqlite3 are left alone (%s)" % out)
        # then all on: every agent's session is saved
        c.config("sessions = on\n")
        code, out, err = c.sync()
        agents = sorted(r.get("agent") for r in runs(c.home).values())
        ok(code == 0 and agents == ["aider", "claude", "cline", "codex", "cursor"], "all on: every agent saved (%s)" % agents)
    finally:
        let_in(*locked)
        os.chmod(aider, 0o644)
    c.check_home_untouched("off")


def check_server_sweep(ctx):
    c = ctx.case("sweep")
    c.config("sessions = on\n")
    fa = os.path.join(c.claude, "projects", "-w-alpha", SID_A + ".jsonl")
    a = session_a(c.alpha)
    write(fa, b"".join(a))
    more = session_a_more(c.alpha)

    def grow():
        time.sleep(1.0)
        write(fa, b"".join(more), "ab")

    t = threading.Thread(target=grow)
    t.start()
    p = subprocess.run([ctx.bin, "sweep-serve", "3000"], cwd=c.alpha, env=c.env(FEWER_TEST_SWEEP_MS="300"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    t.join()
    c.outputs.append(p.stderr.decode("utf-8", "replace"))
    pa = pieces(c.home, fa)
    ok(p.returncode == 0 and len(pa) == 2, "sweep: saved at its start, then the new part at a later turn (%d pieces; %s)" % (len(pa), p.stderr.decode()[-300:]))
    if len(pa) == 2:
        ok(pa[1][2].get("continues") == pa[0][1], "sweep: the later piece continues the first")
        check_piece(c, pa[0][0], pa[1][1], pa[1][2], open(fa, "rb").read(), len(b"".join(a)), more, "sweep tail", "claude", SID_A)
    ok(p.stdout == b"", "sweep: nothing on stdout")
    # off: nothing read, nothing saved
    c2 = ctx.case("sweep-off")
    fill_all(c2)
    locked = [os.path.join(c2.claude, "projects"), os.path.join(c2.codex, "sessions"), os.path.join(c2.cline, "tasks"), c2.cursor]
    lock_out(*locked)
    try:
        p = subprocess.run([ctx.bin, "sweep-serve", "800"], cwd=c2.alpha, env=c2.env(FEWER_TEST_SWEEP_MS="200"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    finally:
        let_in(*locked)
    ok(p.returncode == 0 and runs(c2.home) == {} and c2.sqlite_calls() == [] and b"cannot read" not in p.stderr, "sweep off: nothing read or saved (%s)" % p.stderr.decode()[-300:])
    c.check_home_untouched("sweep")
    c2.check_home_untouched("sweep-off")


def check_mcp(ctx):
    """The MCP server of a wired fewer sweeps at its start and every interval."""
    if not ctx.has_mcp:
        print("check_mcp: skipped (BIN has no mcp command: a test program)")
        return
    c = ctx.case("mcp")
    c.config("sessions = on\n")
    fa = os.path.join(c.claude, "projects", "-w-alpha", SID_A + ".jsonl")
    a = session_a(c.alpha)
    write(fa, b"".join(a))
    p = subprocess.Popen([ctx.bin, "mcp"], cwd=c.alpha, env=c.env(FEWER_TEST_SWEEP_MS="300", FEWER_DEBUG_LOG=os.path.join(c.base, "server.log")), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        p.stdin.write(b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"check","version":"0"}}}\n')
        p.stdin.flush()
        first = p.stdout.readline()
        ok(b'"serverInfo"' in first, "mcp: initialize answered")
        deadline = time.time() + 20
        while time.time() < deadline and len(pieces(c.home, fa)) < 1:
            time.sleep(0.1)
        ok(len(pieces(c.home, fa)) == 1, "mcp: the server saved the session at its start")
        write(fa, b"".join(session_a_more(c.alpha)), "ab")
        deadline = time.time() + 20
        while time.time() < deadline and len(pieces(c.home, fa)) < 2:
            time.sleep(0.1)
        ok(len(pieces(c.home, fa)) == 2, "mcp: the server saved the new part at a later turn")
        p.stdin.write(b'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"text":"Compacted","kind":"sessions"}}}\n')
        p.stdin.flush()
        ans = json.loads(p.stdout.readline())
        text = ans.get("result", {}).get("content", [{}])[0].get("text", "")
        ok("1 line" in text or "1 lines" in text, "mcp: search with kind sessions finds the session's line: %r" % text[:300])
        p.stdin.close()
        code = p.wait(timeout=30)
        ok(code == 0, "mcp: exits 0 at the end of stdin (%s)" % code)
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
    c.outputs.append(p.stderr.read().decode("utf-8", "replace"))
    c.check_home_untouched("mcp")


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    binary, fewer, work = (os.path.abspath(a) for a in sys.argv[1:4])
    fresh(work)
    # every process this check starts gets a made-up HOME, agents' folders and claude, inside
    # WORK (safe_env.py)
    safe = safe_env.setup(work)
    ctx = Ctx(binary, fewer, work)
    checks = [check_claude, check_stopped_sweep, check_codex, check_aider, check_cline, check_cursor, check_off, check_server_sweep, check_mcp]
    only = safe.caller.get("CHECK_ONLY")
    for c in checks:
        if only and only not in c.__name__:
            continue
        n0, f0 = PASSES[0], len(FAILS)
        t = time.time()
        try:
            c(ctx)
        except Exception as e:
            import traceback
            traceback.print_exc()
            ok(False, "%s raised %r" % (c.__name__, e))
        print("%-22s %3d passed, %d failed (%.1f s)" % (c.__name__, PASSES[0] - n0, len(FAILS) - f0, time.time() - t), flush=True)
    print("catalog: %s" % ("fewer's own (keys and edited files read from the .cat files)" if ctx.real_catalog else "the stand-in (edited files as given to it; keys are checked with the real catalog)"))
    print("all passed (%d checks)" % PASSES[0] if not FAILS else "%d FAILED of %d" % (len(FAILS), PASSES[0] + len(FAILS)))
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
