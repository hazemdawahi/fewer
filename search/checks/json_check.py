"""json_check.py SEARCH [N [WORK]]: our JSON reader and writer against Python's json module (python3 -I).

N random documents (default 20000): valid ones written by json.dumps in several styles
(compact, spaced, indented, ASCII-only or not) from random values (nested arrays and objects
with duplicate keys, strings of every kind of character, escapes and surrogate pairs, whole
numbers big and small, fractions and exponents, true, false, null), and invalid ones made from
them (a byte cut, added, changed or doubled; cut short; bytes that are not UTF-8; a lone
surrogate; a control byte inside a string). `fewer json-check` reads each and writes it
back compact. Where Python reads a document, ours must too, and Python must read our compact
text back to the same value (key order and duplicate keys kept, numbers equal); where Python
refuses one, ours must too. Known, intended differences are counted apart: Python takes a lone
surrogate (\\ud800) and NaN/Infinity; we refuse them, as RFC 8259 asks; and we refuse nesting
deeper than 64 levels.

WORK: the scratch folder for safe_env.py, which gives fewer a made-up HOME and agents'
folders (default: a folder under the crate's target/check-tmp/).
"""
import importlib.util, json, os, random, subprocess, sys

SEARCH = sys.argv[1]
N = int(sys.argv[2]) if len(sys.argv) > 2 else 20000
spec = importlib.util.spec_from_file_location("safe_env", os.path.join(os.path.dirname(os.path.abspath(__file__)), "safe_env.py"))
safe_env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safe_env)
safe_env.setup(sys.argv[3] if len(sys.argv) > 3 else None, "json_check")
rnd = random.Random(5)


def rstr():
    n = rnd.choice([0, 1, 2, 5, 12, 40])
    out = []
    for _ in range(n):
        r = rnd.random()
        if r < 0.5:
            out.append(chr(rnd.randint(0x20, 0x7E)))
        elif r < 0.6:
            out.append(rnd.choice(['"', "\\", "/", "\b", "\f", "\n", "\r", "\t"]))
        elif r < 0.7:
            out.append(chr(rnd.randint(0, 0x1F)))
        elif r < 0.85:
            out.append(chr(rnd.randint(0x80, 0xD7FF)))
        elif r < 0.95:
            out.append(chr(rnd.randint(0x10000, 0x10FFFF)))
        else:
            out.append(chr(rnd.choice([0x7F, 0x2028, 0x2029, 0xFEFF, 0xFFFF])))
    return "".join(out)


def rnum():
    r = rnd.random()
    if r < 0.4:
        return rnd.randint(-1000, 1000)
    if r < 0.6:
        return rnd.randint(-(10 ** 30), 10 ** 30)
    if r < 0.9:
        return rnd.uniform(-1e6, 1e6) * 10 ** rnd.randint(-300, 300)
    return rnd.choice([0.0, -0.0, 1e-320, 1.7976931348623157e308])


def rval(depth=0):
    r = rnd.random()
    if depth > 6 or r < 0.35:
        return rnd.choice([rstr, rnum, lambda: rnd.choice([True, False, None])])()
    if r < 0.65:
        return [rval(depth + 1) for _ in range(rnd.randint(0, 5))]
    return ("obj", [(rstr() if rnd.random() < 0.9 else "dup", rval(depth + 1)) for _ in range(rnd.randint(0, 5))])


def dump(v, style):
    """json.dumps of a value whose objects are lists of pairs (keeps duplicates and order)."""
    ea = style % 2 == 0
    sep = [(",", ":"), (", ", ": "), (",", ": ")][style % 3]
    if isinstance(v, tuple) and v[0] == "obj":
        inner = sep[0].join(dump(k, style) + sep[1] + dump(x, style) for k, x in v[1])
        return "{" + inner + "}"
    if isinstance(v, list):
        return "[" + sep[0].join(dump(x, style) for x in v) + "]"
    return json.dumps(v, ensure_ascii=ea)


