#!/usr/bin/env bash
# Boots the ESP32 firmware example in Espressif's QEMU and waits for its
# success line on the serial console. Fails on a crash, an early exit or a
# timeout. Needs `espflash` and Espressif's QEMU build (`qemu-system-xtensa`
# or `qemu-system-riscv32`, from github.com/espressif/qemu) on PATH; upstream
# QEMU lacks the ESP machines.
# Usage: scripts/esp32-qemu.sh <esp32|esp32s3|esp32c3> <firmware ELF> [timeout s]
set -euo pipefail

chip=$1
elf=$2
timeout=${3:-60}
case $chip in
  esp32 | esp32s3) qemu=(qemu-system-xtensa) ;;
  # Instruction counting gives the C3 deterministic timing (Espressif's advice).
  esp32c3) qemu=(qemu-system-riscv32 -icount 3) ;;
  *) echo "unsupported chip: $chip" >&2 && exit 2 ;;
esac

work=$(mktemp -d)
pid=
trap '[ -n "$pid" ] && kill "$pid" 2>/dev/null; rm -rf "$work"' EXIT

# A merged image: bootloader, partition table and app, padded to flash size.
espflash save-image --chip "$chip" --merge "$elf" "$work/flash.bin"
log=$work/serial.log
touch "$log"
"${qemu[@]}" -nographic -machine "$chip" -monitor none \
  -drive "file=$work/flash.bin,if=mtd,format=raw" -serial "file:$log" &
pid=$!

# Keep the marker in step with examples/esp32/src/main.rs.
status=1
for _ in $(seq $((timeout * 4))); do
  # Reject any earlier crash even if a reboot later reaches the marker.
  if grep -qE 'Guru Meditation|panicked at|abort\(\) was called|Stack protection fault|stack overflow|assert failed:|memory allocation of .* failed' "$log" ||
    ! kill -0 "$pid" 2>/dev/null; then
    break
  fi
  if grep -q '^Otter ESP32 smoke test passed' "$log"; then
    status=0
    break
  fi
  sleep 0.25
done
cat "$log"
[ $status = 0 ] || echo "esp32-qemu: no success line from $chip firmware" >&2
exit $status
