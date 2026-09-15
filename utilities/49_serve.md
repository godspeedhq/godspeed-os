# Utility: `serve` - accept one TCP connection

**Status:** Built. Verified end to end in QEMU (`scripts/tcp_serve_test.py`), checked on both sides -
the guest's own report and the bytes this machine received over a socket outside the guest.

    serve <port>

Listen on `<port>`, accept ONE connection, print what arrives, echo it back, close.

## Why this is the interesting one

**This is the machine acting as a host rather than a client, and it is the first command that does.**
Everything else in the networking surface dials out: `ping`, `net dns`, `sock` and `tcp` all start the
conversation. `serve` answers one.

That needed the passive half of the TCP open - `State::SynReceived`, a listener that is not a
connection, and a SYN-ACK owed and sent by the poll step - none of which existed until it was written
(`docs/tcp-design.md`). It also needs the poll step itself: a connection accepted here makes progress
with no client asking, which is exactly what net-stack could not do before.

## One connection, then done

Deliberately. This demonstrates the passive-open path and the capability API around it; it is not a
daemon. A server that stays up is a service with a contract of its own, not a shell built-in - §26.2,
the preferred state of an unneeded feature is *not implemented*.

## Arguments

| | |
|---|---|
| `<port>` | 1 to 65535. The port to answer on |

## Output

```
gsh> serve 8080
listening on port 8080 - waiting for one connection (q aborts)
accepted a connection
received 11 byte(s): knock knock
echoed 11 byte(s) back
closed
```

Bytes outside printable ASCII render as `.`, so a peer cannot spray control codes at the terminal.

## Bounds

Waits at most 30 seconds for somebody to connect and at most 10 seconds for that peer to send
something; `q`, `Q` or ESC abandons the wait at any point. At most `MAX_LISTEN` ports are listened on
across the whole system, and an accepted connection takes one of the connection slots - so a machine
already holding its maximum connections refuses new ones by dropping the SYN, and the peer retries.

## What it is made of

Three capabilities, each minted by `net-stack` and held by the shell (§7.10, the same delegated
resource mechanism as a file and a UDP socket):

1. **The listener.** `net-stack` mints it when the port is granted. Closing it stops new connections
   and leaves connections already accepted from it running.
2. **The connection.** `accept` mints a second capability and grants it to the caller. From that
   point the connection is a thing the client HOLDS - reading needs `READ`, sending and closing need
   `WRITE`, and a stale handle gets `CapRevoked` from the kernel rather than a wrong answer from
   net-stack.
3. **A send cap to `net-stack`**, acquired by name.

A peer's MAC is taken from the frame its SYN arrived in, so an inbound connection needs no ARP at
all: the segment reached us from that address, which is the one fact that never has to be asked for.

## Failure

| what happened | what you see |
|---|---|
| the stack has no address yet | `net-stack would not listen on that port`, and net-stack's log says which of the three reasons it was |
| the port is already listened on, or every listener slot is in use | the same line; net-stack's log distinguishes them |
| nobody connected in time | `nobody connected within 30s` |
| the peer connected and sent nothing | `the peer connected but sent nothing` |

## Conventions

Obeys `utilities/0_conventions.md`: `serve help` prints usage, the argument is a word rather than a
flag, raw facts without editorialising, and `q` escapes every blocking wait. Its argument is a port,
never a path, so it is in the shell's `NO_PATH_CMDS` (rule 9).
