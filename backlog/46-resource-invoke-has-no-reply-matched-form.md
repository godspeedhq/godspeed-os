# 46 - a delegated resource capability cannot be invoked through a reply-matched call

**Opened:** 2026-09-23
**Status:** OPEN - blocks `gs::cap` for any caller that serves clients
**Found by:** designing `gs::cap` on `feat/stdlib`, before writing it.

## The gap

`CLAUDE.md` 8.2's `CallDeadline` amendment (2026-08-21) records why a bounded request/reply must be
one primitive rather than `send` plus `recv`:

> A plain recv takes whatever is next, and a service that SERVES clients on the endpoint it awaits
> replies on would therefore consume an unrelated client request, fail to match it, and drop it:
> the request lost outright, and the real reply arriving later as an orphan that desynced every
> following exchange.

That fix landed for NAMED PEERS. `ServiceContext::request_with_reply_call` uses `CallDeadline`
(syscall 50) and matches the reply by its sender.

**It did not land for delegated resource capabilities.** `resource_invoke` (syscall 31) is a SEND
that embeds a reply cap; there is no `CallDeadline` form of it. Every file-as-capability invocation
is therefore `resource_invoke` + a plain `recv` - the exact shape 8.2 says loses messages.

## Why it has not bitten yet

The only caller is `services/shell`, and it does this first:

```rust
while ctx.try_recv().is_some() {}   // clear any stale late-reply a prior aborted invoke left behind
```

That drain is safe **only because the shell serves nobody on that endpoint**. In a service that
does, the same line is the bug: it discards live client requests. So the existing caller is not
evidence the pattern is sound, it is evidence that one caller happens to be exempt.

## Why this blocks the standard library

`gs::cap` is meant for ORDINARY SERVICES, not only the shell - a file is a capability (7.10) is one
of the system's north stars, and a library that can only be used by a task with no clients is not
the public interface 22.7 measures. The library cannot copy the shell's drain, and without either
the drain or a reply-matched primitive it inherits a message-losing wait.

So `gs::cap` is NOT STARTED. Building it on this foundation and documenting the hazard in a doc
comment would be shipping the defect with a warning label attached, which is the papering-over
26.7 forbids.

## The fix, and why it is not a small one

A `ResourceInvokeDeadline` - `resource_invoke` with the deadline machinery `CallDeadline` already
has, matching the reply to the embedded reply cap. Same reply-cap semantics, same `call_dequeue`.

**That is a new syscall, and a new syscall is a new kernel responsibility.** Commandment I pins the
surface and the enforcement layer refuses the change until `CLAUDE.md` 8.2 is amended to record it,
exactly as `CallDeadline` itself was. It is mechanism not policy (the kernel learns a deadline, not
what is being awaited), and it is arguably the same amendment finishing its job - 8.2 fixed the
named-peer path and left the resource path on the primitive it had just condemned - but it is the
operator's gate, not a library author's.

## Options, for the record

1. **Add `ResourceInvokeDeadline`.** The real fix. Kernel change, amendment, re-verification on all
   four ports.
2. **Ship `gs::cap` restricted**, documented as usable only by a task that serves no clients on its
   endpoint. Honest, and narrow enough to be a trap the first time someone writes a service.
3. **Leave `gs::cap` unbuilt** and keep file-as-capability a shell-only facility for now. Costs
   nothing and hides nothing.

Recorded rather than chosen: 1 needs the operator, and 2 versus 3 turns on whether a restricted
version is worth more than its trap.
