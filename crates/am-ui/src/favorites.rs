//! Which songs are favourites, as far as the service has been asked.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use amui::types::Track;

use crate::data::Music;
use crate::signal::{Signal, background};

#[derive(Default)]
pub struct Favorites {
    favorite: RefCell<HashSet<String>>,
    asked: RefCell<HashSet<String>>,
    waiting: RefCell<Vec<String>>,
    scheduled: std::cell::Cell<bool>,
    /// The set changed: rows look their song up again.
    pub changed: Signal<()>,
    /// A change could not be saved: the message for the user.
    pub failed: Signal<String>,
}

impl Favorites {
    pub fn is_favorite(&self, track: &Track) -> bool {
        track.catalog_id.as_ref().is_some_and(|id| self.favorite.borrow().contains(id))
    }

    /// Ask the service about the songs it has not been asked about yet. Requests of many rows
    /// that appear together go out as one (100 songs per request).
    pub fn ensure(self: &Rc<Self>, music: &Arc<dyn Music>, tracks: &[Track]) {
        let mut added = false;
        {
            let mut asked = self.asked.borrow_mut();
            let mut waiting = self.waiting.borrow_mut();
            for id in tracks.iter().filter_map(|track| track.catalog_id.clone()) {
                if asked.insert(id.clone()) {
                    waiting.push(id);
                    added = true;
                }
            }
        }
        if !added || self.scheduled.replace(true) {
            return;
        }
        let (favorites, music) = (self.clone(), music.clone());
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
            favorites.scheduled.set(false);
            let ids: Vec<String> = favorites.waiting.borrow_mut().drain(..).collect();
            for chunk in ids.chunks(100) {
                favorites.ask(&music, chunk.to_vec());
            }
        });
    }

    /// Ask again about every song asked about so far (fresh data was wanted).
    pub fn refresh(self: &Rc<Self>, music: &Arc<dyn Music>) {
        let ids: Vec<String> = self.asked.borrow().iter().cloned().collect();
        for chunk in ids.chunks(100) {
            self.ask(music, chunk.to_vec());
        }
    }

    fn ask(self: &Rc<Self>, music: &Arc<dyn Music>, ids: Vec<String>) {
        let (music, favorites) = (music.clone(), self.clone());
        background(
            move || {
                let answer = music.favorites(&ids);
                (ids, answer)
            },
            move |(ids, answer)| match answer {
                Ok(answer) => {
                    let mut favorite = favorites.favorite.borrow_mut();
                    let mut changed = false;
                    for (id, is_favorite) in answer {
                        changed |= if is_favorite { favorite.insert(id) } else { favorite.remove(&id) };
                    }
                    drop(favorite);
                    if changed {
                        favorites.changed.emit(&());
                    }
                }
                // ask again next time these songs are shown
                Err(_) => ids.iter().for_each(|id| {
                    favorites.asked.borrow_mut().remove(id);
                }),
            },
        );
    }

    /// Change a song's favourite state: shown at once, taken back when the service refuses.
    pub fn toggle(self: &Rc<Self>, music: &Arc<dyn Music>, track: &Track) {
        let Some(id) = track.catalog_id.clone() else {
            self.failed.emit(&"This song cannot be marked as a favorite".to_string());
            return;
        };
        let now = !self.favorite.borrow().contains(&id);
        self.set(&id, now);
        let (music, track, favorites) = (music.clone(), track.clone(), self.clone());
        background(
            move || music.set_favorite(&track, now),
            move |result| {
                if let Err(problem) = result {
                    favorites.set(&id, !now);
                    favorites.failed.emit(&problem.message);
                }
            },
        );
    }

    fn set(&self, id: &str, favorite: bool) {
        self.asked.borrow_mut().insert(id.to_string());
        if favorite {
            self.favorite.borrow_mut().insert(id.to_string());
        } else {
            self.favorite.borrow_mut().remove(id);
        }
        self.changed.emit(&());
    }
}
