# services/init/

PID-1 equivalent. TCB member (§6.1). **Non-restartable.**

## Sole responsibilities

1. Spawn `supervisor` on Core 0.
2. Spawn `logger` on Core 0 (retry on failure — logger is non-TCB).
3. Print `"init: ready"` to the kernel ring buffer.
4. Loop forever (park); never exit.

> **`registry` is spawned by the `supervisor`, not init** (naming Phase 3b, §11,
> `docs/naming-design.md`). The supervisor owns naming, so it spawns the name service first and
> holds its cap to wire every other service. Its boot-time spawn failure is still fatal (the
> supervisor aborts → kernel panic), enforced there now instead of here.

## What init does NOT do

- Restart services. That is `supervisor`'s job.
- Spawn application services (ping, pong, etc.). That is `supervisor`'s job.
- Hold the `service_control` capability. That belongs only to `supervisor`.

## Failure semantics (§6.2)

If init dies, the kernel panics and the system reboots. There is no recovery. The panic reason is written to serial and the crash page.

## Contract (`contracts/init.toml`)

init declares only `spawn = [supervisor, logger]` and `log_write`. It does not declare IPC send/receive endpoints because it never participates in normal IPC traffic after startup.
