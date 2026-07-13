# Utility: `version`

**Utility:** `version` - the GodspeedOS version and build stamp
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`version` answers **what version of GodspeedOS is this, and exactly which build?** - the
system's version number plus the short git commit SHA it was built from. It is the
whole-system counterpart to the per-utility `<util> version` (which reports one utility's
version); this reports the OS.

It does **not** overlap with `about`: `about` is the identity card (name, slogan, core
count, credits), `version` is the version fact. `about` carries no version number, so
`version` is where the build lives.

## 2. Invocation

| Command | Meaning |
|---|---|
| `version` | Print the version + build stamp and return. |

## 3. Output

```
gsh> version
GodspeedOS 0.3.0 (f7a6946)
```

The number is the system version (kept in lockstep with the crate versions and the shell's
`UTIL_VERSION`); the parenthesised value is the short git commit SHA stamped at build time.
A build made outside a git checkout reports `(unknown)`.

ASCII only - the framebuffer console's font is ASCII (no non-ASCII glyphs).

## 4. Data source

- **Version:** the compile-time `UTIL_VERSION` constant (the shell's version, == the OS
  version by the release convention; see `CONTRIBUTING.md`).
- **Build SHA:** `env!("GODSPEED_GIT_SHA")`, stamped by the shell's `build.rs` from
  `git rev-parse --short HEAD` at compile time. `build.rs` watches `.git/logs/HEAD`, so the
  stamp refreshes when HEAD moves.

## 5. Capabilities

- **Console output** to print the line. No introspection cap needed (both values are
  compile-time constants).

## 6. Non-goals

- **Not `about`.** `version` is the version fact, not the identity card. Keep them separate
  (conventions rule 7 - a utility reports one raw fact).
- **No per-utility versions.** `version` reports the OS; a single utility's version is
  `<util> version` (the universal subcommand, `0_conventions.md` §1 rule 5).

## 7. Conformance

Conforms: own `version help` / `version version` (with a real example, per `0_conventions.md`);
listed by the shell's top-level `help` under **System**; tab-completes as a command name and
its `version` / `help` subcommands complete at the first-arg position. See `0_conventions.md` §3.
