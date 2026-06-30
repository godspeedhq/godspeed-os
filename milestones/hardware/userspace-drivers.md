# Milestone - Userspace Drivers (Framebuffer + USB) ✅

**Status:** ✅ Complete - hardware-proven on the HP T630 (AMD GX-420GI). USB keyboard + mouse on
**both** host controllers, runtime hot-plug, and a framebuffer console rendering to a real display.

**Target hardware:** HP T630 thin client - AMD GX-420GI (Jaguar/Puma+), 4-core, 8 GB RAM.
**Serial capture:** COM1, 115200 8N1 → `build/putty_serial_output.log`.

---

## Scope

v1/v2 ran headless over serial with no real input device. This milestone gives GodspeedOS its first
real **human I/O on bare metal**: a framebuffer text console on the attached display, and USB HID
(keyboard + mouse) driven by **userspace** driver services. The whole point of the capability model
shows here - the driver services touch hardware **only** through the SDK's audited `Mmio`/`Dma`
wrappers, so `xhci` and `ehci` themselves contain **no `unsafe`** (the sole `unsafe` token in each is
the word inside a doc comment: `services/xhci/src/main.rs:12`, `services/ehci/src/main.rs:16`).

---

## Achievements

### Framebuffer console (fbcon)
- ✅ Renders an 8×16 text console to a real TV/monitor over the **Limine framebuffer**, mirroring
  every serial line - `kernel/src/arch/x86_64/fb.rs`. Write-combining stores are flushed with an
  `SFENCE` so glyphs actually reach the panel.
- ✅ Cursor correctness on hardware: a true underline cursor that keeps the character under it visible
  (`f717581`, merge `97d9e48`) and no longer erases text when moving back over it on Left/Home
  (`ec17717`, merge `f70be68`).
- ✅ `clear` shell command; `help` is paged because the framebuffer console has no scrollback
  (`65c0e91`).

### USB stack (xHCI + EHCI)
- ✅ **USB keyboard works on bare metal** - merge `5d5daea` (`feat/usb-keyboard`, T630).
- ✅ **Both controllers** drive **both** HID classes (keyboard **and** mouse); multi-HID enumeration,
  honest connect/disconnect reporting.
- ✅ **Runtime hot-plug on both controllers** - merge `29eb4d3` (`feat/usb-hotplug`): plug/unplug a
  keyboard or mouse on either xHCI or EHCI at runtime and it is enumerated/torn down live. Auto-
  recovery from a wedged endpoint after rapid hot-plug, the "keyboard bomb" (`b8d6078`).
- ✅ Shared HID decode (`sdk/rust/src/hid.rs`) **dedups both drivers**: Caps Lock host-tracked latch
  (`852ba0c`), Ctrl+letter → control codes so `^S`/`^Q`/`^C` work from a USB keyboard (`477f983`),
  and Ctrl+Alt+Del = hardware reboot from any context (`5138df8`).
- ✅ Driver services are **`unsafe`-free** (§18.1): all register/DMA access goes through
  `sdk/rust/src/mmio.rs` (`Mmio::read32`…) and `sdk/rust/src/dma.rs` (`Dma::write32`…), the only
  audited hardware/ABI layer in the SDK.
- ✅ **Restartable** USB drivers - `xhci`/`ehci` survive a kill (`c32f7a1`, merge `d33c370`): the
  keyboard keeps working across `chaos max-carnage` kill/respawn cycles.

---

## Files / evidence

| Area | Path / commit |
|------|---------------|
| Framebuffer console | `kernel/src/arch/x86_64/fb.rs`; cursor fixes `97d9e48`, `f70be68` |
| xHCI driver | `services/xhci/src/main.rs` (no `unsafe`) |
| EHCI driver | `services/ehci/src/main.rs` (no `unsafe`) |
| Shared HID decode | `sdk/rust/src/hid.rs` |
| SDK hardware/ABI layer | `sdk/rust/src/mmio.rs`, `sdk/rust/src/dma.rs` (§18.1) |
| USB keyboard (bare metal) | merge `5d5daea` |
| USB hot-plug (both controllers) | merge `29eb4d3` |
| Restartable USB drivers | `c32f7a1`, merge `d33c370` |

---

## Hardware verification

All of the above is verified on the **HP T630** over null-modem serial + the attached display, not
just QEMU: USB keyboard and mouse enumerate and drive input on both controllers, hot-plug works live,
and the framebuffer console renders the shell. Both physical USB keyboards continue to work with the
**IOMMU enabled** (see the IOMMU/H1 milestone). The drivers also survive `chaos max-carnage`
kill/respawn on metal - the keyboard comes back after each restart.

---

## Follow-up / honest residue

- **Interrupt-driven USB explored, settled on busy-poll.** MSI/INTx interrupt infrastructure was
  built and merged (`49021aa`, `feat/usb-interrupt-driven`), but the drivers ultimately run
  **busy-poll** as the most reliable state on this hardware: the EHCI legacy INTx is effectively dead
  on the GX-420GI (`e68a1aa`, `fafcd0e` - "return to the flawless state"). xHCI busy-polls only while
  a key is held, idling otherwise (`efff4d1`). Cutting the poll cost via working device interrupts
  remains an open optimization, not a correctness gap.
