# The invariant registry

The authoritative list of what the MISCIS kernel is checked to keep, one row per invariant (`backlog/90`, "The
invariant registry"). Each row says what the invariant is, which of the six responsibilities (CLAUDE.md 4.3) it
belongs to, the production code that keeps it, what checks it and how strongly, and what is NOT established.

**How a result is worded.** Every check carries one of five words, and they are never run together:

- **PROVED WITHIN STATED BOUNDS** - a Kani harness found no counterexample for every input inside the bound it
  names, over the production file; or Creusot proved a production function against the contract written on it,
  for every input, where the bound is the contract itself. Nothing outside the bound or the contract is claimed.
- **TESTED - NO VIOLATION OBSERVED** - a test, a suite or a hardware card ran and the property held where it
  looked. A test is never a proof.
- **COUNTEREXAMPLE FOUND** - a check or a trace shows the invariant broken, and the row says whether it is fixed.
- **PROOF GAP** - an argument or the code says it holds and nothing checks it.
- **NOT EXAMINED** - nobody has looked.

**Where each kind of evidence comes from.** Kani harnesses: `tests/kani` (CI: `.github/workflows/verify.yml`). Creusot proofs: `tests/creusot`, against
contracts written on the production functions.
Host tests over the real IPC and naming files: `tests/host-ipc`. The kernel's own unit tests:
`cargo test -p kernel --lib`. Both host suites run on every `osdev build` (`scripts/host_test_check.py`). The
section 22 suites boot `qemu-system-x86_64` only, so a QEMU result is an x86 emulator result; the hardware cards
(two PCs, Pi 2, Pi 4, VisionFive 2 Lite) are recorded in `backlog/90` beside each fix. **Emulator evidence and
hardware evidence are said apart, and a pass on one board says nothing about another.**

**What no row claims.** The registry rests on three stated limits that stay open (`backlog/90`, Done when): there
is no concurrent model of IPC wake and block; weak memory is not checked on any port; and the responsibilities
the suites do not reach are not exercised at runtime under QEMU on arm, aarch64 and riscv64, where the hardware
cards are the only evidence. Every Kani result also assumes that the kernel's target build is the same code as the
host build Kani checks, which is the same files included by `#[path]`.

---

## CAP-001: no authority is created without a capability that permits it

**Definition.** A task performs a privileged action only if it holds a capability authorising it, and a spawned
task holds only what its spawner could delegate plus what the kernel mints for its spawn by rule. Rights only
narrow on a copy or a transfer.

**Responsibilities.** Capabilities; IPC (a transfer carries a cap).

**Production code.** `kernel/src/syscall/dispatch.rs` (each handler's gate); `kernel/src/capability/rights.rs`,
`cap.rs`, `table.rs`; `privileges_caller_lacks` and `SUPERVISOR_DELEGATABLE` in `kernel/src/task/mod.rs`.

| Check | Result |
|---|---|
| `scripts/syscall_gate_check.py`: every dispatch arm reaches a recognised gate or is ungated by record, with its mutants run each time | TESTED - NO VIOLATION OBSERVED (45 arms, 9 ungated by record, each acting on the caller alone) |
| Kani `rights_narrow_never_widens`, `rights_without_removes_exactly`, `rights_contains_is_a_partial_order`, `cap_narrow_for_grant_keeps_identity`, `cap_without_grant_cannot_be_regranted` | PROVED WITHIN STATED BOUNDS (every rights byte) |
| Creusot: the contracts on `Rights::contains`, `narrow`, `without` and `union` (`narrow` keeps `ONCE`; none widens) | PROVED WITHIN STATED BOUNDS (every input, against the contract) |
| `scripts/commandments.py` `VII-service-grants` and `IV-contract-authority`: the supervisor's grants pinned, the contracts reconciled | TESTED - NO VIOLATION OBSERVED |
| Adversarial A1, A4, A9, A11, A12, A13, A17 (`osdev test adv`, x86 QEMU) | TESTED - NO VIOLATION OBSERVED |
| Identity Tests 2, 3B, 5B, 9B, 14 | TESTED - NO VIOLATION OBSERVED |

**Not established.** That a gate checks the right OBJECT for the action (the gate check's own stated limit; review);
anything a capability authorises once transferred at run time, which a static pin cannot see by design. Fixed
counterexamples: D22 (CONSOLE_READ minted from a flag), D23 (query 21 ungated).

---

## CAP-002: revoked or dead authority cannot be reused through a stale reference

**Definition.** Once a resource's generation moves on (a revoke or a death), every capability naming an older
generation fails every use, and a reused id starts strictly above every earlier generation.

**Responsibilities.** Capabilities.

**Production code.** `CapTable::get` and `ended_generation` in `kernel/src/capability/table.rs`;
`kernel/src/capability/generation.rs`; the routing table's generation in `kernel/src/ipc/routing.rs`.

| Check | Result |
|---|---|
| Kani `captable_get_refuses_a_cap_after_revoke_or_death_first_endpoint`, `_mid_range`, `_last_direct` | PROVED WITHIN STATED BOUNDS (ids 100, 4096 and 8191 only, one per harness; any rights, any right asked) |
| Kani `generation_bump_invalidates_every_older_cap`, `generation_bump_never_wraps` | PROVED WITHIN STATED BOUNDS (every generation) |
| Creusot: the contract on `Generation::bump` (exactly one more, below the ceiling) | PROVED WITHIN STATED BOUNDS (every input, against the contract) |
| `tests/host-ipc` `t0_stale_generation_send_is_endpoint_dead` | TESTED - NO VIOLATION OBSERVED |
| Property P2, P8, P9; Identity Tests 4, 6B, 10B, 14 (x86 QEMU) | TESTED - NO VIOLATION OBSERVED |

**Not established.** A symbolic resource id over the whole table (CBMC ran past 15 GB); a full `CapTable`. P9 tests
three slots in one holder on one core, not "every holder". Fixed counterexample: D25.

---

## MEM-001: exclusive memory is never owned by two conflicting owners at once

**Definition.** A dead task's kernel stack, page-table root, user frames and slot are freed exactly once, and only
after no core is running it or leaving it; a fresh task never reads a dead task's data.

**Responsibilities.** Memory isolation; Scheduling (the quiescence wait); SMP routing.

**Production code.** `kill_task_by_slot`, `reclaim_dead_slot`, `drain_pending_kstack`, `core_still_using`,
`core_abort_claim` in `kernel/src/task/scheduler.rs`; each port's `reclaim_user_frames` and
`free_page_table_root`.

| Check | Result |
|---|---|
| Property P7 `no_stale_translation_after_death` (32 cycles, cross-core and self kill) | TESTED - NO VIOLATION OBSERVED (x86 QEMU) |
| Property P4 (allocation accounting) | TESTED - NO VIOLATION OBSERVED |
| Test features `claim-abort-test` (D7), `drain-reentry-test` (D8), `pml4-pending-test` (D9), `selfkill-release-test` (D10), `kill-abandon-test` (D14) | TESTED - NO VIOLATION OBSERVED, each shown to fail without its fix |
| Hardware cards for D7 to D11 on all five boards | TESTED - NO VIOLATION OBSERVED |

**Not established.** Frame exclusivity in general: nothing walks the page tables to show a frame has one owner
(PROOF GAP). One holder per device window is the supervisor's rule, not the kernel's (PROOF GAP). Fixed
counterexamples: D7, D8, D9, D10, D14.

---

## IPC-001: no send reports success unless its delivery or completion holds

**Definition.** Every send returns exactly one defined outcome; `Ok` means the message was queued or handed to a
blocked receiver; a `Call` returns the reply from its target's current incarnation, or `ReplyDead`, or times out
within its bound.

**Responsibilities.** IPC; Scheduling (the wake).

**Production code.** `enqueue`, `call_dequeue`, `withdraw_receiver` in `kernel/src/ipc/routing.rs`; the
`MessageQueue` in `kernel/src/ipc/queue.rs`; `handle_send`, `do_call`, `handle_call_deadline` in
`kernel/src/syscall/dispatch.rs`.

| Check | Result |
|---|---|
| Kani over the real `queue.rs` | PROOF GAP: three harnesses ran CBMC past 8 GB or 30 minutes; one, with the payload stubbed, reported a counterexample that a native replay of all its 1,024 inputs refutes, unexplained (`backlog/90`, layer C) |
| Creusot: the contracts on `MessageQueue::enqueue`, `dequeue`, `is_full` and `is_empty` (head and length stay in range; the length moves by exactly one or the call refuses) | PROVED WITHIN STATED BOUNDS (every input, against the contract; not that the slots hold a message where the length says) |
| `tests/host-ipc` `t2a` to `t2c`, `t3`, `t4a`, `t4b`, `t5a` to `t5c`, `t9` and the two `t10` cases (D2, D4, D5, D6) | TESTED - NO VIOLATION OBSERVED, each failing before its fix |
| Adversarial A16 (a deadline bounds the send leg), A17 (a reply cap carries one message) | TESTED - NO VIOLATION OBSERVED |
| Property P10 (each return code is a defined one, read raw) | TESTED - NO VIOLATION OBSERVED |

**Not established.** Interleavings across cores: the host tests are sequential (the stated limit above). A message
arriving between a deadline check and the withdrawal can still wake a task once (D2's recorded residual).

---

## IPC-002: blocked sender and receiver records stay consistent

**Definition.** Every blocked sender is recorded once, in order, and woken exactly once; a record names a task
blocked there now, in the life that registered; a dying task is withdrawn from every record.

**Responsibilities.** IPC; Scheduling.

**Production code.** `PENDING`, `take_blocked_sender`, `forget_blocked_sender`, `clear_call_await`,
`take_call_waiter` in `kernel/src/ipc/routing.rs`; `reserve_task_slot` in `kernel/src/task/scheduler.rs`.

| Check | Result |
|---|---|
| `tests/host-ipc` `t1a` to `t1d` (D1), `t6` (D3), `t0` controls | TESTED - NO VIOLATION OBSERVED, each failing before its fix |
| The real `routing.rs` unit tests, run by `host_test_check.py` | TESTED - NO VIOLATION OBSERVED |
| Identity Test 4B (a blocked sender wakes on endpoint death), `osdev test reply-dead` | TESTED - NO VIOLATION OBSERVED |

**Not established.** No lost wakeup under concurrency (PROOF GAP; the stated limit above).

---

## SCH-001: a reclaimed task cannot be scheduled

**Definition.** Dead is absorbing within a life: nothing moves a Dead task to Ready, no core switches into a slot
whose claim is stale, and a slot is released only once its last core has left it.

**Responsibilities.** Scheduling; SMP routing.

**Production code.** `wake_by_slot` (returns on Dead), `claimed_is_ours`, `CORE_PENDING_SLOT`,
`release_dead_slot`, `retry_abandoned_kills` in `kernel/src/task/scheduler.rs`.

| Check | Result |
|---|---|
| `stale-claim-test` (D11), `selfkill-release-test` (D10), `kill-abandon-test` (D14) | TESTED - NO VIOLATION OBSERVED |
| Identity, shell, chaos and `driver-packages` suites (x86 QEMU); D10 and D11 cards on all five boards | TESTED - NO VIOLATION OBSERVED |

**Not established.** Weak memory (the stated limit above); the claim handshake is argued under sequential
consistency.

---

## SCH-002: reusing a task's identity cannot expose the previous task's authority

**Definition.** A slot or endpoint id taken by a new task carries none of the previous occupant's wake records,
deadline, badge, pending capability or reply correlation, and a reply cap carries one message.

**Responsibilities.** Scheduling; IPC; Capabilities.

**Production code.** `reserve_task_slot` in `kernel/src/task/scheduler.rs`; `sender_gen` and `call_dequeue` in
`kernel/src/ipc/routing.rs`; `spend_once` and the `ONCE` checks in `kernel/src/syscall/dispatch.rs`.

| Check | Result |
|---|---|
| `tests/host-ipc` `t3`, `t4a`, `t4b`, `t6` | TESTED - NO VIOLATION OBSERVED, each failing before its fix |
| Adversarial A17; the D5 cards on all five boards | TESTED - NO VIOLATION OBSERVED |

**Not established.** A model of a slot's whole life across cores (NOT EXAMINED by a bounded harness).

---

## INT-001: interrupt state transitions stay consistent

**Definition.** A vector routes to at most one live endpoint; a dead owner's routes are released and masked before
its id can be reused; a level line masked for delivery is either notified or reported, never left masked silently.

**Responsibilities.** Interrupt routing.

**Production code.** `kernel/src/interrupt/route.rs` (`unregister_endpoint`, `retry_missed`); the IRQ release at
the top of `kill_task_by_slot`.

| Check | Result |
|---|---|
| Identity IR1A and IR1B (x86 QEMU) | TESTED - NO VIOLATION OBSERVED |
| `irq-miss-test` on `osdev test iommu` (D18) | TESTED - NO VIOLATION OBSERVED |
| `commandments.py` `I-irq-lock` (a lock held in interrupt code masks interrupts) | TESTED - NO VIOLATION OBSERVED |

**Not established.** Any port but x86 under QEMU; the hardware cards are the evidence there.

---

## SMP-001: shared cross-core state changes obey the synchronisation they require

**Definition.** State read by one core and written by another is atomic, or taken under a lock that the interrupt
path cannot re-enter on the same core.

**Responsibilities.** SMP routing; every responsibility that shares a table.

**Production code.** `kernel/src/smp/spinlock.rs`; `kernel/src/smp/names.rs`; the `lock_irq` holds of `FREE_IDS`
and `BAND`.

| Check | Result |
|---|---|
| `smp/names.rs` host tests, including a writer racing readers (D24) | TESTED - NO VIOLATION OBSERVED, torn with the counter off |
| `commandments.py` `I-irq-lock`, over the interrupt files | TESTED - NO VIOLATION OBSERVED |
| Review of every other cross-core read (D16, D24) | PROOF GAP |

**Not established.** Weak memory on any port, and any concurrent interleaving beyond the one names test (the stated
limits above).

---

## Checks weaker than their names

Cited above where they bear on a row, and gathered here so nobody relies on a name (`audits/2026-10-16-kernel-verification.md`
section 4.3, `backlog/90` E5):

- **P1** draws 10,000 random slots against a 64-slot table, so nearly every draw is out of range: it tests "no cap,
  no send", not unforgeability. It reads the raw return code since 2026-10-27.
- **P3** checks one rights value on one path, 5,000 times; `DeriveCap` and spawn installs are not exercised.
- **P9** checks three slots in one holder's table, on one core, not "every holder".
- **P10** calls only `try_send`, nearly always on out-of-range slots; a blocking send is not exercised.
- **B4** times `QueryCapRights`, which reads a slot without the generation check, so its number is a floor for a
  lookup and not the validation cost CLAUDE.md 22 names.
- **`kernel/src/ipc/routing_model.rs` and `names_model.rs`** are host tests of models; the real files are tested by
  `tests/host-ipc`.
- **Every section 22 suite boots x86 QEMU.**
