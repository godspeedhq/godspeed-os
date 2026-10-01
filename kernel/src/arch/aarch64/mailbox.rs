// SPDX-License-Identifier: GPL-2.0-only
//! VideoCore mailbox (property interface) for the Raspberry Pi 4.
//!
//! The ARM does not discover its own memory on a Pi. There is no firmware memory map handed over the
//! way Limine does on x86 - the VideoCore owns the machine at reset, keeps some RAM for itself, and
//! will tell the ARM how much it left over only if asked. This is that ask.
//!
//! ## Why this runs with caches OFF
//!
//! The GPU reads the request buffer out of RAM through its own path, not through the ARM's caches. A
//! buffer written while the D-cache is on may still be sitting in the cache when the GPU looks, and the
//! GPU then reads stale data - or worse, reads a half-written message and acts on it.
//!
//! Two ways out: map the buffer non-cacheable, or make the call before the MMU and caches are enabled.
//! The boot asks its questions early and keeps the answers. Calls AFTER `mmu::enable` exist now too -
//! `notify_xhci_reset`, and `set_expander_gpio` behind `DevicePower` - and `call` handles them: it
//! translates the buffer's address itself and does the cache maintenance on both sides of the exchange,
//! and the one syscall-time caller takes a lock that the pre-MMU callers must not.
//!
//! ## Bus addresses
//!
//! The GPU addresses RAM through an alias, not at the ARM's physical address. `0xC000_0000` is the
//! uncached-alias base the mailbox expects, so an ARM address is handed over ORed with it. Passing a
//! raw ARM address instead usually produces silence rather than an error, because the GPU dutifully
//! reads whatever is at the wrong place and finds no valid message.

use super::{put_dec, put_hex, put_str};

/// Mailbox registers: BCM2711 peripheral base + 0xB880.
const MBOX_BASE: usize = 0xFE00_B880;
/// Reached through `mmio()` rather than as raw constants, so these work on BOTH sides of the jump to
/// the high half. As fixed low addresses they were correct only until `drop_low_map`, which is fine
/// while the only caller is the pre-MMU board query - and a translation fault the moment anything
/// later wants the mailbox, which the xHCI reset notify does.
fn mbox_read() -> *const u32 { super::mmio(MBOX_BASE) as *const u32 }
fn mbox_status() -> *const u32 { super::mmio(MBOX_BASE + 0x18) as *const u32 }
fn mbox_write() -> *mut u32 { super::mmio(MBOX_BASE + 0x20) as *mut u32 }

const MBOX_FULL: u32 = 0x8000_0000;
const MBOX_EMPTY: u32 = 0x4000_0000;

/// Channel 8 is the ARM-to-VideoCore property interface.
const CHANNEL_PROP: u32 = 8;

/// The GPU's uncached alias for ARM RAM. See the module header.
const BUS_ALIAS: u32 = 0xC000_0000;

/// Bounded spin for the mailbox handshake. A GPU that never answers must not hang the boot
/// (invariant 12) - the machine reports "no answer" and carries on with what it does know.
const MBOX_SPINS: u32 = 4_000_000;

/// Property tags.
const TAG_BOARD_REVISION: u32 = 0x0001_0002;
const TAG_ARM_MEMORY: u32 = 0x0001_0005;

/// Request/response buffer. 16-byte aligned because the mailbox packs the channel into the low 4 bits
/// of the address it is handed, so those bits must be zero or the message goes to the wrong channel.
#[repr(C, align(16))]
struct MboxBuf([u32; 36]);

static mut MBOX: MboxBuf = MboxBuf([0; 36]);
static MBOX_LOCK: crate::smp::SpinLock<()> = crate::smp::SpinLock::new(());

/// The firmware's GPIO expander pins the Pi 4 routes board control through (`bcm2711-rpi-4-b.dts`,
/// `expgpio`): 0 BT_ON, 1 WL_ON, 2 PWR_LED_OFF, 3 GLOBAL_RESET, 4 VDD_SD_IO_SEL, 5 CAM_GPIO,
/// 6 SD_PWR_ON, 7 SD_OC_N. Linux's `mmc-pwrseq-simple` for the radio is `reset-gpios = <&expgpio 1>`.
pub const EXPGPIO_WL_ON: u32 = 1;
/// The same chip's Bluetooth power enable (docs/wifi.md 53): the other half of the CYW43455's power.
pub const EXPGPIO_BT_ON: u32 = 0;

