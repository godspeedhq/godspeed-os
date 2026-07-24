# DWC2 split-transaction XactErr: low-speed keyboard behind the LAN9514 hub

**Status: RESOLVED (2026-07-24), QEMU-validated; hardware boot pending (user away).** The XactErr
was TWO bugs stacked, both in how we drove HCSPLT / the split DATA stage - not microframe
scheduling (that lead, section 1b onward, was correctly ruled out by the data; the real cause was
below it). Branch `feat/pi2-arm32`. Driver: `kernel/src/arch/arm/dwc2.rs`.

## 0. Resolution (read this first)

Found by reading Circle's register header (`include/circle/usb/dwhci.h`) - the ratified doctrine of
grokking a *working* reference - instead of trusting a remembered bit layout.

1. **HCSPLT HubAddr / PrtAddr were SWAPPED** (commit `aa83788`). The DWC2 databook / Circle layout is
   `PrtAddr[6:0]` = the hub PORT, `HubAddr[13:7]` = the hub's DEVICE address. Our `hcsplt_for_current`
   built `1 | (port << 7)` - hub address in the port field and vice-versa. So every Start-Split was
   addressed to hub-address = *port number*, which is not a hub, so nothing ACK'd it: transmitted,
   unanswered, XactErr in **every** microframe regardless of TT mode or timing - exactly the observed
   signature. **Why QEMU never caught it:** QEMU's keyboard sits on hub PORT 1, where PrtAddr and
   HubAddr are *both* 1 and the swap is invisible; a device on port >= 2 (the real keyboard on port 2)
   is the first place the two fields differ.

2. **Multi-packet control-IN over split retrieved only the FIRST packet** (commit `d9071ef`). Once (1)
   let low-speed devices start enumerating, the 18-byte device descriptor came back as 8 correct bytes
   + 10 **stale** (leftover LAN9514 interface-descriptor data in the shared DMA buffer). HW dump of two
   *different* devices on ports 2 and 4 showed **identical** tails and `bNumConfigurations = 0x00`
   (impossible) - proof the tail was not real data. Root cause: the DWC2 does **not** auto-continue a
   multi-packet transfer over a SPLIT in buffer-DMA mode (it halts XferCompl after the first low/full-
   speed packet); the direct high-speed path is fine because the core frames + toggles multi-packet
   itself (which is why ethernet/wifi enumerated all along). Fix: `chan_dma` sequences the split
   transfer one `mps`-sized packet per `split_txn`, advancing the buffer and toggling DATA1/DATA0
   itself (matches u-boot / Circle per-packet split handling).

QEMU after both fixes: the low-speed keyboard enumerates with the full correct descriptor
(`... 27 06 01 00 00 00 01 04 0b 01`, VID 0627 PID 0001, `bNumConfigurations=01`) and reaches
`boot keyboard ready`. Everything below is the (correct, still-useful) investigation that led here.

---

## 1. The problem in one sentence

