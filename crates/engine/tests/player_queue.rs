//! Queue entries with ids: what `queue()` and the "queue" event say about them, and the
//! commands that name an entry by its id (jump, remove, move). On the stand-ins of the player's
//! other tests.

#[path = "player/support.rs"]
mod support;

use amui::player::{Container, PlayerEvent};
use amui::types::{PlayerState, Track};
use serde_json::json;

use support::{Call, Harness, track};

fn ids(h: &Harness) -> Vec<u64> {
    h.player.queue().iter().map(|entry| entry.id).collect()
}

/// The last "queue" event, as the Python listener's payload.
fn last_queue_event(h: &Harness) -> serde_json::Value {
    let events = h.events();
    events.iter().rev().find(|event| event.name() == "queue").map(PlayerEvent::to_json).unwrap()
}

fn queue_events(h: &Harness) -> usize {
    h.names().iter().filter(|name| **name == "queue").count()
}

fn set_sources(h: &Harness) -> usize {
    h.output.count(|call| matches!(call, Call::SetSource(..)))
}

#[test]
fn entries_have_ids_flags_and_the_container_they_were_played_from() {
    let h = Harness::new();
    let album = Container::new("albums", "1234567890");
    h.player.play_from(vec![track("1"), track("2"), track("3")], 1, album.clone());
    h.wait_playing(1);
    let queue = h.player.queue();
    assert_eq!(queue.iter().map(|entry| entry.id).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(queue.iter().all(|entry| !entry.autoplay && entry.container.as_ref() == Some(&album)));
    assert_eq!(queue[1].track, track("2"));
    let expected = json!({"keys": ["1", "2", "3"], "index": 1, "ids": [1, 2, 3], "autoplay": [false, false, false]});
    assert_eq!(last_queue_event(&h), expected);
    let entry = queue[0].to_json();
    assert_eq!((&entry["id"], &entry["autoplay"], &entry["track"]["key"]), (&json!(1), &json!(false), &json!("1")));
    assert_eq!(entry["container"], json!({"kind": "albums", "id": "1234567890"}));

    // what is added later has no container, and its own ids
    h.player.enqueue(vec![track("4")]);
    h.player.play_next(vec![track("5")]);
    h.wait_until("the additions", || h.queue_keys() == ["1", "2", "5", "3", "4"]);
    assert_eq!(ids(&h), [1, 2, 5, 3, 4]);
    let queue = h.player.queue();
    assert_eq!((&queue[2].container, &queue[4].container), (&None, &None));
    assert_eq!(queue[4].to_json()["container"], json!(null));

    // the same song twice is two entries; an id is never used again, also after a new queue
    let twice: Vec<Track> = vec![track("7"), track("7")];
    h.player.play(twice, 0);
    h.wait_until("the new queue", || h.queue_keys() == ["7", "7"]);
    assert_eq!(ids(&h), [6, 7]);
    assert!(h.player.queue().iter().all(|entry| entry.container.is_none()));
    h.wait_playing(0);
    h.player.remove_entry(7);
    h.wait_until("one of the two removed", || ids(&h) == [6]);
    assert_eq!(h.snapshot().state, PlayerState::Playing);
}

#[test]
fn jump_and_remove_name_an_entry_by_its_id() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2"), track("3")], 0);
    h.wait_playing(0);
    h.player.jump_to(3);
    let snap = h.wait_playing(2);
    assert_eq!(snap.track.unwrap().catalog_id.as_deref(), Some("3"));
    // an id the queue does not have is ignored
    let events = queue_events(&h);
    h.player.jump_to(99);
    h.player.remove_entry(99);
    h.player.move_entry(99, 0);
    h.settle();
    assert_eq!((h.snapshot().index, queue_events(&h)), (2, events));
    // before the current entry: it stays the current one
    let before = set_sources(&h);
    h.player.remove_entry(1);
    h.wait_until("the first entry removed", || ids(&h) == [2, 3]);
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.index, snap.state, set_sources(&h)), (1, PlayerState::Playing, before));
    // the current entry, with nothing after it: playback stops at the entry that is left
    h.player.remove_entry(3);
    let snap = h.wait_state(PlayerState::Idle);
    assert_eq!((ids(&h), snap.index, snap.queue_length), (vec![2], 0, 1));
    // the current entry, with one after it: that one starts
    h.player.play(vec![track("4"), track("5")], 0);
    h.wait_playing(0);
    let current = ids(&h)[0];
    h.player.remove_entry(current);
    let snap = h.wait_snapshot("the entry that took its place", |snap| {
        snap.state == PlayerState::Playing && snap.track.as_ref().is_some_and(|track| track.key() == "5")
    });
    assert_eq!((snap.index, snap.queue_length), (0, 1));
}

