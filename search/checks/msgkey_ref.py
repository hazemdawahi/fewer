#!/usr/bin/env python3
"""The stable message key, rule v1 (msgkey.rs), written again in Python, on bytes.

This is the reference for src/msgkey.rs. Both must give every line the same key, so both
give the same keys and the same counts per key. See the module doc of msgkey.rs for the rule.
Nothing here is shared with the Rust code: the clean step, the JSON reader and the rule are
all written again.

Run with python3 -I. Usage:
  msgkey_ref.py --dump FILE      "<lines>\\t<key>" per key, in order of first appearance
  msgkey_ref.py --lines FILE     each line's key, one per line
  msgkey_ref.py --check [--bin CMD] [--seed S] [PATH ...]
      compares the Rust keys (CMD --lines FILE, CMD --dump FILE) with these on every file
      of the PATHs (default: the folder FEWER_SAMPLES names, else the crate's tests/data),
      and checks that the keys of each file cut into pieces equal the keys of the whole file
  msgkey_ref.py --fuzz N [--bin CMD] [--seed S]
      compares the keys of N made-up lines (random bytes, made-up JSON, CRI and Docker lines,
      changed lines of the same default samples, long lines) with the Rust keys
  --work DIR (with --check and --fuzz): the scratch folder for safe_env.py, which gives the
      Rust command a made-up HOME and agents' folders (default: the WORK of the check that
      started this one, else a folder under the crate's target/check-tmp/)

CMD is a command that takes "--dump FILE" and "--lines FILE" (default: the crate's
target/release/fewer with its `keys` command). Keys are
written as msgkey::write_key writes them: '\\' as \\\\, control bytes as \\xHH. Exit status 0
when everything is equal. `fewer keys --dump` counts records where
--dump here counts lines: they are equal where every record is one line, as in the public
samples; on files with multi-line records, compare --lines only (the fuzz does).
"""

import os
import random
import shlex
import subprocess
import sys

