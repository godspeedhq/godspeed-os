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

## no_std

The SDK is `#![no_std]`. It does not depend on any allocator. Services that need dynamic allocation must declare it in their contract and call the alloc syscall explicitly.

## What the SDK does NOT provide

- A filesystem API (go through `fs` service via IPC).
- A network API (not in v1 scope).
- Threads (services are single-threaded; parallelism is via multi-service composition).
- A heap allocator (services must manage their own memory if they need it).
- Raw syscall wrappers (intentionally absent — always go through `ServiceContext`).
