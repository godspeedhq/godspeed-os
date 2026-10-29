# GodspeedOS: Engineering Assurance and Independent Review

**Status:** living engineering document, first written 2026-10-15 (at v0.28.0).
**Project:** GodspeedOS, <https://github.com/godspeedhq/godspeed-os>

## 1. Purpose

GodspeedOS is an experimental capability microkernel: a small kernel, isolated userspace services, explicit
authority and recovery by restart. This document says how its architecture, implementation and behaviour can be
examined and challenged by someone who did not build it, and where the evidence for each claim lives today.

It is **not** a certification of correctness, a security guarantee, or a claim that Godspeed is free of defects.
Its job is to make every engineering claim specific, testable, reproducible and falsifiable.

Godspeed assumes software fails. Its architecture tries to contain a failure, limit its consequences and recover
where recovery is possible. That philosophy does not remove defects; it sets requirements the implementation can be
measured against.

**The constitution is the law; this document is not.** `CLAUDE.md` defines the invariants and records every
change to them as a dated amendment. Where this document and `CLAUDE.md` disagree, `CLAUDE.md` wins and this
document is wrong.

## 2. Architectural invariants

### 2.1 The kernel's six responsibilities: MISCIS

The kernel does exactly six things (`CLAUDE.md` 4.3):

1. **M**emory isolation: per-service address spaces and page tables.
2. **I**PC: synchronous message passing, bounded queues, cross-core routing.
3. **S**cheduling: per-core run queues, round-robin with timer preemption.
4. **C**apability enforcement: validation on every privileged syscall, with a generation check.
5. **I**nterrupt routing: delivery to userspace driver services.
6. **S**MP routing: the endpoint-to-core map and inter-processor wakeups.

Filesystems, networking, drivers, the terminal, logging policy, power policy and every utility live in userspace
(`CLAUDE.md` 4.4). The test of a change is whether it adds a seventh responsibility, not how many lines it adds.

**Mechanism versus policy is the line, and it is finer than "kernel or not".** Some features need a last step only
the kernel can take, on a channel it already owns. Turning the machine off is the worked example: `power` and the
shell decide when and do the tidy-up, while the kernel holds the one deadline and writes the final register
(`CLAUDE.md` 12.3, `docs/shutdown.md` 6.1). Each such case is recorded as an amendment that says why it is
mechanism, which surface it widened, and that MISCIS did not change. A reviewer should treat any kernel change
without such a record as a finding.

**Enforced, not only stated.** `scripts/commandments.py` derives the six from `CLAUDE.md` itself and fails the
build on a kernel module that serves something outside them; `COMMANDMENTS.baseline.toml` pins the syscall and
query surface. Ops within a syscall are not pinned, which is a known gap in that gate (the 2026-10-15 amendment in
`CLAUDE.md` 12.3 records one).

The objective is the smallest trusted computing base, not the smallest source tree. The TCB today is the kernel and
the restartable supervisor; on a machine with no IOMMU it also includes every driver that can DMA (section 9).

### 2.2 Capability invariants

- **No ambient authority.** Every privileged action needs an explicit capability (`CLAUDE.md` 3.1).
- **A name is not a permission.** Knowing a service's or a file's name grants nothing. Acquiring a send capability
  by name is itself gated (`AcquireSendCap`, by a declared peer or `ACQUIRE_ANY`), and a file is reached only
  through a capability `fs` minted (`CLAUDE.md` 7.10).
- **Delegation only narrows.** Rights cannot widen on transfer, and transferring needs `GRANT` (`CLAUDE.md` 7.3, 8.5).
- **Revocation is a generation bump.** Every outstanding capability to a destroyed or revoked resource fails its
  next use (`CLAUDE.md` 7.5).
- **Authority comes from the spawn request, not the contract.** The kernel cannot read a contract; it mints exactly
  what the supervisor's spawn request asks for and the supervisor itself holds (`CLAUDE.md` 13.6).
