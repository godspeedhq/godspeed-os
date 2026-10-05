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

**The transmit queues come first - corrected the same day.** As first written, this section said R2 could
skip the page reservation and queue priority `rtl8xxxu` sets before the download, "since `rtlwifi` does not
set them before the download either". That was asserted, not read, and it is false: `rtlwifi`'s
`_rtl92cu_init_mac` does the power-on, the link-list table, the page reservation and the queue priority,
and runs BEFORE `rtl92c_download_fw`. Both drivers set the queues up first, so R2 does too, in
`rtl8xxxu`'s order and before it reached a board: whether the MAC is cold, and which transmit queues the
dongle's endpoints serve (`NORMAL_SIE_EP_TX`, or the bulk OUT endpoints in its configuration descriptor where
that reads 0), are asked BEFORE the power-on; after it, a cold MAC gets its 0xF8 pages reserved
(`RQPN`), every MAC gets the queue priority (`TRXDMA_CTRL`) and the receive FIFO's boundary (0x27FF). The
values are `rtl_queues.rs`, pure and host-tested against `rtl8xxxu`'s own examples (`0x80E9020C`, `0xF5F0`).
`rtl8xxxu` builds the link-list table after the firmware rather than before it, as `rtlwifi` does; that is
left to R3 with the rest of the MAC's setup.

Its prediction, after R1's lines: `the MAC is cold; transmit queues: ...` before the power-on, `transmit queues set
up` after it, then `firmware rtl8192cufw_TMSC.bin VERIFIES - signature 0x88c1, version 88.2,
16094 bytes of code`, then `firmware downloaded - 126 blocks`, then `the firmware is RUNNING - MCU_FW_DL=...`
with bit 6 (`WINT_INIT_READY`) set.

## 7. U2: `xhci` - the design, from a reading of the driver (2026-10-05), NOT BUILT

`wifi-usb` already asks whichever host it was wired to (`HOSTS`, `gs::ipc::peer`), so on the driver's side U2
is a spawn row. On `xhci`'s side it is real work, because that driver was written around keyboards and one
disk, and five of its properties stand in the way:

1. **Nothing survives a hot-plug.** Every arrival or removal re-initialises the controller and re-enumerates
   from scratch (`'reenum`). A radio binding is rebuilt each pass, and whether to send `NOTE_RADIO` is a
   comparison against the previous pass - the way `disk_was_bound` and `prev_sigs` already work.
2. **Its control transfer has no OUT data stage** (`control`, IN or none only), and every register write
   is one. It needs TRT=2 on the setup TRB and DIR=0 on the data TRB.
3. **That transfer is safe only during enumeration.** It keeps no ring cursor or cycle state and takes
   the first transfer event from ANY slot. A control transfer served at runtime needs what
   `hub_port_status` has: a persistent cursor and cycle, a Link-TRB wrap, and the event matched to its own
   TRB - with any HID report it consumes re-armed (`eaten`).
4. **Nothing watches a device it did not bind as a keyboard or a disk.** The radio's root port needs the
   CCS watch the keyboard has; behind a hub, the `GET_STATUS` watch.
5. **Requests are dropped unanswered on two idle paths** (the rescan drain, `wait_for_port`), and a pass
   with no keyboard and no disk never reaches the serve loop - which is exactly the pass a radio alone
   produces. Both must answer, as `dwc2`'s no-disk fix made it answer.

The binding point is where the VID and PID are read (`enumerate_one` for a root port, `address_downstream`
behind a hub), before the class decision; the slot and its DMA slice are kept instead of released, and the
configuration set by hand. `MAX_SLICES` (6) and the early stop at two keyboards and a disk must count it.
The serve branch goes beside `serve_if_block`'s allow-list, named op by op as its own comment asks, and the
crate gains `godspeed-wifi` for the `usbfn` constants. Then: the supervisor's `usb_radio` fact derives from
`xhci` as well as `dwc2`, `xhci` gains `wifi-usb` as a peer, and `wifi-usb`'s row and contract name the
host its board has.

**Why it was not built unattended:** it is the driver that carries the keyboard and the disk on the Pi 4,
the VisionFive and both PCs, hardware-verified on all four, and the dongle path cannot be shown in QEMU. It
is a card per board with the operator present.

## 8. R3a (2026-10-05): the MAC, the baseband and the RF, tuned to one channel - built, NOT YET RUN

R3 is two cards. **R3a** is everything `rtl8xxxu_init_device` does after the firmware that bears on
receiving, `rtl8xxxu_start`'s RF enable, filters and gain, and `rtl8xxxu_gen1_config_channel` for channel 1
at 20 MHz - register writes only, all in `wifi-usb`, nothing in `dwc2`. **R3b** is the bulk IN path in `dwc2`
(its own host channel and buffer, armed in the background and harvested on the USB interrupt, with a notice
to `wifi-usb` as the binding has) and the receive descriptor: beacons. R3b waits for R1 and R2 to run.

**The tables are generated, not typed.** `rtl_tables.rs` was produced by a script that reads each table from
Linux's source between its declaration and its terminator: the MAC defaults (87 entries), the 1T baseband
(186), the standard AGC (160) and RF path A (141, four of them 50 ms pauses), duplicates kept, every entry in
order, the source files' SHA-256 in its header.

**Written from the source, function by function** - after R2's correction, nothing here rests on a summary:
`rtl8xxxu_init_mac` (and `MAX_AGGR_NUM`), `rtl8xxxu_gen1_init_phy_bb`, `rtl8xxxu_init_phy_rf` with
`rtl8xxxu_init_rf_regs`, `rtl8xxxu_write_rfreg` and `rtl8xxxu_read_rfreg` (path A, LSSI and HSSI), the switch
words (0x870 = 0x07000760, 0x860), the transmit boundaries, `PBP`, `rtl8xxxu_init_llt_table` (pure and
host-tested in `rtl_queues.rs`), `rtl8xxxu_gen1_usb_quirks`, the receive configuration (`RCR` without the
BSSID checks, so any network's beacons pass), aggregation off, CCK and OFDM on, `rtl8723a_phy_lc_calibrate`,
`rtl8xxxu_gen1_enable_rf`, the receive filter maps and gain.

**Left out, on purpose, and why:** the transmit power, the response rate set and retry limits, the EDCA, ACK
and beacon timings (transmit, which R5 needs and R3 does not), the IQ calibration (it improves image
rejection and EVM; the baseband table loads default matrices, and a 1 Mb/s beacon does not need it), and the
thermal meter.

**Its check is the RF chip itself.** After the channel is set, `RF_MODE_AG` is read back through the HSSI
path - the one register that only a working RF serial interface can answer - and its channel field must read
1. Prediction, after R2's lines: `MAC, baseband and RF set up in ... ms (137 RF registers); RF_MODE_AG reads
0x.....  - channel 1, as asked`. A wrong channel, or all ones, says the RF path is not answering.
