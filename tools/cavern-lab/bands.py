#!/usr/bin/env python3
"""Band table and waveform correlation of a 16-channel render against a stereo reference.

Both signals go through the PEQdB fold-down (every side channel 1.0, centre -3 dB, LFE x2.26464 after a
4th-order 125 Hz Butterworth low-pass with the matching delay on the other channels), then:
  * 1/3-octave band levels (dB) of ours, of the reference, and reference minus ours;
  * per-second normalised cross-correlation of the folds, split below/above a crossover (default 150 Hz);
  * per-second best lag (to detect clock drift between the two recordings).

Usage: bands.py RENDER --ref REF [--offset N] [--channels 16] [--names "..."] [--split 150]
                [--bass-manage GAIN FREQ]   (emulate: LFE += GAIN * LP4(FREQ, sum of non-LFE channels))
                [--lfe-gain G] [--range START END] [--json OUT]
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import analyze  # noqa: E402

RATE = 48000
THIRD_OCTAVE = [25, 31.5, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600,
                2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000]
BW4_Q = (1 / (2 * math.cos(math.pi / 8)), 1 / (2 * math.cos(3 * math.pi / 8)))


def biquad_lowpass_response(freqs: np.ndarray, f0: float, q: float) -> np.ndarray:
    """RBJ cookbook low-pass biquad H(e^jw) on the given frequency grid."""
    w0 = 2 * math.pi * f0 / RATE
    alpha = math.sin(w0) / (2 * q)
    b0 = (1 - math.cos(w0)) / 2
    b1 = 1 - math.cos(w0)
    b2 = b0
    a0 = 1 + alpha
    a1 = -2 * math.cos(w0)
    a2 = 1 - alpha
    z = np.exp(-1j * 2 * math.pi * freqs / RATE)
    return (b0 + b1 * z + b2 * z ** 2) / (a0 + a1 * z + a2 * z ** 2)


LOWPASS_TYPES = {"bw4": BW4_Q, "bw2": (1 / math.sqrt(2),), "lr4": (1 / math.sqrt(2), 1 / math.sqrt(2)), "bw1": None}


def butterworth4_lowpass(x: np.ndarray, f0: float, kind: str = "bw4") -> np.ndarray:
    """Zero-padding FFT application of cascaded RBJ biquads: bw4 (PipeWire's 4th-order Butterworth), bw2, lr4
    (Linkwitz-Riley 4th order) or bw1 (first-order RC)."""
    size = 1 << int(math.ceil(math.log2(len(x) + RATE)))
    spectrum = np.fft.rfft(x, size)
    freqs = np.fft.rfftfreq(size, 1 / RATE)
    qs = LOWPASS_TYPES[kind]
    if qs is None:
        response = 1 / (1 + 1j * freqs / f0)
    else:
        response = np.ones_like(freqs, dtype=complex)
        for q in qs:
            response = response * biquad_lowpass_response(freqs, f0, q)
    return np.fft.irfft(spectrum * response, size)[: len(x)]


def peqdb_fold(x: np.ndarray, names: list[str], lfe_gain: float = 1.0, bass_manage: tuple[float, float] | None = None,
               bass_type: str = "bw4") -> np.ndarray:
    lfe = x[:, names.index("LFE")].astype(np.float64) * lfe_gain
    others = analyze.fold_down(x, names)  # LFE omitted there
    if bass_manage is not None:
        gain, f0 = bass_manage
        total = np.zeros(len(x), dtype=np.float64)
        for index, name in enumerate(names):
            if name != "LFE":
                total += x[:, index]
        lfe = lfe + gain * butterworth4_lowpass(total, f0, bass_type)
    lfe_path = 2.26464 * butterworth4_lowpass(lfe, 125.0)
    delay = int(round((sum(1 / q for q in BW4_Q) / (2 * math.pi * 125.0) + 0.5 / RATE) * RATE))
    out = np.zeros_like(others)
    out[delay:] = others[: len(others) - delay]
    out[:, 0] += lfe_path
    out[:, 1] += lfe_path
    return out


def band_levels(x: np.ndarray, seconds: float = 1.0) -> np.ndarray:
    """Average power spectrum over 1-s Hann blocks, integrated per 1/3-octave band, in dB."""
    block = int(seconds * RATE)
    count = len(x) // block
    window = np.hanning(block)
    power = np.zeros(block // 2 + 1)
    for i in range(count):
        seg = x[i * block:(i + 1) * block]
        power += np.abs(np.fft.rfft(seg * window)) ** 2
    power /= max(count, 1)
    freqs = np.fft.rfftfreq(block, 1 / RATE)
    levels = []
    for fc in THIRD_OCTAVE:
        lo, hi = fc / 2 ** (1 / 6), fc * 2 ** (1 / 6)
        m = (freqs >= lo) & (freqs < hi)
        levels.append(10 * math.log10(power[m].sum() + 1e-30))
    return np.array(levels)


def split_bands(x: np.ndarray, split: float) -> tuple[np.ndarray, np.ndarray]:
    size = 1 << int(math.ceil(math.log2(len(x) + 1)))
    spectrum = np.fft.rfft(x, size)
    freqs = np.fft.rfftfreq(size, 1 / RATE)
    low_mask = 0.5 * (1 - np.tanh((freqs - split) / (0.1 * split)))  # soft crossover
    low = np.fft.irfft(spectrum * low_mask, size)[: len(x)]
    high = np.fft.irfft(spectrum * (1 - low_mask), size)[: len(x)]
    return low, high


def per_second(ours: np.ndarray, ref: np.ndarray, offset: int, split: float, start: int, end: int):
    rows = []
    ours_mono = ours.sum(axis=1)
    ref_mono = ref.sum(axis=1)
    o_low, o_high = split_bands(ours_mono[start:end], split)
    r_low, r_high = split_bands(ref_mono[start + offset:end + offset], split)
    for i in range((end - start) // RATE):
        a, b = i * RATE, (i + 1) * RATE
        # best lag within +-200 samples of the global offset (drift check), on the full-band mono
        x = ours_mono[start + a:start + b] - ours_mono[start + a:start + b].mean()
        y = ref_mono[start + offset + a - 200:start + offset + b + 200]
        y = y - y.mean()
        c = np.correlate(y, x, mode="valid")  # c[k] compares x with y shifted by k-200
        k = int(np.argmax(c)) - 200
        # the same correlations with the locally best lag applied (drift-corrected)
        ra, rb = start + offset + a + k, start + offset + b + k
        rl_loc, rh_loc = split_bands(ref_mono[ra:rb], split)
        rows.append({"t": (start + a) / RATE, "ncc_low": analyze.ncc(o_low[a:b], r_low[a:b]),
                     "ncc_high": analyze.ncc(o_high[a:b], r_high[a:b]), "ncc_full": analyze.ncc(x, ref_mono[start + offset + a:start + offset + b]),
                     "lag_drift": k, "ncc_low_local": analyze.ncc(o_low[a:b], rl_loc), "ncc_high_local": analyze.ncc(o_high[a:b], rh_loc),
                     "ncc_full_local": analyze.ncc(x, ref_mono[ra:rb])})
    return rows


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("render", type=Path)
    parser.add_argument("--ref", type=Path, required=True)
    parser.add_argument("--offset", type=int)
    parser.add_argument("--channels", type=int, default=16)
    parser.add_argument("--names")
    parser.add_argument("--split", type=float, default=150.0)
    parser.add_argument("--bass-manage", type=float, nargs=2, metavar=("GAIN", "FREQ"))
    parser.add_argument("--bass-type", choices=sorted(LOWPASS_TYPES), default="bw4")
    parser.add_argument("--lfe-gain", type=float, default=1.0)
    parser.add_argument("--range", type=float, nargs=2, default=(1.0, 0.0), help="seconds; end 0 = whole file")
    parser.add_argument("--json", type=Path)
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)
    names = args.names.split() if args.names else analyze.NAMES
    render = analyze.load(args.render, args.channels)
    ref = analyze.load(args.ref, 2)
    ours = peqdb_fold(render, names, args.lfe_gain, tuple(args.bass_manage) if args.bass_manage else None, args.bass_type)
    offset = args.offset if args.offset is not None else analyze.find_offset(analyze.fold_down(render, names), ref)
    start = int(args.range[0] * RATE)
    end = int(args.range[1] * RATE) if args.range[1] > 0 else min(len(ours), len(ref) - offset)
    end = min(end, len(ours), len(ref) - offset)
    end -= (end - start) % RATE
    ours_mono = ours[start:end].sum(axis=1) / 2
    ref_mono = ref[start + offset:end + offset].astype(np.float64).sum(axis=1) / 2
    lo, lr = band_levels(ours_mono), band_levels(ref_mono)
    rows = per_second(ours, ref.astype(np.float64), offset, args.split, start, end)
    med = lambda key: float(np.median([r[key] for r in rows]))
    lfe = render[start:end, names.index("LFE")].astype(np.float64)
    lfe_rms = 20 * math.log10(math.sqrt(float((lfe ** 2).mean())) + 1e-12)
    result = {"render": str(args.render), "offset": offset, "range_seconds": [start / RATE, end / RATE],
              "bands_hz": THIRD_OCTAVE, "ours_db": lo.round(2).tolist(), "ref_db": lr.round(2).tolist(),
              "ref_minus_ours_db": (lr - lo).round(2).tolist(),
              "median_ncc_low": med("ncc_low"), "median_ncc_high": med("ncc_high"), "median_ncc_full": med("ncc_full"),
              "median_ncc_low_local": med("ncc_low_local"), "median_ncc_high_local": med("ncc_high_local"), "median_ncc_full_local": med("ncc_full_local"),
              "lag_drift_first": rows[0]["lag_drift"], "lag_drift_last": rows[-1]["lag_drift"],
              "lag_drift_samples_per_minute": (rows[-1]["lag_drift"] - rows[0]["lag_drift"]) / max((rows[-1]["t"] - rows[0]["t"]) / 60, 1e-9),
              "lfe_rms_dbfs": lfe_rms, "per_second": rows}
    if not args.quiet:
        print(f"{args.render}  offset {offset}  range {start / RATE:.1f}-{end / RATE:.1f} s  LFE rms {lfe_rms:.1f} dBFS")
        print(f"{'Hz':>7} {'ours':>7} {'ref':>7} {'ref-ours':>8}")
        for fc, a, b in zip(THIRD_OCTAVE, lo, lr):
            print(f"{fc:7g} {a:7.1f} {b:7.1f} {b - a:8.1f}")
        print(f"median per-second NCC: <{args.split:g} Hz {result['median_ncc_low']:.3f}, >{args.split:g} Hz {result['median_ncc_high']:.3f}, "
              f"full band {result['median_ncc_full']:.3f}; best-lag drift {rows[0]['lag_drift']} -> {rows[-1]['lag_drift']} samples "
              f"({result['lag_drift_samples_per_minute']:.1f}/min)")
        print(f"drift-corrected (per-second best lag) median NCC: <{args.split:g} Hz {result['median_ncc_low_local']:.3f}, "
              f">{args.split:g} Hz {result['median_ncc_high_local']:.3f}, full band {result['median_ncc_full_local']:.3f}")
        print("per-second lag (first 12 / last 12):", [r["lag_drift"] for r in rows[:12]], "...", [r["lag_drift"] for r in rows[-12:]])
    if args.json:
        args.json.write_text(json.dumps(result, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
