#!/usr/bin/env python3
"""Table of burst metrics / reference correlation for every variant directory under a tag directory.

Usage: compare.py TAG_DIR --ref REF.f32le [--window 4.5 7.9] [--burst 5.55 6.75] [--offset N]
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import analyze  # noqa: E402


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("tag_dir", type=Path)
    parser.add_argument("--ref", type=Path, required=True)
    parser.add_argument("--window", type=float, nargs=2, default=(4.5, 7.9))
    parser.add_argument("--burst", type=float, nargs=2, default=(5.55, 6.75))
    parser.add_argument("--offset", type=int)
    parser.add_argument("--bed", type=Path, help="6-channel core decode (bed.f32le) for the reference-free consistency metric")
    parser.add_argument("--lag", type=int, default=577, help="QMF bank delay between bed and render")
    args = parser.parse_args(argv)
    ref = analyze.load(args.ref, 2)
    bed = None
    if args.bed:
        import consistency
        bed_fold = consistency.bed_fold(analyze.load(args.bed, 6).astype(np.float64))
        bed = bed_fold
    header = (f"{'variant':<16} {'chans':>5} {'offset':>7} {'burst rms':>9} {'peak':>6} {'jump':>6} {'hf>6k':>6} {'clean rms':>9} "
              f"{'ncc burst':>9} {'ncc clean':>9} {'ncc win':>7}")
    if bed is not None:
        header += f" {'cons win':>8} {'cons med':>8}"
    print(header)
    for directory in sorted(p for p in args.tag_dir.iterdir() if p.is_dir()):
        pcm = directory / "atmos-916.f32le"
        summary_path = directory / "render-summary.json"
        if not pcm.exists() or not summary_path.exists():
            continue
        summary = json.loads(summary_path.read_text())
        channels = summary["channels"]
        names = summary.get("lab", {}).get("channel_names") or analyze.NAMES
        render = analyze.load(pcm, channels)
        fold = analyze.fold_down(render, names)
        offset = args.offset if args.offset is not None else analyze.find_offset(fold, ref)
        rows = analyze.bin_metrics(render, fold, ref, offset, *args.window, names=names)
        s = analyze.summarise(rows, burst=tuple(args.burst))
        line = (f"{directory.name:<16} {channels:>5} {offset:>7} {s['burst_front_rms']:9.5f} {s['burst_front_peak']:6.3f} "
                f"{s['burst_front_max_jump']:6.3f} {s['burst_front_hf_share_mean']:6.3f} {s['clean_front_rms']:9.5f} "
                f"{s.get('burst_ncc_mean', 0):9.3f} {s.get('clean_ncc_mean', 0):9.3f} {s.get('window_ncc_mean', 0):7.3f}")
        if bed is not None:
            n = min(len(fold) - args.lag, len(bed))
            ours = fold[args.lag:args.lag + n]
            theirs = bed[:n]
            wa, wb = int(args.window[0] * analyze.RATE), int(args.window[1] * analyze.RATE)
            win = 0.5 * (analyze.ncc(ours[wa:wb, 0], theirs[wa:wb, 0]) + analyze.ncc(ours[wa:wb, 1], theirs[wa:wb, 1]))
            per_second = [0.5 * (analyze.ncc(ours[i * analyze.RATE:(i + 1) * analyze.RATE, 0], theirs[i * analyze.RATE:(i + 1) * analyze.RATE, 0])
                                 + analyze.ncc(ours[i * analyze.RATE:(i + 1) * analyze.RATE, 1], theirs[i * analyze.RATE:(i + 1) * analyze.RATE, 1]))
                          for i in range(n // analyze.RATE)]
            line += f" {win:8.3f} {float(np.median(per_second)):8.3f}"
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
