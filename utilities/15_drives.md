# Utility: `drives` — manage attached disks

**Status:** **Built + QEMU-verified** (step 3 = the data primitives: `flash` / `label` /
list, as a shell built-in over `fs` — `osdev test drives` 7/7). The boot-layer
(`godspeed install` / `update` / `default`) and the multi-drive selectors (`use` /
`use default`) are reserved and specified here so the vocabulary is coherent from the
start, but are later steps (see §8). This doc is the user-facing utility surface; the
architecture/rationale lives in `docs/drives.md`
(multi-drive model, addressing, default-flag) and `docs/prime.md` (the boot layer).
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`drives` manages attached disks from the shell: list them, format (flash) a raw one as
GSFS, name it, and — later — choose the working drive and the boot drive. It is the
user-facing front end to `fs` + `block-driver`.

Because GodspeedOS boots from a removable image and disks are pluggable, the system needs
an explicit, *user-initiated* way to act on a disk — which is also what keeps it honest:
the constitution forbids *silent* reformatting (§3.12), and a human running `drives flash`
(and confirming `[y/N]`) is the opposite of silent.

## 2. Two layers: a data drive is not a bootable drive

The single most important distinction this utility draws:

- **`drives flash`** makes a drive a **GSFS data drive** — a place to hold files. That is
  *all* it does. The drive is **not** bootable; it carries no kernel, no bootloader.
- **`drives godspeed install`** makes a drive a **bootable GodspeedOS** — it stamps the
  ESP boot region (Limine + kernel, A/B slots) *and* a GSFS world onto it (GodspeedOS
  Prime, `docs/prime.md`). This is a separate, heavier operation.

The boot-layer verbs live under a **`godspeed` sub-namespace** (`drives godspeed install`
/ `update` / `default`) because their object differs from the data verbs'. In the data
layer the object *is* the drive — you `flash` the drive, `label` the drive — so flat verbs
read right. In the boot layer the object is **GodspeedOS**, installed *onto* a drive, so
the namespace names it and a bare `drives install` ("install the drive?") never happens.

Conflating them is the mistake to avoid: formatting a data disk and installing an OS are
different acts with different consequences, so they are different verbs. Likewise there
are **two distinct "defaults"** (§5), one per layer — never one ambiguous `default`.

## 3. Addressing a drive — `index | label | index:label`

Every subcommand that picks a drive takes the same **drive selector**:

```
  <drive> ::= 0          index        (location — the SATA port; changes on replug)
            | data       label        (identity — stored in the GSFS superblock)
            | 1:data     index:label  (always unambiguous)
```

This is **identity over location** (invariant 11): the **label is the drive's identity**
(stable across replug), the **index is its location** (the port). You normally use the
label alone; you prefix the index only to disambiguate.

**Ambiguous label → loud, teaching refusal** (§3.12, §26.7). If two drives share a label,
a bare-label command does not guess — it refuses, lists the matches, and prints the exact
disambiguated commands to run:

```
gs> drives flash data
  drives: 'data' is ambiguous — 2 drives are labelled 'data':
    0:data   16 MiB   GSFS
    1:data   32 MiB   GSFS
  run one of:
    drives flash 0:data
    drives flash 1:data
```

A **raw** (unformatted) drive has **no label yet** — flashing is what names it — so a raw
drive is addressable only by **index** (its sole honest handle). While exactly one disk is
attached, `<drive>` defaults to that disk, so the index can be omitted entirely.

### 3.1 There is no `mount` (and why)

A POSIX reflex says "you flash a disk, then you *mount* it." GodspeedOS has **no `mount`**,
deliberately. In Unix, `mount` grafts a filesystem into the *one global directory tree*
(`/mnt/usb`) — it exists because Unix has a single unified namespace you must splice
external filesystems into. GodspeedOS has no such tree: **each drive is its own
independent GSFS**, addressed as `[index:]label/path`. There is nothing to graft into, so
there is nothing to mount.

A drive's availability **is** its physical presence:

```
  plug in a raw drive → `drives` shows it as raw
  flash it            → available at once (addressable by index/label)
  unplug it           → it disappears from `drives`
  replug it           → it reappears
```

