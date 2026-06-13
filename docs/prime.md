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

## 3. Two verbs: `flash` vs `install`

The distinction the whole model rests on:

| Command | Makes | Result |
|---------|-------|--------|
| `drives flash <n> [label]` | a **data** drive (GSFS only) | files; not bootable |
| `drives install <n>` | a **bootable GodspeedOS** drive (boot region + GSFS) | the machine can boot GodspeedOS from it |

`install` is **self-replication**: GodspeedOS writing GodspeedOS onto a drive.

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

## 6. The one new capability it needs

Honesty about cost: `install` means **writing a bootable layout from inside
GodspeedOS** — a GPT, a FAT ESP, Limine, and the kernel image. That's precisely what
`osdev image` does **on the host**; Prime self-install is doing it **self-hosted**.
Bounded but real:

- a small **GPT writer** (partition table) + block writes (already have `block-driver`);
- the boot region: either a **FAT32 writer**, or — simpler — Prime carries a
  prebuilt **ESP blob** and stamps it onto the target, patching only the kernel;
- the **kernel image to copy**: Prime either **carries a compressed copy of its own
  boot image** (self-contained, larger) or **reads it back from the medium it booted
  from** (leaner, but only while that medium is attached). The same recursion every
  live-USB installer uses.

None of this touches the kernel's `unsafe` story — it's block writes + format
construction in userspace (`drives` + `fs` + `block-driver`).

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

## 8. Open questions

1. **Self-replication source:** Prime carries its own boot image (self-contained,
   bigger) vs reads it back from the boot medium (leaner, medium must be attached)?
2. **Boot region writer:** a real FAT32 writer vs a carried ESP blob that's stamped +
   kernel-patched? (Blob is simpler for v1; FAT writer is more flexible.)
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

## 9. Suggested order (when built)

1. **Hierarchical GSFS + `drives flash`/`use`** (the storage foundation —
   `persistence.md`, `drives.md`).
2. **`drives install`** — write a bootable GodspeedOS drive (GPT + ESP/Limine +
   kernel). Self-install from USB to SSD; boot without the USB.
3. **Self-replication** — `install` to *other* drives; carry + boot them elsewhere.
4. **Carrying a world** — supervisor loads services from a drive's GSFS region (§16
   generalized), so a drive carries your *content* on top of portable *Prime*.
