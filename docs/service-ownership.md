# Who owns a service: images, authority, discovery, trust

**Status:** step A BUILT and hardware-verified on `feat/supervisor-owns-images`. Steps C, D and 2 are
design, agreed in discussion, not yet built.
**Merge bar (stated by the project owner):** this branch does not merge to `main` until the supervisor
actually owns the ELF images and the catalogue is out of the kernel. Step A alone does not earn it.

---

## 1. The problem, in one sentence

The kernel spawns only the `supervisor`, but it holds a `service_config` row for **every** service -
the embedded ELF, memory limit, placement core, send peers, hardware class and privileges - so
**adding a service is a kernel change.** That is backwards for a microkernel, and it bites on every
new service.

`COMMANDMENTS.baseline.toml` already names it as debt and refuses growth:

> "DEBT, NOT AN ALLOWANCE. This list may only ever SHRINK. Kernel responsibility does not expand, so
> there is no 'add it deliberately' path here: do not add a line."

The pin is not decorative. It **refused a new `tracer` service** during the trace work, which is what
forced the ring into `logger` instead. That was the right outcome, and it is the proof the pin works.

---

## 2. What step A actually did (BUILT)

The insight that made it cheap: **221 rows were never 221 programs.**

```
   BEFORE                                    AFTER
   =====================================     =====================================
   kernel/src/task/mod.rs                    kernel/src/task/mod.rs
   +---------------------------------+       +---------------------------------+
   | service_config()                |       | service_config()                |
   |                                 |       |                                 |
   |   29 real services .......  29  |       |   29 real services .......  29  |
   |   test probes ...........  193  |       |   test probes ............   1  |  <- ONE generic
   |                                 |       |                                 |     `probe` entry
   |   TOTAL ................   221  |       |   TOTAL ..............     29  |
   +---------------------------------+       +---------------------------------+
            2,317 lines                                   removed

                                             services/probe/src/table.rs
                                             +---------------------------------+
                                             | 193 rows of PARAMETERS          |
                                             |   name, mode, recv, mem, core,  |
                                             |   peers                         |
                                             +---------------------------------+
                                               shared BY SOURCE with the
                                               supervisor (both spawn probes)
```

Only **27 distinct ELFs** were ever embedded, and **193 of the 221 rows were the same `probe`
binary** differing by a test-mode number. One program plus a table of test parameters. A parameter is
policy, and policy belongs to a service (26.10).

### How the parameters travel: no new syscall, no change in arity

`Spawn` already packed `(cap_slot, core)` into the low 32 bits of `arg0` and left the upper 32 unused.

```
  arg0
   63        56 55      48 47          32 31        16 15            0
  +-----------+----------+--------------+------------+---------------+
  | reserved  |  flags   |  probe mode  |    core    | spawn cap slot|
  +-----------+----------+--------------+------------+---------------+
                  |  |  |
                  |  |  +-- bit 0  has_recv_endpoint
                  |  +----- bit 1  small memory limit (4 MiB)
                  +-------- bit 2  is-probe

  name argument:  "probe-4a\0probe-victim\0..."     <- NUL-separated: name, then peers
                  refused if over 128 bytes, never truncated
```

A silently shortened peer name would wire a probe to the wrong service and read as a passing test, so
an over-long payload is **refused**, not trimmed.

### What deliberately did NOT move: authority

Two probes are configured by authority rather than by parameter, and they stayed in the kernel:

| probe | needs | why it is authority |
|---|---|---|
| `probe-11a` | IRQ 33 routed to its endpoint | routing an interrupt line is a grant |
| `probe-5a-send` | peer caps minted with `GRANT` | handing out a re-delegatable capability is a grant |

> **The line this draws, and which the rest of this document keeps:
> the kernel decides what a service may DO; the caller says what it IS.**

### The prerequisite nobody would guess

Task names were `[&'static str; MAX_TASKS]`. That quietly required every task name to be a string
literal compiled into the kernel - and **that is why the catalogue existed at all**: a caller-supplied
name had nowhere to live.

They are owned bytes now, in `smp::names::NameTable`. The layer was chosen deliberately: written
where it is used, it would have grown `task/scheduler.rs` from 37 `unsafe` lines to 40, and 18.5 lets
a grandfathered floor grow only by amendment, and only after trying a permitted layer first. It fits
`smp/` honestly rather than as a dodge - a shared array with one writer per slot and readers on every
core **is** a concurrency primitive, and belongs beside `SpinLock`. `task/scheduler.rs` stayed at 37.

### The new authority step A created, and how it was closed

`spawn_probe` binds a **caller-supplied** name to the probe binary, and every service holds a spawn
cap (22 Test A9 asserts exactly that). A compromised service could wait for `fs` to die and register
the probe binary under the name `fs`; clients reacquiring by name (14.3) would wire straight to it.

The kernel name directory is the recovery anchor, and an anchor that can be squatted is not one. So
`spawn_probe` refuses any name in the real catalogue - **the whole catalogue, not the live set**,
because a name is dangerous precisely while its service is DEAD, which is exactly when a liveness
check would report the name free.

### Verified

| | |
|---|---|
| QEMU | 12 suites, **125 / 0 / 0**; files 222/0; shell 164/0/2 |
| gates | 8/8 (commandments, red team, unsafe audit, dash, contract, embed, arch boundary, stack fit) |
| HP T630 (x86_64 AMD) | selfcheck 377/0 twice, chaos 100 rounds / 590 kills, 0 panics |
| Wyse 5070 (x86_64 Intel) | selfcheck 377/0 twice, chaos 100 rounds / 545 kills, 0 panics |
| Raspberry Pi 2 B (ARMv7) | selfcheck 377/0 twice, chaos 100 rounds / 530 kills, 0 panics |
| Raspberry Pi 4 B (AArch64) | selfcheck 377/0 twice, chaos 100 rounds / 568 kills, 0 panics |

