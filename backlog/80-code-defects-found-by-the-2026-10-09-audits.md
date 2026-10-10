# 80. Code defects found by the 2026-10-09 documentation audits

**Status: FIXED IN CODE on `fix/backlog-80`, 2026-10-10 - every item below carries a FIXED note - and
OPEN until the hardware cards those notes name have passed: the T630 (D1, K17), the Pi 4 (K21, K22), the
Pi 2 (G4, K25, K28) and the VisionFive (D11). The audits changed documents and comments only; the items
were fixed separately, and the kernel ones with the operator's go-ahead.**

**Seen while closing it, not yet explained:** one `osdev test shell` run on 2026-10-10 went silent at the
refused-`tcp` step - no further serial output from the kernel or any service for twenty minutes, no panic,
no liveness report - and the rerun on the same code passed 253 of 253. A guest-wide silence is not a
service fault, since a stuck service cannot silence the kernel's own log. One sighting, no reproduction;
the capture is not committed. Recorded here so the next sighting has a first one to compare with.

Two audits ran on `feat/dogfood-gs` on 2026-10-09 (`audits/documentation-audit.md`, Audits 14 and 15).
Reading every comment against the code beside it finds code that does not do what its comment says, and
some of the time the comment is right. Those are below, with the evidence each reader gave and a proposed
fix. An item says VERIFIED where it was re-read in the code after the reader reported it; the rest are
the readers' findings, each with its evidence, and none has been reproduced by running it.

## Kernel (needs the operator)

- **K1. Every PCI driver is granted a fixed 64 KiB MMIO window, whatever its BAR's size.**
  `kernel/src/task/mod.rs` (`XHCI_MMIO_PAGES`, the PCI grant). On the T630 the audio grant then also
  covers the HDMI audio controller at `0xfeb64000`: authority beyond what the grant names (CLAUDE.md 3.1).
  Fix: size the window from the BAR.
  **FIXED on `feat/audio-finish`, 2026-10-09, x86 only; hardware-verified on the T630 the same day (every window its BAR: audio 16 KiB, xhci 8 KiB, EHCI 256 bytes, AHCI 1 KiB, the RTL8168 4 KiB; `selfcheck` 537/0 before and after a 100-round chaos).** It was worse than this
  says: on the T630 the audio BAR at `0xfeb60000` plus 64 KiB reaches the HDMI audio (`0xfeb64000`), `xhci`
  (`0xfeb68000`), EHCI (`0xfeb6c000`) and AHCI (`0xfeb6d000`). x86 `pci::bar_len` sizes a memory BAR at grant
  time (the standard probe, under the config lock) and the window is the BAR, its pages whole and the `Mmio`
  length exact; at most 16 MiB. The other six ports answer 0 and keep the fixed window, said at spawn - the
  Pi 4's and VisionFive's PCIe `xhci` among them, which is the part left open. QEMU: audio 16 KiB, AHCI 4 KiB,
  e1000 128 KiB (wider than before: its BAR is twice the old window).
- **K2. The T630's `audio-driver` is probably bound to the HDMI audio controller, not the analog codec.**
  `kernel/src/arch/x86_64/pci.rs` `find_by_class` returns the FIRST device of a class; the 2026-10-08 log
  shows 00:01.1 (HDMI) confined, not 00:09.2 (Azalia). Fix: finish step D3, so a supplied BDF selects the
  window, arena and vector, not only bus mastering.
  **FIXED on `feat/audio-finish`, 2026-10-09; hardware-verified on the T630 the same day (the driver granted 00:09.2 by the supplied BDF and confined there; heard through the speaker and the headphones).** One resolver, `HwClass::pci_dev`,
  decides the device for the window, the arena, the vector, the confinement and the bus mastering; a supplied
  BDF whose device is of another class is REFUSED (built, not yet seen firing). Which device: `hw-enumerator`
  op 3 takes a `PREFER_OWN` byte - the first device of the class that is not a display's companion function -
  and the supervisor sets it; `hardware` asks the same question to say which device a driver holds. Pinned by
  `osdev test audio`, which now boots a decoy HD Audio controller as function 1 of a display, first on the bus.
- **K3. `DevicePower`, `CpuClock` and `PciCfgRead` answer a caller WITHOUT the capability with 0.**
  VERIFIED. `kernel/src/syscall/dispatch.rs` (`handle_device_power`, `handle_cpu_clock`,
  `handle_pci_cfg_read`) return `CapError::CapNotHeld as i64` - the enum discriminant, 0 - not
  `cap_err_to_i64`'s -2. For `DevicePower` 0 is the documented SUCCESS; for `CpuClock` it reads as 0 Hz; for
  `PciCfgRead` as a config word of 0. Nothing is done and the kernel log says "refused", but the caller is
  told otherwise (invariant 12). Fix: `cap_err_to_i64(CapError::CapNotHeld)`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the three handlers return `cap_err_to_i64(CapNotHeld)`, -2.
