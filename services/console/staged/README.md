# Staged, not yet wired

Work in progress on taking `kernel/src/fbcon` out of ring 0 - the last standing Commandment I
violation. **Nothing in `services/console/` is built yet**: the crate is deliberately absent from the
workspace `Cargo.toml`, so this whole directory is inert and the tree builds exactly as it did before.

The design is settled and written up in **`docs/console-service.md` §9**. Read that first.

| File | What it is | Where it goes |
|------|-----------|---------------|
| `KERNEL_bootcon_mod.rs` | The kernel's boot/panic blit - plain ASCII, escapes discarded, no grid or cursor. What is LEFT in ring 0 after the terminal leaves. | `kernel/src/bootcon/mod.rs`, once wired |
| `../src/term.rs` | The terminal, moved from `kernel/src/fbcon/mod.rs` and adapted: lock removed (a service is one task), state owned by `service_main`, dirty-rectangle machinery deleted, grid bounds resized for a stack-resident grid | stays |
| `../src/render.rs` | Glyph and pixel rendering, moved from `kernel/src/fbcon/render.rs` | stays |

`KERNEL_bootcon_mod.rs` is parked here rather than at its destination for one reason: a module under
`kernel/src/` must claim one of the six responsibilities, and claiming one for code no service yet
drives would be a claim on dead code (§26.2 - features are pulled into existence). It moves, gains its
`kernel-log-floor` claim in `COMMANDMENTS.baseline.toml`, and gets its §11.4 amendment in the same
change that wires it - not before.

## What is left to do

See `docs/console-service.md` §9 and the "Still to do" list in the session memory.

The first thing to do is the memory type, and it is **Normal non-cacheable, not Device**. The neutral
`PageFlags::PCD` encodes Device on ARM, which is right for a peripheral's registers and wrong here:
Device semantics (no gathering, no reordering, no speculation) exist to protect stores that have side
effects, and a framebuffer store has none - it is just memory the display happens to scan. The cost of
using it anyway is real (non-gathering means every 32-bit pixel store is its own bus transaction, about
1.4M of them for a full-screen repaint on a Pi 2, and `edit` already crawled on that TV once). So the
ARM encoder needs a Normal-non-cacheable case, and `mmu::section_fb` needs to match it on the kernel
side. Measurement then confirms it on hardware; it does not choose it.
