"""read_check.py SEARCH WORK DIR...: `fewer read` loses nothing (2026-11-24).

Every text file under each DIR (.py .rs .js .ts .go .java .c .h .json .md .txt .csv .toml .yaml
.html .css .sh, at most 3,000 files) is read with `fewer read --max 100000000 FILE`, and
rebuilt from the printed view by this script, a reader written apart from fewer: numbered lines,
and each fold (a block's template after "  | ", the numbers of each copy from "«x» = ..." lines
and from the rows, a copy's numbers joined by "/"). The rebuilt text must equal the file's text
byte for byte, and every number the view gives must be the file's: each numbered line's number,
each fold's range and the line it says each copy starts at (line numbers
were never checked); a fold's rows must give each copy its numbers, no more and no fewer. Files
with a secret fewer hides, or not UTF-8 (said in the view's header notes), are counted and
skipped. Then the files of each folder are read together, 40 at a time (blocks of one shape in
several files share one template, printed in the first file that has it), whole and then 28,000
characters at a time, following each read-on line to the end as a reader would, and each file is
rebuilt from the views of all of them. Fails when nothing was checked.

Review 4 (2026-11-25): every view of a read-on must be within its --max; each header's line count
must be the file's; a line cut for being longer than an output is checked, not excused: its note's
count is the file's line in characters and the text shown is the line up to the character the note
names (line[N:] is the rest). Files made here (WORK/made: a 40,000-character line, a long line of
non-ASCII characters, a CSV whose sensor got stuck, JSON records past an output, wide blocks) are
checked with the others, so those paths are always tried.

Review 5 (2026-11-25): every folder's files are followed at 28,000 characters, a folder of one file
too, and a sample of files (every 25th, and the made ones) alone at 3,000; every file of a batch
must have its header; a file whose header says secrets are hidden is checked too, but for its lines
with a hidden marker and the lines left empty for a key; a cut line that an output of its own would
hold is a fault (it must be shown whole after a read-on); a read-on in the named form keeps a
--max given and puts "--" before names that start with "-"; several files read without --max give
either a view within 10,000 characters with no cut line, or the outline.
"""
import os

# (fewer read keeps a Claude Code session's project bounds (perms.rs, CLAUDECODE): this check is
# about what a read shows, of files in any folder, so its fewer runs outside such a session)
ENV = {k: v for k, v in os.environ.items() if k != "CLAUDECODE"}
import re
import shlex
import subprocess
import sys

EXT = {".py", ".rs", ".js", ".ts", ".go", ".java", ".c", ".h", ".json", ".md", ".txt", ".csv", ".toml", ".yaml", ".yml", ".html", ".css", ".sh"}
HEAD = re.compile(r"^== (.*) \(([\d,]+) lines?((?:, [^)]*)?)\)$")
OTHER = re.compile(r"^== (.*): (not text .*|.*)$")
RUN = re.compile(r"^(\d+)-(\d+): (\d+) copies of block (T\d+) \((above; )?(\d+) lines?, (no|\d+) empty lines? between; copy k \(from 0\) at line (\d+)\+(\d+)k\), (?:differing only in ((?:«[a-z]» ?)+)|all the same):$")
TOK = re.compile(r"«([a-z])(?::(\d+))?»")
STOP = re.compile(r"^… stopped at line (\d+) of ")
CUT = re.compile(r' … \(line (\d+) is ([\d,]+) characters, shown up to character ([\d,]+): print the rest, line\[(\d+):\] of line \d+ with the file split at \\n alone \(open\(path, newline=""\)\), with a small python3 program\)$')


def decode(items):
    out = []
    for it in items:
        m = re.fullmatch(r"\((.*)\)×(\d+)", it)
        inner, times = (m.group(1), int(m.group(2))) if m else (it, 1)
        if "×" in inner:
            v, n = inner.split("×")
            once = [int(v)] * int(n)
        elif "-" in inner:
            a, b = inner.split("-")
            once = list(range(int(a), int(b) + 1))
        else:
            once = [int(inner)]
        out += once * times
    return out


