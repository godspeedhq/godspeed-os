# `feat/stdlib` - the hardware matrix, for the release note

**Run:** 2026-09-25. **Branch:** `feat/stdlib`. **Image:** built from `6f4ccd76` (one x86 image, one
per port; `git diff main...HEAD -- kernel/ sdk/` is 0 lines of CODE, comments only).

Kept here because it is release-note material and was asked for as such: paste the table, and take
the notes under it if the claims need backing.

---

## The matrix

| board | ISA | NIC | bus | selfcheck | sock | serve |
|-------|-----|-----|-----|-----------|------|-------|
| HP T630 | x86-64 | RTL8168 | PCIe | 516 / 0 | 94 B | 3/3 |
| Dell Wyse 5070 | x86-64 | RTL8168 | PCIe | 518 / 0 | 94 B | 3/3 |
| Raspberry Pi 2 | ARMv7 | smsc95xx | USB | 509 / 0 | 94 B | 3/3 |
| Raspberry Pi 4 | AArch64 | GENET | on-SoC | 516 / 0 | 94 B | 3/3 |
| StarFive VisionFive 2 | RISC-V 64 | dwmac | on-SoC | 516 / 0 | 94 B | 3/3 |

**Five boards, four instruction sets, four NIC drivers across three bus types. Zero failures.**

---

## What each column is, so the table cannot be misread

**`selfcheck`** is `ran N, failed 0`. The Pi 2 also reports `skipped 1`, which names itself:
`hw-enumerator - this machine has no PCI to enumerate (Pi 2); not a failure`. Every other board
reports `skipped 0`.

**The `ran` figures differ and all five are correct.** `ran` counts statements EXECUTED, and the
suite has guarded blocks, so it is a function of hardware and disk state - not of correctness. The
Pi 2 skipped a PCI block it has no bus for; the Wyse carried leftover churn files from an earlier
session and so ran two extra verification statements on them (`60-data.gsh` checks an existing
`/churn` BEFORE overwriting it, and those two extra checks passed). **`failed 0` is the comparable
figure; `ran` is not.** Reading `ran` as comparable produced one false alarm during this run.

**`sock`** is a UDP socket opened as a real kernel capability (§7.10), invoked to send a DNS query to
the resolver from the DHCP lease. 29 bytes out, 94 back, byte-identical on all five boards.

**`serve`** is `3/3` inbound TCP connections from a separate machine on the LAN, each accepted through
ACCEPT's embedded connection capability, echoed, and closed, with every byte returned unchanged.
Fifteen connections in total across the five boards.

---

## The three claims worth making from this

**1. `serve` is evidence QEMU structurally cannot produce.** SLIRP's only peer is the gateway, so an
unsolicited inbound connection from a third party had never once happened under emulation. All
fifteen of those connections are hardware-only evidence.

**2. `sock` had never completed on real hardware before this branch.** Two defects sat on top of each
other. The first (`82705c59`) was a hardcoded `10.0.2.3` - QEMU SLIRP's resolver - so on any real LAN
the datagram went nowhere. Fixing it revealed the second (`5716da17`): `udp_roundtrip` re-transmitted
the query on every retry and read whatever came back from the SEND, but `nic-driver` no longer couples
a receive to a transmit, so each retry drained the reply that had arrived and discarded it. It also
never answered an ARP for us and never paced its polls. All three are invisible under emulation, for
three separate reasons.

**3. The net-stack wire format change is validated across four drivers.** The badged path strips two
header bytes instead of one, with patience read from `pl.get(1)`. That is a protocol change; it has
now held on RTL8168 over PCIe, smsc95xx over USB, and GENET and dwmac on-SoC.

---

## What this matrix does NOT cover

- **`backlog/48` / `backlog/49`** - a userspace-reachable kernel panic, reproduced under load. A
  violation of an absolute bar (CLAUDE.md §22). Not caused or worsened by this branch, whose kernel
  diff is comments only, and already scheduled for after it.
- **`backlog/52`** - the QEMU shell suite flaking on `sock` and `serve`. Five green boards do not make
  a flaky harness deterministic.
- **Chaos and power-cut behaviour**, which are their own runs with their own evidence
  (`docs/gsfs-carnage.md`, `milestones/resilience/`).
