# Playback core notes (output.py, player.py, loudness.py, cli_play.py) — 2026-11-08

Owner of this slice: the playback agent. Files: `amui/output.py`, `amui/player.py`, `amui/loudness.py`,
`amui/render.py`, `amui/cli_play.py`, `tests/test_output.py`, `tests/test_player.py`, `tests/test_render.py`,
`tests/test_loudness.py`, `tests/test_cache_inuse.py`, this file. (`amui/cache.py` got the in-use
markers described below; everything else there is the pipeline owner's.)

## The two reported glitches (investigation of 2026-11-08)

Reports: (1) song A (fresh Cavern render) "distorted for < 2 s around 0:05"; (2) song C
(stereo AAC, complete `PcmFile`) "a buzzing noise at 3:44 in a quiet outro". Both decoded
files are clean and pw-top showed `ERR 0` for the am-ui node.

What the fake paced consumer captures showed (every written frame compared against the PCM files by
exact match; scripts kept in the session scratchpad):

* 1a — fresh render from t=0 with `MIN_BUFFER_SECONDS` while a second Cavern render and 2-6 CPU hogs
  run: **no starvation, no gating, 0 discontinuities, bit-exact stream**. The render keeps 19x real
  time even next to a second render on this 16-thread machine (lead over the cursor grows ~9 s per
  second). The starvation path cannot have fired at 0:05 with a healthy render.
* 1b/1c — the real Player (acquire from cache, Cavern renders, ffmpeg decodes, `PcmCache(keep=3)`,
  `next()` every 3 s, near-end seeks into prefetched renders, 2-ch -> 16-ch -> 2-ch handoffs): every
  transition clean, channels placed correctly, no stale data, evictions and cancellations never
  touched an active file. **One real bug surfaced: `PipeWireOutput` had no `handoffs` property, so
  `Player._cmd_source_ended` never recognised a gapless continuation and re-issued `set_source(B, 0)`
  ~120 ms after every handoff: the first 0.1 s of each track played twice (fade-out, restart from
  0).** Fixed (counter added); the re-run shows the next track continuing straight through.
* 1a + an in-process GIL hog (a 1.5 M-element `sorted()` every 0.5 s, i.e. a C-level call that holds
  the GIL for 330-630 ms the way heavy UI/JSON/image work does): the **feeder cannot write while the
  GIL is held**, the consumer's read gaps grew to 564 ms against an 85 ms pipe cushion, yet the
  *content* stayed bit-exact. With the real pw-cat the blocked read stalls its process callback; the
  stream is `node.async = true` on PipeWire 1.6, so the graph does not stall and the sink mixes
  silence for the missing cycles (a gap, or a crackle/buzz when it happens repeatedly), and the
  missed cycles are not necessarily counted in the node's ERR column.

Conclusion: neither glitch is a content error of the feeder; the only mechanism found that produces
audible trouble with clean bytes and `ERR 0` is the feeder being held off the CPU by another thread
of the same process (the UI, API JSON parsing, artwork decoding) for longer than the pipe cushion.
The skipping session fits (lots of UI work), as does a glitch a few seconds into a track. What this
revision does about it: the pipe cushion is doubled (170 ms), every stall now leaves evidence
(`feeder:` lines, pipe-low counter), and the last 30 s can be dumped as a WAV right after a glitch.
If the log shows `feeder: pipe low` lines next time, the fix is structural: move the feeder into its
own process (no shared GIL), which the output's design already allows (stdin pipe + control pipe).
`python3 -m amui output-test --stall 300` reproduces the mechanism on the real sink and prints the
feeder gap, the pipe-low counter and pw-top's ERR before/after; it plays audio, so only with consent.

## PipeWireOutput (amui/output.py)

One long-lived `pw-cat` per output (exact command in `pw_cat_command()`; it is the verified
command plus `-v`, whose stderr lines carry the stream clock). stdin is the only data path.

### Pacing: the pipe is the clock

pw-cat reads stdin *inside its process callback*, so whoever feeds it must never let it
wait. The feeder thread ("am-ui-feeder") composes 10 ms blocks (480 frames x 64 B) and writes
them with **blocking writes** into a stdin pipe enlarged to 512 KiB (`F_SETPIPE_SZ`; granted
524288 = 8192 frames = 170 ms). Steady state: the pipe is full, the writer sleeps in `write()`;
each graph cycle pw-cat takes one quantum (2048 frames here, 42.7 ms: `size:2048` in its clock
line), the writer wakes, refills ~4.3 blocks, blocks again. No timers, no sleeps; the pipe's
backpressure is the only pacing. Measured on the fake consumer: 1.000 s of audio per wall second,
the writer never more than pipe + one block + one quantum ahead.

Why 512 KiB (was 256 KiB): the pipe is the only slack the feeder has when this process's GIL is
held by another thread (see above); 85 ms was not enough for ordinary UI work. The cost is control
latency: a pause/seek/volume change becomes audible after pipe + stream delay ≈ 255 ms (was 170).
`pipe_bytes=` is a constructor option.

### Position

```
audible_stream_frame = frames_written - FIONREAD(pipe)/64 - delay_frames
```

* `FIONREAD` on the *write* end of a pipe works on Linux (verified); if it ever fails we assume
  the pipe is full (the writer is blocked almost always, so that is the common case anyway).
* `delay_frames` is what pw-cat reports once a second on stderr with `-v`
  (`stream time: ... delay:N ... size:Q`): 4096 frames (85 ms) on this machine (2 x 2048:
  the async stream's own buffer plus the graph/driver). Until the first line arrives (first
  ~30 ms) the spec fallback `latency_ms * 48` (2400) is used.
* A **segment table** maps stream frames to source frames: the feeder appends a segment
  whenever the relation changes (new source, seek, pause/resume, starvation, gapless
  handoff). Each segment: `stream_start, source, frame, advancing, generation`.
  `position_frames()` finds the segment containing the audible stream frame; advancing ->
  `frame + (audible - stream_start)`, held (silence) -> `frame`.
* Every `set_source()`/`seek()` request (and every gapless handoff) bumps a *generation* and
  records the target frame. Audible content from an older generation reports the target
  (the slider stays where the user dropped it during the ~250 ms until the new audio is
  heard); content of the current generation maps through the table. A per-generation
  `max()` clamp keeps the value monotonic within a segment even though `frames_written`
  and `FIONREAD` are sampled at different instants.
* Events: `on_source_started(source)` when its first frame is audible, `on_source_ended(source)`
  when its last frame has been audible; queued with their stream frame when the feeder writes
  them and dispatched after each write once the audible estimate passes them (so they fire
  ~250 ms after the bytes left the feeder, in order; ended(A) precedes started(B) for a
  gapless pair).

Measured accuracy against the fake consumer's own clock (it reads a 1024-frame quantum per
cycle and reports `delay:1024`): mean -15 ms, max |error| 28 ms over 43 samples, i.e. within the
quantum granularity. On the real sink (`python3 -m amui output-test --seconds 6 --volume 0.05`,
measured with the 256 KiB pipe): pw-dump node id 157 (`Stream/Output/Audio`, latency 2400/48000,
target `atmos_916_downmix`), `started` fired 0.19 s after `set_source`, position advanced 1.000 s
per second, pw-top `ERR 0`, clean drain and exit 0 on stop.

