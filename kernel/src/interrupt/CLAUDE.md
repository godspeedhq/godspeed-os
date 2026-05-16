# kernel/src/interrupt/

Hardware interrupt routing to userspace driver services (§12).

## Files

| File        | Responsibility |
|-------------|---------------|
| `mod.rs`    | Module declaration |
| `route.rs`  | `IRQ_TABLE[256]`, `register(irq, endpoint)`, `deliver(irq)` |

## How it works (§12.2)

1. Hardware fires IRQ N on some core.
2. The IDT stub calls `arch::x86_64::interrupts::dispatch_irq(N)`.
3. `dispatch_irq` calls `interrupt::route::deliver(N)`.
4. `deliver` looks up the registered driver endpoint for IRQ N in `IRQ_TABLE`.
5. If found: builds an interrupt-event IPC message and calls `ipc::routing::enqueue`.
6. If the driver is on a different core: the enqueue returns the blocked receiver task ID and the caller sends an IPI to wake it.

## Registration

`register(irq, endpoint)` is called from the spawn path when the kernel processes a `hw_interrupt` capability (§12.3). It is called exactly once per IRQ line per system lifetime (drivers are non-restartable only if they are in the TCB; otherwise restart re-registers).

`IRQ_TABLE` is a `SpinLock<[Option<EndpointId>; 256]>`. `register()` is a safe function. `deliver()` is `pub unsafe fn` because it is called from the IDT with IF=0 — the `unsafe` communicates the interrupt-context calling convention, not a memory-safety obligation.

## If no driver is registered

`deliver` discards the IRQ with no log message and no panic. The kernel cannot know whether a driver will register later (AP timing during boot). The driver will start receiving queued interrupts once it registers.

## Kernel does not handle device logic

The kernel IDT routes IRQs to userspace and EOIs the APIC. Everything after that — MMIO reads, DMA, protocol state machines — lives in the driver service.
