//! Cavern JOC -> 16-channel float PCM, progressive (playback starts while rendering).
//!
//! Runs `config.render_script` (am-ui's patched Cavern, `tools/cavern-lab/render-music-916-lab.sh`,
//! or an older workspace's `tools/render-music-916.sh`; both exec the dotnet CavernProbe). The
//! probe writes `<out_dir>/atmos-916.f32le` progressively through a 1 MiB FileStream buffer (one
//! flush ~ 0.34 s of audio; ~23x real time measured), prints a progress line every 20 s of audio,
//! writes `render-summary.json` at the end and exits non-zero on validation failure. It refuses
//! an existing PCM file: always render into a fresh directory from `PcmCache::claim()`.
//! Validation after completion: channel_fallback_blocks == 0, nonfinite_samples == 0,
//! bytes == frames * 64 == file size.
//!
//! Beyond the [`PcmSource`] contract: [`RenderSession::with_script`] for tests, the session's
//! `summary_path`, `log_path`, `returncode()`, `running()`, `started_at()`, `finished_at()`,
//! `pause()`/`resume()`/`paused()` (SIGSTOP/SIGCONT: the player runs one Cavern process at a time
//! and freezes a prefetch render while the current track's render needs the CPU),
//! [`load_summary`], [`validate_summary`], [`validate_pcm_file`], [`read_meta`].
//! There is no binaural renderer (decision 2026-11-08: "Apple binaural" means the `apple`
//! fold-down matrix preset of the live downmix, see research/engine-pipeline.md).
//!
//! Deliberate differences from `amui/render.py`:
//!
//! * The configuration is a parameter: [`RenderSession::new`] and [`stamp_generation`] take the
//!   [`Config`] the Python module read from its globals.
//! * `total_frames`, `done`, `error`, `summary`, `running`, `paused` and `returncode` are methods;
//!   the four of the [`PcmSource`] contract are called through that trait.
//! * `returncode()` looks at the renderer before it answers, so it is never behind `done()`.
//! * A summary with `NaN` or `Infinity` literals is "unreadable" (Python's parser accepts them).
//! * A summary is rewritten by [`stamp_generation`] with this crate's JSON writer: same keys in
//!   the same order, same two-space layout; a float may be spelled differently (`1e-5` for
//!   Python's `1e-05`) and non-ASCII text is written as it is instead of `\u` escapes. Both
//!   engines read both spellings.
//! * Missing folders above the output directory are created 0700 (Python: the leaf only).
//! * An output directory that cannot be created is a [`RenderError::Start`] (Python let the
//!   `OSError` through).
//! * `CAVERN_LAB_OPTIONS` set in the environment the configuration was made from wins over the
//!   recommended renderer options, as in Python (the render script documents that override);
//!   `Config::lab_render_options()` answers with it.
//! * A session that goes away while its renderer runs leaves it running, as in Python, but wakes
//!   it when it was frozen and waits for its end on a thread, so that neither a stopped nor a
//!   defunct process stays behind.
//!
//! The helpers at the end (`private_umask`, `wait_timeout`, `terminate`, `last_line`,
//! `splitlines`, `os_error_name`) are shared with `acquire` and `lossless`, which start helper
//! programs the same way.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::config::{self, Config};
use crate::util;

pub const PCM_NAME: &str = "atmos-916.f32le";
pub const SUMMARY_NAME: &str = "render-summary.json";
pub const LOG_NAME: &str = "render.log";
/// 16 channels x float32.
pub const FRAME_BYTES: usize = config::FRAME_BYTES;

/// A render that cannot be used or started, with a message that is safe to show and log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// A summary, a `meta.json` or a PCM file that does not describe a complete, valid render.
    #[error("{0}")]
    Invalid(String),
    /// The renderer could not be started.
    #[error("{0}")]
    Start(String),
}

fn invalid(message: impl Into<String>) -> RenderError {
    RenderError::Invalid(message.into())
}

/// What the output feeder reads: a growing or complete interleaved f32le file.
///
/// `channels()` is 16 (Cavern 9.1.6 render in the sink's channel order) or 2 (stereo AAC or ALAC
/// decode, or a binaural render); the feeder expands 2 channels into FL/FR of the 16-channel
/// stream.
pub trait PcmSource: Send + Sync {
    fn pcm_path(&self) -> &Path;

    fn channels(&self) -> usize;

    fn frames_available(&self) -> u64;

    /// Exact when done; an estimate (duration * 48000) before.
    fn total_frames(&self) -> Option<u64>;

    fn done(&self) -> bool;

    fn error(&self) -> Option<String>;
}

// --- validation helpers ---------------------------------------------------------

/// What Python's `int()` makes of a JSON value: whole numbers, floats cut off towards zero,
/// text holding a whole number, true and false.
fn python_int(value: &Value) -> Option<i128> {
    match value {
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(whole), _, _) => Some(i128::from(whole)),
            (_, Some(whole), _) => Some(i128::from(whole)),
            (_, _, Some(float)) if float.is_finite() => Some(float.trunc() as i128),
            _ => None,
        },
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(i128::from(*flag)),
        _ => None,
    }
}

fn int_field(value: Option<&Value>, name: &str) -> Result<i128, RenderError> {
    value.and_then(python_int).ok_or_else(|| invalid(format!("render summary field {name} is not a number")))
}

