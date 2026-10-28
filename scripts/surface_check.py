#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""THE COVERED SURFACE HOLDS: what `docs/compatibility.md` promises not to break, checked against a snapshot.

`backlog/71`, step 3: "a checked-in snapshot of the public `gs` surface and the utility vocabulary, and a
checker that fails the build when something is REMOVED or CHANGED rather than added. A breaking change then
needs a recorded reason, the way an amendment does." This is that checker, and
`scripts/SURFACE.baseline.txt` is that snapshot.

WHAT IT READS, each from the source that defines it rather than from a list kept beside it:

  gs       every public item of the standard library (`stdlib/rust/src`, `gs::driver` included): functions
           with their signatures, types with their public fields and variants, traits with their items,
           inherent methods, trait implementations, constants and re-exports. Walked from `lib.rs` through
           the `pub mod`s, as a user of the crate reaches them.
  util     the utility names (`UTILS` in the shell) and the words each takes first (`SUBCMD_FIRST`).
  record   the record columns each producer emits: every `Table::new(&[..])` with literal columns, in the
           shell, the standard library and the record services, keyed by the function that builds it.
  contract every key the contract schema accepts, its type, its allowed values, and which keys are required.

THE RULE, per line of the snapshot:
  - every line in the snapshot must still be produced. A line that is not is a REMOVAL or a CHANGE (a
    changed signature is the old line gone and a new one present), and fails;
  - a line produced that the snapshot lacks is GROWTH, which the promise allows. It still fails until it is
    recorded with `--bless`, because the snapshot IS the promise: an item nobody recorded could change
    unnoticed the day after it was added;
  - `contract required` runs the other way: a NEW required key is the break (a contract that validated
    yesterday stops validating), and dropping one is growth.

THE TWO COMMANDS:
  --bless                    record growth. Refuses while anything is removed or changed, so it cannot be
                             used to accept a break by accident.
  --accept-break "<reason>"  record a removal or change, with its reason, in `scripts/SURFACE-BREAKS.txt`,
                             dated. Before v1 that is how the surface moves (it is v0.x and still allowed
                             to); after v1 the policy in `docs/compatibility.md` says when a break is
                             allowed at all. Either way it is never silent (CLAUDE.md 21).

WHAT THIS DOES NOT SEE, recorded rather than implied (26.7):
  - MEANING. A function that keeps its signature and changes what it does passes. No text check can see that;
    the identity, shell and selfcheck suites are what hold behaviour.
  - Items gs RE-EXPORTS from the SDK (`ServiceContext`, `Message`): the `pub use` line is held, the methods
    of the re-exported type are not, because they are defined in the SDK, which is not covered.
  - The gsh LANGUAGE has no table to read: its keywords are tests in the interpreter. It is held by the
    selfcheck corpus and `osdev test shell`, which run scripts. GSFS0008 is held by `osdev test fs-compat`.
  - The text is parsed, not compiled. It understands the forms this crate uses; an item in a form it does not
    understand is reported as UNREADABLE rather than skipped, so a gap fails loudly.
"""
import datetime
import io
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "SURFACE.baseline.txt")
BREAKS = os.path.join(ROOT, "scripts", "SURFACE-BREAKS.txt")
GS_ROOT = os.path.join(ROOT, "stdlib", "rust", "src", "lib.rs")
SDK_SRC = os.path.join(ROOT, "sdk", "rust", "src")
# Re-exported by gs and held by NAME only. `ServiceContext` is the handle every program is given, and its
# methods are the raw system calls - the SDK, which is not covered; `gs` is the way to use them.
SDK_HANDLES = ("ServiceContext",)
SHELL = os.path.join(ROOT, "services", "shell", "src", "main.rs")
SCHEMA = os.path.join(ROOT, "contracts", "schema", "service.schema.json")
# Where a record producer can live: the shell, the standard library, and a service that sends records.
RECORD_TOPS = ("services", "stdlib", "examples")
# A record's columns, written literally: a `Table`, or `godspeed`'s answer table (`Rows`, `.columns`).
RECORD_SITE = re.compile(r"(?:Table::new|Rows::new|\.columns)\(&\[([^\]]*)\]\)")
# Schema keywords that NARROW what validates: a new one, or a changed value, can refuse a contract that passed.
NARROWING = ("pattern", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "minLength", "maxLength",
             "minItems", "maxItems", "uniqueItems", "const", "format", "additionalProperties")
MIN_REASON = 20


# ---- reading Rust ----------------------------------------------------------------------------------------

# Every source file is read through `read` (so `--selftest` can hand the checker a mutant without touching
# the tree) and stripped once per distinct text (so the mutants cost only the file each one changes).
OVERRIDE = {}
_STRIPPED = {}


def key(path):
    return os.path.normcase(os.path.abspath(path))


_READ = {}


def read(path):
    k = key(path)
    if k in OVERRIDE:
        return OVERRIDE[k]
    if k not in _READ:
        with io.open(path, encoding="utf-8") as fh:
            _READ[k] = fh.read()
    return _READ[k]


def stripped(path):
    text = read(path)
    if text not in _STRIPPED:
        _STRIPPED[text] = strip_comments(text)
    return _STRIPPED[text]


def strip_comments(src):
    """The text with comments blanked, strings and char literals kept. Lifetimes (`'a`) are not chars.

    One regex, so a 27,000-line file costs a scan rather than a Python loop per character. At any position the
    alternatives are tried in order, and the scan moves left to right, so `"//"` inside a string is a string
    and `'"'` is a char. Block comments are taken as not nested, which is all this tree writes."""
    return TOKENS.sub(lambda m: " " if m.group(0)[:2] in ("//", "/*") else m.group(0), src)


TOKENS = re.compile(
    r"""(?<!\w)b?r(#*)".*?"\1"""                                    # raw string
    r"""|(?<!\w)b?"(?:\\.|[^"\\])*\""""                             # string
    r"""|(?<!\w)b?'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^'\\])'"""  # char, not a lifetime
    r"""|//[^\n]*"""                                                 # line comment
    r"""|/\*.*?\*/""",                                               # block comment
    re.S)
