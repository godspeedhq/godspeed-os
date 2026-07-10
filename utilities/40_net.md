# Utility: `net` - am I on the network?

**Utility:** `net` - network status (IP, gateway, reachability)
**Status:** Built. As-built reference.
**Shape:** shell built-in that brokers a query to the `net-stack` service (see `0_conventions.md` §2).

---

## 1. Purpose

`net` answers one question: **am I on the network, and who is my gateway?** It reports the
IP `net-stack` configured for itself, the gateway it resolved by ARP, and whether that
gateway answered a ping. Raw facts, no verdict (`0_conventions.md` §7) - it prints what is,
and lets you read connectivity off it.

The networking stack (`docs/networking.md`) is two userspace services: `nic-driver` (raw
frames) and `net-stack` (ARP/IPv4/ICMP/UDP over those frames). At boot `net-stack` learns
its IP by DHCP, resolves the gateway by ARP, and pings it - then *freezes* that result and
serves it. `net` is the window onto it: the shell acquires `net-stack` by name and asks.

## 2. Invocation

| Command | Meaning |
|---|---|
| `net` | Print the current network status: NIC (link/speed/MAC/hardware counters), IP, gateway, ping, DNS. |
| `net dns <host>` | Resolve a hostname to an IPv4 address (a DNS A-record lookup). |
| `net stats` | Dump the NIC's raw registers (chip state) - is the receiver enabled, is the ring armed? |
| `net arp <ip>` | Resolve one host's hardware (MAC) address by ARP. |
| `net scan` | ARP-sweep the local /24 and list the hosts that answer. |
| `net renew` | Re-run DHCP/ARP to reconfigure the network in place - recover a link that came up after boot (a cable plugged in later), no reboot. |
| `net version` | Print the version. |
| `net help` | Print usage. |

Bare `net` reports the status net-stack froze at boot (plus a live read of the NIC's own hardware
counters); `net dns <host>` is a live query; `net stats` is a live register read. `net` never
*changes* the network - it reads and resolves (`0_conventions.md` §7, and the non-goals below).

Related utilities: **`ping <ip>`** (a one-shot ICMP echo to a raw IP, no DNS - see `42_ping.md`) and
**`sock`** (a UDP socket as a capability - see `41_sock.md`).

## 3. Output

On hardware with the RTL8168 (T630), `net` reports the full picture - the NIC, its live link,
the chip's own hardware counters, then the L3 status:

```
gsh> net
nic      10ec:8168  mmio 0xfea04000  (RTL8168)
nic-mac  7c:d3:0a:2b:b0:e3  reset ok
nic-link UP 1000M full  |  tx ok (4 sent)  |  rx 90B (4 recv)
nic-hw   RxOk=1832 TxOk=7 RxBcast=1210 RxErr=0 Miss=0
ip       192.168.4.80
gateway  192.168.4.1 at 00:ab:48:da:1b:0d
ping     ok
dns      192.168.4.1
```

- **nic** - PCI vendor:device, the MMIO register base, and the chip name (from the kernel).
- **nic-mac** - the MAC read off the chip, and whether the chip reset succeeded (queries
  `nic-driver`; `TIMEOUT` here means MMIO is not reaching the chip).
- **nic-link** - link up/down, negotiated speed/duplex, and the driver's TX/RX request counts.
- **nic-hw** - the **chip's own** cumulative hardware tally counters, read straight off silicon
  (DTCCR dump), independent of net-stack: RxOk, TxOk, RxBcast (broadcasts received), RxErr,
  Miss. **Layer-1 ground truth** - if `RxOk`/`RxBcast` climb between two `net`s, the receiver is
  alive; if they stay flat, the NIC is not receiving.
- **ip** - the address `net-stack` holds (learned by DHCP, or the fallback if there was no offer).
- **gateway** - the gateway IP and the MAC ARP resolved for it, or `unresolved` if ARP got no answer.
- **ping** - `ok` if the gateway answered an ICMP echo; `no` otherwise.
- **dns** - the DNS server `net-stack` will use (DHCP option 6, or the gateway as a fallback).

On QEMU's e1000 the `[3]` status reply is shorter, so only `nic` + `nic-mac` show (no link/hw lines);
and when there is no drivable NIC, `nic-driver` serves empty replies and `net-stack` reports
`gateway unresolved` / `ping no` plainly rather than faking it.

`net dns <host>` resolves a name; `net-stack` tries the DHCP-learned server, then a public resolver
(8.8.8.8) reached through the gateway (a home router may do DHCP + ICMP yet not run a DNS forwarder):

```
gsh> net dns example.com
example.com is 104.20.23.154
gsh> net dns nope.invalid
nope.invalid: no reply from the DNS server (0 frames, 0 UDP, 6 timeouts)
```

The no-reply line carries a diagnostic - frames seen, how many were UDP, and how many driver requests
timed out - so a failure says *where* it failed, not just that it did.

