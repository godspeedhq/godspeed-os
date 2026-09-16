# Utility: `move` - relocate a file

**Status:** **Built + QEMU-verified** (`osdev test files` 21/21) on GSFS0003. Same-drive move
is a **relink** - only the directory entries change, no data copied and (so) no reclamation
needed; `fs` treats a same-directory move as a rename. (Cross-drive move = copy + delete is
later, with multi-drive.) Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`move <src> <dst>` relocates a file to a new path - a different directory, or a different
drive (`move 0:data/a 1:archive/a`). It replaces POSIX `mv`, but **only the relocation
half**: renaming a file in place is a separate verb, `rename` (`23_rename.md`). POSIX's `mv`
secretly does both; GodspeedOS keeps them distinct because they are different acts (§26.5).

## 2. Usage

```
move 0.4.0 - relocate a file

usage:
  move <src> <dst>    move the file <src> to <dst>
  move version        print the version
  move help           print this message

<src>,<dst> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

**Built and shipping.** This section used to read "why it's gated" and explained that `move` was
deliberately withheld because GSFS had no block reclamation, so a move would strand the source's
blocks. Reclamation landed (`docs/persistence.md` §6.11) and `move` shipped with it; the section
described a state that had not been true for some time.

A same-directory move is a **rename** (re-point the entry). Across directories it is an `dir_add`
into the destination followed by a `dir_remove` from the source, both inside one journal
transaction, so a crash leaves the file in exactly one of the two places and never in neither or
both. Moving a directory moves its whole subtree with it; nothing is copied, because nothing needs
to be.

Open capabilities to the moved path are **revoked** (`revoke_open_subtree`), including for every
descendant when a directory moves. A capability names a path, the path no longer names that file,
and a holder that kept using it would be a confused deputy (§7.10, SEC-5).

### A directory cannot be moved into itself, or into its own subtree

`move /a /a/b` would write an entry inside `/a` pointing at `/a`, then unlink `/a` from its parent.
The subtree becomes a cycle that the root cannot reach - and **`drives check` rebuilds the free
bitmap by walking the tree**, so those still-occupied blocks would be marked free and handed to the
next allocation, which overwrites live data. `MAX_TREE_DEPTH` keeps a walk from hanging on it, so it
would show up as a leak that turns into corruption rather than as a hang.

**It is refused in two places, deliberately.** `move` itself refuses before sending
(`move: cannot move into itself`), which gives the better message and costs a round trip. `fs`
refuses it too, because a check in the caller is a convention and only a check in the owner is an
enforcement: `fs` owns the tree, and owns the bitmap rebuild that depends on the tree being acyclic.
Leaving that to the client means the next client - a script, another service, a refactor of this one
- inherits an obligation nobody told it about.

`fs` proves the predicate on every boot (`fs: path guard selftest PASS`), including the case a
sloppy prefix test gets wrong: `/ab` is **not** inside `/a`, and moving it there must succeed.

## 4. Failure

| what happened | what you see |
|---|---|
| the source does not exist | `move: failed (not found, or dest exists?)` |
| the destination already exists | the same line - `fs` distinguishes them in its log |
| moving a directory into itself or its subtree | `move: cannot move into itself` |
| storage is not available | `move: storage unavailable` |

## 5. Later (separate doc so it can grow)

- Cross-DRIVE move, which is genuinely copy-then-delete rather than a re-point, and therefore needs
  a progress report and an interruption story of its own.

## 6. Conformance

Conforms: own `move help` / `move version` (with a real example, per `0_conventions.md`).