- **K4. A received capability can be lost silently.** In `handle_recv`, `try_recv`, `recv_timeout` and
  `do_call`, a failed `current_task_insert_cap` (receiver's table full) drops the embedded cap, already
  removed from the sender; `push_pending_recv_cap` drops slots past 4 - the cap is installed but the
  service never learns its slot. Fix: refuse delivery, or report it.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a received cap that cannot be inserted or queued is dropped and LOGGED on all four receive paths, not left unclaimed.
- **K5. `recv`, `try_recv` and `recv_timeout` truncate a payload longer than the buffer silently**, where
  `Call` refuses (26.7). Fix: refuse, or report the truncation.
  **FIXED on `fix/backlog-80`, 2026-10-10:** an oversized message is refused, with a log line, on `recv`, `try_recv` and `recv_timeout`, as `Call` refuses it.
- **K6. riscv64: a kernel fault halts only the faulting hart** (`trap.rs`, `super::halt()`); the others run on
  until the liveness watchdog panics about 10 s later, against 6.2 (a panic halts every core). Fix:
  `halt_all_cores()` after `report_fault` on the kernel path.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the kernel-fault path halts every hart.
- **K7. x86 `int80_entry` is still installed at IDT[0x80] with DPL=3**, and runs the syscall chain on the
  top-of-kstack region the timer switch writes - the Bug 2 class the `ud2` path was moved off. Any service
  can issue `int 0x80`. Fix: remove the entry (the SDK uses `ud2`), or re-base it as `ud2_syscall_entry` is.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `int80_entry` and its IDT gate removed; the SDK uses `ud2`.
- **K8. x86 `ud2_syscall_entry`'s ring-0 path is `cli; hlt` on one core, silently**, so a kernel Rust trap
  is invisible until the watchdog. Fix: route to `exception_halt` / `halt_all_cores`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a ring-0 `ud2` is reported as a kernel exception and halts every core.
- **K9. x86 `exception_halt_handler` tests CS for 0x08/0x28**; a ring-3 frame carries 0x2B, so a user-mode
  frame reaching it prints no RIP. Fix: compare `w & !3 == 0x28`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** CS is compared without its privilege bits.
- **K10. x86 COM1 input ring has two unsynchronised producers** (`console_push_byte` from syscall context,
  `uart_rx_drain_fifo` from the core-0 timer), and its comment says so. Bytes can be lost or doubled where
  COM1 input is live. Fix: a lock, or a CAS on the tail.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the COM1 input ring takes a producer lock.
- **K11. x86 `iommu::confine_device` loses records silently**: a full 4-slot `CONFINED_TABLE` drops the
  record (I/O page table leaked, DTE never reverted); a failed `io_map_page` leaks partial tables;
  re-confining a BDF replaces its record without freeing the old table. Fix: log and refuse when full; free
  on failure.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a full table is refused up front, a partial or replaced I/O table is freed, and a confinement that lost the race for the last slot is undone (`unconfine`).
- **K12. x86 `ioapic::set_level_route` drops silently when its 16 slots are full.** Fix: log.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a duplicate route is recorded once, and a full table is logged.
- **K13. `kernel_main`'s attributes sit on `banner`**: `#[no_mangle]` and a clippy `allow` in
  `kernel/src/main.rs` apply to `banner`, because a doc comment and fn were inserted between them and
  `kernel_main`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the attributes are on `kernel_main`.
- **K14. `SpawnWithCaps` returns -2 for "spawned, no endpoint"**, the same value as `CapNotHeld`, so a
  failure reads as `Ok(None)` in the SDK. Syscalls 7, 19, 38 and 39 can now only ever name `supervisor`
  (the catalogue holds nothing else): surface Commandment I pins that does nothing.
  **FIXED on `fix/backlog-80`, 2026-10-10:** syscalls 7, 19, 38 and 39 retired with their SDK wrappers (`spawn_on_kernel`, `spawn_returning_endpoint`, `spawn_with_caps`, `spawn_pipe`, `spawn_pipe_on`); each number falls through to `UnknownSyscall` and fuzz F2 still passes. The supervisor's fallbacks after a missing image are a loud failure. `service_context.rs`'s unsafe floor 84 -> 80.
- **K15. `do_call` validates `buf_ptr` for 4096 bytes** even when `CallDeadline` declares a smaller reply
  buffer (reachability unverified). **`handle_resource_mint` leaks the id and cap when `write_user_bytes`
  fails.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** `Call` validates the buffer at the larger of the request and reply sizes; a failed `ResourceMint` write removes the cap and revokes the resource.
- **K16. `kernel/build.rs` looks for `.git/logs/HEAD` relative to `kernel/`**, so the commit-SHA rerun trigger
  is never registered; 25 of its 29 `SVC_*_ELF` lines are unused. Fix: `workspace.join(".git/logs/HEAD")`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the commit-SHA trigger is registered from the workspace root, and only the images read are listed.
- **K17. x86 `route_ehci_intx` always targets the BSP while `EHCI_CORE` is 3**, so each wake crosses cores
  (unverified on hardware). **Possible:** a line masked at a driver's death is not unmasked by `register`;
  whether a respawned level-triggered `ehci` unmasks before its first interrupt is unverified.
  **FIXED on `fix/backlog-80`, 2026-10-10:** every spawn of `ehci` re-aims its INTx candidates' IOAPIC entries at the spawned core (`ioapic::set_dest`, the destination only). **T630 card 2026-10-10:** the back-port keyboard types, and `ehci: legacy INTx candidates re-aimed at core 3` is logged at boot and on each of eight respawns (`chaos kill-storm ehci 4`, twice, 8 of 8 recovered); `selfcheck` 534 passed, 0 failed. The interrupt itself is still not seen on this board - `ehci` falls back to polling, as `backlog/11` already records - so the card shows the re-aim does no harm, not that INTx now arrives.
- **K18. InspectKernel's ungated list still names deleted queries 9 and 22**, and INTROSPECT refusals return
  data-like zeros. Harmless today, wrong in an authority list.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the ungated list names only queries that exist; the seven gated figures (frames, a core's ticks, a task's endpoints, the endpoint count) are `Option` in the SDK, and the shell and `observe` say "unknown" or refuse instead of drawing zeroes.
- **K19. Strings broken by line joins**: the AcquireSendCap "table FULL" log, a log near `dispatch.rs` ~1580,
  riscv64 `context_switch.rs` panic messages, raw line breaks in riscv64 `trap.rs` and `usb.rs` literals.
  **FIXED on `fix/backlog-80`, 2026-10-10:** 80 strings with raw line breaks rewritten with `\n`, byte-identical output.
- **K20. Dead kernel code**: `SPAWN_FLAG_HAS_RECV/SMALL_MEM/IS_PROBE`, the `Task` and `Endpoint` structs,
  `smp::placement`, `memory::ownership`, `revoke::revoke`, `route::unregister`, both TLB broadcasts,
  `assert_cap_validated(&Ok(()))` (cannot fire), `assert_tcb_alive` (a no-op), `program_xhci_msi`,
  `uart_rx_enable`, four unused `pci.rs` constants, an empty `if progif == PROGIF_XHCI {}`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** deleted: the three unused spawn flags, the `Task` and `Endpoint` structs, `smp::placement`, `memory::ownership`, `revoke::revoke`, `route::unregister`, the two assertions that could not fire, `program_xhci_msi`, `uart_rx_enable`, the four `pci.rs` constants and the empty `if`. **And the two TLB broadcasts, after the per-port proof the operator asked for** (CLAUDE.md 10.5 amendment): reclamation waits until no core runs or is still leaving the task; an idle core then holds the boot or kernel root (x86, ARMv7, RISC-V), whose switch invalidated locally (CR3 write, `TLBIALL`, `sfence.vma`), or, on AArch64, keeps the dead root and is covered by `free_page_table_root`'s `tlbi vmalle1is`; no ASID or PCID anywhere, no live mapping changed under a running task. Two speculation-only residuals are recorded there, not checked. P7 now tests the property (a fresh task at a killed task's addresses reads only zeroes) and fails when broken, with its limit stated.
