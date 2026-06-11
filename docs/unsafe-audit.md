# Unsafe Audit (§18.4)

`scripts/unsafe_check.py` runs on every CI push. It counts non-comment lines
containing the `unsafe` keyword per file and compares to the baseline table below.

**A PR that increases any file's count, or adds unsafe to a new file, fails CI
unless this file is updated in the same commit with a written SAFETY argument.**

---

## 2026-06-08 — fbcon scroll without VRAM read-back

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/fb.rs` | 4 → 3 (−1) | `scroll` no longer `core::ptr::copy`s the framebuffer up in place (which *read* uncached/WC VRAM — ~130 ms/line on the T630, the fbcon perf trap behind the "40× respawn"). It now shifts a RAM char-grid shadow and repaints from it — write-only via `draw_glyph`/`put_pixel` — so the block is gone. |

Reduction only; locks in the lower count. The three remaining blocks (`clear`,
`put_pixel`, `wc_flush`) keep their `// SAFETY:` comments. Hardware-verified
(T630): pixel-correct after thousands of scrolls; spawn 0.906 s → 9.9 ms.

> **Note.** This same day also reconciled 3 files the earlier H4b/H4 hardening
> merges left unaccounted (`page_tables.rs 25→35` permitted; `main.rs` and
> `task/mod.rs` held at their floors 2 and 7 by the clip) — see the entry below.

---

## 2026-06-08 — H4 hardening reconcile, **grandfathered floors held (no amendment)**

The W^X-remap (H4a/H4b) and kstack-guard (H4) work that merged earlier this session
added `unsafe` (all `// SAFETY:`-commented in source) without updating this audit. It
*briefly* raised two grandfathered floors; that was then **clipped back** so the
grandfathered counts return to their long-standing floors and **no §18 amendment is
needed**. The hardening's page-table `unsafe` now lives in the permitted arch layer,
where §18.1 says page-table manipulation belongs.

| File | Net | Layer | What |
|------|-----|-------|------|
| `arch/x86_64/page_tables.rs` | 25 → 35 (+10) | permitted | `entry_for_va`/`walk` PTE-walk + `unmap_active_4k` + `harden_hhdm_nx` (now a safe `fn`) + new `unmap_4k_strided` (the kstack guard-unmap loop, moved here from `task/`). Permitted-layer growth, allowed with SAFETY comments + this entry. |
| `main.rs` | 2 → 2 (no change) | grandfathered | `install_kstack_guards` / `harden_hhdm_nx` are now **safe `fn`s** (their preconditions are boot-ordering, not UB — same shape as `memory::init`/`smp::init`), so the call sites need no `unsafe`. |
| `task/mod.rs` | 7 → 7 (no change) | grandfathered | `install_kstack_guards` is now a safe `fn` whose guard-unmap delegates to `page_tables::unmap_4k_strided` (arch); the static-pool-address `unsafe` is centralised in `kstack_pool_base()` and reused by `free_kstack`, so the net count is unchanged. |

**Why this is better than amending (the clip).** `unsafe fn` is for memory-safety
preconditions whose violation is *UB*; `harden_hhdm_nx` / `install_kstack_guards` have
only *boot-ordering* preconditions (calling them out of order wedges boot — a liveness
bug, not UB), exactly like the already-safe `memory::init` / `smp::init`. Marking them
safe is both more honest and removes the call-site `unsafe`. The genuinely-unsafe work
(CR3 reads, PTE writes, the page unmap) stays in `unsafe {}` blocks **inside the
permitted arch layer** (§18.1). Net: the security hardening landed with **zero**
grandfathered growth. Hardware-verified on the T630 (guard pages install; W^X holds).

---