CHAR_LIT = re.compile(r"'(\\(x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^'\\])'")


def matching(text, i):
    """Index just past the bracket that closes the one at `text[i]`, skipping string and char literals
    (`b'{'` in a body must not open a brace; a lifetime `'a` is not a char literal and is not skipped)."""
    pairs = {"{": "}", "(": ")", "[": "]"}
    stack, j, n = [pairs[text[i]]], i + 1, len(text)
    while j < n and stack:
        c = text[j]
        if c == "'":
            m = CHAR_LIT.match(text, j)
            if m:
                j = m.end()
                continue
        if c == '"':
            j += 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
        elif c in pairs:
            stack.append(pairs[c])
        elif c == stack[-1]:
            stack.pop()
        j += 1
    return j


def norm(s):
    """One spelling for one signature: whitespace collapsed, no space beside punctuation, no trailing comma."""
    s = re.sub(r"\s+", " ", s).strip()
    s = re.sub(r"\s*([()\[\]<>,;:&=*{}])\s*", r"\1", s)
    s = re.sub(r",([)\]>}])", r"\1", s).rstrip(",")
    # `->` and `=>` lose their spaces with the rest; `impl Trait`, `dyn Trait`, `mut x` keep theirs.
    return s


def split_top(s, sep=","):
    """`s` split on `sep` outside (), [], {} and <>."""
    parts, depth, cur, prev = [], 0, [], ""
    for c in s:
        if c in "([{<":
            depth += 1
        elif c in ")]}" or (c == ">" and prev != "-" and prev != "="):
            depth -= 1
        if c == sep and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(c)
        prev = c
    if "".join(cur).strip():
        parts.append("".join(cur))
    return parts


def param_types(params):
    """A parameter list with the NAMES dropped: renaming a parameter breaks no caller, so it is not a change."""
    out = []
    for p in split_top(params):
        p = p.strip()
        if re.fullmatch(r"&?('\w+\s+)?(mut\s+)?self", p) or ":" not in p.replace("::", ""):
            out.append(norm(p))
            continue
        k = 0
        while True:                                       # the first `:` that is not half of `::`
            k = p.index(":", k)
            if p[k:k + 2] == "::":
                k += 2
                continue
            if k > 0 and p[k - 1] == ":":
                k += 1
                continue
            break
        pat, ty = p[:k].strip(), p[k + 1:].strip()
        # A `self: Type` receiver keeps its name; every other pattern is the caller's business.
        out.append(norm(("self: " if pat == "self" else "_: ") + ty))
    return ",".join(out)


def fn_signature(head):
    """`pub const unsafe fn name<G>(params) -> R where ...` with the parameter names dropped."""
    m = re.search(r"\bfn\s+(\w+)", head)
    lp = head.index("(", m.end())
    rp = matching(head, lp)
    params = head[lp + 1:rp - 1]
    pre = head[:lp]
    post = head[rp:]
    return norm(pre) + "(" + param_types(params) + ")" + norm(post)


ATTR = re.compile(r"#!?\[")
VIS = re.compile(r"^pub(\s*\((crate|super|self|in [^)]*)\))?\s+")


