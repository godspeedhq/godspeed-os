# Example: cap-grant

Transfer authority on GodspeedOS by handing another service a **capability**. There is no flag to
flip and no memory to share: you give a token, and the kernel moves it.

## Purpose

Show, end to end, how one service grants another the right to do something. Authority is a held,
unforgeable capability (§7); to delegate it you transfer the capability itself, only if it carries
the GRANT right, and on success the kernel removes it from your table so authority **moves** rather
than silently duplicating.

## What it demonstrates

The granter side, using only real `ServiceContext` methods:

| Step | Call | What happens |
|------|------|--------------|
| Hold a grantable cap | `ctx.self_grant_handle()` | our own SEND\|GRANT cap to our endpoint (minted from the contract at spawn) - the cap others use to call us back |
| Make a copy to give | `ctx.derive_cap(self_cap)` | a derived cap; rights can only narrow, never widen (§7.3) |
| Find the peer | `ctx.acquire_send_cap("receiver")` | a SEND cap to the service we will grant to |
| Transfer it | `ctx.send_with_cap_by_handle(receiver, gift, &note)` | the kernel checks the cap carries GRANT, then **moves** it into the receiver's table and removes it from ours (§7.6, §8.5) |

The receiver side, in its own service, completes the transfer:

```rust
let _carrier = ctx.recv();                 // the message that carried the cap
if let Some(granted) = ctx.take_pending_cap() {
    // `granted` now lives in OUR table; use it to call the granter back.
    let _ = ctx.send_by_handle(granted, &Message::from_bytes(b"thanks"));
}
```

## Why it is built this way (the Commandments)

- **Commandment VII (no ambient authority).** The receiver gains the ability to call us back only
  because it now *holds* a capability we transferred. No identity, ancestry, or "it is a trusted
  service" grants authority; only the token does. The GRANT right is what makes the transfer legal,
  and on success the kernel takes the cap out of our table, so the same authority cannot quietly
  exist in two places. *(COMMANDMENTS.md VII; CLAUDE.md §7.4, §7.6, §8.5, Invariant 1.)*
- **Commandment VI (no shared mutable state).** We pass a *capability*, never a pointer into shared
  memory. The receiver cannot reach into our address space, and we cannot reach into theirs;
  delegated authority travels as a typed token through IPC, which is the only channel between
  services. *(COMMANDMENTS.md VI; Invariant 2, §2.5.)*
- **Commandment IX (plan for recovery).** We `derive_cap` a *copy* to give away and keep the
  original, so that if the receiver restarts we can mint and re-grant without having lost our own
  authority. A delegator that gave away its only handle could never recover. *(COMMANDMENTS.md IX;
  CLAUDE.md §14.2.)*
- **Commandment X (place complexity where it belongs).** Deciding *who* may do *what* is policy, and
  policy lives in services. The kernel only enforces the mechanism (the GRANT check, the move). A
  capability broker - a service that decides which child gets which authority - is exactly this
  example grown up. *(COMMANDMENTS.md X; §26.10.)*

## The contract, annotated

```toml
[capabilities]
ipc_receive = ["cap-grant"]   # our own endpoint; the cap we give away points here
ipc_send    = ["receiver"]    # the peer we transfer the grantable cap to
log_write   = true
```

Everything the service can do is on this list and nowhere else (Commandment VII). It owns an
endpoint (so it has a SEND\|GRANT cap to itself to hand out) and a SEND cap to `receiver`.

## What you must NOT do

- **Do not stash the cap in a `static` to "share" it.** That is shared mutable state and invisible
  coupling - it breaks **Commandment VI**. Transfer the capability through IPC; that *is* the sharing
  mechanism.
- **Do not send a capability that lacks GRANT.** The kernel refuses with `CapNotGrantable` (§7.7) and
  keeps the cap in your table - authority cannot be widened or leaked by accident. Trying to work
  around it breaks **Commandment VII**.
- **Do not assume the receiver acted on the cap because the send returned `Ok`.** A successful send
  means *queued*, not *processed* (§8.6). If you need confirmation, the receiver must acknowledge
  explicitly - waiting on truth, not time (**Commandment VIII**).

## How to adapt this

A capability broker (a shell, a supervisor-like spawner, a connection manager) follows this exact
shape: mint or hold a cap, `derive_cap` a narrowed copy, and `send_with_cap_by_handle` it to the
child you are authorizing. Declare the endpoints you broker in the contract; let the kernel enforce
GRANT. To receive a delegated cap, pair `ctx.recv()` with `ctx.take_pending_cap()`.

## See also

- **Commandments VI, VII, IX, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §7.4 (the GRANT right), §7.6 (the transfer rule), §8.5 (embedded capabilities).
- `examples/00-hello` (the service skeleton), `examples/ping` (IPC + restart recovery).
- `examples/resource-server` (planned) - minting *new* resource capabilities a service owns (the
  "a file is a capability" mechanism, §7.10).
