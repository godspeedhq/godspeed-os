//! x86_64 architecture layer — the unsafe boundary (§18.1).
//!
//! All `unsafe` code in the kernel that touches hardware directly lives in
//! this module or in `memory/`, `capability/`, `smp/`. Nowhere else.

pub mod ap_boot;
pub mod boot;
pub mod context_switch;
pub mod interrupts;
pub mod page_tables;

use limine::request::{HhdmRequest, MemmapRequest, MpRequest};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

// ---------------------------------------------------------------------------
// Limine protocol — requests must survive to link time via #[used] + KEEP().
// ---------------------------------------------------------------------------

#[used]
#[link_section = ".requests_start"]
static _REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[link_section = ".requests"]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[used]
#[link_section = ".requests"]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[link_section = ".requests"]
static SMP_REQUEST: MpRequest = MpRequest::new(0);

#[used]
#[link_section = ".requests_end"]
static _REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

// ---------------------------------------------------------------------------
// Kernel entry point (called by Limine on the BSP).
// ---------------------------------------------------------------------------

/// Raw entry point called by the Limine bootloader.
///
/// # Safety
/// Called exactly once by Limine on the BSP after paging and long-mode are
/// already set up. We initialise serial first so all subsequent kprintln!
/// calls produce output, then build BootInfo and hand off to kernel_main.
#[no_mangle]
extern "C" fn _start() -> ! {
    // SAFETY: called once at boot; no concurrent serial access yet.
    unsafe { serial_init() };

    assert!(BASE_REVISION.is_supported(), "unsupported Limine protocol revision");

    let boot_info = collect_boot_info();

    // SAFETY: _start never returns (kernel_main is -> !), so boot_info on
    // this stack frame is valid for the entire kernel lifetime.
    unsafe { crate::kernel_main(&boot_info as *const _) }
}

fn collect_boot_info() -> BootInfo {
    // Static buffer: memory map entries are written here once and referenced
    // for the kernel's lifetime.  64 slots is far more than any real system
    // returns (typical count is 10–20).
    const MAX_REGIONS: usize = 64;
    static mut MAP_BUF: [MemoryRegion; MAX_REGIONS] = [MemoryRegion {
        base: 0,
        len: 0,
        kind: MemoryKind::Reserved,
    }; MAX_REGIONS];

    let mut count = 0usize;

    if let Some(resp) = MEMMAP_REQUEST.response() {
        for entry in resp.entries().iter().take(MAX_REGIONS) {
            let kind = match entry.type_ {
                limine::memmap::MEMMAP_USABLE             => MemoryKind::Usable,
                limine::memmap::MEMMAP_ACPI_RECLAIMABLE   => MemoryKind::AcpiReclaimable,
                limine::memmap::MEMMAP_EXECUTABLE_AND_MODULES => MemoryKind::KernelImage,
                _ => MemoryKind::Reserved,
            };
            // SAFETY: single-threaded boot path; no concurrent access.
            unsafe {
                MAP_BUF[count] = MemoryRegion { base: entry.base, len: entry.length, kind };
            }
            count += 1;
        }
    }

    let hhdm_offset = HHDM_REQUEST
        .response()
        .map(|r| r.offset)
        .unwrap_or(0);

    // Collect non-BSP LAPIC IDs from the SMP response.
    const MAX_AP_IDS: usize = 16;
    static mut AP_ID_BUF: [u32; MAX_AP_IDS] = [0u32; MAX_AP_IDS];
    let mut ap_count = 0usize;

    if let Some(mp) = SMP_REQUEST.response() {
        let bsp_lapic = mp.bsp_lapic_id;
        for cpu in mp.cpus() {
            if cpu.lapic_id != bsp_lapic && ap_count < MAX_AP_IDS {
                // SAFETY: single-threaded boot path.
                unsafe { AP_ID_BUF[ap_count] = cpu.lapic_id; }
                ap_count += 1;
            }
        }
    }

    BootInfo {
        memory_map: unsafe { &MAP_BUF[..count] },
        ap_ids: unsafe { &AP_ID_BUF[..ap_count] },
        kernel_phys_start: 0,
        kernel_phys_end: 0,
        hhdm_offset,
    }
}

// ---------------------------------------------------------------------------
// BootInfo — populated by collect_boot_info(), consumed by kernel_main.
// ---------------------------------------------------------------------------

