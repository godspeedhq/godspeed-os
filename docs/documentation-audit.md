<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Documentation Clarity Audit

> **Living document.** Records every audit of the *documentation* - `CLAUDE.md`, `COMMANDMENTS.md`,
> the SDK and per-directory `CLAUDE.md` files, the examples, and `docs/` - for clarity and intent.
> Re-run and append with each audit. This is the third of the audit trilogy: the kernel has
> `docs/kernel-audit.md`, userspace has `docs/userspace-audit.md`, and the docs themselves have this.
> First audit: 2026-07-15.


## Audit 4 - drift left behind by deleting the in-kernel aarch64 USB driver (2026-08-09, `feat/pi4-aarch64`)

**Scope:** everything that described USB on AArch64. Commit `e71e64a6` deleted
`kernel/src/arch/aarch64/xhci.rs` (2742 lines of ring-0 USB stack) and the two cargo features that
used to select between it and the userspace `services/xhci` (`xhci-userspace` in the kernel and the
supervisor, `usb-via-xhci` in `block-driver`). ARM32 (Pi 2) is deliberately unchanged. This audit asks
one question of every doc, comment and manifest that mentioned the old arrangement: **is it still
true?**

**Verdict: 5 HIGH, 5 MED, 3 LOW.** The constitution itself was amended correctly and in the same
commit - `CLAUDE.md` §6.4's 2026-08-09 amendment is present, accurate, and explicitly keeps ARM32
separate rather than blurring the two ports. The drift is everywhere *around* it. The dominant shape
is new and worth naming: **the deletion was surgical on code and blunt on prose.** Three `[features]`
blocks had their feature *declaration* line removed and their multi-paragraph *rationale* left in
place, so three manifests now end a sentence mid-word and document a build option that does not
exist. This is the DA3-2 lesson from the 2026-07-31 audit repeating at commit scale: a stale comment
about a FINISHED stage reads as present tense, and here it reads as a live `--features` switch.

The north-star failure is concrete and easy to state. A competent engineer coming cold and asking
**"how does USB work on the Pi 4 now?"** has no entry point: there is no `services/xhci/CLAUDE.md`, no
`kernel/src/arch/aarch64/CLAUDE.md`, the two design docs that would answer it are not in the docs
index, and the primary onboarding doc for the port still opens with "Status: design, not built".

### Ranked ledger