**2,233 real service kills, zero kernel panics, zero liveness wedges.** Every service-level panic on
every machine came from the one designed site (`service_context.rs:571`, `recv` panicking on
`EndpointDead` so the supervisor restarts it). A second panic site anywhere in that tally would have
been the tell; there was none.

Tagged `probe-params-hw-verified`.

**What hardware did NOT cover.** The bare-metal, arm32 and aarch64 supervisors all gate out probe
spawning, so no hardware run exercised the parameterised path itself. Hardware proved the
consequences; the QEMU suites proved the path. Together complete, neither alone.

### Three pre-existing defects the battery surfaced

All three were confirmed against `main` before being attributed to this branch.

1. **BP7 read the generation in the dead window.** It killed the victim, read its generation by name,
   and only then respawned. Unregister-on-death clears the name while the id is still ours, so that
   read correctly returns 0 - meaning BP7 failed on its FIRST iteration, and had since the self-heal
   landed. `P7` was updated at the time and carries the comment explaining it; BP7 was not.
2. **Three harness expectations held an ESCAPED em-dash** against probes whose output had been purged
   to plain hyphens. BA3, BA4 and BA6 each sat out a **900-second timeout** and failed: 45 minutes of
   every adv-brutal run spent waiting for text that could never arrive.
3. **`dash_check.py` could not see any of it.** It scanned for literal U+2014/U+2013, and an escaped
   dash is seven ASCII characters. A broken test suite was hiding behind a passing gate. It now
   catches escaped forms, and the guard was **observed firing** before being trusted.

Number 3 is the one that matters. The other two are bugs; that one is why they survived.

---

## 3. What is left: three steps

```
   step A  DONE      probe PARAMETERS out of the kernel      pin 221 -> 29
      |
      v
   step C            supervisor owns the IMAGES + authority  pin 29 -> 1
      |              => adding a SERVICE touches no kernel file
      v
   step D            bus DISCOVERY out of the kernel
      |              => adding a DRIVER touches no kernel file
      v
   step 2            images come from `fs`, packages are SIGNED
                     => update without reflash; authority enforceable again
```

---

## 4. Step C: the supervisor owns the images

```
   TODAY                                   AFTER STEP C
   ------------------------------          ------------------------------
   kernel                                  kernel
     include_bytes! x27                      include_bytes! x1  (supervisor only)
     service_config: 29 rows                 service_config: 1 row
     service_hw(name)                        - gone -
     service_privileges(name)                - gone -
        |                                       |
        | spawns by NAME,                       | spawns from a POINTER the
        | from its own rodata                   | supervisor hands it
        v                                       v
   supervisor                              supervisor
     ctx.spawn("fs")                         include_bytes! x26
                                             spawn(image_ptr, len, config)
```

The supervisor passes what a service **is**: the image, memory limit, core, peers, and - for a driver
- the hardware facts (MMIO base and length, IRQ line, DMA arena size, BDF, whether to confine it).

### Why the kernel's name-based authority table cannot survive this

This is the load-bearing point of the whole design, and it was reached the hard way.

```
   TODAY                                   AFTER STEP C, IF THE KERNEL KEPT A NAME TABLE
   -------------------------------------   ---------------------------------------------
   kernel holds the xhci ELF               supervisor holds the ELF
   kernel grants IRQ 0x28 to "xhci"        kernel grants IRQ 0x28 to "xhci"

   name and code are WELDED together:      a compromised supervisor says
   "xhci" can only ever be the xhci          spawn(<any bytes>, name = "xhci")
   binary, because the kernel has          and the kernel grants the IRQ,
   no other bytes to spawn.                because the NAME matched.

   the check MEANS something.              the check is UNENFORCEABLE.
```

Keeping the table would buy nothing. It would make adding a driver a kernel change, and stop no
attacker. So it goes, and the supervisor becomes the arbiter of hardware authority - because with it
holding the images, **it already is**, whether the kernel pretends otherwise or not.

### What genuinely widens at step C, recorded rather than smoothed over (26.7)

| | today | after step C |
|---|---|---|
| a compromised supervisor can destroy everything | yes | yes |
| ... can restart/kill anything | yes | yes |
| ... can **introduce new code** | **no** (the kernel holds all binaries) | **yes** |

The third row is not a side effect to mitigate. It **is** the feature being asked for. A supervisor
compromised at runtime could point `Spawn` at bytes it synthesised. Nothing in step C prevents that;
step 2 is what puts a ceiling on it. This belongs in the CLAUDE.md amendment in these words rather
than reassurance.

### Why signing at step C would be ceremony

At step C everything still ships inside one kernel image. Anyone who can tamper with the service
blobs can tamper with the kernel that would check them. Signing here is hashing your own pocket - the
identical argument that makes hashing the supervisor pointless today (see section 6). It becomes
load-bearing at step 2, which is the first moment the kernel is handed code it did not ship with.

### Size: measured, not guessed

| | |
|---|---|
| ELFs the kernel embeds | 27 |
| their total size | **1,343 KiB (1.31 MiB)** |
| supervisor today | 32 KiB |
| supervisor after step C | **~1.34 MiB**, which is 2% of its 64 MiB limit |

