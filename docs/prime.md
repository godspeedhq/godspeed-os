# GodspeedOS Prime - the minimal, self-installing, portable core

> **Status:** Design doc, non-normative, **not yet built**. Records the
> "GodspeedOS Prime" concept and its self-install / self-replication model,
> decided in conversation. Builds on `docs/persistence.md` (GSFS), `docs/ahci.md`
> (block driver), and `docs/drives.md` (the `drives` utility). Trails `CLAUDE.md`;
> does not amend it.

## 1. What Prime is

**GodspeedOS Prime is the irreducible core of GodspeedOS given a name and a
deployment story.** It is the smallest thing that can boot, talk to you, and
*reproduce itself onto a drive*.

Prime = the constitution's non-restartable trusted root (§6.1) **plus exactly the
utilities needed to run and to make itself portable** - nothing more:

| Layer | Members | Why it's in Prime |
|-------|---------|-------------------|
| Kernel + arch + smp | the microkernel | the mechanism (§6.1) |
| `init` · `supervisor` · `registry` | trusted root | bootstrap + lifecycle + naming (§6.1) |
| `block-driver` · `fs` · AHCI | storage stack | read/write drives - needed to install & carry state |
| console + keyboard driver | interaction | a usable prompt (§B.3) |
| `shell` | the prompt | where you type commands |
| `drives` | drive utility | flash data drives + **install/replicate Prime** (`docs/drives.md`) |

Everything *beyond* Prime - your apps, networking, your data - is **content**, not
Prime. Prime stays whiteboardable (§26.11): "boot, interact, reproduce."

> **Prime ⊇ TCB, but it is not the same set.** The TCB (§6.1) is *what must be
> trusted*. Prime is *what ships in the minimal bootable core*. `block-driver`/`fs`
> are in Prime because you can't install or carry state without them; their TCB
> status is the separate Phase-3 question (§6.3).

## 2. Anatomy of a bootable GodspeedOS drive

A drive GodspeedOS can **boot from** has two regions:

```text
  ┌─────────────────────────────┬───────────────────────────────────────────┐
  │  Boot region - ESP (FAT)    │  GSFS region                              │
  │  Limine + Prime kernel image│  data + (later) your services/state/config│
  │  firmware boots THIS         │  (docs/persistence.md, hierarchical GSFS) │
  └─────────────────────────────┴───────────────────────────────────────────┘
```

- **Boot region (ESP):** a small FAT partition with **Limine + the Prime kernel
  ELF + `limine.conf`** - exactly what `osdev image` writes today on the host. The
  firmware boots this. It must be FAT because Limine can't read GSFS (it's ours).
- **GSFS region:** the hierarchical GSFS filesystem (`persistence.md §6.2`) - the
  drive's data, and eventually the services/state that make it *your world*.

A **data-only drive** has just a GSFS region (no boot region). A **bootable
GodspeedOS drive** has both.

### 2.1 At a glance

A bootable Prime drive - note the **two interchangeable kernel slots** (A/B, §8) in
the boot region:

```text
╔══════════════════════════════════════════════════════════════════════╗
║              A bootable GodspeedOS Prime drive  (GPT)                ║
╠══════════════════════════════════════════════════════════════════════╣
║  BOOT REGION ── ESP (FAT) ── the firmware boots THIS                 ║
║  ┌────────────────────────────────────────────────────────────────┐  ║
║  │  Limine  +  limine.conf   (default ─▶ active slot)             │  ║
║  │    ▸ slot A : kernel_a.elf      ● ACTIVE    (running now)       │  ║
║  │    ▸ slot B : kernel_b.elf      ○ inactive  (fallback / target)│  ║
║  └────────────────────────────────────────────────────────────────┘  ║
║        = PRIME = mechanism (kernel + TCB + drives/shell/storage)     ║
╠══════════════════════════════════════════════════════════════════════╣
║  GSFS REGION ── your WORLD = content                                 ║
║  ┌────────────────────────────────────────────────────────────────┐  ║
║  │  /services   /state   /config   /data  …                       │  ║
║  │  label:"ssd"      DEFAULT ✓     (identity + auto-mount)         │  ║
║  └────────────────────────────────────────────────────────────────┘  ║
╚══════════════════════════════════════════════════════════════════════╝
   Prime travels; the World travels on top of it; the machine is fungible.
```

