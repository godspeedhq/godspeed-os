#!/usr/bin/env python3
"""Enforce the arch boundary: NO arch-specific code in the kernel's arch-neutral layers.

The whole kernel reaches hardware through ONE seam, `crate::arch::imp` (`kernel/src/arch/mod.rs`), which
`#[cfg(target_arch)]`-selects the implementation module - one directory per ISA under `arch/`. For an
architecture to be BOUNDED - "implement `arch/<new>/` to the same surface, touch zero neutral files" -
two invariants must hold in every kernel file OUTSIDE `arch/`:

  1. No inline assembly (`asm!` / `naked_asm!`). Arch-specific instructions live only in `arch/`, reached
     through `arch::imp` primitives (e.g. `read_page_table_base`, `invalidate_tlb_page`, `local_irq_save`).
  2. No reference to a NAMED arch module (`arch::x86_64::`, `arch::aarch64::`, ...). Neutral code names
     only `arch::imp::`; naming a specific arch is exactly the leak that makes a port unbounded.
  3. No `core::sync::atomic::AtomicU64` / `AtomicI64`. RV32 has no 64-bit atomic so the `core` type
     does not exist there; `portable_atomic` supplies it at zero cost everywhere else. This is the
     WORD-SIZE half of portability, and `arch/CLAUDE.md` calls it one of the two rules the boundary
     rests on - while nothing enforced it until a 32-bit port hit it as a compile error.

This is the arch-boundary counterpart to `unsafe_check.py` (the unsafe boundary) and `contract_check.py`
(the contract<->kernel reconcile): a boundary survives only if it is mechanically enforced (CLAUDE.md
§26 - the architecture survives only if the discipline survives). A violation here means the NEXT port
would be forced to edit a neutral file; fix it by adding an `arch::imp` primitive.

THE ARCH LIST IS DERIVED, NOT RESTATED. It used to be a hand-written alternation with the comment
"Extend the arch list as arches are added" - and that manual step was missed: `loongarch64` and `s390x`
have directories under `arch/` and were absent from the pattern, so `arch::loongarch64::` in a neutral
file would have passed while this script printed an unqualified all-clear. A check that cannot see two
of the seven arches it guards is worse than no check, because it is believed. The list now comes from
the directory listing, which is the thing that actually defines what an arch IS here, so the next port
is covered the moment its directory exists and nobody has to remember this file.

Exit: 0 if the neutral layers are arch-clean, 1 otherwise.
"""

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent
KERNEL_SRC = REPO_ROOT / "kernel" / "src"
ARCH_DIR = KERNEL_SRC / "arch"

# `core::arch` submodules that are NOT a directory under `arch/`. `x86` is the 32-bit intrinsic module
# that accompanies `x86_64` (`core::arch::x86::__cpuid`); we build no 32-bit x86 kernel, so no directory
# names it, but a neutral file could still reach for it.
_EXTRA_INTRINSIC_ARCHES = ["x86"]


def _arch_names() -> list[str]:
    """Every ISA name this repository knows, derived from the directories under `kernel/src/arch/`.

    Sorted LONGEST FIRST so the alternation cannot match a prefix: with `x86|x86_64`, the regex engine
    takes `x86` and reports the wrong arch in the violation message. Longest-first makes `x86_64` win.
    """
    dirs = [p.name for p in ARCH_DIR.iterdir() if p.is_dir() and not p.name.startswith(".")]
    if not dirs:
        # LOUD, never a silent pass (invariant 12). An empty alternation would build the regex
        # `arch::()::` - which matches nothing, so every violation would slip through while this
        # script still printed "passed". A check that cannot find its own subject must say so.
        raise SystemExit(f"arch_boundary_check: no arch directories under {ARCH_DIR} - refusing to "
                         f"report a pass against an empty arch list")
    return sorted(set(dirs + _EXTRA_INTRINSIC_ARCHES), key=lambda n: (-len(n), n))


_ARCH_NAMES = _arch_names()
_ARCHES = "|".join(_ARCH_NAMES)
NAMED_ARCH = re.compile(rf"\barch::({_ARCHES})::")
CORE_ARCH_INTRINSIC = re.compile(rf"\bcore::arch::({_ARCHES})::")  # e.g. core::arch::x86_64::__cpuid
INLINE_ASM = re.compile(r"\b(?:core::arch::)?(?:naked_)?asm!")

