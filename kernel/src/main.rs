#![no_std]
#![no_main]

mod arch;
mod capability;
mod interrupt;
mod invariants;
mod ipc;
mod log;
mod memory;
mod smp;
mod syscall;
mod task;

use core::panic::PanicInfo;

/// Kernel entry point — called by the bootloader on the BSP only.
///
/// Execution order mirrors §11.1:
///   1. arch init (paging, IDT, GDT)
///   2. frame allocator + capability subsystem
///   3. bring APs online
///   4. mark all cores ready
///   5. spawn init on Core 0
#[no_mangle]
pub extern "C" fn kernel_main(boot_info_ptr: *const arch::x86_64::BootInfo) -> ! {
    // SAFETY: bootloader guarantees boot_info_ptr is valid and aligned.
    let boot_info = unsafe { &*boot_info_ptr };

    arch::x86_64::init(boot_info);
    memory::init(boot_info);
    capability::init();
    ipc::init();
    smp::init(boot_info);

    kprintln!("kernel: all cores ready");

    task::spawn_init();

    // BSP enters the scheduler; never returns.
    task::scheduler::run()
}

/// AP entry point — called by each secondary core after long-mode setup.
#[no_mangle]
pub extern "C" fn ap_main(core_id: u32) -> ! {
    arch::x86_64::ap_init(core_id);
    smp::core::mark_ready(core_id);
    task::scheduler::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Write panic reason to serial and the reserved crash page (§19).
    kprintln!("KERNEL PANIC: {}", info);
    arch::x86_64::halt_all_cores();
}
