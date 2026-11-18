# am-ui for developers and AI agents

The README is for people who use am-ui. This file is the rest: how the program is put together,
how to build and test it, where everything lives, and the rules every change follows. It is
written for human developers and AI coding agents alike; agents should read it whole before
changing anything.

More detail, closer to the code:

- `crates/amui/README.md`: the engine's modules and conventions
- `crates/am-ui/README.md`: the application's structure, its rules (text size, threads, widgets)
  and the options of the headless screenshots
- `workspace/README.md`: the vendored engine tools
- `research/`: the investigations behind the renderer, the lossless route and the API (index below)

## Rules

These hold for every change, by a person or an agent.

**Credentials and the account**

- The music user token, Apple's developer token, the Apple ID password, verification codes, signed
  URLs, PSSH data, license challenges and responses, and keys are never printed, logged, copied
  into notes or committed. Code that handles them keeps them in memory and redacts them from errors.
- Credentials are sent only to `amp-api.music.apple.com` and `play.itunes.apple.com`, over HTTPS,
  never on redirects. CDN, artwork and manifest requests are anonymous. `crates/amui/src/web.rs`
  enforces this; keep every new request inside it.
- A sign-in is the account owner's action. An agent never starts an Apple sign-in, never triggers a
  verification code, and never runs `tools/lossless/login.sh`, `wrapper login` or
  `./am-ui cli connect` on someone's account.
- The lossless service of a working install (`var/wrapper`, port 12351) is left alone: no
  `stop`, `start`, `stage`, `build` or login against it while it runs. Experiments use a copy:
  `AMUI_VAR=<other folder>` and `LOSSLESS_PORT=<other port>`.
- Read-only requests are fine for development; anything that changes the account (library,
  playlists, ratings) is done only when the owner asks for it.

**The developer's machine**

- The window is looked at headlessly, never on the developer's display:
  `crates/am-ui/tools/headless.sh` runs it in a private compositor with its own D-Bus session.
- Tests that play audio run against a private audio server, never the desktop's:
  `crates/amui/tools/private-audio.sh` starts a PipeWire daemon with a null sink only.
- Nothing changes the system's audio setup (PipeWire configuration, sinks, volumes, mute states).
- Processes you did not start are not stopped. A running am-ui (from `target/release`) keeps its
  binary: rebuilding replaces the file, the running program is unaffected.
- `tools/cavern-lab/cavern-src/Cavern.Format/Decoders/EnhancedAC3/ObjectAudioElementMetadata.cs`
  carries a header that forbids AI agents to read or work with it. Agents do not open, quote or
  edit it (it is kept verbatim because the library does not compile without it); searches exclude
  it (`grep --exclude=ObjectAudioElementMetadata.cs`).

**The repository**