Largest single item is `shell` at 405 KiB; everything else is under 100 KiB. Size is a non-issue.

### What step C does NOT do: the bytes stay in kernel.bin

`SUPERVISOR_ELF` must stay embedded - it is the recovery anchor. So after step C the same service
bytes still sit inside `kernel.bin`, reached one level deeper:

```
   kernel.bin
   +-----------------------------------------------+
   |  kernel code                                  |
   |  +------------------------------------------+ |
   |  |  supervisor ELF                          | |
   |  |  +-------------------------------------+ | |
   |  |  | shell.elf  fs.elf  xhci.elf  ...    | | |   <- same bytes, nested
   |  |  +-------------------------------------+ | |
   |  +------------------------------------------+ |
   +-----------------------------------------------+
```

Step C changes **who decides**, not **what ships**. Same image size, same flash. The packaging only
changes at step 2.

---

## 5. Step D: bus discovery leaves the kernel

After step C, adding an ordinary **service** touches no kernel file. Adding a **driver** still can,
and it is worth being precise about why, because it is not authority.

Adding a driver today takes four kernel edits:

| edit | what it is | removed by |
|---|---|---|
| `service_config` row | image, memory, core, peers | **step C** |
| `service_privileges(name)` | which caps it may hold | **step C** |
| `service_hw(name)` + an `HwClass` variant | MMIO base, DMA size, BDF, IOMMU policy | **step C** (passed at spawn) |
| `pci::XXX_FOUND` / `MMIO_BASE` / `BDF` statics | what the kernel's PCI scan found | **step D** |

`HwClass::Xhci` does not decide anything. It **reads** `pci::XHCI_FOUND` and `pci::XHCI_MMIO_BASE`,
which the kernel's PCI scanner filled in. The kernel enumerates the bus for a fixed set of device
classes it was taught about.

```
   TODAY                                AFTER STEP D
   ---------------------------          -----------------------------------
   kernel                               kernel
     PCI scan                             (no bus knowledge at all)
     XHCI_FOUND  = true
     XHCI_BAR    = 0x...                bus-manager service
     AHCI_FOUND  = ...                    reads PCI config space
     NIC_FOUND   = ...                    reports: "BDF 00:14.0, class 0x0c0330,
        |                                            BAR0 0x..., IRQ ..."
        | a NEW kind of device                |
        | needs the SCANNER taught            v
        v                                supervisor
   kernel change required                  picks a driver for it,
                                           spawns it with those facts
                                         => NO kernel change
```

So the truthful answer to "can a contributor write a driver without touching the kernel?" is:

- **an ordinary service** - yes, after step C
- **a driver for a device the scanner already recognises** - yes, after step C
- **a driver for a new kind of device** - not until step D

Step D is not large: the kernel already does the scan, so it is a matter of granting a bus-manager
service access to config space and having it report, instead of the kernel keeping a table of device
classes. It also fits the constitution better than what is there now - enumerating a bus and deciding
which driver claims which device is policy, and 4.4 already says drivers are not kernel scope.

Step D is arch-shaped work: x86 config space via I/O ports, Pi 4 via ECAM, and **arm32 has no PCI at
all** (the DWC2 is soldered to the SoC, which is why `HwClass::Dwc2` is the one class whose presence
is a `cfg!`, not a scan).

### What happens when `bus-manager` dies

Drivers get their MMIO base, IRQ and DMA arena from `bus-manager`, so it must come up before any
driver. That is a new ordering dependency, and the failure question follows immediately.

**The supervisor caches the enumeration results** - a bounded array of `(BDF, class, BAR, size, IRQ)`,
fixed size, no heap, the same discipline as its `name -> cap` map. It is the thing that respawns
drivers, so it needs those facts at restart time. If it had to query a possibly-dead `bus-manager`
first, a `bus-manager` crash would take out every driver restart with it (Commandment VIII: a
dependent must not hang on a dependency that is gone).

If the SUPERVISOR dies, it re-derives, and the chain terminates:

```
  supervisor dies
       |
       v
  kernel respawns it            (6.2 - unconditionally, forever)
       |
       v
  adopts the still-running services, including bus-manager
       |
       v
  re-queries bus-manager  ->  facts back
```

If both are dead, the supervisor spawns `bus-manager` first and then queries it. There is no
circularity, because **`bus-manager` needs no discovered facts to start**: it needs access to PCI
config space, which is an architectural constant (an I/O port pair on x86, a fixed ECAM window on
Pi 4), not something anyone discovered.

### Why the enumeration results do NOT go in the kernel

The tempting move is to put them where nothing can die. It is the same instinct that produced the
kernel name directory - which was the right call - but this case fails the test that one passed:

> **The kernel keeps what cannot be RE-DERIVED. Anything re-readable stays out.**

A `name -> endpoint` mapping is irreducible: if the supervisor dies holding it there is nothing to
re-read, because endpoint ids exist only in kernel state. That is exactly why 3.7 justified the
directory as a recovery anchor.

Bus enumeration is the opposite. The irreducible source is **the hardware itself**, and it never goes
away - config space can be re-read in microseconds by anything holding the capability. A kernel copy
would be a derived view of a truth that is always available: the kind of cache 26.4 and Commandment
III reject, adding a second thing that can be wrong and buying no availability.

It would also quietly undo step D. The point of D is that the kernel stops knowing what is on the
bus; keeping the results puts a device table back - smaller, but the same category of thing. The pin
exists to catch precisely that.

