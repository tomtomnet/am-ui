//! A player that plays nothing: for `am-ui --demo`. It walks through the loading phases, lets a
//! clock run for the position, moves through the queue and answers every setting, so the whole
//! user interface can be used and checked without an account or a sound.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use amui::types::{AudioFormat, AudioMode, PlayerState, RepeatMode, Station, Track};
use gtk::glib;

use crate::playback::{Listener, Playback, PlayerEvent, PlayerView, Profile};

const TICK_MS: u64 = 250;

/// A song the demonstration's Autoplay added.
fn is_autoplay(track: &Track) -> bool {
    track.catalog_id.as_deref().is_some_and(|id| id.starts_with("ap-"))
}

struct Inner {
    /// Counts Autoplay's rounds, so that its songs' ids differ from round to round.
    autoplay_round: Cell<usize>,
    listener: RefCell<Option<Listener>>,
    view: RefCell<PlayerView>,
    queue: RefCell<Vec<Track>>,
    position: Cell<f64>,
    /// Counts starts of a song: timers of an earlier start do nothing.
    generation: Cell<u64>,
    ticking: Cell<bool>,
    instant: bool,
}

#[derive(Clone)]
pub struct DemoPlayback {
    inner: Rc<Inner>,
}

impl DemoPlayback {
    pub fn new() -> DemoPlayback {
        let view = PlayerView {
            lossless_available: true,
            downmix_profile: Some("peqdb".into()),
            preamp: Some(-6.0),
            ..PlayerView::default()
        };
        DemoPlayback {
            inner: Rc::new(Inner {
                autoplay_round: Cell::new(0),
                listener: RefCell::new(None),
                view: RefCell::new(view),
                queue: RefCell::default(),
                position: Cell::new(0.0),
                generation: Cell::new(0),
                ticking: Cell::new(false),
                instant: std::env::var_os("AMUI_DEMO_INSTANT").is_some(),
            }),
        }
    }
}

