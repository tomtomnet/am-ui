#!/usr/bin/env python3
"""Measure the 0:05 Atmos artifact in a 16-channel Cavern render and compare it with the Apple reference.

Inputs are raw little-endian float32 files at 48 kHz:
  render   16-ch interleaved, channel order FL FR FC LFE SL SR RL RR FLW FRW TFL TFR TSL TSR TRL TRR
  ref      2-ch interleaved (Apple's own renderer through the PEQdB fold-down + headphone EQ)

Metrics (per 100 ms bin inside the analysis window):
  FL/FR burst metrics: RMS, peak, crest factor, share of energy above 6 kHz, max sample-to-sample jump
  fold-down vs reference: normalised cross-correlation after alignment (PEQdB matrix, LFE omitted)
  spectral flatness of FL+FR

Usage:
  analyze.py RENDER [--ref REF] [--window 4.5 7.9] [--channels 16] [--json OUT]
             [--excerpt OUT.wav START END] [--excerpt-channels fold|front] [--lag N]
"""
from __future__ import annotations

import argparse
import json
import math
import struct
import sys
from pathlib import Path

import numpy as np

RATE = 48000
NAMES = "FL FR FC LFE SL SR RL RR FLW FRW TFL TFR TSL TSR TRL TRR".split()
# PEQdB fold-down (workspace tools/downmix.py): every side channel 1.0, centre -3 dB into both sides.
# The LFE path (+7.1 dB after a 125 Hz low-pass) is omitted: it carries nothing above 125 Hz.
LEFT = {"FL": 1.0, "FC": 0.70710678, "SL": 1.0, "RL": 1.0, "FLW": 1.0, "TFL": 1.0, "TSL": 1.0, "TRL": 1.0}
RIGHT = {"FR": 1.0, "FC": 0.70710678, "SR": 1.0, "RR": 1.0, "FRW": 1.0, "TFR": 1.0, "TSR": 1.0, "TRR": 1.0}


def load(path: Path, channels: int) -> np.ndarray:
    raw = np.memmap(str(path), dtype="<f4", mode="r")
    frames = raw.size // channels
    return raw[: frames * channels].reshape(frames, channels)


def fold_down(x: np.ndarray, names=NAMES) -> np.ndarray:
    """PEQdB matrix generalised by channel name: *L -> left, *R -> right, FC -> both at -3 dB, LFE omitted."""
    out = np.zeros((x.shape[0], 2), dtype=np.float64)
    for index, name in enumerate(names):
        if name == "FC":
            out[:, 0] += 0.70710678 * x[:, index]
            out[:, 1] += 0.70710678 * x[:, index]
        elif name == "LFE":
            continue
        elif name.rstrip("W").endswith("L"):
            out[:, 0] += x[:, index]
        elif name.rstrip("W").endswith("R"):
            out[:, 1] += x[:, index]
        else:
            raise ValueError(f"cannot place channel {name}")
    return out


def find_offset(ours: np.ndarray, ref: np.ndarray, seconds: float = 20.0, max_lag: int = 4 * RATE) -> int:
    """Offset D such that ours[n] ~ ref[n + D] (D > 0: the reference file has D samples of extra lead-in)."""
    n = min(int(seconds * RATE), len(ours), len(ref))
    a = ours[:n].sum(axis=1)
    b = ref[:n].sum(axis=1)
    a = a - a.mean()
    b = b - b.mean()
    size = 1 << int(math.ceil(math.log2(2 * n)))
    fa = np.fft.rfft(a, size)
    fb = np.fft.rfft(b, size)
    xc = np.fft.irfft(fa * np.conj(fb), size)  # xc[k] = sum a[m + k] b[m]
    lags = np.concatenate((np.arange(0, max_lag), np.arange(-max_lag, 0)))  # xc[k] = sum a[m + k] b[m]
    values = np.concatenate((xc[:max_lag], xc[-max_lag:]))
    return -int(lags[np.argmax(values)])


def ncc(a: np.ndarray, b: np.ndarray) -> float:
    a = a - a.mean()
    b = b - b.mean()
    d = math.sqrt(float(a @ a) * float(b @ b))
    return float(a @ b) / d if d > 0 else 0.0


def hf_share(x: np.ndarray, cutoff: float = 6000.0) -> float:
    spectrum = np.abs(np.fft.rfft(x * np.hanning(len(x)))) ** 2
    total = spectrum.sum()
    if total <= 0:
        return 0.0
    freqs = np.fft.rfftfreq(len(x), 1 / RATE)
    return float(spectrum[freqs >= cutoff].sum() / total)


def spectral_flatness(x: np.ndarray) -> float:
    spectrum = np.abs(np.fft.rfft(x * np.hanning(len(x)))) ** 2 + 1e-20
    return float(np.exp(np.mean(np.log(spectrum))) / np.mean(spectrum))


