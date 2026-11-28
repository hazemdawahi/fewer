"""Acceptance checks of `install claude` and `uninstall claude`,
with a stub in place of Claude Code's `claude`, never the real one.

    python3 -I checks/install_check.py BIN WORK

BIN is fewer; WORK a
scratch folder (made fresh; an existing one is cleared only when it holds the
mark file a check leaves). The stub is ../tests/data/claude-stub (next to this folder), copied
into each case's own folder in WORK as `claude`; it records every call's arguments and reads
them as Claude Code 2.1.287 does. Every run gets a clean environment (and safe_env.py checks
it): PATH /usr/bin:/bin (checked to hold no claude), plus the stub's folder only when a case
wants a named claude found on PATH; HOME a folder in WORK (checked to stay empty); FEWER_HOME
in WORK; FEWER_CLAUDE_BIN the stub, unless a case says otherwise; every agent's folder
(FEWER_CLAUDE_DIR, ..., made up and empty) in WORK, unless a case says otherwise. No network;
nothing is left running.

The checks, each in a terminal (Python's pty module) unless
it says otherwise:
- yes: "y" registers; the stub got exactly `mcp get fewer`, then
  `mcp add --scope user fewer -e FEWER_HOME=<home> -- <BIN> mcp`, and read them as claude
  does (user scope, command BIN, args mcp, env FEWER_HOME); claude's output is shown; the
  change is shown before the question; the use line ends the install;
- no: "n", an empty line, an unknown word and the end of input register nothing and ask
  nothing more; y, Y, yes and YES are yeses;
- --dry-run: shown, nothing run but `mcp get`, nothing changed;
- already registered: said, claude's details shown, nothing else run;
- claude missing (FEWER_CLAUDE_BIN not a program, or a name not on PATH): the command to run
  later, exit 0, the stub never run;
- in development, FEWER_CLAUDE_BIN not set: install, its dry run and uninstall exit 2 and say how to
  set it, even with a claude on PATH; nothing is run;
- stdin not a terminal (/dev/null, a pipe): exit 1 without --yes; --yes registers without the
  question and leaves the sessions question unanswered;
- the exact command and arguments, also with spaces and quotes in the paths, and as printed
  for the shell (it reads back to the same arguments); the order `-e VAR=value fewer --` fails with
  the stub as with claude;
- the sessions question names every agent a yes would save (each one whose folder is set,
  and not turned off by a `sessions.<agent> = off` line) with where its sessions are; it sets
  the config: yes sets sessions = on (and sessions.claude = on when that line said off),
  keeping every other line and the file's mode; no changes nothing; not asked when they are
  saved already, or when no agent's folder is set;
- fewer (savers.rs): shown first, turned on under the one question that registers fewer; it writes Claude Code's settings.json in the made-up
  FEWER_CLAUDE_DIR (four hooks naming BIN and the store, the smaller start, the lean agent
  setting and environment; no autoCompactWindow) and the three agents' files (fewer-lean,
  fewer-lean-workflow, fewer-worker), keeping the developer's own keys and a copy of the file as it was; the
  hook commands written there work when run as Claude Code runs them (sh -c, the event on
  stdin, no FEWER_HOME in the environment); also when fewer is registered already, not again
  when it is on; a file fewer cannot read is left as it is; uninstall's yes takes it out again;
- uninstall: yes removes (`mcp remove --scope user fewer`), no keeps, not registered says so,
  --dry-run, not a terminal, a failure prints the command; install again afterwards;
- and: claude mcp add failing (the command is printed, exit 1), no FEWER_HOME (exit 2),
  usage errors (exit 2), FEWER_CLAUDE_BIN a name found on PATH;
- with a BIN that has the MCP server (fewer, wired): the command the stub registered,
  run with its arguments and environment as Claude Code runs it, answers initialize and
  tools/list (what `claude mcp get` checks). Lane B's own test program has no `mcp`
  command: then this check says it is skipped.
"""
import errno
import importlib.util
import json
import os
import pty
import re
import select
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time

MARK = ".laneb-check-scratch"
HERE = os.path.dirname(os.path.abspath(__file__))
STUB = os.path.normpath(os.path.join(HERE, "..", "tests", "data", "claude-stub"))
SYSTEM_PATH = "/usr/bin:/bin"
USE_LINE = "\nNext: in Claude Code, open a new chat (or close and reopen an old one) and ask it to run your tests and tell you what fails\n  fewer tokens   the tokens fewer saved\n  fewer off      turn fewer off (fewer on turns it back on)"


def flat(text):
    """The text with its wrapped lines joined: each line end, and the spaces around it, one space."""
    return " ".join(l.strip() for l in text.split("\n") if l.strip())


READS = ("read", "table", "look", "changes", "tool", "skill")
READS_LINE = "fewer's reading commands (read, table, look, changes, tool, skill) run without asking, as Claude Code's Read tool does"


def read_rules(word):
    """fewer's allow rules for fewer's reading commands (savers::read_rules); none when the
    name of fewer in them has a comma, a quote or a bracket (savers::reads_plan)."""
    return [] if any(ch in word for ch in ",'\"()") else ["Bash(%s %s:*)" % (word, c) for c in READS]
FAILS = []
PASSES = [0]
_spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(safe_env)
# the agents, as the sessions question names them, and their folder settings
AGENTS = [("Claude Code", "FEWER_CLAUDE_DIR", "projects"), ("Codex", "FEWER_CODEX_DIR", "sessions"), ("Aider", "FEWER_AIDER_DIR", ".aider.chat.history.md"), ("Cline", "FEWER_CLINE_DIR", "tasks"), ("Cursor", "FEWER_CURSOR_DIR", "state.vscdb")]
ALL = "Claude Code, Codex, Aider, Cline and Cursor"


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


