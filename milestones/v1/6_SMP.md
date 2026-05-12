# Milestone 6 — SMP and Cross-Core IPC ✅

> Multiple cores running, cross-core IPC working via routing table + IPIs.

## AP Startup

- ✅ Real-mode trampoline at `arch/x86_64/ap_boot.rs` copies to low memory
- ✅ BSP sends INIT + SIPI to each AP
- ✅ Each AP: long-mode setup → `ap_main(core_id)` → `core::mark_ready`
- ✅ BSP waits for all APs to reach `mark_ready` before proceeding
- ✅ AP startup failure logged as warning; system continues with available cores (§11.3)

## Per-Core Scheduler

- ✅ Each core has its own `RunQueue` (no sharing)
- ✅ `scheduler::run()` on each core operates independently
- ✅ Local APIC timer per core fires at 10 ms independently

## IPI Infrastructure

- ✅ `ipi::send(core_id, vector)` writes to APIC ICR
- ✅ `WAKE_RECEIVER (0xF0)` — received by target core's IPI handler → `scheduler::wake(task_id)`
- ✅ `TLB_SHOOTDOWN (0xF1)` — received → `invlpg(addr)` + ack counter increment
- ✅ `SCHEDULER_TICK (0xF2)` — received → force scheduling point

## TLB Shootdown

- ✅ `broadcast_tlb_shootdown(virt)` sends to all cores, spins for acks (§10.5)
- ✅ Called by `memory::ownership` after `PageTable::unmap` on task death

## Cross-Core IPC

- ✅ `send` to endpoint on remote core: enqueue into target core's queue, send `WAKE_RECEIVER` IPI
- ✅ Receiver on remote core wakes from `recv` block when IPI fires
- ✅ Routing table consulted on every send to find target core

## Bugs Fixed

1. **16 KiB kernel task stack overflow** — `Message` is 4208 bytes; a single `recv` iteration copies it to a local variable, pushing pong's stack 688 bytes past its 16 KiB boundary into `STACK_PING`. The `WAKE_RECEIVER` IPI interrupt frame landed in the overlap zone, causing a GPF on `iretq`. Fix: `KernelStack` increased from 16 KiB to 64 KiB (`main.rs`).

2. **Silent `exception_halt` on all unhandled vectors** — all 256 IDT slots mapped to `exception_halt` (cli; hlt) except 32, 0xF0, 0xF1, 0xF2. Any kernel exception silently halted the faulting core with IF=0 — no output, no lock release. Core 0 spun forever on `ROUTE_LOCKED`. Fix: diagnostic handlers for vector 13 (GPF) and vector 14 (#PF) now print `error_code` + `rip` (and `cr2` for PF) then call `halt_all_cores()`.

## Acceptance ✅

- `osdev run --smp 4` shows `smp: 4 cores ready` on serial
- ping on core 0 sends to pong on core 1; pong receives and logs the message
- Cross-core send latency is bounded — 346,500+ sends / 345,200+ receives confirmed with no hang or panic
- Commit: `009f30e`
