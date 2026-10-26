# Compatibility: what GodspeedOS promises not to break

**Status: the covered surface is DECIDED (operator, 2026-10-26); the gate that holds it and the written
policy are not built yet.** This is step 1 of the path to v1 in
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

## What is still to do

- **The gate** (`backlog/71` step 3): a checked-in snapshot of the covered surface, and a checker on the build
  path that fails when something in it is removed or changed, so a breaking change needs a recorded reason.
- **The policy** (`backlog/71` step 4): the exceptions, such as security and soundness fixes, and how a
  removal is deprecated first.
- **The soak** (`backlog/71` step 5): releases in which the gate never has to accept a breaking change.
