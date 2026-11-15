//! What the player's threads share: the state every command works on, the prepared sources,
//! the commands themselves, the settings file and the snapshot.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Map, Value, json};

use super::gate::RenderGate;
use super::jobs::{ApplyJob, Job, LoudnessJob, PreampJob, Preparing};
use super::radio::{Equivalents, Radio, RadioJob};
use super::seams::{Output, PcmStore, Pipeline, Source, same_pcm};
use super::{Container, Kind, OutputFactory, PlayerEvent, QueueEntry, Tuning};
use crate::cache::MediaCache;
use crate::config::{Config, SAMPLE_RATE};
use crate::loudness::{self, TARGET_LUFS, lock};
use crate::output::PlaybackLog;
use crate::render::PcmSource;
use crate::types::{AcquireResult, AudioFormat, AudioMode, PlaybackSnapshot, PlayerState, RepeatMode, Station, Track};
use crate::util;

/// Frames per second, as a float.
pub(super) const RATE: f64 = SAMPLE_RATE as f64;

pub(super) type Listener = Box<dyn Fn(PlayerEvent) + Send + Sync>;

/// 0..1, as Python's `min(1.0, max(0.0, volume))` gives it (0 for not-a-number).
pub(super) fn unit(volume: f64) -> f64 {
    if volume.is_nan() { 0.0 } else { volume.clamp(0.0, 1.0) }
}

/// What is known about the loudness of a prepared source: the integrated loudness of what is
/// audible (the PEQdB fold for renders), as the trim Sound Check applies for it.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Loudness {
    /// A value (an estimate, or the measurement of the whole file) is in.
    pub known: bool,
    /// Sound Check attenuation (<= 0).
    pub trim_db: f64,
}

impl Loudness {
    /// A measured value in LUFS (`None`: nothing above the gate, silence).
    pub fn of(lufs: Option<f64>) -> Loudness {
        Loudness { known: true, trim_db: loudness::trim_db(lufs, TARGET_LUFS) }
    }
}

/// One (song, kind) ready for the output.
pub(super) struct Prepared {
    pub kind: Kind,
    pub source: Arc<dyn Source>,
    /// What is audible.
    pub format: AudioFormat,
    pub format_detail: Mutex<String>,
    pub duration_s: Option<f64>,
    pub cache_key: String,
    pub acquire: AcquireResult,
    pub loudness: Mutex<Loudness>,
}

impl Prepared {
    /// The source as the output takes it.
    pub fn pcm(&self) -> Arc<dyn PcmSource> {
        self.source.clone()
    }

    pub fn is(&self, source: &Arc<dyn PcmSource>) -> bool {
        same_pcm(source, &self.source)
    }

    pub fn loudness(&self) -> Loudness {
        *lock(&self.loudness)
    }

    pub fn format_detail(&self) -> String {
        lock(&self.format_detail).clone()
    }

    /// The summary of a finished render.
    pub fn summary(&self) -> Option<Map<String, Value>> {
        self.source.summary()
    }
}

/// The entry on the output and the source it plays from.
#[derive(Clone)]
pub(super) struct Current {
    pub entry: Arc<QueueEntry>,
    pub prepared: Arc<Prepared>,
    /// The output's count of gapless continuations when this entry started.
    pub handoffs_at_start: u64,
}

/// Everything a command reads or changes that another thread may look at. Held for short
/// stretches only, never across a call that blocks.
pub(super) struct State {
    // settings (persisted)
    pub mode: AudioMode,
    pub profile: Option<String>,
    pub volume: f64,
    pub stereo_fallback: bool,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub sound_check: bool,
    pub bass_manage: bool,
    pub lossless: bool,
    pub autoplay: bool,
    // queue
    pub queue: Vec<Arc<QueueEntry>>,
    /// The unshuffled order while shuffle is on.
    pub original: Option<Vec<Arc<QueueEntry>>>,
    /// Position in the queue, -1 when it is empty.
    pub index: i64,
    /// The station whose songs the queue holds (`play_station()`), while it does.
    pub station: Option<Station>,
    // playback
    pub state: PlayerState,
    pub error: Option<String>,
    pub loading_phase: Option<String>,
    pub loading_progress: Option<f64>,
    pub current: Option<Current>,
    pub switching: bool,
    pub paused_requested: bool,
    pub pending_start_frame: u64,
    /// The sink's preamp in dB, read at start-up and after each change.
    pub preamp: Option<f64>,
    /// Ready sources per (key, kind).
    pub prepared: HashMap<(String, Kind), Arc<Prepared>>,
    pub load_job: Option<Arc<Job>>,
    pub prefetch_job: Option<Arc<Job>>,
    pub switch_job: Option<Arc<Job>>,
    /// Running measurements per PCM cache key.
    pub loudness_jobs: HashMap<String, Arc<LoudnessJob>>,
    /// PCM cache keys this process marked.
    pub inuse: HashSet<String>,
    pub output: Option<Arc<dyn Output>>,
}

