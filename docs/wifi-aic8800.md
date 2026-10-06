# WiFi on the VisionFive 2 Lite: the AIC8800D80 (design, 2026-10-04)

**Status: phases V0 to V6 DONE and verified on the board (2026-10-04 to 2026-10-05): the grant, the `dw_mmc` host, the upload, the firmware's bring-up, scan, WPA2 join, and the frame path through `nic-driver`'s radio bridge (section 4). V7 (a rekey seen on this radio) is open.** V0 is the kernel's grant (`kernel/src/arch/riscv64/sdio.rs`); V1 is the userspace `dw_mmc` host (`services/wifi-driver/src/dwmmc.rs`) and identification. The driver answers `radio down` with the reason `DOWN_NOT_BUILT` only when the bring-up stops before a station interface exists. This is the plan for the third radio in `docs/wifi.md`'s table and the
second WiFi driver. `docs/wifi.md` section 44 identified the chip from the board's own boot log; the
firmware is in `nonfree/aic8800d80/` (byte for byte what the board's vendor image loaded, with its
licence position recorded there and in `docs/licensing.md` 5a). What follows is the hardware as the
reference code describes it, what carries over from the Pi 4, what is new, and the order of the work.

Every fact below was read from a working implementation used as an executable datasheet (CLAUDE.md
26.14), never from a summary: Linux `drivers/mmc/host/dw_mmc.c` and `dw_mmc-starfive.c` (mainline and
StarFive's `JH7110_VisionFive2_6.12.y_devel` branch), U-Boot's `dw_mmc.c`, the mainline and vendor
device trees for the board, and AICSemi's out-of-tree driver as Radxa packages it (`radxa-pkg/aic8800`
at `09c65d61`, the commit whose firmware matches the board; `aic8800_bsp` for the bus and the upload,
`aic8800_fdrv` for the full-MAC protocol). Facts that are inferred rather than read are marked
UNVERIFIED, and section 9 collects them, because each is a thing the first card run must settle.

---

## 1. The answer first: how much of the Pi 4 work carries over

**Most of the security stack, and all of the policy.** The AIC8800's full-MAC firmware does
authentication, association and the 802.11 MAC itself, and - exactly like the CYW43455 firmware this
project already drives (`docs/wifi.md` 37) - **leaves the WPA2 handshake to the host.** `aic8800_fdrv`
advertises no handshake offload (no `set_pmk`, no 4-way offload feature); Linux runs `wpa_supplicant`
over it. The EAPOL frames arrive and leave as ordinary ethernet frames with ethertype `0x888E`, and keys
are installed with one firmware command. So:

| Piece | Where it is today | On the AIC8800 |
|---|---|---|
| WPA2 crypto: PBKDF2, the PRF, HMAC-SHA1 MIC, AES key unwrap | `sdk/wifi/src/crypto.rs` | **as is** |
| EAPOL key frames: parse, build, MIC check, GTK KDE | `sdk/wifi/src/eapol.rs` | **as is** |
| The four-way and group-key handshakes, rekeys included | `sdk/wifi/src/supplicant.rs` (`Handshake`, `group_rekey`) | **moved** behind a two-method `KeyPath`; each radio keeps its own short pairwise-rekey driver |
| The credential table, `/wifi.keys`, the scan cache, auto-join, every reply layout | `wifi-driver/main.rs` serve loop, over `&mut dyn Station` (moved to `sdk/wifi/src/serve.rs` on 2026-10-06, for the USB dongle; `docs/wifi-usb.md` 10) | **as is** - the AIC8800 is a second `Station` |
| SDIO protocol: CMD52, CMD53, identification, CIS | `sdk/wifi/src/sdio.rs`, behind `SdioHost` | **as is**, over a new host |
| The `wifi` utility, `nic-driver`'s radio bridge, `net-stack` | above the driver | **as is** |
| The SDIO HOST controller | `wifi-driver/host.rs` (Arasan) | **new**: DesignWare `dw_mmc` |
| Firmware upload | `upload.rs`, `armcr4.rs`, backplane (Broadcom) | **new**: AIC debug messages to the ROM |
| Control channel, scan, join, key install | `ctrl.rs`, `scan.rs`, `bcm.rs` (SDPCM/BCDC/iovars) | **new**: AIC `lmac_msg` messages |
| Received data frames | Broadcom hands over ETHERNET frames | **new**: AIC hands over raw **802.11** frames; the host converts |

`sdk/wifi`'s `Station` trait was written anticipating exactly this: its header names "the VisionFive 2's
AIC8800" as the next station. That expectation holds.

## 2. The hardware

**Host controller.** The JH7110's second SD/MMC host, a Synopsys DesignWare Mobile Storage Host
(`dw_mmc`), at `0x1602_0000` (`mmc1`). The first, `0x1601_0000`, is the SD card the board boots from
and is never touched. `mmc1` is wired to a soldered module: non-removable, 3.3 V signalling, SDR "high
speed" at up to 50 MHz (49.5 MHz in practice), no UHS and no tuning. PLIC interrupt 75.

**Clocks, reset and pins** - all in SHARED blocks (the system clock generator at `0x1302_0000`, the
system GPIO block at `0x1304_0000`), so the kernel does them as part of the grant, the way it routes the
Pi's audio pins (CLAUDE.md 12.3, the 2026-10-03 amendment):

