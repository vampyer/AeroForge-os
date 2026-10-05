#!/usr/bin/env bash
# Builds build/usb.img: a 64 MiB MBR disk with one FAT32 partition holding
# tools/usb-files/ and the design doc. QEMU attaches it as a USB stick
# (usb-storage on the xHCI controller), so it tests USB mass storage. Needs
# mkfs.fat (dosfstools), mtools and python3.
# With arguments it builds another stick instead: make-usb-disk.sh <image>
# <volume label> <files directory> (the boot test plugs one in later).
set -euo pipefail
cd "$(dirname "$0")/.."

IMG=${1:-build/usb.img}
LABEL=${2:-USBSTICK}
FILES=${3:-tools/usb-files}
PART=$IMG.part
START=2048
SECTORS=$((64 * 1024 * 2 - START))
mkdir -p build

rm -f "$PART"
truncate -s $((SECTORS * 512)) "$PART"
mkfs.fat -F 32 -s 1 -n "$LABEL" "$PART" >/dev/null 2>&1
mcopy -s -i "$PART" "$FILES"/* ::/
if [ $# -eq 0 ]; then
    # A multi-cluster file bigger than one USB transfer, for checksum tests.
    mcopy -i "$PART" docs/AeroForge-OS-Design.md ::/
fi

truncate -s 0 "$IMG.tmp"
truncate -s 64M "$IMG.tmp"
# One MBR entry: type 0x0C (FAT32 LBA) from LBA $START.
python3 - "$IMG.tmp" "$START" "$SECTORS" <<'PY'
import struct, sys
path, start, count = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
entry = struct.pack("<B3sB3sII", 0x00, b"\xfe\xff\xff", 0x0C, b"\xfe\xff\xff", start, count)
with open(path, "r+b") as f:
    f.seek(0x1BE); f.write(entry)
    f.seek(510); f.write(b"\x55\xaa")
PY
dd if="$PART" of="$IMG.tmp" bs=512 seek="$START" conv=notrunc status=none
rm -f "$PART"
mv "$IMG.tmp" "$IMG"
echo "Built $IMG"
