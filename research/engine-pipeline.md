# Media pipeline notes (acquire / cache / render / downmix) — 2026-11-08

Owner of: `amui/acquire.py`, `amui/cache.py`, `amui/render.py`, `amui/downmix.py`,
`amui/cli_pipeline.py`, `tests/test_cache.py`, `tests/test_acquire_parsing.py`,
`tests/test_render.py`. Consumers: `amui/player.py` and `amui/output.py` (other agent).

## What the pipeline does

```
catalog id ──acquire──► var/cache/media/<id>.<variant>.m4a ──render/decode──► var/cache/pcm/<id>.<kind>/
                        (decrypted E-AC-3 JOC or AAC-LC)                       atmos-916.f32le + render-summary.json
                                                                               stereo.f32le    + meta.json
```

* `acquire.acquire(id, MediaCache(), variant="lossless", …)` is a different route: `amui.lossless`
  asks the local FairPlay wrapper (`tools/lossless`, loopback) for the account's device playlist, picks
  the ALAC rendition (highest rate ≤ `config.LOSSLESS_MAX_RATE`, default 48000) and runs the Go
  primitive `var/build/lossless/bin/amui-fairplay-rip` (rendition URL on stdin; download + FairPlay
  decrypt + defragment + ALAC fix), then verifies with ffprobe (`alac`, 2 ch) and stores
  `<id>.lossless` with `sample_rate`/`bit_depth` in the meta. Failures are `LosslessUnavailable` /
  `NoLosslessError` (⊂ `AcquireError`); the player falls back to the AAC variant with a notice.
  `decode_stereo()` decodes ALAC as well (soxr resampling when the rate differs from 48 kHz).
* `acquire.acquire(id, MediaCache(), variant="atmos"|"stereo", progress, cancel)` mirrors the
  workspace's `music-web-probe.py` + `music-web-download.py` step for step (assets API → `_default`
  master → variant → leaf → DRM helper challenge → license POST → download → decrypt → ffprobe →
  `MediaCache.put`). Credentials only ever travel through `amui.web` (API + license hosts); manifests
  and media are fetched anonymously. Nothing sensitive is logged: exceptions carry fixed strings and
  integer status fields only; challenge/license strings are local variables deleted after use; the
  helper's stderr log (fixed messages from `main.rs`) lives in `var/tmp` while the helper runs and is
  deleted afterwards together with the encrypted file.
* Two playlist layouts (2026-11-13). Most songs: every stream sits in a named audio group
  (`audio-stereo-256`, `audio-atmos-…`, `audio-alac-…`), the stream playlist lists Apple's fixed
  prefetch key next to the song's key, and the media file has two sample descriptions: the first
  fragment uses the first one (fixed key), the rest the second (song key). A song Apple encoded only
  as AAC (catalog `audioTraits` = `lossy-stereo` alone): three streams without an `AUDIO` attribute
  (AAC-LC 256 and 128 kbps, HE-AAC 64 kbps; the peak `BANDWIDTH` of the 256 kbps stream is about
  302000), a stream playlist with the song's key alone, one sample description, every sample
  encrypted with the song's key. The `tenc` key ids are zero in both layouts, so the file does not
  say which key applies; the playlist does. `select_stereo` takes an ungrouped AAC stream when no
  stereo group exists (AAC-LC first), `parse_leaf` records whether the prefetch key is listed
  (`LeafInfo.prefetch`), and the helper is told (`"prefetch_key"`). Upstream's decrypt module picks
  the key by sample-description index alone, which turns the second layout into noise that still
  parses as an MP4; `tools/drm-helper/patches/0001-…` makes it use the song's key throughout when
  there is no prefetch key. The helper answers with `"protocol": 2` from that build on; an older
  build is refused for the second layout before a license is requested (`HELPER_OUT_OF_DATE`).
  The format badge of an ungrouped stream names Apple's encoding rate (256/128/64), not the peak.
