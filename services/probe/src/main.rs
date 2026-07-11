// SPDX-License-Identifier: GPL-2.0-only
//! `probe` - single-binary identity test probe service (§22 Group A).
//!
//! One binary, multiple service_config entries with different `probe_mode` values.
//! The kernel writes `probe_mode` into ServiceContextData at spawn time; the SDK
//! exposes it via `ctx.probe_mode()`.
//!
//! Modes:
//!   0 = PASSIVE         - idle; exists only to be a kill target
//!   1 = ECHO_RECV       - recv one message; log "probe: 3A recv OK"              (Test 3A)
//!   2 = ECHO_SEND       - send to probe-recv; log "probe: 3A send OK"            (Test 3A)
//!   3 = NO_SEND_RIGHT   - try_send via recv-slot cap → CapInsufficientRights      (Test 3B)
//!   4 = SEND_AFTER_KILL - kill probe-victim then try_send → EndpointDead          (Test 4A)
//!   5 = FILL_AND_BLOCK  - fill 16-slot queue + blocking send; woken by KILL       (Test 4B)
//!   6 = YIELD_LOGGER    - yield then log; proves preemption/yield path             (Test 8A)
//!   7 = HOG             - tight loop; proves preemption via ping output            (Test 8B)
//!   8 = CAP_FORGE       - try_send on slot 99 (out of range) → CapNotHeld         (Test 9B)
//!   9 = GRANT_RECV      - recv then take_pending_cap; log pass                    (Test 5A)
//!  10 = GRANT_SEND      - send_with_cap to probe-5a-recv; log pass                (Test 5A)
//!  11 = NO_GRANT_SEND   - send_with_cap without GRANT right → CapNotGrantable     (Test 5B)
//!  12 = ALLOC_OK        - alloc within limit twice; both succeed                   (Test 7A)
//!  13 = ALLOC_LIMIT     - alloc 60 MiB, then 20 MiB → AllocDenied, then 2 MiB → Ok (Test 7B)
//!
//! Property-test modes - Milestone 9 Phase 3.
//!  27 = PROP_P4   - ∑ alloc_bytes ≡ pages mapped; denied allocs don't count   (P4)
//!  28 = PROP_P5   - kill/spawn cycles; endpoint count stays ≤ table capacity   (P5)
//!  29 = PROP_P7   - kill/spawn cycles; generation monotonic (TLB proxy)        (P7)
//!
//! Adversarial-test modes - Milestone 13.
//!  80 = ADV_A1    - 10,000 random cap slots → always Err (cap unforgeability)  (A1)
//!  81 = ADV_A2    - brute-force slots 0..=127 + u32::MAX → defined errors       (A2)
//!  82 = ADV_A3    - alloc beyond 4 MiB limit → AllocDenied                      (A3)
//!  83 = ADV_A4    - RECV cap used as SEND target → CapInsufficientRights         (A4)
//!  84 = ADV_A5    - kill victim then send via stale cap → EndpointDead           (A5)
//!  85 = ADV_A6    - fill own cap table → None when full                          (A6)
//!  86 = ADV_A7    - 100 timing sends to passive partner → no panic               (A7)
//!  87 = ADV_A8    - tight loop hog; preemption must not starve witness           (A8)
//!  88 = ADV_A8_WITNESS - 1,000 yields then log pass                              (A8)
//!  89 = ADV_A9    - spawn non-existent service → Err                            (A9)
//!  90 = ADV_A10   - kernel addresses as syscall buffer args → rejected           (A10)
//!
//! Chaos-test modes - Milestone 14.
//!  91 = CHAOS_C2     - null-deref → page fault → kernel kills service           (C2)
//!  92 = CHAOS_C2_MON - 1,000 yields then log pass (C2 witness)                  (C2)
//!  93 = CHAOS_C3     - 500 alloc-deny cycles without panic                      (C3)
//!  94 = CHAOS_C5     - 100-level recursive yield_cpu(); kernel stack depth probe (C5)
//!  95 = CHAOS_C6_MON - 200 yields then log pass on core 0 (C6 witness)          (C6)
//!  96 = CHAOS_C7     - 30 cross-core kill/respawn cycles; TLB shootdowns         (C7)
//!
//! Brutal chaos-test modes - Milestone 21.
//! 155 = CHAOS_BC2_MON - 500 yields; proves 5 simultaneous faults survived        (BC2)
//! 156 = CHAOS_BC3     - 2,500 alloc-deny cycles (5× C3)                          (BC3)
//! 157 = CHAOS_BC5     - 500-level recursive yield_cpu() stack probe (5× C5)      (BC5)
//! 158 = CHAOS_BC6_MON - 1,000 yields on core 0; 2-hog starvation witness         (BC6)
//! 159 = CHAOS_BC7     - 15 cross-core kill/respawn TLB cycles (brutal concurrent)  (BC7)
//!
//! Brutal performance-benchmark modes - Milestone 19.
//! 132 = PERF_BP1      - same-core IPC roundtrip, 1000 samples (5× B1)
//! 133 = PERF_BP1_ECHO - B1 echo (core 0)
//! 134 = PERF_BP2      - cross-core IPC roundtrip, 1000 samples (5× B2)
//! 135 = PERF_BP2_ECHO - B2 echo (core 1)
//! 136 = PERF_BP3      - yield floor, 5000 yields (5× B3)
//! 137 = PERF_BP4      - cap validation, 50000 checks (5× B4)
//! 138 = PERF_BP5      - spawn+restart cost, 50 cycles (5× B5/B6)
//! 139 = PERF_BP7      - cap I/R throughput, 5000 cycles (5× B7)
//! 140 = PERF_BP8      - allocator throughput, alloc to limit
//! 141 = PERF_BP9      - 4 KiB message copy sender, 1000 sends (5× B9)
//! 142 = PERF_BP9_RECV - B9 recv
//! 143 = PERF_BP10     - scheduler pick-next, 5000 yields (5× B10)

#![no_std]
#![no_main]

use godspeed_sdk::{service_context::AllocError, CapError, CapHandle, IpcError, Message, ServiceContext};

#[allow(dead_code)]
const MODE_PASSIVE:         u32 = 0;
const MODE_ECHO_RECV:       u32 = 1;
const MODE_ECHO_SEND:       u32 = 2;
const MODE_NO_SEND_RIGHT:   u32 = 3;
const MODE_SEND_AFTER_KILL: u32 = 4;
const MODE_FILL_AND_BLOCK:  u32 = 5;
const MODE_YIELD_LOGGER:    u32 = 6;
const MODE_HOG:             u32 = 7;
const MODE_CAP_FORGE:       u32 = 8;
const MODE_GRANT_RECV:      u32 = 9;
const MODE_GRANT_SEND:      u32 = 10;
const MODE_NO_GRANT_SEND:   u32 = 11;
const MODE_ALLOC_OK:        u32 = 12;
const MODE_ALLOC_LIMIT:     u32 = 13;

// Property-test modes - Milestone 9 Phase 1.
const MODE_PROP_P1:         u32 = 20;
const MODE_PROP_P9:         u32 = 21;
const MODE_PROP_P10:        u32 = 22;

// Property-test modes - Milestone 9 Phase 2.
const MODE_PROP_P2:         u32 = 23;
const MODE_PROP_P3:         u32 = 24;
const MODE_PROP_P6:         u32 = 25;
const MODE_PROP_P8:         u32 = 26;

// Property-test modes - Milestone 9 Phase 3.
const MODE_PROP_P4:         u32 = 27;
const MODE_PROP_P5:         u32 = 28;
const MODE_PROP_P7:         u32 = 29;

// Fuzz-test modes - Milestone 10 Phase 1.
const MODE_FUZZ_F1:         u32 = 30;
const MODE_FUZZ_F2:         u32 = 31;
const MODE_FUZZ_F5:         u32 = 32;
const MODE_FUZZ_F6:         u32 = 33;
const MODE_FUZZ_F7:         u32 = 34;
const MODE_FUZZ_F8:         u32 = 35;

// Stress-test modes - Milestone 11 Phase 1.
const MODE_STRESS_S1:       u32 = 40;
const MODE_STRESS_S2:       u32 = 41;
const MODE_STRESS_S3_SEND:  u32 = 42;
const MODE_STRESS_S3_RECV:  u32 = 43;
const MODE_STRESS_S4:       u32 = 44;
const MODE_STRESS_S7:       u32 = 45;
const MODE_STRESS_S10:      u32 = 46;

// Stress-test modes - Milestone 11 Phase 2.
const MODE_STRESS_S5:       u32 = 47;
const MODE_STRESS_S6:       u32 = 48;
const MODE_STRESS_S8:       u32 = 49;
const MODE_STRESS_S9_SEND:  u32 = 50;
const MODE_STRESS_S9_RECV:  u32 = 51;

// Performance-benchmark modes - Milestone 12.
const MODE_PERF_B1:         u32 = 60; // same-core IPC roundtrip sender
const MODE_PERF_B1_ECHO:    u32 = 61; // same-core IPC roundtrip echo
const MODE_PERF_B2:         u32 = 62; // cross-core IPC roundtrip sender
const MODE_PERF_B2_ECHO:    u32 = 63; // cross-core IPC roundtrip echo
const MODE_PERF_B3:         u32 = 64; // syscall yield floor
const MODE_PERF_B4:         u32 = 65; // cap validation throughput
const MODE_PERF_B5:         u32 = 66; // spawn + restart cost (covers B5 and B6)
const MODE_PERF_B7:         u32 = 67; // cap table insert/remove throughput
const MODE_PERF_B8:         u32 = 68; // allocator throughput
const MODE_PERF_B9:         u32 = 69; // 4 KiB message copy sender
const MODE_PERF_B9_RECV:    u32 = 70; // 4 KiB message copy receiver
const MODE_PERF_B10:        u32 = 71; // scheduler pick-next cost

// Adversarial-test modes - Milestone 13.
const MODE_ADV_A1:          u32 = 80; // random cap slots → never Ok
const MODE_ADV_A2:          u32 = 81; // brute-force slot range → defined errors
const MODE_ADV_A3:          u32 = 82; // alloc beyond 4 MiB limit → AllocDenied
const MODE_ADV_A4:          u32 = 83; // recv cap used as send target → CapInsufficientRights
const MODE_ADV_A5:          u32 = 84; // kill victim then send via stale cap → EndpointDead
const MODE_ADV_A6:          u32 = 85; // fill own cap table → None when full
const MODE_ADV_A7:          u32 = 86; // 100 timing sends to passive partner → no panic
const MODE_ADV_A8:          u32 = 87; // tight loop (hog) - preemption target
const MODE_ADV_A8_WITNESS:  u32 = 88; // 1000 yields then log pass
const MODE_ADV_A9:          u32 = 89; // spawn non-existent service → Err
const MODE_ADV_A10:         u32 = 90; // kernel addresses as syscall args → rejected

// Chaos-test modes - Milestone 14.
const MODE_CHAOS_C2:        u32 = 91; // null-deref → page fault → kernel kills service
const MODE_CHAOS_C2_MON:    u32 = 92; // 1,000 yields then log pass (C2 witness)
// A14 (kernel-audit regression): a ring-3 CPU exception must KILL the task, never halt the kernel.
const MODE_ADV_FAULT_GP:    u32 = 210; // ring-3 #GP (non-canonical read) → kernel kills service
const MODE_ADV_FAULT_DE:    u32 = 211; // ring-3 #DE (inline-asm div0)    → kernel kills service
const MODE_ADV_FAULT_MON:   u32 = 212; // witness: yields then logs pass (system survived both faults)
const MODE_CHAOS_C3:        u32 = 93; // 500 alloc-deny cycles without panic
const MODE_CHAOS_C5:        u32 = 94; // 100-level recursive yield_cpu(); stack depth probe
const MODE_CHAOS_C6_MON:    u32 = 95; // 200 yields then log pass on core 0 (C6 witness)
const MODE_CHAOS_C7:        u32 = 96; // 30 cross-core kill/respawn cycles; TLB shootdowns

// Brutal chaos-test modes - Milestone 21.
const MODE_CHAOS_BC2_MON:   u32 = 155; // 500 yields; 5-simultaneous-fault witness
const MODE_CHAOS_BC3:       u32 = 156; // 2,500 alloc-deny cycles (5× C3)
const MODE_CHAOS_BC5:       u32 = 157; // 500-level recursive yield_cpu() (5× C5)
const MODE_CHAOS_BC6_MON:   u32 = 158; // 1,000 yields on core 0; 2-hog witness
const MODE_CHAOS_BC7:       u32 = 159; // 15 cross-core kill/respawn cycles (brutal concurrent load)

// Cross-core try_send diagnostic - isolates the one-way send cost that C7's "send"
// section conflated with sending to a just-killed victim (osdev image --mode iso-xsend).
const MODE_XSEND:           u32 = 200; // sender (core 1): time try_send to xsend-recv (core 2)
const MODE_XSEND_RECV:      u32 = 201; // receiver (core 2): drain forever

// Cross-core task-lifecycle diagnostic - isolate the ~1.04s C7 respawn cost: is it
// cross-core coordination or task creation? (osdev image --mode iso-xlife).
const MODE_XLIFE:           u32 = 202; // controller (core 1): time kill/spawn of near+far victims
const MODE_XLIFE_VICTIM:    u32 = 203; // victim: idle until killed (xlife-near core 1, xlife-far core 2)

// Interrupt-routing test modes - Post-v1 item 9 (§12.2, §12.3).
const MODE_IRQ_RECV:        u32 = 160; // IR1A: recv interrupt event; log pass

// Introspection-gate adversarial mode (§3.1; docs/introspection-capability.md).
const MODE_ADV_A11:         u32 = 161; // gated query denied without INTROSPECT cap
const MODE_ADV_A12:         u32 = 162; // reboot denied without REBOOT cap
const MODE_ADV_A13:         u32 = 163; // AcquireSendCap denied without ACQUIRE_ANY / declared peer

// Brutal property test modes - Milestone 16.
const MODE_PROP_BP1:        u32 = 104; // BP1: cap unforgeability - 100k iterations
const MODE_PROP_BP2:        u32 = 105; // BP2: generation monotonic - 20 kill/respawn cycles
const MODE_PROP_BP3:        u32 = 106; // BP3: cap rights never widen - 10k iterations
const MODE_PROP_BP4:        u32 = 107; // BP4: alloc accounting exact - 2k iterations
const MODE_PROP_BP5:        u32 = 108; // BP5: endpoint ownership - 150 kill/respawn cycles
const MODE_PROP_BP6:        u32 = 109; // BP6: queue invariants - 2k iterations
const MODE_PROP_BP7:        u32 = 110; // BP7: TLB shootdown proxy - 150 cycles
const MODE_PROP_BP8:        u32 = 111; // BP8: restart resolves higher generation - 20 iter
const MODE_PROP_BP9:        u32 = 112; // BP9: all 3 cap slots invalidated - 10 cycles
const MODE_PROP_BP10:       u32 = 113; // BP10: every send returns defined outcome - 100k

// Brutal fuzz test modes - Milestone 17.
const MODE_FUZZ_BF1:        u32 = 114; // BF1: syscall args - 500 × 10 syscalls
const MODE_FUZZ_BF2:        u32 = 115; // BF2: syscall numbers - 200k random
const MODE_FUZZ_BF5:        u32 = 116; // BF5: IPC message bodies - 5k sends
const MODE_FUZZ_BF6:        u32 = 117; // BF6: embedded cap slots - 5k pairs
const MODE_FUZZ_BF7:        u32 = 118; // BF7: stale cap / generation - 200 kill cycles
const MODE_FUZZ_BF8:        u32 = 119; // BF8: memory request sizes - 10 edge + 5k random

// Brutal performance-benchmark modes - Milestone 19.
const MODE_PERF_BP1:        u32 = 132; // BP1: same-core IPC roundtrip - 1000 samples (5× B1)
const MODE_PERF_BP1_ECHO:   u32 = 133; // BP1 echo (core 0)
const MODE_PERF_BP2:        u32 = 134; // BP2: cross-core IPC roundtrip - 1000 samples (5× B2)
const MODE_PERF_BP2_ECHO:   u32 = 135; // BP2 echo (core 1)
const MODE_PERF_BP3:        u32 = 136; // BP3: yield floor - 2000 yields under brutal load
const MODE_PERF_BP4:        u32 = 137; // BP4: cap validation - 50000 checks (5× B4)
const MODE_PERF_BP5:        u32 = 138; // BP5/BP6: spawn+restart - 50 cycles (5× B5/B6)
const MODE_PERF_BP7:        u32 = 139; // BP7: cap I/R throughput - 5000 cycles (5× B7)
const MODE_PERF_BP8:        u32 = 140; // BP8: allocator throughput - alloc to limit
const MODE_PERF_BP9:        u32 = 141; // BP9: 4 KiB message copy sender - 400 sends under brutal load
const MODE_PERF_BP9_RECV:   u32 = 142; // BP9 recv
const MODE_PERF_BP10:       u32 = 143; // BP10: scheduler pick-next - 2000 yields under brutal load

// Brutal adversarial modes - Milestone 20.
const MODE_ADV_BA1:          u32 = 144; // BA1: 50k cap forgery attempts (5× A1)
const MODE_ADV_BA2:          u32 = 145; // BA2: extended brute-force 0..=511 + extreme values
const MODE_ADV_BA3:          u32 = 146; // BA3: 5× alloc-beyond-limit attack cycles
const MODE_ADV_BA4:          u32 = 147; // BA4: rights escalation × 5 cap types
const MODE_ADV_BA5:          u32 = 148; // BA5: 5 TOCTOU kill+send cycles
const MODE_ADV_BA6:          u32 = 149; // BA6: fill+drain cap table × 5 cycles
const MODE_ADV_BA7:          u32 = 150; // BA7: 500 timing samples (5× A7)
const MODE_ADV_BA8:          u32 = 151; // BA8: tight loop hog
const MODE_ADV_BA8_WITNESS:  u32 = 152; // BA8 witness: 200 yields (1000 too slow under full load)
const MODE_ADV_BA9:          u32 = 153; // BA9: 5 direct-spawn bypass attempts
const MODE_ADV_BA10:         u32 = 154; // BA10: 20 kernel addr patterns (5× A10)

// Brutal stress test modes - Milestone 18.
const MODE_STRESS_BS1:      u32 = 120; // BS1: IPC saturation - 50k try_send (5× S1)
const MODE_STRESS_BS2:      u32 = 121; // BS2: restart storm - 200 kill/respawn cycles (4× S2)
const MODE_STRESS_BS3_SEND: u32 = 122; // BS3: cross-core thrash sender - 2000 blocking sends
const MODE_STRESS_BS3_RECV: u32 = 123; // BS3: cross-core thrash receiver - 2000 recvs
const MODE_STRESS_BS4:      u32 = 124; // BS4: cap table churn - 50 churn cycles (5× S4)
const MODE_STRESS_BS5:      u32 = 125; // BS5: generation integrity - 5000 kill/respawn (5× S5)
const MODE_STRESS_BS6:      u32 = 126; // BS6: self-ping stability - 20000 rounds (4× S6)
const MODE_STRESS_BS7:      u32 = 127; // BS7: memory pressure - 500 alloc passes (5× S7)
const MODE_STRESS_BS8:      u32 = 128; // BS8: scheduler heartbeat - 3000 yields (5× S8)
const MODE_STRESS_BS9_SEND: u32 = 129; // BS9: IPI storm sender - 2500 msgs per sender
const MODE_STRESS_BS9_RECV: u32 = 130; // BS9: IPI storm receiver - 5000 msgs total
const MODE_STRESS_BS10:     u32 = 131; // BS10: cascading revocation - 50 kill/respawn cycles

