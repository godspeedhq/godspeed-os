# 80. Code defects found by the 2026-10-09 documentation audits

**Status: OPEN - recorded, not fixed. The audits changed documents and comments only; each item here is
code, and is fixed separately, one at a time. Items under "Kernel" need the operator's go-ahead.**

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
- **K2. The T630's `audio-driver` is probably bound to the HDMI audio controller, not the analog codec.**
  `kernel/src/arch/x86_64/pci.rs` `find_by_class` returns the FIRST device of a class; the 2026-10-08 log
  shows 00:01.1 (HDMI) confined, not 00:09.2 (Azalia). Fix: finish step D3, so a supplied BDF selects the
  window, arena and vector, not only bus mastering.
  **FIXED in QEMU on `feat/audio-finish`, 2026-10-09, T630 card pending.** One resolver, `HwClass::pci_dev`,
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
- **K4. A received capability can be lost silently.** In `handle_recv`, `try_recv`, `recv_timeout` and
  `do_call`, a failed `current_task_insert_cap` (receiver's table full) drops the embedded cap, already
  removed from the sender; `push_pending_recv_cap` drops slots past 4 - the cap is installed but the
  service never learns its slot. Fix: refuse delivery, or report it.
- **K5. `recv`, `try_recv` and `recv_timeout` truncate a payload longer than the buffer silently**, where
  `Call` refuses (26.7). Fix: refuse, or report the truncation.
- **K6. riscv64: a kernel fault halts only the faulting hart** (`trap.rs`, `super::halt()`); the others run on
  until the liveness watchdog panics about 10 s later, against 6.2 (a panic halts every core). Fix:
  `halt_all_cores()` after `report_fault` on the kernel path.
- **K7. x86 `int80_entry` is still installed at IDT[0x80] with DPL=3**, and runs the syscall chain on the
  top-of-kstack region the timer switch writes - the Bug 2 class the `ud2` path was moved off. Any service
  can issue `int 0x80`. Fix: remove the entry (the SDK uses `ud2`), or re-base it as `ud2_syscall_entry` is.
- **K8. x86 `ud2_syscall_entry`'s ring-0 path is `cli; hlt` on one core, silently**, so a kernel Rust trap
  is invisible until the watchdog. Fix: route to `exception_halt` / `halt_all_cores`.
- **K9. x86 `exception_halt_handler` tests CS for 0x08/0x28**; a ring-3 frame carries 0x2B, so a user-mode
  frame reaching it prints no RIP. Fix: compare `w & !3 == 0x28`.
- **K10. x86 COM1 input ring has two unsynchronised producers** (`console_push_byte` from syscall context,
  `uart_rx_drain_fifo` from the core-0 timer), and its comment says so. Bytes can be lost or doubled where
  COM1 input is live. Fix: a lock, or a CAS on the tail.
- **K11. x86 `iommu::confine_device` loses records silently**: a full 4-slot `CONFINED_TABLE` drops the
  record (I/O page table leaked, DTE never reverted); a failed `io_map_page` leaks partial tables;
  re-confining a BDF replaces its record without freeing the old table. Fix: log and refuse when full; free
  on failure.
- **K12. x86 `ioapic::set_level_route` drops silently when its 16 slots are full.** Fix: log.
- **K13. `kernel_main`'s attributes sit on `banner`**: `#[no_mangle]` and a clippy `allow` in
  `kernel/src/main.rs` apply to `banner`, because a doc comment and fn were inserted between them and
  `kernel_main`.
- **K14. `SpawnWithCaps` returns -2 for "spawned, no endpoint"**, the same value as `CapNotHeld`, so a
  failure reads as `Ok(None)` in the SDK. Syscalls 7, 19, 38 and 39 can now only ever name `supervisor`
  (the catalogue holds nothing else): surface Commandment I pins that does nothing.
- **K15. `do_call` validates `buf_ptr` for 4096 bytes** even when `CallDeadline` declares a smaller reply
  buffer (reachability unverified). **`handle_resource_mint` leaks the id and cap when `write_user_bytes`
  fails.**
- **K16. `kernel/build.rs` looks for `.git/logs/HEAD` relative to `kernel/`**, so the commit-SHA rerun trigger
  is never registered; 25 of its 29 `SVC_*_ELF` lines are unused. Fix: `workspace.join(".git/logs/HEAD")`.
- **K17. x86 `route_ehci_intx` always targets the BSP while `EHCI_CORE` is 3**, so each wake crosses cores
  (unverified on hardware). **Possible:** a line masked at a driver's death is not unmasked by `register`;
  whether a respawned level-triggered `ehci` unmasks before its first interrupt is unverified.
- **K18. InspectKernel's ungated list still names deleted queries 9 and 22**, and INTROSPECT refusals return
  data-like zeros. Harmless today, wrong in an authority list.
- **K19. Strings broken by line joins**: the AcquireSendCap "table FULL" log, a log near `dispatch.rs` ~1580,
  riscv64 `context_switch.rs` panic messages, raw line breaks in riscv64 `trap.rs` and `usb.rs` literals.
- **K20. Dead kernel code**: `SPAWN_FLAG_HAS_RECV/SMALL_MEM/IS_PROBE`, the `Task` and `Endpoint` structs,
  `smp::placement`, `memory::ownership`, `revoke::revoke`, `route::unregister`, both TLB broadcasts,
  `assert_cap_validated(&Ok(()))` (cannot fire), `assert_tcb_alive` (a no-op), `program_xhci_msi`,
  `uart_rx_enable`, four unused `pci.rs` constants, an empty `if progif == PROGIF_XHCI {}`.
- **K21. Pi 4: the framebuffer is mapped with two memory types at once.** `console` gets it as Normal
  non-cacheable (MAIR slot 2) while the kernel's direct map still covers the same pages as Normal
  write-back cacheable - mismatched attributes ARM leaves UNPREDICTABLE. Fix: map the range non-cacheable
  in the direct map (split the 2 MiB block), or withhold the grant.
- **K22. GICv2 EOI drops the SGI source CPU**: `gic::acknowledge` returns `IAR & 0x3FF` and `eoi` writes only
  the ID, but an SGI's EOIR value must carry IAR's CPUID bits [12:10], so an SGI from cores 1-3 is EOI'd with
  a mismatched value (effect on hardware unverified). Fix: EOI with the raw IAR value.
- **K23. aarch64 `cpu_clock` returns a rate it did not read back** (`now.unwrap_or(set)`); CLAUDE.md 12.3
  says the result is the read-back. Fix: `None`, or log, when the read-back fails.
- **K24. riscv32 `_start` clears BSS with RV64-only `sd` and an 8-byte stride** (could not build to confirm).
  Fix: `sw` and 4.
- **K25. arm32 strings and dead branches**: the kernel-fault log says "the arm32 port cannot yet kill a task"
  (it can); `wait_for_interrupt` has an empty `if !irq::usb_owned_by_userspace() { }` doing a route lookup on
  every idle pass; `route_usb_irq_to_core0` has no callers; `start_tick_ap` has an unreachable branch;
  `mmu::enable` prints "caches ON" after a selftest the caches were already on for; the framebuffer section is
  non-shareable while the console's small-page mapping is shareable (effect unverified).
- **K27. USB driver placement is stated twice with nothing linking the copies**: x86 MSI destinations use
  `task::XHCI_CORE`/`EHCI_CORE` (2 and 3), and the supervisor's `USB_IMAGES` rows hard-code 2 and 3. Change
  one alone and the interrupts land on the wrong core. Fix: one constant and a check, or derive the
  destination from the spawned task's core.
- **K28. arm32 `hw_random` is an unlocked FIFO pop**: two cores can both pass the "word available" check and
  read the same or a stale word - and it now feeds the WPA2 SNonce (`sdk/wifi` `supplicant.rs`). Fix: a
  spinlock around the check and the read.
- **K26. The stubs' `unimplemented!("aarch64::switch_to_boot_stack")` names the wrong arch** (riscv32,
  loongarch64, s390x). **aarch64 `mailbox::notify_xhci_reset` takes no `MBOX_LOCK`** and runs after the
  secondaries start - safe only because nothing calls it from a syscall yet.

