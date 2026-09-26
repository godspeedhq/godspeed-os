# Utility: `paginate` - read long output a screenful at a time

**Status:** **Built + QEMU-verified** (`osdev test files`). A pipe-only sink in the shell.
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`<producer> | paginate` shows a long stream one screenful at a time, with keys to scroll.

```
paginate 0.4.0 - read long output a screenful at a time

usage:
  <producer> | paginate    page the piped stream (text or records)
  paginate version         print the version
  paginate help            print this message

keys:
  up / down                scroll one line
  PgUp / PgDn, space       move a page
  Home / End               jump to the top / the end
  q                        quit back to the prompt
```

It takes no arguments and must be the **last** stage: it reads the stream, it does not pass
it on. `dir | paginate | write /f.txt` is refused rather than quietly doing one of the two
things it might have meant.

## 2. Why it is a stage and not a mode

The obvious design is to build paging into the commands that produce a lot of output -
`dir`, `read`, `find`, `help`. That design has a defect that only shows up later, and it is
not a small one: **a command that pages itself has to guess whether a human is watching, and
the guess is wrong exactly when it matters.**

- `dir /big | write /list.txt` has a reader, but the reader is the shell.
- `run /nightly.gsh` has no reader at all, which is often why it was started.
- `selfcheck` runs 357 cases and nobody is pressing a key for any of them.

A built-in pager must therefore carry a guard at every call site, and the failure mode when
a guard is missed is not a wrong character on screen - it is a run that **hangs forever**
waiting for a keystroke nobody will type.

Asking for it cannot make that mistake. A pipe that captures does not contain the word
`paginate`, so there is nothing to guard. And because it is a stage rather than a feature,
**every producer gets it at once** - `find`, `read`, `status`, `caps`, `events log`, and
whatever is added next - instead of a pager wired separately into each, which is the same
fact stated six times waiting to disagree with itself (`backlog/35`).

It also composes, which a built-in pager never could:

```
dir /projects | where type=file | sort size | paginate
```

## 3. It still refuses to hang

`paginate` is safe to leave in a script. Two conditions must **both** hold before it waits
for a key:

- the shell is **not** inside a script, `run`, `assert` or `selfcheck` (depth 0), and
- the output is going to the **console** - not `$( )` capture, not `save <path>`, not a
  captured function body.

When either fails the stream is simply printed, exactly as an unsinked pipe prints it. That
is a no-op rather than an error on purpose: a pipeline copied out of an interactive session
into a script should not start failing because of a word that only ever concerned a screen.

## 4. Records keep their column header

A record stream (`dir`, `status`, `caps`, `find`, `trace deps`) pages with its **column
header pinned** - repainted at the top of every frame, never scrolled.

This is the one thing a pager does that the console's scrollback structurally cannot. Scroll
back through a grid in a scrollback buffer and the column names are off the top, leaving
columns you have to count. `paginate` keeps them in view at every position.

```
gsh> dir /many | paginate
name        type  size  sealed
f0.txt      file     1  false
f1.txt      file     1  false
...
[ lines 1-22 of 45 ] [up/down] scroll [PgUp/PgDn] page [Home/End] ends [q] quit
```

## 5. What it does to the content, and why

A pager **owns the whole screen** and repaints it by homing the cursor, so it cannot let the
content drive the terminal. Two things are filtered, both for the same reason `dir` sanitises
filenames - the view that is supposed to reveal the content must not be something the content
controls:

- **Control bytes render as `.`** A single `ESC [ 2J` inside a file would clear the frame
  mid-paint, scrolling itself and everything after it out of the view meant to show it. A tab
  becomes one space; expanding it properly needs a column model this does not have.
- **An over-long line is cut at the screen width**, with a trailing `>` to say so. A line that
  wrapped would silently push every following row down, making the row count the pager just
  computed wrong - and the status line would then be lying about which lines you are looking at.

`read` on its own is unchanged. It does not own the screen, so it has no reason to filter.

## 6. Relationship to the console's scrollback

They are complements, not alternatives.

| | scrollback | `paginate` |
|---|---|---|
| when you decide | after the output has gone past | before you run the command |
| what it needs | nothing | the word in the pipeline |
| covers | every command, including ones that already ran | the pipeline you asked about |
| column headers | scroll away with everything else | stay pinned |
| bound | the ring, oldest lines age out | the stream, whole |

Scrollback is the safety net for output you did not expect to be long. `paginate` is what you
reach for when you already know it will be.

Between them they retired `help`'s built-in pager (2026-09-18, once scrollback was verified on a
board). `trace` keeps its own, and the pinned column header in the row above is exactly why.

## 7. Implementation

`paginate_sink` in `services/shell/src/main.rs`, dispatched from `pipe_run` alongside the
other two sinks (`write`, `assert`). Bytes go through `paginate_bytes`, records through
`paginate_table`; both drive `line_pager`, the single screenful-at-a-time reader the shell has
had since `help` needed one. `trace` routes through `paginate_table` too, so there is one table
pager rather than two copies of it.

**No line index array.** `Lines` walks the buffer with a one-entry cached cursor instead. An
index over a 16 KiB buffer of one-byte lines would want 16,384 offsets, and `pipe_run`'s frame
already sits at 68% of the user stack - §26.6.1 says the move is to change the representation,
not to find room for a big one, and two words beat sixteen thousand.

**One frame, a dozen syscalls.** Every line of a repaint goes into a shared 256-byte `FrameBuf`
and out in batched writes. Writing each row straight to the console costs two syscalls per row,
which against a 16-deep console queue meant holding a scroll key outran the sink and the
keyboard stopped responding.
