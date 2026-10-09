<!-- SPDX-License-Identifier: GPL-2.0-only -->
# WiFi: two problems, one of which is not a driver

**Status:** sections 1-12 are the design, written on `feat/wifi-driver` before any code, deliberately,
because the sizing conclusion below would have been discovered four weeks late otherwise. Sections 13
onward are the bring-up record, dated. As of 2026-10-04 the Pi 4 radio scans, joins WPA2 networks with the
handshake run on the host (hardware 2026-09-29, sections 40-41), and carries `net-stack`'s frames behind
`nic-driver` with the cable winning (41); it answers a group-key rekey and has pairwise rekey code (42,
neither yet seen on hardware, `backlog/64`), keeps derived keys in `/wifi.keys` (43), adopts a running
firmware on respawn (46), cuts and restores the chip's power through its own grant (47-52), holds a lease
on the Arm clock for the upload (57), and shares its station half with other radios through `sdk/wifi`
(59). The VisionFive 2 Lite's AIC8800 scans, joins and carries frames behind `nic-driver` (V0-V6,
`docs/wifi-aic8800.md`, 44). The USB dongle is its own service, `wifi-usb` (`docs/wifi-usb.md`). All three
share `sdk/wifi`, the serve loop included since 2026-10-06. `utilities/56_wifi.md`
is the command surface and its status section is the current truth. Where a design section below and a
later dated section disagree, the later one is what was built.

**The one-line answer.** Once a station is associated, a WiFi link is a frame source, and this project
already has a NIC-agnostic frame interface that two unrelated drivers speak. `net-stack` needs no
change at all. Everything hard about WiFi happens *before* the first frame.

---

## 1. The hardware, measured rather than assumed

The five machines, with what each actually has. Two of these corrected an assumption held going in, and
one is still open.

| Machine | WiFi | Attach | Chip | MAC model |
|---|---|---|---|---|
| **Raspberry Pi 4** | **Onboard** | **SDIO** (Arasan `mmc1`) | Cypress **CYW43455** | **Full-MAC** |
| **Raspberry Pi 2** | USB dongle | USB (via `dwc2`) | Realtek **RTL8188CUS**, `0bda:8176` | **Soft-MAC** |
| **VisionFive 2 Lite** | **Onboard** (settled 2026-09-30, section 44) | **SDIO** (DesignWare `dw_mmc` at `16020000`, `mmc1`, non-removable) | AICSemi **AIC8800D80**, SDIO `C8A1:0082` + `C8A1:0182` | **Full-MAC** (its Linux driver is `fmac`) |
| **Dell Wyse 5070** | None | - | - | - |
| **HP T630** | None | - | - | - |

**The Pi 2's dongle was already identified, by this project, on this hardware.**
`bugs/3_DWC2_SPLIT_XACTERR_LOWSPEED_KBD.md` records the hub enumeration: port 1 is the `smsc95xx`
ethernet at `0424:ec00`, and **port 5 is a Realtek WiFi dongle at `0bda:8176`**. It enumerated all
along, as a high-speed device needing no SPLIT transaction. So there is no unknown here and nothing to
plug in and look up: the chip is an RTL8188CUS (the RTL8192CU family).

**The VisionFive 2 was the open question, and it is settled - by the board, as this paragraph asked.**
When this was written nothing in the repository claimed the board had WiFi, and the paragraph refused
to assert it from a spec sheet. The vendor's own Linux boot log from the board (2026-09-07, kept in the
operator's captures) answers it: an AICSemi AIC8800D80 WiFi/Bluetooth combo on SDIO, the third radio.
Section 44 has the evidence and what it means for a port. This document now plans for three radios, two
of them full-MAC.

**Neither x86 box has WiFi, which is a convenience.** (Neither has an onboard radio; the USB dongle,
`docs/wifi-usb.md`, has since run on the T630 through `xhci`.) It means the QEMU-first development pattern that
carried the whole network stack (`e1000` in QEMU, RTL8168 on the bench) **does not transfer here**.
QEMU has no model of either of our radios. Every line of this work is bench-only, on ARM, which is a
real change in iteration cost and is the strongest argument for the phasing in section 7.

---

## 2. A WiFi driver is two problems, and only the first is a driver

This is the whole sizing argument, so it goes before anything else.

**Below association** is an ordinary driver, and this project has written five of them. A transport
(SDIO or USB), a firmware image pushed into the chip, a command/event channel, bounded waits, an IRQ.
Nothing here is conceptually new; `ahci.rs`, `sdhci.rs`, `usbdisk.rs` and `xhciblk.rs` are all this
shape.

**Above association** is not a driver at all. It is the 802.11 MAC state machine plus a supplicant:
scanning and channel management, authentication and association, the WPA2 four-way handshake, pairwise
and group key installation, rekeying, and deauthentication handling. In Linux this is mac80211 plus
wpa_supplicant, and the second of those is on the order of a hundred thousand lines.

That second half is **policy** by this constitution's own definition (§26.10), which settles where it
cannot go: not the kernel. It is also not obviously part of the driver. Section 6 argues it should be
split out, and that the split is what keeps a credential away from a device driver.

**And above BOTH of them, the PROTOCOL does not change.** `docs/networking.md` §5 states the contract:
*"Raw frames only... The frame interface is the entire contract."* The ops are `0x10` INFO, `0x11` TX,
`0x12` RX, and they are already proven agnostic across four drivers on four ISAs - e1000, RTL8168,
LAN9514 and GENET - with `dwc2` serving them alongside the block protocol on one endpoint. A radio that
presents those three ops carries DHCP, ARP, ICMP, DNS and TCP with no change to any of them.

### Four names, and why there is no `wifi-stack`

- **`wifi`** - the utility, the verb a person types (`utilities/56_wifi.md`).
- **`wifi-driver`** - the service that owns the radio, named to the same convention as `nic-driver`
  and `block-driver`.
- **`keyring`** - designed to own the credential and never built (section 6); the driver holds derived
  keys and `/wifi.keys`.
- **`wifi-usb`** - the USB dongle's driver (`docs/wifi-usb.md`).
- **`nic-driver`** - unchanged, and it is the one that matters here: the link front end the radio sits
  behind.

**And there is deliberately no `wifi-stack`.** A second stack means a second ARP, a second IPv4, a
second ICMP and eventually a second TCP - two implementations of one protocol, which is Commandment III
broken at the largest scale available: two sources of truth that will drift, with every bug needing
fixing twice. `docs/networking.md` already names the property that forbids it - *"the NIC-agnostic frame
interface is exactly what makes this clean, the stack never knows the difference"* - and describes it as
load-bearing rather than theoretical. Nothing about a radio pulls a second stack into existence (§26.2),
and the coupling a fork would route around turns out not to exist at all.

`net-stack` carries the literal string `"nic-driver"` in three code paths -
`ctx.reacquire_by_name("nic-driver")`, the peer-death recovery §14.3 requires - and again in its
contract's `ipc_send` list. From that it looks as though a second link driver must mean editing
`net-stack`.

**It does not, and the reason is already in the tree. `nic-driver` is not the Ethernet driver - it is
the LINK FRONT END, and on the Pi 2 it owns no registers at all.** That board's ethernet is a CDC-ECM
USB adapter behind the **`dwc2` service**, and `nic-driver` reaches it **by IPC over ops `0x10`/`0x11`/
`0x12` - the very interface it serves upward.** It is a client of the frame interface and a server of
the frame interface at the same time, and its own source says so.

So the shape is this, and it has been in production on four boards:

```text
  net-stack                        IP and up. One implementation. Always asks for "nic-driver"
      |  frame interface           0x10 INFO / 0x11 TX / 0x12 RX
  nic-driver                       the LINK front end - one name, per-board backend
      |- e1000 / RTL8168           MMIO directly            (x86)
      |- GENET                     MMIO directly            (Pi 4)
      |- dwmac                     MMIO directly            (VisionFive)
      `- IPC to `dwc2`             no registers at all      (Pi 2)
```

**`wifi-driver` is the fifth row of that table**, and it is the row the Pi 2 already demonstrates. No
change to `net-stack`, no second name above the front end, no kernel change, and no new abstraction -
the indirection the question is reaching for **exists and is called `nic-driver`.** It is a role name
that happens to read like a device name.

**Two earlier claims in this document were wrong and are withdrawn here rather than quietly edited.**
The first said `net-stack` needed zero changes, which was right by luck and wrong by reasoning. The
second said it needed a name change after all, and proposed a candidate-name list or a supervisor-wired
role indirection. Both proposals are unnecessary: they were answers to a coupling problem this project
solved when the USB ethernet adapter arrived, and I reached them by reading `net-stack`'s contract and a
top-level grep instead of following what the Pi 2 actually does. The candidate list and the role name
stay recorded as the shapes to reach for **if** two simultaneous links are ever wanted, because that is
the one thing the front-end pattern does not answer.

**What it costs, stated honestly.** On the Pi 4 both GENET and a radio exist, so `nic-driver` must
choose a backend at RUNTIME rather than at compile time - the Pi 2's choice is settled by the
architecture it is built for. That runtime choice is the "one link at a time" policy, and it landing in
a driver is worth a second look during phase 5: the supervisor is the natural home for a wiring
decision, and `nic-driver` is the convenient one. Flagged rather than settled.

### What "link up" means for a radio, which genuinely costs nothing

This is the part where the design pays off. Op `0x10` INFO already reports a MAC and a link state, and
`net-stack` already **self-configures on link-up** - hardware-proven on the T630, where `ping` rides an
unplug and replug. So:

**Associated IS link-up.** Before association `wifi-driver` reports link-down; the moment association
completes it reports link-up, and `net-stack`'s existing path fires DHCP without knowing why the link
appeared. No new op, no new field, no new state machine - a radio looks exactly like a cable being
plugged in, which is what it is.

It also settles who initiates. **Not `net-stack`**: association is driven by the `wifi` utility talking
to `wifi-driver`, and `net-stack` is purely reactive. It never learns that wireless exists.

### One link at a time, deliberately

The Pi 4 has GENET ethernet **and** a radio, so both drivers can run at once. Two simultaneous links is
routing: interface selection, source-address selection, metrics, and a policy for which one wins.
That is a real feature and §26.2 says it is not pulled into existence by anything here, so **v1 has one
active link at a time**, chosen explicitly - `wifi join` means "make the radio the link". Multi-homing
is out of scope with that as the reason, rather than unmentioned.

> **Amended 2026-09-29 (phase 5): the choice is the CABLE's, not a command's.** The operator, having
> joined the radio and pulled the cable expecting the frames to follow: *"cable always wins. unplug the
> cable, switch to wifi automatically."* So `wifi join` makes the radio AVAILABLE, and the cable decides:
> while the PHY reports a link the frames go over GENET; when it does not, they go to the radio if it is
> joined; when the cable comes back, so do the frames. One link at a time still, and still no routing -
> the rule is a single comparison in `nic-driver`'s genet backend (`Carrier`), and it lives there because
> `nic-driver` IS the link front end. Section 41 has what it cost.

---

## 3. Full-MAC versus soft-MAC decides the size of the project

Our two radios sit on opposite sides of the most consequential line in wireless.

**The CYW43455 (Pi 4) is full-MAC.** Its firmware runs the MAC state machine. The host sends commands
over a control channel - set SSID, set security, join - and receives events. The four-way handshake can
be performed *by the firmware*, given the PSK. So the host side is: bring up SDIO, upload firmware,
speak a command protocol. **No 802.11 frame construction and, in phase 1, no cryptography at all.**

**The RTL8188CUS (Pi 2) is soft-MAC.** The host builds and parses management frames, runs the
handshake itself, derives and installs keys, and implements CCMP. Everything mac80211 does, we would
do.

**Therefore: the Pi 4 first, and it is not close.** The counterintuitive part is worth stating plainly,
because the instinct runs the other way: the board that needs an entirely new SDIO stack is *far* less
work than the board whose radio is already enumerated on a USB stack we already own. Attach is the easy
half; MAC model is the hard half. Choosing by attach would have been the expensive mistake, and it is
the mistake this document exists to have avoided.

---

## 4. The enabler nobody remembered: `sdhci.rs` already exists

`services/block-driver/src/sdhci.rs` is 25 KB of working polled SD Host Controller driver for the
**Arasan** block, written in August, deliberately not compiled in. Its own header says why: on the Pi 2
the Arasan EMMC *is* the SD card the board boots from, so driving it risked writing GSFS over the boot
partition.

**On the Pi 4 that objection does not apply, and the controller is the right one.** The BCM2711 puts
the SD card on `emmc2` and the CYW43455 on the older Arasan controller. If that holds - and it must be
**confirmed against the BCM2711 device tree and datasheet before a single line is written**, not taken
from this paragraph - then a driver written for the Arasan block, which cannot be safely pointed at the
Pi 2's boot card, is pointed at exactly the controller the Pi 4's radio lives behind.

It is PIO rather than DMA, with bounded waits, and the header explains that choice: it sidesteps ARM DMA
cache coherence and needed no device-IRQ-to-userspace routing. Both of those constraints have since
been lifted on ARM, so DMA is available later as an optimisation. PIO is the right phase-1 choice
regardless - SDIO command/response traffic for association is small, and a bounded polled loop is the
§26.6 shape.

What it does **not** have is SDIO, as distinct from SD: CMD52/CMD53 (IO direct and extended), function
enumeration, the CIS tuple walk, and the interrupt-enable path. That is the actual phase-1 deliverable.

---

## 5. No crypto exists in this tree

I checked rather than assumed, and the answer is cleaner than expected: there is **no cryptography
anywhere** in `kernel/`, `sdk/` or `services/`. The only matches for `aes`, `sha1`, `hmac` and `pbkdf2`
in the whole tree are the word *aesthetic* in two comments.

WPA2-PSK needs, at minimum, PBKDF2-HMAC-SHA1 at 4096 iterations to turn a passphrase into a PMK, an
HMAC-SHA1 PRF to derive the PTK, and AES for either CCMP or key unwrap. Under §26.6.1 all of it must be
no-heap: fixed buffers, no allocator, a footprint readable off the source.

**Full-MAC lets phase 4 skip all of it.** Hand the firmware the passphrase and it does the handshake.
That is a large simplification and it comes with a trust statement that must be recorded rather than
enjoyed quietly: **the credential then lives in a closed-source firmware blob's memory, and the
handshake is performed by code we cannot audit.** Per §26.7 that is written down here, at the place a
reader would otherwise assume otherwise. It is not obviously wrong - the radio firmware is already
trusted with every frame - but it is a different claim from "GodspeedOS implements WPA2", and this
document will not make the second claim.

When crypto is eventually needed - a soft-MAC radio, or a decision to do our own handshake - it goes in
**one place, and not in the SDK until there is a second consumer** (§26.2: features are pulled into
existence). A `crypto` module inside the supplicant service, promoted to `sdk/` only when something
else needs it.

**Superseded (sections 37, 40, 59):** crypto now exists, and the host runs the handshake. The Broadcom
firmware has no supplicant (37), so the driver performs the WPA2 four-way handshake itself
(`join::Handshake` in `services/wifi-driver/src/join.rs`, section 40) and the passphrase never reaches the
firmware - only the derived temporal key does. The crypto (`sdk/wifi/src/crypto.rs`: SHA-1, HMAC, PBKDF2,
the 802.11 PRF, AES-128, the RFC 3394 unwrap) moved into the SDK when a second radio became its second
consumer (59), exactly as the paragraph above said it would.

---

## 6. Where the credential lives, which is the interesting Godspeed question

> **Superseded (2026-09-29).** The keyring service this section designs was not built. The decision the
> operator made instead - the driver derives the pairwise master key from the passphrase the moment it
> arrives, keeps up to 64 keys in its own memory, and loses them on any restart ("better that than the
> kernel crashing"; since section 43 it saves up to 48 of them to `/wifi.keys`, which a restarted driver
> reads back) - is recorded in `utilities/56_wifi.md` section 6, which is the current truth. This
> section stays as the argument that led there.

A WiFi passphrase is a **credential**, and this project has strong opinions about authority that apply
directly. Three claims:

**It is not configuration.** An ambient config file every service can read is ambient authority with
extra steps (§3.1). The PSK should be reachable only by holding a capability to it, so that a reviewer
can answer §26.9's question - what can this service do, and which capability granted it - about the
network credential specifically.

**The driver should never see the passphrase.** Split the work: a `supplicant` service holds the
credential and owns association policy; the `wifi` driver owns the chip. The driver is then a transport
for commands it does not originate, exactly as `block-driver` moves blocks it does not interpret. This
also keeps the driver restartable without re-prompting a human, because the credential outlives it.

The full-MAC shortcut in section 5 **cuts against this**, and the tension should be resolved
deliberately rather than by accident: if the firmware performs the handshake, the passphrase must reach
the chip, so it passes through the driver. The honest phase-4 position is that the split exists for the
*interface* and the secret does transit the driver, with the day-one design being the driver never
persisting, logging, or re-reading it - and the log rule is worth stating as an absolute, because a PSK
in the kernel ring buffer is a PSK in `build/putty_serial_output.log`, committed.

**Who prompts?** The shell is where authority is decided (Appendix D.4), so the passphrase is typed at
the prompt - and **the invisible-entry path already exists**: `input secret` (`docs/scripting.md` §8)
gives keystrokes that do not echo, a line excluded from both the recall ring and `/.gsh_history`, taint
that propagates across assignment, and `echo` refused and masked as `[secret]`. `input secret sealed` is
already reserved for the escalation that additionally forbids write and assignment, which is exactly
what a passphrase wants. So the prompt half of this is built; what was missing is somewhere to put the
answer.

### Decided 2026-09-27

The three questions this section left open are settled, and the reasoning is recorded because each
answer is narrower than the obvious one.

**1. Not "encrypted at rest" - capability-protected, and it SAYS so.** Encrypted with what key? A key
beside the ciphertext is decoration; a key in hardware is a TPM the Pis do not have, so the guarantee
would vary silently by board; a key derived from a master passphrase is the only honest option and it
only pays for itself at several credentials, since you would type one password to avoid typing one
password. So phase 1 stores the credential in the clear, protected by the thing this OS actually
enforces - a capability - and **prints which case it is in**, exactly as §6.4 prints the IOMMU posture
rather than assuming it:

```
keyring: 1 credential, capability-protected, NOT encrypted at rest (no master passphrase set)
```

What that protects against is any other service reading it: nothing without a capability to that
resource can. What it does not survive is someone taking the card out, and that belongs in the boot log
rather than in a footnote. A master-passphrase mode can be added later without changing the interface,
and until it is, nothing here claims a property it does not have (§26.7).

**2. A `keyring` SERVICE, not a file under `fs`.** The honest minimal alternative was to let `fs` hold
it and give the driver a file capability - zero new services, reusing the machinery §22 Test 14 already
pins on hardware. It loses on three counts: a file cap grants READ (the bytes, not "use without
reading"), PBKDF2 would have nowhere to live, and it makes credentials depend on a filesystem the
diskless case does not have. The service wins for one reason that is functional rather than aesthetic:
**a credential must outlive the driver.** If the PSK lives in the radio driver, a driver restart loses
it and re-prompts a human, which turns a supervisor restart into an outage. It is also revocable by
generation bump without killing anything.

**NOT named `credentials`.** That is the trap the `logger` -> `events` rename was about: a service
named after an abstract property becomes the dumping ground §4.4 and §26.2 exist to prevent. `keyring`
is a thing, house style is one short word, and nobody is tempted to put session management in it.

**3. Diskless means a SESSION credential, and that is a different failure model rather than a lesser
one.** Services are restartable, so a RAM-held credential dies with the keyring and the supervisor
restarts it empty - after which the radio cannot reassociate without a human. §15 is explicit that
state which must survive restart persists externally, and this cannot. That is acceptable only because
it is declared: the failure is a loud "network credential lost, retype it", never a hang, and `chaos
max-carnage` is what proves it. Declared it is a design; left implicit it is a chaos finding.

**One thing that follows from all three, worth stating as an absolute:** the passphrase is never
logged. A PSK in the kernel ring buffer is a PSK on the serial console, and this project commits
`build/putty_serial_output.log` as hardware evidence (§23.3).

**And one residual that cannot be engineered away.** A USB keyboard driver sees the passphrase as it is
typed. That is the SEC-2 residual - `CONSOLE_PUSH` holders sit inside the shell's trust perimeter
because keystrokes *are* commands - and IOMMU confinement bounds that driver's DMA, not what it reads.
Recorded here at the place a reader would otherwise assume otherwise.

**The command surface is `utilities/56_wifi.md`**, which settles the shape this implies: `connect` must
be a shell built-in, because there is one console input ring with one reader slot and the shell is the
reader, so a spawned service cannot prompt at all.

---

## 7. Phases, each ending in something demonstrable

Mirrors the shape that worked for persistence (ahci -> gsfs -> file-cap) and networking. Every phase
ends at a thing you can see on a screen, because this is bench-only work with no QEMU model.

| Phase | Deliverable | Needs |
|---|---|---|
| **0** | Settle the hardware: confirm the Pi 4 radio is behind the Arasan block and the SD card is on `emmc2`; read the device tree; settle the VisionFive question on the board | Nothing new |
| **1** | SDIO bring-up: CMD52/CMD53, function enumeration, the CIS walk. **Prints the CYW43455 chip ID** | `sdhci.rs` compiled in for aarch64, a new `HwClass` kind, MMIO + IRQ grant |
| **2** | Firmware upload and the control channel. **Prints the firmware version string the chip reports** | The blob decision (section 8) |
| **3** | **`wifi scan` lists the SSIDs in the room.** First user-visible win, and it needs no cryptography and no credential | A shell command, an event path |
| **4** | **Associated, WPA2-PSK, firmware-offloaded. DHCP lease from the existing `net-stack`** | The credential path (section 6) |
| | *Done 2026-09-29 - except "firmware-offloaded", which this firmware cannot do (37): the host runs the handshake (40). The lease came with phase 5.* | |
| **5** | The frame interface: ops `0x10`/`0x11`/`0x12`. **`ping` over WiFi, `net-stack` unmodified** | Nothing above the driver |
| | *Done 2026-09-30 - except "unmodified": `net-stack` needed one rule, for a link whose address changes (41).* | |
| **6** | The Pi 2 dongle, soft-MAC | A real 802.11 MAC and real crypto. **Deferred, with section 3 as the reason** |
| | *Taken up 2026-10-05 as its own service, `wifi-usb`, behind any USB host - `docs/wifi-usb.md`. The crypto exists by now (`sdk/wifi`).* | |

Phase 3 is deliberately placed before any credential handling. A scan is the cheapest proof that the
transport, the firmware and the event channel all work, and it is worth having that proof standing
before anything touches a secret.

---

## 8. The firmware blob: why one exists, and why it IS in this repository

**DECIDED 2026-09-27.** This section used to pose the licensing question as open. It is closed, and by
following what Linux does rather than by inventing a policy.

### Why this device needs a file when no other one did

Every device this project drives is fixed-function silicon: `e1000`, `RTL8168`, `GENET`, `dwmac`, AHCI,
xHCI, EHCI, DWC2, the LAN9514. For all of them **the registers are the interface** - write a descriptor
ring address, set a bit, frames move - and the state machine is in gates. Our driver is the whole driver.

The CYW43455 is a different kind of thing. **It contains its own processor and RAM, and no ROM firmware
for the MAC.** Until a host uploads code into it, there is no 802.11 inside to talk to: it cannot scan,
associate or encrypt. And the register-level interface to the radio is not published at all; what is
published is the protocol you speak to the firmware once it is running.

So the blob is not driver code being borrowed instead of written. **It is the program for a second CPU**,
and nobody writes it - not Linux, not the Pi's own firmware, not Windows. Everyone uploads Broadcom's.

**This project already depends on three of these**, which is the clearest way to see it is not a new
category. `scripts/deploy_pi.ps1` verifies them on every flash:

```text
pi4 firmware present: start4.elf, fixup4.dat
pi2 firmware present: bootcode.bin, start.elf, fixup.dat
```

Those are closed vendor blobs for the VideoCore processor - which on a Pi is what actually boots the
machine and hands control to this kernel. Nobody had to think about them because the Imager put them
there.

**The mental model does not change.** Enumeration finds a radio, the supervisor spawns `wifi-driver`, our
driver drives it. The only addition is what the driver does first: **`wifi-driver` is to the CYW43455 what
Limine is to this kernel.** Limine does not implement the OS, it loads it and gets out of the way. Our
driver does not implement 802.11, it loads the firmware that does and then speaks to it over SDIO.

### The licence closes a door, and that is worth knowing before anyone hopes otherwise

`LICENCE.broadcom_bcm43xx` in `linux-firmware` permits **redistributing the binary** with the Broadcom
copyright notice attached, and explicitly forbids attempting to *"modify in any way, reverse engineer,
decompile or disassemble any portion of the software."*

So for this chip, unlike every other device here, there is **no source to read AND no permission to study
the binary**. §26.14's method - read a working driver as an executable datasheet, reimplement, never
translate - has nothing to be applied to. We hand the chip its program and use the documented protocol.

### In this repository, in `nonfree/` - and why that is not Linux's answer

**This section first concluded the opposite**, on the reasoning that the Linux kernel tree carries no
blobs: they live in a separate `linux-firmware` that distributions package, and the driver calls
`request_firmware()` to read one off the filesystem. A `.gitignore` guard went in to enforce it.

**Reversed the same day, and by trying it.** `scripts/get_firmware.py` was written to fetch the files on
the owner's machine. It failed three times on one file:

1. the board-specific name is a **symlink**, so the raw URL returns the target path as text - 31 bytes
   which, written to a disk as firmware, is a radio that never starts and says nothing about why;
2. the target is `../cypress/...` - a **sibling directory**, where the resolver had taken a basename;
3. that target, `cyfmac43455-sdio.bin`, **does not exist in the tree at all.** The repository is Debian
   *packaging source*; the unsuffixed name is produced by the packaging rules at build time, and choosing
   between `-minimal` and `-standard` is a decision the packaging makes.

So a fetch script must reimplement somebody else's packaging logic and re-breaks whenever they change it.
Three failures in one sitting, by someone reading the API responses directly - every one of them would
have been a user's failure on a Tuesday with a silent radio and no clue.

**And Linux's separation is not a technical conclusion.** It is Debian's social contract and the DFSG.
Shipping a non-linked binary beside GPL code is mere aggregation, Linux itself did it in-tree for years,
and GodspeedOS is not a distribution with a package manager to lean on. The earlier claim that "not
redistributing takes on nothing" was simply false: it takes on fetch fragility, and pushes it onto every
user rather than absorbing it once.

| | decision |
|---|---|
| **In this repository** | **Yes.** `nonfree/brcm43455/`, committed. A plain `git clone` gets them - no submodule, no separate repo, no git-lfs, no fetch at setup |
| **What goes beside them** | `LICENCE`, because the notice must travel with every copy and a repository is a copy; and `PROVENANCE` - upstream URL, retrieval date, SHA-256 per file |
| **Who enforces it** | `scripts/nonfree_check.py`, in `EXTRA_CHECKS`, so every build refuses a blob that lacks either or whose digest does not match its content |
| **Size** | ~614 KB for this board. A repository that carries `.rs`, `.md`, `.py` and now `.bin` is a repository that is honest about what the hardware needs |
| **Firmware that may NOT be redistributed** | Stays out. `scripts/get_firmware.py` fetches it on the owner's machine, and the gate is what keeps the two legal situations from being confused |

**The digest is the load-bearing part.** A binary cannot be read, cannot be usefully diffed, and cannot be
told apart from something a contributor built. A SHA-256 is 64 characters, cannot be nearly right, and
lets anyone verify this copy against upstream **without trusting this project**. Recording a fact about a
binary is the opposite of recording the binary and hoping.

**And a contributor gets an obvious place to put one.** A driver that needs a blob adds
`nonfree/<part>/`, and the gate makes them declare the licence and the provenance or the build fails. The
policy is mechanical rather than remembered, which is the only kind this project keeps.

### What that costs, and the part that is not built yet

**WiFi still depends on `fs`** - the blob is in the repository, but the driver is a userspace service
reading a file, and on the Pi 4 that file is on a USB stick. Commandment VIII governs it: wait on `fs`'s
reply or on the loud fact of its absence, never on a timer, and report "firmware unavailable" rather than
hanging. The rule above the rules applies - no missing dependency may wedge the machine.

What the vendoring removed is the SETUP problem, not the runtime one: nobody has to find the file, but it
still has to reach a disk the OS can read.

**Superseded (section 21):** the firmware does not come from a disk. It is embedded in the driver's image
at build time (`include_bytes!` in `services/wifi-driver/src/firmware.rs`), so bringing the radio up does
not depend on `fs` at all and no bake verb was needed. `fs` is still a send peer, for `/wifi.keys` only
(section 43), and a missing `fs` costs the saved keys, never the radio.

**Getting the file onto a Godspeed disk needs one small addition**, and this is a correction to an earlier
draft of this section: `osdev mkfs` only FORMATS an empty GSFS image. The host-side bake path does exist -
`gsfs_add_file`, which `osdev script-disk` uses to put a `.gsh` script on a flashable disk - but no CLI
verb takes an arbitrary binary. Its constraints matter to anyone planning on it: a name of at most 38
bytes, a single root directory block holding **seven** entries, and one contiguous extent per file. The
firmware set is three files with short names, so it fits; the verb is a phase-2 job.

**Loading it over the network** is recorded only so nobody proposes it as new: absurd for a network driver
on a machine with no other link, merely awkward on a Pi 4, which has ethernet.

### Restartability has a cost here worth naming now

§6.2 requires a driver's death to be a supervisor restart. A WiFi driver's restart means re-uploading
~600 KB over SDIO and re-associating - on the order of a second, during which the link is down and
`net-stack` sees a dead peer. That is acceptable, and it is exactly the `EndpointDead`,
reacquire-by-name, retry path (§14.3). But it must be measured rather than assumed, and
`chaos max-carnage` will find out.

**Superseded (section 46):** a respawn does not re-upload. The firmware outlives the service, so the new
instance adopts the firmware that is already running, rescans, reloads `/wifi.keys` and rejoins - measured
at about seven seconds of lost link, with no upload and no power cycle. Only a firmware that has stopped
needs the upload again, after a power cycle (section 47).

---

## 9. Non-goals, stated so scope cannot creep (§26.2, §13.3)

**In:** WPA2-PSK, both bands the chip offers (one band when this was written), one SSID, one station link, scanning, DHCP through the existing stack.

**Out, and each for a reason rather than for now:**

- **WEP.** Broken. Never.
- **WPA3 / SAE.** Needs elliptic-curve crypto in a tree with no crypto at all.
- **Enterprise / 802.1X / EAP.** A certificate stack. Multi-month on its own.
- **AP mode, mesh, concurrent STA+AP.** A different product.
- **Roaming, band steering, power save.** Optimisations of a thing that does not exist yet.
- **Rate adaptation.** Firmware's job on a full-MAC part; leave it there.
- **A generic `cfg80211`-shaped abstraction over both radios.** Speculative abstraction is
  architectural debt (§26.2). Two radios that share nothing but a frame interface do not need a third
  layer between them. If the Pi 2 dongle is ever done, *then* look for what genuinely repeats.

---

## 10. What the enforcement layer will demand

Written here because this branch is also a dogfood run: a driver contributed *after* the rules became
findable, held to them by the same gates a stranger would meet. Predicted, so it can be checked:

- A new service needs a contract **and** a supervisor spawn row that agree - `contract_check.py`, plus
  `IV-contract-authority` in `commandments.py`. A capability in the `.toml` that nothing grants is the
  silent failure §13.6 was amended for; it will not pass.
- A new device class must be resolved by the kernel from a **name**, never an address. `SpawnImage`
  refuses raw MMIO and raw vectors (§14.1, step C), and that is the rule, not an inconvenience.
- `arch_seam_check.py` will require every arch to answer any new `arch::imp` member - all seven, not
  just the one with the radio.
- This document must be listed in `docs/CLAUDE.md` or `docs_index_check.py` fails; every path it cites
  must exist (`doc_refs.py`); every line citation must still point at what it claims
  (`line_ref_check.py`); and no em-dash anywhere.
- The service must be `unsafe`-free. All MMIO goes through the SDK's audited `Mmio` wrapper (§18.1), and
  `#![deny(unsafe_code)]` enforces it at the compiler.
