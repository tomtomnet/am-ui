//! Autoplay and stations: the songs that follow.
//!
//! After every command the worker looks at the entry at the queue position (`follow_queue()`):
//! when it changed (the position moved, or the queue was replaced) it asks the radio for what
//! the queue needs. For the user's own queue that is Autoplay's songs, about [`AUTOPLAY_AHEAD`]
//! of them ahead of the position (see [`autoplay_wanted`]), from a station made from the
//! queue's last songs; for the queue of a station (`play_station()`) it is [`STATION_MORE`]
//! more of the station's songs once fewer than [`STATION_LOW`] are left.
//!
//! The radio's calls block on the network, so they are radio jobs. Each kind of request has one
//! slot; an answer is used only while its job is still in its slot. Replacing or emptying the
//! queue, Autoplay off and repeat on empty the slots, so that answers for a queue that has
//! changed are dropped when they arrive.

use std::sync::Arc;

use super::Worker;
use crate::api::{ApiError, CONTINUOUS_SEEDS, Seed};
use crate::loudness::lock;
use crate::player::radio::{Answer, Ask, Equivalents, RadioJob};
use crate::player::shared::State;
use crate::player::{
    AUTOPLAY_AHEAD, AUTOPLAY_USER_LIMIT, Container, LIVE_RADIO, QueueEntry, STATION_LOW, STATION_MORE,
    STATION_QUEUE_FIXED,
};
use crate::types::{RepeatMode, Station, Track};

/// What the worker follows of the queue for Autoplay and for the station whose queue plays.
#[derive(Default)]
pub(super) struct Following {
    /// The id of the entry at the queue position when the worker last looked.
    seen: Option<u64>,
    /// Autoplay's station for the queue.
    station: Option<Station>,
    /// Autoplay's request in flight: its station, or its songs.
    autoplay_job: Option<Arc<RadioJob>>,
    /// Autoplay's last request failed: it is tried again at the next change of the position.
    autoplay_failed: bool,
    /// The first songs of the station `play_station()` asked for.
    first_songs: Option<Arc<RadioJob>>,
    /// More songs of the station whose queue plays.
    more_songs: Option<Arc<RadioJob>>,
    /// The last entry of the station's queue when more songs were asked for: the same need is
    /// never asked for twice (every request moves the station on).
    asked_after: Option<u64>,
}

/// Whether `job` is the one in `slot`.
fn is(slot: &Option<Arc<RadioJob>>, job: &Arc<RadioJob>) -> bool {
    slot.as_ref().is_some_and(|waiting| Arc::ptr_eq(waiting, job))
}

/// How many songs Autoplay asks for: [`AUTOPLAY_AHEAD`] minus its entries after the current one,
/// so that that many are always ahead. (Apple's web player counts the current entry among the
/// unplayed ones and adds one back when it is Autoplay's; counting from the next entry is the
/// same.) Nothing is asked for when this is 0 or less.
fn autoplay_wanted(unplayed_autoplay: usize) -> i64 {
    AUTOPLAY_AHEAD as i64 - unplayed_autoplay as i64
}

/// What Autoplay's station is made from: the last [`CONTINUOUS_SEEDS`] songs the user put into
/// the queue (in the user's order, also while it is shuffled), each with what it was played from
/// when that is known, under the catalog id it plays under.
fn seeds_of(st: &State, equivalents: &Equivalents) -> Vec<Seed> {
    let order = st.original.as_ref().unwrap_or(&st.queue);
    let seed = |entry: &Arc<QueueEntry>| {
        let ids = Track {
            catalog_id: equivalents.playing_id(&entry.track),
            library_id: entry.track.library_id.clone(),
            ..Track::default()
        };
        let seed = Seed::of(&ids)?;
        Some(match &entry.container {
            Some(container) => seed.within(&container.kind, &container.id),
            None => seed,
        })
    };
    let users = order.iter().rev().filter(|entry| !entry.autoplay);
    let mut seeds: Vec<Seed> = users.filter_map(seed).take(CONTINUOUS_SEEDS).collect();
    seeds.reverse();
    seeds
}

impl Worker {
    /// After each command: when the entry at the queue position is another one than last time,
    /// ask for what the queue needs now. A failure of Autoplay is tried again here.
    pub(super) fn follow_queue(&mut self) {
        let entry = self.st().entry_at_index().map(|entry| entry.id);
        if entry == self.following.seen {
            return;
        }
        self.following.seen = entry;
        if entry.is_none() {
            // the queue is empty: what comes into it is a new queue
            self.forget_queue();
            return;
        }
        self.following.autoplay_failed = false;
        self.top_up();
    }

    /// Let go of what was asked for the queue as it is: Autoplay's station and the answers on
    /// their way (for Autoplay and for the station whose queue plays).
    pub(super) fn forget_queue(&mut self) {
        let following = &mut self.following;
        following.station = None;
        following.autoplay_job = None;
        following.autoplay_failed = false;
        following.more_songs = None;
        following.asked_after = None;
    }

