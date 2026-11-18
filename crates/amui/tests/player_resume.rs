//! What comes back after a restart: the queue, the song paused where it stopped, what follows it.
//! Players on the stand-ins of the other player tests, one after the other on the same files.

#[path = "player/support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use amui::acquire::AcquireError;
use amui::player::{Container, PlayerEvent, RESUME_FILE, RESUME_POSITION_FILE, RESUME_VERSION, Tuning};
use amui::types::{AudioMode, PlayerState, RepeatMode, Track};
use amui::web::WebError;
use serde_json::{Value, json};

use support::{Call, Harness, RATE, Setup, patient, station, track};

/// A song of 200 s.
fn song(id: &str) -> Track {
    Track { duration_ms: Some(200_000), album: "Album A".to_string(), ..track(id) }
}

fn songs(ids: &[&str]) -> Vec<Track> {
    ids.iter().map(|id| song(id)).collect()
}

/// The folder of the files that stay from one player to the next.
struct Run {
    dir: tempfile::TempDir,
    interval: f64,
}

impl Run {
    fn new() -> Run {
        Run { dir: tempfile::tempdir().unwrap(), interval: 0.02 }
    }

    /// A run whose position is saved only when something happens to it.
    fn without_regular_saves() -> Run {
        Run { interval: 3600.0, ..Run::new() }
    }

    fn queue_file(&self) -> PathBuf {
        self.dir.path().join(RESUME_FILE)
    }

    fn position_file(&self) -> PathBuf {
        self.dir.path().join(RESUME_POSITION_FILE)
    }

    fn setup(&self) -> Setup {
        let tuning = Tuning { resume_interval_s: self.interval, ..patient() };
        Setup {
            resume_path: Some(self.queue_file()),
            state_path: Some(self.dir.path().join("player.json")),
            tuning: Some(tuning),
            ..Setup::default()
        }
    }

    fn player(&self) -> Harness {
        Harness::with(self.setup())
    }

    fn player_with_radio(&self) -> Harness {
        Harness::with(Setup { radio: true, ..self.setup() })
    }

    fn saved_queue(&self) -> Option<Value> {
        amui::util::read_json(&self.queue_file())
    }

    /// The seconds in the position's file.
    fn saved_position(&self) -> Option<f64> {
        amui::util::read_json(&self.position_file())?.get("position")?.as_f64()
    }

    /// A player that played and stopped: songs 1 to 3, the second one paused at 30 s.
    fn left_paused(&self) {
        let h = self.player();
        h.player.play(songs(&["1", "2", "3"]), 1);
        h.wait_playing(1);
        h.output.set_position(30 * RATE);
        h.player.pause();
        h.wait_state(PlayerState::Paused);
        h.wait_until("the position saved", || self.saved_position() == Some(30.0));
        h.player.shutdown();
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-3
}

#[test]
fn the_queue_comes_back_paused_where_it_stopped() {
    let run = Run::new();
    run.left_paused();
    assert_eq!(fs::metadata(run.queue_file()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(run.position_file()).unwrap().permissions().mode() & 0o777, 0o600);

    let h = run.player();
    // at once, without waiting for anything
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.queue_length), (PlayerState::Paused, 1, 3));
    assert_eq!(snap.track.as_ref().map(Track::key).as_deref(), Some("2"));
    assert!(close(snap.position, 30.0), "{}", snap.position);
    assert_eq!(snap.duration, Some(200.0));
    assert_eq!((snap.format, snap.buffered), (None, 0.0), "nothing is loaded: no format to show");
    assert_eq!(h.queue_keys(), ["1", "2", "3"]);
    assert_eq!(h.player.queue()[1].track, song("2"));

    // the listener hears of it, and nothing starts by itself
    h.wait_until("the first events", || {
        let names = h.names();
        ["queue", "state", "position"].iter().all(|name| names.contains(name))
    });
    h.settle();
    let heard = h.events().into_iter().find_map(|event| match event {
        PlayerEvent::Position { position, .. } => Some(position),
        _ => None,
    });
    assert!(close(heard.unwrap(), 30.0));
    assert_eq!(h.snapshot().state, PlayerState::Paused);
    assert_eq!(h.output.calls(), [], "the output is not touched before a song plays");
    assert_eq!(h.output.st().starts, 0);
    assert_eq!(h.pipeline.acquired(), [], "nothing is loaded before a song plays");
}

