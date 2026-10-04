#!/usr/bin/env bash
# Builds build/sata.img: a 32 MiB MBR disk with one FAT32 partition holding
# tools/sata-files/ and the design doc. QEMU attaches it to the q35 AHCI controller, so it tests
# the SATA driver and MBR parsing (the NVMe disk covers GPT). Needs
# mkfs.fat (dosfstools), mtools and python3.
set -euo pipefail
cd "$(dirname "$0")/.."

IMG=build/sata.img
PART=build/sata-part.img
START=2048
SECTORS=$((32 * 1024 * 2 - START))
mkdir -p build

rm -f "$PART"
truncate -s $((SECTORS * 512)) "$PART"
mkfs.fat -F 32 -s 1 -n SATADISK "$PART" >/dev/null 2>&1
mcopy -s -i "$PART" tools/sata-files/* ::/
# A multi-cluster file bigger than one AHCI transfer, for checksum tests.
mcopy -i "$PART" docs/AeroForge-OS-Design.md ::/

truncate -s 0 "$IMG.tmp"
truncate -s 32M "$IMG.tmp"
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