- Tracked files and commit messages carry no personal data: no song titles, artist names, ids or
  counts from someone's library; no account details (Apple ID, storefront, country); no machine
  details (hardware, IP addresses, home paths, the developer's tools or system). Use placeholders:
  "song A", ids like 1234567890, invented names in the demo data. This holds for notes, logs and
  measurements too: a benchmark or a log is about a machine, so it stays out of the repository.
  Look through every file that is about to be pushed, not only the code. `var/` and `notes-local/`
  are never committed; `crates/amui/tests/repo_hygiene.rs` checks the ignore rules.
- Commit messages describe the change to the code, neutrally: what changed and why, not who asked
  or what was reported. When an AI agent wrote the change, end the message with a
  `Co-Authored-By:` line for it.
- Dolby Atmos is never faked: the format badge shows what plays. A song without an Atmos version
  plays its 2-channel version or is skipped, by the user's setting, never silently.

## What it is

```
your Apple Music sign-in ─► Apple Music web API (library, catalog, search, lyrics, radio)
                         │
                         ▼  /v1/play/assets (enhanced HLS)
                 Atmos variant (E-AC-3 JOC 768 kbps)      or      stereo variant (AAC 256)
                         │ Widevine license (local Rust helper)        │
                         ▼                                            ▼
                 decrypted .m4a  ─► Cavern 9.1.6 render ─► 16-ch f32 ─► downmix + EQ ─► stereo stream
                                    (progressive, ~25× real time)          (in the app)   (PipeWire / PulseAudio)
                                                         ffmpeg ─► 2-ch f32 ─┘
local FairPlay wrapper (tools/lossless, the same sign-in) ─► ALAC 24-bit ──┘  "Lossless"
```

Two Rust crates in one Cargo workspace:

- **`crates/amui`**, the engine (library and program `amui`): Apple Music's web API, the session,
  song acquisition (Widevine route for Dolby Atmos and AAC, FairPlay wrapper route for ALAC), the
  caches, the Cavern render, decoding, loudness, the in-app downmix and equalizer, the audio
  output, and the player (queue, Autoplay, stations). No user interface.
- **`crates/am-ui`**, the GTK 4 / libadwaita application (program `am-ui`). It talks to the
  engine through four traits (`data::Music`, `playback::Playback`, `account::AccountService`, the
  equalizer service), implemented by adapters in `crates/am-ui/src/engine/` and, for `--demo`, by
  invented data (`demo.rs`, `demo_playback.rs`).

External programs, built by `setup.sh` and run by path:

- the patched **Cavern** renderer (.NET), `tools/cavern-lab/`
- the **DRM helper** (Rust, from Kyant0/AMDL), `tools/drm-helper/`, built into
  `workspace/builds/music-downloader/amdl-linux-target/`
- the **lossless route**: WorldObservationLog's wrapper running Apple's Android libraries, and
  `amui-fairplay-rip` (Go, from zhaarey/apple-music-downloader), `tools/lossless/`
- **ffmpeg** and **ffprobe** from the system

## Layout

| path | what |
|---|---|
| `crates/amui/src/` | the engine: `config` (paths, settings), `types`, `util`, `web` (HTTP with the credential rules), `api` and `api/discover.rs` (web API; Home, New, Radio, stations), `lyrics` (TTML), `session` (sign-in state, native sign-in), `cache` (media, PCM, art), `acquire` (Widevine route), `lossless` and `wrapper` (FairPlay route and its service), `render` (Cavern), `loudness`, `downmix`, `dsp`, `eq`, `output` (the stream), `player` (queue, Autoplay, stations, prefetch), `doctor`, `cli` |
| `crates/amui/tests/` | integration tests; `reference/` and `fixtures/` (values computed by the first, Python version, see `reference/README.md`); `live.rs` (hand-run checks against Apple, `#[ignore]`) |
| `crates/amui/examples/downmix_file.rs` | runs the app's downmix over a 16-channel render file |
| `crates/amui/tools/private-audio.sh` | a private audio server for tests |
| `crates/am-ui/src/` | the application: `app.rs` (start, options, shared context), `window.rs`, `nav.rs`, `pages/`, `widgets/`, `engine/` (adapters), `library.rs` (lists and their snapshots), `finder.rs` (library search), `sorting.rs`, `mpris.rs`, `demo*.rs`, `style.css` |
| `crates/am-ui/tools/headless.sh` | the window in a private headless compositor (mutter, or KWin's virtual backend) |
| `crates/am-ui/tools/fake-input.*` | a real pointer and keyboard for headless runs under KWin |
| `crates/am-ui/resources/icons/` | every icon the window shows (`am-*-symbolic`) and the application's icon, so that no desktop theme can change them |
| `crates/am-ui/data/` | the desktop entry's template |
| `crates/am-ui/demo/` | the demonstration's invented albums and lyrics; `gen_demo.py` makes the covers (git-ignored) |
| `tools/cavern-lab/` | the patched Cavern copy (`cavern-src`), the probe that renders (`probe/`), `build.sh`, `render-music-916-lab.sh`, analysis scripts (`analyze.py`, `bands.py`, ...) |
| `tools/drm-helper/` | `build.sh` and `patches/` for the Widevine helper |
| `tools/lossless/` | the wrapper route: `build.sh`, `wrapperctl.py`, `login.sh`, `start.sh`/`stop.sh`/`status.sh`, patches, `go/amui-fairplay-rip` |
| `workspace/` | vendored engine tool sources; everything else there is a machine's own (ignored) |
| `research/` | notes (index below) |
| `setup.sh`, `am-ui` | set-up script, launcher |
| `var/` | runtime data, git-ignored (below) |
| `notes-local/` | private notes of a machine, git-ignored |

## Build, run, test

Rust 1.88 or later (edition 2024). After the first build has fetched the crates, everything works
offline.

```sh
cargo build --release                      # target/release/am-ui and target/release/amui
cargo test --offline --workspace           # engine and application tests
cargo fmt --all                            # rustfmt.toml; keep `cargo fmt --check` clean
cargo clippy --offline --workspace
./am-ui --demo                             # the window with invented data and a silent player
./am-ui cli help                           # the engine's command line
```

The About dialog shows the commit a build was made from (`+`: with uncommitted changes).

- The engine's tests use stand-ins for the network, the helpers and the audio server; nothing
  contacts Apple. `cargo test -p amui --test live -- --ignored` runs hand-made checks against the
  real service with the saved sign-in: read-only, and they print shapes, never values.
- Audio tests that need a real server: `crates/amui/tools/private-audio.sh cargo test -p amui
  --offline --test output_audio -- --ignored --test-threads 1`.
- Screenshots: `crates/am-ui/tools/headless.sh [-s 3840x2160] target/release/am-ui --demo --page
  albums --size 1400x900 --screenshot /tmp/shot.png`. Options for pages, dialogs, the player, text
  scale (`--text-scale 1.5`) and the demo's timing are listed in `crates/am-ui/README.md`. The
  private compositor follows the desktop's scaling factor; `GSETTINGS_BACKEND=memory` in front gives
  scale 1.
- Without `--demo` the window runs on the real engine and the saved sign-in; it refuses the options
  that would play (`--playing`, `--song`, ...).
- `./setup.sh` can be rehearsed as on a fresh machine without touching the real install: clone into
  a folder of its own, then run `setup.sh` there under `bwrap` with an empty home folder, an empty
  runtime folder and `LOSSLESS_PORT` set to a free port (`--no-lossless` skips the long part).

## setup.sh

Steps, each of which looks first and does only what is missing:

1. **System programs and libraries**, checked, never installed: git, curl, a C compiler,
   pkg-config, patch, ffmpeg and ffprobe, python3, the development files of GTK 4.20+,
   libadwaita 1.8+ and the PulseAudio client library; for the lossless route also cmake, ninja,
   a C++ compiler and unzip. Missing packages are named with the install command of the
   distribution (dnf, apt, pacman, zypper).
2. **Rust**: cargo 1.88+ from `AMUI_CARGO`, `var/toolchains` or the PATH, else a private
   toolchain from rustup into `var/toolchains`.
3. **The application**: `cargo build --release --locked -p am-ui -p amui`. With the system's cargo,
   crate downloads go to `var/build/cargo-home` unless `CARGO_HOME` is set.
4. **.NET SDK 8+** from `AMUI_DOTNET`, the PATH or an existing workspace, else a private one in
   `var/toolchains/dotnet`.
5. **The renderer**: `tools/cavern-lab/build.sh`.
6. **The DRM helper**: `tools/drm-helper/build.sh` fetches a pinned commit of Kyant0/AMDL, checks
   the SHA-256 of the files it uses, applies `patches/` to a copy and builds. A fingerprint of its
   inputs decides whether it is built again (`build.sh --check`: 0 current, 1 absent, 2 stale).
7. **The lossless route** (x86_64 only): `tools/lossless/build.sh`, about 1 GB of downloads (pinned
   commits, checksummed toolchains: Android NDK, Go) and 3 GB on disk. Built again only after
   `tools/lossless` changed; while its service runs an update waits.
8. **The desktop entry**: `dev.amui.Music.desktop` and its icon under `~/.local/share`
   (`applications/`, `icons/hicolor/scalable/apps/`), starting this folder's `./am-ui`. The file name
   is the application id, by which desktops match the window to the entry.
9. **The doctor**: `target/release/amui doctor`.

Options: `--check` (step 1 and the doctor, nothing else), `--no-lossless` (skip step 7; the app then
cannot sign in by itself), `--no-desktop-entry` (skip step 8), `--venv` and `--no-sink` (accepted for
older instructions, no effect). Everything else stays inside the repository folder (`var/`,
`workspace/`, `target/`); `sudo` is never run.

## Environment variables

| variable | effect |
|---|---|
| `AMUI_VAR` | the runtime data folder (default `var/`) |
| `AMUI_ROOT` | the repository folder, for a program started from elsewhere |
| `AMUI_WORKSPACE` | another folder with the engine tools (same layout as `workspace/`); ignored when it lacks them. An existing `~/Documents/apple-music-gpu` holding them is used too |
| `AMUI_CARGO`, `AMUI_DOTNET` | the `cargo` or `dotnet` the build scripts use |
| `AMUI_RENDER_SCRIPT` | another render script instead of the patched Cavern |
| `CAVERN_LAB_OPTIONS` | the renderer's switches instead of the engine's (see The renderer) |
| `AMUI_LOSSLESS_MAX_RATE` | the highest ALAC sample rate fetched (48000 by default; the output runs at 48 kHz) |
| `AMUI_SIGNIN=browser` | prefer a saved browser session over the native sign-in |
| `AMUI_LANGUAGE` | the catalog's language (default: the desktop's `LC_ALL`, `LC_MESSAGES`, `LANG`) |
| `LOSSLESS_PORT`, `LOSSLESS_DNS`, `LOSSLESS_CA_BUNDLE` | the wrapper service's port (12351), resolvers and CA bundle |
| `AMUI_DEMO_*`, `AMUI_SHOT_DELAY_MS` | the demo's timing and size, the screenshot's moment (`crates/am-ui/README.md`) |

## Runtime data: `var/` (mode 0700)

| path | what |
|---|---|
| `cache/media/<id>.atmos.m4a`, `.stereo.m4a`, `.lossless.m4a` | decrypted songs, least recently used go first beyond 2 GiB; `index.json` |
| `cache/pcm/<id>.<kind>/` | renders and decodes (`atmos-916.f32le` with `render-summary.json`, or 2-channel PCM); the current, next and previous are kept |
| `cache/art/` | artwork, 256 MiB |
| `state/player.json` | the player's settings (mode, profile, volume, fallback, Sound Check, lossless, autoplay) |
| `state/downmix.json`, `state/eq.json` | the downmix profile with its preamp, the equalizer |
| `state/library/*.json` | snapshots of the library's lists (0600), shown at start-up |
| `state/equivalents.json` | current catalog versions of withdrawn songs (30 days) |
| `state/developer-token.json`, `state/api/` | Apple's public developer token, the account's storefront |
| `state/android-session.json`, `state/ui-settings.json` | sign-in status, window settings |
| `logs/playback.log` | loads, renders, seeks, the output's buffer and timing (rotated at 1 MiB) |
| `logs/output.log`, `logs/downmix.log` | the audio stream's own log, every change of the downmix |
| `wrapper/` | the lossless service's runtime and the sign-in (0700) |
| `build/`, `toolchains/`, `tmp/` | build trees, private toolchains, scratch |

## Sign-in

- **Sign In** (the banner above the player bar, the sidebar's menu) opens a sheet: Apple ID and
  password, then the six-digit code a trusted device shows. This is Apple's native sign-in, run by
  Apple's Android libraries inside the wrapper (`amui wrapper login --pipe`, JSON lines on stdin and
  stdout). `./am-ui cli connect` and `tools/lossless/login.sh` do the same in a terminal.
- The password travels through a pipe and an owner-only file that is removed as soon as the wrapper
  has read it; never argv, logs or settings.
- Each attempt answers Apple once: a refused password or code ends it (Apple locks accounts after a
  few wrong passwords). A failed or cancelled attempt keeps the previous sign-in. While an attempt
  runs, the service is stopped; the next lossless song starts it again.
- A failed attempt leaves `var/wrapper/logs/signin-last.log` (owner-only; Apple IDs, e-mail
  addresses, token-like strings and every line containing typed input left out; removed by the
  next successful sign-in).
- The wrapper runs in a private user namespace, which some systems refuse. `./am-ui cli wrapper
  selftest` checks it without an account or the network; the sheet, `setup.sh` and the doctor do too.
- The engine reads one file of the sign-in, the music user token, to send it to the two Apple hosts;
  the others are only checked for presence. A browser session saved by the first version
  (`workspace/builds/music-downloader/web-session/`) is still read when there is no native sign-in.
- Signing out: quit the app, then `tools/lossless/stop.sh && rm -rf var/wrapper/rootfs/data`.

## Playback

**Acquisition, Widevine route** (`acquire.rs`): `/v1/play/assets` gives the song's enhanced HLS
master; the Atmos variant (ec-3, 768 kbps) or the AAC one is chosen; the DRM helper makes the
license challenge, the license comes from `play.itunes.apple.com`, the file is downloaded whole and
decrypted, checked with ffprobe and cached. A song whose audio traits name no Atmos (or no
lossless) version goes straight to the other kind; a library song carries no traits and is asked
once per session.

**Withdrawn songs**: a library song whose catalog version Apple withdrew is refused with code 40304
("not entitled"); the player looks up the current version (same title, artist and duration) and
plays that, remembered in `state/equivalents.json`.

**The renderer** (`render.rs`, `tools/cavern-lab`): the probe decodes E-AC-3 JOC and renders the
objects to 16 channels (FL FR FC LFE SL SR RL RR FLW FRW TFL TFR TSL TSR TRL TRR, f32, 48 kHz),
progressively, about 25 times real time once warm. The engine's switches (`config::LAB_FIXES`):

- `--surround-phase -1`: Apple's streams use JOC downmix configuration 3, a 90° phase shift on the
  surround channels that stock Cavern ignores; without the correction a quadrature residue of the
  mix lands in the front channels.
- `--joc-timeslot-fix`, `--joc-interp-fix`: the JOC applier's timeslot wrap and the matrix
  interpolation of ETSI TS 103 420 clause 6.6.5.
- `--lfe-delay 577 --lfe-gain 1.41421356`: Cavern takes the LFE straight from the decoder while the
  other channels pass the QMF bank of the JOC reconstruction (577 samples) and gives it the objects'
  -3 dB; Apple keeps it aligned and at the core's level.

Against captures of Apple's own render through the same fold-down, the result correlates at 0.996
with band levels within half a decibel (method and numbers: `research/cavern-artifact.md`,
sections 6 and 9). Every render records its generation (`cavern-lab-2`); a different one is rendered
again. One render process owns its folder under `cache/pcm/`: nothing else may delete or reuse the
folder while it runs. Playback of a render starts once one second is out and the renderer runs at
three times real time or more.

**The lossless route** (`lossless.rs`, `wrapper.rs`, `tools/lossless`): Apple refuses Widevine
licenses for ALAC, so lossless comes through WorldObservationLog's wrapper "lite": Apple's Android
libraries, rootless in a user namespace, bound to 127.0.0.1, TLS verified inside Apple's
libraries, the host's DNS (security review and protocol: `research/lossless-route.md`). Per song the
service hands out the account's device playlist; the engine picks the ALAC rendition and runs
`amui-fairplay-rip`, which downloads the fMP4, decrypts it through the service's FairPlay key
templates and writes a plain `.m4a`, verified with ffprobe before it is cached as
`<id>.lossless.m4a`. The app starts the service with the first lossless song and stops it at exit;
while it runs it holds the account's playback lease. `amui wrapper` (the app runs itself as
`am-ui --wrapper`) controls the service: `paths`, `fingerprint`, `build-state`, `stage`, `start`,
`stop`, `status`, `selftest`, `login`; `tools/lossless/wrapperctl.py` is the same in Python, used by
the build scripts.

