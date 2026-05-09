//! BSP/AP hardware initialisation — §11.1, §11.2.

use super::BootInfo;

// ---------------------------------------------------------------------------
// GDT — three 64-bit descriptors: null, kernel code (0x08), kernel data (0x10).
// ---------------------------------------------------------------------------
//
// x86 sets the Accessed bit in GDT descriptors when segment registers are
// loaded, which is a hardware write to the GDT.  It must live in .data (rw-),
// not .rodata (r--), or the CPU will fault with a write-protection violation.
#[link_section = ".data"]
static GDT: [u64; 3] = [
    0x0000_0000_0000_0000, // null descriptor
    0x00AF_9A00_0000_FFFF, // kernel code: 64-bit, ring 0, execute/read
    0x00CF_9200_0000_FFFF, // kernel data: ring 0, read/write
];

// ---------------------------------------------------------------------------
// IDT — 256 interrupt gates.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low:  u16,
    selector:    u16, // kernel code segment = 0x08
    ist:         u8,  // 0 = use current RSP
    type_attr:   u8,  // 0x8E = present, ring 0, interrupt gate
    offset_mid:  u16,
    offset_high: u32,
    _reserved:   u32,
}

impl IdtEntry {
    const ABSENT: Self = Self {
        offset_low: 0, selector: 0, ist: 0, type_attr: 0,
        offset_mid: 0, offset_high: 0, _reserved: 0,
    };

    fn new(handler: u64) -> Self {
        Self {
            offset_low:  handler as u16,
            selector:    0x08,
            ist:         0,
            type_attr:   0x8E, // P=1, DPL=0, interrupt gate (IF cleared on entry)
            offset_mid:  (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved:   0,
        }
    }
}

// SAFETY: written only during init_idt before APs start; read-only after.
static mut IDT: [IdtEntry; 256] = [IdtEntry::ABSENT; 256];

// ---------------------------------------------------------------------------
// Shared descriptor table pointer format.
// ---------------------------------------------------------------------------

#[repr(C, packed)]
struct TableDescriptor {
    limit: u16,
    base:  u64,
}

// ---------------------------------------------------------------------------
// Local APIC MMIO — set during init_local_apic, read by apic_send_eoi.
// ---------------------------------------------------------------------------

static mut APIC_VIRT_BASE: u64 = 0;

// APIC register offsets (xAPIC MMIO, 32-bit accesses).
const APIC_ID:           u64 = 0x020;
const APIC_EOI:          u64 = 0x0B0;
const APIC_SPURIOUS:     u64 = 0x0F0;
const APIC_LVT_TIMER:    u64 = 0x320;
const APIC_TIMER_INIT:   u64 = 0x380;
const APIC_TIMER_DIVIDE: u64 = 0x3E0;

// ---------------------------------------------------------------------------
// Public init surface.
// ---------------------------------------------------------------------------

/// BSP-only initialisation: GDT, IDT (with timer stub), paging stub.
/// APIC timer is programmed separately via `init_local_apic` after memory init.
///
/// # Safety
/// Called exactly once before any other kernel subsystem.
pub unsafe fn init_bsp(boot_info: &BootInfo) {
    unsafe {
        mask_pic();   // silence 8259 before IDT is live; avoids vector-8 collision
        init_gdt();
        init_idt();
        init_paging(boot_info);
    }
}

/// Program the local APIC timer for a ~10 ms periodic interrupt on vector 32.
/// Must be called after `memory::init` (needs HHDM offset).
///
/// # Safety
/// Called once per core (BSP from kernel_main, APs from ap_main) after HHDM is set.
pub unsafe fn init_local_apic() {
    // Read APIC base physical address from IA32_APIC_BASE MSR (0x1B).
    let (lo, hi): (u32, u32);
    // SAFETY: RDMSR is privileged; ring 0 throughout kernel boot.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0x1Bu32,
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem),
        );
    }
    let apic_phys = ((hi as u64) << 32) | (lo as u64 & !0xFFF_u64);
    let apic_virt = crate::arch::x86_64::page_tables::get_hhdm_offset() + apic_phys;

    // Limine's HHDM maps RAM but not MMIO regions.  Ensure the APIC frame is
    // reachable by adding it to the active page tables before the first write.
    // PCD (bit 4) + PWT (bit 3) disable caching for MMIO correctness.
    {
        use crate::arch::x86_64::page_tables::{PageFlags, map_in_active_tables};
        let mmio_flags = PageFlags::PRESENT.bits()
                       | PageFlags::WRITABLE.bits()
                       | (1 << 3)   // PWT
                       | (1 << 4);  // PCD
        // SAFETY: called after set_hhdm_offset; APIC page is MMIO.
        unsafe { map_in_active_tables(apic_virt, apic_phys, mmio_flags) }
            .unwrap_or_else(|_| {
                // If the mapping already exists (second core, or pre-mapped),
                // that is fine — we just proceed.
            });
    }

    // SAFETY: APIC_VIRT_BASE written once per core before apic_send_eoi is called.
    unsafe { APIC_VIRT_BASE = apic_virt };

    // Enable APIC software: set bit 8 of the spurious interrupt vector register.
    // Spurious vector = 0xFF (unused; interrupt gates handle real vectors).
    write_apic(apic_virt, APIC_SPURIOUS, 0x1FF);

    // LVT timer: periodic mode (bit 17), vector 32 (0x20).
    write_apic(apic_virt, APIC_LVT_TIMER, (1 << 17) | 0x20);

    // Divide by 16.
    write_apic(apic_virt, APIC_TIMER_DIVIDE, 0x03);

    // Initial count → ~10 ms at a 1 GHz APIC bus / 16 divider (QEMU default).
    // 1 GHz / 16 = 62.5 MHz; 62.5 MHz × 0.01 s = 625,000 ticks.
    write_apic(apic_virt, APIC_TIMER_INIT, 625_000);
}

