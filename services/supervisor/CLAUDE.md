# services/supervisor/

Restart authority. TCB member (§6.1). **Non-restartable.**

## Responsibilities

- Read the boot manifest and spawn all non-TCB services per placement rules (§9.2).
- Monitor services for death (via kernel death-notification endpoint).
- Kill and restart failed services.
- Expose `kill` and `restart` API (§14.4).
- Log all lifecycle events.

## Sole holder of `service_control`

The `service_control` capability is held **only** by supervisor. No other service can kill or restart another service. This is the enforcement mechanism for §3.1 (no ambient authority) at the service lifecycle level.

## Placement on restart (§9.2, §14.4)

When supervisor calls `restart(name, placement_override)`:
- If `placement_override` is `Some(n)`: requires core `n`; fails with `PlacementInvalid` if that core is unavailable.
- If `placement_override` is `None`: re-evaluates from the service contract — same rules as initial spawn.
- **The previous core is NOT remembered.** A service on core 1 that is restarted without an override may land on core 2.

## Failure semantics (§6.2)

Supervisor death = kernel panic = system reboot. No silent recovery. This is intentional: the supervisor is the system's recovery authority; without it there is no meaningful recovery possible.

## API (§14.4)

```rust
supervisor.kill(service_name)                              -> Result<()>
supervisor.restart(service_name, placement_override?)      -> Result<()>
```

Both require the `service_control` capability which only supervisor holds.
