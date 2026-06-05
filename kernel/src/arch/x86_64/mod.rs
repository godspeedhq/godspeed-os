//! x86_64 architecture layer — the unsafe boundary (§18.1).
//!
//! All `unsafe` code in the kernel that touches hardware directly lives in
//! this module or in `memory/`, `capability/`, `smp/`. Nowhere else.

pub mod ap_boot;
pub mod boot;
pub mod context_switch;
pub mod fb;
pub mod interrupts;
pub mod page_tables;
pub mod pci;
pub mod syscall_entry;

use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, MpRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

// Kernel virtual extent from the linker script (kernel.ld).
// Used to compute the physical range to exclude from the frame allocator.
extern "C" {
    // SAFETY: linker symbol; valid virtual address marking the end of .bss.
    static __bss_end: u8;
}

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
#[link_section = ".requests"]
static KERNEL_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[link_section = ".requests"]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

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

    // Bring up the framebuffer console before the first kprintln so all boot
    // output mirrors to the display (§11.4). The framebuffer Limine mapped is in
    // the higher half and stays valid for the system lifetime.
    if let Some(resp) = FRAMEBUFFER_REQUEST.response() {
        if let Some(&fb) = resp.framebuffers().first() {
            fb::fb_init(fb);
        }
    }

    let boot_info = collect_boot_info();

    // _start never returns (kernel_main is -> !), so boot_info on this stack
    // frame is valid for the entire kernel lifetime.
    crate::kernel_main(&boot_info as *const _)
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

    // Compute the physical range of the kernel image so the frame allocator
    // can exclude it. BSS (including KSTACK_STORAGE) lives past the file-backed
    // sections; we use __bss_end to cover the full loaded extent.
    let (kernel_phys_start, kernel_phys_end) = KERNEL_ADDRESS_REQUEST
        .response()
        .map(|r| {
            let phys_base = r.physical_base;
            let virt_base = r.virtual_base;
            // __bss_end is a linker symbol; its address is the first virtual
            // byte past the kernel's .bss section. addr_of! does not deref.
            let virt_end = core::ptr::addr_of!(__bss_end) as u64;
            let kernel_size = virt_end.saturating_sub(virt_base);
            // Round up to the next page boundary.
            let phys_end = phys_base
                .saturating_add(kernel_size)
                .saturating_add(0xFFF) & !0xFFF_u64;
            (phys_base, phys_end)
        })
        .unwrap_or((0, 0));

    BootInfo {
        // SAFETY: MAP_BUF written above in single-threaded boot; slice is valid for kernel lifetime.
        memory_map: unsafe { &MAP_BUF[..count] },
        // SAFETY: AP_ID_BUF written above in single-threaded boot; slice is valid for kernel lifetime.
        ap_ids: unsafe { &AP_ID_BUF[..ap_count] },
        kernel_phys_start,
        kernel_phys_end,
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
    // SAFETY: called once per AP from ap_main after long-mode entry.
    unsafe {
        boot::init_gdt(core_id);
        boot::init_idt();
        boot::init_local_apic();
        boot::init_syscall(core_id);
    }
}

pub use interrupts::{disable_interrupts, enable_interrupts, wait_for_interrupt};
pub use syscall_entry::{read_cycle_counter, read_user_bytes, validate_user_ptr, write_user_bytes};

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

/// Issue an x86 hardware reset via the keyboard controller CPU reset line.
///
/// Writes 0xFE to I/O port 0x64, asserting CPURST# and causing an unconditional
/// hardware reset. Used by the Reboot syscall (18). Does not return.
pub fn hardware_reset() -> ! {
    // SAFETY: CLI before touching the KBC so no ISR reads port 0x60 between
    // our status poll and the reset command.
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };

    // Wait for KBC input buffer empty (status bit 1 = 0).
    // SAFETY: port 0x64 is the standard keyboard controller status/command port.
    unsafe {
        loop {
            if inb(0x64) & 0x02 == 0 { break; }
            core::hint::spin_loop();
        }
        // 0xFE on port 0x64 pulses the CPURST# line — unconditional CPU reset.
        // SAFETY: keyboard controller command; universally supported on x86.
        outb(0x64, 0xFE);
    }

    // Reset propagates in a few µs. Spin in case it doesn't fire immediately.
    loop {
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
    // Bounded best-effort lock acquire: a wedged SERIAL_LOCK must never spin a
    // core forever with IF=0 (see bugs/1_CROSS_CORE_IPC_REPLY_TO_BSP_STALLS.md).
    let mut got = false;
    let mut t = 0u32;
    while t < SERIAL_LOCK_SPIN_CAP {
        if SERIAL_LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            got = true;
            break;
        }
        core::hint::spin_loop();
        t += 1;
    }
    // SAFETY: port I/O to COM1; initialised before first use in _start.
    unsafe {
        // Bounded THRE poll: drop the byte rather than spin forever if the UART
        // never reports THR-empty (the unbounded poll was a real IF=0 wedge).
        if serial_thre_wait() {
            outb(COM1, b);
        }
    }
    if got {
        SERIAL_LOCK.store(false, Ordering::Release);
    }
    // Log stream → COM1. During boot it is ALSO mirrored to the framebuffer so the
    // user sees the init sequence on the TV; the shell ends this once boot output
    // settles (console_boot_complete). After that, logs are serial-only and only
    // the console path reaches the TV (Stage 1; docs/console-service.md).
    if boot_log_to_fb() {
        fb::put_byte(b);
    }
}

