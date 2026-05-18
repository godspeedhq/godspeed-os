# kernel/src/syscall/

Syscall entry point and dispatch (§8.2, §7.5).

## Files

| File           | Responsibility |
|----------------|---------------|
| `mod.rs`       | Module declaration |
| `dispatch.rs`  | `syscall_handler(number, arg0, arg1, arg2)` — raw entry from IDT stub; dispatch table |

## Invariant: cap before action

Every syscall that performs a privileged action must call `CapTable::get(slot, required_right)` before doing anything with the resource. This is invariant §3.1. `invariants::assertions::assert_cap_validated` is called on the result.

If you are adding a syscall:
1. Assign it a number in `SyscallNumber`.
2. Add a handler `handle_<name>` in `dispatch.rs`.
3. The first thing `handle_<name>` does is validate the capability.
4. There are no exceptions to this rule.

## Syscall table (v1)

| Number | Name        | Required cap right             |
|--------|-------------|--------------------------------|
| 1      | `send`      | SEND                           |
| 2      | `recv`      | RECV                           |
| 3      | `try_send`  | SEND                           |
| 4      | `yield`     | none                           |
| 5      | `log`       | log_write cap                  |
| 6      | `alloc_mem` | implicit (own task memory)     |

## Safety

`syscall_handler` is `unsafe extern "C"` because it is called from a raw IDT stub at the ring 3 → ring 0 boundary. Arguments are raw register values from untrusted user code:
- Never dereference `arg*` as a kernel pointer.
- Always validate length fields before copying user memory.
- Always validate cap slots are within `0..MAX_CAPS_PER_TASK`.

User-pointer operations go through `arch::x86_64::read_user_bytes(ptr, len)` and `write_user_bytes(dst, src)`, which validate the pointer range before touching memory. Do not use `from_raw_parts` or `copy_nonoverlapping` directly in handler functions — use those wrappers instead.

## Dispatch flow

```mermaid
flowchart TD
    IDT[IDT stub — ring 3 → ring 0] --> H[syscall_handler]
    H --> N{SyscallNumber}
    N -->|Send/TrySend| S[handle_send: validate SEND cap → enqueue → maybe IPI]
    N -->|Recv| R[handle_recv: validate RECV cap → dequeue or block]
    N -->|Yield| Y[handle_yield: timer_tick immediately]
    N -->|Log| L[handle_log: validate log_write cap → append to ring buffer]
    N -->|AllocMem| A[handle_alloc: track_alloc → map page]
    N -->|Unknown| E[Return UnknownSyscall]
```