#[test]
fn move_entry_reorders_and_the_current_entry_stays_current() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2"), track("3"), track("4")], 1);
    h.wait_playing(1);
    h.wait_until("the next entry armed", || h.output.next_name().is_some_and(|name| name.starts_with("3.")));
    let before = set_sources(&h);

    // another entry comes to follow the current one: the prefetch follows
    h.player.move_entry(4, 2);
    h.wait_until("the move", || h.queue_keys() == ["1", "2", "4", "3"]);
    assert_eq!(ids(&h), [1, 2, 4, 3]);
    h.wait_until("the new next entry armed", || h.output.next_name().is_some_and(|name| name.starts_with("4.")));
    let event = last_queue_event(&h);
    assert_eq!(
        (&event["keys"], &event["index"], &event["ids"]),
        (&json!(["1", "2", "4", "3"]), &json!(1), &json!([1, 2, 4, 3]))
    );

    // the current entry moves: it goes on playing, at its new place
    h.player.move_entry(2, 0);
    h.wait_until("the current entry first", || h.queue_keys() == ["2", "1", "4", "3"]);
    h.wait_until("its new follower armed", || h.output.next_name().is_some_and(|name| name.starts_with("1.")));
    let snap = h.snapshot();
    assert_eq!((snap.index, snap.state), (0, PlayerState::Playing));
    assert_eq!(snap.track.unwrap().key(), "2");

    // a place beyond the end is the last one; nothing follows the current entry then
    h.player.move_entry(2, 99);
    h.wait_until("the current entry last", || h.queue_keys() == ["1", "4", "3", "2"]);
    h.wait_until("nothing armed", || h.output.next().is_none());
    assert_eq!(h.snapshot().index, 3);

    // an entry before the current one moves behind it: the position follows
    h.player.move_entry(2, 1);
    h.wait_until("back to the second place", || h.queue_keys() == ["1", "2", "4", "3"]);
    h.player.move_entry(1, 3);
    h.wait_until("the first entry last", || h.queue_keys() == ["2", "4", "3", "1"]);
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.index, snap.state), (0, PlayerState::Playing));
    assert_eq!(set_sources(&h), before, "the song on the output was never touched");

    // the place an entry is at already: nothing happens
    let events = queue_events(&h);
    h.player.move_entry(4, 1);
    h.settle();
    assert_eq!(queue_events(&h), events);
}

#[test]
fn a_prefetch_in_flight_for_an_entry_that_moved_away_is_replaced() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2"), track("3")], 0);
    h.wait_playing(0);
    h.wait_until("the next entry armed", || h.output.next_name().is_some_and(|name| name.starts_with("2.")));
    // the prefetch of what follows now takes a while; before it is done, another entry follows
    h.pipeline.st().delay = 1.0;
    h.player.next();
    h.wait_until("the prefetch of the third entry asked for", || h.pipeline.acquisitions("3", "atmos") == 1);
    h.player.enqueue(vec![track("4")]);
    h.player.move_entry(4, 2);
    h.wait_until("the move", || h.queue_keys() == ["1", "2", "4", "3"]);
    h.wait_until("the new follower armed", || h.output.next_name().is_some_and(|name| name.starts_with("4.")));
    h.settle();
    assert_eq!(h.output.next_name().as_deref(), Some("4.atmos:render"), "the abandoned prefetch armed nothing");
    assert!(!h.pcm.is_marked("3.atmos"), "and holds no cache entry");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_move_is_forgotten_when_the_queue_is_unshuffled() {
    let h = Harness::new();
    let keys: Vec<String> = (1..7).map(|number| number.to_string()).collect();
    h.player.play(keys.iter().map(|key| track(key)).collect(), 0);
    h.wait_playing(0);
    h.player.set_shuffle(true);
    h.wait_snapshot("shuffle on", |snap| snap.shuffle);
    let shuffled = ids(&h);
    assert_eq!(shuffled[0], 1, "the current entry is first");
    // the last entry becomes the second one
    let moved = shuffled[5];
    h.player.move_entry(moved, 1);
    h.wait_until("the move", || ids(&h)[1] == moved);
    assert_eq!(h.snapshot().index, 0);
    h.player.set_shuffle(false);
    h.wait_snapshot("shuffle off", |snap| !snap.shuffle);
    assert_eq!(h.queue_keys(), keys);
    assert_eq!(ids(&h), [1, 2, 3, 4, 5, 6]);
}