## Standard library (`gs`)

- **G1. `gs::fs::Fs::write` cannot write more than one chunk (3556 bytes).** It creates the file with the
  first chunk (`OP_WRITE_FILE`), which `fs` sizes for that chunk alone, so the first `OP_WRITE_AT` past it
  is refused "write past extent": `Error::Failed`, and a truncated file left behind. Its own comment now
  says so. Fix: `OP_WRITE_NEW(total)`, then `OP_WRITE_AT` from 0.
- **G2. `gs::file::File` records the REQUESTED rights, not the granted ones**, so a sealed file narrowed to
  READ, or an APPEND-only open, closes under rights the cap lacks: the kernel refuses, `fs` never frees the
  slot (64 entries), and `Drop` swallows the error. Fix: record the granted rights; close under them.
- **G3. `gs::net::Conn::recv` truncates silently** (`min(data.len(), buf.len())`; `net-stack` returns up to
  2048 bytes already consumed). Fix: refuse with `BufferTooSmall`, or send the length.
- **G4. `gs::task::sleep_quantum` returns after about 1 us on a real Pi 2**: the counter is 1 MHz, so
  `sleep(1)` takes the sub-tick path, and loops meant to yield a quantum spin. Fix: sleep one 10 ms
  duration.

## SDK

- **S1. `spawn_via_supervisor` retries after ANY error, `ReplyDead` included**, so a supervisor that dies
  mid-spawn could start the service twice. Fix: retry only a send that never left.
