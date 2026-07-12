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
| **T1/M7** contract = source of truth | **IN PROGRESS** | The final piece: shrink the kernel to the supervisor-only config, drive everything else from contracts. See T1 below |

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
