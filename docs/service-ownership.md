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
     XHCI_BAR    = 0x...                hw-enumerator
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

Step D is not large: the kernel already does the scan, so it is a matter of granting a hw-enumerator
service access to config space and having it report, instead of the kernel keeping a table of device
classes. It also fits the constitution better than what is there now - enumerating a bus and deciding
which driver claims which device is policy, and 4.4 already says drivers are not kernel scope.

Step D is arch-shaped work: x86 config space via I/O ports, Pi 4 via ECAM, and **arm32 has no PCI at
all** (the DWC2 is soldered to the SoC, which is why `HwClass::Dwc2` is the one class whose presence
is a `cfg!`, not a scan).

### What happens when `hw-enumerator` dies

Drivers get their MMIO base, IRQ and DMA arena from `hw-enumerator`, so it must come up before any
driver. That is a new ordering dependency, and the failure question follows immediately.

**The supervisor caches the enumeration results** - a bounded array of `(BDF, class, BAR, size, IRQ)`,
fixed size, no heap, the same discipline as its `name -> cap` map. It is the thing that respawns
drivers, so it needs those facts at restart time. If it had to query a possibly-dead `hw-enumerator`
first, a `hw-enumerator` crash would take out every driver restart with it (Commandment VIII: a
dependent must not hang on a dependency that is gone).

If the SUPERVISOR dies, it re-derives, and the chain terminates:

```
  supervisor dies
       |
       v
  kernel respawns it            (6.2 - unconditionally, forever)
       |
       v
  adopts the still-running services, including hw-enumerator
       |
       v
  re-queries hw-enumerator  ->  facts back
```

If both are dead, the supervisor spawns `hw-enumerator` first and then queries it. There is no
circularity, because **`hw-enumerator` needs no discovered facts to start**: it needs access to PCI
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

### D1 (BUILT): a driver names its device by PCI CLASS CODE

D splits in two, and the halves are not alternatives - the first is a prerequisite for the second:

| | what moves | what it buys |
|---|---|---|
| **D1 - built** | the kernel stops INTERPRETING the bus | adding a driver needs no kernel change |
| **D2 - not built** | the kernel stops LOOKING at the bus | `hw-enumerator`; the kernel holds no bus code |

You cannot move WHO produces a device table that is not produced yet, so D1 comes first regardless.

**What is built.** The boot scan records every device it meets into a generic table -
`(BDF, class code, six BARs, IRQ line, vendor, device)` - before any per-class branch cares what it
is. A driver then names its device by the industry-standard 24-bit (class, subclass, prog-if) triple:
`0x010601` IS an AHCI SATA controller on every machine ever built, so the kernel needs no name for
it. `hw_flags` bit 31 marks the value as a class code; the kernel looks it up and grants that
device's BAR, DMA arena and BDF.

Three facts travel with it, because they belong to the DRIVER rather than to the bus, and **none of
them is an address**:

- **BAR index** - which BAR holds the registers. xHCI uses 0, AHCI uses 5 (`ABAR`). An index, so the
  kernel still supplies the value from its own scan. This is not a detail: the first version of the
  table recorded BAR0 only, and the boot cross-check caught it at once - AHCI read back 0 against a
  static that said `0xfebd9000`.
- **DMA pages** - how much arena. A size; the kernel still decides WHERE. Bounded at 2048 pages,
  because an unbounded size is a denial of service (§26.6).
- **Confine** - IOMMU or passthrough. Policy (§6.4).

`mmio_base`, `mmio_len` and `bdf` remain REFUSED, exactly as step C left them.

DMA reservations are keyed per DEVICE now rather than per class name, so a device the kernel cannot
name still gets the same physical arena back across a restart - which is what makes a driver respawn
transparent to a controller still DMAing into it.

**`block-driver` is the first caller**, chosen as the strictest test available: a non-zero BAR index,
a DMA arena, a BDF for bus-master enable, and no interrupt. `files` (222 tests) exercises the whole
path end to end.

**The cross-check that made it safe.** Before anything switched over, the boot log printed the
generic table against the per-class statics it replaces. Two independent views of one truth had to
agree first, and the agreement is evidence in the log rather than an assertion in a commit message:

```
pci: device table - 8 device(s) recorded (generic, no class knowledge)
pci:   xHCI class 0x0c0330 AGREES (BAR0 0xfebd4000 BDF 0x0020)
pci:   EHCI class 0x0c0320 absent from both - agrees
pci:   AHCI class 0x010601 AGREES (BAR5 0xfebd9000 BDF 0x00fa)
pci:   NIC  class 0x020000 AGREES (BAR0 0xfeb80000 BDF 0x0010)
```

### D1b (BUILT): the MSI vector pool - an interrupt without a name

D1 left one thing forcing a kernel rebuild: a driver spawned by class code got MMIO, DMA and its BDF,
but **no vector**. The named classes each had a constant the kernel was taught (`XHCI_MSI_VECTOR`,
`EHCI_MSI_VECTOR`) with its own hand-written IDT stub, so a device the kernel had no name for had no
vector to be given. A ported driver that polls needed no kernel change; one that wanted an interrupt
still did - which is most real drivers.