- **`chaos max-carnage` is the bar, not a unit test.** If Chaos finds a bug, the bug already existed.

One prediction recorded in advance, in the spirit of the one-change-per-flash rule: **the gate I expect
to bite first is `contract_check`**, because a WiFi driver wants an unusual capability set and the
temptation will be to declare it in the `.toml` and assume the kernel grants it. That is precisely the
mistake CLAUDE.md §13.6 was amended to stop, and this is a fair test of whether the amendment works on
someone reading it fresh.

> **The prediction was wrong, and it was wrong within a minute.** The first gate to fire was
> `doc_symbols_check` (`GS0402`), on **this document**, before a line of code existed: it flagged one
> backticked snake_case name that resolves nowhere in the source. The prediction is left standing above
> rather than tidied to match, because a prediction quietly corrected after the fact measures nothing.
>
> Two things worth keeping from it. First, the enforcement layer reaches the SPEC, not just the
> implementation - a gate fired on a design document on a branch with no code in it, which is the
> earliest a rule has ever caught anything here. Second, the fix was **not** the one the diagnostic
> offered. Its help text sanctions a baseline entry for an external symbol, which would have been
> legitimate; de-backticking was better, because the name was being used as prose rather than as a
> symbol reference and CLAUDE.md §26.14 already writes Linux, BSD and u-boot plain. A good diagnostic
> suggests a fix; it does not get to choose it.

---

## 11. Open questions, for sign-off before any code

1. **Does the VisionFive 2 Lite have WiFi at all?** Settle on the board. It changes the radio count and
   nothing else in this plan. *Settled 2026-09-30, section 44: yes, an AIC8800D80 on SDIO.*
2. **Firmware blob: in the repo, or supplied by the user?** A licensing call, not a technical one.
   *Settled, section 8: in the repository, `nonfree/brcm43455/`, with its provenance.*
3. **Is the Pi 4 radio behind the Arasan controller, with the SD card on `emmc2`?** Section 4's whole
   argument rests on it. Confirm against the device tree before writing code. *Settled, section 14.*
4. **Does the passphrase persist across reboots**, and if so where and under what capability? "Retype it
   each boot" is a legitimate phase-4 answer and avoids the question entirely. *Settled, section 43:
   derived keys, never passphrases, in `/wifi.keys` through `fs`.*
5. **One service or two** - `wifi` alone, or `wifi` plus `supplicant`? Section 6 argues two for the
   interface even though the full-MAC shortcut sends the secret through the driver anyway. *Settled,
   section 6's superseding note: one service, the driver, which also runs the handshake.*
6. **Is `ping` over WiFi on the Pi 4 the finish line for v1 of this work?** Naming the finish line now is
   what stopped the networking effort sprawling, and phases 0-5 are already a substantial body of work.
   *Reached, section 41: ping over the radio on the Pi 4.*

---

## 12. Hardware test 1: the verb, before there is a radio

**Written before the boards booted, per the one-change-per-flash rule.** One logical change - the `wifi`
shell verb (`1602f3df`) - and a prediction per machine that is specific enough to be wrong. Nothing here
touches a radio; what is being tested is that a new verb reached four ISAs without breaking anything and
that its one truthful answer is the same on all of them.

**Already verified in QEMU before any flash**, so the boards are being asked a narrower question than
"does it work": `osdev test shell` 215 passed / 0 failed (206 before, so all nine new assertions ran and
passed), `osdev test identity` 24 of 24, `conform --check` 18 of 18, and all four boards build -
`kernel8.img` 3,718,080 bytes for the Pi 4, `kernel7.img` 2,087,344 for the Pi 2,
`godspeed-riscv64-visionfive.img` 3,107,752 for the VisionFive.

### What each machine should print

| Board | `wifi` | `help` | Anything else |
|---|---|---|---|
| **HP T630** (x86-64) | `no wireless radio on this machine` + the `wifi-driver` line | a `wifi [list\|connect <ssid>]` row | nothing changes anywhere else |
| **Dell Wyse 5070** (x86-64) | identical to the T630 | same row | same |
| **Raspberry Pi 2** (ARMv7) | identical | same row | same |
| **Raspberry Pi 4** (AArch64) | identical - **the radio exists in silicon and no driver claims it** | same row | same |
| **VisionFive 2** (RISC-V 64) | identical | same row | same |

**The Pi 4 line is the one worth reading carefully.** That board HAS a CYW43455 on the board, and the
prediction is still the absence line - because absence is measured by whether a service named
`wifi-driver` is running, not by whether silicon is present. If the Pi 4 ever prints something different
from the T630 here, something is wrong with this change, not right.

### What would falsify this

Each of these means stop and investigate rather than shrug:

- **Any board printing a different sentence from the others.** The verb is arch-neutral shell code; a
  per-board difference means something arch-conditional leaked in.
- **`wifi` reporting an error** (a non-zero `result`). Asking about wireless on a machine with no radio is
  a legitimate question with a definite answer, and reporting it as a fault is the silent-failure inversion
  invariant 12 forbids. Pinned in QEMU; if hardware disagrees, the pin is wrong.
- **`wifi join Some hunter2` being ACCEPTED.** It must refuse, by name, with the reason. If any board
  takes it, a passphrase reaches `/.gsh_history` and this is a security regression, not a cosmetic one.
- **`help` missing the row.** Then the verb exists and nobody can find it, which is what `facts_check`
  caught in QEMU.
- **Tab after `wifi ` listing a directory.** It must offer the seven subcommands; a path menu means
  `NO_PATH_CMDS` did not take effect on that build.
- **Any selfcheck or chaos regression.** The change adds a verb and touches nothing else, so a fall in
  either count is a real finding and not noise.

### What this test does NOT establish

That any of the wireless design works. There is no SDIO code, no firmware, no association, no keyring and
no crypto - sections 4 through 8 are all unstarted. This flash proves the surface exists on every machine
and that adding it cost nothing elsewhere. **Phase 1 still needs what section 11 asks for**: the Pi 4
device tree read for the `mmc` versus `emmc2` question, and the firmware-blob decision.

---

## 13. Pi 2 (soft-MAC), milestone 1: the chip answers - PASSED 2026-09-27

The operator asked for the Pi 2 first, which is the soft-MAC path section 3 defers as phase 6. Taken in
the order that makes each step decidable, the first question is not a design question at all: **can we
talk to this chip.**

```
dwc2-svc: port 5 DEVICE direct - VID:PID=0bda:8176 class=0x00 speed=high addr=5
dwc2-svc: RTL8188CUS at 0bda:8176 - SYS_CFG(0xF0)=0x04400735 ISO_CTRL(0x00)=0x541c82f8
dwc2-svc: RTL8188CUS register reads OK - the chip answers
```

Two vendor control reads, 13 ms, first boot, no retry needed. The RTL8192CU family exposes its whole
register file through one vendor request (`bRequest` 0x05, `wValue` = offset, direction in
`bmRequestType`) rather than through MMIO, so this needed only the control path `dwc2` already had.

**All three criteria set in advance are met**, which is what makes this a pass rather than an
impression: both values are non-zero, neither is `0xFFFFFFFF`, and **they differ from each other** - so
the device is answering from a register file rather than returning one latched value. A floating bus and
a dead chip both read as all-zeros or all-ones, and the probe counts either as a failure for that
reason.

**The values are recorded undecoded, on purpose.** `SYS_CFG`'s version and vendor bits are decodable,
but doing it from memory rather than from the register map would put a confident wrong number in this
document, and a wrong decode is worse than none because the next reader trusts it.

### Where it lives, and the honest note attached to that

Inside `dwc2`, as `services/dwc2/src/rtl.rs`. On this board `dwc2` owns the USB bus - no other service
can issue a control transfer - and inventing a bus-passthrough surface to host one driver is the
speculative abstraction §26.2 forbids. The precedent is `net.rs`, the LAN9514's ethernet function,
matched by VID:PID for the same reason this is (class 0xff, nothing to match on).

But a radio is far larger than `smsc95xx`, and an 802.11 MAC plus a supplicant do **not** belong in the
service that also owns the keyboard and the disk. The likely end state is a `wifi-driver` service plus a
narrow USB-transfer protocol in `dwc2`, following section 2's split. Not built for one register read.

### What gates milestone 2, stated rather than guessed around

Milestone 2 is the **write** path: write a register and read the value back, which every init table
above it depends on. It needs a register that is safe to write, and **choosing one from memory on real
hardware is how a device gets wedged.** §26.14 is explicit that a reference implementation is read as
an executable datasheet; that reading has not happened, so this is a blocker and not a task.

So the Pi 2 path now has the same shape of blocker as the Pi 4 path, and both are one fact each:

| board | blocked on |
|---|---|
| **Pi 2** | the RTL8192CU register map - which registers are writable, what the PHY/RF tables contain, which firmware blob the part wants |
| **Pi 4** | the BCM2711 device tree - is the CYW43455 behind the Arasan controller `sdhci.rs` already drives |

**And the sizing argument from section 3 has not changed.** Above milestone 2 the Pi 2 needs PHY/RF
register tables, IQ calibration, a firmware blob, an 802.11 management state machine and WPA2-PSK
host-side in a tree with no cryptography. The Pi 4's full-MAC part needs a fraction of it. Milestone 1
was worth doing first on either board because it is cheap and decisive; the order of everything after
it is still the one section 3 argues for.

---

## 14. The Pi 4 blocker is CLOSED - the radio is on the Arasan, confirmed 2026-09-27

Section 4 rested the whole phase-1 estimate on one board fact and section 11 listed it as the first thing
needing sign-off: **is the CYW43455 behind the Arasan controller `services/block-driver/src/sdhci.rs`
already drives, with the SD card on the other one.** It is. From the vendor's own device tree, which is
what the firmware and Linux both act on:

```text
arch/arm/boot/dts/broadcom/bcm2711-rpi-4-b.dts
    &mmcnr  { pinctrl-names = "default"; pinctrl-0 = <&sdio_pins>;
              bus-width = <4>; status = "okay"; };        <- the WiFi
    &emmc2  { vqmmc-supply = <&sd_io_1v8_reg>; vmmc-supply = <&sd_vcc_reg>;
              broken-cd; status = "okay"; };              <- the SD card
    &sdhost { status = "disabled"; };

arch/arm/boot/dts/broadcom/bcm270x.dtsi
    mmcnr: mmcnr@7e300000 { compatible = "brcm,bcm2835-mmc", "brcm,bcm2835-sdhci";
                            reg = <0x7e300000 0x100>; }
    sdhci: mmc@7e300000   { compatible = "brcm,bcm2835-mmc", "brcm,bcm2835-sdhci";
                            reg = <0x7e300000 0x100>; }

arch/arm/boot/dts/broadcom/bcm2711.dtsi
    emmc2 ... compatible = "brcm,bcm2711-emmc2"; reg = <0x0 0x7e340000 0x100>;
```

**`mmcnr` and `sdhci` are one controller described twice** - identical `reg`, differing only in which
driver claims it (`brcm,bcm2835-mmc` for the non-removable SDIO case, `brcm,bcm2835-sdhci` for a card).
Bus `0x7e300000` is ARM physical `0xFE30_0000` in low-peripheral mode.

| node | bus | ARM physical | holds |
|---|---|---|---|
| `mmcnr` = `sdhci` | `0x7e300000` | **`0xFE30_0000`** | **the CYW43455 radio**, 4-bit bus, `sdio_pins` |
| `emmc2` | `0x7e340000` | `0xFE34_0000` | the SD card |
| `sdhost` | `0x7e202000` | - | disabled on this board |

### What this buys, stated exactly

**The Arasan is not the boot medium on this board.** That is the whole objection that kept `sdhci.rs`
uncompiled - on the Pi 2 the Arasan *is* the card the machine boots from, so driving it risked writing
GSFS over the boot partition. Here the card is on `emmc2`, so the 25 KB of working polled SD-host code
can be compiled for aarch64 and pointed at `0xFE30_0000` without going anywhere near the boot medium.

What it does **not** buy is a driver. `sdhci.rs` speaks SD, not SDIO: CMD52/CMD53 (IO direct and
extended), function enumeration and the CIS tuple walk are all absent, and those are phase 1's actual
deliverable. The controller half is the part that was already written.

### What is still true, and what is now the next fact needed

Section 3's sizing argument is untouched: full-MAC means the firmware runs the 802.11 state machine and
can perform the WPA2 handshake, so the host side is transport plus a command protocol. That remains the
reason this board is the right one.

The next thing that needs a source rather than a guess is the **firmware**: `brcmfmac43455-sdio.bin`, its
CLM blob and a board-specific NVRAM text file, which is section 8's licensing decision and is unchanged
by any of this.

### And the census stays

`arch/aarch64/sdio.rs` now CONFIRMS the device tree rather than guessing at it, and that is worth
keeping: a document is not a board, and a disagreement between the two would be the most interesting
thing the probe could find. Its first run also earned its place a different way - it read physical
addresses through a high-half mapping, reported "neither answered", and was wrong in the conservative
direction. Both addresses it probes turn out to be correct, including the one that was labelled
UNVERIFIED. That label was still right to be there: it described the EVIDENCE, not the value, and being
lucky about a number is not the same as knowing it.

---

## 15. Phase 1 step 1: the prediction, written before the flash

**Everything below is the PREDICTION**, written and committed before the image was flashed so that it could
be wrong. It is left exactly as written; section 16 has what the board said, including the three
numbers this got wrong. Nothing in THIS section is a result.

### What was built