## 2026-06-04 — idle-halt (cool when idle) + introspection holds-check reconcile

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/interrupts.rs` | 12 → 13 (+1) | `wait_for_interrupt` gains a `sti; hlt` branch so ARAT-capable cores halt (run cool) instead of spinning; the no-ARAT branch keeps the legacy `sti`-only spin. |
| `arch/x86_64/boot.rs` | 79 → 81 (+2) | `cpuid_arat_supported` (`unsafe fn` + `__cpuid(6)`) — detects whether the LAPIC timer survives a C-state, gating the halt. |
| `task/scheduler.rs` | 36 → 37 (+1, grandfathered) | reconciles `current_task_holds_resource` — the §3.1 introspection holds-check (mirrors the existing grandfathered `current_task_lookup_cap`: reads `TASK_CAP[cur].assume_init_ref()`). Added with the introspection gate; the audit count was not bumped then — corrected here. A single read-only line for a security gate, same pattern as the lines already grandfathered in this file. |

All blocks carry `// SAFETY:` comments. The `hlt` is ARAT/TSC-Deadline-gated, so on
hardware without an always-running timer it never executes (no regression).

---

## 2026-06-03 — USB/xHCI stack (boot-verified, T630)

Branch `feat/usb-keyboard`. The userspace USB keyboard stack (§12) added unsafe
in the permitted arch + memory layers (the driver *service* itself is unsafe-free
behind the SDK's audited `Mmio`/`Dma` wrappers — §18.1).

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/pci.rs` | **new, 5 lines** | PCI config mechanism #1 port I/O (`outl`/`inl` + `config_read32`) to locate the xHCI controller. |
| `arch/x86_64/mod.rs` | 33 → 34 (+1) | `console_push_byte` pushes a USB-decoded key into the COM1 RX ring (`uart_rx_push`) so keystrokes reach the shell's `ConsoleRead`. |
| `memory/allocator.rs` | 29 → 32 (+3) | `alloc_contiguous(n)` — bitmap scan for a physically-contiguous run, for the driver's DMA arena. |

All blocks carry `// SAFETY:` comments in source. SDK `mmio.rs`/`dma.rs` unsafe
lives outside `kernel/src/` (the §18.1-amended SDK hardware/ABI layer) and is not
counted by `scripts/unsafe_check.py`, which scans `kernel/src/` only.

---

## 2026-05-31 — static-analysis + unsafe-audit pass (boot-verified, T630)

Full write-up: `milestones/v2/STATIC_ANALYSIS_AUDIT.md`. Branch
`verify/static-analysis-unsafe-audit`, commit `d276566`.

| Area | Result |
|------|--------|
| Policy violation | **Fixed** — `unsafe` removed from `ipc/` (§18.1); moved to `SpinLock::ZEROED` in `smp/spinlock.rs`. |
| Safety / correctness lints | **0** — 11 unnecessary `unsafe`, 11 `static mut` refs (→ `addr_of!`), 14 fn-item→int casts, 6 no-op `mem::forget`. |
| Cruft removed | orphaned `page_fault_handler` + `INTERRUPTED_*` statics. |
| Inventory | reconciled below — 302 lines / 23 files, passes clean; `task/scheduler.rs` 37 → 36 (under floor). |
| Kernel warnings | 104 → 57 (rest intentional unwired architecture). |
| Hardware | boots clean on T630, cross-core ping/pong to 83k+ msgs, zero `#PF`/panic. |

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
| arch/x86_64/ap_boot.rs | 2 | permitted |
| arch/x86_64/boot.rs | 81 | permitted |
| arch/x86_64/context_switch.rs | 11 | permitted |
| arch/x86_64/fb.rs | 3 | permitted |
| arch/x86_64/interrupts.rs | 13 | permitted |
| arch/x86_64/iommu.rs | 74 | permitted |
| arch/x86_64/mod.rs | 34 | permitted |
| arch/x86_64/page_tables.rs | 35 | permitted |
| arch/x86_64/pci.rs | 8 | permitted |
| arch/x86_64/rtc.rs | 1 | permitted |
| arch/x86_64/syscall_entry.rs | 13 | permitted |
| capability/table.rs | 7 | permitted |
| memory/allocator.rs | 32 | permitted |
| memory/frame.rs | 1 | permitted |
| memory/mod.rs | 1 | permitted |
| memory/page.rs | 1 | permitted |
| smp/core.rs | 6 | permitted |
| smp/ipi.rs | 23 | permitted |
| smp/mod.rs | 1 | permitted |
| smp/placement.rs | 1 | permitted |
| smp/spinlock.rs | 5 | permitted |
| interrupt/route.rs | 1 | grandfathered |
| loader.rs | 4 | grandfathered |
| main.rs | 2 | grandfathered |
| syscall/dispatch.rs | 2 | grandfathered |
| task/mod.rs | 7 | grandfathered |
| task/scheduler.rs | 37 | grandfathered |
<!-- unsafe-inventory-end -->

**Permitted total:** 353 lines across 21 files  
**Grandfathered total:** 53 lines across 6 files  
**Grand total:** 406 lines across 27 files

> **2026-06-10** (branch `feat/iommu-dma-confinement`). New file `arch/x86_64/iommu.rs`
> (+60, permitted): the H1 AMD-Vi IOMMU work. Phase 0 (+18) is ACPI-table reads
> (RSDP → RSDT/XSDT → IVRS) through the HHDM. Phase 1 (+42) is the IOMMU control
> interface and translation setup: uncached MMIO register read/write, device-table
> /command-buffer/event-log allocation and DTE writes, the 4-level I/O page-table
> builder/translator/free, and command-buffer invalidation. Every block carries a
> `// SAFETY:` argument that the target is a kernel-mapped IOMMU structure (MMIO
> window, device table, command buffer, or I/O page table) and the access is in
> bounds. All hardware `unsafe` is contained here behind the safe wrapper
> `confine_device()`; `task/mod.rs` calls it without any new `unsafe` (its
> grandfathered floor of 7 is unchanged). See the `arch/x86_64/iommu.rs` entry below.

> **Reconciled 2026-05-31** (branch `verify/static-analysis-unsafe-audit`). The
> permitted-layer growth since the prior baseline is from the AMD GX-420GI ring-3 /
> TSC-Deadline-APIC / COM1 work that landed on `main` (boot.rs, mod.rs, interrupts.rs,
> ipi.rs, allocator.rs). `smp/spinlock.rs` +1 is the new `ZEROED` const (below).
> Reductions: the static-analysis pass removed unnecessary `unsafe` blocks
> (ap_boot, boot, mod, scheduler) and the orphaned `page_fault_handler` /
> `INTERRUPTED_*` diagnostics (interrupts.rs net still up from the AMD work).
> **`task/scheduler.rs` is back to 36** — under its grandfathered floor again.

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

One additional `unsafe {}` block (count +1): `write_apic(apic_virt, APIC_TPR, 0x00)`
in `init_local_apic` — zeroes the Task Priority Register so all interrupt
vector classes (including `WAKE_RECEIVER` at 0xF0) are accepted. Sound because
`apic_virt` is established by the preceding `map_in_active_tables` call within
the same function; `APIC_TPR` offset is within the mapped 4 KiB MMIO page.
`// SAFETY:` comment present in source.

---

### arch/x86_64/context_switch.rs

Stack construction for new kernel and user tasks. `new_kernel` and `new_user`
write a synthetic initial register frame to a freshly allocated kernel stack
pointer. Sound because the stack buffer is owned exclusively by the new task
and not yet visible to the scheduler.

---

### arch/x86_64/fb.rs

Framebuffer text console (Phase 1 boot output, §11.4). Three blocks; two write
to Limine's linear framebuffer at `base + y*pitch + x*bpp`:
- `clear`: `write_bytes(base, 0, height*pitch)` — fills the whole buffer.
- `put_pixel`: writes `bpp` bytes at a bounds-checked offset (`x<width`, `y<height`).

Sound because the framebuffer is the region Limine mapped and sized
(`height*pitch` bytes), it lives in the higher half (PML4 256–511) that every
address space inherits via `PageTable::new`, so it is valid for writes for the
system lifetime; every offset is bounds-checked against the reported geometry.

`scroll` previously held a fourth block — an in-place `copy`/`write_bytes` that
shifted the framebuffer up one glyph row. That `copy` *read the framebuffer back*
(uncached/WC VRAM, ~130 ms/line on the T630); it was replaced by a RAM char-grid
shadow that scroll shifts and repaints from, leaving `scroll` entirely safe
(write-only via `draw_glyph` → `put_pixel`). Net **4 → 3** (2026-06-08).

The remaining `wc_flush` block is a single `SFENCE` instruction. The framebuffer
is mapped write-combining (Limine HHDM default), so the FB lock's atomic release
does not order the WC store buffer — a scroll's pixel stores on one core could
flush after the next line's first glyph drawn on another core, erasing it. Each
`put_byte`/`put_bytes` issues `SFENCE` before releasing the lock so its WC stores
are globally visible in order. Sound because `SFENCE` only orders stores and has
no memory or privilege effects.

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

One additional `unsafe {}` block (count +1): `send_eoi` — writes the local APIC
EOI register via `boot::apic_send_eoi`. Sound because the APIC is mapped before
any IRQ fires and EOI register writes are idempotent with no memory-safety
implications. Exposes APIC EOI as a safe call site in `interrupt/route.rs` (§12)
without increasing the grandfathered count there.

One additional `unsafe {}` block (count +1): `fire_test_irq` — calls
`interrupt::route::deliver(irq)` after disabling interrupts and before
re-enabling them. Sound because IF=0 satisfies `deliver`'s calling convention;
the surrounding `disable_interrupts()` / `enable_interrupts()` calls are safe
arch functions; EOI inside `deliver` is idempotent outside a real hardware
interrupt. Used only by the `FIRE_IRQ` COM2 control command (§22 Tests IR1A/IR1B).

---

### arch/x86_64/iommu.rs

AMD-Vi IOMMU detection (H1 Phase 0). All 18 unsafe lines are raw reads of
firmware ACPI tables — the RSDP, the RSDT/XSDT, and the IVRS — through the HHDM.
The helpers `read_bytes`, `read32`, `read64` are `unsafe fn`; `detect` calls them
at every step. Each block is sound because:

- The RSDP virtual address comes from Limine's `RsdpRequest`, which points at a
  table Limine keeps mapped in the HHDM; the signature is checked before any
  further read.
- Every subsequent table is reached only through a physical pointer that lives
  inside an already-validated parent table, converted to a virtual address via
  the HHDM (`hhdm + phys`), which Limine maps for all usable + ACPI memory.
- Each read stays within the table's own length field (`sdt_len`, `ivrs_len`),
  and the IVHD walk advances by the block's self-reported length and stops on a
  zero length, so it cannot run off the end or loop forever.

Detection only — no behaviour change, no writes, no device programming. The
results are published in two atomics (`IOMMU_PRESENT`, `IOMMU_MMIO_BASE`).

**Phase 1 (translation setup), +42.** The remaining unsafe in this file programs
the IOMMU and builds translation structures. Grouped:

- `mmio_read64` / `mmio_write64` — volatile access to the IOMMU MMIO control
  registers, which `bringup` maps uncached (PCD|PWT) at their HHDM alias before
  any access. Offsets are compile-time constants within the mapped 0x4000 window.
- `setup_structures` / `write_dte` — allocate the device table (2 MiB contiguous),
  command buffer, and event log from the frame allocator, zero them through the
  HHDM, and write DTEs. All writes target the freshly-allocated, HHDM-mapped
  structures; the DTE index is a 16-bit BDF, in bounds of the 64K-entry table.
- `io_walk_or_alloc` / `io_map_page` / `io_translate` / `free_io_table` — the
  4-level AMD-Vi I/O page-table builder, read-only translator, and frame reclaim.
  Each level VA is the HHDM alias of a present/just-allocated table; indices are
  masked to 9 bits (< 512), so every read/write is in bounds of a 4 KiB table.
  `free_io_table` frees only the page-table frames (reached top-down from a root
  that `release_device` has already detached from the device), never the leaf
  arena pages.
- `invalidate_device` — writes 16-byte commands into the mapped command-buffer
  ring at the hardware tail offset (masked to the 4 KiB ring) and rings the tail
  register; serialised by `CMD_LOCK`.
- `drain_event_log` — reads decoded fault events from the mapped 4 KiB event-log
  ring (head < 0x1000) and advances the head register; bounded per call so it is
  safe to invoke from the timer-tick path (`control::process_pending`). Also
  recovers from event-log overflow (disable EvtLogEn, RW1C the status bit, reset
  head/tail, re-enable) — all writes to valid IOMMU control/status/pointer regs.
- `confine_device` / `confinement_selftest` / `release_device` — orchestrate the
  above; the raw work they do directly is zeroing a freshly-allocated page table,
  an `sfence` (no memory-safety effect, orders prior stores), and (on release)
  reverting a DTE before freeing the now-unreachable I/O page table.

`confine_device`, `release_device`, `event_log_state`, and `bringup` are the safe
entry points;
all callers outside the arch layer (e.g. `task/mod.rs`) use them without `unsafe`.
`// SAFETY:` comments present on every block.

---

### arch/x86_64/mod.rs

Serial port init and COM2 init via `outb`/`inb`, `cli`/`hlt` in the halt loop,
and the `init` call chain into `boot.rs`. Sound because serial ports are
exclusively owned by the kernel at these call sites; `cli`/`hlt` is the correct
halt sequence; all callers are within the single-threaded BSP init path.

One additional `unsafe {}` block (count +1): `console_push_byte` calls
`uart_rx_push(b)` to enqueue a USB-keyboard-decoded byte into the COM1 RX ring,
then wakes any task blocked in `ConsoleRead`. Sound because the RX ring is a
single-logical-producer buffer (the timer-ISR UART drain and the xHCI driver's
`ConsolePush` syscall both run on Core 0's serial path); the push is a bounded
ring write with head/tail wrap. `// SAFETY:` comment present in source.

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

Ten additional unsafe lines (count 25 → 35) from the W^X / guard-page hardening
(H4a/H4b, 2026-06-07/08):
- `entry_for_va` / `walk` / `read_entry` chain — read-only PTE walk used to probe
  a VA's mapping (PTE/large-page) for the W^X audit and the kstack-guard install.
- `unmap_active_4k(virt)` (`unsafe fn` + CR3 read + present-entry walk + clear PTE
  + `invlpg`) — marks a 4 KiB page non-present; no-ops on a large page (fails safe).
- `unmap_4k_strided(base, stride, count)` — a **safe `fn`** that unmaps the low page
  of each kstack slot via `unmap_active_4k`; the guard-unmap loop moved here from
  `task/` (§18.1 — page-table work belongs in arch) so it adds no grandfathered
  unsafe. Boot-ordering contract (BSP, before APs).
- `harden_hhdm_nx()` — a **safe `fn`** (CR3 read + HHDM subtree walk OR-ing NX into
  every present PDPT/PD/PT, then CR3 reload) that flips the HHDM `NO_EXEC`, closing the
  Limine-mapped RWX direct map (§3.12). Boot-ordering precondition (after `smp::init`),
  not UB — hence safe; the CR3/PTE work inside stays `unsafe {}`.

All sound for the same reason as the rest of the file: HHDM is live, the tables are
reached via present entries rooted at the live CR3-referenced PML4, and these run
BSP-only at boot before APs execute from the affected region. `// SAFETY:` comments
and `# Safety` docs present in source for every block.

---

### arch/x86_64/pci.rs

PCI configuration-space access via legacy mechanism #1 (port `0xCF8` address /
`0xCFC` data), used once at boot to locate the xHCI USB host controller and
record its MMIO base + IRQ (§12). Five unsafe lines:
- `unsafe fn outl` / its inner `unsafe {}` block — 32-bit `out dx, eax` port write.
- `unsafe fn inl` / its inner `unsafe {}` block — 32-bit `in eax, dx` port read.
- `unsafe {}` in `config_read32` — pairs an `outl(address)` then `inl(data)`.

Sound because port I/O is ring-0 and these ports are the architecturally fixed
PCI config registers, owned exclusively by the kernel during single-threaded BSP
boot (the scan runs before any AP or task exists); the address dword is
constructed from bounded bus/dev/func/offset values with the enable bit set per
the mechanism-#1 spec. `// SAFETY:` comments present in source.

Three additional unsafe lines (+3) for the EHCI BIOS→OS handoff
(`ehci_bios_handoff`): the `unsafe {}` in `config_write32` (paired `outl(address)`
+ `outl(data)`, same discipline as `config_read32`), the `map_in_active_tables`
call mapping the EHCI MMIO page to read HCCPARAMS, and the `read_volatile` of
HCCPARAMS. Sound for the same reason — ring-0 BSP boot, architecturally fixed
ports, the MMIO page mapped uncached before the single aligned read. All carry
`// SAFETY:` comments.

---

### arch/x86_64/rtc.rs

MC146818 CMOS real-time-clock read via the legacy index/data ports (`0x70` /
`0x71`), used to answer `InspectKernel` query 11 (wall-clock date/time) for the
shell's `date`/`time` commands (§12). One unsafe line:
- `unsafe {}` in `cmos_read` — wraps an `out dx, al` (select register) followed by
  an `in al, dx` (read its value); the two asm blocks are not `pure`, so their
  order is preserved.

Sound because port I/O is ring-0 and these are the architecturally fixed CMOS
ports; only a register number (`0x00..0x3F`) is written, and the read is
side-effect-free. The driver is read-only — it never writes CMOS — so it cannot
disturb other clock/NMI state. `// SAFETY:` comment present in source.

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

Three additional `unsafe` lines (count 29 → 32): the `alloc_contiguous(n)` path
for driver DMA arenas (§12) — the `unsafe fn alloc_contiguous` method, its inner
`&mut *addr_of_mut!(BITMAP)` access, and the public `alloc_contiguous` wrapper's
`(*addr_of_mut!(ALLOCATOR)).alloc_contiguous(n)` call. Sound for the same reason
as the rest of the allocator: every access holds `ALLOC_LOCKED` (single writer
across all cores), and the bitmap scan is bounds-checked against
`max_valid_frame`. `// SAFETY:` comments present in source for all three.

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

`SpinLock<T>` interior-mutable spinlock. Five unsafe constructs:
- `unsafe impl Send for SpinLock<T>`: sound because the atomic spinlock
  serialises all access to `T`; `T: Send` is required.
- `unsafe impl Sync for SpinLock<T>`: same reasoning — mutual exclusion is
  enforced by the atomic before any shared reference is handed out.
- `unsafe { &*self.lock.data.get() }` in `Deref`: sound because the lock is
  held (we have a `SpinLockGuard`); no other reference to the inner data can
  exist simultaneously.
- `unsafe { &mut *self.lock.data.get() }` in `DerefMut`: same reasoning for
  mutable access.
- `pub const ZEROED: Self = unsafe { core::mem::zeroed() }`: all-zeroes
  initializer for placing a large `SpinLock<T>` in `.bss` without the undef
  padding bytes that LLD rejects there. Sound only when the all-zeroes bit
  pattern is a valid `T` — the caller's responsibility via the `T` instantiated.
  Replaces a `core::mem::zeroed()` that previously sat in `ipc/routing.rs`
  (outside the permitted layers); moving it here keeps `ipc/` unsafe-free (§18.1).

`// SAFETY:` comments present in source for all five.

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
guarantees alignment and validity. (The earlier COM2-init block was removed when
`com2_init` was made a safe function.) `// SAFETY:` comments present in source.

The H4 hardening calls (`install_kstack_guards`, `harden_hhdm_nx`) are **safe `fn`s**
(boot-ordering preconditions, not UB), so they add no `unsafe` here — see the
2026-06-08 reconcile.

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

Seven unsafe blocks: two in the kstack pool — `kstack_pool_base` (`addr_of!` of the
`static mut KSTACK_STORAGE`, the single encapsulated pool-address read, reused by
`free_kstack`) and `alloc_kstack` (`(addr_of_mut!(...) as *mut u8).add(...)` slot-top
arithmetic) — and five in the spawn path (`write_bytes` for stack zeroing,
`task_cap_init_empty`, `write_bytes` + `*mut ServiceContextData` cast for the ctx
page, `TaskContext::new_user`, and `commit_task`). All bounded by prior bounds checks
or scheduler-layer invariants. `// SAFETY:` comments present in source.

The H4 kstack-guard install (`install_kstack_guards`) is a **safe `fn`**: it reads the
pool base via `kstack_pool_base()` and delegates the per-slot page unmap to
`page_tables::unmap_4k_strided` (the arch layer, §18.1), so it adds no `unsafe` here —
see the 2026-06-08 reconcile. (Centralising the pool-address read in `kstack_pool_base`
also let `free_kstack` drop its own `addr_of!` block, holding the net count at 7.)

The previous magic-word liveness scheme (`KSTACK_MAGIC_USED` volatile
reads/writes at slot offset 0) was replaced by `SpinLock<[bool; TASK_KSTACK_MAX]>`,
removing 5 unsafe lines.

---

### task/scheduler.rs *(grandfathered)*

36 unsafe lines. Five formerly-`static mut` arrays converted to atomic types,
removing eight standalone `unsafe {}` blocks (previous count was 42, then 38,
now back to the original 36 floor after `TASK_VALID` was also converted):

- `CORE_CURRENT` → `[AtomicUsize; MAX_CORES]`: removed standalone `unsafe` in
  `current_task_slot()`; accesses updated to `.load()`/`.store()`.
- `CORE_RR_SLOT` → `[AtomicUsize; MAX_CORES]`: removed both standalone `unsafe`
  blocks in `pick_next()`.
- `CORE_PENDING_KSTACK_LEN` → `[AtomicUsize; MAX_CORES]`: removed both
  standalone `unsafe` blocks in `drain_pending_kstack()`.
- `TASK_KERNEL_STACK_TOP` → `[AtomicU64; MAX_TASKS]`: removed the standalone
  `unsafe` in `prepare_ring3_switch()`.
- `TASK_VALID` → `[AtomicBool; MAX_TASKS]`: removed the standalone
  `unsafe { TASK_VALID[slot] = false; }` in `release_task_slot()` and the
  inline `if !unsafe { TASK_VALID[slot] }` in `for_each_active_cap()`. All
  stores use `Release` ordering; the lock-free `for_each_active_cap` read uses
  `Acquire` to pair with `Release` stores and ensure cap table visibility; all
  reads inside lock-protected regions use `Relaxed`.
- `CORE_PENDING_PML4` is `AtomicU64` so its load/store sites are safe — only
  the `Frame::from_phys` + `free_frame` pair and the `send_ipi` call needed
  `unsafe` blocks.

One remaining line in `for_each_active_cap` is still `unsafe`:
- `unsafe { TASK_CAP[slot].assume_init_ref() }.for_each_slot(&mut f)` — reads
  a `MaybeUninit<CapTable>` after `TASK_VALID[slot].load(Acquire)` returned
  `true`. Sound because the `Acquire` load pairs with the `Release` store in
  `reserve_task_slot`/`enqueue`, establishing that the `CapTable` write
  happened-before this read. `CapTable` cannot be const-constructed so
  `MaybeUninit` is necessary; `assume_init_ref` is the unavoidable unsafe
  assertion that the slot is initialised.

One additional `unsafe {}` block (count +1, net): `TASK_CORE` reads in
`pick_next` — the wake-hint fast path (`TASK_CORE[hint]`) and the RR scan loop
(`TASK_CORE[idx]`) both read this `static mut [u32; MAX_TASKS]` array. Sound
because `TASK_CORE[slot]` is written exactly once at spawn and never modified
thereafter (§9.1 static-placement invariant); all indices are bounded by
`MAX_TASKS`; reads are unsynchronised but safe because the value is immutable
after task spawn. Two new `unsafe` lines were added; one previously-unsafe
access to a now-atomic variable was removed, yielding net +1.
`// SAFETY:` comments present in source for both new blocks.

Sound in aggregate: all arrays are indexed by slot or core_id with bounds
checked at their call sites; ring3 switch is called only from the scheduler
with interrupts disabled; cap init runs before the task is visible to other
cores; deferred PML4 free runs only after CR3 switch.
`// SAFETY:` comments present in source for all blocks.
