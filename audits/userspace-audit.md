# Userspace Commandment Audit

> **Living document.** Records every audit of the userspace services (everything above the kernel)
> against the Ten Commandments (`COMMANDMENTS.md`) and the constitution (`CLAUDE.md`). Re-run and
> append with each audit. The kernel has its own living record in `audits/kernel-audit.md`; this file
> is its userspace counterpart. First audit: 2026-07-12.



## Audit 11 - inherited from `commandment-audit.md`, which is now deleted (2026-08-25)

`audits/commandment-audit.md` was scaffolding, and its own header set the two conditions for deleting
it: every Commandment through the process, and every finding closed or homed. The first was met - all
ten have sections, and `scripts/commandments.py` carries 15 checks with 63 red-team probes. The second
was not, so its four surviving findings move HERE, where open findings belong, and the scaffolding
goes. Each was re-verified against the tree today rather than copied forward.

| ID | Finding | Then | Now |
|----|---------|------|-----|
| **C3-1** | The largest ethernet frame is defined in several places | 6 copies, **disagreeing** | 5 copies, **all agree at 1600** (`dwc2/net.rs`, `nic-driver/genet.rs`, `nic-driver/main.rs` x2, kernel `dispatch.rs`). The dangerous half is gone; the duplication is not. Commandment III. |
| **C4-2** | Service crates shipping no contract | 8 of 14 | **1 of 17** - only `probe`, the adversarial/fuzz harness, which test builds spawn. `scripts/contract_check.py` reconciles the other 16 against their kernel service_config. Arguably legitimate; recorded rather than assumed so. |
| **C8-1** | Waits bounded by an ITERATION COUNT rather than a clock | 16 | **9**. Commandment VIII, and the shape this session met twice: a count means a different duration on every machine. |
| **C10-1** | `services/shell/src/main.rs` has no module header | 10,812 lines | **11,466 lines, still none**. It grew. Commandment X - the file is in the right place, it just does not say what it is. |

**Closed, and not carried forward:**

- **C1-1** the kernel spawning services by name - the two ARM scaffolding functions are gone, and
  `I-kernel-spawns` now fails the build if a second spawn appears.
- **C1-4** `InspectKernel`'s unpinned query space - pinned by `I-introspect`.
- **C1-6** four kernel modules serving none of the six responsibilities - closed when `fbcon` left for
  the `console` service, and the enforcement probe was ratcheted on 2026-08-25 so that a module going
  back to claiming nothing breaks the build.
- **C6-1** unowned global mutable state in two services - `VI-static-mut` passes, and the red-team
  suite confirms the check catches a planted violation.
- **C1-2** (arch is 65% of the kernel) was an observation with no action expected; **C1-3** (the font
  crate) and **C1-5** (218 service configs) are accepted with pins in `COMMANDMENTS.baseline.toml`,
  which is exactly the "accepted rather than fixed" home the scaffolding named.

**What deleting it cost: nothing that was not already somewhere else.** The checklist became the build,
the process lives in `scripts/commandments.py`'s module docstring, accepted findings live in the
baseline, and the four above now live in a living audit instead of a temporary one.


## Audit 10 - the link tick, the cable edge, and four reverts (2026-08-11, `feat/pi4-aarch64` @ f67f5c15)

**Scope:** everything committed since `5426c6db` (20 commits) across `services/`, `sdk/`, and
`scripts/selfcheck.gsh`. The surviving net diff is small - `net-stack/src/main.rs`,
`nic-driver/src/genet.rs`, `xhci/src/main.rs`, `selfcheck.gsh` - because four things were **added and
then reverted** inside the range (the kernel console-write counter + the shell prompt-redraw
machinery; net-stack's "press Enter" hint; net-stack's PHY settle; net-stack's DHCP frame
instrumentation). Verifying those reverts was half the brief. Audit 9's own fixes (A9-1 through A9-4)
landed just *before* the range, so they were re-checked as delivered rather than as claimed.

**Verdict: 2 HIGH, 5 MED, 3 LOW.** The reverts are clean - byte-clean, in fact. The real defects are
all one shape: **the new link machinery is a chain of three edge-triggered mechanisms across two
services, and every link in it consumes its trigger whether or not it did its job.** net-stack's tick
consumes messages that were not its reply; nic-driver's re-apply consumes the cable edge even when it
failed; the boot gate consumes its one and only probe. Each was written as if the thing it asks is
guaranteed to answer.

| ID | Sev | Commandment | Finding |
|----|-----|-------------|---------|
| A10-1 | HIGH | III, VIII, §8 | **net-stack's idle tick opens a message-stealing window on its own serve endpoint, once a second, forever.** `services/net-stack/src/main.rs:982` calls `link_is_up` from the timeout arm; `link_is_up` (`:896`) -> `nic_req` (`:90`) -> `ServiceContext::request_with_reply_deadline_outcome`, whose wait loop is `if let Some(r) = self.try_recv() { return DeadlineOutcome::Reply(r); }` (`sdk/rust/src/service_context.rs:717`). That is the **same endpoint net-stack serves clients on**, and the loop returns the first message that lands there, whatever it is. Three consequences, all confirmed by reading: (a) the link verdict is derived from a client's payload - a shell `net` is `[0]`, so `p.len() > 7` is false and `!p.is_empty()` is true, and `link_is_up` returns **true** regardless of the cable, which can flip `last_link` and print a **false** `NET: ethernet cable connected` on the console; (b) the client's request is consumed and never served, while the kernel has already installed its embedded reply cap and pushed it onto the per-task pending FIFO (`kernel/src/syscall/dispatch.rs:388-397`, `scheduler.rs:767` - strictly FIFO), so the **next** message net-stack serves is answered on the **wrong client's** cap; (c) nic-driver's real op-3 reply then arrives and is parsed as a request - `[1, mac(6), link(1)]`, first byte 1 = op 1 = **DNS lookup**. On a healthy NIC the window is the op-3 round trip (ms) and the FIFO rebalances after one bad exchange; when nic-driver is slow, restarting, or newly respawned the window is the full `LINK_SECS = 1` s against a 1 s tick, and each stolen request leaves the FIFO one entry deeper - a persistent reply-to-the-wrong-requester offset plus a leaked cap slot each time. The SDK comment at `service_context.rs:726-729` names exactly half of this ("this variant cannot drain blindly, because a service that also SERVES on this endpoint (net-stack) would discard live client requests") and then the loop consumes anyway. **Not new in kind** - the request path already called `nic_req` on this endpoint - but it *is* newly reachable, because a client is only free to send while net-stack is idle, and idle is precisely when the tick now runs. CONFIRMED code path; the interleaving is PLAUSIBLE and is produced by any `net`/`ping` typed during a NIC hiccup. |
| A10-2 | HIGH | V, IX, §26.7 | **nic-driver spends the cable edge even when the re-apply failed, and there is no second edge.** `services/nic-driver/src/genet.rs:1315-1319`: `if up_now && !link_was_up { g.apply_link_settings(); }` then `link_was_up = up_now;` - unconditionally. `apply_link_settings` returns `u32` and has an explicit failure branch (`:956-960`): when `negotiated_speed()` reads 0 it logs "PHY has not settled on a speed - leaving the MAC at its default" and **returns 0 without programming the speed**. That verdict is discarded, the edge is marked consumed, and `apply_link_settings` has no other caller after `bring_up` (`:1095`) - so the MAC stays at its default clock and **receives nothing** until the cable is physically unplugged and replugged. The window is not exotic, it is the expected one: `link_is_up()` reads BMSR bit 2 (`:585-588`) while `negotiated_speed()` reads the vendor AUX status auto-negotiation result (`:565-577`) - two different registers, and nothing gates the second on `BMSR_ANEGCOMPLETE`. net-stack's tick probes at 1 s while copper auto-negotiation takes seconds, so the very promptness the tick was added for is what makes the transition likely to be seen before the speed resolves. This is also the hole the removed PHY settle used to cover: `51ba26cf` deleted it on the strength of `27c719bd`'s re-apply, and `27c719bd`'s re-apply is one-shot. Same class as the "storage recovered" and `sdhci` step-up discards (A8-1, U6-2, U6-10): a failed recovery recorded as a completed one. CONFIRMED code fact; the trigger frequency is PLAUSIBLE, not measured. |
| A10-3 | MED | III, VIII, §26.7 | **The boot link gate is a single un-retried 1 s probe, taken at the moment nic-driver is least able to answer.** `net-stack/src/main.rs:939` replaced an unconditional `run_dance` - which retries `DANCE_TRIES = 6` times over `DANCE_SECS` budgets and therefore tolerated a peer that was still starting - with one `link_is_up`, whose only budget is `LINK_SECS = 1` and which has no retry at all. The supervisor spawns nic-driver and net-stack back to back (`services/supervisor/src/main.rs:175`, `:536`, `:548`), and nic-driver does not reach `serve()` until `bring_up` has finished MDIO probing, the PHY clock delay, an arena clear, two ring inits and two `DMA_START_TIMEOUT_US` waits. A `None` from `nic_req` therefore means "the driver is not serving yet" at least as often as it means "no cable" - and the code prints `no link at boot (cable unplugged?)`, a statement it has no evidence for, then **skips the entire boot configuration**. A cabled machine comes up with no IP, no gateway, no DNS and no SNTP wall clock. It is recovered by the first `net`/`ping` (the request-path auto-configure at `:1008`), so it is not permanent - but nothing recovers it on an idle machine (A10-4), and the check that would have caught the unset clock now skips itself (A10-9). CONFIRMED. |
| A10-4 | MED | III, §26.4 | **The tick announces a cable it never configures, and two comments say otherwise.** The timeout arm (`:979-988`) reads the link, prints `ethernet cable connected`, and `continue`s. Auto-configure lives only in the request path (`:1008`), gated on `badge.is_none() && !have_mac && matches!(pl.first(), ..)` - i.e. it needs a **client request**. So on an idle machine the console reports that the network arrived and the stack stays unconfigured indefinitely. The comment at `:975-976` claims the tick "is what makes connect INFO, disconnect INFO and **auto-config-on-plug-in** possible at all", and `:935-938` justifies skipping the boot dance with "a machine booted unplugged configures itself on plug-in rather than needing `net renew`". Neither is true as written: what the tick delivers is the announcement, not the configuration. A derived view that contradicts its source. CONFIRMED. |
| A10-5 | MED | IX, III, §14.3 | **A9-3's `revoke_all_open` was applied at one of the five sites that replace or drop the mount.** `services/fs/src/main.rs:2714` is correct and `:519` calls it. The other four do not: `:906` (`serve`'s retry-after-I/O-error re-mount, `*vol = Some(nf)` - **the second site A9-3 named by line**), `:1007` (the `drives` handler dropping the mount on an authoritative `Some(0)` capacity), `:1066` (`OP_FLASH` success - the disk was just **wiped**, so every open file is physically gone), `:1113` (`OP_RESET`). At each, the client's file cap stays valid at the kernel, its `rid` no longer resolves, and `serve_filecap` answers `FS_NOTFOUND` - "your file was deleted" instead of the truthful `CapRevoked` - while the resource is never revoked and leaks in fs's 2048-entry delegated band. The `:1066` case is the sharpest: fs knows with certainty that every open handle is void, and says nothing. CONFIRMED. |
| A10-6 | MED | V, §26.7 | **xhci's "block request had NO reply cap" warning is gated on the wrong counter and is therefore unreachable in practice.** `services/xhci/src/main.rs:1421` computes `n` from `SEEN`, which counts **every** block-path message; `:1434` then logs the refusal only `if n == 1`. `block-driver` always sends its block requests through `request_with_reply` (`xhciblk.rs`), so message #1 carries a reply cap and spends the latch before any cap-less message can arrive. The refusal only ever prints if the very first block-path message of the instance is the malformed one. `f67f5c15`'s own commit message says "The refusal is worth keeping - a request with no reply cap leaves its caller waiting - but once per instance says everything a hundred repeats do"; what the code implements is "only if it happens first". The file already has the right pattern three times over (`NO_DISK_LOGGED`, `READ_FAIL_LOGGED`, `hub_none_logged`): a latch on the *event*, not a counter of *arrivals*. Sixth instance in this repo of a guard whose condition cannot hold in the failing case, and the commit that introduced it is in this range. CONFIRMED. |
| A10-7 | MED | II, IX | **A9-2's fix does not cover the configuration A9-2 was about.** `services/xhci/src/main.rs:3441` now reads `if hub_due && (ndev < MAX_HID \|\| disk.is_some())`, which rescues the disk-removal watch. But the same block is the **only** site that increments `PROBE_FAILS` (`:3666`), which is the only trigger for the Reset Endpoint + Set TR Dequeue repair of a halted hub EP0 - the state the code's own comment calls one that "never recovers on its own". With `ndev == MAX_HID` (a keyboard **and** a mouse) and no disk bound, neither this gate nor the `ndev == 0` fallback (`:3719`) fires, so the repair is still dead in exactly the two-HID case A9-2 raised. The comment at `:3439-3440` states the correct rule - "a guard belongs on the ACTION it protects, not on the observation that feeds it" - and then leaves a second observation (the halted-EP0 probe) behind an action guard (`disk.is_some()`). CONFIRMED by reading the nesting end to end. Carried from a fix that landed just before this range; recorded here because it is live. |
| A10-8 | LOW | III, §26.7 | **`learn_our_mac(&ctx).unwrap_or([0; 6])`** (`net-stack/src/main.rs:948`) turns "nic-driver did not answer" into an all-zero MAC presented as our hardware identity, which `net` then prints as `00:00:00:00:00:00`. The comment two lines above asserts the opposite - "The MAC is still learned - it is our hardware identity and true with or without a cable" - which holds when the cable is out and the driver is up, and fails in precisely the case A10-3 makes common (the driver is not serving yet). A silent fallback where a `None` was available. CONFIRMED. |
| A10-9 | LOW | III | **The selfcheck clock probe skips silently when `date epoch` is BROKEN, not only when the clock is unset.** `scripts/selfcheck.gsh`: `let mut clockset = 0` / `for line in (date epoch) { if $line > 0 { clockset = 1 } }`. If `date epoch` errors or prints nothing the loop body never runs, `clockset` stays 0, and the suite prints `SKIP date - the clock is not set on this machine`. A regression in `date epoch` is therefore reported as a missing RTC. The block's own comment sets the bar it misses ("a silent skip is a test that has quietly stopped testing"); the probe needs `assert ok date epoch` beside it so a broken producer fails rather than excuses itself. Compounds A10-3: the boot dance being skipped is exactly what leaves the clock unset, and this is the check that would have shown it. CONFIRMED. |
| A10-10 | LOW | VIII | **nic-driver's link re-apply is reachable only through net-stack's op-3 request.** `genet.rs:1294-1319` sits inside the status arm, so the driver's own recovery from an unclocked MAC depends on another service polling it. Kill net-stack (`chaos kill-storm net-stack` does, and it is in the restartable set) and no cable event is ever acted on until it comes back. Deliberate and documented ("Done HERE because this is the one place the link is already read live... with no polling added anywhere"), and net-stack's tick does drive it in the normal case - recorded because it makes a hardware recovery contingent on a peer's liveness, which is the coupling §14.3 asks to be stated rather than assumed. CONFIRMED. |

**Clean results (verified, not assumed):**

- **The four reverts are byte-clean.** `git diff 5426c6db..HEAD` touches **no** shell file and **no**
  kernel file at all: the console-write-counter/prompt-redraw chain nets exactly to zero
  (`9a233ad7` +24 and `6231e21c` +45 in `dispatch.rs` against `a7be98da` -69; SDK +21/-21; shell
  +22-1+7-3 against -26). No orphan survives - `console_write_count`, `prompt_redraw`, the query-23
  helper and the "press Enter" hint are absent from `services/`, `sdk/`, `kernel/` and `osdev/` (the
  two surviving `press Enter` hits are prose: an unrelated `chaos` comment and the net-stack comment
  that explains why the hint was removed). The PHY settle and the DHCP frame instrumentation likewise
  leave no constant, no counter and no dead helper behind, and `link_notify` has exactly one caller.