- **K21. Pi 4: the framebuffer is mapped with two memory types at once.** `console` gets it as Normal
  non-cacheable (MAIR slot 2) while the kernel's direct map still covers the same pages as Normal
  write-back cacheable - mismatched attributes ARM leaves UNPREDICTABLE. Fix: map the range non-cacheable
  in the direct map (split the 2 MiB block), or withhold the grant.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the framebuffer's pages are split out of their 2 MiB blocks in the direct map and remapped non-cacheable, break-before-make; the split-table pool 12 -> 32. Needs a Pi 4 card.
- **K22. GICv2 EOI drops the SGI source CPU**: `gic::acknowledge` returns `IAR & 0x3FF` and `eoi` writes only
  the ID, but an SGI's EOIR value must carry IAR's CPUID bits [12:10], so an SGI from cores 1-3 is EOI'd with
  a mismatched value (effect on hardware unverified). Fix: EOI with the raw IAR value.
  **FIXED on `fix/backlog-80`, 2026-10-10:** EOI is written with the raw IAR value. Needs a Pi 4 chaos run.
- **K23. aarch64 `cpu_clock` returns a rate it did not read back** (`now.unwrap_or(set)`); CLAUDE.md 12.3
  says the result is the read-back. Fix: `None`, or log, when the read-back fails.
  **FIXED on `fix/backlog-80`, 2026-10-10:** returns the read-back rate, or `None` with a log line.
- **K24. riscv32 `_start` clears BSS with RV64-only `sd` and an 8-byte stride** (could not build to confirm).
  Fix: `sw` and 4.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `sw` and a 4-byte stride, confirmed by assembling the loop for rv32imac (no board runs riscv32).
- **K25. arm32 strings and dead branches**: the kernel-fault log says "the arm32 port cannot yet kill a task"
  (it can); `wait_for_interrupt` has an empty `if !irq::usb_owned_by_userspace() { }` doing a route lookup on
  every idle pass; `route_usb_irq_to_core0` has no callers; `start_tick_ap` has an unreachable branch;
  `mmu::enable` prints "caches ON" after a selftest the caches were already on for; the framebuffer section is
  non-shareable while the console's small-page mapping is shareable (effect unverified).
  **FIXED on `fix/backlog-80`, 2026-10-10:** the fault line corrected and every core halted; the empty `if`, the uncalled `route_usb_irq_to_core0` and the unreachable branch removed; "caches ON" before the selftest; the console's framebuffer pages non-shareable to match the section. Needs a Pi 2 card.
- **K27. USB driver placement is stated twice with nothing linking the copies**: x86 MSI destinations use
  `task::XHCI_CORE`/`EHCI_CORE` (2 and 3), and the supervisor's `USB_IMAGES` rows hard-code 2 and 3. Change
  one alone and the interrupts land on the wrong core. Fix: one constant and a check, or derive the
  destination from the spawned task's core.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the EHCI MSI is aimed at the core the driver was spawned on, at every spawn; `EHCI_CORE` deleted.
- **K28. arm32 `hw_random` is an unlocked FIFO pop**: two cores can both pass the "word available" check and
  read the same or a stale word - and it now feeds the WPA2 SNonce (`sdk/wifi` `supplicant.rs`). Fix: a
  spinlock around the check and the read.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a lock around the enable, the check and the read. Needs a Pi 2 card (`random`, and a WPA2 join over the dongle).
- **K26. The stubs' `unimplemented!("aarch64::switch_to_boot_stack")` names the wrong arch** (riscv32,
  loongarch64, s390x). **aarch64 `mailbox::notify_xhci_reset` takes no `MBOX_LOCK`** and runs after the
  secondaries start - safe only because nothing calls it from a syscall yet.
  **FIXED on `fix/backlog-80`, 2026-10-10:** each stub names its own arch; `notify_xhci_reset` takes `MBOX_LOCK`.