- **S2. The SDK traces a full queue as a lost peer** (`request_with_reply_call_err` records every `Err` as
  `KIND_PEER_LOST`), so `gs::trace` shows `Busy` from `gs::call` as `PeerLost`. Fix: its own kind.
- **S3. `DeadlineOutcomeInto::SendFailed` includes `ReplyDead`**, though the request was delivered (safe for
  `fs`'s block transfers, wrong for a caller that is not idempotent).
- **S4. `restart` does `let _ = self.kill(name)`, and `kill` maps every error, `CapNotHeld` included, to
  `InvalidArgument`** - the silent drop CLAUDE.md 13.6's amendment describes, still there; the shell uses it.
- **S5. Silent drops and truncations**: `print` drops anything over 256 bytes (`gs::io` now chunks around
  it); `log_fmt` and `console_*_fmt` cut at 256 with no marker and log `(fmt error)` on a mid-character cut;
  `try_recv`/`recv_timeout` turn every error into `None`, so a deadline loop spins on a dead endpoint; drain
  loops discard messages without reclaiming their caps; `set_installs` allows 32-byte labels against the
  kernel's 24; `sleep` is not clamped on 32-bit; `max_secs <= 0` maps to a `CallDeadline` of 0, which
  blocks forever.
- **S6. `mmio.rs` `read16/32/64` and `write16/32/64` never check alignment**, so a safe call with an
  unaligned offset is a misaligned volatile access - undefined behaviour, and an alignment fault on Device
  memory. A possible soundness hole in the 18.1 layer. Fix: assert alignment, or compose as `Dma` does.
- **S7. `record::add_row` drops extra values without setting `overflow`.** **`drain_kernel_ring_buffer` is a
  dead stub** whose doc says `events` calls it.
- **S8. `sdk/wifi`**: `sdio.rs` `read_extended`/`write_extended` truncate the CMD53 count (`& 0x1FF`)
  silently; `prf_sha1` zero-fills on oversize input; `pbkdf2_sha1` cuts a salt over 60 bytes; `rxq::pop`
  returns 0 forever if the head frame does not fit; `serve.rs` and `keyfile.rs` treat names over 16 bytes
  differently; an `sdio.rs` log says "network adapter"; a Pi 4 hint prints on every board.

## Services

- **V1. `observe live` cannot start**: the shell calls `ctx.spawn_on("observe-live")`, and the kernel
  catalogue holds only `supervisor`, so it prints "failed to spawn" (from the code; not run). Fix: ask the
  supervisor.
- **V2. `fs` answers an absent path from a mutating op with `FS_ERR` and a reason, and `OP_OPEN` with a bare
  `FS_ERR`** (the reason dropped); a data block that fails its CRC answers `FS_NOTFOUND`, so a damaged file
  reads as missing. Fix: `FS_NOTFOUND` for walk misses, a distinct status for CRC failures.
- **V3. `fs` `op_is_mutating` omits `OP_SEAL`**, so `seal` can write the superblock on a read-only mount.
- **V4. `fs` `delete_tree` reports failure after the unlink committed** when freeing fails. Fix: success,
  with a leak warning. **An `fs` log string (the APPEND refusal) has a run of spaces.**
- **V5. The supervisor's misplaced `#[cfg]` attributes** gate `time` instead of `block-driver`, and stack a
  second onto `ensure_wired("wifi-driver")` - already open as backlog/77; the audit confirms it, unchanged
  since `c6473b55`.
- **V6. `let _ = ctx.spawn("observe")` in the supervisor probably fails silently**: it takes the kernel's
  by-name path, whose catalogue holds only `supervisor` - the trap the supervisor's own comments warn about at
  the `events` and `ping` spawns (runtime unverified). Fix: `spawn_mapped`.
- **V7. `fs`'s mount retry after `E_IO` is bounded by a count (1000 x `yield_now`), not a duration** - the
  count-is-not-a-duration bug the capacity loop above it already fixed. Fix: the same `sleep_ms` + clock.
- **V8. The supervisor adopts `events` and `console` without the `name_alive` check `ensure_mapped` uses**
  (`converge` mitigates). **`NAME_MAP_MAX` = 16 has no headroom** - exactly full on the Pi 4 and on x86 with
  the dongle attached.
- **V9. Contracts declare endpoints their spawn rows never request**: `mem-pressure` and `observe` declare
  `ipc_receive` with no `SPAWN_FLAG_REQ_RECV`, which `contract_check` does not compare. **Dead code**:
  `usbdisk.rs`'s BUSY/ABSENT arms (so "no disk" now logs per block), chaos `PACE_YIELDS`, recorder `FS_TAG`,
  control `Q_COM2_BYTE`; `ahci.rs` `cache_commit` discards `write_block` errors under its test feature.

## Drivers and the network stack

- **D1. `ehci` puts the QH hub port at bit 22**, in `control` and `poll_devices` (`<< 22`); the EHCI spec and
  Linux's QH_HUBPORT mask (0x3f800000) put Port Number at [29:23] and Hub Address at [22:16], so port 1 sets the
  hub address's top bit and reads as port 0. Its keyboard is recorded as hardware-verified, so which port
  that run used matters. Fix: `<< 23`.
- **D2. `nic-driver` drives any class-0x020000 NIC that is not an RTL8168 as an e1000**, with no vendor or
  device check. Fix: check the e1000 IDs; refuse with a status otherwise.
- **D3. `net-stack` op 8 (renew) does not set `leased`** as the other dances do, so a stale lease flag drives
  re-DHCP and retry wrongly.
- **D4. `net-stack` has raw requests that bypass its displaced-request sifting** - the ARP reply and receive
  poll in the DNS loop, and `ping`'s ARP - so they can consume a client's request. Fix: `nic_req`/`nic_req_ms`.
- **D5. `net-stack` op 21's failure reply is empty**, which `Reply::send` `debug_assert!`s against (a debug
  build panics), and an empty message is refused on three ports (backlog/66). Fix: a status byte.