### Content rules (per block)

* Gain = `volume x trim`, applied as a linear ramp over the block from the previous block's gain to
  the new target (`np.linspace(..., endpoint=False)`): volume changes, pause (ramp to 0, then
  silence), resume (ramp from 0), seek/set_source (one fade-out block from the old position,
  the jump, then a fade-in block). The Sound Check trim (`set_trim(source, dB, ramp_s=0.05)`,
  attenuation only, per source, `set_sound_check()` on/off) ramps linearly over 5 blocks = 50 ms when
  a source starts (track start, handoff) or Sound Check is toggled, and over the source's own
  `ramp_s` when its trim changes while it plays (the player passes 1.5 s for a refined measurement).
  Samples are **not clipped** (float graph; decoded AAC peaks above 1.0 on loud masters and
  clipping it distorted them, commit 7594eca); only non-finite values are zeroed.
* 2-channel sources land in FL/FR of a zeroed 16-channel block (channels 2..15 stay exactly 0).
* Reads are `os.pread` of exactly the frames needed (no memmap: files grow). The reader opens its
  fd when the source is armed (`set_source`/`set_next_source`) and keeps it: a cache directory that
  is re-claimed or evicted under us is still read from the inode we opened; `fstat(fd)` caps what
  the feeder believes exists, so a replacement file is never read at a wrong offset, and the event
  is logged once ("replaced under us" / "complete file shrank").
