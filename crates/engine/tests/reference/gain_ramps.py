"""The gain ramp of amui/output.py (`_apply_gain`) in bits: what numpy makes of a block of 0.5.

`output::tests::gain_ramps_are_numpy_s` (crates/engine/src/output.rs) holds what this prints: frames
0, 1, 240 and 479 of a block whose samples are all 0.5, after the ramp from the first gain to the
second.

    python3 crates/engine/tests/reference/gain_ramps.py
"""
from __future__ import annotations

import struct

import numpy as np

BLOCK_FRAMES = 480


def bits(value) -> str:
    return "0x%08x" % struct.unpack("<I", struct.pack("<f", float(value)))[0]


for start, end in ((0.0, 0.8), (0.5, 0.15), (1.0, 0.0), (0.3, 0.30000000000000004), (0.0, 5e-324)):
    block = np.full(BLOCK_FRAMES, 0.5, dtype=np.float32)
    block *= np.linspace(start, end, BLOCK_FRAMES, endpoint=False, dtype=np.float32)
    print(f"{start!r} -> {end!r}: [{', '.join(bits(block[frame]) for frame in (0, 1, 240, 479))}]")
