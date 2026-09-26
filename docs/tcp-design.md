# TCP for GodspeedOS: the design, and the two facts that shaped it

> **Status: BUILT and shipping** (this header said "design, being built" until 2026-09-26, while the same document's own "HARDWARE VERIFIED" and "Working, and verified on the wire" sections said otherwise). `services/net-stack/src/tcp.rs` is 1,745 lines; `OP_LISTEN`, the shell's `tcp` and `serve`, `tcp selftest` (53 checks) and `gs::net::Net::tcp` / `Listener::accept` are all live. `docs/networking.md` recorded the commitment on
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

## HARDWARE VERIFIED - four instruction sets, 2026-09-14

| board | ISA | NIC | small exchange | 2884 bytes |
|-------|-----|-----|----------------|------------|
| Raspberry Pi 2 | ARMv7 | LAN9514 over USB (dwc2) | 200 ms -> 205 ms | 500 ms -> **237 ms** |
| Raspberry Pi 4 | AArch64 | GENET, on-SoC | 94 ms | 79 ms |
| StarFive VisionFive 2 Lite | riscv64 | dwmac | 189 ms | 189 ms |
| HP T630 | x86-64 | RTL8168 | 130 ms | 173 ms |
| Dell Wyse 5070 | x86-64 | RTL8168 | 142 ms | 79 ms + 1.39 s of ARP |

### Re-verified end to end after the queue-latency work (2026-09-16)

All five boards above were measured before `backlog/29` - three defects that made a command take
twenty seconds rather than fail, so the transaction numbers were right and the time to START one was
not. Re-run on the two x86 boards after the fix:

| board | `tcp ... big`, dispatch to reply | queue wait before dispatch | selfcheck |
|---|---|---|---|
| Dell Wyse 5070 | 189 / 252 ms | under 20 ms (was ~20 s) | **ran 461, failed 0** (see `backlog/31`) |
| HP T630 | 159 ms | under 20 ms | **ran 461, failed 0** |
| StarFive VisionFive 2 Lite | 204 / 78 / 174 / 205 ms (4 runs) | under 20 ms | **ran 461, failed 0** |
| Raspberry Pi 4 | **31 / 47 / 32 ms** (3 runs) | under 40 ms | **ran 461, failed 0** |
| Raspberry Pi 2 | 47 to 236 ms, median ~95 ms (8 runs) | under 20 ms | **ran 452, failed 0** |

The Wyse cell was filled afterwards and took two runs: `ran 462, failed 1`, then `ran 461, failed 0`
after a `kill net-stack`. The failure was real and is `backlog/31` - net-stack blocked for 48 seconds
inside one serve pass, waiting on a `nic-driver` that was alive and not answering, so DNS resolution
and `ping` both failed until its client was restarted. **A green second run does not retire that**, and
the entry says what is and is not established about it.

An earlier version of this cell said "n/a, no disk", and the distinction is a correction rather than a
nicety. Its `xhci: alive` line reports `disk no`, and that was read here as the machine having no
storage - but `xhci` only knows about USB, and the Wyse's disk is AHCI on `block-driver`, which its own
log shows working (`block-driver: op 5 spent 108630 us`, `fs: op 10 took 227238 us, 17 block ops`).
A driver reporting the absence of what IT can see is not the machine reporting an absence.

The three boards with a real disk report **the same 461 checks and the same 0 failures on three
different instruction sets** - x86-64 on AMD, riscv64, and AArch64 - with `drives check` and `drives
scrub` green on each. That is the portability claim in its strongest available form: not "it builds
everywhere" but one suite, one count, one result, across ISAs that share no arch code.

**A warning about reading these timings out of a serial capture, because it nearly produced a false
entry in this very table.** The Pi 2's numbers first read as a flat 31 to 48 ms - as fast as the Pi 4,
on the one board whose NIC is behind USB, and 7x better than its own previous figure. It was an
artifact. Those lines came from the `events` log ring being DUMPED and re-rendered, not from live
output, and the giveaway is the format: a dumped line is column-padded (`net-stack  `, `shell      `,
`fs         ` aligned to one width) where a live one carries a colon (`net-stack: `). The timestamps in
a dump are when the HOST received the repaint, so five transactions appear inside 400 ms and every one
of them "takes" the repaint interval.

**Take timings only from the colon form.** A figure well BELOW the other boards is a measurement
artifact until proven a difference - this table would otherwise have claimed the Pi 2 matched the Pi 4.

The Pi 4 is the fastest in the fleet by a wide margin - a 2884-byte transfer in 31 ms against 159 ms
on the T630 and 78 to 205 ms on the VisionFive - which is the GENET MAC being on-SoC rather than
behind USB or PCIe. Its slow passes (1584 to 4001 ms) sit in the same band as every other configured
board, which is the point: the blocking dance is architecture-neutral because `net-stack` is.

The T630 is the useful one here, for three reasons: it is AMD, so every timing bound in this service
calibrates through a different path; it has a real AHCI disk, so `check` and `scrub` run for real (461
checks against 349-354 on the diskless boards); and it had never run this branch at all - both earlier
x86 sessions booted the Wyse in its place. It needed no change.

**The VisionFive shows the fix ABSORBING the condition that used to break it**, which is better
evidence than a fast run. One of its `hello` commands arrived while net-stack was inside a 2006 ms
block:

```
19:46:18.115  op 21 reached dispatch (from the queue)
19:46:19.788    (q to quit)                              <- the wait lingers past 2 s, so it says so
19:46:20.008  net-stack: tcp 192.168.4.40:7777 ok - 10 byte(s)
19:46:20.034  net-stack: a serve pass took 2006 ms (over 1000)
```

Held, then served, 1.9 s end to end. **That is precisely the case that was dropped at 1.5 s and cost
twenty seconds before `backlog/29`.** Its three slow passes (3406, 1875, 2006 ms) are the same
configured-stack scale as the x86 boards, with zero drops and zero timeouts beside them.

**What it shows that a green run usually cannot.** Its log carries three genuinely slow serve passes -
1981 ms, 1557 ms and 4001 ms - and **zero dropped requests and zero timeouts beside them.** The
in-loop dance still blocks this service for seconds (`backlog/28`, open by design); what changed is
that a request displaced by one is now served the moment it ends instead of ageing out. A four-second
block used to be exactly what produced a discarded request and a twenty-second command.

**Every board after the first worked FIRST TIME, with no new bugs.** That is the portability
claim earning its keep: every one of the four hardware-only failures was in `net-stack`, which is
architecture-neutral, so fixing them on one board fixed the rest. Four instruction sets and four
entirely unrelated ethernet controllers, one set of fixes.

The T630 is the second-sharpest piece of evidence after the VisionFive: its RTL8168 is a NIC driver
QEMU cannot exercise at all (it emulates an e1000), so that path had never carried a TCP segment
before this run.

**ALL FIVE MACHINES PASS.** Four instruction sets, four ethernet controllers, and the same set of
fixes on every one.

### The protocol work, re-measured on the Pi 2 (2026-09-14)

The second figure in each row above is the same board, same cable, same host, after the maximum
segment size option, RFC 5681 congestion control and the persist timer landed. The small exchange is
unchanged; the 2884-byte one halved.

| | ARP | TCP | total |
|---|---|---|---|
| 10-byte echo | 158 ms | 47 ms | 205 ms |
| 2884-byte reply | 222 ms | **15 ms** | 237 ms |

**The TCP phase of the large transfer is 15 ms.** Both sides logged the same exchange, as always: the
board reported `ok - 2884 byte(s)` and the host `[20] from 192.168.4.64:49153 - 3 byte(s): b'big'`
followed by 2884 sent.

**What this is evidence of, stated carefully.** The explanation that fits is the MSS option: this
stack now advertises 1460 where it previously advertised nothing, so a peer that had to assume the
RFC 1122 default of 536 sends roughly two segments instead of six - and on dwc2 every segment costs a
USB round trip, which is why this board gains more from it than any other. That is INFERENCE from a
timing change, not proof: neither log shows an option on the wire. What is proven separately is that
the option is emitted at all, by `scripts/tcp_qemu_test.py`, which decodes it out of a packet capture
QEMU writes outside the guest and asserts its value. The mechanism is pinned; the attribution of this
particular speedup to it is not, and a capture on this board is what would close that.

Congestion control cannot be responsible: the board SENDS 3 and 5 bytes in these exchanges, so the
congestion window never binds. It is the inbound direction that got faster, which is governed by what
we told the peer it could send us.

Also verified here, and not reachable from any test that uses a network: `net-stack: tcp selftest
PASS - 31 checks`, 47 ms on ARMv7. Fast retransmit, the persist timer and the congestion arithmetic
react to loss, reordering and a shut window, none of which a healthy LAN or the QEMU backend
produces, so they are proven against a synthesised peer at startup on every board instead.

### One cost this measured, recorded rather than smoothed over

On the Wyse the large transfer took 1.47 s, of which **1.39 s was the ARP resolve** and 79 ms was the
TCP exchange. The on-link lookup runs once per transaction and can be slow on a cold cache, so it
dominates a short connection. It is correctness-neutral - the alternative is the gateway-routing bug
this replaced - but a cache keyed on the destination would remove it, and a background engine would
pay it once per peer rather than once per request. Not built, because nothing yet needs it (§26.2);
recorded so the next person reading a 1.5-second `tcp` does not go looking for it in the protocol.

The VisionFive is the sharpest of the three as evidence, because its networking has a history of
board-specific trouble (the VisionFive's dwmac unicast-loss repair) and this needed none of it.

Both boards were verified the same way - the board's log and the peer's log showing the same exchange,
which is worth more than either alone.



A Pi 2 (ARMv7, LAN9514 ethernet over USB/dwc2) completed TCP transactions across a real LAN to a
Windows peer. Both ends logged the same exchange, which is stronger than either log alone:

    board    net-stack: tcp 192.168.4.40 on-link at 44:1c:a8:e3:72:61 - direct
             net-stack: tcp 192.168.4.40:7777 ok - 2884 byte(s)
             tcp: 2884 byte(s) back
             BIG:0123456789abcdef...

    peer     [7] from 192.168.4.64:49152 - 5 byte(s): b'hello'
             [8] from 192.168.4.64:49153 - 3 byte(s): b'big'

Handshake, data in both directions, 2884 bytes reassembled across several segments with window
updates, and an orderly close. 0.2 s for the small exchange, 0.5 s for the large one.

### Four bugs QEMU could not have found

Every one of these passed the full QEMU suite and failed on hardware, and the reason is the same in
each case: SLIRP's only reachable peer IS the gateway, it answers in under a millisecond, and it
already knows our MAC.

| bug | why QEMU was blind to it |
|-----|--------------------------|
| no window update when the arena drained | the reply fitted the buffer, so the window never shut |
| the transaction loop spun through its budget in 0.4 s | a SLIRP peer answers faster than the loop can spin |
| ARP requests swallowed during a transaction | the gateway already had us cached from the boot dance |
| every frame addressed to the GATEWAY | the peer and the gateway are the same host there |

The last one is the sharpest: on SLIRP the right answer and the wrong one are byte-identical, so no
amount of QEMU testing could distinguish them. It took a second machine on a real subnet.

### And the lesson about diagnosis

Five theories were advanced from outside the machine - gateway MAC, IP identification and DF flags,
Malwarebytes, Windows Firewall, and gateway MAC again - and all five were wrong. Each was killed by a
measurement that took minutes: verifying checksums out of the capture, a pktmon trace naming
`Address resolution failure` in one line, and a transmit journal showing `SSSADDDD` with every frame
accepted by the driver.

Five instruments misreported along the way, three of them written for this very investigation: an
echo server whose log was block-buffered, a capture script that read UTF-16 as ASCII, `netstat -s`
counters swamped by 34,000 packets of ordinary traffic, a QEMU test run against a stale image, and a
harness that closed its socket before the line it was asserting on arrived. **An instrument is a
claim, and it needs checking like any other.**

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
| **done** | every conversation with `nic-driver` goes through a SIFTING wait, so a client request met during one is identified rather than consumed and mis-served. Dropped with its capability reclaimed and counted - `docs/net-tags-design.md` phase 2, which previously guarded one call site out of sixteen |
| **done** | a client displaced by net-stack's OWN unsolicited work (the clock nudge) is HELD for up to 500 ms and served, instead of lost. Scoped to that one region on purpose; net-tags §7.4 has the measurement and the reason the broad version was withdrawn |
| **done** | the protocol itself: a maximum-segment-size option on every SYN and the peer's honoured, RFC 5681 congestion control (slow start, congestion avoidance, fast retransmit, NewReno fast recovery), and the persist timer this page used to record as missing |
| **next** | correlation on the CLIENT hop: a tag net-stack echoes, so a client can discard a reply to a question it is no longer asking. This is the real prerequisite for deferring a request at all, and it is NOT the tag `docs/net-tags-design.md` describes - that one is for the driver hop |
| then | the bounded stash (net-tags phase 3), which needs the above. It was built, measured and withdrawn first; §7.2 there has the log that killed it |
| **done** | the poll step. While configured, the serve loop waits at most `POLL_MS` for a client and otherwise drains the NIC once, answering ARP and ICMP for itself and running one pass of the connection table. This is the tick that was REVERTED; it is safe now because the two phases above are in |
| then | listen and accept, so the machine can serve rather than only fetch |
| then | congestion control: slow start, congestion avoidance, fast retransmit and recovery |

Until the stash lands, one transaction per request is the honest ceiling, and `utilities/48_tcp.md`
says so where a user would otherwise wonder.

**The SDK change this predicted has been made, and it was the right one of the two.** net-stack could
not write its own send-and-await: `find_send_slot` and `await_slice` are private, and
`request_with_reply_deadline_outcome` does the send AND the wait in one call with no way to inspect
what arrives. The choice was between making those two public and adding a bounded await that hands
back messages it did not expect. The second was taken, because it keeps the policy in the SDK where
every other caller can reach it: `request_with_reply_deadline_sifted` and its millisecond twin ask the
caller about each message as it arrives and continue waiting on a no.

Two details of that primitive are load-bearing rather than incidental. The closure is called AT THE
MOMENT OF ARRIVAL, because `take_pending_cap` and `last_recv_badge` describe the message just received
and are overwritten by the next one - so the discriminator net-stack needs is only readable from
inside the wait. And it hands the message back rather than taking a disposition, because the thing
being handed back usually carries a reply capability that must be reclaimed or answered; deciding
which is policy, and policy does not belong in the SDK (§26.10).

## The machine answers for itself now (2026-09-15)

Two things this stack could not do, both of which are how one machine ordinarily checks another is
alive:

- **Answer ARP while idle.** All five ARP-reply sites were inside drain loops, so between commands a
  peer asking "who has this address" got nothing.
- **Answer a ping at all.** Every ICMP path built or matched our OWN outbound echoes; there was no
  echo-REQUEST handler anywhere, busy or idle.

The poll step fixes the first and `build_icmp_reply` the second. A reply is the request REFLECTED -
same identifier, sequence and payload, which is what makes the sender's matching work - with only
the direction fields changed and both checksums recomputed. It takes its length from the IP header
rather than the frame, so the ethernet minimum padding is not echoed back as extra data.

### Two guards that refuse to poll rather than poll wrongly

- **Unconfigured**: with no address of our own, nothing on the wire is ours to answer, so the loop
  blocks exactly as it did before. Verified: on arm32 under QEMU, which has no NIC, the poll never
  ran and the driver saw the same three messages as the pre-poll baseline.
- **No calibrated clock**: `duration_cycles` floors to one quantum when the counter is uncalibrated
  (`backlog/27`), so a bounded wait silently becomes a SPIN and this loop would ask the driver for
  frames as fast as it is scheduled. A missing clock means no poll, not a fast one.

### And one landmine defused on the way

`poll_one` emitted to `Net::peer_mac`, which is per CALL. A connection's peer MAC is per CONNECTION -
the peer's own when it is on-link, the gateway's otherwise - and getting that wrong is the bug that
cost a day on the Pi 2 (a handshake reaching Established and then silence, while `ping` to the same
host worked). It was fixed once, per transaction; the moment a connection outlives the request that
opened it, a background poll would have rebuilt `Net` from the gateway and reintroduced it. `peer_mac`
now lives on `Conn`, set at `connect`, so that is impossible rather than merely unlikely.

**What this is NOT proven to do yet.** SLIRP gives the host no route to the guest, so no QEMU test can
ping this machine. The poll's effect is proven on hardware or not at all; what QEMU proves is that it
breaks nothing (174/0 with the stack configured, so the poll is running throughout).

## The Raspberry Pi 4, and the bug it found (2026-09-15)

The second board to see the protocol work of this branch - MSS negotiation, RFC 5681 congestion
control, the persist timer, the client-hop correlation tag, the bounded stash, the poll step and
passive open. AArch64, GENET on-SoC ethernet.

| | Pi 4 | Pi 2, for comparison |
|---|---|---|
| ping, ARP cache cleared | 6/6, 33-189 ms, TTL 64 | 4/4 |
| `tcp hello` | ARP 16 ms, TCP **47 ms** | 205 ms total |
| `tcp big`, 2884 bytes | ARP 47 ms, TCP **16 ms** | 174 ms total |
| `serve`, three connections | 3/3 echoed | 2/3, twice |
| `tcp selftest` | 53 checks | 53 checks |

**No new bugs from the port.** The one defect it found is in neutral code and would have bitten every
board equally - the Pi 2 was hiding it behind its own slowness.

### A zero-length reply cannot be sent, and three paths tried to

Every `serve` close took five seconds, `FILTER_WAIT_SECS` to the millisecond, while the close itself
had plainly worked: net-stack reaped the connection 400 ms later, which only happens once it reaches
`Closed`. The echo before it took 11 ms.

The kernel's `validate_user_ptr` rejects `len == 0`, so a zero-length `try_send` fails, the reply
never leaves, and the caller waits out its entire deadline for a message that could not have been
sent. Silent at both ends: the sender discards the failed send, the receiver sees only a timeout.

`COP_CLOSE` replied with nothing, and so did the refusal path and the UDP socket path when a datagram
drew no answer. Every other reply in the service happens to carry a byte, which is the only reason
this took until the fourth board to surface. All three now answer with a status byte, and
`Reply::send` debug-asserts a non-empty body - deliberately asserting rather than padding, because
"no data" and "no answer" are different things and the sender should have to choose between them.

Confirmed by re-measuring the same three connections:

| | before | after |
|---|---|---|
| echo to close, on the board | 4916 ms | **201 ms** |
| client total, connection 2 | 4086 ms | **366 ms** |
| client total, connection 3 | 4195 ms | **379 ms** |

### The VisionFive 2, third instruction set and third NIC (2026-09-15)

riscv64, StarFive dwmac ethernet. Everything first time, no new bugs.

| | result |
|---|---|
| ping, ARP cache cleared | 8/8, 7-143 ms, TTL 64 |
| `tcp hello` | ARP 174 ms, TCP **16 ms** |
| `tcp big`, 2884 bytes | ARP 94 ms, TCP **16 ms** |
| `serve`, three connections | 3/3, echo to close 200/206/219 ms |
| `tcp selftest` | 53 checks |

The zero-length reply fix travelled: closes are the deliberate 200 ms sleep, not the five-second
timeout the Pi 4 exposed.

Worth noting for this board specifically: every ping reply and every TCP segment here is UNICAST, and
this is the hardware whose dwmac driver once dropped 60% of unicast frames
(the dwmac unicast-loss repair). 8/8 and two clean transfers say that repair is holding under
a protocol that did not exist when it was made.

### The same protocol work, three drivers

| | Pi 2, dwc2 | Pi 4, GENET | VisionFive, dwmac |
|---|---|---|---|
| `hello` TCP phase | 346 ms median | 47 ms | 16 ms |
| `big` TCP phase, 2884 bytes | 15 ms | 16 ms | 16 ms |

**The large transfer is 15-16 ms on all three.** Identical protocol work across three instruction
sets and three ethernet controllers, which is the portability claim measured rather than asserted.
The spread on the small transfer is entirely the driver: the Pi 2's dwc2 pays a USB round trip per
frame, and the other two do not.

### One theory retired

The Pi 2's small-transfer latency - a reproducible ~346 ms where a 2884-byte reply took 15 ms - does
NOT reproduce here: 47 ms for the same exchange against the same peer. So it is a dwc2 property, not
anything in the TCP close sequence, which is where the transmit-journal diagnostic had been pointing.

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