| ID | Sev | File:line | Finding |
|----|-----|-----------|---------|
| **A4-1** | HIGH | `kernel/Cargo.toml:105-141`, `services/block-driver/Cargo.toml:15-20`, `services/supervisor/Cargo.toml:18-21` | **Three orphaned, mid-sentence-truncated feature rationales for flags that no longer exist.** The commit removed the `xhci-userspace = []` / `usb-via-xhci = []` declaration lines and the sentence that named them, leaving ~30, ~6 and ~4 lines of present-tense justification behind. `kernel/Cargo.toml` ends "Build both sides with it" with a dangling open paren and falls straight into the next feature's comment; `block-driver/Cargo.toml`'s `[features]` section trails off at "and both halves build and" with no period before `[dependencies]`; `supervisor/Cargo.toml`'s block runs without a blank line into `blockdev`'s comment, so **`blockdev = []` now reads as the flag that "spawns the `xhci` service on aarch64"** - it is an unrelated persistence smoke-test flag. A reader reasonably concludes `--features xhci-userspace` is a current build option. Verified: zero `#[cfg(feature = "xhci-userspace")]` or `"usb-via-xhci"` remain in any `.rs`, so this is dead prose, not dead code. |
| **A4-2** | HIGH | `kernel/src/task/mod.rs:486-493` | **A false rationale sitting directly on top of a live capability grant.** The comment reads "on BOTH ARM ports the USB stack is in-kernel, so `block-driver` reaches a USB stick through syscalls 46-48 ... The Pi 4 needs it for the same reason the Pi 2 does." False for aarch64. The code below it is unchanged: `usb_disk: cfg!(any(target_arch = "arm", target_arch = "aarch64")) && matches!(name, "block-driver")`. On aarch64 the four syscalls that grant authorises are permanent stubs returning 0/false (`arch/aarch64/mod.rs:1277-1301`) and `block-driver` reaches the disk by IPC to the `xhci` service instead (`send_peers: &["xhci"]`, `task/mod.rs:716`). This is the worst class in this audit: the comment explains why an authority is needed, and the reason is gone. Filed as a security finding too - `security-audit.md` **SEC-37**. |
| **A4-3** | HIGH | `services/block-driver/src/xhciblk.rs:1-8` | **The module header states the opposite of the truth.** "As long as they are the only route to a disk, `kernel/src/arch/aarch64/xhci.rs` - 2742 lines of ring 0 ... - **cannot be deleted**, and Commandment I stays broken on this port." It has been deleted and Commandment I is closed on this port. The one module whose whole purpose is the new arrangement documents itself as the reason the old one could not be dismantled. |
| **A4-4** | HIGH | `docs/unsafe-audit.md:2322` | **A phantom row in the LIVE inventory, invisible to CI.** The `<!-- unsafe-inventory-start -->` .. `<!-- unsafe-inventory-end -->` table (lines 2299-2374) - the current, mechanically-parsed inventory, not a dated changelog - still carries a row reading `arch/aarch64/xhci.rs` / `42` / `permitted` for a file that does not exist, inflating the audited total by 42 lines. `scripts/unsafe_check.py` **passes**: it iterates real `.rs` files and checks each one's count is within its audited figure, and never checks the reverse direction (a row whose file is gone). So the enforcement mechanism `docs/CLAUDE.md` calls "special" has a blind spot exactly where a deletion lands. The dated changelog rows at 150, 204, 527, 566, 589, 610 and 657 are ratified history and are correct as they stand. |
| **A4-5** | HIGH | `services/xhci/src/main.rs:2204` (+ 1665, 2149, 2255, 2450, 2530) | **Six present-tense provenance references to the deleted driver, one of which asserts it still exists.** Line 2204: "Taken from the in-kernel driver **still in this tree** (`arch/aarch64/xhci.rs`)". The other five ("has always driven this exact VL805", "waits 200 ms here", "clears BOTH change bits", "reached the same conclusion", "refuses loudly above 512") all describe behaviour of code a maintainer can no longer consult. These read as load-bearing hardware-provenance notes - precisely the kind a reader will go looking for when a USB bug appears. The provenance is real and worth keeping; the tense and the path are not. |
| **A4-6** | MED | `kernel/src/syscall/CLAUDE.md:49` | **A doc that contradicts a shipped security fix.** The syscall table lists 18 `reboot` as "REBOOT (WRITE) - held by `shell` (its `reboot` cmd) + `xhci`/`ehci` (Ctrl+Alt+Del)". SEC-2 removed REBOOT from the USB drivers; the code is correct (`task/mod.rs:467`, `reboot: matches!(name, "shell")`) and the Ctrl+Alt+Del chord is routed through the shell via `hid::CTRL_ALT_DEL_SIGNAL` (SEC-2 follow-up, §6.4). A stale row in an authority table is worse than a stale prose line: it reads as the design, and a reader reasoning about who can reset the machine gets the pre-SEC-2 answer. Pre-existing (SEC-2 landed 2026-07-16), surfaced by this audit's authority sweep. |
| **A4-7** | MED | `services/block-driver/src/usbdisk.rs:184, 216-225, 284`; `services/block-driver/contracts/block-driver.toml:1-6, 27-30` | **Five sites describe a build-flag mechanism that has been replaced by an architecture check.** "chosen at BUILD time ... the kernel's `xhci-userspace` feature is what stops it driving the controller ... `scripts/pi4_build.py --xhci-userspace` sets both", and "The re-derive above is cfg-gated to `usb-via-xhci`". The real gate is `#[cfg(target_arch = "arm")]` vs `not(...)`, unconditional on aarch64. The contract additionally claims the service is granted `USB_DISK` "because the ARM USB stack lives in the kernel" (false for aarch64, see A4-2) and calls the `ipc_send = ["xhci"]` peer "present only when the USB stack runs in userspace ... this peer goes unused" - on aarch64 it is the only path to the disk and is exercised on every block request. No dead code results (verified), but a reader will search for a feature flag that is not there and misread the actual mechanism. |
| **A4-8** | MED | `kernel/src/arch/mod.rs:12-14` | The shared `hid` module is documented as "shared by the in-kernel USB drivers (`arch/arm/dwc2.rs` and `arch/aarch64/xhci.rs`)". Half of that is now false, and the half that matters is inverted: `hid` is now shared between an *in-kernel* driver (arm32) and a *userspace service* (`services/xhci`, via `sdk`). The sentence's own justification - "so the two ports cannot drift apart" - is still exactly right, which is why the stale naming is worth fixing rather than deleting. |
| **A4-9** | MED | `docs/aarch64.md:1-6` (and its row in `docs/CLAUDE.md`) | **The Pi 4 port's primary doc opens by denying the port exists.** "**Status:** design, not built. Non-normative until the constitution is amended ... Target board: Raspberry Pi 4 Model B, **4 GB**". The body from line 9 documents milestones 1-21 hardware-verified through a live `gsh>` prompt; the constitution **has** been amended (§6.4, 2026-08-09, on disk); and the board is **2 GB** (rev 1.5) by the port's own memory-map evidence in the same file. The milestone log also stops at Milestone 21 (2026-08-04) with no entry for the USB deletion. The one section that is *correct and valuable* is §4's "no usable SMMU ... so H1/§6.4 does not travel" - it is the honest posture statement the new §6.4 amendment does not make (see A4-10). |
| **A4-10** | MED | `CLAUDE.md` §6.4 (2026-08-09 amendment) vs §6.1 table + glossary | **The new amendment is accurate about the kernel and silent about the TCB, and a reader has to join two sections to get the right answer.** It says the driver is out of the kernel and Commandment I is closed - both true - but never states the trust consequence. §6.1's table row still governs and gives it: `xhci`/`ehci` are "in the TCB only on a machine with no IOMMU", and the Pi 4 has no usable SMMU (`arch/aarch64/mod.rs:2328-2334` - every `iommu::` entry point is a no-op stub, `confine_device` unconditionally returns `false`). So **the userspace `xhci` on the Pi 4 is a TCB member**, and the amendment reads as though it stopped being one. Separately, §6.4's standing promise that "which case holds is reported loudly at boot (invariant 12)" is **not met on this port** - nothing is printed in either direction (`security-audit.md` **SEC-34**). Both are one sentence each in §6.4. Related pre-existing drift found while checking: §12.1 still says "Essential drivers (block-driver) are trusted in v1" (Phase D dropped them) and the glossary's `TCB` entry still reads "Kernel + arch + smp + init + supervisor" (`init` was removed in Phase 5). |
| **A4-11** | LOW | `scripts/pi4_build.py:29-32` and `:132-135` | Two small self-contradictions in the build script. The docstring says the removed flag "had to reach **TWO** crates: the kernel ... and the `supervisor`", contradicting the same file at line 107 ("the kernel, the supervisor AND block-driver") and the commit message ("three crates"). And lines 132-135 are a **dangling comment with no code**: "The THIRD crate the one switch has to reach. block-driver must be told to fetch its sectors from the `xhci` SERVICE ... without it storage silently disappears" now sits directly above an unrelated `if svc == "net-stack" and EL0_FAULT_TEST:` branch. The scenario it warns about can no longer occur (the backend is `cfg(target_arch)`), so it is a warning about an impossibility, attached to the wrong code. |
| **A4-12** | LOW | `docs/CLAUDE.md` (index); absent `services/xhci/CLAUDE.md`, `kernel/src/arch/aarch64/CLAUDE.md` | **The discoverability gap, and the one this audit would fix first.** `docs/xhci-split.md` and `docs/xhci-topology.md` (both added 2026-08-09) are not in the docs index. There is no `services/xhci/CLAUDE.md` and no `kernel/src/arch/aarch64/CLAUDE.md` - note that `kernel/src/arch/arm/CLAUDE.md` exists precisely because DA4 of the 2026-07-23 audit found the same hole for the Pi 2 and called creating it "the single highest-leverage fix". The best available cold path to "how does USB work on the Pi 4?" today is a 800-line milestone log plus source comments, five of which are A4-5. |
| **A4-13** | LOW | `docs/xhci-split.md:8-9` | Cites `arch/aarch64/mod.rs:1756` and `:1290` for the in-kernel driver's `poll()`/`disk_read()`. The doc's framing is already past-tense and correct (it was written hours before the deletion), but the two line-number citations no longer resolve to anything. Design-history doc, so the cost is a reader's wasted lookup, not a wrong belief. |

