# GodspeedOS Prime — the minimal, self-installing, portable core

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
utilities needed to run and to make itself portable** — nothing more:

| Layer | Members | Why it's in Prime |
|-------|---------|-------------------|
| Kernel + arch + smp | the microkernel | the mechanism (§6.1) |
| `init` · `supervisor` · `registry` | trusted root | bootstrap + lifecycle + naming (§6.1) |
| `block-driver` · `fs` · AHCI | storage stack | read/write drives — needed to install & carry state |
| console + keyboard driver | interaction | a usable prompt (§B.3) |
| `shell` | the prompt | where you type commands |
| `drives` | drive utility | flash data drives + **install/replicate Prime** (`docs/drives.md`) |

Everything *beyond* Prime — your apps, networking, your data — is **content**, not
Prime. Prime stays whiteboardable (§26.11): "boot, interact, reproduce."

> **Prime ⊇ TCB, but it is not the same set.** The TCB (§6.1) is *what must be
> trusted*. Prime is *what ships in the minimal bootable core*. `block-driver`/`fs`
> are in Prime because you can't install or carry state without them; their TCB
> status is the separate Phase-3 question (§6.3).

## 2. Anatomy of a bootable GodspeedOS drive

A drive GodspeedOS can **boot from** has two regions:

```text
  ┌─────────────────────────────┬───────────────────────────────────────────┐
  │  Boot region — ESP (FAT)    │  GSFS region                              │
  │  Limine + Prime kernel image│  data + (later) your services/state/config│
  │  firmware boots THIS         │  (docs/persistence.md, hierarchical GSFS) │
  └─────────────────────────────┴───────────────────────────────────────────┘
```

- **Boot region (ESP):** a small FAT partition with **Limine + the Prime kernel
  ELF + `limine.conf`** — exactly what `osdev image` writes today on the host. The
  firmware boots this. It must be FAT because Limine can't read GSFS (it's ours).
- **GSFS region:** the hierarchical GSFS filesystem (`persistence.md §6.2`) — the
  drive's data, and eventually the services/state that make it *your world*.

A **data-only drive** has just a GSFS region (no boot region). A **bootable
GodspeedOS drive** has both.

## 3. Three verbs: `flash`, `install`, `update`

The distinction the whole model rests on:

| Command | Makes | Result |
|---------|-------|--------|
| `drives flash <n> [label]` | a **data** drive (GSFS only) | files; not bootable |
| `drives install <n>` | a **bootable GodspeedOS** drive (boot region + GSFS) | the machine can boot GodspeedOS from it |
| `drives update <n>` | a new kernel in the **inactive A/B slot** of an existing Prime | reboot boots the new kernel; the old stays as fallback (§10) |

`install` is **self-replication** (GodspeedOS writing GodspeedOS onto a drive);
`update` is **self-update** (GodspeedOS replacing its own kernel, safely, §10).

## 4. The flow — boot, self-install, propagate

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
and identical everywhere; your instance travels** — identity over location, for a
whole OS (invariant 11, scaled up).

## 5. Carrying your world (run programs off a drive)

Prime boots the *mechanism*. The richer vision — "plug my drive into any GodspeedOS
and **continue from there**" — layers on top: after Prime boots, the supervisor
**loads additional services from the drive's GSFS region** and spawns them, and
services reconstruct their state from GSFS (§15). Today services are baked into the
kernel image; loading-and-running from `fs` is the one capability that unlocks the
portable *world* on top of portable *Prime*.

This is the **update model (§16) generalized**: §16 is "restart a service with a new
binary"; this is "load a service's binary from a drive." Same principle — the binary
is data, authority comes from capabilities, the kernel just runs what it's handed —
pointed at a *pluggable* drive instead of a fixed manifest.

So the layering is clean:
- **Prime** = portable *mechanism* (kernel + TCB + utilities), via `drives install`.
- **World** = portable *content* (your services + state) in a drive's GSFS region,
  loaded on top of whatever Prime booted.

## 6. Self-replication: Prime is *self-carrying* (resolved)

`install` means **writing a bootable layout from inside GodspeedOS** — a GPT, a FAT
ESP (Limine + kernel), and a GSFS region — exactly what `osdev image` does **on the
host**, now self-hosted. The crux was: where do the boot-image bytes come from? A
constraint settles it:

> **GodspeedOS cannot read the medium it booted from.** `block-driver` is an
> **AHCI/SATA** driver; the USB you boot Prime from lives on the **xhci/ehci**
> controller — a different device entirely. So "read the boot ESP back from the boot
> medium" is impossible for the first USB→SSD install (it would need a USB
> mass-storage driver). The bytes must come from inside Prime.

**So Prime *carries* a copy of its own bootable image** and stamps it onto any target
(raw block writes — the ESP is an opaque blob, so **no FAT *read/write* needed for
install**). `install` = write GPT → stamp the carried boot image into the ESP region →
make a fresh GSFS data partition. Source-medium-independent.