- clocks: `SDIO1_AHB` at `0x1302_0170` (gate, bit 31) and `SDIO1_SDCARD` at `0x1302_0178` (gate bit 31,
  divider bits 23:0); the device tree assigns the card clock 50 MHz and the log's arithmetic (div 62 ->
  399,193 Hz) says the controller sees 49.5 MHz;
- reset `SDIO1_AHB` (id 65): assert register `0x1302_0300` bit 1, status `0x1302_0310` bit 1 (reads 1
  when released). Clocks on BEFORE the release - the reset driver notes a release can otherwise hang;
- pins: GPIO 10 CLK, 9 CMD, 11/12/7/8 D0-D3, function numbers in the table the SDIO-host research
  produced (CLK 55; CMD out 57, OE 19, in 44; D0 58/20/45; D1 59/21/46; D2 60/22/47; D3 61/23/48),
  pull-up and 12 mA on all six; input and Schmitt enabled on CMD and the data pins, both OFF on CLK
  (`jh7110-common.dtsi`'s `mmc1_pins`);
- **the radio's power: GPIO 33, the old left-channel audio pin** (the Pi audio research noted it; StarFive's
  vendor device tree confirms it as `gpio_wl_reg_on`, and disables the PWM-DAC that used it). The vendor
  glue drives it LOW for 10 ms, then HIGH, then waits 10 ms before the first command. Mainline Linux has
  no node for it at all, which is how a board can look radio-less to a kernel that trusts mainline.

**The chip.** AICSemi AIC8800D80, SDIO vendor `0xC8A1`, function 1 = device `0x0082` (WiFi, and the ONLY
function the driver talks to), function 2 = `0x0182` (only the probe trigger, never enabled). The board
reads as chip revision 7 (`CHIP_REV_U03`), which selects the `u02` firmware set - "u02" is a file-set
name, not the revision.

## 3. The grant (kernel side) - no new responsibility

A fourth fixed device on a Pi-shaped seam, keyed by KIND, never by name (`docs/audio.md`, "No service
names in the kernel"): the VisionFive radio is `WIFI_SDIO`, the kind the Pi 4's radio already has. At
boot the riscv64 arch layer's census does what `net.rs` does for the ethernet MAC - enables the two clocks,
releases the reset, routes the six pins - and then checks that the controller ANSWERS by reading its
`VERID` (`0x6C`) and `HCON` (`0x70`) registers and printing both. Only a controller that answered is
granted: `fixed_device_present(WIFI_SDIO)`, `map_fixed_device(.., WIFI_SDIO)` -> one 4 KiB page at
`0x1602_0000` (the FIFO lives inside it, at `0x100` or `0x200`).

The power pin is the `DevicePower` seam (CLAUDE.md 12.3, syscall 54): `device_power_control(WIFI_SDIO)`
answers true on this board and `device_power(WIFI_SDIO, on)` drives GPIO 33, reading the pin back as the
Pi 4's does. The 10 ms hold-offs are the device's and live in the driver.

**MISCIS is unchanged**: no syscall, no privilege bit, no runtime role - the same argument as the Pi
audio grant. The seam itself is in place (`4f34ab63`, merged into this branch from `feat/audio` on
2026-10-04): riscv64 answered it with `false` and `None` until V0, which made it answer
for this board.

## 4. The SDIO host: `dw_mmc` behind `SdioHost`

A second implementation of `sdk/wifi`'s `SdioHost` trait (reset, park, set the operating clock, a
command, a command with data). Polled PIO, no IDMAC and no interrupts in the first version - the same
shape as the Arasan host, and the reason it is safe on a non-coherent machine with no IOMMU: the host
never points a DMA engine at memory at all.

- **Init**, after Linux's `dw_mci_probe` and U-Boot's `dwmci_init`: power on (`PWREN` = 1), reset the
  controller, FIFO and DMA (`CTRL` bits 0-2, self-clearing, bounded wait), clear `RINTSTS`, mask
  everything (`INTMASK` = 0, interrupts off), `TMOUT` all ones, `FIFOTH` for a depth of 32. Read `HCON`
  bits 9:7 for the FIFO width and `VERID` for the FIFO's offset (`0x100` below version `0x240A`, else
  `0x200`) - both read at run time, never assumed.
- **Clock change** (`dw_mci_setup_bus`): clock off, then a clock-update command (`CMD` = start |
  update-clock-only | wait-previous-data, poll `CMD` bit 31 clear); divider, update; clock on (NEVER the
  low-power bit, which stops the card clock when idle and kills SDIO interrupts later), update; bus
  width in `CTYPE`. 400 kHz for identification; the operating clock afterwards.
- **A command**: wait while `STATUS` bit 9 (busy) is set, clear `RINTSTS`, set `BLKSIZ`/`BYTCNT` and reset
  the FIFO if there is data, write `CMDARG`, write `CMD` with the response bits (R4 for CMD5 has NO CRC;
  R5 and R6 do), poll for command-done, map `RTO`/`RE`/`RCRC`/`HLE` to the trait's failure reasons. The
  first command after power-up carries the 80-clock init bit.
- **Data**, PIO through the FIFO: drain on receive-ready or data-over, fill on transmit-ready, any of
  end-bit/start-bit/data-CRC/starvation/data-timeout is an error - and after a CMD53 error, abort with a
  CMD52 write to the CCCR abort register, as Linux does. Every wait bounded (`gs::driver::wait`).