    /// A station asked for with `play_station()` is not wanted any more.
    pub(super) fn forget_station_request(&mut self) {
        self.following.first_songs = None;
    }

    /// Start a radio job; `None` without a radio.
    pub(super) fn ask(&mut self, ask: Ask) -> Option<Arc<RadioJob>> {
        let radio = self.shared.radio.clone()?;
        Some(RadioJob::start(&radio, &self.shared.poster, ask))
    }

    /// Ask for the songs the queue needs: more of the station whose queue plays, else
    /// Autoplay's.
    pub(super) fn top_up(&mut self) {
        if self.shared.radio.is_none() {
            return;
        }
        let station = self.st().station.clone();
        match station {
            Some(station) => self.more_station_songs(station),
            None => self.more_autoplay_songs(),
        }
    }

    /// Autoplay applies: it is on, repeat is off and the queue is not a station's.
    fn autoplay_active(&mut self) -> bool {
        let st = self.st();
        st.autoplay && st.repeat == RepeatMode::Off && st.station.is_none()
    }

    /// Autoplay's next request: its station when there is none yet, else its songs.
    fn more_autoplay_songs(&mut self) {
        let waiting = self.following.autoplay_job.is_some() || self.following.autoplay_failed;
        if waiting || !self.autoplay_active() {
            return;
        }
        let (users, wanted, seeds) = {
            let st = lock(&self.shared.state);
            if st.entry_at_index().is_none() {
                return;
            }
            let unplayed = st.unplayed();
            let users = unplayed.iter().filter(|entry| !entry.autoplay).count();
            let wanted = autoplay_wanted(unplayed.len() - users);
            // (the seeds are needed only to make a station)
            let made = self.following.station.is_some();
            let seeds = if made { Vec::new() } else { seeds_of(&st, &self.shared.equivalents) };
            (users, wanted, seeds)
        };
        if users > AUTOPLAY_USER_LIMIT || wanted <= 0 {
            return;
        }
        let ask = match self.following.station.clone() {
            Some(station) => {
                self.log(&format!("autoplay: asking station {} for {wanted} songs", station.id));
                Ask::AutoplaySongs(station, wanted)
            }
            None if seeds.is_empty() => return, // no song of the queue a station can start from
            None => {
                self.log(&format!("autoplay: asking for a station from {} songs", seeds.len()));
                Ask::AutoplayStation(seeds)
            }
        };
        self.following.autoplay_job = self.ask(ask);
    }

    /// More songs of the station whose queue plays, once fewer than [`STATION_LOW`] unplayed
    /// entries are left; once for each end of the queue.
    fn more_station_songs(&mut self, station: Station) {
        if self.following.more_songs.is_some() {
            return;
        }
        let (unplayed, last) = {
            let st = self.st();
            if st.index < 0 {
                return;
            }
            (st.unplayed().len(), st.queue.last().map(|entry| entry.id))
        };
        if unplayed >= STATION_LOW || last == self.following.asked_after {
            return;
        }
        self.log(&format!("station {}: asking for {STATION_MORE} more songs", station.id));
        self.following.asked_after = last;
        self.following.more_songs = self.ask(Ask::MoreStationSongs(station));
    }

    /// Autoplay stops applying (it was switched off, or repeat on): its unplayed entries leave
    /// the queue, and what was asked for it is not wanted any more. Its station stays.
    pub(super) fn drop_autoplay_entries(&mut self) {
        self.following.autoplay_job = None;
        let removed = self.st().remove_unplayed_autoplay();
        if removed > 0 {
            self.log(&format!("autoplay: {removed} unplayed entries removed"));
            self.emit_queue();
            self.arm_prefetch();
        }
    }

    /// Autoplay applies again (it was switched on, or repeat off): it asks at once.
    pub(super) fn resume_autoplay(&mut self) {
        self.following.autoplay_failed = false;
        self.top_up();
    }

    /// Put songs of `station` at the end of the queue (and of the order it goes back to):
    /// Autoplay's entries, or the station's own.
    fn append(&mut self, songs: Vec<Track>, station: &Station, autoplay: bool) {
        let songs: Vec<Track> = songs.into_iter().filter(Track::playable).collect();
        if songs.is_empty() {
            return;
        }
        let container = Container::new("stations", station.id.clone());
        let entries = self.shared.new_entries(songs, Some(&container), autoplay);
        {
            let mut st = self.st();
            st.queue.extend(entries.iter().cloned());
            if let Some(original) = st.original.as_mut() {
                original.extend(entries);
            }
        }
        self.emit_queue();
        self.emit_state();
        self.arm_prefetch(); // the first of them may be the next entry
    }

    // ------------------------------------------------------------------ commands

