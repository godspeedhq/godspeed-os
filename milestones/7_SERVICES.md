# Milestone 7 — Services and Supervisor Restart

> init, supervisor, registry, logger, ping, and pong reach steady state.
> Supervisor can kill and restart a service; clients reacquire via registry.

---

## Phase 1 — Build Infrastructure ✅

Commit `c2cc77c`.

- ✅ `services/user.ld` — linker script placing all service ELFs at 0x400000,
  page-aligned sections (text rx / rodata r / data+bss rw), discards unwind tables.
- ✅ `services/*/build.rs`, `examples/*/build.rs` — emit `-T user.ld` linker arg
  for all service crates (init, supervisor, registry, logger, block-driver, fs,
  ping, pong).
- ✅ `kernel/build.rs` — emits `SVC_*_ELF` env vars pointing at compiled service
  ELF paths so `include_bytes!(env!("SVC_INIT_ELF"))` works at kernel compile time.
- ✅ `osdev/src/main.rs` `cmd_build` — builds 6 service crates before the kernel
  (kernel/build.rs records their paths; services must exist before kernel compiles).

---

## Phase 2 — Ring-3 Arch Foundation ✅

Commit `c2cc77c`.

- ✅ **Per-core GDT (8 entries)** — null / kernel code (0x08) / kernel data (0x10) /
  SYSRETQ placeholder (0x18) / user data (0x20) / user code (0x28) / TSS low+high
  (0x30). GDT_PER_CORE + TSS_PER_CORE statics in .data (CPU writes Accessed/busy bits).
- ✅ **`init_gdt(core_id)`** — fills TSS descriptor at slots 6/7 from `TSS_PER_CORE[cid]`
  address; calls `ltr 0x30`; reloads CS/SS/DS/ES/FS/GS. Called on BSP and each AP.
- ✅ **`init_syscall(core_id)`** — writes EFER.SCE, STAR (kernel CS=0x08, SYSRETQ
  base=0x18 → user CS=0x28 / SS=0x20), LSTAR → `syscall_entry`, SFMASK=0x200 (clears
  IF); writes `IA32_KERNEL_GS_BASE` via `init_per_core_syscall`.
- ✅ **`syscall_entry.rs`** — `PerCoreSyscallData {user_rsp @ offset 0, kernel_rsp @ offset 8}`;
  naked SYSCALL stub: `swapgs` → save user RSP → load kernel RSP → push r11/rcx →
  shuffle regs to SysV ABI → `call syscall_handler` → `cli` → pop → restore user RSP
  → `swapgs` → `sysretq`.
- ✅ **`set_tss_rsp0(core_id, rsp)`** — updates `TSS_PER_CORE[cid].rsp0` via
  `write_unaligned` so the CPU uses the correct per-task kernel stack on ring-3 interrupts.
- ✅ **`ring3_entry_trampoline`** — naked function: `pop rcx` (user_rip) → `pop rsp`
  (user_rsp) → `mov r11, 0x202` → `sysretq`. First-entry path for ring-3 tasks.
- ✅ **`TaskContext::new_user(kernel_stack_top, user_entry, user_stack_top, cr3)`** —
  builds initial kernel stack layout `[trampoline, user_rip, user_rsp, pad]`.
- ✅ **Scheduler ring-3 support** — `TASK_IS_USER` / `TASK_KERNEL_STACK_TOP` statics;
  `enqueue` gains `is_user` + `kernel_stack_top` params; `prepare_ring3_switch`
  updates TSS.rsp0 and PER_CORE_SYSCALL.kernel_rsp; all four context-switch sites
  call it when the incoming task is ring-3.

---

## Phase 3 — ELF Loader + Kernel Spawn API ✅

Commit `3e53a1c`.

- ✅ `kernel/src/loader.rs` — ELF64 PT_LOAD parser; allocates frames per segment,
  copies file bytes, zero-fills BSS, maps into fresh `PageTable` with PF_X/PF_W/PF_R flags.
- ✅ Kernel stack pool — static `[KernelStack; 32]` of 64 KiB each in `task/mod.rs`;
  `TASK_KERNEL_STACK_TOP[slot]` set via `scheduler::enqueue(..., kstack_top)`.
- ✅ `task::spawn_service(name, elf_bytes, core_id)` — loads ELF, maps user stack
  (4 pages at 0x7FFF_C000), writes `ServiceContextData` page at 0x3FF000,
  allocates kernel stack, calls `TaskContext::new_user`, calls `scheduler::enqueue`.
- ✅ `task::spawn_init()` — embeds init ELF via `include_bytes!(env!("SVC_INIT_ELF"))`,
  calls `spawn_service("init", ..., 0)`.
- ✅ Syscall 7 (`Spawn`) + Syscall 8 (`Kill`) stub entries in `syscall/dispatch.rs`.
- ✅ User-pointer validation (`validate_user_slice`) in `handle_log` and `build_message`.
- ✅ `services/.cargo/config.toml` + `examples/.cargo/config.toml` — override workspace
  rustflags so service crates do NOT inherit `-Tkernel/kernel.ld`; kernel linker script
  moved into `kernel/build.rs`.
- ✅ `PageFlags` derives `Clone, Copy` — fixes move-in-loop error.
- ✅ `kernel/src/main.rs` — removed demo ring-0 ping/pong; calls `task::spawn_init()`.
- [ ] Death-notification endpoint infrastructure — deferred to Phase 4.

---

## Phase 4 — SDK and Service Implementations ✅

Commit `d41b418`.

### SDK (`sdk/rust/src/`)

