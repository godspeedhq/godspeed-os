# sdk/wifi/

The chip-independent half of every WiFi driver, as a library: `godspeed-wifi` (`godspeed_wifi`).
`no_std`, `#![deny(unsafe_code)]`, depending only on `sdk/rust`.

| Module | What it is |
|---|---|
| `station.rs` | The `Station` trait - what the serve loop asks of a radio (scan, join, keys, up/down, link, frames, debug) - and the types it speaks in: `ScanStep`, `Outcome`, `Secret`, `Link`, `Pulled` |
| `serve.rs` | THE serve loop: what the shell's `wifi` and `nic-driver`'s frame ops are answered by - the sweep as a state, the scan cache, the credential table and `/wifi.keys`, auto-join, every reply layout - run by every radio service over its `Station`, with a `Host` for what is around the radio (power, and notices with no reply cap). Moved here from `wifi-driver` for the USB dongle's service (`docs/wifi-usb.md` 10) |
| `bss.rs` | A scan's networks: `Network`, `Scan`, the beacon security classification, and the `wifi list` record encoding |
| `mgmt.rs` | A beacon or probe response read from the raw 802.11 frame: BSSID, capability, SSID and the DS-parameter channel, every length from the air distrusted; and the frames a host-built radio sends - the probe request (R5a), the authentication, association request and deauthentication (R5b) - with the two answers read and a network's RSN element checked for CCMP. For every radio that forwards frames (the RTL8188CUS; the AIC8800 still reads its own, recorded debt in `docs/wifi-usb.md` 9). Pure, host-tested by `scripts/host_test_check.py` |
| `data.rs` | An 802.11 data frame and the ethernet frame it carries, both ways: `llc_payload` and `to_ethernet` (moved from the AIC8800's `aic_wire.rs`) and `to_80211` (a station's frame to its access point). Pure, host-tested |
| `rxq.rs` | The bounded queue of received frames between a radio and `nic-driver` |
| `crypto.rs` | SHA-1, HMAC, PBKDF2, the 802.11 PRF, AES-128, the RFC 3394 unwrap - each checked against its published vector at start (`selftest`) |
| `eapol.rs` | EAPOL-Key frames: recognised, the PTK derived, our messages built and signed, the AP's verified, the group key found |
| `supplicant.rs` | The WPA2 four-way handshake (`Handshake`) and the group-key rekey (`group_rekey`), run by the HOST on every radio over the `KeyPath` each radio supplies (how an EAPOL frame is sent, how a key is installed) |
| `keyfile.rs` | The credentials a join earned, in `/wifi.keys` - merged with the file on save, since the onboard radio and the dongle both keep it - and the operator's choice of radio, `/wifi.radio`, which the shell writes and the radios read |
| `sdio.rs` | The SDIO card protocol (CMD52, CMD53, identification, the CIS) and the `SdioHost` trait every SDIO controller implements - the Pi 4's Arasan (`host.rs`) and the VisionFive 2's DesignWare (`dwmmc.rs`: commands since V1, the PIO data phase since V2) |
| `wire.rs` | The request/reply vocabulary between a radio driver and the shell's `wifi`. ONE definition, read by both sides. `OP_HARDWARE` (12) answers what a radio is - chip and bus, from the `Host` - for `wifi hardware`; `OP_HARDWARE_DETAIL` (13) one radio in full, as labelled facts (`DETAIL_LABELS`) from the `Host` and the `Station`, for `wifi hardware <radio>`; `OP_USE` (14) whether this radio is the one the operator chose, asked or told (`USE_*`), for `wifi hardware use`, which `OP_NET_INFO`'s reply also carries for `nic-driver`. The frame ops `nic-driver` uses (`OP_NET_*`, 0x10-0x12) are here since the serve loop moved; `nic-driver` does not link this crate and writes the three numbers as literals (`services/nic-driver/src/radio.rs`) |
| `usbfn.rs` | What a USB host service answers for the one device it has bound as a radio - `OP_INFO`, `OP_CONTROL`, `OP_CONTROL_ONCE` (a transfer the host must not retry: a firmware block), `OP_BULK_IN` (the received transfer the host holds) and `OP_BULK_OUT` (a frame to send, on the OUT endpoint named by its position), and `OP_SYNC` (never answered: names a notice taken in place of an answer) - and what `wifi-usb` asks; and `NOTE_RADIO` and `NOTE_BULK_IN`, which the host sends the driver when the binding changes or a transfer is held, so the driver blocks instead of polling (`docs/wifi-usb.md`). One definition for `dwc2`, `xhci` (control transfers since U2a, bulk since U2b and U2c) and the driver |

## Why a crate, and why not in the SDK

Three radios, one shape above the firmware: the Pi 4's Broadcom CYW43455 (SDIO, full-MAC), the
VisionFive 2 Lite's AICSemi AIC8800D80 (SDIO, full-MAC) and the Pi 2's Realtek RTL8188CUS (USB,
soft-MAC). The bus, the firmware upload and the firmware's language differ; the station does not. This
crate is the part that does not.

`sdk/rust` is the operating system's interface and its audited `unsafe` layer, linked by every service;
802.11 is not that. And it is a library rather than one service with backends because the radios will not
all live in one service - a USB dongle's driver is its own service, `wifi-usb`, reaching the dongle through
whichever USB host bound it (`docs/wifi-usb.md`).

## The plan it is step 1 of (`docs/wifi.md` 59)

1. The chip-independent code moves here (done).
2. a. `SdioHost`, and the SDIO protocol moves here (done). b-i. `Station`: the serve loop talks to a `Station`, the Broadcom code is the first (done). b-ii. The WPA2 handshake runner moves here (done, `supplicant.rs`). b-iii. The serve loop itself moves here (done 2026-10-06, `serve.rs`), and `wifi-driver`'s `serve_radio` is a wrapper that calls it.
3. The VisionFive: a DesignWare `SdioHost` and an AIC8800 `Station`. Started: the `SdioHost` is `services/wifi-driver/src/dwmmc.rs` (identification over the CMD line since V1; the PIO data phase since V2, `docs/wifi-aic8800.md`), and the chip's message bus and firmware upload are `services/wifi-driver/src/aic.rs` (the firmware is uploaded and started). Its bring-up, scan, join and `Station` (`aic_station.rs`) are hardware-verified, with DHCP and `ping` over the radio (2026-10-05).
4. A USB dongle - the Pi 2's Realtek first, then any board with a USB host: the `wifi-usb` service (`docs/wifi-usb.md`; reaching the chip through the host is hardware-verified on the Pi 2), which is a `Station` under the shared loop since R4 (`services/wifi-usb/src/station.rs`), sweeping with probe requests since R5a; the host-side MLME (authentication, association) and the WPA2 join are R5b-R5c, hardware-verified on the Pi 2.

Anything added here must be true of every radio. A name, a structure or a constant from one vendor's
firmware belongs in that vendor's driver.
