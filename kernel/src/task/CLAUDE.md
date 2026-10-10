# kernel/src/task/

Task management and per-core scheduler (§9, §14).

## Files

| File            | Responsibility |
|-----------------|---------------|
| `mod.rs`        | `spawn_supervisor()` (the kernel's one direct spawn - init removed, Phase 5), `spawn_from_image()` (every other spawn, from the supervisor's `SpawnImage` request), `kill_current()`, the kernel stack pool, `privbits` / `privileges_caller_lacks` / `SUPERVISOR_DELEGATABLE` |
| `state.rs`      | `TaskState` enum: Ready, Running, BlockedOnRecv, BlockedOnSend, Dead |
| `scheduler.rs`  | `run()` (never returns), `timer_tick_from_irq()`, `yield_current()`, `block_and_reschedule()`, `wake_by_slot()`, `kill_task_by_slot()`, `drain_pending_kstack()` |

> **Doc-drift correction (documentation-audit Audit 2, 2026-07-15; kernel-audit M1/M2).** Two mechanisms
> once named below had no callers: `smp::placement::resolve` (the live core placement is
> `task/mod.rs::resolve_spawn_core`) and `memory::ownership::reclaim_all` (the live kill-path reclaim is
> `arch/x86_64/page_tables.rs::reclaim_user_frames`). The steps below name the live functions; the dead
> ones, with an unconstructed `Task` struct and `TaskId` in `task.rs`, were deleted 2026-10-10
> (`backlog/80` K20).

## Static placement invariant (§9.1)

A task's `core_id` is set at spawn and never changes. Mid-execution migration is forbidden. The invariant is enforced by `invariants::assertions::assert_no_mid_execution_migration`, called from the scheduler before every context switch.

## Preemption (§9.1, §9.3)

The 10 ms quantum is enforced by each core's local timer (the local APIC on x86-64; each port's own timer elsewhere). `timer_tick_from_irq()` is called from the timer ISR on every core independently. `yield()` is advisory - `yield_current()` gives up the rest of the quantum at once, but preemption happens regardless of whether the service yields.

## Kernel stack pool

224 slots × 64 KiB = 14 MiB of static BSS (`TASK_KSTACK_MAX`, `KSTACK_SIZE`). Liveness is tracked by `KSTACK_USED: SpinLock<[bool; TASK_KSTACK_MAX]>` - a boolean flag per slot, locked for the duration of alloc/free. `alloc_kstack()` returns the top pointer; `free_kstack(kstack_top)` reverse-computes the slot index from the pointer and clears the flag. The pool uses two unavoidable unsafe lines: one pointer-arithmetic `as_mut_ptr().add(...)` to locate the slot top, and one `as_ptr() as u64` to compute the base address for reverse-index in `free_kstack`.

## Spawn flow (§14.1)

`spawn_supervisor()` is the only direct spawn from kernel code (Path C / Phase 5 - init removed; the kernel boots the supervisor directly). Every other spawn is the supervisor's `SpawnImage` request (§14.1, step C). The kernel side of spawn:
1. Calls `resolve_spawn_core` to get the target core (strict override, else the request's preferred core with a loud round-robin fallback).
2. Allocates a task slot with a fresh `CapTable` populated from the SPAWN REQUEST (§13.6) - never from a contract, which the kernel cannot read.
3. Allocates a page table and maps the service binary.
4. Adds the task to the target core's run queue.

## Kill flow (§14.4)

`kill_current()` (a fault) and the `kill` syscall both reach `scheduler::kill_task_by_slot`, which:
1. Sets the task's state to Dead.
2. Kills its endpoints (`ipc::routing::kill_endpoint`), wakes blocked senders/receivers with `EndpointDead` and blocked `Call`ers with `ReplyDead`, and marks the resources dead in the capability table.
3. Waits until no other core is still running the task, then reclaims its user frames (`arch::imp::page_tables::reclaim_user_frames`) - **skipping the PML4 frame** on the self-kill path (see below).
4. Notifies the supervisor via its death-notification endpoint **only if the task was spawned `SPAWN_FLAG_WATCHED`**; if the dead task is the supervisor itself, flags the kernel's respawn of it instead (§6.2).
5. Returns to the scheduler, which picks another task.

## Deferred PML4 free (self-kill path)

In the self-kill path (the dying task's CR3 is still active on the core), the PML4 frame is **not** freed immediately. Freeing it while it is still loaded in CR3 creates a use-after-free window: another core's `PageTable::new()` can immediately alloc and zero that frame, and on a TLB miss the hardware page-walker reads the zeroed PML4, sees entry 511 = 0 (not present), and generates a kernel page fault.

Fix: the PML4 frame is stored in `CORE_PENDING_PML4[my_core]` during the kill path. It is freed at the next `drain_pending_kstack` call (timer tick or scheduler idle) when a different CR3 is already loaded on that core.

## Control channel polling (§17) - NO LONGER IN THE KERNEL

`timer_tick_from_irq()` does **not** call `control::process_pending()`; that module does not exist,
and the supervisor-respawn note in `task/mod.rs` (above `poll_supervisor_respawn`) says so directly.
The COM2 control channel is `services/control`, a restartable userspace service (moved out in C1-6),
which reads the port's bytes through `InspectKernel` query 21. Nothing on the timer tick drains COM2.