* `acquire.has_atmos(id)` is manifest-only and caches its answer per process (`forget_atmos()` resets).
* `MediaCache` / `PcmCache` / `ArtCache`: 0600 files in 0700 dirs, `index.json` per cache (sizes,
  last access, media meta), LRU eviction (media/art by bytes, PCM by `keep` distinct songs — every
  variant of a kept song stays, `protect` keys count toward `keep`). One `RLock` per instance; the
  index is re-read when another process rewrote it (CLI next to the player).
* `render.RenderSession` runs `tools/render-music-916.sh` (bash → exec dotnet, so the Popen pid *is*
  the renderer) with stdout+stderr into `<out_dir>/render.log`. `frames_available()` is the PCM size
  / 64; `total_frames` is exact from the summary after completion, else `round(duration * 48000)`;
  `done` requires exit 0 *and* a validated summary (0 fallback blocks, 0 non-finite samples,
  bytes == frames*64 == file size); `error` is a short reason plus the last log line.
* `downmix` loads the workspace `tools/downmix.py` by path (verified side-effect free: constants and
  functions only, `main()` guarded) and drives its `apply` subcommand as a subprocess.

## Contract extensions (all compatible; stub signatures unchanged)

| Module | Addition | Why |
|---|---|---|
| cache | `MediaCache.entries()/total_bytes()/remove()`, `PcmCache.path()/pcm_path()/kind()/split_key()/entries()/remove()`, `ArtCache.evict()/flush()/entries()/total_bytes()/name_for()` | CLI listing, player housekeeping |
| acquire | `probe_info(path) -> dict` (duration_s, codec, profile, bit_rate, sample_rate, channels, container_bit_rate), `master_url_for`, `select_atmos`, `select_stereo`, `stream_variants`, `parse_leaf`/`LeafInfo`, `forget_atmos` | tests + the player's format badge |
| acquire | cancellation raises `web.Cancelled` (same class `web.download` raises); `web.WebError` subclasses propagate unchanged (AuthError → "reconnect") | one exception path for the player |
| render | `RenderSession(script=...)`, `.summary_path/.log_path/.returncode/.running/.started_at/.finished_at`; `load_summary()`, `validate_summary()`, `validate_pcm_file()`, `read_meta()`; `PcmFile.meta` | tests with fake scripts, PcmCache validation |
| render | `wait_for()` returns True when the frames are there **or the render completed** (file holds everything), False on failure/timeout | callers asking past the end |
| downmix | `current()` adds `"source"` ("workspace" or "am-ui"); `apply()` returns the applied state dict (+ `"output"`); `sink_node_id()`, `live_controls()`, `expected_controls()`, `effective_preamp()` | verification, UI display |

`MediaCache.meta()` for a fresh acquisition holds: catalog_id, variant, audio_group, codecs, bandwidth,
segments, encrypted_bytes, bytes, acquired_at, duration_s, codec, profile, bit_rate, sample_rate,
channels, container_bit_rate. `AcquireResult.variant` is `{audio_group, codecs, bandwidth}`.

### Downmix state and preamps

