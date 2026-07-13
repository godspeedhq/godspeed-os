# Userspace Commandment Audit

> **Living document.** Records every audit of the userspace services (everything above the kernel)
> against the Ten Commandments (`COMMANDMENTS.md`) and the constitution (`CLAUDE.md`). Re-run and
> append with each audit. The kernel has its own living record in `docs/kernel-audit.md`; this file
> is its userspace counterpart. First audit: 2026-07-12.

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
| U9 OUR_MAC hardcoded, not reconciled | DEFERRED | III - learn from nic-driver `[3]` status; needs T630 HW re-validation (the hardcoded MAC is part of the HW-proven DHCP path) - defer with the same discipline as M4 |
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
> `docs/kernel-audit.md` "Hardware sign-off". The shell/net-stack userspace fixes (U4-U14) were verified
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
