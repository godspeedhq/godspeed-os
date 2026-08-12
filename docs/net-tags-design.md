# Design Spec: Correlation Tags Between net-stack and nic-driver

> **Status:** Direction agreed (2026-08-11), **not built**. Written after the idle-link tick was
> reverted (`aa569bcc`) for the bug this fixes. Three phases, each independently testable; do them in
> order and verify on hardware between each.
>
> **Author intent:** net-stack serves clients and receives nic-driver replies on **one untagged
> endpoint**, so anything it asks the driver outside of serving a request can consume a client's
> message instead. That is not a tuning problem; it is a missing correlation, and until it is fixed
> net-stack cannot do ANY background work - no link watching, no periodic re-sync, no staying
> responsive during the DHCP dance.

---

## 1. The bug this fixes

net-stack owns exactly one endpoint. Two unrelated kinds of message arrive on it:

- **client requests** - the shell asking for `net` status, `ping`, DNS, sockets;
- **replies from nic-driver** - answers to net-stack's own questions (link state, TX, RX).

Nothing distinguishes them. When net-stack asks the driver something it calls `nic_req`, which
delegates to the SDK's `request_with_reply_deadline_outcome`, whose wait loop is:

```rust
if let Some(r) = self.try_recv() { return DeadlineOutcome::Reply(r); }
```

It returns **whatever lands next**. So a client request arriving in that window is:

1. consumed as though it were the driver's reply, and misparsed (a `net` request read as a link
   status answers "link up" from `[0] != 0`);
2. never served - the client waits out its own deadline and reports "net-stack not responding";
3. worse, its **reply capability stays on the kernel's pending FIFO**, so the *next* reply net-stack
   sends goes to the wrong requester.

Found independently by two audits (userspace A10-1, documentation A5-2) after an idle tick made the
window open once a second, forever. The tick was reverted; the window still exists whenever net-stack
talks to the driver while a client is active.

**Precedent:** `fs` had exactly this and fixed it exactly this way. Its replies were matched by arrival
order, which produced the "run `ls` twice and it is out of step" desync; the fix was a correlation byte
at offset 0 of both request and reply (see `project_fs_reply_correlation`, and the `tag` handling in
`services/fs/src/main.rs`). This spec is that pattern applied one layer down.

**Rejected alternative - a second endpoint.** Structurally cleaner (client traffic and driver traffic
in different mailboxes, impossible to confuse) but **not available**: there is no `CreateEndpoint`
syscall, a service's receive endpoint is minted at spawn from its contract, and `ServiceContext`
carries a single `recv_slot`. It would take a new kernel primitive, an SDK change to select a mailbox,
and a contract change, to solve one service's problem. Tags need none of that and there is a working
example in the tree.

---

## 2. Wire format

One byte at **offset 0** of every net-stack -> nic-driver request and every nic-driver -> net-stack
reply. Every existing field shifts up by one.

```
request : [tag, op, ...args]          (was [op, ...args])
reply   : [tag, ...payload]           (was [...payload])
```

- The tag is **echoed**, never interpreted: nic-driver copies request[0] into reply[0] and does
  nothing else with it. The driver needs no state and no memory of outstanding requests.
- `tag = 0` is **reserved** and means "untagged". A reply carrying 0 is from an instance that predates
  this change, or from something that is not nic-driver; treat it as unmatched.
- The counter is per net-stack instance, incremented per request, wrapping. Wrapping is safe: the
  window that matters is one outstanding request, so only the current tag is ever compared.

Both services must ship together. There is no compatibility mode - they are spawned by the same
supervisor from the same image, so a mixed pair cannot occur outside a partial build.

---

## 3. Phases

### Phase 1 - tag the protocol

Add the byte on both sides, one op at a time, keeping the two in step.

- `services/nic-driver/src/genet.rs` (`serve`) and the other backends' serve loops: read the tag from
  `p[0]`, shift every existing parse by one, echo the tag into `out[0]`.
- `services/net-stack/src/main.rs`: every place that builds a request for `nic_req` and every place
  that parses its reply.

Ops to convert (grep `nic_req(` for the full list): status/link, TX frame, RX frame, and the ARM
USB-net variants if that backend is in the build.

**Verify after each op**, not at the end: `net`, `ping`, and a DHCP configure must still work. A wrong
shift shows up as a plausible-looking wrong value, not a crash - the exact failure that is cheap to
find one op at a time and expensive to find after six.

### Phase 2 - a tag-aware await in net-stack

`nic_req` stops using `request_with_reply_deadline_outcome` (it cannot know about tags; it is generic
and shared with every other caller). net-stack grows its own send-and-await:

```
send the request carrying tag T
loop until deadline:
    m = try_recv()
    if m is None: yield/sleep, continue
    if m[0] == T: return Some(m)          // our reply
    else:         stash(m)                // NOT ours - see phase 3
return None                                // deadline; caller already handles this
```

After phase 2 alone, `stash` may simply **drop** the message. That is already a strict improvement:
a mis-served client with a corrupt reply becomes a client that times out and retries - a defined,
loud, recoverable outcome. Ship it here if phase 3 has to wait.

### Phase 3 - the bounded stash

Dropping loses work, so keep what is not ours and serve it after.

- A small fixed array (4-8 entries) of pending client messages, owned by the serve loop, **not** a
  heap or a growable buffer (§26.6.1).
- The serve loop drains the stash **before** calling `recv()`.
- **On overflow, drop the OLDEST and say so once.** A bound that is silently exceeded is the
  unbounded-behaviour case §26.6 forbids; a bound that is loud is a bound.
- A stashed message carries a reply cap. Reclaim it if the message is dropped, or the slot leaks
  (§8.5, and the class behind `1ecfd98e`).

---

## 4. What this unblocks

- **The idle link tick** (cable INFO on plug/unplug without being asked) - reverted for exactly this
  bug; see the note at the revert site in `net-stack/src/main.rs`.
- **Staying responsive during the DHCP dance** - net-stack currently cannot serve a client while
  configuring, because the dance runs inline in the request path.
- **Any periodic work at all**, including the re-sync a future time service would ask for.

---

## 5. Test plan

Per phase, on hardware (QEMU cannot reproduce the Pi 4's NIC):

1. `net` and `ping` with the cable in - the ordinary path still works.
2. Boot cable-out, plug in, confirm DHCP configures (this is the path that regressed twice already).
3. **The bug itself:** run a continuous `ping` and, while it is running, make net-stack talk to the
   driver (a second `net` from another prompt, or plug/unplug the cable). Before the fix this can
   misparse; after it, both complete correctly.
4. `chaos max-carnage` - net-stack and nic-driver are both restartable, and a tag must not survive a
   restart in a way that matches a stale reply. A respawned net-stack starts its counter fresh; a
   reply from before the restart carries a tag it will not match, which is the correct outcome.

---

## 6. Notes for whoever builds it

- Do **not** add a tick, a background poll, or any other unsolicited driver traffic before phase 2
  lands. That is the change that turned a latent race into a once-a-second one.
- `fs` is the reference for the pattern, including what it does with a message that is not its reply.
  Read `services/fs/src/main.rs` and the shell's `drain_stale_fs_replies` before designing the stash.
- The tag proves a reply is *for this request*. It does not prove the reply is *correct* - keep the
  existing length and shape checks on every parse.
