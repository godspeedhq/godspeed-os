#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""`tomllib` where Python has it, and a parser for the TOML THIS REPOSITORY WRITES where it does not.

WHY THIS EXISTS. `README.md` declares Python 3.8 as the floor, and `scripts/commandments.py` - which
every build runs - read `COMMANDMENTS.baseline.toml` and every service contract with `tomllib`, which
arrived in 3.11. So on 3.8, 3.9 and 3.10 the first checker of every build failed at import, and the
declared floor was false while `python_floor_check.py` said it held (backlog/80 T1: it looked at
syntax, never at what a script imports).

WHAT IT PARSES. Not TOML: the part of it these files use, which is small and stable - comments,
`[table]` and `[dotted.table]` headers, `[[array.of.tables]]`, bare and quoted keys, basic and literal
strings (with the common escapes), integers, booleans, and arrays and inline tables, nested and over
several lines. Anything else raises `TOMLDecodeError` with a line number, rather than guessing.

WHY IT CANNOT DRIFT UNSEEN. A hand parser is right on the day it is written. So wherever `tomllib`
exists, `commandments.py --selftest` parses every tracked `.toml` with BOTH and fails if any differs
(`self_check`). A new construct in a contract fails that self-test on the machine that has 3.11+ -
every developer machine today - before anyone on an older Python could be misled by it.
"""
from __future__ import annotations

import re

try:  # Python 3.11+
    import tomllib as _tomllib
    TOMLDecodeError = _tomllib.TOMLDecodeError
except ImportError:  # 3.8 - 3.10
    _tomllib = None

    class TOMLDecodeError(ValueError):
        """Raised for TOML this fallback does not understand."""


_BARE_KEY = re.compile(r"[A-Za-z0-9_-]+")
_INT = re.compile(r"[+-]?(?:0x[0-9A-Fa-f_]+|0o[0-7_]+|0b[01_]+|[0-9][0-9_]*)")
_ESCAPES = {"b": "\b", "t": "\t", "n": "\n", "f": "\f", "r": "\r", '"': '"', "\\": "\\"}


class _Parser:
    def __init__(self, text):
        self.s = text.replace("\r\n", "\n")
        self.i = 0

    # ---- low level -------------------------------------------------------------------------------
    def line(self):
        return self.s.count("\n", 0, self.i) + 1

    def fail(self, what):
        raise TOMLDecodeError("%s (line %d)" % (what, self.line()))

    def peek(self, n=1):
        return self.s[self.i:self.i + n]

    def skip_ws(self, newlines=False):
        while self.i < len(self.s):
            c = self.s[self.i]
            if c in " \t" or (newlines and c == "\n"):
                self.i += 1
            elif c == "#":
                while self.i < len(self.s) and self.s[self.i] != "\n":
                    self.i += 1
            else:
                break

    def expect_eol(self):
        self.skip_ws()
        if self.i < len(self.s) and self.s[self.i] != "\n":
            self.fail("unexpected text after a value")

    # ---- keys --------------------------------------------------------------------------------------
    def key_part(self):
        c = self.peek()
        if c == '"':
            return self.basic_string()
        if c == "'":
            return self.literal_string()
        m = _BARE_KEY.match(self.s, self.i)
        if not m:
            self.fail("expected a key")
        self.i = m.end()
        return m.group(0)

    def dotted_key(self):
        parts = [self.key_part()]
        self.skip_ws()
        while self.peek() == ".":
            self.i += 1
            self.skip_ws()
            parts.append(self.key_part())
            self.skip_ws()
        return parts

    # ---- values ------------------------------------------------------------------------------------
    def basic_string(self):
        if self.peek(3) == '"""':
            self.fail("multi-line strings are not in the subset this fallback reads")
        self.i += 1
        out = []
        while True:
            if self.i >= len(self.s):
                self.fail("unterminated string")
            c = self.s[self.i]
            if c == '"':
                self.i += 1
                return "".join(out)
            if c == "\n":
                self.fail("newline in a string")
            if c == "\\":
                e = self.s[self.i + 1:self.i + 2]
                if e in _ESCAPES:
                    out.append(_ESCAPES[e])
                    self.i += 2
                elif e in ("u", "U"):
                    n = 4 if e == "u" else 8
                    hexd = self.s[self.i + 2:self.i + 2 + n]
                    if len(hexd) != n or not re.fullmatch(r"[0-9A-Fa-f]+", hexd):
                        self.fail("bad unicode escape")
                    out.append(chr(int(hexd, 16)))
                    self.i += 2 + n
                else:
                    self.fail("unknown escape \\%s" % e)
                continue
            out.append(c)
            self.i += 1

    def literal_string(self):
        if self.peek(3) == "'''":
            self.fail("multi-line strings are not in the subset this fallback reads")
        j = self.s.find("'", self.i + 1)
        if j < 0 or "\n" in self.s[self.i + 1:j]:
            self.fail("unterminated literal string")
        v = self.s[self.i + 1:j]
        self.i = j + 1
        return v

    def value(self):
        c = self.peek()
        if c == '"':
            return self.basic_string()
        if c == "'":
            return self.literal_string()
        if c == "[":
            return self.array()
        if c == "{":
            return self.inline_table()
        if self.s.startswith("true", self.i):
            self.i += 4
            return True
        if self.s.startswith("false", self.i):
            self.i += 5
            return False
        m = _INT.match(self.s, self.i)
        if m:
            after = self.s[m.end():m.end() + 1]
            if after in (".", "e", "E", ":", "-") and after != "":
                self.fail("floats, dates and times are not in the subset this fallback reads")
            self.i = m.end()
            return int(m.group(0).replace("_", ""), 0)
        self.fail("expected a value")

    def array(self):
        self.i += 1
        out = []
        while True:
            self.skip_ws(newlines=True)
            if self.peek() == "]":
                self.i += 1
                return out
            out.append(self.value())
            self.skip_ws(newlines=True)
            if self.peek() == ",":
                self.i += 1
            elif self.peek() == "]":
                self.i += 1
                return out
            else:
                self.fail("expected `,` or `]` in an array")

    def inline_table(self):
        self.i += 1
        out = {}
        self.skip_ws()
        if self.peek() == "}":
            self.i += 1
            return out
        while True:
            self.skip_ws()
            keys = self.dotted_key()
            if self.peek() != "=":
                self.fail("expected `=` in an inline table")
            self.i += 1
            self.skip_ws()
            self.put(out, keys, self.value())
            self.skip_ws()
            if self.peek() == ",":
                self.i += 1
            elif self.peek() == "}":
                self.i += 1
                return out
            else:
                self.fail("expected `,` or `}` in an inline table")

    # ---- tables ------------------------------------------------------------------------------------
    def put(self, table, keys, val):
        for k in keys[:-1]:
            table = table.setdefault(k, {})
            if not isinstance(table, dict):
                self.fail("key `%s` is not a table" % k)
        if keys[-1] in table:
            self.fail("duplicate key `%s`" % keys[-1])
        table[keys[-1]] = val

    def walk(self, root, keys):
        t = root
        for k in keys:
            nxt = t.setdefault(k, {})
            if isinstance(nxt, list):
                nxt = nxt[-1]
            if not isinstance(nxt, dict):
                self.fail("key `%s` is not a table" % k)
            t = nxt
        return t

    def document(self):
        root = {}
        cur = root
        while True:
            self.skip_ws(newlines=True)
            if self.i >= len(self.s):
                return root
            if self.peek(2) == "[[":
                self.i += 2
                self.skip_ws()
                keys = self.dotted_key()
                if self.peek(2) != "]]":
                    self.fail("expected `]]`")
                self.i += 2
                parent = self.walk(root, keys[:-1])
                arr = parent.setdefault(keys[-1], [])
                if not isinstance(arr, list):
                    self.fail("`%s` is not an array of tables" % keys[-1])
                cur = {}
                arr.append(cur)
            elif self.peek() == "[":
                self.i += 1
                self.skip_ws()
                keys = self.dotted_key()
                if self.peek() != "]":
                    self.fail("expected `]`")
                self.i += 1
                cur = self.walk(root, keys)
            else:
                keys = self.dotted_key()
                if self.peek() != "=":
                    self.fail("expected `=`")
                self.i += 1
                self.skip_ws()
                self.put(cur, keys, self.value())
            self.expect_eol()


def fallback_loads(text):
    """The fallback parser on its own - used by `self_check` to compare it with `tomllib`."""
    return _Parser(text).document()


def loads(text):
    return _tomllib.loads(text) if _tomllib else fallback_loads(text)


def load(fh):
    """Like `tomllib.load`: `fh` is a file opened in BINARY mode."""
    return loads(fh.read().decode("utf-8"))


def self_check(paths):
    """Parse each path with both `tomllib` and the fallback; return the ones that differ.

    Returns None when `tomllib` is absent (nothing to compare against - this IS the fallback then).
    A file `tomllib` itself refuses is skipped: then there is no right answer to agree with.
    """
    if _tomllib is None:
        return None
    bad = []
    for p in paths:
        with open(p, "rb") as f:
            text = f.read().decode("utf-8")
        try:
            want = _tomllib.loads(text)
        except _tomllib.TOMLDecodeError:
            continue
        try:
            got = fallback_loads(text)
        except TOMLDecodeError as e:
            bad.append((p, "the fallback refused it: %s" % e))
            continue
        if got != want:
            bad.append((p, "the fallback reads it differently from tomllib"))
    return bad
