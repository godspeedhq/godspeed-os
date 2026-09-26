# Example: resource-server

> **Real and QEMU-proven by `osdev test resource-server`** - it spawns resource-server with its client
> `examples/holder`. resource-server mints a resource it owns, narrows a READ-ONLY copy of the cap, and
> grants it to holder; holder then proves all three §7.3 properties and logs `holder: read OK` (use),
> `holder: write denied (non-escalation)`, and `holder: revoked (CapRevoked)`. Run it yourself to
> re-confirm.

MINT a brand-new capability for a resource your service **owns** - the mechanism that makes "a file
is a capability" literally true (§7.10). The kernel mints, validates, routes, and revokes the cap
exactly as it does an endpoint cap, yet it never learns what the resource *means*. Only your service
does.

## Purpose

Teach the third capability operation the examples were missing. Capability **use** is in
`examples/00-hello` + `examples/ping`; capability **transfer** is in `examples/cap-grant`; capability
**mint** is here. A delegated resource capability lets a service define a resource whose meaning is
its own (a file, a socket, a database row) while the kernel handles the unforgeable plumbing. This is
exactly how `services/fs` turns a file into a real kernel capability.

## What it demonstrates

The owner side, using only real `ServiceContext` methods:

| Step | Call | What happens |
|------|------|--------------|
| Mint | `gs::resource::mint(&ctx, gs::cap::READ \| gs::cap::GRANT)` | the kernel allocates a fresh opaque `ResourceId` at generation 0, records THIS service as its owner, and mints a real cap for it - returns `(resource_id, cap)` |
| Make a copy to give | `gs::cap::duplicate(&ctx, cap)` | a derived copy; rights can only narrow, never widen (§7.3) |
| Hand it to a client | `gs::ipc::send_granting(&ctx, client, copy, &note)` | the kernel **moves** the copy into the client's table (§7.6, §8.5); we drop our own cap and serve via the badge |
| Serve a use | `gs::resource::last_badge(&ctx)` -> `(resource_id, right)` | a holder's `resource_invoke` is kernel-validated and routed here, **badged** with which resource and the right the kernel already checked |
| Enforce non-escalation | `op <= right` | a READ-validated cap must never drive a WRITE - the owner's matching check (§7.3) |
| Revoke | `gs::resource::revoke(&ctx, resource_id)` | a generation bump makes EVERY outstanding cap to the resource go stale: next use is `CapRevoked` (§7.5) |

A holder USES the cap with `ctx.resource_invoke(cap, right, reply, &msg)`; the kernel validates it and
routes it here badged. `fs` does exactly this: `Open` mints a file cap, an invoke reads/writes the
file, and delete/close revokes it.

## Minting is gated (this is the point)

`resource_mint` is **not** ambient. It requires a `RESOURCE_MINT` authority, which arrives in the
SPAWN REQUEST and is refused unless the SUPERVISOR itself may delegate it
(`SUPERVISOR_DELEGATABLE`) - so a spawner passes on what it has and never mints authority into
existence (§3.1). It IS declarable in the contract (`resource_mint = true`, and this example's own
contract carries it), as the build-time statement of that grant, reconciled by
`IV-contract-authority`.

> **Corrected 2026-09-26.** This said RESOURCE_MINT is "granted by name inside the kernel" to
> minters "(today: `fs`)" and is "deliberately NOT a contract capability field (not in the schema)".
> The schema has `resource_mint`, this example's contract declares it, and the `service_hw` arm the
> claim rested on has no arms left. Three places in this file said it and the contract said it a
> fourth time, nine lines below its own `resource_mint = true`. The kernel grants it
to `resource-server` too - but ONLY in the `resource-test` build (`osdev test resource-server`), the
only build that spawns this service - so the grant is effectively test-only, exactly as
`reply-server`/`asker` are spawned only in the `reply-test` build (the kernel always embeds them; the
supervisor feature gates whether they run). Built that way, the grant is present and the example runs
for real against its client `examples/holder`. Built standalone (a plain `cargo build` of this crate
alone, never spawned), the grant is absent, so `ctx.resource_mint` returns `None` and the service logs
and idles - loud, bounded degradation (Commandment V); `fs` is the production proof.

## Why it is built this way (the Commandments)

- **Commandment VII (minting is gated, never ambient).** Issuing a new authority is itself a
  privileged act. `resource_mint` needs the `RESOURCE_MINT` capability, granted by name in the kernel
  to legitimate minters - it is not in any contract and cannot be requested. No service mints
  resources because it feels entitled to; only because the kernel granted it that authority, exactly
  as a driver reaches its BAR only because the kernel mapped it. *(COMMANDMENTS.md VII; CLAUDE.md
  §7.10, §3.1, Invariant 1.)*
