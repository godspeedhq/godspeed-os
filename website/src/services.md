# The services, and how they reach each other

Everything above the kernel is a service: an isolated address space, a contract, a capability table,
and one receive endpoint. Nothing here shares memory with anything else. Every arrow on every diagram
below is an **IPC endpoint** - a message copied by the kernel from one address space into another,
under a capability that had to be granted (§8, §7).

This page is the map. It is written from the supervisor's own `IMAGES` table, which is the single
place a service's name, image, core, peers, privileges and device class are declared.

---

## How a connection is made at all

A service does not "find" another service. It is **wired** to it, or it **asks for it by name** and
the kernel mints a capability - and nothing else works.

```
   SPAWN TIME (the supervisor wires it)
   ┌────────────┐                              ┌────────────┐
   │ supervisor │──Spawn(image, peers:["fs"])─▶│   kernel   │
   └────────────┘                              └─────┬──────┘
                                                     │ mints a SEND cap to
                                                     │ fs's endpoint, into
                                                     ▼ the new task's table
                                               ┌────────────┐
                                               │   shell    │ holds cap → fs
                                               └────────────┘

   LATER (fs restarts; the shell's cap is now stale)
   ┌────────────┐   send ──▶ EndpointDead      ┌────────────┐
   │   shell    │──AcquireSendCap("fs")───────▶│   kernel   │ name directory
   └────────────┘   ◀── fresh cap, new gen ────└────────────┘ fs → EndpointId
```

Two properties follow, and they are the reason the rest of this page is shaped the way it is:

- **Identity is stable, location is not** (invariant 11). A restarted `fs` is a different task on a
  possibly different core with a new endpoint generation. Clients see `EndpointDead`, reacquire by
  name, and carry on. Nobody tracks where anything runs.
- **Authority is explicit** (invariant 1, 3). A service can only reach a peer it was granted, or one
  it declared in its contract and reacquired by name. There is no ambient directory to browse.

---

## The whole system

Arrows point from the caller to the service it sends to.

```
                       ┌──────────────┐
                       │    kernel    │  spawns exactly one thing
                       └──────┬───────┘
                              │
                       ┌──────▼───────┐
             ┌─────────│  supervisor  │─────────┐  spawns everything else,
             │         └──────┬───────┘         │  restarts what dies
             │                │                 │
     ┌───────▼──────┐  ┌──────▼──────┐   ┌──────▼──────┐
     │   control    │  │    shell    │   │   logger    │
     │ (COM2 ops)   │  │  (the user) │   │             │
     └──────────────┘  └──┬───┬───┬──┘   └─────────────┘
                          │   │   │
        ┌─────────────────┘   │   └──────────────┐
        │                     │                  │
  ┌─────▼─────┐         ┌─────▼─────┐      ┌─────▼─────┐
  │    fs     │         │  console  │      │   time    │
  └─────┬─────┘         │ (display) │      └──┬────────┘
        │               └───────────┘         │
  ┌─────▼────────┐                      ┌─────▼──────┐
  │ block-driver │                      │ net-stack  │
  └─────┬────────┘                      └─────┬──────┘
        │  (ARM only)                         │
  ┌─────▼────────┐                      ┌─────▼──────┐
  │ dwc2 / xhci  │                      │ nic-driver │
  │  USB host    │                      └─────┬──────┘
  └──────────────┘                            │ (ARM: through USB)
                                        ┌─────▼──────┐
                                        │   dwc2     │
                                        └────────────┘
```

On x86 `block-driver` talks to an AHCI controller through its own MMIO capability and has no USB peer
at all; on the Pi 2 and Pi 4 the disk is a USB device, so it goes through the USB host service. Same
service, same block protocol, different peer - which is the point of naming peers rather than devices.

---

## The services

### `supervisor` - restart authority and the image catalogue

```
   kernel ──spawn (the ONE direct spawn)──▶ supervisor
                                              │
        ┌─────────────────────────────────────┼──────────────────────────┐
        │ IMAGES[]: name, ELF, core, peers,   │  death notification      │
        │           privileges, device class  │  ◀───────────────────────┤
        ▼                                     ▼                          │
     Spawn(image, class, bdf) ──▶ kernel   respawn ──▶ kernel ───────────┘
```

Trusted, and **restartable**: when it dies the kernel respawns it, unconditionally and forever, and it
reconciles by adopting the services still running rather than duplicating them. The only unkillable
thing in the system is the kernel (§6.2, §6.3).

**Peers:** none. Everything reaches *it*, not the other way round.

### `shell` - the user's interface, and a capability broker

```
   keyboard ──▶ kernel console ring ──▶ shell ──▶ console  (what you see)
                                          │
                     ┌────────────────────┼──────────────┬──────────┐
                     ▼                    ▼              ▼          ▼
                    fs              block-driver       time     supervisor
              (files, pipes)         (drives)        (clock)    (spawn/kill)
```

There is no `stdin` and no `fork`. A pipe `A | B` is the shell creating an endpoint and granting one
end to each side (Appendix D.3). A killed shell respawns as a fresh prompt - the in-flight command is
lost, the session is not.

**Peers:** `fs`, `block-driver`, `time`, `console`, `logger`, `supervisor`.

### `fs` - the filesystem, and files as capabilities