class Reader:
    """Walks one crate's modules and collects its public surface as lines."""

    def __init__(self):
        self.lines = set()
        self.unreadable = []
        self.pub_types = set()
        self.impls = []          # (module, self type name, line) - kept only if the type is public
        self.children = []       # (file, module) of each `pub mod x;`, walked after the file that names it

    def add(self, mod, what):
        self.lines.add(f"gs {mod} {what}")

    def walk_file(self, path, mod):
        """One file's items, remembered per (file, module, text): a selftest mutant re-reads only its own file.
        The file's `mod x;` children are walked after it, each through here, so a cached parent still sees a
        changed child."""
        text = stripped(path)
        k = (key(path), mod, text)
        if k not in _WALKED:
            r = Reader()
            r.walk(text, mod, os.path.dirname(path), path)
            _WALKED[k] = (r.lines, r.unreadable, r.pub_types, r.impls, r.children)
        lines, unreadable, pub_types, impls, children = _WALKED[k]
        self.lines |= lines
        self.unreadable += unreadable
        self.pub_types |= pub_types
        self.impls += impls
        for cand, sub in children:
            self.walk_file(cand, sub)

    def walk(self, text, mod, here, path, in_dir=None):
        i, n = 0, len(text)
        while i < n:
            while i < n and text[i].isspace():
                i += 1
            if i >= n:
                break
            attrs = []
            while ATTR.match(text, i):
                close = matching(text, text.index("[", i))
                attrs.append(re.sub(r"\s+", " ", text[i:close]))
                i = close
                while i < n and text[i].isspace():
                    i += 1
            if i >= n:
                break
            # The item's head: up to the `{` or `;` that ends it, outside () and [].
            j, depth = i, 0
            while j < n:
                c = text[j]
                if c in "([":
                    j = matching(text, j)
                    continue
                if c == '"':
                    j = matching_string(text, j)
                    continue
                if c in "{;":
                    break
                j += 1
            head = text[i:j].strip()
            body = None
            if j < n and text[j] == "{" and re.match(r"(pub\S*\s+)?use\b", head):
                # `use a::{b, c};` - the braces are part of the path, not a body.
                k = text.find(";", j)
                k = n if k < 0 else k
                head, j = (head + text[j:k]).strip(), k
            if j < n and text[j] == "{":
                end = matching(text, j)
                body = text[j + 1:end - 1]
                j = end
                # `const X: T = { .. };` - the block is the value; the item ends at the `;`.
                if re.match(r"(pub\S*\s+)?(const|static)\s+(mut\s+)?\w+\s*:", head):
                    k = text.find(";", j)
                    j = n if k < 0 else k + 1
            else:
                j += 1
            i = j
            self.item(head, body, attrs, mod, here, path)

    def item(self, head, body, attrs, mod, here, path):
        if any(re.match(r"#\[cfg\((test\)|all\(.*\btest\b)", a) and "not(test)" not in a for a in attrs):
            return                                              # test-only: not part of the crate
        cfg = " ".join(a for a in attrs if a.startswith("#[cfg") and "not(test)" not in a)
        derives = sorted(set(d.strip() for a in attrs for m in [re.match(r"#\[derive\((.*)\)\]", a)] if m
                             for d in m.group(1).split(",") if d.strip()))
        public = bool(VIS.match(head)) and not re.match(r"pub\s*\(", head)
        rest = VIS.sub("", head)
        tag = (cfg + " ") if cfg else ""

        if head.startswith("macro_rules!"):
            if any("macro_export" in a for a in attrs):
                self.add("crate", tag + "macro " + head.split("!")[1].strip() + "!")
            return
        if not head:
            return
        kind = re.match(r"(?:(?:const|unsafe|async|extern\s*\"\w+\")\s+)*(mod|fn|struct|enum|union|trait|type|const|static|use|impl)\b", rest)
        if head.startswith("impl") or (kind and kind.group(1) == "impl"):
            self.impl(head, body, mod, tag)
            return
        if not kind:
            if public:
                self.unreadable.append(f"{os.path.relpath(path, ROOT)}: {head[:80]}")
            return
        k = kind.group(1)
        if not public:
            # A private module is not reachable by a user, except through a glob re-export from it
            # (`pub use granted::*`); its public items are kept aside for that case (`finish`).
            if k == "mod" and body is not None:
                name = re.match(r"mod\s+(\w+)", rest).group(1)
                self.walk(body, f"{mod}::({name})", here, path)
            return
        if k == "mod":
            name = re.match(r"mod\s+(\w+)", rest).group(1)
            sub = name if mod == "crate" else f"{mod}::{name}"
            self.add(mod, tag + f"mod {name}")
            if body is not None:
                self.walk(body, sub, here, path)
            else:
                stem = os.path.splitext(os.path.basename(path))[0]
                base = here if stem in ("lib", "mod") else os.path.join(here, stem)
                for cand in (os.path.join(base, name + ".rs"), os.path.join(base, name, "mod.rs")):
                    if os.path.isfile(cand):
                        self.children.append((cand, sub))
                        break
                else:
                    self.unreadable.append(f"{os.path.relpath(path, ROOT)}: no file for `mod {name}`")
        elif k == "fn":
            self.add(mod, tag + fn_signature(rest))
        elif k == "use":
            for one in expand_use(re.sub(r"^use\s+", "", rest).strip()):
                self.add(mod, tag + "use " + one)
        elif k in ("struct", "union"):
            name = re.match(r"(?:struct|union)\s+(\w+)", rest).group(1)
            self.pub_types.add(name)
            gens = norm(rest[rest.index(name) + len(name):])
            tuple_fields = None
            if "(" in gens and not gens.startswith("<") or gens.startswith("("):
                lp = rest.index("(")
                tuple_fields = rest[lp + 1:matching(rest, lp) - 1]
                gens = norm(rest[rest.index(name) + len(name):lp])
            line = f"{k} {name}{gens}"
            if derives:
                line += " derive(" + ",".join(derives) + ")"
            self.add(mod, tag + line)
            if tuple_fields is not None:
                for idx, f in enumerate(split_top(tuple_fields)):
                    f = strip_attrs(f).strip()
                    if VIS.match(f) and not re.match(r"pub\s*\(", f):
                        self.add(mod, tag + f"field {name}.{idx}: {norm(VIS.sub('', f))}")
            if body is not None:
                for f in split_top(body):
                    f = strip_attrs(f).strip()
                    if VIS.match(f) and not re.match(r"pub\s*\(", f):
                        self.add(mod, tag + f"field {name}.{norm(VIS.sub('', f))}")
        elif k == "enum":
            name = re.match(r"enum\s+(\w+)", rest).group(1)
            self.pub_types.add(name)
            gens = norm(rest[rest.index(name) + len(name):])
            line = f"enum {name}{gens}"
            if derives:
                line += " derive(" + ",".join(derives) + ")"
            if any("non_exhaustive" in a for a in attrs):
                line += " non_exhaustive"
            self.add(mod, tag + line)
            for v in split_top(body or ""):
                v = strip_attrs(v).strip()
                if v:
                    self.add(mod, tag + f"variant {name}::{norm(v)}")
        elif k == "trait":
            name = re.match(r"(?:unsafe\s+)?trait\s+(\w+)", rest).group(1)
            self.pub_types.add(name)
            self.add(mod, tag + norm(rest))
            for it in items_of(body or ""):
                self.add(mod, tag + f"trait-item {name}::" + item_line(it))
        elif k == "type":
            self.pub_types.add(re.match(r"type\s+(\w+)", rest).group(1))
            self.add(mod, tag + norm(rest))
        elif k in ("const", "static"):
            # The VALUE is not the promise; the name and the type are.
            m = re.match(r"(const|static)\s+(mut\s+)?(\w+)\s*:\s*(.*?)\s*=", rest, re.S)
            if not m:
                self.unreadable.append(f"{os.path.relpath(path, ROOT)}: {head[:80]}")
                return
            self.add(mod, tag + f"{m.group(1)} {m.group(3)}: {norm(m.group(4))}")

    def impl(self, head, body, mod, tag):
        h = norm(head)
        m = re.match(r"(?:unsafe )?impl(<.*?>)?\s*(.*)", re.sub(r"\s+", " ", head).strip())
        target = m.group(2)
        target = re.split(r"\bwhere\b", target)[0].strip()
        if re.search(r"\bfor\b", target):
            self_ty = re.split(r"\bfor\b", target, maxsplit=1)[1].strip()
            self.impls.append((mod, type_name(self_ty), tag + "impl " + h))
            return
        self_ty = type_name(target)
        for it in items_of(body or ""):
            if VIS.match(it) and not re.match(r"pub\s*\(", it):
                self.impls.append((mod, self_ty, tag + f"method {self_ty}::" + item_line(VIS.sub("", it))))

    def finish(self):
        for mod, ty, line in self.impls:
            if ty in self.pub_types:
                self.add(mod, line)
        self.follow_sdk()
        # `pub use name::*` of a private module: its public items are this module's, under this module.
        hidden = [l for l in self.lines if re.match(r"gs \S*::\(\w+\) ", l)]
        for l in hidden:
            self.lines.discard(l)
        for l in list(self.lines):
            m = re.match(r"gs (\S+) (.*)use (\w+)::\*$", l)
            if not m:
                continue
            inner = f"{m.group(1)}::({m.group(3)})"
            for h in hidden:
                if h.startswith(f"gs {inner} "):
                    self.lines.add(f"gs {m.group(1)} {m.group(2)}" + h[len(f"gs {inner} "):])


    def follow_sdk(self):
        """An SDK item `gs` re-exports is `gs`'s: its fields, variants and methods are held where gs exports it.

        `pub use godspeed_sdk::record::Table` makes `gs::record::Table` part of the standard library, so what
        a user can do with it is part of the promise, wherever the code happens to live. Two are held by NAME
        only (`SDK_HANDLES`), and a re-exported MODULE (`as wire`) by its name too.
        """
        sdk = {}
        for l in sorted(self.lines):
            m = re.match(r"gs (\S+) (.*)use godspeed_sdk::(\w+)::(\w+)$", l)
            if not m or m.group(4) in SDK_HANDLES:
                continue
            gsmod, tag, sdkmod, name = m.groups()
            if sdkmod not in sdk:
                r = Reader()
                path = os.path.join(SDK_SRC, sdkmod + ".rs")
                if not os.path.isfile(path):
                    self.unreadable.append(f"{l}: no {os.path.relpath(path, ROOT)}")
                    continue
                r.walk_file(path, "sdk")
                for _, ty, line in r.impls:
                    if ty in r.pub_types:
                        r.add("sdk", line)
                sdk[sdkmod] = r.lines
            mine = re.compile(r"gs sdk (#\[cfg[^\]]*\] )*(?:"
                              r"(struct|enum|union|trait|type|const|static) " + name + r"\b"
                              r"|fn " + name + r"\b|(field|variant|method|trait-item) " + name + r"(::|\.)"
                              r"|impl .*\bfor " + name + r"\b)")
            found = [x for x in sdk[sdkmod] if mine.match(x)]
            if not found:
                self.unreadable.append(f"{l}: `{name}` not found in sdk/rust/src/{sdkmod}.rs")
            for x in found:
                self.add(gsmod, tag + x[len("gs sdk "):])


