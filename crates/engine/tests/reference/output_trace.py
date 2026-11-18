"""What amui/output.py composes for a fixed script of commands: the reference of the Rust feeder.

No thread, no pipe and no clock are involved. The blocks come from the output's own _fill_block(),
followed by the bookkeeping _after_write() does (count the block, hand out the events that are due,
take the log lines), with nothing in the pipe and the stream delay at its start value. Every block
gives one line: a hash of its samples, the gain and trim it ends with, where the source is read and
what position is reported, the last entry of the segment table, the events that became due and the
lines the block logged. The commands' own log lines follow at the end. Gains are printed as the bits
of their doubles; the trims among them come from the C library's pow().

The Rust test `follows_the_python_feeder_block_by_block` (crates/engine/src/output.rs) runs the same
script and compares its lines with output_trace.txt, which is what this prints:

    python3 crates/engine/tests/reference/output_trace.py > crates/engine/tests/reference/output_trace.txt

Sources are synthetic; their folders carry made-up ids.
"""
from __future__ import annotations

import hashlib
import os
import shutil
import struct
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SCRATCH = Path(tempfile.mkdtemp(prefix="output-trace-"))
os.environ["AMUI_VAR"] = str(SCRATCH / "var")  # nothing of this run goes into the engine's data folder
sys.path.insert(0, str(ROOT))

import numpy as np  # noqa: E402

from amui import output as output_module  # noqa: E402
from amui.output import BLOCK_FRAMES, PipeWireOutput  # noqa: E402

output_module.LOG.set_path(SCRATCH / "playback.log")


def signal(frames: int, channels: int, seed: int) -> np.ndarray:
    """Sample n of the file (frames interleaved) is ((7 n + seed) mod 251) / 251 - 0.5, as float32."""
    index = np.arange(frames * channels, dtype=np.int64)
    return (((index * 7 + seed) % 251) / 251.0 - 0.5).astype("<f4").reshape(frames, channels)


class Source:
    """A PcmSource the script controls: what it announces is independent of what its file holds."""

    def __init__(self, name: str, channels: int, frames: int, *, seed: int, available: int | None = None,
                 done: bool = True, total: int | None | str = "frames", file_frames: int | None = None,
                 missing: bool = False):
        self.pcm_path = SCRATCH / name / ("atmos-916.f32le" if channels == 16 else "stereo.f32le")
        self.pcm_path.parent.mkdir()
        self.content = signal(frames if file_frames is None else file_frames, channels, seed).tobytes()
        if not missing:
            self.appear()
        self.channels = channels
        self.available = frames if available is None else available
        self.is_done = done
        self.total = frames if total == "frames" else total
        self.error = None

    def appear(self) -> None:
        self.pcm_path.write_bytes(self.content)

    def frames_available(self) -> int:
        return self.available

    @property
    def total_frames(self) -> int | None:
        return self.total

    @property
    def done(self) -> bool:
        return self.is_done


class Named:
    """A source that only has a trim."""

    def __init__(self, number: int):
        self.pcm_path = SCRATCH / f"2{number:09}.stereo" / "stereo.f32le"
        self.channels = 2


def bits(value: float) -> str:
    return "%016x" % struct.unpack("<Q", struct.pack("<d", value))[0]


class Trace:
    def __init__(self):
        self.output = PipeWireOutput(command=["true"], log_path=SCRATCH / "pw-cat.log")  # never started
        self.block = np.zeros((BLOCK_FRAMES, 16), dtype=np.float32)
        self.stream = hashlib.sha256()
        self.lines: list[str] = []

    def step(self, count: int = 1, final: bool = False) -> None:
        out = self.output
        for _ in range(count):
            with out._lock:
                out._fill_block(self.block, final=final)
            data = self.block.tobytes()
            self.stream.update(data)
            with out._lock:  # what _after_write() does
                out._frames_written += BLOCK_FRAMES
                audible = out._audible_locked()
                due = []
                while out._events and out._events[0][0] <= audible:
                    due.append(out._events.popleft())
                logged, out._pending_log = out._pending_log, []
                last = out._segments[-1]
                segment = f"{len(out._segments)}:{last.stream_start},{last.frame},{int(last.advancing)},{last.generation}"
            line = (f"{len(self.lines):03} {hashlib.sha256(data).hexdigest()[:12]} gain={bits(out._gain)} "
                    f"trim={bits(out._trim)} cur={out._cursor} pos={out.position_frames()} "
                    f"cf={out.cursor_frame} bf={out.buffered_frames()} buf={int(out.buffering)} "
                    f"under={out.underruns} hand={out.handoffs} seg={segment}")
            if due:
                line += " ev=" + ",".join(f"{kind}:{output_module.source_label(source)}@{frame}"
                                          for frame, kind, source in due)
            if logged:
                line += " log=" + "|".join(logged)
            self.lines.append(line)


