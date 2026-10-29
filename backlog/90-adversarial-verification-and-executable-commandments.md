# 90. Adversarial verification of the kernel, and the commandments made more executable

**Status: OPEN, 2026-10-16; the programme from here set by the operator 2026-10-19; under way since 2026-10-26 on `feat/v1-groundwork`, with Kani, Creusot and Verus as layer C's tools, run in CI (Next step).** Phases 1 and 2 done against the core kernel at `537e6861`, read-only:
the evidence is `audits/2026-10-16-kernel-verification.md`. 29 kernel defects (7 high) and 6
enforcement-layer defects are listed below; since 2026-10-26 D1, D2, D3, D5, D12, D16, D19, D23, D26, D29, the id and IRQ halves of D15 and the placement half of
D17 are fixed, and E1 to E6 are repaired apart from the parts each row names as open. Of the four executable
prototypes, C4 is landed (`scripts/syscall_gate_check.py`); the other three are outside the tree. The brief, as recorded on 2026-10-15, follows; only its list of checker
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
| D1 | high | a second blocked sender erases the first: it never wakes, its message and any carried cap are destroyed. **FIXED 2026-10-27** (below) | `ipc/routing.rs:445-449` |
| D2 | high | `try_recv`, timed-out `recv_timeout` and CallDeadline leave the caller registered; a blocking send can return 0 for a message never queued. **FIXED 2026-10-27:** `try_recv` no longer registers (it never blocks); `RecvTimeout` and `do_call` withdraw on every exit (`routing::withdraw_receiver`, and `clear_call_await` for a Call); `t2a`-`t2c`, `t5a`-`t5c` pass unignored. Residual, recorded: a message arriving between a deadline check and the withdrawal can still wake the task once - a window of a few instructions under the lock, where it was until the task's next wait | `syscall/dispatch.rs:516`, `:582`, `:1630-1640` |
| D7 | high | a claim abort releases twice, drops `CORE_LEAVING`, and a concurrent kill frees a stack and root in use | `task/scheduler.rs:85-91`, `:2040`, `:2227`, `:3289` |
| D8 | high | the deferred-free drain is re-entered by the timer on the same core and frees a stack and root twice | `task/scheduler.rs:1545`, `:1737`, `:2466-2469` |
| D10 | high | a self-kill releases its slot before the core leaves it; a spawn into it lets the dead task run with the new task's caps | `task/scheduler.rs:3163-3166` |
| D12 | high | `MONOTONIC_TICKS` counts handler entries: early on x86 (wake IPIs) and arm32, stalls on riscv64, drops ticks everywhere; every timeout and the power deadline **FIXED 2026-10-28 for x86 and arm32** (an entry that is not the periodic timer - the x86 wake IPI, the arm32 sub-tick wake - is marked and no longer advances the clock or re-arms a one-shot deadline); riscv64's stall was already gone with `backlog/fixed/94`. Measured in QEMU x86: under 400 cross-core round trips the shutdown deadline moved 19 s in 11 s of wall clock before, 12 s in 12 s after. **Not covered:** ticks dropped while core 0 has interrupts masked are still not caught up, so a deadline can arrive late; hardware-confirmed 2026-10-28 on all five boards at `1d569c22` (Wyse, T630, Pi 2, Pi 4, VisionFive: selfcheck 673/673/670/679/679, 0 failed; kill-storm of the USB host 10/10 on each) | `task/scheduler.rs:452`, `arch/x86_64/boot.rs:1546` |
| D19 | high | KNOWN A10-2 (user-copy guard per core), plus: a cross-killed copier leaves x86's guard stuck true **FIXED 2026-10-28** on x86, aarch64 and riscv64: every guarded copy runs with interrupts masked, so no task is preempted inside one - neither the shared guard nor riscv64's `SUM` can pass to the next task, and a cross-core kill cannot catch a copy mid-way. QEMU: identity, adv, shell 276/0; `osdev test perf` before and after shows no cost above QEMU's noise. **Not covered:** `read_user_bytes` still returns a slice into a per-core buffer that the next copy on that core overwrites, so a caller must consume it before it can be preempted; hardware-confirmed 2026-10-28 on all five boards at `1d569c22` (Wyse, T630, Pi 2, Pi 4, VisionFive: selfcheck 673/673/670/679/679, 0 failed; kill-storm of the USB host 10/10 on each) | `arch/x86_64/syscall_entry.rs:155-260` |
| D3 | med | a dead task's wait records (blocked sender on a peer, call-await, deadline, badge) reach the slot's next occupant. **Blocked-sender half FIXED 2026-10-27** with D1 (`forget_blocked_sender`, `t6`); deadline and badge open | `task/scheduler.rs:2576-2640` |
| D4 | med | CallDeadline's bound does not cover the send leg | `syscall/dispatch.rs:1557-1590` |
| D5 | med | Call liveness, reply correlation, AcquireSendCap and ResourceInvoke key identity on a bare, LIFO-reused endpoint id (partly KNOWN). **Call half FIXED 2026-10-27:** every message carries its sender's endpoint generation (`sender_gen`, recorded per task at spawn so the send path pays one atomic load), a Call's reply must match id AND generation, and a target whose entry has another generation is a successor, so the caller gets `ReplyDead`; `t3`, `t4a`, `t4b` pass unignored. `AcquireSendCap` and `ResourceInvoke` open | `ipc/routing.rs:581-589`, `dispatch.rs:1312`/`:1350` |
| D6 | med | a delivered reply loses to ReplyDead | `syscall/dispatch.rs:1638-1639` |
| D9 | med | `CORE_PENDING_PML4` is one slot (leaks a root, an L1 slot on arm32); the pending-stack overflow branch frees the stack in use (a proof gap: nine undrained self-kills not shown reachable) | `task/scheduler.rs:852`, `:3107`, `:3156-3160` |
| D11 | med | `run()` picks with interrupts on, then claims a different life of the slot: a kernel panic from userspace kill and spawn. **Seen in QEMU 2026-10-26** (`backlog/95`, where a guard for it was tried and reverted) | `task/scheduler.rs:1569-1593` |
| D14 | med | the kill path publishes the death before it releases device state, IRQ routes and the band | `task/scheduler.rs:2560-2903` |
| D15 | med | a failed spawn never frees its reply endpoint id, leaves its IRQ route on a reused id, strands the framebuffer (IRQ and T1 parts KNOWN). **The id and IRQ halves FIXED 2026-10-29:** `cleanup_partial_spawn` frees the reply id and releases both endpoints' IRQ routes before their ids can be reused, as the kill path does, and the routing-full refusal gives back the primary id and its resource. **Open:** the framebuffer (the slot's device kind is recorded only at commit, so the unwind cannot see a grant), the device record's bus mastering, and the partial address space (T1) | `task/mod.rs:1450-1457`, `:1677-1683` |
| D16 | med | `FREE_IDS` is a plain lock held by the preemptible supervisor respawn: a core-0 wedge. **FIXED 2026-10-29:** both sites take it with `lock_irq`. Not yet re-examined, as the audit asks: `BAND`, `SEEN` and `FB` with the respawn path in mind | `ipc/mod.rs:47`, `:74` |
| D17 | med | `ready_count()` used as a core-id bound (placement onto a dead core, watchdog gap); aarch64 arenas sized by arrivals. **The placement and watchdog half FIXED 2026-10-29:** both ask by ready core id (`smp::core::nth_ready`, `is_ready`). Shown with the test feature `ap-gap-test` (x86 QEMU, `-smp 4`, core 2 never started): before, 44 services were placed on core 2, which runs no scheduler; after, they spread over 0, 1 and 3 and none on 2. Open: the aarch64 arena sizing | `task/mod.rs:1308-1319`, `task/scheduler.rs:1799` |
| D18 | med | a level interrupt masked and its notification skipped under queue pressure stays masked forever, silently | `interrupt/route.rs:142`, `:167-170` |
| D20 | med | no kernel stack guards on riscv64; x86 overflow is a triple-fault reset; debug kernels exceed 64 KiB (riscv64 part KNOWN) | `arch/riscv64/mod.rs:2355` |
| D21 | med | kernel W^X and the 6.4 DMA report exist only on x86 (aarch64 parts KNOWN, A10-9, SEC-34) | `arch/arm/mmu.rs:85-102`, `arch/riscv64/mod.rs:3691-3700` |
| D24 | med | cross-core reads of unsynchronised per-task state, one feeding a panicking assertion in `handle_kill` | `invariants/assertions.rs:34-46` |
| D26 | med | `fs`'s in-request remount leaves up to 64 band slots unrevoked per event (userspace; a site of KNOWN A10-5 the fix missed). **FIXED 2026-10-29:** the in-request remount and the format (A10-5's `OP_FLASH` site) revoke the open files first, as the top-of-loop remount does. Open: the kernel does not log band exhaustion distinctly | `services/fs/src/main.rs:1459-1460` |
| D13 | low | arm32: the power path from the tick ISR takes `FB` with a plain lock while idle can hold it. **FIXED 2026-10-29:** every writer takes `FB` with `lock_irq`, so the tick never finds it held by its own core; the panic and power-off paths take it with `try_lock`, and once they have begun no writer waits for it (a holder halted mid-draw cannot keep the last words off the screen; serial has them) | `bootcon/mod.rs:302-311` |
| D22 | low | SpawnImage mints CONSOLE_READ and grantable peer caps from flags the supervisor does not hold (14.1 step 4) | `syscall/dispatch.rs:1150-1160` |
| D23 | low | x86: ungated query 21 pops the COM2 operator stream; queries 11 and 17 race on the CMOS index port, a candidate for the T630 "4383d" misread. **The CMOS half is FIXED** (`80f2bb49`, one reader of the index/data pair at a time; `syscall_gate_check.py` now records the lock as the queries' declared effect); **the COM2 pop FIXED 2026-10-29:** query 21 is INTROSPECT-gated, which `control` holds (other INTROSPECT holders can still read it, recorded) | `dispatch.rs:1900`, `arch/x86_64/rtc.rs:41-55` |
| D25 | low | a cap minted on a dead record validates at the capability layer; no-resurrection rests on spawn order | `capability/table.rs:59-70`, `:261-265` |
| D27 | low | results that do not reflect outcomes (SpawnImage error after success, AllocMem leak KNOWN A10-15, stub successes, arch-divergent codes) | `syscall/dispatch.rs:1144-1179` |
| D28 | low | serial output under hot locks; panic path not re-entrant; `nomem` interrupt-mask asm | `ipc/routing.rs:776-779` |
| D29 | low | KNOWN A5-10: the loader never bounds `p_vaddr`; on x86 that reaches the shared kernel half. **FIXED 2026-10-29:** a segment whose first or last page fails the arch's `validate_user_ptr` (the user ceiling the syscall path already uses, on every arch) is refused, and `p_vaddr + p_memsz` is checked for wrap | `loader.rs:262-332` |

### D1 fixed, 2026-10-27 (with D3's blocked-sender half)

The operator approved kernel changes the same day. The cause was shape, not a slip: each endpoint held
ONE blocked sender and ONE pending message, so a second sender to a full queue overwrote the first,
whose message and any carried cap were destroyed and who was never woken.

**The fix keeps the contract every blocking syscall relies on** - a sender woken with 0 has had its
message moved INTO the queue, delivered, not "try again" - and moves the pending message from the
endpoint to the TASK: `routing.rs` holds one blocked-send record per task slot (`PENDING`: the
endpoint, an order stamp, the message), because a task waits in at most one send, so `MAX_TASKS`
records hold every waiter there can be. Each endpoint keeps only a count. A dequeue that frees a slot
moves the OLDEST waiter's message in and wakes it (exact FIFO among blocked senders); `kill_endpoint`
returns the oldest and the kill path drains the rest with `take_blocked_sender`, each woken
`EndpointDead`; a task that dies while blocked is withdrawn (`forget_blocked_sender`, D3's half) and its
never-accepted message is dropped and counted. The console path (`enqueue_from_kernel_blocking`) uses
the same records. Cost: about 0.5 MB more kernel `.bss` (224 records against the 96 per-endpoint
messages it replaces), beside a routing table that already holds 96 x 16 queued messages.

**The proposal this replaces** (a FIFO of task slots, each woken sender RETRYING its own enqueue) was
dropped: it changed the meaning of a wake in five syscalls and gave up FIFO order to any non-blocking
sender. **Two host tests were wrong about the fixed state** and say so: `t1b` asserted the kill names
the SECOND sender as its one blocked sender (which only the overwrite could give) and now states I-BLK -
both woken, oldest first, none twice; `t6` expected a dead sender's message to be delivered (only the
left-in-place dead record did that) and now expects it withdrawn. `t1d` adds five waiters, delivered in
order and each woken once. Proof so far: `t1a`-`t1d` and `t6` pass with their `#[ignore]` removed; QEMU
identity, adversarial and chaos suites follow; then one card per board.

## Enforcement-layer defects (scripts and tests; the audit's section 11)

| Id | Defect |
|---|---|
| E1 | `I-irq-lock` never reads the three arch ISR files it declares (`os.walk` on a file path yields nothing). **FIXED 2026-10-26** (`b920aa5a`): a scope entry may be a file, an entry that contributes no file fails, riscv64's ISR files joined; 7 files read instead of 2, and a red-team probe catches a plain `lock()` planted in one |
| E2 | pins fail open on spelling; VII misses request flags and the probe table; no check pins syscall gates (15 gate mutations, 0 caught); IX passes on a comment; VI misses four `fs` statics; check-defining tables unratcheted; 10 of 21 checks have no red-team row. **FIXED 2026-10-26** apart from the operator's items below, each with a red-team row: IX reads code, not comments (`c0898a06`); VI sees path-qualified, array and `pub(crate)` statics, and the four `fs` statics are listed debt that may only shrink (`e2c2f22c`); V-managed-watched requires `is_watched`'s exact shape, and V-no-panic scans runtime asserts and deref lines (`a062c8bd`); II requires `is_transient` to be a plain disjunction of names (`d123d828`); IV now fails a schema capability key that is neither mapped nor listed as not reconciled with a reason, and a stale entry. The I-* pins read structurally and refuse what they cannot read: I-syscalls checks every dispatch arm against the enum, I-introspect reads or-patterns and refuses other heads, and pins the UNGATED query set as `introspect_ungated`, I-authorities takes hex ids and any visibility, I-kernel-deps and I-features parse `Cargo.toml`, and I-kernel-spawns refuses a non-literal name outside `spawn_from_image`. The role whitelists are ratcheted: `kernel_support_roles` and `arch_permitted_roles` must equal frozen copies in `commandments.py`, and IV's mapped privilege keys may only grow (new check `I-frozen-tables`). **Two gaps IV now names and leaves for the operator:** `spawn` is a dead, array-typed schema key, so SPAWN cannot be declared, and `chaos`, `control`, `shell` and the supervisor hold it undeclared (closing it is a schema key, which is covered surface); `dwc2`, `ehci` and `xhci` hold CONSOLE_PUSH undeclared, and `contract_check.py` would fail the declaration because they push through `gs::driver::input::push`, not `console_push(`. **Also for the operator:** the SDK's deliberate bounds-check panics (`Mmio`, `Dma`, the arena) are outside V-no-panic, which scans the kernel. VII reads each supervisor row's flags and privilege word as FIELDS: REQ_CONSOLE, PEERS_GRANT and HANDOVER are grants (REQ_CONSOLE, which has the kernel mint CONSOLE_READ outside `SUPERVISOR_DELEGATABLE`, D22, is now pinned for `chaos`, `observe-live` and `shell`), a literal or named-constant word is an unreadable token that must be pinned, and the probe table and `SUPERVISOR_DELEGATABLE` are pinned as grantors of their own. **The syscall gate check (C4) is LANDED** as `scripts/syscall_gate_check.py`, on `osdev`'s `EXTRA_CHECKS`: every dispatch arm is gated by a recognised check or ungated by record with its reason, each arm's gate signature is pinned, and its 18 mutants run with every plain check (about 6 s; the kernel index is built once, which took the self-test from 27 s to 5). Its records were brought up to the tree: `Exit` (57) ungated by record (14.5), and queries 11 and 17 declared to take the CMOS lock. It still WARNS on `ResourceInvoke`'s caller-chosen right, where `Rights(0)` passes vacuously and only the owner (`fs`) refuses it. **Found 2026-10-29, open:** `arch_seam_check.py` discovers a seam member only from a path-qualified `arch::imp::name` use, so a member a neutral file brings in through a `use crate::arch::imp::{..}` list is not counted - `validate_user_ptr` was a member all along and joined the count (125 to 126) only when the loader called it qualified (D29). A port missing such a member still fails to COMPILE, so nothing ships broken; what is wrong is the count and the bounded-port promise it underwrites |
| E3 | `cargo test -p kernel --lib`, Miri and mutation run on no automatic path; `mutation.yml` ends `\|\| true`; results five months old. **MOSTLY FIXED 2026-10-27:** `cargo test -p kernel --lib` runs on every build (`host_test_check.py`, `8c2a9f1b`). Mutation was rerun (716 mutants, 49 minutes on four jobs: 546 caught, 43 unviable, 1 timeout, 126 missed, 65 distinct once line numbers go) and `mutation.yml` now fails on a survivor set that differs from `kernel/mutants-survivors.txt` either way (`scripts/mutants_check.py`, a ratchet that only shrinks), with cargo-mutants pinned to 27.0.0. `kernel/mutants.toml` is an include list: the exclude list mutated bin-only files (881 mutants) and still named the deleted `control.rs`, and `table.rs` and `delegated.rs` are mutated for the first time. **Still open:** Miri and mutation run only when `build.yml` and `mutation.yml` are dispatched, as every test workflow is paused for CI minutes |
| E4 | the routing and naming host tests test models; the real `routing.rs` and `names.rs` host-compile with one constant stubbed. **Routing half DONE 2026-10-27:** `tests/host-ipc` (the audit's caps-transit harness, landed) compiles the real `routing.rs`, `queue.rs`, `message.rs`, `endpoint.rs` and the capability files with `MAX_TASKS` stubbed, and `host_test_check.py` runs it on every build: 4 positive controls and the real files' 51 unit tests must pass, and 13 scenarios that reproduce D1, D2, D3 and D5 are `#[ignore]`d with their ids and must still FAIL under `--ignored` - one that passes fails the build until its ignore is removed, so each becomes its fix's regression test. The stub is checked against the kernel's `MAX_TASKS`. **Naming half DONE the same day:** the real `names.rs` is in the same crate (`PEER_NAME_BYTES` stubbed and checked): resolution, a respawn re-pointing a name, the old instance unable to unregister its successor, a reused endpoint id evicting the stale name, a full table, and a property test against a plain map; a mutation of the respawn guard fails two of them. **Recorded, not fixed:** `register` drops a name longer than `PEER_NAME_BYTES` without a log or an error, and a full table only logs - neither tells its caller (unreachable today: a spawn refuses a name over 64 bytes, 14.1) |
| E5 | P1, P3, P9, P10 test less than their rows; host tests credit dead code; nothing at any level checks that SEC-7 removes GRANT; every QEMU suite is x86 only. **Measured 2026-10-27 by the mutation run:** the 65 survivors were the list. **The mutation half FIXED the same day (the rest of this row is not), down to 5, every one an equivalent mutant:** the global-path tests in `table.rs` (`c7a1ad4b`) caught 19, and test-only additions caught 41 more - `queue.rs`'s `dequeue_matching` (the reply correlation `Call` depends on: middle, oldest, no match, across the wrap, a FULL queue, and a proptest against a list model), `delegated.rs` through the real band and global table (owner-only revoke making the cap `CapRevoked`, owner death making each cap `EndpointDead` with an exact count, the cursor not reissuing a just-freed slot), `Rights::without` and an overlapping `|`, `next_generation`, the first overflow id, `Message`'s `Debug` never printing the payload, and `packed_from_epoch` by its exact fields (a January packed as month 13 of the year before, and the 2100 century rule, both pass a round trip). The x86 kernel binary is byte-identical throughout. A full run: 716 mutants, 37 minutes, 649 caught, 45 unviable, 3 timeouts, no new survivor. **The 5 that remain cannot be caught, and why:** `delegated.rs` `% -> +` in the cursor advance (the next claim reduces it mod the band anyway); `rights.rs` `1 << 0 -> 1 >> 0` (both 1); `CapTable::get` `>= -> <` (it only decides whether a missing record is LOGGED, and the host `kprintln` prints nothing); `deglitch_epoch` `< -> <=` (equal times give the same answer either way); `packed_from_epoch` `| -> ^` (the fields never overlap). **Four since 2026-10-29:** `packed_from_epoch` had no caller but its tests and moved into them (Audit 26), and a full run that day (513 mutants, 457 caught, 44 unviable, 5 timeouts) left exactly the other four. **Found while strengthening P10, 2026-10-27, and FIXED the same day at the operator's word: the SDK folded three kernel answers into `EndpointDead`.** `i64_to_ipc_error` now decodes -5 as `CapWrongScope`, -6 as `CapRevoked` and any other code as the new `IpcError::Unknown(code)`; `gs` maps `Unknown` to `Error::Failed` (not retry-safe) in both of its mappings, and `CapWrongScope` now reaches them as the `PermissionDenied` they already said it was. The original finding, as recorded: `i64_to_ipc_error` (`sdk/rust/src/ipc.rs`) has no arm for -5 (`CapWrongScope`) or -6 (`CapRevoked`), and ends `_ => IpcError::EndpointDead`, so a revoked file cap, a cap of the wrong kind and any code the kernel does not define all reach a service as "the endpoint died" - which `gs` answers by reacquiring by name. CLAUDE.md 7.7 keeps `CapRevoked` and `EndpointDead` apart on purpose, and 2.4 asks for no silent fallback; the fix is two arms and an explicit unknown, but it changes what every service's error handling sees, so it is recorded rather than made. P1 and P10 now read the raw code (`adversarial::fuzz_syscall`) for this reason. The heaviest: `CapTable::get`'s rights check (`delete !` survives), `holds_resource` (returning `true` survives - the by-holdings gate of Kill, Reboot and the introspection reads), `mark_dead_resource` and `revoke_resource` as no-ops, and most of `delegated.rs`. The cause is one fact: the host tests build LOCAL `GlobalResourceTable`s, so the global table `get` reads is exercised by no host test |
| E6 | KNOWN SEC-11: the guard is a `debug_assert`, absent from release images (argument holds today). **FIXED 2026-10-28:** an `assert!` in release too (`capability/table.rs`, `bump_generation`) - a bump of a stable gate resource is corrupted capability state, which 6.2 says halts every core; one compare on a death or revocation path. QEMU x86 identity 24/0, adv 15/0, kernel --lib 130/0; hardware-confirmed 2026-10-28 on all five boards at `1d569c22` (Wyse, T630, Pi 2, Pi 4, VisionFive: selfcheck 673/673/670/679/679, 0 failed; kill-storm of the USB host 10/10 on each) |

Specification and comment drift (the audit's section 12) is recorded there and not edited, including
CLAUDE.md 10.5's quiescence claim, which D7 refutes.

## Prototypes (outside the tree; the audit's section 15)

C1 capability reference model (PROPERTY TEST on the real `capability/*.rs`), C2 IPC routing state machine
(PROPERTY TEST on the real `routing.rs`), C3 kill and reclaim model checker (MODEL CHECK, sequential
consistency), C4 syscall gate check (ARCHITECTURAL CHECK). All four were rerun at `537e6861` and
reproduce their recorded results. Their evidence copy is `GodspeedOS/verification/backlog90-2026-10-16`, outside
the repository. C4 is landed (`scripts/syscall_gate_check.py`, E2); C2 needs a `kernel/src/lib.rs` seam to live
in the repo.

## The programme from here: MISCIS continuous verification and engineering assurance (the operator's spec, 2026-10-19)

The operator's specification for what this entry becomes once phases 1 and 2 above are acted on, recorded here
rather than as a second entry because it is the same work: its "previous adversarial audit" is this entry's, its
priority list is section 13's, and its enforcement repairs are E1 to E6. Adapted to the tree; the operator's words
are kept where they are the rule.

### The aim, and the rule it is judged by

A lightweight, repeatable, machine-checked verification system for the MISCIS kernel that runs on every kernel
change. **Very high engineering confidence, not a claim of complete mathematical correctness** or seL4-equivalent
formal verification. The kernel stays small, boring, predictable and resistant to regression, and it becomes
steadily harder for a human or an AI contributor to introduce a violation of a fundamental invariant.

> **Make MISCIS harder to break with every contribution, not harder to contribute to.**

Scope is the six responsibilities (CLAUDE.md 4.3), and only those: no seventh, and nothing moved from userspace
into the kernel to make it easier to verify. **The verification lives outside the running system**: never a
kernel service, never a runtime dependency, nothing added to an image to check it.

### Five layers, each mapped to what already exists

- **A. Architectural enforcement.** `scripts/commandments.py` and `commandments_redteam.py`, the boundary, seam,
  scope and unsafe checkers, all in `osdev`'s `EXTRA_CHECKS`. What it must cover: kernel responsibilities limited
  to MISCIS; privileged operations only in the approved modules; no prohibited dependency; no new entry point that
  bypasses a capability check (C4, the syscall gate check, is the prototype); architecture code behind the seam;
  no userspace service with a hidden privileged interface; and **no authority from a package's identity, a
  filesystem path or ambient privilege** (the package work of 2026-10-15 to 2026-10-19 is the newest place this
  could fail). **First: repair E1 to E6**, because a check that cannot fail is not evidence: a guard never observed
  firing proves nothing.
- **B. Property-based state-machine testing.** C1 (capabilities) and C2 (IPC routing), PROPERTY TESTS on the real
  `capability/*.rs` and `routing.rs`, waiting to be landed (C2 needs the `kernel/src/lib.rs` seam). Then sequences
  that cross responsibilities rather than single functions: spawn and kill, grant, attenuate, transfer and revoke,
  send, receive, block and cancel, queue saturation and exhaustion, task-identity reuse, ownership and
  reclamation, supervisor and service death. Deterministic seeds; a failing sequence kept and replayable.
- **C. Machine-checked verification: Kani, Creusot, and Verus where a mechanism needs it (the operator's
  choice, 2026-10-26).** New here. Start with the capability table, task identity and lifecycle transitions, the
  IPC queue and blocked-task records, and ownership and reclamation. Every harness or proof documents the property,
  the PRODUCTION functions it exercises, the bounds or the specification, the assumptions, what it does not
  support, and the result. **A bounded proof is never described as unrestricted, and a simplified model is never
  presented as the implementation.** Each tool has one job:
  - **Kani: bounded counterexample discovery** (model checking through CBMC). A harness explores every input
    within its stated bounds and either produces a concrete failing input or finds none. A result is
    COUNTEREXAMPLE FOUND, or PROVED WITHIN STATED BOUNDS for exactly those bounds. It reasons about `unsafe` and
    raw pointers, which Creusot cannot, so it covers the `arch/`, `memory/` and `smp/` obligations as well as the
    table and queue operations: rights narrowing, generation checks, `0 <= len <= capacity`, a blocked-sender
    record kept or withdrawn.
  - **Creusot: invariants of safe Rust** (deductive verification through Why3 and its SMT solvers). A function is
    proved against a contract written beside it (`#[requires]`, `#[ensures]`, loop invariants), for every input. A
    result is PROVED, but only against that contract, so the contract is what review checks: a proof of the wrong
    specification proves nothing useful. It cannot reason about `unsafe` and handles statics and interior mutability
    poorly, so it is for small pure functions whose property is the whole point: `Rights::without`,
    `narrow_embedded_for_receiver` (SEC-7's GRANT strip, which no test checks today, E5), `next_generation`, the
    queue index arithmetic.
  - **Verus: selected low-level mechanisms, decided one mechanism at a time.** The one of the three that proves
    properties of raw memory and concurrency primitives, through ghost permissions: candidates are the spinlocks,
    the frame allocator's bitmap and a page-table walk. Adopted for a mechanism only when Kani's bounds are not
    enough for it, and with one decision taken explicitly each time: Verus verifies code written inside its
    `verus!` macro against its own library, so proving PRODUCTION code makes its crates kernel dependencies - what
    `I-kernel-deps` (Commandment I) exists to refuse without a recorded reason - while proving a COPY proves the
    algorithm, not the implementation, and is labelled so.
  - **Where they run: in CI, reproducibly, never on one workstation.** Kani and Creusot are Linux and macOS tools,
    so they run on Linux CI runners, in jobs that run on every pull request touching `kernel/` and are a required
    check for merging - the first automatic check on the OS repository's pull requests, which today have none,
    where the test workflows are `workflow_dispatch` and paused. The tool versions are pinned (Kani's release,
    Creusot's commit, Why3's and each solver's version, and the solver timeouts, the usual cause of a flaky proof),
    and the same environment ships as a container image, so a contributor reruns a failing proof exactly: on Linux
    or macOS directly, on Windows under WSL. The fast checks stay in `osdev build`, where every build runs them; the
    proofs and the long property runs are CI's. Each tool proves the HOST build of the files it covers - the same
    files the kernel compiles (`capability/*.rs`, and `routing.rs` and `endpoint.rs`, which E4 found host-compile
    with one constant stubbed), through the `kernel/src/lib.rs` seam C2 already needs - so that the kernel's target
    build is the same code is an assumption every result states.
  - **Kani landed, 2026-10-27: `tests/kani`, 15 harnesses, all PROVED WITHIN STATED BOUNDS** over the real
    `capability/{cap,generation,rights,table}.rs` and `smp/spinlock.rs`, run in `.github/workflows/verify.yml`
    (Kani 0.68.0 pinned, `ubuntu-24.04`, on a pull request touching those files). Rights narrowing, `without`,
    `contains` as a partial order, the empty right, union; `validate` exact; a narrowed or GRANT-stripped cap
    keeps its identity; a generation bump strictly increases and never wraps; a slot round-trips; an empty slot
    is `CapNotHeld`; and lazy invalidation - after a revoke or a death the SAME held cap is `CapRevoked` or
    `EndpointDead` for any rights and right asked. **Bounds, stated:** the revocation proof runs at three
    CONCRETE ids (100, 4096, 8191), one harness each, because a symbolic index into the 8192-entry
    `GLOBAL_RESOURCES` took CBMC past 15 GB even when limited to three values; concrete, each takes seconds in
    about 50 MB. A full `CapTable` is not proved (64 concrete inserts left nothing symbolic and CBMC ran 25
    minutes); the host proptest covers it. **Shown not vacuous:** with CapRevoked and EndpointDead swapped in
    the expectation, the revocation proof FAILED on that assertion. Not yet a required merge check: that is a
    branch-protection setting, the operator's.
  - **Creusot installed and calibrated, 2026-10-27, no proof of kernel code yet.** Creusot 0.15.0-dev with
    Alt-Ergo 2.6.4, Z3 5.1.0, CVC4 1.8, CVC5 1.4.1, on Ubuntu 24.04 (its Z3 needs glibc 2.39, so 22.04 cannot
    run it). A true bit-level spec and a true integer spec proved, and a deliberately false one failed. A bound
    from a bit operation to an integer (`(a & b)@ <= a@`) is NOT proved unaided, which matters for
    `Rights::without`: its contract is best written on the `u8`, not on its integer value.
    **The decision before the first kernel proof, the operator's:** Creusot is modular, so a call is checked
    against the callee's contract and a function without one promises nothing. A test crate that includes
    `rights.rs` by `#[path]` (Kani's shape) cannot prove a wrapper around `Rights::without`; the contract has
    to sit on the production function. That means `creusot-std` attributes in kernel source and the crate in
    `kernel/Cargo.toml`, behind `cfg(creusot)` so the target build never sees them - still a dependency line,
    which is what `I-kernel-deps` (Commandment I) refuses without a recorded reason. The alternative, a copy
    of the function, proves the algorithm and not the implementation, and would be labelled so.
- **D. Runtime chaos with invariant assertions.** The section 22 suites (property, fuzz, stress, adversarial, chaos) and
  `chaos max-carnage`. What changes: each run checks EXPLICIT invariants, not that the machine stayed up. **A
  surviving kernel is not necessarily a correct kernel.** Termination and recreation, IPC floods, memory
  exhaustion, revocation mid-operation, service death and recovery, rapid scheduling, interrupt pressure, and
  cross-core contention where the port has it.
- **E. Architecture-specific regression.** The four ports and five boards: page tables, isolation, context
  switches, interrupts, timer semantics (D12 is the live example), atomics and memory ordering, teardown, SMP.
  **Emulator evidence and hardware evidence are said apart**; QEMU passing proves nothing about a board, and every
  QEMU suite today is x86 only (E5).

### The invariant registry

A version-controlled registry, `verification/invariants/registry.md` (the layout is below): one row per invariant
with a stable id, a precise definition, the responsibilities involved, the production code, the mechanisms that
check it, the test and harness references, the assumptions and limits, and the current assurance status. The
operator's first proposals, to be refined against the code before any is claimed:

| Id | Invariant | Checked by |
|---|---|---|
| CAP-001 | No authority is created without a capability that permits it | model, Kani |
| CAP-002 | Revoked authority cannot be reused through a stale reference | model, targeted tests (D25) |
| MEM-001 | Exclusive memory is never owned by two conflicting owners at once | model, targeted checks (D7, D8, D9) |
| IPC-001 | No send reports success unless its delivery or completion holds | model, Kani, QEMU (D2, D6) |
| IPC-002 | Blocked sender and receiver records stay consistent | model, regression tests (D1, D3) |
| SCH-001 | A reclaimed task cannot be scheduled | model, lifecycle tests (D10, D11) |
| SCH-002 | Reusing a task's identity cannot expose the previous task's authority | model, Kani (D3, D5, D10) |
| INT-001 | Interrupt state transitions stay consistent | targeted checks, QEMU (D18) |
| SMP-001 | Shared cross-core state changes obey the synchronisation they require | concurrency tests, review (D16, D24) |

The audit's own invariants (section 4 of `audits/2026-10-16-kernel-verification.md`) seed it as well.

### Start with the findings, not the whole kernel

Not a proof of everything: the highest-risk defects above first, in section 13's order (D8, D7, D2, D1, D12,
D10), covering task termination and slot reuse, kernel stack and page-table lifetime, blocked-sender and receiver
consistency, false IPC completion, timer semantics, and capabilities across the task lifecycle. For each one:
reproduce it, or record why it is invalid (a source-level finding is never presented as a reproduced exploit);
fix the root cause; add a regression test **shown to fail against the defect**; add a model property and a
bounded harness where practical; verify in QEMU and on the boards it touches - one change per card.

### The checks are themselves tested

Mutation testing of the verification system: inject a known violation into a DISPOSABLE variant - a removed
capability check, an invalid state transition, a task id reused too early, a stale IPC registration, a double
release, a forbidden kernel dependency - and the matching gate must catch it; one that does not is an enforcement
gap to record and repair. `commandments_redteam.py` is this for the commandments, and E2 lists where it is thin.
**A green pipeline is not strong assurance unless its checks can detect representative violations.** No
deliberate defect ever stays in production code.

### Two tiers

- **Tier 1, every kernel change**, in minutes where practical: the architectural checks, the fast property tests,
  the targeted Kani harnesses, every target compiling, a short QEMU smoke and chaos run, and an assurance report.
  A failing required check blocks the merge. A change that touches an invariant no check exercises is reported as
  a COVERAGE GAP, never counted as verified. *Adapted:* this project's workflows are `workflow_dispatch`-only and
  paused, and every gate runs in `osdev build` (`EXTRA_CHECKS`), so tier 1 lands there, where a build cannot skip
  it - a check on a path nobody runs is a check on none; a hosted pipeline is added when CI runs again.
- **Tier 2, nightly, for high-risk changes and for release candidates**: longer property runs, wider bounds, full
  mutation testing, long QEMU chaos, architecture stress, the boards where infrastructure allows, and a review of
  unresolved assumptions and gaps. A high-risk change gets it before merging.

### How results are worded

Every result is one of: **PROVED WITHIN STATED BOUNDS**, **TESTED - NO VIOLATION OBSERVED**, **COUNTEREXAMPLE
FOUND**, **PROOF GAP**, **NOT EXAMINED**. Never conflated: passing tests with proof, a model with the
implementation, a bounded proof with unrestricted correctness, the emulator with the hardware, simplicity with the
absence of vulnerabilities. No percentage of correctness without a measurement model that defends it. (The audit's
check grades - architectural check, property test, model check, implementation verification, machine-checked proof,
unverified - and its reasoning results - counterexample found, argument supported, proof gap, not examined - map
onto these.)

### Layout

```text
verification/
  README.md              how to run each layer, and what a failure or a gap means
  invariants/registry.md the authoritative list (above)
  architecture/          checks not already in scripts/ (most stay there, where the build runs them)
  models/                reference state machines (C1, C2 land here, beside a kernel test seam)
  properties/            property-based state-machine tests and their kept failing seeds
  kani/                  bounded harnesses, each with its bounds and assumptions written beside it
  mutations/             the injected violations and which gate must catch each
  harnesses/             QEMU scenario drivers that assert invariants
  results/               GENERATED reports, by commit, kept apart from the sources
```

Adapted to the tree: what `scripts/` and `osdev` already do is not duplicated here, and generated reports never sit
beside the authoritative specifications.

### Contributors run it themselves

Documented commands for: every fast kernel check; one responsibility's checks; replaying a failing property test
from its seed; one Kani harness; a QEMU scenario; reading the registry; reading an assurance failure or a gap. No
private infrastructure and no proprietary AI service needed for the core suite. Correct kernel changes become
easier to review, and incorrect ones easier to reject.

### Order

1. **Restore trust in architectural enforcement**: E1 to E6 fixed, the checks mutation-tested, in the build.
2. **Capture the known high-severity defects** as permanent regression properties (D8, D7, D2, D1, D12, D10
   first), each fix one change per card.
3. **Bounded verification** of a few production functions, starting with capabilities and lifecycle.
4. **Integration assurance**: cross-responsibility state machines, invariant-checking chaos, per-architecture
   regression.
5. **Maintained**: a kernel change preserves the invariants it touches and extends verification when it adds a
   state transition.

No TLA+ or theorem-proving programme unless a concrete gap justifies one. *(Amended 2026-10-26 by the operator:
Creusot, a deductive verifier, joins Kani in layer C, and Verus may for a selected low-level mechanism. The rule
still bounds them: a deductive proof is for a function or mechanism whose property a test or a bounded search
cannot cover, never a programme to prove the kernel.)*

### Done when

Every kernel change runs the fast tier; known architectural violations are reliably rejected; the confirmed
high-severity defects have permanent regression coverage; capability, IPC and lifecycle operations have
reproducible state-machine tests; selected production functions have machine-checked bounded properties; injected
representative faults make the relevant checks fail; assumptions and uncovered areas are visible; contributors
reproduce the checks locally; basic assurance needs no manual AI review; and MISCIS is still six responsibilities.

### Not attempted

Reproducing seL4; proving the whole kernel at once; eliminating every vulnerability; verification machinery in the
running kernel; a seventh responsibility; models in place of hardware testing; checks in place of engineering
review; claiming that green CI proves correctness.

> The kernel should become increasingly boring because its behaviour is constrained, its invariants are explicit,
> and its critical properties are repeatedly checked against the real implementation - a kernel contributors
> modify confidently, because the verification challenges their assumptions before their code is merged.

## Next step

**Under way since 2026-10-26, on `feat/v1-groundwork`** (the operator: "Let's use this branch to do backlog 90").
Order step 1, restoring trust in the enforcement layer, is done there: E1 to E6 are repaired (apart from what
each row leaves open), `cargo test -p kernel --lib` and `tests/host-ipc` run on every build
(`scripts/host_test_check.py`), and C4 (the syscall gate check) is landed. Of section 13's fixes, D2, D1 and D12
are done (with D3, D5 and D19).

Then, as before: the rest of section 13's order (D8, D7, D10), each one change per flash, QEMU
first; and layer C (above): **Kani** first on the capability files C1 already drives, then `routing.rs` once the
`kernel/src/lib.rs` seam exists, then **Creusot** on the pure functions; **Verus** only when a mechanism needs it.
All of it in a pinned CI job on Linux runners, required on pull requests touching `kernel/`, with a container
image for running it locally (WSL on Windows). The job exists for Kani (`.github/workflows/verify.yml`, on a pull
request touching the capability code, `smp/spinlock.rs` or `tests/kani`; not yet required); the image does not.

**What the scheduler fixes need first.** D7, D8, D10 and D11 change the task-switch path, and on 2026-10-26
`osdev test shell` could not judge a change there: in QEMU a supervisor respawn sits at the liveness watchdog's
allowance, and a guard whose slow path never ran went with 5 wedges in 7 runs (`backlog/49`, `backlog/95`). So
either `backlog/49` is fixed before them, or each is A/B-tested over several runs each way.

Still open from the brief: a concurrent model of IPC wake and block, weak memory on every port, and the
remaining responsibilities at runtime on arm, aarch64 and riscv64.
