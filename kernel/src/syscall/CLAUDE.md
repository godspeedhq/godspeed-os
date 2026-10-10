# kernel/src/syscall/

Syscall entry point and dispatch (§8.2, §7.5).

## Files

| File           | Responsibility |
|----------------|---------------|
| `mod.rs`       | Module declaration |
| `dispatch.rs`  | `syscall_handler(number, arg0, arg1, arg2)` - raw entry from each port's trap path (x86 `ud2`/IDT stub, ARM `svc`, RISC-V `ecall`); dispatch table |

## Invariant: cap before action

Every syscall that performs a privileged action must call `CapTable::get(slot, required_right)` (through `scheduler::current_task_lookup_cap`) before doing anything with the resource. This is invariant §3.1. (An `assert_cap_validated(&Ok(()))` checkpoint after it was passed a literal `Ok` and could not fire; it was deleted 2026-10-10, `backlog/80` K20.)

If you are adding a syscall:
1. Assign it a number in `SyscallNumber`.
2. Add a handler `handle_<name>` in `dispatch.rs`.
3. The first thing `handle_<name>` does is validate the capability.
4. There are no exceptions to this rule.

**Two validation forms.** Most syscalls take a `cap_slot` argument and validate
with `CapTable::get(slot, right)`. Syscalls whose arguments fill the ABI registers
or that take none - the introspection reads (`InspectKernel` 13 system queries,
`TaskStat` 16, `TaskCaps` 28), `Kill` (8, both args carry the name), `Reboot` (18),
`ResourceMint` (30), `NetFrame*`/`NetInfo` (42-44), `Gpio` (45), `UsbDisk*` (46-49),
`FireIrq` (51), `PciCfgRead` (53), `DevicePower` (54), `CpuClock` (55), and the
IMAGE_SPAWN half of `SpawnImage` (52) - instead validate by **holdings**:
`scheduler::current_task_holds_resource(rid, right)` confirms the calling task
holds the gating resource (e.g. INTROSPECT for the reads, SERVICE_CONTROL for kill).
This still satisfies §3.1 (a capability is validated before the privileged
action); only the calling convention differs. `holds_resource` is for **stable**
resources only (gen 0 forever) - see `docs/introspection-capability.md`,
`docs/service-control-cap.md`, and the note on its definition in
`capability/table.rs`.

## Syscall table (v1, partial - `SyscallNumber` in `dispatch.rs` is the full list, 1-55 with 9 removed)

| Number | Name        | Required cap right             |
|--------|-------------|--------------------------------|
| 1      | `send`      | SEND                           |
| 2      | `recv`      | RECV                           |
| 3      | `try_send`  | SEND                           |
| 4      | `yield`     | none                           |
| 5      | `log`       | log_write cap                  |
| 6      | `alloc_mem` | implicit (own task memory)     |
| 7      | `spawn`     | SPAWN (WRITE)                  |
| 8      | `kill`      | SERVICE_CONTROL (WRITE) - held by shell/supervisor/probes; validated by holdings (no slot - both args carry the name). See `docs/service-control-cap.md` |
| 13     | `inspect_kernel` | INTROSPECT (READ) for the system queries (1, 2, 4-8); **none** for the ungated task-neutral reads: 0 (own alloc), 3 (TSC), 10 (input-ready), 11 (RTC), 12 (boot datetime), 13 (console-foreground-allows for the calling task), and the board/transport reads 14-21 and 23. Queries 24-27 (owned endpoint, CALL-awaited endpoint, unlocked-serial count, boot record) are INTROSPECT-gated. **Query 9 (framebuffer console geometry) is DELETED** - terminal geometry belongs to the `console` service and is read over IPC via `ctx.console_dims()` (`docs/console-service.md` 9.7) |
| 16     | `task_stat` | INTROSPECT (READ) - discloses any task's state |
| 18     | `reboot`    | REBOOT (WRITE) - held by `shell` alone (its `reboot` cmd, and the Ctrl+Alt+Del chord the USB drivers only SIGNAL to it - SEC-2); validated by holdings (no args). Closes the ambient-reboot gap |

## Safety

`syscall_handler` is `unsafe extern "C"` because it is called from a raw trap stub (on x86, an IDT stub) at the ring 3 → ring 0 boundary. Arguments are raw register values from untrusted user code:
- Never dereference `arg*` as a kernel pointer.
- Always validate length fields before copying user memory.
- Always validate cap slots are within `0..MAX_CAPS_PER_TASK`.

User-pointer operations go through `arch::imp::read_user_bytes(ptr, len)` and `write_user_bytes(dst, src)`, which validate the pointer range before touching memory. Do not use `from_raw_parts` or `copy_nonoverlapping` directly in handler functions - use those wrappers instead.

## Dispatch flow

```mermaid
flowchart TD
    IDT[IDT stub - ring 3 → ring 0] --> H[syscall_handler]
    H --> N{SyscallNumber}
    N -->|Send/TrySend| S[handle_send: validate SEND cap → enqueue → maybe IPI]
    N -->|Recv| R[handle_recv: validate RECV cap → dequeue or block]
    N -->|Yield| Y[scheduler::yield_current]
    N -->|Log| L[handle_log: validate log_write cap → append to ring buffer]
    N -->|AllocMem| A[handle_alloc: track_alloc → map page]
    N -->|Unknown| E[Return -1]
```
