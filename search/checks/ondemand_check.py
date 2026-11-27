"""ondemand_check.py SEARCH WORK: fewer's features on demand and its switch, on made-up
skills and a claude stub; every process with a made-up HOME and agents' folders (safe_env.py):
- `skill NAME`: the user's, the project's and a plugin's skills, by name;
- `helper` and `workflow` with a claude stub answering in Claude Code's JSON: the answer printed,
  two jobs at once (in about the time of one), a failed helper (exit 1), the tokens logged in
  helpers.jsonl, FEWER_HELPER_SETTINGS passed as --settings, a helper as fewer's worker only when
  the worker's file is there (Claude Code's folder, or the project's .claude/agents from a
  subfolder), a workflow refused without its agent's file, else its own agent with its
  Workflow tool turned on and allowed;
- `on` and `off` through a made-up Claude Code folder: off leaves the settings as they were,
  with copies kept; the lean rules name no add-on command (Claude uses the add-ons through
  Claude Code itself);
- `mcp` with anything after it: add-ons are not in this version, said so.
"""
import importlib.util, json, os, shutil, stat, subprocess, sys, time

if len(sys.argv) != 3:
    raise SystemExit(__doc__)
SEARCH, WORK = (os.path.abspath(a) for a in sys.argv[1:3])
HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mcpc", os.path.join(HERE, "mcpc.py"))
mcpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcpc)
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(HERE, "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)
mcpc.fresh_work(WORK)
safe = safe_env.setup(WORK)
fails = []
checks = 0


def ok(cond, what):
    global checks
    checks += 1
    if not cond:
        fails.append(what)


def flat(text):
    """The text with its wrapped lines joined: each line end, and the spaces around it, one space."""
    return " ".join(l.strip() for l in text.split("\n") if l.strip())


def put(path, text, mode=0o644):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    os.chmod(path, mode)



store = WORK + "/store"
put(store + "/config", "dna = off\nsessions = off\n")
os.makedirs(WORK + "/state", exist_ok=True)
proj = WORK + "/proj"
sub = proj + "/sub"
os.makedirs(sub)
subprocess.run(["git", "init", "-q", proj], check=True, env=safe.env())
claude_dir = os.environ["FEWER_CLAUDE_DIR"]
# (a setting of the developer's own, kept through on and off)
put(claude_dir + "/settings.json", json.dumps({"enabledMcpjsonServers": ["approved"]}))
env = safe.env(FEWER_HOME=store)


def run(args, cwd=sub, stdin=None, e=None, timeout=60):
    r = subprocess.run([SEARCH, *args], cwd=cwd, env=e or env, input=stdin, capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout, r.stderr


# ---------------------------------------------------------------------------- add-ons: Claude Code's own
for args in (["mcp", "list"], ["mcp", "call", "x", "y"], ["mcp", "ask", "x", "q"], ["mcp", "login", "x"]):
    code, out, err = run(args)
    # (no add-on commands: Claude uses the add-ons through Claude Code itself)
    ok(code == 2 and out == "" and "fewer has no commands for add-ons: with fewer on Claude uses your add-ons (MCP servers)" in err and "fewer off" not in err, "%s: said there are none: %d %r" % (args, code, err))

# ---------------------------------------------------------------------------- skills
put(claude_dir + "/skills/gstack/verify/SKILL.md", "---\nname: verify\ndescription: checks\n---\nRun the checks until green.\n")
put(claude_dir + "/plugins/cache/mkt/anthropic-skills/1.0/skills/docx/SKILL.md", "---\nname: docx\n---\nWord files.\n")
put(proj + "/.claude/skills/deploy/SKILL.md", "---\nname: deploy\n---\nShip it.\n")
code, out, err = run(["skill", "verify"])
ok(code == 0 and out == "skill verify: %s/skills/gstack/verify/SKILL.md (its other files are in that folder)\n---\nname: verify\ndescription: checks\n---\nRun the checks until green.\n" % claude_dir, "skill verify: %r %r" % (out, err))
code, out, err = run(["skill", "deploy"])
ok(code == 0 and out.endswith("Ship it.\n"), "the project's skill: %r" % out)
code, out, err = run(["skill", "anthropic-skills:docx"])
ok(code == 0 and out.endswith("Word files.\n"), "a plugin's skill: %r" % out)
code, out, err = run(["skill", "claude-api"])
ok(code == 1 and "type /claude-api in Claude Code" in err, "a built-in skill: %r" % err)

# ---------------------------------------------------------------------------- helpers and workflows
STUB = r'''#!/usr/bin/python3
import json, os, sys, time
log = os.environ["STUB_LOG"]
with open(log, "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\n")
# (the job after "--", last, as fewer passes it now; before, right after -p)
job = sys.argv[sys.argv.index("--") + 1] if "--" in sys.argv else sys.argv[sys.argv.index("-p") + 1]
time.sleep(1.0)
fail = "FAIL" in job
print(json.dumps({"type": "result", "subtype": "error_during_execution" if fail else "success", "is_error": fail, "num_turns": 2,
                  "result": "answer to: " + job, "total_cost_usd": 0.01,
                  "modelUsage": {"m": {"inputTokens": 10, "outputTokens": 5, "cacheReadInputTokens": 100, "cacheCreationInputTokens": 50}}}))
sys.exit(1 if fail else 0)
'''
stub = WORK + "/bin/claude-json"
put(stub, STUB, 0o755)
stub_log = WORK + "/state/stub-calls"
hs = WORK + "/helper-settings.json"
put(hs, json.dumps({"permissions": {"allow": ["Bash(python3:*)"]}}))
henv = safe.env(FEWER_CLAUDE_BIN=stub, FEWER_HOME=store, STUB_LOG=stub_log, FEWER_HELPER_SETTINGS=hs)
code, out, err = run(["helper", "find the retry limit"], e=henv)
ok((code, out) == (0, "answer to: find the retry limit\n"), "one helper: %d %r %r" % (code, out, err))
calls = [json.loads(l) for l in open(stub_log)]
ok(calls[-1][:4] == ["-p", "find the retry limit", "--output-format", "json"] and calls[-1][4] == "--settings" and json.loads(calls[-1][5]) == {"permissions": {"allow": ["Bash(python3:*)"]}}, "the helper's arguments: %r" % calls[-1])
ok("--agent" not in calls[-1], "no worker's file anywhere: the helper is the session's own agent: %r" % calls[-1])
t0 = time.time()
code, out, err = run(["helper", "review the first half", "review the second half"], e=henv)
took = time.time() - t0
ok((code, out) == (0, "== helper 1 of 2 ==\nanswer to: review the first half\n== helper 2 of 2 ==\nanswer to: review the second half\n"), "two helpers: %r" % out)
ok(took < 1.9, "two helpers at once take about the time of one: %.2f s" % took)
code, out, err = run(["helper", "review", "the", "code"], e=henv)
ok(out == "answer to: review the code\n", "words without spaces are one job: %r" % out)
code, out, err = run(["helper", "this one will FAIL now"], e=henv)
ok(code == 1 and "the helper ended with an error (error_during_execution)" in err, "a failed helper: %d %r" % (code, err))
n_calls = len(open(stub_log).readlines())
code, out, err = run(["workflow", "review everything with a workflow"], e=henv)
ok(code == 1 and out == "" and "workflow agent (fewer-lean-workflow.md), which is not there: fewer on" in err and len(open(stub_log).readlines()) == n_calls,
   "a workflow without its agent's file: refused, claude not started: %d %r" % (code, err))
# the agents' files in the project's .claude/agents, the jobs started from a subfolder
put(proj + "/.claude/agents/fewer-worker.md", "---\nname: fewer-worker\n---\nlean\n")
put(proj + "/.claude/agents/fewer-lean-workflow.md", "---\nname: fewer-lean-workflow\n---\nlean\n")
code, out, err = run(["helper", "find the retry limit"], cwd=sub, e=henv)
helper = [json.loads(l) for l in open(stub_log)][-1]
# (a worker starts with no add-ons: fewer keeps them on for the main chat, and a helper's every step
# would re-read their instructions)
ok(code == 0 and helper[4:7] == ["--agent", "fewer-worker", "--strict-mcp-config"] and helper[7] == "--settings", "the worker's file in the project: the helper is fewer's worker, with no add-ons: %r %r" % (helper, err))
code, out, err = run(["workflow", "review everything with a workflow"], cwd=sub, e=henv)
ok((code, out) == (0, "answer to: review everything with a workflow\n"), "a workflow: %r %r" % (out, err))
wf = [json.loads(l) for l in open(stub_log)][-1]
ok(wf[4:9] == ["--agent", "fewer-lean-workflow", "--allowedTools", "Workflow", "--strict-mcp-config"] and wf[9] == "--settings"
   and json.loads(wf[10]) == {"permissions": {"allow": ["Bash(python3:*)"]}, "env": {"CLAUDE_CODE_DISABLE_WORKFLOWS": "0"}}, "the workflow's arguments: %r" % wf)
used = [json.loads(l) for l in open(store + "/helpers.jsonl")]
ok(len(used) == 7 and [u["kind"] for u in used].count("workflow") == 1 and all(u["tokens"] == 160 and u["output"] == 5 for u in used) and [u["ok"] for u in used].count(False) == 1, "helpers.jsonl: %r" % used)
code, out, err = run(["helper", "x y"], e=safe.env(FEWER_CLAUDE_BIN=None, FEWER_HOME=store))
ok(code == 1 and "FEWER_CLAUDE_BIN is not set" in err, "a helper without FEWER_CLAUDE_BIN: %r" % err)

# ---------------------------------------------------------------------------- on and off
tenv = safe.env(FEWER_HOME=store)
settings = claude_dir + "/settings.json"
before = open(settings).read()
code, out, err = run(["on", "--dry-run"], e=tenv)
ok(code == 0 and out.endswith("Dry run: nothing was changed. fewer on turns it on; fewer off turns it off.\n") and open(settings).read() == before, "on --dry-run changes nothing: %r" % out[-200:])
code, out, err = run(["on"], e=tenv)
ok(code == 0 and out == "✓ Fewer is on. To turn it off: fewer off\n", "on: %r %r" % (out[-300:], err))
s = json.load(open(settings))
# (the add-ons stay as they were, fewer's own MCP server alone kept off)
ok(s.get("agent") == "fewer-lean" and s.get("enabledMcpjsonServers") == ["approved"] and "CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH" not in s["env"]
   and s.get("deniedMcpServers") == [{"serverName": "fewer"}] and "allowedMcpServers" not in s, "the settings after on: %r" % s)
lean = open(claude_dir + "/agents/fewer-lean.md").read()
ok("its tools, as usual (ToolSearch loads them)" in lean and ("`%s web \"question, or a page's address and question\"`" % SEARCH) in lean and "mcp list" not in lean and "mcp ask" not in lean, "the lean agent names fewer by its path (not on the safe PATH), and no add-on command")
worker = open(claude_dir + "/agents/fewer-worker.md").read()
ok(("`%s changes [REV]`" % SEARCH) in worker and "mcp list" not in worker and "helper \"" not in worker, "the worker: the lean rules for one job, without the features: %r" % worker[-400:])
ok("agentType: 'fewer-worker'" in open(claude_dir + "/agents/fewer-lean-workflow.md").read(), "the workflow agent gives its agents the worker's type")
os.remove(proj + "/.claude/agents/fewer-worker.md")
code, out, err = run(["helper", "find the retry limit"], e=henv)
helper = [json.loads(l) for l in open(stub_log)][-1]
ok(code == 0 and helper[4:6] == ["--agent", "fewer-worker"], "the worker's file in Claude Code's folder (fewer on): the helper is fewer's worker: %r" % helper)
code, out, err = run(["off"], e=tenv)
ok(code == 0 and out == "✓ Fewer is off. To turn it on: fewer on\n", "off: %r %r" % (out, err))
ok(json.load(open(settings)) == json.loads(before), "off leaves the settings as they were: %r" % open(settings).read())
ok(not any(os.path.exists(claude_dir + "/agents/" + f) for f in ("fewer-lean.md", "fewer-lean-workflow.md", "fewer-worker.md")), "off removes the agents' files")
copies = [n for n in os.listdir(claude_dir) if n.startswith("settings.json.before-fewer-")]
ok(len(copies) == 2, "a copy of the settings each time: %r" % copies)
code, out, err = run(["off"], e=tenv)
ok((code, out) == (0, "fewer is off already.\n"), "off again: %r" % out)
code, out, err = run(["on", "sideways"], e=tenv)
ok(code == 2, "on with a wrong word is a usage error")
# `on` and `off` once more
code, out, err = run(["on"], e=tenv)
ok(code == 0 and out == "✓ Fewer is on. To turn it off: fewer off\n", "on: %r %r" % (out[-300:], err))
code, out, err = run(["on"], e=tenv)
ok(code == 0 and out == "Fewer is on already. To turn it off: fewer off\n", "on when it is on: said so: %r" % out[-300:])
code, out, err = run(["off"], e=tenv)
ok(code == 0 and out == "✓ Fewer is off. To turn it on: fewer on\n", "off: %r %r" % (out, err))
ok(json.load(open(settings)) == json.loads(before), "off leaves the settings as they were: %r" % open(settings).read())
code, out, err = run(["off"], e=tenv)
ok((code, out) == (0, "fewer is off already.\n"), "off again: %r" % out)

if fails:
    print("\n".join("FAIL: " + f for f in fails))
    print("ondemand_check: %d of %d checks FAILED" % (len(fails), checks))
    sys.exit(1)
print("ondemand_check: all passed (%d checks)" % checks)
