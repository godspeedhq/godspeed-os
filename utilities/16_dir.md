# Utility: `dir` - list a directory

**Status:** **Built + QEMU-verified** (`osdev test files` 222/0, `osdev test shell` 174/0) - a
shell built-in over the `fs` LIST_DIR API, on hierarchical GSFS (`docs/persistence.md`). Read-only.
Trails `CLAUDE.md`; does not amend it. Shared addressing/current-location rules live in `17_cd.md`
and `docs/drives.md` §4.1.

**Renamed from `ls` (2026-09-17).** Typing `ls` is not an error you have to decode: the shell names
the word for you, once, and does not run anything (`FOREIGN_HINTS` in `services/shell`).

---

## 1. What it is

`dir` lists the entries of a directory - each name, whether it is a file or a directory,
and (for files) the size. With no argument it lists the **current directory** (`20_cd.md`);
with a path it lists that directory. `dir` is read-only: it never changes the disk.

### Why `dir` and not `ls`, and not `list`

**`ls` is an abbreviation of "list", chosen for a teletype.** It came to Unix from Multics, and the
reason it is two letters is that early Unix was typed on a Model 33 at ten characters a second,
where every keystroke was physical work - the same pressure that produced `cp`, `mv`, `rm` and
`creat`. It was never chosen because it described the operation well, so replacing it is not
departing from a well-chosen name; it is supplying precision that never existed.

It was also the last POSIX abbreviation in this vocabulary. Everything around it is already its own
word - `read`, `write`, `move`, `rename`, `delete`, `copy`, `find`, `tree`, `match`, `count`,
`first`, `last` - where POSIX would say `cat`, `mv`, `rm`, `cp`, `grep`, `wc`, `head`, `tail`. `ls`
was the odd one out, inconsistent with its siblings rather than merely with the model.

**`list` was rejected, and the reason is specific to this shell.** "List" is a generic verb, and
this shell lists drives, caps, cores and events. A bare `list` would be genuinely ambiguous here in
a way it would not be on a system that only ever lists one thing. The original name's vagueness was
inherited, not a reason to keep it.

**`dir` names its subject rather than the operation performed on it**, and the argument for it owes
nothing to DOS: **`mkdir` already teaches the word.** Somebody who has typed `mkdir /docs` has been
told that the word for this thing is "dir"; `dir /docs` then shows the thing `mkdir` made. The pair
is coherent on its own terms. And unlike `ls`, it is transparent - a reader who has never seen it
reads "directory", where `ls` is opaque unless you already know Unix. That is the actual
discriminator: not length, but whether the name explains itself.

It can be concrete because GSFS has exactly one kind of container. PowerShell could not be - having
to cover a filesystem, a registry and a certificate store, its noun had to generalise, which is how
a rename aimed at clarity arrived at `Get-ChildItem`. There are no providers here, so there is no
abstraction to leak into the name.

So the three short names kept are `dir`, `cd` and `mkdir`, and each is kept for the same reason
rather than for brevity: it says what it does.

## 2. Usage

```
dir 0.4.0 - list a directory (records when piped)

usage:
  dir                       list the current directory
  dir <path>                list the directory at <path>
  dir long                  one entry per line with type, size and MODIFIED time
  dir human                 sizes as KiB/MiB/GiB rather than raw bytes
  dir [path] | <verb>       piped: emits records (name/type/size)
  dir | select … / sort …   project / order the listing
  dir version               print the version
  dir help                  print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

The words combine, in any order, and mix with a path: `dir long human /projects` and
`dir /projects human long` are the same command. **Words, not flags** (`0_conventions.md` rule 4) -
there is no `-lh`, because a flag is a thing you have to have been told.

Example:

```
gsh> dir /projects
  NAME            TYPE   SIZE
  notes.txt       file   18 B
  drafts          dir    -

gsh> dir long human /projects
  NAME                  TYPE        SIZE  MODIFIED
  notes.txt             file       18 B  2026-09-17 05:21
  drafts                dir            -  unknown
```

### Why the terse form is still the default

`dir` is read far more often than it is studied. The common use is "what is in here", and a wall of
columns answers a question that was not asked. The long form is there when the question IS when or
how big, and it is one word away.

### `unknown` is a real answer, not a missing one

A `MODIFIED` column reads `unknown` when the filesystem genuinely records no time for that entry -
a file written before the volume recorded times at all, or one written while the machine had no
idea what time it was.
**It is never shown as 1970**, because a wrong date is worse than an absent one: a date you can see
is a date you will act on. See `docs/gsfs-next.md` §3.

Sizes line up as a column, which needed more care than it looks: a Rust `Display` implementation
silently ignores the width it is given unless it asks for it, so `{:>10}` did nothing until the
renderer was changed to pad explicitly. A column of ragged numbers is not a column.

## 2a. As a record producer (typed pipes)

`dir` is a **record producer** (`docs/records.md`, `utilities/31_records.md`): bare it prints
the text listing above, but **in a pipe** it emits a typed **table** with columns
**`name` / `type` / `size`** (`type` is `file`/`dir`; `size` is the byte count for files and
empty for directories). So the structured verbs operate on real fields instead of re-parsing
text:

```
dir | where type=file               only files
dir /docs | where size>0            non-empty files
dir | select name size              keep two columns
dir | where type=file | sort reverse size   biggest files first
dir | to json                       the listing as JSON
```

This is the structured replacement for the old plan to bolt name/size sorting onto `dir` itself
(§5) - sorting and filtering are the pipe's job (`where`/`select`/`sort <col>`), not flags on
`dir`. A *text* filter (`match`/`count`/`first`/`last`) on `dir` output is a loud, guided error
(it is records, not text - use `where`, or `to json` to get text first).

## 3. Addressing & current location

`<path>` is the standard file address: a `[index:]label/path` on any present drive, a
`/absolute` path on the current drive, or a `relative` path from the current directory.
The current location is one drive+directory pointer moved by `cd` (`20_cd.md`).

## 4. Implementation

Read-only, so `dir` is a **shell built-in** that sends `ListDir` to `fs` (op 14) over a
narrow `ipc_send=["fs"]` cap and formats the reply. `fs` enforces; the shell's authority is
not widened beyond reading (`0_conventions.md` §2). Two code paths share that one `fs` call:
`cmd_ls` formats the text listing (bare `dir`), and `build_ls_table` parses the same reply into
the `name`/`type`/`size` table for the record pipe (`is_record_producer` routes a piped `dir` to
it before the text producers are consulted).

## 5. Later (separate doc so it can grow)

- A long/short form toggle (a *word*, e.g. `ls long`, never `-l` - `0_conventions.md` §4):
  show generation, block extent, file-capability state once file-as-capability lands
  (`docs/persistence.md` §7).
- A recursive `ls tree`. (Sorting and filtering by name/size are **done** - they are the
  record pipe's job now, §2a, not flags on `dir`.)

## 6. Conformance

Conforms: `ls help` (usage with a real example per row) and `ls version` (number +
creator credit) per `0_conventions.md` (the shared `help_block` helper).

Also conforms to **rule 10** (`0_conventions.md` §1.10): the `fs` request is **q-abortable** via
`fs_request_q` - a wait past ~2s prints `(q to quit)` and `q`/`Q`/ESC returns to the prompt (a fast
reply prints nothing). This replaced a bare `request_with_reply`, which rule 10 forbids for an
interactive command.