impl Inner {
    fn emit(&self, event: PlayerEvent) {
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener(event);
        }
    }

    fn announce(&self) {
        let view = self.view.borrow().clone();
        self.emit(PlayerEvent::View(Box::new(view)));
    }

    fn announce_queue(&self) {
        let (tracks, index) = (self.queue.borrow().clone(), self.view.borrow().index);
        let autoplay = tracks.iter().map(is_autoplay).collect();
        self.emit(PlayerEvent::Queue { tracks, index, autoplay });
    }

    /// With Autoplay on, five songs "like" the queue's after them: copies of its last ones.
    fn add_autoplay(&self) {
        let current = self.view.borrow().index;
        let ahead = self.queue.borrow().iter().skip((current + 1).max(0) as usize).any(is_autoplay);
        if !self.view.borrow().autoplay || ahead || self.queue.borrow().is_empty() {
            return;
        }
        let round = self.autoplay_round.get() + 1;
        self.autoplay_round.set(round);
        let users: Vec<Track> = self.queue.borrow().iter().filter(|track| !is_autoplay(track)).cloned().collect();
        let songs = users.iter().rev().take(5).enumerate().map(|(n, track)| Track {
            catalog_id: Some(format!("ap-{round}-{n}-{}", track.catalog_id.as_deref().unwrap_or_default())),
            library_id: None,
            ..track.clone()
        });
        self.queue.borrow_mut().extend(songs.collect::<Vec<_>>());
    }

    /// Where songs added to the queue go: before the first of Autoplay's after the current one.
    fn end_of_users(&self) -> usize {
        let current = (self.view.borrow().index + 1).max(0) as usize;
        let queue = self.queue.borrow();
        queue.iter().skip(current).position(is_autoplay).map_or(queue.len(), |at| current + at)
    }

    fn change(&self, edit: impl FnOnce(&mut PlayerView)) {
        edit(&mut self.view.borrow_mut());
        self.announce();
    }

    fn format_of(&self, track: &Track) -> (AudioFormat, String) {
        let view = self.view.borrow();
        let has = |name: &str| track.audio_traits.iter().any(|t| t == name);
        if view.mode == AudioMode::Atmos && has("atmos") {
            (AudioFormat::Atmos, "Dolby Atmos · E-AC-3 JOC 768 kbps · 15 objects".into())
        } else if view.lossless && has("hi-res-lossless") {
            (AudioFormat::Lossless, "Hi-Res Lossless · ALAC 24-bit/96 kHz".into())
        } else if view.lossless && has("lossless") {
            (AudioFormat::Lossless, "Lossless · ALAC 24-bit/48 kHz".into())
        } else {
            (AudioFormat::Stereo, "AAC 256 kbps".into())
        }
    }

    /// Start the song at `index` of the queue: the loading phases, then the clock.
    fn start(self: &Rc<Self>, index: usize) {
        let Some(track) = self.queue.borrow().get(index).cloned() else { return self.stop() };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.position.set(0.0);
        self.change(|view| {
            view.state = PlayerState::Loading;
            view.track = Some(track.clone());
            view.index = index as i64;
            view.duration = track.duration_ms.map(|ms| ms as f64 / 1000.0);
            view.loading = "Preparing…".into();
            view.format = None;
            view.format_detail.clear();
            view.error = None;
        });
        self.announce_queue();
        self.emit(PlayerEvent::Position { position: 0.0, buffered: 0.0 });
        let phases = ["Licensing…", "Downloading 35 %", "Downloading 100 %", "Rendering…"];
        let step = if self.instant { 1 } else { 180 };
        for (n, phase) in phases.into_iter().enumerate() {
            let inner = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(step * (n as u64 + 1)), move || {
                if inner.generation.get() == generation {
                    inner.change(|view| view.loading = phase.to_string());
                }
            });
        }
        let inner = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(step * (phases.len() as u64 + 1)), move || {
            if inner.generation.get() != generation {
                return;
            }
            let (format, detail) = inner.format_of(&track);
            inner.change(|view| {
                view.state = PlayerState::Playing;
                view.loading.clear();
                view.format = Some(format);
                view.format_detail = detail;
            });
            inner.tick();
        });
    }

    fn tick(self: &Rc<Self>) {
        if self.ticking.replace(true) {
            return;
        }
        let inner = self.clone();
        glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
            let playing = inner.view.borrow().state == PlayerState::Playing;
            if !playing {
                inner.ticking.set(false);
                return glib::ControlFlow::Break;
            }
            let position = inner.position.get() + TICK_MS as f64 / 1000.0;
            let duration = inner.view.borrow().duration.unwrap_or(180.0);
            if position >= duration {
                inner.ticking.set(false);
                inner.advance(true);
                return glib::ControlFlow::Break;
            }
            inner.position.set(position);
            inner.emit(PlayerEvent::Position { position, buffered: (duration - position).min(30.0) });
            glib::ControlFlow::Continue
        });
    }

    /// The song after the current one; `ended` when the current one played to its end.
    fn advance(self: &Rc<Self>, ended: bool) {
        let (index, count) = (self.view.borrow().index, self.queue.borrow().len() as i64);
        let repeat = self.view.borrow().repeat;
        if ended && repeat == RepeatMode::One {
            return self.start(index.max(0) as usize);
        }
        if index + 1 < count {
            self.start((index + 1) as usize);
        } else if repeat == RepeatMode::All && count > 0 {
            self.start(0);
        } else if self.view.borrow().autoplay && count > 0 {
            // "more like this": the demonstration plays its queue again as if Apple had chosen it
            let more: Vec<Track> = self.queue.borrow().iter().rev().take(5).cloned().collect();
            self.queue.borrow_mut().extend(more);
            self.start((index + 1) as usize);
        } else {
            self.stop();
        }
    }

    fn stop(&self) {
        self.generation.set(self.generation.get() + 1);
        self.position.set(0.0);
        self.change(|view| {
            view.state = PlayerState::Idle;
            view.loading.clear();
            view.format = None;
            view.format_detail.clear();
        });
        self.emit(PlayerEvent::Position { position: 0.0, buffered: 0.0 });
    }
}

