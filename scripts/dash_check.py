#!/usr/bin/env python3
"""Enforce the house dash convention (CLAUDE.md §21): NO em-dash (U+2014) or en-dash (U+2013) anywhere.

Only the plain ASCII hyphen (-) is a permitted dash - in prose, code, comments, string literals, and
docs. Box-drawing characters (U+2500 etc.) are fine; this checks ONLY the two dash code points. The rule
was previously kept by hand-grepping each diff; this makes it a mechanical CI guard, like
`unsafe_check.py` / `contract_check.py` / `arch_boundary_check.py` (discipline as mechanism, §26).

Exit: 0 if no em/en dash is present in tracked text files, 1 otherwise.
"""

import subprocess
import sys
import re
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent
EM, EN = chr(0x2014), chr(0x2013)  # em-dash / en-dash; via chr() so this file has no literal dash

# ESCAPED forms count too. A dash written as a source escape is invisible to a literal scan, and the
# miss is not cosmetic: osdev/src/validator.rs held three harness expectations containing an escaped
# em-dash while the probes they matched had been purged to plain hyphens. The strings stopped
# matching, so each of those tests sat out its 900-second timeout and then FAILED - a broken suite
# hiding behind a passing dash gate. Built with chr() for the same reason as the pair above.
_D = chr(92)
ESCAPED = re.compile(_D + _D + r"(?:u" + _D + r"{20(?:14|13)" + _D + r"}|u20(?:14|13)|"
                     r"N" + _D + r"{E[MN] DASH" + _D + r"}|x" + _D + r"{20(?:14|13)" + _D + r"})", re.I)
# Text file suffixes worth scanning. Binary assets (images, fonts, os.img) are skipped.
TEXT_SUFFIXES = {".rs", ".md", ".toml", ".py", ".yml", ".yaml", ".sh", ".json", ".html", ".css",
                 ".js", ".txt", ".gsh", ".c", ".h", ".s", ".ld", ".cfg", ".conf"}


def tracked_files() -> list[Path]:
    """Git-tracked files, so generated/vendored trees (target/, build/, tools/) are never scanned."""
    out = subprocess.run(["git", "ls-files"], cwd=REPO_ROOT, capture_output=True, text=True, check=True)
    return [REPO_ROOT / line for line in out.stdout.splitlines() if line]


def main() -> int:
    violations: list[str] = []
    for path in tracked_files():
        if path.suffix.lower() not in TEXT_SUFFIXES:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError):
            continue  # binary or removed; not our concern
        for i, line in enumerate(text.splitlines(), 1):
            esc = ESCAPED.search(line)
            if EM in line or EN in line or esc:
                if esc:
                    kind = "escaped dash (" + esc.group(0) + ")"
                else:
                    kind = "em-dash (U+2014)" if EM in line else "en-dash (U+2013)"
                rel = path.relative_to(REPO_ROOT).as_posix()
                violations.append(f"  {rel}:{i}: {kind} - use a plain ASCII hyphen (-)")

    if violations:
        print("Dash check - FAILURES (CLAUDE.md §21: only the ASCII hyphen is permitted):")
        for v in violations[:200]:
            print(v)
        if len(violations) > 200:
            print(f"  ... and {len(violations) - 200} more")
        print(f"\n{len(violations)} em/en dash(es). Replace each with a plain hyphen (-).")
        return 1

    print("Dash check passed - no em-dash (U+2014) or en-dash (U+2013), literal or escaped, in any tracked text file.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
