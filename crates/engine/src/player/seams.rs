//! What the player asks of the modules below it, as traits: the pipeline (acquire, render,
//! downmix, loudness), the audio output, the PCM cache and the sources they hand around. The
//! engine's own implementations are here too; tests pass stand-ins.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::acquire::{self, AcquireError, Acquirer, ProgressFn};
use crate::cache::{CacheError, MediaCache, PcmCache};
use crate::config::{Config, SAMPLE_RATE};
use crate::downmix::{self, Applied, Downmix, DownmixError, Profile};
use crate::lossless::Lossless;
use crate::loudness::{self, LoudnessError, Measurement, StreamMeter};
use crate::output::{OutputError, OutputOptions, PipeWireOutput, PlaybackLog};
use crate::render::{self, CompleteRender, PcmFile, PcmSource, RenderError, RenderSession, seconds};
use crate::types::AcquireResult;
use crate::util::Event;

// --- sources -----------------------------------------------------------------------

/// A PCM source as the player handles it: what the output reads ([`PcmSource`]) and, for a
/// render that is still being produced, the controls of its process. A complete file keeps the
/// defaults.
pub trait Source: PcmSource {
    /// The summary of a finished 16-channel render (`joc_object_count_max` names its objects).
    fn summary(&self) -> Option<Map<String, Value>> {
        None
    }

    /// When the render was started; `None` for a file that was complete when it was opened.
    fn started_at(&self) -> Option<Instant> {
        None
    }

    /// Start producing the audio.
    fn start(&self) -> Result<(), RenderError> {
        Ok(())
    }

    /// Block until at least `frames` are available, the source finished or failed, or `timeout`
    /// seconds passed. True when the frames are there or the source completed.
    fn wait_for(&self, frames: u64, timeout: f64) -> bool {
        let deadline = Instant::now().checked_add(seconds(timeout));
        loop {
            if self.frames_available() >= frames {
                return true;
            }
            if self.error().is_some() {
                return false;
            }
            if self.done() {
                return true;
            }
            let mut pause = Duration::from_millis(20);
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                pause = pause.min(deadline - now);
            }
            sleep(pause);
        }
    }

    /// End the production; what was produced stays readable.
    fn stop(&self) {}

    /// Freeze the production so that another render gets the processor.
    fn pause(&self) {}

    fn resume(&self) {}

    fn paused(&self) -> bool {
        false
    }

    /// Whether the producing process runs; `None` for a source without one.
    fn running(&self) -> Option<bool> {
        None
    }
}

impl Source for RenderSession {
    fn summary(&self) -> Option<Map<String, Value>> {
        RenderSession::summary(self)
    }

    fn started_at(&self) -> Option<Instant> {
        RenderSession::started_at(self)
    }

    fn start(&self) -> Result<(), RenderError> {
        RenderSession::start(self)
    }

    fn wait_for(&self, frames: u64, timeout: f64) -> bool {
        RenderSession::wait_for(self, frames, timeout)
    }

    fn stop(&self) {
        RenderSession::stop(self);
    }

    fn pause(&self) {
        RenderSession::pause(self);
    }

    fn resume(&self) {
        RenderSession::resume(self);
    }

    fn paused(&self) -> bool {
        RenderSession::paused(self)
    }

    fn running(&self) -> Option<bool> {
        Some(RenderSession::running(self))
    }
}

impl Source for CompleteRender {
    fn summary(&self) -> Option<Map<String, Value>> {
        Some(self.summary.clone())
    }
}

impl Source for PcmFile {}

