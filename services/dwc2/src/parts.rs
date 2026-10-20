//! The DMA arena, laid out by name (`docs/driver-library.md` step 3).
//!
//! One part per buffer the controller is pointed at, taken in order through `gs::driver::arena`, so no
//! two can overlap and an access past one ends the driver naming it. They land where the offset
//! constants they replaced put them, but for the hub's status buffer: that sat at 0x300, ON the
//! mass-storage CBW, and two parts cannot share bytes, so it has its own at 0x280.
//!
//! **Why overlap is refused by construction now.** Twice this driver's buffers were laid over each
//! other by hand. The keyboard report first sat at 0x100, inside the control-transfer scratch
//! (0x40..0x140): the 256-byte config-descriptor read landed on it, and the poll read back leftover
//! descriptor bytes - non-zero, so they passed the "is there a keypress" filter, and garbage, so they
//! decoded to nothing (88 reports in 15 seconds with nothing typed). Then the network's transmit frame
//! was first placed at 0x1000, inside the disk buffer (0x400..0x1400), and a const assertion caught it
//! at build time. Each assertion guarded one pair; a layout guards every pair, and the hub's status
//! buffer is the overlap no assertion was written for.

use godspeed::driver::arena::NoRoom;
use godspeed::driver::{Arena, Part};
use godspeed_sdk::ServiceContext;

/// The parts. `Copy`: views of the same bytes, passed where the arena was.
#[derive(Clone, Copy)]
pub struct Parts {
    /// A control transfer's 8-byte setup packet.
    pub setup: Part,
    /// A control transfer's data stage: descriptors, `chan::DATA_LEN` bytes.
    pub ctl_data: Part,
    /// The keyboard's 8-byte boot report.
    pub report: Part,
    /// The hub's status-change bitmap, up to 4 bytes.
    pub hub_status: Part,
    /// A mass-storage command block wrapper (31 bytes), its status wrapper (13) and its data.
    pub cbw: Part,
    pub csw: Part,
    pub disk: Part,
    /// The smsc95xx's transmit frame, with its 8-byte command header, and its receive burst.
    pub tx: Part,
    pub rx: Part,
    /// The USB WiFi dongle's receive and transmit buffers.
    pub radio_rx: Part,
    pub radio_tx: Part,
}

/// Lay the arena out, said once; `None`, said with the part that did not fit, is no arena.
pub fn lay_out(ctx: &ServiceContext, a: Arena) -> Option<Parts> {
    let mut l = a.layout();
    let laid = (|| Ok::<_, NoRoom>(Parts {
        setup: l.take("setup packet", 8, 4096)?,
        ctl_data: l.take("control data", crate::chan::DATA_LEN, 0x40)?,
        report: l.take("keyboard report", 8, 0x100)?,
        hub_status: l.take("hub status", 4, 0x80)?,
        cbw: l.take("command block", 31, 0x100)?,
        csw: l.take("command status", 13, 0x40)?,
        disk: l.take("disk data", crate::msc::DATA_MAX, 0x400)?,
        tx: l.take("network tx", crate::net::FRAME_MAX + 8, 0x1000)?,
        rx: l.take("network rx", crate::net::RX_BURST, 0x1000)?,
        radio_rx: l.take("radio rx", godspeed_wifi::usbfn::BULK_IN_MAX, 0x1000)?,
        radio_tx: l.take("radio tx", godspeed_wifi::usbfn::BULK_OUT_MAX, 0x1000)?,
    }))();
    match laid {
        Ok(p) => {
            ctx.log_fmt(format_args!(
                "dwc2-svc: arena laid out - setup {:#x}, control data {:#x}, report {:#x}, hub status {:#x}, CBW {:#x}, CSW {:#x}, disk {:#x}, net tx {:#x}, net rx {:#x}, radio rx {:#x}, radio tx {:#x} ({} of {} bytes)",
                p.setup.offset(), p.ctl_data.offset(), p.report.offset(), p.hub_status.offset(),
                p.cbw.offset(), p.csw.offset(), p.disk.offset(), p.tx.offset(), p.rx.offset(),
                p.radio_rx.offset(), p.radio_tx.offset(), l.used(), l.len()));
            Some(p)
        }
        Err(e) => {
            ctx.log_fmt(format_args!("dwc2-svc: {} - serving as with no arena", e));
            None
        }
    }
}
