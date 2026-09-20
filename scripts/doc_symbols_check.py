"""A doc that names a FUNCTION must name one that exists.

`scripts/doc_refs.py` checks that a doc pointing at a PATH names a file that exists. Nothing checked
the other half, and a RENAME is exactly when it breaks - silently, because the prose still reads
perfectly well. Four were found by hand in one audit:

  * `utilities/16_dir.md` named `cmd_ls` and `build_ls_table`, renamed when `ls` became `dir`.
  * `docs/records.md` named `render_table`/`render_json`/`render_yaml`; they are `to_grid`/`to_json`/
    `to_yaml`.
  * four `utilities/*.md` named `is_filter_builtin` in `stage_filter`/`stage_sink`, a pipe dispatch
    replaced by the unified `pipe_run`/`pipe_transform`.
  * `CLAUDE.md` cited `arch/arm/dwc2.rs`, a file DELETED by an amendment in the same document -
    invisible to `doc_refs.py` because the path is written relative to `kernel/src/`.

WHY A BASELINE RATHER THAN AN ALLOWLIST. Plenty of backticked SCREAMING_CASE is legitimately not our
code: `NO_HZ`, `SCI_EN`, `CONFIG_MODULE_SIG`, `LLVM_PROFILE_FILE`, `workflow_dispatch`. Others name a
PROPOSAL the doc is explicitly discussing (`send_remote` in the cluster appendix, `first_byte` in the
trace design notes). A hand-kept allowlist of those would rot exactly like the counts this repo keeps
having to re-take. A baseline ratchets instead: whatever is accepted today is recorded, anything NEW
fails, and the file may shrink freely. Same shape as `SHARED-SURFACE.baseline.txt`.

NOT SCANNED: `audits/` and `milestones/`. Those are append-only EVIDENCE and dated history (CLAUDE.md
§5) - a symbol that existed when the audit ran is CORRECT there, and rewriting it would falsify the
record. Same reason `backlog/29` keeps the serial capture it quotes verbatim.

A RECURRING CASE, recorded so the next person does not re-argue it from scratch: a CLOSED backlog
entry's post-mortem names the code it deleted, and naming it is the point - `backlog/37` explains a
bug by listing the five functions removed to fix it. That is the same "dated history" argument the
paragraph above makes for `audits/`, and it will happen again every time a removal is written up.
It is baselined per-symbol for now rather than exempting closed entries wholesale, because the
exemption would silently stop checking eleven existing files to make one new one pass. If this
keeps recurring, generalise it THEN, on the evidence of several cases rather than the first.
"""
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "DOC-SYMBOLS.baseline.txt")

# `scripts/` is SOURCE here too. Docs legitimately name the checkers - "gated by
# `help_philosophy_problems`" is exactly the kind of cross-reference this gate should be validating,
# not refusing. Scanning only Rust made a correct reference look like a stale one, which is the
# false positive that gets a checker ignored.
SRC_DIRS = ["kernel/src", "services", "sdk/rust/src", "osdev/src", "examples", "scripts"]
DOC_DIRS = ["docs", "utilities", "backlog"]
DOC_FILES = ["CLAUDE.md", "COMMANDMENTS.md", "README.md", "osdev/CLAUDE.md"]

# Anything that looks like a Rust item: snake_case or SCREAMING_CASE, with at least one underscore.
# One word alone is far too often prose ("`dir`", "`sealed`") to be worth the noise.
TOKEN = re.compile(r"`([a-z][a-z0-9]*(?:_[a-z0-9]+)+|[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)`")


def read(p):
    try:
        return io.open(p, encoding="utf-8", errors="replace").read()
    except OSError:
        return ""


def source_text():
    """Every byte of Rust in the tree, and the set of names DEFINED in it."""
    files = []
    for d in SRC_DIRS:
        for dirpath, _, names in os.walk(os.path.join(ROOT, d)):
            if "target" in dirpath.split(os.sep):
                continue
            files.extend(os.path.join(dirpath, n) for n in names
                         if n.endswith(".rs") or n.endswith(".py"))
    return "\n".join(read(f) for f in files), len(files)


def docs():
    out = list(DOC_FILES)
    for d in DOC_DIRS:
        p = os.path.join(ROOT, d)
        if not os.path.isdir(p):
            continue
        for dirpath, _, names in os.walk(p):
            for n in names:
                if n.endswith(".md"):
                    out.append(os.path.relpath(os.path.join(dirpath, n), ROOT).replace("\\", "/"))
    return sorted(out)


def unknown():
    """{token: [docs that name it]} for tokens that appear NOWHERE in the source."""
    text, nfiles = source_text()
    found = {}
    for rel in docs():
        s = read(os.path.join(ROOT, rel))
        for m in TOKEN.finditer(s):
            tok = m.group(1)
            # Present anywhere in the source - as a definition, a call, or a comment - is enough.
            # This is a DANGLING-reference check, not a "is it public API" check, and being stricter
            # would report every private helper a doc legitimately mentions.
            if tok not in text:
                found.setdefault(tok, set()).add(rel)
    return {k: sorted(v) for k, v in found.items()}, nfiles


def load_baseline():
    if not os.path.exists(BASELINE):
        return None
    out = set()
    for line in io.open(BASELINE, encoding="utf-8"):
        line = line.split("#", 1)[0].strip()
        if line:
            out.add(line)
    return out


def main():
    found, nfiles = unknown()
    base = load_baseline()
    if base is None:
        print("doc symbols: no baseline at %s - writing one" % os.path.relpath(BASELINE, ROOT))
        write_baseline(found)
        return 0

    new = sorted(k for k in found if k not in base)
    gone = sorted(k for k in base if k not in found)

    if new:
        print("DOC SYMBOL CHECK FAILED: %d backticked name(s) in current-tense docs match nothing"
              " in the source.\n" % len(new))
        for t in new:
            print("  %-34s %s" % (t, ", ".join(found[t])[:100]))
        print("\nA doc that names a function must name one that exists. Either the name is stale -"
              "\nsomething was renamed and the prose was not - or it is a deliberate mention of an"
              "\nexternal symbol or a proposal, in which case add it to"
              "\n  %s" % os.path.relpath(BASELINE, ROOT))
        print("with a reason on the line. The baseline may shrink freely; it may not grow silently.")
        return 1

    if gone:
        print("doc symbols: %d baseline entr(y/ies) no longer needed - the ratchet can tighten:"
              % len(gone))
        for t in gone:
            print("    %s" % t)
        print("  (remove them from %s)" % os.path.relpath(BASELINE, ROOT))

    print("doc symbols: every backticked name in %d current-tense docs resolves in the source "
          "(%d source files, %d baselined exceptions)" % (len(docs()), nfiles, len(base)))
    return 0


def write_baseline(found):
    lines = [
        "# Backticked names in current-tense docs that match nothing in the source.",
        "#",
        "# Generated by scripts/doc_symbols_check.py, which refuses any NEW entry. Each of these is",
        "# one of: an EXTERNAL symbol (Linux, LLVM, ACPI, GitHub Actions), or a PROPOSAL the doc is",
        "# explicitly discussing and has not built. Neither is a dangling reference.",
        "#",
        "# This file may SHRINK freely - the checker says so when it can. It may not grow without",
        "# somebody deciding the new entry is one of those two things.",
        "",
    ]
    for t in sorted(found):
        lines.append("%-34s # %s" % (t, ", ".join(found[t])[:90]))
    io.open(BASELINE, "w", encoding="utf-8", newline="").write("\n".join(lines) + "\n")
    print("doc symbols: baselined %d entries" % len(found))


if __name__ == "__main__":
    sys.exit(main())