`net stats` dumps the NIC's raw registers - the chip state, for when you need to know whether the
receiver is even enabled (`CR.RE`), whether the receiver is promiscuous (`RCR`), and whether frames
are sitting in the RX ring (each descriptor's `OWN` bit):

```
gsh> net stats
NIC registers (RTL8168):
  CR        0x0c   RE=1 TE=1 RST=0
  9346CR    0x00   locked
  PHYSTATUS 0x2f   link=1 spd=1000M dup=full
  IMR       0x0000
  ISR       0x0005
  RMS       0x0800   (2048 bytes)
  RCR       0x0000e70f   AAP=1 APM=1 AM=1 AB=1
  TCR       0x03000700
  TNPDS.lo  0x1f2c0000   TX ring base
  RDSAR.lo  0x1f2c2000   RX ring base
  RX ring (rx_idx=0):
    [0] opts1=0x80000800  OWN=1 len=2048
    [1] opts1=0x80000800  OWN=1 len=2048
    [2] opts1=0x80000800  OWN=1 len=2048
    [3] opts1=0xc0000800  OWN=1 len=2048
```

`OWN=1` means the NIC owns that descriptor (armed, waiting for a frame); `OWN=0` means a frame has
landed and the driver has not consumed it yet. (On QEMU the e1000 path prints CTRL/STATUS/RCTL/etc.)

`net arp <ip>` resolves one host's MAC by ARP, and `net scan` sweeps the whole /24 - both live queries
(like `net dns`), for "who else is on this LAN?":

```
gsh> net arp 192.168.4.1
192.168.4.1 is at 00:ab:48:da:1b:0d
gsh> net scan
Scanning 192.168.4.0/24 for live hosts (press q to abort):
  192.168.4.1
  192.168.4.80
  192.168.4.107
3 host(s) responded.
```

`net scan` walks the /24 host-by-host, **driven from the shell** (one ARP resolve per host, the same op
`net arp` uses), printing responders as it finds them. Driving it from the shell is deliberate: `q`/ESC
then actually **stops the work** - net-stack is only ever resolving one host, never wedged finishing a
254-host sweep - so a *second* `net scan` never finds net-stack busy (a single batch op left the next
command stuck). Quick on a real LAN; slower on a quiet link, but always abortable (`0_conventions.md`
§1.10). `net arp` is likewise abortable.

## 4. Pipe behaviour (`to` / `from` / `where`)

`net` is a pipe **producer**, never a consumer or filter. Its labelled lines are ordinary
output, so they flow onward like any producer's:

- **to a file** - `net | write /netstat.txt` snapshots the status to disk (redirection is
  `| write`; there is no `>`, see `19_write.md`).
- **to a filter** - `net | match gateway` keeps just the gateway line; `net | count` counts
  the lines.
- **from** - `net` reads *from* the `net-stack` service (over IPC, acquired by name), not
  from a file or from stdin - so it is always the **first** stage of a pipe, never a later
  one. Nothing pipes *into* `net`; piping into it is a loud `unknown`-shaped error, not a
  silent no-op.

`where` the bytes come from: `net-stack`'s frozen boot-time status record (§5), formatted by
the shell. `net` performs no network I/O itself - it asks the service that does.

## 5. Data source

- **Service:** `net-stack` (`services/net-stack`). After its boot dance (DHCP -> ARP -> ICMP)
  it freezes a 19-byte record - our IP (4), gateway IP (4), gateway MAC (6), a flags byte (bit 0
  = gateway resolved, bit 1 = ping OK), and the learned DNS server (4) - and serves it. It also
  answers live requests: `net dns` (byte 0 = 1, then the hostname), `ping <ip>` (byte 0 = 3, then the 4 IP
  bytes), `net arp <ip>` (byte 0 = 6, then the 4 IP bytes -> `[found, mac(6)]`), and `net renew` (byte 0 = 8
  -> re-runs the boot dance `run_dance` in place and replies the fresh status). `net scan` reuses op 6
  host-by-host - the shell walks the /24 itself, so aborting it actually stops the work (no batch op).
- **Recovery:** the boot dance (DHCP -> ARP -> ICMP) is `run_dance`, run once at boot AND again on `net
  renew`, so a link that comes up after boot reconfigures the stack without a reboot (nothing is special;
  the link recovers like any restartable thing).
- **Driver:** `nic-driver` answers two diagnostic queries directly (the shell holds `ACQUIRE_ANY`
  and asks it by name): `[3]` returns the 32-byte hardware status (MAC, link, speed, and the chip's
  tally counters via a DTCCR dump); `[5]` returns the raw register dump for `net stats`.
- **Shell:** the `net` built-in acquires `net-stack` (and `nic-driver`) by name (the kernel name
  directory, §14.3), sends each query bounded (`net_query`, abortable with `q`), and formats the
  replies. On no reply it reacquires by name and retries, then reports plainly (Commandment VIII / IX).

## 6. Capabilities

- **Console output** to print the lines.
- **A SEND cap to `net-stack`, acquired by name** - the same brokering the file commands use
  to reach `fs`. `net` gains no network authority of its own; it can only *ask* `net-stack`,
  which is the sole holder of the frame interface to `nic-driver`.

## 7. Non-goals (deliberate - §26.2 minimal surface)

- **No configuration.** `net` reads; it never sets an IP, route, or DNS server. Those are
  `net-stack`'s job, and a future `net set ...` would be a separate, deliberate surface.
- **Ping-on-demand is its own utility.** ICMP to an arbitrary IP is `ping <ip>` (`42_ping.md`),
  not a `net` subcommand - `net` reports the gateway ping net-stack froze at boot.
- **Sockets are their own utility.** Opening a UDP socket is `sock` (`41_sock.md`) - a socket
  *is* a delegated resource cap (`docs/networking.md`, §7.10). `net` stays a status/diagnostic window.

## 8. Conformance

Conforms to `0_conventions.md`: own `net help` / `net version` (version header on line 1 of
help; `version` prints the name + number + the copyright credit), real example per usage row,
words-not-flags, raw-facts-no-verdict. Pinned by `osdev test shell` (the `net` status line
after the networking phases).
