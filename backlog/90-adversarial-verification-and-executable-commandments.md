# 90. Adversarial verification of the kernel, and the commandments made more executable

**Status: OPEN, 2026-10-15.** Not started. Recorded at the operator's request as the brief for a
future piece of work; nothing below has been examined yet, and no claim in it is a finding.

## The brief

Run a rigorous, adversarial, lightweight formal verification exercise on the whole kernel, with two
aims:

1. Find out whether the kernel preserves its architectural, security and correctness invariants.
2. Turn the suitable invariants into checks that run repeatedly without AI reasoning.

The commandments are to end up as a mix of human-readable specification, precise invariants,
architectural checks, model-checked transitions, implementation-level verification, regression and
fault-injection tests, and machine-checked proofs where they are practical.

The stance is adversarial: challenge the implementation, look for counterexamples, name proof gaps.
Treat `CLAUDE.md`, `COMMANDMENTS.md` and `docs/ENGINEERING_ASSURANCE.md` as specifications to
investigate, not as evidence that their claims hold. **Never say the kernel is formally verified**
unless a machine-checked proof establishes the property in question.

## Scope

The trusted kernel: Rust and assembly, every `unsafe` block and its safety contract, syscall entry
and exit, memory ownership, mapping and isolation, IPC queues, blocking and cancellation, capability
lookup, transfer, attenuation and revocation, task creation, scheduling, termination and cleanup,
interrupt handling and reentrancy, SMP synchronisation and memory ordering, each `arch/<isa>`, and
the kernel's interaction with the supervisor. Name the real TCB and the assumptions outside the Rust
kernel. A result on one architecture says nothing about another.

## Work, in priority order

1. **Commandment inventory and traceability matrix.** For each commandment: what it requires or
   forbids; whether it is safety, liveness, an architectural constraint or a policy; whether it can
   be stated mathematically, checked statically, dynamically, model-checked or proved against code;
   whether human review is still needed; what already enforces it; the smallest useful addition.
   Columns: Commandment / Formal property / Existing enforcement / Proposed verification / Coverage
   limits. Do not force every rule into notation.
2. **State models per MISCIS responsibility**: state variables, domains, ownership, operations,
   pre- and postconditions, failure, cancellation, concurrent, cleanup and restart transitions. For
   each invariant I check `Init(s) => I(s)` and `I(s) AND T(s, s') => I(s')`, and name every
   transition that can break it. Failed operations are not assumed atomic. Keep the abstract model
   and the implementation visibly apart.
3. **Capabilities first (highest priority).** No ambient authority, no unauthorised creation, no
   escalation, correct attenuation, explicit transfer, revocation, no use after revocation, no
   resurrection through identifier reuse, no escalation through restart, no authority through a
   kernel interface. Start from `Authority_after(t) ⊆ Authority_before(t) ∪ ValidGrants(t)` and
   refine it for object identity, delegation, attenuation and revocation. Hard cases: revocation
   during transfer, death during transfer, recycled identifiers, concurrent table updates, failed IPC,
   restart of a privileged service.
4. **Memory**: isolation, page ownership, map and unmap, stale mappings, use-after-free, double free,
   kernel/user split, page-table lifetime, address arithmetic, per-arch protection. Rust safety does
   not establish the correctness of `unsafe` or of a hardware mapping.
5. **IPC**: queue bounds (`0 ≤ len ≤ capacity`), message size, ownership, sender and receiver
   lifetime, wakeup, cancellation, exhaustion, death during IPC, transfer during IPC, partial-failure
   cleanup, deadlock and starvation. A queue bound proves nothing about integrity or liveness.
6. **Scheduling**: task states, run-queue membership, duplicate scheduling, lost wakeups, preemption,
   timer accounting, termination. Safety and liveness kept apart; liveness only under stated fairness
   assumptions; no real-time claims.
7. **Interrupts and SMP**: routing, reentrancy, lock ordering, atomics, barriers, cross-core
   visibility, per-arch ordering. Only concurrency paths that exist in the code.
8. **Architecture-by-property matrix** covering privilege levels, page-table semantics, TLB
   invalidation, interrupt masking, atomics, ordering, context switch, boot and ABI. Unsupported,
   unexamined and unproven marked as such.

## Executable verification

