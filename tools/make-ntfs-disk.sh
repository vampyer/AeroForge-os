#!/usr/bin/env bash
# Builds build/ntfs.img: a 32 MiB MBR disk whose one partition (type 0x07)
# is the NTFS volume in tools/ntfs-test.part.xz (see tools/make-ntfs-image.sh).
# QEMU attaches it as a second SATA drive, like a Windows data drive.
set -euo pipefail
cd "$(dirname "$0")/.."

IMG=build/ntfs.img
START=2048
mkdir -p build
truncate -s 0 "$IMG.tmp"
truncate -s 32M "$IMG.tmp"
python3 - "$IMG.tmp" "$START" <<'PY'
import lzma, struct, sys
path, start = sys.argv[1], int(sys.argv[2])
part = lzma.open("tools/ntfs-test.part.xz").read()
entry = struct.pack("<B3sB3sII", 0x00, b"\xfe\xff\xff", 0x07, b"\xfe\xff\xff", start, len(part) // 512)
with open(path, "r+b") as f:
    f.seek(0x1BE); f.write(entry)
    f.seek(510); f.write(b"\x55\xaa")
    f.seek(start * 512); f.write(part)
PY
mv "$IMG.tmp" "$IMG"
echo "Built $IMG"
