# Adversarial verification of the core kernel, 2026-10-16

**Question (the operator's words):** "Please do backlog 90 against the core kernel and see if you can capture
the checks in code (like we did for the commandments). [...] Don't change code. Just record findings."
The brief is `backlog/90-adversarial-verification-and-executable-commandments.md`.

**Reviewed commit:** `537e68615e5e496924c4a2b9434cdd971fec3f1b` (`537e6861`, `main`). Every `file:line` below
is at that commit. The repository was not modified: `git status` was clean before and after every pass, and
mutations were applied only to copies outside the tree.

**What this is NOT.** It is not a formal verification, and nothing here is a machine-checked proof. No Kani,
Verus, TLA+, Alloy or Apalache was installed or run. The "model checks" below are explicit-state checkers
written in Python for this pass, over models extracted by hand from the code; a model is not the
implementation, and a model check says nothing about the binary. No QEMU suite, `osdev` test or hardware
board was run. Every COUNTEREXAMPLE is a trace through source code, confirmed by skeptics who re-read the
code, and in some cases reproduced by a host prototype that compiles the real kernel files. None was
observed on a running machine.

**Code defects are recorded, not fixed.** Per the standing rule and the brief, every code defect is listed in
ONE backlog entry, `backlog/90`, and is fixed separately with the operator's approval, one change per flash,
QEMU first. Specification and comment drift found here is also recorded (section 12) and not edited.

---

## 1. The verdict

**The capability table itself holds up; the defects are in waiting and dying.** The core capability code
(`capability/table.rs`, `delegated.rs`, `rights.rs`, `generation.rs`) was driven by a spec-derived reference
model against the REAL files over 8 seeds x 40,000 generated operations with no mismatch other than the
stale-cap error-code divergence it records (mint-after-death operations are probed separately, not
compared), and that harness caught 16 of 16 injected faults. Non-escalation of rights, revocation by generation bump and
delegated-band ownership are ARGUMENT SUPPORTED and PROPERTY TESTED at the table level. The two divergences it
found (a cap minted on a dead record still validates; a stale cap's error code flips after id reuse) are
contained downstream today.

The serious findings sit where a task waits, is woken, dies, or is replaced:

- **IPC wait records are single-slot and are never withdrawn.** A second blocked sender silently erases the
  first (it never wakes, and its message and any carried capability are destroyed). `try_recv`, a timed-out
  `recv_timeout` and a timed-out `CallDeadline` leave the caller registered as a waiter, so a later message
  wakes it inside an unrelated wait and a blocking send can report success for a message that was never
  queued. Seven independent readers found the first; the IPC harness reproduces both on the real
  `ipc/routing.rs`.
- **The kill and reclaim protocol has three memory-safety windows.** An abort after a successful claim
  releases the claim twice and stops naming the task the core is still standing on, so a concurrent kill
  frees a kernel stack and page-table root in use; the deferred-free drain is re-entered by the timer
  interrupt on the same core and frees the same stack and root twice; a self-kill releases its slot before
  the core leaves it, so a spawn into that slot can let the dead task return to user mode with the new
  task's capabilities. The scheduler model finds all three; the proposed fixes for the first two check
  clean there (the drain fix leaves a separate root leak, D9), and a fix for the third was checked only in
  a separate follow-up model.
- **The tick clock is not a clock.** Deadlines, sleeps, `recv_timeout`, `CallDeadline` and the power-off and
  reboot deadline all count core-0 timer-handler entries. On x86 every wake interrupt to core 0 adds a tick,
  so a `shutdown 10m` can fall due in under a minute under IPC load; on riscv64 the tick can stall;
  on every port masked stretches drop ticks with no catch-up.
- **Failure and teardown paths leak or misattribute.** A failed spawn never frees its reply endpoint id (a
  permanent leak; whether enough of them can reach the `endpoint id space exhausted` panic before memory
  runs out is open), leaves an interrupt route on a reused id, and strands the framebuffer; the kill path publishes the death before it releases device state, so a fast
  respawn can have its new IOMMU confinement reverted.

**The enforcement layer is weaker than it reads.** Of 32 violations injected into a scratch copy of the tree,
`scripts/commandments.py` missed 28; a separate set of 15 syscall-gate mutations was caught by none of its
checks. `I-irq-lock` never reads the three interrupt-handler files it declares. The 98 kernel host tests, Miri and mutation testing run
on no automatic path; the mutation job cannot fail and its last results are five months old. The host tests
for routing and naming test parallel models, although the real `ipc/routing.rs` turns out to host-compile
with one constant stubbed. Every section 22 QEMU suite boots x86 only.

**What can be captured in code (the operator's question): most of it, at named levels.** Four prototypes
were built and rerun for this report, each compiling or reading the real source, each with negative tests:

| Prototype | Level | Against | Result at `537e6861` |
|---|---|---|---|
| capability reference model | PROPERTY TEST | real `capability/*.rs` via `#[path]` | 8 x 40k ops pass; 16/16 fault mutants caught; 3 expected misses |
| IPC routing state machine | PROPERTY TEST (implementation level for `routing.rs`; scheduler side emulated) | real `ipc/routing.rs`, `queue.rs`, `message.rs` | 22 tests pass incl. 11 reproduced counterexamples; 11/12 mutants killed, 1 equivalent |
| kill/reclaim model checker | MODEL CHECK (sequential consistency only) | hand-extracted step model, 53 source anchors | 26 runs, 1.28M states; finds D7, D8, D9, D10; D7 and D8 fixes clean; 6 negative tests caught |
| syscall gate check | ARCHITECTURAL CHECK | `syscall/dispatch.rs` text | 51 arms, 0 failures; 13 mutants caught, 2 stated limits, 3 fail closed |

Section 14 says which of them, and which further checks, belong in the build, at what cost, and what each
does not establish.

The totals: 239 raw findings from 21 readers. 184 were upheld by a majority of their skeptics, 28 were
killed, and 27 were ARGUMENT SUPPORTED results that are not sent to skeptics. Many readers found the same
defect independently (seven found D1), so the upheld findings group into 29 distinct kernel defects (D1 to
D29), 6 enforcement-layer defects (E1 to E6) and one cluster of specification drift (section 12). Section 6
lists the defects, worst first; 7 are high, 15 medium and 7 low. Section 8 lists the 28 killed findings
with the reason, so they are not raised again. Appendix A maps every raw finding to its defect and verdict.

---

## 2. Scope, trusted base and assumptions

**In scope:** the architecture-neutral kernel at `537e6861`: `kernel/src/{capability,ipc,task,syscall,memory,
smp,interrupt,invariants}/` and `kernel/src/{loader.rs,main.rs,log.rs,clock.rs,elf_flags.rs,lib.rs}`, about
16,100 lines. `kernel/src/arch/<isa>` was read where a neutral invariant rests on it (user-copy validation and
fault fixups, interrupt masking, the tick path, boot order, page-table hardening, stack guards), and every
arch finding says which ISA was read. A result on one ISA is never carried to another.

**The real trusted computing base, beyond the Rust kernel.** Every guarantee below also rests on:

- **The toolchain:** `rustc` (nightly), `core`, the linker and the target specs. Several findings depend on
  codegen facts (stack probes are emitted on x86_64 and aarch64 only; `asm!` blocks for interrupt masking
  are marked `nomem`).
- **Hand-written assembly:** entry stubs, `switch_context`, trap vectors, the AP trampolines. Read at the
  points cited; not verified.
- **Firmware and boot loaders:** Limine and ACPI tables (x86), the Pi firmware and its mailbox (Pi 2, Pi 4),
  OpenSBI (RISC-V), the device trees. Their outputs are trusted at boot.
- **The hardware memory model and interrupt delivery:** x86-TSO, ARMv7, ARMv8 and RVWMO. The scheduler model
  is sequentially consistent only; a separate weak-memory pass used litmus approximations (section 9).
- **The IOMMU** where one confines a device (x86 AMD-Vi only), and on every other machine, every DMA-capable
  driver (CLAUDE.md 6.4).
- **The supervisor.** It is trusted root and the only `IMAGE_SPAWN` holder; the kernel mints what a spawn
  request asks within the checks in section 6 (D22). A compromised supervisor is out of scope.

**Assumptions used throughout:** an attacker is any task with the capabilities its spawn request gives it, or
a race between cores; the supervisor is honest; deadlines are judged against wall time; liveness is claimed
only under stated fairness (each core's timer fires, no task holds a kernel critical section forever).

---

## 3. Method, and the two scales

1. **Read.** Nine adversarial readers, one per area: capabilities (core, then transfer and IPC), IPC queues
   and blocking, scheduling and termination, memory, SMP locks and interrupts, the syscall surface, the
   commandment traceability matrix, and the strength of the existing tests. Each wrote an abstract model
   from the specification first, kept it apart from the implementation, and checked `Init => I` and
   `I and T => I'` for each invariant, naming the transitions that break it. Failed operations were not
   assumed atomic.
2. **Build.** Four prototype builders, each against the REAL source, each required to inject faults into
   copies and report caught and missed honestly.
3. **Refute.** Every COUNTEREXAMPLE, and every finding rated high or critical, went to three skeptics with
   different lenses: REACHABILITY (can untrusted code or a cross-core race actually drive it, from a syscall
   entry), CODE (re-read every cited line and look for the check the finder missed), SPECIFICATION (does the
   constitution, an amendment or a recorded limitation already permit it, or is it already on record).
   Medium findings got two, low and info one. A finding survives on a strict majority. ARGUMENT SUPPORTED
   results were not sent to skeptics and are labelled as such.
4. **Complete.** A completeness critic read every reader's coverage and named eight gaps; eight follow-up
   agents chased them and their findings went through the same skeptics.
5. **Repair the pass itself.** 27 skeptic checks failed when the session limit was reached, leaving 13
   findings under-voted. They were re-run before this report was written (section 8 notes the outcome).

In total 571 agents ran (544 in the main pass, 27 in the re-run). Each prototype was then rerun once by hand
for this report, and the kernel host tests were run again (98 pass).

**Two scales, kept apart.** Checks are graded ARCHITECTURAL CHECK, PROPERTY TEST, MODEL CHECK, IMPLEMENTATION
VERIFICATION, MACHINE-CHECKED PROOF or UNVERIFIED. Reasoning results are graded COUNTEREXAMPLE FOUND,
ARGUMENT SUPPORTED UNDER STATED ASSUMPTIONS, PROOF GAP or NOT EXAMINED. A passing check never upgrades a
reasoning result, and no result here is MACHINE-CHECKED PROOF.

**A verifier is credited only with the faults it was shown to catch.** Each prototype section lists its
mutants, caught and missed.

---

## 4. Traceability matrix

"Existing enforcement" says what the code ACTUALLY checks, measured by running each check on a copy of the
tree and injecting violations into the copy; it is not what the check's docstring says. MISSED means an
injected violation passed.

### 4.1 The Ten Commandments

| Commandment | Kind | Formal property | Existing enforcement (measured) | Proposed verification | Coverage limits |
|---|---|---|---|---|---|
| **I. Kernel responsibilities complete** | Architectural, static | `Resp = parse(CLAUDE.md 4.3)`, six; every top-level kernel module claims one of `Resp` or a support role; `Surface(tree) = Surface(baseline)` unless re-pinned, where Surface = syscalls, arch syscall intercepts, queries, ungated queries, ungated syscalls, authorities, features, dependencies, kernel spawns, arch roles | `I-syscalls` reads the enum, not the dispatch match (a raw arm is MISSED). `I-introspect` reads single-integer arms, not the ungated list or or-patterns (MISSED). `I-authorities` misses hex ids and `pub(crate)`. `I-kernel-deps` misses `[dependencies.x]`. `I-features` misses underscore names. `I-kernel-spawns` misses a const name. `I-responsibilities` derives the six from 4.3, but its support-role whitelist is unratcheted: a seventh responsibility enters with two baseline lines (MISSED). `I-arch-drivers` same. `I-irq-lock` reads 1 of its 4 scope entries (the `kernel/src/interrupt` directory, 2 files); its 3 declared arch files are never read (E1) | Fail-closed structural parsing of every pinned surface; pin the ungated syscall and query sets and the arch intercepts; a scope-fidelity selftest; ratchet the role tables (ARCHITECTURAL CHECK) | Growth inside one syscall's op space is not counted (acknowledged in CLAUDE.md 12.3). Honesty of a declared role is review |
| **II. Chaos; nothing escapes** | Policy (static) + liveness (runtime) | Static: no spawned service is transient except chaos and its own children. Runtime: N rounds, no panic, eventually every managed service running | `II-chaos-exclusions` sees only `name == "lit"` / `starts_with` in `is_transient`; a `matches!` escape is MISSED. Runtime: chaos is run by hand | Refuse any `is_transient` body that is not a disjunction of literals (ARCHITECTURAL CHECK); a QEMU chaos job with a threshold (PROPERTY TEST) | Other exclusion sites are not read; runtime evidence is x86 QEMU or hardware by hand |
| **III. One irreducible truth** | Judgment, one decidable instance | Every fact has one irreducible source; every derived view has a repair path and the source wins | `III-duplicate-constants`: the same integer const name in two files of one crate; cross-crate copies excluded | Kernel/SDK ABI consistency (prototype `abi_consistency.py`: privilege bits, spawn flags, syscall numbers agree today; catches a one-sided SDK flag change that `commandments.py` passes) | Undecidable in general |
| **IV. Honor contracts** | Architectural (declared vs granted) | For reconcilable keys, the contract claims a capability iff the spawn request grants it | `IV-contract-authority`: 5 keys through an unratcheted map; deleting a key silently stops reconciling it (MISSED). `contract_check.py`: memory, core, peers, class, resource_mint. Both inherit VII's blind spots | Require every schema capability key to be mapped or explicitly listed as not reconcilable (ARCHITECTURAL CHECK) | Behavioural use stays runtime-only (13.4) |
| **V. No service special; recovery loud** | Liveness + architectural | Every managed service is spawned watched; death leads to a respawn; a failed recovery is logged | `V-managed-watched`: a substring test, so `MANAGED.contains(&name) && name != "fs"` is MISSED. `V-no-panic`: six tokens in `services/` only; misses the assert family, `*`-prefixed lines, the SDK and stdlib (MISSED x4). Identity Tests 6, 11, 13, 15 (x86 QEMU) | Exact-shape `is_watched`; widen V-no-panic; a QEMU kill matrix per managed service (PROPERTY TEST) | Hang-freedom untested; recovery observed, not proved |
| **VI. No shared mutable state** | Safety | Every user frame is writable from at most one task unless declared shared (framebuffer, DMA arena, device window, each with one holder) | `VI-static-mut` matches `static mut` or `static X: Atomic...` at line start in `services/`. Misses path-qualified atomics, `pub(crate)` and arrays; on the real tree it misses four statics in `fs` (`services/fs/src/main.rs:4809, 4827, 4829, 4835`). Kernel statics: nothing. Frame and window exclusivity: a comment (`task/mod.rs:1930`) | Widen the pattern; a kernel mutable-static ratchet; a single-holder check per device kind (IMPLEMENTATION VERIFICATION) | Frame exclusivity needs a page-table walk; nothing does it |
| **VII. No ambient authority** | Safety | `Authority_after(t) subset Authority_before(t) union ValidGrants(t)`; every effect is dominated by its gate; a spawned child holds only what the spawner could delegate plus a pinned kernel-minted set | `VII-service-grants` reads `privbits::`, `hwclass::` and peer slices in the supervisor rows. Not read: request FLAGS (`SPAWN_FLAG_REQ_CONSOLE` gives CONSOLE_READ to chaos, observe-live and shell, unpinned on the real tree), the probe table, literal or const privilege words, `SUPERVISOR_DELEGATABLE` (all MISSED). Syscall gating: UNMECHANISED (`commandments.py:1825-1827`) | The syscall gate check (prototype C4); a spawn-mint inventory; host-evaluated grant tables (needs the supervisor table in its own file) | A static pin cannot see runtime transfers, by design |
| **VIII. Wait on truth, including failure** | Liveness | Every blocking wait returns iff its event happened, its producer died, or its deadline passed | Not mechanised (`commandments.py:1828-1830`). `quit_wait_check.py` covers shell waits only. Runtime: Test 4, `osdev test reply-dead`; P10 checks non-hang of `try_send` only | IPC harness HANG / SPURIOUS_WAKE checks (prototype C2); a TLA+ model of Send/Recv/Call/CallDeadline with kill (MODEL CHECK) | Any liveness claim needs fairness assumptions |
| **IX. Plan for recovery** | Liveness | After a peer restarts, every client either holds the new generation or reports failure, and discards state derived from the old one | `IX-peer-reacquire`: a raw-text substring including comments, alias-blind. `nic-driver` passes ONLY because of the comment at `services/nic-driver/src/main.rs:996`; its real path, `godspeed::cap::reacquire`, is not recognised | Strip comments, resolve crate aliases, require the reacquire on an `Err` arm (ARCHITECTURAL CHECK) | Derived state (socket ids, file caps) is not reached by any pattern |
| **X. Complexity where it belongs** | Judgment | Not stated mathematically | `X-user-vocabulary` (utilities vs shell verbs); the kernel half is filed under I | None beyond I | Judgment |

### 4.2 The constitutional invariants (CLAUDE.md 3)

| Invariant | Formal property | Existing enforcement | Status after this pass |
|---|---|---|---|
| 1 No ambient authority | `performs(t, op) => exists c in Caps(t): authorizes(c, op)` | VII (partial); adversarial A1, A4, A11-A13 (x86 QEMU); Tests 2, 3 | **COUNTEREXAMPLE**: query 21 pops the COM2 operator stream with no capability on x86 (D23); the aarch64 syscall range `>= 0x1000` is ungated (known A10-10); otherwise every neutral syscall reaches a cap check, except 8 ungated arms (Yield, AllocMem, TakePendingCap, QueryCapRights, RemoveCap, Park, LastRecvBadge, Sleep) that act only on the caller's own state (measured, prototype C4) |
| 2 No shared mutable memory by default | as VI | none static; P4 and P7 adjacent | **PROOF GAP**: one holder per device window is a comment, not code (the attack needs the supervisor; refuted as a counterexample, section 8) |
| 3 Authority explicit; delegation never mints | `Caps(child) subset {x : spawner holds x with GRANT} union KernelMintedAtSpawn` | none | **COUNTEREXAMPLE** to 14.1 step 4: CONSOLE_READ is minted from a request flag the supervisor does not hold (D22) |
| 4 Kernel remains tiny | as I | the I-* checks | **PROOF GAP**: the pins fail open on spelling, and two whitelists are writable (E2) |
| 5 Unsafe isolated and audited | unsafe lines only in the four layers plus the grandfathered floors; count <= audit; a SAFETY comment on each block | `unsafe_check.py` (counts, services zero, SDK floors, `deny(unsafe_code)`); it does not check placement or SAFETY presence | Placement holds on the real tree (measured: 0 lines outside). **COUNTEREXAMPLE** to 18.3: `task/scheduler.rs:1174, 1189, 1450, 2471` open unsafe blocks with no SAFETY comment above them, plus 12 arch blocks (16 in all) |
| 6 Services restartable | death leads to a running successor of the same name, under fairness | V-managed-watched; Tests 6, 11, 13, 15; chaos by hand | ARGUMENT SUPPORTED, observed not proved, x86 QEMU and hardware by hand |
| 7 Contracts enforced, not interpreted | runtime caps = f(spawn request); contract = request on reconcilable fields | `contract_check.py`, IV | PROOF GAP: the reconciling map is unratcheted |
| 8, 9 State explicit and owned; no unowned global mutable state | every mutable static has one owner and is reachable only through it | VI (services only) | PROOF GAP: the kernel core has 20 `static mut` and about 60 atomic or lock statics, none ratcheted; the `fs` statics claim an owner only in a comment |
| 10 Understandable | judgment | I-responsibilities counts modules | Judgment |
| 11 Identity stable, location not | restart keeps the name and raises the generation; no migration within a life | Test 10, P2, P8 (x86 QEMU); `assert_no_mid_execution_migration` | ARGUMENT SUPPORTED for restart; the migration assertion re-checks the filter `pick_next` already applies, so it cannot fire for the property it names (S1); a real migration window exists (D11) |
| 12 Failures loud | every syscall returns success or a defined error; nothing at the boundary is discarded silently | none; VIII is unmechanised; P10 discards results | **COUNTEREXAMPLE**: a blocking send can return success for a message never queued (D2); several stub syscalls report success (D27). The kernel core has 48 `let _ =` discards (29 in `loader.rs`), unratcheted |

### 4.3 What the property tests really test

- **P1** (`services/probe/src/main.rs:681-696`): 10,000 random u32 slots against a 64-slot table, so about
  1.5e-4 draws are in range; the probe holds no SEND cap, so even a broken lookup returns an error; the error
  kind is not asserted. It tests "no SEND cap, no send", not unforgeability.
- **P3** (767-814): one rights value, one path, 5,000 identical repeats. DeriveCap and spawn installs are not
  exercised.
- **P9** (698-721): three slots in one holder's table, one core. "All holders" is not exercised.
- **P10** (724-737): accepts any return value, discards results, calls only `try_send` on almost entirely
  out-of-range slots. "Exactly one defined outcome" and blocking `send` are not tested.
- **Every** section 22 suite boots `qemu-system-x86_64` (`osdev/src/qemu.rs:270-278`).
- The host tests of `routing_model.rs` and `names_model.rs` test parallel models; `memory/bitmap.rs` shares no
  algorithm with `memory/allocator.rs`.

---

## 5. State models, one per MISCIS responsibility

Each model was written from CLAUDE.md first and then mapped onto the code; the two are kept apart. Where the
implementation keeps LESS state than the model needs, that gap is usually where the defects are, and it is
called out. Statuses use the reasoning scale. The full per-area models, with every operation's pre- and
postconditions, are kept with the scratch evidence (section 15).

### 5.1 Capabilities

**Model.** `Res : ResourceId -> (gen, {Alive, Revoked, Dead})`; per task `Held : Slot(0..63) -> (rid, rights,
gen)`; `Owner : DelegatedId -> EndpointId` for the band `[4096, 6144)`; `Name : String -> EndpointId`;
`Peers : Task -> set of names`; `Ident : Task -> EndpointId`; a global generation counter and a LIFO free
list of endpoint ids. Authority of t = the caps in `Held(t)` whose generation matches a live record, plus
holdings-gated stable rights (ids below 100, never revoked), plus identity rights derived from `Ident(t)`,
plus `Peers(t)` for by-name reacquire.

**Implementation.** `Res` is TWO stores that must agree: `GLOBAL_RESOURCES` (`capability/table.rs`) and the
routing table's own generation and liveness (`ipc/routing.rs:39-52`). `Held` is the `TASK_CAP` static mut
array. `CapTable::get` is the gate; `holds_resource` is a second, generation-free gate for stable ids.

| Invariant | Status |
|---|---|
| I-RIGHTS: no installed cap is wider than a GRANT-validated source or a fixed kernel mint | ARGUMENT SUPPORTED; PROPERTY TESTED at table level (prototype C1) |
| I-REVOKE: after a bump, every older cap to the resource fails every use | ARGUMENT SUPPORTED; PROPERTY TESTED |
| I-DELEG: band allocation, ownership-checked revoke, owner death releases its band | ARGUMENT SUPPORTED single-threaded; **COUNTEREXAMPLE** concurrently: a dying minter can claim a band slot after `release_owner` ran, and `release_owner` frees a slot before marking it Dead (D14) |
| I-HOLDS (SEC-11): stable gate ids are never bumped, so holdings gates are sound | ARGUMENT SUPPORTED; its guard is a `debug_assert` absent from every shipped kernel (E6) |
| I-AUTH: `Authority_after subset Authority_before union ValidGrants` | **COUNTEREXAMPLE** at the syscall level: AcquireSendCap looks up and mints in two lock holds, so a reused id can yield a SEND cap to a different service (D5); spawn channels not bounded by the spawner (D22) |
| I-NORESURRECT: a stale (rid, gen) never validates again; a reused id starts above every earlier generation | PROOF GAP: holds today only because spawn always draws the reply endpoint's generation right after the primary's; `register_resource_at_gen` accepts a lower generation (D25) |
| I-KILL-REVOKES: a task marked Dead performs no further authorised effect | **COUNTEREXAMPLE**: the victim runs, with full holdings authority, until an IPI sent only after the whole teardown (D14), and a self-kill race lets it exercise the next occupant's caps (D10) |
| INV-ID-ACCOUNT: every endpoint id allocated is freed on every exit | **COUNTEREXAMPLE** (D15) |

### 5.2 IPC

**Model.** Per endpoint e: `alive`, `gen`, a FIFO `q` with `|q| <= 16`, a SET of receivers waiting, a
SEQUENCE of blocked senders each with its message, a set of callers awaiting a reply from e. Per task a state
in {Running, Ready, BlockedOnRecv(e), BlockedOnSend(e, m), BlockedInCall(own, target), Dead} and an
incarnation. Cancellation (a timeout, a ReplyDead return, an error return) removes the task from every
waiter set it is in. Death withdraws the task from every endpoint's waiter sets.

**Implementation.** `RoutingEntry` keeps ONE `blocked_receiver`, ONE `blocked_sender` and ONE `pending_send`
(`routing.rs:47-51`), written without checking that the slot is free; there is no cancel or withdraw
operation; registrations name a bare task slot with no incarnation; `CALL_AWAIT_EP[slot]` holds the awaited
target. The refinement from Set and Seq to Option is the root of D1, D2 and D3.

| Invariant | Status |
|---|---|
| I1 queue bound `0 <= len <= 16` | ARGUMENT SUPPORTED; PROPERTY TESTED on the real `routing.rs` (prototype C2) |
| I6, I7 generation discipline; no cross-generation delivery | ARGUMENT SUPPORTED; PROPERTY TESTED |
| I2 every blocked sender is woken exactly once (CLAUDE.md 8.6 row 4) | **COUNTEREXAMPLE** (D1), reproduced on the real code |
| I3 a waiter record names a task currently blocked there, in the incarnation that registered | **COUNTEREXAMPLE** (D2, D3), reproduced |
| I4 exactly one defined outcome per send; Ok iff queued (P10) | **COUNTEREXAMPLE** (D2) |
| I5 no silent loss | **COUNTEREXAMPLE**: an overwritten `pending_send` is dropped with no count or log (D1) |
| I8 a Call takes only a reply from the target's current incarnation | **COUNTEREXAMPLE**: correlation is by bare, LIFO-reused endpoint id (D5) |
| I10 CallDeadline returns within its bound | **COUNTEREXAMPLE**: the send leg has no deadline (D4) |
| RW a delivered reply wins over ReplyDead | **COUNTEREXAMPLE** (D6) |
| I9 no lost wakeup | PROOF GAP: holds in 33,000 sequential cases; the concurrent interleaving is not explored |

### 5.3 Scheduling and termination

**Model.** Per slot: a ghost life counter (incremented at each reserve), `valid`, `state`, `core`, context,
kernel stack, page-table root, caps. Per core: `current`, `leaving`, and a ghost `on` (the stack and root the
core is physically executing on). The run queue is the predicate `{s : valid, Ready, core = c}`. Wake
records, registrations, `MONOTONIC_TICKS` and the one pending power action.

**Implementation.** There is no life counter: every registration stores a bare slot number, and
`wake_by_slot` moves ANY non-Dead state to Ready. `on(c)` is not represented; `CORE_LEAVING` stands in for it,
which is exactly where D7 breaks.

| Invariant | Status |
|---|---|
| I1 run-queue membership; at most one queue per slot | ARGUMENT SUPPORTED |
| D Dead is absorbing within a life | ARGUMENT SUPPORTED; MODEL CHECKED (never violated in any run) |
| S switch-in safety (the Dekker claim handshake) | ARGUMENT SUPPORTED under sequential consistency; the re-check is load-bearing (a mutant without it gives use-after-free); weak memory: no outcome found in litmus approximations, not proved |
| I7 no migration within a life | ARGUMENT SUPPORTED in the common case; **COUNTEREXAMPLE** through the run-loop stale pick (D11) |
| Q / I5 reclaim only after quiescence (CLAUDE.md 10.5 amendment) | **COUNTEREXAMPLE** (D7), MODEL CHECKED |
| F each deferred kernel stack or root is freed exactly once | **COUNTEREXAMPLE** (D8), MODEL CHECKED |
| B deferred roots are never leaked | **COUNTEREXAMPLE** (D9), MODEL CHECKED |
| C a self-killed task's core never touches the slot's next occupant | **COUNTEREXAMPLE** (D10), MODEL CHECKED; safety today rests on a timing assumption |
| I6 no cross-life delivery of a wake, error, deadline, badge or pending message | **COUNTEREXAMPLE** (D3) |
| L every started kill completes | **COUNTEREXAMPLE**: crossed kills wait on each other until the time budget abandons them; unbounded where the liveness deadline is 0 (D7) |
| I9 timed wakes and the power deadline track time | **COUNTEREXAMPLE** (D12) |
| I11 every Ready task eventually runs, under timer fairness | ARGUMENT SUPPORTED under the stated fairness |
| I12 `switch_context(cur, next)` has `cur != next` | PROOF GAP (low, upheld by its one skeptic): `block_and_reschedule` can call it with both equal (the trace is confirmed); whether that is harmful depends on each arch's `switch_context` with aliased pointers, read for x86 and arm32 only (D11) |

### 5.4 Memory isolation

**Model.** Physical frames with a free bitmap, kernel-page-table protection and permanent DMA reservations;
an abstract owner per frame (the implementation stores none and reconstructs ownership from page tables at
death). Per task: an address space, a budget `alloc_bytes <= limit_bytes`, and grants (device windows, DMA
arena, framebuffer) that are mapped but not owned for reclaim.

| Invariant | Status |
|---|---|
| M1, M2 no phantom free; a double free does not inflate the free count | ARGUMENT SUPPORTED |
| M3 W^X on loaded user segments | ARGUMENT SUPPORTED |
| M4 user-pointer range and overflow check before every copy | ARGUMENT SUPPORTED (range only; the fault-guard race is D19) |
| M5 page-table frames freed exactly once, after quiescence | ARGUMENT SUPPORTED for the cross-kill path; the deferred path breaks it (D8) |
| M9 the allocator never returns a live, kernel or reserved frame | ARGUMENT SUPPORTED (host evidence is a model only, E4) |
| M6 the AllocMem budget equals what is mapped (P4) | **COUNTEREXAMPLE**: the full budget is charged before mapping, so a mid-loop failure leaves the charge above the pages mapped, and the data frame leaks (D27, the leak known as A10-15) |
| M7 no user page maps onto a kernel or shared VA | PROOF GAP: the loader never bounds `p_vaddr` against the user ceiling; on x86 that reaches the shared kernel half (D29, known A5-10, blast radius understated) |
| M8 fixed per-task VA regions are pairwise disjoint | the heap base equals the PCI MMIO window base on every 64-bit port; refuted as reachable (section 8) |
| Kernel W^X | holds on x86 only (D21) |

### 5.5 Interrupts

**Model.** `IRQ_TABLE : vector -> endpoint`, `Mask : vector -> {masked, open}`, a per-endpoint notification
reserve. Invariant I5: a masked level line with a live owner implies a queued notification, or an owner that
consumed one and owes `IrqUnmask`.

| Invariant | Status |
|---|---|
| I4 an IRQ route is owned only by the live task that registered it | **COUNTEREXAMPLE**: a failed driver spawn leaves the route on a freed id that LIFO reuse hands to another task, with its `IrqUnmask` authority (D15, partly known); the kill path frees the reply id before unregistering routes (D14) |
| I5 no silent permanent mask | **COUNTEREXAMPLE**: `deliver` masks a level line and then skips the notification under queue pressure; nothing unmasks it and nothing reports it (D18) |
| I7 every interrupt loss is reported (invariant 12) | **COUNTEREXAMPLE** (D18) |
| riscv64 masked state reachable | ARGUMENT SUPPORTED that it is not: no `route::deliver` call site exists there and the mask seam is a no-op |
| I8 x86 IOAPIC indirect access is atomic per register | PROOF GAP: unlocked, cross-core; the claimed harm was refuted (section 8) |

### 5.6 SMP synchronisation

**Model.** Per core an interrupt flag and a context (scheduler loop, task, ISR); locks with acquire modes
(`lock` leaves IF unchanged, `lock_irq` masks); the ready set may be non-contiguous after a failed AP
(CLAUDE.md 11.3); a timebase intended to advance once per BSP timer period.

| Invariant | Status |
|---|---|
| I2 lock order acyclic | ARGUMENT SUPPORTED over the edge set found (no cycle); not exhaustive for arch locks |
| I15 the kill vs pick handshake | ARGUMENT SUPPORTED (independently of 5.3's model) |
| I16 per-core arena publication | ARGUMENT SUPPORTED |
| I1 a lock taken at IF=0 is never held by a preemptible context on the same core | **COUNTEREXAMPLE**: `FREE_IDS` is taken with plain `lock()` by the preemptible supervisor respawn (D16) |
| I3 deferred free exactly once | **COUNTEREXAMPLE** (D8) |
| I6 the timebase advances only with time | **COUNTEREXAMPLE** (D12) |
| I9 placement within the ready set; I10 the watchdog covers every ready core | **COUNTEREXAMPLE**: `ready_count()` is used as a bound on core ids (D17) |
| I13, I14 per-core single owner; name tables data-race free | **COUNTEREXAMPLE**: `TASK_NAMES.set()` aliases the whole table with `&mut` while other cores read it (D24) |
| I12 a panic is always reported | PROOF GAP: the panic path takes the ordinary FB and RING locks (D28) |

### 5.7 The syscall boundary (cross-cutting)

51 dispatcher arms, 27 InspectKernel queries, multi-op PowerOff and ConsoleForeground. The prototype check
(C4) finds 43 arms gated by a recognised returning gate (21 slot-validated, 20 holdings, 2 ownership) and 8
ungated by design, each confined to the caller's own state on reading. The ungated InspectKernel set
`{0, 3, 10-21, 23, 28}` is claimed (`dispatch.rs:1886-1893`) to disclose nobody's state;
that claim is broken by query 21 on x86 (D23). "A result `< 0` means no effect" is broken by SpawnImage and
AllocMem (D27).

---

## 6. Verified counterexamples: the kernel defects

Each entry gives the reasoning class and severity AFTER the skeptics (the readers' own ratings were often
higher), how many independent readers found it, the skeptic vote, the decisive lines, the violated
invariant, the trace, the assumptions, the minimal correction (NOT applied) and the check that would catch it
or its regression. "Reproduced" means a host prototype drove the REAL kernel source into the state; every
other trace is by reading, upheld by skeptics who re-read the code. None was observed on a running machine.

Severity is judged by consequence and reachability together: high means memory unsafety, a hang that cannot
be recovered from, or a silent wrong answer at the kernel boundary, reachable by ordinary services or a
cross-core race; medium means the same class with a narrower trigger, or a bounded leak, or a wrong error;
low means supervisor-only, cosmetic, or an argument that is fragile rather than broken.

### 6.1 High

#### D1. A second blocked sender erases the first

**COUNTEREXAMPLE, high.** Found independently by 7 readers; upheld 3/3 by each panel. Reproduced on the real
`ipc/routing.rs`. Not previously recorded: `audits/security-audit.md:230` marks the blocked-sender
`EndpointDead` path "traced race-free", which considered one sender only.

- **Where:** `kernel/src/ipc/routing.rs:47-51` (one `blocked_sender`, one `pending_send`), `:445-449` and
  `:432-435` (written with no check that the slot is free), `:716-717` and `:725-726` (the console's
  kernel-blocking path), `:782-784` (kill returns only the recorded one).
- **Invariant:** every task told QueueFull and recorded as blocked is released exactly once, by a dequeue
  that delivers its message or by the endpoint's death (CLAUDE.md 8.6: "sender already blocked when endpoint
  closes: wakes with EndpointDead").
- **Trace:** A and B hold SEND to E, whose owner is not receiving; 16 sends fill E. A sends: recorded as the
  blocked sender with message mA, blocks (`dispatch.rs:378`). B sends: the same two lines overwrite the
  record with b and mB; mA is dropped, uncounted and unlogged. When E's owner receives, B's message is
  promoted and B woken; A has no record anywhere and stays `BlockedOnSend`. If E's owner is killed instead,
  only B is woken with EndpointDead. A hangs until it is itself killed.
- **Also on:** SendWithCap (the carried cap was removed from A at `dispatch.rs:1450`, so the authority is
  destroyed), Call and CallDeadline (the send leg, which has no deadline, D4), ResourceInvoke, and
  ConsoleWrite.
- **Reproduced:** `cx1_raw_kill_returns_only_the_last_sender` prints `kill_endpoint -> rx None tx Some(12)
  (slots 11 and 12 were both blocked)`; the generated search shrinks it unaided to three operations.
- **Assumptions:** two tasks with SEND to one endpoint whose receiver lags by 16 messages, as any two clients
  of `fs` or of the supervisor can be. How often that happens in production was not measured.
- **Correction (not applied):** keep `pending_send` single, but record further blocked senders as a bounded
  set of slots (a bitmask per entry); on dequeue, promote the pending message and wake one recorded slot to
  retry; on kill, wake every recorded slot with EndpointDead. Or refuse a second blocking sender with
  QueueFull and a loud log. Count any dropped pending message.
- **Check:** the IPC harness HANG check (PROPERTY TEST on the real code); an ARCHITECTURAL CHECK that no
  `blocked_sender = Some(` write lacks a guard; a QEMU identity case 4C with two blocked senders.

#### D2. Wait registrations outlive the wait

**COUNTEREXAMPLE, high** for the receive side, medium for the Call side. Found by 4 readers; every panel
upheld 3/3. Reproduced on the real `routing.rs`.

- **Where:** `kernel/src/syscall/dispatch.rs:516` (`try_recv` passes `Some(my_slot)`, so an empty queue
  records it as the blocked receiver, `routing.rs:491-494`), `:530` (returns `TRY_RECV_EMPTY` without
  blocking), `:582` (`recv_timeout` exits on timeout with the registration left), `:1630-1640` (CallDeadline
  times out leaving `CALL_AWAIT_EP` and the receiver registration), `routing.rs:593-595` and `:607-616`
  (the ReplyDead wake clears only `CALL_AWAIT_EP`).
- **Invariant:** a waiter record names a task currently blocked there; a send returns 0 only if its message
  is queued; a failed receive has no effect.
- **Trace (the wide form, no race needed):** T calls `try_recv` on its empty endpoint E, leaving
  `E.blocked_receiver = T`. T then sends to a full endpoint F and blocks as F's sender. A client sends to E:
  the enqueue takes the stale record and returns T; `wake_by_slot(T, 0)` moves T from BlockedOnSend to Ready
  with result 0 (`scheduler.rs:2365-2378`). T's send returns 0 although its message is still only
  `pending_send` on F. If T sends again, D1's overwrite drops the first message: reported sent, never
  delivered. The same stale record is consumed by interrupt delivery and by death notices to the supervisor.
- **The Call side:** a CallDeadline that timed out leaves `CALL_AWAIT_EP` set; when the old callee later dies,
  the caller is woken with ReplyDead (-12) in whatever it is doing, and in `ctx.recv()` the SDK then panics,
  killing a healthy service.
- **Reproduced:** `cx2_raw` prints `enqueue after an empty try_recv -> Ok(Some(21))`; the search shrinks a
  MISMATCH plus SPURIOUS_WAKE to two operations in the original run (four in this report's rerun; the shrink
  depends on the seed), and finds the CallDeadline and ReplyDead forms in three or four.
- **Reach:** `try_recv` is used by the supervisor, shell, console, block-driver, net-stack, xhci, dwc2, ehci
  and chaos (grep).
- **Correction (not applied):** `try_recv` passes `None`; add `routing::cancel_wait(ep, slot)`, which clears
  a registration only if it names the caller, and call it on every timeout and error exit of
  `recv_timeout`, CallDeadline and Call.
- **Check:** IPC harness SPURIOUS_WAKE, FALSE_OK and LOSS_AFTER_OK (PROPERTY TEST); an ARCHITECTURAL CHECK
  that every path registering a wait either blocks on it or withdraws it (four sites fail today).

#### D7. An aborted claim drops the "still leaving" record, and a kill frees a stack in use

**COUNTEREXAMPLE, high.** Found by 3 readers (the scheduler reader, the model checker and the weak-memory
follow-up); upheld 3/3 each. Found by the model checker; not reproduced on a machine. Not previously
recorded.

- **Where:** `kernel/src/task/scheduler.rs:85-91` (`core_release_current` reads `CORE_CURRENT` to decide
  what it is leaving), and the abort branches `:2033` then `:2040` (timer), `:2220` then `:2227` (yield),
  `:3280` then `:3289` (block); the kill wait `:69-72` and `:2979`; reclaim `:3101`, `:3116`, `:3173`.
- **Invariant:** a core executing on task t's stack or page tables has `CORE_CURRENT = t` or
  `CORE_LEAVING = t`; nothing is freed while a core executes on it (the CLAUDE.md 10.5 amendment).
- **Trace (model, mapped to code):** core 0 runs P, with N ready. Core 2 kills P and spins on core 0. Core 0
  takes the IPI, picks N, claims it (`LEAVING = P`, `CURRENT = N`). Core 1 kills N in that instant. Core 0's
  re-check sees N dead and aborts; the abort calls `core_release_current(IDLE)` again, which re-reads
  `CURRENT = N` and stores `LEAVING = N`. Core 0 is still on P's kernel stack and root until
  `switch_context` (`:2085` on the timer and wake-IPI path, which is the one x86's WAKE_RECEIVER stub takes;
  `:2276` via yield; `:3291` via block). Core 2's wait now sees neither naming P, and frees P's root and
  kernel stack. Core 0 keeps pushing onto the freed, possibly re-allocated stack.
- **Assumptions:** sequential consistency; one cross-core kill of P plus a kill of N, or an ordinary wake of
  N, inside the window (a skeptic showed a second kill is not needed). The window is tens of instructions.
- **Also (low):** two killers whose cores still name each other's victim in `CORE_LEAVING` wait on each other
  with interrupts masked until the time budget (0.75 s on x86, 2.5 s elsewhere) abandons at least one kill,
  leaking its slot. A claimed silent wedge where the liveness deadline is 0 was refuted: no built
  configuration has that deadline with more than one core.
- **Correction (not applied):** on the abort branch after a successful claim, store only `CORE_CURRENT =
  IDLE` and leave `CORE_LEAVING = prev` until the next scheduler entry (a `core_abort_claim` helper at all
  three sites). The model shows every CLAIM_GAP and use-after-free scenario clean with it; the crossed-kills
  wait also needs `LEAVING` cleared when the switch completes.
- **Check:** `schedmodel.py` (MODEL CHECK; must keep reporting the faithful violations and clean fixes); an
  ARCHITECTURAL CHECK that no path calls `core_release_current` twice before a switch; chaos-build
  assertions in `free_kstack` and `free_page_table_root` that no core names the owner.

#### D8. The deferred-free drain is re-entered on the same core and frees twice

**COUNTEREXAMPLE, high.** Found by 2 readers; upheld 3/3 each. Found by the model checker. Not previously
recorded.

- **Where:** `kernel/src/task/scheduler.rs:1545` (`run()` drains with interrupts ON, after `enable_interrupts`
  at `:1615`), `:1737` (the timer ISR drains the same per-core queue), `:2466` and `:2469` (load the length,
  then store 0: not atomic against the nested drain), `:2471` (the array is never cleared), `:2482-2489`
  (the pending root: load, store 0, free).
- **Invariant:** each deferred kernel stack or root is freed exactly once, and only while still owned by the
  dead task.
- **Trace:** a task self-kills (any fault), deferring its stack and root; the core returns to `run()`, which
  re-enables interrupts and calls the drain. Between the length load and the store of 0, the timer fires and
  its drain frees the same stack and root. The run loop resumes and frees them again, possibly after another
  core's spawn has taken that kernel stack. Two live tasks can then share one kernel stack; on ARM a freed L1
  table can be handed to two address spaces.
- **Note:** the comment at `:2468` says "Clear before processing so re-entrant callers see an empty queue";
  re-entrancy was anticipated, but a load followed by a store is not an atomic take.
- **Correction (not applied):** take the work atomically with interrupts masked (`swap(0)` on the root, copy
  and zero the entries), or run the whole drain inside `without_interrupts`.
- **Check:** `schedmodel.py drain_reentry` (MODEL CHECK); an ARCHITECTURAL CHECK that every drain call site
  is masked; a host PROPERTY TEST over the queue if it is extracted to a host-compiled type.

#### D10. A self-kill releases its slot before the core leaves it

**COUNTEREXAMPLE, high.** Found by 3 readers; upheld 3/3, 3/3 and 1/1. Found by two models (the scheduler
model and a dedicated slot-reuse model). Not previously recorded (SEC-9 is a different race).

- **Where:** `kernel/src/task/scheduler.rs:3163-3166` (`TASK_VALID[slot] = false` in the self-kill branch,
  while the core still runs the dead task); the decisions taken afterwards on that core from slot-indexed
  state, `:2108-2121`, `:2193-2203`, `:2258-2276`, and `dispatch.rs:1233` (`current_task_is_dead()`).
- **Invariant:** once a self-kill releases slot s on core A, nothing core A does may read or write slot s's
  state; a task marked Dead never runs user code again.
- **Trace (the worst form):** task X holds SERVICE_CONTROL and kills its own name. The kill frees X's user
  frames and page tables inline, releases the slot and returns. On core B, a spawn reserves the lowest free
  slot, which is X's, and commits it as Ready. Back on core A, `handle_kill` asks `current_task_is_dead()`,
  which reads the NEW occupant's state (Ready) and says no; the syscall returns to X's user code. X now runs
  with `CORE_CURRENT = s`, so every syscall resolves capabilities from the NEW task's table: the dead task
  exercises another service's authority, through stale TLB entries to frames already freed. Other forms
  clobber the new task's saved context or halt the machine.
- **Assumptions:** a spawn completing inside the self-kill tail on another core; the code relies on that
  being too slow, which is a timing assumption, contrary to Commandment VIII.
- **Correction (not applied):** record the self-kill per core, not per slot (a `CORE_SELF_KILLED[cid]` flag
  set before releasing the slot, consulted by the yield and `handle_kill` decisions, cleared after the
  switch away).
- **Check:** `schedmodel.py selfkill_slot_reuse` and `selfkill_reuse_model.py` (MODEL CHECK); a fault-
  injection feature that widens the window in QEMU (PROPERTY TEST).

#### D12. The tick clock counts handler entries, not time

**COUNTEREXAMPLE, high.** Found by 3 readers plus 4 follow-up findings; upheld 3/3 throughout except one
single-skeptic (1/1) finding. Not previously
recorded (the x86 cousin A9-4 was a different mechanism and is fixed).

- **Where:** `kernel/src/task/scheduler.rs:452` (`MONOTONIC_TICKS.fetch_add(1)` in `scan_timed_wakes`),
  `:1755-1766` (called whenever `timer_tick_from_irq` runs on core 0, whatever entered it);
  `kernel/src/arch/x86_64/boot.rs:1546` and `:1601-1612` (the wake IPI stub calls `timer_tick_from_irq`);
  `kernel/src/arch/arm/irq.rs:566` and `:625` (the hi-res wake does too); `kernel/src/arch/riscv64/mod.rs:1776-1797`.
- **Invariant:** one tick is one 10 ms quantum (`scheduler.rs:276-280`, `:338`); a deadline never fires
  early, and arrives within a bounded error.
- **Trace (x86, early):** the shell arms `shutdown 10m` (600 s = 60,000 ticks). Every time a task on another
  core wakes a core-0 task (fs replying to the shell, any service replying to the supervisor), core 0 takes
  a WAKE_RECEIVER IPI, enters `timer_tick_from_irq`, and adds a tick. At about 1,000 such wakes a second the
  shutdown falls due in under a minute. Every `sleep`, `recv_timeout` and CallDeadline on x86 shortens the
  same way.
- **Other ports:** on arm32 each sub-tick sleep woken on core 0 adds a tick (dwc2 paces split transactions
  this way), and a tick that lands inside a running user syscall is skipped entirely. On riscv64 core 0's
  tick is pushed back at every idle halt and every switch-in, so under ordinary IPC load the clock, and the
  power deadline with it, can stall. On every port, masked stretches on core 0 (synchronous serial, the kill
  wait) drop ticks with no catch-up, so deadlines arrive late without bound. aarch64 was checked and its
  SGIs do not inflate the count.
- **Why it matters now:** the shutdown and reboot deadline shipped in v0.28.0 rides this clock, and
  CLAUDE.md 12.3 promises it "fires whether or not any service is alive"; it does, but not at the time asked.
- **Correction (not applied):** advance the tick only on the real timer interrupt (an entry flag from the
  arch, or separate tick and reschedule entries); better, derive deadlines from each arch's calibrated
  counter rather than an interrupt count.
- **Check:** an ARCHITECTURAL CHECK that only the arch timer handler reaches the tick-advancing entry; a QEMU
  IMPLEMENTATION VERIFICATION that arms a 20 s deadline under cross-core IPC load and compares query 28 with
  the TSC.

#### D19. The user-copy fault guard is per core (A10-2, still open), and can stick

**COUNTEREXAMPLE, high. KNOWN:** `audits/kernel-audit.md` A10-2 (HIGH, open), including the stuck-armed
"mirror-image" case on aarch64 (`:310-313`). Upheld 3/3. This pass adds the x86 instance of the stuck guard.

- **Where:** `kernel/src/arch/x86_64/syscall_entry.rs:155`, `:176-187`, `:228-260`; `boot.rs:1842-1848`;
  `kernel/src/arch/aarch64/uaccess.rs:61-66`, `:235-320`.
- **The known half:** a copy after a block runs with interrupts enabled; a preempting task's copy on the same
  core stores false to the shared guard, so the first task's fault is no longer recognised as a user copy.
- **What this pass adds (x86):** a task preempted mid-copy and then killed from another core never clears
  `USER_COPY_ACTIVE`, so it stays true on that core. A genuine kernel null dereference in a later syscall on that core is then
  classified as a user-copy fault and kills the task, where the comment at `boot.rs:1836-1840` promises a
  loud halt.
- **Also:** riscv64 carries `sstatus.SUM` across a preempted copy into the next task; `read_user_bytes`
  returns a `&'static` slice into a per-core buffer that the next copy overwrites (on arm32, a borrow of raw
  user memory).
- **Correction (not applied):** as A10-2 asks: mask interrupts across each guarded copy, or record the
  guarded task and compare it with `CORE_CURRENT` in the fault handler; clear the guard when a victim is
  killed mid-copy.
- **Check:** a debug assertion that interrupts are masked at each guard set (IMPLEMENTATION VERIFICATION); a
  QEMU probe racing a faulting receive against a same-core logger (PROPERTY TEST).

### 6.2 Medium

#### D3. A dead task's wait records survive it and hit the slot's next occupant

**COUNTEREXAMPLE, medium.** Found by 8 readers (six plus two follow-ups); the core trace upheld 3/3 each,
the per-slot class 2/3. Reproduced on the real `routing.rs`.

- **Where:** the kill path cleans only the victim's own endpoints and its `CALL_AWAIT_EP` record
  (`kernel/src/task/scheduler.rs:2576-2640`); a record of the victim as `blocked_sender` on SOMEONE ELSE's
  endpoint survives (`routing.rs:500-507`, `:782-785`). The `CALL_AWAIT_EP` clear itself runs before the
  victim is quiescent (`:2576`), so the victim can re-register and leave a stale record behind.
- **Trace:** A, in slot X, blocks sending to C's endpoint. A is killed; slot X is reused by B, which blocks
  receiving on its own endpoint. C dies: its endpoint's kill returns X as the blocked sender, and
  `wake_by_slot(X, -7)` makes B's receive return EndpointDead on its OWN live endpoint. If C receives instead,
  A's dead message is delivered posthumously and B gets a spurious wake.
- **The class:** wake records are keyed by a bare slot index with no incarnation, and per-slot fields outlive
  their task (low, upheld 2/3): a mechanical scan listed seven, of which the skeptics found two or three with
  a real consequence: `TASK_WAKE_DEADLINE` gives the next occupant a spurious wake and `TASK_RUN_TICKS` shows
  the dead task's CPU time. `TASK_LAST_BADGE` is the same class and already recorded (A10-18, A11-8). On
  arm32 a task killed in a sub-tick sleep leaves its hi-res entry armed. SEC-35 fixed one field of this
  class.
- **Correction (not applied):** on death, remove the slot from every routing entry's sender record under the
  table lock and release its hi-res entry; or stamp every registration with a per-slot life counter and have
  `wake_by_slot` ignore a mismatched life, which closes the class.
- **Check:** IPC harness `kill_blocked_sender` (PROPERTY TEST); the prototype `per_slot_reset_check.py`
  (ARCHITECTURAL CHECK: every per-slot static is reset at reserve or death, or listed with a reason).

#### D4. CallDeadline's bound does not cover the send leg

**COUNTEREXAMPLE, medium.** Found by 5 readers; upheld 3/3 each.

- **Where:** `kernel/src/syscall/dispatch.rs:1557-1575` (the request enqueue blocks on QueueFull), `:1586-1590`
  (the deadline is computed only after it), `kernel/src/task/scheduler.rs:464-473` (timed wakes only wake
  `BlockedOnRecv`).
- **Trace:** CallDeadline(target, 1 s) to a live target whose queue is full and not draining: the caller
  blocks as a sender with no deadline armed, and returns only when the target drains or dies. With D1, a
  second caller's request erases the first's registration and the first is stranded for good.
- **Invariant broken:** the CLAUDE.md 8.2 amendment (CallDeadline exists to bound `call`, "what 26.6 asks of
  every wait").
- **Correction (not applied):** arm the deadline before the send; let the timed-wake scan wake an expired
  `BlockedOnSend` task; on that wake withdraw the undelivered request and return the timeout code.
- **Check:** a QEMU adversarial case (a 1 s CallDeadline to a pre-filled, non-draining endpoint must return
  within about 1.1 s).

#### D5. Identity keyed on a bare, reusable endpoint id

**COUNTEREXAMPLE, medium** (the Call liveness case), low for the rest. Found by 5 readers across 12
findings. Partly KNOWN: the late-reply hazard is recorded at protocol level (`backlog/fixed/70`,
`backlog/46`, `audits/userspace-audit.md` A9-10) and mitigated in userspace by correlation tags; the kernel
claims below are not.

- **Call liveness** (`kernel/src/ipc/routing.rs:581-589`): `call_dequeue` checks the replier's liveness by id
  only. A request sent to a dying incarnation of X, whose id is freed last and so sits on top of the LIFO
  free list, is answered by nobody: the respawned X reuses the id at a fresh generation, the caller finds it
  "alive" and waits forever (CallDeadline reports a timeout instead of ReplyDead). Reproduced on the real
  routing code by the IPC reader's harness (`cx_ipc4`).
- **Reply correlation** (`dispatch.rs:741`, `queue.rs:87-90`): a reply is matched by `sender_ep` alone, so a
  dead incarnation's late reply satisfies a call to its respawn; `message.rs:35-37` claims a dead
  incarnation's id "differs", which LIFO reuse makes false.
- **AcquireSendCap** (`dispatch.rs:1312` then `:1350`) and spawn peer wiring (`task/mod.rs:2039`, `:2046`)
  resolve the name and mint in two separate lock holds; an id freed and reused in between yields a live SEND
  cap to a different service. Same class as SEC-40 (ResourceInvoke), which is recorded; this path is not.
- **ResourceInvoke** passes the owner endpoint's CURRENT generation (`dispatch.rs:1771-1773`), so the 8.7
  generation check cannot reject a reused owner id.
- **The "one-shot reply cap"** is an ordinary SEND|GRANT cap: the replier keeps it, can copy and re-delegate
  it, and a later message on it satisfies the caller's next Call (a convention, `backlog/67`, not enforced).
- **Correction (not applied):** pass the target cap's generation into `call_dequeue` and treat a generation
  mismatch as ReplyDead; stamp `sender_ep` with the generation; bind AcquireSendCap's mint to the generation
  read under the name lookup.
- **Check:** `cx_ipc4` (the IPC reader's harness) and `cx5` (C2) (PROPERTY TEST); the capability prototype's lookup-then-mint
  probe (IMPLEMENTATION VERIFICATION ledger); an ARCHITECTURAL CHECK forbidding `names::lookup` followed by
  `mint_cap` without a generation binding.

#### D6. A delivered reply loses to ReplyDead

**COUNTEREXAMPLE, medium.** Upheld 3/3. Reproduced (routing half) by the IPC reader's own harness
(`$V\ipc\harness`, test `cx_ipc3`), not by prototype C2, whose single schedule cannot express it.

- **Where:** `kernel/src/syscall/dispatch.rs:1639`: `if err != 0 { break err; }` after the reply wait,
  while the comment at `:1629-1631` says the wake result is deliberately ignored and re-derived.
- **Trace:** X replies to C, waking C with 0. Before C runs, X dies; the kill path still finds C awaiting X
  (the record is cleared only when C runs) and overwrites C's wake result with -12. C returns ReplyDead with
  the reply sitting in its queue; if X respawns on the same id, C's next Call to X takes that orphan as the
  answer to a different request.
- **Correction (not applied):** ignore the wake result and always re-run `call_dequeue`, which already
  returns ReplyDead when the target is dead and nothing is queued.
- **Check:** `cx_ipc3` (PROPERTY TEST at routing level).

#### D9. The pending page-table root is one slot; the pending-stack overflow frees the stack in use

**COUNTEREXAMPLE, medium.** Found by 3 readers; upheld 3/3. Found by the model checker.

- **Where:** `kernel/src/task/scheduler.rs:852` (one `CORE_PENDING_PML4` per core), `:3107` (stored
  unconditionally), drained only at `:1545` and `:1737`; `:3156-3160` (PROOF GAP, low: the overflow branch
  frees the kernel stack the core is still running on; nine undrained self-kills on one core were not shown
  reachable; acknowledged only in the comment at `:3157-3158`).
- **Trace:** T faults and self-kills (its root is deferred); the yield switches straight into U, which also
  faults before the next tick: U's root overwrites T's, and T's root is never freed. On x86 that is one frame;
  on arm32 it is one of 32 static L1 slots, lost until reboot, so repetition ends in spawn failures with RAM
  free.
- **Correction (not applied):** free any previously pending root at the store (a second self-kill on the core
  proves it already switched off the first), or give roots the same small queue as stacks; never free the
  current stack on overflow.
- **Check:** `schedmodel.py pml4_overwrite` and `drain_reentry+cap1` (MODEL CHECK).

#### D11. The run loop claims a slot picked before an interruptible window

**COUNTEREXAMPLE, medium.** Upheld 3/3 (the reader rated it high). Found by a model.

- **Where:** `kernel/src/task/scheduler.rs:1569` (`pick_next` with interrupts enabled), `:1573` (masked only
  after the pick), `:1586-1593` (the claim CAS compares only the state value).
- **Trace:** core 0's `run()` picks slot s; an interrupt before the mask switches away for a while; s is
  killed, reclaimed and re-spawned as a new life pinned to core 2, which has not yet claimed it. Core 0
  resumes and its CAS succeeds on the new life: the user-task path then trips
  `assert_no_mid_execution_migration` and panics, which is a userspace-driven kill and spawn panicking the
  kernel; a ring-0 task would migrate. `block_and_reschedule` can also call `switch_context` with the
  current and next contexts equal.
- **Correction (not applied):** mask before `pick_next` in `run()`; after every successful claim CAS, check
  `TASK_CORE[next] == cid` and abort if not.
- **Check:** `weakmem/stale_pick.py` (MODEL CHECK, with clean negative controls); an ARCHITECTURAL CHECK that
  `run()` masks before picking.

#### D14. The kill path publishes the death before it finishes tearing down

**COUNTEREXAMPLE, medium.** Found by 7 readers across 9 findings; upheld 2/3 or 3/3 (two supporting proof
gaps 1/1).

- **Device state:** `kill_task_by_slot` sets Dead, unregisters the name and notifies the supervisor
  (`kernel/src/task/scheduler.rs:2560-2825`) BEFORE releasing the device (`:2890-2903`). A fast respawn of a
  confined driver (xhci on the T630) installs its new IOMMU domain and bus mastering, and the old instance's
  late release then clears bus mastering and reverts the DTE to passthrough. The new driver logs "confined"
  and runs unconfined with its DMA stopped. `backlog/fixed/80` K11 fixed the leak half of this interleaving,
  not this half.
- **Interrupt routes:** the reply-mailbox id is freed (`:2615`) before routes by that id are unregistered
  (`:2734-2741`); a concurrent spawn that reuses the id as a driver endpoint loses its interrupts.
- **The delegated band:** a dying minter can allocate a band slot after `release_owner` has run (`:2648`);
  the slot leaks to the next holder of its endpoint id and invocations can reach a task that never minted it.
  This contradicts `audits/kernel-audit.md:536-539` ("closed by construction").
- **The zombie window:** a killed task keeps running user code and syscalls with all its holdings authority
  until the IPI sent after the whole teardown (x86) or its core's next tick (arm32, aarch64).
- **Abandoned kills** are never retried, and their still-valid slot keeps naming endpoint ids already freed
  and reused (KNOWN as a visible leak, `backlog/fixed/48`; the stale attribution is not recorded).
- **Correction (not applied):** release device-keyed state immediately after the Dead claim; free endpoint ids
  last; send the stop IPI first; re-check Dead after the band claim in ResourceMint.
- **Check:** the prototype `kill_order.py` (ARCHITECTURAL CHECK: four device-keyed releases run late at HEAD)
  and `kill_spawn_model.py` (MODEL CHECK: 30 bad interleavings, 0 with release before publish).

#### D15. A failed spawn does not give back everything it took

**COUNTEREXAMPLE, medium** (the endpoint-id leak), low for the rest. Found by 8 readers across 12 findings.
Partly KNOWN: the leaked IRQ lines and partial address space are in `audits/kernel-audit.md` (T1, A5-11 and
`:1099-1117`), where the IRQ case is reasoned harmless; the reply-id leak is not recorded, and `:1077` claims
the rollback leaks nothing toward the id space.

- **Where:** `kernel/src/task/mod.rs:1450-1457` (`cleanup_partial_spawn` kills the reply endpoint and marks it
  dead but never calls `free_endpoint_id`), `:1677-1683` (the routing-full refusal never frees the primary).
- **Consequence:** each spawn that fails after registering its reply mailbox leaks one id for good (nothing
  sweeps dead entries back to the free list). The failure is reachable without privilege: `AllocMem` checks
  only the caller's own budget, budgets can sum past RAM, and the supervisor respawns any watched service
  that dies, so a spawn whose image loads but whose context frame or kernel stack fails is a reachable
  state, and one such event breaks id conservation. The escalation is a PROOF GAP: the id space is
  `[100, 4096)`, so about 3,996 such failures in one boot would reach `endpoint id space exhausted`
  (`ipc/mod.rs:57`), and each of them also leaks its address space (T1), so memory may run out first.
- **The rest of the unwind:** the IRQ route stays on the freed id, and LIFO reuse hands it, with its
  `IrqUnmask` authority, to the next endpoint allocated; a console spawn failing after the framebuffer grant
  strands the display; the device record is discarded with bus mastering still on; the partial address space
  is leaked (T1), and the supervisor's retry repeats it.
- **Correction (not applied):** one line for the id (`free_endpoint_id(rep)` after `mark_dead_resource`);
  release routes, the framebuffer and the device record in the same unwind.
- **Check:** an ARCHITECTURAL CHECK pairing every `kill_endpoint` and `alloc_endpoint_id` with a free on each
  exit (`release_pairs.py` prototype); a QEMU stress forcing post-registration spawn failures and asserting
  the id counter stays bounded.

#### D16. A plain lock held by the preemptible supervisor respawn

**COUNTEREXAMPLE, medium.** Upheld 3/3. Model-checked (`irq-smp/models/mc.py`, model B, three steps).

- **Where:** `kernel/src/ipc/mod.rs:47` and `:74` (`FREE_IDS.lock()`, not `lock_irq`); the supervisor respawn
  runs at IF=1 in core 0's scheduler loop (`task/mod.rs:2612-2650`).
- **Trace:** the supervisor dies; core 0's loop runs the respawn and takes `FREE_IDS`; the timer fires inside
  the few-instruction hold and core 0 switches to a task; that task faults or is killed on core 0 at IF=0
  and its kill path spins on `FREE_IDS`, whose holder can only resume on core 0. The spinlock watchdog then
  panics, or another core's liveness watchdog does.
- **Contradicts:** `scheduler.rs:1888-1890` and `task/mod.rs:2594` ("the spawn's locks are IRQ-safe"); the
  audit refutation of the analogous BAND and IRQ_TABLE concern assumed every acquirer runs at IF=0, which
  the respawn does not.
- **Correction (not applied):** `lock_irq` at both sites, and re-examine `BAND`, `SEEN` and `FB` with the
  respawn path in mind.
- **Check:** `irq_lock_check.py` rule R1 (ARCHITECTURAL CHECK, flags both lines today).

#### D17. The core set is assumed dense

**COUNTEREXAMPLE, medium.** Found by 2 readers plus 2 boot findings; upheld 3/3.

- **Where:** `kernel/src/task/mod.rs:1308-1309`, `:1318-1319` (round-robin placement returns
  `RR % ready_count()` with no ready check), `kernel/src/task/scheduler.rs:1799` (the liveness watchdog loops
  `0..ready_count()`).
- **Trace:** a 4-CPU x86 machine whose AP 2 fails to come up and AP 3 succeeds (CLAUDE.md 11.3 says continue
  with the available cores): `ready_count()` is 3, a service with no placement is placed on core 2, which
  runs no scheduler, so it never runs and nothing says so; and no core ever checks core 3's liveness.
- **aarch64:** per-core arenas are sized by the secondaries that check in within 1 s, but every secondary that
  arrives later is still released by hardware id (`arch/aarch64/smp_boot.rs:151-157`,
  `arch/aarch64/mod.rs:859-869`); secondaries are also released before the GIC is initialised and before the
  boot core's tick selftest.
- **Correction (not applied):** iterate ready core ids, never `0..ready_count()`; size arenas by the hardware
  maximum or refuse late arrivals.
- **Check:** an ARCHITECTURAL CHECK forbidding `ready_count()` as a core-id domain; a QEMU chaos C1 variant
  skipping a middle AP.

#### D18. A level interrupt can be masked forever, silently

**COUNTEREXAMPLE, medium.** Upheld 3/3. Model-checked (`irq_mask_model.py`).

- **Where:** `kernel/src/interrupt/route.rs:142` (deliver masks), `:167-170` (then skips the notification when
  the owner's queue holds 8 or more messages), `:179-180`.
- **Trace (Pi 2):** `chaos flood-storm dwc2 1` fills dwc2's queue with 16 one-byte messages; the controller
  raises its level interrupt; deliver masks the line, sees depth 16, sends nothing and prints nothing. dwc2
  drains the flood; none of it is an interrupt event, so it never re-arms. The line stays disabled until dwc2
  is restarted; the network falls back to polling and flood-storm reports "drained, alive". The
  driver-skeleton template, which re-arms only on an interrupt, is never woken by its device again. The code
  comment anticipates a LOST notification, not a permanently masked line.
- **Also:** nothing can report a masked, silent line (the only per-vector evidence is a once-per-boot
  one-shot); x86 IOAPIC index and data accesses are unlocked across cores; interrupt delivery is not tied to
  the endpoint instance.
- **Correction (not applied):** a per-vector "notification pending" bit: deliver enqueues only when it is
  clear and sets it; `IrqUnmask` and register clear it; skip only when it is already set. Count and log every
  remaining drop.
- **Check:** `irq_mask_model.py --self-test` (MODEL CHECK); a property test over the real `route.rs` with a
  recording fake controller.

#### D20. Kernel stack overflow is not loud on every port

**COUNTEREXAMPLE, medium.** Upheld 2/3. Partly KNOWN (riscv64's missing guard is noted parenthetically in
`audits/unsafe-audit.md:3710` and `arch/arm/page_tables.rs:813`).

- **riscv64:** the guard pages are absent. `install_kstack_guards` is never called on that boot path and
  `unmap_4k_strided` is an empty function (`kernel/src/arch/riscv64/mod.rs:2355`), so all 224 kernel stacks
  are contiguous and an overflow silently writes the neighbour's trap frame. The riscv64 DEBUG dev loop
  already exceeds its 64 KiB stack on every Call: 66,592 bytes for the send half by the finder's count; one
  skeptic measured the reply-dequeue chain at about 75 KiB, past the unguarded page into the neighbouring
  slot, and another found a worst case of about 69 KiB inside the stride, so the exact depth is disputed.
  Adding the guard alone would make the debug kernel fault on its first Call, and the trap entry would
  re-fault walking into the neighbour.
- **x86:** an overflow into the guard is a silent triple-fault reset (no IST stacks), not a loud fault.
- **Debug profiles** exceed the 64 KiB kernel stack on every Call and CallDeadline on x86, aarch64 and
  riscv64; arm32 fits by 144 bytes. Release builds measure about 21.5 KiB per syscall and at most about 44 KiB
  with a nested trap (ARGUMENT SUPPORTED: no overflow reachable in release).
- **Stale:** `scheduler.rs:1494-1497` claims the deepest syscall is about 4.6 KiB.
- **Correction (not applied):** split riscv64's large mappings and install the guards with an emergency trap
  stack; give x86 a double-fault IST stack; size or forbid debug kernels.
- **Check:** a guard-installed ARCHITECTURAL CHECK (fails today on riscv64); a QEMU kernel-stack overflow
  test per port (none exists on any port).

#### D21. Kernel W^X and the DMA posture report exist only on x86

**COUNTEREXAMPLE, medium.** Upheld 3/3. Partly KNOWN: A10-9 (aarch64 W^X) and SEC-34 (aarch64 DMA report).

- **Where:** the hardening seam (`harden_hhdm_nx`, `audit_wx`) is an empty function on arm, aarch64 and
  riscv64 and is never called. arm maps every RAM section RW at PL1 with XN clear
  (`kernel/src/arch/arm/mmu.rs:85-102`); riscv64 maps one RWX gigapage identity map
  (`kernel/src/arch/riscv64/mod.rs:3691-3700`); aarch64 high-half RAM has no PXN. On those three ports kernel
  text is writable at kernel privilege and every RAM frame is executable at kernel privilege, and no
  `wx-audit` line is printed.
- **DMA:** CLAUDE.md 6.4 requires the confinement case to be "reported loudly at boot"; only x86 prints it.
- **Spec drift:** `GLOSSARY.md:104` defines W^X with no per-arch qualifier. (`docs/ENGINEERING_ASSURANCE.md:115`
  means the loader's user-segment W^X, which holds on every port; the skeptics rejected that sub-claim.)
- **Correction (not applied):** per port, map text RX and data and the direct map RW and non-executable, give
  `audit_wx` a body, print the posture; until then record the residual per arch.
- **Check:** `boot_parity.py` (ARCHITECTURAL CHECK, reports the stubs today); a per-arch boot assertion.

#### D24. Cross-core reads of unsynchronised per-task state, one of them panicking

**PROOF GAP, medium** (a Rust data race, so undefined behaviour regardless of outcome). Upheld 2/2.

- **Where:** `handle_kill` runs `assert_cap_table_consistent` (`kernel/src/invariants/assertions.rs:34-46`,
  called at `syscall/dispatch.rs:1219-1228`) over `for_each_active_cap`, which reads every task's `TASK_CAP`
  with no lock while their owners insert and remove caps on other cores (`scheduler.rs:1204-1225`). A torn
  read would report a "future" generation and panic the kernel from a userspace kill.
- **The class:** `TASK_WAKEUP_ERR` and `TASK_CORE` are `static mut` written and read by different cores;
  `TASK_NAMES.set()` aliases the whole table with `&mut` while other cores read it (`smp/names.rs`);
  `TASK_VALID` is published before the cap table and commit fields, yet two SAFETY texts treat an Acquire
  read of it as publishing them; three cap-table accessors carry no SAFETY comment (section 10).
- **Correction (not applied):** make the snapshot check log loudly instead of panicking, or take a per-table
  seqlock; make the cross-core statics atomic.
- **Check:** a loom or shuttle test of insert against the walk on a host seam (IMPLEMENTATION VERIFICATION).

#### D26. `fs` leaks delegated band slots on its in-request remount

**COUNTEREXAMPLE, medium** (userspace, outside the core scope, recorded because a kernel table with no
per-owner quota is what it leaks into). Upheld 3/3. Partly KNOWN: `audits/userspace-audit.md:67` A10-5 names
this site; fix `b5b4f6c9` added revokes at three other sites but not here, and A10-5's `OP_FLASH` site
(`format_into`, `:1709`) leaks the same way.

- **Where:** `services/fs/src/main.rs:1459-1460` (a read-only request that hits an I/O error re-mounts with a
  fresh state whose open-file table is empty, without revoking the old one; the top-of-loop path at `:871`
  does revoke).
- **Consequence:** each such event leaves up to 64 band slots unrevoked until their holders close them (a
  holder's close still revokes, because `FOP_CLOSE` revokes before it looks the id up); exhausting the
  shared band needs many such events with long-lived holders, which is plausible, not demonstrated. The band
  is 2,048 slots shared with net-stack and resource-server, has no per-owner quota, and its exhaustion is not
  logged by the kernel, collapses into the same error as three other failures, and is labelled
  PermissionDenied by `gs`. A client's still-valid file cap gets "no such file" for a file that exists.
- **Correction (not applied):** revoke the open files before that remount (one line in `fs`, no kernel
  change); log band exhaustion distinctly in the kernel.
- **Check:** `fs_remount_revoke_check.py` (ARCHITECTURAL CHECK; flags `:1460`, and two other sites that need
  explicit markers before it can gate).

### 6.3 Low

- **D13. The power-off path from the tick ISR takes plain locks.** COUNTEREXAMPLE, low (arm32 only, narrow).
  `power_off_now` reclaims the framebuffer with `FB.lock()` (`kernel/src/bootcon/mod.rs:302-311`) while the
  interrupted idle path on the same core can hold FB with interrupts on (arm32 does not mask idle): core 0
  self-deadlocks until the spin watchdog. The power path's waits are iteration counts that run before core
  0's liveness stamp. "No holder can postpone what another asked for" is not enforced (cancel then re-arm
  postpones; ARGUMENT SUPPORTED, info, moot while the shell is the only holder); the protocol itself
  model-checks clean. Correction: `try_lock` on the panic and power paths,
  `lock_irq` everywhere else for FB.
- **D22. SpawnImage mints CONSOLE_READ and grantable peer caps from request flags the supervisor does not
  hold.** COUNTEREXAMPLE, low (supervisor-only, so not an escalation by untrusted code), upheld 3/3. It
  contradicts CLAUDE.md 14.1 step 4 ("a spawner passes on what it has and never mints") and
  ENGINEERING_ASSURANCE 4.1 (`dispatch.rs:1045-1052`, `:1150-1160`, `task/mod.rs:1791-1792`). Spawn installs
  copy a delegated cap with GRANT intact, bypassing the SEC-7 narrowing (ARGUMENT SUPPORTED, info, not sent
  to skeptics; it needs the supervisor to hold a delegated GRANT cap, which was not observed). Correction: make CONSOLE_READ a
  delegatable privilege checked like the others, or amend 14.1 to list the kernel-minted-on-request set.
- **D23. Two ungated InspectKernel queries misbehave under concurrency or by design (x86).** COUNTEREXAMPLE,
  low. Query 21 was found by three readers, upheld 3/3 each; the CMOS race was upheld 1/1 and corrected by
  its skeptic from a proof gap to a counterexample.
  - **Query 21 pops the COM2 operator stream.** `dispatch.rs:1900` lists 21 as ungated and `:1993` calls
    `com2_try_read_byte`, which reads the UART FIFO destructively (`arch/x86_64/mod.rs:897-909`). Any task
    with no capability can read and delete operator commands meant for `control`; it cannot inject. The
    comment at `dispatch.rs:1886-1893` puts query 21 in the ungated set on purpose, as "one byte off the
    COM2 operator channel", among facts "that disclose nobody's state"; a destructive read of the
    operator's input is neither. SEC-12 records the channel as reachable only physically, which C1-6
    changed. Correction: gate query 21 to `control`.
  - **Queries 11 and 17 race on the CMOS index port.** `cmos_read` is a bare `out 0x70` then `in 0x71`
    with no lock (`arch/x86_64/rtc.rs:41-55`); query 17 has 164 call sites, including every
    `request_with_reply_deadline` wait, and the kernel calls it at spawn, so cores collide in ordinary use.
    The trace closes into a concrete wrong answer: status register B is read once (`rtc.rs:185`), and if
    another core has just selected register 0x0A or 0x09 the read returns 0x26, which sets the binary and
    24-hour bits; every BCD field is then misread (year 0x26 as 38, day 0x16 as 22, October packed as month
    0), the century defaults to 20, and query 11 returns a datetime with the year field 12 ahead (2038) and
    month 0, which `year_plausible` accepts: about 11.2 years, roughly 4,085 days, ahead on this trace. That
    is a year jump of the same +12 kind as the T630 "4383d" CMOS misread cited at `dispatch.rs:1943`, which
    was masked and never root-caused. This is a CANDIDATE cause, not a demonstrated one: nothing here reproduced it on the
    T630. PCI configuration space got a lock for exactly this index/data shape (`pci.rs:40-46`); CMOS did not.
    Correction: a lock around the CMOS pair, as PCI has.
  - Three other ungated queries write shared state on some ports (q13 on arm and aarch64, known A8-19; q17's
    `LAST_NOW_EPOCH`; q19's RNG restart on aarch64). The skeptics refuted that as a violation 0/3: the
    in-code rule at `dispatch.rs:1886-1893` is about DISCLOSURE ("disclose nobody's state"), not about
    effects, and no stated invariant says ungated queries are read-only.
- **D25. A cap minted on a dead record validates at the capability layer.** COUNTEREXAMPLE, low. Found by the
  capability harness on the real code (`probe d1-mint-after-death`). `mint_cap` reads only the generation and
  `get` consults liveness only on a mismatch (`capability/table.rs:59-70`, `:261-265`), so AcquireSendCap in
  the window between the death bump and the name unregister mints a cap that passes `get`. Every endpoint use
  still ends in the routing liveness check, so no authority was found to leak, but DeriveCap, SendWithCap's
  carried-cap check and spawn installs accept it. No-resurrection through id reuse holds today only because
  spawn always draws the reply endpoint's generation right after the primary's; a dead record's generation
  can equal the next global draw. After an id is reused, a stale cap's error flips from EndpointDead to
  CapRevoked (reproduced by the capability harness and upheld 3/3 as behaviour, at info; the specification
  skeptic on the tests reader's duplicate showed that CLAUDE.md 14.3, Test 6B and F7 all allow either code, so it is not a violation, only a gap between 14.2 step 2's wording and what a
  client sees after the respawn).
- **D27. Results that do not reflect the outcome.** COUNTEREXAMPLE, low. SpawnImage returns an error after
  the child is already running when the spawner's cap table is full (`dispatch.rs:1144-1179`); AllocMem
  charges the whole budget before mapping and leaks the data frame when mapping fails (A10-15, KNOWN, still
  present); several syscalls report success on ports where they are empty stubs; TaskCaps for a dead or
  out-of-range slot returns -1 on x86, aarch64 and riscv64 but 0 on arm32; CallDeadline reply-buffer class 0
  means both 4096 bytes and 1 byte; names of 33 to 64 bytes are accepted by SpawnImage and silently not
  registered; receives consume the message and install its caps before refusing it (not recorded: K5 made
  the refusal loud but did not address the ordering, and the SDK's error arm then removes the just-installed
  cap, contradicting `backlog/67`'s premise); `Rights::REVOKE` exists and is checked nowhere
  (ARGUMENT SUPPORTED, info). ResourceInvoke accepting a caller-chosen right of 0 was REFUTED as a defect
  (section 8): it is the designed path for closing a no-right file, and refusing it would be a regression.
- **D28. Diagnostics under hot locks.** COUNTEREXAMPLE, low. Serial lines are printed while holding the
  routing table, the IRQ table and the global capability table (`routing.rs:776-779`, `route.rs:50-51`,
  `table.rs:48-53`), against the rule `routing.rs:285-287` states; the panic path takes the ordinary FB and
  RING locks, so a panic on a core holding either is never reported; an unknown LAPIC id maps to core 0
  silently; interrupt-mask `asm!` is marked `nomem`, so the compiler is not forbidden to move memory accesses
  across it; on ARMv7 the Ready store is ordered before the doorbell only by an unrelated barrier.
- **D29. The ELF loader never bounds `p_vaddr`.** PROOF GAP, low (reachable only through the supervisor).
  KNOWN as A5-10 (STAGED); this pass adds that on x86 the effect is not confined to the task's own address
  space, because the kernel half of every page table is shared by pointer (`loader.rs:262`, `:288`, `:332`).
  Correction: refuse a segment above an arch-provided user ceiling, as arm32 already does.
- **X1. The generation counter's overflow panic fires once.** ARGUMENT SUPPORTED, info: after about 2^31
  spawns `next_generation` panics (KNOWN, `audits/kernel-audit.md:865`), and if execution continued the next
  call would return 1 (not recorded; unreachable in practice).

---

## 7. What holds, and what cannot be established

### 7.1 Arguments supported under stated assumptions

These were argued from the code, not machine-checked; where a prototype also exercised them, the level is
named.

- **Capability table:** validation agreement with the specification on every reachable path apart from the
  recorded error-code divergence (PROPERTY TEST, 8 seeds x 40,000 operations on the real code, 121 million
  comparisons in seed 1 alone); non-escalation of rights on every live install path
  (`narrow_for_grant` is dead code; `without_grant` is the deployed attenuation); revocation completeness for
  caps minted while a resource was alive; delegated-band ownership single-threaded; slot integrity;
  generation exhaustion is a loud panic; stable gate ids are never bumped by any current caller.
- **RECV never leaves its endpoint's owner;** SEND and RECV caps target only endpoint ids; GRANT is required
  for every transfer; the badge on a received message was produced by a validated ResourceInvoke.
- **The name directory** never maps a name to a reused id while its owner lives, and only the spawn path
  registers names; AcquireSendCap is gated (GRANT only with ACQUIRE_ANY).
- **IPC queue bound and FIFO;** generation discipline and no cross-generation delivery inside `routing.rs`
  (PROPERTY TEST on the real code; inside `routing.rs` the generation bump on kill is unobservable, because
  liveness already rejects every stale use).
- **Scheduling:** Dead is absorbing within a life (MODEL CHECK, never violated); the Dekker claim handshake
  under sequential consistency, with the re-check shown load-bearing; run-queue membership; fairness under
  the stated timer assumptions.
- **Weak memory:** no outcome possible only under weak ordering was found in the kill and pick handshake,
  the claim order, commit publication, the SpinLock handoff, or the `CALL_AWAIT_EP` clear (litmus
  approximations with an ARMv8-derived model; ARMv7 three-thread shapes and RISC-V byte CAS over a shared
  word NOT modelled). This is an argument, not a proof.
- **Memory:** no phantom free; a double free does not inflate the free count; W^X on loaded user segments;
  every copy is range- and overflow-checked first; the allocator never returns a live, kernel or reserved
  frame.
- **Gating:** every one of the 51 dispatcher arms is gated by a recognised returning gate or is confined to
  the caller's own state (ARCHITECTURAL CHECK, prototype C4); no state-changing call precedes a gate; every
  error path returns a negative code except six recorded exceptions; PowerOff ops 0 to 2 need POWER_OFF and
  3 to 4 need REBOOT, and no op falls through.
- **The power deadline protocol** (earliest wins within a kind, the other kind refused, cancel and fire
  mutually exclusive) holds under model check. Its CLOCK is D12.
- **Release kernels** fit the 64 KiB kernel stack with margin on every port examined.
- **Device records:** `TASK_HW_BDF` is released at most once per slot life.
- **Fault kills:** `kill_current` from a CPL0 user-copy fault runs the normal teardown soundly, apart from the
  self-kill race (D10).
- **riscv64 interrupts:** the permanently masked line of D18 cannot be reached there (no `route::deliver`
  call site exists, and the mask seam is a no-op).
- **Services are restartable** and the supervisor is respawned, as observed on x86 QEMU and on hardware by
  hand.

### 7.2 Proof gaps

- **Concurrency of IPC wake and block.** No lost wakeup holds in 33,000 sequential cases; the real hazard is
  the interleaving of the table lock, the wake CAS, the pending-wake flag and the block CAS on two cores,
  which no model here explores. A TLA+ model is proposed (section 16).
- **Weak memory in general.** Every scheduler model is sequentially consistent. Three of the four shipping
  ports are weakly ordered.
- **No resurrection through identifier reuse** rests on an unstated coupling (D25): change the spawn order and
  it fails silently. `register_resource_at_gen` enforces nothing.
- **Delegated allocate against owner death** (D14): three separate lock holds.
- **Cross-core cap-table reads** (D24): a data race regardless of outcome.
- **One holder per device window** (invariant 2): a comment, not code; reachable only through the supervisor.
- **The kernel remains tiny** (invariant 4): the pins can be evaded by spelling (E2).
- **Contracts enforced** (invariant 7) and **state owned** (invariants 8, 9): the reconciling maps and the
  kernel's mutable statics are unratcheted.
- **The lock order** over arch locks (serial, PCI configuration, IOMMU, mailbox) was not inventoried.
- **The panic path** is not re-entrant against its own locks; **the x86 IOAPIC** register pair is unlocked;
  **interrupt delivery** is not bound to an endpoint instance; **interrupt-mask `asm!`** is `nomem`.
- **The user-copy slice API** (`read_user_bytes` returning `&'static`) is unsound as a signature.
- **Guard installation** is verified for slot 0 only, and x86's unmapper silently skips 2 MiB pages.
- **Unknown syscall numbers** return -1, the same code as many handler failures, and the F2 fuzz never
  exercises 16 to 56 or the removed numbers.

### 7.3 Not examined

Collected from every reader's own list:

- Per-port syscall entry interrupt state for aarch64 and riscv64 in depth.
- `arch/*/uaccess.rs` fault fixups on arm, aarch64 and riscv64 beyond what the findings cite.
- The loader's `ImageSource::User` double-fetch discipline.
- `names.rs` eviction under a full table.
- Mailbox credit accounting.
- The supervisor's reconcile timing.
- Hardware page-walker speculation (the two CLAUDE.md 10.5 residuals).
- x2APIC ids above 255.
- The riscv32, loongarch64 and s390x stubs.
- Every QEMU suite, and every board.

---

## 8. Findings the skeptics killed

A finding is killed unless a strict majority of its skeptics upheld it, so a 1/2 tie kills. These are recorded so they are not raised
again, and because several of the refutations carry facts worth keeping. The vote is upheld over total.

| Finding (area) | Vote | Why it was killed |
|---|---|---|
| A child spawned under the name "supervisor" inherits the supervisor's GRANT-only set (cap-core) | 0/3 | the trace breaks at its first step: `spawn_from_image` refuses every kernel-catalogue name, `supervisor` included (`task/mod.rs:1379-1382`); in any case only the supervisor holds `IMAGE_SPAWN` |
| A Call reply parked in `pending_send` is invisible to `call_dequeue` (ipc) | 0/3 | the code fact holds; neither consequence is a kernel violation reachable by in-tree or hostile code |
| Sender-side lost-wakeup safety examined only for x86 and arm32 (ipc) | 0/1 | closed by reading: every sender registration on all four ports is followed directly by its block, with nothing preemptible between |
| `kill_by_name` can kill the supervisor's fresh respawn (sched) | 0/3 | the defect underneath is real; the trace as written does not work and its timing is not shown reachable |
| The AllocMem heap base equals the PCI MMIO window base (mem) | 0/1 | the overlap is real; untrusted code cannot pair a device window with code of its choosing (needs `IMAGE_SPAWN`) |
| x86 IOAPIC index/data pair unlocked (smp-irq) | 1/2 | the interleaving is real; neither harmful outcome follows from the trace |
| Per-device MSI vector record is check-then-act (smp-irq) | 0/1 | one caller, inside SpawnImage, which needs `IMAGE_SPAWN`; other arches never store a vector |
| Refusal logging lets a capless task flood serial (syscall) | 0/3 | no production task lacks a capability: every spawn puts `log_write` in slot 0, so it confers nothing new |
| ConsoleForeground lets any holder steal or release a claim (syscall) | 1/3 | the mechanism is described correctly; the stated attack trace is not reachable |
| A MessageTooLarge refusal consumes the message first (syscall) | 1/3 | already recorded (K5); the proposed correction would be harmful |
| ResourceInvoke with right 0 lets any holder revoke for everyone (syscall) | 0/1 | there is no second holder: `fs` mints a fresh id on every open |
| Ungated query 19 lets any task drain the RNG with a long spin (syscall) | 0/1 | holds as stated on aarch64, but breaks no stated invariant |
| `cargo mutants` can never mutate `table.rs` or `delegated.rs` (tests) | 0/2 | wrong: cargo-mutants walks `main.rs` as well as `lib.rs`, and the binary's module tree has no `cfg(test)` gate |
| The SEC-11 guard was never armed (tests) | 1/2 | it fired in host proptests once (`74ee6ff3`) and is armed in default debug QEMU kernels on three ports; the property it guards holds (E6) |
| Four more ungated queries change shared state (syscall gate) | 0/3 | the writes are real; the in-code rule is about disclosure, not effects, and no invariant says ungated queries are read-only |
| ResourceInvoke accepts a caller-chosen right of 0 (syscall gate) | 0/3 | the designed path: `fs` mints GRANT-only file caps and the stdlib closes them with right 0; refusing 0 would leak `fs`'s open-file table, a regression |
| A failed gate logs one serial line per call, unbounded (syscall gate) | 0/1 | reachable, but the claimed violation does not follow |
| ConsoleForeground(0) releases another task's claim (syscall gate) | 0/1 | the rule it names is broken on purpose in three other places; not one the kernel states |
| Nothing enforces one holder per device (device-grant follow-up) | 0/3 | every step after the first needs `IMAGE_SPAWN`; the finding admits as much |
| aarch64 overflow walks the vector frame into the neighbour (kstack follow-up) | 1/3 | sound under ARMv8, but no image the project builds runs the overflowing path (`stack_fit_check` runs first) |
| armv7 and riscv64 emit no stack probes (kstack follow-up) | 0/1 | true, but no arm32 release frame exceeds the guard; on riscv64 there is no guard to step over (D20) |
| riscv64 continues after a BSP init failure (boot follow-up) | 0/3 | neither branch is reachable from the syscall surface; one is dead code and the other ends in the panic 11.3 requires |
| The idle-tick configuration lines exist only on x86 (boot follow-up) | 0/3 | there is no slowed idle tick off x86 to report: `rearm_idle_timer` is empty on arm and aarch64 |
| Copy failure after dequeue differs by arch (user-copy follow-up) | 1/3 | the code does what it says; the defect part is already recorded (K5) and the new parts break no invariant |
| A fault in the loader's user-image copy leaks a half-built address space (user-copy follow-up) | 0/1 | the mechanism is right; no untrusted principal, restartable service or race can reach it |
| `unregister_endpoint` masks outside the table lock (irq follow-up) | 0/1 | the window cannot be preempted: interrupts are masked through it on every arch where masking does anything |
| Pi 4 GENET's line stays masked after the first interrupt (irq follow-up) | 0/1 | accurate and by design; no kernel invariant is broken and the trigger is not observed |
| x86 tick runs at 1-10 Hz when PIT calibration fails (power follow-up) | 1/3 | the arithmetic holds; calibration failure is not shown reachable |

**The re-run.** 13 findings lost some or all of their skeptic votes when the session limit was reached during
the first pass (27 checks). All 27 were re-run with the same prompts and lenses: 10 of the 13 findings
survived and 3 were killed (the SEC-11 "never armed" claim, the ungated-query effects, and ResourceInvoke's
right 0), all three listed above.

Three more were down-graded rather than killed, and their entries above say so: the stale-cap error code
(D25, allowed by 14.3 and Test 6B), the reply-id leak's escalation to a panic (D15, a PROOF GAP), and the
mutation survivors (E5, mostly killed in QEMU).

---

## 9. Architecture-by-property matrix

Each cell says what was found for THAT ISA; a blank result is never inferred from another column. "Not
examined" means not examined. Columns: x86_64 (HP T630, Dell Wyse), arm (ARMv7, Pi 2), aarch64 (Pi 4),
riscv64 (VisionFive 2 Lite, QEMU virt). The riscv32, loongarch64 and s390x stubs were not examined.

| Property | x86_64 | arm | aarch64 | riscv64 |
|---|---|---|---|---|
| Kernel W^X (kernel map) | holds; `wx-audit` printed | **broken**: RAM RW at PL1, XN clear (D21) | **broken**: no PXN on high-half RAM (known A10-9) | **broken**: the whole identity map (gigapages up to the top of RAM, MMIO included) is RWX (D21) |
| DMA confinement | AMD-Vi for xhci and audio only | none (stub) | none (stub) | none (stub) |
| DMA posture reported at boot (6.4) | yes | **no** (D21) | **no** (known SEC-34) | **no** (D21) |
| Kernel stack guard pages | installed | installed | installed | **absent** (D20) |
| Overflow into the guard is loud | **no**: triple-fault reset, no IST (D20) | not established (no stack probes; refuted as reachable in release) | reported, after the vector frame walks out of the guard (refuted as reachable, section 8) | n/a, no guard |
| Debug kernel fits the 64 KiB stack | **no** (D20) | yes, by 144 bytes | **no** | **no** (66,592 B on every Call) |
| User-copy fault guard | **per-core race** (known A10-2) and can stick true (D19) | pre-walk, single-hart TOCTOU recorded; returns a borrow of raw user memory | **per-core race** (known A10-2) | `SUM` carried across a preempted copy (D19) |
| Copy failure after dequeue | receiver killed, message gone | -1 returned, message gone | -1, message gone | -1, message gone (KNOWN class, K5) |
| Tick clock | **inflated** by every wake IPI to core 0 (D12) | **inflated** by hi-res wakes; ticks inside a running syscall **dropped** (D12) | SGIs do not inflate (checked) | **stalls**: core 0's tick re-armed at every idle halt and switch-in (D12) |
| Masked stretches drop ticks with no catch-up | yes | yes | yes | yes |
| Interrupt delivery and masking | IOAPIC indirect access unlocked (PROOF GAP); depth-skip mask (D18) | depth-skip mask reproduced in model for dwc2 (D18) | GENET masked after first interrupt by design (refuted as a defect) | masked state unreachable (no `deliver` call site; mask seam no-op) |
| Syscall numbers intercepted before dispatch | none found | `0x5555_0001` | **`>= 0x1000` ungated** (known A10-10) | `0x5555_0003` |
| Ungated query 21 (COM2) | **pops the operator stream** (D23) | returns None | returns None | returns None |
| Ungated queries 11 and 17 (CMOS) | **unlocked index/data pair**: a concurrent read can return a date 12 years ahead (D23) | not examined | not examined | not examined |
| Ungated query side effects (judged not a violation, section 8) | q17 stores `LAST_NOW_EPOCH` | q13 releases a lapsed foreground claim (known A8-19) | q13 (known A8-19); q19 RNG restart writes | q19 writes the TRNG's command register (GENERATE, reseed on lockup) |
| TaskCaps for a dead or out-of-range slot | -1 | **0** | -1 | -1 |
| Boot path | `kernel_main` (W^X, observability lines, the cores-ready line) | own entry; `ipc::init` never called (harmless) | `ipc::init` never called (harmless); arenas sized by arrivals within 1 s, late secondaries still released, APs before GIC (D17) | continues after a BSP init failure (refuted as reachable) |
| Power-deadline path FB lock from the tick ISR | masked contexts only | **self-deadlock possible** (D13) | masked (mirror wrapped in `without_interrupts`) | masked |
| Weak-memory handshake | x86-TSO; SC model applies to SeqCst accesses | ARMv7: doorbell ordered by an incidental barrier (PROOF GAP) | litmus approximation: no weak-only outcome found | byte CAS as `lr.w`/`sc.w` over a shared word: not modelled |
| Section 22 QEMU suites | all of them | **none** | **none** | **none** |

---

## 10. `unsafe` obligations

**Placement holds.** Measured on the real tree: no `unsafe` line outside `arch/`, `memory/`, `capability/`,
`smp/` and the grandfathered floors of CLAUDE.md 18.5. `scripts/unsafe_check.py` does not enforce placement
(a new inventory row anywhere passes) or SAFETY presence (its docstring says that is review-only).

**Obligations that do not hold, or are not argued:**

1. **18.3 SAFETY comments.** 16 kernel `unsafe {` blocks have no SAFETY comment in the 15 lines above them;
   three are core cap-table accessors: `task/scheduler.rs:1174` (`current_task_remove_cap`), `:1189`
   (`current_task_read_cap_rights`), `:1450` (`current_task_insert_cap`). The fourth neutral one is
   `task/scheduler.rs:2471` in `drain_pending_kstack` (its SAFETY comment follows it rather than preceding
   it); the other 12 are in arch code.
2. **`for_each_cap_of` and `for_each_active_cap`** read other tasks' `TASK_CAP` (a `static mut` array) while
   the owners mutate them on other cores. The audited argument (`audits/unsafe-audit.md:3389-3396`) covers
   initialisation order only, and is wrong even there: `TASK_VALID` is published (`scheduler.rs:990`)
   before `task_cap_init_empty` runs (`task/mod.rs:1577`). The concurrent read is a data race (D24).
3. **`TASK_NAMES.set()`** creates a `&mut` to the whole table while other cores read slots of it
   (`smp/names.rs:107-133`); `audits/unsafe-audit.md:3497` records the argument as sound, and it is not.
4. **`TASK_WAKEUP_ERR` and `TASK_CORE`** are `static mut`, written and read by different cores without
   atomics (`scheduler.rs:143`, `:2343`, `:3254`).
5. **`drain_pending_kstack`'s** SAFETY argument ("RSP is not on this kstack") is true, but the obligation the
   free actually needs, exactly once and only while the dead task owns it, does not hold (D8). The overflow
   branch frees the stack it is running on (D9).
6. **`read_user_bytes`** returns `&'static [u8]` into a per-core buffer the next copy overwrites: an unsound
   signature kept safe only by every caller copying before it blocks (D19).
7. **`switch_context(cur, next)`** with `cur == next` is reachable from `block_and_reschedule` (D11); the
   aliasing case was read only for x86 and arm32.
8. **The free paths** (`free_kstack`, arm `free_l1`) accept a double free silently; x86's frame allocator
   absorbs a double free only while the frame is still free.
9. **109 `unsafe fn`, `impl` and `extern` sites** have no nearby `# Safety` section (counted, not reviewed).

---

## 11. The enforcement layer

`scripts/commandments.py` passes all 21 checks and 78 of 78 selftest probes on the real tree, and
`commandments_redteam.py` catches 11 checks. That is the evidence it offers. Measured against injected
violations:

- **E1. `I-irq-lock` never reads the files it was written for.** COUNTEREXAMPLE, medium, upheld 3/3.
  `files_in_scope` walks each `dirs` entry with `os.walk`, which yields nothing for a FILE path, so the three
  arch ISR files it declares (`arch/arm/irq.rs`, `arch/aarch64/exceptions.rs`, `arch/x86_64/interrupts.rs`)
  are never scanned; `--report` prints `scanned: 2 files`. Its docstring cites a deadlock in `arch/arm/irq.rs`
  as the reason it exists. The selftest cannot see this because source probes test only the regex.
  Correction: accept file paths, add `riscv64/trap.rs`, assert every scope entry contributes a file.
- **E2. Pins fail open on spelling, and authority routes are unseen.** COUNTEREXAMPLE, low to medium, upheld
  throughout. The table in section 4.1 lists each MISSED mutation. VII does not see request flags (CONSOLE_READ
  for chaos, observe-live and shell is unpinned on the real tree), the probe table, literal privilege words
  or the delegatable set. No check pins which capability gates which syscall: 15 gate mutations, 0 caught.
  IX passes `nic-driver` on a comment. VI misses four real statics in `fs`. V-managed-watched accepts any
  predicate that mentions `MANAGED.contains(`. V-no-panic misses the assert family and the SDK and stdlib.
  The tables that DEFINE what is checked (`kernel_support_roles`, `arch_permitted_roles`,
  `contract_privileges`, `reacquire_api`) have no ratchet. 10 of 21 checks have no real-tree red-team
  injection, and the red team gates nothing.
- **E3. Host tests, Miri and mutation run on no automatic path.** COUNTEREXAMPLE, medium, upheld 3/3. The
  `osdev` gate runs Python checkers only (`osdev/src/main.rs:482-520`, `:621-650`); every test workflow is
  `workflow_dispatch` and paused. `build.yml:148` cites a "regular cargo test step" that does not exist.
  `mutation.yml:34` ends `|| true`. `mutants.out/` is dated 2026-05-16. `kernel/mutants.toml` excludes
  `table.rs` and lists two files that do not exist.
- **E4. The host models are not the implementation.** PROOF GAP, medium, upheld 2/2. `routing_model.rs` and
  `names_model.rs` share no code with `routing.rs` and `names.rs` and omit exactly the waiter machinery where
  D1 to D3 live; their stated reason to exist (the real files cannot host-compile) is false: `routing.rs`,
  `endpoint.rs` and `names.rs` compile on the host byte-identical with one constant (`MAX_TASKS`) stubbed
  (the tests reader compiled all three; the IPC prototype compiled `routing.rs` and `endpoint.rs`). The host P8 test would pass on a kernel that
  resurrects stale caps through id reuse. `memory/bitmap.rs` shares no algorithm with `allocator.rs`.
- **E5. Tests credited for properties they do not test.** PROOF GAP, low to medium. P1, P3, P9 and P10 test
  less than their section 22 rows (section 4.3). Seven host tests exercise `Capability::validate` and
  `narrow_for_grant`, which the kernel never calls; the live gate `CapTable::get` has no host test. No
  host test calls `Rights::without` (the SEC-7 strip), `without_grant`, `dequeue_matching` (Call
  correlation), `next_generation` or `len`, so every viable mutant of them survives the host tests, and all
  four May survivors persist (upheld 3/3, corrected to medium). Most of those mutants ARE killed in QEMU: the
  three `without` mutants break `osdev test file-cap` (Test 14), and a broken `next_generation` panics the
  first spawn. What no test at any level checks is the distinctive SEC-7 property, that GRANT is removed so a
  receiver cannot re-delegate: a `narrow_embedded_for_receiver` that returned the cap unchanged would pass
  every suite, while `audits/security-audit.md:629` records SEC-7 "FIXED (file-cap 10/0)". A focused
  mutation run on the HEAD-identical
  capability files caught 45 of 94 mutants with the existing tests, and 66 (plus 3 timeouts) with the
  reference-model test added. Every QEMU suite is x86 only, so the ELF32 loader branch and every boot-path
  divergence above are untested. No test drives a kernel stack into its guard on any port.
- **E6. The SEC-11 guard is a `debug_assert`.** ARGUMENT SUPPORTED, info (KNOWN, SEC-11). The property it
  guards holds today: every caller of `bump_generation` was enumerated and none can pass a stable gate id
  (`capability/table.rs:189-197`, `:268-280`). The guard is compiled out of every release and board image,
  and no CURRENT test reaches its condition, although it did fire in host proptests once (commit `74ee6ff3`,
  `audits/kernel-audit.md:1310-1314`) and is armed in the debug kernels that `scripts/arm_build.py` and
  `scripts/riscv_build.py` build by default for QEMU. A finding that it was "never armed" was refuted (section 8). Under the release profile the
  capability prototype shows `holds_resource(SPAWN, WRITE) = true` while `get()` says CapRevoked, so the
  guard is the only thing that would catch a future caller bumping a stable id.

---

## 12. Specification and comment drift (recorded, not edited)

Each of these is a document or comment asserting something the code does not do. None was changed.

- **CLAUDE.md 10.5 amendment (2026-10-10):** "A dead task's stack and page tables are not freed until no core
  is running it or still leaving it." D7 is a counterexample. The same claim is at `scheduler.rs:3072-3085`.
- **`scheduler.rs:849-851`:** the pending page-table root (`CORE_PENDING_PML4`) is "always written by one
  core and read by the same core, so ... ordering is sufficient"; D8 is a same-core re-entrancy that frees it
  twice.
- **`audits/security-audit.md:230`** marks the blocked-sender EndpointDead path "traced race-free" (one
  sender only, D1); **`:126`** marks the `CALL_AWAIT_EP` death-wake race-free (timeouts not considered, D2).
- **`audits/kernel-audit.md:536-539`:** the band's owner-recycling hazard is "closed by construction" (D14);
  **`:1077`:** the spawn rollback leaks nothing toward the id space (D15).
- **`audits/unsafe-audit.md:3497`:** the NameTable SAFETY argument is recorded as sound (section 10).
- **`ipc/message.rs:35-37`:** a dead incarnation's id "differs" (LIFO reuse, D5).
- **`routing_model.rs:13-16`:** the model exists because `routing.rs` cannot host-compile (it can, E4).
- **`kernel/src/syscall/CLAUDE.md`:** "the first thing a handler does is validate" (AcquireSendCap reads the
  caller's own name first, harmlessly); the table lists retired syscall 7 as gated by SPAWN and says "1-56
  with 9 removed"; `docs/introspection-capability.md:83` misstates the query surface.
- **`delegated.rs:166-168`:** the band lock is never held across the global-table lock; `revoke_owned`
  (`:152-157`) does exactly that (no cycle results).
- **`scheduler.rs:1494-1497`:** the deepest syscall is about 4.6 KiB (measured 21.5 KiB release).
- **`invariants/`:** the module claims "the kernel checking its own invariants" and holds one near-tautological
  and one narrow assertion; `assert_no_mid_execution_migration` cannot fire for the property it names.
- **`arch/riscv64/mod.rs:875-877`:** `spawn_supervisor` "needs core 0 marked ready" (x86 spawns before it);
  arm never registers core 0's hardware id although a comment says every core does.
- **False concurrency comments**, among them `dispatch.rs:631-633`, `arch/arm/mod.rs:2565-2567`,
  `memory/allocator.rs:498-499`, `scheduler.rs:960-962` and `task/mod.rs:126-128` (both say the kill path
  runs in the timer ISR; it does not), and the claim that `yield_current` is called from the IPI handler
  (`scheduler.rs:2090`).
- **`GLOSSARY.md:104`** defines W^X as "no page is both writable and executable" with no per-arch qualifier;
  the kernel map is not W^X on arm, aarch64 or riscv64 (D21). (`docs/ENGINEERING_ASSURANCE.md:115` refers to
  the loader's user-segment W^X, which holds on every port.)
- **The legacy `enqueue`** publishes `TASK_VALID` before `TASK_CORE` and the task fields; it is dead code, as
  are the `TASK_RECV_BUF` accessors.

---

## 13. Recommended corrections, in order (none applied)

Ordered by consequence over size. Each kernel change is one change per flash, QEMU before any board, and is
the operator's to approve. The smallest are one or two lines.

| Order | Defect | Change | Size | Why first |
|---|---|---|---|---|
| 1 | D8 | take the deferred-free work atomically with interrupts masked | a few lines in `drain_pending_kstack` | double free of a live kernel stack or root, in a window of a few instructions that every self-kill opens |
| 2 | D7 | on a claim abort, clear `CORE_CURRENT` only and leave `CORE_LEAVING` | one helper, three call sites | use-after-free of a stack and root under concurrent kills (chaos does this) |
| 3 | D2 | `try_recv` passes `None`; add `cancel_wait` on timeout and error exits | one line, then one function and three calls | false success at the kernel boundary, reachable by ordinary services |
| 4 | D1 | record every blocked sender, or refuse the second loudly | `RoutingEntry` grows a slot set | a hang with a lost message and capability, reachable by any two clients |
| 5 | D12 | advance the tick only on the real timer interrupt | per-arch entry flag | the shutdown and reboot deadline, and every timeout, fire early on x86 |
| 6 | D10 | record the self-kill per core, not per slot | a per-core flag at four sites | a dead task can run with another task's capabilities |
| 7 | D15, D16 | `free_endpoint_id(rep)` in the unwind; `lock_irq` on `FREE_IDS` | three lines | an endpoint-id leak reachable without privilege (whether it can reach the id-exhaustion panic is a PROOF GAP), and a core-0 wedge on supervisor respawn |
| 8 | D3 | purge a dying slot's records, or stamp registrations with a life counter | moderate | closes the slot-reuse class (seven fields) |
| 9 | D4, D5, D6 | CallDeadline deadline before the send; generation-checked `call_dequeue`; ignore the wake result | small each | the bound the 8.2 amendment promises |
| 10 | D9, D11, D14, D17, D18 | as in section 6 | small to moderate | each narrower |
| 11 | D19 (A10-2), D20, D21 | per-arch hardening | per port | already partly recorded |

**Changes outside the kernel that would raise confidence at once, with no kernel change:**

- Put `cargo test -p kernel --lib --locked` into the `osdev` gate (E3), about 1.3 s once built.
- Make `commandments.py`'s `files_in_scope` accept file paths and assert every scope entry yields a file
  (E1).
- Revoke `fs`'s open files before its in-request remount (D26, one line in `services/fs`).

---

## 14. Executable verification proposal (deliverable B)

### 14.1 What exists

| Mechanism | Level | Runs on | Evidence of strength |
|---|---|---|---|
| `scripts/commandments.py` (21 checks), `commandments_redteam.py` | ARCHITECTURAL CHECK | the `osdev` gate (commandments) / by hand (redteam) | 28 of 32 injected violations missed; scope bug (E1) |
| `unsafe_check.py`, `arch_boundary_check.py`, `arch_seam_check.py`, `port_scope_check.py`, `shared_surface_check.py` | ARCHITECTURAL CHECK | the `osdev` gate | not mutated in this pass |
| 98 kernel host tests (proptest) | PROPERTY TEST | **no automatic path** (E3) | real code for cap/rights/generation/queue/message/clock/elf_flags; MODELS for routing, names and the allocator (E4) |
| `cargo mutants` (`kernel/mutants.toml`) | mutation adequacy | a paused workflow ending `\|\| true` | five months old; excludes the security-critical files |
| Section 22 QEMU suites | IMPLEMENTATION VERIFICATION (runtime) | by hand; x86 only | P1, P3, P9, P10 weaker than their rows (4.3) |
| Hardware cards | IMPLEMENTATION VERIFICATION (runtime) | by hand | outside this pass |

### 14.2 What can be automated, and where it should run

**Fast lane (every `osdev` build, seconds, no toolchain beyond Python):**

| Check | Level | Catches | Cost | State today |
|---|---|---|---|---|
| syscall gate check (C4) | ARCHITECTURAL CHECK | an ungated arm, an effect before its gate, a gate that falls through, a caller-chosen right, a gate weakened in its signature, an error path returning 0 | ~3 s | passes the real tree (1 warning, ResourceInvoke right 0); its records belong in `COMMANDMENTS.baseline.toml` |
| `irq_lock_check.py` | ARCHITECTURAL CHECK | a plain lock shared with an IF=0 context; serial output under a hot lock | seconds | flags D16 and D28 today |
| `kill_order.py`, `release_pairs.py`, `per_slot_reset_check.py` | ARCHITECTURAL CHECK | teardown order (D14), allocate/free pairing (D15), per-slot fields not reset at reuse (D3) | seconds | each fails today: land with a baseline of the recorded debt, may only fall |
| `boot_parity.py` | ARCHITECTURAL CHECK | a hardening seam that is a stub on some port (D21) | seconds | reports three stubs |
| `anchors.py` (model drift) | ARCHITECTURAL CHECK | a modelled source line that changed, so a model check cannot outlive its code | milliseconds | 53 anchors, 0 drifted |
| `abi_consistency.py`, `safety_presence.py` | ARCHITECTURAL CHECK | one-sided kernel/SDK ABI edits; new `unsafe` blocks with no SAFETY | seconds | consistent; 16 blocks to ratchet |
| commandments hardening (E1, E2) | ARCHITECTURAL CHECK | the missed mutations of section 4.1 | ~200 lines of Python | proposed |

**Host-test lane (every build that compiles the kernel, seconds):**

| Check | Level | Catches | Cost | Needs |
|---|---|---|---|---|
| `cargo test -p kernel --lib` | PROPERTY TEST | regressions in the host-compiled modules | ~1.3 s | an `osdev` change only |
| capability reference model, fast lane (2 seeds x 20k ops) | PROPERTY TEST on real code | the 16 recorded capability faults, as measured at 3 x 40k (the fast-lane configuration itself is not yet mutation-tested; three faults are caught only by phases at steps 20,000 and 40,000) | ~10 s | nothing in the kernel: it compiles the real files in place by path |
| IPC routing state machine | PROPERTY TEST on real `routing.rs` | D1, D2, D3, D5 and 11 routing mutants | ~5 s | to live in the repo: `kernel/src/lib.rs` gains host builds of `ipc/endpoint.rs`, `routing.rs`, `names.rs` and a `MAX_TASKS` seam (today the prototype copies the constant, which would drift). No change to `routing.rs` itself |

The capability and IPC checks reproduce every recorded defect today and pass while they do. They are built
for that: each recorded
divergence is a probe that exits 0 while it reproduces and 3 when it stops, so a fix that is not followed by
updating the record, or a change that silently alters a known behaviour, is caught. They can land as an
expected-failure ledger before the fixes.

**Nightly or on-demand lane (minutes):**

| Check | Level | Cost |
|---|---|---|
| kill/reclaim model checker (C3), 26 runs | MODEL CHECK | ~3.5 min |
| capability model full (8 seeds x 40k) plus its 19 mutants | PROPERTY TEST + mutation adequacy | ~5 min |
| IPC harness 12 mutants; 30,000-case soak | mutation adequacy | ~1.5 min; ~6 s |
| `cargo mutants` over the capability and IPC files with a survivor ratchet | mutation adequacy | ~13 min |
| the small models: `selfkill_reuse_model.py`, `irq_mask_model.py`, `kill_spawn_model.py`, `stale_pick.py`, `mc.py` | MODEL CHECK | seconds to minutes each |

**QEMU lane (separate job, one boot each; not yet written; each is expected to fail at HEAD by the traces in
section 6, and the slot-reuse and timing-dependent ones only probabilistically):** two blocked senders on one endpoint (identity
4C); a 1 s CallDeadline to a full, non-draining target; a capless task polling query 21 while the harness
writes COM2; a 20 s deadline under cross-core IPC load compared with the TSC; forced post-registration spawn
failures with the endpoint-id counter bounded; a kill-and-respawn into a freed slot. **Hardware** stays by
hand, per the standing rules.

**Results kept by commit:** each lane writes its outcome to a file named by the commit SHA (a proposal; no
such store exists today).

### 14.3 Cost, access and limits

- **Access needed:** none beyond what this machine has (Python, the Rust toolchain, `cargo-mutants`).
  Kani, Verus and TLA+ are not installed (section 16).
- **Maintenance:** the gate check's records change whenever a syscall, query or gate changes, each with a
  reason, which is the point. The models must be re-extracted when an anchored line changes; `anchors.py`
  forces that. The IPC emulation of the scheduler side must track `dispatch.rs` and `scheduler.rs`.
- **Limits that stay:** none of these is a proof. The host checks are single-threaded. The models are
  sequentially consistent and bounded. Text checks cannot decide whether a gate guards the object acted on,
  or notice a gate weakened by an extra conjunct (the gate check's two stated misses); a gate moved into a
  macro is not seen, and that fails closed. Nothing here runs on arm, aarch64 or riscv64.
- **What needs the operator's approval:** every change under `kernel/` (the `lib.rs` seams, any fix), any new
  entry in `osdev`'s `EXTRA_CHECKS`, and landing the prototypes at all. Verification stays outside the
  kernel; none of it adds a responsibility (MISCIS unchanged).

---

## 15. The prototypes (deliverable C)

All of them live OUTSIDE the repository, in the session scratchpad, written as `$V` below:

`$V` = `C:\Users\Bankole Ogundero\AppData\Local\Temp\claude\C--Downloads-Bankole-GodspeedOS-github-godspeed\79378695-e6ac-477e-ade5-88f3ebdaea8b\scratchpad\verify`

That directory is temporary. Nothing in the repository depends on it, and it is recorded here so the runs can
be reproduced while it exists; landing any of it is the operator's decision (section 14.3). None needs
network access or anything not already on this machine. C1 to C4 were rerun by hand for this report at
`537e6861` with the results shown; the C5 prototypes were run once by the readers that built them.

### C1. Capability reference model (PROPERTY TEST against the real code)

- **What:** a Rust crate that compiles the REAL `kernel/src/capability/mod.rs` (and so `cap.rs`, `rights.rs`,
  `generation.rs`, `table.rs`, `delegated.rs`) and `smp/spinlock.rs` in place via `#[path]`, with shims only
  for `kprintln!` and interrupt masking. A spec-derived model of lives, generations, rights and the delegated
  band generates valid and adversarial operations (forged slots and generations, wrong-owner revoke, boundary
  ids, full tables, band exhaustion, id reuse, generation exhaustion) and compares every observable after
  every step. It replicates the CALL ORDER of the dispatch and kill paths but does not run that code.
- **Run:** `cd "$V\proto-capmodel"` then `python -I run_all.py --seeds 8 --ops 40000` (about 5 minutes; add
  `--slow` for the generation-wrap probe). Pieces: `cargo build --release`,
  `target\release\proto-capmodel.exe run --seed 1 --ops 40000`, `... probe d1-mint-after-death`.
- **Result:** Phase A passes, 8 seeds x 40,000 operations (seed 1 alone: about 121 million validation
  comparisons and about 3,300 endpoint and 5,700 delegated reuses; the counts vary slightly between runs:
  3,292 and 5,768 in the reader's run, 3,282 and 5,743 in the rerun). All 9 probes reproduce their recorded divergence. Mutation:
  16 of 16 fault mutants caught (generation check skipped, revoke without bump, dead without bump, rights
  `contains` any, `without` a no-op, narrowing that widens, non-owner revoke, band bound off by one, reuse at
  generation 0, `release_owner` without bump, bump that wraps, matches always, slot-to-id off by one, insert
  clobbering slot 63, holdings ignoring rights, revoked reported as dead); 3 expected misses: an equivalent
  log threshold, an overflow-table coverage gap (ids at or above 8192 are never generated), and
  `Capability::validate`, which is dead code in the kernel. Rerun for this report: OVERALL OK.
- **Does not establish:** anything about `dispatch.rs`, `scheduler.rs`, `routing.rs` or `names.rs` (only
  their call order is replicated); any concurrency (single thread, masking is a no-op on the host); any
  arch; release-build behaviour beyond two probes. Not a proof.

### C2. IPC routing state machine (PROPERTY TEST against the real `routing.rs`)

- **What:** a scratch crate compiling the REAL `kernel/src/ipc/{routing,queue,message,endpoint}.rs`,
  `capability/{generation,cap,rights}.rs` and `smp/spinlock.rs` unchanged, with three shims (`MAX_TASKS`
  copied from `scheduler.rs:32`, `kprintln!`, interrupt masking). The dispatch loops and the scheduler's
  wake and block semantics are EMULATED from reading (a model, not the code) under one schedule (the woken
  task runs next). Results are compared operation by operation with a spec model.
- **Run:** `cd "$V\proto-ipcmodel\harness"`, set `CARGO_TARGET_DIR` to a short path (the full scratch path
  exceeds the Windows path limit for the MSVC linker; the 8.3 form
  `C:\Users\BANKOL~1\AppData\Local\Temp\claude\C-4E70~1\793786~1\SCRATC~1\verify\PROTO-~4\t` works), then
  `cargo test --offline --test routing_sm -- --nocapture`. Mutants: `python -I "$V\proto-ipcmodel\run_mutants.py"`.
- **Result:** 22 passed, 1 ignored (the 30,000-case soak, which passes in about 6 s when run). 7 positive
  tests and 3,000 generated cases pass; 11 counterexample tests reproduce D1, D2 (the try_recv, CallDeadline and
  ReplyDead forms), D3 and D5, four of them on the raw API with no model at all (D6's routing half is
  reproduced by the IPC reader's separate harness, `cx_ipc3`, not by C2); an unrestricted profile finds a violation and shrinks it to 2 to 6
  operations; each relaxation is found unaided. Mutation: 11 of 12 killed (no generation bump, no sender
  wake, queue past 16, ignore the generation, drop the promotion, revert `backlog/fixed/65`, no receiver
  wake, `take_call_waiter` a no-op, drop the oldest, no drain, no reply match); 1 survived and is equivalent
  (no reset in `try_register`, because kill already clears the entry). Rerun for this report: as recorded.
- **Does not establish:** concurrency (one schedule; the lost-wakeup property stays a PROOF GAP); the
  dispatch and scheduler code itself (emulated); arm, aarch64 and riscv64 interrupt behaviour; `names.rs`,
  the console's kernel-blocking path and credit accounting.

### C3. Kill and reclaim model checker (MODEL CHECK, sequential consistency)

- **What:** `schedmodel.py`, an explicit-state breadth-first checker in Python's standard library over a step
  model of `timer_tick_from_irq`, `yield_current`, `block_and_reschedule`, `run()`, `kill_task_by_slot`,
  `kill_current`, `handle_kill`, `drain_pending_kstack`, reserve and commit, and `wake_by_slot`, each step one
  shared access, CAS, lock operation or switch. It checks use-after-free of a stack or root, double and live
  frees, revival of a dead task, migration, stale or double resume, write after claim, return into a dead
  task, root leaks, and that every kill completes. `stepmap.py` maps each step to its line; `anchors.py`
  fails if any of the 53 modelled lines drifts.
- **Run:** `cd "$V\proto-schedmodel"` then `python schedmodel.py` (about 3.5 minutes; NOT with `-I`, which
  hides its sibling module), `python schedmodel.py -s two_killers -m fix_abort` for one scenario and traces,
  `python anchors.py C:\Downloads\Bankole\GodspeedOS\github\godspeed`.
- **Result:** 26 runs, 1.28 million states, all expectations met: the faithful model reports D7
  (CLAIM_GAP, UAF_ROOT), D8 (double and live frees), D9 (PML4_OVERWRITTEN), D10 (RETURN_INTO_DEAD_TASK,
  WRITE_AFTER_CLAIM) and the crossed-kills hang. `fix_abort` makes every CLAIM_GAP and use-after-free
  scenario clean (D7); `fix_drain` removes D8's double and live frees (D9's PML4_OVERWRITTEN remains);
  `fix_abort+fix_leave` clears the crossed-kills hang. No correction for D9 or D10 is modelled here (D10's
  fixed model is `selfkill_reuse_model.py`, C5). Six negative tests are caught (no wait, CURRENT before LEAVING,
  LEAVING cleared before the switch, no Dekker re-read, self-kill without deferral, a queue of 1). Anchors: 53
  checked, 0 drifted.
- **Does not establish:** anything about the binary; weak memory on any port; timing (a TIMING-tagged result
  is reachable in the model, not shown plausible); paths not modelled (supervisor respawn branches, IPC
  teardown, the abandon budget, lower page tables, `switch_context` bodies). Bounded at 1 to 3 cores and 2 to
  5 slots.

### C4. Syscall gate check (ARCHITECTURAL CHECK)

- **What:** `syscall_gate_check.py`, standard-library Python in the style of `scripts/commandments.py`. It
  reads `kernel/src/syscall/dispatch.rs` (or `--file`) with comments and literals blanked, splits the
  dispatcher arms, resolves each to its handler, recognises slot, holdings and ownership gates, classifies
  calls by resolving their definitions across the neutral kernel and each arch to depth 3, and checks: the
  enum and dispatcher agree; every arm is gated or allowlisted with a reason (and stale entries fail); no
  state change precedes a gate; a failing gate returns; gate rights are constants unless allowlisted; every
  ungated query is allowlisted with a declared effect set; multi-op gates are attributed per op; error paths
  returning 0 are allowlisted; and a pin of every arm's full gate signature. It does not duplicate
  `I-syscalls`, `I-introspect` or `I-authorities`, which pin names and numbers only.
- **Run:** `python -I "$V\proto-syscallgate\syscall_gate_check.py" --report` (about 3 s); negative tests:
  `python -I "$V\proto-syscallgate\run_mutants.py"` (about 40 s).
- **Result:** 51 arms, 0 failures, 1 warning (ResourceInvoke's runtime right), 17 info lines listing error
  returns after a first state change. Mutation: 18 of 18 behaved as expected: 13 caught (gate removed, effect
  moved above a gate, new ungated arm, error path returning 0, query made ungated, gate that logs and falls
  through, caller-chosen right, wrong resource, effect hidden in a helper, inline atomic store before a gate,
  `kill_by_name` before its gate, dangling enum variant, SpawnImage's second gate deleted); 2 stated misses (a
  gate on one slot with the action on another; a gate weakened by an extra conjunct); 3 fail closed by design.
- **Does not establish:** that a gate guards the object acted on; that a gate's condition is the intended one;
  anything inside a macro; the truth of its list of pure functions; any runtime behaviour.

### C5. Smaller prototypes built by the readers

All under `$V`, standard-library Python unless noted, each run once by the reader that built it:

| Path | Level | What it shows |
|---|---|---|
| `sched\sched_mc.py` | MODEL CHECK | the CORE_LEAVING handshake including the abort path (D7), the blocked-sender registration (D1); 28 of 28 expectations |
| `irq-smp\models\mc.py`; `irq-smp\checks\irq_lock_check.py` | MODEL CHECK; ARCHITECTURAL CHECK | `FREE_IDS` wedge in three steps, clean with `lock_irq` (D16); plain locks shared with IF=0 contexts and serial under hot locks (D16, D28) |
| `weakmem\litmus.py`, `stale_pick.py`, `call_await_kill.py` | MODEL CHECK | weak-memory litmus approximations of the handshake; the run-loop stale pick (D11); the `CALL_AWAIT_EP` clear (D3) |
| `followup-user-copy-fault-kill\selfkill_reuse_model.py`, `per_slot_reset_check.py`, `copy_order_check.py` | MODEL CHECK; ARCHITECTURAL CHECK | the self-kill slot reuse (D10, two plus one counterexamples, fixed model clean); per-slot fields not reset (D3) |
| `followup-device-grant\kill_order.py`, `kill_spawn_model.py`, `release_pairs.py` | ARCHITECTURAL CHECK; MODEL CHECK | four device releases run late (D14); 30 bad interleavings, none with release before publish; unwind pairing (D15) |
| `followup-band\kill_mint_model.py`, `kill_fence_check.py`, `fs_remount_revoke_check.py` | MODEL CHECK; ARCHITECTURAL CHECK | mint after `release_owner` (D14); the `fs` remount leak (D26) |
| `followup-irq-line\irq_mask_model.py` | MODEL CHECK | the silent permanent mask (D18), with a self-test |
| `power-deadline\tick_model.py` | simulation of five fixed schedules (not a model check; UNVERIFIED as one) | the tick-rate and deadline arithmetic (D12) |
| `boot-parity\boot_parity.py`, `ap_subset_model.py` | ARCHITECTURAL CHECK; MODEL CHECK | hardening stubs per port (D21); non-dense ready sets (D17) |
| `kstack\*.py` | measurement | per-port stack depths from `-Z emit-stack-sizes` (D20) |
| `memory\va_layout_check.py` | ARCHITECTURAL CHECK | fixed per-task VA regions overlap (the heap and MMIO bases) |
| `traceability\scripts\abi_consistency.py`, `safety_presence.py`, `syscall_gates.py`, `mutate.py` | ARCHITECTURAL CHECK | kernel and SDK ABI agree; 16 unsafe blocks with no SAFETY; the ungated syscall set; the commandment mutations behind section 4 |
| `tests-strength\rep4\src\capability\refmodel.rs` (Rust, proptest) | PROPERTY TEST | a second reference model inside a lib-only replica; raises the focused mutation score from 45 to 66 of 94 |
| `tests-strength\rep3\src\ipc\routing_cx.rs` (Rust) | PROPERTY TEST | the real `routing.rs` host-compiled with one constant; the executed counterexamples |

---

## 16. Machine-verification roadmap

Nothing in this repository is machine-checked today, and none of the tools below is installed. In order of
value for effort:

1. **Kani bounded proofs (IMPLEMENTATION VERIFICATION, bounded):** `CapTable::insert`, `remove` and `get`,
   `Band::claim` and `take_one_owned`, and `MessageQueue` (needs a message-size seam; the routing table's
   statics are about 6.5 MB, so `routing.rs` needs a size seam too). Uses the same `#[path]` wiring as C1, so
   no kernel change. Catches the bound and clobber classes exhaustively rather than by sampling.
2. **TLA+ (MODEL CHECK, specification level):** the routing wake protocol (registration, wake CAS, the
   pending-wake flag, the block CAS, preemption, IPI, kill) at 2 cores, 3 tasks, 2 endpoints, queue depth 2,
   for the lost-wakeup PROOF GAP; and the kill/reclaim protocol re-stated from C3. Replay C2's traces as TLA+
   behaviours to tie the specification to the code.
3. **herd7 or GenMC litmus tests (MODEL CHECK, per memory model):** the Dekker claim handshake and
   `CORE_LEAVING` order under ARMv7, ARMv8 and RVWMO, replacing the approximations used here.
4. **Alloy (relational):** the spawn delegation relation (privilege word, `SUPERVISOR_DELEGATABLE`, request
   flags, installs) against "delegation never mints" (D22).
5. **Verus (MACHINE-CHECKED PROOF, deductive):** the capability table's invariants (I-RIGHTS, I-REVOKE,
   I-NORESURRECT) once 1 to 4 have settled the specification. This is the first step that could support the
   words "machine-checked" for any property, and only for that property.

Each step needs its tool installed and the operator's approval; each says nothing about any other property,
and nothing about an arch it was not run for.

---

## 17. Checkpoint (deliverable D)

- **Reviewed commit:** `537e68615e5e496924c4a2b9434cdd971fec3f1b`.
- **Files examined:** the whole neutral kernel (`capability/`, `ipc/`, `task/`, `syscall/`, `memory/`,
  `smp/`, `interrupt/`, `invariants/`, `bootcon/`, `loader.rs`, `main.rs`, `log.rs`, `clock.rs`, `elf_flags.rs`,
  `lib.rs`); the arch files the findings cite on x86_64, arm, aarch64 and riscv64; `scripts/commandments.py`,
  `commandments_redteam.py`, `COMMANDMENTS.baseline.toml`, `unsafe_check.py`; `kernel/mutants.toml` and
  `mutants.out/`; the probe and validator sources of the section 22 suites; the services and SDK lines cited.
- **Invariants established (ARGUMENT SUPPORTED, some PROPERTY TESTED or MODEL CHECKED):** section 7.1.
- **Defects:** 29 kernel defects (7 high, 15 medium, 7 low; 27 counterexamples and 2 proof gaps, D24 and
  D29) and 6 enforcement-layer records (E1 to E3 counterexamples, E4 and E5 proof gaps, E6 argument
  supported), sections 6 and 11; all recorded in `backlog/90`.
- **Proof gaps and transitions left:** sections 7.2 and 7.3. The largest: concurrency of IPC wake and block,
  weak memory everywhere, and every port other than x86 at runtime.
- **Assumptions:** section 2.
- **Open obligations:** the corrections of section 13 (operator approval, one change per flash, QEMU first);
  landing any prototype (operator approval; the IPC harness needs a `lib.rs` seam); the specification drift
  of section 12, to be corrected by whoever fixes the code it describes, or amended.
- **Next step:** the operator chooses (a) which defects to fix first (section 13 proposes D8, D7, D2, D1, D12,
  D10), and (b) which checks to land first (section 14 proposes the gate check and `cargo test -p kernel
  --lib` in the gate, then the capability and IPC harnesses as an expected-failure ledger). The brief's
  phase 3 (IPC transfer, cancellation and termination) is largely done: cancellation and termination by C2,
  transfer by the cap-transfer reader and C1's transfer operation. Its remaining half is the concurrent model
  of item 2 in section 16.

---

## Appendix A. Every raw finding, by defect

Each raw finding is `area:id` as the reader named it, with its skeptic vote ("argument" means an
ARGUMENT SUPPORTED result, not sent to skeptics). The full text of every finding and every verdict is
kept with the scratch evidence.

- **D1:** `proto-ipcmodel:ipc-single-blocked-sender-overwrite` (upheld 3/3), `cap-transfer:transit-single-blocked-sender` (upheld 3/3), `ipc:ipc1-blocked-sender-overwrite` (upheld 3/3), `sched:blocked-sender-single-slot-overwrite` (upheld 3/3), `smp-irq:blocked-sender-overwrite` (upheld 3/3), `syscall:ipc-second-blocked-sender-orphans-first` (upheld 3/3), `tests:ipc-blocked-sender-overwrite` (upheld 3/3)
- **D2:** `proto-ipcmodel:ipc-try-recv-stale-registration` (upheld 3/3), `cap-transfer:transit-stale-receiver-registration` (upheld 3/3), `ipc:ipc2-stale-wait-registrations` (upheld 3/3), `cap-transfer:transit-stale-call-await-spurious-replydead` (upheld 3/3), `proto-ipcmodel:ipc-calldeadline-timeout-stale-call-await` (upheld 3/3), `proto-ipcmodel:ipc-reply-dead-leaves-receiver-registration` (upheld 3/3), `cap-transfer:transit-sendwithcap-failure-ambiguous` (upheld 3/3), `proto-capmodel:sendwithcap-blocked-loss` (upheld 3/3)
- **D3:** `sched:stale-registration-wakes-next-occupant` (upheld 3/3), `cap-transfer:transit-dead-sender-record-survives` (upheld 3/3), `proto-ipcmodel:ipc-dead-sender-record-wakes-reused-slot` (upheld 3/3), `smp-irq:stale-slot-wake-records` (upheld 3/3), `tests:ipc-stale-blocked-sender-after-death` (upheld 3/3), `followup:weak-memory-kill-pick-and-publication:call-await-clear-before-quiescence` (upheld 3/3), `followup:user-copy-fault-kill-half-state:stale-wake-deadline` (upheld 3/3), `sched:per-slot-state-not-reset` (upheld 2/3), `cap-core:last-badge-not-reset` (argument)
- **D4:** `sched:calldeadline-send-leg-unbounded` (upheld 3/3), `cap-transfer:transit-calldeadline-send-phase-unbounded` (upheld 3/3), `ipc:ipc7-calldeadline-send-phase-unbounded` (upheld 3/3), `proto-ipcmodel:ipc-calldeadline-send-leg-unbounded` (upheld 3/3), `syscall:calldeadline-send-leg-unbounded` (upheld 3/3)
- **D5:** `ipc:ipc4-call-target-generation-ignored` (upheld 3/3), `cap-transfer:transit-replydead-lost-on-id-reuse` (upheld 3/3), `cap-transfer:transit-sender-ep-recycled-id` (upheld 3/3), `ipc:ipc5-sender-ep-correlation-claims` (upheld 3/3), `cap-core:sender-ep-generation-free` (upheld 1/1), `proto-ipcmodel:ipc-reply-correlation-by-id-only` (upheld 3/3), `cap-transfer:transit-reply-cap-not-one-shot` (upheld 2/3), `ipc:ipc9-resource-invoke-uses-current-generation` (upheld 3/3), `cap-transfer:transit-resource-invoke-routes-to-current-owner` (upheld 1/1), `proto-ipcmodel:ipc-resource-invoke-owner-generation-toctou` (upheld 1/1), `cap-core:acquire-lookup-mint-toctou` (upheld 3/3), `proto-capmodel:acquire-lookup-mint-toctou` (upheld 3/3)
- **D6:** `ipc:ipc3-reply-loses-to-replydead` (upheld 3/3)
- **D7:** `proto-schedmodel:abort-double-release-drops-leaving` (upheld 3/3), `sched:leaving-abort-double-release` (upheld 3/3), `followup:weak-memory-kill-pick-and-publication:abort-release-drops-prev-claim` (upheld 3/3), `proto-schedmodel:crossed-kills-circular-wait` (upheld 3/3)
- **D8:** `proto-schedmodel:drain-reentrancy-double-free` (upheld 3/3), `smp-irq:drain-reentry-double-free` (upheld 3/3)
- **D9:** `proto-schedmodel:pending-pml4-overwrite-leak` (upheld 3/3), `sched:pending-pml4-overwrite-leak` (upheld 3/3), `smp-irq:pending-pml4-single-slot` (upheld 3/3), `proto-schedmodel:kstack-queue-overflow-frees-current-stack` (upheld 1/1), `sched:pending-kstack-overflow-frees-live-stack` (upheld 2/2)
- **D10:** `followup:user-copy-fault-kill-half-state:selfkill-slot-reuse-race` (upheld 3/3), `sched:selfkill-early-slot-release` (upheld 3/3), `proto-schedmodel:selfkill-valid-released-before-leaving` (upheld 1/1)
- **D11:** `followup:weak-memory-kill-pick-and-publication:run-loop-stale-pick-migration` (upheld 3/3), `sched:block-self-switch-same-pointer` (upheld 1/1)
- **D12:** `sched:tick-clock-advanced-by-non-timer-entries` (upheld 3/3), `smp-irq:x86-ipi-advances-monotonic-clock` (upheld 3/3), `followup:power-deadline-isr-context-and-slow-timebase:arm32-hires-wake-advances-tick` (upheld 3/3), `followup:power-deadline-isr-context-and-slow-timebase:riscv64-rearm-starves-bsp-tick` (upheld 3/3), `followup:power-deadline-isr-context-and-slow-timebase:masked-stretches-lose-ticks-no-catchup` (upheld 3/3), `followup:power-deadline-isr-context-and-slow-timebase:arm32-atomic-syscall-skip-drops-clock` (upheld 3/3), `sched:wake-pending-stale-subtick-sleep-early` (upheld 3/3), `proto-syscallgate:sleep-early-wake-reports-success` (upheld 1/1)
- **D13:** `followup:power-deadline-isr-context-and-slow-timebase:arm32-fb-self-deadlock-in-deadline-isr` (upheld 3/3), `followup:power-deadline-isr-context-and-slow-timebase:power-path-iteration-spins-vs-watchdog` (upheld 1/1), `followup:power-deadline-isr-context-and-slow-timebase:kill-wait-extensions-exceed-liveness` (upheld 1/1), `sched:power-deadline-earliest-wins` (argument)
- **D14:** `followup:device-grant-arena-reuse-and-exclusivity:kill-publishes-before-device-release` (upheld 2/3), `ipc:ipc8-reply-id-freed-before-irq-unregister` (upheld 3/3), `followup:delegated-band-shared-exhaustion-and-holder-leak:kill-race-mint-after-release-owner` (upheld 3/3), `cap-core:zombie-window` (upheld 3/3), `sched:killed-task-keeps-running` (upheld 3/3), `proto-capmodel:delegated-allocate-vs-owner-death` (upheld 1/1), `cap-core:delegated-allocate-nonatomic` (upheld 1/1), `cap-transfer:transit-release-owner-not-atomic` (upheld 3/3), `sched:abandon-leaks-permanently` (upheld 3/3)
- **D15:** `proto-capmodel:reply-id-leak-failed-spawn` (upheld 3/3), `followup:device-grant-arena-reuse-and-exclusivity:cleanup-leaks-reply-endpoint-id` (upheld 3/3), `proto-ipcmodel:ipc-endpoint-id-leak-spawn-rollback` (upheld 3/3), `followup:irq-line-masked-and-unnotified:failed-spawn-endpoint-id-leak` (upheld 3/3), `ipc:ipc13-partial-spawn-leaks-reply-id` (upheld 2/3), `tests:cap-reply-id-leak-partial-spawn` (upheld 3/3), `smp-irq:failed-spawn-irq-route-and-reply-id-leak` (upheld 3/3), `followup:irq-line-masked-and-unnotified:failed-spawn-route-inherited-by-reused-id` (upheld 3/3), `followup:device-grant-arena-reuse-and-exclusivity:cleanup-leaves-irq-route-on-reused-id` (upheld 3/3), `followup:device-grant-arena-reuse-and-exclusivity:cleanup-strands-framebuffer` (upheld 3/3), `followup:device-grant-arena-reuse-and-exclusivity:cleanup-discards-device-record` (upheld 2/3), `sched:partial-spawn-leak-respawn-ratchet` (upheld 3/3)
- **D16:** `smp-irq:free-ids-preempted-holder` (upheld 3/3)
- **D17:** `smp-irq:ready-count-as-core-range` (upheld 3/3), `followup:boot-entry-path-parity:rr-placement-nondense-ready-set` (upheld 3/3), `followup:boot-entry-path-parity:aarch64-arena-sized-by-arrivals` (upheld 3/3), `followup:boot-entry-path-parity:aarch64-aps-before-gic-and-tick-check` (upheld 1/1)
- **D18:** `followup:irq-line-masked-and-unnotified:irq-depth-skip-masks-level-line-forever` (upheld 3/3), `followup:irq-line-masked-and-unnotified:irq-loss-unobservable` (upheld 2/3), `followup:irq-line-masked-and-unnotified:x86-ioapic-indirect-access-unlocked` (upheld 1/1), `smp-irq:irq-route-aba` (upheld 1/1), `followup:irq-line-masked-and-unnotified:ehci-consumes-notification-without-rearm` (upheld 3/3)
- **D19:** `followup:user-copy-fault-kill-half-state:a10-2-guard-still-per-core` (upheld 3/3), `followup:user-copy-fault-kill-half-state:riscv64-sum-leaks-across-preemption` (upheld 3/3), `followup:user-copy-fault-kill-half-state:user-copy-slice-api` (upheld 1/1)
- **D20:** `followup:kstack-overflow-loud-on-every-port:riscv64-no-kstack-guard` (upheld 2/3), `followup:kstack-overflow-loud-on-every-port:x86-overflow-triple-fault` (upheld 2/3), `followup:kstack-overflow-loud-on-every-port:debug-profile-kstack-exceeds-64k` (upheld 2/3), `followup:kstack-overflow-loud-on-every-port:guard-verify-slot0-only` (upheld 1/1), `followup:kstack-overflow-loud-on-every-port:stale-deepest-syscall-claim` (upheld 3/3), `followup:kstack-overflow-loud-on-every-port:no-overflow-test-any-port` (upheld 1/1), `followup:kstack-overflow-loud-on-every-port:release-depth-margin` (argument)
- **D21:** `followup:boot-entry-path-parity:kernel-wx-absent-arm-riscv64` (upheld 3/3), `followup:boot-entry-path-parity:dma-posture-unreported-non-x86` (upheld 3/3)
- **D22:** `matrix:spawn-flag-mints-undelegated-console-read` (upheld 3/3), `cap-core:spawn-request-channels-unbounded` (upheld 3/3), `syscall:spawnimage-mints-unchecked-authority` (upheld 2/3), `cap-core:installs-keep-grant-on-delegated` (argument)
- **D23:** `syscall:ungated-query21-pops-com2` (upheld 3/3), `matrix:introspect-ungated-set-unpinned-com2` (upheld 3/3), `proto-syscallgate:q21-com2-ungated-pop` (upheld 3/3), `proto-syscallgate:ungated-query-effects` (killed 0/3), `proto-syscallgate:cmos-index-race` (upheld 1/1)
- **D24:** `sched:cap-consistency-assert-on-racy-snapshot` (upheld 2/2), `matrix:unsafe-safety-and-aliasing` (upheld 2/2), `cap-core:cross-core-cap-table-race` (upheld 1/1), `followup:weak-memory-kill-pick-and-publication:valid-is-not-the-publication-flag` (upheld 1/1), `sched:non-atomic-cross-core-statics` (upheld 1/1), `smp-irq:task-wakeup-err-race` (upheld 1/1), `smp-irq:nametable-data-race` (upheld 3/3)
- **D25:** `proto-capmodel:cap-mint-after-death` (upheld 2/3), `proto-capmodel:gen-collision-dead-vs-global` (upheld 1/1), `cap-transfer:transit-endpoint-gen-reuse-coupling` (upheld 1/1), `ipc:ipc16-no-resurrection-rests-on-mailbox-generation` (upheld 1/1), `cap-core:mint-and-get-ignore-liveness` (argument), `proto-capmodel:stale-errcode-after-reuse` (upheld 3/3), `tests:spec-stale-cap-error-code` (upheld 2/3)
- **D26:** `followup:delegated-band-shared-exhaustion-and-holder-leak:fs-serve-retry-remount-leaks-band` (upheld 3/3), `followup:delegated-band-shared-exhaustion-and-holder-leak:no-per-owner-band-quota` (upheld 1/1), `followup:delegated-band-shared-exhaustion-and-holder-leak:band-exhaustion-not-loud` (upheld 2/3), `followup:delegated-band-shared-exhaustion-and-holder-leak:release-not-owner-checked` (upheld 1/1), `followup:delegated-band-shared-exhaustion-and-holder-leak:holder-death-no-owner-signal` (argument), `followup:delegated-band-shared-exhaustion-and-holder-leak:band-occupancy-unobservable` (argument), `followup:device-grant-arena-reuse-and-exclusivity:arena-length-not-bound-to-reservation` (upheld 2/3)
- **D27:** `proto-syscallgate:spawnimage-success-as-error` (upheld 1/1), `syscall:spawnimage-error-after-success` (upheld 1/1), `proto-syscallgate:allocmem-frame-leak` (upheld 3/3), `syscall:allocmem-frame-leak-still-present` (upheld 3/3), `mem:mem-p4-partial-alloc` (upheld 3/3), `syscall:stub-silent-success` (upheld 3/3), `syscall:taskcaps-empty-arch-divergent` (upheld 3/3), `syscall:silent-argument-narrowing` (upheld 1/1), `ipc:ipc11-calldeadline-class0-ambiguity` (upheld 3/3), `syscall:calldeadline-class-zero-collision` (upheld 3/3), `ipc:ipc10-long-names-silently-unregistered` (upheld 2/3), `syscall:unknown-syscall-code-and-f2-coverage` (upheld 1/1), `cap-transfer:transit-receive-error-has-side-effects` (upheld 3/3), `ipc:ipc12-consume-then-refuse` (upheld 3/3), `proto-syscallgate:invoke-empty-right` (killed 0/3), `cap-core:empty-right-invoke` (argument), `proto-capmodel:zero-right-invoke` (argument), `cap-core:revoke-right-unenforced` (argument)
- **D28:** `smp-irq:kprintln-under-hot-locks` (upheld 3/3), `smp-irq:panic-path-not-reentrant` (upheld 2/2), `smp-irq:current-core-id-silent-zero` (upheld 1/1), `followup:weak-memory-kill-pick-and-publication:irq-mask-asm-nomem` (upheld 1/1), `followup:weak-memory-kill-pick-and-publication:armv7-doorbell-incidental-barrier` (upheld 1/1)
- **D29:** `mem:mem-loader-vaddr` (upheld 2/2)
- **E1:** `matrix:irq-lock-scope-unscanned` (upheld 3/3)
- **E2:** `matrix:pins-fail-open-on-spelling` (upheld 3/3), `matrix:vii-grants-flags-probe-table-unseen` (upheld 3/3), `matrix:arch-syscall-intercepts-outside-pin` (upheld 3/3), `syscall:syscall-gates-not-mechanised` (upheld 3/3), `matrix:no-syscall-gate-check` (upheld 2/2), `matrix:ix-peer-reacquire-reads-comments` (upheld 3/3), `matrix:vi-static-mut-misses-real-statics` (upheld 3/3), `matrix:v-managed-watched-predicate-bypass` (upheld 3/3), `matrix:v-no-panic-scope-and-rationale` (upheld 2/3), `matrix:baseline-knobs-unratcheted` (upheld 2/3), `matrix:redteam-coverage-gap` (upheld 3/3)
- **E3:** `matrix:host-tests-and-mutation-off-every-path` (upheld 3/3)
- **E4:** `matrix:host-models-not-implementation` (upheld 2/2), `ipc:ipc14-models-diverge-from-implementation` (upheld 2/2), `tests:tests-routing-model-divergence` (upheld 2/2), `tests:tests-names-model-divergence` (upheld 1/1), `tests:tests-bitmap-model-divergence` (upheld 2/2), `proto-ipcmodel:ipc-host-tests-cover-a-model-not-routing` (argument), `tests:tests-p8-model-accepts-equal-generation` (upheld 2/2)
- **E5:** `matrix:property-tests-weaker-than-claims` (upheld 2/2), `tests:tests-qemu-oracles-weak` (upheld 2/2), `tests:tests-credit-dead-code` (upheld 3/3), `proto-capmodel:live-validator-untested-on-host` (upheld 1/1), `tests:tests-head-mutation-survivors` (upheld 3/3), `matrix:qemu-suites-x86-only` (upheld 2/2), `followup:boot-entry-path-parity:suites-run-x86-only` (upheld 2/2)
- **E6:** `cap-core:holds-resource-release-unchecked` (argument), `proto-capmodel:sec11-guard-debug-only` (argument), `tests:cap-sec11-guard-never-armed` (killed 1/2)
- **S1:** `proto-schedmodel:spec-drift-quiescence-claims` (upheld 1/1), `smp-irq:spec-comments-false` (upheld 3/3), `syscall:doc-drift-syscall-surface` (upheld 3/3), `proto-syscallgate:syscall-claude-md-drift` (argument), `tests:cap-lock-order-comment` (argument), `matrix:invariants-module-thin` (argument), `sched:migration-assert-tautological` (argument), `sched:dead-code-wrong-publication-order` (argument), `followup:boot-entry-path-parity:core0-ready-ordering-claims` (argument), `followup:boot-entry-path-parity:ipc-init-omitted-arm-aarch64` (argument)
- **X1:** `cap-core:generation-counter-panic` (argument), `proto-capmodel:nextgen-wrap-reissue` (argument)

**Not grouped** (each is either killed, in section 8, or ARGUMENT SUPPORTED, in section 7.1):

- `cap-core:supervisor-name-not-reserved` (killed 0/3)
- `ipc:ipc6-reply-parked-in-pending-send` (killed 0/3)
- `ipc:ipc15-sender-wake-pending-premise` (killed 0/1)
- `sched:kill-by-name-kills-fresh-respawn` (killed 0/3)
- `mem:mem-heap-mmio-overlap` (killed 0/1)
- `smp-irq:ioapic-index-data-race` (killed 1/2)
- `smp-irq:pci-msi-vector-cta` (killed 0/1)
- `syscall:unprivileged-refusal-log-flood` (killed 0/3)
- `syscall:console-foreground-any-holder-release` (killed 1/3)
- `syscall:recv-refusal-is-destructive` (killed 1/3)
- `syscall:resourceinvoke-zero-right` (killed 0/1)
- `syscall:query19-ungated-rng-busywait` (killed 0/1)
- `tests:tests-mutation-blind-to-cfg-test-modules` (killed 0/2)
- `proto-syscallgate:capget-log-unbounded` (killed 0/1)
- `proto-syscallgate:foreground-release-not-owner-checked` (killed 0/1)
- `proto-ipcmodel:ipc-kill-generation-bump-redundant` (argument)
- `followup:device-grant-arena-reuse-and-exclusivity:device-exclusivity-unenforced` (killed 0/3)
- `followup:device-grant-arena-reuse-and-exclusivity:per-slot-record-taken-once` (argument)
- `followup:kstack-overflow-loud-on-every-port:aarch64-overflow-vector-walk` (killed 1/3)
- `followup:kstack-overflow-loud-on-every-port:arm32-riscv-no-stack-probes` (killed 0/1)
- `followup:boot-entry-path-parity:riscv64-continues-after-bsp-init-failure` (killed 0/3)
- `followup:boot-entry-path-parity:boot-observability-x86-only` (killed 0/3)
- `followup:weak-memory-kill-pick-and-publication:weakmem-handshake-sound` (argument)
- `followup:user-copy-fault-kill-half-state:copy-after-dequeue-arch-divergent` (killed 1/3)
- `followup:user-copy-fault-kill-half-state:loader-fault-kill-partial-address-space` (killed 0/1)
- `followup:user-copy-fault-kill-half-state:fault-kill-context-argument` (argument)
- `followup:irq-line-masked-and-unnotified:kill-respawn-mask-after-unmask` (killed 0/1)
- `followup:irq-line-masked-and-unnotified:pi4-genet-line-masked-by-design` (killed 0/1)
- `followup:irq-line-masked-and-unnotified:riscv64-no-delivery-path` (argument)
- `followup:power-deadline-isr-context-and-slow-timebase:x86-uncalibrated-periodic-tick-vs-100hz-literal` (killed 1/3)