## Standard library (`gs`)

- **G1. `gs::fs::Fs::write` cannot write more than one chunk (3556 bytes).** It creates the file with the
  first chunk (`OP_WRITE_FILE`), which `fs` sizes for that chunk alone, so the first `OP_WRITE_AT` past it
  is refused "write past extent": `Error::Failed`, and a truncated file left behind. Its own comment now
  says so. Fix: `OP_WRITE_NEW(total)`, then `OP_WRITE_AT` from 0.
  **FIXED on `fix/backlog-80`, 2026-10-10**, as proposed (`create_sized`, then `write_at` from 0). Nothing in the
  tree wrote past one chunk, which is how it stayed unseen; `examples/stdlib-hello` now writes 8000 bytes and reads
  them back, and `osdev test examples` checks every byte - shown to FAIL against the old code (10 passed, 2
  failed) and pass against the new (12 of 12).
- **G2. `gs::file::File` records the REQUESTED rights, not the granted ones**, so a sealed file narrowed to
  READ, or an APPEND-only open, closes under rights the cap lacks: the kernel refuses, `fs` never frees the
  slot (64 entries), and `Drop` swallows the error. Fix: record the granted rights; close under them.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `open` records the rights the kernel says the cap CARRIES (QueryCapRights), and `close` invokes under one it holds. The fcap self-check gains the append-only case; `osdev test file-cap` 16 of 16, shown to fail against the old code (14 passed, 2 failed).
- **G3. `gs::net::Conn::recv` truncates silently** (`min(data.len(), buf.len())`; `net-stack` returns up to
  2048 bytes already consumed). Fix: refuse with `BufferTooSmall`, or send the length.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the receive request carries the caller's buffer size and `net-stack` reads no more than that off the connection (an empty length still means all of it), so nothing is consumed that cannot be delivered. `osdev test shell`'s `serve` round trip, through a 512-byte buffer, passes.
- **G4. `gs::task::sleep_quantum` returns after about 1 us on a real Pi 2**: the counter is 1 MHz, so
  `sleep(1)` takes the sub-tick path, and loops meant to yield a quantum spin. Fix: sleep one 10 ms
  duration.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `sleep_quantum` sleeps 10 ms as a duration (one tick on an uncalibrated clock, as before). Needs a Pi 2 to see.

## SDK

- **S1. `spawn_via_supervisor` retries after ANY error, `ReplyDead` included**, so a supervisor that dies
  mid-spawn could start the service twice. Fix: retry only a send that never left.
  **FIXED on `fix/backlog-80`, 2026-10-10:** retried only when the send never left; `ReplyDead` and a full queue are returned.
- **S2. The SDK traces a full queue as a lost peer** (`request_with_reply_call_err` records every `Err` as
  `KIND_PEER_LOST`), so `gs::trace` shows `Busy` from `gs::call` as `PeerLost`. Fix: its own kind.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a full queue is traced `KIND_QUEUE_FULL`.
