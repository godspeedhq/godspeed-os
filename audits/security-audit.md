# Security Audit (living)

The fourth of the audit family, alongside [`kernel-audit.md`](kernel-audit.md),
[`userspace-audit.md`](userspace-audit.md), and [`documentation-audit.md`](documentation-audit.md).
Where those pin correctness, Commandment-compliance, and clarity, this one pins **authority**.

## North-star

**No principal gains authority beyond what it was granted.**

On Linux, "escalation" means reaching a single global principal: root. GodspeedOS has no root - authority
is distributed across a handful of principals, so escalation means **reaching the authority of a
deputy**. The deputies, in descending power:

- **kernel** - the only true root (ring 0, mints every cap, owns memory). Smallest, hardest.
- **supervisor** - `SERVICE_CONTROL` + `SPAWN` (kill/restart/spawn anything).
- **fs** - `RESOURCE_MINT` (owns the meaning of files, mints file caps).
- **shell** - the capability *broker*: `SPAWN` + `SERVICE_CONTROL` + `ACQUIRE_ANY` + `REBOOT` + reach-any-service-by-name + write/delete any file.
- **DMA drivers** (`xhci`/`ehci`) - can DMA within (or, for `ehci`, outside) their arena.

Caps are unforgeable (kernel-minted `ResourceId+Rights+Generation`, generation-checked), and that is
already pinned by the adversarial suite (A1/A2). So this audit does **not** re-prove forgery is hard. It
assumes the attacker cannot forge and asks the only question that remains: **can a caller become, or
fool, a deputy that already holds the authority?**

## Why now, and what we are defending against

Recent AI models find real kernel vulnerabilities by taking a bounded slice of code and reasoning about
it: Google's *Big Sleep* found an exploitable buffer underflow in SQLite; OpenAI's o3 found a Linux
kernel use-after-free in ksmbd (CVE-2025-37899) by reasoning about a race across concurrent connection
handlers. The methodology generalizes - the same pattern has been reused to find further bugs.

That pattern runs on two fuels: **(1) memory-unsafe code** and **(2) complex concurrent shared state.**
GodspeedOS starves fuel #1 by construction - safe Rust makes use-after-free and overflow impossible
outside the four `unsafe` layers (`arch/`, `memory/`, `capability/`, `smp/`) plus the DMA drivers. So
the corruptible surface is small and finite; the fight narrows to **fuel #2 (concurrency / interleavings)**
and to **logic (confused-deputy / gate-scoping)**. This audit is organized around exactly what a
powerful model hunting that pattern would target, and its goal is to drive each vulnerability class to
its floor *before* someone runs the pattern on the public repo.

## Method

Source-only reasoning (no build, no QEMU - matches the reasoning nature of the work). Six parallel
readers: five per-principal (supervisor, fs, shell, kernel cap-logic, DMA/user-copy seam) plus one
adversarial **concurrency/interleaving hunt** running the ksmbd methodology over the kill/reclaim,
cap-revoke, IPC-death, and SMP paths. Each enumerated authority held, who can reach it, and where
attacker-controlled input meets that authority; findings are traced to concrete `file:line` and marked
by confidence. The one HIGH finding was independently re-verified against source before recording.

## Severity

- **HIGH** - a reachable path to authority-beyond-grant, or ring-0 memory corruption (caps become moot).
- **MED** - a real weakness with a bounded precondition (needs a compromised driver, a specific op sequence, or a non-default holder), or a latent defect one edit away from HIGH.
- **LOW** - a smell, a DoS bounded by restartability, or an information-only hazard.

Confidence: **CONFIRMED** (traced end to end in source) / **PLAUSIBLE** (mechanism confirmed, full window depends on reachability argued but not executed) / **SPECULATIVE** (flagged, not traced to a concrete case).

---

## Audit 1 - 2026-07-16 (TCB-principal threat model, first pass)

**Result: 1 HIGH, 5 MED, 8 LOW.** The core is sound where it matters most: the **generation/ResourceId
reuse invariant holds** (no old cap can validate against a new resource), the **syscall user-copy seam
has no gap**, the **supervisor exposes no command-RPC surface**, band ownership is enforced, and rights
non-escalation is enforced at both the kernel and fs layers. Most of the concurrency surface was traced
**race-free**. The findings cluster into three stories (below); the standout is a single HIGH
memory-safety UAF in the scheduler - mechanically fixable, and the whole yield of a full concurrency
hunt over the kernel.

### Ranked ledger

