//! Playback on the engine: the window's commands go to `amui::player::Player`, and what the
//! player tells (from its own thread) comes back to the window on the user interface thread.
//!
//! Nothing plays until a command asks for it: the player makes its output on the first song.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use amui::acquire::Acquirer;
use amui::api::MusicApi;
use amui::config::Config;
use amui::downmix::Downmix;
use amui::lossless::Lossless;
use amui::output::PlaybackLog;
use amui::player::{
    EnginePipeline, Player, PlayerError, PlayerEvent as EngineEvent, PlayerOptions, RESUME_FILE, Radio,
};
use amui::types::{AudioMode, AudioQuality, PlaybackSnapshot, PlayerState, RepeatMode, Station, Track};
use amui::web::Web;
use gtk::glib;

use crate::playback::{Listener, Playback, PlayerEvent, PlayerView, Profile};

/// What the loading of a song is waiting for, in words: "Downloading 42 %".
pub fn loading_text(phase: Option<&str>, fraction: Option<f64>) -> String {
    let Some(phase) = phase.filter(|phase| !phase.is_empty()) else { return String::new() };
    let label = match phase {
        "manifest" => "Preparing…".to_string(),
        "license" => "Licensing…".to_string(),
        "download" => "Downloading".to_string(),
        "decrypt" => "Decrypting…".to_string(),
        "decode" => "Decoding…".to_string(),
        "render" => "Rendering…".to_string(),
        "buffer" => "Buffering…".to_string(),
        other => {
            let mut letters = other.chars();
            letters
                .next()
                .map(|first| first.to_uppercase().chain(letters).collect::<String>() + "…")
                .unwrap_or_default()
        }
    };
    match fraction {
        Some(fraction) if matches!(phase, "download" | "render" | "decode") => {
            format!("{} {} %", label.trim_end_matches('…'), (fraction * 100.0).round() as i64)
        }
        _ => label,
    }
}

/// What the window shows of the player's state. `stereo_fallback` is a setting the snapshot
/// does not carry.
pub fn view_of(snapshot: &PlaybackSnapshot, stereo_fallback: bool) -> PlayerView {
    PlayerView {
        state: snapshot.state,
        track: snapshot.track.clone(),
        index: snapshot.index,
        duration: snapshot.duration,
        volume: snapshot.volume,
        // a song that plays may still be decoded or rendered ahead: only a song that waits says so
        loading: if snapshot.state == PlayerState::Loading {
            loading_text(snapshot.loading_phase.as_deref(), snapshot.loading_progress)
        } else {
            String::new()
        },
        error: snapshot.error.clone(),
        repeat: snapshot.repeat,
        shuffle: snapshot.shuffle,
        mode: snapshot.mode,
        format: snapshot.format,
        format_detail: snapshot.format_detail.clone(),
        downmix_profile: snapshot.downmix_profile.clone(),
        switching: snapshot.switching,
        preamp: snapshot.preamp,
        sound_check: snapshot.sound_check,
        quality: snapshot.quality,
        lossless_available: snapshot.lossless_available,
        lossless_problem: snapshot.lossless_problem.clone(),
        stereo_fallback,
        buffering: snapshot.buffering,
        autoplay: snapshot.autoplay,
        // the song's own id when nothing stands in for it
        playing_catalog_id: snapshot
            .playing_catalog_id
            .clone()
            .filter(|id| snapshot.track.as_ref().is_none_or(|track| track.catalog_id.as_ref() != Some(id))),
        station: snapshot.station.clone(),
    }
}

/// Whether the song of a job (`catalog_id`) is the one that `view` waits for.
fn waits_for(view: &PlayerView, catalog_id: &str) -> bool {
    view.state == PlayerState::Loading
        && (view.playing_catalog_id.as_deref() == Some(catalog_id)
            || view.track.as_ref().is_some_and(|track| track.catalog_id.as_deref().unwrap_or_default() == catalog_id))
}

struct Inner {
    player: Arc<Player>,
    listener: RefCell<Option<Listener>>,
    /// The last state, for the progress events that change only what loading waits for.
    view: RefCell<PlayerView>,
}

