//! The worker thread: it consumes the command queue and owns every decision of the playback
//! state machine. Its methods take `&mut self`, so none of them can be called while a guard of
//! the shared state is held. What it asks of the radio lives in two parts of its own:
//! `autoplay` (Autoplay and stations) and `equivalent` (songs whose catalog version was
//! withdrawn); `resume` keeps the queue for the next run.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use super::jobs::{ApplyJob, Job, LoudnessJob, PreampJob, Purpose, panic_text};
use super::queue::{Shuffler, before_autoplay};
use super::seams::Output;
use super::shared::{Command, Current, Prepared, RATE, Shared, State, unit};
use super::{
    Container, Kind, LOUDNESS_REFINE_RAMP_S, POSITION_INTERVAL_S, PlayerError, PlayerEvent, QueueEntry,
    RESTART_THRESHOLD_S, format_detail_for,
};
use crate::config::SAMPLE_RATE;
use crate::downmix::Profile;
use crate::loudness::{self, TARGET_LUFS, lock};
use crate::output::TRIM_RAMP_S;
use crate::render::PcmSource;
use crate::types::{AudioMode, PlayerState, RepeatMode, Station, Track};
use crate::util::python_repr;

mod autoplay;
mod equivalent;
mod resume;

use self::autoplay::Following;
use self::equivalent::Withdrawn;
pub(super) use self::resume::Keeper;

/// The four kinds of one song in the PCM cache.
const ALL_KINDS: [Kind; 4] = [Kind::Atmos, Kind::Stereo, Kind::Lossless, Kind::Binaural];

/// Hand the source's Sound Check trim to the output (`ramp_s`: how slowly a change while that
/// source plays is applied; the default is the output's 50 ms).
fn apply_trim(output: &Arc<dyn Output>, prepared: &Prepared, ramp_s: Option<f64>) {
    let loudness = prepared.loudness();
    let db = if loudness.known { loudness.trim_db } else { 0.0 };
    output.set_trim(&prepared.pcm(), db, ramp_s.unwrap_or(TRIM_RAMP_S));
}

pub(super) struct Worker {
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    /// The source armed as the output's next one.
    next_prepared: Option<Arc<Prepared>>,
    /// Songs known to have no Dolby Atmos version.
    no_atmos: HashSet<String>,
    /// Songs whose lossless acquisition failed this session.
    no_lossless: HashSet<String>,
    apply_job: Option<Arc<ApplyJob>>,
    preamp_job: Option<Arc<PreampJob>>,
    pending_preamp: Option<f64>,
    last_position_emit: Option<Instant>,
    shuffler: Shuffler,
    /// Autoplay and the station whose queue plays.
    following: Following,
    /// Songs whose catalog version was withdrawn.
    withdrawn: Withdrawn,
    /// What is kept for the next run; `None`: nothing is.
    keeper: Option<Keeper>,
}

impl Worker {
    pub fn new(shared: Arc<Shared>, commands: Receiver<Command>, keeper: Option<Keeper>) -> Worker {
        Worker {
            shared,
            commands,
            next_prepared: None,
            no_atmos: HashSet::new(),
            no_lossless: HashSet::new(),
            apply_job: None,
            preamp_job: None,
            pending_preamp: None,
            last_position_emit: None,
            shuffler: Shuffler::new(),
            following: Following::default(),
            withdrawn: Withdrawn::default(),
            keeper,
        }
    }

