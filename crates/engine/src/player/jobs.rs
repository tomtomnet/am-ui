//! What runs off the worker thread: preparing a (song, kind) for the output, measuring its
//! loudness, and the two changes of the shared sink. Every job reports back through the
//! worker's command queue; the worker owns all decisions.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::gate::label;
use super::seams::{LoudnessStream, Source};
use super::shared::{Command, Loudness, Prepared, RATE, Shared};
use super::{
    Kind, LOUDNESS_REFINE_DB, LOUDNESS_SLICE_FRAMES, LOUDNESS_STALL_S, PlayerError, RENDER_POLL_S,
    RENDER_RATE_MIN_SPAN_S, RENDER_RATE_WINDOW_S, RENDER_START_RATE, RENDER_START_TIMEOUT_S, format_detail_for,
};
use crate::cache::CacheError;
use crate::downmix::{Applied, DownmixError};
use crate::loudness::{self, Measurement, lock};
use crate::types::{AcquireResult, Track};
use crate::util::Event;

/// A description of what a thread panicked with.
pub(super) fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    let text = panic.downcast_ref::<&str>().map(|text| text.to_string());
    text.or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "internal error".to_string())
}

/// Start a named thread; false when the system has none to give.
pub(super) fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> bool {
    thread::Builder::new().name(name.to_string()).spawn(body).is_ok()
}

// --- preparing a song ----------------------------------------------------------------

/// What a job prepares its source for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    Load,
    Prefetch,
    Switch,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Load => "load",
            Purpose::Prefetch => "prefetch",
            Purpose::Switch => "switch",
        }
    }

    /// A render for the track the user wants to hear now.
    pub fn priority(self) -> bool {
        self != Purpose::Prefetch
    }
}

/// Prepare one (song, kind) off the worker thread; cancellable; reports back through the queue.
pub(super) struct Job {
    purpose: Mutex<Purpose>,
    /// The song as its queue entry shows it.
    pub track: Track,
    /// The key the song plays under.
    pub key: String,
    /// The catalog id that is acquired; `None` for a song that has none.
    pub playing_id: Option<String>,
    pub kind: Kind,
    start_frame: AtomicU64,
    pub cancel: Arc<Event>,
    outcome: Mutex<Option<Result<Arc<Prepared>, PlayerError>>>,
    /// Known once the variant is acquired.
    cache_key: Mutex<Option<String>>,
    /// The render while this job renders.
    session: Mutex<Option<Arc<dyn Source>>>,
    alive: AtomicBool,
}

impl Job {
    pub fn start(
        shared: &Arc<Shared>,
        purpose: Purpose,
        track: &Track,
        key: String,
        playing_id: Option<String>,
        kind: Kind,
        start_frame: u64,
    ) -> Arc<Job> {
        let job = Arc::new(Job {
            purpose: Mutex::new(purpose),
            track: track.clone(),
            key,
            playing_id,
            kind,
            start_frame: AtomicU64::new(start_frame),
            cancel: Arc::new(Event::new()),
            outcome: Mutex::new(None),
            cache_key: Mutex::new(None),
            session: Mutex::new(None),
            alive: AtomicBool::new(true),
        });
        let (thread_job, thread_shared) = (job.clone(), shared.clone());
        let started = spawn(&format!("am-ui-{}", purpose.as_str()), move || thread_job.run(&thread_shared));
        if !started {
            job.finish(shared, Err(PlayerError::Other("could not start a thread".to_string())));
        }
        job
    }

    fn run(self: &Arc<Self>, shared: &Arc<Shared>) {
        let prepared = catch_unwind(AssertUnwindSafe(|| shared.prepare(self)));
        // reported to the worker, never raised on this thread
        let outcome = prepared.unwrap_or_else(|panic| Err(PlayerError::Other(panic_text(&*panic))));
        self.finish(shared, outcome);
    }

    fn finish(self: &Arc<Self>, shared: &Shared, outcome: Result<Arc<Prepared>, PlayerError>) {
        *lock(&self.outcome) = Some(outcome);
        shared.post(Command::JobDone(self.clone()));
        self.alive.store(false, Ordering::SeqCst);
    }

    fn progress(self: &Arc<Self>, shared: &Shared, phase: &str, fraction: Option<f64>) {
        if !self.cancel.is_set() {
            shared.post(Command::JobProgress(self.clone(), phase.to_string(), fraction));
        }
    }

    pub fn purpose(&self) -> Purpose {
        *lock(&self.purpose)
    }

