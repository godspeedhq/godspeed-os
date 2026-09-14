# TCP for GodspeedOS: the design, and the two facts that shaped it

> **Status: design, being built** on `feat/tcp`. `docs/networking.md` recorded the commitment on
> 2026-07-04 ("TCP/IPv4 is committed from the start, not far-future"); this is how it gets built, and
> why it is not a port of anyone else's stack.

## What is already here, measured rather than remembered

`services/net-stack/src/main.rs` is 2,174 lines and implements ARP (resolve and reply), IPv4 with
checksum, ICMP echo, UDP, DHCP (discover / request / lease), DNS and SNTP. All hardware-proven on four
boards.

**There is no TCP.** The only occurrence of the string in the service is its own header comment,
claiming "ARP/IPv4/ICMP/UDP/TCP over those frames". That comment is corrected in the same change as
this document: a file that asserts a protocol it does not implement is the defect this project spent a
week removing from its checkers, and it should not survive in the service being extended.

## The two facts

### 1. The serve loop blocks, and TCP cannot live in it

`service_main` ends in `loop { let req = ctx.recv(); ... }`. The service does nothing until a client
asks, then performs a **complete blocking protocol exchange** and replies. That is right for what it
does today: a ping, a DNS lookup and a DHCP lease are each one bounded round trip, and the client is
waiting for exactly that answer.

A TCP connection is not a round trip. It must:

- make progress across many client turns, holding sequence state between them;
- **make progress when no client is asking at all** - a retransmission timer fires, an incoming
  segment needs acknowledging, a peer's FIN needs answering;
- do that for several connections at once.

None of those can happen inside a call that only runs when a client speaks first. The service's own
comment already predicted this, about a different symptom:

> "the in-loop dance still blocks this service while it runs ... Making the dance incremental so
> net-stack answers THROUGHOUT it is the real fix, and that is a rework of the state machine rather
> than a constant."

This is that rework, arriving because TCP forces it.

### 2. Socket-as-capability is real, and is exactly the right shape

`op 2` mints a delegated resource capability (§7.10 - the same mechanism `fs` uses for a file), grants
it to the client, and the kernel badges each invocation with the socket's `ResourceId` so net-stack
knows which socket without the kernel knowing what a socket is.

That model needs no change for TCP. What changes is the state behind it: today a socket is
`struct Socket { rid: u64, port: u16 }` - ten bytes, no buffers - because a UDP exchange completes
inside one invocation. A TCP connection is a state machine plus buffers that outlive every call.

**A TCP connection is therefore a delegated resource capability, exactly as a file is.** Close is a
generation bump; a stale cap gets `CapRevoked`; authority to talk to a peer is a thing you hold, not a
number you guessed. That is the part of this that is not a port of anything.

## The architecture

**One bounded, non-blocking poll step, run every time round the loop, whether or not a client spoke.**

    loop {
        poll_step();                    // drain frames, advance every connection, fire due timers
        match recv_bounded(budget) {    // wake even in silence
            Some(req) => serve(req),    // enqueue intent / collect results - never a full exchange
            None      => {}             // the budget expired; that is normal, not an error
        }
    }

Three consequences worth stating because they are the whole design:

- **The client API stops performing protocol.** `connect` starts a handshake and returns; it does not
  sit inside the exchange. A client that wants to wait uses the deadline primitive it already has
  (`CallDeadline`, §8.2). This is mechanism, not policy: net-stack does not decide how long anyone
  waits.
- **Every poll step is bounded.** A fixed maximum of frames drained and connections advanced per
  iteration, so one busy connection cannot starve the serve path. Unbounded work here would be
  §26.6 broken in the one service most able to be flooded from outside the machine.
- **Nothing blocks that cannot be woken.** A stall in the poll step is a stall in the whole service,
  which is Commandment V ("nothing above the kernel may halt the machine") applied inward.

### Buffering: the window IS the bound

TCP is the business of holding data you cannot deliver yet. §26.6.1 forbids a heap by default and asks
for fixed arenas, and the two fit together better than they might sound:

> **Fixed per-connection arenas, a bounded connection table, and the advertised receive window is
> literally the free space in the receive arena.**

So TCP's own flow control *becomes* the enforcement of §26.6, instead of fighting it. The peer is told
the exact truth about what we can hold, and telling a peer the truth about capacity is Commandment VIII
at the protocol level. No allocator, no growth, and the maximum footprint is readable off the source.

Out-of-order reassembly gets a **small fixed number of held segments**, beyond which we drop and let
the peer retransmit. That is always-legal TCP and it is bounded. FreeBSD does far more; we should not.

## What we take from FreeBSD, and what we refuse

§26.14 is written for this and TCP is its sharpest case.

**Borrowed (facts about the protocol):** the state machine and its awkward corners (simultaneous open,
TIME_WAIT, half-close), which flag combinations are legal in which state, RTO estimation per
Jacobson/Karels with Karn's algorithm, fast retransmit and fast recovery, the reset rules, and the
hundred things RFC 793 leaves the reader to discover. Read as an executable RFC.

**Refused (properties of their design, not of the protocol):** mbuf chains, sockets as file
descriptors, a kernel-resident stack, callback timers, global tables, unbounded queues. Those are
BSD's answers to BSD's constraints. Here the stack is a restartable userspace service with no heap and
no ambient authority, and a socket is a capability.

Divergence gets recorded at the point of difference (§26.14), so the next reader knows it was a
decision.

## Phases

Each is independently testable in QEMU and stops at a point where the thing works.