- **D6. `dwc2`'s fallback loop (no MMIO/DMA grant) takes requests without answering or reclaiming their reply
  caps**, so clients hang to their deadline. Fix: answer through `dispatch`/`answer_no_disk`.
- **D7. `xhci` misses a root-port unplug with two HIDs bound**: root-port detection is gated on
  `ndev < MAX_HID`, and the same gate covers the WiFi dongle's empty-port check and the poison clearing.
- **D8. `xhci`'s disk-hub scan re-enumerates on ONE connected read**, where the HID scan requires two
  (`hub_seen`) for the phantom-arrival reason its own comment gives.
- **D9. Invariant 9 statics**: `ehci` `TIMED_OUT_ONCE`; `xhci` `PROBE_FAILS`, `DIAG_HINT_SAID`, and in `msc.rs`
  `NO_DISK_LOGGED`, `READ_FAIL_LOGGED`. Fix: loop-owned state, as `xhci`'s own comments recommend.
- **D10. Strings broken by lost line continuations** (about 25 spaces mid-line): five in `net-stack`, one in
  `tcp.rs`, two in `dwc2`, one in `dwmac_ring.rs`.
- **D11. Smaller**: `dwc2` `net::serve` discards the reply result; `net-stack` `feed_tx` is effectively dead;
  unused `stale_after`/`last_ok` (dwc2 hid), `POLL_FRAMES`/`active()` (tcp), `tx_fault_counters` (dwmac);
  `wifi-usb`'s ST_FAILED message says "has no bulk IN" for a failed repair too.

## Shell

