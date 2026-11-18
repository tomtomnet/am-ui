//! What the player keeps between runs: the queue and where it stood, so that the application
//! comes back with the last song in the player bar, paused where it stopped, and with what was
//! to follow it. Nothing plays until the user asks for it.
//!
//! Two files, 0600 and replaced atomically like every state file:
//!
//! * [`RESUME_FILE`](super::RESUME_FILE) holds the queue: its entries with what showing and
//!   playing them needs (the tracks' metadata, whether Autoplay added them, what they were played
//!   from), the order to go back to while it is shuffled, and the station whose queue it is. It is
//!   written when any of that changes; a song that plays on does not touch it, however long the
//!   queue is. Of a queue longer than [`RESUME_MAX_ENTRIES`](super::RESUME_MAX_ENTRIES) the part
//!   around the current entry is kept.
//! * [`RESUME_POSITION_FILE`](super::RESUME_POSITION_FILE) holds the current entry and the
//!   position in it, the part that changes all the time: a few bytes, written when the current
//!   entry changes, on pause, stop and seek, every
//!   [`RESUME_INTERVAL_S`](super::RESUME_INTERVAL_S) while a song plays, and at shutdown.
//!
//! Entries keep their ids from run to run, which is how the second file names one in the first.
//! Files that are missing, damaged or of another format version count as not being there: the
//! player starts empty, or at the start of the song when only the position cannot be used.
//!
//! The worker decides what is due (`worker::resume`) and hands copies to [`ResumeWriter`], which
//! does the serializing and the writing on a thread of its own: nothing here runs on the worker
//! or on the audio path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::jobs::spawn;
use super::shared::RATE;
use super::{Container, QueueEntry, RESUME_ENTRIES_BEFORE, RESUME_MAX_ENTRIES, RESUME_POSITION_FILE, RESUME_VERSION};
use crate::loudness::lock;
use crate::output::PlaybackLog;
use crate::render::os_error_name;
use crate::types::{Station, Track};
use crate::util;

/// No player hands out ids above this: a file with one is damaged (and counting on from it would
/// overflow).
const HIGHEST_ENTRY_ID: u64 = u64::MAX / 2;
/// No song is this long: a position beyond it comes from a damaged file.
const LONGEST_POSITION_S: f64 = 7.0 * 24.0 * 3600.0;

/// A queue found in the files, ready to be put in place.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Resumed {
    pub entries: Vec<QueueEntry>,
    /// The ids of the entries in the order to go back to, for a queue that was shuffled.
    pub original: Option<Vec<u64>>,
    /// The id of the current entry.
    pub current: u64,
    /// Where it stood.
    pub start_frame: u64,
    pub station: Option<Station>,
}

/// The two files.
pub(super) struct ResumeFiles {
    queue: PathBuf,
    position: PathBuf,
}

impl ResumeFiles {
    /// The queue's file at `queue`, the position's file next to it.
    pub fn new(queue: PathBuf) -> ResumeFiles {
        let position = queue.with_file_name(RESUME_POSITION_FILE);
        ResumeFiles { queue, position }
    }

    /// What the files hold; `None` when there is nothing usable.
    pub fn load(&self) -> Option<Resumed> {
        let mut saved = read_versioned(&self.queue)?;
        let station = match saved.remove("station") {
            None | Some(Value::Null) => None,
            // a live broadcast is no queue of songs that could be gone back to
            Some(station) => Some(serde_json::from_value::<Station>(station).ok().filter(Station::has_tracks)?),
        };
        let Value::Array(items) = saved.remove("entries")? else { return None };
        let entries = items.into_iter().map(parse_entry).collect::<Option<Vec<QueueEntry>>>()?;
        let ids: HashSet<u64> = entries.iter().map(|entry| entry.id).collect();
        if entries.is_empty() || ids.len() != entries.len() {
            return None;
        }
        let original = match saved.remove("original") {
            None | Some(Value::Null) => None,
            Some(order) => Some(parse_order(&order, &ids)?),
        };
        let current = saved.get("current")?.as_u64().filter(|id| ids.contains(id))?;
        // the position names its entry; the queue's file only says which one was current when
        // the queue last changed
        let (current, seconds) = self.load_position(&entries).unwrap_or((current, 0.0));
        let track = &entries.iter().find(|entry| entry.id == current)?.track;
        Some(Resumed { start_frame: start_frame(track, seconds), entries, original, current, station })
    }

    /// The entry the position file names and the seconds into it, when it is one of `entries`.
    fn load_position(&self, entries: &[QueueEntry]) -> Option<(u64, f64)> {
        let saved = read_versioned(&self.position)?;
        let id = saved.get("entry")?.as_u64()?;
        let entry = entries.iter().find(|entry| entry.id == id)?;
        if saved.get("key")?.as_str()? != entry.track.key() {
            return None;
        }
        let seconds = saved.get("position")?.as_f64().filter(|seconds| (0.0..=LONGEST_POSITION_S).contains(seconds))?;
        Some((id, seconds))
    }

