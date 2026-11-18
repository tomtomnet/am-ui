//! Stand-ins for the player's tests: a pipeline (acquire, render, downmix, loudness), an output,
//! a PCM cache and a radio that only keep notes, and the harness that puts a player on top of
//! them. Nothing here touches the network, a sink or the data folder: every path points into a
//! scratch folder, and most of them at files that do not exist.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::sleep;
use std::time::{Duration, Instant};

use amui::acquire::{AcquireError, Acquirer, ProgressFn};
use amui::api::{ApiError, Seed};
use amui::cache::{CacheError, MediaCache};
use amui::config::{Config, SAMPLE_RATE};
use amui::downmix::{Applied, Downmix, DownmixError, Output as Printed, Paths, Profile, RunError};
use amui::lossless::{Lossless, LosslessError};
use amui::loudness::{LoudnessError, Measurement};
use amui::output::{OutputError, PlaybackLog};
use amui::player::{
    EnginePipeline, ErrorCallback, LoudnessStream, Output, OutputFactory, PcmStore, Pipeline, Player, PlayerEvent,
    PlayerOptions, Radio, Source, SourceCallback, Tuning, fallback_profiles,
};
use amui::render::{PcmSource, RenderError};
use amui::types::{AcquireResult, PlaybackSnapshot, PlayerState, Station, Track};
use amui::util::Event;
use amui::web::{Call as Request, Http, Progress, Source as TokenSource, WebError};
use serde_json::{Map, Value, json};

pub const RATE: u64 = SAMPLE_RATE as u64;
/// How long a test waits for something the player does on its own threads.
pub const PATIENCE: Duration = Duration::from_secs(30);

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn track(catalog_id: &str) -> Track {
    Track::new(Some(catalog_id), format!("Song {catalog_id}"), "Artist")
}

pub fn named(catalog_id: &str, name: &str) -> Track {
    Track::new(Some(catalog_id), name, "Artist")
}

/// The name a stand-in source was made with (its file's name without the ending).
pub fn name_of(source: &Arc<dyn PcmSource>) -> String {
    let name = source.pcm_path().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    name.strip_suffix(".f32le").unwrap_or(&name).to_string()
}

// --- sources ---------------------------------------------------------------------------

#[derive(Default)]
struct Growth {
    started_at: Option<Instant>,
    paused: bool,
    paused_for: Duration,
    pause_started: Option<Instant>,
    pauses: u32,
    stopped: bool,
    frames: u64,
}

/// A complete file or, with a `rate`, a render whose file grows at that many frames per second
/// of wall time once it is started.
pub struct FakeSource {
    pub name: String,
    pcm_path: PathBuf,
    channels: usize,
    total: u64,
    summary: Option<Map<String, Value>>,
    rate: Option<f64>,
    start_delay: f64,
    growth: Mutex<Growth>,
    /// The other growing renders that were running when this one was started.
    pub running_at_start: Mutex<Vec<String>>,
}

impl FakeSource {
    fn new(base: &Path, name: &str, channels: usize, frames: u64, summary: Option<Value>) -> FakeSource {
        FakeSource {
            name: name.to_string(),
            pcm_path: base.join("nonexistent").join(format!("{name}.f32le")),
            channels,
            total: frames,
            summary: summary.and_then(|summary| summary.as_object().cloned()),
            rate: None,
            start_delay: 0.0,
            growth: Mutex::default(),
            running_at_start: Mutex::default(),
        }
    }

    fn growing(base: &Path, name: &str, total: u64, rate: f64, start_delay: f64) -> FakeSource {
        let summary = Some(json!({"joc_object_count_max": 15}));
        FakeSource { rate: Some(rate), start_delay, ..FakeSource::new(base, name, 16, total, summary) }
    }

    pub fn is(&self, other: &Arc<dyn PcmSource>) -> bool {
        std::ptr::addr_eq(self as *const FakeSource, Arc::as_ptr(other))
    }

    pub fn stopped(&self) -> bool {
        lock(&self.growth).stopped
    }

    pub fn pauses(&self) -> u32 {
        lock(&self.growth).pauses
    }

    /// Started, not finished, not stopped and not frozen: a render that uses the processor.
    pub fn busy(&self) -> bool {
        let started = lock(&self.growth).started_at.is_some();
        self.rate.is_some() && started && !self.done() && !self.stopped() && !Source::paused(self)
    }

    fn produced(&self, growth: &mut Growth) -> u64 {
        let Some(rate) = self.rate else { return self.total };
        if growth.stopped {
            return growth.frames.min(self.total);
        }
        let elapsed = match growth.started_at {
            None => 0.0,
            Some(started) => {
                let now = growth.pause_started.unwrap_or_else(Instant::now);
                (now - started).as_secs_f64() - growth.paused_for.as_secs_f64() - self.start_delay
            }
        };
        growth.frames = self.total.min((elapsed.max(0.0) * rate) as u64);
        growth.frames
    }
}

impl PcmSource for FakeSource {
    fn pcm_path(&self) -> &Path {
        &self.pcm_path
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn frames_available(&self) -> u64 {
        self.produced(&mut lock(&self.growth))
    }

    fn total_frames(&self) -> Option<u64> {
        Some(self.total)
    }

    fn done(&self) -> bool {
        let mut growth = lock(&self.growth);
        self.rate.is_none() || (!growth.stopped && self.produced(&mut growth) >= self.total)
    }

    fn error(&self) -> Option<String> {
        (self.rate.is_some() && lock(&self.growth).stopped).then(|| "render stopped".to_string())
    }
}

impl Source for FakeSource {
    fn summary(&self) -> Option<Map<String, Value>> {
        self.summary.clone()
    }

    fn started_at(&self) -> Option<Instant> {
        lock(&self.growth).started_at
    }

    fn start(&self) -> Result<(), RenderError> {
        lock(&self.growth).started_at = Some(Instant::now());
        Ok(())
    }