/// A JSON value the way Python prints it inside a message.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Check a `render-summary.json` value against its PCM file; returns the frame count.
pub fn validate_summary(summary: &Value, pcm_path: &Path) -> Result<u64, RenderError> {
    let Value::Object(summary) = summary else {
        return Err(invalid("render summary is not a JSON object"));
    };
    let field = |name: &str, default: i128| match summary.get(name) {
        None => Ok(default),
        found => int_field(found, name),
    };
    let frames = int_field(summary.get("frames"), "frames")?;
    if frames <= 0 {
        return Err(invalid("render summary reports no frames"));
    }
    if field("channels", config::CHANNELS as i128)? != config::CHANNELS as i128 {
        return Err(invalid("render is not 16 channels"));
    }
    if field("sample_rate", i128::from(config::SAMPLE_RATE))? != i128::from(config::SAMPLE_RATE) {
        return Err(invalid("render is not 48 kHz"));
    }
    if field("channel_fallback_blocks", 0)? != 0 {
        let blocks = python_str(&summary["channel_fallback_blocks"]);
        return Err(invalid(format!("{blocks} channel-only fallback blocks")));
    }
    if field("nonfinite_samples", 0)? != 0 {
        let samples = python_str(&summary["nonfinite_samples"]);
        return Err(invalid(format!("{samples} non-finite samples")));
    }
    let expected = frames.saturating_mul(FRAME_BYTES as i128);
    if field("bytes", expected)? != expected {
        return Err(invalid("render summary byte count does not match its frame count"));
    }
    let size = fs::metadata(pcm_path).map_err(|_| invalid("render PCM file missing"))?.len();
    if i128::from(size) != expected {
        return Err(invalid(format!("render PCM file is {size} bytes, expected {expected}")));
    }
    Ok(size / FRAME_BYTES as u64)
}

/// The frame count of a validated summary.
fn summary_frames(summary: &Map<String, Value>) -> Option<u64> {
    summary.get("frames").and_then(python_int).and_then(|frames| u64::try_from(frames).ok())
}

/// Read and validate `<out_dir>/render-summary.json`.
pub fn load_summary(out_dir: &Path) -> Result<Map<String, Value>, RenderError> {
    let path = out_dir.join(SUMMARY_NAME);
    if !path.is_file() {
        return Err(invalid("render summary missing"));
    }
    let summary = util::read_json(&path).ok_or_else(|| invalid("render summary unreadable"))?;
    validate_summary(&summary, &out_dir.join(PCM_NAME))?;
    match summary {
        Value::Object(summary) => Ok(summary),
        _ => Err(invalid("render summary is not a JSON object")),
    }
}

/// Record which renderer produced `<out_dir>` (`config.render_generation()` + the options) in its
/// summary.
///
/// Renders made by an older or different renderer are then re-rendered instead of reused: the
/// lab renderer's output differs audibly from the workspace probe's (JOC phase compensation,
/// LFE alignment). `summary` is the directory's summary when the caller has just read it.
pub fn stamp_generation(config: &Config, out_dir: &Path, summary: Option<&Map<String, Value>>) {
    let path = out_dir.join(SUMMARY_NAME);
    let read;
    let summary = match summary {
        Some(summary) => summary,
        None => match util::read_json_object(&path) {
            Some(found) => {
                read = found;
                &read
            }
            None => return,
        },
    };
    let generation = config.render_generation();
    if summary.get("generation").and_then(Value::as_str) == Some(generation.as_str()) {
        return;
    }
    let lab = config.render_script == config.lab_render_script;
    let mut stamped = summary.clone();
    stamped.insert("generation".to_string(), json!(generation));
    stamped.insert("render_script".to_string(), json!(config.render_script.to_string_lossy()));
    stamped.insert("render_options".to_string(), json!(if lab { config.lab_render_options() } else { String::new() }));
    if let Ok(mut text) = serde_json::to_vec_pretty(&stamped) {
        text.push(b'\n');
        // a missing stamp only costs a re-render
        let _ = util::write_private_atomic(&path, &text);
    }
}

/// The generation stamped into `<out_dir>/render-summary.json`, `None` when absent or unreadable.
pub fn render_generation_of(out_dir: &Path) -> Option<String> {
    util::read_json_object(&out_dir.join(SUMMARY_NAME))?.get("generation")?.as_str().map(str::to_owned)
}

/// `meta.json` next to a stereo or binaural PCM file, or `None`.
pub fn read_meta(pcm_path: &Path) -> Option<Map<String, Value>> {
    util::read_json_object(&pcm_path.with_file_name("meta.json"))
}

/// Frames in a complete interleaved f32le file; `meta.json` (when present) must agree with the
/// size.
pub fn validate_pcm_file(pcm_path: &Path, channels: usize) -> Result<u64, RenderError> {
    let size = fs::metadata(pcm_path).map_err(|_| invalid("PCM file missing"))?.len();
    let frame_bytes = channels as u64 * 4;
    if size == 0 || frame_bytes == 0 || size % frame_bytes != 0 {
        return Err(invalid(format!("PCM file size {size} is not a whole number of {channels}-channel frames")));
    }
    let frames = size / frame_bytes;
    if let Some(meta) = read_meta(pcm_path) {
        if meta.contains_key("frames") && int_field(meta.get("frames"), "frames")? != i128::from(frames) {
            return Err(invalid("meta.json frame count does not match the PCM file size"));
        }
        if meta.contains_key("channels") && int_field(meta.get("channels"), "channels")? != channels as i128 {
            return Err(invalid("meta.json channel count does not match"));
        }
    }
    Ok(frames)
}

// --- sources -----------------------------------------------------------------------

#[derive(Default)]
struct SessionState {
    process: Option<Child>,
    returncode: Option<i32>,
    summary: Option<Map<String, Value>>,
    error: Option<String>,
    done: bool,
    stopped: bool,
    paused: bool,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
}

/// A running render. Start it, then read frames as they appear.
pub struct RenderSession {
    config: Arc<Config>,
    pub m4a: PathBuf,
    pub out_dir: PathBuf,
    pub pcm_path: PathBuf,
    pub summary_path: PathBuf,
    pub log_path: PathBuf,
    pub expected_duration_s: Option<f64>,
    pub script: PathBuf,
    state: Mutex<SessionState>,
}