    /// A running prefetch becomes the load of the track the user skipped to.
    pub fn adopt(&self, purpose: Purpose, start_frame: u64) {
        *lock(&self.purpose) = purpose;
        self.set_start_frame(start_frame);
    }

    pub fn start_frame(&self) -> u64 {
        self.start_frame.load(Ordering::SeqCst)
    }

    pub fn set_start_frame(&self, frame: u64) {
        self.start_frame.store(frame, Ordering::SeqCst);
    }

    pub fn matches(&self, key: &str, kind: Kind) -> bool {
        self.key == key && self.kind == kind
    }

    /// Whether the job's thread still works (it ends right after it has reported).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn cache_key(&self) -> Option<String> {
        lock(&self.cache_key).clone()
    }

    pub fn session(&self) -> Option<Arc<dyn Source>> {
        lock(&self.session).clone()
    }

    /// What the job ended with; `None` while it runs.
    pub fn outcome(&self) -> Option<Result<Arc<Prepared>, PlayerError>> {
        lock(&self.outcome).clone()
    }

    /// The song's catalog id for log lines and events.
    pub fn catalog_id(&self) -> String {
        self.track.catalog_id.clone().unwrap_or_default()
    }
}

/// The PCM cache entries that are being prepared right now. A second job for the same entry
/// waits for the first one instead of claiming the directory a second time.
#[derive(Default)]
pub(super) struct Preparing {
    keys: Mutex<HashSet<String>>,
    changed: Condvar,
}

/// Holds one entry of [`Preparing`] until it is dropped.
struct InPreparation<'a> {
    preparing: &'a Preparing,
    cache_key: &'a str,
}

impl Drop for InPreparation<'_> {
    fn drop(&mut self) {
        lock(&self.preparing.keys).remove(self.cache_key);
        self.preparing.changed.notify_all();
    }
}

impl Preparing {
    /// Wait until nobody else prepares `cache_key`, then take it; `Ok(Err(ready))` when the
    /// other preparation left a ready source behind.
    fn enter<'a>(
        &'a self,
        cache_key: &'a str,
        cancel: &Event,
        ready: impl Fn() -> Option<Arc<Prepared>>,
    ) -> Result<Result<InPreparation<'a>, Arc<Prepared>>, PlayerError> {
        let mut keys = lock(&self.keys);
        let mut waited = false;
        loop {
            if cancel.is_set() {
                return Err(PlayerError::Cancelled);
            }
            if !keys.contains(cache_key) {
                if waited {
                    // the other preparation is over: its source, when it made one, is the answer
                    drop(keys);
                    if let Some(prepared) = ready() {
                        return Ok(Err(prepared));
                    }
                    keys = lock(&self.keys);
                    if keys.contains(cache_key) {
                        continue;
                    }
                }
                keys.insert(cache_key.to_string());
                return Ok(Ok(InPreparation { preparing: self, cache_key }));
            }
            waited = true;
            let poll = Duration::from_secs_f64(RENDER_POLL_S);
            keys = self.changed.wait_timeout(keys, poll).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
    }
}

fn other(message: impl Into<String>) -> PlayerError {
    PlayerError::Other(message.into())
}

