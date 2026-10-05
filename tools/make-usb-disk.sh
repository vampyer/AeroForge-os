#!/usr/bin/env bash
# Builds build/usb.img: a 64 MiB MBR disk with one FAT32 partition holding
# tools/usb-files/ and the design doc. QEMU attaches it as a USB stick
# (usb-storage on the xHCI controller), so it tests USB mass storage. Needs
# mkfs.fat (dosfstools), mtools and python3.
# With arguments it builds another stick instead: make-usb-disk.sh <image>
# <volume label> <files directory> [fat32|exfat] (the boot test plugs one in
# later). exFAT needs mkfs.exfat (exfatprogs); files go on with tools/exfat.py.
set -euo pipefail
cd "$(dirname "$0")/.."

IMG=${1:-build/usb.img}
LABEL=${2:-USBSTICK}
FILES=${3:-tools/usb-files}
FS=${4:-fat32}
PART=$IMG.part
START=2048
SECTORS=$((64 * 1024 * 2 - START))
mkdir -p build

rm -f "$PART"
truncate -s $((SECTORS * 512)) "$PART"
if [ "$FS" = exfat ]; then
    # 512-byte clusters, so files and folders span many of them.
    # The design doc goes on too: a file of many clusters to read back.
    mkfs.exfat -c 512 -L "$LABEL" "$PART" >/dev/null
    rm -rf "$IMG.files" && cp -r "$FILES" "$IMG.files" && cp docs/AeroForge-OS-Design.md "$IMG.files/"
    python3 tools/exfat.py add "$PART" 0 "$IMG.files"
    rm -rf "$IMG.files"
    TYPE=0x07
else
    mkfs.fat -F 32 -s 1 -n "$LABEL" "$PART" >/dev/null 2>&1
    mcopy -s -i "$PART" "$FILES"/* ::/
    TYPE=0x0C
fi
if [ $# -eq 0 ]; then
    # A multi-cluster file bigger than one USB transfer, for checksum tests.
    mcopy -i "$PART" docs/AeroForge-OS-Design.md ::/
fi

truncate -s 0 "$IMG.tmp"
truncate -s 64M "$IMG.tmp"
# One MBR entry: type 0x0C (FAT32 LBA) or 0x07 (exFAT) from LBA $START.
python3 - "$IMG.tmp" "$START" "$SECTORS" "$TYPE" <<'PY'
import struct, sys
path, start, count, kind = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4], 16)
entry = struct.pack("<B3sB3sII", 0x00, b"\xfe\xff\xff", kind, b"\xfe\xff\xff", start, count)
with open(path, "r+b") as f:
    f.seek(0x1BE); f.write(entry)
    f.seek(510); f.write(b"\x55\xaa")
PY
dd if="$PART" of="$IMG.tmp" bs=512 seek="$START" conv=notrunc status=none
rm -f "$PART"
mv "$IMG.tmp" "$IMG"
echo "Built $IMG"