When you address `1:data/notes.txt`, `fs` reads that drive's superblock on demand and
walks the path — no "load into fs" step, no mount table, no bounded mount-slots. "Where am
I working" is a single **current location** (drive + directory), moved by `cd` (a
file-command utility, §3.2), *not* a mount. This note exists so the concept is not
reintroduced by reflex (cf. `14_poweroff.md`).

### 3.2 There is no `drives use` either — `cd` is the one location pointer

A second POSIX reflex: "set the current drive, then `cd` within it." That is also two
pointers where one suffices. GSFS is hierarchical, so a **current directory** already
exists; the honest model is that *where you are* is one address — **drive + path** — moved
by a single verb. `cd` takes a full drive-qualified address, so it subsumes "pick a drive"
entirely:

```
  cd 1:data/projects    current = drive 1 (data), dir /projects
  cd /etc               current drive, absolute path
  cd ../bin             relative
  cd archive            switch to the 'archive' drive's root
```

`cd` is **not** an unwanted import — a "where am I" pointer is inherent to any hierarchical
namespace (DOS and Windows have `cd` too), and real directories were a deliberate GSFS
choice. `cd` is a **file-command utility** (its own doc, step 3-file-commands), not a
`drives` subcommand — `drives` manages *drives*; `cd`/`ls`/`cat` navigate within and across
them. The one thing the old `drives use default` would have added — *persisting* a starting
drive across boots (a session `cd` does not) — is **deferred** (§26.2): decided when file
commands + multi-drive exist, not invented now.

## 4. Labelling is optional, and separate from flashing

`flash` *formats*; `label` *names*. They are deliberately separate verbs:

- `drives flash 1` → working GSFS storage immediately, addressed by index. No name forced.
- `drives label 1 archive` → give it identity later, when identity starts to matter.
- `drives flash 1 archive` → the one-shot convenience (format **and** name).

An unlabelled drive is not broken — it genuinely *has no identity yet*, so addressing it by
index (location) is honest, not a fallback. Forcing a name at flash time would be imposing
policy; GodspeedOS adds identity deliberately (like the boot/default flags), never by
compulsion.

## 5. The boot default (later — Prime)

There is no `default` in the first cut because, with one data drive and no install, there
is nothing to choose. The one settled persistent default is the **boot default**:

| Default | Verb | Layer | Means |
|---------|------|-------|-------|
| Boot default | `drives godspeed default <drive>` | boot | which **installed** GodspeedOS the machine boots (ESP/bootloader level; `docs/prime.md`) |

A persistent *working-location* default — booting straight into a particular data drive,
which a session `cd` does not survive — is the only thing the dropped `drives use default`
would have added. It is **deferred** (§3.2, §26.2): decided alongside the file commands and
multi-drive, not invented now. (When it lands it is the data layer's concern, kept distinct
from the boot default — you might boot off drive 0 yet work on drive 1.)

## 6. Command set

| Command | Effect | Persists? | Step |
|---------|--------|-----------|------|
| `drives` | list every drive: index, label, status, size, current/default | — | **3** |
| `drives flash <drive> [label]` | format `<drive>` as a GSFS data drive (asks `[y/N]` — it ERASES); optional label; usable at once | data: yes | **3** |
| `drives label <drive> <name>` | name / rename a drive — rewrites the superblock (duplicates allowed, §3) | data: yes | **3** |
| `drives reset <drive>` | un-format a drive back to raw (asks `[y/N]` — it ERASES the GSFS marker); the inverse of `flash`. NOT a secure wipe (data blocks remain) — a quick clean slate, mainly for re-testing the raw→flash path | data: yes | **3** |
| `drives godspeed install <drive>` | install bootable GodspeedOS onto the drive (Prime) | **yes** | 6 |
| `drives godspeed update <drive>` | A/B kernel update of an installed drive | **yes** | 6 |
| `drives godspeed default <drive>` | which installed GodspeedOS the machine boots | **yes** | 6 |
| `drives godspeed` | list installed GodspeedOS drives / help | — | 6 |
| `drives version` | print the version | — | **3** |
| `drives help` | print usage | — | **3** |