- **S3. `DeadlineOutcomeInto::SendFailed` includes `ReplyDead`**, though the request was delivered (safe for
  `fs`'s block transfers, wrong for a caller that is not idempotent).
  **FIXED on `fix/backlog-80`, 2026-10-10:** `DeadlineOutcomeInto::PeerDied` for `ReplyDead`; `fs`'s block transfer retries it, being idempotent.
- **S4. `restart` does `let _ = self.kill(name)`, and `kill` maps every error, `CapNotHeld` included, to
  `InvalidArgument`** - the silent drop CLAUDE.md 13.6's amendment describes, still there; the shell uses it.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `kill` says which failure (refused, or no such service); `restart` passes over only "not running" and returns a refusal instead of spawning; `control` logs a refusal as one.
- **S5. Silent drops and truncations**: `print` drops anything over 256 bytes (`gs::io` now chunks around
  it); `log_fmt` and `console_*_fmt` cut at 256 with no marker and log `(fmt error)` on a mid-character cut;
  `try_recv`/`recv_timeout` turn every error into `None`, so a deadline loop spins on a dead endpoint; drain
  loops discard messages without reclaiming their caps; `set_installs` allows 32-byte labels against the
  kernel's 24; `sleep` is not clamped on 32-bit; `max_secs <= 0` maps to a `CallDeadline` of 0, which
  blocks forever.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `print`/`console_write` write in pieces on character boundaries; the formatted writers cut on a boundary and end `...`; `try_recv`/`recv_timeout` die loudly on their own endpoint's error, as `recv` does; the mailbox drains give back a discarded message's cap; `set_installs` takes the kernel's 24; `sleep` clamps to one register on 32-bit; a deadline of <= 0 s is one second, never "none".
- **S6. `mmio.rs` `read16/32/64` and `write16/32/64` never check alignment**, so a safe call with an
  unaligned offset is a misaligned volatile access - undefined behaviour, and an alignment fault on Device
  memory. A possible soundness hole in the 18.1 layer. Fix: assert alignment, or compose as `Dma` does.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the 16/32/64-bit accessors assert the ADDRESS is aligned to the access size.
- **S7. `record::add_row` drops extra values without setting `overflow`.** **`drain_kernel_ring_buffer` is a
  dead stub** whose doc says `events` calls it.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `add_row` sets `overflow` for extra values; `drain_kernel_ring_buffer` and `recv_log_message` deleted.
- **S8. `sdk/wifi`**: `sdio.rs` `read_extended`/`write_extended` truncate the CMD53 count (`& 0x1FF`)
  silently; `prf_sha1` zero-fills on oversize input; `pbkdf2_sha1` cuts a salt over 60 bytes; `rxq::pop`
  returns 0 forever if the head frame does not fit; `serve.rs` and `keyfile.rs` treat names over 16 bytes
  differently; an `sdio.rs` log says "network adapter"; a Pi 4 hint prints on every board.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a CMD53 count past the 9-bit field is refused; `prf_sha1`/`pbkdf2_sha1` return `false` instead of a key of zeroes or a cut salt; `rxq::pop` drops and counts (`too_big`) a frame that does not fit; a radio name over 16 bytes chooses no radio; the FUNCID line and the no-card hint corrected.

## Services

- **V1. `observe live` cannot start**: the shell calls `ctx.spawn_on("observe-live")`, and the kernel
  catalogue holds only `supervisor`, so it prints "failed to spawn" (from the code; not run). Fix: ask the
  supervisor.
  **FIXED on `fix/backlog-80`, 2026-10-10:** it did start - `spawn_on` routes through the supervisor - but nothing had ever run it. `osdev test shell` now opens the live view, sees a frame and quits with `q`.
- **V2. `fs` answers an absent path from a mutating op with `FS_ERR` and a reason, and `OP_OPEN` with a bare
  `FS_ERR`** (the reason dropped); a data block that fails its CRC answers `FS_NOTFOUND`, so a damaged file
  reads as missing. Fix: `FS_NOTFOUND` for walk misses, a distinct status for CRC failures.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a walk miss answers `FS_NOTFOUND` (with its reason) on every operation; a failed `OP_OPEN` carries its reason; a file whose block fails its CRC or cannot be read answers the new `FS_DAMAGED` (7), `gs::Error::Damaged`.
- **V3. `fs` `op_is_mutating` omits `OP_SEAL`**, so `seal` can write the superblock on a read-only mount.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `OP_SEAL` is in `op_is_mutating`.
- **V4. `fs` `delete_tree` reports failure after the unlink committed** when freeing fails. Fix: success,
  with a leak warning. **An `fs` log string (the APPEND refusal) has a run of spaces.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** a reclaim failure after the unlink committed is logged with the repair (`drives check`) and the delete reports success; the APPEND string rejoined.
- **V5. The supervisor's misplaced `#[cfg]` attributes** gate `time` instead of `block-driver`, and stack a
  second onto `ensure_wired("wifi-driver")` - already open as backlog/77; the audit confirms it, unchanged
  since `c6473b55`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** both misplaced attributes removed rather than moved: every build has started `block-driver` and `nic-driver` all along, so `time` now starts in every build and nothing else changes (`backlog/77`).
- **V6. `let _ = ctx.spawn("observe")` in the supervisor probably fails silently**: it takes the kernel's
  by-name path, whose catalogue holds only `supervisor` - the trap the supervisor's own comments warn about at
  the `events` and `ping` spawns (runtime unverified). Fix: `spawn_mapped`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `spawn_mapped`, with a line if it fails.
- **V7. `fs`'s mount retry after `E_IO` is bounded by a count (1000 x `yield_now`), not a duration** - the
  count-is-not-a-duration bug the capacity loop above it already fixed. Fix: the same `sleep_ms` + clock.
  **FIXED on `fix/backlog-80`, 2026-10-10:** bounded by the same 20 s clock as the capacity wait, sleeping between attempts.
- **V8. The supervisor adopts `events` and `console` without the `name_alive` check `ensure_mapped` uses**
  (`converge` mitigates). **`NAME_MAP_MAX` = 16 has no headroom** - exactly full on the Pi 4 and on x86 with
  the dongle attached.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `events` and `console` are adopted through `ensure_mapped`, with its `name_alive` gate; `NAME_MAP_MAX` 16 -> 24.
- **V9. Contracts declare endpoints their spawn rows never request**: `mem-pressure` and `observe` declare
  `ipc_receive` with no `SPAWN_FLAG_REQ_RECV`, which `contract_check` does not compare. **Dead code**:
  `usbdisk.rs`'s BUSY/ABSENT arms (so "no disk" now logs per block), chaos `PACE_YIELDS`, recorder `FS_TAG`,
  control `Q_COM2_BYTE`; `ahci.rs` `cache_commit` discards `write_block` errors under its test feature.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the two contracts no longer claim an endpoint; `usbdisk`'s unreachable busy loop is the single call it was; `PACE_YIELDS`, `FS_TAG` and `Q_COM2_BYTE` deleted; the test cache counts a failed block and answers the barrier ERR.

## Drivers and the network stack

- **D1. `ehci` puts the QH hub port at bit 22**, in `control` and `poll_devices` (`<< 22`); the EHCI spec and
  Linux's QH_HUBPORT mask (0x3f800000) put Port Number at [29:23] and Hub Address at [22:16], so port 1 sets the
  hub address's top bit and reads as port 0. Its keyboard is recorded as hardware-verified, so which port
  that run used matters. Fix: `<< 23`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** port number in bits 29:23, as Linux writes it. **T630 card 2026-10-10, passed:** a keyboard on hub port 4 (even) enumerates and types across eight respawns, and moved to hub port 3 (odd) it enumerates again (`ehci: SPLIT device (hub port 3)`) and types `version`.
- **D2. `nic-driver` drives any class-0x020000 NIC that is not an RTL8168 as an e1000**, with no vendor or
  device check. Fix: check the e1000 IDs; refuse with a status otherwise.
  **FIXED on `fix/backlog-80`, 2026-10-10:** drives only the 82540EM (`8086:100e`) and names any other NIC; no new arch-conditional site.
- **D3. `net-stack` op 8 (renew) does not set `leased`** as the other dances do, so a stale lease flag drives
  re-DHCP and retry wrongly.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `renew` copies the lease flag.
- **D4. `net-stack` has raw requests that bypass its displaced-request sifting** - the ARP reply and receive
  poll in the DNS loop, and `ping`'s ARP - so they can consume a client's request. Fix: `nic_req`/`nic_req_ms`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the ARP replies and the DNS loop's receive poll go through the helpers that set a client's request aside.
- **D5. `net-stack` op 21's failure reply is empty**, which `Reply::send` `debug_assert!`s against (a debug
  build panics), and an empty message is refused on three ports (backlog/66). Fix: a status byte.
  **FIXED on `fix/backlog-80`, 2026-10-10:** op 21's reply is `[status, bytes...]` - 0 and the data, or 1 alone - and `gs::net::Net::tcp` reads it; no empty message is sent.
