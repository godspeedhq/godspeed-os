#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""No document may show a POSIX/DOS word being USED as a Godspeed command.

WHY THIS EXISTS. `ls` became `dir` on the `feat/gsfs` branch. The rename reached the shell, the
utility specs and the help text - Commandment X reconciles those three - and it did NOT reach the
EXAMPLES scattered through `docs/`, where nine `gsh>` prompts and pipelines went on showing `ls`.
The operator spotted them after the v0.19.0 release notes shipped with one. Nothing was watching,
because the enforcement layer checks that the shell and its specs agree about vocabulary, not that
a worked example in a design note uses a word the shell still answers to.

THE LIST IS DERIVED, NEVER HAND-KEPT. `FOREIGN_HINTS` in `services/shell/src/main.rs` is the
shell's own table of words it recognises and refuses - `("ls", "dir")`, `("cat", "read")`,
`("rm", "delete")` and the rest. That table exists so a newcomer typing the Unix reflex is told the
Godspeed word. It is exactly the set of words that must never appear in our own examples, so it is
read from the source rather than copied here, and a word added there is enforced here the moment it
is written.

WHAT COUNTS AS "USED AS A COMMAND", deliberately narrow, because these words are ordinary English
and ordinary prose about other systems:

  * at a prompt          `gsh> ls /data`
  * first in a line or COLUMN of a fence that is a GSH TRANSCRIPT (it contains a `gsh>` prompt)
  * first in a pipeline inside backticks   `` `ls | match foo` ``
  * backticked with `help` or `version`   `` `ls help` `` - the two words every utility must answer

THE FENCE RULE WAS MISSING UNTIL 2026-09-26 and this docstring claimed it for months: the code had
only the three backtick patterns and never looked at a fence. `website/src/utilities.md` shipped
`ls /data | grep .txt` inside a fence to the published site with this gate green.

IT IS SCOPED TO A GSH TRANSCRIPT ON PURPOSE. A first attempt applied it to every fence and widened
the backtick rule to any argument; it produced 22 hits, mostly false - `mv {}, tp` (RISC-V assembly),
"which is where a stale superblock lives" (the English word), `cp build/os.img ...` (a HOST shell
command), "`rm -rf /` cannot exist" (prose saying so). A check that fires on assembly and on ordinary
English gets switched off, and then it guards nothing. A fence carrying `gsh>` has already declared
which shell it is, so a foreign word in command position there is wrong by construction; a host-shell
fence is left alone, because `cp` on the developer's machine is not a Godspeed command.

Column-awareness is not embellishment: the case that slipped was a two-column UNIX-vs-GODSPEEDOS
table whose offending word sat in the RIGHT-hand column, not at the start of the line.

One explicit escape, applied by the author and greppable, for a transcript that deliberately shows a
REFUSAL (typing `ls` to demonstrate the machine rejecting it is worth documenting):

    <!-- foreign-ok: why this fence may name a foreign word -->
    ```
    gsh> ls
    try `dir`
    ```

