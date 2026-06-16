# `drives` — drive management from the shell

> **Status:** Design doc, non-normative, **not yet built**. Records the `drives`
> shell utility and the multi-drive model, decided in conversation. Builds on the
> GSFS filesystem (`docs/persistence.md`) and the AHCI block driver (`docs/ahci.md`).
> Trails `CLAUDE.md`; does not amend it.
>
> **Command surface superseded by `utilities/15_drives.md`** (+ the file commands
> `utilities/16`–`24`), which are authoritative for the user-facing verbs. Three POSIX
> reflexes shown in this doc's older examples were **dropped** when the utility surface
> was settled: **`mount`** (GodspeedOS has no global tree to graft into — a drive is
> reachable on demand by `[index:]label/path` whenever it's plugged in; `15_drives.md`
> §3.1), and **`drives use` / `drives use default`** (the current location is a single
> drive+dir pointer moved by `cd`, not a second "current drive" verb; `15_drives.md`
> §3.2). The boot layer is the `drives godspeed install|update|default` namespace. This
> doc is retained for the **architecture/rationale** — addressing (§4.1/§4.2), the
> default-flag (§3), the multi-drive model (§4), build scope (§7) — not the verb list.

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
it only uses the first — multi-drive lifts that). Each drive is an independent GSFS
tree (its own root directory). A drive moves through states:

```
  raw  ──flash──▶  flashed  ◀──── present whenever plugged in ────▶  (absent when unplugged)
   (no GSFS)        (GSFS;        addressable on demand by [index:]label/path
                     usable)
```

> **Superseded — no `mounted` state** (`utilities/15_drives.md` §3.1). The earlier model
> here had `raw → flashed → mounted → current`; the `mounted` step was a POSIX reflex.
> GodspeedOS has no global tree to graft into, so there is nothing to "mount" — a flashed
> drive is reachable on demand by `[index:]label/path` whenever it is physically present,
> and disappears when unplugged. There is no mount table and no bounded mount-slots.

- **`current`** — the drive+directory that *unqualified* commands operate on, moved by
  `cd` (`utilities/17_cd.md`). One **location** pointer (drive + dir), not a separate
  "current drive" — see `utilities/15_drives.md` §3.2 for why `drives use` was dropped.

### 4.1 Addressing — `[N:]label/path/to/file`

GSFS has **real directories** (§ persistence.md), so a file is named by a path
*within* a drive, and a drive is named by its label (optionally prefixed by index):

```
  <address> ::= [ N: ] label / dir / … / file        # on another drive
              |             /path/to/file             # on the current drive (leading /)
              |              path/to/file             # relative to the current dir
```

The **drive** part is the GodSpeed-native bit — **identity over location**
(invariant 11): the **label is the drive's identity** (stored in the GSFS
superblock, stable across replug); the **index is its location** (the port — changes
when you replug). You normally use the **label alone**; you prefix the **index `N:`**
only to disambiguate (see §4.2). Examples:

```
  ls archive/projects/2026          # 'archive' is unique → no index needed
  cat 0:data/notes.txt              # 'data' on drive 0
  cat 1:data/notes.txt              # 'data' on drive 1 (a different drive, same label)
  cat /etc/boot.cfg                 # leading / → current drive, absolute path
  cat notes.txt                     # relative to the current directory
```

Switching the *current* drive (`drives use`) changes what bare/relative paths mean;
the `[N:]label/…` form reaches any mounted drive without switching.

### 4.2 Duplicate labels are fine — index disambiguates

Labels need **not** be unique. Two drives can both be `data` — flashed separately,
or one arriving pre-labelled from another GodSpeed instance. They are distinguished
by the **index prefix**, which is unique by physics (one drive per port):

- Unique label → address by label alone: `archive/…`.
- Clashing label → prefix the index: `0:data/…` vs `1:data/…`. Both forms still
  show a readable name; the number only disambiguates.

This makes cross-instance drives **just work**: plug in a foreign `data` disk and it
mounts as `1:data` — no refusing, no forced relabel, no silent renaming (§26.5), no
UUID. Identity (label) names it; location (index) disambiguates when identity repeats.

- `drives` flags a duplicated label so you can see it (and relabel with
  `drives label N <new>` if you want a unique name), but it is never *required*:
  ```
  gsh> drives
    #  LABEL      STATUS     …
    0  data       mounted    …
    1  data  (2)  mounted    …   ← duplicate label; address as 0:data / 1:data
  ```
- `drives use data` with two `data` drives asks you to qualify:
  `drives use 1` (or `drives use 1:data`).

## 5. Command set

> Authoritative command surface: `utilities/15_drives.md` (drive lifecycle) +
> `utilities/16`–`24` (file commands). This table reflects the settled vocabulary.

| Command | Effect | Persists? |
|---------|--------|-----------|
| `drives` | list every drive: index, label, status, size, current/default | — |
| `drives flash <drive> [label]` | format `<drive>` as a GSFS data drive (asks `[y/N]` — it ERASES); optional label; usable at once | data: yes |
| `drives label <drive> <new>` | rename a drive's label; rewrites the superblock (duplicates allowed — index disambiguates) | data: yes |
| `drives godspeed install <drive>` | install bootable GodspeedOS onto the drive (Prime) | **yes** |
| `drives godspeed update <drive>` | A/B kernel update of an installed drive | **yes** |
| `drives godspeed default <drive>` | which installed GodspeedOS the machine boots | **yes** |
| `drives godspeed` | list installed GodspeedOS drives / help | — |
| `cd <path>` · `mkdir <path>` | change current location / create a directory (`utilities/17`,`20`) | — |
| `ls [path]` · `read <path>` · `write <path> …` · `copy`/`move`/`rename`/`delete` | navigate / read / write / manage files (`utilities/16`–`24`) | — |

