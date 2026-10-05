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
        echo "FAIL: kernel panic"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
    fi
    if [ "$(grep -c "done, exiting" "$LOG" 2>/dev/null)" -ge 3 ] && grep -q "rotest (pid" "$LOG" && grep -q "nxtest (pid" "$LOG" \
        && grep -q "ping 10.0.2.2: \|WARN.*\(eth\|DHCP\|Ethernet\)" "$LOG" \
        && { [ $STAGE = 6 ] && [ "$(grep -c 'pitch about\|mic: ' "$LOG")" -ge 2 ] || grep -q "WARN.*hci0\|bt pair:\|mic: " "$LOG"; }; then
        sleep 1
        sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | sed -n '/AeroForge OS/,$p'
        grep -q "SMP: 4 of 4" "$LOG" || { echo "FAIL: not all CPUs came online"; exit 1; }
        grep -q "\[aerosmss\] all services up" "$LOG" || { echo "FAIL: aerosmss did not finish startup"; exit 1; }
        grep -q "mounted at /" "$LOG" || { echo "FAIL: NVMe FAT32 volume not mounted"; exit 1; }
        grep -q "mounted at /sata0p1" "$LOG" || { echo "FAIL: SATA FAT32 volume not mounted"; exit 1; }
        grep -q "mounted at /usb0p1" "$LOG" || { echo "FAIL: USB stick FAT32 volume not mounted"; exit 1; }
        grep -q "hub [0-9]* port 1, .*HID boot keyboard" "$LOG" || { echo "FAIL: USB keyboard behind the hub not set up"; exit 1; }
        grep -q "hub [0-9]* port 2, .*HID boot mouse" "$LOG" || { echo "FAIL: USB mouse behind the hub not set up"; exit 1; }
        grep -q "Security audit passed" "$LOG" || { echo "FAIL: kernel security audit did not pass"; exit 1; }
        grep -q "SMEP on, SMAP on" "$LOG" || { echo "FAIL: SMEP/SMAP not enabled"; exit 1; }
        grep -q "attacks blocked, system call checks OK" "$LOG" || { echo "FAIL: sectest: a bad pointer got through a system call"; exit 1; }
        grep -q "nxtest (pid [0-9]*) killed: .*execute in a no-execute page" "$LOG" || { echo "FAIL: code on the stack was not stopped by NX"; exit 1; }
        grep -q "rotest (pid [0-9]*) killed: .*write to a read-only page" "$LOG" || { echo "FAIL: write to code was not stopped"; exit 1; }
        grep -q "eth0: Intel 8086:10c9 (igb/igc driver) .*link up" "$LOG" || { echo "FAIL: Intel igb card not set up"; exit 1; }
        grep -q "eth1: Intel 8086:10d3 (e1000 driver) .*link up" "$LOG" || { echo "FAIL: Intel e1000e card not set up"; exit 1; }
        grep -q "DHCP: eth0 got 10.0.2.15/24, gateway 10.0.2.2" "$LOG" || { echo "FAIL: no DHCP lease"; exit 1; }
        grep -q "ping 10.0.2.2: reply from 10.0.2.2" "$LOG" || { echo "FAIL: no ping reply from the gateway"; exit 1; }
        grep -q "hci0: MediaTek MT7961 firmware .* loaded" "$LOG" || { echo "FAIL: MediaTek Bluetooth firmware not loaded"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "firmware OK" "$FAKEBT_LOG" && ! grep -q "FAIL" "$FAKEBT_LOG" || { echo "FAIL: simulated adapter rejected the firmware download"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "hci0: Bluetooth 5.2 adapter 0e8d:0608 up, address F0:0D:AE:F0:12:01, made by MediaTek" "$LOG" || { echo "FAIL: Bluetooth adapter not up"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "11:22:33:44:55:66  classic .* gamepad .*\"Wireless Gamepad\"" "$LOG" || { echo "FAIL: classic Bluetooth gamepad not found by the scan"; exit 1; }
        grep -q "C0:FF:EE:00:12:34  LE random .* gamepad .*\"BLE Pad\"" "$LOG" || { echo "FAIL: LE gamepad not found by the scan"; exit 1; }
        grep -q "\"Wireless Gamepad\" paired and connected (4 axes, hat, 12 buttons)" "$LOG" || { echo "FAIL: Bluetooth gamepad did not pair"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "buttons: 1 3 10, hat right, axes: X +127 Y +0 Z +0 Rz -127" "$LOG" || { echo "FAIL: gamepad input not decoded"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "reconnect OK" "$FAKEBT_LOG" || { echo "FAIL: gamepad could not reconnect with the stored key"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "buttons: 2 12, hat down, axes: X -127 Y +0 Z +0 Rz +0" "$LOG" || { echo "FAIL: gamepad input after the reconnect not decoded"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "\"BT Headset\" paired and connected (headset with microphone, hands-free)" "$LOG" || { echo "FAIL: Bluetooth headset did not pair"; cat "$FAKEBT_LOG"; exit 1; }
        [ "$(grep -c '"BT Headset": [0-9.]* s, [0-9]* samples at 8 kHz, peak 4[0-9]%, RMS 3[0-9]%, pitch about 4[34][0-9] Hz' "$LOG")" -ge 2 ] \
            || { echo "FAIL: the headset microphone's 440 Hz tone was not recorded before and after its reconnect"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "headset reconnect OK" "$FAKEBT_LOG" || { echo "FAIL: headset could not reconnect to the audio gateway"; cat "$FAKEBT_LOG"; exit 1; }
        grep -q "read /system/session.cfg" "$LOG" || { echo "FAIL: aerosmss did not read its config from disk"; exit 1; }
        echo "PASS: booted, mounted the NVMe and SATA disks and a USB stick, set up the USB keyboard and mouse behind a hub, brought up igb and e1000e cards, got an address over DHCP and pinged the gateway, loaded MediaTek Bluetooth firmware, found the gamepads in a scan, paired the classic gamepad, read its input and saw it reconnect, paired a headset and recorded its microphone before and after it reconnected, the security self-test passed, aerosmss read its config, IPC round trips completed"; exit 0
    fi
    sleep 1
done
echo "FAIL: no prompt within ${TIMEOUT}s"; sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG"; exit 1
