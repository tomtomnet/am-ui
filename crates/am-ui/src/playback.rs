//! What the user interface asks of the player, independent of what plays: the engine's player
//! for the real application, a simulation for the demonstration.
//!
//! Commands return at once; what happened arrives later as events on the user interface thread.

use amui::types::{AudioFormat, AudioMode, PlayerState, RepeatMode, Station, Track};

/// A stereo downmix profile of the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub key: String,
    pub label: String,
}

/// The state of the player as the window shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerView {
    pub state: PlayerState,
    pub track: Option<Track>,
    /// Position in the queue, -1 when it is empty.
    pub index: i64,
    /// Seconds, when known.
    pub duration: Option<f64>,
    /// 0..1.
    pub volume: f64,
    /// What a loading song is waiting for ("Downloading 42 %"), empty when nothing loads.
    pub loading: String,
    pub error: Option<String>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Requested: Dolby Atmos on or off.
    pub mode: AudioMode,
    /// What is actually playing.
    pub format: Option<AudioFormat>,
    /// "Dolby Atmos · E-AC-3 JOC 768 kbps", "Lossless · ALAC 24-bit/48 kHz", "AAC 256 kbps".
    pub format_detail: String,
    pub downmix_profile: Option<String>,
    /// A change of mode or profile is being prepared in the middle of a song.
    pub switching: bool,
    /// The shared output's preamp in dB.
    pub preamp: Option<f64>,
    pub sound_check: bool,
    pub lossless: bool,
    /// Lossless can be chosen on this machine.
    pub lossless_available: bool,
    pub stereo_fallback: bool,
    /// The output ran out of audio and waits for more.
    pub buffering: bool,
    /// Keep playing similar music when the queue ends.
    pub autoplay: bool,
    /// The catalog id that actually plays, when it is not the song's own (the current version of
    /// a song whose own was withdrawn): what lyrics are asked for.
    pub playing_catalog_id: Option<String>,
    /// The station whose songs the queue holds.
    pub station: Option<Station>,
}

impl Default for PlayerView {
    fn default() -> Self {
        PlayerView {
            state: PlayerState::Idle,
            track: None,
            index: -1,
            duration: None,
            volume: 1.0,
            loading: String::new(),
            error: None,
            repeat: RepeatMode::Off,
            shuffle: false,
            mode: AudioMode::Atmos,
            format: None,
            format_detail: String::new(),
            downmix_profile: None,
            switching: false,
            preamp: None,
            sound_check: true,
            lossless: false,
            lossless_available: false,
            stereo_fallback: true,
            buffering: false,
            autoplay: true,
            playing_catalog_id: None,
            station: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// Anything but the position changed.
    View(Box<PlayerView>),
    /// Seconds into the song, and seconds ready beyond that.
    Position { position: f64, buffered: f64 },
    /// The queue, the position of the current song in it, and which entries Autoplay added.
    Queue { tracks: Vec<Track>, index: i64, autoplay: Vec<bool> },
    /// Something the user should read; `fatal` when playback stopped because of it.
    Problem { message: String, fatal: bool },
}

pub type Listener = Box<dyn Fn(PlayerEvent)>;

pub trait Playback {
    /// Where events go (user interface thread). Called once, before any command.
    fn connect(&self, listener: Listener);

    /// Replace the queue and start at `index`.
    fn play(&self, tracks: Vec<Track>, index: usize);
    /// Play a station: its songs take the queue's place and go on for as long as it has more.
    fn play_station(&self, station: Station);
    fn play_next(&self, tracks: Vec<Track>);
    fn enqueue(&self, tracks: Vec<Track>);
    fn jump(&self, index: usize);
    fn remove(&self, index: usize);
    /// Move a song of the queue to another place in it.
    fn reorder(&self, from: usize, to: usize);
    /// Drop everything after the current song.
    fn clear_upcoming(&self);

    fn toggle(&self);
    fn next(&self);
    /// Restart the song after its first seconds, otherwise go back one.
    fn previous(&self);
    fn seek(&self, seconds: f64);

    fn set_volume(&self, volume: f64);
    fn set_repeat(&self, repeat: RepeatMode);
    fn set_shuffle(&self, shuffle: bool);
    fn set_autoplay(&self, autoplay: bool);

    fn set_mode(&self, mode: AudioMode);
    fn set_lossless(&self, lossless: bool);
    fn set_sound_check(&self, sound_check: bool);
    fn set_stereo_fallback(&self, stereo_fallback: bool);
    fn downmix_profiles(&self) -> Vec<Profile>;
    fn set_downmix_profile(&self, key: &str);
    fn set_preamp(&self, db: f64);

    /// Stop and release the output (the application quits).
    fn shutdown(&self);
}