class Case:
    """A case's own folder: stub/claude (the stub), home/ (FEWER_HOME, not made), fakehome/
    (HOME)."""

    n = 0

    def __init__(self, ctx, name, home_name="home"):
        Case.n += 1
        self.ctx = ctx
        self.base = os.path.join(ctx.work, "%02d-%s" % (Case.n, name))
        self.stub_dir = os.path.join(self.base, "stub")
        self.stub = os.path.join(self.stub_dir, "claude")
        self.home = os.path.join(self.base, home_name)
        self.fakehome = os.path.join(self.base, "fakehome")
        os.makedirs(self.stub_dir)
        os.makedirs(self.fakehome)
        shutil.copyfile(STUB, self.stub)
        os.chmod(self.stub, 0o755)

    def agent_dir(self, setting):
        """A made-up folder of an agent (made only by a yes to the token savers, for Claude
        Code's settings.json; install otherwise only names it)."""
        return os.path.join(self.base, "agents", setting.split("_")[1].lower())

    def settings_file(self):
        return os.path.join(self.agent_dir("FEWER_CLAUDE_DIR"), "settings.json")

    def settings(self):
        """Claude Code's settings.json in the made-up folder, read as JSON; None when there is none."""
        try:
            with open(self.settings_file(), "rb") as f:
                return json.loads(f.read())
        except FileNotFoundError:
            return None

    def env(self, **change):
        e = {"PATH": SYSTEM_PATH, "HOME": self.fakehome, "TERM": "xterm", "LANG": "en_US.UTF-8", "FEWER_HOME": self.home, "FEWER_CLAUDE_BIN": self.stub}
        for _, s, _ in AGENTS:
            e[s] = self.agent_dir(s)
        for k, v in change.items():
            if v is not None:
                e[k] = v
        # (a setting removed on purpose stays removed: safe_env.py)
        return safe_env.without(e, *[k for k, v in change.items() if v is None])

    def where(self):
        """What the sessions question says each agent's sessions are, all folders set."""
        return ", ".join("%s (in %s)" % (n, os.path.join(self.agent_dir(s), sub)) for n, s, sub in AGENTS)

    def calls(self):
        """The stub's calls, each as its list of arguments."""
        p = os.path.join(self.stub_dir, "calls")
        if not os.path.exists(p):
            return []
        data = open(p, "rb").read()
        return [[a.decode("utf-8", "surrogateescape") for a in c.split(b"\0")[:-1]] for c in data.split(b"\x1e")[:-1]]

    def server(self):
        """The stub's record of the registered server: {scope, command, args, env}, or None."""
        p = os.path.join(self.stub_dir, "server-fewer")
        if not os.path.exists(p):
            return None
        r = {"args": [], "env": []}
        for line in open(p, encoding="utf-8", errors="surrogateescape").read().split("\n")[:-1]:
            k, v = line.split("=", 1)
            if k in ("arg", "env"):
                r[k if k == "env" else "args"].append(v)
            else:
                r[k] = v
        return r

    def config(self):
        p = os.path.join(self.home, "config")
        return open(p, "rb").read() if os.path.exists(p) else None

    def put_config(self, text, mode=None):
        os.makedirs(self.home, exist_ok=True)
        p = os.path.join(self.home, "config")
        open(p, "wb").write(text)
        if mode is not None:
            os.chmod(p, mode)

    def register(self, args=None):
        """Register a server with the stub directly, as if the developer had done it before."""
        args = args or ["mcp", "add", "--scope", "user", "fewer", "--", "/elsewhere/fewer", "mcp"]
        r = subprocess.run([self.stub] + args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env={"PATH": SYSTEM_PATH})
        return r

    def done(self):
        ok(os.listdir(self.fakehome) == [], "%s: the program wrote into HOME: %r" % (self.base, os.listdir(self.fakehome)))

    def tty(self, args, answers, env=None, timeout=60):
        """BIN with ARGS on a terminal (Python's pty module): each time a new "[y/N] " shows,
        the next answer is typed (None: the end of input, Ctrl-D). Returns (exit status,
        output with "\\r\\n" made "\\n", the number of questions seen)."""
        env = env or self.env()
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.execve(self.ctx.bin, [self.ctx.bin] + args, env)
            finally:
                os._exit(127)
        out = bytearray()
        asked = 0
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
            seen = out.count(b"[y/N] ")
            while asked < seen:
                a = answers[asked] if asked < len(answers) else None
                os.write(fd, b"\x04" if a is None else a.encode() + b"\n")
                asked += 1
            if time.time() > end:
                os.kill(pid, signal.SIGKILL)
                ok(False, "timed out: %r" % (args,))
                break
        _, st = os.waitpid(pid, 0)
        os.close(fd)
        # ("\r\r\n" too: macOS's terminal driver can send it when a long write is split)
        return code_of(st), re.sub(rb"\r+\n", b"\n", bytes(out)).decode("utf-8", "surrogateescape"), asked

    def pipe(self, args, env=None, stdin=None):
        """BIN with ARGS, stdin /dev/null or the given bytes through a pipe (not a terminal):
        (exit status, stdout, stderr)."""
        r = subprocess.run([self.ctx.bin] + args, input=stdin, stdin=None if stdin is not None else subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env or self.env(), timeout=60)
        return r.returncode, r.stdout.decode("utf-8", "surrogateescape"), r.stderr.decode("utf-8", "surrogateescape")


def code_of(st):
    return os.waitstatus_to_exitcode(st) if hasattr(os, "waitstatus_to_exitcode") else (os.WEXITSTATUS(st) if os.WIFEXITED(st) else -os.WTERMSIG(st))


class Ctx:
    def __init__(self, binary, work):
        self.bin = binary
        self.work = work


GET = ["mcp", "get", "fewer"]
REMOVE = ["mcp", "remove", "--scope", "user", "fewer"]


def add_call(c, me):
    return ["mcp", "add", "--scope", "user", "fewer", "-e", "FEWER_HOME=" + c.home, "--", me, "mcp"]


def cfg_rest(c):
    """The store's config without fewer's records (savers.* lines, which install now writes by
    itself), its line ends as written; None when nothing else is in it."""
    data = c.config()
    if data is None:
        return None
    keep = b"".join(l for l in data.splitlines(keepends=True) if not l.startswith(b"savers."))
    return keep or None


def same_rest(c, want):
    """Whether the config is `want` once fewer's records are set aside (a last line end that
    install's own lines added is not counted)."""
    got = cfg_rest(c)
    return (got or b"").rstrip(b"\n") == (want or b"").rstrip(b"\n")


# (install saves Claude Code's chats under its one yes: sessions on, every other agent off)
OTHERS_OFF = b"sessions.codex = off\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"
CHATS_ON = b"sessions = on\n" + OTHERS_OFF


def chats_said(c):
    """What install shows of the chats, with the other changes before its question (its wrapped
    lines joined: compare with flat(out))."""
    return "3. Save your Claude Code chats (%s/projects) on this computer too, in %s, with secrets hidden: the token card and the start card come from them (sessions = on in %s/config; to stop: fewer memory sessions off)." % (c.agent_dir("FEWER_CLAUDE_DIR"), c.home, c.home)


TENX_ON = "✓ fewer is on: it applies to new Claude Code chats, and to old ones when you reopen them.\n"
# fewer's slash commands, written after fewer (install, and again on an update)
SLASH_ON = "✓ fewer's slash commands are in Claude Code: type /fewer in a chat (/fewer-tokens, /fewer-off, /fewer-on, /fewer-memory).\n"
SLASH_FILES = ["fewer-memory.md", "fewer-off.md", "fewer-on.md", "fewer-tokens.md"]


def slash_said(c):
    """Install's step 4, before its question (its wrapped lines joined: compare with flat(out))."""
    return "4. Add fewer's slash commands to Claude Code (%s/commands): /fewer-tokens, /fewer-off, /fewer-on and /fewer-memory. Only you run them: Claude is not told about them." % c.agent_dir("FEWER_CLAUDE_DIR")


def check_yes(ctx):
    c = Case(ctx, "yes")
    code, out, asked = c.tty(["install", "claude"], ["y"])
    ok(code == 0, "yes: exit %r\n%s" % (code, out))
    ok(asked == 1, "yes: %d questions" % asked)
    ok("fewer? [y/N]" not in out and TENX_ON in out and (c.settings() or {}).get("agent") == "fewer-lean", "yes: fewer on by itself, not asked:\n%s" % out)
    ok(c.calls() == [GET, add_call(c, ctx.bin)], "yes: calls %r" % c.calls())
    ok(c.server() == {"scope": "user", "command": ctx.bin, "args": ["mcp"], "env": ["FEWER_HOME=" + c.home]}, "yes: the stub read %r" % c.server())
    # (the MCP server and fewer's changes shown before the one question that covers them,
    # as they are)
    shown = "fewer will make these changes in Claude Code:\n\n  1. Register fewer's MCP server for all your projects (user scope):\n       %s\n\n  2. Turn fewer on, 86%% fewer tokens in our tests" % shlex.join([c.stub] + add_call(c, ctx.bin))
    ok(out.startswith(shown) and ("takes all of this back out. " + chats_said(c) + " " + slash_said(c) + " Register fewer and turn it on (the changes above)? [y/N] ") in flat(out), "yes: the change is shown before the question:\n%s" % out)
    commands = os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), "commands")
    ok(SLASH_ON in out and os.path.isdir(commands) and sorted(os.listdir(commands)) == SLASH_FILES and all("disable-model-invocation: true" in open(os.path.join(commands, f)).read() for f in SLASH_FILES), "yes: the slash commands written:\n%s" % out)
    ok("Register fewer and turn it on (the changes above)? [y/N] y\nAdded stdio MCP server fewer with command: %s mcp to user config\nFile modified: %s/.claude.json\n" % (ctx.bin, c.stub_dir) in out, "yes: claude's output shown:\n%s" % out)
    ok(("✓ Your Claude Code chats will be saved (sessions = on in %s/config).\n" % c.home) in out, "yes: the chats:\n%s" % out)
    ok(out.endswith(USE_LINE + "\n"), "yes: the use line last:\n%s" % out)
    ok(cfg_rest(c) == CHATS_ON and b"savers.lean = lean:agent," in (c.config() or b""), "yes: config %r" % c.config())
    ok(sorted(os.listdir(c.home)) == ["config", "config.lock"], "yes: the store holds %r" % os.listdir(c.home))
    c.done()


