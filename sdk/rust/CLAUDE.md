# sdk/rust/

The GodspeedOS service SDK. Every userspace service links against this crate.

> **Services and drivers are not written against this crate. They are written on `gs`**, the standard
> library (`stdlib/rust/src`, crate `godspeed`, imported as `gs`). This crate is the layer underneath
> it: the syscall wrappers, the `ServiceContext` every entry point receives, and the hardware accessors
> (`Mmio`, `Dma`) CLAUDE.md 18.1 designates. `scripts/one_way_check.py` gates the boundary: a raw
> `ctx.<method>()` call that `gs` already covers is refused in any service or example, and the message
> names the `gs` call to use instead. Where `gs` lacks something a service needs, the gap is closed in
> `gs` (`backlog/71`), not by reaching past it. The method menu below is what `gs` is built FROM, and
> what a service still reaches for only where `gs` has nothing (spawning, `mmio`, `dma_region`, ...).

## Purpose

Provide typed, safe wrappers around kernel syscalls so service code:
- Never issues raw syscall numbers.
- Never touches raw capability slot integers directly.
- Gets compile-time assurance that message size limits are respected.

## Files

| File                  | Responsibility |
|-----------------------|---------------|
| `lib.rs`              | Crate root: re-exports, `Error` enum |
| `capability.rs`       | `CapHandle` (opaque slot index), `CapError` (mirrors kernel errors) |
| `ipc.rs`              | `Message`, `recv`, `send`, `try_send`, `call` (synchronous request/reply), `IpcError` (incl. `ReplyDead`) |
| `record.rs`           | `Table` (the typed structured-pipe value), `Value`, `RecordSink`; `where`/`select`/`sort` ops, `to_json`/`to_yaml`/`to_grid` renderers, `from_json`. The model behind typed pipes (`docs/records.md`), shared so any service can produce records |
| `trace.rs`            | The IPC trace wire format + emission arming (`utilities/47_events.md`). A service emits only if its spawn request granted it a send cap to `events` (declared as `ipc_send = ["events"]`), so tracing is AUTHORITY, not a switch; the ring itself lives in the `events` service and the kernel records nothing |
| `service_context.rs`  | `ServiceContext`: handed to `service_main`; named cap lookup; log helpers; spawn helpers (TCB-only); the `request_with_reply*` family (synchronous request/reply; the plain one has no deadline, the `_deadline` ones are bounded) |
| `syscall.rs`          | `raw_syscall`, one per ISA (`pub(crate)`, so no service can name it) - the syscall ABI CLAUDE.md 18.1 designates |
| `mmio.rs`             | `Mmio` and `Framebuffer`: bounds-checked volatile access to a granted register window (18.1) |
| `dma.rs`              | `Dma`: accessors for a granted DMA arena (18.1) |
| `hid.rs`              | USB HID boot-protocol decoding (keyboard and mouse), key repeat and the Ctrl+Alt+Del signal, shared by `xhci`, `ehci` and `dwc2` - pure logic, host-tested |
| `churn.rs`            | Pure helpers for the `churn` power-cut test's file content |
| `adversarial.rs`      | The test-only fault and fuzz primitives (18.1): `fuzz_syscall` and the deliberate ring-3 faults; also the panic handler's way to die |

## `ServiceContext` contract

`ServiceContext` is the single entry point for all OS interaction:
- Passed by the kernel to `service_main` at spawn.
- Non-`Copy` - one instance per service; cannot be duplicated.
- The only way to invoke syscalls (no raw `asm!` in service code).

What a service writes, on `gs` over this crate (the `gs` calls are the ones `one_way_check.py` requires):

```rust
use godspeed::{self as gs, ipc::Message};

// IPC to a peer this service was wired to at spawn (its spawn request's send peers, CLAUDE.md 13.6).
gs::ipc::send(&ctx, "pong", &Message::from_bytes(b"hello"))?;   // Result<(), gs::Error>
let msg = gs::ipc::recv(&ctx);                                   // Message, blocking

// Synchronous request/reply, bounded (CallDeadline). If the peer dies holding the request the kernel
// wakes the caller with ReplyDead (CLAUDE.md 8.6) and `gs` returns `Error::PeerDied` - it does NOT
// re-send, because the request was delivered and may have been acted on. Only a send that never left
// (`Error::Unreachable`) is reacquired by name and sent once more, for you.
let reply = gs::call::request(&ctx, "fs", &request_msg);        // Result<Message, gs::Error>

// Logging - plain, and formatted. This is the kernel floor (CLAUDE.md 11.4), and the one thing a
// service calls on `ServiceContext` directly: `gs` has no second way to log. `log_fmt` renders
// `format_args!` through a bounded 256-byte stack buffer (no heap).
ctx.log("ping: starting");
ctx.log_fmt(format_args!("ping: sent {} of {} messages", sent, total));

// Spawn has no `gs` route yet. Spawning BY NAME (`spawn`/`spawn_on`) now reaches only the kernel's
// catalogue, which holds `supervisor` alone (CLAUDE.md 14.1), so it fails for any other name: a
// service that wants another started asks the supervisor over IPC, and only the supervisor holds
// IMAGE_SPAWN to supply an image (`spawn_image`).
```

(`?` on the two `gs` calls assumes a helper returning `Result<_, gs::Error>`; `service_main` itself
returns `!`.)

