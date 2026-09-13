# 19. Networking does not recover from a chaos storm (Wyse / RTL8168)

**Severity:** real, user-visible, and it survives the storm looking healthy - which is the worse half.
**Status:** FIXED 2026-09-11 (`dd74d4c1`), verified on the machine that showed it. Kept open as a
record because the fix implements a documented sequence rather than pinpointing the faulty register -
see *What is NOT established* below, which still stands.

## REPRODUCIBLE 2026-09-13: the first ping of the POST-CHAOS selfcheck loses one packet

**Two machines, two chipsets, the same counters.** This is a different and much sharper fingerprint
than the symptom set below, and it is worth chasing because it is deterministic rather than flaky.

|                  | T630 (AMD GX-420GI)  | Wyse 5070 (Intel Gemini Lake) |
|------------------|----------------------|-------------------------------|
| window closed after | 915,892 us        | 909,443 us                    |
| **drains**       | **44**               | **44**                        |
| **frames seen**  | **0**                | **0**                         |
| to-our-mac / arp-for-us / nic timeouts | 0 / 0 / 0 | 0 / 0 / 0          |
| tsc_hz           | 1,996,160,201        | 1,497,671,940                 |
| next packet      | reply in 36 ms       | reply in 36 ms                |

```
net-stack: ping window closed after 909443 us (44 drains, 0 frames seen, 0 to-our-mac,
           0 arp-for-us, 0 nic timeouts)  [budget 900000 us, tsc_hz 1497671940]
Request timed out.
Reply from 8.8.8.8: bytes=32 time=36ms TTL=117
```

**What makes it chaseable rather than noise:**

- **44 drains on both**, at two completely different TSC rates. A timing coincidence would not land
  on the same integer; that is a loop reaching a bound, not a race.
- **0 frames seen**, not frames-seen-but-unmatched. The NIC handed up nothing at all for 900 ms and
  then worked immediately.
- **Position is fixed**: the first ping of selfcheck's net section, immediately after
  `PASS net - the stack holds a lease`, in the POST-CHAOS run.
- **The pre-chaos selfcheck is clean on both.** Verified on the Wyse: zero `ping window closed` lines
  before the first `ran 461, failed 0`. So it needs the storm to have happened.

**ARM32 DOES NOT REPRODUCE IT, and that is the useful half.** The Pi 2 ran the same sequence on
2026-09-13 and its ping windows are a different shape entirely:

```
arm32:  89 drains, 25 frames seen,  3 to-our-mac, 3 arp-for-us   [tsc_hz 999996]
arm32:  90 drains, 105 frames seen, 0 to-our-mac, 1 arp-for-us
x86:    44 drains, 0 frames seen,   0 to-our-mac, 0 arp-for-us
```

On x86 the NIC hands up NOTHING. On arm32 it hands up plenty - 105 frames in one window - and the
echo reply is simply not among them. Those are different faults, so the x86 one is **not** a shared
`net-stack` bug: it is on the RTL8168 side, which is what this entry has always been about. A
cross-architecture negative is worth more here than another x86 repeat would have been.

(arm32 loses the odd packet too, but with frames flowing and ARP answered it looks like ordinary LAN
behaviour rather than this fingerprint. Not chased, and not claimed as the same thing.)

**What it is NOT.** None of the defining symptoms below returned: no TX timeout, no RX SILENT, no
DHCP failure, one boot to get networking back. `selfcheck` passed 461/0 three times on each machine,
and pings either side of the failure were clean. So the `dd74d4c1` fix stands; this is a narrower
residue that the fix does not cover.

**Not established, and not to be guessed at:** whether the frames never arrived, arrived and were
consumed by something else, or arrived before the window opened. The instrument says the NIC handed
up nothing; it does not say why. The next step is an RX-side counter comparison across that window
(MMC counters on the chip versus frames the driver handed to `net-stack`), which discriminates "the
wire was silent" from "we dropped them".

**One further observation, T630, 2026-09-13 (`b3054b53`), recorded as evidence and NOT as a
recurrence.** After `chaos max-carnage all-services 100 yes` (658 kills, 567 flooded) the first
post-chaos ping was clean 2/2. Twenty-eight seconds later, inside `selfcheck`, one ping lost its
FIRST packet and the second replied in 36 ms:

```
net-stack: ping window closed after 915892 us (44 drains, 0 frames seen, 0 to-our-mac,
           0 arp-for-us, 0 nic timeouts)  [budget 899999 us, tsc_hz 1996160201]
Request timed out.
Reply from 8.8.8.8: bytes=32 time=36ms TTL=117
```

What is notable is `0 frames seen` across 44 drains - the NIC handed up nothing at all for 915 ms,
rather than handing up frames that did not match. Selfcheck still passed 461/0, three times, and
networking was working either side of it.

