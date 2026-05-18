# kernel/src/ipc/

Synchronous message-passing IPC (§8). No unsafe code lives here; physical memory and APIC access are done through `memory/` and `smp/ipi.rs`.

## Files

| File           | Responsibility |
|----------------|---------------|
| `mod.rs`       | Public API: re-exports, `init()` |
| `message.rs`   | `Message` (4 KiB max payload, ≤4 embedded caps), `IpcError` |
| `queue.rs`     | `MessageQueue`: fixed-depth FIFO, 16 messages, `enqueue`/`dequeue`/`drain` |
| `endpoint.rs`  | `Endpoint`: owner, core pin, queue, blocked-receiver field |
| `routing.rs`   | `RoutingTable`: `EndpointId → (CoreId, Generation, Liveness)`; `enqueue`, `kill_endpoint`. Protected by `SpinLock<[RoutingEntry; MAX_ENDPOINTS]>`. |
| `names.rs`     | Name → `EndpointId` registry. `register(name, ep)`, `lookup(name)`. Protected by `SpinLock<[NameEntry; MAX_ENTRIES]>`. |

## Message size and queue depth (§8.5)

- Max message payload: **4 KiB** (one page). Enforced in `Message::new`.
- Queue depth: **16 messages per endpoint**. Fixed in v1; per-endpoint depth is v2.
- Worst-case queue memory: 64 KiB per endpoint.

## Cross-core send flow (§8.4)

1. `syscall::dispatch::handle_send` validates the cap.
2. Calls `routing::enqueue(endpoint, msg, cap_gen)`.
3. `enqueue` returns `Ok(Some(blocked_receiver_task_id))` if a task was blocked on recv.
4. Dispatcher calls `smp::ipi::send_ipi(target_core, vectors::WAKE_RECEIVER)` if the receiver is on a different core.
5. The IPI handler on the target core calls `scheduler::wake(task_id)`.

## Endpoint death (§8.6)

`routing::kill_endpoint(id)`:
1. Bumps the generation in the routing table (all outstanding caps on every core become stale).
2. Drains the queue (drops all queued messages).
3. Wakes any sender blocked on a full queue with `EndpointDead` (cross-core IPI if needed).

Generation bump is lazy invalidation: no cap is deleted from remote task tables. Each cap fails on its next use when it loses the generation check. The check is atomic and the bump is visible to all cores after the spinlock release.

## Zero-copy is permanently rejected (§2.5)

All messages are copied: sender buffer → kernel `Message` → receiver buffer. If you are about to add a `share_buffer` syscall, read §2.5 and stop.

## Deadlock warning (§8.9)

In any protocol where A sends to B and B sends to A, at least one direction MUST use `try_send`. Mutual blocking `send` calls are a protocol bug the kernel will not detect. The supervisor's quantum-starvation watchdog is a last resort, not a primary mitigation.