- **`link_notify` grants itself nothing.** It uses `console_write` (gated on the `LOG_WRITE` this
  service already holds) and puts the newline **inside** the written string, rather than `console_push`,
  which injects into the shell's input ring and would put net-stack inside the shell's trust perimeter
  (§6.4, SEC-2 - keystrokes are commands). Checked against `xhci`'s `notify` (`main.rs:484-491`), which
  does push. The visible cost is that a net-stack announcement does not redraw the prompt the way a USB
  one does; that is the accepted residual the reverted redraw work was chasing, and it is documented
  in place.
- **`last_link`'s seeding is correct on every path except through A10-1's misread.**
  `d.have_mac \|\| link_is_up(&ctx)` short-circuits, so a successful boot dance never re-probes; a
  failed dance and a no-cable boot both re-probe and get the truth. A net-stack restart with the cable
  in seeds `true` and does not announce a phantom plug-in; a restart with it out seeds `false`.
  nic-driver's own `link_was_up` seed is sound *as a seed* - the defect in A10-2 is what the edge does,
  not where it starts.
- **The xhci heartbeat hoist is correct, and is the fix `a2647c5b` claims.** `last_beat` and `passes`
  are declared above `'reenum` (`main.rs:2638-2639`), so they survive both `continue 'reenum` sites
  (`:3257`, `:3310`) and every `break 'poll` - which is the whole content of the bug, a timer reset by
  each re-enumeration under chaos. Nothing between the top of `'poll` and the beat at `:3353` can
  `continue 'poll`, so the beat is reached on every completed pass. It is bounded by a clock
  (`duration_cycles(HEARTBEAT_MS)`), not a count, and the pass counter it carries genuinely
  distinguishes "entered but not progressing" from "silent".
- **No new count-as-duration, and no new unbounded wait.** Every wait added in this range is
  clock-bounded: `recv_timeout(ctx.duration_cycles(LINK_TICK_MS))`, the heartbeat interval, and
  `nic_req`'s `LINK_SECS` wall-clock deadline. The one timing regression is the opposite error -
  A10-3 replaced a *retrying* wait with a single-shot one.