- **H1. `fg` prints `copying... N%` for every job kind**; a delete-tree, check or scrub reads `copying...
  100%` while it runs. Fix: as `jobs` does.
- **H2. `chaos kill-storm`'s refusal says "only supervisor/block-driver/fs recover on death"**;
  `CHAOS_RESTARTABLE` has 11 entries. Fix: build the sentence from the array.
- **H3. chaos target lists drift** between the help, two `max-carnage` hints and `kill-storm` (which refuses
  `shell`; `max-carnage` takes any live service). Fix: derive them from `CHAOS_RESTARTABLE`.
- **H4. chaos link-flap says "The in-kernel ARM NICs have no link override"**; they are `nic-driver`
  backends. Fix: "this NIC backend has no link override".
- **H5. `drives` shows a zero-capacity drive as a `raw 0 MiB` row.**
- **H6. The shell's `op_is_mutating` omits `OP_WRITE_AT_J`**, which `fs`'s has (latent; nothing compares the
  lists).
- **H7. `#[inline(never)]` sits on `let_capture_form`, not `run_lines`** - on `main` too. The attribute was
  written for `run_lines`, which holds the verdict array on a tight stack. VERIFIED. Fix: move it.
- **H8. `fmt` is not in `UTILS`**: `fmt help` treats "help" as a path and its help block is unreachable;
  `whatis` answers "unknown" for `fmt`, `tcp`, `serve`, `random`, `gpio`, `spawncap`, `spawnwired`.
- **H9. `PRODUCER_COLS` is wrong for `roster`** (emits name/role/seat) **and `uptime`** (uptime/seconds), so
  Tab offers columns that do not exist.
- **H10. Failures reported as success**: `ping` usage errors and a bad address; `net_stats_dump` on a timeout
  or abort; all three `date sync` failure paths; the comma-list forms of `fmt`, `mkdir` and `restart` (which
  also drop items past 16 silently).
- **H11. `stream_minify` treats an `fs_read_at` error as end of file**, silently truncating a script; the
  import path ignores the truncation flag.
- **H12. The shell is a second writer of `/clock.last`** though `time` owns it, and two strings say "the time
  service refused" when `time` never refuses.
- **H13. Wrong sizes and names in strings**: "(16 KiB cap)" (12 KiB), "(4 KiB)" (512 B), an assert naming
  `complete_command` (`complete_tab`), `wifi debug firmware`/`transport` hard-coding CYW43455 and SDIO, the
  down-reason-4 "not written yet".
- **H14. Tab completion**: `ping` drops `version` and `help`; `RESTART_TARGETS` offers `supervisor` and
  `shell`, which `restart` refuses. The `pwd` hint sends the user to `cd`, which goes to `/`.

- **H15. `drives flash typo` erases drive 0** (after the y/N): `drive_sel_ok` accepts any non-numeric word as
  a label without comparing it to anything. The most dangerous item in this section. Fix: compare against
  the superblock label, and refuse a word that does not match.
- **H16. Utility convention 10 (`q` quits a wait) regressed across the fs and net commands**: `fs_stat_r`,
  `fs_read_at`, `filter_read`, the `find`/`tree` walks, `build_dir_table` and `sock`'s `gs::net::Net::new`
  build handles with no notice, so `cd`, `read`, `find`, `tree`, `match`, `count`, `sort`, `first`, `last`,
  `copy`, `edit`'s open and `sock` cannot be quit (`sock` can wait about 40 s); their `Cancelled` arms are
  dead. Fix: `.noticing(&notice)` / `Net::with_notice`, as `dir` and `tcp` do.
- **H17. `ping` caps the round trip at 65 ms silently**: `net-stack` stores it as `u16` microseconds
  (`.min(65535)`). Fix: widen it, or print "65 ms or more".
- **H18. Recursive `copy` reports success when it was not**: `cmd_copy_tree` returns `Ok` after skipped files,
  an INCOMPLETE listing or a truncated walk; a subdirectory `mkdir_at` failure and a `join_path`/`remap`
  overflow are skipped silently; `copy_file_streaming` reports a short read as a complete copy.
- **H19. `find` reports any listing error** (a bad start path, a storage error) as "INCOMPLETE ... too large",
  then `0 match(es)` and `Ok`.
