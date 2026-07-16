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
| **SEC-6** | LOW-MED | CONFIRMED | kernel | **`AcquireSendCap` GRANT bit is caller-chosen** for a merely-*declared* send-peer (`dispatch.rs:842-847`). A service can self-mint `SEND|GRANT` to a declared peer and re-delegate send authority the contract never intended to be re-delegatable. | Condition the GRANT bit on a contract flag (a `re-delegate` permission), not `arg2`. |
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
