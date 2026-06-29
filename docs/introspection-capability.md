# Design Note: Gating Introspection Behind a Capability (§3.1)

**Status:** DESIGN - approved, not yet implemented.
**Branch:** `feat/introspect-cap` (off `feat/observe`).
**Date:** 2026-06-03
**Pins:** §3.1 (no ambient authority), §3.3 (authority is explicit), invariant 1, §26.9 (authority stays visible and scoped).

---

## 1. Problem

The two kernel introspection syscalls are **ambient** - any task can call them
holding no capability:

- `InspectKernel` (syscall 13) - alloc bytes, live-endpoint count, frame counts,
  per-core ticks, core count, endpoint generation by name, TSC.
- `TaskStat` (syscall 16) - full per-task snapshot for *any* scheduler slot: name,
  core, state, memory used/limit, queue depth, restart generation.

`handle_task_stat` says so explicitly in source: *"No capability required -
read-only kernel state."* This was a deliberate convenience for the §22
property/perf harness, which calls these from many `probe` instances.

But it is a standing exception to the constitution. Invariant 1 (§3.1) is "no
ambient authority - every privileged action requires an explicit capability," and
`kernel/src/syscall/CLAUDE.md` states the rule with "there are no exceptions."
Enumerating *every* task's name, memory, and restart count, or probing another
named service's lifecycle generation, is information-disclosure authority - a
confidentiality boundary the capability model is supposed to mediate. Today it is
not mediated. This note closes that gap.

It surfaced while deciding the architecture of the `observe` utility
(`utilities/1_observe.md` §7): `observe` is a least-authority service, but "least
authority" is hollow if the authority it needs (read system metrics) is something
*every* service already has for free.

---

## 2. Decision summary

| # | Decision | Choice |
|---|----------|--------|
| 1 | What to gate | Disclosure of **another task's or system-wide** state requires the cap; reading **only your own** state or a **hardware clock** stays ambient. |
| 2 | Mechanism | A **holds-resource check** - the kernel verifies the calling task holds `INTROSPECT_RESOURCE` (READ) - not a passed cap-slot. |
| 3 | Denial | Return a **distinct denied code** (`CapNotHeld`); SDK wrappers are unchanged (legitimate holders never see it). |

---

## 3. Scope - what is gated, what stays ambient

**Rule:** *disclosing another task's or system-wide state requires `INTROSPECT`;
reading only your own state, or a hardware clock, does not.*

### Gated (require `INTROSPECT_RESOURCE` with `Rights::READ`)

| Syscall / query | Discloses |
|---|---|
| `TaskStat` (16) | any task's full snapshot |
| `InspectKernel` 1 | live endpoint count (system) |
| `InspectKernel` 2 | another named endpoint's generation |
| `InspectKernel` 4 | free physical frames (system) |
| `InspectKernel` 5 | total physical frames (system) |
| `InspectKernel` 6 | a core's active ticks (system) |
| `InspectKernel` 7 | a core's total ticks (system) |
| `InspectKernel` 8 | ready core count (system) |

### Ambient (no capability)

| Syscall / query | Why it stays open |
|---|---|
| `InspectKernel` 0 | the caller's **own** allocated bytes - its own state |
| `InspectKernel` 3 | `read_tsc` - a hardware clock, not anyone's state |

This line is chosen so the migration cost is identical to a narrower line (the
same three services need the cap regardless), making the complete version free:
gate the whole cross-task/system surface, keep self-state and the clock open.

---

## 4. Mechanism - holds-resource gate, not a cap-slot

### Why not the existing slot pattern

Every gated syscall today (Log, Spawn, ConsoleRead, ConsolePush) passes a
`cap_slot` argument and the handler does
`current_task_lookup_cap(slot, right)` + `resource_id` check. That does not fit
here: `TaskStat` (slot / buf_ptr / buf_len) and `InspectKernel` query 2
(query_id / name_ptr / name_len) already consume all three ABI argument
registers. There is no spare slot to pass a cap-slot without adding a fourth ABI
argument (which would touch the asm syscall entry).

### The gate

Add a stable kernel resource:

```rust
// kernel/src/capability/mod.rs
/// Introspection authority - read another task's or system-wide kernel state
/// via InspectKernel (13, system queries) and TaskStat (16). Self-state queries
/// (own alloc bytes, TSC) remain ungated. Gating prevents an arbitrary service
/// from enumerating every task's name/memory/restart count (§3.1).
pub const INTROSPECT_RESOURCE: ResourceId = ResourceId(5);
// ... register_resource(INTROSPECT_RESOURCE) in init()
```

Each gated handler, before doing anything, checks that the **calling task holds**
`INTROSPECT_RESOURCE` with `READ`:

```rust
if !scheduler::current_task_holds_resource(INTROSPECT_RESOURCE, Rights::READ) {
    return cap_err_to_i64(CapError::CapNotHeld);
}
```

This needs one new scheduler/`CapTable` helper -
`current_task_holds_resource(rid, right) -> bool` - that scans the calling task's
cap table for a live cap on `rid` carrying `right`. (The cap table already
supports per-slot iteration via `for_each_active_cap`.)

