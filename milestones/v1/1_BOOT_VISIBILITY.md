# Milestone 1 — Boot Visibility

> Kernel boots in QEMU and writes to the serial console. No services yet.

**Status: COMPLETE** — commit `dc0aed9`, 2026-05-08

## Bootloader

- ✅ Choose and integrate bootloader (Limine v12.2.0, limine-rs 0.6.3)
- ✅ Boot entry point wired to `kernel_main`
- ✅ `BootInfo` populated from bootloader (memory map, core count)

## Serial Output

- ✅ `arch/x86_64`: `serial_write_byte` writes to COM1
- ✅ `log.rs`: ring buffer + `kprintln!` macro working
- ✅ `kernel_main` prints `"kernel: all cores ready"` visible on serial

## Minimal Arch Init

- ✅ GDT: null segment, kernel code (ring 0), kernel data (ring 0)
- ✅ IDT: minimal entries — catch-all halt handler in all 256 slots
- ✅ `arch::x86_64::init(boot_info)` completes without panic

## QEMU Integration

- ✅ `osdev run --smp 4` boots kernel and streams serial output
- ✅ No triple fault, no immediate reboot loop

## Acceptance

`osdev run --smp 4` produces:

```
memory: frame allocator ready
capability: subsystem ready
ipc: routing table ready
kernel: all cores ready
```

## Notes

Two non-obvious bugs required to reach this milestone:

1. **Linker script — explicit PHDRS required.** Without them, lld emitted `.got`
   and `.requests` as separate `PT_LOAD` segments sharing the same 4 KB page.
   Limine's ELF loader silently stopped mapping segments after the conflict,
   leaving `.rodata` and `.bss` unmapped.

2. **GDT must be in writable memory.** The x86 CPU writes the Accessed bit into
   a GDT descriptor when a segment register is loaded. `static GDT` in `.rodata`
   (mapped `r--` by Limine) caused a write-protection fault (`e=0003`) on the
   first `mov ds, ax`. Fixed with `#[link_section = ".data"]`.