`downmix.py apply <preset> --preamp <dB>` sets the final preamp; the installed config uses
`saved preamp + LOUDNESS_OFFSET[preset]` when `match` is on (`params_for`). `downmix.apply()` adds
the same offset so a preset switch sounds like the bridge's. The workspace state file
(`pipewire/downmix-state.json`, owned by the user's bridge) is **never written**; am-ui records its
own last apply in `var/state/downmix.json` (0600) and `current()` reports whichever file is newer.

**Decision 2026-11-08: no Cavern binaural renderer.** "Apple binaural" means the existing `apple`
fold-down matrix preset; `profiles()` offers exactly `peqdb`, `apple`, `itu` (all kind "matrix").
Nothing was built under `tools/cavern-binaural/`. `PcmCache` still accepts the `binaural` kind from
the stub contract (harmless, tested) but nothing produces it.

## Verified 2026-11-08 (song F, an Atmos single from the test library; one live acquisition per variant)

| Step | Result |
|---|---|
| `atmos-check <song F>` | `atmos=True` in 10.8 s (most of it `web.developer_token()` fetching music.apple.com) |
| `acquire <song F>` | 1.3 s total: manifest 0.5 s, license 0.2 s, download 0.5 s, decrypt 0.1 s. 20,080,708 bytes, `audio-atmos-2768` ec-3 bandwidth 772001, ffprobe eac3 768 kbit/s 48 kHz 208.880 s. **sha256 identical to the 2026-11-05 fixture `atmos.private.m4a`.** |
| `acquire <song F> --stereo` | 1.0 s. 6,752,751 bytes, `audio-stereo-256` mp4a.40.2 bandwidth 270424, ffprobe aac LC 257,168 bit/s **44.1 kHz** 2 ch 208.926 s |
| re-run both | `from_cache: True`, 60 ms process time (Python start-up) |
| `render <song F>` | first PCM at t+0.5 s (1.0 s of audio), ~23 s of audio per second after that; done in 9.1 s wall, renderer 8.93 s = 23.4x real time; 10,026,240 frames, 641,679,360 bytes, JOC 15 / OAMD 16 objects, 313,320 object blocks, **0 fallback blocks, 0 non-finite**, peak 0.5115, no silent channel. **PCM bit-identical to the fixture render**, summary identical apart from timing. |
| `decode <song F>` | 0.3 s; 10,028,428 frames = 208.926 s at 48 kHz (resampled from 44.1 kHz), 80,227,424 bytes; 0.046 s longer than the Atmos render (AAC priming/padding) |
| `downmix --list` / `downmix` | peqdb / apple / itu; current `peqdb -9.0 dB match=True`, sink running |
| `downmix peqdb --preamp -9` (the saved state, a no-op) | `applied to node 38 (atmos_916_downmix)`; `pw-cli enum-params 38 Props` is byte-identical before and after (669 control strings); workspace state file untouched (mtime 2026-10-25); `var/state/downmix.json` written 0600 |
| unit tests | `tests/test_cache.py` 17, `tests/test_acquire_parsing.py` 19, `tests/test_render.py` 12: all pass (`python3 -m unittest discover -s tests -p "test_*.py"`) |

Observed `downmix.py apply` behaviour: it `pw-dump`s to find the node, runs one
`pw-cli set-param <id> Props '{ params = [ ... ] }'` with all 24 control values (gains, LFE
low-pass/Q, delays, LFE level, preamp), prints `applied to node …` plus the matrix, and exits 1 with
`node 'atmos_916_downmix' not found - is the downmix loaded?` when the sink is absent. It does not
touch the installed config, the state file or the EQ stage, and nothing restarts.

## Things the player should know

* First play of a cached song: `MediaCache.get()` → `PcmCache.complete()` → `CompleteRender`;
  otherwise `PcmCache.claim()` → `RenderSession.start()` → `wait_for(2 s * 48000, …)`: the first
  MiB (16,384 frames = 0.34 s) appears ~0.5 s after start, 2 s of audio after ~0.6 s.
* Stereo PCM from `decode_stereo` is 48 kHz 2-channel regardless of the AAC's 44.1 kHz.
* `has_atmos()` + `acquire()` share the per-process answer cache; `acquire(variant="atmos")`
  raising `NoAtmosError` records False.
* Keep the current and next keys in `PcmCache.evict(protect=…)`; `claim()` deletes a stale dir of the
  same key, so stop a running `RenderSession` for that key before claiming it again.

## Open issues

* `config.ensure_dirs()` creates `var/cache` implicitly (0755) because `mkdir(parents=True)` applies
  the mode to the leaf only; the leaves (`media/pcm/art`) are 0700. The config owner may want to add
  `VAR / "cache"` to the list. Other agents' scratch files in `var/tmp` are 0644 (not this pipeline's).
* `web.developer_token()` costs ~10 s on a cold process (fetching the web player's JS); the player
  should warm it once at start-up so the first acquisition is fast.
* Multi-process index safety is best effort (mtime re-read); the CLI is a debugging tool, the player
  is the single writer in normal use.
