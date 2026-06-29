# The Registry: why a name service exists (§14.2, §3.11, §26.10)

> Narrative design doc. Explains *why* GodspeedOS has a `registry` service at all -
> the reasoning behind name → capability resolution. The spec (`CLAUDE.md`) is the
> authority; this trails it as a courtesy to readers.

## The fundamental problem: how do you get the *first* capability?

In a capability system, to talk to a service you must hold a **capability** - an
unforgeable token (§7.3) granting authority to its endpoint. You cannot fabricate
one. That creates a bootstrapping question: if service A wants to talk to service B
but does not already hold a cap to B, how does it *ever* get one?

There are only two answers:

1. **Delegation** - someone who already holds the cap hands it to you (a parent or
   the supervisor passes caps at spawn time).
2. **Name lookup** - you ask a known service: *"give me a cap to whatever is called
   `pong`."*

The registry is answer #2: a **rendezvous point** so two services that do not already
share a capability can find each other through an agreed-upon name.

## Why names exist at all: "identity is stable, location is not" (§3.11)

This is the real reason, and it is the heart of the architecture. Services
**restart** - possibly on a different core (§9.2), always with a fresh endpoint and a
new generation, which means **a brand-new capability** (§14.2). The cap you held to
`pong` before its restart is now *dead* (its generation no longer matches; a send
returns `EndpointDead`).

So: how does a client reconnect to `pong` after it restarts? It cannot keep using the
old cap (dead), and it cannot forge a new one (unforgeable). The only thing that
survives the restart unchanged is the **name** `"pong"`. The client asks the registry
*"what is the current cap for `pong`?"* and receives the fresh one - pointing at the
new instance, on whatever core it landed on, which the client never has to know.

> **The registry's core job is to map a stable name → the current capability.**

That is precisely what makes restartability work. Without a name-resolution service, a
service could restart but nobody could reconnect to it - restart would be pointless.
This is invariant 11 (§3.11) made usable: *names are the stable identity; the registry
maps them to the mutable, restart-changing capabilities.*

This is not abstract. The canonical milestone scenario (§23, identity tests 6B/10B)
is exactly it:

```
pong restarts (new core, new endpoint, new cap)
  → ping's old cap to pong returns EndpointDead
  → ping looks "pong" up via the registry → gets a fresh cap (new core)
  → ping resumes; it never learned which core pong moved to
```

## Why a *service*, and not the kernel

The kernel *could* resolve names (and in v1 it still does, as a shortcut - see the
caveat below). But the deeper principle is **§26.10: the kernel is mechanism, not
policy.** A *name* is a policy/human concept - "which endpoint do we agree to call
`pong`." A pure capability kernel (seL4 is the reference design) has **no name service
at all**; it routes only unforgeable tokens, and naming lives in a userspace server.

Keeping name resolution in a userspace service:

- keeps the **kernel name-free and small** - pure mechanism (§3.4, §26.10);
- makes the name service **replaceable and restartable** - it is just a service
  (H11; §14);
- lets naming **policy** evolve later - access control on lookups, namespaces,
  per-tenant views - **without touching the kernel**;
- means applications do name resolution by talking to the **registry, not the
  kernel** - the kernel stays out of that conversation and only routes the IPC.

## How resolution works mechanically (H11)

Only the kernel mints capabilities, so a userspace registry never *fabricates*
authority - it only ever holds and **duplicates** caps it was legitimately given:

- **register(name)** - a service grants the registry a `SEND|GRANT` cap to its own
  endpoint (the self-grant cap minted at spawn). The registry records `name → cap`.
- **lookup(name)** - the registry **derives** a SEND copy of the held cap
  (`DeriveCap`, syscall 29 - non-escalating, §7.3) and grants that copy back to the
  client. It keeps the original, so the next lookup works too.

The client receives a usable SEND cap to the named service. No name ever enters the
kernel via this path; the kernel only sees cap derivation and routing.

## The honest v1 caveat

In v1 the kernel still keeps its own `name → endpoint` map - it needs one to wire
contract-declared `send_peers` caps at spawn time, and that wiring is synchronous and
kernel-internal (it cannot block on a userspace registry mid-spawn). So **today the
registry partly duplicates kernel state**, and its full payoff only lands when naming
is removed from the kernel entirely (spawn-time wiring reworked into explicit
capability delegation - Appendix B.3 / D.3). H11 makes the registry a real,
independent, restartable name authority - the necessary step before the kernel can
shed naming and become the pure mechanism the model intends.

## See also

- `docs/restart.md` - the cap-rebinding / client-reconnect flow.
- `docs/capability.md` - generation mechanism, GRANT, transfer.
- `services/registry/` - the implementation.
- `CLAUDE.md` §6 (TCB), §14.2 (restart), §3.11 (identity over location), §26.10
  (mechanism not policy).