# WORD SIZE, not instruction set - the second of the two rules `kernel/src/arch/CLAUDE.md` says the
# boundary is built on, and until 2026-09-13 the only one of them nothing enforced.
#
# 32-bit RISC-V (RV32A) has no 64-bit atomic, so `core::sync::atomic::AtomicU64` DOES NOT EXIST there.
# `portable_atomic::AtomicU64` is the native, zero-cost type on every ISA that has one and a small
# lock-based shim only on RV32, which is what makes this kernel word-size portable as well as
# ISA-portable.
#
# EIGHT neutral-kernel sites were violating this when the check was written, in `interrupt/route.rs`,
# `ipc/routing.rs`, `syscall/dispatch.rs` and `task/scheduler.rs`. The rule had been documented for
# months; nothing read it. They surfaced only when a 32-bit port was actually attempted, as part of
# its 46 compile errors - which is the worst way to find a rule you already wrote down, because the
# porter has to work out that the fault is OURS and not theirs.
#
# `AtomicI64` is included though nothing uses it today: the hardware limitation is about WIDTH, so the
# signed type would arrive with the identical bug and a checker that waited for it would be pedantry
# rather than enforcement.
CORE_ATOMIC_64 = re.compile(r"\bcore::sync::atomic::(AtomicU64|AtomicI64)\b")


def strip_comments(text: str) -> str:
    """Drop // line comments so a doc-comment mentioning `asm!` or `arch::x86_64::` never trips the check.
    (Block comments and string literals are rare enough in this codebase that a line-comment strip
    suffices; a false positive is a loud, easily-silenced doc rewrite, never a silent miss.)"""
    return "\n".join(line.split("//", 1)[0] for line in text.splitlines())


def main() -> int:
    violations: list[str] = []
    for path in sorted(KERNEL_SRC.rglob("*.rs")):
        # The arch implementation dir is where arch-specific code BELONGS - skip it.
        if ARCH_DIR in path.parents:
            continue
        rel = path.relative_to(REPO_ROOT).as_posix()
        code = strip_comments(path.read_text(encoding="utf-8"))
        for i, line in enumerate(code.splitlines(), 1):
            if INLINE_ASM.search(line):
                violations.append(f"  {rel}:{i}: inline asm in a neutral file - move it behind an "
                                  f"`arch::imp` primitive in kernel/src/arch/")
            m = NAMED_ARCH.search(line)
            if m:
                violations.append(f"  {rel}:{i}: names `arch::{m.group(1)}::` directly - use "
                                  f"`arch::imp::` (the seam) so a new arch stays a drop-in")
            a64 = CORE_ATOMIC_64.search(line)
            if a64:
                violations.append(f"  {rel}:{i}: uses `core::sync::atomic::{a64.group(1)}` in a "
                                  f"neutral file - use `portable_atomic::{a64.group(1)}`. RV32 has "
                                  f"no 64-bit atomic, so the `core` type does not exist there and "
                                  f"this file cannot compile for a 32-bit port.")
            ci = CORE_ARCH_INTRINSIC.search(line)
            if ci:
                violations.append(f"  {rel}:{i}: uses `core::arch::{ci.group(1)}::` intrinsics in a "
                                  f"neutral file - wrap it in an `arch::imp` primitive in kernel/src/arch/")

    if violations:
        print("Arch-boundary check - FAILURES (arch-specific code leaked into a neutral kernel layer):")
        for v in violations:
            print(v)
        print()
        print(f"{len(violations)} violation(s). The neutral layers must reach hardware only through the "
              "`arch::imp` seam (docs/aarch64.md); add an `arch::imp` primitive rather than inlining asm "
              "or naming a specific arch. This keeps the NEXT port BOUNDED.")
        return 1

    print(f"Arch-boundary check passed - no inline asm and no named-arch references outside "
          f"kernel/src/arch/, across all {len(_ARCH_NAMES)} arch names ({', '.join(_ARCH_NAMES)}). "
          f"The neutral layers reach hardware only through the `arch::imp` seam; a new arch is a drop-in.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
