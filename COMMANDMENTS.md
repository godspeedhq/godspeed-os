# The Ten Commandments of Godspeed

These Commandments define the architectural boundaries of Godspeed. They are **not** coding
guidelines or stylistic preferences. They exist to preserve a system that remains **simple,
recoverable, and understandable** as it grows.

They are the human-readable distillation of the constitution in [`CLAUDE.md`](./CLAUDE.md) — each is
grounded in the invariants and sections it enforces. Where a Commandment and the code disagree, the
code is wrong (`CLAUDE.md` §1). Where a Commandment and the constitution disagree, the constitution
is the law and this document is amended to match.

---

## Respect Godspeed

### I. Thou shalt not expand the responsibilities of the kernel. It is complete. Use a service.

The kernel's responsibilities are complete: memory isolation, scheduling, IPC, capability
enforcement, interrupt routing, and cross-core routing — and **nothing else**. New hardware support,
new CPU architectures, and bug fixes are welcome. New *responsibilities* are not.

Before proposing a kernel change, ask: **"Why isn't this a service?"** If the answer is "because it
is more convenient," it belongs outside the kernel.

> *Grounded in:* §4.3–§4.4 (kernel scope and anti-scope), §26.10 (the kernel is mechanism, not
> policy), Invariant 4 (the kernel remains tiny).

---

### II. Thou shalt love Chaos and trust in it. Thy service shall pass through Maximum Carnage.

Passing unit tests does not prove correctness. Every service shall eventually face failure, memory
pressure, restart storms, spawn storms, queue floods, and whatever new forms Chaos invents.

If Chaos finds a bug, **the bug already existed.** Treat Chaos as your teacher, not your enemy.

> *Grounded in:* §22 (the identity, stress, adversarial, and chaos suites — and `chaos
> max-carnage`), §2.3 (execution over theory), §26.7 (loud failure over hidden recovery).

---

### III. Thou shalt not duplicate truth. Store facts. Derive the rest.

Truth must exist in exactly one place. Everything else is derived from it. Duplicated truth
eventually diverges, producing inconsistency, needless complexity, and subtle bugs.

Store only what is necessary. Derive everything else. A counter that can be recomputed is not a
fact; a cache that can drift is a second truth waiting to lie.

> *Grounded in:* §3.8 (state must be explicit and owned), §3.9 / Invariant 9 (no unowned global
> mutable state; immutable globals are fine), §26.4 (no invisible caching layers).

---

### IV. Thou shalt honor service contracts.

Services communicate through explicit contracts. Do not bypass a contract. Do not invent hidden
communication paths. If a service cannot express its needs through its declared contract, **redesign
the contract — not the architecture.**

> *Grounded in:* §13 (service contracts), §3.7 / Invariant 7 (contracts are enforced, not
> interpreted), §13.6 (runtime enforcement from the contract at spawn).

---

### V. Thou shalt not assume thy service is special. Only the kernel is special.

Every service must be prepared to fail. Every service must be prepared to restart. No service is
exempt. If the Supervisor itself must survive its own death — and it must, for the kernel respawns it
— so must yours.

The only unkillable component is the kernel. Everything above it is identity, not location, and
identity survives restart.

> *Grounded in:* §6.2–§6.3 (the supervisor is restartable; the unkillable set is `{kernel}` alone),
> Invariant 6 (services must be restartable), Invariant 11 (identity is stable; location is not).

---

## Corrupt Not Godspeed

### VI. Thou shalt not introduce shared mutable state.

Services communicate through IPC. Shared mutable state creates invisible coupling, destroys
isolation, complicates recovery, and makes failure unpredictable. If multiple services require the
same information, expose it **through a service — not through shared memory.**

> *Grounded in:* Invariant 2 (no shared mutable memory by default), Invariant 9 (no unowned global
> mutable state), §2.5 (zero-copy IPC is permanently rejected for this reason).

---

### VII. Thou shalt not introduce ambient authority.

Authority must always be explicit. A service may perform only the actions its capabilities grant.
Hidden privilege eventually becomes both a security vulnerability and an architectural dependency.

There is no authority by identity, ancestry, or inheritance — only by capability.

> *Grounded in:* Invariant 1 (no ambient authority), Invariant 3 (all authority is explicit), §7 (the
> capability system), §26.9 (authority must remain visible).

---

### VIII. Thou shalt not rely upon timing for correctness. Wait for truth, not time.

Time does not prove correctness. Sleeping longer does not make a race disappear. Correctness must
come from observable truth:

* acknowledgements
* state transitions
* contracts
* events
* capabilities (and their generations)

Time may conserve CPU. It must **never** determine correctness.

> *Grounded in:* §8.6 (a successful `send` means queued, not processed — acknowledgement is explicit),
> §7.5 (the generation check, not a delay, settles a restart race), §9.3 (yield is advisory), §22.4
> (a test waits for observable output, never a fixed sleep).

---

### IX. Thou shalt always plan for recovery, for thy service shall fail.

Failure is not exceptional. It is expected. Every service should recover cleanly without assuming a
perfect world. A client whose dependency restarts must reacquire and retry, not crash.

**If recovery cannot be tested, it does not exist.**

> *Grounded in:* §14 (service lifecycle, restart, and cap rebinding), §14.3 (cascading failure is the
> client's responsibility), §6.2 (death is recovered, not a reboot), §22 (recovery is pinned by the
> chaos and identity suites).

---

### X. Thou shalt place complexity where it belongs.

Complexity is sometimes necessary. **Hidden** complexity never is. Do not move complexity into the
kernel because it is convenient. Do not push complexity onto users because it is easier. Place it in
the layer that naturally owns it.

Good architecture is not the absence of complexity. It is complexity in the correct place.

> *Grounded in:* §26.10 (the kernel is mechanism, not policy), §26.11 (the 30-minute whiteboard
> rule), §26.13 (discipline over cleverness), §26.4 (no silent complexity).

---

## Final Admonition

These Commandments are not independent. Violate one and you eventually violate all.

Architectural corruption rarely begins with catastrophe. It begins with a single compromise:

> *"Just this once."*

Over time, that compromise spreads in subtle and unpredictable ways until the architecture itself
becomes difficult to reason about.

These Commandments were not written for style, tradition, or amusement. They were written in response
to real bugs, real wedges, real failures, and real lessons learned while building Godspeed.

Treat them seriously. Chaos certainly will.

## Blessings

*Godspeed on thy journey.*

* May thy architecture remain uncorrupted.
* May thy contracts remain true.
* May thy services endure failure.
* May truth be uncovered through fire.