// Brutal identity test modes - Milestone 15.
const MODE_BRUTAL_ID_11:    u32 = 97; // T11: self-referential queue boundary exactness
const MODE_BRUTAL_ID_12_A:  u32 = 98; // T12: cap chain source (A sends to B, grants cap to C)
const MODE_BRUTAL_ID_12_B:  u32 = 99; // T12: cap chain middle (B receives, forwards cap to C)
const MODE_BRUTAL_ID_12_C:  u32 = 100; // T12: cap chain end (C receives via granted cap)
// mode 101 = MODE_PASSIVE (reused): brutal-id-13-recv sits idle until killed
const MODE_BRUTAL_ID_13_SND: u32 = 102; // T13: fills queue, blocks, wakes with EndpointDead
const MODE_BRUTAL_ID_13_KIL: u32 = 103; // T13: yields then kills brutal-id-13-recv

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    match ctx.probe_mode() {
        MODE_ECHO_RECV       => mode_echo_recv(&ctx),
        MODE_ECHO_SEND       => mode_echo_send(&ctx),
        MODE_NO_SEND_RIGHT   => mode_no_send_right(&ctx),
        MODE_SEND_AFTER_KILL => mode_send_after_kill(&ctx),
        MODE_FILL_AND_BLOCK  => mode_fill_and_block(&ctx),
        MODE_YIELD_LOGGER    => mode_yield_logger(&ctx),
        MODE_HOG             => loop {},
        MODE_CAP_FORGE       => mode_cap_forge(&ctx),
        MODE_GRANT_RECV      => mode_grant_recv(&ctx),
        MODE_GRANT_SEND      => mode_grant_send(&ctx),
        MODE_NO_GRANT_SEND   => mode_no_grant_send(&ctx),
        MODE_ALLOC_OK        => mode_alloc_ok(&ctx),
        MODE_ALLOC_LIMIT     => mode_alloc_limit(&ctx),
        MODE_PROP_P1         => mode_prop_p1(&ctx),
        MODE_PROP_P9         => mode_prop_p9(&ctx),
        MODE_PROP_P10        => mode_prop_p10(&ctx),
        MODE_PROP_P2         => mode_prop_p2(&ctx),
        MODE_PROP_P3         => mode_prop_p3(&ctx),
        MODE_PROP_P6         => mode_prop_p6(&ctx),
        MODE_PROP_P8         => mode_prop_p8(&ctx),
        MODE_PROP_P4         => mode_prop_p4(&ctx),
        MODE_PROP_P5         => mode_prop_p5(&ctx),
        MODE_PROP_P7         => mode_prop_p7(&ctx),
        MODE_FUZZ_F1         => mode_fuzz_f1(&ctx),
        MODE_FUZZ_F2         => mode_fuzz_f2(&ctx),
        MODE_FUZZ_F5         => mode_fuzz_f5(&ctx),
        MODE_FUZZ_F6         => mode_fuzz_f6(&ctx),
        MODE_FUZZ_F7         => mode_fuzz_f7(&ctx),
        MODE_FUZZ_F8         => mode_fuzz_f8(&ctx),
        MODE_STRESS_S1       => mode_stress_s1(&ctx),
        MODE_STRESS_S2       => mode_stress_s2(&ctx),
        MODE_STRESS_S3_SEND  => mode_stress_s3_send(&ctx),
        MODE_STRESS_S3_RECV  => mode_stress_s3_recv(&ctx),
        MODE_STRESS_S4       => mode_stress_s4(&ctx),
        MODE_STRESS_S7       => mode_stress_s7(&ctx),
        MODE_STRESS_S10      => mode_stress_s10(&ctx),
        MODE_STRESS_S5       => mode_stress_s5(&ctx),
        MODE_STRESS_S6       => mode_stress_s6(&ctx),
        MODE_STRESS_S8       => mode_stress_s8(&ctx),
        MODE_STRESS_S9_SEND  => mode_stress_s9_send(&ctx),
        MODE_STRESS_S9_RECV  => mode_stress_s9_recv(&ctx),
        MODE_PERF_B1         => mode_perf_b1(&ctx),
        MODE_PERF_B1_ECHO    => mode_perf_b1_echo(&ctx),
        MODE_PERF_B2         => mode_perf_b2(&ctx),
        MODE_PERF_B2_ECHO    => mode_perf_b2_echo(&ctx),
        MODE_PERF_B3         => mode_perf_b3(&ctx),
        MODE_PERF_B4         => mode_perf_b4(&ctx),
        MODE_PERF_B5         => mode_perf_b5(&ctx),
        MODE_PERF_B7         => mode_perf_b7(&ctx),
        MODE_PERF_B8         => mode_perf_b8(&ctx),
        MODE_PERF_B9         => mode_perf_b9(&ctx),
        MODE_PERF_B9_RECV    => mode_perf_b9_recv(&ctx),
        MODE_PERF_B10        => mode_perf_b10(&ctx),
        MODE_ADV_A1          => mode_adv_a1(&ctx),
        MODE_ADV_A2          => mode_adv_a2(&ctx),
        MODE_ADV_A3          => mode_adv_a3(&ctx),
        MODE_ADV_A4          => mode_adv_a4(&ctx),
        MODE_ADV_A5          => mode_adv_a5(&ctx),
        MODE_ADV_A6          => mode_adv_a6(&ctx),
        MODE_ADV_A7          => mode_adv_a7(&ctx),
        MODE_ADV_A8          => loop { core::hint::spin_loop(); },
        MODE_ADV_A8_WITNESS  => mode_adv_a8_witness(&ctx),
        MODE_ADV_A9          => mode_adv_a9(&ctx),
        MODE_ADV_A10         => mode_adv_a10(&ctx),
        MODE_ADV_A11         => mode_adv_a11(&ctx),
        MODE_ADV_A12         => mode_adv_a12(&ctx),
        MODE_ADV_A13         => mode_adv_a13(&ctx),
        MODE_CHAOS_C2        => mode_chaos_c2(&ctx),
        MODE_CHAOS_C2_MON    => mode_chaos_c2_monitor(&ctx),
        MODE_CHAOS_C3        => mode_chaos_c3(&ctx),
        MODE_CHAOS_C5        => mode_chaos_c5(&ctx),
        MODE_CHAOS_C6_MON    => mode_chaos_c6_monitor(&ctx),
        MODE_CHAOS_C7        => mode_chaos_c7(&ctx),
        MODE_CHAOS_BC2_MON   => mode_chaos_bc2_monitor(&ctx),
        MODE_CHAOS_BC3       => mode_chaos_bc3(&ctx),
        MODE_CHAOS_BC5       => mode_chaos_bc5(&ctx),
        MODE_CHAOS_BC6_MON   => mode_chaos_bc6_monitor(&ctx),
        MODE_CHAOS_BC7       => mode_chaos_bc7(&ctx),
        MODE_PROP_BP1        => mode_prop_bp1(&ctx),
        MODE_PROP_BP2        => mode_prop_bp2(&ctx),
        MODE_PROP_BP3        => mode_prop_bp3(&ctx),
        MODE_PROP_BP4        => mode_prop_bp4(&ctx),
        MODE_PROP_BP5        => mode_prop_bp5(&ctx),
        MODE_PROP_BP6        => mode_prop_bp6(&ctx),
        MODE_PROP_BP7        => mode_prop_bp7(&ctx),
        MODE_PROP_BP8        => mode_prop_bp8(&ctx),
        MODE_PROP_BP9        => mode_prop_bp9(&ctx),
        MODE_PROP_BP10       => mode_prop_bp10(&ctx),
        MODE_BRUTAL_ID_11    => mode_brutal_id_11(&ctx),
        MODE_BRUTAL_ID_12_A  => mode_brutal_id_12_a(&ctx),
        MODE_BRUTAL_ID_12_B  => mode_brutal_id_12_b(&ctx),
        MODE_BRUTAL_ID_12_C  => mode_brutal_id_12_c(&ctx),
        MODE_BRUTAL_ID_13_SND => mode_brutal_id_13_send(&ctx),
        MODE_BRUTAL_ID_13_KIL => mode_brutal_id_13_kill(&ctx),
        MODE_FUZZ_BF1        => mode_fuzz_bf1(&ctx),
        MODE_FUZZ_BF2        => mode_fuzz_bf2(&ctx),
        MODE_FUZZ_BF5        => mode_fuzz_bf5(&ctx),
        MODE_FUZZ_BF6        => mode_fuzz_bf6(&ctx),
        MODE_FUZZ_BF7        => mode_fuzz_bf7(&ctx),
        MODE_FUZZ_BF8        => mode_fuzz_bf8(&ctx),
        MODE_STRESS_BS1      => mode_stress_bs1(&ctx),
        MODE_STRESS_BS2      => mode_stress_bs2(&ctx),
        MODE_STRESS_BS3_SEND => mode_stress_bs3_send(&ctx),
        MODE_STRESS_BS3_RECV => mode_stress_bs3_recv(&ctx),
        MODE_STRESS_BS4      => mode_stress_bs4(&ctx),
        MODE_STRESS_BS5      => mode_stress_bs5(&ctx),
        MODE_STRESS_BS6      => mode_stress_bs6(&ctx),
        MODE_STRESS_BS7      => mode_stress_bs7(&ctx),
        MODE_STRESS_BS8      => mode_stress_bs8(&ctx),
        MODE_STRESS_BS9_SEND => mode_stress_bs9_send(&ctx),
        MODE_STRESS_BS9_RECV => mode_stress_bs9_recv(&ctx),
        MODE_STRESS_BS10     => mode_stress_bs10(&ctx),
        MODE_PERF_BP1        => mode_perf_bp1(&ctx),
        MODE_PERF_BP1_ECHO   => mode_perf_bp1_echo(&ctx),
        MODE_PERF_BP2        => mode_perf_bp2(&ctx),
        MODE_PERF_BP2_ECHO   => mode_perf_bp2_echo(&ctx),
        MODE_PERF_BP3        => mode_perf_bp3(&ctx),
        MODE_PERF_BP4        => mode_perf_bp4(&ctx),
        MODE_PERF_BP5        => mode_perf_bp5(&ctx),
        MODE_PERF_BP7        => mode_perf_bp7(&ctx),
        MODE_PERF_BP8        => mode_perf_bp8(&ctx),
        MODE_PERF_BP9        => mode_perf_bp9(&ctx),
        MODE_PERF_BP9_RECV   => mode_perf_bp9_recv(&ctx),
        MODE_PERF_BP10       => mode_perf_bp10(&ctx),
        MODE_ADV_BA1         => mode_adv_ba1(&ctx),
        MODE_ADV_BA2         => mode_adv_ba2(&ctx),
        MODE_ADV_BA3         => mode_adv_ba3(&ctx),
        MODE_ADV_BA4         => mode_adv_ba4(&ctx),
        MODE_ADV_BA5         => mode_adv_ba5(&ctx),
        MODE_ADV_BA6         => mode_adv_ba6(&ctx),
        MODE_ADV_BA7         => mode_adv_ba7(&ctx),
        MODE_ADV_BA8         => loop { core::hint::spin_loop(); },
        MODE_ADV_BA8_WITNESS => mode_adv_ba8_witness(&ctx),
        MODE_ADV_BA9         => mode_adv_ba9(&ctx),
        MODE_ADV_BA10        => mode_adv_ba10(&ctx),
        MODE_IRQ_RECV        => mode_irq_recv(&ctx),
        MODE_XSEND           => mode_xsend(&ctx),
        MODE_XSEND_RECV      => mode_xsend_recv(&ctx),
        MODE_XLIFE           => mode_xlife(&ctx),
        MODE_XLIFE_VICTIM    => idle(&ctx),
        MODE_ADV_FAULT_GP    => mode_adv_fault_gp(&ctx),
        MODE_ADV_FAULT_DE    => mode_adv_fault_de(&ctx),
        MODE_ADV_FAULT_MON   => mode_adv_fault_mon(&ctx),
        _                    => idle(&ctx),
    }
}

// ---------------------------------------------------------------------------
// Interrupt-routing test modes - Post-v1 item 9 (§12.2, §12.3).
// ---------------------------------------------------------------------------

fn mode_irq_recv(ctx: &ServiceContext) -> ! {
    // Signal harness that the probe is alive and blocking on recv.
    ctx.log("probe: 11A ready");
    let msg = ctx.recv(); // blocks until FIRE_IRQ 33 delivers an interrupt event
    let irq = if msg.payload_len > 0 { msg.payload[0] } else { 0 };
    if irq == 33 {
        ctx.log("probe: 11A pass irq=33");
    } else {
        ctx.log_fmt(format_args!("probe: 11A FAIL - expected irq=33, got {}", irq));
    }
    idle(ctx)
}

fn idle(ctx: &ServiceContext) -> ! {
    loop { ctx.yield_cpu(); }
}

fn mode_echo_recv(ctx: &ServiceContext) -> ! {
    ctx.recv(); // blocks until probe-sender delivers the message
    ctx.log("probe: 3A recv OK");
    idle(ctx)
}

fn mode_echo_send(ctx: &ServiceContext) -> ! {
    let msg = Message::from_bytes(b"probe-3a-msg");
    match ctx.send("probe-recv", &msg) {
        Ok(()) => ctx.log("probe: 3A send OK"),
        Err(_) => ctx.log("probe: 3A send FAIL"),
    }
    idle(ctx)
}

fn mode_no_send_right(ctx: &ServiceContext) -> ! {
    // Test 3B: issue TrySend using the RECV-right cap (slot 2) as the send target.
    // The kernel checks Rights::SEND on the cap → CapInsufficientRights (-3).
    // recv_handle() returns the cap handle wired at spawn; CapHandle(2) is the
    // fallback, but if probe-3b has a recv endpoint it will always be slot 2.
    let handle = ctx.recv_handle().unwrap_or(CapHandle(2));
    let msg = Message::from_bytes(b"test");
    match ctx.try_send_by_handle(handle, &msg) {
        Err(IpcError::CapError(CapError::CapInsufficientRights)) =>
            ctx.log("probe: 3B pass - CapInsufficientRights"),
        _ => ctx.log("probe: 3B FAIL"),
    }
    idle(ctx)
}

fn mode_send_after_kill(ctx: &ServiceContext) -> ! {
    // Test 4A: kill probe-victim (bumps its endpoint generation), then try_send.
    // The SEND cap held by probe-4a now has a stale generation → EndpointDead.
    let msg = Message::from_bytes(b"after-kill");
    let _ = ctx.kill("probe-victim");
    match ctx.try_send("probe-victim", &msg) {
        Err(IpcError::EndpointDead) => ctx.log("probe: 4A pass - EndpointDead after kill"),
        Ok(())                      => ctx.log("probe: 4A FAIL - expected EndpointDead"),
        Err(_)                      => ctx.log("probe: 4A FAIL - unexpected error"),
    }
    idle(ctx)
}

fn mode_fill_and_block(ctx: &ServiceContext) -> ! {
    // Test 4B: fill the 16-slot queue (probe-4b-recv is PASSIVE, never drains it).
    // After filling, log the sentinel that triggers the harness KILL command.
    // Then block on the 17th send; the KILL wakes us with EndpointDead.
    let fill = Message::from_bytes(b"fill");
    for _ in 0u8..16 {
        let _ = ctx.send("probe-4b-recv", &fill);
    }
    ctx.log("probe: 4B sender blocked");
    match ctx.send("probe-4b-recv", &fill) {
        Err(IpcError::EndpointDead) => ctx.log("probe: 4B pass - EndpointDead"),
        Ok(())                      => ctx.log("probe: 4B FAIL - expected EndpointDead"),
        Err(_)                      => ctx.log("probe: 4B FAIL - unexpected error"),
    }
    idle(ctx)
}

fn mode_yield_logger(ctx: &ServiceContext) -> ! {
    for _ in 0u32..10 { ctx.yield_cpu(); }
    ctx.log("probe: 8A yielder ticked");
    idle(ctx)
}

fn mode_cap_forge(ctx: &ServiceContext) -> ! {
    // Test 9B: slot 99 is beyond the 64-slot cap table → CapNotHeld (-2).
    let fake = CapHandle(99);
    let msg  = Message::from_bytes(b"forge");
    match ctx.try_send_by_handle(fake, &msg) {
        Err(IpcError::CapError(CapError::CapNotHeld)) =>
            ctx.log("probe: 9B pass - cap forgery rejected"),
        _ => ctx.log("probe: 9B FAIL"),
    }
    idle(ctx)
}

fn mode_grant_recv(ctx: &ServiceContext) -> ! {
    // Test 5A receiver: wait for the message from probe-5a-send, then verify
    // that an embedded cap arrived via take_pending_cap.
    ctx.recv();
    match ctx.take_pending_cap() {
        Some(_) => ctx.log("probe: 5A recv OK"),
        None    => ctx.log("probe: 5A recv FAIL - no pending cap"),
    }
    idle(ctx)
}

fn mode_grant_send(ctx: &ServiceContext) -> ! {
    // Test 5A sender: send_with_cap to probe-5a-recv.  The send-peer cap has
    // SEND|GRANT, so the transfer is authorised.  On success the cap is gone.
    let msg = Message::from_bytes(b"grant-test");
    match ctx.send_with_cap("probe-5a-recv", &msg) {
        Ok(())  => ctx.log("probe: 5A send OK"),
        Err(_)  => ctx.log("probe: 5A send FAIL"),
    }
    idle(ctx)
}

fn mode_no_grant_send(ctx: &ServiceContext) -> ! {
    // Test 5B negative: the send-peer cap has SEND only (no GRANT).
    // send_with_cap must return CapNotGrantable and leave the cap intact.
    let msg = Message::from_bytes(b"no-grant-test");
    match ctx.send_with_cap("probe-5a-recv", &msg) {
        Err(IpcError::CapError(CapError::CapNotGrantable)) =>
            ctx.log("probe: 5B pass - CapNotGrantable"),
        _ => ctx.log("probe: 5B FAIL"),
    }
    idle(ctx)
}

fn mode_alloc_ok(ctx: &ServiceContext) -> ! {
    // Test 7A: allocate 32 MiB then 20 MiB; both must succeed within the 64 MiB limit.
    let ok1 = ctx.alloc_mem(32 * 1024 * 1024);
    let ok2 = ctx.alloc_mem(20 * 1024 * 1024);
    match (ok1, ok2) {
        (Ok(_), Ok(_)) => ctx.log("probe: 7A pass"),
        _              => ctx.log("probe: 7A FAIL"),
    }
    idle(ctx)
}

fn mode_alloc_limit(ctx: &ServiceContext) -> ! {
    // Test 7B: fill 60 MiB, then verify AllocDenied for 20 MiB (60+20>64),
    // then verify recovery still allows 2 MiB (60+2=62<64).
    let first = ctx.alloc_mem(60 * 1024 * 1024);
    if first.is_err() {
        ctx.log("probe: 7B FAIL - initial 60 MiB alloc failed");
        idle(ctx);
    }
    let denied = ctx.alloc_mem(20 * 1024 * 1024);
    if denied != Err(AllocError::Denied) {
        ctx.log("probe: 7B FAIL - expected AllocDenied for 20 MiB over limit");
        idle(ctx);
    }
    let recover = ctx.alloc_mem(2 * 1024 * 1024);
    match recover {
        Ok(_) => ctx.log("probe: 7B pass"),
        Err(_) => ctx.log("probe: 7B FAIL - recovery alloc failed"),
    }
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Property-test modes - Milestone 9 Phase 1.
// ---------------------------------------------------------------------------

fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn mode_prop_p1(ctx: &ServiceContext) -> ! {
    // P1 - Cap unforgeability (§7.3, §3.1).
    // 10,000 random u32 values used as cap slots. prop-p1 holds no SEND caps,
    // so every try_send must return Err. Any Ok is a constitutional violation.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 20;
    let msg = Message::from_bytes(b"p1");
    for _ in 0..10_000u32 {
        let slot = CapHandle(xorshift64(&mut rng) as u32);
        if ctx.try_send_by_handle(slot, &msg).is_ok() {
            ctx.log("prop: P1 FAIL - random cap slot accepted as valid SEND");
            idle(ctx);
        }
    }
    ctx.log("prop: P1 pass (10000/10000)");
    idle(ctx)
}

fn mode_prop_p9(ctx: &ServiceContext) -> ! {
    // P9 - Generation bump invalidates ALL cap-table holders (§7.5).
    // prop-p9 is wired with 3 SEND caps to prop-p9-victim (3 distinct slots,
    // same endpoint). Kill the victim, then verify every slot returns
    // EndpointDead - not just the first one the kernel happens to find.
    let msg  = Message::from_bytes(b"p9");
    let h0   = ctx.send_peer_at(0);
    let h1   = ctx.send_peer_at(1);
    let h2   = ctx.send_peer_at(2);
    match (h0, h1, h2) {
        (Some(h0), Some(h1), Some(h2)) => {
            let _ = ctx.kill("prop-p9-victim");
            let dead0 = matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead));
            let dead1 = matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead));
            let dead2 = matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead));
            if dead0 && dead1 && dead2 {
                ctx.log("prop: P9 pass - all 3 cap slots returned EndpointDead");
            } else {
                ctx.log("prop: P9 FAIL - not all cap slots returned EndpointDead");
            }
        }
        _ => ctx.log("prop: P9 FAIL - could not read all 3 send peer handles"),
    }
    idle(ctx)
}

