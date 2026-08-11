<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Documentation Clarity Audit

> **Living document.** Records every audit of the *documentation* - `CLAUDE.md`, `COMMANDMENTS.md`,
> the SDK and per-directory `CLAUDE.md` files, the examples, and `docs/` - for clarity and intent.
> Re-run and append with each audit. This is the third of the audit trilogy: the kernel has
> `docs/kernel-audit.md`, userspace has `docs/userspace-audit.md`, and the docs themselves have this.
> First audit: 2026-07-15.


## Audit 5 - the revert range: does the prose still describe machinery that was taken back out? (2026-08-11, `feat/pi4-aarch64`)

**Scope:** everything committed since `5426c6db` - 20 commits, of which five ADDED a mechanism and a
later one TOOK IT BACK OUT: a kernel console-write counter (`InspectKernel` query 23), the shell
prompt-redraw built on it, a `ConsoleReadTimeout` syscall, a PHY settle before DHCP, and the "press
Enter to return to the prompt" hint that replaced the redraw. A revert range is the sharpest test this
audit has: prose survives a `git revert` that code does not. This audit asks two questions - **does
anything still describe the machinery that is gone**, and **does the code that REMAINED obey the
Commandments** (the second half at the user's explicit request).

**Verdict: 0 HIGH, 3 MED, 4 LOW. Two Commandments are violated: V/IX (one finding) and VIII (one
finding).** Everything else in the range is clean, and one result is worth stating first because it is
the answer to the question that prompted the audit.

**The reverts are complete. All five mechanisms left nothing behind, in code OR in prose.** Verified
two ways rather than one: targeted greps for `console_write_seq`, `CONSOLE_WRITE_SEQ`,
`ConsoleReadTimeout`, `console_read_timeout`, `query 23`, `PHY_SETTLE`, `press Enter to return` and
`nudge` return **nothing** across the repo; and `git diff 5426c6db..HEAD` touches only **four files**
(`scripts/selfcheck.gsh`, `services/net-stack/src/main.rs`, `services/nic-driver/src/genet.rs`,
`services/xhci/src/main.rs`), so `kernel/src/syscall/dispatch.rs`, `sdk/rust/src/service_context.rs`
and `services/shell/src/main.rs` are **byte-identical** to where the range began. `dispatch.rs` gained
24 + 45 lines and lost 69; the `InspectKernel` query arms now run 4..22 with no 23. The two surviving
`press Enter` strings are unrelated and correct (`chaos` describing a screen-switch glitch, and
net-stack's own comment recording why the hint was removed). No doc, no `CLAUDE.md`, no manifest and no
comment ever described any of the five, so there was nothing to sweep. **This is the clean result, and
it is the exception in this document rather than the rule** - Audit 3 (2026-07-31) and Audit 4 both
found the opposite shape.

The drift that DOES exist in this range is a new shape and worth naming: **comments that describe what
the change was MEANT to achieve rather than what it achieves.** A5-3 and A5-5 are both a paragraph of
excellent, honest reasoning attached to code that stops one step short of the claim. That is harder to
catch than a stale sentence, because the prose was written in the same commit as the code and reads as
authoritative precisely because it is so specific.

### Ranked ledger

| ID | Sev | Kind | File:line | Finding | Confidence |
|----|-----|------|-----------|---------|------------|
| **A5-1** | MED | Commandment **V + IX** | `services/nic-driver/src/genet.rs:1314-1319` | **A recovery that consumes its own trigger and discards its own failure.** The new hot-plug fix re-applies MAC speed and DMA burst on a down -> up transition: `if up_now && !link_was_up { log(...); g.apply_link_settings(); } link_was_up = up_now;`. `apply_link_settings` **returns `u32`, and the return is dropped**. It returns `0` - after logging "PHY has not settled on a speed - leaving the MAC at its default" - in two reachable cases: the BCM aux-status HCD field (bits 10:8) has not resolved yet, and **any failed MDIO read** (`let Some(aux) = self.mdio(BCM_AUX_STATUS, None) else { return 0 }`, `genet.rs:566`). `link_is_up` reads a *different* register (BMSR bit 2, `genet.rs:585`), so the two can disagree. When they do, `link_was_up` is set to `true` regardless, **the edge is consumed, and the re-apply can never run again**: every later status request sees `up_now == link_was_up`. The MAC stays unclocked, nothing is received, and the only recovery is a physical unplug/replug. Commandment V is explicit that "a recovery whose own failure is discarded becomes a silent success"; here the failure is at least *logged*, so it is loud - but it is **not recovered**, which is Commandment IX. The one-line shape of the fix is to gate the state update on the result. | **CONFIRMED** (code + register divergence); frequency PLAUSIBLE |
| **A5-2** | MED | Commandment **VIII** | `services/net-stack/src/main.rs:977-988` (with `:90-102`, `sdk/rust/src/service_context.rs:715-733`) | **The new idle tick opens a request-swallowing window once a second, forever.** net-stack serves clients and receives nic-driver's replies on **one endpoint** with **no correlation tag** (`task/mod.rs:762-765`: "Owns its endpoint (nic-driver replies frames there via the per-request reply cap)"). `nic_req` waits via `request_with_reply_deadline_outcome`, which polls **`try_recv()`** on that same endpoint (`service_context.rs:717`) - and `try_recv` and `recv` both read `data.recv_slot` (`:475-481`, `:487-493`). So any client request that lands while `link_is_up` is waiting is returned **as the link reply**: `link_is_up` reads `!p.is_empty()` as "up" (`main.rs:895`), and the client's message is dropped with its reply cap never taken. The client waits out its own deadline and reports a **false** "net-stack not responding". This is the same class as the `fs` reply-correlation desync already recorded in this repo. It is not new *in kind* - the request path has always called `link_is_up` - but it is newly **permanent**: before this change an idle net-stack sat in `ctx.recv()` and could not swallow anything, and now it opens the window ~86,400 times a day on a machine with no network traffic at all. Arguably HIGH; held at MED only because the window is one IPC round trip wide. | **CONFIRMED** code path; frequency PLAUSIBLE |
| **A5-3** | MED | Doc drift (in-code) | `services/net-stack/src/main.rs:975-976` and `:934-938` | **Two comments promise auto-configuration on plug-in that the code does not perform.** The tick's comment says "That single change is what makes connect INFO, disconnect INFO **and auto-config-on-plug-in** possible at all", and the boot-skip's comment says "`link up while unconfigured - auto-configuring` already re-runs the dance **the moment a cable appears**". Neither is true: on timeout the tick announces and **`continue`s** (`:987`), skipping the auto-configure block entirely. `run_dance` has exactly three call sites - boot (`:940`), a **client request** that needs the network (`:1018`, gated on `badge.is_none() && !have_mac && matches!(pl.first(), ...)`), and `net renew` (`:1127`). So a Pi 4 booted with the cable out, then plugged in and left alone, prints `NET: ethernet cable connected` and **stays unconfigured** until somebody runs a network command. The behaviour is defensible; the claim is not, and it is exactly the claim a reader would rely on when deciding this path needs no further work. | **CONFIRMED** |
| **A5-4** | LOW | Doc drift (in-code) | `scripts/selfcheck.gsh:246-247` | **"`date epoch` yields 0 when the clock is unset" is false on the board this was written for.** The kernel returns packed `0` for an unset clock (`arch/aarch64/mod.rs:2296-2300`), the SDK unpacks that to year/month/day = 0/0/0 (`service_context.rs:1394-1403`), and Hinnant's `days_since_epoch` maps it to **-719,560 days**, so `date epoch` prints **`-62169984000`**. The check still behaves correctly - `parse_i64` accepts the leading `-` (`shell/src/main.rs:2208`) and `-62169984000 > 0` is false, so it SKIPs - but it works for a reason the comment does not state. A future maintainer "tidying" the guard to `== 0` or `!= 0` would silently break the skip, or worse, make it fire on a machine whose clock IS set. | **CONFIRMED** (arithmetic + code) |
| **A5-5** | LOW | Doc drift (in-code) | `scripts/selfcheck.gsh:238-241` | **The stated principle and the chosen mechanism disagree.** The comment argues, correctly and at length, that "The probe READS, it does not repair ... a check must not perform a network operation or set the machine's clock as a side effect ... a test that changes the system has stopped measuring it". The mechanism it then picks, `for line in (date epoch)`, **writes a temp file through `fs`** on every run: `forlines_capture` deletes, creates and writes `/.fl<id>~`, and `forlines_step` deletes it at EOF (`shell/src/main.rs:2977-3037`). The side effect is small and self-cleaning, but it is a side effect, and it is a **storage dependency in a clock probe**. Sharper consequence: if `fs` cannot serve the write, `forlines_capture` prints "gsh: for line: capture write failed" and **counts a failure** (`:3413`) - so a storage fault now fails the *clock* check, the opposite of the skip-with-reason this rewrite exists to provide. | **CONFIRMED** |
| **A5-6** | LOW | Doc drift (in-code) | `services/net-stack/src/main.rs:966` vs `:977` | **The file contradicts itself about whether the kernel's quantum calibration works.** Line 977 (new) paces the serve loop with `ctx.duration_cycles(LINK_TICK_MS)`; line 966 (pre-existing) says the kernel's calibration "is 0 on T630" and calibrates its own `tsc_hz` for that reason. Both cannot be current. It matters because the failure is silent and 100x: `duration_cycles` floors to `1` when `tsc_ticks_per_10ms()` is 0 (`service_context.rs:1305`), and `cycles_to_ticks` then floors to **one scheduler tick** (`scheduler.rs:171-174`), so the "one second" tick becomes ~10 ms - a hundred NIC round trips per second on an idle machine, and a hundred times the A5-2 window. x86 now PIT-calibrates the quantum for every CPU (`arch/x86_64/boot.rs:376-383`), so the line-966 comment is *probably* the stale half - but one boot settles it and neither line should be left standing as written. | **PLAUSIBLE** (needs one T630 boot to decide which line is stale) |
| **A5-7** | LOW | Doc drift | `docs/xhci-topology.md:100-105`, and its absence from `docs/CLAUDE.md` | **Step 1 of the plan has landed and the plan still reads as unstarted.** `services/xhci/src/topo.rs` exists, and its own header says so honestly ("**Step 1 of `docs/xhci-topology.md`, and deliberately behaviour-neutral**"). The doc's "Order of work" still presents step 1 in the imperative with no status marker, and its "What this deletes" list names `disk_absent_seen` and `hub_tried` as subsumed while both are still live (`xhci/src/main.rs:2632`, `:3034`). The code is the honest half; the doc is one-way. Compounded by A4-12, which is still open: neither `xhci-topology.md` nor `xhci-split.md` is in the docs index, so the only way to find either is to already know it exists. | **CONFIRMED** |

### Verified still true (do not re-check)

- **`docs/xhci-split.md` is still accurate.** Its central claim - that the service does disk work and
  HID polling on one pass, and that its recommended fix (option 1, a HID callback during disk waits)
  is not yet done - holds: `msc::await_on_slot` still takes `eaten: &mut u32` and no callback
  (`services/xhci/src/msc.rs:246-254`). Only A4-13 applies (two dead line citations into the deleted
  in-kernel driver).
- **The `xhci` liveness heartbeat is correctly built** (`services/xhci/src/main.rs:3354-3359`). Its
  state is hoisted above `'reenum` so a re-enumeration cannot reset it (the bug its own comment
  records), it sits **before** the only `break 'poll` (`:3419`), and the loop it beats from always
  waits on a bounded `recv_timeout` (`:3245`), so an idle driver still beats and a stopped one does
  not. It cannot become the log flood it replaced. The one nit is cosmetic: `passes` counts passes
  that reach `:3354`, not every pass, and the line says "poll passes".
- **`link_notify` gets the authority question right, deliberately**
  (`services/net-stack/src/main.rs:869-897`). It uses `console_write` (LOG_WRITE, slot 0, held by
  every service - `task/mod.rs:3497`) and explicitly **not** `console_push`, with the SEC-2 reasoning
  written into the comment: a `CONSOLE_PUSH` holder is inside the shell's trust perimeter because
  keystrokes are commands (§6.4). A cosmetic notice buys no new authority. This is the pattern the
  USB drivers' `notify` does *not* follow (`xhci/src/main.rs:478-490`, `ehci/src/main.rs:871-884`,
  both pre-existing and both justified there).
- **The reverted prompt-redraw was the range's one real §26.10 problem, and it is gone.** A kernel
  counter whose only purpose is to let the shell decide when to repaint a prompt is shell *policy*
  living in the kernel. It was added, found to trip on its own output, and removed - and the
  replacement hint was then removed too, for the better reason that net-stack cannot know whether the
  shell is at a prompt. The end state prints only the fact it actually knows.

### Carried over from Audit 4 - still open, NOT counted against this range

Nothing in this range touched a doc, a manifest or `CLAUDE.md`, so **the entire Audit 4 ledger stands
unfixed**. Spot-verified this session: `docs/unsafe-audit.md:2328` still carries the phantom
`arch/aarch64/xhci.rs | 42 | permitted` row in the LIVE inventory (A4-4); `kernel/src/task/mod.rs:486`
still justifies the `USB_DISK` grant with "on BOTH ARM ports the USB stack is in-kernel" (A4-2 /
SEC-37); `services/block-driver/src/xhciblk.rs:5` still says the in-kernel driver "cannot be deleted"
(A4-3); `docs/aarch64.md:3` still opens "**Status:** design, not built ... 4 GB" (A4-9);
`kernel/src/arch/mod.rs:13`, `services/block-driver/contracts/block-driver.toml:28`,
`kernel/Cargo.toml:113`, `scripts/pi4_build.py:29` and `services/xhci/src/main.rs:2241` ("still in
this tree") are all unchanged (A4-1, A4-5, A4-7, A4-8, A4-11). **A4-10 also stands**: §6.4's
2026-08-09 amendment remains accurate about the kernel and silent about the TCB consequence, while
§6.1's table row and the glossary's `TCB` entry ("Kernel + arch + smp + init + supervisor" - `init`
was removed in Phase 5) still give the answer it omits. One addition to A4-10 found here: `CLAUDE.md`
§6.4's ARM32 amendment cites `selfcheck 349` as verification evidence, and after A5-4/A5-5 the
selfcheck's assertion count is **machine-dependent** (348 when the clock check skips), so an exact
number is now a fragile thing to pin in the constitution.

### Commandment-by-commandment compliance (the four files changed in this range)

| # | Commandment | Verdict | Evidence |
|---|-------------|---------|----------|
| **I** | Kernel responsibilities are complete | **RESPECTED, and improved** | The kernel is **byte-identical** to `5426c6db` across the whole range. Nothing new was added to it, and the one thing that was - a console-write counter existing so the shell could time a prompt repaint - was reverted whole. No policy, no device logic, no new syscall. |
| **II** | Trust in Chaos | **Partly exercised** | The `xhci` log-rate fix (`f67f5c15`) came *out of* a 10-hour soak in which chaos restarted services 501 times, which is Chaos working as intended. The new net-stack tick and the nic-driver re-apply have **no chaos evidence on this branch**; A5-1 and A5-2 are both the kind of defect a kill-storm against `nic-driver` would surface. Recorded as an observation, not a violation. |
| **III** | One irreducible truth | **RESPECTED** | Four new pieces of derived state, all reconcilable and subordinate: `last_link` (net-stack, re-read from the NIC every tick), `link_was_up` (nic-driver, re-read from the PHY every status request), `passes` (a pure counter), `clockset` (recomputed each selfcheck run). None is authoritative and none can outlive its source. Two *services* holding a view of the same PHY is not a second truth - each reconciles against the hardware. |
| **IV** | Honor service contracts | **RESPECTED** | No contract was touched and none needed to be. `console_write` rides the LOG_WRITE slot every service already holds; no new peer, no new capability, no back channel. |
| **V** | No service is special; surface a failed recovery | **VIOLATED - A5-1** | `genet.rs:1317-1319` runs a recovery, drops its `u32` result, and marks the transition handled either way. The failure is logged by the callee, so it is not silent - but the caller proceeds as though it had succeeded. |
| **VI** | No shared mutable state | **RESPECTED** | Nothing new is shared. The `static SEEN: AtomicU32` in `xhci::serve_if_block` is pre-existing, service-local, and only had its threshold changed. |
| **VII** | No ambient authority | **RESPECTED, notably well** | See "Verified still true": `link_notify` reasons explicitly about `console_write` vs `console_push` and takes the lesser authority for a cosmetic feature. |
| **VIII** | Wait on truth, bounded, including failure | **VIOLATED - A5-2** | The *bounding* is exemplary: every new wait is clock-bounded (`recv_timeout(duration_cycles(LINK_TICK_MS))`, `nic_req`'s seconds deadline, `HEARTBEAT_MS`, the always-bounded xhci `recv_timeout`), and **no count-bounded wait was added anywhere in the range**. The violation is the other half - the truth waited on is not distinguishable from a different message. A shared, untagged endpoint plus a `try_recv` poll loop means the tick can accept a client request *as* the link reply, and the real client then times out against a service that is alive. A5-6 is the related hazard: if the quantum calibration is 0 on any target, the "one second" bound silently becomes ~10 ms. |
| **IX** | Plan for recovery | **VIOLATED - A5-1** | The consumed edge leaves the NIC in a state nothing recovers from except a human touching the cable. Note the *good* IX work in the same range for contrast: the boot no-link skip (`main.rs:939-953`) turns ~25 s of unresponsive DHCP budgets into an immediately responsive service that says exactly why it is unconfigured, and `nic_req` reacquires nic-driver by name after a restart (`:95`). |
| **X** | Complexity in the right place | **RESPECTED** | Same evidence as I. The redraw put prompt policy in the kernel; the revert put it back in the only layer that can know the answer, and then removed the guess entirely. |
| **-** | **The rule above the rules** (nothing above the kernel may hang or crash the machine) | **HELD** | Every new wait terminates on a clock. No new unbounded loop, no new blocking send, no new panic path. The two violations degrade *correctness*, not liveness: A5-1 leaves the network dead but the machine and every service alive and responsive; A5-2 makes a client report a loud, bounded "net-stack not responding" - wrong, but returned, not hung. The range's largest single improvement is on exactly this axis (the boot no-link skip). |


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
