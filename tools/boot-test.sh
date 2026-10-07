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
# and 'padtest' must read them again; a second USB stick, formatted exFAT,
# plugged into the hub must be mounted and readable, take saved files (enough
# that a folder grows), an overwrite, a delete and a new folder, and be
# unmounted when it is pulled out (fsck.exfat checks it afterwards);
# last, 'diskwrite' must write, flush and read back a test pattern on the
# NVMe, SATA and USB disks, and the disk images must hold it after QEMU exits;
# then the 'savetest' program saves, overwrites and deletes files through the
# file system calls and the shell saves and deletes files on the SATA and USB
# disks; after QEMU exits every volume must pass fsck.fat and mtools must read
# the saved files back. Last, an NTFS drive (a second SATA disk, from
# tools/ntfs-test.part.xz) must mount read-only and its folders, a
# 300-entry folder, fragmented and sparse files read back exactly, while
# compressed files and writes are refused. Last of all, five copies of
# 'fputest' must keep their x87, SSE, AVX and MXCSR registers while they are
# switched against each other, and compute a known floating point result.
# Then 'threadtest' runs four threads under one futex lock, uses the heap,
# waits for a child program's exit code and exits with three threads still
# running, which 'ps' must no longer show. 'balancetest' runs six busy
# threads on the four CPUs: CPUs that run out of work must take waiting
# threads from busier ones, and 'sched' must count the moves. 'priotest'
# times a high-priority thread against sixteen busy normal ones: it must run
# ahead of them. 'timertest' checks that 2 ms sleeps take about 2 ms (not a
# 10 ms tick), that 144 Hz frame pacing holds and that futex timeouts work.
# 'nettest' uses UDP and TCP sockets against tools/echo-server.py on this
# host (10.0.2.2 to the guest): a UDP echo, a receive timeout, 64 KiB echoed
# over TCP with the end of data, and a refused connection; 'ifconfig' must
# then show its sockets closed and gone. It also reads the network info,
# looks names up through the test DNS server in echo-server.py, and runs a
# TCP server that a client on this host reaches through QEMU's port
# forwarding (host 5580 to guest 7070). 'waittest' waits on several handles
# at once: events set by another thread, a port, a UDP socket that the
# echo server answers, and a child process that it kills. 'drawtest' takes
# the whole screen (the firmware framebuffer here; tools/display-test.sh
# covers virtio-gpu): screenshots through QEMU's monitor must show its
# picture, then the console again once it lets go.
# 'spreadtest' leaves its busy threads at one, three, one and three per CPU:
# periodic balancing must even them out although no CPU goes idle.
# 'wakeups' counts timer interrupts per CPU over a second: cpu0, which only
# runs the (sleeping) shell, must be tickless, with no more than 5, and
# all CPUs together must take fewer than 100 (one ticking CPU's worth): the
# USB, network, mixer and Bluetooth threads must not wake every tick.
# Last, 'irq'
# must show that the
# USB, NVMe and SATA (AHCI) controllers and the igb network card have been
# raising interrupts.
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
./tools/make-usb-disk.sh build/usb2.img HOTSTICK tools/usb2-files exfat >/dev/null 2>&1
./tools/make-ntfs-disk.sh >/dev/null
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
: >build/echo-server.log
# Appending: the host client below writes to the same log.
python3 tools/echo-server.py >>build/echo-server.log 2>&1 &
ECHO_PID=$!
for _ in $(seq 50); do [ -S build/fakebt.sock ] && [ -S build/xpad.sock ] && [ -S build/hidpad.sock ] && break; sleep 0.1; done
cp "$OVMF_VARS" build/test-vars.fd
rm -f "$LOG" build/qemu-monitor.sock build/sound.wav build/gop-draw.ppm build/gop-console.ppm build/gop-desktop.ppm build/gop-snap.ppm build/gop-start.ppm