impl RenderSession {
    pub const POLL_S: f64 = 0.02;
    pub const STOP_GRACE_S: f64 = 5.0;

    /// A render of `m4a` into `out_dir` with the configured render script. `expected_duration_s`
    /// (from the media cache's meta) gives `total_frames()` an estimate until the render is done.
    pub fn new(
        config: Arc<Config>,
        m4a: impl Into<PathBuf>,
        out_dir: impl Into<PathBuf>,
        expected_duration_s: Option<f64>,
    ) -> RenderSession {
        let out_dir = out_dir.into();
        RenderSession {
            script: config.render_script.clone(),
            config,
            m4a: m4a.into(),
            pcm_path: out_dir.join(PCM_NAME),
            summary_path: out_dir.join(SUMMARY_NAME),
            log_path: out_dir.join(LOG_NAME),
            out_dir,
            expected_duration_s,
            state: Mutex::default(),
        }
    }

    /// Run another script than the configured one (tests).
    pub fn with_script(mut self, script: impl Into<PathBuf>) -> RenderSession {
        self.script = script.into();
        self
    }

    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn start(&self) -> Result<(), RenderError> {
        let mut state = self.lock();
        if state.process.is_some() {
            return Err(RenderError::Start("render already started".to_string()));
        }
        if !self.script.is_file() {
            return Err(RenderError::Start(format!("render script missing: {}", self.script.display())));
        }
        if !self.m4a.is_file() {
            return Err(RenderError::Start("media file missing".to_string()));
        }
        util::create_private_dirs(&self.out_dir).map_err(|error| {
            RenderError::Start(format!("could not create the output directory ({})", os_error_name(&error)))
        })?;
        if self.pcm_path.exists() {
            return Err(RenderError::Start("output directory already holds a render; claim a fresh one".to_string()));
        }
        let process = self
            .spawn()
            .map_err(|error| RenderError::Start(format!("could not start the render ({})", os_error_name(&error))))?;
        state.process = Some(process);
        state.started_at = Some(Instant::now());
        Ok(())
    }

    fn spawn(&self) -> io::Result<Child> {
        let log = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&self.log_path)?;
        let mut renderer = Command::new("bash");
        renderer
            .arg(&self.script)
            .arg(&self.m4a)
            .arg(&self.out_dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .current_dir(&self.out_dir);
        // read by the lab script only
        renderer.env("CAVERN_LAB_OPTIONS", self.config.lab_render_options());
        private_umask(&mut renderer);
        renderer.spawn()
    }

    // --- state

    /// Finalize once the process has exited (lock held).
    fn poll(&self, state: &mut SessionState) {
        if state.done || state.error.is_some() {
            return;
        }
        let Some(process) = state.process.as_mut() else { return };
        let code = match process.try_wait() {
            Ok(None) => return,
            Ok(Some(status)) => returncode(status),
            // reaped elsewhere: the summary decides, as Python's Popen assumes
            Err(_) => 0,
        };
        state.returncode = Some(code);
        state.finished_at = Some(Instant::now());
        if state.stopped {
            state.error = Some("render stopped".to_string());
            return;
        }
        if code != 0 {
            let tail = last_line(&self.log_path, 300);
            let detail = if tail.is_empty() { String::new() } else { format!(": {tail}") };
            state.error = Some(format!("render exited with status {code}{detail}"));
            return;
        }
        match load_summary(&self.out_dir) {
            Ok(summary) => {
                stamp_generation(&self.config, &self.out_dir, Some(&summary));
                state.summary = Some(summary);
                state.done = true;
            }
            Err(error) => {
                let tail = last_line(&self.log_path, 300);
                let detail = if tail.is_empty() { String::new() } else { format!(" ({tail})") };
                state.error = Some(format!("render validation failed: {error}{detail}"));
            }
        }
    }

    fn polled(&self) -> MutexGuard<'_, SessionState> {
        let mut state = self.lock();
        self.poll(&mut state);
        state
    }

    /// The renderer's exit status once it has ended (minus the signal number when a signal
    /// ended it).
    pub fn returncode(&self) -> Option<i32> {
        self.polled().returncode
    }

    /// The validated summary of a finished render.
    pub fn summary(&self) -> Option<Map<String, Value>> {
        self.polled().summary.clone()
    }

    pub fn running(&self) -> bool {
        let mut state = self.polled();
        state.process.as_mut().is_some_and(|process| matches!(process.try_wait(), Ok(None)))
    }

    pub fn started_at(&self) -> Option<Instant> {
        self.lock().started_at
    }

    pub fn finished_at(&self) -> Option<Instant> {
        self.lock().finished_at
    }

    /// Block until at least `frames` are available, the render finished or failed, or `timeout`
    /// seconds passed.
    ///
    /// Returns true when the frames are available or the render completed successfully (the
    /// file then holds every frame there is); false on failure or timeout.
    pub fn wait_for(&self, frames: u64, timeout: f64) -> bool {
        let deadline = Instant::now().checked_add(seconds(timeout));
        loop {
            if self.frames_available() >= frames {
                return true;
            }
            if self.error().is_some() {
                return false;
            }
            if self.done() {
                return true; // complete: the file holds every frame there is
            }
            let mut pause = seconds(Self::POLL_S);
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                pause = pause.min(deadline - now);
            }
            std::thread::sleep(pause);
        }
    }

    pub fn paused(&self) -> bool {
        self.lock().paused
    }

    /// Freeze the renderer (SIGSTOP) so another render gets the CPU; the file keeps what it has.
    pub fn pause(&self) {
        let mut state = self.lock();
        let SessionState { process, paused, .. } = &mut *state;
        let Some(process) = process.as_mut() else { return };
        if *paused || !matches!(process.try_wait(), Ok(None)) {
            return;
        }
        if signal(process, libc::SIGSTOP) {
            *paused = true;
        }
    }

    pub fn resume(&self) {
        let mut state = self.lock();
        let SessionState { process, paused, .. } = &mut *state;
        let Some(process) = process.as_mut() else { return };
        if !*paused {
            return;
        }
        *paused = false;
        if matches!(process.try_wait(), Ok(None)) {
            signal(process, libc::SIGCONT);
        }
    }

    /// Terminate the renderer; the partial directory is left for `PcmCache::claim()` to clean.
    pub fn stop(&self) {
        let mut state = self.lock();
        {
            let SessionState { process, stopped, paused, .. } = &mut *state;
            let Some(process) = process.as_mut() else { return };
            if matches!(process.try_wait(), Ok(None)) {
                *stopped = true;
                if *paused {
                    // a stopped process would only see SIGTERM after SIGCONT
                    signal(process, libc::SIGCONT);
                    *paused = false;
                }
                terminate(process, seconds(Self::STOP_GRACE_S));
            }
        }
        self.poll(&mut state);
    }
}

