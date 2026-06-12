# `drives` — drive management from the shell

> **Status:** Design doc, non-normative, **not yet built**. Records the `drives`
> shell utility and the multi-drive model, decided in conversation. Builds on the
> GSFS filesystem (`docs/persistence.md`) and the AHCI block driver (`docs/ahci.md`).
> Trails `CLAUDE.md`; does not amend it.

## 1. What it is

`drives` is the shell utility for managing attached disks: list them, format
(flash) a raw one as GSFS, choose which one is active, and pick a default that
auto-mounts on boot. It's the user-facing front end to `fs` + `block-driver`.

Because GodSpeed boots from a removable image (USB) and disks are pluggable, the
system needs an explicit, *user-initiated* way to say "use this disk" — which is
also what keeps it honest: the constitution forbids *silent* reformatting (§3.12),
and a human running `drives flash` is the opposite of silent.

## 2. Flashing does not need a reboot

A common misconception worth stating: **after `drives flash`, the drive is mounted
immediately and usable right away** — read/write at once, no reboot. A reboot
*proves* persistence (the bytes survive a power-cycle) but is never part of the
workflow. Flash → use. The only reason to reboot is to demonstrate durability, or
because you normally power-cycle the machine.

## 3. The default drive (persistent)

With pluggable drives, the system needs to know *which* disk to mount on boot.

- **`drives use default <n>`** marks a drive as the default. The flag is stored in
  **that drive's own GSFS superblock** (a `DEFAULT` bit in the `flags` field), so
  the drive is **self-describing**: on boot, `fs` scans the drives, finds the one
  flagged default, and auto-mounts it. Move the drive to another machine/port and
  it is still recognized. Setting a new default clears the flag on the others.
- Survives reboot because it lives on the disk, not in the (removable) OS image.

## 4. Multiple drives

GodSpeed sees every attached disk (block-driver enumerates all SATA ports; today
it only uses the first — multi-drive lifts that). Each drive is an independent flat
GSFS namespace. A drive moves through states:

```
  raw  ──flash──▶  flashed  ──mount──▶  mounted  ──use──▶  current
   (no GSFS)        (GSFS, not          (metadata        (bare commands
                     loaded)             loaded;           target it)
                                         readable)
```

- **`mounted`** — `fs` has loaded the drive's superblock + entry table; you can
  list/read it. Bounded: `fs` holds up to a fixed number of mounted drives (e.g. 4).
- **`current`** — the one *unqualified* commands operate on. Exactly one at a time.
- **`mount` ≠ `use`.** `drives mount 1` makes a drive accessible without making it
  current; `drives use 1` mounts it (if needed) **and** makes it current. This
  separates "I can see into it" from "it's my working drive."

### 4.1 Addressing files across drives — by label (identity over location)

GSFS is **flat** (name → blob, no directories), so there is nothing to `cd` into;
"switching drives" is just changing the *current* drive. To touch a file on a
*different* drive without switching, qualify the name with the drive.

The GodSpeed-native way to name a drive is by **label, not index** — this is
**invariant 11 (identity is stable; location is not)** applied to storage:

- A drive's **index/port is its *location*** — it changes when you replug it.
- A drive's **label is its *identity*** — stored in the GSFS superblock, stable
  forever, replug-safe.

So you flash a drive *with a name* and address it by that name regardless of port:

```
  ls               # current drive
  ls backup:       # the drive labelled "backup", without switching to it
  cat backup:notes.txt
  drives use backup
```

Bare name → current drive; `label:name` → that drive explicitly. (Plain indices
like `1:name` also work as a fallback before a drive is labelled.)

### 4.2 Labels are unique — no duplicates (prevents cleverness)

**There is never more than one drive with a given label.** GodSpeed refuses to
*create* a clash rather than build machinery to *resolve* one (§26.13 discipline
over cleverness, §26.2 simplicity). Uniqueness is enforced among the drives
GodSpeed can currently see:

- **`drives flash <n> <label>`** and **`drives label <n> <label>`** are refused if
  that label is already used by an attached drive — pick another unique name:
  ```
  gs> drives flash 2 data
    drives: label 'data' is already used by drive 0. choose another name.
  ```
- **On boot / plug-in**, if a drive's stored label collides with one that's already
  mounted, the newcomer is **not mounted** — reported loudly (§3.12), and shown as
  unusable until relabelled. The index (location) is still unique, so you can always
  reach it to fix it:
  ```
  gs> drives
    #  LABEL      STATUS          …
    0  data       mounted         …
    1  data       label-clash     …   ← not mounted; relabel to use it
  gs> drives label 1 backup
    drives: drive 1 relabelled → 'backup'; now mountable
  ```

Because labels are unique, **`label:name` is always unambiguous** and there is no
index-fallback resolution, no duplicate-marker, and no need for a separate UUID.
The label *is* the drive's identity, and identities don't collide.

