#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""No document may show a SUBCOMMAND or a TEST SUITE the code does not have.

WHY THIS EXISTS. The enforcement layer checks that a documented NAME resolves somewhere
(`doc_symbols_check.py`) and that a documented NUMBER matches the code that owns it
(`facts_check.py`). Nothing checked that a documented INVOCATION works. So the 2026-09-26 audit found
a class no gate could see:

  `utilities/50_seal.md`   showed `dir long /`   - `long` was removed from `dir`; the shell parses
                                                  `long` as the PATH and lists a directory by that
                                                  name, discarding the `/`. A wrong answer, not an
                                                  error, which is the worst shape for a reader.
  `utilities/47_events.md` said `events blocked` - refused by name: "that reads LIVE kernel state,
                                                  so it is `trace blocked`".
  `docs/ahci.md`           cited `osdev test blockdev-ahci` three times - no such suite; the
                                                  dispatch has `blockdev` and `blockdev-reboot`.

Each is a reader following the documentation and getting nothing, or worse, something.

THE TWO LISTS ARE DERIVED, NEVER HAND-KEPT, for the same reason `foreign_word_check.py` reads
`FOREIGN_HINTS` from the shell: a copy rots, and the point is to catch rot.

  subcommands   `SUBCMD_FIRST` in `services/shell/src/main.rs` - the shell's own completion table,
                which exists so tab offers the real words.
  test suites   the `match suite` arms in `osdev/src/main.rs`.

WHAT COUNTS AS AN INVOCATION - only two things, because everything wider was worse than the hole:

  * `gsh> <cmd> <sub>`    a PROMPT asserts this was typed and worked
  * `osdev test <name>`   a literal tool invocation with a closed set of names

A first cut also read two words inside one backtick pair, and produced 80 hits of which almost all
were false, in two ways worth recording:

  A BACKTICK-SPAN ARTIFACT. `` `unsafe_check.py` to scan `sdk/` `` contains, between the closing
  backtick of one span and the opening of the next, the text " to scan " - and `to` really is a pipe
  stage (`to json`). Any rule that reads between backticks without tracking parity has this bug.

  PROPOSALS. `docs/drives.md` discusses `drives use`; `backlog/28` proposes `net listeners`. Naming a
  command that does not exist yet is what a design note is FOR, and a check hostile to that gets
  switched off.

A prompt has neither problem: no English collision, and nobody writes a proposal at a prompt. A second
token that is a PATH, a PLACEHOLDER, a number or a flag is an argument and is skipped - `dir /data`,
`write <path>`, `events log 5` all pass. A bare `` `events` `` is prose and must keep passing.

`audits/`, `milestones/` and `bugs/` are exempt: dated records of what was typed on a day.
"""
import io
import os
import re
import sys

# Author-applied escape on the line before a fence, for a MOCKUP or a PROPOSAL.
# `docs/drives.md` section 6 is titled "How it looks (`gsh>` mockups)"; `utilities/46_trace.md`
# shows what was "proposed here... they shipped as a single `trace chain`". Both are design
# documents doing their job, and a check that forbids that gets switched off - so the escape is
# visible and greppable rather than inferred.
COMMAND_OK = re.compile(r"<!--\s*doc-command-ok\b")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SHELL = os.path.join(ROOT, "services", "shell", "src", "main.rs")
OSDEV = os.path.join(ROOT, "osdev", "src", "main.rs")
SKIP_DIRS = ("target", ".git", "build", "node_modules", "book", "audits", "milestones", "bugs")

# At a gsh prompt ONLY. A backticked pair was tried and produced a flood of false hits - see the
# header: `` `a` to scan `b` `` reads as `` `to scan` `` to any regex that ignores backtick parity.
PROMPT_PAIR = re.compile(r"gsh>\s+([a-z][a-z0-9_-]*)\s+(\S+)")
# `osdev test <name>`, backticked or bare at a prompt.
# A TRAILING colon is prose - "osdev test files: 222/0" reads "the files suite: 222/0" - so a
# name counts as prefixed (`perf:bp3`) only when something follows the colon.
OSDEV_TEST = re.compile(r"osdev\s+test\s+([a-z][a-z0-9-]+(?::[a-z0-9-]+)*)")


def read(path):
    try:
        return io.open(path, encoding="utf-8", errors="ignore").read()
    except OSError:
        return ""


def subcmd_table():
    """The shell's own `SUBCMD_FIRST`: command -> the words it accepts in first position."""
    src = read(SHELL)
    m = re.search(r"const SUBCMD_FIRST:\s*&\[\(&str,\s*&\[&str\]\)\]\s*=\s*&\[(.*?)\n\];", src, re.S)
    if not m:
        raise SystemExit(
            "doc commands: could not find SUBCMD_FIRST in services/shell/src/main.rs.\n"
            "        This check derives its table from the shell rather than keeping a copy.\n"
            "        If it moved or was renamed, point this script at it - do not paste a copy.")
    body = re.sub(r"//[^\n]*", "", m.group(1))
    table = {}
    for cmd, words in re.findall(r'\(\s*"([a-z0-9_-]+)"\s*,\s*&\[(.*?)\]\s*\)', body, re.S):
        subs = set(re.findall(r'"([a-z0-9_:-]+)"', words))
        # A command may appear twice (`dir` is in both completion tables); union the words.
        table.setdefault(cmd, set()).update(subs)
    return table


