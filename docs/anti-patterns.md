<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Field Guide to Constitutional Violations

These are **architectural** violations, not compiler errors. Most compile; many even work; they violate
the model in [`../COMMANDMENTS.md`](../COMMANDMENTS.md) and [`../CLAUDE.md`](../CLAUDE.md), which no
compiler checks. Architectural corruption rarely begins with catastrophe - it begins with one
reasonable-looking compromise (`COMMANDMENTS.md`, "Just this once"), so the danger is exactly that these
look sensible.

## How to use this guide

- **As an author, before you open a PR:** scan the categories your change touches and check you did the
  right-hand column, not the left.
- **As a reviewer (human or AI):** this is the checklist. A rule you can *name* is a rule you *catch*;
  the whole point of this file is to make each violation class legible, the way §8.9's "at least one
  direction MUST use `try_send`" is legible.
- Each category names the **Commandment(s) and section(s)** it enforces (`Grounds:`). Each row pairs the
  **violation** (looks reasonable) with **the correct pattern**. When a violation and this guide
  disagree with the constitution, the constitution wins and this guide is amended to match
  (`CLAUDE.md` §1).

This catalog is deliberately exhaustive; it is a reference, not a read-through.

---

## Responsibility Leaks

A component takes on responsibility that belongs elsewhere.
**Grounds:** Commandment I (the kernel is complete) + X (complexity in the layer that owns it); §4.3-§4.4
(kernel scope and anti-scope), §26.10 (the kernel is mechanism, not policy).

| Violation | The correct pattern |
|-----------|---------------------|
| Kernel parses TOML service contracts. | `osdev` validates contracts at build time (§13.4); the supervisor reads them; the kernel only mints caps *from* the parsed result at spawn (§13.6). |
| Kernel knows service names beyond the supervisor. | The kernel holds one minimal `name -> EndpointId` recovery directory (`ipc::names`, §3.7); all *policy* about names lives in the supervisor. |
| Kernel decides restart policy. | The supervisor holds restart authority (§14.4). The kernel's one restart act is respawning the supervisor itself (§6.2) - the last-resort anchor, not a policy engine. |
| Kernel retries failed IPC automatically. | The kernel returns the failure (`EndpointDead`/`CapRevoked`); the *client* retries, degrades, or fails (§14.3). Hidden kernel retries are a silent fallback (§26.4). |
| NIC driver parses DHCP packets. | DHCP is policy for the network stack (a service), not the driver. The driver moves frames; `net-stack` interprets them (`docs/networking.md`). |
| Filesystem performs authentication decisions. | Authority is a capability the caller already holds (§7); `fs` enforces `op <= right` on the cap, it does not authenticate identities. |
| Shell caches network configuration. | The shell queries the owner (`net-stack`) and reflects the live answer; it stores no second copy (see Duplicate Truth). |
| Scheduler contains networking logic. | The scheduler picks the next task and nothing else (§9); networking is a service. |

---

## Architecture Boundary Leaks

Shared (arch-neutral) kernel code learns ISA-specific details.
**Grounds:** the single `arch::imp` seam (`kernel/src/arch/CLAUDE.md`, `docs/multi-arch.md`), §18.1;
mechanically enforced by `scripts/arch_boundary_check.py`.

| Violation | The correct pattern |
|-----------|---------------------|
| Shared kernel imports `arch::x86_64::`. | Name only `arch::imp::`; a specific arch named outside `arch/` is exactly the leak the CI guard rejects. |
| Generic code knows about the APIC (or any interrupt controller). | Reach it through an `arch::imp` primitive (e.g. `send_ipi`, `eoi`); the controller is arch-specific. |
| Generic code assumes a page size. | Take the page size from the arch layer; do not hardcode `4096` in neutral code. |
| Common allocator assumes a cache-line size. | Parameterize from the arch layer; do not bake a constant into neutral code. |
| Driver or neutral code assumes little-endian layout. | Convert explicitly (`to_le_bytes`/`from_le_bytes`); the neutral kernel compiles big-endian (s390x, `docs/multi-arch.md`), so implicit endianness is a bug. |
| Neutral code uses `core::sync::atomic::AtomicU64`. | Use `portable_atomic::AtomicU64` - the only thing that stood between the kernel and a 32-bit target (`docs/multi-arch.md`, "Word size"). |
| IPC depends on the interrupt implementation. | IPC calls an `arch::imp` wakeup primitive; it does not know how the IPI is delivered. |
| Shared code references architecture-specific registers. | Wrap the register access in an `arch::imp` function; neutral code calls the wrapper. |

