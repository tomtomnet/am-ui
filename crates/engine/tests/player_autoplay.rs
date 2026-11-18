//! Autoplay: songs like the queue's follow where the user's queue ends, from a station made
//! from its last songs, about ten ahead of the position. On the stand-ins of the player's other
//! tests and a stand-in radio.
//!
//! What the player asks of the radio is written to the playback log by the worker before it
//! handles the next command, so after `settle()` the log holds every request made so far; the
//! stand-in radio notes the calls a moment later, on the job threads, and is waited for.

#[path = "player/support.rs"]
mod support;

use std::fs;

use amui::api::ApiError;
use amui::player::{Container, PlayerEvent, QueueEntry};
use amui::types::{PlayerState, RepeatMode, Track};
use amui::web::WebError;
use serde_json::json;

use support::{Call, Harness, Noted, RadioCall, Setup, radio_song, radio_station};

/// A song of the user's: catalog id 100000000n.
fn user(n: usize) -> Track {
    Track::new(Some(&format!("{}", 1_000_000_000 + n)), format!("Song {n}"), "Artist A")
}

fn users(range: std::ops::RangeInclusive<usize>) -> Vec<Track> {
    range.map(user).collect()
}

fn key(n: usize) -> String {
    format!("{}", 1_000_000_000 + n)
}

fn queue(h: &Harness) -> Vec<QueueEntry> {
    h.player.queue()
}

fn wait_length(h: &Harness, length: usize) {
    h.wait_until(&format!("a queue of {length}"), || queue(h).len() == length);
}

/// Requests made of the radio for Autoplay so far.
fn asked(h: &Harness) -> usize {
    h.log_lines("autoplay: asking")
}

fn last_queue_event(h: &Harness) -> serde_json::Value {
    let events = h.events();
    events.iter().rev().find(|event| event.name() == "queue").map(PlayerEvent::to_json).unwrap()
}

