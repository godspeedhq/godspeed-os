# Milestone 13 — Adversarial / Red-Team Tests

**Status:** ✅ 10/10 implemented — all pass
**Spec ref:** §22 Adversarial / Red-Team Test
**Command:** `osdev test adv`

---

## Purpose

Adversarial tests verify capability isolation holds under direct attack.
The system claims a capability model with no ambient authority; these tests run services that try to break that claim.

Every attack must return a **defined error**. Any attack that succeeds is a security hole;
any attack that panics the kernel is a kernel bug. Both are mandatory fixes.

---

## Attacks

| ID  | Attack                                                                                | Expected outcome                             |
|-----|---------------------------------------------------------------------------------------|----------------------------------------------|
| A1  | Service crafts random u64 values and tries to use them as caps                        | `CapNotHeld` or other `Err` — never `Ok`     |
| A2  | Service brute-forces cap slot range 0..=127 and extreme values                        | Defined errors for every slot; no panic      |
| A3  | Service allocates beyond its 4 MiB contract limit via every path                      | `AllocDenied` when over limit; no panic      |
| A4  | Service uses its RECV-right cap handle as a SEND target                               | `CapInsufficientRights`; send rejected       |
| A5  | TOCTOU: kill victim, then send via now-stale cap                                      | `EndpointDead` — not `Ok`; no panic         |
| A6  | Service fills its own cap table via `acquire_send_cap` loop                           | `None` when table full; no panic            |
| A7  | Service measures IPC timing to detect partner identity via timing side-channel         | No panic; all sends return defined outcomes  |
| A8  | Service tight-loops without yielding on a core                                        | Preemption fires; witness service logs pass  |
| A9  | Service tries to spawn a non-existent service directly, bypassing supervisor           | `Err` (SpawnError::NotFound); no panic       |
| A10 | Service passes kernel-space addresses as `send`/`recv` buffer pointers                | `validate_user_slice` rejects; no panic      |

---

## Design

### A1 — Cap Unforgeability Under Adversarial Input

10,000 random u32 values used as cap slot indices. `adv-a1` holds no SEND caps (no
`send_peers`). Every `try_send_by_handle` must return `Err`. Any `Ok` proves a cap was
forged — a constitutional violation (§7.3, §3.1).

### A2 — Endpoint ID Brute Force

Iterate cap slots 0..=127 (covering the 64-slot cap table and well beyond), plus
`u32::MAX`. All must return defined errors. Cap slots 0 (log_write, WRITE right) and
1 (spawn, WRITE right) are not SEND caps → `CapInsufficientRights`. Out-of-range slots
return `CapNotHeld`. No panic on any value.

### A3 — Memory Limit Enforcement Under Adversarial Alloc

`adv-a3` has a 4 MiB `memory_limit`. The first 2 MiB allocation succeeds. The next
3 MiB allocation would push total to 5 MiB > limit → `AllocDenied`. Edge cases
(0, `usize::MAX`, 1 TiB) must not panic — `claim_alloc` must reject them cleanly.

### A4 — Insufficient Rights Enforcement

`adv-a4` has a recv endpoint. Its recv cap sits in slot 2 with `Rights::RECV`.
Using that slot handle as the endpoint argument to `try_send_by_handle` must return
`CapInsufficientRights` (§7.4). No SEND right = no send authority, regardless of which
slot is used.

### A5 — TOCTOU Race: Kill Then Send

`adv-a5` holds a SEND cap to `adv-a5-victim`. It kills the victim (bumping the endpoint
generation to `gen+1`), then immediately issues `try_send` via the now-stale cap. The
kernel's generation check (§8.7, §7.5) must catch this and return `EndpointDead`. The
cap's recorded generation `gen` ≠ routing-table's current generation `gen+1` → reject.

### A6 — Cap Table Exhaustion

`adv-a6` calls `acquire_send_cap("adv-a6")` in a loop, inserting SEND caps to itself.
Starting from 3 pre-filled slots (log_write=0, spawn=1, recv=2), 61 dynamic caps can
be inserted before the 64-slot cap table is full. The 62nd `acquire_send_cap` must
return `None` — the kernel returns an error from the cap-insert path, which the SDK
translates to `None`. No panic on cap table exhaustion. The count of slots filled is
logged.

