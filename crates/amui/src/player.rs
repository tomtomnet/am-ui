//! Queue and playback state machine on top of acquire, render and output.
//!
//! Thread-safe, non-blocking public API: every command is queued for an internal worker thread
//! and returns at once. The listener receives [`PlayerEvent`]s from that thread:
//!
//! ```text
//! State(snapshot)                           on any change of state, track or queue
//! Position { position, buffered }           throttled (~4 Hz) while playing
//! Progress { phase, fraction, catalog_id }  while a song loads
//! Error { message, catalog_id, fatal }
//! Queue { keys, index, ids, autoplay }
//! ```
//!
//! Per track: acquire (a cache hit is instant) -> render (start; wait for the start buffer
//! delivered at a healthy rate, or completion) -> `output.set_source` -> "playing". The next
//! queue item is prefetched (acquired and rendered) while the current one plays. A song without
//! a Dolby Atmos version plays its 2-channel version when the stereo fallback is on (default),
//! and is reported and skipped when it is off. `snapshot.format` and `format_detail` always
//! describe what is audible.
//!
//! How it is put together (details in `research/engine-playback.md`):
//!
//! * One worker thread ("am-ui-player") consumes a command queue; every public method queues a
//!   command and returns. `snapshot()` and `queue()` read under a lock.
//! * Loading a track is a cancellable job thread (acquire -> PCM source); a newer command
//!   cancels it. Jobs post their result back to the worker, which owns all decisions. Prefetch
//!   and mode or profile switches are jobs too.
//! * A [`Kind`] names the PCM form of a track: atmos (16-channel Cavern render), stereo (AAC
//!   decode), lossless (ALAC decode through the lossless route) or binaural (2-channel headphone
//!   render, when the pipeline provides one). Mode, downmix profile and the lossless setting
//!   select the kind; ready sources are kept per (song, kind) so switching back is instant. With
//!   Dolby Atmos off (or a song without an Atmos version) the 2-channel kind is lossless when the
//!   setting is on and the route is signed in, else stereo; a lossless failure falls back to the
//!   AAC decode with a notice and is remembered for that song.
//! * One Cavern process at a time (the render gate): the current track's render runs at once
//!   and freezes a prefetch render until it is done; prefetch renders wait for a free gate.
//!   Adaptive start: a growing render is handed to the output once the start buffer exists and
//!   the last 0.5 s came in at 3 times real time or more (or it is done, or 6 s have passed).
//! * The output's "source ended" drives track transitions; when the output already continued
//!   gaplessly into the prefetched source (its count of handoffs grew), only the state changes.
//!   After each transition the PCM cache is evicted with {previous, current, next, everything
//!   prepared, armed or still rendering} protected; the entries this process uses carry in-use
//!   markers so another process never claims or evicts them.
//! * Sound Check: every prepared source gets its integrated loudness (EBU R128 on the PEQdB
//!   stereo fold) from a background thread that feeds the PCM into the stream meter while it is
//!   produced: a first estimate from what the start buffer holds, the final value the moment the
//!   file is complete (stored next to the PCM). A start waits up to 1 s for that estimate (0.6 s
//!   for the full measurement of a complete decode) so the track begins at its trim; later
//!   refinements ramp over 1.5 s. With Sound Check on (default) the output applies the
//!   attenuation toward -16 LUFS, never a boost.
//! * Diagnostics go to the playback log; `dump_recent()` saves the last 30 s of the written
//!   stream as a WAV.
//! * Settings (mode, profile, volume, fallback, repeat, shuffle, sound_check,
//!   lossless, autoplay) persist in `<state folder>/player.json` (0600) and are restored at
//!   construction. No program is started at construction: the output starts on the first play,
//!   the lossless service on the first lossless song (and is stopped at shutdown when this
//!   engine started it).
//!
//! Deliberate differences from `amui/player.py`:
//!
//! * A player is built from its dependencies: `Player::new(config, pipeline, log, listener,
//!   options)`. The pipeline is not optional (the engine's is [`EnginePipeline`], made of the
//!   acquirer, the lossless route and the downmix sink the application holds), the playback log
//!   is the one the output writes to, and Python's keyword arguments are the fields of
//!   [`PlayerOptions`]. Construction can fail (the data folders, the worker thread).
//! * Events are [`PlayerEvent`] values; `name()` and `to_json()` give the event name and the
//!   payload the Python listener gets. The snapshot is [`PlaybackSnapshot`], which has the fields
//!   of Python's subclass (buffering, underruns, sound_check, track_gain_db,
//!   lossless, lossless_available), and three that are not part of its `to_json()` (autoplay,
//!   station, playing_catalog_id).
//! * The seams are traits: [`Pipeline`] (Python's class of that name), [`Output`] and
//!   [`PcmStore`] for the objects the Python tests replaced, [`Source`] for what a pipeline
//!   hands out (a `PcmSource` plus the controls of a running render) and [`LoudnessStream`] for
//!   the stream meter. The numbers the Python tests patched, and the two waits of a start for
//!   its loudness, are in [`Tuning`]. Errors are typed, so `Pipeline.is_no_atmos()` and
//!   `is_lossless_error()` are methods of the error.
//! * Arguments are typed: [`RepeatMode`], [`AudioMode`], [`Kind`]; positions are `usize` (Python
//!   took a negative index for the first entry), an unknown repeat or mode cannot be asked for.
//! * Queue entries have ids: `queue()` returns [`QueueEntry`]s, and an entry is told apart by
//!   its id where Python went by the identity of the `Track` object (which made one item of a
//!   song queued twice from the same object). The snapshot's track is the queue entry's own.
//! * A snapshot never shows the new queue position with the song that played before: the song
//!   leaves the snapshot in the step that moves the position (the state is "loading" until the
//!   next one is on the output, also for the moment a prepared one takes), and a gapless
//!   continuation changes position, song and state in one step. A swapped variant (mode,
//!   profile, lossless) is on the output by the time the snapshot shows it. In Python a reader
//!   on another thread could catch the states in between.
//! * `shutdown()` waits for the worker, which cancels the jobs, stops the renders, removes the
//!   in-use markers and stops the output itself; only when it does not end within 5 s does the
//!   caller do that in its place. Dropping the player shuts it down.
//! * A panic inside a command is reported as "<command> failed: <text>" (Python: with the
//!   exception's class in front), and one inside a job as that job's failure.
//! * Entries that cannot be played are passed over in a loop (Python recursed once per entry).
//! * One preparation per PCM cache entry at a time. With repeat one, a change of mode made
//!   Python start two jobs for the same song and kind (its prefetch and the switch), and the
//!   second claimed the directory the first was writing into. Here the running prefetch becomes
//!   the switch, as it becomes the load when the user skips to its song; and should two jobs
//!   ever want the same entry, the second waits for the first and takes its source.
//! * The render gate keeps a job's turn until its render has started. In Python the worker's
//!   regular look at the gate could fall between the two and resume the render that had just
//!   been frozen for the job; it then ran next to the new one, unknown to the gate.
//! * The thread that measures a source writes the final value into it itself and queues its
//!   report before it lets a waiting start go on, so a start that waited for the measurement
//!   has it (Python logged "no value ...; starting at 0 dB" for a short complete file whose
//!   value was already there) and the worker hears of a value before it starts the track.
//! * A source that is prepared again is measured again: the measurement registered for an
//!   earlier source of the same cache entry is cancelled. In Python a measurement that never
//!   reported (its render had been stopped) kept the new source from
//!   being measured, so it played without its Sound Check trim.
//! * The sources outside {previous, current, next} are taken out in one step after a transition;
//!   in Python a source that a job registered during that step was lost to the bookkeeping.
//! * The engine has no binaural renderer (as in Python, where the functions the pipeline looked
//!   for did not exist): [`EnginePipeline`] offers the matrix profiles only, and asking it for a
//!   binaural render is an error with a plain message.
//! * [`parse_song_ref`] is the helper of Python's command line (`cli_play.py`), which has no
//!   counterpart in this crate. A song id is ASCII digits.
//! * A song without a Dolby Atmos version falls back quietly. Python reported "<song> has no
//!   Dolby Atmos version; playing the stereo version" each time such a song was loaded in the
//!   Atmos mode. Here nothing is reported for that: the snapshot's format says what is audible.
//!   A notice ("...; staying with the stereo version") comes only when the user switches Dolby
//!   Atmos on while such a song is the current one, since the switch cannot take effect for it
//!   (also when the song was already known to have no Atmos version, where Python said nothing),
//!   and "...; skipped" when the fallback is off.
//!
//! What this player has that the Python one had not:
//!
//! * Queue entries are told apart by their id (never used again within a player), and carry
//!   whether Autoplay added them and what they were played from ([`Container`]). `queue()` and
//!   the "queue" event list ids and flags next to Python's keys and index. `jump_to()`,
//!   `remove_entry()` and `move_entry()` name an entry by its id; `play_from()` is `play()` with
//!   the container of the songs. Moving entries does not change the order an unshuffled queue
//!   goes back to.
//! * The network calls below go through the [`Radio`] seam ([`PlayerOptions::radio`]; the
//!   engine's `MusicApi` is one). They block, so they run on job threads, never on the worker,
//!   and an answer that arrives for a queue that has changed since (replaced or emptied, Autoplay
//!   off, repeat on) is dropped. Without a radio these features are quietly off.
//! * Autoplay, as Apple's web player does it (`set_autoplay()`, default on, persisted; in
//!   `player.json` only while it is off, so that with its default the file is the Python
//!   engine's). It applies while it is on, repeat is off and the queue is not a station's.
//!   After the queue was replaced and after every change of the queue position: with more than
//!   [`AUTOPLAY_USER_LIMIT`] unplayed entries of the user's nothing happens; otherwise a station
//!   is made from the last ten entries of the user's (each with its container; the user's order,
//!   also while shuffled) unless the queue has one, and [`AUTOPLAY_AHEAD`] minus Autoplay's
//!   unplayed entries (plus one when the current entry is Autoplay's) of its songs are asked for
//!   and appended as Autoplay's entries, which play, prefetch and hand over like any other.
//!   Replacing the queue drops the station and starts over. "Add to queue" goes before the first
//!   unplayed entry of Autoplay, "play next" right after the current entry; shuffle keeps
//!   Autoplay's entries in their order after the user's; switching Autoplay off or repeat on
//!   takes its unplayed entries out, switching it back on asks again. A failure is written to
//!   the playback log, never shown, and tried again at the next change of the position.
//! * Stations (`play_station()`): a station of songs replaces the queue once its first songs
//!   are there, and [`STATION_MORE`] more are asked for whenever fewer than [`STATION_LOW`]
//!   unplayed entries are left (once for each end of the queue: every request moves the station
//!   on). The snapshot names the station while its queue plays; shuffle and repeat have no effect
//!   on it and edits of it are refused with a notice ([`STATION_QUEUE_FIXED`]). A live broadcast
//!   is refused ([`LIVE_RADIO`]), and a station without songs right now is reported; the queue
//!   is left alone in both cases.
//! * A library song whose catalog version was withdrawn (Apple refuses it: "Not Entitled", no
//!   sign-in problem) plays the catalog's current version of the song in its place; the entry
//!   keeps its own track and key, and the snapshot's `playing_catalog_id` names the id that
//!   plays. The version found is remembered in memory and in [`EQUIVALENTS_FILE`] of the state
//!   folder (0600, with the time it was found; after [`EQUIVALENT_MAX_AGE_S`] it is looked up
//!   again), so the next play does not fail first. Without one (or without a radio) the song is
//!   reported as no longer available on Apple Music and passed over.
//! * What was playing comes back after a restart: with [`PlayerOptions::resume_path`] (the
//!   application's player; the command line's has none) the queue and where it stood are kept in
//!   the state folder (see `resume`), and a new player starts with them: the queue in its order
//!   with Autoplay's entries marked, shuffle and repeat as they were, the current song *paused*
//!   at its position, with nothing loaded and nothing on the output. Such a song is in the state
//!   "paused" without a current source (`State::restored()`): the snapshot shows it with the
//!   saved position, a seek moves that position, and a change of mode or quality has nothing to
//!   do yet. The first command that plays (toggle, resume, or next, previous and jump as for a
//!   paused song) loads it as usual, from that position. Until then the player asks nothing of
//!   the network; Autoplay and the station's songs follow when the queue plays.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::acquire::AcquireError;
use crate::cache::{CacheError, MediaCache, PcmCache};
use crate::config::{self, Config, SAMPLE_RATE};
use crate::downmix::Profile;
use crate::lossless::quality_label;
use crate::loudness::lock;
use crate::output::{OutputError, PlaybackLog};
use crate::render::{RenderError, os_error_name};
use crate::types::{AcquireResult, AudioFormat, AudioMode, PlaybackSnapshot, RepeatMode, Station, Track};
use crate::util::{self, Event};
use crate::web::WebError;

