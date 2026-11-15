//! Stations: `play_station()` replaces the queue with a station's songs and asks for more
//! before they run out; a station's queue cannot be edited, and shuffle and repeat have no
//! effect on it. On the stand-ins of the player's other tests and a stand-in radio.

#[path = "player/support.rs"]
mod support;

use amui::api::ApiError;
use amui::player::{Container, LIVE_RADIO, STATION_QUEUE_FIXED};
use amui::types::{PlayerState, RepeatMode, Station, Track};
use amui::web::WebError;

use support::{Harness, RATE, RadioCall, radio_song, station};

const STATION: &str = "ra.1000000001";

fn station_a() -> Station {
    station(STATION, "Station A")
}

fn user(n: usize) -> Track {
    Track::new(Some(&format!("{}", 1_000_000_000 + n)), format!("Song {n}"), "Artist A")
}

fn key(n: usize) -> String {
    format!("{}", 1_000_000_000 + n)
}

fn wait_length(h: &Harness, length: usize) {
    h.wait_until(&format!("a queue of {length}"), || h.player.queue().len() == length);
}

/// Requests for more songs of the station so far.
fn asked_more(h: &Harness) -> usize {
    h.log_lines("asking for 5 more songs")
}

fn notice(message: &str) -> (String, Option<String>, bool) {
    (message.to_string(), None, false)
}

/// A harness playing Station A with `batch` first songs; Autoplay stays out of the way.
fn playing_station(batch: usize) -> Harness {
    let h = Harness::with_radio();
    h.radio.st().batch = batch;
    h.player.play_station(station_a());
    h.wait_snapshot("the station playing", |snap| {
        snap.state == PlayerState::Playing && snap.station.as_ref().is_some_and(|station| station.id == STATION)
    });
    h
}

