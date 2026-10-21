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
   (`services/nic-driver/src/main.rs`, `RX_RING_COUNT`, the `tally` part). The chip fills it, latches RDU, and
   drops what arrives until the driver restarts it once every frame is taken: about 130 restarts in a 10 MB
   download on the Wyse, about 64 per 12,000 frames on the T630. Each stop is a loss the sender recovers
   from. A larger arena and ring is the obvious step. **Why the Wyse is 3.6 times slower than the T630 on the
   same chip and image is not measured.**
   *(2026-10-21: the ring is 256 in a 132-page arena (`b162d007`). The T630 card on it saw no restart during a
   download and none since, only while the link is idle; downloads went from 0.16-0.72 to 1.5-2.0 MiB/s on
   that change alone. Why the Wyse is slower is measured now, in item 12: the same work, a slower trip
   between cores.)*
2. **The Pi 4's radio loses superframes over 4 KiB.** *(The region read is built on `feat/gs-driver`,
   `1bcfb9cb`: the CYW43455's data and scan paths read into a 64 KiB region. A Pi 4 card is owed, and
   `small_window` is not yet lifted. Item 10 names the paths that still read 4 KiB.)* `wifi-driver` reads the CYW43455 into a 4096-byte
   buffer, and a large window's bursts arrive as superframes of 4.2 to 8 KB, which are dropped and logged
   (`exceeds this driver's 4096 byte buffer`). Until the 2026-10-19 fix that also killed the radio; now a
   connection over a radio offers the old 8 KiB window (`Conn::small_window`, `a496e4de`), which survives
   but still sees them (15 in one `selfcheck packages`). The fix is to read the Broadcom frames into a
   region (as the AIC8800 now reads its queue, `receive_region`), then lift `small_window`.
3. **The VisionFive's radio is slow on transmit.** About 8 KiB/s; the latency is in sending over the AIC8800,
   not in receiving. Not investigated further.
4. **Every connect waits about 1.8 s on an ARP for an address that is not on the link.** *(Fixed on
   `feat/gs-driver`, `3ec99a27`: net-stack keeps the DHCP server's mask and sends an address off it to the
   gateway with no ARP. Seen on the T630 at `27201c83`: `connect 188.114.96.1:443 - off the subnet, via the
   gateway`. The other boards owe a card.)* Before going via the
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
   were proved only on the two PCs: the progress line fitted to the console (`6e50b1f5`, since replaced by the
   bar on its own line, `1e282e7b`) and the fin-wait-2
   bound (`d4ed712c`). Their last cards passed on `a496e4de`.

10. **Found by the docs and comments audit of 2026-10-19 (Audit 17), and fixed the same day** in the commit
   after it:
   - `net-stack` op 21 said `did not answer ARP` for an address off the subnet, where since `3ec99a27` no ARP
     is sent; it says `is off the subnet` now, as op 23 does.
   - `RxQueue::push` (`sdk/wifi/src/rxq.rs`) turned away a frame over `FRAME_MAX` without counting it; it
     counts in `too_big` now. The AIC8800's `data_dropped` counts a frame the full queue refused as well as
     `full` does, which is two reports of one loss, now said on the field.
   - The CYW43455's join read 4 KiB, so an association event or a handshake message in a larger superframe
     was lost; it reads into the 64 KiB region now, as the pull and the sweep do. **Not changed, and why:**
     the control waits (`query_raw`, `set_cmd`) still read 4 KiB, and the size costs them nothing: they skip
     every frame not on the control channel, a superframe included whatever its size, as the reference's
     control path drops glommed frames (`docs/wifi.md` 39). A data frame read during a control wait is lost
     either way, which is older than this and not a size question.
   - `wifi-driver`'s log line takes the region's size from `ctrl::RX_REGION` rather than by hand.
   - Built for the Pi 4 and the VisionFive; neither radio is in QEMU, so a Pi 4 join is owed a card.