### The fiddly part of D: assignment once, re-read always

Re-enumeration after a restart must not disturb live drivers. On the Pi 4 today the kernel does not
merely read the bus, it **assigns**:

```
  pcie: xHCI BAR0 assigned bus 0xf8000000 -> CPU 0x600000000
```

If `bus-manager` inherits that job, dies, and restarts while `xhci` is running against the window it
was given, a naive re-scan that reassigns BARs pulls the address out from under a live driver.

So enumeration splits in two, and the split is a hard requirement rather than an optimisation:

| phase | when | what it may do |
|---|---|---|
| **assignment** | a device with nothing programmed | write BARs, enable bus-master |
| **re-enumeration** | every restart after that | **read only** - report what is already programmed |

Idempotent by construction: read the BAR, and if it is already programmed, keep it and report it.
This is the genuinely fiddly part of D, more so than config-space access itself, and it wants
designing in rather than discovering the first time `chaos` kills `bus-manager` mid-transfer.

### Slice 1 built, and the next problem it revealed: RESTART must go through the supervisor

Built and proven (2026-08-30):

| piece | what |
|---|---|
| `copy_user_to_kernel` | new arch seam on all 7 arches, **bounded to ONE PAGE per call** |
| `loader::ImageSource` | the loader reads from kernel rodata OR a caller's address space |
| header-window fetch | closes the double-fetch (see below) |
| `SpawnImage` (syscall 52) | a versioned `SpawnRequest`, fetched once, gated by the same SPAWN cap |
| `task::spawn_from_image` | no `ServiceConfig`, nothing resolved by name |
| SDK `spawn_image` | matching layout |
| `services/supervisor/build.rs` | the supervisor can embed service images - the other end of the move |

Identity **24/0/0** on the `ImageSource` refactor alone, before any service moved, so the loader
rework is proven transparent independently of the new path.

**Then `pong` was moved across as the end-to-end proof, and identity failed 6A, 6B, 10A and 10B - all
four restart tests.** The cause is not a plumbing bug, it is the next required piece:

```
  control RESTART pong 2
       |
       v
  control does  ctx.spawn_on("pong", 2)   ->  the KERNEL CATALOGUE path
       |
       v
  kernel: no `pong` row (it moved)  ->  NotFound
```

`control` has been bypassing the supervisor and asking the kernel to spawn by name, which only ever
worked because the kernel held every image. 14.4 already says restart authority is the SUPERVISOR's
(`supervisor.restart(name, placement_override)`); step C forces the code onto the path the
constitution already describes.

**This blocks every service, not just `pong`**, so it is the next slice rather than a detail:

- `control` needs to reach the supervisor - it has neither a send-peer to it nor `ACQUIRE_ANY` today.
- The supervisor's endpoint currently receives only DEATH NOTIFICATIONS, whose payload is a bare
  name. A restart command carrying a core override needs to be distinguishable from one, without
  changing the kernel-generated notification format.
- Death-notification respawn alone is not enough: 10A restarts with an explicit core override, and a
  death notice cannot carry one.

`pong` is reverted to the catalogue for now so the tree stays green; `spawn_pong` is kept compiled
(`#[allow(dead_code)]`) so the ABI cannot rot before the restart path lands.

### What a service move actually costs: every caller of spawn-by-name

Moving an image is the easy half. The hard half is that a service the kernel does not know cannot be
spawned BY NAME through the kernel - and far more code did that than the catalogue suggested. Found
one regression at a time:

| caller | mechanism | found by |
|---|---|---|
| `control RESTART` | `ctx.spawn_on` | identity 6A/6B/10A/10B |
| supervisor's own respawn | `spawn_returning_endpoint` | the same |
| shell `spawn` | `ctx.spawn` | reading the code |
| shell PIPES | `ctx.spawn_pipe` | files 189/33 |
| shell `spawncap` | `spawn_returning_endpoint` | shell 67/99 |
| shell `spawnwired` | `spawn_with_caps` | shell 67/99 |
| `chaos` | `ctx.spawn` | shell 67/99 |

Four are fixed and routed through the supervisor. The lesson is the SHAPE: fixing these one at a time
is wrong, because the eighth is found by a regression rather than by reading. **The routing belongs in
the SDK's `ctx.spawn*`** - ask the supervisor when the caller holds a supervisor peer, fall back to the
kernel when it does not. The supervisor has no supervisor-peer, so it keeps the kernel path naturally.

A pipe collapsed into the same thing on the way: **a pipe is not a special kind of spawn, it is a spawn
whose producer is handed its downstream as a peer** - which is exactly what the kernel's own
`spawn_service_pipe` does. One request shape covers both.

### Two more things that had to follow the config out of the kernel

Moving an image breaks whatever still READS that service's config from the kernel, and each one is
invisible until something fails. Two found so far, both real:

**`SpawnImage` could not install caller-provided caps.** `SpawnWithCaps` installs a cap the SPAWNER
supplies into the child - which is how `fs` gets `block-driver`'s cap rather than resolving a name,
and `net-stack` gets `nic-driver`'s. `SpawnImage` passed `None`, so no WIRED service could move. The
kernel already had the machinery; only the ABI did not carry it. `SpawnRequest` gained
`installs_ptr`/`installs_count` using the SAME `[label_len][label][slot_lo][slot_hi]` encoding
`SpawnWithCaps` uses (one wire format, not two), the version bumped 1 -> 2 so a mismatched spawner is
refused rather than misparsed, and the GRANT check is kept exactly: the caller must already hold each
cap grantably, which is what makes installing it non-escalating (7.3).

