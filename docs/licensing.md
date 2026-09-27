# GodspeedOS Licensing

> **Status:** **Settled.** This is the recorded licensing decision, now backed by real
> `LICENSE` files: the OS is **GPL-2.0-only** (repo-root `LICENSE`) and the SDK + examples
> are **Apache-2.0** (`sdk/LICENSE`). Per-file `SPDX-License-Identifier` tags mark every
> source file's zone. This document records the *strategy* and *rationale* behind that text.

## 1. The decision

| Component | License | Why |
|-----------|---------|-----|
| **The OS** - kernel + trusted root (`supervisor`, arch, smp) + system services (`events`, `block-driver`, `fs`, drivers, `shell`) + host tooling (`osdev`) + tests | **GPL-2.0-only** (like Linux) | The OS itself stays free and open; improvements flow back. |
| **SDK** (`sdk/rust`) and the **standard library** (`stdlib/rust`) | **Apache-2.0** | Services link them; permissive keeps the app ecosystem free to choose any license. Apache-2.0 adds an explicit patent grant over plain MIT. |
| **Examples** (`examples/`) | **Apache-2.0** | They are reference apps meant to be copied as the starting point for real services; permissive keeps that copy unencumbered. |
| **Userspace services / apps** | **Any license** (their authors' choice) | They are separate programs over IPC - not derivative works of the kernel. A service built against the SDK carries **no copyleft**, so proprietary commercial services are fine. |
| **Bundled bootloader** (Limine) | **BSD-2-Clause** (upstream; preserve its notice) | A separate program; GPL-compatible (§3). |

The one-line summary: **copyleft OS, permissive ecosystem.** GodspeedOS-the-OS is
GPL-2.0-only and open; what you build *on* it (against the Apache-2.0 SDK) is yours.

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

## 5. The recorded choices

These were the open questions; they are now decided:

1. **GPLv2 vs GPLv3 → GPL-2.0-only.** "Like Linux" - the OS is **GPL-2.0-only**. This is the
   plain, broadly-understood copyleft for a kernel; the anti-tivoization and patent clauses of
   GPLv3 were not worth the narrower compatibility surface for v1.
2. **SDK permissive license → Apache-2.0** (for both the SDK *and* the examples). Apache-2.0
   adds an explicit patent grant over plain MIT. Note the well-known one-way edge: Apache-2.0 is
   GPLv3-compatible but *not* GPLv2-compatible - which is exactly why the SDK is a **separate,
   permissively-licensed** component a service merely links, never code pulled into the
   GPL-2.0-only kernel. The capability/IPC boundary (§2) keeps the two zones from entangling.
3. **Where the `LICENSE` files live:** repo-root `LICENSE` (the OS, GPL-2.0-only) + `sdk/LICENSE`
   (the SDK + examples, Apache-2.0). Every source file additionally carries a one-line
   `// SPDX-License-Identifier: <id>` tag naming its zone.
4. **Contributor terms:** a `CONTRIBUTING`/DCO (sign-off) vs a CLA - how inbound
   contributions are licensed. *(Still to formalize; does not affect the license of the code in tree.)*

## 5a. Vendor device firmware, and the `nonfree/` tree

**Decided 2026-09-27.** Some devices are not fixed-function silicon. The Pi 4's CYW43455 radio carries its
own processor with no ROM firmware for the MAC, so it does nothing until a host uploads a vendor binary
into it. Nobody writes that binary - not Linux, not the Pi's own boot firmware, not Windows - and its
licence forbids modifying, reverse engineering, decompiling or disassembling it. There is no source to
read and no permission to study the object, which makes it the one exception to §26.14's method in this
whole project.

**Such firmware IS committed here**, under `nonfree/<part>/`, when its licence permits redistribution:

- `LICENCE` beside it, because the terms require the notice to accompany every copy and a repository is a
  copy;
- `PROVENANCE` - upstream URL, retrieval date, and a **SHA-256 per file**, so anyone can verify the copy
  against upstream without trusting this project;
- `scripts/nonfree_check.py` enforces both on every build. A blob with no licence, or whose digest does not
  match its content, fails the build.

**Where it does NOT permit redistribution**, the blob stays out and `scripts/get_firmware.py` fetches it on
the owner's machine. Two mechanisms for two legal situations, each declared rather than assumed.

**Why not Linux's model.** Linux keeps blobs in a separate `linux-firmware` repository, but that separation
is Debian's social contract and the DFSG rather than a technical conclusion - a non-linked binary beside
GPL code is mere aggregation, and Linux shipped them in-tree for years. GodspeedOS is not a distribution
with a package manager, and fetch-at-setup was tried and failed three times on one file in an afternoon:
the board name is a symlink, into a sibling directory, to a filename the packaging creates at build time.
Absorbing that fragility once, here, beats pushing it onto every user.

The aggregation point matters and is worth stating plainly: the firmware is **data uploaded to a device**,
not code linked into a GPL work. It does not make the OS a derivative of it, nor the reverse.

---

## 6. Scope of this document

The `LICENSE` files (repo-root GPL-2.0-only, `sdk/LICENSE` Apache-2.0) are the legal
instruments; this document records the *strategy* behind them - copyleft OS, permissive
ecosystem, IPC as the boundary - so the architecture and the license stay aligned rather than
fighting each other.