| | |
|---|---|
| **P0** | Restructure: split the service into modules, introduce the poll step, keep every existing behaviour. Nothing new works; nothing old breaks. |
| **P1** | Segment parse and emit, the TCP checksum with its pseudo-header, and the bounded connection table. |
| **P2** | State machine: active open (connect), the three-way handshake, FIN and close, RST handling. |
| **P3** | Reliable transfer: sequence arithmetic, cumulative ACK, the retransmit queue, RTO per Jacobson/Karels with Karn. |
| **P4** | Receive path: in-order delivery, the bounded out-of-order queue, window advertisement and update. |
| **P5** | Passive open: listen and accept, so the machine can serve rather than only fetch. |
| **P6** | Congestion control: slow start, congestion avoidance, fast retransmit and recovery. |
| **P7** | The capability API and shell surface, and the identity/property tests that pin it. |

**Deliberately not in the first pass** (§26.2 - features are pulled into existence): window scaling,
SACK, timestamps, PMTU discovery, delayed-ACK tuning. Each is real complexity and each should arrive
when a test needs it, not because a reference implementation has it.

## How it is tested, and why the test does not take the stack's word for it

`osdev` already runs QEMU with an **e1000 on a user-mode (SLIRP) backend**, and already attaches a
`filter-dump` writing every frame to `build/net-tx.pcap`. That gives two independent instruments:

1. **The guest's own report** - what the shell and net-stack say happened.
2. **The pcap** - what actually went on the wire, decoded on the host.

Only the second can catch a stack that believes it sent something it did not, which is the failure mode
a self-reporting network test is blind to. Assertions are written against the pcap: a SYN with the
right flags, a SYN-ACK accepted, sequence numbers that advance correctly, a FIN exchange that
completes, and a retransmission that actually appears on the wire when a segment is dropped.

SLIRP routes outbound TCP, so the guest connects to a server on the **host at 10.0.2.2**. A small
host-side echo and sink server gives real peer behaviour - real ACK timing, real window updates, real
resets - instead of a mock that agrees with us.

## The one hazard this inherits

`backlog/27`: `ctx.duration_cycles(ms)` returns **1** when the TSC is uncalibrated, so any deadline
built from it collapses to now. A TCP poll loop paced that way would spin at 100%, and an RTO computed
that way would be meaningless.

So the poll loop checks `tsc_ticks_per_10ms()` **once, explicitly**, and if there is no clock it says
so loudly and paces on yields instead of a deadline. TCP timers genuinely require a clock; on a port
without one the honest report is that retransmission timing is unavailable, not a silently wrong
number. That is recorded here rather than discovered later.

---

# Where this stands (2026-09-14)

## Working, and verified on the wire

`tcp <ip> <port> [text]` performs a complete TCP transaction. `scripts/tcp_qemu_test.py` boots QEMU,
runs it against a real echo server on the host, and checks the result twice: once from the guest's
output and once by decoding `build/net-tx.pcap`, which QEMU writes outside the guest entirely.

All 17 assertions pass, over two connections including one whose reply spans several segments:

    guest   single-segment echo returned; 2884-byte reply reassembled in order
    host    both connections accepted; both payloads received intact
    wire    SYN and SYN-ACK per connection, six data segments (two over 1000 bytes),
            four FINs, no RST, and the peer acknowledging our ISS+1

Implemented: segment parse and emit, the TCP checksum with its pseudo-header, the active-open state
machine through to TIME_WAIT, cumulative ACK with the send arena draining behind it, in-order
delivery, a bounded out-of-order queue, window updates, retransmission with clamped backoff, and RTO
estimation per RFC 6298 with Karn's algorithm.

## The prerequisite this design missed, and the plan it changes

**A background TCP engine is blocked**, and not by effort. `docs/net-tags-design.md` records that
net-stack receives client requests and nic-driver replies on **one untagged endpoint**, so any
unsolicited driver traffic consumes client messages. An idle tick was tried here before, caused
exactly that, and was reverted; the comment it left says *"fix the correlation BEFORE adding a tick,
not after"*. The poll step specified earlier on this page is that tick.

So the phase order changes. What was P0 is now this, and everything after it depends on it:

| | |
|---|---|
| **next** | `docs/net-tags-design.md` phase 3 - the bounded stash. A client request met while awaiting a driver reply is KEPT and served after, instead of dropped. Built on the discriminator that already exists (a client request carries a reply cap; a driver reply does not), so it does NOT need the 40-edit wire-tag change that document warns against doing in one pass. Sized: a `&mut` threaded through 15 `nic_req` call sites, every one compiler-checked |
| then | the poll step, and connections that progress with no client asking |
| then | listen and accept, so the machine can serve rather than only fetch |
| then | congestion control: slow start, congestion avoidance, fast retransmit and recovery |

Until the stash lands, one transaction per request is the honest ceiling, and `utilities/48_tcp.md`
says so where a user would otherwise wonder.

## What the tests caught, recorded because each is a class rather than an incident

- **A drain reply is a batch**, `[count, (len_u16le, frame) x count]`, not a bare frame. Treating it
  as one meant the peer's SYN-ACK arrived six times and was parsed as garbage six times. The guest's
  own log could only say "nothing came back"; the pcap said exactly what had happened. This is the
  argument for the second instrument, in one incident.
- **The window is the arena, and that only works with window updates.** A reply larger than the arena
  shuts the window; reading reopens it; a peer that is not told stays throttled. Found by asking for
  a reply bigger than the buffer, which the first version of the test never did.
- **A fix that cannot fire.** The first window-update threshold was one MSS, which with MSS 1460 and
  a 2048-byte arena is unreachable by construction. It looked right, changed nothing, and the
  symptom was unchanged. The rule is min(MSS, RCV_BUF/2).
- **Two of the three failures the harness reported were the harness.** It closed the serial socket
  before the content line arrived, and later compared against a hand-computed length that was wrong
  by four bytes. Both are now commented at the site, and the expected length is derived rather than
  restated.
