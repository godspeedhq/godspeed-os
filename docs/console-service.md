# Design Note: Console Service - separating logs from the interactive console

**Status:** DESIGN (draft for discussion). Not yet implemented.
**Branch:** `feat/console-service` (off `main`).
**Date:** 2026-06-05
**Pins:** §26.10 (kernel = mechanism, not policy), Appendix B.3 (shell = capability-broker holding a console cap), Appendix C.1 / live `observe`.

---

## 1. Problem

There is **one console stream**, and everything dumps to it. The kernel's
`kprintln`, every service's `ctx.log`/`ctx.print`, the xhci driver's progress,
and the shell's prompt all write the same append-only stream that is mirrored to
**both** the serial port and the framebuffer (TV). So the interactive prompt
fights with asynchronous log output: `gsh>` and `xhci: keyboard ready` race for
the bottom of the screen, `observe now` output interleaves with whatever else is
logging, and there is no way for a full-screen view to own the display.

This blocks two things:
- A clean, stable interactive prompt on the TV.
- **Live `observe`** - a full-screen view needs to clear+home and repaint in
  place, read `q` to quit, and *own the screen* so log lines don't smear its
  frame. Impossible while logs share the surface.

The interim fixes already in `main` (the boot-flush yield, the `observe now`
park-wait, the inline prompt) are workarounds for *not having this separation*.

---

## 2. Current state (what exists today)

**Output** - all one path:
```
kprintln! / ctx.log (syscall 5) / ctx.print (syscall 22)
    → kernel log::write_fmt
        → serial_write_byte  (COM1)           ─┐ mirrored, one stream
        → fb::put_byte       (framebuffer/TV)  ─┘ to both surfaces
```

**Input** - kernel-owned ring:
```
USB keyboard → xhci driver → ctx.console_push (syscall 20)
    → console_push_byte → console input ring (+ echo to the console) + wake
shell → ConsoleRead (syscall 17) → reads the ring
```

**`logger`** is a stub: it logs "ready" and parks. All logging actually
short-circuits through the kernel ring buffer to serial+fbcon; nothing goes *to*
the logger service.

---

## 3. The split

The fix is to stop having one stream. There are two distinct kinds of output, and
they want different destinations:

| Kind | Examples | Wants to go to |
|------|----------|----------------|
| **Log** (diagnostic) | kernel `kprintln`, `xhci:` progress, `spawn[...]`, cap-test | the **log stream** (serial + a queryable buffer) |
| **Console** (interactive) | the shell prompt, `observe` output, command results | the **interactive console** (the TV surface) |

And the **interactive console** is owned by a **console service** the shell
brokers (Appendix B.3) - it holds the keyboard + display and gives the shell a
clean surface, separate from the log firehose.

**Division of labour (per §26.10 - kernel is mechanism, console service is policy):**
- **Kernel keeps the *mechanism*:** rendering a glyph to the framebuffer, the
  serial UART, the keyboard input ring. It does not decide layout.
- **Console service owns the *policy*:** the terminal model - where the prompt
  sits, what scrolls, the cursor, foreground ownership for a full-screen app. It
  drives the display through a kernel console-output capability and reads the
  keyboard through a console-input capability; the shell holds a cap to *it*.

The two physical surfaces fall out naturally:
- **Serial = the log/debug stream.** Unchanged for debugging (TeraTerm shows the
  verbose logs). Kernel + service logs go here.
- **Framebuffer/TV = the interactive console**, owned by the console service. The
  kernel stops mirroring log output to the framebuffer; the console service owns
  what the TV shows.

---

## 4. Proposed staging

A full console service is a genuine subsystem (Appendix D calls it far-future).
Stage it so each step is useful on its own.

### Stage 1 - separate the streams (clean TV, no new service yet)
- Split output into **log** vs **console** at the API:
  - `ctx.log` / `kprintln` → **log stream** → serial (+ the kernel ring buffer,
    later drained by the `logger`). **No longer mirrored to the framebuffer.**
  - a **console** output path → the framebuffer (the interactive surface).