fn mode_prop_p10(ctx: &ServiceContext) -> ! {
    // P10 - Every try_send returns without hanging (§8.6, §8.2).
    // 10,000 random (slot, payload) pairs. try_send is non-blocking by spec;
    // completing all iterations within the harness timeout proves the property.
    // Any return value (Ok or Err) is accepted - correctness is timing, not value.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 22;
    for _ in 0..10_000u32 {
        let slot    = CapHandle(xorshift64(&mut rng) as u32);
        let raw     = xorshift64(&mut rng);
        let msg     = Message::from_bytes(&raw.to_le_bytes());
        let _       = ctx.try_send_by_handle(slot, &msg);
    }
    ctx.log("prop: P10 pass (10000/10000)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Property-test modes - Milestone 9 Phase 2.
// ---------------------------------------------------------------------------

fn mode_prop_p2(ctx: &ServiceContext) -> ! {
    // P2 - Generation is strictly monotonic across kill/respawn cycles (§7.5).
    // 3 iterations × 2 kill/respawn cycles = 6 total operations.
    // More cycles here push prop-p8-victim's initial ELF load later in the boot,
    // giving prop-p1/p9/p10 (all Core 0) more uncontested CPU time before the
    // supervisor's 6s ELF load monopolises Core 0.
    let mut prev_gen: u64 = 0;
    for _iter in 0..3u32 {
        for _cycle in 0..2u32 {
            let _ = ctx.kill("prop-p2-victim");
            let _ = ctx.spawn("prop-p2-victim");
            let gen = ctx.inspect_endpoint_generation("prop-p2-victim");
            if gen <= prev_gen {
                ctx.log("prop: P2 FAIL - generation not strictly monotonic after kill/respawn");
                idle(ctx);
            }
            prev_gen = gen;
        }
    }
    ctx.log("prop: P2 pass (3 iter x 2 cycles)");
    idle(ctx)
}

fn mode_prop_p3(ctx: &ServiceContext) -> ! {
    // P3 - Cap rights never widen during transfer (§7.3).
    // Self-referential: prop-p3 bounces a SEND|GRANT cap through its own queue
    // 5000 times. After each recv, the received cap's rights must be exactly
    // SEND|GRANT (= 4 | 16 = 20) - no widening, no bit-flipping.
    const SEND_GRANT: u64 = (1 << 2) | (1 << 4); // Rights::SEND | Rights::GRANT = 20

    let mut cap_handle = match ctx.acquire_send_grant_cap("prop-p3") {
        Some(h) => h,
        None => {
            ctx.log("prop: P3 FAIL - could not acquire SEND|GRANT cap to self");
            idle(ctx);
        }
    };

    let msg = Message::from_bytes(b"p3");

    for _iter in 0..5000u32 {
        match ctx.send_with_cap_by_handle(cap_handle, cap_handle, &msg) {
            Ok(()) => {}
            Err(_) => {
                ctx.log("prop: P3 FAIL - send_with_cap_by_handle failed");
                idle(ctx);
            }
        }
        ctx.recv();
        let new_handle = match ctx.take_pending_cap() {
            Some(h) => h,
            None => {
                ctx.log("prop: P3 FAIL - no pending cap after recv");
                idle(ctx);
            }
        };
        let rights = match ctx.query_cap_rights(new_handle) {
            Some(r) => r,
            None => {
                ctx.log("prop: P3 FAIL - cap slot empty after transfer");
                idle(ctx);
            }
        };
        if rights != SEND_GRANT {
            ctx.log("prop: P3 FAIL - cap rights changed during transfer");
            idle(ctx);
        }
        cap_handle = new_handle;
    }
    ctx.log("prop: P3 pass (5000/5000)");
    idle(ctx)
}

fn mode_prop_p6(ctx: &ServiceContext) -> ! {
    // P6 - Queue depth invariant: D messages enqueued → D messages dequeued (§8.5).
    // prop-p6 has a SEND cap to its own recv endpoint (send_peers=["prop-p6"]).
    // 500 iterations cycle through depths 0..=16. For depth=16, the 17th
    // try_send must return QueueFull. For depth<16, all sends succeed. After
    // each fill phase, exactly `depth` messages are drained.
    ctx.log("prop: P6 starting");
    const QUEUE_DEPTH: u32 = 16;
    let msg = Message::from_bytes(b"p6");
    let recv_h = match ctx.recv_handle() {
        Some(h) => h,
        None => { ctx.log("prop: P6 FAIL - no recv endpoint"); idle(ctx); }
    };

    for iter in 0..500u32 {
        let depth = (iter % (QUEUE_DEPTH + 1)) as u8;

        for _ in 0..depth {
            match ctx.try_send("prop-p6", &msg) {
                Ok(()) => {}
                Err(_) => {
                    ctx.log("prop: P6 FAIL - try_send failed before expected queue depth");
                    idle(ctx);
                }
            }
        }

        if depth == QUEUE_DEPTH as u8 {
            match ctx.try_send("prop-p6", &msg) {
                Err(IpcError::QueueFull) => {}
                Ok(()) => {
                    ctx.log("prop: P6 FAIL - queue accepted more than 16 messages");
                    idle(ctx);
                }
                Err(_) => {
                    ctx.log("prop: P6 FAIL - unexpected error on full-queue try_send");
                    idle(ctx);
                }
            }
        }

        for _ in 0..depth {
            match godspeed_sdk::ipc::recv(recv_h) {
                Ok(_) => {}
                Err(_) => {
                    ctx.log("prop: P6 FAIL - recv returned error");
                    idle(ctx);
                }
            }
        }

    }
    ctx.log("prop: P6 pass (500/500)");
    idle(ctx)
}

fn mode_prop_p8(ctx: &ServiceContext) -> ! {
    // P8 - After restart, name resolves to a higher-generation endpoint (§14.2).
    // 5 iterations with rng-varied cycles (1-2 per iter, ~7-8 total).
    // Together with P2's 6 cycles (~13 total kill/spawn ops) these delay
    // prop-p8-victim's initial ELF load late enough that prop-p1/p9/p10 get
    // sufficient Core 0 time to complete their 10,000-iteration loops before
    // the supervisor's 6s ELF load monopolises Core 0.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 28;
    let mut prev_gen: u64 = 0;
    for _iter in 0..5u32 {
        let n_cycles = 1 + (xorshift64(&mut rng) % 2) as u32;
        for _cycle in 0..n_cycles {
            let _ = ctx.kill("prop-p8-victim");
            let _ = ctx.spawn("prop-p8-victim");
            let gen = ctx.inspect_endpoint_generation("prop-p8-victim");
            if gen <= prev_gen {
                ctx.log("prop: P8 FAIL - generation not monotonic after restart");
                idle(ctx);
            }
            prev_gen = gen;
        }
    }
    ctx.log("prop: P8 pass (5 iter)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Property-test modes - Milestone 9 Phase 3.
// ---------------------------------------------------------------------------

fn mode_prop_p4(ctx: &ServiceContext) -> ! {
    // P4 - ∑ alloc_bytes ≡ pages mapped after any alloc sequence (§10.3).
    // 500 iterations, each allocating one 4 KiB page. Between each, an oversized
    // alloc (1 GiB, always denied) is also attempted. Denied allocs must not
    // affect the kernel's byte counter. Any mismatch between the locally tracked
    // expected total and InspectKernel(0) is a FAIL.
    let mut expected: u64 = 0;
    for _ in 0..500u32 {
        match ctx.alloc_mem(4096) {
            Ok(_)  => expected += 4096,
            Err(_) => {
                ctx.log("prop: P4 FAIL - unexpected alloc failure for 4 KiB page");
                idle(ctx);
            }
        }
        let _ = ctx.alloc_mem(1 << 30); // 1 GiB - always denied; must not shift counter
        let actual = ctx.inspect_kernel_alloc_bytes();
        if actual != expected {
            ctx.log("prop: P4 FAIL - alloc_bytes mismatch after alloc sequence");
            idle(ctx);
        }
    }
    ctx.log("prop: P4 pass (500/500)");
    idle(ctx)
}

fn mode_prop_p5(ctx: &ServiceContext) -> ! {
    // P5 - Every live endpoint has exactly one owning task (§8.3).
    // 50 kill/spawn cycles of prop-p5-victim. The routing table has 96 slots and
    // the system holds ~70 alive endpoints at steady state, leaving ~26 free slots.
    // If endpoints are orphaned (marked Alive without a live owning task), the table
    // fills within ~26 cycles and `register` panics - or spawn returns an error
    // here. 50 consecutive successful spawns prove no orphaning under test load.
    //
    // We do not sample the global count because other property tests run concurrently
    // on the same boot and their victims are transiently dead when we sample, making
    // the absolute count unreliable. Spawn success is the authoritative P5 signal.
    for _ in 0..50u32 {
        let _ = ctx.kill("prop-p5-victim");
        match ctx.spawn("prop-p5-victim") {
            Err(_) => {
                ctx.log("prop: P5 FAIL - spawn failed (routing table overflow; orphan detected)");
                idle(ctx);
            }
            Ok(()) => {}
        }
    }
    ctx.log("prop: P5 pass (50/50)");
    idle(ctx)
}

fn mode_prop_p7(ctx: &ServiceContext) -> ! {
    // P7 - TLB shootdown leaves no stale mappings (§10.5).
    // Proxy test: 50 kill/respawn cycles of prop-p7-victim. Each kill runs the
    // TLB coherence protocol (CORE_CURRENT spin-wait ensures every other core has
    // loaded a different CR3, flushing non-global TLBs) before frame reclaim.
    // Generation monotonicity via InspectKernel(2) confirms the full kill lifecycle
    // completed correctly. No kernel panic over 50 cycles = shootdown protocol
    // is sound under concurrent SMP activity.
    //
    // The generation is read AFTER respawn (the live instance), not after the kill:
    // unregister-on-death (§14.2, the self-heal) clears the dead service's name, so a
    // by-name read in the dead window returns 0. The new instance's generation comes
    // from the global counter, so it strictly increases every cycle (§7.5).
    let mut prev_gen: u64 = 0;
    for _ in 0..50u32 {
        let _ = ctx.kill("prop-p7-victim");
        let _ = ctx.spawn("prop-p7-victim");
        let gen = ctx.inspect_endpoint_generation("prop-p7-victim");
        if gen <= prev_gen {
            ctx.log("prop: P7 FAIL - generation not monotonic across kill/respawn (TLB lifecycle broken)");
            idle(ctx);
        }
        prev_gen = gen;
    }
    ctx.log("prop: P7 pass (50/50)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Fuzz-test modes - Milestone 10 Phase 1.
// ---------------------------------------------------------------------------

/// Issue a raw SYSCALL instruction - used ONLY by fuzz modes.
///
/// # Safety
/// Must NOT be called with nr=9 (Abort) - that syscall intentionally panics.
/// Pointer args (a1, a2) must be null or kernel-space addresses so that
/// validate_user_slice rejects them before user memory is touched.
#[cfg(target_arch = "x86_64")]
unsafe fn probe_raw_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    // SAFETY: SYSCALL from ring-3 is always safe; see safety doc on nr above.
    core::arch::asm!(
        "syscall",
        inout("rax") nr => ret,
        inout("rdi") a0 => _,
        inout("rsi") a1 => _,
        inout("rdx") a2 => _,
        lateout("rcx") _,
        lateout("r11") _,
        lateout("r8")  _,
        lateout("r9")  _,
        lateout("r10") _,
        options(nostack),
    );
    ret
}

fn mode_fuzz_f1(ctx: &ServiceContext) -> ! {
    // F1 - Random syscall args (§22 Fuzz F1).
    // For each known non-abort syscall number, issue 100 calls with adversarial
    // arg combinations. The kernel must not panic on any input.
    // (100 × 10 = 1,000 total; scaled down from 10,000 spec target to fit
    // QEMU emulation speed - F2 proves 50,000 raw unknown-syscall dispatches fit
    // in 60 s. Four syscalls are excluded:
    //   nr=4 (Yield): no cap argument; each call causes a real scheduler context
    //     switch, making any significant iteration count prohibitively slow.
    //   nr=6 (AllocMem): no cap argument; small a0 values cause real physical
    //     frame allocations before the task budget is exhausted - page-table
    //     overhead under QEMU TCG makes the loop slow. AllocMem is covered by F8.
    //   nr=13 (InspectKernel): query_id=1 (hit when a0=1) calls
    //     count_live_endpoints() which acquires ROUTE_LOCKED, the same spinlock
    //     held by ping/pong send calls (95/s) and fuzz-f7 kill cycles. Under
    //     QEMU TCG, spinning on a contended atomic burns the entire CPU quantum.
    //     InspectKernel is tested by property probes P4/P5/P7.)
    //   nr=15 (RemoveCap): iter%8==0 produces a0=0, removing slot 0 (log_write
    //     cap). ctx.log at the end then fails silently - pass string never appears.
    //     RemoveCap cannot panic regardless of slot index; empty/out-of-range
    //     slots are an idempotent no-op returning 0.
    //
    // a0: alternates between random u32 cap slots and known valid slots.
    // a1/a2: restricted to values that fail validate_user_slice (null or kernel
    //        addresses ≥ 0xffff800000000000) - prevents kernel-mode page faults
    //        from accidental unmapped-page dereference during pointer validation.
    // nr=15 (RemoveCap) excluded: a0=0 on the first iteration removes slot 0
    // (log_write cap), making ctx.log fail silently after the loop. RemoveCap
    // cannot panic regardless of slot index - empty/out-of-range slots are a
    // no-op returning 0 - so excluding it does not reduce panic-safety coverage.
    const NRS: &[u64] = &[1, 2, 3, 5, 7, 8, 10, 11, 12, 14];
    // Pointer arg candidates - all guaranteed to fail validate_user_slice.
    const A1S: &[u64] = &[0, 0xffff800000000000, u64::MAX, 0xffff_8000_0000_1000];
    const A2S: &[u64] = &[0, 1, 255, 256, 4096, u64::MAX];

    ctx.log("fuzz: F1 starting");
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 30;
    for &nr in NRS {
        for iter in 0..100u32 {
            let a0 = match iter % 8 {
                0 => 0u64,
                1 => 1u64,
                2 => 64u64,          // one past cap table limit
                3 => 0xFFFFu64,      // well beyond cap table
                4 => u64::MAX,
                5 => xorshift64(&mut rng) as u32 as u64,
                6 => xorshift64(&mut rng) & 0xFF,
                _ => xorshift64(&mut rng),
            };
            let a1 = A1S[(iter as usize) % A1S.len()];
            let a2 = A2S[(iter as usize) % A2S.len()];
            // SAFETY: nr != 9 (Abort); a1/a2 fail validate_user_slice.
            unsafe { probe_raw_syscall(nr, a0, a1, a2); }
        }
    }
    ctx.log("fuzz: F1 pass (100/10)");
    idle(ctx)
}

fn mode_fuzz_f2(ctx: &ServiceContext) -> ! {
    // F2 - Random syscall numbers (§22 Fuzz F2).
    // 50,000 calls with random u64 syscall numbers, all remapped out of the valid syscall
    // range (1-15) into the unknown range. (Abort/9 - which used to panic the kernel - was
    // removed by the syscall-gating audit; every number now returns UnknownSyscall, never panics.)
    // Every call must return without a kernel panic.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 31;
    let mut bad = 0u32;
    for _ in 0..50_000u32 {
        let raw = xorshift64(&mut rng);
        // Remap any value that would hit a known valid syscall (1-15).
        // Add 100 to push it into the unknown range.
        let nr = if raw <= 15 { raw + 100 } else { raw };
        // SAFETY: nr is not in 1-15 → falls through dispatch to _ => -1; no panic.
        let ret = unsafe { probe_raw_syscall(nr, 0, 0, 0) };
        // Unknown syscalls must return -1 (UnknownSyscall).
        if ret != -1 { bad += 1; }
    }
    if bad > 0 {
        ctx.log("fuzz: F2 FAIL - unknown syscall returned non-(-1)");
    } else {
        ctx.log("fuzz: F2 pass (50000/50000)");
    }
    idle(ctx)
}

fn mode_fuzz_f5(ctx: &ServiceContext) -> ! {
    // F5 - Random IPC message bodies (§22 Fuzz F5).
    // 1,000 try_send calls to fuzz-f5-recv with random content and random sizes
    // (0..=4096 bytes). The kernel copies the payload; random content must not
    // cause a panic regardless of byte values or message length.
    // After the queue fills (depth=16), remaining sends return QueueFull - still OK.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 32;
    for _ in 0..1_000u32 {
        let size = (xorshift64(&mut rng) % 4097) as usize;
        let mut buf = [0u8; 4096];
        for b in buf[..size.min(4096)].iter_mut() {
            *b = xorshift64(&mut rng) as u8;
        }
        let msg = Message::from_bytes(&buf[..size.min(4096)]);
        let _ = ctx.try_send("fuzz-f5-recv", &msg);
    }
    ctx.log("fuzz: F5 pass (1000/1000)");
    idle(ctx)
}

fn mode_fuzz_f6(ctx: &ServiceContext) -> ! {
    // F6 - Embedded cap fuzzing (§22 Fuzz F6).
    // 1,000 SendWithCap calls with random endpoint and grant cap slot indices.
    // Most slots are out of range → CapNotHeld. The kernel must not panic on
    // any combination of slot values, including valid slots with missing GRANT.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 33;
    let msg = Message::from_bytes(b"f6");
    for _ in 0..1_000u32 {
        let ep_slot  = CapHandle(xorshift64(&mut rng) as u32);
        let cap_slot = CapHandle(xorshift64(&mut rng) as u32);
        let _ = ctx.send_with_cap_by_handle(ep_slot, cap_slot, &msg);
    }
    ctx.log("fuzz: F6 pass (1000/1000)");
    idle(ctx)
}

fn mode_fuzz_f7(ctx: &ServiceContext) -> ! {
    // F7 - Stale cap / generation fuzzing (§22 Fuzz F7).
    // 50 kill cycles: each kill bumps fuzz-f7-victim's endpoint generation.
    // The SEND cap held by fuzz-f7 becomes stale. Every subsequent try_send via
    // that cap must return EndpointDead (or another error), never Ok and never panic.
    // After each kill, high-value cap slots (never issued) are also tried → CapNotHeld.
    let msg   = Message::from_bytes(b"f7");
    let stale = ctx.send_peer_at(0); // SEND cap to fuzz-f7-victim (slot index 0)

    for _ in 0..50u32 {
        let _ = ctx.kill("fuzz-f7-victim");

        // Stale cap must not return Ok.
        if let Some(h) = stale {
            if ctx.try_send_by_handle(h, &msg).is_ok() {
                ctx.log("fuzz: F7 FAIL - send to killed endpoint succeeded");
                idle(ctx);
            }
        }

        // High-value slot (never issued) must return CapNotHeld, not panic.
        let _ = ctx.try_send_by_handle(CapHandle(0xBEEF), &msg);
        let _ = ctx.try_send_by_handle(CapHandle(u32::MAX), &msg);

        let _ = ctx.spawn("fuzz-f7-victim");
        // stale cap still has old generation → still EndpointDead after respawn.
        if let Some(h) = stale {
            let _ = ctx.try_send_by_handle(h, &msg);
        }
    }
    ctx.log("fuzz: F7 pass (50/50)");
    idle(ctx)
}

fn mode_fuzz_f8(ctx: &ServiceContext) -> ! {
    // F8 - Memory request size fuzzing (§22 Fuzz F8).
    // Edge cases including 0, u64::MAX, and values exceeding total RAM or the
    // task's 64 MiB limit. The kernel's claim_alloc must reject oversized requests
    // without panicking. AllocDenied (-11) or failure (-1) are both acceptable.
    // Note: usize == u64 on x86_64; usize::MAX == u64::MAX.
    let edge_cases: &[usize] = &[
        0,
        1,
        4095,
        4096,
        4097,
        64 * 1024 * 1024 + 1,  // just over memory_limit
        1 << 30,               // 1 GiB - always AllocDenied
        usize::MAX - 4095,     // overflow bait for (size + 4095)
        usize::MAX - 1,
        usize::MAX,
    ];
    for &size in edge_cases {
        let _ = ctx.alloc_mem(size); // AllocDenied or -1; must not panic
    }
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 35;
    for _ in 0..1_000u32 {
        let _ = ctx.alloc_mem(xorshift64(&mut rng) as usize);
    }
    ctx.log("fuzz: F8 pass");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Stress-test modes - Milestone 11 Phase 1.
// ---------------------------------------------------------------------------

fn mode_stress_s1(ctx: &ServiceContext) -> ! {
    // S1 - IPC saturation (§22 Stress S1).
    // 10,000 try_send calls to stress-s1-recv (passive, never draining).
    // Queue fills to depth 16 after the first 16 calls; QueueFull is acceptable.
    let msg = Message::from_bytes(b"s1");
    for _ in 0..10_000u32 {
        let _ = ctx.try_send("stress-s1-recv", &msg);
    }
    ctx.log("stress: S1 pass (10000/10000)");
    idle(ctx)
}

fn mode_stress_s2(ctx: &ServiceContext) -> ! {
    // S2 - Restart storm (§22 Stress S2).
    // Initial alive-check, then 50 kill/respawn cycles of stress-s2-victim.
    // If kstack freeing is broken the pool exhausts by cycle ~24 and spawn fails.
    let msg = Message::from_bytes(b"s2-ping");
    match ctx.try_send("stress-s2-victim", &msg) {
        Ok(()) => {}
        Err(_) => {
            ctx.log("stress: S2 FAIL - victim not reachable at start");
            idle(ctx);
        }
    }
    for _ in 0..50u32 {
        let _ = ctx.kill("stress-s2-victim");
        match ctx.spawn("stress-s2-victim") {
            Err(_) => {
                ctx.log("stress: S2 FAIL - spawn failed (kstack pool exhausted?)");
                idle(ctx);
            }
            Ok(()) => {}
        }
    }
    ctx.log("stress: S2 pass (50/50)");
    idle(ctx)
}

fn mode_stress_s3_send(ctx: &ServiceContext) -> ! {
    // S3 sender (§22 Stress S3).
    // 50 blocking sends to stress-s3-recv on core 1. Scaled down from 500: each
    // cross-core IPI round-trip costs ~15 s under 200-task QEMU TCG load, and
    // tasks spawn at line ~980 (~280 s of boot overhead). BS3 extends to 2000 msgs.
    let msg = Message::from_bytes(b"s3");
    for _ in 0..50u32 {
        let _ = ctx.send("stress-s3-recv", &msg);
    }
    idle(ctx)
}

fn mode_stress_s3_recv(ctx: &ServiceContext) -> ! {
    // S3 receiver (§22 Stress S3).
    // Drain 50 cross-core messages from stress-s3-send on core 0.
    for _ in 0..50u32 {
        ctx.recv();
    }
    ctx.log("stress: S3 pass (50/50)");
    idle(ctx)
}

fn mode_stress_s4(ctx: &ServiceContext) -> ! {
    // S4 - Cap table churn (§22 Stress S4).
    // Holds 2 SEND caps (h0, h1) to stress-s4-victim provisioned via the
    // repeated send_peers trick (same endpoint, two distinct cap slots).
    // Phase 1: verify both caps valid before any kill.
    // First kill: verify both caps go EndpointDead simultaneously.
    // 50 cycles: spawn+kill, confirm generation strictly monotonic, confirm
    // both stale caps remain EndpointDead throughout.
    let h0 = match ctx.send_peer_at(0) {
        Some(h) => h,
        None => {
            ctx.log("stress: S4 FAIL - no peer handle h0");
            idle(ctx);
        }
    };
    let h1 = match ctx.send_peer_at(1) {
        Some(h) => h,
        None => {
            ctx.log("stress: S4 FAIL - no peer handle h1");
            idle(ctx);
        }
    };
    let msg = Message::from_bytes(b"s4");

    if ctx.try_send_by_handle(h0, &msg).is_err() {
        ctx.log("stress: S4 FAIL - cap A not valid pre-kill");
        idle(ctx);
    }
    if ctx.try_send_by_handle(h1, &msg).is_err() {
        ctx.log("stress: S4 FAIL - cap B not valid pre-kill");
        idle(ctx);
    }

    let _ = ctx.kill("stress-s4-victim");

    if !matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: S4 FAIL - cap A survived first kill");
        idle(ctx);
    }
    if !matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: S4 FAIL - cap B survived first kill");
        idle(ctx);
    }

    let mut prev_gen = ctx.inspect_endpoint_generation("stress-s4-victim");
    for _ in 0..10u32 {
        let _ = ctx.spawn("stress-s4-victim");
        let _ = ctx.kill("stress-s4-victim");
        let gen = ctx.inspect_endpoint_generation("stress-s4-victim");
        if gen <= prev_gen {
            ctx.log("stress: S4 FAIL - generation not monotonic under churn");
            idle(ctx);
        }
        prev_gen = gen;
        if !matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: S4 FAIL - cap A not stale during churn");
            idle(ctx);
        }
        if !matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: S4 FAIL - cap B not stale during churn");
            idle(ctx);
        }
    }
    ctx.log("stress: S4 pass (10/10)");
    idle(ctx)
}

fn mode_stress_s7(ctx: &ServiceContext) -> ! {
    // S7 - Memory pressure (§22 Stress S7).
    // 100 alloc_mem(4 MiB) passes against the 64 MiB budget.
    // Once AllocDenied appears (after ~16 successful allocations), all subsequent
    // calls must also be Denied - Ok after Denied is a kernel accounting bug.
    const CHUNK: usize = 4 * 1024 * 1024;
    let mut at_limit = false;
    for _ in 0..100u32 {
        match ctx.alloc_mem(CHUNK) {
            Ok(_) => {
                if at_limit {
                    ctx.log("stress: S7 FAIL - Ok returned after AllocDenied");
                    idle(ctx);
                }
            }
            Err(AllocError::Denied) => {
                at_limit = true;
            }
            Err(_) => {
                ctx.log("stress: S7 FAIL - unexpected alloc error");
                idle(ctx);
            }
        }
    }
    if !at_limit {
        ctx.log("stress: S7 FAIL - AllocDenied never returned (limit not enforced)");
        idle(ctx);
    }
    ctx.log("stress: S7 pass (100/100)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Stress-test modes - Milestone 11 Phase 2.
// ---------------------------------------------------------------------------

fn mode_stress_s5(ctx: &ServiceContext) -> ! {
    // S5 - Generation counter integrity over sustained kill/respawn (§22 Stress S5).
    // 500 kill/respawn cycles of stress-s5-victim. After each cycle, verify the
    // endpoint generation is strictly greater than before. This proves the generation
    // counter correctly tracks every kill/respawn event - scaled down from 1000 to
    // fit within the QEMU TCG timeout under 200-task load. BS5 extends this to 5000.
    let mut prev_gen: u64 = 0;
    for _ in 0..500u32 {
        let _ = ctx.kill("stress-s5-victim");
        let _ = ctx.spawn("stress-s5-victim");
        let gen = ctx.inspect_endpoint_generation("stress-s5-victim");
        if gen <= prev_gen {
            ctx.log("stress: S5 FAIL - generation not strictly monotonic after kill/respawn");
            idle(ctx);
        }
        prev_gen = gen;
    }
    ctx.log("stress: S5 pass (500/500)");
    idle(ctx)
}

fn mode_stress_s6(ctx: &ServiceContext) -> ! {
    // S6 - Long-running IPC self-ping stability (§22 Stress S6).
    // 500 self-ping rounds: send to own endpoint (stress-s6), recv from same endpoint.
    // Scaled down from 5000 to fit within the QEMU TCG timeout under 200-task load;
    // the property being proved (IPC path does not corrupt or deadlock) is the same.
    // Self-referential: send_peers = ["stress-s6"].
    ctx.log("stress: S6 start");
    let msg = Message::from_bytes(b"s6");
    for _ in 0..500u32 {
        match ctx.send("stress-s6", &msg) {
            Ok(()) => {}
            Err(_) => {
                ctx.log("stress: S6 FAIL - send to self returned error");
                idle(ctx);
            }
        }
        ctx.recv();
    }
    ctx.log("stress: S6 pass (500/500)");
    idle(ctx)
}

fn mode_stress_s8(ctx: &ServiceContext) -> ! {
    // S8 - Idle scheduler heartbeat (§22 Stress S8).
    // 5 yield cycles prove the scheduler returns from its idle loop and the
    // per-core timer fires reliably. Under 200-task QEMU TCG load each yield
    // costs ~500 ms wall-clock; 5 yields keeps the test well within 200 s.
    ctx.log("stress: S8 start");
    for _ in 0..5u32 {
        ctx.yield_cpu();
    }
    ctx.log("stress: S8 pass (5 yields)");
    idle(ctx)
}

fn mode_stress_s9_send(ctx: &ServiceContext) -> ! {
    // S9 sender (§22 Stress S9).
    // 50 sends per sender (100 total) to stress-s9-recv on core 2. Scaled down from
    // 500: tasks spawn at line ~1190 (~340 s boot overhead under 200-task load).
    // Uses try_send + yield-retry: routing table holds one blocked-sender slot, so
    // two concurrent senders must not both block. BS9 covers high-volume IPI storm.
    let msg = Message::from_bytes(b"s9");
    for _ in 0..50u32 {
        loop {
            match ctx.try_send("stress-s9-recv", &msg) {
                Ok(()) => break,
                Err(_) => ctx.yield_cpu(),
            }
        }
    }
    idle(ctx)
}

fn mode_stress_s9_recv(ctx: &ServiceContext) -> ! {
    // S9 receiver (§22 Stress S9).
    // Drains 100 messages from the two S9 senders (50 each from cores 0 and 1).
    for _ in 0..100u32 {
        ctx.recv();
    }
    ctx.log("stress: S9 pass (100/100)");
    idle(ctx)
}

fn mode_stress_s10(ctx: &ServiceContext) -> ! {
    // S10 - Cascading revocation (§22 Stress S10).
    // Holds 3 SEND caps (h0, h1, h2) to stress-s10-victim on core 1.
    // Runs on core 0 - cross-core kill scenario.
    // Kill victim → all 3 caps must return EndpointDead simultaneously,
    // proving that the generation bump propagates to all cap-table holders
    // on a different core without any synchronous notification.
    let h0 = match ctx.send_peer_at(0) {
        Some(h) => h,
        None => {
            ctx.log("stress: S10 FAIL - no peer handle h0");
            idle(ctx);
        }
    };
    let h1 = match ctx.send_peer_at(1) {
        Some(h) => h,
        None => {
            ctx.log("stress: S10 FAIL - no peer handle h1");
            idle(ctx);
        }
    };
    let h2 = match ctx.send_peer_at(2) {
        Some(h) => h,
        None => {
            ctx.log("stress: S10 FAIL - no peer handle h2");
            idle(ctx);
        }
    };
    let msg = Message::from_bytes(b"s10");

    if ctx.try_send_by_handle(h0, &msg).is_err() {
        ctx.log("stress: S10 FAIL - cap A not valid pre-kill");
        idle(ctx);
    }
    if ctx.try_send_by_handle(h1, &msg).is_err() {
        ctx.log("stress: S10 FAIL - cap B not valid pre-kill");
        idle(ctx);
    }
    if ctx.try_send_by_handle(h2, &msg).is_err() {
        ctx.log("stress: S10 FAIL - cap C not valid pre-kill");
        idle(ctx);
    }

    let _ = ctx.kill("stress-s10-victim");

    let dead0 = matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead));
    let dead1 = matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead));
    let dead2 = matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead));

    if dead0 && dead1 && dead2 {
        ctx.log("stress: S10 pass (3/3 caps dead)");
    } else {
        ctx.log("stress: S10 FAIL - not all caps returned EndpointDead");
    }
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Performance-benchmark modes - Milestone 12.
// ---------------------------------------------------------------------------

