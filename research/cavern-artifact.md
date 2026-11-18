# Cavern Atmos render: the 0:05 distortion in song A, and render fidelity against Apple's renderer

Date: 2026-11-08. Scope: the deterministic artefact the user hears at 0:05 of song A (an Atmos single) in Atmos mode, plus the follow-up questions on bass level, JOC fidelity and "decorrelation" raised
on song B (another Atmos single). Everything here was measured with the tools in `tools/cavern-lab/` on this machine;
no private media is in the repo (renders and excerpts live under `var/tmp/cavern-lab/`).

## Summary

* **Root cause of the distortion: Cavern ignores the 90-degree surround phase shift of the JOC downmix.** Both
  songs are coded with `joc_dmx_config_idx = 3` ("5.X with 90 degree phase shift", ETSI TS 103 420 table 47):
  the E-AC-3 core's Ls/Rs are the surround downmix shifted by 90 degrees (measured: a steady -90 degree
  cross-spectral phase between L and Ls, and between R and Rs, at every frequency, while L/R and Ls/Rs sit at
  0 degrees). Cavern reads the index only to pick 5 or 7 channels (open upstream issue #423). At 0:05 the loud
  front L/R object pair is reconstructed by the encoder's matrices as roughly `0.9*L - 0.9*Ls` and
  `0.9*R - 0.9*Rs`; with the phase-shifted surrounds the subtraction cannot cancel the rest of the mix and a
  quadrature residue of it lands in FL/FR, gated band by band at the frame rate. Multiplying the Ls/Rs QMF
  samples by `-j` before the JOC matrix (the `--surround-phase -1` switch) makes the burst passage match Apple's
  render at a normalised cross-correlation of **0.987** (was 0.77) and the whole song at **0.98** above 150 Hz
  (was 0.85); on song B the same switch takes the whole-song correlation above 150 Hz from 0.74 to **0.976**.
  `+j` makes everything worse (0.29), so the sign is unambiguous.
* **Secondary Cavern bugs** (fixed by switches, small measurable gains, kept in the recommendation): the JOC
  applier's timeslot counter wraps at the number of input channels (7) instead of the 24 QMF timeslots per frame,
  so the interpolated matrices are recomputed every 7 timeslots and start out of phase with the frame in 6 of 7
  frames; the two-data-point and steep-slope interpolation branches deviate from TS 103 420 clause 6.6.5 (not
  exercised by these two songs, which use one smooth data point in every frame).
* **Ruled out**: the E-AC-3 core (clean), the QMF filter bank (78-83 dB round-trip SNR), OAMD placement (the
  pair really is at the front corners), sparse JOC (no sparse-coded object in either song, so upstream #419 is
  irrelevant here), renderer settings (UpdateRate 16/64/256, LFESeparation, DirectLFE, a 7.1.4 layout: no change
  to the burst), clipping, and newer upstream code (no JOC commit after the local checkout 7f530786).
* **Bass**: Cavern's LFE output is simply the core LFE x 0.707 (the same -3 dB it applies to every object); there is
  no bass management and no +10 dB convention anywhere. `LFESeparation = false` would mix *every* object into LFE
  at full gain through a first-order 120 Hz filter, which is neither the level (-9 dB) nor the slope of the feed
  the coordinator fitted, so the feed is implemented as a post-process in the probe (`--bass-manage 0.35 90`,
  selectable filter type). The 4th-order 90 Hz feed reproduces Apple's band levels within 1 dB rms but lowers
  the sub-150 Hz waveform agreement; low-order filters do the opposite (details in section 5).
* **"Decorrelation"**: neither the JOC specification (clause 6.6.6 is a pure linear mix) nor Cavern decorrelates
  anything. The near-zero waveform correlation above 150 Hz was clock drift between the reference capture and
  our render: the best lag moves by about 63 samples per minute on song B (190 samples over the song) and about
  80 per minute on song A. With per-second alignment the baseline already correlates at 0.74/0.85 above 150 Hz,
  and 0.98 once the phase shift is compensated.
* **Recommendation**: build `tools/cavern-lab/` and point am-ui's render at
  `tools/cavern-lab/render-music-916-lab.sh` (same interface and output as the workspace script, default options
  `--joc-timeslot-fix --joc-interp-fix --surround-phase -1`). Report the phase-shift measurement on upstream
  issue #423 and the timeslot/interpolation bugs as a new issue (section 7).

## 1. Setup

* Lab: `tools/cavern-lab/cavern-src` is a copy of the workspace Cavern checkout (upstream 7f530786, 2026-11-04)
  with three additions (`LabSwitches.cs`, `LabQmfTest.cs`, and switch-gated patches in `JointObjectCoding.cs`,
  `JointObjectCodingApplier.cs`, `JointObjectCodingDecoder.cs`); `probe/Program.cs` is the workspace probe plus
  options and instrumentation. With no options the output is **bit-identical** to the workspace render
  (`cmp` against the coordinator's renders of both songs), so every variant below differs only by its switch.
* Scripts: `build.sh` (dotnet build with all state under `var/build/`), `render-music-916-lab.sh` (drop-in
  wrapper), `run-variants.sh` (named variant renders), `analyze.py` (burst metrics, fold-down, reference
  alignment), `bands.py` (1/3-octave band table, per-second correlation split at 150 Hz, drift check),
  `consistency.py` (reference-free check against the core), `objects_report.py` (per-object view of an
  instrumented render), `compare.py` (variant table). `tests/test_render_artifact.py` covers the analysis tools.
* Reference alignment: the Apple captures lead-in is 36,165 samples on song A and 18,431 on song B at the start;
  both drift (section 6), so all whole-song correlations use the per-second best lag.
* Stream facts (both songs): E-AC-3 5.1 core (L C R Ls Rs + LFE), 1536-sample frames (24 QMF timeslots), 15 JOC
  objects, 16 OAMD objects = 1 LFE bed + 15 dynamic objects, `joc_dmx_config_idx = 3`, every active object-frame
  in the analysed windows uses 12 parameter bands, fine quantisation, one data point, smooth slope; no sparse
  coding, no two-data-point or steep-slope frames in either song (counters in `render-summary.json` -> `lab`).

## 2. Localising the artefact

### 2.1 Bed-only decode (`--mode bed`)

The 5.1 core decoded without JOC or renderer is clean: FL/FR carry the whole mix throughout (RMS 0.02-0.07 in
4-8 s, including the loud event at 6.1-6.3 s that is the "burst" in our render), no anomaly in 5.55-6.75 s. Folded
to stereo the core correlates with Apple's render at 0.79 over 4-7.9 s, better than our 16-channel render (0.63
with the same global alignment). So the fault is downstream of the core.

### 2.2 Which object carries the burst (`--dump-objects --log-objects`)

Renderer objects 8 and 9 (JOC objects 7 and 8; renderer slot 0 is the LFE bed) carry it: raw JOC output RMS
0.028 each in 5.55-6.75 s, correlation 0.99 with the render's FL and FR, zero before 5.5 s and after 6.9 s. The
OAMD places them at (0, 0, 0) and (1, 0, 0), i.e. exactly the front-left and front-right corners (object 9 is
explicitly moved there at 5.1 s). So FL/FR are the right speakers for this content; the renderer is not misplacing
anything, and the burst is this object pair's reconstructed signal.

### 2.3 JOC parameters and matrices (`--log-joc`)

Through the whole passage both objects use 12 bands, fine quantisation, one smooth data point, clip gain 1.0.
Their mixing matrices (rows L R C Ls Rs, columns = parameter bands) at 6.207 s:

```
front-left  (JOC 7):  L  0.8 0.9 0.8 0.9 0.4 0.8 0.4 0.8 0.9 0.8 0.8 0.9
                      Ls 0.0 -.9 -.9 -.9 -.4 -.8 -.3 -.9 -.7 0.0 0.0 0.0   (R, C, Rs ~ 0)
front-right (JOC 8):  R  0.9 0.9 0.4 0.9 0.8 0.8 0.9 0.9 0.9 0.0 0.0 0.9
                      Rs -.5 -.6 -.4 -1. -.8 -.7 -.6 -1. -.7 0.0 0.0 0.0   (L, C, Ls ~ 0)
```

Band-limited regression of the dumped object signals against the renderer's own JOC inputs reproduces these
coefficients (e.g. front-right in 375-750 Hz: +0.78 R - 0.58 Rs), so decoding, channel mapping and mixing are
self-consistent. The point is the large negative surround weight: the encoder cancels the rest of the mix out of
L and R by subtracting Ls and Rs.

### 2.4 The reconstruction is wrong only where the surround subtraction matters

* Per-frame band energy of the front-left object relative to the core's L: about -1 to -2 dB in every band during
  the loud part, but the top bands drop to -15..-80 dB in the frames around the onset and the decay (e.g. at
  6.718 s: band 1 -34 dB, band 10 -47 dB, band 11 -77 dB). This band-wise gating at the 31 Hz frame rate is what
  the ear reports as distortion.
* Reference-free consistency (`consistency.py`: objects re-rendered and folded to stereo versus the core folded
  the same way, QMF delay of 577 samples applied): median 0.96 over the song but **0.74 in 4-7.9 s**. The error
  concentrates in the burst window.
* The core itself shows the shift that breaks the subtraction. Cross-spectral phase between core channels:

```
                 100 Hz  200   400   800   1600  3150  6300  12500   (coherence 0.3-0.8)
L vs Ls  (5.5-6.9 s)  -68  -66   -84   -90   -90   -90   -89   -87
R vs Rs  (5.5-6.9 s)  -83  -62   -98   -93   -90   -89   -91   -76
L vs Ls  (30-60 s)    -68  -87   -92   -93   -92   -90   -93   -89
L vs R   (5.5-6.9 s)   -9    0    -1    -3    +2    +1    -1    +1
Ls vs Rs (5.5-6.9 s)  -25   +6   +16   -40    +4    +2    +2    +8
```

### 2.5 Fix: compensate the surround phase before JOC reconstruction

`LabSwitches.SurroundPhaseShift` (probe `--surround-phase -1|1`) multiplies the complex QMF samples of inputs 3
and 4 (Ls, Rs) by `-j` or `+j` after the analysis bank and before the matrix multiply, only when
`joc_dmx_config_idx` is 3 or 4. Results (drift-corrected per-second NCC against Apple's render; `analyze.py`
burst metrics on FL/FR in 5.55-6.75 s):

| song A variant | burst FL/FR RMS | peak | max jump | HF share >6 kHz | NCC vs Apple 4-8 s | NCC >150 Hz, whole song |
|---|---|---|---|---|---|---|
| baseline (workspace render) | 0.0287 | 0.261 | 0.146 | 0.053 | 0.770 (0.80 0.76 0.78 0.61 per second) | 0.846 |
| UpdateRate 16 / 64 / 256, LFESeparation off, DirectLFE off | identical to baseline | | | | | |
| ref714 layout (12 ch) | 0.0294 | 0.246 | 0.149 | 0.067 | 0.728 (global offset) | - |
| `--joc-timeslot-fix` | 0.0287 | 0.261 | 0.146 | 0.053 | 0.795 | 0.846 |
| `--joc-interp-fix` | = baseline (rounding only; no 2-dp/steep frames) | | | | | |
| `--surround-phase 1` (+j) | 0.0335 | 0.267 | 0.146 | 0.047 | **0.287** | 0.641 |
| `--surround-phase -1` (-j) | 0.0231 | 0.184 | 0.139 | 0.091 | **0.987** (0.99 0.99 0.99 0.98) | **0.982** |
| all three fixes, -j | 0.0231 | 0.185 | 0.137 | 0.091 | 0.986 | 0.981 |

The compensated front pair is 2 dB quieter with a lower peak and relatively more high-frequency content: the
low/mid-frequency leak of the rest of the mix that the subtraction was meant to remove is gone. Audition files
are listed in section 8; the "wrong sign" render is included as a control.

### 2.6 The JOC applier timeslot bug (`--joc-timeslot-fix`)

`JointObjectCodingApplier.Apply` wraps its `timeslot` counter at `input.Length`, the number of downmix channels
(7), instead of the 24 QMF timeslots of a frame (`frameSize / 64`). `GetMixingMatrices` is therefore recomputed
every 7 timeslots, only the first 7 of the 24 interpolated timeslots are ever used, and the cycle is out of phase
with the frame start in 4263 of 4974 frames (exactly 6/7). The switch wraps at the frame length; the output stays
in phase for the whole song. Effect: consistency window 0.741 -> 0.777 (median 0.961 -> 0.964), NCC vs Apple in
4-8 s 0.770 -> 0.795. The burst itself is unchanged, so this is not its cause. The code dates from 2022
(`627afcc3`), before the current renderer structure.

### 2.7 Interpolation deviations from TS 103 420 clause 6.6.5 (`--joc-interp-fix`)

Compared with the spec's Pseudocode 6 (clause 6.6.5, pages 63-64 of V1.2.1):

* smooth slope, two data points, first half of the frame: Cavern indexes `mixMatrix[0]` by QMF subband
  (`channelTo[sb]`) where the matrix is indexed by parameter band (the same class of bug #402 fixed in the steep
  path); subbands above the band count read stale values;
* steep slope, one data point: `mixMatrix[ts < timeslotOffsets[obj][1] ? 1 : 0]` consults the second offset and
  the second data point, which do not exist for one data point (stale from earlier frames);
* steep slope, two data points: the second data point is never used and the timeslot comparison is off by one.

`InterpolateSpec` in `JointObjectCodingDecoder.cs` implements the spec text (with the parameter band mapping in
every branch, as #402 argued). For one smooth data point it is the same formula, and the output differs from the
baseline only by float rounding. Neither song contains two-data-point or steep frames, so this fix is untested on
real content; it can be dropped from `CAVERN_LAB_OPTIONS` if a song shows a problem.

### 2.8 Other checks

* QMF bank (`--mode qmf-test`): analysis + synthesis with a unity matrix gives 577 samples of delay, 0.00 dB gain
  and 78-83 dB SNR for noise, 1 kHz, 10 kHz and a sweep. Not the cause.
* Clipping: fold-down peak 0.27 in the passage, 0.69 over the song (no preamp) - no clipping anywhere.
* Level: per 100 ms, our fold-down is within +-1.7 dB of Apple's through the passage - the burst is not a level
  error.
* Sparse JOC (#419): no sparse-coded object in either song (`frames_with_sparse_objects = 0`). Note for later:
  Cavern's sparse decoding deliberately departs from the spec's Pseudocode 2 (the spec sets unselected channels to
  the quantiser *offset*, 50/100, and predicts per channel; Cavern sets them to the centre, 48/96, and follows
  bands), the upstream author considers the spec text wrong there.
* Dequantisation step: Cavern uses 0.2/0.1 where the spec says 820/4096 and 820/8192 (0.2002/0.1001): -0.1 %,
  negligible.

## 3. Upstream status

The local checkout 7f530786 (2026-11-04) is the newest commit touching the JOC/E-AC-3 code; upstream HEAD
c6426fe (2026-11-07, "Legends for graphs") is unrelated, so there is no upstream fix to adopt. Issue **#423**
(opened 2026-10-24, open, assigned 2026-11-04) reports exactly the missing Ls/Rs phase compensation for
`joc_dmx_config_idx = 3` and says the Dolby reference player applies the inverse shift before reconstruction;
our measurement (section 2.4-2.5) confirms the shift in Apple's streams and the `-j` direction. Older reports of
distorted Apple E-AC-3 JOC decodes (#17, #283, #326) were closed as not planned/duplicates. (Upstream history and
issues were read from a temporary clone that is not kept in the repo.)

## 4. Cavern's LFE path

* `EnhancedAC3Renderer`: the core LFE channel bypasses JOC (as the spec requires) and becomes the renderer object
  at `OAMD.GetLFEPosition()` (slot 0 here) with `LFE = true`; like every object it gets `Volume = 0.707`
  ("general -3 dB gain to prevent clipping"), so LFE and objects keep the core's relative levels.
* `Listener` with am-ui's settings: `LFESeparation = true` means `BalanceBasedRenderer.Render` mixes a source into
  LFE channels only if `source.LFE` is set, so the LFE output is the LFE object alone; `DirectLFE = true` skips the
  `Lowpass(SampleRate, 120)` on LFE channels, which is a **first-order** filter (`Cavern/Filters/Lowpass.cs`);
  `LFEVolume = 1`. No +10 dB LFE convention exists in Cavern; the output LFE is 0.707 x core LFE.
* `LFESeparation = false` adds every object to the LFE channel at its full render gain (then the first-order 120 Hz
  low-pass if `DirectLFE = false`). That is about 9 dB above and much shallower than the feed fitted on song B
  (0.35 x 4th-order 90 Hz low-pass of the sum of the 15 other channels), so it cannot emulate Apple's output.
  Hence the probe option `--bass-manage GAIN FREQ [--bass-type bw4|bw2|lr4|bw1]`, applied to the 16-channel
  output before it is written (the PEQdB fold then multiplies LFE by 2.26 as before).

## 5. song B: band table and correlation per variant

Band levels are of the PEQdB fold (centre -3 dB, LFE x2.26 after the 125 Hz 4th-order low-pass and matching
delay) without the -9 dB preamp, so the flat part of "reference minus ours" is -7.5 dB here where the coordinator
saw +1.5 dB; the table lists the deviation from that flat part. NCC values are medians of per-second correlations
with per-second alignment (section 6), split at 150 Hz.

| variant | NCC <150 Hz | NCC >150 Hz | NCC full | ref - ours relative to flat, 25 / 31.5 / 40 / 50 / 63 / 80 / 100 / 125 Hz |
|---|---|---|---|---|
| baseline | 0.664 | 0.736 | 0.705 | +2.9 +5.0 +7.9 +12.0 +8.0 +2.2 -4.3 -0.3 |
| `--joc-timeslot-fix` | 0.663 | 0.736 | 0.706 | same |
| `--surround-phase -1` | 0.726 | **0.976** | 0.846 | +4.6 +6.5 +9.3 +12.6 +8.2 +2.1 -4.6 -0.2 |
| `--surround-phase 1` | 0.643 | 0.453 | 0.499 | - |
| `--lfe-gain 3.16` (+10 dB) | 0.054 | 0.724 | 0.379 | -2.1 -0.4 +1.8 +3.8 +1.3 -3.1 -8.9 -2.9 |
| `--bass-manage 0.35 90` (bw4) | 0.545 | 0.734 | 0.665 | -2.8 -0.8 +0.7 +0.5 -0.7 +1.4 +1.9 +0.4 |
| all fixes, -j, bass bw4 0.35/90 | 0.558 | 0.977 | 0.777 | -1.0 +0.4 +1.4 +1.0 -0.3 +1.7 +1.3 +0.6 |

Reference-free consistency against the core (30-60 s): baseline 0.944, timeslot fix 0.945, -j 0.761, +j 0.677.
The drop with -j is expected: the core's Ls/Rs still carry the shift, so a correctly de-rotated object no longer
matches them sample by sample; the Apple correlation is the right arbiter here.

Bass feed variants, emulated on the -j render (first 60 s; band error = rms of the deviation 31.5-160 Hz):

| feed | NCC <150 Hz | band error rms |
|---|---|---|
| none | 0.709 | 7.1 dB |
| bw4 0.35 @ 90 Hz (coordinator's fit) | 0.552 | **1.0 dB** |
| bw4 0.35 @ 100 Hz | 0.634 | 1.4 dB |
| lr4 0.35 @ 90 Hz | 0.549 | 2.0 dB |
| bw2 0.35 @ 90 Hz | 0.818 | 3.0 dB |
| bw2 0.25 @ 120 Hz | 0.838 | 3.3 dB |
| bw1 0.35 @ 90 Hz | **0.864** | 3.9 dB |

The steep 4th-order feed reproduces Apple's magnitude response (including the 100 Hz dip, which is an
interference effect between the feed and the mains' own bass) but its phase makes the sum less coherent with
Apple's waveform below 150 Hz; gentle filters keep the waveform coherent but miss the dip. A two-input
frequency-domain fit of Apple's feed is ill-conditioned (the feed is derived from our own channels), so the exact
filter Apple uses could not be identified from these captures. Both are available in the probe; the choice should
be made by listening (`--bass-type bw4` for level match, `bw2`/`bw1` for coherence).

**Superseded (section 9):** Apple's render has no bass feed. Cavern's LFE is 577 samples early and 3 dB low;
the "missing" bass was the early LFE cancelling the mains' bass around 40-50 Hz.

## 6. Drift, "decorrelation", object size

* The per-second best lag between the reference captures and our renders drifts linearly: song B from -157 to
  +35 samples over 182 s (63.6 samples/min, about 22 ppm), song A from -164 to about +47 over 150 s. Any analysis
  with one global offset is misaligned by up to 3.3 ms, which zeroes waveform correlation above a few hundred
  hertz while sparing the bass - the signature that was read as decorrelation. With per-second alignment the
  baseline correlates at 0.736 (song B) / 0.846 (song A) above 150 Hz, the -j renders at 0.976 / 0.982.
* TS 103 420 clause 6.6.6 reconstructs objects as `z[obj][ts][sb] += x[ch][ts][sb] * joc_mix_mtx_interp`; the
  word "decorrelation" does not occur in the specification. Cavern does the same; there is no decorrelator to
  enable.
* OAMD object size is parsed (`ObjectInfoBlock.UpdateSource` -> `Source.Size`) and rendered by
  `BalanceBasedRenderer` (the renderer Cavern picks for symmetric layouts such as ref916) as a crude uniform
  spread: the point-source gain is scaled by `1 - Size` and every non-LFE speaker receives `gain *
  sqrt(Size / channels)`. Zone constraints and speaker snap are parsed and ignored. No renderer setting changes
  this.

## 7. Recommendation

1. Build once: `tools/cavern-lab/build.sh` (dotnet 8 SDK from the workspace, all writable state under
   `var/build/`; a NuGet named-mutex error on the first restore is retried automatically).
2. Render with `tools/cavern-lab/render-music-916-lab.sh input.m4a output-dir` instead of the workspace
   `tools/render-music-916.sh`. Same arguments, same `atmos-916.f32le` (16-channel f32le, FL FR FC LFE SL SR RL RR
   FLW FRW TFL TFR TSL TSR TRL TRR) and the same `render-summary.json` fields plus a `lab` block that records the
   switches, `joc_dmx_config_idx`, and the frame/sparse/two-data-point counters. Default switches:
   `--joc-timeslot-fix --joc-interp-fix --surround-phase -1 --lfe-gain 1.41421356 --lfe-delay 577` (the last two
   since 2026-11-15, section 9); the phase compensation only acts on streams with `joc_dmx_config_idx` 3 or 4.
   Override with `CAVERN_LAB_OPTIONS`, e.g. add `--bass-manage 0.35 90` for the old bass feed.
3. Upstream: add the measurement to VoidXH/Cavern issue #423 (core L-Ls cross-spectral phase -90 degrees on Apple
   streams with `joc_dmx_config_idx = 3`; multiplying the Ls/Rs QMF samples by -j before `ProcessObject` gives
   0.98 correlation with Apple's renderer; +j is wrong) and open an issue for `JointObjectCodingApplier.Apply`
   wrapping `timeslot` at `input.Length` (7) instead of `frameSize / 64`, plus the interpolation indexing in
   `GetMixingMatrices` (section 2.7). The lab patches are switch-gated and small enough to be lifted into a PR.

## 8. Files

* Lab (ours): `tools/cavern-lab/{build-env.sh,build.sh,render-music-916-lab.sh,run-variants.sh}`,
  `tools/cavern-lab/probe/{Program.cs,CavernProbe.csproj}`, `tools/cavern-lab/cavern-src/` (Cavern 7f530786 plus
  `Cavern.Format/Decoders/EnhancedAC3/{LabSwitches.cs,LabQmfTest.cs}` and the gated patches),
  `tools/cavern-lab/{analyze.py,bands.py,consistency.py,objects_report.py,compare.py}`,
  `tests/test_render_artifact.py`.
* Renders (not for the repo): `var/tmp/cavern-lab/baseline/`, `var/tmp/cavern-lab/songA/<variant>/`,
  `var/tmp/cavern-lab/songB/<variant>/`, each with `atmos-916.f32le` + `render-summary.json`; `songA/bed/bed.f32le`
  and `songB/bed/bed.f32le` are the 6-channel cores; `songA/instrumented/` has the per-object dump and logs.
* Audition excerpts (stereo float WAV, 48 kHz) in `var/tmp/cavern-lab/excerpts/`:
  `songA-baseline-fold-4.5-7.5s.wav`, `songA-fixed-phase-fold-4.5-7.5s.wav` (recommended fix),
  `songA-control-wrong-sign-fold-4.5-7.5s.wav`, `songA-core-5.1-fold-4.5-7.5s.wav`, `songA-apple-reference-4.5-7.5s.wav`,
  `songA-baseline-FL-FR-only-4.5-7.5s.wav`, `songA-fixed-phase-FL-FR-only-4.5-7.5s.wav`,
  `songB-baseline-fold-30-40s.wav`, `songB-fixed-phase-fold-30-40s.wav`, `songB-apple-reference-30-40s.wav`.
  The folds are the PEQdB matrix without the LFE path and without EQ, scaled by 0.5 (song A) / 0.35 (song B).

## 9. The LFE's alignment (2026-11-15)

Measured on song B with the engine's own downmix (`crates/engine/src/dsp.rs`, PEQdB, flat EQ;
`cargo run --release -p amui --example downmix_file`) against the reference capture. The capture's clock wanders
(the lag grows from about -1 to +177 samples over the song, not linearly), so the comparison follows it: lags of
2 s windows, then 0.25 s windows aligned on the band above 150 Hz with fractional refinement.

* With the switches of section 7 and the bass feed, the mid (L+R) correlates at 0.99+ above 150 Hz but only 0.54
  below; the side (L-R), which the LFE does not reach, at 0.99 in every band. So the difference is the LFE path.
* Per-frequency least squares of the reference's mid on our render's parts (the mains' fold at the measured gain,
  the sum of the mains, the LFE) puts almost nothing on the sum of the mains (no feed) and a phase on the LFE that
  falls linearly with frequency: a delay. A search over gain and delay of the LFE (no feed) has its optimum at
  569 +- 2 samples and x1.36 (residual -24.0 dB of the reference's low band; -23.0 dB at 577 samples).
* Cavern's code explains both: `EnhancedAC3Renderer` takes the LFE of the timeslot straight from the decoder while
  the other channels come out of the JOC reconstruction in the QMF domain, and the bank's round trip delays by 577
  samples (`--mode qmf-test`); every object, the LFE one included, gets `Volume = 0.707`. Apple's render keeps the
  LFE aligned with the other channels and at the core's level.
* `--lfe-delay 577 --lfe-gain 1.41421356` without the feed: mid correlation 0.998 below 150 Hz (0.54 before),
  0.996 full band (0.83); band levels within 0.2 dB rms from 31.5 Hz to 16 kHz (0.57 dB before; below 160 Hz
  0.3 dB). Without the delay, no feed leaves up to 12.7 dB missing at 50 Hz, the interference that the 2026-11-08
  fit had read as a bass-management feed.
* Song A, the same comparison: with the feed, mid 0.874 below 150 Hz and 0.916 full band, band levels 1.42 dB rms
  off (a 6.9 dB hole at 100 Hz); with `--lfe-delay 577 --lfe-gain 1.41421356` and no feed, 0.998 below 150 Hz,
  0.996 full band, 0.43 dB rms.
* The engine's default switches carry both options and its render generation is `cavern-lab-2`. The bass feed is no
  longer offered by the application (the probe keeps `--bass-manage` for experiments): the captures show none.

## Notes

* `Cavern.Format/Decoders/EnhancedAC3/ObjectAudioElementMetadata.cs` carries a 2026 header stating that reading or
  working with the file by AI agents is prohibited. It was displayed once as part of a directory listing before
  the header was noticed; it was not used, quoted or modified afterwards (the lab copy keeps it unchanged because
  the library does not compile without it). Nothing in this investigation depends on that file.
* The upstream repository was cloned temporarily for history and removed again so the am-ui repo does not embed
  a nested git checkout; the relevant findings are in section 3.
