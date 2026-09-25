# Utility: `sock` - a UDP socket as a capability

**Utility:** `sock` - open a UDP socket capability and send a datagram through it
**Status:** Built (first slice). As-built reference. Hardware-verified on the Raspberry Pi 2
(2026-09-15): the mint, the grant and the badged invocation all complete on real hardware. The
DATAGRAM round trip took until 2026-09-25 and two separate fixes - see 3.
**Shape:** shell built-in that opens a socket cap from `net-stack` and invokes it.

---

## 1. Purpose

`sock` demonstrates the payoff of the networking design: **a socket IS a capability** (§7.10 - the
same delegated-resource-cap mechanism as file-as-capability). The shell asks `net-stack` to open a
UDP socket; `net-stack` MINTS a socket capability and hands it back. The shell then INVOKES that cap
to send a datagram - the kernel validates the cap and badges the invocation with the socket's
ResourceId, so `net-stack` knows which socket without the kernel knowing what a socket is.

A socket is not an ambient channel or a file-descriptor number - it is an unforgeable token the
client holds, exactly like a file cap.

## 2. Invocation

| Command | Meaning |
|---|---|
| `sock` | Open a UDP socket cap, send a datagram through it, report the round-trip. |
| `sock version` | Print the version. |
| `sock help` | Print usage. |

First slice: one fixed demonstration. A general `sock <ip> <port> <data>` and a receive path are the
next steps (§7).

## 3. Output

```
gsh> sock
sock: UDP socket cap - sent 29 bytes to 192.168.4.1:53, received 94 bytes back (a round-trip through a capability)
```

The datagram is a small DNS query (just data that elicits a UDP response); `sock` reports the
round-trip - bytes out and back - which proves the cap does real UDP I/O. When there is no NIC, the
invocation returns nothing and `sock` says so plainly. The destination is **the resolver from the
DHCP lease**, which is why the address above is a LAN address and not a constant; with no lease,
`sock` says there is no resolver rather than sending into the void.

### It took TWO fixes to work on hardware, and the first one's success hid the second

This section used to say the destination was "hardcoded to `10.0.2.3:53`, which is a QEMU address,
so on real hardware this reports 0 bytes back", and recommended reading the resolver from the lease.
That was true, the fix shipped (`82705c59`), and **it was only half the cause**:

1. **The address.** `10.0.2.3` is QEMU SLIRP's resolver and nothing else, so on any real LAN the
   datagram went nowhere. Unfalsifiable under emulation, where the constant happens to be right.
2. **The wait.** `udp_roundtrip` RE-TRANSMITTED the query on every retry and read whatever came back
   from the send. `nic-driver` no longer couples a receive to a transmit, so each retry drained the
   reply that HAD arrived and discarded it; it also never answered an ARP for us, and never paced its
   polls. Fixed in `5716da17` by giving it the send-once-then-RX-poll shape the DNS path already had.

The lesson is about the DOC, not the code: stating one cause confidently concealed the other. After
fix 1 the address was right, the command still failed, and the spec said the cause was known. A
partial diagnosis asserted as complete is worse than no diagnosis, because it stops the next person
looking. Measured on the T630 between the two fixes:

```
sock: UDP socket cap - sent 29 bytes to 192.168.4.1:53, nothing came back (the send went through the capability; the peer did not answer)
```

Right address, real resolver, reply destroyed in flight. The *capability* path - minted, granted,
invoked, badged, routed, answered - was working correctly the whole time, which is what made the
wrong half so easy to believe.

**The socket path is deliberately UNTAGGED.** Every other net-stack request carries a correlation
byte at offset 0 (`docs/net-tags-design.md` §8); a badged socket invocation does not, because the
badge already names the socket and there is nothing for the client to disambiguate. `fs` makes the
identical exception for file capabilities.

## 4. Pipe behaviour

`sock` is a pipe producer: `sock | write /sock.txt` snapshots the result to a file.

## 5. How it works (the capability path)

1. `net-stack` holds `RESOURCE_MINT` (granted by the kernel by name, exactly like `fs`).
2. `sock` sends `net-stack` an "open socket" request; `net-stack` mints a socket cap
   (`resource_mint`, READ|WRITE) and grants it back (`send_with_cap_by_handle`).
3. `sock` invokes the cap (`resource_invoke` with `RIGHT_WRITE` and a payload of `dest_ip, dest_port,
   data`); the kernel validates rights + generation, badges the message with the socket's ResourceId,
   and routes it to `net-stack`.
4. `net-stack` reads the badge (`last_recv_badge`), finds the socket, builds a UDP datagram, sends it
   through `nic-driver`, and replies with the response.

Because this is the file-cap mechanism, every §7.3 property is inherited: unforgeable (a fabricated
handle is not a socket cap), non-escalating (a send needs `RIGHT_WRITE`), revocable (closing bumps
the generation). This first slice exercises the mint + invoke + send; the forged/revoke checks (as
`fcap` does for files) come with the fuller `sock` surface.

## 6. Capabilities

- **Console output.**
- **A SEND cap to `net-stack`, acquired by name** (the shell holds `ACQUIRE_ANY`).
- **The socket cap itself**, minted by `net-stack` per open - the client's authority to use that one
  socket, and nothing more.

## 7. Non-goals / next

- **First slice.** One fixed demo (send + report). A general `sock <ip> <port>`, receiving on a
  socket, and binding a local port are the next steps.
- **No raw ICMP/TCP sockets yet.** UDP first; TCP sockets ride the same mechanism once TCP lands.

## 8. Conformance

Conforms to `0_conventions.md`: `sock version` / `sock help`, words-not-flags, raw facts. Pinned by
`osdev test shell` (open + invoke a socket capability, and `net`'s tab-completion adjusted for the new
`so`-prefixed verb).

**Rule 10: opening the socket is `q`-escapable.** It goes through the shell's net-stack transaction
helper, which polls `q` while it waits, advertises `(q to quit)` once the wait lingers, and gives up
after 20 seconds. It used to block in the syscall with no way out (`backlog/29`).