- The shell's prompt/results and `observe`'s frames use the console path; all the
  `xhci:`/`spawn[...]`/kernel diagnostics use the log path.
- **Result:** the TV shows a clean interactive session; serial keeps the logs.
  The boot chatter stops smearing the prompt **on the TV** without silencing it
  on serial. This alone fixes the felt problem and needs no new service - just a
  routing split in the kernel console layer.

### Stage 2 - the console service (userspace) + terminal model
- A userspace `console` service owns the interactive surface: a scrolling output
  region plus a **fixed input line** redrawn after any output, the cursor, and a
  **foreground-app API** (take the screen, clear+home, stream keys, release).
- The shell brokers it (holds a cap); `ctx.log`-style console output from the
  shell goes *through* the console service.
- **Unlocks live `observe`:** it asks the console service for the foreground,
  repaints each tick, reads `q`, and releases - with no log lines smearing it.

### Stage 3 - `logger` becomes real (optional, parallel)
- Route `ctx.log` to the `logger` service (today a stub) so logs have a real home
  (a queryable buffer, `osdev logs <svc>`, later a file via `fs`). Independent of
  Stages 1–2; makes the log stream first-class.

---

## 5. Key decisions - RESOLVED (2026-06-05)

All settled per the recommendations below: **(1)** land **Stage 1 first** as its
own mergeable win, then Stage 2; **(2)** console output reaches the framebuffer
via a **kernel render path** (the kernel keeps glyph rendering = mechanism);
**(3)** cursor control via a **minimal ANSI subset** (one escape stream works on
the TV and a serial terminal); **(4)** the **console service owns the keyboard**
(Stage 2); **(5)** log-vs-console routing is **by API** (`ctx.log` = log → serial;
new `ctx.console_*` = console → serial + framebuffer).

### Stage 1 mechanism (this branch, first)
- `serial_write_byte` / `serial_write_bytes_lockfree` → **COM1 only** (drop the
  framebuffer mirror). This makes *all* existing logs - `kprintln` and every
  service's `ctx.log` - serial-only automatically; the TV goes quiet during boot.
- New `console_write_*` (kernel) → COM1 **and** the framebuffer. The keyboard echo
  uses this so typed input still shows on the TV.
- New `ConsoleWrite` syscall (23) + `ctx.console_write`/`console_writeln` - the
  interactive path. Gated by `LOG_WRITE` for now (Stage 2 introduces a dedicated
  console cap held only by the console service).
- The **shell** (prompt + command output) and **observe** (its frame) switch their
  user-facing output to `ctx.console_*`. Everything else keeps `ctx.log` → serial.
- **Result:** TV shows a clean interactive session (shell, echo, observe); serial
  keeps the full logs *and* the session, so debugging/capture is unaffected.

---

## 5a. Stage 2 direction - REVISED (2026-06-05)

Stage 1 changed the premise. The console service's original headline job - keeping
the input line from being smeared by async **log** output - is **already solved**:
Stage 1 stopped mirroring logs to the framebuffer, so the TV only ever shows
console output. The *only* remaining job a console service was for is **foreground
input arbitration** for a full-screen live view (live `observe` reading `q`).

A confirmed kernel constraint then settled the design: there is **one console ring
and one `CONSOLE_READ_WAITER`** (`arch/x86_64/mod.rs`) - exactly one task reads the
keyboard at a time, and there is no `select()`. A separate always-on console
service that *owns* the keyboard would block in `ConsoleRead` and could not also
field out-of-band "take/release the screen" messages without either busy-polling
(throwing away the idle-halt work) or reworking the hardware-verified USB keystroke
path into IPC messages. The kernel's single-waiter slot **is** the foreground-input
ownership primitive already.

