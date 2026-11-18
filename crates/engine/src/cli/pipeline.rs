//! `amui <command>`: acquisition, render, decode, downmix and cache commands
//! (`amui/cli_pipeline.py`).
//!
//! ```text
//! acquire <id|url> [--stereo|--lossless]   license, download and decrypt one song into the media cache
//! atmos-check <id|url>          manifest-only: does Apple advertise an ec-3 Atmos variant?
//! lossless [--start|--stop|--check SONG]   the ALAC route: wrapper build, sign-in, service, renditions
//! render <id> [--force]         Cavern 9.1.6 render of the cached Atmos file, progressive
//! decode <id> [--force] [--lossless]   ffmpeg decode of the cached stereo (AAC or ALAC) file to f32le
//! downmix [key] [--preamp dB] [--list] [--live]   show or switch the live stereo downmix
//! cache [--evict]               list the media/PCM/art caches
//! ```
//!
//! Output never contains URLs, keys or token values; progress and ffprobe facts only.
//!
//! The pure parts are public: [`song_id`], [`fmt_bytes`].

use std::time::Duration;

use serde_json::{Map, Value};

use crate::acquire::{AcquireError, Acquirer};
use crate::cache::{ArtCache, CacheError, MediaCache, PcmCache};
use crate::config::SAMPLE_RATE;
use crate::doctor;
use crate::downmix::{self, DownmixError, format_g, py_float, py_splitlines};
use crate::lossless::{LosslessError, alac_variants};
use crate::render::{CompleteRender, PcmFile, PcmSource, RenderError, RenderSession, os_error_name};
use crate::util::Event;
use crate::web::WebError;

use super::args::{Arg, Opt, Parsed, Spec};
use super::text::{float, local_minute, repr, shown, truth, truthy};
use super::{Command, Console, Context, PROGRAM};

// --- helpers ------------------------------------------------------------------

/// The catalog id a command line names: digits as they are, or the song of an Apple Music link
/// (`https://music.apple.com/…/song/<name>/<id>`, `…/album/<name>/<album id>?i=<id>`). An album or
/// playlist link names no song.
///
/// The Python command line took this from the workspace's probe module (`track_id`).
pub fn song_id(value: &str) -> Result<String, &'static str> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if digits(value) {
        return Ok(value.to_string());
    }
    let link = url::Url::parse(value).ok().filter(|link| {
        link.scheme() == "https" && matches!(link.host_str(), Some("music.apple.com" | "beta.music.apple.com"))
    });
    let Some(link) = link else { return Err("Use an Apple Music song link or numeric song ID") };
    let chosen = link.query_pairs().find(|(name, value)| name == "i" && !value.is_empty());
    let chosen = chosen.map(|(_, value)| value.into_owned()).or_else(|| {
        let parts: Vec<&str> = link.path().trim_matches('/').split('/').collect();
        (parts.len() >= 3 && parts[1] == "song").then(|| parts[parts.len() - 1].to_string())
    });
    match chosen {
        Some(id) if digits(&id) => Ok(id),
        _ => Err("Use a single song link, or an album link with an ?i= song selection"),
    }
}

/// A size in bytes or, from 1024 of a unit on, in the next binary unit with one decimal:
/// `512 B`, `1.5 KiB`, `3.2 MiB`, `2.0 GiB`. There is no unit above GiB.
pub fn fmt_bytes(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    let mut size = size as f64 / 1024.0;
    for unit in ["KiB", "MiB"] {
        if size < 1024.0 {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1} GiB")
}

/// Why a command of this module ended early.
enum Failure {
    /// An acquisition failed: the exit statuses of Python's `_handle`.
    Acquire(AcquireError),
    /// Anything else: `error: ...`, status 1.
    Other(String),
}

impl From<AcquireError> for Failure {
    fn from(error: AcquireError) -> Failure {
        Failure::Acquire(error)
    }
}

impl From<LosslessError> for Failure {
    fn from(error: LosslessError) -> Failure {
        Failure::Acquire(error.into())
    }
}

impl From<RenderError> for Failure {
    fn from(error: RenderError) -> Failure {
        Failure::Other(error.to_string())
    }
}

impl From<CacheError> for Failure {
    fn from(error: CacheError) -> Failure {
        Failure::Other(error.to_string())
    }
}

impl From<DownmixError> for Failure {
    fn from(error: DownmixError) -> Failure {
        Failure::Other(error.to_string())
    }
}

/// Run a command's body on its parsed arguments and print why it failed. No sign-in is status
/// 3, no Dolby Atmos 4, a cancelled acquisition 130, anything else 1 (Python's `_handle`).
fn run(
    spec: &Spec,
    args: &[String],
    console: &mut Console,
    body: impl FnOnce(&Parsed, &mut Console) -> Result<i32, Failure>,
) -> i32 {
    let parsed = match spec.parse(args) {
        Ok(parsed) => parsed,
        Err(stop) => return console.stop(&stop),
    };
    let (text, status) = match body(&parsed, console) {
        Ok(status) => return status,
        Err(Failure::Acquire(AcquireError::Web(error @ WebError::Auth { .. }))) => {
            (format!("not connected: {error}"), 3)
        }
        Err(Failure::Acquire(error @ AcquireError::NoAtmos(_))) => (format!("no Atmos: {error}"), 4),
        Err(Failure::Acquire(AcquireError::Web(WebError::Cancelled(_)))) => ("cancelled".to_string(), 130),
        Err(Failure::Acquire(error)) => (format!("error: {error}"), 1),
        Err(Failure::Other(error)) => (format!("error: {error}"), 1),
    };
    console.error(text);
    status
}

/// The song a command line names, or what is wrong with the word.
fn song(word: Option<&str>) -> Result<String, Failure> {
    song_id(word.unwrap_or("")).map_err(|problem| Failure::Other(problem.to_string()))
}

/// Create the private data folders.
fn data_folders(context: &Context) -> Result<(), Failure> {
    let failed = |error| Failure::Other(format!("the data folders could not be created ({})", os_error_name(&error)));
    context.config.ensure_dirs().map_err(failed)
}

fn acquirer(context: &Context) -> Acquirer {
    Acquirer::new(context.config.clone(), context.http.clone(), context.lossless.clone())
}

/// Frames as seconds of audio.
fn audio_seconds(frames: u64) -> f64 {
    frames as f64 / f64::from(SAMPLE_RATE)
}

/// How often a command that tidies up looks whether Ctrl+C was pressed.
const INTERRUPT_POLL: Duration = Duration::from_millis(50);

/// Do `work` while Ctrl+C is watched: once it is pressed, `cancel` is set, for the work to end
/// by itself.
fn until_interrupted<T>(context: &Context, cancel: &Event, work: impl FnOnce() -> T) -> T {
    /// Ends the watch when the work is over, however it ends.
    struct Over<'a>(&'a Event);
    impl Drop for Over<'_> {
        fn drop(&mut self) {
            self.0.set();
        }
    }
    let over = Event::new();
    context.interrupt.asked(); // from here on Ctrl+C is this command's
    std::thread::scope(|scope| {
        let watch = || {
            while !over.wait(Some(INTERRUPT_POLL)) {
                if context.interrupt.asked() {
                    cancel.set();
                    return;
                }
            }
        };
        // without the watch the first Ctrl+C goes unnoticed; the second one still ends the process
        let _ = std::thread::Builder::new().name("amui-interrupt".to_string()).spawn_scoped(scope, watch);
        let _over = Over(&over);
        work()
    })
}

