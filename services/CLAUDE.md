# services/

All userspace services. Each service is a separate Rust crate that links against `sdk/rust`.

## TCB members (§6.1) - trusted root

| Service         | Role |
|-----------------|------|
| `supervisor/`   | Holds restart authority + name authority; **spawned directly by the kernel** (init removed, Phase 5). Trusted, but **restartable** (Phase 6) |

The supervisor is trusted root, but **it is restartable** (Path C / Phase 6, §6.2): when it dies (a
fault, or `chaos kill-storm supervisor`) the **kernel respawns it** - unconditionally and forever (no
bound; a bound would re-introduce the reboot and hand an attacker a DoS) - and the respawned supervisor
**reconciles**, adopting the still-running services (reacquiring each by name from the kernel directory)
instead of duplicating them. So its death is *recovered, not a reboot*. The **only unkillable component
is the kernel itself** (`{kernel}`). Pinned by §22 Test 15.

## Restartable services

**Directly auto-restarted** - the kernel notifies the supervisor of their death, which respawns them:

| Service      | Notes |
|--------------|-------|
| `block-driver/` | Restartable (Phase D); holds no persistent state; re-inits the controller on respawn |
| `fs/`        | Restartable (Phase D); re-mounts to a consistent state via its crash-consistency journal (§6.8) |
| `shell/`     | The user's interface - a crash or `kill shell` respawns a fresh prompt (in-flight command lost - a re-init, not a resume). "Nothing escapes" |
| `dwc2/` | The ARM (Pi 2) USB host driver - controller, enumeration, HID, mass storage and the smsc95xx NIC. Was `kernel/src/arch/arm/dwc2.rs` until ARM routed device IRQs to userspace (`USB_VECTOR`); that file is deleted. Restartable: a respawn re-runs core bring-up and re-enumerates, which chaos exercises thousands of times per run |
| `xhci/` `ehci/` | USB host drivers - own-death respawn re-grants MMIO/DMA/IRQ caps + re-inits the controller + re-enumerates devices. Without this, a `chaos max-carnage` that kills them in its last rounds left the keyboard dead until a lucky supervisor respawn |
| `events/`    | Stateless; respawn drains the kernel ring buffer afresh |
| `time/`      | The wall clock as a service. Restartable; a respawn re-reads the persisted floor (`/clock.last`) and re-asks the network, so the clock is re-established rather than resumed |
| `control/`   | The operator control channel (COM2 / second UART) the test harness drives. Restartable; a respawn re-opens the port |
| `hw-enumerator/` | PCI enumeration in USERSPACE (x86). Restartable: a respawn re-walks the bus, so a death costs a rescan and nothing else |
| `nic-driver/` | The ethernet driver (e1000 / RTL8168 / GENET / smsc95xx by port). Restartable; a respawn re-initialises the controller and re-establishes the link |
| `net-stack/` | ARP/ICMP/UDP-DHCP/DNS/TCP. Restartable; a respawn re-configures from the link (or stays unconfigured and RESPONSIVE if there is none) and clients reacquire by name |
| `console/`   | The terminal - owns the display (`docs/console-service.md` §9). A respawn re-maps the framebuffer grant, clears it, and renders from the next byte on; scrollback is lost because it lived in the dead instance's grid (a re-init, not a resume). While it is dead the kernel's `bootcon` floor takes the screen back, so the machine is never mute |

`block-driver` must respawn before `fs` (fs's send-peer cap to it wires at spawn). The kernel notifies
the supervisor only for this **named set** (not probes), so ordinary probe/app churn never floods it.

**This table is the set, and it is checkable.** The authority is the `matches!` on `task_name` in
`kernel/src/task/scheduler.rs` that bumps the restart count; a name missing from THERE dies without
accruing a restart, so `observe` reports 0 for a service that died repeatedly - which is exactly how
`time` and `control` were found missing. A name missing from the table HERE is the same drift one
layer up: five managed services (`time`, `control`, `hw-enumerator`, `nic-driver`, `net-stack`) were
absent from this list while the kernel had been restarting them all along. If you add a service to
that `matches!`, add a row here.

A respawn is always a **fresh instance**: the supervisor spawns a new task with a *new* endpoint
(generation bumped) and *fresh* caps minted from the contract - never the dead instance's. The dead
generation goes stale, so clients get `EndpointDead` and reacquire by name (§14.3). The service never
restarts *itself* (a dead task can't); the kernel is the messenger, the supervisor the actor.

**Revived on a supervisor respawn (only)** - `ping`, `pong` (demo services, bare-metal skips them) are
not individually watched; a supervisor respawn re-runs its boot sequence and re-spawns them fresh.

## Spawned on demand, and deliberately NOT restarted

| Service | Notes |
|---------|-------|
| `recorder/` | Drains the `events` log to a file (`events persist`). The shell spawns it on demand; it is absent from the boot set AND from the kernel's managed-service lists, which is what keeps the whole persistence feature free of a kernel change. It is not restarted on death **on purpose**: a respawned recorder would not know its target path, so it would be alive and writing nothing while `status` said "running" - worse than dead. The capture file opens with a header and closes with a footer, so one without a footer says it died. See `services/recorder/CLAUDE.md` |

## Supervisor spawn order

The supervisor spawns services in this order, observed on hardware (Pi 4, and the same on x86):

0. **events**, then **console** - console before anything that produces console output, so the
   display changes hands once, early, rather than mid-boot
1. **time**, **control**, **hw-enumerator** - the clock, the operator channel, and bus enumeration
2. the **storage chain, in dependency order**: the USB/AHCI host driver (**xhci** / **ehci** /
   **dwc2** by port), then **block-driver**, then **fs**. `block-driver` must precede `fs` because
   fs's send-peer cap to it wires at spawn
3. **shell** - after storage, so the first prompt can already reach the disk
4. **nic-driver**, then **net-stack**
5. In the identity/QEMU build only: **pong** (core 1) before **ping** (core 0), so ping's SEND cap is
   wired at ping's spawn time, then 178 probe services (§22 test infrastructure). A `bare-metal`
   build skips both
6. Logs `"supervisor: ready"`

The order is a DEPENDENCY order, not a preference, and the ordering constraint is the same one §14.3
describes: a service spawned before a peer it declares comes up without a cap to it. That is survivable
- the declaration is kept and the peer is reacquired by name - but it costs a round of failure and
recovery, so the sequence above avoids it where it can be avoided.

Pong and ping start communicating within ~10 s of boot. `"supervisor: ready"` appears after all spawns complete.

## Adding a new service

1. `osdev new <name>` - scaffolds the directory.
2. Write `contracts/<name>.toml` - declare only what the service actually needs.
3. Implement `service_main(ctx: ServiceContext)` - use `ctx.capability()` for every privileged action.
4. Add the crate to the workspace `Cargo.toml`.
5. Run `osdev validate` - must pass before any PR.

## Service rules

- No global mutable state (§3.9). Per-task state is fine; anonymous singletons are not.
- No `unsafe` in service code (§18.2). If you think you need `unsafe`, you need the kernel instead.
- Services must be restartable unless explicitly listed in the TCB (§3.6).
- A service that calls `try_send` in a loop toward another service that also sends back must use `try_send` on both sides - not blocking `send` (§8.9).
