# Utility: `read` - print a file's contents

**Status:** **Built + QEMU-verified** (`osdev test files` 11/11) - a shell built-in over
the `fs` STAT_FILE and READ_AT API, on hierarchical GSFS (`docs/persistence.md`). Read-only.
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`read <path>` prints a file's contents to the console. It is the counterpart to `write`
(`19_write.md`), and the replacement for POSIX `cat` - whose name ("concatenate") describes
a *different* operation (joining several files) that nobody means when they just want to see
one file. `read` says exactly what it does: read this file out.

Reading **one** file is the whole job. Joining multiple streams is a pipe concern
(Appendix D.3), not an overloaded read command - so `read` takes a single path.

## 2. Usage

```
read 0.4.0 - print a file's contents

usage:
  read <path>         print the file at <path>
  read version        print the version
  read help           print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour & bounds

`read` stats the file through `fs` (`STAT_FILE`, op 12) to learn its size, then streams it
with `READ_AT` (op 26) in `IO_CHUNK` pieces (3556 bytes) and writes the bytes out. A file
travels in message-bounded chunks (§8.5: 4 KiB max IPC message; §2.5: no shared memory), so
a large file is a sequence of copied reads - the honest, bounded data path
(`docs/persistence.md` §6.1). A missing trailing newline is supplied. Reading a directory is
a loud error, not a silent dump - though it is reported as `read: not found: <path>`, the
same words as a missing file. A read that fails part way prints `read: storage error`.

## 4. Implementation

Read-only, so a **shell built-in** sending `STAT_FILE` and `READ_AT` to `fs` over a narrow
`ipc_send=["fs"]` cap. `fs` enforces. File-as-capability has landed (`docs/persistence.md`
§7, `35_fcap.md`), but `read` still addresses the file by name; presenting a per-file READ
cap instead is not done.

## 5. Later (separate doc so it can grow)

- Paging for long output is **done**, as a pipe stage rather than a word on `read`:
  `read <path> | paginate` (`52_paginate.md`).
- A hex/binary view for non-text files.
- Range reads (offset + length) once a real need pulls them in (§26.2).

## 6. Conformance

Conforms: `read help` (usage with a real example per row) and `read version` (number +
creator credit) per `0_conventions.md` (the shared `help_block` helper).

**Does NOT currently conform to rule 10** (`0_conventions.md` §1.10), found 2026-10-09. The `fs`
requests go through a `gs::fs::Fs` handle that is lent no notice, so each request is bounded
(`gs::call::DEFAULT_SECS`, 5 s) but prints no `[q] quit` and cannot be ended with `q`; the
`Cancelled` branches in the handler are unreachable. This said the request was q-abortable via
`fs_request_q`, which no longer exists. `dir` is the one fs-backed command that still lends the
notice (`16_dir.md` §6); the same `.noticing(...)` here is the fix.