**`AcquireSendCap` authorised a reacquire from the kernel CATALOGUE.** A service may reacquire a peer
it DECLARED without holding the broad `ACQUIRE_ANY` - that is the 14.3 recovery every client owes a
restartable peer. The check was:

```rust
match service_config(name) {          // the kernel catalogue
    Some((_, cfg)) => cfg.send_peers.iter().any(|p| *p == peer),
    None           => false,          // a moved service: "declares nothing"
}
```

So a supervisor-owned service was permanently denied it: `ping` looped
`reacquire failed, retrying next tick` forever once `pong` changed core. It now reads what the task
was ACTUALLY WIRED WITH, recorded at spawn - the honester source anyway, being the real wiring rather
than a table saying what it should have been.

> **The pattern to expect: as configuration leaves the kernel, everything that READS that
> configuration has to follow it.** The contract reconciler was the first (it now checks the kernel or
> the supervisor); these are the second and third. More will surface in the `service_hw` and
> privileged groups.

**The unsafe rule improved the design a second time.** Recording per-task peers first added an
`unsafe fn` to `task/scheduler.rs` - 37 to 39 on an 18.5 grandfathered floor, which may grow only by
a CLAUDE.md amendment. But that table only ever needs COMPARISON, never a borrowed `&str`, so storing
it as per-byte atomics makes every operation safe: `smp::names::AtomicNameSet` beside `NameTable`,
zero new `unsafe` anywhere, floor untouched, no amendment needed. (The loader shrank 2 -> 1 the same
way.)

### The blocker: only a SPAWNER can obtain a grantable cap

`spawn_with_caps` TRANSFERS a cap into the child, and a transfer requires `GRANT` (8.5). Only a
service's SPAWNER is handed a `SEND|GRANT` cap to it; `acquire_send_cap` yields SEND alone, and rights
never widen (7.3).

So once the supervisor owns a service's image, **nothing else can ever obtain a grantable cap to that
service.** No amount of acquiring differently fixes it. The supervisor must hand one back:

```
   today   CMD_SPAWN reply = [status]
   needed  CMD_SPAWN reply = [status] + an embedded SEND|GRANT cap to the new endpoint
```

**BUILT.** The reply now carries a derived `SEND|GRANT` cap when the spawn produced an endpoint. This
is the capability model pointing somewhere real rather than an obstacle to route around:

> A transfer needs GRANT. Only a spawner holds GRANT. Rights never widen.
> => **whoever owns the image must be the principal that delegates access to it.**

Ownership and delegation belong together; the old code only avoided that because the kernel owned
everything. Three details the implementation has to get right:

- **The caller reclaims what it does not want.** `spawn_via_supervisor` drops the returned cap
  immediately, or every spawn leaks a cap-table slot - the exact leak `logger` had, which surfaced as
  a wrong arrow in a dependency tree.
- **A DERIVED copy, not the original.** The supervisor keeps its own (it needs it to wire dependents
  later) and reclaims the derived copy if the send fails.
- **It always answers.** No cap, or a failed cap-send, still replies with the status alone, so a
  caller is never left waiting on a reply that is not coming (invariant 12).

`spawnwired` keeps testing exactly what it was written to test - that a child uses a PASSED cap rather
than a name - with the cap now sourced from whoever actually spawned the service.

### Where this stopped, and why

Seven moved: `pong`, `ping`, `roster`, `reply-server`, `holder`, `upper`, `mem-pressure`.
Pin 29 -> 22. `ping` is wired to `pong`, so the installs path is exercised rather than merely built.

`pong`, `greet`, `upper` and `mem-pressure` were moved and REVERTED. Each is referenced by something
that spawns it by name (`spawnwired`, the pipe tests, `chaos`), and the three blockers above are
prerequisites rather than details. Reverting is sequencing, not scope reduction: the infrastructure
(`SpawnImage`, the loader, the command channel, the reacquire recovery, the widened contract gate) is
all in place and green.

**A correction worth recording.** The `pong` move was reported as "proven end to end" on the strength
of identity 24/0/0. The SHELL suite - not run at the time - fails on it, and would have then. One
suite is not end to end, and the commit message that says so is wrong.

### The double-fetch the loader closes

A user image is MUTABLE BY ITS OWNER while the kernel reads it. Validating the program headers in
place and then acting on them would let a supplier pass validation and rewrite the offsets before the
segment copy, landing bytes at an address the kernel approved a different value for.

```
  1. copy ELF header + program headers into a BOUNDED kernel buffer   (one page; refuse if beyond)
  2. validate ENTIRELY from that copy                                  <- offsets now immutable
  3. stream segment CONTENT page by page, straight from user memory    <- kernel never interprets it
```

Only content is read live, and the kernel forms no opinion about content, so a byte that changes
mid-copy can only corrupt the image its own supplier provided.

**A note on how the unsafe rule paid off.** The first implementation added an `unsafe fn copy_out`
beside the loader's existing raw zeroing and raw copy: 2 unsafe lines to 4, in a file that is neither
a permitted layer nor one of 18.5's grandfathered floors. `unsafe_check.py` refused it. Restructuring
so the destination page becomes a SAFE SLICE ONCE made both the zeroing and the copy safe operations,
and `loader.rs` went **2 -> 1**: it shrank while gaining the ability to read user memory. The rule did
not merely catch a violation, it produced a better design.

### Where step C stopped, and exactly what each remaining service waits on

