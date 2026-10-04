#!/usr/bin/env bash
# Boots the ISO headless in QEMU and passes if every CPU comes online and the
# user-mode IPC demo completes (aerosmss starts echod and three clients, each
# client finishes its round trips). Intended for CI (design doc, Phase 0).
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}
TIMEOUT=${TIMEOUT:-120}
LOG=build/boot-test.log

make iso >/dev/null
cp "$OVMF_VARS" build/test-vars.fd
rm -f "$LOG"

qemu-system-x86_64 -M q35 -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/test-vars.fd \
    -cdrom build/aeroforge.iso -serial file:"$LOG" -display none &
QEMU_PID=$!
trap 'kill $QEMU_PID 2>/dev/null || true' EXIT

for _ in $(seq "$TIMEOUT"); do
    if grep -q "PANIC" "$LOG" 2>/dev/null; then
        echo "FAIL: kernel panic"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
    fi
    if [ "$(grep -c "done, exiting" "$LOG" 2>/dev/null)" -ge 3 ]; then
        sleep 1
        sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | sed -n '/AeroForge OS/,$p'
        grep -q "SMP: 4 of 4" "$LOG" || { echo "FAIL: not all CPUs came online"; exit 1; }
        grep -q "\[aerosmss\] all services up" "$LOG" || { echo "FAIL: aerosmss did not finish startup"; exit 1; }
        echo "PASS: AeroKernel booted, user processes completed IPC round trips"; exit 0
    fi
    sleep 1
done
echo "FAIL: no prompt within ${TIMEOUT}s"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