- **A restart re-grants exactly the spawn request, and nothing the dead instance acquired.** A respawned service
  gets fresh capabilities at a new generation; capabilities to the dead instance go stale (`CLAUDE.md` 14.2), and
  a revoked delegated resource stays revoked because its generation moved.

These are requirements to verify, not assumptions that every path satisfies them. Section 3 says which are
exercised by tests today.

### 2.3 Isolation and recovery

Services and drivers run outside the kernel, and a failing one is terminated and restarted without taking
unrelated services down. **The supervisor restarts services; the kernel restarts only the supervisor**, which it
respawns unconditionally (`CLAUDE.md` 6.2). The only component that cannot be restarted is the kernel.

- **A restart is a re-initialisation, not a resume.** In-flight work is lost; clients see `EndpointDead`, reacquire
  by name and must re-derive anything they held from the old instance (`CLAUDE.md` 14.3).
- **Recovery must not pretend.** An operation whose outcome cannot be established is reported as unknown, never
  as success or failure (the standard library's `OutcomeUnknown`; `CLAUDE.md` 26.7).
- **Guarantees are per subsystem, and written down there.** For example, `fs` is restartable everywhere and
  crash-recoverable only on a storage device that honours a flush (`CLAUDE.md` 6.1, 2026-07-25 and 2026-09-23
  amendments).

### 2.4 Portability

Four ports are complete (x86-64, ARMv7, AArch64, RISC-V 64) and three are boot-only stubs (`CLAUDE.md` 4.1).
Architecture-neutral code reaches hardware only through the `arch::imp` seam, and that is checked mechanically
(`scripts/arch_boundary_check.py`, `scripts/arch_seam_check.py`, `scripts/shared_surface_check.py`).

Every portability claim states which of these it means, because passing one does not establish the next:

1. source-level portability;
2. it compiles;
3. it runs in an emulator;
4. it runs on physical hardware;
5. it behaves the same on every port.

The project has evidence for 1 to 4 on all four complete ports. It has no systematic evidence for 5: suites run in
QEMU, and hardware is checked by hand-run "cards" (section 4.2).

## 3. The assurance framework, and where the evidence is today

| Area | Question | Evidence today | Not yet covered |
|---|---|---|---|
| Kernel boundary | What does the kernel do? | MISCIS (`CLAUDE.md` 4.3), `scripts/commandments.py` and its red-team probes (`scripts/commandments_redteam.py`), the surface baseline; every syscall arm gated or ungated by record (`scripts/syscall_gate_check.py`) | Ops within a syscall are not counted |
| Memory isolation | Can a task reach memory outside its grant? | Identity Test 7, property P4 and P7, adversarial A3, A10, A14 and A15 (`osdev/src/validator.rs`) | No hardware run of these suites |
| Capabilities | Can a task obtain or use rights it was not given? | Identity Tests 2, 3, 5, 9 and 14; property P1, P3 and P9; adversarial A1, A2, A4, A5, A9, A11 to A13; bounded Kani proofs of the capability files (`tests/kani`) and their host tests on every build | Restart against revocation is covered for files (Test 14), not as a general property |
| IPC | Are messages and queues bounded and validated? | Queue depth 16 (`CLAUDE.md` 8.5); property P6 and P10; fuzz F5 and F6; Test 4; `osdev test reply-dead` | |
| Scheduling | Can a task starve others? | Test 8; adversarial A8; perf B3 and B10 | No latency bound is claimed or measured under load |
| Recovery | What happens when a component dies? | Tests 11, 13 and 15; `chaos` (kill, flood, memory and spawn storms, max-carnage) in QEMU and on all five boards | Recovery is observed, not proved; storms are bounded runs |
| Filesystem | What durability and consistency hold? | Journal and replay suites; an independent model (`osdev test fs-model`); unassisted power cuts recovered on all five boards (`docs/gsfs-carnage.md`) | Section 3.6 of that document: stale identity is partly covered |
| Drivers | Can a driver failure harm others? | Drivers are restartable services; IOMMU confinement on x86 (Test 12, `docs/iommu.md`) | No IOMMU on the ARM and RISC-V boards: a compromised DMA driver is kernel-equivalent there |
| Executable loading | Are images and permissions validated? | `kernel/src/loader.rs`; fuzz F3 (bit-flipped ELFs); W^X (`CLAUDE.md` 18.5); the spawn request checks (`CLAUDE.md` 14.1) | No signature over built-in service images; installed packages are signed (section 8, C) |
| Unsafe Rust | Where and why? | Every block listed in `audits/unsafe-audit.md` and checked against source on every build (`scripts/unsafe_check.py`); services forbid `unsafe` at the compiler | The SDK's wrapper layer has 80-odd call-site blocks a safe wrapper would remove (`CLAUDE.md` 18.5) |
| Portability | Which board showed what? | Release notes and `docs/` tables per board | Hardware serial logs are not published (section 4.2) |
| Release quality | Can a result be reproduced? | Every suite is an `osdev test` command; artefacts and `SHA256SUMS` are built by CI from the tag | QEMU suites are run locally, not in CI |
| Known limitations | What is unverified? | Recorded where the claim is made (`CLAUDE.md` 26.7); `backlog/`; the security audit (`audits/security-audit.md`) | This table's last column |

A passing test shows the tested behaviour happened under the tested conditions. It does not show universal
correctness.

## 4. What independent scrutiny needs

### 4.1 Published invariants

The invariants are published in `CLAUDE.md` (the constitution) and distilled in `COMMANDMENTS.md`, including what
Godspeed deliberately refuses to do (`CLAUDE.md` 2.5, 4.4). Each should be stated so a reviewer can recognise a
violation. Examples of that form:

- A task cannot acquire authority by knowing a resource's name.
- Delegation cannot create authority the delegating task does not hold.
- A service restart does not restore a revoked capability.
- An unresponsive service cannot block unrelated kernel progress indefinitely.
- A failed or unknown operation is never reported as a success.
- Architecture-specific detail does not escape `arch/`.

Where an invariant is not yet established by a test or an argument, its status says so.

### 4.2 Reproducible evidence

A developer can run every QEMU suite (`osdev test <suite>`; `osdev/CLAUDE.md` lists them) and read what each check
asserts in `osdev/src/validator.rs` and `osdev/src/shell_test.rs`. Evidence comes in four kinds, and each should be
labelled as one of them:

- **Emulator testing.** Reproducible command lines; QEMU's devices are not the boards' (its serial port is
  instant, its IOMMU does not fault, it has no `\_PTS` gate).
- **Physical-hardware testing.** Five boards: HP T630, Dell Wyse 5070, Raspberry Pi 2, Raspberry Pi 4, StarFive
  VisionFive 2 Lite. Each "card" is a hand-run sequence at the prompt, read from the serial log.
- **Stress and fault injection.** `chaos` storms, power cuts, injected I/O errors and crash windows, a lying-flush
  device model.
- **Untested or partially tested.** Recorded beside the claim, and in section 9.

**Two gaps a reviewer will find, recorded here rather than left to be found:**

1. **The hardware serial logs are not in the repository.** Cards are captured to `build/`, which is not tracked,
   and published only as excerpts in `docs/` and release notes. A claim such as "tested on five boards" is
   therefore supported by the project's word and excerpts, not by logs a reviewer can read in full. Committing each
   release's card logs, named by board and commit, would close this.
2. **The test suites have not run on hardware.** The bare-metal images leave out the test probes, so the identity,
   property, fuzz and adversarial suites run in QEMU only; hardware is exercised through the shell
   (`docs/verification-road-map.md`, note of 2026-10-09).

Release evidence names the revision (every boot banner prints its commit), the platform, the procedure and the
outcome.

### 4.3 Adversarial review

Reviewers are asked to challenge the implementation, not to confirm that it boots. The priority areas are
capability validation, delegation and revocation; IPC validation, queue bounds and cancellation; memory ownership
and mapping; ELF loading and executable permissions; filesystem durability; interrupt handling and driver
isolation; supervisor restart and authority recovery; and the `unsafe` code the trusted base cannot avoid.

Methods that fit: manual adversarial review, fuzzing, property-based tests, targeted static analysis,
boundary-condition tests, fault injection, and model checking or formal reasoning for narrowly scoped invariants.
**Formal verification is limited to bounded model checking of the capability code** (Kani, `tests/kani`: rights only narrow, a cap validates exactly when its generation and right match, a generation bump invalidates every older cap, the table's lookup after a revoke or a death), each proof holding within its stated bound; nothing beyond that has been formally verified. Chaos testing shows observed outcomes; it is not an analysis of a
security boundary, and nothing here claims it is.

Security findings go privately through `SECURITY.md`; fixed ones are recorded in `audits/security-audit.md`.

### 4.4 AI assistance

Godspeed is developed with extensive AI assistance; commits made with it say so in their trailers. Architectural
direction, acceptance criteria and release decisions are the maintainer's.

AI assistance is not independent verification of the code it produces, and its use does not by itself make code
wrong, insecure or unmaintainable. The same standard applies to every line however it was written: the
architecture, the source, the recorded decisions, the reproducible tests and the observed behaviour.

The project neither hides AI involvement nor makes it its identity. The evidence for how it is used is public: the
commit history, the design documents, the defect investigations in `bugs/` and `backlog/`, and the corrections
recorded in amendments and release notes.

## 5. Independent maintainability

The stronger test of maintainability is whether a systems programmer new to Godspeed can, for one subsystem:

1. understand it from its interfaces, source and documentation;
2. build and test it on its own;
3. find a defect or a violated invariant;
4. produce a reproducible failure;
5. fix it;
6. verify the fix without the original author explaining the internals.

`CLAUDE.md` 22.7 (the Stranger Test, `docs/stranger-test.md`) measures a narrower version of this with the least
capable model available, so that the result measures the interfaces rather than the reader. It has not yet been run
with a human. Documentation helps only where it lets someone change the system correctly; it is no substitute for
comprehensible code, stable interfaces and reproducible tests.

## 6. Defects

Defects are expected in experimental systems software, and each is judged by its technical consequence, not by
how the affected code was written. For a significant one:

1. establish a reproducer where feasible;
2. name the violated invariant or contract;
3. find the root cause;
4. assess the impact and the affected platforms;
5. fix it;
6. add regression coverage where practical;
7. look for the same defect class on related paths;
8. record the fix and any uncertainty left.

`bugs/` holds long-form investigations, `backlog/` the open items. A vulnerability is not evidence that every
design decision is wrong; passing tests is not evidence that none remain.

## 7. Claims and evidence

Every statement about behaviour is one of four kinds, and should say which:

- **Design requirement:** a property the architecture is meant to provide.
- **Verified observation:** a behaviour shown by a named test or observation under named conditions.
- **Inference:** a conclusion the evidence supports but does not directly show.
- **Unknown:** no adequate evidence either way.

The repository already uses words for these, and should use them consistently: **built** (it exists), **QEMU-verified**
and **hardware-verified** (observed, with the board named), **built, not seen** (exists but never observed doing
the thing), and **recorded, not fixed** (a known gap, stated where the claim is made).

The distinction matters most for recovery, security, storage and hardware, where the tempting claim is a size
larger than the evidence:

- A CPU halt is not electrical power-off. (`shutdown` says which it got: the Wyse powers off, the T630 halts and
  says why, `docs/shutdown.md` 5.)
- A service restart is not restoration of its previous state.
- A completed write request is not proof of durable persistence.
- An absence of observed failures is not proof that no failure mode exists.

## 8. Review priorities before stronger claims

These areas need the most scrutiny before any security or production-readiness claim. Listing a mechanism here
does not mean it exists; where it does not, that is said.

**A. The trusted computing base.** Kernel memory isolation; capability checks and transfer; IPC validation;
scheduler and interrupt invariants; the `unsafe` boundaries.

**B. Recovery and persistence.** Supervisor restart; service recovery against revocation; `fs` crash consistency;
interrupted operations; the backend-conditional durability guarantee.

**C. Executable trust.** What exists: ELF validation in the kernel loader, a versioned spawn request the
kernel checks field by field, and, for an INSTALLED package (`godspeed install`), a signed manifest the supervisor
checks against this machine's trust record before every start (`CLAUDE.md` 14.1, 2026-10-17 amendment). **What
does not:** any signature over a built-in service image, which the supervisor carries inside `kernel.bin`. A
compromised supervisor can start arbitrary code under any name (`CLAUDE.md` 14.1, 2026-09-26 amendment), and the
trust record an installed package is checked against is on `/system`, which every `fs` holder can write (the
2026-10-17 amendment records it).

**D. Cross-platform behaviour.** The arch seam; differences between boards and their firmware; driver isolation
where there is no IOMMU; the repeatability of release cards.

## 9. Limitations

GodspeedOS is experimental software (`CLAUDE.md` 2.2). These stay explicit until stronger evidence exists:

- Passing tests does not prove the absence of vulnerabilities.
- Isolation depends on the correctness of the kernel's mechanisms, which have not been independently reviewed,
  and of which only narrow properties of the capability code are formally checked (bounded, `tests/kani`).
- On the Pis and the VisionFive no IOMMU confines DMA, so a compromised DMA-capable driver has kernel-equivalent
  reach; on x86, two drivers are confined (`CLAUDE.md` 6.4).
- A restart does not preserve interrupted work.
- `fs` recovers from a crash only on a device that honours a flush.
- Boards, firmware and emulators behave differently; QEMU hides timing and device behaviour that real boards
  show.
- Four ports compiling and booting does not establish identical behaviour on each.
- No built-in service image is signed; only installed packages are (section 8, C).
- The test suites run in QEMU only, and the hardware logs are not published (section 4.2).
- AI-assisted code needs the same review and verification as any other.
- No independent security review has been performed, and formal verification covers only the bounded capability
  proofs above. Neither more will be claimed until it has been done.

The aim is failures that are containable, diagnosable and recoverable where possible, not a claim that they cannot
happen.

## 10. Engineering principles

- **Architecture before implementation.** A feature respects the existing boundaries; it does not widen a
  responsibility because that is convenient.
- **Authority is explicit.** Names, paths and identities grant nothing by themselves.
- **Failure is expected.** Components are designed and tested for interruption, termination and recovery.
- **Recovery is truthful.** An unknown outcome is never reported as a success.
- **Evidence over assertion.** A claim cites a reproducible test, an analysis or a recorded observation.
- **Review over reputation.** Code is judged by its behaviour, correctness, maintainability and security, not by
  the tools that produced it.
- **Limitations are visible.** Unverified properties and known defects are written down where the claim is made.
- **Scrutiny is welcome.** A reviewer who finds a real defect improves the project.

These restate commitments already in `CLAUDE.md` 25 and 26, where they are the law.

## 11. Closing

GodspeedOS does not claim perfection. It does not claim that its architecture eliminates bugs, that testing
proves correctness, or that AI assistance guarantees quality. Its approach is narrow responsibilities, explicit
authority, isolated failure, restartable services and behaviour that can be tested.

Criticism backed by source analysis, a reproducible failure or a violated invariant is welcome. A defect is
investigated and fixed whoever or whatever wrote the code. The aim is not to defend a way of working; it is an
operating system whose claims can be examined, challenged and improved.

---

*This document records engineering expectations and review priorities. It is updated as the implementation, the
test coverage and any independent assurance change.*
