# GodspeedOS Licensing

> **Status:** Licensing **intent / policy**, decided in conversation. This records
> the *strategy* and the *rationale*; the actual `LICENSE` files (full legal text)
> are added when the choices below are finalized. Not yet a legal instrument.

## 1. The decision

| Component | License | Why |
|-----------|---------|-----|
| **Kernel** (and the trusted root: `init`, `supervisor`, `registry`, arch, smp) | **GPL copyleft** (like Linux) | The OS itself stays free and open; improvements flow back. |
| **SDK** (`sdk/rust`) | **Permissive** (MIT or Apache-2.0) - *to decide* | Services link it; permissive keeps the app ecosystem free to choose any license. |
| **Userspace services / apps** | **Any license** (their authors' choice) | They are separate programs over IPC - not derivative works of the kernel. |
| **Bundled bootloader** (Limine) | **BSD-2-Clause** (upstream; preserve its notice) | A separate program; GPL-compatible (§3). |

The one-line summary: **copyleft OS, permissive ecosystem.** GodspeedOS-the-OS is
GPL and open; what you build *on* it is yours.

## 2. The license boundary is the capability/IPC boundary

A microkernel gives GodspeedOS something Linux never cleanly had: an **unambiguous
non-derivative line.**

- Userspace services are **separate programs** that talk to the kernel only over the
  syscall ABI / capability IPC. They are never linked into the kernel, never share its
  address space (§2.2, §3.2, §10.1).
- Therefore a service is **not a derivative work** of the kernel. The GPL's copyleft
  reaches the kernel and the trusted root; it does **not** reach across the IPC boundary
  to userspace.
- Linux needs a "userspace programs that use syscalls are not derivative works" clarifying
  note because its boundary is fuzzier (in-kernel modules, etc.). GodspeedOS's strict
  isolation makes the boundary **structural**, not a promise - the IPC edge *is* the
  license edge.

**The SDK is the one thing that needs care,** because services *link* it. A GPL SDK could
force apps to GPL by linkage. So the SDK is **permissive** (or LGPL): the kernel stays
copyleft, but linking the SDK imposes nothing on the app. This is what keeps the
"portable world" / app ecosystem (`docs/prime.md`) open to any license.

## 3. The bundled bootloader (Limine)

- **Limine is BSD-2-Clause** (permissive). BSD-2-Clause is **GPL-compatible**: permissive
  code may be bundled with and distributed alongside a GPL project; the only obligation is
  preserving Limine's copyright notice.
- Limine is a **separate program** (the bootloader binary), not linked into the kernel -
  they communicate via the Limine Boot Protocol (a data handoff at boot, `arch/x86_64/
  boot.rs`). So the licenses do not entangle, exactly as a GPL Linux booted by GPL GRUB or
  by a BSD bootloader is fine.
- **Not a lock-in:** Limine is a v1 commitment (Appendix A), not permanent. The kernel owns
  the handoff; swapping bootloaders (GRUB, direct UEFI, custom) is bounded work in one file.
  The Prime A/B model's UEFI-boot-variable variant is bootloader-agnostic (`docs/prime.md
  §8.2`). Limine is kept because it's good, not because we must.

## 4. Why copyleft for the OS

- GodspeedOS's value is the **coherence of the model** (§26.1, *"the model is the
  product"*). Copyleft keeps that model - and every improvement to it - open and shared,
  which is the point of building a small, fully-understood system in the open.
- Copyleft on the *kernel* + permissive *ecosystem* is a deliberate balance: the commons
  (the OS) stays a commons; the frontier (apps) stays free. It is the opposite of a
  permissive kernel that can be taken closed.

## 5. Open choices (to finalize before writing the `LICENSE` files)

1. **GPLv2 vs GPLv3.** "Like Linux" usually means **GPLv2-only**. **GPLv3** adds
   anti-tivoization (you can't lock users out of running modified firmware) and explicit
   patent grants - which may actually fit an OS that self-updates and runs on devices
   (`docs/prime.md`). Decide deliberately.
2. **SDK permissive license: MIT vs Apache-2.0.** Apache-2.0 adds an explicit patent grant
   and is GPLv3-compatible (but *not* GPLv2-compatible); MIT is simplest and broadly
   compatible. The SDK choice interacts with the GPLv2/v3 choice above.
3. **Where the `LICENSE` files live:** repo-root `LICENSE` (kernel GPL) + `sdk/rust/LICENSE`
   (permissive) + a `NOTICE`/`THIRD-PARTY` for Limine's BSD notice and any other deps.
4. **Contributor terms:** a `CONTRIBUTING`/DCO (sign-off) vs a CLA - how inbound
   contributions are licensed.

## 6. What this is not

This document is **intent**, not the license. Until the `LICENSE` files exist with real
text, nothing here grants or restricts rights. It exists so the *strategy* - copyleft OS,
permissive ecosystem, IPC as the boundary - is on record and the eventual legal text
matches the architecture rather than fighting it.
