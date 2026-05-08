# Milestone 7 — Services and Supervisor Restart

> init, supervisor, registry, logger, ping, and pong reach steady state.
> Supervisor can kill and restart a service; clients reacquire via registry.

## init

- [ ] Spawns supervisor, registry, logger in order (§11.1)
- [ ] Panic if any TCB service fails to spawn (§6.2)
- [ ] Logs `"init: ready"` on serial

## supervisor

- [ ] Reads boot manifest; spawns services per placement policy (§9.2)
- [ ] `kill(service_name)` — kills the named service
- [ ] `restart(service_name, placement_override?)` — kill + respawn, placement re-evaluated
- [ ] Logs `PlacementInvalid` and skips if contracted core unavailable (§9.2)
- [ ] Logs `"supervisor: ready"`

## registry

- [ ] `register(name, endpoint_cap)` — service registers its endpoint on startup
- [ ] `lookup(name) -> endpoint_cap` — client resolves a fresh cap by name
- [ ] Generation in returned cap matches current resource generation
- [ ] Logs `"registry: ready"`

## logger

- [ ] Drains kernel ring buffer on startup (§11.4)
- [ ] Receives log messages from services holding `log_write`; writes to serial
- [ ] Logs `"logger: ready"`

## ping / pong (examples)

- [ ] ping placed on core 0; pong placed on core 1 (via contract)
- [ ] ping sends a message to pong every second
- [ ] pong receives and logs each message
- [ ] ping handles `EndpointDead` by re-looking up pong via registry

## Restart Flow

- [ ] `osdev restart pong --core 2` kills pong on core 1, respawns on core 2
- [ ] ping observes `EndpointDead`, reacquires via registry, continues sending
- [ ] New cap routes to core 2 correctly

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
