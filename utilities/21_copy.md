# Utility: `copy` — copy a file

**Status:** Design — built in the file-commands step (step 4) on hierarchical GSFS
(`docs/persistence.md`). Mutating; the file form works on Phase-2 GSFS. Trails `CLAUDE.md`;
does not amend it.

---

## 1. What it is

`copy <src> <dst>` duplicates a file — same drive or across drives (`copy 0:data/a
1:backup/a`), since every drive is addressable by `[index:]label/path`. It replaces POSIX
`cp`; the full word is barely longer and not cryptic.

## 2. Usage

```
copy 0.1.0 — copy a file

usage:
  copy <src> <dst>    copy the file <src> to <dst>
  copy version        print the version
  copy help           print this message

<src>,<dst> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

`copy` reads `src` and writes `dst` (`read` + `write` underneath), so cross-drive copy is
just read-here, write-there — the no-shared-memory data path (§2.5, `docs/persistence.md`
§6.1). If `dst` exists it is overwritten (announced, not silent; §26.7). `src` is left
untouched. **File-only in the first cut** — recursive directory copy needs a tree walk and
is later work.

## 4. Implementation

Mutating, so the least-authority shape of the other writers (`19_write.md` §4): it drives
`fs` `ReadFile` (op 11) + `WriteFile` (op 10).

## 5. Later (separate doc so it can grow)

- **Recursive** directory copy (`copy <dir> <dir>`), once a tree walk exists.
- Copy-from/to a pipe, when the capability-pipe model lands (Appendix D).
- Overwrite confirm, mirroring `write`/`flash`, if wanted.

## 6. Conformance

Built spec-first against `0_conventions.md`: implements its own `copy help` / `copy version`.
