# Milestone 13 — Adversarial / Red-Team Tests

**Status:** [x] 10/10 implemented — all pass
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

---

## Brutal Adversarial Phase (adv-brutal)

**Status:** [x] 10/10 implemented — all pass
**Command:** `osdev test adv-brutal`

The brutal phase repeats each adversarial attack at 5–50× intensity, combined with the
full brutal stress suite (BS1–BS8) running concurrently in the same QEMU session.
BA1–BA10 correspond to A1–A10 but with higher iteration counts and longer running times.

### Attacks

| ID   | Attack                                                                                 | Intensity vs A-series              |
|------|----------------------------------------------------------------------------------------|------------------------------------|
| BA1  | 50,000 cap forgery attempts (random u64 slot values)                                   | 5× A1 (10 k → 50 k)               |
| BA2  | Extended slot sweep 0..=511 + extreme values                                           | Wider range than A2 (0..=127)      |
| BA3  | 5× alloc-beyond-limit attack cycles (request, over-limit, free, repeat)                | 5× A3                              |
| BA4  | RECV cap used as SEND target × 5 cap types (RECV, WRITE, GRANT, zero, random)         | 5× A4, multi-rights variants       |
| BA5  | TOCTOU kill+send race × 5 cycles                                                       | 5× A5                              |
| BA6  | Fill + drain cap table × 5 cycles                                                      | 5× A6                              |
| BA7  | 500 timing samples (5× A7 count)                                                       | 5× A7 (100 → 500)                  |
| BA8  | Tight-loop hog + witness runs 200 yields                                               | Spawned early (before stress); 200 yields proves preemption fires |
| BA9  | 5× direct spawn bypass attempts (non-existent service name)                            | 5× A9                              |
| BA10 | 20 kernel-address patterns × raw send/recv syscalls                                    | 5× A10 (4 → 20)                    |

### Probe services

| Service          | Mode | Notes                                                      |
|------------------|------|------------------------------------------------------------|
| adv-ba1          | 144  | No caps; 50 k random slot → always `Err`                   |
| adv-ba2          | 145  | Brute-force slots 0..=511 + `u32::MAX`                     |
| adv-ba3          | 146  | 4 MiB limit; 5× alloc edge cycles                          |
| adv-ba4          | 147  | Has recv endpoint; uses RECV cap as SEND × 5 variants      |
| adv-ba5-victim   | 0    | Passive; killed × 5 by adv-ba5                             |
| adv-ba5          | 148  | SEND cap to victim; 5× kill-then-send TOCTOU               |
| adv-ba6          | 149  | Has recv endpoint; fills own cap table × 5 drain cycles    |
| adv-ba7-recv     | 0    | Passive; absorbs 500 timing probe messages                 |
| adv-ba7          | 150  | SEND cap to adv-ba7-recv; 500 timing sends                 |
| adv-ba8          | 151  | Tight loop; preemption target                              |
| adv-ba8-witness  | 152  | 200 yields then log pass                                   |
| adv-ba9          | 153  | Spawn non-existent service × 5 → `Err`                     |
| adv-ba10         | 154  | 20 kernel addr patterns via raw syscalls                   |

### Implementation checklist

- ✅ `services/probe/src/main.rs` — modes 144–154 (11 modes across 10 attacks)
- ✅ `kernel/src/task/mod.rs` — 14 brutal adversarial service configs
- ✅ `services/supervisor/src/main.rs` — brutal adversarial probe spawns
- ✅ `osdev/src/validator.rs` — `ADV_BRUTAL_TESTS`, timeouts 900 s for BA4/BA5/BA8/BA9
- ✅ `build/tests/13_ADVERSARIAL_BRUTAL/.gitkeep`
- ✅ `kernel/src/memory/allocator.rs` — `KERNEL_PT_PROTECTED` bitmap prevents kernel PT
  frame theft under sustained stress; `protect_kernel_page_table_frames()` called at boot