### Deviation, documented

This is a **holds-resource** check rather than the documented
`CapTable::get(slot, right)` slot check. The intent of the §3.1 rule - *validate
a capability before performing the privileged action* - is fully satisfied; only
the calling convention differs, because read syscalls that consume all argument
registers have no slot to pass. `kernel/src/syscall/CLAUDE.md` will be updated to
record this as the sanctioned form for argument-saturated read syscalls.

### SDK is unchanged

Because the gate reads the task's holdings rather than a passed slot, the SDK
introspection wrappers (`inspect_*`, `task_stat`) keep their exact signatures and
bodies. No caller code changes except declaring the capability (§6).

---

## 5. Denial semantics

A denied call returns `cap_err_to_i64(CapError::CapNotHeld)` - a distinct negative,
separable from the existing `-1` "not found / invalid args" the inspect handlers
already use.

The three legitimate callers (§6) all hold the cap, so they never observe denial;
denial only bites an undeclared or adversarial service. The current SDK wrappers
coerce a negative return to a default (`0`, or `valid: false` for `task_stat`),
which means denial is, today, *quiet*. We accept that for now because no
legitimate caller hits it.

> **Follow-up (logged, out of scope here):** "loud over silent" (§26.7) argues for
> making the introspection wrappers fallible (`Result`/`Option`) so a denied call
> is surfaced rather than coerced to a default. Deferred - it is SDK API churn
> with no current consumer, and would be pulled into existence by the first service
> that must distinguish "denied" from "zero."

---

## 6. Migration

Add an `introspect` grant to `ServiceConfig` (a `has_introspect: bool` flag, minted
at spawn exactly like `has_console_read`), and set it for the services that
legitimately read cross-task/system state:

| Service | Needs the cap because it calls | Notes |
|---|---|---|
| `shell` | `task_stat` (status), `inspect_core_count` (cores) | privileged broker already |
| `observe` | `task_stat` + aggregates | the metrics utility itself |
| `probe` | `inspect_endpoint_generation` (query 2) | test harness; query 0 + TSC stay ambient so most probe paths are unaffected |

The kernel mints `INTROSPECT_RESOURCE` (READ) into each declaring task's cap table
at spawn (§14.1). No contract-file change is required if grants are driven by the
in-kernel `ServiceConfig` table, consistent with how `has_console_read` /
`console_push` (name-gated `xhci`) work today.

> **To confirm at implementation:** how the `probe` configs are enumerated (one
> `ServiceConfig` reused across probe_modes vs. several) so the grant lands on
> every probe instance that needs query 2.

---

## 7. Constitutional & doc impact

- **Not an amendment - an alignment.** This *removes* a standing exception to
  §3.1; it strengthens conformance rather than changing an invariant, so no
  CLAUDE.md invariant change is needed. (A one-line note may be added to the §7
  capability section listing `INTROSPECT_RESOURCE` among the stable resources.)
- **`kernel/src/syscall/CLAUDE.md`:** add `INTROSPECT_RESOURCE` to the syscall
  table and document the holds-resource form for argument-saturated read syscalls.
- **`docs/unsafe-audit.md`:** unaffected - no new `unsafe` (the gate is safe Rust).
- **`utilities/1_observe.md` §7/§8:** update from "introspection is currently
  ambient (verify)" to "introspection is gated by `INTROSPECT_RESOURCE`; `observe`'s
  contract declares it," restoring the accuracy of observe's least-authority story.

---

## 8. Testing

- **Regression (must stay green):** the full §22 suite - identity, property
  (P2/P4/P5/P8 use the gated/ambient queries), perf (TSC stays ambient), stress -
  with the three services now holding the cap. A pass proves the gate is
  transparent to legitimate holders.
- **New adversarial test (the property this unlocks):** a service that does **not**
  declare `introspect` calls `task_stat` / a gated `InspectKernel` query and is
  denied (`CapNotHeld`), while a self query (own alloc bytes) and TSC still
  succeed. This is the first test that can assert introspection is *not* ambient -
  it was impossible to write before. Fits the §22 Adversarial (A-series) bar.

---

## 9. Implementation checklist

1. `capability/mod.rs`: add `INTROSPECT_RESOURCE = ResourceId(5)` + register it.
2. `task/scheduler.rs` (+ `capability/table.rs` as needed):
   `current_task_holds_resource(rid, right) -> bool`.
3. `syscall/dispatch.rs`: gate `handle_task_stat` and the system-state arms of
   `handle_inspect_kernel` (1,2,4,5,6,7,8); leave 0 and 3 ambient.
4. `task/mod.rs`: `ServiceConfig.has_introspect`; mint the cap at spawn; set it for
   `shell`, `observe`, `probe`.
5. Docs: update `syscall/CLAUDE.md`, the `§7` capability list, and
   `utilities/1_observe.md` §7/§8.
6. Build (`osdev image`), run the identity/property/perf/adversarial suites; add the
   new "introspection denied without cap" adversarial test.
7. Hardware sanity on the T630 if the suite passes in QEMU.

When this lands, merge `feat/introspect-cap` → `feat/observe` and resume the
`observe now` build on solid, least-authority ground.
