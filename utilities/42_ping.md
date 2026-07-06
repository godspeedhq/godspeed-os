# Utility: `ping` - continuous ICMP echo to a raw IPv4 address

**Utility:** `ping` - repeatedly echo a raw IPv4 and report round-trip time + TTL, Windows-style
**Status:** Built. As-built reference.
**Shape:** shell built-in that sends a `[3, ip, bytes]` request to `net-stack` per echo.

---

## 1. Purpose

`ping` is the reachability probe everyone already knows: it sends ICMP echo requests to a raw IPv4
address, once a second, and prints a line per reply with the **round-trip time** and the reply's
**TTL** - exactly like `ping` on Windows/Linux. No DNS is involved (you give it an IP, not a name),
so it is the most direct way to ask "can I reach this host, and how fast?".

It is also the cleanest diagnostic for the rest of the stack. `ping` runs through `net-stack`'s
*serve loop* (the same path `net dns` and `sock` use after boot), not the one-shot boot dance - so
`ping <your-gateway>` proves the post-boot request path works, and `ping 8.8.8.8` probes the internet
directly, each without depending on DNS.

## 2. Invocation

| Command | Meaning |
|---|---|
| `ping <ip>` | Ping continuously (a reply line per second). **`q` quits**, then a statistics summary prints. |
| `ping count <N> <ip>` | Send exactly `N` echoes, then stop and print statistics. |
| `ping bytes <N> <ip>` | Set the ICMP **data** size (default 32, max 1024). Combinable with `count`. |
| `ping version` | Print the version. |
| `ping help` | Print usage. |

Options come **before** the IP and may be combined: `ping bytes 64 count 4 192.168.4.1`.
`<ip>` is a raw IPv4 literal (`a.b.c.d`). A hostname (`ping google.com`) needs DNS and is rejected
with a clear message - use `net dns <host>` to resolve, then `ping` the IP.

## 3. Output

```
gsh> ping 192.168.4.1
Pinging 192.168.4.1 with 32 bytes of data:
Reply from 192.168.4.1: bytes=32 time=2ms TTL=64
Reply from 192.168.4.1: bytes=32 time=3ms TTL=64
Request timed out.
Reply from 192.168.4.1: bytes=32 time=2ms TTL=64
(q)

Ping statistics for 192.168.4.1:
    Packets: Sent = 4, Received = 3, Lost = 1 (25% loss)
Approximate round trip times in milli-seconds:
    Minimum = 2ms, Maximum = 3ms, Average = 2ms
```

- **`time=Nms`** - the measured round trip. A reply that returns in under a millisecond prints
  `time<1ms` (as on Windows). The time is measured with the CPU's TSC and converted to milliseconds
  using the kernel's boot-time TSC calibration (InspectKernel query 16).
- **`TTL=N`** - the time-to-live in the reply's IP header (the pinged host's, not ours).
- **`Request timed out.`** - no matching echo reply arrived within the budget for that round.
- The **statistics** summary prints whether you stopped with `q` or a `count` run finished.

## 4. Pipe behaviour

`ping` is a pipe producer. A bounded run is the sensible thing to capture:
`ping count 4 8.8.8.8 | write /ping.txt`. (Piping the endless default is possible but pointless.)

## 5. How it works

1. The shell parses the options and `<ip>` (`parse_ipv4`, no allocation) and, per echo, sends
   `net-stack` a `[3, a, b, c, d, bytes_lo, bytes_hi]` request.
2. `net-stack`'s serve loop builds an ICMP echo request of `bytes` data inside an IPv4 packet
   addressed to `<ip>` (Ethernet destination = the gateway's MAC, so it routes off-subnet), times
   the round trip with the TSC, sends it through `nic-driver`, and waits for the echo *reply* -
   matching the reply's source IP so a gateway ping and an internet ping cannot be confused.
3. `net-stack` replies `[alive, rtt_ms(le u16), reply_ttl]`; the shell formats the `Reply from` line,
   accumulates min/max/average, and paces ~1 s to the next echo while polling for `q`.

This reuses the same `ping()` helper `net-stack` runs at boot to check the gateway, so a working
`ping <gateway>` from the shell and a working boot-time gateway ping exercise identical code.

## 6. Capabilities

- **Console output**, and **console input** (to catch `q`).
- **A SEND cap to `net-stack`, acquired by name** (the shell holds `ACQUIRE_ANY`).

`ping` holds no hardware capability itself - `net-stack` owns the ICMP construction and the frame
path to `nic-driver`; the shell only asks (and reads the ungated TSC calibration for the timing).

## 7. Non-goals

- **No name resolution.** `ping <hostname>` needs DNS; resolve with `net dns` first.
- **No fragmentation.** The data size is capped at 1024 bytes (one frame); larger is clamped.

## 8. Conformance

Conforms to `0_conventions.md`: `ping version` / `ping help`, words-not-flags (`count`/`bytes`, not
`-c`/`-s`), raw facts, pipe producer. Pinned by `osdev test shell` (`ping count 3 10.0.2.2` -> the
Windows-style header, reply/timeout lines, and statistics summary, end to end).
