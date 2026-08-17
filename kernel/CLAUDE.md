# kernel/

The kernel crate. Bare-metal `#![no_std]` binary targeting `x86_64-unknown-none`.

## What lives here

Everything that runs in ring 0. The kernel is the only code that:
- Directly touches hardware (via the arch layer).
- Enforces capability checks.
- Manages physical memory.
- Owns the routing table and IPC queues.
- Issues IPIs.

## What does NOT live here

Filesystem logic, network stack, drivers (beyond minimal arch boot stubs), logging infrastructure, application logic. These belong in `services/`. If you are about to add something to the kernel that isn't on the list in `src/main.rs`, read §4.4 first.

## Build

```bash
cargo build -p kernel --target x86_64-unknown-none
```

The kernel requires a custom target spec. The binary is a flat ELF loaded by Limine.

## Module map

| Module           | Spec section | Unsafe permitted? |
|------------------|-------------|-------------------|
| `arch/x86_64`    | §11, §12    | Yes - hardware boundary |
| `memory/`        | §10         | Yes - physical addresses |
| `capability/`    | §7          | Yes - global table |
| `smp/`           | §9, §11     | Yes - APIC MMIO |
| `ipc/`           | §8          | No  |
| `task/`          | §9, §14     | grandfathered: `mod.rs` 7 (kstack pool + spawn), `scheduler.rs` 37 - see `docs/unsafe-audit.md` |
| `syscall/`       | §8.2        | 2 grandfathered lines (syscall entry - see audit) |
| `interrupt/`     | §12         | 1 grandfathered line (IDT delivery - see audit) |
| `invariants/`    | §22         | No  |
| `bootcon/`       | §11.4       | No - see below |
| `log.rs`         | §11.4       | No  |
| `control.rs`     | §17         | No  |

## Unsafe policy (§18)

`unsafe` is permitted **only** in `arch/`, `memory/`, `capability/`, `smp/`. Every `unsafe` block must have a `// SAFETY:` comment. The grandfathered lines in `task/`, `syscall/`, and `interrupt/` are documented in `docs/unsafe-audit.md` and frozen - they may decrease but increase only by a recorded §18.5 amendment with rationale. There are no such amendments: hardening that needs `unsafe` (e.g. the H4 W^X / kstack-guard work) puts it in a permitted layer (`arch/`) and uses safe `fn`s for boot-ordering call sites, so the grandfathered floors hold.

A PR adding an unsafe block without a SAFETY comment is rejected without review.

## Boot/panic console floor (`bootcon/`)

**Not a terminal.** It draws printable ASCII, honours newline, carriage return, tab and backspace,
and DISCARDS escape sequences. No character grid, no cursor, no reverse video, no UTF-8, no scrollback; reaching the bottom
of the screen clears it and starts at the top.

The terminal - the ANSI/CSI state machine, the UTF-8 decoder, the shadow grid, the cursor, scrolling,
reverse video - is the **`console` service** (`docs/console-service.md` §9). It drives this same
framebuffer through an MMIO grant. What is left in the kernel earns ring-0 residency by impossibility:
a panic halts every core, including that service, so it cannot ask anyone to report it, and boot output
precedes every service including the one that would render it. Both are §11.4's ring-buffer argument on
a machine with no serial port, which is what the §11.4 amendment records.

`bootcon/` is **not** one of the four unsafe-permitted layers and does not need to be: the arch hands it
the framebuffer as a `&'static mut [u8]`, so every pixel write is a bounds-checked slice write. The one
`unsafe` per arch - turning a mapped address into that slice - lives in the arch backend.

Each arch owes ONE primitive through `arch::imp`: `fb_commit` (publish a written rectangle - a cache
clean where the mapping is cacheable, a store fence where it is write-combining, a drain where it is
non-cacheable). `FB_READBACK_CHEAP` is gone with the scroll it selected.

**Ownership of the framebuffer is a state, not a convention.** The kernel draws until the framebuffer is
GRANTED to the `console` service at spawn (`release`), and stops from that moment - not from the first
byte the service renders, which can be seconds later and let the floor paint over a live terminal. It
takes the screen back if that service dies (`reclaim_on_death`) or on a panic (`reclaim_for_panic`).

## Control channel (`control.rs`)

`control.rs` implements the COM2 serial control channel used by the test harness to inject `RESTART`/`KILL` commands at runtime (§17). `process_pending()` is called from Core 0's timer ISR on every tick - not only in the scheduler idle branch - so commands are processed even under full task load.

## Panic behaviour

The panic handler prints `KERNEL PANIC: {info}` to the serial console (and the log ring buffer), then calls `halt_all_cores()` - which broadcasts an NMI so **every** core halts, not just the panicking one (SEC-18). There is no recovery. A reserved crash-page that persists the panic reason across reboot was once described here but was **never implemented**, and `init` (which would have re-read it) is removed (Phase 5); the panic reason lives on the serial console only (SEC-20 doc-drift correction).
