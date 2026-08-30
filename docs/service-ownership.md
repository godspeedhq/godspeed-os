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
| C | 1 | **no kernel change** | kernel change | no |
| D | 1 | no kernel change | **no kernel change** | no |
| 2 | 1 | no kernel change | no kernel change | **yes** |

---

## 10. Open items

- **The CLAUDE.md amendment for step C** must state the widening in section 4's words: after C, a
  runtime-compromised supervisor can introduce new code, and nothing before step 2 prevents it.
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
