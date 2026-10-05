#!/usr/bin/env bash
# Boots the ISO headless in QEMU and passes if every CPU comes online and the
# user-mode IPC demo completes (aerosmss starts echod and three clients, each
# client finishes its round trips), and both the NVMe (GPT) and SATA (MBR)
# test disks and a USB stick get mounted, and a USB keyboard and mouse get set up behind a USB hub, and the
# Intel igb card gets an address over DHCP and pings the gateway (a second,
# e1000e card must come up with a link), and a simulated MediaTek Bluetooth
# adapter (tools/fakebt) gets its firmware, comes up and finds the simulated
# gamepads in a scan; then the test types 'bt pair' and 'gamepad' at the
# shell: the classic gamepad must pair, report its input, reconnect on its own
# after it "turns off and on" and report again; then 'bt pair' and 'mic record'
# for the simulated headset: it must pair over the hands-free profile, and a
# recording must hear its 440 Hz tone, before and after it reconnects on its
# own. Intended for CI (design doc, Phase 0).
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}
TIMEOUT=${TIMEOUT:-120}
LOG=build/boot-test.log

make iso >/dev/null
./tools/make-disk.sh >/dev/null
./tools/make-sata-disk.sh >/dev/null 2>&1
./tools/make-usb-disk.sh >/dev/null 2>&1
cc -O2 -Wall -o build/fakebt tools/fakebt/fakebt.c -lusbredirparser -lm
FAKEBT_LOG=build/fakebt.log
build/fakebt build/fakebt.sock build/firmware/mediatek/BT_RAM_CODE_MT7961_1_2_hdr.bin 2>"$FAKEBT_LOG" &
FAKEBT_PID=$!
for _ in $(seq 50); do [ -S build/fakebt.sock ] && break; sleep 0.1; done
cp "$OVMF_VARS" build/test-vars.fd
rm -f "$LOG" build/qemu-monitor.sock

qemu-system-x86_64 -M q35 -cpu max -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/test-vars.fd \
    -drive file=build/disk.img,if=none,id=nvm,format=raw -device nvme,serial=AERO0001,drive=nvm \
    -drive file=build/sata.img,if=none,id=sata,format=raw -device ide-hd,drive=sata,bus=ide.1,serial=AEROSATA1 \
    -device qemu-xhci,id=xhci -device usb-hub,bus=xhci.0,port=1 \
    -device usb-kbd,bus=xhci.0,port=1.1 -device usb-mouse,bus=xhci.0,port=1.2 \
    -drive file=build/usb.img,if=none,id=stick,format=raw -device usb-storage,bus=xhci.0,port=2,drive=stick,serial=AEROUSB1 \
    -chardev socket,id=fakebt,path=build/fakebt.sock -device usb-redir,chardev=fakebt,bus=xhci.0,port=3 \
    -nic user,model=igb -nic user,model=e1000e \
    -cdrom build/aeroforge.iso -serial file:"$LOG" -display none \
    -monitor unix:build/qemu-monitor.sock,server,nowait &
QEMU_PID=$!
trap 'kill $QEMU_PID $FAKEBT_PID 2>/dev/null || true' EXIT

# In GitHub Actions a failure also becomes an annotation, readable without the log.
fail() {
    echo "FAIL: $1"
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error::$1 | $(sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | grep -a 'bt pair\|paired\|mic\|pitch\|headset\|WARN' | tail -8 | tr '\n' '|') | fakebt: $(grep -a 'FAIL\|voice\|reconnect' "$FAKEBT_LOG" | tail -6 | tr '\n' '|')"
    fi
    shift
    for f in "$@"; do cat "$f"; done
    exit 1
}

type_keys() { python3 tools/qemu-type.py build/qemu-monitor.sock "$1"; }
STAGE=0