impl State {
    pub fn new(config: &Config, output: Option<Arc<dyn Output>>) -> State {
        State {
            mode: AudioMode::Atmos,
            profile: None,
            volume: 1.0,
            stereo_fallback: true,
            repeat: RepeatMode::Off,
            shuffle: false,
            sound_check: true,
            bass_manage: config.bass_manage(),
            lossless: true,
            autoplay: true,
            queue: Vec::new(),
            original: None,
            index: -1,
            station: None,
            state: PlayerState::Idle,
            error: None,
            loading_phase: None,
            loading_progress: None,
            current: None,
            switching: false,
            paused_requested: false,
            pending_start_frame: 0,
            preamp: None,
            prepared: HashMap::new(),
            load_job: None,
            prefetch_job: None,
            switch_job: None,
            loudness_jobs: HashMap::new(),
            inuse: HashSet::new(),
            output,
        }
    }

    /// The entry at the queue position, when there is one.
    pub fn entry_at_index(&self) -> Option<&Arc<QueueEntry>> {
        usize::try_from(self.index).ok().and_then(|index| self.queue.get(index))
    }

    pub fn active(&self) -> bool {
        matches!(self.state, PlayerState::Playing | PlayerState::Paused)
    }

    /// Nothing is on the output for the entry at the queue position (yet): the snapshot shows
    /// that entry, loading.
    pub fn leave_current(&mut self) {
        self.current = None;
        self.state = PlayerState::Loading;
        self.loading_phase = None;
        self.loading_progress = None;
    }

    /// The saved settings: mode, downmix_profile, volume, stereo_fallback, repeat, shuffle,
    /// sound_check, bass_feed, lossless, autoplay. Anything missing or of another type keeps
    /// its default.
    pub fn restore_settings(&mut self, saved: &Map<String, Value>, config: &Config) {
        if let Some(mode) = saved.get("mode").and_then(Value::as_str).and_then(AudioMode::parse) {
            self.mode = mode;
        }
        if let Some(Value::String(profile)) = saved.get("downmix_profile") {
            self.profile = Some(profile.clone());
        }
        let volume = match saved.get("volume") {
            None => Some(1.0),
            Some(Value::Number(number)) => number.as_f64(),
            Some(Value::Bool(flag)) => Some(f64::from(u8::from(*flag))),
            Some(Value::String(text)) => text.trim().parse().ok(),
            Some(_) => None,
        };
        if let Some(volume) = volume {
            self.volume = unit(volume);
        }
        let flag = |name: &str| saved.get(name).and_then(Value::as_bool);
        if let Some(enabled) = flag("stereo_fallback") {
            self.stereo_fallback = enabled;
        }
        if let Some(repeat) = saved.get("repeat").and_then(Value::as_str).and_then(RepeatMode::parse) {
            self.repeat = repeat;
        }
        if let Some(enabled) = flag("shuffle") {
            self.shuffle = enabled;
        }
        if let Some(enabled) = flag("sound_check") {
            self.sound_check = enabled;
        }
        // the bass feed was saved as `bass_manage` while it was on by default, before the LFE was
        // aligned: that value is not taken over
        if let Some(enabled) = flag("bass_feed") {
            self.bass_manage = enabled;
            config.set_bass_manage(enabled);
        }
        if let Some(enabled) = flag("lossless") {
            self.lossless = enabled;
        }
        if let Some(enabled) = flag("autoplay") {
            self.autoplay = enabled;
        }
    }

