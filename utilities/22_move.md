# Utility: `move` — relocate a file

**Status:** **Built + QEMU-verified** (`osdev test files` 21/21) on GSFS0003. Same-drive move
is a **relink** — only the directory entries change, no data copied and (so) no reclamation
needed; `fs` treats a same-directory move as a rename. (Cross-drive move = copy + delete is
later, with multi-drive.) Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`move <src> <dst>` relocates a file to a new path — a different directory, or a different
drive (`move 0:data/a 1:archive/a`). It replaces POSIX `mv`, but **only the relocation
half**: renaming a file in place is a separate verb, `rename` (`23_rename.md`). POSIX's `mv`
secretly does both; GodspeedOS keeps them distinct because they are different acts (§26.5).

## 2. Usage

```
move 0.1.0 — relocate a file

usage:
  move <src> <dst>    move the file <src> to <dst>
  move version        print the version
  move help           print this message

<src>,<dst> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour & why it's gated

`move` = copy `src` to `dst`, then **free** `src`. The free step is what blocks it: Phase-2
GSFS has no reclamation (overwrite/delete leak blocks until reformat — a stated carry-over,
`docs/persistence.md`). Until the reclamation phase, a "move" would leave the source's blocks
stranded, so `move` is deliberately not shipped rather than shipped leaking. Cross-drive move
is copy-then-delete; same-drive move re-points the directory entry and frees the old extent
only where layout requires it.

## 4. Implementation (when unblocked)

Mutating, least-authority shape of the writers (`19_write.md` §4): `fs` copy + a real
`Delete` (op TBD) that returns freed blocks to the allocator.

## 5. Later (separate doc so it can grow)

- **Recursive** directory move, once a tree walk + reclamation exist.
- Move is the natural client of the reclamation work — its arrival is the milestone that
  proves the allocator frees correctly.

## 6. Conformance

Conforms: own `move help` / `move version` (with a real example, per `0_conventions.md`).
