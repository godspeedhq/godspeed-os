# 90. Adversarial verification of the kernel, and the commandments made more executable

**Status: OPEN, 2026-10-16.** Phases 1 and 2 done against the core kernel at `537e6861`, read-only:
the evidence is `audits/2026-10-16-kernel-verification.md`. 29 kernel defects (7 high) and 6
enforcement-layer defects are listed below, NOT fixed; four executable prototypes exist outside the tree
and are NOT landed. Both wait on the operator. The brief, as recorded on 2026-10-15, follows; only its list of checker
names was spelled out in full, because `scripts/doc_symbols_check.py` could not resolve the short forms.

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
- The checkers `scripts/arch_boundary_check.py`, `scripts/arch_seam_check.py`, `scripts/port_scope_check.py`,
  `scripts/shared_surface_check.py`, `scripts/unsafe_check.py`, `scripts/one_way_check.py` and
  `scripts/quit_wait_check.py`.
- `#![deny(unsafe_code)]` on every service, example and `osdev`.
- The §22 suites in `osdev/src/validator.rs`: identity (24 cases), property P1-P10, fuzz F1-F8,
  adversarial A1-A15, chaos, stress.
- A `mutants.out/` from an earlier `cargo-mutants` run (gitignored), which may show what the tests
  already miss.
- `docs/ENGINEERING_ASSURANCE.md`, which already lists no formal verification and no independent
  review among its limitations.

## Progress

**2026-10-16, phases 1 and 2 (and most of phase 3), read-only.** Nine adversarial readers (two for
capabilities, one each for IPC, scheduling, memory, and SMP with interrupts, plus the syscall surface, the
traceability matrix and the existing tests), four prototype
builders, up to three skeptics per finding, a completeness critic and eight follow-ups: 239 raw findings,
184 upheld, 28 killed, 27 argued. No QEMU, no boards, no code changed. Nothing is machine-checked; no tool
of that kind is installed. Deliverables A to D are sections 1 to 17 of the audit.

**The verdict in one paragraph.** The capability TABLE holds: a reference model drove the real
`capability/*.rs` through 8 x 40,000 generated operations with no mismatch outside two recorded,
downstream-contained divergences (D25), and caught 16 of 16 injected faults. The defects are in waiting and dying: single-slot IPC wait records that are
never withdrawn, three memory-safety windows in kill and reclaim, a tick clock that counts handler
entries rather than time (it carries the v0.28.0 shutdown and reboot deadline), and failure paths that
leak. The enforcement layer missed 28 of 32 injected violations, and the kernel host tests run on no
automatic path.

## Defects found (code; each is fixed separately, one change per flash, QEMU first)

Ids match the audit, section 6, where each has its trace, the minimal correction and the check that would
catch it. Severity is after the skeptics. KNOWN means already recorded elsewhere, and this pass adds to it.