---

## Duplicate Truth

Two places claim to own the same fact.
**Grounds:** Commandment III; §3.8 (state explicit and owned), §26.4 (caches must be visible and
reconcilable, never a second truth), Invariant 9.

| Violation | The correct pattern |
|-----------|---------------------|
| A cache of capabilities separate from the capability table. | The kernel cap table is the one truth; a derived view must reduce to it and reconcile (§26.4). On disagreement, the table wins. |
| Supervisor and another service both store the current leader. | One owner holds the fact; everyone else queries or is notified. Two stored copies that can diverge is a second irreducible truth (forbidden). |
| Kernel remembers mounted filesystems. | `fs` owns mount state; the kernel knows only opaque `ResourceId`s and owners (§4.4, §7.10). |
| `net` caches the IP address instead of querying the network owner. | Query `net-stack` (or reflect its notification); do not keep a second authoritative IP that can drift (this is the exact `net` bug fixed in the networking robustness pass). |
| Driver stores configuration already owned elsewhere. | Hold only what the driver irreducibly owns (device registers); derive the rest from the owner. |
| Multiple sources of current time. | One clock source (the RTC/TSC via the kernel, §clock); everything else derives from it. |
| Cached health/status instead of querying the service. | Serve the live answer, or a *reconciled* view; never present a stale cache as authoritative (Honest Truth). |

A stored cache/index/count is fine **if** it reduces to one source, reconciles when it drifts, and is
subordinate (the source wins). The `fs` free bitmap and free count are legitimate exactly on those terms
(§26.4). The test is not "is it stored?" but "does it reduce to one source, and does that source win?"

---

## Ownership Violations

Responsibility for data is unclear.
**Grounds:** Commandment VI; Invariant 2 (no shared mutable memory), Invariant 9 (no unowned global
mutable state), §3.8-§3.9.

| Violation | The correct pattern |
|-----------|---------------------|
| Two services mutate the same object. | Each service owns its state; they exchange copies over IPC. Shared mutable memory is invisible coupling (Invariant 2). |
| A borrow escapes its owning service. | Data crosses a service boundary only as a *copied* message (§8.5); a reference into another service's address space cannot exist by construction. |
| Shared mutable singleton. | Expose the state *through* a service (Commandment VI); a singleton is an anonymous owner (§3.8). |
| A temporary global becomes permanent. | Per-task state lives on the task's stack/arena; if it must be shared, it belongs to a service, not a `static mut`. |
| Service modifies another service's state directly. | Send a request; the owner mutates its own state. No back doors (Commandment VII). |
| Mutable state shared across unrelated components. | One owner, explicit messages. If two components need the same fact, one owns it and serves it. |

---

## Capability Violations

The capability model is weakened.
**Grounds:** Commandment VII; §7 (the capability system), §7.3 (non-escalation), §7.6 + §8.5 (transfer /
GRANT), §7.5 (revocation). The transfer three-checks are in §8.5.

| Violation | The correct pattern |
|-----------|---------------------|
| A capability silently duplicated. | Authority *moves* on transfer and is removed from the sender's table (§8.5); it is not copied behind the scenes. |
| A capability widened unnecessarily (or over-granted). | Grant the least rights the receiver needs (§7.3); do not pass `GRANT` onward unless re-delegation is intended. Rights only narrow. |
| Ambient authority introduced. | Every privileged action requires a held capability; there is no authority by identity, ancestry, or a hardcoded name (§3.1, Invariant 1). |
| A capability transferred without explicit authority. | Transfer requires the `GRANT` right, or the send is rejected `CapNotGrantable` (§7.7). Do not assume a transfer succeeded (§8.5). |
| A capability reused after transfer moved it out of your table. | After a successful transfer the handle is no longer yours; re-derive if you need one. On a *failed* transfer, reclaim the cap so a slot does not leak (§26.6). |
| A capability never revoked. | The owner revokes on end-of-life (`resource_revoke`, generation bump, §7.5); every outstanding cap goes stale. Nothing outlives its resource silently. |
| The capability system bypassed entirely. | A syscall that touches a resource validates a capability first (§21). No privileged path skips the check. |

