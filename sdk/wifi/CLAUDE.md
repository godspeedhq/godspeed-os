# sdk/wifi/

The chip-independent half of every WiFi driver, as a library: `godspeed-wifi` (`godspeed_wifi`).
`no_std`, `#![deny(unsafe_code)]`, depending only on `sdk/rust`.

| Module | What it is |
|---|---|
| `station.rs` | The `Station` trait - what the serve loop asks of a radio (scan, join, keys, up/down, link, frames, debug) - and the types it speaks in: `ScanStep`, `Outcome`, `Secret`, `Link`, `Pulled` |
| `bss.rs` | A scan's networks: `Network`, `Scan`, the beacon security classification, and the `wifi list` record encoding |
| `rxq.rs` | The bounded queue of received frames between a radio and `nic-driver` |
| `crypto.rs` | SHA-1, HMAC, PBKDF2, the 802.11 PRF, AES-128, the RFC 3394 unwrap - each checked against its published vector at start (`selftest`) |
| `eapol.rs` | EAPOL-Key frames: recognised, the PTK derived, our messages built and signed, the AP's verified, the group key found |
| `supplicant.rs` | The WPA2 four-way handshake (`Handshake`) and the group-key rekey (`group_rekey`), run by the HOST on every radio over the `KeyPath` each radio supplies (how an EAPOL frame is sent, how a key is installed) |
| `keyfile.rs` | The credentials a join earned, in `/wifi.keys` |
| `sdio.rs` | The SDIO card protocol (CMD52, CMD53, identification, the CIS) and the `SdioHost` trait every SDIO controller implements - the Pi 4's Arasan (`host.rs`) and the VisionFive 2's DesignWare (`dwmmc.rs`: commands since V1, the PIO data phase since V2) |
| `wire.rs` | The request/reply vocabulary between a radio driver and the shell's `wifi`. ONE definition, read by both sides. The frame ops `nic-driver` uses (0x10-0x12) are not here: they are the Broadcom driver's own, in `services/wifi-driver/src/frames.rs`, and `nic-driver` does not link this crate |

## Why a crate, and why not in the SDK

Three radios, one shape above the firmware: the Pi 4's Broadcom CYW43455 (SDIO, full-MAC), the
VisionFive 2 Lite's AICSemi AIC8800D80 (SDIO, full-MAC) and the Pi 2's Realtek RTL8188CUS (USB,
soft-MAC). The bus, the firmware upload and the firmware's language differ; the station does not. This
crate is the part that does not.

`sdk/rust` is the operating system's interface and its audited `unsafe` layer, linked by every service;
802.11 is not that. And it is a library rather than one service with backends because the radios will not
all live in one service - a USB radio's driver sits behind the USB host service on the Pi 2.

## The plan it is step 1 of (`docs/wifi.md` 59)

1. The chip-independent code moves here (done).
2. a. `SdioHost`, and the SDIO protocol moves here (done). b-i. `Station`: the serve loop talks to a `Station`, the Broadcom code is the first (done). b-ii. The WPA2 handshake runner moves here (done, `supplicant.rs`); the loop itself is still `services/wifi-driver`'s `serve_radio`, which now takes any `Station`.
3. The VisionFive: a DesignWare `SdioHost` and an AIC8800 `Station`. Started: the `SdioHost` is `services/wifi-driver/src/dwmmc.rs` (identification over the CMD line since V1; the PIO data phase since V2, `docs/wifi-aic8800.md`), and the chip's message bus and firmware upload are `services/wifi-driver/src/aic.rs` (the firmware is uploaded and started; talking to it is V3). The `Station` is not begun.
4. The Pi 2: a `RawRadio` for the Realtek dongle and a shared host-side MLME that makes it a `Station`.

Anything added here must be true of every radio. A name, a structure or a constant from one vendor's
firmware belongs in that vendor's driver.
