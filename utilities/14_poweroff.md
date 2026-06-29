# Utility: `poweroff` - not provided (considered and rejected)

**Status:** Considered, implemented, tested on hardware, and **removed.**
**Why this doc exists:** so the next person doesn't re-attempt it expecting it to
be easy. It isn't, and the reason is instructive.

---

## The tempting symmetry that isn't real

`reboot` is one line - `outb(0x64, 0xFE)` pulses the CPU reset line via the
keyboard controller. So a `poweroff` feels like it should be just as small. It
is not, because **`reboot` never powers anything down.**

- **`reboot` is a *reset*.** The reset line snaps the CPU and chipset back to
  their power-on state; the CPU restarts at the firmware reset vector and POST →
  bootloader → OS runs again. Power is delivered the entire time. It's a warm
  reset, and it's trivially simple and universal precisely because the reset line
  is dumb, hardwired silicon that needs no firmware cooperation.
- **`poweroff` actually cuts power.** There is no equivalent hardware line for
  "turn off." The only standard mechanism is **ACPI sleep state S5**, which is a
  full power-management protocol, not a pin.

So the two are not symmetric operations. One is a wire; the other is a protocol.

## What S5 actually requires

Entering S5 is "write `SLP_TYPa | SLP_EN` to the `PM1a_CNT` I/O port" - but real
firmware gates that write behind ACPI machine language (**AML**) you must *run*:

1. The `SLP_TYPa` value lives in the DSDT's `\_S5` object (we can byte-scan it).
2. The machine must be in ACPI mode (`SCI_EN=1`) - an SMI handshake.
3. **Crucially, most firmware requires the `\_PTS` (Prepare To Sleep) control
   method to be executed first** - AML the firmware uses to do platform-specific
   prep (often via the embedded controller / chipset) that *gates* the sleep.

This is why **Linux uses ACPICA** - a complete AML bytecode interpreter (tens of
thousands of lines). It evaluates `\_S5`, runs `\_PTS`/`\_GTS`, and only then
writes the sleep register. "Write a port" is the last step of a much larger job.

## What we verified on the T630 (AMD GX-420GI)

A full minimal implementation was built and tested on real hardware:

- The FADT + DSDT `\_S5` parse was **correct**: `PM1a_CNT=0x804`, `SMI_CMD=0xb2`,
  `SLP_TYPa=5`.
- The machine booted in legacy mode (`SCI_EN=0`); the SMI enable handshake
  **worked** (`SCI_EN` went `0 → 1`).
- The S5 write executed correctly (read-modify-write, preserving `SCI_EN`).
- **The machine still did not power off** - confirming the firmware gates the
  power-down on `\_PTS`, which cannot run without an AML interpreter.

It worked under QEMU (which implements no `\_PTS` gate) and refused on the T630.

## Why it was removed rather than shipped

Two project principles, both decisive:

- **A command must do what it says (mechanical honesty, §26.5/§26.7).** A
  `poweroff` that prints "powering off…" and then leaves the machine running -
  working only under emulation - is a dishonest command. Better none than a
  half-one.
- **No ACPICA in a tiny, fully-understood kernel (§4.4, §26.11).** A full AML
  interpreter is a multi-tens-of-thousands-of-line subsystem that would blow the
  "30-minute whiteboard" budget for the sake of one shell verb. That is exactly
  the kind of hidden complexity §26 tells us to refuse.

## If it is ever revisited

It needs a real (if minimal) AML interpreter able to evaluate `\_S5` and execute
`\_PTS`/`\_GTS` - a deliberate, separate undertaking, not a shell built-in. Until
then, the **Power** category has exactly one honest member: `reboot`.
