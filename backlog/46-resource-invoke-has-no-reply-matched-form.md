# 46 - resource capability invocation has no bounded, correlated reply

**Opened:** 2026-09-23
**Status:** OPEN - architectural question RECORDED, deliberately not acted on
**Found by:** designing `gs::cap` on `feat/stdlib`, before writing it.

> **This entry was rewritten the day it was opened.** The first version concluded "a new syscall is
> a new kernel responsibility, so this is the operator's gate". That reasoning was wrong and the
> operator corrected it: **syscall count is not responsibility count.** A primitive that completes
> the semantics of a responsibility the kernel ALREADY owns is not kernel growth. The question is
> not "does this add a syscall", it is "does an existing MISCIS mechanism have an incomplete
> semantic". The original framing is left recorded here rather than quietly replaced, because the
> error - letting a feature's needs decide a kernel question - is the one worth not repeating.

## What `resource_invoke` guarantees today

**Delivery, and nothing else.** Syscall 31 validates the cap holds `right`, routes the message to
the owning service badged with the resource id, and embeds a reply cap. `Ok(())` means it was
routed. The kernel then forgets the exchange.

The caller waits with a plain `recv` on its own endpoint.

## Two holes, and both are generic

### 1. No reply correlation

A plain `recv` takes whatever is next. A caller that also SERVES clients on that endpoint will
dequeue a client request, fail to match it, and have nowhere to put it back - the request is lost,
and the real reply arrives later as an orphan that desyncs every exchange after it.

That is verbatim the failure `CLAUDE.md` 8.2's `CallDeadline` amendment was written to remove. That
amendment fixed the NAMED-PEER path (`request_with_reply_call` matches the reply by its sender) and
left the RESOURCE path on the primitive it had just condemned.

**A service-layer tag cannot fix this.** Echoing a caller-supplied tag would let the caller
RECOGNISE a wrong message, but `recv` has already CONSUMED it and there is no requeue. Selective
dequeue is inherently kernel-side - it is what `call_dequeue` does.

### 2. No `ReplyDead`

8.6's reply-side death-wake reaches a caller blocked in a synchronous `Call`, because the kernel
knows which endpoint that caller awaits. `resource_invoke` is a SEND, so the kernel is never told a
reply is awaited, and the owner dying does not wake the caller.

`examples/holder` - the published worked example of this mechanism - is exactly this:

```rust
Ok(())  => Ok(ctx.recv()),   // routed: block for the owner's reply on our endpoint
```

An unbounded `recv`. If `resource-server` dies after receiving the invocation and before replying,
`holder` hangs forever. **That is Commandment VIII broken in the example that teaches the
mechanism**, and 8.6's table has a row for precisely this case on the `Call` path.

## The MISCIS test: would this hole exist if `gs::cap` never existed?

**Yes, and it already does.** The evidence predates the standard library entirely:

- **Three independent issuers** mint delegated resource caps: `fs` (files), `net-stack` (TCP
  connection and listener caps), and `shell`. The mechanism was never file-specific; 7.10 is written
  in terms of an opaque `ResourceId` whose meaning only the owner knows.
- **`examples/resource-server` + `examples/holder`** are a worked pair that mention no filesystem at
  all, and `holder` carries hole 2 in three words of code.
- **`services/shell` hits it on the socket path too** (`sock` invokes connection caps), not only on
  the file path.

So this is not stdlib pressure. `gs::cap` did not create the problem; it was the first caller that
could not look away from it, because a library cannot make the assumption the shell makes.

## The assumption, named

The shell opens every resource invocation by draining its queue:

```rust
while ctx.try_recv().is_some() { .. }
```

That is correct **only because the shell serves nobody on that endpoint**. So the current contract
of `resource_invoke` is, unwritten:

> safe to use only from a task whose endpoint carries no traffic but this reply

Is that an intentional contract or an accidental limitation? Nothing in 7.10, 8.2 or 8.6 states it,
`examples/holder` does not honour it knowingly, and the mechanism is offered generically to any
service. **It reads as accidental.** That is the finding.

## The genericity test

A bounded, correlated resource invocation can be specified without naming files, GSFS, `gs::cap` or
filesystem handles:

> Invoke a resource capability and wait no longer than the supplied deadline for the reply
> associated with that invocation; wake if the owner dies first.

That is capability invocation + bounded waiting + reply correlation + IPC - four things the kernel
already owns under MISCIS. On that reading it would COMPLETE an existing responsibility rather than
add one, exactly as `CallDeadline` did for the named-peer path ("it is `call` with a bound, not a
new capability").

## What is NOT being done, and why

**No kernel change. No `ResourceInvokeDeadline`.** The operator's position, and it is the right one:
the fact that the stdlib exposed this creates no urgency to change the kernel, and a mechanism must
justify itself independently of the feature that happened to reveal it.

`gs::cap` stays unbuilt. Nothing is lost - file-as-capability works today for the caller it has.

## What a future investigation should settle

Independently of `gs::cap`, and from first principles:

1. Is the "endpoint otherwise idle" assumption intentional? If so it belongs in 7.10 in writing, and
   `examples/holder` needs a deadline and a comment saying why it is allowed to block.
2. Is hole 2 (`ReplyDead` not reaching resource invocation) separable from hole 1? It may be the
   smaller and more clearly-owed of the two: 8.6 already promises a caller is never left hanging by
   a dead replier, and this path does not deliver that promise.
3. Would a bounded correlated form be reachable by REUSING `CallDeadline`'s machinery rather than
   adding a parallel one - the same `call_dequeue`, keyed on the reply cap already embedded?
4. Is a third option available: does a resource-cap holder even need to await on its own general
   endpoint, or could the reply cap name a dedicated one?

Acceptable outcomes include concluding that `resource_invoke` is intentionally narrow and its
contract sufficient - in which case the contract gets written down and `examples/holder` gets fixed
or explained, and that alone is worth the investigation.