def matching_string(text, j):
    k = j + 1
    while k < len(text) and text[k] != '"':
        k += 2 if text[k] == "\\" else 1
    return k + 1


def strip_attrs(s):
    s = s.strip()
    while ATTR.match(s):
        s = s[matching(s, s.index("[")):].strip()
    return s


def type_name(t):
    t = t.strip().lstrip("&").strip()
    t = re.sub(r"^(mut|dyn)\s+", "", t)
    return re.match(r"[\w:]*?(\w+)\s*(<|$|\s)", t + " ").group(1)


def items_of(body):
    """The items of an impl or trait body: each fn as its head, each const/type as its line."""
    out, i, n = [], 0, len(body)
    while i < n:
        while i < n and body[i].isspace():
            i += 1
        if i >= n:
            break
        attrs_start = i
        while ATTR.match(body, i):
            i = matching(body, body.index("[", i))
            while i < n and body[i].isspace():
                i += 1
        attrs = body[attrs_start:i]
        j = i
        while j < n and body[j] not in "{;":
            if body[j] in "([":
                j = matching(body, j)
                continue
            j += 1
        head = body[i:j].strip()
        if j < n and body[j] == "{":
            j = matching(body, j)
        else:
            j += 1
        i = j
        if re.search(r"#\[cfg\(test\)\]", attrs):
            continue
        if head:
            out.append(head)
    return out


