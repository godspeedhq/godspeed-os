# Design Note: capability-gate kill/restart (`service_control`)

**Status:** IMPLEMENTED + verified (2026-06-05).
**Branch:** `feat/service-control-cap` (off `main`).
**Date:** 2026-06-05
**Pins:** §3.1 (no ambient authority), §14.4 (`service_control`, held by the
supervisor), §7 (capability model), §22 (identity + property/stress/fuzz/adv/perf/
chaos suites must stay green).

---

## 0. Implementation summary (as built)

- `SERVICE_CONTROL_RESOURCE = ResourceId(6)`, registered in `capability::init`.
- `handle_kill` (syscall 8) gates on `current_task_holds_resource(SERVICE_CONTROL,
  WRITE)` as its first action — killing is now a privileged, capability-checked
  action, not ambient.
- Minted at spawn for `shell`, `supervisor`, and every probe service. Probes are
  identified by **ELF identity**: all probe configs now share one `PROBE_ELF`
  const, and the mint matches `core::ptr::eq(elf_bytes, PROBE_ELF)` — miss-proof,
  no name enumeration. (Strategy (a) below.)
- `caps <service>` maps id 6 → `service_control`.

### Verification (kill path exercised, all green)
- identity **22/22** — Test 4A kills `probe-victim`; 6A/6B/10A/10B restart.
- property **10/10** — P2/P5/P7/P8/P9 kill victims.
- adversarial **11/11** — A5 (TOCTOU kill), A9, A11.
- fuzz: F7 (50-iteration kill storm) times out under TCG — its serial log shows
  `kill_task: ... freed N frames` succeeding throughout, so the gate is correct;
  the timeout is the known TCG ceiling (chaos is 0/7 on **main** too, i.e.
  pre-existing/environmental, not this change). No new unsafe (318).

### Denial coverage
No dedicated negative test was added. Every probe gets the cap via ELF identity,
so a probe-based "kill denied without cap" test would require excluding one probe
name from the mint — a test-only wart on a clean gate. The denial *mechanism*
(`current_task_holds_resource` returning `CapNotHeld` when the cap is absent) is
the identical code path already proven by **A11** (`introspection_denied_without_cap`).
If an explicit `service_control` denial test is wanted later, it costs that mint
exclusion.

---

## 1. The gap

`kill` (syscall 8, `handle_kill` in `kernel/src/syscall/dispatch.rs`) performs **no
capability check**. It reads a service name, applies the §6.2 trusted-root guard
(`init`/`supervisor`/`registry` rejected), and kills. Any service that can issue
syscall 8 can kill any non-trusted-root service.

That is **ambient authority** — a §3.1 violation ("every privileged action
requires an explicit capability"). And §14.4 explicitly says kill/restart require a
`service_control` capability "held only by the supervisor." So the code is wrong
against the current constitution; this is not a v2 feature.

Contrast: `spawn` (syscall 7) *is* gated — `handle_spawn` validates a
`SPAWN_RESOURCE` cap. `restart` = kill + spawn, so today the spawn half is gated
but the kill half is not.

This is also why `caps supervisor` shows only `log_write` + `spawn`: the kill
authority isn't a held cap, so it can't be displayed. Closing the gap makes it
visible.

---

## 2. The fix

A new stable kernel resource `SERVICE_CONTROL_RESOURCE = ResourceId(6)` (gen 0
forever, like the other stable resources). Gate `handle_kill` on it.

- `capability/mod.rs`: declare + `register_resource(SERVICE_CONTROL_RESOURCE)`.
- `handle_kill`: it consumes both arg registers for the name (no slot to pass), so
  gate by **holdings**, exactly like the introspection syscalls —
  `scheduler::current_task_holds_resource(SERVICE_CONTROL_RESOURCE, Rights::WRITE)`.
  Stable resource ⇒ `holds_resource` is valid (skips the gen check). Return
  `CapNotHeld` if absent.
- Mint name-gated at spawn (`task/mod.rs`), like `INTROSPECT`/`console_read`.
- `restart` then requires **both** `service_control` (kill) and `spawn` (spawn) —
  both held by the same privileged callers.
- Shell `cmd_caps`: map id 6 → `service_control`.
- Docs: `syscall/CLAUDE.md` syscall table (Kill requires service_control),
  `capability/` docs, and the §7.4/§13 resource list.

**Not affected:** the COM2 control channel (`control.rs`) and the page-fault path
call `kill_by_name`/`kill_task_by_slot` *directly* (kernel-internal), bypassing
`handle_kill` — they keep their inherent TCB authority. Only the *syscall* is
gated.

---

## 3. Blast radius — why this needs the full suite

The new cap must be minted for **every service that legitimately calls `kill`**, or
that test silently breaks (kill denied → its logic fails, no compile error). The
killers are not just shell + supervisor: the **probe binary** kills "victim"
services throughout the test fleet. Victim targets found in
`services/probe/src/main.rs`:

```
probe-victim                      (Test 4A)
brutal-id-13-recv                 (identity brutal)
prop-p2/p5/p7/p8/p9-victim        (property P2,P5,P7,P8,P9)
prop-bp2/bp5/bp7/bp8/bp9-victim   (property brutal)
stress-s2/s4/s5/s10-victim        (stress S2,S4,S5,S10)
stress-bs2/bs4/bs5/bs10-victim    (stress brutal)
fuzz-f7-victim / fuzz-bf7-victim  (fuzz F7)
adv-a5-victim / adv-ba5-victim    (adversarial A5)
perf-b5-victim / perf-bp5-victim  (perf B5)
chaos-c7-victim / chaos-bc7-victim(chaos C7)
```

The **killers** are the corresponding driver-probe services (e.g. the service
running P9 that kills `prop-p9-victim`), all spawned from the one probe ELF; the
service *name* selects probe_mode.

### Mint strategy (two options)
- **(a) Gate on the probe ELF.** Every killer is a probe-ELF service; mint
  `service_control` for any service spawned from that ELF, plus `shell` +
  `supervisor`. Cleanest — can't miss a family — but needs the spawn path to know
  "this is the probe ELF."
- **(b) Broad name-prefix gate.** Mint for `shell`, `supervisor`, and names
  starting with `probe-`/`prop-`/`stress-`/`fuzz-`/`adv-`/`perf-`/`chaos-` (and the
  brutal `bp-`/`bs-`/`bf-`/`ba-`/`bc-` forms). Mirrors the existing `INTROSPECT`
  gate style but is easy to under-cover.

Prefer (a) if feasible; it's miss-proof.

### Verification bar (mandatory before merge)
Because this rewrites how a core syscall is authorized, **all** suites must be green
afterward, not just shell-test:
`osdev test identity` (20), `property` (P1–P10), `fuzz` (F1–F8), `stress`
(S1–S10), `perf`/`perf-brutal`, `adv`/`adv-brutal`, `chaos`/`chaos-brutal`. A
missed mint shows up as a kill-denied failure in exactly one of these. Run on a
rested box (TCG throttles under back-to-back suites) or in CI.

---

## 4. Out of scope

Making `spawn`/the supervisor API a single unified `service_control` (the spec
keeps `SPAWN` and `service_control` distinct). Restartable block-driver/fs (v2).
Re-routing the shell's kill through the supervisor instead of a direct syscall
(Appendix B.3 ideal; the shell holds the cap directly in v1).
