//! What the player asks of Apple Music besides the songs themselves, as a seam: Autoplay's
//! station and its songs, the songs of a station the user plays, and the catalog's current
//! version of a song whose own catalog version was withdrawn. The calls block on the network:
//! each runs on a job thread ([`RadioJob`]) that reports back through the worker's command
//! queue, and the worker decides what the answer is still good for. The current versions found
//! are kept in memory and in the state folder ([`Equivalents`]).

use std::collections::HashMap;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::jobs::{panic_text, spawn};
use super::shared::{Command, Poster};
use super::{EQUIVALENT_MAX_AGE_S, STATION_MORE};
use crate::api::{ApiError, MusicApi, Seed};
use crate::loudness::lock;
use crate::types::{Station, Track};
use crate::util;
use crate::web::WebError;

/// The network calls of Autoplay, of stations and of withdrawn songs: [`MusicApi`]'s, or a
/// test's stand-in. Every call blocks until Apple has answered; the player makes them on job
/// threads only.
pub trait Radio: Send + Sync {
    /// The station that goes on where a queue ends, made from the queue's last songs.
    fn continuous_station(&self, seeds: &[Seed]) -> Result<Station, ApiError>;

    /// The next songs of a station: `limit` of them, or as many as Apple decides (`None`).
    /// Every call moves the station on.
    fn station_next_tracks(&self, station_id: &str, limit: Option<i64>) -> Result<Vec<Track>, ApiError>;

    /// The catalog's current version of a song whose own catalog version Apple no longer
    /// serves; `None` when the catalog has none.
    fn equivalent_song(&self, track: &Track) -> Result<Option<Track>, ApiError>;
}

impl Radio for MusicApi {
    fn continuous_station(&self, seeds: &[Seed]) -> Result<Station, ApiError> {
        MusicApi::continuous_station(self, seeds)
    }

    fn station_next_tracks(&self, station_id: &str, limit: Option<i64>) -> Result<Vec<Track>, ApiError> {
        MusicApi::station_next_tracks(self, station_id, limit)
    }

    fn equivalent_song(&self, track: &Track) -> Result<Option<Track>, ApiError> {
        MusicApi::equivalent_song(self, track)
    }
}

// --- the calls -------------------------------------------------------------------------

/// What a radio job asks for, and what for.
pub(super) enum Ask {
    /// Autoplay's station, made from these songs of the queue.
    AutoplayStation(Vec<Seed>),
    /// This many next songs of Autoplay's station.
    AutoplaySongs(Station, i64),
    /// The first songs of a station the user wants to hear.
    StationSongs(Station),
    /// [`STATION_MORE`] more songs of the station whose queue plays.
    MoreStationSongs(Station),
    /// The catalog's current version of a song.
    Equivalent(Box<Track>),
}

/// What Apple answered.
pub(super) enum Answer {
    Station(Box<Station>),
    Songs(Vec<Track>),
    Equivalent(Option<Box<Track>>),
}

/// One call of the [`Radio`] off the worker thread. It cannot be cancelled (the call blocks on
/// the network): an answer the worker no longer wants is dropped when it arrives.
pub(super) struct RadioJob {
    pub ask: Ask,
    outcome: Mutex<Option<Result<Answer, ApiError>>>,
}