- **H20. More silent fallbacks**: `date epoch` prints `0` when the clock is unset; `max-carnage` usage and
  unknown-target errors and `link-flap` with no NIC return `Ok`; comma-list `spawn`/`kill`/`delete` (as well
  as the H10 set) always return `Ok`; `inspect_core_count` returns 1 on error; `min`/`max` of zero rows is 0;
  `cores <junk>` and `events log <junk>` use defaults silently; `cores ticks | ...` drops `ticks`.
- **H21. Convention gaps**: `ping` accepts hidden synonyms `size` and `n` (rule 3); `fcap reuse`/`gsreuse` are
  in neither help nor completion; `cores ticks` is not in `SUBCMD_FIRST`; `wait` busy-yields instead of
  sleeping; `cores ticks` has no `q` for its 5 s; `observe` live quits on `q` only, not Esc; `observe now`
  waits on a 1,000,000-yield count; the observe painter title and the shell's notice differ though a comment
  says they match byte for byte; `read` on a directory says "not found"; `parse_duration` checks the year
  bound before applying the unit.

## Examples

- **E1. `stdlib-hello` never reaches `fs`, and its test passes anyway**: its spawn row has flags 0 (no
  `REQ_RECV`), so every `gs::fs` call is `Unreachable`; `build/tests/examples_serial.log` shows "could not be
  reached" while `run_examples` accepts "stdlib-hello: done". Fix: set `REQ_RECV`; assert on the file.
- **E2. `examples/counter` leaks a cap slot per second while saves fail** (`gs::cap::acquire(..).is_ok()`).
- **E3. `resource-server`'s log cites a by-name grant that does not exist.**

## Tooling and gates

- **T1. The Python 3.8 floor is false and its checker cannot see why**: three checkers use `list[...]` /
  `dict[...]` in signatures with no `from __future__ import annotations` (a TypeError on 3.8, which
  `build.yml` pins), and `python_floor_check` matches no `def` line (reasoned from PEP 585; not run on 3.8).
- **T2. `port_scope_check.py` fails on every branch**: `docs/porting.md` marks 12 rows `+`, `SEAM_ABOVE`
  allows 11 - hidden because only the paused `build.yml` runs it.
- **T3. `osdev test <typo>` exits 0.**
- **T4. `commandments.py` reports 10/10 mechanised; it is 9**, and prints stale counts.
- **T5. `stack_fit_check` passes while measuring nothing** when objdump fails.
- **T6. `unsafe_check` never compares counts for the four `SDK_PERMITTED` files.**
- **T7. `service_embed_check` reads vestigial kernel lists on arm/aarch64 and checks no x86_64 list.**
- **T8. Gates that claim more than they check**: `doc_refs` skips CLAUDE.md, COMMANDMENTS.md, `examples/` and
  `tests/`; `facts_check` drops a fact whose source is missing; `doc_symbols_check` resolves by substring and
  passes after writing a missing baseline; board builds run 7 checkers, not the 24 in `EXTRA_CHECKS`;
  `conform-ok` markers for other rules are ignored; `commandments_redteam` always exits 0;
  `foreign_word_check` does not scan COMMANDMENTS.md (III says `fsck`); `contract_check` does not reconcile
  `examples/`, whose contracts drift from their spawn rows.
- **T9. Limine v12.2.0 in three workflows, v12.3.1 in `release.yml`.**

## Repository

- **R1. 13 `.rs` files carry no SPDX licence tag**: `services/dwc2/src/*` (9), `services/net-stack/src/tcp.rs`,
  `services/probe/src/table.rs`, `services/wifi-driver/src/main.rs`, `osdev/src/fs_model.rs`. Fix: add them,
  and a check.
- **R2. `osdev/src/main.rs` `boot_blockdev_qemu` appears to have no callers** (its comment says ATA, retired).

## Fixed during Audit 14, recorded here for the operator's review

Audit 14 found these and changed the code before the rule above was set; all twelve x86 QEMU suites pass
with them, and the VisionFive ran `selfcheck` 539/0 either side of a 100-round `max-carnage` on them. They
stay unless the operator says otherwise: the shell reporting a mutation `fs` died holding as "failed"
(introduced by this branch); `gs::io` silently dropping a line over 256 bytes; `examples/resource-server`
leaking a reply-cap slot per invocation; GENET's log calling a rights refusal a dead cap; `fs`'s last raw
`resource_mint`; and `scripts/stdlib_gap_check.py`'s stale driver list, now on the build path.