def main() -> None:
    trace = Trace()
    out, step = trace.output, trace.step
    a = Source("1000000001.atmos", 16, 9000, seed=1)
    b = Source("1000000002.stereo", 2, 4100, seed=2)
    c = Source("1000000003.atmos", 16, 30000, seed=3, available=1300, done=False)
    d = Source("1000000004.stereo", 2, 900, seed=4)
    e = Source("1000000005.atmos", 16, 1500, seed=5, file_frames=1203)  # shorter than announced
    f = Source("1000000006.atmos", 16, 1000, seed=6)
    g = Source("1000000007.stereo", 2, 500, seed=7, total=None)  # no total: plays what is available
    h = Source("1000000008.atmos", 16, 2000, seed=8, done=False, file_frames=600)  # growing, file behind
    i = Source("1000000009.stereo", 2, 5000, seed=9, available=700, done=False)  # will fail while growing
    j = Source("1000000010.atmos", 16, 1000, seed=10, missing=True)  # complete by its own account: no file
    k = Source("1000000011.stereo", 2, 2000, seed=11, done=False, missing=True)  # growing, no file yet

    step(2)  # nothing to play
    out.seek(100)  # no source: ignored
    out.set_trim(a, -3.0)
    out.set_source(a)
    out.seek(240)  # moves the start of the pending source
    out.set_next_source(b)
    out.set_next_source(b)  # the same again: nothing
    step(3)
    out.set_volume(0.7)
    step(2)
    out.pause()
    out.pause()  # already paused: nothing
    step(3)
    out.resume()
    step(2)
    out.seek(6000)  # fade out, jump, fade in
    step(3)
    out.set_volume(0.25)  # quieter
    step(3)
    out.set_volume(1.7)  # more than everything is everything
    step(1)
    out.set_trim(b, -6.0, 0.05)
    step(3)  # a ends inside a block: b continues in it
    out.set_sound_check(False)
    step(3)
    out.set_sound_check(True)
    step(2)
    out.set_trim(c, float("nan"))  # not a number: no trim
    out.set_next_source(c)
    step(6)  # b ends inside a block: c (growing) continues in it, plays its reserve and starves
    c.available = 6300  # more audio, but less than the resume threshold: still waiting
    step(2)
    c.available = 27000  # enough: fade in
    step(4)
    out.set_trim(c, -5.0, 0.2)  # a refined measurement while the source plays: 20 blocks
    step(6)
    out.set_trim(c, 4.0, float("inf"))  # a boost is no trim; an endless ramp is the usual one
    step(3)
    out.seek(9999)  # dropped by the source change that follows
    out.set_source(d, 100)  # while c plays: fade out, then d from frame 100
    out.set_trim(d, -2.5, 0.004)  # a ramp shorter than a block is one block
    step(5)  # d ends, nothing follows
    out.set_source(e)  # announces 1500 frames, the file holds 1203
    step(5)
    out.set_source(f)
    step(1)
    f.available = 800  # a complete file that shrinks
    step(3)
    out.set_source(g)
    out.set_next_source(h)
    step(4)  # g ends, h starts and starves at the end of its file
    h.is_done = True  # done with less than announced: ends at the file's end
    step(3)
    out.set_source(i, 100)
    step(2)  # too little for a first block: waits
    i.error = "render failed"  # an error ends a growing source at what it has
    step(3)
    out.set_source(j)  # nothing to read: ends at once
    step(2)
    out.set_source(k)
    step(2)  # nothing to read yet: waits
    k.appear()
    step(1)  # too little to resume
    k.is_done = True
    step(6)
    out.set_source(None)
    step(2)
    out.set_trim(c, -4.0)  # c keeps its place among the trims
    for number in range(30):  # 34 trims: the two oldest (a, b) are dropped
        out.set_trim(Named(number), -1.0)
    out.set_source(a, 8000)
    out.set_next_source(c)
    step(2)  # a without its trim
    out.seek(8900)  # a ends in the block that fades out for the jump: no continuation in that block
    step(3)  # the last frames of a once more, then c from frame 0 with -4 dB
    out.pause()
    step(2)
    out.resume()
    step(2)
    step(1, final=True)  # the fade stop() ends with

    print("\n".join(trace.lines))
    print("stream " + trace.stream.hexdigest())
    for line in (SCRATCH / "playback.log").read_text().splitlines():
        print("log " + line.split(" ", 1)[1])
    shutil.rmtree(SCRATCH, ignore_errors=True)


if __name__ == "__main__":
    main()