RULE = 1
MESSAGE_MAX = 64 * 1024
JSON_MAX = 1 << 20
MESSAGE_FIELDS = ["msg", "message", "log", "MESSAGE", "eventMessage", "event", "text", "error", "err"]
LEVEL_FIELDS = ["level", "severity", "lvl", "levelname", "log.level", "messageType", "PRIORITY"]
BOM = b"\xef\xbb\xbf"
MONTHS = [b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov", b"dec"]
WEEKDAYS = [b"mon", b"tue", b"wed", b"thu", b"fri", b"sat", b"sun"]

CRATE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# (read before safe_env.py makes this process's environment the safe one)
SAMPLES = os.environ.get("FEWER_SAMPLES")

# byte classes
DIGIT = [0x30 <= c <= 0x39 for c in range(256)]
ALPHA = [(0x41 <= c <= 0x5A) or (0x61 <= c <= 0x7A) for c in range(256)]
ALNUM = [DIGIT[c] or ALPHA[c] for c in range(256)]
HEXDIGIT = [DIGIT[c] or (0x41 <= c <= 0x46) or (0x61 <= c <= 0x66) for c in range(256)]
HEXLETTER = [(0x41 <= c <= 0x46) or (0x61 <= c <= 0x66) for c in range(256)]
CAND = [ALNUM[c] or c >= 0x80 or c in b".:/@%+~-" for c in range(256)]
BLANK = [c == 0x20 or c < 0x20 or c == 0x7F for c in range(256)]


def at(c, i, x):
    return i < len(c) and c[i] == x


def digits(c, i):
    j = i
    n = len(c)
    while j < n and DIGIT[c[j]]:
        j += 1
    return max(0, j - i)


# ---------------------------------------------------------------------------------------
# Step 1: the clean text (clean.rs's without_end and strip_escapes; formats.rs's clean_line)

ESC = 0x1B
CONTROL = bytes(c for c in range(0x20) if c != 0x09) + b"\x7f"


def without_end(b):
    if b.endswith(b"\n"):
        b = b[:-1]
    if b.endswith(b"\r"):
        b = b[:-1]
    return b


def strip_escapes(b):
    if ESC not in b:
        return b
    out = bytearray()
    i = 0
    n = len(b)
    while i < n:
        if b[i] != ESC:
            out.append(b[i])
            i += 1
            continue
        i += 1
        if i >= n:
            break
        c = b[i]
        if c == 0x5B:  # ESC [ ... final byte 0x40-0x7E
            i += 1
            while i < n and not (0x40 <= b[i] <= 0x7E):
                i += 1
            i += 1
        elif c == 0x5D:  # ESC ] ... BEL or ESC backslash
            i += 1
            while i < n:
                if b[i] == 0x07:
                    i += 1
                    break
                if b[i] == ESC and i + 1 < n and b[i + 1] == 0x5C:
                    i += 2
                    break
                i += 1
        else:  # ESC, intermediate bytes 0x20-0x2F, one final byte
            while i < n and 0x20 <= b[i] <= 0x2F:
                i += 1
            i += 1
    return bytes(out)


def clean_line(line):
    l = without_end(line)
    if len(l.translate(None, CONTROL)) == len(l):
        return l
    b = strip_escapes(l)
    if b"\r" in b:
        piece = b""
        for p in reversed(b.split(b"\r")):
            if p:
                piece = p
                break
    else:
        piece = b
    return piece.translate(None, CONTROL)


# ---------------------------------------------------------------------------------------
# JSON, read as json.rs reads it: RFC 8259, one value then only whitespace, nesting at most
# 64 deep, strings UTF-8 with no raw control bytes, \u escapes with surrogate pairs (a lone
# surrogate is an error), numbers kept as their text, objects keeping order and repeats.


class JsonError(Exception):
    pass


class Num:
    __slots__ = ("text",)

    def __init__(self, text):
        self.text = text


class Obj(list):
    """A JSON object: its (name, value) pairs in order, repeated names kept."""

    __slots__ = ()


class Arr(list):
    __slots__ = ()


ABSENT = object()
MAX_DEPTH = 64
HEX = b"0123456789abcdefABCDEF"


class Parser:
    def __init__(self, b):
        self.b = b
        self.i = 0

    def ws(self):
        b = self.b
        i = self.i
        n = len(b)
        while i < n and b[i] in (0x20, 0x09, 0x0A, 0x0D):
            i += 1
        self.i = i

    def lit(self, word, v):
        if self.b.startswith(word, self.i):
            self.i += len(word)
            return v
        raise JsonError("not JSON")

    def value(self, depth):
        b = self.b
        if self.i >= len(b):
            raise JsonError("the text ends where a value should be")
        c = b[self.i]
        if c == 0x7B or c == 0x5B:
            if depth >= MAX_DEPTH:
                raise JsonError("nested too deep")
            close = 0x7D if c == 0x7B else 0x5D
            self.i += 1
            out = Obj() if c == 0x7B else Arr()
            self.ws()
            if at(b, self.i, close):
                self.i += 1
                return out
            while True:
                self.ws()
                if c == 0x7B:
                    if not at(b, self.i, 0x22):
                        raise JsonError("expected a key in quotes")
                    k = self.string()
                    self.ws()
                    if not at(b, self.i, 0x3A):
                        raise JsonError("expected ':'")
                    self.i += 1
                    self.ws()
                    out.append((k, self.value(depth + 1)))
                else:
                    out.append(self.value(depth + 1))
                self.ws()
                if at(b, self.i, 0x2C):
                    self.i += 1
                elif at(b, self.i, close):
                    self.i += 1
                    return out
                else:
                    raise JsonError("expected ',' or a closing bracket")
        if c == 0x22:
            return self.string()
        if c == 0x74:
            return self.lit(b"true", True)
        if c == 0x66:
            return self.lit(b"false", False)
        if c == 0x6E:
            return self.lit(b"null", None)
        if c == 0x2D or DIGIT[c]:
            return self.number()
        raise JsonError("not JSON")

    def number(self):
        b = self.b
        n = len(b)
        s = i = self.i
        if at(b, i, 0x2D):
            i += 1
        if at(b, i, 0x30):
            i += 1
        elif i < n and 0x31 <= b[i] <= 0x39:
            while i < n and DIGIT[b[i]]:
                i += 1
        else:
            raise JsonError("bad number")
        if at(b, i, 0x2E):
            i += 1
            d = i
            while i < n and DIGIT[b[i]]:
                i += 1
            if i == d:
                raise JsonError("bad number")
        if i < n and b[i] in (0x65, 0x45):
            i += 1
            if i < n and b[i] in (0x2B, 0x2D):
                i += 1
            d = i
            while i < n and DIGIT[b[i]]:
                i += 1
            if i == d:
                raise JsonError("bad number")
        self.i = i
        return Num(b[s:i].decode("ascii"))

    def hex4(self):
        h = self.b[self.i:self.i + 4]
        if len(h) < 4 or any(x not in HEX for x in h):
            raise JsonError("bad \\u escape")
        self.i += 4
        return int(h, 16)

    def string(self):
        b = self.b
        n = len(b)
        self.i += 1
        out = []
        while True:
            s = i = self.i
            while i < n:
                x = b[i]
                if x == 0x22 or x == 0x5C or x < 0x20:
                    break
                i += 1
            self.i = i
            if i > s:
                try:
                    out.append(b[s:i].decode("utf-8"))
                except UnicodeDecodeError:
                    raise JsonError("not UTF-8")
            if i >= n:
                raise JsonError("the text ends inside a string")
            x = b[i]
            if x == 0x22:
                self.i = i + 1
                return "".join(out)
            if x != 0x5C:
                raise JsonError("a control byte inside a string")
            i += 1
            if i >= n:
                raise JsonError("the text ends inside a string")
            e = b[i]
            self.i = i + 1
            simple = {0x22: '"', 0x5C: "\\", 0x2F: "/", 0x62: "\b", 0x66: "\f", 0x6E: "\n", 0x72: "\r", 0x74: "\t"}
            if e in simple:
                out.append(simple[e])
            elif e == 0x75:
                hi = self.hex4()
                if 0xD800 <= hi < 0xDC00:
                    if self.b[self.i:self.i + 2] != b"\\u":
                        raise JsonError("a lone surrogate")
                    self.i += 2
                    lo = self.hex4()
                    if not (0xDC00 <= lo < 0xE000):
                        raise JsonError("a lone surrogate")
                    cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                elif 0xDC00 <= hi < 0xE000:
                    raise JsonError("a lone surrogate")
                else:
                    cp = hi
                out.append(chr(cp))
            else:
                raise JsonError("bad escape in a string")


def parse_json(b):
    p = Parser(b)
    p.ws()
    v = p.value(0)
    p.ws()
    if p.i != len(b):
        raise JsonError("more after the end of the value")
    return v


def get(o, key):
    """The first member named key of an object; ABSENT for anything else."""
    if isinstance(o, Obj):
        for k, v in o:
            if k == key:
                return v
    return ABSENT


# ---------------------------------------------------------------------------------------
# Step 2: the message part


def line_key(line):
    return clean_key(clean_line(line))


def clean_key(c):
    if c.startswith(BOM):
        c = c[3:]
    o = json_line(c)
    if o is not None:
        return json_key(o)
    t = cri_text(c)
    if t is not None:
        o = json_line(t)
        return json_key(o) if o is not None else text_key(t)
    return text_key(c)


def json_line(c):
    if not c.startswith(b"{") or len(c) > JSON_MAX:
        return None
    try:
        v = parse_json(c)
    except JsonError:
        return None
    return v if isinstance(v, Obj) else None


def rfc3339_end(c):
    def d(i, n):
        return digits(c, i) == n

    if not (d(0, 4) and at(c, 4, 0x2D) and d(5, 2) and at(c, 7, 0x2D) and d(8, 2) and at(c, 10, 0x54)
            and d(11, 2) and at(c, 13, 0x3A) and d(14, 2) and at(c, 16, 0x3A) and d(17, 2)):
        return None
    i = 19
    if at(c, i, 0x2E):
        n = digits(c, i + 1)
        if not 1 <= n <= 9:
            return None
        i += 1 + n
    if at(c, i, 0x5A):
        return i + 1
    if i < len(c) and c[i] in (0x2B, 0x2D) and d(i + 1, 2) and at(c, i + 3, 0x3A) and d(i + 4, 2):
        return i + 6
    return None


def cri_text(c):
    t = rfc3339_end(c)
    if t is None:
        return None
    rest = c[t:]
    if not (rest.startswith(b" stdout ") or rest.startswith(b" stderr ")):
        return None
    rest = rest[8:]
    if not rest or rest[0] not in (0x50, 0x46):  # P, F
        return None
    rest = rest[1:]
    if not rest:
        return rest
    return rest[1:] if rest[0] == 0x20 else None


# JSON lines (5.2)


def json_key(o):
    def has(k):
        return get(o, k) is not ABSENT

    log = get(o, "log")
    if has("stream") and has("time") and isinstance(log, str):
        m = clean_line(log.encode("utf-8"))
        inner = json_line(m)
        if inner is not None:
            return json_key(inner)
        return leveled(o, m)
    if has("sessionId") and has("type"):
        return session_key(claude_parts(o))
    if (has("payload") and has("type") and has("timestamp")) or has("record_type"):
        return session_key(codex_parts(o))
    m = message_field(o)
    if m is not None:
        return leveled(o, m.encode("utf-8"))
    return b"{" + b",".join(k.encode("utf-8") for k, _ in o) + b"}"


def message_field(o):
    for k in MESSAGE_FIELDS:
        v = get(o, k)
        if isinstance(v, str):
            return v
    return None


def level_field(o):
    for k in LEVEL_FIELDS:
        v = get(o, k)
        if k == "log.level" and v is ABSENT:
            v = get(get(o, "log"), "level")
        if isinstance(v, str):
            return v
        if isinstance(v, Num):
            return v.text
    return None


def leveled(o, m):
    k = text_key(m)
    level = level_field(o)
    if level is None:
        return k
    out = b"{" + level.encode("utf-8") + b"}"
    return out + b" " + k if k else out


def session_key(parts):
    return b"{session" + b"".join(b" " + p.encode("utf-8") for p in parts) + b"}"


def push_str(p, v):
    if isinstance(v, str):
        p.append(v)


def claude_parts(o):
    p = []
    push_str(p, get(o, "type"))
    m = get(o, "message")
    push_str(p, get(m, "role"))
    blocks = get(m, "content")
    if isinstance(blocks, Arr):
        push_str(p, get(blocks[0], "type") if blocks else ABSENT)
        for b in blocks:
            name = get(b, "name")
            if isinstance(name, str):
                p.append(name)
                break
    return p


def codex_parts(o):
    p = []
    push_str(p, get(o, "type"))
    body = get(o, "payload")
    if body is ABSENT:
        body = o
    else:
        push_str(p, get(body, "type"))
    push_str(p, get(body, "role"))
    blocks = get(body, "content")
    if isinstance(blocks, Arr):
        push_str(p, get(blocks[0], "type") if blocks else ABSENT)
    push_str(p, get(body, "name"))
    return p


# ---------------------------------------------------------------------------------------
# Steps 3 to 5


def text_key(m):
    if len(m) <= MESSAGE_MAX:
        return rule(m)
    k = rule(m[:MESSAGE_MAX])
    return (k + b" " if k else k) + b"<long>"


def rule(m):
    out = bytearray()
    gap = False
    quotes_close = True
    i = 0
    n = len(m)
    while i < n:
        b = m[i]
        if BLANK[b]:
            gap = True
            i += 1
            continue
        if gap and out:
            out.append(0x20)
        gap = False
        if b == 0x22:
            e = quote_end(m, i) if quotes_close else None
            if e is not None:
                out += b"<str>"
                i = e
            else:
                quotes_close = False
                out.append(0x22)
                i += 1
            continue
        if not CAND[b]:
            out.append(b)
            i += 1
            continue
        if ALPHA[b]:
            e = spaced_time(m, i)
            if e is not None:
                out += b"<time>"
                i = e
                continue
        e = cand_end(m, i)
        classify(m[i:e], out)
        i = e
    return bytes(out)


def quote_end(m, i):
    j = i + 1
    n = len(m)
    while j < n:
        x = m[j]
        if x == 0x5C:
            j += 2
        elif x == 0x22:
            return j + 1
        else:
            j += 1
    return None


def cand_end(m, i):
    n = len(m)
    j = i
    while j < n and CAND[m[j]]:
        j += 1
    if j == i + 2 and ALPHA[m[i]] and m[i + 1] == 0x3A and at(m, j, 0x5C):
        while j < n and (CAND[m[j]] or m[j] == 0x5C):
            j += 1
    return j


def classify(c, out):
    if is_uuid(c):
        out += b"<uuid>"
    elif is_time(c):
        out += b"<time>"
    elif ipv4_end(c) == len(c):
        out += b"<ip>"
    elif is_ipv4_port(c):
        out += b"<ip>:<num>"
    elif is_hex_ip(c):
        out += b"<ip>"
    elif is_url(c):
        out += b"<url>"
    elif is_path(c):
        out += b"<path>"
    elif any(DIGIT[x] for x in c) or (len(c) >= 12 and all(HEXLETTER[x] for x in c)):
        out += b"<num>"
    else:
        out += c


def is_uuid(c):
    if len(c) != 36:
        return False
    for i, x in enumerate(c):
        if i in (8, 13, 18, 23):
            if x != 0x2D:
                return False
        elif not HEXDIGIT[x]:
            return False
    return True


def is_time(c):
    n = len(c)
    d = date_end(c)
    if d is not None:
        if d == n:
            return True
        s = c[d]
        if s in (0x54, 0x3A):  # T :
            t = tod_end(c, d + 1)
            return t is not None and zone_end(c, t) == n
        if s == 0x2D:
            if dots_tod_end(c, d + 1) == n:
                return True
            t = tod_end(c, d + 1)
            return t is not None and zone_end(c, t) == n
        return False
    t = tod_end(c, 0)
    return t is not None and zone_end(c, t) == n


def date_end(c):
    r1 = digits(c, 0)
    if r1 >= len(c):
        return None
    s = c[r1]
    if r1 == 4 and s in (0x2D, 0x2F, 0x2E):
        def ok(k):
            return k == 2 if s == 0x2E else 1 <= k <= 2

        r2 = digits(c, 5)
        if not ok(r2) or not at(c, 5 + r2, s):
            return None
        k = 6 + r2
        r3 = digits(c, k)
        return k + r3 if ok(r3) else None
    if 1 <= r1 <= 2 and s in (0x2D, 0x2F):
        j = r1 + 1
        if name3(c, j, MONTHS) and at(c, j + 3, s):
            return j + 8 if digits(c, j + 4) == 4 else None
        r2 = digits(c, j)
        if not 1 <= r2 <= 2 or not at(c, j + r2, s):
            return None
        k = j + r2 + 1
        r3 = digits(c, k)
        return k + r3 if r3 in (2, 4) else None
    if r1 == 2 and s == 0x2E and digits(c, 3) == 2 and at(c, 5, 0x2E) and digits(c, 6) == 4:
        return 10
    return None


def tod_end(c, i):
    h = digits(c, i)
    if not 1 <= h <= 2 or not at(c, i + h, 0x3A):
        return None
    j = i + h + 1
    if digits(c, j) != 2 or not at(c, j + 2, 0x3A) or digits(c, j + 3) != 2:
        return None
    return fraction_end(c, j + 5)


def dots_tod_end(c, i):
    if (digits(c, i) == 2 and at(c, i + 2, 0x2E) and digits(c, i + 3) == 2 and at(c, i + 5, 0x2E)
            and digits(c, i + 6) == 2):
        return fraction_end(c, i + 8)
    return None


def fraction_end(c, e):
    n = digits(c, e + 1)
    return e + 1 + n if at(c, e, 0x2E) and n > 0 else e


def zone_end(c, i):
    if i >= len(c):
        return i
    x = c[i]
    if x in (0x5A, 0x7A):
        return i + 1
    if x in (0x2B, 0x2D):
        dd = digits(c, i + 1)
        if dd == 4:
            return i + 5
        if dd == 2 and at(c, i + 3, 0x3A) and digits(c, i + 4) == 2:
            return i + 6
        if dd == 2:
            return i + 3
    return i


def name3(c, i, names):
    t = c[i:i + 3]
    return len(t) == 3 and all(ALPHA[x] for x in t) and t.lower() in names


def spaced_time(m, i):
    j = i
    if name3(m, j, WEEKDAYS) and at(m, j + 3, 0x20):
        j += 4
    if not (name3(m, j, MONTHS) and at(m, j + 3, 0x20)):
        return None
    j += 4
    if at(m, j, 0x20):
        j += 1
    d = digits(m, j)
    if not 1 <= d <= 2 or not at(m, j + d, 0x20):
        return None
    j += d + 1
    t = cand_end(m, j)
    c = m[j:t]
    if not c:
        return None
    e = tod_end(c, 0)
    if e is None or zone_end(c, e) != len(c):
        return None
    end = t
    if at(m, end, 0x20):
        y = cand_end(m, end + 1)
        if y == end + 5 and all(DIGIT[x] for x in m[end + 1:y]):
            end = y
    return end


def ipv4_end(c):
    i = 0
    for k in range(4):
        if k > 0:
            if not at(c, i, 0x2E):
                return None
            i += 1
        n = digits(c, i)
        if not 1 <= n <= 3:
            return None
        i += n
    return i


def is_ipv4_port(c):
    e = ipv4_end(c)
    if e is None or not at(c, e, 0x3A):
        return False
    n = digits(c, e + 1)
    return n > 0 and e + 1 + n == len(c)


def is_hex_ip(c):
    return all(HEXDIGIT[x] or x == 0x3A for x in c) and c.count(b":") >= 2 and any(HEXDIGIT[x] for x in c)


def is_url(c):
    if not c or not ALPHA[c[0]]:
        return False
    n = 0
    while n < len(c) and (ALNUM[c[n]] or c[n] in (0x2B, 0x2E, 0x2D)):
        n += 1
    return c[n:].startswith(b"://")


def is_path(c):
    return (c.startswith(b"/") or c.startswith(b"./") or c.startswith(b"../") or c.startswith(b"~/")
            or (len(c) >= 3 and ALPHA[c[0]] and c[1] == 0x3A and c[2] in (0x5C, 0x2F))
            or c.count(b"/") >= 2)


# ---------------------------------------------------------------------------------------
# Lines, counts and the dump


def split_lines(data):
    """Lines as Rust's split_inclusive gives them: each with its '\\n', the last one maybe without."""
    if not data:
        return []
    parts = data.split(b"\n")
    lines = [p + b"\n" for p in parts[:-1]]
    if parts[-1]:
        lines.append(parts[-1])
    return lines


def write_key(key):
    out = bytearray()
    for x in key:
        if x == 0x5C:
            out += b"\\\\"
        elif x < 0x20 or x == 0x7F:
            out += b"\\x%02x" % x
        else:
            out.append(x)
    return bytes(out)


def count(keys):
    c = {}
    for k in keys:
        c[k] = c.get(k, 0) + 1
    return c


def dump(counts):
    return b"".join(b"%d\t%s\n" % (n, write_key(k)) for k, n in counts.items())


def lines_text(keys):
    return b"".join(write_key(k) + b"\n" for k in keys)


# ---------------------------------------------------------------------------------------
# Comparing with the Rust keys


def rust_command(spec):
    if spec:
        return shlex.split(spec)
    exe = os.path.join(CRATE, "target", "release", "fewer")
    if not os.path.exists(exe):
        raise SystemExit("%s is not built: run `cargo build --release` in %s, or give --bin" % (exe, CRATE))
    return [exe, "keys"]


def default_samples():
    """The samples when no PATH is given: the folder FEWER_SAMPLES names (copies of public
    logs, kept out of the repository), else the crate's own made-up ones."""
    return [SAMPLES] if SAMPLES else [os.path.join(CRATE, "tests", "data")]


def run_rust(cmd, mode, path):
    r = subprocess.run(cmd + [mode, path], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if r.returncode != 0:
        raise SystemExit("%s %s %s failed: %s" % (" ".join(cmd), mode, path, r.stderr.decode("utf-8", "replace")))
    return r.stdout


def first_difference(a, b):
    al = a.split(b"\n")
    bl = b.split(b"\n")
    for i, (x, y) in enumerate(zip(al, bl)):
        if x != y:
            return "line %d: python %r, rust %r" % (i + 1, x[:200], y[:200])
    return "lengths differ: python %d lines, rust %d lines" % (len(al), len(bl))


def sample_files(paths):
    out = []
    for p in paths:
        if os.path.isdir(p):
            for root, dirs, files in os.walk(p):
                dirs.sort()
                for f in sorted(files):
                    out.append(os.path.join(root, f))
        else:
            out.append(p)
    return out


def pieces_equal(lines, keys, rng):
    """Cuts the file at random line starts into 2 to 12 pieces: the keys of the pieces, one
    after the other, and the counts summed over the pieces, equal the whole file's."""
    if len(lines) < 2:
        return True
    cuts = sorted(rng.sample(range(1, len(lines)), min(len(lines) - 1, rng.randint(1, 11))))
    got = []
    total = {}
    for a, b in zip([0] + cuts, cuts + [len(lines)]):
        piece = b"".join(lines[a:b])
        pk = [line_key(l) for l in split_lines(piece)]
        got.extend(pk)
        for k, n in count(pk).items():
            total[k] = total.get(k, 0) + n
    return got == keys and dump(total) == dump(count(keys))


def check(paths, cmd, seed):
    rng = random.Random(seed)
    files = sample_files(paths)
    bad = 0
    print("%-55s %7s %6s  %s" % ("file", "lines", "keys", "result"))
    for f in files:
        with open(f, "rb") as fh:
            data = fh.read()
        lines = split_lines(data)
        keys = [line_key(l) for l in lines]
        py_lines = lines_text(keys)
        py_dump = dump(count(keys))
        problems = []
        rs_lines = run_rust(cmd, "--lines", f)
        if rs_lines != py_lines:
            problems.append("line keys differ, " + first_difference(py_lines, rs_lines))
        rs_dump = run_rust(cmd, "--dump", f)
        if rs_dump != py_dump:
            problems.append("counts differ, " + first_difference(py_dump, rs_dump))
        if not pieces_equal(lines, keys, rng):
            problems.append("the keys of the pieces differ from the whole file's")
        bad += bool(problems)
        name = os.path.relpath(f, CRATE)
        print("%-55s %7d %6d  %s" % (name[-55:], len(lines), len(count(keys)), "; ".join(problems) or "same"))
    print("%d files, %d with differences" % (len(files), bad))
    return bad == 0


# ---------------------------------------------------------------------------------------
# Made-up lines

TOKENS = [
    b"ERROR", b"error:", b"WARN", b"INFO", b"Dec", b"Jun", b"jun", b"Sun", b"May", b"10", b"9", b"06:55:46", b"1:02:03",
    b"10:15", b"2026-11-18", b"2026-11-18T10:15:02.123Z", b"2026-11-18T10:15:02+01:00", b"18/Nov/2026:10:00:00",
    b"18-nov-2026", b"2005-06-03-15.42.50.675872", b"2005.06.03", b"17/06/09", b"1/5/2026", b"18.11.2026", b"081109",
    b"03-17", b"10.30", b"10.0.0.1", b"10.0.0.1:8080", b"10.0.0.1.", b"/10.0.0.1:5", b"fe80::1", b"::", b"::1", b"a::b",
    b"00:1a:2b:3c:4d:5e", b"0:0:0:0:0:0:0:0", b"https://example.com/a/b?c=1&d=2", b"jdbc:mysql://h/db", b"x://",
    b"/var/log/x", b"./a", b"../b", b"~/c", b"a/b/c", b"and/or", b"C:\\Windows\\x.dll", b"C:", b"C:/x", b"\\", b"\\\\",
    b"deadbeefcafe", b"DEADBEEFCAFEBABE", b"decade", b"0x1F", b"-1", b"blk_-123", b"x86_64", b"v1.2.3", b"HTTP/1.1",
    b"550e8400-e29b-41d4-a716-446655440000", b"550E8400-E29B-41D4-A716-44665544000Z", b'"quoted text"', b'"un\\"closed',
    b'"', b'\\"', b"'single'", "é".encode(), "日本".encode(), b"\xff", b"\xc3", BOM, b"\t", b"  ", b"\x1b[31m", b"\x1b[0m",
    b"\x1b]0;title\x07", b"\x1b(B", b"\r", b"\r\r", b"\x00", b"\x7f", b"\x0b", b"{", b"}", b"[", b"]", b":", b"=", b",",
    b"%", b"@", b"+", b"~", b"-", b".", b"_", b"2005", b"1131566461", b"99%", b"1.2.3", b"1.2.34", b"user@host", b"<num>",
    b"stdout", b"stderr", b"F", b"P", b"Z", b"T",
]
SEPS = [b" ", b"", b"\t", b"  ", b", ", b":", b"=", b"_", b" - "]


def soup(rng, n=None):
    n = rng.randint(0, 14) if n is None else n
    out = b""
    for _ in range(n):
        out += rng.choice(TOKENS) + rng.choice(SEPS)
    return out


def spaced_date(rng):
    month = rng.choice([b"Jan", b"FEB", b"mar", b"Dec", b"Jux", b"Mayo"])
    day = rng.choice([b"1", b"09", b"31", b"123", b""])
    gap = rng.choice([b" ", b"  ", b"   "])
    tod = rng.choice([b"06:55:46", b"6:55:46", b"06:55", b"06:55:46.123", b"06:55:46Z", b"06:55:46:", b"x"])
    out = rng.choice([b"", b"Sun ", b"mon ", b"Fry "]) + month + gap + day + b" " + tod
    return out + rng.choice([b"", b" 2005", b" 205", b" 20055", b" 2005]", b" UTC 2026", b" host"])


def json_string(rng, text):
    """text (bytes) as a JSON string, with random escapes; sometimes broken on purpose."""
    out = bytearray(b'"')
    for ch in text.decode("utf-8", "replace"):
        cp = ord(ch)
        r = rng.random()
        if ch == '"' or ch == "\\":
            out += b"\\" + ch.encode()
        elif cp < 0x20:
            out += rng.choice([b"\\u%04x" % cp, {9: b"\\t", 10: b"\\n", 13: b"\\r"}.get(cp, b"\\u%04x" % cp)])
        elif r < 0.05:
            out += b"\\u%04x" % cp if cp < 0x10000 else ch.encode("utf-8")
        elif r < 0.06:
            out += b"\\/"
        else:
            out += ch.encode("utf-8")
    r = rng.random()
    if r < 0.03:
        out += b"\\ud83d\\ude00"  # a surrogate pair: fine
    elif r < 0.05:
        out += b"\\ud800"  # a lone surrogate: not JSON
    elif r < 0.06:
        out += b"\t"  # a raw tab: not JSON
    elif r < 0.07:
        out += b"\\x"  # a bad escape: not JSON
    return bytes(out) + b'"'


def json_value(rng, depth=0):
    r = rng.random()
    if depth > 3 or r < 0.45:
        return json_string(rng, soup(rng))
    if r < 0.6:
        return rng.choice([b"0", b"-1", b"30", b"1.5e3", b"-0.0", b"1E+2", b"01", b"1.", b"-", b"1e", b"12345678901234567890"])
    if r < 0.7:
        return rng.choice([b"true", b"false", b"null", b"nul", b"True"])
    if r < 0.85:
        return json_object(rng, depth + 1)
    return b"[" + b",".join(json_value(rng, depth + 1) for _ in range(rng.randint(0, 3))) + b"]"


NAMES = MESSAGE_FIELDS + LEVEL_FIELDS + ["time", "ts", "host", "pid", "log", "stream", "sessionId", "type", "payload",
                                          "record_type", "timestamp", "content", "name", "role", "message", "x", "é"]


def json_object(rng, depth=0):
    fields = []
    for _ in range(rng.randint(0, 6)):
        name = rng.choice(NAMES)
        fields.append(json_string(rng, name.encode()) + rng.choice([b":", b" : "]) + json_value(rng, depth))
    body = b"{" + rng.choice([b",", b", "]).join(fields) + b"}"
    r = rng.random()
    if r < 0.03:
        body += b" x"  # more after the value
    elif r < 0.05:
        body = body[:-1]  # not closed
    elif r < 0.07:
        body = body.replace(b"}", b",}", 1)  # a trailing comma
    return body


def docker_line(rng):
    inner = rng.choice([soup(rng), json_object(rng), b"\x1b[33mWARN\x1b[0m " + soup(rng), b"10%\r50%\r" + soup(rng)])
    log = json_string(rng, inner + rng.choice([b"\n", b"\r\n", b""]))
    return b'{"log":' + log + b',"stream":"' + rng.choice([b"stdout", b"stderr"]) + b'","time":"2026-11-18T10:15:02.1Z"}'


def session_line(rng):
    if rng.random() < 0.5:
        blocks = b",".join(rng.choice([b'{"type":"text","text":"hi"}', b'{"type":"tool_use","name":"Bash","input":{}}',
                                       b'{"name":7}', b'{"type":"tool_result","content":"x"}']) for _ in range(rng.randint(0, 3)))
        content = rng.choice([b'"plain"', b"[" + blocks + b"]", b"null"])
        return (b'{"sessionId":"s1","type":' + rng.choice([b'"user"', b'"assistant"', b"3"]) + b',"message":{"role":"'
                + rng.choice([b"user", b"assistant"]) + b'","content":' + content + b"}}")
    payload = rng.choice([b'{"type":"function_call","name":"shell"}', b'{"type":"message","role":"user","content":[{"type":"input_text"}]}',
                          b"null", b'"text"'])
    return b'{"timestamp":"2026-11-18T10:15:02Z","type":"response_item","payload":' + payload + b"}"


def cri_prefix(rng):
    return rng.choice([b"2026-11-18T10:15:02.123456789Z", b"2026-11-18T10:15:02Z", b"2026-11-18T10:15:02+01:00",
                       b"2026-11-18T10:15:02.1234567890Z", b"2026-11-18T10:15:02+0100", b"2026-11-18 10:15:02Z"]) + \
        rng.choice([b" stdout ", b" stderr ", b" stdin "]) + rng.choice([b"F", b"P", b"X"]) + rng.choice([b" ", b"", b"x"])


def mutate(rng, line):
    line = bytearray(line)
    for _ in range(rng.randint(1, 3)):
        r = rng.random()
        pos = rng.randint(0, len(line))
        if r < 0.4:
            line[pos:pos] = rng.choice(TOKENS)
        elif r < 0.7 and line:
            del line[pos:pos + rng.randint(1, 8)]
        elif line:
            line[min(pos, len(line) - 1)] = rng.randrange(256)
    return bytes(line)


def made_up_line(rng, samples):
    r = rng.random()
    if r < 0.12:
        line = bytes(rng.randrange(256) for _ in range(rng.randint(0, 100)))
    elif r < 0.32:
        line = soup(rng)
    elif r < 0.42:
        line = soup(rng, 2) + spaced_date(rng) + soup(rng, 2)
    elif r < 0.57:
        line = json_object(rng)
    elif r < 0.65:
        line = docker_line(rng)
    elif r < 0.71:
        line = session_line(rng)
    elif r < 0.80:
        line = cri_prefix(rng) + rng.choice([soup(rng), json_object(rng)])
    elif samples:
        line = mutate(rng, rng.choice(samples).rstrip(b"\n"))
    else:
        line = soup(rng)
    return line.replace(b"\n", b"")


def fixed_lines():
    """Edge lines that random lines rarely reach: long messages and the JSON size limit."""
    out = [
        b"word " * 20000,
        b"g" * MESSAGE_MAX,
        b"g" * (MESSAGE_MAX + 1),
        b" " * (MESSAGE_MAX + 1),
        b'"' + b"x" * (MESSAGE_MAX + 10) + b'"',
        b"[" * 64 + b"]" * 64,
    ]
    for size in (JSON_MAX, JSON_MAX + 1):
        filler = size - len(b'{"msg":"","level":"x"}')
        out.append(b'{"msg":"' + b"y " * (filler // 2) + b"y" * (filler % 2) + b'","level":"x"}')
    for depth in (63, 64, 65):
        out.append(b'{"msg":"deep","a":' + b"[" * depth + b"]" * depth + b"}")
    return out


def fuzz(n, cmd, seed):
    rng = random.Random(seed)
    samples = []
    for f in sample_files(default_samples()):
        with open(f, "rb") as fh:
            samples.extend(split_lines(fh.read())[:300])
    lines = fixed_lines() + [made_up_line(rng, samples) for _ in range(n)]
    tmp = os.path.join(CRATE, "target", "msgkey-check")
    os.makedirs(tmp, exist_ok=True)
    path = os.path.join(tmp, "made_up_lines.txt")
    with open(path, "wb") as fh:
        fh.write(b"\n".join(lines) + b"\n")
    rust = run_rust(cmd, "--lines", path).split(b"\n")
    bad = 0
    for i, line in enumerate(lines):
        py = write_key(line_key(line + b"\n"))
        rs = rust[i] if i < len(rust) else None
        if py != rs:
            bad += 1
            if bad <= 10:
                print("line %d %r\n  python %r\n  rust   %r" % (i + 1, line[:300], py[:300], (rs or b"")[:300]))
    if len(rust) != len(lines) + 1:
        bad += 1
        print("rust gave %d keys for %d lines" % (len(rust) - 1, len(lines)))
    print("%d made-up lines (seed %d): %d differ" % (len(lines), seed, bad))
    if bad == 0:
        os.remove(path)
    return bad == 0


def main(argv):
    args = list(argv)

    def option(name, default=None):
        if name in args:
            i = args.index(name)
            v = args[i + 1]
            del args[i:i + 2]
            return v
        return default

    if len(args) == 2 and args[0] in ("--dump", "--lines"):
        with open(args[1], "rb") as fh:
            keys = [line_key(l) for l in split_lines(fh.read())]
        out = dump(count(keys)) if args[0] == "--dump" else lines_text(keys)
        sys.stdout.buffer.write(out)
        return 0
    bin_spec = option("--bin")
    seed = int(option("--seed", "1"))
    work = option("--work")
    if args and args[0] in ("--check", "--fuzz"):
        # the Rust command gets a made-up HOME and agents' folders (safe_env.py)
        import importlib.util
        spec = importlib.util.spec_from_file_location("safe_env", os.path.join(CRATE, "checks", "safe_env.py"))
        safe_env = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(safe_env)
        safe_env.setup(work, "msgkey_ref")
    if args and args[0] == "--check":
        paths = args[1:] or default_samples()
        return 0 if check(paths, rust_command(bin_spec), seed) else 1
    if args and args[0] == "--fuzz" and len(args) == 2:
        return 0 if fuzz(int(args[1]), rust_command(bin_spec), seed) else 1
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