Deployment & propagation - install once, then never re-flash:

```text
   [ USB stick : Prime ]
          │ boot
          ▼
   GodspeedOS running ─────────────────────────────────────────────────┐
          │                                                             │
          │  drives install 0   (stamp self-carried image → GPT+ESP+GSFS)
          ▼                                                             │
   [ Internal SSD : Prime A/B ] ──boot, no USB──▶ GodspeedOS            │
          │                                          │                  │
          │  drives update 0                         │  drives install 1
          │  (new kernel → inactive slot → reboot)   ▼                  │
          ▼                                  [ Spare drive : Prime ]    │
     runs NEW kernel                            unplug → carry →        │
     (old slot = rollback)                      boot on ANY machine ────┘
```

Legend: `●` active · `○` inactive · `install` = whole-drive raw stamp (no FAT) ·
`update` = in-place A/B slot swap (minimal FAT write).

## 3. Three verbs: `flash`, `install`, `update`

The distinction the whole model rests on:

| Command | Makes | Result |
|---------|-------|--------|
| `drives flash <n> [label]` | a **data** drive (GSFS only) | files; not bootable |
| `drives install <n>` | a **bootable GodspeedOS** drive (boot region + GSFS) | the machine can boot GodspeedOS from it |
| `drives update <n>` | a new kernel in the **inactive A/B slot** of an existing Prime | reboot boots the new kernel; the old stays as fallback (§10) |

`install` is **self-replication** (GodspeedOS writing GodspeedOS onto a drive);
`update` is **self-update** (GodspeedOS replacing its own kernel, safely, §10).

## 4. The flow - boot, self-install, propagate

```text
  1. USB (Prime) boots GodspeedOS
        │  drives install 0
        ▼
  2. Internal SSD is now a bootable GodspeedOS
        │  remove the USB, reboot → boots off the SSD (no USB needed)
        ▼
  3. SSD GodspeedOS
        │  drives install 1
        ▼
  4. Drive 1 is a PORTABLE GodspeedOS
        │  unplug it, carry it
        ▼
  5. Plug into ANY machine → boot GodspeedOS from it
        │           or
        └─ on another GodspeedOS box: copy / merge it into the host's primary
```

You install GodspeedOS once from USB, then never need the USB; and any GodspeedOS
instance can mint more portable GodspeedOS drives. The **mechanism (Prime) is fixed
and identical everywhere; your instance travels** - identity over location, for a
whole OS (invariant 11, scaled up).

## 5. Carrying your world (run programs off a drive)

Prime boots the *mechanism*. The richer vision - "plug my drive into any GodspeedOS
and **continue from there**" - layers on top: after Prime boots, the supervisor
**loads additional services from the drive's GSFS region** and spawns them, and
services reconstruct their state from GSFS (§15). Today services are baked into the
kernel image; loading-and-running from `fs` is the one capability that unlocks the
portable *world* on top of portable *Prime*.

This is the **update model (§16) generalized**: §16 is "restart a service with a new
binary"; this is "load a service's binary from a drive." Same principle - the binary
is data, authority comes from capabilities, the kernel just runs what it's handed -
pointed at a *pluggable* drive instead of a fixed manifest.

So the layering is clean:
- **Prime** = portable *mechanism* (kernel + TCB + utilities), via `drives install`.
- **World** = portable *content* (your services + state) in a drive's GSFS region,
  loaded on top of whatever Prime booted.

