<!-- SPDX-License-Identifier: GPL-2.0-only -->
# WiFi: two problems, one of which is not a driver

**Status:** SPEC, nothing built. Written on `feat/wifi-driver` before any code, deliberately, because
the sizing conclusion below would have been discovered four weeks late otherwise.

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
| **VisionFive 2 Lite** | **Believed NONE** - see below | (n/a) | (n/a) | (n/a) |
| **Dell Wyse 5070** | None | - | - | - |
| **HP T630** | None | - | - | - |

**The Pi 2's dongle was already identified, by this project, on this hardware.**
`bugs/3_DWC2_SPLIT_XACTERR_LOWSPEED_KBD.md` records the hub enumeration: port 1 is the `smsc95xx`
ethernet at `0424:ec00`, and **port 5 is a Realtek WiFi dongle at `0bda:8176`**. It enumerated all
along, as a high-speed device needing no SPLIT transaction. So there is no unknown here and nothing to
plug in and look up: the chip is an RTL8188CUS (the RTL8192CU family).

**The VisionFive 2 is the open question, and it is the one fact I will not assert.** Nothing in this
repository claims it has WiFi. What the repository *does* record for that board is 2x gigabit ethernet
(DWMAC, zero packet loss), an SD card it boots from, USB through an onboard hub, and HDMI. The JH7110
carries an M.2 M-key slot intended for NVMe, and a PCIe WiFi card in it would be a different project
from either of the two above. **Settle this by booting the board** rather than by reading a spec sheet:
`hw-enumerator` already walks PCI on riscv64, and the device tree names any SDIO WiFi node. Until then
this document plans for two radios, not three.

**Neither x86 box has WiFi, which is a convenience.** It means the QEMU-first development pattern that
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
- **`keyring`** - the service that owns the credential.
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
active link at a time**, chosen explicitly - `wifi connect` means "make the radio the link". Multi-homing
is out of scope with that as the reason, rather than unmentioned.

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

---

## 6. Where the credential lives, which is the interesting Godspeed question

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
| **5** | The frame interface: ops `0x10`/`0x11`/`0x12`. **`ping` over WiFi, `net-stack` unmodified** | Nothing above the driver |
| **6** | The Pi 2 dongle, soft-MAC | A real 802.11 MAC and real crypto. **Deferred, with section 3 as the reason** |

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

---

## 9. Non-goals, stated so scope cannot creep (§26.2, §13.3)

**In:** WPA2-PSK, one band, one SSID, one station link, scanning, DHCP through the existing stack.

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
   nothing else in this plan.
2. **Firmware blob: in the repo, or supplied by the user?** A licensing call, not a technical one.
3. **Is the Pi 4 radio behind the Arasan controller, with the SD card on `emmc2`?** Section 4's whole
   argument rests on it. Confirm against the device tree before writing code.
4. **Does the passphrase persist across reboots**, and if so where and under what capability? "Retype it
   each boot" is a legitimate phase-4 answer and avoids the question entirely.
5. **One service or two** - `wifi` alone, or `wifi` plus `supplicant`? Section 6 argues two for the
   interface even though the full-MAC shortcut sends the secret through the driver anyway.
6. **Is `ping` over WiFi on the Pi 4 the finish line for v1 of this work?** Naming the finish line now is
   what stopped the networking effort sprawling, and phases 0-5 are already a substantial body of work.

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
- **`wifi connect Some hunter2` being ACCEPTED.** It must refuse, by name, with the reason. If any board
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
| `kernel/src/arch/aarch64/mod.rs` | `map_fixed_driver_mmio` gains one arm - `"wifi-driver" => (0xFE30_0000, 1)`, gated on the census having seen the controller answer - and `emmc_base_clock_hz` returns the clock instead of a flat 0 |
| `services/wifi-driver/` | the service: `host.rs` (the SDHCI host controller, reset/clock/`cmd`) and `sdio.rs` (CMD0, CMD5 twice, CMD3, CMD7, CMD52, the CIS walk, function enable) |
| registration | workspace member, `aarch64_built`, the supervisor's embed list and `has_wifi_driver` cfg, its `IMAGES` row, `MANAGED`, the boot spawn, the death-notification arm, and the kernel's two restart lists |

### Why the pin mux and the clock are in the kernel

Both are BOARD facts, and a driver service is granted its own controller's registers and nothing else
(§12.3) - so it cannot route the pins that connect it to the part it drives, and it cannot ask the
VideoCore anything. arm32 makes exactly this argument at `sd_route_to_emmc`, one SoC generation earlier.
The clock matters more than it looks: `emmc_base_clock_hz` returned 0 on this port, and 0 is a REFUSAL
rather than a default, so without it the driver would correctly decline to set any card clock at all.

### The authority, stated plainly

One page of MMIO, granted by name and only where the census saw the controller answer. **No DMA arena,
no interrupt, and no send peers** - not even `events`, which every other driver here declares. Each
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
the gate rejected. A third pair inside `sdio.rs` that the cross-file rule could not see (`READY_TRIES` at
two values, 100 and 500) was found while fixing the first two and split into `OCR_TRIES` and `READY_TRIES`.

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
