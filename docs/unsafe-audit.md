# Unsafe Audit (§18.4)

`scripts/unsafe_check.py` runs on every CI push. It counts non-comment lines
containing the `unsafe` keyword per file and compares to the baseline table below.

**A PR that increases any file's count, or adds unsafe to a new file, fails CI
unless this file is updated in the same commit with a written SAFETY argument.**

---

## Policy (§18)

`unsafe` is permitted only in:

| Permitted layer | Path |
|---|---|
| Architecture | `kernel/src/arch/` |
| Memory | `kernel/src/memory/` |
| Capability table | `kernel/src/capability/` |
| SMP | `kernel/src/smp/` |

**All other locations are outside policy.** The files marked `grandfathered` in
the table below contain unsafe that pre-dates this audit. Their counts are frozen:
they may decrease (fix welcome) but may not increase. New unsafe in those files
requires a policy amendment in `CLAUDE.md §18` before CI will accept it.

When you add an `unsafe` block anywhere:

1. Add `// SAFETY: <argument>` on the line immediately above it in the source.
2. Increase the count for that file in the inventory table below.
3. Add a SAFETY argument entry under that file in the **Entries** section.
4. Both changes must land in the same commit; CI checks the count.

---

## Inventory

Counts are non-comment lines containing the `unsafe` keyword.
CI script: `scripts/unsafe_check.py` — parses the table between the markers.

<!-- unsafe-inventory-start -->
| File (kernel/src/) | Count | Layer |
|---|---|---|
| arch/x86_64/ap_boot.rs | 3 | permitted |
| arch/x86_64/boot.rs | 60 | permitted |
| arch/x86_64/context_switch.rs | 11 | permitted |
| arch/x86_64/interrupts.rs | 8 | permitted |
| arch/x86_64/mod.rs | 21 | permitted |
| arch/x86_64/page_tables.rs | 25 | permitted |
| arch/x86_64/syscall_entry.rs | 16 | permitted |
| capability/table.rs | 7 | permitted |
| memory/allocator.rs | 28 | permitted |
| memory/frame.rs | 1 | permitted |
| memory/mod.rs | 1 | permitted |
| memory/page.rs | 1 | permitted |
| smp/core.rs | 6 | permitted |
| smp/ipi.rs | 21 | permitted |
| smp/mod.rs | 1 | permitted |
| smp/placement.rs | 1 | permitted |
| smp/spinlock.rs | 4 | permitted |
| interrupt/route.rs | 1 | grandfathered |
| loader.rs | 4 | grandfathered |
| main.rs | 2 | grandfathered |
| syscall/dispatch.rs | 2 | grandfathered |
| task/mod.rs | 7 | grandfathered |
| task/scheduler.rs | 45 | grandfathered |
<!-- unsafe-inventory-end -->

**Permitted total:** 215 lines across 17 files  
**Grandfathered total:** 61 lines across 6 files  
**Grand total:** 276 lines across 23 files

---

## Entries

Each entry documents WHY an unsafe block is sound. Entries are grouped by file.
Files with thorough existing `// SAFETY:` comments in source reference them here.
Files lacking source comments are noted with `(SAFETY comment missing in source)`.

New entries must be added in the same commit as the unsafe block they cover.

---

### arch/x86_64/ap_boot.rs

Unsafe in this file: AP trampoline entry, AP boot identity mapping, and calling
`ap_main` after the long-mode switch. All three are sound because the trampoline
runs before any Rust invariants apply; the stack is valid; identity mapping holds
for the trampoline duration and is torn down by the kernel immediately after.

---

### arch/x86_64/boot.rs

Largest unsafe surface in the kernel. Covers: BSP init (GDT/IDT/TSS per core),
APIC MMIO mapping and register writes, serial I/O port access, TSS RSP0 reads
and writes, paging init, and IPI delivery. All operations are sound because
they target fixed hardware addresses verified against the Limine memory map, or
operate on per-core structures indexed by a valid `core_id` bounded by
`MAX_CORES`. APIC MMIO is mapped once before any AP comes up.

---

### arch/x86_64/context_switch.rs

Stack construction for new kernel and user tasks. `new_kernel` and `new_user`
write a synthetic initial register frame to a freshly allocated kernel stack
pointer. Sound because the stack buffer is owned exclusively by the new task
and not yet visible to the scheduler.

---

### arch/x86_64/interrupts.rs

IRQ dispatch and CR2 read on page fault. Sound because the IRQ handler runs at
known IDT vector; CR2 is only read inside the page-fault handler where it is
valid.

Three additional `unsafe {}` blocks (count +3): `enable_interrupts` (STI),
`disable_interrupts` (CLI), and `wait_for_interrupt` (STI+HLT). All three are
ring-0 privileged instructions with no memory effects; the callers are
responsible for the context invariants (e.g., interrupts were disabled before
calling `wait_for_interrupt`). `// SAFETY:` comments present in source.

