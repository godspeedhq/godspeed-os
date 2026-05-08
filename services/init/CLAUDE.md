# services/init/

PID-1 equivalent. TCB member (§6.1). **Non-restartable.**

## Sole responsibilities

1. Spawn `supervisor` on Core 0.
2. Spawn `registry` on Core 0.
3. Spawn `logger` on Core 0 (retry on failure — logger is non-TCB).
4. Print `"init: ready"` to the kernel ring buffer.
5. Loop forever; never exit.

## What init does NOT do

- Restart services. That is `supervisor`'s job.
- Spawn application services (ping, pong, etc.). That is `supervisor`'s job.
- Hold the `service_control` capability. That belongs only to `supervisor`.

## Failure semantics (§6.2)

If init dies, the kernel panics and the system reboots. There is no recovery. The panic reason is written to serial and the crash page.

## Contract (`contracts/init.toml`)

init declares only `spawn = [supervisor, registry, logger]` and `log_write`. It does not declare IPC send/receive endpoints because it never participates in normal IPC traffic after startup.
