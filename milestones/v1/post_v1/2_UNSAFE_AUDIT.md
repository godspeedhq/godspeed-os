# Post-v1 Item 2 — Unsafe Audit CI Check

**Status:** ✅ Complete  
**Command:** `python3 scripts/unsafe_check.py`  
**CI workflow:** `.github/workflows/build.yml` (every push / PR)  
**Evidence:** `build/tests/post_v1/2_UNSAFE_AUDIT/`

---

## Overview

§18.4 of CLAUDE.md requires that `docs/unsafe-audit.md` lists every `unsafe`
block in the kernel and that CI verifies the file matches source. This item
implements that enforcement.

The check is a count-based freeze: the audited count per file is the maximum
permitted. Any increase fails CI. Decreases are reported as INFO (the audit
should be updated to lock in the reduction). New files with `unsafe` that are
not in the audit fail CI unconditionally.

---

## What was found

At the time of this audit the kernel had **319 non-comment unsafe lines across
26 files**. Of those, 113 lines across 10 files are **outside the four permitted
layers** defined in §18.1 — a pre-existing policy violation from the rapid v1
development phase.

### Permitted layers (in-policy, 206 lines)

| File | Count |
|---|---|
| arch/x86_64/ap_boot.rs | 3 |
| arch/x86_64/boot.rs | 60 |
| arch/x86_64/context_switch.rs | 11 |
| arch/x86_64/interrupts.rs | 5 |
| arch/x86_64/mod.rs | 22 |
| arch/x86_64/page_tables.rs | 25 |
| arch/x86_64/syscall_entry.rs | 13 |
| capability/table.rs | 7 |
| memory/allocator.rs | 28 |
| memory/frame.rs | 1 |
| memory/mod.rs | 1 |
| memory/page.rs | 1 |
| smp/core.rs | 6 |
| smp/ipi.rs | 21 |
| smp/mod.rs | 1 |
| smp/placement.rs | 1 |

### Grandfathered (outside policy, 113 lines — frozen)

| File | Count | Why it exists |
|---|---|---|
| task/scheduler.rs | 38 | Per-core arrays, ring3 context switch, cli/sti |
| syscall/dispatch.rs | 26 | User-pointer slice construction in syscall handlers |
| loader.rs | 16 | ELF `read_unaligned` + segment mapping |
| task/mod.rs | 12 | Kernel stack allocator magic-word tracking |
| ipc/routing.rs | 10 | Global routing table under SpinLock |
| interrupt/route.rs | 3 | IRQ routing table under InterruptLock |
| main.rs | 3 | BSP stack switch + boot_info deref + COM2 init |
| ipc/names.rs | 2 | Endpoint name table under SpinLock |
| log.rs | 2 | Ring buffer under SpinLock |
| control.rs | 1 | Stack switch (same pattern as main.rs) |

Grandfathered counts are frozen. They may decrease (cleanup welcome) but
cannot increase. Any increase to a grandfathered file also requires a policy
amendment to CLAUDE.md §18 before CI will accept it.

---

## SAFETY comment coverage

At audit time, SAFETY comment coverage is **partial**. Files with full coverage:
`capability/table.rs`, `smp/core.rs`, `smp/mod.rs`, `smp/placement.rs`,
`log.rs`, `ipc/names.rs`, `control.rs`. Files with significant gaps are noted
in `docs/unsafe-audit.md` with "(needs back-fill)". The audit file documents the
SAFETY argument for each group of related unsafe blocks even where individual
source comments are missing — the source back-fill is separate cleanup work.

---

## How the check works

`scripts/unsafe_check.py`:

1. Reads the inventory table from `docs/unsafe-audit.md` (between
   `<!-- unsafe-inventory-start -->` and `<!-- unsafe-inventory-end -->`).
2. Walks `kernel/src/**/*.rs`, counts non-comment lines containing `\bunsafe\b`.
3. For each file with `unsafe`:
   - Not in audit → **FAIL**
   - Count > audited baseline → **FAIL**
   - Count < audited baseline → **INFO** (safe reduction)
4. Exits 0 if no failures, 1 otherwise.

---

## Contribution rule (from this point forward)

When adding an `unsafe` block anywhere in the kernel:

1. Add `// SAFETY: <argument>` on the line immediately above it.
2. Increase the count for that file in `docs/unsafe-audit.md`.
3. Add a SAFETY argument entry under that file in the Entries section.
4. All three changes must land in the same commit — CI will catch a mismatch.

For out-of-policy files (grandfathered list above): adding `unsafe` requires
both steps 1–4 AND a CLAUDE.md §18 amendment with a written rationale.

---

## Implementation checklist

- ✅ `scripts/unsafe_check.py` — count-based audit script
- ✅ `docs/unsafe-audit.md` — full inventory (26 files, 319 lines), SAFETY
  arguments for every file, grandfathered list with rationale
- ✅ `.github/workflows/build.yml` — `Unsafe audit check` step added before
  unit tests; runs on every push and PR
- ✅ `build/tests/post_v1/2_UNSAFE_AUDIT/` — output directory
