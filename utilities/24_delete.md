# Utility: `delete` - remove a file, directory, or whole subtree

**Status:** **Built + QEMU-verified** (`osdev test files`) on GSFS0003 - reclamation is
intrinsic (delete clears the entry and frees its blocks in the bitmap). Removes a file or an
**empty** directory by default; `delete <path> recursive` removes a non-empty directory and
everything under it. Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`delete <path>` removes a file or directory. It replaces POSIX `rm` - `rm` ("remove") is
cryptic; `delete` says it plainly. It is destructive, so it is **loud** about what it does
(§26.7): there is no trash, no silent undo - a delete is a delete.

## 2. Usage

```
delete 0.1.0 - remove a file or directory

usage:
  delete <path>             remove the file or empty directory at <path>
  delete <path> recursive   remove the directory <path> and everything under it
  delete version            print the version
  delete help               print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

`delete <path>` removes the directory entry **and frees the data blocks** in the bitmap
(GSFS0003 reclamation is intrinsic). A non-empty directory is a **loud error** by default -
no accidental tree wipes - and the error names the opt-in (`use 'delete <path> recursive'`).

`delete <path> recursive` removes a non-empty directory and its whole subtree. The safe
default is deliberate (mirrors `mkdir … parents`, `copy … recursive`): the destructive
operation only happens when you spell it out. There is no trash and no undo (§26.7).

## 4. Implementation

Mutating, least-authority shape of the writers (`19_write.md` §4). Plain delete is `fs`
`Delete` (op 16): unlink the entry, free its blocks. Recursive delete is `fs` `DeleteTree`
(op 19): unlink the entry from its parent, then free the entry **and every descendant** with
a **depth-bounded** post-order subtree walk (§26.6) - capped (`MAX_TREE_DEPTH`) and refused
loudly past it, with small per-level stack frames (the 512-byte directory block is dropped
before recursing). Path-length limits (`PATH_MAX`, the u8 wire `path_len`) bind well before
the depth cap, so the cap is a backstop, not the everyday limit.

## 5. Later (separate doc so it can grow)

- A `[y/N]` confirm for a recursive (or single-file) delete, mirroring `flash`'s
  destructive-confirm (`15_drives.md`), if wanted.
- `delete` + `move` are the two clients that prove the reclamation allocator frees correctly.

## 6. Conformance

Conforms: own `delete help` / `delete version` (with a real example, per `0_conventions.md`).