- **The idle tick's cost is bounded and degrades free.** One op-3 IPC per second, carrying two MDIO
  reads inside nic-driver (`link_is_up`'s deliberate double read of the latching-low BMSR bit); with
  no nic-driver at all, `find_send_slot` misses, `reacquire_by_name` fails, and `nic_req` returns
  immediately with no wait. Nothing accumulates.
- **The `selfcheck.gsh` rewrite is grammatically sound** (setting A10-9 aside). `Vars`' scope stack is
  per-**function** (`services/shell/src/main.rs:1802-1805` - `scope_count`/`scope_alen`/`sp`), so a
  `for` body pushes no scope and `clockset = 1` assigns the outer `let mut` rather than shadowing it.
  `compare` (`:2237-2242`) is numeric when both operands parse, and `date epoch` prints a bare integer
  and nothing else (`cmd_date`, `:4853-4856`), so `$line > 0` is a genuine numeric test. The probe
  READS rather than repairs, which was the point of `8bf5b0f9`.
- **Audit 9's fixes are genuinely delivered, re-checked at HEAD rather than taken on trust.** A9-1:
  `serve_if_block` replies with `try_send_by_handle` and **consults** the verdict, logging a
  non-delivery (`xhci/main.rs:1455-1460`) - the §8.9 mutual-block is gone. A9-4: `fs_raw`
  (`shell/main.rs:8072-8092`) now drains, tags, and uses `request_with_reply_abortable`, so `drives`
  and its siblings are q-abortable and reacquiring like every other fs helper.
- **Zero `unsafe` in `services/` (§18.2)**, mechanically confirmed: `scripts/unsafe_check.py` passes -
  72 audited files, 1096 total unsafe lines, no unaccounted additions. Nothing in this range adds one.
- **Builds:** `cargo build -p net-stack -p nic-driver -p xhci -p fs -p shell -p block-driver --target
  aarch64-unknown-none --release` is clean; the only warnings are the pre-existing
  `improper_ctypes_definitions` on each `service_main`.

---

## Audit 9 - the USB stack after it left the kernel (2026-08-09, `feat/pi4-aarch64` @ 34bc8233)

**Scope:** the code the aarch64 port moved and rewrote in the last two days - `services/xhci` (`main.rs`
3764 lines, `msc.rs`, `topo.rs`, spawned out of the kernel at `e71e64a6`), the block path that now rides
it (`services/block-driver` `main.rs`/`usbdisk.rs`/`xhciblk.rs`), `services/fs` (`serve_filecap`, the
mount self-heal, `block_rpc`), the shell's storage and file-capability commands, and
`sdk/rust/src/service_context.rs` (the `request_with_reply*` reply-cap lifetimes after `1ecfd98e`).
Method: four auditors on the coupled edges (xhci read in full by the lead), every reported finding
re-verified against source at file:line before it was written down.

**Verdict: 2 HIGH, 5 MED, 5 LOW.** The reply-cap class closed by `1ecfd98e` is genuinely closed
everywhere (verified against the kernel, not just the SDK). The new defects cluster in one place: the
driver moved out of the kernel but the *waits* did not all come with it. A ring-0 driver could not
deadlock against its client and could not be starved by a full IPC queue; a service can, and two of the
new paths do.

| ID | Sev | Commandment | Finding |
|----|-----|-------------|---------|
| A9-1 | HIGH | VIII, V, §8.9 | **`xhci` answers a block request with a BLOCKING `send`, into a queue its caller cannot drain.** `services/xhci/src/main.rs:1435` - `let _ = ctx.send_by_handle(reply, ...)`, and `send_by_handle` is `ipc::send` = syscall 1, which **blocks when the target queue is full** (§8.2). The target is `block-driver`'s own endpoint, and `block-driver` is at that moment blocked in `request_with_reply` (`xhciblk.rs:46`) awaiting exactly this reply, so it is not draining. Fill those 16 slots from anywhere else and both services are stuck: `xhci` blocked in the send, `block-driver` blocked in the call, neither able to release the other. `EndpointDead` cannot rescue it - both are alive. §8.9 states the rule this breaks: in a protocol where A and B both send to each other, at least one direction MUST use `try_send`; here neither does. **It is reachable today:** `chaos` floods every service except `shell` and `fs` (`services/chaos/src/main.rs:369`), and its `flood` bursts 64 `try_send`s at a 16-deep queue precisely to saturate it (`:157-173`). The blocked service is the one holding the keyboard, so the failure presents as a dead machine, which is the one outcome nothing above the kernel is allowed to produce. CONFIRMED code path; the interleaving is PLAUSIBLE and is what `chaos flood-storm block-driver` exists to produce. Same shape on x86 (`ahci.rs` replies with `send_by_handle` into `fs`), so the class is not new - it is newly load-bearing, because the reply now comes from the service that also owns input. |
| A9-2 | HIGH | II, IX, §26.4 | **The disk-removal watch and the halted-endpoint repair are BOTH inert once two HIDs are bound.** `main.rs:3360` gates the hub scan on `hub_due && ndev < MAX_HID`, and `main.rs:3636` gates the fallback scan on `hub_due && ndev == 0`. With `MAX_HID = 2` (a keyboard *and* a mouse - the configuration the multi-HID support exists for), `ndev == 2` satisfies neither, so nothing in the pass runs: not the disk's own port probe (`:3386`), not `disk_absent_seen >= 200` (`:3492`), and not `PROBE_FAILS` (`:3585`), which is the ONLY site that increments the counter feeding the Reset Endpoint + Set TR Dequeue recovery added at `430770d1`/`592bb30b`. The comment at `:3634` justifies the second gate with "with a HID bound the loop above already covers this hub" - true for `ndev == 1`, false at exactly `ndev == MAX_HID`, because the loop above is *additionally* gated on `ndev < MAX_HID`. Consequence: plug in a mouse alongside the keyboard and a pulled stick is noticed only when a block operation fails, and a halted EP0 - the state that "never recovers on its own" and "stayed dead until a reboot" per the code's own comment at `:226` - is never repaired. This is the guard-that-cannot-fire class, and it silently disables the day's most important recovery. CONFIRMED by reading the nesting end to end. |
| A9-3 | MED | IX, III, §14.3 | **An `fs` self-heal remount orphans every open file capability, silently.** `Fs::mount` always initialises `open_files: [OpenFile { rid: 0, .. }; MAX_OPEN]` (`services/fs/src/main.rs:1457`), and both remount sites replace the volume wholesale without revoking the old table: `:503` (the per-request self-heal after `io_error_seen`) and `:887` (the retry-after-I/O-error inside `serve`). `fs` did not die, so no generation was bumped: the client's file cap stays **valid**, but its `rid` no longer resolves, and `serve_filecap` answers `FS_NOTFOUND` (`:1293-1296`) - indistinguishable from "the file was deleted". The resource is never revoked either, so it is orphaned in fs's delegated band. §14.3's rule is exactly this case ("everything derived from the previous incarnation must be re-established"), and here the *server* tore down the derived state while telling the client nothing. The correct answer is `resource_revoke` on every live `rid` before the swap, so the holder gets `CapRevoked` - a defined truth it can act on. CONFIRMED. |
| A9-4 | MED | VIII, IX | **`drives`, `drives flash`, `drives reset`, `drives label` are the one fs path that neither aborts nor reacquires.** `services/shell/src/main.rs:8080` (`fs_raw`) uses the bare `ctx.request_with_reply("fs", ..)` - a synchronous kernel CALL with no deadline and no `q` poll. Its four siblings do not: `fs_request` (`:8102`) uses `request_with_reply_abortable`, `fs_request_bounded` (`:8182`) a deadline, `fs_request_q`/`fs_op_q` the q-abortable forms whose own doc comment (`:8271`) names this bug ("parks the shell in the syscall for the WHOLE operation ... the only way out is cutting the power"). `fs_raw` also lacks the `reacquire_by_name("fs")` + retry every sibling has, so after an `fs` restart these commands report storage-unavailable and stay broken until some *other* command happens to refresh the shared peer cache. Callers: `:5786`, `:10451`, `:10528`, `:10556`, `:10661`. A dead `fs` returns promptly (`ReplyDead`); an `fs` that is merely slow - which, per A9-5, now means up to a minute and a half - hangs the prompt with no exit. CONFIRMED. |
| A9-5 | MED | VIII, §26.6 | **`bind_msc`'s TEST UNIT READY loop multiplies a 30 s clock budget by 16.** `main.rs:1605`: `for _ in 0..16 { test_unit_ready(..) }`. Each attempt is a BOT command = two `await_on_slot` waits (`msc.rs:328`, `:351`), each bounded by `XFER_TIMEOUT_MS = 30_000` (`msc.rs:92`). A device that stops answering mid-enumeration - the realistic case, a stick pulled during a bind - therefore takes up to **16 x 60 s** before the driver gives up, and the loop is on the same thread as the keyboard poll and the block server. The per-command bound is a clock and is correct; the *aggregate* is the count-as-duration error one level up, and the comment at `:1601` defends it with "the loop cannot outlive the device's own answer", which is true only if the device answers. The same composition applies to a single `bot()` (three stages x 30 s) inside `serve_block`, which is the ceiling A9-4's hang inherits. CONFIRMED by composition; PLAUSIBLE as a live event. |
| A9-6 | MED | VIII | **`fs`'s mount wait is still a retry COUNT.** `services/fs/src/main.rs:332` `MOUNT_MAX_ATTEMPTS = 1000`, used at `:351` (the boot capacity wait) and `:419` (the E_IO mount retry). When `block-driver` is not yet registered, `block_capacity` -> `block_rpc` -> `request_with_reply` returns `None` **immediately** (no peer slot, no wait), so each "attempt" is a failed lookup plus a `reacquire_by_name` plus a `yield_cpu` - the intended "give block-driver time to come up" window collapses to whatever 1000 fast failures cost. This is the identical bug both siblings in the same storage stack already converted, with the reasoning written out: `usbdisk.rs:56-71` (`BUSY_RETRIES` -> `BUSY_BUDGET_SECS`, "burned its whole budget in 173 ms") and `xhciblk.rs:53-67` (`CAPACITY_ATTEMPTS = 200` -> `CAPACITY_TIMEOUT_MS`, "the fourth time that has bitten this port"). `fs` was not converted. Mitigated, not fixed, by the request-driven self-heal at `:527`. CONFIRMED. |
| A9-7 | MED | VII, §26.6 | **A failed file-cap grant leaks a cap-table slot in `fs`.** `services/fs/src/main.rs:2646-2657`: `derive_cap(cap)` yields `c`, `send_with_cap_by_handle(reply, c, ..)` embeds it, and the failure path removes only `cap` (`:2653`) and revokes the resource - `c` is never removed. The kernel removes an embedded cap **only on confirmed transfer** (`kernel/src/syscall/dispatch.rs:990`, `:1078`), so on a failed send `c` stays in fs's table forever, stale. One slot per failed `Open`; the trigger is the client dying between its `OP_OPEN` and fs's reply, which is what a kill-storm produces. Bounded by the 64-slot table, after which `resource_mint`/`derive_cap` start failing and `fs` needs a restart. CONFIRMED as a code fact; rate PLAUSIBLE, not measured. |
| A9-8 | LOW | VIII, §26.6 | **The control and command paths in `xhci` still wait on an iteration count.** `main.rs:574`, `:689`, `:1120`, `:1979` all pass `next_event(.., 10_000_000)`, and each sits inside a `for _ in 0..8`, so the worst case is 80 million polls of DMA memory - "however long 80 million uncached reads happen to take on this board". These are hardware-completion waits (exempt from the service-wait rule) and `spin()` at `:357` plus `hub_port_status` at `:785` show the file already knows the clock-bounded form; these four are what is left. The consequence is not a hang but a driver whose enumeration stall is arch-dependent and unstated - the same property that made `PROBE_ANSWER_MS` necessary. CONFIRMED. |
| A9-9 | LOW | V, §26.7 | **Discarded reply-send verdicts, in two more places.** `services/xhci/src/main.rs:1435` and `services/fs/src/main.rs:1273` both `let _ =` the result of the reply send. This is A8-1 recurring on the new path: if the reply is ever not delivered, the client stays blocked, and `ReplyDead` does **not** cover it (that fires when the replier *dies*, not when a live replier's reply fails to queue). On the xhci site the discard also hides the A9-1 block. CONFIRMED. |
| A9-10 | LOW | VIII, III | **`request_with_reply_deadline*` has no drain at its own top.** `sdk/rust/src/service_context.rs:699-734`: the abortable and qhint variants drain stale replies before sending (`:750`, `:821`); this variant does not, while its own comment (`:721-729`) documents that a timed-out request's reply arrives later and will be read by the next `try_recv`. The busiest caller is `net-stack`'s `nic_req`, which has no correlation tag of its own, so a timed-out query's late reply can be read as the answer to a semantically different one. Not new on this branch, and the shell's fs tag (Audit 6) is the pattern that solves it. CONFIRMED. |
| A9-11 | LOW | III, §26.7 | **The topology model's table never evicts, and its "once" is not once.** `services/xhci/src/topo.rs:92-95` logs "port table full" on **every** probe once `MAX_PORTS = 32` is reached, though the comment says "say so once". Entries are keyed on `hub_slot`, which the controller reassigns from scratch on every `'reenum` pass, so a machine that re-enumerates repeatedly can accumulate distinct keys for the same physical port and fill a table that is never pruned - at which point modelling stops *and* the log spams the loop that also polls the keyboard, which is the exact cost the file elsewhere calls out (`main.rs:3387-3396`). Observation-only, so nothing else breaks. CONFIRMED. |
| A9-12 | LOW | VI, Invariant 9 | **Four file-scope mutable statics in the new driver.** `main.rs:332` `PROBE_FAILS`, `main.rs:1417` `SEEN`, `msc.rs:159` `NO_DISK_LOGGED`, `msc.rs:161` `READ_FAIL_LOGGED`. All are atomics (correctly - the first draft of `SEEN` used `static mut` and `unsafe_check.py` refused it, which the comment records), all are counters or log-once latches owned by one path, and none is read by another service - the same shape accepted with rationale in A6-2. Recorded rather than raised because `PROBE_FAILS` is the one that carries *control* meaning (it triggers an endpoint reset) rather than only diagnostics, and A9-2 shows that state living outside the poll-loop struct is harder to see gated off. |

**Clean results (verified, not assumed):**

- **Zero `unsafe` in `services/`**, mechanically confirmed: `scripts/unsafe_check.py` passes (72 audited
  files, 1095 lines, no unaccounted additions). The new 4.7k-line userspace USB driver adds none - it
  reaches the controller entirely through the SDK's `Mmio`/`Dma` wrappers (§18.1).
- **The `1ecfd98e` remove-by-stale-index class is fully closed, and the reasoning now holds against the
  kernel rather than against a comment.** `handle_call` (`dispatch.rs:1016-1114`) and
  `handle_send_with_cap` (`:938-990`) remove the embedded cap **only on confirmed transfer**; embedded
  caps are installed into a receiver's table at `recv`/`call`-dequeue time (`:335`, `:388`, `:448`,
  `:1095`), never at send time. Every surviving `remove_cap(reply_cap)` therefore fires only where the
  slot is genuinely still ours or provably empty, and each wait loop returns the instant `try_recv`
  yields, before it can reach a removal. Checked at all four SDK variants plus `fc_invoke`,
  `sock_invoke`, fs's `serve` (`:553-561`) and xhci's `serve_if_block` (`:1424-1436`) - the last two
  take a reply cap and send *on* it (no embed), so their `remove_cap` is correct.
- **The file capability is not over-granted.** `fs` mints with `want | RIGHT_GRANT` (`fs:2641`), which
  reads like a §8.5 rule-3 violation, but the kernel strips it: `narrow_embedded_for_receiver`
  (`dispatch.rs:296`) installs a delegated cap `without_grant()` for its receiver (SEC-7). The client
  gets exactly the rights it asked for and cannot re-delegate. Verified rather than trusted.
- **`with_busy_retry`'s count backstop does not undercut its clock budget.** `BUSY_RETRIES = 6000`
  against `BUSY_BUDGET_SECS = 30` looked like the A7-3 trap returning; it is not. Past
  `SPIN_ATTEMPTS = 64` each iteration calls `sleep_ms(1)`, and the kernel floors a sleep at one tick
  (`scheduler::cycles_to_ticks`, `.max(1)`), so 5936 paced attempts are ~59 s of capacity against a 30 s
  budget - the clock decides, as documented.
- **The clock conversions that landed this week are arithmetically right.** The unsigned deadline test
  `now.wrapping_sub(deadline) < (1 << 63)` (xhci `:789`, `msc.rs:276`, `xhciblk.rs:98`) is a correct
  "now >= deadline" under wraparound at every site it appears.
- **Builds:** `cargo build -p xhci -p block-driver -p fs -p shell --target aarch64-unknown-none
  --release` is clean (warnings only, all pre-existing `improper_ctypes_definitions` on
  `service_main`).

## Audit 8 - full-userspace sweep against the Commandments (2026-08-03, `main` @ v0.9.0)

**Scope:** all of `services/`, `sdk/`, and `examples/` (~31k lines), swept for the violation classes the
Commandments name, with attention to the shell paths the v0.9.0 console work touched.

**Verdict: 0 Commandment violations. 1 LOW finding.**

| ID | Severity | Commandment | Finding |
|----|----------|-------------|---------|
| A8-1 | LOW | VIII, V | The block-driver reply paths (`ahci.rs` lines 417-563, `sdhci.rs` 403-447) discard the verdict of `send_by_handle` when replying to a client. If a reply is ever dropped, the client stays blocked - and `ReplyDead` does **not** rescue it, because that fires when the *replier dies*, not when a live replier's reply fails to queue. So the one case the discarded verdict covers is exactly the case the kernel's death-wake cannot. Commandment VIII's "the truth must include failure" is about the waiter; this is the same requirement seen from the replier's side. Discarding is defensible when the client is already dead (nothing to do), but the code cannot currently tell the two apart. |

**Clean results:**

- **Zero `unsafe` in `services/` (§18.2)**, mechanically confirmed by `scripts/unsafe_check.py`.
- **Every service `sleep` is CPU conservation, not a correctness wait (VIII).** All 13 sites are
  drain-then-park (`xhci`/`ehci` idle loops), a paced repaint (`observe`, the shell's muted poll), or a
  demo tick (`counter`). The one timing-shaped wait, the shell's bare-Escape timeout, is terminal
  protocol - it counts scheduler quanta rather than cycles, which is why it is arch-portable.
- **The `reacquire_by_name` discards are correct (V).** `fs` lines 355/411/515 and net-stack/shell sites
  drop the boolean, but each sits inside a bounded loop whose *aggregate* failure is reported loudly -
  `fs` logs "block-driver did not report capacity after bounded attempts - coming up storage-unavailable"
  and keeps `storage_unreadable` set, so no failed recovery is converted into a silent success. This is
  the pattern Commandment V asks for and it is the LS1 work holding.
- **Contracts (IV):** `contract_check.py` reconciles all 6 contracts against their kernel
  `service_config` (memory limit, placement core, ipc_send).

## Audit 7 - the wait helpers, the block-driver visibility, and the chaos harness (2026-07-31, `feat/arm-usb-interrupt`)

**Scope:** `d8de0f2..HEAD` - the SDK wait-helper reversal, block-driver's slow-request reporting,
the chaos pacing fix, and fs's truncation report. Audited against the Ten Commandments.

**Verdict: 0 outstanding violations. 3 defects found and fixed DURING the period, 2 of them mine.**

| ID | Sev | Commandment | Finding |
|----|-----|-------------|---------|
| A7-1 | HIGH (mine, REVERTED) | II, IX | Making the four SDK wait helpers BLOCK broke x86 networking: `net-stack` and `nic-driver` both sit on core 1, so every exchange is **same-core request/reply**, and net-stack degraded to "no NIC MAC yet". Proven by a hardware A/B (identical build, one difference). Reverted @ e72596d. **The suite could not see it:** identity, file-cap, fs-restart and reply-dead are ALL cross-core - 57 green tests covering none of the changed case. Recorded in the code: a future attempt needs a same-core request/reply test FIRST. |
| A7-2 | MED (mine, FIXED) | VIII, X | The slow-request notice read `epoch_secs_monotonic()` - a SYSCALL - unconditionally at the top of `with_busy_retry`, so every block read/write paid one, plus one per busy iteration. Chaos pauses went 2.3% -> 20% of rounds: instrumentation added to explain a delay became a cause of one. FIXED @ 88418a8 - lazy (only once actually BUSY) and sampled (every 64th attempt). **Measure the cost of your instrumentation.** |
| A7-3 | HIGH (pre-existing, FIXED) | VIII | `PACE_YIELDS = 3000` paced chaos rounds by COUNTING YIELDS. A yield costs a full quantum when the yielding task is the only runnable one - exactly the state chaos creates - so on the Pi that beat was 30.03 s, and only on rounds where nothing else was runnable. A count is not a duration and its cost is arch-dependent: the harness was measuring itself in units it does not control. FIXED @ de46f55 (clock-bounded, yield count kept as the §26.6 cap). Same bug in `SHELL_SETTLE_YIELDS`. |
| A7-4 | note | V, §26.7 | block-driver now SAYS it is waiting (once per request, at 2 s, plus a resolution line). A correct 30 s wait that says nothing is indistinguishable from a dead board - it cost three chaos runs and two power-cuts of a healthy machine. A normal sub-millisecond request stays silent and pays nothing. |
| A7-5 | note | V | fs reports a truncated reply rather than silently sending a short one, and an empty reply is now impossible (it goes out as FS_ERR). Both close silent-corruption paths rather than adding behaviour. |
| A7-6 | note | III | The correlation tag (Audit 6) held under a 1M-round soak and repeated selfchecks; `shell: discarded an fs reply for tag N` fired on hardware exactly when an overtake occurred, confirming the mechanism is load-bearing rather than decorative. |

**Standing:** contract_check unchanged (no contract touched); selfcheck 349/0 on both arches; ARM 1M-round
soak in progress (55K+ clean at last inspection).

## Audit 6 - fs reply correlation, the buffered reply, and the blocking waits (2026-07-30, `feat/arm-usb-interrupt`)

**Scope:** `services/fs` (correlation tag, buffered reply + in-place retry, failure logging),
`services/shell` (tag helpers, all five fs senders), `sdk/rust/service_context.rs` (the four wait
helpers now block), `services/block-driver/usbdisk.rs` (absent vs busy), `examples/counter`.

**Verdict: 1 MEDIUM fixed, 1 MEDIUM accepted with rationale, 2 LOW recorded.**

| ID | Sev | Commandment | Finding |
|----|-----|-------------|---------|
| A6-1 | MED | V, §26.7 | fs's buffered `send` clamped with `bytes.len().min(out.len())` and set the length to the clamp - a **silent truncation**. Unreachable today (largest reply is `5 + MAX_FILE_BYTES` = 3561 against 4095 available after the tag), but if an arm ever grew past the buffer the caller would parse a header describing bytes that are not there: silent corruption, not a short read. FIXED: truncation is reported loudly and the short reply still sent, so the caller is wrong-but-not-hanging rather than wrong-and-silent. |
| A6-2 | MED | VI, Invariant 9 | `next_fs_tag()` uses a function-scope `static AtomicU8`. Before this change the shell had **zero** mutable globals (`HELP` is immutable), and nic-driver's `tx_fail` was deliberately threaded through its serve loop rather than made a file-scope static (userspace-audit A5). **ACCEPTED, recorded:** threading a counter through `fs_request`/`fs_request_q`/`fs_request_bounded`/`fs_op_q`/`fs_raw` reaches dozens of call sites, and correctness does not depend on the counter's *value* - only on successive requests differing, which any monotonic source satisfies. It is owned by exactly one path, is a count rather than a resource (§6.2's distinction), and is never read by another service. Revisit if the shell ever needs a second one. |
| A6-3 | LOW | III | `drain_stale_fs_replies` is now redundant: the tag supersedes arrival-order hygiene. Two mechanisms for one job. Kept as belt-and-braces for the first hardware runs; retire once the tag has soaked. |
| A6-4 | LOW | VIII | `fs_take_tagged` waits via `recv_abortable_deadline`, which is q-abortable - so a stray `q` during a non-interactive path (the history write-through) could abort the wait. Bounded and harmless (the caller treats it as no reply and retries), but the abortability is inherited rather than chosen. |
| A6-5 | note | VIII | The four wait helpers now BLOCK with a short timeout. Correctness still comes from truth (the reply), not time: the timeout only paces the console poll, and the deadline is a bound, not a decision. **But** they now depend on `scheduler::cycles_to_ticks` flooring at 1 tick - if that floor were ever removed the loops would hot-spin with no yield at all. Documented at both ends. |
| A6-6 | note | IX | The in-place retry re-establishes derived state (a fresh mount) before retrying, and is limited to read-only opcodes so a partially-applied mutation is never re-applied. A retry after `reacquire_by_name` takes a FRESH tag - reusing it would accept the dead instance's late reply as the new one's answer. |
| A6-7 | note | IV | No contract changed. The tag is a wire-format field between two services that already talk; no new capability, no new named peer. |

## North-star for services

A service is **identity, not location** (Commandment V): it must be **prepared to fail and to
restart**, and its clients must **reacquire and retry**, never crash or hang. Concretely, for every
service the audit asks:

- **VIII (wait on truth, INCLUDING failure).** Does every wait on a *dependency* observe failure as a
  first-class truth (a `ReplyDead`/`EndpointDead` wake, a bounded deadline, or a q-abort), so it can
  never hang forever if the peer dies or goes silent? Hardware/protocol timing waits (USB reset holds,
  AHCI/MMIO completion spins, PHY link-up) are **exempt** - they are bounded hardware timing, not a
  service-to-service correctness wait.
- **IX (plan for recovery).** After a dependency restarts (generation bump -> stale cap), does the
  client reacquire **by name** via the kernel directory and retry, on **every** path a user drives -
  not just the happy path?
- **VII (no ambient authority).** Is every privileged action gated by an explicit, kernel-validated
  capability, never by identity/ancestry/inheritance?
- **III (do not duplicate truth).** Is every stored fact either the one irreducible source or a
  **reconcilable, subordinate** derived view of it - never a second truth that can silently lie?
- **IV / VI / X.** Authority expressed through the contract; no shared mutable memory; complexity in
  the layer that owns it.

### Severity

- **HIGH** - a live wedge, hang, corruption, or authority escape reachable by ordinary use/chaos.
- **MED** - a real defect that degrades recovery/clarity but does not (yet) wedge or corrupt.
- **LOW** - hygiene, weak-test, or doc-drift; latent, not active.

---

## Audit 1 - 2026-07-12 (full userspace sweep)

Method: 6 parallel auditors, grouped by coupling - **block-driver+fs**, **nic-driver+net-stack**,
**supervisor+logger**, **xhci+ehci**, **shell**, **chaos/observe/probe/mem-pressure** - each reading
its crates in full and triaging against the commandments above, with the coupled dependency edges
(the VIII/IX failure-and-recovery paths) as the highest-value target. Confirmed findings spot-verified
against source.

**Result: 0 HIGH, 8 MED, 8 LOW. No hang, corruption, shared-state, or ambient-authority violation on
any critical path.** The two coupled *storage* and *network* edges are VIII-airtight (no dependency
wait can hang). The real defects cluster in two places: **incomplete recovery** (net-stack and the
supervisor do not retry every path to satisfaction) and **contract drift** (privileged hardware/mint
authority granted by kernel name-match, not expressed in the contract). Two shell pipe/invoke paths
still use a bare blocking `recv` that a mid-stream peer death could hang.

### Fix log (Audit 1 remediation)

Staged high-priority-first. Status updated as fixes land on `feat/dell-wyse-5070-goldmont-plus`.

| Item | Status | Commit / note |
|------|--------|---------------|
| **M1** drain_service bare recv | **FIXED** | `b4f212c` - SDK `recv_abortable_deadline`; happy-path drain unchanged, adds Timeout/Aborted wakes. Verified: files pipe checks green |
| **M2** fc_invoke/sock_invoke bare recv | **FIXED** | `b4f212c` - same primitive; verified `osdev test file-cap` 10/0 |
| **M3** net-stack interactive reacquire | **FIXED** | `c54b5dc` - `nic_req` reacquires on `SendFailed` only (SDK `DeadlineOutcome`); no-regression proven vs baseline shell-test; recovery mirrors the proven dhcp/udp reacquire pattern (live demo blocked by QEMU-11 ICMP flakiness) |
| **L1** driver-death mislabeled "no link" | **FIXED** | `c54b5dc` - subsumed by M3 (reacquire returns real link status) |
| M4 net-stack identity cache reconcile | **DEFERRED** | Trades against the deliberate instant-replug design; needs a real multi-subnet network to validate - not doable away from hardware |
| M5 supervisor steady-state respawn retry | open | Stage 3 |
| **M6** block-driver contract drift | **FIXED** | `1cf...` - removed the dead `hw_pio` lie (read by nothing; kernel grants AHCI MMIO/DMA by name); contract now tells the truth |
| M7 by-name grant (T1) | **DECIDED, deferred** | Resolution chosen: reconcile + drive-grants-from-declaration (see T1 below). Scheduled AFTER the small items |
| **M8** probe unsafe untracked | **FIXED** | `4428c92` - probe made unsafe-free (fuzz + faults -> audited SDK `adversarial` module, §18.1 amendment); `unsafe_check.py` now scans `services/`. adv 15/0, fuzz 8/0 |
| **L4** 256-slot scan | **NOT A BUG** | kernel `MAX_TASKS=224` (fixed) < 256; scan over-covers |
| **L2** FS_UNAVAIL/FS_DENIED collide | **FIXED** | `cf8fb08` - FS_DENIED now 5 (distinct); file-cap 10/0 |
| **L3** logger stub vs docs | **FIXED** | `cf8fb08` - logger/CLAUDE.md now honest about current vs future behaviour |
| **L5** chaos orphans mem-pressure | **FIXED** | `cf8fb08` - a run reaps prior-run orphans at start |
| **L6** probe BA6 weak test | **FIXED** | `3a748ff` - BA6 drains caps between cycles (all 5 real) |
| **L7** build_uptime_table inline | **FIXED** | `cf8fb08` - `#[inline(never)]` added |
| **T1/M7** contract = source of truth | **DONE (both phases)** | Scope corrected: 217 kernel configs but only 6 have contracts, so this reconciled the 6 (not a full kernel-shrink). **Phase A `334502c`**: `scripts/contract_check.py` (CI-wired) reconciles each `.toml` vs kernel `service_config` (memory/placement/ipc_send) - drift IMPOSSIBLE; fixed 4 live divergences (logger+supervisor memory, logger serial-MMIO lie, supervisor ipc_send lie). **Phase B `2dab12b`**: the 7-site by-name hardware/mint scatter (`name == "block-driver" && AHCI_FOUND`) collapsed into one `HwClass` abstraction + `service_hw(name)` declaration; grants are field-driven; drivers declare `hw_device`/`resource_mint` in the `.toml` (schema + check extended). BAR address stays PCI-scan-resolved. Verified: AHCI/NIC/xHCI + mint all work, identity 24/0 |

> **Storage-stack prerequisite fixed (bonus, not an audit finding): `fe59cbf`.** Verifying any fs
> fix in QEMU was blocked by a block-driver AHCI stall - it probed every implemented port (QEMU's HBA
> reports `PI=0x3f` = 6 ports) and spent `wait_port_ready`'s full slow-establish budget (~4M MMIO
> reads + a COMRESET) on each *empty* port. On hardware an MMIO read is ~ns so it's invisible (your
> Wyse is fine); under QEMU 11.0.50's slow TCG MMIO it blew the boot window and `fs` never mounted, so
> every fs test (file-cap, files, fs-restart) timed out. Fix: stop the scan at the first SATA disk
> (block-driver uses exactly one) - keeps full slow-establish robustness for the disk's port, skips
> empty ports. `osdev test file-cap` 10/0 (was: fcap timed out). A real latent driver bug QEMU
> surfaced (Commandment II). Note: the QEMU `files` suite's residual failures are host-load timing in
> the heavy gsh section (the gsh engine itself passes `osdev test script` 4/0), not a code defect.

