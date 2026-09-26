# Bare-Metal Boot Freeze - Intel J5005 (Goldmont+, Wyse 5070)

## Status: RESOLVED - the Wyse 5070 boots bare metal and is a routine test machine

This freeze does not reproduce. On 2026-06-09 a hardened image booted the Wyse first try - four
cores, the AHCI flash, the Realtek NIC taking a DHCP lease and answering a ping, the shell
(`milestones/ALMANAC.md`) - and the board has been a standing hardware target since: selfcheck
377/0 twice and 100 rounds of chaos max-carnage with zero kernel panics and zero wedges
(`docs/probe-params-design.md`).

The C-state hypothesis below was never confirmed, and it is not what was blocking: MSR 0xE2 is still
locked by firmware on this part, the OS still cannot raise the C-state limit, and it boots anyway.
`milestones/ALMANAC.md` records the diagnosis this board's symptoms were traced to instead, and it
was in our own concurrency - core 0 wedging with interrupts disabled, so a task blocked on `recv` on
the BSP was never woken (`bugs/1_CROSS_CORE_IPC_REPLY_TO_BSP_STALLS.md`,
`bugs/1_FINDINGS_AP_TO_BSP_IPI.md`). Nothing in the boot path had to be told about this board; the
rest of this file is the record of the investigation as it stood.

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

## Why the MSR cannot be changed (still true, and still not a blocker)

- The lock bit prevents the OS from raising the C-state limit via RDMSR/WRMSR.
- Firmware does not expose the setting in BIOS/UEFI setup.
- Changing it would require custom BIOS/microcode or an ACPI override - out of scope.

The machine boots and runs the full suite with the MSR exactly as locked, so none of this needs
working around.

## Hardware data points

- TSC: 1497600000 Hz (J5005 base clock 1.5 GHz)
- RAM: ~5780 MiB free (8 GB installed)
- LAPIC IDs: 0, 2, 4, 6 (four Goldmont+ cores)
- MSR 0xE2 on all four cores: 0x0000000014008072 (locked, PC2 limit)