| where | what |
|---|---|
| `kernel/src/arch/aarch64/sdio.rs` | the census now also asks the firmware to power the SD domain, asks it for the Arasan's base clock, and routes GPIO34-39 to ALT3 (the Arasan's SD1 interface, which is the only path to the radio). It caches whether the Arasan answered |
| `kernel/src/arch/aarch64/mod.rs` | the fixed-window table gains one arm - `"wifi-driver" => (0xFE30_0000, 1)` (keyed on the device kind `WIFI_SDIO` since 2026-10-03, `docs/audio.md`), gated on the census having seen the controller answer - and `emmc_base_clock_hz` returns the clock instead of a flat 0 |
| `services/wifi-driver/` | the service: `host.rs` (the SDHCI host controller, reset/clock/`cmd`) and `sdio.rs` (CMD0, CMD5 twice, CMD3, CMD7, CMD52, the CIS walk, function enable) |
| registration | workspace member, `aarch64_built`, the supervisor's embed list and `has_wifi_driver` cfg, its `IMAGES` row, `MANAGED`, the boot spawn, the death-notification arm, and the kernel's two restart lists (all three replaced since 2026-10-03 by the supervisor's `SPAWN_FLAG_WATCHED`) |

### Why the pin mux and the clock are in the kernel

Both are BOARD facts, and a driver service is granted its own controller's registers and nothing else
(§12.3) - so it cannot route the pins that connect it to the part it drives, and it cannot ask the
VideoCore anything. arm32 makes exactly this argument at `sd_route_to_emmc`, one SoC generation earlier.
The clock matters more than it looks: `emmc_base_clock_hz` returned 0 on this port, and 0 is a REFUSAL
rather than a default, so without it the driver would correctly decline to set any card clock at all.

### The authority, stated plainly

One page of MMIO, granted (by the service's name at the time; by device kind, `WIFI_SDIO`, since
2026-10-03) and only where the census saw the controller answer. **No DMA arena, no interrupt, and no send
peers at this step** (`fs` and `power` came later, sections 43 and 57) - not even `events`, which every other driver here declares. Each
absence is in `services/wifi-driver/contracts/wifi-driver.toml` with its reason; the short form is that every command in this
phase rides the SDIO command line, and a capability that buys nothing is standing authority a compromise
inherits (§3.1, §26.9). They arrive with the phase that needs them.

### The prediction

**The kernel, before any service starts** (the census, which already ran on the previous image - these
four lines are new):

```text
sdio: SET_POWER_STATE(SD, ON|WAIT) -> on
sdio: Arasan base clock <N> Hz
sdio: GPIO34-39 fsel=777777 (ALT3 = Arasan SD1, the firmware already routed the radio to us)
sdio: the Arasan answered, so `wifi-driver` will be granted 0xFE300000 at spawn.
spawn[mmio]: 'wifi-driver' fixed peripheral -> VA 0x60000000 (4096 B)
```

**The service**, in this order, each stage reachable only through the one before it:

```text
wifi-driver: stage 1 - granted 4096 byte(s) of SDIO host registers
wifi-driver: stage 2 - SLOTISR_VER=0x99020000 (the kernel's census read this same register)
wifi-driver: base clock <N> Hz, identification divisor <D> (target 400 kHz)
wifi-driver: CMD5 answered R4=0x... - 2 I/O function(s), memory absent, I/O OCR 0x...
wifi-driver: card selected, RCA 0x...
wifi-driver: stage 3 - an SDIO card with 2 function(s) at RCA 0x..., I/O OCR 0x...
wifi-driver: CCCR rev 0x... , caps 0x... , bus iface 0x...
wifi-driver: stage 5 - walking the CIS from 0x...
wifi-driver: the radio is CONFIRMED ON THE BUS - manufacturer 0x02d0 (Broadcom), device 0xa9bf (CYW43455)
wifi-driver: function 1 enabled and READY
wifi-driver: phase 1 step 1 complete
```

**The specific numbers being predicted, because a prediction with no numbers cannot be wrong:** two I/O
functions, no memory, manufacturer `0x02D0`, device `0xA9BF`, and `SLOTISR_VER` matching what the
census printed on the line above it.

### What each failure would mean, so one boot log is the diagnosis

The stages exist for this. The LAST line printed names the layer that failed:

| stops after | what it means | where to look next |
|---|---|---|
| stage 1 absent, "no SDIO register window was granted" | the census did not see the Arasan answer, so the kernel refused the grant | the `sdio:` census lines - this is the correct outcome on a board without the controller |
| stage 2, "SRST_HC never cleared" | the window is mapped but the controller is not behind it | the grant address against the census address |
| stage 2, "the platform reported NO base clock" | the mailbox `GET_CLOCK_RATE` gave nothing | the `sdio: Arasan base clock` line - it will say UNKNOWN |
| stage 3, "CMD5 got NO ANSWER" | the controller is ours and nothing is on its bus | **the two lines the kernel prints for exactly this**: the power domain and the GPIO mux. If `fsel` was not `777777` before the write, the firmware had the radio muxed elsewhere and this is the first boot that claims it |
| stage 3, "never reported READY" | the card is there and answering; the voltage window was refused | the I/O OCR in the CMD5 line against the window asked for |
| stage 5, an unexpected manufacturer/device | something is on the bus and it is not what this board is documented to carry | a finding, not a failure - report the two codes |
| function 1 not open | reads work and the first WRITE did not | everything before that line is a read, so this is the one line that tests the other direction |

### What this step does NOT do

No firmware upload, so **no 802.11 of any kind** - the chip runs no MAC until a host uploads one into
it (section 8). `wifi list` therefore still cannot work, and the shell still answers that it cannot talk
to the driver yet. `net-stack` is untouched and the radio is not in any frame path. Every request the
service receives is ANSWERED with one byte meaning "unavailable", never queued and never dropped: a
missing capability must return loudly rather than hang.

---

## 16. Phase 1 step 1: PASSED on hardware 2026-09-27 - and the part is not the one expected

Raspberry Pi 4 Model B **rev 1.5, 2 GB** (board revision `0xb03115`), booted from a plain image, no
crash-window, serial at 115200. Every stage of section 15 passed in order. No kernel panic, no liveness
wedge; the machine stayed healthy for the rest of the session (xhci 6157 passes, keyboard, disk,
network, SNTP clock set).

```text
sdio: SET_POWER_STATE(SD, ON|WAIT) -> on
sdio: Arasan base clock 250000000 Hz
sdio: GPIO34-39 fsel=000000 (NOT all ALT3 - the radio was muxed away from the Arasan; routing it back)
sdio: 0xfe300000 Arasan ... CAPS=0x0 VER=0x99020000 - A CONTROLLER ANSWERED
sdio: 0xfe340000 emmc2  ... CAPS=0x45ee6432 VER=0x10020000 - A CONTROLLER ANSWERED
spawn[mmio]: 'wifi-driver' fixed peripheral -> VA 0x60000000 (4096 B)
wifi-driver: stage 2 - SLOTISR_VER=0x99020000 (the kernel's census read this same register)
wifi-driver: base clock 250000000 Hz, identification divisor 313 (target 400 kHz)
wifi-driver: CMD5 answered R4=0x30ffff00 - 3 I/O function(s), memory absent, I/O OCR 0xffff00
wifi-driver: card selected, RCA 0x0001
wifi-driver: CCCR rev 0x32 (CCCR fmt 2, SDIO spec 3), caps 0x02, bus iface 0x40, IOE 0x00, IOR 0x00
wifi-driver: stage 5 - walking the CIS from 0x010ac
wifi-driver: CIS FUNCID 0x0c (0x0c = network adapter)
wifi-driver: function 1 enabled and READY (IOE 0x00 -> 0x02, after 1 read(s) of IOR)
```

The clock arithmetic checks out both times, which is worth stating because it is the number the whole
bring-up rests on: 250 MHz / (2 x 313) = 399,361 Hz for identification, and 250 MHz / (2 x 5) = 25 MHz
for operation. `SLOTISR_VER` matched between the kernel's census and the service, so the grant is
pointed where the census looked.

### THE FIRMWARE ASSUMPTION IS BROKEN: the part reports 43430, not 43455

```text
wifi-driver: an SDIO part answered but it is NOT the expected radio -
             manufacturer 0x02d0, device 0xa9a6 (expected 0x02d0/0xa9bf)
```

**The manufacturer is exactly right and the device is not.** Broadcom's SDIO device codes for this
family are the decimal part number written in hex, which makes this arithmetic rather than
recollection:

| code | decimal | part |
|---|---|---|
| `0xA9BF` | 43455 | CYW43455 - what section 4 and section 8 assume, and what `nonfree/brcm43455/` holds |
| `0xA9A6` | **43430** | BCM43430 - what this board actually reported |

**The reading is trustworthy, and for a reason independent of the reading.** Manufacturer came back as
exactly `0x02D0`. Both values come out of the same four-byte CISTPL_MANFID body at consecutive offsets,
so a wrong tuple offset or a swapped byte order would have produced garbage for the manufacturer too.
It did not, so the offsets and the endianness are right and the device code is what the chip published.

**The consequence is concrete and it lands on section 8.** Linux matches on this exact id to choose a
firmware file, and `0xa9a6` selects `brcmfmac43430-sdio.bin`. The blob vendored in this repository is
the 43455 one. So the firmware upload in phase 2 would very likely have failed against a board that
wants a different file - which is the failure this step existed to find BEFORE building the upload path
on top of a guess.

**What is NOT settled, stated rather than reasoned away.** A 43430 is a 2.4 GHz-only 802.11n part, and a
Pi 4 is sold as dual-band. That tension is real and this section does not resolve it: either later Pi 4
revisions carry a different radio than the product page implies, or a 43455 variant publishes a
different SDIO device code than its part number. Picking whichever answer is more comfortable would be
explaining away a reading, so it stays open until something measures it.

**And it is cheaply settleable, from the chip rather than from its CIS.** Function 1 is the backplane and
it is now open. The chipcommon core sits at backplane address `0x18000000` and its register 0 is
`chipid` - the silicon's own id and revision. That read is needed by the firmware upload anyway (it is
what says where the chip's RAM is and where its cores live), so the next step answers the firmware
question as a side effect of work already on the list.

### The pin mux was NOT redundant, which the prediction got backwards

Section 15 predicted `fsel=777777` and said the firmware would already have routed the radio to the
Arasan. **It reported `000000`** - GPIO34-39 left as plain inputs. So the firmware does NOT do this, and
the kernel's `route_pins_to_arasan` is what made the radio reachable at all.

Had that been left out as "the firmware surely handles it", CMD5 would have timed out and the log would
have pointed at the power domain and the bus - the two suspects the failure table names - while the real
cause sat in a register nobody was printing. It cost eleven lines and it was the difference between this
boot and a debugging session.

### Two findings in our own code, both benign here

- **The CIS walk hit its 64-tuple bound without reaching an END tuple**, and said so. Identification was
  unaffected because `CISTPL_MANFID` is the very first tuple at `0x010ac`; what follows is a long run of
  `0x80` vendor-specific tuples, and the addresses advance correctly through every one of them
  (`0x010ac` +2+4 -> `0x010b2` +2+2 -> `0x010b6` ...), so the walk is right and the ceiling is just low.
  The bound firing and NAMING ITSELF is the bound working. Raise the ceiling, and raise the per-tuple
  report limit so the rest of the chain is visible.
- **A log-ordering wart:** the decoded `CIS FUNCID 0x0c` line prints ABOVE its own `CIS tuple 0x21`
  header, because the decode branch logs before the generic line does. Cosmetic, and it misreads.

### Everything else behaved

`observe` showed `wifi-driver C3 BlockRecv 292 KiB/16 MiB 1% 0/16 0%` for the whole session: blocked on
`recv`, queue empty, burning no core - the serve loop doing what it claims. `wifi` at the prompt
answered exactly as section 12 specifies, that the driver is running and the shell cannot talk to it
yet, and `wifi about` still gets the subcommand list rather than an unhandled error.

---

## 17. Phase 1 step 2: reach the chip's own bus - the prediction

**A PREDICTION, written and committed before the flash.** Nothing here is a result.

### What this step is for

Section 16 left one question, and it decides which firmware blob phase 2 uploads: the CIS said device
`0xA9A6` (43430 decimal) where the part this board is documented to carry answers `0xA9BF` (43455). The
CIS cannot settle that - it IS the disputed reading. The chip can, and the register that does it is the
first one a firmware upload has to read anyway.

### What was built

| where | what |
|---|---|
| `host.rs` | `cmd_data` - a bounded PIO data transfer. CMD52 carries one byte in its RESPONSE and cannot read a 32-bit register at all; CMD53 moves bytes through the controller's FIFO. This is also the path a 600 KB firmware image rides, so it is on the list regardless |
| `sdio.rs` | `read32` - CMD53 in byte mode, incrementing address, four bytes |
| `backplane.rs` | the window mechanism (three control bytes, written only where they CHANGE), the clock handshake, and the chipcommon identity read |
| `main.rs` | stage 6 gets a numbered line of its own, and stage 7 is the new one |

PIO rather than DMA, for the two reasons `block-driver`'s backend gives: DMA on this SoC is not cache
coherent without explicit maintenance, and these transfers are four bytes. Whether a firmware upload
wants DMA is a MEASUREMENT for the phase that does one, not a guess for this one.

### How a host reaches inside this chip, since nothing else in this tree works this way

The radio's internal bus is not memory-mapped anywhere the host can see. It is reached through SDIO
function 1, whose 17-bit address space is split three ways:

```text
  0x00000 .. 0x07FFF   a 32 KiB WINDOW onto the backplane, wherever the window currently points
  0x08000              the same window, flagged as a 2-or-4-byte access rather than a single byte
  0x1000A .. 0x1000F   the function's own control registers, which is where the window is SET
```

So one 32-bit backplane read is: point the window with up to three `CMD52` writes, then `CMD53` at
`(addr & 0x7FFF) | 0x8000`. The window is only rewritten where a byte actually changes, because each
write is a command on the bus and three per register would dominate a firmware upload made of thousands.

### The prediction

```text
wifi-driver: stage 6 - opening function 1, the backplane
wifi-driver: function 1 enabled and READY (IOE 0x00 -> 0x02, after 1 read(s) of IOR)
wifi-driver: stage 7 - waking the backplane to read the chip's own identity
wifi-driver: backplane awake - CHIPCLKCSR 0x68, ALP available after <N> read(s)
wifi-driver: CHIP SAYS id 0xa9a6 (BCM43430 ...) rev <R> package <P> type <T> [raw 0x????????]
wifi-driver: the silicon AGREES with the CIS - this is a 43430, not the 43455 ...
```

**`CHIPCLKCSR 0x68` is arithmetic, not a guess:** the driver writes `0x28`
(`FORCE_HW_CLKREQ_OFF | ALP_AVAIL_REQ`), requires that exact value to read back, and then waits for
`ALP_AVAIL` (`0x40`) to appear on top of it. `0xE8` would mean the HT clock is up too, which says
something about the state the firmware left the chip in and is not a problem.

### The chip id is a two-way fork, and this commits to the less comfortable side

| if the chip says | then |
|---|---|
| **`0xA9A6` (43430)** | the silicon agrees with the CIS. The board carries a 2.4 GHz-only part, `nonfree/brcm43455/` is the WRONG blob, and phase 2 needs the 43430 firmware |
| `0x4345` | the CIS device code is NOT the chip id on this part, the board does carry a 4345-family radio, and the vendored blob is right after all. Which 4345 variant is then the revision field |
| anything else | a finding, reported with its id and revision |

> **WRONG, and corrected here rather than only in section 20.** The chip answered `0x4345` rev 6: the
> comfortable side. The reasoning below was sound about METHOD and wrong about the FACT, and the fact it
> missed is that the CIS device code and the silicon chip id are **different fields** which nothing
> requires to name the same part - so `0xA9A6` was never evidence about which part this is. Neither option
> in the table above said that, which is why both were framed as a contradiction to be resolved rather
> than as two readings of two different things.

**`0xA9A6` is the prediction**, and it is deliberately the uncomfortable one. A 43430 on a Pi 4 rev 1.5
contradicts the product being sold as dual-band, so the comfortable answer is `0x4345` - and the reason
not to pick it is section 16: the last prediction got the device code wrong by preferring a document to
the chip, and the only MEASUREMENT taken so far says 43430. The supporting reasoning is weaker than the
measurement and is flagged as such: Linux's SDIO device table appears to carry `0xa9a6` and `0xa9bf` as
SEPARATE entries, which would be pointless if one part reported both - but that is recollection, and
recollection is exactly what this read replaces.

### What each failure would mean

| stops after | what it means |
|---|---|
| stage 6, function 1 not open | the first WRITE to the card failed. Everything before it was a read, so this is the one line that tests the other direction |
| `CHIPCLKCSR wrote 0x28 and read back ...` | the write was accepted and did not stick, so the bus is talking to something that is not that register - and no read below it would mean anything. This check exists for exactly that case and is Linux's own first question of a chip it has just enabled |
| `never reported the ALP clock available` | the register answers, so the chip is there and its clock is not coming up |
| `could not set the backplane window` | one of the three window bytes was refused. The window is then PARTLY written, so the cached value is discarded rather than left to make the next read silently skip a write it needed |
| `the register reads 0x00000000` or `0xffffffff` | the bus answering with nothing rather than a chip identifying itself. Refused by name, because a chip id of 0 would otherwise be reported as "a part this driver has no name for" - a wrong answer instead of an error |

### Also in this image: the two findings from the last boot

The CIS tuple ceiling is 256 rather than 64, so the chain can be seen to reach its END tuple instead of
running out of bound; the per-tuple report limit is 24 rather than 8 for the same reason. And the generic
"CIS tuple" line now prints BEFORE the branches that decode a tuple's contents, so `CIS FUNCID` no
longer appears above its own header and read as belonging to the tuple before it.

---

## 18. Phase 1 step 2, boot 1: the chip was fine and the GUARD was wrong

Same board, 2026-09-27. Stages 1 through 6 repeated exactly, including the pin mux reporting
`fsel=000000` a second time - so the firmware reliably does not route those pins and the kernel reliably
does. Stage 7 stopped one line in:

```text
wifi-driver: stage 7 - waking the backplane to read the chip's own identity
wifi-driver: CHIPCLKCSR wrote 0x28 and read back 0x68 - the write was accepted and did not stick ...
```

**That diagnosis is wrong, and the arithmetic says so immediately.** `0x68` is `0x28 | 0x40`. Both bits
the driver wrote are present, so the write DID stick. The extra `0x40` is `ALP_AVAIL` - a **read-only
status bit the hardware sets** - which means the chip had already granted the clock the driver was about
to ask for. `0x68` is the success value, and the check rejected it.

### The contradiction was inside one commit

`CHIPCLKCSR` mixes bits a host WRITES with bits the hardware REPORTS. Comparing the whole register for
equality with what was written therefore asks it a question it cannot answer: a chip that grants a clock
sets a status bit, so a healthy readback is legitimately different from the write. A *working* chip is
what breaks that assertion.

Worse, both halves of the mistake are in the same change. The commit message singled this check out as
earning its place. Section 17, one paragraph below the assertion, **predicted `CHIPCLKCSR 0x68` as the
healthy reading** - and called it arithmetic rather than a guess, correctly. The prediction and the
assertion disagreed with each other and neither noticed, which is a more useful thing to know about this
process than the bug itself: a prediction is only a check on the code if something compares them.

### The fix is the mask, not the deletion

The check stays. A write that is accepted and does not stick means the bus is talking to something that
is not this register, and every later read would silently inherit that - a real failure worth catching,
and the reason Linux asks this question of a chip it has just enabled. What changes is that it asks about
**the bits this driver owns**:

```text
REQUEST_BITS = FORCE_ALP | FORCE_HT | FORCE_ILP | ALP_AVAIL_REQ | HT_AVAIL_REQ | FORCE_HW_CLKREQ_OFF
```

and requires `read & REQUEST_BITS == written`. `ALP_AVAIL` and `HT_AVAIL` sit above that mask because
they are not ours to predict. The failure message now prints the masked value AND the whole register, so
the next reader can see which half disagreed.

Every writable bit is in the mask even though this step sets only two of them, because a mask that
covered just the bits we happen to use would forgive a real failure in the ones we do not.

### A second bound that exited in silence

Found while checking the above rather than from a failure, which is the only reason it is here: the CIS
walk is bounded twice - a tuple count and a byte span - and **only the count reported**. The span
condition failing dropped out of the loop with nothing printed. That is the thing `arch/CLAUDE.md` rule 2
names outright: every bound must return a result the caller reads.

All four exits are named now - an END tuple, the count, the span, and a failed read - and it earns its
keep immediately, because it explains a disagreement that would otherwise have been invisible.

### CORRECTED: the two boots did NOT disagree, and this paragraph invented an anomaly

**What this section claimed, and it was wrong.** It said two boots of one chip disagreed about where its
CIS ends, that the byte at `0x010d8` read as `0xFF` on one boot and something else on the other, and that
the reading was unexplained. The next boot measured it: **176 tuples, ending properly at `0x01180`**, and
the arithmetic settles it completely.

212 bytes for 176 tuples is 1.2 bytes each, and a `cistpl::NULL` tuple is **one byte** - it has no length byte at
all, which is the one special case the walk handles. So the chain is 8 real tuples followed by **168 bytes
of NULL padding**, and a NULL hits `continue` BEFORE the print, which is also why only 8 tuple lines
appear however high the report limit goes.

So the three boots were consistent throughout:

| boot | what happened |
|---|---|
| section 16 | hit the **64-tuple ceiling** - correct, because the chain needs 176 |
| section 18 (this one) | ceiling raised to 256, so it **reached the END tuple and said nothing**, because the "ended properly" line did not exist yet |
| section 19 | same walk, and now it SAYS `the CIS ended properly at 0x01180 after 176 tuple(s)` |

**The diagnosis in this section was right and the story told beside it was invented.** A bound that exits
in silence was indeed the gap, and naming every exit was indeed the fix. But having found a mute
instrument, this section then wrote up a hardware anomaly to explain readings the instrument had simply
failed to report - which is the error it was warning about, committed one paragraph later. The rule is to
suspect the instrument before the board; the rule was quoted and then not applied.

Nothing downstream depended on it. What follows in this section is unchanged and still stands.

### Still open: the chip identity, and therefore the firmware

Stage 7 never reached the identity register, so section 16's question stands untouched: the CIS says
`0xA9A6` (43430) and the board is documented to carry a part that answers `0xA9BF` (43455). Section 17's
prediction - `0xA9A6`, the uncomfortable side - is unresolved and is carried forward unchanged.

---

## 19. Phase 1 step 2, boot 2: two fixes land, and CMD53 is the one thing left

Same board. Both corrections from section 18 worked on the first try:

```text
wifi-driver: backplane awake - CHIPCLKCSR 0x68, ALP available after 1 read(s)
wifi-driver: the CIS ended properly at 0x01180 after 176 tuple(s)
```

The first is the masked check accepting the value the exact compare rejected, and `ALP available after 1
read` says the chip had granted the clock before it was asked - which is what `0x68` meant all along. The
second is the walk naming its own exit, and it immediately paid for itself by disproving the anomaly
section 18 had invented (corrected in place, above).

### The last failure: the data phase

```text
wifi-driver: CMD53 read of function 1 address 0x08000 failed - INT=0x00000000
wifi-driver: the chipcommon identity register could not be read
```

So the backplane window was set (three CMD52 writes, all accepted) and the 32-bit read did not happen.

**`INT=0x00000000` narrows nothing, and that is the first thing wrong.** `cmd_data` has four bounded
waits and all four reported the same sentence - and the register is AMBIGUOUS between them by
construction, because `CMD_DONE` is cleared once the command lands. A zero there is exactly what a
healthy command looks like while the FIFO is being awaited. "The command never issued" and "the command
was fine and no data came" are different bugs, and the log could not tell them apart.

### Two fixes, and the second is the one this project's own rules asked for first

**Every wait names itself**, and STATUS and the CMD53 argument print beside INTERRUPT.

**And `cmd_data` no longer reimplements the command phase.** `block-driver`'s sdhci backend drives this
exact Arasan block, and its data path is:

```text
wait DAT_INHIBIT  ->  write BLKSIZECNT  ->  cmd()  ->  poll READ_RDY  ->  drain DATA  ->  DATA_DONE
```

where `cmd()` is the same function every non-data command uses. `cmd_data` had inlined its own copy of
that logic - the inhibit wait, the stale-status clear, the ARG1/CMDTM writes, the CMD_DONE poll, the
error handling - which is four chances to differ subtly from code known to work on this silicon. It calls
`cmd()` now, so a CMD53's command phase is literally the path CMD0, CMD3, CMD5, CMD7 and CMD52 all take
successfully on this board, and only the data phase is new.

That is the porting rule this repository states for itself: diff against the working code before
debugging on hardware. The reimplementation was worth removing whether or not it is the fault - and if
the read still fails, the failure is now confined to the data phase and will say so.

### What the next boot distinguishes

| the line says | what it means |
|---|---|
| `the command itself did not complete` | CMD53 is not being accepted, though CMD52 is. The command encoding or the block registers |
| `the FIFO never became ready` | the command was fine and the card sent nothing. A four-byte block size, byte mode, or the DAT line in a state the transfer needs |
| `the DAT line never came out of inhibit` | something earlier left the line busy |
| `the data moved and the transfer never reported complete` | the read worked and only the completion signal is missing - the value would be in hand |

### Still open, and unchanged

The chip identity, and therefore which firmware blob phase 2 needs. Section 17's prediction of `0xA9A6`
remains unresolved. Everything up to it stands: the radio is on the bus, identified by its CIS, its
backplane is awake, and its clock is granted.

---

## 20. Phase 1 COMPLETE: the radio is identified from its own silicon, 2026-09-28

```text
wifi-driver: backplane awake - CHIPCLKCSR 0x69, ALP available after 1 read(s), FORCE_ALP held
wifi-driver: CHIP SAYS id 0x4345 (the 4345 family - CYW43455 at rev 6) rev 6 package 2 type 1
             [raw 0x15264345]
```

The CYW43455 is confirmed **by asking the part**, not by reading a device tree or a product page. Its
backplane is reachable, its clock is running, and a 32-bit register read through the SDIO window works.

### What was actually wrong: one bit

`SBSDIO_FORCE_ALP`, `0x01`, missing from the chip clock word. `cyw43-driver` - Infineon's own driver for
this chip family - branches on the transport:

```c
#if !CYW43_USE_SPI
    SBSDIO_FORCE_HW_CLKREQ_OFF | SBSDIO_ALP_AVAIL_REQ | SBSDIO_FORCE_ALP      /* 0x29, SDIO */
#else
    SBSDIO_ALP_AVAIL_REQ                                                      /* 0x08, SPI  */
#endif
```

This driver wrote `0x28`, which is **neither**: the SPI form plus one bit. `ALP_AVAIL` (`0x40`) reports
that the clock is AVAILABLE; `FORCE_ALP` is what RUNS it. Without it the backplane had no clock, so a
backplane read could not be serviced - the card accepted the command and never produced data, which is
exactly what six boots measured.

### The firmware question is CLOSED

brcmfmac's table, quoted:

```c
BRCMF_FW_ENTRY(BRCM_CC_4345_CHIP_ID, 0x00000200, 43456),
BRCMF_FW_ENTRY(BRCM_CC_4345_CHIP_ID, 0xFFFFFDC0, 43455),
```

The second field is a **bitmask over revisions**, which does not look like one: bit N set means revision N
matches. `0xFFFFFDC0` excludes revs 0-5 and rev 9 (rev 9 being the 43456), so rev 6 is bit `0x40`, set.

**`0x4345` rev 6 selects `brcmfmac43455-sdio`, so `nonfree/brcm43455/` is the right blob.** Section 8's
vendoring decision stands, and section 16's alarm about it is resolved: the wrong field was being
consulted.

### The check that caused that alarm is gone

`Manfid::is_expected_radio` compared the **CIS device code** against `0xA9BF` and announced on every boot
that the part "is NOT the expected radio". The CIS device code is the SDIO id - what a host matches a
DRIVER on - and firmware is selected from the CHIP ID read over the backplane. On this board those
disagree (`0xA9A6` versus `0x4345` rev 6), and nothing requires them to name the same part number, so the
comparison was asking a question it could not answer. It is removed; the manufacturer check stays (it is
meaningful, and confirmed the tuple was being read correctly all along) and the verdict moved to the chip
id.

Why a 4345 part advertises a 43430 SDIO code is left open. It has no bearing on anything this driver does.

### What six boots cost, and what they bought

Every element of the setup was verified along the way, each by a measurement rather than an argument:

| verified | how |
|---|---|
| CMD53 argument, byte mode | read off `mmc_io_rw_extended` and `sdio_io_rw_ext_helper` |
| `BLKSIZECNT`, `CMDTM` | read BACK from the controller, and matching Linux's words exactly |
| `CONTROL0` / DMA select | read back, zero |
| backplane window | read back, `0x18000000` |
| function 1 enabled and ready, both function block sizes | read back, held |
| GPIO 34-39 mux | read back, `34=f7/p1 ... 39=f7/p1` - all six ALT3 |
| card accepted the transfer | R5 flags clean |
| the host DID run a data phase | `STATUS` accumulated: DAT Line Active and Read Transfer Active both seen |

Five hypotheses died to those: a reimplemented command path, `TM_BLKCNT_EN`, `CMD_CRC`/`CMD_INDEX`, the DMA
selection, and the pin mux. **The fault was in the one part of the sequence assembled from bit names that
looked sufficient rather than copied from a reference for the transport in use** - and one read of the
vendor driver found it.

The instruction to stop guessing and read the implementation was given twice before that read happened.
Recorded here because it is the most useful thing in this section: the accumulated-OR instrument and the
register readbacks were each worth more than the hypothesis they replaced, and the vendor driver was worth
more than all of them.

### What phase 1 does NOT include

No firmware upload, so still no 802.11 of any kind. `wifi list` cannot work and the shell still answers
that it cannot talk to the driver. What phase 1 delivers is the transport: the radio identified, its
backplane readable, and the correct firmware blob named from the silicon.

---

## 21. Phase 2 steps 1-2 COMPLETE: everything the upload needs, verified on hardware

Every line below is a reading from the board, not a plan.

| fact | how it was established |
|---|---|
| the part is a **CYW43455** (`0x4345` rev 6) | chipcommon identity register, over the backplane |
| firmware wanted: **`brcmfmac43455-sdio`** | brcmfmac's revision BITMASK (`0xFFFFFDC0` covers rev 6) |
| 7 cores, EOT reached | EROM walk |
| ARM core: **CR4 rev 9 at `0x18002000`** | EROM |
| its wrapper: **`0x18102000`** | derived as base + `0x100000`, and the rule CHECKED against all three wrappers the EROM did publish |
| no SOCRAM; it runs from TCM | EROM, and the correct shape for this part |
| **800 KiB of TCM**, 8 banks | `ARMCR4_CAP` then `BANKIDX`/`BANKINFO` per bank |
| firmware load address **`0x198000`** | `brcmf_chip_tcm_rambase`, a per-part table |
| the image is **in the booted binary** | FNV-1a over the embedded bytes matching what `build.rs` measured on disk |
| 611,383 bytes fits with 202 KiB spare | the chip's own size against this build's own image |

Backplane reads and writes both work, the pin mux is confirmed ALT3 on all six SDIO pins, and no boot has
panicked or wedged.

### Why the firmware is embedded rather than read from `fs`

GodspeedOS is a live system, and this is how a live system supplies firmware: a Linux live ISO carries
`/lib/firmware/brcm/brcmfmac43455-sdio.bin` inside the squashfs or initramfs that was loaded into RAM at
boot, and `request_firmware()` reads it from there. The blob travels with the kernel image.

Reading it through `fs` would have been worse on this board in three separate ways: `block-driver` is built
`storage_is_usb`, so the disk sits behind the **`xhci` service** and the radio would depend on the USB stack
plus a stick being present; it would need an `fs` send peer, which is new authority for a driver that has
none; and it would fail on any boot without storage, which is every first boot.

**And the embedding had to be MEASURED, because the source lied about it.** `const IMAGE: &[u8] =
include_bytes!(..)` inlines at each use site, so with only `.len()` used the bytes were discarded - a
135,312-byte binary claiming a 609 KB image. `static` did not fix it either; dead data is dropped at link
time regardless. What retains them is `firmware::verify` genuinely reading them, which is also the check
that proves they are the vendored blob. The guard first written against this - `assert!(IMAGE.len() >
64 * 1024)` - could never have fired, because `len()` is a compile-time constant either way.

### What step 3 needs, all of it now read rather than guessed

**Halt, upload, release.** From `cyw43-driver` and `brcmfmac/chip.c`:

```text
AI_IOCTRL    0x408   (BCMA_IOCTL)        SICF_CLOCK_EN 0x01   SICF_FGC 0x02   SICF_CPUHALT 0x20
AI_RESETCTRL 0x800   (BCMA_RESET_CTL)    AIRC_RESET    0x01
```

and the ordering, quoted: disable first (require `AIRC_RESET` set), then write `IOCTL = FGC | CLOCK_EN |
halt`, read it back, write `RESETCTRL = 0`, wait 1 ms, write `IOCTL = CLOCK_EN | halt`, read back, wait 1 ms.
Both `brcmf_chip_disable_arm` and `brcmf_chip_cr4_set_active` reach these through the WRAPPER, which is why
deriving `0x18102000` unblocked this step.

**Chunking, which is the part that needed reading.** The backplane window is only 32 KiB, so a 609 KB write
cannot be one transfer. `brcmf_sdiod_ramrw` chunks by `SBSDIO_SB_OFT_ADDR_LIMIT` and **sets the window once
per chunk**, not per access - and `brcmf_sdiod_set_backplane_window` caches it (`if (bar0 ==
sdiodev->sbwad) return 0;`), which `Window` here already does.

That matters arithmetically: the current `write32` does one 4-byte CMD53 per call, so 609 KB would be about
152,000 transactions. The upload needs **block-mode CMD53** instead - argument bit 27 set, the count field
carrying a BLOCK count, and `BLKSIZECNT` as `(blocks << 16) | 64` for function 1's 64-byte block size, which
is why that block size was set in step 2.

**Still to read before writing any of it:** where exactly the NVRAM lands relative to `rambase + ramsize`
(brcmfmac writes it to the END of RAM with a length token, not to an address anyone picks), and the
reset-vector handoff in `brcmf_sdio_buscore_activate`, which truncates out of `sdio.c` and will have to come
from the vendor driver's equivalent.

### What is NOT done

No firmware has been uploaded, so there is still no 802.11 of any kind: `wifi list` cannot work and the
shell still answers that it cannot talk to the driver. Phase 2 delivers the transport and the destination;
phase 3 is the radio actually running.

## 22. The first block write, and the difference between a word and a block

The upload ran for the first time. The halt worked, which is the part that had never been exercised:

```
wifi-driver: core wrapper 0x18102000 held in reset after 1 read(s), CPU halted
```

That is the derived wrapper being used for something real rather than merely agreeing with a published
value, so 0x18102000 is now confirmed twice over.

Then the first block-mode write failed:

```
wifi-driver: CMD53 write of 256 word(s) to function 1 address 0x08000 failed - the FIFO never
  became ready - the command completed and no data came (arg=0x9d000010 BLKSIZECNT=0x00107040
  CMDTM=0x353a0022 R5 flags 0x10 STATUS=0x01ef0000)
```

**Every register in that line is correct, which is why the diagnostic mattered more than the failure.**
`arg=0x9d000010` decodes as write, function 1, block mode, incrementing address, address 0x08000, count 16.
`BLKSIZECNT=0x00107040` is 16 blocks of 64 bytes with boundary 7. `CMDTM=0x353a0022` is index 53 with
`BLK_CNT_EN | MULTI` and no READ. `R5 flags 0x10` is the card accepting the command. And
`STATUS=0x01ef0000` has the DAT[3:0] field reading `0b1110`: **DAT0 low, which is the card signalling busy,
waiting for data.**

So the command was right and the card was waiting. The fault was in how the FIFO was fed - and per the rule
this effort has been run on since the `FORCE_ALP` bug, the answer came from reading the reference rather
than from reasoning about what the controller might want.

### What the reference does, quoted

`u-boot`'s `sdhci_transfer_pio`, in full, because it is four lines and the whole answer is in them:

```c
static void sdhci_transfer_pio(struct sdhci_host *host, struct mmc_data *data)
{
	int i;
	char *offs;
	for (i = 0; i < data->blocksize; i += 4) {
		offs = data->dest + i;
		if (data->flags == MMC_DATA_READ)
			*(u32 *)offs = sdhci_readl(host, SDHCI_BUFFER);
		else
			sdhci_writel(host, *(u32 *)offs, SDHCI_BUFFER);
	}
}
```

It moves exactly **one block** - `data->blocksize` bytes - and it re-checks **nothing** while doing so. Its
caller supplies the discipline around it:

```c
	do {
		stat = sdhci_readl(host, SDHCI_INT_STATUS);
		...
		if (stat & rdy) {
			if (!(sdhci_readl(host, SDHCI_PRESENT_STATE) & mask))
				continue;
			sdhci_writel(host, rdy, SDHCI_INT_STATUS);
			sdhci_transfer_pio(host, data);
			data->dest += data->blocksize;
			if (++block >= data->blocks)
				break;
		}
	} while (!(stat & SDHCI_INT_DATA_END));
```

The ready flag is cleared **before** the block moves, once per block, and the next wait happens between
blocks.

### The bug

This driver waited for the ready bit **per word** and cleared it **per word**.

For a four-byte byte-mode transfer that is accidentally correct, because one word *is* one block - which is
exactly why every register read in phases 1 and 2 worked, and why nothing caught this until a 64-byte block
was attempted. For a block it deadlocks: the controller raises the ready bit once when the block buffer is
free, one word goes in, the flag is cleared, and the loop then waits for a bit that cannot set again until
the block completes, which it cannot, because the transfer stopped after 4 of 64 bytes. The card holds DAT0
low waiting for the other 60. That is precisely the state `STATUS=0x01ef0000` reported.

**The instrument told the truth and the conclusion drawn from it was still wrong for one boot**, because
"the FIFO never became ready" is a true statement that invites a theory about the FIFO rather than about the
loop reading it. The fix is structural: wait once per block, clear once per block, then move
`blocksize / 4` words with no further checks. The block geometry is taken from the `BLKSIZECNT` word the
caller already supplies, so the loop and the controller cannot disagree about how big a block is.

### Prediction

With the FIFO fed a block at a time, stage 11 should get past the first write and the next thing observed
is one of three outcomes, in order of what each would mean:

1. **The upload completes.** 609 KB at 0x198000, the NVRAM at the top of RAM, the ARM released, and
   `RESETCTRL 0x00000000 ... RUNNING`. That is phase 2 complete. It would be the first time the chip's own
   processor has executed anything.
2. **A later write fails**, with the byte count and backplane address in the message saying where. A
   failure at a 32 KiB boundary points at the window set; a failure at a chunk boundary points at the
   block-count arithmetic. The message carries both numbers deliberately so the two cannot be confused.
3. **Every write succeeds and the core does not come out of reset** - `STILL IN RESET` on the last line.
   That is the outcome `aicore.rs` already names in its own documentation: it would mean
   `brcmf_sdio_buscore_activate`'s reset-vector write (the image's first four bytes to backplane address 0)
   is required and missing. It is recorded as unimplemented rather than guessed at, so this outcome is
   expected to be legible rather than mysterious.

What is **not** predicted is a working radio. Even a clean release only means the firmware is running; there
is no control channel to it yet, so `wifi list` still cannot work. Phase 3 is that conversation.

## 23. The per-block fix was correct and was not the cause

The next boot produced the identical failure, byte for byte. That is worth stating plainly rather than
softening: **the diagnosis in section 22 was wrong.**

The per-block change was a real bug and worth keeping - it would have deadlocked at block 2 of every
transfer. But it cannot have been *this* failure, and the message said so all along. "The FIFO never became
ready" is the **first** wait timing out, and the first wait is the one rung the change did not touch. A fix
that addresses what happens after the first block cannot fix a failure that occurs before it.

### What the log actually established

- The command completed (`CMD_DONE` was observed, or `cmd_inner` would have returned a different error).
- `INT_ERR` never tripped. That mask is `0x017E_8000`, which includes bit 20 `DATA_TIMEOUT` and the bit 15
  error summary, so **no timeout, no CRC fault, nothing**. This also eliminated a genuine difference from
  the reference found while reading it: u-boot writes `sdhci_writeb(host, 0xe, SDHCI_TIMEOUT_CONTROL)`
  before every data command and this driver never writes that register at all. Worth fixing, but not this,
  because a data timeout would have been reported.
- The card accepted the command (`R5 flags 0x10`, no error bits, `IO_CURRENT_STATE` in transfer).
- `cmd_inner` acknowledges only `INT_CMD_DONE`, so it is not consuming the ready bit before the FIFO loop
  can see it.

So the host controller **never started the data phase**, and nothing said why.

### The instrument that was collected and never printed

`STATUS=0x01ef0000` was quoted in section 22 and a conclusion drawn from its DAT0 bit. That was worthless:
`h.status()` is a **live read taken after the timeout and after `reset_cmd_dat()`**, so it describes a
controller that has already been cleaned up. Reading meaning into it was the same mistake as trusting a
stale counter.

The registers that would answer the question were already being collected - `seen()` is the OR of every bit
ever seen in `INTERRUPT` and `STATUS` during the wait, and `dat_window()` records whether `DAT_ACTIVE` was
ever observed - and **neither was ever printed**. They were also never reset between transfers, so they
described every transfer since boot at once, which reads as an answer and is not one. Both are fixed: reset
per transfer, printed on failure. `dat=(0, 0)` will say, in one line, that no data phase ever began.

### Reading the working code before the foreign reference

`services/block-driver/src/sdhci.rs` is a **working** SDHCI driver in this repository, on this controller
family, and it validates the section 22 structure exactly - wait once, clear the flag, then move 128 words
with no re-check:

```rust
        self.wr(INTERRUPT, INT_WRITE_RDY);
        for i in 0..128 {
            ...
            self.wr(DATA, w);
        }
```

But its command words are `CMD_READ_SINGLE = 0x1122_0010` and `CMD_WRITE_SINGLE = 0x1822_0000`: transfer
mode `0x10` and `0x00`, so **`BLK_CNT_EN` is not set** and neither is `MULTI`. It only ever moves one block,
of 512 bytes.

That is the finding. **Nothing in this project has ever issued a multi-block PIO transfer on this
controller**, and the firmware write fails precisely there. Single-block PIO is proven at 512 bytes, and
byte-mode CMD53 is proven at 4 bytes; the failing case differs in three ways at once.

### Bisect, do not guess again

Three separable candidates remain, and testing them one per boot would cost three flashes:

1. the 64-byte block **size** (the working driver only ever used 512),
2. the **`MULTI`** bit,
3. the block **count**.

So the upload now opens with a ladder, smallest difference first, and the first rung to fail names the
culprit:

| rung | mode  | MULTI | blocks | if this is the first to fail             |
|------|-------|-------|--------|------------------------------------------|
| 1    | byte  | no    | -      | the bus or the window, not block mode    |
| 2    | block | no    | 1      | the 64-byte block **size**              |
| 3    | block | yes   | 2      | the **`MULTI`** bit                     |
| 4    | block | yes   | 16     | the block **count**                     |

Every rung writes a distinct marker and **reads the first word back**, because a write that reports success
and lands nothing is a silent failure and worse than a loud one (§26.7) - and it would send the bulk write
off on a false green light. The ladder writes into the halted ARM's TCM at the firmware's own load address,
which is where the image goes next, so it needs no scratch region.

### Prediction

One of five outcomes, and each names its own cause:

1. **All four rungs pass and the upload proceeds.** Then the fault was in `write_bytes`'s chunking
   arithmetic rather than in the data path, and the bulk write's own failure message carries the byte offset
   and backplane address to place it.
2. **Rung 2 fails** - the 64-byte block size. The working driver's 512 would then be the difference, and the
   fix is to raise the transfer block size rather than to match function 1's 64-byte SDIO block size to it.
3. **Rung 3 fails** - `MULTI`. The controller does not do multi-block PIO, and the upload becomes a loop of
   single-block writes: slower, and correct.
4. **Rung 4 fails** - the block count. Some smaller maximum applies, and the chunk size comes down to it.
5. **Rung 1 fails** - the bus or the window, and everything above about block mode is beside the point.

Whichever it is, the new `INT bits seen` / `STATUS bits seen` / `data phase active at poll` line should
accompany it, and `dat=(0, 0)` versus a real window is the difference between a data phase that never
started and one that started and stalled.

## 24. A core in reset does not answer for its own memory

The ladder failed on **rung 1** - byte mode, four bytes, the mode that had already worked - and in doing so
answered a question nobody had asked yet:

```
CMD53 write of 1 word(s) to function 1 address 0x08000 failed - the data moved and the transfer
  never reported complete (arg=0x95000004 BLKSIZECNT=0x00017004 CMDTM=0x353a0002 R5 flags 0x10)
  during the wait: INT bits seen 0x00000010, STATUS bits seen 0x01ff0506, data phase active at
  poll 1..1 (it did start)
```

`INT 0x00000010` is `WRITE_RDY`. `STATUS 0x0506` carries `BUFFER_WRITE_ENABLE` (bit 10),
`WRITE_TRANSFER_ACTIVE` (bit 8), `DAT_ACTIVE` (bit 2) and `DAT_INHIBIT` (bit 1). So the data phase started,
the FIFO was ready, the word went in, and `TRANSFER_COMPLETE` never arrived.

**All three candidates the ladder was built to separate are eliminated at once** - block size, the `MULTI`
bit and the block count are all irrelevant, because the failure reproduces in plain byte mode. A ladder
built to choose between three hypotheses instead falsified all three, which is the most useful thing it
could have done and is the argument for bisecting rather than fixing.

### The one difference

The same byte-mode write had succeeded minutes earlier, to `0x18102408` - the ARM's wrapper - confirmed by
reading `RESETCTRL` back. The ladder wrote to `0x198000`, which is **TCM inside the ARM core**.

The distinction that matters is not byte versus block. It is **wrapper versus core internals**, and the
core was being held in reset.

### The reference makes the distinction explicit

`brcmf_chip_disable_arm` does not treat all ARM cores alike:

```c
	switch (id) {
	case BCMA_CORE_ARM_CM3:
		brcmf_chip_coredisable(core, 0, 0);
		break;
	case BCMA_CORE_ARM_CR4:
	case BCMA_CORE_ARM_CA7:
		cpu = container_of(core, struct brcmf_core_priv, pub);

		/* clear all IOCTL bits except HALT bit */
		val = chip->ops->read32(chip->ctx, cpu->wrapbase + BCMA_IOCTL);
		val &= ARMCR4_BCMA_IOCTL_CPUHALT;
		brcmf_chip_resetcore(core, val, ARMCR4_BCMA_IOCTL_CPUHALT,
				     ARMCR4_BCMA_IOCTL_CPUHALT);
		break;
```

A CM3 is **disabled** and stays in reset. A CR4 - which is what this chip has - is **reset**, which ends
with the core out of reset and clocked, carrying `ARMCR4_BCMA_IOCTL_CPUHALT` (0x0020) as both the reset and
post-reset `IOCTL` value so that the CPU is held halted while the core runs.

**Halting the CPU and holding the core in reset are not the same thing**, and only the first makes the TCM
reachable. A core in reset does not answer backplane accesses to its own memory: the card accepts the
command, the host FIFO drains into the controller, and the backplane transaction never completes. Which is
exactly, and only, what was measured.

`aicore::reset(halt = true)` already implemented that sequence - quoted from the vendor driver and
self-checked against `RESETCTRL` - and was simply being called one level too low. The change is **which
function is called**, so the `unsafe` count, the syscall surface and the capability set are all untouched.

The log was also lying about its own success: `aicore::reset` printed `RUNNING` for any core out of reset,
halted or not. Out-of-reset-with-CPU-halted (the state firmware is written in) and out-of-reset-with-CPU-
executing (the state it runs in) are different milestones, and a label that conflates them is the kind of
thing that costs a boot. It now names all three states.

**A divergence recorded rather than left silent (§26.14).** On *release*, Linux passes `postreset = 0` - an
`IOCTL` of zero, with no `CLOCK_EN` - while the vendor's `reset_device_core` ends with `SICF_CLOCK_EN` set.
This keeps the vendor sequence, because that driver is written for this exact chip family and `aicore.rs`
already quotes and follows it. The difference is noted so the next reader knows it was a decision.