**Pin 29 -> 19. Ten services the kernel has never heard of:** `pong`, `ping`, `time`, `logger`,
`asker`, `roster`, `reply-server`, `holder`, `upper`, `mem-pressure`. That includes `logger`, which
every service logs through, and `time`, which the shell and net-stack depend on - so the mechanism is
carrying load-bearing services, not only demos.

Everything that could move without new mechanism has moved. The remaining 19 rows:

| service | waits on |
|---|---|
| `counter` | nothing technical - it is `spawncap`'s only viable subject (not running at boot, has a recv endpoint, and its peer `fs` is always up) |
| `greet` | `spawnwired` needs the SHELL to TRANSFER a cap into the spawn request; nothing else wants that, so building it would be speculative (26.2) |
| `probe` | the probe path resolves its ELF via `service_config`, AND probes need `is_probe` privileges |
| `observe`, `observe-now`, `observe-live` | INTROSPECT, granted by NAME PREFIX |
| `chaos`, `control`, `shell` | privileges (SPAWN, SERVICE_CONTROL, ACQUIRE_ANY, REBOOT) |
| `console`, `fs`, `net-stack`, `resource-server` | `service_hw` - framebuffer grant / RESOURCE_MINT |
| `block-driver`, `xhci`, `ehci`, `dwc2`, `nic-driver` | `service_hw` - MMIO, DMA arena, IRQ, IOMMU |
| `supervisor` | stays forever - the recovery anchor |

**Seventeen of the nineteen reduce to ONE question:** the `privileges` and `hw` fields, which is the
amendment. Only `counter` and `greet` are blocked on anything else, and both are small.

So the machinery of step C is done and proven; what remains is a decision about the trust model, not
more plumbing.

### A trap the privileges design must not inherit: privilege by NAME PREFIX

`INTROSPECT` is granted to any service whose name starts with `observe`, `prop-` or `stress-`:

```rust
introspect: ... || name.starts_with("observe") || name.starts_with("prop-") ...
```

Harmless while the kernel owns every name. The moment a CALLER supplies names it reads as "call
yourself `observe-x` and get introspection" - a privilege obtainable by choosing a string. Whatever
replaces `service_privileges` must not carry this forward, and it is a concrete reason the privileges
design deserves a look rather than a mechanical port.

It is the same shape as the two defects the moves already surfaced (the contract reconciler,
`AcquireSendCap`): **authorization keyed on something the kernel is giving up.**

### Ordering: C first, D second, with one condition

**Design C's spawn ABI for the end state, not the interim.** The supervisor should pass the hardware
facts explicitly at spawn, so `service_hw` and `HwClass` leave the kernel **at C**. In the interim
the supervisor obtains those values from a kernel discovery query; at D, the bus-manager supplies
them instead.

The ABI is the expensive, hard-to-change part. Design it once for where we are going, and D only
swaps where the numbers come from. Get this wrong and we build a name-keyed hardware table at C and
tear it out at D - the "build it twice" trap.

---

## 6. The trust chain, and why the supervisor stays welded to the kernel

**Decided:** Limine boots the kernel, the kernel boots the supervisor, the supervisor boots the rest.
The chain stays exactly that shape.

```
   Limine  ---->  kernel  ---->  supervisor  ---->  every other service
                  |
                  +-- ONE artifact: kernel.bin = kernel + supervisor
                      one trust boundary, nothing to forge
```

A Limine **boot module** was considered - Limine can place the supervisor's bytes in RAM and tell the
kernel where they are, with the kernel still parsing, validating and spawning it, so the authority
chain would be untouched. It was **rejected**, and for a stronger reason than simplicity:

> If the supervisor arrives as a separate file on the ESP, anyone who can write to the ESP can swap
> it. The kernel would be trusting a file it did not ship with - the step-2 problem arriving early,
> before signing exists to answer it.

This matters specifically because 6.2 has the kernel respawning the supervisor **unconditionally,
forever**. For that to be a recovery anchor rather than a liability, the image has to be as
untamperable as the kernel itself. Embedding is what makes that true by construction.

**The supervisor therefore never leaves the kernel image, not even at step 2.**

### What "a compromised supervisor" means, honestly

At runtime, you cannot tell. That is not a gap; it is the definition. The TCB is the set trusted
**without** verification, and 6 makes the kernel's trust in the supervisor axiomatic - it respawns it
forever, no questions, because there is nothing to check against.

Which is why hashing the supervisor today would be theatre: `SUPERVISOR_ELF` is compiled into
`kernel.bin`, so the same image protects both. Hashing your own pocket. 26.2 - features are pulled
into existence, and nothing pulls this yet.

A hash also only proves the **image at spawn**. A supervisor exploited through memory corruption
after it started hashes identically. Worth stating so it is never oversold as "we would know."

---

## 7. Step 2: images from `fs`, as signed packages

Today, changing one service means reflashing the machine. Step 2 puts the service binaries on the
GSFS disk as files; the supervisor reads them through `fs` and spawns them. Update a service = write
a file and restart it.

That turns the disk from a data store into a **code** store, so anything that can write to the disk
could inject code. A signed package is the answer.

```
   service package
   +-------------------------------------------+
   |  the ELF                the code          |
   |  its contract           declared caps     |
   |  a signature over BOTH  made at build time|
   +-------------------------------------------+
                    |
                    | private key: NEVER on the machine (build/release only)
                    | public  key: inside kernel.bin, the trust boundary
                    v
   at spawn: kernel verifies BEFORE mapping a single page
             bad signature / tampered ELF / altered contract  ->  REFUSED, loudly
```

