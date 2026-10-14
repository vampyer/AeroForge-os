#!/usr/bin/env python3
"""Draws AeroForge's icons (our own artwork, in the glossy style of the
desktop) and writes them to userland/src/icons.bin for the desktop to
build in. Each icon is drawn at 4 x 96 pixels and shrunk to 96 x 96 for
smooth edges; the desktop shrinks them again to the size each place needs.

icons.bin: "AFIC", a count, then for each icon a 16-byte name (zero
padded), its width and height (u16 each) and its pixels as runs: a u16
count of fully clear pixels, a u16 count of pixels that follow as ARGB
u32s, until the icon is full. Run with --sheet out.png for a preview."""
import math
import struct
import sys

from PIL import Image, ImageChops, ImageDraw, ImageFilter

N = 96
K = 4
W = N * K


def S(v):
    return int(round(v * K))


def new():
    return Image.new('RGBA', (W, W), (0, 0, 0, 0))


def mask(shape_fn):
    m = Image.new('L', (W, W), 0)
    shape_fn(ImageDraw.Draw(m))
    return m


def rrect(box, r):
    return lambda d: d.rounded_rectangle([S(v) for v in box], radius=S(r), fill=255)


def ellipse(box):
    return lambda d: d.ellipse([S(v) for v in box], fill=255)


def poly(points):
    return lambda d: d.polygon([(S(x), S(y)) for x, y in points], fill=255)


def gradient(top, bottom, y0=0, y1=N, horizontal=False):
    """A full-size image going from `top` to `bottom` between y0 and y1."""
    g = Image.new('RGBA', (W, W))
    px = g.load()
    a, b = S(y0), max(S(y1), S(y0) + 1)
    line = []
    for i in range(W):
        t = min(max((i - a) / (b - a), 0.0), 1.0)
        line.append(tuple(int(top[k] + (bottom[k] - top[k]) * t) for k in range(4)))
    if horizontal:
        for y in range(W):
            for x in range(W):
                px[x, y] = line[x]
    else:
        for y in range(W):
            c = line[y]
            for x in range(W):
                px[x, y] = c
    return g


def rgba(c, a=255):
    return (c >> 16 & 255, c >> 8 & 255, c & 255, a)


def paint(img, m, fill):
    """Lays `fill` (a colour or a full-size image) over `img` through mask `m`."""
    if isinstance(fill, tuple):
        layer = Image.new('RGBA', (W, W), fill)
    else:
        layer = fill
    a = ImageChops.multiply(layer.getchannel('A'), m)
    layer = layer.copy()
    layer.putalpha(a)
    img.alpha_composite(layer)


def outline(img, shape, color, width=1.0):
    """A line around a shape: the shape less the shape shrunk by `width`."""
    m = mask(shape)
    inner = m.filter(ImageFilter.MinFilter(max(3, S(width) * 2 + 1)))
    paint(img, ImageChops.subtract(m, inner), color)