const fn one_song(name: &'static str, description: &'static str, options: &'static [Opt]) -> Spec {
    const SONG: &[Arg] = &[Arg::one("song", "")];
    Spec { name, description, options, arguments: SONG, exclusive: &[] }
}

// --- acquire -------------------------------------------------------------------

/// What `acquire` prints while it works: a line for every phase it enters, and for the download
/// one per tenth.
struct Progress {
    phase: String,
    tenth: i64,
}

impl Progress {
    fn new() -> Progress {
        Progress { phase: String::new(), tenth: -1 }
    }

    /// The line for a report of the acquisition, when it is due one. `elapsed` (seconds since the
    /// start) is looked at only for a line that is printed.
    fn line(&mut self, phase: &str, fraction: Option<f64>, elapsed: impl Fn() -> f64) -> Option<String> {
        let line = match fraction {
            Some(fraction) if phase == "download" => {
                let tenth = (fraction * 10.0) as i64;
                if tenth == self.tenth && self.phase == phase {
                    return None;
                }
                self.tenth = tenth;
                Some(format!("  {phase:<9} {:5.1}%  t+{:.1}s", fraction * 100.0, elapsed()))
            }
            _ if self.phase != phase => Some(format!("  {phase:<9}        t+{:.1}s", elapsed())),
            _ => None,
        };
        self.phase = phase.to_string();
        line
    }
}

const ACQUIRE: Spec = Spec {
    name: "acquire",
    description: "license, download and decrypt one song",
    options: &[
        Opt::switch("--stereo", "the AAC stereo variant instead of Dolby Atmos"),
        Opt::switch(
            "--lossless",
            "the ALAC rendition through the FairPlay wrapper (tools/lossless; needs its sign-in)",
        ),
        Opt::switch("--hires", "the ALAC rendition above 48 kHz, up to 192 kHz, through the same wrapper"),
    ],
    arguments: &[Arg::one("song", "Apple Music song URL or numeric catalog id")],
    exclusive: &[&["--stereo", "--lossless", "--hires"]],
};

fn cmd_acquire(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&ACQUIRE, args, console, |args, console| {
        data_folders(context)?;
        let catalog_id = song(args.word("song"))?;
        let variant = match (args.flag("--hires"), args.flag("--lossless"), args.flag("--stereo")) {
            (true, _, _) => "hires",
            (false, true, _) => "lossless",
            (false, false, true) => "stereo",
            (false, false, false) => "atmos",
        };
        let started = (context.clock)();
        let elapsed = || (context.clock)() - started;
        console.line(format!("acquire {catalog_id} ({variant})"));
        let media = MediaCache::new(context.config.clone());
        let mut shown_so_far = Progress::new();
        let mut progress = |phase: &str, fraction: Option<f64>| {
            if let Some(line) = shown_so_far.line(phase, fraction, elapsed) {
                console.line(line);
            }
        };
        let cancel = Event::new();
        let acquired = until_interrupted(context, &cancel, || {
            acquirer(context).acquire(&catalog_id, &media, variant, Some(&mut progress), Some(&cancel))
        });
        let result = match acquired {
            Ok(result) => result,
            // whatever the acquisition stumbled over on its way out (the helper gets the
            // terminal's signal too): it was cancelled
            Err(_) if context.interrupt.asked() => {
                return Err(AcquireError::Web(WebError::Cancelled("Acquisition cancelled".to_string())).into());
            }
            Err(error) => return Err(error.into()),
        };
        let elapsed = elapsed();
        // what ffprobe said of the file when it was acquired
        let meta = media.meta(&format!("{catalog_id}.{variant}")).ok().flatten().unwrap_or_default();
        let said = |key: &str| shown(meta.get(key));
        console.line(format!("  from_cache: {}   elapsed: {elapsed:.1}s", truth(result.from_cache)));
        console.line(format!("  file: {}", result.path));
        console.line(format!("  bytes: {} ({})", result.bytes, fmt_bytes(result.bytes)));
        console.line(format!("  variant: {}", repr(&Value::Object(result.variant.clone()))));
        // the value as the cache holds it, as Python printed it; for a file that was just
        // acquired that is the number of the result
        let duration = match meta.get("duration_s") {
            Some(recorded) => shown(Some(recorded)),
            None => result.duration_s.map_or_else(|| "None".to_string(), float),
        };
        let mut facts = format!(
            "  duration_s: {duration}  codec: {}  profile: {}  bit_rate: {}  sample_rate: {}  channels: {}",
            said("codec"),
            said("profile"),
            said("bit_rate"),
            said("sample_rate"),
            said("channels")
        );
        if truthy(meta.get("bit_depth")) {
            facts.push_str(&format!("  bit_depth: {}", said("bit_depth")));
        }
        console.line(facts);
        Ok(0)
    })
}

