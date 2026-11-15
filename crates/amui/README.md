# amui: the engine

The engine of am-ui as a Rust library: Apple Music library and catalog access, song acquisition
(Dolby Atmos, AAC, lossless), rendering, the in-app downmix, the output and the playback state
machine. It replaced the Python package `amui/` of the first version of am-ui module by module
and behaves like it: same requests, same files, same messages. That version is not part of this
repository's history; where comments and the conventions below name its modules (`amui/api.py`,
...) or tests, they describe what the port follows. Its notes on Apple's endpoints and the
pipeline are in `research/engine-*.md`. No user interface code lives here.

## Modules

| module | what |
|---|---|
| `config` | every path and constant; `Config::from_process()` at start-up, `Config::new()` in tests |
| `types` | plain data: `Track`, `Album`, `Playlist`, `Artist`, `Page`, `Lyrics`, `PlaybackSnapshot`... |
| `util` | private files, state-file JSON, `Event` (a waitable flag, also used for cancelling), URL quoting |
| `web` | the HTTP layer with the credential rules; `Http` is what the other modules ask for |
| `api`, `lyrics` | Apple Music web API client (library, catalog, search, Home/New/Radio shelves, stations, Autoplay's station); TTML lyrics |
| `cache` | decrypted media, PCM renders and artwork on disk |
| `acquire`, `lossless` | song -> decrypted file (Widevine helper; FairPlay wrapper for ALAC) |
| `render`, `loudness`, `output` | Cavern render sessions, loudness measurement, the PipeWire feeder |
| `dsp` | the downmix of 16 channels to stereo and the headphone equalizer, computed in the engine (bit-identical to PipeWire's filter chain) |
| `eq`, `downmix` | the equalizer's presets and the downmix profiles; today still applied to the shared downmix sink |
| `session` | sign-in status and the native sign-in |
| `wrapper` | the lossless route's control helper (stage, start, stop, status, the native sign-in); `amui wrapper` and `am-ui --wrapper` run it |
| `player` | queue and playback state machine: prefetch, gapless hand-over, Sound Check, Autoplay, stations, current versions of withdrawn songs |
| `doctor`, `cli` | what this machine has and lacks; the `amui` command line (`amui help`) |

## Conventions

- **Same behaviour as the Python module.** A port keeps the Python module's public functions and
  classes (snake_case names stay), its constants, its file names and formats under the data
  folder, its permissions (0600 files in 0700 folders), its timeouts and every message a user or a
  log can see. Both implementations must be able to work on the same data folder. A deliberate
  difference is written down in the module's top comment.
- **No globals, no environment.** Everything that needs a path takes an `Arc<Config>` (or
  `&Config`); only `Config::from_process()` reads the environment. Tests build a `Config` over a
  scratch folder, so they can run in parallel.
- **Network only through `web::Http`.** Modules take an `Arc<dyn Http>` (or `&dyn Http`); tests
  pass their own implementation. Nothing else opens connections to Apple. The loopback lossless
  service is the one exception (`lossless`).
- **Secrets.** The music user token is read inside `web` at request time and goes into the Cookie
  header of Apple's API and license hosts, nowhere else. Tokens, signed URLs, PSSH/challenge/
  license values, keys and response bodies never appear in logs, errors, `Debug` output or test
  fixtures. Errors name the host and the HTTP status. Lyrics text and song titles stay out of logs.
- **Errors.** One `thiserror` enum per module, named after the Python exceptions
  (`WebError`, `AcquireError`...), with the same texts. No `unwrap()` on anything that can fail at
  run time; `expect()` only for invariants.
- **Blocking code and threads**, as in Python: no async runtime. Cancellation and "wait until" use
  `util::Event` in an `Arc`. Helper programs (`ffmpeg`, `ffprobe`, the DRM helper, the renderer,
  `pw-cat`, `pw-cli`) are started with `std::process::Command`, never through a shell.
- **Seams instead of patching.** Where the Python tests replace a function (`mock.patch`), the
  Rust code takes that dependency as a trait object, a closure or a parameter, so the test can
  pass its own.
- **Tests.** Every Python test has a Rust test with the same name (without the `test_` prefix)
  and the same assertions, next to the code (`#[cfg(test)] mod tests`) or under `tests/`.
  Fixtures are synthetic: placeholder names ("Song A"), made-up ids ("1234567890"), no real
  titles, ids, storefronts or account details. Tests never touch the network, a real token, a
  real sink or the real data folder.
- **Style.** `cargo fmt` (see `rustfmt.toml`), `cargo clippy --all-targets` without warnings,
  doc comments that say what the Python docstrings said. Dependencies are declared in the
  workspace `Cargo.toml`; a new one needs a reason.

## Checks

```sh
cargo test -p amui
cargo clippy -p amui --all-targets
```
