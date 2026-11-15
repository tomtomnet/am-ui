# workspace/ — engine tools and this machine's private engine state

The engine (`amui/`) loads a handful of helper tools by path and lets them keep their state next
to themselves. This folder holds both: tracked sources, and git-ignored state that every machine
creates for itself.

Tracked (the same on every machine):

| Path | What it is |
| --- | --- |
| `tools/music-web-probe.py` | HTTP rules (host allowlist, credential scoping), token reading, HLS helpers |
| `tools/music_session.py` | saved-session status |
| `tools/music-web-login.py`, `tools/music_browser.py` | the browser sign-in (a browser window you see) |
| `tools/downmix.py` | the 9.1.6 → stereo downmix: presets, config generator, live controls |
| `pipewire/` | the sink: its own PipeWire client (`applemusic-downmix.conf`, `.service`) and the example filter chain |
| `builds/music-downloader/amdl-linux/` | the DRM helper's harness (a few dozen lines of Rust over Kyant0/AMDL's modules) |

Created on each machine, never committed (`.gitignore` ignores everything here that is not listed
above, and `tests/test_repo_hygiene.py` checks it):

| Path | Made by |
| --- | --- |
| `builds/music-downloader/web-session/` | the browser sign-in: its saved Apple Music session (0700, token 0600) |
| `builds/music-downloader/amdl-android-src/` | `tools/drm-helper/build.sh`: Kyant0/AMDL at the pinned commit, left as fetched |
| `builds/music-downloader/amdl-patched/` | the same script: the decrypt module with `tools/drm-helper/patches/` applied, which the harness compiles |
| `builds/music-downloader/amdl-linux-target/` | the built helper `release/amdl-linux-probe`, and next to it `amdl-linux-probe.inputs`: a fingerprint of the sources it was built from, so `./setup.sh` builds again when they change |
| `builds/mb-tmp/`, `reports/` | scratch and sign-in reports of the tools |
| `pipewire/downmix-state.json` | this machine's downmix settings |

`./setup.sh` at the repository root builds what has to be built. A sign-in is never copied
between machines: sign in with Connect on each one (the app's default sign-in keeps its state in
`var/wrapper`, not here; this folder holds the browser sign-in).

A different folder with the same layout can be used with `AMUI_WORKSPACE=/path`. An existing
`~/Documents/apple-music-gpu` that holds these tools is preferred automatically, so a machine that
already has a saved session there keeps using it. The DRM helper is the exception: the one built
here is used whenever it exists, whichever workspace is active, because the engine and that build
have to agree on the messages they exchange.

Licence note: the helper binary links Kyant0/AMDL's Rust modules (GPL-3.0), fetched at build time
and not stored in this repository.