**The pool is eight anonymous vectors** (`MSI_POOL_BASE = 0x30`, `MSI_POOL_LEN = 8`) with identical
stubs, handed to devices as they spawn. Nothing in the kernel knows what any of them is FOR: a vector
is allocated to a device, written into that device's own MSI message-data register, and delivered to
whichever endpoint registered the route (§12.2) - exactly what the two named vectors do, minus the
name. Eight, and bounded on purpose (§26.6): the busiest board here presents three interrupt-driven
devices, and exhaustion is REPORTED rather than silently leaving a driver without interrupts.

**A vector number is still authority, and callers still cannot name one.** This is the part that had
to stay true. Step C established that a caller may not ask for a specific IRQ, because a caller that
could would be asking to be handed another device's interrupts. The pool does not weaken that: the
driver declares only `hw_pci_irq = true` - that it NEEDS an interrupt - and the kernel decides which.
Which vector it got is a runtime fact printed at boot, exactly as the BAR address is:

```
pci-msi: class 0x0c0330 BDF 0x0020 -> vector 0x30 (pool slot 0)
```

**Allocated per DEVICE, not per spawn**, and that is what makes a restart work. The vector is written
into the device's own register, so a respawned driver must be handed the SAME one back - a fresh
vector each time would both exhaust the pool after eight restarts and leave the controller raising an
interrupt nobody routes. Same reasoning as the permanent DMA reservation beside it.

The same vector, but **reprogrammed** on every spawn rather than just handed back, for two reasons.
The first is a real bug the constitution's own restart rule implies: §9.2 re-evaluates placement from
scratch and deliberately does NOT remember the previous core, so a respawn can land elsewhere - and a
cached vector still aimed at the dead instance's LAPIC would deliver the device's interrupts to a core
with nobody waiting on them. The destination is therefore re-derived per spawn.

The second is defensive. Returning early without writing assumes the device's MSI configuration
survived the driver's death, which is an assumption about every driver that will ever use this pool
rather than a guarantee. It holds for xHCI (MSI lives in PCI config space; `HCRST` resets the
operational registers, not config space), but a heavier reset path would come back with no MSI
programmed and fail SILENTLY: the driver restarts, reports ready, and the keyboard never types again.
The write is idempotent and costs a few config-space accesses once per spawn.

**Testability gap, stated rather than glossed:** neither of those is exercised in QEMU. The only build
with a live xHCI is the IOMMU test (`-device qemu-xhci`), which has no shell or `chaos` to kill it
with, and the drivers the ordinary QEMU config does spawn take no pool vector. So the restart path is
reached first by chaos ON HARDWARE, which does kill `xhci`. Both changes are reasoned, not measured,
and this says so (§26.7).

**`xhci` is the caller, chosen as the strictest test available.** It is the only IOMMU-CONFINED
driver, it needs the largest arena (292 pages), and §22 Test 12 checks the entire chain end to end
rather than any one link: confined to its arena, the page past it unmapped, and a keyboard actually
enumerated THROUGH the confined domain - on a pool vector, with the kernel holding no name for the
device. That test passes.

Programming it at spawn also made the kernel's boot-time write to the same register dead, so it was
deleted rather than left as a harmless duplicate. It is not harmless: a write that a later write
always replaces keeps working if the later one is ever removed, which hides the regression instead of
failing loudly (invariant 12). One writer, at the point the vector is decided.

**So D1 + D1b together: a driver for any PCI device this kernel has never heard of - registers, DMA
arena, bus-master enable and interrupt - needs NO kernel change.** That was the goal.

### `BAR_AUTO`, and the NIC that could not be expressed without it

Moving a second driver found the design's first real limit, which is the useful kind of finding:
**`hw_pci_bar = <index>` could not express the NIC.**

The NIC scan never used a fixed index. It walks the BARs and takes the **first mapped MEMORY BAR**,
and that is not an accident - it is what lets ONE driver cover both supported cards. The e1000 puts
its registers in BAR0; the RTL8168 on the Wyse puts I/O ports there and its registers in BAR2. Any
number written in a contract is wrong for one of them.

So BAR index **7** now means AUTO: the first mapped memory BAR. It costs no bits (the field is 3
bits and only 0..5 are real BARs), and it is still not an address - it is a RULE the kernel evaluates
over its own scan, exactly as an index is. It is arguably the better default: a driver wants "my
registers", and which BAR they landed in is the bus's business.

**What moving `nic-driver` actually bought is bigger than the addressing.** `HwClass::Nic` granted
MMIO only to two hard-coded vendor/device IDs:

```rust
HwClass::Nic if matches!(NIC_VENDOR_DEVICE, 0x100E_8086 | 0x8168_10EC) => NIC_MMIO_BASE
```

Intel 8086:100E and Realtek 10EC:8168. **A third NIC could not be driven at all without a kernel
rebuild** - a whitelist of specific silicon, in the kernel, which is precisely what step D exists to
delete. Named by class 0x020000 ("PCI ethernet controller", which every one of them reports) the
whitelist is gone and the kernel knows none of them.

ARM keeps the kind, correctly: neither Pi has a PCI bus (LAN9514 over USB on the Pi 2, GENET on the
Pi 4), so there is no class code to name and no table to find it in.

The remaining gap is not interrupts, it is arches: only x86_64 fills the device table (below).

**Per-arch state.** x86_64 fills the table. arm32 never will - it has no PCI at all, the DWC2 is
soldered, and every driver there names a non-PCI kind. aarch64 is a real GAP rather than a
non-applicable one: the Pi 4 has PCIe and `pcie::init` walks it, but it does not record into the
table yet, so its drivers still name a kind.

