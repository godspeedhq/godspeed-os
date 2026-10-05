# WiFi on a USB dongle: `wifi-usb` (design, 2026-10-05)

The Pi 2 has no radio of its own, and the PCs have none either. A USB WiFi dongle - a Realtek
RTL8188CUS today, `0bda:8176` - gives every board with a USB host a radio. This is the plan for driving
one, and the record of each card that does.

## 1. The two decisions, both the operator's (2026-10-05)

**The dongle's driver is its own service, `wifi-usb`, not part of `wifi-driver`.** The dongle must work
behind any USB host - `dwc2` on the Pi 2, `xhci` on the Wyse 5070, the T630, the Pi 4 and the VisionFive
- and a board with an onboard radio may have a dongle as well. Two services keep two radios apart: each
restarts alone, and no board's image carries a radio it cannot have. On a board with both, `wifi`
addresses the onboard radio unless told otherwise; how it is told is decided when a scan exists.

**The USB host serves the dongle; it does not drive it.** A host binds the dongle by VID:PID - it is a
vendor-class device, so there is no class to match - and then answers `godspeed_wifi::usbfn` for that one
device: who it is, and its control transfers (bulk transfers join when frames do). It is not a passthrough
to the bus: a client cannot address another device, and the host knows nothing of the chip behind the
requests. The register file, the firmware and the 802.11 above them are `wifi-usb`'s. That is what lets
one driver run behind every host.

`wifi-usb` holds no hardware grant - no window, no arena, no interrupt, no device class. Its one peer is
the host. It is started at boot where such a host exists and is idle until a dongle is bound, which is
what "plug it in and it is picked up" costs without a new mechanism for spawning a driver on demand.

The chip itself is soft-MAC (`docs/wifi.md` 59): the host builds and parses every 802.11 frame. So above
the register file this needs a host-side MLME - scan, authenticate, associate - over a raw radio, and the
serve loop the Broadcom and AIC8800 run moves into `sdk/wifi` so both WiFi services share it. That move
is due when a scan exists, not before.

## 2. The protocol (`sdk/wifi/src/usbfn.rs`)

| Op | Request | Reply |
|---|---|---|
| `OP_INFO` 0x20 | `[op]` | `[op, status, vid(2), pid(2)]` |
| `OP_CONTROL` 0x21 | `[op, setup(8), data out...]` | `[op, status, data in...]` |
| `NOTE_RADIO` 0x2F | sent by the HOST to the driver, `[note]`, no reply cap | none |

Every reply starts `[op, status]`, so an answer to the wrong op, or a host that does not speak this, is
told apart from an answer. A control transfer carries at most 256 bytes either way, and the host retries a
transient failure itself (`dwc2` sequences transfers in software, so one bus error is not a verdict).

`NOTE_RADIO` runs the other way: the host tells the driver the binding changed, with `try_send`, and the
driver asks `OP_INFO`. It is what lets the driver block rather than poll (section 5).

## 3. The phases - each card ends at something visible

| Phase | Ends at |
|---|---|
| **U1** | `wifi-usb` reads `SYS_CFG` and `ISO_CTRL` through `dwc2`: the values milestone 1 read inside `dwc2` |
| **U1b** | the host TELLS the driver when the dongle is bound or removed: no timer in `wifi-usb` |
| **U2** | `xhci` serves `usbfn`: the same reads on a PC, the Pi 4 or the VisionFive |
| **R1** | the power-on sequence and the efuse: the dongle's own MAC address |
| **R2** | the firmware (`rtl8192cufw_TMSC.bin`, from `linux-firmware`): the chip reports it running |
| **R3** | the MAC, baseband and RF tables, receive on: beacons arrive on one channel |
| **R4** | channel switching and beacon parsing: `wifi scan` |
| **R5** | authentication and association from the host, the shared WPA2 handshake, keys in the chip: `JOINED` |
| **R6** | data frames both ways through `nic-driver`: DHCP and ping |
| **R7** | rekeys |