/// Spin cap for best-effort `SERIAL_LOCK` acquisition (~seconds on real HW).
const SERIAL_LOCK_SPIN_CAP: u32 = 5_000_000;
/// Spin cap for the COM1 THRE (transmit-holding-register-empty) poll.
const THRE_SPIN_CAP: u32 = 1_000_000;

/// Poll COM1 LSR bit 5 (THR empty) up to `THRE_SPIN_CAP` times.
/// Returns `true` if the transmit register became empty, `false` on timeout.
///
/// # Safety
/// COM1 must be initialised; performs port I/O.
#[inline]
unsafe fn serial_thre_wait() -> bool {
    let mut k = 0u32;
    while k < THRE_SPIN_CAP {
        // SAFETY: reading the COM1 line-status register.
        if unsafe { inb(COM1 + 5) } & 0x20 != 0 {
            return true;
        }
        core::hint::spin_loop();
        k += 1;
    }
    false
}

/// Write bytes to COM1, serializing the whole sequence through `SERIAL_LOCK`.
///
/// Best-effort lock: if `SERIAL_LOCK` can't be acquired within the spin cap it
/// proceeds anyway (so it still works during a hang where another core holds the
/// lock), but normally it serializes with `serial_write_byte` so concurrent
/// writers can't corrupt each other's THRE poll / TX FIFO state.  Every THRE
/// poll is bounded (drop the byte on timeout) so a stuck UART can't wedge a core
/// with IF=0.  (Formerly bypassed the lock with an unbounded poll — a real bug.)
pub fn serial_write_bytes_lockfree(s: &[u8]) {
    use core::sync::atomic::Ordering;
    let mut got = false;
    let mut t = 0u32;
    while t < SERIAL_LOCK_SPIN_CAP {
        if SERIAL_LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            got = true;
            break;
        }
        core::hint::spin_loop();
        t += 1;
    }
    for &b in s {
        // SAFETY: port I/O to COM1; bounded THRE poll, drop the byte on timeout.
        unsafe {
            if serial_thre_wait() {
                outb(COM1, b);
            }
        }
    }
    if got {
        SERIAL_LOCK.store(false, Ordering::Release);
    }
    // Log stream → COM1, mirrored to the framebuffer during boot. See
    // `serial_write_byte`.
    if boot_log_to_fb() {
        for &b in s {
            fb::put_byte(b);
        }
    }
}

/// Write one byte to the **interactive console** — COM1 *and* the framebuffer
/// (TV). This is the CONSOLE path (the shell prompt, `observe`, keystroke echo);
/// kept separate from the log path so logs don't smear the TV. See
/// `docs/console-service.md` (Stage 1).
pub fn console_write_byte(b: u8) {
    serial_write_byte(b);
    // During boot, `serial_write_byte` already mirrored to the framebuffer; adding
    // it again here would double-render every console glyph. After boot-complete,
    // the log mirror is off, so the console path is what puts console output on the
    // TV.
    if !boot_log_to_fb() {
        fb::put_byte(b);
    }
}

/// Write bytes to the interactive console — COM1 (serialised) and the framebuffer.
pub fn console_write_bytes(s: &[u8]) {
    serial_write_bytes_lockfree(s);
    if !boot_log_to_fb() {
        for &b in s {
            fb::put_byte(b);
        }
    }
}

// ---------------------------------------------------------------------------
// Serial (COM2) — control channel for `osdev restart` (§17).
// ---------------------------------------------------------------------------

const COM2: u16 = 0x2F8;

/// Initialise COM2 at 115200 baud, 8N1. Receive-only (no TX interrupts).
///
/// Idempotent: re-initialising reinitialises the port to the same settings.
pub fn com2_init() {
    // SAFETY: COM2 I/O ports; standard UART register layout; ring-0 port I/O is always permitted.
    unsafe {
        outb(COM2 + 1, 0x00); // Disable UART interrupts
        outb(COM2 + 3, 0x80); // DLAB on
        outb(COM2 + 0, 0x01); // 115200 baud divisor lo
        outb(COM2 + 1, 0x00); // divisor hi
        outb(COM2 + 3, 0x03); // 8N1
        outb(COM2 + 2, 0xC7); // FIFO on, clear
        outb(COM2 + 4, 0x0B); // RTS + DTR
    }
}