#[test]
fn play_resumes_from_the_saved_position_and_loads_as_usual() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.toggle();
    let snap = h.wait_playing(1);
    assert_eq!(snap.track.as_ref().map(Track::key).as_deref(), Some("2"));
    assert!(close(snap.position, 30.0), "{}", snap.position);
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 1);
    assert_eq!(h.output.set_sources().first(), Some(&(Some("2.atmos:render".to_string()), 30 * RATE)));
    assert_eq!(h.output.position(), 30 * RATE);
    assert_eq!(snap.format_detail, "Dolby Atmos · E-AC-3 JOC 768 kbps · 15 objects");
    // and it goes on from there like any song: the next one is prepared
    h.wait_until("the next song armed", || h.output.next_name().is_some_and(|name| name.starts_with("3.")));
    // pause and play work as they do for a song that was started here
    h.player.toggle();
    h.wait_state(PlayerState::Paused);
    h.player.resume();
    h.wait_state(PlayerState::Playing);
}

#[test]
fn seeking_before_play_moves_the_saved_position() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.seek(50.0);
    let snap = h.wait_snapshot("the new position", |snap| close(snap.position, 50.0));
    assert_eq!(snap.state, PlayerState::Paused);
    h.wait_until("the position told", || {
        h.events().iter().any(|event| matches!(event, PlayerEvent::Position { position, .. } if close(*position, 50.0)))
    });
    h.wait_until("the position saved", || run.saved_position().is_some_and(|saved| close(saved, 50.0)));
    assert_eq!(h.pipeline.acquired(), [], "a seek loads nothing");
    // past the end of the song it stays inside the song
    h.player.seek(9999.0);
    let snap = h.wait_snapshot("the end of the song", |snap| snap.position > 199.0);
    assert!(snap.position < 200.0, "{}", snap.position);
    h.player.seek(50.0);
    h.wait_snapshot("back", |snap| close(snap.position, 50.0));
    h.player.toggle();
    h.wait_playing(1);
    assert_eq!(h.output.set_sources().first(), Some(&(Some("2.atmos:render".to_string()), 50 * RATE)));

    // the next player finds that position
    h.player.shutdown();
    let next = run.player();
    assert!(close(next.snapshot().position, 50.0));
}

#[test]
fn next_and_previous_work_on_the_restored_queue() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.next();
    let snap = h.wait_playing(2);
    assert_eq!(snap.track.as_ref().map(Track::key).as_deref(), Some("3"));
    assert_eq!(h.output.set_sources().first(), Some(&(Some("3.atmos:render".to_string()), 0)));
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 0, "the song that was left is not loaded");
    h.player.shutdown();

    let run = Run::new();
    run.left_paused();
    let h = run.player();
    // 30 s in: back to the start of the song, which stays paused and unloaded
    h.player.previous();
    let snap = h.wait_snapshot("the start of the song", |snap| close(snap.position, 0.0));
    assert_eq!((snap.state, snap.index), (PlayerState::Paused, 1));
    assert_eq!(h.pipeline.acquired(), []);
    // at its start: the song before it plays
    h.player.previous();
    let snap = h.wait_playing(0);
    assert_eq!(snap.track.as_ref().map(Track::key).as_deref(), Some("1"));
    h.player.shutdown();

    // at the first song of the queue there is nothing before it: it stays where it is
    let run = Run::new();
    let h = run.player();
    h.player.play(songs(&["1", "2"]), 0);
    h.wait_playing(0);
    h.player.pause();
    h.wait_state(PlayerState::Paused);
    h.player.shutdown();
    let h = run.player();
    h.player.previous();
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.position), (PlayerState::Paused, 0, 0.0));
    assert_eq!(h.pipeline.acquired(), []);
    // Next from the last entry behaves as it does while playing: playback stops there
    let run = Run::new();
    let h = run.player();
    h.player.play(songs(&["1", "2"]), 1);
    h.wait_playing(1);
    h.player.shutdown();
    let h = run.player();
    h.player.next();
    let snap = h.wait_state(PlayerState::Idle);
    assert_eq!((snap.index, snap.queue_length), (1, 2));
}