def check_no(ctx):
    for answer in ["n", "no", "", "yess", "ok", None]:
        c = Case(ctx, "no")
        code, out, asked = c.tty(["install", "claude"], [answer, "y"])
        ok(code == 0, "no %r: exit %r" % (answer, code))
        ok(asked == 1, "no %r: %d questions" % (answer, asked))
        ok(out.endswith("Not registered. Nothing was changed.\n"), "no %r:\n%s" % (answer, out))
        ok(c.calls() == [GET], "no %r: calls %r" % (answer, c.calls()))
        ok(c.server() is None and c.config() is None and not os.path.exists(c.home), "no %r: something changed" % answer)
        c.done()
    for answer in ["y", "Y", "yes", "YES", " Yes "]:
        c = Case(ctx, "yes-words")
        code, out, asked = c.tty(["install", "claude"], [answer])
        ok(code == 0 and asked == 1 and c.server() is not None and cfg_rest(c) == CHATS_ON, "yes %r: exit %r, %d questions, %r\n%s" % (answer, code, asked, c.config(), out))
        c.done()


def check_dry_run(ctx):
    c = Case(ctx, "dry-run")
    for run in ("tty", "pipe"):
        if run == "tty":
            code, out, asked = c.tty(["install", "claude", "--dry-run"], ["y", "y"])
        else:
            code, out, _ = c.pipe(["install", "claude", "--dry-run"])
            asked = out.count("[y/N] ")
        ok(code == 0 and asked == 0, "dry run (%s): exit %r, %d questions" % (run, code, asked))
        # (it changes the file, or makes it when there is none yet)
        start = "2. Turn fewer on, 86%% fewer tokens in our tests, with the same answers: it %s %s" % ("changes" if os.path.exists(c.settings_file()) else "makes", c.settings_file())
        ok(start in flat(out) and chats_said(c) in flat(out), "dry run (%s), the start: %r" % (run, out[:1500]))
        ok(out.endswith('Dry run: nothing was changed. `fewer install` asks "Register fewer and turn it on (the changes above)? [y/N]" and makes them on a yes; `fewer off` takes fewer back out.\n'), "dry run (%s), the end: %r" % (run, out[-200:]))
        ok(("\n       %s\n" % shlex.join([c.stub] + add_call(c, ctx.bin))) in out, "dry run (%s): the command:\n%s" % (run, out))
    ok(c.calls() == [GET, GET], "dry run: calls %r" % c.calls())
    ok(c.server() is None and not os.path.exists(c.home) and not os.path.exists(c.agent_dir("FEWER_CLAUDE_DIR")), "dry run: something changed")
    # uninstall --dry-run
    c.register()
    code, out, asked = c.tty(["uninstall", "claude", "--dry-run"], ["y"])
    ok(code == 0 and asked == 0 and out.endswith("The command:\n  %s mcp remove --scope user fewer\nDry run: nothing was changed.\n" % c.stub), "uninstall dry run:\n%s" % out)
    ok(c.server() is not None and c.calls()[-1] == GET, "uninstall dry run: removed, or calls %r" % c.calls())
    c.done()


def check_already(ctx):
    c = Case(ctx, "already")
    r = c.register()
    ok(r.returncode == 0, "already: the stub did not register: %r" % r.stderr)
    for run in ("tty", "pipe --yes"):
        n = len(c.calls())
        if run == "tty":
            # (registered by hand, fewer never on: its changes shown and asked about)
            code, out, asked = c.tty(["install", "claude"], ["y"])
            ok(code == 0 and asked == 1 and "Turn fewer on (the changes above)? [y/N] y\n" in out and TENX_ON in out and (c.settings() or {}).get("agent") == "fewer-lean", "already (%s): exit %r, %d questions\n%s" % (run, code, asked, out))
        else:
            code, out, _ = c.pipe(["install", "claude", "--yes"])
            ok(code == 0 and "[y/N]" not in out and out.endswith("fewer is on already in %s.\n%s" % (c.settings_file(), SLASH_ON)), "already (%s): exit %r\n%s" % (run, code, out))
        if run == "tty":
            ok(out.startswith("Claude Code has an MCP server named fewer already (claude mcp get fewer):\nfewer:\n  Scope: user config\n"), "already (%s):\n%s" % (run, out))
            ok("  Command: /elsewhere/fewer\n" in out and ("\nIt runs another fewer than this one (%s). To register this one: fewer uninstall, then fewer install.\n" % ctx.bin) in out, "already (%s):\n%s" % (run, out))
        else:
            # (fewer on since the first run: Claude Code shows and adds no MCP server, as Claude
            # Code 2.1.287 does, so an update leaves the registration as it is)
            ok(out.startswith("fewer's MCP server is left as it is: fewer keeps it off (Claude runs fewer's commands instead), and Claude Code neither shows nor adds it then (fewer off shows it again).\n"), "already (%s), fewer on:\n%s" % (run, out))
        ok(c.calls()[n:] == [GET], "already (%s): calls %r" % (run, c.calls()[n:]))
    # (the store's config holds only when the token savers went on: tokens reports before and after it)
    ok(c.server()["command"] == "/elsewhere/fewer" and re.fullmatch(rb"savers\.on = \d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\n(savers\.off = \n)?(savers\.made = settings\.json\n)?(savers\.made_agents = agents\n)?savers\.start = (?:skill:[^,\n]+(?:,skill:[^,\n]+)*)?\nsavers\.lean = lean:agent(,lean:(?:[a-zA-Z_:-]+|allow:Bash\([^()\n]+\)))+\n", c.config() or b"") is not None, "already: something else changed: %r" % c.config())
    c.done()


def check_missing(ctx):
    c = Case(ctx, "missing")
    for d in SYSTEM_PATH.split(":"):
        if os.path.exists(os.path.join(d, "claude")):
            sys.exit("refusing to run: %s/claude exists, and the checks would find it" % d)
    noexec = os.path.join(c.base, "claude-noexec")
    open(noexec, "w").write("#!/bin/sh\n")
    os.chmod(noexec, 0o644)
    cases = [
        ("FEWER_CLAUDE_BIN missing", c.env(FEWER_CLAUDE_BIN=os.path.join(c.base, "nowhere", "claude")), "FEWER_CLAUDE_BIN=%s is not a program" % os.path.join(c.base, "nowhere", "claude"), os.path.join(c.base, "nowhere", "claude")),
        ("FEWER_CLAUDE_BIN not runnable", c.env(FEWER_CLAUDE_BIN=noexec), "FEWER_CLAUDE_BIN=%s is not a program" % noexec, noexec),
        ("FEWER_CLAUDE_BIN a name not on PATH", c.env(FEWER_CLAUDE_BIN="claude"), "FEWER_CLAUDE_BIN=claude was not found on PATH", "claude"),
    ]
    for name, env, why, shown in cases:
        for run in ("tty", "pipe"):
            if run == "tty":
                code, out, asked = c.tty(["install", "claude"], ["y", "y"], env=env)
            else:
                code, out, _ = c.pipe(["install", "claude"], env=env)
                asked = 0
            # (nothing installed: exit 1)
            want = "%s. When Claude Code is installed, run fewer install again (or, for fewer's MCP server alone: the command below).\n  %s\n" % (why, shlex.join([shown] + add_call(c, ctx.bin)))
            ok(code == 1 and asked == 0 and out == want, "missing, %s (%s): exit %r\n%r\nwanted\n%r" % (name, run, code, out, want))
        code, out, _ = c.pipe(["uninstall", "claude"], env=env)
        # (Claude Code not found, nothing of fewer in its settings: exit 3, nothing of fewer runs)
        ok(code == 3 and out == "%s, so fewer's MCP server cannot be taken out of Claude Code now. When Claude Code is there, run:\n  %s mcp remove --scope user fewer\n" % (why, shlex.quote(shown)), "missing, %s: uninstall %r\n%s" % (name, code, out))
    ok(c.calls() == [], "missing: the stub was run: %r" % c.calls())
    c.done()


