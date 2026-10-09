# Utility: `clear`

**Utility:** `clear` - clear the screen
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`clear` wipes the console and returns the cursor to the top, then the shell
reprints `gsh> ` beneath. For tidying the screen after scrolling output.

## 2. Invocation

| Command | Meaning |
|---|---|
| `clear` | Clear the screen and home the cursor. |

## 3. Behaviour

Emits the ANSI sequence `ESC[2J` (erase display) + `ESC[H` (cursor home). The
`console` service honours both, and so does a serial terminal, so both output
surfaces clear. The shell loop reprints the prompt afterward.

## 4. Capabilities

- **Console output** only.

## 5. Non-goals

- **No alternate-screen restore.** The console has no alternate-screen buffer;
  `clear` erases in place. (This is the same reason `observe` leaves its final
  frame on screen rather than restoring the prior screen - `1_observe.md` §5.2.)
  The console's scrollback (`54_scrollback.md`) captures a line only as it scrolls
  off the top, so `clear` neither empties it nor adds the lines it erases to it.

## 6. Conformance

Conforms: own `clear help` / `clear version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Console**. See `0_conventions.md` §3.
