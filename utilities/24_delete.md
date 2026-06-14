# Utility: `delete` — remove a file or directory

**Status:** **Built + QEMU-verified** (`osdev test files` 21/21) on GSFS0003 — reclamation is
intrinsic (delete clears the entry and frees its blocks in the bitmap). Removes a file or an
**empty** directory (a non-empty directory is a loud error; recursive delete is future work).
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`delete <path>` removes a file or directory. It replaces POSIX `rm` — `rm` ("remove") is
cryptic; `delete` says it plainly. It is destructive, so it is **loud** about what it does
(§26.7): there is no trash, no silent undo — a delete is a delete.

## 2. Usage

```
delete 0.1.0 — remove a file or directory

usage:
  delete <path>       remove the file or empty directory at <path>
  delete version      print the version
  delete help         print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour & why it's gated

`delete` removes the directory entry **and frees the inode + data blocks**. That free is the
blocker: Phase-2 GSFS has no reclamation (`docs/persistence.md`), so a delete today would
only unlink — leaking the blocks — which is exactly the silent-loss behaviour the system
refuses to ship. So `delete` waits for the reclamation phase, alongside `move`
(`22_move.md`). A non-empty directory delete is a loud error until recursive delete exists
(no accidental tree wipes).

## 4. Implementation (when unblocked)

Mutating, least-authority shape of the writers (`19_write.md` §4): a real `fs` `Delete` (op
TBD) that unlinks the entry, frees the inode, and returns the data blocks to the allocator.

## 5. Later (separate doc so it can grow)

- **Recursive** directory delete (a word, e.g. `delete <dir> tree`), with a `[y/N]` confirm
  for a non-empty target — mirroring `flash`'s destructive-confirm (`15_drives.md`).
- A confirm prompt for single files too, if wanted.
- `delete` + `move` are the two clients that prove the reclamation allocator frees correctly.

## 6. Conformance

Will be built spec-first against `0_conventions.md`: its own `delete help` / `delete version`.
