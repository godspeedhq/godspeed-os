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
| `OP_CONTROL_ONCE` 0x22 | as `OP_CONTROL`, attempted exactly once | as `OP_CONTROL` |
| `OP_BULK_IN` 0x23 | `[op]` | `[op, status, transfer...]` - the held bulk IN transfer, or none; the host's IN armed again |
| `OP_BULK_OUT` 0x24 | `[op, out, transfer...]` - `out` the OUT endpoint's position in the configuration descriptor | `[op, status]` |
| `NOTE_BULK_IN` 0x2E | sent by the HOST to the driver, `[note]`, no reply cap: a transfer is held | none |
| `NOTE_RADIO` 0x2F | sent by the HOST to the driver, `[note]`, no reply cap | none |

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