---

## Contract Violations

The explicit contract is bypassed or false.
**Grounds:** Commandment IV; §13 (service contracts), §13.6 (runtime enforcement from the contract),
Invariant 7.

| Violation | The correct pattern |
|-----------|---------------------|
| Use authority the contract never declared. | Declare it. A cap not in the contract is not minted at spawn; using it fails `CapNotHeld` (§13.6). Redesign the contract, not a back channel. |
| Behavior exceeds what the contract states. | The contract is the honest description of what the service does and needs; keep them in lockstep. |
| Communicate outside declared endpoints. | All IPC goes through declared `ipc_send`/`ipc_receive` peers; there is no hidden path (Commandment IV). |
| A contract that over-declares "just in case". | Declare the *minimum*; that minimum is the security boundary. Unused authority is latent risk. |
| Change behavior without amending the contract. | The contract changes with the behavior; a stale contract lies to the reviewer and the OS. |

---

## Hidden Coupling

Components depend on each other without an explicit contract.
**Grounds:** Commandment IV + X; §13 (contracts), §26.4 (no silent complexity).

| Violation | The correct pattern |
|-----------|---------------------|
| Shell depends on logger output formatting. | Depend on a stable interface (a typed record / a documented protocol), not on how a peer happens to render text today. |
| Filesystem assumes the block size. | Query it from `block-driver`, or negotiate it; do not bake a peer's internal constant into your logic. |
| Driver knows the scheduler quantum. | Wait on the hardware event, not on "roughly a quantum" of time (Commandment VIII). The quantum is not your contract. |
| Network stack knows the NIC vendor. | The driver abstracts the device; the stack talks a device-neutral interface. Vendor specifics stop at the driver. |
| Service depends on boot order. | Reacquire dependencies by name and retry; do not assume "X started before me" (§14.3). See Temporal Violations. |
| Consumer assumes a provider's implementation details. | Depend only on the declared interface; implementation is the provider's to change. |

---

## Lifecycle Violations

Objects outlive their valid lifetime.
**Grounds:** Commandment V + IX; §14 (lifecycle), §8.6 (endpoint death), §7.5 (generation), §14.3.

| Violation | The correct pattern |
|-----------|---------------------|
| A waiter survives its owner's death. | A caller blocked awaiting a reply wakes with `ReplyDead` the instant its replier dies (§8.6); a blocked sender wakes with `EndpointDead`. Never a wait that cannot observe death. |
| A future/handle bound to a dead endpoint. | The generation check makes it stale on next use (§7.5); do not hold a handle as if liveness were permanent. |
| Restart keeps stale handles. | On restart, every handle/id/generation from the previous incarnation is stale; re-establish them (§14.3). |
| A queue contains references to destroyed objects. | A destroyed endpoint's queue is drained and its generation bumped (§8.6); references do not linger. |
| A callback executes after service exit. | The callback is a capability; when the service dies its generation bumps and the callback goes stale (§7.5). |
| A timer survives a restart incorrectly. | Re-establish timers on spawn; a restarted service assumes nothing carried over (Commandment V/IX). |
| A resource reclaimed before all users release it. | Reclaim on generation bump so every holder learns via `CapRevoked`; do not free under live holders silently. |

---

## Recovery Violations

Recovery succeeds only on the happy path.
**Grounds:** Commandment IX + V; §14.2-§14.3 (restart + cap rebinding), §6.2, §7.5.