mod gate;
mod jobs;
mod queue;
mod radio;
mod resume;
mod seams;
mod shared;
mod worker;

pub use self::queue::{Container, QueueEntry};
use self::radio::Equivalents;
pub use self::radio::Radio;
use self::resume::ResumeFiles;
pub use self::seams::{
    EnginePipeline, ErrorCallback, LoudnessStream, Output, PcmStore, Pipeline, Source, SourceCallback,
};
use self::shared::{Command, Poster, Shared, State};
use self::worker::{Keeper, Worker};

/// Seconds between two position events.
pub const POSITION_INTERVAL_S: f64 = 0.25;
/// The render must deliver the start buffer within this (the renderer's own start included).
pub const RENDER_START_TIMEOUT_S: f64 = 180.0;
/// ... and have produced audio at this many times real time over the last window.
pub const RENDER_START_RATE: f64 = 3.0;
pub const RENDER_RATE_WINDOW_S: f64 = 0.5;
/// The shortest history that yields a rate (a render that starts fast is not held back).
pub const RENDER_RATE_MIN_SPAN_S: f64 = 0.1;
/// Start anyway after this long with the start buffer available.
pub const RENDER_START_MAX_WAIT_S: f64 = 6.0;
pub const RENDER_POLL_S: f64 = 0.1;
/// Audio fed to the meter per step (a complete file yields an estimate after the first slice).
pub const LOUDNESS_SLICE_S: f64 = 10.0;
/// A start waits at most this long for the first estimate.
pub const LOUDNESS_START_WAIT_S: f64 = 1.0;
/// A complete file: wait this long for the full measurement instead.
pub const LOUDNESS_FINAL_WAIT_S: f64 = 0.6;
pub const LOUDNESS_SLICE_FRAMES: u64 = (LOUDNESS_SLICE_S * SAMPLE_RATE as f64) as u64;
/// A running estimate is applied again when it moved this much.
pub const LOUDNESS_REFINE_DB: f64 = 0.3;
/// ... ramped this slowly while the track plays.
pub const LOUDNESS_REFINE_RAMP_S: f64 = 1.5;
/// A complete file that stays shorter than announced: finish with what was read.
pub const LOUDNESS_STALL_S: f64 = 3.0;
/// `previous()` restarts the track when it has played for longer than this.
pub const RESTART_THRESHOLD_S: f64 = 3.0;
pub const STATE_FILE: &str = "player.json";
/// (key, label) of the matrix profiles, for a pipeline that cannot ask the downmix module.
pub const FALLBACK_PROFILES: [(&str, &str); 3] =
    [("peqdb", "PEQdB"), ("apple", "Apple fold-down"), ("itu", "Default (ITU)")];