### MED findings (fix these)

#### M1. [VIII] `services/shell/src/main.rs:5854-5861` (`drain_service`) - bare `ctx.recv()` on the general pipe path can hang forever

The pipe-through-a-service drain is `for _ in 0..512 { let msg = ctx.recv(); ... if p == [PIPE_EOT] break }`.
The `512` bounds *iterations*, not the *blocking wait per iteration*: `ctx.recv()` blocks on the
shell's own endpoint (and loops on error), it is not a kernel CALL, so there is no `ReplyDead` wake, no
deadline, no q-abort. **Trigger:** `producer | badfilter` where the filter registers its input endpoint
(passing the `FILTER_WAIT` gate) then page-faults or wedges *before* emitting `PIPE_EOT` or any output.
The shell blocks forever on the first `recv()`; the prompt never returns and the keyboard reads dead -
the exact wedge conventions rule 12 forbids. This is the broad pipe path (`is_pipe_producer_service` /
`is_record_producer_service` / filter stages), not a diagnostic. **Fix:** replace the bare drain with a
`try_recv` + console-q-poll + deadline loop (mirror `request_with_reply_abortable`), or add a shared
SDK `recv_abortable_deadline`. Highest-priority of the shell findings.

#### M2. [VIII] `services/shell/src/main.rs:7408` (`fc_invoke`) and `:4763` (`sock_invoke`) - bare `ctx.recv()` after a fire-and-forget `resource_invoke`

`resource_invoke` (syscall 31) returns Ok/Err on the *send* only; the reply is then awaited with a
plain `ctx.recv()` - again no `ReplyDead`, no deadline, no q-abort. If fs/net-stack dies after receiving
the badged invocation but before replying, the shell hangs. **Blast radius is limited** (`fc_invoke`/
`fc_open` are used only by the `fcap` self-check; `sock_invoke` only by the `sock` demo), but both are
user-invokable and can wedge the prompt if the owner is killed at the wrong instant. Contrast the
correct paths: file commands use `fs_request_q` (q-abortable), report saves use `fs_request_bounded`
(deadline), net commands use `net_query` (abortable) - only the resource-cap invoke path regressed to
bare recv. **Fix:** same failure-aware wait as M1.

#### M3. [V + IX] `services/net-stack/src/main.rs` - interactive paths never `reacquire_by_name`, so a configured stack does not self-heal after a nic-driver restart

Only `dhcp_discover` (:153), `udp_roundtrip` (:325), and `run_dance`'s ARP loop (:553) reacquire the
driver on a stale cap. The interactive diagnostic surface the user actually drives - `link_is_up`
(:585-590), `ping` (:461,:488-492), `dns_resolve` (:227,:276-280), `arp_resolve` (:390,:402-406) -
retries against the same stale send cap and **never calls `reacquire_by_name`**. **Trigger:**
`chaos max-carnage nic-driver` respawns the driver (generation bumps -> cached cap is `EndpointDead`);
with the stack already configured, `ping`/`net`/`dns`/`net arp` report failure/"no link" and never
recover; only a manual `net renew` (or a socket send) re-dances. No hang (the waits are bounded
deadlines - VIII holds), but recovery is **incomplete and inconsistent** with the stated design intent
("reacquire and retry after the driver restarts"), which currently holds only for DHCP/socket. **Fix:**
on a `None` whose cause is a *send failure* (dead cap, not a plain deadline), `reacquire_by_name("nic-driver")`
and retry once in each interactive path - or drop the `!have_mac` reconcile gate (see M4).

#### M4. [III] `services/net-stack/src/main.rs:624` - cached `have_mac`/`gw_mac`/`our_ip` is a second truth that suppresses its own reconcile path

The auto-configure reconcile (`run_dance` in place, which *does* reacquire) is gated
`badge.is_none() && !have_mac && ...`. Once configured, `have_mac` stays `true` forever unless
`net renew` is issued, so the identity cache (`our_ip`/`gw_mac`/`dns_server`, :636-638,:672-675) is
never *automatically* reconciled against live DHCP/ARP truth. **Trigger:** the link drops and returns
on a **different** subnet (replug into another network); the stale IP/gateway/DNS are used verbatim by
`ping`/`dns`/`udp_roundtrip`. The cache is subordinate (a manual repair path exists) but not
auto-reconciled, so a live configured stack can silently lie after a network change. It is precisely
`have_mac == true` that also disables the one path in M3 that would reacquire. **Note:** the live-link
*up/down bit* discipline **does** hold (`net_status`/`ping` re-read `link_is_up` and clear the flag);
the gap is the IP/gateway/DNS **identity**, not the link state. **Fix:** re-dance on a link-up
transition (or a stale-cap send failure), not only when `!have_mac`.

#### M5. [IX] `services/supervisor/src/main.rs:539-607` - an isolated transient respawn failure in steady state is loud but not retried-to-satisfaction

Each death arm logs on a failed respawn (`"supervisor: fs restart FAILED"`) and moves on; the only
backstop is a single `reconcile(...)` pass (:607) doing one `respawn_managed` attempt. The
retry-until-satisfied loop `converge()` (MAX_TRIES=7) runs only at **supervisor-respawn boot** (:502),
not per-death in steady state. **Trigger:** a lone `kill fs` coincides with a momentary allocator
low-water mark; the respawn fails once, and because no other managed service dies afterward, fs is
never retried and stays dead. Loud (§26.7 satisfied) but not recovered (IX weak). **Fix:** run a
bounded `converge`-style retry after a failed steady-state respawn, not only at boot. **Note:** the
feared "respawn panics on transient NoMemory" defect is **absent from the supervisor service** (grep:
no `panic!`/`expect`/bare `unwrap`) - that concern lives in the *kernel* respawn path and is tracked as
kernel-audit C3 (already fixed there).

#### M6. [IV] `services/block-driver/contracts/block-driver.toml:9-13` - contract declares PIO, service is MMIO + DMA

The contract declares `hw_pio = ["0x170+0x8", "0x376+0x1"]` with the comment "No DMA, no MMIO - a PIO
driver is least-privilege by construction." The shipping service is a pure **MMIO + DMA AHCI** driver
(`ctx.mmio()` ABAR at `main.rs:39`; `ctx.dma_region()` at `ahci.rs:598`) - stale authority from the
retired ATA-PIO bring-up backend. **Trigger:** a reviewer reading the contract to answer "what can
block-driver reach?" concludes PIO-port-only/least-privilege; the running service actually holds an
MMIO window plus a DMA arena (kernel-equivalent reach on a machine without an IOMMU, §6.4). The
contract is not the source of truth for this service's authority, which is what IV forbids
(`osdev validate` passes because it checks only TOML structure, §13.4). **Fix:** update the contract to
declare the real MMIO/DMA shape (see cross-cutting T1 for the by-name-grant tension).

#### M7. [IV/VII, PLAUSIBLE] `services/fs/contracts/fs.toml` + `kernel/src/task/mod.rs:3390,3510,3554` - privileged authority granted by service *name*, not by declared contract

fs mints file capabilities (`resource_mint`, requiring `RESOURCE_MINT`) but `fs.toml` declares only
`ipc_*`/`log_write`; the mint cap is granted in the kernel spawn path by matching `name == "fs"`
(commented "the same e1000-BAR-style by-name kernel grant, never a contract field"). Likewise
block-driver's MMIO/DMA are granted gated on `name == "block-driver"`. So the *granting decision* is
authority-by-identity and the contract omits the granted authority. Marked **PLAUSIBLE**, not a hard
invariant-1 break, because at **runtime** the service still holds an explicit unforgeable capability in
its cap table and cannot act without it (no ambient-authority-*at-use*). The defect is that the
contract is not the authority's source of truth - see cross-cutting theme T1. **Fix:** decide T1 (make
the contract express hardware/mint grants, or document the by-name grant as the sanctioned mechanism).

#### M8. [§18.2] `services/probe/src/main.rs:908,1808,2016,2034,2051,2060,2091` - `unsafe` in a userspace service, untracked by the audit CI

probe issues raw `syscall` via inline `asm!` (`probe_raw_syscall`) and performs deliberate faults
(`read_volatile(null)`, a non-canonical read) to drive the fuzz/adversarial regressions. §18.2 forbids
`unsafe` in "all userspace services" and §21 rejects such PRs; §18.1 permits raw-ABI `unsafe` only in
the SDK. The unsafe-audit CI (`scripts/unsafe_check.py`) scans only `kernel/src/`, so probe's `unsafe`
is **untracked**. Each block carries a `// SAFETY:` comment (§18.3 met) and the `unsafe` is genuinely
necessary - you cannot fuzz raw syscall numbers/args or trigger a ring-3 #GP/#DE/#PF through the safe
SDK. So this is a **spec gap**, not sloppy code: a necessary-but-unsanctioned exception. **Fix:** record
a §18.5-style exception for probe (test-only harness that must reach the raw ABI) and extend
`unsafe_check.py` to cover `services/` so the exemption is explicit and tracked.

### LOW findings

- **L1. [III/obs] `net-stack/src/main.rs:698-699,587-588`** - a nic-driver death is reported to the
  user as `[2] "no link"` (any `None` from `link_is_up` maps to "down"), conflating a dead driver task
  with an unplugged cable. Misleading status; the surface symptom of M3. A distinct code (or
  reacquire-then-retry) would tell the truth.
- **L2. [X] `fs/src/main.rs:164,166`** - `FS_UNAVAIL = 4` and `FS_DENIED = 4` share a value; a file-cap
  client cannot distinguish "storage unavailable" from "permission denied" by the reply code. Latent
  (different code paths today), not an active bug. Give them distinct values.
- **L3. [logger] `logger/src/main.rs:16-29`** - the logger never calls `drain_kernel_ring_buffer` and
  drops every message (`loop { let _ = ctx.recv(); }`); its own header and `logger/CLAUDE.md` describe
  draining + formatting. Harmless (services log via `ctx.log`, which writes the ring buffer + serial
  directly), but the docs oversell the stub. Either implement the drain or trim the docs to match.
- **L4. [ceiling] `supervisor/src/main.rs:182,196`** - `managed_alive`/`name_alive` scan task slots
  `0..256`. Now that core/arena sizing is fully dynamic, a live task count past 256 would read a
  high-slot managed service as "not alive" and trigger a duplicate respawn (rejected by the kernel
  singleton guard - no corruption, just a wasted attempt + misleading log). Widen or make dynamic.
- **L5. [V/IX] `chaos/src/main.rs:416-423`** - chaos reaps its spawned `mem-pressure` children only on
  its clean-exit path; an *external* `kill chaos` mid-run orphans parked `mem-pressure` tasks holding
  their allocations until a later external kill. Bounded (one spawn/round, chaos excludes itself from
  its victim pool), so LOW. A later chaos run does not adopt/reap pre-existing orphans.
- **L6. [III/test] `probe/src/main.rs:1948-1962`** - BA6 claims "5x cap-table fill" but never drops the
  caps between cycles, so only cycle 0 fills; cycles 1-4 are no-ops. Weak test (echoes the repo lesson
  that a trivially-passing test is a weak test), not a resource leak.
- **L7. [§26.6.1] `shell/src/main.rs:4850` (`build_uptime_table`)** - the lone record-builder called
  from `pipe_run` that omits `#[inline(never)]`; its siblings all carry it to keep their frame out of
  `pipe_run`'s 64 KiB `Stream` frame (the PUSER-PF stack lesson). Its frame is small so overflow is
  unlikely, but add the attribute for uniformity.
- **L8. [VIII/SDK] `sdk/rust/src/service_context.rs:~331,338` (`ctx.recv()`)** - the recv wrapper does
  `loop {}` on a recv error rather than failing loudly. Unreachable in the audited services (a service's
  own recv endpoint is stable while it lives), and it is SDK code, not a service - noted so the M1/M2
  fix (a shared abortable-deadline recv) can also close this.

### Clean per commandment (verified, not assumed)

