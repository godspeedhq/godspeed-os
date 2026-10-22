# 93. Downloads are not yet at the speed of the link: what is left on the radios, the RTL8168 and `download`

**Status: OPEN, 2026-10-19.** Recorded at the operator's request at the end of the download-speed work on
`feat/packages` (`8d38fa61` to `d4ed712c`), to come back to. Everything below was seen on a board or in QEMU
that day; nothing here is fixed.

## Where it stands

The work that was done: a 256 KiB receive window with window scaling, out-of-order data kept in the ring,
delayed ACKs, burst polling, NIC receive rings of 24, the AIC8800 read whole, the RTL8168's receive ring no
longer emptied on every send, an orphaned fin-wait-2 bounded at 2 s, and the `download` utility
(`utilities/65_download.md`, `docs/tcp-design.md` "Downloads at the speed of the link").

On 2026-10-19 (item 12 has the figures since):

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
   `1bcfb9cb`: the CYW43455's data and scan paths read into a 64 KiB region. Item 10 names the paths that
   still read 4 KiB. 2026-10-21: the region read works on the Pi 4 - a 15,648-byte superframe came through
   whole - and a radio connection offers `RCV_RADIO`, 32 KiB, since `2bf32f56`; the cards showed the window
   was not the radio's limit, item 12.)* `wifi-driver` reads the CYW43455 into a 4096-byte
   buffer, and a large window's bursts arrive as superframes of 4.2 to 8 KB, which are dropped and logged
   (`exceeds this driver's 4096 byte buffer`). Until the 2026-10-19 fix that also killed the radio; now a
   connection over a radio offered the old 8 KiB window until `2bf32f56` (`Conn::small_window`, `a496e4de`), which survives
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
9. **The VisionFive has not run `d4ed712c`** (the Pi 4 has, in the download cards of 2026-10-21, and the Pi 2's
   cards of the same day ran builds that contain it, though none of them looked for the bound). Two of the last changes run on every board and
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
   | `440b8cd1` | xhci gathers consecutive runs, 56 KiB to a WRITE(10) | 1.2-1.3 MiB/s, 6-7 s (disk 4.8-5.0 s) | 159 and 186 KiB/s (50-61 s; disk 1.8-3.4 s) |
   | `027c4ade` | one gather written while the next fills (`4fb414f1`, `02dd4fe8`, Audit 20's re-scan fix) | **1.5 MiB/s, 5-6 s** (disk 3.5 s); 1.3 MiB/s with the keyboard pulled mid-download, the controller re-scanned and nothing lost | - |

   At `ec7e2bf0` the cable was the stick's: 10 s of its 12 writing, a chunk of seven blocks per USB command,
   and more than seven needs more than one 4 KiB message (2.5, 8.5) - which the gather, next row, works
   around inside xhci. GENET's overflow and discard counters read 0
   with the ring of 256; they were not read with 24. The radio is something else: its reads cost 606 us a
   frame and 105 us empty at either clock, so it is not the CPU, and at `ec7e2bf0` 33 s of its 51 were reads
   that found nothing - the data is not arriving, for a reason not yet measured. The governor takes the clock to 1,500
   MHz within a look of a download starting and, once there, drops back for one look each 5 s when the
   load at full speed falls under half, as ondemand does with no step between the two ends.

   After the gather the cable's disk is about 28 ms per 56 KiB command, 2 MB/s: the stick's own writing
   speed, not its per-command turnaround, so larger commands buy less now. Overlap - writing one gather
   while the next fills, so a download takes the longer of the network and the disk rather than their
   sum - is built since `4fb414f1` (with `02dd4fe8`, which keeps the write's completion across the driver's
   passes), and the Pi 4 card at `027c4ade` showed 5-6 s. A plain `http://` download still parks for a tick after a read that found nothing
   (`godspeed-pm/src/download.rs`); only `https://` busy-polls since `fb417544` (Audit 20). The radio varies from card to card (23, 25, 54, 50 and 61 s over the
   last five runs at nearly the same code), its reads cost the same each time (about 610 us a frame), and
   the time is data that has not arrived; the link itself (signal, band, rate - `wifi info`) has not
   been looked at.

   **2026-10-21 and 22, the Pi 2, one change per card** (10 MB over the LAN9514, through `dwc2`). Before
   these, the cable was 4-13 KiB/s and the RTL8188CUS dongle 16-36 KiB/s: the radio beat the cable.

   | Build | Change | Cable |
   |-------|--------|-------|
   | before | - | 4-13 KiB/s; 169 of 600 frames dropped inside `dwc2`, the interrupt harvesting into a receive queue of one burst still full |
   | `6b773823` | the receive queue holds four bursts, and a burst is taken only when it fits | none dropped; the chip's own RXDF (`INT_STS` bit 11) set throughout, and 42 KiB/s sliding to 5 |
   | `81fa1e05` | pause advertised and resolved as Linux does (`SMSC_ADVERTISE` 0x0DE1) | pause negotiated, `FLOW` 0xffff0002, `INT_STS` 0; still 53 sliding to 9, with bursts unparsed after every transmit's stand-down |
   | `78185667` | a transmit keeps the receive it stands down, and a cut frame is carried | **170-183 KiB/s**, none unparsed (52-87 caught by a transmit, 1-3 joined); then, at about 6%, the controller wedged - twice |
   | `742f6b00` | the whole core said beside a channel that never halted (instrument only) | wedged four times in four downloads (1-9%); `kill dwc2` alone recovered each |
   | `4cd3a01d` | the AHB burst length Linux sets for this core (`GAHBCFG_HBSTLEN_BCM`) | **136-161 KiB/s, 1 min to 1 min 11 s, six downloads in six**, with the keyboard re-plugged and the stick moved to another port between them; no stall. Disk 43-50 s of each, network 11-16 s |
   | `fac877e8` | a run of up to seven blocks to the stick as one WRITE(10), not seven | **286-338 KiB/s, 28-35 s, four downloads in four** (disk 16-19 s, network 8-12 s, decrypt 4.5-6.9 s); `drives check` clean |
   | `fdc8fd6c` | a file's consecutive runs gathered into one 32 KiB WRITE(10) | 335-374 KiB/s (disk 9.6-11 s) |
   | `f78b7b91` | the cable's received frames in one exchange (`OP_NET_RX_BATCH`); a backgrounded job says how to get back to it | **424-471 KiB/s, 20-23 s**; an ask of the driver 2,720 us to 1,590 us; no segment lost on the cable |
   | `72a1f584` | a 96 KiB gather in a 128 KiB arena | 459 KiB/s, disk 10.6 s: no gain, the stick's own speed (about 1 MB/s through `dwc2`); back to 32 KiB at `422257cb` |
   | `d48dbc01` | (the radio's; the cable unchanged) | **465-469 KiB/s, 20 s** (network 5.8 s, decrypt 5.3-5.4 s, disk 9.3 s) |

   **Over the RTL8188CUS, the same day** (the cable out; 10 MB; `net-stack`'s segment and gap lines from
   `f78b7b91` and `1eae207b`):

   | Build | Change | Radio |
   |-------|--------|-------|
   | before | - | 16-36 KiB/s; 65% of the radio's USB transfers were other networks' beacons |
   | `ffc440ec` | once joined, the chip hears only the joined network (RCR bits 6 and 7, as rtl8xxxu) | 72 KiB/s; beacons from about 118 a second to about 10 |
   | `f78b7b91` | (instrument) | 48 KiB/s: 1,763 segments of 9,246 ahead of a gap, 476 retransmitted |
   | `72a1f584` | (instrument) | 81 KiB/s: the data waited 33.7 s of 119 s behind 62 gaps; a received transfer uncollected for up to 708 ms |
   | `422257cb` | `dwc2` holds a ring of eight received transfers, not one | no WiFi download: a notice owed only on empty-to-not left the ring full for good with the cable in (fixed `d7f4c6ad`) |
   | `d7f4c6ad` | the ring's notice owed again while it holds more | 58 KiB/s: 9 gaps, 5.2 s behind them; 41 frames a second arrived, 87% of asks found none |
   | `d48dbc01` | the transmit side of rtl8xxxu's init (response rates, SIFS, retries, EDCA, ACK timeout) | 79 KiB/s: 102 gaps, 61 s behind them, about 23 segments a gap; the ring full only 3 s of it |
   | `91e9448e` | the radio's IN no longer stands aside for the disk's transfers | 64 KiB/s: 91 gaps, 62.6 s behind them - unchanged; the disk unharmed (cable 409-470 KiB/s). Reverted: the losses are not the stand-aside |

   **Where the Pi 2's radio stands, set aside by the operator 2026-10-22** ("the Pi 2 ethernet is quick enough
   for the package manager"): 58-81 KiB/s, against 465-470 over the cable. What is known: the losses come in
   bursts of about 23 segments, 60-100 a 10 MB download with up to a minute waited behind them; they land
   before `dwc2`'s ring (full for 3 s of it); they are not the stand-aside (`91e9448e`); and about 41 frames a
   second arrive, with the access point resending frames already received and once ending an association
   for frames left unacknowledged (reason 34). What is left, in the order to try:
   1. The chip's IQ calibration (rtl8xxxu's gen-1 IQ calibration), the last of `rtl8xxxu_init_device` not
      done: it sets the transmitter's quality, which the access point hearing our acknowledgements turns on.
   2. `wifi-usb`'s reply mailbox (`backlog/74`, a kernel change, the operator's to decide): about 10,000 of the
      host's notices taken in place of answers in three minutes, each resynchronised (`usbfn::OP_SYNC`).
   3. Receive aggregation (off by default in `rtl8xxxu`): several frames to a USB transfer.
   4. A 32 KiB window over a radio (`net-stack`), once the losses are gone.

   **The VisionFive, the same day** (10 MB, the cable and the AIC8800): at `8f150cfe`, 113-271 KiB/s over the
   cable and two downloads in three failing to connect, the dwmac receive ring of 24 running dry (`RBU` 237
   to 403 in a download) with 4,270 of 10,084 segments after a gap; the disk 2.1 s and decryption 1.1 s. At
   `6bfd508d`, the ring 256 as GENET's: **1.4-1.7 MiB/s, 5-6 s, `RBU` 0**, the disk now the largest part
   (2.5-3.2 s). The radio 37-45 KiB/s, with 122-126 gaps and up to 161 s waited behind them - unchanged, and
   left as the Pi 2's is. The same card's `drives check` ended in a kernel panic, `backlog/94`.

   **The VisionFive's WiFi stays slow, set aside at the operator's word (2026-10-22).** The serve loop's receive
   queue of 32 (`1047ac67`) cut the frames turned away from 136 to 1, and the radio stayed at about 60 KiB/s,
   with `nic-driver`'s frame reads taking 23-86 ms. The SDIO bus never leaves the identification clock (400 kHz
   by the driver's arithmetic, one data line), so two cards raised it to 25 MHz. Raised at identification
   (`ae94e8a5`), the uploads ran in half the time and the ROM then stopped answering in the patch table's
   Bluetooth power-on group. Raised after the firmware's start (`0e9aec68`), the first CMD53 after the switch
   went unanswered (`INT` 0). Both left the radio down, and both are reverted (`860fe372`). Two facts for
   whoever picks it up: the patch upload's 21 ms a block at the slow clock is less than its bit time at 400 kHz
   (`docs/wifi-aic8800.md` 4), so the card clock may already be above what the driver computes, and the bus
   may not be the whole limit; and how the reference runs this chip faster on this board (the host's sample
   and drive phase, a clock change after the power-on group, a 4-bit bus) has not been read. Read it first.

   **Recovery, the same day.** The dongle's first firmware block failed on the bus at boot on four boots and
   every control read after it was NAKed until it was replugged by hand: `wifi-usb` asks for a software
   replug now (`usbfn::OP_REPLUG`, `bab37ad0`; routed wrongly until `ae4805fb`). **Seen at `91e9448e`, and not
   enough:** the port was reset and the chip did not enumerate after it (`port 2 FAILED (split)`), and `dwc2`
   said to replug it, which brought it back. A port reset does not clear this wedge; a power cycle does, and
   the Pi 2's USB ports share one power switch, so on this board the remaining remedy is the hand. Why the
   first block fails at boot (four boots in eight on 2026-10-22, never on a replug) is not known. And an
   access point refused the boot rejoin once (reason 34), and nothing tried again until the dongle was
   replugged: a refused rejoin is tried three more times since `d48dbc01`.

   **The wedge, as it was seen (fixed at `4cd3a01d`, by the burst length).** Both times the stick's WRITE(10) bulk OUT never halted first
   (`HCCHAR` with ChEna and ChDis both set), then every transfer to every device did; `block-driver` lost
   its disk and `fs` its writes, and `net-stack` failed one connect while it lasted. `kill all-services`
   recovered it, and the new `dwc2` found the AHB master not idle at its reset - the core's DMA hung. No
   earlier run moved data this fast, so whether `78185667` caused it or exposed it is not known.
   `742f6b00`'s lines then showed it four times out of four: a channel had received data and never
   finished writing it to memory - the network's receive at 3072 of 4096 bytes three times and at 1024
   once, and once the stick's 13-byte status reply - with the AHB master not idle and `GAHBCFG` at 0x21,
   DMA on and the burst length 0, single transfers. Linux writes `ahbcfg = 0x10` for this core
   ("brcm,bcm2835-usb", `drivers/usb/dwc2/params.c`, v4.4 to v6.6); with that, six downloads in six
   finished. Why single transfers hang this core is not known; that the reference's value does not is
   what was shown.

   Two things Audit 21 found to look at, not shown to matter, and not needed for the wedge: `chan::halt` returns
   nothing, so when its 50 ms expires `tx` goes on as if the receive channel were retired - reads its
   `HCTSIZ` as held bytes, and `resume` re-arms a channel the core may still own; and `resume` takes held
   bytes as a burst that may continue (`may_continue`) even when the IN had completed on a short packet
   rather than been stood down, so a malformed tail would be joined to the next transfer (`HCINT`'s
   transfer-complete bit would tell the two apart).

   Two code defects in the disk path, found by Audit 19: `fs`'s `BLOCK_RPC_SECS` (30 s) now
   bounds a whole run of seven blocks, while on USB `block-driver` makes seven host calls of up to
   `IO_RPC_SECS` (10 s) each, so a slow stick can have `fs` give up on a run still in progress and leave its
   reply orphaned; and `block_write_run` shortened a run longer than `WRITE_RUN_MAX` without a word (its one
   caller never passes one) - that one is fixed, it refuses such a run now (`97892ba0`).

