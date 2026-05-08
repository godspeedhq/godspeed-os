//! BSP (Bootstrap Processor) initialisation — §11.1, §11.2.
//!
//! Called once by `kernel_main` on the first core to execute.
//! Sets up paging, GDT, IDT, and the local APIC before APs are started.

use super::BootInfo;

/// Perform early BSP-only hardware init.
///
/// # Safety
/// Must be called exactly once, before any other kernel subsystem.
pub unsafe fn init_bsp(boot_info: &BootInfo) {
    // SAFETY: caller guarantees single-call, pre-subsystem invariant.
    unsafe {
        init_gdt();
        init_idt();
        init_paging(boot_info);
        init_local_apic();
    }
}

pub(super) unsafe fn init_gdt() {
    todo!("load 64-bit GDT with kernel CS/DS and TSS")
}

pub(super) unsafe fn init_idt() {
    todo!("install IDT: exception vectors + IRQ stubs → interrupt::route dispatch")
}

unsafe fn init_paging(boot_info: &BootInfo) {
    todo!("map kernel image + memory map from boot_info into page tables")
}

pub(super) unsafe fn init_local_apic() {
    todo!("configure BSP local APIC, enable timer interrupt for 10ms quantum (§9.1)")
}
