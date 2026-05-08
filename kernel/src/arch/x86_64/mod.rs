//! x86_64 architecture layer — the unsafe boundary (§18.1).
//!
//! All `unsafe` code in the kernel that touches hardware directly lives in
//! this module or in `memory/`, `capability/`, `smp/`. Nowhere else.

pub mod ap_boot;
pub mod boot;
pub mod context_switch;
pub mod interrupts;
pub mod page_tables;

/// Boot information passed from the bootloader to `kernel_main`.
#[repr(C)]
pub struct BootInfo {
    /// Physical memory map entries.
    pub memory_map: &'static [MemoryRegion],
    /// APIC IDs of all detected processors (index 0 is the BSP).
    pub ap_ids: &'static [u32],
    /// Physical address where the kernel image was loaded.
    pub kernel_phys_start: u64,
    pub kernel_phys_end: u64,
}

#[repr(C)]
pub struct MemoryRegion {
    pub base: u64,
    pub len: u64,
    pub kind: MemoryKind,
}

#[repr(u32)]
pub enum MemoryKind {
    Usable = 1,
    Reserved = 2,
    AcpiReclaimable = 3,
    KernelImage = 4,
}

/// Full BSP hardware initialisation (§11.1 step 1).
pub fn init(boot_info: &BootInfo) {
    // SAFETY: called once by kernel_main before any other subsystem.
    unsafe { boot::init_bsp(boot_info) };
}

/// Per-AP hardware initialisation (§11.1).
pub fn ap_init(core_id: u32) {
    // SAFETY: called once per AP from ap_main after long-mode entry.
    unsafe {
        boot::init_gdt();
        boot::init_idt();
        boot::init_local_apic();
    }
    crate::kprintln!("smp: core {} ready", core_id);
}

/// Write one byte to the serial console (COM1). Used by the ring buffer.
///
/// # Safety
/// Caller must ensure COM1 is initialised and not concurrently written.
pub fn serial_write_byte(b: u8) {
    // SAFETY: direct port I/O; COM1 is always initialised early in BSP boot.
    unsafe {
        core::arch::asm!(
            "out 0x3F8, al",
            in("al") b,
            options(nomem, nostack)
        )
    }
}

/// Halt this core and send NMI to all other cores (§19, panic handler).
pub fn halt_all_cores() -> ! {
    // SAFETY: panic path — correctness no longer required.
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    todo!("broadcast NMI IPI; hlt loop");
}
