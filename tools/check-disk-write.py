#!/usr/bin/env python3
"""Checks that a disk image holds the pattern the shell's 'diskwrite' command
writes (kernel/src/block.rs, test_block), so the boot test can prove the data
reached the disk and not just a cache.

usage: check-disk-write.py <image> <dev> <lba> <blocks>"""
import sys

path, dev, lba, count = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
BS = 512
with open(path, "rb") as f:
    f.seek(lba * BS)
    data = f.read(count * BS)
bad = 0
for i in range(count):
    n = lba + i
    want = bytearray((j * 7 + n) & 0xFF for j in range(BS))
    head = f"AeroForge write test {dev} LBA {n}\n".encode()
    want[: len(head)] = head
    if data[i * BS : (i + 1) * BS] != want:
        bad += 1
if bad:
    print(f"{path}: {bad} of {count} blocks from LBA {lba} do not hold the {dev} test pattern")
    sys.exit(1)
print(f"{path}: {count} blocks from LBA {lba} hold the {dev} test pattern")
