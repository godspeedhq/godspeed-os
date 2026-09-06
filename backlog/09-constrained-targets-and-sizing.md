# 9. Constrained targets: what actually blocks a microcontroller, and how the sizing should be tuned

**Severity:** design question, unforced. Nobody has the hardware; this exists so the answer is not
re-derived from scratch when someone does.
**Measured:** 2026-09-06, on the x86_64 release kernel.

## The numbers, so nobody has to guess again

```
.text      96 KiB      the actual kernel code
.rodata   1.5 MiB      mostly the embedded supervisor image (193 KiB) and its services
.bss     22.5 MiB      the static arenas
         ────────
         24.1 MiB
```

The arenas are two objects and nothing else comes close:

| object | size | driven by |
|--------|------|-----------|
| `task::KSTACK_STORAGE` | 15.2 MiB | `MAX_TASKS = 224` x **69.6 KiB** per task |
| `ipc::routing::TABLE` | 6.8 MiB | `MAX_ENDPOINTS = 96` x **72 KiB** per endpoint |
| `BSP_BOOT_STACK` | 524 KiB | |
| `scheduler::TASK_CAP` | 344 KiB | |

**The code is microcontroller-sized already.** 96 KiB of text would fit an ESP32-C3's 400 KB SRAM
with room to spare. What does not fit is arenas dimensioned for a four-core desktop.

## The four numbers, and the fact that TWO of them are law

None of these is configurable today - they are plain `const`s, and nothing in the kernel is
feature-driven for size:

```
kernel/src/ipc/message.rs      MAX_MESSAGE_SIZE = 4096
kernel/src/ipc/queue.rs        QUEUE_DEPTH      = 16
kernel/src/ipc/routing.rs      MAX_ENDPOINTS    = 96
kernel/src/task/scheduler.rs   MAX_TASKS        = 224
```

They are not the same KIND of number, and conflating them is the mistake to avoid:

- `MAX_TASKS` and `MAX_ENDPOINTS` are **just numbers**. Nothing in CLAUDE.md fixes them;
  `MAX_ENDPOINTS` even carries a comment recording that it was raised from 64.
- `QUEUE_DEPTH` and `MAX_MESSAGE_SIZE` are **constitutional**. 8.5 states "Maximum message size:
  4 KiB (one page)" and "16 messages per endpoint, fixed in v1", and then addresses this exact
  question: *"Queue depth is not configurable per endpoint in v1. Per-endpoint depth is a v2
  concern."*

And it is the constitutional pair that actually blocks the small machine: **4 KiB x 16 = 64 KiB per
endpoint**, so eight endpoints exceed an ESP32-C3's entire SRAM. No amount of tuning the other two
gets around it.

## Should the sizing be externalised to a config file? NO - and the reason is this project's own

It is the obvious answer and it is the wrong one here.

1. **A config is a feature flag with infinite settings.** `COMMANDMENTS.baseline.toml` says it
   plainly: *"A feature flag is a switch on what the kernel IS... Test-only features count - a build
   the kernel can be put into is a build someone can ship."* Every kernel feature must be pinned and
   admitted deliberately. A sizing config admits an unbounded, unpinned space of kernels, which is
   the same rule broken at a larger scale. The `single-core` flag needed a written rationale for ONE
   boolean; a config file would need one per dimension and would not get it.
2. **Every combination is an untested kernel.** The identity suite pins behaviour for one
   configuration. A config space ships configurations nobody ran.
3. **Two of the four are law, and law does not become a setting.** Making `QUEUE_DEPTH` a config key
   silently converts a constitutional constant into a preference - the "silent fallback" family,
   21's automatic-rejection list.
4. **26.2.** Nobody has the hardware. Building a configuration system for a hypothetical target is
   speculative extensibility, which that section names as architectural debt in as many words.

## What to do instead, when someone actually has the board

**Boot-size the arenas, do not configure them.** There is precedent in the tree: the `MAX_CORES`
ceiling was deleted outright and replaced with arenas sized at boot from the live core count
(`smp::core::init_arenas`, `CORES.init_with(n, ...)`). The same treatment fits `MAX_TASKS` and
`MAX_ENDPOINTS`: size them from what the machine reports it has, at boot. One mechanism, no
configuration matrix, no untested combinations, and every port benefits rather than just the small
one. This is the piece worth doing even without an ESP32 in hand - it would cut ~22 MiB of .bss on
every target.

**Amend 8.5 deliberately, for a named profile, only when the hardware exists.** A smaller message
and a shallower queue is a real trade with real consequences (a 512-byte message changes what a
protocol can send in one hop), and it deserves the amendment process rather than a key in a file.

## The wall behind the wall: there is no MMU

Worth knowing before anyone starts, because it is not a tuning problem. 10.1 requires each service
to hold a separate VIRTUAL address space via per-task page tables, and invariant 2 rests on it. The
ESP32-C3 has **PMP - physical memory protection, a handful of regions** - not paging. Isolation
could plausibly be enforced with PMP and satisfy invariant 2's INTENT, but 10.1's mechanism would
need amending and 10.5 (TLB shootdown) becomes meaningless.

That makes a microcontroller target a **variant of the model, not a port of it** - which is a much
larger decision than any of the sizing above.

Two things do favour it: the project is already 32-bit-capable (arm32 is a first-class port, so RV32
is not a new class of problem), and the ESP32-C3/C6 are RISC-V, which is the port already named as
coming next.