/// Apple's AAC encodings; a stream's peak bandwidth lies a little above its rate.
pub const AAC_RATES_KBPS: [i64; 3] = [256, 128, 64];
/// Autoplay keeps this many of its songs ahead of the position (see `Player::set_autoplay()`).
pub const AUTOPLAY_AHEAD: usize = 10;
/// Autoplay adds nothing while more entries than this that the user added are unplayed.
pub const AUTOPLAY_USER_LIMIT: usize = 50;
/// The queue of a station asks for this many more of its songs ...
pub const STATION_MORE: i64 = 5;
/// ... whenever fewer unplayed entries than this are left.
pub const STATION_LOW: usize = 2;
/// The catalog's current versions of withdrawn songs, in the state folder.
pub const EQUIVALENTS_FILE: &str = "equivalents.json";
/// A current version found longer ago than this (30 days) is looked up again.
pub const EQUIVALENT_MAX_AGE_S: f64 = 30.0 * 24.0 * 3600.0;
/// The queue kept between runs (with [`PlayerOptions::resume_path`], which names it), in the
/// state folder ...
pub const RESUME_FILE: &str = "resume.json";
/// ... and, next to it, the current entry and the position in it.
pub const RESUME_POSITION_FILE: &str = "resume-position.json";
/// The format of those two files; other versions are not read.
pub const RESUME_VERSION: u64 = 1;
/// Seconds between two savings of the position while a song plays.
pub const RESUME_INTERVAL_S: f64 = 5.0;
/// Entries kept of a longer queue: some before the current one ...
pub const RESUME_MAX_ENTRIES: usize = 5000;
/// ... at most this many, the rest after it.
pub const RESUME_ENTRIES_BEFORE: usize = 200;
/// The notice for an edit of the queue of a station.
pub const STATION_QUEUE_FIXED: &str = "The queue of a station cannot be changed";
/// The notice for a station without songs to ask for (a live broadcast).
pub const LIVE_RADIO: &str = "Live radio cannot be played yet";

