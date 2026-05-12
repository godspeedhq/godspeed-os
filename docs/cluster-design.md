# Cluster Mode Design Notes

> **Status:** Non-normative, far-future. Records architectural reasoning and open questions for a future clustered GodspeedOS. Nothing here is a commitment. This document does not amend the constitution (CLAUDE.md).

---

## 1. Routing table generalization

Current GodspeedOS routing resolves:

```
EndpointId → CoreId
```

In a clustered model, the routing table generalizes to:

```
EndpointId → (NodeId, CoreId)
```

This extends "location" from a core to a (node, core) pair while leaving everything else — EndpointId, generation, liveness, capability structure — unchanged. The invariant "identity is stable; location is not" already encodes this. SMP taught the system that identity and execution location are separate concepts. Cluster mode extends the definition of location; it does not change the philosophy.

The generation mechanism already handles service mobility across nodes correctly. When a service moves to a new node, its endpoint generation is bumped. Clients receive `EndpointDead` on their next send, look up the new endpoint via the registry, and resume with a new cap that routes to `(NodeId=2, CoreId=0)` instead of `(NodeId=0, CoreId=1)`. Client code does not change — the pattern is identical to the existing cross-core restart flow demonstrated by `restart_changes_core_transparently` (§22 Test 10 in CLAUDE.md).

---

## 2. Why the remote IPC API should be distinct

Two options exist for the developer-facing API:

**Option A — Explicit remote semantics:**
```rust
send_remote(endpoint_cap, msg, timeout) -> Result<(), RemoteIpcError>
```

**Option B — Transparent routing:**
```rust
send(endpoint_cap, msg)  // kernel resolves (NodeId, CoreId) internally
```

The existing constitution invariants settle this without needing to re-argue it. "Loud failures, never silent" and "bounded behavior" (§3) rule out Option B: transparent routing means `send()` silently paying network latency and returning errors with semantics the caller was not written to handle. That is a silent fallback, which the constitution forbids.

The deeper reason is what "Ok" means. A successful local `send` guarantees delivery to a queue on this machine. A successful remote `send` guarantees handoff to a transport. These are different contracts — not different performance profiles, but different durability, different failure domains, and different recovery obligations. Pretending they are the same primitive is the architectural mistake transparent-clustering systems have historically made.

**Conclusion:** local IPC keeps its current synchronous semantics. Cross-node IPC uses a distinct API surface with explicit timeout and failure handling. This is not a retreat from "identity over location" — it is an honest representation of a different failure domain using the same capability model.

---

## 3. Developer experience

An application that communicates cross-node is written explicitly cluster-aware — but only at the boundary it crosses. Everything else is unchanged.

**Contract declaration:**

```toml
[capabilities]
ipc_send        = ["pong"]     # local — same-node send, current semantics
ipc_send_remote = ["ledger"]   # explicit cross-node send, different failure domain
```

**SDK call sites:**

```rust
// Local service — identical to today
let pong = ctx.send_cap("pong")?;
pong.send(msg)?;

// Remote service — developer explicitly opts into a different failure domain
let ledger = ctx.remote_send_cap("ledger")?;
ledger.send_remote(msg, Duration::from_millis(500))?;
```

`RemoteSendCap` is a distinct type from `LocalSendCap`. The compiler prevents calling `send()` on a remote endpoint. That is "explicit authority" applied to the network boundary at compile time — the network boundary is visible at three layers: contract review, code review, and compiler.

**The registry is the cluster-aware component, not the application.** When the app calls `ctx.remote_send_cap("ledger")`, the SDK queries the registry, which resolves `"ledger"` to `(NodeId=2, CoreId=0, EndpointId=7, Generation=3)`. The application receives a cap and does not need to know which node.

**Mobility.** If `ledger` restarts on node 3, the client receives `RemoteEndpointDead`, queries the registry, and gets a new cap. The reacquire pattern is identical to the local restart flow. No new error-handling primitives are required.

**Local-only services are unaffected.** An application that never declares `ipc_send_remote` compiles and runs identically whether or not the machine is part of a cluster. Cluster semantics do not leak in.