## Where to start

Item 4 is fixed and item 1 is (the ring of 256). As of 2026-10-21 the PCs' downloads are limited by data
arriving (item 12); the Pi 4's cable by the stick's own writing speed, its radio by data not arriving, the
link not yet looked at (`wifi info`). The Pi 2's cable reaches 170-183 KiB/s and then wedges the USB
controller (item 12): the wedge first, then the receive transfer Linux uses (a burst cap of 37, 18,944
bytes, a bulk-in delay of 0x2000) against this driver's 8 and 4,096. *(2026-10-22: the wedge is fixed at
`4cd3a01d`, and the Pi 2's cable is now the stick's - 43-50 s of a 60-71 s download is the disk, 11-16 s
the network - so `dwc2`'s stick writes are next, ahead of the receive transfer; xhci's gather and overlap
are the Pi 4's answer to the same thing.)* *(Later the same day: the one-command run took the disk to 16 s (`fac877e8`), and the disk is still the
largest share; gathering runs and overlapping them, as xhci does, is next. Over the RTL8188CUS a download ran
at about 50 KiB/s, and plugging the cable in part way through stalled it for about 90 s: `nic-driver` moved
the frames to the cable on its next read of the link bit, while `net-stack` kept the radio's address and
gateway until its next address check (`ADDR_CHECK_SECS`), so the download's frames went to the other
network. Every backend with a radio routes by that live bit (`dwc2`'s, GENET's, dwmac's and the RTL8168's).)*
