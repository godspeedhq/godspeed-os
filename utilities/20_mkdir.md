# Utility: `mkdir` — create a directory

**Status:** Design — built in the file-commands step (step 4) on hierarchical GSFS
(`docs/persistence.md`). Mutating; works on Phase-2 GSFS. Trails `CLAUDE.md`; does not
amend it.

---

## 1. What it is

`mkdir <path>` creates a directory. The parent directory must already exist; creating an
already-existing name is a loud error, never a silent no-op (§3.12). `mkdir` is kept as a
verb — a universal contraction, one of the three short ones we keep (`ls` / `cd` / `mkdir`).

## 2. Usage

```
mkdir 0.1.0 — create a directory

usage:
  mkdir <path>        create the directory at <path>
  mkdir version       print the version
  mkdir help          print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour & bounds

`mkdir` allocates a directory inode and a directory block, then adds an entry to the parent
(`fs` op 13). GSFS bounds it (§26.6): a directory is one 512-byte block — **16 entries** —
in this phase, and a fixed inode count overall; exceeding either is a loud, defined error,
not silent growth. No POSIX permission bits (authority is by capability, §3.3).

## 4. Implementation

Mutating, so the same least-authority shape as the other writers (`19_write.md` §4): `fs`
holds the disk authority (`Mkdir`, op 13) and enforces.

## 5. Later (separate doc so it can grow)

- Create intermediate parents in one call — as a **word**, e.g. `mkdir <path> parents`,
  never `-p` (`0_conventions.md` §4).
- Multi-block directories (lift the 16-entry bound) once a real need pulls it in (§26.2).

## 6. Conformance

Built spec-first against `0_conventions.md`: implements its own `mkdir help` / `mkdir version`.