The register-level sequences for R1 and R2 - the power-on, the efuse map, the firmware header and download -
are taken from Linux's `rtl8xxxu` and `rtlwifi`, with rtlwifi's differences noted where they disagree
(26.14: the silicon's requirement, not their model).

## 4. U1 (2026-10-05): the plumbing - hardware-verified on the Pi 2

`dwc2` binds the dongle as the radio at boot and on hot-plug, drops it on removal, and serves `usbfn` for
it; milestone 1's two reads at bind stay, as the line a board log is checked against. `wifi-usb` asks
`dwc2` every 2 s whether a dongle is bound - saying so once when it is not - and when one is, reads the
same two registers through it and decodes `SYS_CFG` as `rtl8192cu_identify_chip` does.

Its prediction: the same values milestone 1 printed (`SYS_CFG = 0x04400735`, `ISO_CTRL = 0x541c82f8`),
now through two services, and the decode: an RTL8188C (1T1R), cut A, made by TSMC, a normal chip, so its
firmware is `rtl8192cufw_TMSC.bin`.

Two gates had the same boundary bug, found by adding this service: the parsers of the supervisor's spawn
table ran the LAST row on to the end of the file, so it was credited with whatever privilege or device
class appeared below the table. `dwc2`, last for months, was pinned as holding `hw:PCI`, which nothing
grants it. Both parsers stop at the end of the table now, and the phantom pin is gone.

**On the board (14:39):** every line as predicted. `dwc2` read milestone 1's values and bound the dongle;
50 ms later `wifi-usb` read the same two through it and decoded an RTL8188C (1T1R), cut A, TSMC, normal
chip, so `rtl8192cufw_TMSC.bin`. Hot-plug both ways while the stick moved ports: the dongle out of port 5
(`usb: WiFi dongle removed`, and `wifi-usb` said at once that none was bound), back in port 4 (rebound and
read again 1.4 s later), the stick re-mounted on port 2 throughout.

**A `dwc2` bug the first QEMU boot found, fixed with it.** With no USB stick bound, `dwc2` answered EVERY
request with the block protocol's one-byte "no disk" error, because its no-disk paths never routed by op -
so a diskless Pi 2's `nic-driver` and now `wifi-usb` both got a disk error back. `dispatch` takes the disk
as optional now: radio and net requests are answered whether or not a stick is in, and only a block
request gets the no-disk answer.

## 5. R1 and U1b (2026-10-05) - built, NOT YET RUN

**R1 (`services/wifi-usb/src/rtl8188.rs`).** After U1's identification: the efuse, then the power-on, in
`rtl8xxxu`'s order. The efuse loader is enabled as Linux enables it, the physical efuse is walked into its
512-byte logical map (a header per section, a mask of absent words, the extended header where the low five
bits read `0x0F`), and the ID (`0x8129`), VID, PID and MAC (offset `0x16`) are read from it. Then the ten
steps of `rtl8192cu_power_on`, each logged when it fails. Every wait is bounded in milliseconds, not in a
count of reads: a read here is a USB round trip through two services, so Linux's "1000 reads" means
nothing at this distance. Its prediction: ID `0x8129`, VID:PID `0bda:8176`, a MAC that is neither all
zeros nor all ones - the label on the dongle, if it has one, is the real check - and `CR` reading `0x..ff`.

**U1b: told, not polled - the operator's question, "is it interrupt driven?".** U1's driver asked the host
every 2 s whether a dongle was bound. Now it asks once at start, then blocks in `recv` with no timer; the
host sends `NOTE_RADIO` when it binds or loses the dongle, and once at the end of its own boot enumeration,
bound or not - on a respawn that is the only way the driver learns the dongle did not come back. The host
sends with `try_send` and reacquires the driver's cap once if it is stale; a notice that still cannot be
delivered is logged, except the one at enumeration's end, which on a first boot has nobody to reach yet
(the driver is spawned after the host and asks for itself). `dwc2` gains `wifi-usb` as a send peer for it,
pinned in `COMMANDMENTS.baseline.toml`. What is still polled, by necessity: the chip's own status bits
during a bring-up - the efuse's ready bit, `MAC_ENABLE`, later the firmware's ready bit - which it reports
only when read, as Linux reads them. Frames, from R3, come the way the binding does: the host is driven by
its USB interrupt and tells the driver.

**One way of writing a service or a driver, for v1 (the operator, 2026-10-05): `gs`.** Where `gs` lacks a
mechanism a driver needs, `gs::driver` gains it (`backlog/71`, `docs/driver-library.md`); the raw SDK is not
the alternative. `wifi-usb` is written on the standard library throughout: requests through
`gs::call::request_within` (which reacquires a stale cap once and never re-sends after a deadline), its
receive loop on `gs::ipc::recv`, `take_sent_cap` and `reply`, its holds on `gs::driver::delay`, its waits and
its timings on `gs::driver::wait`. It holds no hardware, so it touches no hardware API at all; the SDK
appears only as the `ServiceContext` and `Message` types every `gs` call takes, and `ctx.log`, which is the
one way every service logs (CLAUDE.md 11.4). `dwc2`'s new notification is on `gs::ipc::try_send` and
`gs::cap::reacquire`; its REPLY to `wifi-usb` still goes through the raw SDK, because it shares `dwc2`'s
request dispatch with the block and network servers, which are raw throughout - converting that dispatch
is the stdlib-dogfood branch's work, not this one's.

**Three gates fixed on the way, each found by doing the consistent thing:**

- `IX-peer-reacquire` credited the SDK's reacquire and the stdlib's `request` calls but not the stdlib's
  own `gs::cap::reacquire`, so a service that reacquired the stdlib way FAILED the build. It is credited
  now, and `IX-stdlib-delegates` verifies its body reaches a real reacquire, with a probe that proves the
  check fires.
- `scripts/arm_build.py`'s `verify_image` looked for the VideoCore alias anywhere in the image - and since
  `pwm-audio` (2026-10-03), which applies the same alias to its own DMA correctly, every `--qemu` Pi 2
  build failed it whatever `dwc2` held. It now looks in `dwc2`'s own ELF, and requires the image to embed
  exactly that ELF.
- That second half caught a real stale embed on its first hardware build: after a `--qemu` build, a
  hardware build kept the QEMU `dwc2` inside the supervisor, because cargo copied the up-to-date hardware
  `dwc2` back into place with its OLD time and the supervisor re-embeds by time. That image would have
  DMAd to the wrong addresses on a Pi - no keyboard, no stick - and the old check passed it. `arm_build`
  now stamps the two services built in variants (`dwc2`, `fs`), and a QEMU-then-hardware flip was rebuilt
  both ways and checked. The R1 card already on the SD card was checked the same way: it embeds the
  hardware `dwc2`.

## 6. R2 (2026-10-05): the firmware - built, NOT YET RUN

The 8051's program is `rtl8192cufw_TMSC.bin` from upstream `linux-firmware`, in `nonfree/rtl8192cu` with
Realtek's licence and its digest (`PROVENANCE`): binary redistribution is permitted with the notice
attached, the footing `docs/wifi.md` 59 already decided for this chip. `build.rs` embeds it and passes its
FNV-1a hash through; the service recomputes the hash over what the binary holds before trusting it, the
check `wifi-driver` learned it needs (a length is a constant whether or not the bytes are kept).

The file is a 32-byte header - signature `0x88C1` (an A-cut 8188C), version 88.2, 16094 bytes of code - and
the code goes to the chip in 4 KiB pages, each selected in `MCU_FW_DL`'s third byte and written through the
window at `0x1000` in 128-byte control transfers: 126 transfers in all. That plan is `rtl_fw.rs`, which names
nothing outside `core`, so `scripts/host_test_check.py` runs its tests on every build - including the one
that caught its author's own arithmetic, the last block at `0x1E80`. The download and the start are
`rtl8xxxu_download_firmware` and `rtl8xxxu_start_firmware` step for step: the 8051 enabled, a firmware
already running from RAM reset first, the download enabled and its checksum report reset, the blocks, the
download disabled whatever happened; then the chip's checksum report, `READY` set, the 8051 reset so it
starts from RAM, and `WINT_INIT_READY` - the firmware's own word that it runs. The download is tried up to
six times, `rtl8xxxu_init_device`'s figure.

**Where it departs from Linux, on purpose:** `rtl8xxxu` sets up the reserved pages and queue priority
before the download when the MAC was cold; `rtlwifi` does not, and neither treats them as the download's
prerequisite, so R2 does the download straight after the power-on and leaves those to R3 with the rest of
the MAC's setup.

Its prediction, after R1's lines: `firmware rtl8192cufw_TMSC.bin VERIFIES - signature 0x88c1, version 88.2,
16094 bytes of code`, then `firmware downloaded - 126 blocks`, then `the firmware is RUNNING - MCU_FW_DL=...`
with bit 6 (`WINT_INIT_READY`) set.
