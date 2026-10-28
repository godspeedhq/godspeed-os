#!/usr/bin/env python3
"""Every subcommand answers `help`, and `help` tab-completes at every depth (0_conventions.md rules 2 and 9).

The shell's completion tables (`SUBCMD_FIRST`, `SUBCMD_SECOND`, `SUBCMD_THIRD`) are the list of words a
person can reach by Tab. Rule 2 says each of them answers `<util> <word> help`; rule 9 says Tab offers
`help` wherever it offers words. Until 2026-09-29 nothing checked either, and the audit that day found:

- 22 first-level words with no help at all (`events` x6, `chaos` x4, `churn` x3, `to`/`from` x3, `net` x2,
  `dir bytes`, `busiest` x3): `chaos kill-storm help` read `help` as a service name and refused it;
- no answer at depth three (`wifi debug trace help` printed "not a view");
- `help` completing at position 1 only.

This reads the tables and `sub_help` from the source, so it cannot drift from what the shell offers.

Rules:
1. For every `(util, word)` in `SUBCMD_FIRST` where `util` is in `UTILS`: `sub_help` has an arm
   `("util", "word")`, or a delegating arm `("util", v) => return <fn>(ctx, v)` and `<fn>` has an arm
   `"word"`. (A util not in `UTILS` is a library script or a command that owns its own words; those are
   out of this checker's reach and are listed, not failed.)
2. Depth two and three are answered by the word above them (the dispatch falls back to `sub_help(util,
   args[1])` for `argc >= 4`), so this checks that the fallback exists.
3. The completer appends `help` at positions 1, 2 and 3: position 1 via the `["version", "help"]` append,
   positions 2 and 3 via `complete_with_help` / the `avail[a] = "help"` line.
4. The OTHER DIRECTION (2026-10-28): every literal first word a utility's own help rows name - `shutdown
   status` in `("shutdown status", ...)`, both halves of `gpio high|low` - is a word Tab offers there,
   from `SUBCMD_FIRST` or from that utility's own completer (`"<util>".as_bytes() == cmd`). Rules 1-3
   asked whether what Tab offers answers `help`; nothing asked whether Tab offers what a command takes,
   and `shutdown`, `reboot`, `assert`, `background`, `fmt`, `gpio`, `input`, `jobs` and `scrollback` had
   drifted out of it - `shutdown` seen first, on the Wyse, by the operator.

Exit 1 with the list on any failure; exit 0 with a one-line summary otherwise.
"""
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SHELL = os.path.join(ROOT, 'services', 'shell', 'src', 'main.rs')
# A path argument points the check at another copy - how `commandments_redteam.py`-style probes prove it fires.
if len(sys.argv) > 1:
    SHELL = sys.argv[1]


def table(src, name):
    m = re.search(r"const %s: [^\n=]*=\s*&\[(.*?)\n\];" % re.escape(name), src, re.S)
    if not m:
        sys.exit("subcmd help: could not read %s from the shell source" % name)
    body = re.sub(r"//.*", "", m.group(1))
    return body


def first_level(src):
    body = table(src, 'SUBCMD_FIRST')
    out = []
    for util, words in re.findall(r'\(\s*"([^"]+)",\s*&\[([^\]]*)\]\s*\)', body):
        for w in re.findall(r'"([^"]+)"', words):
            out.append((util, w))
    return out


def utils(src):
    body = table(src, 'UTILS')
    return set(re.findall(r'"([^"]+)"', body))


def fn_body(src, name):
    m = re.search(r"\nfn %s\(.*?\n}\n" % re.escape(name), src, re.S)
    return m.group(0) if m else ''


def help_row_words(src):
    """{util: {word}} - the literal first words of each utility's TOP-LEVEL help block rows.

    A row's signature is `util word ...`; the word counts when it is a plain keyword (`status`,
    `fails-with`), and each half of an alternation (`high|low`) counts. Placeholders (`<path>`), options
    in brackets and quoted prompts are not keywords and are skipped."""
    out = {}
    blk = re.compile(r'^\s*"([a-z][a-z0-9-]*)"\s*=>\s*help_block\(ctx,\s*"([^"]+)",\s*"(?:[^"\\]|\\.)*",\s*&\[(.*?)\]\s*,\s*(?:true|false)\)',
                     re.S | re.M)
    for m in blk.finditer(src):
        util, title, rows = m.group(1), m.group(2), m.group(3)
        if title.split()[0] != util:
            continue
        for sig in re.findall(r'\(\s*"((?:[^"\\]|\\.)*)"\s*,', rows):
            toks = sig.split()
            if len(toks) < 2 or toks[0] != util:
                continue
            for w in toks[1].split('|'):
                if re.fullmatch(r'[a-z][a-z0-9-]*', w) and w not in ('help', 'version'):
                    out.setdefault(util, set()).add(w)
    return out