impl Playback for DemoPlayback {
    fn connect(&self, listener: Listener) {
        self.inner.listener.replace(Some(listener));
        self.inner.announce();
    }

    fn play(&self, tracks: Vec<Track>, index: usize) {
        let playable: Vec<Track> = tracks.into_iter().filter(Track::playable).collect();
        if playable.is_empty() {
            self.inner.emit(PlayerEvent::Problem { message: "This song is not available".into(), fatal: false });
            return;
        }
        let index = index.min(playable.len() - 1);
        self.inner.queue.replace(playable);
        self.inner.view.borrow_mut().index = index as i64;
        self.inner.add_autoplay();
        self.inner.start(index);
    }

    fn play_station(&self, station: Station) {
        if !station.has_tracks() {
            let message = "Live radio cannot be played yet".to_string();
            self.inner.emit(PlayerEvent::Problem { message, fatal: false });
            return;
        }
        // a station of the demonstration has eight made-up songs under its own artwork
        let song = |n: usize| Track {
            catalog_id: Some(format!("{}-{n}", station.id)),
            name: format!("{} {}", station.name.trim_end_matches(" Station"), ["One", "Two", "Three", "Four"][n % 4]),
            artist: ["Glass Orbit", "Static Meadow", "Quiet Satellites"][n % 3].to_string(),
            album: station.name.clone(),
            duration_ms: Some(150_000 + 17_000 * (n as i64 % 5)),
            artwork: station.artwork.clone(),
            ..Track::default()
        };
        self.inner.queue.replace((0..8).map(song).collect());
        self.inner.start(0);
    }

    fn play_next(&self, tracks: Vec<Track>) {
        let at = ((self.inner.view.borrow().index + 1).max(0) as usize).min(self.inner.queue.borrow().len());
        let idle = self.inner.queue.borrow().is_empty();
        self.inner.queue.borrow_mut().splice(at..at, tracks);
        if idle {
            self.inner.start(0);
        } else {
            self.inner.announce_queue();
        }
    }

    fn enqueue(&self, tracks: Vec<Track>) {
        let idle = self.inner.queue.borrow().is_empty();
        let at = self.inner.end_of_users();
        self.inner.queue.borrow_mut().splice(at..at, tracks);
        if idle {
            self.inner.start(0);
        } else {
            self.inner.announce_queue();
        }
    }

    fn jump(&self, index: usize) {
        self.inner.start(index);
    }

    fn remove(&self, index: usize) {
        let current = self.inner.view.borrow().index;
        if index >= self.inner.queue.borrow().len() {
            return;
        }
        self.inner.queue.borrow_mut().remove(index);
        if (index as i64) < current {
            self.inner.view.borrow_mut().index = current - 1;
            self.inner.announce_queue();
        } else if index as i64 == current {
            self.inner.start(index);
        } else {
            self.inner.announce_queue();
        }
    }

    fn reorder(&self, from: usize, to: usize) {
        let current = self.inner.view.borrow().index;
        let mut queue = self.inner.queue.borrow_mut();
        if from >= queue.len() || to >= queue.len() || from == to {
            return;
        }
        let playing = (current >= 0).then(|| queue[current as usize].key());
        let track = queue.remove(from);
        queue.insert(to, track);
        // the current song keeps playing wherever it ended up
        let index = match (from as i64, to as i64) {
            (f, t) if f == current => t,
            (f, t) if f < current && t >= current => current - 1,
            (f, t) if f > current && t <= current => current + 1,
            _ => current,
        };
        debug_assert!(playing.is_none() || queue.get(index as usize).map(Track::key) == playing);
        drop(queue);
        self.inner.view.borrow_mut().index = index;
        self.inner.announce_queue();
    }

    fn clear_upcoming(&self) {
        let keep = (self.inner.view.borrow().index + 1).max(0) as usize;
        self.inner.queue.borrow_mut().truncate(keep);
        self.inner.announce_queue();
    }

