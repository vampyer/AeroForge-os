#!/usr/bin/env bash
# Boots the ISO headless in QEMU and passes if every CPU comes online and the
# user-mode IPC demo completes (aerosmss starts echod and three clients, each
# client finishes its round trips), and both the NVMe (GPT) and SATA (MBR)
# test disks get mounted, and a USB keyboard and mouse get set up behind a USB hub, and the
# Intel igb card gets an address over DHCP and pings the gateway (a second,
# e1000e card must come up with a link). Intended for CI (design doc, Phase 0).
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

qemu-system-x86_64 -M q35 -cpu max -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/test-vars.fd \
    -drive file=build/disk.img,if=none,id=nvm,format=raw -device nvme,serial=AERO0001,drive=nvm \
    -drive file=build/sata.img,if=none,id=sata,format=raw -device ide-hd,drive=sata,bus=ide.1,serial=AEROSATA1 \
    -device qemu-xhci,id=xhci -device usb-hub,bus=xhci.0,port=1 \
    -device usb-kbd,bus=xhci.0,port=1.1 -device usb-mouse,bus=xhci.0,port=1.2 \
    -nic user,model=igb -nic user,model=e1000e \
    -cdrom build/aeroforge.iso -serial file:"$LOG" -display none &
QEMU_PID=$!
trap 'kill $QEMU_PID 2>/dev/null || true' EXIT

for _ in $(seq "$TIMEOUT"); do
    if grep -q "PANIC" "$LOG" 2>/dev/null; then
        echo "FAIL: kernel panic"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
    fi
    if [ "$(grep -c "done, exiting" "$LOG" 2>/dev/null)" -ge 3 ] && grep -q "rotest (pid" "$LOG" && grep -q "nxtest (pid" "$LOG" \
        && grep -q "ping 10.0.2.2: \|WARN.*\(eth\|DHCP\|Ethernet\)" "$LOG"; then
        sleep 1
        sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | sed -n '/AeroForge OS/,$p'
        grep -q "SMP: 4 of 4" "$LOG" || { echo "FAIL: not all CPUs came online"; exit 1; }
        grep -q "\[aerosmss\] all services up" "$LOG" || { echo "FAIL: aerosmss did not finish startup"; exit 1; }
        grep -q "mounted at /" "$LOG" || { echo "FAIL: NVMe FAT32 volume not mounted"; exit 1; }
        grep -q "mounted at /sata0p1" "$LOG" || { echo "FAIL: SATA FAT32 volume not mounted"; exit 1; }
        grep -q "hub 1 port 1, .*HID boot keyboard" "$LOG" || { echo "FAIL: USB keyboard behind the hub not set up"; exit 1; }
        grep -q "hub 1 port 2, .*HID boot mouse" "$LOG" || { echo "FAIL: USB mouse behind the hub not set up"; exit 1; }
        grep -q "Security audit passed" "$LOG" || { echo "FAIL: kernel security audit did not pass"; exit 1; }
        grep -q "SMEP on, SMAP on" "$LOG" || { echo "FAIL: SMEP/SMAP not enabled"; exit 1; }
        grep -q "attacks blocked, system call checks OK" "$LOG" || { echo "FAIL: sectest: a bad pointer got through a system call"; exit 1; }
        grep -q "nxtest (pid [0-9]*) killed: .*execute in a no-execute page" "$LOG" || { echo "FAIL: code on the stack was not stopped by NX"; exit 1; }
        grep -q "rotest (pid [0-9]*) killed: .*write to a read-only page" "$LOG" || { echo "FAIL: write to code was not stopped"; exit 1; }
        grep -q "eth0: Intel 8086:10c9 (igb/igc driver) .*link up" "$LOG" || { echo "FAIL: Intel igb card not set up"; exit 1; }
        grep -q "eth1: Intel 8086:10d3 (e1000 driver) .*link up" "$LOG" || { echo "FAIL: Intel e1000e card not set up"; exit 1; }
        grep -q "DHCP: eth0 got 10.0.2.15/24, gateway 10.0.2.2" "$LOG" || { echo "FAIL: no DHCP lease"; exit 1; }
        grep -q "ping 10.0.2.2: reply from 10.0.2.2" "$LOG" || { echo "FAIL: no ping reply from the gateway"; exit 1; }
        grep -q "read /system/session.cfg" "$LOG" || { echo "FAIL: aerosmss did not read its config from disk"; exit 1; }
        echo "PASS: booted, mounted the NVMe and SATA disks, set up the USB keyboard and mouse behind a hub, brought up igb and e1000e cards, got an address over DHCP and pinged the gateway, the security self-test passed, aerosmss read its config, IPC round trips completed"; exit 0
    fi
    sleep 1
done
echo "FAIL: no prompt within ${TIMEOUT}s"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
