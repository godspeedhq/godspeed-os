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
| arch/x86_64/interrupts.rs | 5 | permitted |
| arch/x86_64/mod.rs | 22 | permitted |
| arch/x86_64/page_tables.rs | 25 | permitted |
| arch/x86_64/syscall_entry.rs | 13 | permitted |
| capability/table.rs | 7 | permitted |
| memory/allocator.rs | 28 | permitted |
| memory/frame.rs | 1 | permitted |
| memory/mod.rs | 1 | permitted |
| memory/page.rs | 1 | permitted |
| smp/core.rs | 6 | permitted |
| smp/ipi.rs | 21 | permitted |
| smp/mod.rs | 1 | permitted |
| smp/placement.rs | 1 | permitted |
| control.rs | 1 | grandfathered |
| interrupt/route.rs | 3 | grandfathered |
| ipc/names.rs | 2 | grandfathered |
| ipc/routing.rs | 10 | grandfathered |
| loader.rs | 16 | grandfathered |
| log.rs | 2 | grandfathered |
| main.rs | 3 | grandfathered |
| syscall/dispatch.rs | 26 | grandfathered |
| task/mod.rs | 12 | grandfathered |
| task/scheduler.rs | 38 | grandfathered |
<!-- unsafe-inventory-end -->

**Permitted total:** 206 lines across 16 files  
**Grandfathered total:** 113 lines across 10 files  
**Grand total:** 319 lines across 26 files

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

### control.rs *(grandfathered)*

One unsafe block: stack switch at kernel entry using inline ASM. This is the
same pattern as `main.rs` below. Sound because the new stack pointer targets
the top of `BSP_BOOT_STACK`, a 512 KiB static buffer; the alignment is
enforced by the `& !15` mask. `// SAFETY:` comment present in source.

---

### interrupt/route.rs *(grandfathered)*

IRQ routing table (`IRQ_TABLE`) is a static array protected by an
`InterruptLock`. Three unsafe blocks: `register` (writes the table), `deliver`
(reads the table), and the inner read inside `deliver`. Sound because interrupt
delivery runs with interrupts disabled (CLI in the IDT stub); the lock prevents
concurrent registration.
*(One SAFETY comment missing in source — needs back-fill.)*

---

### ipc/names.rs *(grandfathered)*

Endpoint name table (`NAMES`) — static array protected by a `SpinLock`. Two
unsafe blocks access the table under the lock. Sound because the lock ensures
mutual exclusion; indices are bounds-checked. `// SAFETY:` comments present in
source.

---

### ipc/routing.rs *(grandfathered)*

IPC routing table (`TABLE`) — static array protected by a `SpinLock`. Ten unsafe
blocks: `register`, `find_index`, `enqueue_locked`, `dequeue_locked`, and
`kill_endpoint`. Sound because all paths hold the routing lock before accessing
the table; liveness flags are set atomically with generation bumps.
*(Several SAFETY comments missing in source — needs back-fill.)*

---

### loader.rs *(grandfathered)*

ELF loader: reads ELF header and program header fields via `read_unaligned`
(ELF structs have no alignment guarantee), then calls `map_in_active_tables`
and `copy_nonoverlapping` to load segments. Sound because: unaligned reads are
the correct way to access packed ELF structs; segment bounds are validated
against the input slice before any copy; `map_in_active_tables` holds the frame
lock for each mapping.
*(Most SAFETY comments missing in source — needs back-fill.)*

---

### log.rs *(grandfathered)*

Ring buffer write and drain: two unsafe calls into a static `RingBuffer`
protected by a `SpinLock`. Sound because both paths hold the lock; the ring
buffer is never accessed from interrupt context without disabling interrupts
first. `// SAFETY:` comments present in source.

---

### main.rs *(grandfathered)*

Three unsafe blocks: (1) BSP stack switch via inline ASM — sound because
`BSP_BOOT_STACK` is a 512 KiB static buffer and the pointer arithmetic is
bounded; (2) deref of `boot_info_ptr` — sound because the Limine bootloader
guarantees alignment and validity; (3) COM2 init call — sound because it runs
once on the BSP before the scheduler starts. `// SAFETY:` comments present in
source for two of the three.

---

### syscall/dispatch.rs *(grandfathered)*

26 unsafe lines covering all syscall handlers. Common pattern: read a user-space
pointer as a `&[u8]` via `core::slice::from_raw_parts`. Sound in each case
because: (a) the syscall entry point validates that the pointer and length fit
within the user address space before dispatch; (b) the kernel holds no reference
to user memory after the syscall returns. Cap table access goes through the
capability lock.
*(Most SAFETY comments missing in source — needs back-fill.)*

---

### task/mod.rs *(grandfathered)*

Kernel stack allocator: `kstack_marker` reads/writes the first word of a stack
slot to track liveness; `alloc_kstack` / `free_kstack` manage the pool;
`spawn_task` calls into `scheduler.rs`. Sound because stack slot indices are
bounded by `TASK_KSTACK_MAX`; the magic-word liveness check prevents
double-alloc. `// SAFETY:` comments present in source for most blocks.

---

### task/scheduler.rs *(grandfathered)*

Largest grandfathered file (38 unsafe lines). Covers: per-core current-task
arrays, run-queue manipulation, `prepare_ring3_switch` (context frame write),
`task_cap_init_empty`, `commit_task`, memory-budget arrays, and `cli`/`sti`
in the idle loop. Sound in aggregate because: all arrays are indexed by slot or
core_id with bounds checked at their call sites; ring3 switch is called only
from the scheduler with interrupts disabled; cap init runs before the task is
visible to other cores.
*(~10 SAFETY comments missing in source — needs back-fill.)*