| ID | Sev | Conf | Principal | Finding | Fix direction |
|----|-----|------|-----------|---------|---------------|
| **SEC-1** | HIGH | CONFIRMED (defect) / PLAUSIBLE (window) | kernel/smp | **Freed-CR3 UAF on cross-core kill.** `yield_current` (`scheduler.rs:1453`) and `block_and_reschedule` (`:2104`) claim `next` with a plain `store(Running, Relaxed)` and switch in, with no re-read/abort - unlike the hardened `run` (`:1006`) and `timer_tick_from_irq` (`:1276`). A `Ready` task cross-killed by another core can be switched into after its PML4 is reclaimed and freed -> `switch_context` loads a freed page-table root. | Port the `timer_tick_from_irq` handshake verbatim: CAS `Ready->Running` + publish `CORE_CURRENT` SeqCst + `fence(SeqCst)` + re-read `STATE` + `abort_to_sched`. |
| **SEC-2** | MED | CONFIRMED (mechanism) | shell / drivers | **CONSOLE_PUSH confused-deputy.** `xhci`/`ehci` hold `CONSOLE_PUSH` (+ `REBOOT`) and can inject *arbitrary* bytes into the shell input ring (`dispatch.rs:1688`), driving the entire broker surface (kill/spawn/reach-any/write-any-file/reboot). A compromised USB driver escalates through the shell to ~full system authority. Directly tensions §6.4's claim that an IOMMU-confined USB driver is "genuinely least-privilege." | Account `CONSOLE_PUSH` holders inside the shell's trust perimeter in §6.4, or add a trust boundary (secure-attention path / decoded-keystroke-only channel) so a confined driver cannot type arbitrary commands. |
| **SEC-3** | MED | CONFIRMED (posture) | drivers / iommu | **`ehci` runs IOMMU passthrough** (`task/mod.rs:3699-3711`) - a parser slip in `ehci` is uncontained DMA-anywhere, unlike arena-confined `xhci`. The highest-value memory-safety target. Accepted per §6.4 (ehci in TCB on those machines) but it is the first place a device-input bug becomes full-RAM read/write. | Harden/fuzz the `ehci` descriptor walk (`ehci/src/main.rs:531-549`) first; prefer bounds-checked accessors there before anywhere else. |
| **SEC-4** | MED | CONFIRMED (latent) | sdk | **"Safe" `Dma`/`Mmio` wrappers do no bounds check.** `Dma::writeN(off)` is `base.add(off)` with only a comment promising range (`dma.rs:56-109`); `Mmio` has no `len` at all (`mmio.rs:30-84`). §18.1's memory-safety-behind-safe-wrappers claim rests entirely on author discipline. All live sites bounded today; one edit from a cross-arena (for ehci, DMA-anywhere) write. | Add `assert!(off + size <= self.len)` to the `Dma` accessors; give `Mmio` a `len` and the same assert. Cheap; turns a future slip into a loud one-service panic. |
| **SEC-5** | MED | CONFIRMED | fs | **`delete_tree` / dir rename+move don't revoke descendant file caps.** `revoke_open_by_path` matches exact path only (`fs/src/main.rs:2056`). Descendant caps survive: (a) `open_files` slot leak -> `MAX_OPEN=64` exhaustion -> Open DoS (the fs case of userspace-audit **F1**, and the leading root-cause hypothesis for stress finding **LS1**); (b) escalation: a surviving cap re-resolves to a **recreated** file at the same path (authority beyond grant; also violates §7.5 / §22 Test 14, which requires `CapRevoked`). | `revoke_open_by_path` should match on subtree **prefix** for `delete_tree`/dir-move, or revoke every `open_files` entry under the affected subtree. |
| **SEC-6** | LOW-MED | CONFIRMED | kernel | **`AcquireSendCap` GRANT bit is caller-chosen** for a merely-*declared* send-peer (`dispatch.rs:842-847`). A service can self-mint `SEND\|GRANT` to a declared peer and re-delegate send authority the contract never intended to be re-delegatable. | Condition the GRANT bit on a contract flag (a `re-delegate` permission), not `arg2`. |
| **SEC-7** | LOW-MED | CONFIRMED | fs / kernel | **fs hands every file cap with GRANT** (`fs/src/main.rs:2018`); the kernel copies rights verbatim on transfer and never strips GRANT, so fs *cannot* hand out a non-grantable delegated cap through the embed path. Re-delegation breadth (not rights widening). Contradicts §8.5 "don't pass GRANT onward." | Offer a kernel transfer variant that narrows/strips GRANT, so the safe path is the default; have fs mint the client copy without GRANT. |
| **SEC-8** | LOW-MED | CONFIRMED | fs | **`FOP_CLOSE` ignores the badged right** (`fs/src/main.rs:941-946`): any holder (even READ-only or zero-right) revokes the resource for *all* holders. Cross-holder revocation/DoS. | Gate `FOP_CLOSE` on an appropriate right, or scope close to the caller's own cap rather than the shared resource generation. |
| **SEC-9** | LOW | PLAUSIBLE | kernel/smp | **Cross-kill racing self-kill** frees a victim's kstack/PML4 while the victim's own core may still be on it (`scheduler.rs:1937,2032`). Timing-benign today (the killer's long reclaim walk almost always lets the self-killer leave the stack first), but classification is made by the killer while the stack-occupancy risk belongs to the victim. | Have the kill spin-wait/defer gate on the victim core having completed `switch_context`, not merely changed `CORE_CURRENT`. |
| **SEC-10** | LOW-MED | PLAUSIBLE | kernel | **Torn cross-core `TASK_NAME` read.** `&str` is two words; `task_stat` reads it non-atomically while a concurrent `commit_task` writes it (`scheduler.rs:830`). Introspection-only over-read of adjacent rodata (not a privilege break); the "naturally-atomic" SAFETY comment is wrong for a two-word `&str`. | Gate `task_stat`'s `TASK_NAME` read on an Acquire load of `TASK_STATE`, matching the publish order in `commit_task`. |
| **SEC-11** | LOW | CONFIRMED | kernel | **`holds_resource` skips the generation check** (`table.rs:111-120`) - safe today because all gate-resources are stable gen-0 IDs, latent if any becomes revocable. | Pin an invariant/test: no `holds_resource`-gated resource is ever passed to `revoke_resource`/`mark_dead_resource`. |
| **SEC-12** | LOW | CONFIRMED | kernel | **COM2 control channel is fully ungated** (`control.rs:79-124`): kills/spawns/fires-IRQs with no cap check. **Not** reachable by any in-system service (no service holds a port cap) - exposure is physical serial / the harness only. | Feature-fence `RESTART`/`KILL`/`FIRE_IRQ` out of production bare-metal builds, or document it as an accepted physical-access authority. |
| **SEC-13** | LOW | CONFIRMED | shell | **`spawnwired`/`spawncap` leak GRANT to a child** (`shell/src/main.rs:6119-6128`) - diagnostics, fixed targets (not steerable). | Confirm they are absent from the bare-metal build; if present, mint the child cap SEND-only. |
| **SEC-14** | LOW | CONFIRMED | loader | **ELF loader copy** cites but does not (in this pass) confirm `p_offset + p_filesz <= bytes.len()` before `copy_nonoverlapping` (`loader.rs:224-231`). Build-embedded ELFs, not runtime-syscall input (off the live user-copy seam); the F3 fuzz surface. | Confirm the program-header loop rejects `p_offset + p_filesz > bytes.len()` before the copy. |

### The three stories

**Story A - the USB-driver escalation chain (SEC-3 + SEC-4 + SEC-2).** This is the #1 real-world break
and it stitches three findings into one path: a malicious USB device feeds a parser slip in `ehci`
(SEC-4: the "safe" `Dma` wrapper is unchecked) which, because `ehci` runs passthrough (SEC-3), is
uncontained DMA-anywhere and compromises the driver; the compromised driver then uses its held
`CONSOLE_PUSH` (SEC-2) to type arbitrary commands into the shell and inherit the broker's authority. The
DMA confinement we were proud of does **not** close this: for `ehci` it isn't even applied, and the
final hop into the shell is a *held capability*, not DMA. It also exposes a genuine constitutional
tension - a §6.4 "confined, least-privilege, non-TCB" USB driver still holds `CONSOLE_PUSH` and `REBOOT`.

**Story B - the scheduler UAF (SEC-1).** The lone HIGH, and standalone: a concurrency use-after-free of
the CR3 root, the exact class the AI pattern mines in Linux C - found here in one place, with a
mechanical fix the codebase already applies in the two sibling paths. This is the "latent true-concurrency
reclaim UAF" previously logged as a follow-up (see [`kernel-audit.md`], the kill-path PF guard), now
traced to a concrete two-core interleaving and root cause.

**Story C - fs descendant revoke (SEC-5).** One finding, two faces: the leading root-cause hypothesis
for the **LS1** long-soak `ls` degradation (an `open_files` slot leak whose fs-restart recovery signature
matches), *and* a real revocable-property/aliasing escalation. Fixing the subtree revoke closes both.

### Per-principal summary

| Principal | Authority held | Reachable by | Verdict |
|-----------|----------------|--------------|---------|
| supervisor | SERVICE_CONTROL, SPAWN, INTROSPECT, ACQUIRE_ANY | kernel death-notify (gated); no command-RPC surface | **Clean** - no untrusted-reachable escalation |
| kernel cap-logic | mints/validates all caps | every service via syscalls | **Sound** - reuse invariant holds; only SEC-6/11 smells |
| fs | RESOURCE_MINT | any fs-send-cap holder (ambient within fs, by design) | SEC-5/7/8; boundary is fs-send-cap vs file-cap holders |
| shell | SPAWN, SERVICE_CONTROL, ACQUIRE_ANY, REBOOT, reach-all | physical console + `CONSOLE_PUSH` holders (xhci/ehci) | SEC-2 - broker is only as trusted as its input drivers |
| DMA seam | MMIO/DMA per driver | malicious USB device (device-input parsers) | SEC-3/4; user-copy seam itself clean |

### Verified sound / race-free (do not re-hunt)

- **Generation / ResourceId reuse across time.** Endpoint gens come from a global strictly-monotonic counter that never repeats and panics on wrap; delegated-resource records are append-only (no slot ever cleared), so a reused id never resets to gen 0 while a stale cap exists. Cross-resource gen equality is irrelevant (gens compared only within a `ResourceId`). The linchpin invariant holds.
- **Syscall user-copy seam.** Every user pointer/length funnels through three audited wrappers; `read_user_bytes` returns a slice into per-core kernel scratch (never raw user memory); a bad user pointer is a caller-kill via the `USER_COPY_ACTIVE` guard, not a kernel halt. No dispatch handler bypasses them. `SpawnWithCaps` bounds-checks every descriptor field.
- **Band ownership + badge trust.** Per-slot owner tracking; invoke routes to the true owner; the badge is kernel-set only after cap validation and cleared per message - un-forgeable over a plain `send`.
- **Rights non-escalation.** Kernel copies caps verbatim (never wider); Open masks to READ|WRITE and non-escalation is enforced at both the kernel (`CapInsufficientRights`) and fs (`op <= right`) layers.
- **Concurrency, traced race-free:** cap-validate-vs-revoke TOCTOU (closed by a second lock at enqueue), enqueue-into-draining-queue, the reply-side death-wake (`CALL_AWAIT_EP`), `block_and_reschedule` lost-wakeup (for the *blocking* task), the TLB-shootdown protocol and its deadlock-breaker, the deferred self-kill PML4/kstack (per-core single-owner), and the frame allocator (fully serialized, rejects phantom/double frees).

### Constitutional note

SEC-2 surfaces a spec-vs-implementation tension worth resolving deliberately (§26.3: a gap is either
fixed or the constitution is amended). §6.4 treats an IOMMU-confined USB driver as least-privilege and
drops it from the TCB, but that same driver holds `CONSOLE_PUSH` + `REBOOT` over the shell. Either those
capabilities belong inside the shell's trust perimeter (and §6.4 should say so), or a confined driver
should not be able to drive arbitrary shell commands. Not resolved here; recorded for decision.

---

## Audit 1b - 2026-07-16 (assumption-challenge ledger)

Driven by a ~130-question challenge checklist across 18 categories (the "challenge assumptions, not
merely find bugs" pass). Four new source-only readers closed the categories Audit 1 did not reach:
**Interrupts**, **Boot / Recovery / Failure semantics**, **Resource-Exhaustion / Information-Disclosure**,
and the **Architecture Layer** (a portability-security pass on memory ordering). Every question is
answered below with a verdict and evidence.

**Delta over Audit 1: +1 HIGH (SEC-18), +1 reachable MED info-leak (SEC-21), a portability-latent race
class (SEC-25..28, safe on x86 today), and LOW hygiene items.** Cumulative: **2 HIGH** (SEC-1 freed-CR3
UAF; SEC-18 panic-does-not-halt), the Story-A chain, SEC-5, SEC-21, plus the portability set and LOWs.

The **portability-latent** class deserves its own note: these are `Relaxed`-ordering / x86-TLB / DMA-
coherence assumptions that are **correct on x86's strong (TSO) memory model today** and become **real
races the day the neutral kernel runs SMP on AArch64 / RISC-V**. They are not live x86 bugs; they are
port blockers, the same failure class as SEC-1, and belong on the aarch64 critical path.

### New findings (SEC-15 .. SEC-28)

| ID | Sev | Conf | Area | Finding | Fix direction |
|----|-----|------|------|---------|---------------|
| **SEC-18** | HIGH | CONFIRMED | kernel/arch | **A panic does not halt the system.** `halt_all_cores()` (`arch/x86_64/mod.rs:301-308`) is `cli`+`hlt` on the *calling core only*; its own comment admits the NMI broadcast is unfinished ("Milestone 6: broadcast NMI IPI"). The panic handler (`main.rs:334-338`) calls it, so other cores keep running on the shared state whose corruption triggered the panic, and a lock the dead core held live-wedges survivors. Contradicts §6.2 / §19. | Broadcast an NMI/IPI to all cores in the panic path (the promised, absent code) before halting; the receiving cores `cli`+`hlt`. |
| **SEC-21** | MED | CONFIRMED | kernel/memory | **AllocMem returns non-zeroed frames -> cross-task info disclosure.** `handle_alloc_mem` (`dispatch.rs:1245-1259`) maps `alloc_frame()` pages with no zeroing; the allocator zeroes neither on alloc nor free (`allocator.rs:216-242,316-381`). AllocMem needs **no capability**, so any service can read a dead service's stale frame contents before overwriting. Spawn ELF/stack/PT frames *are* zeroed. | `write_bytes(dst,0,PAGE_SIZE)` per AllocMem frame (as loader/stack already do), or zero-on-free. |
| **SEC-25** | MED (portability-latent) | CONFIRMED | smp/arch | **Task-slot publication is `Release`-store / `Relaxed`-load.** Spawn writes plain `TASK_*` fields then `TASK_VALID.store(true, Release)` (`scheduler.rs:547-551`); ~30 readers `TASK_VALID.load(Relaxed)` then read the plain fields / `TASK_CTX`. No synchronizes-with on weak arches -> observe `VALID=true` with stale CR3/kstack. Same UAF class as SEC-1; x86-safe. | Pair the `Relaxed` loads with `Acquire` (or an acquire fence) on the weak-arch ports. |
| **SEC-26** | MED (portability-latent) | CONFIRMED | task/arch | **Kill-path elides the TLB shootdown on an x86 assumption** (`scheduler.rs:1942-1950`: "a CR3 reload flushes non-global TLB"). False for ARM/RISC-V ASID-tagged switches -> stale translation to a reclaimed frame. | Issue an arch-appropriate flush/shootdown through the seam on the ports. |
| **SEC-27** | MED (portability-latent) | CONFIRMED | arch | **`arch::imp` seam pins names, not semantics.** `write_page_table_base` / `invalidate_tlb_page` (`page_tables.rs:261-287`) have divergent flush/broadcast semantics per arch; neutral callers assume the x86 shape. No trait/contract to catch it. | Document/encode the seam's semantic contract (barrier + flush + broadcast obligations per primitive). |
| **SEC-28** | MED (portability-latent) | CONFIRMED | sdk/arch | **SDK assumes DMA cache-coherence.** `sdk/rust/src/dma.rs:12-13` maps DMA buffers cacheable with no maintenance ("x86 DMA is cache-coherent"); false on non-coherent ARM. `docs/aarch64.md` flags §6.4/SMMU non-portable but not this SDK coherence assumption. | Add a cache-maintenance hook the DMA accessors call on non-coherent arches. |
| **SEC-15** | LOW | CONFIRMED | interrupts | **`fire_test_irq` force-enables IF inside the timer ISR** (`interrupts.rs:373-380`) via unconditional `enable_interrupts()`, opening ISR-stack re-entrancy. Reached only via the FIRE_IRQ harness command. | Use `local_irq_save/restore`; confirm FIRE_IRQ is compiled out of production supervisor builds. |
| **SEC-16** | LOW | CONFIRMED | interrupts | **`route::register` silently overwrites an existing IRQ route** (`route.rs:27-29`) - no two-driver-claims-one-IRQ detection (violates loud-failure). Unreachable today (distinct vectors). | Reject a duplicate IRQ claim loudly. |
| **SEC-17** | LOW | CONFIRMED | interrupts | **MSI/edge vectors have no kernel-side storm rate-limit** (`route.rs:79` coalesce only; `mask_vector` no-op for edge/MSI). The liveness watchdog panic is the loud backstop; no graceful backpressure. | Consider a per-vector rate threshold before the watchdog. |
| **SEC-19** | LOW | CONFIRMED | kernel | **Memory budget seeded after the task is schedulable.** `set_task_memory_budget` (`task/mod.rs:3799`) runs after `commit_task` publishes `Ready` (`3790`); a task run on another core in that window reads the previous occupant's `TASK_LIMIT_BYTES` (`scheduler.rs:759-777`). | Seed the budget before `commit_task`. |
| **SEC-20** | LOW (doc) | CONFIRMED | docs | **Crash-page doc-drift.** §19 and `kernel/CLAUDE.md` describe a reserved crash page that survives reboot and is re-read on next boot; neither the write nor the read exists in code (removed with `init`, Phase 5). Also: supervisor `reconcile` logs only on respawn *success*, not on a failed one (`supervisor/main.rs:250-254`). | Correct the docs (or implement); log the failed-respawn case. |
| **SEC-22** | LOW | PLAUSIBLE | memory | **No recovery reservation.** Global first-come pools (frames `allocator.rs:484`; task slots `scheduler.rs:31/451`; kstacks `task/mod.rs:27/106`) mean exhaustion can fail the supervisor respawn path itself. Loud + SPAWN-gated + distinct-name-bounded, but not guaranteed-recoverable. | Reserve headroom (a frame/slot/kstack) for the recovery path. |
| **SEC-23** | LOW | CONFIRMED | kernel | **Cap-mismatch logging discloses resource IDs + generations** (`table.rs:56-66`) to the serial + ring-buffer log stream. Not an authority leak (a log reader still cannot forge a cap). | Drop or rate-limit the numeric detail. |
| **SEC-24** | LOW | CONFIRMED | memory | **A task's own intermediate page-table frames are uncounted** against `TASK_ALLOC_BYTES` (`page_tables.rs:474` vs `scheduler.rs:768-771`). Bounded (contiguous VA), minor quota bypass. | Count PT frames against the budget, or document the allowance. |

### Question-by-question ledger

Verdict key: **SAFE** (no issue) / **BY-DESIGN** (intentional, documented) / **FINDING** (-> SEC-N) /
**NEEDS-CHECK** (bounded, verify) / **N/A** (mechanism absent).

**Syscall Surface**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Validate all user args? | SAFE | 3 audited copy wrappers; every handler bounds its length band |
| Malformed pointer escape? | SAFE | `validate_user_ptr` + `USER_COPY_ACTIVE` fault-guard (caller-kill) |
| Zero-length UB? | SAFE | `read_user_bytes` rejects zero-len; recv/send require len in (0,MAX] |
| Invalid processor state? | SAFE | syscall entry masks IF (SFMASK), swapgs, dedicated kstack (`syscall_entry.rs`) |
| Optional args truly optional? | SAFE | positional u64s validated per-handler; unused args ignored |
| Succeed with partially-valid input? | SAFE | `SpawnWithCaps` bounds each field before acting; embedded-cap move is atomic |
| Validation itself overflow? | SAFE | copy path checks `ptr+len` wrap; accounting uses checked/saturating (SEC-19 is ordering, not overflow) |
| Reserved values rejected? | SAFE | unknown syscall nr -> `UnknownSyscall` (F2); reserved gen/rights validated |

**Capability Model**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Any capability forged? | SAFE | kernel-only construction; random handle -> `CapNotHeld` (A1/A2) |
| Silently gain privileges? | SAFE | rights copied verbatim; `narrow` = pure intersection, never widened |
| Revocation race with usage? | SAFE | validate re-checked under the enqueue lock; kill bumps gen under same lock |
| Revoked cap completes in-flight? | SAFE | IPC re-checks liveness at enqueue; (SEC-8 is close-initiation, not in-flight) |
| Duplicated without authority? | SAFE | derive/transfer require GRANT and *move*; SEC-6/7 are GRANT-breadth smells |
| Leak through logging? | FINDING | SEC-23 (resource id + gen logged; not an authority leak) |
| Inheritance accidental? | SAFE | child caps minted from child's static contract, never inherited ambient |
| Stale mistaken for live? | SAFE | generation check + reuse invariant (monotonic, append-only) |
| Outlive owner? | FINDING | SEC-5 (fs file caps survive `delete_tree` of an ancestor) |
| Two identities share a cap? | SAFE | per-slot band owner; SEC-8 is a deliberately-shared cap, not accidental |

**Memory Safety**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Integer overflow -> wrong alloc size? | SAFE | `checked_add(4095)`/`saturating_add` on the live path (`scheduler.rs:768`) |
| Arithmetic wrap silently? | SAFE | generation wrap panics (H7); accounting saturates-to-reject |
| Wrong alignment alloc? | SAFE | frame allocator is page-granular; 4 KiB masks (portability note only) |
| Freed memory observable? | FINDING | SEC-1 (freed CR3), SEC-21 (non-zeroed AllocMem frames) |
| Ownership ambiguous? | FINDING | SEC-1 / SEC-9 (cross-core kill/reclaim); else per-task clear |
| Kernel memory mapped to userspace? | SAFE | AllocMem maps only allocator frames USER; `free` rejects kernel-PT frames |
| Overlapping mappings? | SAFE | per-task page tables; VA bump-allocated per task (no cross-task overlap) |
| Permissions grow over time? | SAFE | flags fixed at map; no re-protect-looser path (W^X: user pages NO_EXEC) |
| Used after destruction? | FINDING | SEC-1 |
| Refcount under/overflow? | N/A | generation-based, not refcounted; frame double-free absorbed idempotently |

**Object Lifetime**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Outlive owner? | FINDING | SEC-5, SEC-1 |
| Destruction twice? | SAFE | allocator absorbs double-free; SEC-9 (kstack/PML4) is the LOW edge |
| Destruction race with use? | FINDING | SEC-1 (HIGH) |
| Stale handle -> recycled object? | SAFE | reuse invariant (generation); SEC-10 torn-read is introspection-only |
| Waiter survive its object? | SAFE | reply-death wake + blocked-sender EndpointDead traced race-free |
| Callbacks after destruction? | N/A | no callback mechanism |
| References escape lifetime? | FINDING | SEC-1 |
| Cleanup fail silently? | FINDING | SEC-5 (silent slot leak); kernel-audit T1 (PT-frame leak on partial spawn) |

**Identity**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Identity confused with impl? | SAFE | name is stable + ELF-bound; adopt-by-name refuted as impostor vector |
| Generation numbers wrap? | SAFE | panics loud, never wraps (H7) |
| Stale identifiers reused? | SAFE | reuse invariant (append-only + monotonic) |
| Endpoint identity change silently? | SAFE | death bumps generation -> loud `EndpointDead` |
| Compare addresses not identity? | SAFE | compared by ResourceId+generation values (SEC-10 is a torn value read) |
| Survive restart incorrectly? | SAFE / FINDING | reconcile refuted; SEC-5 is the cap-survives-recreate aliasing case |
| Identity duplicated? | SAFE | kernel singleton guard (one live instance per name) |

**IPC**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Deadlock permanently? | BY-DESIGN | kernel doesn't detect; §8.9 makes it the protocol author's duty (`try_send`) |
| Messages replayed? | SAFE | copy-once into a bounded queue; no replay surface |
| Arrive after endpoint destruction? | SAFE | enqueue re-checks liveness under the TABLE lock |
| Ordering assumptions violated? | BY-DESIGN | FIFO per queue; no cross-core total order (§8.8) |
| Queues grow unbounded? | SAFE | fixed depth 16, static memory |
| One service starve another? | SAFE | bounded queue + block; scheduler preempts (A8) |
| Endpoint ownership ambiguous? | SAFE | one owning task per endpoint (P5) |
| Malformed messages bypass validation? | SAFE | body opaque bytes <=4 KiB copied; embedded caps validated |
| Partial completion -> inconsistent state? | SAFE | send is atomic (queued or error); embedded-cap move atomic |

**Scheduler**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Permanent starvation? | SAFE | 10 ms preemption (Test 8) |
| Priority inversion? | N/A | round-robin, no priorities |
| State inconsistent? | FINDING | SEC-1, SEC-9 |
| Runnable tasks disappear? | FINDING | SEC-1 (killed-task race) |
| Blocked tasks wake twice? | SAFE | `block_and_reschedule` lost-wakeup CAS; `wake_by_slot` guards |
| Metadata leak between tasks? | FINDING | SEC-10 (torn `TASK_NAME`); `CORE_DEAD_CTX` never a load source |
| Malicious workload exhaust fairness? | SAFE | preemption enforced regardless of yield |

**Interrupts**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Arrive during inconsistent state? | SAFE | every critical section runs IF=0 (entry/switch/routing/kill) |
| Nested interrupts violate invariants? | FINDING | SEC-15 (`fire_test_irq` IF inversion, test-only) |
| Acknowledged incorrectly (EOI)? | SAFE | device IRQ EOI after handling; spurious 0xFF no-EOI no-op |
| Ownership confused? | FINDING | SEC-16 (silent route overwrite); reused-endpoint hazard closed |
| Storms exhaust recovery? | FINDING | SEC-17 (no MSI rate-limit; watchdog-panic backstop) |
| Masking persists unintentionally? | SAFE | `local_irq_save/restore` nest; no leaked IF=0 (SEC-15 is a leaked IF=1) |

**Architecture Layer** (portability-security; safe on x86 today)

| Question | Verdict | Evidence |
|----------|---------|----------|
| Arch assumptions leak into neutral code? | FINDING | SEC-26 (TLB elision); 4 KiB granule unstated but valid on all targets |
| Implementations expose different semantics? | FINDING | SEC-27 (seam pins names not semantics) |
| Page-table assumptions diverge? | SAFE | PTE/4-level/canonical-VA confined to `arch/x86_64/` |
| Cache maintenance differ silently? | FINDING | SEC-28 (SDK DMA coherence assumption) |
| Atomics differ across ISAs? | SAFE | `portable_atomic::AtomicU64` everywhere; gap is ordering not availability |
| Memory barriers omitted? | FINDING | SEC-25 (task-slot Release/Relaxed publication) |
| Exception entry violate assumptions? | SAFE | entry establishes GS/RSP/IF x86-appropriately; neutral code uses abstractions |

**Boot**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Continue after failed init? | SAFE | AP-fail continues degraded; supervisor-spawn-fail panics; BSP init faults loud |
| Trust widened unnecessarily? | SAFE | least-privilege from `service_privileges`/`service_hw`; probe grants are test-only |
| Bootstrap state stays mutable? | SAFE | immutable-after-boot facts behind atomics/arenas; `static mut` are working memory |
| Partially-init services visible? | FINDING | SEC-19 (budget after `Ready`); endpoint-before-`Ready` is benign (not schedulable) |
| Init order hide dependency cycles? | SAFE | strictly linear spawn (block-driver->fs->shell); non-blocking, no circular wait |
| Config drift from runtime truth? | SAFE | `contract_check.py` reconciles kernel tables vs `.toml` (T1) |

**Recovery**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Preserve stale state? | SAFE | fresh instance + fresh endpoint at higher gen; `converge` re-checks real liveness |
| Duplicate ownership? | SAFE | singleton guard + adopt-if-running (can't both land) |
| Resurrect invalid objects? | SAFE | adopted stale cap -> `EndpointDead`; `converge` respawns the actually-dead name |
| Hide failure? | SAFE | bounded retries then loud "restart FAILED"; minor: SEC-20 (reconcile logs only success) |
| Violate capability boundaries? | SAFE | adopt via `ACQUIRE_ANY`; respawn caps minted fresh from static tables |
| Two authoritative owners? | SAFE | one live task per name; unregister-then-free ordering |
| Silently weaken security? | SAFE | respawn caps from the same static tables; installs GRANT-validated, only narrow |

**Failure Semantics**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Every failure an honest state? | FINDING | SEC-18 (panic does not halt other cores) |
| Partial failure appear as success? | SAFE | spawn errors -> `cleanup_partial_spawn` + Err; `Ready` published last |
| Panic expose sensitive state? | SAFE | prints only `KERNEL PANIC: {info}`; no secret store dumped (crash page not impl - SEC-20) |
| Panic corrupt ownership? | SAFE | panic path frees nothing / touches no tables (SEC-18 is the survivors, not the panic) |
| Panic leave inconsistent metadata? | FINDING | SEC-18 (a lock held by the dead core live-wedges survivors) |
| Panic recovery reintroduce stale state? | SAFE | no crash-page re-read exists (SEC-20 doc-drift only) |

**Resource Exhaustion**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Exhaustion -> escalation? | SAFE | alloc-fail returns typed error; no partial-state-as-valid; `AllocDenied` vs kill distinct |
| Endpoint exhaustion deny indefinitely? | SAFE | IDs reclaimed ABA-safe; distinct-name + `AlreadyRunning` bound live count < 96 |
| Malicious alloc starve critical services? | NEEDS-CHECK | SEC-22 (no global reservation; per-task limit enforced but aggregate can exceed RAM) |
| One service monopolize kernel objects? | SAFE | cap slots per-task (64); task/kstack pools SPAWN-gated + name-bounded |
| Descriptor exhaustion prevent recovery? | NEEDS-CHECK | SEC-22 (exhaustion can fail the respawn path) |
| Accounting overflow? | SAFE | live path checked/saturating; dead `ownership.rs` unchecked `+` has no callers |
| Quotas bypassed? | FINDING | SEC-24 (task's own PT frames uncounted); SEC-19 (stale budget window) |

**Information Disclosure**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Kernel memory leak through errors? | SAFE | errors are small fixed ints; introspection out-buffers zero-init, prefix-only |
| Timing reveal privileged info? | BY-DESIGN | validation not constant-time; discloses nothing not already held (§20, A7) |
| Object ids reveal topology? | SAFE | ids sequential (mild hint) but authority needs the gen; counts INTROSPECT-gated |
| Debug logs expose capabilities? | FINDING | SEC-23 (ids+gen logged; not forgeable) |
| Stale memory expose secrets? | FINDING | SEC-21 (non-zeroed AllocMem); spawn/IPC/introspection paths all zeroed |
| Panic messages leak impl details? | BY-DESIGN | prints kernel addrs to serial; serial ~= operator access; loud-failure mandated (§26.7) |

**Concurrency**

| Question | Verdict | Evidence |
|----------|---------|----------|
| Lock ordering deadlock? | SAFE | mostly single global locks; shootdown has an explicit deadlock-breaker |
| Lock-free ABA? | SAFE | delegated-resource + endpoint-id reuse both gen-bump ABA-guarded |
| Races bypass validation? | SAFE | validate re-checked at enqueue under the lock |
| Concurrent destruction violate ownership? | FINDING | SEC-1 (HIGH), SEC-9 |
| Publication before initialization? | FINDING | SEC-25 (weak-arch), SEC-10, SEC-19 |
| Reads observe partially-init objects? | FINDING | SEC-25 (weak-arch); x86 TSO safe today |

**Truth** (Commandment III)

| Question | Verdict | Evidence |
|----------|---------|----------|
| Every reported state actual? | SAFE | introspection reads live kernel state; SEC-10 torn-read caveat (introspection only) |
| Every cache explicitly non-authoritative? | SAFE | fs free bitmap/count are reconcilable views (§26.4); §14.3 forbids serving cached truth post-restart |
| Every authoritative owner unique? | SAFE | per-slot band owner; kernel singleton guard |
| Convenience override truth? | NOTE | the by-name grant (SEC-6, T1/M7) is the closest smell; runtime is still explicit-cap |
| Stale truth survive restart? | FINDING | SEC-5 (cap survives delete+recreate); else generation invalidates |
| Inferred state replace observed? | SAFE | liveness is observed (generation), not inferred |

**Security Philosophy** (the lens questions, answered as the audit's synthesis)

- *If this service is malicious, what damage?* Bounded to its caps - except the **USB drivers** (Story A: own the system via CONSOLE_PUSH) and any **fs-send-cap holder** (ambient over the whole tree, by design). Ordinary services: only what their contract grants.
- *If this service dies now, what remains true?* Its endpoint generation is bumped (clients get `EndpointDead` and reacquire, §14.3); its frames/caps/kstack are reclaimed; the supervisor respawns the managed set. The one dishonest case is SEC-18 (a panic, not a death, does not halt the survivors).
- *Who owns this truth / who may mutate it?* Every authority traces to one central table (`service_privileges`/`service_hw`) or one owning task; no ambient inheritance.
- *Can this responsibility move out of the kernel? / Can this privilege be reduced?* Naming already moved out (Path C). The open reduction is SEC-2 (should a confined driver hold CONSOLE_PUSH/REBOOT?).
- *Can this operation fail louder / more deterministically?* SEC-16/SEC-17/SEC-20 are the "fail louder" gaps; SEC-25/26 are the "more deterministic across arches" gaps.

### Verified sound in this pass (do not re-hunt)

- **Boot boundary** (fatal vs degrade), spawn error-unwinding (`cleanup_partial_spawn`), duplicate-owner
  prevention (singleton + adopt-if-running), respawn-is-a-fresh-instance, and `converge` real-liveness backstop.
- **Interrupt discipline**: IF=0 critical sections, EOI ordering per source type, driver-death IRQ teardown, TLB-shootdown deadlock-breaker + watchdog.
- **Exhaustion**: per-task cap tables (A6 self-contained), ABA-safe id reuse, checked/saturating accounting, typed alloc-fail (no partial-state-as-valid).
- **Disclosure**: spawn (ELF/BSS/stack/PT), IPC message buffers, and introspection out-buffers are all zeroed; errors carry no kernel pointers; introspection + name-acquire are capability-gated.
- **Weak-arch-safe already**: `portable_atomic` word-size portability, the SPSC console ring (own-index Relaxed / cross-index Acquire-Release), lock-guarded cap/routing/`CALL_AWAIT_EP` ordering, per-core arena publication.

---

## Audit 2 - 2026-07-23 (ARM32 in-kernel USB stack)

**Scope:** the ARM32 USB stack this branch (`feat/pi2-arm32`) added - the in-kernel DWC2 host driver
(`arch/arm/dwc2.rs`: keyboard + CDC-ECM/smsc95xx networking + mass-storage), the `NetFrame*` syscalls
(42-44), the `NET_DEVICE` authority, and the ARM `nic-driver` bridge. A memory-safety pass on this exact
code was done first (see `audits/unsafe-audit.md`, 2026-07-23) and found no UB/OOB/race; this pass audits
**authority, trust boundaries, untrusted-input robustness (DoS/logic), and confused-deputy** only.

**Result: 1 MED, 3 LOW.** No exploitable escalation. The descriptor/frame parsing is genuinely
DoS-robust (every walk and hardware wait is bounded and guards the malicious cases); the `NET_DEVICE`
gate is correctly enforced and gen-safe (the SEC-11 assert covers its id); "TX/RX raw frames" is
on-the-wire transport, not a reach into another principal; the `nic-driver` bridge leaks no cap and
over-serves nothing. **The finding is a trust-posture one:** the whole USB stack runs in ring 0 on ARM,
a real and previously under-documented TCB expansion versus x86's confined-userspace-driver model.

### Ranked ledger

| ID | Sev | Conf | Principal | Finding | Fix direction |
|----|-----|------|-----------|---------|---------------|
| **SEC-29** | MED | CONFIRMED (posture) | kernel/arch (TCB) | **The entire ARM USB stack runs in-kernel (ring 0 / TCB) and parses untrusted device input there.** On x86 the USB drivers are confined *userspace* services (IOMMU arena, non-TCB, restartable - §6.4); on ARM `dwc2.rs` is kernel code (enumeration + the core-0 poll, `arch/arm/mod.rs:726`) parsing untrusted descriptors/frames. A logic flaw is a *kernel* compromise, not a bounded-service one. **Worse than §6.4's no-IOMMU x86 case:** the Pi 2 has no IOMMU/SMMU to confine DMA *and* ARM does not yet route device IRQs to userspace, so the driver is ring-0 code regardless. Not code-fixable on this hardware - the honest resolution is to *record* it (§26.3), as §6.4 records the DMA-driver posture. | CLAUDE.md §6.4-analog amendment: on ARM the in-kernel USB drivers are machine/arch-dependent TCB members until device-IRQ-to-userspace routing exists. **Done** (this commit). |
| **SEC-30** | LOW | CONFIRMED | shell / drivers | **SEC-2's least-privilege win does not translate to ARM.** The in-kernel keyboard driver calls `console_push_byte` directly (`dwc2.rs`, no cap - it is kernel code), so a hostile USB keyboard still injects shell commands (the inherent, un-codeable SEC-2 residual). But SEC-2's actual *win* - "REBOOT lives only with the shell; the USB driver no longer holds it" - is meaningless here: an in-kernel driver implicitly holds *all* kernel authority (it can call `hardware_reset` directly). The ARM keyboard driver is a superset of the x86 CONSOLE_PUSH+REBOOT posture SEC-2 narrowed. | Note in the SEC-2 / §6.4 text that the REBOOT-removal win is x86-only; on the in-kernel-driver ARM port the driver is inside the *kernel* trust perimeter, not merely the shell's. Folded into the SEC-29 amendment. |
| **SEC-31** | LOW | CONFIRMED (latent) | kernel/task | **`NET_DEVICE` is granted by service name on every arch, and a stale gen-safety comment.** `service_privileges` grants it via `matches!(name,"nic-driver")` and mints it unconditionally, but `net_frame_*` are inert stubs off-ARM - so the held cap authorizes nothing today, yet becomes live the day a non-ARM path wires a real `net_frame_*`. Separately, the SEC-11 comment (`table.rs:186`) says gate ids are "1-9" but `NET_DEVICE` is id 10; the actual assert is `id.0 >= 100`, which **does** correctly cover id 10 (so `holds_resource` gen-safety is sound - only the comment is stale). | Arch-gate the grant (`cfg!(target_arch="arm")`); update the SEC-11 comment to "ids 1-10". **Done** (this commit). |
| **SEC-32** | LOW | PLAUSIBLE | kernel/arch (DoS) | **`wait_halt`'s up-to-4M-iteration spin runs inside the core-0 timer ISR.** The keyboard `poll()` runs from the timer tick and calls `wait_halt`, which spins up to 4,000,000 iterations if a channel arms but never sets `CHHLTD` (a wedged/hostile controller state; a normal NAK/STALL/complete returns fast). When it occurs it is a per-tick tax degrading core-0 scheduler/console responsiveness (on x86 the equivalent poll is a preemptible userspace driver on its own core). Bounded - never a hang or escalation. | Use a tighter bound for the *polled* (steady-state) path than the enumeration path, and/or a one-shot "controller wedged" latch that stops polling loudly (invariant 12). **Done** (tighter poll bound, this commit). |

### Sound / no finding (verified)

- **Descriptor + frame parsing is DoS-robust.** Every walk is `while i + 2 <= total` with `blen == 0` break and `total` clamped `.min(cfg.len())`; the hub walk is bounded by `next_addr > 120` regardless of a hostile `bNbrPorts`; there is no recursion into a downstream hub; every hardware wait is a bounded counter with a loud break. The smsc95xx RX length is masked + validated (`4 + flen > got` rejects) before any copy. A malformed device slows or aborts a one-time boot enumeration; it cannot hang it.
- **`NET_DEVICE` gate is enforced + gen-safe.** All three syscalls check `current_task_holds_resource(NET_DEVICE_RESOURCE, WRITE)` first (non-holder -> `CapNotHeld`); lengths are bounded; user memory goes through the validated `read_user_bytes`/`write_user_bytes`. The gate resource is stable gen-0 and provably never revoked.
- **"TX/RX raw frames" is transport, not escalation.** Bounded to `NET_FRAME_MAX`, mediated by the in-kernel device; frames reach only the physical wire, never local IPC/routing - a `net_frame_tx` cannot reach or spoof another in-system principal. Bounded exactly like a DMA driver's arena.
- **The `nic-driver` ARM bridge (`usb_net_main`) leaks/over-serves nothing.** Opcodes are bounded to transport/info; the reply cap is taken, used once, and `remove_cap`'d after every reply. A client can only make it move frames or report info.

---

## Audit 3 - 2026-08-09 (the AArch64 USB stack moves to userspace; file-cap escalation)

**Scope:** what commit `e71e64a6` changed on the Pi 4 - `kernel/src/arch/aarch64/xhci.rs` (2742 lines
of ring-0 USB) deleted, USB now served by the userspace `services/xhci`. Three questions were asked:
(1) does moving the driver out of the kernel actually change its trust posture on a board with no
SMMU; (2) what authority does the service hold that it does not need; (3) how could the file-capability
escalation observed once after a chaos run (a read-only cap performed a write) occur. ARM32 is out of
scope and unchanged (Audit 2, SEC-29/30, still governs it).

**Result: 1 HIGH (posture), 2 MED, 5 LOW.** No new cross-principal escalation was found. The USB
service's authority is genuinely minimal in the capability model - it holds `CONSOLE_PUSH`, `log`, its
own recv endpoint, its controller's BAR and its DMA arena, and **nothing else** (verified against
`service_privileges`, not assumed). The HIGH is a posture correction, not a bug: **the move is a large
reduction in the accident surface and no reduction at all in the authority ceiling**, and the
constitution's own rule already says so - the amendment just does not repeat it. The file-cap
escalation is explained by a confirmed client-side capability-handle desync in which **neither the
kernel nor `fs` misbehaves**; both were re-read line by line and are correct.

### The question, answered directly: is the userspace `xhci` on the Pi 4 least-privilege?

**No. It is least-privilege in the capability model and still kernel-equivalent through DMA, and by
`CLAUDE.md` §6.1's own rule it is therefore a TCB member on this board.**

- **There is no IOMMU, and the code does not pretend otherwise - it just does not say so.**
  `kernel/src/arch/aarch64/mod.rs:2328-2334`: `iommu::detect`, `bringup`, `release_device` and
  `drain_event_log` are empty bodies and `confine_device` is `-> bool { false }`. `CONFINE_USB_DRIVERS`
  is `true` (`task/mod.rs:172`) and `HwClass::Xhci::iommu_confine()` is `true` (`task/mod.rs:387`), so
  `task/mod.rs:3871-3874` **does** call `confine_device` on every `xhci` spawn and **discards** its
  `false`. The driver is never confined; nothing is printed either way (SEC-34).
- **The service is handed the means to DMA anywhere.** 16 pages of the VL805's MMIO BAR
  (`task/mod.rs:3779-3786`) plus a 292-page physically-contiguous DMA arena whose **physical** base is
  handed to the service along with its VA (`task/mod.rs:3818-3855`). Holding the controller's
  registers it can point `DCBAAP`, `CRCR`, `ERSTBA` and any TRB buffer pointer at an arbitrary
  physical address, and the controller will execute those transfers.
- **The only hardware bound is the PCIe inbound window, and it covers all of RAM.**
  `arch/aarch64/pcie.rs:266-272` sizes `RC_BAR2` as `ram_bytes.next_power_of_two().max(64 KiB)` based
  at 0. That is a genuine and deliberately-tightened bound (the `+ 1` that opened a window twice the
  size of RAM was fixed, and `RC_BAR1`/`RC_BAR3` are explicitly shut, `pcie.rs:281-282`) - it stops a
  bus master reaching past the end of memory. It does **not** separate kernel memory, page tables or
  another service's pages from the driver's arena. It is a cap, not a confinement.
- **So §6.4's no-IOMMU bullet describes this board exactly**: "it programs its controller's DMA engine
  with physical addresses and can therefore read or write *anywhere* in RAM, regardless of the
  capabilities it holds. Its compromise is unbounded, so it is trust-critical by necessity."
  `docs/aarch64.md` §4 already reached this conclusion before the port existed ("no usable SMMU ... so
  H1/§6.4 does not travel"). Nothing has changed it.

**What the move genuinely bought, and it is a lot:**

- **The accident surface.** The thing that parses hub descriptors, HID reports and SCSI status words
  supplied by whatever is plugged in is no longer ring-0 code. A parser bug is now one killed service
  that the supervisor respawns (`xhci` is in the death-notification set, `scheduler.rs:1948-1963`),
  where before it was a kernel fault. That was the entire content of SEC-29 for arm32, and on aarch64
  it is now closed.
- **Ambient authority.** An in-kernel driver implicitly holds every kernel power (SEC-30's point: it
  could call `hardware_reset` directly). The service holds an enumerable, checkable set. **SEC-2's win
  now travels to aarch64** where it never travelled to arm32: no `REBOOT` (`task/mod.rs:467`), and the
  Ctrl+Alt+Del chord reaches the shell as the out-of-band `hid::CTRL_ALT_DEL_SIGNAL` byte, exactly the
  x86 arrangement.
- **Restartability and the blast radius of a *bug*.** A compromised driver is unbounded; a *buggy* one
  is now bounded to its own service and its own permanently-reserved arena.

**The honest one-line form:** *a buggy USB driver on the Pi 4 is now bounded; a compromised one is
still kernel-equivalent.* That is the same posture as an x86 machine without an IOMMU, and strictly
better than arm32's in-kernel driver - but it is not the confined case, and the difference should be
stated where a reader looks (SEC-33) and printed where §6.4 promises it will be (SEC-34).

### Ranked ledger

| ID | Sev | Conf | Principal | Finding | Fix direction |
|----|-----|------|-----------|---------|---------------|
| **SEC-33** | HIGH | CONFIRMED (posture) | `xhci` (aarch64) | **The userspace `xhci` on the Pi 4 is still a TCB member, and §6.4's 2026-08-09 amendment does not say so.** Full argument above. The amendment is accurate about the *kernel* (Commandment I is closed, the ring-0 code is gone) and silent about *trust*; §6.1's table row supplies the answer ("in the TCB only on a machine with no IOMMU") but a reader has to join two sections to get it, and the amendment's framing ("Commandment I is closed on this port, not merely satisfiable") reads as though the driver left the TCB. It did not. Recording it is the whole point of the machine-dependent posture: the same binary is least-privilege on an IOMMU machine and trust-critical here. | One paragraph in the §6.4 amendment: on the Pi 4 there is no usable SMMU, so the userspace `xhci` is a TCB member by §6.1's rule; what the move bought is the accident surface and the ambient authority, not the DMA ceiling. Per §26.3 this is **recorded**, not closed - there is no SMMU to close it with. |
| **SEC-34** | MED | CONFIRMED | kernel/arch (aarch64) | **§6.4's "reported loudly at boot" promise is unmet on the port where the answer is worst, and a failed confinement is silently discarded.** §6.4 rests its whole machine-dependent posture on the case being "a printed boot fact rather than a hidden assumption" - on x86 that is `iommu: no IVRS table ... drivers stay in TCB` or `iommu: ... confined BDF ...`. On aarch64 `iommu::detect`/`bringup` print nothing (`mod.rs:2329-2330`), `confine_device` returns `false` in silence (`mod.rs:2331`), and the call site ignores the return (`task/mod.rs:3872`). The `else` branch that *would* have printed "left in IOMMU passthrough" is not reached, because `hw.iommu_confine()` is `true` for xhci. **A Pi 4 boot therefore prints nothing about DMA confinement in either direction.** This is also the §26.7 shape (a hardening step whose failure is discarded) and invariant 12 (failures are loud, never silent). | Check `confine_device`'s return at the call site and print the outcome either way; give the aarch64 `iommu::detect` a one-line "no SMMU on this SoC - DMA drivers stay in the TCB (§6.4)". Cheap, and it makes the posture self-reporting on every board. |
| **SEC-35** | MED | CONFIRMED (mechanism); PLAUSIBLE (as the specific observed instance) | shell / SDK | **A discarded message's embedded capability stays queued, so `take_pending_cap()` can hand a later `open` an EARLIER open's capability - which reproduces the observed "read-only cap performed a write" with no kernel or `fs` bug.** Detail below. Two independent sub-causes, both confirmed by reading: (a) nothing pops the pending-cap FIFO when a *message* is thrown away, and the shell throws messages away in three places; (b) `TASK_PENDING_RECV_CAP_COUNT` is never reset, so a respawned task inherits a dead task's queue. | `fc_open` should verify the returned cap's rights against the rights it asked for (`query_cap_rights`, already used by the `[fcapr]` instrumentation) and fail loudly on a mismatch; the drain paths should pop the FIFO for every message they discard; and the pending-cap FIFO should be cleared when a task slot is reused. |
| **SEC-36** | LOW | CONFIRMED (residual) | shell / SDK | **The abort/timeout paths still remove a reply cap the kernel already removed - the same remove-by-stale-index shape that `1ecfd98e` just fixed on the reply path.** `handle_resource_invoke` removes `reply_slot` from the caller's table on **every** delivering outcome (`dispatch.rs:1231`, `1236`, `1240` - including `QueueFull`). `fc_invoke` (`shell:8487`), `sock_invoke` (`shell:5460-5463`) and the two SDK sites still call `remove_cap(reply)` on `Timeout`/`Aborted`. The comment justifying it ("there the send never delivered, so the cap IS still ours") is true for the name-addressed `request_with_reply` path but **not** for `resource_invoke`, whose kernel handler frees the slot before the caller can time out. I could not construct a reachable exploitation (only a received message can refill the slot during the wait, and such a message becomes `Reply`), so it is LOW - but it is one path away from the bug that was just closed. | Remove the reply cap only where the wrapper knows the send did not deliver (the `resource_invoke` error return), never after a delivering send, on any outcome. |
| **SEC-37** | LOW | CONFIRMED (latent) | kernel/task | **A vestigial `USB_DISK` grant on aarch64 - SEC-31 repeating one arch later.** `task/mod.rs:492-493` grants `USB_DISK` to `block-driver` on `arm` **or** `aarch64`. On aarch64 the four syscalls it authorises are permanent stubs returning 0/false (`arch/aarch64/mod.rs:1277-1292`), and `block-driver` reaches the disk by IPC to the `xhci` service instead (`send_peers: &["xhci"]`, `task/mod.rs:716`; `usbdisk.rs` routes to `xhciblk::`). The held cap authorises nothing today and becomes live the day a real aarch64 `usb_disk_*` is written - exactly SEC-31's `NET_DEVICE` finding, whose chosen fix (arch-gate the grant) was not extended when aarch64 joined the `cfg!` list. The comment above it still claims "on BOTH ARM ports the USB stack is in-kernel" (`documentation-audit.md` A4-2). | Drop `aarch64` from the `usb_disk` `cfg!` (leaving `arm`), and correct the rationale comment. |
| **SEC-38** | LOW | CONFIRMED, materially mitigated | kernel/task (aarch64) | **The kill path cannot quiesce the controller on aarch64.** `scheduler.rs:1993-2010` clears PCI bus-mastering before reclaiming a dead driver's frames - the cure for the `max-carnage` page-table corruption. On aarch64 `pci::clear_bus_master` is an empty stub (`mod.rs:2317`), as are `set_bus_master` and `set_power_d0`. **Why this is LOW and not HIGH:** the DMA arena is allocated **once** via `allocator::alloc_dma_arena`, permanently reserved out of the general pool, and **reused** on every respawn (`task/mod.rs:177-183`, `3811-3824`) - so a still-live controller's stray DMA can only land in `xhci`'s own reserved arena, never in a page table, a kernel struct, or another service. **Residual:** the respawned instance initialises the *same* arena the old controller may still be writing, so a rapid kill/respawn can corrupt transient enumeration or disk data in the new instance. Bounded to `xhci`; not an escalation. | Implement `clear_bus_master` (PCIe config-space command register, which `pcie.rs` already reaches via `cfg_write`) so the kill path's quiesce is real rather than a no-op on this arch. |
| **SEC-39** | LOW | CONFIRMED | `xhci` (robustness, not authority) | **Two missing clamps in the USB service, neither memory-unsafe.** (a) `OP_READ_BLOCK`/`OP_WRITE_BLOCK` (`msc.rs:754-793`) do not check `lba` against the device's own `d.sectors`; only `read10`/`write10`'s `lba > u32::MAX` guard applies (`msc.rs:436`, `487`). Memory-safe (`count` is hardcoded 1 and every buffer index is compile-time bounded), so this is a capacity-validation gap the layer above must not rely on `xhci` to close. `OP_WRITE_ZEROS` does check it (`msc.rs:794-812`), which is what makes the omission look like an oversight rather than a decision. (b) `main.rs:1135-1144`'s HID config-descriptor walk does not clamp the device-reported `wTotalLength` to the 64 bytes actually fetched - only a hardcoded `i < 200` bounds it - unlike `parse_msc`, which does `.min(buf_len)` (`msc.rs:580-585`). Stays inside the service's own DMA page so there is no OOB, but a lying device can make it parse the *previous* device's stale descriptor bytes as endpoints. | Mirror `parse_msc`'s clamp in the HID walk; bound `lba` by `d.sectors` in the two block ops. |
| **SEC-40** | LOW | PLAUSIBLE | kernel/capability | **`handle_resource_invoke` validates the cap's generation and then reads the owner without re-checking; the two are not atomic.** The generation check is at `dispatch.rs:1181` and `delegated::owner_of` at `1196`, with no lock spanning them. The delegated band is shared by `fs`, `net-stack` and `resource-server` (`service_hw`, `task/mod.rs:424`), so in principle a concurrent `revoke_owned` + `allocate` on another core inside that window would route a badged message to a **different owning service** than the one whose resource the caller validated. The window is a few instructions with IF=0 on the calling core, I could not demonstrate it, and it does not produce the observed symptom - recorded so it is not re-discovered as new. | Read the owner under the same acquisition that validates the generation, or re-check the generation after the owner lookup and fail with `CapRevoked` on a change. |

### SEC-35 in full: how a read-only file capability performed a write

The observed failure is `fcap` step 5 printing **"FAIL ro cap wrote (escalation!)"** (`shell:8562`).
Everything below was read, not inferred.

**Both of the obvious suspects are innocent, and were checked first:**

- **`fs` enforces `op <= right` correctly.** `serve_filecap` refuses `FOP_WRITE` unless the badge
  carries `RIGHT_WRITE` (`services/fs/src/main.rs:1318`), refuses `FOP_READ`/`FOP_STAT` without
  `RIGHT_READ` (`1301`, `1326`), and resolves `rid -> path` only through its own `open_path` table
  (`1293`).
- **The kernel validates before it badges.** `handle_resource_invoke` looks the cap up **with the
  requested right** (`dispatch.rs:1181`), rejects anything outside the delegated band (`1193`), and
  only then stamps `msg.badge_right` (`1219`).
- **The delegated band is ABA-safe.** `allocate` re-registers a reused id at `prev_gen.bump()`
  (`capability/delegated.rs:114-122`), so a stale cap from an id's previous life can never re-validate.
  The `[fcapr]` instrumentation's "suspect 2" - a rid minted by a dead `fs` instance resolving in the
  new one - is closed by design.
- **A client cannot embed two caps to desync a reply-cap queue.** `build_message` leaves
  `cap_count = 0` (`dispatch.rs:546-563`) and the only three assignments in the kernel set it to 1
  (`965`, `1056`, `1221`). The multi-cap desync attack against `fs`'s own `take_pending_cap`
  (`fs:553`) is therefore not reachable.

**The actual mechanism is in the client's handle bookkeeping.**

1. `handle_recv` / `handle_try_recv` install **every** embedded cap into the receiver's table and push
   its slot onto a per-task FIFO (`dispatch.rs:330-340`, `383-392`, `443-452`). This happens for every
   received message, regardless of what the service then does with the message.
2. `pop_pending_recv_cap` is a **FIFO** - it returns index 0 and shifts down
   (`scheduler.rs:754-768`).
3. **Nothing pops the FIFO when a message is discarded**, and the shell discards messages in three
   places: `drain_stale_fs_replies` (`shell:8226-8230`, up to 8 `try_recv`), `fc_invoke`'s
   `while ctx.try_recv().is_some() {}` (`shell:8470`), and `fs_take_tagged`'s overtaken-reply discard
   (`shell:8043-8060`). If any discarded message was an `OP_OPEN` reply, its file cap is now in the
   shell's table and at the head of the queue, owned by nobody.
4. **`TASK_PENDING_RECV_CAP_COUNT` is never reset** (`scheduler.rs:231`) - not at task death, not at
   spawn (the array appears only at its definition and in push/pop). A task that dies with a pending
   cap leaves its slot's count non-zero, and the next task to land in that slot inherits the queue.
5. `fc_open` takes the head and **trusts it** (`shell:8444-8458`): it never checks the returned cap's
   rights against the rights it asked for.

**The scenario, end to end.** `fcap` opens `rw` (`READ|WRITE`, `shell:8530`) and later `ro` (`READ`,
`shell:8554`). If the FIFO is one entry ahead - because a chaos-induced `fs` restart made an earlier
reply late and it was drained, or because the shell was killed mid-`fcap` and respawned into the same
task slot - then step 4's `take_pending_cap` returns **step 1's `READ|WRITE` cap**, and the shell's
`ro` handle names it. Step 5 then invokes it declaring `RIGHT_WRITE`; the kernel validates a cap that
genuinely holds `WRITE`, badges the message `WRITE`, and `fs` correctly serves the write. **Every layer
behaves exactly as specified.** The escalation is a lie told by a handle number.

**Why this is the live suspect rather than a theory.** The (now feature-gated) `[fcapr]`
instrumentation was built to catch precisely this: `fc_open` logs `asked=` against `holds=`, and its
own comment says *"if they disagree, the handle names a different cap than the one fs minted, which is
the identity-confusion suspect rather than a rights-check bug"* (`shell:8448-8456`). And the same class
was already confirmed and fixed in this code four hours before the deletion commit - `1ecfd98e`
("stop removing a reply cap the SEND already transferred away"), whose commit message states the
general rule plainly: *"a remove-by-stale-index can bite ANY request whose reply carries a cap"*.
SEC-35 is that rule applied to the *insert* side rather than the remove side.

**Ranked hypotheses, for the record:**

1. **Pending-cap FIFO desync (SEC-35)** - CONFIRMED as a code defect, and the only one that reproduces
   the exact symptom with correct kernel and `fs` behaviour. Sub-cause (b) (no reset across task
   lives) explains the chaos correlation directly: chaos kills the shell.
2. **The already-fixed remove-by-stale-index (`1ecfd98e`)** - CONFIRMED, and it produces the same
   symptom by aliasing two handles onto one slot. Fixed on the reply path; SEC-36 is its residual on
   the timeout path.
3. **Cross-service TOCTOU on the owner lookup (SEC-40)** - PLAUSIBLE, extremely narrow, and does not
   produce this symptom.
4. **`fs` rights enforcement / delegated-id reuse** - RULED OUT by reading (above).

**Severity judgement.** This is MED, not HIGH, and the reason matters: the shell already held the
`READ|WRITE` cap, so **no principal gained authority beyond its grant** - the north-star holds. What
broke is the model's integrity and a guarantee `CLAUDE.md` §22 Test 14 exists to pin ("non-escalation
at both layers"), plus Commandment III (one truth: the handle number is a derived view of the kernel's
cap table and it drifted with no reconciliation) and Commandment IX (a client must re-establish
everything derived from a restarted dependency). It becomes HIGH the moment any service brokers caps
between principals, because the same desync would hand one principal another's capability.

### Verified sound in this pass (do not re-hunt)

- **`services/xhci` holds no authority it does not need.** Checked against `service_privileges`
  (`task/mod.rs:450-514`) and `service_hw` (`418-427`), not assumed: no `SPAWN`, no `SERVICE_CONTROL`,
  no `REBOOT`, no `ACQUIRE_ANY`, no `INTROSPECT`, no `RESOURCE_MINT`, no `NET_DEVICE`, no `USB_DISK`,
  no `GPIO_DEVICE`, no `SET_CLOCK`, no `CONSOLE_READ`. It holds `CONSOLE_PUSH`, `log`, its own recv
  endpoint, 16 pages of its controller's BAR and its DMA arena. That is the whole set.
- **`CONSOLE_PUSH` is genuinely narrow.** Only two push sites exist (`main.rs:203`, `211`, plus the
  auto-repeat at `3759`). Everything pushed is either the output of `hid::emit_key`/`hid_to_ascii`'s
  fixed lookup table (`sdk/rust/src/hid.rs:15-138`) or the single out-of-band
  `CTRL_ALT_DEL_SIGNAL = 0x80` chord byte, which `is_ctrl_alt_del` gates on both modifier bits and the
  Delete usage (`hid.rs:254-262`). **No raw device-supplied byte reaches the console ring.** SEC-2's
  residual (a keyboard's keystrokes are commands) is unchanged and remains inherent; SEC-2's *win* now
  holds on this port, unlike arm32.
- **`services/xhci` contains no `unsafe`.** Three grep hits, all prose (`main.rs:6`, `1413-1414`). All
  hardware access goes through the SDK's audited `Mmio`/`Dma` wrappers, satisfying §18.2, and
  `scripts/unsafe_check.py` does walk `services/**/*.rs`, so a regression would be caught.
- **Who can reach the block service.** `xhci` does not authenticate senders - it serves any message
  carrying a reply cap (`main.rs:1385-1456`) - which is the correct capability-model answer: authority
  is holding the SEND cap. `handle_acquire_send_cap` grants one only to a declared peer
  (`block-driver`, `task/mod.rs:716`) or an `ACQUIRE_ANY` holder, so the reachable set is the intended
  one.
- **A crafted block request cannot overflow the service.** `read10`/`write10` bound `count` by
  division rather than multiplication to avoid a u32 wrap (`msc.rs:432-436`, `483-487`); `serve_block`
  length-checks every wire field before indexing (`req.len() < 9`, `< 9 + SECTOR`, `< 17`); the DMA
  layout is provably non-overlapping (`DISK_BASE` page 288 = scratchpad base page 32 + 256 pages, and
  `XHCI_DMA_PAGES = 32 + 256 + 4` matches exactly). The gaps are the two clamps in SEC-39, neither of
  which is a memory-safety issue.
- **Hub and mass-storage descriptor parsing is bounded.** `parse_msc` clamps `wTotalLength` to the
  fetched length and breaks on a zero-length descriptor (`msc.rs:580-621`); the hub port walk guards
  every EP0-ring write against overrunning its page (`main.rs:2136-2236`); the CSW tag check rejects a
  device answering out of turn (`msc.rs:355-359`).

---

## Fix log

| Finding | Status | Commit | Notes |
|---------|--------|--------|-------|
| **SEC-1** | FIXED (compile-verified; HW-pending) | `d08d7d4` | Ported the `timer_tick_from_irq` Dekker handshake (CAS `Ready->Running` + publish `CORE_CURRENT` SeqCst + fence + re-read + abort-to-scheduler) into `yield_current` and `block_and_reschedule` - the two switch-in sites that lacked it. |
| **SEC-18** | FIXED (**HW-verified on the T630**, 2026-07-18) | `dc9d580` | `halt_all_cores` broadcasts an NMI to every other core (new `boot::broadcast_nmi_all_but_self`, NMI delivery mode so it reaches a core spinning IF=0), and `idt[2]` is repointed to the unconditional `exception_halt`. A panic now stops the machine (§6.2 / §19). **Verified end-to-end** (QEMU int-trace + T630 serial) via a keystroke-induced kernel panic - see "SEC-18 hardware verification" below. |
| **SEC-21** | FIXED (compile-verified; HW-pending) | `b110191` | New safe `memory::allocator::zero_frame` zeroes each AllocMem frame via the HHDM before it is mapped, closing the cross-task stale-memory leak. `unsafe` kept in the permitted `memory/` layer so the grandfathered `dispatch.rs` stays `unsafe`-free (§18.5). |
| **SEC-4** | FIXED (compile-verified; HW-pending) | `cc9288d` | `check(off,size)` (checked-add) bounds-assert on every `Dma` and `Mmio` accessor; `Mmio` gained a `len`, threaded from the kernel through the mirrored `#[repr(C)]` context ABI. An out-of-bounds access now loudly panics the one driver instead of silently corrupting memory. |
| **SEC-5** | FIXED (compile-verified; HW-pending) | `5b2893f` | New `revoke_open_subtree` (prefix-match with a `/` boundary guard) revokes descendant file caps on `delete_tree` / dir rename / move. Closes the slot leak and the recreate-path aliasing escalation; single-file `delete` keeps the exact-match revoke. **NOT the LS1 fix** - the T630 capture showed LS1 is a block-driver transient-disk-detection miss + fs mount not self-healing (fixed separately @ `658df88`, `audits/userspace-audit.md` LS1 resolution); the SEC-5 slot leak was a wrong hypothesis for LS1, though a real bug in its own right. |
| **SEC-6** | FIXED (build-verified) | `df7e4e6` | `AcquireSendCap` mints the GRANT right only for `ACQUIRE_ANY` holders (the operator/test instruments that legitimately re-delegate); a declared-peer acquirer gets SEND-only. GRANT follows the instrument permission, not the caller's `arg2`. |
| **SEC-7** | FIXED (**`file-cap` 10/0**) | `2dc6dde` | The kernel strips GRANT from an embedded **delegated**-resource cap at every install site, so a client's file/socket cap is READ/WRITE-only and cannot be re-delegated (the owner controls delegation by minting). Endpoint caps unchanged. New `Rights::without` / `Capability::without_grant` / `narrow_embedded_for_receiver`. |
| **SEC-8** | RESOLVED (subsumed by SEC-7) | `2dc6dde` | With no GRANT a file cap cannot be shared -> a resource has exactly one holder -> `FOP_CLOSE` can no longer revoke a *different* holder's cap. The cross-holder revoke is unreachable, so no separate change is needed. |
| **SEC-11** | FIXED (build-verified) | `f7e64d0` | `debug_assert` in `bump_generation` that no stable gate resource (ids 1-9) is ever revoked/killed - pins the invariant that keeps `holds_resource` gen-safe. |
| **SEC-14** | VERIFIED (no change) | - | `loader.rs:178` already rejects `p_offset + p_filesz > bytes.len()` with `checked_add`. |
| **SEC-15** | FIXED (build-verified) | `de685b5` | `fire_test_irq` uses `local_irq_save/restore` (preserves the caller's IF; no ISR-stack re-entrancy). |
| **SEC-16** | FIXED (build-verified) | `de685b5` | `route::register` logs loudly on an IRQ-route collision instead of a silent overwrite (invariant 12). |
| **SEC-19** | FIXED (build-verified) | `de685b5` | Task memory budget seeded **before** `commit_task` publishes `Ready` - no stale-quota window on the scheduling core. |
| **SEC-20** | FIXED (doc) | `f7e64d0` | Crash-page doc-drift corrected in `CLAUDE.md` §19, `kernel/CLAUDE.md`, `docs/prime.md` (no crash page exists; the panic reason is serial-only; the panic now NMI-halts all cores). |
| **SEC-23** | ASSESSED (keep) | - | The cap-mismatch log is operator-only serial/ring-buffer diagnostic detail, not an authority leak (an id+generation can't forge a cap); §26.7 favours keeping loud diagnostics. No change. |
| **SEC-13** | ACCEPTED (dev-only) | - | `spawnwired`/`spawncap` are completed-phase Phase-0 diagnostics that spawn the `greet`/`pong` **examples**, which are absent from the bare-metal/production image - so the GRANT leak is dev-only, fixed-target, and not attacker-steerable. Documented rather than coded. |
| **LS1** | FIXED (root-caused) | `658df88` | Not SEC-5: a block-driver transient AHCI disk-detection miss (`sig=0xffffffff`) + fs latching a degraded mount. block-driver waits `PxTFD.BSY/DRQ` before reading `PxSIG`; fs re-mounts on a request while degraded (self-heal). See `audits/userspace-audit.md` LS1 resolution. |
| **SEC-2** | FIXED (least-privilege) + §6.4 note | `feat/hardening` | Removed **REBOOT** from the USB drivers (`xhci`/`ehci`) - a compromised driver can no longer hard-reset the machine directly from any context; reboot lives only with the shell (its `reboot` command). The core residual is **inherent and un-codeable** (a keyboard driver synthesizes keystrokes, and keystrokes are commands the kernel can't tell from real ones), so §6.4 now acknowledges CONSOLE_PUSH holders are inside the shell's trust perimeter. Chosen scope: least-privilege + honest note, not a UX-breaking trusted-serial path. |
| **SEC-25..28** | SPECIFIED (SMP-port contract) | `feat/hardening` | Portability-latent, weak-arch-only - **no x86 code change** (identical/no-op codegen on x86, and untestable there; the barrier/flush/coherence code belongs in the future `arch/aarch64/` SMP layer). Encoded as an authoritative **SMP-port contract** in `kernel/src/arch/CLAUDE.md`: task-slot publication ordering - Release-before-flag + Acquire loads (SEC-25); address-space-switch TLB flush (SEC-26); the `arch::imp` semantic contract, not just signatures (SEC-27); and DMA cache-coherence maintenance (SEC-28). Inline `SEC-25`/`SEC-28` markers at the `reserve_task_slot` store site and the SDK `Dma` wrapper point back to it, so a porter meets the obligation by construction rather than as a heisenbug. |

All are on `feat/hardening`, compile clean (`osdev build`) with the arch-boundary / dash / unsafe guards
green. **Boot-verified on the T630:** the hardening image booted clean and `selfcheck` ran **349, failed
0** (SMP + IOMMU + AHCI detection + fs mount + all file ops), and **`osdev test file-cap` is 10/0** (SEC-7).
**SEC-18 is now HW-verified on the T630** (2026-07-18; keystroke-induced panic - see "SEC-18 hardware
verification" below). The only fix still needing an active fault to prove is **SEC-1** (a cross-core
interleaving TCG cannot reproduce - a long `chaos max-carnage` soak, clean past ~400k rounds as of
2026-07-18). Everything else is exercised by the ordinary boot + selfcheck + file-cap paths.

The **portability set SEC-25..28** is now SPECIFIED as the SMP-port contract (above; `arch/CLAUDE.md`) -
weak-arch-only, no x86 code change. SEC-2 is fixed (least-privilege + §6.4 note); SEC-3 (ehci passthrough)
is an accepted §6.4 posture; SEC-9/10/12/17/22/24 remain recorded LOWs (weak-arch / DoS-bounded /
info-only - no reachable x86 escalation). **Every SEC finding with a reachable x86 impact is now fixed,
subsumed, verified, assessed, or specified** - the two HIGH by code (SEC-1 - soak-pending; **SEC-18 - HW-verified 2026-07-18**),
the reachable MEDs by code (SEC-4/5/7/21 + the SEC-2/3 driver posture), and the rest by targeted fixes,
assessment, or the port contract.

---

## SEC-18 hardware verification (2026-07-18)

SEC-18 fires only on a real multi-core kernel panic, so it is reachable neither by the ordinary boot nor
by soak depth - it needs an *induced* panic. A temporary, uncommitted "faulty" image induced one
deterministically; the mechanism was verified identically in QEMU and on the T630. (The trigger lives
only in the faulty image; `feat/hardening` HEAD is clean.)

**Trigger - why a keystroke, not a timer.** A first attempt fused the panic to a core-0 timer-tick
count. That proved the wrong tool on the T630, which boots `idle_can_halt = true` (AMD, cool-when-idle)
and, in the bare-metal image (no ping/pong), goes fully idle after boot - so core-0 ticks accrue far too
slowly to time a fuse (the arming banner, printed on the 2nd core-0 tick, took ~1.7 s to appear; a
1500-tick fuse never fired). The trigger was changed to a **keystroke-induced** panic: a `panic!` in the
`ConsolePush` syscall handler (`dispatch.rs`), which is reached only on a real key press from the USB
keyboard driver. A keypress deterministically wakes a core and runs kernel code regardless of idle state,
so it fires the instant a key is pressed.

**Result - identical in QEMU and on hardware.** On a key press the kernel panics on the core running the
keyboard driver; `halt_all_cores` broadcasts the NMI; every *other* core takes `v=02` (NMI) into
`exception_halt` and stops.
- **QEMU** (`-d int`, USB keyboard + monitor `sendkey`): the CPU trace's final three events are all
  `v=02` into `exception_halt` (`IP=0xffffffff8010e823`), one per non-panicking core, after which the
  trace goes silent. Single boot, zero service lines after the panic.
- **T630 serial** (2026-07-18 10:23:28.451): `KERNEL PANIC ... keystroke-induced kernel panic` at
  `dispatch.rs`, followed by `EXCEPTION ... RIP=0x...8010e823` breadcrumbs *garbled together at the
  character level* - the interleaving is itself the proof that multiple cores' panic handlers hit the one
  serial port simultaneously. The log then dead-ends: single boot (no reboot), zero service lines after
  the panic (no `gsh>`, no heartbeat) - the whole machine went dark on one keypress.

**Strongest form of the guarantee.** On the T630 the keyboard is on `ehci`, which runs on **core 3** (a
non-BSP core), so the panic *originated on core 3* and the NMI still halted cores 0/1/2 (including the
BSP). The property proven is "a panic *anywhere* halts *everywhere*", not merely "a BSP panic halts the
APs". Before SEC-18, `halt_all_cores` stopped only the calling core and the survivors kept scheduling
past a dead kernel (§6.2 / §19 violated); this is that law holding on real silicon.