    fn remove(&self) -> io::Result<()> {
        let queue = remove_file(&self.queue);
        remove_file(&self.position)?;
        queue
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The JSON object in a file, when it is of the format this player writes.
fn read_versioned(path: &Path) -> Option<Map<String, Value>> {
    util::read_json_object(path).filter(|saved| saved.get("version").and_then(Value::as_u64) == Some(RESUME_VERSION))
}

fn parse_entry(value: Value) -> Option<QueueEntry> {
    let Value::Object(mut fields) = value else { return None };
    let id = fields.get("id")?.as_u64().filter(|id| *id <= HIGHEST_ENTRY_ID)?;
    let track = serde_json::from_value(fields.remove("track")?).ok()?;
    let autoplay = fields.get("autoplay").and_then(Value::as_bool)?;
    let container = match fields.get("container") {
        None | Some(Value::Null) => None,
        Some(container) => Some(Container::new(container.get("kind")?.as_str()?, container.get("id")?.as_str()?)),
    };
    Some(QueueEntry { id, track, autoplay, container })
}

/// An order of the queue: every entry once.
fn parse_order(value: &Value, ids: &HashSet<u64>) -> Option<Vec<u64>> {
    let order = value.as_array()?.iter().map(Value::as_u64).collect::<Option<Vec<u64>>>()?;
    let distinct: HashSet<u64> = order.iter().copied().collect();
    (order.len() == ids.len() && distinct == *ids).then_some(order)
}

/// The frame to start at for a position in seconds. A position at or past the song's end is no
/// place to come back to: the song starts over.
fn start_frame(track: &Track, seconds: f64) -> u64 {
    let ended = track.duration_ms.is_some_and(|ms| ms > 0 && seconds * 1000.0 >= ms as f64);
    if ended { 0 } else { (seconds * RATE).round() as u64 }
}

// --- what is written -----------------------------------------------------------------------

/// A copy of the queue for its file.
pub(super) struct QueueSave {
    pub entries: Vec<Arc<QueueEntry>>,
    pub original: Option<Vec<u64>>,
    pub current: u64,
    pub station: Option<Station>,
}

impl QueueSave {
    /// What is kept of `queue` with the entry at `index` current, in `original`'s order to go
    /// back to: everything, or for a very long queue the part around that entry.
    pub fn of(
        queue: &[Arc<QueueEntry>],
        original: Option<&Vec<Arc<QueueEntry>>>,
        index: usize,
        station: Option<Station>,
    ) -> QueueSave {
        let start = index.saturating_sub(RESUME_ENTRIES_BEFORE).min(queue.len().saturating_sub(RESUME_MAX_ENTRIES));
        let kept = &queue[start..queue.len().min(start + RESUME_MAX_ENTRIES)];
        let ids: HashSet<u64> = kept.iter().map(|entry| entry.id).collect();
        let original = original.map(|order| order.iter().map(|entry| entry.id).filter(|id| ids.contains(id)).collect());
        QueueSave { entries: kept.to_vec(), original, current: queue[index].id, station }
    }

    fn to_json(&self) -> Value {
        let entry = |entry: &Arc<QueueEntry>| {
            let mut track = serde_json::to_value(&entry.track).unwrap_or(Value::Null);
            if let Some(fields) = track.as_object_mut() {
                fields.retain(|_, field| !field.is_null()); // what is unknown comes back as unknown
            }
            json!({
                "id": entry.id,
                "track": track,
                "autoplay": entry.autoplay,
                "container": entry.container.as_ref().map(Container::to_json),
            })
        };
        json!({
            "version": RESUME_VERSION,
            "current": self.current,
            "entries": self.entries.iter().map(entry).collect::<Vec<Value>>(),
            "original": self.original,
            "station": self.station,
        })
    }
}

/// The current entry and the position in it, for their file.
pub(super) struct PositionSave {
    pub entry: u64,
    pub key: String,
    pub seconds: f64,
}

impl PositionSave {
    fn to_json(&self) -> Value {
        json!({"version": RESUME_VERSION, "entry": self.entry, "key": self.key, "position": self.seconds})
    }
}

/// What is to be written; later requests replace earlier ones that were not written yet.
#[derive(Default)]
pub(super) struct Saves {
    /// There is nothing to come back to any more: both files go.
    pub remove: bool,
    pub queue: Option<QueueSave>,
    pub position: Option<PositionSave>,
}

impl Saves {
    pub fn is_empty(&self) -> bool {
        !self.remove && self.queue.is_none() && self.position.is_none()
    }

    fn merge(&mut self, later: Saves) {
        if later.remove {
            *self = Saves { remove: true, ..Saves::default() };
        }
        if later.queue.is_some() {
            self.queue = later.queue;
            self.remove = false;
        }
        if later.position.is_some() {
            self.position = later.position;
            self.remove = false;
        }
    }
}

#[derive(Default)]
struct WriterState {
    pending: Saves,
    /// A thread is at work on them.
    running: bool,
}

/// Writes the two files on a thread of its own, which runs only while there is something to
/// write. What arrives while it is busy is merged: only the latest state reaches the disk.
pub(super) struct ResumeWriter {
    files: ResumeFiles,
    log: Arc<PlaybackLog>,
    state: Mutex<WriterState>,
    idle: Condvar,
}

impl ResumeWriter {
    pub fn new(files: ResumeFiles, log: Arc<PlaybackLog>) -> Arc<ResumeWriter> {
        Arc::new(ResumeWriter { files, log, state: Mutex::default(), idle: Condvar::new() })
    }

    pub fn submit(self: &Arc<Self>, saves: Saves) {
        let mut state = lock(&self.state);
        state.pending.merge(saves);
        if state.running || state.pending.is_empty() {
            return;
        }
        state.running = true;
        let writer = self.clone();
        if !spawn("am-ui-resume", move || writer.drain()) {
            // (what is pending goes with the next request)
            state.running = false;
        }
    }

    /// Wait until everything handed over is written, `timeout` at most.
    pub fn flush(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        while state.running {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            state = self.idle.wait_timeout(state, left).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
    }

    fn drain(&self) {
        loop {
            let saves = {
                let mut state = lock(&self.state);
                if state.pending.is_empty() {
                    state.running = false;
                    self.idle.notify_all();
                    return;
                }
                std::mem::take(&mut state.pending)
            };
            if let Err(error) = self.write(&saves) {
                // a full disk costs the position of this moment, nothing else
                self.log.write(&format!("resume: could not save ({})", os_error_name(&error)));
            }
        }
    }

    fn write(&self, saves: &Saves) -> io::Result<()> {
        if saves.remove {
            self.files.remove()?;
        }
        if let Some(queue) = &saves.queue {
            util::write_private_json(&self.files.queue, &queue.to_json())?;
        }
        if let Some(position) = &saves.position {
            util::write_private_json(&self.files.position, &position.to_json())?;
        }
        Ok(())
    }
}

/// Queue entries by their ids.
pub(super) fn by_id(entries: &[Arc<QueueEntry>]) -> HashMap<u64, Arc<QueueEntry>> {
    entries.iter().map(|entry| (entry.id, entry.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::RESUME_FILE;

    fn entry(id: u64, catalog_id: &str) -> Arc<QueueEntry> {
        let track = Track {
            duration_ms: Some(200_000),
            ..Track::new(Some(catalog_id), format!("Song {catalog_id}"), "Artist A")
        };
        Arc::new(QueueEntry { id, track, autoplay: id.is_multiple_of(2), container: None })
    }

    fn files(dir: &tempfile::TempDir) -> ResumeFiles {
        ResumeFiles::new(dir.path().join(RESUME_FILE))
    }

    fn write(files: &ResumeFiles, saves: Saves) {
        let writer = ResumeWriter::new(
            ResumeFiles::new(files.queue.clone()),
            Arc::new(PlaybackLog::new(files.queue.with_file_name("log"))),
        );
        writer.submit(saves);
        writer.flush(Duration::from_secs(30));
    }

    fn saved_queue() -> Saves {
        let queue: Vec<Arc<QueueEntry>> = vec![entry(7, "1000000001"), entry(8, "1000000002"), entry(9, "1000000003")];
        let order = vec![queue[2].clone(), queue[0].clone(), queue[1].clone()];
        Saves {
            queue: Some(QueueSave::of(&queue, Some(&order), 1, None)),
            position: Some(PositionSave { entry: 8, key: "1000000002".into(), seconds: 12.5 }),
            ..Saves::default()
        }
    }

    #[test]
    fn what_is_written_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(&dir);
        assert_eq!(files.load(), None);
        write(&files, saved_queue());
        let loaded = files.load().unwrap();
        assert_eq!(loaded.entries.iter().map(|entry| entry.id).collect::<Vec<_>>(), [7, 8, 9]);
        assert_eq!(loaded.entries[1].track, entry(8, "1000000002").track);
        assert!(loaded.entries[1].autoplay && !loaded.entries[0].autoplay);
        assert_eq!((loaded.current, loaded.original), (8, Some(vec![9, 7, 8])));
        assert_eq!(loaded.start_frame, 600_000);
        assert_eq!(loaded.station, None);
    }

    #[test]
    fn a_position_that_does_not_fit_starts_the_entry_of_the_queue_file_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(&dir);
        write(&files, saved_queue());
        for position in [
            json!({"version": RESUME_VERSION, "entry": 99, "key": "1000000002", "position": 5.0}),
            json!({"version": RESUME_VERSION, "entry": 8, "key": "another song", "position": 5.0}),
            json!({"version": RESUME_VERSION, "entry": 8, "key": "1000000002", "position": -5.0}),
            json!({"version": RESUME_VERSION, "entry": 8, "key": "1000000002", "position": 1e12}),
            json!({"version": RESUME_VERSION + 1, "entry": 8, "key": "1000000002", "position": 5.0}),
            json!("nonsense"),
        ] {
            util::write_private_json(&files.position, &position).unwrap();
            let loaded = files.load().unwrap();
            assert_eq!((loaded.current, loaded.start_frame), (8, 0), "{position}");
        }
        // the position may name another entry of the queue: the queue's file was written earlier
        let other = json!({"version": RESUME_VERSION, "entry": 9, "key": "1000000003", "position": 1.0});
        util::write_private_json(&files.position, &other).unwrap();
        assert_eq!(files.load().map(|loaded| (loaded.current, loaded.start_frame)), Some((9, 48_000)));
        // past the end of the song: it starts over
        let past = json!({"version": RESUME_VERSION, "entry": 9, "key": "1000000003", "position": 200.0});
        util::write_private_json(&files.position, &past).unwrap();
        assert_eq!(files.load().map(|loaded| (loaded.current, loaded.start_frame)), Some((9, 0)));
        fs::remove_file(&files.position).unwrap();
        assert_eq!(files.load().map(|loaded| (loaded.current, loaded.start_frame)), Some((8, 0)));
    }

    #[test]
    fn a_damaged_or_foreign_queue_file_counts_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(&dir);
        write(&files, saved_queue());
        let good: Value = serde_json::from_str(&fs::read_to_string(&files.queue).unwrap()).unwrap();
        let spoiled = |change: &dyn Fn(&mut Value)| {
            let mut value = good.clone();
            change(&mut value);
            value
        };
        for broken in [
            spoiled(&|value| value["version"] = json!(RESUME_VERSION + 1)),
            spoiled(&|value| value["entries"] = json!([])),
            spoiled(&|value| value["entries"] = json!("none")),
            spoiled(&|value| value["entries"][0]["track"] = json!(5)),
            spoiled(&|value| value["entries"][0]["id"] = json!(8)),
            spoiled(&|value| value["entries"][0]["id"] = json!(u64::MAX)),
            spoiled(&|value| value["current"] = json!(99)),
            spoiled(&|value| value["original"] = json!([7, 8])),
            spoiled(&|value| value["original"] = json!([7, 8, 8])),
            spoiled(&|value| {
                value["station"] = json!({"id": "ra.1", "name": "Live", "subtitle": "", "live": true,
                                                       "kind": "streaming", "format": "stream"})
            }),
        ] {
            fs::write(&files.queue, broken.to_string()).unwrap();
            assert_eq!(files.load(), None, "{broken}");
        }
        for text in ["", "{", "[1, 2]", "null"] {
            fs::write(&files.queue, text).unwrap();
            assert_eq!(files.load(), None, "{text:?}");
        }
    }

    #[test]
    fn a_very_long_queue_is_kept_around_the_current_entry() {
        let total = RESUME_MAX_ENTRIES as u64 + 500;
        let queue: Vec<Arc<QueueEntry>> = (0..total).map(|id| entry(id, &format!("{}", 1_000_000_000 + id))).collect();
        let order: Vec<Arc<QueueEntry>> = queue.iter().rev().cloned().collect();
        let save = QueueSave::of(&queue, Some(&order), 400, None);
        assert_eq!(save.entries.len(), RESUME_MAX_ENTRIES);
        assert_eq!(save.entries[0].id, 400 - RESUME_ENTRIES_BEFORE as u64);
        assert_eq!(save.current, 400);
        let original = save.original.as_ref().unwrap();
        assert_eq!(original.len(), RESUME_MAX_ENTRIES);
        assert_eq!(original[0], save.entries.last().unwrap().id);
        // near the end it is the last part of the queue
        let save = QueueSave::of(&queue, None, total as usize - 2, None);
        assert_eq!(save.entries.last().unwrap().id, total - 1);
        assert_eq!(save.entries.len(), RESUME_MAX_ENTRIES);
    }

    #[test]
    fn later_requests_replace_earlier_ones() {
        let mut saves = saved_queue();
        saves.merge(Saves { remove: true, ..Saves::default() });
        assert!(saves.remove && saves.queue.is_none() && saves.position.is_none());
        saves.merge(saved_queue());
        assert!(!saves.remove && saves.queue.is_some() && saves.position.is_some());
    }
}