#[test]
fn jump_and_new_songs_work_on_the_restored_queue() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.enqueue(songs(&["4"]));
    h.player.play_next(songs(&["5"]));
    h.wait_until("the additions", || h.queue_keys() == ["1", "2", "5", "3", "4"]);
    assert_eq!(h.snapshot().state, PlayerState::Paused, "adding to the queue starts nothing");
    let entry_ids: Vec<u64> = h.player.queue().iter().map(|entry| entry.id).collect();
    assert_eq!(entry_ids.len(), 5);
    assert_eq!(entry_ids.iter().collect::<std::collections::HashSet<_>>().len(), 5, "ids stay unique");
    h.player.jump(3);
    let snap = h.wait_playing(3);
    assert_eq!(snap.track.as_ref().map(Track::key).as_deref(), Some("3"));
    assert_eq!(h.output.set_sources().first(), Some(&(Some("3.atmos:render".to_string()), 0)));

    // a new queue replaces the one that came back
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.play(songs(&["7"]), 0);
    h.wait_playing(0);
    assert_eq!(h.queue_keys(), ["7"]);
}

#[test]
fn shuffle_repeat_and_the_entries_of_autoplay_come_back() {
    let run = Run::new();
    let h = run.player_with_radio();
    h.player.set_shuffle(true);
    let ids = ["1", "2", "3", "4", "5", "6"];
    h.player.play_from(songs(&ids), 2, Container::new("albums", "1234567890"));
    h.wait_playing(0);
    h.wait_until("Autoplay's songs", || h.autoplay_flags().iter().filter(|flag| **flag).count() == 10);
    h.settle();
    let (keys, flags) = (h.queue_keys(), h.autoplay_flags());
    assert_eq!(keys[0], "3", "the song that was played comes first");
    h.output.set_position(12 * RATE);
    h.player.pause();
    h.wait_state(PlayerState::Paused);
    h.wait_until("the position saved", || run.saved_position() == Some(12.0));
    h.wait_until("the queue saved", || {
        run.saved_queue().is_some_and(|queue| queue["entries"].as_array().is_some_and(|entries| entries.len() == 16))
    });
    h.player.shutdown();

    let h = run.player_with_radio();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.shuffle), (PlayerState::Paused, 0, true));
    assert_eq!(h.queue_keys(), keys);
    assert_eq!(h.autoplay_flags(), flags);
    assert_eq!(h.player.queue()[0].container, Some(Container::new("albums", "1234567890")));
    h.settle();
    assert_eq!(h.radio.calls(), [], "nothing is asked before the queue plays");
    // shuffle off: the order it was played in, the current song still current
    h.player.set_shuffle(false);
    h.wait_until("the original order", || h.queue_keys()[..6] == ids);
    let snap = h.snapshot();
    assert_eq!((snap.index, snap.state), (2, PlayerState::Paused));
    assert!(close(snap.position, 12.0), "{}", snap.position);
    assert_eq!(h.autoplay_flags()[6..], [true; 10], "Autoplay's entries keep their place at the end");

    // repeat is back with the settings
    h.player.set_repeat(RepeatMode::One);
    h.wait_snapshot("repeat", |snap| snap.repeat == RepeatMode::One);
    h.settle();
    h.player.shutdown();
    let h = run.player();
    assert_eq!(h.snapshot().repeat, RepeatMode::One);
    assert_eq!(h.queue_keys()[..6], ids, "and so is the order the queue had");
}