- **D6. `dwc2`'s fallback loop (no MMIO/DMA grant) takes requests without answering or reclaiming their reply
  caps**, so clients hang to their deadline. Fix: answer through `dispatch`/`answer_no_disk`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the no-grant loop answers every request, routed by type as the normal path does (no board takes that path today).
- **D7. `xhci` misses a root-port unplug with two HIDs bound**: root-port detection is gated on
  `ndev < MAX_HID`, and the same gate covers the WiFi dongle's empty-port check and the poison clearing.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the root-port scan runs with the HID table full; only the new-device branch waits for a free slot.
- **D8. `xhci`'s disk-hub scan re-enumerates on ONE connected read**, where the HID scan requires two
  (`hub_seen`) for the phantom-arrival reason its own comment gives.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a disk-hub arrival needs two consecutive connected readings, as the HID scan does.
- **D9. Invariant 9 statics**: `ehci` `TIMED_OUT_ONCE`; `xhci` `PROBE_FAILS`, `DIAG_HINT_SAID`, and in `msc.rs`
  `NO_DISK_LOGGED`, `READ_FAIL_LOGGED`. Fix: loop-owned state, as `xhci`'s own comments recommend.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the latches are loop-owned (`ehci` function-local; `xhci` loop locals and a disk-record field).
- **D10. Strings broken by lost line continuations** (about 25 spaces mid-line): five in `net-stack`, one in
  `tcp.rs`, two in `dwc2`, one in `dwmac_ring.rs`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the nine strings rejoined, and one more in `wifi-driver`.
- **D11. Smaller**: `dwc2` `net::serve` discards the reply result; `net-stack` `feed_tx` is effectively dead;
  unused `stale_after`/`last_ok` (dwc2 hid), `POLL_FRAMES`/`active()` (tcp), `tx_fault_counters` (dwmac);
  `wifi-usb`'s ST_FAILED message says "has no bulk IN" for a failed repair too.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `dwc2` logs an undelivered NIC reply; the dead code removed; `wifi-usb`'s ST_FAILED names both causes; the dwmac fault counters are logged after eight transmits (a VisionFive boot to read).

## Shell

- **H1. `fg` prints `copying... N%` for every job kind**; a delete-tree, check or scrub reads `copying...
  100%` while it runs. Fix: as `jobs` does.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `foreground` shows a percentage only for copy and churn; other kinds say they have no measurable progress.
- **H2. `chaos kill-storm`'s refusal says "only supervisor/block-driver/fs recover on death"**;
  `CHAOS_RESTARTABLE` has 11 entries. Fix: build the sentence from the array.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the refusal lists `CHAOS_RESTARTABLE` itself.
- **H3. chaos target lists drift** between the help, two `max-carnage` hints and `kill-storm` (which refuses
  `shell`; `max-carnage` takes any live service). Fix: derive them from `CHAOS_RESTARTABLE`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the help and the hints derive from the array, and the refusals return errors.
- **H4. chaos link-flap says "The in-kernel ARM NICs have no link override"**; they are `nic-driver`
  backends. Fix: "this NIC backend has no link override".
  **FIXED on `fix/backlog-80`, 2026-10-10:** the wording names a NIC backend, and the no-NIC and no-override exits return errors.
- **H5. `drives` shows a zero-capacity drive as a `raw 0 MiB` row.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** a zero-capacity drive is an empty table.
- **H6. The shell's `op_is_mutating` omits `OP_WRITE_AT_J`**, which `fs`'s has (latent; nothing compares the
  lists).
  **FIXED on `fix/backlog-80`, 2026-10-10:** `OP_WRITE_AT_J` and `OP_SEAL` added.
- **H7. `#[inline(never)]` sits on `let_capture_form`, not `run_lines`** - on `main` too. The attribute was
  written for `run_lines`, which holds the verdict array on a tight stack. VERIFIED. Fix: move it.
  **FIXED on `fix/backlog-80`, 2026-10-10:** moved onto `run_lines`.
- **H8. `fmt` is not in `UTILS`**: `fmt help` treats "help" as a path and its help block is unreachable;
  `whatis` answers "unknown" for `fmt`, `tcp`, `serve`, `random`, `gpio`, `spawncap`, `spawnwired`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `fmt`, `tcp`, `serve`, `random`, `gpio`, `spawncap` and `spawnwired` in `UTILS`, with help blocks; `utilities/59_spawncap.md` added.
- **H9. `PRODUCER_COLS` is wrong for `roster`** (emits name/role/seat) **and `uptime`** (uptime/seconds), so
  Tab offers columns that do not exist.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the completion columns match what each producer builds.