Decide whether a host-side harness is needed at all; prefer extending what exists. A
`verification/` tree and `verify capabilities | ipc | scheduling | all` subcommands are proposals,
not existing interfaces, and the front door would more naturally be `osdev` than a new binary. It
stays outside the kernel: verification must not become a seventh responsibility.

Report results at separate levels, never one pass/fail: ARCHITECTURAL CHECK, PROPERTY TEST, MODEL
CHECK, IMPLEMENTATION VERIFICATION, MACHINE-CHECKED PROOF, UNVERIFIED. Reasoning results are a
separate scale: COUNTEREXAMPLE FOUND, ARGUMENT SUPPORTED UNDER STATED ASSUMPTIONS, PROOF GAP, NOT
EXAMINED.

**Mutation.** For each important property, derive the invariant from the commandment independently
of the code, then inject a fault in a disposable branch or harness (an unauthorised transfer, a queue
past capacity, a duplicate run-queue entry, a revoked identifier reused, a skipped ownership check)
and confirm the verifier catches it. A verifier that misses an injected violation is not credited
with covering it.

**Tools**, chosen by property and only if actually run: TLA+ (concurrent state machines), Alloy
(relational authority), Kani (bounded Rust), Verus (deductive Rust), property-based testing, and the
existing self-checks.

## Phases

1. Inventory and analysis: read, list existing enforcement, define invariants, hunt counterexamples,
   record gaps.
2. First executable property: capability non-escalation and revocation, unless the code shows
   something more urgent. A small reference state machine that generates valid and invalid
   operations, applies them to the model and to a harness over the real implementation, compares
   observable results, checks invariants after each step, and gives reproducible counterexamples,
   including one deliberate failing case. If the real functions cannot run on the host, say so and
   propose the smallest justified test seam; testing the model is not testing the kernel.
3. IPC: transfer, cancellation, termination.
4. The remaining responsibilities, only once the earlier work is stable and useful.
5. Fast deterministic checks into CI; expensive bounded runs and hardware in separate jobs; results
   kept by commit SHA.

## Deliverables

- **A. Verification report**: commit SHA and scope, TCB and assumptions, the traceability matrix,
  state models, transition analysis, counterexamples, proof gaps, per-arch findings, `unsafe`
  obligations, recommended corrections, a machine-verification roadmap.
- **B. Executable verification proposal**: what exists, what can be automated, mechanisms, access
  needed, CI, maintenance cost, limits.
- **C. Prototype**: the smallest meaningful capability verifier, reproducible, documented, runnable
  without AI, reporting its real scope, with a negative test, and no change to kernel behaviour.
- **D. Checkpoint**: SHA, files examined, invariants established, transitions left, findings,
  assumptions, open obligations, next step.

Each finding gives the source location, commandment, invariant, violating transition or gap, a
concrete counterexample where possible, assumptions, severity, the minimal correction and the
regression check. Speculation is never reported as a confirmed vulnerability.

## Constraints

No kernel growth beyond MISCIS, no ambient authority, no weakening of capability security. Tests are
not proofs and a model is not the implementation. No production code change without the operator's
approval: per the standing rule, code defects found go into ONE entry (this one, or one the operator
approves) and are fixed separately. No elaborate framework. If resources run short, a trustworthy
invariant inventory, concrete counterexamples and one executable prototype come before a shallow
pass over everything.

## What already exists, to reuse rather than duplicate

Not yet surveyed in detail; the starting list:

- `scripts/commandments.py` and `scripts/commandments_redteam.py` (the commandments as gates, and
  probes that break each check), with `COMMANDMENTS.baseline.toml`.
- The `arch_*`, `port_scope`, `shared_surface`, `unsafe_check`, `one_way` and `quit_wait` checkers.
- `#![deny(unsafe_code)]` on every service, example and `osdev`.
- The §22 suites in `osdev/src/validator.rs`: identity (24 cases), property P1-P10, fuzz F1-F8,
  adversarial A1-A15, chaos, stress.
- A `mutants.out/` from an earlier `cargo-mutants` run (gitignored), which may show what the tests
  already miss.
- `docs/ENGINEERING_ASSURANCE.md`, which already lists no formal verification and no independent
  review among its limitations.

## Next step

Phase 1 only: read the kernel against the commandments and produce the traceability matrix and the
capability state model, with the reviewed SHA recorded. Cost: a long session of reading, no code.
