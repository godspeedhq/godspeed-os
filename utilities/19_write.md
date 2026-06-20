# Utility: `write` — create, overwrite, append, or prepend a file

**Status:** **Built + QEMU-verified** (`osdev test files`, `osdev test script`) — a shell
built-in over the `fs` WRITE_FILE / WRITE_NEW / WRITE_AT / READ_FILE API, on hierarchical GSFS
(`docs/persistence.md`). Mutating; inline content (`write <path> <text>`) with `append` and
`prepend` modes, and the **pipe sink** (`<producer> | write [append|prepend] <path>`). GSFS0003
reclaims freed blocks, so overwrite no longer leaks. Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`write <path> [content]` puts content into a file, creating it if absent and replacing its
contents if present. With no `content`, it creates an **empty** file. POSIX has no `write`
*command* — it puts content in files via shell redirection (`echo hi > file`), which leans on
fork/fd machinery GodspeedOS doesn't have. Our "redirection" is the capability-mediated pipe
(`docs/pipes.md`, Appendix D.3): `<producer> | write <path>` — `| write` **is** the redirect, so
there is no `>` operator (a second syntax for one mechanism — see `docs/pipes.md` "Why there is
no `>`"). `write` is both the inline primitive and that pipe sink.

> **Why there is no `touch`.** `touch`'s real job is "update a file's modification
> timestamp"; people abuse it to make an empty file. GSFS inodes carry **no timestamps**
> (deliberate minimalism), so that purpose has no referent here — and "make an empty file"
> is just `write <path>` with no content. One honest verb; no name that lies about its job.

## 2. Usage

```
write 0.1.0 — create, overwrite, append, or prepend a file

usage:
  write <path>                     create an empty file at <path>
  write <path> <content>           create/overwrite <path> with <content>
  write append <path> <content>    add <content> to the END of <path> (create if missing)
  write prepend <path> <content>   add <content> to the FRONT of <path> (create if missing)
  <producer> | write [append|prepend] <path>   save piped output to a file
  write version                    print the version
  write help                       print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

`content` is the rest of the line (quote it to keep spaces). Plain `write` always **overwrites**;
the additive behaviour is the explicit keyword `append`/`prepend`, never punctuation. Piped content
(`about | write /about.txt`) goes through the same sink.

> **Why `append`/`prepend` lead instead of trailing.** Every other modifier sits at the end
> (`mkdir <path> parents`, `delete <path> recursive`). `write` can't: its content is the
> free-form rest of the line, so a trailing word would be swallowed as content. The keyword
> therefore comes right after the verb, and only counts when followed by whitespace — so
> `write appendix.txt …` is still a plain write to a file named `appendix.txt`.

## 3. Behaviour

Overwrite is **deliberate and announced** (`wrote /path (N bytes)`), never a silent clobber
(§26.7). `append`/`prepend` add to the end / front (`appended N bytes to /path`), creating a
missing file. The parent directory must exist (no implicit creation). `prepend` is honestly a
**full-file rewrite** — there is no insert-at-front in the filesystem, so it costs the same as
rewriting the file (stated, not hidden, §26.7).

## 4. Implementation

A narrow `ipc_send=["fs"]` built-in; `fs` (`WriteFile`, op 10) holds the disk authority and
enforces. With file-as-capability (`docs/persistence.md` §7) `write` would present a WRITE cap.
**Append/prepend are shell-side and add no new `fs` surface** (`fs_stream_combine`): they stream
`[old|new]` (append) or `[new|old]` (prepend) to a temp file — reading the intact original via
`ReadAt` while writing `WriteNew`/`WriteAt` chunks — then atomically replace the target
(`Delete` + `Move`). Constant memory (one chunk scratch), any file size; the same streaming the
pipe `write` sink and `copy` use. This lifts the old 4 KiB read-modify-write ceiling.

## 5. Later (separate doc so it can grow)

- Confirm-on-overwrite for an existing file (mirrors `flash`'s `[y/N]`), if wanted.

## 6. Conformance

Conforms: `write help` (usage with a real example per row, incl. `write /docs/todo.txt
"buy milk"`) and `write version` (number + creator credit) per `0_conventions.md`.
