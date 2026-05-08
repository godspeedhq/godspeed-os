# Milestone 1 — Boot Visibility

> Kernel boots in QEMU and writes to the serial console. No services yet.

## Bootloader

- [ ] Choose and integrate bootloader (Limine recommended)
- [ ] Boot entry point wired to `kernel_main`
- [ ] `BootInfo` populated from bootloader (memory map, core count)

## Serial Output

- [ ] `arch/x86_64`: `serial_write_byte` writes to COM1
- [ ] `log.rs`: ring buffer + `kprintln!` macro working
- [ ] `kernel_main` prints `"kernel: all cores ready"` visible on serial

## Minimal Arch Init

- [ ] GDT: null segment, kernel code (ring 0), kernel data (ring 0)
- [ ] IDT: minimal entries — double fault, page fault (enough to not triple-fault)
- [ ] `arch::x86_64::init(boot_info)` completes without panic

## QEMU Integration

- [ ] `osdev run --smp 1` boots kernel and streams serial output
- [ ] No triple fault, no immediate reboot loop

## Acceptance

`osdev run --smp 1` produces serial output including:

```
kernel: all cores ready
```
