# Milestone 3 — Scheduler (Single Core)

> Per-core run queue, context switching, and timer preemption working on one core.

**Status: COMPLETE** — 2026-05-09

Serial output (SMP=1 and SMP=4 both pass):
```
memory: frame allocator ready (510 MiB free)
capability: subsystem ready
ipc: routing table ready
kernel: all cores ready
scheduler: task-a and task-b enqueued
task-b: 1
task-a: 1
task-a: 2
task-b: 2
task-b: 3
task-a: 3
...  (both tasks interleave indefinitely without either stalling)
```

## Task Structure

- ✅ `Task` struct: `TaskContext`, stack, state (`Ready`/`Running`/`Blocked`/`Dead`)
- ✅ Stack allocation per task (static `KernelStack` wrappers, 16-byte aligned)
- ✅ Initial `TaskContext` set up via `new_kernel` with entry-trampoline so first
      `switch_context` jumps to `task_entry_trampoline` → `sti` → real entry

## Run Queue

- ✅ Per-core `RunQueue`: round-robin over `Ready` tasks (parallel static arrays,
      max 8 slots, `IDLE` sentinel)
- ✅ `enqueue(name, ctx)` / `pick_next()` (round-robin from CURRENT+1)
- ✅ `scheduler::run()` loop: CLI, pick next task, switch_context; idle-HLTs when empty

## Context Switch

- ✅ `switch_context` saves/restores callee-saved registers (rbx/rbp/r12–r15) + RSP + CR3
- ✅ RSP field at offset 0x38 (layout verified against struct)
- ✅ Verified: switching between two tasks executes both

## Preemption

- ✅ Legacy 8259 PIC remapped (IRQ0–7 → vectors 32–39) and fully masked before IDT live
- ✅ Local APIC timer: periodic mode, vector 32, ÷16, 625,000 ticks (~10 ms at 1 GHz bus)
- ✅ APIC MMIO mapped explicitly via `map_in_active_tables` with PCD+PWT (Limine HHDM
      covers RAM only, not MMIO at 0xFEE00000)
- ✅ `init_local_apic` called after `memory::init` so HHDM offset is set
- ✅ Timer ISR stub (`timer_isr_stub`) saves/restores caller-saved regs, calls
      `timer_tick_from_irq`, iretq
- ✅ `timer_tick_from_irq` sends EOI, transitions prev task Ready, picks next, switches
- ✅ A tight-loop task does not starve another task on the same core

## Key bugs fixed

- **IF=0 on first task run**: `scheduler::run()` calls `cli` before `switch_context`;
  new tasks started with interrupts disabled.  Fix: `task_entry_trampoline` — a naked
  `sti` + `ret` stub pushed below the real entry on each task's initial stack.
- **APIC MMIO page fault**: Limine's HHDM does not map physical MMIO regions.
  Fix: `map_in_active_tables()` in `page_tables.rs` adds the APIC frame to the live
  page tables before the first MMIO write.
- **PIC vector collision**: 8259 IRQ0 fires at vector 8 (double-fault) by default.
  Fix: `mask_pic()` in `boot.rs` remaps and masks all PIC IRQs before IDT is loaded.

## Acceptance

- ✅ Two tasks on core 0 both make progress (serial output from each) over a 1 s window
- ✅ Removing explicit yields does not stop the second task from running
- ✅ No kernel panic on SMP=1 or SMP=4
