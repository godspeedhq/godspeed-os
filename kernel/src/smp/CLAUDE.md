# kernel/src/smp/

Multi-core coordination (§9, §11). The IPI protocol lives in `ipi.rs`; the interrupt-controller register writes behind it live in `arch::imp` (`send_ipi_to_lapic`), so `ipi.rs` holds the protocol, not the MMIO.

## Files

| File            | Responsibility |
|-----------------|---------------|
| `mod.rs`        | `init(boot_info)`: init per-core state, start APs; `percpu_init(boot_info)`: size the per-core arenas to N |
| `core.rs`       | `CoreState` per core; `mark_ready(core_id)`, `ready_count()`, `is_ready(core_id)` |
| `ipi.rs`        | `send_ipi(core_id, vector)`, `ipi_handler(vector)` |
| `percpu.rs`     | `PerCore<T>` - boot-allocated per-core arenas (§26.6.1), sized to N cores at boot from the frame allocator (not `[_; MAX_CORES]`); all `unsafe` isolated here behind safe `get()` |
| `names.rs`      | `NameTable` / `AtomicNameSet` - fixed-size task-name and peer-name storage, so `task/` needs no `unsafe` for them |
| `spinlock.rs`   | `SpinLock<T>` / `SpinLockGuard<T>` - RAII spinlock for safe mutable statics throughout the kernel |

## SpinLock usage

`SpinLock<T>` is the standard way to protect mutable kernel globals outside the permitted unsafe layers. All unsafe code lives in `spinlock.rs` itself (5 lines: `unsafe impl Send`, `unsafe impl Sync`, the `ZEROED` constant's `mem::zeroed()`, two `UnsafeCell::get()` deref calls in `Deref`/`DerefMut`). Call sites are entirely unsafe-free:

```rust
static FOO: SpinLock<[T; N]> = SpinLock::new([...]);
let mut guard = FOO.lock();   // RAII; releases on drop
guard[i] = value;
```

`try_lock()` is available for non-blocking acquisition (returns `Option<SpinLockGuard>`).

## Core lifecycle (§9.5)

Cores discovered at boot are fixed for the system lifetime. No hotplug. The core count is `arch::imp::ap_count() + 1` (the +1 is the BSP), set once in `percpu_init`. Any core that fails to call `mark_ready` within the timeout is logged as a warning and excluded from placement (§11.3).

## IPI vectors

Two vectors, defined in `ipi::vectors`:

| Vector              | Value | Purpose |
|---------------------|-------|---------|
| `WAKE_RECEIVER`     | 0xF0  | Wake a task blocked on `recv` (cross-core send) |
| `SCHEDULER_TICK`    | 0xF2  | Force a scheduling point (timer overflow broadcast) |

0xF1 was `TLB_SHOOTDOWN`. It is unused now, and is not renumbered.

## TLB coherence (§10.5) - no shootdown, and why

There is no cross-core TLB shootdown in this kernel. A software broadcast (a per-initiator request
slot and ack bitmask, serviced from the IPI handler and from the waiting core's own spin so that two
initiators could not deadlock) lived in `ipi.rs` with no caller, and was deleted on 2026-10-10
(`backlog/fixed/80` K20) after a per-port proof that translation coherence holds without it:

- **Nothing unmaps a live user mapping.** Memory is reclaimed only at task death; the only other
  unmaps are boot-time (the kernel-stack guards, before any other core starts) and selftests.
- **Reclaim waits for quiescence.** `kill_task_by_slot` frees nothing until no core is running the
  task or still leaving it (`core_still_using`: `CORE_CURRENT` and `CORE_LEAVING`).
- **Every switch invalidates on its own core.** x86 writes CR3 (no PCID), ARMv7 issues `TLBIALL`
  when TTBR0 changes, RISC-V issues `sfence.vma` after writing `satp`, and AArch64 flushes on every
  switch. No port uses ASIDs or PCIDs, and no user mapping is global.
- **AArch64's idle core keeps the dead root installed**, and the Cortex-A72 may walk it
  speculatively, so its `free_page_table_root` invalidates inner-shareable (`tlbi vmalle1is`)
  before the root is freed. The other ports' idle cores run on the kernel root.

A future feature that unmaps or downgrades a mapping a LIVE task can reach is a new requirement and
brings its own coherence mechanism with it. Property test P7 checks the property itself: a fresh
task given the same addresses as a killed one must read its own zeroed pages, never the dead
task's.

## Placement (§9.2)

Placement is not in this module. The live rule is `task::resolve_spawn_core`: a strict override,
else the spawn request's preferred core with a loud round-robin fallback, else round-robin. On restart
it is asked again from scratch, so the previous core is not remembered (§9.2 "on restart" clause).
(An `smp::placement::resolve` described here had no callers and was deleted 2026-10-10, `backlog/fixed/80`
K20.)