**Same-node optimisation.** If a service declared `ipc_send_remote` happens to be on the same node as the caller, the kernel can route it as a local send internally. The developer always uses `send_remote` with a timeout; the kernel optimises transparently. Correctness does not depend on the optimisation.

---

## 4. Remote IPC failure semantics

§8.6 of the spec defines failure semantics for local IPC as a table. Remote IPC requires the same treatment. The key variable is **delivery state** — developers writing recovery logic must know whether a message was definitively not delivered, definitively delivered, or unknown.

| Event | Phase | `send_remote` result | Delivery state |
|-------|-------|--------------------|----------------|
| Remote node unreachable | Before transport handoff | `Err(RemoteNodeUnreachable)` | Definitively not delivered |
| Timeout expires | Before transport handoff | `Err(RemoteTimeout)` | Definitively not delivered |
| Remote endpoint dead (generation mismatch) | Any | `Err(RemoteEndpointDead)` | Definitively not delivered |
| Remote queue full, timeout expires while waiting | Waiting for queue space | `Err(RemoteTimeout)` | Definitively not delivered |
| Transport error | After handoff, before remote ack | `Err(RemoteTransportError)` | **Unknown** |
| Timeout expires | After handoff, before remote ack | `Err(RemoteTimeout)` | **Unknown** |
| Remote service crashes after receipt | After delivery | `Ok(())` already returned | Delivered; processing unknown |
| Success | Transport handoff complete | `Ok(())` | Handed off to remote transport |

The **Unknown** rows are unavoidable without an explicit acknowledgment protocol. `Ok(())` from `send_remote` means the message was handed off to the transport layer — it does not mean the remote service received or processed it. Developers who require delivery confirmation must build an acknowledgment into their protocol (a response message back), exactly as §8.6 states for local IPC: *"No delivery guarantee. A successful send means the message was queued, not processed."* The same rule applies cross-node.

This table must be finalized before the `send_remote` API is implemented. Developers will write recovery logic based on which errors mean "safe to retry" vs "unknown state, do not retry blindly."

---

## 5. Flow control across nodes

Local IPC has 16-message bounded queues; a blocking `send` waits at capacity until a slot is available (§8.5). The remote equivalent needs a defined saturation policy.

**Proposed v1 cluster semantics:** the `timeout` parameter covers the full call, including any waiting for remote queue space. If the remote queue is full and the timeout expires before a slot opens, `send_remote` returns `Err(RemoteTimeout)` — delivery state is definitively not delivered (no handoff occurred). This is consistent with the failure table above and requires no new primitives.

**What this does not address:** sustained asymmetric load where a fast sender floods a slow remote receiver. Without a backpressure signal that the sender can act on before the timeout fires, the only tool the sender has is reducing its send rate. Whether a dedicated `RemoteBackpressure` error is useful — signaling "queue full, back off" without starting the timeout clock — is an open question for the protocol design phase.

---

## 6. Message ordering

Local IPC is FIFO per endpoint, guaranteed by the queue discipline (§8.5).

Remote IPC ordering is **transport-dependent and not guaranteed at the `send_remote` API level.** Two consecutive `send_remote` calls to the same endpoint may arrive out of order if:

- The transport does not guarantee ordering (UDP-based).
- A reconnection occurs between the two sends (any stateful transport).

**Implication for protocol design:** any protocol built on `send_remote` that requires ordering must either (a) use a transport that guarantees it (TCP, QUIC), or (b) include sequence numbers in the message payload and reorder at the receiver. The kernel does not provide sequence numbering on the remote path.

This should be stated explicitly in the API documentation for `send_remote` so developers do not assume FIFO and write ordering-dependent protocols that fail silently under reconnection.

---

## 7. The registry problem

The bullet "the registry must become cluster-aware" in the current spec is doing enormous work in one sentence. It covers at minimum:

1. **Distributed name resolution.** How does node A learn about endpoints on node B? Push (nodes announce services on join), pull (query a central directory), or gossip? Each has failure modes.
2. **Node membership tracking.** What nodes are in the cluster? Who decides? How is the membership list kept consistent across nodes?
3. **Handling unreachable nodes.** If the node hosting `"ledger"` is unreachable, what does the registry return? Stale data? An error? How long before a node is declared dead vs partitioned?
4. **Consistency vs availability.** If the registry is the cluster-aware component, it is also the distributed-systems component. A partitioned registry either returns stale data (availability) or blocks (consistency). Neither is free.
5. **Possible consensus requirement.** If multiple nodes can register services, the registry may need distributed consensus (Raft or equivalent) to prevent split-brain registration of the same service name on two nodes simultaneously.

Making the registry cluster-aware is likely the largest single piece of work in cluster mode — comparable in effort to most of v1. It is not an incremental extension of the existing name-lookup table. It is a new distributed system component. Future design work should scope this explicitly rather than treating it as a one-sentence prerequisite.

---

## 8. Transport layer

The remote IPC path requires a transport layer. This is an open design decision. Candidate options:

| Transport | Ordering | Reliability | Overhead | Notes |
|-----------|----------|-------------|----------|-------|
| TCP | FIFO | At-least-once (with retries) | Connection setup cost | Simple; well-understood failure modes |
| QUIC | FIFO per stream | At-least-once | Low reconnect latency | Better for mobile nodes; more complex |
| Custom UDP + retries | Not guaranteed | Best-effort (caller handles) | Minimal | Maximum control; most implementation work |

All candidates should require **mutual TLS (mTLS)** to authenticate both endpoints before any message is exchanged. Without mTLS, a node joining the cluster can forge capability sends — the unforgeability guarantee of the capability system depends on the kernel, but the kernel's identity must be authenticated at the transport layer when the boundary crosses a network.

The transport choice directly determines the answers to the ordering question (§6) and shapes part of the failure semantics table (§4). This decision should be made before any cluster implementation begins, not deferred to implementation time.

---

## 9. TCB authority across nodes

The current spec raises "cross-node TCB definition" as an open question. It deserves more weight: **this is the central security question for cluster mode, and cluster mode cannot ship without a resolved answer.**

The question: does the supervisor on node A have authority over services running on node B?

**Option 1 — Federated supervisors (no cross-node authority).**
Each node has its own supervisor with authority only over local services. Cross-node restarts require a protocol between supervisors — node A's supervisor requests that node B's supervisor restart a service. Neither supervisor holds a capability to directly kill or spawn on the other node.

- Pro: a compromised node A supervisor cannot directly harm node B.
- Con: coordinated restarts require a distributed protocol between supervisors. Who initiates? Who has final authority? This needs its own design.

**Option 2 — Hierarchical supervisors (one cluster supervisor governs all).**
A cluster-level supervisor holds authority over all node-level supervisors. Restart decisions flow from the cluster supervisor downward.

- Pro: single point of control; simpler restart coordination.
- Con: a compromised cluster supervisor compromises the entire cluster. This expands the TCB by one cross-node component and introduces a new single point of failure.

**Option 3 — Capability delegation (services hold cross-node restart caps).**
The supervisor on each node holds a `REVOKE`-style cap that can be delegated cross-node. Cross-node restart authority is granted explicitly, not assumed.

- Pro: consistent with the capability model; authority is explicit.
- Con: cap delegation across nodes requires cryptographic authentication of the cap itself (see §8 on transport). The generation mechanism needs a cross-node equivalent.

No option is obviously correct. The choice shapes the entire cluster security model and cannot be resolved without a dedicated threat-model exercise. Any cluster design document that does not address this question explicitly is incomplete.

---

## 10. Summary of open questions

Before cluster mode can be designed in detail, the following questions must be answered:

| Question | Where addressed | Blocking? |
|----------|----------------|-----------|
| Transport protocol choice | §8 | Yes — shapes failure semantics and ordering |
| Registry consistency model | §7 | Yes — defines what `remote_send_cap("name")` can guarantee |
| TCB authority model across nodes | §9 | Yes — cannot ship without this resolved |
| Flow control / backpressure primitive | §5 | No — can defer, but shapes protocol ergonomics |
| Ordering guarantee level | §6 | No — can be "transport-dependent" with documentation |
| Delivery acknowledgment semantics | §4 | No — "Unknown" is a valid stated answer |
