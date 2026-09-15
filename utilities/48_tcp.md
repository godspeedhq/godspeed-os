# Utility: `tcp` - one TCP transaction

**Status:** Built. **Hardware-verified on five machines** across four instruction sets and four
ethernet controllers (Raspberry Pi 2, Raspberry Pi 4, StarFive VisionFive 2, HP T630, Dell Wyse
5070), and in QEMU against a decoded packet capture. See `docs/tcp-design.md`.

    tcp <ip> <port> [text]

Open a TCP connection to `<ip>:<port>`, send `text` if given, print whatever comes back, and close.

## Why one transaction and not a session

net-stack drives the connection from inside the request that asked for it, so `tcp` is one complete
exchange: connect, send, read, close.

**This used to be BLOCKED, and it is not any more - it is simply unbuilt, which is a different thing
and worth stating precisely.** The blocker was that net-stack received client requests and nic-driver
replies on one untagged endpoint, so any unsolicited driver traffic consumed client messages; a
background connection was therefore impossible rather than merely absent
(`docs/net-tags-design.md` §1). That is resolved: the service now sifts what arrives on every driver
conversation (phase 2), keeps a displaced client request in a bounded stash rather than dropping it
(phase 3, §9), carries a correlation tag on the client hop so a late reply is discarded instead of
misread (§8), and runs a bounded poll step when no client is asking.

What remains is the API. A long-lived connection needs to be a thing a client can HOLD - a TCP
connection as a delegated resource capability, the way a file and a UDP socket already are - plus
`listen`/`accept` so the machine can serve rather than only fetch. Both are ordinary work now.

## What the protocol actually implements

The three-way handshake, sequence numbers and cumulative acknowledgement, the retransmit queue with
RTO estimation per RFC 6298 and Karn's algorithm, in-order delivery with a bounded out-of-order
queue, window advertisement and update, and the full close sequence through TIME_WAIT.

Plus, since the first hardware run:

| | |
|---|---|
| **maximum segment size** | advertised on every SYN and honoured from the peer's. Before this the stack advertised nothing, so every peer had to assume the RFC 1122 default of 536 bytes per segment |
| **congestion control** | RFC 5681: slow start, congestion avoidance, fast retransmit on three duplicate acknowledgements, and NewReno fast recovery. Sending is bounded by the smaller of the peer's window and the congestion window - the receiver's limit and the network's are different questions |
| **persist timer** | a shut receive window is probed, so a lost window update cannot deadlock two correct implementations |

**A startup self-test proves the parts the network cannot reach.** Congestion control reacts to loss,
fast retransmit to reordering, the persist timer to a closed window - none of which a healthy LAN or
the QEMU backend produces. So net-stack drives its own state machine against a synthesised peer when
it starts, using the same encoder and parser that serve the wire, and logs
`net-stack: tcp selftest PASS - 31 checks`. A guard nobody has seen fire is not evidence that it
works.

## Arguments

| | |
|---|---|
| `<ip>` | an IPv4 address in dotted form, for example `10.0.2.2`. Not a hostname - resolve it with `net dns` first, so that a name that will not resolve is a separate, visible failure from a peer that will not answer |
| `<port>` | 1 to 65535 |
| `[text]` | optional. Remaining words are joined with single spaces and sent as the request body |

## Output

    tcp: 13 byte(s) back
    hello, world.

Bytes outside printable ASCII render as `.`, so a binary reply cannot spray control codes at the
terminal. An empty reply is reported as **connected to nothing** rather than as silence, because
net-stack answered - the reason it produced no data is in its log, and the two cases are different
faults.

## Failure

Every failure is named rather than folded into "it did not work":

| what happened | net-stack logs |
|---|---|
| the peer refused, or reset an established connection | `peer reset the connection` |
| six retransmissions went unacknowledged | `no acknowledgement after 6 retransmissions` |
| no answer to the opening SYN | `no answer to our SYN` |
| the connection worked but returned nothing before the budget | `no data and no fault (budget expired)` |
| the network is not configured yet | `tcp asked for before the stack is configured` |

## Bounds

One transaction is capped at 8 seconds and at a fixed number of protocol steps; a connection holds a
2 KiB send and a 2 KiB receive arena, and **at most 2 connections** exist at once. The advertised
receive window is the free space in that receive arena, so the peer is told exactly what this machine
can hold (`docs/tcp-design.md`).

*(This said "at most four connections" until 2026-09-15. `MAX_CONNS` was reduced from 4 to 2 when
`service_main`'s stack frame reached 37% of the 256 KiB user stack on arm32, and the spec was not
brought with it - the kind of drift `scripts/facts_check.py` exists to catch.)*

## Conventions

Obeys `utilities/0_conventions.md`: `tcp help` prints usage, arguments are words rather than flags,
and the command reports raw facts without editorialising. Its arguments are an address and a port,
never a path, so it is in the shell's `NO_PATH_CMDS` (rule 9).
