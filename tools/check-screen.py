#!/usr/bin/env python3
"""Checks a QEMU screendump (binary PPM) taken during the drawtest program.

Usage: check-screen.py drawtest <ppm>   red, green, blue and white quarters
                                        with a yellow 64x64 square at 100,100
       check-screen.py console <ppm>    the console is back on screen
"""
import sys


def load(path):
    data = open(path, 'rb').read()
    fields = []
    pos = 0
    while len(fields) < 4:
        while data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b'#':
            pos = data.index(b'\n', pos)
            continue
        end = pos
        while not data[end:end + 1].isspace():
            end += 1
        fields.append(data[pos:end])
        pos = end
    if fields[0] != b'P6' or fields[3] != b'255':
        sys.exit(f'{path}: not an 8-bit binary PPM')
    w, h = int(fields[1]), int(fields[2])
    pixels = data[pos + 1:]

    def at(x, y):
        i = (y * w + x) * 3
        return tuple(pixels[i:i + 3])
    return w, h, at


def main():
    mode, path = sys.argv[1], sys.argv[2]
    w, h, at = load(path)
    if mode == 'drawtest':
        want = {
            (w // 4, h // 4): (255, 0, 0),
            (3 * w // 4, h // 4): (0, 255, 0),
            (w // 4, 3 * h // 4): (0, 0, 255),
            (3 * w // 4, 3 * h // 4): (255, 255, 255),
            (w // 2 - 1, h // 2 - 1): (255, 0, 0),
            (w // 2, h // 2): (255, 255, 255),
            (100, 100): (255, 255, 0),
            (163, 163): (255, 255, 0),
            (99, 99): (255, 0, 0),
            (164, 164): (255, 0, 0),
        }
    elif mode == 'console':
        # Inside the console window's padding (see fb::draw_scene).
        x, y = w // 14 + 2, h // 14 + 30 + 2
        want = {(x, y): (0x0c, 0x16, 0x26)}
    else:
        sys.exit(__doc__)
    bad = [f'{xy} is {at(*xy)}, expected {rgb}' for xy, rgb in want.items() if at(*xy) != rgb]
    if bad:
        sys.exit(f'{path} ({w}x{h}): ' + '; '.join(bad))
    print(f'{path}: {w}x{h}, {mode} screen OK')


main()