qemu-system-x86_64 -M q35 -cpu max -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/test-vars.fd \
    -drive file=build/disk.img,if=none,id=nvm,format=raw -device nvme,serial=AERO0001,drive=nvm \
    -drive file=build/sata.img,if=none,id=sata,format=raw -device ide-hd,drive=sata,bus=ide.1,serial=AEROSATA1 \
    -drive file=build/ntfs.img,if=none,id=ntfs,format=raw -device ide-hd,drive=ntfs,bus=ide.3,serial=AERONTFS \
    -device qemu-xhci,id=xhci -device usb-hub,bus=xhci.0,port=1 \
    -device usb-kbd,bus=xhci.0,port=1.1 -device usb-mouse,bus=xhci.0,port=1.2 \
    -drive file=build/usb.img,if=none,id=stick,format=raw -device usb-storage,bus=xhci.0,port=2,drive=stick,serial=AEROUSB1 \
    -chardev socket,id=fakebt,path=build/fakebt.sock -device usb-redir,chardev=fakebt,bus=xhci.0,port=3 \
    -chardev socket,id=xpad,path=build/xpad.sock -device usb-redir,chardev=xpad,bus=xhci.0,port=4 \
    -chardev socket,id=hidpad,path=build/hidpad.sock -device usb-redir,chardev=hidpad,bus=xhci.0,port=1.3 \
    -nic user,model=igb,hostfwd=tcp:127.0.0.1:5580-:7070 -nic user,model=e1000e \
    -device ich9-intel-hda,id=hda -device hda-output,bus=hda.0,audiodev=snd -audiodev wav,id=snd,path=build/sound.wav \
    -cdrom build/aeroforge.iso -serial file:"$LOG" -display none \
    -monitor unix:build/qemu-monitor.sock,server,nowait &
QEMU_PID=$!
trap 'kill $QEMU_PID $FAKEBT_PID $XPAD_PID $HIDPAD_PID $ECHO_PID ${CLIENT_PID:-} 2>/dev/null || true' EXIT

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
# Shell commands for the exFAT stick, each with the output that ends it
# (the success line, or the error line, which starts with the path).
EXFAT_CMDS=('wc /usb1p1/AeroForge-OS-Design.md')
EXFAT_WAIT=('fd53da30847dce45  /usb1p1/AeroForge-OS-Design.md\|  /usb1p1/AeroForge-OS-Design.md: ')
for n in 4 5 6 7 8; do
    EXFAT_CMDS+=("write \"/usb1p1/Games/Saves/Level $n checkpoint.sav\" checkpoint=lava-$n")
    EXFAT_WAIT+=("bytes to /usb1p1/Games/Saves/Level $n checkpoint.sav\|  /usb1p1/Games/Saves/Level $n checkpoint.sav: ")
done
EXFAT_CMDS+=('write /usb1p1/AeroForge-OS-Design.md replaced by AeroForge' 'rm /usb1p1/hot.txt' 'mkdir /usb1p1/Music' 'ls /usb1p1/games/saves')
EXFAT_WAIT+=('bytes to /usb1p1/AeroForge-OS-Design.md' 'deleted /usb1p1/hot.txt\|  /usb1p1/hot.txt: ' 'created directory /usb1p1/Music\|  /usb1p1/Music: ' '  Level 8 checkpoint.sav\|  /usb1p1/games/saves: ')
EXFAT_STEP=0
# The same for the NTFS drive (checksums are the files' FNV-1a, as made by tools/make-ntfs-image.sh).
NTFS_CMDS=('ls /sata1p1' 'ls /sata1p1/Photos' 'cat /sata1p1/users/player/saved games/level 9.sav'
    'wc /sata1p1/Program Files/AeroForge/manual.md' 'wc /sata1p1/fragmented.bin' 'wc /sata1p1/interleaved.bin'
    'wc /sata1p1/sparse.bin' 'cat /sata1p1/Packed/squeezed.txt' 'write /sata1p1/new.txt hello')
NTFS_WAIT=('  Café ☕.txt\|  /sata1p1: ' '  IMG_0300.JPG\|  /sata1p1/Photos: ' 'checkpoint=castle-9\|  /sata1p1/users/player/saved games/level 9.sav: '
    '  /sata1p1/Program Files/AeroForge/manual.md' '  /sata1p1/fragmented.bin' '  /sata1p1/interleaved.bin'
    '  /sata1p1/sparse.bin' '  /sata1p1/Packed/squeezed.txt: ' '  /sata1p1/new.txt: ')