impl Shared {
    /// Job thread: acquire the right variant and open or produce its PCM source.
    fn prepare(self: &Arc<Self>, job: &Arc<Job>) -> Result<Arc<Prepared>, PlayerError> {
        let (kind, cancel) = (job.kind, &*job.cancel);
        if let Some(ready) = self.ready(&job.key, kind) {
            return Ok(ready);
        }
        let Some(catalog_id) = job.playing_id.as_deref() else {
            return Err(other("this song has no catalog id and cannot be played"));
        };
        let cache_key = format!("{catalog_id}.{}", kind.as_str());
        let _mine = match self.preparing.enter(&cache_key, cancel, || self.ready_entry(job, &cache_key))? {
            Ok(mine) => mine,
            Err(ready) => return Ok(ready),
        };
        // (a preparation that ended between the look above and this job's turn left its source)
        if let Some(ready) = self.ready_entry(job, &cache_key) {
            return Ok(ready);
        }
        let mut progress = |phase: &str, fraction: Option<f64>| job.progress(self, phase, fraction);
        let result = self.pipeline.acquire(catalog_id, &self.media_cache, kind.variant(), &mut progress, cancel)?;
        if cancel.is_set() {
            return Err(PlayerError::Cancelled);
        }
        *lock(&job.cache_key) = Some(cache_key.clone());
        let mut complete = self.pcm_cache.complete(&cache_key)?;
        if kind == Kind::Atmos && complete.as_ref().is_some_and(|folder| !self.pipeline.render_is_current(folder)) {
            self.log.write(&format!("render {cache_key}: stale generation, rendering again"));
            complete = None; // render() claims the directory afresh
        }
        if complete.is_some() {
            self.mark(&cache_key);
        }
        let source = match self.open_source(job, &cache_key, &result, complete, &mut progress) {
            Ok(source) => source,
            Err(error) => {
                self.unmark(&cache_key);
                return Err(error);
            }
        };
        if cancel.is_set() {
            self.stop_sessions([source]);
            self.unmark(&cache_key);
            return Err(PlayerError::Cancelled);
        }
        let total = source.total_frames().filter(|frames| *frames != 0).map(|frames| frames as f64 / RATE);
        let mut duration = result.duration_s.filter(|seconds| *seconds != 0.0);
        // the exact length of a complete file, else what the acquisition measured
        if (source.done() && total.is_some()) || duration.is_none() {
            duration = total;
        }
        if duration.is_none() {
            let known = job.track.duration_ms.filter(|milliseconds| *milliseconds != 0);
            duration = known.map(|milliseconds| milliseconds as f64 / 1000.0);
        }
        let prepared = Arc::new(Prepared {
            kind,
            format: kind.format(),
            format_detail: Mutex::new(format_detail_for(kind, Some(&result), source.summary().as_ref())),
            source,
            duration_s: duration,
            cache_key: cache_key.clone(),
            acquire: result,
            loudness: Mutex::default(),
        });
        self.attach_loudness(&prepared);
        self.await_loudness(&prepared, cancel);
        lock(&self.state).prepared.insert((job.key.clone(), kind), prepared.clone());
        Ok(prepared)
    }

    /// The ready source of the job's (key, kind), or one of the PCM cache entry it is made into
    /// that was prepared for another key: a withdrawn song plays its current version, which may
    /// be queued as an entry of its own. (Preparing that entry again would claim the directory
    /// a render of it may still be writing.)
    fn ready_entry(&self, job: &Job, cache_key: &str) -> Option<Arc<Prepared>> {
        let st = lock(&self.state);
        let own = st.prepared.get(&(job.key.clone(), job.kind));
        let shared = || st.prepared.values().find(|ready| ready.kind == job.kind && ready.cache_key == cache_key);
        own.or_else(shared).cloned()
    }

    /// The source of an acquired variant: the complete entry of the PCM cache, or a new render
    /// or decode into a directory claimed for it.
    fn open_source(
        &self,
        job: &Arc<Job>,
        cache_key: &str,
        result: &AcquireResult,
        complete: Option<PathBuf>,
        progress: &mut dyn FnMut(&str, Option<f64>),
    ) -> Result<Arc<dyn Source>, PlayerError> {
        let m4a = Path::new(&result.path);
        match job.kind {
            Kind::Atmos => match complete {
                Some(folder) => Ok(self.pipeline.complete_render(&folder)?),
                None => self.render(m4a, cache_key, result, progress, job),
            },
            // stereo.f32le (AAC), lossless.f32le or hires.f32le (ALAC), see PcmCache::KINDS
            Kind::Stereo | Kind::Lossless | Kind::HiRes => {
                let pcm_name = format!("{}.f32le", job.kind.as_str());
                let folder = match complete {
                    Some(folder) => folder,
                    None => {
                        let out_dir = self.claim(cache_key)?;
                        progress("decode", None);
                        self.pipeline.decode_stereo(m4a, &out_dir.join(&pcm_name))?;
                        out_dir
                    }
                };
                Ok(self.pipeline.pcm_file(&folder.join(&pcm_name), 2)?)
            }
            Kind::Binaural => match complete {
                Some(folder) => Ok(self.pipeline.pcm_file(&folder.join("binaural.f32le"), 2)?),
                None => {
                    let out_dir = self.claim(cache_key)?;
                    progress("render", None);
                    Ok(self.pipeline.render_binaural(m4a, &out_dir)?)
                }
            },
        }
    }