### Prediction

1. **The ladder passes all four rungs and the upload proceeds.** Most likely, because the failure was the
   destination not answering rather than anything about the transfer, and that is now addressed for every
   rung equally.
2. **If a later rung fails**, the original three-way question is live again and rung 2, 3 or 4 names which
   part, now against a destination that does answer.
3. **If the upload completes, the release is the next thing that can fail** - and the reference has now
   confirmed why. `brcmf_chip_cr4_set_active` calls `chip->ops->activate(chip->ctx, &chip->pub, rstvec)`
   *before* restoring the ARM, and that is `brcmf_sdio_buscore_activate`, the reset-vector write that
   `aicore.rs` records as unimplemented. So `STILL IN RESET` or a core that comes up and does nothing points
   there, and it is no longer a maybe: it is a confirmed missing step held back deliberately so that this
   boot tests one change.

What is still not predicted is a working radio. A halted core that accepts 609 KB is not a radio; it is a
loaded one.

## 25. Phase 2 complete on hardware, and the difference between loaded and running

The halt level was the whole fault. On the next boot every rung passed and the upload ran:

```
core wrapper 0x18102000 out of reset: RESETCTRL 0x00000000, IOCTRL 0x00000021
  - CORE RUNNING, CPU HALTED - TCM is reachable
  rung 1 ok (byte mode, 4 bytes) - and 0xa1a10001 read back
  rung 2 ok (block mode, ONE 64-byte block, no MULTI) - and 0xb2b20002 read back
  rung 3 ok (block mode, TWO 64-byte blocks, MULTI) - and 0xc3c30003 read back
  rung 4 ok (block mode, SIXTEEN 64-byte blocks, MULTI) - and 0xd4d40004 read back
firmware written - 609309 bytes to 0x198000 in 596 command(s)
NVRAM 2074 bytes of text stripped to 1748 bytes (including the 4-byte token),
  going to 0x25f92c..0x260000 - the token is the last word of RAM
NVRAM written - 1748 bytes to 0x25f92c in 3 command(s)
core wrapper 0x18102000 out of reset: RESETCTRL 0x00000000, IOCTRL 0x00000001
  - CORE RUNNING, CPU EXECUTING
```

609,309 bytes in 596 commands in about 1.13 s. Every rung read its own marker back, so the data path is
proven to carry 16 blocks of 64 rather than merely reporting that it did. The reset vector turned out **not**
to be needed to bring the core out of reset, which the previous section had listed as a live possibility.

### What that does not prove

`RESETCTRL 0x00000000, IOCTRL 0x00000001` is a statement about the **reset controller**: the CPU is fetching.
A CPU fetching garbage reports exactly the same thing. So "the ARM is running its firmware" was an assertion
dressed as an observation, and on the strength of this project's own rules it should not have been written
that way.

The reference has a real test, and its comment is the entire idea:

```c
	/* NVRAM length at the end of memory should have been overwritten. */
	shaddr = bus->ci->rambase + bus->ci->ramsize - 4;
	rv = brcmf_sdiod_ramrw(bus->sdiodev, false, shaddr, (u8 *)&addr_le, 4);
```

The last word of RAM carries the NVRAM length token **the host wrote**, so the firmware can find and parse
its calibration at boot. Having consumed it, the firmware **overwrites that word** with the address of its
own SDPCM shared structure. So one read answers the question:

- still our token, and the firmware never ran;
- a plausible address inside TCM, and it booted - and that is where its structure lives.

It lands exactly where this driver already writes. `rambase + ramsize - 4` is `0x198000 + 0xC8000 - 4` =
`0x25FFFC`, and the NVRAM run `0x25f92c..0x260000` ends on that same word. Not a collision: the mechanism.

**The check here is stronger than the reference's**, for once. brcmfmac cannot know which token the host
wrote, so it uses a generic pattern test; this driver wrote it, so the comparison is exact - if the word
still equals the value we put there, the firmware definitively has not touched it. The value is then
range-checked inside TCM, and the shared structure's `flags` word is read and its version masked with
`0x00FF` and compared against `0x0003`, which is what the reference validates.

It also **retries where the reference does not**, and the reason is a limitation rather than a
precaution: brcmfmac reaches this point late in a longer sequence, while here the read happens milliseconds
after release, so a firmware still starting would be called dead. Twenty bounded attempts, 10 ms apart.

And if the word never changes, that is reported as a **fact, not an error** - a loaded chip whose firmware
did not start is precisely the state worth naming (§26.7), and the message says what to read next: the reset
vector in `brcmf_sdio_buscore_activate`, which is now confirmed to be called before the ARM is restored and
is still unimplemented here.

### A log that contradicted itself

`"phase 1 complete ... NO firmware is uploaded and no 802.11 exists yet"` printed **immediately after**
`"PHASE 2 COMPLETE"`. The log contradicted itself by one line.

That is the third time in this effort that the code moved on and the sentence did not - the earlier two were
a `CHIPCLKCSR` guard that rejected a value its own prediction called healthy, and a `RUNNING` label applied
to a halted core. The pattern is consistent enough to be worth naming as a hazard rather than three
accidents: **a message that asserts a state, rather than reporting one it just read, goes stale silently.**
The corrected line describes what it can see and explicitly declines to call a loaded chip a usable radio.

### Prediction

1. **The firmware is alive** - the last word changes from `0xFE4A01B5` to an address inside
   `0x198000..0x260000`, and its `flags` word reports SDPCM version 1, 2 or 3. That closes phase 2 properly
   and makes phase 3 (the control channel) the next work.
2. **The word never changes**, and the firmware did not start despite loading cleanly. Then the reset vector
   is the missing step, exactly as the failure message will say, and it is a small change: the image's first
   four bytes written to backplane address 0 before the core is restored.
3. **The word changes to something outside RAM.** Then something executed and went wrong early - a bad load
   address or a corrupted image - and the next move is to read back a few words of the image at `0x198000`
   and compare them against the blob, which the FNV machinery already makes cheap.

No outcome here is a working radio. Even outcome 1 means the firmware booted and nothing has spoken to it.

## 26. The reset vector, and a second reference when the first will not yield

The liveness check earned its place on the first boot it ran:

```
the last word of RAM still holds OUR NVRAM token 0xfe4b01b4 after 20 reads over ~200 ms,
  so THE FIRMWARE HAS NOT RUN
```

Outcome 2 of the section 25 prediction. The image and the NVRAM load perfectly, the CPU comes out of reset,
and no firmware runs. **Without that check this boot would have been reported as a success** - the line above
it still says `CORE RUNNING, CPU EXECUTING`, which is true and means nothing.

One correction to section 25, made here because the number was published: the token was predicted as
`0xFE4A01B5` and is `0xFE4B01B4`. The prediction divided 1748 by 4 instead of subtracting the four-byte token
first, so it is 436 words, not 437. Nothing depended on it - the check compares against the value actually
written rather than a computed one, which is the reason it was built that way - but the arithmetic was wrong
and is corrected rather than quietly left.

### Why address 0

The image is loaded at `0x198000`, but the CR4 **begins fetching from backplane address 0** when it comes out
of reset. The first word of the image is the branch that gets it from there to the loaded code. Without that
word written to address 0, the CPU executes whatever address 0 happens to hold, which is exactly the state
measured: fetching, and the NVRAM token untouched because no firmware ever ran to consume it.

The blob's first four bytes are `98 f1 3e b8`, so the vector is **`0xb83ef198`**. The words after it -
`99f1 fcbd`, `99f1 08be`, `99f1 14be` - are the same shape with a varying second halfword, which is a
Thumb-2 branch vector table. That is what a reset vector table should look like, and it is a weak but real
corroboration that the first word is a branch rather than data.

### Two references, because the first would not yield the function

`brcmf_sdio_buscore_activate` sits near the end of Linux's `sdio.c`, and every fetch of that file truncates
before it - three attempts, including narrow single-question prompts. Rather than reconstruct it from
memory, which is the thing this whole effort is run to avoid, the body came from **OpenBSD's `bwfm`**, a
clean-room reimplementation of the same driver:

```c
void
bwfm_sdio_buscore_activate(struct bwfm_softc *bwfm, uint32_t rstvec)
{
	struct bwfm_sdio_softc *sc = (void *)bwfm;

	bwfm_sdio_dev_write(sc, BWFM_SDPCMD_INTSTATUS, 0xFFFFFFFF);

	if (rstvec)
		bwfm_sdio_ram_read_write(sc, 0, (char *)&rstvec,
		    sizeof(rstvec), 1);
}
```

and its caller gives the value: `bwfm_chip_set_active(bwfm, *(uint32_t *)ucode)`.

**The two references agree independently on both halves**, which is worth more than either alone. Linux
supplies `rstvec = get_unaligned_le32(fw->data)` and `brcmf_chip_set_active(bus->ci, rstvec)`; OpenBSD
supplies the write itself and its address. Neither was inferred from the other.

The ordering is the reference's too: `brcmf_chip_cr4_set_active` calls `activate(..., rstvec)` and **only
then** `resetcore(core, ARMCR4_BCMA_IOCTL_CPUHALT, 0, 0)`. So the vector is written while the CPU is still
halted, before the release - which is where it goes here.

### One divergence, recorded rather than dropped

The reference's first action is to clear the SDIO device core's `INTSTATUS` with `0xFFFFFFFF`. That is
housekeeping for an interrupt path this driver does not have: every transfer here is polled, and the EROM
walk has not identified the SDIOD core's base. Stale bits in a register nobody reads cannot affect a polled
driver, so the write is **omitted deliberately** and noted at the point of difference (§26.14). It becomes
required the moment this driver takes SDIO interrupts.

No new syscall, no new capability, no `unsafe`. It reuses `write_bytes`, which already handles address 0
correctly: `win_off = (0 & 0x7FFF) | 0x8000`, byte mode, four bytes.

### Prediction

The log should first show `reset vector 0xb83ef198 (the image's first four bytes) going to backplane
address 0`. Then:

1. **The firmware is alive** - the last word of RAM changes from `0xfe4b01b4` to an address inside
   `0x198000..0x260000`, and its `flags` word reports SDPCM version 1, 2 or 3. That is phase 2 genuinely
   complete: a chip running its own firmware, and phase 3 (the control channel) becomes the next work.
2. **Still our token.** Then the vector was necessary but not sufficient, and the next thing to read is what
   else `brcmf_sdio_download_firmware` does between the NVRAM write and the release - the candidates being
   the `INTSTATUS` clear omitted above, and whether the chip's clock must be moved from ALP to HT before the
   core is let go.
3. **A value outside RAM.** Something executed and went wrong early, and the next move is reading image
   words back from `0x198000` to compare against the blob, which the existing FNV machinery makes cheap.

Outcome 1 is still not a working radio. It is a chip that has booted its own firmware with nothing yet
talking to it.

## 27. Phase 2 complete - the radio is running its own firmware

The reset vector was the last missing step, and the chip confirmed it on the first read:

```
reset vector 0xb83ef198 (the image's first four bytes) going to backplane address 0
  - the CR4 fetches from there on release, not from 0x198000
reset vector written - 4 bytes to 0x000000 in 1 command(s)
releasing the ARM
core wrapper 0x18102000 out of reset: RESETCTRL 0x00000000, IOCTRL 0x00000001
  - CORE RUNNING, CPU EXECUTING
THE FIRMWARE IS ALIVE - it overwrote our NVRAM token with 0x00201cc0 after 1 read(s),
  and its shared structure reports flags 0x00000001 (SDPCM version 1, up to 3 understood)
PHASE 2 COMPLETE
```

**Why this is evidence and not a claim.** The value that changed was a token *this driver wrote* -
`0xfe4b01b4`, the NVRAM length the firmware needed in order to find its own calibration - so nothing else in
the system could have produced the change. It changed to `0x00201cc0`, inside the chip's RAM
(`0x198000..0x260000`), which the check range-tests before believing. Reading `flags` at that address gave
`0x00000001`: SDPCM version 1, with no trap or assert bits set. And it happened on the **first** read,
needing none of the twenty bounded retries.

Two independent windowed reads are in the log with their readbacks verified - the window to `0x258000` for
the token at `0x25FFFC`, then to `0x200000` for `flags` at `0x201cc0`.

### Where phase 2 stands, as measured

| fact | value |
|------|-------|
| chip | `0x4345` rev 6 pkg 2, identified from its own silicon |
| firmware | `brcmfmac43455-sdio`, chosen from brcmfmac's revision bitmask |
| image | 609,309 bytes to `0x198000` in 596 commands |
| NVRAM | 2,074 bytes of text stripped to 1,748, at `0x25f92c..0x260000` |
| reset vector | `0xb83ef198` to backplane address 0 |
| ARM CR4 | rev 9 at `0x18002000`, wrapper `0x18102000` (derived, self-checked) |
| TCM | 800 KiB in 8 banks |
| shared structure | `0x00201cc0`, flags `0x00000001` |
| upload time | about 1.13 s |

### The scoreboard on method, since it is the point

Seven boots from the first upload attempt to a running firmware. What each one cost is worth recording,
because the pattern is one-sided:

- **One boot lost to theorising.** Section 22 reasoned from the symptom to a per-block FIFO fix. The fix was a
  real latent bug and kept, but it was not the cause, and the failure message had said so all along.
- **Every fix that worked came from a quoted source.** The halt level from `brcmf_chip_disable_arm`'s CR4
  branch; the liveness test from the reference's own comment; the reset vector from OpenBSD's `bwfm` after
  Linux's `sdio.c` truncated three times.
- **The one instrument change was worth three boots.** The ladder was built to choose between three
  candidates and instead falsified all three at once, because it reproduced the failure in the simplest mode.
  A bisection that eliminates every hypothesis is more useful than a fix that confirms one.
- **Three stale log messages** were found and corrected, each asserting a state rather than reporting one it
  had read. That class of bug is now named rather than treated as three accidents.

### What is NOT done

There is no control channel, so nothing has asked the firmware anything. `wifi list` cannot work and the
shell still answers `unavailable`. A chip running its own firmware is not a usable radio; it is a
prerequisite.

Phase 3 is that conversation: enable SDIO function 2, bring the chip to its HT clock, and speak SDPCM/BCDC
over it. The first verifiable result will be a value only the firmware can supply - its own MAC address.

## 28. Stage 12 - the bus, and a limitation that was never real

Phase 3 begins with the four steps between a running firmware and a bus that could carry a frame. All four
come from OpenBSD's `bwfm`, quoted:

```c
bwfm_sdio_clkctl(sc, CLK_AVAIL, 0);
bwfm_sdio_write_1(sc, BWFM_SDIO_FUNC1_CHIPCLKCSR, clk |
    BWFM_SDIO_FUNC1_CHIPCLKCSR_FORCE_HT);
bwfm_sdio_dev_write(sc, SDPCMD_TOSBMAILBOXDATA,
    SDPCM_PROT_VERSION << SDPCM_PROT_VERSION_SHIFT);
sdmmc_io_set_blocklen(sc->sc_sf[2], 512);
sdmmc_io_function_enable(sc->sc_sf[2])
```

with the offsets and values from its header:

```c
#define SDPCM_PROT_VERSION			4
#define SDPCM_PROT_VERSION_SHIFT		16
#define SDPCMD_INTSTATUS			0x020
#define SDPCMD_TOSBMAILBOXDATA			0x048
```

That header also gave the `CHIPCLKCSR` bits - `FORCE_ALP 0x01`, `FORCE_HT 0x02`, `ALP_AVAIL_REQ 0x08`,
`HT_AVAIL_REQ 0x10`, `ALP_AVAIL 0x40`, `HT_AVAIL 0x80` - which **match this driver's `clk` module exactly**.
A third independent confirmation of the constants that cost six boots to get right in section 19.

### Every step is checked by something the chip says

1. **The HT clock.** The whole upload ran on ALP, the low-power clock the backplane needed. Frames need HT.
   Confirmed by `CHIPCLKCSR` reporting `HT_AVAIL` (0x80) - the chip saying the clock is up, not that the
   request was accepted - and `FORCE_HT` is written only after that, which is the reference's order.
2. **The SDIO core's `INTSTATUS`, cleared**, discarding bits the firmware's own start-up left set.
3. **The protocol version to the mailbox**: `4 << 16` = `0x0004_0000`.
4. **Function 2**, at 512-byte blocks, checked by the `IOR` ready bit that `enable_function` already polls.

The HT clock is **reported but not required**. The entire upload ran on ALP, so a chip that will not raise HT
is degraded rather than dead, and refusing to continue would erase that distinction. Function 2 coming ready
is required, because it is the one outcome that makes a frame possible.

### Two version numbers that are not the same number

The firmware reported shared-structure **version 1**; the host announces protocol **version 4**. Those are
the shared-memory *layout* version and the *framing* protocol version, and nothing but the names suggests
they should agree. Recorded because conflating them is a mistake available to the next reader for free.

### A limitation I recorded that the log had already disproved

Section 26 omitted the `INTSTATUS` clear and justified it: *"the EROM walk has not identified the SDIOD
core's base."* **That was false when it was written.** The same boot log contains:

```
core 0x829 rev 21  base 0x18004000 wrap 0x18104000  SDIO device
```

The walk found it, `core_id::SDIO_DEV` already existed, and the driver printed its name. What was actually
true is narrower and duller: the walk found the core and **nothing held on to it**, because `Cores` kept only
`arm` and `mem`. I reached for a limitation instead of checking, and §26.7 is explicit that a recorded gap is
supposed to be a real one - a false limitation is worse than an unrecorded one, because the next reader
believes it and stops looking.

`Cores` now keeps `sdiod`, the clear is implemented where the reference puts it, and the note in `upload.rs`
is corrected at the point it was made rather than quietly deleted.

That is the fourth self-contradicting statement in this effort, and the first three all had the same
shape - a message asserting a state instead of reporting a read one. This one is the same error one level
up: **a claim about the system made without querying the system.** The instrument was right there in the log.

### Prediction

1. **The bus comes up.** `CHIPCLKCSR` gains `HT_AVAIL`, the mailbox write succeeds, and function 2 reports
   ready - `the bus is up for frames - function 2 ready at 512 bytes a block`. Then the next step is the
   SDPCM and BCDC headers, and the first thing worth asking the firmware is its own MAC address.
2. **HT never arrives.** The line says so and the bus continues on ALP, which is what carried 609 KB, so
   function 2 should still come ready. A degraded-but-working bus is a legitimate outcome here, not a
   failure.
3. **Function 2 never reports ready.** Then the data path is the problem while the firmware and backplane are
   demonstrably fine, and the next reads are whether the firmware must be given something more before it
   enables its own data function - the SDPCM shared structure's other fields become relevant, and this
   driver can already reach them at `0x201cc0`.

None of these sends a frame. This stage ends with a bus that could carry one.

## 29. The bus is up, and the first question

Stage 12 passed on every step, each confirmed by the chip rather than by a write landing:

```
the chip is on its HT clock - CHIPCLKCSR 0x69 -> 0xf9 after 1 read(s), HT_AVAIL set, then forced (0xfb)
announced SDPCM protocol version 4 to the firmware (0x00040000 -> mailbox 0x18004048),
  and cleared the SDIO core's INTSTATUS
function 2 enabled and READY (IOE 0x02 -> 0x06, after 1 read(s) of IOR)
the bus is up for frames - function 2 ready at 512 bytes a block, protocol announced, clock HT
```

`0x69 -> 0xf9` decodes without slack: the `INIT` word this driver writes is `0x29`
(`FORCE_HW_CLKREQ_OFF | ALP_AVAIL_REQ | FORCE_ALP`), plus `HT_AVAIL_REQ` is `0x39`, and the chip added
`ALP_AVAIL` (`0x40`) and `HT_AVAIL` (`0x80`) itself. `FORCE_HT` then gives `0xfb`. `IOE 0x02 -> 0x06` is
function 1 plus function 2.

### Stage 13: three headers, all quoted

A control frame is a hardware header, a software header, a BCDC command header and a payload. Every field
is from a reference, because a wrong field produces a frame the firmware ignores in silence - the worst
failure shape available here.

From OpenBSD's `bwfm`:

```c
struct bwfm_sdio_hwhdr {  uint16_t frmlen;  uint16_t cksum;  };

struct bwfm_sdio_swhdr {
	uint8_t seqnr;    uint8_t chanflag;  uint8_t nextlen;  uint8_t dataoff;
	uint8_t flowctl;  uint8_t maxseqnr;  uint16_t res0;
};
```

and from Linux's `bcdc.c`, which unlike `sdio.c` does not truncate:

```c
struct brcmf_proto_bcdc_dcmd {
	__le32 cmd;	__le32 len;	__le32 flags;	__le32 status;
};
#define BCDC_DCMD_ERROR		0x01
#define BCDC_DCMD_ID_MASK	0xFFFF0000
#define BCDC_DCMD_ID_SHIFT	16
```

with `BRCMF_C_GET_VAR 262` from `fwil.h`.

### The one fact that guessing would have got wrong

```c
addr = sc->sc_cc->co_base;
bwfm_sdio_backplane(sc, addr);
addr &= BWFM_SDIO_SB_OFT_ADDR_MASK;
addr |= BWFM_SDIO_SB_ACCESS_2_4B_FLAG;
if (write)
	err = bwfm_sdio_buf_write(sc, sc->sc_sf[2], addr, data, size);
```

A frame goes to **function 2 with the backplane window set to the CHIPCOMMON core base**, `0x18000000` -
not to address 0, and not to the firmware's RAM. The resulting offset is `0x8000`, which looks identical to
every other access this driver makes for a completely different reason. That coincidence is exactly what
would have made a wrong guess look plausible.

Padding is also not the obvious rule:

```c
len = sizeof(*hwhdr) + sizeof(*swhdr) + m->m_len;
if (len > 512 && (len % 512) != 0)
	roundto = 512;
else
	roundto = 4;
```

Short frames pad to four bytes. Only a frame both longer than a block and not a whole number of blocks
rounds to 512.

### What is checked, because a silent wrong answer is the hazard

- **The hardware header validates itself**: `frmlen ^ cksum` must be `0xFFFF`, which is the reference's own
  test and the only way to tell a real frame from a FIFO read that found nothing. So a reply is *waited for*
  by re-reading until the checksum holds, not assumed ready.
- **The reply's request id must match.** A mismatch is another exchange's answer, and it is discarded rather
  than parsed. This is the same failure the `fs` protocol needed a correlation tag to fix.
- **`BCDC_DCMD_ERROR` means the firmware refused**, and `status` carries its reason. Reported, never
  swallowed.
- **The MAC is sanity-checked.** All-zero and all-`0xFF` are rejected: both are what a successful exchange
  that returned nothing looks like, and printing one as the radio's address would be precisely the silent
  wrong answer this effort keeps being reshaped to avoid.

The question asked is `cur_etheraddr`, because **only the firmware knows it**. No host-side arithmetic can
fabricate a plausible MAC, so a correct-looking answer is real evidence rather than a self-consistent
guess - the same reasoning as the NVRAM-token liveness test.

### Two gates that improved the code rather than being silenced

The duplicate-constant check refused this work twice, both times correctly. `DATA_FUNC` was declared in
`bus.rs` and `ctrl.rs` with the same value, so it now lives once in `sdio.rs`, where a fact about the SDIO
card belongs. And `TRIES` existed in three files with two different values - "two facts wearing one name".
They are now `RESET_TRIES`, `HT_TRIES`, `REPLY_TRIES` and `LIVENESS_TRIES`, which is better code than what
the gate rejected. A third pair inside `sdio.rs` that the cross-file rule could not see (one name for the
OCR poll count and the function-ready count, 100 and 500) was found while fixing the first two and split
into an OCR try count and a ready count. Both are durations now: section 45 made the ready count three
seconds, and `gs::driver` step 1d (`docs/driver-library.md`) made the OCR count a second, as
`OCR_WAIT` and `READY_WAIT`.

A general `read_extended` also had to be written: the read side of CMD53 stopped at four bytes, because every
backplane read is a single word. A reply header is twelve.

### Prediction

1. **The radio answers.** `THE RADIO ANSWERED - its MAC address is xx:xx:xx:xx:xx:xx`, with a first byte
   whose low bit is 0 (a unicast address) and very likely a Broadcom or Raspberry Pi OUI - `b8:27:eb`,
   `dc:a6:32` or `e4:5f:01` are the Pi Foundation's. That is the control channel working end to end, and
   `wifi list` becomes reachable work rather than a stub.
2. **No valid reply frame.** The header never satisfies `frmlen ^ cksum == 0xFFFF` within 200 reads. Then the
   frame reached the bus and the firmware did not answer it, and the next thing to read is whether the
   firmware must be brought "up" first - `BRCMF_C_UP` is 2, and brcmfmac issues it during bus init.
3. **A reply that fails one of the checks** - wrong request id, the error flag set with a status, or six
   bytes of zeros. Each of those prints what it saw, and each points somewhere different: an id mismatch at
   the framing, an error status at the iovar name, zeros at the payload offset arithmetic.

Outcome 1 is the first moment this is a radio rather than a loaded chip.

## 30. Phase 3 designed at the desk: the event path and `escan`

Written away from the bench, so nothing here is claimed to work. It is the mechanism, read out of the
references, with the offsets computed from the declarations rather than from memory - and, kept separate
on purpose, the parts that are **our** design decisions rather than the chip's requirements (§26.14).

### 30.1 A scan is not a request with a reply

Everything the driver does so far is synchronous: send a BCDC command, read the answer. **A scan is not
that shape.** `escan` is a *set* that returns immediately, and the results arrive afterwards as a stream of
**events** the firmware sends unprompted. That is the new mechanism in phase 3, and it is the whole reason
this phase is the one with real unknowns in it.

The SDIO receive path tells the three cases apart by the software header's channel:

```c
switch (swhdr->chanflag & BWFM_SDIO_SWHDR_CHANNEL_MASK) {
case BWFM_SDIO_SWHDR_CHANNEL_CONTROL:
	sc->sc_sc.sc_proto_ops->proto_rxctl(...);
case BWFM_SDIO_SWHDR_CHANNEL_EVENT:
case BWFM_SDIO_SWHDR_CHANNEL_DATA:
	sc->sc_sc.sc_proto_ops->proto_rx(&sc->sc_sc, m, &ml);
```

So channel 0 is a control reply - what `ctrl.rs` already reads - and channels 1 and 2 are frames. An event
is a **pseudo-ethernet frame** carrying ethertype `BWFM_ETHERTYPE_LINK_CTL` (`0x886c`), not a bare struct.

### 30.2 The layouts, quoted, and the offsets computed from them

The event frame, from the quoted declarations:

```c
struct bwfm_ethhdr {
	uint16_t subtype;  uint16_t length;  uint8_t version;
	uint8_t oui[3];    uint16_t usr_subtype;
} __packed;

struct bwfm_event_msg {
	uint16_t version;  uint16_t flags;    uint32_t event_type;
	uint32_t status;   uint32_t reason;   uint32_t auth_type;
	uint32_t datalen;  struct ether_addr addr;
	char ifname[IFNAMSIZ];  uint8_t ifidx;  uint8_t bsscfgidx;
} __packed;
```

| field | offset in frame | how |
|---|---|---|
| ethernet destination / source | 0, 6 | `ether_header` is 14 bytes |
| ethertype (`0x886c`) | 12 | |
| `bwfm_ethhdr` | 14 | 2+2+1+3+2 = **10 bytes** packed |
| `bwfm_event_msg` | 24 | 2+2+4+4+4+4+4+6+16+1+1 = **48 bytes** packed |
| `event_type` | 24 + 4 = **28** | |
| `status` | **32** | |
| `datalen` | **44** | |
| event payload | 24 + 48 = **72** | `datalen` bytes |

Event codes, quoted: `BWFM_E_ESCAN_RESULT = 69`, `BWFM_E_LINK = 16`, `BWFM_E_SET_SSID = 0`,
`BWFM_E_ASSOC = 7`.

The payload of an escan-result event:

```c
struct bwfm_escan_results {
	uint32_t buflen;  uint32_t version;
	uint16_t sync_id; uint16_t bss_count;
	struct bwfm_bss_info bss_info[];
};
```

so `buflen` at 0, `bss_count` at **10**, and the first `bss_info` at **12**.

And each result, with offsets computed from the quoted declaration (`BWFM_MAX_SSID_LEN 32`,
`BWFM_MCSSET_LEN 16`):

| field | offset | type |
|---|---|---|
| `version` | 0 | `uint32_t` |
| `length` | **4** | `uint32_t` - **step to the next entry with THIS, never `sizeof`** |
| `bssid` | **8** | `uint8_t[6]` |
| `capability` | 16 | `uint16_t` |
| `ssid_len` | **18** | `uint8_t` |
| `ssid` | **19** | `uint8_t[32]` |
| `chanspec` | **72** | `uint16_t` |
| `rssi` | **78** | `uint16_t` on the wire, read as **signed** dBm |
| `ie_offset` | 116 | `uint16_t` |
| `ie_length` | 120 | `uint32_t` |
| total | 126 | |

**`length` at offset 4 is the one that matters for correctness.** The struct has versions, so iterating by a
compiled-in `sizeof` would walk off alignment the moment the firmware sends a longer one. The reference's own
field is the answer, and using it costs nothing.

### 30.3 What to send, values quoted from Linux

```c
params_le->bss_type = DOT11_BSSTYPE_ANY;
params_le->scan_type = cpu_to_le32(BRCMF_SCANTYPE_ACTIVE);
params_le->nprobes = cpu_to_le32(-1);
params_le->active_time = cpu_to_le32(-1);
params_le->passive_time = cpu_to_le32(-1);
params_le->home_time = cpu_to_le32(-1);
eth_broadcast_addr(params_le->bssid);
params->action = cpu_to_le16(WL_ESCAN_ACTION_START);
params->sync_id = cpu_to_le16(0x1234);
```

set through the iovar `"escan"`. `WL_ESCAN_ACTION_START` is 1, `BWFM_SCANTYPE_ACTIVE` is 0,
`DOT11_BSSTYPE_ANY` is 2. The `-1`s mean "firmware default", which is what we want: a scan tuned by hand is
an optimisation of something that does not work yet.

The request struct, offsets computed from OpenBSD's declarations plus the confirmed
`struct bwfm_ssid { uint32_t len; uint8_t ssid[32]; }` (36 bytes):

| field | offset |
|---|---|
| `version` | 0 |
| `action` | 4 |
| `sync_id` | 6 |
| `ssid.len` | 8 |
| `ssid.ssid[32]` | 12 |
| `bssid[6]` | **44** |
| `bss_type` | 50 |
| `scan_type` | 51 |
| `nprobes` | 52 |
| `active_time` | 56 |
| `passive_time` | 60 |
| `home_time` | 64 |
| `channel_num` | 68 |
| total (no channel list) | **72** |

### 30.4 A conflict between the two references, recorded rather than resolved

OpenBSD declares `uint8_t bss_type; uint8_t scan_type;`. Linux's quoted assignment is
`params_le->scan_type = cpu_to_le32(BRCMF_SCANTYPE_ACTIVE)` - a **32-bit** store. Those cannot both describe
the same struct, and the explanation is that **there are versioned variants**: OpenBSD has `bwfm_scan_v0` and
`bwfm_scan_v2` and dispatches between them, and newer brcmfmac has a v2 params struct too.

