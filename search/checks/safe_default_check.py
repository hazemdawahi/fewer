"""fewer is safe by default in development: with sessions
on and no agent's folder setting, nothing of any agent is read or saved, whatever HOME holds.

    python3 -I checks/safe_default_check.py BIN PACK WORK [--control OLD_BIN]

BIN is fewer, PACK the fewer-pack that packs, WORK a scratch folder (made
fresh; an existing one is cleared only when a check made it). Every file here is made up in
WORK; every process gets its environment from safe_env.py.

The test:
- a made-up HOME (in WORK) holds Claude Code's folder (.claude/projects/<folder>/<id>.jsonl),
  Codex's (.codex/sessions/2026/11/18/rollout-...jsonl), Cursor's (Library/Application
  Support/Cursor/User/globalStorage/state.vscdb, a real SQLite database, and the same under
  .config/ for Linux) and Cline's in VS Code's folder, each with a canary session (a made-up
  session holding a word of its own, CANARY-<agent>-<random>); the project root the server
  and `sessions sync` run in holds Aider's history with its canary;
- the store's config says `sessions = on`, and no agent's folder setting is set
  (FEWER_CLAUDE_DIR, FEWER_CODEX_DIR, FEWER_AIDER_DIR, FEWER_CLINE_DIR, FEWER_CURSOR_DIR); a
  second round also sets the agents' own settings (CLAUDE_CONFIG_DIR, CODEX_HOME,
  XDG_CONFIG_HOME) to the canary folders;
- every canary folder and file is made unreadable while the programs run, and a recorder in
  front of sqlite3 notes any call: a try to list or open one would show as a note ("cannot
  read", "no folder", sqlite3's error) or a recorded call;
- the MCP server runs (its sweep at its start and every 200 ms, a test switch) and answers
  `logs` and `search` for every canary word over every project; then `sessions sync` runs;
- nothing is saved: the store holds no project and no run (the store's runs), the canary
  words are in no file of the store, no answer, no output and no log; the server's debug log
  and the sync's output name every agent as passed over because its setting is not set, and
  nothing else of the agents; sqlite3 was never run; the canaries are unchanged.

--control OLD_BIN runs the same scenario with a build from before the change (agents' folders
from HOME and the agents' own settings), without making the canaries unreadable, and expects
it to save them: it shows the test sees the slip. The canaries are made up, so the old build
reads only WORK.
"""
import hashlib
import importlib.util
import json
import os
import random
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


mcpc = load("mcpc")
safe_env = load("safe_env")
SETTINGS = ["FEWER_CLAUDE_DIR", "FEWER_CODEX_DIR", "FEWER_AIDER_DIR", "FEWER_CLINE_DIR", "FEWER_CURSOR_DIR"]
AGENTS = ["claude", "codex", "aider", "cline", "cursor"]
SID = "5afe5afe-0000-4000-8000-00000000c0de"
FAILS = []
PASSES = [0]


def ok(cond, what):
    if cond:
        PASSES[0] += 1
        print("ok:   " + what, flush=True)
    else:
        FAILS.append(what)
        print("FAIL: " + what, flush=True)


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def files_under(d):
    out = []
    for root, dirs, files in os.walk(d):
        for f in files:
            out.append(os.path.join(root, f))
    return sorted(out)


def fingerprint(paths):
    """Each canary's bytes and modification time (never changed by the programs)."""
    fp = {}
    for p in paths:
        for f in files_under(p) if os.path.isdir(p) else [p]:
            st = os.stat(f)
            fp[f] = (hashlib.sha256(open(f, "rb").read()).hexdigest(), st.st_mtime_ns)
    return fp


