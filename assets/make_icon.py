"""Generates the SizeMatters icon (strata mark) as .ico, .png and raw RGBA.
Run: python3 assets/make_icon.py"""
from PIL import Image, ImageDraw
import os

HERE = os.path.dirname(os.path.abspath(__file__))
S = 1024  # draw big, downsample for crisp edges

def rr(d, box, r, fill):
    d.rounded_rectangle(box, radius=r, fill=fill)

def make():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    # background: vertical gradient navy -> deep indigo, rounded square
    bg = Image.new("RGBA", (S, S))
    for y in range(S):
        t = y / S
        c = (int(22 + 18 * t), int(28 + 10 * t), int(48 + 30 * t), 255)
        ImageDraw.Draw(bg).line([(0, y), (S, y)], fill=c)
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((24, 24, S - 24, S - 24), radius=210, fill=255)
    img.paste(bg, (0, 0), mask)
    d = ImageDraw.Draw(img)

    m = 150            # side margin
    g = 22             # gap
    h = 150            # row height
    y = 190
    W = S - 2 * m
    # row 0: the whole disk
    rr(d, (m, y, S - m, y + h), 34, (232, 238, 250, 255))
    y += h + g
    # row 1: three big branches
    parts = [(0.52, (86, 156, 255)), (0.30, (239, 108, 96)), (0.18, (92, 196, 128))]
    x = m
    row1 = []
    for frac, col in parts:
        w = W * frac
        rr(d, (x, y, x + w - g, y + h), 30, col + (255,))
        row1.append((x, w, col))
        x += w
    y += h + g
    # row 2: subdivisions (lighter)
    def lighter(c, k):
        return tuple(int(v + (255 - v) * k) for v in c) + (255,)
    subs = [[0.6, 0.4], [0.7, 0.3], [1.0]]
    for (x0, w, col), sp in zip(row1, subs):
        xx = x0
        for f in sp:
            ww = (w - g) * f
            if ww - g > 10:
                rr(d, (xx, y, xx + ww - (g if f != sp[-1] else 0), y + h * 0.8), 26, lighter(col, 0.35))
            xx += ww
    y += h * 0.8 + g
    # row 3: a few small blocks (fading)
    x0, w, col = row1[0]
    rr(d, (x0, y, x0 + (w - g) * 0.6 * 0.55, y + h * 0.6), 22, lighter(col, 0.6))
    x1, w1, col1 = row1[1]
    rr(d, (x1, y, x1 + (w1 - g) * 0.7 * 0.7, y + h * 0.6), 22, lighter(col1, 0.6))
    return img

big = make()
sizes = [16, 24, 32, 48, 64, 128, 256]
big.resize((256, 256), Image.LANCZOS).save(os.path.join(HERE, "icon.ico"), sizes=[(s, s) for s in sizes])
big.resize((512, 512), Image.LANCZOS).save(os.path.join(HERE, "icon.png"))
small = big.resize((64, 64), Image.LANCZOS)
with open(os.path.join(HERE, "icon_64.rgba"), "wb") as f:
    f.write(small.tobytes())
print("ok")
