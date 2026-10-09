# kernel/src/interrupt/

Hardware interrupt routing to userspace driver services (§12).

## Files

| File        | Responsibility |
|-------------|---------------|
| `mod.rs`    | Module declaration |
| `route.rs`  | `IRQ_TABLE[256]`, `register(irq, endpoint)`, `unregister_endpoint(endpoint)`, `registered_endpoint(irq)`, `deliver(irq)` |

## How it works (§12.2)

```text
  Hardware IRQ N fires on some core
    │
    ▼
  Kernel IDT stub ── dispatch_irq(N) ──▶  interrupt::route::deliver(N)
                                               │
                                  mask the line (level-triggered only)
                                  lookup IRQ_TABLE[N]
                                               │
                         no driver registered ─┤─ discard, EOI
                                               │
                         driver endpoint found ▼
                                    queue below half full? ipc::routing::enqueue_from_interrupt(endpoint, interrupt_event_msg)
                                    wake_by_slot (IPI) if driver blocked on recv
                                    EOI (unconditionally)
                                               │
                                    Driver Service: recv() returns interrupt event
                                    Driver re-opens a level-triggered line with IrqUnmask (36)
                                               │
                                    Driver handles device via its MMIO grant
```

## Registration

`register(irq, endpoint)` is called from the spawn path for each vector the kernel derived from the device CLASS the spawn request names (`task::hw_irqs_for`, or an allocated PCI MSI vector) - never a vector the spawner or a contract names (§12.3, §14.1). A driver's death releases its routes with `unregister_endpoint` (which masks each released line), and its respawn registers again.

`IRQ_TABLE` is a `SpinLock<[Option<EndpointId>; 256]>`. `register()` is a safe function. `deliver()` is `pub unsafe fn` because it is called from the IDT with IF=0 - the `unsafe` communicates the interrupt-context calling convention, not a memory-safety obligation.

## If no driver is registered

`deliver` discards the IRQ and does not panic. It logs the FIRST delivery of each vector, routed or not (`irq: FIRST delivery of vector ... (routed: NO - discarded)`), and nothing after that. The kernel cannot know whether a driver will register later (AP timing during boot); interrupts that arrive before it registers are lost, not queued.

## Kernel does not handle device logic

The kernel routes IRQs to userspace and sends the EOI. Everything after that - MMIO reads, DMA, protocol state machines - lives in the driver service. The MMIO window the kernel grants at spawn (resolved from the device class) gives the driver direct access to its device's registers; no kernel mediation at runtime.
