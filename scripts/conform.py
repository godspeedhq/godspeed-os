#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""`conform` - one front door to the enforcement layer, rendering like `rustc`.

WHY THIS EXISTS. The rules of this project are mechanised and good: 40 scripts, and `osdev`'s
`EXTRA_CHECKS` runs 16 of them plus `commandments.py` on every build. What there was no way to do was
ASK. A contributor could not find out whether they were clear without compiling a kernel, there was no
single verdict, and the rules were enforced without being DISCOVERABLE - you learned them by failing a
build, which is exactly what CLAUDE.md 22.7 says the repository must not require of a stranger.

    py scripts/conform.py              fix what is DECIDABLE, report what needs JUDGEMENT
    py scripts/conform.py --check      report both, change nothing (this is what CI wants)
    py scripts/conform.py --explain GS0403
    py scripts/conform.py --list       every rule, its code and its commandment
    py scripts/conform.py --selftest   prove the OUTPUT is good, not just that rules fire

THE ONE DESIGN DECISION, and everything else follows from it: **decidable versus judgement.**

  DECIDABLE  one right answer, no reader needed. An em-dash must be a hyphen; a CRLF must be an LF;
             trailing whitespace goes. `conform` fixes these and NAMES each file it touched.
  JUDGEMENT  the fix is not determined by the violation. A comment naming a dead symbol might want
             correcting, or might be correctly RECORDING a removal and want baselining - and only a
             human knows which. HALF of what the 2026-09-26 comment sweep found was the second kind.
             `conform` never guesses at these and never lets their existence be implied by silence.

So a clean run prints `fixed 3, 0 need a decision`, not a bare `ok`. "I changed your files and said
nothing" is what makes people distrust a formatter.

WHAT IS NOT COPIED FROM RUSTC: the caret. rustc underlines a column because it has a parser and knows
the span. These checkers report a file and usually a line; several report only a file. A caret under
the wrong token asserts precision the instrument does not have - the same defect as an instrument that
cannot tell a refusing device from an absent one - so a column is rendered only where a checker
genuinely produced one, and never synthesised.

THE CHECKER LIST IS READ FROM `osdev/src/main.rs`, deliberately. `EXTRA_CHECKS` is what a BUILD
enforces, so reading it is the only way `conform` cannot drift into passing while a build fails. A
checker added there is picked up here with no edit. `doc_command_check.py` reads osdev the same way,
for the same reason.

