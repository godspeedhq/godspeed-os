# Utility: `tree` - print the directory hierarchy

**Status:** **Built + QEMU-verified** (`osdev test files`) as a shell built-in. On hierarchical
GSFS (`docs/persistence.md`). Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`tree [path]` prints the directory hierarchy under `path` (default: the current directory) as
an indented tree - the read-only companion to `ls` for seeing structure at a glance. It keeps
the same name as the POSIX/util `tree` because the name is already plain and not cryptic.

## 2. Usage

```
tree 0.1.0 - print the directory hierarchy

usage:
  tree            tree of the current directory
  tree <path>     tree rooted at <path>
  tree version    print the version
  tree help       print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

- **Box-drawing, like Unix `tree`.** Connectors `├── ` / `└── ` mark each entry and `│` / blank
  draw the continuation lines, so structure reads at a glance. A trailing `/` still marks
  directories (the console is monochrome - there's no colour to lean on).
- **UTF-8.** The box glyphs (`U+2500..U+253C`) are emitted as UTF-8 and render on **both** the
  serial terminal and the framebuffer console - the fbcon decodes UTF-8 and draws the box
  glyphs with **procedural strokes** (the antialiased Noto font it uses for text has no U+2500
  block; procedural strokes also connect cell-to-cell exactly). See `kernel/src/arch/x86_64/fb.rs`.
  Unsupported codepoints render as `?`, never silently dropped (§3.12).
- The root line shows the path as given; deeper entries show their basename.
- Ends with a blank line then a summary: `N directories, M files` (counting everything *under*
  the root).
- A path that names a file prints just that file; a missing path is a loud error (§3.12).

Example:

```
gsh> tree /docs
/docs
├── a.txt
└── sub/
    └── b.txt

1 directory, 2 files
```

## 4. Implementation

Read-only, so no capability beyond the `fs` `ListDir` (op 14) it already uses for `ls`/`find`.
It adds **no new `fs` surface**: the hierarchy is reconstructed client-side with the **same
bounded-walk discipline** `find` uses (§26.6) - a fixed-capacity explicit stack, depth-first,
**no recursion**. Every child (file or dir) is pushed so siblings nest correctly, and a
directory's whole subtree drains before its next sibling (LIFO + reverse-push). If a tree is
wider than the walk's capacity it reports truncation rather than silently dropping branches
(§3.12), exactly like `find`. Path-length limits (`PATH_MAX`, the u8 wire `path_len`) bound
real depth to ~60 levels, well within the walk.

The connectors come for free from the same DFS: each node carries whether it is its parent's
**last** child (drives `└──` vs `├──`), and a small `level_last[depth]` array records each
ancestor's last-child flag for the `│`/blank prefix. Because the DFS finishes a subtree before
its siblings, that array is always valid when a node prints - no recursion, no per-node prefix
storage.

## 5. Later (separate doc so it can grow)

- A depth limit flag (`tree <path> depth <n>`) if deep trees get noisy.
- Sizes / a `-s`-style column, reusing the size `ls` already shows.

## 6. Conformance

Conforms: own `tree help` / `tree version` (with a real example, per `0_conventions.md`).

Also conforms to **rule 10** (`0_conventions.md` §1.10): each `LIST_DIR` step is **q-abortable** via
`fs_request_q` - a wait past ~2s prints `(q to quit)` and `q`/`Q`/ESC returns to the prompt (a fast
reply prints nothing). This replaced a bare `request_with_reply`, which rule 10 forbids for an
interactive command.