def check_claude_bin_required(ctx):
    # in development, claude is run only from FEWER_CLAUDE_BIN; without it install, its dry run and
    # uninstall exit 2 and say how to set it, even with a claude (the stub) on PATH
    c = Case(ctx, "claude-bin-required")
    path = c.stub_dir + ":" + SYSTEM_PATH
    for args in (["install", "claude"], ["install", "claude", "--dry-run"], ["install", "claude", "--yes"], ["uninstall", "claude", "--yes"]):
        for run in ("tty", "pipe"):
            env = c.env(FEWER_CLAUDE_BIN=None, PATH=path)
            if run == "tty":
                code, out, asked = c.tty(args, ["y", "y"], env=env)
                err = out
            else:
                code, out, err = c.pipe(args, env=env)
                asked = 0
            ok(code == 2 and asked == 0, "FEWER_CLAUDE_BIN not set, %r (%s): exit %r, %d questions" % (args, run, code, asked))
            ok(("fewer %s: FEWER_CLAUDE_BIN is not set. In development (FEWER_DEV), fewer runs Claude Code's `claude` only from FEWER_CLAUDE_BIN, never from PATH by itself. Set it to the claude program and run this again, for example:\n  FEWER_CLAUDE_BIN=\"$(command -v claude)\" fewer %s\n" % (args[0], args[0])) in err, "FEWER_CLAUDE_BIN not set, %r (%s):\n%s" % (args, run, err))
            if run == "pipe":
                ok(out == "", "FEWER_CLAUDE_BIN not set, %r: nothing on stdout: %r" % (args, out))
    ok(c.calls() == [] and not os.path.exists(c.home), "FEWER_CLAUDE_BIN not set: the stub was run %r, or the store made" % c.calls())
    c.done()


def check_not_terminal(ctx):
    c = Case(ctx, "not-terminal")
    for name, stdin in [("/dev/null", None), ("a pipe holding yes", b"y\ny\n")]:
        code, out, err = c.pipe(["install", "claude"], stdin=stdin)
        ok(code == 1, "not a terminal (%s): exit %r" % (name, code))
        ok(err == 'fewer install: not registered: stdin is not a terminal, so no one can answer "Register fewer and turn it on (the changes above)? [y/N]". Run it in a terminal, add --yes, or run the command above yourself (fewer\'s MCP server alone).\n', "not a terminal (%s): %r" % (name, err))
        ok(("\n       %s\n" % shlex.join([c.stub] + add_call(c, ctx.bin))) in out, "not a terminal (%s):\n%s" % (name, out))
    ok(c.calls() == [GET, GET] and c.server() is None and not os.path.exists(c.home), "not a terminal: something changed: %r" % c.calls())
    # --yes: registered without the question, the chats saved under it
    code, out, err = c.pipe(["install", "claude", "--yes"], stdin=b"y\ny\n")
    ok(code == 0 and err == "", "--yes: exit %r %r" % (code, err))
    ok(c.calls()[2:] == [GET, add_call(c, ctx.bin)], "--yes: calls %r" % c.calls())
    ok("[y/N]" not in out and ("✓ Your Claude Code chats will be saved (sessions = on in %s/config).\n" % c.home) in out, "--yes:\n%s" % out)
    ok(out.endswith(TENX_ON + SLASH_ON + USE_LINE + "\n") and (c.settings() or {}).get("agent") == "fewer-lean", "--yes, fewer on by itself:\n%s" % out)
    ok(cfg_rest(c) == CHATS_ON, "--yes: config %r" % c.config())
    # --yes in a terminal: no question at all
    c2 = Case(ctx, "yes-in-terminal")
    code, out, asked = c2.tty(["install", "claude", "--yes"], ["n"])
    ok(code == 0 and asked == 0 and ("✓ Your Claude Code chats will be saved (sessions = on in %s/config).\n" % c2.home) in out, "--yes in a terminal: %r %d\n%s" % (code, asked, out))
    ok(c2.server() is not None and cfg_rest(c2) == CHATS_ON, "--yes in a terminal: state")
    c.done()
    c2.done()


def check_exact_args(ctx):
    # paths with a space and a quote: the arguments arrive exactly; the printed line reads
    # back to the same arguments in sh
    c = Case(ctx, "exact", home_name="my store's home")
    code, out, asked = c.tty(["install", "claude"], ["y"])
    ok(code == 0 and asked == 1, "exact: exit %r, %d questions\n%s" % (code, asked, out))
    want = add_call(c, ctx.bin)
    ok(c.calls() == [GET, want], "exact: calls %r" % c.calls())
    ok(c.server() == {"scope": "user", "command": ctx.bin, "args": ["mcp"], "env": ["FEWER_HOME=" + c.home]}, "exact: the stub read %r" % c.server())
    line = [l for l in out.split("\n") if l.startswith("  ") and " mcp add " in l]
    ok(len(line) == 1, "exact: the command line shown:\n%s" % out)
    if line:
        # sh itself reads the printed line: `set -- LINE`, then each argument it got
        script = "set -- %s; for a in \"$@\"; do printf '%%s\\0' \"$a\"; done" % line[0].strip()
        argv = subprocess.run(["/bin/sh", "-c", script], stdout=subprocess.PIPE, env={"PATH": SYSTEM_PATH}).stdout.split(b"\0")[:-1]
        ok([a.decode("utf-8", "surrogateescape") for a in argv] == [c.stub] + want, "exact: the shown line reads back as %r" % argv)
        ok("'FEWER_HOME=%s'" % c.home.replace("'", "'\\''") in line[0], "exact: quoting %r" % line[0])
    # -e before the name: the stub, as claude, takes fewer as a variable
    r = c.register(["mcp", "add", "--scope", "user", "-e", "FEWER_HOME=/x", "fewer", "--", "/abs/fewer", "mcp"])
    ok(r.returncode == 1 and b"Invalid environment variable format: fewer, environment variables should be added as: -e KEY1=value1 -e KEY2=value2" in r.stderr, "-e before the name: %r %r" % (r.returncode, r.stderr))
    c.done()