    pub(super) fn cmd_set_autoplay(&mut self, enabled: bool) {
        {
            let mut st = self.st();
            if st.autoplay == enabled {
                return;
            }
            st.autoplay = enabled;
        }
        self.shared.save_settings();
        self.log(&format!("autoplay {}", if enabled { "on" } else { "off" }));
        if !enabled {
            self.drop_autoplay_entries();
        }
        self.emit_state();
        if enabled {
            self.resume_autoplay();
        }
    }

    /// A station of songs replaces the queue once its first songs are there; until then the
    /// queue plays on. A live broadcast is refused.
    pub(super) fn cmd_play_station(&mut self, station: Station) {
        if !station.has_tracks() {
            self.emit_error(LIVE_RADIO.to_string(), None, false);
            return;
        }
        if self.shared.radio.is_none() {
            self.log(&format!("station {}: no radio to ask for its songs", station.id));
            return;
        }
        self.log(&format!("station {}: asking for its songs", station.id));
        // (a station asked for before is not wanted any more)
        self.following.first_songs = self.ask(Ask::StationSongs(station));
    }

    /// The queue of a station is not edited: a command that would is refused with a notice.
    pub(super) fn refuse_station_edit(&mut self) -> bool {
        if self.st().station.is_none() {
            return false;
        }
        self.emit_error(STATION_QUEUE_FIXED.to_string(), None, false);
        true
    }

    // ------------------------------------------------------------------ answers

    pub(super) fn cmd_radio_done(&mut self, job: &Arc<RadioJob>) {
        let Some(outcome) = job.take_outcome() else { return };
        match &job.ask {
            Ask::AutoplayStation(_) | Ask::AutoplaySongs(..) => self.autoplay_answered(job, outcome),
            Ask::StationSongs(station) => self.first_songs_answered(job, station, outcome),
            Ask::MoreStationSongs(station) => self.more_songs_answered(job, station, outcome),
            Ask::Equivalent(track) => self.equivalent_answered(job, track, outcome),
        }
    }

    fn autoplay_answered(&mut self, job: &Arc<RadioJob>, outcome: Result<Answer, ApiError>) {
        if !is(&self.following.autoplay_job, job) {
            self.log("autoplay: an answer for the queue as it was is dropped");
            return;
        }
        self.following.autoplay_job = None;
        match (outcome, &job.ask) {
            (Ok(Answer::Station(station)), _) => {
                self.log(&format!("autoplay: station {}", station.id));
                self.following.station = Some(*station);
                self.more_autoplay_songs();
            }
            (Ok(Answer::Songs(songs)), Ask::AutoplaySongs(station, _)) => {
                self.log(&format!("autoplay: {} songs from station {}", songs.len(), station.id));
                self.append(songs, station, true);
            }
            (Ok(_), _) => {}
            (Err(error), _) => {
                self.following.autoplay_failed = true;
                self.log(&format!("autoplay failed: {error}"));
            }
        }
    }

    fn first_songs_answered(&mut self, job: &Arc<RadioJob>, station: &Station, outcome: Result<Answer, ApiError>) {
        if !is(&self.following.first_songs, job) {
            self.log(&format!("station {}: its songs came after another queue was asked for; dropped", station.id));
            return;
        }
        self.following.first_songs = None;
        let songs: Vec<Track> = match outcome {
            Ok(Answer::Songs(songs)) => songs.into_iter().filter(Track::playable).collect(),
            Ok(_) => return,
            Err(error) => {
                self.emit_error(format!("could not play {}: {error}", station.name), None, false);
                return;
            }
        };
        self.log(&format!("station {}: {} songs", station.id, songs.len()));
        if songs.is_empty() {
            self.emit_error(format!("{} has no songs to play right now", station.name), None, false);
            return;
        }
        let container = Container::new("stations", station.id.clone());
        self.replace_queue(songs, 0, Some(&container), Some(station.clone()));
    }

    fn more_songs_answered(&mut self, job: &Arc<RadioJob>, station: &Station, outcome: Result<Answer, ApiError>) {
        if !is(&self.following.more_songs, job) {
            self.log(&format!("station {}: an answer for the queue as it was is dropped", station.id));
            return;
        }
        self.following.more_songs = None;
        match outcome {
            Ok(Answer::Songs(songs)) => {
                self.log(&format!("station {}: {} more songs", station.id, songs.len()));
                self.append(songs, station, false);
            }
            Ok(_) => {}
            Err(error) => {
                // asked again at the next change of the position
                self.following.asked_after = None;
                self.log(&format!("station {} failed: {error}", station.id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::autoplay_wanted;

    #[test]
    fn autoplay_keeps_ten_of_its_songs_ahead() {
        assert_eq!(autoplay_wanted(0), 10);
        assert_eq!(autoplay_wanted(4), 6);
        assert_eq!(autoplay_wanted(9), 1);
        assert_eq!(autoplay_wanted(10), 0);
        assert_eq!(autoplay_wanted(12), -2);
    }
}
