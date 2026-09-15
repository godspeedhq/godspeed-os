# Utility: `serve` - accept one TCP connection

**Status:** Built. Verified end to end in QEMU (`scripts/tcp_serve_test.py`), checked on both sides -
the guest's own report and the bytes the test machine received over a socket outside the guest. That
test also runs `serve` twice on the same port, which is the regression for the listener leak below.

    serve <port> [for]

Listen on `<port>`, accept ONE connection, print what arrives, echo it back, close.

**Waits until you press `q`.** A duration bounds it instead: `30s`, `5m`, `2h`, `1d`, or a plain
number of seconds.

    serve 8080           wait until q
    serve 8080 5m        give up after five minutes
    serve 8080 90        or after ninety seconds

The first version of this capped the wait at 30 seconds, which is shorter than walking to another
machine and typing - a server that stops listening because a timer ran out was not listening when
somebody called. A duration is now something you ask for, not something imposed on you.

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
| `[for]` | optional. How long to wait: `30s`, `5m`, `2h`, `1d`, or a plain number of seconds. Omitted, it waits until you press `q` |

## Output

```
gsh> serve 8080
listening on 192.168.4.37:8080 - waiting for one connection (q aborts)
still listening - 10s (q aborts)
accepted a connection
received 11 byte(s): knock knock
echoed 11 byte(s) back
closed
```

The address is asked of `net-stack` rather than remembered, so it is the one the stack holds now and
cannot drift from a changed lease. `net-stack` logs the other side of the same story - an inbound SYN
by address and port, the handshake completing, and a connection refused for want of a slot - so "the
SYN never arrived" and "the SYN arrived and we never answered" are not the same silence.

Bytes outside printable ASCII render as `.`, so a peer cannot spray control codes at the terminal.

## Bounds

Waits for as long as you asked - by default until `q`, `Q` or ESC - and at most 10 seconds for an
accepted peer to send something. A duration over a year is refused as a typo rather than honoured.

While it waits it says so every ten seconds, because two minutes of a mute prompt is
indistinguishable from a wedged one.

At most `MAX_LISTEN` ports are listened on across the whole system, and an accepted connection takes
one of the connection slots - so a machine already holding its maximum connections refuses new ones
by dropping the SYN (saying so in its log) and the peer retries.

### The port is RELEASED, on every exit

Closing the listener is an explicit operation, and `serve` performs it whether it finished, timed
out, or you pressed `q`.

**This is not automatic and its absence was a real leak.** Dropping the client's capability does not
tell net-stack anything - the kernel revokes the holder's authority, but net-stack's listener table
is its own state and nothing walks back to it from a dropped cap. Found on a Raspberry Pi 2: the
second `serve 8080` was refused, and stayed refused until the service restarted. With `MAX_LISTEN` at
2, that leak is two runs deep. Connections are released the same way - net-stack reaps one that has
finished and revokes it, so the holder's next call gets `CapRevoked` from the kernel.

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
| nobody connected in time | `nobody connected within <n>s`, when a duration was given |
| you pressed `q` | `serve: aborted`, and the port is released |
| the peer connected and sent nothing | `the peer connected but sent nothing` |

## Conventions

Obeys `utilities/0_conventions.md`: `serve help` prints usage, the argument is a word rather than a
flag, raw facts without editorialising, and `q` escapes every blocking wait. Its argument is a port,
never a path, so it is in the shell's `NO_PATH_CMDS` (rule 9).