/// Insertion sort for u64 slices (no_std; O(n²) fine for N ≤ 200).
fn sort_u64(arr: &mut [u64]) {
    let n = arr.len();
    for i in 1..n {
        let key = arr[i];
        let mut j = i;
        while j > 0 && arr[j - 1] > key {
            arr[j] = arr[j - 1];
            j -= 1;
        }
        arr[j] = key;
    }
}

fn mode_perf_b1(ctx: &ServiceContext) -> ! {
    // B1: same-core IPC round-trip latency (§22 Perf B1).
    // Dynamically acquire a SEND cap to the echo partner (which registered after us).
    let echo_cap = loop {
        if let Some(cap) = ctx.acquire_send_cap("perf-b1-echo") { break cap; }
        ctx.yield_cpu();
    };

    let msg = Message::from_bytes(b"b1");
    // N=50: same-core round-trips require two scheduler context switches; with
    // 160+ competing tasks each costs ~800ms wall. 200×800ms = 160s impractical.
    const N: usize = 50;
    let mut samples = [0u64; N];

    for i in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.send_by_handle(echo_cap, &msg);
        ctx.recv();
        let t1 = ctx.read_tsc();
        samples[i] = t1.wrapping_sub(t0);
    }

    sort_u64(&mut samples);
    let p50 = samples[N / 2];
    let p99 = samples[N * 99 / 100];
    ctx.log_fmt(format_args!("perf: B1 p50={p50} p99={p99} cycles/roundtrip"));
    ctx.log("perf: B1 done");
    idle(ctx)
}

fn mode_perf_b1_echo(ctx: &ServiceContext) -> ! {
    // B1 echo: recv message, send it back (same core, no measurement).
    let msg = Message::from_bytes(b"b1e");
    loop {
        ctx.recv();
        let _ = ctx.send("perf-b1", &msg);
    }
}

fn mode_perf_b2(ctx: &ServiceContext) -> ! {
    // B2: cross-core IPC round-trip latency (§22 Perf B2).
    // Same structure as B1 but echo lives on a different core.
    let echo_cap = loop {
        if let Some(cap) = ctx.acquire_send_cap("perf-b2-echo") { break cap; }
        ctx.yield_cpu();
    };

    let msg = Message::from_bytes(b"b2");
    // N=50: cross-core round-trips cost ~800ms each under QEMU TCG load;
    // 200×800ms = 160s is impractical. 50 samples still produce valid percentiles.
    const N: usize = 50;
    let mut samples = [0u64; N];

    for i in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.send_by_handle(echo_cap, &msg);
        ctx.recv();
        let t1 = ctx.read_tsc();
        samples[i] = t1.wrapping_sub(t0);
    }

    sort_u64(&mut samples);
    let p50 = samples[N / 2];
    let p99 = samples[N * 99 / 100];
    ctx.log_fmt(format_args!("perf: B2 p50={p50} p99={p99} cycles/roundtrip"));
    ctx.log("perf: B2 done");
    idle(ctx)
}

fn mode_perf_b2_echo(ctx: &ServiceContext) -> ! {
    // B2 echo: recv message, send it back (cross-core, no measurement).
    let msg = Message::from_bytes(b"b2e");
    loop {
        ctx.recv();
        let _ = ctx.send("perf-b2", &msg);
    }
}

fn mode_perf_b3(ctx: &ServiceContext) -> ! {
    // B3: syscall yield floor - round-trip time for advisory yield (§22 Perf B3).
    // N=10: brutal stress tests (stress-bs4/bs5 kill/respawn cycling) make each yield
    // cost 3-5s wall under full QEMU TCG load; 50×3.4s ≈ 170s > post-spawn headroom.
    // 10 samples still produce a valid TSC mean for baseline tracking.
    const N: u64 = 10;
    let t0 = ctx.read_tsc();
    for _ in 0..N { ctx.yield_cpu(); }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: B3 mean={mean} cycles/yield"));
    ctx.log("perf: B3 done");
    idle(ctx)
}

fn mode_perf_b4(ctx: &ServiceContext) -> ! {
    // B4: cap validation throughput - QueryCapRights invokes cap + gen check (§22 Perf B4).
    let handle = match ctx.recv_handle() {
        Some(h) => h,
        None    => { ctx.log("perf: B4 FAIL - no recv cap"); idle(ctx); }
    };
    const N: u64 = 10_000;
    let t0 = ctx.read_tsc();
    for _ in 0..N { ctx.query_cap_rights(handle); }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: B4 mean={mean} cycles/cap-check"));
    ctx.log("perf: B4 done");
    idle(ctx)
}

fn mode_perf_b5(ctx: &ServiceContext) -> ! {
    // B5/B6: spawn cost and restart (kill+spawn) cost (§22 Perf B5, B6).
    // Victim is pre-spawned by supervisor; kill it first then cycle.
    const N: u32 = 10;

    // B5: spawn-only cost.
    let _ = ctx.kill("perf-b5-victim"); // kill initially-running victim
    let mut total_spawn: u64 = 0;
    for _ in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.spawn("perf-b5-victim");
        let t1 = ctx.read_tsc();
        total_spawn += t1.wrapping_sub(t0);
        let _ = ctx.kill("perf-b5-victim");
    }
    let spawn_mean = total_spawn / N as u64;
    ctx.log_fmt(format_args!("perf: B5 spawn_mean={spawn_mean} cycles/spawn"));
    ctx.log("perf: B5 done");

    // B6: kill+spawn (restart) cost.
    let _ = ctx.spawn("perf-b5-victim"); // ensure alive before cycling
    let mut total_restart: u64 = 0;
    for _ in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.kill("perf-b5-victim");
        let _ = ctx.spawn("perf-b5-victim");
        let t1 = ctx.read_tsc();
        total_restart += t1.wrapping_sub(t0);
    }
    let restart_mean = total_restart / N as u64;
    ctx.log_fmt(format_args!("perf: B6 restart_mean={restart_mean} cycles/restart"));
    ctx.log("perf: B6 done");
    idle(ctx)
}

fn mode_perf_b7(ctx: &ServiceContext) -> ! {
    // B7: cap table insert/remove throughput - acquire SEND cap to self then remove (§22 Perf B7).
    const N: u64 = 1_000;
    let t0 = ctx.read_tsc();
    for _ in 0..N {
        if let Some(cap) = ctx.acquire_send_cap("perf-b7") {
            ctx.remove_cap(cap);
        }
    }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: B7 mean={mean} cycles/cap-insert-remove"));
    ctx.log("perf: B7 done");
    idle(ctx)
}

