"""The numbers amui/loudness.py reports for signals the Rust tests rebuild bit for bit.

`loudness::tests::matches_the_python_implementation` (crates/amui/src/loudness.rs) holds what this
prints. The signals use integer steps and exact float operations only (a 64-bit linear congruential
generator, Paul Kellet's pinking filter, one square root), so both languages produce the same samples:

    python3 crates/amui/tests/reference/loudness_values.py
"""
from __future__ import annotations

import math
import os
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SCRATCH = Path(tempfile.mkdtemp(prefix="loudness-values-"))
os.environ["AMUI_VAR"] = str(SCRATCH / "var")  # nothing of this run goes into the engine's data folder
sys.path.insert(0, str(ROOT))

import numpy as np  # noqa: E402

from amui import config, loudness  # noqa: E402

RATE = config.SAMPLE_RATE
MASK = (1 << 64) - 1


def uniform(seed: int, channel: int):
    """Uniform numbers in [-0.5, 0.5) (Knuth's MMIX generator)."""
    state = ((seed + 1) * 0x9E3779B97F4A7C15 + channel * 0xBF58476D1CE4E5B9) & MASK
    while True:
        state = (state * 6364136223846793005 + 1442695040888963407) & MASK
        yield (state >> 11) / 9007199254740992.0 - 0.5


def pinkish_noise(seconds: float, seed: int) -> np.ndarray:
    """Stereo noise with a pink spectrum, both channels scaled to an RMS of 0.05."""
    frames = int(seconds * RATE)
    out = [0.0] * (2 * frames)
    for channel in range(2):
        white_noise = uniform(seed, channel)
        b0 = b1 = b2 = b3 = b4 = b5 = b6 = 0.0
        for frame in range(frames):
            white = next(white_noise)
            b0 = 0.99886 * b0 + white * 0.0555179
            b1 = 0.99332 * b1 + white * 0.0750759
            b2 = 0.96900 * b2 + white * 0.1538520
            b3 = 0.86650 * b3 + white * 0.3104856
            b4 = 0.55000 * b4 + white * 0.5329522
            b5 = -0.7616 * b5 - white * 0.0168980
            out[2 * frame + channel] = b0 + b1 + b2 + b3 + b4 + b5 + b6 + white * 0.5362
            b6 = white * 0.115926
    total = 0.0
    for value in out:
        total += value * value
    scale = 0.05 / math.sqrt(total / len(out))
    return np.array([value * scale for value in out], dtype=np.float64).reshape(frames, 2)


def reference_loudness(signal: np.ndarray) -> float:
    """tests/test_loudness.py: sample-by-sample biquads and literal 400 ms / 100 ms gating blocks."""
    weighted = np.stack([loudness.biquad(loudness.biquad(signal[:, c], loudness._STAGE1), loudness._STAGE2)
                         for c in range(signal.shape[1])], axis=1)
    squares = np.sum(weighted ** 2, axis=1)
    hop, block = int(0.1 * RATE), int(0.4 * RATE)
    powers = np.array([squares[i:i + block].mean() for i in range(0, len(squares) - block + 1, hop)])
    levels = -0.691 + 10 * np.log10(powers)
    above = powers[levels > -70]
    gate = -0.691 + 10 * np.log10(above.mean()) - 10
    gated = powers[(levels > -70) & (levels > gate)]
    return float(-0.691 + 10 * np.log10(gated.mean()))


def main() -> None:
    noise = pinkish_noise(6.0, 7)
    print("samples", repr(float(noise[0, 0])), repr(float(noise[0, 1])), repr(float(noise[287999, 1])))
    print("stereo", repr(loudness.integrated_loudness(noise)))
    gapped = noise.copy()
    gapped[int(2 * RATE):int(2.5 * RATE)] *= 1e-4
    print("with a gated gap", repr(loudness.integrated_loudness(gapped)))
    print("left channel alone", repr(loudness.integrated_loudness(noise[:, 0])))
    short = pinkish_noise(3.0, 1)
    print("3 s, seed 1: meter", repr(loudness.integrated_loudness(short)), "reference", repr(reference_loudness(short)))
    meter = loudness.Meter()
    for start in range(0, len(noise), 70001):
        meter.feed(noise[start:start + 70001])
    print("fed in chunks", repr(meter.integrated()), "sub-blocks", len(meter.sub_energies),
          repr(meter.sub_energies[0]), repr(meter.sub_energies[-1]), "peak", repr(meter.peak))

    wide = np.zeros((len(noise), 16), dtype=np.float32)  # tests/test_loudness.py: rotated copies at their own level
    for channel in range(16):
        wide[:, channel] = np.roll(noise[:, channel % 2], 97 * channel) * (0.3 + 0.05 * channel)
    wide_path = SCRATCH / "atmos-916.f32le"
    wide.tofile(wide_path)
    # the Rust engine folds FLW to the left, where the downmix sink plays it; the Python module's own
    # matrix put it on the right, so the fold is handed in
    fold = loudness.fold_matrix()
    fold[config.CHANNEL_MAP.index("FLW")] = (1.0, 0.0)
    result = loudness.measure_file(wide_path, 16, fold=fold)
    print("16 channels", repr(result["lufs"]), "peak", repr(result["peak"]), result["frames"], result["fold"],
          "trim", repr(loudness.trim_db(result["lufs"])))
    first = loudness.measure_file(wide_path, 16, frames=100000, fold=fold)
    print("16 channels, first 100000 frames", repr(first["lufs"]), first["frames"])
    stereo_path = SCRATCH / "stereo.f32le"
    noise.astype("<f4").tofile(stereo_path)
    result = loudness.measure_file(stereo_path, 2)
    print("stereo file", repr(result["lufs"]), "peak", repr(result["peak"]), result["frames"], result["fold"])

    response = loudness.k_weighting_ir()
    print("impulse response", repr(float(response[0])), repr(float(response[1])), repr(float(response[8191])))
    print(loudness.pan_expression(fold))
    for lufs in (-13.004999, -12.345, -15.995, -16.004, -3.25, float("inf")):
        print("trim for", lufs, repr(loudness.trim_db(lufs)))
    shutil.rmtree(SCRATCH, ignore_errors=True)


if __name__ == "__main__":
    main()
