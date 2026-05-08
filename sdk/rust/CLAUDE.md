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
| `service_context.rs`  | `ServiceContext`: handed to `service_main`; named cap lookup; log helpers; TCB-only helpers |

## `ServiceContext` contract

`ServiceContext` is the single entry point for all OS interaction. It is:
- Passed by the kernel to `service_main` at spawn.
- Non-`Copy` — one per service instance.
- The only way to invoke syscalls (no raw `asm!` in service code).

Named capability lookup (`ctx.capability("ipc_send.pong")`) resolves names against the kernel's contract metadata for this task. Requesting a name not in the contract returns `Err(CapNotHeld)`.

## no_std

The SDK is `#![no_std]`. It does not depend on any allocator. Services that need dynamic allocation must declare it in their contract and call the alloc syscall explicitly.

## What the SDK does NOT provide

- A filesystem API (go through `fs` service via IPC).
- A network API (not in v1 scope).
- Threads (services are single-threaded; parallelism is via multi-service composition).
- A heap allocator (services must manage their own memory if they need it).
