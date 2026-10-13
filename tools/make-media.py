#!/usr/bin/env python3
"""Makes the sample pictures and songs that go on the test disk (all drawn
and composed here, so they are AeroForge's own): a sunset over hills as JPEG,
a glossy orb on a see-through background as PNG, a small striped flag as
BMP, a short tune as WAV (44.1 kHz mono) and the same tune with a bass line
as MP3 (44.1 kHz stereo, through ffmpeg's libmp3lame), and in Music/Formats
a short note in each kind of song file Music plays (FLAC, OGG, WAV and MP3,
through ffmpeg).

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

# Formats: one short note per kind of song file Music plays, each its own
# pitch with a little silence after it, so the boot test can tell them apart
# on the sound card. Played in name order.
fmt_dir = os.path.join(out, "Music", "Formats")
os.makedirs(fmt_dir, exist_ok=True)
def note(hz, rate, channels):
    frames = []
    for k in range(int(rate * 0.9)):
        t = k / rate
        env = min(1.0, t / 0.01, (0.6 - t) / 0.01) if t < 0.6 else 0.0
        v = 0.25 * env * math.sin(2 * math.pi * hz * t)
        frames.append((v,) * channels)
    return frames
def encode(name, hz, rate, channels, args):
    raw = b"".join(struct.pack("<" + "h" * channels, *(int(v * 32767) for v in f)) for f in note(hz, rate, channels))
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "s16le", "-ar", str(rate), "-ac", str(channels), "-i", "-",
                    *args, "-map_metadata", "-1", "-fflags", "+bitexact", os.path.join(fmt_dir, name)], input=raw, check=True)
encode("1 FLAC 24-bit.flac", 330, 44100, 2, ["-c:a", "flac", "-sample_fmt", "s32"])
encode("2 FLAC mono.flac", 392, 22050, 1, ["-c:a", "flac"])
encode("3 OGG.ogg", 440, 44100, 2, ["-c:a", "libvorbis", "-q:a", "4"])
encode("4 OGG mono.ogg", 494, 22050, 1, ["-c:a", "libvorbis", "-q:a", "0"])
encode("5 WAV 8-bit.wav", 587, 22050, 1, ["-c:a", "pcm_u8"])
encode("6 WAV 24-bit.wav", 659, 48000, 2, ["-c:a", "pcm_s24le"])
encode("7 WAV float.wav", 740, 44100, 2, ["-c:a", "pcm_f32le"])
encode("8 MP3 64k.mp3", 880, 22050, 1, ["-c:a", "libmp3lame", "-b:a", "64k"])
