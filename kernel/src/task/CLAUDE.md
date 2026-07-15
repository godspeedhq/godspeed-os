# kernel/src/task/

Task management and per-core scheduler (§9, §14).

## Files

| File            | Responsibility |
|-----------------|---------------|
| `mod.rs`        | `spawn_supervisor()` (the kernel's one direct spawn - init removed, Phase 5), `kill_current()`, `drain_pending_kstack()` |
| `task.rs`       | `Task` struct: id, name, core_id, state, context, page table, cap table, memory owner |
| `state.rs`      | `TaskState` enum: Ready, Running, BlockedOnRecv, BlockedOnSend, Dead |
| `scheduler.rs`  | `run()` (never returns), `timer_tick()`, `wake(task_id)`, `block_on_send(endpoint)` |

> **Doc-drift correction (documentation-audit Audit 2, 2026-07-15; kernel-audit M1/M2).** Two mechanisms
> named below are **dead code** (zero live callers), pending removal: the spawn-flow's
> `smp::placement::resolve` is dead - the live core placement is `task/mod.rs::resolve_spawn_core` (atomic
> round-robin); the kill-flow's `memory::ownership::reclaim_all` is dead - the live kill-path reclaim is
> `arch/x86_64/page_tables.rs::reclaim_user_frames`. The described behaviour is right; the function names
> are stale.

## Static placement invariant (§9.1)

A task's `core_id` is set at spawn and never changes. Mid-execution migration is forbidden. The invariant is enforced by `invariants::assertions::assert_no_mid_execution_migration`, called from the scheduler before every context switch.

## Preemption (§9.1, §9.3)

The 10 ms quantum is enforced by the local APIC timer. `timer_tick()` is called from the timer ISR on every core independently. `yield()` is advisory - it calls `timer_tick()` immediately but preemption happens regardless of whether the service yields.

## Kernel stack pool

224 slots × 64 KiB = 14 MiB of static BSS. Liveness is tracked by `SpinLock<[bool; TASK_KSTACK_MAX]>` - a boolean flag per slot, locked for the duration of alloc/free. `alloc_kstack()` returns the top pointer; `free_kstack(kstack_top)` reverse-computes the slot index from the pointer and clears the flag. The pool uses two unavoidable unsafe lines: one pointer-arithmetic `as_mut_ptr().add(...)` to locate the slot top, and one `as_ptr() as u64` to compute the base address for reverse-index in `free_kstack`.

## Spawn flow (§14.1)

`spawn_supervisor()` is the only direct spawn from kernel code (Path C / Phase 5 - init removed; the kernel boots the supervisor directly). All other spawns go through supervisor → syscall → kernel. The kernel side of spawn:
1. Calls `smp::placement::resolve(contract_core)` to get the target core.
2. Allocates a `Task` with a fresh `CapTable` populated from the contract.
3. Allocates a page table and maps the service binary.
4. Adds the task to the target core's run queue.

## Kill flow (§14.4)

`kill_current()` (page fault) and the supervisor `kill` syscall both:
1. Set `task.state = Dead`.
2. Bump the generation of all endpoints owned by the task (via `ipc::routing::kill_endpoint`).
3. Call `smp::ipi::broadcast_tlb_shootdown` for any mapped pages.
4. Call `memory::ownership::reclaim_all` to return frames - **skipping the PML4 frame** (see below).
5. Notify supervisor via its death-notification endpoint.
6. Call `scheduler::pick_next()` to resume another task.

## Deferred PML4 free (self-kill path)

In the self-kill path (the dying task's CR3 is still active on the core), the PML4 frame is **not** freed immediately. Freeing it while it is still loaded in CR3 creates a use-after-free window: another core's `PageTable::new()` can immediately alloc and zero that frame, and on a TLB miss the hardware page-walker reads the zeroed PML4, sees entry 511 = 0 (not present), and generates a kernel page fault.

Fix: the PML4 frame is stored in `CORE_PENDING_PML4[my_core]` during the kill path. It is freed at the next `drain_pending_kstack` call (timer tick or scheduler idle) when a different CR3 is already loaded on that core.

## Control channel polling (§17)

`timer_tick()` on Core 0 calls `control::process_pending()` on every tick. This drains COM2 bytes and executes any `RESTART`/`KILL` commands. The call is made **on every tick** (not only in the idle branch) so control commands are processed even when Core 0 always has runnable tasks.