/// Send an End-Of-Interrupt signal to the local APIC.
///
/// # Safety
/// Must be called from interrupt context (after `init_local_apic`).
pub unsafe fn apic_send_eoi() {
    // SAFETY: APIC_VIRT_BASE is valid after init_local_apic; write 0 to EOI reg.
    unsafe { write_apic(APIC_VIRT_BASE, APIC_EOI, 0) };
}

/// Return the virtual base address of this core's local APIC.
///
/// # Safety
/// Valid only after `init_local_apic` has been called on this core.
pub unsafe fn get_apic_virt_base() -> u64 {
    // SAFETY: APIC_VIRT_BASE is set in init_local_apic before any use.
    unsafe { APIC_VIRT_BASE }
}

/// Read the local APIC ID register and return the ID (bits 31:24).
///
/// # Safety
/// Valid only after `init_local_apic` has been called on this core.
pub unsafe fn get_lapic_id() -> u32 {
    // SAFETY: APIC_VIRT_BASE is set in init_local_apic; read_volatile is safe for MMIO.
    unsafe {
        let val = ((APIC_VIRT_BASE + APIC_ID) as *const u32).read_volatile();
        (val >> 24) & 0xFF
    }
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Write a byte to an x86 I/O port.
#[inline]
unsafe fn outb(port: u16, val: u8) {
    // SAFETY: caller selects ports that are safe to write (PIC, diagnostic).
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nostack, nomem),
        );
        // Short I/O delay via the diagnostic port so old hardware settles.
        core::arch::asm!(
            "out 0x80, al",
            in("al") 0u8,
            options(nostack, nomem),
        );
    }
}

/// Remap the legacy 8259 PIC to vectors 0x20–0x2F then mask all IRQs.
///
/// Without remapping, the PIC's IRQ0 (system timer) fires at vector 8, which
/// is the double-fault entry.  We use the APIC for all timing so the 8259
/// must be silenced before we enable interrupts.
unsafe fn mask_pic() {
    // SAFETY: ICW/OCW writes to 0x20/0xA0 (command) and 0x21/0xA1 (data)
    //         are the standard 8259 programming sequence; no side effects on
    //         non-existent PIC (virtual QEMU environment).
    unsafe {
        outb(0x20, 0x11);     // ICW1: init master PIC with ICW4
        outb(0xA0, 0x11);     // ICW1: init slave  PIC with ICW4
        outb(0x21, 0x20);     // ICW2: master IRQ0–7 → vectors 32–39
        outb(0xA1, 0x28);     // ICW2: slave  IRQ8–15 → vectors 40–47
        outb(0x21, 0x04);     // ICW3: master has slave on IRQ2
        outb(0xA1, 0x02);     // ICW3: slave cascade identity = 2
        outb(0x21, 0x01);     // ICW4: 8086 mode
        outb(0xA1, 0x01);     // ICW4: 8086 mode
        outb(0x21, 0xFF);     // OCW1: mask all master IRQs
        outb(0xA1, 0xFF);     // OCW1: mask all slave  IRQs
    }
}