#[test]
fn autoplay_goes_on_once_the_queue_plays() {
    let run = Run::new();
    let h = run.player();
    h.player.play(songs(&["1", "2"]), 1);
    h.wait_playing(1);
    h.player.shutdown();

    let h = run.player_with_radio();
    h.settle();
    assert_eq!(h.radio.calls(), []);
    h.player.toggle();
    h.wait_playing(1);
    h.wait_until("Autoplay's songs", || h.autoplay_flags().iter().filter(|flag| **flag).count() == 10);
    assert_eq!(h.autoplay_flags()[..2], [false, false]);
    assert_eq!(h.radio.continuous_calls().len(), 1);
}

#[test]
fn a_change_of_mode_or_quality_waits_for_the_song_to_be_loaded() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.set_mode(AudioMode::Stereo);
    h.player.set_lossless(false);
    h.wait_snapshot("the mode", |snap| snap.mode == AudioMode::Stereo);
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.switching), (PlayerState::Paused, false));
    assert!(close(snap.position, 30.0));
    assert_eq!(h.pipeline.acquired(), []);
    assert_eq!(h.output.calls(), []);
    h.player.toggle();
    let snap = h.wait_playing(1);
    assert_eq!(h.pipeline.acquired()[0], ("2".to_string(), "stereo".to_string()));
    assert_eq!(snap.format_detail, "AAC 256 kbps");
    assert_eq!(h.output.set_sources().first(), Some(&(Some("2.stereo:stereo.f32le".to_string()), 30 * RATE)));
}

#[test]
fn a_song_that_cannot_be_loaded_takes_the_usual_way() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.pipeline.st().fail.insert("2".to_string(), AcquireError::Web(WebError::other("no network")));
    h.player.toggle();
    let snap = h.wait_playing(2);
    assert_eq!(snap.track.as_ref().map(Track::key).as_deref(), Some("3"));
    assert_eq!(h.output.set_sources().first(), Some(&(Some("3.atmos:render".to_string()), 0)));
    let errors = h.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].0.starts_with("could not play Song 2"), "{errors:?}");
}

#[test]
fn removing_the_song_that_came_back_leaves_the_next_one_paused_at_its_start() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.remove(1);
    h.wait_until("the entry removed", || h.queue_keys() == ["1", "3"]);
    let snap = h.wait_snapshot("the next song", |snap| snap.track.as_ref().is_some_and(|track| track.key() == "3"));
    assert_eq!((snap.state, snap.index, snap.position), (PlayerState::Paused, 1, 0.0));
    h.settle();
    assert_eq!(h.pipeline.acquired(), []);
    assert_eq!(h.output.calls(), []);
    // another entry goes: nothing changes for the current one
    h.player.remove(0);
    h.wait_until("the first entry removed", || h.queue_keys() == ["3"]);
    let snap = h.wait_snapshot("the position kept", |snap| snap.index == 0);
    assert_eq!(snap.state, PlayerState::Paused);
    // the last one goes: nothing is left, and nothing to come back to
    h.player.remove(0);
    h.wait_state(PlayerState::Idle);
    assert_eq!(h.snapshot().queue_length, 0);
    h.wait_until("the files gone", || !run.queue_file().exists() && !run.position_file().exists());
}