11. **A download on the Wyse fails with TLS `DecryptFailed`, on `main` as well (2026-10-20).** The same
   10 MB download, the same RTL8168 and cable: `269c86f3` (`feat/gs-driver`) finished 5 of 6, the one
   failure about 30% in, the four after it clean. `main` at `bd0973bf` finished 3 of 9: the fourth failed at
   about 90%, and the next five each failed within a second of connecting, at 0% or a few hundred bytes, so
   on `main` a failure was followed by nothing but failures and on the branch it was not. Why is not
   known. An earlier bisect of the day (`82a0d73b` 2 of 5, `b9365a1d` 1 of 4, `4e0c733d` a stalled first
   download) is consistent with the bug being older than every commit it tested. The RTL8168's receiver
   restarts (item 1) run at the same rate in the downloads that pass as in those that fail, so they do
   not tell the two apart.
   **The suspect, and the change made for it:** `net-stack` did not check an inbound segment's IPv4 or
   TCP checksum, so a segment damaged before it reached `net-stack` was taken as data and acknowledged, and
   only TLS noticed. Since 2026-10-20 it drops such a segment, counts it (`Tcp::bad_checksum`) and logs
   the first eight and every 64th, and the sender's retransmission repairs it. **The prediction a Wyse
   card tests:** if damaged segments are the cause, the count rises during a download and `DecryptFailed`
   stops; if the count stays at 0 and `DecryptFailed` persists, they are not, and what a failure leaves
   behind for the next connection is the next question.
   **The first card, 2026-10-20 (`d830652d`, Wyse):** 6 of 6 downloads finished, and two real segments
   from the registry were dropped for a bad checksum, each in a download that then completed; none was
   dropped in QEMU. So damaged segments do reach `net-stack` on the Wyse, and the sender's retransmission
   repairs them. The first drop came 160 ms after five RTL8168 TX-timeout recoveries in a row, the second
   with nothing logged near it. Where the damage is made, the chip or the driver's receive path, is not
   known; that the Ethernet FCS would have caught damage on the wire points at the second. Six clean
   downloads against the earlier failure rates is evidence, not yet proof: a few more cards will say.
   `chaos max-carnage all-services 1000` and `selfcheck` (666 ran, 0 failed) on the same image, clean.
   **The T630, three more cards (2026-10-20).** `d830652d`: 5 of 6 downloads, the sixth stalled after six
   drops in eight seconds and 64 or more drops in the run; `chaos max-carnage all-services 1000` and
   `selfcheck` (603 ran, 0 failed) clean. `596c37b6` (says what a drop was): 7 of 7, three drops, every one
   a full-size segment with Don't Fragment set and its IP length matching the frame (1468 of 1472, the
   four being the FCS the chip leaves on), so **not fragmentation** - the bytes were damaged. `2b07c406`
   (compares a damaged segment with its intact resend): 5 of 5, two drops, one compared:
   `1349 of 1428 payload bytes differ, 5410 bits, offsets 74..1427 (frame at 54)`. The first 128 bytes of
   the frame were the right frame and everything after them was other data, which is not bit damage on a
   wire: it is the shape of a receive buffer read when only the first 128-byte DMA write of the frame had
   landed (128 bytes is a usual PCIe maximum payload), or of a buffer reused under the reader. The other
   drop came 0.5 s after three receiver restarts and a TX-timeout recovery in 0.7 s. Two suspects in
   `nic-driver`, neither shown: `rtl_restart_rx` turns the receiver off and re-arms every descriptor
   without waiting for a frame the chip is still writing, and nothing in the system sets or clears the
   RTL8168's PCIe Relaxed Ordering, which would let the descriptor's write overtake the data's. The
   restarts are logged only at the first eight and every 64th, so their timing against the drops is not
   yet known.
   **Answered on `6cda1d65` (T630, every restart logged):** all seven damaged segments came 0 to 76 ms after
   a receiver restart, six of them within 24 ms, against restarts about 2 a second - by chance about one in
   a million. One more shape showed: a segment whose IP length was 136 in a frame whose descriptor said
   1472, a length from another frame. **The first suspect, then:** `rtl_restart_rx` re-armed the ring while
   a frame was still landing. It now holds 1 ms between RX off and the re-arm, as the bring-up already
   holds `RTL_QUIESCE_MS`; a card owes the count of damaged segments falling to none.
   **It did not (`672ba255`, T630):** three damaged segments in six downloads, and a new shape, a 108-byte
   frame whose IP header said 1468. All three again came within 25 ms after a restart or a TX-timeout
   recovery, two of them the latter, while only 4% of download time lies that close to one (one per 476 ms
   during the downloads; the "one in a million" above was taken against the average over four minutes and is
   too strong, though four in a hundred, ten times running, still is not chance). So the pause is not the
   whole of it. **Next, on `nic-driver`'s own side:** a received descriptor is now taken only when the chip's
   error summary (`RxRES`, bit 21) is clear and the frame is first AND last segment, as r8169 checks; one
   that fails is given back unread, counted and said with its status word. Whether the damaged frames carry
   those marks is what the next card shows.