def test_suites():
    """The suite names `osdev test` actually dispatches."""
    src = read(OSDEV)
    m = re.search(r"match suite \{(.*?)\n    \}", src, re.S)
    if not m:
        raise SystemExit(
            "doc commands: could not find the `match suite` block in osdev/src/main.rs.\n"
            "        Point this script at it rather than keeping a list here.")
    body = re.sub(r"//[^\n]*", "", m.group(1))
    names = set(re.findall(r'"([a-z0-9:-]+)"\s*=>', body))
    # `s if s == "chaos-repro" || s.starts_with("chaos-repro:")` - guard arms, not literal arms, so
    # the plain `"name" =>` sweep above misses them and a REAL suite looked absent.
    names.update(re.findall(r's\s*==\s*"([a-z0-9:-]+)"', body))
    # `s if s.starts_with("perf:")` and friends: prefixes, not exact names.
    prefixes = set(re.findall(r'starts_with\("([a-z0-9-]+):"\)', body))
    names.update(prefixes)
    return names, prefixes


def is_argument(tok):
    """A second token that is an ARGUMENT rather than a subcommand."""
    return ("/" in tok or "." in tok or '"' in tok
            or tok[0] in "/<[$0123456789-"
            or tok in ("path", "name", "file", "svc", "service", "n", "secs", "port", "col",
                       "dir", "src", "dst", "cmd", "word", "text", "key", "id"))


def main():
    subs = subcmd_table()
    suites, prefixes = test_suites()
    if not subs or not suites:
        raise SystemExit("doc commands: a derived table parsed as EMPTY - refusing to pass "
                         "vacuously on a list of nothing.")

    bad = []
    for root, dirs, files in os.walk(ROOT):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            if not name.endswith(".md"):
                continue
            path = os.path.join(root, name)
            rel = os.path.relpath(path, ROOT).replace(os.sep, "/")
            lines = read(path).split("\n")
            # Lines inside a fence the author marked `doc-command-ok` are not judged.
            hushed = [False] * len(lines)
            i = 0
            while i < len(lines):
                if lines[i].lstrip().startswith("```"):
                    start = i
                    i += 1
                    while i < len(lines) and not lines[i].lstrip().startswith("```"):
                        i += 1
                    if start > 0 and COMMAND_OK.search(lines[start - 1]):
                        for k in range(start + 1, i):
                            hushed[k] = True
                i += 1

            for line_no, line in enumerate(lines, 1):
                if hushed[line_no - 1]:
                    continue
                for pat in (PROMPT_PAIR,):
                    for m in pat.finditer(line):
                        cmd, sub = m.group(1), m.group(2)
                        # Strip trailing punctuation the capture swept up: a prompt quoted INSIDE
                        # prose ends `drives flash`, and the comma is not part of the subcommand.
                        sub = sub.rstrip('`,.;:)]"')
                        if not sub:
                            continue
                        if cmd not in subs or is_argument(sub):
                            continue
                        if sub not in subs[cmd]:
                            bad.append((rel, line_no,
                                        "`%s %s` - `%s` takes %s" % (
                                            cmd, sub, cmd,
                                            ", ".join(sorted(subs[cmd])) or "no subcommand"),
                                        line.strip()[:76]))
                for m in OSDEV_TEST.finditer(line):
                    suite = m.group(1)
                    if suite in suites:
                        continue
                    if any(suite.startswith(p + ":") for p in prefixes):
                        continue
                    bad.append((rel, line_no,
                                "`osdev test %s` - no such suite in osdev's dispatch" % suite,
                                line.strip()[:76]))

    if bad:
        print("doc commands: %d documented invocation(s) the code does not answer" % len(bad))
        print()
        for rel, line_no, why, snippet in bad:
            print("  %s:%d" % (rel, line_no))
            print("      %s" % why)
            print("      %s" % snippet)
        print()
        print("A reader following one of these gets nothing, or a wrong answer - `dir long /` lists a")
        print("directory NAMED `long` and discards the path. The two tables are read from")
        print("`SUBCMD_FIRST` and osdev's `match suite`, so fix the doc or the code, never this list.")
        return 1

    print("doc commands: every documented subcommand and `osdev test` suite exists "
          "(%d commands with subcommands, %d suites)" % (len(subs), len(suites)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