This is NOT the symptom set above returning: no TX timeout, no RX SILENT, no DHCP failure, one boot.
It is one packet on the same chip family that this entry concerns, which is why it is written here
rather than being explained away. A single occurrence discriminates nothing; if the Wyse shows the
same shape, that is two and worth chasing.

**Verification, same board, same sequence:** ping, `chaos max-carnage all-services 100 yes`, ping
again WITHOUT a reboot. Every defining symptom is gone:

```
                              before        after
TX timeout                       5            0
RX SILENT                        1            0
DHCP - no offer                 15            0
boots needed to get ping back     2            1
```

The post-storm ping showed 9 of 13 replies. One loss is the stack re-ARPing seconds after the storm;
three consecutive losses mid-run carry `link not confirmed` and coincide with a deliberate Ethernet
hot-plug test. Neither resembles the failure recorded here, which was total and permanent until
reboot. During the storm itself `nic-driver` restarted repeatedly and each time reached
`reset OK / C+ rings up / DHCP ACK / echo reply`, which is the recovery that previously never came.

## What happens

The suite passes and the machine is fine. Networking is not.

```
18:31:21  ping 8.8.8.8 works                          (before chaos)
18:31:43  chaos round 1
18:33:25  chaos max-carnage: report - storm over
18:33:36  net-stack: DHCP - no offer within the budget - degrading to the fallback IP
   ...    the same line for FIVE MINUTES, across several ping attempts
18:39:08  REBOOT -> DHCP offer, ARP, ICMP echo reply OK
```

`selfcheck` returned `464, failed 0` twice during that window. The storm itself was clean: 0 kernel
panics, 0 liveness wedges.

## Where it is stuck

`nic-driver` restarts, resets the chip, and reports success:

```
nic-driver: RTL8168 reset OK  link UP  MAC e4:54:e8:0a:5c:7e
nic-driver: RTL8168 TX timeout - desc=0xf000011e isr=0x0280 cr=0x0c len=286 - recovering   (repeatedly)
nic-driver: RX SILENT 256 drains - RxOk=148 RxErr=0 Missed=1029 TxOk=11, link=up
```

**The reset genuinely succeeded.** `realtek_main` does check it - `reset_ok = spins <
REALTEK_RESET_MAX` - and printed `OK`, so `CR.RST` self-cleared. The link is up. And every transmit
times out with the descriptor still owned by the chip, while receive hears nothing and misses 1029
frames.

So a bare `CR.RST` does not restore an RTL8168 that was mid-DMA when its driver was killed. The C+
rings, descriptor ownership and Rx/Tx configuration come back in a state the chip will not run.

## The pattern this belongs to

This is the same parent failure as the VisionFive USB bug fixed in `d7d4e6db`, in a different driver
on a different architecture:

| | riscv64 `xhci` | x86_64 `nic-driver` |
|---|---|---|
| driver killed mid-flight | yes | yes |
| controller left in a bad state | yes | yes |
| re-init insufficient | reset never completed (HCRST) | reset completed, chip still dead |
| what the driver reported | `1 HID, disk yes` | `reset OK  link UP` |
| what actually worked | `probes 12/412` | `TxOk=11, Missed=1029` |

**Both drivers reported intent rather than function.** That is the property worth fixing generally: a
health line that says what was asked for is worse than no health line, because it is trusted.

The kernel's kill-path quiesce clears PCI Bus-Master-Enable and `nic-driver` IS in that list, so
bus-mastering WAS cleared here. It is not sufficient: it stops new DMA without restoring the chip's
internal state, and the next instance's minimal reset does not either.

## What is NOT established

**Whether this is new.** `TX timeout` and `RX SILENT` appear in no saved capture, including 600 chaos
rounds of earlier Wyse logs - but those are dated 2026-08-30, and the most recent clean Wyse run had
its serial log overwritten, so the comparison crosses an unknown gap. Against that: `DHCP - no offer`
after a storm HAS been seen before (6 occurrences in `wyse2.log`, 1 on the Pi 4), so the shape is not
unprecedented, and nothing in the code changed since the last multi-board coverage touches x86 or
`nic-driver` (only `kernel/Cargo.toml`, `arch/riscv64/mod.rs`, and `services/xhci/src/main.rs`).

Most likely reading: intermittent and pre-existing, like the USB one. Not proven.

## What a fix probably needs

A full re-initialisation rather than `CR.RST` alone - what a production RTL driver does on open:
unlock 9346CR, program RxConfig/TxConfig, rebuild the C+ descriptor rings from scratch and re-arm
them, reset descriptor ownership, re-enable RE/TE last. Read the reference as an executable datasheet
(§26.14) and take the silicon's requirement, not their driver model.

And separately, worth its own thought: `link UP` plus `TX timeout` is a contradiction the driver can
see. A driver that has timed out every transmit should stop reporting itself healthy.
