#!/usr/bin/env bash
# Display test on QEMU's virtio-gpu with no firmware framebuffer
# (-vga none): the bootloader gives AeroForge no screen, so the console
# appears only if the C++ virtio-gpu driver works. The console must be on
# screen with the boot history replayed; 'drawtest' then takes the whole
# screen, and screenshots taken through QEMU's monitor must show its
# picture and then the console again. The main boot test runs drawtest on
# the firmware framebuffer path.
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}
TIMEOUT=${TIMEOUT:-120}
LOG=build/display-test.log
MON=build/display-monitor.sock

fail() {
    echo "FAIL: $1"
    [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error::display test: $1"
    sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | tail -40
    exit 1
}

make iso >/dev/null
cp "$OVMF_VARS" build/display-vars.fd
rm -f "$LOG" "$MON" build/virtio-boot.ppm build/virtio-draw.ppm build/virtio-console.ppm

qemu-system-x86_64 -M q35 -cpu max -m 512M -smp 2 -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file=build/display-vars.fd \
    -cdrom build/aeroforge.iso -vga none -device virtio-gpu-pci \
    -serial file:"$LOG" -display none -monitor unix:"$MON",server,nowait &
QEMU_PID=$!
trap 'kill $QEMU_PID 2>/dev/null || true' EXIT

monitor() { python3 tools/qemu-monitor.py "$MON" "$@" >/dev/null; }
STAGE=boot
for _ in $(seq "$TIMEOUT"); do
    sleep 1
    grep -q "PANIC" "$LOG" 2>/dev/null && fail "kernel panic"
    if [ $STAGE = boot ] && grep -q "\[client [0-9]*\] done, exiting" "$LOG" 2>/dev/null; then
        sleep 2
        monitor "screendump build/virtio-boot.ppm"
        python3 tools/qemu-type.py "$MON" $'display\n'
        sleep 1
        python3 tools/qemu-type.py "$MON" $'run drawtest\n'
        STAGE=draw
    elif [ $STAGE = draw ] && grep -q "\[drawtest\] holding the screen\|\[drawtest\] FAILED" "$LOG"; then
        grep -q "\[drawtest\] FAILED" "$LOG" && fail "drawtest failed"
        sleep 1
        monitor "screendump build/virtio-draw.ppm"
        STAGE=release
    elif [ $STAGE = release ] && grep -q "\[drawtest\] .*: OK\|\[drawtest\] FAILED" "$LOG"; then
        sleep 1
        monitor "screendump build/virtio-console.ppm"
        STAGE=done
        break
    fi
done
[ $STAGE = done ] || fail "stopped at stage $STAGE within ${TIMEOUT}s"
sed 's/\x1b\[[0-9;=]*[a-zA-Z]//g' "$LOG" | grep -a "virtio-gpu\|drawtest\|through the\|whole screen\|showing the console"
grep -q "C++ virtio-gpu driver attached through DHI v[0-9]*: console on screen at [0-9]*x[0-9]*" "$LOG" || fail "the virtio-gpu driver did not start"
grep -q "  [0-9]*x[0-9]* through the virtio-gpu" "$LOG" || fail "'display' did not report the virtio-gpu"
grep -q "\[drawtest\] full-screen frame, partial update, bad frame refused, screen given back: OK" "$LOG" || fail "drawtest failed"
python3 tools/check-screen.py console build/virtio-boot.ppm || fail "the console was not on screen after boot"
python3 tools/check-screen.py drawtest build/virtio-draw.ppm || fail "drawtest's picture was not on screen"
python3 tools/check-screen.py console build/virtio-console.ppm || fail "the console did not come back after drawtest"
echo "PASS: with no firmware framebuffer, the C++ virtio-gpu driver put the console on screen, a program drew the whole screen and a partial update, and the console came back"
