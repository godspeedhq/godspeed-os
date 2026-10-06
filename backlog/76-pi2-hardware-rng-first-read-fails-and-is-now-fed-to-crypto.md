# 76. The Pi 2's hardware RNG fails its first read after boot, and its output is now fed to crypto

**Status: OPEN - found 2026-10-06 on the USB WiFi dongle's R5c card (`docs/wifi-usb.md` 13). A kernel change,
so not made without the operator's go-ahead and a QEMU boot first.**

## What was seen

The dongle's first WPA2 join after boot sent message 2 of the handshake 650 ms after message 1 arrived,
and the log between them says why:

```
11:37:32.382  wifi-usb:   EAPOL-Key from <the access point> - message 1 of 4 (ANonce) ...
11:37:33.028  wifi-usb: NO HARDWARE RNG on this board - the SNonce is hashed from the cycle counter, the
              AP's nonce and our address (weaker than the standard intends; recorded, not hidden)
11:37:33.028  wifi-usb:   message 2 of 4 sent ...
```

The access point resent message 1 in the gap, and the join still completed. The second join, a minute
later, had NO such line and sent message 2 in 31 ms: the same call answered at once.

## Why, from the code

`kernel/src/arch/arm/mod.rs`, `hw_random`:

- The FIRST call enables the BCM2835 RNG with a warm-up count of `0x40000` (`RNG_STATUS`), then waits for
  a word to be available.
- The wait is bounded by an ITERATION count, `n > 2_000_000` register reads, which took about 650 ms on
  this board. That is shorter than the warm-up, so the first call returns `None`; every later one finds
  words ready. "A count is not a duration" (CLAUDE.md 26.6) is the rule this bound breaks, and here it
  breaks in the direction that makes the first read always fail.

And its comment says: "Best-effort under concurrent callers (an unlocked FIFO pop) - fine for a diagnostic,
not fed to crypto." Since the supplicant became every radio's (`godspeed_wifi::supplicant::snonce`), it IS
fed to crypto: the SNonce of every WPA2 handshake on this board. The Pi 4's and the VisionFive's `hw_random`
were not checked for the same two properties here.

## What it costs today

The first handshake after boot uses the counter-hashed fallback nonce, which the supplicant logs as weaker
than the standard intends, and waits 650 ms, long enough for the access point to resend. Two callers at
once - a handshake, and the shell's `random` utility, the other consumer the comment names - could pop one
FIFO word between them (the comment's "unlocked").

## What would fix it, for whoever takes it

- Bound the wait by time (the warm-up's real length on this SoC), or warm the RNG up at boot so the first
  caller does not pay for it.
- Decide whether `hw_random` is a crypto source. If it is, the comment changes, and the concurrency note
  becomes a lock. If it is not, the supplicant needs a different source on this board.
