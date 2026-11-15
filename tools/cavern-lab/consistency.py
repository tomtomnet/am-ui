#!/usr/bin/env python3
"""Reference-free check of the JOC object reconstruction: the objects re-rendered by Cavern and folded to stereo
should add back up to the E-AC-3 core (the encoder's own downmix of the same objects) folded the same way.

NCC(fold(render), fold(bed)) per second, with the QMF bank delay (577 samples) applied to the bed.

Usage: consistency.py RENDER BED [--channels 16] [--names "..."] [--lag 577] [--window START END]
"""
from __future__ import annotations

import argparse
import math
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import analyze  # noqa: E402

RATE = 48000


def bed_fold(bed: np.ndarray) -> np.ndarray:
    out = np.zeros((len(bed), 2))
    out[:, 0] = bed[:, 0] + 0.70710678 * bed[:, 2] + bed[:, 4]
    out[:, 1] = bed[:, 1] + 0.70710678 * bed[:, 2] + bed[:, 5]
    return out


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("render", type=Path)
    parser.add_argument("bed", type=Path)
    parser.add_argument("--channels", type=int, default=16)
    parser.add_argument("--names")
    parser.add_argument("--lag", type=int, default=577)
    parser.add_argument("--window", type=float, nargs=2, default=(4.0, 7.9))
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)
    names = args.names.split() if args.names else analyze.NAMES
    render = analyze.load(args.render, args.channels)
    bed = analyze.load(args.bed, 6)
    ours = analyze.fold_down(render, names)
    theirs = bed_fold(bed.astype(np.float64))
    n = min(len(ours) - args.lag, len(theirs))
    ours = ours[args.lag:args.lag + n]
    theirs = theirs[:n]
    seconds = n // RATE
    rows = []
    for i in range(seconds):
        a, b = i * RATE, (i + 1) * RATE
        rows.append(0.5 * (analyze.ncc(ours[a:b, 0], theirs[a:b, 0]) + analyze.ncc(ours[a:b, 1], theirs[a:b, 1])))
    wa, wb = int(args.window[0] * RATE), int(args.window[1] * RATE)
    window = 0.5 * (analyze.ncc(ours[wa:wb, 0], theirs[wa:wb, 0]) + analyze.ncc(ours[wa:wb, 1], theirs[wa:wb, 1]))
    energy = 10 * math.log10(float((ours ** 2).mean()) / float((theirs ** 2).mean()))
    result = {"render": str(args.render), "median_ncc": float(np.median(rows)), "mean_ncc": float(np.mean(rows)),
              "p10_ncc": float(np.percentile(rows, 10)), "window_ncc": window, "fold_energy_ratio_db": energy}
    if not args.quiet:
        print(f"{args.render.parent.name:<18} consistency NCC render-fold vs bed-fold: median {result['median_ncc']:.3f} mean {result['mean_ncc']:.3f} "
              f"p10 {result['p10_ncc']:.3f} window {args.window[0]}-{args.window[1]} s {window:.3f}; fold energy render/bed {energy:+.1f} dB")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
