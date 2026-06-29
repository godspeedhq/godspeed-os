# Milestone - Shell + Utilities ✅

**Status:** ✅ Built + hardware-proven on the HP T630 (AMD GX-420GI, real AHCI SSD) - the shell
`selfcheck` suite reported **`ran 163, failed 0`** on bare metal (CLAUDE.md §23.3).
**Target hardware:** HP T630 thin client, 4-core ~2 GHz, 8 GB RAM; console over COM1 (115200 8N1)
and over the framebuffer + USB keyboard.

---

## Scope

An interactive shell and a userspace utility ecosystem built on the capability model - not a
port of a Unix shell. The shell is a **capability-broker service** (CLAUDE.md Appendix B.3, §2181):
there is no `fork`/`exec`, no inherited file descriptors, no ambient `stdin`/`stdout`. It holds a
console capability and the authority to ask the supervisor to spawn services, and it constructs each
command's authority explicitly. The result *looks* familiar (`ls /data | match .txt`) while the
mechanics underneath are capability delegation, not POSIX plumbing.

This milestone is the human-facing surface of everything below it: 38 built-in utilities, a
windowed text editor, a `.gsh` scripting + self-test ladder, and capability-mediated pipes.

---

## Achievements

- ✅ **Interactive shell** with a `gsh>` prompt, driven over COM1 *and* over the framebuffer console
  + USB keyboard on real hardware. A capability-broker, not a Unix shell (Appendix B.3): commands
  spawn through the supervisor, never `fork`/`exec`.
- ✅ **Capability-mediated pipes** (`A | B`, `docs/pipes.md`). A pipe is a fresh IPC endpoint with a
  `SEND` cap granted to the producer and a `RECV` cap to the consumer - not fd inheritance.
  Display commands pipe into `write`; `write append|prepend`; when a stage cannot be a direct pipe
  source it is materialized through a file first (`read <file> | …`).
- ✅ **`observe`** - a native top/htop-style live full-screen view: per-service
  `TASK · NAME · CORE · STATE · MEM · RESTARTS · QUEUE`, refreshed in place. Possible because
  per-service state is already structured (caps, core, memory, generation, liveness); no `/proc`
  text to parse. Console log/output split so the live view and log stream don't collide.
- ✅ **`edit`** - a full-screen MS-Edit-style editor backed by a **bounded piece table**, so it
  opens a file of **any size** with no heap (§26.6): the original stays on disk and is read in
  `IO_CHUNK` windows on demand; typed bytes accumulate in a fixed 32 KiB add-buffer; the document is
  a 1024-piece span list (loud-when-full); save streams the spans out and atomically replaces.
  `osdev test edit` (15 checks, incl. a multi-window `/big.txt` opened windowed, edited at start +
  mid-file, saved, and verified off-disk).
- ✅ **Scripting + self-test ladder** - a `gsh` script language (`.gsh`), the **records** subsystem,
  and `run`/`assert`/`result`. Scripts run host-baked from a GSFS disk (`osdev script-disk` →
  `dd` → `run /suite.gsh`) or embedded in-memory. A comprehensive in-memory **`selfcheck`** suite
  (~159 commands) exercises the whole surface; `osdev test script` runs both the baked `smoke.gsh`
  (with a piped assert) and `selfcheck`, each asserting `ran N, failed 0`.
- ✅ **`date` / `uptime`** from a real **MC146818 CMOS RTC** (`arch/x86_64/rtc.rs`): wall-clock
  `date` (+ `date epoch`) and a glitch-free per-service uptime.
- ✅ **`drives` multi-drive model** + the full file command set over the GSFS filesystem
  (read/write/ls/cd/mkdir/copy/move/rename/delete/find/tree).
- ✅ **Hardware-proven end to end** - the T630 `selfcheck` run is `ran 163, failed 0` on a real
  AHCI SSD, covering persistence, file-as-capability (`fcap`), and the utility surface, no panic.

---

## Utilities (38 built-ins; full reference in `utilities/`)

| Group | Commands |
|-------|----------|
| Introspection | `observe` · `date` · `mem` · `cores` · `about` · `caps` · `status` · `uptime` |
| Console | `echo` · `clear` |
| Service control | `spawn` · `kill` · `restart` · `reboot` · `poweroff` |
| Storage / files | `drives` · `ls` · `cd` · `read` · `write` · `mkdir` · `copy` · `move` · `rename` · `delete` · `find` · `tree` |
| Data / text | `match` · `count` · `sort` · `first`/`last` · `records` · `result` |
| Scripting | `run` · `assert` |
| Capability | `fcap` (open a file as a real kernel capability - §7.10) |
| Editor | `edit` (bounded piece table, any file size) |
| Chaos | `chaos` (the userspace fault-injection driver - see the chaos milestone) |

Each command has a reference doc at `utilities/<n>_<name>.md`; shell conventions in
`utilities/0_conventions.md`.

---

## Why this fits the constitution

- **No ambient authority (§3.1).** The shell grants each command exactly the caps it needs; there is
  no inherited environment or ambient `stdout`. `rm -rf /` cannot exist unless the shell explicitly
  hands a `WRITE` cap to `/` (Appendix D.4).
- **No heap (§26.6).** `edit`'s piece table and the records/scripting buffers are fixed stack
  arenas with loud-when-full ceilings - the "scroll a huge file like iOS" property realized without
  an allocator.
- **The model is the product (§26.1).** A pipe is a capability-mediated endpoint, a file handed to
  `fcap` is a real kernel capability - familiar syntax over an honest capability substrate.

---

## Evidence / tests

- `osdev test shell` - scripted smoke test (boot, help, cores, status, unknown, chaos).
- `osdev test edit` - 15 checks (small-file + large-file windowed editing, saved bytes verified off-disk).
- `osdev test script` - `smoke.gsh` (baked, piped assert) + in-memory `selfcheck`, both `ran N, failed 0`.
- Hardware (T630): shell `selfcheck` `ran 163, failed 0` on a real AHCI SSD (CLAUDE.md §23.3).
