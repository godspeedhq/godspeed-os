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
and (for files) the size. With no argument it lists the **current directory** (`17_cd.md`);
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

### Four columns, and why there is no fifth

`dir` shows **name, type, size and when it changed**. There are no words to turn any of it on; that
is the listing.

**This is not `ls -l` made default.** `ls -l` is opt-in on Unix because it adds mode bits, link
count, owner and group - four columns of POSIX bookkeeping. None of them exist here, so removing
them is not a choice this made: there is nothing to remove. What is left is four columns, and four
columns is not a wall.

**There is no permissions column because permissions are not a property of a file.** Authority here
is a capability, and a capability is held by a HOLDER - so "who can read this?" has no per-file
answer to put in a column. The question does not live on the file; it lives on whoever holds a cap
to it, and `caps` is what answers it. This is the one place the capability model visibly changes
what a familiar command can even mean.

**Sealing is the exception that proves it**, and why `seal` sits in the TYPE column rather than in a
column of its own: a seal genuinely IS a property of the file, recorded on disk, constraining what
anyone may do to it regardless of what they hold. It also costs no width, because sealing is
file-only (`fs` refuses "only a file can be sealed"), so TYPE stays single-valued.

A **directory** shows `-` for size. A directory occupies blocks, but reporting those answers a
question nobody asked and would not equal the sum of what is inside it.

`unknown` in MODIFIED is a real answer, not a missing one - see below.

## 2. Usage

```
dir 0.4.0 - list a directory (records when piped)

usage:
  dir                       list the current directory
  dir <path>                list the directory at <path>
  dir bytes                 sizes as an exact byte count, not KiB/MiB/GiB
  dir [path] | <verb>       piped: emits records (name/type/size)
  dir | select … / sort …   project / order the listing
  dir version               print the version
  dir help                  print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

The word mixes with a path in either order: `dir bytes /projects` and `dir /projects bytes` are the
same command. **Words, not flags** (`0_conventions.md` rule 4) -
there is no `-lh`, because a flag is a thing you have to have been told.

Example:

```
gsh> dir /
/
  NAME                  TYPE       SIZE  MODIFIED
  canary.txt            file       41 B  unknown
  a.[2Jb.txt            file       49 B  unknown
  .gsh_history          file      664 B  2026-09-17 14:09
  fz                    dir           -  unknown
  clock.last            file       10 B  unknown
  5 entries

gsh> dir bytes /
/
  NAME                  TYPE       SIZE  MODIFIED
  canary.txt            file         41  unknown
  a.[2Jb.txt            file         49  unknown
  .gsh_history          file        651  2026-09-17 14:09
  fz                    dir           -  unknown
  clock.last            file         10  unknown
  5 entries
```

Both captured from one boot of `osdev test fs-fuzz`, not written by hand. Three things in them are
worth pointing at:

**The count is LAST, and it is the number of entries actually printed.** It used to lead, next to the
path. A directory larger than one reply block arrives in several pages, so the total is not known
until the walk ends - and the obvious fix, walking once to count and again to print, produces two
answers that can disagree, because the directory may change between them. A header contradicting the
rows beneath it is precisely the wrong answer this format exists to avoid. Counting what was rendered
cannot disagree with itself, and it puts `dir` in line with `find` and `tree`, which have always
summarised at the end.

`a.[2Jb.txt` is a filename holding a raw `ESC [ 2J` - a clear-screen sequence - baked onto the disk
by that suite. It renders with `.` where the control bytes are, so listing it cannot scroll itself
off the screen. That sanitising is why the name column can be trusted, and it is a reason not to
decorate names with markers.

`.gsh_history` differs between the two listings because the shell appended the command in between.
That is the file honestly changing, not the two renderings disagreeing.

### The size column is right-aligned INCLUDING its unit

`8 B` and `1.4 MiB` end at the same column. Aligning the digits and letting the units straggle still
reads as ragged, which is the subtler half of the problem; the blunter half is that a Rust `Display`
implementation **silently ignores the width it is given** unless it calls `f.pad()`, so `{:>9}` did
nothing at all until the renderer was changed to render into a buffer and pad it.

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
The current location is one drive+directory pointer moved by `cd` (`17_cd.md`).

## 4. Implementation

Read-only, so `dir` is a **shell built-in** that sends `ListDir` to `fs` (op 14) over a
narrow `ipc_send=["fs"]` cap and formats the reply. `fs` enforces; the shell's authority is
not widened beyond reading (`0_conventions.md` §2). Two code paths share that one `fs` call:
`cmd_dir` formats the text listing (bare `dir`), and `build_dir_table` parses the same reply into
the `name`/`type`/`size`/`sealed` table for the record pipe (`is_record_producer` routes a piped
`dir` to it before the text producers are consulted).

Both walk the reply through a `DirCursor`, because one reply block carries only about twenty
entries: the request says which entry to resume at and the reply says where to continue, so a
directory of any size is listed in full (`backlog/33`). The count `dir` prints is therefore the
number of entries it actually rendered, and it is printed LAST - the total is not known until the
walk ends, and a header that can disagree with the rows under it is the wrong answer this whole
mechanism exists to remove.

## 5. Later (separate doc so it can grow)

- **`dir extents`** (a *word*, never `-x` - `0_conventions.md` §4): the on-disk placement - first
  block, block count, and whether the file is fragmented (`ITYPE_FILE_FRAG`). Deliberately NOT called
  `dir raw`, which fails the same test `ls` failed: raw *what*? Not built - nothing has asked for it
  outside a test harness, and §26.2 says a feature is pulled into existence rather than anticipated.
- A recursive form is already `tree`, which exists.
  (Sorting and filtering by name/size are **done** - the record pipe's job, §2a, not words on `dir`.)

## 6. Conformance

Conforms: `ls help` (usage with a real example per row) and `ls version` (number +
creator credit) per `0_conventions.md` (the shared `help_block` helper).

Also conforms to **rule 10** (`0_conventions.md` §1.10): the `fs` request is **q-abortable** via
`fs_request_q` - a wait past ~2s prints `(q to quit)` and `q`/`Q`/ESC returns to the prompt (a fast
reply prints nothing). This replaced a bare `request_with_reply`, which rule 10 forbids for an
interactive command.
