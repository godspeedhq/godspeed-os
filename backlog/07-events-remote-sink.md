# 7. `events persist start <url>` - ship a capture off-box

**Severity:** feature, deliberately deferred.
**Origin:** raised while designing `events persist`; recorded in `docs/observability.md`.

## The idea

```
events persist start https://user:pass@host/path
```

The same capture stream `recorder` writes to a file, sent to an endpoint instead. The service split
already supports it: `recorder` drains the sink and owns the destination, so a second destination
kind is a `recorder` change and touches neither `events` nor the kernel.

## The hazard that must be designed for, not discovered

**Credentials in a log.** A URL with a password in it is exactly the kind of string that ends up
echoed into a capture, printed by `events persist status`, and then written to the very file being
shipped. Any design has to answer where the credential lives and how `status` renders it, before
any of it is built.

## Prerequisites

- The network stack is up and proven (TCP/IPv4, DNS), so transport is not the blocker.
- What is missing is a decision on the credential store, and on what happens to the capture when
  the endpoint is unreachable - the answer must not be "buffer without bound" (26.6).

Not scheduled. Recorded so the design conversation starts from here rather than from scratch.
