# 2. GodspeedOS on ONE core - does it actually work, and what has been hiding behind SMP?

**Severity:** feature, and an audit of everything "put it on another core" ever settled.
**Status:** first experiment run 2026-09-06. It BOOTS. What that proves is narrower than it looks.

## Why this is worth doing

Several times, a scheduling or liveness problem has been answered by moving a service to a
different core. That is not a fix; it is a workaround that depends on a second core existing to
absorb the fault. A single-core boot removes the absorber, so anything that was only ever
"solved" by spreading load has to fail honestly instead.

The constitution already claims this configuration works. 11.3:

> AP startup: kernel logs warning, continues with available cores; **if zero APs come up, system
> runs as single-core**

So this is not a new feature request. It is checking a promise the document already makes.

## What the first run showed

`osdev run --smp 1`, identity build, ran for the full 180 s window:

- It boots. `supervisor`, `events`, `console`, `control`, `shell`, `block-driver`, `nic-driver`,
  `net-stack`, `hw-enumerator`, `xhci` all start, all on core 0.
- Cross-core IPC still works, because there is nothing to cross: `pong: received "1224"` by the
  end of the window, so ping/pong sustained ~1200 exchanges on the one run queue.
- No kernel panic, no liveness wedge in the window.
- Every task landed on core 0, including the ones that asked for another core - see item 1, which
  this experiment is what found.

## What it did NOT show, and must not be read as showing

- **`fs` never started, and that is the BUILD, not single-core.** This was the identity build,
  which is probe-heavy and carries no `fs`. A single-core test of the storage stack has not been
  run at all yet.
- 180 s is a window, not a soak. Starvation and priority inversion are exactly the faults that need
  longer than that to show.
- No chaos was run single-core. `chaos max-carnage` on one core is the interesting case, because
  every kill and respawn now contends with the shell for the same quantum.

## What the contracts currently pin

```
supervisor, console, shell, chaos, dwc2  -> core 0
fs                                       -> core 1
xhci                                     -> core 2
ehci                                     -> core 3
```

Each of these needs the same question asked of it: **is this a property of the work, or a
workaround that happened to help?** 9.2's own guidance is "contract authors should specify
placement only when they have a real reason". Whatever the answers, they should be written into
the contracts as comments, because right now the reason for `ehci -> core 3` is not recorded
anywhere.

## How to actually run it (branch `test/single-core`)

**No new kernel feature, deliberately.** The first attempt added `single-core` to
`kernel/Cargo.toml` and the enforcement layer refused it:

```
Commandment I - kernel feature flags are pinned
  new kernel feature 'single-core': a switch on what the kernel IS, which can add a
  responsibility no other pin sees
  An exemption is legitimate ONLY if a CLAUDE.md amendment already accepts it.
```

That is the aarch64 lesson enforced mechanically - *a flag that selects between a compliant and a
non-compliant kernel leaves the violation one build away*. The check was right, the flag was
reverted, and the test is built out of what already exists instead:

| machine | how |
|---------|-----|
| **x86** | `KERNEL_FEATURES=single-core cargo run -p osdev -- image` |
| **Pi 2** | `py scripts/arm_build.py --release --feature single-core` |
| **Pi 4** | `py scripts/pi4_build.py --release --single-core` (omits the already-pinned `pi4-smp`) |
| any, QEMU only | `cargo run -p osdev -- run --smp 1` - no build change needed |

**Check the log before trusting any run.** A single-core kernel says so on the way up:

```
smp: SINGLE-CORE BUILD - APs deliberately not started (backlog/02)
smp: 1 cores ready
```

and the image itself carries the string, so `grep -ac "SINGLE-CORE BUILD" build/os-usb.img` tells
you what you are about to flash. That check exists because two hardware runs were wasted without
it: the first image predated the feature, and a QEMU run was made against a kernel `osdev run` had
quietly rebuilt without it.

### Why a kernel feature after all

The first attempt was refused by `scripts/commandments.py` (Commandment I, kernel feature flags are
pinned) and the external routes were tried first. They do not exist: the **Wyse 5070 firmware has
no core-count setting**, and **Limine's protocol has no core limit** - the APs come from the
kernel's own `MpRequest`, so nothing outside the kernel can withhold them. QEMU's `-smp 1` covers
QEMU alone.

So it is pinned in `COMMANDMENTS.baseline.toml` with that reasoning. It ADDS no kernel
responsibility: it removes the AP-start call and pins the arena count at 1, reaching the state
11.3 already defines. Default builds are byte-identical to before - verified by booting one and
watching it come up on four cores with services on 1, 2 and 3.

## Next steps, in order

1. Settle item 1 first. Until `PlacementInvalid` is enforced or 9.2 is amended, a single-core run
   silently ignores every pin, so nothing learned about placement is trustworthy.
2. Run the STORAGE build single-core (`osdev test script --smp 1` equivalent) and get a selfcheck
   tally. That is the real test: `fs` and `block-driver` on one queue with the shell.
3. Run `chaos max-carnage 100` single-core. Expect this to be the one that finds something.
4. For each pinned contract, record the reason or remove the pin.
