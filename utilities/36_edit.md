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
A directory is refused loudly; a file of **any size** opens (§5).

## 3. The screen

```
 edit  /notes.txt  * (modified)                                     ← title bar (name + dirty mark)
shopping list                                                       ← the text area
- milk
- eggs
_                                                                   ← the editing cursor

 ^S save   ^Q quit      Col 1   23 bytes   (buf 12/32768)           ← status bar (hints + position)
```

The title and status bars are drawn in reverse video on a serial terminal and as plain text on
the framebuffer console (which has no colour) — readable on both. The status bar shows the two
essential keys, the live column, the document size, and the **edit-buffer fill** (`buf N/32768`)
— how much you've typed since the last save (§5). When that buffer fills it flips to a loud
`edit buffer full — save (^S) to continue` prompt. There is no absolute line number: that would
require scanning the file from the top on every keystroke, which is exactly the O(file) cost the
windowed design exists to avoid.

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

## 5. Files of any size — a bounded piece table (honest)

`edit` opens a file of **any size**. It never loads the whole file into memory. The model is a
**bounded piece table**, the no-heap (§26.6) realisation of "scroll a huge file the way iOS scrolls
millions of rows":

- **The original file stays on disk.** It is read in fixed `IO_CHUNK` (3556-byte) **windows** as you
  scroll — only the visible window is ever materialised, so opening a 1 MiB file is as cheap as
  opening a 1 KiB one.
- **Edits never touch the original.** Typed bytes go into a fixed in-memory **add buffer**, and the
  document is an ordered list of **spans** (pieces) into either the original file or the add buffer.
  Inserting or deleting rewrites that span list — a few bytes — not the file.
- **Save streams the spans out** to a temporary file and atomically replaces the original (write
  temp → delete target → move temp into place), then **resets** the add buffer and span list. The
  saved file becomes the new original and editing continues.

So the only thing that is bounded is **how much you edit between saves**, not the file size. The add
buffer holds **32 KiB** of new text and the span list holds 1024 pieces; if either fills, the next
edit is **refused loudly** — the status bar shows `edit buffer full — save (^S) to continue` — and a
save empties both. Nothing is ever silently dropped or truncated, which is the failure the
constitution forbids (§3.12, §26.7). All of this state is fixed-size stack arrays: no heap, bounded,
loud on overflow.

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
end-to-end by `osdev test edit` — **15 checks, QEMU-validated**:

- Small file: open → type/backspace/newline → ^S save → ^Q quit → `read`-back; edit-existing
  insert-at-start; quit-with-discard at the unsaved-changes prompt; no-arg usage.
- **Large file** (a pre-baked ~16 KiB / multi-window `/big.txt`): open it windowed, insert at offset
  0, save; re-open, PageDown past the first window, insert a mid-file marker, save. The harness then
  reads the bytes **back off the disk** and asserts the start edit, the mid-file edit, and the far
  original tail all survived the streaming save — proving the windowed load + multi-chunk save path
  end to end.

See `0_conventions.md` §3.
