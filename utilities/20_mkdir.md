# Utility: `mkdir` - create a directory

**Status:** **Built + QEMU-verified** (`osdev test files` 11/11) - a shell built-in over
the `fs` MKDIR API, on hierarchical GSFS (`docs/persistence.md`). Mutating. Trails
`CLAUDE.md`; does not amend it.

---

## 1. What it is

`mkdir <path>` creates a directory. The parent directory must already exist; creating an
already-existing name is a loud error, never a silent no-op (§3.12). `mkdir` is kept as a
verb - a universal contraction, one of the two short ones we keep (`cd` / `mkdir`).

## 2. Usage

```
mkdir 0.4.0 - create a directory

usage:
  mkdir <path>        create the directory at <path>
  mkdir <path> parents   create <path> and any missing parents
  mkdir <a>,<b>,...   create each (comma list, no spaces, at most 16)
  mkdir version       print the version
  mkdir help          print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour & bounds

`mkdir` allocates a directory inode and a directory block, then adds an entry to the parent
(`fs` op 13; `parents` is op 18). GSFS bounds it (§26.6) with a fixed inode count overall;
running out is a loud, defined error, not silent growth. (A directory is no longer one
16-entry block - see §5.) No POSIX permission bits (authority is by capability, §3.3).

Success prints `created <path>`. A request whose answer never came is reported as
`mkdir: OUTCOME UNKNOWN - <path> MAY HAVE BEEN created`, not as a failure.

## 4. Implementation

Mutating, so the same least-authority shape as the other writers (`19_write.md` §4): `fs`
holds the disk authority (`Mkdir`, op 13) and enforces.

## 5. Built / later

- **`mkdir <path> parents` - done.** Creates every missing parent directory in one call
  (a *word*, never `-p`, per `0_conventions.md` §4); `fs` walks the path component by
  component, creating what's missing. Idempotent; errors only if a component is in the way
  as a file. Plain `mkdir <path>` stays strict (parent must exist) - `parents` is opt-in.
- **Later:** on GSFS0003 directories already grow without bound (the old 16-entry limit is
  gone - `docs/persistence.md` §6.4), so that item is moot.

## 6. Conformance

Conforms: `mkdir help` (usage with a real example per row) and `mkdir version` (number +
creator credit) per `0_conventions.md` (the shared `help_block` helper).