#[test]
fn autoplay_adds_ten_songs_after_the_queue_from_its_last_songs() {
    let h = Harness::with_radio();
    let album = Container::new("albums", "1000000100");
    let mut tracks = users(1..=11);
    // a song of the library without a catalog version is a seed too (by its library id)
    tracks.push(Track { library_id: Some("i.AbCdEf12".to_string()), ..Track::new(None, "Song 12", "Artist A") });
    h.player.play_from(tracks, 0, album.clone());
    h.wait_playing(0);
    wait_length(&h, 22);
    let entries = queue(&h);
    assert!(entries[..12].iter().all(|entry| !entry.autoplay && entry.container.as_ref() == Some(&album)));
    let station = Container::new("stations", radio_station(1));
    assert!(entries[12..].iter().all(|entry| entry.autoplay && entry.container.as_ref() == Some(&station)));
    let added: Vec<String> = entries[12..].iter().map(|entry| entry.track.key()).collect();
    assert_eq!(added, (1..=10).map(radio_song).collect::<Vec<_>>());
    // the station is made from the last ten songs, each with what it was played from
    let within = Some(("albums".to_string(), "1000000100".to_string()));
    let mut seeds: Vec<_> = (3..=11).map(|n| (key(n), "songs".to_string(), within.clone())).collect();
    seeds.push(("i.AbCdEf12".to_string(), "library-songs".to_string(), within.clone()));
    h.wait_until("the calls noted", || h.radio.calls().len() == 2);
    assert_eq!(h.radio.calls(), [RadioCall::Continuous(seeds), RadioCall::NextTracks(radio_station(1), Some(10))]);
    // the "queue" event and the snapshot say it
    let flags: Vec<bool> = (0..22).map(|index| index >= 12).collect();
    h.wait_until("the queue event", || last_queue_event(&h)["autoplay"] == json!(flags));
    let snap = h.snapshot();
    assert!(snap.autoplay && h.player.autoplay());
    assert_eq!((snap.queue_length, snap.index, snap.state), (22, 0, PlayerState::Playing));
    h.settle();
    assert_eq!(asked(&h), 2);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn autoplay_waits_while_more_than_fifty_of_the_users_songs_are_unplayed() {
    let h = Harness::with_radio();
    h.player.play(users(1..=52), 0);
    h.wait_playing(0);
    h.settle();
    assert_eq!(asked(&h), 0, "51 unplayed songs of the user's");
    assert_eq!(queue(&h).len(), 52);
    // fifty are not more than fifty
    h.player.jump(1);
    h.wait_playing(1);
    wait_length(&h, 62);
    assert_eq!(h.autoplay_flags()[52..], [true; 10]);
    assert!(h.radio.calls().iter().all(|call| !matches!(call, RadioCall::Equivalent(_))));
}

#[test]
fn autoplay_songs_play_gaplessly_and_ten_stay_ahead() {
    let h = Harness::with_radio();
    h.player.play(vec![user(1)], 0);
    h.wait_playing(0);
    wait_length(&h, 11);
    // the first of them is prepared and handed to the output while the user's song plays
    let first = radio_song(1);
    h.wait_until("the first autoplay song armed", || h.output.next_name().is_some_and(|name| name.starts_with(&first)));
    let set_sources = h.output.count(|call| matches!(call, Call::SetSource(..)));
    h.output.finish_current(true);
    let snap = h.wait_playing(1);
    assert_eq!(snap.track.unwrap().key(), first);
    assert_eq!(h.output.count(|call| matches!(call, Call::SetSource(..))), set_sources, "gapless");
    // the current entry is one of Autoplay's, nine more are ahead: 10 - 9
    wait_length(&h, 12);
    h.wait_until("the calls noted", || h.radio.next_calls().len() == 2);
    assert_eq!(h.radio.next_calls()[1], (radio_station(1), Some(1)));
    // one further: again 10 - 9
    h.player.next();
    h.wait_playing(2);
    wait_length(&h, 13);
    h.wait_until("the calls noted", || h.radio.next_calls().len() == 3);
    assert_eq!(h.radio.next_calls()[2], (radio_station(1), Some(1)));
    // back to the user's song: twelve of Autoplay's are ahead, nothing is asked for
    h.player.jump(0);
    h.wait_playing(0);
    h.settle();
    assert_eq!(asked(&h), 4);
    assert_eq!(h.radio.continuous_calls().len(), 1, "one station for the queue");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn what_the_user_adds_goes_before_autoplay_s_songs() {
    let h = Harness::with_radio();
    h.player.play(users(1..=2), 0);
    h.wait_playing(0);
    wait_length(&h, 12);
    h.player.enqueue(vec![user(3)]);
    h.player.play_next(vec![user(4)]);
    wait_length(&h, 14);
    let keys = h.queue_keys();
    assert_eq!(keys[..5], [key(1), key(4), key(2), key(3), radio_song(1)]);
    assert_eq!(h.autoplay_flags()[..5], [false, false, false, false, true]);
    // from one of Autoplay's songs: "play next" right after it, "add" before the next of them
    h.player.jump(4);
    h.wait_playing(4);
    wait_length(&h, 15); // (10 - 9 more)
    h.player.enqueue(vec![user(5)]);
    h.player.play_next(vec![user(6)]);
    wait_length(&h, 17);
    let keys = h.queue_keys();
    assert_eq!(keys[4..8], [radio_song(1), key(6), key(5), radio_song(2)]);
    assert_eq!(keys.last(), Some(&radio_song(11)), "Autoplay's songs stay at the end");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn autoplay_off_takes_its_songs_out_and_on_asks_again() {
    let h = Harness::with_radio();
    h.player.play(users(1..=2), 0);
    h.wait_playing(0);
    wait_length(&h, 12);
    let second = key(2);
    h.player.set_autoplay(false);
    wait_length(&h, 2);
    let snap = h.wait_snapshot("autoplay off", |snap| !snap.autoplay);
    assert_eq!((snap.queue_length, snap.state), (2, PlayerState::Playing));
    assert!(!h.player.autoplay());
    h.wait_until("the user's next song armed", || h.output.next_name().is_some_and(|name| name.starts_with(&second)));
    // off: a change of the position asks nothing
    h.player.next();
    h.wait_playing(1);
    h.settle();
    assert_eq!(asked(&h), 2);
    // on again: the queue's station gives ten more
    h.player.set_autoplay(true);
    wait_length(&h, 12);
    assert_eq!(h.radio.continuous_calls().len(), 1);
    h.wait_until("the calls noted", || h.radio.next_calls().len() == 2);
    assert_eq!(h.radio.next_calls(), [(radio_station(1), Some(10)), (radio_station(1), Some(10))]);
    assert_eq!(h.autoplay_flags()[2..], [true; 10]);
    let keys: Vec<String> = h.queue_keys()[2..].to_vec();
    assert_eq!(keys, (11..=20).map(radio_song).collect::<Vec<_>>());
}

#[test]
fn repeat_takes_autoplay_s_songs_out_and_off_asks_again() {
    let h = Harness::with_radio();
    h.player.play(users(1..=2), 0);
    h.wait_playing(0);
    wait_length(&h, 12);
    h.player.set_repeat(RepeatMode::All);
    wait_length(&h, 2);
    // the queue repeats: after the last song comes the first, not Autoplay
    h.player.next();
    h.wait_playing(1);
    h.wait_until("the wrap-around armed", || h.output.next_name().is_some_and(|name| name.starts_with(&key(1))));
    h.settle();
    assert_eq!(asked(&h), 2);
    h.player.set_repeat(RepeatMode::One);
    h.player.set_repeat(RepeatMode::Off);
    wait_length(&h, 12);
    assert_eq!(h.autoplay_flags(), [vec![false; 2], vec![true; 10]].concat());
    h.wait_until("the calls noted", || h.radio.next_calls().len() == 2);
    assert_eq!(h.radio.continuous_calls().len(), 1);
}

#[test]
fn shuffle_leaves_autoplay_s_songs_in_their_order_after_the_user_s() {
    let h = Harness::with_radio();
    h.player.play(users(1..=6), 0);
    h.wait_playing(0);
    wait_length(&h, 16);
    h.player.set_shuffle(true);
    h.wait_snapshot("shuffle on", |snap| snap.shuffle);
    let shuffled = h.queue_keys();
    assert_eq!(shuffled[0], key(1), "the current song first");
    let mut theirs = shuffled[1..6].to_vec();
    theirs.sort();
    assert_eq!(theirs, (2..=6).map(key).collect::<Vec<_>>());
    assert_eq!(shuffled[6..], (1..=10).map(radio_song).collect::<Vec<_>>());
    // what the user adds while shuffled goes before Autoplay's songs, in both orders
    h.player.enqueue(vec![user(7)]);
    wait_length(&h, 17);
    assert_eq!(h.queue_keys()[6], key(7));
    h.player.set_shuffle(false);
    h.wait_snapshot("shuffle off", |snap| !snap.shuffle);
    let expected: Vec<String> = (1..=7).map(key).chain((1..=10).map(radio_song)).collect();
    assert_eq!(h.queue_keys(), expected);
    assert_eq!(h.snapshot().index, 0);
}

#[test]
fn a_failure_is_logged_never_shown_and_tried_again_at_the_next_position() {
    let h = Harness::with_radio();
    h.radio.st().continuous_error = Some(ApiError::Web(WebError::other("amp-api.music.apple.com: HTTP 500")));
    h.player.play(users(1..=3), 0);
    h.wait_playing(0);
    h.wait_until("the failure logged", || h.log_lines("autoplay failed: amp-api.music.apple.com: HTTP 500") == 1);
    // other commands do not ask again
    h.player.set_volume(0.5);
    h.player.set_sound_check(false);
    h.settle();
    assert_eq!(asked(&h), 1);
    assert_eq!(queue(&h).len(), 3);
    // the next position does
    h.radio.st().continuous_error = None;
    h.radio.st().next_error = Some(ApiError::Web(WebError::other("amp-api.music.apple.com: HTTP 503")));
    h.player.next();
    h.wait_playing(1);
    h.wait_until("the station's songs refused", || {
        h.log_lines("autoplay failed: amp-api.music.apple.com: HTTP 503") == 1
    });
    h.settle();
    assert_eq!(asked(&h), 3, "a station, then its songs");
    h.radio.st().next_error = None;
    h.player.next();
    h.wait_playing(2);
    wait_length(&h, 13);
    // the station made before is asked again, nothing is shown
    assert_eq!(h.radio.continuous_calls().len(), 2);
    h.wait_until("the calls noted", || h.radio.next_calls().len() == 2);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(h.snapshot().error.is_none());
}

#[test]
fn replacing_the_queue_starts_over_and_late_answers_are_dropped() {
    let h = Harness::with_radio();
    h.radio.hold();
    h.player.play(vec![user(1)], 0);
    h.wait_until("the first station asked for", || h.radio.continuous_calls().len() == 1);
    h.player.play(users(2..=3), 0);
    h.wait_until("the second station asked for", || h.radio.continuous_calls().len() == 2);
    h.radio.release();
    wait_length(&h, 12);
    h.wait_until("a dropped answer", || h.log_lines("autoplay: an answer for the queue as it was is dropped") == 1);
    let seeds = h.radio.continuous_calls();
    let ids = |seeds: &[Noted]| -> Vec<String> { seeds.iter().map(|(id, _, _)| id.clone()).collect() };
    assert_eq!((ids(&seeds[0]), ids(&seeds[1])), (vec![key(1)], vec![key(2), key(3)]));
    // only the new queue's station was asked for songs, and they follow the new queue
    h.wait_until("the calls noted", || h.radio.next_calls().len() == 1);
    let (station, count) = h.radio.next_calls()[0].clone();
    assert_eq!(count, Some(10));
    let entries = queue(&h);
    assert_eq!(entries[..2].iter().map(|entry| entry.track.key()).collect::<Vec<_>>(), [key(2), key(3)]);
    assert!(entries[2..].iter().all(|entry| entry.container == Some(Container::new("stations", station.clone()))));

    // the songs asked for a queue that is replaced before they come are dropped too
    h.player.next();
    h.wait_playing(1);
    h.radio.hold();
    h.player.next();
    h.wait_playing(2);
    h.wait_until("more songs asked for", || h.radio.next_calls().len() == 2);
    h.player.play(vec![user(4)], 0);
    h.wait_until("a station for the newest queue", || h.radio.continuous_calls().len() == 3);
    h.radio.release();
    wait_length(&h, 11);
    h.wait_until("the late songs dropped", || h.log_lines("an answer for the queue as it was is dropped") == 2);
    h.settle();
    let entries = queue(&h);
    assert_eq!(entries[0].track.key(), key(4));
    let newest = Container::new("stations", h.radio.next_calls()[2].0.clone());
    assert!(entries[1..].iter().all(|entry| entry.autoplay && entry.container.as_ref() == Some(&newest)));
    assert_eq!(entries.len(), 11);
}

#[test]
fn autoplay_is_kept_in_player_json_while_it_is_off() {
    let h = Harness::new();
    h.player.set_autoplay(false);
    h.wait_snapshot("autoplay off", |snap| !snap.autoplay);
    h.player.shutdown();
    let saved = fs::read_to_string(&h.state_path).unwrap();
    assert!(saved.contains("\"autoplay\": false"), "{saved}");
    let off = Harness::with(Setup { state_path: Some(h.state_path.clone()), ..Setup::default() });
    assert!(!off.player.autoplay() && !off.snapshot().autoplay);
    off.player.set_autoplay(true);
    off.wait_snapshot("autoplay on", |snap| snap.autoplay);
    off.player.shutdown();
    // on, its default: the file is the Python engine's
    let saved = fs::read_to_string(&h.state_path).unwrap();
    assert!(!saved.contains("autoplay"), "{saved}");
    let on = Harness::with(Setup { state_path: Some(h.state_path.clone()), ..Setup::default() });
    assert!(on.player.autoplay());
}

#[test]
fn without_a_radio_autoplay_is_only_a_setting() {
    let h = Harness::new();
    h.player.play(users(1..=2), 0);
    h.wait_playing(0);
    h.player.set_autoplay(false);
    h.player.set_autoplay(true);
    h.wait_snapshot("autoplay on again", |snap| snap.autoplay);
    h.output.finish_current(false);
    h.wait_playing(1);
    h.output.finish_current(false);
    let snap = h.wait_state(PlayerState::Idle);
    assert_eq!((snap.queue_length, snap.index), (2, 1), "the queue ends where it ends");
    assert!(h.radio.calls().is_empty());
    assert_eq!(asked(&h), 0);
}