class Canaries:
    """The made-up HOME with every agent's canary session, and the project root with Aider's."""

    def __init__(self, base):
        tag = "%08x" % random.Random(os.getpid() ^ int(time.time())).getrandbits(32)
        self.words = {a: "CANARY-%s-%s" % (a, tag) for a in AGENTS}
        self.home = os.path.join(base, "home")
        self.proj = os.path.join(base, "proj")
        h, p = self.home, self.proj
        self.claude = os.path.join(h, ".claude")
        self.codex = os.path.join(h, ".codex")
        mac = os.path.join(h, "Library", "Application Support")
        lin = os.path.join(h, ".config")
        self.cursor = [os.path.join(x, "Cursor") for x in (mac, lin)]
        self.code = [os.path.join(x, "Code") for x in (mac, lin)]
        self.aider = os.path.join(p, ".aider.chat.history.md")
        claude_line = {"cwd": p, "sessionId": SID, "type": "user", "message": {"role": "user", "content": self.words["claude"]}, "timestamp": "2026-11-18T10:00:00.000Z"}
        write(os.path.join(self.claude, "projects", "-canary-proj", SID + ".jsonl"), (json.dumps(claude_line) + "\n").encode())
        codex = [
            {"timestamp": "2026-11-18T10:15:02.000Z", "type": "session_meta", "payload": {"id": SID, "timestamp": "2026-11-18T10:15:02.000Z", "cwd": p, "originator": "codex_cli_rs", "cli_version": "0.50.0"}},
            {"timestamp": "2026-11-18T10:15:03.000Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": self.words["codex"]}]}},
        ]
        write(os.path.join(self.codex, "sessions", "2026", "11", "18", "rollout-2026-11-18T10-15-02-%s.jsonl" % SID), "".join(json.dumps(x) + "\n" for x in codex).encode())
        for c in self.code:
            write(os.path.join(c, "User", "globalStorage", "saoudrizwan.claude-dev", "tasks", "1", "api_conversation_history.json"), json.dumps([{"role": "user", "content": self.words["cline"]}]).encode())
        for c in self.cursor:
            db = os.path.join(c, "User", "globalStorage", "state.vscdb")
            os.makedirs(os.path.dirname(db), exist_ok=True)
            sql = "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB); INSERT INTO cursorDiskKV VALUES ('composerData:1', '%s');" % self.words["cursor"]
            subprocess.run(["/usr/bin/sqlite3", "-init", "/dev/null", db], input=sql.encode(), stdout=subprocess.DEVNULL, check=True, env=SAFE.env(HOME=base))
        write(self.aider, ("# aider chat started at 2026-11-18 10:00:00\n\n#### %s\n" % self.words["aider"]).encode())
        # what is made unreadable while the programs run (folders, and Aider's file)
        self.locked = [self.claude, self.codex] + self.cursor + self.code + [self.aider]

    def lock(self, on):
        for p in self.locked:
            os.chmod(p, (0 if on else (0o755 if os.path.isdir(p) else 0o644)))


SAFE = None