const LOSSLESS: Spec = Spec {
    name: "lossless",
    description: "state of the lossless (ALAC) route: wrapper build, sign-in, service",
    options: &[
        Opt::switch("--start", "start the wrapper service if it is not running"),
        Opt::switch("--stop", "stop the wrapper service (only one this helper started)"),
        Opt::text("--check", "SONG", "manifest-only: which ALAC renditions Apple offers for a song"),
    ],
    arguments: &[],
    exclusive: &[],
};

fn cmd_lossless(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&LOSSLESS, args, console, |args, console| {
        let lossless = &context.lossless;
        if args.flag("--start") {
            let regions = lossless.ensure_service(None)?;
            console.line(format!("service running; storefronts: {}", regions.join(", ")));
        }
        if args.flag("--stop") {
            let stopped = doctor::wrapperctl(&context.config, context.machine.as_ref(), "stop", None);
            let done = stopped
                .map_err(|error| Failure::Other(format!("could not run the lossless helper ({})", error.name())))?;
            console.pass(&done.stdout);
            console.pass_error(&done.stderr);
            if done.status != Some(0) {
                return Ok(done.status.unwrap_or(1)); // without a status a signal ended it
            }
        }
        let state = lossless.status();
        let keys =
            ["built", "signed_in_files", "running", "storefronts", "available", "backoff_s", "max_sample_rate", "url"];
        for key in keys {
            console.line(format!("  {key:<16} {}", shown(state.get(key))));
        }
        if !truthy(state.get("built")) {
            console.line("  next: ./setup.sh (builds the lossless route on x86_64)");
        } else if !truthy(state.get("signed_in_files")) {
            console.line("  next: sign in with the Sign In button of the app, or ./am-ui cli connect in a terminal");
        }
        if let Some(named) = args.text("--check") {
            let catalog_id = song(Some(named))?;
            lossless.ensure_service(None)?;
            let (master, master_url) = lossless.device_master(&catalog_id)?;
            let variants = alac_variants(&master, &master_url);
            let chosen = if variants.is_empty() { None } else { Some(lossless.select_alac(&master, &master_url)?) };
            console.line(format!("{catalog_id}: {} ALAC rendition(s)", variants.len()));
            for variant in &variants {
                let is_chosen = chosen.as_ref().is_some_and(|chosen| chosen.audio_group == variant.audio_group);
                let kbps = variant.bandwidth.trim().parse::<i64>().unwrap_or(0).div_euclid(1000);
                console.line(format!(
                    "  {} {:<32} {}-bit/{} Hz  avg {kbps} kbps",
                    if is_chosen { "*" } else { " " },
                    variant.audio_group,
                    variant.bit_depth,
                    variant.sample_rate
                ));
            }
        }
        Ok(0)
    })
}

const ATMOS_CHECK: Spec = one_song("atmos-check", "manifest-only Atmos availability", &[]);

fn cmd_atmos_check(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&ATMOS_CHECK, args, console, |args, console| {
        let catalog_id = song(args.word("song"))?;
        let started = (context.clock)();
        let answer = acquirer(context).has_atmos(&catalog_id)?;
        console.line(format!("{catalog_id}: atmos={} ({:.2}s)", truth(answer), (context.clock)() - started));
        Ok(0)
    })
}

// --- render / decode -----------------------------------------------------------

const RENDER: Spec = one_song(
    "render",
    "progressive Cavern 9.1.6 render",
    &[Opt::switch("--force", "render again even when a complete render is cached")],
);

fn cmd_render(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&RENDER, args, console, |args, console| {
        data_folders(context)?;
        let catalog_id = song(args.word("song"))?;
        let config = &context.config;
        let (media, pcm) = (MediaCache::new(config.clone()), PcmCache::new(config));
        let key = format!("{catalog_id}.atmos");
        let Some(m4a) = media.get(&key)? else {
            console.error(format!("error: {key} is not in the media cache; run: {PROGRAM} acquire {catalog_id}"));
            return Ok(1);
        };
        if !args.flag("--force")
            && let Some(existing) = pcm.complete(&key)?
        {
            let frames = CompleteRender::new(&existing)?.total_frames().unwrap_or(0);
            console.line(format!(
                "already rendered: {} ({frames} frames, {:.3}s); use --force to render again",
                existing.display(),
                audio_seconds(frames)
            ));
            return Ok(0);
        }
        let duration = media.meta(&key)?.and_then(|meta| meta.get("duration_s").and_then(Value::as_f64));
        let out_dir = pcm.claim(&key)?;
        let session = RenderSession::new(config.clone(), m4a, &out_dir, duration);
        console.line(format!("render {key} -> {}", out_dir.display()));
        session.start()?;
        let started = (context.clock)();
        loop {
            // Ctrl+C: the renderer is stopped; what it wrote stays for the next claim to remove
            if context.interrupt.asked() {
                session.stop();
                console.error("stopped");
                return Ok(130);
            }
            let frames = session.frames_available();
            let total = session.total_frames().filter(|total| *total != 0);
            let (done, error) = (session.done(), session.error());
            let share = total.map(|total| format!(" ({:5.1}%)", 100.0 * frames as f64 / total as f64));
            console.line(format!(
                "  t+{:5.1}s  {frames:>10} frames  {:7.2}s{}",
                (context.clock)() - started,
                audio_seconds(frames),
                share.unwrap_or_default()
            ));
            if done || error.is_some_and(|error| !error.is_empty()) {
                break;
            }
            session.wait_for(frames + u64::from(SAMPLE_RATE), 1.0);
        }
        if let Some(error) = session.error().filter(|error| !error.is_empty()) {
            console.error(format!("error: {error}"));
            return Ok(1);
        }
        let summary = session.summary().unwrap_or_default();
        pcm.touch(&key)?;
        pcm.evict([&key])?;
        let elapsed = (context.clock)() - started;
        let stats = summary.get("channels_stats").and_then(Value::as_array);
        let stats: Vec<&Map<String, Value>> = stats.into_iter().flatten().filter_map(Value::as_object).collect();
        let peaks = stats.iter().filter_map(|channel| channel.get("peak").map_or(Some(0.0), py_float));
        let peak = peaks.reduce(f64::max).unwrap_or(0.0);
        let silent = stats.iter().filter(|channel| !truthy(channel.get("nonzero_samples")));
        let silent: Vec<Value> = silent.map(|channel| channel.get("name").cloned().unwrap_or(Value::Null)).collect();
        let silent = if silent.is_empty() { "none".to_string() } else { repr(&Value::Array(silent)) };
        let number = |key: &str| summary.get(key).and_then(py_float).unwrap_or(0.0);
        let said = |key: &str| shown(summary.get(key));
        console.line(format!(
            "  done in {elapsed:.1}s wall; renderer {:.2}s = {:.1}x real time",
            number("elapsed_seconds"),
            number("realtime_factor")
        ));
        console.line(format!(
            "  frames: {}  duration_s: {}  bytes: {}",
            said("frames"),
            said("duration_seconds"),
            said("bytes")
        ));
        console.line(format!(
            "  objects: JOC {}-{}, OAMD {}-{}; object blocks {}, fallback blocks {}, non-finite {}",
            said("joc_object_count_min"),
            said("joc_object_count_max"),
            said("oamd_object_count_min"),
            said("oamd_object_count_max"),
            said("rendered_object_blocks"),
            said("channel_fallback_blocks"),
            said("nonfinite_samples")
        ));
        console.line(format!("  peak: {peak:.4}  silent channels: {silent}"));
        Ok(0)
    })
}