#[inline]
unsafe fn write_apic(base: u64, reg: u64, val: u32) {
    // SAFETY: base + reg is the APIC MMIO address; volatile write is required.
    unsafe { ((base + reg) as *mut u32).write_volatile(val) };
}

/// Load our 64-bit GDT and reload all segment registers.
///
/// # Safety
/// Must be called with a valid stack; invalidates the current CS/DS/ES/SS.
pub(super) unsafe fn init_gdt() {
    let desc = TableDescriptor {
        limit: (core::mem::size_of_val(&GDT) - 1) as u16,
        base:  GDT.as_ptr() as u64,
    };
    // SAFETY: GDT lives in .data (writable); desc is valid for the duration of lgdt.
    unsafe {
        core::arch::asm!(
            "lgdt [{desc}]",
            desc = in(reg) &desc as *const TableDescriptor as u64,
            options(nostack, readonly)
        );
        core::arch::asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            in("ax") 0x10u16,
            options(nostack)
        );
        // Reload CS via far return: push [CS, RIP], retfq pops RIP then CS.
        core::arch::asm!(
            "push {sel}",
            "lea {tmp}, [rip + 99f]",
            "push {tmp}",
            "retfq",
            "99:",
            sel = in(reg)  0x08u64,
            tmp = lateout(reg) _,
            options(nostack)
        );
    }
}

/// Install ISR stubs in all 256 IDT slots, then load the IDT.
///
/// - All vectors default to `exception_halt`.
/// - Vector 13  → GPF diagnostic handler (prints error + RIP, halts).
/// - Vector 14  → Page-fault diagnostic handler (prints CR2 + error, halts).
/// - Vector 32  → APIC timer preemption (§9.1).
/// - Vector 0xF0 → WAKE_RECEIVER IPI.
/// - Vector 0xF1 → TLB_SHOOTDOWN IPI.
/// - Vector 0xF2 → SCHEDULER_TICK IPI.
///
/// # Safety
/// Must be called after `init_gdt` (entries reference the kernel CS = 0x08).
pub(super) unsafe fn init_idt() {
    let halt    = exception_halt as u64;
    let timer   = super::interrupts::timer_isr_stub as u64;

    // SAFETY: IDT is a kernel-lifetime static; APs haven't started yet.
    unsafe {
        for entry in IDT.iter_mut() {
            *entry = IdtEntry::new(halt);
        }
        IDT[13]   = IdtEntry::new(gpf_stub  as u64);
        IDT[14]   = IdtEntry::new(pf_stub   as u64);
        IDT[32]   = IdtEntry::new(timer);
        IDT[0xF0] = IdtEntry::new(ipi_wake_stub   as u64);
        IDT[0xF1] = IdtEntry::new(ipi_tlb_stub    as u64);
        IDT[0xF2] = IdtEntry::new(ipi_tick_stub   as u64);

        let desc = TableDescriptor {
            limit: (core::mem::size_of_val(&IDT) - 1) as u16,
            base:  IDT.as_ptr() as u64,
        };
        core::arch::asm!(
            "lidt [{desc}]",
            desc = in(reg) &desc as *const TableDescriptor as u64,
            options(nostack, readonly)
        );
    }
}

// ---------------------------------------------------------------------------
// IPI ISR stubs (§9.4) — one naked stub per vector.
// ---------------------------------------------------------------------------

/// Dispatch target called from all three IPI stubs with the vector number.
///
/// # Safety
/// Called from raw interrupt context (IF=0).
#[no_mangle]
unsafe extern "C" fn ipi_dispatch(vector: u64) {
    // SAFETY: called from raw ISR with IF=0; ipi_handler is safe to call here.
    unsafe { crate::smp::ipi::ipi_handler(vector as u8) }
}