| Service group | II/III/IV/V/VI/VII/VIII/IX/X |
|---|---|
| **xhci + ehci** | **CLEAN on every commandment** - zero `unsafe` in either driver (all hardware via SDK safe wrappers), authority explicit/kernel-granted, restart re-enumerates from pristine hardware, every wait bounded hardware-timing, device tables reconciled from PORTSC each pass |
| **block-driver + fs** | VIII **airtight** (`block_rpc` -> `ipc::call`, `ReplyDead` wake; mount bounded-then-degrade to `FS_UNAVAIL`); III (tree = irreducible, bitmap+count reconciled by `check()`); V/IX (journal `recover()` on mount); VI; VII-at-use; X. Defects are IV contract-drift only (M6/M7) + L2 |
| **nic-driver + net-stack** | VIII **CLEAN** (every net-stack->nic-driver wait is `request_with_reply_deadline`, reply cap reclaimed on send-fail and timeout); VI; VII (`RESOURCE_MINT` gated, reply caps reclaimed); IV; X (no IP logic in the driver, no register poking in the stack). Defects are M3/M4/L1 recovery+identity-cache |
| **supervisor + logger** | V (adopt-not-duplicate via `acquire_send_grant_cap`, `converge` reconcile at respawn); VIII (blocks on real death, no fixed sleep for correctness); III (name-map subordinate to `task_stat`); VI; VII; IV; X. Defects: M5 + L3/L4 |
| **shell** | III (`net_status` gates every line on live link `p[7]`, refuses stale 10.0.2.x); VII (kill/restart via SERVICE_CONTROL, files via file caps); VI (one immutable `static`); IX (reacquire-by-name+retry on the fs/net paths); X; §26.6.1 (zero heap, streaming, 73 loud ceilings). Defects: M1/M2 + L7 |
| **chaos/observe/probe/mem-pressure** | VII **CLEAN/CONFIRMED** (every privileged op cap-gated; observe holds INTROSPECT only; the prior `sv_floodcap`-on-kill leak is fixed; count != resource across uncapped rounds); VIII; VI; III; IV; X. Defects: M8 + L5/L6 |

### Cross-cutting theme