- ✅ `syscall.rs` (new) — `pub(crate) raw_syscall(nr, a0, a1, a2)` shared by all syscall
  wrappers; eliminates the circular-import problem.
- ✅ `service_context.rs` — `ServiceContextData` gains `spawn_slot: u32` (was `_pad`);
  `spawn()` issues real Spawn syscall (7); `recv()` calls `ipc::recv` with `recv_slot`;
  all Phase 5 stubs annotated.
- ✅ `ipc.rs` — `recv`, `send`, `try_send` SYSCALL wrappers implemented; `recv` passes
  a stack-allocated buffer to the kernel and returns `Message::from_bytes(payload)`.
- (capability.rs send/recv/try_send wrappers deferred — CapHandle-level IPC is Phase 5)

### Kernel changes

- ✅ `capability/mod.rs` — `SPAWN_RESOURCE` (ResourceId 2) registered as a stable resource.
- ✅ `task/mod.rs` — `ServiceContextData.spawn_slot` populated (slot 1 = spawn); all
  services receive a spawn cap; `SpawnError::NotFound` variant; `spawn_service_by_name`
  and `service_elf_table` (embeds supervisor/registry/logger/ping/pong ELFs).
- ✅ `syscall/dispatch.rs` — `handle_spawn` validates SPAWN_RESOURCE cap, reads name from
  user space, calls `spawn_service_by_name`; `handle_recv` now accepts an output buffer
  pointer and copies message payload to user space (was no-op).

### init (`services/init/`) ✅

- ✅ Logs `"init: ready"`.
- ✅ Spawns supervisor, registry, logger via Spawn syscall in order.
- ✅ Loops forever on TCB spawn failure (§6.2 "loud failure" semantics).
- ✅ Retries logger once (logger is not TCB — §11.3).

### supervisor (`services/supervisor/`) — Phase 4 minimal ✅

- ✅ Logs `"supervisor: ready"` and yields in a loop.
- [ ] Boot manifest reading; service spawn per placement policy — Phase 5.
- [ ] kill/restart API — Phase 5.

### registry (`services/registry/`) — Phase 4 minimal ✅

- ✅ Logs `"registry: ready"` and yields in a loop.
- [ ] register/lookup IPC operations — Phase 5.

### logger (`services/logger/`) — Phase 4 minimal ✅

- ✅ Logs `"logger: ready"` and yields in a loop.
- [ ] Kernel ring buffer drain; log message recv loop — Phase 5.

### ping / pong (`examples/`)

Cross-core IPC plumbing is confirmed working (serial.log from `build/serial.log`
shows the Milestone 6 ring-0 demo running indefinitely on 4 cores with no panics,
proving the IPC fast path and SMP scheduler are sound). The Phase 4 service versions
(below) need wiring once supervisor spawns them in Phase 5.

- [ ] Supervisor spawns ping on core 0; pong on core 1 (round-robin or contract).
- [ ] ping sends a message to pong every second (`sleep_one_second` + `try_send` cap lookup).
- [ ] pong receives and logs each message (`recv` with endpoint cap minted at spawn).
- [ ] ping handles `EndpointDead` by re-looking up pong via registry.

---

## Phase 5 — Supervisor + ping/pong + Restart Flow

### Kernel

- [ ] `handle_kill` (syscall 8) — validates `service_control` cap, marks task Dead,
  bumps endpoint generation, reclaims memory (TLB shootdown), notifies supervisor.
- [ ] `task::kill_current` — page-fault path; same sequence as above.
- [ ] Per-service IPC endpoint creation at spawn — populate `recv_slot` in
  `ServiceContextData`; register endpoint in `ipc::routing` table.

### SDK

- [ ] `ServiceContext::send` / `try_send` — look up peer cap by name from a
  name→slot table embedded in `ServiceContextData` (or via a dedicated cap slot per
  declared `ipc_send` peer).
- [ ] `drain_kernel_ring_buffer` — syscall to drain the 16 KiB ring buffer.
- [ ] `restart(name, core_override)` — Kill + Spawn syscalls with placement §9.2.

### Supervisor (Phase 5)

- [ ] Reads embedded boot manifest; spawns ping and pong per placement policy (§9.2).
- [ ] Receives death notifications; restarts dead non-TCB services.
- [ ] Exposes kill/restart IPC API (§14.4).

### Registry (Phase 5)

- [ ] `register(name, endpoint_cap)` IPC operation — stores name→cap slot entry.
- [ ] `lookup(name)` IPC operation — returns fresh cap or `NotFound`.
- [ ] Endpoint cap minted at spawn; clients discover registry via a well-known cap slot.

### Logger (Phase 5)

- [ ] `drain_kernel_ring_buffer()` on startup (§11.4).
- [ ] Receive loop: `ipc::recv` on `log_write` endpoint; write formatted lines to serial.

### Restart flow acceptance (`osdev restart pong --core 2`)

- [ ] ping sends messages to pong continuously.
- [ ] `osdev restart pong --core 2` — supervisor kills pong on core 1, respawns on core 2.
- [ ] ping observes `EndpointDead`, calls `registry.lookup("pong")`, gets fresh cap.
- [ ] ping resumes sending; messages route to core 2 transparently.
- [ ] No kernel panic on any core.

---

## Acceptance

All six serial lines appear within 5 s of boot:
```
init: ready
supervisor: ready
registry: ready
logger: ready
smp: 4 cores ready
kernel: all cores ready
```
After `osdev restart pong --core 2`, ping resumes without kernel panic.
All ten §22 identity tests pass.