/// Drive one firmware-expander GPIO through the `SET_GPIO_STATE` property tag (`0x00038041`). The
/// expander's pins are numbered from 128 on the mailbox side, which is why Linux's `gpio-raspberrypi-exp`
/// adds the same offset. `state` is the PHYSICAL level: for WL_ON, 1 is powered and 0 is off/reset.
/// Returns whether the firmware took the request; it is a runtime caller of `property_call`, which does
/// its own address translation and cache maintenance around the shared buffer.
/// Read one firmware-expander GPIO back through `GET_GPIO_STATE` (`0x00030041`): the PHYSICAL level the
/// firmware reports for the pin, or `None` if it did not answer. The instrument that says whether a
/// `set_expander_gpio` took, because the SET tag answers "accepted" whether or not the pin moved.
pub fn get_expander_gpio(pin: u32) -> Option<u32> {
    const TAG_GET_GPIO_STATE: u32 = 0x0003_0041;
    const EXPANDER_BASE: u32 = 128;
    let _one = MBOX_LOCK.lock();
    let mut req = [0u32; 8];
    req[0] = 8 * 4;
    req[1] = 0;
    req[2] = TAG_GET_GPIO_STATE;
    req[3] = 8;
    req[4] = 4;
    req[5] = EXPANDER_BASE + pin;
    req[6] = 0;
    req[7] = 0;
    property_call(&mut req)?;
    Some(req[6])
}

pub fn set_expander_gpio(pin: u32, on: bool) -> bool {
    // ONE RUNTIME CALLER AT A TIME. `MBOX` is one buffer and the channel is one register pair. This is
    // reached from a syscall with the MMU on, where a lock is sound and another core may be in a boot
    // call; the boot callers themselves cannot take it (see `property_call`).
    let _one = MBOX_LOCK.lock();
    const TAG_SET_GPIO_STATE: u32 = 0x0003_8041;
    const EXPANDER_BASE: u32 = 128;
    let mut req = [0u32; 8];
    req[0] = 8 * 4;
    req[1] = 0;
    req[2] = TAG_SET_GPIO_STATE;
    req[3] = 8;
    req[4] = 8;
    req[5] = EXPANDER_BASE + pin;
    req[6] = if on { 1 } else { 0 };
    req[7] = 0;
    property_call(&mut req).is_some()
}

/// The firmware's ARM clock (clock id 3) and the property tags that read and set it, from the firmware's
/// mailbox property interface: get rate `0x00030002`, get max `0x00030004`, get min `0x00030007`, set
/// rate `0x00038002`. Reached from `cpu_clock` behind the `CpuClock` syscall, so at runtime with the MMU
/// on, and therefore under `MBOX_LOCK` like the expander GPIO calls.
const CLOCK_ID_ARM: u32 = 3;
pub const TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;
pub const TAG_GET_MAX_CLOCK_RATE: u32 = 0x0003_0004;
pub const TAG_GET_MIN_CLOCK_RATE: u32 = 0x0003_0007;

/// Read one of the ARM clock's rates (`tag` is one of the three GET tags above), in Hz.
pub fn arm_clock(tag: u32) -> Option<u32> {
    let _one = MBOX_LOCK.lock();
    let mut req = [0u32; 8];
    req[0] = 8 * 4;
    req[1] = 0;
    req[2] = tag;
    req[3] = 8;
    req[4] = 4;
    req[5] = CLOCK_ID_ARM;
    req[6] = 0;
    req[7] = 0;
    property_call(&mut req)?;
    if req[6] == 0 { None } else { Some(req[6]) }
}

/// Ask the firmware to run the ARM clock at `hz`; returns the rate it says it set. `skip setting turbo` is
/// 0, so above the default rate the firmware also raises the voltage it needs - the setting the
/// documentation describes as the default, and the one that keeps a fast clock stable.
pub fn set_arm_clock(hz: u32) -> Option<u32> {
    const TAG_SET_CLOCK_RATE: u32 = 0x0003_8002;
    let _one = MBOX_LOCK.lock();
    let mut req = [0u32; 9];
    req[0] = 9 * 4;
    req[1] = 0;
    req[2] = TAG_SET_CLOCK_RATE;
    req[3] = 12;
    req[4] = 12;
    req[5] = CLOCK_ID_ARM;
    req[6] = hz;
    req[7] = 0;
    req[8] = 0;
    property_call(&mut req)?;
    Some(req[6])
}

