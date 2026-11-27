"""safe_env.py: the environment every process a check starts gets (the privacy rule of the checks).

A rule since a check once started the MCP server with the real HOME (and its sessions sweep
read the real ~/.claude): every process a check starts, the MCP
server included, gets a made-up HOME, made-up agents' folders, a store and the claude stub,
all inside the check's scratch folder. Every check in this folder calls, once, before it
starts any process (python3 -I does not put this folder on the import path):

    spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
    safe_env = importlib.util.module_from_spec(spec); spec.loader.exec_module(safe_env)
    safe = safe_env.setup(WORK)

From then on:
- the check's own environment (os.environ) is the safe one, built from nothing, so a process
  started without env= inherits it; `safe.caller` keeps the caller's environment for the
  check's own switches (CHECK_ONLY, FEWER_SAMPLES, ...), which are not passed on;
- `safe.env(**changes)` gives a copy for one process; a value of None removes that setting
  on purpose (a check of what happens without it), as `safe_env.without(env, KEY, ...)` does
  for a copy of another environment (deleting a key by hand is not enough: the setting would
  be given its safe value back);
- every process the check starts, through subprocess or os.execve (the child of pty.fork),
  goes through `safe.final` first: a safe setting the environment lacks, and that was not
  removed on purpose, is given its safe value; then HOME must lie inside WORK, as must
  FEWER_HOME, the agents' folders and the other folder settings below when they are absolute
  paths (a relative one is refused by the programs themselves), FEWER_CLAUDE_BIN must be the
  stub or another program inside WORK (or a name, looked up on the safe PATH), FEWER_OPEN_BIN
  (what opens a page in the browser) when it is set a program inside WORK, never a name (the
  system's `open` is on the safe PATH: no check opens a real browser), and set so whenever a
  check removes FEWER_DEV (fewer would fall back to the system's opener), PATH may hold
  only the system's folders and folders inside WORK, and no program named claude outside
  WORK is ever started. Anything else stops the check (SafeEnvError) before the process
  starts.

The safe environment:
- HOME: WORK/safe-env/home, a made-up home folder, empty;
- the agents' folders, made up and empty, in WORK/safe-env/agents: FEWER_CLAUDE_DIR,
  FEWER_CODEX_DIR, FEWER_AIDER_DIR, FEWER_CLINE_DIR and FEWER_CURSOR_DIR (in development an agent's
  sessions are read only from its setting, so these are the only folders a sweep can read);
- FEWER_HOME: WORK/safe-env/store (a check gives its own stores, also in WORK);
- FEWER_MANAGED_DIR: WORK/safe-env/managed (an organization's Claude Code settings, made up and
  empty: never the system's own);
- FEWER_CLAUDE_BIN: ../tests/data/claude-stub copied to WORK/safe-env/bin/claude (it keeps
  its state beside it), so `install` can only ever run the stub;
- PATH /usr/bin:/bin:/usr/sbin:/sbin, LANG en_US.UTF-8, TMPDIR WORK/safe-env/tmp, git without
  the developer's settings (GIT_CONFIG_GLOBAL=/dev/null, GIT_CONFIG_NOSYSTEM=1), and the
  caller's TZ when it has one (so the check and its processes read times in one zone).

WORK must be a scratch folder of its own: not the real home or a folder holding it, and not
inside the real agents' folders. Without WORK (a check that has no scratch folder of its own),
the WORK of the check that started this one is used (SAFE_ENV_WORK), else a folder under the
crate's target/check-tmp/, as the unit tests write under target/test-tmp/.
"""
import os
import pwd
import shutil
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
STUB = os.path.normpath(os.path.join(HERE, "..", "tests", "data", "claude-stub"))