/// Whether two handles name the same source object.
pub(super) fn same_source(a: &Arc<dyn Source>, b: &Arc<dyn Source>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

/// Whether the output's handle names the player's source object.
pub(super) fn same_pcm(a: &Arc<dyn PcmSource>, b: &Arc<dyn Source>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

// --- the output ----------------------------------------------------------------------

/// Called with a source when its first or its last frame has been audible.
pub type SourceCallback = Box<dyn Fn(Arc<dyn PcmSource>) + Send + Sync>;
/// Called with the message when the output dies.
pub type ErrorCallback = Box<dyn Fn(&str) + Send + Sync>;

/// The audio output as the player uses it: the methods of [`PipeWireOutput`] it calls.
pub trait Output: Send + Sync {
    fn on_source_started(&self, callback: SourceCallback);
    fn on_source_ended(&self, callback: SourceCallback);
    fn on_error(&self, callback: ErrorCallback);

    fn start(&self) -> Result<(), OutputError>;
    fn stop(&self);
    /// True while the output is started and has not failed.
    fn alive(&self) -> bool;

    /// Switch to a new track (`None` = silence); also clears the pending next source.
    fn set_source(&self, source: Option<Arc<dyn PcmSource>>, start_frame: u64);
    /// Gapless continuation once the current source is exhausted (`None` disarms it).
    fn set_next_source(&self, source: Option<Arc<dyn PcmSource>>);
    fn seek(&self, frame: u64);
    fn pause(&self);
    fn resume(&self);
    fn set_volume(&self, volume: f64);
    /// Sound Check trim of one source in dB, reached over `ramp_s` seconds when it plays.
    fn set_trim(&self, source: &Arc<dyn PcmSource>, db: f64, ramp_s: f64);
    fn set_sound_check(&self, enabled: bool);

    fn active_source(&self) -> Option<Arc<dyn PcmSource>>;
    fn next_source(&self) -> Option<Arc<dyn PcmSource>>;
    fn buffering(&self) -> bool;
    fn underruns(&self) -> u64;
    /// Gapless continuations into a next source so far.
    fn handoffs(&self) -> u64;
    fn position_frames(&self) -> u64;
    fn buffered_frames(&self) -> u64;
    /// Save the last seconds of the written stream as a WAV.
    fn dump_recent(&self, path: &Path) -> io::Result<PathBuf>;
}

impl Output for PipeWireOutput {
    fn on_source_started(&self, callback: SourceCallback) {
        PipeWireOutput::on_source_started(self, callback);
    }

    fn on_source_ended(&self, callback: SourceCallback) {
        PipeWireOutput::on_source_ended(self, callback);
    }

    fn on_error(&self, callback: ErrorCallback) {
        PipeWireOutput::on_error(self, callback);
    }

    fn start(&self) -> Result<(), OutputError> {
        PipeWireOutput::start(self)
    }

    fn stop(&self) {
        PipeWireOutput::stop(self);
    }

    fn alive(&self) -> bool {
        PipeWireOutput::alive(self)
    }

    fn set_source(&self, source: Option<Arc<dyn PcmSource>>, start_frame: u64) {
        PipeWireOutput::set_source(self, source, start_frame);
    }

    fn set_next_source(&self, source: Option<Arc<dyn PcmSource>>) {
        PipeWireOutput::set_next_source(self, source);
    }

    fn seek(&self, frame: u64) {
        PipeWireOutput::seek(self, frame);
    }

    fn pause(&self) {
        PipeWireOutput::pause(self);
    }

    fn resume(&self) {
        PipeWireOutput::resume(self);
    }

    fn set_volume(&self, volume: f64) {
        PipeWireOutput::set_volume(self, volume);
    }

    fn set_trim(&self, source: &Arc<dyn PcmSource>, db: f64, ramp_s: f64) {
        PipeWireOutput::set_trim(self, source, db, ramp_s);
    }

    fn set_sound_check(&self, enabled: bool) {
        PipeWireOutput::set_sound_check(self, enabled);
    }

    fn active_source(&self) -> Option<Arc<dyn PcmSource>> {
        PipeWireOutput::active_source(self)
    }

    fn next_source(&self) -> Option<Arc<dyn PcmSource>> {
        PipeWireOutput::next_source(self)
    }

    fn buffering(&self) -> bool {
        PipeWireOutput::buffering(self)
    }

    fn underruns(&self) -> u64 {
        PipeWireOutput::underruns(self)
    }

    fn handoffs(&self) -> u64 {
        PipeWireOutput::handoffs(self)
    }

    fn position_frames(&self) -> u64 {
        PipeWireOutput::position_frames(self)
    }

    fn buffered_frames(&self) -> u64 {
        PipeWireOutput::buffered_frames(self)
    }

    fn dump_recent(&self, path: &Path) -> io::Result<PathBuf> {
        PipeWireOutput::dump_recent(self, path)
    }
}

// --- the PCM cache -------------------------------------------------------------------

/// The PCM cache as the player uses it: the methods of [`PcmCache`] it calls.
pub trait PcmStore: Send + Sync {
    /// Directory of a finished, validated decode, or `None`.
    fn complete(&self, key: &str) -> Result<Option<PathBuf>, CacheError>;
    /// Fresh empty directory for a new decode; [`CacheError::Busy`] when another process uses
    /// the entry.
    fn claim(&self, key: &str) -> Result<PathBuf, CacheError>;
    /// Delete the least recently used entries, never those in `protect`.
    fn evict(&self, protect: &HashSet<String>) -> Result<(), CacheError>;
    fn mark_inuse(&self, key: &str) -> Result<(), CacheError>;
    fn unmark_inuse(&self, key: &str) -> Result<(), CacheError>;
}

impl PcmStore for PcmCache {
    fn complete(&self, key: &str) -> Result<Option<PathBuf>, CacheError> {
        PcmCache::complete(self, key)
    }

    fn claim(&self, key: &str) -> Result<PathBuf, CacheError> {
        PcmCache::claim(self, key)
    }

    fn evict(&self, protect: &HashSet<String>) -> Result<(), CacheError> {
        PcmCache::evict(self, protect)
    }

    fn mark_inuse(&self, key: &str) -> Result<(), CacheError> {
        PcmCache::mark_inuse(self, key)
    }

    fn unmark_inuse(&self, key: &str) -> Result<(), CacheError> {
        PcmCache::unmark_inuse(self, key)
    }
}

// --- loudness ------------------------------------------------------------------------

/// A running loudness measurement of a growing file: what the player uses of
/// [`StreamMeter`].
pub trait LoudnessStream: Send {
    /// Frames fed so far.
    fn fed(&self) -> u64;
    /// Push the file up to `frames_available` frames; the running integrated loudness.
    fn feed(&mut self, frames_available: u64) -> Result<Option<f64>, LoudnessError>;
    /// End the stream and return the measurement.
    fn finish(&mut self) -> Result<Measurement, LoudnessError>;
    /// Abandon the measurement.
    fn close(&mut self);
}

impl LoudnessStream for StreamMeter {
    fn fed(&self) -> u64 {
        StreamMeter::fed(self)
    }

    fn feed(&mut self, frames_available: u64) -> Result<Option<f64>, LoudnessError> {
        StreamMeter::feed(self, frames_available)
    }

    fn finish(&mut self) -> Result<Measurement, LoudnessError> {
        StreamMeter::finish(self, loudness::FINISH_TIMEOUT)
    }

    fn close(&mut self) {
        StreamMeter::close(self);
    }
}

// --- the pipeline ----------------------------------------------------------------------

/// Seam over acquire, render, downmix and loudness: the player only talks to these (tests pass
/// their own). [`EnginePipeline`] is the engine's.
pub trait Pipeline: Send + Sync {
    /// The decrypted file of one variant ("atmos", "stereo", "lossless", "hires") of a song.
    fn acquire(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        variant: &str,
        progress: ProgressFn<'_>,
        cancel: &Event,
    ) -> Result<AcquireResult, AcquireError>;

    /// Cheap (filesystem) check: the lossless route is built and signed in, and no recent
    /// service failure.
    fn lossless_available(&self) -> bool;

    /// While the route cannot be used: why, in a sentence for the user.
    fn lossless_problem(&self) -> Option<String> {
        None
    }

    /// At shutdown: stop the lossless service when this engine started it.
    fn shutdown_lossless(&self);

    /// A finished 16-channel render of the PCM cache.
    fn complete_render(&self, out_dir: &Path) -> Result<Arc<dyn Source>, RenderError>;

    /// A cached 16-channel render counts only if the active renderer generation produced it.
    fn render_is_current(&self, out_dir: &Path) -> bool;

    /// A render of `m4a` into `out_dir`, not started yet.
    fn render_session(&self, m4a: &Path, out_dir: &Path, expected_duration_s: Option<f64>) -> Arc<dyn Source>;

    /// A complete interleaved f32le file.
    fn pcm_file(&self, path: &Path, channels: usize) -> Result<Arc<dyn Source>, RenderError>;

    /// AAC or ALAC file -> 2-channel f32le file; the frames written.
    fn decode_stereo(&self, m4a: &Path, out_pcm: &Path) -> Result<u64, AcquireError>;

    fn binaural_available(&self) -> bool;

    /// A 2-channel headphone render of `m4a` into `out_dir` (blocking).
    fn render_binaural(&self, m4a: &Path, out_dir: &Path) -> Result<Arc<dyn Source>, RenderError>;

    /// The downmix profiles the user can pick.
    fn downmix_profiles(&self) -> Vec<Profile>;

    /// Switch the live filter chain to a matrix profile.
    fn downmix_apply(&self, key: &str) -> Result<(), DownmixError>;

    /// Change the shared sink's preamp; the live matrix preset stays.
    fn downmix_set_preamp(&self, db: f64) -> Result<Applied, DownmixError>;

    /// The sink's preamp in dB, when it is known.
    fn downmix_preamp(&self) -> Option<f64>;

    /// The matrix preset the sink runs, when it is known.
    fn downmix_current(&self) -> Option<String>;

    /// A new output at `volume`, not started yet.
    fn make_output(&self, volume: f64) -> Result<Arc<dyn Output>, OutputError>;

    /// Integrated loudness of a complete file (the first `frames` frames of it).
    fn measure_loudness(
        &self,
        pcm_path: &Path,
        channels: usize,
        frames: Option<u64>,
        cancel: &Event,
    ) -> Result<Measurement, LoudnessError>;

    /// A stream meter that gives a running value while the file grows, or `None` to fall back
    /// to `measure_loudness()` once the file is complete.
    fn loudness_stream(
        &self,
        pcm_path: &Path,
        channels: usize,
    ) -> Result<Option<Box<dyn LoudnessStream>>, LoudnessError>;
}

/// The engine's pipeline: the acquirer, the lossless route, the renderer, the downmix sink and
/// the loudness meters.
pub struct EnginePipeline {
    config: Arc<Config>,
    acquirer: Arc<Acquirer>,
    lossless: Arc<Lossless>,
    downmix: Arc<Downmix>,
    log: Arc<PlaybackLog>,
}

impl EnginePipeline {
    /// `log` is the playback log the output this pipeline makes writes to (the player's).
    pub fn new(
        config: Arc<Config>,
        acquirer: Arc<Acquirer>,
        lossless: Arc<Lossless>,
        downmix: Arc<Downmix>,
        log: Arc<PlaybackLog>,
    ) -> EnginePipeline {
        EnginePipeline { config, acquirer, lossless, downmix, log }
    }
}

impl Pipeline for EnginePipeline {
    fn acquire(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        variant: &str,
        progress: ProgressFn<'_>,
        cancel: &Event,
    ) -> Result<AcquireResult, AcquireError> {
        self.acquirer.acquire(catalog_id, media_cache, variant, Some(progress), Some(cancel))
    }

    fn lossless_available(&self) -> bool {
        self.lossless.available()
    }

    fn lossless_problem(&self) -> Option<String> {
        self.lossless.problem()
    }

    fn shutdown_lossless(&self) {
        self.lossless.stop_service_if_started();
    }

    fn complete_render(&self, out_dir: &Path) -> Result<Arc<dyn Source>, RenderError> {
        Ok(Arc::new(CompleteRender::new(out_dir)?))
    }

    fn render_is_current(&self, out_dir: &Path) -> bool {
        render::render_generation_of(out_dir).is_some_and(|generation| generation == self.config.render_generation())
    }

    fn render_session(&self, m4a: &Path, out_dir: &Path, expected_duration_s: Option<f64>) -> Arc<dyn Source> {
        Arc::new(RenderSession::new(self.config.clone(), m4a, out_dir, expected_duration_s))
    }

    fn pcm_file(&self, path: &Path, channels: usize) -> Result<Arc<dyn Source>, RenderError> {
        Ok(Arc::new(PcmFile::new(path, channels)?))
    }

    fn decode_stereo(&self, m4a: &Path, out_pcm: &Path) -> Result<u64, AcquireError> {
        acquire::decode_stereo(m4a, out_pcm)
    }

    fn binaural_available(&self) -> bool {
        false
    }

    fn render_binaural(&self, _m4a: &Path, _out_dir: &Path) -> Result<Arc<dyn Source>, RenderError> {
        Err(RenderError::Start("this engine has no binaural renderer".to_string()))
    }

    fn downmix_profiles(&self) -> Vec<Profile> {
        downmix::profiles()
    }

    fn downmix_apply(&self, key: &str) -> Result<(), DownmixError> {
        self.downmix.apply(key, None, None).map(|_| ())
    }

    fn downmix_set_preamp(&self, db: f64) -> Result<Applied, DownmixError> {
        self.downmix.set_preamp(db)
    }

    fn downmix_preamp(&self) -> Option<f64> {
        Some(self.downmix.current().preamp)
    }

    fn downmix_current(&self) -> Option<String> {
        Some(self.downmix.current().preset)
    }

    fn make_output(&self, volume: f64) -> Result<Arc<dyn Output>, OutputError> {
        let options = OutputOptions { volume, ..OutputOptions::default() };
        let output = Arc::new(PipeWireOutput::new(&self.config, self.log.clone(), options));
        // the engine's own downmix and equalizer run in the output's chain: it gets their values
        // now and after every change (a downmix of the shared sink gives none)
        let chain = Arc::downgrade(&output);
        self.downmix.deliver_to(Box::new(move |controls| {
            if let Some(output) = chain.upgrade() {
                output.set_dsp(crate::dsp::Settings::from_controls(controls, controls));
            }
        }));
        Ok(output)
    }

    fn measure_loudness(
        &self,
        pcm_path: &Path,
        channels: usize,
        frames: Option<u64>,
        cancel: &Event,
    ) -> Result<Measurement, LoudnessError> {
        loudness::measure_file(pcm_path, channels, frames, None, Some(cancel))
    }

    fn loudness_stream(
        &self,
        pcm_path: &Path,
        channels: usize,
    ) -> Result<Option<Box<dyn LoudnessStream>>, LoudnessError> {
        if !loudness::ffmpeg_available() {
            return Ok(None);
        }
        Ok(Some(Box::new(StreamMeter::new(pcm_path, channels, None, SAMPLE_RATE)?)))
    }
}