#[unsafe(naked)]
unsafe extern "C" fn ipi_wake_stub() {
    core::arch::naked_asm!(
        "push rax", "push rcx", "push rdx",
        "push rdi", "push rsi", "push r8",
        "push r9",  "push r10", "push r11",
        "mov rdi, 0xF0",
        "call ipi_dispatch",
        "pop r11", "pop r10", "pop r9",
        "pop r8",  "pop rsi", "pop rdi",
        "pop rdx", "pop rcx", "pop rax",
        "iretq",
    )
}

#[unsafe(naked)]
unsafe extern "C" fn ipi_tlb_stub() {
    core::arch::naked_asm!(
        "push rax", "push rcx", "push rdx",
        "push rdi", "push rsi", "push r8",
        "push r9",  "push r10", "push r11",
        "mov rdi, 0xF1",
        "call ipi_dispatch",
        "pop r11", "pop r10", "pop r9",
        "pop r8",  "pop rsi", "pop rdi",
        "pop rdx", "pop rcx", "pop rax",
        "iretq",
    )
}

#[unsafe(naked)]
unsafe extern "C" fn ipi_tick_stub() {
    core::arch::naked_asm!(
        "push rax", "push rcx", "push rdx",
        "push rdi", "push rsi", "push r8",
        "push r9",  "push r10", "push r11",
        "mov rdi, 0xF2",
        "call ipi_dispatch",
        "pop r11", "pop r10", "pop r9",
        "pop r8",  "pop rsi", "pop rdi",
        "pop rdx", "pop rcx", "pop rax",
        "iretq",
    )
}

/// No-op: Limine sets up identity-mapped paging before calling _start.
unsafe fn init_paging(_boot_info: &BootInfo) {}

// ---------------------------------------------------------------------------
// Diagnostic exception stubs — vectors 13 (GPF) and 14 (#PF).
//
// Both exceptions push an error code before RIP on the stack, so on entry:
//   [RSP+0]  = error_code
//   [RSP+8]  = saved RIP (fault address)
//   [RSP+16] = saved CS
//   ...
// ---------------------------------------------------------------------------

/// GPF stub: read error code + RIP, call diagnostic handler.
#[unsafe(naked)]
unsafe extern "C" fn gpf_stub() -> ! {
    // SAFETY: vector 13 pushes error_code then RIP; reads are before any RSP change.
    core::arch::naked_asm!(
        "mov rdi, [rsp]",      // error_code → first arg
        "mov rsi, [rsp + 8]",  // saved RIP  → second arg
        "call gpf_handler",
        "2: hlt",
        "jmp 2b",
    )
}

/// Page-fault stub: read error code + RIP, call diagnostic handler.
#[unsafe(naked)]
unsafe extern "C" fn pf_stub() -> ! {
    // SAFETY: vector 14 pushes error_code then RIP.
    core::arch::naked_asm!(
        "mov rdi, [rsp]",      // error_code → first arg
        "mov rsi, [rsp + 8]",  // saved RIP  → second arg
        "call pf_handler",
        "2: hlt",
        "jmp 2b",
    )
}

/// Print GPF info and halt all cores.
#[no_mangle]
unsafe extern "C" fn gpf_handler(error_code: u64, fault_rip: u64) -> ! {
    crate::kprintln!(
        "KERNEL GPF: error_code={:#x} rip={:#x}",
        error_code, fault_rip
    );
    crate::arch::x86_64::halt_all_cores()
}

/// Print page-fault info and halt all cores.
#[no_mangle]
unsafe extern "C" fn pf_handler(error_code: u64, fault_rip: u64) -> ! {
    let cr2: u64;
    // SAFETY: reading CR2 in ring 0 is always valid.
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem)) };
    crate::kprintln!(
        "KERNEL PF: fault_addr={:#x} error_code={:#x} rip={:#x}",
        cr2, error_code, fault_rip
    );
    crate::arch::x86_64::halt_all_cores()
}

// ---------------------------------------------------------------------------
// Exception stub — all unhandled vectors point here.
// ---------------------------------------------------------------------------

#[unsafe(naked)]
unsafe extern "C" fn exception_halt() -> ! {
    core::arch::naked_asm!(
        "cli",
        "2: hlt",
        "jmp 2b",
    )
}