def rebuild_all(view):
    """Each file of a view: [dict(path, first, lines, notes, cut)]: its lines from line `first`
    (templates shared across the files); every line number and fold range checked."""
    lines = view.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    files, templates, i = [], {}, 0
    cur, expected = None, None

    def at(n, what):
        nonlocal expected
        if expected is None:
            cur["first"] = n
            expected = n
        if n != expected:
            raise ValueError("%s at line %d of the view says line %d, the file's next line is %d" % (what, i + 1, n, expected))

    while i < len(lines):
        l = lines[i]
        m = HEAD.match(l)
        if m:
            cur = dict(path=m.group(1), first=None, lines=[], notes=m.group(3), cuts={}, count=int(m.group(2).replace(",", "")))
            files.append(cur)
            expected = None
            i += 1
            continue
        if STOP.match(l):
            i += 1
            continue
        m = OTHER.match(l) if l.startswith("== ") else None
        if m:
            cur = None
            i += 1
            continue
        if cur is None:
            raise ValueError("line %d comes before any file's header: %r" % (i + 1, l[:120]))
        m = RUN.match(l)
        if m:
            first, last, copies, name, each = int(m.group(1)), int(m.group(2)), int(m.group(3)), m.group(4), int(m.group(6))
            gap = 0 if m.group(7) == "no" else int(m.group(7))
            if (int(m.group(8)), int(m.group(9))) != (first, each + gap):
                raise ValueError("a fold at line %d of the view starts its copies at line %s+%sk, not %d+%dk" % (i + 1, m.group(8), m.group(9), first, each + gap))
            at(first, "a fold")
            if last != first + copies * each + (copies - 1) * gap - 1:
                raise ValueError("a fold at line %d of the view ends at %d, its copies at %d" % (i + 1, last, first + copies * each + (copies - 1) * gap - 1))
            i += 1
            if not m.group(5):
                templates[name] = [x[4:] for x in lines[i:i + each]]
                if not all(x.startswith("  | ") for x in lines[i:i + each]):
                    raise ValueError("template lines at line %d of the view" % (i + 1))
                i += each
            vals = {}
            while i < len(lines) and lines[i].startswith("  «"):
                k, items = lines[i][3:].split("» = ", 1)
                vals[k] = [str(v) for v in decode(items.split(", "))]
                i += 1
            if i < len(lines) and lines[i].startswith("  rows of "):
                ks = [x[1] for x in lines[i][len("  rows of "):].split(", one per copy:")[0].split("/")]
                i += 1
                rows = []
                while i < len(lines) and lines[i].startswith("    "):
                    rows += [r.split("/") for r in lines[i][4:].split(" ")]
                    i += 1
                if len(rows) != copies or any(len(r) != len(ks) for r in rows):
                    raise ValueError("a fold of %d copies has %d rows (%s numbers each)" % (copies, len(rows), sorted({len(r) for r in rows})))
                for j, k in enumerate(ks):
                    vals[k] = [r[j] for r in rows]
            for k, v in vals.items():
                if len(v) != copies:
                    raise ValueError("«%s» has %d values for %d copies" % (k, len(v), copies))
            for u in range(copies):
                if u:
                    cur["lines"] += [""] * gap
                for t in templates[name]:
                    cur["lines"].append(TOK.sub(lambda t: (vals[t.group(1)][u].zfill(int(t.group(2))) if t.group(2) else vals[t.group(1)][u]), t))
            expected = last + 1
            continue
        n, _, text = l.partition(" ")
        if not n.isdigit():
            raise ValueError("line %d is neither numbered nor a fold: %r" % (i + 1, l[:120]))
        at(int(n), "a numbered line")
        c = CUT.search(text)
        if c:
            total, upto, py = int(c.group(2).replace(",", "")), int(c.group(3).replace(",", "")), int(c.group(4))
            if int(c.group(1)) != int(n) or upto != py:
                raise ValueError("the cut note at line %d of the view names line %s, character %d and line[%d:]" % (i + 1, c.group(1), upto, py))
            cur["cuts"][len(cur["lines"])] = (total, upto, text[:c.start()])
        cur["lines"].append(text)
        expected += 1
        i += 1
    return files


def same(got, cuts, want, max_chars=None):
    """The first line (from 0) where the rebuilt lines are not the file's, or None: a cut line is
    the file's when its note's count is the line's in characters and the text shown is the line up
    to the character the note names; at a --max (`max_chars`), a line cut that an output of its
    own would hold (its numbered line within --max less 2,500 for the header and the stop line)
    is a fault too."""
    for j in range(max(len(got), len(want))):
        if j >= len(got) or j >= len(want):
            return j
        if j in cuts:
            total, upto, shown = cuts[j]
            if total != len(want[j]) or shown != want[j][:upto]:
                return j
            if max_chars is not None and len(("%d %s\n" % (j + 1, want[j])).encode("utf-8")) <= max_chars - 2_500:
                return j
        elif got[j] != want[j]:
            return j
    return None