**Decoding**: AAC and ALAC through ffmpeg into 2-channel f32 PCM.

**Sound Check** (`loudness.rs`): BS.1770 integrated loudness (ffmpeg's `ebur128` while the PCM
grows; a Rust meter for complete files, the same numbers), 16-channel renders measured through the
PEQdB fold; songs are attenuated toward -16 LUFS, never boosted. A song starts at the estimate from
its first second; later estimates are ramped in over 1.5 s.

**Downmix and equalizer** (`dsp.rs`, `downmix.rs`, `eq.rs`): the 16 channels become stereo in the
app, with the arithmetic of the PipeWire filter chain the first version used (32-bit floats,
cookbook biquads, the mixers' summing order, integer delays), bit-identical to PipeWire 1.6.11's
code (`crates/amui/tests/reference/pipewire_chain.c`, rebuilt by `pipewire_chain.sh`). Per side:
eight channels times their gains, a delay matching the LFE low-pass, the LFE through a 4th-order
low-pass, preamp, then the equalizer's shelves and peaks. Profiles: PEQdB (every channel at 1.0,
centre -3 dB, LFE +7.1 dB after a 125 Hz low-pass), Apple fold-down (macOS's, measured: everything
but the front pair at -3 dB, no LFE), Default (ITU). Changes are cross-faded over 960 frames.
`cargo run --release -p amui --example downmix_file -- IN OUT [PROFILE]` runs the chain over a render.

