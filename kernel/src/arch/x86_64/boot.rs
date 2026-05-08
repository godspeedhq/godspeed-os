//! BSP/AP hardware initialisation — §11.1, §11.2.
//!
//! Sets up our own GDT and IDT. Limine provides a minimal GDT and has
//! already configured paging and long mode before calling _start, so
//! init_paging is a Milestone 2 placeholder.

use super::BootInfo;

// ---------------------------------------------------------------------------
// GDT — three 64-bit descriptors: null, kernel code (0x08), kernel data (0x10).
// ---------------------------------------------------------------------------
//
// Descriptor encoding (Intel manual, section 3.4.5):
//   Byte 0-1:  limit[15:0]
//   Byte 2-3:  base[15:0]
//   Byte 4:    base[23:16]
//   Byte 5:    access (Present|DPL|S|Type)
//   Byte 6:    flags[7:4] (G|D/B|L|AVL) | limit[19:16] in low nibble
//   Byte 7:    base[31:24]
//
// Kernel code: G=1, L=1 (64-bit), DPL=0 → 0x00AF_9A00_0000_FFFF
// Kernel data: G=1, D=1, DPL=0       → 0x00CF_9200_0000_FFFF

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
// IDT — 256 interrupt gates, all pointing at exception_halt until real
//        handlers are installed in later milestones.
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
            type_attr:   0x8E, // P=1, DPL=0, interrupt gate (1110)
            offset_mid:  (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved:   0,
        }
    }
}

// SAFETY: written only during init_idt before APs start; read-only after.
static mut IDT: [IdtEntry; 256] = [IdtEntry::ABSENT; 256];

// ---------------------------------------------------------------------------
// Shared descriptor table pointer format (for lgdt / lidt).
// ---------------------------------------------------------------------------

#[repr(C, packed)]
struct TableDescriptor {
    limit: u16,
    base:  u64,
}

// ---------------------------------------------------------------------------
// Public init surface.
// ---------------------------------------------------------------------------

/// BSP-only initialisation: GDT, IDT, paging stub, APIC stub.
///
/// # Safety
/// Called exactly once before any other kernel subsystem.
pub unsafe fn init_bsp(boot_info: &BootInfo) {
    // SAFETY: caller guarantees single-call, pre-subsystem invariant.
    unsafe {
        init_gdt();
        init_idt();
        init_paging(boot_info);
        init_local_apic();
    }
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
        // Reload data segments (kernel data selector = 0x10).
        core::arch::asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            in("ax") 0x10u16,
            options(nostack)
        );
        // Reload CS via far return: stack = [RIP, CS]; retfq pops RIP then CS.
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

/// Install a generic halt handler in all 256 IDT slots and load the IDT.
///
/// # Safety
/// Must be called after `init_gdt` (IDT entries reference the kernel CS).
pub(super) unsafe fn init_idt() {
    let handler = exception_halt as u64;
    // SAFETY: IDT is a kernel-lifetime static; APs haven't started yet.
    unsafe {
        for entry in IDT.iter_mut() {
            *entry = IdtEntry::new(handler);
        }
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

/// No-op: Limine sets up identity-mapped paging before calling _start.
/// Milestone 2 installs per-task page tables from the physical memory map.
unsafe fn init_paging(_boot_info: &BootInfo) {}

/// No-op: APIC timer for 10 ms quantum is Milestone 3 work (§9.1).
pub(super) unsafe fn init_local_apic() {}

// ---------------------------------------------------------------------------
// Exception stub — all vectors point here until real handlers exist.
// ---------------------------------------------------------------------------

/// Catch-all exception handler: disable interrupts and halt this core.
/// Any unhandled exception in Milestone 1 ends up here rather than
/// triple-faulting.
#[unsafe(naked)]
unsafe extern "C" fn exception_halt() -> ! {
    core::arch::naked_asm!(
        "cli",
        "2: hlt",
        "jmp 2b",
    )
}
