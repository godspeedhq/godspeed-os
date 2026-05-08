# Milestone 6 — SMP and Cross-Core IPC

> Multiple cores running, cross-core IPC working via routing table + IPIs.

## AP Startup

- [ ] Real-mode trampoline at `arch/x86_64/ap_boot.rs` copies to low memory
- [ ] BSP sends INIT + SIPI to each AP
- [ ] Each AP: long-mode setup → `ap_main(core_id)` → `core::mark_ready`
- [ ] BSP waits for all APs to reach `mark_ready` before proceeding
- [ ] AP startup failure logged as warning; system continues with available cores (§11.3)

## Per-Core Scheduler

- [ ] Each core has its own `RunQueue` (no sharing)
- [ ] `scheduler::run()` on each core operates independently
- [ ] Local APIC timer per core fires at 10 ms independently

## IPI Infrastructure

- [ ] `ipi::send(core_id, vector)` writes to APIC ICR
- [ ] `WAKE_RECEIVER (0xF0)` — received by target core's IPI handler → `scheduler::wake(task_id)`
- [ ] `TLB_SHOOTDOWN (0xF1)` — received → `invlpg(addr)` + ack counter increment
- [ ] `SCHEDULER_TICK (0xF2)` — received → force scheduling point

## TLB Shootdown

- [ ] `broadcast_tlb_shootdown(virt)` sends to all cores, spins for acks (§10.5)
- [ ] Called by `memory::ownership` after `PageTable::unmap` on task death

## Cross-Core IPC

- [ ] `send` to endpoint on remote core: enqueue into target core's queue, send `WAKE_RECEIVER` IPI
- [ ] Receiver on remote core wakes from `recv` block when IPI fires
- [ ] Routing table consulted on every send to find target core

## Acceptance

- `osdev run --smp 4` shows `smp: 4 cores ready` on serial
- ping on core 0 sends to pong on core 1; pong receives and logs the message
- Cross-core send latency is bounded (no hang, no panic)