    pub fn run(mut self) {
        // report the matrix the sink really runs before any command
        let _ = catch_unwind(AssertUnwindSafe(|| self.check_live_profile()));
        let live_preamp = self.shared.pipeline.downmix_preamp();
        self.st().preamp = live_preamp;
        if self.st().restored() {
            // what came back from the last run is there before anything else happens
            self.emit_queue();
            self.emit_state();
            self.emit_position();
        }
        let interval = Duration::from_secs_f64(POSITION_INTERVAL_S);
        loop {
            let command = match self.commands.recv_timeout(interval) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
            };
            let name = command.as_ref().map_or("tick", Command::name);
            let handled = catch_unwind(AssertUnwindSafe(|| match command {
                Some(command) => {
                    self.handle(command);
                    // the queue was replaced or its position moved: Autoplay and stations
                    self.follow_queue();
                    self.shared.gate.poll();
                    self.maybe_emit_position(false);
                    self.remember(true, false);
                }
                None => {
                    self.tick();
                    self.remember(false, false);
                }
            }));
            if let Err(panic) = handled {
                // a bug must not kill the worker
                self.emit_error(format!("{name} failed: {}", panic_text(&*panic)), None, false);
            }
        }
        let _ = catch_unwind(AssertUnwindSafe(|| self.remember_last()));
        self.shared.cleanup();
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Play(tracks, index, container) => self.cmd_play(tracks, index, container.as_ref()),
            Command::Enqueue(tracks) => self.cmd_enqueue(tracks),
            Command::PlayNext(tracks) => self.cmd_play_next(tracks),
            Command::Jump(index) => self.cmd_jump(index),
            Command::JumpTo(id) => self.cmd_jump_to(id),
            Command::Remove(index) => self.cmd_remove(index),
            Command::RemoveEntry(id) => self.cmd_remove_entry(id),
            Command::MoveEntry(id, position) => self.cmd_move_entry(id, position),
            Command::Clear => self.cmd_clear(),
            Command::Toggle => self.cmd_toggle(),
            Command::Pause => self.cmd_pause(),
            Command::Resume => self.cmd_resume(),
            Command::Stop => self.cmd_stop(),
            Command::Next => self.cmd_next(),
            Command::Previous => self.cmd_previous(),
            Command::Seek(seconds) => self.cmd_seek(seconds),
            Command::SetVolume(volume) => self.cmd_set_volume(volume),
            Command::SetRepeat(mode) => self.cmd_set_repeat(mode),
            Command::SetShuffle(enabled) => self.cmd_set_shuffle(enabled),
            Command::SetMode(mode) => self.cmd_set_mode(mode),
            Command::SetStereoFallback(enabled) => self.cmd_set_stereo_fallback(enabled),
            Command::SetSoundCheck(enabled) => self.cmd_set_sound_check(enabled),
            Command::SetLossless(enabled) => self.cmd_set_lossless(enabled),
            Command::SetDownmixProfile(key) => self.cmd_set_downmix_profile(&key),
            Command::SetPreamp(db) => self.cmd_set_preamp(db),
            Command::SetAutoplay(enabled) => self.cmd_set_autoplay(enabled),
            Command::PlayStation(station) => self.cmd_play_station(station),
            Command::JobProgress(job, phase, fraction) => self.cmd_job_progress(&job, phase, fraction),
            Command::JobDone(job) => self.cmd_job_done(&job),
            Command::ApplyDone(job) => self.cmd_apply_done(&job),
            Command::PreampDone(job) => self.cmd_preamp_done(&job),
            Command::LoudnessEstimate(job, fed) => self.cmd_loudness_estimate(&job, fed),
            Command::LoudnessDone(job) => self.cmd_loudness_done(&job),
            Command::RadioDone(job) => self.cmd_radio_done(&job),
            Command::SourceStarted => self.refresh_format_detail(),
            Command::SourceEnded(source) => self.cmd_source_ended(&source),
            Command::OutputError(message) => self.cmd_output_error(&message),
            Command::Shutdown => {}
        }
    }

    fn tick(&mut self) {
        self.shared.gate.poll();
        self.refresh_format_detail();
        self.maybe_emit_position(true);
    }

    // ------------------------------------------------------------------ small helpers

    /// The shared state. The guard must be gone before any other method of the worker runs:
    /// taking `&mut self` here lets the compiler see to that.
    fn st(&mut self) -> MutexGuard<'_, State> {
        lock(&self.shared.state)
    }

    fn log(&mut self, line: &str) {
        self.shared.log.write(line);
    }

    fn state(&mut self) -> PlayerState {
        self.st().state
    }

    fn current(&mut self) -> Option<Current> {
        self.st().current.clone()
    }

    fn output(&mut self) -> Option<Arc<dyn Output>> {
        self.st().output.clone()
    }

    fn next_index(&mut self, auto: bool) -> Option<i64> {
        self.st().next_index(auto)
    }

    /// Make the entry at `index` the one to play. The song that played leaves the snapshot in
    /// the same step: no reader sees the new position with the old song.
    fn move_to(&mut self, index: i64, unpause: bool) {
        let mut st = self.st();
        st.index = index;
        st.leave_current();
        if unpause {
            st.paused_requested = false;
        }
    }

    fn entry_at(&mut self, index: i64) -> Option<Arc<QueueEntry>> {
        let st = self.st();
        usize::try_from(index).ok().and_then(|index| st.queue.get(index)).cloned()
    }

    /// The key a song plays under: what ready sources and jobs are told apart by.
    fn key_of(&mut self, track: &Track) -> String {
        track.key()
    }

    /// Whether the song has no Dolby Atmos version: Apple's audio traits say so (a song of the
    /// catalog), or an attempt found none this session. A song of the library alone carries no
    /// traits and is asked for the first time it plays.
    fn without_atmos(&mut self, track: &Track) -> bool {
        let key = self.key_of(track);
        track.atmos_known() == Some(false) || self.no_atmos.contains(&key)
    }

    /// The catalog id that is acquired for a song: the catalog's current version when the song's
    /// own one was withdrawn, else its own.
    fn playing_id(&mut self, track: &Track) -> Option<String> {
        self.shared.equivalents.playing_id(track)
    }

    /// Whether a change of the matrix profile is still being applied.
    fn applying(&mut self) -> bool {
        self.apply_job.as_ref().is_some_and(|job| job.is_alive())
    }

    fn emit_state(&mut self) {
        let snapshot = self.shared.snapshot();
        self.shared.emit(PlayerEvent::State(Box::new(snapshot)));
    }

    fn emit_queue(&mut self) {
        let event = {
            let st = self.st();
            PlayerEvent::Queue {
                keys: st.queue.iter().map(|entry| entry.track.key()).collect(),
                index: st.index,
                ids: st.queue.iter().map(|entry| entry.id).collect(),
                autoplay: st.queue.iter().map(|entry| entry.autoplay).collect(),
            }
        };
        self.shared.emit(event);
    }

    /// The position of the entry with that id.
    fn index_of(&mut self, id: u64) -> Option<usize> {
        self.st().queue.iter().position(|entry| entry.id == id)
    }

    fn emit_error(&mut self, message: String, catalog_id: Option<String>, fatal: bool) {
        let named = catalog_id.as_deref().filter(|id| !id.is_empty()).map(|id| format!(" [{id}]")).unwrap_or_default();
        self.log(&format!("error{}: {message}{named}", if fatal { " (fatal)" } else { "" }));
        self.shared.emit(PlayerEvent::Error { message, catalog_id, fatal });
    }

    fn maybe_emit_position(&mut self, force: bool) {
        let output = {
            let st = self.st();
            if st.state != PlayerState::Playing || st.current.is_none() {
                return;
            }
            st.output.clone()
        };
        let Some(output) = output else { return };
        let due = self.last_position_emit.is_none_or(|last| last.elapsed().as_secs_f64() >= POSITION_INTERVAL_S);
        if !force && !due {
            return;
        }
        self.last_position_emit = Some(Instant::now());
        self.shared.emit(PlayerEvent::Position {
            position: output.position_frames() as f64 / RATE,
            buffered: output.buffered_frames() as f64 / RATE,
        });
    }

    /// Say where the song stands now, whatever the state: on the output, or where it will start
    /// (a seek while nothing plays, a song kept from the last run).
    fn emit_position(&mut self) {
        let (position, buffered) = {
            let st = self.st();
            match (&st.current, &st.output) {
                (Some(_), Some(output)) if st.active() => {
                    (output.position_frames() as f64 / RATE, output.buffered_frames() as f64 / RATE)
                }
                _ => (st.pending_start_frame as f64 / RATE, 0.0),
            }
        };
        self.last_position_emit = Some(Instant::now());
        self.shared.emit(PlayerEvent::Position { position, buffered });
    }

    fn set_error(&mut self, message: String, fatal: bool, catalog_id: Option<String>) {
        {
            let mut st = self.st();
            st.state = PlayerState::Error;
            st.error = Some(message.clone());
            st.switching = false;
        }
        self.emit_error(message, catalog_id, fatal);
        self.emit_state();
    }

    // ------------------------------------------------------------------ settings

    fn profiles(&mut self) -> Vec<Profile> {
        self.shared.pipeline.downmix_profiles().into_iter().filter(|profile| !profile.key.is_empty()).collect()
    }

    /// "matrix" or "render" for a profile the pipeline offers.
    fn profile_kind(&mut self, key: Option<&str>) -> Option<String> {
        let key = key?;
        self.profiles().into_iter().find(|profile| profile.key == key).map(|profile| profile.kind)
    }

    fn render_profile_active(&mut self) -> bool {
        let profile = self.st().profile.clone();
        self.profile_kind(profile.as_deref()).as_deref() == Some("render")
    }

    /// Report the matrix the user's sink really runs (never applies anything at start-up).
    fn check_live_profile(&mut self) {
        if self.render_profile_active() {
            return;
        }
        let Some(live) = self.shared.pipeline.downmix_current().filter(|live| !live.is_empty()) else { return };
        let changed = {
            let mut st = self.st();
            let changed = st.profile.as_deref() != Some(live.as_str());
            if changed {
                st.profile = Some(live);
            }
            changed
        };
        if changed {
            self.emit_state();
        }
    }

    // ------------------------------------------------------------------ kinds

    /// The PCM kind the settings ask for; with a track, what is known about it is applied (no
    /// Atmos version -> the 2-channel kind when the fallback is on; a failed lossless attempt
    /// -> AAC).
    fn kind_for_mode(&mut self, track: Option<&Track>) -> Kind {
        let (mode, stereo_fallback) = {
            let st = self.st();
            (st.mode, st.stereo_fallback)
        };
        if mode == AudioMode::Stereo {
            return self.stereo_kind(track);
        }
        if let Some(track) = track
            && stereo_fallback
            && self.without_atmos(track)
        {
            return self.stereo_kind(Some(track));
        }
        if self.render_profile_active() { Kind::Binaural } else { Kind::Atmos }
    }

    /// The 2-channel kind: lossless (ALAC) when enabled, available and not known to fail for
    /// this song (its audio traits name no lossless version, or an attempt failed), else stereo
    /// (AAC).
    fn stereo_kind(&mut self, track: Option<&Track>) -> Kind {
        let failed = match track {
            Some(track) => {
                let key = self.key_of(track);
                let traits = &track.audio_traits;
                let no_lossless =
                    !traits.is_empty() && !traits.iter().any(|t| t == "lossless" || t == "hi-res-lossless");
                no_lossless || self.no_lossless.contains(&key)
            }
            None => false,
        };
        let lossless = self.st().lossless;
        if lossless && !failed && self.shared.pipeline.lossless_available() { Kind::Lossless } else { Kind::Stereo }
    }

    fn stereo_fallback(&mut self) -> bool {
        self.st().stereo_fallback
    }

    fn apply_trim(&mut self, prepared: &Prepared, ramp_s: Option<f64>) {
        if let Some(output) = self.output() {
            apply_trim(&output, prepared, ramp_s);
        }
    }

    fn start_job(&mut self, purpose: Purpose, track: &Track, kind: Kind, start_frame: u64) -> Arc<Job> {
        let (key, playing_id) = (self.key_of(track), self.playing_id(track));
        Job::start(&self.shared, purpose, track, key, playing_id, kind, start_frame)
    }

    // ------------------------------------------------------------------ output

    /// Start the output on first use; after a death, one restart per play command.
    fn ensure_output(&mut self) -> bool {
        let existing = self.output();
        if existing.as_ref().is_some_and(|output| output.alive()) {
            return true;
        }
        let output = match existing {
            Some(output) => {
                output.stop(); // dead or never started: the output restarts in place
                output
            }
            None => {
                let volume = self.st().volume;
                let made = match &self.shared.output_factory {
                    Some(factory) => factory(volume),
                    None => self.shared.pipeline.make_output(volume),
                };
                match made {
                    Ok(output) => output,
                    Err(error) => {
                        self.set_error(format!("audio output unavailable: {error}"), true, None);
                        return false;
                    }
                }
            }
        };
        let poster = self.shared.poster.clone();
        output.on_source_started(Box::new(move |_| poster.post(Command::SourceStarted)));
        let poster = self.shared.poster.clone();
        output.on_source_ended(Box::new(move |source| poster.post(Command::SourceEnded(source))));
        let poster = self.shared.poster.clone();
        output.on_error(Box::new(move |message| poster.post(Command::OutputError(message.to_string()))));
        if let Err(error) = output.start() {
            self.st().output = Some(output);
            self.set_error(format!("audio output failed to start: {error}"), true, None);
            return false;
        }
        let (volume, sound_check) = {
            let st = self.st();
            (st.volume, st.sound_check)
        };
        output.set_volume(volume);
        output.set_sound_check(sound_check);
        self.st().output = Some(output);
        true
    }

    // ------------------------------------------------------------------ commands: queue

    fn cmd_play(&mut self, tracks: Vec<Track>, index: usize, container: Option<&Container>) {
        self.forget_station_request();
        self.replace_queue(tracks, index, container, None);
    }

    /// A new queue, started at `index`; with `station`, the queue of that station (which keeps
    /// the order of its songs). What was asked for the queue that goes is not wanted any more.
    fn replace_queue(
        &mut self,
        tracks: Vec<Track>,
        index: usize,
        container: Option<&Container>,
        station: Option<Station>,
    ) {
        self.cancel_jobs(true, true, true);
        self.forget_queue();
        let entries = self.shared.new_entries(tracks, container, false);
        let empty = {
            let shuffler = &mut self.shuffler;
            let mut st = lock(&self.shared.state);
            st.queue = entries;
            st.original = None;
            st.station = station;
            st.index = if st.queue.is_empty() { -1 } else { index.min(st.queue.len() - 1) as i64 };
            st.paused_requested = false;
            st.pending_start_frame = 0;
            if st.shuffle && st.station.is_none() && !st.queue.is_empty() {
                st.apply_shuffle_order(true, shuffler);
            }
            if !st.queue.is_empty() {
                st.leave_current();
            }
            st.queue.is_empty()
        };
        self.emit_queue();
        if empty {
            self.cmd_stop();
            return;
        }
        self.start_current(0, None, 0);
    }

    /// Add to the end of the user's part of the queue: before the first unplayed entry of
    /// Autoplay.
    fn cmd_enqueue(&mut self, tracks: Vec<Track>) {
        if self.refuse_station_edit() {
            return;
        }
        let entries = self.shared.new_entries(tracks, None, false);
        {
            let mut st = self.st();
            let current = st.entry_at_index().map(|entry| entry.id);
            let at = before_autoplay(&st.queue, current);
            st.queue.splice(at..at, entries.iter().cloned());
            if let Some(original) = st.original.as_mut() {
                let at = before_autoplay(original, current);
                original.splice(at..at, entries);
            }
            if st.index < 0 && !st.queue.is_empty() {
                st.index = 0;
            }
        }
        self.emit_queue();
        self.emit_state();
        self.arm_prefetch();
    }

    fn cmd_play_next(&mut self, tracks: Vec<Track>) {
        if self.refuse_station_edit() {
            return;
        }
        let entries = self.shared.new_entries(tracks, None, false);
        {
            let mut st = self.st();
            let at = usize::try_from(st.index + 1).unwrap_or(0).min(st.queue.len());
            st.queue.splice(at..at, entries.iter().cloned());
            let current = st.entry_at_index().map(|entry| entry.id);
            if let Some(original) = st.original.as_mut() {
                let found = original.iter().position(|entry| Some(entry.id) == current);
                let at = found.map_or(original.len(), |index| index + 1);
                original.splice(at..at, entries);
            }
            if st.index < 0 && !st.queue.is_empty() {
                st.index = 0;
            }
        }
        self.emit_queue();
        self.emit_state();
        self.arm_prefetch();
    }

    fn cmd_jump(&mut self, index: usize) {
        if index >= self.st().queue.len() {
            return;
        }
        self.move_to(index as i64, true);
        self.emit_queue();
        self.start_current(0, None, 0);
    }

    fn cmd_remove(&mut self, index: usize) {
        if !self.refuse_station_edit() {
            self.remove_at(index);
        }
    }

    fn remove_at(&mut self, index: usize) {
        let (was_current, busy, takes_its_place, restored) = {
            let mut st = self.st();
            if index >= st.queue.len() {
                return;
            }
            let restored = st.restored();
            let removed = st.queue.remove(index);
            if let Some(original) = st.original.as_mut() {
                original.retain(|entry| entry.id != removed.id);
            }
            let index = index as i64;
            let was_current = index == st.index;
            if index < st.index {
                st.index -= 1;
            }
            if st.index >= st.queue.len() as i64 {
                st.index = st.queue.len() as i64 - 1;
            }
            let busy =
                !restored && matches!(st.state, PlayerState::Playing | PlayerState::Paused | PlayerState::Loading);
            let takes_its_place = index < st.queue.len() as i64;
            if was_current && busy && takes_its_place {
                // the entry that took its place starts
                st.index = index;
                st.leave_current();
            }
            if was_current && restored && takes_its_place {
                // it stands where this one stood: paused, at its beginning, nothing loaded
                st.pending_start_frame = 0;
            }
            (was_current, busy, takes_its_place, restored)
        };
        self.emit_queue();
        if !was_current {
            self.emit_state();
            self.arm_prefetch();
        } else if busy && takes_its_place {
            self.start_current(0, None, 0);
        } else if (busy || restored) && !takes_its_place {
            self.cmd_stop();
        } else {
            if restored {
                self.emit_position();
            }
            self.emit_state();
        }
    }

    fn cmd_jump_to(&mut self, id: u64) {
        if let Some(index) = self.index_of(id) {
            self.cmd_jump(index);
        }
    }

    fn cmd_remove_entry(&mut self, id: u64) {
        if self.refuse_station_edit() {
            return;
        }
        if let Some(index) = self.index_of(id) {
            self.remove_at(index);
        }
    }

    /// Put an entry at another place of the queue. The current entry stays current wherever it
    /// moves; what follows it may be another entry now, which the prefetch then follows.
    fn cmd_move_entry(&mut self, id: u64, position: usize) {
        if self.refuse_station_edit() {
            return;
        }
        {
            let mut st = self.st();
            let Some(from) = st.queue.iter().position(|entry| entry.id == id) else { return };
            let to = position.min(st.queue.len() - 1);
            if from == to {
                return;
            }
            let current = st.entry_at_index().map(|entry| entry.id);
            let entry = st.queue.remove(from);
            st.queue.insert(to, entry);
            if let Some(index) = st.queue.iter().position(|entry| Some(entry.id) == current) {
                st.index = index as i64;
            }
        }
        self.emit_queue();
        self.emit_state();
        self.arm_prefetch();
    }

    fn cmd_clear(&mut self) {
        if self.refuse_station_edit() {
            return;
        }
        // nothing asked for before is wanted any more, a station either
        self.forget_station_request();
        self.forget_queue();
        self.cmd_stop();
        {
            let mut st = self.st();
            st.queue = Vec::new();
            st.original = None;
            st.index = -1;
        }
        self.emit_queue();
        self.emit_state();
    }

    // ------------------------------------------------------------------ commands: transport

    fn cmd_toggle(&mut self) {
        match self.state() {
            PlayerState::Playing => self.cmd_pause(),
            PlayerState::Paused => self.cmd_resume(),
            PlayerState::Loading => {
                {
                    let mut st = self.st();
                    st.paused_requested = !st.paused_requested;
                }
                self.emit_state();
            }
            PlayerState::Idle | PlayerState::Error => self.start_where_it_stopped(),
        }
    }

    /// Start the entry at the queue position, at the frame a seek while stopped asked for (or
    /// where the song kept from the last run stood).
    fn start_where_it_stopped(&mut self) {
        let start_frame = {
            let mut st = self.st();
            if st.queue.is_empty() || st.index < 0 {
                return;
            }
            st.paused_requested = false;
            st.pending_start_frame
        };
        self.start_current(start_frame, None, 0);
    }

    fn cmd_pause(&mut self) {
        {
            let mut st = self.st();
            st.paused_requested = true;
            if st.state == PlayerState::Playing
                && let Some(output) = st.output.clone()
            {
                output.pause();
                st.state = PlayerState::Paused;
            }
        }
        self.emit_state();
    }

    fn cmd_resume(&mut self) {
        let (state, output, has_current) = {
            let mut st = self.st();
            st.paused_requested = false;
            (st.state, st.output.clone(), st.current.is_some())
        };
        match (state, output) {
            (PlayerState::Paused, Some(output)) if has_current => {
                output.resume();
                self.st().state = PlayerState::Playing;
                self.emit_state();
            }
            // the song kept from the last run: it is loaded now, from where it stood
            (PlayerState::Paused, _) if !has_current => self.start_where_it_stopped(),
            (PlayerState::Idle | PlayerState::Error, _) if self.has_position() => self.start_where_it_stopped(),
            _ => self.emit_state(),
        }
    }

    fn has_position(&mut self) -> bool {
        let st = self.st();
        !st.queue.is_empty() && st.index >= 0
    }

    fn cmd_stop(&mut self) {
        self.cancel_jobs(true, true, true);
        self.next_prepared = None;
        let restored = {
            let mut st = self.st();
            let restored = st.restored();
            if let Some(output) = st.output.clone()
                && output.alive()
            {
                output.set_source(None, 0);
                output.set_next_source(None);
            }
            st.current = None;
            st.state = PlayerState::Idle;
            st.error = None;
            st.switching = false;
            st.loading_phase = None;
            st.loading_progress = None;
            st.pending_start_frame = 0;
            restored
        };
        self.log("stop");
        if restored {
            self.emit_position(); // the place it was kept at is gone
        }
        self.emit_state();
    }

    fn cmd_next(&mut self) {
        let Some(index) = self.next_index(false) else {
            self.cmd_stop();
            return;
        };
        self.move_to(index, true);
        self.emit_queue();
        self.start_current(0, None, 0);
    }

    fn cmd_previous(&mut self) {
        let (index, count, repeat, playing, restored) = {
            let st = self.st();
            let playing = if st.current.is_some() && st.active() { st.output.clone() } else { None };
            let restored = st.restored().then_some(st.pending_start_frame);
            (st.index, st.queue.len() as i64, st.repeat_mode(), playing, restored)
        };
        if index < 0 || count == 0 {
            return;
        }
        let position = match (&playing, restored) {
            (Some(output), _) => output.position_frames() as f64 / RATE,
            (None, Some(frame)) => frame as f64 / RATE,
            (None, None) => 0.0,
        };
        if position > RESTART_THRESHOLD_S {
            self.cmd_seek(0.0);
            return;
        }
        let mut index = index - 1;
        if index < 0 {
            if repeat == RepeatMode::All && count > 1 {
                index = count - 1;
            } else if restored.is_some() {
                self.cmd_seek(0.0); // the song stays as it is, at its beginning
                return;
            } else {
                self.start_current(0, None, 0);
                return;
            }
        }
        self.move_to(index, true);
        self.emit_queue();
        self.start_current(0, None, 0);
    }

    fn cmd_seek(&mut self, seconds: f64) {
        let target = (seconds * RATE).round_ties_even();
        let mut frame = if target.is_finite() && target > 0.0 { target as u64 } else { 0 };
        let playing = {
            let st = self.st();
            match (&st.current, &st.output) {
                (Some(current), Some(output)) if st.active() => Some((current.prepared.clone(), output.clone())),
                _ => None,
            }
        };
        match playing {
            Some((prepared, output)) => {
                if let Some(total) = prepared.source.total_frames().filter(|total| *total != 0) {
                    frame = frame.min(total - 1);
                }
                output.seek(frame);
            }
            None => {
                let mut st = self.st();
                if st.restored()
                    && let Some(ms) = st.entry_at_index().and_then(|entry| entry.track.duration_ms)
                    && ms > 0
                {
                    // inside the song, as far as that is known before it is loaded
                    frame = frame.min(((ms as f64 / 1000.0 * RATE) as u64).saturating_sub(1));
                }
                st.pending_start_frame = frame;
                if let Some(job) = &st.load_job {
                    job.set_start_frame(frame);
                }
            }
        }
        self.remember_seek();
        self.emit_position();
        self.emit_state();
    }

    fn cmd_set_volume(&mut self, volume: f64) {
        {
            let mut st = self.st();
            st.volume = unit(volume);
            if let Some(output) = &st.output {
                output.set_volume(st.volume);
            }
        }
        self.shared.save_settings();
        self.emit_state();
    }

    /// The setting changes in any case; on the queue of a station it has no effect. Autoplay
    /// applies only while repeat is off: its unplayed entries go when repeat comes on, and it
    /// asks again when repeat goes off.
    fn cmd_set_repeat(&mut self, mode: RepeatMode) {
        let (before, station) = {
            let mut st = self.st();
            let before = st.repeat;
            st.repeat = mode;
            (before, st.station.is_some())
        };
        self.shared.save_settings();
        if !station && mode != RepeatMode::Off {
            self.drop_autoplay_entries();
        }
        self.emit_state();
        self.arm_prefetch();
        if !station && mode == RepeatMode::Off && before != RepeatMode::Off {
            self.resume_autoplay();
        }
    }

    /// The setting changes in any case; the queue of a station keeps its order.
    fn cmd_set_shuffle(&mut self, enabled: bool) {
        {
            let shuffler = &mut self.shuffler;
            let mut st = lock(&self.shared.state);
            if enabled == st.shuffle {
                return;
            }
            st.shuffle = enabled;
            if st.station.is_none() {
                if enabled {
                    st.apply_shuffle_order(true, shuffler);
                } else {
                    st.restore_order();
                }
            }
        }
        self.shared.save_settings();
        self.emit_queue();
        self.emit_state();
        self.arm_prefetch();
    }

    // ------------------------------------------------------------------ commands: sound

    fn cmd_set_mode(&mut self, mode: AudioMode) {
        {
            let mut st = self.st();
            if mode == st.mode {
                return;
            }
            st.mode = mode;
        }
        self.shared.save_settings();
        self.emit_state();
        if mode == AudioMode::Atmos {
            self.report_current_without_atmos();
        }
        self.switch_current_if_needed();
    }

    /// The user switched Dolby Atmos on while a song that is known to have no Atmos version
    /// plays its 2-channel version: the switch cannot take effect for it, which is said once.
    /// (A song that is not known yet is asked for; the failed switch says it then.)
    fn report_current_without_atmos(&mut self) {
        let playing = {
            let st = self.st();
            st.current.clone().filter(|_| st.active() && st.stereo_fallback)
        };
        let Some(current) = playing else { return };
        let track = &current.entry.track;
        if self.without_atmos(track) {
            let message = format!("{} has no Dolby Atmos version; staying with the stereo version", track.name);
            self.emit_error(message, track.catalog_id.clone(), false);
        }
    }

    fn cmd_set_stereo_fallback(&mut self, enabled: bool) {
        self.st().stereo_fallback = enabled;
        self.shared.save_settings();
        self.emit_state();
    }

    fn cmd_set_lossless(&mut self, enabled: bool) {
        {
            let mut st = self.st();
            if enabled == st.lossless {
                return;
            }
            st.lossless = enabled;
        }
        self.shared.save_settings();
        self.log(&format!("lossless {}", if enabled { "on" } else { "off" }));
        self.emit_state();
        self.switch_current_if_needed();
    }

    fn cmd_set_sound_check(&mut self, enabled: bool) {
        let (changed, output) = {
            let mut st = self.st();
            let changed = st.sound_check != enabled;
            st.sound_check = enabled;
            (changed, st.output.clone())
        };
        if changed {
            self.shared.save_settings();
            if let Some(output) = output {
                output.set_sound_check(enabled);
            }
            self.log(&format!("sound check {}", if enabled { "on" } else { "off" }));
        }
        self.emit_state();
    }

    fn cmd_set_downmix_profile(&mut self, key: &str) {
        let Some(kind) = self.profile_kind(Some(key)) else {
            self.emit_error(format!("unknown downmix profile {}", python_repr(key)), None, false);
            return;
        };
        let previous = self.st().profile.clone();
        if kind == "render" {
            self.st().profile = Some(key.to_string());
            self.shared.save_settings();
            self.emit_state();
            self.switch_current_if_needed();
            return;
        }
        if self.applying() {
            self.emit_error("a downmix profile change is still in progress".to_string(), None, false);
            return;
        }
        {
            let mut st = self.st();
            st.profile = Some(key.to_string());
            st.switching = true;
        }
        self.emit_state();
        self.apply_job = Some(ApplyJob::start(&self.shared, key, previous.clone()));
        if self.profile_kind(previous.as_deref()).as_deref() == Some("render") {
            self.switch_current_if_needed(); // back from the binaural render to the 16-channel one
        }
    }

    fn cmd_apply_done(&mut self, job: &Arc<ApplyJob>) {
        if !self.apply_job.as_ref().is_some_and(|current| Arc::ptr_eq(current, job)) {
            return;
        }
        self.apply_job = None;
        {
            let mut st = self.st();
            st.switching = st.switch_job.is_some();
        }
        match job.error() {
            Some(error) => {
                // report what the sink really runs
                let live = self.shared.pipeline.downmix_current();
                let live_matrix = match &live {
                    Some(live) => self.profile_kind(Some(live)).as_deref() == Some("matrix"),
                    None => false,
                };
                let previous_render = self.profile_kind(job.previous.as_deref()).as_deref() == Some("render");
                {
                    let mut st = self.st();
                    if st.profile.as_deref() == Some(job.key.as_str()) {
                        if live_matrix {
                            st.profile = live;
                        } else if !previous_render {
                            st.profile = job.previous.clone();
                        }
                    }
                }
                self.emit_error(format!("could not apply the downmix profile: {error}"), None, false);
            }
            None => self.shared.save_settings(),
        }
        self.emit_state();
    }

    fn cmd_set_preamp(&mut self, db: f64) {
        if !(-60.0..=20.0).contains(&db) {
            self.emit_error("preamp must be between -60 and +20 dB".to_string(), None, false);
            return;
        }
        let preamp = self.st().preamp;
        if preamp.is_some_and(|preamp| (db - preamp).abs() < 1e-9) && self.pending_preamp.is_none() {
            return; // nothing to change; never touch the shared sink without a reason
        }
        if self.preamp_job.as_ref().is_some_and(|job| job.is_alive()) {
            self.pending_preamp = Some(db); // the latest value wins once the running change finishes
            return;
        }
        self.pending_preamp = None;
        self.preamp_job = Some(PreampJob::start(&self.shared, db));
    }

    fn cmd_preamp_done(&mut self, job: &Arc<PreampJob>) {
        if !self.preamp_job.as_ref().is_some_and(|current| Arc::ptr_eq(current, job)) {
            return;
        }
        self.preamp_job = None;
        let applied = match job.outcome() {
            Some(Ok(applied)) => Some(applied.preamp),
            Some(Err(error)) => {
                self.emit_error(format!("could not change the preamp: {error}"), None, false);
                None
            }
            None => None,
        };
        let preamp = applied.or_else(|| self.shared.pipeline.downmix_preamp());
        self.st().preamp = preamp;
        if let Some(pending) = self.pending_preamp.take()
            && preamp.is_none_or(|preamp| (pending - preamp).abs() > 1e-9)
        {
            self.preamp_job = Some(PreampJob::start(&self.shared, pending));
        }
        self.emit_state();
    }

    /// Bring the playing (or loading) song to the kind the settings now ask for.
    fn switch_current_if_needed(&mut self) {
        self.arm_prefetch();
        let playing = {
            let st = self.st();
            st.current.clone().filter(|_| st.active())
        };
        let Some(current) = playing else {
            let loading = {
                let st = self.st();
                st.load_job.clone().filter(|_| st.state == PlayerState::Loading)
            };
            if let Some(job) = loading {
                let wanted = self.kind_for_mode(Some(&job.track));
                if job.kind != wanted {
                    self.start_current(job.start_frame(), None, 0);
                }
            }
            return;
        };
        let track = &current.entry.track;
        let wanted = self.kind_for_mode(Some(track));
        let switch_job = self.st().switch_job.clone();
        if current.prepared.kind == wanted {
            if let Some(job) = switch_job {
                job.cancel.set();
                let applying = self.applying();
                {
                    let mut st = self.st();
                    st.switch_job = None;
                    st.switching = applying;
                }
                self.emit_state();
            }
            return;
        }
        let key = self.key_of(track);
        if let Some(job) = &switch_job {
            if job.matches(&key, wanted) {
                return;
            }
            job.cancel.set();
        }
        if let Some(ready) = self.shared.ready(&key, wanted) {
            self.st().switch_job = None;
            self.complete_switch(&ready);
            return;
        }
        // the song's own prefetch in that kind (repeat one) becomes the switch
        let adopted = {
            let mut st = self.st();
            st.switching = true;
            let prefetch = st.prefetch_job.clone().filter(|job| job.matches(&key, wanted) && job.is_alive());
            if let Some(job) = &prefetch {
                job.adopt(Purpose::Switch, 0);
                st.switch_job = prefetch.clone();
                st.prefetch_job = None;
            }
            prefetch.is_some()
        };
        self.emit_state();
        if !adopted {
            let job = self.start_job(Purpose::Switch, track, wanted, 0);
            self.st().switch_job = Some(job);
        }
    }

    fn complete_switch(&mut self, prepared: &Arc<Prepared>) {
        let Some(output) = self.output() else { return };
        let position = output.position_frames();
        let applying = self.applying();
        {
            // the output is told while the state is held: a reader that sees the new variant
            // finds it on the output (the gate is asked too, hence the guard taken directly)
            let mut st = lock(&self.shared.state);
            let Some(current) = st.current.as_mut() else { return };
            current.prepared = prepared.clone();
            current.handoffs_at_start = output.handoffs();
            st.switching = applying;
            apply_trim(&output, prepared, None);
            self.shared.gate.set_current(&prepared.source);
            output.set_source(Some(prepared.pcm()), position);
        }
        self.log(&format!("switch {} @ {position} ({})", prepared.cache_key, prepared.format_detail()));
        self.emit_state();
        self.after_transition();
    }

    // ------------------------------------------------------------------ loading

    fn cancel_jobs(&mut self, load: bool, prefetch: bool, switch: bool) {
        let applying = self.applying();
        let mut st = self.st();
        if load && let Some(job) = st.load_job.take() {
            job.cancel.set();
        }
        if prefetch && let Some(job) = st.prefetch_job.take() {
            job.cancel.set();
        }
        if switch && let Some(job) = st.switch_job.take() {
            job.cancel.set();
            st.switching = applying;
        }
    }

    /// Begin playing the entry at the queue position: instantly when prepared, else through a
    /// load job. Entries that cannot be played are passed over, `attempts` of them so far.
    fn start_current(&mut self, mut start_frame: u64, mut kind: Option<Kind>, mut attempts: usize) {
        while !self.try_start_current(start_frame, kind) {
            if !self.pass_over(attempts) {
                return;
            }
            (start_frame, kind, attempts) = (0, None, attempts + 1);
        }
    }

    /// Move the queue position past an entry that cannot be played; false when there is nothing
    /// left to try (playback is stopped then).
    fn pass_over(&mut self, attempts: usize) -> bool {
        let length = self.st().queue.len();
        if attempts + 1 >= length {
            self.cmd_stop();
            return false;
        }
        let Some(index) = self.next_index(false) else {
            self.cmd_stop();
            return false;
        };
        self.move_to(index, false);
        self.emit_queue();
        true
    }

    fn skip_unplayable(&mut self, attempts: usize) {
        if self.pass_over(attempts) {
            self.start_current(0, None, attempts + 1);
        }
    }

    /// One attempt of `start_current()`; false when the entry cannot be played and was reported.
    fn try_start_current(&mut self, start_frame: u64, kind: Option<Kind>) -> bool {
        self.cancel_jobs(true, false, true);
        let entry = {
            let mut st = self.st();
            let entry = st.entry_at_index().cloned();
            if entry.is_some() {
                st.leave_current();
                st.error = None;
                st.pending_start_frame = start_frame;
            }
            entry
        };
        let Some(entry) = entry else {
            self.cmd_stop();
            return true;
        };
        self.next_prepared = None;
        let track = &entry.track;
        if !track.playable() {
            self.emit_error(format!("{} is not available for playback", track.name), None, false);
            return false;
        }
        let key = self.key_of(track);
        let mut kind = match kind {
            Some(kind) => kind,
            None => self.kind_for_mode(Some(track)),
        };
        if kind.rendered() && self.without_atmos(track) {
            if self.stereo_fallback() {
                kind = self.stereo_kind(Some(track));
            } else {
                let message = format!("{} has no Dolby Atmos version; skipped", track.name);
                self.emit_error(message, track.catalog_id.clone(), false);
                return false;
            }
        }
        if let Some(ready) = self.shared.ready(&key, kind) {
            self.play_prepared(&ready, &entry, start_frame);
            return true;
        }
        let prefetch = {
            let mut st = self.st();
            st.state = PlayerState::Loading;
            st.loading_phase = None;
            st.loading_progress = None;
            st.switching = false;
            if let Some(output) = &st.output
                && output.alive()
            {
                output.set_source(None, 0);
            }
            st.prefetch_job.clone()
        };
        let name = format!("{}.{}", track.catalog_id.as_deref().unwrap_or_default(), kind.as_str());
        match prefetch {
            Some(prefetch) if prefetch.matches(&key, kind) && prefetch.is_alive() => {
                prefetch.adopt(Purpose::Load, start_frame);
                {
                    let mut st = self.st();
                    st.load_job = Some(prefetch);
                    st.prefetch_job = None;
                }
                self.log(&format!("load {name}: adopting the running prefetch"));
            }
            _ => {
                let job = self.start_job(Purpose::Load, track, kind, start_frame);
                self.st().load_job = Some(job);
                self.log(&format!("load {name} @ {start_frame}"));
            }
        }
        self.emit_state();
        true
    }

    fn play_prepared(&mut self, prepared: &Arc<Prepared>, entry: &Arc<QueueEntry>, start_frame: u64) {
        if !self.ensure_output() {
            return;
        }
        let Some(output) = self.output() else { return };
        let applying = self.applying();
        let mut start_frame = start_frame;
        {
            // the gate is asked while the state is held, so this is the one guard taken directly
            let mut st = lock(&self.shared.state);
            st.current = Some(Current {
                entry: entry.clone(),
                prepared: prepared.clone(),
                handoffs_at_start: output.handoffs(),
            });
            st.loading_phase = None;
            st.loading_progress = None;
            st.error = None;
            st.switching = applying;
            if prepared.source.total_frames().is_some_and(|total| total != 0 && start_frame >= total) {
                start_frame = 0;
            }
            apply_trim(&output, prepared, None);
            self.shared.gate.set_current(&prepared.source);
            output.set_source(Some(prepared.pcm()), start_frame);
            if st.paused_requested {
                output.pause();
                st.state = PlayerState::Paused;
            } else {
                output.resume();
                st.state = PlayerState::Playing;
            }
            st.pending_start_frame = 0;
        }
        let loudness = prepared.loudness();
        self.log(&format!(
            "play {} @ {start_frame} ({}; trim {:+.1} dB{})",
            prepared.cache_key,
            prepared.format_detail(),
            loudness.trim_db,
            if loudness.known { "" } else { " pending" }
        ));
        self.emit_state();
        self.last_position_emit = None;
        self.after_transition();
    }

    /// Housekeeping once a track (or variant) is on the output: caches and the next prefetch.
    fn after_transition(&mut self) {
        let mut protect: HashSet<String> = HashSet::new();
        let mut keep_keys: HashSet<String> = HashSet::new();
        let (neighbours, output) = {
            let st = self.st();
            let mut indexes = vec![st.index];
            indexes.extend(st.next_index(true));
            if st.index > 0 {
                indexes.push(st.index - 1);
            } else if st.repeat_mode() == RepeatMode::All && st.queue.len() > 1 {
                indexes.push(st.queue.len() as i64 - 1);
            }
            let entry = |index: i64| usize::try_from(index).ok().and_then(|index| st.queue.get(index)).cloned();
            (indexes.into_iter().filter_map(entry).collect::<Vec<_>>(), st.output.clone())
        };
        for entry in &neighbours {
            keep_keys.insert(self.key_of(&entry.track));
            if let Some(catalog_id) = self.playing_id(&entry.track) {
                protect.extend(ALL_KINDS.iter().map(|kind| format!("{catalog_id}.{}", kind.as_str())));
            }
        }
        let live: Vec<Arc<dyn PcmSource>> =
            output.iter().flat_map(|output| [output.active_source(), output.next_source()]).flatten().collect();
        let (dropped, kept) = {
            let mut st = self.st();
            let mut dropped = Vec::new();
            st.prepared.retain(|(key, _), prepared| {
                let keep = keep_keys.contains(key) || live.iter().any(|source| prepared.is(source));
                if !keep {
                    dropped.push(prepared.clone());
                }
                keep
            });
            let kept: HashSet<String> = st.prepared.values().map(|prepared| prepared.cache_key.clone()).collect();
            (dropped, kept)
        };
        self.shared.stop_sessions(dropped.iter().map(|prepared| prepared.source.clone()));
        for prepared in &dropped {
            let job = {
                let mut st = self.st();
                let measures_it =
                    st.loudness_jobs.get(&prepared.cache_key).is_some_and(|job| Arc::ptr_eq(&job.prepared, prepared));
                if measures_it { st.loudness_jobs.remove(&prepared.cache_key) } else { None }
            };
            if let Some(job) = job {
                job.cancel.set();
            }
            self.shared.unmark(&prepared.cache_key);
        }
        protect.extend(kept.iter().cloned());
        protect.extend(self.in_flight_keys());
        for cache_key in &kept {
            let marked = self.st().inuse.contains(cache_key);
            if !marked {
                self.shared.mark(cache_key);
            }
        }
        if let Err(error) = self.shared.pcm_cache.evict(&protect) {
            self.log(&format!("evict failed: {error}"));
        }
        self.arm_prefetch();
    }

    /// The PCM cache entries of the jobs that are still at work.
    fn in_flight_keys(&mut self) -> Vec<String> {
        let st = self.st();
        let jobs = [&st.load_job, &st.prefetch_job, &st.switch_job];
        jobs.into_iter().flatten().filter(|job| job.is_alive()).filter_map(|job| job.cache_key()).collect()
    }

    /// Nothing follows (or nothing may be prepared for it): the output's next source is cleared.
    fn disarm_prefetch(&mut self, output: &Arc<dyn Output>) {
        self.cancel_jobs(false, true, false);
        self.next_prepared = None;
        output.set_next_source(None);
    }

    /// Prepare the next queue item in the current kind and hand it to the output for gapless
    /// play.
    fn arm_prefetch(&mut self) {
        let output = {
            let st = self.st();
            if st.current.is_none() || !st.active() {
                return;
            }
            st.output.clone()
        };
        let Some(output) = output else { return };
        let Some(entry) = self.next_index(true).and_then(|index| self.entry_at(index)) else {
            self.disarm_prefetch(&output);
            return;
        };
        let track = &entry.track;
        if self.known_unavailable(track) {
            self.disarm_prefetch(&output); // its turn reports it
            return;
        }
        let key = self.key_of(track);
        let mut kind = self.kind_for_mode(Some(track));
        if kind.rendered() && self.without_atmos(track) {
            if !self.stereo_fallback() {
                self.disarm_prefetch(&output);
                return;
            }
            kind = self.stereo_kind(Some(track));
        }
        let prefetch = self.st().prefetch_job.clone();
        if prefetch.as_ref().is_some_and(|job| !job.matches(&key, kind)) {
            // a stale prefetch must not keep rendering (and holding the gate)
            self.cancel_jobs(false, true, false);
        }
        if let Some(ready) = self.shared.ready(&key, kind) {
            apply_trim(&output, &ready, None);
            output.set_next_source(Some(ready.pcm()));
            self.next_prepared = Some(ready);
            return;
        }
        self.next_prepared = None;
        output.set_next_source(None);
        if self.st().prefetch_job.as_ref().is_some_and(|job| job.is_alive()) {
            return;
        }
        self.cancel_jobs(false, true, false);
        if !track.playable() {
            return;
        }
        let job = self.start_job(Purpose::Prefetch, track, kind, 0);
        self.st().prefetch_job = Some(job);
        self.log(&format!("prefetch {}.{}", track.catalog_id.as_deref().unwrap_or_default(), kind.as_str()));
    }

    // ------------------------------------------------------------------ commands: job results

    fn cmd_job_progress(&mut self, job: &Arc<Job>, phase: String, fraction: Option<f64>) {
        if job.cancel.is_set() {
            return;
        }
        let phase_changed = {
            let mut st = self.st();
            let loading = st.load_job.as_ref().is_some_and(|load| Arc::ptr_eq(load, job));
            let changed = loading && st.loading_phase.as_deref() != Some(phase.as_str());
            if loading {
                st.loading_phase = Some(phase.clone());
                st.loading_progress = fraction;
            }
            changed
        };
        self.shared.emit(PlayerEvent::Progress { phase, fraction, catalog_id: job.catalog_id() });
        if phase_changed {
            // download fractions arrive often: they travel as "progress" only
            self.emit_state();
        }
    }

    fn cmd_job_done(&mut self, job: &Arc<Job>) {
        let outcome = match job.outcome() {
            Some(outcome) if !job.cancel.is_set() && !matches!(outcome, Err(PlayerError::Cancelled)) => outcome,
            _ => {
                if let Some(cache_key) = job.cache_key() {
                    let prepared = self.st().prepared.values().any(|prepared| prepared.cache_key == cache_key);
                    if !prepared {
                        self.shared.unmark(&cache_key);
                    }
                }
                return;
            }
        };
        let purpose = {
            let mut st = self.st();
            let is = |slot: &Option<Arc<Job>>| slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, job));
            if is(&st.load_job) {
                st.load_job = None;
                Some(Purpose::Load)
            } else if is(&st.prefetch_job) {
                st.prefetch_job = None;
                Some(Purpose::Prefetch)
            } else if is(&st.switch_job) {
                st.switch_job = None;
                Some(Purpose::Switch)
            } else {
                None
            }
        };
        match purpose {
            Some(Purpose::Load) => self.finish_load(job, outcome),
            Some(Purpose::Prefetch) => self.finish_prefetch(job, outcome),
            Some(Purpose::Switch) => self.finish_switch(job, outcome),
            None => {}
        }
    }

    fn finish_load(&mut self, job: &Arc<Job>, outcome: Result<Arc<Prepared>, PlayerError>) {
        let track = &job.track;
        let error = match outcome {
            Ok(prepared) => {
                let entry = self.st().entry_at_index().cloned();
                match entry {
                    Some(entry) => self.play_prepared(&prepared, &entry, job.start_frame()),
                    None => self.cmd_stop(),
                }
                return;
            }
            Err(error) => error,
        };
        if error.is_not_entitled() {
            // a withdrawn catalog version: its current one plays, or the song is passed over
            self.load_refused(job);
            return;
        }
        if error.is_no_atmos() && job.kind.rendered() {
            self.no_atmos.insert(job.key.clone());
            if self.stereo_fallback() {
                // the 2-channel version simply plays: the snapshot's format says what is audible
                let kind = self.stereo_kind(Some(track));
                self.start_current(job.start_frame(), Some(kind), 0);
            } else {
                let message = format!("{} has no Dolby Atmos version; skipped", track.name);
                self.emit_error(message, track.catalog_id.clone(), false);
                self.skip_unplayable(0);
            }
            return;
        }
        if job.kind == Kind::Lossless && error.is_lossless() {
            self.no_lossless.insert(job.key.clone());
            let message = format!("{}: {error}; playing the AAC version", track.name);
            self.emit_error(message, track.catalog_id.clone(), false);
            self.start_current(job.start_frame(), Some(Kind::Stereo), 0);
            return;
        }
        let message = format!("could not play {}: {error}", track.name);
        self.emit_error(message.clone(), track.catalog_id.clone(), false);
        {
            let mut st = self.st();
            st.state = PlayerState::Error;
            st.error = Some(message);
        }
        self.emit_state();
        self.skip_after_error();
    }

    fn skip_after_error(&mut self) {
        let Some(index) = self.next_index(false) else { return };
        self.move_to(index, false);
        self.emit_queue();
        self.start_current(0, None, 0);
    }

    fn finish_prefetch(&mut self, job: &Arc<Job>, outcome: Result<Arc<Prepared>, PlayerError>) {
        let name = format!("{}.{}", job.catalog_id(), job.kind.as_str());
        let prepared = match outcome {
            Ok(prepared) => prepared,
            Err(error) => {
                if error.is_no_atmos() && job.kind.rendered() {
                    self.no_atmos.insert(job.key.clone());
                    self.arm_prefetch(); // the stereo fallback's prefetch, or nothing
                } else if job.kind == Kind::Lossless && error.is_lossless() {
                    self.no_lossless.insert(job.key.clone());
                    self.log(&format!("prefetch {name} failed: {error}; AAC instead"));
                    self.arm_prefetch(); // the AAC decode
                } else if error.is_not_entitled() {
                    self.log(&format!("prefetch {name} failed: {error}"));
                    self.prefetch_refused(job); // its current version is looked for now
                } else {
                    self.log(&format!("prefetch {name} failed: {error}"));
                }
                return; // other failures are reported when the track is actually played
            }
        };
        let next = self.next_index(true).and_then(|index| self.entry_at(index));
        let Some(next) = next else { return };
        if self.key_of(&next.track) != job.key || self.current().is_none() {
            return;
        }
        if let Some(output) = self.output() {
            apply_trim(&output, &prepared, None);
            output.set_next_source(Some(prepared.pcm()));
        }
        self.next_prepared = Some(prepared);
    }

    fn finish_switch(&mut self, job: &Arc<Job>, outcome: Result<Arc<Prepared>, PlayerError>) {
        let applying = self.applying();
        let track = &job.track;
        let prepared = match outcome {
            Ok(prepared) => prepared,
            Err(error) => {
                self.st().switching = applying;
                if error.is_no_atmos() {
                    self.no_atmos.insert(job.key.clone());
                    let message = format!("{} has no Dolby Atmos version; staying with the stereo version", track.name);
                    self.emit_error(message, track.catalog_id.clone(), false);
                } else if job.kind == Kind::Lossless && error.is_lossless() {
                    self.no_lossless.insert(job.key.clone());
                    let message = format!("{}: {error}; staying with the AAC version", track.name);
                    self.emit_error(message, track.catalog_id.clone(), false);
                    self.emit_state();
                    self.switch_current_if_needed(); // the mode may ask for the AAC decode now
                    return;
                } else {
                    let message = format!("could not switch {}: {error}", track.name);
                    self.emit_error(message, track.catalog_id.clone(), false);
                }
                self.emit_state();
                return;
            }
        };
        let wanted = match self.current() {
            Some(current) if self.key_of(&current.entry.track) == job.key => {
                self.kind_for_mode(Some(&current.entry.track)) == job.kind
            }
            _ => false,
        };
        if !wanted {
            self.st().switching = applying;
            self.emit_state();
            return;
        }
        self.complete_switch(&prepared);
    }

    fn is_current(&mut self, prepared: &Arc<Prepared>) -> bool {
        self.st().current.as_ref().is_some_and(|current| Arc::ptr_eq(&current.prepared, prepared))
    }

    /// A running value from the first seconds of a source: applied (slowly, if it already plays).
    fn cmd_loudness_estimate(&mut self, job: &Arc<LoudnessJob>, fed: u64) {
        let prepared = &job.prepared;
        let live = self.st().loudness_jobs.get(&prepared.cache_key).is_some_and(|live| Arc::ptr_eq(live, job));
        let Some(estimate) = job.estimate().filter(|_| live && !job.cancel.is_set()) else { return };
        self.log(&format!(
            "loudness {}: {estimate:.1} LUFS after {:.0} s, trim {:+.1} dB (estimate)",
            prepared.cache_key,
            fed as f64 / f64::from(SAMPLE_RATE),
            prepared.loudness().trim_db
        ));
        if self.shared.is_prepared(prepared) {
            self.apply_trim(prepared, Some(LOUDNESS_REFINE_RAMP_S));
        }
        if self.is_current(prepared) {
            self.emit_state();
        }
    }

    fn cmd_loudness_done(&mut self, job: &Arc<LoudnessJob>) {
        let prepared = &job.prepared;
        {
            let mut st = self.st();
            if st.loudness_jobs.get(&prepared.cache_key).is_some_and(|live| Arc::ptr_eq(live, job)) {
                st.loudness_jobs.remove(&prepared.cache_key);
            }
        }
        if job.cancel.is_set() {
            return;
        }
        if let Some(error) = job.error() {
            self.log(&format!("loudness {} failed: {error}", prepared.cache_key));
            return;
        }
        let Some(result) = job.result() else { return };
        // the measuring thread has put the value into the source; the last estimate is the job's
        let trim_db = loudness::trim_db(result.lufs, TARGET_LUFS);
        let measured = result.lufs.map_or_else(|| "silent".to_string(), |lufs| format!("{lufs:.1} LUFS"));
        let estimated = match (job.estimate(), result.lufs) {
            (Some(estimate), Some(_)) => format!(" (estimate was {estimate:.1})"),
            _ => String::new(),
        };
        self.log(&format!(
            "loudness {}: {measured}, peak {:.3}, trim {trim_db:+.1} dB{estimated}",
            prepared.cache_key, result.peak
        ));
        if self.shared.is_prepared(prepared) {
            self.apply_trim(prepared, Some(LOUDNESS_REFINE_RAMP_S));
        }
        if self.is_current(prepared) {
            self.emit_state();
        }
    }

    // ------------------------------------------------------------------ commands: output callbacks

    fn cmd_source_ended(&mut self, source: &Arc<dyn PcmSource>) {
        let Some(current) = self.current().filter(|current| current.prepared.is(source)) else { return };
        let Some(output) = self.output() else { return };
        let track = &current.entry.track;
        if let Some(error) = source.error().filter(|error| !error.is_empty()) {
            self.emit_error(format!("{}: {error}", track.name), track.catalog_id.clone(), false);
        }
        let ended = &current.prepared.cache_key;
        let Some(index) = self.next_index(true) else {
            self.cancel_jobs(false, true, false);
            self.next_prepared = None;
            {
                let mut st = self.st();
                st.current = None;
                st.state = PlayerState::Idle;
                st.pending_start_frame = 0;
                output.set_source(None, 0);
            }
            self.log(&format!("ended {ended}: end of the queue"));
            self.emit_state();
            return;
        };
        let handed_off = self.next_prepared.clone().filter(|next| {
            output.handoffs() > current.handoffs_at_start
                && output.active_source().is_some_and(|active| next.is(&active))
        });
        let entry = self.entry_at(index);
        match (handed_off, entry) {
            (Some(next), Some(entry)) => {
                self.next_prepared = None;
                {
                    let mut st = self.st();
                    st.index = index;
                    st.current = Some(Current { entry, prepared: next.clone(), handoffs_at_start: output.handoffs() });
                    st.state = if st.paused_requested { PlayerState::Paused } else { PlayerState::Playing };
                }
                self.emit_queue();
                self.shared.gate.set_current(&next.source);
                self.log(&format!("ended {ended}: continued gaplessly into {}", next.cache_key));
                self.emit_state();
                self.last_position_emit = None;
                self.after_transition();
            }
            _ => {
                self.move_to(index, false);
                self.emit_queue();
                self.log(&format!("ended {ended}: starting the next track"));
                self.start_current(0, None, 0);
            }
        }
    }

    fn cmd_output_error(&mut self, message: &str) {
        self.cancel_jobs(true, true, true);
        self.set_error(format!("audio output stopped: {message}"), false, None);
    }

    /// The format line gains the number of objects once a progressive render has finished.
    fn refresh_format_detail(&mut self) {
        let Some(current) = self.current() else { return };
        let prepared = &current.prepared;
        if prepared.kind != Kind::Atmos || prepared.format_detail().contains("objects") {
            return;
        }
        let Some(summary) = prepared.summary().filter(|summary| !summary.is_empty()) else { return };
        let detail = format_detail_for(Kind::Atmos, Some(&prepared.acquire), Some(&summary));
        if detail != prepared.format_detail() {
            *lock(&prepared.format_detail) = detail;
            self.emit_state();
        }
    }
}
