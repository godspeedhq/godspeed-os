# Utility: `find` — search the tree for a name

**Status:** **Built + QEMU-verified** (`osdev test files`) on GSFS0003. A shell built-in
that walks the directory tree client-side via the `fs` LIST_DIR op. Trails `CLAUDE.md`;
does not amend it.

---

## 1. What it is

`find <name> [path]` searches a subtree — the **whole filesystem** (`/`) by default, or a
given starting `path` — for entries whose name **contains** `<name>` (substring match), and
prints each match's full path. So `find report` matches `report.txt`, `2026-report`, etc.;
`find .txt` lists every `.txt`. It is whole-filesystem **enumeration**: the one operation
that reads across the entire tree rather than a single path or directory.

```
find 0.1.0 — search the tree for a name

usage:
  find <name>          search the whole filesystem (from /) for entries named <name>
  find <name> <path>   search only the subtree under <path>
  find version         print the version
  find help            print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

Example:

```
gs> find note.txt
  /docs/sub/note.txt
  find: 1 match(es)
```

## 2. How it works — a tree walk (the tree *is* the index)

`find` is the disciplined realisation of "enumerate everything": it **walks the directory
tree** (GSFS0003's tree is the index — `docs/persistence.md` §6.4), done **client-side** in
the shell via repeated `LIST_DIR` calls. Each directory's entries are listed; a match is
printed as it's found (results **stream**, no buffering of the whole result set); each
subdirectory is pushed onto a pending stack to visit next. So `fs` needs **no new op** and
there is no chunked-reply problem — the shell just keeps asking `fs` to list directories.

## 3. Bounds — loud, not silent (§26.6 / §3.12)

The walk holds a **bounded stack** of pending directories (`FIND_QCAP`, currently 32). A
tree wide/deep enough to exceed it does not silently drop results — `find` prints
`search truncated …` so the user knows the answer is partial. Exact-name match only in this
cut (no globbing/substring — that's §5).

## 4. Why no `fs_index` yet

A global index (`fs_index`, `docs/persistence.md` §6.5) would let `find` skip the walk and
jump straight to matches. It is **deliberately not built** (§26.2): `find` is exactly the
need that would pull it into existence, but the *correct* first implementation is the tree
walk — and for any normal tree the walk is fine. `fs_index` is the optimisation we add the
day the walk is *measured* too slow on a genuinely huge tree, and it will sit behind this
same `find` command (lazy, version-invalidated, rebuilt-from-truth — §6.5). Until then,
`find` is honest and complete; it just walks.

## 5. Later (separate doc so it can grow)

- **Substring match — done** (`find report` matches `report.txt`). **Glob patterns**
  (`find "*.txt"`) remain later, as a *word*-flagged mode, never `-name`.
- **Type filter** (files only / dirs only).
- **`fs_index`-backed fast path** when the tree-walk is measured too slow (§6.5).

## 6. Implementation shape & conformance

A shell built-in (like the other file commands) sending `LIST_DIR` to `fs`; `fs` holds all
disk authority. Conforms: `find help` (usage with a real example per row) and
`find version` (number + creator credit) per `0_conventions.md`.