def check_sessions_config(ctx):
    # Claude Code's chats, saved under install's one yes: the store's config after it (its other
    # lines kept as written, its mode too), and what install said
    cases = [
        # (config before, its mode, answers, config after)
        (b"# my settings\ndna = off\npause = /w/secret folder\nsecret = ACME_ alnum 20\n", 0o600, ["y"], b"# my settings\ndna = off\npause = /w/secret folder\nsecret = ACME_ alnum 20\n" + CHATS_ON),
        (b"sessions = off\r\ndna = on", None, ["yes"], b"sessions = on\r\ndna = on\n" + OTHERS_OFF),
        (b"sessions = on\nsessions.claude = off\n", None, ["Y"], b"sessions = on\nsessions.claude = on\n" + OTHERS_OFF),
        (b"sessions.codex = on\n", None, ["y"], b"sessions.codex = on\nsessions = on\nsessions.aider = off\nsessions.cline = off\nsessions.cursor = off\n"),
        (b"sessions = off\n", None, ["n"], b"sessions = off\n"),
        (b"sessions = on\n", None, ["y"], b"sessions = on\n"),
    ]
    for before, mode, answers, after in cases:
        c = Case(ctx, "sessions")
        c.put_config(before, mode)
        code, out, asked = c.tty(["install", "claude"], answers)
        ok(code == 0 and asked == 1, "chats %r %r: exit %r, %d questions\n%s" % (before, answers, code, asked, out))
        ok(same_rest(c, after), "chats %r %r: config %r" % (before, answers, c.config()))
        if mode is not None:
            m = stat.S_IMODE(os.stat(os.path.join(c.home, "config")).st_mode)
            ok(m == mode, "chats: mode %o" % m)
        if before == b"sessions = on\n":
            ok(("3. Your Claude Code chats: saved already (sessions = on in %s/config)." % c.home) in flat(out), "chats saved already:\n%s" % out)
        elif answers == ["n"]:
            ok(chats_said(c) in flat(out) and "will be saved" not in out, "chats, a no: said, not saved:\n%s" % out)
        else:
            ok(chats_said(c) in flat(out) and ("✓ Your Claude Code chats will be saved (sessions = on in %s/config).\n" % c.home) in out, "chats:\n%s" % out)
        ok(c.server() is not None or answers == ["n"], "chats: not registered")
        c.done()
    # Claude Code's folder not known: said, nothing saved (and no fewer settings, which is written there)
    c = Case(ctx, "sessions-none")
    env = c.env(FEWER_CLAUDE_DIR=None)
    code, out, asked = c.tty(["install", "claude"], ["y"], env=env)
    ok(code == 0 and asked == 1 and "Your Claude Code chats: not saved, as Claude Code's folder is not known (FEWER_CLAUDE_DIR)." in flat(out) and cfg_rest(c) is None, "chats, no folder: exit %r, %d questions, %r\n%s" % (code, asked, c.config(), out))
    c.done()


def check_uninstall(ctx):
    c = Case(ctx, "uninstall")
    # not registered
    code, out, asked = c.tty(["uninstall", "claude"], ["y"])
    ok(code == 0 and asked == 0 and out == "Claude Code has no MCP server named fewer and no fewer settings: nothing to remove.\n", "uninstall, not registered: %r\n%s" % (code, out))
    code, out, asked = c.tty(["install", "claude"], ["y", "y"])
    ok(code == 0 and c.server() is not None, "uninstall: install first")
    # no, then not a terminal: kept
    n = len(c.calls())
    code, out, asked = c.tty(["uninstall", "claude"], ["n"])
    # (fewer came on with install: its settings are listed too, under the same yes)
    # (a no: exit 1, so that a script keeps the programs Claude Code runs)
    ok(code == 1 and asked == 1 and ("Claude Code, user scope: remove the MCP server fewer once fewer is out (Claude Code shows no MCP server while it is on)\nThe command:\n  %s mcp remove --scope user fewer\nClaude Code's settings (" % c.stub) in out and (out.endswith("; a copy of the file is kept next to it.\nRemove it? [y/N] n\nNot removed. Nothing was changed.\n") or out.endswith("; the file goes, as fewer made it and nothing else is in it.\nRemove it? [y/N] n\nNot removed. Nothing was changed.\n")), "uninstall no:\n%s" % out)
    code, out, err = c.pipe(["uninstall", "claude"])
    ok(code == 1 and "not removed: stdin is not a terminal" in err, "uninstall, not a terminal: %r %r" % (code, err))
    ok(c.server() is not None and c.calls()[n:] == [GET, GET], "uninstall: kept, calls %r" % c.calls()[n:])
    # yes: removed; the store and its config stay
    code, out, asked = c.tty(["uninstall", "claude"], ["y", "n"])
    ok(code == 0 and asked == 2, "uninstall yes: %r %d" % (code, asked))
    # (fewer out first, then the server Claude Code shows again is removed)
    ok("Remove it? [y/N] y\nfewer is out of " in out and out.index("fewer is out of ") < out.index("\nRemoved MCP server fewer from user config\n") and "Removed from Claude Code.\nAlso delete what fewer saved in " in out and "? [y/N] n\nKept what fewer saved, in " in out, "uninstall yes:\n%s" % out)
    ok(c.calls()[n + 2:] == [GET, GET, REMOVE] and c.server() is None, "uninstall yes: calls %r" % c.calls()[n + 2:])
    ok(cfg_rest(c) == CHATS_ON and b"savers.lean = \n" in (c.config() or b""), "uninstall: config %r" % c.config())
    # installed again: asked again, the sessions question skipped (saved already), fewer on again
    code, out, asked = c.tty(["install", "claude"], ["y"])
    ok(code == 0 and asked == 1 and c.server() is not None and (c.settings() or {}).get("agent") == "fewer-lean", "install again: %r %d\n%s" % (code, asked, out))
    # a failing remove: the command to run by hand
    open(os.path.join(c.stub_dir, "fail-remove"), "w").close()
    code, out, err = c.pipe(["uninstall", "claude", "--yes"])
    ok(code == 1 and err.endswith("fewer uninstall: claude mcp remove failed (exit status 1). To remove fewer's MCP server yourself, run:\n  %s mcp remove --scope user fewer\n" % c.stub), "uninstall fails: %r\n%s" % (code, err))
    ok(c.server() is not None, "uninstall fails: removed")
    c.done()