/// [`FALLBACK_PROFILES`] as profiles (all of kind "matrix").
pub fn fallback_profiles() -> Vec<Profile> {
    let profile = |(key, label): (&str, &str)| Profile {
        key: key.to_string(),
        label: label.to_string(),
        kind: "matrix".to_string(),
        description: String::new(),
    };
    FALLBACK_PROFILES.into_iter().map(profile).collect()
}

/// The PCM form of a track: the kinds of the PCM cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// 16-channel Cavern render.
    Atmos,
    /// AAC decode.
    Stereo,
    /// ALAC decode.
    Lossless,
    /// 2-channel Cavern headphone render.
    Binaural,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Atmos => "atmos",
            Kind::Stereo => "stereo",
            Kind::Lossless => "lossless",
            Kind::Binaural => "binaural",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        [Kind::Atmos, Kind::Stereo, Kind::Lossless, Kind::Binaural].into_iter().find(|kind| kind.as_str() == text)
    }

    /// A decoded 2-channel file (the binaural kind is a render).
    pub fn two_channel(self) -> bool {
        matches!(self, Kind::Stereo | Kind::Lossless)
    }

    /// Made by the renderer from the Dolby Atmos variant.
    pub fn rendered(self) -> bool {
        !self.two_channel()
    }

    /// The variant that is acquired for this kind.
    pub fn variant(self) -> &'static str {
        if self.two_channel() { self.as_str() } else { "atmos" }
    }

    /// What is audible.
    pub fn format(self) -> AudioFormat {
        match self {
            Kind::Stereo => AudioFormat::Stereo,
            Kind::Lossless => AudioFormat::Lossless,
            Kind::Atmos | Kind::Binaural => AudioFormat::Atmos,
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the player could not do something; the messages are safe to show and log.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlayerError {
    /// A newer command made the work pointless; never shown.
    #[error("cancelled")]
    Cancelled,
    /// The song could not be acquired.
    #[error(transparent)]
    Acquire(#[from] AcquireError),
    /// Anything else (Python's `RuntimeError`, and what the render, the caches and the output
    /// report).
    #[error("{0}")]
    Other(String),
}

impl PlayerError {
    /// Apple advertises no Dolby Atmos variant for the song.
    pub fn is_no_atmos(&self) -> bool {
        matches!(self, PlayerError::Acquire(error) if error.is_no_atmos())
    }

    /// A failure of the lossless route for which the AAC variant is the right fallback.
    pub fn is_lossless(&self) -> bool {
        matches!(self, PlayerError::Acquire(error) if error.is_lossless())
    }

    /// Apple refuses the song to the account (a catalog version that was withdrawn); the
    /// sign-in is fine.
    pub fn is_not_entitled(&self) -> bool {
        matches!(self, PlayerError::Acquire(AcquireError::Web(WebError::NotEntitled(_))))
    }
}

impl From<RenderError> for PlayerError {
    fn from(error: RenderError) -> PlayerError {
        PlayerError::Other(error.to_string())
    }
}

impl From<CacheError> for PlayerError {
    fn from(error: CacheError) -> PlayerError {
        PlayerError::Other(error.to_string())
    }
}

/// What the listener is told.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// The state, the track or the queue changed.
    State(Box<PlaybackSnapshot>),
    /// Seconds into the track and seconds ready beyond that (about four a second while playing).
    Position { position: f64, buffered: f64 },
    /// A song is loading: the phase ("manifest", "license", "download", "decrypt", "decode",
    /// "render") and how far it is, when that can be measured.
    Progress { phase: String, fraction: Option<f64>, catalog_id: String },
    /// Something the user should read; `fatal` when nothing can play until it is dealt with.
    Error { message: String, catalog_id: Option<String>, fatal: bool },
    /// The queue, entry by entry: the keys of the tracks, the ids of the entries and whether
    /// Autoplay added them; and the position in it (-1 when it is empty).
    Queue { keys: Vec<String>, index: i64, ids: Vec<u64>, autoplay: Vec<bool> },
}

impl PlayerEvent {
    /// The event's name in the Python engine: "state", "position", "progress", "error", "queue".
    pub fn name(&self) -> &'static str {
        match self {
            PlayerEvent::State(_) => "state",
            PlayerEvent::Position { .. } => "position",
            PlayerEvent::Progress { .. } => "progress",
            PlayerEvent::Error { .. } => "error",
            PlayerEvent::Queue { .. } => "queue",
        }
    }

    /// The payload the Python listener gets with the event.
    pub fn to_json(&self) -> Value {
        match self {
            PlayerEvent::State(snapshot) => snapshot.to_json(),
            PlayerEvent::Position { position, buffered } => json!({"position": position, "buffered": buffered}),
            PlayerEvent::Progress { phase, fraction, catalog_id } => {
                json!({"phase": phase, "fraction": fraction, "catalog_id": catalog_id})
            }
            PlayerEvent::Error { message, catalog_id, fatal } => {
                json!({"message": message, "catalog_id": catalog_id, "fatal": fatal})
            }
            PlayerEvent::Queue { keys, index, ids, autoplay } => {
                json!({"keys": keys, "index": index, "ids": ids, "autoplay": autoplay})
            }
        }
    }
}

/// Timing of a start; `Default` is the engine's (tests change it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Seconds of audio a render must have delivered before it plays
    /// ([`config::MIN_BUFFER_SECONDS`]).
    pub min_buffer_seconds: f64,
    /// Seconds after which a render with its start buffer plays whatever its rate
    /// ([`RENDER_START_MAX_WAIT_S`]).
    pub render_start_max_wait_s: f64,
    /// Seconds a start waits at most for the first loudness estimate
    /// ([`LOUDNESS_START_WAIT_S`]).
    pub loudness_start_wait_s: f64,
    /// Seconds a start waits for the full measurement of a complete file before it makes do
    /// with the estimate ([`LOUDNESS_FINAL_WAIT_S`]).
    pub loudness_final_wait_s: f64,
    /// Seconds between two savings of the position while a song plays ([`RESUME_INTERVAL_S`]).
    pub resume_interval_s: f64,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            min_buffer_seconds: config::MIN_BUFFER_SECONDS,
            render_start_max_wait_s: RENDER_START_MAX_WAIT_S,
            loudness_start_wait_s: LOUDNESS_START_WAIT_S,
            loudness_final_wait_s: LOUDNESS_FINAL_WAIT_S,
            resume_interval_s: RESUME_INTERVAL_S,
        }
    }
}

