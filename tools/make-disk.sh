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
dd if="$PART" of="$IMG.tmp" bs=512 seek=2048 conv=notrunc status=none
rm -f "$PART"
mv "$IMG.tmp" "$IMG"
echo "Built $IMG"