    fn wait_for(&self, frames: u64, timeout: f64) -> bool {
        if self.rate.is_none() {
            return true;
        }
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        while Instant::now() < deadline {
            if self.frames_available() >= frames || self.done() || self.error().is_some() {
                return true;
            }
            sleep(Duration::from_millis(5));
        }
        self.frames_available() >= frames || self.done()
    }

    fn stop(&self) {
        lock(&self.growth).stopped = true;
    }

    fn pause(&self) {
        let mut growth = lock(&self.growth);
        if !growth.paused {
            growth.paused = true;
            growth.pauses += 1;
            growth.pause_started = Some(Instant::now());
        }
    }

    fn resume(&self) {
        let mut growth = lock(&self.growth);
        if growth.paused {
            growth.paused = false;
            if let Some(since) = growth.pause_started.take() {
                growth.paused_for += since.elapsed();
            }
        }
    }

    fn paused(&self) -> bool {
        lock(&self.growth).paused
    }

    fn running(&self) -> Option<bool> {
        self.rate.map(|_| !self.stopped() && !self.done())
    }
}

// --- the output ------------------------------------------------------------------------

/// What the player asked of the output, sources by their names.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Start,
    Stop,
    SetSource(Option<String>, u64),
    SetNextSource(Option<String>),
    Seek(u64),
    Pause,
    Resume,
    SetTrim(String, f64),
    SetSoundCheck(bool),
}

pub struct OutputState {
    pub alive: bool,
    pub paused: bool,
    pub volume: f64,
    pub active_source: Option<Arc<dyn PcmSource>>,
    pub next_source: Option<Arc<dyn PcmSource>>,
    pub handoffs: u64,
    pub position: u64,
    pub calls: Vec<Call>,
    pub starts: u32,
    pub trims: HashMap<String, f64>,
    /// Source name -> the ramp of the last `set_trim()`.
    pub ramps: HashMap<String, f64>,
    /// The trim known when `set_source()` came.
    pub trim_at_start: HashMap<String, Option<f64>>,
    pub sound_check: bool,
    pub buffering: bool,
    pub underruns: u64,
    /// What `start()` fails with.
    pub start_error: Option<OutputError>,
}

#[derive(Default)]
struct Callbacks {
    started: Option<SourceCallback>,
    ended: Option<SourceCallback>,
    error: Option<ErrorCallback>,
}

pub struct FakeOutput {
    state: Mutex<OutputState>,
    callbacks: Mutex<Callbacks>,
}

impl FakeOutput {
    pub fn new(volume: f64) -> FakeOutput {
        FakeOutput {
            state: Mutex::new(OutputState {
                alive: false,
                paused: false,
                volume,
                active_source: None,
                next_source: None,
                handoffs: 0,
                position: 0,
                calls: Vec::new(),
                starts: 0,
                trims: HashMap::new(),
                ramps: HashMap::new(),
                trim_at_start: HashMap::new(),
                sound_check: true,
                buffering: false,
                underruns: 0,
                start_error: None,
            }),
            callbacks: Mutex::default(),
        }
    }

    pub fn st(&self) -> MutexGuard<'_, OutputState> {
        lock(&self.state)
    }

    pub fn active(&self) -> Option<Arc<dyn PcmSource>> {
        self.st().active_source.clone()
    }

    pub fn active_name(&self) -> Option<String> {
        self.active().as_ref().map(name_of)
    }

    pub fn next(&self) -> Option<Arc<dyn PcmSource>> {
        self.st().next_source.clone()
    }

    pub fn next_name(&self) -> Option<String> {
        self.next().as_ref().map(name_of)
    }

    pub fn position(&self) -> u64 {
        self.st().position
    }

    pub fn set_position(&self, frame: u64) {
        self.st().position = frame;
    }

    pub fn calls(&self) -> Vec<Call> {
        self.st().calls.clone()
    }

    pub fn count(&self, wanted: impl Fn(&Call) -> bool) -> usize {
        self.st().calls.iter().filter(|call| wanted(call)).count()
    }

    pub fn set_sources(&self) -> Vec<(Option<String>, u64)> {
        let set = |call: &Call| match call {
            Call::SetSource(name, frame) => Some((name.clone(), *frame)),
            _ => None,
        };
        self.st().calls.iter().filter_map(set).collect()
    }

    pub fn seeks(&self) -> usize {
        self.count(|call| matches!(call, Call::Seek(_)))
    }

    pub fn trim(&self, name: &str) -> Option<f64> {
        self.st().trims.get(name).copied()
    }

    pub fn ramp(&self, name: &str) -> Option<f64> {
        self.st().ramps.get(name).copied()
    }

    pub fn trim_at_start(&self, name: &str) -> Option<f64> {
        self.st().trim_at_start.get(name).copied().flatten()
    }

    /// The feeder reaches the end of the active source.
    pub fn finish_current(&self, gapless: bool) {
        let ended = {
            let mut st = self.st();
            let ended = st.active_source.clone();
            if gapless && st.next_source.is_some() {
                st.active_source = st.next_source.take();
                st.handoffs += 1;
                st.position = 0;
            }
            ended
        };
        if let (Some(ended), Some(callback)) = (ended, &lock(&self.callbacks).ended) {
            callback(ended);
        }
    }

    /// The consumer process dies.
    pub fn die(&self, message: &str) {
        self.st().alive = false;
        if let Some(callback) = &lock(&self.callbacks).error {
            callback(message);
        }
    }
}

impl Output for FakeOutput {
    fn on_source_started(&self, callback: SourceCallback) {
        lock(&self.callbacks).started = Some(callback);
    }

    fn on_source_ended(&self, callback: SourceCallback) {
        lock(&self.callbacks).ended = Some(callback);
    }

    fn on_error(&self, callback: ErrorCallback) {
        lock(&self.callbacks).error = Some(callback);
    }