const DECODE: Spec = Spec {
    name: "decode",
    description: "decode the cached stereo AAC (or ALAC) file to f32le",
    options: &[
        Opt::switch("--force", ""),
        Opt::switch("--lossless", "the cached ALAC file (<id>.lossless) instead of the AAC one"),
        Opt::switch("--hires", "the cached ALAC file above 48 kHz (<id>.hires), resampled to 48 kHz"),
    ],
    arguments: &[Arg::one("song", "")],
    exclusive: &[&["--lossless", "--hires"]],
};

fn cmd_decode(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&DECODE, args, console, |args, console| {
        data_folders(context)?;
        let catalog_id = song(args.word("song"))?;
        let config = &context.config;
        let (media, pcm) = (MediaCache::new(config.clone()), PcmCache::new(config));
        let kind = match (args.flag("--hires"), args.flag("--lossless")) {
            (true, _) => "hires",
            (false, true) => "lossless",
            (false, false) => "stereo",
        };
        let key = format!("{catalog_id}.{kind}");
        let pcm_name = format!("{kind}.f32le");
        let Some(m4a) = media.get(&key)? else {
            let hint = format!("{PROGRAM} acquire {catalog_id} --{kind}");
            console.error(format!("error: {key} is not in the media cache; run: {hint}"));
            return Ok(1);
        };
        if !args.flag("--force")
            && let Some(existing) = pcm.complete(&key)?
        {
            let frames = PcmFile::new(existing.join(&pcm_name), 2)?.total_frames().unwrap_or(0);
            console.line(format!(
                "already decoded: {} ({frames} frames, {:.3}s); use --force to decode again",
                existing.display(),
                audio_seconds(frames)
            ));
            return Ok(0);
        }
        let out_dir = pcm.claim(&key)?;
        let started = (context.clock)();
        let frames = (context.decode)(&m4a, &out_dir.join(&pcm_name))?;
        let pcm_file = PcmFile::new(out_dir.join(&pcm_name), 2)?;
        pcm.touch(&key)?;
        pcm.evict([&key])?;
        let media_duration = media.meta(&key)?.and_then(|mut meta| meta.remove("duration_s"));
        console.line(format!("decode {key} -> {}", out_dir.display()));
        console.line(format!(
            "  frames: {frames}  duration_s: {:.3}  media duration_s: {}  elapsed: {:.1}s",
            audio_seconds(frames),
            shown(media_duration.as_ref()),
            (context.clock)() - started
        ));
        console.line(format!("  meta: {}", repr(&Value::Object(pcm_file.meta))));
        Ok(0)
    })
}

// --- downmix / cache -----------------------------------------------------------

const DOWNMIX: Spec = Spec {
    name: "downmix",
    description: "show or switch the live stereo downmix",
    options: &[
        Opt::float("--preamp", "DB", "preamp in dB (default: current)"),
        Opt::switch("--list", "list the available profiles"),
        Opt::switch("--live", "also print the sink's live control values"),
    ],
    arguments: &[Arg::optional("key", "profile key to apply (peqdb, apple, itu)")],
    exclusive: &[],
};

/// The sink's controls `downmix --live` prints.
const LIVE_CONTROLS: [&str; 7] = [
    "preamp_l:Mult",
    "dry_l:Gain 1",
    "dry_l:Gain 2",
    "dry_l:Gain 3",
    "out_l:Gain 2",
    "lfe_lp1:Freq",
    "dly_l:Delay (s)",
];

fn cmd_downmix(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&DOWNMIX, args, console, |args, console| {
        data_folders(context)?;
        if args.flag("--list") {
            for profile in downmix::profiles() {
                let (key, label, kind) = (profile.key, profile.label, profile.kind);
                console.line(format!("  {key:<9} {label:<18} {kind:<7} {}", profile.description));
            }
            return Ok(0);
        }
        let sink = &context.downmix;
        let applied = match (args.word("key"), args.float("--preamp")) {
            (None, None) => {
                let state = sink.current();
                let running = sink.sink_running();
                console.line(format!(
                    "current: preset={} preamp={:+.1} dB match={} (from {}); sink running: {}",
                    state.preset,
                    state.preamp,
                    truth(state.match_on),
                    state.source,
                    truth(running)
                ));
                if args.flag("--live") && running {
                    let live = sink.live_controls()?;
                    for name in LIVE_CONTROLS {
                        if let Some(value) = live.get(name) {
                            console.line(format!("  {name:<16} {}", format_g(value, 6)));
                        }
                    }
                }
                return Ok(0);
            }
            (None, Some(preamp)) => sink.set_preamp(preamp)?,
            (Some(key), preamp) => sink.apply(key, preamp, None)?,
        };
        console.line(format!(
            "applied: preset={} preamp={:+.1} dB match={}",
            applied.preset,
            applied.preamp,
            truth(applied.match_on)
        ));
        for line in py_splitlines(&applied.output) {
            console.line(format!("  {line}"));
        }
        Ok(0)
    })
}

const CACHE: Spec = Spec {
    name: "cache",
    description: "list the private caches",
    options: &[Opt::switch("--evict", "run LRU eviction on every cache")],
    arguments: &[],
    exclusive: &[],
};

