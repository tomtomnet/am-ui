# am-ui

**Apple Music on Linux, natively, with Dolby Atmos and lossless audio.**

am-ui is a native Apple Music player for GNOME, written in Rust with GTK 4 and libadwaita. It
renders Dolby Atmos on your own computer (no Mac needed) and plays lossless audio where Apple
offers it.

- Your library, Apple Music's catalog, search, radio stations and time-synced lyrics
- Dolby Atmos rendered to 9.1.6 and folded down to stereo
- Lossless (ALAC) or AAC, with a badge that always shows what is playing
- Autoplay, playlists, favourites, sorting, grid and list views
- Sound Check, an equalizer, downmix profiles, and your desktop's media keys

## Requirements

- 64-bit x86 Linux with GNOME 49 or later (GTK 4.20, libadwaita 1.8): for example Fedora 43,
  Ubuntu 25.10, Arch or openSUSE Tumbleweed
- PipeWire or PulseAudio
- An Apple Music subscription
- About 6 GB of disk space

## Install

```sh
git clone git@github.com:tomtomnet/am-ui.git
cd am-ui
./setup.sh
```

`setup.sh` checks your system and builds everything inside this folder; it never uses `sudo`. If
something is missing, it prints the command that installs it on your distribution: run it, then
`./setup.sh` again. On Fedora, all it can ask for is:

```sh
sudo dnf install gtk4-devel libadwaita-devel pulseaudio-libs-devel pkgconf-pkg-config gcc gcc-c++ \
    cargo dotnet-sdk-8.0 ffmpeg-free git curl patch python3 cmake ninja-build unzip
```

To update later: `git pull && ./setup.sh`.

## Use

```sh
./am-ui           # start the player
./am-ui --demo    # try the interface with made-up music, no account needed
```

The first time, press **Sign In** and enter your Apple ID, your password and the six-digit code
that Apple shows on your other devices. You sign in once per computer; your password is not kept.

The **Sound** menu of the player bar has Dolby Atmos on or off, Lossless, Sound Check, the downmix
profile (how Atmos becomes stereo), the preamp and the equalizer.

## How it works

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

For Dolby Atmos, am-ui decrypts the song, renders its sound objects to 9.1.6 channels with a
patched copy of the Cavern renderer, and mixes them down to stereo itself. Lossless songs come
through Apple's own Android libraries, which run in a small isolated service on your computer and
also handle the sign-in.

## Privacy

- Your account token is only ever sent to Apple's music servers, and never written to a log.
- Your password and verification code go straight to Apple's sign-in component and are not kept.
- Downloads, caches, settings and the sign-in stay in `var/`, readable by you only.

## Troubleshooting

- `./am-ui cli doctor` lists what this computer has and what it still lacks.
- `var/logs/playback.log` records what the player did, for example around a glitch.
- While am-ui plays lossless audio, the account counts it as a playing device, so music playing on
  another device of the same account may stop.
- To sign out: quit am-ui, then run `tools/lossless/stop.sh && rm -rf var/wrapper/rootfs/data`.

## For developers

How it is built, how to test it, the project's conventions and rules: see [AGENTS.md](AGENTS.md),
written for developers and AI coding agents alike.

## Credits

am-ui stands on the work of others:

- **[Cavern](https://github.com/VoidXH/Cavern)** by Bence Sgánetz ([en.sbence.hu](http://en.sbence.hu)):
  the Dolby Atmos decoder and renderer. A modified copy is included in `tools/cavern-lab/cavern-src`
  under the Cavern licence, free of charge.
- **[AMDL](https://github.com/Kyant0/AMDL)** by Kyant0: the Widevine helper for Dolby Atmos and AAC
  (its GPL-3.0 modules are fetched at build time, not stored here).
- **[wrapper](https://github.com/WorldObservationLog/wrapper)** by WorldObservationLog: Apple's
  Android libraries as a local service, for lossless audio and the sign-in.
- **[apple-music-downloader](https://github.com/zhaarey/apple-music-downloader)** by zhaarey: the
  lossless decryption code.
- [GTK](https://gtk.org), [libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/),
  [gtk-rs](https://gtk-rs.org), [FFmpeg](https://ffmpeg.org), [PipeWire](https://pipewire.org),
  [.NET](https://dotnet.microsoft.com), and the Rust crates listed in `Cargo.lock`.

**Built with AI.** This project relied heavily on AI: most of its code, research and documentation
were written by Anthropic's Claude (in Claude Code), directed, reviewed and tested by its author.

am-ui is an independent project, not affiliated with or endorsed by Apple or Dolby. Apple Music is
a trademark of Apple Inc. and Dolby Atmos a trademark of Dolby Laboratories. You need your own Apple
Music subscription.