    fn start(&self) -> Result<(), OutputError> {
        let mut st = self.st();
        if let Some(error) = &st.start_error {
            return Err(error.clone());
        }
        st.alive = true;
        st.starts += 1;
        st.calls.push(Call::Start);
        Ok(())
    }

    fn stop(&self) {
        let mut st = self.st();
        st.alive = false;
        st.calls.push(Call::Stop);
    }

    fn alive(&self) -> bool {
        self.st().alive
    }

    fn set_source(&self, source: Option<Arc<dyn PcmSource>>, start_frame: u64) {
        let name = source.as_ref().map(name_of);
        let mut st = self.st();
        if let Some(name) = &name {
            let known = st.trims.get(name).copied();
            st.trim_at_start.insert(name.clone(), known);
        }
        st.active_source = source;
        st.next_source = None;
        st.position = start_frame;
        st.calls.push(Call::SetSource(name, start_frame));
    }

    fn set_next_source(&self, source: Option<Arc<dyn PcmSource>>) {
        let name = source.as_ref().map(name_of);
        let mut st = self.st();
        st.next_source = source;
        st.calls.push(Call::SetNextSource(name));
    }

    fn seek(&self, frame: u64) {
        let mut st = self.st();
        st.position = frame;
        st.calls.push(Call::Seek(frame));
    }

    fn pause(&self) {
        let mut st = self.st();
        st.paused = true;
        st.calls.push(Call::Pause);
    }

    fn resume(&self) {
        let mut st = self.st();
        st.paused = false;
        st.calls.push(Call::Resume);
    }

    fn set_volume(&self, volume: f64) {
        self.st().volume = volume;
    }

    fn set_trim(&self, source: &Arc<dyn PcmSource>, db: f64, ramp_s: f64) {
        let name = name_of(source);
        let mut st = self.st();
        st.trims.insert(name.clone(), db);
        st.ramps.insert(name.clone(), ramp_s);
        st.calls.push(Call::SetTrim(name, db));
    }

    fn set_sound_check(&self, enabled: bool) {
        let mut st = self.st();
        st.sound_check = enabled;
        st.calls.push(Call::SetSoundCheck(enabled));
    }

    fn active_source(&self) -> Option<Arc<dyn PcmSource>> {
        self.active()
    }

    fn next_source(&self) -> Option<Arc<dyn PcmSource>> {
        self.next()
    }

    fn buffering(&self) -> bool {
        self.st().buffering
    }

    fn underruns(&self) -> u64 {
        self.st().underruns
    }

    fn handoffs(&self) -> u64 {
        self.st().handoffs
    }

    fn position_frames(&self) -> u64 {
        self.st().position
    }

    fn buffered_frames(&self) -> u64 {
        let (active, position) = {
            let st = self.st();
            (st.active_source.clone(), st.position)
        };
        active.map_or(0, |source| source.frames_available().saturating_sub(position))
    }

    fn dump_recent(&self, path: &Path) -> io::Result<PathBuf> {
        fs::write(path, b"RIFF")?;
        Ok(path.to_path_buf())
    }
}

// --- the pipeline ----------------------------------------------------------------------

/// What a stand-in stream meter was fed.
#[derive(Default)]
pub struct StreamNotes {
    pub fed: u64,
    pub feeds: Vec<u64>,
    pub finished: bool,
    pub closed: bool,
}

/// Stands in for the stream meter: the running value is the configured estimate (else the final
/// value).
struct FakeStream {
    pipeline: Arc<FakePipeline>,
    name: String,
    notes: Arc<Mutex<StreamNotes>>,
}

impl LoudnessStream for FakeStream {
    fn fed(&self) -> u64 {
        lock(&self.notes).fed
    }

    fn feed(&mut self, frames_available: u64) -> Result<Option<f64>, LoudnessError> {
        {
            let mut notes = lock(&self.notes);
            notes.fed = notes.fed.max(frames_available);
            let fed = notes.fed;
            notes.feeds.push(fed);
        }
        let st = self.pipeline.st();
        Ok(lookup(&st.estimates, &self.name).or_else(|| lookup(&st.loudness, &self.name).unwrap_or(Some(-16.0))))
    }

    fn finish(&mut self) -> Result<Measurement, LoudnessError> {
        let frames = {
            let mut notes = lock(&self.notes);
            notes.finished = true;
            notes.fed
        };
        let mut st = self.pipeline.st();
        st.measured.push(self.name.clone());
        let lufs = lookup(&st.loudness, &self.name).unwrap_or(Some(-16.0));
        Ok(Measurement { lufs, peak: 0.9, frames, fold: None, measured_at: 0.0 })
    }

    fn close(&mut self) {
        lock(&self.notes).closed = true;
    }
}

/// The value of the last entry whose prefix `name` starts with.
fn lookup<T: Clone>(table: &[(String, T)], name: &str) -> Option<T> {
    table.iter().rev().find(|(prefix, _)| name.starts_with(prefix.as_str())).map(|(_, value)| value.clone())
}