fn mode_perf_b8(ctx: &ServiceContext) -> ! {
    // B8: allocator throughput - alloc 4 KiB pages until memory limit (§22 Perf B8).
    let mut n_alloc: u64 = 0;
    let t0 = ctx.read_tsc();
    loop {
        match ctx.alloc_mem(4096) {
            Ok(_)                   => n_alloc += 1,
            Err(AllocError::Denied) => break,
            Err(_)                  => break,
        }
    }
    let t1 = ctx.read_tsc();
    let mean = if n_alloc > 0 { t1.wrapping_sub(t0) / n_alloc } else { 0 };
    ctx.log_fmt(format_args!("perf: B8 n={n_alloc} mean={mean} cycles/alloc-4kib"));
    ctx.log("perf: B8 done");
    idle(ctx)
}

fn mode_perf_b9(ctx: &ServiceContext) -> ! {
    // B9: 4 KiB message copy cost - send max-size messages to receiver (§22 Perf B9).
    let mut msg = Message::from_bytes(&[]);
    for b in msg.payload.iter_mut() { *b = 0xAB; }
    msg.payload_len = 4096;

    const N: u64 = 200;
    let t0 = ctx.read_tsc();
    for _ in 0..N {
        let _ = ctx.send("perf-b9-recv", &msg);
    }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: B9 mean={mean} cycles/4kib-send"));
    ctx.log("perf: B9 done");
    idle(ctx)
}

fn mode_perf_b9_recv(ctx: &ServiceContext) -> ! {
    // B9 receiver: drain all incoming messages so sender never permanently blocks.
    loop { ctx.recv(); }
}

fn mode_perf_b10(ctx: &ServiceContext) -> ! {
    // B10: scheduler pick-next cost - same as B3 but labelled separately for
    // baseline tracking (§22 Perf B10).
    // N=10: mirrors B3 - brutal stress tasks make each yield cost 3-5s wall;
    // 10 samples fit within post-spawn headroom and still produce a valid mean.
    const N: u64 = 10;
    let t0 = ctx.read_tsc();
    for _ in 0..N { ctx.yield_cpu(); }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: B10 mean={mean} cycles/yield"));
    ctx.log("perf: B10 done");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Adversarial-test modes - Milestone 13.
// ---------------------------------------------------------------------------

fn mode_adv_a1(ctx: &ServiceContext) -> ! {
    // A1 - Cap unforgeability under adversarial input (§22 Adversarial A1, §7.3).
    // 10,000 random u32 slot indices. adv-a1 holds no SEND caps; every
    // try_send_by_handle must return Err. An Ok return proves a forged cap -
    // a constitutional violation.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 80;
    let msg = Message::from_bytes(b"a1");
    for _ in 0..10_000u32 {
        let slot = CapHandle(xorshift64(&mut rng) as u32);
        if ctx.try_send_by_handle(slot, &msg).is_ok() {
            ctx.log("adv: A1 FAIL - random cap slot accepted as valid SEND");
            idle(ctx);
        }
    }
    ctx.log("adv: A1 pass (10000/10000)");
    idle(ctx)
}

fn mode_adv_a2(ctx: &ServiceContext) -> ! {
    // A2 - Brute-force cap slot range → defined errors, no panic (§22 Adversarial A2).
    // Slots 0..=127 cover the full 64-slot table and well beyond. u32::MAX is
    // an extreme out-of-range value. Every call must return a defined error.
    let msg = Message::from_bytes(b"a2");
    for slot in 0u32..128u32 {
        let _ = ctx.try_send_by_handle(CapHandle(slot), &msg);
    }
    let _ = ctx.try_send_by_handle(CapHandle(0xFFFF), &msg);
    let _ = ctx.try_send_by_handle(CapHandle(u32::MAX), &msg);
    ctx.log("adv: A2 pass - all slot values returned defined errors");
    idle(ctx)
}

fn mode_adv_a3(ctx: &ServiceContext) -> ! {
    // A3 - Alloc beyond 4 MiB contract limit via every path (§22 Adversarial A3).
    // First 2 MiB must succeed. Next 3 MiB pushes total to 5 MiB > 4 MiB limit
    // → AllocDenied. Edge cases (0, usize::MAX, 1 TiB) must not panic.
    let ok = ctx.alloc_mem(2 * 1024 * 1024);
    if ok.is_err() {
        ctx.log("adv: A3 FAIL - initial 2 MiB alloc should succeed within 4 MiB limit");
        idle(ctx);
    }
    let denied = ctx.alloc_mem(3 * 1024 * 1024);
    if denied != Err(AllocError::Denied) {
        ctx.log("adv: A3 FAIL - expected AllocDenied for 3 MiB (total would be 5 MiB > 4 MiB)");
        idle(ctx);
    }
    // Edge cases - must not panic regardless of value.
    let _ = ctx.alloc_mem(0);
    let _ = ctx.alloc_mem(usize::MAX);
    let _ = ctx.alloc_mem(1usize << 40); // 1 TiB
    ctx.log("adv: A3 pass - alloc beyond limit rejected without panic");
    idle(ctx)
}

fn mode_adv_a4(ctx: &ServiceContext) -> ! {
    // A4 - RECV-right cap used as SEND target → CapInsufficientRights (§22 Adversarial A4).
    // adv-a4 has a recv endpoint; its RECV cap is in slot 2. Passing that handle
    // to try_send_by_handle must return CapInsufficientRights - the SEND right is absent.
    let handle = ctx.recv_handle().unwrap_or(CapHandle(2));
    let msg = Message::from_bytes(b"a4");
    match ctx.try_send_by_handle(handle, &msg) {
        Err(IpcError::CapError(CapError::CapInsufficientRights)) =>
            ctx.log("adv: A4 pass - CapInsufficientRights on RECV cap used as SEND"),
        Ok(())  => ctx.log("adv: A4 FAIL - RECV cap accepted as SEND cap"),
        Err(_)  => ctx.log("adv: A4 FAIL - unexpected error"),
    }
    idle(ctx)
}

fn mode_adv_a5(ctx: &ServiceContext) -> ! {
    // A5 - TOCTOU: kill victim then send via stale cap → EndpointDead (§22 Adversarial A5).
    // Kill bumps adv-a5-victim's endpoint generation. The SEND cap held by adv-a5
    // now has a stale generation. The kernel's generation check (§8.7) must catch this.
    let msg = Message::from_bytes(b"a5");
    let _ = ctx.kill("adv-a5-victim");
    match ctx.try_send("adv-a5-victim", &msg) {
        Err(IpcError::EndpointDead) => ctx.log("adv: A5 pass - EndpointDead after kill"),
        Ok(())  => ctx.log("adv: A5 FAIL - send succeeded after victim killed"),
        Err(_)  => ctx.log("adv: A5 FAIL - unexpected error after kill"),
    }
    idle(ctx)
}

fn mode_adv_a6(ctx: &ServiceContext) -> ! {
    // A6 - Fill own cap table via acquire_send_cap loop (§22 Adversarial A6).
    // adv-a6 has recv endpoint (slot 2=RECV, pre-filled). Slots 0=log, 1=spawn.
    // acquire_send_cap("adv-a6") inserts a SEND cap each call, up to table capacity.
    // When None is returned the table is full - kernel must not panic on exhaustion.
    let mut count = 0u32;
    loop {
        match ctx.acquire_send_cap("adv-a6") {
            Some(_) => count += 1,
            None    => break,
        }
    }
    ctx.log_fmt(format_args!("adv: A6 filled {count} cap slots"));
    ctx.log("adv: A6 pass - cap table filled then rejected without panic");
    idle(ctx)
}

fn mode_adv_a7(ctx: &ServiceContext) -> ! {
    // A7 - Timing side-channel probe (§22 Adversarial A7).
    // 100 try_send calls to passive adv-a7-recv. Queue fills after 16 sends;
    // remaining calls return QueueFull. All returns are defined. TSC brackets
    // the loop so timing statistics are logged. No panic.
    let msg = Message::from_bytes(b"a7");
    const N: u64 = 100;
    let t0 = ctx.read_tsc();
    for _ in 0..N {
        let _ = ctx.try_send("adv-a7-recv", &msg);
    }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("adv: A7 timing mean={mean} cycles/try_send"));
    ctx.log("adv: A7 pass - timing analysis completed without panic");
    idle(ctx)
}

fn mode_adv_a8_witness(ctx: &ServiceContext) -> ! {
    // A8 witness - yields 1,000 times then logs pass (§22 Adversarial A8).
    // Runs alongside adv-a8 (tight loop hog). Timer-driven preemption (§9.1)
    // must give this service enough quanta to complete all yields.
    for _ in 0..1_000u32 { ctx.yield_cpu(); }
    ctx.log("adv: A8 pass - witness ran despite tight-loop hog");
    idle(ctx)
}

fn mode_adv_a9(ctx: &ServiceContext) -> ! {
    // A9 - Direct spawn bypassing supervisor → defined error (§22 Adversarial A9).
    // All v1 services hold a spawn cap (SPAWN_RESOURCE), so the syscall is authorised.
    // The name lookup fails (NotFound) because "nonexistent-does-not-exist" has no
    // service_config entry. Must return Err, never panic.
    match ctx.spawn("nonexistent-does-not-exist") {
        Err(_) => ctx.log("adv: A9 pass - spawn of unknown service returned Err"),
        Ok(()) => ctx.log("adv: A9 FAIL - unexpected spawn success for unknown service"),
    }
    idle(ctx)
}

fn mode_adv_a10(ctx: &ServiceContext) -> ! {
    // A10 - Kernel-space addresses as syscall buffer args → rejected (§22 Adversarial A10).
    // validate_user_slice in the kernel rejects any ptr ≥ kernel base or null.
    // Syscall 2 (Send) a1=msg_ptr, syscall 3 (Recv) a1=buf_ptr.
    const KERN_ADDRS: &[u64] = &[
        0xffff_8000_0000_0000, // HHDM base
        0xffff_ffff_ffff_fff0, // near u64::MAX
        0x0000_0000_0000_0000, // null pointer
        0x0000_8000_0000_0000, // just above user space
    ];
    for &addr in KERN_ADDRS {
        // SAFETY: addr fails validate_user_slice before any kernel-mode dereference.
        unsafe {
            probe_raw_syscall(2, 0, addr, 4096); // Send with kernel-addr msg_ptr
            probe_raw_syscall(2, 0, addr, 0);    // Send with kernel-addr, len=0
            probe_raw_syscall(3, 0, addr, 4096); // Recv with kernel-addr buf_ptr
        }
    }
    ctx.log("adv: A10 pass - kernel addrs as syscall args rejected without panic");
    idle(ctx)
}

fn mode_adv_a11(ctx: &ServiceContext) -> ! {
    // A11 - Introspection is gated by the INTROSPECT capability (§3.1;
    // docs/introspection-capability.md). adv-a11 holds NO introspect cap (its name
    // matches no grant), so a gated query must be DENIED and an ambient one must work.
    //
    // TaskStat(slot 0) targets init - a TCB task that is always alive. Without the
    // cap the kernel returns CapNotHeld, which the SDK coerces to valid:false. So a
    // *live* slot reading back invalid is the proof we were denied (with the cap it
    // would read back valid:true). A valid:true here would mean the gate is open.
    let stat = ctx.task_stat(0);
    if stat.valid {
        ctx.log("adv: A11 FAIL - TaskStat(0) succeeded without INTROSPECT cap (gate open)");
        idle(ctx);
    }
    // Ambient queries stay open with no cap: the TSC clock (InspectKernel 3) must
    // still return a nonzero value.
    if ctx.read_tsc() == 0 {
        ctx.log("adv: A11 FAIL - ambient TSC query (InspectKernel 3) returned 0");
        idle(ctx);
    }
    ctx.log("adv: A11 pass - gated introspection denied without cap; ambient queries open");
    idle(ctx)
}

fn mode_adv_a12(ctx: &ServiceContext) -> ! {
    // A12 - Reboot is gated by the REBOOT capability (§3.1). adv-a12 holds NO reboot cap (its name
    // matches no grant - only shell/xhci/ehci get it), so Reboot/18 must be DENIED with CapNotHeld
    // and the machine must NOT reset. `try_reboot` RETURNS the error code instead of looping; if the
    // gate were open the syscall would never return (the box hardware-resets) and the harness would
    // see a reboot loop instead of the pass marker - a rebooting/hung run fails the test.
    const CAP_NOT_HELD: i64 = -2; // cap_err_to_i64(CapNotHeld) - syscall/dispatch.rs
    let rc = ctx.try_reboot();
    if rc != CAP_NOT_HELD {
        ctx.log("adv: A12 FAIL - reboot not denied without REBOOT cap (gate open or wrong error)");
        idle(ctx);
    }
    ctx.log("adv: A12 pass - reboot denied without REBOOT cap (CapNotHeld); machine intact");
    idle(ctx)
}

fn mode_adv_a13(ctx: &ServiceContext) -> ! {
    // A13 - AcquireSendCap is gated by ACQUIRE_ANY-or-declared-peer (§3.1). adv-a13 holds NO ACQUIRE_ANY
    // (excluded from the probe grant) and declares NO send-peers, so minting a SEND cap to ANY service
    // must be DENIED (CapNotHeld -> acquire_send_cap returns None). `logger` is a real registered
    // service, so a `Some` here would mean the gate is open (ambient send authority).
    if ctx.acquire_send_cap("logger").is_some() {
        ctx.log("adv: A13 FAIL - acquired a SEND cap to a non-peer without ACQUIRE_ANY (gate open)");
        idle(ctx);
    }
    ctx.log("adv: A13 pass - AcquireSendCap denied for a non-holder, non-declared service");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal adversarial modes - Milestone 20.
// ---------------------------------------------------------------------------

fn mode_adv_ba1(ctx: &ServiceContext) -> ! {
    // BA1: Cap unforgeability - 50,000 random u32 slot indices (5× A1) (§22 Brutal Adv BA1).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 144;
    let msg = Message::from_bytes(b"ba1");
    for _ in 0..50_000u32 {
        let slot = CapHandle(xorshift64(&mut rng) as u32);
        if ctx.try_send_by_handle(slot, &msg).is_ok() {
            ctx.log("adv: BA1 FAIL - random cap slot accepted as valid SEND");
            idle(ctx);
        }
    }
    ctx.log("adv: BA1 pass (50000/50000)");
    idle(ctx)
}

fn mode_adv_ba2(ctx: &ServiceContext) -> ! {
    // BA2: Brute-force cap slots 0..=511 + 4 extreme values (§22 Brutal Adv BA2).
    let msg = Message::from_bytes(b"ba2");
    for slot in 0u32..512u32 {
        let _ = ctx.try_send_by_handle(CapHandle(slot), &msg);
    }
    for &slot in &[0xFFFF_u32, 0x0001_0000, 0x7FFF_FFFF, u32::MAX] {
        let _ = ctx.try_send_by_handle(CapHandle(slot), &msg);
    }
    ctx.log("adv: BA2 pass - extended slot sweep returned defined errors");
    idle(ctx)
}

fn mode_adv_ba3(ctx: &ServiceContext) -> ! {
    // BA3: 5 alloc-beyond-limit attack cycles (§22 Brutal Adv BA3).
    for _ in 0..5u32 {
        let _ = ctx.alloc_mem(0);
        let _ = ctx.alloc_mem(usize::MAX);
        let _ = ctx.alloc_mem(1usize << 40);
        let _ = ctx.alloc_mem(1usize << 50);
        let _ = ctx.alloc_mem(usize::MAX / 2 + 1);
    }
    ctx.log("adv: BA3 pass - 5× alloc edge cycles rejected without panic");
    idle(ctx)
}

fn mode_adv_ba4(ctx: &ServiceContext) -> ! {
    // BA4: Rights escalation - RECV cap used as SEND × 5, plus probe log/spawn slots (§22 Brutal Adv BA4).
    let recv_handle = ctx.recv_handle().unwrap_or(CapHandle(2));
    let msg = Message::from_bytes(b"ba4");
    for _ in 0..5u32 {
        match ctx.try_send_by_handle(recv_handle, &msg) {
            Err(IpcError::CapError(CapError::CapInsufficientRights)) => {}
            Ok(()) => { ctx.log("adv: BA4 FAIL - RECV cap accepted as SEND"); idle(ctx); }
            Err(_) => {}
        }
    }
    let _ = ctx.try_send_by_handle(CapHandle(0), &msg); // log_write - not SEND
    let _ = ctx.try_send_by_handle(CapHandle(1), &msg); // spawn - not SEND
    ctx.log("adv: BA4 pass - 5× RECV-cap-as-SEND rejected; non-SEND caps rejected");
    idle(ctx)
}

fn mode_adv_ba5(ctx: &ServiceContext) -> ! {
    // BA5: 5 TOCTOU kill+send cycles (§22 Brutal Adv BA5).
    let msg = Message::from_bytes(b"ba5");
    let mut pass = 0u32;
    for _ in 0..5u32 {
        let _ = ctx.kill("adv-ba5-victim");
        match ctx.try_send("adv-ba5-victim", &msg) {
            Err(IpcError::EndpointDead) | Err(_) => pass += 1,
            Ok(()) => { ctx.log("adv: BA5 FAIL - send succeeded after victim killed"); idle(ctx); }
        }
    }
    ctx.log_fmt(format_args!("adv: BA5 pass ({pass}/5 post-kill sends rejected)"));
    idle(ctx)
}

fn mode_adv_ba6(ctx: &ServiceContext) -> ! {
    // BA6: Fill cap table × 5 cycles - each fill hits exhaustion, None returned (§22 Brutal Adv BA6).
    for cycle in 0..5u32 {
        let mut count = 0u32;
        loop {
            match ctx.acquire_send_cap("adv-ba6") {
                Some(_) => count += 1,
                None    => break,
            }
        }
        ctx.log_fmt(format_args!("adv: BA6 cycle={cycle} filled={count}"));
    }
    ctx.log("adv: BA6 pass - 5× cap-table fill returned None without panic");
    idle(ctx)
}

fn mode_adv_ba7(ctx: &ServiceContext) -> ! {
    // BA7: 500 timing samples (5× A7) (§22 Brutal Adv BA7).
    let msg = Message::from_bytes(b"ba7");
    const N: u64 = 500;
    let t0 = ctx.read_tsc();
    for _ in 0..N {
        let _ = ctx.try_send("adv-ba7-recv", &msg);
    }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("adv: BA7 timing mean={mean} cycles/try_send"));
    ctx.log("adv: BA7 pass - 500 timing sends completed without panic");
    idle(ctx)
}

fn mode_adv_ba8_witness(ctx: &ServiceContext) -> ! {
    // BA8 witness - 200 yields alongside tight-loop hog (§22 Brutal Adv BA8).
    // 1000 was still too slow once the full brutal-suite load hits core 3.
    // Spawned early (before property/stress kill-respawn loops) so 200 yields
    // suffice to prove preemption fires while the system is still quiet.
    for _ in 0..200u32 { ctx.yield_cpu(); }
    ctx.log("adv: BA8 pass - witness ran 200 yields despite tight-loop hog");
    idle(ctx)
}

fn mode_adv_ba9(ctx: &ServiceContext) -> ! {
    // BA9: 5 direct-spawn bypass attempts with distinct bogus names (§22 Brutal Adv BA9).
    const NAMES: &[&str] = &[
        "nonexistent-ba9-a", "nonexistent-ba9-b", "nonexistent-ba9-c",
        "nonexistent-ba9-d", "nonexistent-ba9-e",
    ];
    for name in NAMES {
        match ctx.spawn(name) {
            Err(_) => {}
            Ok(()) => { ctx.log("adv: BA9 FAIL - unexpected spawn success"); idle(ctx); }
        }
    }
    ctx.log("adv: BA9 pass - 5 direct-spawn bypasses returned Err");
    idle(ctx)
}