/// What the firmware told us about the machine. `None` for anything it declined to answer - never a
/// guess, because a wrong memory size is worse than a known-absent one.
#[derive(Clone, Copy, Default)]
pub struct BoardInfo {
    pub board_revision: Option<u32>,
    /// ARM-usable RAM: (base, size) in bytes.
    pub arm_memory: Option<(u32, u32)>,
}

/// Send the staged message and wait for the reply. Returns false on timeout or an error response.
///
/// Works before and after `mmu::enable`. Before it, caches are off and the GPU trivially sees what we
/// wrote. After, the buffer is cacheable and the GPU reads RAM directly, so it is cleaned before the
/// doorbell and invalidated before the reply is read - the same DMA-coherency obligation every other
/// bus master on this board has.
///
/// # Safety
/// `MBOX` must not be in use by another caller. Boot is single-threaded, and the only later caller is
/// the xHCI reset notify, which runs from the same boot path.
unsafe fn call() -> bool {
    // SAFETY: mailbox MMIO through the kernel's peripheral mapping, single-threaded boot.
    unsafe {
        // The GPU is handed a PHYSICAL address. Before the jump the static's address is already
        // physical; after it, it is a high-half virtual one, and `virt_to_phys` is the conversion that
        // makes the difference explicit rather than relying on the low 32 bits happening to match.
        let addr = super::mmu::virt_to_phys((&raw const MBOX) as u64) as u32;
        super::mmu::dma_sync((&raw const MBOX) as u64, 36 * 4);
        // Wait for room, bounded.
        let mut spins = 0;
        while mbox_status().read_volatile() & MBOX_FULL != 0 {
            spins += 1;
            if spins > MBOX_SPINS {
                put_str(b"mailbox: WARN status stuck FULL - no GPU?\r\n");
                return false;
            }
        }
        mbox_write().write_volatile((addr | BUS_ALIAS) | CHANNEL_PROP);

        // Wait for a reply on OUR channel, bounded. The mailbox is shared, so a reply for a different
        // channel is discarded rather than mistaken for ours.
        spins = 0;
        loop {
            while mbox_status().read_volatile() & MBOX_EMPTY != 0 {
                spins += 1;
                if spins > MBOX_SPINS {
                    put_str(b"mailbox: WARN no reply before the bound expired\r\n");
                    return false;
                }
            }
            let msg = mbox_read().read_volatile();
            if msg & 0xF == CHANNEL_PROP {
                break;
            }
        }
        // Invalidate before reading the reply: the GPU wrote it to RAM, not through our caches.
        super::mmu::dma_sync((&raw const MBOX) as u64, 36 * 4);
        // 0x8000_0000 = request succeeded. Anything else means the firmware rejected it, and the
        // caller must not read the value buffer.
        (&raw const MBOX).read_volatile().0[1] == 0x8000_0000
    }
}

/// Run an arbitrary property-tag request: copy it into the aligned mailbox buffer, call, copy the
/// reply back. Returns `None` if the firmware rejected it.
///
/// The buffer has to be this module's 16-byte-aligned static rather than the caller's array, because
/// the mailbox packs the channel into the low 4 bits of the address it is handed - a caller's
/// arbitrarily-aligned buffer would send the message to the wrong channel. Copying in and out is the
/// price of keeping that alignment guarantee in one place.
///
/// # Safety of timing
/// Like every other user of this mailbox, must run before `mmu::enable` - see the module header.
pub fn property_call(req: &mut [u32]) -> Option<()> {
    if req.len() > 36 {
        return None; // larger than the shared buffer; a caller bug, refused rather than truncated
    }
    // NO LOCK HERE, on purpose. The boot-time callers run BEFORE `mmu::enable`, and a spinlock's
    // exclusive-access atomics never succeed on AArch64 with the MMU off - the first attempt at a lock
    // here spun forever on the framebuffer's query (boot 2026-10-01 01:33, hung after the memory map).
    // The callers that reach this from a syscall - the expander GPIO pair and the ARM clock pair - take
    // `MBOX_LOCK` themselves.
    // SAFETY: single-threaded boot, caches off, and MBOX is this module's static. The length is
    // checked above, so neither copy can run past either end.
    unsafe {
        let b = &raw mut MBOX;
        for (i, w) in req.iter().enumerate() {
            (*b).0[i] = *w;
        }
        if !call() {
            return None;
        }
        for (i, w) in req.iter_mut().enumerate() {
            *w = (*b).0[i];
        }
    }
    Some(())
}