| Violation | The correct pattern |
|-----------|---------------------|
| A retry assumes the same service instance still exists. | Reacquire the peer *by name* (a fresh instance, new generation) and retry (§14.3). |
| Restart skips rebuilding dependent state. | Reconstruct all state on spawn from its durable source; assume nothing survived (Commandment V, `examples/counter`). |
| A handle reused across service generations. | Reacquiring the endpoint is not enough: a socket/id/generation/cached value from the *dead* incarnation is stale too, and must be re-established (§14.3). |
| Recovery skips capability validation. | The reacquired cap is validated like any other; recovery is not a trusted path. |
| Client assumes reconnect is transparent. | Reconnect is explicit: `EndpointDead` -> reacquire-by-name -> retry. The client owns this (§14.3). |
| A dependency restart is ignored. | Treat `EndpointDead`/a missed reply as the signal to reacquire; ignoring it means operating on a dead peer. |

---

## Temporal Violations

Correct operation at the wrong time.
**Grounds:** Commandment VIII; §8.6 (queued, not processed), §7.5 (generation settles races), §11
(bootstrap ordering).

| Violation | The correct pattern |
|-----------|---------------------|
| Read configuration before the service is ready. | Wait on the observable readiness signal (a reply, a registration), not on elapsed time. |
| Publish an endpoint before initialization completes. | Register only once fully initialized; a client that reaches an unready service is a race. |
| Destroy an object before consumers finish. | Revoke via generation bump so consumers learn (`CapRevoked`); do not free under live use. |
| Notify before state is committed. | Commit the durable state first, then notify; a notification is a promise the state exists. |
| Assume an event ordering that is not guaranteed. | A successful `send` means *queued*, not processed (§8.6); build an explicit acknowledgment if you need ordering. |
| Service accepts requests before fully initialized. | Gate the serving loop on completed bring-up; degrade loudly if bring-up failed (`examples/driver-skeleton`). |

---

## Identity Violations

Identity confused with implementation/location.
**Grounds:** Invariant 11 (identity is stable; location is not), §7.5 (generation), §14.2 (cap
rebinding), §26.8.

| Violation | The correct pattern |
|-----------|---------------------|
| Store a raw pointer instead of a capability. | Hold a capability; a pointer is a location, and location is not identity (Invariant 11). |
| Persist a transient identifier. | Persist the stable *name*; resolve it to a fresh endpoint after restart (identity survives, instances do not). |
| Compare addresses instead of identities. | Compare by name/capability; two instances of the same identity have different addresses. |
| Trust a PID/task id after restart. | The task id is a fresh instance; reacquire by name. Trusting the old id is trusting a dead instance. |
| Ignore the service generation. | The generation check is the identity-vs-staleness test (§7.5); ignoring it uses a stale incarnation. |
| Cache an endpoint identity indefinitely. | Endpoints are rebound across restart (§14.2); a cached endpoint goes stale and must be reacquired. |

---

## Hidden Assumptions

Reality is assumed instead of observed.
**Grounds:** Commandment VIII (wait on truth) + V (no service is special); §2.4 / Invariant 12 (loud
failures), §3.12, §26.7.

| Violation | The correct pattern |
|-----------|---------------------|
| Network always exists. | Query it; degrade loudly when it is absent (report "no link", never a fabricated status). |
| Filesystem is always mounted. | Check the reply status; when `fs` is unreachable, say so and degrade, do not assume success. |
| Logger never fails. | A send can fail; handle the error. The ring buffer + serial are the fallback the kernel owns, not an assumption you make. |
| Supervisor never dies. | The supervisor is restartable; the kernel respawns it (§6.2). Nothing above the kernel is assumed immortal. |
| Memory allocation always succeeds. | `AllocDenied` is a recoverable result (§10.4); handle it and degrade. |
| Timer never jumps backwards. | Use monotonic sources for intervals; do not assume wall-clock monotonicity. |
| Driver always initializes successfully. | Bring-up can fail; on failure, log loudly and degrade/idle, never proceed as if the device is up. |

---

## Silent Failure Violations

Failure becomes invisible.
**Grounds:** Commandment V + VIII; Invariant 12 (failures are loud), §26.7 (loud failure over hidden
recovery), §26.6 (bounded).