/// Boot information passed from the bootloader to `kernel_main`.
#[repr(C)]
pub struct BootInfo {
    pub memory_map: &'static [MemoryRegion],
    pub ap_ids: &'static [u32],
    pub kernel_phys_start: u64,
    pub kernel_phys_end: u64,
    /// Base virtual address of Limine's higher-half direct map (HHDM).
    /// Physical address P is accessible at virtual address `hhdm_offset + P`.
    pub hhdm_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    pub base: u64,
    pub len: u64,
    pub kind: MemoryKind,
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub enum MemoryKind {
    Usable          = 1,
    Reserved        = 2,
    AcpiReclaimable = 3,
    KernelImage     = 4,
}

// ---------------------------------------------------------------------------
// Architecture initialisation.
// ---------------------------------------------------------------------------

/// Full BSP hardware initialisation (§11.1 step 1).
pub fn init(boot_info: &BootInfo) {
    // SAFETY: called once by kernel_main before any other subsystem.
    unsafe { boot::init_bsp(boot_info) };
}

/// Program the local APIC timer on the calling core (§9.1).
/// Must be called after `memory::init` so the HHDM offset is available.
pub fn init_timer() {
    // SAFETY: called after memory::init; HHDM offset is set.
    unsafe { boot::init_local_apic() };
}

/// Per-AP hardware initialisation called from `ap_main`.
pub fn ap_init(core_id: u32) {
    let _ = core_id;
    // SAFETY: called once per AP from ap_main after long-mode entry.
    unsafe {
        boot::init_gdt();
        boot::init_idt();
        boot::init_local_apic();
    }
}

/// Halt this core. Disables interrupts and loops on hlt.
/// Milestone 6: broadcast NMI IPI to other cores before halting.
pub fn halt_all_cores() -> ! {
    // SAFETY: panic path — we want to stop all execution permanently.
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    loop {
        // SAFETY: hlt with IF=0 is safe; we never exit this loop.
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) };
    }
}

// ---------------------------------------------------------------------------
// Serial (COM1) — used by log::write_fmt for all kprintln! output.
// ---------------------------------------------------------------------------

const COM1: u16 = 0x3F8;

// Spinlock protecting all COM1 port I/O. Prevents concurrent UART access
// from multiple cores from corrupting the THRE poll / TX FIFO state.
static SERIAL_LOCK: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Initialise COM1 at 115200 baud, 8N1.
///
/// # Safety
/// Must be called once before `serial_write_byte`. Not reentrant.
pub unsafe fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00); // Disable UART interrupts
        outb(COM1 + 3, 0x80); // Enable DLAB to set baud divisor
        outb(COM1 + 0, 0x01); // Divisor lo: 1 → 115200 baud
        outb(COM1 + 1, 0x00); // Divisor hi
        outb(COM1 + 3, 0x03); // 8 data bits, no parity, 1 stop bit
        outb(COM1 + 2, 0xC7); // Enable FIFO, clear Tx/Rx, 14-byte threshold
        outb(COM1 + 4, 0x0B); // RTS + DTR set, OUT2 enabled (needed for IRQs)
    }
}

/// Write one byte to COM1. Spins until the transmit holding register is empty.
///
/// Thread-safe: serialized through `SERIAL_LOCK` so concurrent calls from
/// multiple cores cannot interleave THRE polls with TX writes.
///
/// # Safety
/// `serial_init` must have been called before the first call.
pub fn serial_write_byte(b: u8) {
    use core::sync::atomic::Ordering;
    // SAFETY: SERIAL_LOCK is a boolean spinlock; we hold it only for the
    // duration of one THRE poll + one outb, then release it unconditionally.
    while SERIAL_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: port I/O to COM1; initialised before first use in _start.
    unsafe {
        while (inb(COM1 + 5) & 0x20) == 0 {}  // wait: THR empty (LSR bit 5)
        outb(COM1, b);
    }
    SERIAL_LOCK.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Port I/O helpers.
// ---------------------------------------------------------------------------

#[inline]
unsafe fn outb(port: u16, val: u8) {
    // SAFETY: caller is responsible for port validity and timing.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    // SAFETY: caller is responsible for port validity.
    let val: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        );
    }
    val
}