impl Inner {
    fn tell(&self, event: PlayerEvent) {
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener(event);
        }
    }

    fn handle(&self, event: EngineEvent) {
        match event {
            EngineEvent::State(snapshot) => {
                let view = view_of(&snapshot, self.player.stereo_fallback());
                self.view.replace(view.clone());
                self.tell(PlayerEvent::View(Box::new(view)));
            }
            EngineEvent::Position { position, buffered } => self.tell(PlayerEvent::Position { position, buffered }),
            EngineEvent::Progress { phase, fraction, catalog_id } => {
                let view = {
                    let mut view = self.view.borrow_mut();
                    // the player reports every job: the next song's prefetch and the rest of the
                    // decoding of a song that already plays are not what the song waits for
                    if !waits_for(&view, &catalog_id) {
                        return;
                    }
                    let loading = loading_text(Some(&phase), fraction);
                    if view.loading == loading {
                        return;
                    }
                    view.loading = loading;
                    view.clone()
                };
                self.tell(PlayerEvent::View(Box::new(view)));
            }
            EngineEvent::Error { message, fatal, .. } => self.tell(PlayerEvent::Problem { message, fatal }),
            EngineEvent::Queue { index, .. } => {
                let (tracks, autoplay) =
                    self.player.queue().into_iter().map(|entry| (entry.track, entry.autoplay)).unzip();
                self.tell(PlayerEvent::Queue { tracks, index, autoplay });
            }
        }
    }
}

pub struct EnginePlayback {
    inner: Rc<Inner>,
}

impl EnginePlayback {
    /// The engine's player on the engine's pipeline: songs are acquired through `web`, rendered
    /// or decoded, and played through the output into the downmix. `api` gives Autoplay and
    /// stations their songs and finds the current versions of withdrawn songs.
    /// `downmix` is the profiles' and the equalizer's: the engine's own (`Downmix::in_app`), whose
    /// values the output's chain runs.
    pub fn new(
        config: Arc<Config>,
        web: Arc<Web>,
        api: Arc<MusicApi>,
        downmix: Arc<Downmix>,
    ) -> Result<EnginePlayback, PlayerError> {
        let log = Arc::new(PlaybackLog::for_config(&config));
        let lossless = Arc::new(Lossless::new(config.clone(), web.clone()));
        let acquirer = Arc::new(Acquirer::new(config.clone(), web, lossless.clone()));
        let pipeline = Arc::new(EnginePipeline::new(config.clone(), acquirer, lossless, downmix, log.clone()));
        // the player tells from its own thread; the window hears it on its own
        let (sender, receiver) = async_channel::unbounded::<EngineEvent>();
        let listener = move |event: EngineEvent| {
            let _ = sender.send_blocking(event);
        };
        let radio: Arc<dyn Radio> = api;
        let options = PlayerOptions {
            radio: Some(radio),
            // the last song and its queue come back at the next start
            resume_path: Some(config.state_dir.join(RESUME_FILE)),
            ..PlayerOptions::default()
        };
        let player = Arc::new(Player::new(config, pipeline, log, listener, options)?);
        let snapshot = player.snapshot();
        let inner = Rc::new(Inner {
            view: RefCell::new(view_of(&snapshot, player.stereo_fallback())),
            player,
            listener: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.handle(event);
            }
        });
        Ok(EnginePlayback { inner })
    }

    fn player(&self) -> &Player {
        &self.inner.player
    }
}

impl Playback for EnginePlayback {
    fn connect(&self, listener: Listener) {
        self.inner.listener.replace(Some(listener));
        let view = self.inner.view.borrow().clone();
        self.inner.tell(PlayerEvent::View(Box::new(view)));
        // a queue that came back from the last run is there from the first moment, with the
        // position its song stopped at
        let (tracks, autoplay): (Vec<Track>, Vec<bool>) =
            self.player().queue().into_iter().map(|entry| (entry.track, entry.autoplay)).unzip();
        if !tracks.is_empty() {
            let snapshot = self.player().snapshot();
            self.inner.tell(PlayerEvent::Queue { tracks, index: snapshot.index, autoplay });
            self.inner.tell(PlayerEvent::Position { position: snapshot.position, buffered: 0.0 });
        }
    }

    fn play(&self, tracks: Vec<Track>, index: usize) {
        self.player().play(tracks, index);
    }

    fn play_station(&self, station: Station) {
        self.player().play_station(station);
    }

    fn play_next(&self, tracks: Vec<Track>) {
        self.player().play_next(tracks);
    }

    fn enqueue(&self, tracks: Vec<Track>) {
        self.player().enqueue(tracks);
    }

    fn jump(&self, index: usize) {
        self.player().jump(index);
    }