# the agents and their folder settings (sessions.rs's SETTINGS)
AGENTS = [("claude", "FEWER_CLAUDE_DIR"), ("codex", "FEWER_CODEX_DIR"), ("aider", "FEWER_AIDER_DIR"), ("cline", "FEWER_CLINE_DIR"), ("cursor", "FEWER_CURSOR_DIR")]
# settings naming a folder or file that must lie inside WORK when they are absolute paths
FOLDERS = ["FEWER_HOME", "FEWER_PROJECT", "FEWER_DEBUG_LOG", "FEWER_CLAUDE_JSON", "FEWER_HELPER_SETTINGS", "FEWER_MANAGED_DIR", "TMPDIR", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "CFFIXED_USER_HOME"] + [s for _, s in AGENTS]
SYSTEM_PATH = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
# what a check removed on purpose (safe.env(KEY=None)); taken out before the process starts
UNSET = "SAFE_ENV_UNSET"
# the WORK of the check that set this environment up, for checks it starts
WORK_VAR = "SAFE_ENV_WORK"
REAL_HOME = os.path.realpath(pwd.getpwuid(os.getuid()).pw_dir)
# the real agents' folders (never a check's WORK, never read)
REAL_AGENTS = [os.path.join(REAL_HOME, p) for p in (".claude", ".claude.json", ".codex", ".cursor", ".vscode", "Library/Application Support/Code", "Library/Application Support/Cursor", ".config/Code", ".config/Cursor")]


class SafeEnvError(Exception):
    pass


def inside(path, folder):
    """Whether `path` (made absolute and resolved, as far as it exists) lies in `folder`."""
    p = os.path.realpath(path)
    return p == folder or p.startswith(folder.rstrip("/") + "/")


class Safe:
    def __init__(self, work, caller):
        self.work = work
        self.caller = caller
        self.root = os.path.join(work, "safe-env")
        self.home = os.path.join(self.root, "home")
        self.store = os.path.join(self.root, "store")
        self.agents = {a: os.path.join(self.root, "agents", a) for a, _ in AGENTS}
        self.claude_bin = os.path.join(self.root, "bin", "claude")
        self.tmp = os.path.join(self.root, "tmp")
        # an organization's Claude Code settings: a made-up folder, never the system's own
        self.managed = os.path.join(self.root, "managed")
        for d in [self.home, self.store, self.tmp, self.managed, os.path.dirname(self.claude_bin)] + list(self.agents.values()):
            os.makedirs(d, exist_ok=True)
        shutil.copyfile(STUB, self.claude_bin)
        os.chmod(self.claude_bin, 0o755)
        self.base = {
            "PATH": ":".join(SYSTEM_PATH),
            "HOME": self.home,
            "LANG": "en_US.UTF-8",
            "TMPDIR": self.tmp,
            "FEWER_HOME": self.store,
            # (development: fewer's defaults off, every setting from its variable only, so that
            # nothing falls back to the real home folder or the real claude)
            "FEWER_DEV": "1",
            "FEWER_CLAUDE_BIN": self.claude_bin,
            "FEWER_MANAGED_DIR": self.managed,
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            WORK_VAR: work,
        }
        for a, s in AGENTS:
            self.base[s] = self.agents[a]
        if caller.get("TZ"):
            self.base["TZ"] = caller["TZ"]

    def env(self, **changes):
        """The safe environment with these changes for one process: a value of None removes
        that setting on purpose."""
        e = dict(os.environ)
        for k, v in changes.items():
            if v is not None:
                e[k] = str(v)
        return without(e, *[k for k, v in changes.items() if v is None])

    def final(self, env, what, cwd=None):
        """The environment a process gets: `env` (None: os.environ) with every safe setting it
        lacks and was not removed on purpose; checked (the module's doc)."""
        e = dict(os.environ if env is None else env)
        unset = set(filter(None, e.pop(UNSET, "").split(",")))
        for k, v in self.base.items():
            if k not in e and k not in unset:
                e[k] = v
        self.check(e, what, cwd)
        return e

    def check(self, e, what, cwd=None):
        def stop(why):
            raise SafeEnvError("safe_env: refusing to start %r: %s" % (what, why))

        home = e.get("HOME", "")
        if not home or not os.path.isabs(home) or not inside(home, self.work):
            stop("HOME=%r is not a folder inside %s" % (home, self.work))
        for k in FOLDERS:
            v = e.get(k, "")
            if v and os.path.isabs(v) and not inside(v, self.work):
                stop("%s=%r is not inside %s" % (k, v, self.work))
        path = e.get("PATH", "")
        for d in path.split(":") if path else []:
            if d not in SYSTEM_PATH and not (os.path.isabs(d) and inside(d, self.work)):
                stop("PATH holds %r: only %s and folders inside %s" % (d, ", ".join(SYSTEM_PATH), self.work))
        cb = e.get("FEWER_CLAUDE_BIN", "")
        if "/" in cb:
            p = cb if os.path.isabs(cb) else os.path.join(cwd or os.getcwd(), cb)
            if not inside(p, self.work):
                stop("FEWER_CLAUDE_BIN=%r is not inside %s" % (cb, self.work))
        ob = e.get("FEWER_OPEN_BIN", "")
        ob_inside = "/" in ob and inside(ob if os.path.isabs(ob) else os.path.join(cwd or os.getcwd(), ob), self.work)
        if ob and not ob_inside:
            stop("FEWER_OPEN_BIN=%r is not a program inside %s (no check opens a real browser)" % (ob, self.work))
        if not e.get("FEWER_DEV") and not ob_inside:
            stop("FEWER_DEV is removed and FEWER_OPEN_BIN is not a program inside %s: fewer would open pages with the system's own opener (no check opens a real browser)" % self.work)
        prog = what[0] if isinstance(what, (list, tuple)) and what else what
        if isinstance(prog, (str, bytes, os.PathLike)):
            prog = os.fsdecode(prog)
            if os.path.basename(prog) == "claude" and not ("/" in prog and inside(prog, self.work)):
                stop("only the stub may run as claude")
            if "/" in prog and any(inside(prog, a) for a in REAL_AGENTS):
                stop("a program inside the real agents' folders")


