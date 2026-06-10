#!/usr/bin/env bash
# Boot the bare-metal image under QEMU with an emulated AMD-Vi IOMMU and a
# qemu-xhci controller behind it, for H1 (DMA-confinement) development.
#
# Usage: scripts/qemu_iommu.sh [seconds] [logfile] [extra-qemu-args...]
#   seconds  - how long to run before killing QEMU (default 22)
#   logfile  - serial capture path (default build/iommu_qemu_serial.log)
# Pass "noiommu" as the 3rd arg to omit the IOMMU (negative case).
set -u
SECS="${1:-22}"
LOG="${2:-build/iommu_qemu_serial.log}"
MODE="${3:-iommu}"
QEMU="/c/Program Files/qemu/qemu-system-x86_64.exe"
OVMF_CODE="/c/Program Files/qemu/share/edk2-x86_64-code.fd"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cp "/c/Program Files/qemu/share/edk2-i386-vars.fd" build/OVMF_VARS.fd
IMG="build/iommu_run.img"
cp build/os.img "$IMG"
rm -f "$LOG"

IOMMU_ARGS=(-device amd-iommu)
[ "$MODE" = "noiommu" ] && IOMMU_ARGS=()

"$QEMU" \
  -machine q35 -m 2G -smp 4 \
  -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
  -drive if=pflash,format=raw,file=build/OVMF_VARS.fd \
  "${IOMMU_ARGS[@]}" \
  -device qemu-xhci,id=xhci \
  -drive id=disk,file="$IMG",format=raw,if=none \
  -device ide-hd,drive=disk,bus=ide.0 \
  -serial file:"$LOG" \
  -display none -no-reboot &
QPID=$!
echo "qemu pid $QPID (mode=$MODE, ${SECS}s) -> $LOG"
sleep "$SECS"
kill $QPID 2>/dev/null
echo "=== iommu serial lines ==="
tr -d '\r' < "$LOG" 2>/dev/null | grep -iE "iommu|ivrs|panic" | head -40