fn cmd_cache(context: &Context, args: &[String], console: &mut Console) -> i32 {
    run(&CACHE, args, console, |args, console| {
        data_folders(context)?;
        let config = &context.config;
        let (media, pcm, art) = (MediaCache::new(config.clone()), PcmCache::new(config), ArtCache::new(config));
        if args.flag("--evict") {
            media.evict()?;
            pcm.evict([] as [&str; 0])?;
            art.evict()?;
        }
        console.line(format!(
            "media cache {} ({} of {}):",
            media.directory.display(),
            fmt_bytes(media.total_bytes()?),
            fmt_bytes(media.max_bytes)
        ));
        // the most recently used first
        let mut entries: Vec<_> = media.entries()?.into_iter().collect();
        entries.sort_by(|(_, a), (_, b)| b.last_access.total_cmp(&a.last_access));
        for (key, entry) in entries {
            let text = |name: &str| entry.meta.get(name).and_then(Value::as_str).filter(|text| !text.is_empty());
            let duration = entry.meta.get("duration_s").and_then(py_float).unwrap_or(0.0);
            console.line(format!(
                "  {key:<24} {:>10}  {:<5} {duration:8.2}s  {}  used {}",
                fmt_bytes(entry.size),
                text("codec").unwrap_or("?"),
                text("audio_group").unwrap_or(""),
                local_minute(entry.last_access)
            ));
        }
        console.line(format!("pcm cache {} (keep {} songs):", pcm.directory.display(), pcm.keep));
        let mut entries: Vec<_> = pcm.entries()?.into_iter().collect();
        entries.sort_by(|(_, a), (_, b)| b.last_access.total_cmp(&a.last_access));
        for (key, entry) in entries {
            let state = if entry.complete { "complete" } else { "partial" };
            let (size, used) = (fmt_bytes(entry.bytes), local_minute(entry.last_access));
            console.line(format!("  {key:<24} {size:>10}  {state:<8}  used {used}"));
        }
        console.line(format!(
            "art cache {}: {} files, {} of {}",
            art.directory.display(),
            art.entries()?.len(),
            fmt_bytes(art.total_bytes()?),
            fmt_bytes(art.max_bytes)
        ));
        Ok(0)
    })
}

/// The commands of this module (Python: `COMMANDS` of `amui/cli_pipeline.py`).
pub const COMMANDS: &[Command] = &[
    Command {
        name: "acquire",
        help: "acquire <id|url> [--stereo|--lossless]: license, download and decrypt into the media cache",
        run: cmd_acquire,
    },
    Command {
        name: "atmos-check",
        help: "atmos-check <id|url>: manifest-only Dolby Atmos availability",
        run: cmd_atmos_check,
    },
    Command {
        name: "lossless",
        help: "lossless [--start] [--stop] [--check SONG]: state of the ALAC route (wrapper, sign-in, service)",
        run: cmd_lossless,
    },
    Command {
        name: "render",
        help: "render <id> [--force]: progressive Cavern 9.1.6 render of the cached Atmos file",
        run: cmd_render,
    },
    Command {
        name: "decode",
        help: "decode <id> [--force] [--lossless]: decode the cached stereo AAC/ALAC file to f32le",
        run: cmd_decode,
    },
    Command {
        name: "downmix",
        help: "downmix [key] [--preamp dB] [--list] [--live]: show or switch the live downmix",
        run: cmd_downmix,
    },
    Command { name: "cache", help: "cache [--evict]: list the media/PCM/art caches", run: cmd_cache },
];

#[cfg(test)]
mod tests {
    //! What the reference of the Python command line (`cli::reference`) does not hold: the
    //! statuses of failures, a decode (through a stand-in for ffmpeg), the control script for
    //! `lossless --stop` (a stand-in machine), a sink that is not there, eviction. No real
    //! program is started, no network, no sink.
    use super::*;
    use crate::acquire::testing::Asked;
    use crate::cli::testing::{Scratch, reference, reference_network};
    use crate::downmix::{Output, RunError};
    use crate::util;
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// A decrypted file in the media cache, with what `acquire` would have recorded.
    fn cached(scratch: &Scratch, key: &str, meta: Value) {
        let source = scratch.config.tmp_dir.join("test-media");
        fs::write(&source, vec![0u8; 64]).unwrap();
        MediaCache::new(scratch.config.clone()).put(key, &source, meta.as_object().unwrap()).unwrap();
    }

    /// Standard output with the scratch data folder taken out.
    fn printed(scratch: &Scratch, out: &str) -> String {
        out.replace(&scratch.config.var.display().to_string(), "<var>")
    }

    /// The reference's reports of an acquisition, on its clock: the lines Python printed.
    #[test]
    fn progress_lines_of_an_acquisition() {
        let reference = reference();
        let case = &reference["values"]["acquire_progress"];
        let step = case["step"].as_f64().unwrap();
        // the clock moves by a step at every look; the first look is the start
        let now = Cell::new(100.0);
        let look = || {
            now.set(now.get() + step);
            now.get()
        };
        let started = look();
        let elapsed = || look() - started;
        let mut progress = Progress::new();
        let mut lines = String::new();
        for report in case["reports"].as_array().unwrap() {
            let (phase, fraction) = (report[0].as_str().unwrap(), report[1].as_f64());
            if let Some(line) = progress.line(phase, fraction, elapsed) {
                lines.push_str(&line);
                lines.push('\n');
            }
        }
        assert_eq!(lines, case["printed"].as_str().unwrap());
        assert_eq!(lines.lines().count(), 10);
    }

    #[test]
    fn failures_have_their_exit_statuses() {
        // no sign-in on this machine
        let scratch = Scratch::with(&[], |_| Err(WebError::auth(crate::web::NO_SIGN_IN, None)));
        let ran = scratch.run(&["atmos-check", "1000000001"]);
        assert_eq!((ran.status, ran.out.as_str()), (3, ""));
        assert_eq!(ran.err, "not connected: No Apple Music sign-in on this machine: connect to sign in\n");
        // cancelled
        let scratch = Scratch::with(&[], |_| Err(WebError::Cancelled("Download cancelled".to_string())));
        let ran = scratch.run(&["atmos-check", "1000000001"]);
        assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (130, "", "cancelled\n"));
        // anything else of the network
        let scratch = Scratch::with(&[], |_| Err(WebError::rate_limited("amp-api.music.apple.com", Some(30.0))));
        let ran = scratch.run(&["atmos-check", "1000000001"]);
        assert_eq!(ran.status, 1);
        assert_eq!(ran.err, "error: amp-api.music.apple.com rate-limited this session; retry after 30s\n");