| Violation | The correct pattern |
|-----------|---------------------|
| Swallow a timeout. | Surface it; a timeout is a fact an operator needs. Prefer waiting on truth so failure arrives as an event (§8.6). |
| Ignore an allocation failure. | Read the `AllocDenied` result and degrade or report (§10.4). |
| Infinite retry loop. | Bound the retries and report exhaustion (§26.6); an unbounded loop hides a stuck dependency. |
| Infinite wait. | Wait on a truth that *includes failure* (`ReplyDead`, `EndpointDead`); a pure success-wait is an infinite wait on time (Commandment VIII). |
| A background task exits silently. | Its exit is logged; a service that dies quietly is a failure no one sees. |
| Recover without reporting the degraded state. | A recovery that itself fails is still a failure - surface it; never proceed as if a failed retry succeeded (§26.7). |

---

## Honest Truth Violations

The system reports convenience instead of reality.
**Grounds:** Commandment V; Invariant 12 (loud failures), §2.4, §26.7.

| Violation | The correct pattern |
|-----------|---------------------|
| Fake fallback IP address. | Report "unassigned"; a fabricated address is a lie the caller will act on. |
| Report healthy before validation. | Report status only after observing it; "probably fine" is not a status. |
| Cached status presented as live. | Label a cache as a cache, or query live; a stale value dressed as current is dishonest (Duplicate Truth). |
| Report connected after the cable is unplugged. | Reflect the live link state; the `net` view gates every line on the real link (networking robustness pass). |
| Guess a dependency's state instead of querying it. | Ask the owner; a guess presented as truth is a reasonable lie. |
| Invent a default configuration silently. | Absence is a fact: report it. A silent default hides that the real config never arrived. |

---

## Convenience Shortcuts

Shortcuts that weaken the architecture.
**Grounds:** Commandment I + V + X; §26.5 (explicitness over magic), §3.9, §21 (the reject list).

| Violation | The correct pattern |
|-----------|---------------------|
| A global supervisor pointer. | Reach the supervisor via a capability/IPC; a global pointer is ambient authority and unowned state. |
| Kernel restarts services directly (as policy). | The supervisor restarts services; the kernel only respawns the supervisor (§6.2). Do not move that policy into the kernel for convenience. |
| Global mutable configuration. | Config is owned by a service and read over IPC; a mutable global is an anonymous owner (§3.9). |
| Direct singleton access instead of IPC. | Cross a service boundary only by IPC; a shared singleton destroys isolation. |
| A temporary bypass that becomes permanent. | If a bypass is needed, it has an owner, a rationale, and a removal plan; "temporary" without those is permanent (§26.5). |
| Hardcoded boot ordering. | Reacquire by name and retry; do not encode "X is always up before me" (§14.3). |

---

## "Reasonable Lies"

Code that looks sensible but violates the architecture. (The most dangerous category: each entry is the
convenient default a careful author reaches for.)
**Grounds:** Commandment III + V + I; §26.4, §26.7, §4.4.

| Violation | The correct pattern |
|-----------|---------------------|
| Invent fallback values instead of reporting absence. | Report absence loudly; a fallback value is a fabricated truth (Honest Truth). |
| Store "just in case" global state. | Store only what a real need requires (§26.2); speculative state is unowned risk. |
| Cache authoritative information owned elsewhere. | Query the owner or hold a reconciled, subordinate view (§26.4); never a second authority. |
| Make the kernel "helpful" by learning policy. | The kernel is mechanism; policy lives in services (§26.10). A helpful kernel is a scope leak. |
| Introduce hidden retries instead of explicit recovery. | Recovery is the client's explicit reacquire-and-retry (§14.3); a hidden retry is invisible complexity (§26.4). |
| Hardcode special cases for "important" services. | No service is special; only the kernel is (Commandment V). A "critical services" list duplicates the supervisor's policy and plays favorites. |
| Remember convenience state rather than querying truth. | Query the owner; convenience state drifts and becomes a second truth. |

---

## Untried by Fire