pub struct PipelineState {
    pub no_atmos: HashSet<String>,
    pub fail: HashMap<String, AcquireError>,
    /// Seconds an acquisition takes.
    pub delay: f64,
    pub acquired: Vec<(String, String)>,
    pub decoded: Vec<String>,
    pub rendered: Vec<String>,
    pub binaural: Vec<String>,
    pub applied: Vec<String>,
    pub apply_error: Option<DownmixError>,
    pub current_preset: String,
    pub profiles: Vec<Profile>,
    pub durations: HashMap<String, f64>,
    pub sources: Vec<Arc<FakeSource>>,
    /// Source name prefix -> LUFS (default -16: no trim; `None`: silence).
    pub loudness: Vec<(String, Option<f64>)>,
    /// Source name prefix -> the running value the stream meter reports before the end.
    pub estimates: Vec<(String, f64)>,
    /// False: no stream meter, the complete file is measured once it is there.
    pub stream_meter: bool,
    pub streams: Vec<Arc<Mutex<StreamNotes>>>,
    pub measured: Vec<String>,
    /// Catalog id -> (rate as a multiple of real time, start delay in seconds).
    pub growing: HashMap<String, (f64, f64)>,
    /// The lossless route "is built and signed in".
    pub lossless_ok: bool,
    /// Songs whose lossless acquisition fails.
    pub no_lossless: HashSet<String>,
    /// Songs with ALAC, but none above 48 kHz.
    pub no_hires: HashSet<String>,
    /// The lossless route is out of order: every ALAC acquisition fails with this.
    pub route_failure: Option<String>,
    /// Why the route cannot be used, as the pipeline says it.
    pub lossless_problem: Option<String>,
    pub lossless_shutdowns: u32,
    pub preamp: Option<f64>,
    pub preamps_set: Vec<f64>,
}

pub struct FakePipeline {
    base: PathBuf,
    state: Mutex<PipelineState>,
    me: std::sync::Weak<FakePipeline>,
}

impl FakePipeline {
    pub fn new(base: &Path) -> Arc<FakePipeline> {
        let mut profiles = fallback_profiles();
        profiles.push(Profile {
            key: "binaural".to_string(),
            label: "Binaural (Cavern)".to_string(),
            kind: "render".to_string(),
            description: String::new(),
        });
        Arc::new_cyclic(|me| FakePipeline {
            base: base.to_path_buf(),
            me: me.clone(),
            state: Mutex::new(PipelineState {
                no_atmos: HashSet::new(),
                fail: HashMap::new(),
                delay: 0.0,
                acquired: Vec::new(),
                decoded: Vec::new(),
                rendered: Vec::new(),
                binaural: Vec::new(),
                applied: Vec::new(),
                apply_error: None,
                current_preset: "peqdb".to_string(),
                profiles,
                durations: HashMap::new(),
                sources: Vec::new(),
                loudness: Vec::new(),
                estimates: Vec::new(),
                stream_meter: true,
                streams: Vec::new(),
                measured: Vec::new(),
                growing: HashMap::new(),
                lossless_ok: false,
                no_lossless: HashSet::new(),
                no_hires: HashSet::new(),
                route_failure: None,
                lossless_problem: None,
                lossless_shutdowns: 0,
                preamp: Some(-11.0),
                preamps_set: Vec::new(),
            }),
        })
    }

