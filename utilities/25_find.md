# Utility: `find` — search the tree for a name

**Status:** **Built + QEMU-verified** (`osdev test files`) on GSFS0003. A shell built-in
that walks the directory tree client-side via the `fs` LIST_DIR op. Trails `CLAUDE.md`;
does not amend it.

---

## 1. What it is

`find <pattern> [path]` searches a subtree — the **whole filesystem** (`/`) by default, or a
given starting `path` — for entries whose name matches `<pattern>`, and prints each match's
full path. The match mode is chosen by the pattern itself:

- **Plain word → substring.** `find report` matches `report.txt`, `2026-report`, etc.;
  `find .txt` lists every `.txt`. The friendly default.
- **Contains `*` or `?` → glob** (anchored to the whole name): `*` matches any run of
  characters (including none), `?` matches exactly one. `find *.txt` matches names *ending*
  in `.txt`; `find f?` matches `f1`…`f9` but not `f10`.

It is whole-filesystem **enumeration**: the one operation that reads across the entire tree
rather than a single path or directory.

```
find 0.1.0 — search the tree by name (substring, or glob with */?)

usage:
  find <name>            search from / for entries whose name contains <name>
  find <glob>            glob match: * = any run, ? = one char (e.g. find *.txt)
  find <pattern> <path>  search only the subtree under <path>
  find version           print the version
  find help              print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

Example:

```
gs> find *.txt
  /docs/inside.txt
  /docs/sub/note.txt
  find: 2 match(es)
```

### As a record producer (typed pipes)

Bare `find` prints the matching paths (above); **in a pipe** it is a record producer
(`docs/records.md`, `utilities/31_records.md`) emitting a typed table — columns
**`name` / `type` / `path`** — so each hit's structure is filterable:

```
find *.txt | where type=file        only file hits (not matching directories)
find report | select path           just the paths
find * /docs | where type=dir       directories under /docs
find *.log | to json                the hits as JSON
```

This is the structured form of the planned type filter (§5): `where type=file` / `where
type=dir` replaces a `-type`-style flag.

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
`search truncated …` so the user knows the answer is partial. The glob matcher is itself
bounded: iterative backtracking (`glob_match`), no recursion and no allocation.

## 4. Why no `fs_index` yet

A global index (`fs_index`, `docs/persistence.md` §6.5) would let `find` skip the walk and
jump straight to matches. It is **deliberately not built** (§26.2): `find` is exactly the
need that would pull it into existence, but the *correct* first implementation is the tree
walk — and for any normal tree the walk is fine. `fs_index` is the optimisation we add the
day the walk is *measured* too slow on a genuinely huge tree, and it will sit behind this
same `find` command (lazy, version-invalidated, rebuilt-from-truth — §6.5). Until then,
`find` is honest and complete; it just walks.

## 5. Later (separate doc so it can grow)

- **Substring match — done** (`find report` matches `report.txt`). **Glob patterns — done**
  (`find *.txt`, `find f?`): a pattern with `*`/`?` switches to anchored glob matching, no
  `-name` flag — the pattern speaks for itself.
- **Type filter — done** via the record pipe (`find … | where type=file`), not a flag.
- **`fs_index`-backed fast path** when the tree-walk is measured too slow (§6.5).

## 6. Implementation shape & conformance

A shell built-in (like the other file commands) sending `LIST_DIR` to `fs`; `fs` holds all
disk authority. Conforms: `find help` (usage with a real example per row) and
`find version` (number + creator credit) per `0_conventions.md`.