**Output** (`output/`): an ordinary stereo stream through the PulseAudio API (served by
pipewire-pulse on PipeWire systems) with about 200 ms of buffer, corked while paused or idle; seeks
rewrite what the server holds instead of flushing; the feeder thread gets real-time priority
through RealtimeKit when allowed. `Player::dump_recent()` writes the last 30 seconds sent as a WAV.

**Player** (`player/`): one worker thread owns the queue (shuffle, repeat), prefetches the next
song, swaps versions in place when Dolby Atmos, Lossless or the profile changes, and runs
**Autoplay** as Apple's apps do: a continuous station seeded with the last ten songs keeps ten
songs ahead, and waits while more than fifty queued songs are still to play. Stations play through
`next-tracks`. Requests of Apple's web player that the engine follows: `research/engine-api.md` and
the notes of 2026-11-14 summarised in `research/engine-playback.md`.

## The application

- Pages: Home, New and Radio (shelves from `api/discover.rs`), Recently Added, Artists, Albums,
  Songs, Playlists (sorting, grid or list), album, playlist and artist pages, Search (the library's
  matches at once from memory, `finder.rs`, Apple Music's below them), Now Playing (lyrics with the
  sung word growing, `widgets/lyrics.rs`; the artwork's glow, `widgets/glow.rs`), the queue with its
  Autoplay switch, the Sound menu, the equalizer, Preferences (catalog region), the sign-in sheet.
- Song menus: play next or later, favourite, add to library or a playlist, Go to Album, Go to
  Artist, Go to Song (its place in the library's Songs), remove from a playlist.
- The library's lists are kept on disk (`var/state/library/`) and shown at once at start-up while
  fresh ones load.
- Pages fade in when opened or closed; switching sections is instant.
- Rows, cards and lists measure themselves in their fonts, so everything follows the desktop's text
  size and display scale (the "Text size" rule in `crates/am-ui/README.md`).
- MPRIS gives the desktop's media keys and controls.

## Engine command line

`./am-ui cli <command>` (or `target/release/amui`):

| command | what |
|---|---|
| `doctor` | what this machine has and lacks |
| `status`, `check` | the saved sign-in (offline), a live read-only check |
| `connect` | Apple's native sign-in in the terminal |
| `albums`, `artists`, `songs`, `playlists`, `recently-added`, `tracks KEY` | the library |
| `search TERM`, `song ID`, `lyrics ID`, `recent`, `home`, `charts` | the catalog |
| `atmos-check ID` | whether Apple offers Dolby Atmos for a song (manifest only) |
| `acquire ID [--stereo\|--lossless]` | license, download and decrypt into the media cache |
| `lossless [--start\|--stop\|--check ID]` | the ALAC route's state, a song's renditions |
| `render ID`, `decode ID` | render or decode a cached song |
| `downmix [KEY] [--preamp DB] [--list]` | the downmix profiles |
| `cache [--evict]` | what is cached |
| `wrapper SUBCOMMAND` | the lossless service (see Playback) |

## Security model in short

- Credentials go to two Apple hosts only, never on redirects; only an allowlist of Apple hosts is
  contacted, HTTPS with normal certificate checks; secrets never reach a log.
- Passwords and codes travel through pipes and a short-lived owner-only file, never argv.
- Everything private stays in `var/` (owner-only) and out of git.
- The wrapper listens on loopback only; its account files are owner-only; signed URLs stay in
  memory and the decrypt helper redacts URLs from its errors.

## Research notes

| file | about |
|---|---|
| `research/cavern-artifact.md` | the renderer's investigation: the Atmos burst, the surround phase, JOC bugs, the LFE alignment, how renders are compared with Apple's |
| `research/cavern-upstream-notes.md` | what to report upstream to Cavern |
| `research/lossless-route.md` | the wrapper: security review, protocol, build |
| `research/engine-api.md`, `engine-pipeline.md`, `engine-playback.md`, `engine-eq.md` | the first version's engine notes: Apple's endpoints, the pipeline, playback, the equalizer |
| `research/apple-web-ui/` | music.apple.com's interface, read from its public assets |

## Open work

- Start time of a song that is not cached (1 to 3 s): download while the license is being
  obtained; keep a renderer process warm between songs (the .NET start is about 1.4 s); remember
  which library songs have Dolby Atmos between sessions.
- A few sizes still in pixels: the equalizer graph's labels, the page headers' artwork, the shelves'
  scroll step at large text sizes.