/// Read one byte from COM2 if the receive data register is non-empty.
pub fn com2_try_read_byte() -> Option<u8> {
    // SAFETY: COM2 port reads; initialised before first use.
    unsafe {
        if inb(COM2 + 5) & 0x01 != 0 {
            Some(inb(COM2))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// COM1 UART RX — interrupt-driven ring buffer for the shell service.
// ---------------------------------------------------------------------------

const COM1_RX_BUF_SIZE: usize = 64;

/// Single-producer (IRQ handler) / single-consumer (ConsoleRead syscall) ring buffer.
/// head = read index (consumer advances), tail = write index (producer advances).
/// Buffer is full when (tail - head) == COM1_RX_BUF_SIZE.
// SAFETY: Access is synchronised by the SPSC protocol: the producer (IRQ handler)
// only writes to indices [tail, next_tail) and the consumer only reads from [head, head+1).
// The atomic head/tail ensure visibility across cores via Release/Acquire ordering.
static mut COM1_RX_BUF: [u8; COM1_RX_BUF_SIZE] = [0u8; COM1_RX_BUF_SIZE];
static COM1_RX_HEAD: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static COM1_RX_TAIL: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Task slot waiting for a console byte (u32::MAX = nobody blocked).
pub static CONSOLE_READ_WAITER: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(u32::MAX);

/// Whether `console_push_byte` echoes keystrokes to the console (serial + TV).
/// Normally true. A foreground full-screen app (e.g. live `observe`) sets it
/// false while it owns the screen, so raw keystrokes (its `q`-to-quit poll) do
/// not smear its frame; the app paints the display itself. Restored on exit.
pub static CONSOLE_ECHO_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

/// Set keystroke echo on/off. Called from the `ConsoleEcho` syscall by a service
/// holding the CONSOLE_READ cap (the shell, or a foreground app).
pub fn set_console_echo(on: bool) {
    CONSOLE_ECHO_ENABLED.store(on, core::sync::atomic::Ordering::Release);
}

/// Whether boot-time **log** output is also mirrored to the framebuffer (TV).
/// True during boot so the user sees the init sequence on the display; the shell
/// flips it false on the first keystroke and clears the screen, leaving a clean
/// interactive console (after that, only console output reaches the TV — Stage 1).
pub static BOOT_LOG_TO_FB: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

#[inline]
fn boot_log_to_fb() -> bool {
    BOOT_LOG_TO_FB.load(core::sync::atomic::Ordering::Acquire)
}

/// End boot-log mirroring to the framebuffer and clear the screen. Called from
/// the `ConsoleBootComplete` syscall once boot output has settled — the boot
/// jargon has served its purpose; hand over a clean console.
pub fn console_boot_complete() {
    BOOT_LOG_TO_FB.store(false, core::sync::atomic::Ordering::Release);
    fb::clear_and_home();
}

/// Set true by the USB keyboard driver (xHCI) once it has finished its setup —
/// in every terminal path: keyboard enumerated, no keyboard found, or no
/// controller/DMA. This is the deterministic end-of-boot signal: the input driver
/// is the last thing to come up, so when it reports in, the boot sequence is done.
/// The shell waits on this to auto-clear the boot screen — no timer, no heuristic.
pub static INPUT_READY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Mark input-subsystem setup complete (from the `SignalInputReady` syscall, called
/// by the xHCI driver). The end-of-boot signal the shell watches.
pub fn set_input_ready() {
    INPUT_READY.store(true, core::sync::atomic::Ordering::Release);
}

/// Whether the input driver has reported in (exposed via `InspectKernel` query 10).
pub fn input_ready() -> bool {
    INPUT_READY.load(core::sync::atomic::Ordering::Acquire)
}

/// Enable COM1 RX interrupts (call once after com2_init, from kernel main).
///
/// # Safety
/// Must be called after serial_init and after the IDT is loaded with vector 36.
pub unsafe fn uart_rx_enable() {
    // Unmask IRQ 4 (COM1) on the master PIC.  mask_pic() left OCW1 = 0xFF.
    // Clearing bit 4 enables IRQ 4; all other IRQs remain masked.
    // SAFETY: PIC port I/O; must run after mask_pic() sets OCW1=0xFF.
    unsafe {
        let mask = inb(0x21);
        outb(0x21, mask & 0xEF); // clear bit 4 (IRQ 4 = COM1)
        outb(COM1 + 1, 0x01);    // IER: enable RX data available interrupt
    }
}

/// Push a byte into the COM1 RX ring buffer (called from IRQ handler).
///
/// # Safety
/// Must be called only from the IRQ handler (single producer).
pub unsafe fn uart_rx_push(b: u8) {
    use core::sync::atomic::Ordering;
    let tail = COM1_RX_TAIL.load(Ordering::Relaxed);
    let head = COM1_RX_HEAD.load(Ordering::Acquire);
    let next_tail = (tail + 1) % COM1_RX_BUF_SIZE;
    if next_tail == head {
        return; // buffer full — drop byte
    }
    // SAFETY: tail index is within COM1_RX_BUF bounds; only this producer writes to it.
    unsafe { COM1_RX_BUF[tail] = b; }
    COM1_RX_TAIL.store(next_tail, Ordering::Release);
}

/// Pop one byte from the COM1 RX ring buffer (called from ConsoleRead syscall).
///
/// Returns `None` if the buffer is empty.
pub fn uart_rx_pop() -> Option<u8> {
    use core::sync::atomic::Ordering;
    let head = COM1_RX_HEAD.load(Ordering::Relaxed);
    let tail = COM1_RX_TAIL.load(Ordering::Acquire);
    if head == tail {
        return None;
    }
    // SAFETY: head index is within COM1_RX_BUF bounds; only the consumer reads here.
    let b = unsafe { COM1_RX_BUF[head] };
    COM1_RX_HEAD.store((head + 1) % COM1_RX_BUF_SIZE, Ordering::Release);
    Some(b)
}

/// Poll COM1 RX and wake any task blocked in ConsoleRead.
///
/// Called from the core-0 timer ISR every 10 ms.
/// Replaces IRQ-driven reception — the legacy PIC (IRQ 4) is fully masked;
/// APIC-only kernels must poll the UART LSR instead.
pub fn uart_rx_poll() {
    use core::sync::atomic::Ordering;
    // SAFETY: called from timer ISR with IF=0; uart_rx_drain_fifo is safe
    // to call here because: (a) timer ISR is IF=0 so no re-entrancy, and
    // (b) this is the only producer path (uart_rx_isr_stub is dead code
    // since IRQ 4 is masked).
    unsafe { uart_rx_drain_fifo(); }
    let head = COM1_RX_HEAD.load(Ordering::Acquire);
    let tail = COM1_RX_TAIL.load(Ordering::Acquire);
    if head != tail {
        let waiter = CONSOLE_READ_WAITER.load(Ordering::Acquire);
        if waiter != u32::MAX {
            crate::task::scheduler::wake_by_slot(waiter as usize, 0);
        }
    }
}

/// Inject one byte into the console input ring from a userspace input driver
/// (the USB keyboard, §12), then wake any blocked ConsoleRead. Mirrors the COM1
/// poll path's push + wake, so USB keystrokes reach the shell exactly like
/// serial bytes would. On the target hardware COM1 RX is dead, so the driver is
/// the only producer in practice; a concurrent COM1 poll would race the ring
/// tail — acceptable while COM1 input is unused (a per-ring lock is future work).
pub fn console_push_byte(b: u8) {
    use core::sync::atomic::Ordering;
    // Echo the keystroke to the console (serial + framebuffer) so the user sees
    // their input inline — the framebuffer has no terminal-side local echo, so
    // without this typing is invisible on a display. (On a serial terminal, turn
    // local echo OFF so characters are not doubled.) Enter advances a line;
    // backspace erases the last glyph.
    // Echo via the CONSOLE path (serial + framebuffer) — keystrokes are part of
    // the interactive session, not the log stream, so they belong on the TV.
    // Suppressed while a foreground full-screen app owns the screen (it paints the
    // display itself; its raw key polls must not smear its frame).
    if CONSOLE_ECHO_ENABLED.load(Ordering::Acquire) {
        match b {
            b'\n' | b'\r' => { console_write_byte(b'\r'); console_write_byte(b'\n'); }
            0x08 | 0x7f   => { console_write_byte(0x08); console_write_byte(b' '); console_write_byte(0x08); }
            0x20..=0x7e   => console_write_byte(b),
            _             => {}
        }
    }
    // SAFETY: single-producer ring push in practice (see note above).
    unsafe { uart_rx_push(b) };
    let waiter = CONSOLE_READ_WAITER.load(Ordering::Acquire);
    if waiter != u32::MAX {
        crate::task::scheduler::wake_by_slot(waiter as usize, 0);
    }
}

/// Drain all available COM1 RX bytes into the ring buffer (called from IRQ).
///
/// # Safety
/// Must be called only from the IRQ handler with IF=0.
pub unsafe fn uart_rx_drain_fifo() {
    // SAFETY: port I/O to COM1; called from ISR with interrupts disabled.
    unsafe {
        while inb(COM1 + 5) & 0x01 != 0 {
            let b = inb(COM1);
            uart_rx_push(b);
        }
    }
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