The mild recursion (an image of yourself contains a kernel that contains the image) is
closed two ways; **v1 picks the simpler (§26.2/§26.13):**
- **One-version-behind (chosen):** Prime carries the *previous stable* Prime image.
  Dead simple, no build-time fixed-point; the freshly-installed copy is one rev old
  until it re-installs. Fine because Prime changes rarely.
- *(Alt)* Compression fixed-point: a compressed self-image converges to a small fixed
  point so `install` always writes *current* Prime — needs a build-time iteration.

None of this touches the kernel's `unsafe` story — block writes + format construction
in userspace (`drives` + `fs` + `block-driver`).

> **Caveat (see §10):** *install* needs no FAT logic (raw blob stamp), but in-place
> A/B *update* — swapping one kernel slot and flipping the boot default — does need a
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
slot, reboot into it — and if it's bad, the old one is still there. This is the
**A/B-slot-with-rollback** scheme (Android / ChromeOS / CoreOS), and it is the
constitution's **§16 update model applied to the whole kernel** instead of one service
("write a new binary, verify before trust").

### 8.1 The model

- A Prime drive has **two kernel slots, A and B.** One is *active* (running), one
  *inactive*. The active slot is never touched by an update — a bad build cannot brick.
- **`drives update <n>`** writes the new kernel to the **inactive** slot, then flips
  "which slot boots next."
- **Reboot auto-selects the new slot.** If it fails, you fall back to the old.
- Verbs: `install` makes a *new* bootable drive (full GPT + ESP + GSFS); `update` swaps
  the inactive A/B kernel slot *in place* on an existing Prime.

### 8.2 The honest cost: flipping the slot needs to touch the boot region

`install` is a raw whole-ESP stamp (no FAT logic, §6). But `update` must **write one
kernel file and change the boot default**, which means modifying the boot region. Two
ways, and this is the key sub-decision:

1. **Two kernel files in one ESP + a *minimal* FAT writer (chosen for v1).**
   `kernel_a.elf` / `kernel_b.elf`; `update` overwrites the inactive file and edits
   `limine.conf`'s default. The FAT writer is *bounded* — "overwrite a known file,
   edit a tiny config" — not a general filesystem. Tractable, and it admits the honest
   truth that a self-updating OS needs a little FAT-write (§6 caveat).
2. *(Alt)* **Two whole-ESP partitions, each raw-stamped + UEFI `BootNext`/`BootOrder`.**
   No FAT logic, but needs UEFI runtime-variable access from a post-boot microkernel —
   fiddly, and bootloader-agnostic (does not depend on Limine).

### 8.3 Rollback

- **v1 — the Limine menu** (short timeout): if the new slot hangs, pick the old one.
- **v2 — boot-count auto-rollback:** mark the new slot "trial"; if userspace doesn't
  confirm "boot OK" within N boots, the bootloader reverts to the known-good slot. This
  is the real safety net and the right long-term shape.

### 8.4 Where the new kernel comes from

- **Now:** a kernel image on a **data drive / GSFS file** that `update` reads — already
  a win (a file copy, no USB re-flash, slot swap + fallback handled by Prime).
- **Later:** **over the network** — then the dev loop is "build, push, reboot" with no
  physical media at all.

## 9. Open questions

1. **Compression fixed-point vs one-version-behind** for the self-carried image (§6) —
   v1 leans one-version-behind; revisit if "always install current Prime" matters.
2. **Boot-default mechanism** (§8.2): minimal FAT writer (v1) vs UEFI boot variables.
3. **Run-from-drive vs merge-into-primary:** does a portable drive *boot directly*
   (Prime + its GSFS world) and/or get *copied/merged* into a host's primary via a
   `drives copy`? (Probably both, eventually.)
4. **Prime's exact utility boundary:** is `drives` + shell + storage + console
   enough, or does Prime also need a minimal `cp`/`copy` to move things between
   drives? Where exactly is the line between "Prime" and "content"?
5. **Versioning / merge semantics:** when a drive's world is merged into a host, or a
   newer Prime meets an older drive — what are the rules? (Verification §16; details TBD.)
6. **Boot precedence:** after `drives install 0`, how is boot order chosen (firmware
   boot menu, or GodspeedOS sets it)? Removing the USB is the simple v1 answer.

## 10. Suggested order (when built)

1. **Hierarchical GSFS + `drives flash`/`use`** (the storage foundation —
   `persistence.md`, `drives.md`).
2. **`drives install`** — write a bootable GodspeedOS drive (GPT + ESP/Limine +
   kernel) from the self-carried image (§6). Self-install USB → SSD; boot without the USB.
3. **A/B `drives update`** (§8) — two kernel slots + the minimal FAT writer + boot-default
   flip + the Limine-menu fallback. *This is the dev-loop payoff: kernel swaps without
   re-flashing.* (Boot-count auto-rollback is a later refinement.)
4. **Self-replication** — `install` to *other* drives; carry + boot them elsewhere.
5. **Carrying a world** — supervisor loads services from a drive's GSFS region (§16
   generalized), so a drive carries your *content* on top of portable *Prime*.
