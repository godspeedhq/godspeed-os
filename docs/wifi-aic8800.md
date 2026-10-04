# WiFi on the VisionFive 2 Lite: the AIC8800D80 (design, 2026-10-04)

**Status: DESIGN; phases V0 and V1 DONE and verified on the board 2026-10-04.** V0 is the kernel's grant (`kernel/src/arch/riscv64/sdio.rs`); V1 is the userspace `dw_mmc` host (`services/wifi-driver/src/dwmmc.rs`) and identification, after which the driver answers `radio down` with the reason `DOWN_NOT_BUILT`. V2 onward is not built. This is the plan for the third radio in `docs/wifi.md`'s table and the
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
| The four-way and group-key handshakes, rekeys included | `wifi-driver/join.rs` (`Handshake`), `frames.rs` | **moves to `sdk/wifi`** behind three calls (section 6) |
| The credential table, `/wifi.keys`, the scan cache, auto-join, every reply layout | `wifi-driver/main.rs` serve loop, over `&mut dyn Station` | **as is** - the AIC8800 is a second `Station` |
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
2026-10-04): riscv64 answers it today with `false` and `None`, and V0 is the change that makes it answer
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
name, and the data phase and the CMD53 abort arrive with the upload (V2). The FIFO depth is read from
`FIFOTH`'s reset value, not assumed to be 32, and the thresholds written are the probe's (`depth/2 - 1`,
`depth/2`, burst code 2). The host does not read `HCON` or `VERID` for itself - the kernel census prints
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
the watch ended on the same sentence. Nothing said `came up warm`.

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

The five files reach the driver the way the CYW43455's do (`docs/wifi.md` 8): from the disk, through `fs`.

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
| RF setup | tx power level, offset, adjust; `MM_SET_RF_CALIB_REQ` (`0x69`) | mandatory: the vendor driver aborts if any fails |
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

**The handshake moves to `sdk/wifi`** in step V5 below. Today `Handshake::on_key_frame` reaches the chip
through exactly two Broadcom calls, `ctrl::send_data` and `ctrl::install_key`, at four sites. Behind a
three-method interface (send an EAPOL frame, install a pairwise key for a peer, install a group key) it
serves both chips, and the rekey work (`backlog/64`) comes with it rather than being redone.

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
  visible).
- How the firmware reports a lost link: `aic8800_fdrv` ignores `MM_CONNECTION_LOSS_IND`; presumably the
  loss surfaces as `SM_DISCONNECT_IND`. V6's cable-pull and AP-off tests will say.
- The `5 MHz` versus `150 MHz` clock question in section 5.

## 10. Also worth doing on this board

**A real random number generator.** The riscv64 kernel's `hw_random` is a stub, so the handshake's
SNonce falls back to a hash of the time and the nonces, as the Pi 4's did until its RNG200 is wired
(`docs/wifi.md` 40). The JH7110 has a hardware TRNG; filling that seam helps `net-stack` (TCP initial
sequence numbers) as much as WiFi, and is a separate, small kernel change for the operator to decide on.
