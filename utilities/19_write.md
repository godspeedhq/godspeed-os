# Utility: `write` — create, overwrite, or append to a file

**Status:** **Built + QEMU-verified** (`osdev test files`) — a shell built-in over the `fs`
WRITE_FILE / READ_FILE API, on hierarchical GSFS (`docs/persistence.md`). Mutating; inline
content (`write <path> <text>`) with an `append` mode. GSFS0003 reclaims freed blocks, so
overwrite no longer leaks. Trails `CLAUDE.md`; does not amend it.

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
  write <path>                 create an empty file at <path>
  write <path> <content>       create/overwrite <path> with <content>
  write append <path> <content>  append <content> to <path> (create if missing)
  write version                print the version
  write help                   print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

`content` is the rest of the line (quote it to keep spaces). Inline content is one line;
multi-line / piped content arrives with the capability-pipe model (Appendix D.3).

> **Why `append` leads instead of trailing.** Every other modifier sits at the end
> (`mkdir <path> parents`, `delete <path> recursive`). `write` can't: its content is the
> free-form rest of the line, so a trailing word would be swallowed as content. `append`
> therefore comes right after the verb, and only counts as the keyword when followed by
> whitespace — so `write appendix.txt …` is still a plain write to a file named `appendix.txt`.

## 3. Behaviour

Overwrite is **deliberate and announced** (`wrote /path (N bytes)`), never a silent
clobber (§26.7). `write append` reads the current content, concatenates, and writes the whole
file back (`appended N bytes to /path (M total)`); appending to a missing file **creates** it.
The parent directory must exist (no implicit creation). Bounded by the file API's message
size — an append that would exceed the maximum file size is refused loudly, not truncated.

## 4. Implementation

A narrow `ipc_send=["fs"]` built-in; `fs` (`WriteFile`, op 10) holds the disk authority and
enforces. With file-as-capability (`docs/persistence.md` §7) `write` would present a WRITE
cap. **Append is shell-side and adds no new `fs` surface**: it `ReadFile`s (op 11) the current
content, concatenates the new text, and `WriteFile`s the whole file back — read-modify-write,
the same shape `copy` uses. `fs`'s file-size limit bounds the result.

## 5. Later (separate doc so it can grow)

- Confirm-on-overwrite for an existing file (mirrors `flash`'s `[y/N]`), if wanted.
- Write-from-pipe (`<producer> | write <path>`) as the primary bulk path (Appendix D).

## 6. Conformance

Conforms: `write help` (usage with a real example per row, incl. `write /docs/todo.txt
"buy milk"`) and `write version` (number + creator credit) per `0_conventions.md`.
