# Bare-Metal Boot Freeze - Intel J5005 (Goldmont+, Wyse 5070)

## Status: Backburner - blocked by firmware

## Symptom

Bare-metal build (`osdev image`) freezes after:
```
task: 'supervisor' spawned OK on core 0 (slot 1)
```
No exception output, no panic, no further serial output.
Full build with probe services also froze earlier at registry (slot 2) before
the MFENCE + deferred-timer-arm fixes.

## What was fixed

- MFENCE before WRMSR to IA32_TSC_DEADLINE (Intel SDM §10.5.4.2)
- Deferred timer arm to `scheduler::run()` after CR3 seeded (prevents cr3=0
  triple-fault on first timer ISR)
- C-state limit call moved into the TSC-Deadline path (was only in the periodic
  path, so never ran on real hardware)

These fixes moved the freeze point later (from slot 2 to slot 1 of supervisor)
but did not resolve it.

## Root cause hypothesis

MSR_PKG_CST_CONFIG_CONTROL (0xE2) is **locked by firmware** at boot:
```
cstate: core 0 MSR 0xE2 = 0x0000000014008072 (lock=1)
cstate: core 0 MSR 0xE2 locked - C-state limit cannot be set via MSR
```

Value 0x14008072: bits[2:0]=010 (PC2 limit), bit 15=1 (locked).

If Goldmont+ (Gemini Lake SoC) power-gates the APIC in PC2, TSC-Deadline
interrupts are dropped after supervisor is queued but before it gets its first
quantum. The system then spins silently in the idle loop with no runnable tasks
getting CPU time.

## Why we cannot fix it

- Lock bit prevents OS from raising the C-state limit via RDMSR/WRMSR.
- Firmware does not expose the setting in BIOS/UEFI setup.
- Would require custom BIOS/microcode or an ACPI override - out of scope.

## Workaround for testing

Use QEMU (`osdev run --smp 4`) for all development and test cycles.
The full build with probe services runs correctly in QEMU.

Hardware testing: wait for AMD GX-420GI (HP T630) which does not have the
Goldmont+ APIC power-gating quirk.

## Hardware data points

- TSC: 1497600000 Hz (J5005 base clock 1.5 GHz)
- RAM: ~5780 MiB free (8 GB installed)
- LAPIC IDs: 0, 2, 4, 6 (four Goldmont+ cores)
- MSR 0xE2 on all four cores: 0x0000000014008072 (locked, PC2 limit)