A sentence ABOUT the foreign word is fine and must stay fine - "`ls` is now `dir`", "the Unix
reflex is `ls`", "typing `ls` over serial" - which is why the check looks for the word in COMMAND
POSITION rather than anywhere. `audits/`, `milestones/` and `bugs/` are exempt: they are dated
records of what was typed on a day, and rewriting them would destroy the evidence.
"""
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SHELL = os.path.join(ROOT, "services", "shell", "src", "main.rs")
SKIP_DIRS = ("target", ".git", "build", "node_modules", "book", "audits", "milestones", "bugs")

# `gsh> ls ...`, or a bare command line inside a fence, or `` `ls | ...` ``.
PROMPT = re.compile(r"(?:^|`)\s*gsh>\s+([a-z][a-z0-9_-]*)")
BACKTICK_PIPE = re.compile(r"`\s*([a-z][a-z0-9_-]*)\s*\|")
# `` `ls /data` ``, `` `ls [path]` ``, `` `ls $x` `` - the word followed by an ARGUMENT, which is
# what makes it a command rather than a mention. A bare `` `ls` `` is prose ABOUT the word and
# must keep passing: "`ls` is now `dir`" is a sentence this repository needs to be able to write.
BACKTICK_ARG = re.compile(r"`\s*([a-z][a-z0-9_-]*)\s+[/\[$<][^`]*`")
# `` `ls help` `` / `` `ls version` `` - the two words EVERY utility must answer (0_conventions rule
# 1), so naming one after a foreign word asserts that a utility by that name exists. Kept to exactly
# these two: widening to any word made "which is where..." and `mv {}, tp` fire.
BACKTICK_VERB = re.compile(r"`\s*([a-z][a-z0-9_-]*)\s+(?:help|version)\s*`")

# Inside a GSH TRANSCRIPT fence: the word at the start of the line, or of a COLUMN (after two or more
# spaces), followed by an argument. The column half catches a side-by-side comparison table.
FENCE_CMD = re.compile(r"(?:^|\s{2,})([a-z][a-z0-9_-]*)\s+[^\s]")
# A fence is a gsh transcript if it prompts. Anything else may be a host shell and is left alone.
GSH_PROMPT = re.compile(r"\bgsh>\s")
# Author-applied escape on the line before a fence, for one that deliberately shows a refusal.
FOREIGN_OK = re.compile(r"<!--\s*foreign-ok\b")


def foreign_words():
    """The shell's own FOREIGN_HINTS table: the words it refuses, and what it says instead."""
    src = io.open(SHELL, encoding="utf-8", errors="ignore").read()
    m = re.search(r"const\s+FOREIGN_HINTS\s*:\s*&\[\(&str,\s*&str\)\]\s*=\s*&\[(.*?)\];", src, re.S)
    if not m:
        raise SystemExit(
            "foreign words: could not find FOREIGN_HINTS in services/shell/src/main.rs.\n"
            "        This check derives its list from that table rather than keeping its own.\n"
            "        If the table moved or was renamed, point this script at it - do not paste a copy.")
    body = re.sub(r"//[^\n]*", "", m.group(1))
    return dict(re.findall(r'\(\s*"([a-z0-9_-]+)"\s*,\s*"([a-z0-9_ -]+)"\s*\)', body))


def main():
    hints = foreign_words()
    if not hints:
        raise SystemExit("foreign words: FOREIGN_HINTS parsed as EMPTY - refusing to pass on a list "
                         "of nothing, which would make this check silently vacuous.")
    bad = []
    for root, dirs, files in os.walk(ROOT):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            if not name.endswith(".md"):
                continue
            path = os.path.join(root, name)
            rel = os.path.relpath(path, ROOT).replace(os.sep, "/")
            try:
                text = io.open(path, encoding="utf-8", errors="ignore").read()
            except OSError:
                continue
            lines = text.split("\n")
            # Which lines sit inside a fence, and is that fence a gsh transcript we may judge?
            judge = [False] * len(lines)
            # Lines inside a fence the author marked `foreign-ok`: EVERY rule is suppressed there,
            # not just the fence rule. A fence that exists to show the shell refusing `ls` will
            # naturally contain `gsh> ls`, so exempting only the fence rule exempted nothing.
            hushed = [False] * len(lines)
            i = 0
            while i < len(lines):
                if lines[i].lstrip().startswith("```"):
                    start = i
                    i += 1
                    while i < len(lines) and not lines[i].lstrip().startswith("```"):
                        i += 1
                    body = lines[start + 1:i]
                    exempt = start > 0 and bool(FOREIGN_OK.search(lines[start - 1]))
                    if exempt:
                        for k in range(start + 1, i):
                            hushed[k] = True
                    elif any(GSH_PROMPT.search(b) for b in body):
                        for k in range(start + 1, i):
                            judge[k] = True
                i += 1

            for line_no, line in enumerate(lines, 1):
                if hushed[line_no - 1]:
                    continue
                pats = [PROMPT, BACKTICK_PIPE, BACKTICK_ARG, BACKTICK_VERB]
                if judge[line_no - 1]:
                    pats.append(FENCE_CMD)
                for pat in pats:
                    hit = False
                    for m in pat.finditer(line):
                        word = m.group(1)
                        if word in hints:
                            bad.append((rel, line_no, word, hints[word], line.strip()[:76]))
                            hit = True
                            break
                    if hit:
                        break
    if bad:
        print("foreign words: %d example(s) use a word the shell REFUSES" % len(bad))
        print()
        for rel, line_no, word, ours, snippet in bad:
            print("  %s:%d" % (rel, line_no))
            print("      `%s` is not a Godspeed command - the shell answers `%s`" % (word, ours))
            print("      %s" % snippet)
        print()
        print("These are our own worked examples showing a command the machine will refuse. The")
        print("shell's FOREIGN_HINTS table is the source of truth and this check reads it directly.")
        print("A sentence ABOUT the foreign word is fine - this only flags COMMAND POSITION.")
        return 1
    print("foreign words: no document uses any of the %d words the shell refuses "
          "(%s...) as a command" % (len(hints), ", ".join(sorted(hints)[:4])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