def check_savers(ctx):
    # yes to both questions: fewer written in the made-up Claude Code folder by itself
    c = Case(ctx, "savers")
    code, out, asked = c.tty(["install", "claude"], ["y"])
    ok(code == 0 and asked == 1, "savers: exit %r, %d questions\n%s" % (code, asked, out))
    cfg = c.settings()
    hooks = (cfg or {}).get("hooks", {})
    # (the store named in the command: Claude Code runs hooks with its own environment, which
    # has no FEWER_HOME)
    post = "FEWER_HOME=%s %s hook post-bash" % (shlex.quote(c.home), shlex.quote(ctx.bin))
    start = "FEWER_HOME=%s %s hook start" % (shlex.quote(c.home), shlex.quote(ctx.bin))
    precompact = "FEWER_HOME=%s %s hook precompact" % (shlex.quote(c.home), shlex.quote(ctx.bin))
    ahead = "FEWER_HOME=%s %s hook ahead" % (shlex.quote(c.home), shlex.quote(ctx.bin))
    ok(cfg == {"hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": post}]}], "SessionStart": [{"hooks": [{"type": "command", "command": start}]}], "PreCompact": [{"hooks": [{"type": "command", "command": precompact}]}],
                         "UserPromptSubmit": [{"hooks": [{"type": "command", "command": ahead}]}]},
               # (Claude Code's own switches stay the user's: no workflows, Artifact or
               # session-title switch, 1.2.0; no autoCompactWindow, 1.2.1)
               "env": {"CLAUDE_CODE_ATTRIBUTION_ANNOUNCEMENT": "0", "CLAUDE_CODE_TOTAL_TOKENS_REMINDER": "off", "FEWER_HOME": c.home},
               "agent": "fewer-lean", "deniedMcpServers": [{"serverName": "fewer"}],
               **({"permissions": {"allow": read_rules(shlex.quote(ctx.bin))}} if read_rules(shlex.quote(ctx.bin)) else {})}, "savers: settings %r" % cfg)
    ok(stat.S_IMODE(os.stat(c.settings_file()).st_mode) == 0o600 and sorted(os.listdir(c.agent_dir("FEWER_CLAUDE_DIR"))) == ["agents", "commands", "settings.json"], "savers: a new file, mode 600, the agent's folder and the slash commands' folder")
    lean = open(os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), "agents", "fewer-lean.md")).read()
    ok(lean.startswith("---\nname: fewer-lean\n") and ("`%s changes [REV]`" % shlex.quote(ctx.bin)) in lean and "ending in ` 2>&1 || true`" in lean and "its tools, as usual (ToolSearch loads them)" in lean and "mcp list" not in lean
       and "\ndisallowedTools: mcp__fewer\n" in lean and "\ntools:" not in lean.split("\n---\n")[0] and "agentType: 'fewer-worker' for every" in lean and "Change files only with Edit and Write" in lean, "savers: the lean agent's file: %r" % lean[:200])
    wf = open(os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), "agents", "fewer-lean-workflow.md")).read()
    ok(wf.startswith("---\nname: fewer-lean-workflow\n") and "tools: Bash, Read, Edit, Write, Workflow, Skill\n" in wf and "agentType: 'fewer-worker'" in wf, "savers: the workflow agent's file: %r" % wf[:200])
    wk = open(os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), "agents", "fewer-worker.md")).read()
    ok(wk.startswith("---\nname: fewer-worker\n") and "tools: Bash, Read, Edit, Write\n" in wk and ("`%s changes [REV]`" % shlex.quote(ctx.bin)) in wk and "mcp list" not in wk, "savers: the worker's file: %r" % wk[:200])
    ok((TENX_ON + SLASH_ON + USE_LINE + "\n") in out, "savers: what was done:\n%s" % out)
    # the hook commands, run as Claude Code runs them: sh -c, the event on stdin, in the project,
    # with no FEWER_HOME or FEWER_PROJECT in the environment
    proj = os.path.join(c.base, "proj")
    os.makedirs(proj)
    with open(os.path.join(c.home, "config"), "a") as f:
        f.write("dna = off\n")
    lines = ["test t%03d ... ok" % i for i in range(200)] + ["test t200 ... FAILED", "test result: FAILED. 200 passed; 1 failed"]
    event = {"session_id": "s", "transcript_path": "/dev/null", "cwd": proj, "hook_event_name": "PostToolUse", "tool_name": "Bash",
             "tool_input": {"command": "cargo test"}, "tool_response": {"stdout": "\n".join(lines) + "\n", "stderr": "", "interrupted": False, "isImage": False}}
    env = safe_env.without(dict(c.env(), FEWER_TEST_NO_PACK="1"), "FEWER_HOME", "FEWER_PROJECT")
    r = subprocess.run(["/bin/sh", "-c", post], input=json.dumps(event).encode(), cwd=proj, env=env, capture_output=True, timeout=60)
    try:
        view = json.loads(r.stdout)["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
    except (ValueError, KeyError, TypeError):
        view = ""
    ok(r.returncode == 0 and view.startswith("cargo test: done; 202 lines saved as r1\n") and "test t200 ... FAILED" in view, "savers: the PostToolUse hook as written: %r %r %r" % (r.returncode, r.stdout[:300], r.stderr[:300]))
    r = subprocess.run(["/bin/sh", "-c", start], input=json.dumps({"session_id": "s2", "cwd": proj, "hook_event_name": "SessionStart", "source": "compact"}).encode(), cwd=proj, env=env, capture_output=True, timeout=60)
    try:
        card = json.loads(r.stdout)["hookSpecificOutput"]["additionalContext"]
    except (ValueError, KeyError, TypeError):
        card = ""
    ok(r.returncode == 0 and "cargo test" in card, "savers: the SessionStart hook as written: %r %r" % (r.stdout[:300], r.stderr[:300]))
    # installed again: on already, except two made-up skills now there, listed by name only (not
    # asked)
    for name, desc in (("verify", "Use when coding: run the checks."), ("ship", "Ship it.")):
        p = os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), "skills", name, "SKILL.md")
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").write("---\nname: %s\ndescription: %s\n---\nbody\n" % (name, desc))
    code, out, asked = c.tty(["install", "claude"], ["y"])
    ok(code == 0 and asked == 0 and "2 skills listed by name only (each still works)" in flat(out), "savers again: %r %d\n%s" % (code, asked, out))
    cfg = c.settings() or {}
    ok(cfg.get("skillOverrides") == {"ship": "name-only", "verify": "name-only"} and "enableArtifact" not in cfg and cfg.get("agent") == "fewer-lean", "the smaller start: %r" % cfg)
    # uninstall: the server and the savers under one yes; the settings.json that fewer made is
    # taken away, not left as {}; the copy of the second install's change stays
    code, out, asked = c.tty(["uninstall", "claude"], ["y", "n"])
    ok(code == 0 and asked == 2 and c.server() is None and c.settings() is None, "savers uninstall: %r %d %r\n%s" % (code, asked, c.settings(), out))
    ok("take out fewer (PostToolUse hook removed, SessionStart hook removed, PreCompact hook removed, UserPromptSubmit hook removed, 2 skills listed in full again, agent removed" in out and not any(os.path.exists(os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), "agents", f)) for f in ("fewer-lean.md", "fewer-lean-workflow.md", "fewer-worker.md")), "savers uninstall: shown:\n%s" % out)
    copies = [n for n in os.listdir(c.agent_dir("FEWER_CLAUDE_DIR")) if n.startswith("settings.json.before-fewer-")]
    ok(len(copies) == 1, "savers uninstall: copies %r" % copies)
    c.done()

    # the developer's own settings: kept exactly, numbers as written, the file's mode kept
    c = Case(ctx, "savers-theirs")
    os.makedirs(c.agent_dir("FEWER_CLAUDE_DIR"))
    theirs = '{\n  "model": "opus",\n  "cleanupPeriodDays": 30.0,\n  "statusLine": {"type": "command", "command": "echo \u00e9t\u00e9"},\n  "env": {"A": "1"},\n  "hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt"}]}]}\n}\n'
    with open(c.settings_file(), "w") as f:
        f.write(theirs)
    os.chmod(c.settings_file(), 0o644)
    code, out, asked = c.tty(["install", "claude"], ["y", "n", "y"])
    now = open(c.settings_file(), encoding="utf-8").read()
    ok(code == 0 and '"cleanupPeriodDays": 30.0' in now and stat.S_IMODE(os.stat(c.settings_file()).st_mode) == 0o644, "savers theirs: %r\n%s" % (code, now))
    cfg = json.loads(now)
    ok(cfg["model"] == "opus" and cfg["env"].get("A") == "1" and cfg["statusLine"]["command"] == "echo \u00e9t\u00e9" and cfg["hooks"]["PostToolUse"][0] == {"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt"}]} and len(cfg["hooks"]["PostToolUse"]) == 2, "savers theirs: %r" % cfg)
    copies = [n for n in os.listdir(c.agent_dir("FEWER_CLAUDE_DIR")) if n.startswith("settings.json.before-fewer-")]
    ok(len(copies) == 1 and open(os.path.join(c.agent_dir("FEWER_CLAUDE_DIR"), copies[0]), encoding="utf-8").read() == theirs, "savers theirs: the copy %r" % copies)
    # the MCP server removed by hand: uninstall still takes the savers out, theirs as before
    c.register(["mcp", "remove", "--scope", "user", "fewer"])
    code, out, asked = c.tty(["uninstall", "claude"], ["y", "n"])
    ok(code == 0 and asked == 2 and json.loads(open(c.settings_file(), encoding="utf-8").read()) == json.loads(theirs), "savers theirs, uninstall: %r %d\n%s" % (code, asked, out))
    c.done()

    # a settings file fewer cannot read: not turned on, left as it is
    c = Case(ctx, "savers-bad")
    os.makedirs(c.agent_dir("FEWER_CLAUDE_DIR"))
    with open(c.settings_file(), "w") as f:
        f.write("{ // mine\n}")
    code, out, asked = c.tty(["install", "claude"], ["y", "n", "y"])
    ok(code == 0 and asked == 1 and "fewer is not turned on: " in out and open(c.settings_file()).read() == "{ // mine\n}", "savers bad: %r %d\n%s" % (code, asked, out))
    c.done()


