# 19. Networking does not recover from a chaos storm (Wyse / RTL8168)

**Severity:** real, user-visible, and it survives the storm looking healthy - which is the worse half.
**Status:** FIXED 2026-09-11 (`dd74d4c1`), verified on the machine that showed it. Kept open as a
record because the fix implements a documented sequence rather than pinpointing the faulty register -
see *What is NOT established* below, which still stands.

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