`<drive> = index | label | index:label` (§4.1/§4.2). No `mount`/`use` — the current
location is a `cd` pointer (§4 note, `utilities/15_drives.md` §3.2).

## 6. How it looks (`gsh>` mockups)

Flash and use a raw drive — immediately, no reboot:

```
gsh> drives
  #  LABEL      STATUS     SIZE      CONTENTS                   USE
  0  —          raw        16 MiB    — not formatted —

gsh> drives flash 0 data
  This ERASES drive 0 (QEMU HARDDISK, 16 MiB). Continue? [y/N] y
  drives: formatting drive 0 as GSFS, label 'data' … done
  drives: drive 0 mounted — ready to use now (no reboot needed)

gsh> mkdir projects
  fs: created /projects
gsh> write projects/notes.txt "works immediately"
  fs: wrote /projects/notes.txt (18 bytes)

gsh> drives use default 0
  drives: 'data' is now the default — auto-mounts on every boot
```

Multiple drives, paths, duplicate labels, mount vs use:

```
gsh> drives
  #  LABEL      STATUS     SIZE      CONTENTS                   USE
  0  data       mounted    16 MiB    GSFS · 5 files · 32k free    * default · current
  1  data  (2)  flashed    32 MiB    GSFS · 1 file  · 65k free    (not mounted)
  2  archive    raw        8 MiB     — not formatted —

gsh> drives mount 1:data
  drives: 1:data mounted (read/list only; current is still 0:data)

gsh> ls 1:data/backups
  NAME           SIZE
  2026-06.tar    40 KiB

gsh> cat 0:data/projects/notes.txt
  works immediately

gsh> drives use 1:data
  drives: current drive → 1:data (default 0:data restores on reboot)
```

Replug-safety (identity over location):

```
gsh> drives use archive          # by label
   …unplug drive, move to another SATA port, replug…
gsh> drives                       # the index changed; the label did not
  #  LABEL      STATUS     SIZE   …
  0  archive    flashed    8 MiB  …     ← was #2, now #0; still "archive"
```

## 7. What it touches (build scope)

A real multi-part feature, layered:

- **block-driver:** enumerate *all* SATA disks (not just the first); the block IPC
  gains a **drive index** (`ReadBlock(drive, lba)` / `WriteBlock(drive, lba, …)`);
  a **capacity** request (sector count from IDENTIFY) so a flash sizes the
  filesystem to the real disk.
- **GSFS becomes hierarchical** (directories) — see `persistence.md`. The on-disk
  format gains inodes (file/dir) + directory blocks (name → inode) + path walking;
  the superblock gains a `flags` field (`DEFAULT` bit) and a `label` field. (Phase 1
  shipped a *flat* GSFS; directories are the adopted evolution.)
- **`fs`:** become raw-tolerant (serve the drives API even with no filesystem);
  hold several **mounted** drives at once (bounded slots) with a **current** drive +
  **current directory**; auto-mount the default on boot; resolve `[N:]label/path`
  addressing; drives API = `list` / `flash` / `label` / `mount` / `use` / `use default`.
- **block-driver:** enumerate *all* SATA disks; the block IPC gains a **drive index**;
  a **capacity** request so a flash sizes the filesystem to the disk.
- **shell:** `drives` (+ subcommands) and the file commands (`ls` / `cat` / `write` /
  `cd` / `mkdir`) with `[N:]label/path` addressing.

## 8. Suggested order

1. **Hierarchical GSFS** — evolve the format to inodes + directories + path walking
   (the foundation the rest needs). `persistence.md`.
2. **Single-drive `drives`** — `flash` / `use` / `use default` + boot auto-mount of
   the default (one disk; proves the format/mount/default loop).
3. **File commands** — `ls` / `cat` / `write` / `cd` / `mkdir` on the current drive.
4. **Labels** — name a drive at flash/`label`; address + select by label.
5. **Multi-drive** — enumerate all disks; per-drive block IPC; `mount` vs `use`;
   `[N:]label/path` cross-drive addressing; bounded mounted-drive slots; duplicate
   labels disambiguated by index.

## 9. Open questions

- Bound on simultaneously-mounted drives (4?), label length (16?), path depth / name
  length, max files per directory.
- Label clashes: **resolved — duplicates allowed, disambiguated by index** (§4.2).
  No forced relabel on import, no UUID; `drives label` is available but optional.
- Confirmation UX for `flash` (a `[y/N]` prompt vs a `--force`/`yes` token), given
  the shell is line-based.
- Whether `current` (drive + directory) resets to the default on every boot
  (proposed: yes) or is also remembered (proposed: no — only the *default* persists).
- Hot-plug: re-enumerating drives when a disk is inserted/removed at runtime (later;
  the USB stack already does hot-plug, so there is a pattern to follow).