    fn claim(&self, cache_key: &str) -> Result<PathBuf, PlayerError> {
        match self.pcm_cache.claim(cache_key) {
            Ok(out_dir) => {
                self.mark(cache_key);
                Ok(out_dir)
            }
            Err(CacheError::Busy(reason)) => {
                Err(other(format!("another process is using this song's audio cache ({reason})")))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn render(
        &self,
        m4a: &Path,
        cache_key: &str,
        result: &AcquireResult,
        progress: &mut dyn FnMut(&str, Option<f64>),
        job: &Arc<Job>,
    ) -> Result<Arc<dyn Source>, PlayerError> {
        let cancel = &*job.cancel;
        let turn = self.gate.wait_turn(Some(job), cancel)?;
        let out_dir = self.claim(cache_key)?;
        let session = self.pipeline.render_session(m4a, &out_dir, result.duration_s);
        session.start()?;
        *lock(&job.session) = Some(session.clone());
        self.gate.started(turn, Some(job), &session);
        self.log.write(&format!("render start {cache_key} ({})", job.purpose().as_str()));
        progress("render", Some(0.0));
        let started = Instant::now();
        if let Err(error) = self.wait_render_start(&session, cancel, progress) {
            self.stop_sessions([session]);
            return Err(error);
        }
        self.log.write(&format!(
            "render ready {cache_key}: {:.1} s available after {:.2} s{}",
            session.frames_available() as f64 / RATE,
            started.elapsed().as_secs_f64(),
            if session.done() { " (complete)" } else { "" }
        ));
        Ok(session)
    }

    /// Adaptive start: the start buffer is available and the last `RENDER_RATE_WINDOW_S` produced
    /// audio at `RENDER_START_RATE` times real time or more, or the render is done, or the
    /// longest wait has passed (not while the render is frozen). `RENDER_START_TIMEOUT_S`
    /// without the start buffer is an error.
    fn wait_render_start(
        &self,
        session: &Arc<dyn Source>,
        cancel: &Event,
        progress: &mut dyn FnMut(&str, Option<f64>),
    ) -> Result<(), PlayerError> {
        let target = (self.tuning.min_buffer_seconds * RATE) as u64;
        let started = Instant::now();
        let mut history: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut last_progress: Option<Instant> = None;
        loop {
            if let Some(error) = session.error() {
                return Err(other(format!("render failed: {error}")));
            }
            if cancel.is_set() {
                return Err(PlayerError::Cancelled);
            }
            let available = session.frames_available();
            let now = Instant::now();
            history.push_back((now, available));
            while history.len() > 1 && (now - history[1].0).as_secs_f64() >= RENDER_RATE_WINDOW_S {
                history.pop_front();
            }
            if session.done() {
                return Ok(());
            }
            // the loop may spin per file growth once the target is reached
            if last_progress.is_none_or(|last| (now - last).as_secs_f64() >= 0.2) {
                last_progress = Some(now);
                let total = session.total_frames().filter(|total| *total != 0);
                progress("render", total.map(|total| (available as f64 / total as f64).min(1.0)));
            }
            let waited = (now - started).as_secs_f64();
            if available >= target {
                let (oldest, oldest_frames) = history[0];
                let span = (now - oldest).as_secs_f64();
                let rate = (span >= RENDER_RATE_MIN_SPAN_S)
                    .then(|| available.saturating_sub(oldest_frames) as f64 / span / RATE);
                let healthy = rate.is_some_and(|rate| rate >= RENDER_START_RATE);
                if healthy || (!session.paused() && waited >= self.tuning.render_start_max_wait_s) {
                    return Ok(());
                }
            } else if waited > RENDER_START_TIMEOUT_S {
                return Err(other("the render produced no audio in time"));
            }
            session.wait_for(if available < target { target } else { available + 1 }, RENDER_POLL_S);
        }
    }

    /// Stop the renders among `sources` that are not done and take them out of the gate.
    pub(super) fn stop_sessions(&self, sources: impl IntoIterator<Item = Arc<dyn Source>>) {
        for source in sources {
            if !source.done() {
                source.stop();
                self.log.write(&format!("render stop {}", label(&source)));
            }
            self.gate.release(&source);
        }
    }

    /// Use a stored measurement when there is one, else measure in the background.
    fn attach_loudness(self: &Arc<Self>, prepared: &Arc<Prepared>) {
        let source = &prepared.source;
        let total = source.total_frames().filter(|frames| *frames != 0);
        if let Some(stored) = loudness::stored_loudness(source.pcm_path())
            && source.done()
        {
            let frames = stored.frames.filter(|frames| *frames != 0);
            if frames.is_none() || total.is_none() || frames == total {
                *lock(&prepared.loudness) = Loudness::of(stored.lufs);
                return;
            }
        }
        if self.poster.is_shut_down() {
            return;
        }
        let job = LoudnessJob::new(prepared.clone());
        // a measurement of an earlier source of the same cache entry has nothing to say any more
        if let Some(earlier) = lock(&self.state).loudness_jobs.insert(prepared.cache_key.clone(), job.clone()) {
            earlier.cancel.set();
        }
        job.start(self);
    }

    /// Hold a start until Sound Check knows the track's trim: the full measurement of a complete
    /// file when it arrives within the tuning's `loudness_final_wait_s`, else the running
    /// estimate from the audio the start buffer holds (`loudness_start_wait_s` at most, so a
    /// stalled render never blocks a start).
    fn await_loudness(&self, prepared: &Arc<Prepared>, cancel: &Event) {
        if prepared.loudness().known || !self.sound_check() {
            return;
        }
        let Some(job) = lock(&self.state).loudness_jobs.get(&prepared.cache_key).cloned() else { return };
        let started = Instant::now();
        let complete = prepared.source.done();
        while !cancel.is_set() {
            let waited = started.elapsed().as_secs_f64();
            if job.final_event.is_set() || waited >= self.tuning.loudness_start_wait_s {
                break;
            }
            if job.estimate_event.is_set() && (!complete || waited >= self.tuning.loudness_final_wait_s) {
                break;
            }
            job.final_event.wait(Some(Duration::from_millis(20)));
        }
        if !prepared.loudness().known {
            self.log.write(&format!(
                "loudness {}: no value after {:.2} s; starting at 0 dB",
                prepared.cache_key,
                started.elapsed().as_secs_f64()
            ));
        }
    }
}

// --- the shared sink -------------------------------------------------------------------

/// Switches the live filter chain to a matrix profile off the command thread.
pub(super) struct ApplyJob {
    pub key: String,
    pub previous: Option<String>,
    error: Mutex<Option<DownmixError>>,
    alive: AtomicBool,
}

impl ApplyJob {
    pub fn start(shared: &Arc<Shared>, key: &str, previous: Option<String>) -> Arc<ApplyJob> {
        let job = Arc::new(ApplyJob {
            key: key.to_string(),
            previous,
            error: Mutex::new(None),
            alive: AtomicBool::new(true),
        });
        let (thread_job, thread_shared) = (job.clone(), shared.clone());
        let started = spawn("am-ui-downmix", move || {
            let applied = thread_shared.pipeline.downmix_apply(&thread_job.key);
            thread_job.finish(&thread_shared, applied.err());
        });
        if !started {
            job.finish(shared, Some(DownmixError::Failed("could not start a thread".to_string())));
        }
        job
    }

    fn finish(self: &Arc<Self>, shared: &Shared, error: Option<DownmixError>) {
        *lock(&self.error) = error;
        shared.post(Command::ApplyDone(self.clone()));
        self.alive.store(false, Ordering::SeqCst);
    }

    pub fn error(&self) -> Option<DownmixError> {
        lock(&self.error).clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

/// Sets the shared sink's preamp off the command thread.
pub(super) struct PreampJob {
    pub db: f64,
    outcome: Mutex<Option<Result<Applied, DownmixError>>>,
    alive: AtomicBool,
}

impl PreampJob {
    pub fn start(shared: &Arc<Shared>, db: f64) -> Arc<PreampJob> {
        let job = Arc::new(PreampJob { db, outcome: Mutex::new(None), alive: AtomicBool::new(true) });
        let (thread_job, thread_shared) = (job.clone(), shared.clone());
        let started = spawn("am-ui-preamp", move || {
            let outcome = thread_shared.pipeline.downmix_set_preamp(thread_job.db);
            thread_job.finish(&thread_shared, outcome);
        });
        if !started {
            job.finish(shared, Err(DownmixError::Failed("could not start a thread".to_string())));
        }
        job
    }

    fn finish(self: &Arc<Self>, shared: &Shared, outcome: Result<Applied, DownmixError>) {
        *lock(&self.outcome) = Some(outcome);
        shared.post(Command::PreampDone(self.clone()));
        self.alive.store(false, Ordering::SeqCst);
    }

    pub fn outcome(&self) -> Option<Result<Applied, DownmixError>> {
        lock(&self.outcome).clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

// --- loudness ----------------------------------------------------------------------------

/// Why a measurement gave no value.
enum Stopped {
    /// Cancelled, or the source failed or was stopped: there is nothing to measure.
    Cancelled,
    Failed(String),
}

/// Measures a prepared source's integrated loudness while it is produced: the PCM is fed into
/// the pipeline's stream meter as it grows, `LOUDNESS_SLICE_S` at a time; the running value
/// once the start buffer's audio is in is posted as an estimate (the worker applies it before
/// the track starts), refinements follow when it moves by `LOUDNESS_REFINE_DB` after twice the
/// audio, and the final value is stored next to the PCM and posted the moment the file is
/// complete. Without a stream meter: wait for completion, then measure the whole file.
pub(super) struct LoudnessJob {
    pub prepared: Arc<Prepared>,
    pub cancel: Arc<Event>,
    result: Mutex<Option<Measurement>>,
    error: Mutex<Option<String>>,
    estimate: Mutex<Option<f64>>,
    pub estimate_event: Event,
    pub final_event: Event,
}

impl LoudnessJob {
    fn new(prepared: Arc<Prepared>) -> Arc<LoudnessJob> {
        Arc::new(LoudnessJob {
            prepared,
            cancel: Arc::new(Event::new()),
            result: Mutex::new(None),
            error: Mutex::new(None),
            estimate: Mutex::new(None),
            estimate_event: Event::new(),
            final_event: Event::new(),
        })
    }

    fn start(self: &Arc<Self>, shared: &Arc<Shared>) {
        let (job, thread_shared) = (self.clone(), shared.clone());
        let started = spawn("am-ui-loudness", move || {
            let measured = catch_unwind(AssertUnwindSafe(|| job.measure(&thread_shared)));
            let measured = measured.unwrap_or_else(|panic| Err(Stopped::Failed(panic_text(&*panic))));
            job.finish(&thread_shared, measured);
        });
        if !started {
            self.finish(shared, Err(Stopped::Failed("could not start a thread".to_string())));
        }
    }

    fn finish(self: &Arc<Self>, shared: &Shared, measured: Result<Option<Measurement>, Stopped>) {
        let report = match measured {
            Ok(Some(result)) => {
                // known to the start that waits for the flag below, whatever the worker is at
                *lock(&self.prepared.loudness) = Loudness::of(result.lufs);
                *lock(&self.result) = Some(result);
                true
            }
            Ok(None) | Err(Stopped::Cancelled) => false,
            Err(Stopped::Failed(error)) => {
                *lock(&self.error) = Some(error);
                true
            }
        };
        // in the queue before the start that waits for the flag: the worker logs and applies
        // the value before it starts the track
        if report {
            shared.post(Command::LoudnessDone(self.clone()));
        }
        self.final_event.set();
    }

    pub fn result(&self) -> Option<Measurement> {
        lock(&self.result).clone()
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }

    pub fn estimate(&self) -> Option<f64> {
        *lock(&self.estimate)
    }

    /// The measurement of the whole source, stored next to its PCM; `None` when the source
    /// failed.
    fn measure(self: &Arc<Self>, shared: &Shared) -> Result<Option<Measurement>, Stopped> {
        let source = &self.prepared.source;
        let streamed = match self.measure_stream(shared) {
            Ok(result) => result,
            Err(Stopped::Cancelled) => return Err(Stopped::Cancelled),
            Err(Stopped::Failed(error)) => {
                shared.log.write(&format!(
                    "loudness {}: stream meter failed ({error}); measuring the complete file instead",
                    self.prepared.cache_key
                ));
                None
            }
        };
        let result = match streamed {
            Some(result) => result,
            None => match self.measure_complete(shared)? {
                Some(result) => result,
                None => return Ok(None),
            },
        };
        let _ = loudness::store_loudness(source.pcm_path(), &result);
        Ok(Some(result))
    }

    fn log_render_done(&self, shared: &Shared) {
        let source = &self.prepared.source;
        if let (Some(summary), Some(_)) = (source.summary(), source.started_at()) {
            let number = |name: &str| summary.get(name).and_then(Value::as_f64).unwrap_or(0.0);
            let frames = summary.get("frames").map_or_else(|| "None".to_string(), Value::to_string);
            shared.log.write(&format!(
                "render done {}: {frames} frames, {:.1} s renderer time ({:.1}x)",
                self.prepared.cache_key,
                number("elapsed_seconds"),
                number("realtime_factor")
            ));
        }
    }

    /// Without a stream meter: wait until the source is complete, measure the whole file.
    fn measure_complete(&self, shared: &Shared) -> Result<Option<Measurement>, Stopped> {
        let source = &self.prepared.source;
        while !(source.done() || source.error().is_some()) {
            if self.cancel.wait(Some(Duration::from_millis(500))) {
                return Err(Stopped::Cancelled);
            }
        }
        if source.error().is_some() {
            return Ok(None); // a failed or stopped render has nothing to measure
        }
        self.log_render_done(shared);
        if self.cancel.is_set() {
            return Err(Stopped::Cancelled);
        }
        let total = source.total_frames().filter(|frames| *frames != 0);
        let measured = shared.pipeline.measure_loudness(source.pcm_path(), source.channels(), total, &self.cancel);
        match measured {
            Ok(result) => Ok(Some(result)),
            Err(loudness::LoudnessError::Cancelled) => Err(Stopped::Cancelled),
            Err(error) => Err(Stopped::Failed(error.to_string())),
        }
    }

    fn measure_stream(self: &Arc<Self>, shared: &Shared) -> Result<Option<Measurement>, Stopped> {
        let source = &self.prepared.source;
        let opened = shared.pipeline.loudness_stream(source.pcm_path(), source.channels());
        let Some(mut stream) = opened.map_err(|error| Stopped::Failed(error.to_string()))? else {
            return Ok(None);
        };
        let fed = self.feed_stream(shared, &mut *stream);
        let measured = fed.and_then(|()| stream.finish().map_err(|error| Stopped::Failed(error.to_string())));
        if measured.is_err() {
            stream.close();
        }
        measured.map(Some)
    }

    /// Feed the stream meter until the source is complete and all of it is in.
    fn feed_stream(self: &Arc<Self>, shared: &Shared, stream: &mut dyn LoudnessStream) -> Result<(), Stopped> {
        let source = &self.prepared.source;
        let failed = |error: loudness::LoudnessError| Stopped::Failed(error.to_string());
        let min_frames = (shared.tuning.min_buffer_seconds * RATE) as u64;
        let mut estimate_at = 0;
        let mut logged_done = false;
        let mut stalled_since: Option<Instant> = None;
        loop {
            if self.cancel.is_set() || source.error().is_some() {
                return Err(Stopped::Cancelled); // a failed or stopped render has nothing to measure
            }
            let done = source.done();
            let available = source.frames_available();
            if done && !logged_done {
                logged_done = true;
                self.log_render_done(shared);
            }
            if available > stream.fed() {
                let before = stream.fed();
                let running = stream.feed(available.min(before + LOUDNESS_SLICE_FRAMES)).map_err(failed)?;
                let fed = stream.fed();
                if fed > before {
                    stalled_since = None;
                    let complete = done && fed >= available;
                    if let Some(running) = running.filter(|_| !complete && fed >= min_frames) {
                        let moved = self.estimate().is_none_or(|last| (running - last).abs() >= LOUDNESS_REFINE_DB);
                        if estimate_at == 0 || (fed >= 2 * estimate_at && moved) {
                            estimate_at = fed;
                            self.publish_estimate(shared, running, fed);
                        }
                    }
                    continue;
                }
            }
            if done {
                if stream.fed() >= available {
                    return Ok(());
                }
                let since = *stalled_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_secs_f64() >= LOUDNESS_STALL_S {
                    shared.log.write(&format!(
                        "loudness {}: file shorter than announced ({} of {available} frames); measuring what is there",
                        self.prepared.cache_key,
                        stream.fed()
                    ));
                    return Ok(());
                }
            }
            if self.cancel.wait(Some(Duration::from_secs_f64(RENDER_POLL_S))) {
                return Err(Stopped::Cancelled);
            }
        }
    }

    fn publish_estimate(self: &Arc<Self>, shared: &Shared, value: f64, fed: u64) {
        *lock(&self.prepared.loudness) = Loudness::of(Some(value));
        *lock(&self.estimate) = Some(value);
        shared.post(Command::LoudnessEstimate(self.clone(), fed));
        self.estimate_event.set();
    }
}

#[cfg(test)]
pub(super) mod testing {
    //! Stand-ins for the unit tests of the player's parts.
    use super::*;
    use crate::render::PcmSource;
    use serde_json::Map;

    /// A render that only keeps notes: frozen or not, stopped or not, finished or not.
    #[derive(Default)]
    pub struct Dummy {
        pub name: String,
        pub paused: AtomicBool,
        pub finished: AtomicBool,
        pub stopped: AtomicBool,
    }

    impl Dummy {
        pub fn new(name: &str) -> Arc<Dummy> {
            Arc::new(Dummy { name: name.to_string(), ..Dummy::default() })
        }
    }

    impl PcmSource for Dummy {
        fn pcm_path(&self) -> &Path {
            Path::new(&self.name)
        }

        fn channels(&self) -> usize {
            16
        }

        fn frames_available(&self) -> u64 {
            0
        }

        fn total_frames(&self) -> Option<u64> {
            None
        }

        fn done(&self) -> bool {
            self.finished.load(Ordering::SeqCst)
        }

        fn error(&self) -> Option<String> {
            self.stopped.load(Ordering::SeqCst).then(|| "render stopped".to_string())
        }
    }

    impl Source for Dummy {
        fn stop(&self) {
            self.stopped.store(true, Ordering::SeqCst);
        }

        fn pause(&self) {
            self.paused.store(true, Ordering::SeqCst);
        }

        fn resume(&self) {
            self.paused.store(false, Ordering::SeqCst);
        }

        fn paused(&self) -> bool {
            self.paused.load(Ordering::SeqCst)
        }

        fn running(&self) -> Option<bool> {
            Some(!self.done() && !self.stopped.load(Ordering::SeqCst))
        }
    }

    /// A job that no thread works on.
    pub fn idle_job(purpose: Purpose) -> Arc<Job> {
        Arc::new(Job {
            purpose: Mutex::new(purpose),
            track: Track::new(Some("1000000001"), "Song A", "Artist A"),
            key: "1000000001".to_string(),
            playing_id: Some("1000000001".to_string()),
            kind: Kind::Atmos,
            start_frame: AtomicU64::new(0),
            cancel: Arc::new(Event::new()),
            outcome: Mutex::new(None),
            cache_key: Mutex::new(None),
            session: Mutex::new(None),
            alive: AtomicBool::new(false),
        })
    }

    /// A ready source over a stand-in render.
    pub fn prepared(name: &str) -> Arc<Prepared> {
        Arc::new(Prepared {
            kind: Kind::Atmos,
            source: Dummy::new(name),
            format: Kind::Atmos.format(),
            format_detail: Mutex::new(String::new()),
            duration_s: None,
            cache_key: name.to_string(),
            acquire: AcquireResult {
                catalog_id: "1000000001".to_string(),
                path: String::new(),
                bytes: 0,
                variant: Map::new(),
                duration_s: None,
                from_cache: false,
            },
            loudness: Mutex::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::prepared;
    use super::*;

    const KEY: &str = "1000000001.atmos";

    #[test]
    fn a_second_preparation_waits_for_the_first_and_takes_its_source() {
        let preparing = Arc::new(Preparing::default());
        let cancel = Event::new();
        let first = preparing.enter(KEY, &cancel, || None).unwrap().ok().expect("nobody else prepares it");
        // another entry is not held up
        assert!(preparing.enter("1000000002.atmos", &cancel, || None).unwrap().is_ok());
        let made: Arc<Mutex<Option<Arc<Prepared>>>> = Arc::default();
        let (theirs, slot) = (preparing.clone(), made.clone());
        let second = thread::spawn(move || {
            let outcome = theirs.enter(KEY, &Event::new(), || lock(&slot).clone());
            matches!(outcome, Ok(Err(_)))
        });
        thread::sleep(Duration::from_millis(150));
        assert!(!second.is_finished(), "the second preparation waits while the first is at work");
        *lock(&made) = Some(prepared(KEY));
        drop(first);
        assert!(second.join().unwrap(), "it takes the source the first one made");
        // the entry is free again
        assert!(preparing.enter(KEY, &cancel, || None).unwrap().is_ok());
    }

    #[test]
    fn a_preparation_that_gave_up_lets_the_next_one_do_the_work() {
        let preparing = Arc::new(Preparing::default());
        let first = preparing.enter(KEY, &Event::new(), || None).unwrap().ok().expect("nobody else prepares it");
        let theirs = preparing.clone();
        let second = thread::spawn(move || matches!(theirs.enter(KEY, &Event::new(), || None), Ok(Ok(_))));
        thread::sleep(Duration::from_millis(50));
        drop(first); // it failed or was cancelled: there is no source to take
        assert!(second.join().unwrap(), "the second one prepares the entry itself");
    }

    #[test]
    fn a_cancelled_job_stops_waiting_for_its_turn() {
        let preparing = Arc::new(Preparing::default());
        let _first = preparing.enter(KEY, &Event::new(), || None).unwrap().ok().expect("nobody else prepares it");
        let cancel = Arc::new(Event::new());
        let (theirs, flag) = (preparing.clone(), cancel.clone());
        let second = thread::spawn(move || matches!(theirs.enter(KEY, &flag, || None), Err(PlayerError::Cancelled)));
        thread::sleep(Duration::from_millis(50));
        cancel.set();
        assert!(second.join().unwrap());
    }
}