## 6. Self-replication: Prime is *self-carrying* (resolved)

`install` means **writing a bootable layout from inside GodspeedOS** - a GPT, a FAT
ESP (Limine + kernel), and a GSFS region - exactly what `osdev image` does **on the
host**, now self-hosted. The crux was: where do the boot-image bytes come from? A
constraint settles it:

> **GodspeedOS cannot read the medium it booted from.** `block-driver` is an
> **AHCI/SATA** driver; the USB you boot Prime from lives on the **xhci/ehci**
> controller - a different device entirely. So "read the boot ESP back from the boot
> medium" is impossible for the first USB→SSD install (it would need a USB
> mass-storage driver). The bytes must come from inside Prime.

**So Prime *carries* a copy of its own bootable image** and stamps it onto any target
(raw block writes - the ESP is an opaque blob, so **no FAT *read/write* needed for
install**). `install` = write GPT → stamp the carried boot image into the ESP region →
make a fresh GSFS data partition. Source-medium-independent.

The mild recursion (an image of yourself contains a kernel that contains the image) is
closed two ways; **v1 picks the simpler (§26.2/§26.13):**
- **One-version-behind (chosen):** Prime carries the *previous stable* Prime image.
  Dead simple, no build-time fixed-point; the freshly-installed copy is one rev old
  until it re-installs. Fine because Prime changes rarely.
- *(Alt)* Compression fixed-point: a compressed self-image converges to a small fixed
  point so `install` always writes *current* Prime - needs a build-time iteration.

None of this touches the kernel's `unsafe` story - block writes + format construction
in userspace (`drives` + `fs` + `block-driver`).

> **Caveat (see §10):** *install* needs no FAT logic (raw blob stamp), but in-place
> A/B *update* - swapping one kernel slot and flipping the boot default - does need a
> **minimal, bounded FAT writer** (overwrite a known file + edit `limine.conf`). So a
> fully self-updating Prime carries a small FAT writer after all; the raw-stamp covers
> the whole-ESP case, the FAT writer covers the per-slot case.

## 7. Why this fits the constitution

- **Prime = the TCB (§6) with a name and a deployment story.** It doesn't add trust;
  it packages the minimal core.
- **`install` = the update model (§16) generalized** from "replace a service" to
  "write the whole OS to a drive." Verification-before-use still applies (§16): an
  install/boot image should be checked before it's trusted.
- **Small and understood (§2.1, §26.11):** Prime is defined *by* minimalism. The
  temptation will be to grow it; the discipline is to keep it "boot, interact,
  reproduce" and push everything else into *content*.
- **Identity over location (invariant 11):** a GodspeedOS drive is a portable
  identity; the machine is fungible. This is the project's core principle applied to
  the OS instance itself.

## 8. A/B kernel self-update (the dev-loop feature)

The point that makes Prime worth it day to day: **GodspeedOS updates its own kernel
safely, without re-flashing a USB.** Build a new kernel, push it into the *inactive*
slot, reboot into it - and if it's bad, the old one is still there. This is the
**A/B-slot-with-rollback** scheme (Android / ChromeOS / CoreOS), and it is the
constitution's **§16 update model applied to the whole kernel** instead of one service
("write a new binary, verify before trust").

### 8.1 The model

- A Prime drive has **two kernel slots, A and B.** One is *active* (running), one
  *inactive*. The active slot is never touched by an update - a bad build cannot brick.
- **`drives update <n>`** writes the new kernel to the **inactive** slot, then flips
  "which slot boots next."
- **Reboot auto-selects the new slot.** If it fails, you fall back to the old.
- Verbs: `install` makes a *new* bootable drive (full GPT + ESP + GSFS); `update` swaps
  the inactive A/B kernel slot *in place* on an existing Prime.

