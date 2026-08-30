# Probe parameters at spawn: taking 193 policy rows out of the kernel

**Status:** BUILT, on `feat/supervisor-owns-images`. The pin moved 221 -> 29 and
`kernel/src/task/mod.rs` lost 2,317 lines.
**Scope:** test probes only. Real services are untouched by this step.

## The problem, measured

`kernel/src/task/mod.rs` holds a `service_config` entry for **221** services. `COMMANDMENTS.baseline.toml`
pins that list as debt that may only shrink, and the target is **one** entry: `supervisor`.

But 221 entries is not 221 programs. Only **27 distinct ELFs** are embedded, and **193 of the 221
entries are the same binary** - `PROBE_ELF` - differing by a test-mode number. Their policy barely
varies:

| field | distinct values | dominant |
|---|---|---|
| `memory_limit` | 2 | 64 MiB (189/193); 4 MiB for four alloc-pressure probes |
| `hw_irqs` | 2 | none (192/193); IRQ 33 for `probe-11a` |
| `send_peers_grant` | 2 | false (192/193); true for `probe-5a-send` |
| `has_console_read` | 1 | false (all 193) |
| `has_recv_endpoint` | 2 | false (120), true (73) |
| `preferred_core` | 5 | round-robin (126), then 0 / 1 / 2 / 3 |
| `send_peers` | 45 lists | none (145); a single sibling probe for most of the rest |
| `probe_mode` | 193 | the actual variable, range 0-214 |

So the kernel is not holding a catalogue of services. It is holding **one program and a table of 193
test parameters** - which is policy, and policy belongs to the supervisor (§26.10).

## What changes

The supervisor already names every probe: it spawns them explicitly, in build-feature-gated groups.
What it does not supply is the parameters. It will.

**No new syscall, and no change in arity.** `handle_spawn` today packs `(cap_slot, core)` into the low
32 bits of `arg0` and leaves the upper 32 unused. The parameters ride there:

```text
arg0:  [63..56] reserved  [55..48] flags  [47..32] probe_mode  [31..16] core  [15..0] spawn cap slot
flags: bit0 has_recv_endpoint   bit1 small memory limit (4 MiB)   bit2 is-probe
```

`send_peers` ride in the existing name argument as a **NUL-separated list**: the first element is the
task name, the rest are peer names. The kernel already name-wires peers through `ipc::names`, so it
needs the list and nothing else. The payload limit went from 64 to 128 bytes, which the longest real
payload is well under; a payload that will not fit is REFUSED rather than truncated, because a
silently shortened peer name would wire a probe to the wrong service and read as a passing test.

The kernel keeps **one** `probe` entry: the ELF and the defaults.

### The prerequisite: task names had to be owned

Task names were `[&'static str; MAX_TASKS]`. That is a small thing with a large consequence: a name
had to be a string literal compiled into the kernel, so a caller could not supply one - which is
*why* the catalogue existed. They are owned bytes now, in `smp::names::NameTable`.

That module is in a permitted layer (§18.1) deliberately, not incidentally. Written where it is used
it would have taken `task/scheduler.rs` from 37 `unsafe` lines to 40 - a grandfathered floor, which
§18.5 lets grow only by a CLAUDE.md amendment, and only after trying a permitted layer first. It fits
there honestly rather than as a dodge: a shared array with one writer per slot and readers on every
core **is** a concurrency primitive, and it belongs beside `SpinLock`. `task/scheduler.rs` stays at
its floor; the four blocks are audited once, in `audits/unsafe-audit.md`.

### One table, two spawners

**Two** principals spawn probes: the `supervisor` starts each suite, and a probe **respawns its own
victim** (a restart test has to). Both need the same parameters, so the table is one file -
`services/probe/src/table.rs` - which the supervisor includes by path. Two copies of a parameter
table is two truths (Commandment III): the second drifts, and a probe respawned with the wrong mode
is a test that passes while testing the wrong thing. It lives with the `probe` program because it
describes that program's test modes, not the supervisor's policy.

## What stays in the kernel, and why

Two of the 193 outliers are not parameters, they are **authority**:

- **`probe-11a` needs IRQ 33 routed to it.** Routing a hardware interrupt line to a service is a grant,
  not a setting. It stays keyed by name in the kernel, beside the other hardware-privilege decisions.
- **`probe-5a-send` needs `send_peers_grant`** - its peer caps carry GRANT so it can re-delegate them
  (§22 Test 5A). Handing out a re-delegatable capability is authority too.

This is the line: **the kernel keeps decisions about what a service may DO; the caller supplies what a
service IS.** A probe's mode, core, mailbox and memory ceiling are the latter. An IRQ route and a
grantable capability are the former.

## What this does NOT do

It does not move service IMAGES, and it does not let the supervisor supply hardware privileges. That
is the larger change (the `Spawn` ABI carrying an image pointer), and it needs a CLAUDE.md amendment
because it changes who arbitrates hardware authority. This step deliberately stops short of it.

## What this costs, recorded rather than left to be discovered (§26.7)

A probe can no longer be spawned or restarted **by name alone**, because its parameters are no
longer anywhere the kernel can look them up. Two consequences:

- `control RESTART <probe>` over the operator channel now fails with `NotFound` and a loud kernel
  line, where it used to work. Nothing uses it - the suites only ever `KILL` a probe, and every
  respawn goes through the table - but it is a real capability that went away, so it is written down
  rather than left for the next person to trip over.
- A probe that `chaos` kills stays dead. That is unchanged: probes were never in the supervisor's
  `MANAGED` set, so nothing respawned them before either.

Spawning a **real service** by name is untouched: those 29 rows are still in the kernel, and the
plain `Spawn` path is byte-for-byte what it was.

## Effect on the pin

`service_configs` drops from **221** to **29**: the real services, the examples, and the single
generic `probe` entry. Kernel responsibility decreases; nothing is added.

## How it is verified

The probes ARE the QEMU suites - `identity`, `property`, `fuzz`, `stress`, `adv`, `chaos` and their
brutal variants all spawn them. A wrong parameter does not fail subtly; 193 probes fail loudly. The
full battery is the test for this change.

Result: identity 24/0/0, property 10/0, fuzz 8/0, stress 10/0, adv 15/0, chaos 7/0,
identity-brutal 6/0, property-brutal 10/0. Both authority outliers are covered - IR1A exercises
`probe-11a`'s IRQ route, Test 5A exercises `probe-5a-send`'s grantable peer caps.

**One pre-existing failure surfaced and was fixed on the way.** Brutal property BP7 read the victim's
generation *between* the kill and the respawn - the dead window, where unregister-on-death correctly
returns 0 - so it failed on its first iteration and had done since unregister-on-death landed (P7 was
updated then; BP7 was not). Confirmed against `main` before attributing it, per "don't guess". The
kernel was right and the test was stale; BP7 now reads after the respawn, like P7, and makes the
assertion it was written to make.
