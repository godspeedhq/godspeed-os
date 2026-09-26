# 59 - `SET_CLOCK` is granted and never checked, and net-stack blames the kernel for the `time` service

**Status:** OPEN - two findings from one dead seam. The second is operator-visible and is the same
instrument defect as `backlog/57`, on a different service.
**Found:** 2026-09-26, by following a stale comment: three comments named a `SetClock` syscall, and
there is no such syscall.

## 1. A capability that is minted and checked by nobody

`SET_CLOCK_RESOURCE` (ResourceId 13) is registered (`kernel/src/capability/mod.rs`) and minted from
the privilege word for two bits, `SET_CLOCK` and `SET_CLOCK_FLOOR` (`kernel/src/task/mod.rs`). Nothing
consumes it:

- No `SyscallNumber` variant sets a clock.
- Nothing in `kernel/src/syscall/` or `kernel/src/task/` calls any arch's `set_wall_clock`, and every
  arch defines one (`pub fn set_wall_clock(epoch: i64) -> bool`, seven of them).
- `grep SET_CLOCK_RESOURCE` outside its own declaration and the mint tables returns nothing.

Clock slice 3 moved the wall clock to the `time` SERVICE. `sdk/rust/src/service_context.rs` records
the userspace half of that ("`set_wall_clock`, `set_clock_floor` and `clock_synced_secs_ago` were
REMOVED with the kernel's wall clock"), and `services/net-stack` sets the clock over IPC now. What
was left behind is the kernel-side seam: a capability, two privilege bits, and seven arch functions
with no callers.

**Why it is worth closing rather than shrugging at.** §26.9 says a reviewer must be able to determine
what a service can do and which capability granted it. A capability that is GRANTED and never CHECKED
inverts that: `net-stack`'s contract and spawn row say it may set the clock, and the authority is
inert. It is not a security hole - nothing gains anything - but it is exactly the kind of thing that
makes the next reader distrust the rest of the model, and a future syscall that re-used the ResourceId
would silently inherit whoever holds the bit today.

The comments at both ends now say this is the state (`capability/mod.rs`, `arch/x86_64/rtc.rs`), which
is the §26.7 half. Removing it is the rest: drop the ResourceId, the two privbits, the seven
`set_wall_clock` stubs and the contract/spawn-row grants, then re-verify - a spawn-path change, so it
wants a hardware pass, not just QEMU.

## 2. The log line blames the kernel for a refusal the kernel never made

`services/net-stack/src/main.rs`, in the SNTP path, after asking the `time` service over IPC:

```
net-stack: SNTP - clock set REFUSED by the kernel (no SET_CLOCK cap) - clock unchanged
```

Two things wrong with one line, and the comment four lines above it already knows better ("Clock
slice 2: the wall clock belongs to the `time` service now, not to a kernel syscall"):

- **It names the wrong component.** The request went to `time` by IPC (`OP_SET`). The kernel is not
  in the path and holds no opinion. An operator reading this goes and audits capabilities.
- **It reports a TIMEOUT as a REFUSAL.** The `accepted` value is false on three different outcomes:
  `time` replied 0 (a real refusal - its plausibility check or clock floor said no), `time` replied
  nothing, or `time` was unreachable even after a reacquire. All three print "REFUSED by the kernel".

That is `backlog/57` again in a different service: *an instrument that cannot tell a refusing peer
from an absent one, and reports the refusal it did not observe.* There it mattered because the message
is nominated evidence for a constitutional claim. Here it matters less, but the shape is the one this
project has decided to treat as a defect rather than a wording nit.

**The fix is the same shape as 57's:** carry the distinction that already exists one layer down.
`time` replying `0` is a refusal and should say so with `time`'s own reason; no reply is
`time` unreachable and should say THAT, and say the clock is unchanged either way.

## Why they are filed together

They are one seam read from both ends. The kernel side is dead authority; the userspace side is a
message that still describes the dead path as if it were live. Fixing either alone leaves the other
telling the same wrong story, and the kernel half cannot be removed without checking that nothing in
userspace still expects to be refused by it.
