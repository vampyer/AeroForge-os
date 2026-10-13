#!/usr/bin/env python3
"""Makes the sample pictures and songs that go on the test disk (all drawn
and composed here, so they are AeroForge's own): a sunset over hills as JPEG,
a glossy orb on a see-through background as PNG, a small striped flag as
BMP, a short tune as WAV (44.1 kHz mono) and the same tune with a bass line
as MP3 (44.1 kHz stereo, through ffmpeg's libmp3lame).

usage: make-media.py <out dir>"""
import math, os, struct, subprocess, sys, wave
from PIL import Image, ImageDraw

out = sys.argv[1]
os.makedirs(os.path.join(out, "Pictures"), exist_ok=True)
os.makedirs(os.path.join(out, "Music"), exist_ok=True)

# Sunset: a sky from deep blue to orange, a sun, two rows of hills.
w, h = 1280, 800
img = Image.new("RGB", (w, h))
d = ImageDraw.Draw(img)
for y in range(h):
    t = y / h
    d.line([(0, y), (w, y)], fill=(int(30 + 225 * t), int(40 + 120 * t * t), int(120 - 60 * t)))
d.ellipse([w * 0.6, h * 0.45, w * 0.6 + 180, h * 0.45 + 180], fill=(255, 220, 120))
for base, color, amp, freq in [(0.72, (60, 40, 70), 50, 2.0), (0.82, (25, 20, 35), 40, 3.3)]:
    pts = [(x, h * base - amp * math.sin(x / w * freq * math.pi + base * 9)) for x in range(0, w + 1, 8)]
    d.polygon(pts + [(w, h), (0, h)], fill=color)
img.save(os.path.join(out, "Pictures", "Sunset.jpg"), quality=88)

# A glossy orb, see-through around it.
s = 256
img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
px = img.load()
for y in range(s):
    for x in range(s):
        dx, dy = (x - s / 2) / (s / 2 - 8), (y - s / 2) / (s / 2 - 8)
        r2 = dx * dx + dy * dy
        if r2 <= 1:
            shine = max(0.0, 1 - ((dx + 0.3) ** 2 + (dy + 0.45) ** 2) * 4)
            g = 1 - r2 * 0.6
            px[x, y] = (int(40 * g + 215 * shine), int(160 * g + 95 * shine), int(90 * g + 165 * shine), 255)
        elif r2 <= 1.1:
            px[x, y] = (0, 60, 30, int(255 * (1.1 - r2) / 0.1))
img.save(os.path.join(out, "Pictures", "Green orb.png"))

# A small flag in 24-bit BMP.
img = Image.new("RGB", (90, 60))
d = ImageDraw.Draw(img)
for i, c in enumerate([(200, 40, 40), (240, 240, 240), (40, 90, 200)]):
    d.rectangle([0, i * 20, 89, i * 20 + 19], fill=c)
img.save(os.path.join(out, "Pictures", "Flag.bmp"))

# A tune: a melody of plucked notes, then the MP3 adds a bass line.
rate = 44100
notes = [0, 4, 7, 12, 7, 4, 5, 9, 12, 9, 5, 2, 7, 11, 14, 11, 7, 12]
beat = 0.25
def tune(bass):
    frames = []
    for i, n in enumerate(notes):
        f = 440 * 2 ** ((n - 9) / 12)
        fb = f / 4
        for k in range(int(rate * beat)):
            t = k / rate
            env = math.exp(-t * 6)
            l = 0.35 * env * math.sin(2 * math.pi * f * t)
            r = l
            if bass:
                b = 0.25 * math.exp(-t * 3) * math.sin(2 * math.pi * fb * t)
                l, r = l + b, l * 0.6 + b
            frames.append((l, r))
    return frames
with wave.open(os.path.join(out, "Music", "Chimes.wav"), "wb") as f:
    f.setnchannels(1); f.setsampwidth(2); f.setframerate(rate)
    f.writeframes(b"".join(struct.pack("<h", int(l * 32000)) for l, _ in tune(False)))
raw = b"".join(struct.pack("<hh", int(l * 32000), int(r * 32000)) for l, r in tune(True) * 2)
subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "s16le", "-ar", str(rate), "-ac", "2", "-i", "-",
                "-c:a", "libmp3lame", "-b:a", "128k", "-metadata", "title=Little Tune", "-metadata", "artist=AeroForge",
                os.path.join(out, "Music", "Little Tune.mp3")], input=raw, check=True)