    fn remove(&self, index: usize) {
        self.player().remove(index);
    }

    fn reorder(&self, from: usize, to: usize) {
        // the player moves entries by their ids
        if let Some(entry) = self.player().queue().get(from) {
            self.player().move_entry(entry.id, to);
        }
    }

    fn clear_upcoming(&self) {
        // everything after the current song, from the end, so that the indexes stay valid
        let queue = self.player().queue();
        let current = self.inner.view.borrow().index;
        for entry in queue.iter().skip((current + 1).max(0) as usize).rev() {
            self.player().remove_entry(entry.id);
        }
    }

    fn toggle(&self) {
        self.player().toggle();
    }

    fn next(&self) {
        self.player().next();
    }

    fn previous(&self) {
        self.player().previous();
    }

    fn seek(&self, seconds: f64) {
        self.player().seek(seconds);
    }

    fn set_volume(&self, volume: f64) {
        self.player().set_volume(volume);
    }

    fn set_repeat(&self, repeat: RepeatMode) {
        self.player().set_repeat(repeat);
    }

    fn set_shuffle(&self, shuffle: bool) {
        self.player().set_shuffle(shuffle);
    }

    fn set_autoplay(&self, autoplay: bool) {
        self.player().set_autoplay(autoplay);
    }

    fn set_mode(&self, mode: AudioMode) {
        self.player().set_mode(mode);
    }

    fn set_quality(&self, quality: AudioQuality) {
        self.player().set_quality(quality);
    }

    fn set_sound_check(&self, sound_check: bool) {
        self.player().set_sound_check(sound_check);
    }

    fn set_stereo_fallback(&self, stereo_fallback: bool) {
        self.player().set_stereo_fallback(stereo_fallback);
    }

    fn downmix_profiles(&self) -> Vec<Profile> {
        self.player()
            .downmix_profiles()
            .into_iter()
            .map(|profile| Profile { key: profile.key, label: profile.label })
            .collect()
    }

    fn set_downmix_profile(&self, key: &str) {
        self.player().set_downmix_profile(key);
    }

    fn set_preamp(&self, db: f64) {
        self.player().set_preamp(db);
    }

    fn shutdown(&self) {
        self.player().shutdown();
    }
}

/// What plays when the engine's player could not be made: nothing, and every command that would
/// start a song says why.
pub struct Unavailable {
    reason: String,
    listener: RefCell<Option<Listener>>,
}

impl Unavailable {
    pub fn new(reason: String) -> Unavailable {
        Unavailable { reason, listener: RefCell::new(None) }
    }

    fn refuse(&self) {
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener(PlayerEvent::Problem {
                message: format!("Playback is not available: {}", self.reason),
                fatal: true,
            });
        }
    }
}

impl Playback for Unavailable {
    fn connect(&self, listener: Listener) {
        let problem = format!("Playback is not available: {}", self.reason);
        let view = PlayerView { lossless_problem: Some(problem), ..PlayerView::default() };
        listener(PlayerEvent::View(Box::new(view)));
        self.listener.replace(Some(listener));
    }

    fn play(&self, _: Vec<Track>, _: usize) {
        self.refuse();
    }

    fn play_station(&self, _: Station) {
        self.refuse();
    }

    fn play_next(&self, _: Vec<Track>) {
        self.refuse();
    }

    fn enqueue(&self, _: Vec<Track>) {
        self.refuse();
    }

    fn toggle(&self) {
        self.refuse();
    }

    fn jump(&self, _: usize) {}
    fn remove(&self, _: usize) {}
    fn reorder(&self, _: usize, _: usize) {}
    fn clear_upcoming(&self) {}
    fn next(&self) {}
    fn previous(&self) {}
    fn seek(&self, _: f64) {}
    fn set_volume(&self, _: f64) {}
    fn set_repeat(&self, _: RepeatMode) {}
    fn set_shuffle(&self, _: bool) {}
    fn set_autoplay(&self, _: bool) {}
    fn set_mode(&self, _: AudioMode) {}
    fn set_quality(&self, _: AudioQuality) {}
    fn set_sound_check(&self, _: bool) {}
    fn set_stereo_fallback(&self, _: bool) {}

    fn downmix_profiles(&self) -> Vec<Profile> {
        Vec::new()
    }

    fn set_downmix_profile(&self, _: &str) {}
    fn set_preamp(&self, _: f64) {}
    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use amui::types::{AudioFormat, PlayerState};