def item_line(it):
    it = it.strip()
    if re.search(r"\bfn\s+\w+", it):
        return fn_signature(it)
    return norm(re.sub(r"\s*=.*$", "", it, flags=re.S))


def expand_use(tree, prefix=""):
    """`a::{b, c::{d, e as f}}` -> `a::b`, `a::c::d`, `a::c::e as f`."""
    tree = tree.strip()
    k = tree.find("{")
    if k < 0:
        return [norm(prefix + tree).replace(" as ", " as ")] if tree else []
    head, inner = tree[:k], tree[k + 1:tree.rindex("}")]
    out = []
    for part in split_top(inner):
        out.extend(expand_use(part, prefix + head))
    return out


def gs_surface():
    r = Reader()
    r.walk_file(GS_ROOT, "crate")
    r.finish()
    return r.lines, r.unreadable


# ---- the utilities and their records ---------------------------------------------------------------------

def const_list(src, name):
    m = re.search(r"const " + name + r"\s*:[^=]*=\s*&\[", src)
    if not m:
        return None
    lb = src.index("[", m.end() - 1)
    return src[lb + 1:matching(src, lb) - 1]


def util_surface():
    src = stripped(SHELL)
    if src not in _UTILS:
        _UTILS[src] = util_lines(src)
    return _UTILS[src]


_UTILS = {}