def own_completer_words(src):
    """{util: {word}} for a utility with its own completer (`if "<util>".as_bytes() == cmd ...`): every
    string literal in that block, up to the `return complete_from_list` that ends it."""
    out = {}
    for m in re.finditer(r'"([a-z-]+)"\.as_bytes\(\) == cmd', src):
        end = src.find('return complete_from_list', m.end())
        if end < 0:
            continue
        out.setdefault(m.group(1), set()).update(re.findall(r'"([^"]+)"', src[m.end():end]))
    return out


def main():
    src = io.open(SHELL, encoding='utf-8').read().replace('\r\n', '\n')
    words = first_level(src)
    util_set = utils(src)
    sub_help = fn_body(src, 'sub_help')
    if not sub_help:
        sys.exit("subcmd help: no `fn sub_help` in the shell source")

    direct = set(re.findall(r'\(\s*"([^"]+)",\s*"([^"]+)"\s*\)\s*=>', sub_help))
    delegated = {}
    # `("util", v) => return a(ctx, v) || b(ctx, v),` - every named function's arms count.
    for util, tail in re.findall(r'\(\s*"([^"]+)",\s*v\s*\)\s*=>\s*return\s+(.+?),\s*$', sub_help, re.M):
        fns = re.findall(r'(\w+)\(ctx,\s*v\)', tail)
        arm_words = set()
        for fn in fns:
            body = fn_body(src, fn)
            for line in body.split('\n'):
                if '=>' in line and line.strip().startswith('"'):
                    arm_words.update(re.findall(r'"([^"]+)"', line.split('=>')[0]))
        delegated[util] = (" / ".join(fns), arm_words)

    missing = []
    skipped = []
    for util, word in words:
        if util not in util_set:
            skipped.append((util, word))
            continue
        if (util, word) in direct:
            continue
        if util in delegated and word in delegated[util][1]:
            continue
        missing.append((util, word))

    problems = []
    if missing:
        problems.append("%d subcommand(s) answer no `help`:" % len(missing))
        for util, word in missing:
            problems.append("    `%s %s help`  (no arm in sub_help%s)" % (
                util, word, " or " + delegated[util][0] if util in delegated else ""))

    tabbed = {}
    for util, word in words:
        tabbed.setdefault(util, set()).add(word)
    own = own_completer_words(src)
    untabbed = sorted((u, w) for u, ws in help_row_words(src).items() if u in util_set
                      for w in ws if w not in tabbed.get(u, set()) and w not in own.get(u, set()))
    if untabbed:
        problems.append("%d word(s) a utility's help rows name are not offered by Tab (rule 4):" % len(untabbed))
        for util, word in untabbed:
            problems.append("    `%s %s`  (add it to SUBCMD_FIRST, with its `sub_help` arm)" % (util, word))

    if 'if argc >= 4 && args[argc - 1] == "help" && is_util(args[0])' not in src:
        problems.append("depth 3+ has no `help`: the `argc >= 4` fallback to `sub_help(util, args[1])` is gone")

    if '["version", "help"]' not in src:
        problems.append("position 1 does not complete `help` (the `[\"version\", \"help\"]` append is gone)")
    if 'return complete_with_help(ctx, line, tok_start, cands);' not in src:
        problems.append("position 2 does not complete `help` (`complete_with_help` is not used for SUBCMD_SECOND)")
    if 'avail[a] = "help"' not in src:
        problems.append("position 3 does not complete `help` (the `avail[a] = \"help\"` line is gone)")
    if '// depth-2 help' not in src:
        problems.append("a first-level word with no table below it no longer completes `help` (the depth-2 fallback is gone)")
    if '// depth-3 help' not in src:
        problems.append("a keyword of a keyword no longer completes `help` (the depth-3 fallback is gone)")

    if problems:
        print("SUBCOMMAND HELP CHECK FAILED:")
        for p in problems:
            print("  " + p)
        print()
        print("  Every word Tab can reach answers `help` (0_conventions.md rule 2) and Tab offers `help` at")
        print("  every depth (rule 9). Add an arm to `sub_help` in services/shell/src/main.rs - or, for a")
        print("  command with its own per-word help, a delegating arm `(\"<util>\", v) => return <fn>(ctx, v)`.")
        return 1

    print("subcmd help: every one of %d first-level word(s) under a utility answers `help`%s; depth 3+ "
          "falls back to the word above; `help` completes at positions 1, 2 and 3; every help-row keyword is "
          "offered by Tab" % (
              len(words) - len(skipped),
              (" (%d word(s) belong to library scripts and are not checked: %s)" % (
                  len(skipped), ", ".join(sorted({u for u, _ in skipped})))) if skipped else ""))
    return 0


if __name__ == '__main__':
    sys.exit(main())