### What signing buys, beyond tamper-proofing

**It makes authority enforceable again.** This is the piece that answers section 4's problem. Once
the supervisor owns the images the kernel cannot trust a *name* - but if the contract is bound to the
image cryptographically, the kernel can enforce "**this exact image** is authorised to hold IRQ 0x28"
without trusting the name, and without trusting the supervisor.

**It puts a ceiling on a compromised supervisor.** It can then only spawn images somebody signed. It
can still spawn the wrong signed service at the wrong time, but it cannot introduce new code - which
is exactly the widening step C opens.

16 already specifies this and it was never built:

> "Signature valid? No -> Reject. Contract valid? No -> Reject. Policy allows? No -> Reject."

### Three honest costs

**Crypto in the kernel.** A signature verify (Ed25519: `no_std`, no heap, small) is new TCB code.
This is the one place to accept it, because it is what makes the rest safe. Spawn cost is a hash over
the image plus one verify - milliseconds, against a spawn already costing 2.7 to 27 ms.

**Key management becomes real.** Signing key, rotation, what happens when it leaks. Process, not
code, and none of it exists today.

**`fs` can never come from `fs`.** You cannot load the filesystem driver out of the filesystem. The
embedded set never reaches zero:

```
   stays embedded in kernel.bin        moves to signed packages on disk
   ----------------------------        --------------------------------
   supervisor   (recovery anchor)      shell, console, logger, time,
   block-driver (reach the disk)       net-stack, nic-driver, xhci, ehci,
   fs           (read the disk)        observe, chaos, ping, pong, ...
   shell?       (a prompt on a
                 corrupt disk)         roughly 20 of 27
```

That floor is arguably the right shape regardless - it is the same "enough to recover" set the kernel
already has to guarantee.

---

## 8. How Linux and Windows install a driver, and why ours is a different problem

Worth knowing what the mainstream systems actually do here, because they solve a problem this design
does not have - and the difference is the whole argument for the microkernel shape.

### Linux

A driver is a kernel module, a `.ko` file. Installing one is genuinely dynamic, no reboot:

```
  device appears (boot scan, or hot-plug)
       |
       v
  KERNEL enumerates the bus, builds a modalias string
       "pci:v00008086d0000100Esv...sc..."
       |
       v
  udev (USERSPACE) sees the uevent, looks the alias up in modules.alias
       |
       v
  modprobe  ->  KERNEL relocates and links the .ko INTO KERNEL ADDRESS SPACE,
                runs module_init(), the driver registers with the PCI subsystem
       |
       v
  driver bound to device.  No reboot.  rmmod unloads it.
```

Note the split, because it is the same one step D proposes: the **kernel** scans the bus, **userspace**
decides which module matches. Reboots are for kernel upgrades, not driver installs.

### Windows

The same shape with different names. A `.sys` file plus an `.inf` declaring which hardware IDs it
claims; the PnP manager enumerates, matches the hardware ID against the driver store, and loads the
driver into kernel space at runtime. Most installs need no reboot - you get prompted when the device
is already in use, or for boot-start drivers that load before the disk stack.

Two Windows details matter to this design:

- **Driver signing is kernel-enforced.** Since Vista x64 the kernel refuses unsigned kernel-mode
  drivers. That is not a policy nicety; it is load-bearing, precisely because the code is about to run
  with kernel privilege. Linux has the same under Secure Boot (`CONFIG_MODULE_SIG`).
- **UMDF.** Microsoft moved a large class of drivers - printers, many USB devices - into USER MODE,
  because kernel-mode driver bugs were the single largest cause of blue screens. They spent years
  retrofitting what a microkernel has by construction.

### What they share that we do not

Both are monolithic, so **installing a driver means injecting code into the kernel at runtime.**
Everything else follows from that one fact:

| consequence | Linux / Windows | GodspeedOS |
|---|---|---|
| driver code runs with kernel privilege | yes | no - it is an ordinary service |
| a driver bug can take the machine down | panic / BSOD | the supervisor restarts it |
| signature enforcement is REQUIRED for safety | yes - arbitrary kernel code otherwise | no - capabilities bound it either way |
| unloading a driver | genuinely hard (`rmmod` often refuses) | `kill`, which this system does constantly |
| the kernel grows with each driver | yes | **never** |

On GodspeedOS a driver is already just a service. Installing one never means new kernel code - not at
step C, not at step 2, not ever. It means putting a binary somewhere and asking the supervisor to
spawn it.

```
  device appears on the bus
       |
       v
  bus-manager SERVICE enumerates, reports BDF / class / BAR / IRQ      <- step D
       |
       v
  supervisor picks a driver, reads its signed package from fs          <- step 2
       |
       v
  kernel VERIFIES the signature, then spawns an ordinary task with
  exactly the MMIO window, IRQ and DMA arena the contract declares     <- step C
       |
       v
  driver running in USERSPACE.  No reboot.  `kill` unloads it, and the
  supervisor restarts it if it dies.
```

### The consequence for our signing model

**Their signing exists to make kernel-privileged code safe. Ours exists only to authenticate a
binary.** If a signed driver of ours turns out to be malicious it still cannot do more than its
capabilities allow, and an IOMMU bounds its DMA where the hardware has one (6.4). Same mechanism,
much smaller blast radius, because the hard problem they are solving with it is one this system never
took on.

The other half of their difficulty is already gone too. "Unloading is hard" does not apply here:
killing and restarting services is the thing this system does most. Chaos did it **2,233 times across
four machines** in the step A validation without a single kernel panic.

