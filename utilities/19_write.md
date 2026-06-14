# Utility: `write` — create or overwrite a file

**Status:** **Built + QEMU-verified** (`osdev test files` 11/11) — a shell built-in over
the `fs` WRITE_FILE API, on hierarchical GSFS (`docs/persistence.md`). Mutating; inline
content (`write <path> <text>`) — overwrite still leaks the old extent until the
reclamation phase (a stated carry-over). Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`write <path> [content]` puts content into a file, creating it if absent and replacing its
contents if present. With no `content`, it creates an **empty** file. POSIX has no `write`
*command* — it puts content in files via shell redirection (`echo hi > file`), which leans
on fork/fd machinery GodspeedOS doesn't have; our redirection is the capability-mediated
pipe (Appendix D.3), not yet built. So `write` is the honest inline primitive.

> **Why there is no `touch`.** `touch`'s real job is "update a file's modification
> timestamp"; people abuse it to make an empty file. GSFS inodes carry **no timestamps**
> (deliberate minimalism), so that purpose has no referent here — and "make an empty file"
> is just `write <path>` with no content. One honest verb; no name that lies about its job.

## 2. Usage

```
write 0.1.0 — create or overwrite a file

usage:
  write <path>            create an empty file at <path>
  write <path> <content>  create/overwrite <path> with <content>
  write version           print the version
  write help              print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

`content` is the rest of the line (quote it to keep spaces). Inline content is one line;
multi-line / piped content arrives with the capability-pipe model (Appendix D.3).

## 3. Behaviour

Overwrite is **deliberate and announced** (`fs: wrote /path (N bytes)`), never a silent
clobber (§26.7). The parent directory must exist (no implicit creation). Bounded by the
file API's message size; large writes chunk (`docs/persistence.md` §6.1).

## 4. Implementation

Mutating, so it follows the least-authority reasoning of `drives` (`15_drives.md` §7,
`0_conventions.md` §2): a brokered/standalone path or a narrow `ipc_send=["fs"]` built-in —
final shape decided at build time. `fs` (`WriteFile`, op 10) holds the disk authority and
enforces; with file-as-capability (`docs/persistence.md` §7) `write` presents a WRITE cap.

## 5. Later (separate doc so it can grow)

- **Append** mode (a word, e.g. `write <path> append <content>`), once pipes make
  streaming writes real.
- Confirm-on-overwrite for an existing file (mirrors `flash`'s `[y/N]`), if wanted.
- Write-from-pipe (`<producer> | write <path>`) as the primary bulk path (Appendix D).

## 6. Conformance

Conforms: `write help` (usage with a real example per row, incl. `write /docs/todo.txt
"buy milk"`) and `write version` (number + creator credit) per `0_conventions.md`.