    #[test]
    fn a_player_that_could_not_be_made_says_why() {
        let told: Rc<RefCell<Option<String>>> = Rc::default();
        let heard = told.clone();
        Unavailable::new("no data folder".to_string()).connect(Box::new(move |event| {
            if let PlayerEvent::View(view) = event {
                heard.replace(view.lossless_problem.clone());
            }
        }));
        assert_eq!(told.borrow().as_deref(), Some("Playback is not available: no data folder"));
    }

    #[test]
    fn loading_says_what_a_song_waits_for() {
        assert_eq!(loading_text(None, None), "");
        assert_eq!(loading_text(Some(""), Some(0.5)), "");
        assert_eq!(loading_text(Some("license"), None), "Licensing…");
        assert_eq!(loading_text(Some("download"), Some(0.424)), "Downloading 42 %");
        assert_eq!(loading_text(Some("download"), None), "Downloading");
        assert_eq!(loading_text(Some("render"), Some(1.0)), "Rendering 100 %");
        assert_eq!(loading_text(Some("decrypt"), Some(0.5)), "Decrypting…", "a phase without a measure");
        assert_eq!(loading_text(Some("warming"), None), "Warming…");
    }

    #[test]
    fn the_view_is_the_snapshot_with_the_window_s_settings() {
        let snapshot = PlaybackSnapshot {
            state: PlayerState::Playing,
            track: Some(Track::new(Some("1000000001"), "Song A", "Artist A")),
            index: 2,
            duration: Some(201.0),
            volume: 0.7,
            loading_phase: Some("render".into()),
            loading_progress: Some(0.25),
            format: Some(AudioFormat::Atmos),
            format_detail: "Dolby Atmos".into(),
            preamp: Some(-9.0),
            ..PlaybackSnapshot::default()
        };
        let view = view_of(&snapshot, false);
        assert_eq!((view.state, view.index, view.duration, view.volume), (PlayerState::Playing, 2, Some(201.0), 0.7));
        assert_eq!(view.track.as_ref().map(|track| track.name.as_str()), Some("Song A"));
        // a song that plays is not loading, whatever is still rendered ahead
        assert_eq!(view.loading, "");
        let waiting = PlaybackSnapshot { state: PlayerState::Loading, ..snapshot.clone() };
        assert_eq!(view_of(&waiting, false).loading, "Rendering 25 %");
        assert_eq!(
            (view.format, view.format_detail.as_str(), view.preamp),
            (Some(AudioFormat::Atmos), "Dolby Atmos", Some(-9.0))
        );
        assert_eq!((view.stereo_fallback, view.autoplay), (false, true));
        assert_eq!((view.quality, view.lossless_problem), (AudioQuality::Lossless, None));
        let refused = PlaybackSnapshot {
            quality: AudioQuality::HiRes,
            lossless_problem: Some("No route".into()),
            ..PlaybackSnapshot::default()
        };
        let refused = view_of(&refused, true);
        assert_eq!((refused.quality, refused.lossless_problem.as_deref()), (AudioQuality::HiRes, Some("No route")));
        // the song's own id is no stand-in; another one is what lyrics are asked for
        assert_eq!(view.playing_catalog_id, None);
        let standing_in = PlaybackSnapshot { playing_catalog_id: Some("1000000001".into()), ..snapshot.clone() };
        assert_eq!(view_of(&standing_in, false).playing_catalog_id, None);
        let standing_in = PlaybackSnapshot { playing_catalog_id: Some("1000000009".into()), ..snapshot };
        assert_eq!(view_of(&standing_in, false).playing_catalog_id.as_deref(), Some("1000000009"));
    }

    #[test]
    fn only_the_song_that_waits_shows_its_progress() {
        let mut view = PlayerView {
            state: PlayerState::Loading,
            track: Some(Track::new(Some("1000000001"), "Song A", "Artist A")),
            ..PlayerView::default()
        };
        assert!(waits_for(&view, "1000000001"));
        assert!(!waits_for(&view, "1000000002"), "the next song's prefetch");
        view.playing_catalog_id = Some("1000000009".into());
        assert!(waits_for(&view, "1000000009"), "the current version of a withdrawn song");
        view.state = PlayerState::Playing;
        assert!(!waits_for(&view, "1000000001"), "the rest of the decoding of a song that plays");
    }
}