impl RadioJob {
    pub fn start(radio: &Arc<dyn Radio>, poster: &Poster, ask: Ask) -> Arc<RadioJob> {
        let job = Arc::new(RadioJob { ask, outcome: Mutex::new(None) });
        let (thread_job, thread_radio, thread_poster) = (job.clone(), radio.clone(), poster.clone());
        let started = spawn("am-ui-radio", move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| thread_job.call(&*thread_radio)));
            // reported to the worker, never raised on this thread
            let outcome = outcome.unwrap_or_else(|panic| Err(ApiError::Web(WebError::other(panic_text(&*panic)))));
            thread_job.finish(&thread_poster, outcome);
        });
        if !started {
            job.finish(poster, Err(ApiError::Web(WebError::other("could not start a thread"))));
        }
        job
    }

    fn call(&self, radio: &dyn Radio) -> Result<Answer, ApiError> {
        Ok(match &self.ask {
            Ask::AutoplayStation(seeds) => Answer::Station(Box::new(radio.continuous_station(seeds)?)),
            Ask::AutoplaySongs(station, count) => Answer::Songs(radio.station_next_tracks(&station.id, Some(*count))?),
            Ask::StationSongs(station) => Answer::Songs(radio.station_next_tracks(&station.id, None)?),
            Ask::MoreStationSongs(station) => {
                Answer::Songs(radio.station_next_tracks(&station.id, Some(STATION_MORE))?)
            }
            Ask::Equivalent(track) => Answer::Equivalent(radio.equivalent_song(track)?.map(Box::new)),
        })
    }

    fn finish(self: &Arc<Self>, poster: &Poster, outcome: Result<Answer, ApiError>) {
        *lock(&self.outcome) = Some(outcome);
        poster.post(Command::RadioDone(self.clone()));
    }

    /// The answer; the worker takes it once, when the job has reported.
    pub fn take_outcome(&self) -> Option<Result<Answer, ApiError>> {
        lock(&self.outcome).take()
    }
}

// --- current versions of withdrawn songs ------------------------------------------------

/// One current version, and when it was found (seconds since the Unix epoch).
#[derive(Debug, Clone, PartialEq)]
struct Found {
    catalog_id: String,
    found_at: f64,
}

