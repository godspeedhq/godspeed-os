# v2 - Static-Analysis + Unsafe-Audit Cleanup

**Status:** ✅ Complete - boot-verified on hardware (2026-05-31)
**Branch:** `verify/static-analysis-unsafe-audit` → merged to `main`
**Scope:** local static-analysis pass over the kernel after the AMD GX-420GI
ring-3 / TSC-Deadline-APIC / COM1 / shell work; no CI minutes consumed.

---

## Result

| Area | Result |
|------|--------|
| Policy violation | **Fixed** - `unsafe` removed from `ipc/` (a forbidden layer, §18.1); the `.bss` zeroed-static moved to `SpinLock::ZEROED` in `smp/spinlock.rs` (permitted). `ipc/` is unsafe-free again. |
| Safety / correctness lints | **0** - cleared 11 unnecessary `unsafe` blocks, 11 `static mut` references (→ `addr_of!`/`addr_of_mut!`), 14 fn-item→integer casts (→ via `*const ()`), 6 no-op `mem::forget` on `Copy` `Frame`. |
| Cruft removed | orphaned `page_fault_handler` (live handler is `boot::pf_handler` on IDT[14]) + the orphaned `INTERRUPTED_*` diagnostic statics. |
| Unsafe audit | **passes clean** - 302 lines across 23 files; `task/scheduler.rs` back under its grandfathered floor (37 → 36). |
| Kernel warnings | **104 → 57**. The remaining 57 are intentional **unwired architecture** (the §22 invariant assertions, capability/IPC API, ring-0 task plumbing) kept deliberately - not cruft. |
| Hardware (HP T630) | **boots clean** - 4 cores ready, cross-core ping↔pong ran continuously to **83,043 messages**, zero `#PF` / panic. The changes touched boot-critical paths (IDT setup, frame allocator, BSP stack switch, context-switch trampolines), so the boot test is the decisive verification. |
| Miri (kernel `lib`) | the lone failure was proptest's regression-file I/O hitting miri's filesystem sandbox - passes with `-Zmiri-disable-isolation`; covers pure-logic modules untouched by this change. |

---

## What was decided, not just done

- **`static mut` references** were rewritten with `addr_of!`/`addr_of_mut!` -
  behaviour-preserving (the same pointer, no intermediate `&mut` to the static),
  not a conversion to atomics/locks. Grandfathered files (`main.rs`, `task/mod.rs`)
  kept their exact unsafe line counts.
- **Dead code was *not* blanket-removed.** Most of it is spec-mandated mechanism
  the minimal default-feature boot simply doesn't call (per §26, deleting it would
  be architectural erosion). Only genuine orphans were removed; the rest stay
  visible as warnings rather than be silently deleted or blanket-suppressed.
- **The fn-item→integer casts** go through `*const ()` first, which yields the
  same entry-point address without tripping `fn_to_numeric_cast`.

---

## Follow-up worth doing (not in this pass)

The §22 invariant assertions (`assert_cap_validated`, `assert_tcb_alive`,
`assert_cap_table_consistent`, `assert_no_mid_execution_migration`) are dead
**because they are never called**. Wiring them into the paths they guard would
strengthen the kernel per §26 and clear a chunk of the remaining 57 warnings
*honestly* (by use) rather than by suppression.

---

## Tooling state

- Local: `clippy`, `cargo-geiger`, `cargo-miri` available; `cargo-audit` not installed.
- `scripts/unsafe_check.py` reconciled against `docs/unsafe-audit.md` (passes).
- No GitHub Actions workflows were triggered (CI minutes preserved).