def without(env, *keys):
    """A copy of `env` without these settings, removed on purpose: the process starts without
    them (they are not given their safe values back)."""
    e = {k: v for k, v in env.items() if k not in keys}
    if keys:
        e[UNSET] = ",".join(sorted(set(keys) | set(filter(None, env.get(UNSET, "").split(",")))))
    return e


STATE = [None]


def setup(work=None, label="check"):
    """Make WORK's safe environment, make it this check's own, and guard every process start
    (the module's doc). `work` None: the starting check's WORK (SAFE_ENV_WORK), else a
    folder under the crate's target/check-tmp/. Returns the Safe."""
    caller = dict(os.environ)
    if work is None:
        work = caller.get(WORK_VAR) or os.path.join(HERE, "..", "target", "check-tmp", "%s-%d" % (label, os.getpid()))
    os.makedirs(work, exist_ok=True)
    work = os.path.realpath(work)
    if work == "/" or work == REAL_HOME or REAL_HOME.startswith(work.rstrip("/") + "/"):
        raise SafeEnvError("safe_env: %s is the real home or holds it: give a scratch folder of its own" % work)
    if any(inside(work, a) for a in REAL_AGENTS):
        raise SafeEnvError("safe_env: %s is inside the real agents' folders" % work)
    safe = Safe(work, caller)
    os.environ.clear()
    os.environ.update(safe.base)
    if STATE[0] is None:
        _guard()
    STATE[0] = safe
    return safe


def _guard():
    """Every process start goes through the safe environment's `final`."""
    popen_init = subprocess.Popen.__init__

    def init(self, args, *rest, **kw):
        if len(rest) > 9:
            raise SafeEnvError("safe_env: pass env to subprocess by name")
        kw["env"] = STATE[0].final(kw.get("env"), args, kw.get("cwd"))
        popen_init(self, args, *rest, **kw)

    subprocess.Popen.__init__ = init
    execve = os.execve

    def guarded_execve(path, args, env):
        execve(path, args, STATE[0].final(env, [path] + list(args[1:])))

    os.execve = guarded_execve
