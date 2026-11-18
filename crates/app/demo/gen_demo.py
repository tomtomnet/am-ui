#!/usr/bin/env python3
"""Make the demonstration's music (`am-ui --demo`), deterministic (seed 7), next to this script:
  covers/0000.jpg .. 0999.jpg   600x600 abstract artwork (JPEG q88, ~60-120 KB; git-ignored)
  albums.json                   [{id, title, artist, cover}] (invented names)
  nowplaying.json               {title, artist, cover, lines[24]} (placeholder text)

    python3 crates/app/demo/gen_demo.py      (needs numpy and Pillow)

Without the covers the demonstration shows empty artwork.
"""
import json
import os
import random
from multiprocessing import Pool

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

OUT = os.path.dirname(os.path.abspath(__file__))
N = 1000
SIZE = 600

WORDS = ("velvet paper harbor neon quiet summer glass river ember orbit lantern hollow silver tide "
         "midnight garden static honey cinder violet north echo amber satellite marble fever lotus "
         "cobalt meadow signal winter carousel ivory thunder saffron mirage cascade halo prism juniper "
         "rooftop atlas solace drift polaroid monsoon gravity lullaby origami horizon citrus").split()
CJK = ["밤의 정원", "푸른 계절", "夏の終わり", "星降る街", "月光の窓", "雨の記憶", "遠い海", "새벽 세시",
       "银色的河", "春日之光", "夢の中で", "바람의 노래"]
FIRST = ("Nova Mira Juno Theo Lio Sora Ines Kai Remy Ada Elio Yuna Oskar Lumi Ezra Noor Taro Vera Milo "
         "Selin Ari Wren Dax Ilse").split()
LAST = ("Vale Arden Kestrel Moreau Lindqvist Okafor Hart Sato Brandt Quill Novak Reyes Halden Frost "
        "Ishikawa Marlowe Duval Petrov Lark Okoro").split()
BANDS = ["The Paper Lanterns", "Glass Orbit", "Static Meadow", "Cobalt Youth", "Velvet Signal",
         "Hollow Tide", "Neon Carousel", "Saffron Club", "Atlas & The Drift", "Quiet Satellites"]


def title(r):
    if r.random() < 0.12:
        return r.choice(CJK)
    k = r.choice([1, 2, 2, 3, 3, 4])
    t = " ".join(r.choice(WORDS) for _ in range(k)).title()
    if r.random() < 0.08:
        t += " (Deluxe Edition)"
    return t


def artist(r):
    x = r.random()
    if x < 0.25:
        return r.choice(BANDS)
    if x < 0.32:
        return r.choice(CJK[:6]) + " " + r.choice(["Trio", "Club", "Band"])
    return f"{r.choice(FIRST)} {r.choice(LAST)}"


def palette(r):
    h = r.random()
    out = []
    for i in range(4):
        hh = (h + r.uniform(-0.18, 0.18) + (0.5 if i == 3 and r.random() < 0.5 else 0)) % 1.0
        s = r.uniform(0.35, 0.95)
        v = r.uniform(0.25, 1.0)
        import colorsys
        out.append(tuple(int(c * 255) for c in colorsys.hsv_to_rgb(hh, s, v)))
    return out


def cover(i):
    r = random.Random(7_000 + i)
    pal = palette(r)
    # diagonal gradient between two palette colors
    yy, xx = np.mgrid[0:SIZE, 0:SIZE].astype(np.float32) / SIZE
    ang = r.uniform(0, np.pi)
    g = (np.cos(ang) * xx + np.sin(ang) * yy)
    g = (g - g.min()) / (g.max() - g.min())
    a, b = np.array(pal[0], np.float32), np.array(pal[1], np.float32)
    img = (a[None, None, :] * (1 - g[..., None]) + b[None, None, :] * g[..., None])
    im = Image.fromarray(img.clip(0, 255).astype(np.uint8), "RGB")
    d = ImageDraw.Draw(im, "RGBA")
    style = r.randrange(4)
    for _ in range(r.randint(3, 9)):
        c = pal[r.randrange(4)] + (r.randint(90, 230),)
        x, y = r.randint(-100, SIZE), r.randint(-100, SIZE)
        s = r.randint(40, 320)
        if style == 0:
            d.ellipse((x, y, x + s, y + s), fill=c)
        elif style == 1:
            d.rectangle((x, y, x + s, y + s // 2), fill=c)
        elif style == 2:
            d.polygon([(x, y), (x + s, y + r.randint(-80, 80)), (x + r.randint(-80, 80), y + s)], fill=c)
        else:
            w = r.randint(6, 40)
            d.line((x, y, x + s * 2, y + s), fill=c, width=w)
    if r.random() < 0.5:
        im = im.filter(ImageFilter.GaussianBlur(r.uniform(0.5, 6)))
    # photographic-ish grain so JPEG sizes/decode costs resemble real artwork
    noise = np.random.default_rng(i).normal(0, r.uniform(4, 12), (SIZE, SIZE, 1)).astype(np.float32)
    arr = np.asarray(im, np.float32) + noise
    im = Image.fromarray(arr.clip(0, 255).astype(np.uint8), "RGB")
    path = os.path.join(OUT, "covers", f"{i:04d}.jpg")
    im.save(path, "JPEG", quality=88, optimize=False)
    return os.path.getsize(path)


def main():
    os.makedirs(os.path.join(OUT, "covers"), exist_ok=True)
    with Pool(12) as p:
        sizes = p.map(cover, range(N), chunksize=20)
    r = random.Random(7)
    albums = [{"id": i, "title": title(r), "artist": artist(r), "cover": f"covers/{i:04d}.jpg"}
              for i in range(N)]
    with open(os.path.join(OUT, "albums.json"), "w") as f:
        json.dump(albums, f, ensure_ascii=False, indent=0)
    lr = random.Random(11)
    lines = []
    for _ in range(24):
        n = lr.randint(3, 7)
        lines.append(" ".join(lr.choice(WORDS) for _ in range(n)).capitalize())
    np_ = {"title": albums[0]["title"], "artist": albums[0]["artist"], "cover": albums[0]["cover"],
           "lines": lines}
    with open(os.path.join(OUT, "nowplaying.json"), "w") as f:
        json.dump(np_, f, ensure_ascii=False, indent=1)
    print(f"{N} covers, {sum(sizes) / N / 1024:.0f} KB average, {sum(sizes) / 1e6:.0f} MB total")


if __name__ == "__main__":
    main()
