#!/usr/bin/env python3
"""Enforce the arch boundary: NO arch-specific code in the kernel's arch-neutral layers (aarch64 Phase 0).

The whole kernel reaches hardware through ONE seam, `crate::arch::imp` (`kernel/src/arch/mod.rs`), which
`#[cfg(target_arch)]`-selects the implementation module (`arch/x86_64/`, later `arch/aarch64/`,
`arch/riscv64/`, ...). For a new architecture to be BOUNDED - "implement `arch/<new>/` to the same
surface, touch zero neutral files" - two invariants must hold in every kernel file OUTSIDE `arch/`:

  1. No inline assembly (`asm!` / `naked_asm!`). Arch-specific instructions live only in `arch/`, reached
     through `arch::imp` primitives (e.g. `read_page_table_base`, `invalidate_tlb_page`, `local_irq_save`).
  2. No reference to a NAMED arch module (`arch::x86_64::`, `arch::aarch64::`, ...). Neutral code names
     only `arch::imp::`; naming a specific arch is exactly the leak that makes a port unbounded.

This is the arch-boundary counterpart to `unsafe_check.py` (the unsafe boundary) and `contract_check.py`
(the contract<->kernel reconcile): a boundary survives only if it is mechanically enforced (CLAUDE.md
§26 - the architecture survives only if the discipline survives). A violation here means a future
RISC-V/AArch64 port would be forced to edit a neutral file; fix it by adding an `arch::imp` primitive.

Exit: 0 if the neutral layers are arch-clean, 1 otherwise.
"""

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent
KERNEL_SRC = REPO_ROOT / "kernel" / "src"
ARCH_DIR = KERNEL_SRC / "arch"

# Any named arch module. Neutral code must use `arch::imp::` instead. Extend as arches are added.
NAMED_ARCH = re.compile(r"\barch::(x86_64|aarch64|riscv64|riscv32|arm)::")
INLINE_ASM = re.compile(r"\b(?:core::arch::)?(?:naked_)?asm!")


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

    if violations:
        print("Arch-boundary check - FAILURES (arch-specific code leaked into a neutral kernel layer):")
        for v in violations:
            print(v)
        print()
        print(f"{len(violations)} violation(s). The neutral layers must reach hardware only through the "
              "`arch::imp` seam (docs/aarch64.md); add an `arch::imp` primitive rather than inlining asm "
              "or naming a specific arch. This keeps a future RISC-V/AArch64 port BOUNDED.")
        return 1

    print("Arch-boundary check passed - no inline asm and no named-arch references outside kernel/src/arch/. "
          "The neutral layers reach hardware only through the `arch::imp` seam; a new arch is a drop-in.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
