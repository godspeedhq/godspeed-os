# Milestone 0 — Scaffold and Build Infrastructure

> All crates compile cleanly. No runtime behavior yet.

- ✅ Repository structure matches §5 (all directories created)
- ✅ `CLAUDE.md` files in every directory
- ✅ All Rust source files scaffolded with `todo!()` stubs
- ✅ Workspace `Cargo.toml` with all crate members
- ✅ `rust-toolchain.toml` — stable, `x86_64-unknown-none` target
- ✅ `.cargo/config.toml` — linker flags for bare-metal target
- ✅ `kernel/kernel.ld` — linker script, loads at 0x100000
- ✅ `contracts/schema/service.schema.json` — JSON Schema for service contracts
- ✅ `cargo check -p kernel --target x86_64-unknown-none` — clean
- ✅ `cargo check -p godspeed-sdk --target x86_64-unknown-none` — clean
- ✅ `cargo check -p {init,supervisor,registry,logger,block-driver,fs} --target x86_64-unknown-none` — all clean
- ✅ `cargo check -p {ping,pong} --target x86_64-unknown-none` — clean
- ✅ `cargo check -p osdev` — clean
- ✅ `.gitignore` — excludes `target/`, `.claude/`
- ✅ Pushed to GitHub
