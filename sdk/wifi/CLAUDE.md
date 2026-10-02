# sdk/wifi/

The chip-independent half of every WiFi driver, as a library: `godspeed-wifi` (`godspeed_wifi`).
`no_std`, `#![deny(unsafe_code)]`, depending only on `sdk/rust`.

| Module | What it is |
|---|---|
| `crypto.rs` | SHA-1, HMAC, PBKDF2, the 802.11 PRF, AES-128, the RFC 3394 unwrap - each checked against its published vector at start (`selftest`) |
| `eapol.rs` | The WPA2 four-way handshake and the group-key rekey, run by the HOST on every radio |
| `keyfile.rs` | The credentials a join earned, in `/wifi.keys` |
| `wire.rs` | The request/reply vocabulary between a radio driver and its clients - the shell's `wifi` and `nic-driver`. ONE definition, read by both sides |

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
2. `Station` and `SdioHost` traits: the serve loop talks to a `Station`, the Broadcom code becomes one.
3. The VisionFive: a DesignWare `SdioHost` and an AIC8800 `Station`.
4. The Pi 2: a `RawRadio` for the Realtek dongle and a shared host-side MLME that makes it a `Station`.

Anything added here must be true of every radio. A name, a structure or a constant from one vendor's
firmware belongs in that vendor's driver.
