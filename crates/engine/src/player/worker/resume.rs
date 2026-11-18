//! Keeping the queue for the next run, the worker's part: after every command and at every tick
//! it looks at what changed and hands that to the writer (`player::resume`), which does the rest
//! off this thread. The queue's file is written when the entries or their order change, the
//! position's file when the current entry or the state changes, after a seek, every
//! `Tuning::resume_interval_s` while a song plays, and when the player ends. When nothing
//! changed, nothing is written.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Worker;
use crate::loudness::lock;
use crate::player::QueueEntry;
use crate::player::resume::{PositionSave, QueueSave, ResumeFiles, ResumeWriter, Saves};
use crate::player::shared::{RATE, Shared, State};
use crate::types::{PlayerState, Station};

/// How long a player that ends waits for the last write.
const LAST_WRITE: Duration = Duration::from_secs(2);
/// A position that differs by less than this (in frames) is the same place.
const SAME_PLACE: u64 = RATE as u64 / 4;

/// What is there to come back to: a queue with a current entry, not of a live broadcast.
fn resumable(st: &State) -> Option<&Arc<QueueEntry>> {
    st.entry_at_index().filter(|_| st.station.as_ref().is_none_or(Station::has_tracks))
}

/// Where the current entry stands: on the output, else where it is going to start.
fn frame_of(st: &State) -> u64 {
    match (&st.current, &st.output) {
        (Some(_), Some(output)) if st.active() => output.position_frames(),
        _ => st.pending_start_frame,
    }
}

/// What the queue's file holds. Entries never change and their ids are never used twice: the
/// ids tell whether the queue is still the same.
#[derive(Default)]
struct QueueMark {
    ids: Vec<u64>,
    original: Option<Vec<u64>>,
    station: Option<String>,
}

impl QueueMark {
    fn of(st: &State) -> QueueMark {
        let ids = |entries: &[Arc<QueueEntry>]| entries.iter().map(|entry| entry.id).collect::<Vec<u64>>();
        QueueMark {
            ids: ids(&st.queue),
            original: st.original.as_deref().map(ids),
            station: st.station.as_ref().map(|station| station.id.clone()),
        }
    }

    /// Whether the queue is still the one marked (no copy is made: this runs after every command).
    fn holds(&self, st: &State) -> bool {
        let same = |ids: &[u64], entries: &[Arc<QueueEntry>]| {
            ids.len() == entries.len() && ids.iter().zip(entries).all(|(id, entry)| *id == entry.id)
        };
        let same_original = match (&self.original, &st.original) {
            (None, None) => true,
            (Some(ids), Some(entries)) => same(ids, entries),
            _ => false,
        };
        same(&self.ids, &st.queue)
            && same_original
            && self.station.as_deref() == st.station.as_ref().map(|station| station.id.as_str())
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// What the position's file holds.
struct PositionMark {
    entry: u64,
    frame: u64,
    playing: bool,
}

/// What the worker knows about the two files.
pub(in crate::player) struct Keeper {
    writer: Arc<ResumeWriter>,
    queue: QueueMark,
    position: Option<PositionMark>,
    written_at: Instant,
    seeked: bool,
}

impl Keeper {
    /// The keeper of a player just made: a queue that came from the files needs no writing.
    pub(in crate::player) fn new(files: ResumeFiles, shared: &Shared) -> Keeper {
        let writer = ResumeWriter::new(files, shared.log.clone());
        let st = lock(&shared.state);
        let queue = if st.restored() { QueueMark::of(&st) } else { QueueMark::default() };
        let position = st.entry_at_index().filter(|_| st.restored()).map(|entry| PositionMark {
            entry: entry.id,
            frame: st.pending_start_frame,
            playing: false,
        });
        Keeper { writer, queue, position, written_at: Instant::now(), seeked: false }
    }

    /// What is due to be written now.
    fn due(&mut self, st: &State, check_queue: bool, closing: bool, interval: f64) -> Saves {
        let mut saves = Saves::default();
        let Some(entry) = resumable(st) else {
            // nothing to come back to: what is kept goes
            if !self.queue.is_empty() || self.position.is_some() {
                saves.remove = true;
                self.queue = QueueMark::default();
                self.position = None;
            }
            return saves;
        };
        if (check_queue || closing) && !self.queue.holds(st) {
            self.queue = QueueMark::of(st);
            let index = st.index as usize; // (`resumable` found an entry there)
            saves.queue = Some(QueueSave::of(&st.queue, st.original.as_ref(), index, st.station.clone()));
        }
        let frame = frame_of(st);
        let playing = st.state == PlayerState::Playing;
        let due = match &self.position {
            None => true,
            Some(saved) => {
                let moved = saved.frame.abs_diff(frame) > SAME_PLACE;
                saved.entry != entry.id
                    || saved.playing != playing
                    || self.seeked
                    || (closing && saved.frame != frame)
                    || (moved && !playing)
                    || (moved && self.written_at.elapsed().as_secs_f64() >= interval)
            }
        };
        if due {
            self.position = Some(PositionMark { entry: entry.id, frame, playing });
            self.written_at = Instant::now();
            self.seeked = false;
            saves.position =
                Some(PositionSave { entry: entry.id, key: entry.track.key(), seconds: frame as f64 / RATE });
        }
        saves
    }
}

impl Worker {
    /// Hand over what changed. `check_queue`: a command ran, the queue may be another one.
    pub(super) fn remember(&mut self, check_queue: bool, closing: bool) {
        let interval = self.shared.tuning.resume_interval_s;
        let Some(keeper) = self.keeper.as_mut() else { return };
        let saves = keeper.due(&lock(&self.shared.state), check_queue, closing, interval);
        if !saves.is_empty() {
            keeper.writer.submit(saves);
        }
    }

    /// The position was moved by hand: it is saved at once, also while a song plays.
    pub(super) fn remember_seek(&mut self) {
        if let Some(keeper) = self.keeper.as_mut() {
            keeper.seeked = true;
        }
    }

    /// The player ends: the last state goes to the files before it does.
    pub(super) fn remember_last(&mut self) {
        self.remember(true, true);
        if let Some(keeper) = &self.keeper {
            keeper.writer.flush(LAST_WRITE);
        }
    }
}
