#!/usr/bin/env bash
# Tests the kernel's NTFS writer on the build machine: tools/ntfs-check
# runs the driver against copies of the test volume (tools/ntfs-test.part.xz),
# then ntfsprogs check what it left: ntfsfix (the MFT and its mirror),
# ntfsresize (every cluster accounted for in the bitmap), ntfsls and ntfscat
# (Linux reads the new folders and files), and ntfs-check itself (every
# folder's index in order and balanced).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --quiet --manifest-path tools/ntfs-check/Cargo.toml
CHECK=tools/ntfs-check/target/release/ntfs-check
IMG=build/ntfs-write-test.img
mkdir -p build
fail() { echo "FAIL: $*"; exit 1; }
fresh() { xz -dc tools/ntfs-test.part.xz > "$IMG"; }
run() { "$CHECK" "$IMG" | tee -a build/ntfs-write-test.log; }
checked() {
    ntfsfix -n "$IMG" >/dev/null || fail "ntfsfix found a problem after $1"
    ntfsresize -i -f "$IMG" > build/ntfsresize.log 2>&1 || { cat build/ntfsresize.log; fail "ntfsresize's check failed after $1"; }
    ! grep -qi "error\|inconsisten\|corrupt" build/ntfsresize.log || { cat build/ntfsresize.log; fail "ntfsresize found a problem after $1"; }
    [ "$(ntfsinfo -m "$IMG" | grep -o "Volume Flags: 0x[0-9a-f]*")" = "Volume Flags: 0x0000" ] || fail "the volume was left dirty after $1"
}
: > build/ntfs-write-test.log

# 1. Save over files, make files and folders: enough in one folder that its
# index moves out of the record and splits, and enough files that the MFT grows.
fresh
{
    echo "w|/Users/Player/Saved Games/Level 9.sav|30|1"
    echo "w|/README.TXT|10000|2"
    echo "w|/new.txt|6|3"
    echo "w|/big.bin|200000|4"
    echo "m|/New Folder"
    for i in $(seq 1 60); do echo "w|/New Folder/file number $i with a long name.txt|$((i * 100))|$i"; done
    for i in $(seq 301 420); do echo "w|/Photos/IMG_0$i.JPG|20|$((i % 250))"; done
    echo "w|/fragmented.bin|1000|5"
    echo "w|/Packed/squeezed.txt|10|1"
    echo "w|/bad:name.txt|1|1"
    echo "c|/big.bin|200000|4"
    echo "c|/New Folder/file number 60 with a long name.txt|6000|60"
    echo "l|/New Folder"
    echo "l|/Photos"
    echo "v|/"
} | run
grep -q "^c /big.bin: holds what was written" build/ntfs-write-test.log || fail "a new file did not read back"
grep -q "^l /New Folder: 60 entries" build/ntfs-write-test.log || fail "a new folder did not list all 60 files"
grep -q "^l /Photos: 420 entries" build/ntfs-write-test.log || fail "the 300-photo folder did not take 120 more"
grep -q "^w /Packed/squeezed.txt: ERROR compressed" build/ntfs-write-test.log || fail "a compressed file was not refused"
grep -q "^w /bad:name.txt: ERROR bad file name" build/ntfs-write-test.log || fail "a name Windows forbids was not refused"
grep -q "^v /: 513 names, all in order" build/ntfs-write-test.log || fail "a folder index is out of order"
! grep -q "ERROR" <(grep -v "squeezed\|bad:name" build/ntfs-write-test.log) || fail "a change failed"
checked "making files and folders"
[ "$(ntfsls -p "/New Folder" "$IMG" | grep -c "^file number")" = 60 ] || fail "Linux does not see the 60 new files"
cmp -s <(ntfscat "$IMG" /big.bin) <(python3 -c "import sys; sys.stdout.buffer.write(bytes((i * 31 + 4) & 255 for i in range(200000)))") \
    || fail "Linux does not read the new file back the same"

# 2. Delete most of it again, in a shuffled order, and save in between.
python3 - <<'PY' | run
import random
random.seed(7)
photos = [f"/Photos/IMG_0{i:03d}.JPG" for i in range(1, 421)]
random.shuffle(photos)
for p in photos[:380]:
    print("d|" + p)