This is not resolvable from the desk, so it is written down instead of decided: the plan is **v0, the layout
quoted above**, because the 43455 with this firmware revision is the older part that OpenBSD's v0 path
serves. If a scan is accepted and returns no results, **the params version is the first thing to change**,
not the values. Recorded here so that boot is a two-minute change rather than a fresh investigation
(§26.7).

### 30.5 The Godspeed half - what we deliberately do NOT borrow

The mechanism above is the firmware's requirement and is copied exactly. Everything in *how Linux organises
a scan* is that system's answer to that system's constraints, and none of it comes along (§26.14).

| Linux does | Godspeed does | why |
|---|---|---|
| Accumulates results in a dynamically grown list | A **fixed array of 32 results**, on the stack, and says loudly when it is full | §26.6.1 - no heap; the bound is readable off the source |
| Delivers results by callback into `cfg80211` through a workqueue | The driver **reads frames in its own loop**; there is no callback and no work queue | §26.4 - no hidden control flow; and there is no `cfg80211` to deliver to (a stated non-goal, §9) |
| Keeps scan state in driver-wide structures | Scan state is **owned by the one call doing the scan** | §3.8 - state is explicit and owned |
| Wakes waiters on an unbounded wait | A **deadline**, and whatever arrived by then is reported as a partial result | §26.6 - every wait is bounded, and a partial answer labelled partial is honest |
| Returns `-ETIMEDOUT` and discards | Reports **how many events arrived and how many results were kept**, always | §26.7 - a failed recovery stays as visible as the original failure |

The one place this is a genuine judgement rather than a rule: **a scan has a real duration** - the radio must
dwell on each channel - so a deadline here is not the "wait on time instead of truth" that Commandment VIII
forbids. The truth being waited on is *the firmware saying the scan is complete* (an escan-result event with
`status` marking completion); the deadline is the bound underneath it, and which of the two ended the wait is
printed.

### 30.6 The ladder, because this is three unknowns again

The same discipline as §23, which falsified all three of its hypotheses at once. Three things could be wrong
here - the event framing offsets, the escan request layout, and the result parsing - so they are separated:

| rung | proves | needs |
|---|---|---|
| **A** | An event frame can be received and its header parsed at all - print `event_type`, `status`, `datalen` for **any** event | the §30.2 offsets only |
| **B** | `escan` is accepted and the firmware answers with `BWFM_E_ESCAN_RESULT` (69) events - count them | the §30.3 layout |
| **C** | Results parse - print SSID, BSSID, channel, RSSI | the `bss_info` offsets |

Rung A is worth its own flash: it needs no scan at all, and a `BWFM_E_SET_SSID` or link event may well arrive
unprompted. **A `datalen` consistent with the frame length is the self-check** - if the offsets are wrong,
`datalen` is nonsense, so the arithmetic checks itself the way the `frmlen ^ cksum` test does.

### 30.7 What this does NOT design

The credential path for phase 4 is still open, and it is the one decision that is genuinely the operator's
rather than mine - §6 has the question. Nothing in phase 3 touches a secret, which is exactly why §7 put a
scan before association.

## 31. The chip answered and the driver threw it away

> Where this section says glommed frames are dropped as the reference drops them, section 39 supersedes
> it: they are read now, by the descriptor's chunk lengths, and the association events travel inside them.

Stage 13 sent its request and reported neither success nor failure at first glance. It did report, and the
line is the whole finding:

```
asking the firmware for `cur_etheraddr` - 42 byte frame padded to 44, seq 0, request id 1
the reply header validated but describes an impossible frame - frmlen 12, dataoff 12 (headers are 4+8, buffer is 512)
```

**`frmlen 12, dataoff 12` is not impossible. It is a header-only frame**, exactly `HWHDR + SWHDR`, with the
payload starting where the frame ends - which is what the chip sends for flow control. Its checksum
validated, so it was a genuine frame, arriving **32 ms after the request**. The chip was talking to us and
the driver called its first word nonsense.

### The bug is a duplicate, and that is the part worth keeping

`read_frame` handles this case, explicitly, with a comment saying why:

```rust
    if rest == 0 {
        // A header-only frame is legitimate - the chip uses them for flow control - and carries no payload.
        return Some((chanflag, 0));
    }
```

`query_iovar` was written **first**, hand-rolls the same header read and the same two-step body read, and
never learned it. Two readers of one wire, one of them out of date.

That is the failure the duplicate-constant gate exists to prevent, one level up. It caught `DATA_FUNC`
declared twice and `TRIES` meaning two things, and both catches improved the code - but it watches
**constants**, and nothing in the enforcement layer watches duplicated **logic**. This bug lived in exactly
that blind spot, and it is worth recording as a blind spot rather than as one mistake: the gate's own
principle (one fact, one place) was being violated by a whole function while the gate reported green.

The fix is subtraction. `query_iovar` calls `read_frame`. One parser, less code, and the next thing learned
about the wire cannot be learned by only half the driver. `set_iovar` and `scan::collect` were both written
later and already use it, so `query_iovar` was the only offender - which is itself the tell: the oldest copy
is the one that rots.

### The loop shape was wrong independently of that

Worth separating, because fixing only the header-only case would have left this. The retry loop broke out of
the wait on the **first** checksum-valid header and judged it afterwards, so **any** frame that was not the
reply ended the exchange - an event, a flow-control frame, or another exchange's late answer would all have
done it.

A frame that is not the reply is now **skipped and the wait continues**, and the three reasons are counted
separately. On timeout the message says how many frames arrived and why each was passed over, because
"nothing answered" and "forty frames arrived and none matched" are different failures that were previously
indistinguishable.

Note the id mismatch also became a skip rather than a failure. That is deliberate: id matching exists to stop
a protocol going one reply out of step, and a stale reply arriving late is precisely the case it guards
against - so discarding it and waiting is the point, not an error.

### Prediction

1. **The radio answers** - `THE RADIO ANSWERED - its MAC address is ...`, probably preceded by
   `the reply arrived after N other frame(s)` naming the flow-control frames skipped on the way. Then stage
   14 runs and phase 3's own three outcomes apply (§30.6).
2. **`no reply ... N frame(s) DID arrive`** - the wire works, frames flow, and none is a BCDC reply to this
   request. That points at the request rather than the transport, and the counts say which way: frames that
   are all header-only means the firmware never produced an answer, while frames on another channel means it
   is answering somewhere this is not looking.
3. **No frames at all** - then the 32 ms frame in this boot's log was the last thing the chip had to say, and
   the question is what changed between reading it and not.

What is no longer possible is the outcome that actually happened: a real frame being called impossible and
ending the exchange.

## 32. A control frame arrived and did not match

The duplicate parser is gone and the exchange got further. It did not succeed, but it failed with a census
rather than a shrug:

```
asking the firmware for `cur_etheraddr` - 42 byte frame padded to 44, seq 0, request id 1
no reply to `cur_etheraddr` across 200 reads. 3 frame(s) DID arrive:
  0 header-only (flow control), 2 on another channel, 1 from another exchange
```

Read that carefully, because it is a large step forward disguised as a failure:

- **Three frames arrived.** The transport carries frames in both directions. That was never proven before.
- **Zero were header-only.** So the thing that broke the last boot is not even occurring here - the frames
  arriving now have real payloads.
- **Two were on another channel.** Event or data frames. The firmware is volunteering traffic.
- **One was on the CONTROL channel, long enough to hold a BCDC header, and its request id was not 1.**

Nothing else in this boot issues a BCDC command. **That frame is almost certainly the reply**, and its id is
being read from somewhere other than where the firmware wrote it.

### Three hypotheses, and the counters distinguish none of them

1. The id is at a different offset within the BCDC header.
2. The BCDC header does not start where this driver assumes - the reply's `dataoff` is not the 12 a request
   uses, so `read_frame` hands back bytes that begin somewhere else.
3. The firmware uses a different id convention than the one `brcmf_proto_bcdc_query_dcmd` implies.

All three produce exactly the line above. The census counts frames; it does not describe them, and that is
the gap.

### So this boot describes them

For the first four frames of an exchange - whatever channel they arrive on, and **before** any rule decides
whether they match - the driver now prints the channel, the length, the BCDC fields as this driver would
decode them, and the leading 32 bytes as hex.

The hex is the part that settles it, because every decoded field above it assumes an offset and the bytes
assume nothing. `cur_etheraddr` is `BRCMF_C_GET_VAR`, which is **262** - `0x00000106` little-endian, so
`06 01 00 00`. Wherever that pattern appears in the dump is where the BCDC header actually starts, and the
answer is then arithmetic rather than a theory. If it appears at offset 0, hypothesis 2 is dead and the id
field is the problem. If it appears later, hypothesis 2 is right and the offset is measurable directly.

One flash to separate three hypotheses, instead of three flashes to test them in turn. That is the §23
ladder reasoning, which falsified all three of its own candidates at once and has been the most productive
single change in this effort.

**Describing happens before the channel check on purpose.** A frame skipped by a rule that is itself wrong
would otherwise never be seen, which is how the header-only bug survived a whole boot in §31.

**Bounded, because an instrument that floods is not an instrument.** Only the first four frames of an
exchange are described; a 200-iteration loop that dumped every frame would bury the answer in its own
output, and a console flood jams the queue the shell reads from.

### Prediction

1. **`06 01 00 00` appears at offset 0** of the control frame, with a readable `len` and `status`, and the id
   field holds something other than 1. Then the header is where this driver thinks and the id convention or
   offset is wrong - a one-line fix, and the dump will show which byte pair carries it.
2. **`06 01 00 00` appears at some other offset** - hypothesis 2. The reply's `dataoff` differs from a
   request's, and the fix is to honour it rather than assume 12. The offset is read straight off the dump.
3. **It does not appear at all**, and the control frame is something else entirely - an asynchronous status
   message the firmware sends unprompted. Then the reply genuinely never came, and the 2 frames on another
   channel become the interesting ones, because one of them may be carrying it.

In all three cases the next change is determined by the output rather than chosen from a list, which is the
only thing that has reliably worked here.

## 33. The firmware was answering all along, and saying exactly what was wrong

With the header fields finally printed, the frame explains itself completely:

```
frame 3: channel 0x00 (CONTROL), frmlen 28, dataoff 12, seq 2, nextlen 0
  -> 16 byte(s) of body, payload at +0 for 16 byte(s)
  read as BCDC: cmd 1601336675 len 0 flags 0x64640001 status 0xffffffe8 -> id 25700, error true
  [00] 63 75 72 5f 00 00 00 00
  [08] 01 00 64 64 e8 ff ff ff
```

`frmlen 28` is 12 + 16: a bare BCDC header with `len 0` and no payload, which is what a **rejection** looks
like. And its contents are this driver's own request payload, parsed as a header:

| the firmware read | from my bytes | what came back | observed |
|---|---|---|---|
| `cmd` | `"cur_"` | echoed verbatim | `63 75 72 5f` |
| `len` | `"ethe"` | zeroed | `00 00 00 00` |
| `flags` | `"radd"` | upper half kept, ERROR set | `01 00 64 64` |
| `status` | - | its own error code | `e8 ff ff ff` = **-24** |

`flags = 0x64640001` is exactly `(0x6464 << 16) | BCDC_DCMD_ERROR`, and `0x6464` is the ASCII `dd` from
`cur_ether`**`add`**`r`. **The "id 25700" that looked like nonsense for three boots was two letters of the
iovar name sitting in the request-id field.**

### The cause: one byte, and the reference had already said so

`swhdr.dataoff` says where the **protocol data** begins - the BCDC header, immediately after the SDIO
hardware and software headers:

```c
swhdr->dataoff = sizeof(*hwhdr) + sizeof(*swhdr);
```

4 + 8 = **12**. This driver wrote `PAYLOAD_AT`, which is 28 - past the BCDC header, at the iovar name. So the
firmware skipped the real header and read `"cur_etheraddr\0"` as one, found `"cur_"` where a command number
belongs, and rejected it.

The receive side was **right the whole time** (`off = dataoff - 12`, matching the reference exactly), which is
why the earlier boots kept clearing this code: nothing was wrong except one transmitted byte.

### The part worth keeping: a correct quote next to contradicting code reads as verification

That exact line from the reference was **already in this module's documentation**, quoted correctly. And the
comment beside the write said `dataoff` "points past all three headers, **as the reference sets it**". It
points past two.

So the reference was found, read, transcribed accurately into the doc comment, and then implemented as its
opposite - with a citation attached that made the wrong value look checked. That is worse than not having
read it at all, because every subsequent pass over this code saw an authority for the mistake. Three boots
went into hypotheses about the *receive* path while the transmit value sat there with a footnote.

The guard is structural rather than a resolution to be careful: `DATA_OFF` is now its own named constant with
the reference quoted on it and the trap written down, and both call sites use it - `set_iovar` had the same
bug because it was written by copying `query_iovar`. The duplicate-constant gate would have caught two
literals; it cannot catch one constant used for two different meanings, which is what `PAYLOAD_AT` had become.

### What found it

Not reasoning - the instrument. Printing `frmlen`, `dataoff`, `seq` and `nextlen`, which the driver had been
sending and receiving all along and never showing. Four boots of permuting byte alignments produced four
wrong answers; one boot of printing the header produced a complete account of all sixteen bytes.

Also eliminated by this boot, cleanly: the settling gap and the data timeout from §32 changed nothing (byte
for byte identical output), so they are not this bug. They stay, because they are requirements of the
controller that this driver was ignoring.

### Prediction

1. **`THE RADIO ANSWERED - its MAC address is xx:xx:xx:xx:xx:xx`**, with an even first byte and plausibly a
   Raspberry Pi OUI. Every byte of the failure is now explained, which is the strongest position any fix in
   this effort has started from.
2. **A different error status.** Then the frame reaches the parser correctly and the firmware objects to
   something else - the `len` field's meaning (lower 16 is the output buffer length), or the interface index.
   The status code will say, and this time the decode will be trustworthy because the header is in the right
   place.
3. **No reply at all**, which would be a surprise now, and would mean the rejection was the only thing the
   firmware ever intended to send.

If outcome 1 lands, stage 14 runs immediately after and phase 3 gets its first test.

## 34. The control channel works, and the firmware starts naming its objections

```
frame 3: channel 0x00 (CONTROL), frmlen 42, dataoff 12, seq 2, nextlen 0 -> 30 byte(s) of body
  read as BCDC: cmd 262 len 14 flags 0x00010000 status 0x00000000 -> id 1, set false, error false
  [16] 98 fe 54 1c dc 54 68 65
  [24] 72 61 64 64 72 00 00 00
the reply arrived after 2 other frame(s) - 0 header-only, 2 on another channel, 0 from another exchange
THE RADIO ANSWERED - its MAC address is xx:xx:xx:xx:xx:54
```

Everything decodes: command 262 echoed, `len 14`, `id 1`, no error, the MAC at payload offset 0, and the
untouched tail of the request buffer (`heraddr\0`) behind it - which is what a six-byte answer written into a
fourteen-byte buffer looks like.

**Independently corroborated**, which matters because the alternative is a self-consistent parse of our own
bytes. The Raspberry Pi OS boot carried `smsc95xx.macaddr=XX:XX:XX:XX:XX:53` on its kernel command line - the
ethernet MAC. Ours is `...dc:54`. Consecutive, which is how the Pi Foundation assigns the pair. Two unrelated
sources agree.

### `escan` refused: BCME_NOTUP

```
the firmware REFUSED `escan` - status 0xfffffffc (-4)
```

The error table decodes it - `brcmf_fil_errstr` index 4 is `BCME_NOTUP`: **the interface is down.** A scan
cannot start on a down interface, and `BRCMF_C_UP` (command 2) raises it; brcmfmac issues that during
bring-up before anything else touches the radio.

**And -24 was `BCME_BADLEN`**, which confirms §33 from the firmware's own mouth. That account was
reconstructed from byte patterns - having read `"cur_"` as the command it read `"ethe"` as the length - and
the error table says the same thing independently.

### What the refusal proves, which is more than it looks

`escan` was rejected on **semantics, not structure**. The firmware parsed the 106-byte frame, found a
well-formed BCDC header, recognised `SET_VAR`, read the iovar name, and objected only that the interface was
down. So the frame layout, the `DATA_OFF` fix, the SET flags and the 72-byte params block all passed
inspection by the one authority that matters. Phase 3's request layout is provisionally validated by the
thing that refused it.

### Changes

- **Firmware errors decode.** `err_name` names the codes quoted from the reference and prints the number for
  the rest rather than guessing. A refusal reading `-4` teaches nothing; `BCME_NOTUP` teaches the fix.
- **`set_cmd` sends a raw BCDC command**, with `set_iovar` as a caller. `BRCMF_C_UP` has no payload and no
  name, which the iovar-shaped function could not express. One frame builder, two entry points - copying it
  is exactly how `set_iovar` inherited the `dataoff` bug.
- **`interface_up` runs before the scan**, and its failure is reported.
- **A misleading hint is removed.** The scan's refusal message said the params VERSION was the first thing to
  change. That was written for the accepted-but-silent case and is wrong for a refusal, where the status code
  states the objection. The version hint now appears only where it applies.

### Prediction

1. **The interface comes up, `escan` is accepted, and events arrive** - then §30.6's rungs B and C decide
   whether results parse.
2. **`BRCMF_C_UP` is itself refused**, naming another prerequisite. brcmfmac does more during bring-up than
   this driver does, so a chain of these is plausible, and each one now names itself.
3. **`escan` accepted and no `ESCAN_RESULT` events** - the case the version hint was written for, and it now
   says so in the right place.

The event path remains completely unproven: no `ESCAN_RESULT` has ever arrived. Also unexplained, and worth
watching: the two frames on the event channel arrive with `frmlen 12` and zero body, which is a header-only
frame on a channel that should carry events.

## 35. Phase 3 delivered: the radio scanned the room

```
`escan` accepted - listening until the firmware says the scan is over
  event 69 (ESCAN_RESULT), status 8, 380 byte payload
  ... eleven more, 292 to 548 bytes each ...
  event 69 (ESCAN_RESULT), status 0, 12 byte payload
listening ended: complete (235 empty poll(s) of a 500 bound)
the scan window saw 12 event/data frame(s), 12 escan-result event(s), 10 glommed frame(s) ignored
10 network(s) from 12 escan-result event(s)
```

Ten networks, with names, BSSIDs, signal strength and channel. The operator's own network at -30 dBm on three
radios of one access point and a guest network beside it; two hidden networks; four neighbours between -63
and -80 dBm. Channels spanning 2.4 GHz channel 6 and 11 (`0x1006`, `0x100b`) and 5 GHz channel 42 at 80 MHz
(`0xe02a`) and channel 155 (`0xe09b`). Values that can only have come from the air, which is the whole test.

The identifiers themselves are in the operator's capture and not here: this repository is public, and the
neighbours did not agree to appear in it.

**And the wait ended on the firmware's word.** `listening ended: complete` - the `SUCCESS` event - with 235
empty polls of a 500 bound unused. Commandment VIII, as the fix rather than the rule.

### What §7 promised and what was delivered

> **3** - `wifi scan` lists the SSIDs in the room. First user-visible win, and it needs no cryptography and no
> credential.

Delivered on 2026-09-28, with one honest qualification: the DRIVER lists them, at boot, as its own self-test.
The shell cannot yet ask it to. `wifi` at the prompt still answers `unavailable`, and that IPC path - and the
picker the operator designed in §29's discussion - is the next work. So the deliverable is met as a
capability and not yet as a command.

### What it took, counted

From `escan` first being refused to networks printing: **nine boots**. What each one found, in order,
because the order is the lesson:

| boot | found | kind |
|---|---|---|
| 1 | `BCME_NOTUP` - the interface was down | protocol |
| 2 | `BRCMF_C_UP` reported success it never got: every command shared request id 2 | **mine** |
| 3 | an integer command carries four bytes, and UP is a chain, and events are off until asked for | protocol |
| 4 | the CLM regulatory blob - vendored, unembedded, and the actual cause of `NOTUP` | protocol (and mine) |
| 5 | the download flag carries a handler version I assembled away | **mine**, against my own rule |
| 6 | a byte-mode CMD53 carries at most 512 bytes; and 1400 IS the protocol limit | hardware (and mine) |
| 7 | the body READ was still byte mode - the fix landed on two writes and I counted instead of looking | **mine** |
| 8 | byte mode must round up to a word too - a regression in the previous fix | **mine** |
| 9 | events carry a 4-byte BDC header, and the event header is big-endian | protocol |
| 10 | the list never printed: a count of polls is not a duration | **mine**, and deferred twice |

Five of ten were the protocol. Five were mine - regressions, skipped verification, and one rule I had
written myself. The protocol half moved at one real step per boot, every step read from a reference. The
other half was the cost of not applying three habits that, when applied, worked every time: **read whole
functions in execution order**, **list what changed rather than counting it**, and **check every branch of
anything edited**.

### Two things learned that are worth more than the scan

**The radio needs regulatory data before it will do anything.** `BCME_NOTUP` was never about `UP`. The CLM
blob - which channels may be used at what power - was in the repository the whole time with a comment
explaining exactly how it is delivered and why it was not yet. A radio without channel rules cannot lawfully
transmit or scan, and "not up" is what that looks like from outside.

**Linux does not drive this controller with `sdhci`.** Booting Raspberry Pi OS on the same board showed
`mmc-bcm2835` bound to `fe300000` - the Pi Foundation's own driver for that block - which has a settling
delay after every register write, a data timeout, and a PIO loop that moves a whole block between checks.
Every host-side comparison before that boot was against the wrong driver. The operator's suggestion to boot
Linux and look was worth more than any hypothesis of mine that day.

### What is not done

- The shell cannot drive the driver. `wifi scan` as a command, the numbered picker, `wifi join <ssid>`.
- Secure-versus-open is not shown: it needs the RSN/WPA information elements parsed out of `ie_offset` /
  `ie_length`, which are in hand but unread.
- No association, no credential path, no data frames - phases 4 and 5.
- The glommed frames (10 of 22 this boot) are counted and dropped. The reference does the same and its
  scans work, so nothing waits on them; it is recorded rather than left implied.

### Second boot, 22:11 - it reproduces, and a smaller count is not a regression

```
listening ended: complete (236 empty poll(s) of a 500 bound)
the scan window saw 10 event/data frame(s), 10 escan-result event(s), 11 glommed frame(s) ignored
7 network(s) from 10 escan-result event(s)
```

Seven networks this time, not ten. **That is a different room, not a worse driver**, and it is worth writing
down because this project's own rule is that a number lower than last time is a truncation until proven a
difference. Here it is proven: two networks appeared that the first scan did not see at all (a BT hub and a
FRITZ box, at -78 and -83 dBm), and the three radios that reported -30 dBm in the first scan did not beacon
inside this window - the same access point appeared instead on two other BSSIDs at -80 on 5 GHz. Every
result was again a PARTIAL event followed by SUCCESS, and every wait ended on the firmware's word.

A scan is a sample of who happened to transmit while the radio listened on each channel. Two scans of the same
room differ; that is what makes the result real rather than replayed.

Both message fixes hold: stage 14 no longer announces itself unverified, and the closing line describes the
shell's state rather than contradicting the ten lines above it.

## 36. `wifi list` at the prompt - phase 3 as a command

```
gsh> wifi list
scanning  [q] quit
wifi-driver: sending `escan` - command 263, ... seq 8, request id 9
wifi-driver:   the firmware ACCEPTED `escan` (request id 9 matched, status 0)
wifi-driver: listening ended: complete (225 empty poll(s) of a 500 bound)
wifi-driver: the scan window saw 17 event/data frame(s), 17 escan-result event(s), 13 glommed frame(s) ignored
<ssid> -58 5GHz unknown
<ssid> -31 5GHz unknown
(hidden) -59 5GHz unknown
... eleven records ...
```

The shell asked, the radio swept, and one record per network printed in the spec's order. A second
`wifi list` twenty-seven seconds later ran on the same driver session - request id 10, sequence 9 - which is
the point of the `Session` outliving any one scan. Both ended on the firmware's `SUCCESS` event, in under
three seconds.

The identifiers are again in the operator's capture and not here.

### Why neither contract changed

The shell holds `ACQUIRE_ANY` and reaches the driver by name through the kernel directory, exactly as it
reaches `net-stack`; the driver replies through the cap embedded in each request, as it already did. So the
whole path is: one request byte, one fixed-layout reply, no new authority anywhere.

### The one boot it cost, and why

The first attempt said `not answering` in fifteen milliseconds. Not a lapsed deadline - the SDK reports a
send that could not leave that way, and the driver logged nothing. The shell is spawned before the driver,
so at spawn there was no cap to wire. `ns_abortable` handles exactly this for `net-stack` - on `Timeout`,
reacquire by name, ask once more - and the request half of that pattern was copied without the reacquire
half. The project's own notes had it written down already.

### Bring-up once, scan many

`scan::run` was split. `bring_up` sends the CLM blob, the event mask and the `UP` chain **once**, at boot -
re-sending `clmload` to an interface that is already up would be wrong - and `scan_once` does the part that
repeats. The boot self-test calls both; the serving loop calls only the second. A radio that never came up
answers every `wifi list` with `radio down` at once, which is the rule above the rules: a dependency that
cannot do the thing returns with a loud fact, never a hang.

### What phase 3 left open, in one place - and where each was closed

- `security` prints `unknown` until the RSN/WPA information elements are parsed. *Closed: `scan::sec`
  reads the Privacy bit, the RSN element and the WPA vendor element (section 38).*
- Rule 11: `q` does not yet stop the radio's sweep, because the driver cannot hear an abort while it is
  inside `collect`. It needs to poll its endpoint between frames. *Closed: the sweep is a state the serve
  loop steps one frame at a time (`utilities/56_wifi.md` 4e).*
- Glommed frames (channel 3) up to 3328 bytes now arrive and are dropped, as the reference drops them. The
  log used to call them "an impossible shape"; it now says what they are. *Closed: section 39 reads them.*
- The numbered picker the operator designed becomes live with `wifi join`, which is phase 4. *Closed:
  section 38.*

## 37. The firmware has no supplicant, and it said so three ways (2026-09-29)

Phase 4's join sequence went out on hardware on 2026-09-28 and 2026-09-29. `wpa_auth WPA2-PSK`, `auth open`
and `wsec AES` were accepted. `sup_wpa` - the switch that hands the WPA2 4-way handshake to the firmware -
was refused **-23 BCME_UNSUPPORTED** both times: as a plain iovar, and as `bsscfg:sup_wpa` with the 23-byte
payload `brcmf_create_bsscfg` builds (`bsscfg:sup_wpa\0`, index 0, value 1). Two refusals of two forms said
the FORM was not the question.

### What the references say

- **Linux `feature.c`**: `brcmf_feat_iovar_int_get(ifp, BRCMF_FEAT_FWSUP, "sup_wpa")`. The firmware has an
  internal supplicant iff a GET of `sup_wpa` does not return `BCME_UNSUPPORTED`. Asked at
  `brcmf_feat_attach`: after the preinit commands, before `UP`. Nothing in the Raspberry Pi kernel's copy
  changes this for 4345/43455.
- **OpenBSD `bwfm.c`**: sets `sup_wpa 0` on purpose - the comment reads "the firmware supplicant can handle
  the WPA handshake for us, but we honestly want to do this ourselves" - and net80211 runs the handshake.
  EAPOL frames (ethertype 0x888e) come up `bwfm_rx` and go to `ieee80211_eapol_key_input`; keys go down
  through the `wsec_key` iovar (`struct bwfm_wsec_key`: `ea`, `index`, `len`, `data`, `algo`, `flags`).
- **Pi OS on this chip family**: users of firmware 7.45.241 (`firmware-nonfree` issue 34) and of the May
  2026 package (issue 58) hit locally-generated `ASSOC-REJECT status_code=16` and cured it with
  `feature_disable=0x2000` - which is the FWSUP bit. So on 7.45.241 Linux had detected the feature and was
  using it.

### What the board said, asked Linux's way

`ctrl::report_firmware` asks three GETs at the point `brcmf_feat_attach` asks, before `UP`. The answers:

```
ver: wl0: Aug 29 2023 01:47:08 version 7.45.265 (28bca26 CY) FWID 01-b677b91b
cap: ap sta wme 802.11d 802.11h rm cqa cac dualband ampdu ampdu_tx ampdu_rx amsdurx radio_pwrsave btamp
     p2p proptxstatus mchan p2po anqpo vht-prop-rates dfrts txpwrcache stbc-tx stbc-rx-1ss epno pfnx wnm
     bsstrans mfp sae_ext fbt
sup_wpa: REFUSED -23 BCME_UNSUPPORTED
```

Three facts, each checked against something outside this project:

1. **The version string is byte-for-byte the one Pi OS prints for this firmware file** - `7.45.265
   (28bca26 CY) FWID 01-b677b91b` appears in Pi 4 owners' `dmesg` on the web. So the code running on the
   radio is the file `nonfree/brcm43455/PROVENANCE` says it is, confirmed from the far side of the upload.
2. **The capability string has `sae_ext` and lacks `idauth` and `sae `.** In `brcmf_fwcap_map`, `sae_ext`
   means WPA3 authentication is done by the HOST (that is what the Raspberry Pi kernel's `sae_ext` support
   was added for), and `idauth` is the firmware-side authenticator. This firmware announces that it has
   moved authentication to the host, which is the direction the third fact confirms for WPA2.
3. **`sup_wpa` GET is refused.** By the rule Linux itself decides on, this build has no internal
   supplicant. On Pi OS with this exact firmware, the 4-way handshake is wpa_supplicant's, in userspace.

### What this means for phase 4

The passphrase-to-firmware join this project built (`join.rs`, after `cyw43-driver`) cannot work on this
firmware, and not because of anything in the driver. The 4-way handshake has to be run by the host - as
Linux does with wpa_supplicant and OpenBSD does in net80211, both on this same chip - and the keys installed
with `wsec_key`. That is: EAPOL frames on the data channel in both directions, PBKDF2-SHA1 for the PMK,
the PRF for the PTK, HMAC-SHA1 for the MIC, AES key-unwrap for the GTK, and the four-message state machine.
The data path is needed for phase 5 regardless; the crypto is new.

The alternative that exists - the older `7.45.241` firmware, which by issue 34's evidence DOES answer
`sup_wpa` - is recorded here rather than chosen: it is the firmware Raspberry Pi moved away from, and the
bug that made its users turn the feature off is in exactly the feature this project would be leaning on.
Which road to take is the operator's decision, and it is open as of this section.

### The instrument that was not one

Before asking the board, `strings` was run over the firmware image for `sup_wpa`. Zero hits. As a control,
`escan`, `wsec` and `wpa_auth` - iovars this firmware demonstrably answers - were also zero, so the iovar
names are not stored as plain text and the zero for `sup_wpa` earned nothing. Discarded, and recorded so it
is not run again as evidence.

## 38. One boot, the whole surface: scan, list, status, info, debug, a key kept and reused (2026-09-29)

Between §37 and this boot the driver stopped being a sequence of calls and became a state machine, and the
shell grew every verb `utilities/56_wifi.md` describes except the WPA2 join. The boot at 13:05 ran all of it
at the prompt for the first time and it behaved as the spec says, first try, with one finding.

