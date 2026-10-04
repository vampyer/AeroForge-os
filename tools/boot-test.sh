#!/usr/bin/env bash
# Boots the ISO headless in QEMU and passes if every CPU comes online and the
# user-mode IPC demo completes (aerosmss starts echod and three clients, each
# client finishes its round trips), and both the NVMe (GPT) and SATA (MBR)
# test disks get mounted. Intended for CI (design doc, Phase 0).
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}
TIMEOUT=${TIMEOUT:-120}
LOG=build/boot-test.log

make iso >/dev/null
./tools/make-disk.sh >/dev/null
./tools/make-sata-disk.sh >/dev/null 2>&1
cp "$OVMF_VARS" build/test-vars.fd
rm -f "$LOG"

qemu-system-x86_64 -M q35 -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/test-vars.fd \
    -drive file=build/disk.img,if=none,id=nvm,format=raw -device nvme,serial=AERO0001,drive=nvm \
    -drive file=build/sata.img,if=none,id=sata,format=raw -device ide-hd,drive=sata,bus=ide.1,serial=AEROSATA1 \
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
        grep -q "mounted at /" "$LOG" || { echo "FAIL: NVMe FAT32 volume not mounted"; exit 1; }
        grep -q "mounted at /sata0p1" "$LOG" || { echo "FAIL: SATA FAT32 volume not mounted"; exit 1; }
        grep -q "read /system/session.cfg" "$LOG" || { echo "FAIL: aerosmss did not read its config from disk"; exit 1; }
        echo "PASS: booted, mounted the NVMe and SATA disks, aerosmss read its config, IPC round trips completed"; exit 0
    fi
    sleep 1
done
echo "FAIL: no prompt within ${TIMEOUT}s"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
