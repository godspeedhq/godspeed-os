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

// ---------------------------------------------------------------------------
// Demo task stacks — Milestone 3.
// 16-byte aligned wrappers so the initial RSP is properly aligned.
// Replaced by frame-allocator stacks in Milestone 7.
// ---------------------------------------------------------------------------

#[repr(C, align(16))]
struct KernelStack([u8; 16 * 1024]);

static mut STACK_A: KernelStack = KernelStack([0; 16 * 1024]);
static mut STACK_B: KernelStack = KernelStack([0; 16 * 1024]);

/// Demo task A: counts iterations and prints every ~1 M loops.
unsafe extern "C" fn task_a() -> ! {
    let mut n: u64 = 0;
    loop {
        n = n.wrapping_add(1);
        if n % 5_000_000 == 0 {
            kprintln!("task-a: {}", n / 5_000_000);
        }
    }
}

/// Demo task B: counts iterations and prints every ~1 M loops.
unsafe extern "C" fn task_b() -> ! {
    let mut n: u64 = 0;
    loop {
        n = n.wrapping_add(1);
        if n % 5_000_000 == 0 {
            kprintln!("task-b: {}", n / 5_000_000);
        }
    }
}

// ---------------------------------------------------------------------------
// Kernel entry point.
// ---------------------------------------------------------------------------

/// Called by the bootloader on the BSP only.
#[no_mangle]
pub extern "C" fn kernel_main(boot_info_ptr: *const arch::x86_64::BootInfo) -> ! {
    // SAFETY: bootloader guarantees boot_info_ptr is valid and aligned.
    let boot_info = unsafe { &*boot_info_ptr };

    // SAFETY: debug probe — port 0xe9 is QEMU debug console.
    unsafe { core::arch::asm!("out 0xe9, al", in("al") b'M', options(nostack, nomem)) };

    arch::x86_64::init(boot_info);

    unsafe { core::arch::asm!("out 0xe9, al", in("al") b'G', options(nostack, nomem)) };

    memory::init(boot_info);

    // Program the APIC timer now that HHDM offset is available (§9.1).
    arch::x86_64::init_timer();

    capability::init();
    ipc::init();
    smp::init(boot_info);

    kprintln!("kernel: all cores ready");

    // Enqueue two demo tasks that prove round-robin preemption works (§22 Test 8).
    let cr3: u64;
    // SAFETY: reading CR3 is always valid in ring 0.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem)) };

    let ctx_a = unsafe {
        arch::x86_64::context_switch::TaskContext::new_kernel(
            task_a,
            STACK_A.0.as_mut_ptr().add(STACK_A.0.len()),
            cr3,
        )
    };
    let ctx_b = unsafe {
        arch::x86_64::context_switch::TaskContext::new_kernel(
            task_b,
            STACK_B.0.as_mut_ptr().add(STACK_B.0.len()),
            cr3,
        )
    };

    task::scheduler::enqueue("task-a", ctx_a);
    task::scheduler::enqueue("task-b", ctx_b);

    kprintln!("scheduler: task-a and task-b enqueued");

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
    kprintln!("KERNEL PANIC: {}", info);
    arch::x86_64::halt_all_cores();
}
