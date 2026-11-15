//! The player as the window sees it: the last known state, the queue, the lyrics of the current
//! song, and signals for the widgets that show them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use amui::types::{Lyrics, PlayerState, Track};

use crate::data::Music;
use crate::playback::{Playback, PlayerEvent, PlayerView};
use crate::signal::{Signal, background};

pub struct Player {
    pub playback: Rc<dyn Playback>,
    music: Arc<dyn Music>,
    view: RefCell<PlayerView>,
    queue: RefCell<Vec<Track>>,
    /// Which entries of the queue Autoplay added.
    autoplay_flags: RefCell<Vec<bool>>,
    position: Cell<f64>,
    buffered: Cell<f64>,
    /// When `position` was reported.
    position_at: Cell<Instant>,
    lyrics: RefCell<Option<Lyrics>>,
    lyrics_for: RefCell<Option<String>>,
    lyrics_loading: Cell<bool>,
    /// State, song, format or a setting changed.
    pub changed: Signal<()>,
    /// The current song changed (or there is none any more).
    pub song_changed: Signal<()>,
    /// A position report arrived (about four per second; draw in between with `position()`).
    pub position_changed: Signal<()>,
    pub queue_changed: Signal<()>,
    pub lyrics_changed: Signal<()>,
    /// A problem to show.
    pub problem: Signal<String>,
}

impl Player {
    pub fn new(playback: Rc<dyn Playback>, music: Arc<dyn Music>) -> Rc<Player> {
        let player = Rc::new(Player {
            playback,
            music,
            view: RefCell::default(),
            queue: RefCell::default(),
            autoplay_flags: RefCell::default(),
            position: Cell::new(0.0),
            buffered: Cell::new(0.0),
            position_at: Cell::new(Instant::now()),
            lyrics: RefCell::default(),
            lyrics_for: RefCell::default(),
            lyrics_loading: Cell::new(false),
            changed: Signal::new(),
            song_changed: Signal::new(),
            position_changed: Signal::new(),
            queue_changed: Signal::new(),
            lyrics_changed: Signal::new(),
            problem: Signal::new(),
        });
        let weak = Rc::downgrade(&player);
        player.playback.connect(Box::new(move |event| {
            if let Some(player) = weak.upgrade() {
                player.handle(event);
            }
        }));
        player
    }

    fn handle(self: &Rc<Self>, event: PlayerEvent) {
        match event {
            PlayerEvent::View(view) => {
                let before = self.now_key();
                let was_playing = self.is_playing();
                self.view.replace(*view);
                if was_playing != self.is_playing() {
                    // the clock that extrapolates the position starts or stops here
                    self.position.set(self.position_at_change(was_playing));
                    self.position_at.set(Instant::now());
                }
                if before != self.now_key() {
                    self.position.set(0.0);
                    self.buffered.set(0.0);
                    self.position_at.set(Instant::now());
                    self.song_changed.emit(&());
                }
                // also when the song stays and what plays of it changes (its current version)
                self.load_lyrics();
                self.changed.emit(&());
            }
            PlayerEvent::Position { position, buffered } => {
                self.position.set(position);
                self.buffered.set(buffered);
                self.position_at.set(Instant::now());
                self.position_changed.emit(&());
            }
            PlayerEvent::Queue { tracks, index, autoplay } => {
                self.queue.replace(tracks);
                self.autoplay_flags.replace(autoplay);
                self.view.borrow_mut().index = index;
                self.queue_changed.emit(&());
            }
            PlayerEvent::Problem { message, .. } => self.problem.emit(&message),
        }
    }

    fn position_at_change(&self, was_playing: bool) -> f64 {
        if was_playing {
            self.position.get() + self.position_at.get().elapsed().as_secs_f64()
        } else {
            self.position.get()
        }
    }

    pub fn view(&self) -> PlayerView {
        self.view.borrow().clone()
    }

    pub fn track(&self) -> Option<Track> {
        self.view.borrow().track.clone()
    }

    /// The key of the current song.
    pub fn now_key(&self) -> Option<String> {
        self.view.borrow().track.as_ref().map(Track::key)
    }

    pub fn is_playing(&self) -> bool {
        self.view.borrow().state == PlayerState::Playing
    }

    /// Seconds into the song right now (between reports the clock runs on while playing).
    pub fn position(&self) -> f64 {
        let view = self.view.borrow();
        let mut position = self.position.get();
        if view.state == PlayerState::Playing && !view.buffering {
            position += self.position_at.get().elapsed().as_secs_f64();
        }
        match view.duration {
            Some(duration) => position.min(duration),
            None => position,
        }
    }

    pub fn buffered(&self) -> f64 {
        self.buffered.get()
    }

    pub fn duration(&self) -> Option<f64> {
        let view = self.view.borrow();
        view.duration.or_else(|| view.track.as_ref().and_then(|track| track.duration_ms).map(|ms| ms as f64 / 1000.0))
    }

    pub fn queue_index(&self) -> i64 {
        self.view.borrow().index
    }

    /// For each song after the current one, whether Autoplay added it.
    pub fn upcoming_autoplay(&self) -> Vec<bool> {
        let flags = self.autoplay_flags.borrow();
        let next = (self.queue_index() + 1).max(0) as usize;
        let count = self.queue.borrow().len().saturating_sub(next);
        (next..next + count).map(|at| flags.get(at).copied().unwrap_or(false)).collect()
    }

    /// The songs after the current one.
    pub fn upcoming(&self) -> Vec<Track> {
        let queue = self.queue.borrow();
        let next = (self.queue_index() + 1).max(0) as usize;
        queue.get(next..).map(<[Track]>::to_vec).unwrap_or_default()
    }

    pub fn lyrics(&self) -> Option<Lyrics> {
        self.lyrics.borrow().clone()
    }

    pub fn lyrics_loading(&self) -> bool {
        self.lyrics_loading.get()
    }

    fn load_lyrics(self: &Rc<Self>) {
        let playing = self.view.borrow().playing_catalog_id.clone();
        let wanted = playing.or_else(|| self.track().and_then(|track| track.catalog_id));
        if *self.lyrics_for.borrow() == wanted {
            return;
        }
        self.lyrics_for.replace(wanted.clone());
        self.lyrics.replace(None);
        self.lyrics_loading.set(wanted.is_some());
        self.lyrics_changed.emit(&());
        let Some(id) = wanted else { return };
        let (music, player, asked) = (self.music.clone(), self.clone(), id.clone());
        background(
            move || music.lyrics(&id).ok().flatten(),
            move |lyrics| {
                // a newer song may have replaced the one these lyrics are for
                if player.lyrics_for.borrow().as_deref() == Some(asked.as_str()) {
                    player.lyrics.replace(lyrics);
                    player.lyrics_loading.set(false);
                    player.lyrics_changed.emit(&());
                }
            },
        );
    }
}