def check_update_adds_new_rules(ctx):
    """An install made before the reading commands' allow rules (this version's settings and record
    without them), beside the developer's own permissions, one of those rules among them: updated
    (`fewer install` again, as `fewer update` runs it), the other rules are added after theirs,
    recorded and said; `off` takes back only what it added."""
    c = Case(ctx, "update-rules")
    rules = read_rules(shlex.quote(ctx.bin))
    code, out, err = c.pipe(["install", "claude", "--yes"])
    ok(code == 0 and err == "", "update: the first install %r %r\n%s" % (code, err, out))
    if not rules:
        ok("permissions" not in (c.settings() or {}), "update: no rules for a path the rules cannot name: %r" % c.settings())
        c.done()
        return
    s = c.settings()
    theirs = {"allow": ["Bash(ls:*)", rules[2]], "deny": ["Read(./.env)"]}
    s["permissions"] = dict(theirs)
    open(c.settings_file(), "w").write(json.dumps(s, indent=2) + "\n")
    c.put_config(re.sub(r",lean:allow:[^,\n]*", "", (c.config() or b"").decode()).encode())
    code, out, err = c.pipe(["install", "claude", "--yes"])
    flat_out = flat(out)
    ok(code == 0 and err == "" and "fewer is on; this version changes it in" in flat_out and READS_LINE in flat_out and "Your add-ons" not in flat_out
       and "✓ fewer is brought up to this version (a copy of the file before this change: " in flat_out, "update: what the update said:\n%s" % out)
    ok(c.settings().get("permissions") == {"allow": theirs["allow"] + rules[:2] + rules[3:], "deny": theirs["deny"]}, "update: the rules added after theirs: %r" % c.settings().get("permissions"))
    code, out, err = c.pipe(["install", "claude", "--yes"])
    ok(code == 0 and ("fewer is on already in %s.\n" % c.settings_file()) in out, "update: a second update:\n%s" % out)
    code, out, err = c.pipe(["off"])
    ok(code == 0 and (c.settings() or {}).get("permissions") == theirs, "update: off takes back only fewer's rules: %r\n%s" % (c.settings(), out))
    c.done()


WINDOW_BACK = "✓ Took out Fewer's old autoCompactWindow line, so long chats compact as normal again. Restart Claude Code once: quit VSCode or the terminal fully, then open it again (/clear is not enough)."


def restarts(out):
    """How many times `out` says to restart Claude Code (its wrapped lines joined)."""
    return flat(out).count("Restart Claude Code")


def as_1_2_0(c, window=100000, claimed=True):
    """This version's install in case c made into what 1.0.0 to 1.2.0 left there (savers.rs at
    v1.2.0): the line `"autoCompactWindow": window` right after fewer's hooks and, when claimed,
    "lean:window" last in fewer's record (claimed=False: a window the developer set before fewer,
    kept and not recorded). The settings' text as this version wrote it."""
    text = open(c.settings_file(), encoding="utf-8").read()
    old = {}
    for k, v in json.loads(text).items():
        old[k] = v
        if k == "hooks":
            old["autoCompactWindow"] = window
    open(c.settings_file(), "w", encoding="utf-8").write(json.dumps(old, indent=2) + "\n")
    if claimed:
        c.put_config(re.sub(rb"(?m)^(savers\.lean = .+)$", rb"\1,lean:window", c.config()))
    return text


def check_old_window(ctx):
    """fewer's own old autoCompactWindow line (1.0.0 to 1.2.0 wrote 100000, so Claude Code
    compacted near 67,000 tokens) in a home as 1.2.0 left it: the update (`fewer install` again,
    as `fewer update` runs it) and `fewer on` take it out, say WINDOW_BACK once and keep every
    other setting and fewer's record but its claim; a second update says nothing of it; `off`
    and uninstall take it out with the rest and say it once; a window the developer set before
    fewer or changed since is kept and never mentioned."""
    runs = {"update": ["install", "claude", "--yes"], "on": ["on"], "off": ["off"], "uninstall": ["uninstall", "claude", "--yes"]}
    for way in ("update", "on", "off", "uninstall"):
        c = Case(ctx, "old-window-" + way)
        os.makedirs(c.agent_dir("FEWER_CLAUDE_DIR"))
        open(c.settings_file(), "w").write('{\n  "model": "opus"\n}\n')
        code, out, err = c.pipe(runs["update"])
        ok(code == 0 and "autoCompactWindow" not in out and restarts(out) == 0 and "autoCompactWindow" not in (c.settings() or {}), "old window, %s: the fresh install %r\n%s" % (way, code, out))
        record = c.config()
        mine = as_1_2_0(c)
        code, out, err = c.pipe(runs[way])
        said = flat(out).count(WINDOW_BACK) == 1 and restarts(out) == 1
        if way in ("update", "on"):
            ok(code == 0 and err == "" and said and out.count("autoCompactWindow") == 1, "old window, %s: said once:\n%s%s" % (way, out, err))
            ok(open(c.settings_file(), encoding="utf-8").read() == mine and c.config() == record, "old window, %s: every other setting and the record kept: %r %r" % (way, c.settings(), c.config()))
            if way == "on":
                ok(flat(out) == "✓ Fewer is on. To turn it off: fewer off " + WINDOW_BACK, "old window, on: %r" % out)
            code, out, err = c.pipe(runs["update"])
            ok(code == 0 and ("fewer is on already in %s.\n" % c.settings_file()) in out and "autoCompactWindow" not in out and restarts(out) == 0, "old window, %s: a second update:\n%s" % (way, out))
            code, out, err = c.pipe(["off"])
            ok(code == 0 and out == "✓ Fewer is off. To turn it on: fewer on\n" and c.settings() == {"model": "opus"}, "old window, %s: off then: %r\n%s" % (way, c.settings(), out))
        else:
            ok(code == 0 and said and c.settings() == {"model": "opus"}, "old window, %s: taken out and said once: %r\n%s%s" % (way, c.settings(), out, err))
            if way == "off":
                ok(flat(out) == "✓ Fewer is off. To turn it on: fewer on " + WINDOW_BACK, "old window, off: %r" % out)
            code, out, err = c.pipe(runs[way])
            ok(code == 0 and "autoCompactWindow" not in out and restarts(out) == 0, "old window, %s: again:\n%s" % (way, out))
        c.done()
    # the developer's own window: set before fewer (at fewer's old value), or changed since
    for name, theirs in (("own-window", 100000), ("changed-window", 400000)):
        c = Case(ctx, name)
        os.makedirs(c.agent_dir("FEWER_CLAUDE_DIR"))
        open(c.settings_file(), "w").write('{\n  "model": "opus"%s\n}\n' % (',\n  "autoCompactWindow": 100000' if name == "own-window" else ""))
        outs = [c.pipe(runs["update"])]
        if name == "changed-window":
            as_1_2_0(c, window=theirs)
        outs.append(c.pipe(runs["update"]))
        ok((c.settings() or {}).get("autoCompactWindow") == theirs and b"lean:window" not in (c.config() or b"") and b"savers.lean = lean:agent," in (c.config() or b""), "%s: kept, and fewer's claim to it out of the record: %r %r" % (name, c.settings(), c.config()))
        for args in (runs["on"], runs["update"], ["off"]):
            outs.append(c.pipe(args))
        ok(all(r[0] == 0 and "autoCompactWindow" not in r[1] + r[2] and restarts(r[1]) == 0 for r in outs), "%s: never mentioned: %r" % (name, outs))
        ok(c.settings() == {"model": "opus", "autoCompactWindow": theirs}, "%s: kept by off: %r" % (name, c.settings()))
        code, out, err = c.pipe(runs["uninstall"])
        ok(code == 0 and "autoCompactWindow" not in out and c.settings() == {"model": "opus", "autoCompactWindow": theirs}, "%s: uninstall: %r\n%s" % (name, c.settings(), out))
        c.done()