```text
  drives update 0
  (1) running          (2) write inactive      (3) flip default       (4) reboot
  ┌──────────────┐     ┌──────────────────┐    ┌────────────────┐    ┌──────────────┐
  │ A ● active   │     │ A ● active (safe)│    │ A ○            │    │ A ○ fallback │
  │ B ○ old      │ ──▶ │ B ◀ NEW kernel   │──▶ │ B ◀ default    │──▶ │ B ● active   │
  └──────────────┘     └──────────────────┘    └────────────────┘    └──────────────┘
       old build           A untouched            atomic flip            NEW kernel
                          (can't brick)        (edit limine.conf)            │
                                                                             │
     new kernel bad / hangs?  ──reboot──▶ Limine menu picks A  ◀─────────────┘
                                          ( = rollback, old kernel back )
```

### 8.2 The honest cost: flipping the slot needs to touch the boot region

`install` is a raw whole-ESP stamp (no FAT logic, §6). But `update` must **write one
kernel file and change the boot default**, which means modifying the boot region. Two
ways, and this is the key sub-decision:

1. **Two kernel files in one ESP + a *minimal* FAT writer (chosen for v1).**
   `kernel_a.elf` / `kernel_b.elf`; `update` overwrites the inactive file and edits
   `limine.conf`'s default. The FAT writer is *bounded* - "overwrite a known file,
   edit a tiny config" - not a general filesystem. Tractable, and it admits the honest
   truth that a self-updating OS needs a little FAT-write (§6 caveat).
2. *(Alt)* **Two whole-ESP partitions, each raw-stamped + UEFI `BootNext`/`BootOrder`.**
   No FAT logic, but needs UEFI runtime-variable access from a post-boot microkernel -
   fiddly, and bootloader-agnostic (does not depend on Limine).

### 8.3 Rollback

- **v1 - the Limine menu** (short timeout): if the new slot hangs, pick the old one.
- **v2 - boot-count auto-rollback:** mark the new slot "trial"; if userspace doesn't
  confirm "boot OK" within N boots, the bootloader reverts to the known-good slot. This
  is the real safety net and the right long-term shape.

### 8.4 Where the new kernel comes from

- **Now:** a kernel image on a **data drive / GSFS file** that `update` reads - already
  a win (a file copy, no USB re-flash, slot swap + fallback handled by Prime).
- **Later:** **over the network** - then the dev loop is "build, push, reboot" with no
  physical media at all.

## 9. Open questions

1. **Compression fixed-point vs one-version-behind** for the self-carried image (§6) -
   v1 leans one-version-behind; revisit if "always install current Prime" matters.
2. **Boot-default mechanism** (§8.2): minimal FAT writer (v1) vs UEFI boot variables.
3. **Run-from-drive vs merge-into-primary:** does a portable drive *boot directly*
   (Prime + its GSFS world) and/or get *copied/merged* into a host's primary via a
   `drives copy`? (Probably both, eventually.)
4. **Prime's exact utility boundary:** is `drives` + shell + storage + console
   enough, or does Prime also need a minimal `cp`/`copy` to move things between
   drives? Where exactly is the line between "Prime" and "content"?
5. **Versioning / merge semantics:** when a drive's world is merged into a host, or a
   newer Prime meets an older drive - what are the rules? (Verification §16; details TBD.)
6. **Boot precedence:** after `drives install 0`, how is boot order chosen (firmware
   boot menu, or GodspeedOS sets it)? Removing the USB is the simple v1 answer.

## 10. Suggested order (when built)

1. **Hierarchical GSFS + `drives flash`/`use`** (the storage foundation -
   `persistence.md`, `drives.md`).
2. **`drives install`** - write a bootable GodspeedOS drive (GPT + ESP/Limine +
   kernel) from the self-carried image (§6). Self-install USB → SSD; boot without the USB.
3. **A/B `drives update`** (§8) - two kernel slots + the minimal FAT writer + boot-default
   flip + the Limine-menu fallback. *This is the dev-loop payoff: kernel swaps without
   re-flashing.* (Boot-count auto-rollback is a later refinement.)
