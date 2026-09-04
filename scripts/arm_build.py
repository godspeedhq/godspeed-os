#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Reproducible ARM32 (Raspberry Pi 2) build for GodspeedOS.

Cross-compiles the SDK + every arm-ported userspace service to armv7a-none-eabi,
then builds the kernel (which embeds those service ELFs via kernel/build.rs) and
objcopies it to a flat kernel7.img the Pi firmware / QEMU raspi2b can boot.

`osdev` is x86-only; this is the ARM equivalent of `osdev build` until ARM is a
first-class osdev target. Usage:

    python scripts/arm_build.py [--feature arm-supervisor] [--release]

The default feature is arm-supervisor (the full stack: supervisor -> events +
ping/pong). The kernel spawns only the supervisor (C1-1), so there is no kernel-spawned
bring-up build any more - the supervisor path IS the bring-up path.
"""
import argparse, subprocess, sys, os, shutil
import io
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = "armv7a-none-eabi"

# Services that build for ARM (arch-neutral: SDK + syscalls only, no x86 hardware
# probe). Must stay in sync with `arm_built` in kernel/build.rs. Hardware drivers
# DERIVED from `kernel/build.rs`, not declared here. Two lists of one fact is the shape that keeps
# biting: `kernel/build.rs` decides which services are embedded as REAL ELFs on ARM (`arm_built`), and
# this script decides which are CROSS-COMPILED - and you cannot embed what was never built. They are the
# same set, and keeping a second copy here means a service added to one and not the other silently
# becomes a PLACEHOLDER: the kernel builds, the image builds, and the failure is a runtime
# `LoadFailed(TooSmall)` at spawn.
#
# That is not hypothetical. It is the THIRD time this exact drift has landed (the scheduler's
# death-notification set vs the supervisor's watched set was the second, and is now derived too), and it
# cost the `console` service its first boot on hardware: everything reported success, the display just
# quietly stayed on the kernel's boot floor.
#
# So parse the kernel's list and use it. `kernel/build.rs` is the single source of truth because it is
# the one that must be right for the IMAGE to be right - this script only feeds it.
def _arm_services():
    kb = os.path.join(ROOT, "kernel", "build.rs")
    src = io.open(kb, encoding="utf-8").read()
    m = re.search(r"let arm_built: &\[&str\] = &\[(.*?)\n    \];", src, re.S)
    if not m:
        raise SystemExit(
            "arm_build: cannot find `arm_built` in kernel/build.rs. It is the source of truth for which\n"
            "services are built + embedded on ARM; refusing to guess, because guessing wrong produces a\n"
            "placeholder ELF and a service that fails to spawn at runtime with LoadFailed(TooSmall)."
        )
    # Strip comments, then take every quoted name.
    body = re.sub(r"//[^\n]*", "", m.group(1))
    return re.findall(r'"([a-z0-9-]+)"', body)


ARM_SERVICES = _arm_services()


def _check_embed():
    """Same gate the Pi 4 build runs: every service the supervisor spawns must be embedded for real."""
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import service_embed_check
    service_embed_check.enforce(ROOT, "arm")


def run(cmd):
    print(">", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=ROOT)
    if r.returncode != 0:
        print("FAILED:", " ".join(cmd), file=sys.stderr)
        sys.exit(r.returncode)


# `orr rX, rX, #0xC0000000` - the VideoCore bus alias `services/dwc2` applies to every DMA address on
# real silicon, and must NOT be present in a `--qemu` build (QEMU addresses RAM directly). The encoded
# word is 0x_3822103 with the condition nibble varying, so match the low three bytes as stored
# little-endian: 03 21 82 <cond|8>.
_ALIAS_SIG = bytes([0x03, 0x21, 0x82])


def verify_image(img_path, want_qemu):
    """Assert the built image embeds the DWC2 variant that was actually asked for.

    This exists because the failure it catches is SILENT and reaches hardware: the kernel embeds each
    service ELF at compile time, so a stale embed produces an image that builds, boots, and is wrong -
    diagnosed an hour later as a driver fault. `--qemu` on a Pi means USB DMA to the wrong physical
    addresses (no keyboard); no `--qemu` under emulation means a DATA-stage STALL.

    Checking the ARTIFACT rather than the build steps is the point: it is the only thing that cannot be
    fooled by a caching or ordering mistake in the steps above.
    """
    img = io.open(img_path, "rb").read()
    found = _ALIAS_SIG in img
    if want_qemu and found:
        raise SystemExit(
            "BUILD VERIFY FAILED: --qemu was requested but the image embeds the HARDWARE DWC2 "
            "(VideoCore bus alias present). Under emulation this STALLs in the DATA stage. "
            "The embedded service ELF is stale."
        )
    if not want_qemu and not found:
        raise SystemExit(
            "BUILD VERIFY FAILED: a hardware build must embed the VideoCore bus alias and this image "
            "does not - it has the --qemu DWC2 in it. On a real Pi that DMAs to the wrong physical "
            "addresses and USB does not work (no keyboard). The embedded service ELF is stale."
        )
    print("verify: DWC2 DMA target is %s, as requested" % ("QEMU (identity)" if want_qemu else "hardware (VideoCore alias)"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--feature", default="arm-supervisor",
                    help="kernel boot-path feature (arm-supervisor)")
    ap.add_argument("--release", action="store_true")
    ap.add_argument("--qemu", action="store_true",
                    help="target QEMU emulation (identity DWC2 DMA in services/dwc2); "
                         "default is real-Pi hardware (VideoCore bus alias)")
    args = ap.parse_args()
    profile = "release" if args.release else "debug"
    rel = ["--release"] if args.release else []
    kfeatures = args.feature

    # 0. THE COMMANDMENTS GATE.
    #
    # `osdev build` has run these since the enforcement layer was written. This script never did - and
    # this script is what builds every ARM image, so for the whole arm32 effort the checks existed and
    # were not consulted. That is not a discipline problem, it is a missing gate: a rule enforced on one
    # path and not the other is enforced on neither, because work flows down the ungated path.
    #
    # Failing the BUILD rather than warning is deliberate. A warning scrolls past above a successful
    # image; a build that refuses to produce one cannot be ignored, and cannot ship.
    for check in ("commandments.py", "dash_check.py", "unsafe_check.py",
                  "arch_boundary_check.py", "contract_check.py"):
        r = subprocess.run([sys.executable, os.path.join("scripts", check)],
                           cwd=ROOT, capture_output=True, text=True)
        if r.returncode != 0:
            sys.stdout.write(r.stdout)
            sys.stderr.write(r.stderr)
            raise SystemExit(
                "\nBUILD REFUSED: %s failed. Fix the violation, or amend CLAUDE.md and cite\n"
                "the amendment - those are the only two ways past this, by design." % check)
    print("commandments + dash + unsafe + arch-boundary + contracts: pass")

    # 1. Cross-compile every ARM-ported service to armv7 so build.rs can embed them.
    #    The Pi 2 is a bare-metal target (no QEMU control port), so the supervisor is built with its
    #    `bare-metal` feature - the designated "usable OS, quiet gsh> prompt" spawn set (events + shell,
    #    no 178 harness probes, no ping/pong flood). ping/pong are spawnable on demand from the shell.
    #
    #    `--qemu` goes to `dwc2` and NOWHERE ELSE, because `dwc2` is the only crate that reads it:
    #    `DMA_BUS_ALIAS` in `services/dwc2/src/regs.rs` is 0 under emulation and the VideoCore alias
    #    0xC000_0000 on silicon. It used to be passed to the KERNEL instead, where the feature existed
    #    but nothing read it - so `--qemu` was a silent no-op and USB under emulation ran with the
    #    hardware alias, which STALLs in the DATA stage. A flag that selects nothing is worse than an
    #    absent one: it reports success and you debug the wrong layer.
    _check_embed()
    # THE SUPERVISOR IS BUILT LAST, because it `include_bytes!`s every other service. Built earlier -
    # it was index 4 of 24 - cargo reuses the previous run's binaries for everything after it, and the
    # image ships services one build behind next to a current kernel. Nothing fails and nothing warns.
    # Caught when a shell change verified on x86 was demonstrably absent from `kernel7.img`.
    ordered = [s for s in ARM_SERVICES if s != "supervisor"] +               (["supervisor"] if "supervisor" in ARM_SERVICES else [])
    for svc in ordered:
        feats = []
        if svc == "supervisor":
            feats = ["--features", "bare-metal"]
        elif svc == "dwc2" and args.qemu:
            feats = ["--features", "qemu"]
        run(["cargo", "build", "-p", svc, "--target", TARGET] + feats + rel)

    # 1a2. THE SUPERVISOR MUST BE NEWER THAN EVERYTHING IT EMBEDS - ordering made enforceable.
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import embed_order_check
    embed_order_check.enforce(ROOT, TARGET, profile, ARM_SERVICES)

    # 1b. EVERY SERVICE MUST FIT ITS OWN STACK.
    #
    #     The kernel gives each service a 256 KiB user stack (`USER_STACK_PAGES = 64`, task/mod.rs).
    #     A service whose `service_main` frame is larger than that faults on the FIRST STORE of its own
    #     prologue - before a line of its code runs, and before it can say anything about why.
    #
    #     This is checkable here and nowhere else useful: it is a fixed number, sitting in the binary,
    #     readable in a second. Left unchecked it presents on HARDWARE as a service that spawns and
    #     instantly dies, forever, with the supervisor faithfully restarting it - `fs` crash-looped at
    #     roughly 30 restarts a second, and the restart storm interleaved the serial log badly enough to
    #     corrupt the network lines being debugged. The fault address said "stack"; the first reading of
    #     it was a memory bug in the filesystem.
    #
    #     A DEBUG build is the case that bites. With no optimisation every by-value move of a large
    #     struct is a separate copy, so `fs`'s 36 KiB mount record becomes a 503 KiB frame - twice the
    #     stack it is given. The same service built release is 155 KiB and fits. So a debug image is not
    #     "the same image, slower": for some services it is an image that cannot run at all, and until
    #     now the only signal was on hardware. It is a build refusal with the number in it instead.
    stack_limit = 64 * 4096   # USER_STACK_PAGES * PAGE_SIZE, kernel/src/task/mod.rs
    objdump = shutil.which("rust-objdump") or "rust-objdump"
    # One gate, shared with the Pi 4 build (scripts/stack_fit_check.py). This was an inline copy that
    # checked `service_main` ALONE; the shared one checks every function, which is what Linux's
    # `-Wframe-larger-than` does and what this copy could not - `service_main` is rarely the deepest
    # frame, only the one somebody thought to look at.
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import stack_fit_check
    stack_fit_check.enforce(objdump, ROOT, TARGET, profile, ARM_SERVICES, stack_limit)

    # 2. Build the kernel (embeds the service ELFs) with the chosen boot path.
    #
    #    TOUCH build.rs FIRST, unconditionally. The kernel embeds each service with
    #    `include_bytes!(env!("SVC_*_ELF"))` and its build script emits `cargo:rerun-if-changed` for
    #    every one - which SHOULD be enough, and twice in one session it was not: a service was rebuilt
    #    and the kernel kept the previous copy, producing an image that built cleanly and was wrong.
    #    Once it embedded a placeholder (the `console` service failed to spawn); once it embedded the
    #    `--qemu` DWC2 on a hardware build (DMA at the wrong physical addresses, so no keyboard).
    #
    #    Both times the symptom appeared on HARDWARE, minutes later, looking like a driver bug. The
    #    cost of being wrong is that; the cost of always re-running build.rs is a kernel recompile.
    #    That is not a close trade.
    kernel_build_rs = os.path.join(ROOT, "kernel", "build.rs")
    os.utime(kernel_build_rs, None)
    run(["cargo", "build", "-p", "kernel", "--target", TARGET,
         "--features", kfeatures] + rel)

    # 3. Flatten to a raw image the Pi firmware / QEMU loads at 0x8000.
    kelf = os.path.join(ROOT, "target", TARGET, profile, "kernel")
    out_dir = os.path.join(ROOT, "build")
    os.makedirs(out_dir, exist_ok=True)
    img = os.path.join(out_dir, "kernel7.img")
    objcopy = shutil.which("rust-objcopy") or "rust-objcopy"
    run([objcopy, "-O", "binary", kelf, img])

    # Stage the canonical Pi 2 boot config next to the kernel, so deploying is copying TWO files
    # (kernel7.img + config.txt) from build/ - the config the boot depends on is versioned in the repo
    # (boot/pi2/config.txt), never re-typed onto a card by hand (docs/pi2-deploy.md).
    cfg_src = os.path.join(ROOT, "boot", "pi2", "config.txt")
    if os.path.exists(cfg_src):
        shutil.copyfile(cfg_src, os.path.join(out_dir, "config-pi2.txt"))

    # Verify the ARTIFACT, not the steps: a stale embed is silent and reaches hardware (verify_image).
    verify_image(img, args.qemu)

    size = os.path.getsize(img)
    print(f"\nOK  build/kernel7.img  ({size} bytes, feature={kfeatures}, profile={profile})")
    print("Boot in QEMU:  python scripts/arm_run.py")
    print("Deploy to Pi:  copy build/kernel7.img, and build/config-pi2.txt AS config.txt, to the card")
    print("               (full procedure incl. the storage USB stick: docs/pi2-deploy.md)")


if __name__ == "__main__":
    main()