    pub fn st(&self) -> MutexGuard<'_, PipelineState> {
        lock(&self.state)
    }

    pub fn acquired(&self) -> Vec<(String, String)> {
        self.st().acquired.clone()
    }

    /// How often a variant of a song was acquired.
    pub fn acquisitions(&self, catalog_id: &str, variant: &str) -> usize {
        self.st().acquired.iter().filter(|(id, kind)| id == catalog_id && kind == variant).count()
    }

    pub fn sources(&self) -> Vec<Arc<FakeSource>> {
        self.st().sources.clone()
    }

    /// The sources whose names start with `prefix`, oldest first.
    pub fn sources_named(&self, prefix: &str) -> Vec<Arc<FakeSource>> {
        self.sources().into_iter().filter(|source| source.name.starts_with(prefix)).collect()
    }

    pub fn set_loudness(&self, prefix: &str, lufs: f64) {
        self.st().loudness.push((prefix.to_string(), Some(lufs)));
    }

    pub fn set_estimate(&self, prefix: &str, lufs: f64) {
        self.st().estimates.push((prefix.to_string(), lufs));
    }

    fn source(&self, name: &str, channels: usize, duration: f64, summary: Option<Value>) -> Arc<dyn Source> {
        let source = Arc::new(FakeSource::new(&self.base, name, channels, (duration * RATE as f64) as u64, summary));
        self.st().sources.push(source.clone());
        source
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

impl Pipeline for FakePipeline {
    fn acquire(
        &self,
        catalog_id: &str,
        _media_cache: &MediaCache,
        variant: &str,
        progress: ProgressFn<'_>,
        cancel: &Event,
    ) -> Result<AcquireResult, AcquireError> {
        let delay = {
            let mut st = self.st();
            st.acquired.push((catalog_id.to_string(), variant.to_string()));
            st.delay
        };
        progress("manifest", None);
        let deadline = Instant::now() + Duration::from_secs_f64(delay);
        while Instant::now() < deadline {
            if cancel.is_set() {
                return Err(AcquireError::Web(WebError::Cancelled("cancelled".to_string())));
            }
            sleep(Duration::from_millis(10));
        }
        let duration = {
            let st = self.st();
            if variant == "atmos" && st.no_atmos.contains(catalog_id) {
                let message = "Apple advertises no Dolby Atmos (ec-3) variant for this song";
                return Err(AcquireError::NoAtmos(message.to_string()));
            }
            let alac = matches!(variant, "lossless" | "hires");
            if let Some(failure) = st.route_failure.as_ref().filter(|_| alac) {
                return Err(LosslessError::Unavailable(failure.clone()).into());
            }
            if alac && st.no_lossless.contains(catalog_id) {
                let message = "Apple offers no lossless (ALAC) rendition for this song";
                return Err(LosslessError::NoLossless(message.to_string()).into());
            }
            if variant == "hires" && st.no_hires.contains(catalog_id) {
                let message = "Apple offers no Hi-Res Lossless rendition for this song";
                return Err(LosslessError::NoHiRes(message.to_string()).into());
            }
            if let Some(error) = st.fail.get(catalog_id) {
                return Err(error.clone());
            }
            st.durations.get(catalog_id).copied().unwrap_or(200.0)
        };
        progress("download", Some(1.0));
        let meta = match variant {
            "atmos" => json!({"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"}),
            "lossless" => json!({
                "audio_group": "audio-alac-stereo-48000-24", "codecs": "alac", "bandwidth": "1846745",
                "sample_rate": 48000, "bit_depth": 24,
            }),
            "hires" => json!({
                "audio_group": "audio-alac-stereo-96000-24", "codecs": "alac", "bandwidth": "3800000",
                "sample_rate": 96000, "bit_depth": 24,
            }),
            _ => json!({"audio_group": "audio-stereo-256", "codecs": "mp4a.40.2", "bandwidth": "256000"}),
        };
        Ok(AcquireResult {
            catalog_id: catalog_id.to_string(),
            path: self.base.join(format!("fake/{catalog_id}.{variant}.m4a")).to_string_lossy().into_owned(),
            bytes: 1,
            variant: meta.as_object().cloned().unwrap_or_default(),
            duration_s: Some(duration),
            from_cache: false,
        })
    }

    fn lossless_available(&self) -> bool {
        self.st().lossless_ok
    }

    fn lossless_problem(&self) -> Option<String> {
        self.st().lossless_problem.clone()
    }

    fn shutdown_lossless(&self) {
        self.st().lossless_shutdowns += 1;
    }

    fn complete_render(&self, out_dir: &Path) -> Result<Arc<dyn Source>, RenderError> {
        let name = format!("{}:complete", file_name(out_dir));
        Ok(self.source(&name, 16, 200.0, Some(json!({"joc_object_count_max": 15}))))
    }

    fn render_is_current(&self, _out_dir: &Path) -> bool {
        true
    }

    fn render_session(&self, m4a: &Path, out_dir: &Path, expected_duration_s: Option<f64>) -> Arc<dyn Source> {
        let folder = file_name(out_dir);
        let name = format!("{folder}:render");
        let duration = expected_duration_s.filter(|seconds| *seconds != 0.0).unwrap_or(200.0);
        let catalog_id = folder.split('.').next().unwrap_or_default().to_string();
        let mut st = self.st();
        st.rendered.push(file_name(m4a));
        let Some((rate, delay)) = st.growing.get(&catalog_id).copied() else {
            drop(st);
            return self.source(&name, 16, duration, Some(json!({"joc_object_count_max": 15})));
        };
        let total = (duration * RATE as f64) as u64;
        let source = Arc::new(FakeSource::growing(&self.base, &name, total, rate * RATE as f64, delay));
        let busy = st.sources.iter().filter(|other| other.busy()).map(|other| other.name.clone()).collect();
        *lock(&source.running_at_start) = busy;
        st.sources.push(source.clone());
        source
    }

    fn pcm_file(&self, path: &Path, channels: usize) -> Result<Arc<dyn Source>, RenderError> {
        let folder = path.parent().map(file_name).unwrap_or_default();
        Ok(self.source(&format!("{folder}:{}", file_name(path)), channels, 200.0, None))
    }

    fn decode_stereo(&self, m4a: &Path, _out_pcm: &Path) -> Result<u64, AcquireError> {
        self.st().decoded.push(file_name(m4a));
        Ok(200 * RATE)
    }

    fn binaural_available(&self) -> bool {
        true
    }

    fn render_binaural(&self, m4a: &Path, out_dir: &Path) -> Result<Arc<dyn Source>, RenderError> {
        self.st().binaural.push(file_name(m4a));
        Ok(self.source(&format!("{}:binaural", file_name(out_dir)), 2, 200.0, None))
    }

    fn downmix_profiles(&self) -> Vec<Profile> {
        self.st().profiles.clone()
    }

    fn downmix_apply(&self, key: &str) -> Result<(), DownmixError> {
        let mut st = self.st();
        if let Some(error) = &st.apply_error {
            return Err(error.clone());
        }
        st.applied.push(key.to_string());
        Ok(())
    }

    fn downmix_set_preamp(&self, db: f64) -> Result<Applied, DownmixError> {
        let mut st = self.st();
        st.preamp = Some(db);
        st.preamps_set.push(db);
        Ok(Applied {
            preset: st.current_preset.clone(),
            preamp: db,
            match_on: true,
            applied_at: 0.0,
            output: String::new(),
        })
    }

    fn downmix_preamp(&self) -> Option<f64> {
        self.st().preamp
    }

    fn downmix_current(&self) -> Option<String> {
        Some(self.st().current_preset.clone())
    }

    fn make_output(&self, volume: f64) -> Result<Arc<dyn Output>, OutputError> {
        Ok(Arc::new(FakeOutput::new(volume)))
    }

    fn measure_loudness(
        &self,
        pcm_path: &Path,
        _channels: usize,
        frames: Option<u64>,
        _cancel: &Event,
    ) -> Result<Measurement, LoudnessError> {
        let name = file_name(pcm_path);
        let mut st = self.st();
        st.measured.push(name.clone());
        let lufs = lookup(&st.loudness, &name).unwrap_or(Some(-16.0));
        Ok(Measurement { lufs, peak: 0.9, frames: frames.unwrap_or(0), fold: None, measured_at: 0.0 })
    }

    fn loudness_stream(
        &self,
        pcm_path: &Path,
        _channels: usize,
    ) -> Result<Option<Box<dyn LoudnessStream>>, LoudnessError> {
        let mut st = self.st();
        if !st.stream_meter {
            return Ok(None);
        }
        let notes = Arc::new(Mutex::new(StreamNotes::default()));
        st.streams.push(notes.clone());
        let pipeline = self.me.upgrade().expect("the pipeline is alive while it is used");
        Ok(Some(Box::new(FakeStream { pipeline, name: file_name(pcm_path), notes })))
    }
}

// --- the PCM cache ---------------------------------------------------------------------

#[derive(Default)]
pub struct PcmNotes {
    pub completed: HashMap<String, PathBuf>,
    pub claimed: Vec<String>,
    pub evictions: Vec<HashSet<String>>,
    pub marked: HashSet<String>,
}

pub struct FakePcmCache {
    base: PathBuf,
    notes: Mutex<PcmNotes>,
}

impl FakePcmCache {
    pub fn new(base: &Path) -> FakePcmCache {
        FakePcmCache { base: base.to_path_buf(), notes: Mutex::default() }
    }

    pub fn st(&self) -> MutexGuard<'_, PcmNotes> {
        lock(&self.notes)
    }

    pub fn claimed(&self) -> Vec<String> {
        self.st().claimed.clone()
    }

    pub fn marked(&self) -> HashSet<String> {
        self.st().marked.clone()
    }

    pub fn is_marked(&self, key: &str) -> bool {
        self.st().marked.contains(key)
    }
}