| Id | Sev | Defect | Where |
|---|---|---|---|
| D1 | high | a second blocked sender erases the first: it never wakes, its message and any carried cap are destroyed | `ipc/routing.rs:445-449` |
| D2 | high | `try_recv`, timed-out `recv_timeout` and CallDeadline leave the caller registered; a blocking send can return 0 for a message never queued | `syscall/dispatch.rs:516`, `:582`, `:1630-1640` |
| D7 | high | a claim abort releases twice, drops `CORE_LEAVING`, and a concurrent kill frees a stack and root in use | `task/scheduler.rs:85-91`, `:2040`, `:2227`, `:3289` |
| D8 | high | the deferred-free drain is re-entered by the timer on the same core and frees a stack and root twice | `task/scheduler.rs:1545`, `:1737`, `:2466-2469` |
| D10 | high | a self-kill releases its slot before the core leaves it; a spawn into it lets the dead task run with the new task's caps | `task/scheduler.rs:3163-3166` |
| D12 | high | `MONOTONIC_TICKS` counts handler entries: early on x86 (wake IPIs) and arm32, stalls on riscv64, drops ticks everywhere; every timeout and the power deadline | `task/scheduler.rs:452`, `arch/x86_64/boot.rs:1546` |
| D19 | high | KNOWN A10-2 (user-copy guard per core), plus: a cross-killed copier leaves x86's guard stuck true | `arch/x86_64/syscall_entry.rs:155-260` |
| D3 | med | a dead task's wait records (blocked sender on a peer, call-await, deadline, badge) reach the slot's next occupant | `task/scheduler.rs:2576-2640` |
| D4 | med | CallDeadline's bound does not cover the send leg | `syscall/dispatch.rs:1557-1590` |
| D5 | med | Call liveness, reply correlation, AcquireSendCap and ResourceInvoke key identity on a bare, LIFO-reused endpoint id (partly KNOWN) | `ipc/routing.rs:581-589`, `dispatch.rs:1312`/`:1350` |
| D6 | med | a delivered reply loses to ReplyDead | `syscall/dispatch.rs:1638-1639` |
| D9 | med | `CORE_PENDING_PML4` is one slot (leaks a root, an L1 slot on arm32); the pending-stack overflow branch frees the stack in use (a proof gap: nine undrained self-kills not shown reachable) | `task/scheduler.rs:852`, `:3107`, `:3156-3160` |
| D11 | med | `run()` picks with interrupts on, then claims a different life of the slot: a kernel panic from userspace kill and spawn | `task/scheduler.rs:1569-1593` |
| D14 | med | the kill path publishes the death before it releases device state, IRQ routes and the band | `task/scheduler.rs:2560-2903` |
| D15 | med | a failed spawn never frees its reply endpoint id, leaves its IRQ route on a reused id, strands the framebuffer (IRQ and T1 parts KNOWN) | `task/mod.rs:1450-1457`, `:1677-1683` |
| D16 | med | `FREE_IDS` is a plain lock held by the preemptible supervisor respawn: a core-0 wedge | `ipc/mod.rs:47`, `:74` |
| D17 | med | `ready_count()` used as a core-id bound (placement onto a dead core, watchdog gap); aarch64 arenas sized by arrivals | `task/mod.rs:1308-1319`, `task/scheduler.rs:1799` |
| D18 | med | a level interrupt masked and its notification skipped under queue pressure stays masked forever, silently | `interrupt/route.rs:142`, `:167-170` |
| D20 | med | no kernel stack guards on riscv64; x86 overflow is a triple-fault reset; debug kernels exceed 64 KiB (riscv64 part KNOWN) | `arch/riscv64/mod.rs:2355` |
| D21 | med | kernel W^X and the 6.4 DMA report exist only on x86 (aarch64 parts KNOWN, A10-9, SEC-34) | `arch/arm/mmu.rs:85-102`, `arch/riscv64/mod.rs:3691-3700` |
| D24 | med | cross-core reads of unsynchronised per-task state, one feeding a panicking assertion in `handle_kill` | `invariants/assertions.rs:34-46` |
| D26 | med | `fs`'s in-request remount leaves up to 64 band slots unrevoked per event (userspace; a site of KNOWN A10-5 the fix missed) | `services/fs/src/main.rs:1459-1460` |
| D13 | low | arm32: the power path from the tick ISR takes `FB` with a plain lock while idle can hold it | `bootcon/mod.rs:302-311` |
| D22 | low | SpawnImage mints CONSOLE_READ and grantable peer caps from flags the supervisor does not hold (14.1 step 4) | `syscall/dispatch.rs:1150-1160` |
| D23 | low | x86: ungated query 21 pops the COM2 operator stream; queries 11 and 17 race on the CMOS index port, a candidate for the T630 "4383d" misread | `dispatch.rs:1900`, `arch/x86_64/rtc.rs:41-55` |
| D25 | low | a cap minted on a dead record validates at the capability layer; no-resurrection rests on spawn order | `capability/table.rs:59-70`, `:261-265` |
| D27 | low | results that do not reflect outcomes (SpawnImage error after success, AllocMem leak KNOWN A10-15, stub successes, arch-divergent codes) | `syscall/dispatch.rs:1144-1179` |
| D28 | low | serial output under hot locks; panic path not re-entrant; `nomem` interrupt-mask asm | `ipc/routing.rs:776-779` |
| D29 | low | KNOWN A5-10: the loader never bounds `p_vaddr`; on x86 that reaches the shared kernel half | `loader.rs:262-332` |

## Enforcement-layer defects (scripts and tests; the audit's section 11)

| Id | Defect |
|---|---|
| E1 | `I-irq-lock` never reads the three arch ISR files it declares (`os.walk` on a file path yields nothing) |
| E2 | pins fail open on spelling; VII misses request flags and the probe table; no check pins syscall gates (15 gate mutations, 0 caught); IX passes on a comment; VI misses four `fs` statics; check-defining tables unratcheted; 10 of 21 checks have no red-team row |
| E3 | `cargo test -p kernel --lib`, Miri and mutation run on no automatic path; `mutation.yml` ends `\|\| true`; results five months old |
| E4 | the routing and naming host tests test models; the real `routing.rs` and `names.rs` host-compile with one constant stubbed |
| E5 | P1, P3, P9, P10 test less than their rows; host tests credit dead code; nothing at any level checks that SEC-7 removes GRANT; every QEMU suite is x86 only |
| E6 | KNOWN SEC-11: the guard is a `debug_assert`, absent from release images (argument holds today) |

Specification and comment drift (the audit's section 12) is recorded there and not edited, including
CLAUDE.md 10.5's quiescence claim, which D7 refutes.

## Prototypes (outside the tree; the audit's section 15)

C1 capability reference model (PROPERTY TEST on the real `capability/*.rs`), C2 IPC routing state machine
(PROPERTY TEST on the real `routing.rs`), C3 kill and reclaim model checker (MODEL CHECK, sequential
consistency), C4 syscall gate check (ARCHITECTURAL CHECK). All four were rerun at `537e6861` and
reproduce their recorded results. They live in the session scratchpad, which is temporary; landing them is
the operator's decision, and C2 needs a `kernel/src/lib.rs` seam to live in the repo.

## Next step

The operator's choice, in two parts. Fixes: section 13 of the audit proposes D8, D7, D2, D1, D12 and D10
first, each one change per flash, QEMU first. Checks: `cargo test -p kernel --lib` into the `osdev` gate and
the syscall gate check first (no kernel change), then the capability and IPC harnesses as an
expected-failure ledger. Still open from the brief: a concurrent model of IPC wake and block, weak memory
on every port, and the remaining responsibilities at runtime on arm, aarch64 and riscv64.