def util_lines(src):
    lines, problems = set(), []
    utils = const_list(src, "UTILS")
    if utils is None:
        problems.append("services/shell/src/main.rs: no `UTILS` list")
    else:
        for name in re.findall(r'"([^"]+)"', utils):
            lines.add(f"util name {name}")
    sub = const_list(src, "SUBCMD_FIRST")
    if sub is None:
        problems.append("services/shell/src/main.rs: no `SUBCMD_FIRST` table")
    else:
        for m in re.finditer(r'\(\s*"([^"]+)"\s*,\s*(&\[[^\]]*\]|[A-Za-z_:]+)\s*\)', sub):
            words = re.findall(r'"([^"]+)"', m.group(2)) if m.group(2).startswith("&") else None
            if words is None:
                # A table entry that names another list: read that list where it is defined.
                ref = const_list(src, m.group(2).split("::")[-1])
                words = re.findall(r'"([^"]+)"', ref) if ref is not None else []
                if ref is None:
                    problems.append(f"services/shell/src/main.rs: SUBCMD_FIRST `{m.group(1)}` names "
                                    f"`{m.group(2)}`, which is not a list here")
            for w in words:
                lines.add(f"util sub {m.group(1)} {w}")
    return lines, problems


_WALKED = {}
_RECORDS = {}
_FILES = []


def rust_files():
    if not _FILES:
        for top in RECORD_TOPS:
            for dirpath, dirs, files in os.walk(os.path.join(ROOT, top)):
                dirs[:] = sorted(d for d in dirs if d not in ("target", "tests"))
                _FILES.extend(os.path.join(dirpath, f) for f in sorted(files) if f.endswith(".rs"))
    return _FILES


def file_records(path):
    """The record columns one file builds, remembered per text."""
    raw = read(path)
    if raw in _RECORDS:
        return _RECORDS[raw]
    lines = set()
    if RECORD_SITE.search(raw):
        src = stripped(path)
        rel = os.path.relpath(path, ROOT).replace(os.sep, "/")
        fns = [(m.start(), m.group(1)) for m in re.finditer(r"\bfn\s+(\w+)", src)]
        # The spans of `#[cfg(test)]` items: a table built in a test is not a producer.
        tests = []
        for t in re.finditer(r"#\[cfg\(test\)\]\s*", src):
            k = src.find("{", t.end())
            semi = src.find(";", t.end())
            if k >= 0 and (semi < 0 or k < semi):
                tests.append((t.start(), matching(src, k)))
        f = 0
        for m in RECORD_SITE.finditer(src):
            cols = re.findall(r'"([^"]*)"', m.group(1))
            while f < len(fns) and fns[f][0] < m.start():
                f += 1
            if not cols or f == 0 or any(a <= m.start() < b for a, b in tests):
                continue                                    # a doc example or a test, not a producer
            for c in cols:
                lines.add(f"record {rel}:{fns[f - 1][1]} {c}")
    _RECORDS[raw] = lines
    return lines


def record_surface():
    lines = set()
    for path in rust_files():
        lines |= file_records(path)
    return lines


# ---- the contract schema ---------------------------------------------------------------------------------

def contract_surface():
    schema = json.loads(read(SCHEMA))
    defs = schema.get("$defs", {})
    lines = set()

    def resolve(node):
        seen = 0
        while isinstance(node, dict) and "$ref" in node and seen < 8:
            node = defs.get(node["$ref"].split("/")[-1], {})
            seen += 1
        return node

    def visit(node, path):
        node = resolve(node)
        if not isinstance(node, dict):
            return
        t = node.get("type")
        if t:
            lines.add(f"contract type {path} {','.join(t) if isinstance(t, list) else t}")
        for v in node.get("enum", []):
            lines.add(f"contract value {path} {json.dumps(v)}")
        for key in node.get("required", []):
            lines.add(f"contract required {path}.{key}")
        for kw in NARROWING:
            if kw in node and not (kw == "additionalProperties" and node[kw] is not False):
                lines.add(f"contract constraint {path} {kw}={json.dumps(node[kw], sort_keys=True)}")
        for key, sub in sorted(node.get("properties", {}).items()):
            lines.add(f"contract key {path}.{key}")
            visit(sub, f"{path}.{key}")
        items = node.get("items")
        if isinstance(items, dict):
            visit(items, path + "[]")
        for alt in ("oneOf", "anyOf"):
            for k, sub in enumerate(node.get(alt, [])):
                visit(sub, path)

    visit(schema, "$")
    return lines


# ---- the comparison --------------------------------------------------------------------------------------

def current():
    gs, unreadable = gs_surface()
    utils, problems = util_surface()
    return gs | utils | record_surface() | contract_surface(), unreadable + problems


def load_baseline():
    if not os.path.isfile(BASELINE):
        return None
    with io.open(BASELINE, encoding="utf-8") as fh:
        return set(l.rstrip("\n") for l in fh if l.strip() and not l.startswith("#"))


HEADER = """# The covered surface (docs/compatibility.md), as `scripts/surface_check.py` reads it. GENERATED - do not edit by hand.
# Growth: `python scripts/surface_check.py --bless`. A removal or change: `--accept-break "<reason>"`, recorded in
# scripts/SURFACE-BREAKS.txt. backlog/71 step 3.
"""