#[test]
fn the_files_hold_what_a_new_player_needs() {
    let run = Run::new();
    run.left_paused();
    let queue = run.saved_queue().unwrap();
    assert_eq!(queue["version"], RESUME_VERSION);
    let entries = queue["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1]["track"]["catalog_id"], "2");
    assert_eq!(entries[1]["track"]["name"], "Song 2");
    assert_eq!(entries[1]["track"]["duration_ms"], 200_000);
    assert_eq!(entries[1]["autoplay"], false);
    assert!(entries[1]["track"].get("library_id").is_none(), "what is not known is left out");
    assert_eq!(queue["current"], entries[1]["id"]);
    let position = amui::util::read_json(&run.position_file()).unwrap();
    assert_eq!(position["entry"], entries[1]["id"]);
    assert_eq!(position["key"], "2");
}

#[test]
fn damaged_or_outdated_files_are_ignored() {
    // nothing readable at all
    let run = Run::new();
    fs::write(run.queue_file(), "{ not json").unwrap();
    fs::write(run.position_file(), "[1, 2").unwrap();
    let h = run.player();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.queue_length), (PlayerState::Idle, -1, 0));
    h.player.play(songs(&["1"]), 0);
    h.wait_playing(0);
    // ... and what is written afterwards is good again
    h.wait_until("the queue saved", || run.saved_queue().is_some_and(|queue| queue["version"] == RESUME_VERSION));
    h.player.shutdown();
    assert_eq!(run.player().snapshot().state, PlayerState::Paused);

    // of another version
    let run = Run::new();
    run.left_paused();
    let mut queue = run.saved_queue().unwrap();
    queue["version"] = json!(RESUME_VERSION + 1);
    fs::write(run.queue_file(), queue.to_string()).unwrap();
    assert_eq!(run.player().snapshot().state, PlayerState::Idle);

    // the queue is good, the position of no use: the song comes back at its beginning
    let run = Run::new();
    run.left_paused();
    fs::write(run.position_file(), "nonsense").unwrap();
    let h = run.player();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.position), (PlayerState::Paused, 1, 0.0));
    h.player.toggle();
    h.wait_playing(1);
    assert_eq!(h.output.set_sources().first(), Some(&(Some("2.atmos:render".to_string()), 0)));
}

#[test]
fn the_position_is_saved_when_something_happens_to_it() {
    let run = Run::without_regular_saves();
    let h = run.player();
    h.player.play(songs(&["1", "2"]), 0);
    h.wait_playing(0);
    h.wait_until("the first save", || run.saved_position() == Some(0.0) && run.saved_queue().is_some());
    let queue_file = fs::metadata(run.queue_file()).unwrap().ino();

    // playing on is not a reason here (the interval is an hour)
    h.output.set_position(7 * RATE);
    h.settle();
    assert_eq!(run.saved_position(), Some(0.0));

    // a seek is, and so is a pause
    h.player.seek(42.0);
    h.wait_until("the seek saved", || run.saved_position() == Some(42.0));
    h.output.set_position(44 * RATE);
    h.player.pause();
    h.wait_until("the pause saved", || run.saved_position() == Some(44.0));

    // while nothing moves nothing is written, whatever commands come
    let position_file = fs::metadata(run.position_file()).unwrap().ino();
    h.player.set_volume(0.5);
    h.player.pause();
    h.settle();
    assert_eq!(fs::metadata(run.position_file()).unwrap().ino(), position_file);
    assert_eq!(fs::metadata(run.queue_file()).unwrap().ino(), queue_file, "the queue did not change");

    // the next song, and the queue's file only when the queue changes
    h.player.next();
    h.wait_playing(1);
    h.wait_until("the next song saved", || {
        amui::util::read_json(&run.position_file()).is_some_and(|saved| saved["key"] == "2" && saved["position"] == 0.0)
    });
    assert_eq!(fs::metadata(run.queue_file()).unwrap().ino(), queue_file);
    h.player.enqueue(songs(&["3"]));
    h.wait_until("the queue saved", || run.saved_queue().unwrap()["entries"].as_array().unwrap().len() == 3);

    // what a player that ends knows goes to the file, without waiting for anything
    h.output.set_position(77 * RATE);
    h.player.shutdown();
    assert_eq!(run.saved_position(), Some(77.0));
    let h = run.player();
    let snap = h.snapshot();
    assert_eq!((snap.index, snap.state), (1, PlayerState::Paused));
    assert!(close(snap.position, 77.0));
}

#[test]
fn the_position_is_saved_regularly_while_a_song_plays() {
    let run = Run::new();
    let h = run.player();
    h.player.play(songs(&["1"]), 0);
    h.wait_playing(0);
    for seconds in [5, 11, 17] {
        h.output.set_position(seconds * RATE);
        h.wait_until("the position saved", || run.saved_position() == Some(seconds as f64));
    }
    // a player whose song plays comes back paused
    h.player.shutdown();
    let h = run.player();
    let snap = h.snapshot();
    assert_eq!(snap.state, PlayerState::Paused);
    assert!(close(snap.position, 17.0));
}

