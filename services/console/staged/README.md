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

See `docs/console-service.md` §9 and the "Still to do" list in the session memory. The open question
that stopped work is recorded in §9.5: which memory type the shared framebuffer mapping uses. The
neutral `PageFlags::PCD` is **Device** memory on ARM (non-gathering - every 32-bit pixel store is its
own bus transaction), which is correct but slow for bulk blits; Normal non-cacheable would gather.
Resolving that is a measurement, not a guess.
