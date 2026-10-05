#!/usr/bin/env python3
"""Measures the tones in a WAV file QEMU's wav audio backend is writing (the
header is only completed when QEMU exits, so the samples are read raw).
Prints one line per sound separated from the next by at least 20 ms of
silence: 'tone <seconds> s, <hz> Hz, peak <value>', or 'silence'."""
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
segments, start, prev = [], loud[0], loud[0]
for i in loud[1:]:
    if i - prev > rate // 50:
        segments.append((start, prev))
        start = i
    prev = i
segments.append((start, prev))
for a, b in segments:
    tone = samples[a:b + 1]
    if len(tone) < rate // 100:
        continue
    ups = sum(1 for x, y in zip(tone, tone[1:]) if x < 0 <= y)
    print('tone %.2f s, %.0f Hz, peak %d' % (len(tone) / rate, ups * rate / len(tone), max(abs(v) for v in tone)))