### A7 — Timing Side-Channel Probe

100 `try_send` calls to `adv-a7-recv` (passive, never draining). Queue fills after
16 sends; subsequent calls return `QueueFull`. TSC brackets the entire loop; mean
cycles/try_send is logged. No panic; all return values are defined. Verifies the kernel
does not expose undocumented behavior under sustained timing analysis.

### A8 — Preemption of Monopolizing Service

`adv-a8` runs a tight `loop { core::hint::spin_loop(); }` without yielding.
`adv-a8-witness` is round-robin placed (potentially on the same core as `adv-a8`) and
yields 1,000 times then logs `adv: A8 pass`. Timer-driven preemption (§9.1, §3.6)
must give the witness enough CPU quanta to complete all yields and log. If preemption
fails, the witness never logs and the test times out.

### A9 — Direct Spawn Bypassing Supervisor

`adv-a9` calls `ctx.spawn("nonexistent-does-not-exist")`. The kernel's `service_config`
lookup returns `None` → `SpawnError::NotFound` → `Err`. No panic. Note: all v1 services
hold a `spawn` cap (SPAWN_RESOURCE), so the attempt is authorised; the rejected name is
the defined-error path. The kernel enforces capability rights but not service-name policy
— that is a v2 concern.

### A10 — Kernel Address Rejection

Raw syscalls for Send (nr=2) and Recv (nr=3) with `a1` (the buffer pointer argument)
set to kernel-space addresses (`0xffff_8000_0000_0000`, `0xffff_ffff_ffff_fff0`) and
null. `validate_user_slice` in the kernel rejects any pointer ≥ the kernel base
(≥ `0xffff_800000000000`) or null, returning an error code. No page fault, no kernel
panic.

---

## Probe services

| Service         | Core    | Mode | Notes                                          |
|-----------------|---------|------|------------------------------------------------|
| adv-a1          | r-robin | 80   | No caps; random slot → always `Err`            |
| adv-a2          | r-robin | 81   | Brute-force slots 0..=127 + `u32::MAX`         |
| adv-a3          | r-robin | 82   | 4 MiB limit; alloc edge cases                  |
| adv-a4          | r-robin | 83   | Has recv endpoint; uses it as send target       |
| adv-a5-victim   | r-robin | 0    | Passive; killed by adv-a5                      |
| adv-a5          | r-robin | 84   | SEND cap to victim; kill then send              |
| adv-a6          | r-robin | 85   | Has recv endpoint; fills own cap table          |
| adv-a7-recv     | r-robin | 0    | Passive; absorbs timing probe messages          |
| adv-a7          | r-robin | 86   | SEND cap to adv-a7-recv; 100 timing sends       |
| adv-a8          | r-robin | 87   | Tight loop; preemption target                  |
| adv-a8-witness  | r-robin | 88   | 1,000 yields then log pass                     |
| adv-a9          | r-robin | 89   | Spawn non-existent service → `Err`             |
| adv-a10         | r-robin | 90   | Raw syscalls with kernel-space buffer addresses |

---

## Pass criteria

Each attack logs `adv: AN pass` on success. The harness passes if all 10 "pass" lines
appear within their timeout without a `KERNEL PANIC`.

**No attack must succeed where the spec says it fails.**
**No attack must panic the kernel.**

---

## Implementation checklist

- ✅ `services/probe/src/main.rs` — modes 80–90 (11 modes across 10 attacks)
- ✅ `kernel/src/task/mod.rs` — 13 adversarial service configs; `TASK_KSTACK_MAX` raised to 100
- ✅ `services/supervisor/src/main.rs` — adversarial probe spawns
- ✅ `osdev/src/validator.rs` — `ADV_TESTS`, `run_adv_tests()`, `run_adv_one()`, `adv_serial_path()`
- ✅ `osdev/src/main.rs` — `"adv"` branch in `cmd_test`
- ✅ `build/tests/6_ADVERSARIAL/.gitkeep`