/// Tell the firmware we have reset the VL805, so it reloads the controller's firmware.
///
/// **This is the step without which the Pi 4's USB does not exist.** The VL805 is not a
/// self-contained controller: its firmware lives in an SPI EEPROM on the board and is loaded into it by
/// the VideoCore at power-on. Asserting PERST# during PCIe bring-up - which the bring-up must do, to
/// give the endpoint a clean reset edge - wipes it. What is left answers CONFIG space perfectly,
/// because that is the PCIe core, and does not answer its memory BAR at all, because that is the
/// firmware. Every register read comes back as the root complex's poison value.
///
/// That failure is indistinguishable from a bridge window that was never opened, which is how the
/// first two hardware iterations were spent. The firmware exposes this tag precisely so an OS that
/// resets the controller can ask for the reload; Linux calls it from the same place for the same
/// reason.
///
/// `dev_addr` is the controller's PCI address in the usual `(bus << 20) | (dev << 15) | (fn << 12)`
/// encoding.
pub fn notify_xhci_reset(dev_addr: u32) -> bool {
    const TAG_NOTIFY_XHCI_RESET: u32 = 0x0003_0058;
    let mut req = [0u32; 7];
    req[0] = 7 * 4;
    req[1] = 0;
    req[2] = TAG_NOTIFY_XHCI_RESET;
    req[3] = 4;
    req[4] = 4;
    req[5] = dev_addr;
    req[6] = 0; // end tag
    property_call(&mut req).is_some()
}

/// Ask the firmware what it knows. Call once, early, with caches off.
pub fn query() -> BoardInfo {
    let mut info = BoardInfo::default();

    // SAFETY: single-threaded boot before the MMU, MBOX is this module's static.
    unsafe {
        let b = &raw mut MBOX;

        // --- board revision (diagnostic: confirms which board we are actually on) ---
        (*b).0[0] = 7 * 4; // total size in bytes
        (*b).0[1] = 0; // request
        (*b).0[2] = TAG_BOARD_REVISION;
        (*b).0[3] = 4; // value buffer size
        (*b).0[4] = 0; // request code
        (*b).0[5] = 0; // value
        (*b).0[6] = 0; // end tag
        if call() {
            info.board_revision = Some((*b).0[5]);
        }

        // --- ARM-usable memory: base + size ---
        (*b).0[0] = 8 * 4;
        (*b).0[1] = 0;
        (*b).0[2] = TAG_ARM_MEMORY;
        (*b).0[3] = 8; // two u32s come back
        (*b).0[4] = 0;
        (*b).0[5] = 0; // base
        (*b).0[6] = 0; // size
        (*b).0[7] = 0; // end tag
        if call() {
            info.arm_memory = Some(((*b).0[5], (*b).0[6]));
        }
    }

    info
}

/// Report what the firmware said, in a form a reader can sanity-check at a glance.
pub fn report(info: &BoardInfo) {
    match info.board_revision {
        Some(rev) => {
            put_str(b"mailbox: board revision ");
            put_hex(rev as u64);
            put_str(b"\r\n");
        }
        None => put_str(b"mailbox: WARN board revision unavailable\r\n"),
    }
    match info.arm_memory {
        Some((base, size)) => {
            put_str(b"mailbox: ARM memory base ");
            put_hex(base as u64);
            put_str(b" size ");
            put_dec(size as u64 / (1024 * 1024));
            put_str(b" MiB\r\n");
        }
        None => put_str(
            b"mailbox: WARN ARM memory unavailable - the map stays unknown rather than guessed\r\n",
        ),
    }
}
