# Post-v1 Item 2 - Unsafe Audit CI Check

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

## What was found (initial audit)

> **Note on counting:** The CI script counts *lines* containing the `unsafe`
> keyword (excluding comment-only lines). This is not the same as counting
> unsafe blocks. A single `unsafe { }` expression = 1 line. An `unsafe fn`
> declaration = 1 line. A nested `unsafe { unsafe { } }` = 2 lines. The line
> count is used because it is stable and grep-reproducible; the block count is
> documented separately below.

| Metric | Initial | After reduction |
|---|---|---|
| Non-comment lines with `unsafe` keyword | 319 | 267 |
| Total files with `unsafe` | 26 | 23 |
| Lines in permitted layers (§18.1) | 206 | 215 |
| - Files in permitted layers | 16 | 17 |
| Lines outside permitted layers (grandfathered) | 113 | 52 |
| - Files outside permitted layers | 10 | 6 |

See [Grandfathered reduction](#grandfathered-reduction) below for the full
account of what changed and why.

### Permitted layers (current, 215 keyword lines)

| File | Count |
|---|---|
| arch/x86_64/ap_boot.rs | 3 |
| arch/x86_64/boot.rs | 60 |
| arch/x86_64/context_switch.rs | 11 |
| arch/x86_64/interrupts.rs | 8 |
| arch/x86_64/mod.rs | 21 |
| arch/x86_64/page_tables.rs | 25 |
| arch/x86_64/syscall_entry.rs | 16 |
| capability/table.rs | 7 |
| memory/allocator.rs | 28 |
| memory/frame.rs | 1 |
| memory/mod.rs | 1 |
| memory/page.rs | 1 |
| smp/core.rs | 6 |
| smp/ipi.rs | 21 |
| smp/mod.rs | 1 |
| smp/placement.rs | 1 |
| smp/spinlock.rs | 4 |
| **Total** | **215** |

### Grandfathered (current, 52 keyword lines)

| File | Count | Why it remains |
|---|---|---|
| task/scheduler.rs | 36 | Per-core arrays, ring3 context switch |
| task/mod.rs | 7 | kstack pointer arithmetic + spawn path arch calls |
| loader.rs | 4 | Segment copy (`write_bytes` + `copy_nonoverlapping`) |
| syscall/dispatch.rs | 2 | `syscall_handler` entry point + `map_in_active_tables` |
| main.rs | 2 | BSP stack switch + boot_info deref |
| interrupt/route.rs | 1 | `pub unsafe fn deliver` - IDT calling convention |

Grandfathered counts are frozen. They may decrease (cleanup welcome) but
cannot increase. Any increase to a grandfathered file also requires a policy
amendment to CLAUDE.md §18 before CI will accept it.

### Totals at initial audit

| Metric | Count |
|---|---|
| `unsafe { }` blocks | 216 |
| `unsafe fn` declarations | 75 |
| **Total distinct unsafe constructs** | **291** |
| Keyword lines counted by CI | 319 |
| Files with unsafe | 26 |
| - in permitted layers | 16 |
| - grandfathered (outside policy) | 10 |

---

## SAFETY comment coverage

Every `unsafe` block and `unsafe fn` declaration in the kernel now has a
`// SAFETY:` comment on the line immediately above it. Back-fill completed
across: `arch/x86_64/boot.rs`, `arch/x86_64/mod.rs`,
`arch/x86_64/syscall_entry.rs`, `ipc/routing.rs`, `memory/allocator.rs`,
`syscall/dispatch.rs` (all 13 `handle_*` fns), and `task/scheduler.rs`.
Files that were already complete: `capability/table.rs`, `smp/core.rs`,
`smp/mod.rs`, `smp/placement.rs`, `log.rs`, `ipc/names.rs`, `control.rs`,
`memory/frame.rs`, `memory/page.rs`, `interrupt/route.rs`, `task/mod.rs`.

---

## Grandfathered reduction

Commit `53a7e79` reduced grandfathered unsafe from 113 to 70 lines (−38%)
across three groups of related changes. The unsafe itself was not removed from
the kernel - it was moved to where the policy says it belongs: the permitted
arch/smp layers.

### Group 1 - `static mut` + manual spinlock → `SpinLock<T>`

**New file:** `smp/spinlock.rs` - `SpinLock<T>` / `SpinLockGuard<T>` RAII
type. The `unsafe impl Send/Sync` and the two `UnsafeCell::get()` dereferences
inside `Deref` / `DerefMut` live here in the permitted `smp/` layer. All call
sites throughout the kernel are unsafe-free.

Five files converted:

| File | Before | After | Mechanism |
|---|---|---|---|
| `ipc/routing.rs` | 10 | 0 | `TABLE: SpinLock<[RoutingEntry; MAX_ENDPOINTS]>`; `enqueue_locked`/`dequeue_locked`/`find_index` become safe fns taking `&mut [RoutingEntry]` |
| `ipc/names.rs` | 2 | 0 | `NAMES: SpinLock<[NameEntry; MAX_ENTRIES]>` |
| `log.rs` | 2 | 0 | `RING: SpinLock<RingBuffer>`; `with_lock` closure removed |
| `interrupt/route.rs` | 3 | 1 | `IRQ_TABLE: SpinLock<[Option<EndpointId>; 256]>`; `register()` is now unsafe-free; `deliver()` keeps `unsafe fn` (IDT context) |
| `control.rs` | 1 | 0 | `LINE_BUF`/`LINE_LEN` → `SpinLock<LineBuf>`; `try_lock()` replaces manual `compare_exchange` + unlock |

Net: −18 grandfathered lines. Five files removed from audit; one file reduced.
Permitted layer gains +4 for `smp/spinlock.rs`.

### Group 2 - Inline `asm!` in wrong layer → arch safe wrappers

`arch/x86_64/interrupts.rs` gained three new safe functions:

```rust
pub fn enable_interrupts()    // STI
pub fn disable_interrupts()   // CLI
pub fn wait_for_interrupt()   // STI; HLT (atomic enable + halt)
```

`task/scheduler.rs` had two standalone `unsafe { asm!(...) }` blocks (one for
`sti; hlt` in the idle path, one for `cli` in `yield_current`) that were the
only reason those blocks were unsafe. Both replaced by calls to the arch
wrappers: 38 → 36 lines.

Net: −2 grandfathered lines. Permitted layer gains +3 for the new wrappers.

### Group 3 - User-pointer unsafe in wrong layer → arch safe wrappers

`arch/x86_64/syscall_entry.rs` gained four new safe functions:

```rust
pub fn validate_user_ptr(ptr: u64, len: usize) -> bool
pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]>
pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool
pub fn read_cycle_counter() -> u64
```

`syscall/dispatch.rs` was substantially simplified:
- `fn validate_user_slice` removed (logic moved to arch layer)
- 13 `unsafe fn handle_*` → `fn` (no unsafe needed after wrappers absorb it)
- All `from_raw_parts` / `copy_nonoverlapping` / `_rdtsc` calls replaced
- Only 2 lines remain: `pub unsafe extern "C" fn syscall_handler` (ring-3
  boundary - must stay unsafe) and one `unsafe { map_in_active_tables(...) }`
  inside `handle_alloc_mem` (arch call, cannot be wrapped away)

Net: −24 grandfathered lines. Permitted layer gains +3 for the new wrappers.

### Group 4 - ELF header/program-header reads → `read_ehdr` / `read_phdr` helpers

`loader.rs` previously had 14 individual `addr_of!((*ptr).field).read_unaligned()`
calls scattered through `load()` - one per accessed field.  These were
consolidated into two private helpers:

```rust
fn read_ehdr(bytes: &[u8]) -> Elf64Ehdr { unsafe { (bytes.as_ptr() as *const Elf64Ehdr).read_unaligned() } }
fn read_phdr(bytes: &[u8], off: usize) -> Elf64Phdr { unsafe { (bytes.as_ptr().add(off) as *const Elf64Phdr).read_unaligned() } }
```

Each helper copies the entire packed struct into a local value in one
`read_unaligned` call.  All field accesses in `load()` then go through safe
local copies - zero unsafe at the call site.  The two segment-copy blocks
(`write_bytes` for BSS zeroing, `copy_nonoverlapping` for file data) remain
because they are inherently unsafe arch calls with bounds that cannot be
checked at compile time.

`com2_init` in `arch/x86_64/mod.rs` was also made a safe function (inner
`outb` calls stay unsafe in the permitted arch layer); the call in `main.rs`
is now unsafe-free.

Net: −12 grandfathered lines (loader 16 → 4, main 3 → 2) and −1 permitted line
(mod.rs 22 → 21 after `unsafe fn com2_init` became `fn com2_init`).

### Group 5 - kstack magic-word tracking → `SpinLock<[bool; TASK_KSTACK_MAX]>`

`task/mod.rs` previously tracked liveness by writing a magic constant
(`0xCA11_CA11`) to the first 4 bytes of each 64 KiB stack slot via
`read_volatile` / `write_volatile`, plus a manual `AtomicBool` spinlock.
This was replaced by:

```rust
static KSTACK_USED: SpinLock<[bool; TASK_KSTACK_MAX]> =
    SpinLock::new([false; TASK_KSTACK_MAX]);
```

The `kstack_marker()` helper (`unsafe fn`), three volatile reads/writes, and
the manual `kstack_lock` / `kstack_unlock` functions were all removed.  The
two remaining unsafe lines in the kstack functions are unavoidable pointer
arithmetic: one `as_mut_ptr().add(...)` to locate the top of an allocated slot,
and one `as_ptr() as u64` to compute the base address needed by `free_kstack`
to reverse the slot index from a stack-top pointer.

Net: −5 grandfathered lines (task/mod.rs 12 → 7).

### What remains and why

| File | Count | Reason not reducible |
|---|---|---|
| `task/scheduler.rs` | 36 | Per-core `static mut` arrays indexed by slot/core_id; large refactor needed to replace with `SpinLock` or per-core ownership type |
| `task/mod.rs` | 7 | 2 kstack pointer-arithmetic lines; 5 spawn-path arch calls (`write_bytes`, `task_cap_init_empty`, ctx-page cast, `TaskContext::new_user`, `commit_task`) |
| `loader.rs` | 4 | `write_bytes` (BSS zeroing) + `copy_nonoverlapping` (segment copy) - inherently unsafe HHDM pointer arithmetic |
| `syscall/dispatch.rs` | 2 | `syscall_handler` (ring-3 entry point, must be `unsafe extern "C"`) + one `map_in_active_tables` call |
| `main.rs` | 2 | BSP stack switch ASM (unavoidable), `boot_info_ptr` deref (Limine contract) |
| `interrupt/route.rs` | 1 | `pub unsafe fn deliver` - called from IDT with IF=0; the `unsafe` communicates the calling-convention constraint |

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
4. All three changes must land in the same commit - CI will catch a mismatch.

For out-of-policy files (grandfathered list above): adding `unsafe` requires
both steps 1–4 AND a CLAUDE.md §18 amendment with a written rationale.

---

## Implementation checklist

- ✅ `scripts/unsafe_check.py` - count-based audit script
- ✅ `docs/unsafe-audit.md` - full inventory (23 files, 267 lines), SAFETY
  arguments for every file, grandfathered list with rationale
- ✅ `.github/workflows/build.yml` - `Unsafe audit check` step added before
  unit tests; runs on every push and PR
- ✅ `build/tests/post_v1/2_UNSAFE_AUDIT/` - output directory
- ✅ `smp/spinlock.rs` - `SpinLock<T>` eliminates `static mut` in 5 files
- ✅ `arch/x86_64/interrupts.rs` - `disable_interrupts()` / `wait_for_interrupt()` wrappers
- ✅ `arch/x86_64/syscall_entry.rs` - `read_user_bytes()` / `write_user_bytes()` / `read_cycle_counter()` wrappers
- ✅ Grandfathered reduced: 113 → 52 lines, 10 → 6 files
- ✅ `loader.rs` - `read_ehdr`/`read_phdr` helpers consolidate 14 scattered `read_unaligned` calls into 2 (16 → 4 lines)
- ✅ `main.rs` - `com2_init` made safe; one unsafe call site removed (3 → 2 lines)
- ✅ `task/mod.rs` - kstack magic-word volatile tracking replaced by `SpinLock<[bool; TASK_KSTACK_MAX]>` (12 → 7 lines)