def shadow(img, shape, dy=2.5, blur=3.0, alpha=110):
    m = mask(shape).filter(ImageFilter.GaussianBlur(S(blur)))
    m = ImageChops.offset(m, 0, S(dy)).point(lambda v: v * alpha // 255)
    paint(img, m, (0, 0, 0, 255))


def gloss(img, shape, top_alpha=150, bottom_alpha=20, upto=0.5, box=None):
    """A soft white shine over the top part of a shape."""
    m = mask(shape)
    y0, y1 = (box[1], box[1] + (box[3] - box[1]) * upto) if box else (0, N * upto)
    g = gradient((255, 255, 255, top_alpha), (255, 255, 255, bottom_alpha), y0, y1)
    cut = Image.new('L', (W, W), 0)
    ImageDraw.Draw(cut).rectangle([0, 0, W, S(y1)], fill=255)
    paint(img, ImageChops.multiply(m, cut), g)


def glossy_box(img, box, r, top, bottom, edge, shine=True):
    shape = rrect(box, r)
    shadow(img, shape)
    paint(img, mask(shape), gradient(rgba(top), rgba(bottom), box[1], box[3]))
    if shine:
        gloss(img, rrect((box[0] + 1.5, box[1] + 1.5, box[2] - 1.5, box[3] - 1.5), r - 1), 140, 30, 0.45, box)
    outline(img, shape, rgba(edge), 1.0)


def orb(img, cx, cy, r, light, dark, edge):
    box = (cx - r, cy - r, cx + r, cy + r)
    shadow(img, ellipse(box))
    paint(img, mask(ellipse(box)), gradient(rgba(light), rgba(dark), cy - r, cy + r))
    # A glow along the bottom edge, as light through glass.
    glow = mask(ellipse((cx - r * 0.75, cy + r * 0.15, cx + r * 0.75, cy + r * 0.95))).filter(ImageFilter.GaussianBlur(S(r * 0.18)))
    paint(img, ImageChops.multiply(glow, mask(ellipse(box))), (255, 255, 255, 90))
    gloss(img, ellipse((cx - r * 0.72, cy - r * 0.93, cx + r * 0.72, cy + r * 0.1)), 190, 40, 1.0, (0, cy - r * 0.93, 0, cy + r * 0.1))
    outline(img, ellipse(box), rgba(edge), 1.0)


# ------------------------------------------------------------------ icons

def computer():
    img = new()
    # Stand: a neck and an oval foot.
    shadow(img, ellipse((24, 80, 72, 92)), 1.5, 2.5, 120)
    paint(img, mask(ellipse((24, 79, 72, 91))), gradient(rgba(0xB8BEC8), rgba(0x5A606A), 79, 91))
    gloss(img, ellipse((28, 80, 68, 86)), 160, 40, 1.0, (0, 80, 0, 86))
    paint(img, mask(poly([(42, 66), (54, 66), (57, 83), (39, 83)])), gradient(rgba(0x8A909A), rgba(0x3A3F48), 66, 83))
    # The screen: a black glossy bezel and an Aero sky inside.
    glossy_box(img, (6, 8, 90, 70), 5, 0x4A505C, 0x14161C, 0x0A0B0E, shine=False)
    glass = (11, 13, 85, 63)
    sky = gradient(rgba(0x7CCBFF), rgba(0x0F4FA8), glass[1], glass[3])
    paint(img, mask(rrect(glass, 1.5)), sky)
    # Swooshes of light across the glass.
    for (a, b, c, alpha) in [(-10, 48, 120, 120), (0, 58, 130, 70)]:
        sw = Image.new('L', (W, W), 0)
        ImageDraw.Draw(sw).ellipse([S(a), S(b), S(c), S(b + 40)], outline=255, width=S(4))
        sw = sw.filter(ImageFilter.GaussianBlur(S(1.2)))
        paint(img, ImageChops.multiply(sw, mask(rrect(glass, 1.5))), (210, 245, 255, alpha))
    gloss(img, poly([(11, 13), (85, 13), (85, 30), (11, 44)]), 120, 25, 1.0, (0, 13, 0, 44))
    outline(img, rrect((6, 8, 90, 70), 5), (120, 130, 145, 255), 0.6)
    # A small power light.
    paint(img, mask(ellipse((46.5, 65.5, 49.5, 68.5))), (120, 220, 255, 255))
    return img


def notes():
    img = new()
    page = (16, 10, 82, 90)
    shadow(img, rrect(page, 3))
    # Sheets underneath, slightly offset.
    paint(img, mask(rrect((19, 13, 85, 92), 3)), rgba(0xD8DEE8))
    paint(img, mask(rrect(page, 3)), gradient(rgba(0xFFFFFF), rgba(0xEEF3FA), page[1], page[3]))
    outline(img, rrect(page, 3), rgba(0x8592A8), 0.8)
    # A blue top band with the spiral through it.
    paint(img, mask(rrect((16, 10, 82, 24), 3)), gradient(rgba(0x6FB2F2), rgba(0x1E64C0), 10, 24))
    gloss(img, rrect((17, 11, 81, 17), 2), 150, 40, 1.0, (0, 11, 0, 17))
    for i in range(7):
        x = 22 + i * 8.6
        paint(img, mask(ellipse((x, 15, x + 4, 19))), (20, 40, 80, 255))
        ring = Image.new('L', (W, W), 0)
        ImageDraw.Draw(ring).arc([S(x - 1), S(4), S(x + 5), S(19)], 180, 360, fill=255, width=S(1.8))
        paint(img, ring, gradient(rgba(0xF2F4F8), rgba(0x8890A0), 4, 12))
    # Ruled lines and a red margin.
    for i in range(8):
        y = 32 + i * 7
        paint(img, mask(lambda d, y=y: d.rectangle([S(20), S(y), S(78), S(y + 0.8)], fill=255)), rgba(0x9DBBE3))
    paint(img, mask(lambda d: d.rectangle([S(28), S(25), S(28.8), S(89)], fill=255)), rgba(0xE59A9A))
    # Handwriting: a few grey scribbles.
    for i, w in enumerate([38, 30, 42, 22]):
        y = 29.5 + i * 7
        sc = Image.new('L', (W, W), 0)
        pts = [(S(32 + t), S(y + 1.2 * math.sin(t / 2.1))) for t in range(0, w, 1)]
        ImageDraw.Draw(sc).line(pts, fill=255, width=S(1.1))
        paint(img, sc, rgba(0x3A4A66))
    # A pencil across the corner.
    pen = Image.new('RGBA', (W, W), (0, 0, 0, 0))
    glossy_pencil(pen)
    pen = pen.rotate(-38, resample=Image.BICUBIC, center=(S(62), S(66)))
    img.alpha_composite(pen)
    return img


def glossy_pencil(img):
    body = (44, 61, 88, 71)
    shadow(img, rrect(body, 1), 2, 2, 90)
    paint(img, mask(lambda d: d.rectangle([S(v) for v in body], fill=255)), gradient(rgba(0xFFD45A), rgba(0xD98A14), 61, 71))
    gloss(img, lambda d: d.rectangle([S(44), S(62), S(88), S(65)], fill=255), 160, 60, 1.0, (0, 62, 0, 65))
    paint(img, mask(lambda d: d.rectangle([S(84), S(61), S(92), S(71)], fill=255)), gradient(rgba(0xC8CDD5), rgba(0x80868F), 61, 71))
    paint(img, mask(rrect((90, 61, 97, 71), 2)), gradient(rgba(0xF6A2A2), rgba(0xC75050), 61, 71))
    paint(img, mask(poly([(44, 61), (44, 71), (33, 66)])), gradient(rgba(0xF4D7AE), rgba(0xC7A070), 61, 71))
    paint(img, mask(poly([(37.5, 64), (37.5, 68), (33, 66)])), rgba(0x30323A))


def calculator():
    img = new()
    body = (18, 6, 78, 92)
    glossy_box(img, body, 7, 0x8C96A6, 0x3C424E, 0x23272E)
    # The display: a green-grey LCD set into the body.
    lcd = (25, 13, 71, 31)
    paint(img, mask(rrect(lcd, 2.5)), gradient(rgba(0x2A2F38), rgba(0x5B626E), lcd[1], lcd[3]))
    paint(img, mask(rrect((26.5, 14.5, 69.5, 29.5), 1.8)), gradient(rgba(0xDDEED8), rgba(0xA9C6A2), 14.5, 29.5))
    for i, d in enumerate("1234"):
        seg_digit(img, 45 + i * 6, 17.5, d)
    # Keys: four rows of light keys, with an orange key at the end.
    for row in range(4):
        for col in range(4):
            x, y = 25 + col * 11.6, 38 + row * 12.5
            key = (x, y, x + 9.4, y + 9.6)
            if col == 3 and row == 3:
                top, bottom = 0xFFC46A, 0xE07A10
            elif col == 3:
                top, bottom = 0xB7D9FF, 0x4F8FD8
            else:
                top, bottom = 0xFFFFFF, 0xC9CFD8
            shadow(img, rrect(key, 2), 1.0, 0.8, 140)
            paint(img, mask(rrect(key, 2)), gradient(rgba(top), rgba(bottom), key[1], key[3]))
            outline(img, rrect(key, 2), (40, 45, 55, 180), 0.5)
    return img


def seg_digit(img, x, y, d):
    """A small seven-segment digit 4.4 wide, 10 tall."""
    segs = {'1': 'bc', '2': 'abged', '3': 'abgcd', '4': 'fgbc'}[d]
    places = {'a': (0.4, 0, 3.6, 1), 'b': (3.4, 0.4, 4.4, 4.8), 'c': (3.4, 5.2, 4.4, 9.6), 'd': (0.4, 9, 3.6, 10),
              'g': (0.4, 4.5, 3.6, 5.5), 'f': (0, 0.4, 1, 4.8), 'e': (0, 5.2, 1, 9.6)}
    for sname in segs:
        a, b, c, e = places[sname]
        paint(img, mask(lambda dd, a=a, b=b, c=c, e=e: dd.rectangle([S(x + a), S(y + b), S(x + c), S(y + e)], fill=255)), rgba(0x23301F))


def photos():
    img = new()
    # A second print behind, turned a little.
    back = Image.new('RGBA', (W, W), (0, 0, 0, 0))
    frame = (14, 18, 84, 76)
    shadow(back, rrect(frame, 2))
    paint(back, mask(rrect(frame, 2)), gradient(rgba(0xFFFFFF), rgba(0xE6E9EE), frame[1], frame[3]))
    paint(back, mask(lambda d: d.rectangle([S(19), S(23), S(79), S(66)], fill=255)), gradient(rgba(0xF7B36A), rgba(0xB4485A), 23, 66))
    outline(back, rrect(frame, 2), rgba(0x9AA3B2), 0.6)
    img.alpha_composite(back.rotate(10, resample=Image.BICUBIC, center=(S(49), S(47))))
    front = Image.new('RGBA', (W, W), (0, 0, 0, 0))
    frame = (10, 22, 82, 84)
    shadow(front, rrect(frame, 2))
    paint(front, mask(rrect(frame, 2)), gradient(rgba(0xFFFFFF), rgba(0xE6E9EE), frame[1], frame[3]))
    pic = (15, 27, 77, 72)
    sky = gradient(rgba(0x58A8F0), rgba(0xCFE9FF), pic[1], pic[3] - 10)
    paint(front, mask(lambda d: d.rectangle([S(v) for v in pic], fill=255)), sky)
    paint(front, mask(ellipse((56, 32, 68, 44))), gradient(rgba(0xFFF4B0), rgba(0xFFC233), 32, 44))
    clip = mask(lambda d: d.rectangle([S(v) for v in pic], fill=255))
    hill1 = mask(ellipse((2, 52, 60, 100)))
    hill2 = mask(ellipse((36, 56, 110, 104)))
    paint(front, ImageChops.multiply(hill2, clip), gradient(rgba(0x7FCB5A), rgba(0x2E7A2A), 56, 72))
    paint(front, ImageChops.multiply(hill1, clip), gradient(rgba(0x9BDB6E), rgba(0x3A8C34), 52, 72))
    outline(front, rrect(frame, 2), rgba(0x9AA3B2), 0.6)
    img.alpha_composite(front.rotate(-4, resample=Image.BICUBIC, center=(S(46), S(53))))
    return img


def music():
    img = new()
    orb(img, 48, 48, 41, 0xFFB04A, 0xC2410E, 0x7A2A08)
    # A white double note.
    note = Image.new('RGBA', (W, W), (0, 0, 0, 0))
    paint(note, mask(ellipse((27, 58, 43, 70))), (255, 255, 255, 255))
    paint(note, mask(ellipse((51, 52, 67, 64))), (255, 255, 255, 255))
    paint(note, mask(lambda d: d.rectangle([S(39.5), S(26), S(43), S(64)], fill=255)), (255, 255, 255, 255))
    paint(note, mask(lambda d: d.rectangle([S(63.5), S(20), S(67), S(58)], fill=255)), (255, 255, 255, 255))
    paint(note, mask(poly([(39.5, 26), (67, 20), (67, 29), (39.5, 35)])), (255, 255, 255, 255))
    sh = note.getchannel('A').filter(ImageFilter.GaussianBlur(S(1.5)))
    paint(img, ImageChops.offset(sh, S(1), S(1.5)).point(lambda v: v * 100 // 255), (90, 30, 0, 255))
    img.alpha_composite(note)
    return img


def control_panel():
    img = new()
    body = (8, 14, 88, 82)
    glossy_box(img, body, 6, 0xEAF3FD, 0xA9C4E2, 0x51698A)
    # Three sliders in their own colours.
    for i, (top, bottom, at) in enumerate([(0x8FE07A, 0x2E9A2A, 0.7), (0xFFD27A, 0xE0880F, 0.35), (0x8CC8FF, 0x2366C8, 0.55)]):
        y = 28 + i * 16
        track = (18, y, 78, y + 5)
        paint(img, mask(rrect(track, 2.5)), gradient(rgba(0x6E7C90), rgba(0xC6D2E0), y, y + 5))
        fill_to = 18 + 60 * at
        paint(img, mask(rrect((18, y, fill_to, y + 5), 2.5)), gradient(rgba(top), rgba(bottom), y, y + 5))
        knob = (fill_to - 5, y - 4, fill_to + 5, y + 9)
        shadow(img, rrect(knob, 2.5), 1, 1, 130)
        paint(img, mask(rrect(knob, 2.5)), gradient(rgba(0xFFFFFF), rgba(0xB9C6D6), knob[1], knob[3]))
        outline(img, rrect(knob, 2.5), rgba(0x4A5A72), 0.6)
    return img


def system():
    img = new()
    orb(img, 48, 48, 41, 0x8ED2FF, 0x0D4FA8, 0x0A2E66)
    paint(img, mask(ellipse((42, 20, 54, 32))), (255, 255, 255, 255))
    paint(img, mask(rrect((42.5, 38, 53.5, 74), 2)), (255, 255, 255, 255))
    return img


def welcome():
    img = new()
    orb(img, 48, 48, 41, 0x9EEA7A, 0x1E8A2E, 0x0E4A18)
    # A white five-pointed star.
    pts = []
    for i in range(10):
        a = -math.pi / 2 + i * math.pi / 5
        r = 25 if i % 2 == 0 else 10.5
        pts.append((48 + r * math.cos(a), 50 + r * math.sin(a)))
    paint(img, mask(poly(pts)), (255, 255, 255, 255))
    return img


def folder():
    img = new()
    back = poly([(6, 22), (8, 16), (36, 16), (42, 22), (90, 22), (90, 82), (6, 82)])
    shadow(img, back)
    paint(img, mask(back), gradient(rgba(0xF2C252), rgba(0xC8901E), 16, 82))
    # Papers sticking out.
    paint(img, mask(rrect((14, 26, 82, 70), 1)), gradient(rgba(0xFFFFFF), rgba(0xE4E8EE), 26, 70))
    front = poly([(4, 36), (92, 36), (88, 84), (8, 84)])
    paint(img, mask(front), gradient(rgba(0xFFE38A), rgba(0xE9A92E), 36, 84))
    gloss(img, poly([(5, 37), (91, 37), (90.2, 50), (6, 50)]), 150, 30, 1.0, (0, 37, 0, 50))
    outline(img, front, rgba(0xB07A12), 0.8)
    outline(img, back, rgba(0xB07A12), 0.8)
    return img


def drive():
    img = new()
    body = (6, 34, 90, 72)
    glossy_box(img, body, 6, 0xE9EDF2, 0x8A93A0, 0x4E5664)
    # A dark slot and a green light.
    paint(img, mask(rrect((14, 58, 64, 63), 2)), gradient(rgba(0x3A404A), rgba(0x6A7280), 58, 63))
    paint(img, mask(ellipse((72, 56, 81, 65))), gradient(rgba(0xC8FFB0), rgba(0x2FB33A), 56, 65))
    return img


def usb():
    img = new()
    plug = (60, 38, 88, 62)
    paint(img, mask(rrect(plug, 2)), gradient(rgba(0xF2F4F7), rgba(0x9AA2AE), plug[1], plug[3]))
    outline(img, rrect(plug, 2), rgba(0x5A6270), 0.8)
    for y in (44, 52):
        paint(img, mask(lambda d, y=y: d.rectangle([S(70), S(y), S(76), S(y + 3)], fill=255)), rgba(0x3A404A))
    glossy_box(img, (8, 32, 64, 68), 8, 0x6FB2F2, 0x174E9E, 0x0E2F60)
    paint(img, mask(rrect((16, 44, 46, 56), 2)), (255, 255, 255, 200))
    return img


def network_drive():
    img = drive()
    paint(img, mask(lambda d: d.rectangle([S(45), S(72), S(51), S(82)], fill=255)), rgba(0x46505E))
    glossy_box(img, (16, 80, 80, 90), 3, 0x7FE08A, 0x1E8A34, 0x10501E)
    return img


def recycle_bin():
    img = new()
    can = poly([(22, 26), (74, 26), (68, 90), (28, 90)])
    shadow(img, can)
    glass = gradient((225, 240, 255, 230), (150, 190, 230, 230), 26, 90)
    paint(img, mask(can), glass)
    for x in (36, 48, 60):
        paint(img, mask(poly([(x - 1.5, 34), (x + 1.5, 34), (x + 1.2 - (x - 48) * 0.06, 84), (x - 1.2 - (x - 48) * 0.06, 84)])), (110, 150, 200, 180))
    gloss(img, poly([(24, 27), (40, 27), (37, 88), (30, 88)]), 150, 60, 1.0, (0, 27, 0, 88))
    outline(img, can, rgba(0x4C6A90), 0.9)
    lid = (16, 16, 80, 27)
    glossy_box(img, lid, 4, 0xD9E8F7, 0x7E9CBE, 0x3E5878)
    return img


def page(kind):
    img = new()
    sheet = poly([(18, 6), (62, 6), (80, 24), (80, 90), (18, 90)])
    shadow(img, sheet)
    paint(img, mask(sheet), gradient(rgba(0xFFFFFF), rgba(0xE9EEF5), 6, 90))
    fold = poly([(62, 6), (80, 24), (62, 24)])
    paint(img, mask(fold), gradient(rgba(0xDDE5F0), rgba(0xB8C6D8), 6, 24))
    outline(img, sheet, rgba(0x7D8AA0), 0.8)
    outline(img, fold, rgba(0x7D8AA0), 0.6)
    if kind == 'text':
        for i in range(8):
            w = [50, 46, 52, 38, 50, 44, 50, 30][i]
            y = 32 + i * 7
            paint(img, mask(lambda d, y=y, w=w: d.rectangle([S(25), S(y), S(25 + w * 0.92), S(y + 2)], fill=255)), rgba(0x8FA6C6))
    elif kind == 'sound':
        orb(img, 49, 58, 22, 0xFFB04A, 0xC2410E, 0x7A2A08)
        paint(img, mask(ellipse((39, 61, 49, 69))), (255, 255, 255, 255))
        paint(img, mask(lambda d: d.rectangle([S(46), S(43), S(49), S(65)], fill=255)), (255, 255, 255, 255))
        paint(img, mask(poly([(46, 43), (58, 47), (58, 53), (49, 50)])), (255, 255, 255, 255))
    elif kind == 'picture':
        pic = (25, 34, 73, 80)
        paint(img, mask(lambda d: d.rectangle([S(v) for v in pic], fill=255)), gradient(rgba(0x58A8F0), rgba(0xCFE9FF), 34, 70))
        clip = mask(lambda d: d.rectangle([S(v) for v in pic], fill=255))
        paint(img, mask(ellipse((56, 38, 66, 48))), rgba(0xFFC233))
        paint(img, ImageChops.multiply(mask(ellipse((10, 62, 64, 104))), clip), gradient(rgba(0x9BDB6E), rgba(0x3A8C34), 62, 80))
        paint(img, ImageChops.multiply(mask(ellipse((44, 66, 104, 108))), clip), gradient(rgba(0x7FCB5A), rgba(0x2E7A2A), 66, 80))
        outline(img, lambda d: d.rectangle([S(v) for v in pic], fill=255), rgba(0x7D8AA0), 0.6)
    return img


ICONS = [
    ('computer', computer), ('notes', notes), ('calculator', calculator), ('photos', photos), ('music', music),
    ('controlpanel', control_panel), ('system', system), ('welcome', welcome), ('folder', folder), ('drive', drive),
    ('usb', usb), ('netdrive', network_drive), ('bin', recycle_bin), ('page', lambda: page('blank')),
    ('text', lambda: page('text')), ('sound', lambda: page('sound')), ('picture', lambda: page('picture')),
]


def encode(img):
    w, h = img.size
    px = [(a << 24) | (r << 16) | (g << 8) | b for r, g, b, a in zip(*[iter(img.tobytes())] * 4)]
    out, i = bytearray(), 0
    while i < len(px):
        clear = 0
        while i < len(px) and px[i] >> 24 == 0 and clear < 65535:
            clear += 1
            i += 1
        start = i
        while i < len(px) and px[i] >> 24 != 0 and i - start < 65535:
            i += 1
        out += struct.pack('<HH', clear, i - start)
        for p in px[start:i]:
            out += struct.pack('<I', p)
    return struct.pack('<HH', w, h) + bytes(out)


def main():
    made = [(name, fn().resize((N, N), Image.LANCZOS)) for name, fn in ICONS]
    if len(sys.argv) > 2 and sys.argv[1] == '--sheet':
        sheet = Image.new('RGBA', (len(made) * 104 + 8, 104 + 60 + 40), (40, 90, 160, 255))
        bg = gradient(rgba(0x2D6CB5), rgba(0x0E3770), 0, N)
        for i, (_, im) in enumerate(made):
            sheet.alpha_composite(im, (8 + i * 104, 4))
            sheet.alpha_composite(im.resize((48, 48), Image.LANCZOS), (8 + i * 104 + 24, 108))
            sheet.alpha_composite(im.resize((24, 24), Image.LANCZOS), (8 + i * 104 + 36, 164))
            sheet.alpha_composite(im.resize((16, 16), Image.LANCZOS), (8 + i * 104 + 64, 168))
        sheet.save(sys.argv[2])
        return
    out = bytearray(b'AFIC' + struct.pack('<I', len(made)))
    for name, im in made:
        out += name.encode().ljust(16, b'\0') + encode(im)
    open('userland/src/icons.bin', 'wb').write(out)
    print('icons.bin: %d icons, %d bytes' % (len(made), len(out)))


main()
