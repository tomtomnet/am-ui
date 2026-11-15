#!/usr/bin/env python3
"""Per-object view of an instrumented render (probe --log-joc --log-objects --dump-objects).

Shows which renderer object carries the FL/FR burst, where the OAMD places it, and what its JOC
parameters/mixing matrices look like frame by frame.

Usage: objects_report.py INSTRUMENTED_DIR [--burst 5.55 6.75] [--objects 3,7] [--matrices]
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np

RATE = 48000


def ncc(a, b):
    a = a - a.mean()
    b = b - b.mean()
    d = math.sqrt(float(a @ a) * float(b @ b))
    return float(a @ b) / d if d > 0 else 0.0


def hf_share(x, cutoff=6000.0):
    spectrum = np.abs(np.fft.rfft(x * np.hanning(len(x)))) ** 2
    total = spectrum.sum()
    if total <= 0:
        return 0.0
    return float(spectrum[np.fft.rfftfreq(len(x), 1 / RATE) >= cutoff].sum() / total)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--burst", type=float, nargs=2, default=(5.55, 6.75))
    parser.add_argument("--objects", help="comma-separated object indexes for the time series (default: the 3 most front-correlated)")
    parser.add_argument("--matrices", action="store_true", help="print the mixing matrices of the selected objects")
    parser.add_argument("--step", type=float, default=0.1)
    args = parser.parse_args(argv)
    d = args.directory
    meta = json.loads((d / "objects-dump.json").read_text())
    n, start, frames = meta["channels"], meta["start_sample"], meta["frames"]
    objects = np.memmap(d / "objects.f32le", dtype="<f4", mode="r").reshape(frames, n)
    render = np.memmap(d / "atmos-916.f32le", dtype="<f4", mode="r")
    render = render.reshape(render.size // 16, 16)[start:start + frames]
    t0 = start / RATE

    # positions: one log entry per step
    positions = {}
    with open(d / "objects-log.jsonl") as f:
        for line in f:
            e = json.loads(line)
            key = round(e["out_time"] / args.step)
            positions.setdefault(key, e)

    # burst-window summary per object
    a = max(0, int(round((args.burst[0] - t0) * RATE)))
    b = min(frames, int(round((args.burst[1] - t0) * RATE)))
    seg = objects[a:b].astype(np.float64)
    fl = render[a:b, 0].astype(np.float64)
    fr = render[a:b, 1].astype(np.float64)
    print(f"burst window {args.burst[0]}-{args.burst[1]} s: render FL rms {math.sqrt((fl ** 2).mean()):.5f}, FR rms {math.sqrt((fr ** 2).mean()):.5f}")
    print(f"{'obj':>3} {'rms':>8} {'peak':>7} {'crest':>6} {'hf>6k':>6} {'ncc FL':>7} {'ncc FR':>7}  mean position (x right, y up, z front)  vol   lfe")
    rows = []
    keys = [k for k in positions if args.burst[0] <= positions[k]["out_time"] < args.burst[1]]
    for obj in range(n):
        x = seg[:, obj]
        rms = math.sqrt((x ** 2).mean())
        peak = float(np.abs(x).max()) if x.size else 0.0
        pos = np.array([positions[k]["objects"][obj]["pos"] for k in keys]) if keys else np.zeros((1, 3))
        vol = np.mean([positions[k]["objects"][obj]["vol"] for k in keys]) if keys else float("nan")
        lfe = positions[keys[0]]["objects"][obj]["lfe"] if keys else None
        rows.append((obj, rms, peak, 20 * math.log10(peak / rms) if rms > 0 else 0.0, hf_share(x) if rms > 0 else 0.0,
                     ncc(x, fl), ncc(x, fr), pos.mean(axis=0), pos.std(axis=0), vol, lfe))
    for r in rows:
        print(f"{r[0]:3d} {r[1]:8.5f} {r[2]:7.4f} {r[3]:6.1f} {r[4]:6.2f} {r[5]:7.3f} {r[6]:7.3f}  "
              f"({r[7][0]:6.2f},{r[7][1]:5.2f},{r[7][2]:6.2f}) +-({r[8][0]:.2f},{r[8][1]:.2f},{r[8][2]:.2f}) {r[9]:5.3f} {r[10]}")

    if args.objects:
        selected = [int(s) for s in args.objects.split(",")]
    else:
        selected = [r[0] for r in sorted(rows, key=lambda r: -max(abs(r[5]), abs(r[6])))[:3]]
    print(f"\ntime series for objects {selected} (per {args.step} s): rms, hf>6k, position; render FL/FR rms")
    step = int(args.step * RATE)
    for i in range(frames // step):
        a, b = i * step, (i + 1) * step
        t = t0 + a / RATE
        key = round(t / args.step)
        line = f"{t:6.2f} FL/FR {math.sqrt((render[a:b, :2].astype(np.float64) ** 2).mean()):.5f} |"
        for obj in selected:
            x = objects[a:b, obj].astype(np.float64)
            rms = math.sqrt((x ** 2).mean())
            p = positions[key]["objects"][obj] if key in positions else None
            ps = f"({p['pos'][0]:6.2f},{p['pos'][1]:5.2f},{p['pos'][2]:6.2f}) v{p['vol']:.3f}" if p else "(no log)"
            line += f" o{obj}: {rms:.5f} hf{hf_share(x) if rms > 0 else 0:.2f} {ps} |"
        print(line)

    # JOC parameters of the selected objects per frame. Renderer object k is JOC object k-1 when the LFE bed sits
    # at renderer slot 0 (objects-dump.json: lfe_object); the LFE slot itself has no JOC data.
    lfe_slot = meta.get("lfe_object", -1)
    def joc_index(renderer_object):
        if renderer_object == lfe_slot:
            return None
        return renderer_object - 1 if 0 <= lfe_slot < renderer_object else renderer_object
    selected_joc = [(obj, joc_index(obj)) for obj in selected if joc_index(obj) is not None]
    print(f"\nJOC parameters per frame for renderer objects {selected} = JOC objects {[j for _, j in selected_joc]}")
    with open(d / "joc-log.jsonl") as f:
        entries = [json.loads(line) for line in f]
    for e in entries:
        if e["kind"] != "frame":
            continue
        parts = []
        for obj, joc in selected_joc:
            o = e["objects"][joc]
            parts.append(f"o{obj}: act={int(o['active'])} sparse={int(o['sparse'])} bands={o['bands']} q={o['quant']} dp={o['data_points']} "
                         f"steep={int(o['steep'])} off={o['offsets']}")
        print(f"frame {e['frame']} t={e['out_time']:.3f} ts_after={e['applier_timeslot_after']} gain={e['gain']:.3f} | " + " | ".join(parts))
    if args.matrices:
        print(f"\nmixing matrices (rows: downmix channel L R C Ls Rs; columns: parameter bands) for renderer objects {selected}")
        wanted = {joc: obj for obj, joc in selected_joc}
        for e in entries:
            if e["kind"] != "matrices":
                continue
            for o in e["objects"]:
                if o["obj"] in wanted:
                    for dp, matrix in enumerate(o["dp"]):
                        flat = [v for ch in matrix for v in ch]
                        print(f"t={e['out_time']:.3f} frame {e['frame']} renderer o{wanted[o['obj']]} (JOC {o['obj']}) dp{dp} max|m|={max(abs(v) for v in flat):.3f}")
                        for ch, row in zip("L R C Ls Rs Lrs Rrs".split(), matrix):
                            print(f"    {ch:>3} " + " ".join(f"{v:6.3f}" for v in row))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
