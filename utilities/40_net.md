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
| `net` | Print the current network status (IP, gateway, ping). |
| `net dns <host>` | Resolve a hostname to an IPv4 address (a DNS A-record lookup). |
| `net version` | Print the version. |
| `net help` | Print usage. |

Bare `net` reports the status net-stack froze at boot; `net dns <host>` is a live query. `net`
never *changes* the network - it reads and resolves (`0_conventions.md` §7, and the non-goals
below).

## 3. Output

```
gsh> net
ip       10.0.2.15
gateway  10.0.2.2 at 52:55:0a:00:02:02
ping     ok
```

Three labelled lines. When there is no NIC (a non-e1000 host, where `nic-driver` serves
empty replies and `net-stack` falls back to a default IP and resolves no gateway), the
truth is reported plainly rather than faked:

```
gsh> net
ip       10.0.2.15
gateway  unresolved
ping     no
```

- **ip** - the address `net-stack` holds (learned by DHCP, or the fallback if there was no
  offer).
- **gateway** - the gateway IP and the MAC ARP resolved for it, or `unresolved` if ARP got
  no answer.
- **ping** - `ok` if the gateway answered an ICMP echo; `no` otherwise.

`net dns <host>` resolves a name through net-stack's DNS query (to slirp's resolver):

```
gsh> net dns example.com
example.com is 104.20.23.154
gsh> net dns nope.invalid
nope.invalid: no answer (DNS goes via slirp to the host resolver)
```

DNS depends on the host's resolver - slirp forwards the query to it - so `no answer` is a
legitimate result (the query worked; nothing came back), not a bug.

## 4. Pipe behaviour (`to` / `from` / `where`)

`net` is a pipe **producer**, never a consumer or filter. Its three lines are ordinary
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

- **Service:** `net-stack` (`services/net-stack`). After its boot dance (DHCP -> ARP ->
  ICMP) it freezes a 15-byte record - our IP (4), gateway IP (4), gateway MAC (6), and a
  flags byte (bit 0 = gateway resolved, bit 1 = ping OK) - and serves it: any request
  carrying a reply cap gets that record back.
- **Shell:** the `net` built-in acquires `net-stack` by name (the kernel name directory,
  §14.3), sends a status request with `request_with_reply`, and formats the reply. On
  `EndpointDead` / no reply it reacquires by name and retries, then reports plainly if
  `net-stack` is still unreachable (Commandment VIII / IX).

## 6. Capabilities

- **Console output** to print the lines.
- **A SEND cap to `net-stack`, acquired by name** - the same brokering the file commands use
  to reach `fs`. `net` gains no network authority of its own; it can only *ask* `net-stack`,
  which is the sole holder of the frame interface to `nic-driver`.

## 7. Non-goals (deliberate - §26.2 minimal surface)

- **No configuration.** `net` reads; it never sets an IP, route, or DNS server. Those are
  `net-stack`'s job, and a future `net set ...` would be a separate, deliberate surface.
- **No live re-ping (yet).** the *status* (`net`) is what net-stack froze at boot; `net dns`
  is a live query, but a `net ping <host>` that pings on demand is a natural next subcommand
  once sockets exist.
- **No socket surface.** `net` is a status window. Opening a socket is the socket-capability
  work (`docs/networking.md`, a socket *is* a delegated resource cap, §7.10), reached through
  a different verb when it lands.

## 8. Conformance

Conforms to `0_conventions.md`: own `net help` / `net version` (version header on line 1 of
help; `version` prints the name + number + the copyright credit), real example per usage row,
words-not-flags, raw-facts-no-verdict. Pinned by `osdev test shell` (the `net` status line
after the networking phases).
