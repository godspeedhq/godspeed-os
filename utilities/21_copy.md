# Utility: `copy` - copy a file or a whole subtree

**Status:** **Built + QEMU-verified** (`osdev test files`) - the **file** form *and* the
**recursive** directory form, as a shell built-in. On hierarchical GSFS
(`docs/persistence.md`). Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`copy <src> <dst>` duplicates a file - same drive or across drives (`copy 0:data/a
1:backup/a`), since every drive is addressable by `[index:]label/path`. It replaces POSIX
`cp`; the full word is barely longer and not cryptic.

## 2. Usage

```
copy 0.1.0 - copy a file

usage:
  copy <src> <dst>              copy the file <src> to <dst>
  copy <src> <dst> recursive    copy directory <src> and everything under it
  copy version                  print the version
  copy help                     print this message

<src>,<dst> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

`copy <src> <dst>` reads `src` and writes `dst` (`read` + `write` underneath), so cross-drive
copy is just read-here, write-there - the no-shared-memory data path (§2.5,
`docs/persistence.md` §6.1). If `dst` exists it is overwritten (announced, not silent; §26.7).
`src` is left untouched.

`copy <src> <dst> recursive` duplicates a whole directory subtree. It is the safe default's
opt-in: a plain `copy` of a directory falls through to the single-file path and reports the
source not found, so the destructive/large operation only happens when you ask for it
(mirrors `mkdir … parents`, `delete … recursive`). Refuses to copy a directory into its own
subtree (`copy /a /a/inner recursive`) - that would never terminate.

## 4. Implementation

Mutating, so the least-authority shape of the other writers (`19_write.md` §4): it drives
`fs` `ReadFile` (op 11) + `WriteFile` (op 10). The recursive form adds no new `fs` surface -
copy already lives in the shell, so it walks the source subtree with the **same bounded
`PathStack`** `find` uses (§26.6): pop a source dir, recreate it under `dst` (`Mkdir`, op 13),
copy each file, push each subdir. Loud if the tree is wider than the walk's pending-dir cap
(§3.12), exactly like `find`.

## 5. Later (separate doc so it can grow)

- Copy-from/to a pipe, when the capability-pipe model lands (Appendix D).
- Overwrite confirm, mirroring `write`/`flash`, if wanted.

## 6. Conformance

Built spec-first against `0_conventions.md`: implements its own `copy help` / `copy version`.
