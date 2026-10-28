# Compatibility: what GodspeedOS promises not to break

**Status: the covered surface is DECIDED (operator, 2026-10-26) and GATED (2026-10-28, below); the policy
is DRAFTED (2026-10-28, below) with four questions open for the operator.** This is step 1 of the path to v1 in
[`backlog/71`](../backlog/71-a-v1-compatibility-promise-is-not-credible-yet-and-the-path-to-one.md). No
promise is in force yet: GodspeedOS is `v0.x`, and stays `v0.x` until the gate exists and a stretch of
releases has held it (`backlog/71`, steps 3 to 5).

The promise serves the contributor. In the operator's words: *"I want contributions to be easily grokable from
api interfaces and stdlibs that don't change."* So this lists what a person writing a service, a utility, an app,
a driver or a script relies on. Everything listed may **grow**: a new module, a new function, a new utility,
a new record field, a new contract key. Removing or changing something listed is a **breaking change**.

## What is covered

| What | Where it is defined | What "unchanged" means |
|---|---|---|
| **`gs`**, the standard library (the `godspeed` crate) | [`stdlib/rust/src`](../stdlib/rust/src) | Its public items keep their names, signatures and meaning. |
| **`gs::driver`**, the driver tier | [`stdlib/rust/src/driver`](../stdlib/rust/src/driver), [`docs/driver-library.md`](driver-library.md) | **Grow-only**: a new mechanism may be added; an existing one (`wait`, `delay`, `irq`, `registers`, `arena`, `bus`, `input`, `power`, `device`) may not change. |
| **The `gsh` language** | [`docs/scripting.md`](scripting.md) | A script that runs on one release runs on the next. |
| **The record fields utilities emit** | [`utilities/`](../utilities), [`docs/records.md`](records.md) | A utility keeps its name and the names and meaning of the fields it emits as records, which is what a pipe or a script consumes. The human-readable text around them may change. |
| **The on-disk formats** | GSFS0008: [`docs/persistence.md`](persistence.md); the WiFi key store: [`utilities/56_wifi.md`](../utilities/56_wifi.md) section 6; the layout: [`docs/filesystem-layout.md`](filesystem-layout.md) | A disk written by one release reads on the next. GSFS0008 already evolves through its feature masks (`osdev test fs-compat`): a new `compat` or `ro_compat` bit is growth, a new `incompat` bit is breaking. |
| **The contract schema** | [`contracts/schema/service.schema.json`](../contracts/schema/service.schema.json) | A contract that validates on one release validates on the next, and each key keeps its meaning. |
| **The package format** | [the package format](https://github.com/godspeedhq/godspeed-cli/blob/main/docs/package-format.md), in the godspeed-cli repository | A package built for one release still installs on the next. The format already carries its own version, and the program layout its own release (`LAYOUT_RELEASE`). |
| **The platform file** | `godspeed-platform.toml`, [`docs/developer-services-design.md`](developer-services-design.md) section 11 | The facts the CLI builds against keep their names and meaning. |

## What is not covered

These change when the system needs them to, and are named so nobody mistakes them for a promise:

- **The SDK** (`godspeed-sdk`, `sdk/rust`). It is what the kernel can do, and moves with the kernel. A crate
  depends on it only as a named exception (`scripts/SDK-DEPENDENCY.baseline.txt`); every other crate is on `gs`
  alone. A contributor's driver package *may* call the SDK, outside this promise and unchecked
  ([`docs/driver-library.md`](driver-library.md), "A contributor's driver").
- **Syscall numbers and the system-call ABI.**
- **Service-to-service protocols**: the supervisor's operator commands, the filesystem's and the network
  stack's wire formats, the USB hosts' device protocols. `gs` is the client for the ones a program needs.
- **The kernel**, its queries (`InspectKernel` numbers) and its log text.
- **A utility's human-readable output**: wording, layout and colour. Only its record fields are covered.
- **The test suites and the enforcement scripts.**

## How it is held

[`scripts/surface_check.py`](../scripts/surface_check.py) runs on every build (`osdev build` and the board
builds) and compares what the source defines with the snapshot,
[`scripts/SURFACE.baseline.txt`](../scripts/SURFACE.baseline.txt), one line per promise:

| What | Read from | One line per |
|---|---|---|
| `gs` and `gs::driver` | `stdlib/rust/src`, walked from `lib.rs` through the public modules | function signature (parameter names ignored), type and derive, public field, variant, trait item, method, trait implementation, constant, re-export |
| SDK items `gs` re-exports | the SDK file each comes from | the same, under the `gs` module that exports it (`gs::record::Table`'s methods) |
| utilities | `UTILS` and `SUBCMD_FIRST` in the shell | utility name, and each word a utility takes first |
| record fields | every literal `Table::new(&[..])`, and `godspeed`'s report columns | column, keyed by the function that builds the table |
| the contract schema | `contracts/schema/service.schema.json` | key, type, allowed value, required key, narrowing constraint (pattern, bounds, `additionalProperties: false`) |

- **Growth** - a line the source produces that the snapshot lacks - fails until it is recorded with
  `python scripts/surface_check.py --bless`. The promise allows it; the snapshot records it so that the item
  is held from then on.
- **A removal or a change** - a snapshot line the source no longer produces - fails. A changed signature
  shows as the old line gone and the new one present. `--bless` refuses it.
  `python scripts/surface_check.py --accept-break "<reason>"` records it, dated, with its reason, in
  `scripts/SURFACE-BREAKS.txt`, and moves the snapshot. Before v1 that is how the surface still moves;
  after v1 the policy below says when a break is allowed at all.
- **The contract schema runs the other way for narrowing**: a new required key, or a new or tighter
  constraint, is the break (a contract that validated stops validating), and dropping one is growth.
- **It tests itself**: 20 mutants of the real sources (a renamed function, a dropped `Copy`, a removed
  column, a new required key, a parameter only renamed, an SDK method `gs` does not re-export) run on every
  build in memory, and each must be reported as what it is.

**What it does not hold**, so nobody mistakes the gate for more than it is:

- **Meaning.** A function that keeps its signature and changes what it does passes. The identity, shell
  and `selfcheck` suites hold behaviour.
- **The `gsh` language** has no table to read; its keywords are tests inside the interpreter. It is held by
  the scripts the `selfcheck` corpus and `osdev test shell` run.
- **The on-disk formats**: GSFS0008 by `osdev test fs-compat` and its feature masks; the WiFi key store and
  the layout by their own suites. **The package format** is in the godspeed-cli repository. **The platform
  file** by `scripts/platform_file.py`.
- **`ServiceContext`'s methods.** `gs` re-exports `ServiceContext` as the handle every program is given, and
  the gate holds that name; its methods are the raw system calls, which are the SDK, and are not covered.
- **Adding a variant to an enum, or a public field to a struct,** is recorded as growth. In Rust it can break
  a caller that matches every variant or builds the struct by its fields. Whether that is allowed is the
  policy's to say.

## The policy

**Status: DRAFTED 2026-10-28, not yet approved.** The two rules `backlog/71` settled (security and soundness
fixes may break; anything else removed is deprecated for one release first) are written out below. The
questions the gate raised that are the operator's to answer are listed after them, each with a proposal. No
part of this is in force before v1: until then the gate records every break with its reason and allows it.

### From v1

1. **The covered surface may grow. It may not be removed or changed, except as below.** Growth means a new
   item, never a new meaning for an old one: a function that keeps its name and signature keeps what it does.
2. **A security or soundness fix may break it at once.** Security: keeping the item would leave authority
   where the model says there is none (CLAUDE.md 3.1). Soundness: the item cannot be used correctly, or
   promises what the system cannot deliver. The break is recorded with `--accept-break`, naming which of the
   two it is, and the release notes say what to do instead.
3. **Anything else is deprecated for one release, then removed.** In the release that deprecates it, the
   replacement already exists and the old item still works:
   - a `gs` item carries `#[deprecated(since = "<version>", note = "use <replacement>")]`, so the compiler
     says so at every caller;
   - a utility or subcommand still runs and says once, on the console and never into a pipe, what replaces it;
   - a record column is still emitted beside its replacement;
   - a contract key still validates, and its schema `description` names the replacement.
   The removal in the next release is recorded with `--accept-break "deprecated in <version>: <what>"`.
4. **The version says which happened.** A `v1.x` release only grows the surface or makes the removals above.
   A break that is neither a security or soundness fix nor a finished deprecation is `v2`.

### Open questions, with proposals

- **Q1. A new enum variant or a new public field is recorded as growth, but Rust can break on it.** A caller
  that matches every variant of `gs::Error`, or builds `gs::fs::Stat` by its fields, stops compiling when one
  is added. *Proposal:* before v1, mark the public enums meant to grow (`Error`, `gs::trace::Outcome`,
  `gs::driver::bus::UsbError`) and the structs `gs` hands back (`Stat`, `Check`, `Scrub`, `Drive`,
  `DirEntry`, `Listing`, `gs::net::Status`) `#[non_exhaustive]`, so adding one really is growth; the gate
  then records `non_exhaustive` and treats removing it as a break. Doing that before v1 is itself a break,
  which is the reason to do it now.
- **Q2. The gate holds every utility in `UTILS`, test and debug tools included** (`chaos`, `churn`, `fcap`,
  `spawncap`, `spawnwired`, and subcommands such as `wifi debug`, `audio debug`, `fcap reuse`). The list of
  what is not covered names the test suites. *Proposal:* name these as test tools, outside the promise, and
  have the gate leave them out.
- **Q3. `ServiceContext`'s methods stay out** (they are the SDK, which is not covered), and the gate holds
  only the name `gs` re-exports. *Proposal:* confirm, and say in the developer guide that a program
  written on `gs` alone is the one the promise protects.
- **Q4. "One release" may be days.** Releases have come a few days apart. *Proposal:* one release AND at
  least 30 days, whichever is later, so a deprecation is seen before it is acted on.

## What is still to do

- **The policy's open questions** above (`backlog/71` step 4), then the policy is in force from v1.
- **The soak** (`backlog/71` step 5): releases in which the gate never has to accept a breaking change.
