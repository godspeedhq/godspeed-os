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

## Fix log

| Finding | Status | Commit | Notes |
|---------|--------|--------|-------|
| **SEC-1** | FIXED (compile-verified; HW-pending) | `d08d7d4` | Ported the `timer_tick_from_irq` Dekker handshake (CAS `Ready->Running` + publish `CORE_CURRENT` SeqCst + fence + re-read + abort-to-scheduler) into `yield_current` and `block_and_reschedule` - the two switch-in sites that lacked it. |
| **SEC-18** | FIXED (compile-verified; HW-pending) | `dc9d580` | `halt_all_cores` broadcasts an NMI to every other core (new `boot::broadcast_nmi_all_but_self`, NMI delivery mode so it reaches a core spinning IF=0), and `idt[2]` is repointed to the unconditional `exception_halt`. A panic now stops the machine (§6.2 / §19). |
| **SEC-21** | FIXED (compile-verified; HW-pending) | `b110191` | New safe `memory::allocator::zero_frame` zeroes each AllocMem frame via the HHDM before it is mapped, closing the cross-task stale-memory leak. `unsafe` kept in the permitted `memory/` layer so the grandfathered `dispatch.rs` stays `unsafe`-free (§18.5). |
| **SEC-4** | FIXED (compile-verified; HW-pending) | `cc9288d` | `check(off,size)` (checked-add) bounds-assert on every `Dma` and `Mmio` accessor; `Mmio` gained a `len`, threaded from the kernel through the mirrored `#[repr(C)]` context ABI. An out-of-bounds access now loudly panics the one driver instead of silently corrupting memory. |
| **SEC-5** | FIXED (compile-verified; HW-pending) | `5b2893f` | New `revoke_open_subtree` (prefix-match with a `/` boundary guard) revokes descendant file caps on `delete_tree` / dir rename / move. Closes the slot leak and the recreate-path aliasing escalation; single-file `delete` keeps the exact-match revoke. **NOT the LS1 fix** - the T630 capture showed LS1 is a block-driver transient-disk-detection miss + fs mount not self-healing (fixed separately @ `658df88`, `docs/userspace-audit.md` LS1 resolution); the SEC-5 slot leak was a wrong hypothesis for LS1, though a real bug in its own right. |

All are on `feat/hardening`, compile clean (`osdev build`) with the arch-boundary / dash / unsafe
guards green. **SEC-1 and SEC-18 are not boot-verified** yet, by design of the bugs: SEC-1 is a
cross-core interleaving TCG cannot reproduce, and SEC-18 fires only on a real multi-core panic - both
validated on **hardware** (a `chaos max-carnage` soak for SEC-1; an induced panic for SEC-18), not
under the flaky dev-host QEMU. SEC-21 / SEC-4 / SEC-5 are ordinary-path changes whose live behaviour a
hardware `selfcheck` + soak exercises directly (SEC-5's subtree revoke is also what a long soak needs to
confirm LS1 is gone).