- **H10. Failures reported as success**: `ping` usage errors and a bad address; `net_stats_dump` on a timeout
  or abort; all three `date sync` failure paths; the comma-list forms of `fmt`, `mkdir` and `restart` (which
  also drop items past 16 silently).
  **FIXED on `fix/backlog-80`, 2026-10-10:** each failure returns an error; one `run_list` helper serves the comma lists, refuses one over 16 before anything runs, and fails the list if any item fails.
- **H11. `stream_minify` treats an `fs_read_at` error as end of file**, silently truncating a script; the
  import path ignores the truncation flag.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a read failure is not end of file: `run` refuses an unreadable or too-large script, `import` refuses a partial one, and `fmt` no longer replaces a file with a truncated copy.
- **H12. The shell is a second writer of `/clock.last`** though `time` owns it, and two strings say "the time
  service refused" when `time` never refuses.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the shell's write of `/clock.last` removed, and the wording corrected.
- **H13. Wrong sizes and names in strings**: "(16 KiB cap)" (12 KiB), "(4 KiB)" (512 B), an assert naming
  `complete_command` (`complete_tab`), `wifi debug firmware`/`transport` hard-coding CYW43455 and SDIO, the
  down-reason-4 "not written yet".
  **FIXED on `fix/backlog-80`, 2026-10-10:** the sizes, the `complete_tab` name, the reason-4 sentence and the radio-neutral `wifi debug` views corrected.
- **H14. Tab completion**: `ping` drops `version` and `help`; `RESTART_TARGETS` offers `supervisor` and
  `shell`, which `restart` refuses. The `pwd` hint sends the user to `cd`, which goes to `/`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `ping` completes `version` and `help`; `restart` offers neither `supervisor` nor `shell`; the hint is `cd .`.

- **H15. `drives flash typo` erases drive 0** (after the y/N): `drive_sel_ok` accepts any non-numeric word as
  a label without comparing it to anything. The most dangerous item in this section. Fix: compare against
  the superblock label, and refuse a word that does not match.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a word selector must be drive 0's own label, read from `fs`, or it is refused before the `[y/N]`. `osdev test drives` 11 of 11, and 9 passed, 2 failed against the old shell.