---

## 9. Where the pin lands at each step

| step | `service_configs` pin | adding a service | adding a driver | update without reflash |
|---|---|---|---|---|
| before | 221 | kernel change | kernel change | no |
| **A (done)** | **29** | kernel change | kernel change | no |
| **C (done)** | **2** | **no kernel change** | kernel change | no |
| D | 1 | no kernel change | **no kernel change** | no |
| 2 | 1 | no kernel change | no kernel change | **yes** |

**Step C is built.** The pin is **2**: `supervisor` (the target - the kernel must bootstrap it
because nothing is beneath it) and `probe`. Every other service's image, memory limit, placement,
peers, privileges and device class lives in the supervisor's `IMAGES` table.

### 9.1 What a driver names, and what it may not

A driver row names a device **class** (`hwclass::AHCI`, `XHCI`, `EHCI`, `DWC2`, `NIC`,
`FRAMEBUFFER`). The kernel resolves that against its own bus scan and supplies the MMIO window, the
DMA arena, the PCI BDF **and the interrupt vector**. `SpawnImage` refuses a request that names any
of them directly.

The vector belongs on that list and was nearly missed. An IRQ vector is authority in exactly the way
a physical address is - routing a vector to a task is what makes that task receive the device's
interrupts, and on ARM granting the USB vector is precisely what takes the controller away from
whoever held it. For a while addresses were refused while vectors passed straight through from the
caller, which is the same hole with a different field name. `hw_irqs_for(class)` closes it: the
kernel states the vector it assigned, so the supervisor never picks one.

This is also why step C needed **no** constitutional amendment for IRQ routing. The concern recorded
earlier - that the kernel can only refuse an IRQ route to the wrong ELF *because it holds the ELF* -
dissolves once the vector is derived rather than requested. There is no wrong route to refuse,
because the caller cannot express one.

### 9.2 What the move cost, and what caught it

Six services moved in this step; three defects came with them, and **none was visible to the 410
tests that were passing at the time**:

| defect | why the suites missed it |
|---|---|
| `console` could have lost its framebuffer | identity/shell/files all drive the shell over SERIAL; the screen is not looked at. Caught by a QEMU **screendump**. |
| the USB drivers were dead | the identity QEMU has no xHCI controller at all. Caught by **22 Test 12**, the only suite that attaches one. |
| a table's PREFERRED core was sent as a STRICT override | every suite runs `-smp 4`, and no row names a core above 3. Caught only because Test 12 boots `-smp 2`, where `logger` and `xhci` vanished. |

The pattern is the one this programme keeps re-learning: **a moved service missing a field does not
fail to start. It starts, looks healthy, and does the wrong thing.** A green suite is evidence about
the paths that suite exercises and nothing else. Two further traps of the same shape were found
beside them: `EMBEDDED` was a flat list, so on x86 the absent `dwc2` resolved to a twelve-day-old
binary rather than tripping the build guard (a guard that fires on a MISSING file does not fire on a
stale one); and seven refusals in `handle_spawn_image` returned `-1` with nothing on the console, so
a failed spawn reported only "InvalidArgument".

### 9.3 An authority that left rather than moved

`USB_DISK` gated `block-driver`'s whole-device reach to a USB stick through the in-kernel Bulk-Only
stack. It has **no** privilege bit, deliberately: on both ARM ports the driver now reaches its stick
through the `dwc2` / `xhci` service over IPC and calls no `usb_disk_*` syscall at all. So the move
dropped the authority instead of carrying it - the narrowing audit SEC-37 asked for, arrived at as a
consequence rather than as a task. `NET_DEVICE` went the other way and did get a bit, because
`nic-driver` genuinely uses it for the Pi 4's GENET. The test for the two was the same question:
does the service still call the syscall?

---

## 10. Open items

- **The CLAUDE.md amendment for step C** must state the widening in section 4's words: after C, a
  runtime-compromised supervisor can introduce new code, and nothing before step 2 prevents it.
  (The IRQ-routing half of this concern is closed - see 9.1 - but the code-introduction half stands
  and is the reason step 2 exists.)
- **`probe` is the last non-supervisor row.** It needs two things, both of which fit the existing
  model: a `SPAWN_FLAG_PEERS_GRANT` bit (`probe-5a-send` gets grantable peer caps, which the
  supervisor already holds and so may pass on), and a class for the TEST interrupt line that
  `probe-11a` receives - the kernel supplying vector 33 from the class, exactly as it does a
  device's. The work is larger than it looks because the 193 probe spawns go through `Spawn`'s
  packed-parameter path rather than `SpawnImage`.
- **Step D's arch shape** is unmeasured. x86 I/O-port config space and Pi 4 ECAM are known
  quantities; arm32 has no PCI, so the bus-manager is x86 + Pi 4 only and `dwc2` keeps its
  SoC-presence path.
- **Step 2's key management** has no design yet, and it is process as much as code.
- **A9-4, the BSP idle wedge**, is unrelated to this work and remains open and deferred. It did not
  recur on any of the four hardware runs.

---

## 11. Related

- `docs/probe-params-design.md` - step A in full, including the as-built ABI and the hardware table
- `docs/naming-design.md` - Path C: how naming left the kernel, and the recovery-directory argument
- `CLAUDE.md` 4.3 / 4.4 (kernel scope), 6 (TCB), 16 (update model), 26.2, 26.7, 26.10
- `COMMANDMENTS.baseline.toml` - the `service_configs` pin that makes all of this enforceable
