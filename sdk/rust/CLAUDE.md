# sdk/rust/

The GodspeedOS service SDK. Every userspace service links against this crate.

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
| `ipc.rs`              | `Message`, `recv`, `send`, `try_send`, `IpcError` |
| `record.rs`           | `Table` (the typed structured-pipe value), `Value`, `RecordSink`; `where`/`select`/`sort` ops, `to_json`/`to_yaml`/`to_grid` renderers, `from_json`. The model behind typed pipes (`docs/records.md`), shared so any service can produce records |
| `service_context.rs`  | `ServiceContext`: handed to `service_main`; named cap lookup; log helpers; spawn helpers (TCB-only) |

## `ServiceContext` contract

`ServiceContext` is the single entry point for all OS interaction:
- Passed by the kernel to `service_main` at spawn.
- Non-`Copy` — one instance per service; cannot be duplicated.
- The only way to invoke syscalls (no raw `asm!` in service code).

```
// Named cap lookup resolves against the task's contract metadata.
// Returns Err(CapNotHeld) if the name is not declared in the contract.
let pong_cap = ctx.capability("ipc_send.pong")?;

// IPC
pong_cap.send(Message::text("hello"))?;
let msg = my_endpoint.recv()?;

// Logging
ctx.log("ping: starting");

// Spawn (supervisor-only; requires service_control cap)
ctx.spawn_on("pong", 1)?;
```

## Records and pipe-friendly services (`record.rs`)

GodspeedOS pipes carry a **typed `Table`**, not text (`docs/records.md`). The model lives here in
the SDK so any service — not just the shell — can build records, filter them
(`where`/`select`/`sort`), and render them to JSON/YAML. All bounded and `no_std` (fixed
cols/rows/arena, loud on overflow — §26.6).

A service participates in a record pipe **today, with no new kernel surface**: build a `Table`,
render it with `to_json`, and emit the bytes (EOT-terminated) like any byte producer
(`docs/pipes.md`). The shell's `| from json` lifts it back into records for `where`/`sort`:

```rust
use godspeed_sdk::{Table, Value, RecordSink};

let mut t = Table::new(&["name", "n"]);
let alpha = t.intern(b"alpha");
t.add_row(&[alpha, Value::Int(1)]);

struct MsgSink<'a>(&'a mut [u8], usize);     // any sink: an IPC message, a buffer, …
impl RecordSink for MsgSink<'_> {
    fn put(&mut self, b: &[u8]) { /* append b */ }
}
t.to_json(&mut MsgSink(&mut buf, 0));         // → `[ {"name": "alpha", "n": 1} ]`
```

Crossing a service boundary *as records* (skipping the JSON round-trip) is the future bounded
**wire codec** — deliberately not built until a service needs it (§26.2).

## no_std

The SDK is `#![no_std]`. It does not depend on any allocator. Services that need dynamic allocation must declare it in their contract and call the alloc syscall explicitly.

## What the SDK does NOT provide

- A filesystem API (go through `fs` service via IPC).
- A network API (not in v1 scope).
- Threads (services are single-threaded; parallelism is via multi-service composition).
- A heap allocator (services must manage their own memory if they need it).
- Raw syscall wrappers (intentionally absent — always go through `ServiceContext`).
