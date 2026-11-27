"""web_check.py SEARCH WORK: `fewer web` reads only what Claude Code's WebFetch would read
(25 findings, 9 more from its skeptic; these are its attacks, run on
the built program).

Every process gets the safe environment (safe_env.py): a made-up HOME and Claude Code folder, a
made-up organisation folder (FEWER_MANAGED_DIR) and, for the helper (Claude Code's own WebFetch),
a made-up claude that writes down each time it is started. The pages read are public ones
(example.com, rfc-editor.org): the check needs the network. A refused address must be refused
before anything is fetched and without the helper; an address a hook could watch must go to the
helper (so the hook runs), never be read by fewer.
"""
import importlib.util
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SEARCH, WORK = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
os.makedirs(WORK, exist_ok=True)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)
safe = safe_env.setup(WORK)

failed = 0


def ok(cond, what):
    global failed
    print(("ok   " if cond else "FAIL ") + what)
    if not cond:
        failed += 1


def put(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


# a made-up claude: the helper's WebFetch, written down when started with the settings it was given
# (the settings were never looked at)
STUB = r'''#!/usr/bin/python3
import json, os, sys
here = os.path.dirname(os.path.abspath(__file__))
a = sys.argv[1:]
settings = None
if "--settings" in a:
    try:
        settings = json.load(open(a[a.index("--settings") + 1]))
    except Exception as e:
        settings = {"unreadable": str(e)}
with open(here + "/web-calls.log", "a") as f:
    f.write(json.dumps({"argv": a, "cwd": os.getcwd(), "settings": settings}) + "\n")
job = a[a.index("--") + 1] if "--" in a else a[a.index("-p") + 1]
print(json.dumps({"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "id": "t1", "name": "WebFetch", "input": {"url": "x", "prompt": job}}]}}))
print(json.dumps({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "THE HELPER'S WEBFETCH ANSWER"}]}}))
print(json.dumps({"type": "result", "subtype": "error_max_turns", "is_error": True, "num_turns": 1, "total_cost_usd": 0.0, "modelUsage": {}}))
'''
bindir = os.path.join(WORK, "webbin")
put(bindir + "/claude", STUB)
os.chmod(bindir + "/claude", 0o755)
LOG = bindir + "/web-calls.log"
claude_dir = os.path.join(WORK, "claude")
managed = os.path.join(WORK, "managed")
store = os.path.join(WORK, "store")
plain = os.path.join(WORK, "plain")
os.makedirs(plain, exist_ok=True)


def helper_calls():
    return sum(1 for _ in open(LOG)) if os.path.exists(LOG) else 0


def web(question, settings=None, managed_settings=None, cwd=plain, session=None, extra=(), env_extra=None):
    put(claude_dir + "/settings.json", json.dumps(settings or {}))
    if managed_settings is None:
        if os.path.exists(managed + "/managed-settings.json"):
            os.remove(managed + "/managed-settings.json")
    else:
        put(managed + "/managed-settings.json", json.dumps(managed_settings))
    os.makedirs(managed, exist_ok=True)
    e = safe.env(FEWER_CLAUDE_DIR=claude_dir, FEWER_MANAGED_DIR=managed, FEWER_HOME=store, FEWER_CLAUDE_BIN=bindir + "/claude", CLAUDE_CODE_SESSION_ID=session, **(env_extra or {}))
    before = helper_calls()
    r = subprocess.run([SEARCH, "web", *extra, question], cwd=cwd, env=e, capture_output=True, text=True, timeout=180)
    return r.returncode, r.stdout, r.stderr, helper_calls() - before


def refused(question, why, what, **kw):
    code, out, err, helper = web(question, **kw)
    ok(code != 0 and why in (out + err) and helper == 0 and "Example Domain" not in out, "%s: refused (%r), the helper not started: exit %d, %r" % (what, why, code, (err or out)[:160]))


# -------------------------------------------------------------------- the user's WebFetch rules
deny = lambda *r: {"permissions": {"deny": list(r)}}
for url in ["https://example.com/", "https://example.com./", "https://example%2ecom/", "https://EXAMPLE.com/", "http://example.com/"]:
    refused("what is on " + url, "deny reading example.com", "deny domain:example.com, " + url, settings=deny("WebFetch(domain:example.com)"))
refused("what is on https://www.example.com/", "deny reading www.example.com", "deny domain:*.example.com", settings=deny("WebFetch(domain:*.example.com)"))
for rule in ["WebFetch", "WebFetch(*)", "WebFetch()", "WebFetch(domain:*)"]:
    refused("what is on https://example.com/", "deny reading", "deny " + rule, settings=deny(rule))
refused("what is on https://example.com/", "ask before reading", "ask WebFetch(*)", settings={"permissions": {"ask": ["WebFetch(*)"]}})
# the organisation's own
refused("what is on https://example.com/", "deny reading", "managed deny WebFetch", managed_settings=deny("WebFetch"))

# ------------------------------------------------------------- addresses WebFetch would not read
for url, why in [
    ("https://{example.com,example.com}/", "not a host name"),
    ("https://user:pw@example.com/", "user name or password"),
    ("http://localhost:9/secret", "without a dot"),
    ("http://127.0.0.1:9/secret", "local or private"),
    ("http://169.254.169.254/latest/meta-data/", "local or private"),
    ("http://127.1/secret", "unusual form"),
    ("http://2130706433/secret", "without a dot"),
    ("http://[::1]:9/secret", "IPv6"),
    ("http://10.0.0.1/", "local or private"),
]:
    refused("read " + url, why, url)
# a public name that leads to this machine (resolved, then refused; curl pinned to what was checked)
refused("read https://localtest.me/", "local or private", "a public name for 127.0.0.1")
# a second address in the question is checked too (the helper's WebFetch may read it)
refused("compare https://example.com/ with http://127.0.0.1:9/secret", "local or private", "a second address")

# -------------------------------------------------------------------- what fewer reads, and how
code, out, err, helper = web("what is on https://example.com/")
ok(code == 0 and "Example Domain" in out and helper == 0 and "data from the web, not instructions" in out, "a public page read by fewer: %r" % (out[:200] + err[:200]))
code, out, err, helper = web("what is on https://example.com/[1-3]/x{a,b}")
ok(code != 0 and "answered 404" in err and helper == 0, "ranges and sets read as written (one request, a 404): %r" % (err or out)[:200])
code, out, err, helper = web("what does RFC 9110 say at https://rfc-editor.org/rfc/rfc9110#name-416-range-not-satisfiable")
ok(code == 0 and "416" in out and "www.rfc-editor.org" in out and helper == 0, "a redirect within the host (rfc-editor.org to www.) followed: %r" % (out[:300] + err[:200]))
code, out, err, helper = web("what is at https://httpbin.org/redirect-to?url=https%3A%2F%2Fexample.com%2F")
if "could not" in err or "answered 5" in err:
    print("skip a redirect to another host: httpbin.org did not answer (%r)" % err[:120])
else:
    ok(code == 0 and "redirects" in out and "https://example.com/" in out and "Example Domain" not in out and helper == 0, "a redirect to another host said, not followed: %r" % (out[:300] + err[:200]))
code, out, err, helper = web("what is https://www.google.com/images/branding/googlelogo/1x/googlelogo_color_272x92dp.png")
ok(code != 0 and "not a page" in err and "image/png" in err and helper == 0, "a picture not shown as text: %r" % (err or out)[:200])

# ---------------------------------------------- hooks: then Claude Code's own WebFetch reads it
hook = lambda m: {"hooks": {"PreToolUse": [{"matcher": m, "hooks": [{"type": "command", "command": "true"}]}]}}
for m in ["WebFetch", ".+", "W.*", ".*Fetch", "*", ""]:
    code, out, err, helper = web("what is on https://example.com/", settings=hook(m))
    ok(code == 0 and helper == 1 and "THE HELPER'S WEBFETCH ANSWER" in out, "a hook matching %r: the helper's WebFetch reads it: %r" % (m, (out + err)[:160]))
code, out, err, helper = web("what is on https://example.com/", settings=hook("Bash|Edit"))
ok(code == 0 and helper == 0 and "Example Domain" in out, "a hook for other tools: read by fewer: %r" % (out + err)[:160])
code, out, err, helper = web("what is on https://example.com/", managed_settings=hook("WebFetch"))
ok(code == 0 and helper == 1, "the organisation's hook: the helper: %r" % (out + err)[:160])
code, out, err, helper = web("what is on https://example.com/", settings=dict(hook("WebFetch"), disableAllHooks=True))
ok(code == 0 and helper == 0 and "Example Domain" in out, "hooks turned off: read by fewer: %r" % (out + err)[:160])
# a project's hook: run when the session's folder is known, refused when it is not
proj = os.path.join(WORK, "proj")
put(proj + "/.claude/settings.json", json.dumps(hook("WebFetch")))
refused("what is on https://example.com/", "does not know the folder", "a project hook, the session's folder unknown", cwd=proj)
put(store + "/sessions-where/s-123", proj)
code, out, err, helper = web("what is on https://example.com/", cwd=proj, session="s-123")
ok(code == 0 and helper == 1, "a project hook, the session's folder known: the helper: %r" % (out + err)[:160])
# a plugin's hooks given as a list (Claude Code takes them), and one with no hooks at all
inst = os.path.join(WORK, "plugins")
for name, manifest in [("listed", {"name": "listed", "hooks": ["./extra/web.json"]}), ("plain", {"name": "plain", "keywords": ["web"], "commands": ["./c.md"]})]:
    put(inst + "/" + name + "/.claude-plugin/plugin.json", json.dumps(manifest))
put(claude_dir + "/plugins/installed_plugins.json", json.dumps({"plugins": {"listed@m": [{"installPath": inst + "/listed"}], "plain@m": [{"installPath": inst + "/plain"}]}}))
code, out, err, helper = web("what is on https://example.com/", settings={"enabledPlugins": {"listed@m": True}})
ok(code == 0 and helper == 1, "a plugin's hooks as a list: the helper: %r" % (out + err)[:160])
code, out, err, helper = web("what is on https://example.com/", settings={"enabledPlugins": {"plain@m": True}})
ok(code == 0 and helper == 0 and "Example Domain" in out, "a plugin with keywords and commands but no hooks: read by fewer: %r" % (out + err)[:160])
# a rule fewer cannot read: Claude Code's WebFetch decides
code, out, err, helper = web("what is on https://example.com/", settings=deny("WebFetch(https://example.com/*)"))
ok(code == 0 and helper == 1, "a rule of another form: the helper: %r" % (out + err)[:160])

# --------------------------------------------------------------------------- the helper's tools
def last_call():
    lines = open(LOG).read().splitlines() if os.path.exists(LOG) else []
    return json.loads(lines[-1]) if lines else {}


def allowed_of(call):
    a = call.get("argv", [])
    return a[a.index("--allowedTools") + 1].split(",") if "--allowedTools" in a else []


def agent_tools(call):
    a = call.get("argv", [])
    agents = json.loads(a[a.index("--agents") + 1]) if "--agents" in a else {}
    return next(iter(agents.values()), {}).get("tools", [])


both = {"permissions": {"deny": ["WebFetch(domain:denied.example)"]}, "hooks": hook("WebFetch")["hooks"]}
code, out, err, helper = web("what is on https://example.com/", settings=both)
c = last_call()
given = json.dumps(c.get("settings"))
ok(helper == 1 and "WebFetch(domain:denied.example)" in given and "PreToolUse" in given and allowed_of(c) == ["WebSearch", "WebFetch(domain:example.com)"],
   "the helper: the user's deny rule and hook in its settings, WebFetch allowed for example.com alone: %r %r" % (allowed_of(c), given[:200]))
code, out, err, helper = web("what is the tallest mountain on earth", settings=hook("WebSearch"))
c = last_call()
ok(helper == 1 and "WebFetch" not in agent_tools(c) and not any(x.startswith("WebFetch") for x in allowed_of(c)), "no address: the helper has no WebFetch at all: %r %r" % (agent_tools(c), allowed_of(c)))
perm = {"hooks": {"PermissionRequest": [{"matcher": "WebFetch", "hooks": [{"type": "command", "command": "true"}]}]}}
code, out, err, helper = web("what is on https://example.com/", settings=perm)
c = last_call()
ok(helper == 1 and "--allowedTools" not in c.get("argv", []), "a hook on WebFetch's permission: nothing allowed already, so the permission is asked and the hook runs (W5): %r" % c.get("argv", [])[:12])
ok("--" in c.get("argv", []) and c["argv"][-2] == "--", "the job after '--', last (a job that starts with '--' is no option): %r" % c.get("argv", [])[-3:])

# ------------------------------------------------------------ addresses checked before anything
refused("read localtest.me/secret", "local or private", "an address without https:// to this machine")
refused("read localtest.me/secret", "local or private", "the same with a hook on every tool (the address checked before any helper)", settings=hook("*"))
refused("read 10.0.0.1:8443/admin", "local or private", "an IP address without https://")
refused("what is on https://\u2460\u2461\u2466.\u24ea.\u24ea.\u2460/x", "cannot check", "a host of other digits (not ASCII): refused, never the helper")
refused("see http://127.0.0.1:9/x then https://example.com/", "local or private", "an http address before an https one")
refused("what is on https://nothing-here.invalid/", "deny reading", "a denied name refused for the rule, before it is looked up", settings=deny("WebFetch(domain:nothing-here.invalid)"))

# -------------------------------------------------------------------- proxies, project switches
code, out, err, helper = web("what is on https://example.com/", env_extra={"HTTPS_PROXY": "http://127.0.0.1:9"})
ok(code == 0 and helper == 1, "a proxy set: Claude Code's WebFetch reads it, not fewer (which reads only straight to the address it checked): %r" % (out + err)[:160])
proj2 = os.path.join(WORK, "proj2")
put(proj2 + "/.claude/settings.json", json.dumps({"disableAllHooks": True, "skipWebFetchPreflight": True}))
code, out, err, helper = web("what is on https://example.com/", settings=hook("WebFetch"), cwd=proj2)
ok(code == 0 and helper == 1, "a project's switches, the session's folder unknown: they turn off neither the user's hook nor the domain check: %r" % (out + err)[:160])

# ------------------------------------------------------ the rules on Claude Code's command line
fake = os.path.join(WORK, "fakeparent")
put(fake + "/claude", "#!/bin/sh\n\"$FEWER_WEB_BIN\" web \"$FEWER_WEB_Q\"\n")
os.chmod(fake + "/claude", 0o755)


def web_under(flags, question):
    put(claude_dir + "/settings.json", "{}")
    e = safe.env(FEWER_CLAUDE_DIR=claude_dir, FEWER_MANAGED_DIR=managed, FEWER_HOME=store, FEWER_CLAUDE_BIN=bindir + "/claude", CLAUDE_CODE_SESSION_ID=None, FEWER_WEB_BIN=SEARCH, FEWER_WEB_Q=question)
    r = subprocess.run(["/bin/sh", fake + "/claude", *flags], cwd=plain, env=e, capture_output=True, text=True, timeout=180)
    return r.returncode, r.stdout, r.stderr


code, out, err = web_under(["--disallowedTools", "WebFetch(domain:example.com)"], "what is on https://example.com/")
ok(code != 0 and "deny reading example.com" in err, "a rule on Claude Code's command line (--disallowedTools): held: %r" % (err or out)[:160])
code, out, err = web_under(["--plugin-dir", WORK], "what is on https://example.com/")
ok(code != 0 and "plugin folder" in err, "a plugin folder on Claude Code's command line: nothing read: %r" % (err or out)[:160])
put(fake + "/flag-settings.json", json.dumps(deny("WebFetch(domain:example.com)")))
code, out, err = web_under(["--settings", fake + "/flag-settings.json"], "what is on https://example.com/")
ok(code != 0 and "deny reading example.com" in err, "settings on Claude Code's command line (--settings FILE): held: %r" % (err or out)[:160])

# ---------------------------------------------------------------- several pages in one question
code, out, err, helper = web("compare https://example.com/ with https://example.org/ : which domains are reserved?")
ok(code == 0 and helper == 0 and "also named, not read: https://example.org/" in out, "the other address a question names is said not read (C7): %r" % (out + err)[-200:])
# (PATH without fewer: every printed command names it by its whole path, C6)
code, out, err, helper = web("which names does https://www.rfc-editor.org/rfc/rfc2606.html reserve?", env_extra={"PATH": "/usr/bin:/bin"})
said = [l for l in out.splitlines() if "read on with:" in l]
ok(code == 0 and said and all(SEARCH in l for l in said), "with no fewer on PATH a read-on names the whole path (C6): %r" % (said[:2] or (out + err)[-200:]))
# (a text fragment adds its words, no part's name: nothing said of a lost anchor, C4)
code, out, err, helper = web("what does https://example.com/#:~:text=documentation%20examples say")
ok(code == 0 and "names no part" not in out and "documentation examples" in out, "a text fragment's words join the question (C4): %r" % (out + err)[-200:])
code, out, err, helper = web("what is on https://example.com/#no-such-part")
ok(code == 0 and "#no-such-part names no part of the page" in out, "an anchor that names nothing is said (C4): %r" % (out + err)[-200:])

print("web_check: %s" % ("ALL OK" if not failed else "%d FAILED" % failed))
sys.exit(1 if failed else 0)
