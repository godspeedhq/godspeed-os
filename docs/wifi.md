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

**And above BOTH of them, nothing changes.** `docs/networking.md` §5 states the contract: *"Raw frames
only... The frame interface is the entire contract."* The ops are `0x10` INFO, `0x11` TX, `0x12` RX,
and they are already proven agnostic across four drivers on four ISAs - e1000, RTL8168, LAN9514 and
GENET - with `dwc2` serving them alongside the block protocol on one endpoint. A WiFi service that
presents those three ops binds to `net-stack` with **zero** changes above it: DHCP, ARP, ICMP, DNS and
TCP all work the moment a frame moves. Protecting that property is the single most important design
constraint in this document.

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

**The command surface is `docs/wifi-commands.md`**, which settles the shape this implies: `connect` must
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

## 8. The firmware blob, which is genuinely new for this project

The CYW43455 needs `brcmfmac43455-sdio.bin` (roughly 600 KB), a CLM blob carrying regulatory data, and
a board-specific NVRAM text file. **This project has never shipped a binary blob**, and the question is
not technical:

- **Licensing.** The OS is GPL-2.0-only. Broadcom/Cypress firmware is redistributable under its own
  permissive-but-not-GPL terms. Whether it belongs in this repository at all is a licensing decision
  for the owner, not a design choice, and `docs/licensing.md` is where the answer belongs.
- **If embedded in the image**, it follows the path a service ELF takes and the driver has no
  filesystem dependency. It grows the image by ~600 KB and puts a non-GPL artefact in git history,
  permanently.
- **If loaded from the filesystem**, the repository stays clean and the user supplies the file - but
  then **WiFi depends on `fs`, which on the Pi 4 depends on a USB stick being plugged in.** That is a
  dependency with teeth, and Commandment VIII governs it: the driver waits on `fs`'s reply or on the
  loud fact of its absence, never on a timer, and reports "firmware unavailable" rather than hanging.
  The rule above the rules applies - no missing dependency may wedge the machine.

There is a third option worth weighing: **load it over the network**, which is absurd for a network
driver on a machine with no other link, and merely awkward on the Pi 4, which has ethernet. Recorded
only so nobody proposes it as though it were new.

**Restartability has a cost here that is worth naming now.** §6.2 requires a driver's death to be a
supervisor restart. A WiFi driver's restart means re-uploading 600 KB over SDIO and re-associating,
which is on the order of a second, during which the link is down and `net-stack` sees a dead peer. That
is acceptable - it is exactly the `EndpointDead`, reacquire-by-name, retry path (§14.3) - but it must be
measured rather than assumed, and `chaos max-carnage` will find out.

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
