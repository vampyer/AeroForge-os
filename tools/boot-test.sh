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
# own; and 'sound test' must play a 440 Hz tone on the emulated HD Audio card,
# measured in the WAV file QEMU records, and the 'melody' program must play its
# four notes through the audio system calls; last, 'bt pair' and 'gamepad' for
# the simulated LE gamepad: it must pair (SMP), be read over HID over GATT,
# report its input, reconnect on its own with the stored long-term key and
# report again. Two simulated USB gamepads (tools/fakepad), an Xbox 360 style
# one and a HID one behind the hub, must show up in 'gamepad' with their
# input decoded, and the Xbox style one must get its player 1 LED command;
# then the 'padtest' program must read all four gamepads through the
# gamepad system call, in the Xbox layout; last, both USB pads unplug
# themselves and plug back in (hot-plug on a root port and behind the hub),
# and 'padtest' must read them again; a second USB stick plugged into the
# hub must be mounted and readable, and unmounted when it is pulled out;
# last, 'diskwrite' must write, flush and read back a test pattern on the
# NVMe, SATA and USB disks, and the disk images must hold it after QEMU exits;
# then the 'savetest' program saves, overwrites and deletes files through the
# file system calls and the shell saves and deletes files on the SATA and USB
# disks; after QEMU exits every volume must pass fsck.fat and mtools must read
# the saved files back.
# Intended for CI (design doc, Phase 0).
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}
TIMEOUT=${TIMEOUT:-180}
LOG=build/boot-test.log

make iso >/dev/null
./tools/make-disk.sh >/dev/null
./tools/make-sata-disk.sh >/dev/null 2>&1
./tools/make-usb-disk.sh >/dev/null 2>&1
./tools/make-usb-disk.sh build/usb2.img HOTSTICK tools/usb2-files >/dev/null 2>&1
cc -O2 -Wall -o build/fakebt tools/fakebt/fakebt.c -lusbredirparser -lm
FAKEBT_LOG=build/fakebt.log
build/fakebt build/fakebt.sock build/firmware/mediatek/BT_RAM_CODE_MT7961_1_2_hdr.bin 2>"$FAKEBT_LOG" &
FAKEBT_PID=$!
cc -O2 -Wall -o build/fakepad tools/fakepad/fakepad.c -lusbredirparser
FAKEPAD_LOG=build/fakepad.log
build/fakepad build/xpad.sock --xinput 2>"$FAKEPAD_LOG" &
XPAD_PID=$!
build/fakepad build/hidpad.sock --hid 2>>"$FAKEPAD_LOG" &
HIDPAD_PID=$!
for _ in $(seq 50); do [ -S build/fakebt.sock ] && [ -S build/xpad.sock ] && [ -S build/hidpad.sock ] && break; sleep 0.1; done
cp "$OVMF_VARS" build/test-vars.fd
rm -f "$LOG" build/qemu-monitor.sock build/sound.wav

qemu-system-x86_64 -M q35 -cpu max -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/test-vars.fd \
    -drive file=build/disk.img,if=none,id=nvm,format=raw -device nvme,serial=AERO0001,drive=nvm \
    -drive file=build/sata.img,if=none,id=sata,format=raw -device ide-hd,drive=sata,bus=ide.1,serial=AEROSATA1 \
    -device qemu-xhci,id=xhci -device usb-hub,bus=xhci.0,port=1 \
    -device usb-kbd,bus=xhci.0,port=1.1 -device usb-mouse,bus=xhci.0,port=1.2 \
    -drive file=build/usb.img,if=none,id=stick,format=raw -device usb-storage,bus=xhci.0,port=2,drive=stick,serial=AEROUSB1 \
    -chardev socket,id=fakebt,path=build/fakebt.sock -device usb-redir,chardev=fakebt,bus=xhci.0,port=3 \
    -chardev socket,id=xpad,path=build/xpad.sock -device usb-redir,chardev=xpad,bus=xhci.0,port=4 \
    -chardev socket,id=hidpad,path=build/hidpad.sock -device usb-redir,chardev=hidpad,bus=xhci.0,port=1.3 \
    -nic user,model=igb -nic user,model=e1000e \
    -device ich9-intel-hda,id=hda -device hda-output,bus=hda.0,audiodev=snd -audiodev wav,id=snd,path=build/sound.wav \
    -cdrom build/aeroforge.iso -serial file:"$LOG" -display none \
    -monitor unix:build/qemu-monitor.sock,server,nowait &