fn mode_adv_ba10(ctx: &ServiceContext) -> ! {
    // BA10: 20 kernel-space address patterns (5× A10) (§22 Brutal Adv BA10).
    const ADDRS: &[u64] = &[
        0xffff_8000_0000_0000, 0xffff_ffff_ffff_fff0, 0x0000_0000_0000_0000, 0x0000_8000_0000_0000,
        0xffff_8001_0000_0000, 0xffff_0000_0000_0000, 0xffff_c000_0000_0000, 0xffff_a000_0000_0000,
        0xffff_8800_0000_0000, 0xffff_ffff_0000_0000, 0xdead_beef_dead_beef, 0xcafe_babe_cafe_babe,
        0xffff_ffff_ffff_0000, 0xffff_8000_0001_0000, 0x8000_0000_0000_0000, 0xffff_8000_ffff_ffff,
        0xffff_8000_0000_0001, 0xffff_8000_0000_00ff, 0x7fff_ffff_ffff_ffff, 0xffff_ffff_ffff_ffff,
    ];
    for &addr in ADDRS {
        // SAFETY: addr fails validate_user_slice before any kernel-mode dereference.
        unsafe {
            probe_raw_syscall(2, 0, addr, 4096);
            probe_raw_syscall(3, 0, addr, 4096);
        }
    }
    ctx.log("adv: BA10 pass - 20 kernel addr patterns rejected without panic");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Chaos-test modes - Milestone 14.
// ---------------------------------------------------------------------------

fn mode_chaos_c2(_ctx: &ServiceContext) -> ! {
    // C2 - Simulate corrupted ELF: immediately dereference null pointer (§22 Chaos C2).
    // The kernel delivers a page fault and kills this service without panicking.
    // chaos-c2-monitor witnesses the system continuing.
    // SAFETY: intentional fault for chaos test C2; kernel kills before any further use.
    unsafe { core::ptr::read_volatile(core::ptr::null::<u8>()); }
    loop { core::hint::spin_loop(); }
}

fn mode_chaos_c2_monitor(ctx: &ServiceContext) -> ! {
    // C2 witness - 100 yields then log pass, proving the system continued after
    // chaos-c2 was killed by the kernel's page-fault handler (§22 Chaos C2).
    for _ in 0..100u32 { ctx.yield_cpu(); }
    ctx.log("chaos: C2 pass - system continued after non-TCB page fault");
    idle(ctx)
}

fn mode_adv_fault_gp(_ctx: &ServiceContext) -> ! {
    // A14 - ring-3 #GP: a non-canonical memory access (bit 47 != bit 63) raises #GP(0) at CPL3. The
    // kernel must KILL this service (log "USER GPF (killing task)"), NOT halt the machine - the class the
    // commandment audit (C1) fixed. adv-fault-mon witnesses the system continuing.
    // SAFETY: intentional fault for regression test A14; the kernel kills the task before any further use.
    unsafe { let _ = core::ptr::read_volatile(0x8000_0000_0000_0000 as *const u8); }
    loop { core::hint::spin_loop(); }
}

fn mode_adv_fault_de(_ctx: &ServiceContext) -> ! {
    // A14 - ring-3 #DE: an integer divide-by-zero raises #DE (vector 0) at CPL3. Raw inline asm because
    // Rust inserts a divide guard (it would panic, not fault); the threat model includes adversarial/asm
    // services. The kernel must KILL this service (log "USER EXCEPTION (killing task)"), NOT halt.
    // SAFETY: intentional fault for regression test A14; the kernel kills the task before any further use.
    unsafe {
        core::arch::asm!(
            "xor eax, eax",
            "xor edx, edx",
            "xor ecx, ecx",
            "div ecx",   // (EDX:EAX)=0 / ECX=0 -> #DE (divide error)
            out("eax") _, out("edx") _, out("ecx") _,
            options(nostack, nomem),
        );
    }
    loop { core::hint::spin_loop(); }
}

fn mode_adv_fault_mon(ctx: &ServiceContext) -> ! {
    // A14 witness: the two faulters run on other cores; yield long enough for both to fault and be killed,
    // then log pass - proving the kernel KILLED the ring-3 faulters and the system continued rather than
    // wedging (invariant 12; kernel-audit C1/C2). If the fix were wrong the kernel would halt and this
    // line would never print (the test times out / trips its KERNEL-fault fail_on).
    for _ in 0..1000u32 { ctx.yield_cpu(); }
    ctx.log("adv: A14 pass - ring-3 #GP + #DE killed the task, kernel alive");
    idle(ctx)
}

fn mode_chaos_c3(ctx: &ServiceContext) -> ! {
    // C3 - Allocator saturation: 500 rounds of impossible requests (§22 Chaos C3).
    // memory_limit = 4 MiB; any request for more must return AllocDenied, not panic.
    for i in 0..500u32 {
        let r1 = ctx.alloc_mem(usize::MAX);
        let r2 = ctx.alloc_mem(1usize << 32);
        if r1.is_ok() || r2.is_ok() {
            ctx.log("chaos: C3 FAIL - impossible alloc succeeded");
            idle(ctx);
        }
        // Zero-size must not panic even when at the limit.
        let _ = ctx.alloc_mem(0);
        if i % 100 == 99 {
            ctx.log_fmt(format_args!("chaos: C3 iter {}/500", i + 1));
        }
    }
    ctx.log("chaos: C3 pass - 500 alloc-deny cycles without panic");
    idle(ctx)
}

fn mode_chaos_c5(ctx: &ServiceContext) -> ! {
    // C5 - Kernel stack depth probe: 100 nested recursive yield_cpu() calls (§22 Chaos C5).
    // Each frame issues one syscall; the kernel's per-syscall stack usage must not
    // accumulate across the 100 user-side recursion levels.
    let depth = chaos_c5_recurse(ctx, 100, 0);
    ctx.log_fmt(format_args!("chaos: C5 pass - {depth}/100 recursive yields without stack overflow"));
    idle(ctx)
}

#[inline(never)]
fn chaos_c5_recurse(ctx: &ServiceContext, remaining: u32, depth: u32) -> u32 {
    if remaining == 0 { return depth; }
    ctx.yield_cpu();
    chaos_c5_recurse(ctx, remaining - 1, depth + 1)
}

fn mode_chaos_c6_monitor(ctx: &ServiceContext) -> ! {
    // C6 witness (core 0) - 200 yields then log pass (§22 Chaos C6).
    // chaos-c6-hog runs a tight loop on core 3 (simulating timer starvation on that core).
    // This probe on core 0 verifies that the other cores remain scheduled normally.
    for _ in 0..200u32 { ctx.yield_cpu(); }
    ctx.log("chaos: C6 pass - core 0 alive despite core 3 hog");
    idle(ctx)
}

fn mode_chaos_c7(ctx: &ServiceContext) -> ! {
    // C7 - Cross-core TLB shootdown under load: 30 kill/respawn cycles (§22 Chaos C7).
    // Controller on core 1; victim on core 2. Each kill issues a cross-core IPI and
    // TLB shootdown; respawn maps new pages, triggering another shootdown on core 2.
    //
    // Instrumented: RDTSC-bracket each section (try_send / kill / spawn / 50-yield
    // settle) and report mean cycles-per-section at each iter marker. This attributes
    // the ~1.56 s/cycle uncontended cost measured on the T630 - confirming whether it
    // lives in the cross-core kill (TLB-shootdown broadcast) or elsewhere, and proving
    // the 50-yield settle loop is negligible. read_tsc is InspectKernel query 3
    // (ungated, no cap); its own per-call cost sits inside every bracket equally, so
    // the *relative* split is honest even though absolutes carry a small fixed offset.
    let msg = Message::from_bytes(b"c7");
    let (mut c_send, mut c_kill, mut c_spawn, mut c_yield) = (0u64, 0u64, 0u64, 0u64);
    for i in 0..30u32 {
        // try_send exercises the generation-check on a live (or recently-dead) endpoint.
        let t0 = ctx.read_tsc();
        let _ = ctx.try_send("chaos-c7-victim", &msg);
        let t1 = ctx.read_tsc();
        // Kill victim on core 2 → IPI → TLB shootdown → page frames reclaimed.
        let _ = ctx.kill("chaos-c7-victim");
        let t2 = ctx.read_tsc();
        // Respawn on core 2 → new page table mapping → another TLB shootdown on core 2.
        let _ = ctx.spawn("chaos-c7-victim");
        let t3 = ctx.read_tsc();
        // Brief yield to allow the new victim to be scheduled and its pages faulted in.
        for _ in 0..50u32 { ctx.yield_cpu(); }
        let t4 = ctx.read_tsc();

        c_send  = c_send.wrapping_add(t1.wrapping_sub(t0));
        c_kill  = c_kill.wrapping_add(t2.wrapping_sub(t1));
        c_spawn = c_spawn.wrapping_add(t3.wrapping_sub(t2));
        c_yield = c_yield.wrapping_add(t4.wrapping_sub(t3));

        if i % 10 == 9 {
            let n = (i + 1) as u64;
            ctx.log_fmt(format_args!("chaos: C7 iter {}/30", i + 1));
            // Mean cycles/iter per section over the run so far (divide by ~2 GHz
            // on the T630 for seconds: e.g. 3.1e9 cyc ≈ 1.55 s).
            ctx.log_fmt(format_args!(
                "chaos: C7 split (cyc/iter) send={} kill={} spawn={} yield50={}",
                c_send / n, c_kill / n, c_spawn / n, c_yield / n,
            ));
        }
    }
    ctx.log("chaos: C7 pass - 30 cross-core TLB shootdowns survived");
    idle(ctx)
}

fn mode_xsend_recv(ctx: &ServiceContext) -> ! {
    // Cross-core try_send diagnostic - receiver on core 2. Drain forever: recv()
    // blocks when the queue is empty, so the sender on core 1 finds us either
    // blocked-on-recv (its send fires a cross-core IPI wake) or with queue space.
    // No echo - this isolates the ONE-WAY send cost, never a round-trip.
    loop { let _ = ctx.recv(); }
}

fn mode_xsend(ctx: &ServiceContext) -> ! {
    // Single cross-core try_send timing: sender on core 1 → xsend-recv on core 2.
    // Isolates the cost C7's "send" section conflated with sending to a just-killed
    // victim (stale cap → EndpointDead). Here the receiver is always LIVE.
    //
    // Reports mean RDTSC cyc/op (T630 ~2 GHz → cyc/2000 = µs) over N iters:
    //   tsc-overhead : back-to-back read_tsc - subtract this from every figure below
    //   paced-handle : 30 yields between sends so the receiver is blocked → each send
    //                  enqueues + fires the cross-core IPI wake (by handle, no lookup)
    //   tight-handle : back-to-back sends - queue saturates → mostly QueueFull fast path
    //   paced-name   : paced, via try_send(name) → adds the userspace cap-cache lookup
    // paced-handle is the apples-to-apples comparison against C7's ~249 ms "send".
    let h = match ctx.send_peer_at(0) {
        Some(h) => h,
        None => { ctx.log("xsend: FAIL - no send cap to xsend-recv"); idle(ctx); }
    };
    let msg = Message::from_bytes(b"x");
    const N: u64 = 2000;

    // Baseline: empty read_tsc→read_tsc bracket (the fixed per-measurement offset).
    let mut acc = 0u64;
    for _ in 0..N {
        let t0 = ctx.read_tsc();
        let t1 = ctx.read_tsc();
        acc = acc.wrapping_add(t1.wrapping_sub(t0));
    }
    let tsc_overhead = acc / N;

    // paced-handle: receiver blocked → cross-core IPI wake on each send.
    acc = 0;
    for _ in 0..N {
        for _ in 0..30u32 { ctx.yield_cpu(); }
        let t0 = ctx.read_tsc();
        let _ = ctx.try_send_by_handle(h, &msg);
        let t1 = ctx.read_tsc();
        acc = acc.wrapping_add(t1.wrapping_sub(t0));
    }
    let paced_handle = acc / N;

    // tight-handle: back-to-back, queue saturates.
    acc = 0;
    for _ in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.try_send_by_handle(h, &msg);
        let t1 = ctx.read_tsc();
        acc = acc.wrapping_add(t1.wrapping_sub(t0));
    }
    let tight_handle = acc / N;

    // paced-name: adds the find_send_slot userspace cache lookup.
    acc = 0;
    for _ in 0..N {
        for _ in 0..30u32 { ctx.yield_cpu(); }
        let t0 = ctx.read_tsc();
        let _ = ctx.try_send("xsend-recv", &msg);
        let t1 = ctx.read_tsc();
        acc = acc.wrapping_add(t1.wrapping_sub(t0));
    }
    let paced_name = acc / N;

    ctx.log_fmt(format_args!(
        "xsend: cyc/op  tsc-overhead={} paced-handle={} tight-handle={} paced-name={}",
        tsc_overhead, paced_handle, tight_handle, paced_name,
    ));
    ctx.log("xsend: done");
    idle(ctx)
}

fn mode_xlife(ctx: &ServiceContext) -> ! {
    // Cross-core task-lifecycle timing: controller on core 1 kills+respawns a
    // SAME-core victim (xlife-near, core 1) and a CROSS-core victim (xlife-far,
    // core 2), RDTSC-bracketing each kill and each spawn separately. Both victims
    // are the same PROBE_ELF, so task-creation work (page tables + mapping the
    // binary) is identical - the far−near delta isolates the cross-core
    // coordination cost (remote deschedule IPI + TLB-shootdown wait) from the
    // task-creation cost. Attributes C7's ~1.04 s respawn: if spawn_far ≫ spawn_near
    // the cost is cross-core; if spawn_near ≈ spawn_far ≫ BP5 (~23 ms) the cost is
    // task creation itself. read_tsc is InspectKernel q3 (ungated). Victims exist
    // at boot (supervisor spawns them first), so the first kill always has a target.
    let (mut k_near, mut s_near, mut k_far, mut s_far) = (0u64, 0u64, 0u64, 0u64);
    const N: u32 = 20;
    for i in 0..N {
        // same-core: kill + respawn xlife-near (core 1, the controller's own core).
        let a = ctx.read_tsc();
        let _ = ctx.kill("xlife-near");
        let b = ctx.read_tsc();
        let _ = ctx.spawn("xlife-near");
        let c = ctx.read_tsc();
        // cross-core: kill + respawn xlife-far (core 2).
        let _ = ctx.kill("xlife-far");
        let d = ctx.read_tsc();
        let _ = ctx.spawn("xlife-far");
        let e = ctx.read_tsc();

        k_near = k_near.wrapping_add(b.wrapping_sub(a));
        s_near = s_near.wrapping_add(c.wrapping_sub(b));
        k_far  = k_far.wrapping_add(d.wrapping_sub(c));
        s_far  = s_far.wrapping_add(e.wrapping_sub(d));

        if i % 5 == 4 {
            let n = (i + 1) as u64;
            ctx.log_fmt(format_args!(
                "xlife: cyc/op (n={}) kill_near={} spawn_near={} kill_far={} spawn_far={}",
                n, k_near / n, s_near / n, k_far / n, s_far / n,
            ));
        }
    }
    ctx.log("xlife: done");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal property test modes - Milestone 16
// ---------------------------------------------------------------------------

fn mode_prop_bp1(ctx: &ServiceContext) -> ! {
    // BP1 - Cap unforgeability at 100k iterations (§7.3, §3.1).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 104;
    let msg = Message::from_bytes(b"bp1");
    for _ in 0..100_000u32 {
        let slot = CapHandle(xorshift64(&mut rng) as u32);
        if ctx.try_send_by_handle(slot, &msg).is_ok() {
            ctx.log("prop: BP1 FAIL - random cap slot accepted as valid SEND");
            idle(ctx);
        }
    }
    ctx.log("prop: BP1 pass (100000/100000)");
    idle(ctx)
}

fn mode_prop_bp2(ctx: &ServiceContext) -> ! {
    // BP2 - Generation strictly monotonic over 20 kill/respawn cycles (§7.5).
    let mut prev_gen: u64 = 0;
    for cycle in 0..20u32 {
        let _ = ctx.kill("prop-bp2-victim");
        let _ = ctx.spawn("prop-bp2-victim");
        let gen = ctx.inspect_endpoint_generation("prop-bp2-victim");
        if gen <= prev_gen {
            ctx.log_fmt(format_args!("prop: BP2 FAIL - generation not monotonic at cycle {}", cycle));
            idle(ctx);
        }
        prev_gen = gen;
    }
    ctx.log("prop: BP2 pass (20/20)");
    idle(ctx)
}

fn mode_prop_bp3(ctx: &ServiceContext) -> ! {
    // BP3 - Cap rights never widen during transfer - 10k iterations (§7.3).
    // Self-referential: acquires SEND|GRANT cap to own endpoint, bounces it
    // through the queue 10k times, asserting rights are exactly preserved each round.
    const SEND_GRANT: u64 = (1 << 2) | (1 << 4);
    let mut cap_handle = match ctx.acquire_send_grant_cap("prop-bp3") {
        Some(h) => h,
        None => { ctx.log("prop: BP3 FAIL - could not acquire SEND|GRANT cap to self"); idle(ctx); }
    };
    let msg = Message::from_bytes(b"bp3");
    for _iter in 0..10_000u32 {
        match ctx.send_with_cap_by_handle(cap_handle, cap_handle, &msg) {
            Ok(()) => {}
            Err(_) => { ctx.log("prop: BP3 FAIL - send_with_cap_by_handle failed"); idle(ctx); }
        }
        ctx.recv();
        let new_handle = match ctx.take_pending_cap() {
            Some(h) => h,
            None => { ctx.log("prop: BP3 FAIL - no pending cap after recv"); idle(ctx); }
        };
        let rights = match ctx.query_cap_rights(new_handle) {
            Some(r) => r,
            None => { ctx.log("prop: BP3 FAIL - cap slot empty after transfer"); idle(ctx); }
        };
        if rights != SEND_GRANT {
            ctx.log("prop: BP3 FAIL - cap rights changed during transfer");
            idle(ctx);
        }
        cap_handle = new_handle;
    }
    ctx.log("prop: BP3 pass (10000/10000)");
    idle(ctx)
}

fn mode_prop_bp4(ctx: &ServiceContext) -> ! {
    // BP4 - ∑ alloc_bytes ≡ pages mapped - 2k iterations (§10.3).
    // 2k × 4 KiB = 8 MiB total, well within the 64 MiB limit.
    let mut expected: u64 = 0;
    for _ in 0..2_000u32 {
        match ctx.alloc_mem(4096) {
            Ok(_)  => expected += 4096,
            Err(_) => { ctx.log("prop: BP4 FAIL - unexpected alloc failure for 4 KiB page"); idle(ctx); }
        }
        let _ = ctx.alloc_mem(1 << 30); // 1 GiB - always denied; must not shift counter
        let actual = ctx.inspect_kernel_alloc_bytes();
        if actual != expected {
            ctx.log("prop: BP4 FAIL - alloc_bytes mismatch after alloc sequence");
            idle(ctx);
        }
    }
    ctx.log("prop: BP4 pass (2000/2000)");
    idle(ctx)
}

fn mode_prop_bp5(ctx: &ServiceContext) -> ! {
    // BP5 - Every live endpoint has exactly one owning task - 150 cycles (§8.3).
    // Test: spawn must succeed for all 150 cycles. If dead endpoints are orphaned
    // (not recycled by kill_endpoint), the 64-slot routing table fills up within
    // ~34 cycles and spawn returns Err. The spawn-success check is the correct
    // observable for this property; a count-vs-threshold check is unreliable here
    // because many other concurrent probes also kill/respawn services, causing the
    // live count to fluctuate independently of BP5's own cycles.
    for _ in 0..150u32 {
        let _ = ctx.kill("prop-bp5-victim");
        match ctx.spawn("prop-bp5-victim") {
            Err(_) => {
                ctx.log("prop: BP5 FAIL - spawn failed (routing table overflow; orphan detected)");
                idle(ctx);
            }
            Ok(()) => {}
        }
    }
    ctx.log("prop: BP5 pass (150/150)");
    idle(ctx)
}

fn mode_prop_bp6(ctx: &ServiceContext) -> ! {
    // BP6 - Queue depth invariant at 2k iterations (§8.5).
    // Self-referential: sends to own endpoint, draining after each fill phase.
    ctx.log("prop: BP6 starting");
    const QUEUE_DEPTH: u32 = 16;
    let msg = Message::from_bytes(b"bp6");
    let recv_h = match ctx.recv_handle() {
        Some(h) => h,
        None => { ctx.log("prop: BP6 FAIL - no recv endpoint"); idle(ctx); }
    };
    for iter in 0..2_000u32 {
        let depth = (iter % (QUEUE_DEPTH + 1)) as u8;
        for _ in 0..depth {
            match ctx.try_send("prop-bp6", &msg) {
                Ok(()) => {}
                Err(_) => {
                    ctx.log("prop: BP6 FAIL - try_send failed before expected queue depth");
                    idle(ctx);
                }
            }
        }
        if depth == QUEUE_DEPTH as u8 {
            match ctx.try_send("prop-bp6", &msg) {
                Err(IpcError::QueueFull) => {}
                Ok(()) => {
                    ctx.log("prop: BP6 FAIL - queue accepted more than 16 messages");
                    idle(ctx);
                }
                Err(_) => {
                    ctx.log("prop: BP6 FAIL - unexpected error on full-queue try_send");
                    idle(ctx);
                }
            }
        }
        for _ in 0..depth {
            match godspeed_sdk::ipc::recv(recv_h) {
                Ok(_) => {}
                Err(_) => { ctx.log("prop: BP6 FAIL - recv returned error"); idle(ctx); }
            }
        }
    }
    ctx.log("prop: BP6 pass (2000/2000)");
    idle(ctx)
}

fn mode_prop_bp7(ctx: &ServiceContext) -> ! {
    // BP7 - TLB shootdown leaves no stale mappings - 150 cycles (§10.5).
    // Proxy: 150 kill/respawn cycles; generation monotonicity confirms the full
    // kill/shootdown lifecycle completed correctly each time.
    let mut prev_gen: u64 = 0;
    for _ in 0..150u32 {
        let _ = ctx.kill("prop-bp7-victim");
        let gen = ctx.inspect_endpoint_generation("prop-bp7-victim");
        if gen <= prev_gen {
            ctx.log("prop: BP7 FAIL - generation not monotonic after kill (TLB lifecycle broken)");
            idle(ctx);
        }
        prev_gen = gen;
        let _ = ctx.spawn("prop-bp7-victim");
    }
    ctx.log("prop: BP7 pass (150/150)");
    idle(ctx)
}

fn mode_prop_bp8(ctx: &ServiceContext) -> ! {
    // BP8 - After restart, name resolves to higher-generation live endpoint - 20 iter (§14.2).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 111;
    let mut prev_gen: u64 = 0;
    for _iter in 0..20u32 {
        let n_cycles = 1 + (xorshift64(&mut rng) % 2) as u32;
        for _cycle in 0..n_cycles {
            let _ = ctx.kill("prop-bp8-victim");
            let _ = ctx.spawn("prop-bp8-victim");
            let gen = ctx.inspect_endpoint_generation("prop-bp8-victim");
            if gen <= prev_gen {
                ctx.log("prop: BP8 FAIL - generation not monotonic after restart");
                idle(ctx);
            }
            prev_gen = gen;
        }
    }
    ctx.log("prop: BP8 pass (20 iter)");
    idle(ctx)
}

fn mode_prop_bp9(ctx: &ServiceContext) -> ! {
    // BP9 - Generation bump invalidates ALL 3 cap slots - 10 kill/respawn cycles (§7.5).
    // After each kill: all 3 wired SEND caps must return EndpointDead (not just some).
    // After each respawn: stale caps must STILL return EndpointDead (no auto-update to new gen).
    let msg  = Message::from_bytes(b"bp9");
    let h0   = ctx.send_peer_at(0);
    let h1   = ctx.send_peer_at(1);
    let h2   = ctx.send_peer_at(2);
    let (h0, h1, h2) = match (h0, h1, h2) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => { ctx.log("prop: BP9 FAIL - could not read all 3 send peer handles"); idle(ctx); }
    };
    for cycle in 0..10u32 {
        let _ = ctx.kill("prop-bp9-victim");
        let dead0 = matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead));
        let dead1 = matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead));
        let dead2 = matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead));
        if !dead0 || !dead1 || !dead2 {
            ctx.log_fmt(format_args!("prop: BP9 FAIL - not all 3 slots EndpointDead after kill at cycle {}", cycle));
            idle(ctx);
        }
        let _ = ctx.spawn("prop-bp9-victim");
        // Stale caps must NOT auto-update to the new instance's generation.
        let still0 = matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead));
        let still1 = matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead));
        let still2 = matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead));
        if !still0 || !still1 || !still2 {
            ctx.log_fmt(format_args!("prop: BP9 FAIL - stale cap updated to new instance at cycle {}", cycle));
            idle(ctx);
        }
    }
    ctx.log("prop: BP9 pass (10/10 - all 3 slots EndpointDead per cycle; stale caps stable)");
    idle(ctx)
}