NTFS_STEP=0
monitor() { python3 tools/qemu-monitor.py build/qemu-monitor.sock "$@" >/dev/null; }
fast_monitor() { python3 tools/qemu-monitor.py --gap 0.08 build/qemu-monitor.sock "$@" >/dev/null; }
# The mouse is relative: push the pointer into the top-left corner, then
# move it right and down to $1,$2.
point_at() {
    local x=$1 y=$2 moves=()
    for _ in $(seq 1 30); do moves+=("mouse_move -100 -100"); done
    while [ "$x" -gt 0 ]; do d=$((x > 100 ? 100 : x)); moves+=("mouse_move $d 0"); x=$((x - d)); done
    while [ "$y" -gt 0 ]; do d=$((y > 100 ? 100 : y)); moves+=("mouse_move 0 $d"); y=$((y - d)); done
    fast_monitor "${moves[@]}"; sleep 0.5
}
double_click() { fast_monitor "mouse_button 1" "mouse_button 0" "mouse_button 1" "mouse_button 0"; }
# Where the desktop's Computer window shows the entry named $2 of folder $1,
# from its "[desktop] Computer: <folder> = a | b; first row at x,y, rows h apart" line.
row_of() {
    python3 - "$LOG" "$1" "$2" <<'PY'
import re, sys
log = open(sys.argv[1], 'rb').read().decode(errors='replace')
m = re.search(r'\[desktop\] Computer: ' + re.escape(sys.argv[2]) + r' = (.*); first row at (\d+),(\d+), rows (\d+) apart', log)
names = [n.lower() for n in m.group(1).split(' | ')]
i = names.index(sys.argv[3].lower())
print(m.group(2), int(m.group(3)) + i * int(m.group(4)))
PY
}
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
        # ...read and written (it is exFAT)...
        sleep 1; type_keys "${EXFAT_CMDS[0]}"$'\n'; STAGE=exfat
    elif [ $STAGE = exfat ] && grep -q "${EXFAT_WAIT[$EXFAT_STEP]}" "$LOG"; then
        EXFAT_STEP=$((EXFAT_STEP + 1))
        if [ $EXFAT_STEP -lt ${#EXFAT_CMDS[@]} ]; then
            sleep 1; type_keys "${EXFAT_CMDS[$EXFAT_STEP]}"$'\n'
        else
            # ...and pulled out again.
            sleep 1; monitor "device_del stick2"; STAGE=18
        fi
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
    elif [ $STAGE = 28 ] && grep -q "deleted /usb0p1/usb.txt\|  /usb0p1/usb.txt: " "$LOG"; then
        # Reading the NTFS drive.
        sleep 1; type_keys "${NTFS_CMDS[0]}"$'\n'; STAGE=ntfs
    elif [ $STAGE = ntfs ] && grep -q "${NTFS_WAIT[$NTFS_STEP]}" "$LOG"; then
        NTFS_STEP=$((NTFS_STEP + 1))
        if [ $NTFS_STEP -lt ${#NTFS_CMDS[@]} ]; then
            sleep 1; type_keys "${NTFS_CMDS[$NTFS_STEP]}"$'\n'
        else
            # Floating point and vector registers, five programs at once.
            sleep 1; type_keys $'run fputest\n'; STAGE=fpu
        fi
    elif [ $STAGE = fpu ] && grep -q "\[fputest\] .*\(: OK\|FAILED\)" "$LOG"; then
        # Threads, the heap, futexes, waiting for a child, and exit with threads left.
        sleep 1; type_keys $'run threadtest\n'; STAGE=threads
    elif [ $STAGE = threads ] && grep -q "\[threadtest\] \(exiting with\|FAILED\)" "$LOG"; then
        sleep 2; type_keys $'ps\n'; STAGE=threadps
    elif [ $STAGE = threadps ] && { sed -n '/\[threadtest\] exiting with/,$p' "$LOG" | grep -q "process(es)" || grep -q "\[threadtest\] FAILED" "$LOG"; }; then
        # Load balancing: idle CPUs take waiting threads from busy ones.
        sleep 1; type_keys $'run balancetest\n'; STAGE=balance
    elif [ $STAGE = balance ] && grep -q "\[balancetest\] .*\(: OK\|FAILED\)" "$LOG"; then
        sleep 1; type_keys $'sched\n'; STAGE=sched
    elif [ $STAGE = sched ] && grep -q "thread migrations between CPUs" "$LOG"; then
        # Priorities: a high-priority thread runs ahead of busy normal ones.
        sleep 1; type_keys $'run priotest\n'; STAGE=prio
    elif [ $STAGE = prio ] && grep -q "\[priotest\] .*\(: OK\|FAILED\)" "$LOG"; then
        # Precise timers: microsecond sleeps, frame pacing, futex timeouts.
        sleep 1; type_keys $'run timertest\n'; STAGE=timer
    elif [ $STAGE = timer ] && grep -q "\[timertest\] .*\(: OK\|FAILED\)" "$LOG"; then
        # Balancing between busy CPUs.
        sleep 1; type_keys $'run nettest\n'; STAGE=net
    elif [ $STAGE = net ] && [ -z "${CLIENT_PID:-}" ] && grep -q "\[nettest\] listening on port" "$LOG"; then
        # The other direction: a client on this host connects to nettest's server.
        python3 tools/echo-server.py client 5580 >>build/echo-server.log 2>&1 &
        CLIENT_PID=$!
    elif [ $STAGE = net ] && grep -q "\[nettest\] \(network info, DNS lookups and a TCP server: OK\|FAILED\)" "$LOG"; then
        # Its sockets must be closed and removed once it has exited.
        sleep 3; type_keys $'ifconfig\n'; STAGE=netsockets
    elif [ $STAGE = netsockets ] && grep -q "program socket(s) open" "$LOG"; then
        # One wait on events, a port, a socket and a child process.
        sleep 1; type_keys $'run waittest\n'; STAGE=wait
    elif [ $STAGE = wait ] && grep -q "\[waittest\] .*\(: OK\|FAILED\)" "$LOG"; then
        # A program takes the whole screen; screenshots show it, then the console.
        sleep 1; type_keys $'run drawtest\n'; STAGE=draw
    elif [ $STAGE = draw ] && grep -q "\[drawtest\] \(holding the screen\|FAILED\)" "$LOG"; then
        sleep 1; monitor "screendump build/gop-draw.ppm"; STAGE=drawn
    elif [ $STAGE = drawn ] && grep -q "\[drawtest\] .*\(: OK\|FAILED\)" "$LOG"; then
        sleep 1; monitor "screendump build/gop-console.ppm"
        # The desktop: drag the Notes window with the mouse, type into it,
        # take a screenshot, and leave with Esc.
        sleep 1; type_keys $'run desktop\n'; STAGE=desk
    elif [ $STAGE = desk ] && grep -q "\[desktop\] \(up at\|could not\)" "$LOG"; then
        sleep 1
        # Pointer to the top-left corner, then onto Notes' title bar.
        for i in $(seq 1 30); do monitor "mouse_move -100 -100"; done
        XY=$(grep -ao "Notes title bar at [0-9]*,[0-9]*" "$LOG" | head -1 | grep -o "[0-9]*,[0-9]*")
        X=${XY%,*}; Y=${XY#*,}
        while [ "${X:-0}" -gt 0 ]; do d=$((X > 100 ? 100 : X)); monitor "mouse_move $d 0"; X=$((X - d)); done
        while [ "${Y:-0}" -gt 0 ]; do d=$((Y > 100 ? 100 : Y)); monitor "mouse_move 0 $d"; Y=$((Y - d)); done
        sleep 0.5; monitor "mouse_button 1"; sleep 0.5
        monitor "mouse_move 100 50"; sleep 0.3; monitor "mouse_move 100 50"; sleep 0.5
        monitor "mouse_button 0"; sleep 0.5
        # Then open Computer from its desktop icon, go into /docs and open
        # the welcome text file in Notes.
        XY=$(grep -ao "Computer icon at [0-9]*,[0-9]*" "$LOG" | head -1 | grep -o "[0-9]*,[0-9]*")
        point_at "${XY%,*}" "${XY#*,}"; double_click; STAGE=deskcomp
    elif [ $STAGE = deskcomp ] && grep -q "\[desktop\] Computer: / = " "$LOG"; then
        # Make Computer 100 narrower and 80 taller by its bottom-right corner.
        read -r CX CY CW CH <<< "$(grep -ao "opened Computer at [0-9]*,[0-9]* ([0-9]*x[0-9]*)" "$LOG" | head -1 | tr -c '0-9\n' ' ')"
        sleep 0.5; point_at $((CX + CW - 3)) $((CY + CH - 3))
        monitor "mouse_button 1"; monitor "mouse_move -50 40"; monitor "mouse_move -50 40"; monitor "mouse_button 0"
        STAGE=deskresize
    elif [ $STAGE = deskresize ] && grep -q "\[desktop\] resized Computer to" "$LOG"; then
        sleep 0.5; point_at $(row_of / docs); double_click; STAGE=deskdocs
    elif [ $STAGE = deskdocs ] && grep -qi "\[desktop\] Computer: /docs = " "$LOG"; then
        DOCS=$(grep -aio "\[desktop\] Computer: /docs = " "$LOG" | head -1 | sed 's/.*Computer: //; s/ = //')
        sleep 0.5; point_at $(row_of "$DOCS" "Welcome to AeroForge.txt"); double_click; STAGE=deskfile
    elif [ $STAGE = deskfile ] && grep -qi "\[desktop\] \(opened /docs/Welcome to AeroForge.txt in Notes\|cannot\)" "$LOG"; then
        sleep 0.5; type_keys 'hello aero'
        sleep 1; monitor "screendump build/gop-desktop.ppm"
        # Snap Welcome to the left half by dragging its title bar to the edge.
        point_at 166 126; monitor "mouse_button 1"; monitor "mouse_move -100 0"; monitor "mouse_move -100 0"
        monitor "mouse_button 0"; STAGE=desksnap
    elif [ $STAGE = desksnap ] && grep -q "\[desktop\] \(snapped\|moved\) Welcome" "$LOG"; then
        sleep 1; monitor "screendump build/gop-snap.ppm"
        # Drag it away again: it gets its old size back.
        point_at 100 12; monitor "mouse_button 1"; monitor "mouse_move 100 100"; monitor "mouse_move 100 100"
        monitor "mouse_button 0"; STAGE=deskunsnap
    elif [ $STAGE = deskunsnap ] && grep -q "\[desktop\] moved Welcome" "$LOG"; then
        # Open the Start menu for a screenshot of it and the Start button.
        sleep 0.5; point_at 27 780; monitor "mouse_button 1" "mouse_button 0"; point_at 640 300
        sleep 1; monitor "screendump build/gop-start.ppm"
        monitor "sendkey esc"; STAGE=deskdone
    elif [ $STAGE = deskdone ] && grep -q "\[desktop\] closed, screen given back" "$LOG"; then
        sleep 1; type_keys $'run spreadtest\n'; STAGE=spread
    elif [ $STAGE = spread ] && grep -q "\[spreadtest\] .*\(: OK\|FAILED\)" "$LOG"; then
        # Tickless idle: timer interrupts per CPU over one second.
        sleep 1; type_keys $'wakeups\n'; STAGE=wakeups
    elif [ $STAGE = wakeups ] && grep -q "timer interrupts in 1 s on" "$LOG"; then
        # Device interrupts: the USB controller's MSI-X count, after all that input.
        sleep 1; type_keys $'irq\n'; STAGE=irq
    elif [ $STAGE = irq ] && grep -q "interrupts  xhci0\|no device interrupts" "$LOG"; then
        STAGE=done
    fi
    if grep -q "PANIC" "$LOG" 2>/dev/null; then
        fail "kernel panic" "$LOG"
    fi
    if [ "$(grep -c "done, exiting" "$LOG" 2>/dev/null)" -ge 3 ] && grep -q "rotest (pid" "$LOG" && grep -q "nxtest (pid" "$LOG" \
        && grep -q "ping 10.0.2.2: \|WARN.*\(eth\|DHCP\|Ethernet\)" "$LOG" \
        && { [ $STAGE = done ] || grep -q "WARN.*hci0\|bt pair:\|mic: " "$LOG"; }; then
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
        grep -q "Fast system calls: syscall/sysret entry open to ring 3" "$LOG" || { fail "syscall/sysret not set up"; }
        grep -q "Floating point for programs: x87, SSE, AVX (XSAVE" "$LOG" || { fail "floating point (XSAVE, AVX) not set up for programs"; }
        grep -q "\[fputest\] 5 programs kept their x87, SSE, AVX and MXCSR state across 30 rounds of switching; sum of square roots of 1..100000 = 21082008.973918: OK" "$LOG" \
            || { fail "programs lost floating point or vector registers, or computed a wrong result"; }
        grep -q "\[threadtest\] 4 threads on [0-9]* CPUs added 80000 under one lock, 1000 futex hand-offs, heap and 8 MiB buffer OK, child nxtest exited with -1014: OK" "$LOG" \
            || { fail "threads, the heap, futexes or waiting for a child program went wrong"; }
        sed -n '/\[threadtest\] exiting with 3 threads still running/,/process(es)/p' "$LOG" | grep -q "process(es)" \
            && ! sed -n '/\[threadtest\] exiting with 3 threads still running/,/process(es)/p' "$LOG" | grep -q "  threadtest" \
            || { fail "threadtest's leftover threads were still there after it exited"; }
        grep -q "\[balancetest\] 6 threads on [0-9]* CPUs, [1-9][0-9]* moved to an idle CPU, results agree, [0-9]* ms: OK" "$LOG" \
            || { fail "idle CPUs did not take waiting threads from busy ones, or a moved thread computed a wrong result"; }
        grep -q "^  [1-9][0-9]* thread migrations between CPUs" "$LOG" || { fail "the scheduler counted no thread migrations"; }
        grep -q "\[priotest\] high priority ran ahead of 16 busy threads, bad requests refused: OK" "$LOG" \
            || { fail "a high-priority thread did not run ahead of busy normal ones, or bad priority requests were accepted"; }
        grep -q "\[timertest\] sleeps precise, 144 Hz pacing kept, futex timeouts work: OK" "$LOG" \
            || { fail "sleeps were not precise to well under a tick, frame pacing slipped, or futex timeouts failed"; }
        grep -q "\[nettest\] UDP echo, receive timeout, 64 KiB TCP echo and a refused connection: OK" "$LOG" \
            || { fail "UDP or TCP sockets did not work against the host's echo server" build/echo-server.log; }
        grep -q "\[nettest\] network info, DNS lookups and a TCP server: OK" "$LOG" \
            || { fail "network info, DNS lookups or the TCP server did not work" build/echo-server.log; }
        grep -q "host client: got .*: OK" build/echo-server.log || { fail "the host's client did not get nettest's server's answer" build/echo-server.log; }
        grep -q "^  0 program socket(s) open" "$LOG" || { fail "nettest's sockets were not closed after it exited"; }
        grep -q "\[waittest\] events, ports, sockets and child processes in one wait: OK" "$LOG" \
            || { fail "waiting on several handles at once failed (events, a port, a socket or a child process)"; }
        grep -q "Display: console drawn off-screen and presented to the firmware framebuffer" "$LOG" || { fail "the console was not moved off-screen"; }
        grep -q "\[drawtest\] full-screen frame, partial update, bad frame refused, screen given back: OK" "$LOG" \
            || { fail "a program could not take the screen, draw on it and give it back"; }
        python3 tools/check-screen.py drawtest build/gop-draw.ppm || { fail "drawtest's picture was not on the screen"; }
        python3 tools/check-screen.py console build/gop-console.ppm || { fail "the console did not come back after drawtest"; }
        grep -q "\[desktop\] moved Notes to 466,414" "$LOG" || { fail "dragging the Notes window with the mouse did not move it to 466,414"; }
        grep -q "\[desktop\] opened Computer at " "$LOG" || { fail "double-clicking the Computer icon did not open the Computer window"; }
        grep -q "\[desktop\] Computer: / = .*docs | games | .*README.TXT" "$LOG" \
            || { fail "the Computer window did not list the disk's root folder (folders first)"; }
        grep -q "\[desktop\] snapped Welcome to 0,0 640x760" "$LOG" || { fail "dragging Welcome against the left edge did not snap it to the left half"; }
        grep -q "\[desktop\] moved Welcome to [0-9]*,[0-9]* (400x250)" "$LOG" || { fail "dragging the snapped Welcome window away did not give back its size"; }
        python3 tools/check-screen.py start build/gop-start.ppm || { fail "the Start button or the Start menu is wrong in the screenshot"; }
        python3 tools/check-screen.py snap build/gop-snap.ppm || { fail "the snapped window is not on the left half of the screenshot"; }
        grep -q "\[desktop\] resized Computer to 420x440" "$LOG" || { fail "dragging the Computer window's corner did not resize it to 420x440"; }
        grep -qi "\[desktop\] Computer: /docs = AeroForge-OS-Design.md | Welcome to AeroForge.txt;" "$LOG" \
            || { fail "double-clicking docs in the Computer window did not list /docs"; }
        grep -qi "\[desktop\] opened /docs/Welcome to AeroForge.txt in Notes (167 bytes)" "$LOG" \
            || { fail "double-clicking a text file in the Computer window did not open it in Notes"; }
        grep -q "\[desktop\] notes: Welcome to AeroForge OS\. / .*hello aero" "$LOG" \
            || { fail "the opened file plus keys typed on the desktop were not in the Notes window"; }
        CXY=$(grep -ao "opened Computer at [0-9]*,[0-9]*" "$LOG" | head -1 | grep -o "[0-9]*,[0-9]*")
        python3 tools/check-screen.py desktop build/gop-desktop.ppm 466 414 "${CXY%,*}" "${CXY#*,}" \
            || { fail "the desktop screenshot is wrong"; }
        grep -q "\[spreadtest\] busy CPUs evened out their threads: OK" "$LOG" \
            || { fail "busy CPUs with uneven numbers of threads did not even out"; }
        grep -q "^  cpu0: [0-5] timer interrupts" "$LOG" \
            || { fail "an idle CPU kept taking timer ticks (tickless idle not working)"; }
        grep -qE "^  [0-9]{1,2} timer interrupts in 1 s on" "$LOG" \
            || { fail "the CPUs took 100 or more timer interrupts in an idle second: a kernel thread is still waking every tick"; }
        grep -q "xhci0: MSI-X interrupts on, polling at once when events arrive" "$LOG" || { fail "USB controller interrupts (MSI-X) not set up"; }
        grep -q "vector 0x[0-9a-f]*  MSI-X -> cpu[0-9]* *[1-9][0-9]* interrupts  xhci0" "$LOG" || { fail "the USB controller raised no interrupts"; }
        grep -q "nvme0: MSI-X completion interrupts on" "$LOG" || { fail "NVMe completion interrupts (MSI-X) not set up"; }
        grep -q "vector 0x[0-9a-f]*  MSI-X -> cpu0 *[1-9][0-9]* interrupts  nvme0" "$LOG" || { fail "the NVMe controller raised no interrupts"; }
        grep -q "ahci0: MSI completion interrupts on" "$LOG" || { fail "SATA (AHCI) completion interrupts (MSI) not set up"; }
        grep -q "vector 0x[0-9a-f]*  MSI   -> cpu0 *[1-9][0-9]* interrupts  ahci0" "$LOG" || { fail "the SATA (AHCI) controller raised no interrupts"; }
        grep -q "eth0: MSI-X interrupts on, receiving at once when frames arrive" "$LOG" || { fail "igb network card interrupts (MSI-X) not set up"; }
        grep -q "vector 0x[0-9a-f]*  MSI-X -> cpu[0-9]* *[1-9][0-9]* interrupts  eth0" "$LOG" || { fail "the igb network card raised no interrupts"; }
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
        grep -q "exFAT volume \"HOTSTICK\" on usb1p1 mounted at /usb1p1" "$LOG" || { fail "the exFAT USB stick plugged in later was not mounted"; }
        grep -q "48976 bytes, 561 lines, fnv1a fd53da30847dce45  /usb1p1/AeroForge-OS-Design.md" "$LOG" || { fail "a many-cluster file on the exFAT stick did not read back right"; }
        for n in 4 5 6 7 8; do
            grep -q "wrote 18 bytes to /usb1p1/Games/Saves/Level $n checkpoint.sav" "$LOG" || { fail "saving level $n on the exFAT stick failed"; }
        done
        grep -q "wrote 22 bytes to /usb1p1/AeroForge-OS-Design.md" "$LOG" || { fail "overwriting a file on the exFAT stick failed"; }
        grep -q "deleted /usb1p1/hot.txt" "$LOG" || { fail "deleting a file on the exFAT stick failed"; }
        grep -q "created directory /usb1p1/Music" "$LOG" || { fail "creating a folder on the exFAT stick failed"; }
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
        grep -q "NTFS volume \"WINDATA\" on sata1p1 mounted at /sata1p1 (read-only)" "$LOG" || { fail "the NTFS drive was not mounted"; }
        grep -q "  Café ☕.txt" "$LOG" && ! grep -q "  \$MFT" "$LOG" || { fail "the NTFS root folder was not listed right"; }
        [ "$(grep -c "  IMG_0[0-9]*.JPG" "$LOG")" = 300 ] || { fail "the 300-photo NTFS folder (an index B-tree) was not listed in full"; }
        grep -q "48976 bytes, 561 lines, fnv1a fd53da30847dce45  /sata1p1/Program Files/AeroForge/manual.md" "$LOG" || { fail "a file in an NTFS sub-folder did not read back right"; }
        grep -q "393216 bytes, 1536 lines, fnv1a 22f6a3b59ab0158d  /sata1p1/fragmented.bin" "$LOG" \
            && grep -q "393216 bytes, 1536 lines, fnv1a db413e2ace59158d  /sata1p1/interleaved.bin" "$LOG" || { fail "fragmented NTFS files did not read back right"; }
        grep -q "3145747 bytes, 1 lines, fnv1a 25326fd0f3313539  /sata1p1/sparse.bin" "$LOG" || { fail "a sparse NTFS file did not read back right"; }
        grep -q "/sata1p1/Packed/squeezed.txt: compressed NTFS files are not supported yet" "$LOG" || { fail "a compressed NTFS file was not refused"; }
        grep -q "/sata1p1/new.txt: NTFS volumes are read-only for now" "$LOG" || { fail "a write to the NTFS drive was not refused"; }
        grep -q "read /system/session.cfg" "$LOG" || { fail "aerosmss did not read its config from disk"; }
        echo "PASS: booted, mounted the NVMe and SATA disks and a USB stick, set up the USB keyboard and mouse behind a hub, brought up igb and e1000e cards, got an address over DHCP and pinged the gateway, loaded MediaTek Bluetooth firmware, found the gamepads in a scan, paired the classic gamepad, read its input and saw it reconnect, paired a headset and recorded its microphone before and after it reconnected, played a 440 Hz tone on the HD Audio card and a user program's melody through the audio system calls, paired an LE gamepad, read its input over GATT and saw it reconnect, read an Xbox style and a HID USB gamepad, and a user program read all four gamepads through the gamepad system call, before and after the USB pads were unplugged and plugged back in, mounted, read, wrote and unmounted an exFAT USB stick plugged in while running, wrote to the NVMe, SATA and USB disks and found the data in their images, saved, overwrote and deleted files on all three FAT32 volumes (fsck.fat clean, read back with mtools), read folders and fragmented and sparse files on an NTFS drive, five programs kept their x87, SSE and AVX registers while switched against each other, a program's threads shared a lock and a heap and were all ended when it exited, a program drew on the whole screen and gave it back to the console, a desktop program's window was dragged with the mouse and typed into, and its Computer window, opened by double-clicking its icon, was resized by its corner, a window snapped to half the screen and back, listed folders and opened a text file in Notes, idle CPUs took waiting threads from busy ones, a high-priority thread ran ahead of busy ones, sleeps and futex timeouts were precise to well under a tick, busy CPUs evened out their threads, an idle CPU went tickless, the USB, NVMe and SATA controllers and the igb card raised interrupts, the security self-test passed, aerosmss read its config, IPC round trips completed"; exit 0
    fi
    sleep 1
done
fail "no prompt within ${TIMEOUT}s (test stage $STAGE)" "$LOG" "$FAKEBT_LOG"