#[test]
fn a_station_plays_its_songs_and_asks_for_more_before_they_run_out() {
    let h = playing_station(3);
    let snap = h.snapshot();
    assert_eq!(snap.station, Some(station_a()), "the snapshot names the station");
    assert_eq!((snap.index, snap.queue_length), (0, 3));
    assert_eq!(snap.track.unwrap().key(), radio_song(1));
    let entries = h.player.queue();
    assert!(
        entries.iter().all(|entry| !entry.autoplay && entry.container == Some(Container::new("stations", STATION)))
    );
    assert_eq!(h.queue_keys(), (1..=3).map(radio_song).collect::<Vec<_>>());
    // two left: nothing asked for yet
    h.settle();
    assert_eq!(asked_more(&h), 0);
    // one left: five more
    h.player.next();
    h.wait_playing(1);
    wait_length(&h, 8);
    assert_eq!(h.queue_keys()[3..], (4..=8).map(radio_song).collect::<Vec<_>>());
    h.player.next();
    h.wait_playing(2);
    h.settle();
    assert_eq!(asked_more(&h), 1);
    h.wait_until("the calls noted", || h.radio.calls().len() == 2);
    assert_eq!(h.radio.next_calls(), [(STATION.to_string(), None), (STATION.to_string(), Some(5))]);
    assert!(h.radio.continuous_calls().is_empty(), "no Autoplay on a station's queue");
    // the station's songs hand over gaplessly like any other
    let next = radio_song(4);
    h.wait_until("the next song armed", || h.output.next_name().is_some_and(|name| name.starts_with(&next)));
    h.output.finish_current(true);
    let snap = h.wait_playing(3);
    assert_eq!(snap.track.unwrap().key(), next);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn the_same_need_is_never_asked_for_twice() {
    let h = Harness::with_radio();
    h.radio.st().batch = 2;
    // the first songs come at once; the request for more that follows takes its time
    h.radio.hold_after(1);
    h.player.play_station(station_a());
    h.wait_playing(0);
    h.wait_until("more songs asked for", || h.radio.calls().len() == 2);
    h.player.next();
    h.wait_playing(1);
    h.settle();
    assert_eq!(asked_more(&h), 1, "the request on its way serves this position too");
    h.radio.release();
    wait_length(&h, 7);
    // the station runs out: the answer is empty, and the end it was for is not asked for again
    h.radio.st().left.insert(STATION.to_string(), 0);
    h.player.jump(5);
    h.wait_playing(5);
    h.wait_until("the empty answer", || h.log_lines(&format!("station {STATION}: 0 more songs")) == 1);
    h.player.next();
    h.wait_playing(6);
    h.player.previous();
    h.wait_playing(5);
    h.output.set_position(RATE);
    h.player.previous();
    h.wait_playing(4);
    h.player.next();
    h.wait_playing(5);
    h.settle();
    assert_eq!(asked_more(&h), 2);
    assert_eq!(h.player.queue().len(), 7);
    h.wait_until("the calls noted", || h.radio.calls().len() == 3);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_failed_request_for_more_is_made_again_at_the_next_position() {
    let h = playing_station(3);
    h.radio.st().next_error = Some(ApiError::Web(WebError::other("amp-api.music.apple.com: HTTP 500")));
    h.player.next();
    h.wait_playing(1);
    h.wait_until("the failure logged", || h.log_lines(&format!("station {STATION} failed")) == 1);
    h.settle();
    assert_eq!(asked_more(&h), 1);
    h.radio.st().next_error = None;
    h.player.next();
    h.wait_playing(2);
    wait_length(&h, 8);
    assert_eq!(asked_more(&h), 2);
    assert!(h.errors().is_empty(), "never shown: {:?}", h.errors());
}

#[test]
fn the_queue_of_a_station_cannot_be_changed() {
    let h = playing_station(4);
    let ids: Vec<u64> = h.player.queue().iter().map(|entry| entry.id).collect();
    h.player.enqueue(vec![user(1)]);
    h.player.play_next(vec![user(2)]);
    h.player.remove(1);
    h.player.remove_entry(ids[2]);
    h.player.move_entry(ids[3], 1);
    h.player.clear();
    h.wait_until("six refusals", || h.errors().len() == 6);
    assert_eq!(h.errors(), vec![notice(STATION_QUEUE_FIXED); 6]);
    h.settle();
    assert_eq!(h.player.queue().iter().map(|entry| entry.id).collect::<Vec<_>>(), ids);
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.queue_length), (PlayerState::Playing, 0, 4));

    // shuffle and repeat change as settings and have no effect on it
    h.player.set_shuffle(true);
    h.player.set_repeat(RepeatMode::One);
    h.wait_snapshot("the settings", |snap| snap.shuffle && snap.repeat == RepeatMode::One);
    assert_eq!(h.player.queue().iter().map(|entry| entry.id).collect::<Vec<_>>(), ids, "not shuffled");
    let next = radio_song(2);
    h.wait_until("the next song armed", || h.output.next_name().is_some_and(|name| name.starts_with(&next)));
    h.output.finish_current(true);
    h.wait_playing(1);

    // next, previous, seek and pause work
    h.player.next();
    h.wait_playing(2);
    h.output.set_position(RATE);
    h.player.previous();
    h.wait_playing(1);
    h.player.seek(12.5);
    h.wait_until("the seek", || h.output.position() == (12.5 * RATE as f64) as u64);
    h.player.pause();
    h.wait_state(PlayerState::Paused);
    h.player.resume();
    h.wait_playing(1);
    h.player.jump(3);
    h.wait_playing(3);
    h.wait_until("the songs asked for at the third one", || h.player.queue().len() == 9);
    h.settle();
    assert_eq!(h.errors().len(), 6);

    // repeat all does not take the station's queue back to its start
    h.player.set_repeat(RepeatMode::All);
    h.radio.hold();
    h.player.jump(8);
    h.wait_playing(8);
    h.wait_until("more songs asked for", || h.radio.next_calls().len() == 3);
    h.settle();
    assert!(h.output.next().is_none(), "nothing follows the last song but what the station gives");
    h.radio.release();
    wait_length(&h, 14);
    let next = radio_song(10);
    h.wait_until("the next song armed", || h.output.next_name().is_some_and(|name| name.starts_with(&next)));
}

#[test]
fn live_radio_is_refused_and_the_queue_left_alone() {
    let h = Harness::with_radio();
    h.player.set_autoplay(false);
    h.player.play(vec![user(1), user(2)], 0);
    h.wait_playing(0);
    let live = Station { live: true, format: "stream".to_string(), ..station("ra.1000000009", "Radio A") };
    h.player.play_station(live);
    h.wait_until("the refusal", || !h.errors().is_empty());
    assert_eq!(h.errors(), [notice(LIVE_RADIO)]);
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.index, snap.station), (PlayerState::Playing, 0, None));
    assert_eq!(h.queue_keys(), [key(1), key(2)]);
    assert!(h.radio.calls().is_empty());
}

