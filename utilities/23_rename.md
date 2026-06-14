# Utility: `rename` — rename a file or directory in place

**Status:** **Built + QEMU-verified** (`osdev test files` 15/15) — a shell built-in over
the `fs` RENAME op, which edits the directory entry in place (no reclamation needed). On
hierarchical GSFS (`docs/persistence.md`). Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`rename <path> <newname>` changes the **name** of a file or directory, in the same
directory — it does not move it. This is the half of POSIX `mv` that is purely a name
change; relocation is `move` (`22_move.md`). Splitting the two is the explicit GodspeedOS
way (§26.5), and it mirrors `drives label`, which renames a *drive* — same idea, one level
up.

## 2. Usage

```
rename 0.1.0 — rename a file or directory in place

usage:
  rename <path> <newname>   rename the entry at <path> to <newname>
  rename version            print the version
  rename help               print this message

<path>    = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
<newname> = a single name component (no '/')
```

## 3. Behaviour

`rename` rewrites the directory entry's name in place — no blocks are read or freed, so it
is cheap and reclamation-free. `<newname>` is one component (slashes are a loud error — to
move across directories, use `move`). Renaming to a name that already exists in the
directory is a defined error, not an overwrite.

## 4. Implementation

Mutating, least-authority shape of the writers (`19_write.md` §4): `fs` edits the parent
directory block (a `Rename` op) and persists it.

## 5. Later (separate doc so it can grow)

- Case/charset rules for names, once a real need surfaces.
- Bulk rename / patterns — only if a genuine use pulls it in (§26.2).

## 6. Conformance

Built spec-first against `0_conventions.md`: implements its own `rename help` / `rename version`.