    /// What `player.json` holds. The Python engine reads the same file; it has no autoplay (written
    /// only while it is off) and keeps its own bass feed under `bass_manage`.
    pub fn settings(&self) -> Value {
        let mut settings = json!({
            "mode": self.mode.as_str(),
            "downmix_profile": self.profile,
            "volume": self.volume,
            "stereo_fallback": self.stereo_fallback,
            "repeat": self.repeat.as_str(),
            "shuffle": self.shuffle,
            "sound_check": self.sound_check,
            "bass_feed": self.bass_manage,
            "lossless": self.lossless,
        });
        if !self.autoplay {
            settings["autoplay"] = json!(false);
        }
        settings
    }

    /// The repeat mode that applies: the queue of a station never repeats.
    pub fn repeat_mode(&self) -> RepeatMode {
        if self.station.is_some() { RepeatMode::Off } else { self.repeat }
    }
}

/// What the worker is asked to do: the public commands, and what jobs and the output report.
pub(super) enum Command {
    Play(Vec<Track>, usize, Option<Container>),
    Enqueue(Vec<Track>),
    PlayNext(Vec<Track>),
    Jump(usize),
    JumpTo(u64),
    Remove(usize),
    RemoveEntry(u64),
    MoveEntry(u64, usize),
    Clear,
    Toggle,
    Pause,
    Resume,
    Stop,
    Next,
    Previous,
    Seek(f64),
    SetVolume(f64),
    SetRepeat(RepeatMode),
    SetShuffle(bool),
    SetMode(AudioMode),
    SetStereoFallback(bool),
    SetSoundCheck(bool),
    SetBassManage(bool),
    SetLossless(bool),
    SetDownmixProfile(String),
    SetPreamp(f64),
    SetAutoplay(bool),
    PlayStation(Station),
    JobProgress(Arc<Job>, String, Option<f64>),
    JobDone(Arc<Job>),
    ApplyDone(Arc<ApplyJob>),
    PreampDone(Arc<PreampJob>),
    LoudnessEstimate(Arc<LoudnessJob>, u64),
    LoudnessDone(Arc<LoudnessJob>),
    RadioDone(Arc<RadioJob>),
    SourceStarted,
    SourceEnded(Arc<dyn PcmSource>),
    OutputError(String),
    Shutdown,
}

impl Command {
    /// The name the Python engine's command has: what a failure of the command is reported
    /// under.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Play(..) => "play",
            Command::Enqueue(_) => "enqueue",
            Command::PlayNext(_) => "play_next",
            Command::Jump(_) => "jump",
            Command::JumpTo(_) => "jump_to",
            Command::Remove(_) => "remove",
            Command::RemoveEntry(_) => "remove_entry",
            Command::MoveEntry(..) => "move_entry",
            Command::Clear => "clear",
            Command::Toggle => "toggle",
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::Stop => "stop",
            Command::Next => "next",
            Command::Previous => "previous",
            Command::Seek(_) => "seek",
            Command::SetVolume(_) => "set_volume",
            Command::SetRepeat(_) => "set_repeat",
            Command::SetShuffle(_) => "set_shuffle",
            Command::SetMode(_) => "set_mode",
            Command::SetStereoFallback(_) => "set_stereo_fallback",
            Command::SetSoundCheck(_) => "set_sound_check",
            Command::SetBassManage(_) => "set_bass_manage",
            Command::SetLossless(_) => "set_lossless",
            Command::SetDownmixProfile(_) => "set_downmix_profile",
            Command::SetPreamp(_) => "set_preamp",
            Command::SetAutoplay(_) => "set_autoplay",
            Command::PlayStation(_) => "play_station",
            Command::JobProgress(..) => "job_progress",
            Command::JobDone(_) => "job_done",
            Command::ApplyDone(_) => "apply_done",
            Command::PreampDone(_) => "preamp_done",
            Command::LoudnessEstimate(..) => "loudness_estimate",
            Command::LoudnessDone(_) => "loudness_done",
            Command::RadioDone(_) => "radio_done",
            Command::SourceStarted => "source_started",
            Command::SourceEnded(_) => "source_ended",
            Command::OutputError(_) => "output_error",
            Command::Shutdown => "shutdown",
        }
    }
}

/// The way into the worker's command queue; nothing is queued once the player shuts down.
#[derive(Clone)]
pub(super) struct Poster {
    sender: mpsc::Sender<Command>,
    shutdown: Arc<AtomicBool>,
}

impl Poster {
    pub fn new(sender: mpsc::Sender<Command>) -> Poster {
        Poster { sender, shutdown: Arc::new(AtomicBool::new(false)) }
    }