4. **Self-replication** - `install` to *other* drives; carry + boot them elsewhere.
5. **Carrying a world** - supervisor loads services from a drive's GSFS region (§16
   generalized), so a drive carries your *content* on top of portable *Prime*.

## 11. Rug pull: what happens to running apps when Prime is replaced

> The lifecycle contract for the *whole OS*. Short version: **a Prime swap is a
> reboot, and only state persisted to GSFS (§15) survives - the same restart
> contract every service already lives under, applied one level up.**

### 11.1 The kernel is not a service - you can't restart the floor

Service restart (§14) works because **the kernel survives to mediate it**: it marks
the dead service's generation stale, the client's caps fail, the client reacquires a
fresh cap and resumes. But **Prime *is* the kernel** - the floor every app stands on.
Page tables, cap tables, IPC queues, the scheduler, the CPU state all live *inside* it.
You cannot pull the floor from under apps standing on it. So:

**Replacing Prime is necessarily a reboot** - a clean, intentional one (not a crash,
not "start afresh" blindly), but a reboot. It is **not** the service-restart pattern:
there is no surviving kernel to report endpoints "dead" or keep cap-staleness coherent.
In a kernel swap, *everything in RAM resets at once* - caps, endpoints, running
execution, all gone together.

### 11.2 But the apps step off, the floor is replaced, they step back on

The foundation is **"apps assume failure"** (§14.3, §26.7): every app already recovers
from *any* failure, *anytime*, from its last persisted checkpoint - no warning relied
upon. So a Prime swap is, to an app, **just another failure it already knows how to
survive.** Recovery needs no special handshake - it is the ordinary restart path:

```text
  1. reboot into the new kernel                 (A/B slot, rollback if bad, §8)
  2. new kernel boots → supervisor RESPAWNS the services (per the manifest/contracts,
        §14.1; or from the GSFS world, §5) → kernel RE-MINTS each cap per the contract
  3. each service RECONSTRUCTS its state from GSFS (§15) and RE-ACQUIRES any runtime
        caps via the registry (§14.2) → carries on
```

The apps do **not** survive in memory across the swap. What crosses the seam is
**persisted state, not running processes.** The new kernel does not *resume* the old
execution - it *respawns* the apps and they *reconstruct*. That is
**restart-with-reconstruction (§2.5) at the whole-system level** - the constitution's
stance (*live code update rejected; restart is sufficient*) with the kernel as its
ultimate case, not an exception.