impl Drop for RenderSession {
    /// A renderer that outlives its session is left to finish. It is woken when it was frozen,
    /// and a thread waits for its end.
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(mut process) = state.process.take() else { return };
        if !matches!(process.try_wait(), Ok(None)) {
            return;
        }
        if state.paused {
            signal(&process, libc::SIGCONT);
        }
        let _ = std::thread::Builder::new().name("amui-render-end".to_string()).spawn(move || {
            let _ = process.wait();
        });
    }
}

impl PcmSource for RenderSession {
    fn pcm_path(&self) -> &Path {
        &self.pcm_path
    }

    fn channels(&self) -> usize {
        config::CHANNELS
    }

    fn frames_available(&self) -> u64 {
        let Ok(info) = fs::metadata(&self.pcm_path) else { return 0 };
        let frames = info.len() / FRAME_BYTES as u64;
        // a finished render has the frames its summary names, whatever else is in the file
        let state = self.polled();
        match state.summary.as_ref().and_then(summary_frames) {
            Some(total) if state.done => frames.min(total),
            _ => frames,
        }
    }

    fn total_frames(&self) -> Option<u64> {
        let state = self.polled();
        if let Some(summary) = &state.summary {
            return summary_frames(summary);
        }
        drop(state);
        let estimate = (self.expected_duration_s? * f64::from(config::SAMPLE_RATE)).round_ties_even();
        (estimate.is_finite() && estimate > 0.0).then_some(estimate as u64)
    }

    fn done(&self) -> bool {
        self.polled().done
    }

    fn error(&self) -> Option<String> {
        self.polled().error.clone()
    }
}

/// A finished 16-channel render loaded from the PCM cache (validates `render-summary.json`).
#[derive(Debug, Clone)]
pub struct CompleteRender {
    pub out_dir: PathBuf,
    pub pcm_path: PathBuf,
    pub summary: Map<String, Value>,
    frames: u64,
}

impl CompleteRender {
    pub fn new(out_dir: impl Into<PathBuf>) -> Result<CompleteRender, RenderError> {
        let out_dir = out_dir.into();
        let summary = load_summary(&out_dir)?;
        let frames = summary_frames(&summary).ok_or_else(|| invalid("render summary field frames is not a number"))?;
        Ok(CompleteRender { pcm_path: out_dir.join(PCM_NAME), out_dir, summary, frames })
    }
}

impl PcmSource for CompleteRender {
    fn pcm_path(&self) -> &Path {
        &self.pcm_path
    }

    fn channels(&self) -> usize {
        config::CHANNELS
    }

    fn frames_available(&self) -> u64 {
        self.frames
    }

    fn total_frames(&self) -> Option<u64> {
        Some(self.frames)
    }

    fn done(&self) -> bool {
        true
    }

    fn error(&self) -> Option<String> {
        None
    }
}

/// A complete interleaved f32le file with a known channel count (stereo decode, binaural
/// render).
#[derive(Debug, Clone)]
pub struct PcmFile {
    pub pcm_path: PathBuf,
    pub channels: usize,
    /// `meta.json` next to the file; empty when there is none.
    pub meta: Map<String, Value>,
    frames: u64,
}

impl PcmFile {
    pub fn new(pcm_path: impl Into<PathBuf>, channels: usize) -> Result<PcmFile, RenderError> {
        let pcm_path = pcm_path.into();
        let meta = read_meta(&pcm_path).unwrap_or_default();
        let frames = validate_pcm_file(&pcm_path, channels)?;
        Ok(PcmFile { pcm_path, channels, meta, frames })
    }
}

impl PcmSource for PcmFile {
    fn pcm_path(&self) -> &Path {
        &self.pcm_path
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn frames_available(&self) -> u64 {
        self.frames
    }

    fn total_frames(&self) -> Option<u64> {
        Some(self.frames)
    }

    fn done(&self) -> bool {
        true
    }

    fn error(&self) -> Option<String> {
        None
    }
}

// --- helper programs (shared with acquire and lossless) -------------------------

/// A time span of `value` seconds; negative and not-a-number count as none, too large as
/// forever.
pub(crate) fn seconds(value: f64) -> Duration {
    Duration::try_from_secs_f64(value.max(0.0)).unwrap_or(Duration::MAX)
}

/// Files the program creates are readable by the owner only (Python: `Popen(umask=0o077)`).
pub(crate) fn private_umask(program: &mut Command) -> &mut Command {
    // SAFETY: umask() is async-signal-safe and changes nothing but the new process's own mask.
    unsafe {
        program.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        })
    }
}

/// Python's `returncode`: the exit status, or minus the number of the signal that ended the
/// program.
pub(crate) fn returncode(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(0))
}