Shipping without passing through the fire (the inverse of Testing Violations: not *bypassing* the tests,
but not *facing* the hard paths).
**Grounds:** Commandment II; §22 (the seven trials, `chaos max-carnage`), Commandment IX ("if recovery
cannot be tested, it does not exist").

| Violation | The correct pattern |
|-----------|---------------------|
| Ship a service that never faced `chaos max-carnage`. | Every service survives kill/flood/spawn/memory storms before "done" (Commandment II). If Chaos finds a bug, it already existed. |
| A recovery path with no test. | Recovery you cannot test does not exist (Commandment IX); pin it (see §22 Test 13, `fs` survives its own restart). |
| Disable or weaken an identity/chaos test to green a build. | A failing identity test is a real bug or a spec change; fix the code or amend the constitution (§22.1) - never mute the test. |
| Assume a race cannot happen because unit tests pass. | A green unit test is necessary, never sufficient (§22.2); the property/stress/chaos layers exist because it is not. |

---

## Unbounded Behavior

Growth or work with no ceiling.
**Grounds:** §26.6 (bounded behavior), §26.6.1 (stack and arenas, not heap), §8.5 (fixed queue depth).

| Violation | The correct pattern |
|-----------|---------------------|
| A table/queue/buffer that grows without a fixed ceiling. | Fixed capacity, loud on overflow; queue depth is 16, fixed (§8.5). A bound reached loudly is a feature (§26.6.1). |
| Reach for the heap where a bounded arena or stack was required. | Default to fixed stack arrays / bounded arenas / streaming in fixed chunks (§26.6.1); change the representation, do not add a heap. |
| A retry with no cap. | Bound the retries and report exhaustion (§26.6). |
| Recursion with no bounded stack. | Iterate with an explicit bounded stack (§26.6.1); unbounded recursion overflows into a guard page. |
| Cap-table entries accumulated and never reclaimed. | `remove_cap` when a cap is no longer needed; a long-running server stays bounded (§26.6, `examples/reply-server`). |

---

## Understandability Erosion

Complexity that breaks the 30-minute whiteboard rule.
**Grounds:** §26.11 (understandability is a hard requirement), §26.13 (discipline over cleverness),
Commandment X.

| Violation | The correct pattern |
|-----------|---------------------|
| Abstraction for its own sake. | Add abstraction only when a real need pulls it into existence (§26.2); a boring explicit design beats a clever one (§26.13). |
| Framework-style indirection / meta-programming layers. | Prefer direct, inspectable code; layers you cannot whiteboard are over budget (§26.11). |
| Behavior smeared across many components. | Keep a responsibility in the one layer that owns it (Commandment X); if explaining it needs a dozen components, simplify. |
| Cleverness where boring-and-explicit was available. | The preferred implementation is boring, explicit, testable, restartable (§26.13). |

---

## Discipline / Process Violations

The code is fine; the *change* corrupts the model.
**Grounds:** §21 (the instant-reject list), §26.2 (features are pulled into existence, not pushed),
§2.5 (permanent decisions), §1 (amend with a recorded rationale).

| Violation | The correct pattern |
|-----------|---------------------|
| Edit `CLAUDE.md` without a rationale. | Every constitutional edit carries a rationale in the commit (§21); the law is not changed silently. |
| Add a feature speculatively ("might be useful later"). | Features are pulled into existence by a real need - an invariant, a test, an operational problem (§26.2). The default is to defer. |
| Sneak back a permanently-rejected decision. | Zero-copy IPC, live code update, work-stealing, and service migration are permanently rejected (§2.5, §21); reintroducing one is an instant reject. |
| An em-dash or en-dash anywhere. | Only the ASCII hyphen is permitted, repo-wide (§21); box-drawing characters are fine. |
| A grandfathered `unsafe` count increased without an amendment. | Grandfathered `unsafe` may only decrease, unless an §18.5 amendment records the necessity and safety rationale. |
| Grow the kernel because it is convenient. | Ask "why isn't this a service?" (Commandment I). Refuse the responsibility and be rewarded. |

---

## See also

- [`../COMMANDMENTS.md`](../COMMANDMENTS.md) - the Ten Commandments, the human-readable law each category
  enforces.
- [`../CLAUDE.md`](../CLAUDE.md) - the constitution; §21 is the instant-reject list, §26 is the
  architectural-discipline chapter these categories draw on.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) - how to contribute, and the "Where do I start?" map.
- [`../examples/`](../examples/) - each example teaches the *correct* pattern for a Commandment, in code.
