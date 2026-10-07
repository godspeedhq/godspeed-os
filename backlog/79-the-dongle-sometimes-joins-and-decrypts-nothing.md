# 79. The USB WiFi dongle sometimes joins, transmits, and decrypts nothing it receives

**Status: CLOSED 2026-10-07 - fixed by laying out the key store as rtlwifi does (`docs/wifi-usb.md` 43); about nineteen fresh joins on the Pi 2 since, none refused (45, 46). Found the same day on the T630 (39) and the Pi 2 (42).**

## What is seen

A fresh `wifi-usb` instance joins - authentication, association, the four-way handshake with message 3's
MIC verified, the pairwise key into CAM entry 0 and the group key into entry 1 - and transmits (`the FIRST
data frame sent through the link ... encrypted by the chip`). Every frame back is refused:

```
wifi-usb: a data frame from the access point the chip did not decrypt (protected true, security 0, swdec true) - not taken
```

`security 0` in the receive descriptor is the chip finding no key for the frame. `net` says the radio
carries the link; `ping` answers nothing (`0 frames seen`).

## Where, and how often

| Session | Host | Fresh joins | Refused |
|---|---|---|---|
| T630, `docs/wifi-usb.md` 39 | `xhci` | 4 (plug + `wifi join`; `off hard`/`on`; `powercycle`; replug) | 3 - all but the first |
| T630, 40 (instrument) | `xhci` | 4 rejoins from the key file | 0 |
| T630, 40 (`net` line card) | `xhci` | 3 | 0 |
| Pi 2, 42 | `dwc2` | 4 (boot; `powercycle`; replug; `powercycle` + `wifi join`) | 1 - the last |

About four in fifteen. The same instance's `wifi radio off` / `on` rejoin has never failed.

## What is RULED OUT

- **The USB host:** seen behind `xhci` and behind `dwc2`.
- **The key file:** a hand-made `wifi join` failed too (Pi 2).
- **The power-down:** a plain replug failed too (T630, 39).
- **The security enables:** `REG_CR` bit 9 and `REG_SECURITY_CFG` read back as written (`0x02ff`, `0xcf`)
  after the join AND on the first refused frame, in failing and working joins alike.
- **The handshake:** message 3's MIC verifies, so both ends hold the same pairwise key.

## Not yet known

- **The CAM itself.** The read-back of the CAM entries (`docs/wifi-usb.md` 40) returned the same word for
  two different entries and was removed: the read sequence used was this driver's guess, because the code
  that drives the CAM read in Linux (rtlwifi's CAM code) is not in `build/rtl`.
- What a failing fresh instance does differently from a working one. The four failures have no common
  command before them that the working joins lack.
- Whether a second `wifi join` in a failing instance recovers it - not yet tried.

## The next concrete step

1. Fetch rtlwifi's CAM code (`drivers/net/wireless/realtek/rtlwifi/cam.c`) into `build/rtl` and read the
   CAM read sequence there; then read back CAM entries 0 and 1 (control word and address only, never the
   key) and prove the read on a working join first - two entries written differently must read
   differently.
2. On a failing join: the same reading, then `wifi join` again in the same instance, and say whether it
   decrypts.
3. Only then a fix, from what the reading shows.

**Workaround:** none established. `wifi radio powercycle` brings up a new instance, which usually works;
that is an observation over fifteen joins, not a fix.

## Post-mortem (2026-10-07)

**The cause was where the keys sat, not whether they were written.** This driver had followed rtl8xxxu:
every key in the first free CAM entry - pairwise in 0, group in 1 at the BSSID with a group flag - and
`SECURITY_CFG` 0xcf, which turns the default keys on for unicast as well as broadcast. rtlwifi, Realtek's
own driver for the family, puts a group key in the entry its key id names, at the broadcast address, the
pairwise key in entry 4 at the peer, and sets 0xcc, default keys for broadcast only. This access point's
group key id is 2.

**What the evidence showed:** after the change, four fresh joins on the first card and about fifteen on
the operator's physical-chaos card - powercycles, pulls mid-download, replugs on both hub ports - every one
logged entries 4 and 2 and 0xcc, and not one frame was refused, to us or to a group. Before it, about four
joins in fifteen refused everything and every working join refused at least one group frame.

**What is NOT shown, said plainly:** why the old layout failed only sometimes for unicast. Group frames
missing entry 2 is certain from the layouts; the all-frames failure was intermittent under the same
layout and its mechanism was never observed, because the CAM cannot be read back (rtlwifi has no read
either, and this driver's attempt read the same word for different entries). The fix is the vendor's
layout and the evidence is the joins after it. If a refusal ever returns, the receive path now says
whether it was to us or to a group, which is the first question.

**What it cost to find:** a CAM read-back that was not reading the CAM (removed rather than left logging
a non-fact), and two wrong narrowings - `xhci`, then the key file - each refuted by the next card.