**T1 - authority granted by service name, not by contract (IV).** The recurring pattern under M6/M7:
hardware caps (block-driver MMIO/DMA, nic-driver MMIO/DMA/IRQ, xhci/ehci MMIO/DMA/IRQ) and mint caps
(fs/net-stack `RESOURCE_MINT`) are granted in the kernel spawn path by matching the service **name**,
not declared in the service's contract. Runtime remains explicit-cap (no ambient authority *at use*),
and the auditors split on whether this violates IV: the xhci/ehci and nic-driver auditors called it the
accepted "by-name kernel grant" pattern (consistent across all bare-metal drivers); the block-driver/fs
auditor flagged it MED because the **contract stops being the authority's source of truth** (§13.6:
caps are "populated *from the contract* at spawn"). This is one **design decision** to settle, not five
per-service bugs. **RESOLVED (2026-07-12): reconcile + drive-grants-from-declaration.** The deeper
finding is structural: the kernel's hand-written `service_config(name)` match (`kernel/src/task/mod.rs`)
is a SECOND source of truth alongside the `.toml` contracts (the kernel is `no_std` and cannot parse
TOML at spawn, so a compiled-in table is unavoidable), and the hardware grants are a THIRD scatter
(hardcoded `name == "block-driver" && AHCI_FOUND` in the spawn path). The fix (Commandment III - one
authored source, a reconciled derived view, §26.4): (1) add honest hardware-need declarations to the
driver `.tomls` + schema; (2) drive the spawn-path hardware grants from the *declared* need (keyed by
the name in both), removing the ad-hoc name scatter; (3) an osdev CI check that every `.toml` matches
its kernel `service_config`, so drift is impossible - what runs cannot differ from what is declared.
The runtime BAR *address* stays PCI-scan-resolved (a hardware *location* is a different irreducible fact
from the *authorization*, so no truth is duplicated). Scheduled after the remaining small items (M8 +
LOW). M6 (block-driver's contract *contradicting* its real access) was fixed immediately, independent
of this.

---

## Audit 2 - 2026-07-13 (post-v0.4.0 re-audit)

Method: 4 parallel auditors grouped by coupling - **shell interpreter/library**, **shell
pipes/net/observe**, **net-stack+nic-driver+supervisor recovery**, **library scripts+contracts+fs+SDK**
- each triaging its crates against the Ten Commandments, then the lead **re-verified every confirmed
finding against source** before recording it. Motivation: the entire v0.4.0 release is new
userspace - the gsh system library (5 baked PATH-like scripts), the `whatis`/`wait` utilities, the
**POSIX parameter-cipher retirement** (`$arg1..$arg9`/`$args`/`$argcount`/`$self`), the observe q-poll
change, and `net dns`/`ping` returning `Err` on a failed probe. The audit's job is to prove the new
surface opened no wedge/hang/authority gap and that Audit-1's fixes hold.

**Result: 0 HIGH, 4 MED, ~11 LOW.** No hang, corruption, shared-state, or ambient-authority violation on
any critical path. The MED cluster is instructive: **two of the four are direct consequences of the
param-cipher retirement itself** (the reserved words leak through the two binding sites the `let` guard
did not cover; and an unrelated but adjacent unbounded native recursion in `eval_cond`), one is a
**residual of the Audit-1 M5 fix** (the retry the fix added everywhere was not wired into the
dropped-notification backstop), and one is **M4 still deferred**. Audit-1's M1/M2/M3/M5 and T1 all
verified intact; the storage stack (fs/block-driver) VIII+III still clean; the library scripts obey the
utility conventions.

### Fix log (Audit 2 remediation)

| Item | Status | Note |
|------|--------|------|
| **U1** eval_cond unbounded native recursion | **FIXED** | §26.6.1 - `eval_cond` strips `!` iteratively (parity) + `eval_cond_bare`; selfcheck `!!`/`!!!` parity asserts |
| **U2** reserved words shadowable as for-var/fn-param | **FIXED** | III/§26.4 - reserved check moved to the `define` funnel (`VarErr::Reserved`) so `let`/loop-var/fn-param all refuse loudly; QEMU-verified (`fn f self`/`for args` refused, no stale read) |
| **U3** supervisor `reconcile()` backstop single-shot | **FIXED** | IX - `reconcile` now calls `respawn_retry` (the M5 backstop gap closed) |
| **M4** net identity-cache reconcile | STILL DEFERRED | trades against instant-replug; needs a real multi-subnet net |
| **U4** probe q-abort returns Ok | **FIXED** | VIII/truth - `Aborted -> Err(ShellError::Unknown)` in `net_dns`; ping tail `recv == 0 -> Err` so `online` doesn't false-pass |
| **U5** args past 9 silently dropped | **FIXED** | §26.6 - `parse_params` now takes `ctx` + emits a loud "only the first 9 arguments are available" line when a 10th remains |
| **U6** no compile guard baked-script < 64 KiB | **FIXED** | hygiene - `const _: () = assert!(SELFCHECK_GS.len() < 65536)` + a `while` const-loop over every LIBRARY entry |
| **U7** shell-test dead DNS assertion | **FIXED** | test-drift - `shell_test.rs:214` now matches the live "returned no A record" / "no reply from the DNS server" / "did not answer the resolve" lines |
| **U8** observe q-loop break checks `.valid` not name | **FIXED** | rare slot-reuse - break also on `state == Dead` OR a `name_str() != "observe-live"` mismatch |
| **U9** OUR_MAC hardcoded, not reconciled | **FIXED + HW-SIGNED-OFF (T630)** | III - net-stack now LEARNS its MAC from nic-driver's `[3]` reply (`learn_our_mac`, bytes 1..7) and threads it as `our_mac` through every frame builder (mirroring `our_ip`, no globals); the hardcoded const is deleted. Zero MAC (no NIC) -> stay unconfigured. **T630 (2026-07-13): the RTL8168's real MAC `7c:d3:0a:2b:b0:e3` (not 52:54) drove the full dance - DHCP leased `192.168.4.98`, ARP resolved the gateway, ICMP ping OK, an interactive `ping` ran 14/16 replies @ ~707us avg, no panic.** ICMP even works on HW where slirp cannot. See the U9 note below |
| **U10** open-socket grant-fail replies nothing | **FIXED** | inv12 - the `!granted` arm now `try_send [0]` so the caller wakes instead of blocking on a reply that never comes |
| **U11** net-stack calibrate_tsc_hz unbounded RTC spin | **FIXED** | VIII-edge - each wait bounded by a `SPIN_MAX` yield count; a frozen clock returns 0 (the existing RTT fallback) instead of hanging boot |
| **U12** auto-config gate covers only net/ping | **FIXED** | IX - gate now covers ops 0/1/3/6 (net/dns/ping/arp), every network-using op; op 8 renew already forces a dance, op 2 open only mints |
| **U13** contract_check CONTRACTED hand-list | **FIXED** | III - now glob-derived from `services/*/contracts/*.toml`; a new service's contract is reconciled automatically |
| **U14** example tomls stale pre-T1 doctrine | **FIXED** | III/IV - e1000 + resource-server tomls/CLAUDE.md now describe the current `service_hw` table (and sibling `service_privileges`), not the scattered `if name ==` branch |
| **U15** six privileged grants still name-keyed | **FIXED** | VII/IV - all six (SPAWN/CONSOLE_PUSH/INTROSPECT/SERVICE_CONTROL/REBOOT/ACQUIRE_ANY) centralized into ONE `service_privileges(name, is_probe)` table (the `service_hw` doctrine); ServiceConfig field-promotion rejected (218 all-false rows, §26.13). adv A11/A12/A13 green |
| **L8** SDK recv()/console_read() `loop{}` on error | **FIXED (partial, by design)** | inv12 - the reachable `console_read` slot-guard now logs loudly then parks; the magic-mismatch guards park with a comment (a corrupt ctx can't be logged through - the service-level analog of kernel halt-on-corruption, §6.2) |

> **Hardware sign-off - 2026-07-13 (HP T630, AMD GX-420GI).** The audit branch was flashed and booted
> on real silicon (clean `--mode identity` image; serial `build/serial_output.log`). Relevant to this
> doc's **U15** (`service_privileges` centralization): the T630 boot exercised it live - every service
> that needs a privileged cap got it (supervisor spawn, probe kill/introspect for the self-run identity
> checks), all self-run tests passed, and the negative cap-gating pins (A11/A12/A13) hold in QEMU. No
> panic/exception; cross-core ping/pong ran clean for minutes. Full on-silicon detail is in
> `audits/kernel-audit.md` "Hardware sign-off". The shell/net-stack userspace fixes (U4-U14) were verified
> in QEMU (script + selfcheck 4/0); the `--mode identity` hardware image does not run the shell, so their
> hardware exercise rides the general v0.4.0 selfcheck soak, not this identity boot.

### MED findings (fix these)

#### U1. [§26.6.1] `services/shell/src/main.rs:2253` (`eval_cond`) - unbounded native recursion on leading `!`

**Verified.** `eval_cond` handles a negated condition with `return !eval_cond(ctx, cwd, rest.trim(),
...)` - genuine native recursion once per leading `!`, with `depth` (the *script* nesting level, not a
recursion bound) passed unchanged. Every other gsh construct uses an explicit bounded stack (the "no
native recursion §9" rule); this is the one that breaks it. The frame carries up to 3x1 KiB `ExpBuf`.
**Trigger:** `edit` a script `if ` + ~500 x `!` + ` 1 == 1 { echo x }` (SCRIPT_MAX=7112 admits ~7000
`!`), `run` it -> the ~256 KiB user stack overflows -> PUSER PF -> shell crash + respawn (the
`[[project-shell-stack-pipe]]` failure class). **Fix:** count leading `!` iteratively (parity), one
non-recursive evaluation.

#### U2. [III / §26.4] `services/shell/src/main.rs:3246` (`set_loop_var`) + `:3282` (`dispatch_call` param bind) - reserved parameter words can be silently shadowed

**Verified.** `valid_var_name` (:1961) correctly refuses `args`/`argcount`/`self`/`arg1..arg9` for
`let` - but `set_loop_var` (`self.define(name, ...)`, no validation) and `dispatch_call`'s param loop
(`vars.define(pname, av, false)`, no validation) bypass it. Since `push_ref` resolves reserved words
*before* variables (:1893), the binding is accepted and then **unreadable**: the body reads the outer
script's params instead. **Trigger:** `run /s.gsh one two` with `s.gsh` = `fn greet self { echo $self }`
+ `greet world` prints `/s.gsh`, not `world`; `for args in range 3 { echo $args }` prints the script's
args each pass. A direct consequence of the cipher retirement - the guard covered `let` but not the two
other binding doors. **Fix:** apply `valid_var_name` in both sites, refusing loudly like `stmt_let`.

#### U3. [IX] `services/supervisor/src/main.rs:246` (`reconcile`) - the dropped-notification backstop was not given the M5 retry

**Verified.** The Audit-1 **M5** fix added `respawn_retry` (5 attempts, :223) and wired it into every
steady-state death arm (:560-616). But `reconcile()` - the backstop that recovers a service whose death
*notification was dropped* (16-deep endpoint overflow under a storm) - still calls single-shot
`respawn_managed` (:246). **Trigger:** a `chaos max-carnage` storm drops fs's death notification; the
backstop `reconcile()` respawn of fs hits a transient NoMemory (storm reclaim in flight); fs stays dead
forever (no further death arrives to re-trigger; `converge` runs only at supervisor boot). Loud but not
recovered - the exact IX gap M5 closed on the *other* path. **Fix:** `reconcile` calls `respawn_retry`
(one line, same bound).

#### M4. [III] `services/net-stack/src/main.rs:667` - cached IP/gateway/DNS identity still never auto-reconciled (STILL DEFERRED)

Unchanged from Audit 1. The auto-configure gate is byte-for-byte `badge.is_none() && !have_mac && ...`;
once configured, `have_mac` stays true and the cached `our_ip`/`gw_mac`/`dns_server` are never
auto-reconciled against live DHCP/ARP truth. Trigger: configure on subnet A, link down, re-attach on
subnet B -> stale identity used verbatim until a manual `net renew`. Later work improved what `net`
*displays* (link-state clearing) but did not touch the reconcile gate. Remains deferred: the fix
(re-dance on a link-up *edge*, not only `!have_mac`) trades against the deliberate instant-replug design
and needs a real multi-subnet network to validate.

### LOW findings

- **U4. [VIII/truth]** `net_dns` `Aborted => Ok(())` (:4854) and `cmd_ping` q-abort-on-first-echo
  (`sent=0 -> Ok`, :4608/:4640): a **q-aborted probe reads as a passed probe**, so `online` + q during
  "resolving..." prints `dns ok` for a probe that never completed. v0.4.0's own "probes return Err on
  failure" rule wants `Aborted -> Err` (matches `cmd_wait`). *(Flagged independently by two auditors.)*
- **U5. [§26.6]** `parse_params` (:1841) silently drops arguments past `PARAM_MAX=9` from `$args`/`$argcount`.
  One loud line when tokens remain.
- **U6. [hygiene]** No compile-time guard that a baked script (`SELFCHECK_GS` 21 KB, `LIBRARY` entries)
  stays < 64 KiB; past 65535 the `u16` fn/summary offsets (`prescan_fns` :2950) wrap silently. `const _`
  assert per embedded script.
- **U7. [test]** `osdev/src/shell_test.rs:214` asserts a DNS fallback line (`"...: no answer"`) the shell
  no longer prints (split into "returned no A record" / "no reply from the DNS server" at `7197250`).
  Dead assertion - same class as the stale-version greps fixed on the library branch.
- **U8. [VIII/stale]** `cmd_observe_live`'s child-death break (:5921) checks `task_stat(slot).valid` but
  not the name (unlike `find_running_slot`); a painter fault + slot reuse in the poll window leaves the
  frame frozen. Not a wedge (q is child-independent). Add the `name_str()` check.
- **U9. [III]** `net-stack` `OUR_MAC` (:27) is a hardcoded constant, never reconciled with the NIC's
  real MAC (which nic-driver reports as truth in its `[3]` status). Two GodspeedOS boxes on one LAN
  mutually ARP-poison. Learn `our_mac` from the first `[3]` at `run_dance` start.
  **FIXED (2026-07-13).** `run_dance` now calls `learn_our_mac` FIRST (queries `[3]`, takes bytes 1..7);
  a zero/short reply (no NIC / driver not up) returns an unconfigured `NetState` and the
  auto-config-on-link path retries. The learned `[u8;6]` is added to `NetState.our_mac` and threaded as
  a parameter through every frame builder (`dhcp_discover` / `dns_resolve` / `udp_roundtrip` /
  `build_arp_reply` / `arp_resolve` / `ping`) and the main-loop mutable set - the same no-globals pattern
  as `our_ip` (Commandment VI). The `OUR_MAC` const is deleted; nothing hardcodes a MAC. **Why it was
  deferred and how the deferral was cleared:** the const only "worked" on the T630 because nic-driver
  runs the NIC promiscuous (a spoofed source was forgiven), and switching to the real MAC changes the
  DHCP lease identity - a live-network behavior change worth confirming on the actual network, not
  assuming. QEMU can't settle it (its e1000 MAC *is* `52:54:...`, so learned == old const, byte-identical
  - DHCP + ARP verified green, no regression, but no new information). The deferral was waiting on a
  real-hardware boot, which the T630 provided.
  **T630 sign-off (2026-07-13, `build/serial_output.log`):** nic-driver read the RTL8168's real MAC
  `7c:d3:0a:2b:b0:e3` (Realtek OUI, NOT the old 52:54 const); advertising it, net-stack completed the
  full dance on the live network - `DHCP - offered 192.168.4.98, gw 192.168.4.1`, `ARP - 192.168.4.1 is
  at 00:ab:48:da:1b:0d`, `ICMP - 192.168.4.1 echo reply (ping OK)`; `net nic-mac` reported the same real
  MAC; an interactive `ping` ran 14/16 replies @ ~707us avg. No panic. The router leased to the real MAC
  with no reservation/port-security snag, and ICMP completed where QEMU slirp never does - a *stronger*
  result than the promiscuous-forgiveness path it replaced. U9 closed.
- **U10. [inv12]** `net-stack` open-socket (op 2) grant-failure path (:694) replies nothing on a
  `derive_cap`/grant failure; the client eats its full deadline instead of a loud fast `[0]` (the
  slot-exhausted arm does reply). `try_send_by_handle(reply_cap, &[0])`.
- **U11. [VIII-edge]** `net-stack` `calibrate_tsc_hz` (:440) spins unbounded on the RTC second edge at
  boot before the serve loop - RTC timing is exempt-class, but unlike every other wait it has no bounded
  give-up. Bound by a yield count, return 0 loudly.
- **U12. [IX]** `net-stack` auto-configure fires only for ops 0/3 (`net`/`ping`); `net dns` (op 1) and
  `net arp` (op 6) issued first while unconfigured neither auto-configure nor benefit. Add ops 1/6.
- **U13. [III]** `scripts/contract_check.py:28` `CONTRACTED` is a hand-maintained list; a new service
  whose `.toml` disagrees with its kernel arm stays green because it is not in the list - defeating
  "drift is impossible" for exactly the case the tool exists for. Glob `services/*/contracts/*.toml`.
- **U14. [III/IV]** `examples/resource-server` + `examples/e1000` tomls/CLAUDE.md carry stale pre-T1
  doctrine ("RESOURCE_MINT/BAR is NOT a contract field") - false since T1 Phase B added both to the
  schema - and under-declare grants `service_hw` gives them (M6 class, example/test-build only). Declare
  or annotate kernel-only.
- **U15. [VII/IV, T1 residue]** Six privileged grants (SPAWN, CONSOLE_PUSH, INTROSPECT, SERVICE_CONTROL,
  REBOOT, ACQUIRE_ANY) are still keyed on literal service *name* in the spawn path, not a declaration -
  the T1 fix covered only `hw_device`/`resource_mint` + the ServiceConfig fields. No trigger today
  (holders ship no `.toml`, so the kernel is the single source and III holds), but if any gains a
  contract an M6-class understatement becomes possible. Promote to ServiceConfig bool fields.
- **L8 (carried).** SDK `recv()`/`console_read()` `loop {}` on error - a silent-hang shape; practically
  unreachable (own endpoint dies only with the task) so still LOW, but `fs`'s serve loop rides it.
  Log-once+park, or migrate servers to the loud `recv_result` twin.

### Verified present-and-correct (Audit 1 fixes + new code)

- **M1** (`drain_service`): PRESENT - `recv_abortable_deadline`, 512-bounded, loud Aborted/Timeout.
- **M2** (`fc_invoke`/`sock_invoke`): PRESENT - `recv_abortable_deadline`, reply cap reclaimed, late-reply
  drained. The remaining deadline-less shell waits ride `request_with_reply` -> the CALL syscall ->
  `ReplyDead`/`EndpointDead` on peer death (failure-observable, not the bare-recv wedge class).
- **M3** (net-stack interactive reacquire): PRESENT - `nic_req` (:84) reacquires-by-name on `SendFailed`
  and is the first request of *every* interactive path (link_is_up/ping/arp/dns).
- **M5** (supervisor steady-state retry): CLOSED on the death arms (`respawn_retry` wired everywhere) -
  the one residual is U3 (the reconcile backstop).
- **T1** (contract = source of truth): INTACT - `contract_check.py` passes live 6/6; the spawn path is
  field-driven from `service_hw(name)` (the old `name=="block-driver" && AHCI_FOUND` scatter is gone);
  no contract lies among the six contracted services. Residue = U13/U14/U15.
- **fs/block-driver VIII+III**: still CLEAN - `block_rpc` rides `ipc::call` (ReplyDead wake); mount
  bounded-then-degrade to FS_UNAVAIL; capacity replies length+sanity validated; tree is irreducible,
  `check()` rebuilds bitmap+count.
- **The v0.4.0 changes cleared:** `net dns`/`ping` Err is **blast-safe** (all 8 caller classes traced:
  interactive/pipe-producer/`$()`-capture/command-as-condition/assert/script/selfcheck/shell-test - the
  Err never corrupts a captured stream; `online` rides it correctly); the observe `ctx.sleep` change is
  **safe** (the kernel PIT-calibrates the quantum with sane bounds, so `handle_sleep` can only
  under-sleep, never stretch - the old T630 dead-q root cause is genuinely gone); the `find` size column
  is **bounds-safe** (guarded `i+nl+9 <= len`, dirs Empty, layout == ls); the library **depth-guard is
  airtight** (every `execute` call site + every bypass - `health | ...`, `... | health`, `$(health)`,
  `for line in (health)`, `defer/if/return health`, a `fn health` - enumerated and refused loudly);
  `hid::new_calibrated` div/overflow-safe; unsafe clean (`unsafe_check.py` 28 files, no additions).

### Clean per commandment (this pass)

| Service group | verdict |
|---|---|
| **shell interpreter/library** | depth-guard airtight; ciphers resolve-before-vars + retired-form loud error; wait/whatis bounded; M1/M2 present; stack-frame `#[inline(never)]` discipline complete (L7 fixed). Defects: U1/U2/U5/U6 |
| **shell pipes/net/observe** | net_dns/ping Err blast-safe; observe ctx.sleep safe; find size bounds-safe; pipe dispatch loud on type mismatch. Defects: U4/U7/U8 |
| **net-stack/nic-driver/supervisor** | M3 fixed, M5 closed (death arms); every recv is an own-endpoint server loop, every reply non-blocking; dns/ping Err always replies. Defects: U3/M4/U9-U12 |
| **library/contracts/fs/SDK** | T1 intact; fs/block-driver VIII+III clean; library scripts convention-compliant (argcount-first, help/version, raw-facts); unsafe clean. Defects: U13/U14/U15/L8 |

---

## Audit 3 - 2026-07-15 (full userspace sweep)

Method: 5 parallel auditors by coupling (block-driver+fs, nic-driver+net-stack, supervisor+logger,
xhci+ehci, shell+chaos+observe+probe+mem-pressure), each reading its crates in full and triaging against
VIII/IX/VII/III/IV/VI/X. Every prior Audit 1/2 fix re-verified **present in current source**.

**Result: 0 HIGH, 5 MED, 3 LOW (+1 carried). All prior fixes intact; no hang, corruption, shared-state,
or ambient-authority violation on any path.** The MEDs are resource-lifecycle gaps (a cap/slot/frame not
revoked or reclaimed on some teardown path) and one recovery-backstop gap - each concrete and
ordinary-use-reachable, none a wedge. The two USB MEDs live in the newer hub-walk code Audit 1 predated;
the root-port path stays clean.

| ID | Sev | Service | What | Status |
|----|-----|---------|------|--------|
| **F1** | MED | fs | `OP_RESET`/`OP_FLASH` (`fs/src/main.rs:724,751`) drop the mounted volume without revoking outstanding file caps (unlike delete/rename, which call `revoke_open_by_path`) - the kernel delegated-resource slot for each open file leaks until fs restarts; repeated reset/flash-with-files-open eventually fails `resource_mint` loudly. No corruption/escape. | open, staged - revoke-all `open_files` before `*vol = None` / re-`format`. |
| **N1** | MED | net-stack | Sockets are never closed/revoked (`net-stack/src/main.rs:730-753`; shell `sock` at `:5058,5120`) - the 8-slot table leaks one slot per `open` (no CLOSE op, contra `docs/networking.md` §6 "Close = revoke"); the 9th open fails and the error misattributes exhaustion to "no NIC" (same class as fixed L1). | open, staged - badge-driven CLOSE -> `resource_revoke` + slot clear; shell `cmd_sock` calls it before `remove_cap`; distinct "table full" message. |
| **S1** | MED | supervisor | Logger's fresh-boot spawn (`supervisor/src/main.rs:308-319`) uses plain `ctx.spawn`, so `name_map.get("logger")` is `None`; `reconcile`/`converge` then treat logger as "never spawned" and silently `continue`, disabling its dropped-notification recovery backstop (a `chaos max-carnage logger` that drops the death-notice leaves logger dead with no log - §26.4/Inv 12). Direct death-notify path unaffected. | open, staged - `ensure_mapped(&ctx, &mut name_map, "logger", 0xFFFF)` on fresh boot. |
| **XH-1** | MED | xhci | HC-wedge detection (`hc_wedged`/poison/`continue 'reenum`) was not ported to the hub/downstream command path (`address_downstream`, `configure_as_hub`); a downstream Address-Device wedge can re-trigger the all-core freeze the root-port fix prevents. Newer, not-yet-hardware-tested hub code. | open, staged - thread `hc_wedged` through the hub commands; check `configure_as_hub`'s return. |
| **XH-2** | MED | xhci | Hub-walk Address-Device failure (`:1171-1177`) leaves the enabled xHC slot never `disable_slot`'d (the root-port path does). Bounded (HCRST per pass) but wastes slots on a flaky hub port within a pass. | open, staged - `disable_slot` in the failure branch, guarded by the not-wedged check once XH-1 lands. |
| **XH-3/4/5** | LOW | xhci | (3) `spin()` discards success/failure, so reset logs assert unverified facts (ehci's `wait()` returns `bool` + WARNs); (4) poll drain never re-arms MSI-X (`irq_unmask`); (5) dead-BAR detection is diagnostic-only, falls through to enumerate. | staged (observability hygiene). |
| **M4** | MED | net-stack | (carried, deferred) `!have_mac` auto-config gate (`:706-713`) never re-reconciles a cached IP/gateway on a different-subnet re-DHCP. On record since Audit 1; needs a real multi-subnet network to validate. | deferred (unchanged). |

Re-verified intact: shell/chaos/observe/probe/mem-pressure CLEAN (zero bare `recv` on a dependency in
the 9,938-line shell; `reacquire_by_name` on every fs/net path; `SERVICE_CONTROL`-gated kill;
unsafe-free); block-driver+fs VIII-airtight + III-clean (`block_rpc` single funnel, bounded mount
retries); all prior M1/M2/M3/M5/M6/M8/U3/U9-U12/T1 fixes present. Zero `unsafe` in any service (§18.2);
no shared mutable statics anywhere in scope.

## Long-soak observation - 2026-07-16 (LS1, open + uncaptured)

Not a code-read finding - a **reproducible field observation** from a sustained hardware `chaos
max-carnage` soak on the T630. Recorded here (rather than lost as chat) because its recovery signature
puts it squarely in this audit's resource-lifecycle family (F1 / N1). It is **not yet root-caused**: the
failure moment has never been captured (every log so far ended on a healthy round).

| ID | Sev | Service (suspected) | What | Status |
|----|-----|---------------------|------|--------|
| **LS1** | MED | **block-driver + fs** (NOT the slot-leak / SEC-5 hypothesis) | After a sustained `chaos max-carnage` soak, `ls` (shell -> fs) returns "storage unavailable" and does **not** self-heal even after chaos stops; only **killing all services** (or `kill fs`) restores it. | **ROOT-CAUSED + FIXED @ `658df88`** (2026-07-16, T630 capture) - see resolution below. |

**The bracket (where it lives).** Two hardware datapoints on the same demarcated x86_64 image
(`feat/aarch64-prep` @ `4d48d92`, now merged to `main`):
- **~205,260 rounds -> healthy.** `ls` works; shell `selfcheck` reports **0 failed** (full fs / shell /
  records / pipes suite green). No persistent corruption at this point.
- **> ~300K rounds -> degraded** (seen **once**, the run immediately before the 205K stop): `ls` stuck
  until a full service reset.

So the degradation threshold is **above ~205K and at/below ~300K rounds** - a genuinely slow
accumulation, consistent with one resource leaked per some-thousands of restart/reacquire cycles.

**Why it is a live-service resource leak, not orphaned frames.** The recovery signature is decisive:
*killing all services* clears it. That reclaims resources **held by live services**, so the leaked
thing is a per-service resource freed on task death - a **cap-table slot** or a **delegated-resource
slot** (§7.10) - **not** the orphaned page-table frames of kernel-audit **T1** (those are already
detached from any task; killing services would not reclaim them). This is the same class as the staged
**F1** (fs delegated-slot leak on `OP_RESET`/`OP_FLASH`) and **N1** (net-stack socket-slot leak): a slot
leaked per teardown that accumulates under churn until `resource_mint` / a cap-table alloc fails and the
shell's fs path can no longer complete an `ls`. **Landing the staged F1/N1 revocation fixes is the first
thing to try** - LS1 may be a downstream symptom of the same missing-revoke discipline under sustained
load.

**Capture plan (do this the next time it reproduces, BEFORE `kill` recovers it).** The single most
valuable action is one introspection snapshot at the stuck `gsh>`:
1. At the stuck prompt, run `observe` (or the kernel-introspection command) and record **free frames**,
   **live endpoint count**, and **per-service cap-table / delegated-slot occupancy**. A slot count
   pinned at its ceiling for fs (or shell) names the culprit outright.
2. Then try **`kill fs` alone** (does `ls` return? -> localizes to fs) versus **`kill shell` alone**,
   instead of jumping straight to killing everything. Whichever single kill restores `ls` is the leaking
   service.
3. Grep the captured serial for the loud failure the leak should eventually raise (`resource_mint`
   failure, cap-table full) - if it is *absent* at the stuck moment, the exhaustion is silent and that
   is itself a §26.7 / Inv-12 gap to fix alongside the leak.

Until captured, LS1 stays here as an open long-soak finding: real, reproducible, bracketed, and pointed
at a suspect - but not yet pinned to a line.

### LS1 RESOLUTION - 2026-07-16 (root-caused on the T630, fixed @ `658df88`)

Captured live on the T630 (soak @ 57196 rounds) with `observe now` + targeted single-service kills. The
earlier slot-leak / **SEC-5** hypothesis above was **wrong** - fs never mounted, so no files were ever
opened. The real mechanism is two parts:

1. **block-driver (transient trigger).** After ~27,827 restart-storm respawns, one AHCI init read the
   port signature the instant `DET` went up, before the device's initial D2H FIS latched it ->
   `sig=0xffffffff` -> "no SATA disk" -> served I/O errors. Transient: a later fresh init (a manual
   `kill block-driver`) read `sig=0x00000101` and IDENTIFYd the Samsung SSD fine.
2. **fs (the "no self-heal" persistence - the real LS1).** fs mounted degraded once against that I/O
   error and **latched** "storage unavailable" forever - it never re-mounted, even after block-driver
   recovered the disk. `kill block-driver` alone did NOT fix `ls` (fs stayed latched); `kill fs` did
   (its fresh mount succeeded). That is exactly the "kill-all-services fixes it" signature.

**No memory leak** was involved (`observe` showed RAM 0% across 127k+ restarts each). **Fix (`658df88`):**
fs re-attempts the mount on a request while degraded (self-heal, no manual kill needed); block-driver
waits for `PxTFD.BSY/DRQ` to clear before reading `PxSIG` (robust detection). **SEC-5** (subtree revoke)
remains a real, separate fix - it is **not** the LS1 fix.

---

## Audit 4 - 2026-07-23 (feat/pi2-arm32: the ARM32 userspace we touched)

Scope: **only** the userspace changed for the arm32 port. The service crates (supervisor, logger, shell,
ping/pong, examples) are **arch-neutral and unchanged** on this branch - they cross-compile to armv7 and
run as-is, so their Commandment compliance is what Audits 1-3 already established. The arm32-specific
userspace surface is exactly **three files, 62 lines**: the SDK's ARM syscall ABI (`sdk/rust/src/
syscall.rs`), the SDK's ARM adversarial fault primitives (`sdk/rust/src/adversarial.rs`, the §18.1 audited
test module), and the user linker script (`services/user.ld`). Method: direct thorough read, cross-checking
the ABI against the kernel's `arm_svc_dispatch`, AAPCS, and the x86 `raw_syscall`; plus a runtime
Commandment pass on the arm service stack booted in QEMU `raspi2b`.

**Result: 1 finding (A-U1, MED-latent, FIXED). The syscall ABI is otherwise correct, the adversarial
primitives correctly express the ARM ring-3 faults and are test-only, the linker change is sound, and the
arm service runtime obeys the Commandments (loud graceful degradation, restart-on-fault).**

| ID | Sev | Cmd | What | Status |
|----|-----|-----|------|--------|
| **A-U1** | MED-latent | III / VIII | `sdk/rust/src/ipc.rs` `recv_timeout` - ARM's 32-bit ABI truncates each `raw_syscall` u64 arg to u32. Pointers/handles/lengths/slots genuinely fit, but `timeout_cycles` (generic-timer ticks) does not: at the Pi 2's ~62.5 MHz CNTFRQ, u32::MAX ticks is ~68 s, so a longer finite timeout truncated to a tiny value (premature wake) or - on a multiple of 2^32 - to **0**, which the kernel reads as **block-forever** = a bounded VIII deadline silently becoming an infinite hang. A silent truncation (III/§26.4) diverging arm from x86. Latent: needs a >68 s `recv_timeout`, which no current arm service issues; non-wedging in the common (premature) case, but the block-forever edge is a real VIII violation. | **FIXED** - `recv_timeout` saturates on ARM to `[1, u32::MAX]` (a genuine 0/block-forever stays 0), so a long finite request becomes the longest REPRESENTABLE timeout (~68 s), never tiny and never accidental-forever; x86 passes the full u64. `raw_syscall` comment corrected to name the one wider-than-u32 arg. |

**Verified sound (no violation):**
- **ARM syscall ABI (`raw_syscall`)** - register mapping (nr->r0, a0-a2->r1-r3) matches `arm_svc_dispatch(number, arg0, arg1, arg2)` and AAPCS; the i64 result is read from r0:r1 (low:high) with correct sign extension for negative error codes; the clobber list is right (inout r0-r3, lateout r12) and `options(nostack)` **omits** `nomem`, so the compiler treats the `svc` as a memory barrier - user buffers passed by pointer are not reordered/cached across the trap (matches x86). Same call surface as x86.
- **ARM adversarial primitives** (`fault_noncanonical_read` -> unmapped-high read `0xFFFF_FFF0`; `fault_divide_by_zero` -> `udf #0` undefined instruction) correctly express the arm-equivalent ring-3 CPU faults (ARM has no non-canonical VA form, and integer divide-by-zero does not trap by default), and each is the A14/C1 property the kernel audit confirmed: a USR-mode data-abort / undefined-instruction kills only the faulting task. Test-only (§18.1), `#[cfg(target_arch = "arm")]`, not called by any arm service (`probe` is x86-only).
- **`services/user.ld`** discards `.ARM.exidx`/`.extab`/`.attributes` (dead unwind tables under panic=abort) so every PT_LOAD stays 4 KiB-page-aligned for the loader - a sound build-correctness fix, no-op on x86.
- **Runtime Commandment pass (arm-supervisor in QEMU):** every hardware service absent on the Pi 2 (block-driver, fs, xhci, ehci, nic-driver, net-stack) fails its spawn **loudly** ("kernel will name-wire it" / "returned no endpoint cap") and the supervisor continues to a usable shell (§9.2/§11.3, loud not silent); `ls` on the fs-less shell returns `ls: storage unavailable` (loud degradation, not a hang); a PL0 shell fault (e.g. a debug-build pipe frame) kills only the shell and the supervisor **restarts** it (V/IX - identity over location, recovery). These are the existing arch-neutral service behaviours, confirmed intact on arm.

**Observation (not a finding):** debug-build shell pipe frames (~600 KiB, e.g. `status | count`) exceed the
256 KiB user stack and fault the shell; it recovers via supervisor restart, and the **release** build's
optimized frames fit and run pipes cleanly (`docs/arm32-status.md`). This is a build-environment / kernel
user-stack matter, not a service-code defect - recorded there, not counted here.

### Addendum to Audit 4 (2026-07-23): A-U2 - the `Call` syscall packing (found during SD bring-up)

Building the Pi 2 SD driver surfaced a second instance of the A-U1 class that Audit 4's ABI review did
not catch (it focused on `recv_timeout`). `sdk/rust/src/ipc.rs` `call` (syscall 41) packed **three**
16-bit cap slots into one u64 arg - `target` (0-15), `reply` (16-31), `recv` (**32-47**). On x86-64 the
arg is a full 64-bit register; on ARM the 32-bit ABI truncates each arg to u32, so **`recv_slot` (bits
32-47) was dropped to 0** and the Call routed to the wrong endpoint. `request_with_reply` therefore never
completed on ARM - `fs`'s block I/O to `block-driver` got no reply and `fs` degraded to
storage-unavailable. **FIXED** (`7786a39`): repacked into three 32-bit-safe args (`recv` rides the high
half of the length arg, which is `< 0xFFFF`); `handle_call` mirrors it. Transparent on x86 (same values),
correct on ARM. **Lesson (reinforces A-U1):** any syscall that packs a value above 32 bits into one arg
is broken on the 32-bit ABI; `arch/arm/CLAUDE.md` already warns of this, but the sweep must check *every*
multi-field-packed syscall arg, not just obviously-wide ones like a timeout.

## Audit 5 - 2026-07-23 (feat/pi2-arm32: the ARM USB-net backend + new shell hardware commands)

Scope: the userspace ADDED this session beyond Audit 4's three ABI files - the nic-driver ARM backend
`usb_net_main` (the `cfg(target_arch="arm")` frame-IPC <-> NetFrame* bridge) and the two new shell commands
`cmd_random` / `cmd_gpio`. Method: direct read cross-checked against the x86 e1000/RTL serve loops (same
contract), the SDK wrappers, the UNCHANGED net-stack client's reply parsers (to prove the ARM replies are
contract-compatible and cannot hang it), and the kernel privilege-grant path.

**Result: 0 HIGH, 1 MED-latent, 2 LOW - all FIXED.** The ARM backend's reply-cap discipline is airtight
(no F1/N1/SEC-5-class slot leak - the cap is taken once and `remove_cap`'d unconditionally after every
arm), every buffer is a fixed stack array, degradation is loud (serves empty replies, never hangs), and
every reply is length-guarded on the unchanged net-stack side. The MED is the U15 prediction realized:
nic-driver ships a contract, so a by-name privilege grant it omits is an M6-class understatement.

| ID | Sev | Cmd | What | Status |
|----|-----|-----|------|--------|
| **A5-U1** | MED-latent | IV / VII | The kernel grants nic-driver the **NET_DEVICE** cap BY NAME (`service_privileges`, arch-gated to ARM) - the USB-net frame bridge (syscalls 42-44). But nic-driver SHIPS a contract that declares only `hw_device="nic"` + `log_write`, and on the Pi 2 `hw_device="nic"` resolves to nothing (no PCIe NIC): the contract describes x86 authority the ARM instance doesn't use while omitting the ARM authority it does. A reviewer reading the .toml to answer "what can nic-driver reach on ARM?" gets the wrong answer (M6/M7). Runtime is still explicit-cap (no ambient authority at use), arch-gated, so latent. `contract_check.py` doesn't reconcile it (NET_DEVICE lives in the Privileges table). | **FIXED** (annotate, per the settled U15 doctrine): nic-driver.toml carries an explicit ARM note (real authority = NET_DEVICE + log_write; NET_DEVICE is a sanctioned kernel-only by-name grant, not a contract cap), and `service_privileges` documents NET_DEVICE/GPIO_DEVICE as the sanctioned by-name grants. A latent placement divergence surfaced with it (contract core 1 vs ARM kernel core 0, and `contract_check.py` couldn't parse the arch-conditional `preferred_core`) - the checker now takes the x86 `else` value, and both .tomls note the ARM core-0 override. |
| **A5-U2** | LOW | XXVI.6 / III | nic-driver `usb_net_main` op-9 batch drain called `net_frame_rx` (which DEQUEUES a frame) and THEN checked `opos + 2 + n > out.len()` - so a frame already pulled off the device but too big for `BATCH_MSG_MAX`(3072) was DROPPED (lost, not held), diverging from the x86 path which checks fit before advancing. Unreachable in practice (net-stack sends op 9 only in `ping`, where wire frames are tiny + retried), but a real lost RX frame. | **FIXED** - checks a max-size frame would fit (`opos + 2 + FRAME_MAX > out.len()`) BEFORE dequeuing; stops cleanly and lets net-stack re-poll, never dropping a consumed frame. |
| **A5-U3** | LOW | conventions | `cmd_random` did `arg.parse::<u32>().unwrap_or(1)` - a non-numeric count (`random abc`) silently became `1` rather than a loud rejection, inconsistent with its sibling `cmd_gpio` (which rejects a bad verb/pin loudly) and the loud-input-rejection convention. | **FIXED** - a bare `random` = 1, but a given non-numeric count prints `random: count must be a number 1..64` and returns. |

**Verified sound (no violation):** reply-cap discipline airtight (taken once, `remove_cap`'d
unconditionally after every arm - no path leaks a slot); no hang / no unbounded busy-wait (`rx_one` is a
fixed `RX_TRIES`=8 bounded best-effort, the batch loop breaks on the first empty poll, `recv()` is the
service's own-endpoint server recv, no dependency-wait); loud graceful degradation with no device (logs +
serves empty op-3 replies -> net-stack stays unconfigured, never hangs); fixed stack buffers (no heap);
the frame IPC contract is net-stack-compatible (every reply length-guarded on the unchanged client side -
op 3's 8-byte reply, op 4/9 length-prefixed, ops 5-8 ack - so a shorter-than-x86 ARM reply can't hang or
panic net-stack); `cmd_gpio` fully validated + loud (verb match with loud usage, pin bounded 0..53 with a
loud reject, loud "not available" on the non-ARM stub), GPIO_DEVICE cap-gated; `cmd_random` bounded
(`clamp(1,64)`) + loud on `hw_random()==None`.

---

## Audit 6 (2026-07-25, `feat/pi2-arm32` @ `74ee6ff`) - the USB block backend + durability work

**Scope:** the unaudited range `6929e28..HEAD` across `services/` and `sdk/`: `fs`'s flush barriers and
CRC reporting, the new `usbdisk` block backend and `OP_FLUSH` op, the SDHCI backend, the shell's
tokenizer/line changes, the supervisor's placement call, and the SDK's `resource_invoke` repack plus
`usb_disk_flush` wrapper.

**North star, restated:** a service may only reach what it was granted, must fail loudly, and must
never imply a guarantee it does not deliver. The last clause is where this audit landed.

**Verdict: 1 HIGH, 4 MED, 6 LOW, 1 INFO.** Mechanical checks all pass. The real defects cluster in two
places: **placement not surviving a restart** (the HIGH), and **comments promising §26.7 behaviour the
code did not implement** - three separate cases, one of which loses a committed transaction.

| ID | Sev | Class | What | Status |
|----|-----|-------|------|--------|
| **U6-1** | **HIGH** | (R) restart | `block-driver`'s ARM core-0 pin was applied at boot (`supervisor/main.rs` `ensure_mapped(..., 0)`) but NOT on restart: `respawn_managed` passes `0xFFFF` (no override), so the kernel fell back to `ServiceConfig.preferred_core: 1`. Every `msc_*` entry point refuses on `!on_core0()`, so a respawned block-driver replies `STATUS_ERR` to every block op forever, `fs` degrades to storage-unavailable, and its self-heal re-mount hits the same wall. Storage dead until reboot - block-driver no longer restartable on ARM (invariant 6), and `chaos max-carnage` kills it by design. | **FIXED** - placement moved into the kernel `ServiceConfig`, arch-conditional (`if cfg!(target_arch = "arm") { 0 } else { 1 }`), exactly as `nic-driver` already did; the supervisor's literal is gone. One source of truth, consulted by boot and restart alike. Verified on QEMU: spawns on core 0 with no override. Same finding as kernel Audit 7 K7-4, reached independently. |
| **U6-2** | MED | (C) §26.7 | **BARRIER 3 did not do what its own comment said.** The comment promised "the invalidation below is skipped" when the pre-invalidation flush fails; the early return was lost when the barrier moved to `durable_or_warn` (which returned `()`), so `block_write(journal_start, zeros)` ran unconditionally. On a drive that refuses SYNCHRONIZE CACHE the home writes are acknowledged but still in the volatile buffer, fs erases the commit record, power is cut - and the transaction is lost with no redo record. Precisely the corruption BARRIER 3 documents itself as preventing, performed by the barrier itself. | **FIXED** - `durable_or_warn` returns its verdict, BARRIER 3 genuinely returns without invalidating and logs that the journal was left intact; the two advisory call sites now say `let _ =` with a reason rather than discarding by omission. |
| **U6-3** | MED | (C) §26.7 | The AHCI `OP_FLUSH` comment was factually wrong about its own file. It claimed "SATA FLUSH CACHE (0xE7) is NOT implemented here" and that the gap "reports once at startup (`run`)" - but `write_block` and `write_zeros` have always issued `ATA_FLUSH_EXT` (0xEA) after every write, and `run` logs nothing of the kind. So the BEHAVIOUR was honest (that backend attests durability per-write, stronger than on demand) while the COMMENT was not, and the reply was an asserted `STATUS_OK` for a command never sent rather than an earned one. A maintainer trusting the comment would conclude x86 journal ordering is unenforced, which is false. | **FIXED** - `OP_FLUSH` now issues `ATA_FLUSH_EXT` and reports what the drive said; the comment states the truth. |
| **U6-4** | MED | (C) | `fs`'s `match p[0] & 0x7F` masks the force bit off EVERY opcode, not just the two that use it. `forced` is consulted only by `OP_FLASH` and `OP_RESET`; for the other ~23 arms a garbled op byte `0x9B` silently executes op `0x1B` instead of returning `FS_ERR`, and a client can set a destructive-override bit on an op that has no override with no error. | **RECORDED** - the fix is to reject `p[0] & 0x80 != 0` unless the masked op is `OP_FLASH`/`OP_RESET`. Not applied in this pass: it changes the fs wire contract and belongs with a shell-side change and a test, not bundled into an audit commit. |
| **U6-5** | MED | (C) Commandment III | `storage_unreadable` is overloaded to mean both "keep retrying the mount" and "the disk is present but unreadable". On `capacity == 0` (block-driver's authoritative "no disk") the new branch sets it purely to keep the re-mount loop armed, but that same flag selects the client-facing code, so a cardless Pi reports "storage unavailable ... data may be intact, do NOT flash" when there is no disk at all - blurring the distinction audit L2 created. | **RECORDED** - split `retry_mount` from `storage_unreadable`. Not a behavioural regression (the old bounded-attempt path ended in the same state), but the new code makes the conflation deliberate. |
| **U6-6** | LOW | (C) §26.7 | `format` warns log-only and one-shot when durability is unattested, while the operator at the prompt gets an unqualified "drives: formatted as GSFS - mounted, ready to use now". Success and failure travel on different channels, so on a console/log split the caveat never reaches the person who acted. Same shape for `durable_or_warn`: once-per-mount means an operator attaching later cannot discover that ordering is unenforced. | **RECORDED** - the caveat should ride back in the reply (a distinct byte, or a `drives` flag bit exposing "durability unattested"). |
| **U6-7** | LOW | (C) | `MAX_ARGS` was raised 4 -> 8, but tokens past the ceiling are still dropped SILENTLY - the exact bug being fixed (a dropped 5th word silently disarming `force`) recurs identically at 9 words. The same commit made `MAX_LINE` loud (BEL) but not this. Doc drift: a comment still reads "the shell's MAX_ARGS=4 tokenizer". | **RECORDED**. |
| **U6-8** | LOW | (C) §26.6 | The `MAX_LINE` doubling costs more stack than its comment claims ("about 4 KiB"): `History.lines` 2 -> 4 KiB permanent, `History::load` frame ~4.2 -> ~8.3 KiB, `save` 2 -> 4 KiB, i.e. ~+8 KiB against a 64 KiB user stack already known tight on the `pipe_run` path. Both merge frames are correctly `#[inline(never)]`. | **RECORDED** - correct the comments and re-measure headroom on the deep pipe path. |
| **U6-9** | LOW | (C) §26.7 | The `foreign_disk` guard is bypassed when block 0 cannot be READ: `if let Some(b0) = block_read(ctx, 0)` falls through to the format on `None`, so a transient read failure downgrades "could not verify" to "verified blank" - the one case the guard exists for. | **RECORDED** - refuse and say the disk could not be verified. |
| **U6-10** | LOW | (C) §26.7 | `sdhci`: `let _ = self.set_clock(self.divider_for(25_000_000), ctx);` discards a failed 25 MHz step-up and `init` still returns true, so the driver announces "SD card ready" for a card whose clock was never confirmed. `reset_cmd_dat` also breaks out of its bounded wait with no log. | **RECORDED**. |
| **U6-11** | LOW | (C) §14.3 | The block-driver backend choice is made once at startup and `fs` never re-validates it after a restart. If the stick is absent when block-driver respawns, it silently serves the SD card under the same name while `fs` keeps geometry derived from the previous instance - the hazard `foreign_disk` exists to prevent, arriving through a different door. | **RECORDED** - `fs` should re-validate the superblock or a capacity fingerprint after a block-driver reacquire, not only on `E_IO`. |
| **U6-12** | INFO | (B) | `OP_WRITE_ZEROS` iterates a client-supplied `u64` with no clamp in both `usbdisk` and `sdhci`. The USB one is bounded implicitly (the kernel's `lba >= MSC_SECTORS` check fails the write and breaks the loop); `sdhci::write_block` has no range check, so that loop is genuinely unbounded. | **RECORDED** - clamp `count` to `sectors - lba` in both. |

**Verified sound (no violation):** no global mutable state - `last_bad_dir_lba` and `flush_warned` are
`Cell` fields on `Fs`, re-initialised by `Fs::mount`, and `format` ends in `Fs::mount`, so a reformat
gets a fresh latch (the stale-after-reformat case was checked explicitly and does not occur); no service
gained `unsafe` (`unsafe_check.py`: 52 files, 830 lines, no unaccounted additions), and all new `unsafe`
is the SDK's §18.1-audited syscall wrappers, each SAFETY-commented. Bounded and heap-free throughout:
every new buffer is a fixed array, no `alloc`/`Box`/`Vec`, every new device loop carries an explicit
spin bound. Reply-cap discipline is exactly-once on every path of `usbdisk::serve`, `sdhci::serve` and
`ahci::serve` (empty payload, short payload, unknown op and `OP_FLUSH` all covered), and `fs::block_flush`
rides `block_rpc` -> `request_with_reply`, so a dead block-driver wakes it with `ReplyDead` rather than
hanging a commit. `contract_check.py` passes.

**Contract note (beyond what the checker can see):** `block-driver.toml` declares only `hw_device =
"ahci"` + `log_write`, while the ARM build grants it **`USB_DISK`** (whole-device read/write reach) by
kernel name-match. This is the same class as Audit 5's **A5-U1** (`NET_DEVICE`), which was closed there
by an ARM note in the contract; the new privilege repeats it unannotated and should get the same note.

---

## Audit 7 (2026-07-26, `feat/pi2-arm32` @ `f723e7a`) - the fs journal payload verification, USB block backend, chaos, and the SDK LBA path

Scope: `74ee6ff..HEAD` (34 commits, none previously audited) across `services/fs`, `services/block-driver`,
`services/chaos`, `sdk/rust/src/service_context.rs`, `scripts/selfcheck.gsh`. Two auditors. Every finding
traced before recording; anything without a concrete failure scenario was discarded.

**Clean across the board:** no `unsafe` in any service (§18.2), no heap / `alloc` / `Vec` / `Box`
(§26.6.1), no global mutable state (§3.9 - all new per-mount state is `Cell` on the owned `Fs`), no
hand-rolled number formatting, no em/en dashes (§21), no mutual blocking sends (§8.9).

**The journal commit-record layout was traced byte-for-byte on both the write and the recovery path and
they AGREE - no off-by-one.** `n <= TXN_CAP(56)` is enforced at stage time so the write path can never
exceed the recovery guard; a torn commit record is a torn sector and fails the header CRC; the payload
CRC is verified **before any home block is overwritten**. The design is right. The findings below are
about what happens after it says no.

### FIXED in this audit

| ID | Finding |
|----|---------|
| **U7-1 (HIGH)** | The four USB-disk SDK wrappers passed a **u64 LBA through a 32-bit syscall register**, silently truncating it. See kernel Audit 8 / A8-1 - fixed in the wrapper per hazard A-U1, with the false "the ONE exception" comment in `syscall.rs` corrected. |
| **U7-2 (HIGH)** | `block-driver` matched the literal `-2` for BUSY, which is also `CapNotHeld`. See A8-2 - now matches the named `USB_DISK_BUSY`. |

### OPEN - recorded, not yet fixed

**`fs`, after the journal says no** (the recovery-of-the-recovery gap):

- **U7-3 (HIGH).** A failed journal recovery is **silently erased by the next write**. `recover`'s return
  value is discarded, so on a payload mismatch `fs` mounts read/write, logs "will be re-verified on the
  next mount", and the first `commit_txn` overwrites the very record and staged blocks it retained. The
  promised re-verification never happens; the next mount looks clean over a half-applied tree. §26.7 /
  Commandment V - a failed recovery becoming a silent success one transaction later. Fix: return a status
  and mount **read-only** (the `read_only` field already exists and already gates every mutating op)
  until `drives check` clears it.
- **U7-4 (HIGH).** `recover` returns **silently** when it cannot read the commit record - asymmetric with
  the staged-block read failure added in the same commit, which does log. This is the likelier failure on
  the very device the work targets. `recover` is a static `fn` with no `&self`, so it also cannot set
  `io_error_seen`: the new re-mount machinery is blind to exactly this failure.
- **U7-5 (MED).** The io-error re-mount **abandons open file caps without revoking them**. The kernel-side
  delegated resources stay alive, so a client's still-valid file cap is answered `FS_NOTFOUND` - a lie
  ("your file is gone" vs "I dropped your handle") - and the 2048-entry band leaks across re-mounts until
  every `Open` fails. §14.3 applied to geometry but not to handles.
- **U7-6 (MED).** The re-mount degrade path conflates "unreadable device" with "no filesystem": the boot
  mount distinguishes `E_IO` from every other error, the re-mount arm sets `storage_unreadable` for all.
  A superblock CRC failure therefore reports "data may be intact; awaiting storage recovery" about a
  filesystem whose only remedy is a reformat - and the shell deliberately withholds the `drives flash`
  advice for that code.
- **U7-7 (MED).** `capacity` is the one cached fact the io-error re-mount does **not** refresh (the
  sibling recovery path does). Swap in a smaller stick during an outage and `drives flash` formats past
  the end of the device. Commandment III / §14.3 - the same justification the commit gives for dropping
  cached geometry, unapplied to the cached geometry living outside `Fs`.
- **U7-8 (LOW).** The `io_error_seen` funnel is incomplete: journal invalidation, `drives reset`, and
  `block_write_zeros` in format/check bypass `note_io_error`, so a device error there does not arm the
  re-mount built for it.

**`block-driver`:**

- **U7-9 (MED).** `BUSY_RETRIES` is bounded per block, but `OP_WRITE_ZEROS` multiplies it by a
  caller-supplied `count` bounded nowhere. `drives flash` zeroes ~16k bitmap blocks in **one** request; at
  30 s worst case per block a busy stick puts the serve loop inside a single request for hours, with `fs`
  blocked in `block_rpc` and the shell hung with no output and no abort. §26.6 - the bound must be on the
  operation, not just one block.
- **U7-10 (LOW).** The contract's `[placement] core = 1` is false on ARM (the kernel forces core 0, which
  the driver's own header requires). §13.2/§9.2 say a named core is enforced intent; document it in the
  existing ARM NOTE.

**`chaos` (the frozen-clock class - the range fixed its effect on the loops, not on everything else):**

- **U7-11 (MED).** With no RTC the PRNG seeds to a **constant**, so `chaos max-carnage random N` picks the
  identical victims in the identical order on every run, every boot - while the panel and docs claim a
  fresh random storm. A thousand runs explore one kill ordering. Fix: seed from `ctx.hw_random()` (already
  wired) or `epoch_secs_monotonic()`.
- **U7-12 (MED, PLAUSIBLE).** The handoff escape is bounded but its bound costs minutes (200k iterations x
  a 256-syscall scan), so in practice it is close to the wedge it replaced. One-word fix:
  `epoch_secs_monotonic()` instead of `datetime().epoch_secs()` - it **does** advance on ARM, which also
  fixes U7-13 and the `ARGWAIT` loop.
- **U7-13 (LOW).** Panel elapsed/ETA/"started" render as plausible numbers on a dead clock (`elapsed 0s`
  for a 40-minute run) rather than announcing themselves.
- **U7-14 (LOW).** The end-of-run `mem-pressure` reap is silent when it gives up, unlike the startup reap
  200 lines above which logs its count.

**`selfcheck.gsh`: re-runnability verified correct.** All script-created state is cleaned at both ends;
`write` truncates so leftovers cannot change behaviour; `Vars` is per-run so re-declaration cannot fail;
the `delete` sits inside the tallied block (loud) while the existence probe sits in the untallied
condition (correct). One residue: `cd` mutates the session CWD, so a run aborted mid-script leaves it
inside `/sc`, which the next run deletes. Inert today (every earlier statement uses absolute paths), one
relative path away from a phantom failure - consider `cd /` beside the cleanup.

- **U7-15 (LOW).** `if ls /sc` prints `ls: not a directory: /sc` on every clean run (conditions run with
  console output), so the transcript still carries two lines that read like failures - the tally is
  correct, the appearance is not.


## Audit 8 - the AArch64 branch at merge readiness (2026-08-12, `feat/pi4-aarch64` @ `94711fd2`)

Scope: ~9,300 changed userspace lines since Audit 7 - the `xhci` service (main + msc + topo, the USB
stack moved out of the kernel), the GENET backend (`nic-driver/src/genet.rs`, 1392 lines), and changes
to `shell`, `fs`, `net-stack`, `block-driver`, `supervisor`, `chaos`.

**Mechanical gate: PASS.** No `unsafe` in any service (§18.2).

### A8-1 (MED) - three unbounded message drains, in the file that already learned this lesson

`services/xhci/src/main.rs:597`, `:630`, `:3249` - all `while ctx.try_recv().is_some() {}`.

Nothing bounds these. A sender that enqueues as fast as the driver dequeues keeps the loop running,
and while it runs the USB poll loop is not polling: the keyboard stops. That failure mode is the one
this driver has been chased for repeatedly.

What makes it a finding rather than a theoretical: **the same file bounds its EVENT drain for exactly
this reason**, with the comment *"a device posting events as fast as we retire them never lets it run
dry, and then this loop never returns... 'it stops when the hardware stops' is not a bound, because the
hardware is the thing that might not stop."* The message drain is the same shape with a different
producer, left unbounded - and the comment above `:630` notes the path is *deliberately* exercised by
`chaos flood-storm xhci`, so the flood is not hypothetical.

Fix: the same treatment - a generous cap (the queue is 16 deep, so any bound well above it is only a
storm detector), and log once when it trips.

### A8-2 (MED) - a request with no reply cap is dropped silently by the ACTIVE Pi 4 backend

`services/nic-driver/src/genet.rs:1288` - `let Some(reply_cap) = ctx.take_pending_cap() else { continue };`

The sibling backend does this correctly: `services/nic-driver/src/main.rs:756` logs
*"frame request had no reply cap - dropping"* before continuing. GENET - the backend that actually runs
on the Pi 4 - drops it with no trace. A malformed or mis-sequenced request from net-stack therefore
leaves no evidence anywhere, on the exact protocol boundary whose correlation weakness is already
documented in `docs/net-tags-design.md`.

Two implementations of one rule, one of which forgot it (Commandment III). The log must be
rate-limited, as the existing "NO reply cap" logging elsewhere already is.

### A8-3 (LOW/MED) - every GENET reply discards its send result

`genet.rs:1334`, `:1338`, `:1364`, `:1371`, `:1388` - all `let _ = ctx.try_send_by_handle(reply_cap, ..)`.

A `try_send` that fails means the requester was never answered. It will time out and retry, so there
IS a recovery - but the driver believes it replied and says nothing, so the failure is only ever
visible from the far side as latency (§26.7). At minimum a rate-limited log; the outcome must not be
discarded.

**Not a cap leak:** `remove_cap(reply_cap)` at `:1390` runs on the loop's exit path, so the slot is
reclaimed whether or not the send succeeded. Checked because §8.5 makes this the usual companion
fault; it is genuinely absent here.

### A8-4 (LOW) - net-stack drops a capless request silently too

`services/net-stack/src/main.rs:985` - `None => continue,` with only an inline comment. Same rule as
A8-2, same fix, lower exposure (net-stack is not the hot path a driver is).

### The class this branch produced three of, worth grepping for

**State whose lifetime is shorter than the events it must remember.** All three were found on hardware
during this session's xhci work, each masking the next:

- `eaten` re-zeroed every pass, so a probe abandoned on one pass looked like a genuine failure on the
  next - and walked a wedge counter to its threshold;
- `PROBE_FAILS` reset per re-enumeration;
- `hub_tried` declared INSIDE the `'reenum` loop, so the re-enumeration it triggers wiped it. The
  guard erased its own memory with the very action it fired: 110 controller resets in one run.

A latch that resets when the thing it latches happens is not a latch. Worth a deliberate pass over
every `let mut` inside a restart/retry loop that is read as history.

### Verified sound

- **`genet.rs` `wait_mask`** is bounded BOTH ways - on the clock when the TSC is calibrated, and on an
  iteration count (`UNCALIBRATED_POLLS`) when it is not. That is the correct reading of "a count is not
  a duration": use the clock when you have one, and still refuse to spin forever when you do not.
- **`msc.rs:602` descriptor walk** is bounded and explicitly breaks on a zero-length descriptor, with
  the comment naming why. Device-supplied data driving a loop is the classic parser hang; this one was
  thought about.
- **Reply-cap discipline** is otherwise correct throughout the new code: one `take_pending_cap` per
  request, replies addressed by handle, no reuse across iterations.