files = [f"/New Folder/file number {i} with a long name.txt" for i in range(1, 61)]
random.shuffle(files)
for p in files:
    print("d|" + p)
print("d|/New Folder")
print("d|/big.bin")
print("w|/new.txt|50000|9")
print("w|/README.TXT|10|3")
print("d|/Users/Player/Saved Games/Level 9.sav")
print("d|/Packed")
for i in range(30):
    print(f"w|/Photos/again {i}.jpg|{i * 3000}|{i}")
print("c|/new.txt|50000|9")
print("l|/Photos")
print("v|/")
PY
grep -q "^d /Packed: ERROR directory not empty" build/ntfs-write-test.log || fail "a folder with files in it was deleted"
grep -q "^l /Photos: 70 entries" build/ntfs-write-test.log || fail "deleting photos left the wrong number"
grep -q "^v /: 100 names, all in order" build/ntfs-write-test.log || fail "a folder index is out of order after deleting"
checked "deleting"

# 3. Rename and move: files, a whole folder, back again, and changing just
# a name's case; refuse a taken name and a folder moved into itself.
python3 - <<'PY' | run
import random
random.seed(11)
# Files made by Windows have a DOS short name too; renaming drops it.
print("s|/Photos/again 7.jpg|AGAIN7~1.JPG")
print("s|/Photos/again 8.jpg|AGAIN8~1.JPG")
print("n|/Photos/again 8.jpg|/Photos/AGAIN8~1.JPG")
print("l|/Photos")
print("n|/new.txt|/renamed.txt")
print("n|/renamed.txt|/Renamed.TXT")
print("n|/README.TXT|/Renamed.TXT")
print("m|/Box")
print("n|/Photos/again 5.jpg|/Users/Player/again 5.jpg")
print("n|/Photos|/Box/Pictures")
print("n|/Box|/Box/Pictures/inside")
print("l|/Box/Pictures")
names = [f"/Box/Pictures/again {i}.jpg" for i in range(30) if i not in (5, 8)] + ["/Box/Pictures/AGAIN8~1.JPG"]
random.shuffle(names)
for i, p in enumerate(names):
    print(f"n|{p}|/Box/Pictures/renamed photo number {i:02d} with a much longer name.jpg")
print("n|/Box/Pictures|/Photos")
print("c|/Renamed.TXT|50000|9")
print("c|/Users/Player/again 5.jpg|15000|5")
print("l|/Photos")
print("l|/Box")
print("v|/")
PY
grep -q "^n /README.TXT: ERROR already exists" build/ntfs-write-test.log || fail "a rename onto a taken name was not refused"
grep -q "^n /Box: ERROR can't move a folder into itself" build/ntfs-write-test.log || fail "a folder was moved into itself"
grep -q "^l /Photos: 70 entries" build/ntfs-write-test.log || fail "a short name was listed as a file of its own"
grep -q "^l /Box/Pictures: 69 entries" build/ntfs-write-test.log || fail "a moved folder did not keep its files"
grep -q "^c /Renamed.TXT: holds what was written" build/ntfs-write-test.log || fail "a renamed file did not read back"
grep -q "^c /Users/Player/again 5.jpg: holds what was written" build/ntfs-write-test.log || fail "a moved file did not read back"
grep -q "^l /Photos: 69 entries" build/ntfs-write-test.log || fail "a folder moved back lost files"
grep -q "^l /Box: 0 entries" build/ntfs-write-test.log || fail "a folder moved out was still listed"
grep -q "^v /: 101 names, all in order" build/ntfs-write-test.log || fail "a folder index is out of order after renaming"
! grep -q "ERROR" <(sed -n '/^n \/new.txt/,$p' build/ntfs-write-test.log | grep -v "README.TXT: ERROR already\|/Box: ERROR can't move") || fail "a rename failed"
checked "renaming"
ntfsls "$IMG" | grep -qx "Renamed.TXT" || fail "Linux does not see the new name's case"
[ "$(ntfsls -p /Photos "$IMG" | grep -c "^renamed photo number")" = 29 ] || fail "Linux does not see all 29 renamed files"
ntfsls -p /Photos "$IMG" | grep -q "^renamed photo number 28 with" || fail "Linux does not see a renamed file"
cmp -s <(ntfscat "$IMG" "/Users/Player/again 5.jpg") <(python3 -c "import sys; sys.stdout.buffer.write(bytes((i * 31 + 5) & 255 for i in range(15000)))") \
    || fail "Linux does not read a moved file back the same"