def key_material(run):
    """secrets.rs key_material: '+' or '/', or capitals and small letters each more than a sixth of
    its letters, or hex of 16 or more with digits and letters."""
    if "+" in run or "/" in run:
        return True
    letters = sum(c.isalpha() for c in run)
    caps = sum(c.isupper() for c in run)
    hexy = len(run) >= 16 and all(c in "0123456789abcdefABCDEF" for c in run) and 0 < letters < len(run)
    return hexy or (letters > 0 and caps * 6 > letters and (letters - caps) * 6 > letters)


def key_scrubbed(line):
    """A line inside a private key's block as the hider leaves it (secrets.rs scrub): each run of
    base64 bytes that is key material (16 or more) or a quoted string's whole text taken out (a run
    right after a backslash keeps its first byte)."""
    def keep(m):
        run = m.group(0)
        before = line[m.start() - 1] if m.start() > 0 else ""
        after = line[m.end():]
        whole = before in ("\"", "'") and (after.startswith(before) or after.startswith("\\n") or after.startswith("\\r"))
        if whole or (len(run) >= 16 and key_material(run)):
            return run[0] if before == "\\" else ""
        return run
    return re.sub(r"[A-Za-z0-9+/=]+", keep, line)


def want_lines(text):
    return text[:-1].split("\n") if text.endswith("\n") else (text.split("\n") if text else [])


def skipped_by_notes(notes):
    """A file whose view hides a secret or holds bytes that are not UTF-8 (said in the header's
    notes, never by its path)."""
    return "hidden" in notes or "not all UTF-8" in notes


def follow(search, batch, max_chars):
    """The views of a read of these files at `max_chars`, and of each read-on after it, as a
    reader following the read-on lines makes them."""
    views, args, extra = [], list(batch), []
    for _ in range(1000):
        r = subprocess.run([search, "read", "--max", str(max_chars), *extra, *args], env=ENV, capture_output=True, timeout=300)
        view = r.stdout.decode("utf-8")
        if len(r.stdout) > max_chars:
            raise ValueError("a view of %d bytes, past --max %d: %r" % (len(r.stdout), max_chars, view.rstrip("\n").split("\n")[-1][:160]))
        views.append(view)
        last = view.rstrip("\n").split("\n")[-1]
        m = STOP.match(last)
        if not m:
            return views
        line = m.group(1)
        k = re.search(r"read on: the same command with --skip (\d+) --from (\d+)", last)
        if k:
            extra = ["--skip", k.group(1), "--from", line]
        else:
            rest = last.split(" read --from %s " % line, 1)[1]
            # (a --max given is kept in the read-on; names that start with "-" come after "--")
            if max_chars != 28000 and not rest.startswith("--max %d " % max_chars):
                raise ValueError("a read-on at --max %d does not keep it: %r" % (max_chars, last[:200]))
            words = shlex.split(rest)
            if any(w.startswith("-") and w not in ("--", "--max") and not w.isdigit() for w in words) and "--" not in words:
                raise ValueError("a read-on names a file that starts with '-' without '--': %r" % last[:200])
            args, extra = words, ["--from", line]
    raise ValueError("more than 1,000 read-ons")


def check_together(views, batch, bad, label, max_chars=None):
    """Each file of `batch` rebuilt from the views (its pieces in line order, nothing missing; its
    header there even when it is empty)."""
    pieces = {}
    for v in views:
        for f in rebuild_all(v):
            pieces.setdefault(f["path"], []).append(f)
    for p in batch:
        got, nxt, cuts = [], 1, {}
        want = want_lines(open(p, "rb").read().decode("utf-8"))
        if p not in pieces:
            bad.append((p, "%s: no header for it in any view" % label))
            continue
        for f in sorted(pieces.get(p, []), key=lambda f: f["first"] or 0):
            if f["count"] != len(want):
                bad.append((p, "%s: its header says %d lines, the file has %d" % (label, f["count"], len(want))))
                break
            if f["first"] is None:
                continue
            if f["first"] != nxt:
                bad.append((p, "%s: a piece starts at line %d, the file's next is %d" % (label, f["first"], nxt)))
                break
            cuts.update({len(got) + j: c for j, c in f["cuts"].items()})
            got += f["lines"]
            nxt += len(f["lines"])
        else:
            k = same(got, cuts, want, max_chars)
            if k is not None:
                bad.append((p, "%s: line %d differs: %r vs %r" % (label, k + 1, (got[k] if k < len(got) else None) if k not in cuts else cuts[k][2][:80] + " …", want[k][:200] if k < len(want) else None)))