def check_install_places(ctx):
    """What install.sh hands `fewer install` (the folder fewer was built from, the one it is in,
    the file it added the PATH line to) is recorded once fewer is in Claude Code; `fewer
    uninstall --yes` then removes the two programs there and that PATH line (only fewer's own
    lines; another program and the developer's lines stay) and keeps what fewer saved; `fewer
    update` with nothing recorded says what to do."""
    c = Case(ctx, "places")
    bindir = os.path.join(c.base, "prefix", "bin")
    os.makedirs(bindir)
    for n in ("fewer", "fewer-pack", "other"):
        open(os.path.join(bindir, n), "w").write("#!/bin/sh\n")
    rc = os.path.join(c.base, "zshrc")
    mine = "alias ll='ls -l'\n"
    open(rc, "w").write(mine + "\n# fewer: its programs on PATH (fewer uninstall takes this out)\nexport PATH=\"%s:$PATH\"\n" % bindir)
    code, out, err = c.pipe(["update"])
    ok(code == 1 and "where Fewer came from is not known" in err, "update, nothing recorded: %r %r" % (code, err))
    code, out, err = c.pipe(["install", "claude", "--yes"], env=c.env(FEWER_FROM=c.base, FEWER_BIN_DIR=bindir, FEWER_PATH_FILE=rc))
    cfg = (c.config() or b"").decode()
    ok(code == 0 and ("install.from = %s\n" % c.base) in cfg and ("install.bin = %s\n" % bindir) in cfg and ("install.path_file = %s\n" % rc) in cfg, "places recorded: %r\n%s" % (cfg, out))
    # (the ready-made programs' installer records "release" as where Fewer came from)
    code, out, err = c.pipe(["install", "claude", "--yes"], env=c.env(FEWER_FROM="release", FEWER_BIN_DIR=bindir, FEWER_PATH_FILE=rc))
    ok(code == 0 and "install.from = release\n" in (c.config() or b"").decode(), "places recorded for the ready-made programs: %r" % c.config())
    code, out, err = c.pipe(["uninstall", "claude", "--yes"])
    ok(code == 0 and sorted(os.listdir(bindir)) == ["other"] and open(rc).read() == mine, "uninstall: the programs and the PATH line out: %r %r %r\n%s" % (code, os.listdir(bindir), open(rc).read(), out))
    ok("✓ Removed fewer and fewer-pack from " in out and "✓ Took fewer's PATH line out of " in out and "Kept what fewer saved, in " in out and os.path.exists(c.home), "uninstall says it, and keeps the data with --yes:\n%s" % out)
    c.done()


def check_add_fails(ctx):
    c = Case(ctx, "add-fails")
    open(os.path.join(c.stub_dir, "fail-add"), "w").close()
    code, out, asked = c.tty(["install", "claude"], ["y", "y"])
    ok(code == 1 and asked == 1, "add fails: exit %r, %d questions" % (code, asked))
    want = "Register fewer and turn it on (the changes above)? [y/N] y\nError: cannot write %s/.claude.json (claude-stub was told to fail)\nfewer install: claude mcp add failed (exit status 1). To register fewer yourself, run:\n  %s\n" % (c.stub_dir, shlex.join([c.stub] + add_call(c, ctx.bin)))
    ok(out.endswith(want), "add fails:\n%s\nwanted the end\n%s" % (out, want))
    ok(c.server() is None and c.config() is None, "add fails: state")
    c.done()


def check_misc(ctx):
    c = Case(ctx, "misc")
    # no store: exit 2, claude not run
    code, out, err = c.pipe(["install", "claude", "--yes"], env=c.env(FEWER_HOME=None))
    ok(code == 2 and "FEWER_HOME is not set" in err and c.calls() == [], "no store: %r %r" % (code, err))
    code, out, err = c.pipe(["install", "claude", "--yes"], env=c.env(FEWER_HOME="relative/home"))
    ok(code == 2 and "FEWER_HOME must be an absolute path" in err and c.calls() == [], "relative store: %r %r" % (code, err))
    # usage errors
    for args in (["install", "codex"], ["install", "claude", "--force"], ["uninstall", "claude", "--yes", "x"]):
        code, out, err = c.pipe(args)
        ok(code == 2 and "usage: fewer " in err, "usage %r: %r %r" % (args, code, err))
    ok(c.calls() == [], "usage: claude run: %r" % c.calls())
    # FEWER_CLAUDE_BIN a name, found on PATH: printed as `claude`
    path = c.stub_dir + ":" + SYSTEM_PATH
    code, out, asked = c.tty(["install", "claude"], ["y", "n"], env=c.env(FEWER_CLAUDE_BIN="claude", PATH=path))
    ok(code == 0 and ("\n       %s\n" % shlex.join(["claude"] + add_call(c, ctx.bin))) in out and c.server() is not None, "PATH: %r\n%s" % (code, out))
    c.done()


def check_registered_command_serves(ctx):
    probe = subprocess.run([ctx.bin, "mcp"], input=b"", stdout=subprocess.PIPE, stderr=subprocess.PIPE, env={"PATH": SYSTEM_PATH}, timeout=60)
    if probe.returncode == 2 and probe.stdout == b"":
        print("check_registered_command_serves: skipped (%s has no mcp command: a test program)" % os.path.basename(ctx.bin), flush=True)
        return
    c = Case(ctx, "serves")
    code, out, asked = c.tty(["install", "claude"], ["y", "n"])
    srv = c.server()
    ok(code == 0 and srv is not None, "serves: install %r\n%s" % (code, out))
    if srv is None:
        return
    env = {"PATH": SYSTEM_PATH, "HOME": c.fakehome}
    for kv in srv["env"]:
        k, v = kv.split("=", 1)
        env[k] = v
    proj = os.path.join(c.base, "proj")
    os.makedirs(proj)
    msgs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "check_install", "version": "0"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    ]
    data = "".join(json.dumps(m) + "\n" for m in msgs).encode()
    r = subprocess.run([srv["command"]] + srv["args"], input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=proj, timeout=60)
    ok(r.returncode == 0, "serves: the server's exit %r: %r" % (r.returncode, r.stderr[-500:]))
    try:
        lines = [json.loads(line) for line in r.stdout.decode().split("\n") if line.strip()]
    except ValueError as e:
        lines = []
        ok(False, "serves: not JSON: %r %r" % (e, r.stdout[:500]))
    ok(len(lines) == 2 and lines[0].get("id") == 1 and lines[0].get("result", {}).get("serverInfo", {}).get("name") == "fewer", "serves: initialize %r" % lines[:1])
    tools = [x.get("name") for x in lines[1].get("result", {}).get("tools", [])] if len(lines) == 2 else []
    ok("logs" in tools and "search" in tools, "serves: tools %r" % tools)
    c.done()


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    binary, work = (os.path.abspath(a) for a in sys.argv[1:3])
    if not os.path.exists(STUB):
        sys.exit("no stub at %s" % STUB)
    for d in SYSTEM_PATH.split(":"):
        if os.path.exists(os.path.join(d, "claude")):
            sys.exit("refusing to run: %s/claude exists, and the checks would find it" % d)
    fresh(work)
    # every process this check starts gets a made-up HOME, agents' folders and claude, inside
    # WORK (safe_env.py)
    safe = safe_env.setup(work)
    ctx = Ctx(binary, work)
    checks = [check_yes, check_no, check_dry_run, check_already, check_missing, check_claude_bin_required, check_not_terminal, check_exact_args, check_sessions_config, check_uninstall, check_savers, check_update_adds_new_rules, check_old_window, check_install_places, check_add_fails, check_misc, check_registered_command_serves]
    only = safe.caller.get("CHECK_ONLY")
    for c in checks:
        if only and only not in c.__name__:
            continue
        n0, f0 = PASSES[0], len(FAILS)
        try:
            c(ctx)
        except Exception as e:
            ok(False, "%s raised %r" % (c.__name__, e))
        print("%-24s %3d passed, %d failed" % (c.__name__, PASSES[0] - n0, len(FAILS) - f0), flush=True)
    print("all passed (%d checks)" % PASSES[0] if not FAILS else "%d FAILED of %d" % (len(FAILS), PASSES[0] + len(FAILS)))
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