# 4. Add to the end of files a piece at a time: a file that starts in its
# record, one that gets clusters, and two that grow in turns, so their
# clusters interleave. Then read pieces back from the middle and the end.
{
    echo "w|/grow.bin|100|6"
    echo "a|/grow.bin|100,3000|6"
    echo "a|/grow.bin|3000,1500000|6"
    echo "a|/grow.bin|1500000,1500001|6"
    echo "w|/turns a.bin|0|7"
    echo "w|/turns b.bin|5000|8"
    for i in $(seq 0 19); do
        echo "a|/turns a.bin|$((i * 9000)),$((i * 9000 + 9000))|7"
        echo "a|/turns b.bin|$((5000 + i * 7000)),$((5000 + i * 7000 + 7000))|8"
    done
    echo "a|/Packed/squeezed.txt|10,20|1"
    echo "c|/grow.bin|1500001|6"
    echo "c|/turns a.bin|180000|7"
    echo "c|/turns b.bin|145000|8"
    echo "p|/grow.bin|1499990,100|6"
    echo "p|/grow.bin|4000,8192|6"
    echo "p|/turns b.bin|0,10|8"
    echo "p|/grow.bin|2000000,10|6"
    echo "v|/"
} | run
grep -q "^c /grow.bin: holds what was written" build/ntfs-write-test.log \
    && grep -q "^c /turns a.bin: holds what was written" build/ntfs-write-test.log \
    && grep -q "^c /turns b.bin: holds what was written" build/ntfs-write-test.log || fail "a file added to did not read back"
grep -q "^p /grow.bin: 11 bytes, as written" build/ntfs-write-test.log && grep -q "^p /grow.bin: 8192 bytes, as written" build/ntfs-write-test.log \
    && grep -q "^p /turns b.bin: 10 bytes, as written" build/ntfs-write-test.log && grep -q "^p /grow.bin: 0 bytes, as written" build/ntfs-write-test.log \
    || fail "reading part of a file gave the wrong bytes"
grep -q "^a /Packed/squeezed.txt: ERROR compressed" build/ntfs-write-test.log || fail "adding to a compressed file was not refused"
grep -q "^v /: 104 names, all in order" build/ntfs-write-test.log || fail "a folder index is out of order after adding to files"
! grep -q "ERROR" <(sed -n '/^p \/grow.bin\|^c \/grow.bin\|^a /p' build/ntfs-write-test.log | grep -v squeezed) || fail "adding to a file failed"
checked "adding to files"
for f in "grow.bin|1500001|6" "turns a.bin|180000|7" "turns b.bin|145000|8"; do
    IFS='|' read -r name size seed <<< "$f"
    cmp -s <(ntfscat "$IMG" "/$name") <(python3 -c "import sys; sys.stdout.buffer.write(bytes((i * 31 + $seed) & 255 for i in range($size)))") \
        || fail "Linux does not read /$name back the same"
done

# 5. A change cut off half way leaves the volume dirty: no more writing
# until Windows has checked it.
echo "x|/" | run
"$CHECK" "$IMG" < /dev/null > build/ntfs-crash.log
grep -q "^writable: Err(\"Windows has to check it first" build/ntfs-crash.log || { cat build/ntfs-crash.log; fail "writing was allowed on a volume left dirty"; }

echo "PASS: the NTFS writer saved over files, made 180 files and a folder (index split, MFT grown), deleted 440 of them, renamed and moved files and a folder, added to files a piece at a time and read pieces back, refused compressed files, bad names and a dirty volume; ntfsfix and ntfsresize found nothing wrong"
