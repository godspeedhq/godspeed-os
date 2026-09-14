# 27. A wait bounded by a clock that may not exist, and the absence is handled by not waiting

**Severity:** correctness, latent. Nothing is known to be broken on the five machines; what is
recorded is a shape that turns a three-second wait into no wait at all, silently, and then reports a
different cause.
**Status: OPEN.** The fix is Rust in the SDK, so it is recorded rather than made.

## The shape

`ctx.duration_cycles(ms)` converts a duration to TSC cycles (`sdk/rust/src/service_context.rs`):

```rust
let per_10ms = self.tsc_ticks_per_10ms();
if per_10ms == 0 { return 1; }              // uncalibrated: floor to one quantum
```

and `tsc_ticks_per_10ms()` returns `0` whenever its `InspectKernel` query errors:

```rust
let ret = unsafe { raw_syscall(13, 16, 0, 0) };
if ret < 0 { 0 } else { ret as u64 }
```

So on a machine with no usable calibration, `duration_cycles(3_000)` is **1 cycle**. Every deadline
built from it collapses to now, the loop that was meant to poll for three seconds runs once or not at
all, and nothing says so.

**88 call sites across ten service files** build deadlines this way. It is the local idiom.

## How it was found, and why the caller is not at fault

A weak model was asked to make `nic-driver` wait for the ethernet link before reporting ready. What it
wrote is Commandment VIII done correctly: it polls the truth (the PHY link bit), bounds the wait by a
clock rather than an iteration count, yields, and logs the outcome either way:

```rust
let t_link_deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(LINK_UP_MS));
while ctx.read_tsc() < t_link_deadline { ... }
ctx.log_fmt(format_args!("... (link {})", if link_up { "UP" } else { "down (no cable?)" }));
```

On an uncalibrated machine that wait does not happen and the log says **`link down (no cable?)`** -
blaming the cable for a broken clock. The caller followed the idiom faithfully; the idiom is the
defect. That is invariant 12 broken twice: the degradation is silent, and the loud line that follows
names the wrong cause.

## The same root, twice before

`0` standing for "unknown" and being read as a legitimate value:

- the arm32 **liveness watchdog was inert for a whole port's bring-up**, gated on a
  `tsc_ticks_per_quantum` stub that was `0`;
- the **T630's TSC calibration is recorded as roughly 1000x too small** (CPUID leaves 0x15/0x16 are
  Intel-only; this is an AMD part), which breaks ping RTT and `sleep` - so even a NON-zero value here
  is not evidence the clock is right.

## Why it is not fixed here

The fix is in the SDK and is Rust: `duration_cycles` should not substitute a value it did not measure.
The honest signature returns `Option<u64>`, or the pair `(cycles, calibrated)`, so a caller must
decide what to do when there is no clock - which is what CLAUDE.md 26.7 asks of every failure path.
That is a change on 88 call sites across every service, on a tree whose five boards would all need
re-testing, so per 26.7 it is written down rather than half-started.

**Interim, and cheap:** a caller that cannot tolerate a collapsed deadline can check
`ctx.tsc_ticks_per_10ms() != 0` before building one, and say plainly that it could not wait.

## What would close it

1. `duration_cycles` stops returning a value it did not measure.
2. A checker for the pattern - a measurement helper that converts "unavailable" into a plausible
   number - which is the mechanisable half of Commandment VIII and does not exist yet. The other
   half (wait on truth, bound it by a clock) this code already got right.