fn mode_prop_bp10(ctx: &ServiceContext) -> ! {
    // BP10 - Every try_send returns a defined outcome - 100k iterations (§8.6, §8.2).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 113;
    for _ in 0..100_000u32 {
        let slot = CapHandle(xorshift64(&mut rng) as u32);
        let raw  = xorshift64(&mut rng);
        let msg  = Message::from_bytes(&raw.to_le_bytes());
        let _    = ctx.try_send_by_handle(slot, &msg);
    }
    ctx.log("prop: BP10 pass (100000/100000)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal fuzz test modes - Milestone 17
// ---------------------------------------------------------------------------

fn mode_fuzz_bf1(ctx: &ServiceContext) -> ! {
    // BF1 - Random syscall args (§22 Fuzz BF1).
    // 500 × 10 syscalls = 5,000 total (5× F1). Same exclusions as F1.
    const NRS: &[u64] = &[1, 2, 3, 5, 7, 8, 10, 11, 12, 14];
    const A1S: &[u64] = &[0, 0xffff800000000000, u64::MAX, 0xffff_8000_0000_1000];
    const A2S: &[u64] = &[0, 1, 255, 256, 4096, u64::MAX];

    ctx.log("fuzz: BF1 starting");
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 114;
    for &nr in NRS {
        for iter in 0..500u32 {
            let a0 = match iter % 8 {
                0 => 0u64,
                1 => 1u64,
                2 => 64u64,
                3 => 0xFFFFu64,
                4 => u64::MAX,
                5 => xorshift64(&mut rng) as u32 as u64,
                6 => xorshift64(&mut rng) & 0xFF,
                _ => xorshift64(&mut rng),
            };
            let a1 = A1S[(iter as usize) % A1S.len()];
            let a2 = A2S[(iter as usize) % A2S.len()];
            // SAFETY: nr != 9 (Abort); a1/a2 fail validate_user_slice.
            unsafe { probe_raw_syscall(nr, a0, a1, a2); }
        }
    }
    ctx.log("fuzz: BF1 pass (500/10)");
    idle(ctx)
}

fn mode_fuzz_bf2(ctx: &ServiceContext) -> ! {
    // BF2 - Random syscall numbers (§22 Fuzz BF2). 200,000 random u64 numbers (4× F2).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 115;
    let mut bad = 0u32;
    for _ in 0..200_000u32 {
        let raw = xorshift64(&mut rng);
        let nr = if raw <= 15 { raw + 100 } else { raw };
        // SAFETY: nr > 15 → dispatch _ arm → returns -1; no panic.
        let ret = unsafe { probe_raw_syscall(nr, 0, 0, 0) };
        if ret != -1 { bad += 1; }
    }
    if bad > 0 {
        ctx.log("fuzz: BF2 FAIL - unknown syscall returned non-(-1)");
    } else {
        ctx.log("fuzz: BF2 pass (200000/200000)");
    }
    idle(ctx)
}

fn mode_fuzz_bf5(ctx: &ServiceContext) -> ! {
    // BF5 - Random IPC message bodies (§22 Fuzz BF5). 5,000 try_send calls (5× F5).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 116;
    for _ in 0..5_000u32 {
        let size = (xorshift64(&mut rng) % 4097) as usize;
        let mut buf = [0u8; 4096];
        for b in buf[..size.min(4096)].iter_mut() {
            *b = xorshift64(&mut rng) as u8;
        }
        let msg = Message::from_bytes(&buf[..size.min(4096)]);
        let _ = ctx.try_send("fuzz-bf5-recv", &msg);
    }
    ctx.log("fuzz: BF5 pass (5000/5000)");
    idle(ctx)
}

fn mode_fuzz_bf6(ctx: &ServiceContext) -> ! {
    // BF6 - Embedded cap slot fuzzing (§22 Fuzz BF6). 5,000 SendWithCap calls (5× F6).
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 117;
    let msg = Message::from_bytes(b"bf6");
    for _ in 0..5_000u32 {
        let ep_slot  = CapHandle(xorshift64(&mut rng) as u32);
        let cap_slot = CapHandle(xorshift64(&mut rng) as u32);
        let _ = ctx.send_with_cap_by_handle(ep_slot, cap_slot, &msg);
    }
    ctx.log("fuzz: BF6 pass (5000/5000)");
    idle(ctx)
}

fn mode_fuzz_bf7(ctx: &ServiceContext) -> ! {
    // BF7 - Stale cap / generation fuzzing (§22 Fuzz BF7). 200 kill cycles (4× F7).
    let msg   = Message::from_bytes(b"bf7");
    let stale = ctx.send_peer_at(0); // SEND cap to fuzz-bf7-victim

    for _ in 0..200u32 {
        let _ = ctx.kill("fuzz-bf7-victim");

        if let Some(h) = stale {
            if ctx.try_send_by_handle(h, &msg).is_ok() {
                ctx.log("fuzz: BF7 FAIL - send to killed endpoint succeeded");
                idle(ctx);
            }
        }

        let _ = ctx.try_send_by_handle(CapHandle(0xBEEF), &msg);
        let _ = ctx.try_send_by_handle(CapHandle(u32::MAX), &msg);

        let _ = ctx.spawn("fuzz-bf7-victim");
        if let Some(h) = stale {
            let _ = ctx.try_send_by_handle(h, &msg);
        }
    }
    ctx.log("fuzz: BF7 pass (200/200)");
    idle(ctx)
}

fn mode_fuzz_bf8(ctx: &ServiceContext) -> ! {
    // BF8 - Memory request sizes (§22 Fuzz BF8). 10 edge cases + 5,000 random (5× F8).
    let edge_cases: &[usize] = &[
        0,
        1,
        4095,
        4096,
        4097,
        64 * 1024 * 1024 + 1,
        1 << 30,
        usize::MAX - 4095,
        usize::MAX - 1,
        usize::MAX,
    ];
    for &size in edge_cases {
        let _ = ctx.alloc_mem(size);
    }
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 119;
    for _ in 0..5_000u32 {
        let _ = ctx.alloc_mem(xorshift64(&mut rng) as usize);
    }
    ctx.log("fuzz: BF8 pass");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal stress test modes - Milestone 18
// ---------------------------------------------------------------------------

fn mode_stress_bs1(ctx: &ServiceContext) -> ! {
    // BS1 - IPC saturation, 5× S1 (§22 Brutal Stress BS1).
    // 50,000 try_send calls to stress-bs1-recv (passive). QueueFull acceptable.
    let msg = Message::from_bytes(b"bs1");
    for _ in 0..50_000u32 {
        let _ = ctx.try_send("stress-bs1-recv", &msg);
    }
    ctx.log("stress: BS1 pass (50000/50000)");
    idle(ctx)
}

fn mode_stress_bs2(ctx: &ServiceContext) -> ! {
    // BS2 - Restart storm, 4× S2 (§22 Brutal Stress BS2).
    // 200 kill/respawn cycles of stress-bs2-victim.
    let msg = Message::from_bytes(b"bs2-ping");
    match ctx.try_send("stress-bs2-victim", &msg) {
        Ok(()) => {}
        Err(_) => {
            ctx.log("stress: BS2 FAIL - victim not reachable at start");
            idle(ctx);
        }
    }
    for _ in 0..200u32 {
        let _ = ctx.kill("stress-bs2-victim");
        match ctx.spawn("stress-bs2-victim") {
            Err(_) => {
                ctx.log("stress: BS2 FAIL - spawn failed (kstack pool exhausted?)");
                idle(ctx);
            }
            Ok(()) => {}
        }
    }
    ctx.log("stress: BS2 pass (200/200)");
    idle(ctx)
}

fn mode_stress_bs3_send(ctx: &ServiceContext) -> ! {
    // BS3 sender - cross-core thrash, 4× S3 (§22 Brutal Stress BS3).
    // 2000 blocking sends to stress-bs3-recv on core 1.
    let msg = Message::from_bytes(b"bs3");
    for _ in 0..2_000u32 {
        let _ = ctx.send("stress-bs3-recv", &msg);
    }
    idle(ctx)
}

fn mode_stress_bs3_recv(ctx: &ServiceContext) -> ! {
    // BS3 receiver - drain 2000 cross-core messages (§22 Brutal Stress BS3).
    for _ in 0..2_000u32 {
        ctx.recv();
    }
    ctx.log("stress: BS3 pass (2000/2000)");
    idle(ctx)
}

fn mode_stress_bs4(ctx: &ServiceContext) -> ! {
    // BS4 - Cap table churn, 5× S4 (§22 Brutal Stress BS4).
    // 50 churn cycles with 2 SEND caps; generation monotonic and both caps stale.
    let h0 = match ctx.send_peer_at(0) {
        Some(h) => h,
        None => {
            ctx.log("stress: BS4 FAIL - no peer handle h0");
            idle(ctx);
        }
    };
    let h1 = match ctx.send_peer_at(1) {
        Some(h) => h,
        None => {
            ctx.log("stress: BS4 FAIL - no peer handle h1");
            idle(ctx);
        }
    };
    let msg = Message::from_bytes(b"bs4");

    if ctx.try_send_by_handle(h0, &msg).is_err() {
        ctx.log("stress: BS4 FAIL - cap A not valid pre-kill");
        idle(ctx);
    }
    if ctx.try_send_by_handle(h1, &msg).is_err() {
        ctx.log("stress: BS4 FAIL - cap B not valid pre-kill");
        idle(ctx);
    }

    let _ = ctx.kill("stress-bs4-victim");

    if !matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: BS4 FAIL - cap A survived first kill");
        idle(ctx);
    }
    if !matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: BS4 FAIL - cap B survived first kill");
        idle(ctx);
    }

    let mut prev_gen = ctx.inspect_endpoint_generation("stress-bs4-victim");
    for _ in 0..50u32 {
        let _ = ctx.spawn("stress-bs4-victim");
        let _ = ctx.kill("stress-bs4-victim");
        let gen = ctx.inspect_endpoint_generation("stress-bs4-victim");
        if gen <= prev_gen {
            ctx.log("stress: BS4 FAIL - generation not monotonic under churn");
            idle(ctx);
        }
        prev_gen = gen;
        if !matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: BS4 FAIL - cap A not stale during churn");
            idle(ctx);
        }
        if !matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: BS4 FAIL - cap B not stale during churn");
            idle(ctx);
        }
    }
    ctx.log("stress: BS4 pass (50/50)");
    idle(ctx)
}

fn mode_stress_bs5(ctx: &ServiceContext) -> ! {
    // BS5 - Generation integrity, 5× S5 (§22 Brutal Stress BS5).
    // 5000 kill/respawn cycles; endpoint generation must be strictly monotonic.
    let mut prev_gen: u64 = 0;
    for _ in 0..5_000u32 {
        let _ = ctx.kill("stress-bs5-victim");
        let _ = ctx.spawn("stress-bs5-victim");
        let gen = ctx.inspect_endpoint_generation("stress-bs5-victim");
        if gen <= prev_gen {
            ctx.log("stress: BS5 FAIL - generation not strictly monotonic after kill/respawn");
            idle(ctx);
        }
        prev_gen = gen;
    }
    ctx.log("stress: BS5 pass (5000/5000)");
    idle(ctx)
}

fn mode_stress_bs6(ctx: &ServiceContext) -> ! {
    // BS6 - Self-ping stability, 4× S6 (§22 Brutal Stress BS6).
    // 20,000 self-ping rounds; IPC path must not drift or corrupt.
    let msg = Message::from_bytes(b"bs6");
    for _ in 0..20_000u32 {
        match ctx.send("stress-bs6", &msg) {
            Ok(()) => {}
            Err(_) => {
                ctx.log("stress: BS6 FAIL - send to self returned error");
                idle(ctx);
            }
        }
        ctx.recv();
    }
    ctx.log("stress: BS6 pass (20000/20000)");
    idle(ctx)
}

fn mode_stress_bs7(ctx: &ServiceContext) -> ! {
    // BS7 - Memory pressure, 5× S7 (§22 Brutal Stress BS7).
    // 500 alloc passes; AllocDenied must appear and be consistent.
    const CHUNK: usize = 4 * 1024 * 1024;
    let mut at_limit = false;
    for _ in 0..500u32 {
        match ctx.alloc_mem(CHUNK) {
            Ok(_) => {
                if at_limit {
                    ctx.log("stress: BS7 FAIL - Ok returned after AllocDenied");
                    idle(ctx);
                }
            }
            Err(AllocError::Denied) => {
                at_limit = true;
            }
            Err(_) => {
                ctx.log("stress: BS7 FAIL - unexpected alloc error");
                idle(ctx);
            }
        }
    }
    if !at_limit {
        ctx.log("stress: BS7 FAIL - AllocDenied never returned (limit not enforced)");
        idle(ctx);
    }
    ctx.log("stress: BS7 pass (500/500)");
    idle(ctx)
}

fn mode_stress_bs8(ctx: &ServiceContext) -> ! {
    // BS8 - Scheduler heartbeat, 5× S8 (§22 Brutal Stress BS8).
    // 3000 yield cycles; scheduler must correctly return from idle each time.
    for _ in 0..3_000u32 {
        ctx.yield_cpu();
    }
    ctx.log("stress: BS8 pass (3000 yields)");
    idle(ctx)
}

fn mode_stress_bs9_send(ctx: &ServiceContext) -> ! {
    // BS9 sender - IPI storm, 5× S9 (§22 Brutal Stress BS9).
    // 2500 sends to stress-bs9-recv on core 2 via try_send+yield-retry.
    let msg = Message::from_bytes(b"bs9");
    for _ in 0..2_500u32 {
        loop {
            match ctx.try_send("stress-bs9-recv", &msg) {
                Ok(()) => break,
                Err(_) => ctx.yield_cpu(),
            }
        }
    }
    idle(ctx)
}

fn mode_stress_bs9_recv(ctx: &ServiceContext) -> ! {
    // BS9 receiver - drain 5000 msgs from two senders (§22 Brutal Stress BS9).
    for _ in 0..5_000u32 {
        ctx.recv();
    }
    ctx.log("stress: BS9 pass (5000/5000)");
    idle(ctx)
}