impl PcmStore for FakePcmCache {
    fn complete(&self, key: &str) -> Result<Option<PathBuf>, CacheError> {
        Ok(self.st().completed.get(key).cloned())
    }

    fn claim(&self, key: &str) -> Result<PathBuf, CacheError> {
        self.st().claimed.push(key.to_string());
        Ok(self.base.join("fake/pcm").join(key))
    }

    fn evict(&self, protect: &HashSet<String>) -> Result<(), CacheError> {
        self.st().evictions.push(protect.clone());
        Ok(())
    }

    fn mark_inuse(&self, key: &str) -> Result<(), CacheError> {
        self.st().marked.insert(key.to_string());
        Ok(())
    }

    fn unmark_inuse(&self, key: &str) -> Result<(), CacheError> {
        self.st().marked.remove(key);
        Ok(())
    }
}

// --- the radio -------------------------------------------------------------------------

/// A seed as the stand-in radio notes it: (id, its kind, the container's (kind, id)).
pub type Noted = (String, String, Option<(String, String)>);

/// What the stand-in radio was asked.
#[derive(Debug, Clone, PartialEq)]
pub enum RadioCall {
    /// The seeds of a station.
    Continuous(Vec<Noted>),
    /// (station id, how many songs).
    NextTracks(String, Option<i64>),
    /// The catalog id of the song whose current version was asked for.
    Equivalent(String),
}

pub struct RadioState {
    pub calls: Vec<RadioCall>,
    /// What `continuous_station()` fails with while it is set.
    pub continuous_error: Option<ApiError>,
    /// What `station_next_tracks()` fails with while it is set.
    pub next_error: Option<ApiError>,
    /// What `equivalent_song()` fails with while it is set.
    pub equivalent_error: Option<ApiError>,
    /// The songs a station hands out when no number is asked for.
    pub batch: usize,
    /// The songs a station has left (by its id); a station not listed never runs out.
    pub left: HashMap<String, usize>,
    /// Catalog id -> the current version of that song (`None`: the catalog has none).
    pub equivalents: HashMap<String, Option<Track>>,
    /// Calls wait while this is set (they are noted at once) ...
    pub held: bool,
    /// ... except this many of the next ones.
    pub free: usize,
    stations: usize,
    songs: usize,
}

/// A radio that makes up its stations and songs: station `n` is "ra.100000000n" ("Station n"),
/// the `n`th song handed out is "200000000n" ("Radio Song n").
pub struct FakeRadio {
    state: Mutex<RadioState>,
    released: Condvar,
}

/// The catalog id of the `n`th song the stand-in radio handed out (from 1).
pub fn radio_song(n: usize) -> String {
    format!("{}", 2_000_000_000 + n)
}

/// The id of the `n`th station the stand-in radio made (from 1).
pub fn radio_station(n: usize) -> String {
    format!("ra.{}", 1_000_000_000 + n)
}

/// A station of songs, as the Radio page lists it.
pub fn station(id: &str, name: &str) -> Station {
    Station {
        id: id.to_string(),
        name: name.to_string(),
        subtitle: "Station".to_string(),
        kind: "songSeeded".to_string(),
        format: "tracks".to_string(),
        ..Station::default()
    }
}

impl FakeRadio {
    pub fn new() -> Arc<FakeRadio> {
        Arc::new(FakeRadio {
            state: Mutex::new(RadioState {
                calls: Vec::new(),
                continuous_error: None,
                next_error: None,
                equivalent_error: None,
                batch: 4,
                left: HashMap::new(),
                equivalents: HashMap::new(),
                held: false,
                free: 0,
                stations: 0,
                songs: 0,
            }),
            released: Condvar::new(),
        })
    }

    pub fn st(&self) -> MutexGuard<'_, RadioState> {
        lock(&self.state)
    }

    pub fn calls(&self) -> Vec<RadioCall> {
        self.st().calls.clone()
    }

    /// The calls for the next songs of stations, oldest first.
    pub fn next_calls(&self) -> Vec<(String, Option<i64>)> {
        let next = |call: &RadioCall| match call {
            RadioCall::NextTracks(id, limit) => Some((id.clone(), *limit)),
            _ => None,
        };
        self.st().calls.iter().filter_map(next).collect()
    }

    /// The seeds of the stations asked for, oldest first.
    pub fn continuous_calls(&self) -> Vec<Vec<Noted>> {
        let seeds = |call: &RadioCall| match call {
            RadioCall::Continuous(seeds) => Some(seeds.clone()),
            _ => None,
        };
        self.st().calls.iter().filter_map(seeds).collect()
    }

    /// Calls wait from now on, until `release()`.
    pub fn hold(&self) {
        self.hold_after(0);
    }

    /// The next `free` calls are answered, the ones after them wait until `release()`.
    pub fn hold_after(&self, free: usize) {
        let mut st = self.st();
        st.held = true;
        st.free = free;
    }

    pub fn release(&self) {
        self.st().held = false;
        self.released.notify_all();
    }

    /// Note a call, wait while calls are held (at most [`PATIENCE`]).
    fn called(&self, call: RadioCall) -> MutexGuard<'_, RadioState> {
        let mut st = self.st();
        st.calls.push(call);
        if st.held && st.free > 0 {
            st.free -= 1;
            return st;
        }
        let deadline = Instant::now() + PATIENCE;
        while st.held && Instant::now() < deadline {
            st = self.released.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
        st
    }
}

