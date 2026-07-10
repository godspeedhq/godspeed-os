# Utility: `read` - print a file's contents

**Status:** **Built + QEMU-verified** (`osdev test files` 11/11) - a shell built-in over
the `fs` READ_FILE API, on hierarchical GSFS (`docs/persistence.md`). Read-only. Trails
`CLAUDE.md`; does not amend it.

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
read 0.1.0 - print a file's contents

usage:
  read <path>         print the file at <path>
  read version        print the version
  read help           print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour & bounds

`read` requests the file from `fs` (`ReadFile`, op 11) and writes the bytes to the console.
A file travels in message-bounded chunks (§8.5: 4 KiB max IPC message; §2.5: no shared
memory), so a large file is a sequence of copied reads - the honest, bounded data path
(`docs/persistence.md` §6.1). Reading a directory is a loud error, not a silent dump.

## 4. Implementation

Read-only, so a **shell built-in** sending `ReadFile` to `fs` over a narrow
`ipc_send=["fs"]` cap. `fs` enforces; once file-as-capability lands (`docs/persistence.md`
§7) `read` presents a per-file READ cap instead of a name.

## 5. Later (separate doc so it can grow)

- Paging for long output (screenful at a time) - a console-service concern.
- A hex/binary view for non-text files.
- Range reads (offset + length) once a real need pulls them in (§26.2).

## 6. Conformance

Conforms: `read help` (usage with a real example per row) and `read version` (number +
creator credit) per `0_conventions.md` (the shared `help_block` helper).

Also conforms to **rule 10** (`0_conventions.md` §1.10): the `fs` request is **q-abortable** via
`fs_request_q` - a wait past ~2s prints `(q to quit)` and `q`/`Q`/ESC returns to the prompt (a fast
reply prints nothing). This replaced a bare `request_with_reply`, which rule 10 forbids for an
interactive command.
