# services/logger/

Structured log sink (§11.4). **Restartable.** Not a TCB member.

## Startup sequence

1. Call `ctx.drain_kernel_ring_buffer()` — read all bytes from the 16 KiB kernel ring buffer accumulated before logger started (§11.4).
2. Register with registry.
3. Print `"logger: ready"`.
4. Enter recv loop.

## Input format

Each log message is an IPC message from a service holding `log_write`. The payload encodes: service name (tag), log level, text.

## Output

v1: serial console only. Future: append to a log file via `fs`.

## Restartability

Logger is stateless. On restart it drains the ring buffer again (which may have buffered messages from the outage window) and resumes. Log history before the restart window is lost; the ring buffer preserves the most recent 16 KiB.

When logger is down, services that call `ctx.log()` block on a full queue or get `EndpointDead`. They must handle this gracefully — logging failure should never kill a service.

## Supervisor retry (§11.3)

If logger fails to spawn at boot, init logs to the kernel ring buffer and retries. Logger is not TCB so its spawn failure does not cause a kernel panic.