> **No kernel grant-snapshot is needed (or wanted).** The grant *topology* -
> who-holds-what authority - is already captured **declaratively in the contracts**
> (§13); the supervisor respawns and the kernel re-mints per contract on boot. Caps
> can't be "restored" anyway - they are generationed (`ResourceId + Rights +
> Generation`) and bound to specific resource instances that are recreated fresh on
> reboot, so old caps are stale by definition; authority is **re-minted**, not
> restored. Runtime-delegated caps (not in any contract) are re-established by the
> delegating app's recovery logic - or by the supervisor persisting its own running-set
> (its §15 state). A kernel that snapshotted live grants would duplicate the contracts
> opaquely (§26.4/§26.5) - exactly the hidden magic the constitution forbids.

> **The "prepare to checkpoint" signal is optional gravy, not a required step.** Because
> apps assume failure, correctness never depends on a warning. An optional "system is
> updating" notification can let a *stateful* app (e.g. a database) commit cleanly and
> lose a little less work on a *planned* update - but an app that ignores it still
> recovers fine from its last checkpoint. So it is built only when a stateful app pulls
> it into existence (§26.2); it never changes the failure model, it just makes a planned
> failure tidier than a crash.

### 11.3 Who survives - one consistent rule

**Only apps that followed §15** (persist your own state externally; reconstruct on
startup). A well-behaved app comes back where it left off; a RAM-only app that never
persisted starts fresh - *exactly* as a non-persisting service loses its state on a
normal restart. The kernel rug-pull adds **no new failure mode**; it is the **same
restart contract, one level up.**

### 11.4 Why it's survivable at all

The property worth naming: **the architecture that makes services restartable is what
makes kernel updates survivable.** On a monolith, a kernel update is a reboot where apps
just *die* and someone restarts them by hand. In GodspeedOS the apps were *born* knowing
how to checkpoint and resume - so a Prime swap is *less* disruptive than on Linux, not
more, **because** of the isolation + persist-and-reconstruct discipline. Nothing was ever
standing on the rug that didn't know how to land.

### 11.5 Two edges

- **Unplanned rug pull (kernel panic):** identical to a planned one - the apps recover
  the same way, from their *last* persisted checkpoint. The only difference is there was
  no chance for an optional graceful wind-down (§11.6), so an app loses whatever it had
  not yet checkpointed. The crash page (§19) preserves the panic reason across the
  reboot. Hence the app-author's discipline: persist often.
- **A/B is what makes the rug pull safe to attempt** (§8): a broken new Prime is a
  reboot you can *undo* (rollback to the old slot), not a brick. Without A/B, a bad
  kernel update bricks; with it, it's reversible.

### 11.6 Lifecycle notifications - graceful shutdown (decided; build when pulled in)

"Assume failure" (§11.2) is the floor, but a *stateful* app - a database, anything with
in-flight transactions, buffered writes, or open connections - genuinely benefits from a
chance to wind down **cleanly** before a *planned* restart/Prime swap: commit the current
transaction, flush, close - so there is **no journal replay and no partial-write window**
at all. That gap is real, and wider today because GSFS Phase 1 has no journal (§6.3), so a
clean flush is the difference between "consistent on disk" and "trusting recovery."

So GodspeedOS **will** support lifecycle notifications - but as a **subscribe-and-forget
courtesy broadcast, not POSIX signals.** POSIX signals are async interrupts (they hijack
the program, run a handler → re-entrancy hazards, ambient authority); worse, they invite a
"send signal, *wait* for everyone to wind down" handshake. This is the opposite of both:

- **Pure information - it doesn't *do* anything.** The signal is "FYI, I'm going down," not
  a command and not a request for permission. It triggers no OS behavior and cannot veto,
  stall, or delay the reboot.
- **The OS does not wait.** It *publishes* the event and **proceeds immediately on its own
  schedule** - no deadline, no acknowledgment, no negotiation. It is going to do what it is
  going to do. (So there is *nothing to coordinate* and nothing that can stall an update -
  the whole deadline/ack/watchdog apparatus simply doesn't exist.)
- **Subscribe / opt-in, capability-gated.** An app receives lifecycle events only if it
  subscribed (holds the cap, declared in its contract). Apps that don't care never see them
  and pay nothing. No ambient "anyone can signal you."
- **A message on the app's endpoint**, handled in its *normal recv loop* - synchronous,
  when the app is ready; no async handler, no re-entrancy.
- **Zero guarantees - strictly best-effort.** A responsive app *might* catch it and flush;
  a mid-operation one might not. It does not even promise a *window*. So "assume failure"
  stays the floor entirely; this is courtesy laid on top, never relied on for correctness.

So it is almost the inverse of POSIX signals: *"an event you handle if you happen to catch
it,"* not *"an interrupt that hijacks you and a handshake that waits for you."* And it is
barely any new machinery - a **convention** (a lifecycle message kind) plus a **cap to
subscribe**, over existing IPC. No new subsystem, no coordination, no waiting.

**Timing (§26.2):** the *decision* is recorded here; the *code* is built when a real
stateful app pulls it into existence (there is no database on GodspeedOS today). Recording
the shape now means that when one arrives, the design is waiting - and nobody reaches for
POSIX signals by reflex.