def write_baseline(lines):
    with io.open(BASELINE, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(HEADER)
        for l in sorted(lines):
            fh.write(l + "\n")


def compare(base, now):
    """(broken, grown): what the snapshot has that is gone, and what is new."""
    def req(s):
        return set(l for l in s if l.startswith(("contract required ", "contract constraint ")))
    broken = sorted((base - req(base)) - now) + sorted(req(now) - req(base))
    grown = sorted((now - req(now)) - base) + sorted(req(base) - req(now))
    return broken, grown


def section(line):
    return " ".join(line.split(" ")[:2]) if line.startswith("gs ") else line.split(" ")[0]


# ---- --selftest: prove the gate fires -------------------------------------------------------------------
#
# Each mutant is one exact text replacement in a real source file, handed to the checker IN MEMORY (`OVERRIDE`),
# never written to the tree. `break` must be reported as removed or changed, `grow` as new and nothing else, and
# `pass` as no difference at all. A gate that has only ever been seen to pass has not been seen to work.

MUTANTS = [
    ("a gs function renamed", "stdlib/rust/src/error.rs", "pub fn retry_is_safe(", "pub fn retry_safe(", "break"),
    ("a parameter's type changed", "stdlib/rust/src/ipc.rs",
     "pub fn send(ctx: &ServiceContext, peer: &str, msg: &Message)",
     "pub fn send(ctx: &ServiceContext, peer: &[u8], msg: &Message)", "break"),
    ("only a parameter's NAME changed", "stdlib/rust/src/ipc.rs",
     "pub fn send(ctx: &ServiceContext, peer: &str, msg: &Message)",
     "pub fn send(c: &ServiceContext, name: &str, m: &Message)", "pass"),
    ("a gs function added", "stdlib/rust/src/error.rs", "    pub fn retry_is_safe(",
     "    pub fn brand_new_thing(self) -> bool { true }\n    pub fn retry_is_safe(", "grow"),
    ("a derive dropped (Copy)", "stdlib/rust/src/addr.rs", "#[derive(Copy, Clone, PartialEq, Eq, Debug)]",
     "#[derive(Clone, PartialEq, Eq, Debug)]", "break"),
    ("a public field made private", "stdlib/rust/src/fs.rs", "pub is_dir: bool", "is_dir: bool", "break"),
    ("an Error variant removed", "stdlib/rust/src/error.rs", "    Busy,", "", "break"),
    ("gs::driver: a wait signature changed", "stdlib/rust/src/driver/wait.rs",
     "pub fn until(ctx: &ServiceContext, budget: Budget,", "pub fn until(ctx: &ServiceContext, budget: u64,", "break"),
    ("gs::driver: a method of a glob re-export removed", "stdlib/rust/src/driver/arena.rs",
     "pub fn zero(&self)", "fn zero(&self)", "break"),
    ("an SDK type gs re-exports lost a method", "sdk/rust/src/record.rs", "pub fn to_yaml(", "pub(crate) fn to_yaml(", "break"),
    ("an SDK type gs re-exports lost a variant", "sdk/rust/src/record.rs", "    Signed(i64),", "", "break"),
    ("an SDK method gs does NOT re-export renamed", "sdk/rust/src/service_context.rs", "pub fn log(", "pub fn log_renamed(", "pass"),
    ("a record column removed", "services/shell/src/main.rs", 'Table::new(&["name", "type", "size", "sealed"])',
     'Table::new(&["name", "type", "size"])', "break"),
    ("a record column added", "services/shell/src/main.rs", 'Table::new(&["name", "type", "size", "sealed"])',
     'Table::new(&["name", "type", "size", "sealed", "owner"])', "grow"),
    ("a godspeed report column renamed", "services/godspeed-pm/src/install.rs",
     'r.columns(&["package", "version", "state"]);', 'r.columns(&["package", "ver", "state"]);', "break"),
    ("a utility removed", "services/shell/src/main.rs", '"cd", "pwd", "home",', '"cd", "home",', "break"),
    ("a subcommand removed", "services/shell/src/main.rs", '"dns", "stats", "arp"', '"dns", "arp"', "break"),
    ("a contract key made required", "contracts/schema/service.schema.json", '"required": ["name"',
     '"required": ["placement", "name"', "break"),
    ("a contract key removed", "contracts/schema/service.schema.json", '"console_push"', '"console_pushx"', "break"),
    ("a contract pattern tightened", "contracts/schema/service.schema.json", '"^[a-z][a-z0-9-]*$"',
     '"^[a-z][a-z0-9]*$"', "break"),
]


def selftest(base, quiet):
    bad = 0
    for name, rel, old, new, expect in MUTANTS:
        path = os.path.join(ROOT, rel)
        text = read(path)
        if old not in text:
            print(f"  selftest FAIL  `{name}`: its text is no longer in {rel}; update the mutant")
            bad += 1
            continue
        OVERRIDE[key(path)] = text.replace(old, new, 1)
        try:
            now, problems = current()
        finally:
            del OVERRIDE[key(path)]
        broken, grown = compare(base, now)
        got = "unreadable" if problems else "break" if broken else "grow" if grown else "pass"
        if got != expect:
            bad += 1
            print(f"  selftest FAIL  `{name}` ({rel}): expected {expect}, got {got}")
            for l in (broken or grown)[:6]:
                print(f"                   {l}")
        elif not quiet:
            print(f"  selftest ok    {name}: {got}")
    return bad


def main():
    now, problems = current()
    base = load_baseline()
    if problems:
        print("surface - FAILURES (the checker could not read what it holds):")
        for p in problems:
            print(f"  FAIL  {p}")
        print("  Teach scripts/surface_check.py the form, or write the item in one it reads. A gap here is a")
        print("  part of the promise nobody is checking.")
        return 1
    if base is None:
        if "--bless" in sys.argv:
            write_baseline(now)
            print(f"surface: snapshot written, {len(now)} lines")
            return 0
        print("surface - FAIL  scripts/SURFACE.baseline.txt is missing; `--bless` writes it")
        return 1

    broken, grown = compare(base, now)

    if "--selftest" in sys.argv:
        bad = selftest(base, quiet=False) if not (broken or grown) else -1
        if bad < 0:
            print("surface: the tree differs from the snapshot; a mutant means something only against a clean one")
            return 1
        print(f"surface: selftest {len(MUTANTS) - bad}/{len(MUTANTS)} mutants behaved as expected")
        return 1 if bad else 0

    if "--accept-break" in sys.argv:
        k = sys.argv.index("--accept-break")
        reason = sys.argv[k + 1].strip() if k + 1 < len(sys.argv) else ""
        if len(reason) < MIN_REASON:
            print("surface: --accept-break needs a reason worth the name: what broke, and why it had to")
            return 1
        if not broken:
            print("surface: nothing is removed or changed; growth is recorded with --bless")
            return 1
        stamp = datetime.date.today().isoformat()
        new = not os.path.isfile(BREAKS)
        with io.open(BREAKS, "a", encoding="utf-8", newline="\n") as fh:
            if new:
                fh.write("# Every removal or change to the covered surface, with its reason. Appended by\n"
                         "# `scripts/surface_check.py --accept-break`; never edited (docs/compatibility.md).\n")
            fh.write(f"\n## {stamp}: {reason}\n")
            for l in broken:
                fh.write(f"- {l}\n")
            for l in grown:
                fh.write(f"+ {l}\n")
        write_baseline(now)
        print(f"surface: {len(broken)} break(s) recorded in scripts/SURFACE-BREAKS.txt; snapshot is now {len(now)} lines")
        return 0

    if "--bless" in sys.argv:
        if broken:
            print(f"surface: --bless refused - {len(broken)} line(s) are removed or changed, and --bless only")
            print("  records growth. Restore them, or record the break: --accept-break \"<reason>\"")
            for l in broken[:20]:
                print(f"    - {l}")
            return 1
        write_baseline(now)
        print(f"surface: {len(grown)} new line(s) recorded; snapshot is now {len(now)} lines")
        return 0

    if broken or grown:
        print("surface - FAILURES:")
        if broken:
            print(f"  FAIL  {len(broken)} line(s) of the covered surface are REMOVED or CHANGED (docs/compatibility.md):")
            for l in broken:
                print(f"          - {l}")
            print("        A changed item shows as its old line here and its new one below. Restore it, or record")
            print("        the break with its reason: python scripts/surface_check.py --accept-break \"<reason>\"")
        if grown:
            print(f"  FAIL  {len(grown)} new line(s) are not in the snapshot yet. Growth is allowed; record it:")
            for l in grown:
                print(f"          + {l}")
            print("        python scripts/surface_check.py --bless")
        print("")
        print("backlog/71 step 3: the covered surface may grow, and may not change without a recorded reason.")
        return 1

    bad = selftest(base, quiet=True)
    if bad:
        print(f"surface - FAIL  {bad} of {len(MUTANTS)} selftest mutant(s) did not behave: the gate is not seeing what")
        print("  it claims to (--selftest for the detail)")
        return 1
    counts = {}
    for l in now:
        counts[section(l)] = counts.get(section(l), 0) + 1
    gs_n = sum(v for k, v in counts.items() if k.startswith("gs "))
    print(f"surface: {len(now)} lines hold - gs {gs_n}, utilities {counts.get('util', 0)}, record columns "
          f"{counts.get('record', 0)}, contract schema {counts.get('contract', 0)}; {len(MUTANTS)} mutants caught")
    return 0


if __name__ == "__main__":
    sys.exit(main())