- **H16. Utility convention 10 (`q` quits a wait) regressed across the fs and net commands**: `fs_stat_r`,
  `fs_read_at`, `filter_read`, the `find`/`tree` walks, `build_dir_table` and `sock`'s `gs::net::Net::new`
  build handles with no notice, so `cd`, `read`, `find`, `tree`, `match`, `count`, `sort`, `first`, `last`,
  `copy`, `edit`'s open and `sock` cannot be quit (`sock` can wait about 40 s); their `Cancelled` arms are
  dead. Fix: `.noticing(&notice)` / `Net::with_notice`, as `dir` and `tcp` do.
  **FIXED on `fix/backlog-80`, 2026-10-10:** every wait the item names shows `[q] quit` again, in scripts and in the producers behind a pipe too; a key typed during a wait is kept for the next reader (the shell's 256-key type-ahead) rather than eaten, and the SDK reads no key before the notice is shown, so a `q` typed ahead for a pager does not end a wait it was not meant for.
- **H17. `ping` caps the round trip at 65 ms silently**: `net-stack` stores it as `u16` microseconds
  (`.min(65535)`). Fix: widen it, or print "65 ms or more".
  **FIXED on `fix/backlog-80`, 2026-10-10:** a saturated round trip prints `time>=65ms`, and the summary says the field saturates.
- **H18. Recursive `copy` reports success when it was not**: `cmd_copy_tree` returns `Ok` after skipped files,
  an INCOMPLETE listing or a truncated walk; a subdirectory `mkdir_at` failure and a `join_path`/`remap`
  overflow are skipped silently; `copy_file_streaming` reports a short read as a complete copy.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a recursive copy counts and names what it skipped and returns an error; a short source is a short copy.
- **H19. `find` reports any listing error** (a bad start path, a storage error) as "INCOMPLETE ... too large",
  then `0 match(es)` and `Ok`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** a missing start path is refused as such, an unreadable directory is named, and an incomplete search is an error.
- **H20. More silent fallbacks**: `date epoch` prints `0` when the clock is unset; `max-carnage` usage and
  unknown-target errors and `link-flap` with no NIC return `Ok`; comma-list `spawn`/`kill`/`delete` (as well
  as the H10 set) always return `Ok`; `inspect_core_count` returns 1 on error; `min`/`max` of zero rows is 0;
  `cores <junk>` and `events log <junk>` use defaults silently; `cores ticks | ...` drops `ticks`.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `date epoch` on an unset clock refuses; `max-carnage`'s usage and target refusals and `link-flap` with no NIC or no override are errors; the comma lists fail if any item fails (H10); `inspect_core_count` answers 0 and every view says "unknown"; `min`/`max`/`avg` of no rows refuse; `cores junk` and `events log junk` refuse; `cores ticks | ...` keeps `ticks`.
- **H21. Convention gaps**: `ping` accepts hidden synonyms `size` and `n` (rule 3); `fcap reuse`/`gsreuse` are
  in neither help nor completion; `cores ticks` is not in `SUBCMD_FIRST`; `wait` busy-yields instead of
  sleeping; `cores ticks` has no `q` for its 5 s; `observe` live quits on `q` only, not Esc; `observe now`
  waits on a 1,000,000-yield count; the observe painter title and the shell's notice differ though a comment
  says they match byte for byte; `read` on a directory says "not found"; `parse_duration` checks the year
  bound before applying the unit.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `ping`'s synonyms removed; `fcap reuse`/`gsreuse` in help and completion; `cores ticks` completes and quits on `q`; `wait` sleeps; `observe` live takes Esc; `observe now` waits 5 s by the clock; the titles match; `read` on a directory says so; `parse_duration` bounds after the unit.

## Examples

- **E1. `stdlib-hello` never reaches `fs`, and its test passes anyway**: its spawn row has flags 0 (no
  `REQ_RECV`), so every `gs::fs` call is `Unreachable`; `build/tests/examples_serial.log` shows "could not be
  reached" while `run_examples` accepts "stdlib-hello: done". Fix: set `REQ_RECV`; assert on the file.
  **FIXED on `fix/backlog-80`, 2026-10-10**: `REQ_RECV` in its spawn row, the test's disk formatted host-side so
  `fs` mounts, and the test asserts the write and the byte-for-byte read-back rather than `done` (G1).
- **E2. `examples/counter` leaks a cap slot per second while saves fail** (`gs::cap::acquire(..).is_ok()`).
  **FIXED on `fix/backlog-80`, 2026-10-10:** the cap acquired to probe `fs` is removed each time. `osdev test counter` 5 of 5.
- **E3. `resource-server`'s log cites a by-name grant that does not exist.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** the log says the privilege arrives in the supervisor's spawn row.

## Tooling and gates

- **T1. The Python 3.8 floor is false and its checker cannot see why**: three checkers use `list[...]` /
  `dict[...]` in signatures with no `from __future__ import annotations` (a TypeError on 3.8, which
  `build.yml` pins), and `python_floor_check` matches no `def` line (reasoned from PEP 585; not run on 3.8).
  **FIXED on `fix/backlog-80`, 2026-10-10:** `from __future__ import annotations` where needed, a TOML fallback for `tomllib` (3.11+), and `python_floor_check` reads signatures and unguarded new-module imports.
- **T2. `port_scope_check.py` fails on every branch**: `docs/porting.md` marks 12 rows `+`, `SEAM_ABOVE`
  allows 11 - hidden because only the paused `build.yml` runs it.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `wifi-driver/build.rs` allowed; the check is on the build path (GS0204), and `conform --selftest`'s line-number cases fixed (24 of 24).
- **T3. `osdev test <typo>` exits 0.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** an unknown suite exits 2 and says nothing ran.
- **T4. `commandments.py` reports 10/10 mechanised; it is 9**, and prints stale counts.
  **FIXED on `fix/backlog-80`, 2026-10-10:** reports 9 of 10, and reads its counts from the baseline.
- **T5. `stack_fit_check` passes while measuring nothing** when objdump fails.
  **FIXED on `fix/backlog-80`, 2026-10-10:** stops the build when objdump is missing, fails, or measures no function.
- **T6. `unsafe_check` never compares counts for the four `SDK_PERMITTED` files.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** counts the permitted SDK files by full path against the audit.
- **T7. `service_embed_check` reads vestigial kernel lists on arm/aarch64 and checks no x86_64 list.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** compares MANAGED with the supervisor's roster on every arch, x86_64 included, and is on the build path.
- **T8. Gates that claim more than they check**: `doc_refs` skips CLAUDE.md, COMMANDMENTS.md, `examples/` and
  `tests/`; `facts_check` drops a fact whose source is missing; `doc_symbols_check` resolves by substring and
  passes after writing a missing baseline; board builds run 7 checkers, not the 24 in `EXTRA_CHECKS`;
  `conform-ok` markers for other rules are ignored; `commandments_redteam` always exits 0;
  `foreign_word_check` does not scan COMMANDMENTS.md (III says `fsck`); `contract_check` does not reconcile
  `examples/`, whose contracts drift from their spawn rows.
  **FIXED on `fix/backlog-80`, 2026-10-10:** `doc_refs` scans 187 docs; `facts_check` fails on a missing source; `doc_symbols_check` matches whole identifiers and the 22 names that passed as substrings were corrected in the docs; board builds run the full `EXTRA_CHECKS`; an unhonoured `conform-ok` is refused; the red-team exits on its verdict; `contract_check` reconciles the examples. `foreign_word_check` already scanned COMMANDMENTS.md.
- **T9. Limine v12.2.0 in three workflows, v12.3.1 in `release.yml`.**
  **FIXED on `fix/backlog-80`, 2026-10-10:** all four workflows fetch v12.3.1, and `facts_check` fails on a mismatch.

## Repository

- **R1. 13 `.rs` files carry no SPDX licence tag**: `services/dwc2/src/*` (9), `services/net-stack/src/tcp.rs`,
  `services/probe/src/table.rs`, `services/wifi-driver/src/main.rs`, `osdev/src/fs_model.rs`. Fix: add them,
  and a check.
  **FIXED on `fix/backlog-80`, 2026-10-10:** the 13 files tagged, and `scripts/spdx_check.py` requires a tag in every tracked `.rs` file.
- **R2. `osdev/src/main.rs` `boot_blockdev_qemu` appears to have no callers** (its comment says ATA, retired).
  **FIXED on `fix/backlog-80`, 2026-10-10:** deleted.

## Fixed during Audit 14, recorded here for the operator's review

Audit 14 found these and changed the code before the rule above was set; all twelve x86 QEMU suites pass
with them, and the VisionFive ran `selfcheck` 539/0 either side of a 100-round `max-carnage` on them. They
stay unless the operator says otherwise: the shell reporting a mutation `fs` died holding as "failed"
(introduced by this branch); `gs::io` silently dropping a line over 256 bytes; `examples/resource-server`
leaking a reply-cap slot per invocation; GENET's log calling a rights refusal a dead cap; `fs`'s last raw
`resource_mint`; and `scripts/stdlib_gap_check.py`'s stale driver list, now on the build path.
