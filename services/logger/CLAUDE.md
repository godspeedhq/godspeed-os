# services/logger/

Structured log sink (§11.4). **Restartable.** Not a TCB member.

## Startup sequence

1. Call `ctx.drain_kernel_ring_buffer()` - read all bytes from the 16 KiB kernel ring buffer accumulated before logger started (§11.4).
2. Register its name in the kernel directory.
3. Print `"logger: ready"`.
4. Enter recv loop.

## Input format

Each log message is an IPC message from a service holding `log_write`. The payload encodes: service name (tag), log level, text.

## Output

v1: serial console only. Future: append to a log file via `fs`.

## Restartability

Logger is stateless. On restart it drains the ring buffer again (which may have buffered messages from the outage window) and resumes. Log history before the restart window is lost; the ring buffer preserves the most recent 16 KiB.

When logger is down, services that call `ctx.log()` block on a full queue or get `EndpointDead`. They must handle this gracefully - logging failure should never kill a service.

## Supervisor retry (§11.3)

If logger fails to spawn at boot, init logs to the kernel ring buffer and retries. Logger is not TCB so its spawn failure does not cause a kernel panic.

## TODO: per-core kernel log buffers (post-v1 / post-BP2)

Current `kprintln!` uses a kernel-side `SpinLock` + synchronous UART write. Under heavy diagnostic load multiple cores contend for the lock and busy-wait on the UART FIFO.

Proposed architecture:
- **Kernel side**: static per-core ring buffers (e.g. 4 KiB each) in BSS. `kprintln!` writes to the calling core's own buffer (SPSC - no lock needed). If the buffer is full, increment a per-core `dropped_log_count` and discard.
- **Drain**: new `ReadKernelLog(core_id)` syscall returns buffered bytes for that core. Logger polls all cores via this syscall and writes to UART.
- **Panic path**: keep a `panic_serial_direct()` that bypasses the buffer and writes raw to COM1 (the halting core retakes UART ownership).
- **Ordering**: logs from different cores may appear out of order. Add a TSC timestamp per entry if post-hoc ordering is needed.

Benefits: eliminates cross-core SpinLock contention on the kernel log path; UART is owned by a single writer; dropped-log counter makes buffer pressure visible.

Work estimate: ~200-300 lines across `kernel/src/log.rs`, `kernel/src/syscall/dispatch.rs`, and `services/logger/src/main.rs`.
