# 82. Code defects found by the 2026-10-12 `feat/ask` documentation audit

**Status: CLOSED 2026-10-13.** Every item was fixed in code on `feat/ask` on 2026-10-12, and the branch's
hardware cards have passed: `chaos max-carnage all-services 1000` then `selfcheck` with 0 failed on the
Pi 4, Pi 2, VisionFive 2 Lite, Dell Wyse 5070 and HP T630 (released as v0.26.0).

*Was:* FIXED IN CODE on `feat/ask`, 2026-10-12; open until the branch's hardware cards pass. The
2026-10-12 audit of `feat/ask` changed documents and comments only (`9ce5c479`); the code defects it found
were listed here and fixed separately the same day, as for `backlog/fixed/80`. Every item carries a FIXED note.
Two `dwc2` paths that answered a block request `STATUS_ERR` while a disk could be bound belong to
`backlog/57` and are recorded (and fixed) there.

## Kernel

- **K1. AArch64 reports an UNKNOWN address for an SP alignment fault.** `kernel/src/arch/aarch64/exceptions.rs`,
  the death-cause match: EC `0b100110` (SP alignment) maps to `(F_ALIGNMENT, far)`, but the architecture
  leaves FAR UNKNOWN for that exception, so the death notice and the supervisor's "died, restarting" line
  can carry a garbage address. Fix: use `f.elr`, as the PC alignment arm (`0b100010`) does. A kernel
  change, so QEMU on the Pi 4 first and the operator's go-ahead.
  **FIXED:** both alignment arms report `f.elr`. Shown by reading, not by a fault: nothing in the tree
  takes an SP alignment fault on purpose, so the Pi 4 QEMU check is that the memory-fault path it shares
  a match with still names its kind and address.

## SDK

- **S1. Two `AUTHORITIES` descriptions describe devices that no longer exist.** `sdk/rust/src/service_context.rs`:
  10 `net_device` "move raw ethernet frames on the ARM USB network device" and 12 `usb_disk` "read and write
  any sector of the ARM USB disk". Both were in-kernel devices; the USB stacks are services now and nothing
  is granted either. The strings are user-facing (`caps`, `ask "who can ...?"`). Fix: reword as `set_clock`'s
  entry is, "(granted to nothing now)".
  **FIXED:** both say "an in-kernel ... (none exists; granted to nothing now)".

## `ask`

- **A1. Three answers still say a death's cause is unrecorded.** `services/ask/src/know.rs`: `health`
  ("why each restart happened is not recorded anywhere this can read"), `failed` ("... and the record does
  not say which") and `wifi` ("why `<driver>` died is not recorded ..."). Since `e67fe072` the supervisor
  answers `supcmd::DEATH` with killed-by-whom or the fault, for every watched service. Fix: ask DEATH for
  each restarted service, or say the reason is not asked here and point at `ask "why did <service> fail?"`.
  **FIXED** the second way: each now says how the service ended is not asked HERE and names the question
  that reads it. Asking DEATH for every restarted service from the general answers is left undone - one
  more round trip per restarted service, against the Pi 2 budget.
- **A2. `who can` leaves out a task whose capabilities could not be read.** `services/ask/src/main.rs`, the
  `Kind::WhoCan` arm: `task_caps` returning 0 is read as "holds nothing", while `capabilities()` treats 0 as
  a failed read. The answer can omit a holder silently. Fix: count failed reads and report them as a hole.
  **FIXED:** `Holders { held, unread }`; each unread table is an UNKNOWN line and the verdict says how
  many could not be read ("AT LEAST 2 TASK(S) CAN ..."). Host-tested.
- **A3. The help forms omit questions that work.** `can_answer` (`know.rs`) omits the network question and
  what is / what can / who can; `guide()` omits "what is <term>?"; the shell's `ask` help block has no rows for
  what is / what can / who can, while `utilities/60_ask.md` 1 says `ask help` lists what it can be asked.
  Fix: add the missing forms.
  **FIXED:** all three now list the network, what is, what can, who can and terms.
- **A4. `VOCABULARY` is never read and was not bumped.** `know.rs`: the constant is 1 although words were
  added, and nothing reads it (its comment now says so). Fix: pin it in a test and bump it, or delete it.
  **FIXED:** deleted; the tests are what pin the vocabulary, and `docs/ask-design.md` 4 says so.
- **A5. A test is named for the opposite of what it checks.** The storage-chain test in `know.rs` was named
  for the FIRST link that stops, and checks that the DEEPEST link is named. **FIXED:** renamed
  `storage_chain_names_the_deepest_link_that_stops`.
- **A6. `STORAGE_HOSTS` lists `ehci`, which no board gives `ask` as a peer.** `main.rs`: dead today; if it
  ever matched, the request would fail for want of a peer and be reported as "did not answer in time" when
  it was never sent. Fix: drop it, or report a missing peer as its own fact. **FIXED:** dropped.
