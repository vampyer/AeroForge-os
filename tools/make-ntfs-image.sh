#!/usr/bin/env bash
# Builds tools/ntfs-test.part.xz, the NTFS volume the boot test reads
# (tools/make-ntfs-disk.sh puts it in an MBR disk image). It is checked in
# because putting folders and fragmented, sparse and compressed files on NTFS
# needs a real NTFS driver: this script mounts the volume with ntfs-3g, so it
# needs root, ntfs-3g and attr (setfattr). Run it again only to change the files.
set -euo pipefail
cd "$(dirname "$0")/.."

PART=$(mktemp)
MNT=$(mktemp -d)
truncate -s 31M "$PART"
mkntfs -F -Q -c 4096 -p 2048 -H 255 -S 63 -L WINDATA "$PART" >/dev/null 2>&1
LOOP=$(losetup -f --show "$PART")
trap 'umount "$MNT" 2>/dev/null || true; losetup -d "$LOOP"; rm -rf "$MNT" "$PART"' EXIT
ntfs-3g "$LOOP" "$MNT"

mkdir -p "$MNT/Program Files/AeroForge" "$MNT/Users/Player/Saved Games" "$MNT/Photos" "$MNT/Packed"
printf 'This drive was formatted NTFS by Windows.\r\n' > "$MNT/README.TXT"
cp docs/AeroForge-OS-Design.md "$MNT/Program Files/AeroForge/manual.md"
printf 'checkpoint=castle-9\nhealth=42\n' > "$MNT/Users/Player/Saved Games/Level 9.sav"
printf 'coffee\n' > "$MNT/Café ☕.txt"
# Enough names that the folder's index spills into a B-tree of index blocks.
for i in $(seq -w 1 300); do printf 'photo %s\n' "$i" > "$MNT/Photos/IMG_0$i.JPG"; done
# Two files grown in turns end up in many pieces (runs) each.
python3 - "$MNT" <<'PY'
import os, sys
mnt = sys.argv[1]
a, b = open(f"{mnt}/fragmented.bin", "wb"), open(f"{mnt}/interleaved.bin", "wb")
for i in range(24):
    for f, tag in ((a, b"A"), (b, b"B")):
        f.write(bytes((i * 37 + j) & 0xFF for j in range(16 * 1024 - 1)) + tag)
        f.flush(); os.fsync(f.fileno())
a.close(); b.close()
with open(f"{mnt}/sparse.bin", "wb") as f:
    f.seek(3 * 1024 * 1024)
    f.write(b"end of sparse file\n")
PY
# Files made in a compressed folder are compressed.
setfattr -h -v 0x00000800 -n system.ntfs_attrib_be "$MNT/Packed"
yes "compress me " | head -c 100000 > "$MNT/Packed/squeezed.txt" || true
sync
umount "$MNT"
xz -9 -T1 -c "$PART" > tools/ntfs-test.part.xz
echo "Built tools/ntfs-test.part.xz"
