#!/usr/bin/env bash
# A long "leave it running" test: boots the ISO headless in QEMU and, over and
# over until DURATION seconds (default 3600) have passed, starts the desktop,
# plays a song in Music from the Start menu, works out a sum in Calculator,
# copies a file in Computer with Ctrl+C and Ctrl+V and deletes the copy to the
# Recycle Bin, closes every window, leaves the desktop with Esc and reads 'mem'
# at the console. It fails if the desktop stops answering, a program is
# killed, the kernel panics or warns, or memory in use keeps growing.
# Memory per round goes to build/soak-mem.csv.
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}
DURATION=${DURATION:-3600}
# How much more memory (MiB) the last round may use than the first.
GROWTH=${GROWTH:-32}
LOG=build/soak-test.log
CSV=build/soak-mem.csv

make iso >/dev/null
./tools/make-disk.sh >/dev/null
{ mtype -i build/disk.img@@1M ::/system/session.cfg; echo "console    # soak test: stay at the console"; } > build/session.cfg
mcopy -o -i build/disk.img@@1M build/session.cfg ::/system/session.cfg
cp "$OVMF_VARS" build/soak-vars.fd
rm -f "$LOG" build/soak-monitor.sock

qemu-system-x86_64 -M q35 -cpu max -m 512M -smp 4 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/soak-vars.fd \
    -drive file=build/disk.img,if=none,id=nvm,format=raw -device nvme,serial=AERO0001,drive=nvm \
    -device qemu-xhci,id=xhci -device usb-hub,bus=xhci.0,port=1 \
    -device usb-kbd,bus=xhci.0,port=1.1 -device usb-mouse,bus=xhci.0,port=1.2 \
    -nic user,model=igb \
    -device ich9-intel-hda,id=hda -device hda-output,bus=hda.0,audiodev=snd -audiodev none,id=snd \
    -cdrom build/aeroforge.iso -serial file:"$LOG" -display none \
    -monitor unix:build/soak-monitor.sock,server,nowait &
QEMU_PID=$!
trap 'kill $QEMU_PID 2>/dev/null || true' EXIT
trap 'echo "FAIL: soak-test.sh stopped at line $LINENO (round ${ROUND:-0}, step $STEP)"' ERR

