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
| `dwc2/` | The ARM (Pi 2) USB host driver - controller, enumeration, HID, mass storage and the smsc95xx NIC, and the host for a USB WiFi dongle: it binds the Realtek by VID:PID (configured, its bulk IN found), serves `godspeed_wifi::usbfn` for it to `wifi-usb`, tells `wifi-usb` when it binds or loses it, and keeps the dongle's bulk IN armed on channel 5, taken on the USB interrupt and held until `wifi-usb` collects it (R3b hardware-verified; `docs/wifi-usb.md`); and sends its frames on a bulk OUT, on the disk's bulk channel (`OP_BULK_OUT`, R5a, hardware-verified: the frames reach the air). Was `kernel/src/arch/arm/dwc2.rs` until ARM routed device IRQs to userspace (`USB_VECTOR`); that file is deleted. Restartable: a respawn re-runs core bring-up and re-enumerates, which chaos exercises thousands of times per run |
| `xhci/` `ehci/` | USB host drivers - own-death respawn re-grants MMIO/DMA/IRQ caps + re-inits the controller + re-enumerates devices. Without this, a `chaos max-carnage` that kills them in its last rounds left the keyboard dead until a lucky supervisor respawn |
| `events/`    | Stateless; a respawn starts with empty volatile stores. Logs never pass through it - `ctx.log()` writes the kernel ring and serial directly, and nothing drains that ring (CLAUDE.md 11.4) |
| `time/`      | The wall clock as a service. Restartable; a respawn re-reads the persisted floor (`/clock.last`) and re-asks the network, so the clock is re-established rather than resumed |
| `control/`   | The operator control channel (COM2 / second UART) the test harness drives. Restartable; a respawn re-opens the port |
| `hw-enumerator/` | PCI enumeration in USERSPACE (x86_64, aarch64, riscv64 - every port where configuration space is reachable; not arm32, which has no PCI). Restartable: a respawn re-walks the bus, so a death costs a rescan and nothing else |
| `nic-driver/` | The LINK FRONT END: the ethernet driver (e1000 / RTL8168 / GENET / smsc95xx / dwmac by port), and on the Pi 4 and the VisionFive also the bridge to `wifi-driver` over the frame ops when the cable is out - and on the Pi 2 the same bridge to the USB dongle's `wifi-usb` (R6, `docs/wifi-usb.md`) (the cable always wins - `Carrier` in its genet backend). Restartable; a respawn re-initialises the controller and re-establishes the link |
| `net-stack/` | ARP/ICMP/UDP-DHCP/DNS/TCP. Restartable; a respawn re-configures from the link (or stays unconfigured and RESPONSIVE if there is none) and clients reacquire by name |
| `counter/` | An `examples/` service, but the supervisor's `is_watched` names it beside `MANAGED`, so it is spawned `SPAWN_FLAG_WATCHED`, accrues restarts and is respawned like the rest. Listed here because this table's rule is that the watched set and this set agree |
| `wifi-driver/` | The Pi 4's onboard CYW43455 radio, over SDIO (`docs/wifi.md`). Restartable: a respawn re-grants its SDIO register window by device kind (`WIFI_SDIO`) and looks at the card the dead instance left. Function 2 up means a live firmware, which it adopts (below). A card that answers but has no function 2 is power-cycled through `DevicePower` and identified from CMD0 as one just powered up; the CCCR RES reset of the card's I/O side (CMD0 alone leaves an I/O card where the dead instance left it, and a CYW43455 left initialised answers no CMD5; `chaos max-carnage` found this as 397 respawns that all failed to identify) is used only where there is no power control. An empty bus - no card answering at all - gets the chip's power asserted and is identified once more. Either way it is a re-init, not a resume, so a half-finished transaction on the old instance is not inherited; the driver then reads its keys back from `/wifi.keys` and rejoins the network last joined. **A respawn does not restart the firmware - it ADOPTS the one the dead instance left running** (the card answers CMD52 with function 2 up; the CLM it already carries is not re-sent), because adopting is cheaper than reloading and leaves the link up (`docs/wifi.md` 46); whether a host-side reset alone would recover the chip is open - those tests ran on a slow host (`docs/wifi.md` 56), and the power cycle is shown to work, not shown to be the only way. Measured: the link is back ~7 s after a kill. A firmware that has STOPPED (killed mid-upload, or not answering) gets its power cut and restored through the kernel's `DevicePower` - the grant made renewable (CLAUDE.md 12.3 amendment, `docs/wifi.md` 47) - and starts from cold. Every load holds a LEASE on the Arm clock from `power` while it runs, because the chip's firmware traps when it is uploaded with the cores at their minimum clock (`docs/wifi.md` 55, 57); a chip that does come up trapped is served as `radio down` with its reason (`wifi status` names it), recovered by `wifi radio powercycle` (one cycle per run, re-runnable). No reboot. `wifi radio off hard` cuts the chip's power and leaves it cut: the driver stays alive, answers that the chip is powered down, and `wifi radio on` restores it. Embedded on aarch64 and riscv64. On the VisionFive 2 Lite it is phases V0-V2 of the AIC8800 driver (`docs/wifi-aic8800.md`): it proves the SD host's grant and the radio's power pin, identifies the chip over its own `dw_mmc` host, reads the chip revision from its ROM, uploads the embedded patches and `fmacfw` and starts it (about 9 s) - and then the firmware's bring-up to a station interface and the same serve loop as the Pi 4's through an AIC8800 `Station` (`aic_station.rs`): `wifi scan`, `wifi join`, `/wifi.keys`, and frames through `nic-driver`'s radio bridge (DHCP and `ping` over the radio) - hardware-verified through V6 on 2026-10-05. That loop is `godspeed_wifi::serve` since 2026-10-06, shared with `wifi-usb`, and the Pi 4 and the VisionFive each owe a check card for the move (`docs/wifi-usb.md` 10). A bring-up that stops short answers `radio down`, reason `DOWN_NOT_BUILT`. On QEMU `virt` no window is granted and it says so |
| `wifi-usb/` | A USB WiFi dongle's driver - a Realtek RTL8188CUS (`docs/wifi-usb.md`). Holds no hardware: the USB host that enumerated the dongle (`dwc2` on the Pi 2) binds it as the radio and answers `godspeed_wifi::usbfn` for that one device. Started at boot where such a host exists; asks it once whether a dongle is bound, then blocks until the host tells it the binding changed (U1b, hardware-verified). Written on the standard library throughout (`gs::call`, `gs::ipc`, `gs::driver`). U1, hardware-verified on the Pi 2 with hot-plug both ways: reads `SYS_CFG` and `ISO_CTRL` through the host and decodes the chip. R1 and R2, hardware-verified on the Pi 2: the efuse (the dongle's MAC), the power-on, the transmit queues and the 8051 firmware (`nonfree/rtl8192cu`), each block sent exactly once (`OP_CONTROL_ONCE`, R2b) with `dwc2` re-running a stage that errors, as Linux does (R2c - its boot case hardware-verified, its replug case not yet seen). R3a, hardware-verified: the MAC, baseband and RF tables (`rtl_tables.rs`, generated from Linux) and channel 1, read back from the RF chip. R3b, hardware-verified on the Pi 2: frames from `dwc2`'s bulk IN (`usbfn::OP_BULK_IN`, told by `NOTE_BULK_IN`), read by `rtl_rx.rs` (host-tested) and `mgmt`, each network's beacon named once (`rx.rs`). R4 and R4b, hardware-verified on the Pi 2: the dongle is a `Station` (`station.rs`) under the serve loop every radio shares (`godspeed_wifi::serve`), so the shell's `wifi scan`, `wifi list` and `wifi status` answer for it - a sweep of channels 1 to 13; its peers are the USB host and `fs` (`/wifi.keys`). R5a, hardware-verified on the Pi 2: the first frame it sends, a probe request on each channel the sweep tunes (`rtl_tx.rs`, host-tested). R5b, hardware-verified on the Pi 2: the join through authentication and association. R5c, hardware-verified on the Pi 2: the WPA2 four-way handshake through the shared supplicant, keys into the chip's CAM - JOINED. R6, hardware-verified on the Pi 2: data both ways through `nic-driver` when the cable is out, encrypted and decrypted by the chip - a DHCP lease and `ping` over the dongle. R6b, hardware-verified: `wifi radio off` and `on`, the rejoin after `on` included. R7, built: the access point's group rekey answered. R8, rate mask hardware-verified (the speed-up not yet measured): the data rate left to the chip's firmware, within the access point's rates (`rtl8188::joined`). R9, the power-down hardware-verified (the chip comes back cold, as from a plug-in): `wifi radio off hard` and `powercycle` as the chip's own power-down, Linux's `rtl8192cu_power_off` - the firmware stopped by force if it does not answer, the USB port never touched - with the shell's restart bringing it up cold. A respawn has no reply mailbox, so a host notice can arrive in place of an answer; `usbfn::OP_SYNC` recovers it (`docs/wifi-usb.md` 19, hardware-verified with 19 restarts and `chaos max-carnage`; the cost on a respawn is in `backlog/74`) |
| `power/` | The machine's power policy (`docs/power.md`). Today: the Arm clock. A service that needs the cores fast asks for a LEASE of up to 30 s; while any is open the clock is at its maximum, and when none is, at its minimum. A lease nobody releases expires on its own, so a holder that dies cannot pin the machine at full power. Restartable: a respawn knows of no lease and puts the clock at its minimum, and a holder whose lease died with it runs slower until it asks again. Holds `CPU_CLOCK` alone |
| `audio-driver/` | Intel High Definition Audio (`docs/audio.md`), x86 only. Interrupt-driven (MSI) and IOMMU-confined where there is one. Restartable: on its death the kernel stops the controller's bus mastering and releases its confinement (keyed on the device the spawn gave it, not its name), and a respawn resets the controller, re-surveys the codec and restarts the command rings - a re-init, not a resume, so a sound that was playing stops |
| `pwm-audio/` | The Pis' 3.5 mm jack (`docs/audio.md`, "The Pis"): PWM fed by the SoC's DMA engine, the same protocol as `audio-driver`. The kernel routes the jack's pins and starts the PWM clock as part of the grant. Restartable: a respawn resets the DMA channel and the PWM and silences the ring - until then the engine keeps looping whatever the ring last held, a short sound rather than nothing |
| `console/`   | The terminal - owns the display (`docs/console-service.md` §9). A respawn re-maps the framebuffer grant, clears it, and renders from the next byte on; scrollback is lost because it lived in the dead instance's grid (a re-init, not a resume). While it is dead the kernel's `bootcon` floor takes the screen back, so the machine is never mute |