for _ in $(seq "$TIMEOUT"); do
    # The Bluetooth gamepad test types at the shell as the log advances.
    if [ $STAGE = 0 ] && grep -q "hci0: scan found" "$LOG" 2>/dev/null; then
        sleep 2; type_keys $'bt pair 11:22:33:44:55:66\n'; STAGE=1
    elif [ $STAGE = 1 ] && grep -q "paired and connected\|bt pair:" "$LOG"; then
        sleep 1; type_keys $'gamepad\n'; STAGE=2
    elif [ $STAGE = 2 ] && [ "$(grep -c 'gamepad 11:22:33:44:55:66 .* connected (' "$LOG")" -ge 2 ]; then
        sleep 1; type_keys $'gamepad\n'; STAGE=3
    elif [ $STAGE = 3 ] && grep -q "buttons: 2 12" "$LOG"; then
        sleep 1; type_keys $'bt pair 00:DE:AD:BE:EF:01\n'; STAGE=4
    elif [ $STAGE = 4 ] && grep -q "headset with microphone\|bt pair: .*" "$LOG"; then
        sleep 1; type_keys $'mic record 2\n'; STAGE=5
    elif [ $STAGE = 5 ] && [ "$(grep -c 'headset 00:DE:AD:BE:EF:01 .* connected (' "$LOG")" -ge 2 ]; then
        sleep 1; type_keys $'mic record 1\n'; STAGE=6
    fi
    if grep -q "PANIC" "$LOG" 2>/dev/null; then
        fail "kernel panic" "$LOG"
    fi
    if [ "$(grep -c "done, exiting" "$LOG" 2>/dev/null)" -ge 3 ] && grep -q "rotest (pid" "$LOG" && grep -q "nxtest (pid" "$LOG" \
        && grep -q "ping 10.0.2.2: \|WARN.*\(eth\|DHCP\|Ethernet\)" "$LOG" \
        && { [ $STAGE = 6 ] && [ "$(grep -c 'pitch about\|mic: ' "$LOG")" -ge 2 ] || grep -q "WARN.*hci0\|bt pair:\|mic: " "$LOG"; }; then
        sleep 1
        sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | sed -n '/AeroForge OS/,$p'
        grep -q "SMP: 4 of 4" "$LOG" || { fail "not all CPUs came online"; }
        grep -q "\[aerosmss\] all services up" "$LOG" || { fail "aerosmss did not finish startup"; }
        grep -q "mounted at /" "$LOG" || { fail "NVMe FAT32 volume not mounted"; }
        grep -q "mounted at /sata0p1" "$LOG" || { fail "SATA FAT32 volume not mounted"; }
        grep -q "mounted at /usb0p1" "$LOG" || { fail "USB stick FAT32 volume not mounted"; }
        grep -q "hub [0-9]* port 1, .*HID boot keyboard" "$LOG" || { fail "USB keyboard behind the hub not set up"; }
        grep -q "hub [0-9]* port 2, .*HID boot mouse" "$LOG" || { fail "USB mouse behind the hub not set up"; }
        grep -q "Security audit passed" "$LOG" || { fail "kernel security audit did not pass"; }
        grep -q "SMEP on, SMAP on" "$LOG" || { fail "SMEP/SMAP not enabled"; }
        grep -q "attacks blocked, system call checks OK" "$LOG" || { fail "sectest: a bad pointer got through a system call"; }
        grep -q "nxtest (pid [0-9]*) killed: .*execute in a no-execute page" "$LOG" || { fail "code on the stack was not stopped by NX"; }
        grep -q "rotest (pid [0-9]*) killed: .*write to a read-only page" "$LOG" || { fail "write to code was not stopped"; }
        grep -q "eth0: Intel 8086:10c9 (igb/igc driver) .*link up" "$LOG" || { fail "Intel igb card not set up"; }
        grep -q "eth1: Intel 8086:10d3 (e1000 driver) .*link up" "$LOG" || { fail "Intel e1000e card not set up"; }
        grep -q "DHCP: eth0 got 10.0.2.15/24, gateway 10.0.2.2" "$LOG" || { fail "no DHCP lease"; }
        grep -q "ping 10.0.2.2: reply from 10.0.2.2" "$LOG" || { fail "no ping reply from the gateway"; }
        grep -q "hci0: MediaTek MT7961 firmware .* loaded" "$LOG" || { fail "MediaTek Bluetooth firmware not loaded" "$FAKEBT_LOG"; }
        grep -q "firmware OK" "$FAKEBT_LOG" && ! grep -q "FAIL" "$FAKEBT_LOG" || { fail "simulated adapter rejected the firmware download" "$FAKEBT_LOG"; }
        grep -q "hci0: Bluetooth 5.2 adapter 0e8d:0608 up, address F0:0D:AE:F0:12:01, made by MediaTek" "$LOG" || { fail "Bluetooth adapter not up" "$FAKEBT_LOG"; }
        grep -q "11:22:33:44:55:66  classic .* gamepad .*\"Wireless Gamepad\"" "$LOG" || { fail "classic Bluetooth gamepad not found by the scan"; }
        grep -q "C0:FF:EE:00:12:34  LE random .* gamepad .*\"BLE Pad\"" "$LOG" || { fail "LE gamepad not found by the scan"; }
        grep -q "\"Wireless Gamepad\" paired and connected (4 axes, hat, 12 buttons)" "$LOG" || { fail "Bluetooth gamepad did not pair" "$FAKEBT_LOG"; }
        grep -q "buttons: 1 3 10, hat right, axes: X +127 Y +0 Z +0 Rz -127" "$LOG" || { fail "gamepad input not decoded" "$FAKEBT_LOG"; }
        grep -q "reconnect OK" "$FAKEBT_LOG" || { fail "gamepad could not reconnect with the stored key" "$FAKEBT_LOG"; }
        grep -q "buttons: 2 12, hat down, axes: X -127 Y +0 Z +0 Rz +0" "$LOG" || { fail "gamepad input after the reconnect not decoded" "$FAKEBT_LOG"; }
        grep -q "\"BT Headset\" paired and connected (headset with microphone, hands-free)" "$LOG" || { fail "Bluetooth headset did not pair" "$FAKEBT_LOG"; }
        [ "$(grep -c '"BT Headset": [0-9.]* s, [0-9]* samples at 8 kHz, peak 4[0-9]%, RMS 3[0-9]%, pitch about 4[34][0-9] Hz' "$LOG")" -ge 2 ] \
            || { fail "the headset microphone's 440 Hz tone was not recorded before and after its reconnect" "$FAKEBT_LOG"; }
        grep -q "headset reconnect OK" "$FAKEBT_LOG" || { fail "headset could not reconnect to the audio gateway" "$FAKEBT_LOG"; }
        grep -q "read /system/session.cfg" "$LOG" || { fail "aerosmss did not read its config from disk"; }
        echo "PASS: booted, mounted the NVMe and SATA disks and a USB stick, set up the USB keyboard and mouse behind a hub, brought up igb and e1000e cards, got an address over DHCP and pinged the gateway, loaded MediaTek Bluetooth firmware, found the gamepads in a scan, paired the classic gamepad, read its input and saw it reconnect, paired a headset and recorded its microphone before and after it reconnected, the security self-test passed, aerosmss read its config, IPC round trips completed"; exit 0
    fi
    sleep 1
done
fail "no prompt within ${TIMEOUT}s (test stage $STAGE)" "$LOG" "$FAKEBT_LOG"