def py_parse(b):
    """Python's reading of a document: ('ok', value) or ('err', why)."""
    try:
        s = b.decode("utf-8")
    except UnicodeDecodeError:
        return ("err", "not UTF-8")
    try:
        def no_const(x):
            raise ValueError("constant " + x)
        return ("ok", json.loads(s, object_pairs_hook=lambda p: ("obj", p), parse_constant=no_const))
    except (ValueError, RecursionError) as e:
        return ("err", str(e))


def norm(v):
    if isinstance(v, tuple):
        return ("obj", [(k, norm(x)) for k, x in v[1]])
    if isinstance(v, list):
        return [norm(x) for x in v]
    if isinstance(v, float) and v != v:
        return "nan"
    return v


def mutate(b):
    r = rnd.random()
    if not b:
        return b"\xff"
    k = rnd.randrange(len(b))
    if r < 0.2:
        return b[:k] + b[k + 1:]
    if r < 0.35:
        return b[:k] + bytes([rnd.choice(b'{}[]":,\\0e.-+ \t\nx\x00\x1f\x80\xff')]) + b[k:]
    if r < 0.5:
        return b[:k] + bytes([rnd.randrange(256)]) + b[k + 1:]
    if r < 0.6:
        return b[:k]
    if r < 0.7:
        return b + b[k:k + 3]
    if r < 0.8:
        return b'["' + rnd.choice([b"\\ud800", b"\\udc00x", b"\\ud800\\u0041", b"\x01", b"\xed\xa0\x80", b"\xc3"]) + b'"]'
    if r < 0.9:
        return rnd.choice([b"NaN", b"[Infinity]", b"-Infinity", b"01", b"1.", b".5", b"+1", b"[1,]", b"{\"a\":1,}", b"\xef\xbb\xbf[]", b"[" * 65 + b"]" * 65, b"[" * 64 + b"]" * 64])
    return b[:k] + b[k:k + 1] * 2 + b[k + 1:]


docs = []
for i in range(N):
    v = rval()
    style = rnd.randrange(6)
    t = dump(v, style)
    if style == 5:
        # pretty-printed (objects without duplicate keys here)
        t = json.dumps(json.loads(t), indent=rnd.choice([1, 2, 4]), ensure_ascii=rnd.random() < 0.5)
    b = t.encode("utf-8", "surrogatepass")
    if rnd.random() < 0.35:
        b = mutate(b)
    if rnd.random() < 0.1:
        b = rnd.choice([b" ", b"\t", b"\r\n", b"\n "]) + b + rnd.choice([b"", b" ", b"\n"])
    docs.append(b)

inp = b"".join(d.hex().encode() + b"\n" for d in docs)
res = subprocess.run([SEARCH, "json-check"], input=inp, capture_output=True, check=True).stdout.split(b"\n")[:-1]
assert len(res) == len(docs), (len(res), len(docs))
stats = dict(valid=0, invalid=0, intended=0, bad=0)
for d, r in zip(docs, res):
    ours_ok = r.startswith(b"OK ")
    py = py_parse(d)
    if py[0] == "ok":
        if ours_ok:
            back = py_parse(r[3:])
            if back[0] != "ok" or norm(back[1]) != norm(py[1]):
                stats["bad"] += 1
                print("DIFFERENT VALUE:", d[:120], "->", r[:120])
            else:
                stats["valid"] += 1
        else:
            # intended: lone surrogates, nesting deeper than 64
            why = r.decode("utf-8", "replace")
            if "surrogate" in why or "nested deeper" in why:
                stats["intended"] += 1
            else:
                stats["bad"] += 1
                print("WE REFUSE, PYTHON READS:", d[:120], why)
    else:
        if ours_ok:
            stats["bad"] += 1
            print("WE READ, PYTHON REFUSES:", d[:120], py[1][:80])
        else:
            stats["invalid"] += 1
print("json_check: %d documents: %d valid alike, %d invalid alike, %d intended differences, %d different" % (len(docs), stats["valid"], stats["invalid"], stats["intended"], stats["bad"]))
if stats["bad"]:
    sys.exit(1)
print("json_check: ALL ALIKE")
