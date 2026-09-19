# Utility: `scrollback` - read back what has scrolled off the screen

**Status:** **Built + QEMU-verified** (`osdev test shell`). A shell built-in, full-screen.
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

The console keeps the lines that scroll off the top. `scrollback` shows them.

```
scrollback 0.4.0 - what has scrolled off the screen

  fs: journal recovered 4 block(s)
  drives check - 0 bad, nothing repaired
  gsh> churn verify
  churn: 0 empty, NONE torn

[ 340-372 of 512, older lines aged out ]  [arrows] line  [PgUp/PgDn] page  [Home/End] ends  [q] quit
```

## 2. How it opens

| how | opens at |
|---|---|
| **PgUp** at the prompt | one page back |
| `scrollback` | the newest line |
| **PgDn** at the prompt | nothing happens |

PgUp opens it **one page back** because the key already meant "go back a page". Making you press
it twice to see one page is the interface forgetting what it was just told.

PgDn at a live prompt is inert on purpose. You are already at the bottom; opening a viewer that is
already scrolled to the end is a no-op dressed as an action.

Inside, every key is the one it is everywhere else (`0_conventions.md` rule 10a): arrows move a
line, PgUp/PgDn a page, Home/End the ends, `q` or Esc leaves. Leaving restores the prompt and the
half-typed line, cursor where you left it.

## 3. The history is BOUNDED, and the status line says so

32 KiB or 512 lines, whichever runs out first. When anything has been evicted the status line reads
`older lines aged out`.

That is not a detail. A view that begins in the middle of a session while presenting itself as the
beginning is the same class of wrong answer as a truncated directory listing (`CLAUDE.md` §26.7).
The console's ring has always counted what it dropped precisely so a reader could be told.

## 4. Why it is a utility and not a mode

It **was** a mode, and the mode is what broke on real hardware.

The shell used to drive the console's own view: one blocking request per keypress, each asking the
console to move its view and report back. To answer, the console had to `paint_view` and `present`
**before it could reply** - a full repaint of the framebuffer, inside the caller's deadline. On the
Dell Wyse that framebuffer is 3840x2160, the console is contracted to core 0, and the shell was
round-robined onto core 0 too. So the shell blocked for one second on the core that was painting,
then declared a console that was merely busy to be dead (`backlog/37`).

Holding PgUp issued one of those per key repeat. No deadline makes that shape robust.

Now the console is only ever asked for **bytes** - a bounded copy out of its ring, no painting - and
this utility paints its own screen with ordinary output, which is a send and carries no deadline at
all. A keypress costs the console no repaint whatsoever.

It also takes the view offset **out** of the console. That was a second place holding a derived view
of where the operator is looking, which is what §26.4 is about, and it was exactly the state that
could disagree with the shell's idea of it.

## 5. In a script it prints

`depth > 0` - a script, `run`, `assert` or `selfcheck` - dumps the retained lines and returns. A
full-screen browser waiting for a keypress with nobody there does not degrade, it **hangs the run**.
Same guard `help`, `docs` and `paginate` carry.

## 6. Testing it: nothing it prints can be a marker

Worth recording, because it caught the suite three separate ways in one afternoon.

A scrollback viewer **displays everything the terminal has ever shown**. So any string a frame
contains may also be sitting in the history that frame is rendering:

- `[q] quit` matched a pager's chrome that had scrolled into the ring;
- `" of "` matched ordinary history text;
- `gsh>` matched a prompt inside the replayed dump.

Each time the collect stopped mid-stream and the *following* case reported the failure, which is how
`Home is the line editor` got blamed for a bug it had no part in.

Two kinds of marker are safe. An **escape sequence**, because the ring stores the rendered grid
(`sb.push(&s.grid[0][..cols])`) and escapes are consumed by the terminal, never stored - the frame
cases wait on `ESC[J`. Or a string introduced **after** the output under test, which is what the
dump case does.

There is a second lesson underneath: the old mode's tests passed through the entire bug, because
what broke was a repaint cost on a 4K panel and QEMU has no such panel. Asking for bytes is
deterministic, so this path is genuinely covered rather than merely green.

## 7. Implementation

`cmd_scrollback` in `services/shell/src/main.rs`, over `ServiceContext::console_history`, which is
opcode `REQ_HISTORY` in `services/console/src/main.rs` and `Term::history_into` in
`services/console/src/term.rs`. Frames go through the same `FrameBuf` and `LineBuf` that `paginate`
and `help` use, so a repaint is a dozen console messages rather than two per row.

Lines are clipped to the screen width, never wrapped: a wrapped line pushes every row below it down
and makes the count in the status line a lie about what is on the screen.
