# Utility: `sock` - a UDP socket as a capability

**Utility:** `sock` - open a UDP socket capability and send a datagram through it
**Status:** Built (first slice). As-built reference.
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
sock: UDP socket cap - sent 29 bytes to 10.0.2.3:53, received 45 bytes back (a round-trip through a capability)
```

The datagram is a small DNS query (just data that elicits a UDP response); `sock` reports the
round-trip - bytes out and back - which proves the cap does real UDP I/O. When there is no NIC, the
invocation returns nothing and `sock` says so plainly.

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