    fn toggle(&self) {
        let state = self.inner.view.borrow().state;
        match state {
            PlayerState::Playing => self.inner.change(|view| view.state = PlayerState::Paused),
            PlayerState::Paused => {
                self.inner.change(|view| view.state = PlayerState::Playing);
                self.inner.tick();
            }
            PlayerState::Idle | PlayerState::Error => {
                let index = self.inner.view.borrow().index.max(0) as usize;
                if !self.inner.queue.borrow().is_empty() {
                    self.inner.start(index);
                }
            }
            PlayerState::Loading => {}
        }
    }

    fn next(&self) {
        self.inner.advance(false);
    }

    fn previous(&self) {
        let index = self.inner.view.borrow().index;
        if self.inner.position.get() > 3.0 || index <= 0 {
            self.seek(0.0);
        } else {
            self.inner.start((index - 1) as usize);
        }
    }

    fn seek(&self, seconds: f64) {
        let duration = self.inner.view.borrow().duration.unwrap_or(0.0);
        let position = seconds.clamp(0.0, (duration - 0.5).max(0.0));
        self.inner.position.set(position);
        self.inner.emit(PlayerEvent::Position { position, buffered: (duration - position).min(30.0) });
    }

    fn set_volume(&self, volume: f64) {
        self.inner.change(|view| view.volume = volume.clamp(0.0, 1.0));
    }

    fn set_repeat(&self, repeat: RepeatMode) {
        self.inner.change(|view| view.repeat = repeat);
    }

    fn set_shuffle(&self, shuffle: bool) {
        self.inner.change(|view| view.shuffle = shuffle);
    }

    fn set_autoplay(&self, autoplay: bool) {
        self.inner.change(|view| view.autoplay = autoplay);
        if autoplay {
            self.inner.add_autoplay();
        } else {
            // Autoplay's songs that have not played go
            let next = (self.inner.view.borrow().index + 1).max(0) as usize;
            let mut at = 0;
            self.inner.queue.borrow_mut().retain(|track| {
                at += 1;
                at <= next || !is_autoplay(track)
            });
        }
        self.inner.announce_queue();
    }

    fn set_mode(&self, mode: AudioMode) {
        self.inner.change(|view| view.mode = mode);
        self.refresh_format();
    }

    fn set_lossless(&self, lossless: bool) {
        self.inner.change(|view| view.lossless = lossless);
        self.refresh_format();
    }

    fn set_sound_check(&self, sound_check: bool) {
        self.inner.change(|view| view.sound_check = sound_check);
    }

    fn set_stereo_fallback(&self, stereo_fallback: bool) {
        self.inner.change(|view| view.stereo_fallback = stereo_fallback);
    }

    fn downmix_profiles(&self) -> Vec<Profile> {
        [("peqdb", "PEQdB"), ("apple", "Apple fold-down"), ("itu", "Default (ITU)")]
            .iter()
            .map(|(key, label)| Profile { key: key.to_string(), label: label.to_string() })
            .collect()
    }

    fn set_downmix_profile(&self, key: &str) {
        self.inner.change(|view| view.downmix_profile = Some(key.to_string()));
    }

    fn set_preamp(&self, db: f64) {
        self.inner.change(|view| view.preamp = Some(db.clamp(-60.0, 20.0)));
    }

    fn shutdown(&self) {
        self.inner.stop();
    }
}

impl DemoPlayback {
    /// A change of mode applies to the song that is playing, after a moment of "switching".
    fn refresh_format(&self) {
        let playing = matches!(self.inner.view.borrow().state, PlayerState::Playing | PlayerState::Paused);
        let Some(track) = self.inner.view.borrow().track.clone().filter(|_| playing) else { return };
        self.inner.change(|view| view.switching = true);
        let inner = self.inner.clone();
        glib::timeout_add_local_once(Duration::from_millis(if inner.instant { 1 } else { 500 }), move || {
            let (format, detail) = inner.format_of(&track);
            inner.change(|view| {
                view.switching = false;
                view.format = Some(format);
                view.format_detail = detail;
            });
        });
    }
}