* A growing source (`done` False) keeps one block of **reserve** behind `frames_available()`: when
  the cursor reaches it, that reserve block is played faded out, then silence with
  `buffering=True` without advancing, and playback resumes with a fade-in only once
  `RESUME_SECONDS` (0.5 s) of audio exist beyond the cursor (hysteresis), or the source is done.
  Each episode increments `underruns` and logs `starvation start/end` with source, cursor and
  available frames. A done source plays to `min(total_frames, frames_available())`.
* Gapless: `set_next_source(src)`; when the current source is exhausted the *same* block
  continues with frame 0 of the next (sample-exact, verified: A's frame 23999 then B's frame 0
  in adjacent stream frames); `handoffs` counts them and the player uses the counter to tell a
  continuation from a fresh start. `set_source()` clears a pending next source (the player
  re-arms it; after a mode switch the old prefetch would be the wrong variant).
* A source raising from `frames_available()`/`done`/`error` is treated as finished at the
  current cursor and `on_source_ended` fires; `source.error` is left for the player to report.

### Diagnostics

* `var/logs/playback.log` (0600, rotated to `.1` at 1 MiB; `amui.output.LOG`, shared with the
  player): set_source/set_next_source/seek/pause/resume, handoffs, starvation start/end, replaced
  files, feeder gaps, pipe-low events, pw-cat lines mentioning xrun/underrun/error, and from the
  player: load/prefetch/play/switch/stop, render start/ready/pause/resume/stop/done, evictions,
  loudness results, errors, dumps. Tests redirect it (`LOG.set_path`).
* Feeder jitter monitor: the time between the end of one block write and the start of the next
  is the feeder's own latency (composition, lock waits, GIL waits; pw-cat's consumption shows up
  *inside* the blocking write). Per wall second the maximum is kept (`feeder_gap_ms`) and logged
  when > 40 ms. Before each write the pipe fill is sampled: fewer frames than one quantum while
  playing means pw-cat was about to block (`pipe_low_count`, logged at most once a second).
* Recent-audio ring buffer: every written block is folded to stereo (PEQdB matrix: FL/FR straight,
  FC -3 dB to both, LFE +7.1 dB to both, every other channel at 1.0 into its side; no LFE low-pass)
  into a 30 s float32 ring (~11.5 MB). `dump_recent(path)` writes it as a 48 kHz IEEE-float WAV
  (0600); `Player.dump_recent(path=None)` defaults to `var/logs/recent-<stamp>.wav`;
  `python3 -m amui play ... --dump-on-exit` saves it at shutdown.

### Lifecycle

`start()` spawns pw-cat (stdin PIPE, stdout DEVNULL, stderr -> reader thread -> `LOG_DIR/pw-cat.log`,
one "stream time" line per minute kept, everything else verbatim), enlarges the pipe, starts the
feeder at once (silence flows while pw-cat connects, so the graph never waits on us), then
watches the process for 0.5 s and raises `OutputError("pw-cat exited with status N: <last stderr>")`
if it dies (`on_error` is suppressed during that window). `stop()` lets the feeder finish its
block, writes a last 10 ms fade, closes stdin, waits for the drain (`drain start`/`stream drained`),
terminates/kills if needed. A dead pw-cat (EPIPE or exit) -> `on_error(message)`, `alive` False;
the same object can be `start()`ed again (that is how the player respawns).

`consumer_fd=` (tests) takes the write end of a pipe whose read end a test consumes instead of
pw-cat; `command=` replaces the pw-cat command line (the test suite runs a paced Python consumer
that mimics pw-cat including its stderr clock line).

## Player (amui/player.py)

* Worker thread "am-ui-player" + `queue.Queue` of commands; every public method enqueues.
  `snapshot()`/`queue()` read under a lock (the snapshot pulls `position_frames()`/`buffered_frames()`
  from the output). Position events ~4 Hz from the worker's idle tick.
