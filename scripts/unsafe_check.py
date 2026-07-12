#!/usr/bin/env python3
"""Verify every unsafe usage in kernel/src/ is accounted for in docs/unsafe-audit.md.

Rules enforced:
  - A file not in the audit that gains unsafe lines          → FAIL
  - A file whose unsafe count exceeds its audited baseline   → FAIL
  - A file whose unsafe count dropped below its baseline     → INFO
    (safe to update the audit to lock in the reduction)

Every FAIL means either a new unsafe block was added without a SAFETY comment
and an audit entry, or an out-of-policy file grew its unsafe surface.

Exit: 0 if no failures, 1 if any.
"""

import re
import sys
from pathlib import Path

REPO_ROOT   = Path(__file__).parent.parent
KERNEL_SRC  = REPO_ROOT / "kernel" / "src"
SERVICES    = REPO_ROOT / "services"
AUDIT_FILE  = REPO_ROOT / "docs" / "unsafe-audit.md"

INVENTORY_START = "<!-- unsafe-inventory-start -->"
INVENTORY_END   = "<!-- unsafe-inventory-end -->"


def count_unsafe(path: Path) -> int:
    """Count non-comment lines that contain the `unsafe` keyword."""
    count = 0
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.strip().startswith("//"):
            continue
        if re.search(r'\bunsafe\b', line):
            count += 1
    return count


def parse_audit() -> dict[str, int]:
    """Extract {relative_path: count} from the inventory table in the audit file."""
    text = AUDIT_FILE.read_text(encoding="utf-8")
    in_block = False
    inventory: dict[str, int] = {}

    for line in text.splitlines():
        if INVENTORY_START in line:
            in_block = True
            continue
        if INVENTORY_END in line:
            break
        if not in_block or not line.startswith("|"):
            continue
        parts = [p.strip() for p in line.strip("|").split("|")]
        if len(parts) < 2:
            continue
        path_col  = parts[0].strip()
        count_col = parts[1].strip()
        if not path_col or path_col.startswith("-") or path_col.startswith("File"):
            continue
        try:
            inventory[path_col] = int(count_col)
        except ValueError:
            continue

    return inventory


def main() -> int:
    if not AUDIT_FILE.exists():
        print(f"FAIL: audit file not found: {AUDIT_FILE}")
        return 1

    audit = parse_audit()
    if not audit:
        print("FAIL: no inventory found in audit file (missing markers or empty table)")
        return 1

    failures: list[str] = []
    infos: list[str] = []

    for rs_file in sorted(KERNEL_SRC.rglob("*.rs")):
        rel    = rs_file.relative_to(KERNEL_SRC).as_posix()
        actual = count_unsafe(rs_file)

        if actual == 0:
            continue

        if rel not in audit:
            failures.append(
                f"  FAIL  {rel}: {actual} unsafe line(s) not in audit - "
                f"add a // SAFETY: comment and an entry to docs/unsafe-audit.md"
            )
        elif actual > audit[rel]:
            failures.append(
                f"  FAIL  {rel}: unsafe count grew {audit[rel]} → {actual} - "
                f"add // SAFETY: comment(s) and update docs/unsafe-audit.md"
            )
        elif actual < audit[rel]:
            infos.append(
                f"  INFO  {rel}: unsafe count shrank {audit[rel]} → {actual} "
                f"(update audit to lock in the reduction)"
            )

    # §18.2: NO userspace service may contain `unsafe`. probe (the adversarial/fuzz/chaos test harness)
    # was the one violator; its raw-syscall fuzzing + deliberate ring-3 faults moved to the SDK's audited
    # `adversarial` module (§18.1), so probe is now unsafe-free. This scan enforces that it stays gone AND
    # that no other service regresses - a service that needs `unsafe` needs the kernel or a safe SDK
    # wrapper instead (§18.2). This is the blind spot the userspace audit (M8) found: the check used to
    # scan only kernel/src.
    for rs_file in sorted(SERVICES.rglob("*.rs")):
        n = count_unsafe(rs_file)
        if n > 0:
            rel = rs_file.relative_to(REPO_ROOT).as_posix()
            failures.append(
                f"  FAIL  {rel}: {n} unsafe line(s) - §18.2 forbids `unsafe` in a userspace service; "
                f"move it behind a safe SDK wrapper (§18.1, e.g. sdk `adversarial`/`mmio`/`dma`)"
            )

    if infos:
        print("Unsafe audit - reductions detected (update audit to capture them):")
        for msg in infos:
            print(msg)
        print()

    if failures:
        print("Unsafe audit - FAILURES:")
        for msg in failures:
            print(msg)
        print()
        print(
            f"{len(failures)} violation(s). "
            "See docs/unsafe-audit.md and §18 of CLAUDE.md for the policy."
        )
        return 1

    total = sum(audit.values())
    print(
        f"Unsafe audit passed - {len(audit)} audited files, "
        f"{total} total unsafe lines, no unaccounted additions."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