### Verified still true (do not re-check)

- **`CLAUDE.md` §6.4's 2026-08-09 amendment** is present, accurate, and correctly scoped - it closes
  Commandment I on aarch64 without touching the 2026-07-23 ARM32 amendment below it, and says so
  explicitly ("the two ports now differ in exactly this, and the difference is recorded rather than
  blurred"). Its one gap is A4-10, which is additive, not a correction.
- **`kernel/src/arch/aarch64/mod.rs:1258-1301`** and **`kernel/build.rs:130-145`** are current and
  well-written: the former documents that the kernel drives no USB and why the `usb_disk_*` stubs
  answer "absent" rather than fabricating success; the latter correctly explains that the `xhci` ELF is
  embedded unconditionally and that the *supervisor* gates the spawn.
- **`services/block-driver/src/main.rs`** is fully updated - it documents the `cfg(target_arch)`
  backend split with no stale flag references. (Its sibling `usbdisk.rs` is A4-7.)
- **`kernel/CLAUDE.md`, `services/CLAUDE.md`, `COMMANDMENTS.md`** carry no USB-specific claims and
  needed no change.

### Pre-existing lag, recorded but NOT counted against this commit

`README.md:42` and `docs/multi-arch.md` still describe AArch64 as reaching a "boots + prints to UART"
milestone, and `kernel/src/arch/CLAUDE.md` still calls `arch/aarch64/mod.rs` a stub at that milestone.
These are weeks behind reality (full shell, USB service, GENET networking) but the lag predates this
work and is not USB-specific. Flagged here so the next `main` merge does not ship them - the DA2 fix
from the 2026-07-23 audit did exactly this job for arm32 and the aarch64 rows never got the same pass.


## Audit 3 - drift against the v0.9.0 console work (2026-08-03, `main`)

**Scope:** `docs/`, `CLAUDE.md`, and the per-directory `CLAUDE.md` files, checked for claims the code no
longer supports after the shared framebuffer console landed.

**Verdict: 1 LOW finding. No misleading architectural claims.**

| ID | Severity | Finding |
|----|----------|---------|
| A3-1 | LOW | `docs/console-service.md:177` enumerates the fbcon's ANSI support as "clear, cursor position, erase line, hide/show cursor". That list is now **incomplete**: the shared console also implements SGR reverse video (`ESC[7m`/`ESC[0m`), relative cursor movement (CUU/CUD/CUF/CUB), and erase-to-end-of-screen (`ESC[J`). An enumeration that silently falls behind the code is the drift this audit exists to catch - a reader planning console work would under-estimate what already exists. |

**Clean results:** `kernel/CLAUDE.md` (module map + the `fbcon/` section), `kernel/src/arch/CLAUDE.md`
(the porting checklist now names `fb_commit`/`FB_READBACK_CHEAP`), `kernel/src/arch/x86_64/CLAUDE.md`
(the file table), `docs/CLAUDE.md` (indexes `logging.md`), `docs/unsafe-audit.md` (counts re-baselined
to the 11 -> 3 reduction), and `docs/arm32-status.md` (the HAT section corrected from "it is the HAT" to
the measured cable evidence) were all updated with the work rather than after it. The shell comment
asserting reverse video is "a no-op on the fbcon" was corrected in the same commit that made it render.

## Audit 3 - stale documentation that actively caused wrong diagnoses (2026-07-31, `feat/arm-usb-interrupt`)

**North-star restated:** the least-capable reader, cold, should not have to GUESS. This audit found the
sharper failure mode: documentation that is not merely absent but **confidently wrong**, which is worse -
it is believed, and it redirects effort. Three instances, all of which cost real time in one session.

| ID | Sev | Finding |
|----|-----|---------|
| DA3-1 | HIGH (FIXED) | `arch/x86_64/CLAUDE.md` described `wait_for_interrupt()` as "`sti` only - no C-state hint". The code has TWO branches and **halts** whenever `IDLE_CAN_HALT` (AMD, or ARAT in periodic mode). A reader trusting that table concludes the kernel never halts at idle - and so never looks for a halt-related lost wake, which is precisely the BSP panic diagnosed the same day. FIXED: the entry states both branches, and a new **"The idle contract: never halt without a freshly armed wake"** section explains the one-shot TSC-Deadline hazard, which core arms what, and why the BSP differs. |
| DA3-2 | HIGH (FIXED, in-code) | `chan_dma` carried "until stage 2b, writes keep the spin path" long after stage 2b landed - `msc_write_block` passes `can_block=true`. Reading it as current produced a whole wrong diagnosis (a 30 s pause blamed on writes spinning with the core held, when the core was never held). A stale comment about a FINISHED stage is worse than none: it reads as present tense. Corrected @ ee457b3, along with `IO_BUDGET_US` still calling itself an IRQs-masked core-hold when on the async path it is a park timeout. |
| DA3-3 | HIGH (FIXED, in memory) | An engineering note claimed the T630 TSC PIT-calibration was still "todo" **three weeks after it landed** (77a0e38, 2026-07-07, in main). Believing it produced a confident, wrong root-cause for an x86 failure ("TSC 1000x off -> 30 s waits"); the panic's own numbers disproved it (5988751200/300 = 19,962,504 cycles per 10 ms = ~2 GHz, correct). Corrected, with the lesson attached: **check the code before trusting a note that says "todo"**. |

**The pattern worth naming.** All three are the same shape: a statement that was TRUE when written and
was never revisited when the code moved past it. None would be caught by a grokability review (each
reads clearly and plausibly) and none by CI. The only defence is that **a change which finishes a
staged piece of work must sweep the notes that described it as pending** - the doc equivalent of
Commandment III's "one truth". Cheap to state; this session paid for it three times.

**Also fixed this session (doc-drift found while auditing code):** the retired hot-plug starvation
diagnostic left no stale claims behind (its removal is recorded in the constant it left in place), and
`docs/unsafe-audit.md` tracked every `unsafe` delta (dwc2 33 -> 34) with no unaccounted additions.

## North-star

**The documentation must be clear enough that the least-capable AI model, working cold, does not have
to guess.** Concretely, having only read the repo, that model should be able to:

1. **Produce** constitution-respecting code (a service, a driver, a slice of a subsystem), and
2. **Enforce** the constitution on review (catch a violation and name the rule it breaks),

without inferring intent that the docs left unstated. Every rule a contributor or reviewer needs should
be **stated**, **discoverable where they look**, and **legible** (nameable, so it is checkable) - the
way §8.9's "at least one direction MUST use `try_send`" is legible.

**A perfect grokability score is not the goal.** A weak model scoring **7/10 or higher** on a cold read
is sufficient: the model gains the rest of its understanding from the compiler, the tests, `chaos
max-carnage`, and ultimately a human in the loop. What the audit protects is **clarity of intent**, not
a number - the docs must not *mislead* or *omit*, even if they cannot make a weak model omniscient.

## Method

The audit probes the docs the way a newcomer would meet them - with the least-capable model, cold, so
what a weak model misses is what the docs left unclear. Three probe types:

- **Cold-generation.** Delete a real implementation (a service, a driver, a function), and have a fresh
  weak model regenerate it from the docs + SDK + sibling examples alone (git-recovery forbidden). Judge
  the result for Commandment compliance and *assumptions*. **A mistake the model repeats is a doc gap,
  not a model failing** - the docs did not make the rule clear enough to follow.
- **Cold-review (seeded).** Plant ranked constitutional violations (obvious -> very subtle, plus
  cross-commandment) in a plausible candidate PR, and have a fresh weak model review it under a
  *neutral* prompt (never told violations exist - that would seed the answer). **A violation it misses
  whose rule exists but is scattered/unstated is a legibility gap.** (This probe is the more sensitive
  gap-finder: a reviewer must *positively name* each violation, so an under-packaged rule shows up as a
  miss; `docs/anti-patterns.md` is the seed bank.)
- **Grokability panel.** A small panel of cold weak models groks the repo and answers comprehension
  questions. Record the **distribution** of scores and, more importantly, **comprehension correctness** -
  correct answers matter more than the 1-10 number.

**Classification.** Every miss is triaged: a **doc gap** (fix it), **domain knowledge** the docs are
not meant to carry (e.g. an exact hardware register value - not a gap), or **model thoroughness** where
the rule was clearly available and the weak model simply did not apply it (not a gap; note it). Only doc
gaps produce edits.

### Severity

- **HIGH** - the docs are *wrong* or a *required* rule is absent: a contributor following the docs
  writes incorrect code, or the docs contradict the code.
- **MED** - the rule exists but is not *legible*: scattered/unpackaged (routinely missed on review), or
  a helper/pattern not *discoverable* where readers look.
- **LOW** - drift (stale counts, a wrong pointer), a missing example, or wording that invites
  over-application.

### Cadence

Run a documentation audit **frequently** - after any significant doc or feature change, and
periodically as a standing hygiene pass - the same discipline as the kernel and userspace audits. The
standing artifact the audit maintains is `docs/anti-patterns.md` (the field guide): new violation
classes and their fixes land there.

---

## Audit 1 - 2026-07-15 (clarity sweep via cold weak-model probes)

Method: six cold-generation probes and two seeded cold-review probes on the least-capable model, plus a
five-model grokability panel. Cold-generation targets: `resource-server` (delegated caps), `e1000`
(NIC driver), `counter` (restart-with-state), an `xhci` command-ring slice, and two re-runs to validate
fixes. Cold-review PRs: a network-health feature (kernel + service) and a request `gateway` (IPC +
GRANT), each seeded obvious->very-subtle. Every finding classified doc-gap / domain-knowledge /
model-thoroughness; only doc gaps fixed.

**Result: 0 HIGH, 6 MED, 2 LOW - all fixed.** The docs never *misled* (zero HIGH: no doc contradicted
the code, no required rule was flat-out absent in a way that produced wrong code the model couldn't
recover from). The real defects were **legibility and discoverability**: rules that existed but were
scattered, incomplete at a specific decision point, or a helper/pattern not documented where a reader
looks. Two structural wins came out of it: the constitution gained crisp checklists where it had prose
(§8.5, §14.3), and a whole new standing artifact - the field guide (`docs/anti-patterns.md`) - now makes
every violation class checkable.

Baseline metric (grokability panel, cold least-capable model): **median 7/10 (range 6.5-7.5)**;
**comprehension correctness effectively maxed** (every model answered every architecture question
correctly with citations); **coherence 9/10, unanimous**; doc-vs-code agreement on every spot-check.
By the north-star, this passes: correctness is maxed, and the 7/10 is the deliberate "you must read a
real constitution" tax, not a defect.

### Findings and fix log

| ID | Sev | Finding | Probe | Resolution |
|----|-----|---------|-------|------------|
| **D1** | MED | The `ServiceContext` method surface (esp. `log_fmt`) was not discoverable - `sdk/rust/CLAUDE.md` said only "log helpers", no example showed it, so a driver hand-rolled bounded formatting instead of using the SDK's `log_fmt`. | cold-gen (e1000) | **FIXED** `700d118` - enumerated method menu + `log_fmt` example in `sdk/rust/CLAUDE.md`; §26.6.1 reconciled (`format_args!` is bounded). Re-run validated: fresh model found and used `log_fmt`. |
| **D2** | MED | The recovery contract was stated only at *endpoint-cap* granularity ("reacquire by name and retry"); it never said a socket/id/generation/cached-value from the *dead* incarnation is also stale - so a reviewer *praised* a reacquire that reused a dead instance's socket. | cold-review (net-health) | **FIXED** `5d93b38` - §14.3 + Commandment IX: "reacquiring the endpoint is necessary but not sufficient." |
| **D3** | MED | The GRANT / capability-transfer rules were scattered across §7.3/§7.4/§7.6/§7.7/§8.5/Test 5 with no consolidated statement - so a reviewer missed *all three* rights-reasoning violations (no-GRANT transfer, reuse-after-move, over-grant) while catching the crisp §8.9 rule instantly. | cold-review (gateway) | **FIXED** `4b8c05c` - §8.5 "Transferring a capability - the three checks" (grantable / moved-not-kept / narrowed-to-need). |
| **D4** | LOW | The loud-failure rule was not restated at the *recovery/retry* path - a retry that ultimately fails could be swallowed as success. | cold-gen (counter) | **FIXED** `ca6c522` - §26.7 + Commandment V: "a recovery that itself fails is still a failure." |
| **D5** | MED | The identity-test docs listed `test_NN_*.rs` files that **do not exist** (the cases are data-driven in `osdev/src/validator.rs`), and the counts were stale (20/22 vs. the real 24). | grokability panel | **FIXED** `0f05169` - corrected the mapping and reconciled counts across `tests/`, `tests/qemu/`, `osdev/` docs + a §22.3 spec->implementation pointer. |
| **D6** | MED | Onboarding gaps: no getting-started-by-example path; the contract file was mislabeled `service.toml` (real path `contracts/<name>.toml`); the load-bearing `#[no_mangle]`-on-`service_main` gotcha was undocumented. | grokability + review | **FIXED** `0f05169` - new `GETTING_STARTED.md`; corrected the contract-path references; documented the gotcha in `GETTING_STARTED.md` + `examples/00-hello/CLAUDE.md`. |
| **D7** | MED | No contributor guidance for adding a CPU architecture - the arch seam is the codebase's biggest extension point after the demarcation, and it had no "how to" doc. | multi-arch demarcation | **FIXED** `5d58a20` - new `kernel/src/arch/CLAUDE.md` (the seam + the five-place checklist + the two rules) and a CONTRIBUTING "Adding an architecture" section. |
| **D8** | LOW | Grokability friction: the constitution interleaves current law with dated amendments, so a reader cannot always tell settled law from a proposal. | grokability panel | **FIXED** `0b093db` - §1 "how to read this document" note + a present-tense "current canonical state" box atop §6 (the worst offender). |

**Standing artifact created:** `docs/anti-patterns.md` (`dc29ce8`) - the Field Guide to Constitutional
Violations: 21 categories, each tagged to the Commandment/section it enforces, each row pairing the
violation with the correct pattern. This is the consolidation the audit proved the docs needed (a rule
you can name is a rule you catch) and the seed bank for future review probes.

### Classified as NOT doc gaps (recorded so they are not re-chased)

- **Domain knowledge, not doc scope.** The e1000 command-doorbell value (`0` vs a slot DCI `1`) and the
  `resource-server` op-code/rights numeric coincidence were exact-hardware / exact-encoding facts the
  constitution deliberately does not carry (§4.4 - the kernel and its docs know nothing of a device's
  meaning). Left to the datasheet; the xhci one got a one-line *code* comment (`9b6ff32`), not a doc rule.
- **Model thoroughness, rule was available.** In the `counter` cold-gen, the model applied loud-degrade
  and reacquire only on the path the ticket named, not uniformly - but the principles were documented,
  and the miss traced to the *deleted* per-example `CLAUDE.md` (which restates them at each step). That
  validates the per-example-`CLAUDE.md` design rather than indicting the docs.

---

## See also

- `docs/kernel-audit.md` - the ring-0 audit (nothing above the kernel may panic/wedge it).
- `docs/userspace-audit.md` - the services audit (wait on truth incl. failure; reacquire and retry).
- `docs/anti-patterns.md` - the field guide this audit maintains.
- `COMMANDMENTS.md`, `CLAUDE.md` - the law the docs must convey clearly.

---

## Audit 2 - 2026-07-15 (fix-validation + new-cluster probe)

Method: cold least-capable-model probes - two grokability cold-reads (comprehension questions targeting
Audit-1's fix areas) + one seeded cold-review on an **untested cluster** (memory/allocation,
temporal/boot-order, unbounded) in a `prefetch` candidate PR. Purpose: confirm Audit-1's fixes are
legible to a cold model, and probe a cluster Audit 1's reviews did not cover. Run as part of the
2026-07-15 full-trilogy audit (`audit-report/2026-07-15.md`).

**Result: 0 HIGH, 0 MED, 1 LOW. Audit-1's fixes are confirmed legible, and the field guide works.**

- **Fixes validated legible (cold weak model, unprompted).** Both grokability reads independently
  answered the Audit-1 fix-area questions correctly, citing the source: `log_fmt` (D1), the §8.5 GRANT
  three-checks (D3), the §14.3 stale-handle rule (D2 - one read quoted it verbatim), where
  `anti-patterns.md` lives, and the `#[no_mangle]` gotcha (D6). Grokability held at **median 7/10,
  comprehension maxed, coherence 9/10** - the north-star passes (correctness maxed; the 7/10 is the
  deliberate read-the-constitution tax).
- **The field guide works.** The seeded `prefetch` review caught the planted cluster (alloc-`unwrap` ->
  §10.4; boot-order assumption -> VIII/§14.3; unbounded loop -> §26.6.1) **and cited `anti-patterns.md`
  by category name** to justify findings - direct evidence the field guide is usable as a reviewer
  checklist. No new doc gap (the two partial catches were model-thoroughness; the rules were legible).

| ID | Sev | Finding | Status |
|----|-----|---------|--------|
| **DA1** | LOW | The amendment shorthand (H1, H11, P2, Phase C/D, Path C, naming Phase 4/5/6) is used pervasively across `CLAUDE.md`/docs with no decoder; both grokability reads (and the prior session) flagged it as the top remaining friction. | **fixed** - "Amendment shorthand" glossary added to `GLOSSARY.md`. |
| (cross) | LOW | `memory/CLAUDE.md` + `task/CLAUDE.md` describe dead code (`TaskMemoryOwner`/`ownership::reclaim_all`, `smp::placement`) as the live mechanism (shared with kernel-audit M1/M2). | **doc fixed** (banners repoint at the live code); the dead-code deletion is staged. |

Recorded as NOT doc gaps: the `prefetch` review's two partial catches (silent-swallow, report-ready)
were model-thoroughness, not missing rules - the reviewer caught the cluster and cited the guide. The
recurring "constitution is a reference, not a tutorial" friction remains a **feature**, not a defect
(the north-star sets 7/10 as sufficient).

---

## Audit 3 - 2026-07-23 (feat/pi2-arm32: the ARM32 docs we touched)

Scope: the documentation for the ARM32 (Raspberry Pi 2) port - what a newcomer/porter/service-author
meets. Method: three cold **Haiku** (least-capable) probes, each doing a real task from the docs alone and
flagging every place it had to GUESS - (1) build/run + extend arm32, (2) add a Pi 2 hardware driver, (3) a
grokability panel on the port's state. Then a structural discoverability pass (is the new doc linked where
readers look?). Every miss was triaged **doc gap / domain knowledge / model thoroughness**; only doc gaps
produced edits.

