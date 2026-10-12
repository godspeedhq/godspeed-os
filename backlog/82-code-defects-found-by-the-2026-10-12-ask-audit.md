# 82. Code defects found by the 2026-10-12 `feat/ask` documentation audit

**Status: OPEN - recorded, not fixed.** The 2026-10-12 audit of `feat/ask` changed documents and comments
only; the code defects it found are listed here and fixed separately, as for `backlog/80`. None of them
blocks a hardware card for the branch. Two `dwc2` paths that answer a block request `STATUS_ERR` while
a disk is bound belong to `backlog/57` and are recorded there instead.

## Kernel

- **K1. AArch64 reports an UNKNOWN address for an SP alignment fault.** `kernel/src/arch/aarch64/exceptions.rs`,
  the death-cause match: EC `0b100110` (SP alignment) maps to `(F_ALIGNMENT, far)`, but the architecture
  leaves FAR UNKNOWN for that exception, so the death notice and the supervisor's "died, restarting" line
  can carry a garbage address. Fix: use `f.elr`, as the PC alignment arm (`0b100010`) does. A kernel
  change, so QEMU on the Pi 4 first and the operator's go-ahead.

## SDK

- **S1. Two `AUTHORITIES` descriptions describe devices that no longer exist.** `sdk/rust/src/service_context.rs`:
  10 `net_device` "move raw ethernet frames on the ARM USB network device" and 12 `usb_disk` "read and write
  any sector of the ARM USB disk". Both were in-kernel devices; the USB stacks are services now and nothing
  is granted either. The strings are user-facing (`caps`, `ask "who can ...?"`). Fix: reword as `set_clock`'s
  entry is, "(granted to nothing now)".

## `ask`

- **A1. Three answers still say a death's cause is unrecorded.** `services/ask/src/know.rs`: `health`
  ("why each restart happened is not recorded anywhere this can read"), `failed` ("... and the record does
  not say which") and `wifi` ("why `<driver>` died is not recorded ..."). Since `e67fe072` the supervisor
  answers `supcmd::DEATH` with killed-by-whom or the fault, for every watched service. Fix: ask DEATH for
  each restarted service, or say the reason is not asked here and point at `ask "why did <service> fail?"`.
- **A2. `who can` leaves out a task whose capabilities could not be read.** `services/ask/src/main.rs`, the
  `Kind::WhoCan` arm: `task_caps` returning 0 is read as "holds nothing", while `capabilities()` treats 0 as
  a failed read. The answer can omit a holder silently. Fix: count failed reads and report them as a hole.
- **A3. The help forms omit questions that work.** `can_answer` (`know.rs`) omits the network question and
  what is / what can / who can; `guide()` omits "what is <term>?"; the shell's `ask` help block has no rows for
  what is / what can / who can, while `utilities/60_ask.md` 1 says `ask help` lists what it can be asked.
  Fix: add the missing forms.
- **A4. `VOCABULARY` is never read and was not bumped.** `know.rs`: the constant is 1 although words were
  added, and nothing reads it (its comment now says so). Fix: pin it in a test and bump it, or delete it.
- **A5. A test is named for the opposite of what it checks.** `storage_chain_names_the_first_link_that_stops`
  (`know.rs`) checks that the DEEPEST link is named. Fix: rename it.
- **A6. `STORAGE_HOSTS` lists `ehci`, which no board gives `ask` as a peer.** `main.rs`: dead today; if it
  ever matched, the request would fail for want of a peer and be reported as "did not answer in time" when
  it was never sent. Fix: drop it, or report a missing peer as its own fact.