impl Radio for FakeRadio {
    fn continuous_station(&self, seeds: &[Seed]) -> Result<Station, ApiError> {
        let noted = seeds.iter().map(|seed| (seed.id.clone(), seed.kind.to_string(), seed.container.clone()));
        let mut st = self.called(RadioCall::Continuous(noted.collect()));
        if let Some(error) = &st.continuous_error {
            return Err(error.clone());
        }
        st.stations += 1;
        Ok(station(&radio_station(st.stations), &format!("Station {}", st.stations)))
    }

    fn station_next_tracks(&self, station_id: &str, limit: Option<i64>) -> Result<Vec<Track>, ApiError> {
        let mut st = self.called(RadioCall::NextTracks(station_id.to_string(), limit));
        if let Some(error) = &st.next_error {
            return Err(error.clone());
        }
        let mut count = limit.map_or(st.batch, |limit| limit.max(0) as usize);
        if let Some(left) = st.left.get_mut(station_id) {
            count = count.min(*left);
            *left -= count;
        }
        let first = st.songs + 1;
        st.songs += count;
        let song = |n: usize| Track::new(Some(&radio_song(n)), format!("Radio Song {n}"), "Artist B");
        Ok((first..first + count).map(song).collect())
    }

    fn equivalent_song(&self, track: &Track) -> Result<Option<Track>, ApiError> {
        let own = track.catalog_id.clone().unwrap_or_default();
        let st = self.called(RadioCall::Equivalent(own.clone()));
        if let Some(error) = &st.equivalent_error {
            return Err(error.clone());
        }
        Ok(st.equivalents.get(&own).cloned().flatten())
    }
}

// --- the harness -----------------------------------------------------------------------

/// A scratch am-ui folder: the configuration every test player runs on. The lossless service
/// it names is on a port nothing listens on.
pub fn scratch_config(dir: &Path) -> Arc<Config> {
    let root = dir.join("am-ui");
    fs::create_dir_all(root.join("workspace")).unwrap();
    let env = HashMap::from([("LOSSLESS_PORT".to_string(), "9".to_string())]);
    let config = Arc::new(Config::new(root, &dir.join("home"), &env));
    config.ensure_dirs().unwrap();
    config
}

/// A network nothing may use: every request fails and is counted.
#[derive(Default)]
pub struct NoNetwork {
    pub asked: AtomicUsize,
}

impl Http for NoNetwork {
    fn request(&self, _call: &Request) -> Result<Vec<u8>, WebError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Err(WebError::other("network used"))
    }

    fn download(
        &self,
        _url: &str,
        _destination: &Path,
        _max_bytes: u64,
        _progress: Option<Progress>,
        _cancel: Option<&Event>,
        _timeout: f64,
    ) -> Result<(u64, String), WebError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Err(WebError::other("network used"))
    }

    fn token_sources(&self) -> Vec<TokenSource> {
        Vec::new()
    }
}

/// The engine's own pipeline in a scratch folder, cut off from the world: its network fails
/// every request and the programs of the downmix sink are not there. Both count what they were
/// asked.
pub struct Engine {
    pub pipeline: Arc<EnginePipeline>,
    pub log: Arc<PlaybackLog>,
    pub network: Arc<NoNetwork>,
    /// How often a program of the sink was asked for.
    pub programs: Arc<AtomicUsize>,
    pub config: Arc<Config>,
    dir: tempfile::TempDir,
}

impl Engine {
    pub fn new() -> Engine {
        let dir = tempfile::tempdir().unwrap();
        let config = scratch_config(dir.path());
        let network = Arc::new(NoNetwork::default());
        let programs = Arc::new(AtomicUsize::new(0));
        let asked = programs.clone();
        let no_programs = move |_: &[&str], _: Option<Duration>| -> Result<Printed, RunError> {
            asked.fetch_add(1, Ordering::SeqCst);
            Err(RunError::NotFound)
        };
        let log = Arc::new(PlaybackLog::for_config(&config));
        let lossless = Arc::new(Lossless::new(config.clone(), network.clone()));
        let acquirer = Arc::new(Acquirer::new(config.clone(), network.clone(), lossless.clone()));
        let downmix = Arc::new(Downmix::with(config.clone(), Paths::new(&config), Box::new(no_programs)));
        let pipeline = Arc::new(EnginePipeline::new(config.clone(), acquirer, lossless, downmix, log.clone()));
        Engine { pipeline, log, network, programs, config, dir }
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}

/// The engine's timing, except that a start takes all the time its loudness needs: what a
/// test finds on the output at a start does not depend on how busy the machine is.
pub fn patient() -> Tuning {
    Tuning { loudness_start_wait_s: 30.0, loudness_final_wait_s: 30.0, ..Tuning::default() }
}

/// What a harness is built with; `Default` is the harness of the Python tests.
#[derive(Default)]
pub struct Setup {
    /// The settings file of an earlier harness.
    pub state_path: Option<PathBuf>,
    /// `None`: [`patient`].
    pub tuning: Option<Tuning>,
    /// No output is handed to the player: its pipeline makes one on the first play.
    pub without_output: bool,
    /// With `without_output`: what makes the output instead of the pipeline.
    pub output_factory: Option<OutputFactory>,
    /// The harness's stand-in radio is handed to the player (without it, Autoplay, stations
    /// and the current versions of withdrawn songs are off, as in the Python tests).
    pub radio: bool,
    /// Files (name, content) put into the state folder before the player is made.
    pub state_files: Vec<(String, String)>,
    /// Where the player keeps its queue between runs (`None`: it keeps nothing).
    pub resume_path: Option<PathBuf>,
}

/// A player on stand-ins, in a scratch folder (which goes last, after the player has stopped).
pub struct Harness {
    pub player: Player,
    pub output: Arc<FakeOutput>,
    pub pipeline: Arc<FakePipeline>,
    pub pcm: Arc<FakePcmCache>,
    /// Handed to the player only with [`Setup::radio`].
    pub radio: Arc<FakeRadio>,
    pub log: Arc<PlaybackLog>,
    pub events: Arc<Mutex<Vec<PlayerEvent>>>,
    pub state_path: PathBuf,
    pub config: Arc<Config>,
    dir: tempfile::TempDir,
}

/// An error event: (message, catalog id, fatal).
pub type Reported = (String, Option<String>, bool);

impl Harness {
    pub fn new() -> Harness {
        Harness::with(Setup::default())
    }