### The blocker D2 has to answer: who may name an ADDRESS

The sketch above has the supervisor spawn a driver "with those facts": MMIO base, IRQ, BDF. Step C
established the opposite and `SpawnImage` enforces it, refusing raw addresses outright - because a
caller that can name an MMIO base can point a driver at ANY physical memory, which is kernel-equivalent
reach on any board without an IOMMU (both Pis, and any x86 machine whose firmware supplies no IVRS).

An earlier draft of this section concluded that D was therefore blocked, on the reasoning that the
kernel cannot validate a supplied address without reading config space - the very capability D removes.
**That reasoning was wrong, and this replaces it.** The kernel does not need to know WHICH DEVICE owns a
range. It only needs to know the range is **not RAM** - and it already holds that truth, independently
of any bus, in the memory map the bootloader handed it (`BootInfo.memory_map`, retained for the
kernel's lifetime).

So the check is:

```
  bus-probe    reads config space, enumerates, reports (BDF, class, BAR, size, IRQ)
  supervisor   picks a driver for a device, spawns it with (phys, len)
  kernel       REFUSES any range that intersects usable RAM   <- against its OWN map, not the caller's word
```

What a compromised reporter or supervisor can then do is hand a driver **some other device's
registers**. What it can never do is map RAM, kernel page tables, or another service's memory. That is
a real bound, checked against a truth the kernel owns - not trust in a number it was given. It is also
the same posture §6.4 already accepts for a DMA-capable driver on a board with no IOMMU, so it adds no
new CATEGORY of exposure; today's arrangement is not safer, only more implicit.

With that, the kernel's bus responsibility goes to **zero**: no scan, no classification, no `HwClass`
variant per device, none of the 21 per-class statics.

**Not all of `pci.rs` leaves, and the earlier claim that it did was wrong in kind, not only in number.**
Measured at `8f3c1a10`, the file is ~1,320 lines: roughly **870 go** (the bus walk, `find_by_class`, the
BIOS handoffs, the per-class statics - semantics) and roughly **400 STAY** (`config_read32`,
`config_write32`, `cfg_read_gated`, `bar_and_irq_from_bdf`, `program_msi`/`program_msix`,
`set_bus_master`) because those are MECHANISM the kernel still owes: it performs every `PciCfgRead` on
a service's behalf, it programs MSI, and interrupt routing is one of the six responsibilities by name
(§4.3). The split is approximate - a few helpers could fall either side - but the shape is not.
What it gains is a bounds check against a map it already has, and granting MMIO by `(phys, len)`
instead of by class - mechanism, not policy.

### DECIDED (2026-09-01): `hw-enumerator` may hold direct hardware authority

The question this section poses is settled. A service MAY hold direct hardware-access authority,
scoped to the minimum its contract needs. The governing line:

```
kernel:     "Given a valid capability, I permit this operation."
userspace:  "I know what this operation MEANS."
```

The kernel knows how to perform one configuration read. It does not know what the selector it was
handed NAMES - not that a bus/device/function encoding sits inside it, not how to walk a bus, not what
a class code identifies, not how to read a BAR. That knowledge is hardware SEMANTICS and belongs in
userspace. This is §26.10 applied to a privileged access rather than to a syscall.

**The grant.** `hw-enumerator` receives exactly one operation:

```
MAY:       read one configuration register  (selector + offset, both opaque to the kernel)
MUST NOT:  write configuration space at all
MUST NOT:  leave the hardware selector pointing anywhere
```

Not "all port I/O", and not "read/write config space". Authority reflects the operations actually
required, and enumeration is inherently read-only.

**One operation, not a select-then-read pair - and that is a correctness requirement, not tidiness.**
Configuration space is reached on both ports through a stateful index/data register pair: latch a
selector, then read the data. The first version of this grant exposed those as two syscalls, and that
was wrong in a way hardware found before review did. Two callers interleaved read whichever register
the OTHER selected. Worse, the KERNEL drives the same pair on its spawn and kill paths
(`program_msi`, `set_bus_master`), so a split interface raced the kernel's own accesses and could land
a kernel WRITE on a device a service had selected. A lock cannot close that gap: the kernel would have
to hold it across two syscalls and wait for a service to issue the second, and nothing above the
kernel may make the kernel wait. Folding the pair into one atomic kernel operation removes the window
entirely, makes multiple holders safe, and leaves the caller unable to write anything at all.

**The kernel enforces admissibility, and this is where "a service must not be able to halt the
machine" becomes concrete.** On the Pi 4, a configuration read past the bus range the bridge is
programmed to forward is an unsupported request: the root complex raises an SError and stalls the
interconnect, rather than returning the harmless all-ones a PC's host bridge synthesizes. An early
build of the enumerator walked four buses on a board that forwards one, and took the machine down with
it - a userspace service panicking the kernel through an argument, which is the one thing that must
never happen. So the kernel refuses a bus it did not program a route to. That is address
admissibility, not device provenance: the kernel checks that an access is safe to perform, never that
the caller is entitled to the device behind it.

**Recorded limitation (§26.7): this is PREVENTION, not recovery.** The kernel stops the unsafe access
from being issued; it cannot survive one that is. An aborted configuration read stalls the CPU inside
a load instruction and holds the interconnect, which is a hardware condition with no software exit -
the observed failure was another core, doing unrelated work, making no scheduler progress for ten
seconds until the liveness watchdog fired. The watchdog did its job (a loud stop rather than silent
corruption), and the admissibility check means a service cannot reach that state through this
capability. But if some future access can still abort, the outcome is a panic and not a refusal, and
that is worth knowing before granting a wider hardware capability to anything.


**Read-only is a PERMANENT boundary, not a starting point.** CF8/CFC looks like two ports but CF8
selects WHICH register CFC reaches, so CFC write authority is effectively write access across the
whole of PCI configuration space - every BAR, every command register, every device. There is no
narrower form of it at port granularity, because the target is chosen by data rather than by the
interface. So if a future need requires configuration-space mutation - destructive BAR sizing is the
obvious one - CFC write MUST NOT simply be added. A different mechanism must be designed and justified
separately. Godspeed does not currently need sizing (`mmio_len` comes from a fixed page count per
class, not from probing), so there is nothing to solve today and no reason to solve it speculatively
(§26.2).

**CF8/CFC must have ONE exclusive holder.** The pair is STATEFUL: a write to CF8 selects, a read of
CFC retrieves. Two independent holders do not merely race, they silently read each other's device:

```
A: OUT CF8, addr_A
B: OUT CF8, addr_B
A: IN  CFC          <- A reads B's register, and nothing anywhere says so
```

That is a silent wrong answer, which is worse than a denied one (invariant 12). The holder is
`hw-enumerator`, and there is no kernel exception - see the next section for why the kernel does not
need config reads either.

### Address ADMISSIBILITY, not device PROVENANCE

Moving enumeration out raises a second question, and conflating two guarantees is the trap:

```
admissibility:  "May this physical range EVER be granted as MMIO?"
provenance:     "Does this range actually belong to THIS device?"
```

**The kernel guarantees the first and explicitly declines the second.** It checks a claimed range
against the memory map it already owns and refuses anything intersecting usable RAM, the kernel image,
service memory or the frame pool. No PCI interpretation, no config read - so the exclusivity rule above
holds without a kernel carve-out, and the check is arch-neutral (Limine on x86, device tree on ARM).

The rule this enforces:

> A service may IDENTIFY a physical address. It may not thereby ACQUIRE AUTHORITY over that address.

**The residual, stated rather than disguised.** Admissibility does not prove ownership. A compromised
`hw-enumerator` could still claim *driver A -> device B's window*, provided B's window is itself an
admissible non-RAM region. What it can never do is turn protected memory into MMIO authority. That
reduces the catastrophic case ("userspace can mint authority over arbitrary physical memory") to a
materially smaller one ("the discovery service may misassociate one hardware window with another"),
and the residual is the same posture §6.4 already accepts for an unconfined DMA driver. If the stronger
claim is ever needed, it belongs to device-scoped IOMMU authority - not to dragging PCI parsing back
into ring 0 to strengthen today's check.

**Two implementation hazards, because both are wrong-by-default:**

1. **Unknown classification must DENY.** The memory map's match ends in a catch-all
   (`_ => MemoryKind::Reserved`), which today absorbs ACPI NVS, bad memory, the framebuffer, and every
   firmware type that does not exist yet. Written as a denylist ("deny RAM, kernel, services, pool;
   admit the rest"), the check ADMITS all of those and silently widens as new types appear. A security
   check must default to deny.
2. **The check applies to the PAGE-ALIGNED granted range**, not the claimed range. MMIO is mapped in
   pages; a window sharing a page with a denied region leaks that region even though the claim passed.

**One empirical question, answerable from a boot dump rather than by reasoning:** do real BARs fall
inside a firmware-DESCRIBED region, or in an undescribed gap? If they sit in gaps (common - the PCI
hole is often not described), the clean rule is *admit iff the range overlaps no described region*,
complete by construction and needing no per-type list. If firmware describes the hole as reserved, that
rule denies legitimate BARs and the positive-classification form is required instead. The T630 and the
Wyse may differ; dump both before fixing the wording.

### "Not in `hw-enumerator`" does not mean "must be in the kernel"

The prohibition on CFC writes does not imply that configuration-space mutation belongs in ring 0. It
means such mutation is not `hw-enumerator`'s. Where it belongs is a separate decision requiring its own
justification, and a future constrained probe or separate service may be the right answer.

This is worth stating as a rule because the failure mode is structural: without it, every prohibition
on a service becomes an argument for the kernel by default, and the kernel grows by exclusion - one
carefully-reasoned exception at a time, each individually defensible.

### D2 (BUILT 2026-09-02): `hw-enumerator` walks the bus in userspace

Implemented, proven in QEMU on x86, and on hardware on the Pi 4. The kernel gained ONE gated syscall
- `PciCfgRead` (53) - behind a new `PCI_CFG` authority. It learns a selector and a register offset,
two opaque numbers, and nothing else: not what the selector names, not how to walk a bus, not what a
class code identifies, not where a BAR lives. All of that moved to `services/hw-enumerator`.

**The proof is the same cross-check that made D1 safe**: two independent walks of one bus must agree.
On q35 the kernel's own scan records 8 devices and the userspace walk finds the same 8, with the NIC,
xHCI, AHCI and EHCI each cross-checked `AGREES`:

```
hw-enumerator: probe 00:00.0 vendor/device = 0x29c08086
hw-enumerator: 00:02.0 class 0x020000 vendor 0x8086 device 0x10d3 bar0 0xfeb80000 irq 11
hw-enumerator: 00:04.0 class 0x0c0330 vendor 0x1b36 device 0x000d bar0 0xfebd4004 irq 10
hw-enumerator: 8 device(s) found by USERSPACE enumeration
pci: device table - 8 device(s) recorded (generic, no class knowledge)
pci:   NIC class 0x020000 AGREES (BAR0 0xfeb80000 BDF 0x0010)
```

**TWO PLATFORMS, ONE SERVICE, AND THAT IS THE WHOLE CLAIM.** x86 reaches configuration space through
the CF8/CFC ports; the Pi 4 through a memory-mapped window on its root complex. Both are an
index/data pair, so the same syscall and the same capability serve both. What differs is the selector
encoding, and that lives entirely in the service:

```
x86    bus<<16 | dev<<11 | func<<8      (mechanism #1)
Pi 4   bus<<20 | dev<<15 | func<<12     (root complex config window)
```

A third platform with a third layout needs no kernel change at all. That is the assertion D2 exists to
make, and it now has two data points rather than one.

**Why the kernel mediates on the Pi 4 instead of just granting the window.** The cheaper answer - map
the root complex into the service and let it drive the pair itself - cannot be made safe there. MMIO
grants are PAGE-granular, and the config INDEX register at 0x9000 shares its 4 KiB page with the
bridge's software-reset control at 0x9210. Granting the page so a service could READ config space
would also grant it the power to reset the root complex out from under every device on it. Register
granularity is only available from inside the kernel, which is the same argument the x86 side makes
about port granularity.

**Read-only, permanently.** The capability is minted with `READ` alone - there is no write operation
behind it, so granting `WRITE` would be a right nobody can exercise and exactly the kind of thing a
later change quietly finds a use for. The syscall and the authority are PINNED in
`COMMANDMENTS.baseline.toml`, each answering the checker's question ("why isn't this a service?" - it
is; reaching config space needs a privileged instruction or a kernel-only mapping no ring-3 code has,
the same shape as the RTC read `time` needs).

The Pi 2 has no PCI at all, so there the kernel can only refuse - which it says, rather than handing
back a plausible zero a caller would read as an empty machine.

**What it does NOT do yet.** This is ADDITIVE. `kernel/src/arch/x86_64/pci.rs` still runs and still
does the boot scan - nothing consumes the userspace results for anything load-bearing. Retiring those
the SCAN is the next step - about 870 of the file's ~1,320 lines, the rest being mechanism that stays
(see above) - and it wants the two walks agreeing on REAL HARDWARE first, not only in QEMU. That is the same discipline D1 followed: record, cross-check, and only then switch over.

**THE WALKS NOW AGREE ON EVERY MACHINE (2026-09-03), and getting there took a hardware round trip.**

| machine | kernel scan | userspace walk | verdict |
|---|---|---|---|
| QEMU q35 | 8 | 8 | identical, device for device |
| Pi 4 | 1 endpoint | 2 (bridge + endpoint) | agrees; the extra is the bridge itself |
| Wyse (Intel Gemini Lake) | 15 | 15 | identical, device for device AND class for class |

The Wyse first reported 15 against 14. My two guesses at the cause - the `slots_on` slot rule, or the
`MAX_BUS` bound - were both wrong, and neither could be checked because the table printed only a COUNT.
Printing it per device turned the question into a diff, and the diff named the device in one line:
`00:0d.2`, a serial-bus controller on bus 0 with **no function 0 above it**. The walk broke out of the
function loop on an absent function 0, under a comment asserting "no function 0 means no device here at
all" - a rule PCI does not have and Intel chipsets routinely break.

So the gate D3 was waiting on is OPEN: the two walks agree on every machine available, which is what
"record, cross-check, and only then switch over" asked for. What remains for D3 is design, not data -
see the assignment/re-enumeration split below, and cost 2, which is still unresolved.

The Wyse also produced a false `NIC ... DISAGREES` on a machine whose networking was perfect: the
cross-check compared `bar[0]` against a BAR_AUTO-resolved address, which asks whether BAR0 equals BAR2
on every RTL8168. Fixed, and worth stating as a rule the rest of D depends on - **a cross-check that
fails on a healthy machine destroys the property it exists to establish.** The whole switch-over plan
rests on trusting these two lists; a list that cries wolf is worse than no list. The table now prints
one line per device so the two can be diffed rather than compared by count.

**Adding one service needed its name in SIX places**, and this is worth writing down because nothing
checks that they agree: `IMAGES` (the image), `MANAGED` (reconcile), an `ensure_mapped` call (BOOT -
`MANAGED` says what to RESTART, not what to START), the restart-counter set, the death-notification
set, and the privilege MINT. Each list is correct on its own terms. Three of the six were missed on
the first attempt; the enforcement layer caught them, including a privilege that was checked and
delegatable but never minted - which failed SILENTLY, because the SDK wrapper read any non-negative
syscall return as success, so the service saw "success" and read zeros.

### D3 (IN PROGRESS 2026-09-03): retiring the scan - two slices built, two to go

The gate D3 waited on is open (the walks agree on every machine, see D2 above). What follows is the
switch-over, and two of its four pieces are built and cross-checked. Both are deliberately
NON-LOAD-BEARING: the kernel still resolves every driver's device from its own scan, and nothing here
decides anything yet. That is the same discipline D1 and D2 followed - record, cross-check, and only
then switch over.

**Slice 1 (BUILT): the supervisor asks `hw-enumerator`, so the reporter has a client.**

Its request/reply loop had been written before anything called it - the speculative-feature mistake
(§26.2) - and dead code cannot be trusted to work on the day something depends on it. D3 depends on it:
nothing can be retired until the supervisor OBTAINS the device list rather than the kernel scanning
for it. The supervisor now queries it at boot and logs the result, giving the third leg of the
cross-check:

```
kernel scan       8
userspace walk    8
supervisor (IPC)  8      identical, device for device (QEMU q35)
```

It is best-effort by construction - every failure path logs and returns rather than retrying, because
a supervisor that blocks on a reporter cannot spawn the services the machine needs. The peer cap comes
from `reacquire_by_name`, NOT the handle `ensure_mapped` holds: `acquire_send_grant_cap` returns a
handle without recording it in the SDK's send-cap cache, and `request_with_reply` resolves peers
through that cache, so a request on the map's handle finds no slot and fails instantly. Same trap as
the silent `0 sectors` from `dwc2`.

**Slice 2 (BUILT): the kernel derives BAR and IRQ from a BDF, and it agrees with the scan.**

This is the answer to cost 2, and it rejects the binary that cost was framed as. `bar_and_irq_from_bdf`
takes a bare BDF and reads that device's first memory BAR and its IRQ line straight from config space;
the boot log cross-checks it against the scan on every device and reports
`derive-from-BDF vs scan: AGREES on every device`.

Why neither original arm was right:

- a reported VECTOR is an authority-bearing value the kernel must TRUST, and a range check cannot
  catch a wrong one. A NIC handed vector 1 receives the keyboard's interrupts - SEC-2's residual
  arriving through a new door.
- keeping the kernel's read means keeping the scan that FINDS the device, so nothing is retired and
  the option is only "smaller" if its 870 lines are not counted.

A reported BDF is neither: it is an IDENTIFIER, and the kernel answers the authority question itself
by reading the device's own registers. Exactly the rule already settled for addresses - **a service may
identify a device; it may not thereby acquire authority over that device.** And it grows no kernel
responsibility: config reads, MSI programming and interrupt routing are all already the kernel's
(§4.3). What leaves is the SCAN, which is semantics.

**Slice 3 (NOT BUILT): the supervisor passes the BDF in the spawn request.** The kernel then resolves
MMIO and IRQ from that BDF instead of from `find_by_class`.

**Slice 4 (NOT BUILT, and the fiddly one): the assignment / re-enumeration split.** Designed below and
not yet implemented. A restarted `hw-enumerator` must never reassign a BAR out from under a live
driver, so re-enumeration has to be read-only and idempotent by construction. This is the piece with
real subtlety; the rest is plumbing.

Only after 3 and 4 does the scan come out.

### The service is a REPORTER, and must stay one

The greatest risk to this service is not a bug. It is that the only component able to see the whole bus
is an obvious home for anything bus-shaped, and each addition will look reasonable on its own: power
states, hot-plug policy, BAR rebalancing, MSI vector allocation, AER handling, an `lspci` surface,
"while you are in there, could you also...". That is §26.1 erosion with a different subject, and the
kernel is only small because something says no on its behalf.

So the same discipline applies, and the same way - **by capability, not by policy**:

> It READS config space and REPORTS what is there. It does not decide, assign, configure, or own.

The load-bearing constraint is that it holds almost nothing:

| it holds | it must never hold |
|---|---|
| read access to config space | `SPAWN` / `IMAGE_SPAWN` - it starts nothing |
| its own recv endpoint | `SERVICE_CONTROL` - it stops nothing |
| a log | any MMIO or DMA grant - it drives no device |
| | **write** access to config space - see below |

Most of the funky things are then IMPOSSIBLE rather than forbidden, which is the only kind of "no"
that survives a determined contributor. Deciding which driver claims a device stays with the
supervisor, where it already lives: the reporter says "BDF 00:14.0 is class 0x0c0330", and something
else decides that means `xhci`. A reporter that also chose drivers would be a policy engine.

This is checkable today, without new machinery: a service's authorities are declared in its contract
and reconciled against the kernel by `scripts/contract_check.py`, so a new grant cannot appear
quietly. A second, cheap pin on its IPC opcode list would catch growth that needs no new authority;
worth adding when the service exists, not before (§26.2).

**The name matters more than it looks.** "hw-enumerator" invites management - it names the thing after a
role that has no natural boundary, and the first person to propose power management will be right, by
its own name.

**The service is `hw-enumerator`.** It names the ACTION and nothing else, so "the enumerator should
also configure the device" reads as out of scope on sight - which is the whole job of the name.

The prefix is `hw-`, not `device-`, because that is already this codebase's word for the domain:
`hw_device`, `hw_class`, `hw_mmio`, `hw_irqs`, `hw_flags`. A `device-` prefix would be a second name
for the same concept. It also stays true on arm32, which has NO BUS at all - the DWC2 is soldered to
the SoC - so a name built on "bus" would be a misnomer on one of the four machines while "hardware"
holds everywhere.

Two candidates were ruled out on facts rather than taste. `hw-probe` / `device-probe` COLLIDE with the
existing `probe` service and its 17 `probe-*` rows - the name would read as one of them in logs and in
the kernel name directory. `hw-discovery` invites exactly the probing logic that must not accumulate
here, which is the same failure as "manager" one step quieter.

The road not taken, recorded because it may be the better answer if this service ever starts to drift:
`hw-inventory` names the OUTPUT rather than the action, and a list cannot act at all - marginally
stronger against creep, at the cost of being less obvious to a newcomer.

**Read-only is the goal and may not be free.** On x86 firmware assigns BARs before the OS runs, so
reading is genuinely enough. On the Pi 4 the kernel currently ASSIGNS them
(`pcie: xHCI BAR0 assigned bus 0xf8000000 -> CPU 0x600000000`), and assignment is a write. If that job
moves too, the service acquires write access and with it the power to point any device anywhere - a
materially bigger thing to trust, and the door through which "manager" walks back in. Keeping
assignment where it is (the boot path, once) and the reporter read-only is the better split, and the
"assignment once, re-read always" problem below is exactly why.

### Why there is no signature on any of this, and when that changes

A reader who knows §16 ("Signature valid? No -> Reject") could take the absence of signature
verification here for an oversight and go add one. It is not, and adding one now would be the §26.2
speculative feature - so the reasoning is recorded rather than left to be re-derived.

**Every service ships INSIDE the kernel image.** `hw-enumerator` is embedded in the supervisor
(`include_bytes!`, 29 of them), the supervisor is embedded in the kernel, and no image is read from
disk, filesystem or network by any path that exists today. One artifact.

**For that case embedding is STRICTLY STRONGER than signing, not a substitute for it.** A signature
proves "these bytes are the ones that were signed"; embedding makes them the same bytes as the kernel,
so there is nothing left to prove. Signing the inner parts is also circular - the verifier lives in the
image it would be verifying, so anyone able to alter a service's bytes can alter the check, or the
kernel. And there is no load step, so none of the verify-then-use hazards a signature scheme has to be
careful about exist here.

What actually goes wrong with an embedded artifact is INCOHERENCE - a stale service embedded beside a
fresh kernel - and that is already mechanically enforced by `embed_order_check.py` (the supervisor must
be newer than every service it embeds) and `service_embed_check.py` (every managed service is embedded
for real). Those catch the realistic failure; a signature would not even look at it.

**THE TRIGGER, stated precisely, because the answer flips the moment it is crossed:** embedding
suffices for exactly as long as every service ships in the kernel image. The day one arrives
independently - an update, an image loaded from disk, anything over a wire - the artifact stops being
one thing, and §16's verification becomes mandatory rather than optional. That is not hypothetical:
§16 specifies it and it is spec-only today.

**What a signature would NOT have bought either way**, and the reason it does not bear on D's residual:
it covers the artifact at rest, not the process at runtime. `hw-enumerator` parses config space supplied
by whatever hardware is plugged in; a device feeding it something that triggered a bug leaves the
signature perfectly valid while the running service misbehaves. This is the same shape as SEC-2 -
IOMMU confinement bounds a driver's DMA, not its output. The service is safe Rust with no `unsafe`
(mechanically enforced), so the realistic failure is a BUG, not a compromise, and no signature catches
a bug.

### The cost that grows with every step of D: IPC

Each responsibility that leaves the kernel turns a syscall into an IPC round trip, and an IPC round
trip costs roughly 5-10x a syscall - not from inefficiency, but because it is two privilege
transitions and two context switches rather than one transition and a return.

That is affordable for what D moves, and the reason is FREQUENCY rather than cost: `ask_bdf_for_class`
runs about six times a boot. The test to apply before moving anything else is "does this become an IPC
in a loop, or an IPC once at setup?" - and the measured numbers, the ranked efficiency backlog, and
what is permanently off the table (zero-copy, per §2.5) are in `docs/ipc-efficiency.md`.

### The two costs still to accept before building

**1. Config-space access for the reporter. PAID (2026-09-02).** One gated syscall, `PciCfgRead`, behind
a `PCI_CFG` capability minted READ-only - see D2 below for the shape and why it is one operation rather
than a select/read pair. The kernel gained one syscall, not two, and the authority cannot write.
The original statement of the cost follows, because the trade it describes is what was accepted:
Pi 4 ECAM is an MMIO window - no new mechanism. x86 uses
I/O ports `0xCF8`/`0xCFC`, and userspace cannot execute `IN`/`OUT`, so this needs a port-I/O capability
or a gated syscall: a NEW KERNEL AUTHORITY, and the pin grows again. Defensible - it is the mechanism
that lets the bus scan - about 870 lines - leave ring 0, which is the same trade `FIRE_IRQ` made for
123 lines - but it is a real addition and belongs in the decision, not in the implementation.

**2. The interrupt.** A vector is authority (`task::hw_irqs_for`), and the kernel currently states it
rather than accepting it. Under D the device's IRQ line comes from config space, which the kernel no
longer reads. Either the reporter reports it and the kernel bounds-checks the vector against the range
it is willing to route, or that single fact keeps a foot in the kernel. The first is consistent with
the address decision above; the second is smaller. Unresolved, deliberately.

### The fiddly part of D: assignment once, re-read always

Re-enumeration after a restart must not disturb live drivers. On the Pi 4 today the kernel does not
merely read the bus, it **assigns**:

```
  pcie: xHCI BAR0 assigned bus 0xf8000000 -> CPU 0x600000000
```

If `hw-enumerator` inherits that job, dies, and restarts while `xhci` is running against the window it
was given, a naive re-scan that reassigns BARs pulls the address out from under a live driver.

So enumeration splits in two, and the split is a hard requirement rather than an optimisation:

| phase | when | what it may do |
|---|---|---|
| **assignment** | a device with nothing programmed | write BARs, enable bus-master |
| **re-enumeration** | every restart after that | **read only** - report what is already programmed |

Idempotent by construction: read the BAR, and if it is already programmed, keep it and report it.
This is the genuinely fiddly part of D, more so than config-space access itself, and it wants
designing in rather than discovering the first time `chaos` kills `hw-enumerator` mid-transfer.

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
the supervisor obtains those values from a kernel discovery query; at D, the hw-enumerator supplies
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
  hw-enumerator SERVICE enumerates, reports BDF / class / BAR / IRQ      <- step D
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
| **C (done)** | **1** | **no kernel change** | kernel change | no |
| **D1 (done)** | 1 | no kernel change | **no kernel change** (polled drivers) | no |
| D1b | 1 | no kernel change | **no kernel change** (+ interrupts) | no |
| D2 | 1 | no kernel change | no kernel change, and no bus code in the kernel | no |
| 2 | 1 | no kernel change | no kernel change | **yes** |

**Step C is built, and the pin is at its target: 1.** `supervisor` alone, because the kernel must
bootstrap it - nothing is beneath it. Every other service's image, memory limit, placement, peers,
privileges and device class lives in the supervisor's `IMAGES` table.

The kernel embeds ONE service image now, down from 30. A bare-metal image also embeds no test probe
at all, which is checkable rather than asserted: the probe-only strings are present in `os.img` and
absent from `os-usb.img`.

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
- **CLOSED: the name-squatting regression this work caused.** `spawn_probe` let a SPAWN holder choose
  the NAME of the task it started while the KERNEL supplied the probe image, and refused "a real
  service's name" by asking the kernel's service catalogue - which step C emptied, silently shrinking
  the refusal set to `{supervisor, probe}`. It mattered because the name directory is the recovery
  anchor clients reacquire through (§14.3): a probe registered as `fs` collects `fs`'s clients while
  the real `fs` is dead, which is exactly when a liveness check passes.

  It was first patched with a permanent name RESERVATION in `ipc::names`, and that patch has since
  been REMOVED - not reverted in retreat, but superseded. Moving the probe image closed the hole
  structurally: a name no longer carries an image, it SELECTS a table row, so a caller cannot bind an
  arbitrary name to the probe binary at all and an unknown name is refused outright. The reservation
  had no callers left, and a dead guard that still looks live is worse than none. Pinned by A9b.

  What REMAINED is the step-C widening proper, and it was wider than the probe path was: `SpawnImage`
  takes a caller-supplied image, so a SPAWN holder could start ARBITRARY BYTES under a real service's
  name in the window while that service is dead, bounded only by `AlreadyRunning`.

  **The interim named here is now BUILT: `SpawnImage` is gated behind `IMAGE_SPAWN`**, a capability
  only the supervisor holds. Nothing else legitimately calls it - shell, chaos, control and the
  probes all ask the supervisor instead - so the gate costs nothing and removes "any SPAWN holder"
  from the sentence above. Holding SPAWN now lets a caller start a service the supervisor already
  knows; introducing NEW CODE takes a separate capability.

  That narrows the window rather than closing it: the supervisor is still trusted with arbitrary
  bytes, so a runtime-compromised supervisor can still introduce code. Step 2 (signed packages) is
  the answer to that, and remains open. The honest statement is that step C's widening is now
  bounded to ONE principal instead of every SPAWN holder (§26.7).

- **`probe` is the last non-supervisor row.** It needs two things, both of which fit the existing
  model: a `SPAWN_FLAG_PEERS_GRANT` bit (`probe-5a-send` gets grantable peer caps, which the
  supervisor already holds and so may pass on), and a class for the TEST interrupt line that
  `probe-11a` receives - the kernel supplying vector 33 from the class, exactly as it does a
  device's. The work is larger than it looks because the 193 probe spawns go through `Spawn`'s
  packed-parameter path rather than `SpawnImage`.
- **Step D's arch shape** is unmeasured. x86 I/O-port config space and Pi 4 ECAM are known
  quantities; arm32 has no PCI, so the hw-enumerator is x86 + Pi 4 only and `dwc2` keeps its
  SoC-presence path.
- **Step 2's key management** has no design yet, and it is process as much as code.
- **A9-4, the BSP idle wedge**, is unrelated to this work and remains open and deferred. It did not
  recur on any of the four hardware runs.

- **DONE: `probe` moved (pin 1).** The two authorities that kept it in the kernel both found homes in
  the existing model - the IRQ route as a device CLASS (`hwclass::TEST_IRQ`, the kernel still states
  the vector) and the grantable peer caps as a spawn FLAG - so no constitutional amendment was
  needed. Its 193 spawns route through the supervisor, which holds the image; a probe respawning its
  own victim ASKS, one-way, and confirms by watching the victim register its name.

---

## 11. Related

- `docs/probe-params-design.md` - step A in full, including the as-built ABI and the hardware table
- `docs/naming-design.md` - Path C: how naming left the kernel, and the recovery-directory argument
- `CLAUDE.md` 4.3 / 4.4 (kernel scope), 6 (TCB), 16 (update model), 26.2, 26.7, 26.10
- `COMMANDMENTS.baseline.toml` - the `service_configs` pin that makes all of this enforceable
