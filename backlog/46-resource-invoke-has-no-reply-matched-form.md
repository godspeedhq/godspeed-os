# 46 - resource capability invocation has no bounded, correlated reply

**Opened:** 2026-09-23
**Status:** OPEN - architectural question RECORDED, deliberately not acted on
**Found by:** designing `gs::cap` on `feat/stdlib`, before writing it.

> **`feat/stdlib` CONTAINS NO KERNEL CHANGE, and none is proposed for it.** Verified rather than
> asserted: `git diff main...HEAD -- kernel/` and `-- sdk/` are both empty. This entry is a finding
> about an existing mechanism, written down so that a future branch can pick it up with the analysis
> already done. It is not a design for this one. Anything acting on it belongs in its own branch,
> with its own review and its own hardware re-verification.

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

---

## The governing constraint, and what it settles

> The stdlib's job is to make existing Godspeed functionality pleasant and safe to consume. It isn't
> supposed to create functionality that the OS doesn't already possess. - the operator

Under that rule this entry's conclusion is short: **`gs::cap` correctly does not exist, because the
OS does not currently possess safe general resource invocation.** A standard library cannot offer a
guarantee the system underneath it does not make. The library's job ended when the gap was found;
filling it would have been the library manufacturing a capability rather than re-serving one.

Everything below is therefore a NOTE FOR WHOEVER PICKS UP THE MECHANISM, not a plan. Its value is
that it rules out the cheap rungs with evidence, so the search is not repeated.

## Triage against the operator's ladder (2026-09-23)

The operator supplied a five-rung ladder: A stdlib abstraction wrong, B userspace composed wrong,
C SDK does not expose an existing kernel mechanism, D accidental limitation in the mechanism,
E genuinely missing MISCIS primitive. Walked against the code, not reasoned about abstractly.

### A - is the `gs::cap` abstraction wrong? NO

The hole is reachable with no standard library in the picture: `examples/holder` hangs, and
`services/shell` uses the same shape on its socket path. An abstraction cannot be the cause of a
defect in the mechanism it would wrap. Redesigning `gs::cap` changes who trips over it, not whether
it is there.

### B - is existing userspace machinery sufficient, composed wrongly? NO

The obvious composition is "await the reply on a DEDICATED endpoint that carries no client traffic",
which would make a plain `recv` correlated by construction. **It is not available: a task owns
exactly one endpoint.** There is no endpoint-create syscall - `EndpointCreate` appears nowhere in
`kernel/src/syscall/dispatch.rs`, and the SDK's only endpoint-shaped calls are introspection plus
`self_grant_handle`, a grant to the task's own single endpoint.

The second composition - correlate with a caller-supplied tag - fails for the reason recorded above:
`recv` has already CONSUMED the wrong message before the tag can be read, and there is no requeue.

### C - does the kernel already support it, with the SDK failing to expose it? NO

Two independent blockers, both checked:

1. `do_call` (syscall 41/50) validates the target with `Rights::SEND` and then does
   `EndpointId(target_cap.resource_id.0)` - it treats the target cap's resource id AS an endpoint
   id. A delegated resource cap's id is not an endpoint id; it must go through
   `delegated::owner_of()`. Passing a resource cap to syscall 50 does not route to the owner.
2. The badge is what makes a resource invocation meaningful (7.10), and it is set in exactly one
   place: `handle_resource_invoke` sets `msg.badge_id` / `msg.badge_right` AFTER validating the cap.
   `do_call` sets neither. An SDK cannot fabricate it - that is precisely the unforgeability the
   badge exists for.

So there is no existing kernel entry point that both badges the message and awaits the reply. The
SDK is not withholding anything.

### D - accidental limitation in the existing mechanism? YES. This is the rung.

The machinery `resource_invoke` needs is **already present, already generic, and keyed on a value
`resource_invoke` already computes and then throws away**:

| what is needed | what exists | keyed on |
|---|---|---|
| selective dequeue, leaving other traffic queued | `MessageQueue::dequeue_matching(sender_ep)` - takes only the matching message, shifts the rest back, FIFO preserved | an endpoint id |
| bounded wait | `do_call`'s deadline loop, same shape as `handle_recv_timeout` | - |
| wake on the replier's death (`ReplyDead`, 8.6) | `set_call_await(caller_slot, target: EndpointId)` | an endpoint id |

And `handle_resource_invoke` step 1 already has that endpoint:

```rust
let owner = match delegated::owner_of(file_cap.resource_id) {
    Some(o) => EndpointId(o),   // <- exactly the key both mechanisms want
```

It uses `owner` to enqueue, and then discards it. Nothing about `dequeue_matching` or
`set_call_await` is specific to a named peer; both are generic over "the endpoint that will reply".

**So the limitation is accidental, not intentional.** `resource_invoke` was written as a bare send
and simply never received the completion the named-peer path got in the 8.2 `CallDeadline` amendment
(2026-08-21). Nothing in 7.10, 8.2 or 8.6 states the "endpoint otherwise idle" contract it currently
relies on, and `examples/holder` does not honour it knowingly.

### E - a genuinely missing primitive in C/I of MISCIS? NO

Nothing is missing. Correlated selective dequeue exists, bounded waiting exists, the death-wake
registration exists, and all three are already generic over an endpoint id. This is not a capability
or IPC semantic the kernel lacks; it is one existing syscall not using machinery that is already
there.

## What this changes about the cost

The earlier entry implied a new primitive. It is not one. `do_call` and `handle_resource_invoke`
differ in exactly **one** respect - how the target endpoint is derived (a SEND-validated endpoint cap
versus `delegated::owner_of`), plus setting the badge. Everything from "embed the reply cap" onward
is identical and already written.

The shape of a remedy is therefore a **shared body with two target resolutions**, not a parallel
mechanism. That is a much smaller and much better-understood change than "add a syscall".

## Still not being done

Recorded, not acted on, per the operator's position: a kernel edit costs re-verification across four
ports whatever its size, and the finding creates no urgency. What the triage buys is that whenever it
IS taken up, the question is already answered - it is rung D, the remedy is a completion rather than
an addition, and the two rungs that would have kept it out of the kernel (B and C) have been tested
and ruled out with evidence rather than assumed.