        // a song without Dolby Atmos: the manifest says so before any license is asked for
        let scratch = Scratch::with(&[], reference_network());
        helper_stand_in(&scratch);
        let ran = scratch.run(&["acquire", "1000000002"]);
        assert_eq!(ran.status, 4);
        assert_eq!(ran.out, "acquire 1000000002 (atmos)\n  manifest         t+0.0s\n");
        assert_eq!(ran.err, "no Atmos: Apple advertises no Dolby Atmos (ec-3) variant for this song\n");
        // the lossless route of a machine that has not built it
        let ran = scratch.run(&["acquire", "1000000002", "--lossless"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, "acquire 1000000002 (lossless)\n"));
        assert_eq!(ran.err, "error: the lossless route is not built (./setup.sh builds it)\n");
        let ran = scratch.run(&["acquire", "1000000002", "--hires"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, "acquire 1000000002 (hires)\n"));
        assert_eq!(ran.err, "error: the lossless route is not built (./setup.sh builds it)\n");
        assert!(scratch.route.asked.lock().unwrap().is_empty());
    }

    /// The DRM helper of a scratch folder: a file that is there and is never started.
    fn helper_stand_in(scratch: &Scratch) {
        fs::create_dir_all(scratch.config.drm_helper.parent().unwrap()).unwrap();
        fs::write(&scratch.config.drm_helper, "a stand-in that is never started\n").unwrap();
    }

    #[test]
    fn ctrl_c_cancels_an_acquisition() {
        // a network that takes its time: Ctrl+C arrives while the manifest is asked for
        let answers = reference_network();
        let scratch = Scratch::with(&[], move |asked| {
            std::thread::sleep(Duration::from_millis(300));
            answers(asked)
        });
        helper_stand_in(&scratch);
        scratch.interrupt.quiet.store(1, Ordering::SeqCst); // pressed when the watch first looks
        let ran = scratch.run(&["acquire", "1000000001"]);
        assert_eq!(ran.out, "acquire 1000000001 (atmos)\n  manifest         t+0.0s\n");
        assert_eq!((ran.status, ran.err.as_str()), (130, "cancelled\n"));
        // it went no further than the manifest, and nothing of it is left in the data folder
        assert_eq!(scratch.asked().len(), 2);
        assert_eq!(fs::read_dir(&scratch.config.tmp_dir).unwrap().count(), 0);
        assert!(MediaCache::new(scratch.config.clone()).entries().unwrap().is_empty());

        // a failure on the way out of a cancelled acquisition is the cancellation
        fn failing(_: &Asked) -> Result<Vec<u8>, WebError> {
            std::thread::sleep(Duration::from_millis(300));
            Err(WebError::other("Network request to amp-api.music.apple.com failed (ConnectionReset)"))
        }
        let scratch = Scratch::with(&[], failing);
        helper_stand_in(&scratch);
        scratch.interrupt.quiet.store(1, Ordering::SeqCst);
        let ran = scratch.run(&["acquire", "1000000001"]);
        assert_eq!((ran.status, ran.err.as_str()), (130, "cancelled\n"));
        // without Ctrl+C it is the failure it is
        let scratch = Scratch::with(&[], failing);
        helper_stand_in(&scratch);
        let ran = scratch.run(&["acquire", "1000000001"]);
        let failure = "error: Network request to amp-api.music.apple.com failed (ConnectionReset)\n";
        assert_eq!((ran.status, ran.err.as_str()), (1, failure));
    }

    #[test]
    fn ctrl_c_stops_a_render() {
        // a renderer that writes a tenth of a second of audio and then never ends
        let script = "set -eu\nout=$2\nsleep 0.3\nhead -c 307200 /dev/zero > \"$out/atmos-916.f32le\"\nexec sleep 60\n";
        let scratch =
            Scratch::with(&[("AMUI_RENDER_SCRIPT", "{scratch}/render.sh")], |_| Err(WebError::other("unused")));
        fs::write(&scratch.config.render_script, script).unwrap();
        cached(&scratch, "1000000006.atmos", json!({"duration_s": 0.1}));
        scratch.interrupt.quiet.store(1, Ordering::SeqCst); // pressed after the first line
        let started = Instant::now();
        let ran = scratch.run(&["render", "1000000006"]);
        assert_eq!(
            printed(&scratch, &ran.out),
            "render 1000000006.atmos -> <var>/cache/pcm/1000000006.atmos\n\
             \x20 t+  0.0s           0 frames     0.00s (  0.0%)\n"
        );
        assert_eq!((ran.status, ran.err.as_str()), (130, "stopped\n"));
        assert!(started.elapsed() < Duration::from_secs(30), "the renderer was waited for");
        // what the renderer wrote is no finished render; the next one starts over
        let pcm = PcmCache::new(&scratch.config);
        assert_eq!(pcm.complete("1000000006.atmos").unwrap(), None);
        let listing = scratch.run(&["cache"]).out;
        assert!(listing.contains("  1000000006.atmos          300.0 KiB  partial "), "{listing}");
    }

    #[test]
    fn usage_errors_touch_nothing() {
        let scratch = Scratch::new();
        fs::remove_dir_all(&scratch.config.var).unwrap();
        for (args, error) in [
            (&["acquire"][..], "the following arguments are required: song"),
            (&["acquire", "1", "--stereo", "--lossless"], "argument --lossless: not allowed with argument --stereo"),
            (&["render", "1", "2"], "unrecognized arguments: 2"),
            (&["acquire", "1", "--lossless", "--hires"], "argument --hires: not allowed with argument --lossless"),
            (&["decode", "--force"], "the following arguments are required: song"),
            (&["decode", "1", "--lossless", "--hires"], "argument --hires: not allowed with argument --lossless"),
            (&["downmix", "--preamp", "loud"], "argument --preamp: invalid float value: 'loud'"),
            (&["downmix", "--preamp"], "argument --preamp: expected one argument"),
            (&["downmix", "--li"], "ambiguous option: --li could match --list, --live"),
            (&["lossless", "--check"], "argument --check: expected one argument"),
            (&["cache", "all"], "unrecognized arguments: all"),
            (&["atmos-check"], "the following arguments are required: song"),
        ] {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.out.as_str()), (2, ""), "{args:?}");
            let lines: Vec<&str> = ran.err.lines().collect();
            assert_eq!(lines.len(), 2, "{args:?}");
            assert!(lines[0].starts_with(&format!("usage: amui {} [-h]", args[0])), "{}", ran.err);
            assert_eq!(lines[1], format!("amui {}: error: {error}", args[0]));
        }
        assert!(!scratch.config.var.exists()); // not even the data folders were made
        assert!(scratch.sink.calls.lock().unwrap().is_empty());
        // a negative preamp is a value, with or without `=`
        for args in [&["downmix", "--preamp", "-9"][..], &["downmix", "--preamp=-9"], &["downmix", "--pre", "-9"]] {
            let ran = scratch.run(args);
            assert_eq!(
                (ran.status, ran.out.lines().next()),
                (0, Some("applied: preset=peqdb preamp=-9.0 dB match=True"))
            );
        }
    }

    #[test]
    fn a_data_folder_that_cannot_be_made() {
        let scratch = Scratch::new();
        fs::remove_dir_all(&scratch.config.tmp_dir).unwrap();
        fs::write(&scratch.config.tmp_dir, "a file where a folder belongs").unwrap();
        for args in [&["cache"][..], &["acquire", "1000000001"], &["render", "1000000001"], &["downmix", "--list"]] {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.out.as_str()), (1, ""), "{args:?}");
            assert_eq!(ran.err, "error: the data folders could not be created (FileExistsError)\n", "{args:?}");
        }
    }

    #[test]
    fn stopping_the_wrapper_service() {
        let scratch = Scratch::new();
        let helper = scratch.config.lossless_wrapperctl.display().to_string();
        // the control script stops it and says what it did; then the state
        scratch.machine.answer(" stop", 0, "{\n  \"stopped\": true\n}\n", "");
        let ran = scratch.run(&["lossless", "--stop"]);
        assert_eq!((ran.status, ran.err.as_str()), (0, ""));
        assert!(ran.out.starts_with("{\n  \"stopped\": true\n}\n  built            False\n"), "{}", ran.out);
        let calls = scratch.machine.calls.lock().unwrap().clone();
        assert_eq!(calls, [(vec!["python3".to_string(), "-I".to_string(), helper.clone(), "stop".to_string()], None)]);
        // a wrapper the script did not start: its status, and no state
        let refused = "a wrapper answers on 127.0.0.1:9 but was not started by this helper; leaving it alone\n";
        scratch.machine.answer(" stop", 2, "", refused);
        let ran = scratch.run(&["lossless", "--stop"]);
        assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (2, "", refused));
        // a script that cannot be started, or that a signal ended
        scratch.machine.outcome(" stop", Err(RunError::NotFound));
        let ran = scratch.run(&["lossless", "--stop"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, "error: could not run the lossless helper (FileNotFoundError)\n");
        scratch.machine.outcome(" stop", Ok(Output { status: None, stdout: String::new(), stderr: String::new() }));
        let ran = scratch.run(&["lossless", "--stop"]);
        assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (1, "", ""));
        assert_eq!(scratch.machine.command_lines(), vec![format!("python3 -I {helper} stop"); 4]);
    }

    /// A decode that writes half a second of silence and its description, as ffmpeg's would.
    fn silence(_m4a: &Path, out: &Path) -> Result<u64, AcquireError> {
        fs::write(out, vec![0u8; 24_000 * 8]).unwrap();
        let meta = json!({"channels": 2, "frames": 24_000, "source": "aac", "resampled": false, "bit_rate": 256_000,
                          "duration_s": 0.5, "profile": null});
        util::write_private_json(&out.with_file_name("meta.json"), &meta).unwrap();
        Ok(24_000)
    }

    #[test]
    fn decode_of_a_cached_file() {
        let mut scratch = Scratch::new();
        cached(&scratch, "1000000002.stereo", json!({"duration_s": 0.512, "codec": "aac"}));
        cached(&scratch, "1000000002.lossless", json!({"codec": "alac"}));
        let decodes = Arc::new(AtomicUsize::new(0));
        let counted = decodes.clone();
        scratch.context.decode = Box::new(move |m4a, out| {
            counted.fetch_add(1, Ordering::SeqCst);
            assert!(m4a.is_file() && !out.exists(), "{m4a:?} -> {out:?}");
            silence(m4a, out)
        });
        *scratch.tick.lock().unwrap() = 0.75;
        let ran = scratch.run(&["decode", "1000000002"]);
        assert_eq!(
            printed(&scratch, &ran.out),
            "decode 1000000002.stereo -> <var>/cache/pcm/1000000002.stereo\n\
             \x20 frames: 24000  duration_s: 0.500  media duration_s: 0.512  elapsed: 0.8s\n\
             \x20 meta: {'bit_rate': 256000, 'channels': 2, 'duration_s': 0.5, 'frames': 24000, 'profile': None, \
             'resampled': False, 'source': 'aac'}\n"
        );
        assert_eq!((ran.status, ran.err.as_str()), (0, ""));
        // it is kept: a second call decodes nothing, --force does
        let ran = scratch.run(&["decode", "1000000002"]);
        let kept =
            "already decoded: <var>/cache/pcm/1000000002.stereo (24000 frames, 0.500s); use --force to decode again\n";
        assert_eq!((ran.status, printed(&scratch, &ran.out).as_str()), (0, kept));
        assert_eq!(decodes.load(Ordering::SeqCst), 1);
        assert_eq!(scratch.run(&["decode", "1000000002", "--force"]).status, 0);
        assert_eq!(decodes.load(Ordering::SeqCst), 2);
        // the ALAC file has its own entry; a file without a duration prints None
        let ran = scratch.run(&["decode", "--lossless", "1000000002"]);
        let lines: Vec<String> = printed(&scratch, &ran.out).lines().map(str::to_string).collect();
        assert_eq!(lines[0], "decode 1000000002.lossless -> <var>/cache/pcm/1000000002.lossless");
        assert_eq!(lines[1], "  frames: 24000  duration_s: 0.500  media duration_s: None  elapsed: 0.8s");
        assert!(scratch.config.pcm_cache_dir.join("1000000002.lossless/lossless.f32le").is_file());

        // a decode that fails says why; nothing is announced
        scratch.context.decode = Box::new(|_, _| Err(AcquireError::other("ffmpeg decode failed: Invalid data found")));
        let ran = scratch.run(&["decode", "1000000002", "--force"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, "error: ffmpeg decode failed: Invalid data found\n");
    }

    #[test]
    fn an_entry_another_process_uses_is_left_alone() {
        let mut scratch = Scratch::new();
        cached(&scratch, "1000000006.atmos", json!({"duration_s": 0.1}));
        cached(&scratch, "1000000006.stereo", json!({}));
        scratch.context.decode = Box::new(|_, _| panic!("nothing is decoded over another process's entry"));
        // SAFETY: getppid() has no preconditions and cannot fail.
        let other = unsafe { libc::getppid() }; // alive for as long as this test runs
        let pcm = PcmCache::new(&scratch.config);
        for key in ["1000000006.atmos", "1000000006.stereo"] {
            let directory = pcm.claim(key).unwrap();
            fs::write(directory.join(format!(".inuse-{other}")), "").unwrap();
        }
        let ran = scratch.run(&["render", "1000000006"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, format!("error: 1000000006.atmos is in use by process {other}\n"));
        let ran = scratch.run(&["decode", "1000000006"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, format!("error: 1000000006.stereo is in use by process {other}\n"));
        // and eviction keeps it
        assert_eq!(scratch.run(&["cache", "--evict"]).status, 0);
        assert!(scratch.config.pcm_cache_dir.join("1000000006.atmos").is_dir());
    }

    #[test]
    fn downmix_without_a_sink() {
        let scratch = Scratch::new();
        *scratch.sink.node.lock().unwrap() = None;
        let state = scratch.config.workspace.join("pipewire/downmix-state.json");
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        fs::write(&state, r#"{"preset": "itu", "preamp": -7.5, "match": false}"#).unwrap();
        let ran = scratch.run(&["downmix", "--live"]);
        let current = "current: preset=itu preamp=-7.5 dB match=False (from workspace); sink running: False\n";
        assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (0, current, ""));
        for args in [&["downmix", "apple"][..], &["downmix", "--preamp", "-3"]] {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.out.as_str()), (1, ""), "{args:?}");
            assert_eq!(ran.err, "error: downmix sink is not running (applemusic-downmix service)\n");
        }
        // nothing was set, nothing recorded
        assert!(!scratch.sink.programs().iter().any(|call| call.contains("set-param")));
        assert!(!scratch.config.state_dir.join("downmix.json").exists());
    }

    #[test]
    fn a_change_of_the_sink_is_recorded_with_its_caller() {
        let scratch = Scratch::new();
        let ran = scratch.run(&["downmix", "apple", "--preamp", "-9"]);
        assert_eq!((ran.status, ran.out.lines().next()), (0, Some("applied: preset=apple preamp=-9.0 dB match=True")));
        assert_eq!(scratch.sink.programs(), ["pw-cli ls", "pw-dump", "pw-cli set-param"]);
        let log = fs::read_to_string(scratch.config.log_dir.join("downmix.log")).unwrap();
        let line = log.lines().last().unwrap();
        assert!(line.contains(" pipeline.rs:"), "{line}");
        assert!(line.ends_with(" applied preset=apple preamp=-9.00 final=-7.30 match=True"), "{line}");
        // a sink that does not take the controls
        let refused = Output {
            status: Some(0),
            stdout: "Error: \"set-param: unknown global 38\"\n".into(),
            stderr: String::new(),
        };
        scratch.sink.fail("set-param", Ok(refused));
        let ran = scratch.run(&["downmix", "itu"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, "error: downmix apply failed: could not set the controls of node 38 (atmos_916_downmix)\n");
        // the state is still what was applied before
        let ran = scratch.run(&["downmix"]);
        assert_eq!(ran.out, "current: preset=apple preamp=-9.0 dB match=True (from am-ui); sink running: True\n");
    }

    #[test]
    fn eviction_keeps_the_songs_used_last() {
        let scratch = Scratch::new();
        let pcm = PcmCache::new(&scratch.config);
        for song in ["1000000001", "1000000002", "1000000003", "1000000004"] {
            let directory = pcm.claim(&format!("{song}.stereo")).unwrap();
            fs::write(directory.join("stereo.f32le"), vec![0u8; 800]).unwrap();
            fs::write(directory.join("meta.json"), r#"{"channels": 2, "frames": 100}"#).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let listed = |args: &[&str]| -> Vec<String> {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.err.as_str()), (0, ""));
            let entries = ran.out.lines().filter(|line| line.starts_with("  100"));
            entries.map(|line| line.split("  used ").next().unwrap().to_string()).collect()
        };
        let all = [
            "  1000000004.stereo             800 B  complete",
            "  1000000003.stereo             800 B  complete",
            "  1000000002.stereo             800 B  complete",
            "  1000000001.stereo             800 B  complete",
        ];
        assert_eq!(listed(&["cache"]), all);
        assert_eq!(listed(&["cache", "--evict"]), all[..3]);
        assert!(!scratch.config.pcm_cache_dir.join("1000000001.stereo").exists());
        // the files stay private to the owner
        let mode = fs::metadata(&scratch.config.pcm_cache_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn sizes_and_songs() {
        assert_eq!([fmt_bytes(0), fmt_bytes(1023), fmt_bytes(1536)], ["0 B", "1023 B", "1.5 KiB"]);
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024 * 1024), "3072.0 GiB");
        assert_eq!(song_id("1000000001").as_deref(), Ok("1000000001"));
        assert_eq!(
            song_id("https://music.apple.com/zz/album/album-a/1000000010?i=1000000002").as_deref(),
            Ok("1000000002")
        );
        assert_eq!(song_id("song a"), Err("Use an Apple Music song link or numeric song ID"));
        assert_eq!(
            song_id("https://music.apple.com/zz/album/album-a/1000000010"),
            Err("Use a single song link, or an album link with an ?i= song selection")
        );
    }
}
