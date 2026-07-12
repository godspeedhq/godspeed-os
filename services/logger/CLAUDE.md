# services/logger/

Structured log sink (§11.4). **Restartable.** Not a TCB member.

## Current behaviour (what the code actually does)

The logger is a **minimal placeholder** today (`src/main.rs`): it prints `"logger: ready"` and then
**drains its recv endpoint, dropping each message**. It does that draining deliberately - a registered
service whose endpoint just parked (never `recv`s) would let a flood or a stray send fill its 16-deep
queue and sit at 16/16 forever (the flood-endpoint disease). `recv` parks the task between messages, so
the core still idles.

**How services actually log today:** `ctx.log()` is a **syscall** that writes the kernel's 16 KiB ring
buffer **and** the serial console **directly** - it does NOT send IPC to this service. So logging does
not depend on the logger being up: when the logger is dead, `ctx.log()` still works (it never blocks on
the logger and never returns `EndpointDead` from it), and a chaos storm that kills the logger loses no
log output. The logger service exists to own its name + endpoint and to be a restartable home for the
richer sink described next.

## Future work (not yet implemented)

The `src/main.rs` header lists what a real sink would add - none of it is wired yet:
1. `ctx.drain_kernel_ring_buffer()` on startup - read bytes accumulated before the logger started.
2. A recv loop that DECODES an IPC log protocol from services holding `log_write` (payload = service
   tag + level + text) instead of dropping.
3. Formatted output to serial, and later appended to a log file via `fs`.

## Restartability

Logger is stateless, so it restarts trivially: the supervisor respawns it on death (§6.2) and it prints
`"logger: ready"` again. There is no state to reconstruct. (Once the drain-and-decode sink above lands,
a restart would re-drain the ring buffer for the outage window; today there is nothing to recover.)

## Supervisor retry (§11.3)

The **supervisor** spawns the logger (init was removed, Phase 5) and retries once on failure; its output
falls back to the kernel ring buffer meanwhile. Logger is not TCB, so its spawn failure does not cause a
kernel panic.

## TODO: per-core kernel log buffers (post-v1 / post-BP2)

Current `kprintln!` uses a kernel-side `SpinLock` + synchronous UART write. Under heavy diagnostic load multiple cores contend for the lock and busy-wait on the UART FIFO.

Proposed architecture:
- **Kernel side**: static per-core ring buffers (e.g. 4 KiB each) in BSS. `kprintln!` writes to the calling core's own buffer (SPSC - no lock needed). If the buffer is full, increment a per-core `dropped_log_count` and discard.
- **Drain**: new `ReadKernelLog(core_id)` syscall returns buffered bytes for that core. Logger polls all cores via this syscall and writes to UART.
- **Panic path**: keep a `panic_serial_direct()` that bypasses the buffer and writes raw to COM1 (the halting core retakes UART ownership).
- **Ordering**: logs from different cores may appear out of order. Add a TSC timestamp per entry if post-hoc ordering is needed.

Benefits: eliminates cross-core SpinLock contention on the kernel log path; UART is owned by a single writer; dropped-log counter makes buffer pressure visible.

Work estimate: ~200-300 lines across `kernel/src/log.rs`, `kernel/src/syscall/dispatch.rs`, and `services/logger/src/main.rs`.