- **Never write `CMD` while bit 31 is still set** - that is the hardware-locked error (`HLE`) both
  references guard against.

**V1 as built (2026-10-04), where it differs from the plan above.** Commands only: `cmd_data` refuses by
name, and the data phase and the CMD53 abort arrive with the upload (V2). The FIFO depth was first read
from `FIFOTH`'s reset value rather than taken as the device tree's 32; that was wrong, and it is now the
device tree's (below). The thresholds written are the probe's (`depth/2 - 1`, `depth/2`, burst code 2). The host does not read `HCON` or `VERID` for itself - the kernel census prints
both, and V2's FIFO access is where the offset is needed. Before a command it waits for `CMD` bit 31 to
clear and does NOT wait on `STATUS` busy: Linux's `dw_mci_wait_while_busy` does that only for commands
with a data phase, and after an R1b (CMD7) the host waits out DAT0 busy. A failed command returns no
response and leaves the interrupt word in `last_int`, which the shared identification code prints; there
is no mapping to named reasons.

**V1 on the board (2026-10-04), first run.** Every line predicted, in order: the enable driven 0 then 1 and
read back each time, `FIFO depth 128 words`, the identification clock at 399,193 Hz (div 62), CMD5's R4
`0x20ffff00` (two functions, no memory), RCA `0x2abd`, CCCR rev `0x43` (SDIO 4, caps `0x1f`), and a CIS of
three tuples ending properly, FUNCID `0x0c` and MANFID `0xc8a1` / `0x0082`. About 330 ms from the driver's
start to `V1 done`. Afterwards the radio verbs were made to agree with the status sentence: `wifi radio on`
and `powercycle` on a radio that is not built say so and restart nothing, and the shell's radio watch calls
a chip warm only for `DOWN_TRAPPED`.

**The radio verbs on the board (2026-10-04, second run).** All five predictions held: `wifi radio on` and
`powercycle` gave the not-built sentence and restarted nothing; `off hard` cut the power and verified the
chip silent; `on` restored it, restarted the driver onto the cold chip, V1 ran again to `V1 done`, and
the watch ended on the same sentence. Nothing said `came up warm`. The same run found a bug: the FIFO
depth read 128, then 64, then 32 across the three resets, because the driver read the depth from the
`FIFOTH` watermark it had itself written at depth/2 - 1, and no reset restores that register. Linux's
`dw_mci_probe` warns of exactly this and takes the depth from the device tree, which says
`fifo-depth = <32>` on both `jh7110-mmc` nodes; the driver now does the same. The first read implied 128,
so whether this host's FIFO is really 32 or 128 is open, and 32 is safe either way. V1 sends no data, so
nothing depended on the wrong value yet; V2 would have.

**V2's first exchange on the board (2026-10-04).** The data phase is built: PIO through the FIFO at
`+0x200`, polled, every wait bounded, the FIFO reset after an error and the card told to abort by the
shared CMD53 code. Two fixed-address CMD53 helpers (`read_fifo` / `write_fifo`, Linux's `sdio_readsb` /
`sdio_writesb`) went into `sdk/wifi`, since a message FIFO register is plain SDIO. Then `aic.rs` set up
function 1 as `aicwf_sdiov3_func_init` and `aicwf_sdio_bus_start` do, woke the chip, and sent one
`DBG_MEM_READ_REQ` for `0x4050_0000`. Every line predicted, first card: block size 512 held, function 1
ready on the first read, awake on the first wake attempt (`F1 0x01 = 0x10`), header `10 00 11 d5` (so the
CRC-8 is right), 4 free buffers, then status `0x01` on the FIRST look - one block - and a configuration
packet of type `0x11`, length 20, carrying `0x0401` with 8 parameter bytes. The word is `0xf3078820`:
**revision 7 (U03), not the H variant**, so the `u02` files this directory carries are the set the
vendor driver would pick. 79 ms from `V1 done` to the answer, at the 400 kHz identification clock.