**Cold scores: build/run 6.5/10, add-a-driver 6.0/10, grokability 7.5/10.** The status doc
(`arm32-status.md`) was strong on *state* but the port lacked an implementer/contributor **home** and had
stale/missing pointers. **9 doc gaps found, all FIXED.**

| ID | Sev | What | Fix |
|----|-----|------|-----|
| **DA1** | HIGH | `docs/unsafe-audit.md` claimed "every syscall argument on this arch fits in 32 bits, so the widening is loss-free" - now WRONG: `recv_timeout`'s `timeout_cycles` exceeds u32 (userspace-audit A-U1). Doc contradicted the corrected code. | Corrected to name the one wider-than-u32 arg + its pre-clamp; pointed at `arch/arm/CLAUDE.md`. |
| **DA2** | HIGH | `README.md` said "32-bit ARM ... compile clean" - stale/misleading: arm32 boots the full OS to a shell on real Pi 2 hardware. | Rewrote the line: arm32 *runs the OS* (4-core, supervisor, ping/pong IPC), with the build/run commands + a pointer to `arm32-status.md`. |
| **DA3** | HIGH | The ratified **driver-porting doctrine** ("grok the working driver, reimplement the silicon's wants as a capability service, throw away the OS integration") existed only as a one-line mention + author memory - a contributor could not find "the GodspeedOS way". | Wrote it into `kernel/src/arch/CLAUDE.md` as **"Porting a driver: the method"** (executable-datasheet, reimplement-not-translate, simplest-reference, scope-to-our-chips, license/provenance). |
| **DA4** | MED | **No `kernel/src/arch/arm/CLAUDE.md`** (x86_64 has one) - the ARM syscall ABI, boot flow, and gotchas had no discoverable home; they were scattered across `unsafe-audit.md`/`multi-arch.md`/`arm32-status.md`. Both cold porters went hunting. | **Created it** - the implementer reference: the `svc #0` register ABI + the wider-than-u32 constraint (A-U1), the boot flow + cr3-seed rule, the in-kernel-driver rule, SEC-25..28 status, gotchas, and pointers. |
| **DA5** | MED | "How to add an ARM service" was undocumented - a contributor had to reverse-engineer that `ARM_SERVICES` (`arm_build.py`) and `arm_built` (`kernel/build.rs`) must both be edited and kept in sync. | Added a "Running a new service on the Pi 2" section to `arm32-status.md` (the two-allowlist step, explicitly). |
| **DA6** | MED | `docs/arm32-status.md` was **not listed in `docs/CLAUDE.md`** (the docs index) - a browsing newcomer would not find it. | Added an index entry (state, build/run, add-a-service, gotchas, remaining drivers). |
| **DA7** | MED | The ARM32 audit findings were not discoverable - `arm32-status.md` did not point to `kernel-audit.md` Audit 5 / `userspace-audit.md` Audit 4, and a cold reader concluded "ARM32 isn't audited". | Added a "See also" with the audit pointers (and `arch/arm/CLAUDE.md` closes it too). |
| **DA8** | MED | Misleading phrasing in `arm32-status.md`: "with no block device / NIC ..." read as *hardware* absence; the Pi 2 *has* eMMC/USB - the point is *driver* absence. | Reworded to "the Pi 2 *has* eMMC and USB, but their drivers are not ported to ARM yet". |
| **DA9** | LOW | `docs/multi-arch.md` had a verbatim-duplicated paragraph (the "ARMv7 is a SEPARATE port" note, twice); no pointer from it to `arm32-status.md` / the scripts. | Removed the duplicate; added a pointer box to `arm32-status.md` + `arch/arm/CLAUDE.md`. |

**Triaged as NOT doc gaps:** the physical serial wiring (adapter/pins) is domain knowledge - but the
baud rate (115200 8N1) was worth stating and is now in `arm32-status.md` + `arch/arm/CLAUDE.md`. The
grokability probe's "ARM audits not found" was partly model thoroughness (the entries exist, at the
bottom of long files), addressed by DA7's pointers.

**Result:** the single highest-leverage fix was the missing `kernel/src/arch/arm/CLAUDE.md` (it houses the
ABI + driver rules + hazards a porter/service-author needs, where they look), and writing the driver
doctrine into the repo (DA3) so the method is followable, not tribal. Re-probing would clear the two
below-bar scores: the ABI, add-a-service, and driver method are now stated, discoverable, and legible.