### The `ServiceContext` method menu

The methods `gs` is built from, grouped by purpose. This is the working subset; the full surface is
in `sdk/rust/src/service_context.rs`. **A service does not call these where `gs` wraps them** - nearly
all of the IPC and capability rows, `resource_revoke` and `last_recv_badge`, `yield_cpu`/`park` and
`irq_unmask` have `gs` equivalents, and `scripts/one_way_check.py` names each one it counts. What is
left to call directly is logging, spawning, the hardware accessors, and the few the table there does
not yet cover. And do not hand-roll what either layer provides (e.g. formatting
a number by hand instead of using `log_fmt`).

| Purpose | Methods |
|---------|---------|
| **Log** | `log(&str)`; **`log_fmt(format_args!(...))`** - formatted output, bounded (256-byte stack buffer, no heap) |
| **Capabilities** | `capability(name) -> Result<CapHandle>`; `derive_cap(h) -> Option<CapHandle>`; `remove_cap(h)`; `query_cap_rights(h)`; `self_grant_handle() -> Option<CapHandle>` |
| **IPC** | `recv()`; `try_recv() -> Option`; `recv_timeout(cycles)`; `send(peer, &msg)`; `try_send(peer, &msg)`; `send_by_handle(h, &msg)`; `request_with_reply(peer, &msg) -> Option<Message>` (sync, waits on truth); `reacquire_by_name(peer) -> bool` |
| **Delegated resource caps (§7.10)** | `resource_mint(rights) -> Option<(id, cap)>`; `resource_invoke(cap, right, reply, &msg)`; `resource_revoke(id) -> bool`; `last_recv_badge() -> Option<(id, right)>`; `take_pending_cap() -> Option<CapHandle>`; `send_with_cap_by_handle(peer_h, cap, &msg)`; `acquire_send_cap(peer) -> Option<CapHandle>` |
| **Hardware (drivers, §12)** | `mmio() -> Option<Mmio>`; `dma_region() -> Option<Dma>`; `irq_unmask(vector)`; `device_power(on) -> bool` (needs `DEVICE_POWER`, minted with a fixed device window the arch can power; §12.3); `cpu_clock(max) -> Option<u32>` (needs `CPU_CLOCK`, held by `power` alone) |
| **CPU / lifecycle** | `yield_cpu()`; `park() -> !`; `spawn_on(name, core)` (by name: the kernel catalogue holds `supervisor` alone, so every other name fails) |

## Records and pipe-friendly services (`record.rs`)

GodspeedOS pipes carry a **typed `Table`**, not text (`docs/records.md`). The model lives here in
the SDK so any service - not just the shell - can build records, filter them
(`where`/`select`/`sort`), and render them to JSON/YAML. All bounded and `no_std` (fixed
cols/rows/arena, loud on overflow - §26.6).

A service participates in a record pipe **with no new kernel surface**: build a `Table`, send it
through the shell-delegated pipe cap (EOT-terminated, like any byte producer, `docs/pipes.md`).
Two ways to put it on the wire:

```rust
use godspeed::record::{Table, Value, RecordSink};   // `gs::record` re-exports the SDK's types

let mut t = Table::new(&["name", "n"]);
let alpha = t.intern(b"alpha");
t.add_row(&[alpha, Value::Int(1)]);

struct MsgSink<'a>(&'a mut [u8], usize);     // any sink: an IPC message, a buffer, …
impl RecordSink for MsgSink<'_> {
    fn put(&mut self, b: &[u8]) { /* append b */ }
}

t.encode(&mut sink);   // ← the binary WIRE CODEC: the Table itself, compact & typed.
                       //   The shell decodes it straight into records - no round-trip.
t.to_json(&mut sink);  // ← or JSON text; the shell's `| from json` lifts it back.
```

- **`encode`/`decode`** - the bounded binary codec. Use it for a service that *is* a record
  producer (the shell knows it and `Table::decode`s its stream into a `Table`). Compact, typed,
  not JSON. `examples/roster` does this.
- **`to_json`** - render JSON at the edge; the shell's `| from json` parses external/text JSON
  back into records. Use it for human-facing output and interop, not service→service transport.

Both are bounded (§26.6) and fit the byte-pipe transport. The codec is what makes a service-side
producer first-class - `roster | where role=core` with no `from json` in sight.

## no_std

The SDK is `#![no_std]` (`std` only under `cargo test`, for the host-tested pure modules). It does not depend on any allocator. A service that needs more memory calls `alloc_mem` (syscall 6) explicitly; the kernel maps pages within the memory limit its spawn request set (declared as `resources.memory.limit` in the contract) and answers `AllocDenied` past it.

## What the SDK does NOT provide

- A filesystem API. The `fs` service speaks it over IPC, and `gs::fs` (`Fs`) and `gs::file` are the
  client a service uses.
- A network API. The `net-stack` service speaks it over IPC, and `gs::net` (`Net`) is the client.
- Threads (services are single-threaded; parallelism is via multi-service composition).
- A heap allocator (services must manage their own memory if they need it).
- Raw syscalls. `raw_syscall` is `pub(crate)`; everything goes through `ServiceContext` and the `ipc` wrappers. The one exception is `adversarial::fuzz_syscall`, which exists for the fuzz suite and nothing else.
