# 45 - the published book has no standard-library section

**Opened:** 2026-09-23
**Status:** OPEN
**Raised by:** the operator, while `feat/stdlib` was being written.

## What is missing

`website/` is the published book (mdBook). Its pages `{{#include}}` the sources in this repository
so a document and its page cannot drift, which is the property that makes the site trustworthy.

**There is no page for `stdlib/rust`.** `docs/stdlib-design.md`, `docs/stdlib-brief.md` and the
module documentation are not reachable from the book, so the one thing a newcomer is most likely to
want - how do I write a program for this OS - is the one thing the published site does not show
them.

## Why it matters more than an ordinary docs gap

CLAUDE.md 22.7, the Stranger Test, measures the system by what a weak model can do given **the
public developer documentation only**. The standard library IS that public interface. A test whose
whole premise is "read the published docs" cannot be run honestly while the published docs omit the
library, so this blocks `docs/stranger-test.md` from its first real run.

## What it should cover

- The module surface as it stands (`fs`, `net`, `io`, `call`, `error`, `addr`).
- The error model, prominently: `Error::retry_is_safe` and why `OutcomeUnknown` is not retryable.
  22.7 names this as the single failure a stranger is most likely to commit confidently.
- The no-heap consequence: the caller owns the buffer, there is no `read_to_string`.
- `examples/stdlib-hello` as the worked program.

## Constraint

Follow the existing `{{#include}}` discipline rather than copying prose into the book. A second copy
of the documentation is a second thing to keep true, and `site_check.py` exists because that has
already gone wrong once.

## Not started

Recorded rather than done (26.7): the branch is still adding to the library, and a page written
against a surface that is still moving would be stale before it was published.