def main(argv):
    if len(argv) < 3:
        raise SystemExit(__doc__)
    search, work, dirs = os.path.abspath(argv[0]), argv[1], argv[2:]
    os.makedirs(work, exist_ok=True)
    files = []
    for d in dirs:
        for root, ds, fs in os.walk(d):
            ds[:] = sorted(x for x in ds if not x.startswith(".") and x not in ("node_modules", "target", "__pycache__"))
            for f in sorted(fs):
                if os.path.splitext(f)[1] in EXT:
                    files.append(os.path.join(root, f))
    files = files[:3000]
    # (the files made here: the paths a project's files may not try)
    made = os.path.join(work, "made")
    os.makedirs(made, exist_ok=True)
    samples = {
        "long_ascii.txt": "first\n" + "".join("w%05d " % k for k in range(5_800)) + "\nlast\n",
        "long_utf8.txt": "first\n" + "".join("é%05d" % k for k in range(6_000)) + "\nlast\n",
        "sensor.csv": "".join("%d.%d,%d.%d,%d.%d\n" % (k % 97, k % 89, k % 83, k % 79, k % 73, k % 71) for k in range(3_400)) + "0.0,0.0,0.0\n" * 6_600,
        "events.jsonl": "".join('{"id":%d,"ts":%d,"body":"%s"}\n' % (k, 1_700_000_000 + k, "abcdefgh" * 4_600) for k in range(6)),
        "wide.txt": "".join("%d %d %s\n" % (c, l, "w" * 600) for c in range(4) for l in range(60)),
        # (a line an output of its own holds, after a file that fills most of one)
        "mid.json": '{"k":"%s"}\n{}\n' % ("v" * 10_000),
        "-dash-big.txt": "".join("line %s of %s words here\n" % (chr(97 + k % 26) * 3 + str(k), chr(98 + k % 24) * 2) for k in range(400)),
    }
    for name, text in samples.items():
        with open(os.path.join(made, name), "w", encoding="utf-8") as f:
            f.write(text)
        files.append(os.path.join(made, name))
    bad = []
    skip_set = set()
    folded = skipped = raw = viewed = checked = hidden_checked = 0
    for p in files:
        data = open(p, "rb").read()
        r = subprocess.run([search, "read", "--max", "100000000", p], env=ENV, capture_output=True, timeout=120)
        if r.returncode != 0:
            bad.append((p, "exit %d: %s" % (r.returncode, r.stderr[-200:])))
            continue
        view = r.stdout.decode("utf-8")
        head = HEAD.match(view.split("\n", 1)[0])
        if b"\0" in data or (head and skipped_by_notes(head.group(3))):
            skipped += 1
            skip_set.add(p)
            # (secrets hidden: still every line count, and every line but those hidden or left
            # empty for a key)
            if head and "hidden" in head.group(3) and "not all UTF-8" not in head.group(3) and b"\0" not in data:
                try:
                    got = rebuild_all(view)[0]
                    want = want_lines(data.decode("utf-8"))
                    if got["count"] != len(want) or len(got["lines"]) != len(want):
                        bad.append((p, "secrets hidden: %d lines shown, the header says %d, the file has %d" % (len(got["lines"]), got["count"], len(want))))
                    else:
                        k = next((j for j, (g, w) in enumerate(zip(got["lines"], want)) if g != w and g != "" and "[hidden:" not in g and g != key_scrubbed(w)), None)
                        if k is not None:
                            bad.append((p, "secrets hidden: line %d differs where nothing is hidden: %r vs %r" % (k + 1, got["lines"][k][:120], want[k][:120])))
                        else:
                            hidden_checked += 1
                except Exception as e:  # noqa: BLE001 - every failure is a finding
                    bad.append((p, "unreadable view with secrets hidden: %s" % e))
            continue
        try:
            got = rebuild_all(view)
            if len(got) != 1 or (got[0]["first"] not in (None, 1)):
                raise ValueError("%d files in the view of one, the first line %s" % (len(got), got[0]["first"] if got else None))
        except Exception as e:  # noqa: BLE001 - every failure is a finding
            bad.append((p, "unreadable view: %s" % e))
            continue
        text = data.decode("utf-8")
        want = want_lines(text)
        lines, notes = got[0]["lines"], got[0]["notes"]
        if got[0]["count"] != len(want):
            bad.append((p, "its header says %d lines, the file has %d" % (got[0]["count"], len(want))))
            continue
        k = same(lines, got[0]["cuts"], want)
        if k is not None:
            bad.append((p, "line %d differs: %r vs %r" % (k + 1, lines[k][:200] if k < len(lines) else None, want[k][:200] if k < len(want) else None)))
            continue
        if ("no newline at the end" in notes) == text.endswith("\n") and text:
            bad.append((p, "the end-of-file note is wrong"))
            continue
        checked += 1
        folded += "copies of block" in view
        raw += len(data)
        viewed += len(view.encode("utf-8"))
    # the files of each folder read together (blocks of one shape share a template across them):
    # whole, then 28,000 characters at a time following each read-on to the end
    clean = [p for p in files if p not in skip_set and p not in {b for b, _ in bad}]
    by_dir = {}
    for p in clean:
        by_dir.setdefault(os.path.dirname(p), []).append(p)
    together = parted = alone = 0
    for d, group in sorted(by_dir.items()):
        for k in range(0, len(group), 40):
            batch = group[k:k + 40]
            try:
                if len(batch) > 1:
                    check_together(follow(search, batch, 100000000), batch, bad, "read with its folder")
                views = follow(search, batch, 28000)
                parted += len(views) > 1
                check_together(views, batch, bad, "read with its folder, 28,000 characters at a time", 28000)
            except Exception as e:  # noqa: BLE001 - every failure is a finding
                bad.append((d, "unreadable view of %d files together: %s" % (len(batch), e)))
                continue
            together += len(batch)
    # a sample alone at a small --max (every 25th file, and the made ones)
    for p in [q for i, q in enumerate(clean) if i % 25 == 0 or q.startswith(made + os.sep)]:
        try:
            check_together(follow(search, [p], 3000), [p], bad, "read alone, 3,000 characters at a time", 3000)
            alone += 1
        except Exception as e:  # noqa: BLE001 - every failure is a finding
            bad.append((p, "unreadable views at 3,000: %s" % e))
    # a name that starts with "-", read from its folder: the read-on puts "--" before it
    r = subprocess.run([search, "read", "--max", "3000", "--", "-dash-big.txt", "mid.json"], cwd=made, env=ENV, capture_output=True, timeout=120)
    last = r.stdout.decode("utf-8").rstrip("\n").split("\n")[-1]
    if not (STOP.match(last) and " --max 3000 -- -dash-big.txt mid.json" in last):
        bad.append((made, "the read-on of a name that starts with '-': %r" % last[:200]))
    # several files without --max: a view within 10,000 characters and no cut line, or the outline
    r = subprocess.run([search, "read", os.path.join(made, "long_ascii.txt"), os.path.join(made, "mid.json")], env=ENV, capture_output=True, timeout=120)
    v = r.stdout.decode("utf-8")
    if not (v.startswith("2 files,") or (len(r.stdout) <= 10_000 and not any(CUT.search(l) for l in v.split("\n")))):
        bad.append((made, "several files without --max: neither the outline nor a view within 10,000 characters with no cut line: %r" % v[:200]))
    for p, why in bad[:20]:
        print("FAIL:", p, why)
    print("read_check: %d files, %d checked, %d folded, %d skipped (secrets hidden or not UTF-8; %d of them checked but for the hidden lines), %d differ; %s bytes of files, %s bytes of views; %d read again with their folders' other files (%d batches read on in parts); %d read alone at 3,000" % (
        len(files), checked, folded, skipped, hidden_checked, len(bad), "{:,}".format(raw), "{:,}".format(viewed), together, parted, alone))
    ok = not bad and checked > 0
    print("read_check: ALL REBUILT" if ok else "read_check: FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