Drive *contents* (`ls` / `cat` / `write` / `cd` / `mkdir`) are **their own utilities**, not
`drives` subcommands — they operate on paths within a drive, addressable as
`[index:]label/path` or `/abs` / `rel` on the current drive (`docs/drives.md` §4.1).

## 7. Implementation shape: a shell built-in sending to `fs` (as built)

> **Decided at build time (step 3b).** The earlier plan here was a *standalone service*
> like `observe`. Building it surfaced why that's the wrong shape for storage commands,
> and `drives` ships as a **shell built-in** instead. Recorded honestly, not silently.

`drives` (and the file commands) are **shell built-ins that send the drives/file API to
`fs` over IPC**; `fs` holds and enforces *all* disk authority. The shell gains only a
single narrow `ipc_send = ["fs"]` cap (plus its own endpoint for the reply-cap pattern) —
**not** any new disk authority of its own. Three reasons the built-in is right here, where
`observe` went standalone:

- **`fs` is the enforcing authority, not the shell.** A `SEND` cap to `fs` is not a
  dangerous capability — it can only *ask* `fs`, which validates every request. Adding it
  to the shell creates no new dangerous combination; the shell's `spawn`/`kill`/`restart`
  caps are unchanged. (Contrast `observe`, which needed isolation because it would
  otherwise hold introspection *alongside* the lifecycle caps.)
- **Per-command services would be absurd.** A standalone `read`/`write`/`ls`/… each needs
  console + `fs` caps + a way to receive its arguments — ten services where one narrow
  send-cap on the shell suffices. The least-authority *win* is illusory once `fs` is the
  gatekeeper.
- **The `[y/N]` confirm is trivial in-shell** — the shell already owns the console and the
  read loop; no console-handoff dance is needed.

> **If least authority ever demands it**, the escape hatch is a *single* standalone
> `files` broker the shell forwards command lines to — not one service per verb. Not
> needed now (`fs` is the gate), recorded so the option is on the table.

After a flash the drive is **usable immediately** — no reboot. A reboot only *proves*
persistence (the bytes survive a power-cycle); it is never part of the workflow.

## 8. Build order (mirrors `docs/drives.md` §8)

1. **Hierarchical GSFS** — done (inodes + directories + path walking; `docs/persistence.md`).
2. **`drives` data primitives (step 3, this cut):** `flash` / `label` / list, on the
   single attached drive — proves the format-over-IPC + raw-tolerant-`fs` loop, and lets
   the OS format its own SSD (which unblocks on-hardware persistence verification).
   Needs: `block-driver` capacity request (IDENTIFY sectors → size the filesystem);
   `fs` raw-tolerant (serve the drives API even with no filesystem) + in-OS `format()`.
3. **File commands** — `ls` / `cat` / `write` / `cd` / `mkdir` on the current drive; `cd`
   is the single current-location pointer (drive + dir).
4. **Multi-drive** — enumerate all SATA disks; per-drive block IPC; `cd [index:]label/path`
   cross-drive addressing read on demand; duplicate labels disambiguated by index;
   (deferred) a persistent working-location default.
5. **Boot layer (Prime)** — `drives godspeed install` / `update` / `default`; ESP boot
   region + A/B kernel slots (`docs/prime.md`).

## 9. `help` / `version` (convention shape, `0_conventions.md`)

```
drives 0.1.0 — manage attached disks (format, name, select)

usage:
  drives                        list attached drive(s)
  drives flash <drive> [label]  format <drive> as a GSFS data drive (ERASES; asks y/N)
  drives label <drive> <name>   name / rename a drive
  drives version                print the version
  drives help                   print this message

drive selector:
  <drive> = index | label | index:label   (e.g. 0, data, 1:data)
            an ambiguous label refuses and prints the disambiguated commands

subcommand help:
  drives flash help
  drives label help
```

> **Conformance note (honest, per `0_conventions.md` §3).** `drives` is being built
> spec-first against the conventions, so it implements its own `help` / `version` and
> per-subcommand help from the start (like `observe`, unlike the older shell built-ins).