**What ran.** `stage 0`: SHA-1, HMAC-SHA1, PBKDF2 and the IEEE PSK each matched their published vector.
`wifi scan`: the sweep as a state the serve loop advances one frame at a time; four networks in three
seconds under the header the operator laid out, then the picker; a number joined through the same path as
`wifi join`. The passphrase was asked once, the pairwise master key was derived into slot 0 of the
64-slot table, and a second `wifi join` of the same name asked nothing. `wifi list` showed `saved` in
NOTE. `wifi status` and `wifi info` read the link live (not associated, correctly - see below). `wifi debug`
printed the counters; `wifi debug firmware` asked the firmware its version and words again and got the same
answer as §37; `wifi debug trace` printed the ring.

**The trace is the thing.** Sixty-four frames, timestamped by the driver's own clock in real milliseconds
(the kernel gave it a rate), showing the boot scan, the prompt's scan, the join's six control exchanges each
answered within 4 ms, then the handshake: `RX DATA len=131` at 27.568, 27.626, 28.605, 29.615, 30.615, 31.615
- the access point's message 1, sent, then retried at one-second intervals - and `RX EVENT event=6` (the
deauthentication, reason 15) at 32.615. Every line of that used to be read out of the serial log by hand.
The per-frame and per-command log lines are removed in the same change; a refusal or a silence is still
logged, loudly, and everything else is in the ring.

