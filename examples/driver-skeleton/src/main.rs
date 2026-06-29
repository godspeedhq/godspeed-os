// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! driver-skeleton - an ANNOTATED TEMPLATE for "how do I write a device driver on
//! Godspeed?". It is illustrative, not runnable: the kernel wires a driver's
//! MMIO/DMA/IRQ per recognised driver at spawn, so a real one needs a small
//! kernel-side hook. See `examples/e1000` for a real, runnable driver, and
//! `services/block-driver` (AHCI) / `services/xhci` (USB) for production drivers.
//! Read this top to bottom alongside `CLAUDE.md` in this folder.
//!
//! The whole point: a driver is just a SERVICE that holds three extra
//! capabilities - an MMIO window, a DMA arena, and an IRQ line - all declared in
//! its contract and granted by the kernel (Commandment VII). It writes NO
//! `unsafe`: every register and DMA access goes through the SDK's audited
//! `Mmio`/`Dma` wrappers (§18.1, Commandment X).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Mmio, Dma, Message};

// --- Device register map (byte offsets into the MMIO window) -----------------
// Real values come from the device datasheet; these are illustrative.
const REG_ID:       usize = 0x00; // read-only identity/magic
const REG_STATUS:   usize = 0x04; // status bits (write-1-to-clear for the IRQ)
const REG_CTRL:     usize = 0x08; // control: reset / enable / interrupt-enable
const REG_RING_LO:  usize = 0x10; // low  32 bits of the ring's PHYSICAL address
const REG_RING_HI:  usize = 0x14; // high 32 bits

const CTRL_RESET:   u32 = 1 << 0;
const CTRL_ENABLE:  u32 = 1 << 1;
const CTRL_IRQ_EN:  u32 = 1 << 2;
const STATUS_READY: u32 = 1 << 0;

const EXPECTED_ID:  u32 = 0xC0FF_EE00; // the device's identity magic (illustrative)
const IRQ_VECTOR:   u8  = 11;          // must match `hw_interrupt` in the contract

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("driver-skeleton: starting");

    // 1. Acquire the kernel-granted hardware capabilities. At spawn the kernel
    //    mapped our MMIO window (`hw_mmio`) and a physically-contiguous DMA arena;
    //    we reach them through the SDK, never a raw pointer and never `unsafe`.
    //    If the device is absent we DEGRADE (a service is never special,
    //    Commandment V) instead of panicking.
    let (mmio, dma) = match (ctx.mmio(), ctx.dma_region()) {
        (Some(m), Some(d)) => (m, d),
        _ => {
            ctx.log("driver-skeleton: no device mapped - idling (degraded)");
            // Drain our IPC endpoint forever (the flood-endpoint discipline): a
            // registered service that idles without recv'ing lets a queue flood
            // sit at 16/16 forever. Poll + yield so the core still idles.
            loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
        }
    };

    // 2. Bring the device up. This runs on EVERY spawn, including a restart - a
    //    driver never assumes the controller kept its state across its own death
    //    (Commandments V + IX). On failure, log loudly and degrade.
    if !bring_up(&ctx, &mmio, &dma) {
        ctx.log("driver-skeleton: device bring-up FAILED - idling");
        loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
    }
    ctx.log("driver-skeleton: device ready, serving");

    // 3. Serve. Two kinds of events arrive on our endpoint: a hardware INTERRUPT
    //    (the kernel routes the IRQ here as a message, §12.2), and a client
    //    REQUEST. We WAIT for one of them - never a fixed sleep hoping the device
    //    is done (Commandment VIII: wait for the event, not the clock).
    serve(&ctx, &mmio, &dma)
}

/// Reset the device, verify identity, install the DMA ring, enable interrupts.
/// Returns false if the device never reports ready.
fn bring_up(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma) -> bool {
    // Identity check: confirm we are talking to the device we expect.
    if mmio.read32(REG_ID) != EXPECTED_ID {
        ctx.log("driver-skeleton: unexpected device id");
        return false;
    }

    // Reset, then wait for STATUS_READY by POLLING THE STATUS BIT (observable
    // truth), BOUNDED so a dead device cannot wedge us forever (Commandment VIII).
    mmio.write32(REG_CTRL, CTRL_RESET);
    let mut spins = 0u32;
    while mmio.read32(REG_STATUS) & STATUS_READY == 0 {
        spins += 1;
        if spins > 100_000 { return false; } // give up loudly, never hang
        ctx.yield_cpu();                       // time only conserves CPU here ...
    }                                          // ... the BIT is what proves ready.

    // Build a ring/buffer in OUR DMA arena (the arena is the driver's own, not
    // shared memory - Commandment VI). Hand the device the PHYSICAL address; the
    // device DMAs only inside this arena (IOMMU-confined where present, §6.4).
    dma.zero();
    let ring_phys = dma.phys_base();
    mmio.write32(REG_RING_LO, ring_phys as u32);
    mmio.write32(REG_RING_HI, (ring_phys >> 32) as u32);

    // Enable the device and its interrupt, then unmask our IRQ line so the kernel
    // routes the device's interrupt to our endpoint (§12.2).
    mmio.write32(REG_CTRL, CTRL_ENABLE | CTRL_IRQ_EN);
    ctx.irq_unmask(IRQ_VECTOR);
    true
}

/// Main loop: block on the endpoint for a hardware interrupt or a client request,
/// handle it, then re-arm. Never returns.
fn serve(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma) -> ! {
    loop {
        // Block for truth. recv() parks the task until a message arrives, so the
        // core idles between events - but it is the EVENT that wakes us, never a
        // timer we guessed (Commandment VIII).
        let msg = ctx.recv();

        if is_interrupt(&msg) {
            // The device raised its IRQ: drain what it produced from the ring in
            // our DMA arena, acknowledge (write-1-to-clear), then re-arm the line.
            let _completed = dma.read32(0);          // e.g. a finished descriptor
            mmio.write32(REG_STATUS, mmio.read32(REG_STATUS));
            ctx.irq_unmask(IRQ_VECTOR);
        } else {
            // A client asked for device work. Do it, then REPLY explicitly - a
            // successful send means queued, not processed (Commandment VIII), so
            // any protocol needing confirmation acknowledges on its own.
            handle_request(ctx, mmio, dma, &msg);
        }
    }
}

// --- Illustrative stubs ------------------------------------------------------
// A real driver tells an interrupt from a request by the endpoint it arrived on,
// or by a kernel-set badge; shown here as a placeholder so the shape is clear.
fn is_interrupt(_msg: &Message) -> bool { false }
fn handle_request(_ctx: &ServiceContext, _mmio: &Mmio, _dma: &Dma, _msg: &Message) {}
