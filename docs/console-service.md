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
  Stages 1-2; makes the log stream first-class.

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

---

## 8. Input under a starved timer ISR - drain-on-read (2026-07-09)

> **Built 2026-07-09.** Not a design question - a robustness fix to the *input mechanism* of §2,
> surfaced by the same T630 storm that produced `docs/persistence.md` §6.16 (a rapid double
> `kill all-services`). No new syscall, no format change, no amendment.

The keyboard/serial input path of §2 depends on the **timer ISR** to poll the UART RX line and push
bytes into the console ring. Under a storm - the timer starved by long `IF=0` console writes and
IPI/shootdown pressure - that ISR fires late or not at all, so typed bytes pile up in the UART's 16-byte
RX FIFO and never reach the ring. The console *looks* dead even though the hardware already holds the
input.

**Fix: drain on read, not only on tick.** The console read syscalls (`ConsoleRead` / `TryConsoleRead`)
now call `uart_rx_drain_now` **before** consulting the ring: with interrupts disabled (`cli`), drain the
UART RX FIFO into the console ring, then restore the prior IF. A task blocked in `ConsoleRead` therefore
pulls its own input straight off the hardware the instant it asks for it, independent of whether the
timer ISR is keeping up. The tick-driven poll stays (it is what *wakes* a blocked reader), but
correctness no longer **hinges** on it. This is the input-side echo of the storage stack's Commandment
VIII fix (§6.16): don't wait on a proxy (the tick) for a truth (a keystroke) you can observe directly.
The drain is bounded (16-byte FIFO into a fixed ring) and IF-guarded (it never re-enters and never leaves
interrupts unexpectedly enabled); no new `unsafe` beyond the existing `arch` UART accessors.

**Companion: lazy, bounded shell history.** The shell persists its command history to `/.gsh_history`
(the §15 monotonic-counter pattern). It used to **read that file back at startup** - an `fs` round-trip
fired during the shell's own re-init, which is exactly when the storage stack may be mid-recovery (a
contributing masked race in the §6.16 wedge). The read is now **lazy**: the shell loads `/.gsh_history`
on the **first up-arrow**, not at spawn. A fresh prompt never blocks on `fs` coming up, and the shell's
re-init path touches no disk. In-memory history stays bounded (a fixed ring); saving remains best-effort
(§15). Two small changes, one theme: the interactive path must stay responsive precisely when the rest of
the system is busy recovering.

---

## 9. Stage 4 - the console service is real, and `fbcon` leaves the kernel (2026-08-17)

> **Status:** BUILT and HARDWARE-VERIFIED on `feat/pi2-arm32-hardening` (Pi 2, 2026-08-17: chaos 50 rounds, 0 kernel panics, 0 liveness wedges, selfcheck 350/0, 0 console writes lost). This section supersedes decision **(2)(a)**
> of §5 ("kernel render path") and decision **(4)** of §5a ("input stays shell-brokered" - still true;
> only OUTPUT moves). Driven by `scripts/commandments.py`, for which `kernel/src/fbcon` was the last
> standing Commandment I violation: 1,172 lines of terminal emulation that can claim none of the
> kernel's six responsibilities (§4.3) and no sanctioned support role.

### 9.1 Why (2)(a) was wrong

§5 chose "the kernel keeps glyph rendering = mechanism, the service controls layout", reasoning from
§26.10. That reading does not survive contact with §4.4: a font rasteriser, an ANSI/CSI state machine, a
UTF-8 decoder, a shadow grid and a scroll strategy are a **display driver**, and §4.4 forbids drivers in
the kernel by name. The measured boundary (`docs/commandment-audit.md`, 2026-08-14) also showed there is
no cheap partial slice - the emulator is reached THROUGH `put_byte`, so removing CSI handling alone
leaves the shell's cursor moves and colours going nowhere. The service must take rendering **entirely**.

