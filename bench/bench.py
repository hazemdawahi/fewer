#!/usr/bin/env python3
"""bench.py: Fewer's benchmark, to rerun on your own computer.

The same 9 tasks, in a made-up project, run by Claude Code twice: as it is, and with Fewer. Each
run counts the tokens Claude Code used (input and cache, every model, Fewer's own helpers too)
and checks the answer. The only difference between the two setups is Fewer.

  python3 bench/bench.py run WORK [--tasks T1,T3] [--runs 3] [--setups claude,fewer] [--model M]
  python3 bench/bench.py report WORK

The same setup for everyone: the model is fixed (Claude Opus 5.5 unless --model), and none of
your own Claude Code is loaded (no CLAUDE.md, settings, add-ons, skills or claude.ai
connectors): Claude Code runs from an empty folder of its own inside WORK, logged in with
CLAUDE_CODE_OAUTH_TOKEN (make one with `claude setup-token`) or ANTHROPIC_API_KEY. Without
either, your own login is used and your own CLAUDE.md and skills are loaded too (both setups
alike); the report then says so, as your numbers may differ more. The report also names the
Claude Code version, as a different version changes Claude Code's own token use.

WORK is a new folder for the made-up project, the runs and the results; `run` again goes on
where it stopped. Fewer must be installed (`fewer` on PATH), or name it with FEWER_BIN. Each run
uses your Claude usage: the whole benchmark is 54 runs (9 tasks, 3 runs each, 2 setups). It
stops by itself when your plan's usage limit is reached; `run` again later goes on.
"""
import json, os, platform, random, re, shutil, statistics, subprocess, sys, time

# the model and the Claude Code version of the numbers on the main page
MODEL = "claude-opus-5-5"
MEASURED_WITH = "2.1.332"

TASKS = {
    "T1": "Run the test suite with `python3 -m unittest discover -s tests -v`. Which tests fail, and what exactly does each failure say?",
    "T3": "In logs/app.log, how many requests returned a 5xx status, for each endpoint? Give exact counts.",
    "T5": "In data/sales.csv, what is the total amount for each region (to the cent), and which product sold the most units in all?",
    "T7": "What did the last commit change? Name each function it changed and what the change does.",
    "T21": "Read RFC 9110 (https://www.rfc-editor.org/rfc/rfc9110) on the web: which HTTP status code means Range Not Satisfiable, and which means Misdirected Request? Give both codes.",
    "T22": "Search the web: in what year was version 1.0 of the Rust programming language released?",
    "T6": "The test test_discount_rounding in tests/test_pricing.py fails. Fix the code, not the test, so that it passes, then run that test to show it passes.",
    "T8": "Read the modules in calc/ and answer: (a) the value of DEFAULT_TIMEOUT, (b) which module's normalize() raises ValueError, (c) which modules import json, (d) the value of LIMIT in calc/m09.py, (e) which function calls send_report().",
    "T9": "Review the modules calc/n01.py to calc/n20.py one at a time, in order, reading each one whole (do not search them with grep). List every function that divides by a value without first checking that the value is not zero, with its module. Then answer: (a) the value of MAX_BATCH in calc/n04.py, (b) which module defines retry_policy(), (c) which function in calc/n17.py calls archive().",
}
NAMES = {
    "T1": "Find why the tests fail", "T3": "Count the errors in a log", "T5": "Add up the totals in a CSV file",
    "T7": "Explain the last commit", "T21": "Find two status codes in a web page", "T22": "A web search: when Rust 1.0 came out",
    "T6": "Fix a bug, then run its test", "T8": "Read 13 files and answer 5 questions", "T9": "Review 20 files one by one (a long chat)",
}
WEB = {"T21", "T22"}
TURNS = {"T9": "45"}
SETUPS = ("claude", "fewer")
# the tools both setups may use without asking (a `-p` run cannot ask)
ALLOWED = ["Read", "Grep", "Glob", "Edit", "Write"] + ["Bash(%s:*)" % c for c in (
    "python3", "python", "git", "cat", "head", "tail", "grep", "wc", "sort", "uniq", "awk", "cut", "ls", "find", "jq", "sed", "echo")]
WEB_FACTS = {"T21": [["416"], ["421"]], "T22": [["2015"]]}
BUGGY = ("n02", "n07", "n11", "n15", "n19")


def die(msg):
    raise SystemExit("bench.py: " + msg)


# ---------------------------------------------------------------------------- the made-up project

