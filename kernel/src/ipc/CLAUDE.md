# kernel/src/ipc/

Synchronous message-passing IPC (§8). No unsafe code lives here; physical memory and APIC access are done through `memory/` and `smp/ipi.rs`.

## Files

| File           | Responsibility |
|----------------|---------------|
| `mod.rs`       | Public API: re-exports, `init()` |
| `message.rs`   | `Message` (4 KiB max payload, ≤4 embedded caps), `IpcError` |
| `queue.rs`     | `MessageQueue`: fixed-depth FIFO, 16 messages, `enqueue`/`dequeue`/`drain` |
| `endpoint.rs`  | `EndpointId`. The live per-endpoint state is the routing table's `RoutingEntry` |
| `routing.rs`   | The routing table (`TABLE`): `EndpointId → (CoreId, Generation, Liveness, Queue, blocked receiver, count of blocked senders)`, and one blocked-send record per TASK (`PENDING`: the endpoint, an order stamp, the message); `enqueue`, `dequeue`, `call_dequeue`, `kill_endpoint`, `take_blocked_sender`, `forget_blocked_sender`, `take_call_waiter`. Protected by `SpinLock<[RoutingEntry; MAX_ENDPOINTS]>`, `MAX_ENDPOINTS` = 96. |
| `names.rs`     | Name → `EndpointId` directory. `register(name, ep)`, `lookup(name)`. Protected by `SpinLock<[NameEntry; MAX_ENTRIES]>`. |
| `routing_model.rs`, `names_model.rs` | Host-test models of the two tables (`lib.rs`, `#[cfg(test)]`); not in the kernel binary |

## Message size and queue depth (§8.5)

- Max message payload: **4 KiB** (one page). Enforced in `Message::new`.
- Queue depth: **16 messages per endpoint**. Fixed in v1; per-endpoint depth is v2.
- Worst-case queue memory: 64 KiB per endpoint.

## Cross-core send flow (§8.4)

1. `syscall::dispatch::handle_send` validates the cap.
2. Calls `routing::enqueue(endpoint, msg, cap_gen, Some(my_slot))`.
3. `enqueue` returns `Ok(Some(receiver_slot))` if a task was blocked on recv; on a full queue it records the sender as blocked under the same lock - behind every sender already waiting, never over one (`backlog/90` D1) - and returns `QueueFull`, and the sender blocks. A dequeue that frees a slot moves the OLDEST waiter's message in and wakes it with 0: its message was delivered.
4. Dispatcher calls `scheduler::wake_by_slot(receiver_slot, 0)`, which marks the task Ready and sends the cross-core IPI when the receiver is on another core.

## Endpoint death (§8.6)

`routing::kill_endpoint(id)`:
1. Marks the entry Dead and bumps its generation in the routing table.
2. Drains the queue (drops all queued messages, and says how many were lost).
3. Returns the blocked receiver and the oldest blocked sender; the kill path (`scheduler::kill_task_by_slot`) wakes them with `EndpointDead`, drains every other blocked sender with `take_blocked_sender(id)` the same way, then drains `take_call_waiter(id)` and wakes every caller blocked in a `Call` awaiting this endpoint with `ReplyDead` (§8.6). It also marks the resource dead in the global capability table, which is what makes outstanding caps fail. A task that dies while blocked SENDING is withdrawn first (`forget_blocked_sender`), so a later dequeue cannot wake its slot's next occupant; its never-accepted message is dropped and counted.

Generation bump is lazy invalidation: no cap is deleted from remote task tables. Each cap fails on its next use when it loses the generation check. The check is atomic and the bump is visible to all cores after the spinlock release.

## Zero-copy is permanently rejected (§2.5)

All messages are copied: sender buffer → kernel `Message` → receiver buffer. If you are about to add a `share_buffer` syscall, read §2.5 and stop.

## Deadlock warning (§8.9)

In any protocol where A sends to B and B sends to A, at least one direction MUST use `try_send`. Mutual blocking `send` calls are a protocol bug the kernel will not detect. The supervisor's quantum-starvation watchdog is a last resort, not a primary mitigation.
