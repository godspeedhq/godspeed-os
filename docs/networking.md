# Networking: a Capability-Mediated Userspace Service

> **Status:** Design, being built (branch `feat/networking`). **v2** (networking is out of v1 scope -
> §23.4). Non-normative until built and pinned by an identity test, at which point the relevant
> decisions are amended into `CLAUDE.md`. This doc records the architecture and the phased plan,
> mirroring `docs/persistence.md`.

> **Decision (2026-07-04, owner): TCP/IPv4 is committed from the start, not far-future.** The build
> order is unchanged - the layers still stack NIC -> ARP -> IPv4 -> ICMP (ping) -> transport - but TCP
> is now a first-class goal of this effort, not a deferred maybe. The motivation is Commandment VIII
> made literal: TCP *is* "wait on truth, including the truth of failure" - an ACK is a truth, a peer's
> RST is a truth, and a retransmission timeout that fires is failure surfaced, not a guess. (TCP's own
> protocol timers - RTO, retransmission, TIME_WAIT - are *protocol-mandated timing*, the same exempt
> category as USB/AHCI hardware timing, not the correctness-by-timing that VIII forbids.) UDP (Phase 3)
> stays in as TCP's stateless stepping-stone - it proves the socket-cap model with no state machine -
> and TCP (Phase 5, brought forward) builds the state machine, retransmission, and ordered teardown on
> that same socket-cap foundation. Everything below the transport layer (Phases 0-2) is shared and
> identical either way, so work starts there regardless.

---

## 1. Thesis

**Networking is a userspace service, and the kernel gains nothing.** A socket is a capability - exactly
as a file is a capability (§7.10, P2). The classic "this must live in the kernel" subsystem is, here,
entirely additive userspace: a **NIC driver** service plus a **network stack** service, both built from
capability mechanisms that *already exist*. No new kernel code.

That is not a happy accident - it is the constitution paying off. The kernel anti-scope (§4.4) forbade a
network stack inside the kernel; the capability model (§7) made resources delegable to services; the
delegated-resource-cap work (P2 / §7.10) already lets the kernel mint, route, and revoke caps for a
resource whose *meaning* is owned by a service (it did this for files without learning what a file is).
A network endpoint is just another such resource. The kernel will route opaque socket caps exactly as
it routes opaque file caps, never learning what a socket is - so §4.4 holds verbatim.

This is the headline property and the reason to be confident in the design: **the hardest "kernel"
subsystem in the book is, in GodspeedOS, a couple of ordinary restartable services.**

---

## 2. Why a service, not the kernel

- **§4.4 (kernel anti-scope):** "The kernel does not contain ... network stack." Settled law.
- **§3.1 (no ambient authority):** a network stack in the kernel would be ambient reach to the wire for
  anything that could call it. As a service holding the NIC caps, the network is reached only by holding
  a socket capability the stack minted - authority is explicit, not ambient.
- **§3.10 (30-minute whiteboard rule):** a kernel TCP/IP stack is tens of thousands of lines no single
  engineer fully holds. Out of the kernel, the stack is just another service we can keep small.
- **§6 (TCB):** in the kernel, a stack bug is a kernel bug. As an IOMMU-confined restartable service
  (like `block-driver` / `xhci`), a stack bug kills and restarts one service - it does not own the box.

The kernel's only involvement is what it already provides to every driver: MMIO/IRQ/DMA capabilities
(§12.3), IOMMU confinement (§6.4, H1), and the delegated-resource-cap routing (§7.10). All of it exists.

---

## 3. Why our own minimal stack (not a port)

Same reasoning as "our own filesystem, not ext4/btrfs" (`docs/persistence.md`; §3.3 identity over
authority, §3.10 whiteboard rule):