#[test]
fn a_station_without_songs_right_now_is_reported_and_the_queue_left_alone() {
    let h = Harness::with_radio();
    h.player.set_autoplay(false);
    h.radio.st().left.insert(STATION.to_string(), 0);
    h.player.play(vec![user(1)], 0);
    h.wait_playing(0);
    h.player.play_station(station_a());
    h.wait_until("the notice", || !h.errors().is_empty());
    assert_eq!(h.errors(), [notice("Station A has no songs to play right now")]);
    // one that cannot be asked
    h.radio.st().next_error = Some(ApiError::Web(WebError::other("amp-api.music.apple.com: HTTP 500")));
    h.player.play_station(station_a());
    h.wait_until("the second notice", || h.errors().len() == 2);
    assert_eq!(h.errors()[1], notice("could not play Station A: amp-api.music.apple.com: HTTP 500"));
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.station, snap.queue_length), (PlayerState::Playing, None, 1));
    assert_eq!(snap.track.unwrap().key(), key(1));
}

#[test]
fn a_queue_asked_for_later_wins_over_a_station_whose_songs_are_still_coming() {
    let h = Harness::with_radio();
    h.player.set_autoplay(false);
    h.radio.hold();
    h.player.play_station(station_a());
    h.wait_until("the station's songs asked for", || h.radio.calls().len() == 1);
    h.player.play(vec![user(1), user(2)], 0);
    h.wait_playing(0);
    h.radio.release();
    h.wait_until("the songs dropped", || h.log_lines("its songs came after another queue was asked for") == 1);
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.station, snap.queue_length), (None, 2));
    assert_eq!(h.queue_keys(), [key(1), key(2)]);
    // the same for a second station asked for before the first one's songs came
    h.radio.hold();
    h.player.play_station(station("ra.1000000002", "Station B"));
    h.wait_until("Station B asked for", || h.radio.calls().len() == 2);
    h.player.play_station(station_a());
    h.wait_until("Station A asked for", || h.radio.calls().len() == 3);
    h.radio.release();
    let snap = h.wait_snapshot("Station A playing", |snap| {
        snap.state == PlayerState::Playing && snap.station.as_ref().is_some_and(|station| station.id == STATION)
    });
    assert_eq!(snap.queue_length, 4);
    h.wait_until("Station B dropped", || h.log_lines("its songs came after another queue was asked for") == 2);
    let calls = h.radio.calls();
    assert_eq!(calls[1], RadioCall::NextTracks("ra.1000000002".to_string(), None));
}

#[test]
fn a_station_s_queue_ends_when_another_queue_is_played() {
    let h = playing_station(3);
    h.player.set_shuffle(true);
    h.wait_snapshot("shuffle on", |snap| snap.shuffle);
    h.player.play(vec![user(1), user(2), user(3)], 0);
    // a queue of the user's again: no station, Autoplay asks, edits and shuffle apply
    h.wait_snapshot("the user's queue", |snap| snap.station.is_none() && snap.state == PlayerState::Playing);
    h.wait_until("Autoplay's songs", || h.player.queue().len() == 13);
    assert_eq!(h.radio.continuous_calls().len(), 1);
    h.player.enqueue(vec![user(4)]);
    h.wait_until("the addition", || h.player.queue().len() == 14);
    let snap = h.snapshot();
    assert_eq!(snap.track.unwrap().key(), key(1), "the shuffled queue starts with the song asked for");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn without_a_radio_a_station_does_nothing() {
    let h = Harness::new();
    h.player.play(vec![user(1)], 0);
    h.wait_playing(0);
    h.player.play_station(station_a());
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.station, snap.queue_length, snap.state), (None, 1, PlayerState::Playing));
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert_eq!(h.log_lines(&format!("station {STATION}: no radio")), 1);
    // a live broadcast is refused all the same
    h.player.play_station(Station { live: true, ..station_a() });
    h.wait_until("the refusal", || h.errors() == [notice(LIVE_RADIO)]);
}
