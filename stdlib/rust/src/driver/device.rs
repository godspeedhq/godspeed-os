// SPDX-License-Identifier: Apache-2.0
//! What the kernel knows about a driver's DEVICE that the driver cannot read for itself.
//!
//! # Why this exists
//!
//! A driver holds its own register window and nothing else, so a fact about its device that lives
//! elsewhere - the board's MAC in the firmware's mailbox, the PCI identity of the NIC, a controller's
//! base clock that its own capability register reports wrongly, the configuration space of a bus - has
//! to be asked of the kernel. Drivers asked the SDK directly; these are the same answers, in the shape
//! the promise covers (`docs/driver-library.md`, step 5).
//!
//! # What it is, and is not
//!
//! Queries, no authority: each answers what the kernel already reports, and none changes the device.
//! What the answer MEANS for the device - which chip a PCI identity is, what divider a clock gives - is
//! the driver's.

#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

/// The board's own MAC address, or `None` where the board cannot say. A NIC driver uses it rather than
/// inventing one, since an invented address is the same on every board.
#[cfg(not(test))]
#[inline(always)]
pub fn board_mac(ctx: &ServiceContext) -> Option<[u8; 6]> {
    ctx.board_mac()
}

/// The discovered NIC's PCI identity as `vendor | device << 16`, or 0 when there is none.
#[cfg(not(test))]
#[inline(always)]
pub fn nic_vendor_device(ctx: &ServiceContext) -> u32 {
    ctx.nic_vendor_device()
}

/// The SD/EMMC controller's base clock in Hz, or 0 when the platform does not report one. A driver that
/// gets 0 says so rather than guessing a divider.
#[cfg(not(test))]
#[inline(always)]
pub fn emmc_base_clock_hz(ctx: &ServiceContext) -> u32 {
    ctx.emmc_base_clock_hz()
}

/// Read one 32-bit register of a PCI function's configuration space. `sel` is the platform's own
/// bus/device/function encoding, which is the caller's knowledge. `None` when the capability is missing
/// or the machine will not admit the access; `Some(0xFFFF_FFFF)` is a successful read of an absent
/// device, which is data.
#[cfg(not(test))]
#[inline(always)]
pub fn pci_cfg_read(ctx: &ServiceContext, sel: u32, offset: u16) -> Option<u32> {
    ctx.pci_cfg_read(sel, offset)
}
