# kernel/src/task/

Task management and per-core scheduler (§9, §14).

## Files

| File            | Responsibility |
|-----------------|---------------|
| `mod.rs`        | `spawn_init()`, `kill_current()` |
| `task.rs`       | `Task` struct: id, name, core_id, state, context, page table, cap table, memory owner |
| `state.rs`      | `TaskState` enum: Ready, Running, BlockedOnRecv, BlockedOnSend, Dead |
| `scheduler.rs`  | `run()` (never returns), `timer_tick()`, `wake(task_id)`, `block_on_send(endpoint)` |

## Static placement invariant (§9.1)

A task's `core_id` is set at spawn and never changes. Mid-execution migration is forbidden. The invariant is enforced by `invariants::assertions::assert_no_mid_execution_migration`, called from the scheduler before every context switch.

## Preemption (§9.1, §9.3)

The 10 ms quantum is enforced by the local APIC timer. `timer_tick()` is called from the timer ISR on every core independently. `yield()` is advisory — it calls `timer_tick()` immediately but preemption happens regardless of whether the service yields.

## Spawn flow (§14.1)

`spawn_init()` is the only direct spawn from kernel code. All other spawns go through supervisor → syscall → kernel. The kernel side of spawn:
1. Calls `smp::placement::resolve(contract_core)` to get the target core.
2. Allocates a `Task` with a fresh `CapTable` populated from the contract.
3. Allocates a page table and maps the service binary.
4. Adds the task to the target core's run queue.

## Kill flow (§14.4)

`kill_current()` (page fault) and the supervisor `kill` syscall both:
1. Set `task.state = Dead`.
2. Bump the generation of all endpoints owned by the task (via `ipc::routing::kill_endpoint`).
3. Call `smp::ipi::broadcast_tlb_shootdown` for any mapped pages.
4. Call `memory::ownership::reclaim_all` to return frames.
5. Notify supervisor via its death-notification endpoint.
6. Call `scheduler::pick_next()` to resume another task.