* A **kind** is the PCM form: `atmos` (16-ch render), `stereo` (AAC decode), `lossless` (ALAC
  decode; the file comes from the FairPlay wrapper route, `amui.lossless`) or `binaural` (2-ch
  Cavern headphone render). `mode` + downmix profile + the `lossless` setting pick it
  (`_kind_for_mode(track)`): stereo mode -> `_stereo_kind(track)` = `lossless` when the setting is
  on, `Pipeline.lossless_available()` (wrapper built + signed in, not backing off) and the song is
  not in `_no_lossless`, else `stereo`; atmos mode -> `binaural` if the active profile is a render
  profile, else `atmos` (a song in `_no_atmos` with the fallback on -> `_stereo_kind`).
  Ready sources are kept per (track key, kind) for {previous, current, next}, so switching
  mode back is instant and `previous()` after a skip reuses the render.
* A `LosslessError` from a `lossless` job (service down, decrypt failure, no ALAC rendition):
  load -> non-fatal "error" event ("…; playing the AAC version") and `_start_current(kind="stereo")`;
  prefetch -> quiet, the AAC decode is prefetched instead; switch -> "…; staying with the AAC
  version". The song goes into `_no_lossless` for the session; `amui.lossless` also arms a 60 s
  back-off after service failures so following songs go straight to AAC. `set_lossless()` is
  persisted and switches the playing song like `set_mode()`. `shutdown()` stops the wrapper service
  when this process started it (`Pipeline.shutdown_lossless`).
* Per track a `_Job` thread (`load`, `prefetch` or `switch`) runs `_prepare()`:
  `acquire(variant)` -> `PcmCache.complete(key)` ? `CompleteRender`/`PcmFile` :
  `RenderSession(...).start()` + adaptive start | `decode_stereo` into `PcmCache.claim()`
  (phase "decode") | `render_binaural` (phase "render"). Results come back through the command
  queue; a newer command cancels the job's `threading.Event` and its result is dropped. A running
  prefetch for the track the user skips to is adopted as the load job.
* **Adaptive start** (`_wait_render_start`): a growing render is handed to the output once
  `MIN_BUFFER_SECONDS` (2 s) are available **and** the last 0.5 s of wall time produced audio at
  >= 3x real time (the rate needs >= 0.1 s of history, so a render that starts fast is not held
  back), or the render is done, or 6 s have passed since it started (time spent frozen by the gate
  excluded). No `MIN_BUFFER_SECONDS` within 180 s is an error. Measured here: dotnet needs ~0.6 s
  before the first frames, the first 2 s arrive at 3.4-3.6x, so a healthy render still starts at
  ~0.7-1.3 s as before; only a struggling one waits.
