# 53 - `build.yml` is four months stale, and fails to BUILD rather than failing to RUN

**Status:** OPEN - two breakages found and one fixed; the workflow is still paused and still red.
**Found:** 2026-09-25, by dispatching it for a cosmetic reason and getting a real failure.

## What it is

`.github/workflows/build.yml` triggers on `workflow_dispatch` ONLY:

```yaml
on:
  workflow_dispatch:  # paused: re-add push/pull_request when CI minutes are available
```

Its last green run was **2026-05-24**. The three runs since (July, on a feature branch) failed at 0s.
Nothing has exercised it in four months, and the tree moved underneath it.

## Why a paused workflow being broken is worse than it sounds

**It fails to BUILD, not to RUN.** Anyone who dispatches it - to check something, or to give a commit
a green check - gets a red X and an error about a missing `pong` on a tree that is perfectly healthy.
A dormant workflow should be inert. This one is a trap, and it caught its own maintainer on
2026-09-25: it was dispatched purely to put a check mark on a commit that had none, and manufactured
the failure it was meant to dispel.

Worse, a red run against a commit paints the branch red in the UI, because GitHub aggregates checks
by SHA - so a paused, broken workflow can make `main` look broken.

## What is FIXED

**Ordering: the supervisor must be built after the services it embeds.** `cargo build --workspace`
gives no ordering guarantee between members, and `supervisor/build.rs` panics if `pong` is not
already built. Fixed in both the build and clippy steps (`29b6126c`), and the fix is confirmed: the
next run got past that step and died later.

## What is still BROKEN, and not fixed here

**Clippy now denies a lint the kernel trips:**

```
error: this public function might dereference a raw pointer but is not marked `unsafe`
       in `kernel_main` (kernel/src/main.rs): let boot_info = unsafe { &*boot_info_ptr };
    = note: `#[deny(clippy::not_unsafe_ptr_arg_deref)]` on by default
```

(The compiler names a line; `kernel_main` is quoted instead because a line number rots on the next
edit above it and this entry may sit here a while. The full output is in the run log.)

That is `kernel_main`, the x86 boot entry. It has not changed; the LINT has. `not_unsafe_ptr_arg_deref`
is deny-by-default, and this workflow last passed under a toolchain where this did not fire.

**And there is at least a third layer.** Running `cargo clippy -p kernel` locally fails earlier still,
because the kernel `include_bytes!`-embeds the supervisor and the debug profile has no supervisor
built. So the workflow's later steps have never been reached, and how many more breakages sit behind
them is UNKNOWN. This entry does not claim two problems; it claims two FOUND.

## Why it was not chased further

The workflow is paused **because CI minutes are scarce**, and reviving it consumed 20 minutes of them
in two runs while fixing one layer of an unknown number. Spending scarce CI on a workflow that is
paused for lack of CI is backwards. Reviving it is its own task, with its own decision about whether
it is worth the minutes at all.

## The options, when someone does pick this up

1. **Revive it properly**: fix the clippy lint (either mark `kernel_main` unsafe, or allow the lint
   at that site with a reason), work through whatever is behind it, and re-enable push triggers.
2. **Delete it.** `identity.yml`, `fuzz.yml`, `coverage.yml`, `mutation.yml`, `storage.yml`, `pages.yml`
   and `release.yml` all still run. If nothing has needed `build.yml` in four months, its absence is
   the honest state and deleting it removes the trap.
3. **Make it fail LOUDLY AND IMMEDIATELY** if it must stay paused - a first step that exits with
   "this workflow is paused and unmaintained since 2026-05-24; see backlog/53" costs one second
   instead of six minutes, and cannot be mistaken for a real build failure.

(3) is the cheapest way to stop it being a trap without deciding (1) versus (2).

## The lesson worth keeping

A cosmetic action produced a real failure. The trigger was wanting a green check on a commit that had
none - and the right response to "this commit has no checks" is to leave it alone, not to find a
workflow to run at it.