## 5. Command set

| Command | Effect | Persists? |
|---------|--------|-----------|
| `drives` | list every drive: index, label, status, contents, current/default | — |
| `drives flash <n> [label]` | format drive n as GSFS (asks `[y/N]` — it ERASES); optionally name it (must be unique); mounts immediately | data: yes |
| `drives label <n\|label> <new>` | rename a drive's label (must be unique); rewrites the superblock | data: yes |
| `drives mount <n\|label>` | make a flashed drive accessible (list/read) — **not** current | session |
| `drives use <n\|label>` | mount (if needed) **and** make it the current drive | session |
| `drives use default <n\|label>` | also persist: this drive auto-mounts + is current on every boot | **yes** (superblock flag) |
| `ls` · `cat <name>` · `write <name> …` | operate on the **current** drive | — |
| `ls <label>:` · `cat <label>:<name>` | operate on another mounted drive explicitly | — |

## 6. How it looks (`gs>` mockups)

Flash and use a raw drive — immediately, no reboot:

```
gs> drives
  #  LABEL      STATUS     SIZE      CONTENTS                   USE
  0  —          raw        16 MiB    — not formatted —

gs> drives flash 0 data
  This ERASES drive 0 (QEMU HARDDISK, 16 MiB). Continue? [y/N] y
  drives: formatting drive 0 as GSFS, label 'data' … done
  drives: drive 0 mounted — ready to use now (no reboot needed)

gs> write notes.txt "works immediately"
  fs: wrote notes.txt (18 bytes)

gs> drives use default 0
  drives: 'data' is now the default — auto-mounts on every boot
```

Multiple drives, addressing by label, mount vs use:

```
gs> drives
  #  LABEL      STATUS     SIZE      CONTENTS                   USE
  0  data       mounted    16 MiB    GSFS · 3 files · 32k free    * default · current
  1  backup     flashed    32 MiB    GSFS · 1 file  · 65k free    (not mounted)
  2  —          raw        8 MiB     — not formatted —

gs> drives mount backup
  drives: 'backup' mounted (read/list only; current is still 'data')

gs> ls backup:
  NAME         SIZE
  archive.bin  40 KiB

gs> cat backup:notes
  (file not found on 'backup')

gs> drives use backup
  drives: current drive → 'backup' (default 'data' restores on reboot)
```

Replug-safety (identity over location):

```
gs> drives use archive          # by label
   …unplug drive, move to another SATA port, replug…
gs> drives                       # the index changed; the label did not
  #  LABEL      STATUS     SIZE   …
  0  archive    flashed    8 MiB  …     ← was #2, now #0; still "archive"
```

## 7. What it touches (build scope)

A real multi-part feature, layered:

- **block-driver:** enumerate *all* SATA disks (not just the first); the block IPC
  gains a **drive index** (`ReadBlock(drive, lba)` / `WriteBlock(drive, lba, …)`);
  a **capacity** request (sector count from IDENTIFY) so a flash sizes the
  filesystem to the real disk.
- **GSFS superblock:** add a `flags` field (`DEFAULT` bit) and a `label` field
  (the drive's stable identity).
- **`fs`:** become raw-tolerant (serve the drives API even with no filesystem);
  hold several **mounted** drives at once (bounded slots) with a **current** pointer;
  auto-mount the default on boot; drives API = `list` / `flash` / `mount` / `use` /
  `use default`; resolve `label:name` addressing.
- **shell:** the `drives` command (+ subcommands) and the file commands
  (`ls` / `cat` / `write`) that operate on the current/labelled drive.

## 8. Suggested order

1. **Single-drive `drives`** — `flash` / `use` / `use default` + boot auto-mount of
   the default. (One disk; proves the format/mount/default loop end to end.)
2. **Labels** — name a drive at flash; address + select by label (identity layer).
3. **Multi-drive** — enumerate all disks; per-drive block IPC; `mount` vs `use`;
   `label:name` cross-drive addressing; bounded mounted-drive slots.
4. **File commands** — `ls` / `cat` / `write` against the current/labelled drive.

## 9. Open questions

- Bound on simultaneously-mounted drives (4?) and on the label length (16 chars?).
- Label clashes: **resolved — labels are unique, no duplicates** (§4.2). flash/label
  refuse a taken name; a plugged-in drive whose label clashes is not mounted until
  relabelled. No index-fallback, no UUID.
- Confirmation UX for `flash` (a `[y/N]` prompt vs a `--force`/`yes` token), given
  the shell is line-based.
- Whether `current` resets to the default on every boot (proposed: yes) or is also
  remembered (proposed: no — only the *default* persists; `use` is session-scoped).
- Hot-plug: re-enumerating drives when a disk is inserted/removed at runtime (later;
  the USB stack already does hot-plug, so there is a pattern to follow).