def bin_metrics(render: np.ndarray, fold: np.ndarray, ref: np.ndarray | None, offset: int,
                start: float, end: float, step: float = 0.1, names=NAMES) -> list[dict]:
    """offset = reference index - render index (positive: the reference file starts earlier)."""
    rows = []
    t = start
    fl, fr = names.index("FL"), names.index("FR")
    while t < end - 1e-9:
        a, b = int(round(t * RATE)), int(round((t + step) * RATE))
        if b > len(render):
            break
        front = render[a:b, [fl, fr]].astype(np.float64)
        rms = math.sqrt(float(np.mean(front ** 2)))
        peak = float(np.max(np.abs(front))) if front.size else 0.0
        jumps = np.abs(np.diff(front, axis=0))
        row = {
            "t": round(t, 3),
            "front_rms": rms,
            "front_peak": peak,
            "front_crest_db": 20 * math.log10(peak / rms) if rms > 0 and peak > 0 else None,
            "front_hf_share": hf_share(front.sum(axis=1)) if rms > 0 else 0.0,
            "front_max_jump": float(jumps.max()) if jumps.size else 0.0,
            "front_flatness": spectral_flatness(front.sum(axis=1)) if rms > 0 else 0.0,
            "fold_rms": math.sqrt(float(np.mean(fold[a:b] ** 2))),
        }
        if ref is not None:
            ra, rb = a + offset, b + offset
            if 0 <= ra and rb <= len(ref):
                row["ncc_left"] = ncc(fold[a:b, 0], ref[ra:rb, 0].astype(np.float64))
                row["ncc_right"] = ncc(fold[a:b, 1], ref[ra:rb, 1].astype(np.float64))
                row["ncc"] = 0.5 * (row["ncc_left"] + row["ncc_right"])
        rows.append(row)
        t += step
    return rows


def summarise(rows: list[dict], burst=(5.55, 6.75), clean=(4.0, 5.5)) -> dict:
    def pick(lo, hi):
        return [r for r in rows if lo - 1e-9 <= r["t"] < hi - 1e-9]

    b, c = pick(*burst), pick(*clean)
    out = {
        "burst_window": list(burst),
        "burst_front_rms": math.sqrt(sum(r["front_rms"] ** 2 for r in b) / len(b)) if b else None,
        "burst_front_peak": max((r["front_peak"] for r in b), default=None),
        "burst_front_hf_share_mean": sum(r["front_hf_share"] for r in b) / len(b) if b else None,
        "burst_front_max_jump": max((r["front_max_jump"] for r in b), default=None),
        "clean_front_rms": math.sqrt(sum(r["front_rms"] ** 2 for r in c) / len(c)) if c else None,
    }
    if b and "ncc" in b[0]:
        out["burst_ncc_mean"] = sum(r["ncc"] for r in b) / len(b)
        out["burst_ncc_min"] = min(r["ncc"] for r in b)
        out["clean_ncc_mean"] = sum(r["ncc"] for r in c) / len(c) if c else None
        out["window_ncc_mean"] = sum(r["ncc"] for r in rows) / len(rows)
    return out


def write_wav(path: Path, data: np.ndarray, rate: int = RATE) -> None:
    """32-bit float WAV."""
    pcm = np.ascontiguousarray(data, dtype="<f4")
    channels = pcm.shape[1]
    payload = pcm.tobytes()
    header = b"RIFF" + struct.pack("<I", 36 + len(payload)) + b"WAVE"
    header += b"fmt " + struct.pack("<IHHIIHH", 16, 3, channels, rate, rate * channels * 4, channels * 4, 32)
    header += b"data" + struct.pack("<I", len(payload))
    path.write_bytes(header + payload)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("render", type=Path)
    parser.add_argument("--channels", type=int, default=16)
    parser.add_argument("--names", help="space-separated channel names when not the 16-channel route order")
    parser.add_argument("--ref", type=Path)
    parser.add_argument("--offset", type=int, help="reference index minus render index, in samples; measured if omitted")
    parser.add_argument("--window", type=float, nargs=2, default=(4.5, 7.9))
    parser.add_argument("--json", type=Path)
    parser.add_argument("--excerpt", nargs=3, metavar=("OUT", "START", "END"))
    parser.add_argument("--excerpt-channels", choices=("fold", "front"), default="fold")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)

    names = args.names.split() if args.names else NAMES
    if len(names) != args.channels:
        raise SystemExit("--names must list one name per channel")
    render = load(args.render, args.channels)
    fold = fold_down(render, names)
    ref = load(args.ref, 2) if args.ref else None
    offset = args.offset
    if ref is not None and offset is None:
        offset = find_offset(fold, ref)
    rows = bin_metrics(render, fold, ref, offset or 0, *args.window, names=names)
    summary = summarise(rows)
    summary["reference_offset_samples"] = offset
    summary["render"] = str(args.render)
    if not args.quiet:
        print(f"render {args.render} ({len(render) / RATE:.3f} s), reference offset {offset} samples")
        header = f"{'t':>6} {'FL/FR rms':>10} {'peak':>7} {'crest':>6} {'hf>6k':>6} {'jump':>6} {'flat':>6}"
        if ref is not None:
            header += f" {'ncc':>6}"
        print(header)
        for r in rows:
            line = (f"{r['t']:6.2f} {r['front_rms']:10.5f} {r['front_peak']:7.4f} "
                    f"{(r['front_crest_db'] or 0):6.1f} {r['front_hf_share']:6.2f} {r['front_max_jump']:6.3f} "
                    f"{r['front_flatness']:6.3f}")
            if "ncc" in r:
                line += f" {r['ncc']:6.3f}"
            print(line)
        print(json.dumps(summary, indent=1))
    if args.json:
        args.json.write_text(json.dumps({"summary": summary, "bins": rows}, indent=1))
    if args.excerpt:
        out, start, end = Path(args.excerpt[0]), float(args.excerpt[1]), float(args.excerpt[2])
        a, b = int(start * RATE), int(end * RATE)
        if args.excerpt_channels == "fold":
            data = fold[a:b]
        else:
            data = render[a:b, [names.index("FL"), names.index("FR")]]
        write_wav(out, data)
        if not args.quiet:
            print(f"wrote {out} ({(b - a) / RATE:.2f} s, {args.excerpt_channels})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