/// Wait for a program to end, at most `timeout`: its status, or `None` while it still runs.
pub(crate) fn wait_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now().checked_add(timeout);
    let mut pause = Duration::from_micros(500);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let mut nap = pause;
        if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            nap = nap.min(deadline - now);
        }
        std::thread::sleep(nap);
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

/// Send a signal to a program this process started and has not waited for yet.
fn signal(child: &Child, number: libc::c_int) -> bool {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else { return false };
    // SAFETY: kill() has no memory effects. Callers check that the child has not been waited
    // for, so the process id still names it.
    unsafe { libc::kill(pid, number) == 0 }
}

/// Ask a program to end (SIGTERM, Python's `terminate()`); kill it when it is still there after
/// `grace`. Returns its status when it has one.
pub(crate) fn terminate(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }
    signal(child, libc::SIGTERM);
    match wait_timeout(child, grace) {
        Ok(Some(status)) => Some(status),
        _ => {
            let _ = child.kill();
            child.wait().ok()
        }
    }
}

/// The name Python gives an operating-system error (`type(error).__name__`); the engine's
/// messages carry it.
pub(crate) fn os_error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::AlreadyExists => "FileExistsError",
        io::ErrorKind::NotADirectory => "NotADirectoryError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        io::ErrorKind::TimedOut => "TimeoutError",
        io::ErrorKind::Interrupted => "InterruptedError",
        io::ErrorKind::BrokenPipe => "BrokenPipeError",
        io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
        io::ErrorKind::ConnectionReset => "ConnectionResetError",
        io::ErrorKind::ConnectionAborted => "ConnectionAbortedError",
        io::ErrorKind::WouldBlock => "BlockingIOError",
        _ => "OSError",
    }
}

