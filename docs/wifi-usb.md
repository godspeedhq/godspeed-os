# WiFi on a USB dongle: `wifi-usb` (design, 2026-10-05)

The Pi 2 has no radio of its own, and the PCs have none either. A USB WiFi dongle - a Realtek
RTL8188CUS today, `0bda:8176` - gives every board with a USB host a radio. This is the plan for driving
one, and the record of each card that does.

## 1. The two decisions, both the operator's (2026-10-05)

**The dongle's driver is its own service, `wifi-usb`, not part of `wifi-driver`.** The dongle must work
behind any USB host - `dwc2` on the Pi 2, `xhci` on the Wyse 5070, the T630, the Pi 4 and the VisionFive
- and a board with an onboard radio may have a dongle as well. Two services keep two radios apart: each
restarts alone, and no board's image carries a radio it cannot have. On a board with both, `wifi`
addresses the onboard radio unless told otherwise; how it is told is `wifi hardware use`, specified in
`utilities/56_wifi.md` 11 (both the report and the choosing are built; section 37).

**The USB host serves the dongle; it does not drive it.** A host binds the dongle by VID:PID - it is a
vendor-class device, so there is no class to match - and then answers `godspeed_wifi::usbfn` for that one
device: who it is, and its control transfers (bulk transfers join when frames do). It is not a passthrough
to the bus: a client cannot address another device, and the host knows nothing of the chip behind the
requests. The register file, the firmware and the 802.11 above them are `wifi-usb`'s. That is what lets
one driver run behind every host.

`wifi-usb` holds no hardware grant - no window, no arena, no interrupt, no device class. Its peers are the
host and `fs`, for `/wifi.keys` (section 10). On the Pi 2 the supervisor starts it when `dwc2` reports the
dongle attached and stops it when the dongle leaves (section 26, `docs/usb-device-drivers.md`); `xhci`
reports it the same way since section 27, so no board starts it at boot.

The chip itself is soft-MAC (`docs/wifi.md` 59): the host builds and parses every 802.11 frame. So above
the register file this needs a host-side MLME - scan, authenticate, associate - over a raw radio, and the
serve loop the Broadcom and AIC8800 run moves into `sdk/wifi` so both WiFi services share it. That move
is due when a scan exists, not before.

## 2. The protocol (`sdk/wifi/src/usbfn.rs`)

| Op | Request | Reply |
|---|---|---|
| `OP_INFO` 0x20 | `[op]` | `[op, status, vid(2), pid(2)]`, then where it is, from `INFO_WHERE_AT` - `xhci` only (`wifi hardware <radio>`) |
| `OP_CONTROL` 0x21 | `[op, setup(8), data out...]` | `[op, status, data in...]` |
| `OP_CONTROL_ONCE` 0x22 | as `OP_CONTROL`, attempted exactly once | as `OP_CONTROL` |
| `OP_BULK_IN` 0x23 | `[op]` | `[op, status, transfer...]` - the held bulk IN transfer, or none; the host's IN armed again |
| `OP_BULK_OUT` 0x24 | `[op, out, transfer...]` - `out` the OUT endpoint's position in the configuration descriptor | `[op, status]` |
| `OP_SYNC` 0x25 | `[op, notice]` - never answered: names a notice the driver took in place of an answer (section 19) | none |
| `NOTE_BULK_IN` 0x2E | sent by the HOST to the driver, `[note]`, no reply cap: a transfer is held | none |
| `NOTE_RADIO` 0x2F | sent by the HOST to the driver, `[note]`, no reply cap | none |

0x26 is not free: it is `usbdev::ASK`, the supervisor asking the host for its device report (section 26).

Every reply starts `[op, status]`, so an answer to the wrong op, or a host that does not speak this, is
told apart from an answer. A control transfer carries at most 256 bytes either way, and the host retries a
transient failure itself (`dwc2` sequences transfers in software, so one bus error is not a verdict) -
except `OP_CONTROL_ONCE`, for a transfer that must not reach the device twice: a firmware block (section 6).

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

**Until R4, the shell's `wifi` did not see the dongle.** `wifi` asked `wifi-driver` by name and nothing
else, so on the Pi 2 it answered "no wireless radio on this machine" with the dongle bound, the firmware
running and channel 1 tuned (seen on the R2c card, 2026-10-06). R4 closes it: the shell asks whichever
radio service is up (`RADIOS`, section 10), which is also what makes "never" wrong on the T630 and the
Wyse once U2 lets a dongle reach them.

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

## 5. R1 and U1b (2026-10-05) - hardware-verified on the Pi 2

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
`gs::cap::reacquire`, and its reply to `wifi-usb` is `gs::ipc::reply` too (`rtl::serve`); the block and
network servers beside it in `dwc2`'s dispatch are still raw - converting those is the stdlib-dogfood
branch's work, not this one's.

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

## 6. R2 (2026-10-05): the firmware - hardware-verified on the Pi 2 at boot; replugs found R2b and R2c

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

**What the Pi 2 showed (2026-10-05, three cards in a row).** R1: efuse ID `0x8129`, VID:PID `0bda:8176`, a
MAC, 31 sections in about 1.2 s (each byte a round trip through `dwc2`; a batched read is a later card), and
`powered on in 16 ms - CR=0x00ff`. U1b: the dongle unplugged and replugged twice, `wifi-usb` told each time
and brought it up again. R2, at boot: queues set up, 126 blocks in 145 ms, `the firmware is RUNNING -
MCU_FW_DL=0x000300c6`.

**R2b: a block must reach the chip once.** On two replugs in the R2 run, `dwc2` logged `STATUS stage FAILED
(XACTERR NAK)` during the download - one in the first, five in the second - and both downloads then
reported `126 blocks ... (1 try)` and failed at `the chip never reported the download's checksum`. The
download that ran clean, at boot, did not fail. The cause is in our design, not the chip's: `dwc2` retries
a failed control transfer up to four times itself, so a block whose data had arrived and whose status
stage failed was SENT AGAIN, and the driver above never saw a failure. `rtl8xxxu_download_firmware` sends
each block once and returns `-EAGAIN` when one fails, and `rtl8xxxu_init_device` then restarts the whole
download, checksum reset included, up to six times. `wifi-usb` already had that loop; it never ran. So a
block now goes as `OP_CONTROL_ONCE`, which the host attempts exactly once, and a failure stops the download
and restarts it. Register writes keep the host's retries: a register written twice holds the same value.
That a repeated block is what spoils the checksum is the reading of the log, not yet a measurement: the
card's prediction is a replug whose download fails a block and logs `the download stopped (the transfer did
not complete) - try 1 of 6`, then completes and runs. A download that fails its checksum with no block
failing would refute it.

**What R2b showed (2026-10-05): the loop runs, and cannot help, because the chip is stuck.** Seven
downloads in one boot and six replugs: two clean (126 blocks, RUNNING, R3a's channel 1 after them), five
with one `STATUS stage FAILED (XACTERR NAK)` on a block. In each of the five the download stopped, as it
now should, and then every read after it - the `MCU_FW_DL` read on the abort path, and each of the five
restarts - timed out: `DATA-IN stage timed out (channel never halted)`, the chip NAKing the data stage of
every read until it was unplugged. So the explanation R2b was built on stands half-tested: no download
failed its checksum without a block failing, but neither did a restarted download run, so whether a
repeated block is what spoiled the checksum was never measured. What WAS shown is sharper. In the R2 run
the host re-sent the whole block at once and the chip answered reads afterwards; here nothing was re-sent
and it never answered again. A control transfer whose STATUS stage is abandoned leaves this chip waiting.

**R2c: re-run the stage, as Linux's `dwc2` does.** Read from `drivers/usb/dwc2/hcd_intr.c` (fetched,
SHA-256 `4f3af1392c83d309...`): `dwc2_hc_xacterr_intr` counts the error and halts the channel "so the
transfer can be re-started from the appropriate point"; the control phase advances only on
transfer-complete; `dwc2_release_channel` fails the transfer with `-EPROTO` at the third error. Linux never
abandons a stage on one error and never re-sends a completed DATA stage. Our `dwc2` did one or the other.
`chan::stage` now re-runs a single-packet stage - a SETUP, a STATUS, a register's bytes - up to three
transaction errors, and says so when it does. A longer DATA stage still fails at once: Linux resumes one
from the packet it reached, with the saved toggle, and this driver programs a stage from its start. The
`OP_CONTROL_ONCE` of R2b stays: a block whose transfer fails outright is still not re-sent by the host.

The card (`build/kernel7-R2c.img`) predicts, on a replug where a block's STATUS errors: `STATUS stage
completed after 1 transaction error(s), re-run as Linux does`, the download `(1 try)`, RUNNING, channel 1.
Refuted by: `STATUS stage gave up after 3 transaction errors` followed by the same wall of timed-out reads.

One more thing the R2 run settled: on the U1b run the third replug came up at full speed through the hub's
transaction translator, and every vendor read failed. In the R2 run both replugs came up at high speed and
read the chip at once, so that was the insertion; it is recorded here in case it returns.

## 7. U2: `xhci` - the design, from a reading of the driver (2026-10-05); U2a hardware-verified (sections 25, 27), bulk IN and OUT (U2b, U2c) hardware-verified on the Pi 4 (sections 28-30)

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

**What R3b adds to it (2026-10-06).** `xhci` must answer `OP_BULK_IN` and send `NOTE_BULK_IN` too. Read
from its mass-storage path rather than assumed: a bulk IN there is an endpoint added by Configure Endpoint
with the DIRECTIONAL type (bulk IN is 6, xHCI 6.2.3 - `bind_msc` records that the wrong type configures a
pipe that never completes), with a transfer ring and data page in a region of its own (`msc.rs`'s
`DISK_BASE`, separate because one shared page let an armed interrupt endpoint overwrite a disk command on
the Pi 4). The radio needs the same: its own ring and a `BULK_IN_MAX` buffer, one Normal TRB queued as the
"armed" IN, and its Transfer Event taken where the interrupt already drains the event ring - matched to the
radio's slot and endpoint, which is point 3 above again. `dwc2`'s stand-aside is a fact about `dwc2`'s
non-periodic request queue; whether `xhci` needs anything like it is a hardware question, not assumed either
way.

**Why it was not built unattended:** it is the driver that carries the keyboard and the disk on the Pi 4,
the VisionFive and both PCs, hardware-verified on all four, and the dongle path cannot be shown in QEMU. It
is a card per board with the operator present.

## 8. R3a (2026-10-05): the MAC, the baseband and the RF, tuned to one channel - hardware-verified on the Pi 2

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

**What the Pi 2 showed (2026-10-05), as predicted:** `MAC, baseband and RF set up in 1498 ms (137 RF
registers); RF_MODE_AG reads 0x07401 - channel 1, as asked`. The low byte, 0x01, is the channel; the
rest is the band and bandwidth field the table loaded. Repeated after every clean download since,
including the R2c card's boot (2026-10-06).

## 9. R3b (2026-10-05/06): reading what the chip hands up - hardware-verified on the Pi 2

R3b is two halves. The `dwc2` half - a bulk IN channel of its own, armed in the background and harvested
on the USB interrupt - changes the driver that carries the Pi 2's keyboard and disk, so it waits for R1 and
R2 to run. The other half is pure and is done: given a bulk IN transfer, find the frames in it.

**`services/wifi-usb/src/rtl_rx.rs`** reads a transfer as `rtl8xxxu_parse_rxdesc16` does, from the source:
a 24-byte descriptor (`struct rtl8xxxu_rxdesc16`: the frame's length, CRC and ICV errors, the PHY status's
size in 8-byte units, the shift, the packet count - taken from the FIRST descriptor only - the rate, and
`rpt_sel`), then the PHY status, the shift, and the frame; the next packet on the next 128-byte boundary;
the walk stopping at the first descriptor's count or when what is left cannot hold a descriptor. The
signal is `rtl8723au_rx_parse_phystats`: for a CCK rate, `rtl8723a_cck_rssi` (the 8192C family's `fops`
name it; it lives in `8723a.c`), the LNA index picking one of four offsets; for OFDM, `pwdb / 2 - 110`. A
frame the transfer cut short is passed up marked, not dropped silently. Five tests, on every build.

**`sdk/wifi/src/mgmt.rs`** reads the frame: a beacon or probe response (frame control 0x80 or 0x50), its
BSSID, capability, SSID (at most 32 bytes) and the channel from its DS Parameter Set - the network's own
channel, which is not the tuned one when a neighbour's beacon leaks across. Every length is from the air,
and an element walk that would run past the end stops. Four tests, on every build. It is in `sdk/wifi`
because it is true of every radio that forwards raw frames.

**One-way debt, recorded:** the AIC8800 forwards raw beacons too, and its scan reads them with
`aic_wire::ResultInd`'s own accessors (`bssid`, `capability`, `ies`, `ssid`), written before `mgmt.rs`
existed. Two readings of one frame is what the one-way rule forbids. Moving the AIC8800 onto `mgmt.rs` is
a change to a hardware-verified scan, so it waits for the VisionFive to be on the bench, with a scan as
its card - not done unattended.

Nothing in the image called `rtl_rx.rs` until the `dwc2` half below; the `#[allow(dead_code)]` it carried
went with that change.

### The `dwc2` half (2026-10-06): hardware-verified on the Pi 2 (`build/kernel7-R3b.img`)