**The finding: glommed frames carry events, and this driver drops them.** The trace shows channel-3 frames
constantly - 30 in 74 s, always in pairs: a short descriptor (16 to 22 bytes) then a long body (256 to
1952 bytes). §31 recorded that the reference drops glommed frames and so does this driver, and the scan
works without them. What this boot showed is what is INSIDE them: no `LINK` or `ASSOC` event was ever
delivered on the plain event channel, on either join, although both associations plainly happened - the
handshake frames arrived. The reference drops glommed frames only on the control path; its data path reads
them (`brcmf_sdio_rxglom`, which walks the descriptor's list of sub-frame lengths and delivers each).
Reading them is the prerequisite for the handshake, because message 3 - which carries the encrypted group
key and must be answered - may arrive glommed exactly as the association events did.

**The sequence at the access point, for the record.** `AUTH` twice (the first with status 2, a timeout,
the second status 0), then message 1 six times with the replay counter climbing 1 to 6 and the same
ANonce, then `DEAUTH_IND` reason 15 - `4-Way Handshake timeout`, the reason code the standard assigns to
exactly this. The access point did what a station that never answers deserves. The next slice answers it.

## 39. Reading the superframes: the sub-frames are padded, and the descriptor is the map (2026-09-29)

The first walker assumed what a summary of `brcmf_sdio_rxglom` said - sub-frames back to back, each
starting where the previous one's length ends - and the boot at 13:57 refuted it precisely: sub-frame 0 of
every superframe parsed (an `ASSOC` event appeared for the first time in this port's life), and sub-frame 1
failed to validate at +229, +92, +652, +392 - offsets that are the first sub-frame's own `frmlen`, and where
the bytes were padding, not a header (`len 1976, cksum 0x0000`; `len 0, cksum 0xb800`).

The references never made that assumption; the summary did. OpenBSD's `bwfm_sdio_rx_glom` reads ONE CHUNK
PER DESCRIPTOR ENTRY and then parses each chunk's header, using the header's length for the payload and
letting the rest of the chunk be padding. Linux reads the whole superframe in one transfer and checks each
sub-frame against its descriptor entry. Either way the descriptor - the short channel-3 frame with bit 0x80
that precedes every superframe, whose payload is a list of little-endian u16 lengths starting right after
its 12-byte header - is the map, and the sum of its entries is the superframe's `frmlen`, header included.

So `Session` keeps the last descriptor, `subframes` walks by its chunks (chunk 0 holds the superframe's own
12-byte header, so its sub-frame begins 12 bytes in), checks that the entries sum to the frame, and names any
chunk whose header does not validate rather than stopping the walk. A superframe with no descriptor before
it, or one whose entries do not add up, is not read and says so - the boundaries would be guesses.

Recorded as a method note too: a reference's CODE was right and a summary of it was wrong, and the first
walker was built on the summary. §26.14 says read the mechanism; that means the function, not a paraphrase.

**Verified at 15:58 the same day, exactly as predicted.** On `wifi join`:

```
join event 3 (AUTH), status 0
join event 7 (ASSOC), status 0
join event 16 (LINK), status 0, reason 0, flags 0x0001
ASSOCIATED - the link is up at the 802.11 layer; the handshake is now the access point's move
```

- the first `LINK` and the first `ASSOCIATED` this port has ever printed. `wifi debug trace` showed where
they had been all along:

```
45.788  RX GDESC         lists 2 sub-frame length(s)
45.792  RX GLOM          len=384
45.792  RX EVENT*        event=7 status=0 len=215
45.792  RX EVENT*        event=16 status=0 len=104
45.855  RX DATA          len=131
45.905  RX GDESC         lists 2 sub-frame length(s)
45.909  RX GLOM          len=256
45.909  RX EVENT*        event=1 status=0 len=78
45.909  RX EVENT*        event=0 status=0 len=92
```

`ASSOC` and `LINK` in one superframe, `JOIN` and `SET_SSID` in the next, the handshake's message 1 (the
131-byte data frame) between them. The scan's results ride the same way - four or five `ESCAN_RESULT`
events to a 1792- or 2368-byte superframe - and `rx_glom_sub` read 40 against 24 superframes. No sub-frame
failed to validate. The entries of every descriptor summed to its superframe, or the walk would have said
so. The next thing the driver must do is SEND on the data channel, which it has never done.

## 40. The handshake, built (2026-09-29, evening) - the first frames this driver has ever sent

Everything above association was reference-read before a line was written, and each piece is quoted at the
point it is used:

- **Transmit** (`ctrl::send_data`), from `bwfm_sdio_tx_dataframe`: hardware header (length and its
  complement), software header (sequence, channel DATA, `dataoff` 12), a 4-byte BCDC header with protocol
  version 2 in the flags' high nibble, then the ethernet frame; rounded to 4 or to a 512-byte block as
  every control frame already was. Sent only with CREDIT: every received frame's `swhdr->maxseqnr` is the
  highest sequence the firmware will take next (`bwfm_sdio_rx_frames` stores it from every frame, header-
  only ones included), and `bwfm_sdio_tx_ok` says a frame may go while `(max - seq)` is non-zero and below
  0x80. `Session` keeps it and refuses loudly without it.
- **The key derivation** (`eapol::derive_ptk`), from `ieee80211_derive_ptk`: `PRF-384(PMK, "Pairwise key
  expansion", Min(AA,SPA) || Max(AA,SPA) || Min(ANonce,SNonce) || Max(ANonce,SNonce))`, the label with its
  NUL included as `ieee80211_prf` is called with it (23 bytes). The 48 bytes cut into KCK, KEK, TK.
- **Message 2** (`eapol::build_key_frame`), from `ieee80211_send_4way_msg2` and `ieee80211_send_eapol_key`:
  `PAIRWISE | KEYMIC | version 2`, the access point's replay counter, the SNonce, the RSN element from the
  association request as key data, `key->len` = body after the 4-byte 802.1X header, `paylen` = key data;
  the MIC is HMAC-SHA1 over the body from `version` to the end with the MIC field zeroed, first 16 bytes
  (`ieee80211_eapol_key_mic`, `EAPOL_KEY_DESC_V2`).
- **Message 3**, from `ieee80211_recv_4way_msg3`: the ANonce must equal message 1's; the MIC must verify
  under the KCK (`ieee80211_eapol_key_check_mic`); the key data must be `ENCRYPTED` and unwraps under the
  KEK (`ieee80211_eapol_key_decrypt`, `aes_key_unwrap`, 8 bytes shorter than it arrived); inside, the GTK
  KDE - `0xdd`, OUI `00:0f:ac`, type 1, `key id | tx`, reserved, key. **Message 4** is `PAIRWISE | KEYMIC
  | SECURE`, empty, MIC'd (`ieee80211_send_4way_msg4`).
- **Install** (`ctrl::install_key`), from `bwfm_set_key_cb`: `struct bwfm_wsec_key`, **164** bytes as
  `bwfmreg.h` lays it out - the pairwise key at index 0 with the access point's address in `ea`, the group
  key at its key id with `PRIMARY_KEY` and no address - through the `wsec_key` iovar, then `wsec`
  re-asserted with AES. *This said 162 when first written, and the driver sent 162: the fields sum to 162,
  the struct is not packed, and `sizeof` rounds to the 4-byte alignment. See "The first run" below.*

### The first run (2026-09-29, 19:44): the handshake completed and the key was refused by two bytes

Three joins with the right passphrase, and the log read the same each time:

```
EAPOL-Key ... message 1 of 4 (ANonce) ... replay 1
message 2 of 4 sent (135 bytes, replay 1) - our nonce and the RSN element, signed
EAPOL-Key ... message 1 of 4 (ANonce) ... replay 2
message 2 of 4 sent (135 bytes, replay 2)
EAPOL-Key ... message 3 of 4 (GTK, install) ... info 0x13ca [pairwise ack mic install secure encrypted] key_data 64 bytes
message 3 verified (MIC, ANonce, 56 bytes of key data unwrapped); message 4 sent
setting `wsec_key` - 162 byte value
the firmware REFUSED `wsec_key` - BCME_BUFTOOSHORT (status -14)
```

**Message 3 verifying is the fact that decides everything above it.** Its MIC is computed by the access
point with the KCK it derived from the PMK; ours matched, so the PMK, the PRF, the nonce order, the label
and the bytes the MIC covers are all right, and the passphrase was right too. The key data unwrapped under
the KEK, so that half of the PTK is right as well. The prediction in the commit named the MIC path as the
first suspect if message 1 repeated - it did repeat once (replay 1, then 2), and then message 3 came, so
the repeat was the access point's ordinary retry, not a refusal. The operator had doubted the passphrase
and rechecked it several times; the log says it was never in question.

**The refusal.** `BCME_BUFTOOSHORT` from an iovar set means the value is shorter than the struct the
firmware expects. `struct bwfm_wsec_key` was sized here by adding up its members: 4+4+32+72+4+4+12+4+4+8+8+6
= 162. That is the size of the fields. `sizeof` is 164, because the struct holds `uint32_t` members, is not
packed, and C pads the tail to the alignment; `bwfm_fwvar_var_set_data(sc, "wsec_key", &key, sizeof(key))`
therefore sends 164 and the firmware checks for 164. The fields were counted and the padding was not -
which is the one thing a hand count of a C struct cannot see, and the argument for reading `sizeof` off a
compiler rather than a page. Confirmed against `bwfmreg.h` (no `__packed`) before the constant changed.

**Two more things the run showed, both fixed in the same change.** After the refusal the firmware was still
associated with no keys, so `wifi status` read the link live and reported `joined 5 min ago` to a
`(hidden)` network with `security open` - the driver's memory said not joined and the firmware said joined,
and status believed the firmware. A join that fails after association now disassociates, so the two agree.
And `signal excellent 0 dBm`: the firmware refused `GET_RSSI` (`BCME_BADARG`) and the shell turned the 0 it
was handed into the best word it has. A zero RSSI is not a reading and now prints `unknown`.

The trace also showed the two transmitted frames as `RX other`, because the shell's name table stopped at
kind 9. They are `TX DATA` now.
- **Two primitives** joined `crypto.rs` with their published vectors in the boot self-test: AES-128 both
  directions (FIPS 197 C.1 - the S-box is computed from the field inverse and the affine transform rather
  than typed, because 256 hand-copied bytes are 256 places to be wrong) and AES Key Unwrap (RFC 3394
  §4.1). The PRF has no vector here: it is HMAC-SHA1 concatenated with a counter, and a wrong construction
  would show as a rejected message 2 - which is why, if the RIGHT passphrase ever prints "incorrect
  passphrase", the PRF and MIC path is the first suspect, and this sentence says so.

**How "incorrect passphrase" is decided.** The access point never says it. It receives message 2, cannot
verify a MIC made with the wrong PMK, repeats message 1, and eventually deauthenticates with reason 15.
The driver therefore decides it: message 1 arriving a third time after two answers, or a deauthentication
after any answer. Nothing else produces that pattern.

**The one weakness, recorded.** The SNonce should come from a hardware RNG and the aarch64 kernel's
`hw_random` is a stub. Until it is not, the nonce is SHA-1 over the cycle counter, the monotonic clock, the
access point's nonce and our address, and the driver logs that sentence on every handshake. The Pi 4 has an
RNG (`iproc-rng200`, five registers, read from Linux's driver); wiring it into `arch/aarch64` is the next
kernel change and is small. *Done 2026-09-30: `arch/aarch64::hw_random` reads the RNG200 behind query 19,
and the driver's fallback and its log line remain for the day the block answers nothing.*

Not yet run on hardware. What the first boot must show is in the spec's status section. *(It was run
that evening and refused the key by two bytes - "The first run", above - and joined the next morning;
section 41 opens with it.)*

## 41. Phase 5, built: the frame path, and the one rule `net-stack` needed after all (2026-09-29)

**The join works.** Boot 20:01: `wifi join` with the right passphrase printed `joined <the network>` on
the first try, the log showed `setting wsec_key - 164 byte value` twice with no refusal and `JOINED -
handshake complete, pairwise key installed, group key 2 installed`, and two more `wifi join`s answered
`already joined` with nothing sent. The 162 was the whole of the previous boot's failure (section 40).

**Then the operator pulled the cable and expected `ping` to follow the radio, and it did not**, because
nothing above the join existed yet - phase 5 in section 7's table. This section is that phase.

### What was built, in the shape section 2 said

- **`wifi-driver` serves the frame interface** (`frames.rs`): `0x10` INFO answers the chip's address and
  the driver's own word on the join; `0x11` TX is `ctrl::send_data`, the path the handshake proved,
  refused rather than queued when there is no link; `0x12` RX hands up one ethernet frame from a bounded
  queue of eight, filled by a PULL that reads what the chip has waiting - at most eight frames, and only
  when the queue is empty and the radio has a link. The driver still blocks in `recv` when idle: a frame
  the access point sends waits in the chip until `nic-driver` asks, and `nic-driver` asks on
  `net-stack`'s pace. The pull watches the link as it reads: a `LINK` event without its up bit or a
  deauthentication forgets the join THEN, which closes the spec's "an access point that drops the station
  is noticed only at the next `wifi status`" item while the stack is polling.
- **`nic-driver` on the Pi 4 has a second backend**, reached the way the Pi 2's reaches `dwc2`: one
  bounded request, one reacquire-and-retry, every reply checked against the op it answers (the radio's
  endpoint also serves `wifi`, so a late reply must not be read as the next answer). Which backend is
  `Carrier`: the cable, re-read at most every 500 ms on whatever request arrives, or the radio.
- **The supervisor** wires `nic-driver` to `wifi-driver` where the image is embedded (`NIC_PEERS` on the
  `has_wifi_driver` board fact, not the ISA), and spawns the radio BEFORE `nic-driver` so the peer is in
  the name-cap map when it wires. (Now keyed on the `nic_radio_bridge` board fact. It was split off
`has_wifi_driver` while the VisionFive had no bridge to the radio, and since V6 it is derived from the radio
fact again, `services/supervisor/build.rs`.) The contract, the authority pin and the send-peer list all say the
  same thing, and `contract_check` and Commandment VII hold them to it.
- **`net` names the carrier**: `link  up via the cable`, `up via wifi (the cable is out)`, or `down`.

### The rule `net-stack` needed, and why section 2 did not see it

Section 2 said `net-stack` needs no change, and for the FRAME protocol that held: it sends and drains
the same bytes to the same name. What it did not see is that **a link which changes its address is a
different link**. A cable pulled and put back is the same link, and `net-stack` deliberately resumes it
without re-configuring. A cable pulled while the radio is joined swaps the frames' source address for the
radio's, and the lease, the gateway's ARP entry and our own source MAC all belong to the old one - a
frame sent with the cable's address through the radio is a frame from a station the access point never
associated. So a configured stack now re-reads the link's address every two seconds on a network-using
request (op 3 already carries it) and a changed address re-runs the dance - the same self-configure a
fresh cable gets, for the same reason. Fifteen lines, one new helper, and it is recorded in section 2
rather than left as a silent amendment to "unmodified".

The alternative was to give the radio the cable's address (`cur_etheraddr` is settable) so the stack saw
one link. It was not taken: the cable's address on this board is a made-up one (`backlog/21`), the radio's
is burned in, and a station that borrows another interface's address to avoid telling its stack the truth
is the silent substitution 26.4 names.

### What is NOT done, recorded

- **Group-key rekey is not answered** (`backlog/64`). The access point will drop the link at its rekey
  interval; the driver sees it, says so, and `wifi join` brings it back. The log line will say what this
  router's interval is. *Answered later the same day - section 42, which now answers a pairwise rekey too
(`frames::pairwise_rekey`); what remains is seeing either on hardware.*
- Data frames that arrive DURING A SWEEP are still dropped by the sweep's own reader; RX answers zero
  frames while a sweep runs. A sweep is a moment of no link either way.
- `GET_RSSI` is refused (`BCME_BADARG`) even when joined, so `wifi status` says `signal unknown`. Honest,
  not blocking; the Linux driver's form of the query is the next thing to read. *Read, 2026-09-30: Linux
  sends the same zeroed `scb_val` - but `sizeof` it, which is twelve bytes, not the ten its fields add up
  to (`int32` and a six-byte address, 4-aligned). The same padding lesson as `wsec_key` (section 40), one
  struct later; the driver sends twelve now.*

### The first boot (2026-09-30, 08:16): the frame path WORKED, then went deaf after fifty requests

The machine booted with the cable out. `wifi join` joined. On the next network request the stack saw the
radio's link and configured itself OVER THE RADIO, on the guest network's own subnet:

```
nic-driver: the cable is out - the radio carries the link (MAC xx:xx:xx:xx:xx:54)
net-stack: DHCP reply - 320 bytes, type 2 (2=OFFER 5=ACK), server 192.168.11.1
net-stack: DHCP - offered 192.168.11.20, gw 192.168.11.1, dns 194.168.4.100
net-stack: DHCP - ACK, 192.168.11.20 is ours (server 192.168.11.1)
net-stack: ARP - 192.168.11.1 is at <the access point>
net-stack: ICMP - 192.168.11.1 echo reply (ping OK)
```

Discover, offer, request, acknowledge, ARP and an echo through the radio, on the first attempt: every op
of the frame interface, the transmit credit, the pull, the address-change rule and the DHCP exchange all
proved at once. Plugging the cable back in later did the other half - `the link's address changed
(xx:xx:xx:xx:xx:54 -> 02:00:00:00:00:01) - re-configuring`, a new lease from 192.168.4.1, and `ping`
answering over the cable within a second.

**Then the radio stopped answering, and `ping` over it never happened.** The operator's words: "wifi
radio didn't respond after a while. All successful pings is when the ethernet cable was connected." The
log showed every exchange between `nic-driver` and the radio timing out at exactly its one-second bound,
`net-stack`'s serve passes taking exactly 1000 ms, and `observe now` showed the shape of it:

```
10   wifi-driver      C3   BlockRecv   ...   0/16
11   nic-driver       C1   BlockRecv   ...  16/16!
12   net-stack        C1   BlockSend   ...   0/16
```

`nic-driver` blocked on a reply with sixteen stale requests behind it; the radio idle with nothing
queued. The radio was ANSWERING NOTHING, and `observe` says why it looked idle: it had nothing to answer
on. **`wifi-driver` never released a reply cap.** Every request carries a one-shot cap the caller derives
for its answer; the kernel installs it in the receiver's table, and the receiver must `remove_cap` it
after use - `nic-driver`, `dwc2`, `net-stack` all do, and this driver did not, anywhere. A task holds
sixty-four. `wifi` commands alone never reached that in a boot; `net-stack`'s drains reach it in seconds.
From the fifty-somethingth request on, the kernel had no slot to install a cap into, the request arrived
without one, and the serve loop's `None => continue` dropped it - silently, since that arm said nothing.

**Fixed:** the cap is reclaimed after every answer (and in the no-radio server), a request with no cap is
counted and logged, an answer that cannot be delivered is counted and logged. Three lines were the fault;
the rest is so that the next such fault says its name.

**Two things the failure's SHAPE taught, both acted on.**

- **`nic-driver`'s bound on the radio was a second, and a second is a livelock.** `net-stack` gives up
  well inside it and sends its next request, which queues behind the one still waiting; sixteen of those
  and `nic-driver`'s inbox is full, and now the radio's answer cannot land in it either - so every
  exchange times out whether or not the radio is fine. The bound is 100 ms now (`RADIO_MS`), well over
  the millisecond a real answer takes and short enough that `nic-driver` keeps pace with its callers
  while the radio is busy (a join holds it for seconds; those requests fail fast instead of piling up).
  With a millisecond bound the cap cannot be told stale from silent, so three silences in a row reacquire
  it by name - harmless when it was not stale, the only recovery when it was. The round trip is now
  measured and logged from the side that pays it.
- **The kernel drops a message for a blocked receiver whose queue is full, and says `Ok`.** That is the
  branch the shape above went through, it is a silent fallback at the kernel boundary, and it is
  `backlog/65` - one branch to fix, held for the next kernel change rather than folded into this flash.

Also seen and fixed: `net` printed the stack's never-had-a-link sentinel as addresses
(`ip 108.105.110.107` is the word "link"); and three places said `net status`, which is not a command.

### Prediction for the boot

Cable in, `wifi join <ssid>`: `joined <ssid>`, and `net` shows `link  up via the cable` - nothing else
changes. Pull the cable: within ~1 s `nic-driver: the cable is out - the radio carries the link (MAC ..)`,
then on the next `ping` or `net` the stack logs `the link's address changed (.. -> ..) - re-configuring`,
runs DHCP over the radio (`DHCP - offered ...`), and `ping 8.8.8.8` answers. `net` shows `up via wifi`.
Plug the cable back: `the cable carries the link; the radio stands by`, another address change, another
dance, and `ping` answers over the cable. If the address change is logged but DHCP gets no offer over the
radio, the first suspect is TX credit (`wifi debug stats`, `tx_no_credit`) and the second is the source
address the frames carry.

## 42. The group-key rekey, answered (2026-09-30)

Phase 5's first boot (section 41) proved the link and left one thing that would take it down again on a
timer: a WPA2 access point replaces its group temporal key periodically - an hour on many routers - and
does it with a two-message handshake the station must answer, or be deauthenticated. The driver counted
those frames and said what would happen. Now it answers them.

**What the join keeps.** `join::Keys`: the KCK and KEK halves of the pairwise transient key, the last
replay counter the access point used, and our address. Not the temporal key - that lives in the firmware
from the moment `wsec_key` installs it and is never needed again by the host. Zeroed on `leave`, on
`radio off`, on a dropped link, and at the start of the next join (`join::forget`). Seventy-eight bytes,
on the serve loop's stack, for the life of one association - forty as first written, before the PMK joined
them for the pairwise rekey.

**What the pull does with an EAPOL-Key frame** (`frames::group_rekey`, each step from OpenBSD's
`ieee80211_recv_rsn_group_msg1`, quoted at the function):

1. Pairwise bit set: not a group rekey but a new four-way handshake. `group_rekey` returns
   `Rekey::Pairwise` and the pull answers it with `frames::pairwise_rekey`, deriving a new PTK from the
   kept PMK. (As first written this case was only counted and said once; the code has since closed it.
   Neither rekey has yet been seen on hardware, `backlog/64`.)
2. `KEYMIC` and `KEYACK` both set, or it is not message 1 and there is nothing to answer.
3. The replay counter must exceed the last accepted (`ni_replaycnt`); at or below is a replay, ignored.
4. The MIC must verify under our KCK.
5. The key data must be `ENCRYPTED` and must unwrap under our KEK (RFC 3394, the same unwrap as message 3
   of the four-way).
6. The GTK KDE is found inside (`eapol::find_gtk`, the same as message 3), 16 bytes for CCMP.
7. **Install first, then acknowledge.** An acknowledgement for a key the firmware had refused would tell
   the access point to start using a key this station does not hold.
8. The acknowledgement is `ieee80211_send_group_msg2`'s frame: `KEYMIC | SECURE`, the access point's
   replay counter copied back, an empty key data field, signed under the KCK. The replay counter is then
   remembered.

Every refusal logs its reason. The frame is copied out of the read buffer and handled after the walk of
that read, once per pull, because answering needs the session that the walk is borrowing; an access
point retries, so a second key frame in one read is not lost by being left for the next pull.

**The pairwise rekey too (same evening).** An access point may also restart the whole four-way handshake on
a live link. The join's handshake was lifted into `join::Handshake` - one struct, fed one key frame at a
time, answering `Continue`, `Joined(keys)`, `PassphraseRefused` or `Failed` - and `join` now drives it from
its own frame loop while `frames::pairwise_rekey` drives it from the pull: message 1 arrives, a new PTK is
derived from the PMK the association was made with (`Keys` carries it now), messages 2 and 4 go out,
message 3 is verified, both keys are installed, and the keys are replaced in place. Data frames that arrive
during the exchange are queued as any pull would queue them; a wait of two seconds bounds it. One state
machine, two callers, the same words in the log.

**Not yet run on hardware**, either of them, because it cannot be made to happen: the access point decides
when to rekey. What the log will show, when it does:

```
wifi-driver: group key N re-installed and acknowledged (replay R) - the access point rekeyed
```

and `ping` continuing past it. If instead the link drops at the router's interval with a refusal logged
just before, the refusal names the step.

## 43. The keys on disk: `/wifi.keys`, and the radio ready at boot (2026-09-30)

Section 6 designed a keyring service and was superseded by the operator's decision to keep keys in the
driver's memory and lose them with it. This is the slice that decision pointed at, and it was asked for
in these words: *"save passphrase on filesystem so that when the machine starts up, if there's wifi, it
auto joins with that passphrase and ready to go."* With the exposure named by the operator before it was
built: *"if I were to unplug the usbstick and put it on another machine, that machine will have access to
wifi passphrase (I'm ok with that for now)."*

**What is saved is the derived key, not the passphrase.** The driver has never kept the passphrase text
past the moment it becomes a key; the file holds what the driver holds. A key joins the network exactly
as the passphrase would, so the card's holder can join it - that is the accepted exposure - but it does
not give up the passphrase itself, which people reuse for other things, and that difference costs
nothing. Names are in plain text. Nothing is encrypted at rest, because there is no per-machine secret
to encrypt with; a file that looked encrypted and was not would be the silent substitution 26.4 names.

**The shape section 6's superseding note fixed in advance, built as fixed.** The in-memory table stays
the working set. `keyfile.rs` (now `sdk/wifi/src/keyfile.rs`, section 59) loads `/wifi.keys` once the radio is up - through `fs`, one of the driver's two
send peers (the other is `power`, section 57), each request bounded and matched to its reply, reacquired by name if `fs` restarts - and
writes it after every change: a join that added or re-ordered a key, a `forget`. While `fs` is still
mounting the load is retried between requests, fifteen times two seconds apart, and then given up with
a line; the driver runs on the table alone, exactly as it did before the file existed. The file is at
most 48 entries, most recently used first, in one `fs` write; the table's 64 is the larger bound and the
sixteen least recent are simply not saved. Open networks hold no key and are not saved.

**At boot the radio joins what it last joined.** The first entry is the most recent, and once the file
is read the driver joins it without being asked - the same code path as the rejoin after `radio on`
(`join_known`), so a refusal or an out-of-range network ends in the same words. The cable still wins for
the link; a machine that boots with its cable in is joined and standing by.

**What the earlier decision still governs.** *"If anything restarts that driver, the user will put in
their creds again. Better that than the kernel crashing"* was about the CRASH case, and it still is:
nothing above the kernel must survive, and a respawned driver carries nothing across its own death. It
reads the file back instead, which is the difference between surviving and recovering.

**Format** (`keyfile.rs`): `"GSWK"`, a version byte, a count byte, then `[len, ssid[32], security, pmk[32]]`
per entry. A file of another version is ignored with a line and rewritten by the next join.

**Prediction for the boot.** Boot with the cable out: the log shows `/wifi.keys loaded - 1 network(s)
known` - or `no /wifi.keys` the first time, then `wifi join`, then `/wifi.keys written - 1 network(s)` -
and on the boot after that, `joining the network last joined, from /wifi.keys`, `JOINED`, and `ping`
answering with nothing typed. `wifi forget <name>` rewrites the file with the name gone, and the next boot
does not join.

## 44. The VisionFive 2 Lite has a radio, and its own boot log says which (2026-09-30)

Section 1 refused to assert this from a spec sheet and asked for the board to settle it. The board did,
in the vendor Linux boot log captured on 2026-09-07 while bringing up the RISC-V port (`build/`, not in
the repository). Read from that log, in order:

- U-Boot: `WIFI/BT support: 1`.
- The second DesignWare MMC host, `dwmmc_starfive 16020000.mmc` (`mmc1`), reports `card is non-removable`
  and then `mmc1: new high speed SDIO card at address 390b` at 49.5 MHz. The first host, `16010000.mmc`
  (`mmc0`), is the SD card the board boots from. Same shape as the Pi 4: the radio is a soldered SDIO
  function on its own host, and the boot medium is on the other.
- The vendor driver identifies it: `aicbsp_sdio_probe:1 vid:0xC8A1 did:0x0082`, `:2 vid:0xC8A1
  did:0x0182` (two SDIO functions, WiFi and Bluetooth), `aicwf_sdio_chipmatch USE AIC8800D80`, `chip rev: 7`.
  The SDIO clock is first set to 5 MHz for the firmware load.
- The firmware is loaded FROM THE HOST, in pieces, from `/lib/firmware/aic8800_sdio/`: a patch table
  (`fw_patch_table_8800d80_u02.bin`), an ADID blob (`fw_adid_8800d80_u02.bin`), a patch and an extension
  patch (`fw_patch_8800d80_u02.bin`, `_ext0.bin`), a Bluetooth patch table, and then the WiFi firmware
  proper, `fmacfw_8800d80_u02.bin` - "fmac", full-MAC. Then `wlan0` and `p2p-dev-wlan0` appear.

**What this means for a port, said before anyone plans one.** It is the Pi 4's SHAPE - an SDIO
full-MAC radio whose firmware the host uploads - with none of the Pi 4's PARTS:

- **The SDIO host is a DesignWare MMC block, not an Arasan/SDHCI.** `arch/riscv64` would need a
  `dw_mmc` host layer; nothing in `arch/aarch64/sdio.rs` carries over except the shape of CMD52/CMD53.
- **The chip is AICSemi's, not Broadcom's.** Everything in `services/wifi-driver` from the backplane up
  - the CR4 upload, SDPCM, BCDC, the `escan` protocol, the event codes, the `wsec_key` structure - is the
  Broadcom firmware's language and does not apply. The AIC8800's control protocol is whatever its vendor
  driver speaks; that driver (an out-of-tree Linux module, `aic8800_sdio` / `rwnx`) would be the
  executable datasheet (26.14), and it is large.
- **The firmware is proprietary vendor blobs**, four of them for WiFi alone, and the section 8 question
  - in the repository or supplied by the user - is asked again for a different vendor with a different
  licence. (Answered 2026-10-04: in the repository, `nonfree/aic8800d80/`, on the operator's decision and
  as a recorded exception - AICSemi publishes no licence, `docs/licensing.md` 5a says what that means, and
  the copies are the exact bytes the board's own vendor image loaded.)
- (2026-10-02: this is what `sdk/wifi` became - section 59 - and the SDIO protocol, CMD52/CMD53 and
  identification included, is shared there behind an `SdioHost` trait; the host itself is not
  `arch/aarch64/sdio.rs`, which only census-checks the controller, but `services/wifi-driver/src/host.rs`.)
- **What DOES carry over is everything above the firmware:** `crypto.rs`, `eapol.rs`, the join state
  machine, the credential table and `/wifi.keys`, the frame interface and `nic-driver`'s carrier rule,
  and the `wifi` utility. A full-MAC radio with a host-side handshake needs exactly those, and none of
  them names Broadcom.

(2026-10-04: the design is `docs/wifi-aic8800.md`.) So the VisionFive radio is a real third port and a second driver, not a variant of the first. It is
recorded here as the answer to section 1's question, and as scope that is NOT part of this branch
(section 9).

**Superseded (2026-10-04, `docs/wifi-aic8800.md`):** the VisionFive radio is now being built on this
branch. The `dw_mmc` host is in USERSPACE, not in `arch/riscv64`: phase V0, the kernel's side, is a census
of the radio's SD host and a grant of its window and power pin by the `WIFI_SDIO` kind
(`kernel/src/arch/riscv64/sdio.rs`), and is committed; phase V1, the userspace host
(`services/wifi-driver/src/dwmmc.rs`), is built and reaches identification only. Everything above
identification is the plan in that document. The riscv64 kernel's `hw_random` is still a stub; the JH7110 has a hardware generator of its
own, and filling that seam would help `net-stack` on the board whether or not the radio is ever driven.
*(2026-10-05: V2-V6 hardware-verified and the TRNG wired - `docs/wifi-aic8800.md` 4 and 10.)*

## 45. The first chaos run with the radio: 397 respawns, one join (2026-09-30)

> **Note (2026-10-01, later): read this section against section 55.** The "warm chip" below - a chip
> that came up with its firmware trapping at `pc 0x25` - was measured to be a SLOW HOST: every load that
> trapped ran after the Arm cores dropped to their minimum clock a minute after boot, and with the cores
> held at turbo every load comes up cold. What follows is kept as the record of what was tried and why;
> its conclusions about the chip's state are not established by it.

`chaos max-carnage` on the Pi 4, 826 rounds, with the radio carrying the link. The kernel did not
panic and nothing wedged. The radio was dead from round one.

**The numbers, before any theory.** 397 kills of `wifi-driver`, 397 respawns, 397 times `no SDIO card
answered on this bus`, and ONE `JOINED` in the whole log - the boot's. Every respawn failed at the
same step, so this is not a race and not the storm: the first kill was enough, and the board stayed
without a radio until power-cycled. `nic-driver` saw it as the driver's one-byte "radio down" answer
to every request (`answered 0x02 while we asked 0x10`), and `net` said `the radio is not joined`,
which was true.

**Why.** A respawn re-runs identification from CMD0, as section 43 says, and CMD0 is the wrong reset
for this. `GO_IDLE` returns a MEMORY card to its idle state; the SDIO specification leaves an I/O
card's function side untouched. The CYW43455 the dead instance left behind is initialised - RCA
assigned, selected, 4-bit, firmware running on its ARM - and from that state it does not answer CMD5.
A fresh power-up is the state `identify` was written against, and it was the only state it had ever
seen, because until tonight nothing had killed the driver.

**The reset the card actually needs** is the `RES` bit of CCCR `IO_ABORT` (function 0, address 6, bit
3), written through CMD52. That is what Linux's `sdio_reset` does before every SDIO probe
(`drivers/mmc/core/sdio_ops.c`): read-modify-write the abort register with `RES` set, then go idle,
then CMD5. It is a property of the device, not of their design (26.14), so `identify` now does it
first. On a fresh boot there is no initialised card to accept the write and it fails silently; it is
logged only when accepted, because that is the line that says an earlier instance was here.

**The second half, found by the next boot.** With the reset in, a 100-round storm gave 50 respawns and
50 identifications - and 50 times `ARMCR4_CAP 0x00000000 - ZERO memory banks` at stage 9, radio down
for the life of every instance. Same shape, one stage later: on a fresh boot the CR4 is in reset from
power-on and its capability register reads; on a respawn the dead instance's firmware is running on
it, and the register reads zero. brcmfmac makes the chip passive before it sizes the RAM
(`brcmf_chip_recognition`: "assure chip is passive for core register access"), and for the CR4 that
is a reset-core with the CPU halted - the core ends clocked, out of reset and halted, which is the
state its registers read in. The first attempt at this held the core IN reset, and a fresh boot
answered with the same zero, which is how that distinction was learned. `aicore::reset(halt = true)`,
the sequence stage 11 already performs for the upload, now runs before the stage 9 read as well. A
fresh chip happens to answer unhalted; one running firmware does not.

**The third stage, found by the boot after that.** Memory sized, firmware uploaded, `THE FIRMWARE IS
ALIVE` - and then `function 2 was enabled but never reported ready across 500 reads of IOR`. Two
differences from a fresh boot, both handled by the reference: brcmfmac's passive step for a CR4 chip
resets the 802.11 core too (`brcmf_chip_cr4_set_passive`), so the firmware never starts over a D11 the
previous firmware left running - done now, before the upload, with the state the core was found in
logged; and brcmfmac waits up to three seconds (`SDIO_WAIT_F2RDY`) for function 2 after the download,
where this driver asked 500 times and gave up in tens of milliseconds. The wait is by time now and
reports how long it took. A count is not a duration, and this is the second place in this driver that
lesson had to be paid for.

**The fourth stage, and the one the other three were symptoms of.** With all of the above in, every
respawn under a storm uploaded and the firmware came alive - reporting shared-structure flags `0x0401`
where the boot's firmware reports `0x0001`. Bit `0x0400` is the firmware's own TRAP flag: it crashes at
start on the state the previous firmware left in the chip, and a crashed firmware never brings function
2 ready, in 500 reads or in three seconds. brcmfmac's remedy for a warm chip is a power cycle through
the WLAN regulator, which on this board is a GPIO-expander pin behind the firmware mailbox that only the
kernel drives. Its remedy where it cannot cut power is a watchdog reset of the whole chip. So when the
RES write is accepted - an earlier instance was here - the driver opens the backplane just far enough to
arm the watchdog, waits, and identifies the card again as one just powered up; the second RES write says
whether the reset took. And when a firmware does report a trap, its trap record (type, epc, pc, lr, sp)
is logged, so the next such failure says where.

**Which watchdog, learned the expensive way.** brcmfmac's PCIe path arms the chipcommon `watchdog`
(0x80). Armed on the 43455, it reset the chip HALF WAY: the SDIO core went on answering CMD52 and never
answered CMD5 again, on that respawn and on the 46 after it, until the next power-on - strictly worse
than the warm chip it was meant to cure. Broadcom's own SDIO driver (DHD, `si_watchdog`) arms the PMU
watchdog (`pmuwatchdog`, 0x634) on a chip with a PMU and the chipcommon one only without. The 43455 has a
PMU. The driver arms 0x634 now, and the 0x80 result is kept in the code beside it so it is not tried
twice.

**Where it stands (2026-10-01, 00:02).** The PMU watchdog resets the chip whole: 52 armed under a storm,
48 confirmed by the card answering as freshly powered, every one re-identified, no CMD5 death. And the
firmware started on that chip still trapped, and said where: `trap type 0x1, epc 0x0009384c, pc
0x00000025, lr 0x00000025, sp 0x00000000`. A reset-class trap, in ROM below the RAM base, before the
firmware had a stack. The power-on boot never does this, so a watchdog reset is still not a power-on for
this part - something the ROM sets up from cold is not restored by it - and Broadcom's SDIO driver never
depends on it because it can cut power. **The radio does not survive a respawn of its driver on this
board without a power cycle of the WLAN regulator**, which is a GPIO-expander pin behind the firmware
mailbox that the kernel already drives once at boot. That is `backlog/69`; the five host-side steps above
are each right, each moved the failure one stage later, and each stays.

**One more, before any kernel change - the operator's call.** A power cycle wipes the chip's RAM and a
watchdog reset does not, and a reset-class trap before the firmware has a stack is the shape of cold-boot
code finding the previous run's state. DHD clears the top word of RAM before every download. On a warm
chip the driver now zeroes every word between the image and the NVRAM - all of RAM the upload itself does
not rewrite - before the NVRAM goes in. A fresh boot is untouched.

**Result (00:22 and 00:23, two post-storm instances):** 203 KiB zeroed each time, `0x22cc1d..0x25f92c`,
and the firmware trapped identically - `type 0x1, epc 0x0009384c, pc 0x00000025, lr 0x00000025`, the
trap record at `0x0025ff08`, the only change `sp` reading 4 where it read 0. The difference between a
watchdog reset and a power-on is not in the RAM the host can reach. The clear stays, because a warm chip
should not start on stale data whatever else is wrong, and because it cost one boot to learn that it is
not the answer. That is the end of the host-side chain: six steps, each a correct reading of the
reference, each one stage further, and the last one inside the chip's ROM. `backlog/69` is the record
and the remedy is the power cycle.

## 46. Adopt, do not restart: a respawn attaches to the firmware that is already running (2026-10-01)

Section 45 tried six ways to give a new firmware a chip it would boot on, and learned that only power
does that. The question it never asked was why a new firmware was wanted at all. A kill of the SERVICE
does nothing to the CHIP: the firmware the dead instance loaded is still running, still associated,
still asserting function 2 ready. brcmfmac's resume path with power kept re-attaches to exactly such a
firmware and reinitialises nothing on the card.

So `identify` now decides three ways from two CMD52 reads. A card that does not answer the CCCR is
fresh: the boot's path. A card that answers with function 2 enabled and ready was brought up by an
earlier instance and its firmware is alive: it is ADOPTED - no CCCR reset, no CMD0, no halt, no core
reset, no upload. Stages 9 to 11 are skipped; any transfer the dead instance left in flight is aborted
per function; the bus is brought up on the running firmware and it is asked the same first question the
boot asks. If it answers, the instance carries on: it scans, loads `/wifi.keys`, and joins as a fresh
one would, and the kill cost seconds. A card that answers the CCCR but has no function 2 belongs to an
instance that died before its firmware ran, mid-upload with the ARM halted; that is the state the boot's
own upload starts from, so it gets the RES reset and the fresh path.

**First result (00:53):** the firmware the dead instance loaded ANSWERED the new one - five adoptions
in a storm of eleven kills, the other six killed mid-attach - and the next step refused: `clmload`,
`BCME_NOTDOWN`. The regulatory blob is accepted only while the interface is down, a fresh firmware starts
down, and an adopted one is up and associated, which is the whole point of adopting it. The blob is sent
once per firmware now; an adopted firmware has had it. The rest of the bring-up - event mask, version
report, interface up - is idempotent on a running firmware and runs either way.

**Result (01:00, hardware):** a storm of five kills; the post-storm instance ADOPTED the running
firmware, skipped the CLM, scanned (12 networks), loaded `/wifi.keys`, joined, and `net` said `up via
wifi (the cable is out)`; `ping` 5 of 5. The kill cost the link about seven seconds. **The driver's
restart loop is closed in userspace**, with no reset of any kind and no power cycle: the service is
volatile, the firmware is not, and a respawn converges on the firmware it finds. `backlog/69` is
resolved by this section. What stays true from section 45: a firmware that has STOPPED - killed
mid-upload, or trapped - cannot be restarted on this chip without power, and that case is reported
rather than retried; when this was written it cost a reboot, and section 47 closes that - the driver power-cycles the chip ONCE; if the chip
still comes up warm the driver serves `radio down` with its reason, and `wifi radio powercycle` (one
cycle per run since section 52, re-runnable, section 48) is the way out. No reboot.

The PMU watchdog and the RAM clear are gone from the code, recorded above as tried. They were the reset
path's last two steps, and the reset path no longer runs on a chip with a live firmware. If the adopted
firmware does not answer, the driver says so and serves `radio down` with its reason (section 49)
rather than restarting a firmware the ROM will not boot; `wifi radio on` and `wifi radio powercycle`
take it from there.

The rule all four stages obey: a respawn inherits a chip the previous instance left RUNNING, and every
step written against the power-on state has to say what it does with a running one.

**What stays open from the same run**, recorded rather than folded in: the console after the storm
(`backlog/68`), and the parked lag showing through the post-storm ping as expected (`backlog/66`).
Section 43's claim that a respawn "rejoins the network last joined" is true only once identification
succeeds, which this section is the missing half of.

## 47. Power, at last: the kernel's grant made renewable, and the dead-firmware case closes (2026-10-01)

> **Note (2026-10-01, later): read this section against section 55.** The "warm chip" below - a chip
> that came up with its firmware trapping at `pc 0x25` - was measured to be a SLOW HOST: every load that
> trapped ran after the Arm cores dropped to their minimum clock a minute after boot, and with the cores
> held at turbo every load comes up cold. What follows is kept as the record of what was tried and why;
> its conclusions about the chip's state are not established by it.

Section 45 ended on the one thing the host could not do, and section 46 made it the rare case rather
than every case. This closes the rare case. Every reference driver's recovery path for this chip cuts
its power; on the Pi 4 that is `WL_ON`, pin 1 of the firmware's GPIO expander, reachable only through
the mailbox the kernel owns. So the kernel gained one syscall, `DevicePower`, and one resource,
`DEVICE_POWER` (CLAUDE.md 12.3 amendment, 2026-10-01).

**Why it is the kernel's, and why it is not a seventh responsibility.** The kernel already powers the
SD domain through this mailbox at boot, before it can grant this driver its window; a window to an
unpowered device is not a grant. The grant was never renewable. Now it is: the kernel mints
`DEVICE_POWER` with the window, to this service and nobody else, where the arch layer can power the
device behind it, and `DevicePower(on)` drives that device's pin - resolved from the caller's own grant,
in `arch/aarch64`, by the device kind (`WIFI_SDIO`) the window was granted by (CLAUDE.md 12.3, 2026-10-03
amendment); `arch/riscv64` answers the same seam for the VisionFive's radio. The kernel learns which pin. It does not
learn what the device is, whether its firmware is alive, or when to cut power. Those are this driver's,
and the two waits - WL_REG_ON held low, then the chip's own power-on before its SDIO side answers - are
facts about the chip and live here (`power_cycle_device`).

**Where the driver uses it, and only there.** Adoption (46) comes first, always. The power cycle is
for the two cases where there is no firmware to adopt: a card that answers CMD52 but has no function
2 (the earlier instance died before its firmware ran), and an adopted firmware that does not answer.
Both now cut the power, identify the card as one just powered up, redo the SDIO-side bring-up, and
take the boot's own upload path from stage 9. On a machine with no control over the device's power the
SDK call returns `false`, the CCCR RES path of section 45 remains, and the honest line about ROM traps
is what the log says. Two further uses came later the same day: a driver whose stage 3 finds no card
on its bus at all asserts the chip's power and identifies once more (section 49), and `wifi radio off
hard` cuts the power and leaves it cut. A power cycle is not a guaranteed power-on: it may still come up
warm (section 48), and the driver's own cycle runs once before it reports the radio down with its
reason. No reboot in any case the driver can reach.

**The operator's form: `wifi radio powercycle`.** The same cycle on request, composed from authority both
sides already hold. The shell asks the driver, which holds `DEVICE_POWER`, to cut and restore the chip's
power (the radio op's third mode); the driver answers, and the shell, which holds restart authority,
kills it. The respawn finds a card that does not answer the CCCR - the boot's own path - and the radio
comes up from power-on and rejoins. The order is the point: power first, then the kill, because a
respawn onto a chip whose firmware still runs would adopt it (46), which is the opposite of a power cycle.

**What a power cycle costs, and the first run's lesson.** On that first run the radio took about thirty
seconds from the cut to the join, which included nic-driver's backlog described next; the cold bring-up
itself, power-on to joined, is about twelve to fifteen seconds, and that is the figure the rest of this
document uses. The new instance serves nothing meanwhile. On the first run nic-driver kept asking it, the
full bound per request, and net-stack's exchanges queued behind that for fifteen seconds; ping was dead
for a minute after the join. nic-driver now treats a radio that has gone silent three requests running as
DOWN for a second between probes and answers net-stack at once, so the link reads honestly as down for
the bring-up and comes back by itself when the radio answers again. The rule above the rules, applied:
a quiet dependency makes its caller say "unavailable", never makes the caller quiet too.

**The hold-off, measured rather than assumed (07:35).** With the kernel reading the pin back after each
write - `WL_ON asked 0 - the firmware reads the pin back as 0`, then `asked 1 ... as 1` two seconds later -
the power cycle produced a cold chip: CMD5 answered, firmware alive with flags `0x0001`, joined. Fifty and five
hundred milliseconds had each produced a chip whose SDIO side reset and whose firmware trapped at start;
the pin reads confirm the writes took at two seconds, and two seconds is what the driver holds. One
sample at each value, said as such.

**What the same boot found one layer up.** The radio rejoined and `net` said it was not joined for the
rest of the session: nic-driver had reacquired the driver's capability exactly once, at the third
silence, 160 ms after the kill and before the respawn had registered its name, and every probe after
failed on that stale cap with nothing ever asking again. It reacquires on every failed probe now.
`wifi radio powercycle` also no longer returns to the prompt at the kill: it watches the driver's status
once a second and reports each change of state until the radio has rejoined, with `b` to background and
`q` to quit the watch - the power cycle itself cannot be stopped once the power is cut, and the line it prints say so.

**Verified, and repeated when it has to be (08:29).** Six cycles with the pin read back low: three cold
chips, three firmware traps at start. A hold-off is a power-on only sometimes, and nothing the host reads
predicts which. So `wifi radio powercycle` verifies: a driver that comes back reporting its radio down is
a warm chip, and the shell cycles again with the power off twice as long, up to sixteen seconds, for as
many attempts as it takes, saying which one succeeded - counted, never bounded, because a bound ends in
the one word nothing above the kernel gets to print. (Superseded by section 48: the operator ruled the
unbounded loop out, and the hold-off doubling went with it - three cycles per invocation, a fixed hold-off,
and the command can be run again.) And the upload now zeroes the chip's vector area
(0x0..0x400, where the reset vector goes) before the reset vector: a power-on leaves it zero, a warm chip
keeps the previous firmware's entries there, and every warm start trapped at `pc 0x25`, inside it. The hold-off travels in the request, so the policy is the shell's and the mechanism
the driver's; the driver serves the op from its "radio down" loop, which is where a trap leaves it.
Success is a join YOUNGER than the watch - the second cycle of 08:29 printed "succeeded" 157 ms after the
ON write, on a stale status reply from before the kill, and the join's age in the reply is what tells
the two apart.

**The ladder, as settled with the operator.** `wifi radio off` and `on` are the firmware's switch - the
chip stays powered, two seconds each way - and that is what a radio switch means everywhere else, so they
stay soft. `wifi radio off hard` is one rung down: the driver leaves the network while the firmware can
still say so, cuts the chip's power through its own grant, and stays alive to answer "powered down".
`wifi radio on` converges from either off: the soft switch when the firmware is up, and after `off hard`
it restores the power, restarts the driver, and watches the cold path to `radio on succeeded - joined`.
`powercycle` is off-hard-and-on in one act, one cycle per run (section 52; three as first built,
section 48). Each word names
what it does to the chip; `on` is the one the operator can always type without knowing the state. `off
hard` cuts the power and verifies the cut (section 49); it does NOT produce a cold chip on demand - `on`
after it trapped at 09:44 and again at 14:43. When `on` meets a warm chip it reports it and stops,
naming `wifi radio powercycle` as the one more try (section 52).

`wifi radio powercycle` is also re-runnable from every state the driver can be in: serving normally
(cut, restart, watch), in its radio-down loop (the op is served there), powered down after `off hard`
(the power is restored and the cold start watched), mid-bring-up and not answering (the shell restarts it,
and the respawn adopts a live firmware or power-cycles a dead one), or dead (the supervisor has already
respawned it). None of them ends in a word that is not this system's.

**The state that had no way out, and the two sentences that were one byte.** Boot 2026-10-01 09:44:
`off hard` held the chip powered down for 75 s with the pin read back low, `on` restored the power, and
the firmware trapped at start - so the driver sat in its serve loop with no session. `powercycle` typed
then printed "this machine has no control over the radio's power", and the kernel log shows it was never
asked. The driver's radio-down arm answered `RADIO_DOWN` to every op, the power ops included, and the
shell had been reading that byte as the kernel's refusal because the two cases shared it. Two fixes,
neither of them to the kernel. The driver serves `powercycle` and `off hard` with its radio down - that
state is exactly what the power ops exist for - and the kernel's refusal has its own code,
`NO_POWER_CONTROL`, so "down" and "powerless" can never be confused again. And the shell's `on`, which
did the soft switch and nothing else, is now the HARD ON when the radio is down: it restarts the driver
(the respawn adopts a live firmware or power-cycles a dead one) and watches it join; if the chip comes up
warm it hands over to the powercycle loop rather than telling the operator which command to type next.
**Superseded (section 52):** it no longer hands over. A second cycle on the same chip gives the same
result, so `on` reports the warm chip and stops, and its line names `wifi radio powercycle` as the one
more try.
`on` is the one word that always converges; the operator does not need to know the rung.

`off hard` blocks until the kernel has read the pin back low. As first written it offered `[b]
background` while it waited, on the reasoning that the driver finishes the cut whether or not the shell
waits, so `q` would promise a stop that cannot happen. **The key was removed on 2026-10-01:** the
request is a blocking kernel `Call`, and a shell blocked in a `Call` cannot read the console, so no key
could have been noticed until the reply had already arrived. `off hard` now blocks for about three
seconds and offers no key, and says so, rather than printing a hint it cannot honour. The soft `on` and
`off` went the same way and offer no `[q]` (section 49). The SDK's abortable wait still takes its leave
keys from the caller, and the q-hint wrapper every other command uses passes the same three it always did.

**And the hold-off was never the variable.** Seventy-five seconds powered down is longer than any
capacitor on that rail holds, and the chip still came up warm. What differs between this driver's power-on
and a board power-on is not the chip's side but the HOST's: Linux's `mmc_power_up` raises the card power
with the clock at ZERO and starts the init clock only after the power-on delay, and a Broadcom part samples
its boot straps - some of them on the SDIO data lines - at the rising edge of WL_REG_ON. This driver left
its 25 MHz clock running on those lines through the edge, which is a coin flip on the boot mode and matches
the one-in-two warm starts above better than any theory about residual charge did. The card clock was
then stopped just before every power-on, and the respawned instance re-initialised the host from reset.
The prediction was that cold starts would stop trapping, and that a trap with the clock stopped would
refute it cleanly. **It was refuted:** section 48.

## 48. The host parked across the cut, and a bounded powercycle (2026-10-01)

> **Note (2026-10-01, later): read this section against section 55.** The "warm chip" below - a chip
> that came up with its firmware trapping at `pc 0x25` - was measured to be a SLOW HOST: every load that
> trapped ran after the Arm cores dropped to their minimum clock a minute after boot, and with the cores
> held at turbo every load comes up cold. What follows is kept as the record of what was tried and why;
> its conclusions about the chip's state are not established by it.

**The clock-only change was refuted, and may have made it worse.** Boot 2026-10-01 11:17 ran with the card
clock stopped before every power-on. Seven firmware loads completed on warm restarts and all seven
trapped (`flags 0x0401`, `pc 0x25`). The documented rate before the change was about four cold in seven
(section 47: one of one, then three of six). Seven of seven is not proof of a regression at these sample
sizes, but it is no improvement, and the change is removed. The shell's retry counter in that run read
higher than seven because several of its cycles were cut short before a firmware load finished; only
completed loads are samples.

**What no build had tested: a host that is silent for the whole off window.** In every build before this
one the 25 MHz card clock kept toggling into the unpowered chip from the cut to the power-on - two
seconds in a powercycle, seventy-five in the `off hard` test - and the clock-only change touched only the
last instant. At mains boot the VideoCore raises WL_ON with the Arasan untouched, and boot comes up cold
every time. Two causal readings fit that, and the same change tests both: the toggling lines back-power
the chip through its I/O pads while its rail is nominally off, and the lines are not in their boot state
at the rising edge where the chip samples its straps. So the driver now PARKS its host - a full software
reset (SRST_HC), card and internal clocks off, nothing in flight - immediately after the kernel accepts
the cut, and again just before it restores the power, and nothing re-enables a clock until the next user
of the host brings it back from reset after the power-on delay (refuted the same afternoon - see
**Result** below; the park stays in the code, as the host's honest state while the chip is
unpowered, but it is not the fix). The respawned instance does that at stage
2; the two in-place paths (`sdio::identify` and the adopt-failure branch) do it before CMD0, because
`identify_once` sets no clock. The park follows the accepted cut rather than preceding it, so a cut the
kernel refused leaves the host and a live session untouched; the gap is the mailbox's return, well under
a millisecond of a two-second quiet window.

**What a userspace park cannot reach.** The SDIO pads' pull resistors live in the GPIO block, which the
kernel muxes at boot and this driver cannot touch. If the parked host still comes up warm, the next
experiment is the kernel's: park the pads themselves (input, pulls off) inside `DevicePower` for the off
window. That is a wider kernel change than section 47's and needs the operator's word before it is made.
**That condition is now met** (see the result below): the parked host still comes up warm. The pad
experiment is the next one, and it waits on the operator's word; it has not been made.

**`wifi radio powercycle` is bounded.** *(One cycle per invocation since section 52.)* Three cycles per invocation, then the prompt, saying how each
ended; the command can be run again, and nothing needs a reboot. The hold-off is fixed at two seconds.

**The prediction, specific enough to be wrong.** Ten `wifi radio powercycle` runs, each from a joined
radio: if the park is the fix, every run ends `powercycle succeeded - joined` on its first attempt and the
driver log shows `flags 0x00000001` for each load. Near half cold means the park changed nothing that
matters and the pads are next. None cold means the park made it worse, and that is information too.

**Result: refuted.** The parked host did not stop the warm starts. Boots of 2026-10-01: at 13:58 the
radio came up cold; at 14:30:39 and again at 14:30:54 a `powercycle` cycle trapped at start - one cold in
three across those three samples, no better than the unparked rate. At 14:43:49 `wifi radio off hard`
followed by `on` trapped as well, and the 09:44 boot (section 47) had already shown seventy-five seconds
powered down trapping without the park. For comparison, the clock-only change before it got none of
seven. The samples are small and said as such, but the prediction was "every run cold on its first
attempt", and three of four warm is a clean refutation of that. What the park rules out is the host's
clock and controller state across the off window; what it cannot rule out is the pads, which is the
kernel experiment above, and that waits on the operator's word. Until then the warm chip is a reported,
recoverable state rather than a fixed one: the driver serves `radio down` with its reason (section 49)
and `wifi radio powercycle` is re-run until it comes up cold. One caution on the 14:30 run: its
attempts 2 and 3 reported "came up warm" within 150 ms of the power-on, which no chip does - that was a
stale `radio down` reply, not the chip (section 49). As in the 11:17 count above, only completed firmware
loads that logged a trap are counted as samples.

**On other radios.** The adopt test of section 46 is SDIO-standard - the CCCR, `IO_ENABLE`, `IO_READY`
- and transfers to any full-MAC SDIO radio whose firmware the host loads; the protocol used to ask that
firmware whether it is alive does not (BCDC/SDPCM is Broadcom's), and neither does the power pin, which
is a board fact the arch layer answers per device. The VisionFive 2 Lite's AIC8800D80 (44) would reuse
the shape of both and none of the code.

## 49. Replies matched by sender, a verified off, and why the radio is down (2026-10-01)

**The stale reply, found by what it broke.** Every shell radio command goes through one helper,
`wifi_ask`, which sends the request and waits in a kernel `Call` for the answer. The kernel matches a
`Call`'s reply by SENDER, not by request: any message from the driver that lands in the shell's reply
mailbox is taken as the answer to whatever the shell asked last. A request the shell had stopped waiting
for - a bound that expired, a watch that moved on - still gets its answer eventually, and that answer
then sits in the REPLY MAILBOX until the next request collects it as its own. Two boots showed what that
costs. At 13:59 (boot 13:58) a stale `OK` was read as the answer to a power request, the shell went on to
kill the driver mid power cycle, and the chip was left unpowered with nobody holding it. At 14:30 a late
`radio down` was collected by `powercycle`'s attempts 2 and 3, which therefore reported the chip "came up
warm" 150 ms after the power-on - a time no chip comes up in - and counted two cycles that measured
nothing (section 48 excludes them).

**Two earlier fixes cleared nothing, and said so.** The first drained stale messages at the start of each
command; the second skipped unmatched replies in a loop. Both read the shell's MAIN endpoint, and the
stale replies were never there - they were in the mailbox. The serial log at 14:43 printed `0 stale
message(s) cleared` on every line, and the powercycle watch was blind for 90 s behind replies it could
not see. A drain that reports zero every time is an instrument that never fires.

**The fix now in the code** (image `2f4a1e32`, on the card, NOT yet run on hardware): `drain_stale_replies`
empties the reply mailbox itself; the shell counts the replies it is still OWED (requests whose bound
expired before the answer came); `drain_owed_replies` waits for those in the mailbox before anything new
is asked; nothing is sent while any are owed; and a debt older than 30 s is forgiven, because a driver
that died owes nothing and the shell must not wait for it forever.

**Known gap, recorded rather than fixed.** While replies are owed, `wifi_ask` returns "no answer" WITHOUT
sending the request. Every caller reads "no answer" as the driver not answering. So `powercycle` can take
it for a silent driver and restart the driver without the power ever having been cut - a respawn that
adopts the running firmware rather than a power cycle - and an ordinary verb can print "not answering"
about a request that was never sent. The honest fix is a distinct outcome for "not sent, replies still
owed"; until it exists, a `not answering` within 30 s of an abandoned command may be this rather than the
driver.

**Superseded (section 58).** The two names above are SDK methods (`ServiceContext`), and the shell's
radio path no longer rests on them: `drain_owed_replies` is not called by the shell at all, and
`drain_stale_replies` is only the first step of the shell's own `wifi_drain_stale`, which clears the reply
mailbox of OTHER peers' late replies. Radio answers now arrive on the shell's main endpoint, every request
carries its own tag, and `wifi_sift` keeps only the reply that carries it, counting a late answer to an
abandoned request off what is owed rather than waiting for it. The known gap is closed: a request held back while answers are still owed is counted
(`wifi_unsent`) and reported as its own line, `wifi: not sent - the radio driver still owes ...`, never
as "no answer".

**Why the radio is down, said by the driver.** `wifi status` with the radio down used to print one line,
"did not come up at boot", which was wrong after a respawn and silent about the cause. The driver's
`RADIO_DOWN` reply now carries the reason in byte 1, and the shell says it:

- `DOWN_TRAPPED` - "wifi: the radio is down - its firmware trapped at start (the chip came up warm); `wifi radio powercycle` cuts its power and tries again"
- `DOWN_NO_RADIO` - "wifi: the radio is down - the driver found no working radio on its bus; `wifi radio powercycle` restores the chip's power and tries again"
- `DOWN_BRINGUP` - "wifi: the radio is down - the driver's bring-up stopped before it was up (the serial log names the stage); `wifi radio powercycle` tries again"

After `wifi radio off hard`, status says `radio off (hard - the chip is powered down; wifi radio on powers
it up)`; after the soft `wifi radio off`, `radio off (soft - the firmware's switch; the chip stays powered;
wifi radio on turns it back on)` (2026-10-02 - it said a bare `radio off` until then, which could not be told
from the hard one). "Did not come up at boot" is gone.

**An off that is checked, not assumed.** The soft `wifi radio off` used to report success when the
firmware accepted the command; it now asks the firmware back with `WLC_GET_UP` (162) and reports what it
says. `off hard` verifies the cut from the bus: 50 ms after the kernel accepts it, the driver sends a CMD52
and expects nothing to answer. Reply byte 3 carries the verdict - 1 verified, 2 unverified (the check could
not be made), 3 contradicted (the chip still answered) - and the shell prints `... - verified: ...` for the
first and `... FAILED ...` for a contradiction, never a bare "done". `off hard` blocks for about three
seconds and offers no key; the soft `on` and `off` block too and offer no `[q]`, because a shell blocked in
a `Call` cannot read the console (section 47). A soft `on` that rejoins can take up to 15 s.

**A bus with no card on it gets the power asserted.** A driver whose stage 3 finds no card at all - not a
card with a dead firmware, but nothing answering CMD5 - used to report "no working radio" and stop. A
chip left unpowered (13:59 above is how) looks exactly like that. The driver now asserts the chip's power
through `DevicePower` and identifies once more; only if the bus is still empty does it serve `radio down`
with `DOWN_NO_RADIO`.

**Known gap: the loop for a driver that never reached its firmware.** `serve_unavailable` is where the
driver waits when it has no window, the host failed, the card was still missing after the retry, or the
backplane would not open. It serves `powercycle` and nothing else: `off hard` is answered with
`RADIO_DOWN`, and the shell, reading that as a driver that cannot act, suggests `kill wifi-driver` - which
is the wrong advice for a request the driver could have honoured. Recorded here and in the driver's
comment; `powercycle` works from that state.

> **Closed 2026-10-02.** `serve_unavailable` now serves `off hard` and, once powered down, `on`, a
> repeated `off`, and the powered-down status, with the reply shapes of `serve_radio`'s powered-off arms -
> so the shell sees one shape for one state whichever loop holds it. Where there is no SDIO window (QEMU's
> `raspi4b`, any board without the radio) there is nothing to cut, and `off hard` answers
> `NO_POWER_CONTROL`: "this machine has no control over the radio's power", which is true, instead of
> advice to kill the driver. In the same change `wifi status` says which OFF a radio is in both ways - the
> soft one printed a bare `radio      off`, and now says `off (soft - the firmware's switch; the chip stays
> powered; ...)`.

**On hardware (boot 2026-10-01 16:36, image `0b24562a`).** `wifi radio off` printed "verified: the
firmware reports it is down" and `on` rejoined; `off hard` printed "verified: the chip no longer answers on
its bus"; the shell cleared one late reply before each request instead of reading it as an answer, and all
four power-ups that followed were real attempts. All four trapped at start - see section 50.

## 50. The radio's pins, parked as boot leaves them (2026-10-01)

> **Note (2026-10-01, later): read this section against section 55.** The "warm chip" below - a chip
> that came up with its firmware trapping at `pc 0x25` - was measured to be a SLOW HOST: every load that
> trapped ran after the Arm cores dropped to their minimum clock a minute after boot, and with the cores
> held at turbo every load comes up cold. What follows is kept as the record of what was tried and why;
> its conclusions about the chip's state are not established by it.

**What the verified off settled.** With the chip's power cut, verified silent on its bus 50 ms later, and
the host held in reset for the whole window, the chip still came up warm: one cold start in eight loads.
So the warm state is not the chip staying powered and not the host controller.

**The one difference left.** Every boot log reads `sdio: GPIO34-39 fsel=000000`: when the VideoCore powers
the radio at boot, its six SDIO pins are plain GPIO inputs, and only afterwards does the kernel route them
to the Arasan (ALT3) with pull-ups. Boot always comes up cold. Every power cycle runs with the pins routed
and pulled up - through the off window and at the rising edge of WL_REG_ON, where the chip samples its
straps, and where a pull-up can hold an unpowered chip's I/O up through its clamp diodes. Boot now also
logs the pulls it found (`sdio: GPIO34-39 pulls at boot=`), so the boot state is measured rather than
assumed.

**The change.** The `DevicePower` grant gains a second half (CLAUDE.md 12.3, amendment of the same day):
park the radio's pins (input, no pull) and restore them (ALT3, pull-up, as at boot). The kernel only moves
the pins; the driver decides when. Its order: cut the power; verify the chip is silent (a CMD52, so the
pins must still be routed); park the pins for the off window; raise the power with the pins parked; wait
50 ms past the edge; restore the pins; settle; identify. `device_power` also now reports failure when the
pin reads back the wrong level, instead of success.

**Result: refuted, and reverted (boot 2026-10-01 17:02).** Seven loads after a cut with the pins parked
and every step read back as asked: seven traps. Worse, the boot log measured what this section assumed:
`sdio: GPIO34-39 pulls at boot=111111` - at boot the pins are inputs WITH pull-ups on all six, so the
"no pull" park did not reproduce boot either, and boot has pull-ups on those lines and still comes up cold.
The pins are not the variable. The kernel change (two more `DevicePower` operations) was never committed;
it was reverted rather than kept, because a widened syscall that did not earn its place should not stay.

## 51. What Linux and OpenBSD do, read from their source (2026-10-01)

> **Note (2026-10-01, later): read this section against section 55.** The "warm chip" below - a chip
> that came up with its firmware trapping at `pc 0x25` - was measured to be a SLOW HOST: every load that
> trapped ran after the Arm cores dropped to their minimum clock a minute after boot, and with the cores
> held at turbo every load comes up cold. What follows is kept as the record of what was tried and why;
> its conclusions about the chip's state are not established by it.

Read from Linux master and OpenBSD `a5d3ee8e` the same day, not recalled.

**Linux (brcmfmac).** `ip link set wlan0 down` and an rfkill soft block only disassociate and stop
scanning - not even the firmware's DOWN; the chip stays powered. The radio's power is `WL_ON` through
`mmc-pwrseq-simple` (`reset-gpios = <&expgpio 1 GPIO_ACTIVE_LOW>`), which drives it LOW when it probes, so
every Linux boot starts with a real power cycle. A firmware that reports its own halt (`HMB_DATA_FWHALT`)
is recovered automatically: the card is removed and rescanned, which cuts and restores `WL_ON` (held low
about 12 ms, clock stopped through the edge), re-enumerates, and re-downloads - no reboot. A firmware that
hangs silently, or fails to load at probe, gets no automatic recovery (the driver unbinds). `BT_ON`
(expander pin 0) belongs to the Bluetooth driver and WiFi recovery never touches it.

**OpenBSD (bwfm).** `ifconfig bwfm0 down` sends DOWN and then UP again, leaving the firmware running in
power-save. It never controls the radio's power on a Pi 4 - the SD driver for that controller never runs
a power sequence and there is no driver for the firmware GPIO expander. A missing firmware file is retried
by the next `ifconfig up`; a firmware that crashes after loading is recovered only by an OS reboot.

**What Linux does that this driver did not.** Four steps, in Linux's places: an SDIO I/O reset (CCCR RES)
before CMD0 on every power-up; and, after the cores are passive and before the download, KSO (keep SDIO
on), CARDCTRL WLANRESET, and PMU RES_RELOAD. Linux's recovery rests on the same `WL_ON` cut plus these, so
they are the next suspect - and they are driver-only, no kernel change. Added together as one change;
the prediction is the same as before: after `off hard` / `on` and `powercycle`, loads come up
`flags 0x00000001` and join.

**Result: no change (boot 2026-10-01 17:30).** Four loads after a cut, four traps. The steps ran and read
back as written. They are kept - they are what the reference does, and harmless - but they are not it.

## 52. Time on, not time off, and one attempt (2026-10-01)

> **Note (2026-10-01, later): read this section against section 55.** The "warm chip" below - a chip
> that came up with its firmware trapping at `pc 0x25` - was measured to be a SLOW HOST: every load that
> trapped ran after the Arm cores dropped to their minimum clock a minute after boot, and with the cores
> held at turbo every load comes up cold. What follows is kept as the record of what was tried and why;
> its conclusions about the chip's state are not established by it.

**The registers are the same cold and warm.** The same boot logged the chip's state just before each
download: SLEEPCSR `0x03`, CARDCTRL `0x01`, PMU control `0x01770181` - identical on the cold boot that
worked and on every power-up that trapped. Whatever differs is not in those registers.

**What still differs is time.** At boot the VideoCore raises `WL_ON` seconds before this driver's first
command; after a cycle the driver began 300 ms after. The OFF time was varied from 50 ms to 75 s and never
mattered; the ON time never was. If the chip's own power-on initialisation is still running in ROM when the
driver halts it and loads the firmware, a trap in ROM is what that would look like. `POWER_ON_SETTLE_MS`
goes from 300 ms to 5 s.

**One attempt.** The operator's observation, and the data's: the attempts were not independent - the same
steps on the same chip gave the same result, three warm out of three, run after run. A retry only hid the
cause. `wifi radio powercycle` makes one cycle and reports, and a hard `on` that comes up warm reports and
stops rather than handing over to a loop.

**The prediction.** `off hard` then `on`, and `powercycle`: with five seconds on, the loads come up
`flags 0x00000001` and join. Still trapping means time on is not it either, and what remains is `BT_ON`
(a log-only kernel check first) and the SD I/O supply rail.

**Result: refuted (boot 2026-10-01 17:42).** Three loads after a cut, each with five seconds on before the
first command - `off hard` then `on`, `on` from a radio that was down (the respawn cut and restored the
power itself), and `powercycle` - all three trapped. Time on is not the variable. One attempt each, each
reported and returned: the one-attempt behaviour stays. Ruled out so far, each by a boot: time off (50 ms
to 75 s), the host controller's state, the pins, Linux's four pre-download steps, time on, and the chip's
registers before the download. What remains untested is `BT_ON`.

## 53. BT_ON - the other half of the chip's power (2026-10-01)

**The hypothesis.** The CYW43455 is a WiFi + Bluetooth combo with two power enables on the expander:
`WL_ON` (pin 1), which every cut has driven, and `BT_ON` (pin 0), which nothing in GodspeedOS had touched.
On Broadcom combo chips the shared regulators and power management stay up while EITHER enable is high. At
boot both start low and the VideoCore raises them, so the whole chip comes up from nothing - and boot is
always cold. If `BT_ON` stays high through a cut, the shared domain never loses power, and that fits every
result in sections 48-52: the WLAN side goes silent on its bus, and nothing the host or the timing changes
makes a difference. Linux also cuts only `WL_ON`; nothing found shows its recovery working on a Pi 4.

**The change (kernel, arch only).** The driver's spawn logs `BT_ON`'s level. A cut drops BOTH enables,
remembering whether `BT_ON` was high; a power-up raises `BT_ON` first (only if it was high), then `WL_ON`,
and logs both. `device_power`'s result follows the `WL_ON` read-back again. The power-on settle goes back
to 300 ms.

**The prediction.** If the spawn log shows `BT_ON reads 0`, the hypothesis is dead without further work.
If it reads 1, a cut now takes it to 0 (`BT_ON was Some(1), now reads Some(0)`), and loads after `off
hard` / `on` and `powercycle` come up `flags 0x00000001` and join.

**Result: refuted (boot 2026-10-01 18:01).** `BT_ON` reads 0 at spawn. Nothing holds the shared domain up,
and the cut never touched the pin. The cut code is removed; the spawn log stays, and so does the read-back
result (`CLAUDE.md` 12.3). One power cycle in that session came up cold and joined, and every later one
trapped - section 54 is what told them apart.

## 54. The upload's speed, and the clock it runs on (2026-10-01)

**A difference at last.** The same boot's log, read stage by stage: the cold upload took 3.26 s, and every
upload after a cut took 7.19 s (plus or minus 0.01, five times). Each 32 KiB window: about 157 ms cold,
about 370 ms warm. The bus is the same in every run - 1-bit, 25 MHz, `CONTROL0` 0, CCCR bus 0x40 - and the
stages that are mostly host work run at the same speed, so the host is not slower. Single-register reads of
the card (the CIS walk) are slower too, 0.36 s against 0.61 s. Each 1 KiB write costs 4.9 ms cold and 11.6
ms warm against 0.33 ms on the wire; the time is spent waiting on the card. The one power cycle that came up
cold had a boot-speed upload. After alive, `CHIPCLKCSR` reads 0x69 cold and 0xe9 warm - HT already up.

**What Linux does that this driver did not** (`brcmf_sdio_download_firmware`, read from master the same
day). It opens with `brcmf_sdio_clkctl(bus, CLK_AVAIL, false)` - `HT_AVAIL_REQ` written alone, HT waited
for, `alp_only` false for this chip - so the download runs on HT. Once the ARM is running it calls
`clkctl(CLK_SDONLY)`, which writes 0: the firmware boots with its clocks its own. This driver downloaded
with `FORCE_ALP` held and released the ARM with it still held.

**The change (driver only).** HT is requested and waited for (bounded, said either way) before the upload;
0 goes to `CHIPCLKCSR` after the release, then the HT request again before the alive check, which is the
reference's next step. Every block write now counts the polls spent waiting on the card, so the log says
where an upload's time goes.

**The prediction.** Uploads get faster, cold and warm. Then either the trap is gone - loads after `off hard`
/ `on` and `powercycle` come up `flags 0x00000001` and join - or the HT request is slow or refused after a
cut, which would put the PLL and PMU state that survives the cut on record. Faster and still trapping means
the clock is not it.

**Result (boot 2026-10-01 18:25): the clock is not it, and the counters found what is.** The chip never
grants HT before the download - `CHIPCLKCSR` 0x69 -> 0x50 after 100 ms, every time, cold or warm - so the
upload stayed on ALP; releasing the clocks and re-requesting HT after the ARM starts works (HT after one
poll). Two power cycles came up cold and joined: the boot load and the FIRST `powercycle`. Four more
trapped. The new counters split them cleanly:

| load | time after boot | upload | polls waiting on the card (FIFO / completion) | result |
|---|---|---|---|---|
| boot | ~4 s | 3.0 s | 8925 / 186739 | cold |
| powercycle 1 | 41 s | 3.0 s | 8925 / 186757 | cold, joined |
| powercycle 2-5 | 85 s on | 6.9 s | ~3800 / ~90580 | trapped |

A slower upload that spends FEWER polls waiting on the card is a slower HOST: each poll takes longer, the
card's own time is unchanged, and the host's fixed per-command work doubles. The card is not the variable;
the Arm cores' clock is.

## 55. The Arm cores slow down a minute after boot (2026-10-01)

**Read from the firmware's documentation** (`config.txt`, the same day): `initial_turbo` "enables turbo mode
from boot for the given value in seconds, or until `cpufreq` sets a frequency", and its default became 60
in the November 2024 firmware. GodspeedOS has no cpufreq, so a minute after boot the cores drop to their
minimum clock and stay there. Every load inside that minute came up cold - the boot's, and the first
`powercycle` in this session and in the earlier one that worked - and every load after it was slower and
trapped. The 2.3x matches a turbo-to-minimum drop on a loop that is mostly the host's own work. This also
corrects section 54's reading that the host was not slower: the stages compared there are paced by the
serial port, not the CPU.

**Why a slower host could make the chip trap** is not known yet, and is not claimed. The test is the
direct one.

**The change (board config only, no code).** `force_turbo=1` in `boot/pi4/config.txt` keeps the cores at
their turbo clock. No `over_voltage_*` is set, which is the condition under which it can set the warranty
bit.

**The prediction.** Every upload takes about 3 s, however long after boot, and `powercycle` - run well
past the first minute, several times - comes up `flags 0x00000001` and joins every time. Fast uploads that
still trap would mean the speed was a fellow traveller of something else that changes a minute after boot.
If it holds, the lasting fix is the kernel asking the firmware for the clock it wants rather than a
`config.txt` line, and that is a separate change.

**Result: confirmed (boot 2026-10-01 18:30).** Eight loads from 18:31 to 18:36 - the boot's, six
`powercycle`s, and `off hard` then `on` - every one `flags 0x00000001`, every one joined, none trapped,
out to six minutes after boot. Every stage 11 took 4.27 s (plus or minus 0.01; it includes the 1.3 s spent
on the HT request that the chip never grants before the download), and every one waited about 191,100
polls on the card, the cold figure. The soft `off` / `on` and `off hard` / `on` both verified and rejoined.
With the cores held at their turbo clock there is no warm start: the "warm chip" of sections 45-54 was a
slow host.

**What is still not known** is the mechanism - why an upload paced at about 40% speed leaves a firmware that
traps at `pc 0x25`. It is a timing property of the chip's start-up, and the fix does not depend on knowing
it, but it is recorded as open rather than guessed at.

## 56. What section 55 changes about the record (2026-10-01)

**The HT request before the download is gone.** This chip never grants HT while its CPU is halted - 0x69 ->
0x50 after 100 ms on all eight loads of the confirming boot - so the request did nothing and cost 1.3 s per
load. That is a recorded divergence from Linux's `brcmf_sdio_download_firmware`; the clock release and the
HT request after the ARM starts are kept, because those are granted at once.

**Sections 45, 47, 48, 50, 51 and 52 now open with a note.** Each tested a theory of a "warm chip" - state the chip kept
through a reset or a cut - and each warm load in them ran on the slow clock. What they record as tried is
true; what they conclude about the chip's state is not established. In particular, section 45's six
host-side resets were never tried on a fast host: whether any of them alone would recover a dead firmware
is OPEN, not refuted. The power cycle stays the recovery the driver uses, because it is now shown to work
every time, but it is no longer shown to be the only one.

**What section 55 does not settle.** Why an upload paced at about 40% speed leaves a firmware that traps at
`pc 0x25`. And the fix is a `config.txt` line: the driver still depends on the host's speed, which is
recorded rather than hidden, and any board whose cores run slow would show it again.

## 57. A lease on the clock instead of `force_turbo` (2026-10-01)

**The change.** `force_turbo=1` held the cores at turbo for the machine's whole life to make one driver's
three-second load work. It comes out of `boot/pi4/config.txt`. In its place a new `power` service holds
the authority to set the Arm clock to the firmware's minimum or maximum (`CpuClock`, syscall 55, CLAUDE.md
12.3) and hands out leases (`docs/power.md` 15). It puts the clock at its minimum when it starts; this
driver asks it for a 20 s lease as soon as it holds the SDIO window, and hands it back when it starts
serving, on every exit. A `power` that is absent or refuses costs one log line and the load goes ahead at
whatever the clock is.

**QEMU.** The whole path ran: the kernel minted `CPU_CLOCK` to `power`, `power` set the minimum at start,
the driver took lease 1, the clock went to its maximum, the release brought it back down. QEMU reports
the same 700 MHz at both ends, so the real range is the card's to show.

**The prediction.** At boot `power` logs the clock going to its minimum; each load logs `Arm clock held at
... MHz for the load` at the Pi 4's maximum, and the release logs the minimum again. Every upload takes the
fast time (about 3 s), and `powercycle` - well past the first minute, several times - comes up
`flags 0x00000001` and joins, exactly as it did under `force_turbo`. Between loads the clock reads its
minimum. A load that is slow while the log says the lease was granted would mean the firmware did not
honour the rate, and the `cpu-clock:` read-back line would say so.

**Result: confirmed on the card (boot 2026-10-01 20:29, commit `3dd21a66`, no `force_turbo`).** Five loads,
every one `flags 0x00000001` and joined, no trap, no lease expired, no panic:

| load | after boot | upload |
|---|---|---|
| boot | 8 s | 3.19 s |
| `powercycle` | 2 min 35 s | 3.25 s |
| `powercycle` | 2 min 58 s | 3.25 s |
| `powercycle` | 3 min 17 s | 3.25 s |
| `off hard`, then `on` | 3 min 48 s | 3.24 s |

`power` put the clock at its minimum at start-up - **600 MHz** on this Pi 4 - and each load took a lease,
ran at **1500 MHz**, and handed it back when the driver started serving, about ten seconds later; the clock
read back at its minimum again every time. So the radio is fast only while it loads, and every power
cycle is a power-on. `backlog/69` is closed with this: a respawned driver adopts a running firmware (46),
and a stopped one is power-cycled cold, every time.

**Still open, and recorded rather than chased:** why a firmware uploaded with the cores at their minimum
traps at `pc 0x25`. The fix does not depend on it - the dependence is measured and the lease removes it -
but a board whose cores run slow for any other reason would show it again.

## 58. Every request to the radio carries a tag, and the wifi reports pipe (2026-10-02)

**Why.** A `Call` takes the oldest reply FROM the driver, not the reply to the request just sent, so an
answer the shell had stopped waiting for was read as the next request's (sections 47 and 49 record the
boots where that broke `powercycle`). The shell guarded against it by counting what the driver owed and
skipping that many - and counted them by watching its reply mailbox, which takes every peer's replies.
In QEMU a stray 2-byte reply from another service cancelled an owed radio answer, and the next `wifi
status` printed the previous one's answer (`backlog/70`).

**The tag.** The shell now sends `[0xE7, tag, op, ...]` (`scan::reply::TAGGED`), and the driver serves
`[op, ...]` as before and sends its reply back as `[0xE7, tag, status, ...]` - stripped and re-applied in
one place in each serve loop, so no arm changed. An untagged request is served exactly as it was, which is
what `nic-driver`'s frame ops are. Every radio wait in the shell is a SIFTED wait on its main endpoint
(`wifi_sift`): the reply carrying this request's tag ends it, a reply carrying an older tag is a late
answer and is counted off what is owed, and anything else is dropped with any cap it carries released.
The owed count is therefore exact, and a request held back while answers are owed says so: `wifi: not
sent - the radio driver still owes 1 answer(s) to earlier requests`. Tab completion uses tag 0, which the
shell's counter never hands out. No kernel change; the SDK gained the sifted form of its key-abortable
wait (`request_with_reply_keyhint_sifted`), for the join and the power verbs.

**QEMU** (`raspi4b`, a test-only driver answering `wifi status` 8 s late): the first request timed out and
was owed, the second was held back with the `not sent` line, the third cleared the late answer by its tag
(`1 of them late radio answers`) and was sent. **On the card** (boot 2026-10-02): every wifi verb as before,
`chaos max-carnage` 50 rounds recovered, and none of the `reply cap is dead` lines `backlog/67` left.

**The reports pipe.** `utilities/56_wifi.md` section 3 said `wifi list | count` worked; the shell refused
it, because `wifi` was never on its list of pipe producers. It is now, for the REPORT verbs - `list`,
`stored`, `status`, `info`, `debug`, `version`. The actions refuse with a sentence (`join` reads a
passphrase from the console, and piped its prompt would vanish into the pipe), a report that fails puts
its words on the console and stops the pipe, and an empty scan is zero rows in a pipe, not a row saying
so. QEMU, with a test-only driver serving three canned networks: `wifi list | count` 3 lines, `| match
WPA2` two, `| sort` sorted, the pipe at 29% of the shell's stack.

**Later the same day: `wifi list` is records in a pipe.** The text form could filter and count but not
order by signal - `sort` on text is alphabetical. In a pipe `wifi list` is now a record table (`network`,
`band`, `signal`, `dbm`, `security`, `note`), built from the same decode as the screen's rows, so `wifi
list | sort reverse dbm`, `| where dbm>-60`, `| max dbm` and `| to json` work. The record model had no
negative number, so it gained one (`Value::Signed`, `docs/records.md`). `match` on a record stream points
at `where`, as it does for `dir`. QEMU, three canned networks at -41, -80 and -9: `sort reverse dbm`
ordered -9, -41, -80; `where dbm>-60` kept two; `max` -9, `min` -80, `avg` -43.

## 59. One station, three radios: the shared half moves to `sdk/wifi` (2026-10-02)

**Why now.** A second radio is on the bench - the VisionFive 2 Lite's AIC8800D80 (section 44) - and a third
is identified, the Pi 2's RTL8188CUS. Read against each other (`build/vf2wifi/`, the AIC8800 vendor driver
and the JH7110 sources), they differ in the bus, the firmware upload and the firmware's language, and in
nothing above that: every one has the HOST run the WPA2 handshake, keep the keys, and answer the same
`wifi` and `nic-driver` requests. The one real split is full-MAC (Broadcom, AIC: the chip scans and
associates) against soft-MAC (Realtek: the host builds the 802.11 frames). The AIC sits nearer the
soft-MAC than expected - its scan results and received data are raw 802.11 frames - so beacon parsing and
the 802.11-to-Ethernet conversion are shared work too.

**The shape.** `sdk/wifi` (`godspeed-wifi`), a `no_std` library with no `unsafe`, holds what every radio
shares. Below it, a `Station` trait - planned as scan, connect, add a key, open the control port, disconnect, link,
frames, power, recover - that each full-MAC chip implements and that a shared host-side MLME will
implement over a soft-MAC `RawRadio`; and an `SdioHost` trait under the two SDIO chips, so the Broadcom
code runs on any SDIO host and the AIC gets a DesignWare one. Not in the SDK: that is the operating
system's interface and its audited `unsafe`, and 802.11 is neither (`sdk/wifi/CLAUDE.md`).

(As built in step 2b-i the trait has no add-key, control-port or recover methods: keys and the port are
inside `join`, and recovery stays in the serve loop with the SDIO host and the power pin. The 2b-i paragraph
below lists it.)

**Step 1, this change.** `crypto`, `eapol` and `keyfile` moved as they were (their history followed), and
the wire protocol became one module, `wire`, that the driver's `scan::reply` and the shell's `wifi_wire`
both re-export - the shell had kept a hand mirror of the driver's constants. No behaviour changed. Three
gates listed their source directories by hand and missed the new crate (two symbol checkers and the
`unsafe` deny rule); they cover it now.

**Firmware, decided the same day by the operator: ship it, as the Broadcom blobs are shipped (section 8).**
For the Realtek that is the same footing: its firmware is in upstream `linux-firmware` under Realtek's
redistribution licence. For the AIC8800 it is not, and that is recorded rather than smoothed over: no
licence from AICSemi itself was found, only a packager's blanket claim (Radxa's `debian/copyright`) and a
distribution's `freedist` label. And only one of Radxa's five copies matches what the board loads, so the
files will come from the board's own vendor image when that phase arrives, with their provenance beside them.

**Step 2a (the same day): the SDIO protocol is shared, and the host is a trait.** `sdk/wifi/src/sdio.rs`
holds CMD52, CMD53, identification and the CIS walk, and an `SdioHost` trait the protocol speaks to in SDIO
terms - a command index, a response type, whether to check CRC and index, a transfer's block size, count
and direction. The Pi 4's Arasan implements it by encoding those into its own `CMDTM` and `BLKSIZECNT`,
and the eleven words the driver used before are pinned at compile time, so the controller sees exactly
what it saw. What stayed in the driver is what is Broadcom's: the data function and its block size, and
the adopt-or-reset decision a respawn makes, which turns on the Broadcom firmware asserting function 2.
One failure line in the shared code had compared the controller's words against SDHCI values; it prints
the host's own words now. QEMU raspi4b: the driver's 27 log lines through identification and the power
commands are identical to the run before.

**Step 2b-i (the same day): the serve loop talks to a `Station`.** `sdk/wifi/src/station.rs` holds the trait
- start, step and abort a sweep; join; forget the keys; disassociate; the radio switch and its state; the
link; the station's address; frames in and out; the event names and the debug account - and the types it
speaks in, with `bss.rs` (the network list, its security classification and the `wifi list` records) and
`rxq.rs` (the received-frame queue). The driver's new `bcm.rs` is the Broadcom implementation: each method
is the call the loop used to make directly, with the bus, the backplane window, the firmware session, the
join's keys and the frame buffer held there instead of threaded through the loop. The loop is otherwise
unchanged, every log line included; it moves to the shared crate in 2b-ii, with the handshake runner.
QEMU cannot reach the loop - it has no radio - so this one is the card's to prove.

## 60. A rejoin through a different access point is a different link (2026-10-02)

**The symptom.** After `wifi radio powercycle` the radio rejoined, `wifi status` said joined with a strong
signal, `net` said `ping ok` and `lease ok` - and every ping timed out, with `net-stack` reporting `0 frames
seen`. `net renew` fixed it at once. It was not reproducible on demand, and the reason is the finding.

**The cause, measured across three sessions.** The network is a mesh, and each access point and band serves
its own subnet. The radio's own address was the same throughout:

| joined through | lease | gateway answered ARP as |
|---|---|---|
| main node, 5 GHz | 192.168.11.23 from 192.168.11.1 | the main node's 5 GHz address |
| main node, 2.4 GHz | 192.168.10.21 from 192.168.10.1 | the main node's 2.4 GHz address |
| satellite, 5 GHz | 192.168.11.29 from 192.168.11.1 | the satellite's address |

A rejoin to the same access point kept working, always. A rejoin to a different one (once from the
satellite to the main node, once from 2.4 GHz to 5 GHz on the main node after `chaos`) left `net-stack`
holding the old lease and the old gateway's hardware address, so every frame went to a gateway that was
not on the link. Which access point the firmware picks is not ours to choose, which is why it would not
reproduce when asked.

**Why `net-stack` missed it.** It already re-configures when the link changes, and it decides that the
link changed by our OWN address changing - which is right for the cable stepping in for the radio (section
41) and cannot see this: the radio keeps its address wherever it joins. CLAUDE.md 14.3 says a client must
re-derive everything that hung off the old instance, and the old instance here was an access point.

**The fix, as one fact carried three hops.** `wifi-driver`'s INFO reply (`0x10`) gains the access point
the join reached, asked of the firmware once per join. `nic-driver`'s genet backend passes it on as a new
one-byte query, op 10, rather than growing STATUS: the shell tells the boards' STATUS replies apart by
length, and a Pi 4 reply grown to fifteen bytes would have read as another board's counters. `net-stack`
asks op 10 only after STATUS has said the radio carries the link, which only this backend says, so no
other board's driver ever receives it. When the access point differs from the one last seen, `net-stack`
re-runs DHCP and ARP and says so: `the radio rejoined through a different access point (was -> now)`. A
rejoin to the same access point changes nothing, as a cable put back changes nothing. An access point
the radio does not know (zeros) is no evidence either way.

**What it costs.** One more exchange with `nic-driver`, and through it one with `wifi-driver`, on the
address check `net-stack` already makes every two seconds of network use - only while the radio carries
the link. The firmware is asked once per join, not per check.

**Verified on the Pi 4, the same day.** With the cable out, a power cycle moved the radio from the main
node's 2.4 GHz access point to its 5 GHz one. Six seconds later `net-stack` printed the rejoin line, took
192.168.11.23 in place of 192.168.10.21, and ping answered 17 of 17 with no `net renew`. The cable
stepping in and out around it re-configured on our own address changing, as before.

## 61. An idle link is dropped for inactivity, not for a failed rekey (2026-10-04)

**What the Pi 4 soaks showed.** Twice, an idle joined link - nothing sent after the join - was
disassociated by the access point about six minutes after the join, with 802.11 reason 4: inactivity.
The firmware's power-save mode read 0 at the time (`report_power_mode` in `ctrl.rs`), so the chip was
constantly awake; the radio was not asleep through the access point's traffic. With one echo a minute to
the gateway, the same link stayed up past fifteen minutes, 14 of 14 echoes answered.

**What a reader should take from it.** The first drop to expect on an idle link is the access point's
inactivity timer, not a failed rekey. A keep-alive is the fix, and it belongs in `net-stack`, which knows
whether the link is in use, not in the driver; it is not built. The rekey itself (section 42) is still
waiting for a hardware sighting, `backlog/64`.