- A ported POSIX/BSD stack (lwIP, or even smoltcp's socket layer) brings **fd-and-ambient** semantics
  that fight the capability model - file descriptors, blocking `connect`, an implicit routing table,
  ambient port binding. Retrofitting capability mediation onto that is more work than writing the thin
  layer we want, and it imports a vocabulary (§B.4) we deliberately don't have.
- It is **too big to fully understand.** The value here is a stack a contributor can explain on a
  whiteboard, where every byte on the wire traces to a capability operation.

We write a **minimal, capability-native** stack: ARP + IPv4 + ICMP + UDP first; TCP far-future. `smoltcp`
(Rust, `no_std`) is a fine *reference* for wire formats and the TCP state machine - we may borrow its
parsing shape - but the **socket layer is ours** (caps, not fds) and the scope is "understandable,"
not "feature-complete."

---

## 4. Architecture

```text
  ┌─────────────────────────────────────────────┐
  │  Services (hold socket capabilities)         │   e.g. a future `timeserver`, `httpd`, the shell
  ├─────────────────────────────────────────────┤
  │  net-stack   (service)                       │   ARP / IPv4 / ICMP / UDP  [TCP far-future]
  │   - owns the host IP + ports (a resource)    │   mints + revokes SOCKET caps (§7.10)
  │   - routes datagrams <-> sockets             │
  ├─────────────────────────────────────────────┤
  │  nic-driver  (service, IOMMU-confined)       │   model-specific: e1000 (QEMU/Intel), T630 chipset
  │   - raw Ethernet frames in/out via DMA rings │   MMIO + DMA + IRQ caps (§12.3, §6.4)
  ├─────────────────────────────────────────────┤
  │  Kernel  (routes opaque socket caps only)    │   NO networking - delegated-resource-cap routing
  ├─────────────────────────────────────────────┤
  │  NIC hardware                                │
  └─────────────────────────────────────────────┘
```

Every arrow is IPC + capability-mediated:
- A service ↔ `net-stack`: the service holds a **socket cap**; `send`/`recv` go through it.
- `net-stack` ↔ `nic-driver`: a **NIC-agnostic frame interface** - "transmit this raw Ethernet frame",
  "here is a received frame". `net-stack` is therefore **independent of the NIC model**; swapping an
  e1000 driver for the T630's chipset is invisible above the frame interface.
- `nic-driver` ↔ hardware: MMIO descriptor rings + DMA buffers + a receive IRQ (§12.2).

Two services, one clean seam (raw frames) between them, and the kernel underneath touching none of it.

---

## 5. The NIC driver

A userspace driver service, structurally identical to `block-driver` (AHCI) and `xhci`/`ehci` (USB):

- **Capabilities (§12.3):** `hw_mmio` (the NIC's BARs), `hw_interrupt` (the receive IRQ line), and a
  **DMA arena** for the TX/RX descriptor rings + packet buffers.
- **IOMMU-confined (§6.4, H1):** the DMA arena is the driver's only reach into RAM; a compromise is
  bounded to it. So the NIC driver is **least-privilege and restartable**, and on an IOMMU machine it
  is **not in the TCB** (same posture as the confined USB drivers). All `unsafe` lives behind the SDK's
  audited `Mmio`/`Dma` wrappers (§18.1) - the driver itself is `unsafe`-free.
- **Model-specific, like AHCI.** The dev driver is **e1000** (Intel 82540EM) - exhaustively documented
  and emulated by QEMU (`-device e1000`). (`virtio-net` is simpler but paravirtual-only - it cannot run
  on bare metal, so it is not the dev target.) **Phase 0 identified the T630 as a Realtek RTL8111/8168
  (`10ec:8168`), NOT Intel** - and QEMU has no RTL8168 model (it emulates the *old* rtl8139, e1000,
  virtio-net, but not the 8168). So the two drivers are genuinely separate: **e1000 in QEMU** carries
  all the stack development, and a **Realtek RTL8168 driver is Phase 4**, tested only on the T630 bench.
  The NIC-agnostic frame interface is exactly what makes this clean - the stack never knows the
  difference. This is the "agnostic stack, model-specific drivers" split being load-bearing, not
  theoretical.
- **Raw frames only.** The driver knows Ethernet framing and the ring/DMA mechanics; it knows nothing
  about IP. TX: a service hands it a frame, it enqueues a TX descriptor, rings the doorbell. RX: the
  receive IRQ (routed to the driver, §12.2) wakes it; it copies the frame out of the DMA buffer and
  hands it up. The frame interface is the entire contract.

### Phase 0 - identify the hardware

The kernel already enumerates PCI (`arch/x86_64/pci.rs`, used by `block-driver`), so the first concrete
step was a probe that **enumerates PCI and prints each NIC's `vendor:device` + BAR + IRQ** at boot.

> **Done + hardware-confirmed (2026-06-24).** `pci::init` now logs network controllers (class 0x02)
> alongside USB/AHCI. Flashed on the T630, it printed:
> `pci: NIC (network ctrl, subclass 0x00) at 01:00.0 vendor=0x10ec device=0x8168 MMIO=0xe000 IRQ=5` -
> **a Realtek RTL8111/8168 Gigabit NIC.** (QEMU's e1000 prints `vendor=0x8086 device=0x100e`, confirming
> the detection path in the shell-test harness.) One follow-up for Phase 1: on the RTL8168 BAR0 is an
> **I/O** BAR (bit 0 set) - the printed `MMIO=0xe000` is that I/O base; the real register MMIO is BAR2.
> Phase 0 should print all BARs and flag I/O-vs-memory so the driver grabs the right one.

Costs almost nothing and de-risked the hardware target: we now know Phase 4 writes a Realtek driver.

---

## 6. The capability model - a socket IS a capability (the centerpiece)

This is the part that makes networking *cheap* here, because the mechanism already shipped (P2).

`net-stack` **owns the network resource**: the host IP and the port space. There is **no ambient
network** - a service that holds no socket cap cannot put a byte on the wire (§3.1).

- **Open** (`bind` a UDP port, or `connect` to a peer): the service asks `net-stack`, which
  `resource_mint`s a fresh **socket capability** (a delegated resource cap, §7.10) with chosen rights
  (`SEND`/`RECV`), records `SocketId -> (proto, local, remote)`, and hands a narrowed copy to the
  service - the exact move `fs` makes on `Open`.
- **Send = invoke the cap.** The holder `send`s on the socket cap; the kernel validates it (generation
  + required right) and routes the message to `net-stack`'s endpoint **badged with the opaque
  `SocketId`** (the `LastRecvBadge` mechanism, §22 Test 14). `net-stack` builds the datagram and hands
  the frame to `nic-driver`. The kernel never learns this is a packet - it routes an opaque resource.
- **Recv.** Incoming datagrams are matched to a socket by `net-stack` and delivered to the holder's
  recv endpoint. No socket cap, no delivery.
- **Close = revoke.** `net-stack` `resource_revoke`s the socket (generation bump, §7.5); every
  outstanding cap to it goes stale - the next use returns `CapRevoked`. Identical to deleting a file.

So a socket cap has every §7.3 property a file cap has - unforgeable, non-escalating (rights narrow on
transfer), scoped to one `SocketId`, revocable, generationed. **"A socket is a capability" is literally
true, with zero new kernel mechanism.** The kernel's `resource_mint`/`Invoke`/`revoke` are
resource-agnostic; `net-stack` is simply a second `RESOURCE_MINT` holder alongside `fs`.

### The security upside (Appendix D.4, for the wire)

`curl evil.com` cannot exist unless something explicitly grants the requester a socket cap to that
destination. There is no ambient socket layer to reach. The shell (or a policy service) becomes the
deliberate place where outbound-network authority is decided - the same property D.4 describes for
`rm -rf /`, now for exfiltration. A compromised service with no socket cap is **network-mute** by
construction.

---

## 7. Addressing and identity

- **The host IP.** `net-stack` owns it. **Static config first** (an IP/mask/gateway baked into the
  stack's contract or a tiny config); **DHCP is far-future** (itself just a UDP client of `net-stack`).
- **Ports are resources** `net-stack` allocates. A UDP socket cap is authority over a `(proto, local
  port)` (and optionally a fixed remote); a future TCP socket cap is authority over one connection.
- **Identity over location (invariant 11).** A socket cap is stable identity; the NIC it egresses, the
  route it takes, even the host's L2 address are location. This is the same separation SMP made between
  service identity and core, and it is the on-ramp to **cluster mode** (Appendix C.4): `EndpointId ->
  (NodeId, CoreId)` and a remote socket are the same generalization - a capability whose target may live
  on another machine. We are not building that now, but the socket-as-capability model does not have to
  be redesigned to get there.

---

## 8. The stack - layered, phased

Each layer is small and testable; the milestone for each is a concrete wire event.

- **ARP** - resolve `IP <-> MAC` on the local segment (a tiny cache + request/reply). Needed before any
  IP frame can be addressed on the LAN. The shell surfaces it as `net arp <ip>` (resolve one host) and
  `net scan` (ARP-sweep the local /24 for live hosts).
- **IPv4** - parse/emit headers, checksum, fragmentation we **refuse loudly** rather than implement at
  first (datagrams over MTU are an error, §26.7), a single static default gateway (no routing table).
  IPv6 is far-future.
- **ICMP** - echo request/reply. **`ping` is the first end-to-end milestone** - the networking analogue
  of v1's ping/pong: proof the whole NIC -> ARP -> IP -> ICMP -> back path works on real wire.
- **UDP** - stateless datagrams. **The first socket-cap milestone:** a service opens a UDP socket cap,
  `send`s a datagram, `recv`s a reply. Stateless, so it survives a `net-stack` restart trivially.
- **TCP** - **far-future.** The state machine, retransmission, windows, and ordered teardown are the
  bulk of a real stack. Stateful, so a `net-stack` restart **drops** live connections (see §9). Worth
  doing only after UDP + the cap model are solid and there is a real consumer.

---

## 9. TCB posture and restartability

- **`nic-driver`** - IOMMU-confined -> least-privilege -> **restartable**, and **out of the TCB on an
  IOMMU machine** (§6.4, H1, same as the USB drivers). Its death reclaims its IOMMU/DMA resources; the
  supervisor respawns it; it re-inits the NIC and re-exposes the frame interface; `net-stack` reacquires
  it by name and retries (§14.3).
- **`net-stack`** - a restartable service.
  - **UDP is stateless:** a restart loses nothing structural. Sockets are caps; holders see
    `EndpointDead`/`CapRevoked`, reacquire `net-stack` by name, and re-`bind` - the §14.3 client-recovery
    pattern. The host IP is config, re-read on mount.
  - **TCP is stateful:** a `net-stack` restart **drops** live connections - the peer sees a reset, the
    holder reconnects. We deliberately do **not** build a "TCP journal" (the way `fs` got one for its
    metadata): TCP connection state is ephemeral *by the protocol's own design* (a reset + reconnect is
    a defined, normal event), so reconnection IS the recovery. This is §14.3 ("cascading failure is the
    client's responsibility") applied to the wire, not a robustness gap.
- **Loud failure (§26.7):** a dead NIC or stack surfaces as `EndpointDead`/`CapRevoked`, never a silent
  black hole. A datagram that cannot be sent fails loudly to the caller.

---

## 10. Failure semantics

| Event | Result |
|-------|--------|
| `send` on a closed socket | `CapRevoked` (generation mismatch) |
| `recv` on a dead `net-stack` | `EndpointDead` |
| UDP datagram sent | queued/transmitted, **not** delivery-guaranteed (like §8.6 IPC - the app builds acks if it needs them; for UDP that is the protocol's contract) |
| frame too large (no fragmentation, early phases) | error returned, **loud** (§26.7) |
| ARP resolution fails | send fails loudly after a bounded retry; never an indefinite hang (§26.6 bounded) |
| `net-stack` / `nic-driver` death | endpoint generation bumped; holders reacquire by name (§14.3) |

No silent fallbacks at any boundary. TCP's own ack/retransmit (far-future) is the *protocol* providing
reliability on top of this best-effort substrate - not the kernel, not magic.

---

## 11. Phased plan (mirrors persistence: ahci -> gsfs -> file-cap)

| Phase | Deliverable | Milestone / test |
|-------|-------------|------------------|
| **0** | PCI-enumerate + print the NIC (`vendor:device`, BARs, IRQ) | We learn the T630's chipset from one boot line |
| **1** | `nic-driver` (e1000): raw Ethernet TX/RX via DMA rings + RX IRQ, IOMMU-confined | Send a raw frame; receive a raw frame (host-side listener / loopback) in QEMU |
| **2** | ARP + IPv4 + ICMP in `net-stack` | **`ping` the host** end to end - the networking ping/pong |
| **3** | UDP + **socket-as-capability** (`resource_mint`/badge/revoke); a `net` shell utility | A service opens a UDP socket cap, send/recv a datagram; non-escalation + revoke pinned (a §22 "socket is a capability" test, mirroring Test 14) |
| **4** | The **Realtek RTL8168 driver** (`10ec:8168`, the T630's NIC), same frame interface - HW-only, no QEMU model | `ping` from bare metal on the T630 |
| **5** | (far-future) TCP | A TCP echo against a real peer |

Each phase gets a design beat in this doc plus QEMU + (where it applies) T630 verification, exactly like
the AHCI/GSFS/file-cap ladder.

---

## 12. Constitution alignment

| Decision | Section |
|----------|---------|
| Not in the kernel | §4.4 |
| No ambient network - socket caps only | §3.1 |
| Socket is a delegated resource cap (= file mechanism) | §7.10, P2 |
| NIC driver IOMMU-confined, restartable, TCB-droppable | §6.4 (H1) |
| Receive IRQ routed to the driver | §12.2 |
| Our own minimal stack, not a port | §3.3, §3.10 |
| Loud failure, bounded retries | §26.6, §26.7 |
| Client reconnect/reacquire on restart | §14.3 |
| Socket = stable identity, route = location | invariant 11; on-ramp to Appendix C.4 |
| Capability-security upside (no ambient exfil) | Appendix D.4 |
| Networking is v2 | §23.4 |

When a phase ships and an identity test pins it, the load-bearing decision is amended into `CLAUDE.md`
(e.g. a "socket is a capability" line beside the file-cap one), the way persistence and naming were.

---

## 13. Non-goals (scope discipline, §26.2)

- **No POSIX/BSD sockets API.** Capability-native (socket caps), not file descriptors. We are not
  source-compatible with anything; that is a feature (§B.3, D.6).
- **No full TCP/IP parity.** Understandable over complete. No options soup, no congestion-control
  research, no TLS in the stack (TLS, if ever, is a service on top of a TCP socket cap).
- **No routing/NAT/firewall.** A single host on one LAN segment with a static default gateway.
- **No networking in the kernel** - ever (§4.4).
- Pulled into existence by need, not by "a real OS has it" (§26.2) - which is why TCP waits for a real
  consumer and DHCP waits for a reason.

---

## 14. Open questions (for sign-off)

1. **The T630's NIC chipset** - **RESOLVED (2026-06-24): Realtek RTL8111/8168 (`10ec:8168`).** QEMU has
   no RTL8168 model, so dev is e1000 and the Realtek driver is Phase 4 (HW-only). See §5 Phase 0.
2. **Static IP first** (baked config), DHCP deferred - agreed? (DHCP is later, as a UDP client.)
3. **e1000 as the QEMU dev driver** - agreed? Note this is purely the dev NIC: the T630 is Realtek, so
   e1000 does NOT run on it (vs virtio-net, which can't run on bare metal at all). The HW driver is the
   Phase-4 RTL8168.
4. **IPv4 only** to start, IPv6 far-future - agreed?
5. **UDP + the cap model is the v2 milestone**; TCP is explicitly far-future - agreed on that boundary?
6. **One `net-stack` service** owning IP + all protocols, vs splitting (e.g. a separate `udp` service)?
   Recommendation: one `net-stack` for now (whiteboard-simple); split only if a real reason appears
   (§26.2).

---

## 15. Chaos and link resilience (hardware-proven, 2026-07-07..09)

> **Exercised on the T630 bench** (real RTL8111/8168, live LAN). Once the stack ran end to end on
> hardware - a real DHCP lease, gateway, and `ping` - the question stopped being *"does it work"* and
> became *"does it survive the two failures a real NIC actually sees"*: the **cable** moving, and the
> **services dying**. Both are now chaos-covered, held to the same discipline `block-driver`/`fs` got.

**Link flap - the cable is location, not identity (invariant 11).** A NIC's carrier goes up and down
independently of anything the OS does (someone unplugs the cable, the switch reboots). The stack treats a
link transition as a first-class, *observable* event, never a silent error (§26.7):

- **Self-configure on link-up.** When the driver observes carrier (the RTL8168 PHY reports link a few
  seconds after cable-in), `net-stack` runs its configuration dance automatically - ARP announce, then
  DHCP DISCOVER/REQUEST to a lease. No command, no reboot; plugging in the cable *is* the trigger.
- **`ping` rides an unplug/replug.** A running `ping` does not die when the cable is pulled - it reports
  the link down and **resumes** when the cable returns and the lease re-acquires, with an instant `q`
  abort throughout (the utility never blocks un-abortably on a dead link - Commandment VIII, and the
  "press q to abort" utility convention).
- **`net` reflects the *live* link.** `net` reads carrier + lease state at call time, so it never shows a
  stale "up" for a cable that is out.
- **`net renew` recovers in place.** A link that came up mis-configured, or a lease that expired, is
  repaired by re-running the configuration dance **inside the running `net-stack`** - no service restart,
  no reboot. Recovery is a re-configuration, not a re-spawn.

**Multi-target max-carnage.** The `chaos` utility (a userspace program) storms the networking services
exactly as it storms storage. `chaos max-carnage nic-driver,net-stack N` takes a **comma-separated target
list** and kills **every** named service each round, for N rounds (rounds uncapped - a count is not a
resource, §6.2/§26.6). Killing the NIC driver *and* the stack together, repeatedly, exercises the full
recovery order: the supervisor respawns `nic-driver` first, then `net-stack` reacquires it by name
(§14.3) and re-configures on the freshly-initialised link. The random-target variant picks its victims
from the live set, so no single kill-ordering is special-cased. Result on hardware: the link comes back
and `ping` resumes after every round, with no kernel panic - *"not even a blip."* This is the networking
half of the same restartability story the storage stack tells (`docs/persistence.md` §6.16,
`docs/naming-design.md` §8 risk #11): the system reconverges from any perturbation.
