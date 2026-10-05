#!/usr/bin/env python3
"""Measures the tone in a WAV file QEMU's wav audio backend is writing
(the header is only completed when QEMU exits, so the samples are read raw):
prints 'tone <seconds> s, <hz> Hz, peak <value>' for the stretch that is
not silent, or 'silence'."""
import struct
import sys

data = open(sys.argv[1], 'rb').read()
if len(data) < 46:
    print('silence')
    sys.exit(0)
channels, rate = struct.unpack('<HI', data[22:28])
count = (len(data) - 44) // 2
samples = struct.unpack('<%dh' % count, data[44:44 + 2 * count])[::channels]
loud = [i for i, v in enumerate(samples) if abs(v) > 1000]
if not loud:
    print('silence')
    sys.exit(0)
tone = samples[loud[0]:loud[-1] + 1]
ups = sum(1 for a, b in zip(tone, tone[1:]) if a < 0 <= b)
print('tone %.2f s, %.0f Hz, peak %d' % (len(tone) / rate, ups * rate / len(tone), max(abs(v) for v in tone)))
