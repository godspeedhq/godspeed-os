# 45 - the published book has no standard-library section

**Opened:** 2026-09-23
**Status:** CLOSED 2026-09-24
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

## Closed

The reason this was held open was that "a page written against a surface that is still moving would
be stale before it was published". The dogfood is finished (`docs/stdlib-design.md` 22), so the
surface has settled, and the page ships.

**`website/src/stdlib.md`** - "Writing a program for GodspeedOS", linked from `SUMMARY.md` above the
design report and the brief. It is hand-written rather than an `{{#include}}`, deliberately: what a
newcomer needs is not any document this repository already had, and the constraint above is about not
keeping a SECOND COPY of something - there is no first copy of this. `site_check.py` counts it among
the hand-written pages and checks it against the repository, which is the discipline that constraint
was asking for.

What it covers, against the list above:

- the module surface, by linking the generated reference rather than restating it (`fs`, `net`, `io`,
  `call`, `cap`, `error`, `addr`, each documenting what it blocks on and what authority it needs);
- the error model first and prominently - `retry_is_safe` and why `OutcomeUnknown` is not retryable,
  which 22.7 names as the single failure a stranger commits most confidently;
- the no-heap consequence: the caller owns the buffer, there is no `read_to_string`;
- a whole worked program, every line of it including the contract, because the parts around the
  interesting code are the parts that are not guessable;
- and one thing this list did not anticipate: what happens to a client request that arrives while you
  are waiting for a reply. A program that only computes can ignore it; a SERVICE cannot, and the
  wrong answer there looks exactly like the right one until a client vanishes.

## And the thing that would have made it stale anyway

`pages.yml` rebuilds the site on a change to `website/`, `docs/`, `utilities/`, `sdk/` and the
top-level documents. **It did not list `stdlib/`** - so a corrected doc comment on `gs::fs` would
never have reached the published API reference, on the surface 22.7 actually measures. Added in the
same change. The gap is the same one the `utilities/**` line was added to close, which is worth
noting: a path filter is a list, and a list acquires omissions.
