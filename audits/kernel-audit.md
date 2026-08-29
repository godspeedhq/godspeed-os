# Kernel Commandment Audit

> **Living document.** Records every audit of the ring-0 kernel against the constitution's
> invariants. Re-run and append with each audit. First audit: 2026-07-11.



## Audit 11 - the Audit 10 fixes, and two syscalls added then reverted (2026-08-11, `feat/pi4-aarch64` @ `f67f5c15`)

**Why now.** Audit 10 (2026-08-09 @ `f718e5a1`) recorded 4 HIGH / 9 MED / 7 LOW and fixed none of them
by design. Two were then fixed - A10-1 (the panic that halted nothing) in `ff3004cc`, and the SEC-35
half of A10-4 (the pending-cap FIFO) in `755218a5` - and a third change, the console-write counter and
`ConsoleReadTimeout` syscall, was **added and then reverted** (`9a233ad7` + `6231e21c`, reverted by
`a7be98da`). A fix is exactly where a fresh defect hides, and a partially reverted syscall is a live
hazard, so both classes are re-audited against source rather than against their commit messages.

**Scope.** Everything committed to `kernel/` since Audit 10's base, which is `f718e5a1..f67f5c15` (note:
the task named `5426c6db`, which is the *docs* commit recording Audit 10; the kernel commits under audit
sit between the two). That is six commits touching two files: `arch/aarch64/mod.rs` (+37/-3) and
`task/scheduler.rs` (+13). Everything else in the range is `services/` and `scripts/`, which a kernel
audit does not cover. The blast radius of each change was followed outside the diff (the tick path, the
slot-claim paths, all four cap-delivery sites, and the other arches' `halt_all_cores`).

**Method.** Read each change against the mechanism it claims, then trace the claim to its call sites.
For every finding: "what makes this fire, and can that condition occur in the failing case?" Two
candidate findings were discarded for failing that test and are recorded under "checked and rejected".

**North star unchanged:** nothing above the kernel may panic or wedge it.

**Verdict: 2 HIGH, 3 MED, 3 LOW. The reverts are CLEAN.** Both HIGH are the same defect: A10-1 is
**still open**, because the fix reaches exactly one core on one arch.

### Findings

| ID | Sev | Commandment / section | Finding |
|----|-----|----------------------|---------|
| A11-1 | **HIGH** | V, §19, §6.2 | The A10-1 panic fix parks only **core 0**. `PANIC_HALT` is checked inside `uart_rx_poll`, which `timer_tick_from_irq` calls under `if cid == 0`. Every AP keeps scheduling for ~10 s, then self-panics with a misleading `LIVENESS WEDGE`. |
| A11-2 | **HIGH** | V, §19, §6.2 | The fix did not travel to **arm32**, where `halt_all_cores` is still `loop { spin_loop() }` - unmasked and unsignalled - on a port that runs 4-core SMP on real hardware. |
| A11-3 | MED | Inv 12, §26.7 | The EL1-fault park (`exceptions.rs:689`) does not publish `PANIC_HALT`. A10-1 named it as the same class; it was not covered. |
| A11-4 | MED | VII, §26.4 | SEC-35 closed only the **cross-life** half of A10-4. The within-life FIFO desync is unfixed in the kernel, and the kernel still offers no way for a receiver to resynchronise. |
| A11-5 | MED | VII, Inv 12, §8.5 | All four cap-delivery sites install the embedded cap and push its FIFO slot **before** the user copy, then `return -1` if the copy faults. The receiver never sees the message, keeps the cap, and is one FIFO entry ahead - a desync no userspace discipline can avoid. |
| A11-6 | LOW | §26.4 | `PANIC_HALT` is unread on the non-`pi4` aarch64 build: `uart_rx_poll` is an empty stub there and `liveness_deadline_cycles()` is 0, so nothing ever observes the flag and nothing says so. |
| A11-7 | LOW | III | `scheduler::enqueue` is a second slot-claim path that sets `TASK_VALID = true` without the SEC-35 reset. Dead today (zero callers, `#[allow(dead_code)]`); reviving it silently reopens SEC-35. |
| A11-8 | LOW | III | `TASK_LAST_BADGE` is the other per-slot leftover A10-18 named. SEC-35 reset the FIFO count beside it and left the badge. Latent, not currently reachable. |

### The two HIGH, in full

**A11-1. The panic fix parks core 0 and nothing else.** `arch/aarch64/mod.rs:1409` now sets
`PANIC_HALT` and parks the panicking core with `msr daifset, #0xf` - both correct, and the panicking
core genuinely stops. The other half does not work. The check is at `mod.rs:1765`, inside
`uart_rx_poll`, and `uart_rx_poll` has exactly one caller in the tree: `scheduler.rs:1172`, which sits
inside `if cid == 0`. The function's own doc line one line above says so ("Called from the core-0 timer
tick"). So on a 4-core Pi 4 a panic stops the panicking core immediately and core 0 at its next tick;
**the remaining two or three cores never load the flag** and carry on scheduling user tasks over the
state the kernel just declared corrupt. The idle path does not help: `wait_for_interrupt`
(`mod.rs:2215`) is a bare `wfi` with no check. The fix's own doc comment (`mod.rs:1393`) asserts the
mechanism that does not exist - "`PANIC_HALT` is published for every other core; the timer tick checks
it and parks the core" - which is the §18.3 / §26.4 drift class in its most dangerous form: a comment
that would stop the next reader from looking.

The machine does eventually stop, and the way it stops is itself a finding. Core 0 parks *inside*
`uart_rx_poll`, which runs **before** `CORE_LAST_TICK_TSC` is stamped (`scheduler.rs:1195`) and before
the watchdog loop (`:1214`), so core 0 stops stamping too. `liveness_deadline_cycles()` on `pi4` is
`timer::frequency() * 10`, so roughly **10 seconds** later each surviving core notices the two dark
cores and panics with `LIVENESS WEDGE: core N made NO progress`, which names a *victim* of the original
panic rather than the panic. Only then does that core call `halt_all_cores` and park. Two consequences
in the meantime: services keep running (including storage writes) for ten seconds past a panic, which is
precisely the silent-corruption window §6.2 panics to prevent; and because core 0 is gone,
`control::process_pending()` and `scan_timed_wakes()` stop, so every task blocked in `recv_timeout` or
`sleep` on the surviving cores never wakes. **CONFIRMED** by reading `mod.rs:1760-1774`,
`scheduler.rs:1170-1177` and `:1194-1230`; the ten-second cascade is CONFIRMED as code, not observed on
hardware. The fix is to move the check into `timer_tick_from_irq` itself (or into the idle path), where
every core passes, not into a core-0-only helper. Note the residual either way, since the user asked:
a core spinning with IRQs masked - inside `smp::without_interrupts` on a lock the panicked core still
holds - takes no tick and parks never; it spins at full power forever. A flag checked on the tick cannot
reach it, which is the cost the fix's own comment names when it chose a flag over an SGI.

**A11-2. arm32 still has A10-1 verbatim, and it is the port that ships on hardware today.**
`arch/arm/mod.rs:793` is `pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }` - the
exact body A10-1 was raised against. The neutral `#[panic_handler]` (`main.rs:360-363`) is shared, so
an arm32 panic prints `KERNEL PANIC` and enters that loop with **DAIF untouched**. A panic taken with
interrupts live (the boot path after `enable_interrupts`, or any kernel code resumed by
`block_and_reschedule`, which ends `switch_context(...); enable_interrupts();`) is therefore
preemptible: `irq.rs:271` calls `timer_tick_from_irq`, the scheduler switches that core to another
task, and the machine runs on past its own panic. Nothing signals the other cores, and `mod.rs:416`
puts them all in `scheduler::run`. Worse than the aarch64 case in one specific way: because no core ever
stops stamping, arm32's liveness watchdog (`mod.rs:879`, `timer_hz() * 10`) **never fires**, so there is
no ten-second backstop - the panic is simply absorbed. This is pre-existing rather than introduced by
the range under audit, and it is recorded here because the range is precisely where it should have been
closed: the fix was written for one arch when the defect was known to be shared, and the other five
`halt_all_cores` stubs (riscv32/64, loongarch64, s390x) carry the same body. Those four are
demarcation targets that do not run an OS; arm32 is not. **CONFIRMED** code state; the preemption is
CONFIRMED as reachable by construction, not reproduced.

### The MED findings, in brief

- **A11-3.** `exceptions.rs:685-692` is the EL1-fault park: `put_str("halting.")` then
  `loop { wfe }`. Verified that this core *does* stop - AArch64 masks DAIF on exception entry and
  nothing in the handler unmasks it (`exceptions.rs` has one `daifset, #3` and one `daifclr, #4` for
  the SError probe window, no general unmask) - so unlike A10-1's original claim this park holds. What
  it does not do is set `PANIC_HALT`, so a kernel data abort at EL1 leaves every other core scheduling
  with no signal at all, and the machine relies entirely on the ten-second watchdog cascade of A11-1. A
  one-line `PANIC_HALT.store(true, Release)` before the loop puts it on the same footing as `panic!`.
  A10-1 called this out by file and line; the fix did not include it.
- **A11-4.** `reserve_task_slot` (`scheduler.rs:485`) is the right place and is **sufficient for the
  case it claims**: verified that it is the only *live* slot-claim path (`task/mod.rs:3492` is the
  neutral spawn; the six arch bring-up spawners all call it; the only other `TASK_VALID = true` is dead,
  see A11-7), the write precedes the `Release` publish so it cannot race a reader, and no other core can
  be pushing into a slot whose `TASK_VALID` is false. But A10-4's mechanism had two halves, and this is
  the cross-life one. The within-life half - a receiver that *discards* a cap-bearing message leaves the
  cap installed and the FIFO one entry ahead, forever - is untouched in the kernel. It is now closed
  only by each service's own discipline (`shell` drains at three sites, `755218a5`), and A10-4's second
  point still stands unanswered: `try_recv` does not report that a cap rode along, and `RemoveCap`
  clears the table entry but not the FIFO, so a service that gets it wrong has **no primitive to
  resynchronise with**. Every consumer of `take_pending_cap` in the tree (`fs`, `net-stack`,
  `block-driver` x3, `nic-driver` x4, `xhci`, `shell`, `probe`) is one forgotten `continue` away from
  the SEC-35 shape. Whether that is a kernel API defect or a service obligation is a real design call;
  what it cannot be is unrecorded, because SEC-35's commit reads as though the class is closed.
- **A11-5.** A kernel-only instance of the same desync, which no service discipline can avoid.
  `dispatch.rs:333-346` (`handle_recv`), `:386-398` (`handle_try_recv`), `:446-457`
  (`handle_recv_timeout`) and `:1093-1104` (`handle_call`) all run the same sequence: install every
  embedded cap into the receiver's table, push its slot onto the FIFO, **then** `write_user_bytes` the
  payload, and on failure `return -1` / `break -1`. The copy can fail: `validate_user_ptr` is a range
  check, so a buffer that is in range but unmapped passes it and faults in the copy (that is the whole
  reason the user-copy fixup exists). The receiver then gets `-1`, has no message, and has silently
  gained a capability at a table slot it was never told about plus a FIFO head that is not its next
  reply cap. It is self-inflicted and bounded (the receiver's own 64-slot table), so it is not an
  escalation across principals on its own - but it produces exactly the SEC-35 state *within one task
  life*, on the kernel's side of the boundary, and it is reachable deliberately. Note the sender's view
  too: the cap was already removed from the sender's table and the sender was told `Ok` (§8.5), so this
  is the A10-5 silent-transfer class as well. The ordering is trivially fixable: copy first, install
  after.

### The LOW findings

- **A11-6.** `mod.rs:1758-1785`: the `PANIC_HALT` check lives in the `#[cfg(feature = "pi4")]`
  `uart_rx_poll`; the `#[cfg(not(feature = "pi4"))]` body is `pub fn uart_rx_poll() {}`. The QEMU `virt`
  aarch64 build (the arch-demarcation target, `docs/multi-arch.md`) therefore sets a flag nothing reads,
  and its `liveness_deadline_cycles()` is 0 so there is no watchdog backstop either. That target does
  not run a full OS today, which is why this is LOW and not part of A11-1 - but it is the "silent stub"
  shape this port has already been bitten by twice (A10-9), and a reader of `halt_all_cores` gets no
  hint that its second half is feature-gated away.
- **A11-7.** `scheduler.rs:574-602`, `enqueue`, sets `TASK_CTX`/`TASK_CAP`/`TASK_STATE = Ready` and
  `TASK_VALID[i] = true` for a free slot, with no `TASK_PENDING_RECV_CAP_COUNT` reset. Verified to have
  **zero callers** in the kernel, and it is marked `#[allow(dead_code)]`, so SEC-35 is not open through
  it today. Recorded because the fix's own comment says "that is the one point every task life begins
  at" - true of the live path, false of this one - and because this function also publishes `TASK_VALID`
  *before* writing `TASK_CORE`/`TASK_IS_USER`/`TASK_KERNEL_STACK_TOP`, which is the SEC-25 ordering
  inverted. Delete it, or fix it to match; leaving a dead constructor that violates two invariants the
  live one upholds is a trap for whoever revives it.
- **A11-8.** `TASK_LAST_BADGE` (`scheduler.rs:235`) is per-task-slot and is not cleared at slot claim,
  so a reused slot inherits the dead occupant's badge. Verified **not currently reachable**: every
  delivery site calls `set_last_recv_badge` (which writes 0 for an unbadged message) before the
  receiver can act, `take_last_recv_badge` swaps to 0 on read, and all three in-tree consumers
  (`fs:568`, `net-stack:992`, `examples/resource-server:102`) read it immediately after a `recv`. It
  becomes live the moment a service reads the badge before its first receive. A10-18 asked for these two
  to be fixed together; one was.

### Verified CLEAN

- **Both reverts are byte-clean.** `git diff f718e5a1..HEAD -- kernel/` contains no trace of either
  reverted feature, and `git diff 5426c6db..HEAD -- kernel/` is **empty** - the add and the revert
  cancel exactly. Checked positively as well as by diff: `SyscallNumber` (`dispatch.rs:22-76`) ends at
  `SetClock = 50` with no 51 and no gap, the dispatch match has no orphan arm, `InspectKernel`'s query
  match tops out at 22 with no 23, and a repo-wide grep for `ConsoleReadTimeout`, `console_read_timeout`,
  `console_write_seq`, `CONSOLE_WRITE_SEQ` and "query 23" returns nothing in kernel, SDK, services or
  docs. No half-removed static, no dangling enum variant, no stale comment describing the removed
  mechanism. The SDK and shell halves came out with it. This is the class the audit was asked to watch
  for and it is genuinely absent.
- **`park_core_forever`'s SAFETY comment is accurate as written.** "Masking DAIF and halting is always
  valid at EL1" holds (`msr daifset` is unprivileged-at-EL1 and `wfi` needs no trap consideration here);
  "this never returns, so no state is left inconsistent by the mask" is true of the *mask* specifically,
  which is what the comment claims. The state that IS abandoned - the unstamped liveness counter, any
  lock the core held - is a consequence of parking, not of the mask, and is recorded under A11-1 rather
  than as a false SAFETY claim. The `-> !` and the infinite `wfi` loop are correct; `wfi` waking
  spuriously with DAIF set simply re-enters the loop.
- **The SEC-35 reset is correctly ordered and race-free.** It is inside the `TASK_SLOT_LOCKED` critical
  section, inside `smp::without_interrupts`, and precedes the `Release` store of `TASK_VALID`; the slot
  it writes has `TASK_VALID == false`, so no core can be running that task and no `push_pending_recv_cap`
  can target it (both index by `CORE_CURRENT`). Leaving `TASK_PENDING_RECV_CAPS` itself stale is fine -
  a count of 0 makes the entries unreachable and the next push overwrites index 0.
- **No new syscall, no new capability, no new unbounded loop, no count-where-a-duration-belongs.** The
  range adds one `AtomicBool`, one `-> !` helper and one array store. The only new loop is
  `park_core_forever`'s, which is deliberately infinite. Nothing in the range validates less than it
  did.
- **Mechanical guards all PASS.** `unsafe_check.py` (72 audited files, 1096 unsafe lines, no unaccounted
  additions - one line up from Audit 10's 1095, which is the `park_core_forever` block and is accounted
  for). `arch_boundary_check.py` passes. Kernel builds clean on
  `aarch64-unknown-none --features pi4,pi4-smp --release` (87 warnings) and `x86_64-unknown-none
  --release` (75 warnings). The six `unused import` warnings of A10-16 are unchanged in identity - the
  dead-import commit `34bc8233` removed arch-local ones, and the six that remain are the five neutral
  ones plus the deliberate `epoch_secs` seam re-export A10-16 said to KEEP.

### Checked and rejected (stated so nobody re-finds them)

- **"`handle_call` writes up to 4 KiB into a caller buffer whose length it never checks."** It reads
  `copy_len = payload.len().min(MAX_MESSAGE_SIZE)` and ignores `recv_len` - which looks like a missing
  bound until you read `dispatch.rs:1047`, where the buffer is validated for the **full**
  `MAX_MESSAGE_SIZE` precisely because it is in/out. The comment above it says so. Not a bug.
- **"The SEC-35 reset can be skipped by a slot reused without `reserve_task_slot`."** Traced every
  `TASK_VALID[..].store(true, ..)` (two sites) and every caller of `reserve_task_slot` (eight). The
  only bypass is dead code, recorded as A11-7 rather than as a live hole.

### NOT audited (honest coverage)

- **No hardware run, and no panic was induced.** A11-1 and A11-2 are read from source and from the call
  graph. Forcing a panic on an AP and watching whether the machine stops is one boot and is the cheapest
  way to convert A11-1 from confirmed-by-reading to confirmed-by-observation - and it is exactly the
  repo's own standing lesson that an unfired guard is not evidence.
- **The eighteen open Audit 10 findings were not re-verified**, only the two that were fixed. A10-2
  (the per-core user-copy fixup), A10-3 (the break-before-make block split) and A10-4's remaining half
  are all still open as recorded there; A11-4 and A11-5 extend A10-4 rather than replacing it.
- **`services/` and `scripts/` changes in this range are out of scope** by construction. The shell half
  of SEC-35, the xhci heartbeat, the net-stack link work and the selfcheck clock probe are
  userspace-audit material and this pass gives them no coverage.

### Process note

Both HIGH findings are the same shape, and it is worth naming because it recurs: **a fix verified
against the function it edits rather than against the call site that runs it.** A10-1's fix is correct
code in the wrong place - one grep for `uart_rx_poll`'s callers (there is one, under `if cid == 0`)
would have caught it before the commit message claimed "every core". A11-2 is the same omission one
level up: the defect was described as a class in Audit 10 and fixed as an instance. The cheap
mechanical habit that catches both is the one this repo already knows - after writing a fix, grep the
thing it depends on and read every caller, then ask which cores, which arches, and which build
configurations actually reach it.

## Audit 10 - the AArch64 port, second pass: after the USB driver left the kernel (2026-08-09, `feat/pi4-aarch64` @ `f718e5a1`)

**Why now.** Two things changed since the aarch64 Audit 7 (2026-08-05, five findings, all fixed).
`kernel/src/arch/aarch64/xhci.rs` was **deleted** - 2742 lines of ring-0 USB moved to
`services/xhci` (`e71e64a6`), closing Commandment I on this port - and the in-kernel GENET driver
went the same way (`0ae2cb71`). Deleting ring-0 code removes findings; it also moves the
*boundaries*, and a boundary that moved without being re-examined is where the next defect lives.
Audit 7 additionally recorded four things it had **not** covered; three of them are answered below.

**Scope.** The whole of `kernel/src/arch/aarch64/` (8544 lines across 21 files), `syscall/dispatch.rs`
in full, `capability/`, `ipc/`, and this branch's changes to `task/` + `memory/` + `smp/`.

**Method.** Four independent auditors, one per surface (fault + user-copy seam; MMU + page tables;
the arch surface and its devices; the neutral syscall/cap/ipc layer), then a lead pass that
**re-verified every finding against source before recording it**. Findings that could not be given a
concrete trigger were discarded; two were downgraded, and one auditor claim was corrected outright
(see the note under A10-13). The default verdict was not-a-bug.

**North star unchanged:** nothing above the kernel may panic or wedge it.

**Verdict: 4 HIGH, 9 MED, 7 LOW. None fixed here - this is a report, not a change.** The four HIGH
are each userspace-reachable or architecturally unsound, and two of them (A10-1, A10-2) mean the
kernel's *last-resort* behaviour - the loud stop - does not work on this port.

### Findings

| ID | Sev | Commandment / section | Finding |
|----|-----|----------------------|---------|
| A10-1 | **HIGH** | V, §19, §6.2 | `halt_all_cores()` on aarch64 halts nothing. A kernel panic leaves the other three cores scheduling, and does not even mask the panicking core's interrupts. |
| A10-2 | **HIGH** | Inv 12, §18.3 | The user-copy fault fixup is per-CORE, but a user copy **is preemptible**: a syscall resumed after blocking runs with IRQs live. A second task's copy clears the fixup out from under the first. |
| A10-3 | **HIGH** | §26.6, X | A live 2 MiB block is converted to a table **with the MMU on and no break-before-make**, and the TLBI is deferred to the end of 224 iterations. The fix for this was written, reverted, reapplied, and reverted again, with no rationale in either revert. |
| A10-4 | **HIGH** | VII, §7.3, §26.4 | The pending-received-cap FIFO is uncorrelated with the message that delivered the cap and is never reset - a capability **identity** confusion the kernel's API makes unavoidable. This is the likely root cause of the open `fcap` post-restart escalation. |
| A10-5 | MED | Inv 12, §26.7 | A transferred capability is **silently destroyed** when the receiver's table is full - the sender was told `Ok` and has already lost it. Its twin drops the *slot number* and leaks the slot. |
| A10-6 | MED | VII | `nic-driver` gained `hw_irqs: &[0x2A]` on this branch; the death path's `dead_irq` is a hardcoded name match on `xhci`/`ehci`, so the GENET route outlives the driver and is inherited by whichever service next takes the recycled endpoint id. |
| A10-7 | MED | IX, §26.3 | Audit 7's A10-4 TLB-invalidate guard sits on `PageTable::unmap`, whose **only caller is the boot selftest**. The runtime frame-return paths do not invalidate. |
| A10-8 | MED | I, III | `reclaim_pages` frees device-MMIO and DMA-arena leaves into the RAM frame allocator; x86's counterpart explicitly skips them, with a comment naming the bug it fixed. Contained today only by the allocator's phantom-frame rejects. |
| A10-9 | MED | §18.1 (H4) | aarch64 never calls `kernel_main`, so `harden_hhdm_nx` and `audit_wx` never run - and the high-half direct map is built EL1-RW with **no PXN**. Kernel W^X does not exist on this port, and nothing says so. |
| A10-10 | MED | VII, §18.3 | The **bring-up demo syscall range is live in the production build**: any EL0 task can `svc` with `x16 >= 0x1000` and reach handlers whose SAFETY comments assert "the demo is the only caller". |
| A10-11 | MED | VI, §18.3 | The console input ring has **three producers on three different cores** under a comment that says it has one. Non-atomic RMW of `RX_TAIL`. |
| A10-12 | MED | VIII, §26.6 | `mailbox::call()`'s reply loop **cannot reach its own bound** on one path - `spins` advances only inside an inner wait the path skips. |
| A10-13 | MED | VIII ("a count is not a duration") | Five hardware waits bounded by an iteration count, one of them on the **runtime** log path - `put_byte`, on every `kprintln` and on the panic path. |
| A10-14 | LOW | X, §26.4 | Doc-drift and dead references left by the `xhci.rs` deletion - six distinct stale claims, two of them mutually contradictory twenty lines apart. |
| A10-15 | LOW | §26.6 | `AllocMem` leaks the data frame when the mapping fails: neither freed nor in any page table, so death-time reclaim cannot find it. Ungated syscall; worst exactly under memory pressure. |
| A10-16 | LOW | X | Six `unused import` warnings, judged individually - five neutral and pre-existing, one a deliberate seam item. See the verdict below. |
| A10-17 | LOW | §26.4 | Three exception-vector doc blocks assert the **opposite** of the EL0-kill path added this cycle ("Deliberately does not return. Nothing here can handle a fault yet"). |
| A10-18 | LOW | III | `TASK_LAST_BADGE` and the pending-cap FIFO are per-task-**slot** state that survives the task. Same class as A10-4; fix together. |
| A10-19 | LOW | §26.4 | `kernel/src/syscall/CLAUDE.md` lists the ungated `InspectKernel` queries as 0, 3, 9-13; the code ungates 0, 3, 9-22. A doc that says "there are no exceptions" must enumerate the real set. |
| A10-20 | LOW (PLAUSIBLE) | Inv 12 | `gic::init()` never clears `GICD_ICENABLERn`, so an SPI the armstub left enabled fires into a generic arm that cannot mask what it has no route for - a level source would then re-enter forever. |

### The four HIGH, in full

**A10-1. A kernel panic on aarch64 does not stop the machine.** `arch/aarch64/mod.rs:1383` is
`pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }`. The neutral `#[panic_handler]`
prints `KERNEL PANIC` and calls it. Nothing is sent to the other three cores, so they keep scheduling
tasks over the state that just panicked; and because the loop does not `msr daifset`, a panic taken
with interrupts live - the boot path after `enable_interrupts`, or any kernel code resumed by
`block_and_reschedule`, which re-enables them - is **preemptible**: the timer PPI fires 10 ms later,
`timer_tick_from_irq` switches that core to another task, and the machine simply carries on past its
own panic. §19 requires the opposite in as many words ("the panic path broadcasts an NMI so **every**
core halts"), and §6.2 makes a panic the response to already-corrupted shared state. This is exactly
SEC-18, which was found HIGH on x86, fixed, and hardware-verified on the T630; x86's version is
`broadcast_nmi_all_but_self()` + `cli` + `hlt`. `exceptions.rs:689` (the EL1-fault park) is the same
class: one core stops, the rest run on. GICv2 has no NMI, so the fix is a halt SGI plus the DAIF mask.
**CONFIRMED** by reading both arches. Trigger: any `panic!` - the liveness watchdog, an allocator
invariant, a neutral assert - all runtime-reachable.

**A10-2. The user-copy fixup is per-core; the copy it protects is not.** `uaccess.rs` arms
`FIXUP[core_index()]` with a recovery label, runs a byte-at-a-time loop of up to 4096 iterations, and
disarms on exit. Its SAFETY comment (`uaccess.rs:127`) justifies the per-core slot with "interrupts do
not re-enter a user copy on the same core (a syscall runs with the caller's core to itself until it
returns)". **That claim is false.** Exception entry masks `DAIF.I`, but `block_and_reschedule` ends
`switch_context(...); enable_interrupts();` (`scheduler.rs:1620-1621`), so a syscall that blocked and
was woken runs the rest of itself with IRQs live - and the rest is where the copy is:
`handle_recv` reaches `write_user_bytes` at `dispatch.rs:344` *after* its
`block_and_reschedule(BlockedOnRecv)`, and `handle_call` the same at `:1103`. The interrupted ring is
not consulted: `timer_tick_from_irq`'s `_interrupted_cs` is unused on every arch, and aarch64 passes
`(0, 0, 0)` anyway. So the timer preempts mid-copy, the interposing task performs any user copy of its
own and stores `xzr` to the same per-core slot, and when the first task resumes and faults on a later
byte, `take_fixup()` returns `None`, `from_el0` is false, and the core parks in the `wfe` loop -
which the liveness watchdog then turns into a panic that (A10-1) does not halt anything.
Trigger: a service passes a `recv` buffer whose tail crosses into an unmapped page, blocks, is woken
with a full 4 KiB payload, and retries until a tick lands in the window - tens of microseconds out of
a 10 ms quantum, so a few thousand attempts. The mirror-image leak is live too: a task switched away
with the fixup still armed redirects the *next* genuine EL1 abort on that core into a recovery label
with unrelated register state, which is the opposite of the "a later kernel bug still halts loudly"
property the comment claims. **CONFIRMED** (race-dependent, attacker-repeatable). **Not purely
aarch64:** x86's `USER_COPY_ACTIVE` is per-core with the same assumption and the same post-block
window, and there the lost flag lands in `pf_handler`'s `KERNEL PF` arm and `halt_all_cores()` - which
on x86 really does halt the machine. The comment must go either way; the fix is to mask IRQs across
the copy, or key the fixup to the task rather than the core.

**A10-3. The block split is the unsound version, and the fix has been reverted twice.**
`unmap_high_4k` (`mmu.rs:676`) meets a live `DESC_BLOCK` in the high half, builds an L3 table
reproducing it, and at `:701` overwrites the **live** L2 block descriptor with a table descriptor.
The only TLB maintenance is a single `tlbi vmalle1is` at `:732`, after all 224 guard pages are done.
Between the store and that invalidate the TLB may hold the 2 MiB block entry and a freshly walked
4 KiB entry for the same VA. AArch64 requires break-before-make for a change of block size, and the
Cortex-A72 is Armv8.0 - no FEAT_BBM level 2 to excuse it - so the outcome is CONSTRAINED
UNPREDICTABLE: a TLB conflict abort at EL1 (which A10-1 turns into a machine that limps on past a
panic), or an amalgamated translation. There is a second consequence worth naming on its own: a stale
block entry keeps **translating the guard page**, so the guard is silently absent - and the check that
verifies it (`entry_for_va`) is a software walk of the tables, not of the TLB, so it reports
`guard_unmapped=true` either way. The trigger is every boot: `install_kstack_guards` runs at
`mod.rs:832`, after `mmu::enable()` and after `jump_high`, so the core is executing from the high
half, standing on a high-half stack, and repointing the blocks that describe its own `.bss`. `9e74cd46`
fixed this properly - pre-split the pool's blocks in `enable()` while the MMU is off, then refuse and
report a live block at guard time - and was reverted (`ee50f6cb`), reapplied (`b15a1277`), and
reverted again (`40d4a4a4`). **Both reverts are bare `git revert` messages with no rationale**, so
nothing in the history says whether the fix broke a boot or was undone by accident. That is the part
to fix first: a correctness fix removed without a recorded reason is indistinguishable from one nobody
decided about. Note the naive repair is also wrong (a break-before-make on the block you are standing
on is fatal), which is why the pre-split shape is the one that works. **CONFIRMED** code state and
architectural violation; the abort itself is **PLAUSIBLE**, not observed.

**A10-4. The pending-received-cap FIFO cannot say which message a cap came from - and this is very
likely the open `fcap` escalation.** `TASK_PENDING_RECV_CAPS` / `..._COUNT` (`scheduler.rs:228-231`)
are per-task-slot statics with exactly two writers: `push_pending_recv_cap` and
`pop_pending_recv_cap`. Nothing clears them - not `reserve_task_slot`, not `commit_task`, not the kill
path - and no recv path drains them. Meanwhile all four delivery sites (`handle_recv`,
`handle_try_recv`, `handle_recv_timeout`, `handle_call`) install **every** embedded cap of **every**
delivered message into the receiver's table and push its slot, whether or not the receiver wanted the
message. So a receiver that *discards* a message keeps the cap and leaves a slot number at the head of
the FIFO forever, and `TakePendingCap` has no way to ask for "the cap from *this* message".

The concrete trigger is the one the branch's own `fcap-diag` instrumentation was added to chase.
`services/shell/src/main.rs` opens both `fc_invoke` and `sock_invoke` with
`while ctx.try_recv().is_some() {}` - "clear any stale late-reply a prior aborted invoke left behind".
An `fs` `Open` reply **embeds the file capability** (`fs/src/main.rs:2650`). After `chaos max-carnage`
restarts `fs`, an aborted or timed-out open leaves its reply in flight; the next command's drain loop
discards that message - but the kernel has already installed its file cap in the shell's table and
pushed the slot. The next `fc_open` (`shell:8447`) then pops that **stale head** instead of the cap
from its own reply, so an `open(path, READ)` hands back the handle of an earlier `open(path,
READ|WRITE)`. The kernel then does exactly the right thing and reports WRITE for a cap the shell
believes is read-only, and the write succeeds. That is precisely the reported symptom - `fcap` printing
"ro cap wrote (escalation!)" only after a restart - and it matches the `[fcapr]` predicate written into
`e4118dc3` for suspect 1 ("shell `holds` disagrees with kernel `cap_rights` for the same handle").

Two things make this the kernel's finding rather than the shell's. First, **nothing above the kernel
can resynchronise**: `try_recv` does not report that a cap rode along, and `RemoveCap` clears the table
entry but not the FIFO, so no userspace discipline fully closes it. Second, the enforcement path is
sound and was verified end to end - `handle_resource_invoke` validates the requested right against the
held cap first, `is_delegated` is a pure band range check, the badge is set in exactly one place after
validation, `narrow_embedded_for_receiver` strips GRANT from delegated caps, and `allocate`
re-registers a reused band id at `prev_gen.bump()`. There is no rights bug to find; the defect is that
a *handle* stopped naming the cap its holder thinks it names. **CONFIRMED mechanism; root cause
STRONGLY INDICATED but not executed** - the audit did not run the `fcap` -> `kill fs` -> `fcap` repro.
That repro, with `fcap-diag` on, is the cheap way to convert this from indicated to proven.

### The MED findings, in brief

- **A10-5.** All four delivery sites use `if let Ok(new_slot) = current_task_insert_cap(..)`, so a
  receiver whose 64-slot table is full loses the cap entirely - while the sender has already had it
  removed (§8.5) and was told `Ok`. Authority vanishes with neither side informed, which is the silent
  fallback §26.7 forbids and the setup adversarial test A6 describes. One layer down,
  `push_pending_recv_cap` (`scheduler.rs:743-747`) drops the slot **number** when its 4-entry buffer is
  full while the cap stays installed, leaving an untakeable, unreclaimable table slot. Reachability is
  bounded rather than open: `AcquireSendCap` hands `GRANT` only to `ACQUIRE_ANY` holders (shell,
  supervisor, probes, chaos), so an ordinary service cannot mint a grantable cap to spray at a victim -
  and `fs` drains one cap per message, so it does not overflow in ordinary use. Constructible from a
  privileged instrument against a non-draining receiver (`logger`, `supervisor`); graded MED for that
  reason, not downgraded, because the failure is silent either way.
- **A10-6.** `spawn_service_with_config` registers a route for every `hw_irqs` entry;
  `scheduler.rs:1910-1914` unregisters by `match task_name { "xhci" => 0x28, "ehci" => 0x29, _ => 0xFF }`,
  and the comment directly above it - "block-driver + nic-driver currently declare no hw_irq" - was made
  false by this branch (`task/mod.rs:750-754`). `free_endpoint_id` then recycles the id immediately.
  `interrupt/route.rs:49-55` documents this exact hazard as the reason `unregister` exists. Not a wedge
  (delivery masks the level line first, and only the registered endpoint may `IrqUnmask`), but GENET
  interrupt messages land in a service holding no `hw_interrupt` capability. Found independently by two
  auditors. Fix: drive `dead_irq` off the service's own `hw_irqs`, not a name match.
- **A10-7.** `ptables::unmap` carries the correct A7-4 sequence (`dsb ishst`, `tlbi vaae1is`, `dsb ish`,
  `isb`, then construct the `Frame`) and the operand form is right for `pi4-smp`. It has exactly one
  caller in the kernel: `mod.rs:709`, inside `ptable_selftest`. The runtime paths are `reclaim_pages`
  and `free_all`, neither of which invalidates. The guard is real, correct, and **not on the road** -
  the trap this audit was told to watch for. What actually closes the hazard is
  `free_page_table_root`'s broadcast `vmalle1is` before `free_all` (added for the respawn fault, and
  correct), so this is a MED gap rather than a live UAF - except on the **self-kill** path, where
  `reclaim_user_frames` frees every leaf inline while `free_page_table_root` (and therefore the
  broadcast) is deferred to `drain_pending_pml4`. Related and worth fixing in the same pass:
  `reclaim_user_frames`'s `# Safety` says "with `TTBR0_EL1` switched away and invalidated", which the
  self-kill caller does not satisfy - §18.3 requires the comment to be true.
- **A10-8.** `mod.rs:1202-1234` maps `nic-driver`'s GENET window with `PageFlags::PCD`, and
  `DMA_ARENA_UNCACHED = true` gives the DMA arena the same. `ptables::reclaim_pages` frees every valid
  L3 leaf unconditionally; `x86_64/page_tables.rs:598-627` explicitly skips `PWT|PCD` leaves, with a
  comment naming the chaos double-free it fixed. Traced and **contained today** - `allocator::free`
  rejects the 0xFD58_0000 pages as phantom frames and the arena pages via the permanent DMA reserve -
  so this is defence in depth doing load-bearing work, an over-reported `freed` count, and 16
  `IGNORED phantom frame` lines per `nic-driver` kill that read like corruption during a chaos run.
  arm32 had the identical gap and it was Audit 8's A8-2 HIGH; the difference here is that the SoC's
  device window sits above top-of-RAM, which is luck, not construction.
- **A10-9.** `kernel_main` has exactly one caller in the tree, `arch/x86_64/mod.rs:107`. So on aarch64
  the neutral boot body never runs, and with it neither `harden_hhdm_nx` nor `audit_wx`. Independently,
  `mmu.rs:317-318` builds every high-half RAM block `DESC_AP_RW_EL1 | DESC_UXN` - UXN but **no PXN** -
  so all of physical RAM, including every user data frame and the kernel's own `.text`, is readable,
  writable and executable at EL1 through the direct map. The `harden_hhdm_nx` stub is the established
  shape on all six non-x86 arches, so it is not an aarch64 regression; the missing PXN is. The low map
  carries a "W^X is a later refinement" note and the high map - now the only map - carries none, which
  is the silent-stub class this port has already been bitten by twice.
- **A10-10.** `usermode` is compiled under plain `feature = "pi4"` (unlike `sched_user` / `sched_demo` /
  `sched_spawn`, which are demo-gated), and `aarch64_sync_lower_dispatch:575-578` routes every
  `nr >= BRINGUP_SYSCALL_BASE` (0x1000) to `usermode::syscall` with no gate. Any EL0 task can therefore
  reach handlers whose SAFETY comments read "single-threaded bring-up; the demo is the only caller"
  (`usermode.rs:150,162,175,189`). In the shipping build the callers are arbitrary tasks on four cores,
  so `CALLS`, `ECHO_OK`, `VERDICT_OK`, `REAL_DISPATCH_OK` are non-atomic `static mut` writes racing
  across cores, and every call - including the unknown arm - drives a `put_str` line out of the PL011
  with interrupts masked, where the neutral dispatcher would have returned `-1` silently. The one arm
  that would be dangerous, `SYS_EXIT` (it resumes `CTX_KERNEL`), **is** correctly `DEMO_ACTIVE`-gated
  and `run()` clears the flag on exit - verified, so the guard holds. The residual is the reachability
  and the false safety claims.
- **A10-11.** `uart_rx::drain` masks interrupts and states it "runs on the core that owns the ring, so
  there is nothing to contend with". There are three producers: `uart_rx_poll` on core 0's tick,
  `uart_rx_drain_now` from the ConsoleRead syscall on the *calling task's* core, and `push` from the
  `xhci` service's core via `CONSOLE_PUSH` - with no masking at all. `without_interrupts` excludes only
  the local core. All three do a non-atomic read-modify-write of `RX_TAIL`, so two producers can write
  different bytes to the same slot and store the same `next`, and the tail can move backwards. Every
  index is `% RX_BUF_SIZE`, so this is bounded corruption (a lost keystroke), not a wedge - but it is a
  real `static mut` race whose justification comment is false, and **the exposure was increased by the
  work under audit**: the keyboard producer used to be the same core-0 tick as the drain, and is now a
  syscall from a userspace service on another core.
- **A10-12.** `mailbox.rs:102-115` increments `spins` only inside the inner
  `while status & MBOX_EMPTY != 0` wait. If the status register reads with EMPTY clear, that inner loop
  is skipped entirely, `MBOX_READ` is read, the channel does not match, and the outer loop repeats -
  forever, with `spins` never advancing and `MBOX_SPINS` structurally unable to fire. A mailbox absent,
  held in reset, or reading back zero presents exactly this (status 0 = not empty, read 0 = channel 0).
  The machine hangs inside `mailbox::query()` before the memory map, the framebuffer, or one further
  log line. Boot-only and not reachable on a healthy Pi 4 - but it is a device-supplied register value
  deciding when kernel code stops, with the bound unable to reach it.
- **A10-13.** The repo's standing lesson is that a count is not a duration, and there are five here.
  The one that matters is **runtime**: `put_byte` (`mod.rs:483-494`) spins up to 1,000,000 times per
  byte waiting for TXFF - on every `kprintln`, every console write, and the panic path - so a UART that
  stops draining turns each ~200-byte line into a stall of a length that differs on every machine, and
  the liveness deadline is measured in real time. The other four are boot-only and less severe:
  `pl011_init`'s 1,000,000-iteration BUSY wait, the 500,000,000-iteration timer proof, `MBOX_SPINS`,
  and `hardware_reset`'s `n == 2_000_000` diagnostic threshold (which decides when the operator is told
  the SoC did not reset). `pcie::delay_us` is the in-tree model that does it right: microseconds
  through `timer::frequency()`, compared against `read_cycle_counter()`.
  *(An auditor also reported `smp_boot`'s waits as count-bounded; re-verified and rejected -
  `start_secondaries` and `report_cores_up` both bound on `read_cycle_counter()` against
  `timer::frequency()`. They are clocks, and they are the pattern the five above should copy.)*

### The LOW findings

- **A10-14 (doc-drift from the deletion).** `mod.rs:1711-1714` and `sched_supervisor.rs:16-19` both say
  "`xhci` is a placeholder that never spawns" - the supervisor now spawns it on every arch but arm32
  (`e71e64a6`), so a reader is told the input-ready flag exists for a service that does not run.
  `docs/aarch64.md:568` repeats it. `arch/mod.rs:9` says `hid` is shared with `arch/aarch64/xhci.rs`,
  which no longer exists (and `hid` is now imported only by `arch/arm`). `wait_for_interrupt`'s doc
  (`mod.rs:2184-2196`) still describes watching USB hub ports "from HERE" with 100 ms enumerations and
  one-visit-per-second rate limiting; the body is a bare `wfi`. `mod.rs:1272` says the service is
  "HID-only" while `mod.rs:950`, in the same file, prints that it drives HID *and* mass storage.
  `mod.rs:2349-2353` says the only interrupt routed to userspace is an edge-triggered MSI needing no
  masking, contradicted by `mod.rs:904` registering GENET level-triggered. And `mod.rs:470` / `:1075`
  still carry "arch/aarch64 stubs pending real bodies. halting." as unreachable statements (the
  compiler says so - two `unreachable statement` warnings).
- **A10-15.** `dispatch.rs:1305-1321`: `alloc_frame()` succeeds, `zero_frame` runs, `map_in_active_tables`
  fails and the handler returns -1 - the verdict *is* checked, but the frame is never returned to the
  allocator and never entered a page table, so `reclaim_pages` cannot find it at task death. `AllocMem`
  needs no capability by design, and the failure that reaches this is allocator exhaustion, so a service
  retrying under pressure ratchets the pool down one frame per attempt. Neutral code; x86 identical.
- **A10-16 (the six `unused import` warnings - judged, one by one).** Measured at HEAD: there are
  **six**, and **only one is in `arch/aarch64/mod.rs`**. The note in `f718e5a1` ("8 pre-existing
  unused-import warnings in `arch/aarch64/mod.rs`, most likely fallout from deleting the in-kernel
  driver") is wrong on the count, the location, and the cause: the other five are in neutral files and
  appear **identically on the x86 build**, verified by building both. Verdicts:
  - `task/mod.rs:18` `use crate::capability::generation::Generation` - a **private** import with no
    API-surface argument at all. **Delete.**
  - `ipc/mod.rs:13` `Endpoint` and `task/mod.rs:8` `Task` / `TaskId` - not merely unused re-exports;
    the build also reports `struct Endpoint is never constructed` and `struct Task is never
    constructed`. These are **dead types**, and that is the finding worth acting on: a reader looking
    for "the task struct" finds one nothing builds, because the live task state is the parallel arrays
    in `scheduler.rs`. **Delete the types or say in `task/CLAUDE.md` why they are kept**; deleting the
    re-export alone hides the more interesting fact.
  - `ipc/mod.rs:14` `IpcError` - the type is alive but every consumer names
    `crate::ipc::message::IpcError`. The kernel is a **bin** crate, so `pub use` has no external
    consumer and this is a facade nobody walks through. **Delete**, or switch one call site to it.
  - `capability/mod.rs:17` `CapTable` - same shape; `main.rs` names `capability::table::CapTable`
    directly. **Delete from the re-export list**; the type stays public in `table`.
  - `arch/aarch64/mod.rs:2254` `pub use crate::clock::epoch_secs` - **KEEP.** This is the only
    arch-local one and it is **deliberate seam surface**: `arch/x86_64/rtc.rs:225` carries the identical
    re-export and *uses* it, and all six other arches carry it in their `rtc` stub. Deleting it would
    make aarch64 the one arch whose rtc surface differs, which is the opposite of what the seam is for.
    Silence it with `#[allow(unused_imports)]` and a one-line note saying it is the shared surface.

  The point of the exercise: five of these are noise that trains readers to ignore warnings, and one is
  load-bearing. `cargo fix` would have gotten the last one wrong.
- **A10-17.** `exceptions.rs:28-31`, `:557-559` and `:588-592` all state that a fault reports and halts
  and that there is no task to kill - false since the EL0-kill path at `:655-684` (which is a genuinely
  good change: §10.4 kills the service, not the machine). An auditor reading the fault model from the
  doc comments would conclude an EL0 fault is fatal. Same class as A8-22.
- **A10-18.** `TASK_LAST_BADGE` (`scheduler.rs:235`) and the A10-4 FIFO are per-task-slot statics that
  no spawn or kill path clears, so a slot's next occupant inherits them. The badge is self-healing in
  practice (every recv overwrites it, an unbadged message writes 0); the FIFO is not. Fix together.
- **A10-19.** `dispatch.rs:1345` ungates `InspectKernel` queries 0, 3 and 9-22; the doc lists 0, 3,
  9-13. Each addition is individually argued in a comment and none confers authority - but the doc is
  the one that says "there are no exceptions to this rule".
- **A10-20 (PLAUSIBLE).** `gic.rs:81-95` writes `GICD_CTLR`, `GICC_CTLR` and `GICC_PMR` and nothing
  else - no `GICD_ICENABLERn` clearing, which is what Linux's `gic_dist_init` does and why. The Pi 4's
  armstub configures the GIC before handover (this file's own comment says so). An SPI it left enabled
  fires at `timer::unmask_irqs()`, lands in the generic `id >= 32` arm, and reaches
  `route::deliver` -> `mask_vector` -> `lookup()` returns `None` -> nothing is masked -> EOI -> a level
  source re-enters immediately. That is an IRQ-storm livelock which the watchdog converts into A10-1.
  Eighteen milestones have booted without it, so PLAUSIBLE, not confirmed.

### Classes swept CLEAN (stated because they are the point of auditing)

- **The delegated-resource (file-capability) enforcement path is sound, end to end.** Right validated
  before anything else; `is_delegated` a pure band range check; the badge written in exactly one place
  after validation and zeroed by `Message::new`/`interrupt_event`, so it cannot be forged over an
  ordinary send; `narrow_embedded_for_receiver` strips GRANT from delegated caps; `allocate`
  re-registers a reused band id at `prev_gen.bump()`, closing ABA. **The owner-recycling hazard is
  closed by construction**: `release_owner` runs on endpoint death *before* `free_endpoint_id`
  (`scheduler.rs:1881` then `:1928`), and `mark_dead_resource` bumps the generation, so a stale file
  cap fails its next use. The only residual is the TOCTOU between `owner_of` and `enqueue` (recorded,
  needs a concurrent death plus a spawn inside a few instructions - not graded).
- **Capability validation: 49 handlers, 39 validate a cap first** (23 by slot, 16 by holdings), 2 by
  kernel-recorded ownership (`ResourceRevoke`, `IrqUnmask` - both justified), and 8 require none,
  each touching only the caller's own state. In every one of the 39 the validation is the first thing
  the body does. `handle_console_foreground` was checked specifically because this branch made it push
  a console byte: it is gated on `CONSOLE_READ` **and** scope-checked against `CONSOLE_READ_RESOURCE`,
  so the injected `\n` stays inside the console trust perimeter SEC-2 already defines.
- **User pointers.** aarch64's seam is stronger than x86's, not weaker: a range check that rejects 0,
  wraps and anything at or above `USER_END`; `ldtrb`/`sttrb`, the *unprivileged* load and store, so
  the MMU applies EL0 permissions even though the code runs at EL1; and a fault fixup whose selftest
  proves it **fired** (a `recovered_count` delta) rather than that the pointer was refused upstream.
  A bad pointer returns a defined error instead of killing the caller. A10-2 is the one crack in it.
- **Arithmetic and indexing in the syscall layer.** Every cap slot goes through `CapTable::get`/`remove`
  (`slots.get(slot)`); `task_stat`, `for_each_cap_of`, the per-core counters and `set_wake_deadline`
  all bound against `MAX_TASKS`/`num_cores()`; `handle_spawn_with_caps`'s descriptor parser
  bounds-checks at each of five steps (the `p + label_len + 2 > len` guard was walked against the two
  reads that follow); `current_task_claim_alloc` uses `checked_add`.
- **Frame-bitmap OOB from a corrupt or device-influenced PTE.** `free()` double-bounds on
  `max_valid_frame` **and** `max_ram_frame`; `memmap::build` clamps every bank to `IDENTITY_MAP_LIMIT`,
  so the two numbers genuinely agree. A leaf naming GENET or the PCIe window is rejected as a phantom
  frame rather than corrupting the bitmap.
- **Device-supplied values.** `video::init` range-checks GPU-reported `w`/`h`/`pitch` before they
  become a slice length, and every framebuffer write goes through `render::put_run` (`checked_add` +
  `get_mut`), so an inconsistent geometry drops pixels rather than panicking. `pcie`'s capability walk
  masks every device-derived offset to 0xFFF and is bounded at 48 hops. The DTB walk is
  `MAX_TOKENS`-bounded with every read range-checked against the header's `totalsize`.
- **The A7-5 class (an unbounded drain in an ISR) is gone with the driver.** The timer tick's only
  drain is `uart_rx::drain`, capped at 64 for a 32-entry FIFO. Both routed sources are storm-proof:
  `msi_take_pending` clears whatever is pending whether or not a service is listening, and GENET is
  masked at the distributor before delivery because it is registered level-triggered.
- **Block-attribute carry in the split** (the mechanism A10-3 is about is *correct*, only its ordering
  is not): `ATTR_LOW | ATTR_HIGH` reproduces every bit `enable()` sets, `ADDR_MASK_2M` is bits [47:21],
  and the `0b01`-at-L2 vs `0b11`-at-L3 encoding difference is handled correctly in both
  `unmap_high_4k` and `entry_for_va`. The replacement pages are byte-for-byte the same translation.
- **TLBI operand and shareability** are right everywhere they appear: `vaae1is` with `VA >> 12` for a
  page, `vmalle1is` for whole-map, each with `dsb ishst` before and `dsb ish` + `isb` after. Audit 7's
  A7-2/A7-3 publication barriers are present and correctly placed.
- **`smp_boot.rs` and `genet.rs` came back clean.** Every wait in `smp_boot` is bounded by the cycle
  counter against `timer::frequency()`; the coherency (`SMPEN`) and cache-maintenance reasoning is
  correct and the "boot core only, before the APs" precondition it relies on is **established**
  (`install_kstack_guards` at `mod.rs:832`, `start_secondaries` at `:843`), not assumed. `genet.rs`
  after the driver deletion is one `unsafe`, a `probe_read32` that survives an external abort, correct
  Release/Acquire on `GENET_PRESENT`, and no dead code.
- **Mechanical guards all PASS:** `unsafe_check` (72 files, 1095 lines, no unaccounted additions),
  `arch_boundary_check`, `contract_check`, `dash_check`. Kernel builds clean on
  `aarch64-unknown-none --features pi4,pi4-smp --release` and `x86_64-unknown-none --release`, both
  with and without `fcap-diag`.

### NOT audited (honest coverage)

- **The `fcap` repro was not run.** A10-4 names a root cause from source; it is not proven until
  `fcap` -> `kill fs` -> `fcap` is executed with `fcap-diag` on and the shell/kernel lines are compared.
  That is one boot, and it is the single highest-value thing to do next.
- **No hardware run.** Everything here is source verification and two clean builds. A10-3's abort and
  A10-2's race are both timing-dependent and neither was reproduced.
- **`services/xhci` was not audited.** 2742 lines left the kernel and became ~3600 lines of service;
  they are out of a *kernel* audit's scope by construction, but nobody should read this audit as
  coverage of them. That is a userspace-audit task, and the code is new.
- **Audit 7's fourth open item is still open**: the suspicion that the BOT class reset and clear-halts
  are refused over EP0 because the scratch control ring is shared with enumeration. It moved to
  userspace with the driver rather than being resolved.

### Process note

Audit 7 closed with the observation that all five of its findings were in code written the same day,
and that the standing rule is to audit before presenting for a hardware test. This pass supports the
same conclusion from the other direction: **A10-6 was created by this branch** (a new `hw_irqs` grant
whose teardown is a name match nobody updated, with the now-false comment sitting directly above the
code), **A10-11 was made worse by it** (moving the keyboard to userspace added a third producer to a
ring documented as single-producer), and **A10-3 is a fix that was written correctly and then reverted
twice with no recorded reason**. None of the three needed new insight to catch - each is visible in a
diff read against the comment immediately above it.

## Audit 9 - the shared framebuffer console + a full-kernel sweep (2026-08-03, `main` @ v0.9.0)

Scope: the ~1400 lines of `kernel/src/fbcon/` and the ARM input path shipped in v0.9.0 with no audit
pass, plus a sweep of the whole kernel for the classes the Commandments name.

**Verdict: 0 Commandment violations in the new code. 2 pre-existing defects found in the x86 page-table
and MMIO-mapping path, both of the same shape. Both FIXED (2026-08-03, `fix/mmio-map-verdicts`).**

> **Fix (A9-1, A9-2).** `walk_or_alloc` now returns `Err(AlreadyMapped)` when it meets a large-page
> entry instead of walking into the mapped data, and both MMIO call sites check the verdict. Because
> `AlreadyMapped` means the address IS usable (just not with our flags), the call sites distinguish it
> from a genuine failure with `entry_for_va`: an unmapped address makes `ioapic::init` return (leaving
> `IOAPIC_VA` 0, so `set_redir` no-ops) and `program_msix` return `false` (so the caller falls back to
> INTx), while an already-mapped one proceeds with a loud note that the uncached PCD|PWT flags were not
> applied. The SAFETY comments now describe what the code establishes rather than what it assumed.
> Verified: identity 24/24, and a boot still reports `ioapic: mapped at 0xfec00000 (ver=0x20, 24
> redirection entries)` plus `pci: MSI-X enabled` - the new branches are inert on a healthy machine.
>
> **Silicon coverage (2026-08-03, after v0.9.1 shipped).** The release notes recorded that
> `program_msix` had not been reached on hardware. It has now. `program_msi` is tried first and
> short-circuits the `||`, so `program_msix` only runs on a device with MSI-X *and no MSI* - which no
> machine here has, so the path was QEMU-only. A throwaway build with the order flipped
> (`diag/msix-first`, deleted) reached it on the T630: `pci: MSI-X enabled on 00:10.0 vector=0x28
> bir=0 tbl@0xfeb69000` - a real physical address, not QEMU's `0xc000003000`. The same device reported
> `MSI enabled` under the normal order, confirming it supports both and the short-circuit was hiding
> MSI-X. USB stayed healthy (HID keyboard enumerated, xHCI 8 ports) and `selfcheck` was 349/0, so the
> interrupt delivers rather than merely programming. The EHCI on the same box fell back to
> `legacy INTx routed via IOAPIC`, exercising the `ioapic.rs` half in the same boot.
>
> **Second machine (Wyse 5070, Intel).** The shipped v0.9.1 image was then run on a different vendor
> and chipset: `ioapic: mapped at 0xfec00000 (ver=0x20, 120 redirection entries)` - a table five times
> the T630's 24 - with `selfcheck` 349/0, no panics, and none of the new branches firing. Its xHCI
> reports `MSI enabled on 00:15.0`, so it has MSI and would have fallen straight through the flipped
> order too, which is why the diagnostic was not repeated here. Coverage for these fixes is therefore
> two vendors, two IOAPIC geometries, and both font scales (1x and 3x).
>
> **The A9-2 error paths are now EXERCISED, not merely reasoned about** (2026-08-03,
> `mmio-map-fault-test`). `FrameAllocFailed` cannot be reached by arranging real conditions - the
> allocator is not exhaustible at boot - so a cargo feature forces it at both call sites, following the
> `arm-fault-test` / `iommu-fault-test` precedent. Commandment IX: *if recovery cannot be tested, it
> does not exist*. Off by default; `KERNEL_FEATURES=mmio-map-fault-test` turns it on. Results:
>
> - **`ioapic`:** `MMIO map FAILED (FrameAllocFailed) for 0xfec00000 - no interrupt routing on this
>   machine`, and the kernel **survived** - reached `gsh>` and ran a full-screen editor, zero panics.
>   Before the fix this same condition read an unmapped VA in ring 0.
> - **`pci`:** took the *already-mapped* branch and continued, MSI-X programmed successfully, USB
>   working. Graceful on both sides of the decision.
>
> **A claim in the Audit 9 write-up was wrong, and this is the correction.** The `AlreadyMapped`-style
> branch was described as structurally unreachable, reasoning from the comment at `pci.rs` that
> "Limine's HHDM covers RAM but not MMIO". Under injection `entry_for_va` found the MSI-X table's HHDM
> alias **already mapped** - so the HHDM *does* cover that MMIO here. The conclusion survives but for a
> different reason: a normal (uninjected) boot maps it with no warning, which means the walk reaches a
> present 4 KiB page table rather than a large-page entry, so **A9-1's large-page case is still
> unobserved** - not because the page is absent, but because it is mapped at 4 KiB granularity.
>
> **A9-1 is now CLOSED too - the bug is reproduced, not merely guarded against**
> (`selftest_large_page_guard`, same feature). No machine here produces the trigger, so the trigger is
> manufactured: the test installs a real 2 MiB `PS=1` entry over an unused canonical VA and calls the
> real `map_in_active_tables` inside it, so the guard's own detection is what is under test rather than
> a mocked return. It then checks the property that actually mattered - that the mapped frame is
> **byte-for-byte untouched**.
>
> **Both directions verified**, because a test that has never failed proves nothing:
> - guard present: `pt-selftest: large-page guard PASS (walk refused, mapped frame untouched)`
> - guard removed: `pt-selftest: CORRUPTION at u64 index 1 - the guard did not hold`, at exactly the
>   index predicted (`pt_idx(TEST_VA + 0x1000) == 1`). That is the A9-1 defect, executed.
>
> **The failure run also exposed a flaw in the test itself.** The first sentinel was odd, and
> `map_in_active_tables` only writes a PT entry whose existing value reads as not-present (bit 0
> clear) - so the corrupting write was skipped and the frame stayed intact for the *wrong reason*. The
> sentinel is now even, and the comment says why. Had the guard-removed run not been done, a test that
> could pass without the fix would have been recorded as proof of it.

| ID | Severity | Commandment | Finding |
|----|----------|-------------|---------|
| A9-1 | MED **(FIXED)** | Invariant 12, X | `walk_or_alloc` (`arch/x86_64/page_tables.rs:492`) does **not check `PAGE_SIZE_BIT`**. `PAGE_SIZE_BIT` is tested only in the read-only walk (lines 325/331). So if `map_in_active_tables` is ever called on a VA already covered by a 2 MiB or 1 GiB page, the walk treats that large page's **data frame** as a page table and `read_entry`/`write_entry` at `pt_idx(virt)` reads and writes 8 bytes *into that data*. Silent memory corruption, no error. It does not fire today only because both call sites target MMIO holes (IOAPIC, MSI-X BAR) that Limine's HHDM leaves unmapped, so the PD entry is absent and a fresh PT is allocated - **correct by accident, not by construction**, and nothing checks or documents the assumption. |
| A9-2 | MED **(FIXED)** | V, §18.3 | Two call sites discard the mapping verdict and then immediately dereference the address: `ioapic.rs:68` (`let _ = map_in_active_tables(...)` then `unsafe { read(va, 0x01) }`) and `pci.rs:624` (same, then `write_volatile` to the MSI-X table). `map_in_active_tables` returns `Err(FrameAllocFailed)` when the allocator cannot supply a page-table frame. Both dereferences carry a SAFETY comment asserting the page was "just mapped" - an assertion the code throws away the evidence for. §18.3 requires a SAFETY comment to be *true*; here it is an assumption. |
| A9-3 | LOW | V | `handle_remove_cap` (`syscall/dispatch.rs:1476`) returns `0` unconditionally, discarding whether the slot was actually held. A caller cannot distinguish "removed" from "there was nothing there". |
| A9-4 | LOW (doc) | VII | `handle_resource_revoke` authorises by **ownership** (`revoke_owned(id, caller_endpoint)`), which is a *third* validation form. `kernel/src/syscall/CLAUDE.md` documents exactly two (cap-slot and holdings) and says "There are no exceptions to this rule". The code is **correct** - ownership is established only through the `RESOURCE_MINT`-gated `resource_mint`, so authority does derive from a capability and a task that never minted owns nothing - but an undocumented third form sits close enough to "authority by identity" (which VII forbids outright) that it needs to be written down rather than inferred. |

**Clean results, stated because they are the point of auditing:**

- **`kernel/src/fbcon/` (new, ~1400 lines): no discarded verdicts, no unbounded loops, no `unsafe`.**
  The neutral console is `unsafe`-free by construction - the arch hands the framebuffer over as a
  `&'static mut [u8]`, so `fbcon/` needed no §18.1 amendment to exist outside the four permitted layers.
- **Every hardware wait in `arch/` is bounded.** 15 candidate `while`-on-MMIO loops were checked
  individually; all carry an in-body counter or a real-time deadline and a loud report on expiry
  (`dwc2` reset/port-enable, PL011 TX, the BCM2835 RNG, the GPU mailbox, x86 `apic_wait_icr_idle`).
  Invariant 12 and Commandment VIII hold across the layer.
- **46 syscall handlers; 41 validate a capability explicitly.** The 5 that do not are each justified:
  `sleep` and `alloc_mem` are task-neutral (documented in the syscall table), `remove_cap` and
  `take_pending_cap` act only on the caller's own table, and `resource_revoke` is A9-4 above.
- **The shadow grid is not a second truth (III).** It is the source for repaint and the framebuffer is
  its derived view; the cursor underline is the one thing painted outside it, and it is provably erased
  before every scroll (`advance_line` is only reached from paths that call `cursor_off` first).
- **The two `READY` flags (neutral `fbcon` and `arch/arm/fbcon.rs`) are a deliberate, documented
  duplication**, not a III violation: the ARM copy exists because the pre-MMU serial path must touch
  nothing but a plain load, and it reduces to the same event (`crate::fbcon::init` returning).

## Audit 8 - the liveness watchdog, the MMIO reclaim, and the idle contract (2026-07-31, `feat/arm-usb-interrupt`)

**Scope:** `d8de0f2..HEAD` - the per-arch `liveness_deadline_cycles`, the arm32 device-MMIO reclaim
skip, the BSP idle-halt fix, and the scheduler idle path. Audited against the Ten Commandments.

**Verdict: 0 new violations. 2 real defects were FOUND and FIXED by this work (both pre-existing).**

| ID | Sev | Commandment | Finding |
|----|-----|-------------|---------|
| A8-1 | HIGH (pre-existing, FIXED) | V, IX, Invariant 12 | The BSP was excluded from `rearm_idle_timer` (it must keep driving MONOTONIC_TICKS/scan_timed_wakes/COM at 100 Hz) and then allowed to `hlt` regardless. In TSC-Deadline mode the timer is ONE-SHOT, so it halted onto a deadline already in flight; consumed, the core never woke. `LIVENESS WEDGE: core 0 ... slot 224` (224 = IDLE) ~5 s after boot on the T630. Latent for the life of the port - userspace spin-yielded, so the BSP never reached idle. FIXED @ 2b898c5: **never halt without a freshly armed wake**. |
| A8-2 | HIGH (pre-existing, FIXED) | I, III | `reclaim_user_frames` (arm32) freed every PL0 leaf as "ours", including a driver's device MMIO mapping - 141 occurrences per 100 chaos rounds at phys 0x3F300000 (the Pi's EMMC window). Contained only by a DOWNSTREAM bounds check that happens to reject it on this SoC; a device mapped below top-of-RAM would have entered the free pool and been handed to a service as heap. FIXED @ 0ce48f6 by skipping on the PTE's own memory type. x86 already had the equivalent rule (PCD|PWT); the ARM port never carried it over. |
| A8-3 | note | I, X | `liveness_deadline_cycles` moves the watchdog's deadline BEHIND the arch seam. This is complexity in the right place (§26.10): only the arch knows both its counter and its rate, and the previous neutral derivation silently disabled the whole check on any arch whose quantum figure is a stub. No new kernel responsibility - the same check, correctly parameterised. |
| A8-4 | note | VI, Invariant 9 | Three new statics (`LATCH_CLEAR_FAILED`, `RECLAIM_DEVICE_LOGGED`, `LIVENESS_ARMED`). All are single-owner say-once flags in the layer that owns the event; none is read across a boundary or carries state another component reasons about. |
| A8-5 | note | VIII | The new scheduler call site arms a timer before halting - that is arming a WAKE, not waiting on a clock for correctness. Correctness still comes from the wake itself. |
| A8-6 | note | cross-arch | The new `rearm_quantum_timer` / `idle_can_halt` call sites resolve on all 7 arches (verified); arch_boundary_check passes, so the neutral scheduler still reaches hardware only through `arch::imp`. |

**Standing:** unsafe_check (854 lines, no unaccounted additions), arch_boundary_check, identity 24/24.

## Audit 7 - the ARM USB hot-plug + storage-failure chain (2026-07-30, `feat/arm-usb-interrupt`)

**Scope:** every change from `fba9081`..`d8de0f2` - the hot-plug watcher rewrite (level -> latch),
the absent-vs-busy status split, the block-refusal noise budget, and the ARM `usb_disk_absent`
primitive. Audited against the Ten Commandments.

**Verdict: 1 HIGH, 1 LOW, both fixed; 1 accepted deviation recorded.**

| ID | Sev | Commandment | Finding |
|----|-----|-------------|---------|
| A7-1 | HIGH | V, §26.6 | `hotplug_check_port` discarded the verdict of `control_out(CLEAR_FEATURE, C_PORT_CONNECTION)`. A latch that fails to clear stays set, so **every** later visit believes the port just changed: an occupied port would be stood down and fully re-enumerated **once a second, forever**. Exactly the unbounded retry the `PortBringUp::Failed` bound prevents, re-entered through the new latch path - and the **fourth** discarded-verdict defect in this driver. FIXED: the clear is checked; on failure the event is not consumed (an event we cannot acknowledge is one we cannot safely act on), reported once, and re-read next visit. |
| A7-2 | LOW | I, §26.6 | `HOTPLUG_TRIES[port as usize]` is indexed without a bound inside `hotplug_check_port`. Both current callers clamp to `1..=31` and the array is 32 wide, so it is unreachable today - but a future caller would panic the kernel from a driver path. Recorded, not fixed (guarding at the callers is where the clamp belongs). |
| A7-3 | note | VII | Cap-before-action holds on the new status code: `handle_usb_disk_read`/`_write` validate `USB_DISK_RESOURCE` **before** consulting `usb_disk_absent()`. No new authority; `USB_DISK_ABSENT` is a return value, not a gate. |
| A7-4 | note | III | `MSC_ABSENCE_EXPLAINED` is not a second truth about device presence: it records *whether the absence was announced*, is set only by the removal that prints the cause, and is cleared at both `MSC_READY.store(true)` sites. It reduces to that one event and never overrides `MSC_READY`. |
| A7-5 | note | X | The hot-plug sweep and the latch read are driver mechanism, not new kernel responsibility - no policy moved inward (Commandment I holds). |

**Accepted deviation:** `usb_disk_absent()` returns `true` on the six non-ARM arches, so on x86 a
failed USB-disk syscall now answers `USB_DISK_ABSENT` (-21) rather than -1. No x86 code uses that
path (x86 storage is AHCI), and -21 is the more accurate answer, so this is a widening of truth
rather than a regression.

## North-star invariant

**Nothing above the kernel may panic or wedge the kernel.** For any userspace action - any syscall
with any arguments, any IPC message, any capability use, any driver MMIO/DMA, any hardware state - the
kernel's only allowed responses are: **perform it**, **return a defined error**, or **kill the offending
task**. Never a kernel panic; never an unbounded hang. (Invariant 12; CLAUDE.md 26.6 bounded, 26.4 no
silent fallback, 3.1 validate-before-act; 6.2 the kernel may panic ONLY on its own already-corrupted state.)

### Triage rule (A/B/C)

Every `panic!`/`unwrap`/`expect`/`assert`, every loop/wait, every silent fallback, every arithmetic/index
on a user value, and every driver-hardware/lifecycle access is classified:

- **(A)** unreachable from userspace - not recorded.
- **(B)** a *correct* loud panic on already-corrupted **kernel** state - recorded so no one "fixes" a defense.
- **(C)** reachable from userspace input/behavior/hardware - a **violation** to fix.

## Audit 1 - 2026-07-11 (full-kernel sweep)

Method: 9 parallel subsystem auditors (syscall, ipc, capability, task, memory, smp, arch-cpu, arch-device,
misc), each triaging its files A/B/C, then an adversarial verify pass on every C to confirm it is genuinely
reachable (default: not-a-bug unless a concrete trigger exists). Result: **3 confirmed violations, 5 investigated-and-cleared, 24 correctly-loud panics documented.**

### Confirmed violations (fix these)

> **Status (2026-07-11): all 3 FIXED on `feat/dell-wyse-5070-goldmont-plus`.** C1+C2: the CPU-exception
> vectors 0-31 now discriminate the saved-CS CPL like `pf_handler` - a ring-3 exception (#GP, #DE, #MF,
> #AC, #XM, ...) calls `kill_current()`; only a ring-0 exception halts (`gpf_stub`/`gpf_handler` +
> `exc_stub_noec`/`exc_stub_ec`/`exc_dispatch`, wired in `init_idt`). C3: the runtime supervisor respawn
> calls the non-panicking spawn and re-arms `PENDING` on a transient error instead of `panic!`.
> Boot-verified no regression; a dedicated adversarial regression test (ring-3 `cli`/`div0` -> task
> killed, kernel alive) is the follow-up validation.

#### C1. [HIGH] `kernel/src/arch/x86_64/boot.rs:1344` - arch-cpu (hardware-death)

**What.** A #GP raised by ring-3 code dispatches to gpf_stub -> gpf_handler, which UNCONDITIONALLY calls halt_all_cores() - a whole-machine kernel wedge triggered by userspace. Unlike pf_handler (which checks the user/supervisor bit and calls kill_current for ring-3 faults), the #GP path never inspects the saved CS CPL and never kills the offending task.

**Trigger.** Any ring-3 service that raises #GP(0): a non-canonical data access (e.g. `mov rax, [0x8000_0000_0000_0000]`), a privileged instruction (`hlt`/`cli`/`wrmsr`/`rdmsr`/`in`/`out`), or a bad segment load. Services need not be Rust (Appendix B.2 admits any freestanding ELF) and fuzz F3 bit-flips ELFs, so this is trivially reachable. The IDT (init_idt, boot.rs:1139) routes vector 13 -> gpf_stub with no CPL discrimination; gpf_handler -> halt_all_cores() halts every core. Not covered by adversarial suite A1-A10 (A10 tests syscall-arg validation, not a direct ring-3 fault).

**Fix.** Make gpf_stub mirror pf_stub: for #GP the CPU pushes an error code, so the saved CS is at [rsp+16] - `test byte ptr [rsp+16], 3`; swapgs when ring-3; pass the CPL to gpf_handler. gpf_handler must call crate::task::kill_current() for CPL==3 (service continues) and only halt_all_cores() for CPL==0 (genuine kernel-state corruption).

#### C2. [HIGH] `kernel/src/arch/x86_64/boot.rs:1435` - arch-cpu (hardware-death)

**What.** The catch-all exception_halt (installed at every IDT vector except 6/13/14 in init_idt) unconditionally halts all cores via exception_halt_handler + the `2: hlt; jmp 2b` loop. CPU exceptions that ring-3 code can raise are therefore fatal kernel wedges instead of killing the faulting task.

**Trigger.** Ring-3-reachable exception vectors land here. Most direct: vector 0 #DE via integer divide-by-zero or INT_MIN/-1 overflow in an adversarial or fuzz-mutated (F3) service binary -> exception_halt -> halt_all_cores(). Also #MF (16), #XM (19) via unmasked FP/SIMD exceptions. Each halts the whole multi-core kernel from a single ring-3 instruction.

**Fix.** exception_halt already reads the frame words and identifies the CS slot (0x08 kernel vs 0x28 user) in exception_halt_handler. Use that CPL determination to branch: for a ring-3 CS, swapgs (if needed) and kill_current() so only the offending service dies; reserve the halt loop for ring-0 (CPL==0) exceptions where kernel state is actually compromised.

#### C3. [MEDIUM] `kernel/src/task/mod.rs:3725` - task (panic)

**What.** Runtime supervisor respawn panics the whole kernel on ANY transient spawn failure, defeating the Phase-6 guarantee that supervisor death never reboots.

**Trigger.** Kill the supervisor (`chaos kill-storm supervisor` / control channel) while task slots / frames / kstack pool are momentarily exhausted (e.g. a shell/chaos with SPAWN authority storming transient pipe services). poll_supervisor_respawn() (3704) calls spawn_supervisor() (3725), which does `Err(e) => panic!("supervisor spawn failed")` (3648). A NoMemory/CapTableFull/MapFailed from resource pressure at that instant becomes a kernel panic + reboot - a userspace-reachable DoS reboot.

**Fix.** Split boot-time (fatal) from runtime respawn. In poll_supervisor_respawn, call the non-panicking spawn_service_with_config directly; on Err, log loudly and re-set SUPERVISOR_RESPAWN_PENDING (and clear IN_PROGRESS) so the next Core-0 tick retries, instead of panicking. Only the boot-path spawn_supervisor should remain fatal (Test 1B).

### Backlog hardening pass - 2026-07-11 (post-A14)

The two genuinely-unbounded fault/hardware spins below were *cleared* in Audit 1 (their wedge trigger
is not userspace-controllable) but each is still a latent silent-freeze on absent/wedged hardware -
an invariant-12 / §26.6 gap, and directly relevant to new-hardware bring-up (Wyse 5070). Both are now
**bounded** (committed on `feat/dell-wyse-5070-goldmont-plus`). Behaviour is unchanged on healthy
hardware: a live RTC clears UIP in ~microseconds and a live UART empties its holding register in
microseconds, so the caps are never reached in practice; they only convert a dead-hardware infinite
hang into a bounded, best-effort read/proceed.

- **`kernel/src/arch/x86_64/rtc.rs:125`** - FIXED. `read_datetime_raw` bare `while update_in_progress()
  {}` (x2) replaced with bounded `wait_update_clear()` (`RTC_UIP_SPIN_CAP = 1_000_000`); the
  two-reads-agree retry loop capped at `RTC_CONSISTENCY_TRIES = 128`. On a dead RTC (reads 0xFF, UIP
  bit stuck) the read now returns garbage that `year_plausible` / `deglitch_epoch` already reject,
  keeping the last known-good time - loud-degrade, not a freeze.
- **`kernel/src/arch/x86_64/boot.rs:723`** - FIXED. `serial_poll_thre` (lock-free fault-path THRE poll)
  bare `loop` capped at `SERIAL_THRE_NOLCK_CAP = 1_000_000`; on timeout it proceeds best-effort exactly
  like the already-bounded `serial_thre_wait` (worst case: one dropped diagnostic byte, never a wedge).
- **`kernel/src/arch/x86_64/boot.rs:1452`** (pf_handler fall-through, cleared-fragile) - CLARIFIED. The
  fall-through to `halt_all_cores()` after a ring-3 kill is intentional and fail-safe (halt is the safe
  outcome should `kill_current` ever return; it does not for a ring-3 fault). Comment aligned to the
  sibling `gpf_handler` / `exc_dispatch` idiom introduced by the C1/C2 fix, so the non-return contract
  is explicit rather than implicit. No behaviour change.

Done in a later pass (Item 2, committed `cb24515`):
- **`kernel/src/task/scheduler.rs` driver-death quiesce** - DONE generically, respecting §4.4. Added
  `nic-driver` to the DMA-quiesce (bus-master-clear) set (it was missing - a passthrough NIC DMAing
  into reused frames on death), and added `interrupt::route::unregister` + an IOAPIC line-mask on
  driver death (before the endpoint id is freed) to close the reused-endpoint-id stale-IRQ-route gap.
  Deliberately NOT kernel-side HC reset: that embeds per-device MMIO maps in ring 0 (a §4.4 violation)
  and is redundant - every driver resets its controller on init, so a respawn re-inerts it. A
  bus-master-disabled controller with its route removed + line masked is provably inert with zero
  device knowledge in the kernel. Identity 24/24.

### Investigated and cleared (not violations, but recorded)

- **`kernel/src/task/scheduler.rs:1761`** (task/hardware-death, claimed medium) - MARQUEE: on driver death the kill path only clears PCI bus-master-enable and (for level IRQs) leaves masking to deliver(); it never HALTS/RESETS the controller and never tears down the IRQ route. A co
  - *Cleared:* Traced the real path. On driver death kill_task clears PCI bus-master-enable (pci.rs:159, a straight-line RMW of the Command reg) before frame reclaim, and releases the IOMMU device. It is true it issues no HCRESET/Run-Stop clear and there is no interrupt::route::unregister. But a controller left ru
- **`kernel/src/task/mod.rs:2982`** (task/silent-fallback, claimed low) - resolve_spawn_core returns a placement_override core id unchecked; a spawn onto a non-ready core produces an unschedulable (silently stuck) task instead of a loud PlacementInvalid (violates §9.2 / inv
  - *Cleared:* Traced the full path. handle_spawn (dispatch.rs:537) validates a SPAWN capability before touching core_override, so an ordinary ring-3 task with no caps cannot reach resolve_spawn_core at all; only supervisor/shell/chaos/probes hold SPAWN. The override is masked to 16 bits (core_raw = (arg0>>16)&0xF
- **`kernel/src/arch/x86_64/boot.rs:1415`** (arch-cpu/panic, claimed medium) - pf_handler kills the task for a user #PF (error_code bit 2 set) but then FALLS THROUGH to halt_all_cores(); it is correct ONLY because kill_current() is assumed to diverge (never return). If kill_curr
  - *Cleared:* The fall-through is real in source (kill_current() is typed -> (), not -> !, and halt_all_cores() follows unconditionally), but it is not reachable by a ring-3 page fault. The kill branch runs only when error_code bit 2 (U/S) is set, which the CPU sets exactly for a CPL=3 fault. A CPL=3 fault implie
- **`kernel/src/arch/x86_64/boot.rs:723`** (arch-cpu/unbounded-loop, claimed low) - serial_poll_thre() spins on COM1 LSR bit 5 (THRE) with NO iteration cap, unlike mod.rs::serial_thre_wait which bounds the same poll (THRE_SPIN_CAP). It is used by the lock-free fault-path serial helpe
  - *Cleared:* serial_poll_thre() (boot.rs:720) is genuinely an unbounded `loop` with no iteration cap, unlike the bounded mod.rs::serial_thre_wait (THRE_SPIN_CAP=1_000_000). The SITE is reachable from ring-3: a task with no caps can page-fault (write to unmapped addr, Test 7.B) → pf_stub → pf_handler → serial_put
- **`kernel/src/arch/x86_64/rtc.rs:125`** (arch-device/unbounded-loop, claimed medium) - read_datetime_raw() spins in an unbounded `while update_in_progress() {}` on CMOS status register A bit 7; an absent or wedged RTC (reads 0xFF, so bit 7 is permanently set) makes this loop never termi
  - *Cleared:* The loop IS unbounded and the syscall path IS ungated, but the WEDGE TRIGGER is not userspace-controllable, so this is not a userspace-reachable wedge.

Path verification (all confirmed): dispatch.rs:1280 whitelists query_id 11 and 17 as ungated (`matches!(query_id, 0|3|9|10|11|12|13|14|15|16|17|18)

### Correctly-loud panics (B - do NOT remove; these are the defense)

- **`kernel/src/syscall/dispatch.rs:793`** (syscall/assert) - handle_kill calls assert_cap_table_consistent() after a userspace-triggered kill, which panics if any cap in the kernel tables carries generation > its resource's current generation. This is a correct loud guard on CORRU
- **`kernel/src/syscall/dispatch.rs:792`** (syscall/assert) - handle_kill calls assert_tcb_alive() after a kill; the function panics if a TCB service is found Dead (§6.2). It is currently INERT because the non-restartable TCB set is empty (const TCB: &[&str] = &[]) following Path C
- **`kernel/src/ipc/mod.rs:56`** (ipc/panic) - alloc_endpoint_id panics when the monotonic endpoint-id counter reaches DELEGATED_BASE (4096). This is a loud backstop guarding kernel id-space integrity: colliding endpoint ids with the delegated/file-cap band (capabili
- **`kernel/src/ipc/routing.rs:157`** (ipc/panic) - routing::register panics when all MAX_ENDPOINTS (96) routing slots are valid AND alive. Loud backstop on routing-table exhaustion. Not userspace-unbounded: register() is only called from the kernel spawn path (task/mod.r
- **`kernel/src/capability/generation.rs:31`** (capability/panic) - Generation::bump() uses checked_add(1).expect("generation overflow") - the deliberate H7 loud backstop: at u32::MAX it panics rather than wrapping to a low value, which would resurrect a stale cap's authority. Userspace 
- **`kernel/src/capability/generation.rs:59`** (capability/panic) - next_generation() panics if the global monotonic AtomicU32 wraps to 0 (which would alias Generation::INITIAL and resurrect authority). Every endpoint creation/spawn increments it; overflow needs ~4.2 billion spawns per b
- **`kernel/src/capability/table.rs:250`** (capability/expect) - mint_cap() does .expect("mint_cap: resource not registered"). All userspace-reachable callers mint only ids that were just registered: spawn endpoints (registered in spawn_service_by_name, and the endpoint id space is gu
- **`kernel/src/capability/table.rs:209`** (capability/assert) - register_at_gen() asserts overflow_len < OVERFLOW_CAP for ids >= DIRECT_CAP (8192). No userspace path can register an id in that range: endpoint ids are guarded to < DELEGATED_BASE=4096 (ipc::alloc_endpoint_id panics fir
- **`kernel/src/task/scheduler.rs:1971`** (task/assert) - block_and_reschedule asserts a running task exists; a kernel-internal invariant (CORE_CURRENT is always a valid running slot inside a syscall), not userspace-steerable.
- **`kernel/src/task/scheduler.rs:901`** (task/assert) - prepare_ring3_switch calls assert_no_mid_execution_migration (panics if TASK_CORE[slot] != running core); enforces static-placement (§9.1). pick_next only returns same-core slots, so a mismatch is a kernel logic bug, not
- **`kernel/src/task/scheduler.rs:1088`** (task/panic) - LIVENESS WEDGE watchdog panics when a core makes no progress for ~3s. This is the intended loud-stop defense (invariant 12 / §26.7); it fires on kernel-internal stall state (skew-guarded, TSC-quantum-gated) and is the co
- **`kernel/src/memory/allocator.rs:41`** (memory/panic) - guard_bugcheck panics if alloc_frame is about to hand out a frame inside the kernel-image range [GUARD_START,GUARD_END). Kernel-image frames are never marked free (init_from_map skips [kstart,kend); protect_kernel_page_t
- **`kernel/src/memory/allocator.rs:389`** (memory/unbounded-loop) - alloc_lock_wedge panics after ALLOC_LOCKED spins >=1e9 iterations. The critical section is a bounded bitmap scan (<=256 KiB) always held under without_interrupts (all four entry points), so the holder cannot be preempted
- **`kernel/src/smp/ipi.rs:226`** (smp/panic) - TLB-shootdown ack-wait watchdog: after SHOOTDOWN_WATCHDOG_SPINS (~5e8) iterations of request_and_wait, panics naming the core that never acked. This is the intended loud-failure defense - a remote core stuck IF=0 that wi
- **`kernel/src/smp/spinlock.rs:25`** (smp/panic) - lock_wedge: SpinLock lock()/lock_irq() panics after LOCK_WATCHDOG_SPINS (~1e9) iterations, naming the deadlocked lock address. Intended loud-failure defense - a holder that never releases (un-reschedulable holder or AB-B
- **`kernel/src/smp/percpu.rs:88`** (smp/index) - PerCore::get / PerCoreMut::as_mut_ptr bound the core index with debug_assert! only (compiled out in release), then do base.add(core) - an out-of-range core id would be an OOB pointer deref. Confirmed NOT userspace-reacha
- **`kernel/src/smp/placement.rs:24`** (smp/other) - placement::resolve on an out-of-range contract/override core id is SAFE: is_ready(n) casts to usize and returns false for any c >= num_cores() (no panic, no OOB), so resolve returns Err(PlacementInvalid) for any u32. rou
- **`kernel/src/arch/x86_64/boot.rs:1050`** (arch-cpu/assert) - init_syscall asserts EFER.NXE read back as 1 after setting it (W^X foundation). A boot-time, per-core assertion on CPU/MSR state; correct loud-failure if the NX bit cannot be enabled.
- **`kernel/src/arch/x86_64/boot.rs:250`** (arch-cpu/assert) - audit_wx asserts no sampled page is both writable and executable (W^X hardening). Boot-time audit over kernel-owned page tables userspace cannot influence; correct loud failure on a hardening regression.
- **`kernel/src/arch/x86_64/boot.rs:646`** (arch-cpu/hardware-death) - limit_package_cstates RDMSRs MSR 0xE2 whenever is_intel_cpu() is true, assuming every GenuineIntel CPU implements MSR_PKG_CST_CONFIG_CONTROL. An Intel chip lacking 0xE2 would #GP -> gpf_handler -> halt at boot. Early-har
- **`kernel/src/invariants/assertions.rs:13`** (misc/panic) - assert_cap_validated panics if handed an Err, but every one of its ~9 call sites (syscall/dispatch.rs) passes the literal &Ok(()) on the post-validation success path. The panic branch is a tautological tripwire that cann
- **`kernel/src/invariants/assertions.rs:22`** (misc/assert) - assert_no_mid_execution_migration asserts original_core==current_core before every ring-3 resume (scheduler.rs:901). Runs on every context switch so it is heavily reached, but v1 uses static placement - a task is pinned 
- **`kernel/src/invariants/assertions.rs:59`** (misc/panic) - assert_tcb_alive panics when a TCB service is Dead. Called from handle_kill success path (dispatch.rs:792), but the TCB slice is now empty (&[]) since the supervisor became restartable (Path C/Phase 6), so the loop body 
- **`kernel/src/invariants/assertions.rs:86`** (misc/panic) - assert_cap_table_consistent panics if any active cap carries generation > its resource's current generation ('future' cap). Called from handle_kill success path (dispatch.rs:793). Caps are unforgeable kernel structures; 


## Audit 2 - 2026-07-11 (cross-cutting-concern sweep)

Method: a fresh Workflow decomposed **by cross-cutting concern** (not by subsystem, as Audit 1 did) -
8 parallel auditors, one each for: integer arithmetic on user values, array/slice/pointer indexing,
loop/wait boundedness, lock discipline / deadlock, error-path resource cleanup, `unsafe` SAFETY-claim
re-verification, TOCTOU / cross-core races, and syscall input-validation completeness. Each finding was
then adversarially refuted (default not-a-bug unless a concrete userspace trigger exists), same bar as
Audit 1. This lens finds what a subsystem-local auditor misses: defects whose two ends live in
different files (a cause in `syscall_entry.rs`, a fatal consequence in `boot.rs`).

Result: **13 findings -> 5 CONFIRMED violations (all HIGH), 4 refuted C, 4 B-notes.** (One verify agent
hit the structured-output retry cap and dropped its finding unverified; not among the confirmed set.)
The confirmed set includes the precise root cause of the long-standing intermittent chaos-storm UAF
that was an open follow-up (`project_kernel_pf_reclaim_guard`).

### Confirmed violations (fix these)

> **Status (2026-07-11): ALL 3 FIXED on `feat/dell-wyse-5070-goldmont-plus`.** V3 scheduler UAF
> (`2c402ec`): CAS-claim + Dekker re-check (all four handshake accesses SeqCst) so a cross-core kill
> can never free a task mid-switch. V2 spawn leak (`e907e43`): `cleanup_partial_spawn` unwinds the
> endpoint registrations on every post-reservation error path. V1 user-copy halt (`6a0cbb9`): a per-core
> `USER_COPY_ACTIVE` guard + a `pf_handler` branch kill the caller on a bad user pointer instead of
> halting. Identity 24/24 after each. V3's race needs real multi-core HW to exercise, so its final
> validation is a Wyse/T630 chaos storm; V1/V2 are QEMU-testable (a dedicated A15 regression is a
> follow-up, like A14).

#### V1. [HIGH] `kernel/src/arch/x86_64/syscall_entry.rs:105` + `:114` - user-copy fault halts the machine (unsafe-reverify)

**What.** `read_user_bytes` / `write_user_bytes` rely on `validate_user_ptr`, which only **range-checks**
(nonzero, `< USER_END`, no wrap) - it never verifies the pages are present/writable. The kernel then
reads/writes the slice at CPL0. A range-valid-but-**unmapped** (or read-only, for writes) user pointer
faults inside the kernel copy; because the fault is CPL0 the `#PF` error-code U/S bit is 0, so
`pf_handler` prints "KERNEL PF" and calls `halt_all_cores()`. There is no copy-to/from-user fault fixup
(no extable, no per-CPU user-access flag), so the fault is unrecoverable.

**Trigger.** Trivially reachable by **any** service: `log`/`send` with `msg_ptr` = an in-range but
unmapped VA (e.g. `0x1000`) reads the unmapped page at CPL0 (read side, :105); `recv`/`task_stat`/
`inspect_kernel` with an unmapped/read-only `out_buf` faults on the write (write side, :114). One bad
pointer from one service halts every core. This is the most reachable finding in either audit.

**Fix.** Give the user-copy helpers a page-fault fixup: a per-CPU user-access-in-progress flag with a
resume point, and in `pf_handler`, on a CPL0 fault at a user VA while the flag is set, clear it and
resume to the fixup returning `EFAULT` (kill the caller) instead of reaching the U/S-only halt triage.
Range validity is not a mapping guarantee.

#### V2. [HIGH] `kernel/src/task/mod.rs:3604` (and the other post-endpoint `?` sites) - partial-spawn resource leak (errpath-cleanup)

**What.** The recv-endpoint block (mod.rs:3222-3264) allocates an endpoint id, registers the resource,
routing entry, name, recv+grant caps, and per-IRQ routes. Every fallible step **after** it - driver
MMIO map (:3474), DMA-arena map (:3536), ctx-frame alloc (:3604), ctx-page map (:3638), kstack alloc
(:3645) - returns `Err` via `?` **without unwinding** those registrations. The leaked routing entry
stays `valid + Alive`, so `routing::register` can never recycle it and panics at `MAX_ENDPOINTS=96`
(~26 leaks); independently the leaked endpoint id never returns to the free list, marching
`alloc_endpoint_id` into its panic at `DELEGATED_BASE=4096`.

**Trigger.** A sustained `chaos max-carnage` + `chaos mem-pressure` storm: a driver/service respawn that
loses the frame-allocator race fails at one of the post-endpoint maps, permanently leaking one Alive
routing entry + endpoint id per failure. ~26 accumulated leaks panic `routing::register`; the kstack
pool (224 slots) gives a tighter deterministic variant.

**Fix.** Unwind the partial spawn on any post-endpoint error: free the endpoint id, unregister the
routing / name / resource entries and IRQ routes, release the task slot, then return the error.

#### V3. [HIGH] `kernel/src/task/scheduler.rs:992` (`run`) + `:1244` (timer ISR) - pick-then-commit cross-core UAF (concurrency-races)

**What.** The scheduler publishes a just-picked task (`STATE=Running`, `CORE_CURRENT[cid]=next`, then
load its CR3/kstack and `switch_context`) with **no re-check that a concurrent cross-core kill set the
slot Dead**, and it publishes `CORE_CURRENT` only **after** `pick_next` read `STATE=Ready`.
`kill_task_by_slot`'s spin-wait breaks the instant `CORE_CURRENT[peer] != slot`, so in the pick->publish
window it frees the victim's PML4 / user frames / kstack; `switch_context` then loads a freed
(possibly re-alloced-and-zeroed) CR3 -> kernel `#PF` / UAF. The handshake is one-sided (kill does
store-Dead-then-load-CORE_CURRENT; the scheduler does store-CORE_CURRENT-then-use-CR3 with no matching
load of STATE - an incomplete Dekker pattern). The `next != prev` timer path (:1244) is worse: it stores
Running/CORE_CURRENT **unconditionally**, unlike the Dead-preserving CAS used for `prev` and the
`next == prev` path.

**Trigger.** Real multi-core hardware only (TCG serializes cores, cannot repro). A userspace cross-core
kill (`chaos max-carnage`, shell `kill`, supervisor `restart`) of a service pinned to another core,
racing that core's `pick_next` / timer ISR. **This is the precise root cause of the known intermittent
chaos-storm UAF** (b9dbc4c only catches the downstream corrupt-PTE walk; it does not close this window).

**Fix.** After `cli` + publishing `CORE_CURRENT`, re-load `TASK_STATE[next]` (and `TASK_VALID`) and
abort the switch (set `CORE_CURRENT=IDLE`, re-pick) if it is Dead - completing the Dekker handshake with
the kill's store-Dead-then-load-CORE_CURRENT spin-wait. Apply to both `run` and the `next != prev` timer
path.

### Refuted (investigated, not violations)

- **scheduler.rs:1814** kill-path CORE_CURRENT spin has no *iteration* cap - REFUTED: covered by the
  cross-core LIVENESS WATCHDOG (~3s loud panic naming the stalled core) on real HW; the mutual-wait ring
  needed to hang it is not constructible from the serialized kill triggers.
- **scheduler.rs:413** `TASK_SLOT_LOCKED` hand-rolled CAS has no watchdog - REFUTED: every critical
  section is a bounded `MAX_TASKS` scan under `without_interrupts`, no holder can fail to release without
  the kernel already being wedged (a B scenario). Consistency-hardening only.
- **capability/delegated.rs:172** `BAND` uses `lock()` not `lock_irq()` - REFUTED: every acquirer runs
  IF=0 today (syscall interrupt-gate, IF=0 kill path), so no preemptible holder exists. Latent
  future-code hazard, not live.
- **interrupt/route.rs:59** `IRQ_TABLE` uses `lock()` not `lock_irq()` - REFUTED: same, all acquirers
  IF=0; single-array critical sections drain in ns. `lock_irq`-convention hygiene, not a live deadlock.

### B-notes (correctly-loud, do NOT remove) + latent hardening

- **generation.rs:31 / :59** - `bump()` `checked_add.expect` and `next_generation()` wrap-to-0 panic:
  correct H7 defenses (a silent wrap resurrects stale authority). ~4.2e9 bumps/spawns to reach; keep.
- **ipc/mod.rs:55** - `alloc_endpoint_id` panic at `DELEGATED_BASE`: correct backstop against an endpoint
  id aliasing the delegated/file-cap band; kept unreachable by id reuse bounding the live range to <=96.
- **allocator.rs:261** - `free_frame` phantom-frame guard checked only `idx >= max_valid_frame` but
  `max_valid_frame` is set from region extents **unclamped**, while the bitmap is sized `MAX_FRAMES`
  (8 GiB / 4 KiB). On a machine with **> 8 GiB RAM**, a corrupt/stale PTE whose index lands in
  `[MAX_FRAMES, max_valid_frame)` passed the guard and OOB-indexed the bitmap. Not userspace-reachable
  (only a pre-corrupted PTE reaches it - a B), and the T630/Wyse test boxes have 8 GiB (band empty), but
  a genuine latent hardening gap. **FIXED (`f276f61`):** the guard is now
  `idx >= max_valid_frame || idx >= MAX_FRAMES`; the alloc path never returns `idx >= MAX_FRAMES`, so no
  legitimate free is rejected.

### Regression tests

- **A14** (`b97c23d`) pins C1/C2: a ring-3 CPU exception (#GP, #DE) kills the task, not the kernel.
- **A15** (`90d520a`) pins V1: a bad user pointer to a syscall (`raw_syscall(log, cap 0, 0x1000, 16)`)
  faults in the kernel copy at CPL0 and the kernel logs `USER-COPY PF (killing caller)` + kills the
  caller instead of `halt_all_cores()`. `osdev test adv` 15/15.


## Audit 3 - 2026-07-13 (post-v0.4.0 re-audit)

Method: 2 parallel auditors (arch layer; core syscall/ipc/cap/task/memory/smp), each triaging A/B/C
against the north-star, then the lead **re-verified every confirmed finding against source** before
recording it (a subagent's "confirmed" is a lead, not a verdict - the "day my own test lied" discipline).
Motivation: a large surface landed since Audit 1/2 (dynamic core count / `MAX_CORES` removal, the
multi-method `hardware_reset`, the auto-repeat calibration, fbcon safe-area) plus the whole v0.4.0
userspace release - the audit's job is to prove the *new* code did not open a north-star gap and that
the Audit 1/2 fixes are still intact.

Result: **1 confirmed violation (MED), 2 latent hardening notes (LOW), all Audit-1/2 fixes verified
present-and-correct.** The core kernel came back clean; the one real finding is in the arch fault path.

### Confirmed violation (fix this)

> **Status (2026-07-13): K1 + K2 + K3 ALL FIXED on `feat/audit-kernel-and-userspace`.** K1: all five
> exception stubs now bound the asm THRE poll with an `ecx` spin counter (~1M, mirroring
> `SERIAL_THRE_NOLCK_CAP`), falling through to the breadcrumb write best-effort on timeout - so a ring-3
> fault on a wedged UART kills the task instead of spinning the core forever. `ecx` is safe scratch there
> (the stubs that need `rcx` reload it from the stack after the poll). K2: the BSP LAPIC id now gets the
> same loud xAPIC-ceiling check the APs have. K3: the APIC spurious vector 0xFF now routes to a dedicated
> `spurious_stub` (bare `iretq`) instead of `exception_halt`, so a spurious IRQ is a no-op not a wedge.
> Kernel + image build clean; identity 24/0, adversarial 15/0 (incl. A11/A12/A13 cap-gating).

#### K1. [MED] `kernel/src/arch/x86_64/boot.rs:1291,1336,1514,1592,1622` - arch-cpu (unbounded-loop / invariant 12)

**What.** The five naked exception stubs (`gpf_stub`, `pf_stub`, `exception_halt`, `exc_stub_noec`,
`exc_stub_ec`) each *open* with a raw-asm COM1 THRE poll as their absolute first instructions -
`mov dx,0x3fd; 88: in al,dx; test al,0x20; jz 88b` - which is **unbounded**. This is the exact scenario
`SERIAL_THRE_NOLCK_CAP` (boot.rs:719-725, "an absent or wedged COM1 must not hang a fault handler
forever") was added for, but that Audit-1 fix bounded only the *Rust* `serial_poll_thre`; the inline
asm polls at the front of each stub escaped it. **Verified** in source: `gpf_stub` (:1288-1296) loops
on `jz 88b` before writing its 'G' breadcrumb, and the sibling stubs match.

**Trigger.** Any ring-3 fault a service can raise at will (`div` by zero -> #DE, `cli`/`hlt` -> #GP, a
null deref -> #PF) on a machine whose COM1 LSR reads with THRE (bit 5) *persistently clear* - a
present-but-clock-gated/wedged UART. (An *absent* port reads 0xFF, bit 5 set, exits immediately, so this
needs present-but-wedged, the same hardware state the existing cap targets.) The faulting core then
spins forever with IF=0 - a silent single-core wedge from a ring-3 instruction, instead of killing the
task. Latent on the T630/Wyse (COM1 healthy), but a genuine invariant-12 gap.

**Fix.** Add a bounded spin counter to the asm poll in all five stubs (mirror `SERIAL_THRE_NOLCK_CAP`),
falling through to the breadcrumb write best-effort on timeout - exactly as the Rust helper already does.

### Latent hardening notes (LOW - real but no current trigger)

- **K2. [LOW] `kernel/src/arch/x86_64/ap_boot.rs:33`.** **FIXED (`feat/audit-kernel-and-userspace`).**
  The BSP was exempt from the loud xAPIC 8-bit LAPIC-id ceiling the APs get: it was stored without a
  range check while APs above 0xFF are excluded *loudly* (ap_boot.rs:46-64). A BSP with LAPIC id > 255
  (x2APIC-scale machine) would silently mis-route AP->BSP IPIs (`lapic_id & 0xFF` in `send_ipi`). The
  fix adds the matching loud check before storing the BSP LAPIC id (`bsp_lapic > XAPIC_MAX_LAPIC_ID ->
  loud "needs x2APIC" warning`). Exotic trigger; now consistent with the AP path and the loud-failure
  discipline (§26.7).
- **K3. [LOW] `kernel/src/arch/x86_64/boot.rs:323` (SVR=0x1FF) + `:1173` (IDT[0xFF]).** **FIXED
  (`feat/audit-kernel-and-userspace`).** The kernel programs LAPIC spurious vector 0xFF but routed that
  vector to the default `exception_halt` (hlt-loops the core). A spurious-vector delivery - which the SDM
  says to ignore-and-return - would wedge the whole machine. The fix gives 0xFF a dedicated `spurious_stub`
  (a bare `iretq`: no EOI, no register save, no swapgs - correct from either ring), wired in `init_idt`.
  A spurious IRQ is now a no-op, not a wedge (north-star: a non-fatal hardware event must never wedge the
  kernel, inv12). Identity 24/0 + adversarial 15/0 after the change.

### Verified present-and-correct (Audit 1/2 fixes + new code)

- **C1/C2** (ring-3 CPU-exception CPL discrimination): PRESENT. `init_idt` routes vectors 0-31 to
  CPL-discriminating stubs; `exc_dispatch`/`gpf_handler`/`pf_handler` kill the ring-3 task, halt only on
  ring-0. All gates DPL=0 except 0x80 (no ring-3 `int N` bypass).
- **V1** (user-copy fault fixup): PRESENT. Per-core `USER_COPY_ACTIVE`, set narrowly around the single
  `copy_nonoverlapping`; `pf_handler` clears it and `kill_current()`s on a CPL0 fault at a user VA.
- **V2** (partial-spawn cleanup): PRESENT. `own_endpoint` set right after registration; every post-endpoint
  error path routes through `cleanup_partial_spawn` (no leak toward the routing / endpoint-id panics).
- **V3** (scheduler Dekker re-check): PRESENT. `run()` and the timer `next!=prev` path CAS-claim `next`
  (Ready->Running, SeqCst), publish CORE_CURRENT, fence, re-load STATE, abort if Dead; kill completes the
  handshake. No mid-switch UAF.
- **C3** (runtime supervisor respawn): PRESENT. `poll_supervisor_respawn` re-arms PENDING on a transient
  Err instead of panicking; only boot-time `spawn_supervisor` is fatal.
- **`hardware_reset`** (new, multi-method): SAFE + TERMINAL. `io_delay` (10k) and 8042 wait (1M) bounded;
  the triple-fault fallback (zero-limit IDT + `int3` -> #DF -> shutdown) is unconditionally terminal; the
  trailing hlt-loop is an unreachable type-level backstop. Reboot is cap-gated (`REBOOT_RESOURCE`, granted
  only to shell/xhci/ehci) - no ambient reset authority.
- **Dynamic core count** (new, `MAX_CORES` removed): OOB-FREE. Arena width == cores started (identical
  `lapic_id <= 0xFF && != bsp` filter in `ap_count`/`start_all_aps`); AP exclusion is loud; every runtime
  per-core index guards `core < num_cores()` or uses a kernel-assigned id. No core-id OOB introduced.
- **New-syscall user-value paths**: all bounded before use - resource_mint/invoke/revoke (delegated-band
  range-checked, rights masked, id released on cap-table-full, badge kernel-set-only/unforgeable),
  LastRecvBadge (no user arg), AcquireSendCap (name len <=64, ACQUIRE_ANY-or-declared-peer gated),
  inspect_kernel core-id (guarded `>= num_cores() -> 0`), task_stat/task_caps slot (`>= MAX_TASKS`).
- **fbcon SAFE_PCT, rtc, serial (Rust helpers), iommu, pci, page_tables**: all arithmetic/loops bounded.

### Notes for record (not bugs)

- `cleanup_partial_spawn` does not release a *failed driver spawn*'s IRQ lines (only driver spawns
  register IRQs, only a post-IRQ map failure leaves a stale entry). `IRQ_TABLE` is a fixed
  `[Option; 256]`, and a stale route delivers to a now-dead endpoint, so it cannot fault.

  **AMENDED 2026-08-25.** Two claims here did not survive contact with hardware, and both are worth
  correcting rather than quietly editing away.

  The parenthetical said driver *death* "already unregisters". It did not, in general: the kill path
  worked the owning line out from a hardcoded list of service NAMES - `"xhci"`, `"ehci"`, and nothing
  else - so `dwc2` leaked its route on every restart. Death now releases by ENDPOINT
  (`route::unregister_endpoint`), which is a lookup rather than a list and cannot go stale when a
  service is added, renamed, or ported to an arch with a different vector. The contrast the note drew
  is true now; it was not when it was written.

  And "self-correcting, a respawned driver overwrites it" understated the cost. The overwrite is
  exactly what the kernel had been reporting - "IRQ 41 already routed - overwriting (second claim or a
  missed unregister?)" - and the consequence was interrupt delivery that varied run to run on
  identical code: 9 interrupts, then 7, then 0. Harmless to memory safety, yes; not harmless to the
  driver that stopped receiving interrupts.
- The B-set of correctly-loud panics (generation overflow, endpoint-id/routing exhaustion, liveness/
  shootdown watchdogs, W^X asserts) is unchanged from Audit 1/2 and re-confirmed as the defense.

### Hardware sign-off - 2026-07-13 (HP T630, AMD GX-420GI)

The Audit-3 fixes are validated on real silicon, not just QEMU. Built a clean `--mode identity` image
from `feat/audit-kernel-and-userspace` (`cargo clean` + `osdev image --mode identity`, copied before any
rebuild), pre-flighted it under QEMU/OVMF (UEFI path, green), then flashed and booted it on the T630
(serial `build/serial_output.log`, 22:02-22:05).

- **Boot + bring-up clean:** 4 cores ready; syscall init on every core (LSTAR/EFER/GS correct); **W^X
  audit ok** (kernel-text W=0/NX=0, kernel-data W=1/NX=1); `supervisor: ready`, `logger: ready`.
- **Real AMD-Vi IOMMU** (which QEMU cannot faithfully exercise) came up end to end: IVRS found, device
  table + rings programmed, **translation ON, zero fault events**, block-driver in passthrough
  (`CONFINE_USB_DRIVERS=true`). No IO_PAGE_FAULT.
- **Self-run identity checks all pass:** cap-test 2A/2B/2C + revoke + endpoint-dead + grant; ipc-test
  routing; probe 3A/3B/4A/5A/5B/9B/7A/7B; **8A yielder ticked** (preemption); 11A ready.
- **Steady state healthy:** cross-core ping/pong climbing one/sec with no gaps (`pong: received "127"`
  ~2 min in). **No panic, no exception, no spurious-vector/LAPIC anomaly.**

Bearing on the Audit-3 fixes specifically: **K1** (bounded THRE poll) and **K3** (spurious-vector
`iretq` stub) exercise the arch fault/interrupt path that only fully lights up on real hardware - the
machine ran the fault-touching self-checks and idled/serviced interrupts for minutes with no wedge;
**K2** (BSP LAPIC ceiling) is on the AP-bring-up path that printed all 4 cores ready with no
`unaddressable`/mis-route line; **U15** (`service_privileges`) is proven live because every service
that needs a privileged cap (supervisor spawn, probe kill/introspect for the self-run tests) got it and
the negative pins (A11/A12/A13, verified in QEMU) hold. On-silicon sign-off: the audited kernel boots,
self-checks, and runs clean on the AMD GX-420GI. (The host-driven 24-case suite - Tests 1B/6/10/12/13/15,
which need the control channel - remains the QEMU gate, 24/0 on this branch.)

---

## Audit 4 - 2026-07-15 (full-kernel sweep + arch-demarcation focus)

Method: 5 parallel subsystem auditors (syscall+ipc+interrupt, capability, task+scheduler, memory+smp,
arch+boundary-seam), each reading its files in full and triaging A/B/C, with this branch's arch-boundary
demarcation (`arch::imp` seam, `portable_atomic`) as a focused target. Every prior finding (C1/C2/C3,
K1/K3, V3) re-verified **present in current source**, not just claimed.

**Result: the arch demarcation is sound - zero boundary leaks (verified four ways), a mechanical
extraction with no logic change. One new (C) resource-leak finding (T1) + four (A/B) dead-code/hygiene
items. No new panic/wedge/inconsistent-state violation; no regression of any prior fix.**

| ID | Sev | Class | What | Status |
|----|-----|-------|------|--------|
| **T1** | MED-HIGH | (C) | `task/mod.rs` `spawn_service_with_config` leaks the page-table + ELF + user-stack + ctx frames on any `Err` after `loader::load()` - `cleanup_partial_spawn` unwinds only the endpoint/routing/name/slot half and never reclaims `page_table` (no `Drop` on `PageTable`/`Frame`). Trigger: kstack-pool exhaustion under a concurrent spawn burst (`chaos max-carnage`); `poll_supervisor_respawn` retries on transient `Err`, so a respawn failing partway leaks more frames each attempt - a ratchet that can defeat the "reclaim every respawn" property (mod.rs:3830). Breaches §26.6 / Commandment IX; NOT the strict north-star (no panic/wedge). | open, staged - give `cleanup_partial_spawn` the `page_table` and reclaim via `reclaim_user_frames` + free the never-loaded PML4. |
| **M1** | LOW | (A) | `memory/ownership.rs` (`TaskMemoryOwner`,`FrameSet`) + `memory/page.rs` (`Page`) + `task/task.rs`'s `Task` are **dead code** (zero callers), yet `memory/CLAUDE.md` + `task/CLAUDE.md` describe them as the live limit/reclaim path - III doc-vs-dead-code drift. Live: `scheduler.rs` `TASK_ALLOC_BYTES`/`current_task_claim_alloc`; `arch/x86_64/page_tables.rs::reclaim_user_frames`. | **doc fixed** (banners repoint at live code); dead-code deletion staged. |
| **M2** | LOW | (A) | `smp/placement.rs` (`resolve`,`round_robin_next`,`static mut RR_COUNTER`) is dead - a known-unsound `static mut` stub - yet `task/CLAUDE.md` cites it as the live spawn placement. Live: `task/mod.rs::resolve_spawn_core` (atomic). | **doc fixed** (banner); dead-code deletion staged. |
| **K-a** | LOW | (A) | `capability/cap.rs` `Capability::validate` + `narrow_for_grant` are dead (re-implemented inline in `CapTable::get`) - a mild III duplicate-logic smell. | staged (collapse onto one impl if `capability/` is next touched). |
| **K-b** | LOW | (B) | `capability/table.rs` `CapTable::get`'s diagnostic `kprintln!` runs while a `GLOBAL_RESOURCES` lock guard is live - latency hygiene, not a deadlock. | staged. |

Re-verified present-and-correct (no regression): boundary integrity (0 leaks - `arch_boundary_check.py`
plus independent greps for named-arch / `target_arch`-cfg / `asm!` / `AtomicU64`); C1/C2 (ring-3 fault
kill vs halt), K1 (bounded THRE poll), K3 (spurious-vector `iretq` stub); V3 (scheduler UAF Dekker
handshake); C3 (non-panic supervisor respawn). The demarcation is a mechanical `arch::x86_64::` ->
`arch::imp::` + `core::sync::atomic::AtomicU64` -> `portable_atomic::AtomicU64` substitution, verified via
diff to have zero logic change.

## Audit 5 - 2026-07-23 (feat/pi2-arm32: the ARM32 layer we built)

Method: 4 parallel subsystem auditors over the ~6,000 lines of new/changed kernel code on this branch -
(1) `dwc2` USB driver + `timer` + `video`; (2) `exceptions` + `syscall` + `irq` (the trap/SVC path);
(3) `mmu` + `page_tables` + `spawn` + `usermode` + the neutral `loader`; (4) `mod` boot + `context_switch`
+ the neutral `scheduler`/`task`/`dispatch`/`allocator` changes. Each triaged A/B/C; every (C) was
adversarially re-verified against a concrete trigger (default: not-a-bug) before being recorded here, and
every fix was build-checked on **both** armv7 and x86 (the neutral files) and boot-checked in QEMU
`raspi2b` (arm-shell: `usermode PASS`, shell ready, 0 faults).

**Result: 10 confirmed (C) violations - 8 FIXED, 2 STAGED (latent, neutral-loader).** Two were
userspace-reachable HIGH kernel-wedges (a bad-pointer syscall halted the kernel; a magic syscall number
diverted control into stale boot state) - both now closed. The genuine ring-3 fault path (USR-mode data/
prefetch abort, undefined instruction) was already correct (kills only the faulting task). The SEC-25
weak-memory port obligation is now **met** (the port this branch is). No finding is a kernel panic; the
loader does not panic/hang on any malformed ELF.

| ID | Sev | Class | What | Status |
|----|-----|-------|------|--------|
| **A5-1** | HIGH | (C) | `arch/arm/video.rs` `mbox_call` - 3 unbounded mailbox spins (FULL/EMPTY/response-match) run at boot before the scheduler; an absent/wedged VideoCore hangs the boot forever (invariant 12). | **FIXED** - bounded each (`MBOX_SPIN_CAP`/`MBOX_MATCH_CAP`); on timeout report + return false (callers fall back to serial). |
| **A5-2** | HIGH | (C) | `arch/arm/mod.rs` `read_user_bytes`/`write_user_bytes` - a range-valid-but-unmapped (or, for a write, read-only) user pointer to ANY copying syscall (`log`, `send`/`call`, `recv`/`console_read`, ...) faults the raw copy in SVC mode; the abort handler classifies that as a kernel bug and HALTS the core. Any service wedges the kernel with one bad-pointer syscall (the ARM analog of x86's `USER_COPY_ACTIVE` gap). | **FIXED** - pre-validate every page via the CP15 unprivileged-translation probe (`translate_user`, non-faulting) under the service's own TTBR0; return a defined error instead. No TOCTOU (a task can't mutate its own page tables mid-syscall, nor run concurrently). |
| **A5-3** | HIGH | (C) | `arch/arm/syscall.rs` `arm_svc_dispatch` intercepted the magic `USER_TEST_SVC` (0x5555_0001) **unconditionally**; a live service issuing `svc r0=0x55550001` diverts the kernel into a stale boot context on a boot-era `sp` (PL0-reachable wild control flow) instead of `UnknownSyscall`. | **FIXED** - gated behind `SELFTEST_ACTIVE`, armed only for the boot selftest round trip; production returns `UnknownSyscall`. |
| **A5-4** | MED | (C) | `arch/arm/dwc2.rs` `chan_in` - the inner `RxFLvl` FIFO-drain loop reset the outer 4M timeout on every pop, so a core keeping `RxFLvl` asserted hangs forever (the 4M cap is never reached). Reachable at boot via `enumerate_sync` on a present-but-wedged core. | **FIXED** - independent `RX_DRAIN_CAP` on the inner drain. |
| **A5-5** | MED | (C) | `arch/arm/timer.rs` `delay_us` - unbounded spin on a System Timer that never advances (dead peripheral); also made the `selftest` "did not advance" FAIL branch unreachable. | **FIXED** - `DELAY_SPIN_CAP` ceiling; returns best-effort on a stuck timer. |
| **A5-6** | (config) | (C) | `arch/arm/sched_spawn.rs`/`sched_user.rs`/`sched_ipc.rs`/`sched_demo.rs` - the cr3/TTBR0-seed guard (`disable_interrupts` before `NEUTRAL_SCHED`) was on the shipping paths (`sched_shell`/`sched_supervisor`) but MISSING on these demo/increment paths; a timer in the window wedges the core silently (ARM liveness watchdog is off). NOT userspace-reachable (a service can't pick the boot feature). | **FIXED** - added the guard to all four, uniform with the shipping paths. |
| **A5-7** | MED | (C)-latent | `task/scheduler.rs` (SEC-25) - `reserve_task_slot` stored `TASK_VALID` (Release) *before* `TASK_CORE` (data), and 33 `TASK_VALID` readers were `Relaxed`; on live weak-ordered 4-core ARM a reader can observe `VALID==true` with stale data. The *critical* scheduling path was already saved by the `TASK_STATE` Release/Acquire publish (no UAF today); the residual hazard was best-effort/introspection readers. | **FIXED** - write `TASK_CORE` first then `TASK_VALID` (Release); all 34 readers now Acquire. x86 codegen unchanged (Acquire load == `mov` under TSO); x86+arm both build. The SEC-25 port obligation is met. |
| **A5-8** | LOW | (C) | `arch/arm/video.rs` `request` - GPU-returned `pitch`/`w`/`h` used unvalidated in the `fill` loop + mapping length (GPU is trusted, but defence-in-depth). | **FIXED** - range-check geometry before use (matches `query_display_size`). |
| **A5-9** | LOW | (B->fix) | `syscall/dispatch.rs` `handle_log` - debug `[hl:*]` serial breadcrumbs left in a **neutral** file (fired on x86 too); a capless-log spammer floods serial (bounded, non-wedging console noise). | **FIXED** - removed; error returns unchanged. |
| **A5-10** | MED | (C)-latent | `loader.rs` - `p_vaddr` never range-checked against the kernel/user VA split; a crafted ELF can overlay a USER page onto a kernel/MMIO VA in its own (kernel-shared, no per-syscall TTBR switch) address space. Latent on ARM (only trusted embedded ELFs load; fuzz/probe are x86-only), but the loader is the neutral spawn/fuzz entry. No panic/wedge (indices stay in range). | **STAGED** - add an arch-provided user-VA-window check in the loader (the missing analog of x86 higher-half separation). |
| **A5-11** | MED | (C)-latent | `loader.rs` - on any `Err` after `PageTable::new()`, the partial page-table (L1/L2 arena slots) + already-allocated frames leak (no `Drop`, no cleanup path); repeated failed spawns permanently exhaust the 16-slot L1 arena. Degrades gracefully (errors, no panic - fuzz F3 still passes) but is an unrecoverable resource ratchet (26.6). Same root as x86 **T1** (Audit 4, staged). | **STAGED** - reclaim the partial address space on the error path (`reclaim_user_frames` + `free_page_table_root` exist), or give `PageTable` a `Drop`; fix x86 T1 + this together. |

**Doc-drift corrected (Findings 3/4, Low):** `arch/CLAUDE.md` claimed `invalidate_tlb_page` broadcasts on
ARM (it is local `TLBIMVA` - correct for pinned per-task address spaces, but the doc overstated) and did
not record that `write_page_table_base` does no TLB maintenance (harmless - switching goes through
`switch_context`, which flushes). Both corrected in the SEC-25/26/27 note; SEC-25 marked DONE.

**Re-verified sound (no violation):** the USR-mode data/prefetch-abort + undefined-instruction handlers
correctly kill only the faulting task and keep the kernel alive; the `stub_svc` SPSR/banked-register
window is fully IRQ-masked and the atomic-syscall gate stops a mid-syscall preemption from clobbering
`SPSR_svc`; `switch_context` save/restore (incl. per-task USER-banked SP/LR on both IRQ and SVC paths,
`clrex`, TTBR0-compare+`TLBIALL` on change) is correct; `smp_bringup` is bounded (40M-spin/core then
proceeds; core-3 mis-ID parks in `wfi`); the neutral loader validates every ELF header field before use
(`checked_add`/`checked_mul`, bounds vs `bytes.len()`) so no malformed ELF panics or hangs; `dwc2`
`init`/`reset_port`/`wait_halt`/`chan_out` waits are all bounded-and-loud; and there is **no**
`panic!`/`unwrap`/`expect`/`unreachable!` anywhere in `arch/arm/`.

**Observations (noted, not confirmed-reachable):** a deeply-nested syscall could overflow a task's SVC
kernel stack and fault in SVC mode -> the A5-2 pre-validation does not cover that (no ARM kernel-stack
guard page yet; C5-class hardening, reachability unproven). `USER_SPSR_SAVE` is a global `static mut`
written by every `svc` across cores - benign for a selftest artifact, and dead in production once A5-3
gated the magic path.

## Audit 6 - 2026-07-23 (feat/pi2-arm32: the USB-net bridge + hardware helpers we added since Audit 5)

Method: 2 parallel auditors over this session's kernel additions AFTER Audit 5 - (1) the neutral
syscall/cap layer (the `NetFrame*` syscalls 42-44 + `handle_gpio` 45 + InspectKernel query 19 handlers,
the `NET_DEVICE`/`GPIO_DEVICE` resources, their privilege mints, the SEC-11 gate-safety); (2) the arch/arm
drivers (`hardware_reset` watchdog, `hw_random` RNG, `gpio_op`, `now_epoch_monotonic`, and the dwc2
USB-net bridge / multi-device / smsc95xx work). Lens: **robustness / liveness / correctness of untrusted
syscall input and device-driven loops** - memory-safety (`unsafe-audit.md`) and authority
(`security-audit.md` Audit 2) were audited separately and are NOT re-covered here.

**Result: 0 CONFIRMED north-star violations.** Every new copying syscall gates the cap FIRST, bounds all
user args, and routes pointers exclusively through the fault-safe `read_user_bytes`/`write_user_bytes`
wrappers (inheriting V1/A5-2 fault-safety by construction); every device-driven loop is bounded; the one
divide (`now_epoch_monotonic`) is guarded. 4 latent/defense-in-depth hardening items, all **FIXED**.

| ID | Sev | Class | What | Status |
|----|-----|-------|------|--------|
| **A6-1** | INFO | (defense-in-depth) | `syscall/dispatch.rs` `handle_net_frame_rx` - `buf[..n]` trusts the arch `net_frame_rx` to return `n <= max`; a future buggy arch impl returning `n > max` would index-panic the kernel. Not user-controllable (arch code), so not a live north-star finding. | **FIXED** - `.min(max)` clamp on the returned length; the neutral layer is now robust against a buggy arch impl. |
| **A6-2** | LOW-latent | (C) | `arch/arm/mod.rs` `hardware_reset` - the BCM2835 watchdog poke is correct, but on an absent/wedged PM block that never resets, control fell through to a bare terminal `loop { spin }` on a SINGLE poke; unlike x86's unconditionally-terminal triple-fault. No trigger (QEMU raspi2b + real Pi 2 both honor the watchdog). | **FIXED** - the terminal loop now RE-ISSUES the watchdog poke every iteration, so a write that did not take is retried (BCM2835 has no second reset method; this still never returns). |
| **A6-3** | LOW-latent | (C) | `arch/arm/mod.rs` `now_epoch_monotonic` - the divide-by-zero IS guarded (`if hz==0 return`), but it returned a FROZEN `0` when `timer_hz()==0` (dead generic timer), so any purely time-bounded wait computing `deadline = now + ticks` would never advance -> a bounded wait becomes unbounded. No trigger (QEMU + Pi 2 both set TIMER_HZ). | **FIXED** - falls back to the 1 MHz System Timer (`timer::systimer_secs`) so the monotonic clock still advances in the degraded-timer case (wraps ~71 min, still better than frozen). |
| **A6-4** | LOW | (C) | `arch/arm/mod.rs` `pl011_init` opened with an UNBOUNDED `while FR & BUSY {}` boot wait on the console UART; a present-but-wedged PL011 hangs the boot (invariant 12). Pre-existing machine-layer code (not this session's bridge work), same class as the x86 THRE-poll K1 fix. No trigger (QEMU/firmware leave it idle). | **FIXED** - bounded (1M spin cap, best-effort proceed on timeout). |

**Verified sound (no violation):** all four new handlers gate the cap first + bound args + use the
audited user-copy wrappers (no raw `from_raw_parts`/`copy_nonoverlapping`); `handle_gpio` bounds BOTH
`op` and `pin` before the arch call (and `gpio_op` re-checks the pin, `_ => -1` default); query 19 is
correctly ungated (entropy leaks nothing, x86 returns None immediately, ARM's FIFO wait is 2M-bounded ->
None); `NET_DEVICE`(10)/`GPIO_DEVICE`(11) registered unconditionally so `mint_cap` cannot expect-panic;
the SEC-11 `id.0 >= 100` assert correctly covers ids 10/11 (holds-resource gen-safety sound); the spawn
mints unwind cleanly (`cleanup_partial_spawn` on `CapTableFull`). Arch drivers: the RNG FIFO wait (2M),
GPIO pin/op guards, `hardware_reset` sequence, and the ENTIRE dwc2 chain (init spins, `wait_halt` 4M /
`poll_wait_halt` 500k, `enumerate_hub` bounded by `next_addr > 120`, `configure_smsc95xx` + `smsc_mii_wait`
all capped, `net_frame_tx`/`rx` bounded + the smsc RX length hard-guarded) are individually bounded - no
unbounded device-driven loop, no boot-wedge on hostile/absent hardware.

---

## Audit 7 (2026-07-25, `feat/pi2-arm32` @ `74ee6ff`) - the USB mass-storage + durability work

**Scope:** the unaudited range `6929e28..HEAD` (73 commits, ~1,840 kernel lines across 18 files): the
DWC2 mass-storage/BOT path, the channel interlock, the cache-flush/FUA durability work, syscall 49
(`UsbDiskFlush`), the `resource_invoke` ABI repack, and the ARM console/HID/page-table changes.

**North star, restated:** nothing above the kernel may panic or wedge it, and the kernel must never
hand a service data it did not receive. This audit found one breach of the second half.

**Verdict:** authority and memory-safety are sound. Findings concentrate in liveness, one
data-integrity gap, and one restart break. **1 HIGH, 4 MED, 2 LOW, 2 INFO; 6 FIXED, 2 RECORDED.**

| ID | Sev | Class | What | Status |
|----|-----|-------|------|--------|
| **K7-1** | **HIGH** | (L) liveness | Aggregate IRQ-masked time. Each LEAF wait is bounded (`HALT_BUDGET_US` 2 ms, `POLL_HALT_BUDGET_US` 1 ms, `wait_for_uframe` 1.5 ms), but the retry loops MULTIPLY and nothing bounds the product: `chunks x BOT_TRIES x ss_tries x [uframe + halt + NYET retries]`. A block-I/O syscall runs with IRQs masked, so the timer tick, preemption and the keyboard poll all stop for the duration, and ARM has no liveness watchdog to convert it into a loud panic. The worst case needs a FULL-SPEED stick (`split_port != 0`); the high-speed direct path used on the Pi 2 today costs ~96 ms per FAILING `bot_command`. The comment at `dwc2.rs:559` claims "even a fully wedged device cannot starve the timer ISR" - true of one `wait_halt`, not of one syscall. | **RECORDED** - the fix is a single whole-command deadline captured in `bot_command` and threaded through `chan_dma`/`split_txn`, bounding the product rather than each leaf. Deliberately not attempted blind at the end of an audit: this is exactly the class where an untested "fix" has already cost this port a regression (selfcheck 16 -> 70), and the path the hardware actually takes is the bounded one. Tracked together with the async block path that K7-2 and the durability gap also want. |
| **K7-2** | MED | (L) liveness | Time-only bounds regress the dead-peripheral guarantee. `poll_wait_halt`, `wait_halt`, `wait_for_uframe` and `pl011_write` were converted in this range from hardware-independent spin counts to `systimer_us() - start > BUDGET`. If the 1 MHz System Timer never advances, that condition is never true and the loop is unbounded. `timer::delay_us` keeps a `DELAY_SPIN_CAP` backstop for exactly this reason; these did not inherit it. Worst on `pl011_write`, which sits on the fault/abort path - a dead timer plus a held `SERIAL_BUSY` deadlocks the panic reporter itself. Same class as Audit 6's A6-3. | **RECORDED** with K7-1 (same fix shape: keep the time bound as primary, add an iteration ceiling as backstop). No trigger: QEMU and the Pi 2 both advance the System Timer. |
| **K7-3** | MED | (C) correctness | **A short data phase with a GOOD verdict was accepted as a successful transfer.** `bot_command` decoded the CSW's `dResidue` and used it ONLY in the failure log; `ok` tested signature, tag and status but never the byte count. A device may legally return 100 bytes of a 512-byte read with status "passed" and residue 412 (BOT case 5/6, and the ordinary degraded behaviour of a flaky stick) - `bulk_xfer` copies 100 bytes, `bot_command` returns true, and `fs` receives 412 bytes of stale zeros PRESENTED AS DATA. Silent corruption arriving through the device's verdict rather than the DMA buffer, which is the one failure `dwc2.rs` states this driver must never produce. | **FIXED** - the data stage's moved byte count is kept and the CSW check now requires `residue == 0 && moved == dlen`; a short transfer is a failed transfer and recovers as a framing fault. |
| **K7-4** | MED | (C) restart | `block-driver`'s ARM core-0 pin was BOOT-ONLY. The supervisor forced core 0 at boot, but the kernel `ServiceConfig.preferred_core` said `1` unconditionally and the restart path passes no override - so any respawn landed it on core 1, where every `msc_*` entry point refuses on `!on_core0()`. Storage would die permanently on the first `chaos kill-storm block-driver`, and §22 Test 11 kills a restartable service deliberately. Invariant 6 broken on ARM by one value disagreeing with itself across two code paths. | **FIXED** - `preferred_core: if cfg!(target_arch = "arm") { 0 } else { 1 }` (the idiom `nic-driver` already used), supervisor override removed. Placement now lives in ONE place, consulted by boot and restart alike. Verified: block-driver spawns on core 0 with no override. |
| **K7-5** | MED | (P) portability | `s390x` lacks all five storage `arch::imp` primitives (`emmc_base_clock_hz`, `usb_disk_sectors/read/write/flush`), which neutral `dispatch.rs` calls unconditionally - so the neutral kernel does not compile there. **Pre-existing** (verified at `6929e28`: zero of them present), widened by one in this range. Invisible because CI builds x86_64 only and `arch_boundary_check.py` tests for named-arch references, not surface completeness - so `docs/multi-arch.md`'s "s390x compiles" endian-neutrality claim was stale. | **FIXED** - five stubs added. The structural gap (nothing checks seam completeness) is noted for a future guard. |
| **K7-6** | LOW | (C) | `clean_invalidate_dcache_range` (`page_tables.rs`) uses `saturating_add` for `end` then `p += 32`; at the top of the address space `p` overflows to 0 (release builds have overflow checks off) and the loop never ends. The sibling `flush_dcache` uses wrapping arithmetic and exits naturally. No trigger (Pi 2 frames are all below `0x40000000`), but it is the NEW code that hangs. | **FIXED** - `while p < end && p >= addr` wrap guard. |
| **K7-7** | LOW | (D) doc drift | Three doc-vs-code drifts introduced in this range: (a) new functions were inserted between `hw_random`'s doc block and its item, so the block documented `emmc_base_clock_hz` and `hw_random` had none; (b) `msc_write_block`'s doc asserted FUA behaviour in the present tense while `USE_FUA = false` makes that branch unreachable; (c) `MSC_NO_FLUSH`'s "cleared nowhere" comment justified itself with "a fresh device re-enumerates through `msc_select`", which is false - `msc_select` only re-points the channel and clears nothing. | **FIXED** - (a) doc reattached to its own item; (b) reworded to state FUA is off and why the rationale is deliberately kept; (c) the latches are now genuinely cleared on enumeration and the comment says so. |
| **K7-8** | INFO | (P) | `usb_disk_read/write` take a `u64 lba` that the arm32 single-register ABI truncates to 32 bits with no wrapper clamp - the A-U1 class this same range fixed for `resource_invoke`. Safe today only by coincidence: `READ CAPACITY(10)` caps `MSC_SECTORS` at 2^32 and the bounds check precedes the `as u32`. Worth an explicit note before any `READ(16)` / >2 TiB support. | Recorded. |
| **K7-9** | INFO | (A) | `USB_DISK` is minted `Rights::WRITE` only, and all four handlers - including the read and the capacity query - demand `WRITE`. A read-only block consumer cannot be expressed; the §7.4 READ/WRITE distinction is collapsed for this resource. | Recorded (no current consumer needs it). |

**Verified sound (no violation):** cap-before-action holds for all four `UsbDisk` handlers and for
`handle_resource_invoke` (validated before any hardware touch or user-memory access; `USB_DISK_RESOURCE`
is registered unconditionally so `mint_cap` cannot expect-panic; the mints unwind via
`cleanup_partial_spawn`). The `resource_invoke` repack is exact on both sides (`right<<24 | reply<<12 |
file` against masks `0xFFF`, `0xFFF>>12`, `0xFF>>24`), totals 32 bits so nothing truncates on arm32, and
`MAX_CAPS_PER_TASK` (64) cannot alias into `right` (4095 per field); a hostile `packed` yields only an
out-of-range slot, which `CapTable::get` turns into `CapNotHeld`. No other syscall argument is packed
above bit 31. Unsafe is clean (830 lines, no unaccounted additions); every new `asm!` is in `arch/` with
a SAFETY comment, including `stub_dabt`'s `cps #0x1f` SP_usr capture (`r0-r3`/`r12` are unbanked and the
I bit is untouched). Both descriptor walkers clamp to the 64-byte buffer, break on `blen == 0`, and
range-check before every field read; the 16-bit MPS parse is correct; `ctrl_xfer` clamps the programmed
DMA length on both directions; `bulk_xfer`'s OUT path sends `min(n, data.len())` so leftovers can never
be transmitted. The `switch_context` `dsb`/`isb` bracketing and `publish_user_pages_to_other_cores` are
a correct weak-memory fix (set/way is core-local, MVA is broadcast - the SEC-26/27 class), with a
bounded L1/L2 walk over kernel-built tables only. `hid.rs` is pure logic with provably in-range
indexing; `fbcon`'s CSI state machine is total and cannot escape the escape state.
`arch_boundary_check.py` passes.

**Cross-arch:** x86_64 was NOT broken by this range - services, kernel and `osdev` all build clean,
20/20 contracts validate, and no third party encodes syscall 31's argument (the only encoder is the SDK,
the only decoder the kernel). Confirmed against a clean worktree of the base commit.

**Also fixed during this audit (a test defect, not a kernel one):** three `capability::table` proptests
had been permanently red, sweeping `id in 0..DIRECT_CAP` and thereby fabricating STABLE gate resource
ids (< 100) whose `bump_generation` the kernel correctly refuses (SEC-11). The kernel was right every
time; the generators now start at 100. `97 passed, 0 failed`. Three always-red tests train a reader to
see `3 failed` as the normal state, which is how a real regression walks in unnoticed (§22.4: a test
failing for the wrong reason is a failure of the test, not the kernel).

---

## Audit 8 (2026-07-26, `feat/pi2-arm32` @ `f723e7a`) - the USB recovery layers, console foreground, and 32-bit VA work

Scope: `74ee6ff..HEAD`, 34 commits / ~1300 lines, none previously audited. Four independent auditors
(dwc2 transport; the rest of the arm layer + neutral `syscall/`+`task/`; guards + doc drift; plus the
userspace pass recorded as userspace Audit 7). Every finding below was traced to code before being
recorded; findings that could not be given a concrete failure scenario were discarded.

**Mechanical guards: all PASS** (`unsafe_check`, `arch_boundary_check`, `contract_check`, `dash_check`,
unit tests 97/0, `osdev validate` 20/20). x86 identity **24/0** after the fixes. `PYTHONIOENCODING=utf-8`
set, verdicts taken from exit codes, not from the absence of the word FAILURE (the trap from Audit 7).

**Unsafe inventory: TRUE.** Per-file counts independently recounted and matched (`dwc2.rs` 14,
`arm/mod.rs` 43, `fbcon.rs` 6, `exceptions.rs` 24, `page_tables.rs` 31); grandfathered floors held
(`scheduler.rs` 34/37, `dispatch.rs` 1/2). Every block carries a `// SAFETY:` comment.

### FIXED in this audit (all HIGH, all introduced by the work being audited)

| ID | Finding |
|----|---------|
| **A8-1** | **A u64 LBA truncated to 32 bits on the ARM syscall ABI - silent corruption.** The kernel's `lba >= MSC_SECTORS` guard runs on the value it *received*, so `0x1_0000_0000` arrived as 0, passed, and overwrote the superblock with every layer reporting success. Rejected in the SDK wrapper per hazard A-U1. |
| **A8-2** | **`-2` meant both "device busy" and `CapNotHeld`.** A driver missing its `USB_DISK` cap was retried 6000 times and then reported as a device that "stayed busy" - an authority failure wearing an I/O failure's name, with `fs` degrading storage on it. BUSY moved to `-20`, outside the cap range, named on both sides. |
| **A8-3** | **The data-stage failure could never escalate.** It ran a bare `bot_recover` and discarded the verdict, so it never bumped the failure streak and never reached `revive_if_needed` - the revival machinery was unreachable from the likeliest place for a block transfer to die. Now matches its two siblings. |

### OPEN - recorded, not yet fixed (ranked; all CONFIRMED unless noted)

**Boundedness of the recovery path (the theme).** `revive_if_needed` gave `reset_port` a second caller,
and that one is reached from a syscall, which runs with **IRQs masked on core 0**. Waits written when
they were boot-only are now runtime core-holds:

- **A8-4 (HIGH).** A revival runs the whole boot enumeration inline: `>1 s` of masked-IRQ core hold
  (port-enable poll 2M MMIO reads; `bPwrOn2PwrGood` up to 510 ms and **device-supplied**; 60 ms per hub
  port). Repeats every 4 failed commands. §26.6 and `arm/CLAUDE.md`'s "every hardware wait bounded".
  Fix: drive the revival from the core-0 tick as a bounded state machine, not inline.
- **A8-5 (HIGH).** `smsc_mii_wait` is bounded by **100,000 USB control transfers**, not by time - roughly
  100 s, and there are six such call sites. Boot-only before; on the runtime path now.
- **A8-6 (HIGH).** `CORE_HOLD_US` (5 ms) is checked *after* `split_txn` returns, and `split_txn` has no
  internal deadline: worst case ~25 s, realistic NYET storm ~2.2 s. This is the path a full-speed stick
  behind the Pi's hub actually uses. `split_txn_periodic` already does this correctly via `poll_wait_halt`.
- **A8-7 (MED).** `CORE_HOLD_US` is taken per *chunk*, not per transfer: a 512-byte stage is 8 chunks, so
  one BOT command holds the core ~50 ms even when nothing is wrong.

**Recovery that reports success it did not achieve:**

- **A8-8 (HIGH).** A *successful* revival can leave the keyboard and NIC dead permanently: `KBD_READY`/
  `NET_READY` are cleared before the port reset and restored only on the **failure** path. A stick that
  re-enumerates while the keyboard's descriptor read fails once returns early with the console's only
  input device switched off until reboot. Commandment V / §26.7.
- **A8-9 (MED).** A failed revival re-asserts `KBD_READY`/`NET_READY` without clearing `KBD_ADDR`/`KBD_EP`,
  but enumeration restarts address assignment at 2 - so the keyboard poll can end up issuing an
  interrupt-IN on the **mass-storage device's bulk endpoint** and pushing block payload into the console
  input ring as keystrokes.
- **A8-10 (MED).** `revive_if_needed` can re-enter enumeration *from inside* enumeration (`MSC_REVIVING`
  guards a nested revival, correctly - nothing guards a nested walk), leaving the outer walk with a stale
  `next_addr` and re-resetting ports the inner walk already enumerated. Same class as `8e48bed`.
- **A8-11 (LOW).** The CSW-bad path still discards `bot_recover`'s verdict (the twin of A8-3).

**Short transfers presented as complete data** (both pre-existing, both the class `bot_command`'s residue
check fixed for the bulk path in this range):

- **A8-12 (MED).** `ctrl_xfer` never checks how many bytes the DATA stage delivered, so any descriptor can
  be parsed from the previous transfer's leftovers in the shared DMA buffer.
- **A8-13 (MED).** `poll()` copies 8 bytes of an interrupt-IN without checking the residue; a short HID
  packet yields stale disk bytes decoded as keycodes.

**Console foreground (the mechanism added this range):**

- **A8-14 (MED). FIXED (2026-08-01, `refactor/fbcon-neutral`).** `fbcon::clear_and_home` took no lock and
  its SAFETY comment asserted a precondition ("core 0 holds SERIAL_BUSY") that its only caller did not
  satisfy - two live `&mut` to the same `static mut` across cores. x86's equivalent took `FB.lock()`.
  §18.3: a SAFETY comment must be true. Fixed by *deleting the divergence*: the two consoles are now one
  neutral module (`kernel/src/fbcon/`) behind a single `SpinLock`, so ARM's `clear_and_home` takes the
  same lock x86's always did. The `static mut FBCON` - and with it the untrue SAFETY comment - is gone.
  ARM additionally keeps a single-writer gate at the mirror (`arch/arm/fbcon.rs`), so the tick ISR
  logging mid-render skips its mirror rather than re-entering a non-reentrant lock.
- **A8-15 (MED).** The lease-lapse path uses an unconditional `store`, not a CAS, so it can clobber a
  claim established between the read and the store - destroying a fresh owner's claim. Every other
  mutator on that static uses compare-exchange.
- **A8-16 (MED).** ARM has a 45 s lease; x86's same `arch::imp` primitive is perpetual. SEC-27 requires a
  primitive to owe a documented *semantic*, not just a signature. The owner is never told its claim
  lapsed, so it cannot re-establish (§26.7, Commandment IX). Note the 45 s bound now sits under a 30 s
  storage timeout raised in the same range - two such waits exceed it.
- **A8-17 (LOW).** fbcon terminal state (`reverse`, CSI parser) is not reset on release/death, so a task
  killed mid-`ESC[7m` leaves the TV inverted until reboot.
- **A8-18 (LOW).** The lapse notice goes to serial only - the same defect that disqualified the magic-key
  alternative it replaced ("not where an operator at the TV is sitting"). Also emits a bare `\n`.
- **A8-19 (LOW).** `InspectKernel` query 13 is ungated yet can now release a foreground claim and wake
  another task: a read that writes (§26.4).

**Other:**

- **A8-20 (MED, PLAUSIBLE).** The user-fault handler calls `task_stat`, which takes the routing-table
  spinlock and does three RTC reads, to fetch one `&'static str` - inside an abort handler with IRQs
  masked. Under lock contention a single task's fault can escalate to a kernel-wide wedge, inverting
  §10.4. Fix: a lock-free `task_name(slot)`.
- **A8-21 (LOW).** `systimer_us()` is 32-bit (wraps ~71.6 min) and `CONSOLE_FG_RENEWED_US` is `Relaxed`
  on a weak-ordered A7 - should be Release/Acquire per SEC-25.
- **A8-22 (LOW).** Two stale comments this range's own changes falsified: `dispatch.rs:1301` SAFETY says
  "task heap range (0x1_0000_0000+)", false on 32-bit since the split; and `map_in_active_tables(virt: u64)`
  silently narrows to `u32` with no assert - the primitive behind the bug `f886d6b` worked around.

## Audit 7 - the AArch64 (Raspberry Pi 4) port (2026-08-05, `feat/pi4-aarch64` @ f6e51efc)

**Scope.** All aarch64 kernel code written during the Pi 4 bring-up: `genet.rs` (GENET ethernet),
`xhci.rs` (USB over PCIe, incl. the new BOT recovery), `mmu.rs` (kernel-stack guard pages),
`ptables.rs`, the `mod.rs` arch surface, and the aarch64 capability grants in `task/mod.rs`.

**Method.** By DEFECT CLASS, not by file. The port's own standing lesson is "fix the class, not the
instances", and the first finding arrived by accident while chasing a bug - which is the argument for
sweeping deliberately.

**North star unchanged:** nothing above the kernel may panic or wedge it.

### Findings (5 CONFIRMED, all fixed)

| # | Finding | Class | Where |
|---|---------|-------|-------|
| A7-1 | `drain_rx` looped `while cons != prod` with `prod` read from a HARDWARE REGISTER - a device-supplied value deciding when kernel code stops | unbounded (§26.6) | `genet.rs` |
| A7-2 | A new address space was installed before its descriptors were visible. **The supervisor-respawn fault** (ESR `0x82000007`) | weak-memory publication (SEC-25/27) | `mod.rs::finalize_service_address_space` |
| A7-3 | `map_in_root` published nothing while mapping into the tables THIS CORE EXECUTES UNDER | weak-memory publication | `ptables.rs` |
| A7-4 | `unmap` released a `Frame` to the allocator with no barrier and **no TLB invalidate** - a use-after-free the MMU actively enables, reported by nothing | stale translation (SEC-26) | `ptables.rs` |
| A7-5 | Event drain unbounded in the TIMER TICK - an IRQ-storm livelock. Same class arm32 Audit 6 already recorded HIGH (`net_rx_isr`) | unbounded (§26.6) | `xhci.rs` |

`ptables.rs` - the file the neutral kernel maps and unmaps THROUGH - contained **zero barriers**.
A7-2/3/4 are one class; fixing only A7-2 (the one causing the visible bug) would have left the
use-after-free live.

### Classes swept clean

- **USB descriptor parsers** - all three guard `len < 2` and break, so a zero-length descriptor ends
  the walk rather than spinning it (the classic USB parse bug, already handled).
- **Device-supplied lengths** - every copy is fixed-size or `.min()`-clamped against BOTH source and
  destination. No device value sizes a copy unchecked.
- **Capability grants** - every aarch64 grant is arch-gated AND name-scoped to exactly one service;
  the `SetClock` rights split is correct least-privilege (WRITE to `net-stack`, READ-only floor to
  `shell`).
- **Bounded waits** - `command()` and the transfer waits carry explicit iteration bounds.
- **Userspace reply sends** - the discarded `send_by_handle(reply, ..)` result is the established
  convention across all three block backends (ahci 8, sdhci 7, usbdisk 6): a failed reply means the
  caller is gone and there is nothing to retry. Reviewed and accepted, not a finding.

### Verification

`chaos max-carnage`: 23 rounds, 77 kills, 53 flooded, 23 mem-pressure, 23 spawns. Kernel ALIVE,
**0 panics, 0 exceptions, 10 supervisor respawns all reconciled**, shell back at a prompt. Before
A7-2/3/4 the supervisor faulted on the first or second respawn.

### NOT audited (honest coverage)

- `mmu.rs` block splitting, line by line. Written the same day, page-table surgery, and verified only
  by the `guard_unmapped=true usable_mapped=true` line - which proves ONE page, not the general case.
- `bot_recover` against the xHCI specification. Built, and **never exercised**: no timeout has occurred
  since it landed, so `stop -> reset -> set-dequeue` has not run once.
- **Open suspicion, unresolved:** the BOT class reset and both clear-halts are refused over EP0. The
  scratch control ring is shared with enumeration, so the disk's EP0 context may no longer point at the
  ring this code rings a doorbell for. That is a design question, not a tuning one.
- Userspace beyond the block backends: the shell's clock-floor path and `net-stack` were not read.

### Process finding

Every one of A7-1..A7-5 is in code written the same day it was audited, and three sat on the path a
hardware test was about to exercise. The standing rule is to audit new code against the Commandments
BEFORE presenting it for a hardware test; that did not happen once across a driver, a recovery path,
guard-page block splitting and four capability grants. Bugs then chased over multiple hardware rounds
were audit-shaped: a discarded verdict, a bound that was not a bound, a diagnostic that could not fail
loudly. A7-5 had already been recorded as HIGH on arm32, on a different device, and was written again.



## Audit 8 - the AArch64 branch at merge readiness (2026-08-12, `feat/pi4-aarch64` @ `94711fd2`)

Scope: everything on this branch since kernel Audit 7 (`f6e51efc`) - ~850 changed kernel lines
(aarch64 `pcie.rs`, `exceptions.rs`, `gic.rs`, `mod.rs`, `interrupt/route.rs`, `task/`,
`syscall/dispatch.rs`) plus the deletion of `arch/aarch64/xhci.rs` (-2476).

**Mechanical gates: PASS.** `scripts/unsafe_check.py` - 71 audited files, 1055 unsafe lines, no
unaccounted additions. The deleted in-kernel xhci driver is gone from the inventory (the stale row
that once passed silently is now a hard failure, `e2810e00`).

### K8-1 (LOW) - `fcap-diag` instrumentation outlived the investigation it was for

`kernel/src/syscall/dispatch.rs`, the `#[cfg(feature = "fcap-diag")]` block, is introduced by its own
comment as *"FCAP-RESTART INSTRUMENTATION (temporary - remove when the post-restart escalation is
closed)"*. That escalation was closed (SEC-35). The code is feature-gated so it does not ship, but a
block that documents its own removal condition and then outlives it is how dead scaffolding becomes
permanent - and it prints capability rights, which is exactly the kind of diagnostic that should not
be one `--features` away in a shipped kernel.

### Verified sound

- **EL1 fault -> halt** (`exceptions.rs:689`) is the correct end of that path, not a missing recovery:
  a fault at EL1 IS the kernel's, and there is nothing above it to recover into (§26.7). The comment
  says so explicitly.
- **`pcie.rs` link-up wait** is bounded (`while ms < 100 && !link_up()`), and its delay helper is
  bounded on the cycle counter rather than an iteration count - the "a count is not a duration"
  lesson applied correctly.
- **No new syscall** was added on this branch, so the cap-gate surface is unchanged.

---

## Audit 9 - the reply-endpoint id leak, found by BS5 (2026-08-29, `feat/trace`)

Not a sweep. One bug, found by running a suite that had been red long enough to be assumed flaky, and
recorded here because the shape of it is worth keeping.

### K9-1 (HIGH, FIXED) - a spawn took TWO endpoint ids and a death gave ONE back

`kernel/src/task/scheduler.rs` (death path) and `kernel/src/task/mod.rs` (spawn path).

Every task gets a primary receive endpoint and an optional reply-only endpoint, each with an id from
`ipc::alloc_endpoint_id`. The death path reclaimed the primary (`free_endpoint_id(ep_id)`) and never
the reply one; the spawn path leaked the reply id outright when `try_register_optional` refused it.
So every restart leaked one id, and a sustained restart storm marched the monotonic counter into the
delegated/file-cap band, where the guard did exactly what it was written to do:

```
KERNEL PANIC: endpoint id space exhausted (reached the delegated/file-cap band at 4096)
```

**A userspace restart storm panicked the kernel** - the one outcome nothing above the kernel is
allowed to cause. Reproduced by `osdev test stress` BS5 (5000 kill/respawn cycles), which died at
~1744.

Two things are worth separating here. The reclaim itself was written deliberately, with a comment
naming this exact failure ("without this the id counter only climbs and a sustained restart storm
exhausts the band and panics") - and it was still only half done, because the reply endpoint was
added later and the reclaim was not revisited. **A resource that is allocated in two places must be
freed in two places**, and the review question that catches it is not "is this freed?" but "is every
allocation site matched?".

The second is that the bound WORKED. The counter had a guard, the guard was loud, and it named the
band it hit - which is why this was a two-minute diagnosis from a log line rather than an
investigation. The failure mode a silent version of this produces is an id handed out twice.

Fixed by reclaiming the reply id on death (safe to free immediately - the ordering care the primary
needs is about the NAME directory, and a reply endpoint has no name) and on refusal.

### K9-2 (MEDIUM, FIXED) - the console input ring was sized below one command line

`kernel/src/arch/x86_64/mod.rs`, `COM1_RX_BUF_SIZE = 64`, against a reader whose line buffer is 256.

A line longer than 64 bytes typed or pasted while the shell was finishing the previous command lost
its tail, including the CARRIAGE RETURN, so the next command silently joined the mangled line. This
is what `osdev test files` had been failing on - 40 checks red from one dropped byte. aarch64 has
been 256 since its ring was written; x86 was the inconsistent one, exactly as it was for the drop
COUNTER a few commits earlier. Sized to `MAX_LINE`, which is the derivation: the ring must survive
the reader being busy for as long as one command takes.

The drop was already counted and reported (that counter was added a few commits before this), and
that report is the entire reason this was findable - the transcript said `console input ring FULL`
one line before the mangled command. An instrument added for one bug paid for itself on the next.

### K9-3 (LOW, FIXED) - two stress probes asked a question a dead service cannot answer

`services/probe/src/main.rs`, S4 and BS4. Both killed the victim and THEN read its endpoint
generation by name - but death unregisters the name (`names::unregister_endpoint`, the
post-max-carnage fix), so the lookup misses and the query returns 0. The checks compared 0 against
the previous generation and failed for a reason that has nothing to do with generations; they only
ever passed by winning a race against the death path. Fixed by reading while the instance is alive,
between the spawn and the kill - which is also the stronger question, and the one §7.5 actually makes
a claim about. §22.4: a test failing for the wrong reason is a failure of the test.
