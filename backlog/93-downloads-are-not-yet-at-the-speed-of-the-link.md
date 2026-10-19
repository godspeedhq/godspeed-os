# 93. Downloads are not yet at the speed of the link: what is left on the radios, the RTL8168 and `download`

**Status: OPEN, 2026-10-19.** Recorded at the operator's request at the end of the download-speed work on
`feat/packages` (`8d38fa61` to `d4ed712c`), to come back to. Everything below was seen on a board or in QEMU
that day; nothing here is fixed.

## Where it stands

The work that was done: a 256 KiB receive window with window scaling, out-of-order data kept in the ring,
delayed ACKs, burst polling, NIC receive rings of 24, the AIC8800 read whole, the RTL8168's receive ring no
longer emptied on every send, an orphaned fin-wait-2 bounded at 2 s, and the `download` utility
(`utilities/65_download.md`, `docs/tcp-design.md` "Downloads at the speed of the link").

| Board | Link | Measured |
|-------|------|----------|
| HP T630 | RTL8168, cable | 10 MB in 15 s, **629 KiB/s** |
| Dell Wyse 5070 | RTL8168, cable | 10 MB in 55 s, **176 KiB/s** |
| QEMU | e1000 | fetch 872 KiB/s; `download` with the disk writes about 300 KiB/s |
| Pi 4 | CYW43455, WiFi | a 76.8 KiB package at about 13 KiB/s, on the small window |
| VisionFive 2 Lite | AIC8800, WiFi | about 8 KiB/s |

## What is left

1. **The RTL8168's ring overflows during a download.** It is 24 descriptors, all a 64 KiB DMA arena holds
   (`services/nic-driver/src/main.rs`, `RX_RING_COUNT`, `TALLY_OFF`). The chip fills it, latches RDU, and
   drops what arrives until the driver restarts it once every frame is taken: about 130 restarts in a 10 MB
   download on the Wyse, about 64 per 12,000 frames on the T630. Each stop is a loss the sender recovers
   from. A larger arena and ring is the obvious step. **Why the Wyse is 3.6 times slower than the T630 on the
   same chip and image is not measured.**
2. **The Pi 4's radio loses superframes over 4 KiB.** `wifi-driver` reads the CYW43455 into a 4096-byte
   buffer, and a large window's bursts arrive as superframes of 4.2 to 8 KB, which are dropped and logged
   (`exceeds this driver's 4096 byte buffer`). Until the 2026-10-19 fix that also killed the radio; now a
   connection over a radio offers the old 8 KiB window (`Conn::small_window`, `a496e4de`), which survives
   but still sees them (15 in one `selfcheck packages`). The fix is to read the Broadcom frames into a
   region (as the AIC8800 now reads its queue, `receive_region`), then lift `small_window`.
3. **The VisionFive's radio is slow on transmit.** About 8 KiB/s; the latency is in sending over the AIC8800,
   not in receiving. Not investigated further.
4. **Every connect waits about 1.8 s on an ARP for an address that is not on the link.** Before going via the
   gateway, net-stack asks ARP for the remote server's address and waits for an answer that cannot come
   (`ARP for 104.21.38.128 found nothing`, then `connect ... - via the gateway`; on the Pi 4 the request
   reached dispatch at 09:08:04.334 and connected at 09:08:06.122). It is about 2 s on every `godspeed`
   command and every `download`, on every board. An off-link address should go to the gateway at once.
5. **`download` has no resume and no retry.** A stall ends it, and unplugging the cable mid-download ends it
   (the address changes). Planned: retry on a stall, then resume with an HTTP `Range` request into the
   `.partial` it already keeps.
6. **Writing to the disk halves a download in QEMU** (fetch 872 KiB/s, `download` about 300 KiB/s). Not
   measured on a board.
7. **The send path, and `upload`.** Agreed order: `download` first, then make the send path fast (`SND_BUF`
   is 2 KiB), then an `upload` utility, for an upload speed figure (reported, not passed or failed) and
   sending files to a server.
8. **Two connections at most** (`MAX_CONNS`). Four overflowed net-stack's stack; raising it needs the frames
   measured first.
9. **The Pis and the VisionFive have not run `d4ed712c`.** Two of the last changes run on every board and
   were proved only on the two PCs: the progress line fitted to the console (`6e50b1f5`) and the fin-wait-2
   bound (`d4ed712c`). Their last cards passed on `a496e4de`.

## Where to start

Item 4 is small and helps every board. Item 1 is the cable's ceiling on the PCs, item 2 the Pi 4's, and each
wants one card to show it.
