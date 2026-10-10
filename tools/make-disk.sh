#!/usr/bin/env bash
# Builds build/disk.img: a 64 MiB GPT disk with one FAT32 partition holding
# tools/disk-files/ (plus the design doc, as a multi-cluster test file).
# QEMU attaches it as an NVMe drive. Needs sgdisk (gdisk), mkfs.fat
# (dosfstools) and mtools.
set -euo pipefail
cd "$(dirname "$0")/.."

IMG=build/disk.img
PART=build/disk-part.img
mkdir -p build

truncate -s 64M "$IMG.tmp"
sgdisk --clear --new=1:2048:0 --typecode=1:0700 --change-name=1:"AeroForge data" "$IMG.tmp" >/dev/null
# Partition 1 runs from sector 2048 to the last usable sector.
LAST=$(sgdisk -i 1 "$IMG.tmp" | awk '/Last sector/ {print $3}')
SECTORS=$((LAST - 2048 + 1))

rm -f "$PART"
truncate -s $((SECTORS * 512)) "$PART"
mkfs.fat -F 32 -s 1 -n AEROFORGE "$PART" >/dev/null
mcopy -s -i "$PART" tools/disk-files/* ::/
mcopy -i "$PART" docs/AeroForge-OS-Design.md ::/docs/
# A file deleted for good (as another system would), for the file
# manager's "Show deleted files" to bring back: three clusters of text.
for i in $(seq 1 40); do echo "Line $i of a list that was deleted and should come back."; done > build/undelete-me.txt
mcopy -i "$PART" build/undelete-me.txt "::/games/Old shopping list.txt"
LAST=$(mshowfat -i "$PART" "::/games/Old shopping list.txt" | grep -o '[0-9]*>' | tr -d '>')
mdel -i "$PART" "::/games/Old shopping list.txt"
# Other systems allocate onwards from where they last did, so the next
# writes do not land on it straight away: point FSInfo's next free past it.
python3 - "$PART" "$LAST" <<'PY'
import struct, sys
with open(sys.argv[1], 'r+b') as f:
    bs = f.read(512)
    at = struct.unpack_from('<H', bs, 48)[0] * struct.unpack_from('<H', bs, 11)[0]
    f.seek(at + 492)
    f.write(struct.pack('<I', int(sys.argv[2]) + 1))
PY
dd if="$PART" of="$IMG.tmp" bs=512 seek=2048 conv=notrunc status=none
rm -f "$PART"
mv "$IMG.tmp" "$IMG"
echo "Built $IMG"