**At bind, the dongle is configured.** `dwc2` never sent the radio SET_CONFIGURATION: control transfers on
endpoint 0 work in the Addressed state, and every card from U1 to R3a ran that way. A bulk endpoint exists
only in a configured device, and Linux's USB core configures one before any driver's probe, so `rtl::bind`
now reads the configuration descriptor, takes the one bulk IN (`rtl8xxxu_parse_usb`'s rule), and sets the
configuration - then milestone 1's two reads, as before. A bulk IN whose packet is not 512 bytes means the
dongle came up at full speed behind the hub; that needs split transactions this driver does not run for a
bulk IN, so receive stays off and the log says so.

**One IN, armed in the background, taken on the interrupt.** Channel 5, landing at arena offset 0x4000 (after
the NIC's receive burst, asserted at build time), for `usbfn::BULK_IN_MAX` = 3584 bytes - seven high-speed
packets, holding one unaggregated receive, which Linux sizes as `IEEE80211_MAX_FRAME_LEN` (2352) plus the
descriptor. It is armed the first time `wifi-usb` asks `OP_BULK_IN`, which it does once R3a has set the
chip's receive up. Its halt is the one interrupt it raises; `rtl::service` takes a completed transfer and
HOLDS it, sends `NOTE_BULK_IN`, and arms nothing until `wifi-usb` collects it with `OP_BULK_IN` - so the chip
keeps what arrives meanwhile, and nothing is dropped between the two services. A notice the driver's full
queue refused is sent again on the next pass, because a lost one would stop receive for good. One transfer
per round trip is enough for beacons; R6's data rate is a later question.

**It stands aside for every other non-periodic transfer.** `net.rs` found, on this board, that an armed bulk
IN the device NAKs fills the core's non-periodic request queue with retries and starves a transmit (GNPTXSTS:
zero entries free). Whether it starves a control transfer has never been measured here - the R1 to R3a logs
show the NIC's IN never armed (`nohalt 0`), so they cannot answer it, though they first looked as if they
did - and a SETUP takes the same queue. So `chan::program_ping` halts the radio's IN before programming any
control or bulk transfer on another channel, and `rtl::service` puts it back from the packet it reached, with
the data toggle read out of the channel. Periodic transfers (the keyboard, the hub's status) ride the other
queue and do not stand it aside.

**The interrupt could be dropped, and now cannot.** The USB interrupt reaches `dwc2` as the message `[0x29]`
with no reply cap. Only the disk drain asked for it; the no-disk drain and the blocking receive between passes
handed it to `dispatch`, which dropped it as a capless request and left the vector masked for good. No run had
armed an IN, so none had raised one. `usb_irq` is now asked for first at all three places.

**The driver's half** (`services/wifi-usb/src/rx.rs`): on `NOTE_BULK_IN` it collects up to eight transfers,
walks each (`rtl_rx`), counts frames, CRC failures and frames cut short, reads each good frame as a beacon or
probe response (`mgmt`), and names each network once (eight at most).

**Prediction.** After R3a's line: `dwc2-svc: RTL8188CUS configured (...) - bulk IN endpoint .., 512 bytes,
received on channel 5` at bind; `wifi-usb: receive started`; within a second, `wifi-usb: beacon '...' ... on
channel 1` (or 2 or 3: a neighbour's beacon leaks across, and the channel printed is the network's own) and
`the FIRST frame from the air - ...; R3b done`; the heartbeat's `radio rx - N transfers ...` climbing with no
errors; and the keyboard and the disk unaffected. **Refuted by:** no bulk IN or a failed SET_CONFIGURATION at
bind; `receive started` and then nothing, with `radio rx` at 0 transfers (the IN never completes - the chip's
receive, or the queue); transfers climbing with no beacons and every frame failing its CRC (the descriptor
read); or a keyboard or disk that stalls once receive starts (the stand-aside).

**Result (2026-10-06): passed, at boot and on five replugs.** Every predicted line appeared. At bind,
`configured (1) - bulk IN endpoint 1, 512 bytes, received on channel 5`. Receive started 3.0 s after the
dongle was bound, and the first frame was a beacon on channel 1, 15 ms later. Eight networks were named
in the next 80 ms, between -40 and -84 dBm. In the first 80 seconds, `wifi-usb` walked 7680 transfers,
one frame each, about 100 a second and almost all beacons. **None failed its CRC and none was cut
short.** `dwc2`'s `radio rx` line showed 0 errors throughout. It also showed 50 asides, so the IN stood
aside for other transfers and came back each time. Eleven notices were late, all in the first burst: the
driver's queue was full, and each notice was sent again on the next pass as designed. `dir` listed its
11 entries, and `wifi`, `ping` and other commands were typed while receive ran.

**The replugs.** All five came back the same way: `REMOVED`, then a fresh address, configured, R1 to
R3a, `receive started`, and `R3b done`. One removal landed while `wifi-usb` was collecting a transfer,
and it said so (`collecting a transfer - the dongle is no longer bound`) and waited for the next bind.
**No STATUS stage errored on any of them.** R2c's re-run never had to fire, so the replug it was built
for is still unseen. What is shown is that a replug now works.

**What the run measured that the stand-aside was guessing at.** The NIC's own bulk IN (channel 3, which
the radio does not stand aside for) was armed for the whole run, NAKed by a LAN9514 with no cable. The
`net IRQ` line shows `13032 still in flight` across 13033 interrupts, and `net IN HCINT=0x00000010` (NAK).
Every interrupt was the radio's, because the network had no link; each one also asks the NIC for frames.
So, unlike R1 to R3a, this run had an IN that the device NAKs armed throughout. Five replugs' worth of
control transfers ran beside it, hundreds per replug (the efuse, 126 firmware blocks, the MAC, baseband and RF tables), along with the disk's bulk transfers and the
radio's own IN. None failed. That is one run, not a proof that the queue cannot starve a SETUP. But it
is the first evidence there is, and it points at the transmit-only starvation `net.rs` saw.

**One instrument mislabel, found here.** The `net IRQ` line counts every USB interrupt, and the radio's
now outnumber the network's by thousands, so "net IRQ - 13033 interrupts, 0 frames" reads as a busy NIC
with nothing to show. It is the shared vector. Relabelled `USB IRQ` in the change after this run, with the radio's share said.

## 10. R4 (2026-10-06): `wifi scan` - the dongle a `Station`, under the loop every radio shares - hardware-verified on the Pi 2

**The decision (the operator's, 2026-10-06): move the whole serve loop, not a piece of it.** What answers
the shell's `wifi` - the sweep as a state, the scan cache, the credential table and `/wifi.keys`,
auto-join, every reply layout, and `nic-driver`'s frame ops - was about 1,100 lines inside
`wifi-driver`'s `main.rs`, run by the Pi 4's Broadcom and the VisionFive's AIC8800. A second service
needed it, so it is `sdk/wifi/src/serve.rs` now (`godspeed_wifi::serve::serve`), run by both services,
and `wifi-driver` keeps a short `serve_radio` that calls it with its power host. The alternatives were a shared sweep piece
with the rest left behind, which keeps two scan policies until `wifi-driver` moved, or a scan logged and
not served; both were declined.

**Moved unchanged in what it decides.** A normalised diff of the old loop against the new one (the
mechanical renames undone) shows only these differences:

- **`who`**: every line it logs opens with the service's name instead of a written-in `wifi-driver:`.
  `keyfile` and `crypto::selftest` take it too.
- **A `Host`** (`serve::Host`): what is AROUND the radio. The power operations - `wifi-driver`'s are
  `DevicePower` and the parked SDIO host, exactly the calls the loop made inline (`SdioPower`); a dongle
  has none, and the loop answers "no control over the radio's power", which is true. And the notices
  that arrive with no reply cap: they were counted and dropped; now the host is asked first, and only
  what it calls `Ignored` is counted and dropped. A notice it calls `Changed` (a dongle bound or
  removed) ends the loop, which is the only way it returns.
- **`gs`**: receive, reply, sleep and the monotonic clock are the standard library's.
  `gs::ipc::reply` is exactly the `try_send` and reclaim the loop made by hand. The tick-counter stamps
  (the idle pull, the minute's summary, how long a request took) needed a moment that can be KEPT
  between calls, which a `Deadline` cannot be because it borrows the context. That was a gap in `gs`,
  filled there: `gs::driver::wait::Since`.
- **`crypto::selftest` runs once per service**, before the loop, rather than on entry: `wifi-usb`
  re-enters the loop on every replug.

`wifi-driver`'s one-way count fell 55 -> 25. Some of that fall is calls that became `gs`, and the rest is
code that moved into `sdk/wifi`, which the ratchet does not count. So the library's own remaining raw
calls were converted too: what is left in `serve.rs` is two `log_fmt` calls, which have no `gs`
replacement.

**This changes two hardware-verified radios, so each owes a check card** before the move counts as
verified on it: on the Pi 4 and on the VisionFive, `wifi scan`, `wifi list`, `wifi status`, a join
(auto-join from `/wifi.keys` at boot is one), and `ping` over the radio with the cable out. Both images
build, and every gate passes on both.

**The Pi 4's, run 2026-10-06 on the merge-candidate build (with everything since: the mailbox credit, the
hard-off order, the shell's wording): passed.**
- Auto-join from `/wifi.keys` at boot; `ping 8.8.8.8` 4 of 4. `wifi radio off` and `on` rejoined, 4 of 4.
- `wifi radio powercycle`, a real power cut through `DevicePower`: `radio powered down for 2.0 s`, the
  driver restarted onto the cold chip (taking its reply mailbox back), `powercycle succeeded - joined`.
- `kill wifi-driver`: restarted with its mailbox and rejoined. 7 of 9 pings: the first two were sent 0.3 s
  after the rejoin, inside `nic-driver`'s one-second back-off from the dead radio.
- `chaos max-carnage` 50 rounds, 366 kills: kernel alive, no `spawn REFUSED`, every restart (wifi-driver's
  included) took a mailbox back. Rejoined after, `ping` 7 of 7.

**The VisionFive's, run the same day: everything passed until chaos, and chaos found a `nic-driver` fault.**
- Bring-up, auto-join over 5 GHz, `ping`, `wifi radio off`/`on`, `powercycle` (`powercycle succeeded -
  joined`), `kill wifi-driver` (rejoined with its mailbox, 3 of 3): all as on the Pi 4.
- `chaos max-carnage` 50 rounds, 334 kills: kernel alive, the radio rejoined. But `ping` then said `link not
  confirmed` and kept saying it through a further kill and power cycle of the radio.
- **The cause was `nic-driver`, not the radio.** Its last respawn in the storm, with the cable out, could
  not reset the dwmac's DMA (`DMA reset did not clear in 1000000 us - bus mode was 0x00000000, now
  0x00000001`; at boot the same reset cleared in 4 us). Its failure path then served EMPTY replies to
  everything, the radio bridge's included. So a rejoined, working radio carried nothing until `nic-driver`
  itself was restarted.
- **Fixed in `nic-driver`'s dwmac backend**, with the VisionFive card owed for it:
  - `Dwmac::bring_up` hands its register window and arena back on failure;
  - the serve loop carries on with the radio bridge (`Wire::Down`), the cable counting only through a
    MAC that is up;
  - the MAC is tried again whenever the PHY reports a link, since the clock a DMA reset needs is the
    PHY's.
  - Why the reset did not clear after the storm is not diagnosed; what changed is that it no longer costs
    the radio.
- **Verified on the VisionFive (the rerun, 2026-10-06):** `chaos max-carnage` 20 rounds, 163 kills, and the
  last `nic-driver` respawn's DMA reset failed again, the same way (so it is reproducible after a storm
  with the cable out). This time `dwmac not brought up - the radio still carries the link`; the radio
  rejoined, `the cable is out - the radio carries the link`, and `ping 8.8.8.8` went 5 of 5.
- **The retry is on the cable's ARRIVAL,** once per arrival, not on every re-check while a cable sits
  there: a failed reset waits a second, and repeating it every half second would stall every request
  behind it. That correction came after the rerun, so the cable-arrival path itself is not yet seen.
- **GENET's backend had the same shape and has the same fix** (`genet_main`, `serve` with `mac: Option`):
  the radio is served when the MAC does not come up, and the MAC is tried again when a cable arrives.
  **The Pi 4's card (2026-10-06): no regression, and the failure path not reached.** `chaos max-carnage` 20
  rounds, 155 kills, kernel alive; `nic-driver` respawned 8 times and GENET came up every time; the radio
  rejoined after and `ping 8.8.8.8` went 5 of 5. The GENET failure path and the cable-arrival retry on
  both boards stay unseen, since nothing has needed them yet.
- **Also seen, at boot:** a `wifi status` typed during the AIC8800's 12-second firmware upload was never
  answered. The shell held it owed until its 30-second bound (`owed for over 30 s never came - forgotten`),
  refusing `wifi` meanwhile. The serve loop's tagged reply looks right, so the request was most likely lost
  before being served: `nic-driver` had filled its held slots probing the busy radio at that moment.
  Recorded, not diagnosed.

**The dongle's `Station`** (`services/wifi-usb/src/station.rs`). This chip is soft-MAC, so the sweep is
the host's to run: tune channel 1, listen `DWELL_MS` (150 ms, one beacon interval of 102.4 ms with room
for the hop), tune the next, through 13, then back to the channel it rested on. It is a PASSIVE scan,
with nothing transmitted. A network that beacons is found; a hidden one that only answers probes is not,
because a probe is a transmit, which waits for R5. Frames reach it the way R3b's did: `dwc2` sends
`NOTE_BULK_IN`, the loop hands it to the dongle's `Host` (`rx.rs`) with the running sweep, and each
beacon is kept as a record with its network's OWN channel, from its DS Parameter Set, and its security
from `bss::classify`, the classifier every radio uses. `scan_step` only moves the dial. A join answers
`JOIN_FAILED` with a line that names R5; the link reports not associated, which is the truth.

**The shell** asks `wifi-driver`, then `wifi-usb` (`RADIOS`), and keeps the one it found for the
command. A machine with both is answered by the onboard radio; choosing between two is not built.

**Wiring.** `wifi-usb` gains `fs` as a peer, for `/wifi.keys` - pinned in `COMMANDMENTS.baseline.toml`
with that reason, after the gate refused it unpinned. It is spawned after `fs` now, so both peers wire
at spawn.

**Prediction for the card** (the Pi 2, the dongle in):

1. At boot, before the dongle's lines: `wifi-usb: stage 0 - every primitive matches its vector`. Then
   U1 to R3b as before, ending `receive started` and `R3b done`. Then `no /wifi.keys - nothing to
   rejoin`, or `/wifi.keys loaded`, prefixed `wifi-usb:`.
2. `wifi status` no longer says "no wireless radio": the radio is on, not joined, and no scan yet.
3. `wifi scan`: `sweep started - listening on channels 1 to 13, 150 ms each`, the networks appearing
   as they are heard, and about 2 to 3 seconds later `sweep done - N network(s) ... back on the channel
   it was on` and `sweep complete - N network(s), ended by the last channel's dwell`. N should be MORE
   than R3b's eight, with channels other than 1 to 3 among them (6 and 11 are the usual others).
4. `wifi list` prints the same networks, with channel, signal and security.
5. `wifi join <name>` fails, and the log names R5.
6. The keyboard and the disk unaffected; a replug brings the dongle back and `wifi scan` works again.

**Refuted by:** `sweep fell silent ... discarded` (the dwell never ends, or the loop never turns);
`could not tune channel N` (the RF chip refusing a hop); every network on channels 1 to 3 only (the hops
are accepted and do not retune, so only channel 1's neighbours are heard); no networks at all with
`radio rx` transfers climbing (beacons not reaching the sweep); or the prompt or disk stalling during a
sweep (the hops' control transfers and the stand-aside).

**Result (2026-10-06, `build/kernel7-R4.img`): passed on the Pi 2, with one fault found and fixed.**
`stage 0` matched every vector, then U1 to R3b as before. `wifi scan` swept channels 1 to 13 in 2.3 s and
listed **16 networks**, twice R3b's eight. The prompt read "16 networks in 3 s", and the radio was back on
channel 1 afterwards. The shell's table has no channel column, so the evidence that the hops retune is
timing, not a printed channel. Five networks R3b never heard on channel 1 first appeared 1.8 s into the
sweep, where the dial was near channels 10 to 12. The records carried security: WPA2, WPA2/WPA, open,
and hidden networks shown as hidden. A join with the stored key was refused, and the log named R5. `radio
on` answered "already on", and `radio off` and `powercycle` said the dongle cannot do them. 6,144 frames,
0 failed their CRC. The keyboard worked throughout, and `fs` served `/wifi.keys`.

**The fault: the loop entered three times at boot.** `/wifi.keys loaded` and the auto-join appeared three
times. `dwc2` sends more than one `NOTE_RADIO` for one bind, and the dongle's `Host` ended the loop on each.
Before the loop was shared, `main.rs` had asked the host about every notice and ignored one that changed
nothing; the move dropped that question. Harmless while a join is refused; three real joins once R5 makes
one. **Fixed (R4b, built, not yet run):** `rx.rs` asks `OP_INFO` on a `NOTE_RADIO` and ends the loop only
if the answer differs from the binding the loop was entered for. Predicted on the next card: ONE
`/wifi.keys loaded` and one auto-join line at boot, and a replug still bringing the dongle back.

**Not exercised on this card:** `wifi list` and `wifi status` were not typed, and the dongle was not
replugged. Both were on the next card, R4b.

**A limit the fix inherits, recorded rather than fixed.** "Changed" means a different `(vid, pid)`, or
bound against not bound - the same test `main.rs` made since U1b. A dongle pulled and put back before
`wifi-usb` reads the first notice reads as unchanged, and is not brought up again. The R2 and R3b replugs
were seconds apart and never met it. Closing it needs the host to say WHICH insertion is bound (its USB
address, which changes on every insertion), in `OP_INFO`.

**Two sentences that are the shared loop's, and read wrong for a dongle.** `wifi radio off` logs "the
firmware refused DOWN" after the dongle's own line says the off is not built. And the shell answers a
`powercycle` with "the kernel refused: this machine has no control over the radio's power", which is the
`wire::NO_POWER_CONTROL` sentence; for the dongle the kernel was never asked. Both are true in outcome and
loose in cause. Left for the cards that build those verbs.

**R4b (2026-10-06, `build/kernel7-R4b.img`): passed.** `/wifi.keys loaded` and the auto-join appeared ONCE
at boot, and once more after a replug - one per bind, as predicted, where R4 gave three. `wifi status`:
radio on, not associated, the last scan's age and count. `wifi scan`: 16 networks in 2 s; after the replug,
22 in 3 s (a replug starts a new loop, so `wifi status` between the two said "last scan none": the cache is the loop's, and a new bind is a new radio). `wifi list` printed the cache, the saved network marked `saved`. After the replug the dongle came
back to `R3b done` with no STATUS-stage error, and the join stayed refused naming R5. `dir` worked, and
turned up something that is not WiFi's: two entries with one name in `/` (`backlog/75`).

R4 is done on the Pi 2. Owed elsewhere: the Pi 4 and the VisionFive check card for the shared loop (above).

## 11. R5a (2026-10-06): the first frame sent - a probe request on every channel the sweep tunes - hardware-verified on the Pi 2

R5 is the join: authentication, association and the WPA2 handshake, each of them frames this driver must
SEND. So it starts with sending one, and the smallest frame whose arrival can be seen from here is a probe
request: every access point that hears it and beacons answers with a probe response addressed to the
sender, and the receive path built in R3b already reads those. One wildcard probe request (any SSID, any
network) goes out on each channel as the sweep tunes it.

**The host half: `OP_BULK_OUT`** (`usbfn`, 0x24): `[op, out, transfer...]` -> `[op, status]`, where `out` is
the endpoint's POSITION among the radio's bulk OUT endpoints in its configuration descriptor - the order
Linux's `rtl8xxxu_parse_usb` fills `out_ep[]` in - so the driver maps queues to endpoints as
`rtl8xxxu_init_queue_priority` does without knowing an endpoint number. `dwc2` finds the OUT endpoints at bind
in the same walk as the IN (and keeps none that are not high-speed, as for the IN), stages the frame in its
arena at 0x5000, after the radio's receive area, and sends it on the bulk channel with the disk's own
transfer (`msc::bulk_xfer`, now shared), carrying each endpoint's data toggle forward. The radio's IN stands
aside for it like any bulk transfer. A transfer the chip keeps NAKing for 200 ms is reported not sent. The
heartbeat's `radio rx` line gains `tx - frames, bytes, failed`.

**The driver half**, every value read from `rtl8xxxu` (`core.c`, fetched 2026-10-06):

- **The station's address and link type** (`rtl8188::set_station`): `REG_MACID` to the efuse address a byte
  at a time (`rtl8xxxu_set_mac`) and port 0's link type to station (`rtl8xxxu_set_linktype`), which Linux
  does when the interface is added. Without it the chip drops a probe response addressed to us: `RCR`
  accepts unicast only when the address matches.
- **The descriptor** (`rtl_tx.rs`, host-tested): `rtl8xxxu_tx`'s common words and
  `rtl8xxxu_fill_txdesc_v1`'s for a management frame - own, first and last segment, broadcast for a
  group address; the management queue (0x12) and `AGG_BREAK`; the sequence number; the driver's rate, 1
  Mb/s; a retry limit of 6 - signed by `rtl8xxxu_calc_tx_desc_csum`'s XOR.
- **The endpoint**: the management queue is `out_ep[mgp]`, and `mgp` is 0 for one, two or three endpoints
  (for three it is `TRXDMA_QUEUE_HIGH ^ 3`, and that queue is 3), so position 0 always.
- **The frame** (`mgmt::probe_request`, host-tested): IEEE 802.11 9.3.3.10 - broadcast, the wildcard BSSID,
  the wildcard SSID, and the 2.4 GHz rates (1, 2, 5.5 and 11 basic; 6 to 54 in the two rate elements).

**Transmit power is the table's, not calibrated.** The baseband table R3a loads sets path A's TX AGC
(`0xE00`-`0xE1C`, and the CCK bytes in `0xE08` and `0x86C`) to fixed mid values; Linux replaces them with the
per-channel values in the efuse (`rtl8xxxu_gen1_set_tx_power`). That is enough for a probe across a room; it
is not what Linux transmits at, and the calibration is a later card.

**Prediction for the card** (the Pi 2, the dongle in):

1. At bind, `dwc2`'s configured line names the bulk OUT endpoints (expected two, high and normal, as R2's
   queue reading found), "sent on channel 0". After R3a, `the chip is a station at its efuse address`.
2. `wifi scan`: the sweep as in R4, and at its end `13 probe request(s) sent, 0 not`.
3. `wifi-usb: the FIRST probe response addressed to us - ...; R5a done`, during that sweep.
4. The heartbeat's `radio rx` line: `tx - 13 frames` (and more with each scan), `0 failed`.
5. As many networks as R4 found or more - a network that beacons rarely now answers within the dwell.

**Refuted by:** `a probe request was not sent` with the host's reason (the host half: no OUT endpoint, a
STALL, or a NAK past 200 ms - the chip not taking frames: its transmit DMA, pages or queue map); probes
counted as sent and no probe response to us ever (the frame is on the bus and not on the air - the
descriptor, the queue, the power - or on the air and not answered, which the access point's side would have
to show); or the keyboard or disk stalling during a sweep.

**Result (2026-10-06, `build/kernel7-R5a.img`): the dongle TRANSMITS - and the host said it did not.** `dwc2`
found two bulk OUT endpoints (2 and 3). Within 16 ms of the first sweep starting, `the FIRST probe response
addressed to us ... R5a done`; over two sweeps, 17 probe responses addressed to the dongle, none before the
first. The first sweep found 23 networks, the most yet. And every one of the 26 probe requests was reported
NOT sent: `a probe request was not sent - the device or the bus did not take it`, and `tx - 0 frames, 26
failed`.

Both cannot be true, and the air is the better witness: an access point addresses a probe response to a
station only after hearing that station's probe. So the frames went out and the host's ACCOUNTING was wrong.
`msc::bulk_xfer` measured a completed transfer as `len - HCTSIZ.XferSize` both ways. For an OUT in
buffer-DMA mode that field is no byte count. The disk's own notes had already found it reading 0 for
transfers whose data was right, and the disk never looked at an OUT's count, so nothing had failed on it
until now. Linux's `dwc2_get_actual_xfer_length` (`hcd_intr.c`) never reads it for an OUT: a non-split OUT
halted with transfer-complete moved `chan->xfer_len`, the length asked for. **Fixed (R5a2, hardware-verified):** `bulk_xfer` returns the asked length for a completed OUT. Both disk write paths look only at
whether a command succeeded, not at the count, so the disk's behaviour does not change.

Predicted on R5a2: `13 probe request(s) sent, 0 not` per sweep, `tx - 13 frames ... 0 failed` (26 after
two), and the probe responses to us as before.

**R5a2 (2026-10-06, `build/kernel7-R5a2.img`): passed.** Two sweeps, each `13 probe request(s) sent, 0 not`;
the heartbeat `tx - 26 frames 1924 bytes, 0 failed` - 74 bytes a frame, the 32-byte descriptor and the
42-byte probe request; 15 probe responses addressed to the dongle; 20 and 18 networks. The first transmit is
done: the frame is built right, the host sends it, it reaches the air, and the host now says so.

## 12. R5b (2026-10-06): authentication and association - hardware-verified on the Pi 2

The join up to the keys. R5c is the WPA2 four-way handshake, whose runner already exists and is every
radio's (`godspeed_wifi::supplicant`); R5b is what comes before it, and it is the first exchange with ONE
access point rather than a broadcast to all of them.

**The steps** (`services/wifi-usb/src/station.rs`, `Station::join`):

1. **Find.** A probe request that NAMES the network, on each channel, then 60 ms listening for a beacon
   or probe response that carries the name. The strongest is kept, because several access points may share
   a name. Its BSSID, its own channel (from its channel element), its capability and its RSN element are
   noted. Naming the network is also what finds a hidden one.
2. **Check what it takes.** The supplicant's message 2 repeats a fixed RSN element, `eapol::RSN_IE` (CCMP
   for both ciphers, PSK). So a WPA2 network whose group or pairwise cipher is not CCMP is refused before
   anything is sent (`mgmt::rsn_is_ccmp`), and so is a key offered to an open network or none to a WPA2 one.
3. **Tune there and set the BSSID**: `rtl8188::set_bssid`, `rtl8xxxu_set_bssid` for port 0, which mac80211
   has the driver do before authenticating.
4. **Authenticate.** Open System, transaction 1 out, transaction 2 back.
5. **Associate.** The capability is mac80211's (`net/mac80211/mlme.c`, read 2026-10-06): ESS, short
   preamble and short slot time on 2.4 GHz, and Privacy when the network's capability has it. Then the
   listen interval (10, CHOSEN: mac80211 takes it from the driver's configuration), the SSID, the rates and
   the RSN element.

Each answer is awaited 200 ms and each request tried 3 times, mac80211's `IEEE80211_AUTH_TIMEOUT` (`HZ /
5`) and `IEEE80211_AUTH_MAX_TRIES`, and the same for association. The frames and their two answers are
`mgmt.rs`'s, to IEEE 802.11-2020 9.3.3, host-tested.

**Then it leaves.** With no keys an association carries nothing, so reporting the join as done would be the
lie. It says `ASSOCIATED, association ID n; R5b done`, sends a deauthentication (reason 3, leaving), clears
the BSSID, goes back to channel 1, and reports the join failed. The shell prints its "not joined" line.

**The one place this service asks instead of being told.** A join is a run of exchanges inside one
`Station::join` call, and while it runs the serve loop is not receiving. So `NOTE_BULK_IN` cannot reach the
code waiting for an answer. `rx::wait_frames` asks the host for held transfers itself, 2 ms apart, only for
as long as one answer window or one find dwell. The notices sent meanwhile are queued and taken afterwards,
and each finds nothing held, which is harmless.

**Not done here, recorded.** Linux's post-association steps - the rate mask and the "connected" report to
the firmware (`update_rate_mask`, `report_connect`), `REG_BCN_PSR_RPT` with the association ID - happen
only after the association succeeds and serve the data path, so they are R5c's or R6's. No HT capabilities
are offered, so the association is 802.11g; an access point that admits only HT stations would refuse it,
and the status code would say so.

**Prediction for the card** (the Pi 2, the dongle in, a WPA2 network with its key stored):

1. **At boot, the auto-join runs it unasked.** `/wifi.keys` names a network, so:
   `join - '<name>' found at <bssid> on channel n, -NN dBm, WPA2 with CCMP`, then `AUTHENTICATED (Open
   System, status 0)`, then `ASSOCIATED, association ID n; R5b done`. After that the serve loop's line that
   the network last joined did not take us back.
2. `wifi join <that name>`: the same lines again, then the shell's "not joined".
3. `radio rx`: `tx` frames grow by the find's 13 probes plus about three frames (authentication,
   association, deauthentication), and `0 failed`.

**Refuted by:** `no access point answered a probe` for a network that is in range (the directed probe or the
find's receive); `no authentication answer` (the unicast frame not reaching it, or its answer not reaching
us: `REG_MACID`, `REG_BSSID`, or the chip not acknowledging); a refusal with a status code, which names the
reason itself (17 is the access point full, 18 rates, 40 to 46 the RSN element); or the prompt stalling
during a join longer than a few seconds.

**Result (2026-10-06, `build/kernel7-R5b.img`): passed, three times.** The auto-join at boot and two `wifi
join`s each found the network on channel 1 (-40 to -48 dBm, WPA2 with CCMP), were AUTHENTICATED (Open
System, status 0) about 50 ms after the request, and ASSOCIATED with association ID 1, then left. Each join
took about 1.2 s, most of it the find's thirteen 60 ms dwells.

**The association answer came late twice.** In two of the three joins it arrived about 220 ms after the
first request, just past the 200 ms window. So the second request went out, and its answer was the one taken.
In the other join it came 9 ms after the request. Either the access point is slow to answer some
associations, or the first answer was missed; the log cannot tell which. Three tries cover it as they cover
it for mac80211.

**Every frame is accounted for.** The heartbeat before the third join read `tx - 46 frames ... 0 failed`.
That is 17 for the boot join (13 probes, the authentication, two association requests, the
deauthentication), 13 for a `wifi scan` between the joins, and 16 for the second join, whose association
answered first time. The shell's `wifi leave` between joins answered `nothing to leave - not joined`, which
is true.

## 13. R5c (2026-10-06): the WPA2 four-way handshake - JOINED - hardware-verified on the Pi 2

R5b's join, and then what R5b left out: the keys. The handshake itself is not new code. It is
`godspeed_wifi::supplicant::Handshake`, the runner the Pi 4's Broadcom and the VisionFive's AIC8800 already
join through. A radio supplies it a `KeyPath`: how an EAPOL frame leaves the station, and how a key reaches
the chip. The dongle's station is that `KeyPath` now.

**Frames in.** After the association, `rx::wait_frames` (R5b's paced ask, bounded here by 8 s) reads each
unprotected data frame from the access point. `godspeed_wifi::data::llc_payload` takes off the 802.11
header and the LLC/SNAP, and an EAPOL frame (ethertype `888e`) to us goes to the supplicant as the ethernet
frame it expects (`data::to_ethernet`). The chip passes data frames up because R3a set `REG_RXFLTMAP2` to
`0xFFFF`, as `rtl8xxxu_start` does, and they are addressed to the address R5a set. A deauthentication or
disassociation from the access point ends the wait. If message 2 had been sent, the supplicant's reading
applies: our key is not its key.

**Frames out.** The supplicant's message 2 and message 4, as ethernet, become 802.11 data frames to the
access point (`data::to_80211`: non-QoS, To DS, LLC/SNAP). They go on the best-effort queue's endpoint, which
is `out_ep[bep]` in `rtl8xxxu_init_queue_priority`: position 1 for this dongle's two queues. **A deliberate
difference from Linux:** they go at the driver's rate, 1 Mb/s, not the firmware's choice, because the rate
mask Linux hands the firmware after associating (`update_rate_mask`) is not sent yet (`rtl_tx::eapol`).

**Keys.** `rtl8188::install_key` follows `rtl8xxxu_set_key` and `rtl8xxxu_cam_write`. It sets
`CR_SECURITY_ENABLE` and `REG_SECURITY_CFG`'s six enables, then writes six CAM words for the entry, highest
first, each committed through `REG_CAM_CMD` with 100 us after it. The control word holds the CCMP cipher
(the suite's low nibble, 4), the key id, the valid bit, and the group flag for a group key. The pairwise
key goes into entry 0 against the access point's address and the group key into entry 1 against the BSSID:
first free, as Linux allocates them on a fresh join. Leaving empties them (`DISABLE_KEY`'s zero control
word) and zeroes the kept keys.

**What JOINED means here, and what it does not.** The association stays up and `wifi status` shows it: the
BSSID, the channel, and the signal as the find heard it, not a fresh reading. Nothing yet sends or receives
data through it (R6), and nothing answers the access point's group rekey (R7). So an access point that
rekeys on a timer will eventually see no answer and may drop the station. A dropped link is not noticed
until R6.

**`who`, done.** The supplicant and `eapol` logged as `wifi-driver:`, recorded as debt at R4 for exactly
this card. `Handshake::new`, `group_rekey` and `eapol::describe` now take the service's name.
`wifi-driver`'s callers pass `"wifi-driver"`, so its lines read as before.

**One-way, kept.** The 802.11-to-ethernet conversion was the AIC8800's (`aic_wire.rs`). It is 802.11, so it
moved to `sdk/wifi/src/data.rs` with its test rather than being written a second time, and the AIC8800
imports it from there. The new direction, `to_80211`, is beside it, host-tested.

**Prediction for the card** (the Pi 2, the dongle in, the key stored):

1. **At boot, the auto-join** - R5b's lines, `ASSOCIATED, association ID n`, then the supplicant's, each
   opening `wifi-usb:`: `EAPOL-Key from <the access point> - message 1`, `message 2 of 4 sent`, `EAPOL-Key
   ... message 3`, `message 3 verified ...; message 4 sent`, then `pairwise key 0 in CAM entry 0`, `group key
   n in CAM entry 1`, `JOINED - handshake complete ...` and `join - JOINED; R5c done`.
2. `wifi status`: joined to the network, its BSSID and channel 1.
3. `wifi leave`: left, and `wifi join` joins again with the stored key.

**Refuted by:** `the handshake reached no verdict` with no handshake frames, meaning message 1 never reached
us: the data frame filter, or the access point waiting for something an association without WMM or HT does
not give it. Message 1 repeated until `INCORRECT PASSPHRASE` with the right key means message 2 does not
reach the access point, or reaches it wrong: the data path out, its endpoint, its LLC/SNAP. `message 3's MIC
does not verify` means the key derivation, which is shared and verified on two other radios, so more likely
the addresses fed into it. `the key did not go into the CAM` means the CAM write.

**Result (2026-10-06, `build/kernel7-R5c.img`): JOINED, twice.** At boot the auto-join went: associated
(ID 1), message 1, message 2 sent, message 3 verified with 56 bytes of key data unwrapped, message 4 sent,
the pairwise key into CAM entry 0 and group key 1 into entry 1, `JOINED - handshake complete`. Then `wifi
leave` (`left <the network>`) and `wifi join` joined again, keeping the key in credential slot 0, and
`wifi status` read `joined 15 s ago`. The scan list marks the network `joined`, against both access points
that carry its name.

**The first handshake after boot waited 650 ms for its nonce.** Between message 1 and message 2 the
supplicant logged `NO HARDWARE RNG on this board` and used its counter-hashed fallback. The access point
resent message 1 in the gap, and the handshake completed on the resend. The second join had no such line and
answered in 31 ms. The cause is in the kernel, not the radio: the Pi 2's `hw_random` turns the RNG on at
its first call and then waits an iteration count shorter than the RNG's warm-up, so the first read after
boot always comes back empty. Its comment also says its output is "not fed to crypto", which the shared
supplicant now does. That is `backlog/76`, a kernel change left for the operator's go-ahead.

## 14. R6 (2026-10-06): data both ways - DHCP and ping over the dongle - hardware-verified on the Pi 2

R5c left a joined link that carried nothing. R6 makes it carry the stack's frames. It has three parts, one in
each service the frames pass through.

**`nic-driver` (the Pi 2's backend).** The Pi 4 and the VisionFive already carry `net-stack`'s frames over
their radios when the cable is out (`services/nic-driver/src/radio.rs`): the cable always wins, the radio is
asked over the frame ops (`OP_NET_INFO`, `_TX`, `_RX`), and STATUS names the carrier. The Pi 2's backend,
`kernel_net_main`, had no radio. Now it includes `radio.rs` by path from inside that function, as GENET and
`dwmac` include it from theirs, so no board fact is added. It re-reads the USB ethernet's own link bit every
500 ms and on every STATUS, and routes each op to the cable or to `wifi-usb`. It answers op 10, the
access point the radio is joined to, which `net-stack` asks only once STATUS has said the radio carries the
link. `radio.rs` gained one thing: the radio's service by name (`Radio::new`), `wifi-driver` beside GENET and
`dwmac`, `wifi-usb` here. The supervisor wires `wifi-usb` to `nic-driver` where it is embedded, pinned in
`COMMANDMENTS.baseline.toml` with that reason.

**`wifi-usb`, out.** A frame from the stack becomes an 802.11 data frame to the access point, PROTECTED: the
CCMP header goes after the MAC header with a fresh packet number and key id 0, where mac80211's
`ccmp_encrypt_skb` puts it for a key the hardware holds (`net/mac80211/wpa.c`, fetched 2026-10-06:
`GENERATE_IV` writes the header, `tail = 0` leaves the MIC to the hardware). The descriptor adds
`TXDESC_SEC_AES` (`rtl_tx::protected`), and the chip encrypts with the pairwise key R5c put in its CAM. On
an open network the frame goes plain. It is still at the driver's 1 Mb/s (R5c's reason).

**`wifi-usb`, in.** A data frame from the joined access point, to us or to a group, that the chip decrypted
(`security` AES and `swdec` clear in the descriptor, `rtl8xxxu_parse_rxdesc16`'s `RX_FLAG_DECRYPTED`)
becomes the ethernet frame inside it. `llc_payload` steps over the CCMP header, and the 8-byte MIC the chip
appends (`RCR_APPEND_MIC`) is trimmed, as mac80211 trims it for a frame marked decrypted but not
`MIC_STRIPPED`, which `rtl8xxxu` never sets. It waits in a queue the receive side and the station share
(`rx::Link`, a `RefCell` `main.rs` owns), and the station's `pull` hands it to the serve loop's frame path.
A key frame on the joined link is the access point's group rekey: counted and said, not answered (R7).

**Replay protection, added after a review of the R6 commit.** The chip decrypts, but it does not check the
CCMP packet number, and `rtl8xxxu` never marks a frame `RX_FLAG_PN_VALIDATED`, so mac80211 checks it in
software. Without that check, an old frame sent again would be decrypted and handed up as new.
`data::ccmp_pn` (host-tested) reads the packet number and key id out of the CCMP header, and `rx::Link`
keeps the highest accepted under the pairwise key and under each group key id, all reset at each join.
A frame whose number does not climb is dropped, counted and said. A frame that failed its ICV never gets
this far: the receive side drops it first.

**And the group key's counter does not start at zero (a second review).** A group key was in use before
this station joined, so a broadcast frame sent under it earlier could have been replayed to us once while
its counter stood at 0. Message 3 carries the group key's Key RSC, the packet number it has reached.
`eapol::Key::rsc` reads it, little-endian unlike the rest of that header, because mac80211 loads it as
`rx_pn[j] = seq[5 - j]` (`net/mac80211/key.c`). The supplicant now hands it to the radio after installing
a group key, in message 3 and in a group rekey, through a new `KeyPath::group_rsc`. That method does
nothing by default, which is right for the Broadcom and the AIC8800, whose firmware checks replay itself.
The dongle seeds that key id's counter from it. The counters are zeroed when a join STARTS, so the seeding
is not undone when the join completes. The log says `group key n accepts packet numbers above N`.

**And a group key is never reinstalled (a third review).** That makes the counter seeding the group half of
KRACK: a group-key message carrying a key the station already holds would, reinstalled, set the counter back.
The dongle keeps each slot's installed group key for exactly this comparison. The same key again is not
written to the CAM, and its counter is only ever raised (`Link::group_rsc_at_least`), as wpa_supplicant
skips such a reinstall. The kept keys are zeroed when the station leaves. No frame can be taken between an
install and its seeding, because both run in one supplicant call on one thread.

**A comment that was wrong, corrected.** `rtl_rx::Desc::pkt_len` said "FCS included". The chip's `RCR`
appends the PHY status, the ICV and the MIC (bits 28 to 30, as Linux sets them), not the FCS (bit 31), so the
length has no FCS in it. Nothing used the claim, and the MIC trim depends on the truth of it.

**Prediction for the card** (the Pi 2, the dongle in, the network's key stored, NO ethernet cable):

1. At boot: the auto-join to JOINED (R5c), then `nic-driver: the cable is out - the radio carries the link`
   with the dongle's MAC.
2. `wifi-usb: the FIRST data frame sent through the link - ... encrypted by the chip (CCMP); R6 transmit
   works`, then `the FIRST data frame through the link - ethertype ...; R6 receive works`.
3. `net-stack` gets a DHCP lease over the radio: `net` shows an address, a gateway and DNS.
4. `ping 8.8.8.8` gets replies. They will be slower than the cable's, because every frame goes at 1 Mb/s.

**Refuted by:**
- Frames sent and none received, or `did not decrypt`: the receive side, or the keys' CAM entries -
  especially the group key's for broadcast DHCP.
- DHCP offers received but no lease: the stack's side of a carrier switch.
- Replies that arrive but fail their checksums or come up 8 bytes short: the MIC trim, which would mean
  the chip does not append the MIC for CCMP after all.
- `nic-driver` never switching to the radio: the cable read, or the STATUS answer.

**Result (2026-10-06, `build/kernel7-R6.img` with replay protection): DHCP and ping over the dongle.** The
auto-join reached JOINED, and 50 ms later `nic-driver: the cable is out - the radio carries the link`. Then
`the FIRST data frame sent ... encrypted by the chip (CCMP)` and, 15 ms after it, `the FIRST data frame
through the link - ethertype 0x0800, 320 bytes, decrypted by the chip`: the DHCP offer. The lease was ACKed
8 ms after the offer; `net` read `link up via wifi (the cable is out)`, an address, the gateway and
`lease ok (DHCP)`. `ping 8.8.8.8`: four sent, four received, 18 to 32 ms. No replay was flagged, no frame
was left undecrypted, and the dongle's transmit count read 28 frames with 0 failed.

**Smaller things, recorded.**
- At boot `nic-driver` asked the radio twice before `wifi-usb` had brought the chip up, got no answer
  within 100 ms, and backed off as `radio.rs` is built to.
- Right after the join, one 286-byte frame came back `the radio did not send` while the dongle counted no
  failure. That is `nic-driver`'s 100 ms bound passing while `wifi-usb` was busy, not a transmit that
  failed. DHCP completed regardless.

**The link then dropped, and the cause was `wifi radio off`.** The serve loop every radio shares leaves the
network first (disassociate, forget the keys) and only then asks the radio to go down. The dongle refused,
because it could not yet, and the shell said `the radio did not take the power command`. But the station had
already left, so the next `net` read `the radio is not joined`. Nothing broke. The sentence was the problem:
it said "nothing happened" about an off that had happened halfway.

## 15. R6b (2026-10-06): `wifi radio off` and `on` for the dongle - hardware-verified on the Pi 2

So the off now completes. `rtl8188::radio_off` is what `rtl8xxxu_stop` does to the chip: transmit paused,
the management and data receive filters closed, then `rtl8xxxu_gen1_disable_rf` for its one path (the RF
parameter word's path bits, the transmit paths, the power-saving bit, the CCA wait, RF register 0 to zero,
the regulator bits). `radio_on` is what `rtl8xxxu_start` does after it: `enable_rf` (R3a's, which also
unpauses transmit), the filters open, and the channel the dongle rests on. `is_up` reports which. The
host's bulk IN stays armed; with the filters closed the chip hands it nothing.

**Prediction:** after a join and a lease as in R6, `wifi radio off` logs `radio off - transmit paused,
receive filters closed, the RF module down` and the shell says the radio is off. `wifi status` shows the
radio off. `wifi radio on` logs `radio on - RF up, receive filters open`, and the serve loop rejoins the
network last joined (R5c's lines again), `nic-driver` switches back to the radio, `net-stack` takes a
lease, and `ping` answers. **Refuted by:** `radio off did not complete`, a rejoin that never reaches JOINED
after `on` (the RF not back, or the receive filters still closed), or no lease after a JOINED (the link
switch back).

**Result (2026-10-06, `build/kernel7-R6b.img` as first built): passed.** Boot to JOINED, a lease, `ping` 4 of
4. `wifi radio off`: `radio off - transmit paused, receive filters closed, the RF module down`, and the shell
said `left the network, then radio off - verified`. `ping` then lost everything and `net` read `link down`,
both correct. `wifi status` read the radio off. `wifi radio on`: `radio on - RF up, receive filters open`,
`rejoining the network last joined`, JOINED 1.3 s later, `nic-driver` back on the radio, `lease ok`, and
`ping` 5 of 5. The shell calls the off "soft - the firmware's switch; the chip stays powered", which is
true here as well: the dongle's power is its USB port's.

## 16. R7 (2026-10-06): the access point's group rekey, answered - built, NOT SEEN (needs an hour joined)

An access point changes its group key on a timer, often hourly, by a two-message group key handshake on
the live link. A station that does not answer it is dropped. Until now the dongle counted the frame and left
it.

**In.** A key frame on the joined link waits in one slot in `rx::Link`, as ethernet with the MIC trimmed.
The trim now happens before the EAPOL check, because a key frame's own MIC covers its exact length. The
station's `pull` hands it to `supplicant::group_rekey`, the runner the AIC8800's `pull` uses. The serve loop
pulls at least every 250 ms while joined, frames read or not, so the rekey is answered within that.

**Out, protected.** The acknowledgement is an EAPOL frame on a link that has keys, so it goes protected, as
any data frame does. `send_eapol` now sends protected once the pairwise key is in the CAM (`ptk_in`, set by
the install itself, so it follows what the chip holds). The four-way handshake's own frames still go plain,
being sent before that.

**The CAM stays bounded.** A new group key in a slot that already holds one overwrites that slot's CAM entry
instead of taking a new one, so an hourly rekey cannot fill the CAM. The same key again is refused as the
KRACK guard above refuses it. Its counter starts at the new key's RSC.

**Not answered, and said:** the access point restarting the four-way handshake on the live link (a
pairwise rekey; hostapd does not do it by default). The log says so, and the link will drop.

**Prediction:** join, take a lease, and leave the Pi up past the access point's rekey interval (an hour
covers the common setting). When it comes: `group key n accepts packet numbers above N`, then `the access
point's group rekey answered - ...; R7 done`, and `ping` still answering afterwards. **Refuted by:**
`group-key frame whose MIC does not verify` (the trim, or the frame), `could not be sent` (the protected
send), or the link dropping at the rekey (the acknowledgement not reaching the access point). If nothing
happens in two hours, the access point may not rekey at all, which the log cannot distinguish from
silence - a `wifi status` showing the join still up is then the result.

**The R7 card's run (2026-10-06, a few minutes): no regression, and the rekey not seen.** The operator will
not hold a board up for an hour to wait for an access point's timer, so R7 stays "built, not seen" until a
run happens to last that long. What the run did show:
- Boot to JOINED, with `group key 1 accepts packet numbers above 0 (its Key RSC)`. A fresh group key, so its
  RSC is 0.
- The lease, the gateway's ARP and an ICMP reply.
- `wifi join` again later: JOINED, and `ping` 5 of 5.

**A second misleading off, the same shape as R6's, found and fixed.** `wifi radio off hard` left the network
first and then tried to cut the chip's power, which the dongle cannot. The shell said `the radio is as it
was` while the station had left; the following pings failed and `wifi radio on` answered `already on`. Not
a dongle fault: the order is the shared serve loop's. It now asks `Host::can_cut_power` BEFORE leaving, and a
host that cannot (the default, so the dongle) is answered "no power control" with nothing changed, still on
the network. `wifi-driver`'s SDIO host says it can, so the Pi 4's and the VisionFive's hard off is
unchanged.

## 17. R8 (2026-10-06): the data rate is the firmware's - the rate mask taken; the speed-up not yet measured

Since R6 every data frame went at the driver's rate, 1 Mb/s, the rate the management frames use. Linux
does not send data that way. Once associated, `rtl8xxxu_bss_info_changed` gives the chip's firmware the
access point's rates as a mask (`update_rate_mask`, the `H2C_SET_RATE_MASK` command), and
`fill_txdesc_v1` leaves a data frame's rate to the firmware, which adapts it within that mask.

**What R8 does**, in Linux's order (`rtl8188::joined`, called once the handshake has the link JOINED):
- the rate mask through the host-to-firmware mailbox (`h2c`). Four boxes taken in turn, each waited on
  until the firmware has read it (`REG_HMTFR`), the extension bytes written before the box;
- `REG_BCN_MAX_ERR`;
- the port's beacon transmission stopped;
- `REG_BCN_PSR_RPT` with the association ID;
- the connect report.

Leaving sends the disconnect report.

**The mask** comes from the Supported Rates and Extended Supported Rates elements of the access point's
probe response, which the find already reads (`rtl_tx::rate_mask`, host-tested). There is one bit per rate
in `rtl8xxxu_legacy_ratetable`'s order: 1, 2, 5.5 and 11 Mb/s in bits 0-3, and 6 to 54 Mb/s in bits 4-11.
A typical 802.11g access point gives `0xfff`.

**The descriptor.** Data frames now take `rtl_tx::data`: no `USE_DRIVER_RATE`, `txdw5` = `0x0001ff00`, as
`fill_txdesc_v1` writes for data. The handshake's four frames still go at 1 Mb/s, being sent before the
mask (`eapol`, its comment records why). No 802.11n rates: the station offers no HT element, so the
association is legacy, and HT is a later card.

**Also on this image: the R7b fix** (section 16): `wifi radio off hard` on the dongle answers that there is
no power control and stays on the network.

**Prediction:** at JOINED, `the firmware has the rate mask 0x... and the association; it picks the data rate
from here`, with a mask of `0xff0` or more. Then the lease, then `ping` against the gateway with small and
large payloads.
- The difference is in the uplink. At 1 Mb/s a 1024-byte request is about 8.5 ms in the air before any
  retry. Within the mask it is a fraction of a millisecond.
- So the large ping's round trip should drop by several milliseconds compared with an R7 run, and stay
  within a couple of milliseconds of the small one. The same pings on the R7 image are the baseline if a
  comparison is needed.
- `wifi radio off hard` answers "no power control", and `wifi status` still shows the join.

**Refuted by:**
- `the firmware's mailbox stayed busy` (the firmware not reading its mailbox);
- `the firmware was not given the rates`;
- the lease or the pings failing where R6 succeeded (a data descriptor the chip refuses or sends badly);
- no change in the large ping's time (the firmware not adapting, or the mask not taking).

**Closed on the R9 card's run (2026-10-06): the uplink is not 1 Mb/s.** `ping` to the gateway: 32 bytes,
8 of 8, 4 to 16 ms, average 10; then 1024 bytes, 7 of 7, 8 to 20 ms, average 13. The large ping costs about
4 ms more on the minimum and about 3 ms more on the average. At 1 Mb/s its request alone is 8.5 ms in the
air, so the firmware is sending faster than 1 Mb/s. How much faster this cannot say: the remaining
difference includes the larger copies through USB and three services. The record of the R8 run follows.

**The R8 card's run (2026-10-06): nothing refuted, and the speed-up itself not measured.**
- At JOINED, after boot and again after `radio on`: `the firmware has the rate mask 0xfff and the
  association`. The mailbox took both commands; the access point lists all twelve legacy rates.
- The lease, the gateway and the internet over the dongle: `ping` 8.8.8.8 3 of 3, then 3 of 3 after the
  rejoin. `ping bytes 1024` to the gateway: 7 of 7, 9 to 18 ms, average 14 ms. No frame failed: `dwc2`
  reported `tx - 62 frames, 0 failed`.
- `wifi radio off hard`: `nothing changed, still on the network`, and `wifi status` showed the join still
  up. The R7b fix holds.

**What it does not show.** No small ping to the gateway was run on this image, and no large one on R7. So
there is no pair to subtract, and 14 ms on its own says nothing about the uplink rate: the floor of this
path (USB, three services, the access point) has not been measured. The next run needs `ping 192.168.10.1`
and `ping bytes 1024 192.168.10.1` back to back. A difference of about 8 ms or more is the 1 Mb/s uplink
still in force; a difference of a millisecond or two is the firmware choosing a higher rate.

**Seen on the way, not R8's.** At the first DHCP exchange `nic-driver` logged `wifi-usb answered 0x10
while we asked 0x11 - not our reply (1 mismatched ...)`. The bridge's own request timed out ("2 slow") and
the late answer arrived after it had moved on. The bridge's check caught it, the lease completed 50 ms
later, and it did not recur. It came during the join's busiest second: the keys file being written, an
op that took 1636 ms to serve. Recorded, not chased: the check did its job, and a second sighting would
make it a card.

**The shell's words for hard off, corrected.** It announced "cutting the chip's power - leaving the network
first" before asking, and answered the dongle's refusal with "the kernel refused". Neither is true for a
driver that changes nothing. The shell now asks rather than announces. On `NO_POWER_CONTROL` it reads byte
1 (whether the driver left first): left means the kernel refused a host that could cut; not left means the
driver has no power to cut, and nothing changed. `powercycle`'s refusal no longer blames the kernel alone.

## 18. R9 (2026-10-06): `off hard` and `powercycle` for the dongle - the chip's own power-down - hardware-verified on the Pi 2 (with section 19)

**The question that led here.** A dongle's 5 V is its USB port's, and on the Pi 2 that port is behind the
hub the disk, the keyboard and the ethernet share. Cutting it, or even suspending the port, puts the shared
path at risk to idle one device, so neither is done. Until R9 `off hard` on the dongle refused, and the one
cure for a hung dongle firmware was to unplug it.

**What unplugging does that software can.** Every register access is a USB control transfer to the
dongle's own endpoint 0, answered by the chip's USB block, not its 8051. A hung firmware cannot stop
them. Linux uses that on unplug: `rtl8xxxu_disconnect` calls `rtl8192cu_power_off` (`8192c.c`, read
2026-10-06). R9 ports it whole (`rtl8188::power_off`), in its order:
- **The RF and baseband:** transmit paused, the RF's mode bits zeroed, APSD off, the RF clock gated, the
  baseband reset.
- **The firmware:** a firmware marked ready is asked to stop, and given `FW_STOP_MS` (10 ms; Linux's 100
  reads 50 us apart, as a time). **If it does not answer, its CPU is stopped from the host.** The log says
  which (`FwStop`).
- **The pins and the analog side:** the pins quiet, the regulator to its low setting, the chip suspended
  for the host, the ISO, clock and power registers locked.
- **The RTL8188RU's LNA workaround,** by the efuse's `rf_regulatory` bit, and the extra regulator bit
  for a UMC chip of cut B, from `SYS_CFG`.

**Through the serve loop's `Host`** (`rx.rs`), so nothing in `godspeed_wifi::serve` changes:
- `can_cut_power` is true once a bring-up has made a station.
- `cut_power` is the power-off.
- `verify_off` reads `MCU_FW_DL`: no firmware marked running is `OFF_VERIFIED`.
- `restore_power` has nothing to do in place.
- `power_cycle` is the power-off held for the shell's 2 s.

From there the shell's existing path takes over, unchanged: `on` and `powercycle` restart the driver,
whose bring-up powers the chip on and uploads its firmware. The shell's words for the verified,
contradicted and power-cycled cases no longer name SDIO alone.

**What it cannot recover:** a dongle whose USB block has stopped answering. The power-off fails at its
first transfer, the driver says `the dongle is not answering on USB; only unplugging it recovers that`,
and the shell reports the refusal.

**Also on this image: R8's missing pair.** The rate card still needs the small and the large ping to the
gateway back to back.

**Prediction, Pi 2, cable out, a few minutes:**
1. `ping 192.168.10.1` then `ping bytes 1024 192.168.10.1`: the difference closes R8 (section 17).
2. `wifi radio powercycle`. The shell prints `radio powered down for 2.0 s (the network is left) -
   restarting the driver on the cold chip`. The log shows:
   - `the chip powered down ... - the firmware stopped its CPU when asked` (a healthy firmware should
     answer);
   - `held powered down for 2000 ms`;
   - the new instance's bring-up from the efuse on: the MAC read cold or warm (either is a result, and it
     is recorded), the firmware uploaded, R3a done;
   - JOINED, and the shell's `powercycle succeeded - joined ...`;
   - `ping` answering.
3. `wifi radio off hard`. The log shows `the chip powered down` and `checked - no firmware is marked
   running`, and the shell `left the network, then radio off (hard) - verified by its driver ...`. `wifi
   status` shows the powered-down state. Then `wifi radio on`: `radio powered up - starting the driver on
   the cold chip`, the rejoin, and `ping`.

**Refuted by:**
- `the power-off stopped` with the dongle otherwise answering (a step Linux takes that this chip refuses);
- `Forced` on a firmware that was working (the stop request wrong; the power-off still completes);
- a bring-up after it that stops, at the efuse or `power_on` step 1 (the chip not coming back from Linux's
  power-down the way it does under Linux);
- `dwc2` losing the dongle while it is suspended (its bulk IN erroring into an unbind);
- `checked - a firmware is STILL marked running`.

**The R9 card's run (2026-10-06).** R8's pair first: closed, in section 17. Then `wifi radio powercycle`:
- `the chip powered down (Linux's rtl8192cu_power_off) - the firmware stopped its CPU when asked`, then
  `held powered down for 2000 ms`. The shell printed `radio powered down for 2.0 s (the network is left)`
  and restarted the driver.
- The new instance found the chip **cold**, powered it on, uploaded the firmware, and the firmware ran
  (`MCU_FW_DL=0x000300c6`). **The chip comes back from Linux's power-down exactly as from a plug-in.**
- The bring-up then stopped at `the host answered something other than CONTROL`, and on the shell's
  restart after that, every request the sweep made was `malformed`. Not the chip: section 19.
- `off hard` was not reached.

## 19. A respawned `wifi-usb` and the host's notices (2026-10-06) - hardware-verified on the Pi 2

**What went wrong after R9's power cycle,** and would have after any restart of `wifi-usb` - a crash, a
`kill`, a `chaos` round. Boot gives `wifi-usb` a reply mailbox, an endpoint that carries only replies.
A respawn gets none: `spawn[ipc]: 'wifi-usb' gets no reply mailbox - 71 of 96 routing slots free, reserve
72`, logged at both restarts. `nic-driver` and `net-stack` have none from boot. Without one, `wifi-usb`
awaits its replies on the endpoint where `dwc2` also sends its notices (`NOTE_BULK_IN`, `NOTE_RADIO`).
The kernel matches a call's reply by SENDER (`dequeue_reply_locked`), not by request. So a notice from
`dwc2` can be taken as the reply, and the real reply then answers the next request: every answer one
behind. That is exactly the log: `answered something other than CONTROL`, then `malformed` on every
request after it.

**The cure, with no kernel change.** `usbfn::OP_SYNC`, a request the host never answers:
- **The driver** (`main.rs` `host`): when a request's answer is a notice, it sends `[OP_SYNC, notice]`.
  The kernel returns that call with the host's next message, which is the real answer still on its way.
  Since nothing answers `OP_SYNC`, no reply is left owed behind it. Bounded at 4 notices for one answer.
- **The host** (`dwc2` `rtl.rs`): it gives back `OP_SYNC`'s reply capability and owes the named notice.
  It sends the notice again only once the driver has asked nothing for 5 ms (`DRIVER_QUIET_MS`), so the
  notice reaches the serve loop, not the next request of a sequence. A swallowed `NOTE_BULK_IN` must be
  re-sent: `dwc2` holds the transfer until it is collected and arms nothing meanwhile, so a lost one would
  stop receive. Counted in the heartbeat as `taken as answers (OP_SYNC)`.

**Not done, and recorded.**
- A respawn's missing mailbox is the kernel's reserve policy (`try_register_optional`), and changing the
  reserve is a kernel change with its own measurement. With `OP_SYNC` the driver no longer depends on it.
- `nic-driver` asks `wifi-usb` on an endpoint that also carries its own clients. Its mismatched answers
  (section 17) are the same mechanism one layer up, and its existing check catches them.

**Prediction, the R9 card again:**
- `wifi radio powercycle` ends `powercycle succeeded - joined ...`.
- The new instance's bring-up runs to JOINED. `dwc2`'s heartbeat may show `N taken as answers (OP_SYNC)`
  above 0, which is the cure working. No `malformed` and no `something other than` lines.
- `ping` answers.
- Then `wifi radio off hard`, `wifi status`, `wifi radio on` and `ping`, as section 18 predicts.

**Refuted by:**
- `the host's notices kept arriving in place of its answer`;
- receive stalling after a restart (a swallowed `NOTE_BULK_IN` never re-sent): no data frames, and
  `dwc2` reporting `held for wifi-usb` at every heartbeat;
- any `malformed` from the host.

**The run (2026-10-06, the operator's: the R9 card and more).** Passed throughout:
- **`wifi radio off hard`:** `the firmware stopped its CPU when asked`, then `left the network, then radio
  off (hard) - verified by its driver`. `wifi radio on`: `radio on succeeded - joined ...`.
- **`wifi radio powercycle`, five times.** Every one powered the chip down with the firmware stopping when
  asked. Four ended `powercycle succeeded - joined ...`. The fifth met the operator's unplug during its 2 s
  hold (`port 4 - device REMOVED`). The new instance said `no dongle bound` and the shell `the driver found
  no working radio on its bus`. On the replug (port 5) the chip came up cold, rejoined, and `ping` answered.
- **`chaos max-carnage`, 50 rounds:** 343 kills, kernel alive. `wifi-usb` was restarted 19 times in the
  run. After chaos the driver came back and recovered one transient USB error during the firmware upload
  (`dwc2` re-ran several STATUS stages as R2c does, one gave up after 3 errors, and the upload's own
  retry took it: `try 1 of 6`, then `2 tries`; this was a restart, so R2c's replug case is still unseen). It rejoined, and `ping` went 4 of 4.
- **Unplug and replug** once more by hand: the dongle came back on port 4 (one failed enumeration first,
  retried), rejoined, and `ping`, `wifi scan` and `wifi list` worked.
- **No** `kept arriving`, no `malformed`, no `something other than`.

**The cure's cost, seen in the counters.** On a respawned instance nearly every received transfer is
recovered this way: `1044 transfers ... 987 taken as answers (OP_SYNC)`. Each costs an extra call to the
host and a re-sent notice after 5 ms of quiet. Beacons and pings do not show it; sustained receive would.
The instance with a mailbox showed 0. The real fix is the mailbox for a respawn, a kernel change, recorded
in `backlog/74` with this evidence.

**Also seen:** `dwc2` counted 309 receive errors (`HCINT=0x92`) across the powered-down windows: its bulk
IN polling a suspended chip. They stopped when the chip came back, and no transfer was lost to them.

## 20. A respawn takes its reply mailbox back - a kernel change (2026-10-06) - hardware-verified on the Pi 2

**With the operator's go-ahead, the fix section 19 pointed at.** When a watched task, or the supervisor, dies
holding a reply mailbox, the kernel banks a credit; such a spawn the reserve would refuse may spend one.
Credits are pooled, not tied to the task that banked them
(`routing::MAILBOX_CREDITS`, `backlog/74` option 4). A respawned `wifi-usb` therefore gets back the mailbox
its boot instance had. Its replies come only there, and `dwc2`'s notices can no longer be taken as answers.

**`OP_SYNC` stays.** It is now the fallback for a respawn that still finds no credit (a table that has
genuinely filled), and it costs nothing while unused.

**Verified in QEMU (Pi 2):** `kill time` logged `spawn[ipc]: 'time' takes back a reply mailbox a dead
watched task released - 71 of 96 routing slots free, reserve 72`. At the same count the old rule refused.

**Prediction, the card, Pi 2:**
- `wifi radio powercycle` twice.
- Each restart of `wifi-usb` logs `takes back a reply mailbox` where it logged `gets no reply mailbox`.
- `dwc2`'s heartbeats after it read `0 taken as answers (OP_SYNC)`, where the R9 run read 987 of 1044.
- `powercycle succeeded`, and `ping` answers.
- A `chaos max-carnage` run, as before, shows no panic and the dongle rejoining. Every respawn of a service
  that had a mailbox at boot should take it back.

**Refuted by:**
- `gets no reply mailbox` for `wifi-usb` after a powercycle;
- a non-zero `taken as answers` count on a restarted instance;
- any service refused its MANDATORY endpoint (`spawn REFUSED - IPC routing table full`) during chaos, which
  would mean the credits spent slots the reserve was holding for it.

**The card's run (2026-10-06): as predicted.**
- Every restart of `wifi-usb` (`off hard` then `on`, a `powercycle`, and 7 in chaos) logged `takes back a
  reply mailbox`.
- `dwc2` read `0 taken as answers (OP_SYNC)` at every heartbeat.
- `radio on succeeded` and `powercycle succeeded`.
- `chaos max-carnage` 50 rounds, 351 kills: kernel alive, no mandatory endpoint refused, 167 mailboxes
  taken back across all services. After it the dongle rejoined and `ping` went 3 of 3.
- The pings that failed were sent with the radio off, soft and hard, and were answered `link not
  confirmed`. One `Request timed out` was the first echo after a power cycle, while ARP resolved again.

`backlog/74` has what the run showed about pooling and about the supervisor.

## 21. R11 (2026-10-06): transmit power from the dongle's own calibration - hardware-verified on the Pi 2

Until now the transmit gain was what the baseband table wrote: the same for every dongle and every
channel. The factory writes per-channel-group power indexes into each dongle's efuse (`struct
rtl8192cu_efuse`, from 0x5a: CCK and HT40 one-stream indexes per path, and signed differences for OFDM,
HT20 and two streams). Linux turns them into the gain words on every tune (`rtl8xxxu_gen1_set_tx_power`).

R11 does the same:
- **`rtl_power.rs`** (new, host-tested) holds the arithmetic, to the byte:
  - the channel group (1-3, 4-9, 10-13);
  - the signed nibbles;
  - the ceilings (0x3f, and 0x20 for an 8188RU's CCK);
  - the power base table (the 8188RU has its own);
  - the gain words, added as whole 32-bit values as C adds them;
  - the IQ-imbalance bytes stepped down from the last word.
- **`rtl8188::set_tx_power`** writes them: the CCK indexes by read-modify-write, the rest whole.
  `set_channel` calls it after every tune, as `rtl8xxxu_config` does.
- **An efuse never programmed** (0xFF) writes nothing and keeps the table's gain, and says so.
- **The number of transmit paths** is `rtl8192cu_identify_chip`'s: one on an 8188C, and on an 8192C two
  unless its bonding (`REG_HPON_FSM`) says 1T2R.

**Prediction, Pi 2:**
- At bring-up: `transmit power from the efuse, channel 1 (group 0): CCK 0x.., OFDM 0x.., 1 path(s);
  TX_AGC_A_RATE18_06 written 0x........, reads 0x........ - R11 done`. Indexes in the 0x20s to 0x30s are
  typical for this family.
- Then the join, the lease and `ping`, as before.
- `wifi scan` still lists networks: the sweep re-tunes every channel, and each tune now sets its power.

**Refuted by:**
- `NOT what was written` (the write did not land);
- a join or ping that worked before and fails now (a gain word wrong enough to break transmit);
- `no transmit power calibration` on this dongle, whose efuse is known programmed (its MAC is read from
  the same map).

What it cannot show from this side is the transmitted power itself; the access point's view of it is not
reachable from here. The read-back proves the words landed, and the link proves they are not wrong.

**The R11 card's run (2026-10-06): as predicted.**
- At bring-up: `transmit power from the efuse, channel 1 (group 0): CCK 0x28, OFDM 0x28, 1 path(s);
  TX_AGC_A_RATE18_06 written 0x31333636, reads 0x31333636 - R11 done`. The word is 0x2a in each byte plus
  the base `0x07090c0c`: the efuse's OFDM difference of +2 on its 0x28 index, applied as Linux applies it.
- JOINED. `wifi scan` re-tuned all 13 channels, each with its own power: 23 networks, 13 probe requests
  sent, 0 not, and 9 probe responses addressed to us, so the frames sent at the new power reach the air.
- `ping 8.8.8.8` 7 of 7, 19 to 26 ms.

## 22. R12a (2026-10-06): a WMM (QoS) association - hardware-verified on the Pi 2

The step 802.11n needs first. An HT station is a QoS station (802.11-2020 11.2), and access points give
HT rates only to a station that associated with WMM. mac80211 adds the WMM information element to its
association request for that reason. Until R12a this station associated non-QoS.

**What changes:**
- **The join:**
  - the find notes whether the access point advertises WMM (`mgmt::has_wmm`, a walk over every vendor
    element, host-tested);
  - the association request then carries `mgmt::WMM_INFO`: version 1, no U-APSD, as mac80211 builds it
    for a station not asking for power save;
  - `assoc_request` takes extra whole elements after the RSN one, so R12b's HT element goes the same way.
- **Out:** data goes as QoS data, TID 0, best effort:
  - `data::to_80211` gains the QoS Control field (host-tested);
  - the descriptor gains `TXDESC32_QOS`, as `fill_txdesc_v1` marks every QoS frame;
  - the endpoint is unchanged, since best effort is the queue it already used.
  - The handshake's own frames stay non-QoS, which an access point accepts.
- **In:** the replay counters are per TID now, plus one for non-QoS frames (`data::replay_slot`), per key,
  as mac80211 keeps them. A QoS access point numbers each TID's frames separately, so one counter would
  drop a second TID's frames as replays. A group key's Key RSC seeds every slot.

**Prediction, Pi 2:**
- At JOINED: `the access point does WMM: associated as a QoS station, data goes as QoS data (R12a)`.
  Most access points do WMM.
- Then the lease and `ping 192.168.10.1` and `ping 8.8.8.8` as before.
- No `a data frame replayed` lines.

**Refuted by:**
- an association refused, or answered and then a link that carries nothing (QoS frames the access point
  will not take);
- replays logged where there were none before (the per-TID counters wrong);
- the handshake failing (the access point refusing non-QoS EAPOL on a QoS association).

**The R12a card's run (2026-10-06): as predicted.** `the access point does WMM: associated as a QoS
station, data goes as QoS data (R12a)`, ASSOCIATED, the handshake as before, JOINED and the lease. `ping`
to the gateway went 5 of 5 (4 to 16 ms) and `ping 8.8.8.8` 6 of 6, with no replay dropped. One `did not send
a 286 byte frame` at the join is `nic-driver`'s first DHCP send meeting the join's last moments, seen
since R6.

## 23. R12b (2026-10-06): 802.11n - an HT association - associated and declining aggregation on the Pi 2; slower, see R12c

On a WMM association (R12a) to an access point that advertises HT Capabilities, the station associates
as an HT station, as mac80211 does for `rtl8xxxu`'s band.

**The element** (`mgmt::HT_CAP`, host-tested), as mac80211's `ieee80211_add_ht_ie` builds it from
`rtl8xxxu_probe`'s band:
- capability `0x002C`: short guard interval at 20 MHz, and SM power save off;
- A-MPDU parameters `0x1F`: 64 KiB, 16 us spacing;
- MCS 0-7 received, MCS 32, and the transmit set defined.

It goes after the RSN element and before the WMM one.

**The rates.** The firmware's mask gains the access point's HT receive set (`mcs[0] << 12 | mcs[1] << 20`,
as `rtl8xxxu` builds it). The argument byte gains 0x20 when the access point offers the short guard
interval, and so does a QoS data frame's descriptor (`TXDESC32_SHORT_GI`, as `fill_txdesc_v1` sets it).

**Aggregation is declined, in kind.** An access point offers a Block Ack session with an ADDBA request.
This station does not reorder aggregated frames, so it answers status 37, "request declined", with the
same dialog token and parameters (`mgmt::addba_request` / `addba_decline`, host-tested). That is what
mac80211 sends when it will not start a receive session, and the access point then sends unaggregated.
The receive side keeps the request (`rx::Link::addba`) and the station's `pull` sends the decline, the
way a group rekey is answered. The first is said: `the access point asked to aggregate (ADDBA, TID n) -
declined`.

**Prediction, Pi 2:**
- At the join: `an 802.11n access point: associated as an HT station; it receives MCS 0xff/0x..`.
- Then `the firmware has the rate mask 0x....fff, short GI`.
- The lease and the pings as before. `ping bytes 1024 192.168.10.1` should come back closer still to the
  32-byte one: the uplink can now reach 65 or 72 Mb/s, against 54 before.

**Refuted by:** the association refused (the HT element wrong), or the link carrying nothing afterwards
(the HT rate mask or the short guard interval wrong for this access point).

**The R12b card's run (2026-10-06): the association holds, the speed is refuted.**
- `an 802.11n access point: associated as an HT station; it receives MCS 0xff/0xff, short GI yes`.
- JOINED, then `the firmware has the rate mask 0x0fffffff, short GI`, then `the access point asked to
  aggregate (ADDBA, TID 0) - declined`. The decline was taken: the link carried on.
- **But slower.** To the gateway: 32 bytes average 16 ms (min 10) against R12a's 9 (min 4); 1024 bytes
  average 30 ms (min 26) against R8's 13 (min 8). `ping 8.8.8.8` 5 of 6, average 40 ms against 25.

## 24. R12c (2026-10-06): the two-stream rates out of a one-transmitter chip's mask - hardware-verified on the Pi 2

**The suspect, from the run above.** The mask was `0x0fffffff`: the access point receives two streams
(`0xff/0xff`), so MCS 8-15 went in beside MCS 0-7, exactly as `rtl8xxxu` builds it. This dongle has one
transmit path (SYS_CFG 1T1R; R11's `1 path(s)`). If the firmware's rate adaptation tries rates the chip
cannot send, every such attempt fails and falls back, which costs this kind of time. What Linux's
firmware does with the same mask at runtime is not measured here; only that it passes it.

**The one change:** MCS 8-15 go in only where the chip has two transmit paths (`TxPower::tx_paths`, R11).
On this dongle the mask becomes `0x000fffff`. Everything else is R12b's, including the short guard
interval, so the result points at the mask alone.

**Prediction:** `rate mask 0x000fffff, short GI`. The pings come back to R12a's levels or below: 32 bytes
around 10 ms, and 1024 bytes within a few ms of it.

**Refuted by:** the same slowness. Then the short guard interval is the next suspect, on its own card.

**The R12c card's run (2026-10-06): confirmed - the mask was the cause.** `rate mask 0x000fffff, short GI`,
the ADDBA declined as before. To the gateway: 32 bytes average 9-10 ms (min 4), 1024 bytes average 12 ms
(min 9), so the large ping is now about 2-3 ms over the small one, against 3-4 ms at R8's legacy rates.
`ping 8.8.8.8` 6 of 6, average 25 ms. The short guard interval stays: with the mask right it costs nothing
measurable.

**Where the dongle stands on the Pi 2.** A full station: scan, WPA2 with replay and KRACK protection, DHCP
and ping, the firmware's rate adaptation over 802.11n MCS 0-7 with short GI on a WMM association,
calibrated transmit power, radio off and on, hard off and power cycle by the chip's own power-down,
hot-plug, restarts with the reply mailbox kept, chaos. **Two things remain unseen, both waiting on events
nothing here can cause:** the access point's group rekey (R7, on its timer, often hourly), and `dwc2`'s
STATUS-stage retry on a replug that happens to hit a transient error (R2c's replug case). Each is built
and will say so in the log the first time it happens.

## 25. U2a (2026-10-06): the dongle behind `xhci` - bound, identified, brought up to the channel - hardware-verified on the T630

Section 7's design, first card, on the T630 (no onboard radio, so nothing to choose between yet;
`utilities/56_wifi.md` 11 has the choosing for a board with both).

**`xhci` (`services/xhci/src/radio.rs`, new):**
- **Bound by VID:PID** where the device descriptor is read: on a root port (`enumerate_one`) and behind a
  hub (`address_downstream`'s caller). Before the class decision, since a vendor-class device has no class
  to match on.
  - It is configured (Set Configuration) and KEPT, its slot and slice with it, as the disk is.
  - A second dongle is released and said: one radio per host for now.
- **Its control transfers, both directions,** on its own EP0 ring with a persistent cursor, cycle bit
  and Link-TRB wrap, as `hub_port_status` keeps one. Before this, `xhci`'s control transfer had no OUT data
  stage and ran only during enumeration.
  - The data stage uses the slice's report page, which a dongle has no interrupt endpoint to use. No new
    arena, so no kernel change.
  - `OP_CONTROL` tries 4 times and `OP_CONTROL_ONCE` once, as `dwc2` does.
  - A failed transfer clears the ring and repairs EP0 (`reset_endpoint`). The repairs are bounded per
    pass; past the bound the dongle is re-enumerated.
- **Matched by slot.** Exact while EP0 is the only endpoint driven on the dongle. U2b's armed bulk IN on
  the same slot is where matching must take the endpoint too.
- **Served from the poll loop** before the block server, and a dongle alone now reaches that loop, as a
  disk alone does. The three idle drains answer a radio request `ST_NO_DEVICE` instead of dropping it, so
  `wifi-usb` hears "no dongle" at once.
- **`NOTE_RADIO` to `wifi-usb`** when the binding changes from one pass to the next. `xhci` thereby gains
  a reacquisition path, and leaves `peer_reacquire_debt`.
- `OP_BULK_IN` and `OP_BULK_OUT` answer `ST_FAILED` until U2b and U2c.

**The supervisor:**
- `wifi-usb` is embedded where `xhci` serves it on a board with no onboard radio (`build.rs`, derived from
  the `usb` and `radio` lists, no ISA named).
- Its host peer is a board fact (`WIFI_USB_PEERS`: `dwc2` on the Pi 2, `xhci` elsewhere), and `xhci` gains
  `wifi-usb` as a peer (`XHCI_PEERS`). Both grants are pinned with their reasons.
- `NIC_PEERS` now keys the Pi 2 on `dwc2` rather than on `wifi-usb`, which the PCs embed too.

**Checked before the card:** the x86, Pi 4, VisionFive and Pi 2 images build. `osdev test iommu`
(q35, a confined `xhci`, a USB keyboard) passes, so a keyboard still enumerates and works through the
changed driver.

**Not handled yet, and recorded.** Every `xhci` re-enumeration (a keyboard replug, for one) resets and
re-addresses every device, the dongle included. `NOTE_RADIO` is sent only when the binding CHANGES, so
`wifi-usb` is not told about a re-enumeration that rebinds the same dongle, and whether the chip's state
survives the reset is not known. That is hot-plug, a later card.

**Prediction, T630, dongle on a front port at boot:**
- `xhci: DEVICE DESCRIPTOR class=0x00 VID=0x0bda PID=0x8176`, then `the WiFi dongle 0bda:8176 on port N
  (slot M) - configured, bound as the radio for wifi-usb (U2a)`.
- `wifi-usb: xhci has bound a radio at 0bda:8176`, `SYS_CFG ... read through xhci`, `U1 done`, the efuse
  with the dongle's MAC, `powered on`, the firmware `RUNNING`, `R3a done` on channel 1, and the transmit
  power from the efuse (R11).
- Then `receive did not start - the host has no bulk IN for this dongle` - U2b's work - so `wifi status`
  says the radio is down because its bring-up stopped. Expected at this card.
- The keyboard, on `ehci` on this machine, is unaffected.

**Refuted by:**
- no binding line (the VID:PID not matched, or the slice released);
- `wifi-usb` hearing nothing from `xhci` (`asking xhci about the radio: ...`);
- the bring-up stopping before `R3a done`: a control transfer this host gets wrong, which the step that
  stopped names;
- `could not be repaired` lines.

**The U2a card's first run (T630, 2026-10-06): the binding works, the driver asked the wrong host.**
- `xhci: DEVICE DESCRIPTOR class=0x00 VID=0x0bda PID=0x8176`, then `the WiFi dongle 0bda:8176 on port 7
  (slot 1) - configured, bound as the radio for wifi-usb (U2a)`, then the poll loop entered with the
  dongle alone. The keyboard on `ehci` was unaffected.
- **`wifi-usb: asking dwc2 about the radio: the service could not be reached`.** On x86 `wifi-usb` is
  spawned before `xhci`, so at its spawn the peer was declared but not wired. `host_name` took "the first
  host I hold a cap for", found none, and fell back to `dwc2`, which this machine does not have. Fixed: a
  host not yet held is REACQUIRED by name before falling back.
- **Ten seconds after binding, the dongle's port read empty** (`[topo] root port 7 attached -> empty`),
  then connected again: `new device on port 7 - re-enumerating`, which rebound it. Nothing else in the log
  explains it: no `ehci` event, nothing sent to the dongle. It is NOT diagnosed. The next run logs the
  whole `PORTSC` when the bound dongle's port reads empty, so it can be read rather than guessed (a real
  detach, or a link state).

**The U2a card's second run (T630, 2026-10-06): as predicted, to the line.**
- `wifi-usb: xhci has bound a radio at 0bda:8176`: the host was reacquired by name.
- `SYS_CFG` and `ISO_CTRL` were read through `xhci` and the chip decoded, then `U1 done`; the efuse with
  the dongle's MAC.
- The MAC cold, `powered on in 7 ms` (R1); the transmit queues; the firmware verified and downloaded in
  126 blocks, each sent once (`OP_CONTROL_ONCE`), in one try, then `RUNNING` (R2).
- The MAC, baseband and RF tables, channel 1 read back from the RF chip (`R3a done`), the station
  address, and the transmit power from the efuse written and read back (`R11 done`).
- Then `receive did not start - the host has no bulk IN for this dongle`, U2b's work, where the card was
  meant to stop.

Every register access in that bring-up was a control transfer through `radio.rs`, in both directions. On
the same dongle `xhci` was about twice as fast as `dwc2`:

| Step | `xhci` (T630) | `dwc2` (Pi 2) |
|---|---|---|
| efuse | 487 ms | about 1200 ms |
| firmware | 71 ms | 146 ms |
| MAC, baseband and RF tables | 778 ms | about 1500 ms |

The log ends a second after the bring-up, so it cannot say whether the first run's port drop recurs; the
`PORTSC` line is in place for a longer run.

## 26. The dongle's driver started by the dongle, on the Pi 2 (2026-10-06) - hardware-verified on the Pi 2

The first card of `docs/usb-device-drivers.md`, at the operator's direction: *"I would like the connected
device to be recognised and the appropriate driver/service loaded"*.

**`dwc2` reports, it does not decide.** A new SDK protocol, `usbdev` (`sdk/rust/src/service_context.rs`):
- `[supcmd::MARKER, 'U', present, binding(4), vid(2), pid(2)]`, host to supervisor, `try_send`, never
  answered. It names no driver.
- It is the host's whole state for the dongle, not a change, so a duplicate is harmless and a lost one is
  corrected by the next.
- `dwc2` sends it when it binds the dongle, when it loses it, at the end of its boot enumeration (found or
  not), and when the supervisor asks: `usbdev::ASK`, the one byte `0x26` with no reply capability, which
  `dwc2` answers with the report (`rtl::announce`, `rtl::report_device`).
- `binding` counts the dongle's binds on that host: the same dongle bound again reads differently. It
  counts from 1 in each `dwc2` instance, so a host respawn starts it again - recorded, not yet a problem,
  since nothing compares it across a host restart.

**The supervisor decides** (`USB_MATCH`, `UsbState`, `usb_report`):
- The match table: `0bda:8176 -> wifi-usb`, wired to `board::WIFI_USB_PEERS`.
- Attached: `wifi-usb` started if it is not running, adopted into the name map if it is.
- Not attached: `wifi-usb` stopped (`kill`), and its death is NOT restarted - not by the death arm, the
  reconcile sweep or the startup convergence (`UsbState::wanted`). A crash while the dongle is attached
  is restarted as before.
- A supervisor that has just started (boot, or respawned by the kernel, 6.2) asks each reporting host
  before its convergence, and leaves the dongle's driver alone until the host has answered. That is the
  reconcile: it learns what is attached rather than guessing.
- One host reports, with one such device, so "not attached" stops every row. A second host or a second
  dongle needs the report to name its host, recorded for when either exists.

**Only the Pi 2 changes.** `xhci` does not report yet, so on its boards the table is empty and `wifi-usb`
is still started at boot. `dwc2` gains the supervisor as a send peer (contract, pin).

**Checked before the card:** every gate passes in the x86 build, including the commandments red-team. The
Pi 2 image boots in QEMU (no dongle there): `supervisor: USB host reports no device with a driver here
attached` twice - `dwc2`'s boot report and its answer to the ask - and `wifi-usb` never starts.
`nic-driver` probes the absent radio three times, backs off and says `wifi-usb is not running`.

**Prediction, Pi 2, cable in:**
1. Boot WITHOUT the dongle: the `no device` line, no `wifi-usb` lines at all, and `wifi status` reports no
   radio.
2. Plug the dongle in: `usb: WiFi dongle connected`, then `supervisor: USB 0bda:8176 attached (binding 1)
   - starting wifi-usb`, then `wifi-usb`'s bring-up as before (R1 to R12c) and its auto-join from
   `/wifi.keys`.
3. Unplug it: `supervisor: USB host reports no device ...`, `supervisor: wifi-usb stopped - its USB
   device 0bda:8176 is not attached`, then `supervisor: wifi-usb ended - not restarted ...`. `wifi status`
   reports no radio again.
4. Plug it back: `attached (binding 2) - starting wifi-usb`, and it rejoins.
5. `kill wifi-usb` with the dongle in: restarted, as every service is (`died, restarting`).
6. `kill supervisor` with the dongle in: the respawned supervisor asks, `dwc2` answers, and the line ends
   `- wifi-usb running`; nothing is started twice.

**Expected and not a refutation: the first instance without a reply mailbox.** On the Pi 2 `wifi-usb` no
longer spawns at boot after `fs`; it spawns from the main loop, after `net-stack`, past the reply-mailbox
reserve with no credit banked yet (`backlog/74`). So the first instance is likely to log `spawn[ipc]:
'wifi-usb' gets no reply mailbox` and to run on the `OP_SYNC` fallback (section 19), with `syncs` above 0.
Found by the documentation audit from the spawn order. (This paragraph first said an unplug's stop
banks a credit for the next instance. It cannot: an instance with no mailbox has none to release. The
card refuted it, below.)

**Refuted by:** a `wifi-usb` started with no dongle; a `wifi-usb` restarted after an unplug; `could not
report the WiFi dongle to the supervisor`; a second `wifi-usb` after the supervisor's respawn.

**The card (Pi 2, 2026-10-06, `build/pi2_ondemand_pass.log`): as predicted, step by step.** The operator:
*"well done. The hotplug wifi usb dongle works."*
1. Boot without the dongle: `supervisor: USB host reports no device with a driver here attached` twice
   (`dwc2`'s boot report and its answer to the ask), and no `wifi-usb` at all.
2. Plugged in: `usb: WiFi dongle connected (port 4)`, then `supervisor: USB 0bda:8176 attached (binding 1)
   - starting wifi-usb`, and `JOINED` 6 s later.
3. Unplugged: `wifi-usb stopped - its USB device 0bda:8176 is not attached`, then `wifi-usb ended - not
   restarted`, both within 16 ms of `dongle removed`.
4. Plugged into another port: `binding 2`, `JOINED` 5 s later.
5. `kill wifi-usb` with the dongle in: `died, restarting`, `restarted`, `JOINED` 5 s later.
6. `kill supervisor`: the kernel respawned it, the new supervisor asked, and `dwc2`'s answer read
   `attached (binding 2) - wifi-usb running`. Nothing was started twice.
7. Unplugged and plugged in once more: stopped, then `binding 3` and `JOINED`.

**Every one of the five `wifi-usb` instances ran without a reply mailbox** (`gets no reply mailbox - 70
of 96 routing slots free, reserve 72`), as the audit expected - and none ever took one back, which is the
part of the prediction the card refuted (above). Each still joined in 5 to 6 s on the `OP_SYNC`
fallback, so this costs nothing visible today; giving an on-demand driver a mailbox is `backlog/74`'s.

## 27. `xhci` reports the dongle too, and the Pi 4 and VisionFive carry `wifi-usb` beside their onboard radio (2026-10-06) - hardware-verified on the Pi 4: the reports, the unplug, and U2a's bring-up through its `xhci`

Section 26's mechanism on the second host. The card is the Pi 4, at the operator's choice (*"easier to
test on the pi4/visionfive ... then later on on the x86 machines"*): its debug console is on the GPIO
pins, where the T630's means unplugging the serial adapter.

**`xhci` (`services/xhci/src/radio.rs`, `main.rs`):**
- The same `usbdev` report as `dwc2` (`radio::announce`, `radio::report_device`): sent when the binding
  changes from one enumeration pass to the next, and ALWAYS after the first pass, dongle or not, so the
  supervisor learns either way. Answers `usbdev::ASK` from its serve path, and from the idle drains -
  which run only where nothing is bound - with "not attached".
- `binding` counts the passes that found the dongle. Every pass re-addresses every device, so the count
  rises on any re-enumeration; it is REPORTED only when presence changes.
- **The unplug is seen.** On a root port, the dongle's port reading empty now re-enumerates (it used to
  only log the `PORTSC`). Behind a hub - every USB-A port on the Pi 4, behind the VL805's hub - the
  dongle's hub port reading "disconnected" twice running does the same, in both of the scans that walk
  a hub (the keyboard's and the disk's).
- An undeliverable "not attached" report is quiet: it is every boot on a board whose `xhci` has no
  supervisor peer. An undeliverable "attached" one is loud.

**The supervisor:** the match table holds wherever `wifi-usb` is embedded, `xhci` is a reporting host
where `dwc2` is not, and `xhci` gains the supervisor as a peer (contract, pin). `wifi-usb` is no longer
started at boot anywhere.

**Embedded on the Pi 4 and the VisionFive** (`usb_radio` in `services/supervisor/build.rs` is now
`dwc2` or `xhci`, and the kernel's and `riscv_build.py`'s lists say so). `service_embed_check.py` read
only the FIRST host in that condition, so it reported riscv64 missing the driver it embeds; it now reads
every host named.

**Two radios on one board, and what that means before `wifi hardware use` exists:** the shell's `wifi`
asks the first live service in `RADIOS`, `wifi-driver`, so it keeps answering for the onboard radio, and
`nic-driver`'s bridge is still `wifi-driver`. The dongle's driver gets no further than U2a's bring-up,
since `xhci` has no bulk IN yet (U2b), so it cannot join and does not compete with the onboard radio.
(True at this section's date. With U2c it can join from `/wifi.keys` beside the onboard radio, while the
shell and the bridge keep using `wifi-driver` - section 29.)

**Not covered, recorded:** a dongle behind a hub with no keyboard and no disk bound is not watched -
nothing walks that hub - so its unplug is not seen until the next re-enumeration.

**Checked before the card:** every image builds and x86 passes every gate. The Pi 4 in QEMU (no USB
controller there): one `supervisor: USB host reports no device ...`, from `xhci`'s idle drain answering
the ask, and no `wifi-usb`.

**Prediction, Pi 4, keyboard and storage stick as usual:**
1. Boot without the dongle: the `no device` report, no `wifi-usb`, and the onboard radio as before.
2. Plug the dongle into a USB-A port: `xhci: hub port N DEVICE: VID=0x0bda PID=0x8176`, the dongle
   `bound as the radio for wifi-usb`, `USB: WiFi dongle connected (xhci)`, then `supervisor: USB 0bda:8176
   attached (binding K) - starting wifi-usb`, and `wifi-usb`'s bring-up through `xhci` - U1, R1, R2, R3a,
   R11 - to `receive did not start - the host has no bulk IN for this dongle`, where U2b begins. The
   keyboard stalls briefly for the re-enumeration.
3. `wifi status` keeps answering for the onboard radio.
4. Unplug it: `xhci: the WiFi dongle is gone (hub slot S port N reports disconnected) - re-enumerating`
   within about three seconds, then `supervisor: wifi-usb stopped ...` and `ended - not restarted`.
5. Plug it back in: `starting wifi-usb` again with a higher binding.
6. `kill supervisor` with the dongle in: `- wifi-usb running`, nothing started twice.

**Refuted by:** a `wifi-usb` with no dongle; an unplug with no `gone` line; `wifi-usb` restarted after an
unplug; the onboard radio's `wifi status` disturbed; `could not report the WiFi dongle`.

**The card (Pi 4, 2026-10-06, `build/pi4_xhci_report.log`): the reports as predicted; the bring-up not.**
- Boot without the dongle: two `no device` reports, no `wifi-usb`.
- Plugged into a USB-A port: `hub port 3 DEVICE: VID=0x0bda PID=0x8176`, bound, then `supervisor: USB
  0bda:8176 attached (binding 1) - starting wifi-usb`.
- Unplugged, twice: `xhci: the WiFi dongle is gone (hub slot 1 port 3 reports disconnected) -
  re-enumerating` - the hub-port watch, new on this card - then `wifi-usb stopped` and `ended - not
  restarted`. Plugged back in: `binding 3`, then `binding 5`.
- `kill supervisor`: `attached (binding 6) - wifi-usb running`, nothing started twice.
- `kill wifi-usb` with the dongle in: `died, restarting`, as every service.

**What did not work: `wifi-usb`'s bring-up through the Pi 4's `xhci`.** U1's reads all answered
(`SYS_CFG`, the chip decoded as an RTL8188C, `9346CR`), and the very next transfer - the efuse read's
first, which is the first control transfer with an OUT data stage - left the dongle's EP0 in the xHCI
**Error** state (`endpoint slot 3 dci 1 is in state 4`). Error, not Halted, is where a controller puts an
endpoint whose TRB it rejected, not one whose device stalled. `xhci` re-scanned, rebound the dongle,
and `wifi-usb` said `the efuse read stopped - the transfer did not complete`. The same every time, five
starts in five. The T630's controller took the same TRBs to R11 (section 25), so this is the Pi 4's VL805
refusing something the T630's AMD controller accepts. NOT diagnosed: the next card logs the failed
transfer's completion code, its setup packet and where its TD sat on the ring, so it is read rather than
guessed.

**The instrumented run (Pi 4, 2026-10-06, `build/pi4_xhci_cc5.log`): a TRB Error, at the ring's wrap.**
`xhci: the WiFi dongle's control transfer failed - cc=5 ..., setup=[40, 05, 33, 00, 00, 00, 01, 00], OUT
1 byte(s), TD at ring offset 0xff0`. Completion code 5 is a TRB Error: the controller rejected a TRB.
The request is a one-byte register write and the OUT data stage is not the cause - it is WHERE: offset
0xff0 is the last TRB of the dongle's one-page EP0 ring, so this transfer is the one that wrote the Link
TRB and wrapped. Every device's EP0 ring is one page, but only the dongle's runs long enough to wrap.

**A property of the VL805, read rather than guessed.** Linux enables `XHCI_TRB_OVERFETCH` for the VIA
VL805: at the end of a ring segment it prefetches up to four TRBs from the next page, even past a Link
TRB on a page boundary, and may use them later without reading them again; Linux puts a dummy page after
every segment ([the patch](https://lkml.iu.edu/hypermail/linux/kernel/2501.0/05457.html)). In the
dongle's slice the next page is its interrupt ring, which the dongle does not use and which an earlier
device's TRBs may fill. **The fix, Linux's mitigation:** that page is zeroed when the dongle is bound.

**Prediction:** the bring-up passes the wrap and reaches U2a's end on the T630 - R1, R2, R3a, R11, then
`receive did not start - the host has no bulk IN for this dongle`. **Refuted by** another `cc=5` near a
wrap, which would mean a zero page is not enough here and the Link TRB must move away from the page end.

**Recorded for U2b:** the bulk IN ring will want a page of the slice, and must not be the page after the
EP0 ring, or this returns with live TRBs in it.

**The zeroed page did not help (Pi 4, 2026-10-06, `build/pi4_xhci_cc5_zeroed.log`): refuted.** The same
`cc=5`, the same request, the same `TD at ring offset 0xff0`. The offset is the same on every run because
it is deterministic: the efuse read writes three registers per byte, so the ring reaches its end about
twenty bytes in. And a Link TRB at 0xff0 is not wrong in itself: `xhci`'s hub probes wrap their own
one-page EP0 ring there on this controller thousands of times without a fault. The zeroing stays - it is
Linux's mitigation for a documented property of this part and costs one page write per bind - but it is
not this fault. The Raspberry Pi's own approach, shortening a segment by four TRBs, is the other one on
record and is not tried yet.

**The next card measures which TRB was refused.** In the Error state an endpoint's dequeue pointer stops
at the TRB it rejected, and `xhci` already reads that pointer (`ep0_hw_dequeue`). It is logged at the
failure, and once per binding the controller's dequeue is compared with this host's cursor: the dongle
is enumerated behind a hub on the Pi 4 and on a root port on the T630, and `EP0_RUNTIME_START` assumes
what enumeration left on the ring.

**The measurement (Pi 4, 2026-10-06, `build/pi4_xhci_dequeue.log`): the controller stopped AT 0xff0.**
`the controller stopped at Some((4080, 1))` - offset 0xff0, cycle 1 - for the failure whose TD this host
then put at 0xff0. So the controller was already sitting at 0xff0 before this host wrote its Link TRB
there: it had finished the TD that ends at 0xff0, read on into the next slot, found a TRB with cycle 1 -
one it is allowed to run - and stopped on it with a TRB Error. The request that then failed only
reported the error. The Link TRB was never refused.

**The cause is ours, not the VL805's.** A ring's slots ahead of the producer must read as not yet given
to the controller: cycle 0 against our 1. Linux gets that by zeroing every ring it allocates. The dongle's
EP0 ring is a page of a reused slice, and nothing cleared it, so a slot an earlier device left with cycle
1 is a TRB the controller may run. Every other device's EP0 ring stays near its start; only the dongle's
reaches 0xff0. The T630 got away with what happened to be in its page. (The bind-time dequeue read
`(0, 1)` against this host's 0x80: the endpoint context's pointer is written back only when the endpoint
stops, so while it runs it is stale, and transfers from 0x80 working shows it is.)

**The fix:** the dongle's EP0 ring is cleared from `EP0_RUNTIME_START` to the page's end when it is
bound. The interrupt-ring zeroing stays, for the overfetch, with its comment corrected: it did not cure
this. **Prediction:** the bring-up passes 0xff0 and reaches `receive did not start - the host has no bulk
IN for this dongle`, as on the T630. **Refuted by** any further `cc=5`.

**The cleared ring did not help either (Pi 4, 2026-10-06, `build/pi4_xhci_ep0clear.log`): refuted.** The
same `cc=5`, the controller stopped at 0xff0 cycle 1. With the slot zeroed there was nothing there it was
allowed to run, so the reading above was wrong: the controller idled at 0xff0, this host wrote its Link
TRB there and rang, and the controller stopped ON THE LINK with a TRB Error. The clearing stays - a
ring's unwritten slots should read as not given, and Linux zeroes rings for that reason - but it is not
this fault. **Also withdrawn:** the claim above that the hub probes wrap their EP0 ring at 0xff0
"thousands of times" on this controller. Nobody checked it; at one probe every 1.5 s they may never have
wrapped at all.

**What is documented** (read, not recalled): the Raspberry Pi kernel's `XHCI_AVOID_DQ_ON_LINK`, because
the VL805 "can't cope with the TR Dequeue Pointer for an endpoint being set to a Link TRB" and its context
"ends up stuck at the address of the Link TRB"
([raspberrypi/linux be18ca1](https://github.com/raspberrypi/linux/commit/be18ca1d4ca4cd6b85eabfe3645d3d11ad0939d3)).
That is about a Set TR Dequeue command, not an idle endpoint, so it is a reason, not a proof. And the
disk's bulk rings also write their Link lazily on this controller and work, which argues against it.

**Next experiment: the Link written eagerly** - straight after a TD whose follower would not fit, before
the doorbell - so the controller follows it while busy and never rests on it. A rewind instead (Stop
Endpoint and Set TR Dequeue to the ring's base, which `xhci` already does as a repair and which this
controller accepts) was considered and not taken: `xhci`'s command ring is one page per pass and does not
wrap, and a bring-up would spend two commands every eighty transfers. **Prediction:** no `cc=5`, and the
bring-up reaches `no bulk IN`. **Refuted by** a `cc=5` at the new wrap (offset 0 after the Link, or
wherever the dequeue says).

**The eager Link passed the wrap (Pi 4, 2026-10-06, `build/pi4_xhci_eagerlink.log`): confirmed.** No
`cc=5`. The efuse read completed (the dongle's MAC, 31 sections, 236 ms), the chip powered on (`R1
done`) and the transmit queues were set up - all through the Pi 4's `xhci` and past the ring's end.
Where the cause sits is still not proven - the Link written lazily, with the controller idling on its
slot, is what changed - but the fix is.

**The next fault, on the firmware download:** the first 128-byte block (`setup=[40, 05, 00, 11, ...]`,
an OUT of two 64-byte packets, the first transfer this bring-up makes that is larger than eight bytes)
failed with `cc=4`, a USB Transaction Error, after the controller's own three retries (`CErr` is 3). EP0's
maximum packet is 64 for a high-speed device, as it should be. Then, after `radio.rs` repaired EP0, every
transfer timed out with the controller's dequeue still at the ring's start: the repair leaves an endpoint
the doorbell does not restart. The first error may be the dongle's own - `dwc2` met errors in this
download too, which is why it re-runs a failed stage (R2c) - but the dead endpoint after the repair is
this host's. **The next card measures it:** every logged failure gives the endpoint's state, and every
repair says what it left.

**U2a on the Pi 4 (2026-10-06, `build/pi4_xhci_u2a_pass.log`): as predicted, to `no bulk IN`.** No failed
transfer and no repair. The efuse, `R1 done`, the transmit queues, the firmware in 126 blocks `in 18 ms (1
try)` and `RUNNING`, `R3a done` (channel 1 read back from the RF chip), and the transmit power written
and read back (`R11 done`), then `receive did not start - the host has no bulk IN for this dongle`.

| Step | `xhci` (Pi 4, VL805) | `xhci` (T630) | `dwc2` (Pi 2) |
|---|---|---|---|
| firmware | 18 ms | 71 ms | 146 ms |
| MAC, baseband and RF tables | 417 ms | 778 ms | about 1500 ms |

**Still open, and recorded rather than closed:** the previous run's `cc=4` on the first firmware block did
not recur, so it is intermittent - and when it happens, the repair leaves an endpoint whose transfers all
time out. That is this host's to fix. The instrument stays in, silent unless a transfer fails: the next
occurrence says the endpoint's state at the failure and what the repair left.

The U2a work on the Pi 4, in order of what each run showed: the reports and the hub-port unplug watch
(confirmed), the wrap's TRB Error located at the Link's slot (measured), a zeroed next page and a cleared
ring (both refuted as the cure, both kept as correct ring hygiene), the eager Link (confirmed).

## 28. U2b (2026-10-06): the bulk IN through `xhci` - hardware-verified on the Pi 4 (2026-10-07, with section 30's fix)

Received frames, through the Pi 4's `xhci` first (the operator's test board; the T630 later).

**Telling a slot's two endpoints apart.** The dongle's EP0 and its armed bulk IN share one slot, and a
completion for one must never be taken as the other's. `next_event_at` now returns the transfer event's
endpoint ID (bits 20:16) and residual (bits 23:0) beside what it already did, and `EvMail` files a
completion that another consumer dequeued by endpoint as well as by slot (`take_ep0`, `take_bulk`;
`take` and `have` keep their meaning for the keyboard and the hub probes). Every consumer that files one -
the disk's wait, the hub probe, the poll drain, the dongle's control transfer - passes the endpoint.

**The bulk IN** (`radio.rs`): found in the configuration descriptor already read at bind (`parse_eps`;
every length distrusted) and added with one Configure Endpoint (`configure_radio_bulk` since U2c, which adds the OUTs in the
same command; built as `bind_msc` builds the disk's). Its ring is the second half of the slice's report page, whose first half
is EP0's data stage; its 3584-byte buffer (`usbfn::BULK_IN_MAX`, seven packets) is the slice's
interrupt-ring page. That keeps every RING away from the page after the EP0 ring, which the VL805 reads
into (section 27); a data buffer there does no harm. The Link is written eagerly, as EP0's is.

**The protocol, as `dwc2` answers it:** one transfer kept armed, Interrupt On Completion and On Short
Packet, so every frame completes at once; a completion held, `NOTE_BULK_IN` told (sent again each pass
if the driver's queue refused it); `OP_BULK_IN` returns what is held and arms again, `ST_OK` with no data
when nothing is. A failed transfer is told too, and the ask that follows repairs the endpoint (Reset
Endpoint and Set TR Dequeue to the ring's start, bounded per pass). The bulk OUT is U2c and, in this
image, still answers `ST_FAILED`.

**Missed here, and found by the audit after U2c (section 30):** an `OP_SYNC` naming `NOTE_BULK_IN` was
dropped. `dwc2` keeps it and sends the notice again once the driver is quiet; `xhci` answered only a
`NOTE_RADIO` one, and at once. On the Pi 4 `wifi-usb` is started on demand and has no reply mailbox, so
a receive notice that lands while it is mid-call is that case, and receive would stop at the first one.
The U2b image on the operator's SD card was built before the fix.

**Checked before the card:** the Pi 4, x86 and VisionFive images build and x86 passes every gate.

**Prediction, Pi 4, dongle plugged in at the prompt:**
1. `xhci: the WiFi dongle's bulk IN 0x81 configured (DCI 3, mps 512) - receive ready (U2b)`.
2. The bring-up as in section 27, then `wifi-usb: receive started` instead of `receive did not start`.
3. `xhci: the WiFi dongle's first bulk IN transfer - N bytes (U2b)`, `wifi-usb: the FIRST frame from the
   air ... R3b done`, and `wifi-usb: beacon '<network>' <its BSSID> on channel N, -NN dBm` for the networks
   around, each once.
4. A sweep's probe requests and an auto-join need the bulk OUT, so they fail and say so - U2c's work.
   The onboard radio is untouched.

**Refuted by:** a Configure Endpoint failure; no first bulk IN transfer; a `bulk IN transfer failed` that
recurs; EP0 transfers failing where section 27's did not (the endpoint matching wrong).

**Result (2026-10-07, the Pi 4, the card `card/u2b-sync`: U2b plus only section 30's fix): all four
predictions met.** With the dongle plugged in at the prompt, on hub port 3 behind the VL805:
`bulk IN 0x81 configured (DCI 3, mps 512) - receive ready (U2b)`; the bring-up to R11 as in section 27;
`receive started`; `the WiFi dongle's first bulk IN transfer - 284 bytes (U2b)`; `the FIRST frame from
the air - a 284-byte transfer; R3b done`; eight networks named on channel 1, each once. Receive kept
going: `rx - 1024 transfers, 1024 frames (951 beacons, 0 failed their CRC, 0 cut short), 8 networks
named` twelve seconds later, and the counts were still rising when the log ended. The probe request, the
auto-join's authentication and its deauthentication each said `was not sent - the device or the bus did
not take it`, as prediction 4 says: U2c's work, and the onboard radio carried on untouched.

Section 30's fix was seen working in the same run: three `wifi-usb took NOTE_BULK_IN in place of an
answer (OP_SYNC, N so far) - sent again once it is quiet` lines, during the join, and receive carried on
after every one. Without the fix, receive would have stopped at the first.

The same log found a second fault, with the dongle plugged in at BOOT rather than at the prompt
(section 31).

## 29. U2c (2026-10-06): the bulk OUTs through `xhci` - hardware-verified on the Pi 4 (2026-10-07): joined through the VL805

Built while the operator was away, on top of U2b, and held off the card until U2b has run, so each flash
still tests one change.

**What it adds** (`radio.rs`, `configure_radio_bulk`):
- The dongle's bulk OUTs, read from the configuration descriptor in the walk U2b added (`parse_eps`, in
  descriptor order, as `wifi-usb` names them), are added in the SAME Configure Endpoint as the IN: one
  command, so the device's context is never half-configured.
- **Where the rings live.** Up to three OUT rings of 28 TRBs, in the slice's report page between EP0's
  data stage and the IN ring, each followed by a 64-byte gap. The VL805 reads the 64 bytes after any TRB
  it fetches, and past a ring's Link that lands in the gap rather than in the next ring.
- **Where the frame lives.** In `DATA_BUF_OFF`, the arena page enumeration uses for its control data,
  borrowed for one synchronous send at a time. Nothing else touches it inside the poll loop, and a send
  never overlaps an enumeration, since both run on one task. That needs no kernel change. A dedicated page
  would (`XHCI_DMA_PAGES`), which is the cleaner answer if this one is ever contended.
- **`OP_BULK_OUT`, as `dwc2` sends.** One Normal TRB, the Link written eagerly, the completion waited for
  up to 200 ms (`dwc2`'s budget), matched by slot AND endpoint. A completion for the IN or for another
  consumer met while waiting is handed on, not taken.
- **A failed or unanswered frame.** The endpoint is repaired before the status goes back (its ring
  cleared, `reset_endpoint` to the ring's start, bounded per pass). A TD that never completed must not
  stay queued ahead of the next frame.

**Not handled, recorded:** a frame whose length is an exact multiple of the 512-byte packet gets no
zero-length packet after it. Whether the chip needs one is not checked against Linux, and no frame on
this path has had that length yet.

**Checked:** the Pi 4, x86 and VisionFive images build and x86 passes every gate.

**Prediction, Pi 4, once U2b's card has passed:**
- `xhci: ... bulk OUT(s) [...] configured - receive and send ready (U2b, U2c)`.
- `xhci: the WiFi dongle's first bulk OUT transfer`, then `wifi-usb: the FIRST probe response addressed to
  us ... R5a done`.
- The auto-join from `/wifi.keys` through authentication, association and the WPA2 handshake to `JOINED`
  (R5b, R5c), on the dongle, beside the onboard radio, which keeps carrying the link (the shell and
  `nic-driver` still use `wifi-driver` until `wifi hardware use` exists).

**Refuted by:** a Configure Endpoint failure; `bulk OUT ... failed` lines; no probe response; U2b's
receive breaking.

**First card (2026-10-07, the Pi 4, `card/u2c` at `f230503e`): stopped before any frame was sent, by
the intermittent firmware-download fault of section 27, not by U2c.** The Configure Endpoint passed:
`bulk IN 0x81 (DCI 3, mps 512) and 2 bulk OUT(s) [4, 6] (mps 512) configured - receive and send ready
(U2b, U2c)`. The bring-up reached the firmware, and its first block failed with `cc=4`. EP0 was
repaired eight times, and every transfer after each repair timed out with no event. Then `xhci`
re-enumerated, and the dongle could not be addressed again on two walks. It is now two failed downloads
in four Pi 4 bring-ups. The keyboard was lost as well, which is section 32. U2c's predictions are
untested, not refuted.

**Second run of the same card (2026-10-07): JOINED.** The dongle was left plugged in at boot, so
section 31's fault happened first, exactly as described there. The re-enumeration came 1.7 s after the
bind, the first `wifi-usb` stopped at `LLT_INIT stayed busy`, and the prompt came up later than usual
(section 31). The dongle was then unplugged and plugged into another hub port at the prompt. On that
bring-up the firmware downloaded in one try, and receive started. `the WiFi dongle's first bulk OUT
transfer - 88 bytes (U2c)` came next. Then the auto-join from `/wifi.keys`: `AUTHENTICATED (Open System,
status 0)`, a QoS and HT association (R12a, R12b), `ASSOCIATED, association ID 1`, the four-way
handshake with message 3's MIC verified and its key data unwrapped, both keys in the CAM, and `JOINED -
handshake complete`. Fourteen seconds later came `the FIRST data frame through the link - ethertype
0x0800, 42 bytes, decrypted by the chip; R6 receive works`. Receive ran on for 3072 frames with none
failing their CRC. The onboard radio stayed joined and in use, and `wifi hardware` gave both rows:

```
RADIO      CHIP            BUS         STATE          NETWORK               IN USE
onboard    CYW43455        SDIO        joined         <network>             *
usb        RTL8188CUS      USB xhci    joined         <network>
```

**Met:** predictions 1 and 3. **Not met: prediction 2.** No `FIRST probe response addressed to us` line
appeared, and the rx counters said `0 answers to our probes` throughout. The join found the access point
from its beacons, so this did not stop it. Either the probe requests are not reaching the air, or they
are not being answered, and this run cannot say which. Open. As expected, `nothing is taking received
frames` followed, because the Pi 4's bridge is `wifi-driver`'s (section 27). Carrying traffic over the
dongle is `wifi hardware use`.

## 30. `OP_SYNC` for both of `xhci`'s notices (2026-10-07) - found by an audit, hardware-verified on the Pi 4 (section 28's result)

**What was wrong.** `usbfn::OP_SYNC` is how a `wifi-usb` with no reply mailbox recovers a notice it took
in place of an answer (section 19): it names the notice, and the host gives the reply capability back and
sends the notice again once the driver has asked nothing for a moment, so it reaches the driver's serve
loop rather than its next call. `dwc2` does exactly that for both of its notices (`sync_bulk`,
`sync_radio`, `DRIVER_QUIET_MS`). `xhci` was written at U2a, when its only notice was `NOTE_RADIO`, and
answered a sync for it by sending it again AT ONCE - into the driver's next call, the case the quiet
exists for - and ignored any other. U2b added `NOTE_BULK_IN` and did not touch it, and the comment there
still said `NOTE_RADIO` was the only notice.

**Why it matters on the Pi 4 in particular.** An instance the supervisor starts on demand has no reply
mailbox (`backlog/74`), and since section 27 that is the only way `wifi-usb` starts on the Pi 4. So a
`NOTE_BULK_IN` arriving while `wifi-usb` is in a call - a sweep retuning, a register read - is the
ordinary case, not the respawn corner section 19 met. Dropped, it stops receive for good: nothing is
armed until the held transfer is collected, and the notice had been delivered, so nothing was owed.

**The fix, `dwc2`'s, in `radio.rs`:** the time of the driver's last request is kept; a sync for either
notice is recorded (`sync_bulk`, `sync_radio`) and the notice sent again by `radio::service` once the
driver has asked nothing for 5 ms (`DRIVER_QUIET_MS`, `dwc2`'s figure); a re-sent `NOTE_BULK_IN` goes
only while a transfer is still held or failed. With no dongle bound, a `NOTE_RADIO` sync is still told at
once, as `dwc2` does. The first three syncs are logged: `xhci: wifi-usb took NOTE_BULK_IN in place of an
answer (OP_SYNC, N so far) - sent again once it is quiet`.

**Checked:** the Pi 4, x86 and VisionFive images build and x86 passes every gate. Nothing in QEMU
reaches it (no dongle there).

**On the cards.** The U2b image on the SD card predates this fix, so it may stall after its first few
frames for this reason alone. Section 28's predictions stand for U2b with this fix; a `took NOTE_BULK_IN
in place of an answer` line followed by more beacons is this section working, and receive stopping right
after that line refutes it.

**Result (2026-10-07):** three such lines during the join, more beacons after each, and 1024 frames
received in the next twelve seconds (section 28's result).

## 31. A dongle present at boot re-enumerated the controller under its driver (2026-10-07) - fixed, hardware-verified on the Pi 4

**What was seen.** On the U2b card the dongle was on hub port 1 when the Pi 4 booted. `xhci` bound it
during the boot enumeration and the supervisor started `wifi-usb`, which got as far as R2. About 1.3 s
after the bind, `xhci` logged `new device on hub slot 1 port 1 - re-enumerating` and reset the whole
controller. The dongle came back bound in the same slot, but its driver's set-up had been cut off under
it, and it stopped: `the radio's set-up stopped - a link-list entry was never taken (LLT_INIT stayed
busy)`. The run then went on with the dongle unplugged and plugged in again at the prompt, which is
section 28's result.

**Why.** Each of `xhci`'s two hub scans (the one driven by a bound keyboard and the one driven by the
disk) checks its match arms in order. The arm that takes a connected port that has not been tried yet as
an ARRIVAL came before the arm that knows the port is the dongle's. A port is marked tried only by an
arrival the scan itself saw. A dongle plugged in at the prompt arrives that way and is marked, so section
27's card never met this. One bound during enumeration, as at boot, is not marked, so the scan's second
connected read of it counted as a new device and re-enumerated. Each scan does this at most once, because
the arrival arm marks the port as tried.

**The fix:** the dongle's "still connected" arm now comes first in both scans, so the dongle's own port is
never taken for an arrival. The unplug arm already came before the arm for an empty port and is
unchanged.

**It also delays the prompt.** The shell prints `gsh>` once `fs` is serving, and the re-enumeration
takes the USB stick away and gives it back. On U2c's second run (section 29, an image without this fix)
`fs` mounted at 6.0 s and the prompt came at 6.1 s. With the dongle not plugged in at boot, U2a's pass
had the prompt at 4.8 s.

**Prediction, Pi 4, dongle plugged in BEFORE power-on:** no `new device on hub slot 1 port <the dongle's>`
line and no `xhci: reset: entering` after the boot enumeration. The driver goes on through R11 to `receive
started` and beacons, as in section 28, and to `JOINED` as in section 29. The prompt comes back to about
5 s. **Refuted by:** a re-enumeration naming the dongle's port while
it is bound.

## 32. A command's completion taken from the command before it (2026-10-07) - fixed, hardware-verified on the Pi 4

**What was seen** on U2c's first card (section 29), after the dongle's EP0 could not be repaired.
`xhci` re-enumerated. On hub port 3 the dongle was refused with `Enable Slot REFUSED (completion=4)`.
The keyboard on hub port 4, which had been bound twice before in the same session, was then not found
at all: there was no line for port 4, and `0 HID device(s) bound`. A second walk, five seconds later, did
exactly the same. The keyboard stayed lost for the rest of the run, because a port whose arrival has
been tried is not tried again while it stays connected.

**What is wrong.** `run_command` took the first Command Completion Event as its own. It did not check
the address of the command TRB that every completion carries. So a command that timed out (returning
`None`, in silence, through each caller's `?`) left its completion to be read as the NEXT command's,
and every command after that as the one before it. Enable Slot cannot complete with `cc=4`, a USB
Transaction Error, but an Address Device sent to a device that does not answer does. That is what the
refusal looks like: the address retry's Enable Slot reading the previous attempt's late Address Device.
**Not shown:** why port 4 printed nothing. Its status read is a control transfer, and a failed read was
skipped as an empty port without a word, so this run cannot say whether that is what happened.

**The fix.** `run_command` takes only the completion whose TRB pointer is its own command's. A late one
for an earlier command is logged (`a completion for an earlier command arrived late ... discarded`) and
skipped. A command with no completion says so, rather than returning `None` in silence. The hub walk
now logs a failed port status read (`hub port N status read failed - not scanned this pass`) rather
than treating it as an empty port.

**Prediction, the next time the dongle's EP0 cannot be repaired on the Pi 4:** no `Enable Slot REFUSED
(completion=4)`. Instead, possibly `a completion for an earlier command arrived late` or `command type N
got no completion`. And the keyboard on the other port is bound after the walk (`1 HID device(s)
bound`). **Refuted by:** the keyboard lost again with neither new line explaining why. If a `status read
failed` line names its port, the cause is the hub's EP0, not the command ring.

## 33. The branch tip on the Pi 4 (2026-10-07): sections 31 and 32 and the prompt, confirmed; the download fault still intermittent

One card carried three fixes, each with its own line in the log: section 31, section 32, and the shell
drawing its prompt before it reads `/persist.conf` (`1da33d6b`).

**The prompt:** `gsh>` came with the same timestamp as `shell: ready`, 2.4 s after power-on. On U2c's
second run it came at 6.1 s, and 4.8 s on a boot without the dongle plugged in. **Reverted later the same day
(`06753a16`):** the prompt came early but the check it moved behind it then held the shell until `fs`
served, so for about 2.5 s after `gsh>` nothing typed was echoed - a prompt saying ready when it was
not. The prompt waits for the check again. A prompt that is both early and true needs an `fs` request
that does not hold the console read.

**Section 31, confirmed.** The dongle was plugged in at power-on. There was no re-enumeration naming its
port, and it went from the boot enumeration to `JOINED` with no replug, 6.5 s after power-on.

**Section 32, confirmed, in exactly the predicted shape.** Later the dongle's EP0 died again, and the
walk after it logged `command type 11 got no completion within its bound` (Address Device), then `a
completion for an earlier command arrived late (completion=4 ...) - discarded`. Each happened two or
three times. Then `keyboard found` and `1 HID device(s) bound`: the keyboard survived the walk that lost
it before. That late `completion=4` is the one the old code read as Enable Slot's refusal.

**The download fault is still there, and intermittent.** The dongle was moved between hub ports during the run.
Of the four bring-ups, two failed in the firmware download with `cc=4` and the same dead EP0 after
the repair. The boot one and the last one, on port 3, downloaded in one try and joined. One bring-up also ended with three
bulk IN transfers failing with `cc=4`, just before the hub reported the port disconnected. Whether that
was the plug being pulled or the dongle dropping off the bus, this log cannot say. Counting the Pi 4
bring-ups whose download is in the logs kept, it has failed in 4 of 10. One more fault: during a failed bring-up, `wifi
hardware` showed the dongle as `usb  ?  ?  down`. Its chip and bus were unknown even though the driver
had identified the chip. This fault and the dead repair are the next work on this host.

## 34. The keyboard put on the 10 ms poll by a report its interrupt had not been taken for yet (2026-10-07) - REVERTED after section 35; its card found section 36's stick, and the fix itself was never observed

**What was seen.** On every Pi 4 run since the dongle arrived, about a second after the keyboard is
bound, `xhci` logs `waking on interrupts (MSI) - not polling` and then `a HID report arrived with no
interrupt - polling input at the 10ms tick`. From then on it wakes every 10 ms for the rest of the
session, on a controller whose interrupts demonstrably work: the dongle's frames arrive on them.

**Why.** `xhci` decided that interrupts do not carry input the first time it found a keyboard report on
a pass that the interrupt itself had not woken. That is not evidence. The pass may have been woken by
another message on the same endpoint, such as one of `wifi-usb`'s requests, which are constant during
its bring-up, or a block request. The report's own interrupt is then still queued behind that message,
and the one-message drain took it without noting it. The second site was worse: a report that a hub
probe's synchronous wait had consumed also counted, though it says nothing about interrupts at all. One
such report set the poll for good.

**The fix.**
- Only a pass whose wait ran out its whole deadline, and found a report waiting, counts as a report no
  interrupt announced.
- An interrupt message taken from the queue behind the waking message counts as this pass's interrupt.
- A report consumed by a hub probe counts for nothing.
- Three timed-out finds in a row switch input to the 10 ms poll. Sixteen interrupt deliveries in a row
  switch it back (`16 HID reports in a row came on interrupts - waiting on them again`). Either switch is
  logged, up to four times.
- The cost on a machine whose interrupts really do not cover its HID (the T630 booted single-core, with
  `0 MSI` in its heartbeat) is three slow keystrokes instead of one before the poll engages.

**Prediction, Pi 4, keyboard and dongle:** no `polling input at the 10ms tick` line, and the heartbeat's
`fast` passes far fewer than its `idle` ones while nobody types. The previous run's heartbeat read `14275
fast/4703 idle`. Typing feels the same. **Refuted by:** the polling line appearing anyway, which would
mean three reports really were found by timed-out waits, or by typing lag.

**Reverted, untested (2026-10-07).** The card that carried it alone (after section 35 was reverted) found
the stick unreadable (section 36), and the operator asked for the interrupt work to come out entirely
until it can be revisited. `xhci/src/main.rs` is back at `ebe4bc4e`, so input goes on the 10 ms poll as
before. The analysis above stands; the fix was never seen on hardware.

## 35. Hub ports watched by the hub's status-change endpoint, not a 500 ms timer (2026-10-07) - REVERTED: worse on the Pi 4

**What it replaces.** `xhci` learned of anything plugged into or pulled from a port behind a hub by
asking the hub about every port, over its control endpoint, every 500 ms (`HUB_POLL_MS`). On the Pi 4
every USB socket is behind its internal hub (2109:3431), so this was the driver's main timer: of
55.7 s of work in the first 183 s, the heartbeat charged 46.4 s to the hub segment. It also shared the control endpoint with every
other request to the hub, which is where the hub's late-answer and wedge handling comes from.

**What a hub has for this.** Every hub has one interrupt IN endpoint, 1 IN, whose data is a bitmap of
the ports with a change pending (USB 2.0 11.12.1). The controller polls it at its interval in hardware,
and a transfer completes only when something changed.

**What is built.**
- **Configured with the hub.** The endpoint is read from the hub's configuration descriptor and
  configured in the same Configure Endpoint as the hub (DCI 3). This is done only for a USB 2 hub: a
  SuperSpeed one has other change bits and an endpoint companion, and is still scanned on the timer. If
  the hub refuses the endpoint, it is configured as before and the log says so.
- **The ring.** It has four Normal TRB slots and a Link, written in the producer order the VL805 needs
  (section 27). With one slot, QEMU's controller, which rests on the Link after a completion, met the
  next lap's cycle there and stopped. The debug log showed its endpoint context's dequeue pointer at
  the Link's address.
- **No other wait can swallow a completion.** Every event the driver reads goes through `next_event_at`,
  and that is where a hub's status-change completion is recorded and taken out of the ring. So none of
  the half-dozen loops that read events can file it as someone else's answer. A failed completion
  removes the hub from the armed set, its ports go back to the 500 ms scan, and the log says so.
- **The scan.** On a change the poll loop queues the next TD and scans the hub's ports at once, then
  every 500 ms for 2 s, because the arrival and departure rules want two consecutive
  readings. After that the scan runs every 5 s, but only once every hub that
  something is bound behind is armed, and only once an interrupt has been seen, so a controller whose
  interrupts never arrive keeps its 500 ms wait for the keyboard's sake.
- **The change is acknowledged.** A hub keeps reporting a port while any of its `C_PORT_*` bits is set,
  so the probe now reads `wPortChange` and clears each bit that is set (features 16 to 20) over the same
  control ring. It does this only where the endpoint is armed. Nothing in the driver reads those bits.
- **One more fix it needed.** QEMU refuses a Configure Endpoint that adds EP0 (TRB Error, completion 5)
  and had been refusing the plain hub configure all along. With the endpoint, the command adds the slot
  and DCI 3 only. The plain form, which the VL805 accepts, is unchanged.

**Checked in QEMU** (x86, `qemu-xhci` with a `usb-hub` and the keyboard behind it, and a mouse
hot-plugged onto the hub from the QEMU monitor): `status-change endpoint armed`, then `hub ports
watched by the hub's status-change endpoint`. The mouse's arrival was seen 0.8 s after the monitor added it,
including the two-reading confirmation, and its removal 0.05 s after the monitor removed it. Each change was
reported as `a hub reported a change on its status-change endpoint`. With one slot, before the fix, both
took the 5 s safety scan. `osdev test iommu` still passes.

**Prediction, Pi 4 (its hub behind the VL805):** `hub configure (... status-change endpoint)
completion=1`, `status-change endpoint armed`, then `hub ports watched by the hub's status-change
endpoint`. Plugging and pulling the dongle or the stick is seen within about a second, each with a
`reported a change` line. The heartbeat's `hub` segment and pass count fall well below the previous
run's (`hub 46426` ms, 18978 passes in 183 s). **Refuted by:** `refused its status-change endpoint`, or
`failed - its ports are scanned every 500 ms again` (the VL805 differs from QEMU here), or an unplug that
takes five seconds to be seen (the change was missed and the safety scan caught it).

**Result (2026-10-07, the Pi 4): refuted, and reverted the same hour.** The endpoint was accepted and
armed on every pass (`hub configure (... status-change endpoint) completion=1`), and the first
enumeration bound the keyboard, the stick and the dongle. Then the dongle's firmware download failed
with `cc=4` (section 33), its EP0 could not be repaired, and `xhci` re-enumerated. From the fourth pass
on, NO device behind the hub could be addressed: `downstream Address Device failed (completion=4)` for
the stick, the dongle and the keyboard alike, Address Device and Enable Slot getting no completion at
all, one controller reset that did not halt within 250 ms, and a re-scan every few seconds that bound
nothing. The operator saw the keyboard dead for long stretches: "the user experience is much worse".
The run before, without this change and with the same dongle fault, re-bound the keyboard on every pass.

**Not shown:** which part of the change did it - the endpoint armed during the port walk, the Configure
Endpoint without EP0's add flag, the change-bit clears, or the fault starting from the dongle anyway and
only being unlucky here. `xhci/src/main.rs` went back to `822f8736`, section 34's keyboard fix alone,
and then to `ebe4bc4e` without it. The
design stands as a record; trying it again needs a run that separates those, with the dongle unplugged so
its fault cannot start the chain.

## 36. The stick's first sector no longer GSFS after section 35's run (2026-10-07) - the stick was overwritten; reformatted

On the next boot, with section 35 reverted, `xhci` read the stick's sector 0 as `01 0c 6e 65`, with a
boot-signature word of `0x558a`, where every earlier run read `47 53 46 53` (`GSFS`). `fs` reported `bad
superblock magic - disk not formatted` and is waiting for `drives flash`. Section 35's run had already
shown the disk path misbehaving after the walks began failing: reads of lba 0 refused by `xhci`, and `fs:
CRC mismatch on directory block lba 7784 healed on re-read 1 - a transient bad READ ... (the transport
served garbage as a complete transfer)`.

**Not shown:** whether the sector on the stick was overwritten, or whether only this boot's read returned
the wrong bytes. That log was overwritten, so what was written during section 35's run cannot be
recovered from it. A second boot that reads the same bytes says the stick holds them; GSFS read correctly
says it was the read. Recovering the stick means `drives flash`, which erases it, `/wifi.keys` with it.

**Result (2026-10-07, the next boot, the reverted image):** the same `01 0c 6e 65` and `0x558a` on two
enumerations of that boot and again after a replug, so the stick itself held them: it was overwritten.
What wrote it is still not shown. The operator ran `drives flash -0 data` (`formatted as GSFS - mounted`),
after which sector 0 read `47 53 46 53` on every pass, `fs` worked, and both radios joined again, the
onboard one by `wifi join` and the dongle from the `/wifi.keys` that join wrote. The interrupt work that
preceded this is reverted (sections 34, 35); a retry of it should run with a stick whose loss does not
matter.

## 37. `wifi hardware <radio>` and `wifi hardware use` on the Pi 4 (2026-10-07): the commands work, and `nic-driver` follows the choice (fixed, verified); the dongle drops off under load, and the disk misreads across re-scans

`utilities/56_wifi.md` 11 and 11a, with both radios. What held:

- **`wifi hardware usb` and `wifi hardware onboard`** gave every fact predicted: the dongle's chip from
  `SYS_CFG` (RTL8188C, 1T1R, cut A, TSMC), its IDs, its efuse address, firmware 88.2, the bus as root port
  1 > hub port 3, slot 3, the endpoints and queues; the onboard radio's address and firmware version from
  the firmware. The `in use` lines agreed with the report's `*`.
- **`wifi hardware use usb`** with both joined: the dongle already joined, so no join; the choice written,
  the onboard radio `left` its network, and `wifi hardware` then marked `usb`.
- **The reboot**: `wifi-usb: /wifi.radio names usb - this radio is the one in use`, rejoined; `wifi-driver:
  not rejoining ... another radio is the one in use`. Only the dongle joined.
- **`wifi forget`** went to both radios: `wifi-usb: /wifi.keys written - 0 network(s)` and then
  `wifi-driver: /wifi.keys written - 0 network(s)`, so neither put the key back.
- **`wifi hardware use onboard`**: `joining ... on onboard first`, the passphrase asked for (the forget
  earlier in the run had dropped the key), `JOINED`, the dongle `left`, `/wifi.radio` removed, and
  `nic-driver` back on the onboard radio within 15 s.

**What did not: the link never moved to the dongle.** After `use usb`, and again after the reboot with
`usb` chosen, `net` said `the cable is out and the radio is not joined` and `ping` had no link. There was
no `the radio bridge now goes to wifi-usb` line at all. The cause is in the spawn log: `task: peer
'wifi-usb' not yet registered, no SEND cap for 'nic-driver' (declared - will reacquire)`. `nic-driver`
starts before the dongle's service, so it holds no cap to it, and the reacquire it does for a silent radio
runs on the CURRENT radio's silence count, which the ask of the other radio deliberately leaves alone. So
every ask of `wifi-usb` went to no cap and failed at once, and nothing ever looked the name up.

**Fix:** `Radio::ask_other` reacquires the other radio's cap by name before each ask (at most every 5 s,
`OTHER_EVERY_MS`), and does not ask at all when the name does not resolve. It also covers a dongle service
respawned since the last ask. Built; not yet seen on hardware.

**Second card run (`e90f200a`): the fix is verified.** `use usb` gave `nic-driver: the radio bridge now goes
to wifi-usb (the radio in use, /wifi.radio)` 5 s later, the link stayed up with its DHCP lease, and `ping
8.8.8.8` answered through the dongle (14-66 ms). When the dongle dropped, the bridge went to the onboard
radio (`the one that answers`); when the dongle came back, the bridge went back to it within 2 s of its
`/wifi.radio` line. Both directions of the follow logic were seen.

**What the run found instead: under sustained ping the dongle left the bus.** Twice, about 10 s into
traffic: bulk IN `cc=4` three times in a row, a bulk OUT `cc=4`, then `the WiFi dongle is gone (hub ...
reports disconnected)`. The hub saw the device detach, which is the device or its power, not a ring
state; the Pi 2 carried the same traffic on its own port without it. Recovery was complete each time:
re-enumerated, `wifi-usb` restarted by the supervisor, rejoined, bridge back, ping answered (101 sent, 42
received across both drops). Not explained; one candidate to test is the dongle's transmit current
through the VL805 hub port.

**And after the reboot that followed, the stick read wrong.** The boot hit the known download fault (the
`cc=4` firmware block, EP0 not repairable, section 33), and each failed repair re-scans the whole bus,
which took `xhci` away from `block-driver` for seconds at a time while `fs` mounted. Across those
re-scans the disk returned OTHER blocks' contents as complete transfers: lba 7699 read as the superblock
(`47534653 30303038`), another as ASCII digits, and `fs` logged `healed on re-read 2 - ... the transport
served garbage as a complete transfer`. Sector 0 later read `01 0c 2e 67` twice. No write reached the
stick in that boot (no journal commit, no `/wifi.keys` save), so the stick is probably intact, but that
is not shown.

Read, not yet changed: `msc::await_on_slot` takes the first transfer event for the disk's SLOT, from the
ring or from the `EvMail` filed for that slot, without checking which TRB it completes. The CSW tag check
catches a shifted status, and is what produced the many `refused ... status -1`; nothing checks that a
DATA stage's completion is that stage's. A stale filed completion, or one from a device that held the
same slot number before a re-scan, would end a data stage before the device wrote the buffer. The fix
this points to: match each stage on its TRB pointer, as `hub_port_status` already does, and drop the
event mailbox when the controller is reset. That is the next change, not made here.

**Made (2026-10-07), built, checked in QEMU, not yet on the card.** Each stage of a disk command now
waits for the transfer event whose TRB pointer is the one it posted (`Ring::push` returns it); a completion
for the disk's slot that retires any other TRB is logged as `passed over` and not taken, from the ring and
from `EvMail`, which now files the pointer too (`take_trb`). The mailbox half of the plan above was not
needed: `EvMail` is built fresh on every pass and the whole arena is zeroed on a controller reset, so
nothing filed survives one. The slot-only match predates this branch (it is unchanged since before the
dongle work began); what is new is a second device on the same controller whose faults make the disk's
waits run out. `scripts/cross_isa.py`, whose riscv64 leg runs every disk read and write through `xhci`'s
mass storage in QEMU, passes 12 of 12 with no completion passed over.

**First card run of it (2026-10-07, one boot).** The stick's sector 0 read `01 0c 2e 67` on the FIRST read
of a cold boot, on a controller nothing had happened to yet, and `fs` found no filesystem. So the previous
session did change the stick; the "no write reached it" above was wrong, read from the absence of `fs`
log lines, and `fs` does not log every block write. Reformatted (`drives flash`), then the keys saved and
`dir` listed 3 entries. Across the next two re-scans (the dongle left the bus again, then the download
fault) sector 0 read `47 53 46 53` both times, with no CRC mismatch, no `served garbage` and no
`passed over`. The third re-scan lost the stick altogether: `downstream Address Device failed
(completion=4)` for hub ports 1 and 2, the dongle and the stick, while the keyboard on port 4 bound, and
`fs` then reported storage unavailable, its data intact. A new failure of the re-scan, not of the read
path; not explained.

Also seen, not changed: a `wifi hardware` straight after the onboard radio has left logs `wifi-driver: the
firmware REFUSED the request - BCME_NOTASSOCIATED`, its status ask of a radio that is not associated. The
report is right; the line is noise.

## 38. A dongle fault handled on its hub port alone, and a command ring that wraps (2026-10-07) - REVERTED: on the Pi 4 it took the keyboard away

Section 37 showed every dongle fault re-enumerating the whole controller, and the disk paying for it.
`xhci` was made to handle a dongle behind a hub on its own (rebind.rs in `61c8c827`; all of this was reverted):

- **Its hub port reads disconnected** (it dropped off the bus): its slot is released (Disable Slot) and
  its slice freed. Nothing else is touched. The supervisor is told it is gone, as before.
- **Its EP0 cannot be repaired** (the download fault): it is released, then its hub port alone is reset,
  addressed and bound - the steps the full walk takes for one port, with the same timings - and
  `wifi-usb` is told the new binding.
- **A device arrives on the port the dongle was last on**: that port alone is brought up. If what is
  there is not the dongle, it is released and the bus is re-enumerated so it is bound the ordinary way.
- **A dongle on a root port** is re-enumerated as before; nothing else shares a root port.

The hub's control ring is the poll loop's: the requests ride the same cursor the hub-port probes use
(a hub_request that is reverted), matched on the TRB pointer. A slice reused for the dongle is zeroed first, because the
full walk only ever starts from an arena a reset has just zeroed.

**And the command ring was made to wrap (reverted with the rest).** It never did: each command took the next slot and only a controller
reset put the cursor back. The re-enumerations reset it often enough that the end was never reached;
without them a long session of dongle repairs would have written past the ring into the event ring.
The last slot is a Link TRB back to the first, Toggle Cycle clear, and each command clears the cycle bit
of the slot after it (a next_cmd that is reverted). `scripts/cross_isa.py` passes 12 of 12 with the ring cut to three slots,
so it wrapped every two commands through a whole USB disk bring-up and the round trip, and again at its
real size.

**Not testable in QEMU:** it emulates no RTL8188, so the release and the re-bind themselves have run on
nothing yet.

Also noted the same day: v0.21.0 on the Pi 4, with the dongle in, put `gsh>` up about 6.5 s after power,
against 3.0 s without it, and re-enumerated twice after the prompt (`new device on hub slot 1 port 1`,
then port 2), re-binding the keyboard each time - section 31's boot-dongle fault, before its fix. So the
slow first keystrokes with the dongle in predate this branch's interrupt work, which the operator
confirmed by booting the release.

**On the card, and why it is reverted.** The dongle was plugged in after boot; the bus re-enumerated as
before and bound it, and its firmware download then hit the cc=4 fault. The new path did what it was
written to: `resetting its hub port 3 alone`. But `Address Device` then failed three times
(`completion=4, route=0x3`) - where the full re-enumeration minutes earlier had addressed the same
device on the same port at once. The failure is not explained: something the full walk does to the
hub or the port, which a one-port reset from the poll loop does not, matters.

And the failed re-bind was retried WITHOUT A BOUND. The port was left marked untried, so the next
confirmed reading tried it again: 60 times in two minutes, each costing about 7 s of the poll loop, and
the keyboard is polled by that loop. Typing stopped. That retry was the mistake, and the operator named
the commandment it breaks: VIII. `Address Device` answered with a definite failure, completion 4, and
the code took that answer as a reason to ask again on the next probe, so the timing of the next reading
decided what happened next rather than the cause of the failure. A failure is a truth to act on, not a
cue to wait and repeat (and an unbounded repeat is 26.6 as well). The right next step is the one not
taken: find out why a one-port re-address fails where the full walk succeeds, before any retry at all. The operator chose to close the branch rather than carry this further; the commit is
reverted whole, the command ring wrap with it, since the wrap was needed only once the re-enumerations
that reset the ring were gone. The finding about the ring stands for whoever returns: it does not wrap,
and only a controller reset puts its cursor back.

## 39. The radio bridge on the PCs: the RTL8168 carries frames to the dongle when the cable is out (2026-10-07) - hardware-verified on the T630, the cable path included (section 40), and on the Wyse (41)

Until now a dongle on the T630 or the Wyse could scan and join and carry nothing: `nic-driver`'s RTL8168
backend had no radio bridge, so with the cable out `net-stack`'s frames went to a dead cable. The
bridge is the one the other three boards already share, `services/nic-driver/src/radio.rs`, now
included from the RTL8168's serve loop as well (both PCs have that chip; the Wyse is the board its
single-descriptor transmit fix was found on). The rule is unchanged: the cable always wins.

**What changed in the RTL8168 loop (`realtek_serve`):**
- **The cable is read live on every request**, from PHYSTATUS - one register read, where GENET pays two
  MDIO transactions and so remembers it for 500 ms. A `chaos link-flap` override counts as the cable,
  so a forced DOWN hands the frames to the radio as an unplug would.
- **STATUS** with the cable in is the 32-byte answer it always was, the chip's tally included. With the
  cable out it is the nine-byte answer every radio backend gives (`radio::status`), whose last byte names
  the carrier; that is how `net-stack` and `net` learn the radio carries the link. The tally is the
  cable's, so it is not sent while the cable carries nothing.
- **Op 10** (which access point) is answered. Before, this backend took the one byte for a frame and sent
  it - harmless only because nothing asked it, since only a nine-byte STATUS leads `net-stack` to ask.
- **Receive (ops 4 and 9) and transmit** go to `wifi-usb` with the cable out, and none of the cable's
  receive-ring reset or descriptor work is done for them. The RDU/FOVW re-arm at the top of the loop runs
  on every request either way, so a cable plugged back in finds a receiver that is running.
- **A request that arrives while the bridge waits on the radio** is kept and served first
  (`Radio::take_held`), as on the other boards.
- **Every reply goes through one helper, `answer`**, which sends and gives the reply capability back. The
  radio's held requests carry a raw capability handle, and `gs` has no way to make a `gs` capability
  from one, so the loop answers by handle; `nic-driver`'s raw-SDK count fell from 94 to 82 rather than
  rising (`one_way_check.py`).

**The supervisor** gives `nic-driver` `wifi-usb` as a peer on the PCs (`NIC_PEERS`, its own branch keyed
on `has_wifi_usb`). The contract already declared it.

**One fix in the shared bridge:** "`wifi-usb` is not running" was logged on every failed probe, though
the comment above it said once and every sixteenth. With no dongle and the cable out that is a line a
second; it is now once, then every sixteenth, with a count. The same on every board.

**Checked:** the x86, Pi 2, Pi 4 and VisionFive images build, and x86 passes every gate. In QEMU (x86,
UEFI, e1000 - QEMU has no RTL8168) the image boots, `nic-driver` is spawned with the new peer (`peer
'wifi-usb' not yet registered ... will reacquire`, as on the Pi 2 before the dongle is plugged), DHCP
completes and `ping 10.0.2.2` answers in about 1 ms. So the supervisor change and the e1000 path are
seen; the RTL8168 loop itself, and the radio path, have run on nothing yet.

**Prediction, T630, the dongle on a front port at boot, the cable in:**
1. Boot as before: `nic-driver: RTL8168 C+ TX/RX rings up (link UP)`, a DHCP lease, `ping` to the
   gateway answers. `net` prints the `nic-hw` tally line, as today.
2. `wifi scan` lists the networks, and `wifi join` joins one - the first time either has run on x86
   since `xhci` gained the bulk IN and OUTs (sections 28, 29). **Refuted by** a scan that finds nothing,
   or a join that never reaches the handshake.
3. Cable out: within a status query, `nic-driver: the cable is out - the radio carries the link (MAC
   ...)` with the dongle's MAC, `net-stack` re-configures on the new address and leases again, and `ping`
   to the gateway answers over the dongle. `net` says `link up via wifi (the cable is out)`. **Refuted
   by** `the radio did not send` repeating while `wifi status` says joined.
4. Cable back in: `the cable carries the link; the radio stands by`, a lease on the cable's address,
   `ping` answers, and the `nic-hw` line is back in `net`.
5. Then the same on the Wyse.

**The T630 card's run (2026-10-07).** Booted with no cable and the dongle out, then plugged in.
- **Before the dongle:** `the radio did not answer 0x10` three times, then `wifi-usb is not running (x1)`,
  and nothing more - the rate limit doing what it says.
- **Plugged in after boot:** `xhci` bound it on root port 7, the supervisor started `wifi-usb`, and the
  bring-up ran to receive in about 1.6 s. Hot-plug on x86 works.
- **`wifi scan`** found 12 networks across channels 1 to 13 (probe responses addressed to us included),
  and **`wifi join`** went to JOINED in 1.4 s, WMM and HT. Step 2 confirmed: the first scan and join on x86
  since the bulk endpoints.
- **The cable out, the radio carrying the link:** `nic-driver: the cable is out - the radio carries the
  link` with the dongle's MAC, `net` said `link up via wifi (the cable is out)`, `net-stack` took a DHCP
  lease, pinged the gateway, and `ping 8.8.8.8` answered 6 of 6 at 16-66 ms. Step 3 confirmed.
- **`wifi radio off`, then `on`** (the same instance): the rejoin, and `ping 8.8.8.8` 2 of 2.
- **Steps 1 and 4 were not run:** no cable was plugged in this session, so the cable path of the new
  loop on the RTL8168, and the hand-back to the cable, are still unseen.

**A fault this card found - after the chip's power-down, the restarted driver joins and receives
nothing.** `wifi radio off hard` then `on`, and later `wifi radio powercycle`: each restarted `wifi-usb`
onto the cold chip, the instance rejoined from `/wifi.keys` through the whole handshake (message 3's MIC
verified, both keys in the CAM), `nic-driver` said the radio carries the link, and transmit worked
(`the FIRST data frame sent through the link ... encrypted by the chip`). But every frame back was
refused: `a data frame from the access point the chip did not decrypt (protected true, security 0,
swdec true)`, and `net-stack` saw 0 frames in 16 pings. `security 0` in the receive descriptor is the
chip finding no key for the frame - the CAM lookup missed, or receive decryption is off - although the
key was written. On the Pi 2 the same restarts (section 19) rejoined and `ping` answered, so this is not
yet a fact about the chip. What separates the two runs here: the working instance came from a plug and
joined by `wifi join` after a scan; the failing ones came from `rtl8192cu_power_off` and rejoined from the
key file with no scan. **The same log narrows it:** the dongle was later unplugged and plugged back into
`xhci`, a plug and not a power-down, and that instance rejoined from `/wifi.keys` and refused every frame
the same way. So the power-down is not what separates them; a fresh instance rejoining from the key file
is, and on this host only. NOT diagnosed. The next step is an instrument, not a retry: after the keys go in,
read back `REG_CR`'s security bit, `REG_SECURITY_CFG` and the two CAM entries, and say them once on the
first undecrypted frame.

**Also in the log, not explained:** `cap::get: ResourceId(127) gen mismatch cap=33 rec=35` while the third
instance ran - something still held a send cap to the first `wifi-usb` instance's endpoint, two
generations old. The log does not say who. And at the end the dongle left the bus (`bulk IN ... cc=4`,
`port 7 reads EMPTY while bound`), the controller was reset, and no port had a device on the census -
consistent with the dongle being pulled.

**And on `ehci`, the operator's question.** The dongle was also tried on the back ports, which `ehci`
serves. `ehci` enumerated it at high speed on its hub ports 4 and 3 and said `not a HID, skipping`: it
binds no WiFi dongle, and `wifi-usb`'s hosts are `dwc2` and `xhci` only, so that is the expected result,
not an error. The `didn't enumerate (faulty port - try another)` message was hub port 1's, a LOW-speed
device: the keyboard (a Logitech, 046d:c30a) after it left `ehci`, whose split-transaction descriptor read
failed three resets before it was found on `xhci`'s port 7. `ehci` went on resetting port 1 for 12 s after
the keyboard had moved.

## 40. The key store, read back (2026-10-07) - an instrument for section 39's refused frames; on the card the fault did not recur, and the CAM read was not reading the CAM

Section 39's fault: a fresh `wifi-usb` that rejoins from `/wifi.keys` writes both keys, transmits, and
every frame back comes up `security 0` - the chip found no key for it. The question is whether the key
store holds what was written. Asked of the chip, not of this driver's memory (Commandment VIII): no
retry, no workaround, a reading.

**What it reads (`rtl8188::key_store`), and says twice per join** - once after JOINED, and once on the
first frame of that join the chip did not decrypt (`station.rs`, `say_key_store`):
- `REG_CR`, whose bit 9 is the security enable `install_key` sets;
- `REG_SECURITY_CFG`, which `install_key` writes as 0xcf;
- words 0 and 1 of CAM entries 0 and 1: each entry's control word (valid, group, key id, cipher) and the
  address it matches. **Never words 2 to 5, the key itself:** a read-back for the log must not carry key
  material.

A CAM word is read with `REG_CAM_CMD` (polling bit, no write bit, the word's address) and lands in
the CAM read register (0x0678, REG_CAM_READ in `rtl8xxxu_regs.h`) - both in the register map in `build/rtl` (`rtl8xxxu_regs.h`, and `RWCAM` / `RCAMO`
in `rtl8192cu_sw.c`). The code that drives the read in Linux is not in `build/rtl`, so the wait is this
driver's: 100 us, as a write gets, then the polling bit must be clear or the read says it did not
complete. Ten control transfers, a few milliseconds, once per join and once per bad join.

**Checked:** the x86 and Pi 2 images build and x86 passes every gate. Nothing here runs in QEMU.

**Prediction, T630, the cable out, the dongle on `xhci`.** `/wifi.keys` is on the disk from section 39's
card, so a plug-in now rejoins from it - section 39's failing case:
1. After `JOINED`: `key store after the join - REG_CR=... (security on), SECURITY_CFG=0xcf`, entry 0
   `valid, pairwise, key id 0, cipher 4` at the access point's address, entry 1 `valid, group, key id
   <the group key's id>, cipher 4` at the same address. (4 is CCMP's value in the control word's bits
   4:2, which `install_key` writes as 4 << 2.)
2. On the first refused frame: the same line again, `on the first frame the chip did not decrypt`.
3. Then `wifi join <the same network>` by hand in that instance, and `ping` - whether the hand-made join
   decrypts where the rejoin did not, with its own two readings.

**What each reading would say:**
- `security OFF`, or SECURITY_CFG not 0xcf, on the second reading but not the first: something after the
  join rewrote them - look for the writer.
- An entry `NOT valid`, or at the wrong address: the CAM write did not take, or was undone.
- Both readings exactly as written: the key store is right, and the chip's miss is elsewhere - the
  receive configuration, or the station address it matches against.
- `not read back: the CAM read did not complete`: the read sequence is wrong and this instrument has
  said nothing about the CAM; `REG_CR` and SECURITY_CFG are not read either, since the read stops at the
  first error. Recorded so the next reader does not take silence for a result.

**The card's run (T630, 2026-10-07).** Booted with the dongle in and the cable out; the operator then
power-cycled the radio twice, turned it off and on, and plugged the cable in and out.
- **Section 39's fault did not recur.** Four joins from `/wifi.keys` - the boot's rejoin, the rejoin after
  each `powercycle`, and after `radio off`/`on` - and every one decrypted: `ping 8.8.8.8` answered each time,
  20 of 20 in the longest run. Two `did not decrypt` frames in the whole session, each a single frame, against
  every frame of three joins in section 39. Section 39 failed 3 of 3; this run 0 of 4. That is a fault
  that comes and goes, and this run does not say why. **And the instrument is suspect in its own right:** it
  adds about ten control transfers right after the keys go in, before the first data frame, which is
  exactly the kind of change that hides a timing fault. So "it did not recur" is not "it is fixed", and the
  instrument staying in is not neutral (see what was kept, below).
- **`REG_CR`'s security bit and `SECURITY_CFG` read as written**: `0x02ff (security on)`, `0xcf`, after
  every join.
- **The CAM read was wrong.** Both entries read `control 0xff10`, address all zeros, after every join - for
  a pairwise entry and a group entry written with different control words (the group one with its group
  bit and key id 2) at the access point's address. A read that returns the same word for two different
  entries is not reading them. It is taken out: a log line that looks like a fact and is not one is worse
  than no line. What stays is the two enables. Reading the CAM properly needs the read sequence Linux
  drives (rtlwifi's CAM code), which is not in `build/rtl`; it is not guessed at a second time.
- **The cable path on the RTL8168, and the switch both ways, confirmed** (section 39's steps 1 and 4).
  Cable in while the radio carried a ping: `the cable carries the link; the radio stands by`, a lease on
  the cable's network, and the ping ran on without losing a reply; `net` showed `nic-link UP 1000M full`
  and the chip's tally. Cable out again: `the cable is out - the radio carries the link`, `net-stack`
  re-configured on the dongle's address and leased again.

**And `net` with the cable in did not SAY the cable carries the link.** With the cable out `net` prints
`link up via wifi (the cable is out)`, from the nine-byte answer; with the cable in the RTL8168 answers 32
bytes, and the shell printed the `nic-link` line for it but no `link` line at all, so a reader had to know
that `nic-link UP` means the cable. The shell now prints `link     up via the cable` (or `down - no
cable`) for the long answer too, the same words the eight- and nine-byte answers use. The operator's
report; a shell change only.

**Prediction for the next card (T630):** `net` with the cable in shows `link     up via the cable` above
the `nic-link` line; with the cable out, `link     up via wifi (the cable is out)` as before. After every
join, one `key store after the join` line with `security on` and `0xcf`, and no `CAM entry` lines.

**That card's run (T630, 2026-10-07): as predicted.** `net` with the cable in printed `link     up via the
cable`, and with it out `link     up via wifi (the cable is out)`. Three joins from `/wifi.keys` (boot,
`radio off`/`on`, `powercycle`), each with one `key store after the join` line, `security on` and `0xcf`,
and no `CAM entry` lines; none refused its frames. The cable went in and out twice, a 48-ping run across
the switches lost one reply. The Wyse is next, on the same image.

## 41. The Wyse: the dongle never addressed, and the keyboard lost on a replug - a stale input context in `xhci` (2026-10-07) - fixed, hardware-verified on the Wyse; a USB3 hub fault found behind it, open

**The Wyse card (same image as section 40's last run).** The dongle was never seen, and after a while
the keyboard did not come back from an unplug either. The T630, retested on the same image by the
operator, still hot-plugs its keyboard. Neither fault is in this branch's dongle code.

**What the log showed.** The Wyse's keyboard sits behind the board's internal USB2 hub on root port 1,
and `xhci` walks that hub first. Then:
- **Every high-speed device directly on a root port failed Address Device with Transaction Error
  (completion 4)**, three times, then the port was skipped: the device on port 6 at every boot, and the
  dongle on port 4 when it was plugged in. This was recorded in the Wyse bring-up as "port 6, unknown
  device", and put down to that device.
- **The SuperSpeed devices on ports 10 and 15 got no completion at all** to Address Device. A command ring
  runs in order, so every command after it - Disable Slot, Enable Slot, and the Reset Endpoint and Set TR
  Dequeue that repair the hub's control endpoint - got none either; no late completion was ever logged, so
  the stuck one never finished. With the hub's control endpoint unrepairable the hub cannot be asked what
  changed, and a keyboard unplugged from it is never found again. Every re-enumeration (about one a
  minute, on `hub slot 1 unreachable 200x`) rebuilt the hub and the keyboard and then wedged again on
  port 10. (`xhci` enumerates SuperSpeed root ports since `fb9bc1f8`, on main since August, so a USB3
  stick can be found; the Wyse bring-up had skipped them for exactly this wedge.)

**The cause found in the code, for the first of the two.** One input context is shared by every
Address Device. Every path that fills it clears it first - except `enumerate_one`, the root-port path,
which wrote only the words it uses. The hub path had just written the keyboard's slot dword2: its
transaction translator, hub slot 1, port 4 (`0x0401`), which a low- or full-speed device behind a
high-speed hub needs and nothing else may carry. So every root-port device enumerated after the keyboard
went to the controller claiming a translator it does not have, and the Wyse's Intel controller refused
it. On the T630 the dongle was always the first device found, so nothing had written the word before it.

**The fix:** `enumerate_one` clears the input context first, as the others do.

**Checked:** the x86 image builds and passes every gate. QEMU, the Wyse's order reproduced - a hub on a
root port with a keyboard behind it, a USB stick on a later root port: the hub, the keyboard, then the
stick addressed and its disk read. QEMU accepted the stale word before the fix as well (it does not
validate it), so that run shows the order works and nothing regressed, not that the fault is gone.
`scripts/cross_isa.py`, whose riscv64 leg runs its disk through `xhci`: 12 of 12.

**Prediction, Wyse, the dongle on a front port at boot, the cable in:**
1. Port 6's device and the dongle each pass Address Device - no `completion=4` on any root port.
2. The SuperSpeed ports 10 and 15: either they address (they then show as hubs, the USB3 halves of the
   board's hub), or they fail with a completion code. Either confirms the stale word was also their
   cause. **Refuted by** `Address Device - no completion` on port 10 again: then that is a second fault,
   a command the controller never finishes, and the next step is the spec's answer to one (Command Abort,
   xHCI 4.6.1.2) - to be read before it is written, not retried around.
3. The keyboard unplugged and replugged, twice: found again each time.
4. Then section 40's card: `wifi scan`, `wifi join`, the cable out and in, `ping`.

**The Wyse card's run (2026-10-07): the fix holds, and it uncovered the next fault.**
- **Every root port addresses now**: the hub on port 1 and the keyboard behind it, the dongle on port 4,
  and ports 6, 10 and 15, which turn out to be the board's other hubs (0bda:5415, and the USB3 halves
  0bda:0411 and 0bda:0415). No `completion=4` on any root port, at boot or on any re-enumeration.
  Predictions 1 and 2 confirmed: the stale word was the cause of both.
- **The operator: "everything works"** - the dongle scanned, joined and carried the link, and the cable
  path ran as on the T630.
- **But with the dongle unplugged, the keyboard is not seen leaving or coming back; plugging the dongle in
  brings both back.** The log says why. A dongle unplug re-enumerates the whole controller (section 37's
  known behaviour). The walk then reaches the USB3 hub on port 15 and finds a device on its port 2 - the
  boot stick, most likely, the one SuperSpeed device in the machine - and that device's Address Device
  never completes. The command ring wedges behind it, so the repair of hub slot 1's control endpoint never
  runs, hub slot 1 cannot be asked what changed, and the keyboard behind it is invisible
  (`hub slot 1 port 3 status probe -> None`). Plugging the dongle in is a ROOT port change, which resets
  the controller and rebuilds everything, which is why both come back together.
- **Why only with the dongle out:** `xhci` has six device slices. With the dongle bound all six are in use
  before the walk reaches port 15's downstream port (`out of DMA slices for a downstream device -
  stopping hub walk`), so the stuck device is never tried. With it out, one is free.

**Why the device behind port 15 does not address - read from the code, not yet proved.** `xhci` decides
a hub is SuperSpeed by its answer to the hub descriptor: it asks for the USB2 one (0x29) and only asks for
the USB3 one (0x2A) if that returns no ports. The Pi 4's VL805 answers nothing to 0x29, so it is found as
USB3; this Realtek hub answers 0x29 with two ports, so it is walked as a USB2 hub (`USB2 hub on port 15`)
though it sits on a SuperSpeed port. And nothing in `xhci` sends a USB3 hub SET_HUB_DEPTH, which - as I
understand the USB 3 hub class, NOT yet checked against Linux's hub driver or the spec - a SuperSpeed
hub needs before it can route anything below it by route string. Either would leave a SET_ADDRESS that
never reaches the device.

**Open, recorded rather than fixed here (26.7), as `backlog/78`:** USB3 hubs on the Wyse - decide SuperSpeed by the port's
speed, not by which descriptor answers; SET_HUB_DEPTH, read from the USB 3 spec and Linux's hub driver
before it is written; and the general fault under it, that a command which never completes blocks every
command after it, for which the spec's answer is Command Abort (xHCI 4.6.1.2). No QEMU device emulates a
USB3 hub, so all of it is hardware-only. It predates this branch - the walk never reached that device
while ports 10 and 15 could not be addressed at all - and the state now is strictly better than before:
before the fix the dongle was never seen on the Wyse and a replugged keyboard was always lost.

## 42. The Pi 2 regression card for this branch's shared changes (2026-10-07) - passed; section 39's refused frames seen on the Pi 2 as well

The Pi 2's dongle was hardware-verified before the shared changes of sections 37 to 41 (`wifi hardware`,
the key-file merge, forget to every radio, the bridge's rate-limited log, the key-store line, the `net`
link line). One card, cable and dongle in at boot, image from `05574ac0`.

- **Boot:** `dwc2` reported the dongle, the supervisor started `wifi-usb`, it rejoined from `/wifi.keys`
  and said `key store after the join - REG_CR=0x02ff (security on), SECURITY_CFG=0xcf`.
- **`wifi hardware`:** one row, `usb RTL8188CUS USB dwc2 joined ... *`.
- **`net`:** `link up via the cable`, and with the cable out `link up via wifi (the cable is out)`;
  `ping 8.8.8.8` 6 of 6 over the dongle.
- **`wifi radio powercycle`:** the restart onto the cold chip rejoined from the key file and `ping` went 7
  of 7.
- **The cable in and out three more times:** each switch logged, a 48-ping run across them lost 4.
- **A replug of the dongle** (binding 2): started, rejoined, carried the link.
- **`wifi forget`, then `wifi join`:** `/wifi.keys written - 0 network(s)`, then after the join `1
  network(s)` - the merge and the forget as written.
- **The bridge's log:** `wifi-usb is not running` appears 0 times - the dongle's driver was running or being
  started throughout, and the rate limit was not needed. No panic.

**And section 39's fault, on the Pi 2.** After the forget, a second `wifi radio powercycle` (its wait
quit by the operator), then `wifi join` by hand in that new instance: JOINED, both keys written, both
enables read back as written - and every frame back `did not decrypt (protected true, security 0, swdec
true)`, `ping` 0 of 5. So it is **not `xhci`'s and not the key file's**: it happens behind `dwc2`, and to a
hand-made join as well as a rejoin. What every failure so far shares is a fresh `wifi-usb` instance on a
chip that has just been brought up; what separates a failing one from a working one is still unknown. In
this session's cards: four failures in about fifteen fresh joins, on two hosts. `backlog/79` carries it.

## 43. The key store laid out as Realtek's own driver lays it out (2026-10-07) - for `backlog/79`; hardware-verified on the Pi 2 (sections 45, 46)

`backlog/79`: a fresh `wifi-usb` sometimes joins and every frame back comes up `security 0`. Section 40
showed the two security enables read back as written, and the CAM could not be read back (rtlwifi has no
CAM read either - `build/rtl/cam.c`, fetched for this). So the question became what the CAM layout
should be, read from the driver Realtek wrote for this chip family, rtlwifi, rather than from rtl8xxxu
alone.

**What rtlwifi does, and this driver did not** (`build/rtl/rtlwifi_rtl8192ce_hw.c` `rtl92ce_set_key`,
`cam.c`, `cam.h`, `rtl8192cu_hw.c` `rtl92cu_enable_hw_security_config`, fetched from Linux master):
- **A group key goes in the CAM entry its key id names** - entries 0 to 3 are the default keys - at the
  broadcast address, ff:ff:ff:ff:ff:ff. This driver, following rtl8xxxu, took the first free entry: the
  pairwise key in 0, the group key in 1, at the BSSID. This card's access point uses group key id **2**.
- **The pairwise key goes in entry 4** (`CAM_PAIRWISE_KEY_POSITION`), key id 0, at the access point's
  address.
- **The control word has no group flag** (`rtl_cam_add_one_entry`: valid, cipher, key id).
- **`REG_SECURITY_CFG` is 0xcc for a WPA2 station** - TX encrypt, RX decrypt, and the default keys for
  BROADCAST only. This driver wrote 0xcf, rtl8xxxu's value, which also sets the default keys for unicast;
  rtlwifi sets those only for WEP and IBSS, where every key is a default key.

So with the old layout, a group frame was looked up in default-key entry 2, which was empty. That is
certain from the layouts, and it alone explains the single refused frame in otherwise working joins.
Whether it also explains a join where EVERY frame is refused, unicast included, is NOT shown: the old
layout was the same in working and failing joins. The receive path now counts a refusal to us and a
refusal to a group apart, and says the first of each and every 64th, so the next failure, if there is
one, says which key the chip did not find.

**The change:** rtlwifi's layout and value, in `station.rs` and `rtl8188::install_key`. A rekey into a
group slot overwrites that slot's entry; leaving clears every entry used. Recorded as a deliberate
divergence from rtl8xxxu where the value is set (26.14).

**Checked:** the x86 and Pi 2 images build and x86 passes every gate. Nothing here runs in QEMU.

**Prediction, Pi 2, cable out, dongle in:**
1. After every join: `pairwise key 0 in CAM entry 4`, `group key 2 in CAM entry 2`, and
   `SECURITY_CFG=0xcc`.
2. No `did not decrypt ... to a group` at all - group frames decrypt now.
3. Across the boot's rejoin, five `wifi radio powercycle`s and one replug, each followed by `ping
   8.8.8.8`: every one answers, and no `did not decrypt ... to us`. **Refuted by** a join with `to us`
   refusals: then the pairwise lookup is what fails, and the split says so.

## 44. USB3 hubs: recognised by their protocol, told their depth, their devices addressed at SuperSpeed (2026-10-07) - for `backlog/78`; built, not yet on hardware

`backlog/78`: on the Wyse, after a dongle unplug re-enumerates, the walk reaches the device behind the
USB3 hub on root port 15, its Address Device never completes, and the command ring stops behind it -
taking the keyboard's hub repairs with it. Read against Linux's hub driver (`build/rtl/usb_core_hub.c`
and `usb_core_hub.h`, fetched from master), the walk got three things wrong for a USB3 hub:

- **It decided "SuperSpeed" by which descriptor answered.** It asked for the USB2 hub descriptor (0x29)
  and tried the USB3 one (0x2A) only when that gave no ports. The Pi 4's VL805 answers nothing to 0x29;
  the Wyse's Realtek 0bda:0415 answers it with two ports, so it was walked as USB2. Linux decides by the
  device descriptor's protocol (`hub_is_superspeed`: bDeviceProtocol 3). Now so does this - in addition
  to the old test, which still finds the VL805.
- **It read a USB3 hub port's status with USB2 bits.** Bit 9 is low speed on a USB2 hub and PORT_POWER on
  a USB3 one, set on every powered port, so every device behind a USB3 hub was taken for low speed and
  addressed at speed 2 with a transaction translator. A USB3 hub's ports carry no speed bits; Linux
  (`hub_port_reset`) gives the device SuperSpeed from the hub alone. Now so does this.
- **It never sent Set Hub Depth.** A USB3 hub routes by the route string and must be told its tier first;
  Linux's `hub_activate` sends class request 12 after Set Configuration with the hub's level less one.
  Now sent, depth 0, because this walk runs on hubs on root ports only.

**Checked:** the x86 and Pi 4 images build and x86 passes every gate. QEMU, the USB2 hub path unchanged
(a hub with a keyboard behind it, a stick on a root port: all addressed, the disk read);
`scripts/cross_isa.py` 12 of 12. No QEMU device is a USB3 hub, so the new path itself has run nowhere.

**Not done, and why:** Command Abort (xHCI 4.6.1.2), the general answer to a command that never
completes. With the hub handled as what it is, nothing known produces such a command, so an abort path
written now could not be exercised by any card; it would be code nobody has seen run.

**Prediction, Wyse, the same order as section 41's card (this image also carries section 43's change,
whose own card runs on the Pi 2):**
1. At boot: `USB3 SuperSpeed hub on port 10` and `on port 15`, each followed by `Set Hub Depth 0 ... OK`.
2. With the dongle unplugged (the re-enumeration that reaches port 15's port 2): the device there
   addresses - most likely the boot stick, as USB mass storage - and there is no `no completion` on any
   command.
3. Then the keyboard unplugged and replugged, twice: seen leaving and coming back each time, with the
   dongle still out.
4. The dongle plugged back in: bound, joined, `ping` over it.
**Refuted by** `no completion` behind a USB3 hub again: then the device needs something more than these
three, and the next reading is its port status after reset, logged in full.

## 45. Section 43's card on the Pi 2 (2026-10-07): the key layout confirmed; and `dwc2`'s hub port change bits, all of them acknowledged now

**The key layout, as predicted.** Four fresh joins - the boot's rejoin, one `wifi radio powercycle`, and
two replugs of the dongle - each said `pairwise key 0 in CAM entry 4`, `group key 2 in CAM entry 2` and
`SECURITY_CFG=0xcc`, and every `ping 8.8.8.8` answered (3 of 3, 2 of 2, 36 of 39 across a replug). **Not
one `did not decrypt`**, to us or to a group, in the whole session - where section 42's Pi 2 card had one
failing join in four and every working join had refused at least one group frame. One powercycle, not
the five asked for; the four fresh instances are the evidence, and more joins on the next cards add to
it. `backlog/79` stays open until they do.

**What the card found instead: `dwc2-svc: port 2 - device REMOVED`, about 9,200 times.** The operator
moved the dongle from hub port 2 to port 4. As it came out, port 2 read connected once more: `device
CONNECTED`, then `hub port 2 did not finish reset within 200 ms (status=0x0100)`, `enumeration FAILED;
nothing bound` - and from then on `port 2 - device REMOVED` on every pass of `dwc2`'s loop, in bursts
and then about once a second, until the log ended a minute later.

**Why, from the code.** A USB 2.0 hub keeps a port in its status-change bitmap while ANY of the port's
change bits is set (11.24.2.7.2: connection, enable, suspend, over-current, reset). `dwc2`'s hot-plug
handler acknowledged only the connection change, and `hub::reset_port`, which acknowledges the reset and
connection changes when a reset finishes, returned without acknowledging anything when one did not. The
pulled dongle left a reset unfinished, so the hub went on reporting port 2, and each pass read "not
connected" and logged the removal again. Which change bit it was is not in the log.

**The fix:** `hub::clear_changes` reads the port's status and clears every change bit it reports. The
hot-plug handler uses it in place of clearing the connection change alone, and the reset path calls it
when a reset times out. The status the handler acts on is the one read before the clears.

**Checked:** the Pi 2 and x86 images build and x86 passes every gate. No QEMU device models this hub.

**Prediction, Pi 2, cable out, dongle in:**
1. Section 43's card again, with more fresh joins: five `wifi radio powercycle`s, each left to finish,
   each followed by `ping 8.8.8.8`; no `did not decrypt`.
2. Then the dongle moved between hub ports a few times, quickly, as before. Each move logs one `device
   REMOVED` and one `device CONNECTED` per port touched, and nothing repeats. A move that catches a port
   mid-reset logs the reset failure once and goes quiet. **Refuted by** any repeated `device REMOVED`
   for a port with nothing in it.

## 46. Section 45's card - the operator's physical chaos on the Pi 2 (2026-10-07): the flood gone, the keys hold; a port reset before the plug had settled, now debounced

**Run:** cable out, dongle in; seven `wifi radio powercycle`s, and the dongle pulled and moved between hub
ports 2 and 4 about ten times - twice in the middle of a powercycle (the download stopped, `the dongle is
no longer bound`, and `wifi-usb` ended cleanly).

- **Section 45's fix holds:** `device REMOVED` 9 times in the session, once for each pull. No repeat.
- **Section 43's key layout holds:** about fifteen fresh joins - powercycles, replugs on both ports,
  rejoins from the key file - every one `CAM entry 4`, `CAM entry 2`, `0xcc`, and **no `did not decrypt`
  anywhere**. Every `ping 8.8.8.8` answered. With section 45's four, about nineteen fresh joins and none
  refused, against about four in fifteen before section 43.
- **Every pull mid-powercycle recovered** on the other port: bound, the chip up cold, rejoined, pinged.
- **One move did not recover** - the operator's "didn't recover fully". At 14:28:38 the dongle went into
  port 2: `device CONNECTED`, then `hub port 2 did not finish reset within 200 ms (status=0x0101)` -
  connected, powered, NOT enabled - `enumeration FAILED; nothing bound`. The dongle sat there unbound for
  28 s until it was moved again. It was really in the port (0x0101 is connected), so this is not section
  45's case of a pull during a reset.

**Why, read against USB 2.0 and Linux.** USB 2.0 7.1.7.3 requires at least 100 ms between detecting a
connect and signalling reset, for debounce and power-settling, the timer restarting on any disconnect.
Linux's `hub_port_debounce` reads the port every 25 ms and waits for the connection to hold for 100 ms,
up to 2 s. `dwc2` reset the port the moment the change arrived, while a hand-pushed plug was still making
and breaking contact. And it gave the reset 200 ms - Linux's time for one long reset - where Linux's
bound on the wait (`HUB_RESET_TIMEOUT`) is 800 ms.

**The fix:** `hub::debounce`, Linux's debounce, before a hot-plugged device is reset; a connection that
has not settled within 2 s is said once and treated as not connected until the port changes again -
what Linux does on `connect-debounce failed`, and the cost is the same: a plug that settles only after
2 s is not seen until it is moved. The reset wait is 800 ms. Both are
bounded, and both are the specification's and Linux's numbers rather than new ones. The debounce costs
the `dwc2` loop about 100 ms per plug-in; a device present at boot is not debounced (the boot survey is
unchanged).

**Checked:** the Pi 2 and x86 images build and x86 passes every gate. No QEMU device models this hub.

**Prediction, Pi 2, the same physical chaos:** every move ends bound on the new port, with no `did not
finish reset`; a plug pushed in slowly may show `the connection did not settle within 2 s` once, then
bind when it settles. Powercycles and replugs as before: no `did not decrypt`, no repeated `device
REMOVED`.

**That card's run (Pi 2, 2026-10-07): as predicted.** The same physical chaos - five moves between hub
ports 2 and 4 and powercycles: five `device REMOVED`, five `device CONNECTED`, every move bound on its new
port, and no `did not finish reset`, no `did not settle`, no failed enumeration. Seven joins, no `did not
decrypt`; pings 2/2, 3/3, 2/2, 3/3, 3/3 and one 3 of 5 just after a move. No panic.

## 47. A 1000-round chaos run on the Pi 2, and the 18 `selfcheck` failures after it: the ARMv7 page-table arena (2026-10-07) - a kernel change, operator-approved; hardware-verified on the Pi 2

**The run.** Cable out, dongle in: `chaos max-carnage 1000` - 1000 rounds, 6405 kills, no kernel panic,
every service back - then `selfcheck`: 517 run, **18 failed**. Every failure was one cause: an on-demand
program - `greet`, `upper`, `roster`, `copier` - could not be started, `supervisor: spawn '...' from image
FAILED (InvalidArgument)`. The same refusal hit chaos itself from its first round: 982 of its 1000
memory-pressure spawns, and at times the respawn of `dwc2`, `events`, `shell` and `net-stack` (173 times),
which came back late rather than at once.

**Not memory.** `observe` read `RAM: 9 MiB used / 921 MiB total` during the run. The kernel's own line
beside each refusal said `LoadFailed(MapFailed(FrameAllocFailed))`, and on ARMv7 that comes from
`PageTable::new`: every address space's 16 KiB root table comes from a fixed static arena, sixteen of them
(`L1_TABLES`), not from RAM.

**Why sixteen stopped being enough.** The boot loader selftest built a page table to prove the loader and
dropped it, keeping one root for the life of the machine (`arm32: loader PASS` is in this boot). This
branch made `wifi-usb` the Pi 2's fourteenth resident service. That left one root for everything
transient; during `selfcheck` `recorder` held it, and every program after it was refused. The arena's own
comment sized it for "the concurrent-live set plus the brief overlap of a dying and its replacement", and
the live set had grown past it.

**The change (kernel, `arch/arm` only):**
- `PageTable::discard` gives back an address space built and never run - its pages, L2 tables and L1
  root, the kill path's two steps - and the loader selftest calls it: `loader selftest's page table
  returned - 7 page(s) and its L1 root`.
- `L1_TABLES` 16 -> 32 and `L2_TABLES` 128 -> 256: 768 KiB static, the bound still visible (26.6.1).
- An exhausted arena now says so where it is known, on the first refusal and every 64th: `the L1 arena is
  full - all 32 tables in use (a bound of this arena, not of RAM)`. The spawner still sees
  `FrameAllocFailed`, and the supervisor `InvalidArgument`; a distinct error would add a variant to the
  SDK's public `Error`, which every `match` and the surface `backlog/71` will freeze would feel, so it is
  the kernel's line that names the cause.
- `chaos max-carnage`'s report counts the memory-pressure spawns refused beside those fired, and says
  plainly when none ran (`services/chaos`).

**Found while proving it, not changed:** `chaos spawn-storm` can start only one `mem-pressure` - the
kernel refuses a second task under a live name (`rejected: already running`) - so it cannot reach the
task-pool or memory ceiling it reports on, on any board. Its verdict reads PASS at spawn #2.

**Checked:** the Pi 2 and x86 images build and x86 passes every gate; `audits/unsafe-audit.md` records the
one new `unsafe` block (`discard`). QEMU raspi2b: the kernel boots, the selftest returns its table, the
services run, `observe now` runs. The exhausted-arena line has not fired in QEMU - nothing there spawns
enough distinct tasks.

**Prediction, Pi 2, the same card as before:** `selfcheck` with no failures from a refused spawn; then a
short `chaos max-carnage 100` whose report shows few refused spawns and not the "none ran" note, and no
`arena is full` line in the log.

**The card's run (Pi 2, 2026-10-07), the chaos half.** `loader selftest's page table returned` at boot.
`chaos max-carnage 1000`: 6726 kills, no kernel panic, every service back, the dongle receiving
afterwards; report `1000 spawns (999 refused)` - and every refusal in the kernel log is `rejected: already
running`, the by-design one (the first `mem-pressure` runs and holds memory; later ones are refused by
name). No `FrameAllocFailed`, no `arena is full` - against 1799 arena refusals on the same card before the
change. `selfcheck` not yet run on this image.

**And the `selfcheck` half, on the same image after the chaos run:** `ran 517, failed 0, skipped 1` -
the 18 failures gone - and hot-plug of the USB stick and the keyboard working, by the operator. Section
47's change is hardware-verified on the Pi 2.

## 48. The Pi 4 with `/wifi.radio` left on `usb` and no dongle in: `powercycle` waited on a join that was never coming (2026-10-07) - the watch asks now; built, not yet on hardware

The Pi 4 card for this branch's `xhci` and `net` changes, the dongle not plugged in. The stick still held
`/wifi.radio` = `usb` from section 37's card. After `wifi radio powercycle` the onboard driver said
`/wifi.radio names usb - another radio is in use; this one does not rejoin`, as section 37 verified it
should, and the shell printed `radio up, joining` and waited - bounded at 90 s, but waiting on time for
something the driver had already decided against (Commandment VIII).

**The change (shell):** the power-cycle watch, once the radio is up and not joined, asks it ONCE whether it
is the one in use (`OP_USE`, which every radio answers). `USE_NOT` ends the watch at once - `powercycle
succeeded - the radio is up, and does not rejoin: /wifi.radio chooses the other radio. wifi hardware use
onboard makes this one the one in use` - a success, since the cycle did its job.

**Open, the operator's question:** whether a radio the choice names but that is not present should leave
the machine without WiFi at all, or the one that is present should take over. `nic-driver`'s bridge and
the shell's verbs already fall back; the driver's rejoin at start does not.

**Prediction, Pi 4:** with the choice still on `usb`, `wifi radio powercycle` ends within a few seconds of
`radio up` with the line above; `wifi hardware use onboard`, then `wifi radio powercycle`, rejoins.

## 49. A radio the choice does not name stands in for a chosen dongle that is not attached, and stands down when it arrives (2026-10-07) - built, not yet on hardware

The operator's decision, after section 48: "if a wifi hardware isn't available, fall back to the onboard".
`nic-driver`'s bridge and the shell's verbs already fell back; the radio driver's own rejoin at start did
not, so a Pi 4 whose `/wifi.radio` named `usb` with no dongle in had no WiFi until someone typed `wifi
hardware use onboard`.

**Decided from a fact, never from timing.** At boot the onboard driver starts before the dongle's, whose
host reports it a few seconds later; "the dongle's driver is not running yet" is not "the dongle is not
attached", and acting on it would stand the onboard radio in at every boot and break section 37's rule
that only the chosen radio rejoins. The fact is the supervisor's: a USB host's report.

**The change:**
- **`wire::NOTE_USB_RADIO` `[0x2D, attached]`**, from the supervisor to `wifi-driver`, no reply. Sent once a
  host has reported (`UsbState::heard`), again on every attach and detach, and to a respawned driver - the
  supervisor tells whenever the pair (the driver's endpoint, attached) differs from the last it told
  (`tell_radio_of_dongle`), so no spawn or report path can miss it.
- **The serve loop** (`sdk/wifi`): a radio `/wifi.radio` does not choose holds its rejoin until it is told.
  Not attached: it rejoins in the chosen radio's place - `the radio /wifi.radio chooses is not attached -
  this one rejoins in its place`. Attached: it does not rejoin, as before. Told attached while standing
  in: it leaves the network - `this one leaves the network it held in its place` - and the dongle joins as
  it always has. Told absent again: it stands in again. The dongle's driver never stands in for the
  onboard radio, which is part of the board.
- **`wire::USE_STANDIN`**, the answer a stand-in gives to `OP_USE` and in its link status. `nic-driver`'s
  bridge stays on it as on the radio in use; the shell's power-cycle watch (section 48) ends early only on
  `USE_NOT`, so it waits for a stand-in's join. Choosing a radio (`wifi hardware use`) ends a stand-in.

**Checked:** the Pi 4, Pi 2, VisionFive and x86 images build and x86 passes every gate; with the name-map change, `osdev test identity` 24 of 24 in QEMU. QEMU raspi4b boots,
`supervisor: ready`, and the USB host's boot report arrives; QEMU has no radio behind the SD host, so the
serve loop that takes the notice never runs there.

**Prediction, Pi 4, `/wifi.radio` still on `usb`, no dongle:**
1. Boot: `wifi-driver: not rejoining yet - ... the supervisor's to say`, then `the USB radio is not attached
   (the supervisor)` and `this one rejoins in its place`, and it joins; `net` with the cable out says
   `link up via wifi`, and `ping 8.8.8.8` answers.
2. `wifi radio powercycle`: the respawned driver is told again and rejoins; the watch ends `succeeded -
   joined ...`.
3. If the dongle is plugged in at the end: `the USB radio is attached`, `this one leaves the network it held
   in its place`, and `wifi-usb` joins and carries the link.
**Refuted by** the onboard radio joining at a boot with the dongle IN and chosen - the race this is built to
avoid.

**The card's run (Pi 4, 2026-10-07), step 1, as predicted.** `/wifi.radio` on `usb`, no dongle, cable out.
At boot: `not rejoining yet - ... the supervisor's to say`, then 32 ms later `the USB radio is not attached
(the supervisor)` and `this one rejoins in its place`, JOINED three seconds after; `nic-driver: the cable
is out - the radio carries the link`, a DHCP lease and the gateway answering a ping. The keyboard and the
USB stick came up on `xhci` as before. (The twelve `fs: flash requested ... REFUSED` lines at boot are
`fs`'s own protocol selftest walking every opcode with a zero capacity injected so it cannot format -
documented beside the code that logs them, not a request from anywhere.) Steps 2 to 4 not yet run.

**Steps 2 to 4 (same boot).**
- **`wifi radio powercycle`:** the respawned driver was told again (`the USB radio is not attached`), stood
  in, and the watch ended `powercycle succeeded - joined ...`; `net` said `link up via wifi`; `ping
  8.8.8.8` 3 of 3.
- **Stand-down and stand-in, three times:** each time the operator plugged the dongle in, `the USB radio is
  attached` and `this one leaves the network it held in its place`; each time it came out, `not attached`
  and `this one rejoins in its place`. The keyboard and the stick came and went with it, bound each time.
- **`selfcheck`: 527 run, 1 failed** - `dns: ICMP to 8.8.8.8 works but no name resolves`. `net-stack`'s
  receive and transmit to `nic-driver` (ops 4 and 0) each went unanswered for about 2 s, and `nic-driver`'s
  answers arrived after `net-stack` had given up (`reply cap is dead`), with no slow or missing answer from
  the radio logged in that window. That is `backlog/66`'s signature on the Pi 4 - `nic-driver` waking late
  on core 1 - recorded there; nothing this branch changed touches that path.

**Two things the dongle's arrivals showed.**
- **The dongle's firmware download hit `cc=4`** - section 37's Pi 4 fault, parked. The onboard radio had
  stood down because the dongle was ATTACHED, and the dongle then never came up, so until it was pulled
  the Pi 4 had no WiFi. The stand-in follows "attached", which the supervisor knows; whether the chosen
  radio WORKS is its driver's to know, and nothing carries that to the onboard radio yet.
- **`supervisor: name-map FULL - dropped wifi-usb`.** The supervisor's name map holds sixteen, and every
  spawn it made was recorded in it, on-demand programs included: after `selfcheck`, `upper` and `recorder`
  held two slots for the life of the machine, and the dongle's driver arriving after was dropped (it still
  started, wired from its peers). **Fixed:** the map keeps only what the supervisor restarts (`is_watched`),
  starts for a USB device (`USB_MATCH`), or wires others to (a peer in an image row - `pong`); an on-demand
  program's capability is let go (`map_keeps`, `record_name_quiet`) - since `0a179048` after the caller
  that asked for the spawn is answered, because letting it go at once left `spawncap` with no cap.

**The Pi 4 `selfcheck` on the current image (`edd63579`): 527 run, 1 failed, the same DNS check, and split
by the operator.** With the cable OUT, `net dns google.com` resolved once (on `net-stack`'s retry) in five
tries: `net-stack`'s receive and transmit requests to `nic-driver` went about 2 s unanswered, and
`nic-driver`'s replies arrived after `net-stack` had given up, while every exchange `nic-driver` logged
with the radio was answered within 2 ms. With the cable IN, `net dns google.com` and `net dns example.com`
resolved every time. So the resolver and the UDP path are sound, and what fails is the radio-bridged path
on the Pi 4: `nic-driver` answering late on core 1 (`backlog/66`) - and, separately, the radio's network
is the guest one (`192.168.11.x`, DNS `194.168.4.100`, outside), not the cable's (`192.168.4.x`, DNS the
router). An earlier draft of this note read the late answers as landing "on a one-second grid"; those
times are `net-stack`'s own retry boundaries, not `nic-driver`'s wake, and that reading was withdrawn.
Nothing this branch changed touches `nic-driver`'s GENET path beyond comments.

**And `selfcheck` with the cable in, same boot: 526 run, 0 failed, 0 skipped** (`run: ran 526, failed 0`). The one failure on the
radio is the radio-bridged path's (`backlog/66`), not anything `selfcheck` checks being broken.

**Corrected 2026-10-08: `nic-driver` was not waking late.** A kernel flight recorder on the VisionFive
showed the request answered in milliseconds and the answer refused: a drain that found no frame was
answered with an empty message, which the kernel refused on every port but ARM32 - which is why the Pi 2
below resolved on the same network. Fixed in `e3fcf7ed`; `backlog/66` has the account.

**And the Pi 2, the same day, the cable out and the dongle carrying the link** (`nic-driver: the cable is
out - the radio carries the link`), joined to the SAME guest network the Pi 4's radio was on:
`net dns example.com` resolved every time it was asked, and `selfcheck` ran 517 with 0
failed - `PASS  dns - names resolve over a network that is proven reachable`. So the guest network and
its DNS server are ruled out as the Pi 4's cause, and so is anything in the resolver or the shared radio
bridge (`radio.rs`): what fails is specific to the Pi 4. The Pi 2 image also carries the name-map change
(`map_keeps`), seen here for the first time on that board.
