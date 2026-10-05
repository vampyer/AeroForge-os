#!/usr/bin/env python3
"""Checks the files the boot test saved on the test disk images once QEMU has
exited: every FAT32 volume must pass fsck.fat, and the files written by the
'savetest' program and the shell must hold what was written (read back with
mtools, which knows nothing about AeroForge).

usage: check-fat-files.py <build dir>"""
import os
import subprocess
import sys
import tempfile

build = sys.argv[1]
OFFSET = 2048 * 512  # every test disk's FAT32 partition starts at sector 2048
failed = False


def fail(msg):
    global failed
    print("FAIL: " + msg)
    failed = True


def save(seed, n):
    """The bytes userland/src/bin/savetest.rs writes for save `seed`."""
    return bytes(((i * 31) + i // 251 + seed * 7) & 0xFF for i in range(n))


def mtype(img, path):
    r = subprocess.run(["mtype", "-i", f"{img}@@{OFFSET}", "::" + path], capture_output=True)
    return r.stdout if r.returncode == 0 else None


def names(img, path):
    r = subprocess.run(["mdir", "-b", "-i", f"{img}@@{OFFSET}", "::" + path], capture_output=True, text=True)
    return sorted(os.path.basename(l.rstrip("/")) for l in r.stdout.splitlines() if l.strip())


for img in ("disk.img", "sata.img", "usb.img"):
    path = os.path.join(build, img)
    with open(path, "rb") as f, tempfile.NamedTemporaryFile(suffix=".img") as part:
        f.seek(OFFSET)
        part.write(f.read())
        part.flush()
        r = subprocess.run(["fsck.fat", "-n", part.name], capture_output=True, text=True)
        if r.returncode != 0:
            fail(f"{img}: fsck.fat found problems:\n{r.stdout}{r.stderr}")
        else:
            print(f"{img}: fsck.fat clean ({r.stdout.strip().splitlines()[-1].split(': ', 1)[-1]})")

disk = os.path.join(build, "disk.img")
saves = names(disk, "/saves")
want = ["Slot 1 - Forest Temple.sav", "Slot 2 - Forest Temple.sav"]
if saves != want:
    fail(f"disk.img: /saves holds {saves}, not {want}")
if mtype(disk, "/saves/Slot 1 - Forest Temple.sav") != save(99, 1500):
    fail("disk.img: slot 1 does not hold the overwritten save")
if mtype(disk, "/saves/Slot 2 - Forest Temple.sav") != save(2, 1500):
    fail("disk.img: slot 2 does not hold its save")

sata = os.path.join(build, "sata.img")
if mtype(sata, "/docs/Hello World.txt") != b"Saved from AeroForge on SATA\n":
    fail("sata.img: /docs/Hello World.txt is missing or wrong")

usb = os.path.join(build, "usb.img")
if mtype(usb, "/usbnote.txt") != b"Saved from AeroForge on USB\n":
    fail("usb.img: /usbnote.txt is missing or wrong")
if mtype(usb, "/USB.TXT") is not None:
    fail("usb.img: the deleted USB.TXT is still there")

if failed:
    sys.exit(1)
print("files saved by AeroForge read back correctly with mtools")