A control transfer to a **low-speed** device (USB keyboard) sitting behind the Pi 2's
**high-speed** LAN9514 hub must go through **split transactions** (SSPLIT / CSPLIT via the
hub's Transaction Translator). Our Start-Split of the first SETUP **persistently** halts with
**XactErr**, even though every channel register is verified correct. Suspected root cause:
the DWC2 split state machine needs **microframe scheduling** that we do not do.

---

## 1b. UPDATE (measured on HW 2026-07-24): it is NOT microframe scheduling

A non-perturbing trace was added (capture per SSPLIT/CSPLIT, dump after) and the SSPLIT was placed
into **each of the 8 microframes** by waiting on the `HFNUM` counter truth (`wait_for_uframe`). Result:

- **`HFIR = 0xEA60` (60000), and `HFNUM` advances 8 ticks/ms = a 125 us microframe.** The frame-timing
  domain is HEALTHY. **HFIR is ruled out.**
- **Every microframe (0..7), three full sweeps, XactErrs (`hcint=0x82`).** The Start-Split of a control
  transfer is rejected in EVERY microframe, and fast (issue.uframe == halt.uframe). **Microframe
  placement is ruled out** - it was the leading hypothesis; the data kills it.

**New shape of the problem:** the SSPLIT (a HIGH-speed host->hub transaction) fails with a fast, placement-
independent XactErr, even though (a) HCSPLT/HCCHAR/HCTSIZ are correct, (b) direct high-speed control
transfers to the SAME hub's EP0 succeed (it enumerated), (c) high-speed devices behind the hub enumerate
directly, (d) the port is reset (on the hub's truth) + enabled + shows the low-speed device connected. So
the rejection is **below scheduling** - in how the DWC2 v2.80a forms/drives the split itself, or the TT.

**MORE ruled out by measurement (2026-07-24, later):**

- **On the wire, not internal (GNPTXSTS).** On the failing SSPLIT the NP TX FIFO is empty at halt
  (`nptx` low16 = `0x80` = 128 = full depth free), so the 8 SETUP bytes DRAINED - the core **transmitted**
  the split on the bus. `gints=0x04000029` shows no error (just host mode + SOF). So the high-speed hub
  received the SSPLIT and simply **did not ACK** it (no bit 5; timeout -> XactErr). **This is a bus/TT
  rejection, not a core-internal reject.**
- **`LSpdDev` PREamble - RULED OUT.** Cleared `HCCHAR.LSpdDev` for splits (keeping `SplEna`) - **no
  change**, still `0x82`, still transmitted-but-not-ACK'd. The stray-low-speed-preamble theory is dead.

**Razor-sharp state of the problem:** the DWC2 v2.80a **transmits** a config-correct Start-Split on the
wire, and the LAN9514 hub - which ACKs direct high-speed control transfers to its own EP0 (it enumerated) -
**never ACKs the SSPLIT, in any microframe.** Not timing, not HFIR, not internal-reject, not preamble.

**Linux byte-level diff DONE (2026-07-24) - we now MATCH Linux on every dwc2 register, still XactErr.**

Diffed against Linux `dwc2_hc_init` / `dwc2_hc_start_transfer` / `dwc2_hc_init_split` /
`dwc2_hc_init_xfer`. Every field matches:

- `HCCHAR`: MC=1 (control), **LSpdDev=1** (Linux sets it even for a low-speed split - reverted my earlier
  clear), DevAddr = the low-speed DEVICE (0, the hub addr rides HCSPLT), EPType=control, MPS=8.
- `HCSPLT`: SplEna, HubAddr=1, PrtAddr=2, XactPos=ALL(0b11), CompSplit=0.
- `HCTSIZ`: PID=SETUP(3), PktCnt=1, XferSize=8.
- **Interrupt masks**: Linux writes HCINTMSK + HAINTMSK + GINTMSK before enabling ANY channel; we had them
  OFF on HW (the u-boot transcription removed them, but u-boot does no low-speed splits). **RESTORED @
  8a22056** - HW-confirmed live (GINTSTS bit 25 HChInt now propagates: `gints` 0x04000029 -> 0x06000029)
  and SAFE (the direct hub/ethernet path still enumerates). **Still XactErr.** Correct, but not the fix.

So: register-identical to Linux, the SSPLIT transmits on the wire, and the LAN9514 will not ACK it.
**Confirmed NOT:** microframe timing, HFIR, internal reject (GNPTXSTS drains = transmitted), low-speed
preamble (LSpdDev), interrupt masks. That is an unusual place to be - the dwc2 config is right.

**Remaining leads, prioritized (all need a HW boot to test):**

1. **HUB TT MODE - TESTED, RULED OUT (2026-07-24 07:37).** The LAN9514 IS multi-TT (`proto=0x2`), and
   `SET_INTERFACE(1)` **succeeds** (`dwc2: hub is multi-TT: SET_INTERFACE(1) ok`). But the split **still**
   XactErrs. It also failed in single-TT mode (the default, all prior boots). So the split fails in BOTH
   TT modes -> TT mode / SET_INTERFACE is not the fix. (Kept the SET_INTERFACE(1) - it is the correct
   thing for a multi-TT hub and matches Linux; just not sufficient.)
2. **DESCRIPTOR DMA - now the top structural suspect.** We use BUFFER DMA. Hypothesis: the v2.80a's
   buffer-DMA split path is broken/insufficient and it needs **descriptor DMA** (`HCFG.DescDMA`, a
   linked-list of `dwc2_dma_desc` per channel) for splits - which Linux may enable on the bcm2835. If so,
   NO amount of buffer-DMA register tweaking will ACK the SSPLIT. This is a large change (a different DMA
   model) and unverifiable without HW, but it is the single most plausible thing left: it would explain a
   config-perfect SSPLIT that transmits yet is never accepted, and why Linux (possibly desc-DMA) works.
   Verify first whether Linux enables `dma_desc` for the Pi, and whether the core's `hw_params` advertises
   descriptor-DMA support.
3. **HCSPLT write ORDER - MATCHED (@ 4494b52), no change.** Now HCTSIZ -> HCSPLT -> HCCHAR like Linux.
4. **v2.80a split errata / the ACTUAL Linux C** (WebFetch summaries are exhausted; the remaining answer,
   if it is not desc-DMA, needs the real source read line-by-line for a 2.80a `dwc2` quirk or a subtle
   channel-start precondition), and **HW-level analysis** - a USB bus analyzer to see whether the LAN9514
   receives a malformed SSPLIT (core bug) or a valid one it NAK/ignores (still a config/protocol gap).

**Bottom line:** every software-testable register/protocol hypothesis is eliminated. The split is now
either a **buffer-vs-descriptor DMA** structural issue (top lead #2) or a **v2.80a/LAN9514 quirk** that
needs bus-level visibility. Not another black-box register tweak.

**Instrumentation in place** (nothing to rebuild for the next test): `split_txn` trace (phase / HFNUM
issue.uframe -> halt.uframe / HCINT / GNPTXSTS / GINTSTS) + `wait_for_uframe` microframe placement + the
failure dump (now HCSPLT / HCCHAR / GINTMSK / HFIR). The current image on the SD card is the
Linux-register-faithful build; the next code step is lead #1 (hub `bDeviceProtocol` log + conditional
`SET_INTERFACE(1)`).

---

## 2. System context (so research targets the right silicon)

| Thing | Value |
|-------|-------|
| Board | Raspberry Pi 2 Model B, BCM2836, Cortex-A7 (ARMv7) |
| USB core | Synopsys DWC2 (DesignWare USB 2.0 OTG), `GSNPSID = 0x4f54280a` (**rev 2.80a**) |
| Topology | ALL Pi 2 USB ports hang off an on-board **SMSC LAN9514** = a USB 2.0 **high-speed hub + ethernet**. Hub = `0424:9514`, ethernet = `0424:ec00`. |
| Driver mode | **internal buffer-DMA** (u-boot's exact config, verified: `GAHBCFG = 0x27` = DMAEN + INCR4 + GLBLINT, no interrupt masks on HW), **polled** (ARM does not route device IRQs to userspace yet), runs in-kernel from boot + the core-0 timer tick |
| Key unlock (already done) | The DWC2 AXI DMA master only powers up after a VideoCore mailbox `SET_POWER_STATE(USB_HCD, ON)`. Without it the master never dispatches (registers read via APB, but the bus-master power domain is off). This is why the port now negotiates **high-speed 480 Mbps**. |

---

## 3. What ALREADY works (do NOT re-investigate)

- DMA master dispatches; root port enumerates at **high-speed**.
- The **LAN9514 hub** enumerates fully (`GET_DESCRIPTOR`, `SET_ADDRESS = 1`, hub descriptor,
  `bNbrPorts = 5`, per-port power + status).
- **High-speed** devices behind the hub enumerate **directly** (no split), confirmed on HW:
  - port 1 -> smsc95xx **ethernet** `0424:ec00` (comes up)
  - port 5 -> Realtek **wifi dongle** `0bda:8176`
- The split code path itself is exercised in **QEMU** (QEMU's keyboard is full-speed behind
  its hub, so it rides `split_txn` and still reaches `boot keyboard ready`). So the split
  *plumbing* compiles and runs; what QEMU does NOT model is microframe timing (see section 8).

The failure is specific to **low/full-speed devices behind a high-speed hub on real silicon.**

---

## 4. The failure, with the exact hardware evidence

Hub port status for the keyboard's ports (2 and 4): `0x0303` -> bit 9 set = **PORT_LOW_SPEED**.
So the keyboard is a **low-speed** (1.5 Mbps) device. Enumeration then fails:

```
dwc2: port 2 device status=0x00100303
dwc2: split exhausted last_hcint=0x00000082 HCSPLT=0x8000c101 HCCHAR=0x00120008
dwc2: SETUP failed
dwc2: downstream desc8 failed
```

The failure is on the very first control transfer (`GET_DESCRIPTOR(device, 8)` to address 0),
at the **Start-Split** stage, and it **persists through 24 whole-split retries**.

### Register decode (all verified CORRECT)

**`ss_hcint = 0x00000082`** (the halt status of the Start-Split):

| bit | name | set? |
|-----|------|------|
| 1 | ChHltd | yes (channel halted) |
| 7 | **XactErr** | **yes (transaction error)** |

DWC2 HCINT bit map for reference: `XferCompl=0, ChHltd=1, AHBErr=2, STALL=3, NAK=4, ACK=5,
NYET=6, XactErr=7, BblErr=8, FrmOvrun=9, DataTgl=10`.

**`HCSPLT = 0x8000c101`** (split descriptor) - CORRECT:

| field | bits | value | meaning |
|-------|------|-------|---------|
| SplEna | 31 | 1 | split enabled |
| XactPos | 15:14 | 0b11 | "all" (whole payload in one HS transaction; fine for an 8-byte SETUP) |
| PrtAddr (hub port) | 13:7 | 2 | the keyboard is on hub port 2 |
| HubAddr | 6:0 | 1 | the hub is USB device address 1 |

**`HCCHAR = 0x00120008`** (channel characteristics) - CORRECT:

| field | bits | value | meaning |
|-------|------|-------|---------|
| MPS | 10:0 | 8 | EP0 max packet (before we learn the real one) |
| EPNum | 14:11 | 0 | control endpoint 0 |
| EPDir | 15 | 0 | OUT (a SETUP is an OUT) |
| **LSpdDev** | 17 | **1** | target device is low-speed |
| MultiCnt | 21:20 | 1 | one transaction |
| DevAddr | 28:22 | 0 | freshly reset device answers at address 0 |
| ChEna | 31 | 0 | already halted when dumped |

**`HCTSIZ = 0x60080008`** (transfer size) - CORRECT:

| field | bits | value |
|-------|------|-------|
| XferSize | 18:0 | 8 bytes |
| PktCnt | 28:19 | 1 |
| PID | 30:29 | 3 = SETUP |

So: SplEna set, correct hub address + port, low-speed device bit set, address 0, a SETUP of
8 bytes. The configuration is textbook-correct, and the hub's Transaction Translator returns a
**transaction error every single time.**

---

## 5. What is RULED OUT

- **Wrong register config** - HCSPLT / HCCHAR / HCTSIZ decoded above, all correct.
- **Transient TT-busy** - the whole split is retried 24x, XactErr every time. Not a "hub busy,
  try again" transient (those clear on retry).
- **The direct (non-split) transfer path** - works for the high-speed devices on ports 1 and 5.
- **The split plumbing being broken** - it enumerates QEMU's (full-speed) keyboard.
- **DMA master / power** - solved; the master dispatches and HS enumeration works.
- **Missing SSPLIT/CSPLIT retry** - added a proper retry state machine (`4c8cb3b`); did not help,
  which is itself the signal that this is not a retry problem.

---

## 6. Working hypothesis: split MICROFRAME scheduling (and maybe HFIR)

High-speed USB divides each 1 ms frame into **8 microframes**. A split transaction is not a
single event - the host must issue the **SSPLIT in a specific microframe** and the **CSPLITs in
the following microframes**, timed against the (micro)frame counter (`HFNUM`), matching the
budget the hub's TT expects. The DWC2 in buffer-DMA mode does **not** schedule this for you;
software drives the microframe placement (odd/even frame bit, HFNUM gating, per-uframe CSPLIT
issue). We currently fire SSPLIT then CSPLIT back-to-back with **no microframe awareness at
all**. A persistent XactErr on a correctly-configured split is the classic symptom of the TT
receiving the split in the wrong microframe (or of never getting a complete-split in the window
it expects).

Secondary suspect - **HFIR (Host Frame Interval, `0x404`)**: our `reset_port` writes
`HFIR = 48000` only when the ROOT port is full/low-speed; the Pi 2 root port now negotiates
**high-speed**, so we leave HFIR at the core default. If that default is wrong for the BCM2836
PHY clock, every microframe boundary is mistimed and split scheduling cannot work. **The current
HFIR value on HW is unverified - dump it.** (Also verify whether the core auto-loads HFIR via
`HFIR.HFIRRldCtrl`.)

Note we also currently set `HCCHAR.OddFrm` **only for interrupt endpoints** (`ep_type == 3`) and
force it to 0 for control/bulk. That was correct for direct transfers, but split scheduling may
need the odd/even frame bit set for control splits too - part of the same scheduling question.

---

## 7. Specific questions to answer

1. In DWC2 **buffer-DMA** host mode, does software have to place SSPLIT/CSPLIT into specific
   microframes, or does the core do it automatically? (Expected: software, via `HFNUM` +
   `HCCHAR.OddFrm` + issuing CSPLITs on the following microframes.)
2. What is the exact microframe schedule a hub TT expects for a **control** transfer to a
   low-speed device? (SSPLIT in uframe N; CSPLIT starting uframe N+1, retried through +2/+3;
   how many CSPLIT attempts; what NYET vs ACK vs XactErr mean at each step.)
3. What should **HFIR** be for the high-speed root port on the BCM2836 (PHY clock 30 vs 60 MHz;
   value ~ number of PHY clocks per 125 us microframe), and does the DWC2 auto-load it?
4. Does a split specifically require `HCCHAR.OddFrm` (or an explicit "schedule in the next
   (micro)frame" step reading `HFNUM`) even for a **non-periodic** control transfer?
5. Does XactErr here mean "no valid TT response in the expected microframe" (a timing reject),
   or a genuine bus error? (Confirm against the databook's XactErr definition for splits.)
6. Is there any TT setup step we are missing (e.g. hub `SET_FEATURE(PORT_RESET)` timing, or the
   single-TT vs multi-TT nature of the LAN9514) that affects split acceptance?

---

## 8. How to test (IMPORTANT: HW-only)

**QEMU cannot validate the fix.** QEMU's DWC2 model does not enforce microframe timing, so a
full-speed device "works" through the split path with no scheduling at all. QEMU can only prove
we did not **regress** the direct/HS path and the enumeration plumbing. The authoritative test is
the **real Pi 2 with a USB keyboard plugged in**, reading the serial console
(`build/serial_output.log`), success = `dwc2: boot keyboard ready` then typing echoes at `gsh>`.

Build + deploy loop: `python scripts/arm_build.py --release` (NO `--qemu` = hardware DMA alias +
no interrupt masks), copy `build/kernel7.img` to the SD card's FAT32 (`bootfs`) partition, boot,
capture serial at 115200 8N1.

---

## 9. Where the code lives (dwc2.rs)

| Symbol | Role |
|--------|------|
| `hcsplt_for_current()` | builds the HCSPLT value (SplEna + hubaddr 1 + port + XactPos=all) from `SPLIT_PORT` |
| `SPLIT_PORT` (static) | hub port of the current device if it needs split (0 = direct). Set in `enumerate_downstream` for a non-HS device; cleared by `select_device`; also set in `poll()` for the keyboard |
| `split_txn(...)` | **THE function to fix.** SSPLIT then CSPLIT-poll + retry state machine. Currently no microframe scheduling |
| `chan_program(..., hcsplt)` | programs HCINT/HCSPLT/HCTSIZ/HCDMA/HCCHAR and enables the channel. `HCCHAR.OddFrm` chosen here (currently interrupt-EP-only) |
| `chan_dma(...)` | routes through `split_txn` when `hcsplt != 0`, else the direct path |
| `poll()` | the core-0-tick keyboard interrupt-IN poll; also split-aware (bounded ISR wait) |
| `enumerate_hub()` / `enumerate_downstream()` | walk hub ports; compute `split_port = (not high-speed) ? port : 0` |
| `reset_port()` | writes `HFIR = 48000` only for a full/low-speed ROOT port (see the HFIR suspect) |

**One-shot diagnostic already in place:** `split_txn` prints
`dwc2: split exhausted last_hcint=... HCSPLT=... HCCHAR=...` on enumeration failure. Extend it to
also dump `HFIR` (`0x404`) and `HFNUM` (`0x408`) to test the scheduling hypothesis directly.

---

## 10. Reference material to read

- **Linux kernel `drivers/usb/dwc2/`** - the split scheduler is the gold reference:
  - `hcd.c`: `dwc2_hc_init_split()`, `dwc2_hc_set_even_odd_frame()`, `dwc2_assign_and_init_hc()`,
    `dwc2_hc_start_transfer()` - how HCSPLT/OddFrm are set and how the uframe is chosen.
  - `hcd_intr.c`: `dwc2_hc_chhltd_intr_dma()`, the XactErr / NYET / ACK handling for splits, the
    complete-split retry counting.
  - `hcd_queue.c`: the periodic/split **microframe budgeting** (the "TT bandwidth" scheduler).
- **USB 2.0 spec, chapter 11 (Hub)** - sections 11.14 (Transaction Translator), 11.17-11.20
  (Split Transactions, the SSPLIT/CSPLIT microframe pipeline and budgeting).
- **Synopsys DWC2 databook** - the HCSPLT register, "Host Programming Model" -> split transactions,
  and the periodic/non-periodic channel scheduling sections. XactErr definition.
- **Bare-metal Pi references** - Circle (`rsta2/circle`) `lib/usb/dwhcidevice.cpp` (does splits,
  interrupt-driven) and USPi. Check whether u-boot's `dwc2.c` does splits at all (it may only
  enumerate direct devices, in which case it is not a reference here).

---

## 11. The one-line takeaway for whoever picks this up

Everything up to the split works on real hardware. The split is configured correctly and fails
with a persistent XactErr because we issue SSPLIT/CSPLIT with **no microframe scheduling**.
Implement the DWC2 split microframe schedule (read Linux dwc2), verify/fix **HFIR** for the
high-speed root port, and test on the Pi 2 with a keyboard - QEMU cannot prove it.
