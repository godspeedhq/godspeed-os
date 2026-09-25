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
- ~~the **T630's TSC calibration is recorded as roughly 1000x too small** (CPUID leaves 0x15/0x16 are
  Intel-only; this is an AMD part), which breaks ping RTT and `sleep`~~ - **CORRECTED 2026-09-25, and
  the correction is itself an instance of this entry's subject.** x86 calibration moved to the PIT
  (`arch/x86_64/boot.rs`: "the PIT (portable ground truth - CPUID 0x15/0x16 give a garbage frequency
  on AMD)"), and the T630 now measures itself correctly. From its own boot log:

  ```
  apic: core 16 PIT-calibrated tsc_hz=1996256500 ticks/10ms=19962565
  ```

  1,996,256,500 Hz is ~2.0 GHz, which is right for a GX-420GI.

  **What the stale line cost.** Before a chaos soak on that board, the kill-path budget
  (`tsc_ticks_per_quantum * 75`) was predicted to be ~1000x short, making the T630 the board most
  likely to exercise `backlog/48`'s new abandon path. It fired nothing in 6,485 kills, because the
  budget is a correct 0.75 s. The prediction rested on this line, and on misreading the code comment
  above as a statement of the PROBLEM when it is a statement of the FIX. An entry that records a
  defect and outlives it produces exactly the wrong confidence, which is this entry's own thesis
  applied to itself.

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

---

## Investigation 2026-09-14: which ports actually return 0, and why the fix was deferred

Stopped before changing anything - the fix alters behaviour on a path every service uses, so it
cannot be byte-identical and needs a run on all five boards. Recorded rather than half-made (26.7).

**Query 16 is `arch::imp::boot::tsc_ticks_per_quantum()`** (`syscall/dispatch.rs:2103`). Per port:

| port | answer | note |
|------|--------|------|
| x86_64 | calibrated | the T630's value is recorded elsewhere as roughly 1000x too small (CPUID 0x15/0x16 are Intel-only, this is AMD) - NON-zero, so `duration_cycles` does not fall back, it just lies |
| arm | `timer_hz() / 100` | derived from the hardware |
| aarch64 | **0 unless the `pi4` feature is set** | `#[cfg(not(feature = "pi4"))] pub fn tsc_ticks_per_quantum() -> u64 { 0 }` |
| riscv64 | derived | |
| riscv32, loongarch64, s390x | **0** | scaffolds, expected |

**The aarch64 one is the live edge.** An aarch64 build without `pi4` answers 0, and the comment
directly above that stub already spells out the consequence, in the project's own words: it
"collapses EVERY timed wait to a single tick. Left stubbed, a service asking to sleep one second
slept 10 ms - so `ping`, which sends once a second by contract, sent about a hundred times that and
buried the shell's prompt under 96,000 log lines. The 32-bit port spent its whole bring-up with the
same stub and the same silent 100x error."

So this failure is not hypothetical and is not new - it has bitten twice, and the site documents it.
What this entry adds is the layer above: `duration_cycles` turns that 0 into **1** and says nothing,
and 88 call sites build deadlines on it. The kernel-side stub is honest about being a stub; the SDK
converts it into a plausible number.

**Also worth a porter's attention:** all three scaffolds answer 0. A scaffold that ever reaches
userspace (M4 on the `scaffold_check` ladder) inherits collapsed deadlines everywhere before anyone
has written a timer, which will present as "everything is instantaneous and nothing waits" rather
than as a missing clock.

**Shape of the fix, unchanged from above, with the cost now measured:**

1. Cheapest and safest: keep the signature, make the uncalibrated case LOUD once per service, and
   leave the returned value alone. Timing behaviour identical; a silent degradation becomes a
   reported one (invariant 12). Still a retest, because it is new code on every service's path.
2. Honest: `duration_cycles` returns `Option<u64>` (or `(cycles, calibrated)`) so a caller must decide
   what to do with no clock. 88 call sites across ten service files, every board re-tested.

Do 1 before 2. Neither is a constant-sized change, which is why this is scheduled rather than done.
