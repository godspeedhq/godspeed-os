#!/usr/bin/env python3
"""Verify every unsafe usage in kernel/src/ is accounted for in audits/unsafe-audit.md.

Rules enforced:
  - A file not in the audit that gains unsafe lines          -> FAIL
  - A file whose unsafe count exceeds its audited baseline   -> FAIL
  - A file whose unsafe count dropped below its baseline     -> INFO
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
SDK         = REPO_ROOT / "sdk"

# 18.1 names the SDK files where `unsafe` is PERMITTED: the syscall ABI, the MMIO/DMA accessors a
# userspace driver cannot do without, and the adversarial test module. Everything else under `sdk/`
# is forbidden by 18.2 exactly as a service is.
SDK_PERMITTED = {"syscall.rs", "mmio.rs", "dma.rs", "adversarial.rs"}

# ...and the files that hold `unsafe` anyway, frozen at their counts (18.5's grandfathering, applied
# to the SDK). These are NOT 90 separate defects: 86 of the 90 are `unsafe { raw_syscall(..) }` call
# sites. `raw_syscall` is an `unsafe fn` because it issues the trap instruction, so every caller must
# open a block - and these two files ARE the wrapper layer that exists to keep services unsafe-free.
# The isolation 18.1 describes worked (services/ is at ZERO) and stopped one layer short of itself.
#
# Freezing them makes the debt visible and bounded. The real fix is a SAFE `raw_syscall` wrapper -
# the kernel validates every user pointer, so passing integers to a validating callee is sound - which
# would collapse ~86 of these to nothing. That is an SDK redesign on every service's call path, so it
# is recorded rather than done here.
SDK_GRANDFATHERED = {
    "sdk/rust/src/service_context.rs": 82,
    "sdk/rust/src/ipc.rs": 8,
}
AUDIT_FILE  = REPO_ROOT / "audits" / "unsafe-audit.md"

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


# Roots where 18.2 FORBIDS unsafe outright. Every crate under one of these must say so to the
# COMPILER, not merely avoid the word.
# `stdlib` is here for a REASON BEYOND 18.2, and it is the load-bearing one. The standard
# library exists to make functionality the OS already has pleasant and safe to consume; it
# must never manufacture functionality the OS lacks. `#![deny(unsafe_code)]` is what makes
# that STRUCTURAL: with no `unsafe` the crate cannot issue a syscall, so it is confined to
# the SDK's safe surface and can only re-serve what already exists. A stdlib that could
# reach the raw ABI could quietly grow a capability the system does not have.
#
# The attribute was already there; nothing checked it, so deleting it failed no gate.
DENY_ROOTS = ("services", "examples", "osdev", "stdlib")
DENY_ATTR = "#![deny(unsafe_code)]"


def deny_unsafe_crates() -> list:
    """Every crate under a forbidden root must carry `#![deny(unsafe_code)]`, and the only
    `#[allow(unsafe_code)]` in it must sit on the exported entry symbol.

    WHY THIS IS NOT REDUNDANT WITH THE GREP ABOVE. The scan looks for the TEXT `unsafe` in these
    trees and requires zero. That is a real check and it has held, but it is a grep: it cannot see
    `unsafe` produced by a macro expansion, and it reasons about characters rather than about what
    the compiler will accept. `#![deny(unsafe_code)]` makes rustc refuse the crate outright, which is
    the same rule enforced by the thing that actually knows.

    `deny` and not `forbid`, for exactly one reason: a `#[no_mangle]` declaration is itself covered by
    the `unsafe_code` lint - an exported symbol can collide, which is a soundness hole - and every
    service needs `#[no_mangle] service_main` because `build.rs` links with `--entry=service_main`.
    `forbid` cannot be relaxed even for that, so the crates would not compile. `deny` plus ONE
    targeted `#[allow]` is the strongest form available, and this function is what stops that
    exception being used anywhere else.
    """
    problems = []
    for root_name in DENY_ROOTS:
        root = REPO_ROOT / root_name
        if not root.is_dir():
            continue
        crates = ([root] if (root / "Cargo.toml").exists()
                  else [d for d in sorted(root.iterdir())
                        if d.is_dir() and (d / "Cargo.toml").exists()])
        for crate in crates:
            for stem in ("main.rs", "lib.rs"):
                f = crate / "src" / stem
                if not f.exists():
                    continue
                text = f.read_text(encoding="utf-8", errors="replace")
                rel = f.relative_to(REPO_ROOT).as_posix()
                # STRIP COMMENTS FIRST. This read `DENY_ATTR not in text` against the raw file,
                # so a crate whose doc comment merely MENTIONS the attribute passed whether or not
                # it carried one - and `stdlib/rust/src/lib.rs` is exactly that shape (its module
                # docs explain the attribute a few lines above declaring it). Deleting the real
                # attribute left this green. Same trap the `#[allow]` scan below already avoids for
                # the same reason; the lesson had been learned for one half of this function only.
                code_only = "\n".join(ln.split("//", 1)[0] for ln in text.split("\n"))
                if DENY_ATTR not in code_only:
                    problems.append(f"{rel}: missing {DENY_ATTR} (18.2 forbids unsafe in this tree)")
                    continue
                # The only sanctioned escape is the entry symbol. Anything else is the exception
                # being used as a door.
                # COMMENTS STRIPPED FIRST. Without this the crate note directly above - which
                # EXPLAINS the one sanctioned `#[allow(unsafe_code)]` - was itself counted as one,
                # so every crate reported a violation on the very line describing the rule. Same
                # defect `shared_surface_check.py` had (prose counted as code) and the same fix.
                lines = [ln.split("//", 1)[0] for ln in text.split("\n")]
                for i, line in enumerate(lines):
                    if "#[allow(unsafe_code)]" not in line:
                        continue
                    nxt = lines[i + 1] if i + 1 < len(lines) else ""
                    if not nxt.startswith("#[no_mangle]"):
                        problems.append(
                            f"{rel}:{i + 1}: #[allow(unsafe_code)] that is NOT on the exported entry "
                            f"symbol. The one sanctioned exception is `#[no_mangle] service_main`; "
                            f"everything else must satisfy the deny.")
    return problems


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

    # An inventory row whose FILE NO LONGER EXISTS is a failure too.
    #
    # This loop walks files on disk, so a row for a deleted file was never visited and the check passed
    # while the inventory described a tree that was gone. Deleting `arch/aarch64/xhci.rs` left its
    # 42-line row behind for two audit rounds: the audit claimed 42 unsafe lines that no longer existed
    # anywhere, and this script reported success. §18.4 requires the audit to MATCH the source, and a
    # check that only ever looks one way cannot enforce that.
    #
    # Checked first, so the report leads with "the audit describes a file that is not here" rather than
    # burying it under the per-file results.
    for rel in sorted(audit):
        if not (KERNEL_SRC / rel).exists() and not (SERVICES / rel).exists():
            failures.append(
                f"  FAIL  {rel}: in audits/unsafe-audit.md but the file does not exist - "
                f"remove its inventory row (the audit must match the source, §18.4)"
            )

    for rs_file in sorted(KERNEL_SRC.rglob("*.rs")):
        rel    = rs_file.relative_to(KERNEL_SRC).as_posix()
        actual = count_unsafe(rs_file)

        if actual == 0:
            continue

        if rel not in audit:
            failures.append(
                f"  FAIL  {rel}: {actual} unsafe line(s) not in audit - "
                f"add a // SAFETY: comment and an entry to audits/unsafe-audit.md"
            )
        elif actual > audit[rel]:
            failures.append(
                f"  FAIL  {rel}: unsafe count grew {audit[rel]} -> {actual} - "
                f"add // SAFETY: comment(s) and update audits/unsafe-audit.md"
            )
        elif actual < audit[rel]:
            infos.append(
                f"  INFO  {rel}: unsafe count shrank {audit[rel]} -> {actual} "
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

    # 18.1/18.2 for the SDK. THIS WAS SCANNED BY NOTHING: the script defined two roots, kernel/src
    # and services, so `sdk/`'s ~125 unsafe lines were audited by no tool at all - while 18.4 says
    # "CI checks the file matches source" and the audit's own header implied the SDK's unsafe lived
    # only in the four permitted files. It does not (backlog/18).
    for rs_file in sorted(SDK.rglob("*.rs")):
        rel = rs_file.relative_to(REPO_ROOT).as_posix()
        if "target" in rel.split("/"):
            continue
        n = count_unsafe(rs_file)
        if n == 0 or rs_file.name in SDK_PERMITTED:
            continue
        frozen = SDK_GRANDFATHERED.get(rel)
        if frozen is None:
            failures.append(
                f"  FAIL  {rel}: {n} unsafe line(s) - 18.2 forbids `unsafe` outside the SDK's "
                f"audited layer ({', '.join(sorted(SDK_PERMITTED))}); put it there behind a safe "
                f"wrapper, or record a floor in SDK_GRANDFATHERED with a rationale"
            )
        elif n > frozen:
            failures.append(
                f"  FAIL  {rel}: {n} unsafe line(s), frozen at {frozen} - a grandfathered SDK floor "
                f"may DECREASE freely and may increase only by an amendment (18.5)"
            )
        elif n < frozen:
            infos.append(f"  INFO  {rel}: unsafe count shrank {frozen} -> {n} "
                         f"(lower SDK_GRANDFATHERED to lock in the reduction)")

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
            "See audits/unsafe-audit.md and §18 of CLAUDE.md for the policy."
        )
        return 1

    deny_problems = deny_unsafe_crates()
    if deny_problems:
        print("CRATES THAT MUST REFUSE UNSAFE AT COMPILE TIME:")
        print()
        for pr in deny_problems:
            print(f"  {pr}")
        print()
        print("18.2 forbids `unsafe` in services, examples and osdev. Grepping for the word is not")
        print("the same as the compiler refusing it, so every crate in those trees carries")
        print(f"`{DENY_ATTR}` and rustc enforces the rule.")
        return 1

    total = sum(audit.values())
    print(
        f"Unsafe audit passed - {len(audit)} audited files, "
        f"{total} total unsafe lines, no unaccounted additions."
    )
    print(
        f"Compile-time deny: every crate under {'/'.join(DENY_ROOTS)} carries {DENY_ATTR}, "
        f"with the only #[allow] on the exported entry symbol."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