QEMU_PID=$!
trap 'kill $QEMU_PID $FAKEBT_PID $XPAD_PID $HIDPAD_PID 2>/dev/null || true' EXIT

# In GitHub Actions a failure also becomes an annotation, readable without the log.
fail() {
    echo "FAIL: $1"
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error::$1 | $(sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | grep -a 'bt pair\|paired\|mic\|pitch\|headset\|gamepad\|WARN' | tail -8 | tr -d '\r' | tr '\n' ';') | fakebt: $(grep -a 'FAIL\|voice\|reconnect\|LE\|SMP\|GATT' "$FAKEBT_LOG" | tail -6 | tr -d '\r' | tr '\n' ';')"
    fi
    shift
    for f in "$@"; do cat "$f"; done
    exit 1
}

type_keys() { python3 tools/qemu-type.py build/qemu-monitor.sock "$1"; }
monitor() { python3 tools/qemu-monitor.py build/qemu-monitor.sock "$@" >/dev/null; }
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
    elif [ $STAGE = 6 ] && [ "$(grep -c 'pitch about\|mic: ' "$LOG")" -ge 2 ]; then
        sleep 1; type_keys $'sound test 440 1\n'; STAGE=7
    elif [ $STAGE = 7 ] && grep -q "  played \|sound: " "$LOG"; then
        sleep 1; type_keys $'run melody\n'; STAGE=8
    elif [ $STAGE = 8 ] && grep -q "\[melody\] \(played\|no sound\)" "$LOG"; then
        sleep 1; type_keys $'bt pair C0:FF:EE:00:12:34\n'; STAGE=9
    elif [ $STAGE = 9 ] && grep -q "\"BLE Pad\" paired and connected\|bt pair: " "$LOG"; then
        sleep 1; type_keys $'gamepad\n'; STAGE=10
    elif [ $STAGE = 10 ] && [ "$(grep -c 'gamepad C0:FF:EE:00:12:34 .* connected (' "$LOG")" -ge 2 ]; then
        sleep 1; type_keys $'gamepad\n'; STAGE=11
    elif [ $STAGE = 11 ] && [ "$(grep -c '"BLE Pad" on hci0' "$LOG")" -ge 2 ]; then
        sleep 1; type_keys $'run padtest\n'; STAGE=12
    elif [ $STAGE = 12 ] && grep -q "\[padtest\] [0-9]* gamepad(s)" "$LOG"; then
        # Hot-plug: the simulated pads unplug themselves (one on a root port, one behind the hub)...
        kill -USR1 $XPAD_PID $HIDPAD_PID; STAGE=13
    elif [ $STAGE = 13 ] && grep -q 'gamepad "Controller" unplugged' "$LOG" && grep -q 'gamepad "Generic USB Joystick" unplugged' "$LOG"; then
        # ...and plug themselves in again.
        sleep 1; kill -USR1 $XPAD_PID $HIDPAD_PID; STAGE=14
    elif [ $STAGE = 14 ] && grep -q 'gamepad "Controller" plugged in on' "$LOG" && grep -q 'gamepad "Generic USB Joystick" plugged in on' "$LOG"; then
        sleep 3; type_keys $'run padtest\n'; STAGE=15
    elif [ $STAGE = 15 ] && [ "$(grep -c "\[padtest\] [0-9]* gamepad(s)" "$LOG")" -ge 2 ]; then
        # A second USB stick is plugged into the hub while the system runs...
        monitor "drive_add 0 if=none,id=stick2,file=build/usb2.img,format=raw" \
            "device_add usb-storage,bus=xhci.0,port=1.4,drive=stick2,id=stick2,serial=AEROUSB2"; STAGE=16
    elif [ $STAGE = 16 ] && grep -q "mounted at /usb1p1" "$LOG"; then
        sleep 1; type_keys $'cat /usb1p1/hot.txt\n'; STAGE=17
    elif [ $STAGE = 17 ] && grep -q "This stick was plugged in while AeroForge was running" "$LOG"; then
        # ...read, and pulled out again.
        sleep 1; monitor "device_del stick2"; STAGE=18
    elif [ $STAGE = 18 ] && grep -q "USB disk usb1 unplugged" "$LOG"; then
        sleep 1; type_keys $'ls /usb1p1\n'; STAGE=19
    elif [ $STAGE = 19 ] && grep -q "  /usb1p1: " "$LOG"; then
        # Writes: 80 blocks spans several transfers on every driver.
        sleep 1; type_keys $'diskwrite nvme0 1000 80\n'; STAGE=20
    elif [ $STAGE = 20 ] && grep -q "to nvme0 at LBA 1000\|diskwrite: " "$LOG"; then
        sleep 1; type_keys $'diskwrite sata0 1000 80\n'; STAGE=21
    elif [ $STAGE = 21 ] && grep -q "to sata0 at LBA 1000\|diskwrite: " "$LOG"; then
        sleep 1; type_keys $'diskwrite usb0 1000 80\n'; STAGE=22
    elif [ $STAGE = 22 ] && grep -q "to usb0 at LBA 1000\|diskwrite: " "$LOG"; then
        sleep 1; type_keys $'diskwrite nvme0p1 1000 80\n'; STAGE=23
    elif [ $STAGE = 23 ] && grep -q "diskwrite: give a whole disk" "$LOG"; then
        # Saving files: a program, then the shell on the SATA and USB disks.
        sleep 1; type_keys $'run savetest\n'; STAGE=24
    elif [ $STAGE = 24 ] && grep -q "\[savetest\] " "$LOG"; then
        sleep 1; type_keys $'mkdir /sata0p1/docs\n'; STAGE=25
    elif [ $STAGE = 25 ] && grep -q "created directory /sata0p1/docs\|  /sata0p1/docs: " "$LOG"; then
        sleep 1; type_keys $'write "/sata0p1/docs/Hello World.txt" Saved from AeroForge on SATA\n'; STAGE=26
    elif [ $STAGE = 26 ] && grep -q "bytes to /sata0p1/docs/Hello World.txt\|  /sata0p1/docs/Hello World.txt: " "$LOG"; then
        sleep 1; type_keys $'write /usb0p1/usbnote.txt Saved from AeroForge on USB\n'; STAGE=27
    elif [ $STAGE = 27 ] && grep -q "bytes to /usb0p1/usbnote.txt\|  /usb0p1/usbnote.txt: " "$LOG"; then
        sleep 1; type_keys $'rm /usb0p1/usb.txt\n'; STAGE=28
    fi
    if grep -q "PANIC" "$LOG" 2>/dev/null; then
        fail "kernel panic" "$LOG"
    fi
    if [ "$(grep -c "done, exiting" "$LOG" 2>/dev/null)" -ge 3 ] && grep -q "rotest (pid" "$LOG" && grep -q "nxtest (pid" "$LOG" \
        && grep -q "ping 10.0.2.2: \|WARN.*\(eth\|DHCP\|Ethernet\)" "$LOG" \
        && { [ $STAGE = 28 ] && grep -q "deleted /usb0p1/usb.txt\|  /usb0p1/usb.txt: " "$LOG" || grep -q "WARN.*hci0\|bt pair:\|mic: " "$LOG"; }; then
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
        grep -q "snd0: Intel 8086:293e .*codecs: QEMU .*outputs: Line out" "$LOG" || { fail "HD Audio controller or its output not found"; }
        grep -q "  played 48000 frames on snd0 Line out" "$LOG" || { fail "sound test did not play"; }
        TONE=$(python3 tools/wav-tone.py build/sound.wav)
        echo "sound card output:"; echo "$TONE"
        echo "$TONE" | grep -qE "^tone 1\.0[0-9] s, 4[34][0-9] Hz, peak (7[5-9]|8[0-4])[0-9]{2}$" || { fail "the 440 Hz test tone was not heard on the sound card ($TONE)"; }
        grep -q "\[melody\] played 4 notes" "$LOG" || { fail "the melody program could not play"; }
        [ "$(echo "$TONE" | grep -cE "^tone 0\.(29|30|31) s, (52[0-9]|66[0-9]|78[0-9]|10[45][0-9]) Hz")" = 4 ] \
            || { fail "the melody program's four notes were not heard on the sound card ($TONE)"; }
        grep -q "\"BLE Pad\" paired and connected (4 axes, hat, 12 buttons)" "$LOG" || { fail "LE gamepad did not pair" "$FAKEBT_LOG"; }
        grep -q "buttons: 4 5, hat left, axes: X +0 Y -127 Z +0 Rz +0" "$LOG" || { fail "LE gamepad input not decoded" "$FAKEBT_LOG"; }
        grep -q "LE reconnect OK" "$FAKEBT_LOG" || { fail "LE gamepad could not reconnect with the stored key" "$FAKEBT_LOG"; }
        grep -q "buttons: 6 11, hat up-right, axes: X +127 Y +0 Z -127 Rz +0" "$LOG" || { fail "LE gamepad input after the reconnect not decoded" "$FAKEBT_LOG"; }
        # QEMU's xHCI port=4 is root port 8 (its four USB 3 ports come first).
        grep -q "Xbox 360 style gamepad \"Controller\" on USB .* port 8 (6 axes, hat, 11 buttons)" "$LOG" || { fail "Xbox style USB gamepad not set up" "$FAKEPAD_LOG"; }
        grep -q "gamepad \"Generic USB Joystick\" on USB .* hub [0-9]* port 3 (4 axes, hat, 12 buttons)" "$LOG" || { fail "HID USB gamepad behind the hub not set up" "$FAKEPAD_LOG"; }
        grep -q "buttons: 1 4 5 8, hat down-right, axes: X +127 Y -127 Z +127 Rx -127 Ry +0 Rz -127" "$LOG" || { fail "Xbox style USB gamepad input not decoded" "$FAKEPAD_LOG"; }
        grep -q "buttons: 2 10, hat left, axes: X -127 Y +0 Z +127 Rz +0" "$LOG" || { fail "HID USB gamepad input not decoded" "$FAKEPAD_LOG"; }
        grep -q "LED: player 1" "$FAKEPAD_LOG" && ! grep -q "FAIL" "$FAKEPAD_LOG" || { fail "USB gamepad set-up went wrong" "$FAKEPAD_LOG"; }
        grep -q "\[padtest\] .*\"Controller\" (USB, Xbox layout): A Y LB Start, d-pad down-right, left stick +32766 +32766, right stick -32766 +0, triggers 255 0" "$LOG" \
            || { fail "the gamepad system call did not give the Xbox style pad in the Xbox layout"; }
        grep -q "\[padtest\] .*\"Generic USB Joystick\" (USB, Xbox layout guessed): B RS, d-pad left, left stick -32766 +0, right stick +32766 +0, triggers 0 0" "$LOG" \
            || { fail "the gamepad system call did not give the HID pad"; }
        grep -q "\[padtest\] 4 gamepad(s)" "$LOG" || { fail "the gamepad system call did not list all four gamepads"; }
        grep -q "xhci: port 8, slot [0-9]*: 045e:028e \"Controller\" unplugged" "$LOG" || { fail "unplugging the Xbox style pad from its root port went unnoticed" "$FAKEPAD_LOG"; }
        grep -q "xhci: hub [0-9]* port 3, slot [0-9]*: 0079:0006 \"Generic USB Joystick\" unplugged" "$LOG" || { fail "unplugging the HID pad from the hub went unnoticed" "$FAKEPAD_LOG"; }
        [ "$(grep -c "\[padtest\] .*\"Controller\" (USB, Xbox layout): A Y LB Start" "$LOG")" -ge 2 ] && [ "$(grep -c "\[padtest\] .*\"Generic USB Joystick\" (USB, Xbox layout guessed): B RS" "$LOG")" -ge 2 ] \
            && [ "$(grep -c "\[padtest\] 4 gamepad(s)" "$LOG")" -ge 2 ] || { fail "the USB pads did not work again after being plugged back in" "$FAKEPAD_LOG"; }
        grep -q "USB disk usb1 plugged in: .*USB \"QEMU QEMU HARDDISK\" serial AEROUSB2" "$LOG" || { fail "the USB stick plugged in later was not registered"; }
        grep -q "FAT32 volume \"HOTSTICK\" on usb1p1 mounted at /usb1p1" "$LOG" || { fail "the USB stick plugged in later was not mounted"; }
        grep -q "USB disk usb1 unplugged, /usb1p1 unmounted" "$LOG" || { fail "the unplugged USB stick was not unmounted"; }
        grep -q "  /usb1p1: " "$LOG" || { fail "the unplugged USB stick's files were still reachable"; }
        for d in nvme0 sata0 usb0; do
            grep -q "wrote 80 blocks to $d at LBA 1000, flushed, read back OK" "$LOG" || { fail "writing to $d failed"; }
        done
        grep -q "diskwrite: give a whole disk, not a partition" "$LOG" || { fail "diskwrite did not refuse to write inside a partition"; }
        # The data must be on the disks themselves once QEMU has gone.
        kill $QEMU_PID 2>/dev/null; wait $QEMU_PID 2>/dev/null || true
        python3 tools/check-disk-write.py build/disk.img nvme0 1000 80 || { fail "the NVMe disk image lacks the written data"; }
        python3 tools/check-disk-write.py build/sata.img sata0 1000 80 || { fail "the SATA disk image lacks the written data"; }
        python3 tools/check-disk-write.py build/usb.img usb0 1000 80 || { fail "the USB disk image lacks the written data"; }
        grep -q "\[savetest\] saved 12 slots, read them back, overwrote slot 1 and deleted slots 3-12: OK" "$LOG" || { fail "the savetest program could not save its files"; }
        grep -q "created directory /sata0p1/docs" "$LOG" || { fail "the shell could not create a directory on the SATA disk"; }
        grep -q "wrote 29 bytes to /sata0p1/docs/Hello World.txt" "$LOG" || { fail "the shell could not save a file on the SATA disk"; }
        grep -q "wrote 28 bytes to /usb0p1/usbnote.txt" "$LOG" || { fail "the shell could not save a file on the USB stick"; }
        grep -q "deleted /usb0p1/usb.txt" "$LOG" || { fail "the shell could not delete a file on the USB stick"; }
        python3 tools/check-fat-files.py build || { fail "the saved files are not right on the disk images"; }
        grep -q "read /system/session.cfg" "$LOG" || { fail "aerosmss did not read its config from disk"; }
        echo "PASS: booted, mounted the NVMe and SATA disks and a USB stick, set up the USB keyboard and mouse behind a hub, brought up igb and e1000e cards, got an address over DHCP and pinged the gateway, loaded MediaTek Bluetooth firmware, found the gamepads in a scan, paired the classic gamepad, read its input and saw it reconnect, paired a headset and recorded its microphone before and after it reconnected, played a 440 Hz tone on the HD Audio card and a user program's melody through the audio system calls, paired an LE gamepad, read its input over GATT and saw it reconnect, read an Xbox style and a HID USB gamepad, and a user program read all four gamepads through the gamepad system call, before and after the USB pads were unplugged and plugged back in, mounted, read and unmounted a USB stick plugged in while running, wrote to the NVMe, SATA and USB disks and found the data in their images, saved, overwrote and deleted files on all three FAT32 volumes (fsck.fat clean, read back with mtools), the security self-test passed, aerosmss read its config, IPC round trips completed"; exit 0
    fi
    sleep 1
done
fail "no prompt within ${TIMEOUT}s (test stage $STAGE)" "$LOG" "$FAKEBT_LOG"
