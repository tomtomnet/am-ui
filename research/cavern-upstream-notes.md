# Cavern: findings worth reporting upstream (drafts)

Drafts for the user to post, if they wish, to https://github.com/VoidXH/Cavern. All measurements come
from `research/cavern-artifact.md` (am-ui, 2026-11-08) on two commercial Apple Music E-AC-3 JOC streams
(768 kbps, 5.1 core, 15 JOC objects); the patched sources live in `tools/cavern-lab/cavern-src` as
switch-gated changes (`--surround-phase`, `--joc-timeslot-fix`, `--joc-interp-fix` in the lab probe).
Nothing has been posted; posting is an outward-facing action the project owner decides on.

## 1. Comment for issue #423 (`joc_dmx_config_idx` is only used to pick the channel count)

> Measured on two Apple Music JOC streams coded with `joc_dmx_config_idx = 3` ("5.X with 90-degree phase
> shift", ETSI TS 103 420 table 47): the cross-spectral phase between the core's L and Ls, and between R and
> Rs, is a steady −90° at every frequency (L↔R and Ls↔Rs are at 0°), i.e. the surround channels of the core
> downmix are the quadrature-shifted surround mix. The current decoder reads the index only to choose 5 or 7
> downmix channels, so JOC reconstructions that subtract surround from front (here an object pair placed at the
> front corners, reconstructed as ≈ 0.9·L − 0.9·Ls / 0.9·R − 0.9·Rs) cannot cancel the rest of the mix and a
> quadrature residue of the whole mix lands in the front outputs, gated band by band at the frame rate. It is
> audible as a 1–2 s burst of distortion in one song and lowers waveform correlation with a reference
> (Dolby/Apple) render to ≈0.75 over whole songs.
>
> Multiplying the Ls/Rs QMF samples by −j before applying the JOC mixing matrices (only when the index is 3 or
> 4) removes the artefact: correlation with the reference render goes from 0.77 to 0.987 in the burst passage
> and to 0.98 over both whole songs; +j instead collapses to 0.29, so the sign is unambiguous. The bed-only
> decode (no JOC) is clean, which confirms the fault is in the reconstruction step.

## 2. New issue: JOC applier timeslot counter wraps at the channel count

> In `JointObjectCodingApplier.Apply` the `timeslot` counter wraps at `input.Length` (the number of downmix
> channels, 7 for a 5.1 core with LFE) instead of at the number of QMF timeslots per frame (`frameSize / 64`
> = 24 for 1536-sample frames). Consequences: `GetMixingMatrices` is recomputed every 7 timeslots, only the
> first 7 of the 24 interpolated timeslots are ever used, and the 7-timeslot cycle is out of phase with the
> frame start in 6 of 7 frames (4263 of 4974 frames in our test stream). Wrapping at the frame length instead
> keeps the interpolated matrices aligned with the frame; on our streams the reference-free consistency of the
> reconstruction against the core improves slightly (per-second correlation with a reference render 0.770 →
> 0.795 on the affected passage) and nothing gets worse.

## 3. New issue (or the same one): interpolation branches deviate from TS 103 420 clause 6.6.5

> Compared with Pseudocode 6 of clause 6.6.5 (V1.2.1, pages 63–64):
> * steep slope, one data point: `mixMatrix[ts < timeslotOffsets[obj][1] ? 1 : 0]` consults the second offset
>   where the spec uses the first;
> * steep slope, two data points: the second data point is never used and the timeslot comparison is off by
>   one.
> Neither branch is exercised by the two streams measured here (no frame used two data points or a steep
> slope), so the spec-faithful rewrite (`InterpolateSpec` in the patched `JointObjectCodingDecoder.cs`) is
> verified against the text only, not against audio. Reported for completeness.

## Reproduction material the owner may share

* The measurement scripts (`tools/cavern-lab/analyze.py`, `objects_report.py`, `consistency.py`) are ours and
  can be shared; the Apple Music streams and the reference recordings cannot (licensed content), so a report
  should offer the numbers above and the method, not the files.
* Note: one Cavern source file (`ObjectAudioElementMetadata.cs`) carries a header forbidding AI agents from
  working on it. It was left untouched; a maintainer-facing report should mention that the proposed changes
  are confined to `JointObjectCodingApplier.cs` / `JointObjectCodingDecoder.cs` and the QMF input stage.