Two things this settles. **Polling works**: the vendor driver reads only from its SDIO interrupt, and this
host takes none, yet the status register named the reply on the first look - the substitution was the one
untested part of the exchange. **The framing is right on both sides**: the chip accepted the header CRC
and the message layout, and the receive side has no dummy word (the message starts right after the
4-byte header, with the vendor's extra `pattern` word before the parameters). Still at the identification clock;
whether the upload needs the vendor's 5 MHz or tolerates more is the next card's question.

**V2's second card: the three patches (2026-10-04).** The five files are embedded in the riscv64 build
(`aic_fw.rs`, hashed against the build's measurement at boot, as the CYW43455's are), the load addresses
are read from the patch table's information group rather than assumed, and each file goes in 1 KiB
`DBG_MEM_BLOCK_WRITE_REQ` messages - three-block CMD53s, the first multi-block transfers this host has
made. Every line predicted: the table gave ADID `0x00201940`, patch `0x001e0000`, extension patch
`0x0020b43c`; 2 + 31 + 13 block writes, every confirm status 0; and the first word read back from each
address matched its file (`0x000ee5fd`, `0x00004770`, `0x4c05b510`). About one second from the first
block to the last read-back.

**One number this card got wrong, recorded rather than explained (26.7).** The prediction was 2-3 s, and
it was ~1 s: the patch's 31 blocks took 651 ms, 21 ms each. Each is 1536 bytes out and a 512-byte reply in,
about 41 ms of bit time on ONE data line at 400 kHz - so either the card clock is higher than the 399,193 Hz
the driver computes, or something else is not as believed (`CTYPE` is 0 and the CCCR's bus interface reads
1-bit, so it is not a wider bus that nobody set). The likeliest is that `CIU_HZ` is not 49.5 MHz: that
figure is the vendor kernel's arithmetic from the device tree, never a measurement. If so, identification
also runs above the 400 kHz the SD specification allows before a card is selected - which this chip has
tolerated on every boot, and another might not. Open until something measures the card clock itself.

**V2's third card: the firmware started (2026-10-04).** The patch table's groups written as
`aicbt_patch_table_load` writes them (127 confirmed memory writes, the version group skipped, the
Bluetooth mode group's values replaced with the vendor driver's, 500 us after the power-on group, and the
information group's pairs to addresses 1 and 0 included because the reference includes them), then
`fmacfw` to `0x0012_0000` in 320 block writes, its first word read back, the patch configuration, and
`DBG_START_APP_REQ {0x0012_0000, 1}`. Every line predicted: the pointers read back from the chip matched
the file (`0x06090101`, `0x0016fb48`, `0x00174000`, `0x0017b57c`); the start was confirmed with boot
status 0; `F1 0x02 = 4` written. 1.1 s of table writes, 6.5 s of `fmacfw` (the same 20 ms a block as the
patches, so the clock question above stands), about 9 s from `V1 done` to the start. The started firmware
sent nothing unprompted before the driver stopped looking; whether it announces itself is V3's first
question.

**V3, in two cards - both verified on the board (2026-10-05, section 4's results below).** Built from the vendor runtime driver
(`aic8800_fdrv` at the same commit), in its order:

- **Card 1** (stage 8): the runtime driver's own wake check; the sub-id at `0x20`; `MM_SET_STACK_START_REQ`
  (`0x7B`, the confirm says whether 5 GHz is supported); `MM_GET_FW_VERSION_REQ` (`0x80`); the two RF
  messages the defaults send - the 95-byte transmit power table (`0x77`) and the RF calibration (`0x69`);
  then `MM_GET_MAC_ADDR_REQ` (`0x73`). The transmit power offset and level adjustment are not sent, because
  the vendor's defaults leave both disabled.
- **Card 2** (stage 9): `MM_RESET_REQ`, `MM_VERSION_REQ`, `ME_CONFIG_REQ` (the 112 capability bytes - one
  stream, 80 MHz, HT/VHT/HE), `ME_CHAN_CONFIG_REQ` (14 + 25 channels), `MM_START_REQ` (PHY configuration
  all zero, as the vendor's is: its PHY configuration step is compiled out), `MM_SET_COEX_REQ`, and
  `MM_ADD_IF_REQ` for one station at the firmware's own MAC.

Three differences from the vendor driver, each deliberate. **It stops at a failed confirm**: the vendor's
message sender returns 0 whatever happens, so its bring-up never stops, and a step that failed is the
next thing to look at. **It reads past what it did not ask for**: the receive now logs the firmware's
print packets (type `0x13`) as text and any unrequested message as an indication, and keeps reading for
the confirm for up to 2 s. **The channel list's transmit power is 20 dBm, not the 30 the vendor's table
holds**: Linux's regulatory code lowers 30 to the 20 its world domain allows before the list is built, and
this driver has no regulatory code, so it states the result - the lower value, which is the safe direction
to be wrong in. The vendor may also send a second channel list from its regulatory notifier; this sends one.

**The bytes are tested on the host before any card.** The frame builder, the patch table walk and every
parameter block are in `aic_wire.rs`, which names nothing outside `core`; `scripts/host_test_check.py`
compiles it with `rustc --test` on every `osdev build` (it is in `EXTRA_CHECKS`). Its vectors include the
values the board has already confirmed - the header CRCs the chip accepted, the patch table's addresses -
so a later edit that moves a byte fails a build rather than a flash.

**V4's first card, verified (2026-10-05): one scan at bring-up, logged.** Stage 10 sends
`SCANU_START_REQ` (`0x1000`, task 4) for every channel in the list stage 9 sent, with one empty SSID and
the broadcast BSSID, and reads until the scan ENDS. The vendor driver's order, which is not what the
names suggest: the request's own confirm is `SCANU_START_CFM_ADDITIONAL` (`0x1009`; the vendor spells it `ADDTIONAL`), each network
arrives as `SCANU_RESULT_IND` (`0x1004`) carrying the whole beacon or probe response, and the end is
`SCANU_START_CFM` (`0x1001`), unsolicited. Each result goes into `sdk/wifi`'s `Scan` (deduplicated by
BSSID, the strongest kept) with its security classified from the beacon's own elements by `classify` -
the shared code applies unchanged, as section 6 expected. Up to 8 KB is read at once, since results
queue while the radio sweeps; 15 s is the bound.

**V5's bytes, written and host-tested ahead of the cards (2026-10-05), and called since by the `Station`
below.** From
the vendor runtime driver: `SM_CONNECT_REQ` (320 bytes; flags `CONTROL_PORT_HOST | WPA_WPA2_IN_USE`, the
control port's ethertype `88 8e`, the host's RSN element in its buffer), the parse of `SM_CONNECT_IND`
(852 bytes; its `ap_idx` is the AP's station index that the keys, the transmit descriptor and the control
port all name), `MM_KEY_ADD_REQ` (44 bytes, CCMP = 2, pairwise against `ap_idx`, group against `0xff`),
`ME_SET_CONTROL_PORT_REQ` (sent by the host once the keys are in - the vendor sends it only when its
supplicant authorizes the station), `SM_DISCONNECT_REQ`, and the data path the EAPOL frames ride: out as
a type `0x01` frame with the 28-byte host descriptor and the payload without its Ethernet header; in as a
type `0x00` packet with a 60-byte hardware header before a raw 802.11 frame, its LLC/SNAP header and, on
a protected frame, the CCMP header the firmware leaves in place. No message between the scan and the
connect for WPA2-PSK. An open network sets neither connect flag and offers no element.

**V4's second card, verified (2026-10-05): the radio as a `Station`, under the shared serve
loop.** After stage 10 the driver no longer answers `radio down`: stage 11 builds `aic_station::Aic` and
enters `serve_radio`, the loop the Pi 4 runs under, so `wifi scan`, `wifi list`, `wifi status`, `wifi
join`, the credential table and `/wifi.keys` are the loop's from here. It took two changes outside the
AIC8800's own files, both of them moves rather than new behaviour:

- **`serve_radio` takes a `Station`** rather than the Broadcom's bus, backplane window and session, which
  were only ever used to build one. The Pi 4 builds its `Bcm` in `service_main` and passes it in.
- **The WPA2 handshake moved to `sdk/wifi`** (`supplicant.rs`: `Handshake`, `group_rekey`, `Keys`), with
  the steps and every log line unchanged. A radio supplies a two-method `KeyPath` - send an EAPOL frame,
  install a key - which on the Broadcom is `ctrl::send_data` and `ctrl::install_key` (`join::BcmPath`)
  and on the AIC8800 is the data frame and `MM_KEY_ADD_REQ`. Section 6 planned three methods; two
  suffice, because a group key is a key install with no peer.

Both change the Pi 4's hardware-verified path, so the Pi 4 has a card of its own for them, whose prediction
is that NOTHING in its log changes. It ran on 2026-10-05 with V6's move of the radio bridge out of
`genet.rs` on the same card, and nothing did (section 10).

The AIC8800's `Station` (`aic_station.rs`), what each method sends, and where it departs from the vendor
driver on purpose (the module's header has the full reasoning):

| Method | Here | Note |
|---|---|---|
| `scan_start` / `scan_step` | `SCANU_START_REQ`; one non-blocking look per turn, every result in the read kept | the loop sleeps between empty looks; 15,000 empty looks give up |
| `scan_abort` | nothing sent; answers `false` | `SCANU_CANCEL_REQ` is known only by its place in the source's enum; the sweep finishes on the chip and is discarded |
| `join` | `SM_CONNECT_REQ` to the BSSID and channel a sweep heard; `SM_CONNECT_IND`; the shared handshake; `ME_SET_CONTROL_PORT_REQ` | a name the last sweep did not hear is swept for first, and not heard means NOT FOUND - the vendor's broadcast-BSSID form is not used, because nothing has shown the firmware accepts it |
| keys | `MM_KEY_ADD_REQ`: pairwise against `ap_idx` at index 0, group against `0xff` at its key id | as `rwnx_cfg80211_add_key` |
| `disassoc` | `SM_DISCONNECT_REQ`, reason 3 | as `rwnx_close` |
| `radio_down` / `radio_up` | `MM_REMOVE_IF_REQ` and `MM_RESET_REQ`; then the stage-9 bring-up again | the vendor driver has no radio switch; this is what `rwnx_close` and `rwnx_open` do |
| `link` | the BSSID of the last `SM_CONNECT_IND`, cleared by any `SM_DISCONNECT_IND` read; the RSSI asked each time (`MM_GET_STA_INFO_REQ`) | |
| `send` / `pull` | the 28-byte host descriptor out; in, the 60-byte header, then 802.11 to ethernet (DA = address 1, SA = address 3) | only frames with the header's `upload` flag; the CCMP header is skipped when `decr_status` says CCMP, not by the frame's protected bit |

**V6: the frame path, through the bridge the Pi 4 already had.** `nic-driver`'s radio bridge - the cable
always wins, and with the cable out its frames go to `wifi-driver` over the frame ops - lived inside the Pi
4's GENET serve loop. It is `services/nic-driver/src/radio.rs` now, moved whole and included by both GENET
and the VisionFive's `dwmac`, so the rule and the radio's bounded exchange are written once. The
supervisor's `nic_radio_bridge` fact is derived from the radio fact rather than asked of the instruction
set a second time, which took the shared-surface count from 55 to 54 (CLAUDE.md 4.1).

**The loop reads a radio nobody reads.** Before V6, nothing pulled frames on this board, and the first
link was found DOWN within 2.5 minutes (`SM_DISCONNECT_IND`, reason 1), seen only at the next `wifi
status`. `serve_radio` now waits at most 250 ms while joined, and if no frame op has read the chip in that
time it reads it itself (`IDLE_PULL_MS`): the same `pull` NET_RX makes, so a group rekey is answered and a
drop is logged when it happens. With it the link held for the whole of each run (235 reads a minute,
logged once a minute). Where `nic-driver` pulls, as on the Pi 4, it never fires.

**The receive layout, from the source (2026-10-05).** A data packet has no separate bus header: its first
word is `hw_rxhdr`'s, whose low 16 bits are the length of the frame AFTER the header. The header is 56
bytes plus 4 of SDIO alignment, 60 in all; `flags_upload` is bit 6 of byte 48, `flags_is_80211_mpdu` bit 1,
`decr_status` bits 2..4 of byte 36. The payload is a raw 802.11 frame. A CCMP frame's IV is still in
place; whether its MIC is counted in the length the source does not show (the line that would strip it is
commented out), so a body may carry 8 trailing bytes, which EAPOL and IP bound by their own lengths.
Transmit confirms come back as type `0x12` packets and are the vendor driver's bookkeeping only; nothing
goes back to the chip.

### What the board showed (2026-10-05)

Every card ran one change and its written prediction; the network names stay in the local logs.

- **V3** - the firmware's version text is `di Mar 14 2025 11:20:38 - g5c3af771` (the `di` is in the file
  itself), the MAC came from the chip, 5 GHz is supported; the radio core reports LMAC 6.9.1.1, 32
  stations and 4 interfaces; capabilities HT/VHT/HE, 14 + 25 channels; one station interface at index 0.
  Every confirm arrived first time; RF calibration takes about 1.1 s.
- **V4** - a sweep of 39 channels ends in 1.6 s with status 0. The firmware sends one `0x004f` per channel
  when not joined and a `0x0044`/`0x0045` pair per channel when joined (39 of each, every time); neither
  name is confirmed from the source, so they are numbered (`PER_CHANNEL_IND`, `JOINED_CHANNEL_OUT`/`_BACK`)
  and read without a line each. `wifi scan` and `wifi list` from the shell; a scan while joined keeps the
  link.
- **V5** - the join from `/wifi.keys` at boot and `wifi join` from the shell: connect, association on 5180
  MHz, the four-way handshake from `sdk/wifi`, both keys, the control port - 0.4 s from the connect request
  to `JOINED`.
- **V6** - DHCP, ARP and `ping 8.8.8.8` over the radio at 24 ms, `net-stack` unchanged; so the transmit
  descriptor and the CCMP receive path are both right, and the possible 8 MIC bytes do no harm. `wifi
  radio off`/`on` rejoined by itself, `hard` off and `powercycle` restarted the driver onto a cold chip and
  rejoined, and a 50-round `chaos max-carnage` (374 kills, the radio's driver among them) ended with the
  kernel alive and the link back, unprompted, about 14 s after the storm.
- **A lost link is `SM_DISCONNECT_IND`** - reason 1 for the unread link, 0 for the host's own `radio off`
  (section 9's question).

- **The cable always wins, both ways** - with a `ping` running, the cable in took the link at once
  (`the cable carries the link; the radio stands by`, a new lease on the wired network, 21 ms) and the
  cable out gave it back to the radio (a new lease on the radio's network, 24 ms), several times over.
  A switch costs one or two pings: 37 answered and 9 lost across them. The radio stays joined while the
  cable carries the frames, read by the loop's own pull, so a rekey is answered either way.

Still open: **V7**, a group rekey answered on this radio, which needs the board joined past the access
point's rekey interval; and section 9's clock question.

## 5. The AIC8800 bus and the firmware upload

**Function 1 registers** (the D80's "V3" map, all CMD52): `0x00` interrupt enable, `0x01` pending (bit
4 = awake), `0x02` to-device (wake = `0x11`, power-control hand-off = `4`), `0x03` flow control (free
1536-byte firmware buffers), `0x04` interrupt status (the receive block count lives here), `0x05` byte-mode
length, `0x07` byte-mode enable, `0x0F` read FIFO, `0x10` write FIFO. Function-1 setup: block size 512,
enable function 1, write `0x7F` to CCCR `0xF2`, write 1 to `0x07` (block mode only), then wake: `0x11` to
`0x02` and wait for `0x01` bit 4.

**A frame to the chip**: a 4-byte bus header - a 12-bit length, type byte (`0x11` command, `0x01` data),
and **a CRC-8 of the first three bytes** (polynomial `0x07`, initial 0; the D80 checks it, older chips
did not) - then a zero word, then the 8-byte message header `{id, dest, src, param_len}` and the
parameters. Padded to 4; if not a multiple of 512, a 4-byte zero tail and rounded up to whole 512-byte
blocks; written with CMD53 to `0x10`. **Only after** the flow-control register reports enough free
buffers.

**A frame from the chip**: read register `0x04`; the block count (or byte mode) says how much to read
from `0x0F`. The buffer holds packets back to back: a configuration packet (type bit `0x10`) carries a
message `{id, dest, src, param_len, pattern, params}` - note the extra word on this side; a data packet
carries a 60-byte hardware receive header and then the frame. First version polls register `0x04`; the
SDIO card interrupt is a later refinement.

**The upload** is a conversation with the chip's ROM in "debug" messages (task 1, our task id 100):
memory read `0x400`, write `0x402`, block write `0x40B` (`{addr, size, data[1024]}`, always sent full
size), start `0x40D`; each answered by its confirm. In order:

1. read `0x4050_0000` -> chip id and revision (bits 21:16);
2. the **patch table** (`fw_patch_table_8800d80_u02.bin`): a tagged list of `(address, value)` groups.
   Its INF group gives the load addresses of the next files - the table, not a constant, is the
   authority;
3. upload `fw_adid_8800d80_u02.bin` and `fw_patch_8800d80_u02.bin` to the addresses the table named, then each extension patch
   (`_ext0`) to its own;
4. write every table group's pairs with memory-write messages (the BT-mode group with the values the
   vendor driver patches in; a 500 us pause after the power-on group);
5. upload `fmacfw_8800d80_u02.bin` to `0x0012_0000`;
6. the patch configuration: read three pointers inside the uploaded image, read its version at
   `0x0012_001C` (the board logged `06090101`), write the `PTCH` header and the three `(offset, value)`
   pairs;
7. start the application: `{boot address 0x0012_0000, type 1}`, wait for its confirm;
8. write `4` to register `0x02`.

The five files are embedded in the driver's image at build time (`aic_fw.rs`), as the CYW43455's are
(`docs/wifi.md` 21); bringing the radio up does not depend on `fs`.

**One discrepancy recorded, not resolved**: Radxa's source never changes the bus clock for the D80, but
the board's log shows StarFive's build dropping it to 4.95 MHz for the upload. Uploading at a
conservative clock costs a second; uploading too fast costs a chip that half-boots. The first version
uploads slow.

## 6. Talking to the firmware

After the start, the same framing carries `lmac_msg` requests (`id = task << 10 | index`), each confirmed
by its matching id, with indications arriving unsolicited. The sequence `aic8800_fdrv` sends, and this
driver will:

| Step | Message (id) | What it gives |
|---|---|---|
| start the stack | `MM_SET_STACK_START_REQ` (`0x7B`) | whether 5 GHz is supported |
| firmware version | `MM_GET_FW_VERSION_REQ` (`0x80`) | the string for `wifi info` |
| RF setup | tx power level (`0x77`) and `MM_SET_RF_CALIB_REQ` (`0x69`); offset and adjust not sent (vendor defaults leave both disabled) | mandatory: the vendor driver aborts if any fails |
| our address | `MM_GET_MAC_ADDR_REQ` (`0x73`) | the efuse MAC |
| reset, version | `MM_RESET_REQ` (`0x00`), `MM_VERSION_REQ` (`0x04`) | |
| configure | `ME_CONFIG_REQ` (`0x1400`), `ME_CHAN_CONFIG_REQ` (`0x1402`) | the channel list |
| start | `MM_START_REQ` (`0x02`) | |
| our interface | `MM_ADD_IF_REQ` (`0x06`) | the interface index every later message names |

**Scan**: `SCANU_START_REQ` (`0x1000`); one `SCANU_RESULT_IND` (`0x1004`) per network carrying the whole
beacon or probe response - so `sdk/wifi`'s IE parsing applies unchanged - and `SCANU_START_CFM` (`0x1001`)
when the sweep ends. That maps onto `Station::scan_start`/`scan_step` one for one.

**Join**: `SM_CONNECT_REQ` (`0x1800`) with the SSID, flags "control port on the host" and "WPA2 in use",
and **the RSN element in its IE buffer** (the same element the Broadcom join hands its firmware);
`SM_CONNECT_IND` (`0x1802`) reports the outcome and the AP's station index. Then the four-way handshake
runs over EAPOL data frames, the keys go in with `MM_KEY_ADD_REQ` (`0x24`: pairwise against the AP's
index, group against `0xFF`, cipher 2 = CCMP), and **`ME_SET_CONTROL_PORT_REQ` (`0x1404`) opens the port** -
a step the Broadcom firmware does implicitly. Leaving: `SM_DISCONNECT_REQ` / `_IND`.

**The handshake is in `sdk/wifi`** (`supplicant.rs`, moved 2026-10-05). It reached the chip through
exactly two Broadcom calls, `ctrl::send_data` and `ctrl::install_key`; behind a two-method `KeyPath` it
serves both chips, and the group rekey (`backlog/64`) came with it rather than being redone.

**Data**: transmit is 802.3-shaped - a 28-byte descriptor carrying the destination, source and ethertype,
then the payload - and the firmware does the 802.11 encapsulation and the encryption. **Receive is not**:
the firmware hands over the raw, decrypted 802.11 frame behind its hardware header, and the host builds
the ethernet frame (addresses from the 802.11 header, ethertype from the LLC/SNAP header, the CCMP header
skipped). That converter is the one genuinely new piece of 802.11 this project writes, and the first
version handles only plain, unfragmented data frames - A-MSDU, fragments and block-ack reordering are
counted and reported, not silently mishandled, until something needs them.

## 7. The plan - every phase ends at something visible

Bench-only work: QEMU models neither the controller nor the chip. One change per card, each with its
prediction written first.

| Phase | Deliverable | What the log says when it works |
|---|---|---|
| **V0** | The grant: clocks, reset, pins, GPIO 33, the census | `VERID`/`HCON` printed; the window granted to `wifi-driver` by kind |
| **V1** | `dw_mmc` behind `SdioHost` (commands only; the data phase moves to V2); identification | `CMD5` answered, function count, CIS `C8A1:0082` - the card is there |
| **V2** | The upload | chip revision 7, firmware version `06090101`, the start confirmed |
| **V3** | Bring-up messages | `wifi info` shows the efuse MAC and the firmware version |
| **V4** | Scan | `wifi scan` lists the networks in the room |
| **V5** | Join: the handshake moved to `sdk/wifi`, connect, keys, control port | `JOINED` |
| **V6** | The frame path: 802.11 -> 802.3 receive, descriptor transmit, `nic-driver`'s radio bridge on this board | `ping` over the radio, `net-stack` unchanged |
| **V7** | Rekeys, inherited from the shared handshake | the `backlog/64` lines, on this radio |

V1 is the riskiest card and the cheapest proof: a controller nobody has driven before, answering a
command from a chip nobody has powered before. Everything after it is protocol.

## 8. What this does NOT do, deliberately

- Bluetooth (function 2 and the BT patch table's purpose) - the table's BT groups are written because the
  vendor sequence writes them, and nothing else.
- 5 GHz is not excluded, but the first scan registers what `MM_SET_STACK_START_REQ`'s confirm says the chip supports
  and nothing more.
- No DMA, no SDIO interrupt, no power saving in the first version: polled, bounded, and slow enough to
  read.
- The `aic_userconfig_8800d80.txt` file the vendor driver parses host-side is not needed: it only adjusts
  transmit power and the crystal trim, and the defaults are what a board without the file gets.

## 9. UNVERIFIED - what the first cards must settle

- ~~The controller's `VERID` (so the FIFO offset) and `HCON` FIFO width.~~ **Settled by V0 (2026-10-04):**
  `VERID = 0x5342290a` - version `0x290a`, so the FIFO is at `+0x200` - and `HCON = 0x00c43cc1`, whose
  bits 9:7 read 1: a 32-bit FIFO. The same two values came back through the service's grant. The card
  clock register read `0x80000002` (gate on, divider 2) after the census enabled it, so whether U-Boot left
  it on is still not known; V1 sets the identification clock itself either way. GPIO 33 read 1 after the
  census drove it, and the driver's power cycle read back 0 then 1.
- Whether U-Boot leaves `mmc1`'s clocks, reset and pins configured. The design does all three itself, and
  all three are idempotent, so the answer only explains a result.
- The exact load addresses - they come from parsing the patch table, which is the point of parsing it.
- The byte offsets of the firmware messages: computed from the C structs assuming natural alignment.
  The first exchange (V2's memory read of the chip id) checks the framing; V3's confirms check the rest.
- Whether a received data frame keeps its 8-byte CCMP MIC at the end (the vendor driver trims nothing
  visible). Harmless either way, shown by V6: DHCP, ARP and ICMP all bound themselves by their own lengths.
- ~~How the firmware reports a lost link.~~ **Settled (2026-10-05): `SM_DISCONNECT_IND`**, reason 1 when the
  unread link was dropped and 0 after the host's own `radio off`. An access point switched off has not
  been tried.
- The clock questions in sections 4 and 5: whether `CIU_HZ` is really 49.5 MHz, and whether the upload
  needs the vendor's 4.95 MHz or tolerates more.

## 10. Also done on this board, and what is left

**The random number generator - done (2026-10-05).** riscv64's `hw_random` reads the JH7110's TRNG
(`arch/riscv64/mod.rs`), so the handshake's SNonce comes from hardware, as the Pi 4's does from its
RNG200 (`docs/wifi.md` 40), and `net-stack`'s TCP initial sequence numbers with it. The device tree gave
the block (`rng@1600c000`), its two system-top clocks and its reset; Linux's `jh7110-trng.c` gave the
registers and the sequence - seed once at boot, then one generate command per word - with the interrupt
it waits on replaced by a bounded poll of the same status bits. On the board: seeded in 30 us, the
`NO HARDWARE RNG` line gone from the join, and 320 words from `random 64` all distinct, 50.65% ones, rising
161 times in 315 steps (157.5 expected) and uncorrelated step to step. A first 48 had looked ordered (27
rises in 42); the larger sample says that was chance.

**The Pi 4 after this branch's two moves of shared code - verified (2026-10-05, built from `c87b1521`).**
The handshake now in `sdk/wifi` and the radio bridge now in `radio.rs` ran on the Pi 4 unchanged: the join
from `/wifi.keys` at boot, the cable taking the link and giving it back with a `ping` either way, `wifi
radio off`/`on` twice, `radio off hard` and `powercycle` each restarting onto the cold chip and rejoining,
and `wifi forget` then a join with the passphrase. Its access point sends message 1 twice on every join and
both are answered (`replay 1`, `replay 2`), exactly as in every Pi 4 log from before the move. The one new
line is the serve loop's own read while the cable carries the frames (`still joined - the loop read the
radio ...`), which is the point of it: the radio in standby still answers a rekey.

**Left open, recorded rather than closed (26.7):**

- **V7, a group rekey answered on this radio.** It needs the board joined past the access point's rekey
  interval, which no run so far lasted; the code is the shared `group_rekey` (`sdk/wifi/src/supplicant.rs`),
  the same code the Pi 4 runs and not yet seen on hardware there either (`backlog/64`), reached here by the same `pull` that carried DHCP and ping. Not seen is not shown.
- Section 9's clock question, and `SCANU_CANCEL_REQ`, known only by its place in the source's enum.
- `STAT` bit 27 (`SRVC_RQST` in Linux, which never reads it) is set after the TRNG's first seed; nothing
  here acts on it, and nothing has gone wrong for its being set.