def write(root, rel, text):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    # (newline: the same bytes on Windows too)
    with open(p, "w", newline="\n") as f:
        f.write(text)


def filler(i, rnd, extra=""):
    """A plausible module of about 180 lines."""
    lines = ['"""calc.m%02d: helpers for batch %d."""' % (i, i), "import math", ""]
    lines += extra.split("\n") if extra else []
    for k in range(14):
        lines += [
            "",
            "def step_%02d_%02d(values, scale=%d):" % (i, k, rnd.randint(2, 9)),
            '    """Scale the values of step %d and keep those under the cap."""' % k,
            "    cap = %d" % rnd.randint(100, 999),
            "    out = []",
            "    for v in values:",
            "        w = v * scale + %d" % rnd.randint(0, 50),
            "        if w < cap:",
            "            out.append(round(math.sqrt(abs(w)), 3))",
            "    return out",
        ]
    return "\n".join(lines) + "\n"


def make_project(root):
    """The made-up repository every run starts from (the same bytes on every computer: a fixed
    seed), and the facts a right answer holds."""
    os.makedirs(root)
    rnd = random.Random(20261123)
    facts = {}
    write(root, "calc/__init__.py", "")
    write(root, "calc/pricing.py", '''"""Prices: discounts and tax."""
TAX_RATE = 0.08


def apply_discount(price, pct):
    """The price after a discount of pct percent, in cents (two places)."""
    return int(price * (1 - pct / 100) * 100) / 100


def tax(price):
    return round(price * TAX_RATE, 2)
''')
    write(root, "calc/parse.py", '''"""Parsing amounts typed by people."""


def parse_amount(text):
    return float(text.strip())
''')
    write(root, "calc/upload.py", '''"""Uploads with retries."""
import os
import time

MAX_UPLOAD_RETRIES = int(os.environ.get("CALC_UPLOAD_RETRIES", "7"))


def backoff(attempt):
    return min(2 ** attempt, 30)


def upload(send, payload):
    for attempt in range(MAX_UPLOAD_RETRIES):
        if send(payload):
            return True
        time.sleep(backoff(attempt))
    return False
''')
    for i in range(1, 13):
        extra = ""
        if i == 3:
            extra = "DEFAULT_TIMEOUT = 45\n"
        if i == 7:
            extra = 'def normalize(name):\n    if not name:\n        raise ValueError("empty name")\n    return name.strip().lower()\n'
        if i in (4, 11):
            extra = "import json\n\n\ndef dump(x):\n    return json.dumps(x)\n"
        if i == 9:
            extra = "LIMIT = 1337\n"
        if i == 10:
            extra = "from calc.report import send_report\n\n\ndef close_month(rows):\n    return send_report(rows)\n"
        write(root, "calc/m%02d.py" % i, filler(i, rnd, extra))
    write(root, "calc/report.py", '"""Reports."""\n\n\ndef send_report(rows):\n    return len(rows)\n')
    # T9: twenty long modules, five with a division unchecked for zero
    for i in range(1, 21):
        tag = "n%02d" % i
        if tag in BUGGY:
            share = "def share_%s(total, parts):\n    return total / parts\n" % tag
        else:
            share = "def share_%s(total, parts):\n    if parts == 0:\n        return 0\n    return total / parts\n" % tag
        extra = share
        if i == 4:
            extra += "\nMAX_BATCH = 384\n"
        if i == 13:
            extra += "\ndef retry_policy(attempt):\n    return min(attempt * 2, 20)\n"
        if i == 17:
            extra += "\ndef archive(rows):\n    return list(rows)\n\n\ndef close_books(rows):\n    return archive(rows)\n"
        body = filler(i + 20, rnd, extra)
        more = "".join(filler(100 + 7 * i + k, rnd).split("\n", 3)[3] for k in range(5))
        write(root, "calc/%s.py" % tag, body + more)
    facts["T9"] = [["share_" + t] for t in BUGGY] + [["384"], ["n13"], ["close_books"]]
    facts["T9_not"] = ["share_n%02d" % i for i in range(1, 21) if "n%02d" % i not in BUGGY]
    facts["T8"] = [["45"], ["m07"], ["m04"], ["m11"], ["1337"], ["close_month"]]
    write(root, "tests/__init__.py", "")
    write(root, "tests/test_pricing.py", '''import unittest
from calc.pricing import apply_discount


class TestPricing(unittest.TestCase):
    def test_discount_rounding(self):
        self.assertEqual(apply_discount(2.30, 50), 1.15)

    def test_no_discount(self):
        self.assertEqual(apply_discount(10.00, 0), 10.0)
''')
    write(root, "tests/test_parse.py", '''import unittest
from calc.parse import parse_amount


class TestParse(unittest.TestCase):
    def test_parse_amount(self):
        self.assertEqual(parse_amount("1,234.50"), 1234.5)
''')
    many = ["import unittest", "", "", "class TestMany(unittest.TestCase):"]
    for k in range(160):
        many += ["    def test_case_%03d(self):" % k, "        self.assertEqual(%d + %d, %d)" % (k, k, 2 * k), ""]
    write(root, "tests/test_many.py", "\n".join(many) + "\n")
    facts["T1"] = [["test_discount_rounding"], ["1.14 != 1.15", "1.14"], ["test_parse_amount"], ["could not convert string to float", "ValueError"]]
    eps = ["/api/login", "/api/cart", "/api/pay", "/api/search"]
    fivexx = {e: 0 for e in eps}
    out = []
    t0 = 1763900000
    for k in range(20000):
        e = rnd.choice(eps)
        st = rnd.choices([200, 201, 304, 404, 500, 502, 503], [70, 8, 8, 6, 3, 3, 2])[0]
        if st >= 500:
            fivexx[e] += 1
        ts = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(t0 + k * 3))
        lvl = "ERROR" if st >= 500 else "INFO"
        out.append("%s %s req=%06d %s %s status=%d dur_ms=%d" % (ts, lvl, k, rnd.choice(["GET", "POST"]), e, st, rnd.randint(2, 900)))
    write(root, "logs/app.log", "\n".join(out) + "\n")
    facts["T3"] = [[str(fivexx[e])] for e in eps]
    countries = ["DE", "FR", "US", "BR", "JP", "IN"]
    ev = []
    for k in range(5000):
        c = rnd.choices(countries, [5, 4, 9, 3, 2, 6])[0]
        ty = rnd.choices(["view", "cart", "purchase", "refund"], [60, 20, 15, 5])[0]
        ev.append(json.dumps({"id": k, "type": ty, "country": c, "user": "u%04d" % rnd.randint(1, 900), "amount": round(rnd.uniform(1, 300), 2)}))
    write(root, "data/events.jsonl", "\n".join(ev) + "\n")
    regions = ["north", "south", "east", "west"]
    products = ["anvil", "bolt", "crank", "dowel", "easel"]
    tot = {r: 0 for r in regions}
    units = {p: 0 for p in products}
    rows = ["region,product,units,amount_cents"]
    for k in range(3000):
        r, p = rnd.choice(regions), rnd.choice(products)
        u, a = rnd.randint(1, 40), rnd.randint(100, 90000)
        tot[r] += a
        units[p] += u
        rows.append("%s,%s,%d,%d" % (r, p, u, a))
    write(root, "data/sales.csv", "\n".join(rows) + "\n")
    facts["T5"] = [["%d.%02d" % (tot[r] // 100, tot[r] % 100)] for r in regions] + [[max(units, key=units.get)]]
    # (core.autocrlf off: Git for Windows would change the files' line ends)
    git = lambda *a: subprocess.run(["git", "-c", "user.name=bench", "-c", "user.email=bench@example.invalid", "-c", "core.autocrlf=false", *a], cwd=root, check=True, capture_output=True)
    git("init", "-q")
    git("add", "-A")
    git("commit", "-q", "-m", "base")
    for rel, old, new in (("calc/upload.py", "return min(2 ** attempt, 30)", "return min(2 ** attempt, 60)"),
                          ("calc/pricing.py", "    return round(price * TAX_RATE, 2)", "    rate = TAX_RATE if price < 1000 else 0.05\n    return round(price * rate, 2)")):
        p = os.path.join(root, rel)
        text = open(p, newline="").read()
        if old not in text:
            die("the made-up project came out wrong (%s)" % rel)
        with open(p, "w", newline="\n") as f:
            f.write(text.replace(old, new))
    git("commit", "-q", "-am", "retry longer; lower tax on big orders")
    facts["T7"] = [["backoff"], ["60"], ["tax"], ["1000", "1,000"]]
    facts.update(WEB_FACTS)
    r = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"], cwd=root, capture_output=True, text=True)
    if not ("FAILED (failures=1, errors=1)" in r.stderr and "test_discount_rounding" in r.stderr and "could not convert string to float" in r.stderr):
        die("the made-up project's tests do not fail as designed:\n" + r.stderr[-800:])
    return facts


# ---------------------------------------------------------------------------- the runs

def norm(s):
    return re.sub(r"\s+", " ", s.lower().replace("`", "").replace("*", ""))


def score(t, res, facts):
    """'right' or what was wrong, from the answer's text (T6: the test passes after the run and
    its file is unchanged)."""
    if t == "T6":
        return "right" if res.get("_test_passes") and res.get("_test_file_unchanged") else "the fix failed"
    text = norm(res.get("result") or "")
    want = facts[t]
    got = sum(any(norm(a) in text for a in alts) for alts in want)
    bad = [w for w in facts.get(t + "_not", []) if norm(w) in text]
    return "right" if got == len(want) and not bad else "%d of %d facts%s" % (got, len(want), ", %d wrong" % len(bad) if bad else "")


def tokens(res):
    """Input and cache tokens of every model of one run, and Fewer's helpers' tokens."""
    n = sum(int(m.get("inputTokens") or 0) + int(m.get("cacheReadInputTokens") or 0) + int(m.get("cacheCreationInputTokens") or 0)
            for m in (res.get("modelUsage") or {}).values())
    return n + sum(h.get("tokens", 0) for h in res.get("_helpers") or [])


LOGINS = ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY")


def clean_env():
    """Your environment without Fewer's settings and without the marks of a Claude Code session
    this may run inside (CLAUDE_CONFIG_DIR, where your login is, stays; so do a login token and
    where Git Bash is on Windows)."""
    keep = LOGINS + ("CLAUDE_CODE_GIT_BASH_PATH",)
    return {k: v for k, v in os.environ.items() if k in keep or not (k.startswith(("FEWER_", "CLAUDE_CODE_")) or k == "CLAUDECODE")}


def clean_claude(work):
    """The environment that keeps all of your own Claude Code out of the runs: an empty
    folder of its own for Claude Code (no CLAUDE.md, settings, add-ons or skills of yours) and no
    claude.ai connectors. Only with a login token: Claude Code's own folder holds your login."""
    if not any(os.environ.get(k) for k in LOGINS):
        return {}
    d = os.path.join(work, "claude-config")
    os.makedirs(d, exist_ok=True)
    return {"CLAUDE_CONFIG_DIR": d}


def stub(folder):
    """A stand-in for `claude` during Fewer's install into a run's folder, so that the install
    does not register anything in your real Claude Code (the run itself uses the real one)."""
    if os.name == "nt":
        p = os.path.join(folder, "claude.cmd")
        with open(p, "w", newline="\r\n") as f:
            f.write("@rem bench.py: `claude mcp ...` during Fewer's install into a run's folder\n@exit /b 0\n")
        return p
    p = os.path.join(folder, "claude")
    with open(p, "w") as f:
        f.write("#!/bin/sh\n# bench.py: `claude mcp ...` during Fewer's install into a run's folder\nexit 0\n")
    os.chmod(p, 0o755)
    return p


def prepare_fewer(dest, store, fewer, claude, web):
    """Fewer installed into the run's own folder (its .claude), as `fewer install` writes it, and
    the environment and settings of the run."""
    os.makedirs(store, exist_ok=True)
    with open(os.path.join(store, "config"), "w") as f:
        f.write("dna = off\nsessions = off\n")
    home = os.path.join(store, "home")
    os.makedirs(home, exist_ok=True)
    claude_dir = os.path.join(dest, ".claude")
    env = clean_env()
    env.update({"HOME": home, "FEWER_HOME": store, "FEWER_CLAUDE_DIR": claude_dir, "FEWER_CLAUDE_BIN": stub(store),
                "PATH": os.path.dirname(fewer) + os.pathsep + env.get("PATH", "")})
    if os.name == "nt":
        # (Fewer finds the home folder there first)
        env["USERPROFILE"] = home
    r = subprocess.run([fewer, "install", "claude", "--yes"], cwd=dest, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
    if r.returncode != 0 or not os.path.exists(os.path.join(claude_dir, "agents", "fewer-lean.md")):
        die("Fewer's install into %s failed (exit %d): %s %s" % (dest, r.returncode, r.stdout[-600:], r.stderr[-600:]))
    perms = os.path.join(store, "perms.json")
    with open(perms, "w") as f:
        json.dump({"permissions": {"allow": ALLOWED + ["Bash(fewer:*)"]}}, f)
    run_env = {"PATH": os.path.dirname(fewer) + os.pathsep + os.environ.get("PATH", ""), "FEWER_HOME": store, "FEWER_CLAUDE_BIN": claude,
               "FEWER_HELPER_SETTINGS": perms}
    pack = shutil.which("fewer-pack", path=os.path.dirname(fewer))
    if pack:
        run_env["FEWER_PACK_BIN"] = pack
    if web:
        run_env["FEWER_CLAUDE_DIR"] = claude_dir
    return ["--settings", perms], run_env


def claude_version(claude):
    """Claude Code's version, as `claude --version` says it ("2.1.332")."""
    r = subprocess.run([claude, "--version"], capture_output=True, text=True, timeout=60)
    return (r.stdout.split() or ["unknown"])[0]


def setup_of(work, claude, model):
    """How the runs in WORK are made (saved with the first run; later runs must match):
    Claude Code's version, the model, whether your own Claude Code is left out, the system."""
    now = {"claude_code": claude_version(claude), "model": model, "clean": bool(clean_claude(work)), "system": platform.system()}
    p = os.path.join(work, "setup.json")
    if os.path.exists(p):
        was = json.load(open(p))
        if (was.get("model"), was.get("clean")) != (now["model"], now["clean"]) or was.get("claude_code") != now["claude_code"]:
            die("the runs in %s were made another way (%s); these would not compare: use a new folder" % (work, json.dumps(was)))
    else:
        with open(p, "w") as f:
            json.dump(now, f, indent=1)
    return now


def run(work, argv):
    tasks, runs, setups, model = list(TASKS), 3, list(SETUPS), MODEL
    it = iter(argv)
    for a in it:
        if a == "--tasks":
            tasks = next(it, "").split(",")
        elif a == "--runs":
            runs = int(next(it, "3"))
        elif a == "--setups":
            setups = next(it, "").split(",")
        elif a == "--model":
            model = next(it, MODEL)
        else:
            die("unknown argument %s" % a)
    if bad := [t for t in tasks if t not in TASKS] + [s for s in setups if s not in SETUPS]:
        die("unknown task or setup: %s (tasks: %s; setups: claude, fewer)" % (", ".join(bad), ", ".join(TASKS)))
    claude = shutil.which("claude") or die("Claude Code (claude) is not on PATH")
    fewer = os.environ.get("FEWER_BIN") or shutil.which("fewer")
    if "fewer" in setups and not fewer:
        die("Fewer (fewer) is not on PATH: install it, or name it with FEWER_BIN")
    project = os.path.join(work, "project")
    if not os.path.exists(project):
        facts = make_project(project)
        with open(os.path.join(work, "facts.json"), "w") as f:
            json.dump(facts, f, indent=1)
        print("made the project in %s" % project, flush=True)
    facts = json.load(open(os.path.join(work, "facts.json")))
    how = setup_of(work, claude, model)
    print("Claude Code %s, %s, %s" % (how["claude_code"], how["model"], "none of your own Claude Code loaded" if how["clean"] else "your own login and Claude Code setup (no login token given)"), flush=True)
    for rep in range(1, runs + 1):
        for ti, t in enumerate(tasks):
            # (the setups take turns going first: neither always runs first or last)
            order = setups if (rep + ti) % 2 == 0 else setups[::-1]
            for s in order:
                name = "%s-%s-%d" % (t, s, rep)
                out = os.path.join(work, "results", name + ".json")
                if os.path.exists(out):
                    continue
                dest, store = os.path.join(work, "runs", name), os.path.join(work, "stores", name)
                for p in (dest, store):
                    if os.path.exists(p):
                        shutil.rmtree(p)
                shutil.copytree(project, dest, symlinks=True)
                env = clean_env()
                # (the same for everyone: the model fixed; no settings, add-ons or claude.ai
                # connectors of yours; with a login token none of your Claude Code at all)
                env.update(clean_claude(work))
                env["ENABLE_CLAUDEAI_MCP_SERVERS"] = "false"
                args = [claude, "-p", TASKS[t], "--output-format", "json", "--max-turns", TURNS.get(t, "30"), "--setting-sources", "project,local", "--strict-mcp-config", "--model", model]
                if s == "fewer":
                    more, run_env = prepare_fewer(dest, store, fewer, claude, t in WEB)
                    args += more
                    env.update(run_env)
                else:
                    args += ["--allowedTools", *(ALLOWED + (["WebFetch", "WebSearch"] if t in WEB else []))]
                t0 = time.time()
                r = subprocess.run(args, cwd=dest, capture_output=True, text=True, timeout=3600, env=env)
                try:
                    res = json.loads(r.stdout)
                except ValueError:
                    res = {"error": "no JSON from claude", "exit": r.returncode, "stderr": r.stderr[-2000:]}
                said = (res.get("result") or "") + (res.get("stderr") or "")
                if (res.get("is_error") or res.get("error")) and any(w in said.lower() for w in ("usage limit", "limit reached", "rate limit", "out of extra usage")):
                    print("%s: stopped: your plan's usage limit. Run again later: it goes on from here." % name, flush=True)
                    return 3
                res["_seconds"] = round(time.time() - t0, 1)
                if t == "T6":
                    chk = subprocess.run([sys.executable, "-m", "unittest", "tests.test_pricing", "-v"], cwd=dest, capture_output=True, text=True)
                    res["_test_passes"] = chk.returncode == 0 and re.search(r"test_discount_rounding \(.*\) \.\.\. ok", chk.stderr) is not None
                    res["_test_file_unchanged"] = open(os.path.join(dest, "tests/test_pricing.py")).read() == open(os.path.join(project, "tests/test_pricing.py")).read()
                hp = os.path.join(store, "helpers.jsonl")
                res["_helpers"] = [json.loads(l) for l in open(hp) if l.strip()] if os.path.exists(hp) else []
                res["_score"] = score(t, res, facts) if not res.get("error") else "error: %s" % res["error"]
                os.makedirs(os.path.dirname(out), exist_ok=True)
                with open(out, "w") as f:
                    json.dump(res, f, indent=1)
                print("%-12s %-8s %s tokens, %s steps, %.0f s, answer: %s" % (name, "", "{:,}".format(tokens(res)), res.get("num_turns"), res["_seconds"], res["_score"]), flush=True)
    return report(work)


def report(work):
    """Each task's median tokens with Claude Code alone and with Fewer, and how many answers
    were right."""
    rd = os.path.join(work, "results")
    if not os.path.isdir(rd):
        die("no results in %s yet: run first" % work)
    got = {}
    for fn in os.listdir(rd):
        if fn.endswith(".json"):
            t, s, _ = fn[:-5].split("-")
            res = json.load(open(os.path.join(rd, fn)))
            got.setdefault((t, s), []).append((tokens(res), res.get("_score") == "right"))
    print("\n| Task | Claude Code alone | With Fewer | Fewer tokens |")
    print("|---|--:|--:|--:|")
    totals = [0, 0]
    right = [0, 0]
    count = [0, 0]
    for t in TASKS:
        a, f = got.get((t, "claude")), got.get((t, "fewer"))
        if not a or not f:
            continue
        ma, mf = statistics.median(x[0] for x in a), statistics.median(x[0] for x in f)
        totals[0] += ma
        totals[1] += mf
        for i, side in enumerate((a, f)):
            right[i] += sum(x[1] for x in side)
            count[i] += len(side)
        print("| %s | %s | %s | **%d%%** |" % (NAMES[t], "{:,}".format(int(ma)), "{:,}".format(int(mf)), round(100 * (1 - mf / ma))))
    if totals[0]:
        print("| **All tasks** | **%s** | **%s** | **%d%%** |" % ("{:,}".format(int(totals[0])), "{:,}".format(int(totals[1])), round(100 * (1 - totals[1] / totals[0]))))
        print("\nMedians of each task's runs. Right answers: %d of %d with Claude Code alone, %d of %d with Fewer." % (right[0], count[0], right[1], count[1]))
    sp = os.path.join(work, "setup.json")
    if os.path.exists(sp):
        how = json.load(open(sp))
        print("Claude Code %s, %s, on %s; %s." % (how.get("claude_code"), how.get("model"), how.get("system"), "none of your own Claude Code loaded" if how.get("clean") else "your own login and Claude Code setup were loaded (both setups alike), so your numbers may differ more: a login token (claude setup-token) leaves them out"))
        if how.get("claude_code") != MEASURED_WITH or how.get("model") != MODEL:
            print("The main page's numbers are for Claude Code %s and %s: another version or model changes Claude Code's own token use." % (MEASURED_WITH, MODEL))
    return 0


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] not in ("run", "report"):
        raise SystemExit(__doc__)
    w = os.path.abspath(sys.argv[2])
    os.makedirs(w, exist_ok=True)
    sys.exit(run(w, sys.argv[3:]) if sys.argv[1] == "run" else report(w))
