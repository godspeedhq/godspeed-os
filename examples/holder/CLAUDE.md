# Example: holder

> **Verified by `osdev test resource-server`** - holder is the client that proves a delegated
> resource capability is a *genuine* capability. It receives a READ-ONLY cap minted and granted by
> `examples/resource-server` and exercises all three §7.3 properties end to end, logging a receipt for
> each: `holder: read OK` (use), `holder: write denied (non-escalation)`, `holder: revoked
> (CapRevoked)`. Run it yourself to re-confirm.

The **use-side** of "a file is a capability" (§7.10) - the client half of `examples/resource-server`,
exactly as `ping` is the client to `pong` and `asker` is to `reply-server`. resource-server MINTs a
resource it owns and GRANTs holder a *narrowed, read-only* copy of the cap; holder is what turns that
into a real, exercised proof that the copy behaves like every other capability in the system.

## Purpose

Show that a delegated resource cap (§7.10) keeps the three properties that distinguish a **capability**
from a mere service-level token (§7.3), from the holder's point of view:

| Property | What holder does | Receipt |
|----------|------------------|---------|
| **Use** | `resource_invoke(cap, READ, …)` -> owner serves it | `holder: read OK` |
| **Non-escalation** | `resource_invoke(cap, WRITE, …)` on a READ-ONLY cap -> the KERNEL refuses (`CapInsufficientRights`) | `holder: write denied (non-escalation)` |
| **Revocable** | ask the owner to revoke, then invoke again -> the cap is stale (`CapRevoked`) | `holder: revoked (CapRevoked)` |

## What it demonstrates

The client side, using only real `ServiceContext` methods:

| Step | Call | What happens |
|------|------|--------------|
| Receive the grant | `ctx.recv()` + `ctx.take_pending_cap()` | the kernel moved the granted cap into holder's table (§7.6, §8.5); this hands back its slot |
| Use it | `ctx.resource_invoke(cap, RIGHT_READ, reply, &op)` | the kernel validates the cap holds READ and routes the op to the OWNER, badged - holder never names the owner |
| Over-reach (denied) | `ctx.resource_invoke(cap, RIGHT_WRITE, …)` | the READ-ONLY cap lacks WRITE -> the kernel returns `CapInsufficientRights`; the request never reaches the owner |
| Trigger revoke | `ctx.resource_invoke(cap, RIGHT_READ, reply, &[OP_CLOSE])` | the owner alone revokes what it owns (a generation bump, §7.5) |
| Use after revoke | `ctx.resource_invoke(cap, RIGHT_READ, …)` | the cap is now stale -> `CapRevoked` |

The reply cap holder embeds per invoke is a SEND copy of its OWN endpoint (`derive_cap` of
`self_grant_handle`) - the only channel the owner has to answer it. This is the same dance the shell's
`fcap` command uses against `fs` (`services/shell`, `fc_invoke`); holder is the example-sized version.

## Why it is built this way (the Commandments)

- **Commandment VII (no ambient authority).** holder declares NO `ipc_send` and names no one. It acts
  *only* through the capability it was granted; a `resource_invoke` is routed by the kernel to the
  owner *by the cap itself*. A read-only cap that the kernel refuses to let write is non-escalation
  made mechanical - rights cannot widen (§7.3). *(COMMANDMENTS.md VII; CLAUDE.md §7.3, §7.10, §3.1.)*
- **Commandment VIII (wait on truth, not time).** holder blocks for the owner's *reply* - the truth
  the op happened - never a fixed sleep. A denial or a revoke is a returned error it *reads*, not a
  timeout it guesses; a successful invoke means *routed*, not *processed* (§8.6). *(COMMANDMENTS.md
  VIII; CLAUDE.md §8.6, §7.5.)*
- **Commandment IX (assume you will be killed; fail loud).** holder assumes the granted cap can be
  pulled out from under it. After the owner revokes, the next use fails *loudly* with `CapRevoked`
  rather than silently succeeding on stale authority - exactly what a client must tolerate when an
  `fs` file cap is revoked on delete (§7.5, §26.7). *(COMMANDMENTS.md IX; CLAUDE.md §14.3.)*
- **Commandment X (place complexity where it belongs).** The resource's *meaning* is policy in
  resource-server; the kernel only mints, validates, routes, and revokes the cap, and never learns
  what it is (§4.4). holder just uses it. *(COMMANDMENTS.md X; CLAUDE.md §26.10, §4.4.)*

**Cross-cutting: Commandment II (love Chaos).** holder reads every outcome as a result, never a
panic: a denied write and a revoked read are *expected* answers it logs and moves past - the same
survive-the-failure discipline every example owes `chaos max-carnage`.

## What you must NOT do

- **Do not `recv` after an invoke the kernel rejected.** A rejected `resource_invoke` (denied or
  revoked) never reaches the owner, so no reply is coming - blocking on `recv` would hang forever.
  holder recvs only on `Ok`. *(Commandment VIII.)*
- **Do not treat the write-denial as an arbitrary refusal.** It is the *kernel* refusing to widen a
  read-only cap (`CapInsufficientRights`), not the owner choosing to say no. The cap genuinely cannot
  write - that is the §7.3 property under test.
- **Do not reach the owner by name or a hardcoded endpoint.** holder holds no send authority and
  names no one; it acts only through the granted cap. Authority is by capability, not ancestry.
  *(Commandment VII.)*
- **Do not assume a leaked reply slot is free.** On a rejected invoke holder reclaims the per-invoke
  reply cap (`remove_cap`) - the kernel did not consume it, so dropping it would leak a slot (§26.6).

## How to adapt this

This is the skeleton of any client that is *granted* a capability to a resource it does not own:
receive the cap (`take_pending_cap`), use it with `resource_invoke` (deriving a reply cap from your
own endpoint), and be ready for `CapInsufficientRights` (you tried an op past your rights) and
`CapRevoked`/`EndpointDead` (the owner took it away) as ordinary results. `services/shell`'s `fcap`
against `services/fs` is this exact shape, fully grown.

## See also

- **`examples/resource-server`** - the owner/mint half this client exercises (read it first).
- `services/fs` + the shell `fcap` command (**CLAUDE.md §22 Test 14**) - the production resource
  server and the same three properties proven against a real file (`osdev test file-cap`, 9/9).
- `examples/cap-grant` - capability **transfer** (the operation that *moves* the cap to holder).
- **Commandments VII, VIII, IX, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §7.10 (delegated resource capabilities), §7.3 (cap properties), §7.5 (generations /
  revocation), §8.5 (embedded capabilities), §4.4 (the kernel knows no resources' meaning).