/// The lines of a text as Python's `str.splitlines()` cuts them (`\r\n` is one line end; a line
/// end at the very end adds no empty line).
pub(crate) fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let ends_line = matches!(
            character,
            '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if !ends_line {
            continue;
        }
        lines.push(&text[start..index]);
        start = index + character.len_utf8();
        if character == '\r' && matches!(characters.peek(), Some((_, '\n'))) {
            characters.next();
            start += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The last non-blank line of a log file (its last 8 KiB), at most `limit` characters.
pub(crate) fn last_line(path: &Path, limit: usize) -> String {
    let read = || -> io::Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        let size = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(size.saturating_sub(8192)))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        Ok(tail)
    };
    let Ok(tail) = read() else { return String::new() };
    let text = String::from_utf8_lossy(&tail);
    let last = splitlines(&text).into_iter().map(str::trim).rev().find(|line| !line.is_empty());
    last.map(|line| line.chars().take(limit).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::testing::scratch_config;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// A scratch am-ui folder; `root` is a folder for the test's own files.
    struct Scratch {
        _dir: tempfile::TempDir,
        config: Arc<Config>,
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Scratch {
            let (dir, config) = scratch_config();
            let root = config.tmp_dir.join("test-render");
            fs::create_dir_all(&root).unwrap();
            Scratch { _dir: dir, config, root }
        }

        fn write_script(&self, name: &str, body: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, format!("#!/usr/bin/env bash\nset -eu\nm4a=$1\nout=$2\n{body}")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }

        fn media(&self) -> PathBuf {
            let path = self.root.join("in.m4a");
            fs::write(&path, b"x").unwrap();
            path
        }

        fn session(&self, script: &Path, expected_duration_s: Option<f64>) -> RenderSession {
            RenderSession::new(self.config.clone(), self.media(), self.root.join("out"), expected_duration_s)
                .with_script(script)
        }
    }

    /// A fake render entry: a PCM file of `pcm_frames` frames and a summary (fields
    /// overridable).
    fn write_entry(directory: &Path, pcm_frames: u64, overrides: Value) -> Value {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join(PCM_NAME), vec![0u8; pcm_frames as usize * 64]).unwrap();
        let mut summary = json!({
            "frames": pcm_frames, "bytes": pcm_frames * 64, "channels": 16, "sample_rate": 48000,
            "channel_fallback_blocks": 0, "nonfinite_samples": 0, "realtime_factor": 20.0,
        });
        for (key, value) in overrides.as_object().unwrap() {
            summary[key] = value.clone();
        }
        fs::write(directory.join(SUMMARY_NAME), summary.to_string()).unwrap();
        summary
    }

    // --- summary validation

    #[test]
    fn valid() {
        let scratch = Scratch::new();
        write_entry(&scratch.root.join("ok"), 480, json!({}));
        assert_eq!(load_summary(&scratch.root.join("ok")).unwrap()["frames"], 480);
        let done = CompleteRender::new(scratch.root.join("ok")).unwrap();
        assert_eq!(
            (done.frames_available(), done.total_frames(), done.done(), done.error(), done.channels()),
            (480, Some(480), true, None, 16)
        );
    }

    #[test]
    fn rejections() {
        let scratch = Scratch::new();
        let cases = [
            ("fallback", json!({"channel_fallback_blocks": 3})),
            ("nonfinite", json!({"nonfinite_samples": 1})),
            ("bytes", json!({"bytes": 480 * 64 - 64})),
            ("channels", json!({"channels": 2})),
            ("rate", json!({"sample_rate": 44100})),
            ("zero", json!({"frames": 0, "bytes": 0})),
        ];
        for (name, overrides) in cases {
            write_entry(&scratch.root.join(name), 480, overrides);
            assert!(matches!(CompleteRender::new(scratch.root.join(name)), Err(RenderError::Invalid(_))), "{name}");
        }
        write_entry(&scratch.root.join("short"), 480, json!({}));
        fs::write(scratch.root.join("short").join(PCM_NAME), vec![0u8; 100]).unwrap();
        assert!(load_summary(&scratch.root.join("short")).unwrap_err().to_string().contains("100 bytes"));
        fs::create_dir(scratch.root.join("nosummary")).unwrap();
        assert!(load_summary(&scratch.root.join("nosummary")).unwrap_err().to_string().contains("missing"));
        write_entry(&scratch.root.join("garbage"), 480, json!({}));
        fs::write(scratch.root.join("garbage").join(SUMMARY_NAME), "{not json").unwrap();
        assert!(load_summary(&scratch.root.join("garbage")).unwrap_err().to_string().contains("unreadable"));
    }

    /// The render an older workspace keeps as a fixture, read-only.
    #[test]
    #[ignore = "needs the workspace's render fixture (reports/music-web-download-20261105T173752Z-2/render-916)"]
    fn existing_render_validates() {
        let workspace_render =
            Config::from_process().workspace.join("reports/music-web-download-20261105T173752Z-2/render-916");
        if !workspace_render.is_dir() {
            eprintln!("skipped: workspace render fixture not present");
            return;
        }
        let stamps = || {
            let mut stamps: Vec<(std::ffi::OsString, i64, i64)> = fs::read_dir(&workspace_render)
                .unwrap()
                .map(|entry| entry.unwrap())
                .map(|entry| {
                    let info = entry.metadata().unwrap();
                    (entry.file_name(), info.mtime(), info.mtime_nsec())
                })
                .collect();
            stamps.sort();
            stamps
        };
        let before = stamps();
        let done = CompleteRender::new(&workspace_render).unwrap();
        assert_eq!(done.total_frames(), Some(10026240));
        assert_eq!(done.frames_available(), 10026240);
        assert_eq!(done.summary["channel_fallback_blocks"], 0);
        assert_eq!(done.summary["bytes"], 641679360);
        assert_eq!(fs::metadata(&done.pcm_path).unwrap().len(), 641679360);
        let duration = done.total_frames().unwrap() as f64 / f64::from(config::SAMPLE_RATE);
        assert!((duration - 208.88).abs() < 0.0005);
        assert_eq!(before, stamps(), "fixture must stay untouched");
    }

    // --- PcmFile

    #[test]
    fn size_based_frames() {
        let scratch = Scratch::new();
        let pcm = scratch.root.join("stereo.f32le");
        fs::write(&pcm, vec![0u8; 8 * 1000]).unwrap();
        let source = PcmFile::new(&pcm, 2).unwrap();
        assert_eq!(
            (source.frames_available(), source.total_frames(), source.channels(), source.done(), source.error()),
            (1000, Some(1000), 2, true, None)
        );
        assert_eq!(source.meta, Map::new());
    }

    #[test]
    fn meta_json() {
        let scratch = Scratch::new();
        let pcm = scratch.root.join("stereo.f32le");
        fs::write(&pcm, vec![0u8; 8 * 1000]).unwrap();
        let meta = pcm.with_file_name("meta.json");
        fs::write(&meta, json!({"channels": 2, "frames": 1000, "source": "aac"}).to_string()).unwrap();
        let source = PcmFile::new(&pcm, 2).unwrap();
        assert_eq!(source.total_frames(), Some(1000));
        assert_eq!(source.meta["source"], "aac");
        fs::write(&meta, json!({"channels": 2, "frames": 999}).to_string()).unwrap();
        assert!(matches!(PcmFile::new(&pcm, 2), Err(RenderError::Invalid(_))));
        fs::write(&meta, json!({"channels": 16, "frames": 1000}).to_string()).unwrap();
        assert!(matches!(PcmFile::new(&pcm, 2), Err(RenderError::Invalid(_))));
    }

    #[test]
    fn bad_sizes() {
        let scratch = Scratch::new();
        let pcm = scratch.root.join("stereo.f32le");
        fs::write(&pcm, vec![0u8; 12]).unwrap();
        assert!(matches!(PcmFile::new(&pcm, 2), Err(RenderError::Invalid(_))));
        fs::write(&pcm, b"").unwrap();
        assert!(matches!(PcmFile::new(&pcm, 2), Err(RenderError::Invalid(_))));
        assert!(matches!(PcmFile::new(scratch.root.join("missing.f32le"), 2), Err(RenderError::Invalid(_))));
    }

    // --- RenderSession driven by stand-in render scripts

    const SUCCESS_SCRIPT: &str = r#"
echo "JOC detected: fake"
: > "$out/atmos-916.f32le"
for i in 1 2 3; do
    head -c $((4800 * 64)) /dev/zero >> "$out/atmos-916.f32le"
    echo "Rendered chunk $i"
    sleep 0.15
done
cat > "$out/render-summary.json" <<'EOF'
{"frames": 14400, "bytes": 921600, "channels": 16, "sample_rate": 48000,
 "channel_fallback_blocks": 0, "nonfinite_samples": 0, "elapsed_seconds": 0.5,
 "realtime_factor": 0.6, "channels_stats": []}
