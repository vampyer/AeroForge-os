#!/usr/bin/env python3
"""A small exFAT tool for test images, since exfatprogs can format a volume
but not put files on it (and mounting needs root). Written from Microsoft's
exFAT specification, separately from the kernel's driver; fsck.exfat checks
what it writes.

usage:
  exfat.py add <image> <offset> <dir>   copy the files and folders under <dir> into the root
  exfat.py cat <image> <offset> <path>  print a file
  exfat.py ls  <image> <offset> <path>  list a folder, one name per line ("/" ends folders)

Files are written in one piece with the "no FAT chain" flag, as Windows does.
<offset> is the partition's byte offset in the image.
"""
import os
import struct
import sys
import time


def checksum(data, skip=()):
    s = 0
    for i, b in enumerate(data):
        if i in skip:
            continue
        s = (((s >> 1) | (s << 15)) + b) & 0xFFFF
    return s


def stamp():
    t = time.localtime()
    return ((t.tm_year - 1980) << 25 | t.tm_mon << 21 | t.tm_mday << 16
            | t.tm_hour << 11 | t.tm_min << 5 | t.tm_sec // 2)


class Volume:
    def __init__(self, path, offset):
        self.f = open(path, "r+b")
        self.off = offset
        bs = self.read(0, 512)
        assert bs[3:11] == b"EXFAT   ", "not exFAT"
        self.bps = 1 << bs[108]
        self.cb = self.bps << bs[109]
        self.fat = struct.unpack_from("<I", bs, 80)[0] * self.bps
        self.heap = struct.unpack_from("<I", bs, 88)[0] * self.bps
        self.count = struct.unpack_from("<I", bs, 92)[0]
        self.root = struct.unpack_from("<I", bs, 96)[0]
        root = self.chain(self.root)
        raw = b"".join(self.read_cluster(c) for c in root)
        for i in range(0, len(raw), 32):
            if raw[i] == 0x81:
                first, length = struct.unpack_from("<IQ", raw, i + 20)
                self.bitmap_at = self.cluster_pos(first)  # mkfs.exfat writes it in one piece
                self.bitmap = bytearray(self.read(self.bitmap_at, (self.count + 7) // 8))

    def read(self, pos, n):
        self.f.seek(self.off + pos)
        return self.f.read(n)

    def write(self, pos, data):
        self.f.seek(self.off + pos)
        self.f.write(data)

    def cluster_pos(self, c):
        return self.heap + (c - 2) * self.cb

    def read_cluster(self, c):
        return self.read(self.cluster_pos(c), self.cb)

    def fat_get(self, c):
        return struct.unpack("<I", self.read(self.fat + 4 * c, 4))[0]

    def chain(self, first, n=None):
        out, c = [], first
        while 2 <= c < self.count + 2 and (n is None or len(out) < n):
            out.append(c)
            c = self.fat_get(c)
        return out

    def stream_clusters(self, first, length, contiguous):
        n = -(-length // self.cb)
        if first == 0 or n == 0:
            return []
        return list(range(first, first + n)) if contiguous else self.chain(first, n)

    def entries(self, clusters):
        """(name, attrs, first, length, valid, contiguous) per entry set."""
        raw = b"".join(self.read_cluster(c) for c in clusters)
        out, i = [], 0
        while i < len(raw):
            t = raw[i]
            if t == 0:
                break
            if t != 0x85:
                i += 32
                continue
            n = raw[i + 1] + 1
            s = raw[i:i + 32 * n]
            assert struct.unpack_from("<H", s, 2)[0] == checksum(s, (2, 3)), "bad entry set checksum"
            flags, nlen = s[33], s[35]
            valid, first, length = struct.unpack_from("<Q", s, 40)[0], *struct.unpack_from("<IQ", s, 52)
            name = b"".join(s[k + 2:k + 32] for k in range(64, 32 * n, 32)).decode("utf-16-le")[:nlen]
            # Windows finds names by this hash (exact for the ASCII names the tests use).
            assert struct.unpack_from("<H", s, 36)[0] == checksum(name.upper().encode("utf-16-le")), f"{name}: bad name hash"
            out.append((name, struct.unpack_from("<H", s, 4)[0], first, length, valid, bool(flags & 2)))
            i += 32 * n
        return out

    def lookup(self, path):
        clusters = self.chain(self.root)
        item = ("/", 0x10, self.root, len(clusters) * self.cb, 0, False)
        for part in [p for p in path.split("/") if p]:
            assert item[1] & 0x10, "not a folder"
            found = [e for e in self.entries(clusters) if e[0].upper() == part.upper()]
            assert found, f"{part}: not found"
            item = found[0]
            clusters = self.stream_clusters(item[2], item[3], item[5])
        return item, clusters

    def cat(self, path):
        item, clusters = self.lookup(path)
        data = b"".join(self.read_cluster(c) for c in clusters)[:item[4]]
        return data + bytes(item[3] - len(data))

    # ---- writing (contiguous files only, no folder growth: enough for test images)

    def alloc(self, n):
        run = []
        for i in range(self.count):
            if self.bitmap[i // 8] & (1 << (i % 8)):
                run = []
                continue
            run.append(i + 2)
            if len(run) == n:
                for c in run:
                    self.bitmap[(c - 2) // 8] |= 1 << ((c - 2) % 8)
                self.write(self.bitmap_at, bytes(self.bitmap))
                return run
        raise SystemExit("exfat.py: no room")

    def add_set(self, dir_clusters, name, attrs, first, length):
        units = name.encode("utf-16-le")
        names = -(-len(name) // 15)
        s = bytearray(32 * (2 + names))
        now = stamp()
        s[0], s[1] = 0x85, 1 + names
        struct.pack_into("<HHIII", s, 4, attrs, 0, now, now, now)
        s[32], s[33], s[35] = 0xC0, (1 if first else 0) | (2 if first else 0), len(name)
        struct.pack_into("<H", s, 36, checksum(name.upper().encode("utf-16-le")))
        struct.pack_into("<QIIQ", s, 40, length, 0, first, length)
        for k in range(names):
            s[64 + 32 * k] = 0xC1
            chunk = units[30 * k:30 * k + 30]
            s[64 + 32 * k + 2:64 + 32 * k + 2 + len(chunk)] = chunk
        struct.pack_into("<H", s, 2, checksum(s, (2, 3)))
        raw = b"".join(self.read_cluster(c) for c in dir_clusters)
        need, run = len(s) // 32, 0
        for i in range(0, len(raw), 32):
            run = run + 1 if raw[i] & 0x80 == 0 else 0
            if run == need:
                slot = i // 32 - need + 1
                break
        else:
            raise SystemExit("exfat.py: folder full")
        pos = slot * 32
        self.write(self.cluster_pos(dir_clusters[pos // self.cb]) + pos % self.cb, bytes(s))  # sets fit in one cluster here

    def add_tree(self, src, dir_clusters):
        for name in sorted(os.listdir(src)):
            full = os.path.join(src, name)
            if os.path.isdir(full):
                c = self.alloc(1)
                self.write(self.cluster_pos(c[0]), bytes(self.cb))
                self.add_set(dir_clusters, name, 0x10, c[0], self.cb)
                self.add_tree(full, c)
            else:
                data = open(full, "rb").read()
                c = self.alloc(-(-len(data) // self.cb)) if data else []
                if data:
                    self.write(self.cluster_pos(c[0]), data)
                self.add_set(dir_clusters, name, 0x20, c[0] if c else 0, len(data))


def main():
    cmd, img, offset, arg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
    v = Volume(img, offset)
    if cmd == "add":
        v.add_tree(arg, v.chain(v.root))
    elif cmd == "cat":
        sys.stdout.buffer.write(v.cat(arg))
    elif cmd == "ls":
        _, clusters = v.lookup(arg)
        for e in v.entries(clusters):
            print(e[0] + ("/" if e[1] & 0x10 else ""))
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    try:
        main()
    except AssertionError as e:
        raise SystemExit(f"exfat.py: {e}")