* **One Cavern process at a time** (`_RenderGate`, order current > next > previous): a render for
  the track the user wants now (load/switch jobs, or the session `set_current()` names when a
  prepared render becomes current) starts at once and freezes whatever else renders
  (`RenderSession.pause()` = SIGSTOP; `resume()` = SIGCONT; `stop()` sends SIGCONT first). A
  prefetch render waits while the *current* track still renders, and otherwise freezes a stale
  render (the previous track's) to take its place. `poll()` (worker tick and after every command)
  and `release()` resume the most recently frozen render once the running one finished or was
  stopped. Downloads/decrypts/decodes are not gated.
* NoAtmosError: with `stereo_fallback` (default True) -> non-fatal "error" event ("… has no
  Dolby Atmos version; playing the stereo version") and the stereo kind; without -> non-fatal
  event and skip. The answer is remembered per track (`_no_atmos`) so prefetch/replay go
  straight to the right variant. `format`/`format_detail` always describe the audible source:
  `"Dolby Atmos · E-AC-3 JOC 768 kbps · 15 objects"` (objects from the render summary, added
  when a progressive render finishes), `"AAC 256 kbps"`, `"Lossless · ALAC 24-bit/48 kHz"`
  (`"Hi-Res Lossless · …"` above 48 kHz), `"… · binaural"`.
* Transitions: the output's `on_source_ended` for the current source advances the queue. If
  the output already continued gaplessly into the prefetched source (`output.handoffs` grew and
  `active_source` is the prefetched one) only the state changes; otherwise `_start_current()`.
  After every transition (`_after_transition`): sources outside {previous, current, next} are
  dropped (unfinished renders stopped, loudness jobs cancelled, in-use markers removed) unless the
  output still plays or has them armed; `PcmCache.evict(protect=...)` with the index-based keys
  plus every kept source's key, the output's active/next source and every job in flight; then the
  prefetch is re-armed (a stale prefetch for another track is cancelled so it cannot keep
  rendering).
* **In-use markers**: `PcmCache.mark_inuse(key)` writes `<dir>/.inuse-<pid>` for every entry this
  process prepared, armed or is rendering/decoding; `claim()` raises `CacheBusyError` and
  `evict()` skips entries another *live* process marked (dead pids are stale and cleaned up). The
  player maps `CacheBusyError` to a non-fatal "another process is using this song's audio cache"
  error. This is what makes `python3 -m amui play/render/decode` safe next to the running app.
* **Sound Check** (`amui/loudness.py`): every prepared source gets an integrated loudness
  (BS.1770-4, 400 ms blocks / 100 ms hop, -70 LUFS absolute and -10 LU relative gates; 16-channel
  renders measured through the PEQdB stereo fold, stereo files as they are). Since 2026-11-08 the
  production meter is `loudness.StreamMeter`: the PCM is piped into ffmpeg's `ebur128` filter
  (`pan` does the fold for 16 channels; `ametadata … direct=1` streams the running integrated value
  per 100 ms frame), ~1000x real time against ~230x for the numpy `Meter` (which stays as the
  reference and the fallback without ffmpeg; the two agree within 0.01 LU on real files and
  synthetic signals, `tests/test_loudness.py`). A `_LoudnessJob` thread feeds the file while it is
  produced (10 s slices): the running value once `MIN_BUFFER_SECONDS` of audio are in is posted as
  `loudness_estimate` and written into the `_Prepared` so `_prepare()` → `_await_loudness()` can
  hold the start until it is there (≤ 1 s; a complete decode waits ≤ 0.6 s for the *final* value,
  which takes ~0.2 s for 4 minutes). Refinements follow when the value moved ≥ 0.3 dB after twice
  the audio, the final value the moment the file is complete; both are applied with a 1.5 s ramp
  (`set_trim(..., ramp_s)`), so an uncached track no longer starts loud and drops after a second
  (the 2026-11-08 report). Results are stored next to the PCM (`loudness.json` beside
  `render-summary.json`; stereo: `lufs`/`loudness_frames` keys merged into `meta.json`) so the next
  play is instant. The trim is `min(0, -16 - LUFS)` dB (attenuation only); `set_sound_check(bool)`
  (persisted, default **on** like the user's old client) applies it through `output.set_trim`,
  ramped over 50 ms at track start, handoff, or when a measurement arrives mid-track (a fresh
  render's trim lands a few seconds after its render completes). Measured on test songs:
  song C is -6.0 LUFS (peak 1.135) -> -10 dB, songs D and E -6.6 and -13.1 LUFS,
  the Atmos renders -17..-18 LUFS through the fold (no trim).
* `set_mode()` / render profiles: `switching=True`, the other kind is prepared in a `switch`
  job (instant if kept), then `output.set_source(new, start_frame=output.position_frames())`.
  Matrix profiles run `downmix.apply(key)` in an `_ApplyJob`; failure -> non-fatal error and
  `downmix_profile` reverts to what `downmix.current()` reports. At construction the persisted
  profile is restored; the worker's first act is to read `downmix.current()` and report *that*
  for matrix profiles (nothing is applied at start-up).
* Output death: `on_error` -> state "error" (non-fatal event), jobs cancelled; the next play
  command (`next`, `jump`, `toggle`, `resume`, `play`) restarts the same output object once.
* Settings persist in `STATE_DIR/player.json` (0600, atomic replace): mode, downmix_profile,
  volume, stereo_fallback, repeat, shuffle, sound_check. Shuffle keeps the current track first and
  remembers the original order (enqueue/play_next/remove maintain both lists); unshuffle restores
  it and re-finds the current index.
* `previous()`: restart if position > 3 s, else back (wraps with repeat all). `next()` with
  repeat one goes to the next track (manual), automatic end-of-track repeats it. `remove()` of
  the current track starts the item that took its place. `stop()` silences the output and
  keeps the queue; `clear()` empties it.
* Snapshot: `amui.player.PlaybackSnapshot` subclasses `types.PlaybackSnapshot` and adds
  `buffering` (bool), `underruns` (int), `sound_check` (bool), `track_gain_db` (float|None: the
  trim applied to the current track, None while unknown or with Sound Check off); `to_dict()`
  carries them. `types.py` itself is untouched (it is shared); fold the four fields into it when
  convenient.

## Deviations / extensions from the stubs

* `PipeWireOutput.__init__` gained `command`, `consumer_fd`, `log_path`, `pipe_bytes`; new
  `set_next_source()`, `set_trim()`, `set_sound_check()`, `dump_recent()`, `on_source_started`,
  properties `buffering`, `underruns`, `handoffs`, `trim_db`, `sound_check`, `feeder_gap_ms`,
  `pipe_low_count`, `active_source`, `next_source`, `frames_written`, `cursor_frame`,
  `pipe_bytes`, `delay_frames`, `node_id`, `stream_state`, `error`, `volume`;
  `audible_stream_frame()`; `pw_cat_command()`; module-level `LOG`/`log()`/`source_label()`.
  pw-cat runs with `-v` (needed for the delay/node lines; stderr goes to the log anyway).
* `Player.__init__` gained `output`, `output_factory`, `pipeline`, `state_path` (injection).
  `Pipeline` is the seam class (lazy imports of acquire/render/downmix; `measure_loudness()` is the
  loudness seam). Read-only properties `mode`, `stereo_fallback`, `downmix_profile`, `sound_check`;
  `set_sound_check()`, `dump_recent()`. The stub docstring said "no silent stereo fallback";
  implemented as specified in the task: fallback on by default, never silent.
* `RenderSession.pause()/resume()/paused`; `PcmCache.mark_inuse()/unmark_inuse()/in_use_elsewhere()`
  and `CacheBusyError`.
* `cli_play.COMMANDS`: `play` (`--no-fallback`, `--sound-check on|off`, `--dump-on-exit`,
  `--pcm-cache DIR`, `--state FILE`) and `output-test` (`--file`, `--start`, `--stall MS`).

## Integration assumptions (for the pipeline owner)

* `acquire.acquire(catalog_id, media_cache, variant=, progress=, cancel=)` returns
  `AcquireResult` whose `variant` dict has `audio_group` (`audio-atmos-2768` -> 768 kbps,
  `audio-stereo-256` -> 256 kbps), `codecs`, `bandwidth`; cached hits may carry None values
  (handled). `NoAtmosError` is recognised by isinstance (fallback: class name). Cancellation
  may raise anything: a cancelled job's outcome is ignored.
* `render.RenderSession(m4a, out_dir, expected_duration_s=)`, `.start()`, `.wait_for(frames,
  timeout) -> bool`, `.error`, `.done`, `.total_frames`, `.frames_available()`, `.summary`,
  `.stop()`, `.pause()/.resume()`; `CompleteRender(dir)` (raises on invalid), `PcmFile(path, 2)`,
  `render_binaural(m4a, out_dir) -> PcmFile` (blocking; called from a job thread),
  `binaural_available()`.
* `PcmCache.complete(key) -> dir|None`, `claim(key) -> dir` (raises `CacheBusyError`),
  `evict(protect: set[str])` with full `"<id>.<kind>"` keys (songs counted by id, as
  implemented), in-use markers. Stereo PCM at `<dir>/stereo.f32le`, binaural at
  `<dir>/binaural.f32le`; `decode_stereo` writes `meta.json` (the player later merges the loudness
  keys into it; `frames`/`channels` are left alone). `MediaCache` evicts on put; the player never
  calls it directly.
* `downmix.profiles() -> [{"key","label","kind",...}]`, `current() -> {"preset", ...}`,
  `apply(key)` (raises `DownmixError`). Any import/attribute failure -> the three matrix
  profiles from `FALLBACK_PROFILES` (+ "binaural" when `render.binaural_available()`).
* Frames of the atmos and stereo variants of the same song are assumed to share the timeline
  (both 48 kHz; a mode switch keeps the frame index). Encoder priming differences (a few ms)
  are ignored.

## Running

```
python3 -m unittest discover -s tests -p "test_output.py"     # ~40 s, fake paced consumer
python3 -m unittest discover -s tests -p "test_player.py"     # ~7 s, fakes for pipeline/output
python3 -m unittest discover -s tests -p "test_loudness.py"   # ~2 s
python3 -m unittest discover -s tests                         # everything
python3 -m amui play <id|url>... [--mode stereo] [--profile apple] [--volume 0.2] [--seek 30]
                                 [--sound-check off] [--dump-on-exit] [--pcm-cache var/tmp/pcm-x]
python3 -m amui output-test --seconds 8 --volume 0.05 [--stall 300]   # real sink: with consent only
tail -f var/logs/playback.log
```
