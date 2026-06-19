# Utility: `edit`

**Utility:** `edit` — full-screen text editor
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`edit` answers **how do I change a text file in place?** — it is a full-screen, modeless text
editor for GodspeedOS, modelled after Microsoft's `edit`: a title bar on top, the text area in
the middle, and a key-hint/status bar pinned to the bottom. You open a file, move around, type,
and save — no separate "insert mode", no commands to learn beyond the two on the status bar.

It complements `read` (print a file) and `write` (create/overwrite from the command line):
`edit` is for *interactive* changes — fix a line in a script, jot notes, tweak a config.

## 2. Invocation

| Command | Meaning |
|---|---|
| `edit <path>` | Open `<path>` for editing. A missing file starts empty and is created on first save. |
| `edit help` | Print usage. |
| `edit version` | Print the version (uniform across utilities). |

```
gsh> edit /notes.txt
```

`<path>` resolves like every other file command (absolute, or relative to the current `cd`).
A directory, or a file larger than the editor's buffer, is refused loudly (§5) — never truncated.

## 3. The screen

```
 edit  /notes.txt  * (modified)                                     ← title bar (name + dirty mark)
shopping list                                                       ← the text area
- milk
- eggs
_                                                                   ← the editing cursor

 ^S save   ^Q quit      Ln 4, Col 1   23/3556 bytes                 ← status bar (hints + position)
```

The title and status bars are drawn in reverse video on a serial terminal and as plain text on
the framebuffer console (which has no colour) — readable on both. The status bar always shows the
two essential keys and the live cursor position + buffer fill.

## 4. Keys

| Key | Action |
|---|---|
| printable characters | insert at the cursor |
| **Enter** | split the line (insert a newline) |
| **Backspace** | delete the character before the cursor |
| **Delete** | delete the character at the cursor |
| **Tab** | insert spaces (a fixed soft tab) |
| **←  →** | move by one character |
| **↑  ↓** | move by one line (keeps the column where it can) |
| **Home / End** | jump to start / end of the line |
| **PageUp / PageDown** | move up / down a screen |
| **Ctrl-S** | save to the file (creating it if new) |
| **Ctrl-Q** / **Esc** | quit — with unsaved changes, a prompt offers *save*, *discard*, or *keep editing* |

The view scrolls vertically and horizontally to keep the cursor visible; there are no modes.

## 5. The size limit (honest)

The text lives in **one fixed stack buffer** — services hold no heap by default (§26.6) — sized
to one filesystem message (**3556 bytes**, the file-transfer ceiling, `IO_CHUNK`). So:

- A file up to 3556 bytes is editable.
- A larger file is **refused** with its size (`edit: … too large to edit …`), never opened and
  silently clipped — losing data quietly is the failure the constitution forbids (§3.12, §26.7).

This matches the system's "a small file is one IPC message" model. Editing larger files would
need a streaming buffer; that is deferred until something needs it (§26.2), not built speculatively.

## 6. Capabilities

`edit` is a shell built-in: it runs in the shell's protection domain and uses caps the shell
**already holds** — its console read/write (the same keyboard + screen the prompt uses) and its
narrow `SEND` cap to `fs` (the same one `read` / `write` use). It gains **no** new authority: it
cannot spawn, kill, or reach any file `fs` would not already serve the shell. `fs` enforces all
disk authority; `edit` only asks it to read one file and write one file.

## 7. Non-goals

- **No POSIX editor heritage.** No `vi`/`nano` modal commands or `:wq`; the two keys you need are
  on the bar (§ `0_conventions.md` rule 8).
- **No mouse, menus, syntax highlighting, undo, or search** — a deliberately small v1 (§26.2);
  these are pulled in only when a real need does.
- **No binary editing.** It edits text; control bytes already in a file may render oddly.

## 8. Conformance

Conforms: own `edit help` (usage with a real example per `0_conventions.md`) and `edit version`
(number + creator credit), listed by the shell's top-level `help` under **Storage**. Exercised
end-to-end by `osdev test edit` (open → type/backspace/newline → ^S save → ^Q quit → `read`-back;
edit-existing insert; quit-with-discard) — 9 checks, QEMU-validated. See `0_conventions.md` §3.
