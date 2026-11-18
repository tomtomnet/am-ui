//! The queue: its entries and its orders (as played, shuffled, and the one to go back to).

use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::shared::State;
use crate::types::{RepeatMode, Track};

/// What a song was played from: an album, a playlist or a station.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// The type of the resource: "albums", "library-albums", "playlists", "library-playlists",
    /// "stations".
    pub kind: String,
    pub id: String,
}

impl Container {
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Container {
        Container { kind: kind.into(), id: id.into() }
    }

    pub fn to_json(&self) -> Value {
        json!({"kind": self.kind, "id": self.id})
    }
}

/// One entry of the queue. The same song may be queued more than once; the id tells the
/// entries apart and is never used again within a player.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueEntry {
    pub id: u64,
    pub track: Track,
    /// Added by Autoplay, not by the user.
    pub autoplay: bool,
    /// What the song was played from, when that is known.
    pub container: Option<Container>,
}

impl QueueEntry {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "track": self.track.to_json(),
            "autoplay": self.autoplay,
            "container": self.container.as_ref().map(Container::to_json),
        })
    }
}

/// The order of a shuffled queue: a small generator (SplitMix64) seeded from the hasher's random
/// keys and the clock.
pub(super) struct Shuffler(u64);

impl Shuffler {
    pub fn new() -> Shuffler {
        let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let clock = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos() as u64);
        Shuffler(random ^ clock)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    /// Put `items` into a random order (Fisher-Yates).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for last in (1..items.len()).rev() {
            let other = (self.next() % (last as u64 + 1)) as usize;
            items.swap(last, other);
        }
    }
}

/// Where an entry the user adds to the end goes in `order`: before the first entry of Autoplay
/// that follows the current entry (the one with id `current`), else at the end.
pub(super) fn before_autoplay(order: &[Arc<QueueEntry>], current: Option<u64>) -> usize {
    let start = current.and_then(|id| order.iter().position(|entry| entry.id == id)).map_or(0, |at| at + 1);
    order[start..].iter().position(|entry| entry.autoplay).map_or(order.len(), |offset| start + offset)
}

impl State {
    /// The position that follows the current one: after the end of a song (`auto`) or when the
    /// user asks for the next one. `None` at the end of the queue.
    pub fn next_index(&self, auto: bool) -> Option<i64> {
        let count = self.queue.len() as i64;
        if count == 0 || self.index < 0 {
            return None;
        }
        let repeat = self.repeat_mode();
        if auto && repeat == RepeatMode::One {
            return Some(self.index);
        }
        let index = self.index + 1;
        if index >= count {
            return (repeat == RepeatMode::All).then_some(0);
        }
        Some(index)
    }

    /// The entries after the current one; all of them when the queue has no position.
    pub fn unplayed(&self) -> &[Arc<QueueEntry>] {
        let start = usize::try_from(self.index + 1).unwrap_or(0).min(self.queue.len());
        &self.queue[start..]
    }

    /// Take Autoplay's entries after the current one out of the queue (and out of the order to
    /// go back to); how many there were.
    pub fn remove_unplayed_autoplay(&mut self) -> usize {
        let gone: Vec<u64> = self.unplayed().iter().filter(|entry| entry.autoplay).map(|entry| entry.id).collect();
        if gone.is_empty() {
            return 0;
        }
        // only entries after the position go: it stays where it is
        self.queue.retain(|entry| !gone.contains(&entry.id));
        if let Some(original) = self.original.as_mut() {
            original.retain(|entry| !gone.contains(&entry.id));
        }
        gone.len()
    }

    /// Shuffle the queue, the current entry first; remembers the original order. Autoplay's
    /// entries keep their order, after the user's.
    pub fn apply_shuffle_order(&mut self, keep_current: bool, shuffler: &mut Shuffler) {
        let current = self.entry_at_index().cloned();
        if self.original.is_none() {
            self.original = Some(self.queue.clone());
        }
        let position = self.index;
        let rest = std::mem::take(&mut self.queue)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| *index as i64 != position || !keep_current)
            .map(|(_, entry)| entry);
        let (mut rest, autoplay): (Vec<Arc<QueueEntry>>, Vec<Arc<QueueEntry>>) =
            rest.partition(|entry| !entry.autoplay);
        shuffler.shuffle(&mut rest);
        rest.extend(autoplay);
        match current {
            Some(current) if keep_current => {
                rest.insert(0, current);
                self.index = 0;
            }
            _ => self.index = if rest.is_empty() { -1 } else { 0 },
        }
        self.queue = rest;
    }

    /// Back to the order the queue had before it was shuffled; the current entry stays current.
    pub fn restore_order(&mut self) {
        let Some(original) = self.original.take() else { return };
        let current = self.entry_at_index().map(|entry| entry.id);
        self.queue = original;
        let last = self.queue.len() as i64 - 1;
        self.index = match current {
            Some(id) => match self.queue.iter().position(|entry| entry.id == id) {
                Some(index) => index as i64,
                None => self.index.min(last),
            },
            None if self.queue.is_empty() => -1,
            None => self.index.max(0).min(last),
        };
    }
}