---

### arch/x86_64/mod.rs

Serial port init and COM2 init via `outb`/`inb`, `cli`/`hlt` in the halt loop,
and the `init` call chain into `boot.rs`. Sound because serial ports are
exclusively owned by the kernel at these call sites; `cli`/`hlt` is the correct
halt sequence; all callers are within the single-threaded BSP init path.

---

### arch/x86_64/page_tables.rs

HHDM offset reads and writes, PTE reads and writes via `read_volatile`/
`write_volatile`, `map_in_active_tables` (reads CR3, walks and modifies the
active page table), and `reclaim_user_frames` (walks a dead task's page table
after the TLB shootdown has completed). All are sound because: HHDM offset is
written once before any AP starts; PTE access goes through the HHDM which is
valid after `set_hhdm_offset`; `map_in_active_tables` holds the frame allocator
lock for the duration; `reclaim_user_frames` is called only after TLB shootdown
acknowledgment from all cores.

---

### arch/x86_64/syscall_entry.rs

Serial output helpers (`ser_putc`, `ser_puts`, `ser_hex64`) and per-core SYSCALL
MSR setup. Sound because serial helpers are guarded by the kernel's serial
spinlock; SYSCALL MSR setup runs once during per-core init before the core
enters the scheduler.

Three additional `unsafe {}` blocks (count +3): `read_user_bytes`
(`from_raw_parts`), `write_user_bytes` (`copy_nonoverlapping`), and
`read_cycle_counter` (`_rdtsc`). All three are sound because the pointer/length
pair is validated by `validate_user_ptr` before the unsafe call, ensuring the
range lies in user-space (below `USER_END`) and cannot overlap kernel memory;
`_rdtsc` is a read-only counter with no side effects. `// SAFETY:` comments
present in source.

---

### capability/table.rs

Access to `GLOBAL_RESOURCES` — a static `ResourceTable` protected by an
internal `SpinLock`. All seven unsafe calls go through the lock; the lock
ensures mutual exclusion across cores. `// SAFETY:` comments present in source.

---

### memory/allocator.rs

Frame allocator internals: bitmap manipulation, guard-page checks, allocator
init from the Limine memory map. Sound because the allocator is protected by a
`SpinLock`; bitmap indices are bounds-checked before access; guard-page ranges
are set once during init. `// SAFETY:` comments present in source for most
blocks; a small number need back-fill (see grandfathered note in §18).

---

### memory/frame.rs

`Frame::from_phys` — constructs a `Frame` from a raw physical address. Sound
because all callers are in the frame allocator or page-table walker, both of
which obtain addresses from the validated Limine memory map.
*(SAFETY comment missing in source — needs back-fill.)*

---

### memory/mod.rs

Calls `set_hhdm_offset` with the Limine-supplied HHDM offset during early init.
Sound because this runs exactly once, on the BSP, before any AP or task sees
virtual memory. `// SAFETY:` comment present in source.

---

### memory/page.rs

`Page::from_virt` — constructs a `Page` from a raw virtual address. Used only
by the page-table walker with addresses derived from the HHDM. Sound for the
same reason as `Frame::from_phys`.

---

### smp/core.rs

Per-core ready-flag manipulation via static arrays indexed by `core_id`.
`core_id` is bounded by `MAX_CORES` at all call sites. `// SAFETY:` comments
present in source.

---

### smp/ipi.rs

APIC IPI delivery: reads `APIC_VIRT_BASE`, writes to APIC ICR register, and
dispatches IPI handler. Sound because `APIC_VIRT_BASE` is set during BSP init
before any IPI is issued; ICR writes follow the APIC specification (write high
word first, then low word to trigger). `// SAFETY:` comments present in source
for most blocks; a small number need back-fill.

---

### smp/mod.rs

AP startup via `start_all_aps`. Delegates to `arch/x86_64/ap_boot.rs`.
`// SAFETY:` comment present in source.

---

### smp/placement.rs

Round-robin core assignment reads the `READY_CORES` count set by `smp/core.rs`.
Sound because the count is written before placement is ever called (BSP marks
core 0 ready before spawning init). `// SAFETY:` comment present in source.

---

### smp/spinlock.rs

`SpinLock<T>` interior-mutable spinlock. Four unsafe constructs:
- `unsafe impl Send for SpinLock<T>`: sound because the atomic spinlock
  serialises all access to `T`; `T: Send` is required.
- `unsafe impl Sync for SpinLock<T>`: same reasoning — mutual exclusion is
  enforced by the atomic before any shared reference is handed out.
- `unsafe { &*self.lock.data.get() }` in `Deref`: sound because the lock is
  held (we have a `SpinLockGuard`); no other reference to the inner data can
  exist simultaneously.
- `unsafe { &mut *self.lock.data.get() }` in `DerefMut`: same reasoning for
  mutable access.