STEP=boot
fail() {
    echo "FAIL: $1 (round ${ROUND:-0}, step $STEP, $SECONDS s in)"
    grep -a '\[desktop\]\|PANIC\|killed\|WARN' "$LOG" | tail -15 | tr -d '\r'
    [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error::$1 (round ${ROUND:-0}, step $STEP)"
    exit 1
}
type_keys() { python3 tools/qemu-type.py build/soak-monitor.sock "$1"; }
monitor() { python3 tools/qemu-monitor.py build/soak-monitor.sock "$@" >/dev/null; }
fast_monitor() { python3 tools/qemu-monitor.py --gap 0.08 build/soak-monitor.sock "$@" >/dev/null; }
point_at() {
    local x=$1 y=$2 moves=()
    for _ in $(seq 1 30); do moves+=("mouse_move -100 -100"); done
    while [ "$x" -gt 0 ]; do d=$((x > 100 ? 100 : x)); moves+=("mouse_move $d 0"); x=$((x - d)); done
    while [ "$y" -gt 0 ]; do d=$((y > 100 ? 100 : y)); moves+=("mouse_move 0 $d"); y=$((y - d)); done
    fast_monitor "${moves[@]}"; sleep 0.5
}
click() { monitor "mouse_button 1" "mouse_button 0"; }
double_click() { fast_monitor "mouse_button 1" "mouse_button 0" "mouse_button 1" "mouse_button 0"; }
count() { local c; c=$(grep -ac "$1" "$LOG" 2>/dev/null || true); echo "${c:-0}"; }
last_xy() { grep -ao "$1 [0-9]*,[0-9]*" "$LOG" | tail -1 | grep -o "[0-9]*,[0-9]*$" | tr ',' ' '; }
# Waits (up to $3 seconds, default 30) for more than $2 lines matching $1.
wait_for() {
    local end=$((SECONDS + ${3:-30}))
    while [ "$(count "$1")" -le "$2" ]; do
        [ $SECONDS -lt $end ] || fail "no answer within ${3:-30} s: waited for \"$1\""
        kill -0 $QEMU_PID 2>/dev/null || fail "QEMU exited"
        sleep 0.2
    done
}
# Where the Computer window shows $2 in folder $1, from its latest listing.
row_of() {
    python3 - "$LOG" "$1" "$2" <<'PY'
import re, sys
log = open(sys.argv[1], 'rb').read().decode(errors='replace')
m = list(re.finditer(r'\[desktop\] Computer: ' + re.escape(sys.argv[2]) + r' = (.*); first row at (\d+),(\d+), rows (\d+) apart', log))[-1]
names = [n.lower() for n in m.group(1).split(' | ')]
i = names.index(sys.argv[3].lower())
print(m.group(2), int(m.group(3)) + i * int(m.group(4)))
PY
}
# Where window $1's close button is, from the latest line about it: "<title>
# window at ...; close at x,y", or "opened <title> at x,y (wxh)" with the
# caption's layout (Close 43 wide, 6 in from the right, 19 high, 1 down).
close_of() {
    python3 - "$LOG" "$1" <<'PY'
import re, sys
log = open(sys.argv[1], 'rb').read().decode(errors='replace')
t = re.escape(sys.argv[2])
best = None
for m in re.finditer(r'\[desktop\] ' + t + r' window at .*?; close at (\d+),(\d+)', log):
    best = (m.start(), int(m.group(1)), int(m.group(2)))
for m in re.finditer(r'\[desktop\] opened ' + t + r' at (\d+),(\d+) \((\d+)x(\d+)\)', log):
    if best is None or m.start() > best[0]:
        x, y, w, h = map(int, m.groups())
        best = (m.start(), x + w - 6 - 43 // 2, y + 1 + 19 // 2)
print(best[1], best[2])
PY
}
# Clicks the close button of window $1 and waits for it to close.
close_window() {
    local n; n=$(count "\[desktop\] closed $1")
    point_at $(close_of "$1"); click
    wait_for "\[desktop\] closed $1" "$n"
}
BAD='AEROKERNEL PANIC\|killed\|WARN'
check_health() {
    local n; n=$(count "$BAD")
    [ "$n" -le "$BAD0" ] || fail "new error in the log: $(grep -a "$BAD" "$LOG" | tail -1 | tr -d '\r')"
}
# Types 'mem' and prints "pages-in-use heap-KiB".
read_mem() {
    local n; n=$(count "  heap      ")
    type_keys $'mem\n'; wait_for "  heap      " "$n" 15
    local pages heap
    pages=$(grep -a "  physical  .* pages in use" "$LOG" | tail -1 | sed 's/.*(\([0-9]*\) pages in use.*/\1/')
    heap=$(grep -a "  heap      " "$LOG" | tail -1 | sed 's/.*heap *\([0-9]*\) KiB.*/\1/')
    echo "$pages $heap"
}

wait_for "aero> " 0 180
sleep 2
BAD0=$(count "$BAD")
# The tests move the pointer by exact amounts: one count, one pixel.
type_keys $'mouse speed 5\n'; sleep 1
echo "round,seconds,pages_in_use,heap_kib,songs,sums,copies" > "$CSV"
read -r P0 H0 <<< "$(read_mem)"
echo "0,$SECONDS,$P0,$H0,0,0,0" >> "$CSV"
echo "booted; $((P0 * 4 / 1024)) MiB in use, heap $H0 KiB"

ROUND=0; SONGS=0; SUMS=0; COPIES=0; SLOWEST=0
while [ $SECONDS -lt "$DURATION" ]; do
    ROUND=$((ROUND + 1)); START=$SECONDS
    STEP=desktop
    n=$(count "\[desktop\] up at"); type_keys $'run desktop\n'; wait_for "\[desktop\] up at" "$n" 60
    sleep 1

    # Music from the Start menu: play a song (a different one each round),
    # then Stop and close.
    STEP=music
    n=$(count "\[desktop\] Start menu open; "); point_at 27 780; click
    wait_for "\[desktop\] Start menu open; .*Music at" "$n"; sleep 0.5
    n=$(count "\[desktop\] Music list: .* | end"); point_at $(last_xy "Start menu open; .*; Music at"); click
    wait_for "\[desktop\] Music list: .* | end" "$n"; sleep 0.5
    ML=$(grep -a "\[desktop\] Music list: .* | end" "$LOG" | tail -1)
    read -r RX RY RH SHOWN <<< "$(echo "$ML" | sed 's/.*song [0-9]* at \([0-9]*\),\([0-9]*\), rows \([0-9]*\) apart, \([0-9]*\) shown.*/\1 \2 \3 \4/')"
    K=$(( (ROUND - 1) % (SHOWN < 10 ? SHOWN : 10) ))
    n=$(count "\[desktop\] Music: .* ($((K + 1)) of 10)")
    point_at "$RX" $((RY + K * RH)); click
    wait_for "\[desktop\] Music: .* ($((K + 1)) of 10)" "$n"
    SONGS=$((SONGS + 1)); sleep 3
    point_at $(last_xy "Music: Previous at .*; Stop at"); click; sleep 0.5
    close_window Music

    # Calculator from its desktop icon: 12+30*2, then "=".
    STEP=calculator
    n=$(count "\[desktop\] Calculator = key at")
    point_at $(last_xy "Calculator icon at"); double_click
    wait_for "\[desktop\] Calculator = key at" "$n"; sleep 0.5
    type_keys '12+30*2'
    n=$(count "\[desktop\] calculator: 84")
    point_at $(last_xy "Calculator = key at"); click
    wait_for "\[desktop\] calculator: 84" "$n"
    SUMS=$((SUMS + 1)); sleep 0.5
    close_window Calculator

    # Computer: into C:, copy README.TXT, paste it, delete the copy.
    STEP=computer
    n=$(count "\[desktop\] Computer: drives = ")
    point_at $(last_xy "Computer icon at"); double_click
    wait_for "\[desktop\] Computer: drives = " "$n"; sleep 0.5
    n=$(count "\[desktop\] Computer: / = ")
    point_at $(last_xy "C: / FAT32 at"); double_click
    wait_for "\[desktop\] Computer: / = " "$n"; sleep 0.5
    point_at $(row_of / README.TXT); click; sleep 0.5
    n=$(count "\[desktop\] pasted /README.TXT as ")
    type_keys $'\x03'; sleep 0.3; type_keys $'\x16'
    wait_for "\[desktop\] pasted /README.TXT as \|paste failed\|Can't" "$n"
    grep -a "\[desktop\] pasted /README.TXT as \|paste failed\|Can't" "$LOG" | tail -1 | grep -q "pasted /README.TXT as " \
        || fail "the copy did not work: $(grep -a "\[desktop\] .*\(paste failed\|Can't\)" "$LOG" | tail -1 | tr -d '\r')"
    COPY=$(grep -a "\[desktop\] pasted /README.TXT as " "$LOG" | tail -1 | sed 's/.*pasted \/README.TXT as \(.*\) ([0-9]* file).*/\1/')
    n=$(count "\[desktop\] moved .* to the Recycle Bin\|Not deleted")
    sleep 0.5; monitor "sendkey delete"; sleep 0.5; type_keys $'\n'
    wait_for "\[desktop\] moved .* to the Recycle Bin\|Not deleted" "$n"
    grep -a "\[desktop\] moved .* to the Recycle Bin\|Not deleted" "$LOG" | tail -1 | grep -qF "moved $COPY to the Recycle Bin" \
        || fail "deleting $COPY did not move it to the Recycle Bin: $(grep -a "\[desktop\] moved\|Not deleted" "$LOG" | tail -1 | tr -d '\r')"
    COPIES=$((COPIES + 1)); sleep 0.5
    close_window Computer

    STEP=leave
    n=$(count "\[desktop\] closed, screen given back")
    monitor "sendkey esc"; wait_for "\[desktop\] closed, screen given back" "$n"
    sleep 1
    STEP=mem
    read -r P H <<< "$(read_mem)"
    check_health
    T=$((SECONDS - START)); [ $T -gt $SLOWEST ] && SLOWEST=$T
    echo "$ROUND,$SECONDS,$P,$H,$SONGS,$SUMS,$COPIES" >> "$CSV"
    echo "round $ROUND done in $T s at $SECONDS s: $((P * 4 / 1024)) MiB in use, heap $H KiB"
done

# Memory: compare the last round with the second (the first round loads the
# desktop's files and fonts into caches once).
read -r P1 H1 <<< "$(sed -n 3p "$CSV" | cut -d, -f3,4 | tr ',' ' ')"
GROWN=$(( (P - P1) * 4 / 1024 ))
echo "memory in use: $((P0 * 4 / 1024)) MiB after boot, $((P1 * 4 / 1024)) MiB after round 1, $((P * 4 / 1024)) MiB after round $ROUND; heap $H0, $H1, $H KiB"
[ "$GROWN" -le "$GROWTH" ] || fail "memory in use grew by $GROWN MiB from round 1 to round $ROUND"
echo "PASS: $ROUND rounds in $SECONDS s (slowest $SLOWEST s): $SONGS songs played, $SUMS sums worked out, $COPIES files copied and deleted, no errors, memory grew $GROWN MiB"