EOF
echo "Finished: fake"
"#;

    #[test]
    fn progressive_success() {
        let scratch = Scratch::new();
        let script = scratch.write_script("ok.sh", SUCCESS_SCRIPT);
        let session = scratch.session(&script, Some(0.3));
        assert_eq!(session.total_frames(), Some(14400)); // estimate from the duration before completion
        session.start().unwrap();
        assert!(session.wait_for(4800, 5.0));
        assert!(session.frames_available() >= 4800);
        assert!(session.wait_for(10u64.pow(9), 5.0)); // returns true once complete
        assert!(session.done());
        assert_eq!(session.error(), None);
        assert_eq!(session.total_frames(), Some(14400));
        assert_eq!(session.frames_available(), 14400);
        assert_eq!(session.summary().unwrap()["frames"], 14400);
        assert_eq!(session.returncode(), Some(0));
        assert_eq!(fs::metadata(&session.log_path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(fs::read_to_string(&session.log_path).unwrap().contains("Finished"));
        let done = CompleteRender::new(scratch.root.join("out")).unwrap();
        assert_eq!(done.total_frames(), Some(14400));
        assert!(matches!(session.start(), Err(RenderError::Start(_)))); // a session runs once
    }

    #[test]
    fn total_frames_unknown_without_duration() {
        let scratch = Scratch::new();
        let session =
            RenderSession::new(scratch.config.clone(), scratch.root.join("in.m4a"), scratch.root.join("out"), None)
                .with_script(scratch.root.join("none.sh"));
        assert_eq!(session.total_frames(), None);
        assert_eq!(session.frames_available(), 0);
        assert!(matches!(session.start(), Err(RenderError::Start(_)))); // script missing
    }

    #[test]
    fn refuses_existing_pcm() {
        let scratch = Scratch::new();
        let script = scratch.write_script("ok.sh", SUCCESS_SCRIPT);
        let out = scratch.root.join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join(PCM_NAME), vec![0u8; 64]).unwrap();
        assert!(matches!(scratch.session(&script, None).start(), Err(RenderError::Start(_))));
    }

    #[test]
    fn exit_status_error_with_last_log_line() {
        let scratch = Scratch::new();
        let script = scratch.write_script(
            "fail.sh",
            "echo \"starting\"\necho \"Input did not initialize as object-based E-AC-3\"\nexit 3\n",
        );
        let session = scratch.session(&script, Some(1.0));
        session.start().unwrap();
        assert!(!session.wait_for(1, 5.0));
        assert!(!session.done());
        let error = session.error().unwrap();
        assert!(error.contains("status 3"), "{error}");
        assert!(error.contains("object-based E-AC-3"), "{error}");
        assert_eq!(session.total_frames(), Some(48000)); // still the estimate: no summary
    }

    #[test]
    fn validation_failure_after_exit_zero() {
        let scratch = Scratch::new();
        let bad = SUCCESS_SCRIPT.replace("\"channel_fallback_blocks\": 0", "\"channel_fallback_blocks\": 7");
        let script = scratch.write_script("bad.sh", &bad);
        let session = scratch.session(&script, None);
        session.start().unwrap();
        assert!(session.wait_for(14400, 5.0)); // frames arrive before the verdict
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.error().is_none() && !session.done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!session.done());
        assert!(session.error().unwrap().contains("fallback"));
        assert_eq!(session.returncode(), Some(0));
    }

    #[test]
    fn pause_freezes_and_resume_continues() {
        let scratch = Scratch::new();
        // the shell writes the file itself (printf is part of it), as the real renderer does:
        // what is frozen is what makes the file grow
        let script = scratch.write_script(
            "slowgrow.sh",
            r#"
: > "$out/atmos-916.f32le"
chunk=$(printf '%030720d' 0)
for i in $(seq 1 40); do
    printf '%s' "$chunk" >> "$out/atmos-916.f32le"
    sleep 0.05
done
"#,
        );
        let session = scratch.session(&script, Some(0.4));
        session.start().unwrap();
        assert!(session.wait_for(960, 5.0));
        session.pause();
        assert!(session.paused());
        assert!(session.running(), "a frozen renderer is still a running process");
        // a write that was under way when the signal arrived is in the file by now
        std::thread::sleep(Duration::from_millis(100));
        let frozen_at = session.frames_available();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(session.frames_available(), frozen_at, "no growth while frozen");
        session.resume();
        assert!(!session.paused());
        assert!(session.wait_for(frozen_at + 960, 5.0), "growth continues after resume");
        session.pause();
        let started = Instant::now();
        session.stop(); // stop() must get through to a frozen process
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!session.running());
        assert_eq!(session.error().as_deref(), Some("render stopped"));
    }

    #[test]
    fn stop_terminates() {
        let scratch = Scratch::new();
        // like the real scripts, this one ends by becoming its long-running program
        let script = scratch.write_script("slow.sh", ": > \"$out/atmos-916.f32le\"\nexec sleep 30\n");
        let session = scratch.session(&script, None);
        session.start().unwrap();
        assert!(session.running());
        let started = Instant::now();
        session.stop();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!session.running());
        assert_eq!(session.error().as_deref(), Some("render stopped"));
        assert!(!session.done());
        session.stop(); // idempotent
    }

    // --- beyond the Python tests

    #[test]
    fn sources_can_be_shared_between_threads() {
        fn shared<T: PcmSource + 'static>() {}
        shared::<RenderSession>();
        shared::<CompleteRender>();
        shared::<PcmFile>();
    }

    #[test]
    fn a_session_that_goes_away_leaves_no_stopped_or_defunct_renderer() {
        let scratch = Scratch::new();
        let script = scratch.write_script(
            "brief.sh",
            ": > \"$out/atmos-916.f32le\"\nsleep 0.3\nhead -c 640 /dev/zero >> \"$out/atmos-916.f32le\"\n",
        );
        let session = scratch.session(&script, None);
        session.start().unwrap();
        session.pause();
        assert!(session.paused());
        let pid = session.lock().process.as_ref().map(Child::id).unwrap();
        drop(session);
        // the renderer was woken and finishes its work; once it has ended, its process id is free
        let pcm = scratch.root.join("out").join(PCM_NAME);
        let deadline = Instant::now() + Duration::from_secs(5);
        let gone = || {
            // SAFETY: signal 0 delivers nothing; it only asks whether the process exists.
            unsafe { libc::kill(pid as libc::pid_t, 0) != 0 }
        };
        while !gone() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(), "the renderer is still there");
        assert_eq!(fs::metadata(&pcm).unwrap().len(), 640);
    }

    #[test]
    fn a_finished_render_carries_its_generation() {
        let scratch = Scratch::new();
        let script = scratch.write_script("ok.sh", SUCCESS_SCRIPT);
        let session = scratch.session(&script, None);
        let out = scratch.root.join("out");
        assert_eq!(render_generation_of(&out), None);
        session.start().unwrap();
        assert!(session.wait_for(u64::MAX, 5.0));
        assert_eq!(render_generation_of(&out).as_deref(), Some("cavern-lab-2"));
        assert!(session.summary().unwrap().get("generation").is_none()); // what the renderer wrote
        let stamped = load_summary(&out).unwrap();
        assert_eq!(stamped["render_script"], json!(scratch.config.render_script.to_str().unwrap()));
        assert_eq!(stamped["render_options"], json!(scratch.config.lab_render_options()));
        // the renderer's own fields keep their order, the stamp comes last, two spaces of indentation
        let text = fs::read_to_string(&session.summary_path).unwrap();
        assert!(text.starts_with("{\n  \"frames\": 14400,\n  \"bytes\": 921600,\n"), "{text}");
        assert!(text.contains("  \"channels_stats\": [],\n  \"generation\": \"cavern-lab-2\",\n"), "{text}");
        assert!(text.ends_with("\n}\n"));
        assert_eq!(fs::metadata(&session.summary_path).unwrap().permissions().mode() & 0o777, 0o600);
        // the same generation is not written again; another one is
        let written = fs::metadata(&session.summary_path).unwrap().ino();
        stamp_generation(&scratch.config, &out, None);
        assert_eq!(fs::metadata(&session.summary_path).unwrap().ino(), written);
        let older = fs::read_to_string(&session.summary_path).unwrap().replace("cavern-lab-2", "cavern-lab-1");
        fs::write(&session.summary_path, older).unwrap();
        assert_eq!(render_generation_of(&out).as_deref(), Some("cavern-lab-1"));
        stamp_generation(&scratch.config, &out, None);
        assert_eq!(render_generation_of(&out).as_deref(), Some("cavern-lab-2"));
        assert_eq!(CompleteRender::new(&out).unwrap().total_frames(), Some(14400));
        stamp_generation(&scratch.config, &scratch.root.join("absent"), None); // nothing to stamp, no error
    }

    #[test]
    fn the_renderer_gets_its_options_and_a_private_umask() {
        let scratch = Scratch::new();
        let script = scratch.write_script(
            "env.sh",
            "echo \"options=$CAVERN_LAB_OPTIONS\"\necho \"umask=$(umask)\"\necho \"cwd=$(pwd -P)\"\nexit 9\n",
        );
        let session = scratch.session(&script, None);
        session.start().unwrap();
        assert!(!session.wait_for(1, 5.0));
        assert_eq!(session.returncode(), Some(9));
        let log = fs::read_to_string(&session.log_path).unwrap();
        let options = scratch.config.lab_render_options();
        assert!(log.contains(&format!("options={options}\n")), "{log}");
        assert!(log.contains("umask=0077\n"), "{log}");
        let out = fs::canonicalize(scratch.root.join("out")).unwrap();
        assert!(log.contains(&format!("cwd={}\n", out.display())), "{log}");
    }

    #[test]
    fn summary_numbers_are_read_as_python_reads_them() {
        let scratch = Scratch::new();
        let entry = scratch.root.join("loose");
        write_entry(&entry, 480, json!({"frames": "480", "bytes": 30720.0, "channels": " 16 "}));
        assert_eq!(CompleteRender::new(&entry).unwrap().total_frames(), Some(480));
        for (field, value, message) in [
            ("frames", json!(null), "render summary field frames is not a number"),
            ("frames", json!([480]), "render summary field frames is not a number"),
            ("sample_rate", json!("48k"), "render summary field sample_rate is not a number"),
            ("channel_fallback_blocks", json!("3"), "3 channel-only fallback blocks"),
            ("nonfinite_samples", json!(2.0), "2.0 non-finite samples"),
        ] {
            write_entry(&entry, 480, json!({ field: value }));
            assert_eq!(load_summary(&entry).unwrap_err().to_string(), message);
        }
        fs::write(entry.join(SUMMARY_NAME), "[1, 2]").unwrap();
        assert_eq!(load_summary(&entry).unwrap_err().to_string(), "render summary is not a JSON object");
        fs::remove_file(entry.join(PCM_NAME)).unwrap();
        write_summary_only(&entry);
        assert_eq!(load_summary(&entry).unwrap_err().to_string(), "render PCM file missing");
        assert_eq!(read_meta(&entry.join("stereo.f32le")), None);
    }

    fn write_summary_only(directory: &Path) {
        fs::write(directory.join(SUMMARY_NAME), json!({"frames": 480}).to_string()).unwrap();
    }

    #[test]
    fn log_tail_and_line_ends() {
        let scratch = Scratch::new();
        let log = scratch.root.join("some.log");
        assert_eq!(last_line(&log, 300), "");
        fs::write(&log, "first\r\n  second line  \n\n \n").unwrap();
        assert_eq!(last_line(&log, 300), "second line");
        assert_eq!(last_line(&log, 6), "second");
        fs::write(&log, "10%\r50%\r100% done").unwrap();
        assert_eq!(last_line(&log, 300), "100% done");
        let mut long = "x".repeat(10_000);
        long.push_str("\nlast\n");
        fs::write(&log, long).unwrap();
        assert_eq!(last_line(&log, 300), "last");
        assert_eq!(splitlines("a\r\nb\rc\n\nd\u{2028}e\n"), ["a", "b", "c", "", "d", "e"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(os_error_name(&io::Error::from(io::ErrorKind::NotFound)), "FileNotFoundError");
        assert_eq!(os_error_name(&io::Error::other("x")), "OSError");
    }
}