    /// A harness whose player has the stand-in radio.
    pub fn with_radio() -> Harness {
        Harness::with(Setup { radio: true, ..Setup::default() })
    }

    pub fn with(setup: Setup) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let config = scratch_config(dir.path());
        for (name, content) in &setup.state_files {
            fs::write(config.state_dir.join(name), content).unwrap();
        }
        let log = Arc::new(PlaybackLog::new(dir.path().join("playback.log")));
        let state_path = setup.state_path.unwrap_or_else(|| dir.path().join("player.json"));
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::new(FakeOutput::new(1.0));
        let pipeline = FakePipeline::new(dir.path());
        let pcm = Arc::new(FakePcmCache::new(dir.path()));
        let radio = FakeRadio::new();
        let heard = events.clone();
        let options = PlayerOptions {
            media_cache: Some(Arc::new(MediaCache::new(config.clone()))),
            pcm_cache: Some(pcm.clone()),
            output: (!setup.without_output).then(|| output.clone() as Arc<dyn Output>),
            output_factory: setup.output_factory,
            state_path: Some(state_path.clone()),
            tuning: setup.tuning.unwrap_or_else(patient),
            radio: setup.radio.then(|| radio.clone() as Arc<dyn Radio>),
            resume_path: setup.resume_path,
        };
        let listener = move |event: PlayerEvent| lock(&heard).push(event);
        let player = Player::new(config.clone(), pipeline.clone(), log.clone(), listener, options).unwrap();
        Harness { player, output, pipeline, pcm, radio, log, events, state_path, config, dir }
    }

    /// The scratch folder of this harness.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.player.snapshot()
    }

    /// The value `found` gives once it gives one; panics when it does not within [`PATIENCE`].
    pub fn wait<T>(&self, what: &str, found: impl Fn() -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(value) = found() {
                return value;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}; last snapshot: {:?}; events: {:?}; output: {:?}",
                    self.snapshot(),
                    self.names(),
                    self.output.calls()
                );
            }
            sleep(Duration::from_millis(5));
        }
    }

    pub fn wait_until(&self, what: &str, reached: impl Fn() -> bool) {
        self.wait(what, || reached().then_some(()));
    }

    /// The first snapshot `wanted` accepts.
    pub fn wait_snapshot(&self, what: &str, wanted: impl Fn(&PlaybackSnapshot) -> bool) -> PlaybackSnapshot {
        self.wait(what, || Some(self.snapshot()).filter(|snapshot| wanted(snapshot)))
    }

    pub fn wait_state(&self, state: PlayerState) -> PlaybackSnapshot {
        self.wait_snapshot(&format!("state {state}"), |snapshot| snapshot.state == state)
    }

    /// Playing the entry at `index`.
    pub fn wait_playing(&self, index: i64) -> PlaybackSnapshot {
        self.wait_snapshot(&format!("playing entry {index}"), |snapshot| {
            snapshot.index == index && snapshot.state == PlayerState::Playing
        })
    }

    /// Every command sent so far has been handled when this returns (commands are handled in
    /// the order they were sent: a change of the volume and its way back mark the place).
    pub fn settle(&self) {
        let volume = self.snapshot().volume;
        let marker = if volume == 0.123456 { 0.654321 } else { 0.123456 };
        self.player.set_volume(marker);
        self.wait_snapshot("the commands before this one", |snapshot| snapshot.volume == marker);
        self.player.set_volume(volume);
        self.wait_snapshot("the volume as it was", |snapshot| snapshot.volume == volume);
    }

    pub fn events(&self) -> Vec<PlayerEvent> {
        lock(&self.events).clone()
    }

    pub fn names(&self) -> Vec<&'static str> {
        lock(&self.events).iter().map(PlayerEvent::name).collect()
    }

    pub fn errors(&self) -> Vec<Reported> {
        let error = |event: &PlayerEvent| match event {
            PlayerEvent::Error { message, catalog_id, fatal } => Some((message.clone(), catalog_id.clone(), *fatal)),
            _ => None,
        };
        lock(&self.events).iter().filter_map(error).collect()
    }

    /// The states of the state events, oldest first.
    pub fn states(&self) -> Vec<PlayerState> {
        let state = |event: &PlayerEvent| match event {
            PlayerEvent::State(snapshot) => Some(snapshot.state),
            _ => None,
        };
        lock(&self.events).iter().filter_map(state).collect()
    }

    pub fn queue_keys(&self) -> Vec<String> {
        self.player.queue().into_iter().map(|entry| entry.track.key()).collect()
    }

    /// Which entries of the queue Autoplay added.
    pub fn autoplay_flags(&self) -> Vec<bool> {
        self.player.queue().into_iter().map(|entry| entry.autoplay).collect()
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(self.log.path()).unwrap_or_default()
    }

    /// The lines of the playback log that contain `text`. (The worker writes what it asks the
    /// radio before it handles the next command: after `settle()` every request is there.)
    pub fn log_lines(&self, text: &str) -> usize {
        self.log_text().lines().filter(|line| line.contains(text)).count()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // a test that ends while the radio holds its calls lets them go
        self.radio.release();
    }
}