```
   client ──Open("/doc.txt", READ)──▶ fs ──resource_mint──▶ kernel
          ◀──────── a real kernel capability ──────────────┘
                                     │
   client ──send ON THE FILE CAP──▶ kernel ──badged (resource_id, right)──▶ fs
                                     │
                                     ▼
                              block-driver (512-byte blocks, tagged req/reply)
```

A file **is** a capability (§7.10): unforgeable, non-escalating, revocable by generation bump. `fs`
commits metadata through a crash-consistent redo journal, so its death is a restart rather than a
reboot - which is what took it out of the trusted computing base.

**Peers:** `block-driver`, `logger`.

### `block-driver` - blocks, and nothing above them

```
   fs ──[op, lba, data?]──▶ block-driver ──▶ AHCI  (x86: own MMIO cap + DMA)
      ◀──[status, data?]───              └──▶ dwc2 / xhci  (ARM: over IPC)
```

It knows sectors, not files. On ARM the disk is behind the USB host service, so a peer restart makes
its capacity **temporarily unknowable** - and *unknowable* is answered with an error, never with "no
disk". Publishing zero sectors during a peer restart is how a filesystem gets mounted against nothing.

**Peers:** `dwc2` (ARM only).

### `xhci` / `ehci` / `dwc2` - the USB host controllers

```
   kernel ──MMIO cap + DMA arena + IRQ route──▶ xhci
                                                 │
                  ┌──────────────────────────────┼────────────────┐
                  ▼                              ▼                ▼
             keyboard ──CONSOLE_PUSH──▶ shell   hub          mass storage
                                                              │
                                                    block-driver ◀┘
```

Ring-0 code parsing descriptors supplied by whatever was plugged in is exactly what §4.4 forbids, so
these are ordinary services holding only what their contract grants. **They cannot reboot the machine**
- `REBOOT` lives with the shell alone (SEC-2). Where an IOMMU confines them, a compromise is bounded to
the granted DMA arena and they leave the trusted computing base entirely (§6.4).

### `nic-driver` and `net-stack` - the network

```
   net-stack ──frames──▶ nic-driver ──▶ e1000 / RTL8168 (x86, own MMIO cap)
       │                            └──▶ dwc2 (Pi 2, USB ethernet)
       │                            └──▶ GENET (Pi 4, on the SoC)
       ▼
   ARP · IPv4 · ICMP · UDP · DHCP · DNS · SNTP
       │
       └──▶ a socket is a capability (the same mechanism as a file)
```

The kernel gains nothing from networking: it routes messages, and a socket is a delegated resource
capability owned by `net-stack`. There is no ambient network any more than there is an ambient
filesystem.

**Peers:** `net-stack` → `nic-driver`, `time`.

### `console` - the terminal

```
   any service ──text──▶ console ──▶ framebuffer (grid, cursor, scroll, ANSI/CSI)
                                       ▲
   kernel bootcon ────────────────────┘  boot + panic only, then hands over
```

1,172 lines of terminal emulation that used to live in the kernel. What the kernel kept is a blit that
cannot format, position or scroll - because a panic halts every core including this service, so it
cannot ask a service to report it (§11.4).

**Peers:** none - it is written *to*.

### `time` - the wall clock

```
   shell ──▶ time ──▶ net-stack (SNTP)
                 └──▶ fs (persist a clock floor across boots)
```

**Peers:** `fs`, `net-stack`.

### `logger` - a broker, not a store

```
   any service ──▶ logger ──▶ serial + the kernel ring buffer
                       └────▶ the `trace` ring (IPC events, in-memory)
```

Stateless on purpose. A logger that persisted would depend on `fs`, which would make observing a
storage failure depend on storage.

**Peers:** none.

### `hw-enumerator` - hardware discovery in userspace

```
   supervisor ──"which device is class 0x0C0330?"──▶ hw-enumerator
              ◀──────────── BDF ─────────────────────    │
                                                    PCI_CFG: read ONE
                                                    config register,
                                                    select+fetch atomic.
                                                    No write. Ever.
```

It holds the narrowest hardware capability in the system. It cannot write configuration space at all -
that would be write access to every BAR of every device on the bus.

**Peers:** none - the supervisor asks it.

### `control` - the operator channel

```
   host ──COM2──▶ control ──▶ supervisor   (KILL / RESTART, for tests and operators)
```

**Peers:** `supervisor`.

---

## What a restart looks like from the outside

The single most important behaviour to understand, because every service depends on it:

```
   t0   shell ──send──▶ fs                         works
   t1   fs dies. kernel bumps its endpoint generation, drains the queue,
        clears its name from the directory
   t2   shell ──send──▶ EndpointDead               the stale cap fails loudly
   t3   supervisor respawns fs (possibly on a different core), which
        re-registers its name
   t4   shell ──AcquireSendCap("fs")──▶ fresh cap  and carries on
```

Between t1 and t3 the name does not resolve - a few hundred milliseconds, measured at 312 ms worst on
a Pi 2 and 1,680 ms on a T630. A client that treats that window as *"the peer is gone forever"* rather
than *"ask again"* is the shape of most of the bugs this system has had.

**Reacquiring the endpoint is necessary but not sufficient** (§14.3). Anything derived from the dead
instance - an open-file capability, a socket, a cached capacity - was issued by an instance that no
longer exists. It must be re-derived, not remembered.