The recorded user position settled it: *"fbcon is not special, only the kernel is special; I really don't
want to continue adding exceptions to the kernel."* So option **(b)** of §5b - map the framebuffer to the
service, as the xHCI BAR is mapped to `xhci` - is what gets built. Its stated drawback ("duplicates the
font renderer and is serial-blind") is answered below.

### 9.2 The split

| | Kernel (`bootcon/`, ~330 lines) | `console` service (~1,100 lines) |
|---|---|---|
| Serial | owns it, unchanged - **still the source of truth** | never touches it |
| Framebuffer | a minimal boot/panic blit | the whole terminal |
| Text model | none (no grid, no cursor, no scrollback) | ANSI/CSI, UTF-8, shadow grid, cursor, scroll, reverse video |
| Geometry | private to its own blit, never published | the single source of truth for rows/cols |

**Not serial-blind.** `ConsoleWrite` (syscall 23) still writes serial synchronously, exactly as today, so
a captured `build/serial_output.log` is unchanged and the interactive session still appears in it. What
the syscall stops doing is *rendering*; it enqueues the same bytes to the service's ordinary IPC endpoint
(no new ring - the bounded queue every service already has). Serial is truth, the display is a mirror -
which is already the stated ARM policy (`mirror()`).

**A full queue blocks the WRITER.** The writing task is parked as a blocked sender until the terminal
drains (§8.5, §8.6) - the kernel itself never waits. This is a correction to the first draft of §9.2,
which dropped instead: see §9.8, item 4, for why dropping could not work at any rendering speed.

**Not a duplicated font renderer.** The kernel's blit is deliberately cruder than the terminal's: no box
glyphs, no reverse video, no shadow grid, and a "scroll" that clears and starts at the top. It is a floor,
not a small terminal, and the two are not two copies of one thing.

### 9.3 What earns the kernel's remaining blit

**Impossibility, which is the only thing that does** (`docs/commandment-audit.md`: the bar the control
channel failed and the supervisor spawn clears). A panic halts every core, including the console service,
so **a panic cannot ask a service to report it**. On a Pi wired to a TV with no serial cable, a kernel
with no blit dies with a frozen screen and no reason on it - the silent failure invariant 12 exists to
forbid. Boot output has the same shape: it precedes every service, including the one that would render it.

Both are the §11.4 ring-buffer argument applied to a machine with no serial port, so `bootcon` claims the
same `kernel-log-floor` role and nothing wider. §11.4 is amended to say so, because it currently reads
"serial console", which is **x86-shaped** - on a PC serial is always there.

### 9.4 Ownership of the framebuffer is explicit and one-way

Two writers to one framebuffer is not a race the service can defend against: its shadow grid would be
silently wrong about what is on screen. So ownership is a state, not a convention:

```
kernel owns it  --(service has mapped + cleared: bootcon::release)-->  service owns it
       ^                                                                      |
       +-------------------- bootcon::reclaim_for_panic ----------------------+
```

The service calls `release` only *after* it can draw, so there is no window with no writer. The panic path
reclaims unconditionally and clears, because by then the service is halted and its grid describes nothing.

### 9.5 Memory attributes - the constraint that shapes the mapping

The service maps the *same physical pages* the kernel mapped. ARM leaves **mismatched memory attributes**
for one physical page UNPREDICTABLE, so both mappings must agree. They agree on **Normal non-cacheable**:

- The service cannot do cache maintenance - `unsafe` is forbidden in services (§18.2) and `DCCMVAC` is
  PL1-only on ARMv7 anyway. So a cacheable mapping is not available to it at any price.
- Therefore the kernel's mapping becomes non-cacheable too (`section_fb`), and `fb_commit` (a
  clean-to-PoC on ARM, an `sfence` on x86) collapses to `fb_barrier` - store ordering only, nothing to
  clean. `FB_READBACK_CHEAP` disappears with it: reading back a non-cacheable framebuffer is never cheap,
  so the terminal always repaints from its shadow grid and `scroll_by_copy` is deleted.

Non-cacheable here means **Normal** non-cacheable, not **Device**. The distinction matters and the
neutral `PageFlags::PCD` currently encodes the wrong one on ARM: Device semantics (no gathering, no
reordering, no speculation) exist to protect stores with side effects, and a framebuffer store has none
- it is memory the display happens to scan. Non-gathering would make every 32-bit pixel store its own
bus transaction, roughly 1.4M of them per full-screen repaint on a Pi 2. So the ARM page-table encoder
gains a Normal-non-cacheable case and `mmu::section_fb` matches it; `PCD` keeps meaning Device for the
driver MMIO grants that genuinely need it.

Cost: the kernel's boot text is slower to paint. It is boot text, and the terminal's own scroll was
already repaint-from-shadow on x86 for the same reason.

### 9.6 Surfaces added and removed

| | |
|---|---|
| **Added** | `ConsoleDrain` syscall (the service reads the console byte stream); a `CONSOLE_RENDER` authority gating it; a framebuffer grant in the spawn path; one `service_config` row |
| **Removed** | `InspectKernel` query 9 (`dims_packed`) - the shell asked the KERNEL for terminal geometry, which is a service's question; `fb_commit` / `FB_READBACK_CHEAP` from the arch contract; ~840 lines of ring-0 code |

The syscall pin grows by one and the introspection pin shrinks by one. That is the trade the audit
predicted and it is deliberate: a byte read is mechanism, terminal geometry is policy.

### 9.7 Geometry has ONE owner

`ctx.console_dims()` no longer reads query 9. The shell asks the `console` service, which is where the
safe-area inset, the cell size and the font-scale rule live. The kernel's `bootcon` computes rows/cols for
its own blit and **never publishes them** - a private working value is not a second source of truth, but a
published one would be (Commandment III). A shell that cannot reach the console service reports the
console unavailable rather than guessing a size.


### 9.8 What the hardware found that review did not

Five defects, none of which a checker or a build could have caught, listed because each is a different
KIND of miss:

1. **A placeholder ELF.** `arm_build.py` kept its own copy of the ARM service list, so the console
   shipped as a stub and failed to spawn. The second list is now DERIVED from `kernel/build.rs`, not
   reconciled against it - the third time that shape has bitten this repo.
2. **The wrong green.** `grant()` reconstructed the framebuffer's channel shifts from `blend_lut[255]`
   rather than storing them, and the foreground `(0x80, 0xFF, 0x80)` has red equal to blue - so blue
   always resolved to red's shift. Ambiguous by construction, for any palette with a repeated component.
   §26.13: discipline over cleverness.
3. **A handover window.** Ownership moved on the first byte DELIVERED, which on a quiet boot is seconds
   after the service clears the screen. Both parties drew in between. The GRANT is the handover.
4. **Drops that no amount of speed could fix.** A 16-deep queue cannot absorb a thirty-write burst
   however fast the renderer is, so `observe now` lost its tail every time. The writer now blocks
   (§8.5/§8.6 back-pressure); the kernel still never waits.
5. **`reclaim_for_panic` had zero callers.** The amendment in §9.3 argues this module earns ring-0
   residency because a panic cannot ask a service to report it - and the panic handler did not call it,
   so the first real panic showed a black screen and said nothing. **A guarantee asserted in the
   constitution and absent from the code is worse than an unimplemented feature: the document says it is
   covered, so nobody looks.** There is no test for it (§22 explains why - it is a negative property on
   a machine that cannot be failed on demand in QEMU), which is exactly why it needed reading rather
   than assuming.
