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

## Phase 3 — ELF Loader + Kernel Spawn API

- [ ] `kernel/src/loader.rs` — parse service ELF PT_LOAD segments from embedded bytes;
  map each into a fresh page table with correct per-section flags.
- [ ] Kernel stack allocator — 64 KiB per ring-3 task from the frame allocator;
  `TASK_KERNEL_STACK_TOP[slot]` set at spawn time.
- [ ] `task::spawn_service(name, elf_bytes, core_id) -> usize` — allocates kernel stack,
  builds user page table, calls `TaskContext::new_user`, calls `enqueue`.
- [ ] Syscall 7 (`Spawn`) + Syscall 8 (`Kill`) in `syscall/dispatch.rs`.
- [ ] Death-notification endpoint infrastructure so supervisor learns when a service dies.
- [ ] `kernel/src/main.rs` — remove demo ring-0 ping/pong; spawn only `init`.

---

## Phase 4 — SDK and Service Implementations

### SDK (`sdk/rust/src/`)

- [ ] `lib.rs` / `service_context.rs` — `ServiceContext` wrapping a pointer to
  `ServiceContextData` at a fixed page (0x3ff000) written by the kernel pre-launch.
- [ ] `capability.rs` — typed `CapHandle` wrapper; `send`, `recv`, `try_send` issue
  real `syscall` instructions.
- [ ] `ipc.rs` — `send`, `recv`, `try_send`, `yield_current` using the SYSCALL ABI.

### init (`services/init/`)

- [ ] Spawns supervisor, registry, logger (via Spawn syscall, in order).
- [ ] Panics kernel if any TCB spawn fails (§6.2).
- [ ] Logs `"init: ready"` on serial.
- [ ] Loops forever; never exits.

### supervisor (`services/supervisor/`)

- [ ] Reads boot manifest; spawns services per placement policy (§9.2).
- [ ] `kill(service_name)` — kills the named service.
- [ ] `restart(service_name, placement_override?)` — kill + respawn, placement
  re-evaluated from scratch.
- [ ] Logs `PlacementInvalid` and skips if contracted core unavailable (§9.2).
- [ ] Logs `"supervisor: ready"`.

### registry (`services/registry/`)

- [ ] `register(name, endpoint_cap)` — service registers endpoint on startup.
- [ ] `lookup(name) -> endpoint_cap` — client gets a fresh cap by name.
- [ ] Generation in returned cap matches current resource generation.
- [ ] Logs `"registry: ready"`.

### logger (`services/logger/`)

- [ ] Drains kernel ring buffer on startup (§11.4).
- [ ] Receives log messages from services holding `log_write`; writes to serial.
- [ ] Logs `"logger: ready"`.

### ping / pong (`examples/`)

- [ ] ping placed on core 0; pong placed on core 1 (via contract).
- [ ] ping sends a message to pong every second.
- [ ] pong receives and logs each message.
- [ ] ping handles `EndpointDead` by re-looking up pong via registry.

---

## Phase 5 — Restart Flow

- [ ] `osdev restart pong --core 2` kills pong on core 1, respawns on core 2.
- [ ] ping observes `EndpointDead`, reacquires via registry, continues sending.
- [ ] New cap routes to core 2 correctly.

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