/// A catalog id is ASCII digits.
fn song_id(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Found within [`EQUIVALENT_MAX_AGE_S`] of `now`.
fn fresh(found: &Found, now: f64) -> bool {
    now - found.found_at <= EQUIVALENT_MAX_AGE_S
}

/// The catalog's current versions of withdrawn songs, by the songs' own catalog ids: in memory
/// and in the file (0600) `{"<own id>": {"catalog_id": "<current id>", "found_at": <seconds>}}`.
/// A version found longer than [`EQUIVALENT_MAX_AGE_S`] ago counts as unknown: the song is
/// tried with its own id again, and looked up again should that still be refused.
pub(super) struct Equivalents {
    path: PathBuf,
    found: Mutex<HashMap<String, Found>>,
}

impl Equivalents {
    /// What the file holds that is still fresh; nothing when it is missing or unreadable, and
    /// an entry that is not two catalog ids and a time is passed over.
    pub fn load(path: PathBuf) -> Equivalents {
        let now = util::unix_now();
        let mut found = HashMap::new();
        for (own, entry) in util::read_json_object(&path).unwrap_or_default() {
            let catalog_id = entry.get("catalog_id").and_then(Value::as_str).filter(|id| song_id(id));
            let found_at = entry.get("found_at").and_then(Value::as_f64).filter(|seconds| seconds.is_finite());
            if let (Some(catalog_id), Some(found_at)) = (catalog_id, found_at) {
                let entry = Found { catalog_id: catalog_id.to_string(), found_at };
                if song_id(&own) && fresh(&entry, now) {
                    found.insert(own, entry);
                }
            }
        }
        Equivalents { path, found: Mutex::new(found) }
    }

    /// The current version of the song whose own catalog id is `own`, when a fresh one is known.
    pub fn get(&self, own: &str) -> Option<String> {
        let now = util::unix_now();
        lock(&self.found).get(own).filter(|found| fresh(found, now)).map(|found| found.catalog_id.clone())
    }

    /// The catalog id acquired for `track`: its current version when one is known, else its
    /// own; `None` for a song without a catalog id.
    pub fn playing_id(&self, track: &Track) -> Option<String> {
        let own = track.catalog_id.as_deref()?;
        Some(self.get(own).unwrap_or_else(|| own.to_string()))
    }

    /// `catalog_id` is the current version of `own`, found now; the file is written.
    pub fn remember(&self, own: &str, catalog_id: &str) -> io::Result<()> {
        let found = {
            let mut found = lock(&self.found);
            found.insert(own.to_string(), Found { catalog_id: catalog_id.to_string(), found_at: util::unix_now() });
            found.clone()
        };
        self.save(&found)
    }

    /// The current version of `own` is refused as well: it is forgotten, and the file written.
    pub fn forget(&self, own: &str) -> io::Result<()> {
        let found = {
            let mut found = lock(&self.found);
            if found.remove(own).is_none() {
                return Ok(());
            }
            found.clone()
        };
        self.save(&found)
    }

    /// Only the worker thread writes the file.
    fn save(&self, found: &HashMap<String, Found>) -> io::Result<()> {
        let now = util::unix_now();
        let entry = |found: &Found| json!({"catalog_id": found.catalog_id, "found_at": found.found_at});
        let entries: Map<String, Value> = found
            .iter()
            .filter(|(_, found)| fresh(found, now))
            .map(|(own, found)| (own.clone(), entry(found)))
            .collect();
        util::write_private_json(&self.path, &Value::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const DAY_S: f64 = 24.0 * 3600.0;

    #[test]
    fn current_versions_are_kept_with_the_time_they_were_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("equivalents.json");
        let equivalents = Equivalents::load(path.clone());
        let withdrawn = Track::new(Some("1000000001"), "Song A", "Artist A");
        assert_eq!(equivalents.playing_id(&withdrawn).as_deref(), Some("1000000001"));
        assert_eq!(equivalents.playing_id(&Track::new(None, "Song B", "Artist A")), None);
        assert!(!path.exists(), "nothing is written before something is found");

        equivalents.remember("1000000001", "1000000101").unwrap();
        assert_eq!(equivalents.playing_id(&withdrawn).as_deref(), Some("1000000101"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let saved = util::read_json_object(&path).unwrap();
        assert_eq!(saved["1000000001"]["catalog_id"], "1000000101");
        let found_at = saved["1000000001"]["found_at"].as_f64().unwrap();
        assert!((util::unix_now() - found_at).abs() < 60.0, "{found_at}");
        // another player in the same folder knows it
        assert_eq!(Equivalents::load(path.clone()).get("1000000001").as_deref(), Some("1000000101"));

        equivalents.forget("1000000001").unwrap();
        assert_eq!(equivalents.get("1000000001"), None);
        assert!(util::read_json_object(&path).unwrap().is_empty());
        equivalents.forget("1000000001").unwrap();
    }

    #[test]
    fn a_version_found_more_than_thirty_days_ago_is_looked_up_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("equivalents.json");
        let now = util::unix_now();
        let saved = json!({
            "1000000001": {"catalog_id": "1000000101", "found_at": now - 29.0 * DAY_S},
            "1000000002": {"catalog_id": "1000000102", "found_at": now - 31.0 * DAY_S},
            "1000000003": {"catalog_id": "not an id", "found_at": now},
            "1000000004": {"catalog_id": "1000000104"},
            "not an id": {"catalog_id": "1000000105", "found_at": now},
            "1000000006": "1000000106",
        });
        fs::write(&path, saved.to_string()).unwrap();
        let equivalents = Equivalents::load(path.clone());
        assert_eq!(equivalents.get("1000000001").as_deref(), Some("1000000101"));
        for own in ["1000000002", "1000000003", "1000000004", "not an id", "1000000006"] {
            assert_eq!(equivalents.get(own), None, "{own}");
        }
        // what is written again is what is still good
        equivalents.remember("1000000007", "1000000107").unwrap();
        let saved = util::read_json_object(&path).unwrap();
        let mut owns: Vec<&String> = saved.keys().collect();
        owns.sort();
        assert_eq!(owns, ["1000000001", "1000000007"]);
        // a file that is not a JSON object is no file
        fs::write(&path, "[1, 2]").unwrap();
        assert_eq!(Equivalents::load(path).get("1000000001"), None);
    }
}