/// Makes the output on the first play, given the volume.
pub type OutputFactory = Box<dyn Fn(f64) -> Result<Arc<dyn Output>, OutputError> + Send + Sync>;

/// How `Player::new()` builds the player; `Default` is the engine's player.
#[derive(Default)]
pub struct PlayerOptions {
    /// The cache of decrypted media; `None`: the engine's.
    pub media_cache: Option<Arc<MediaCache>>,
    /// The PCM cache; `None`: the engine's.
    pub pcm_cache: Option<Arc<dyn PcmStore>>,
    /// An output to use, started on the first play (tests).
    pub output: Option<Arc<dyn Output>>,
    /// What makes the output on the first play; `None`: the pipeline's `make_output()`.
    pub output_factory: Option<OutputFactory>,
    /// The settings file; `None`: [`STATE_FILE`] in the state folder.
    pub state_path: Option<PathBuf>,
    pub tuning: Tuning,
    /// The network calls of Autoplay, stations and withdrawn songs (the engine's: its
    /// `MusicApi`); `None`: Autoplay adds nothing, `play_station()` does nothing and a withdrawn
    /// song is reported as no longer available.
    pub radio: Option<Arc<dyn Radio>>,
    /// The file ([`RESUME_FILE`] in the state folder for the application's player) that keeps
    /// the queue and the position from one run to the next; [`RESUME_POSITION_FILE`] lies next to
    /// it. `None`: nothing is kept and nothing comes back.
    pub resume_path: Option<PathBuf>,
}