`// SAFETY:` comments present in source for all four.

---

### interrupt/route.rs *(grandfathered)*

`pub unsafe fn deliver(irq: u8)` — called from the IDT stub with IF=0.
One unsafe line remaining (the `unsafe fn` declaration).
`IRQ_TABLE` is now protected by `SpinLock`; registration and delivery are safe
with respect to the lock. The `unsafe` on `deliver` reflects the interrupt-context
calling convention (must only be called from the IDT with IF=0).
`// SAFETY:` comment present in source.

---

### loader.rs *(grandfathered)*

ELF loader: two private helpers (`read_ehdr`, `read_phdr`) each contain one
`read_unaligned` call that copies the entire packed ELF struct into a local
value; all field accesses in `load()` then go through safe local copies with no
unsafe at the call site. The remaining two unsafe blocks are `write_bytes` (BSS
zeroing) and `copy_nonoverlapping` (segment copy); both are bounded by bounds
checks performed above them. `// SAFETY:` comments present in source for all
four blocks.

---

### main.rs *(grandfathered)*

Two unsafe blocks: (1) BSP stack switch via inline ASM — sound because
`BSP_BOOT_STACK` is a 512 KiB static buffer and the pointer arithmetic is
bounded; (2) deref of `boot_info_ptr` — sound because the Limine bootloader
guarantees alignment and validity. The third block (COM2 init) was removed when
`com2_init` was made a safe function. `// SAFETY:` comments present in source
for both remaining blocks.

---

### syscall/dispatch.rs *(grandfathered)*

2 unsafe lines remaining (reduced from 26):
- `pub unsafe extern "C" fn syscall_handler`: the raw ring-3 → ring-0 entry
  point installed as the LSTAR target; must remain `unsafe` because it
  processes untrusted register values from user space.
- `unsafe { map_in_active_tables(va, phys, flags) }` inside `handle_alloc_mem`:
  sound because `va` is in the task heap region (above `0x1_0000_0000`);
  `phys` is a freshly allocated frame from the bitmap allocator; the active
  page table is the calling task's own CR3. `// SAFETY:` comment present in source.

All other handlers were converted from `unsafe fn` to `fn` — their user-pointer
accesses moved to `arch/x86_64::read_user_bytes` / `write_user_bytes` which
encapsulate the unsafe in the permitted arch layer.

---

### task/mod.rs *(grandfathered)*

Seven unsafe blocks: two in the kstack pool (`as_mut_ptr().add(...)` in
`alloc_kstack` and `as_ptr() as u64` in `free_kstack` — necessary HHDM pointer
arithmetic to locate the per-slot buffer); five in the spawn path
(`write_bytes` for stack zeroing, `task_cap_init_empty`, `write_bytes` +
`*mut ServiceContextData` cast for ctx page, `TaskContext::new_user`, and
`commit_task`). All are bounded by prior bounds checks or scheduler-layer
invariants. `// SAFETY:` comments present in source for all blocks.

The previous magic-word liveness scheme (`KSTACK_MAGIC_USED` volatile
reads/writes at slot offset 0) was replaced by `SpinLock<[bool; TASK_KSTACK_MAX]>`,
removing 5 unsafe lines.

---

### task/scheduler.rs *(grandfathered)*

45 unsafe lines (was 36; +9 from the deferred-PML4 self-kill fix).
Covers: per-core current-task arrays, run-queue manipulation,
`prepare_ring3_switch` (context frame write), `task_cap_init_empty`,
`commit_task`, memory-budget arrays, and the deferred PML4 free path.

The 9 new lines are in `drain_pending_kstack` and `kill_task_by_slot`:
- Reading `CORE_PENDING_PML4[cid]` and clearing it after drain.
- `Frame::from_phys(PhysAddr(pml4_phys))` — sound because `pml4_phys` was
  this task's own PML4 root, allocated from the frame allocator at spawn and
  not freed in the reclaim loop; by the time `drain_pending_kstack` runs, CR3
  has been switched to a different page table on this core.
- `crate::memory::allocator::free_frame(frame)` — the frame is ours to free
  (same invariant as above).
- `CORE_CURRENT[my_core]` read and `is_self_kill` detection — bounded by
  `my_core < MAX_CORES`; read under syscall context with IF=0.
- `CORE_PENDING_PML4[my_core] = pml4_phys` store — single writer (this core).
- `crate::smp::ipi::send_ipi(...)` call for cross-core IPI — APIC is
  mapped before the scheduler starts; core index bounded by `MAX_CORES`.
- Inner nested `unsafe {}` block forwarding the IPI call.

Sound in aggregate because: all arrays are indexed by slot or core_id with
bounds checked at their call sites; ring3 switch is called only from the
scheduler with interrupts disabled; cap init runs before the task is visible
to other cores; deferred PML4 free runs only after CR3 switch.
`// SAFETY:` comments present in source for all new blocks.
