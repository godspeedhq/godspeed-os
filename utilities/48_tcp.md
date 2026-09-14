# Utility: `tcp` - one TCP transaction

**Status:** Built, QEMU-verified. Part of the `feat/tcp` work; see `docs/tcp-design.md`.

    tcp <ip> <port> [text]

Open a TCP connection to `<ip>:<port>`, send `text` if given, print whatever comes back, and close.

## Why one transaction and not a session

net-stack drives the connection from inside the request that asked for it. It cannot yet run a
connection in the background, and the reason is written down rather than being a limitation anyone has
to rediscover: net-stack receives client requests and nic-driver replies on **one untagged endpoint**,
so unsolicited driver traffic consumes client messages (`docs/net-tags-design.md` §1). Background
connections, `listen`, and a long-lived session all wait on that document's phases 2 and 3.

What this does exercise is the whole protocol: the three-way handshake, sequence and cumulative
acknowledgement, retransmission with a backed-off timer, the peer's window, and the FIN exchange.

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
2 KiB send and a 2 KiB receive arena, and at most four connections exist at once. The advertised
receive window is the free space in that receive arena, so the peer is told exactly what this machine
can hold (`docs/tcp-design.md`).

## Conventions

Obeys `utilities/0_conventions.md`: `tcp help` prints usage, arguments are words rather than flags,
and the command reports raw facts without editorialising. Its arguments are an address and a port,
never a path, so it is in the shell's `NO_PATH_CMDS` (rule 9).