/// Sets the flag when the worker's thread ends, however it ends.
struct SetOnDrop(Arc<Event>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

/// The queue and the playback state machine; see the module documentation.
pub struct Player {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    worker_ended: Arc<Event>,
}

impl Player {
    /// A player with its settings restored and its worker running. `log` is the playback log
    /// (the one the pipeline's output writes to); `listener` is called from the worker thread.
    pub fn new(
        config: Arc<Config>,
        pipeline: Arc<dyn Pipeline>,
        log: Arc<PlaybackLog>,
        listener: impl Fn(PlayerEvent) + Send + Sync + 'static,
        options: PlayerOptions,
    ) -> Result<Player, PlayerError> {
        let failed = |what: &str, error: &std::io::Error| {
            PlayerError::Other(format!("could not {what} ({})", os_error_name(error)))
        };
        config.ensure_dirs().map_err(|error| failed("create the data folders", &error))?;
        let media_cache = options.media_cache.unwrap_or_else(|| Arc::new(MediaCache::new(config.clone())));
        let pcm_cache = options.pcm_cache.unwrap_or_else(|| Arc::new(PcmCache::new(&config)));
        let state_path = options.state_path.unwrap_or_else(|| config.state_dir.join(STATE_FILE));
        let mut state = State::new(options.output);
        if let Some(saved) = util::read_json_object(&state_path) {
            state.restore_settings(&saved);
        }
        let equivalents = Equivalents::load(config.state_dir.join(EQUIVALENTS_FILE));
        let resume = options.resume_path.map(ResumeFiles::new);
        let resumed = resume.as_ref().and_then(ResumeFiles::load);
        let (sender, commands) = mpsc::channel();
        let shared = Arc::new(Shared::new(
            config,
            pipeline,
            media_cache,
            pcm_cache,
            log,
            Box::new(listener),
            options.output_factory,
            state_path,
            options.tuning,
            Poster::new(sender),
            options.radio,
            equivalents,
            state,
        ));
        if let Some(resumed) = resumed {
            shared.restore(resumed);
        }
        let keeper = resume.map(|files| Keeper::new(files, &shared));
        let worker_ended = Arc::new(Event::new());
        let (worker, ended) = (Worker::new(shared.clone(), commands, keeper), SetOnDrop(worker_ended.clone()));
        let thread = thread::Builder::new().name("am-ui-player".to_string()).spawn(move || {
            let _ended = ended;
            worker.run();
        });
        let thread = thread.map_err(|error| failed("start the player's thread", &error))?;
        Ok(Player { shared, worker: Mutex::new(Some(thread)), worker_ended })
    }

    fn post(&self, command: Command) {
        self.shared.post(command);
    }

    // ------------------------------------------------------------------ queue

    /// Replace the queue and start at `index`.
    pub fn play(&self, tracks: Vec<Track>, index: usize) {
        self.post(Command::Play(tracks, index, None));
    }

    /// `play()` for the songs of an album, a playlist or a station: the entries remember what
    /// they were played from.
    pub fn play_from(&self, tracks: Vec<Track>, index: usize, container: Container) {
        self.post(Command::Play(tracks, index, Some(container)));
    }

    /// Play a station of songs: once its first songs are there (the queue plays on until then),
    /// they replace the queue, and more follow before it runs out. While the station's queue
    /// plays, the snapshot names the station, shuffle and repeat have no effect and the queue
    /// cannot be edited (the commands are refused with a notice; next, previous, jump, seek and
    /// pause work). A live broadcast is refused with a notice and the queue left alone. Needs
    /// [`PlayerOptions::radio`].
    pub fn play_station(&self, station: Station) {
        self.post(Command::PlayStation(station));
    }

    /// Add to the end of the queue: after what the user added, before Autoplay's songs.
    pub fn enqueue(&self, tracks: Vec<Track>) {
        self.post(Command::Enqueue(tracks));
    }

    /// Add right after the current entry.
    pub fn play_next(&self, tracks: Vec<Track>) {
        self.post(Command::PlayNext(tracks));
    }

    /// Play the entry at `index`.
    pub fn jump(&self, index: usize) {
        self.post(Command::Jump(index));
    }

    /// Play the entry with that id.
    pub fn jump_to(&self, id: u64) {
        self.post(Command::JumpTo(id));
    }

    /// Take the entry at `index` out of the queue; when it is the current one, the entry that
    /// takes its place starts.
    pub fn remove(&self, index: usize) {
        self.post(Command::Remove(index));
    }

    /// `remove()` for the entry with that id.
    pub fn remove_entry(&self, id: u64) {
        self.post(Command::RemoveEntry(id));
    }

    /// Move the entry with that id to `position` (the last place when that is beyond the end).
    /// The current entry stays current wherever it moves.
    pub fn move_entry(&self, id: u64, position: usize) {
        self.post(Command::MoveEntry(id, position));
    }

    /// Stop and empty the queue.
    pub fn clear(&self) {
        self.post(Command::Clear);
    }

    // ------------------------------------------------------------------ transport

    pub fn toggle(&self) {
        self.post(Command::Toggle);
    }

    pub fn pause(&self) {
        self.post(Command::Pause);
    }

    pub fn resume(&self) {
        self.post(Command::Resume);
    }

    /// Silence the output; the queue stays.
    pub fn stop(&self) {
        self.post(Command::Stop);
    }

    pub fn next(&self) {
        self.post(Command::Next);
    }

    /// Restart the track after 3 s, otherwise go back.
    pub fn previous(&self) {
        self.post(Command::Previous);
    }

    pub fn seek(&self, seconds: f64) {
        self.post(Command::Seek(seconds));
    }

    /// 0..1 linear software gain.
    pub fn set_volume(&self, volume: f64) {
        self.post(Command::SetVolume(volume));
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        self.post(Command::SetRepeat(mode));
    }

    pub fn set_shuffle(&self, enabled: bool) {
        self.post(Command::SetShuffle(enabled));
    }

    /// Autoplay: songs like the queue's follow where the user's queue ends (default on;
    /// persisted). It applies while repeat is off and the queue is not a station's, and needs
    /// [`PlayerOptions::radio`]. Switching it off takes Autoplay's unplayed songs out of the
    /// queue; switching it on asks for them again.
    pub fn set_autoplay(&self, enabled: bool) {
        self.post(Command::SetAutoplay(enabled));
    }

    // ------------------------------------------------------------------ sound

    /// Dolby Atmos on or off. Applies to the playing track on the fly: the other variant is
    /// acquired and decoded in the background and swapped in at the current position
    /// (`snapshot.switching` is true meanwhile), and to every following track.
    pub fn set_mode(&self, mode: AudioMode) {
        self.post(Command::SetMode(mode));
    }

    /// When a song has no Atmos variant: play its 2-channel variant (the snapshot shows the real
    /// format) or skip it. Default on. Never silent: `snapshot.format` always tells the truth.
    pub fn set_stereo_fallback(&self, enabled: bool) {
        self.post(Command::SetStereoFallback(enabled));
    }

    /// Loudness normalisation (attenuation only, toward -16 LUFS; default on). Persisted; the
    /// change ramps over 50 ms on the playing track.
    pub fn set_sound_check(&self, enabled: bool) {
        self.post(Command::SetSoundCheck(enabled));
    }

    /// Prefer the ALAC rendition over the AAC one whenever 2-channel audio plays: Dolby Atmos
    /// off, or a song without an Atmos version. Default on; it only takes effect while the
    /// lossless route is built and signed in (`snapshot.lossless_available`). Persisted; applies
    /// to the playing track on the fly like `set_mode()`.
    pub fn set_lossless(&self, enabled: bool) {
        self.post(Command::SetLossless(enabled));
    }

    /// The profiles the pipeline offers: the matrix presets of the live downmix and, when there
    /// is a renderer for it, a profile of kind "render".
    pub fn downmix_profiles(&self) -> Vec<Profile> {
        self.shared.pipeline.downmix_profiles().into_iter().filter(|profile| !profile.key.is_empty()).collect()
    }

    /// Matrix profiles switch the live filter chain; a render profile swaps the PCM source like
    /// `set_mode()` does.
    pub fn set_downmix_profile(&self, key: &str) {
        self.post(Command::SetDownmixProfile(key.to_string()));
    }

    /// Change the shared sink's preamp (dB, -60..20); the live matrix preset stays. Rapid calls
    /// coalesce: only the latest value is applied once the running change has finished.
    pub fn set_preamp(&self, db: f64) {
        self.post(Command::SetPreamp(db));
    }

    // ------------------------------------------------------------------ inspection

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.shared.snapshot()
    }

    pub fn queue(&self) -> Vec<QueueEntry> {
        self.shared.queue()
    }

    pub fn mode(&self) -> AudioMode {
        lock(&self.shared.state).mode
    }

    pub fn stereo_fallback(&self) -> bool {
        lock(&self.shared.state).stereo_fallback
    }

    pub fn lossless(&self) -> bool {
        lock(&self.shared.state).lossless
    }

    pub fn downmix_profile(&self) -> Option<String> {
        lock(&self.shared.state).profile.clone()
    }

    pub fn sound_check(&self) -> bool {
        lock(&self.shared.state).sound_check
    }

    pub fn autoplay(&self) -> bool {
        lock(&self.shared.state).autoplay
    }

    /// Save the last 30 s of what the output wrote (stereo fold) as a 48 kHz float WAV; returns
    /// its path. `None`: `recent-<time>.wav` in the log folder. An error before any output.
    pub fn dump_recent(&self, path: Option<&Path>) -> Result<PathBuf, PlayerError> {
        let Some(output) = self.shared.output() else {
            return Err(PlayerError::Other("nothing has been played yet".to_string()));
        };
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => self.shared.config.log_dir.join(format!("recent-{}.wav", local_stamp())),
        };
        output
            .dump_recent(&path)
            .map_err(|error| PlayerError::Other(format!("could not save the recent audio ({})", os_error_name(&error))))
    }

    /// End the worker, cancel what is in flight, stop the renders and the output, remove the
    /// in-use markers. The first call does it; later ones return at once.
    pub fn shutdown(&self) {
        if !self.shared.poster.shut_down() {
            return;
        }
        let worker = lock(&self.worker).take();
        if let Some(worker) = worker
            && worker.thread().id() != thread::current().id()
            && self.worker_ended.wait(Some(Duration::from_secs(5)))
        {
            let _ = worker.join();
        }
        // the worker has done this on its way out, unless it is held up
        self.shared.cleanup();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Local time as `%Y%m%d-%H%M%S`.
fn local_stamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = now.as_secs() as libc::time_t;
    // SAFETY: `tm` is plain data for which all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call, which keeps neither.
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

// --- the format line ---------------------------------------------------------------------

/// A value of the variant's description as text: text as it is, a number as it is written,
/// nothing for anything else.
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// A value of the variant's description as a whole number: a number (cut off towards zero) or
/// text holding one.
fn whole_of(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|float| float.trunc() as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// The digits a text ends in.
fn trailing_digits(text: &str) -> &str {
    let digits = text.bytes().rev().take_while(u8::is_ascii_digit).count();
    &text[text.len() - digits..]
}

/// "Dolby Atmos · E-AC-3 JOC 768 kbps · 15 objects", "AAC 256 kbps" or "Lossless · ALAC
/// 24-bit/48 kHz" from the variant an acquisition names (and, for a render, its summary).
pub fn format_detail_for(kind: Kind, acquired: Option<&AcquireResult>, summary: Option<&Map<String, Value>>) -> String {
    let nothing = Map::new();
    let variant = acquired.map_or(&nothing, |acquired| &acquired.variant);
    let group = text_of(variant.get("audio_group"));
    let codecs = text_of(variant.get("codecs"));
    if kind == Kind::Lossless {
        let positive = |name: &str| whole_of(variant.get(name)).filter(|value| *value != 0);
        let (mut rate, mut depth) = (positive("sample_rate"), positive("bit_depth"));
        if rate.is_none() || depth.is_none() {
            // audio-alac-stereo-<sample rate>-<bit depth>
            const NAMED: &str = "audio-alac-stereo-";
            let named = group.rfind(NAMED).and_then(|at| group[at + NAMED.len()..].split_once('-'));
            let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
            if let Some((named_rate, named_depth)) = named.filter(|(rate, depth)| digits(rate) && digits(depth)) {
                (rate, depth) = (named_rate.parse().ok(), named_depth.parse().ok());
            }
        }
        let small = |value: Option<i64>| value.and_then(|value| u32::try_from(value).ok());
        return quality_label(small(rate), small(depth));
    }
    let digits = trailing_digits(&group);
    let kbps = if !digits.is_empty() {
        let shown =
            if group.starts_with("audio-atmos-") && digits.len() >= 4 { &digits[digits.len() - 3..] } else { digits };
        shown.parse::<i64>().ok()
    } else {
        // a stream of a master without audio groups: name its encoding rate, not its peak bandwidth
        let peak =
            whole_of(variant.get("bandwidth")).map(|bandwidth| (bandwidth as f64 / 1000.0).round_ties_even() as i64);
        match peak.filter(|kbps| *kbps != 0) {
            Some(kbps) if group.is_empty() && codecs.starts_with("mp4a") => {
                Some(AAC_RATES_KBPS.into_iter().find(|rate| *rate <= kbps).unwrap_or(kbps))
            }
            other => other,
        }
    };
    let rate = kbps.filter(|kbps| *kbps != 0).map(|kbps| format!(" {kbps} kbps")).unwrap_or_default();
    if kind.rendered() {
        let mut detail = format!("Dolby Atmos · E-AC-3 JOC{rate}");
        let objects = summary.and_then(|summary| whole_of(summary.get("joc_object_count_max")));
        if let Some(objects) = objects.filter(|objects| *objects != 0) {
            detail.push_str(&format!(" · {objects} objects"));
        }
        if kind == Kind::Binaural {
            detail.push_str(" · binaural");
        }
        return detail;
    }
    let codec = if codecs.starts_with("mp4a") || codecs.is_empty() { "AAC".to_string() } else { codecs.to_uppercase() };
    format!("{codec}{rate}")
}

// --- song references ---------------------------------------------------------------------

/// Percent-decoding of a query string's name or value (`+` is a space).
fn unquote_plus(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |index: usize| bytes.get(index).and_then(|byte| (*byte as char).to_digit(16));
        match (bytes[at], hex(at + 1), hex(at + 2)) {
            (b'%', Some(high), Some(low)) => {
                decoded.push((high * 16 + low) as u8);
                at += 3;
            }
            (b'+', _, _) => {
                decoded.push(b' ');
                at += 1;
            }
            (byte, _, _) => {
                decoded.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Catalog id from a bare id or a music.apple.com URL (`…/album/x/123?i=456`, `…/song/x/456`);
/// `None` for anything that names no song (an album or playlist URL, another site).
pub fn parse_song_ref(value: &str) -> Option<String> {
    let value = value.trim();
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if digits(value) {
        return Some(value.to_string());
    }
    let (scheme, rest) = value.split_once("://")?;
    let mut letters = scheme.chars();
    let named = letters.next().is_some_and(|first| first.is_ascii_alphabetic())
        && letters.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    if !named {
        return None;
    }
    let (rest, _fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (host, rest) = rest.split_at(end);
    if !host.ends_with("music.apple.com") {
        return None;
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    // …/album/<name>/<album id>?i=<song id>
    let song = query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (unquote_plus(name) == "i" && !value.is_empty()).then(|| unquote_plus(value))
    });
    if let Some(song) = song.filter(|song| digits(song)) {
        return Some(song);
    }
    // …/song/<name>/<song id>
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    let last = segments.last().copied().filter(|last| digits(last))?;
    segments.contains(&"song").then(|| last.to_string())
}