- **Commandment III (the service owns the resource's meaning).** The kernel tracks only an opaque
  `ResourceId` and its owning endpoint - nothing more. It never learns what the resource *is*; this
  service alone maps `ResourceId -> meaning` (for `fs`, `ResourceId -> file`). That is why "a file is a
  capability" is literally true and not an analogy: the kernel mints and routes a real cap while
  remaining entirely ignorant of files (§4.4). *(COMMANDMENTS.md III; CLAUDE.md §7.10, §4.4.)*
- **Commandment X (mechanism in the kernel, policy in the service).** The kernel provides the
  mechanism - mint, validate, route, badge, revoke - and nothing else. What the resource means, which
  ops are legal on it, and when to revoke it are policy, and policy lives here in the service. The
  kernel does not interpret intent; it enforces the cap. *(COMMANDMENTS.md X; CLAUDE.md §26.10, §4.4.)*

## The three capability properties a delegated resource cap keeps

A cap minted this way is a **genuine** capability - every §7.3 property holds, identically to an
endpoint cap:

- **Unforgeable.** Only the kernel constructs it. A client cannot fabricate a `ResourceId` and act on
  it; a random handle is `CapNotHeld` / `CapInvalid`. The badge that proves a real invocation is set
  only by the kernel after the cap check, so it cannot be faked over an ordinary `send`.
- **Non-escalating (`op <= right`).** Rights narrow on transfer, never widen (`gs::cap::duplicate`). A
  READ-only copy cannot write: the kernel rejects a WRITE invocation of a READ cap with
  `CapInsufficientRights`, and the owner re-checks `op <= right` on the validated badge. Two layers,
  same rule.
- **Revocable (generation bump).** `resource_revoke` bumps the resource's generation, making every
  outstanding cap to it stale at once. The holder's next use returns `CapRevoked` (§7.5). `fs` revokes
  on delete/close; nothing escapes.

## What you must NOT do

- **Do not assume declaring `resource_mint` GRANTS it.** The contract is a build-time declaration;
  the authority arrives in the spawn request and is refused unless the supervisor may delegate it.
  Declaring it and finding the call denied means the spawn row, not the contract, is what to look at
  (§13.6 exists because a model got this exactly backwards). **Commandment VII.**
- **Do not teach the kernel what your resource means.** Keep `ResourceId -> meaning` in the service.
  The moment the kernel knows it is a file, the anti-scope (§4.4) is broken and **Commandment III**
  with it.
- **Do not skip the `op <= right` check because the kernel already validated the cap.** The kernel
  checks the cap holds the invoked right; the owner must still refuse an operation that needs more than
  the validated right. Both checks are load-bearing (non-escalation, §7.3).
- **Do not assume the client acted because the grant send returned `Ok`.** A successful send means
  *queued*, not *processed* (§8.6) - wait on truth, not time (**Commandment VIII**).

## How to adapt this

To serve any resource-as-capability: have the supervisor's spawn row grant your service
`RESOURCE_MINT` (and declare it in your contract so the two agree), `gs::resource::mint` a resource
per client, hand each client a `gs::cap::duplicate` copy, then serve invocations off
`gs::resource::last_badge` - resolving the `ResourceId` to your own meaning, enforcing `op <= right`, and
`resource_revoke`-ing when the resource goes away. `services/fs` is this exact shape, fully grown.

## See also

- **`examples/holder`** - the client half this server grants to, spawned next to it by
  `osdev test resource-server`. It proves use / non-escalation / revoke against the granted cap.
- `services/fs` - the real resource server: a file is a delegated resource cap, minted on `Open`,
  served via the badge, revoked on delete/close.
- The shell `fcap <file>` command + **CLAUDE.md §22 Test 14** (file-is-a-capability) - proves every
  property above end to end (`osdev test file-cap`, 9/9; hardware-validated on the T630).
- `examples/cap-grant` - capability **transfer** (the operation before this one).
- `examples/e1000` - the by-name kernel grant this example needs for `RESOURCE_MINT`, shown for a NIC
  BAR; `examples/driver-skeleton` - the compilable-template-without-a-kernel-hook pattern.
- **Commandments III, VII, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §7.10 (delegated resource capabilities), §7.3 (cap properties), §4.4 (the kernel knows
  no files), §26.10 (mechanism vs policy).