`block-driver` must respawn before `fs` (fs's send-peer cap to it wires at spawn). The kernel notifies
the supervisor only for tasks spawned **watched** (not probes), so ordinary probe/app churn never floods it.

**The supervisor's `MANAGED` is the set, and it is the only copy.** Every service in it is spawned with
`SPAWN_FLAG_WATCHED`, and the kernel reports and counts the death of exactly those tasks - it keeps no
list of service names (`docs/audio.md`, "No service names in the kernel"). It used to keep two, and they
drifted: `time` and `control` were missing from the restart counter, so `observe` reported 0 for services
that died repeatedly. `V-managed-watched` in `scripts/commandments.py` checks the whole chain. A service
missing from the table HERE is the same drift one layer up - five managed services (`time`, `control`,
`hw-enumerator`, `nic-driver`, `net-stack`) were once absent from it. If you add a service to `MANAGED`,
add a row here.

A respawn is always a **fresh instance**: the supervisor spawns a new task with a *new* endpoint
(generation bumped) and *fresh* caps minted from the spawn request (its `IMAGES` row; CLAUDE.md 13.6) - never the dead instance's. The dead
generation goes stale, so clients get `EndpointDead` and reacquire by name (§14.3). The service never
restarts *itself* (a dead task can't); the kernel is the messenger, the supervisor the actor.

**Revived on a supervisor respawn (only)** - `ping`, `pong` (demo services, bare-metal skips them) are
not individually watched; a supervisor respawn re-runs its boot sequence and re-spawns them fresh.

## Spawned on demand, and deliberately NOT restarted

| Service | Notes |
|---------|-------|
| `recorder/` | Drains the `events` log to a file (`events persist`). The shell spawns it on demand; it is absent from the boot set AND from the supervisor's `MANAGED`, which is what keeps the whole persistence feature free of a kernel change. It is not restarted on death **on purpose**: a respawned recorder would not know its target path, so it would be alive and writing nothing while `status` said "running" - worse than dead. The capture file opens with a header and closes with a footer, so one without a footer says it died. See `services/recorder/CLAUDE.md` |
| `copier/` | The service behind `background` (`utilities/55_background.md`). The shell spawns it on `background copy ...` or `background delete ... recursive` and it idles until told what to do; it holds `fs` and its log and **no console capability**, which is what stops a detached job writing over a prompt somebody is typing at. Not restarted on death for the recorder's reason: a respawned copier would not know what it was copying, so it would be alive and doing nothing while `jobs` said `running`. The shell reports that death as the job being `lost`. See `services/copier/CLAUDE.md` |

## Supervisor spawn order

The supervisor spawns services in this order, observed on hardware (Pi 4, and the same on x86):

0. **events**, then **console** - console before anything that produces console output, so the
   display changes hands once, early, rather than mid-boot
1. In non-bare-metal builds only: **pong** (core 1) before **ping** (core 0), so ping's SEND cap is
   wired at ping's spawn time; then the probe services (183 in the full build, 16 in `identity-only`)
   and **observe**. **These come SECOND, not last.** Spawning the pair early gets cross-core IPC
   running within ~10 s of boot, and the probe loop alone takes 18-120 s on Windows TCG
   (`supervisor/CLAUDE.md`). A `bare-metal` build skips all of them
2. **time**, **control**, **power**, **hw-enumerator** - the clock, the operator channel, the power
   policy (before anything that leases the Arm clock), and bus enumeration
3. the **storage chain, in dependency order**: the USB host driver where the disk lives behind one
   (**dwc2** on arm32, **xhci** on aarch64), then **block-driver**, then **fs**. `block-driver` must
   precede `fs` because fs's send-peer cap to it wires at spawn.
   *Per-port caveat:* on x86 the disk is AHCI, so no USB host is in the chain. Every USB host named in
   `block-driver`'s peers is spawned before it, so on **riscv64** `xhci` precedes `block-driver` too
4. **shell** - after storage, so the first prompt can already reach the disk
5. **wifi-driver** (where embedded), **audio-driver** or **pwm-audio** (where embedded), then
   **nic-driver** - after `wifi-driver`, which it names as a peer on the Pi 4 - then **net-stack**; on
   x86 the USB hosts (**xhci**, **ehci**) come in before these
6. Logs `"supervisor: ready"`

The order is a DEPENDENCY order, not a preference, and the ordering constraint is the same one §14.3
describes: a service spawned before a peer it declares comes up without a cap to it. That is survivable
- the declaration is kept and the peer is reacquired by name - but it costs a round of failure and
recovery, so the sequence above avoids it where it can be avoided.

Pong and ping start communicating within ~10 s of boot. `"supervisor: ready"` appears after all spawns complete.

## Adding a new service

1. Copy `examples/00-hello` - `osdev new` is not implemented (CLAUDE.md 17).
2. Write `contracts/<name>.toml` - declare only what the service actually needs.
3. Implement `service_main(ctx: ServiceContext)` - use `ctx.capability()` for every privileged action.
4. Add the crate to the workspace `Cargo.toml`.
5. Run `osdev validate` - must pass before any PR.

## Service rules

- No global mutable state (§3.9). Per-task state is fine; anonymous singletons are not.
- No `unsafe` in service code (§18.2). If you think you need `unsafe`, you need the kernel instead.
- Services must be restartable unless explicitly listed in the TCB (§3.6).
- A service that calls `try_send` in a loop toward another service that also sends back must use `try_send` on both sides - not blocking `send` (§8.9).