**Decision:** build the live-view seam now, **shell-brokered**, not as a separate
service. The reusable foundation is the *utility-facing* contract - "become the
console reader, paint via `console_write` + ANSI, poll `q`, release on exit" - which
is **identical** whether the shell or a future console service brokers it. The shell
is already the Appendix B.3 capability-broker and fits the kernel's waiter model.
A dedicated console service is deferred until a real multi-consumer need pulls it
into existence (a real `logger` consuming the log stream, multiple terminals,
multiple foreground apps) - at which point it takes over *brokering* with **zero
changes to the utilities** (§26.2: features pulled into existence; nothing built
speculatively, nothing wasted).

This supersedes decision **(4)** below ("console service owns input"): input stays
shell-brokered for now. Decisions (1) Stage 1 first, (2) kernel render path, (3)
ANSI subset, and (5) routing by API stand.

**Stage 2 as built:**
- **2a (mechanism):** minimal ANSI subset in the fbcon - clear, cursor position,
  erase line, hide/show cursor - plus `InspectKernel` query 9 for screen geometry.
- **2c (live `observe`):** a non-blocking `TryConsoleRead` (24) and a `ConsoleEcho`
  (25) echo toggle, both gated by `CONSOLE_READ`; `observe` gains a `MODE_LIVE_FG`
  that hides the cursor, suppresses echo, repaints in place (home + `ESC[K` per
  line, no full-clear flicker) every ~0.5 s, and polls `q`; the **shell** spawns
  `observe-live`, stops reading the keyboard while it runs, and resumes when it
  parks (the foreground handoff). The first client of the seam validates it.
- (There is no "2b separate console service" - folded away by the decision above.)

---

## 5b. Original options (for reference)

1. **Scope for this branch.** Stage 1 only (separate streams → clean TV, small,
   high-impact), or push through Stage 2 (the console service + live observe) in
   the same branch? *Recommendation: land Stage 1 first as its own mergeable win,
   then Stage 2.*

2. **How does "console" output reach the framebuffer?** The kernel owns the
   framebuffer (arch layer), so a userspace service can't write it directly.
   Options:
   - (a) **Kernel render API / console-output cap** - the console path is a
     syscall the kernel renders to the framebuffer (kernel keeps glyph rendering =
     mechanism; the service controls layout). *Recommended - matches §26.10.*
   - (b) **Map the framebuffer to the console service** (like the xhci BAR) - the
     service renders glyphs itself. More control, but duplicates the font renderer
     and is serial-blind.

3. **Cursor control: ANSI escapes vs positioned-write syscalls.** For the console
   service to manage a terminal (clear, home, move cursor), either the kernel
   fbcon interprets a **minimal ANSI subset** (and a serial terminal understands
   the same escapes for free), or the kernel exposes **positioned-write**
   primitives (explicit, but serial-blind). *Lean: ANSI subset - one escape stream
   works on both the TV and a serial terminal.*

4. **Keyboard ownership.** Does the console service own keyboard input (the shell
   asks it for lines), or does the shell keep reading `ConsoleRead` directly and
   use the console service only for output? *Lean: console service owns input too,
   so it can do line editing and route keys to a foreground app (observe's `q`).*

5. **Log routing granularity.** Stage 1 needs to mark output as log-vs-console.
   By **API** (`ctx.log` = log, a new `ctx.console_*` = console - simple, explicit)
   or by a **level/tag**? *Lean: by API.*

---

## 6. Out of scope (far-future, not this work)

Multiple virtual terminals, a real VT100/xterm emulator, scrollback paging, copy/
paste, resize, colour themes beyond the current green-on-black. The goal here is a
*clean, stable interactive console with foreground-app support*, not a terminal
emulator.

---

## 7. First step once a direction is agreed

Stage 1: in the kernel console layer, split the framebuffer mirror off the log
path and give the shell/observe a console-output path to the framebuffer; verify
the TV shows a clean session while serial keeps the full logs (shell-test + a
framebuffer screendump). Then Stage 2.