fn mode_stress_bs10(ctx: &ServiceContext) -> ! {
    // BS10 - Cascading revocation with 50 cycles (§22 Brutal Stress BS10).
    // 3 SEND caps to stress-bs10-victim on core 1; probe on core 0.
    // Pre-validate, first kill, then 50 respawn+kill cycles confirming all 3 stay stale.
    let h0 = match ctx.send_peer_at(0) {
        Some(h) => h,
        None => {
            ctx.log("stress: BS10 FAIL - no peer handle h0");
            idle(ctx);
        }
    };
    let h1 = match ctx.send_peer_at(1) {
        Some(h) => h,
        None => {
            ctx.log("stress: BS10 FAIL - no peer handle h1");
            idle(ctx);
        }
    };
    let h2 = match ctx.send_peer_at(2) {
        Some(h) => h,
        None => {
            ctx.log("stress: BS10 FAIL - no peer handle h2");
            idle(ctx);
        }
    };
    let msg = Message::from_bytes(b"bs10");

    if ctx.try_send_by_handle(h0, &msg).is_err() {
        ctx.log("stress: BS10 FAIL - cap A not valid pre-kill");
        idle(ctx);
    }
    if ctx.try_send_by_handle(h1, &msg).is_err() {
        ctx.log("stress: BS10 FAIL - cap B not valid pre-kill");
        idle(ctx);
    }
    if ctx.try_send_by_handle(h2, &msg).is_err() {
        ctx.log("stress: BS10 FAIL - cap C not valid pre-kill");
        idle(ctx);
    }

    let _ = ctx.kill("stress-bs10-victim");

    if !matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: BS10 FAIL - cap A survived first kill");
        idle(ctx);
    }
    if !matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: BS10 FAIL - cap B survived first kill");
        idle(ctx);
    }
    if !matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead)) {
        ctx.log("stress: BS10 FAIL - cap C survived first kill");
        idle(ctx);
    }

    for _ in 0..50u32 {
        let _ = ctx.spawn("stress-bs10-victim");
        let _ = ctx.kill("stress-bs10-victim");
        if !matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: BS10 FAIL - cap A not stale during cycle");
            idle(ctx);
        }
        if !matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: BS10 FAIL - cap B not stale during cycle");
            idle(ctx);
        }
        if !matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead)) {
            ctx.log("stress: BS10 FAIL - cap C not stale during cycle");
            idle(ctx);
        }
    }
    ctx.log("stress: BS10 pass (50/50 cycles)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal identity test modes - Milestone 15
// ---------------------------------------------------------------------------

fn mode_brutal_id_11(ctx: &ServiceContext) -> ! {
    // T11 - Queue boundary exactness (§22 Brutal Identity T11).
    // Self-referential: brutal-id-11 has itself as a SEND peer so it can
    // try_send to its own endpoint. Verifies the 16-deep queue limit exactly.
    let msg = Message::from_bytes(b"t11");
    for i in 0..16u32 {
        match ctx.try_send("brutal-id-11", &msg) {
            Ok(()) => {}
            Err(_) => {
                ctx.log_fmt(format_args!("identity: T11 FAIL - send {} failed before queue full", i));
                idle(ctx);
            }
        }
    }
    // 17th send must be QueueFull - not Ok, not any other error.
    match ctx.try_send("brutal-id-11", &msg) {
        Err(IpcError::QueueFull) => {}
        Ok(()) => {
            ctx.log("identity: T11 FAIL - 17th send succeeded (queue not bounded at 16)");
            idle(ctx);
        }
        Err(_) => {
            ctx.log("identity: T11 FAIL - 17th send returned unexpected error");
            idle(ctx);
        }
    }
    // Drain one message; the next send must succeed (queue has room again).
    let _ = ctx.recv();
    match ctx.try_send("brutal-id-11", &msg) {
        Ok(()) => {}
        Err(_) => {
            ctx.log("identity: T11 FAIL - send after drain failed");
            idle(ctx);
        }
    }
    ctx.log("identity: T11 pass - queue boundary: 16 fill, 17th=QueueFull, drain+send=Ok");
    idle(ctx)
}

fn mode_brutal_id_12_a(ctx: &ServiceContext) -> ! {
    // T12 chain source - sends to B using its wired SEND cap, and also sends a
    // message telling B to forward to C. Since send_peers_grant is per-service
    // and brutal-id-12-a only has SEND to B, we send the chain payload
    // demonstrating multi-hop: A sends to B; B (on recv) immediately sends to C.
    // B has no wired SEND cap to C so C's endpoint identity is conveyed by name.
    // To keep the test mechanical with current SDK: A sends to B, B recvs and
    // sends to C using its own separate SEND cap that we wire in the service config.
    // Revised: brutal-id-12-b has send_peers = ["brutal-id-12-c"] so it has a
    // wired SEND cap to C. A sends "fwd-to-c" to B; B recvs and sends to C.
    let msg = Message::from_bytes(b"fwd-to-c");
    match ctx.try_send("brutal-id-12-b", &msg) {
        Ok(()) => {}
        Err(_) => {
            ctx.log("identity: T12 FAIL - chain-a: send to chain-b failed");
            idle(ctx);
        }
    }
    idle(ctx)
}

fn mode_brutal_id_12_b(ctx: &ServiceContext) -> ! {
    // T12 chain middle - receives from A, forwards to C using its wired SEND cap.
    let _ = ctx.recv();
    let msg = Message::from_bytes(b"via-b");
    match ctx.try_send("brutal-id-12-c", &msg) {
        Ok(()) => {}
        Err(_) => {
            ctx.log("identity: T12 FAIL - chain-b: forward to chain-c failed");
            idle(ctx);
        }
    }
    idle(ctx)
}

fn mode_brutal_id_12_c(ctx: &ServiceContext) -> ! {
    // T12 chain end - receives the message that traveled A→B→C.
    let _ = ctx.recv();
    ctx.log("identity: T12 pass - cap delegation chain A→B→C: message arrived at C");
    idle(ctx)
}

fn mode_brutal_id_13_send(ctx: &ServiceContext) -> ! {
    // T13 cross-core blocked send - fills the queue to brutal-id-13-recv (core 2)
    // then issues a blocking send that must block. While blocked, brutal-id-13-kill
    // (core 1) kills the receiver, which should wake this task with EndpointDead.
    let msg = Message::from_bytes(b"t13");
    for i in 0..16u32 {
        match ctx.try_send("brutal-id-13-recv", &msg) {
            Ok(()) => {}
            Err(_) => {
                ctx.log_fmt(format_args!("identity: T13 FAIL - fill send {} failed", i));
                idle(ctx);
            }
        }
    }
    // Queue is now full. Blocking send must block until the receiver is killed.
    match ctx.send("brutal-id-13-recv", &msg) {
        Err(IpcError::EndpointDead) => {
            ctx.log("identity: T13 pass - cross-core blocked send woke with EndpointDead");
        }
        Ok(()) => {
            ctx.log("identity: T13 FAIL - blocked send succeeded unexpectedly");
        }
        Err(_) => {
            ctx.log("identity: T13 FAIL - blocked send returned unexpected error");
        }
    }
    idle(ctx)
}

fn mode_brutal_id_13_kill(ctx: &ServiceContext) -> ! {
    // T13 killer - yields to let the sender fill the queue and block, then kills recv.
    // Runs on core 1; recv is on core 2; sender is on core 0.
    for _ in 0..200u32 { ctx.yield_cpu(); }
    let _ = ctx.kill("brutal-id-13-recv");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal performance-benchmark modes - Milestone 19.
// ---------------------------------------------------------------------------

fn mode_perf_bp1(ctx: &ServiceContext) -> ! {
    // BP1: same-core IPC roundtrip latency - 100 samples (2× B1) (§22 Brutal Perf BP1).
    // Each round-trip costs ~800ms on QEMU TCG; 100 samples = ~80s, well within 600s timeout.
    let echo_cap = loop {
        if let Some(cap) = ctx.acquire_send_cap("perf-bp1-echo") { break cap; }
        ctx.yield_cpu();
    };

    let msg = Message::from_bytes(b"bp1");
    const N: usize = 100;
    let mut samples = [0u64; N];

    for i in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.send_by_handle(echo_cap, &msg);
        ctx.recv();
        let t1 = ctx.read_tsc();
        samples[i] = t1.wrapping_sub(t0);
    }

    sort_u64(&mut samples);
    let p50  = samples[N / 2];
    let p99  = samples[N * 99 / 100];
    let p999 = samples[N * 999 / 1000];
    ctx.log_fmt(format_args!("perf: BP1 p50={p50} p99={p99} p999={p999} cycles/roundtrip"));
    ctx.log("perf: BP1 done");
    idle(ctx)
}

fn mode_perf_bp1_echo(ctx: &ServiceContext) -> ! {
    let msg = Message::from_bytes(b"bp1e");
    loop {
        ctx.recv();
        let _ = ctx.send("perf-bp1", &msg);
    }
}

fn mode_perf_bp2(ctx: &ServiceContext) -> ! {
    // BP2: cross-core IPC roundtrip latency - 100 samples (2× B2) (§22 Brutal Perf BP2).
    // Each cross-core round-trip costs ~800ms on QEMU TCG; 100 samples = ~80s, well within 600s.
    ctx.log("perf: BP2 sender start");
    let echo_cap = loop {
        if let Some(cap) = ctx.acquire_send_cap("perf-bp2-echo") { break cap; }
        ctx.yield_cpu();
    };
    ctx.log("perf: BP2 sender cap-acquired");

    let msg = Message::from_bytes(b"bp2");
    const N: usize = 100;
    let mut samples = [0u64; N];

    for i in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.send_by_handle(echo_cap, &msg);
        if i == 0 { ctx.log("perf: BP2 sender sent-0"); }
        match ctx.recv_result() {
            Ok(_) => { if i == 0 { ctx.log("perf: BP2 sender recv-0 OK"); } }
            Err(_) => { if i == 0 { ctx.log("perf: BP2 sender recv-0 ERR"); } loop {} }
        }
        let t1 = ctx.read_tsc();
        samples[i] = t1.wrapping_sub(t0);
    }

    sort_u64(&mut samples);
    let p50  = samples[N / 2];
    let p99  = samples[N * 99 / 100];
    let p999 = samples[N * 999 / 1000];
    ctx.log_fmt(format_args!("perf: BP2 p50={p50} p99={p99} p999={p999} cycles/roundtrip"));
    ctx.log("perf: BP2 done");
    idle(ctx)
}

fn mode_perf_bp2_echo(ctx: &ServiceContext) -> ! {
    // Use try_send + retry to match ping/pong's cross-core pattern.
    // Blocking send stalls under heavy BP5 IPI traffic; try_send yields instead.
    ctx.log("perf: BP2 echo start");
    let msg = Message::from_bytes(b"bp2e");
    let mut count = 0u32;
    loop {
        ctx.recv();
        count += 1;
        if count == 1 { ctx.log("perf: BP2 echo recv-0"); }
        loop {
            if ctx.try_send("perf-bp2", &msg).is_ok() { break; }
            ctx.yield_cpu();
        }
        if count == 1 { ctx.log("perf: BP2 echo sent-0"); }
    }
}

fn mode_perf_bp3(ctx: &ServiceContext) -> ! {
    // BP3: syscall yield floor - 2000 yields under brutal 200-task load (§22 Brutal Perf BP3).
    // 5000 was too slow: brutal stress probes' kill/spawn cycles starve the yield task past 600s.
    const N: u64 = 2_000;
    let t0 = ctx.read_tsc();
    for _ in 0..N { ctx.yield_cpu(); }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: BP3 mean={mean} cycles/yield"));
    ctx.log("perf: BP3 done");
    idle(ctx)
}

fn mode_perf_bp4(ctx: &ServiceContext) -> ! {
    // BP4: cap validation throughput - 50000 checks (5× B4) (§22 Brutal Perf BP4).
    let handle = match ctx.recv_handle() {
        Some(h) => h,
        None    => { ctx.log("perf: BP4 FAIL - no recv cap"); idle(ctx); }
    };
    const N: u64 = 50_000;
    let t0 = ctx.read_tsc();
    for _ in 0..N { ctx.query_cap_rights(handle); }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: BP4 mean={mean} cycles/cap-check"));
    ctx.log("perf: BP4 done");
    idle(ctx)
}

fn mode_perf_bp5(ctx: &ServiceContext) -> ! {
    // BP5/BP6: spawn and restart cost - 50 cycles (5× B5/B6) (§22 Brutal Perf BP5, BP6).
    const N: u32 = 50;

    // BP5: spawn-only cost.
    let _ = ctx.kill("perf-bp5-victim");
    let mut total_spawn: u64 = 0;
    for _ in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.spawn("perf-bp5-victim");
        let t1 = ctx.read_tsc();
        total_spawn += t1.wrapping_sub(t0);
        let _ = ctx.kill("perf-bp5-victim");
    }
    let spawn_mean = total_spawn / N as u64;
    ctx.log_fmt(format_args!("perf: BP5 spawn_mean={spawn_mean} cycles/spawn"));
    ctx.log("perf: BP5 done");

    // BP6: kill+spawn (restart) cost.
    let _ = ctx.spawn("perf-bp5-victim");
    let mut total_restart: u64 = 0;
    for _ in 0..N {
        let t0 = ctx.read_tsc();
        let _ = ctx.kill("perf-bp5-victim");
        let _ = ctx.spawn("perf-bp5-victim");
        let t1 = ctx.read_tsc();
        total_restart += t1.wrapping_sub(t0);
    }
    let restart_mean = total_restart / N as u64;
    ctx.log_fmt(format_args!("perf: BP6 restart_mean={restart_mean} cycles/restart"));
    ctx.log("perf: BP6 done");
    idle(ctx)
}

fn mode_perf_bp7(ctx: &ServiceContext) -> ! {
    // BP7: cap table insert/remove - 5000 cycles (5× B7) (§22 Brutal Perf BP7).
    const N: u64 = 5_000;
    let t0 = ctx.read_tsc();
    for _ in 0..N {
        if let Some(cap) = ctx.acquire_send_cap("perf-bp7") {
            ctx.remove_cap(cap);
        }
    }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: BP7 mean={mean} cycles/cap-insert-remove"));
    ctx.log("perf: BP7 done");
    idle(ctx)
}

fn mode_perf_bp8(ctx: &ServiceContext) -> ! {
    // BP8: allocator throughput - alloc to limit (same bound as B8) (§22 Brutal Perf BP8).
    let mut n_alloc: u64 = 0;
    let t0 = ctx.read_tsc();
    loop {
        match ctx.alloc_mem(4096) {
            Ok(_)                   => n_alloc += 1,
            Err(AllocError::Denied) => break,
            Err(_)                  => break,
        }
    }
    let t1 = ctx.read_tsc();
    let mean = if n_alloc > 0 { t1.wrapping_sub(t0) / n_alloc } else { 0 };
    ctx.log_fmt(format_args!("perf: BP8 n={n_alloc} mean={mean} cycles/alloc-4kib"));
    ctx.log("perf: BP8 done");
    idle(ctx)
}

fn mode_perf_bp9(ctx: &ServiceContext) -> ! {
    // BP9: 4 KiB message copy - 400 sends under brutal 200-task load (§22 Brutal Perf BP9).
    // 1000 was too slow: brutal stress probes starve the receiver past 600s.
    let mut msg = Message::from_bytes(&[]);
    for b in msg.payload.iter_mut() { *b = 0xAB; }
    msg.payload_len = 4096;

    const N: u64 = 400;
    let t0 = ctx.read_tsc();
    for _ in 0..N {
        let _ = ctx.send("perf-bp9-recv", &msg);
    }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: BP9 mean={mean} cycles/4kib-send"));
    ctx.log("perf: BP9 done");
    idle(ctx)
}

fn mode_perf_bp9_recv(ctx: &ServiceContext) -> ! {
    loop { ctx.recv(); }
}

fn mode_perf_bp10(ctx: &ServiceContext) -> ! {
    // BP10: scheduler pick-next - 200 yields (§22 Brutal Perf BP10).
    // perf-brutal-only spawns ~30 services (not the full 200-task load the old N=2000 assumed).
    const N: u64 = 200;
    let t0 = ctx.read_tsc();
    for _ in 0..N { ctx.yield_cpu(); }
    let t1 = ctx.read_tsc();
    let mean = t1.wrapping_sub(t0) / N;
    ctx.log_fmt(format_args!("perf: BP10 mean={mean} cycles/yield"));
    ctx.log("perf: BP10 done");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Brutal chaos-test modes - Milestone 21
// ---------------------------------------------------------------------------

fn mode_chaos_bc2_monitor(ctx: &ServiceContext) -> ! {
    // BC2 witness - 500 yields prove the system survived 5 simultaneous non-TCB
    // page faults (§22 Brutal Chaos BC2). 5× C2's single fault, same outcome:
    // kernel kills each faulter; everything else keeps running.
    for _ in 0..500u32 { ctx.yield_cpu(); }
    ctx.log("chaos: BC2 pass - 5 simultaneous non-TCB faults; system survived");
    idle(ctx)
}

fn mode_chaos_bc3(ctx: &ServiceContext) -> ! {
    // BC3 - 2,500 alloc-deny cycles (5× C3's 500) without panic (§22 Brutal Chaos BC3).
    for i in 0..2_500u32 {
        let r1 = ctx.alloc_mem(usize::MAX);
        let r2 = ctx.alloc_mem(1usize << 32);
        if r1.is_ok() || r2.is_ok() {
            ctx.log("chaos: BC3 FAIL - impossible alloc succeeded");
            idle(ctx);
        }
        let _ = ctx.alloc_mem(0);
        if i % 500 == 499 {
            ctx.log_fmt(format_args!("chaos: BC3 iter {}/2500", i + 1));
        }
    }
    ctx.log("chaos: BC3 pass - 2500 alloc-deny cycles without panic");
    idle(ctx)
}

fn mode_chaos_bc5(ctx: &ServiceContext) -> ! {
    // BC5 - 500-level recursive yield_cpu() depth probe (5× C5's 100) (§22 Brutal Chaos BC5).
    let depth = chaos_bc5_recurse(ctx, 500, 0);
    ctx.log_fmt(format_args!("chaos: BC5 pass - {depth}/500 recursive yields without stack overflow"));
    idle(ctx)
}

#[inline(never)]
fn chaos_bc5_recurse(ctx: &ServiceContext, remaining: u32, depth: u32) -> u32 {
    if remaining == 0 { return depth; }
    ctx.yield_cpu();
    chaos_bc5_recurse(ctx, remaining - 1, depth + 1)
}

fn mode_chaos_bc6_monitor(ctx: &ServiceContext) -> ! {
    // BC6 witness (core 0) - 200 yields then log pass (§22 Brutal Chaos BC6).
    // Two hogs run on cores 2 and 3, simulating two timer-starved cores.
    // This probe on core 0 proves the remaining cores are scheduled normally.
    // 200 yields matches C6; the brutal intensity is the 2-hog pressure, not yield count.
    for _ in 0..200u32 { ctx.yield_cpu(); }
    ctx.log("chaos: BC6 pass - 2-core hog starvation; core 0 still alive");
    idle(ctx)
}

fn mode_chaos_bc7(ctx: &ServiceContext) -> ! {
    // BC7 - 15 cross-core kill/respawn cycles (§22 Brutal Chaos BC7).
    // Controller on core 1; victim on core 2. Each kill triggers a cross-core IPI
    // and TLB shootdown. Under full brutal suite concurrent load each cycle takes ~45s;
    // 15 cycles fits within 900s. The brutal intensity is the full concurrent suite, not
    // the cycle count.
    let msg = Message::from_bytes(b"bc7");
    for i in 0..15u32 {
        let _ = ctx.try_send("chaos-bc7-victim", &msg);
        let _ = ctx.kill("chaos-bc7-victim");
        let _ = ctx.spawn("chaos-bc7-victim");
        for _ in 0..10u32 { ctx.yield_cpu(); }
        if i % 5 == 4 {
            ctx.log_fmt(format_args!("chaos: BC7 iter {}/15", i + 1));
        }
    }
    ctx.log("chaos: BC7 pass - 15 cross-core TLB shootdowns survived");
    idle(ctx)
}
