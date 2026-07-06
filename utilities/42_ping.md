# Utility: `ping` - ICMP echo to a raw IPv4 address

**Utility:** `ping` - send one ICMP echo request to a raw IPv4 and report whether it answers
**Status:** Built (first slice). As-built reference.
**Shape:** shell built-in that sends a `[3, ip]` request to `net-stack`.

---

## 1. Purpose

`ping` is the simplest reachability probe: it sends a single ICMP echo request to a raw IPv4 address
and reports whether the host answers. No DNS is involved - you give it an IP, not a name - so it is
the most direct way to ask "can I reach this host at all?".

It is also the cleanest diagnostic for the rest of the stack. `ping` runs through `net-stack`'s
*serve loop* (the same path `net dns` and `sock` use after boot), not the one-shot boot dance - so
`ping <your-gateway>` proves the post-boot request path works, and `ping 8.8.8.8` probes the internet
directly, each without depending on DNS.

## 2. Invocation

| Command | Meaning |
|---|---|
| `ping <ip>` | Send one ICMP echo to `<ip>` and report if it answers. |
| `ping version` | Print the version. |
| `ping help` | Print usage. |

`<ip>` is a raw IPv4 literal (`a.b.c.d`). A hostname (`ping google.com`) needs DNS resolution and is
rejected with a clear message until DNS lands - use `net dns <host>` to resolve, then `ping` the IP.

## 3. Output

```
gsh> ping 8.8.8.8
ping: sending echo request ...
8.8.8.8 is alive (ICMP echo reply)
```

On no answer it reports the reason it can see - how many frames came back while it waited, and how
many requests to the driver timed out:

```
gsh> ping 8.8.8.8
ping: sending echo request ...
8.8.8.8: no reply (0 frames, 6 timeouts)
```

## 4. Pipe behaviour

`ping` is a pipe producer: `ping 8.8.8.8 | write /ping.txt` captures the result to a file.

## 5. How it works

1. The shell parses `<ip>` into four octets (`parse_ipv4`, no allocation) and sends `net-stack` a
   `[3, a, b, c, d]` request.
2. `net-stack`'s serve loop builds an ICMP echo request inside an IPv4 packet addressed to `<ip>`
   (Ethernet destination = the gateway's MAC, so it routes off-subnet), sends it through
   `nic-driver`, and waits for the echo *reply* - matching the reply's source IP so a gateway ping
   and an internet ping cannot be confused.
3. `net-stack` replies `[alive, frames, timeouts]`; the shell formats "is alive" or a no-reply line
   with the diagnostic counters.

This reuses the same `ping()` helper `net-stack` runs at boot to check the gateway, so a working
`ping <gateway>` from the shell and a working boot-time gateway ping exercise identical code on
different schedules.

## 6. Capabilities

- **Console output.**
- **A SEND cap to `net-stack`, acquired by name** (the shell holds `ACQUIRE_ANY`).

`ping` holds no hardware capability itself - `net-stack` owns the ICMP construction and the frame
path to `nic-driver`; the shell only asks.

## 7. Non-goals / next

- **First slice.** One echo, one report. Repeated pings, round-trip timing, and a TTL/hop count are
  next steps.
- **No name resolution.** `ping <hostname>` needs DNS; resolve with `net dns` first for now.

## 8. Conformance

Conforms to `0_conventions.md`: `ping version` / `ping help`, words-not-flags, raw facts, pipe
producer. Pinned by `osdev test shell` (`ping 10.0.2.2` -> alive, end to end).