    pub fn post(&self, command: Command) {
        if !self.is_shut_down() {
            let _ = self.sender.send(command);
        }
    }

    pub fn is_shut_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Close the queue with the command that ends the worker; false when that was done before.
    pub fn shut_down(&self) -> bool {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return false;
        }
        let _ = self.sender.send(Command::Shutdown);
        true
    }
}

/// What the worker, the job threads and the player's handle share.
pub(super) struct Shared {
    pub config: Arc<Config>,
    pub pipeline: Arc<dyn Pipeline>,
    pub media_cache: Arc<MediaCache>,
    pub pcm_cache: Arc<dyn PcmStore>,
    pub log: Arc<PlaybackLog>,
    pub listener: Listener,
    pub output_factory: Option<OutputFactory>,
    pub state_path: PathBuf,
    pub tuning: Tuning,
    pub poster: Poster,
    /// The network calls of Autoplay, stations and withdrawn songs; `None`: those are off.
    pub radio: Option<Arc<dyn Radio>>,
    /// The current versions of withdrawn songs.
    pub equivalents: Equivalents,
    pub state: Mutex<State>,
    pub gate: RenderGate,
    pub preparing: Preparing,
    next_entry_id: AtomicU64,
    cleaned: AtomicBool,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        pipeline: Arc<dyn Pipeline>,
        media_cache: Arc<MediaCache>,
        pcm_cache: Arc<dyn PcmStore>,
        log: Arc<PlaybackLog>,
        listener: Listener,
        output_factory: Option<OutputFactory>,
        state_path: PathBuf,
        tuning: Tuning,
        poster: Poster,
        radio: Option<Arc<dyn Radio>>,
        equivalents: Equivalents,
        state: State,
    ) -> Shared {
        Shared {
            gate: RenderGate::new(log.clone()),
            config,
            pipeline,
            media_cache,
            pcm_cache,
            log,
            listener,
            output_factory,
            state_path,
            tuning,
            poster,
            radio,
            equivalents,
            state: Mutex::new(state),
            preparing: Preparing::default(),
            next_entry_id: AtomicU64::new(1),
            cleaned: AtomicBool::new(false),
        }
    }

    pub fn post(&self, command: Command) {
        self.poster.post(command);
    }

    /// Queue entries for `tracks`, each with an id no other entry of this player has.
    pub fn new_entries(
        &self,
        tracks: Vec<Track>,
        container: Option<&Container>,
        autoplay: bool,
    ) -> Vec<Arc<QueueEntry>> {
        let entry = |track| {
            let id = self.next_entry_id.fetch_add(1, Ordering::Relaxed);
            Arc::new(QueueEntry { id, track, autoplay, container: container.cloned() })
        };
        tracks.into_iter().map(entry).collect()
    }

    /// Hand an event to the listener; a panic inside it is contained.
    pub fn emit(&self, event: PlayerEvent) {
        let _ = catch_unwind(AssertUnwindSafe(|| (self.listener)(event)));
    }

    /// The ready source of a (key, kind), when there is one.
    pub fn ready(&self, key: &str, kind: Kind) -> Option<Arc<Prepared>> {
        lock(&self.state).prepared.get(&(key.to_string(), kind)).cloned()
    }

    /// Whether `prepared` is one of the ready sources.
    pub fn is_prepared(&self, prepared: &Arc<Prepared>) -> bool {
        lock(&self.state).prepared.values().any(|ready| Arc::ptr_eq(ready, prepared))
    }

    pub fn output(&self) -> Option<Arc<dyn Output>> {
        lock(&self.state).output.clone()
    }

    pub fn sound_check(&self) -> bool {
        lock(&self.state).sound_check
    }

    /// Note that this process uses a PCM cache entry.
    pub fn mark(&self, cache_key: &str) {
        lock(&self.state).inuse.insert(cache_key.to_string());
        let _ = self.pcm_cache.mark_inuse(cache_key);
    }

    pub fn unmark(&self, cache_key: &str) {
        lock(&self.state).inuse.remove(cache_key);
        let _ = self.pcm_cache.unmark_inuse(cache_key);
    }

    /// Write the settings file (0600, atomic); a failure is ignored.
    pub fn save_settings(&self) {
        let settings = lock(&self.state).settings();
        let _ = util::write_private_json(&self.state_path, &settings);
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        let lossless_available = self.pipeline.lossless_available();
        let st = lock(&self.state);
        let current = st.current.as_ref();
        let track = match current {
            Some(current) => Some(current.entry.track.clone()),
            None => st.entry_at_index().map(|entry| entry.track.clone()),
        };
        let (mut position, mut buffered, mut buffering) = (0.0, 0.0, false);
        let mut duration = track
            .as_ref()
            .and_then(|track| track.duration_ms)
            .filter(|milliseconds| *milliseconds != 0)
            .map(|milliseconds| milliseconds as f64 / 1000.0);
        let underruns = st.output.as_ref().map_or(0, |output| output.underruns());
        let loading_position = st.pending_start_frame as f64 / RATE;
        match current {
            Some(current) => {
                if let Some(known) = current.prepared.duration_s.filter(|seconds| *seconds != 0.0) {
                    duration = Some(known);
                }
                match &st.output {
                    Some(output) if st.active() => {
                        position = output.position_frames() as f64 / RATE;
                        buffered = output.buffered_frames() as f64 / RATE;
                        buffering = output.buffering();
                    }
                    _ if st.state == PlayerState::Loading => position = loading_position,
                    _ => {}
                }
            }
            None if st.state == PlayerState::Loading => position = loading_position,
            None => {}
        }
        let loudness = current.map(|current| current.prepared.loudness());
        let loading = st.state == PlayerState::Loading;
        // what is on the output, else what a load takes (the equivalents' lock is a leaf)
        let playing_catalog_id = match current {
            Some(current) => Some(current.prepared.acquire.catalog_id.clone()),
            None => track.as_ref().and_then(|track| self.equivalents.playing_id(track)),
        };
        PlaybackSnapshot {
            state: st.state,
            track,
            index: st.index,
            queue_length: st.queue.len(),
            position,
            duration,
            buffered,
            volume: st.volume,
            loading_phase: st.loading_phase.clone().filter(|_| loading),
            loading_progress: st.loading_progress.filter(|_| loading),
            error: st.error.clone(),
            repeat: st.repeat,
            shuffle: st.shuffle,
            mode: st.mode,
            format: current.filter(|_| st.active()).map(|current| current.prepared.format),
            format_detail: current.map(|current| current.prepared.format_detail()).unwrap_or_default(),
            downmix_profile: st.profile.clone(),
            switching: st.switching,
            preamp: st.preamp,
            buffering,
            underruns,
            sound_check: st.sound_check,
            track_gain_db: loudness
                .filter(|loudness| st.sound_check && loudness.known)
                .map(|loudness| loudness.trim_db),
            bass_manage: st.bass_manage,
            lossless: st.lossless,
            lossless_available,
            autoplay: st.autoplay,
            station: st.station.clone(),
            playing_catalog_id,
        }
    }

    pub fn queue(&self) -> Vec<QueueEntry> {
        lock(&self.state).queue.iter().map(|entry| QueueEntry::clone(entry)).collect()
    }

    /// What `shutdown()` leaves behind in order: jobs cancelled, renders stopped, in-use markers
    /// removed, the output and the lossless service stopped. Runs once.
    pub fn cleanup(&self) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        let (jobs, loudness_jobs, prepared, output) = {
            let st = lock(&self.state);
            let jobs: Vec<Arc<Job>> =
                [&st.load_job, &st.prefetch_job, &st.switch_job].into_iter().flatten().cloned().collect();
            let loudness_jobs: Vec<Arc<LoudnessJob>> = st.loudness_jobs.values().cloned().collect();
            let prepared: Vec<Arc<dyn Source>> = st.prepared.values().map(|ready| ready.source.clone()).collect();
            (jobs, loudness_jobs, prepared, st.output.clone())
        };
        for job in &jobs {
            job.cancel.set();
        }
        for job in &loudness_jobs {
            job.cancel.set();
        }
        self.stop_sessions(prepared);
        for job in &jobs {
            if let Some(session) = job.session() {
                self.stop_sessions([session]);
            }
        }
        let marked: Vec<String> = lock(&self.state).inuse.iter().cloned().collect();
        for cache_key in marked {
            self.unmark(&cache_key);
        }
        if let Some(output) = output {
            output.stop();
        }
        self.pipeline.shutdown_lossless();
    }
}