def scenario(binary, fewer, base, canaries, own_settings, lock):
    """One round: the server with its sweeps and two tools, then `sessions sync`. Returns what
    the round saw."""
    store = os.path.join(base, "store")
    os.makedirs(store)
    with open(os.path.join(store, "config"), "w") as f:
        f.write("sessions = on\n")
    bindir = os.path.join(base, "bin")
    calls = os.path.join(base, "sqlite3-calls")
    os.makedirs(bindir)
    rec = os.path.join(bindir, "sqlite3")
    with open(rec, "w") as f:
        f.write('#!/bin/sh\nfor a in "$@"; do printf "%%s\\t" "$a" >> "%s"; done\necho >> "%s"\nexec /usr/bin/sqlite3 "$@"\n' % (calls, calls))
    os.chmod(rec, 0o755)
    log = os.path.join(base, "server.log")
    change = {s: None for s in SETTINGS}
    change.update(HOME=canaries.home, FEWER_HOME=store, FEWER_PACK_BIN=fewer, FEWER_PROJECT=canaries.proj, FEWER_DEBUG_LOG=log, FEWER_TEST_SWEEP_MS="200", PATH=bindir + ":/usr/bin:/bin")
    if own_settings:
        change.update(CLAUDE_CONFIG_DIR=canaries.claude, CODEX_HOME=canaries.codex, XDG_CONFIG_HOME=os.path.join(canaries.home, ".config"))
    # (the control, a build from before the rename to fewer, reads the same settings by their
    # old names; the new build passes those over)
    change.update(CMLOG_HOME=store, CMLOG_BIN=fewer, CMLOG_PROJECT=canaries.proj, CMLOG_SEARCH_LOG=log, CMLOG_TEST_SWEEP_MS="200")
    env = SAFE.env(**change)
    seen = {"answers": [], "outputs": []}
    before = fingerprint(canaries.locked)
    if lock:
        canaries.lock(True)
    try:
        srv = mcpc.Server(binary, env, canaries.proj, os.path.join(base, "server.err"))
        srv.initialize()
        # (several sweeps: at its start, then every 200 ms)
        time.sleep(1.5)
        seen["answers"].append(srv.call("logs", {"project": "*", "kind": "all"})[0])
        for w in canaries.words.values():
            seen["answers"].append(srv.call("search", {"text": w, "project": "*", "kind": "all"})[0])
        seen["server_exit"] = srv.close()
        r = subprocess.run([binary, "sessions", "sync"], env=env, cwd=canaries.proj, stdin=subprocess.DEVNULL, capture_output=True, timeout=300)
        seen["sync"] = (r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace"))
    finally:
        if lock:
            canaries.lock(False)
    seen["outputs"] += [seen["sync"][1], seen["sync"][2], open(os.path.join(base, "server.err"), "rb").read().decode("utf-8", "replace")]
    seen["log"] = open(log, "rb").read().decode("utf-8", "replace") if os.path.exists(log) else ""
    seen["unchanged"] = fingerprint(canaries.locked) == before
    seen["sqlite3"] = open(calls).read().split("\n")[:-1] if os.path.exists(calls) else []
    # the store's runs: every project's runs.jsonl
    projects = os.path.join(store, "projects")
    seen["projects"] = sorted(os.listdir(projects)) if os.path.isdir(projects) else []
    seen["runs"] = []
    for pid in seen["projects"]:
        rj = os.path.join(projects, pid, "runs.jsonl")
        if os.path.exists(rj):
            # (a later line with the same id updates that run: merged as store.rs reads them)
            merged = {}
            for l in open(rj).read().split("\n"):
                if l.strip():
                    r = json.loads(l)
                    merged.setdefault(r["id"], {}).update(r)
            seen["runs"] += list(merged.values())
    seen["store_text"] = b"".join(open(f, "rb").read() for f in files_under(store))
    # the canary words as the store would hold them packed: decode every archive
    for f in files_under(store):
        if f.endswith(".cmf"):
            seen["store_text"] += subprocess.run([fewer, "-d", "-c", f], capture_output=True).stdout
    return seen


def check_safe(binary, fewer, work, canaries, own_settings):
    label = "with the agents' own settings" if own_settings else "HOME only"
    s = scenario(binary, fewer, os.path.join(work, "round-" + ("own" if own_settings else "home")), canaries, own_settings, True)
    ok(s["runs"] == [] and s["projects"] == [], "%s: the store's runs: none (%d runs in %d projects)" % (label, len(s["runs"]), len(s["projects"])))
    ok(not any(w.encode() in s["store_text"] for w in canaries.words.values()), "%s: no canary word in any file of the store" % label)
    # (a search's answer may name the word it was asked for, and the debug log records each
    # call with its arguments: the words are looked for in everything else, and the searches
    # must find no line)
    log = "\n".join(l for l in s["log"].split("\n") if " tools/call " not in l)
    text = "\n".join(s["answers"][:1] + s["outputs"] + [log])
    ok(not any(w in text for w in canaries.words.values()), "%s: no canary word in the logs answer, any output or the debug log (but its record of the calls)" % label)
    ok(all("/r" not in a for a in s["answers"]), "%s: logs and the searches for every canary word over every project name no run" % label)
    print("%s: logs answers: %s" % (label, s["answers"][0].split("\n")[0][:160]), flush=True)
    # the debug log's notes: each agent passed over because its setting is not set, once each,
    # and nothing else of the agents (no folder listed, nothing saved, nothing failed)
    notes = [l.split("sessions: ", 1)[1] for l in s["log"].split("\n") if "sessions: " in l]
    for a, k in zip(AGENTS, SETTINGS):
        want = "%s: %s is not set (in development, an agent's sessions are read only from the folder set for it); passed over" % (a, k)
        ok(want in notes, "%s: the debug log notes %s is passed over (%s not set)" % (label, a, k))
    ok(len(notes) == 5, "%s: the debug log notes nothing else of the agents (%d notes)" % (label, len(notes)))
    ok("cannot read" not in text and "no folder" not in text and "not saved" not in text, "%s: no canary folder was even listed (no 'cannot read', 'no folder' or failure)" % label)
    code, out, err = s["sync"]
    ok(code == 0 and "sessions: 0 pieces saved, 0 sources unchanged, 0 failed" in out and err == "", "%s: sessions sync saves nothing, exit 0 (%d)" % (label, code))
    ok(all(("%s: %s is not set" % (a, k)) in out for a, k in zip(AGENTS, SETTINGS)), "%s: sessions sync names every agent as passed over" % label)
    ok(s["server_exit"] == 0, "%s: the server exits 0 at the end of stdin" % label)
    ok(s["sqlite3"] == [], "%s: sqlite3 was never run" % label)
    ok(s["unchanged"], "%s: every canary is unchanged (bytes and time)" % label)
    print("%s: store runs %d, debug-log notes %d, sqlite3 calls %d" % (label, len(s["runs"]), len(notes), len(s["sqlite3"])), flush=True)


def check_control(old, fewer, work, canaries):
    """The old build, canaries readable: it saves them (the test sees the slip)."""
    s = scenario(old, fewer, os.path.join(work, "round-control"), canaries, True, False)
    agents = sorted(set(r.get("agent") for r in s["runs"] if r.get("kind") == "session" and r.get("state") == "ready"))
    found = sorted(a for a, w in canaries.words.items() if w.encode() in s["store_text"])
    ok(agents == sorted(AGENTS) and found == sorted(AGENTS), "control (the build from before): it saved every canary session (agents %s, canary words in the store %s)" % (agents, found))
    print("control: store runs %d (sessions of %s)" % (len(s["runs"]), ", ".join(agents)), flush=True)


def main():
    global SAFE
    args = sys.argv[1:]
    old = None
    if "--control" in args:
        i = args.index("--control")
        old = os.path.abspath(args[i + 1])
        del args[i:i + 2]
    if len(args) != 3:
        sys.exit(__doc__)
    binary, fewer, work = (os.path.abspath(a) for a in args)
    mcpc.fresh_work(work)
    # every process this check starts, the server too, goes through safe_env.py: HOME and the
    # store in WORK; here the agents' folder settings are removed on purpose
    SAFE = safe_env.setup(work)
    canaries = Canaries(os.path.join(work, "canaries"))
    check_safe(binary, fewer, work, canaries, False)
    check_safe(binary, fewer, work, canaries, True)
    if old:
        check_control(old, fewer, work, canaries)
    print("all passed (%d checks)" % PASSES[0] if not FAILS else "%d FAILED of %d" % (len(FAILS), PASSES[0] + len(FAILS)))
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