NEVER RUNS `commandments_redteam.py`. It plants `static mut SNEAK`, a `sneaky-mode` feature and
`ctx.spawn("probe-recv")` into source and restores with `git checkout`, which would destroy a
contributor's uncommitted work. It proves a checker CAN fail; that is a maintainer tool, run
deliberately, never from a verb a newcomer types.
"""
import io
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OSDEV_MAIN = os.path.join(ROOT, "osdev", "src", "main.rs")

# --------------------------------------------------------------------------------------------------
# The rule registry: what each checker enforces, which Commandment, and what to DO about it.
#
# This is the content that makes the output teach rather than merely refuse. `why` is why the rule
# exists (a rule without its reason gets worked around); `help` is the concrete next step, and it names
# the LEGITIMATE escape where one exists - a gate that only ever says "no" invites a contributor to
# disable it.
#
# Codes are stable and never reused, and the FIRST block is the constitution itself:
#
#   GS0001..GS0010  THE TEN COMMANDMENTS, one each - the number IS the numeral, so `GS0004` is
#                   Commandment IV and needs no decoder. A fixed block: there are exactly ten,
#                   permanently, so it never grows.
#   GS01xx          house writing conventions (dashes, line endings, the Python floor)
#   GS02xx          the kernel boundary and unsafe
#   GS03xx          contracts and authority
#   GS04xx          documentation and comments telling the truth
#
# The first cut had the Commandments at `GS09xx`, where the 09 meant nothing - it was the ninth group
# because it was written last - and gave the first block to a dash convention. That put the decoder
# between a reader and the law, and it was backwards for a project whose argument is that the model is
# the product. Renumbered before anything merged, because a code is stable forever once cited.
# --------------------------------------------------------------------------------------------------
RULES = {
    "dash_check.py": dict(
        code="GS0101", fixable=True, commandment=None, section="CLAUDE.md 21",
        title="an em-dash or en-dash appears in a tracked text file",
        why="A house writing convention, enforced repo-wide so that prose, code, comments and commit "
            "messages read the same. Only the plain ASCII hyphen is a dash here.",
        help="`conform` fixes this: every em-dash (U+2014) and en-dash (U+2013) becomes a hyphen. "
             "Box-drawing characters are fine and are left alone."),

    "line_ending_check.py": dict(
        code="GS0102", fixable=True, commandment=None, section="backlog/26",
        title="a tracked text file carries CRLF line endings",
        help="`conform` fixes this by rewriting the file with LF endings.",
        why="A CRLF in a boot config boots NOTHING while showing a perfect menu: U-Boot reads the "
            "trailing CR as part of every filename. It cost two reflashes before it was gated."),

    "unsafe_check.py": dict(
        code="GS0201", fixable=False, commandment="X", section="CLAUDE.md 18",
        title="the unsafe inventory does not match the source",
        why="Unsafe is permitted only in arch/, memory/, capability/ and smp/, plus the SDK's audited "
            "hardware/ABI layer, and every block carries a SAFETY comment. The grandfathered counts "
            "may FALL freely and may rise only by a recorded 18.5 amendment.",
        help="If you added an `unsafe` block, add it to `audits/unsafe-audit.md` in the same commit. "
             "If you removed one, lower the frozen count. If a service needs `unsafe`, it does not: "
             "go through the SDK's `Mmio`/`Dma` wrappers."),

    "arch_boundary_check.py": dict(
        code="GS0202", fixable=False, commandment="I", section="CLAUDE.md 4.1",
        title="neutral kernel code names an ISA, or contains inline assembly",
        why="A port is bounded to `arch/<isa>/`: you write that directory and nothing else in the "
            "kernel changes. Neutral code reaches hardware only through the `arch::imp` seam. Also: "
            "use `portable_atomic::AtomicU64`, never `core`'s - 32-bit RISC-V has no 64-bit atomic.",
        help="Add an `arch::imp` primitive and call that, rather than special-casing your arch at the "
             "call site. The fault is a MISSING primitive, not a stubborn call site."),

    "arch_seam_check.py": dict(
        code="GS0203", fixable=False, commandment="I", section="CLAUDE.md 4.1",
        title="an arch does not answer every member of the `arch::imp` seam",
        why="The other direction of the boundary: neutral code may only call the seam, and every arch "
            "must answer all of it. Discovered from usage rather than a hand-kept list, so it cannot "
            "drift.",
        help="Implement the named members in your `arch/<isa>/`. A stub is fine, but a stub that "
             "returns a number a watchdog reads must say whether zero means disabled or unlimited."),

    "contract_check.py": dict(
        code="GS0301", fixable=False, commandment="IV", section="CLAUDE.md 13.6",
        title="a service contract disagrees with what the spawn request actually grants",
        why="The kernel is no_std and cannot parse TOML: authority comes from the SPAWN REQUEST, never "
            "from the contract. The contract is the reviewable declaration, and this is what keeps the "
            "two from drifting. 13.6 exists because a model added a capability to a contract, reported "
            "that the kernel would grant it, and was wrong in a way nothing caught.",
        help="Change the supervisor's spawn row and the contract together. If the contract says a "
             "service may do something the spawn row does not grant, the service cannot do it - and "
             "will say it did."),

    "doc_refs.py": dict(
        code="GS0401", fixable=False, commandment=None, section="CLAUDE.md 26.7",
        title="a document points at a path that does not exist",
        why="A citation of a file that was deleted sends a reader after nothing. A citation of a "
            "backlog entry that was never written is worse: it reads as though the limitation HAS been "
            "recorded, which is the opposite of what 26.7 asks.",
        help="Re-point it, or write the entry you cited. If the target is genuinely gone, say so where "
             "the citation was rather than deleting the sentence."),

    "doc_symbols_check.py": dict(
        code="GS0402", fixable=False, commandment=None, section="CLAUDE.md 26.7",
        title="a document names a symbol that does not exist in the source",
        why="A rename breaks prose SILENTLY, because the sentence still reads correctly. Four had "
            "rotted when this was written, one of them in CLAUDE.md pointing at a file an amendment in "
            "the same document had deleted.",
        help="Name what does the job now. If the mention is deliberate - an external symbol, or a "
             "proposal that was never built - add it to `scripts/DOC-SYMBOLS.baseline.txt` with the "
             "reason on the line. The baseline may shrink freely; it may not grow silently."),

    "comment_symbol_check.py": dict(
        code="GS0403", fixable=False, commandment=None, section="CLAUDE.md 26.7, 26.14",
        title="a Rust comment names something that exists nowhere in the code",
        why="A comment is read BEFORE any document, because it sits beside the code being changed. "
            "There are 27,000 doc-comment lines here and until 2026-09-26 nothing checked one of them.",
        help="Name what does the job now - or, if it names something OUTSIDE this tree on purpose (a "
             "hardware register, an SBI call, a Linux function cited per 26.14), add it to "
             "`scripts/COMMENT-SYMBOLS.baseline.txt` with which kind it is. A comment that says "
             "\"X was deleted\" is RIGHT to name X: that is a record, and it belongs in the baseline."),

    "line_ref_check.py": dict(
        code="GS0404", fixable=False, commandment=None, section="CLAUDE.md 26.7",
        title="a `path:line` citation no longer points at what it claims",
        why="A line number is the fastest-rotting citation in the repository: every edit above it moves "
            "it. Audit 7 found 7 of 11 live citations wrong, with four documents citing ONE dead line "
            "because the citation had been copied rather than checked.",
        help="Re-point it, or cite the FUNCTION or the distinctive comment instead - those survive "
             "editing and a reader can grep for them. `audits/`, `milestones/` and `bugs/` are exempt: "
             "a line number correct on the day an audit ran is a true record of what was seen."),

    "facts_check.py": dict(
        code="GS0405", fixable=False, commandment="III", section="CLAUDE.md 26.4",
        title="a number a document restates disagrees with the code that owns it",
        why="Commandment III: do not duplicate truth. A restated number is a derived view, and a "
            "derived view that cannot be reconciled is a second truth waiting to lie.",
        help="Fix the document, not the code - the code owns the number. If the number should not be "
             "restated at all, describe it instead so it cannot rot again."),

    "foreign_word_check.py": dict(
        code="GS0406", fixable=False, commandment=None, section="CLAUDE.md Appendix B.4",
        title="a document shows a POSIX or DOS word being used as a command",
        why="The shell's vocabulary is fresh - `dir`, `read`, `delete`, `copy`, `match`, `count` - and "
            "a foreign word is a HINT, never an alias: `ls` does not run, it answers ``try `dir` ``. "
            "The `ls` to `dir` rename reached the shell, the specs and the help text, and missed TEN "
            "worked examples.",
        help="Use the Godspeed word. The list this checks is read from the shell's own `FOREIGN_HINTS`, "
             "so it cannot drift from what the shell actually refuses."),

    "doc_command_check.py": dict(
        code="GS0407", fixable=False, commandment=None, section="CLAUDE.md 26.7",
        title="a documented invocation does not work",
        why="Names resolving and numbers matching is not enough: nothing asked whether a documented "
            "PROMPT runs. `dir long /` lists a directory NAMED `long` and discards the path, which is a "
            "WRONG ANSWER rather than an error, and it shipped.",
        help="Run it and paste what it does. The accepted words are read from the shell's `SUBCMD_FIRST` "
             "and osdev's own `match suite`, so this cannot drift from either."),

    "docs_index_check.py": dict(
        code="GS0408", fixable=False, commandment=None, section="CLAUDE.md 5",
        title="a file in docs/ is not reachable from the docs index",
        why="An index that calls itself the index while files are invisible to it is worse than no "
            "index. Two documents sat unlisted, and six backlog entries before that.",
        help="Add a row to `docs/CLAUDE.md` saying what the file is FOR. A reader picks from the index; "
             "a one-word entry does not help them choose."),

    "site_check.py": dict(
        code="GS0409", fixable=False, commandment=None, section="CLAUDE.md 5",
        title="a hand-written website page disagrees with the repository",
        why="Most pages `{{#include}}` their source and cannot drift. The hand-written ones can, and "
            "they are the most public text in the project.",
        help="Fix the page. If you added a page, link it from `website/src/SUMMARY.md` - this checks "
             "both directions, so an unlinked page and a linked-but-absent page both fail."),

    "backlog_check.py": dict(
        code="GS0410", fixable=False, commandment=None, section="CLAUDE.md 26.7",
        title="a backlog entry has no status line, or is not linked from the index",
        why="26.7 says a limitation that cannot be closed is RECORDED. A record nobody can find is not "
            "a record: two hand surveys of that folder disagreed with each other before the status line "
            "was made mandatory and mechanical.",
        help="Put `**Status:` in the first 12 lines carrying OPEN or CLOSED, and add a row to "
             "`backlog/README.md`. An entry also owes its evidence, what is RULED OUT, and the next "
             "concrete step."),

    "python_floor_check.py": dict(
        code="GS0103", fixable=False, commandment=None, section="README.md, Requirements",
        title="a script uses a Python feature newer than the declared floor",
        why="`README.md` tells a contributor they need Python 3.8. That number was measured by hand, "
            "and a hand-measured number is right on the day it is taken and silently wrong afterwards. "
            "A contributor on the floor version would meet the drift as a SyntaxError from a CHECKER, "
            "which is the worst first experience this repository can offer.",
        help="Rewrite it to work on the floor, or RAISE the floor deliberately - `FLOOR` in "
             "`scripts/python_floor_check.py` and the Requirements line in `README.md`, together. "
             "Never let the number drift upward by accident."),

    # THE FALLBACK, and it is deliberately still here. `render_commandments` normally attributes a
    # failure to its Commandment and uses GS0001..GS0010. This entry catches the case where
    # `commandments.py` fails but its output cannot be parsed - a format change, say. Reporting it
    # UNATTRIBUTED is the safe direction: the alternative is a Commandment violation that renders as
    # nothing because a regex moved, which is the silent failure this whole tool exists to prevent.
    # GS0000 reads as "a Commandment violation, not attributed", at the head of the Ten block.
    "commandments.py": dict(
        code="GS0000", fixable=False, commandment="one of the Ten - not attributed",
        section="COMMANDMENTS.md",
        title="a Commandment check failed, and conform could not tell which",
        why="The per-commandment frame reads `Commandment <numeral> - <title>` out of the checker's "
            "output. Seeing this instead means that line was not found, so the violation is real but "
            "unattributed - most likely `commandments.py` changed its report format and "
            "`render_commandments` needs updating.",
        help="Read the raw output below: it names the commandment, the module and the rule. Then fix "
             "the parse in `render_commandments`, because an unattributed violation defeats the point "
             "of the frame. If you believe the RULE is wrong, that is a CLAUDE.md amendment with a "
             "written rationale, never a baseline entry."),
}

# The decidable class, fixed in place. Each entry: (name, finder, fixer) over one file's TEXT.
# COMPUTED, not written - and the reason is a small lesson about strict gates. The first cut put the
# literal em-dash and en-dash here, so the script that fixes dashes contained two. Writing them as
# source escapes does not help either: `dash_check` catches the escaped form deliberately, because
# "a dash written as a source escape is invisible to a literal scan". A tool that must NAME these
# characters therefore has to compute them, which is the honest way round.
#
# Also worth knowing, found the same way: `dash_check` reads `git ls-files`, so a brand new file
# violates NOTHING until it is staged. `git add -N` is how you find out before you commit.
DASHES = {chr(0x2014): "-", chr(0x2013): "-"}
SKIP_DIRS = {".git", "target", "build", "node_modules", "book", "tools", "__pycache__"}
TEXT_SUFFIXES = {".rs", ".md", ".toml", ".py", ".yml", ".yaml", ".sh", ".json", ".html", ".css",
                 ".js", ".txt", ".ld", ".gsh", ".conf", ".cfg", ".S", ".s"}


sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dash_check                     # noqa: E402  - for its tracked-text-file set
import line_ending_check              # noqa: E402  - for its .gitattributes rules


def _eol_lf_paths():
    """Files `.gitattributes` declares must be LF, resolved LAST-WINS as git resolves them.

    Last-wins is not a detail. `boot/** text eol=lf` is deliberately followed by `*.dtb binary`, so
    that marking a whole directory text does not have git "normalise" a 58 KB device tree blob. A
    first-wins match here would hand a binary to the CRLF rewriter.
    """
    try:
        rules = line_ending_check.rules()
    except Exception:
        return set()

    want = set()
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in line_ending_check.SKIP_DIRS]
        for n in filenames:
            rel = os.path.relpath(os.path.join(dirpath, n), ROOT).replace(os.sep, "/")
            verdict = None
            for rule in rules:
                pattern, kind = rule[0], rule[1]
                if line_ending_check.matches(rel, pattern):
                    verdict = kind
            if verdict == "lf":
                want.add(rel)
    return want


def fix_decidable(apply):
    """Returns [(rel, [what changed])]. Writes only when `apply`.

    THE RULE: `conform` may only fix what a gate would FAIL you for, and it takes the scope FROM that
    gate rather than restating it. That is what makes it structurally impossible for `conform` and a
    build to disagree - the same reason the checker LIST is read out of `osdev/src/main.rs`.

      GS0101  dashes  every tracked text file, per `dash_check.tracked_files`
      GS0102  CRLF    only where `.gitattributes` says `eol=lf`, per `line_ending_check`
    """
    changed = []
    lf_only = _eol_lf_paths()

    # `tracked_files()` is every tracked file; the SUFFIX FILTER lives in `dash_check.main()` against
    # `TEXT_SUFFIXES`. Borrowing only the listing helper made this broader than the gate it derives
    # from, and a fixture caught it: `conform` offered to "fix" the em-dash planted in
    # `tests/conformance/ui/an-em-dash-in-prose.case`, which `dash_check` cannot see because `.case` is
    # not a text suffix - so the fixer would have silently defeated its own test. Deriving scope from a
    # checker means deriving the same FILTER, not just its helper.
    for path in dash_check.tracked_files():
        if path.suffix.lower() not in dash_check.TEXT_SUFFIXES:
            continue
        p = str(path)
        rel = os.path.relpath(p, ROOT).replace(os.sep, "/")
        try:
            text = io.open(p, "rb").read().decode("utf-8")
        except (OSError, UnicodeDecodeError):
            continue

        what, new = [], text
        if any(d in new for d in DASHES):
            n = sum(new.count(d) for d in DASHES)
            for d, r in DASHES.items():
                new = new.replace(d, r)
            what.append("%d dash%s to hyphen" % (n, "" if n == 1 else "es"))
        if rel in lf_only and "\r\n" in new:
            what.append("CRLF to LF (.gitattributes: eol=lf)")
            new = new.replace("\r\n", "\n")

        if what:
            changed.append((rel, what))
            if apply:
                io.open(p, "w", encoding="utf-8", newline="").write(new)
    return changed


EXTRA_LIST = os.path.join(ROOT, "scripts", "CONFORM-EXTRA.txt")


def checkers():
    """(gated, ungated) - what a BUILD enforces, and what only `conform` runs.

    `gated` is read from `osdev/src/main.rs` so the two cannot disagree. `ungated` is
    `scripts/CONFORM-EXTRA.txt`: checkers that cannot reach the build path without a Rust edit. They
    are run and LABELLED, because the alternatives are to leave them unrun or to run them silently as
    though they were gated - and both hide something. The count is printed on every run, so the gap
    is visible and gets closed.
    """
    src = io.open(OSDEV_MAIN, encoding="utf-8", errors="replace").read()
    m = re.search(r"const EXTRA_CHECKS[^=]*=\s*&\[(.*?)\n\];", src, re.S)
    gated = re.findall(r'"(scripts/[a-z_0-9]+\.py)"', m.group(1)) if m else []
    if not gated:
        print("conform: could not read EXTRA_CHECKS from osdev/src/main.rs - refusing to guess "
              "which checks a build runs.", file=sys.stderr)
        sys.exit(2)
    gated = gated + ["scripts/commandments.py"]

    ungated = []
    if os.path.exists(EXTRA_LIST):
        for line in io.open(EXTRA_LIST, encoding="utf-8"):
            line = line.split("#", 1)[0].strip()
            if line and line not in gated:
                ungated.append(line)
    return gated, ungated


ANSI = re.compile(r"\x1b\[[0-9;]*m")

# One stable code per Commandment. `commandments.py` covers all ten, so framing it under a single code
# threw away the only thing the frame is for: naming WHICH of the Ten a violation breaks.
NUMERALS = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"]
# GS0001..GS0010: the number IS the commandment numeral, so `GS0004` is IV and needs no decoder.
# This is COMPUTED, which is why the renumber from the first scheme nearly missed it - a
# literal search-and-replace across the tree found 37 written codes and could not see this line.
# The UI fixture for a Commandment violation caught it, which is what the fixtures are for.
COMMANDMENT_CODE = {n: "GS00%02d" % (i + 1) for i, n in enumerate(NUMERALS)}
VIOLATION = re.compile(r"^\s*Commandment\s+(I|II|III|IV|V|VI|VII|VIII|IX|X)\s*-\s*(.+?)\s*$")


def commandment_text():
    """{numeral: the commandment's own words} read from COMMANDMENTS.md.

    Read rather than restated: a second copy of the Ten Commandments inside a Python file is exactly
    the duplicate truth Commandment III forbids, and it would drift the first time one is reworded.
    """
    out = {}
    path = os.path.join(ROOT, "COMMANDMENTS.md")
    if not os.path.exists(path):
        return out
    for line in io.open(path, encoding="utf-8", errors="replace"):
        m = re.match(r"^#+\s*(I|II|III|IV|V|VI|VII|VIII|IX|X)\.\s+(.+?)\s*$", line)
        if m:
            out.setdefault(m.group(1), m.group(2))
    return out
SITE = re.compile(r"\b((?:[a-z0-9_.-]+/)+[A-Za-z0-9_.-]+\.(?:rs|md|toml|py|json|ld|gsh))(?::(\d+))?")


def run_one(script):
    try:
        # ENCODING IS EXPLICIT. `text=True` alone decodes with the locale encoding, which on a
        # Windows machine is not UTF-8 - so every `§` a checker printed came through as a
        # replacement character. A tool that garbles the output it is quoting is not to be trusted
        # about anything else.
        r = subprocess.run([sys.executable, script], cwd=ROOT, capture_output=True,
                           text=True, encoding="utf-8", errors="replace")
    except OSError as e:
        return None, "conform: cannot run %s (%s)" % (script, e)
    return r.returncode, ANSI.sub("", (r.stdout or "") + (r.stderr or "")).rstrip()


def wrap(label, text, width=94):
    """`= label: body`, continuation lines aligned exactly under the body.

    The indent is `4 + len("= label: ")`, which is `len(label) + 8`. It was `+ 7` at first, so every
    wrapped line sat one column left of the body - the kind of thing that makes a tool look broken and
    costs it the authority it needs to be believed.
    """
    pad = " " * (len(label) + 8)
    words, lines, cur = text.split(), [], "    = %s: " % label
    for w in words:
        if len(cur) + len(w) + 1 > width and cur.strip() != "= %s:" % label:
            lines.append(cur.rstrip())
            cur = pad + w + " "
        else:
            cur += w + " "
    lines.append(cur.rstrip())
    return "\n".join(lines)


def covered_by_fixer(script, output, fixed_rels):
    """True if this failure is entirely the fixer's job and the fixer named every file.

    Then it is not a decision: it is the same problem already listed as fixable. Counting it in both
    columns would put a number in the "needs a human" column that needs no human.
    """
    rule = RULES.get(os.path.basename(script))
    if rule is None or not rule["fixable"] or not fixed_rels:
        return False
    sites = {m.group(1) for m in SITE.finditer(output)}
    return bool(sites) and sites <= set(fixed_rels)


def render_commandments(output):
    """One frame per failing Commandment, named and coded individually."""
    words = commandment_text()
    found = []
    for line in output.split("\n"):
        m = VIOLATION.match(line)
        if m and (m.group(1), m.group(2)) not in found:
            found.append((m.group(1), m.group(2)))

    if not found:
        return None

    frames = []
    for numeral, title in found:
        code = COMMANDMENT_CODE[numeral]
        body = ["error[%s]: %s" % (code, title)]
        site = None
        take = False
        detail = []
        for line in output.split("\n"):
            m = VIOLATION.match(line)
            if m:
                take = m.group(1) == numeral
                continue
            if take and line.strip():
                if site is None:
                    s = SITE.search(line)
                    if s:
                        site = s.group(1) + (":" + s.group(2) if s.group(2) else "")
                detail.append(line.strip())
        if site:
            body.append("   --> %s" % site)
        body.append("    |")
        body.append(wrap("commandment", "%s - %s" % (numeral, words.get(numeral, "see COMMANDMENTS.md"))))

        # ANYTHING THE FRAME STATES, THE PASSTHROUGH MUST NOT RESTATE. Two ways that was broken here:
        # the site appears on its own line and was taken as the `why` (it is already in the arrow),
        # and the checker's constant FOOTER repeated the frame's own `help` twice.
        FOOTER = ("COMMANDMENTS.md is the law", "An exemption is legitimate")
        useful = []
        for d in detail:
            if site and d.rstrip(":").strip() == site.split(":")[0]:
                continue
            if d.startswith(FOOTER):
                continue
            useful.append(d)

        for i, d in enumerate(useful[:3]):
            body.append(wrap("why" if i == 0 else "note", d))
        body.append(wrap("help", "`COMMANDMENTS.md` is the law and `docs/anti-patterns.md` has the "
                                 "correct pattern for this category. An exemption is legitimate ONLY "
                                 "if a CLAUDE.md amendment already accepts it - not a baseline entry."))
        body.append(wrap("note", "`py scripts/conform.py --explain %s` for the long form" % code))
        frames.append("\n".join(body))
    return "\n\n".join(frames)


def render(script, output, fixed_rels=()):
    """Frame one checker's failure.

    `fixed_rels` is what the fixer already claimed. A FIXABLE rule whose files are all in there gets
    one compact line instead of a frame: the "would fix" list above has already named them, and a
    second full explanation of the least interesting class trains a reader to skim. A fixable rule
    that fails on a file the fixer did NOT claim keeps its frame - that is the escaped-dash case,
    which the fixer cannot reach and a human must.
    """
    name = os.path.basename(script)
    rule = RULES.get(name)

    if name == "commandments.py":
        framed = render_commandments(output)
        if framed:
            return framed

    if covered_by_fixer(script, output, fixed_rels):
        n = len({m.group(1) for m in SITE.finditer(output)})
        return ("error[%s]: %s\n    = note: %d file%s, listed above; "
                "`py scripts/conform.py` fixes %s"
                % (rule["code"], rule["title"], n, "" if n == 1 else "s",
                   "it" if n == 1 else "them"))
    body = []
    if rule is None:
        # Not in the registry: pass the checker's own words through, LABELLED, never dropped.
        body.append("error: %s reported a problem (no rule entry yet)" % name)
        body.append("    = note: rendered unframed - add %s to conform.py's RULES to frame it" % name)
        body.append("")
        body.extend("  " + ln for ln in output.split("\n"))
        return "\n".join(body)

    body.append("error[%s]: %s" % (rule["code"], rule["title"]))

    site = SITE.search(output)
    if site:
        where = site.group(1) + (":" + site.group(2) if site.group(2) else "")
        body.append("   --> %s" % where)
    body.append("    |")
    if rule["commandment"]:
        body.append(wrap("commandment", "%s - %s" % (rule["commandment"], rule["section"])))
    else:
        body.append(wrap("rule", rule["section"]))
    body.append(wrap("why", rule["why"]))
    body.append(wrap("help", rule["help"]))
    body.append(wrap("note", "`py scripts/conform.py --explain %s` for the long form" % rule["code"]))
    # THE CHECKER'S UNIQUE CONTRIBUTION IS THE SITES, not a second explanation. The frame above
    # already carried why and what to do; repeating the checker's own advice under it printed the
    # same help twice, which rustc never does. So keep the summary line and every line that names a
    # file, drop the prose - and SAY the output was abridged, with the command to see all of it.
    # Silently dropping a checker's words is the one thing this tool must not do.
    lines = [ln for ln in output.split("\n") if ln.strip()]
    kept, dropped = [], 0
    for i, ln in enumerate(lines):
        if i == 0 or SITE.search(ln):
            kept.append(ln)
        else:
            dropped += 1

    body.append("")
    body.append("  %s reported:" % name)
    body.extend("  | " + ln.rstrip() for ln in kept)
    if dropped:
        body.append("  | (%d more line%s of explanation, which the frame above covers - "
                    "`py scripts/%s` for all of it)" % (dropped, "" if dropped == 1 else "s", name))
    return "\n".join(body)


def _para(label, text, width=94):
    """`  label: body` for --explain, continuation aligned under the body.

    Its own function rather than reshaping `wrap`'s output with `.replace()`, which is how the
    continuation came to sit one column out. --explain is what a contributor reads to UNDERSTAND a
    rule rather than to clear it, so it is the last place to leave a ragged edge.
    """
    head = "  %s: " % label
    pad = " " * len(head)
    lines, cur = [], head
    for w in text.split():
        if len(cur) + len(w) + 1 > width and cur.strip() != head.strip():
            lines.append(cur.rstrip())
            cur = pad + w + " "
        else:
            cur += w + " "
    lines.append(cur.rstrip())
    return "\n".join(lines)


def explain_commandment(code):
    """The long form for a GS09NN code, READ from `commandments.py --report`.

    Not restated here. Which checks cover a commandment - and which aspects are deliberately not
    mechanised - is a fact that report owns, and a second copy would drift about precisely how much of
    the constitution is proved. That is the last claim in this repository that should be allowed to
    overstate itself.
    """
    numeral = next((n for n, c in COMMANDMENT_CODE.items() if c.lower() == code.lower()), None)
    if numeral is None:
        return None

    words = commandment_text().get(numeral, "(see COMMANDMENTS.md)")
    print("%s - Commandment %s" % (code.upper(), numeral))
    print()
    print(_para("law ", words))
    print()

    # Separate argv elements. Passing "commandments.py --report" as ONE string made python look for a
    # file of that name, which fails quietly enough that the parse simply found nothing and the
    # explain printed "none found" - a wrong answer rather than an error.
    r = subprocess.run([sys.executable, os.path.join("scripts", "commandments.py"), "--report"],
                       cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace")
    out = ANSI.sub("", (r.stdout or "") + (r.stderr or ""))

    mech, manual = [], []
    for line in out.split("\n"):
        plain = line.rstrip()
        m = re.match(r"^\s*(I|II|III|IV|V|VI|VII|VIII|IX|X)\s+(\S+)\s+\[([a-z ]+)\]\s+(\w+)\s+(.*)$",
                     plain)
        if m and m.group(1) == numeral:
            mech.append((m.group(2), m.group(3).strip(), m.group(4), m.group(5).strip()))
            continue
        m2 = re.match(r"^\s*(I|II|III|IV|V|VI|VII|VIII|IX|X)\s+\[(.+?)\]\s*(.*)$", plain)
        if m2 and m2.group(1) == numeral:
            manual.append((m2.group(2), m2.group(3).strip()))

    if mech:
        print("  mechanised checks:")
        for cid, kind, verdict, title in mech:
            print("    %-24s [%s] %s" % (cid, kind, title))
    else:
        print("  mechanised checks: none found in the report")
    print()

    if manual:
        print("  NOT mechanised - human review, every time:")
        for kind, what in manual:
            print(_para("  [%s]" % kind, what))
    else:
        print("  NOT mechanised: nothing outstanding for this commandment")
    print()
    print(_para("scheme", "The number is the Commandment numeral - GS0001 is I, GS0010 is X. "
                          "`--list` shows every block."))
    print()
    print(_para("note", "Every one of the Ten has at least one mechanical check, which is what "
                        "\"10 of 10 mechanised\" means. It does NOT mean each is proved: eight aspects "
                        "across the Ten are human review, and `docs/x-residue.md` records what "
                        "Commandment X's checker specifically does not show."))
    return 0


def explain(code):
    r = explain_commandment(code)
    if r is not None:
        return r

    for name, rule in sorted(RULES.items()):
        if rule["code"].lower() == code.lower():
            print("%s - %s" % (rule["code"], rule["title"]))
            print()
            print("  enforced by : scripts/%s" % name)
            print("  commandment : %s" % (rule["commandment"] or "(house convention, not a commandment)"))
            print("  written in  : %s" % rule["section"])
            print("  auto-fixable: %s" % ("yes - `py scripts/conform.py` fixes it"
                                          if rule["fixable"] else "no - it needs a decision"))
            print()
            print(_para("why ", rule["why"]))
            print()
            print(_para("help", rule["help"]))
            print()
            print(_para("scheme", "Codes are grouped: GS0001..GS0010 are the Ten Commandments (the "
                                  "number is the numeral), GS01xx house conventions, GS02xx the "
                                  "kernel boundary, GS03xx contracts and authority, GS04xx "
                                  "documentation. `--list` shows them all."))
            return 0
    print("conform: no rule with code %s. `--list` shows them all." % code)
    return 2


UI_DIR = os.path.join(ROOT, "tests", "conformance", "ui")


def _parse_case(path):
    """(meta, plant, expect) from a `.case` file. See tests/conformance/ui/README.md."""
    text = io.open(path, encoding="utf-8").read()
    meta, plant, expect, where = {}, [], [], "head"
    for line in text.split("\n"):
        if line.strip() == "--- plant ---":
            where = "plant"
            continue
        if line.strip() == "--- expect ---":
            where = "expect"
            continue
        if where == "head":
            m = re.match(r"#\s*([a-z]+)\s*:\s*(.*)$", line)
            if m:
                k, v = m.group(1), m.group(2).rstrip()
                meta[k] = (meta.get(k, "") + " " + v).strip() if k == "why" else v
        elif where == "plant":
            plant.append(line)
        else:
            expect.append(line)
    # `\uXXXX` is decoded so a case can plant a character it must not CONTAIN literally.
    body = "\n".join(plant)
    body = re.sub(r"\\u([0-9a-fA-F]{4})", lambda m: chr(int(m.group(1), 16)), body)
    return meta, body, "\n".join(expect).strip()


def _norm(s):
    return "\n".join(ln.rstrip() for ln in s.strip().split("\n"))


def _dirty_paths():
    out = subprocess.run(["git", "status", "--porcelain"], cwd=ROOT,
                         capture_output=True, text=True).stdout
    dirty = set()
    for line in out.split("\n"):
        if len(line) > 3:
            dirty.add(line[3:].strip().strip('"'))
    return dirty


def selftest():
    """Plant each case, render it, restore, diff against its `expect`.

    THE GUARD IS SCOPED TO THE CASE TARGETS, not to the tree. It was the whole tree at first, which
    meant editing `conform.py` blocked `--selftest` - while iterating on the RENDERER, which is exactly
    when the golden files are what you want. A guard that stops the work it protects gets turned off,
    and then it protects nothing. The real risk is narrow: a plant landing on a file with unsaved work,
    and a crash before the restore.
    """
    if not os.path.isdir(UI_DIR):
        print("conform --selftest: no tests/conformance/ui/ - nothing to check")
        return 0

    cases = sorted(f for f in os.listdir(UI_DIR) if f.endswith(".case"))
    if not cases:
        print("conform --selftest: tests/conformance/ui/ holds no `.case` files")
        return 0

    parsed = [(fn, _parse_case(os.path.join(UI_DIR, fn))) for fn in cases]
    dirty = _dirty_paths()
    at_risk = sorted({m.get("target", "") for _, (m, _, _) in parsed} & dirty)
    if at_risk:
        print("conform --selftest: these case TARGETS have uncommitted changes, and a case plants a")
        print("violation into them to measure it:")
        for p in at_risk:
            print("    %s" % p)
        print("Commit or stash those files first. (The restore is byte-for-byte from an in-memory")
        print("copy and never touches git - but a crash with unsaved work in a planted file is not a")
        print("risk worth taking for a test.) Everything else in the tree may be dirty; only these")
        print("matter.")
        return 2

    passed, failed = 0, []
    for fn, (meta, plant, expect) in parsed:
        target = os.path.join(ROOT, meta.get("target", ""))
        checker = meta.get("checker", "")
        mode = meta.get("mode", "append")
        if not os.path.isfile(target) or not checker:
            failed.append((fn, "case is malformed: needs `# target:` and `# checker:`"))
            continue

        original = io.open(target, "rb").read()
        try:
            text = original.decode("utf-8")
            io.open(target, "w", encoding="utf-8", newline="").write(
                text + plant if mode == "append" else plant)

            rc, out = run_one(checker)
            if rc is None:
                failed.append((fn, out))
                continue
            if expect.strip() == "NOTHING":
                got = "" if rc == 0 else render(checker, out)
                want = ""
            else:
                # Dry-run the fixer so a DECIDABLE case renders in its compact form, exactly as it
                # would for a real `--check`. Never `apply=True`: the plant must survive being
                # measured.
                got = render(checker, out, [rel for rel, _ in fix_decidable(apply=False)]) if rc else ""
                want = expect
        finally:
            io.open(target, "wb").write(original)

        if _norm(got) == _norm(want):
            passed += 1
            print("  ok    %s" % fn)
        else:
            failed.append((fn, None))
            print("  FAIL  %s" % fn)
            print("    --- expected ---")
            for ln in (want or "(no finding)").split("\n"):
                print("    %s" % ln)
            print("    --- got ---")
            for ln in (got or "(no finding)").split("\n"):
                print("    %s" % ln)

    print()
    for fn, why in failed:
        if why:
            print("  %s: %s" % (fn, why))
    print("conform --selftest: %d of %d case(s) render as expected" % (passed, len(cases)))
    return 0 if passed == len(cases) else 1


def main(argv):
    if "--selftest" in argv:
        return selftest()

    if "--explain" in argv:
        i = argv.index("--explain")
        if i + 1 >= len(argv):
            print("conform: --explain needs a code, e.g. --explain GS0101")
            return 2
        return explain(argv[i + 1])

    if "--list" in argv:
        # THE LEGEND FIRST. This is the command whose job is "what are the rules", so it is where the
        # code convention belongs - it was previously stated only in a comment in this file and a
        # table in `docs/conformance.md`, neither of which is reachable from a contributor's actual
        # position, which is staring at `error[GS0004]` in a terminal. A convention nobody can find is
        # not a convention.
        print("Rule codes are stable and never reused. The number is not arbitrary:")
        print()
        print("    GS0001..GS0010   the Ten Commandments, one each - the NUMBER IS THE NUMERAL,")
        print("                     so GS0004 is Commandment IV. Exactly ten, permanently.")
        print("    GS01xx           house writing conventions (dashes, line endings, Python floor)")
        print("    GS02xx           the kernel boundary and unsafe")
        print("    GS03xx           contracts and authority")
        print("    GS04xx           documentation and comments telling the truth")
        print("    GS0000           a Commandment violation conform could not attribute (a fallback:")
        print("                     unattributed beats a violation that renders as nothing)")
        print()
        print("`--explain <code>` gives the long form for any of them. Full rationale:")
        print("docs/conformance.md.")
        print()
        print("%-8s %-28s %-12s %s" % ("code", "enforced by", "commandment", "fixable"))
        # THE TEN FIRST, in numeral order, because they have the first block and they are the law.
        # Listing them last would undo the whole point of the renumbering.
        for numeral in NUMERALS:
            print("%-8s %-28s %-12s %s" % (COMMANDMENT_CODE[numeral], "commandments.py",
                                           numeral, "no"))
        print()
        for name, rule in sorted(RULES.items(), key=lambda kv: kv[1]["code"]):
            if name == "commandments.py":
                continue
            print("%-8s %-28s %-12s %s" % (rule["code"], name,
                                           rule["commandment"] or "-",
                                           "yes" if rule["fixable"] else "no"))
        return 0

    check_only = "--check" in argv

    # ---- the decidable half -------------------------------------------------------------------
    changed = fix_decidable(apply=not check_only)

    # ---- the judgement half -------------------------------------------------------------------
    gated, ungated = checkers()
    scripts = gated + ungated
    fixed_rels = [rel for rel, _ in changed]
    ran, failed, deferred, reports = 0, [], [], []
    for s in scripts:
        rc, out = run_one(s)
        if rc is None:
            print(out, file=sys.stderr)
            print("conform: refusing to report a verdict - a checker that cannot RUN is not a "
                  "checker that passed.", file=sys.stderr)
            return 2
        ran += 1
        if rc != 0:
            # Split the two kinds: what the fixer already handles, and what wants a human.
            (deferred if covered_by_fixer(s, out, fixed_rels) else failed).append(s)
            reports.append(render(s, out, fixed_rels))

    # ---- report -------------------------------------------------------------------------------
    if changed:
        verb = "would fix" if check_only else "fixed"
        print("%s (decidable - one right answer, no reader needed):" % verb)
        for rel, what in changed:
            print("    %-58s %s" % (rel, ", ".join(what)))
        print()

    for r in reports:
        print(r)
        print()

    n_fix, n_judge = len(changed), len(failed)
    n_problems = n_judge + len(deferred)
    if check_only:
        head = "conform --check: %d would be fixed, %d need a decision" % (n_fix, n_judge)
    else:
        head = "conform: fixed %d, %d need a decision" % (n_fix, n_judge)
    tail = ""
    if ungated:
        tail = (" (%d of them not yet on the build path - scripts/CONFORM-EXTRA.txt)" % len(ungated))
    print("%s - %d checks ran, %d passed%s" % (head, ran, ran - n_problems, tail))

    if n_judge == 0 and n_fix == 0:
        print("nothing to do. Every rule this project enforces is satisfied.")
    return 1 if (n_judge or (check_only and n_fix)) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