#[test]
fn nothing_is_written_for_a_player_that_only_came_back() {
    let run = Run::new();
    run.left_paused();
    let queue_file = fs::metadata(run.queue_file()).unwrap().ino();
    let position_file = fs::metadata(run.position_file()).unwrap().ino();
    let h = run.player();
    h.player.set_volume(0.3);
    h.settle();
    h.player.shutdown();
    assert_eq!(fs::metadata(run.queue_file()).unwrap().ino(), queue_file);
    assert_eq!(fs::metadata(run.position_file()).unwrap().ino(), position_file);
}

#[test]
fn stopping_and_emptying_the_queue() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.player.stop();
    let snap = h.wait_state(PlayerState::Idle);
    assert_eq!((snap.index, snap.queue_length), (1, 3));
    h.wait_until("the position saved", || run.saved_position() == Some(0.0));
    h.wait_until("the listener told", || {
        h.events().iter().any(|event| matches!(event, PlayerEvent::Position { position, .. } if *position == 0.0))
    });
    h.player.shutdown();
    let h = run.player();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.position), (PlayerState::Paused, 1, 0.0));
    // an empty queue leaves nothing to come back to
    h.player.clear();
    h.wait_snapshot("cleared", |snap| snap.queue_length == 0);
    h.wait_until("the files gone", || !run.queue_file().exists() && !run.position_file().exists());
    h.player.shutdown();
    let h = run.player();
    assert_eq!(h.snapshot().state, PlayerState::Idle);
}

#[test]
fn the_queue_of_a_station_comes_back_as_the_station_s() {
    let run = Run::new();
    let h = run.player_with_radio();
    h.player.play_station(station("ra.1000000009", "Station A"));
    let snap = h
        .wait_snapshot("the station's first song", |snap| snap.state == PlayerState::Playing && snap.station.is_some());
    assert_eq!(snap.index, 0);
    h.wait_until("the queue saved", || run.saved_queue().is_some());
    let keys = h.queue_keys();
    h.player.shutdown();

    let h = run.player_with_radio();
    let snap = h.snapshot();
    assert_eq!(snap.state, PlayerState::Paused);
    assert_eq!(snap.station.as_ref().map(|station| station.id.as_str()), Some("ra.1000000009"));
    assert_eq!(h.queue_keys(), keys);
    h.settle();
    assert_eq!(h.radio.calls(), []);
    // it goes on as the station's queue: more of its songs follow when they run low
    h.player.toggle();
    let snap = h.wait_playing(0);
    assert_eq!(snap.track.map(|track| track.key()), keys.first().cloned());
    h.player.jump(keys.len() - 1);
    h.wait_playing(keys.len() as i64 - 1);
    h.wait_until("more of the station's songs", || {
        h.radio.next_calls().iter().any(|(id, count)| id == "ra.1000000009" && *count == Some(5))
    });
}

#[test]
fn a_player_without_the_option_keeps_nothing() {
    let h = Harness::new();
    h.player.play(songs(&["1"]), 0);
    h.wait_playing(0);
    h.settle();
    h.player.shutdown();
    assert!(!h.config.state_dir.join(RESUME_FILE).exists());
    assert!(!h.config.state_dir.join(RESUME_POSITION_FILE).exists());
    assert!(!h.dir().join(RESUME_FILE).exists());
    assert_eq!(Harness::new().snapshot().state, PlayerState::Idle);
}

#[test]
fn a_restored_player_touches_the_output_only_when_a_song_plays() {
    let run = Run::new();
    run.left_paused();
    let h = run.player();
    h.settle();
    assert_eq!(h.output.st().starts, 0);
    h.player.toggle();
    h.wait_playing(1);
    let calls = h.output.calls();
    assert!(calls.contains(&Call::Start) && calls.contains(&Call::Resume), "{calls:?}");
    assert_eq!(h.output.st().starts, 1);
}
