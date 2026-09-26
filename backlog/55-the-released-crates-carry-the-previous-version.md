# 55 - v0.20.0 shipped every crate labelled `0.19.0`, and the published API docs say so

**Status:** CLOSED 2026-09-26, by option (1) - both halves. `workspace.package.version` is `0.21.0`, matching the v0.21.0 tag, and `release.yml` now compares the tag against the manifest in the step that resolves the tag and FAILS before anything is published. Verified both directions: a v0.20.0 tag against a 0.21.0 manifest is refused, and `v0.21.0-rc1` is still accepted (only the leading vMAJOR.MINOR.PATCH is compared, so a pre-release suffix does not fail for a reason nobody intended).
**Found:** 2026-09-25, during the documentation audit of `feat/stdlib-complete`, from a line of
ordinary build output.

## What it is

Every crate in the workspace takes its version from one place:

```toml
# Cargo.toml
[workspace.package]
version = "0.19.0"
```

That line was last changed by `fb96aaf4 chore: v0.19.0`. The `v0.20.0` tag does not touch it -
`git show v0.20.0:Cargo.toml` still reads `0.19.0`. So the v0.20.0 release built, tagged and
published a workspace in which `godspeed`, `godspeed-sdk`, `osdev` and every service crate declare
themselves to be 0.19.0.

It is visible in the most ordinary output there is:

```
Compiling godspeed v0.19.0 (C:\...\godspeed\stdlib\rust)
```

## Why it matters more than a cosmetic version string

**The published API documentation carries it.** `pages.yml` builds rustdoc and deploys it, and
rustdoc prints the crate version in the sidebar of every page. The README sends a reader to those
pages as the canonical description of the standard library - twelve links, one per module - so the
first fact a newcomer reads about the library is a version that was superseded a release ago.

It also quietly breaks the one thing a version is for. Someone comparing "the API in 0.19.0" with
"the API in 0.20.0" finds the same number on two different surfaces, and a difference they cannot
attribute. The 130-item surface this branch builds would be the third release to claim 0.19.0.

## What it is NOT

Not a build break, not a runtime defect, and not a broken link - the documentation is correct about
every function it describes. `doc_refs`, `doc_symbols_check`, `facts_check` and `site_check` all pass
and are all right to: none of them reads the crate version, because no document restates it.

That is the actual gap. `facts_check.py` exists precisely to catch a number a document repeats
drifting from the code that owns it, and the version is a number the *tag* restates and the
*workspace* owns. Nothing compares the two.

## The options

1. **Bump `workspace.package.version` as part of cutting a release**, and add the check that would
   have caught this: at tag time, assert `Cargo.toml`'s version equals the tag. That is a few lines
   in `release.yml` and it fails loudly before anything is published.
2. **Bump it now to 0.20.0** to make the tree honest about the last release, and let the next release
   move it again. Cheap, and leaves the process gap open.
3. **Decide the version is not load-bearing** and stop publishing it - remove it from the rustdoc
   surface. This is the honest option only if nobody is expected to reason about versions, which
   contradicts tagging releases at all.

(1) is the one that stops it happening a fourth time. It needs a decision about which version the
next release carries, which is the operator's call and not something an audit should make.

## The lesson worth keeping

Every gate in this repository checks a claim that some human wrote down. This one was never written
down anywhere, so nothing could check it - the tag and the manifest are two statements of one fact
(Commandment III) that no instrument compares. A fact restated in a place nobody thought of as a
document is exactly where drift survives an audit that passes ten gates.