12. **The target, set by the operator 2026-10-20: a download at the speed of the link, 1 Gbit/s (about 119
   MiB/s).** The best today is 718 KiB/s. The same card's `time` rows (`download`, `utilities/65_download.md`)
   say where it goes, over six 10 MB downloads on the T630: **the network 9.7 to 20.3 s, almost all of it
   reads of `net-stack` that found nothing and waited** (1,004 to 2,098 per download, 8.6 to 19.2 s), the reads
   that brought bytes about 1.1 s, decrypting 0.4 s, the disk 3.2 to 4.1 s, the screen nothing. So the data is
   not arriving: the RTL8168's ring of 24 overflowing (item 1, 471 restarts in four minutes) is first, the disk
   second. QEMU reverses the two (disk 30 s, network 4 s), so its order is not this board's. The disk's cost is
   structural: each 3,556-byte `write_at` walks the path from the root (about three directory reads) and
   writes seven blocks as seven block-driver requests, about ten round trips per chunk. **What is not
   reachable as the constitution stands, recorded rather than hidden (26.7):** every frame crosses services by
   copy in messages of at most 4 KiB (2.5, 8.5), and a same-core round trip is about 51 us on the T630 (BP1),
   so the cable's hop alone tops out near 55 MB/s with perfect batching, before TLS and `fs` take theirs. An
   estimate of 15 to 30 MB/s is what this design can reach; beyond that is a constitutional question for the
   operator, not a change to make quietly.

   **2026-10-21: where it stands, and where a read's time goes.** Three changes since: the ring of 256 (item 1),
   `fs` keeping the last path walk (`d4c5a750`) and a chunk's seven blocks written in one request
   (`OP_WRITE_BLOCKS`, `075734d5`; on AHCI one command, on USB still one host call per block). So the disk path
   above, a walk from the root and seven requests per chunk, is how it was until 2026-10-20. Six T630 downloads
   at `c095c1ed`: **3.0 to 3.8 MiB/s**, the network 1.0 to 1.7 s, decrypting 0.4 s, the disk 0.56 to 1.08 s.
   The Wyse on the same image: 1.5 to 2.1 MiB/s, the network 1.7 to 3.8 s, the disk 1.0 to 1.8 s - its disk is
   a "32GB SATA Flash Drive" on AHCI, the T630's a Samsung SSD.

   `net-stack` now says where its own time goes while a connection is read (`tcp::Spent`, `14ac8dd8`). Its
   card, both machines, five or six downloads each, gives one read of 4 KiB from both sides:

   | Machine | `godspeed-pm` on | one read, as `godspeed-pm` waits for it | `net-stack`'s work for it | one ask of `nic-driver` |
   |---------|------------------|------------------------------------------|---------------------------|-------------------------|
   | T630 | core 0, 2 or 3 | 243 to 291 us | 2 us | 173 to 208 us, 1.9 frames |
   | T630 | core 1, beside `net-stack` | 508 us | 2 us | 329 us |
   | Wyse | core 0, 2 or 3 | 679 to 805 us | 2 us | 157 to 166 us |
   | Wyse | core 1 | 1,032 to 1,258 us | 3 us | 480 to 487 us |

   So a read costs `net-stack` almost nothing. 40 to 55% of reads arrive while it is asking `nic-driver` for
   frames and wait out the ask (about 45 us on average on the T630); the rest, about 230 us on the T630 and
   about 650 on the Wyse, is the request and the reply crossing between cores, where a same-core round trip
   measured 51 us (BP1). Core 1 is slower still because `godspeed-pm` then shares it with `nic-driver`,
   `net-stack`, `fs` and `block-driver`. That is the Wyse's slower network, and likely its slower disk, since
   every write is a call to `fs` on core 1 too. Spreading services over cores was considered and set aside:
   it adds cross-core trips rather than removing cost (the operator's question, 2026-10-21). **The next
   measurement is BP1 and BP2 on today's kernel, on both machines** (`osdev image --mode perf-brutal`), to see
   whether the cross-core round trip itself costs what these reads imply; if it does, its wake path is the
   target.

   **2026-10-21, later: it does not.** perf-brutal at `376e1595`: BP1 (same core) 112,013 cycles on the T630
   (56 us) and 64,498 on the Wyse (43 us); BP2 (cross core) 135,651 on the T630 (68 us) and 15,448,268 on the
   Wyse (10.3 ms, 1.03 ticks). A kernel wake trace (feature `wake-trace`, `51379c86`) then ran the same suite
   on both: no wake IPI is lost (sent and taken agree on every core), the tick clock keeps time (500 ticks in
   5,030 to 5,040 ms), and a woken task runs as soon as the IPI's handler does. What is slow is DELIVERY, and
   it has two clusters with nothing between: under 50 us, or 1 ms to 166 ms, none at 200 to 1,000 us. The slow
   ones fall only while the suite runs - every wake after it took under 50 us - on the T630 77 of about 580
   (13%), on the Wyse 177 of about 560 (32%). That is a core taking no interrupts for milliseconds under the
   suite's load (the spawn and restart probes are the first suspect, not shown), on both machines and worse
   on the Wyse, so BP2's 10 ms there measured the suite, not the machine.

   **And the T630's reads were not the cost.** The `time` row of a download at the same build (card of
   2026-10-21): 2,515 reads in 1,157 ms, of which 115 found nothing and waited 1,082 ms - the 2,400 that
   brought bytes cost about 31 us each. Its 9,023 frames came in 4,916 asks of `nic-driver` (190 us each, 935
   ms), a pace that would allow about 14 MB/s. So the table above, read as "a read costs 243 to 291 us", folds
   the waiting in; on the T630 the time is data not yet arriving, and why it arrives slowly (the TCP window,
   the acknowledgement pace) is the next question, not the cross-core hop.

   **2026-10-21, the Pi 4, one change per card** (10 MB, cable then radio unless said):

   | Build | Change | Cable | Radio |
   |-------|--------|-------|-------|
   | `0a927f39` | before | 124 KiB/s, 78 s (disk 40 s) | 63 KiB/s, 154 s (disk 43 s) |
   | `b2f91e9a` | a chunk to the stick as one WRITE(10) | 192 KiB/s, 50 s (disk 9.8 s) | 66 KiB/s, 146 s |
   | `3745c471` | GENET's ring 24 -> 256, the kernel's fixed-NIC arena 528 KiB | 655 KiB/s, 14 s | - |
   | `c772d2cc` | the SDIO host's settling gaps by the clock (a "2 us" spin was 137 us) | - | 208 KiB/s, 46 s |
   | `2bf32f56` | a 32 KiB window over a radio, not 8 | 673 KiB/s, 14 s | 191 KiB/s, 50 s: not the limit |
   | `ec7e2bf0` | `power`'s governor (Raspberry Pi OS's ondemand, `docs/power.md` 15.7) | 776 KiB/s, 12 s | 188 KiB/s, 51 s |
   | `4d485192` | an empty read yields for 2 ms before it parks (a "2 ms" sleep was a 10 ms tick) | 778-786 KiB/s, 12 s | 381 and 413 KiB/s (23-25 s); one run 179 |
   | `440b8cd1` | xhci gathers consecutive runs, 56 KiB to a WRITE(10) | **1.2-1.3 MiB/s, 6-7 s** (disk 4.8-5.0 s) | 159 and 186 KiB/s (50-61 s; disk 1.8-3.4 s) |

   The cable is the stick's now: 10 s of its 12 writing, a chunk of seven blocks per USB command, and more
   than seven needs more than one 4 KiB message (2.5, 8.5). GENET's overflow and discard counters read 0
   with the ring of 256; they were not read with 24. The radio is something else: its reads cost 606 us a
   frame and 105 us empty at either clock, so it is not the CPU, and 33 s of its 51 are reads that found
   nothing - the data is not arriving, for a reason not yet measured. The governor takes the clock to 1,500
   MHz within a look of a download starting and, once there, drops back for one look each 5 s when the
   load at full speed falls under half, as ondemand does with no step between the two ends.

   After the gather the cable's disk is about 28 ms per 56 KiB command, 2 MB/s: the stick's own writing
   speed, not its per-command turnaround, so larger commands buy less now. What would still help is
   overlap - writing one gather while the next fills, so a download takes the longer of the network and
   the disk rather than their sum. The radio varies from card to card (23, 25, 54, 50 and 61 s over the
   last five runs at nearly the same code), its reads cost the same each time (about 610 us a frame), and
   the time is data that has not arrived; the link itself (signal, band, rate - `wifi info`) has not
   been looked at.

   Two code defects in the disk path, found by Audit 19: `fs`'s `BLOCK_RPC_SECS` (30 s) now
   bounds a whole run of seven blocks, while on USB `block-driver` makes seven host calls of up to
   `IO_RPC_SECS` (10 s) each, so a slow stick can have `fs` give up on a run still in progress and leave its
   reply orphaned; and `block_write_run` shortened a run longer than `WRITE_RUN_MAX` without a word (its one
   caller never passes one) - that one is fixed, it refuses such a run now (`97892ba0`).

## Where to start

Item 4 is small and helps every board. Item 1 is the cable's ceiling on the PCs, item 2 the Pi 4's, and each
wants one card to show it.
