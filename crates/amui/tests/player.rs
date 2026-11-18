//! The player's state machine with a stand-in pipeline (acquire, render, downmix), a stand-in
//! output and a stand-in PCM cache: the tests of `tests/test_player.py`, under their names.
//!
//! Where the Python tests paused for a fixed time, these wait for what the pause was for
//! (`Harness::settle()`: every command sent so far has been handled), so that a busy machine
//! does not fail them; what a test reads right after a change of state is waited for as well,
//! because the worker tells the output and the listener a moment after the snapshot changes.

#[path = "player/support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use amui::acquire::AcquireError;
use amui::downmix::DownmixError;
use amui::lossless::LosslessError;
use amui::output::{OutputError, PlaybackLog};
use amui::player::{
    Kind, Output, OutputFactory, Player, PlayerEvent, PlayerOptions, Source, Tuning, format_detail_for,
};
use amui::render::PcmSource;
use amui::types::{AcquireResult, AudioFormat, AudioMode, PlayerState, RepeatMode};
use serde_json::{Value, json};

use support::{Call, FakePcmCache, FakePipeline, Harness, RATE, Setup, named, patient, scratch_config, track};

const ATMOS_DETAIL: &str = "Dolby Atmos · E-AC-3 JOC 768 kbps · 15 objects";

fn pairs(acquired: &[(&str, &str)]) -> Vec<(String, String)> {
    acquired.iter().map(|(id, variant)| (id.to_string(), variant.to_string())).collect()
}

fn catalog_id(snapshot: &amui::types::PlaybackSnapshot) -> Option<&str> {
    snapshot.track.as_ref().and_then(|track| track.catalog_id.as_deref())
}

fn fast_render(min_buffer: f64, max_wait: f64) -> Setup {
    let tuning = Tuning { min_buffer_seconds: min_buffer, render_start_max_wait_s: max_wait, ..patient() };
    Setup { tuning: Some(tuning), ..Setup::default() }
}

#[test]
fn play_reaches_playing_with_truthful_format() {
    let h = Harness::new();
    h.player.play(vec![track("1001"), track("1002")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(catalog_id(&snap), Some("1001"));
    assert_eq!(snap.index, 0);
    assert_eq!(snap.queue_length, 2);
    assert_eq!(snap.mode, AudioMode::Atmos);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    assert_eq!(snap.format_detail, ATMOS_DETAIL);
    assert_eq!(snap.error, None);
    assert!((snap.duration.unwrap() - 200.0).abs() < 1e-9);
    assert_eq!(h.output.st().starts, 1);
    assert!(h.pipeline.sources()[0].is(&h.output.active().unwrap()));
    assert_eq!(h.pcm.claimed()[0], "1001.atmos");
    assert_eq!(h.pipeline.st().rendered[0], "1001.atmos.m4a");
    // the next item is prefetched and handed to the output for gapless continuation
    h.wait_until("prefetch handoff", || h.output.next().is_some());
    assert_eq!(h.pipeline.acquired(), pairs(&[("1001", "atmos"), ("1002", "atmos")]));
    assert!(h.pcm.claimed().contains(&"1002.atmos".to_string()));
    // events: queue first, loading state with progress, then playing
    let names = h.names();
    let first = |name: &str| names.iter().position(|event| *event == name);
    assert!(first("queue").is_some() && first("progress").is_some(), "{names:?}");
    assert!(first("queue") < first("progress"));
    h.wait_until("position events", || h.names().contains(&"position"));
    let states = h.states();
    assert!(states.contains(&PlayerState::Loading));
    assert_eq!(states.last(), Some(&PlayerState::Playing));
    assert!(!h.errors().iter().any(|(_, _, fatal)| *fatal));
}

#[test]
fn next_and_previous() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2"), track("3")], 0);
    h.wait_state(PlayerState::Playing);
    h.player.next();
    let snap = h.wait_playing(1);
    assert_eq!(catalog_id(&snap), Some("2"));
    h.output.set_position(5 * RATE); // 5 s in: previous restarts
    let seeks = h.output.seeks();
    h.player.previous();
    h.wait_until("restart seek", || h.output.seeks() > seeks);
    // (the prefetch of track 3 may tell the output something at any time, so not "the last call")
    let calls = h.output.calls();
    assert_eq!(calls.iter().rev().find(|call| matches!(call, Call::Seek(_))), Some(&Call::Seek(0)));
    assert_eq!(h.snapshot().index, 1);
    h.output.set_position(RATE); // 1 s in: previous goes back
    h.player.previous();
    let snap = h.wait_playing(0);
    assert_eq!(catalog_id(&snap), Some("1"));
    // going back used the kept source: no second acquisition of track 1
    assert_eq!(h.pipeline.acquisitions("1", "atmos"), 1);
}

#[test]
fn no_atmos_falls_back_to_stereo_with_a_notice() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("77".to_string());
    h.player.play(vec![named("77", "Mono Song")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.mode, AudioMode::Atmos);
    assert_eq!(snap.format, Some(AudioFormat::Stereo));
    assert_eq!(snap.format_detail, "AAC 256 kbps");
    assert_eq!(h.pipeline.acquired(), pairs(&[("77", "atmos"), ("77", "stereo")]));
    assert_eq!(h.pipeline.st().decoded, ["77.stereo.m4a"]);
    let active = h.output.active().unwrap();
    assert!(h.pipeline.sources().last().unwrap().is(&active));
    assert_eq!(active.channels(), 2);
    // (the Python player reported the fallback here; this one lets the format say it, and
    // reports only a switch to Dolby Atmos that cannot take effect for the playing song)
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    h.player.set_mode(AudioMode::Stereo);
    h.player.set_mode(AudioMode::Atmos);
    let (message, song, fatal) = h.wait("the notice", || h.errors().pop());
    assert!(!fatal);
    assert!(message.contains("no Dolby Atmos version"), "{message}");
    assert_eq!(song.as_deref(), Some("77"));
    h.settle();
    assert_eq!(h.errors().len(), 1);
    assert_eq!(h.snapshot().format, Some(AudioFormat::Stereo));
    assert_eq!(h.pipeline.acquisitions("77", "atmos"), 1);
}

#[test]
fn no_atmos_skips_when_fallback_is_off() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("77".to_string());
    h.player.set_stereo_fallback(false);
    h.player.play(vec![track("77"), track("78")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(catalog_id(&snap), Some("78"));
    assert_eq!(snap.index, 1);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    assert!(!h.pipeline.acquired().contains(&("77".to_string(), "stereo".to_string())));
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].0.contains("skipped"), "{}", errors[0].0);
    assert!(!errors[0].2);
}

#[test]
fn mode_switch_keeps_position_and_is_instant_on_the_way_back() {
    let h = Harness::new();
    h.player.play(vec![track("5")], 0);
    h.wait_state(PlayerState::Playing);
    let atmos_source = h.output.active().unwrap();
    h.output.set_position(30 * RATE);
    h.player.set_mode(AudioMode::Stereo);
    let snap = h.wait_snapshot("stereo switch", |snap| snap.format == Some(AudioFormat::Stereo));
    assert_eq!(snap.mode, AudioMode::Stereo);
    assert!(!snap.switching);
    assert_eq!(snap.format_detail, "AAC 256 kbps");
    let last_set = h.output.set_sources().pop().unwrap();
    assert_eq!(last_set.1, 30 * RATE, "the swap lands at the current position");
    assert_eq!(h.output.active().unwrap().channels(), 2);
    assert_eq!(h.output.position(), 30 * RATE);
    let acquisitions = h.pipeline.acquired().len();
    h.output.set_position(40 * RATE);
    h.player.set_mode(AudioMode::Atmos);
    let snap = h.wait_snapshot("atmos switch", |snap| snap.format == Some(AudioFormat::Atmos));
    assert!(Arc::ptr_eq(&h.output.active().unwrap(), &atmos_source), "the kept render is reused");
    assert_eq!(h.output.position(), 40 * RATE);
    assert_eq!(h.pipeline.acquired().len(), acquisitions, "no new acquisition for the kept variant");
    assert_eq!(snap.format_detail, ATMOS_DETAIL);
}

#[test]
fn prefetch_handoff_advances_without_a_new_set_source() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("prefetch", || h.output.next().is_some());
    h.settle();
    let set_sources = h.output.set_sources().len();
    let evictions = h.pcm.st().evictions.len();
    h.output.finish_current(true);
    let snap = h.wait_snapshot("advance", |snap| snap.index == 1);
    assert_eq!(snap.state, PlayerState::Playing);
    assert_eq!(catalog_id(&snap), Some("2"));
    h.settle();
    assert_eq!(h.output.set_sources().len(), set_sources, "gapless: the output already switched");
    let evicted = h.pcm.st().evictions.clone();
    assert!(evicted.len() > evictions);
    let protected = evicted.last().unwrap();
    assert!(protected.contains("2.atmos") && protected.contains("1.atmos"), "{protected:?}");
    // end of the queue: idle, output silenced
    h.output.finish_current(false);
    let snap = h.wait_state(PlayerState::Idle);
    assert!(h.output.active().is_none());
    assert_eq!(snap.index, 1);
}

#[test]
fn repeat_modes() {
    let h = Harness::new();
    h.player.set_repeat(RepeatMode::One);
    h.player.play(vec![track("1"), track("2")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("repeat-one prefetch of itself", || match (h.output.next(), h.output.active()) {
        (Some(next), Some(active)) => Arc::ptr_eq(&next, &active),
        _ => false,
    });
    h.output.finish_current(true);
    h.settle();
    assert_eq!(h.snapshot().index, 0);
    assert_eq!(h.snapshot().state, PlayerState::Playing);
    h.player.set_repeat(RepeatMode::All);
    h.player.jump(1);
    h.wait_playing(1);
    h.wait_until("wrap-around prefetch", || h.output.next_name().is_some_and(|name| name.starts_with("1.")));
    h.output.finish_current(true);
    h.wait_playing(0);
    h.player.set_repeat(RepeatMode::Off);
    h.player.jump(1);
    h.wait_playing(1);
    h.settle();
    h.output.finish_current(false);
    h.wait_state(PlayerState::Idle);
}

#[test]
fn shuffle_keeps_current_first_and_unshuffles() {
    let h = Harness::new();
    let tracks: Vec<_> = (1..9).map(|number| track(&number.to_string())).collect();
    h.player.play(tracks.clone(), 2);
    h.wait_state(PlayerState::Playing);
    h.player.set_shuffle(true);
    h.wait_snapshot("shuffle on", |snap| snap.shuffle);
    let shuffled = h.player.queue();
    assert_eq!(shuffled[0].track, tracks[2]);
    assert_eq!(h.snapshot().index, 0);
    let sorted = |mut keys: Vec<String>| {
        keys.sort();
        keys
    };
    let keys_of = |tracks: &[amui::types::Track]| tracks.iter().map(|track| track.key()).collect::<Vec<_>>();
    assert_eq!(sorted(h.queue_keys()), sorted(keys_of(&tracks)));
    h.player.enqueue(vec![track("9")]);
    h.wait_until("enqueue while shuffled", || h.player.queue().len() == 9);
    h.player.set_shuffle(false);
    h.wait_snapshot("shuffle off", |snap| !snap.shuffle);
    assert_eq!(h.queue_keys(), (1..10).map(|number| number.to_string()).collect::<Vec<_>>());
    assert_eq!(h.snapshot().index, 2);
    assert_eq!(catalog_id(&h.snapshot()), Some("3"));
}

#[test]
fn errors_are_non_fatal_and_the_queue_moves_on() {
    let h = Harness::new();
    h.pipeline.st().fail.insert("1".to_string(), AcquireError::License("license refused".to_string()));
    h.player.play(vec![track("1"), track("2")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(catalog_id(&snap), Some("2"));
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].0.contains("license refused"), "{}", errors[0].0);
    assert!(!errors[0].2);
    assert_eq!(errors[0].1.as_deref(), Some("1"));
    let states = h.states();
    assert!(states.contains(&PlayerState::Error));
    assert_eq!(states.last(), Some(&PlayerState::Playing));
    assert_eq!(snap.error, None);
}

#[test]
fn downmix_profiles_matrix_and_render() {
    let h = Harness::new();
    let keys: Vec<String> = h.player.downmix_profiles().into_iter().map(|profile| profile.key).collect();
    assert_eq!(keys, ["peqdb", "apple", "itu", "binaural"]);
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    assert_eq!(h.snapshot().downmix_profile.as_deref(), Some("peqdb"), "the live preset is reported");
    h.player.set_downmix_profile("apple");
    h.wait_until("apply", || h.pipeline.st().applied == ["apple"]);
    h.wait_snapshot("apply finished", |snap| !snap.switching);
    assert_eq!(h.snapshot().downmix_profile.as_deref(), Some("apple"));
    let render_source = h.output.active().unwrap();
    h.output.set_position(10 * RATE);
    h.player.set_downmix_profile("binaural");
    h.wait_until("binaural", || !Arc::ptr_eq(&h.output.active().unwrap(), &render_source));
    let snap = h.snapshot();
    assert_eq!(h.output.active().unwrap().channels(), 2);
    assert_eq!(h.output.position(), 10 * RATE);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    assert!(snap.format_detail.ends_with("binaural"), "{}", snap.format_detail);
    assert_eq!(snap.downmix_profile.as_deref(), Some("binaural"));
    assert_eq!(h.pipeline.st().binaural, ["1.atmos.m4a"]);
    h.wait_snapshot("switch finished", |snap| !snap.switching);
    h.player.set_downmix_profile("peqdb");
    h.wait_until("back to the matrix render", || Arc::ptr_eq(&h.output.active().unwrap(), &render_source));
    h.wait_until("apply peqdb", || h.pipeline.st().applied == ["apple", "peqdb"]);
    // a failing apply is reported, not fatal, and the profile reverts
    h.wait_snapshot("settled", |snap| !snap.switching);
    h.pipeline.st().apply_error = Some(DownmixError::Failed("sink not running".to_string()));
    h.player.set_downmix_profile("itu");
    h.wait_until("apply error", || h.errors().iter().any(|(message, _, _)| message.contains("downmix")));
    h.wait_snapshot("revert", |snap| snap.downmix_profile.as_deref() == Some("peqdb"));
    assert!(!h.errors().iter().any(|(_, _, fatal)| *fatal));
    assert_eq!(h.snapshot().state, PlayerState::Playing);
}

#[test]
fn pause_during_loading_starts_paused() {
    let h = Harness::new();
    h.pipeline.st().delay = 0.4;
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Loading);
    h.player.pause();
    let snap = h.wait_state(PlayerState::Paused);
    assert!(h.output.st().paused);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    h.player.toggle();
    h.wait_state(PlayerState::Playing);
    assert!(!h.output.st().paused);
    h.player.toggle();
    h.wait_state(PlayerState::Paused);
}

#[test]
fn seek_and_stop() {
    let h = Harness::new();
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    h.player.seek(12.5);
    h.wait_until("seek", || h.output.position() == (12.5 * RATE as f64) as u64);
    assert!((h.snapshot().position - 12.5).abs() < 1e-9);
    h.player.stop();
    let snap = h.wait_state(PlayerState::Idle);
    assert!(h.output.active().is_none());
    assert_eq!(snap.position, 0.0);
    assert_eq!(snap.queue_length, 1);
    h.player.toggle();
    h.wait_state(PlayerState::Playing);
}

#[test]
fn settings_persist_across_players() {
    let h = Harness::new();
    h.player.set_volume(0.4);
    h.player.set_mode(AudioMode::Stereo);
    h.player.set_repeat(RepeatMode::All);
    h.player.set_shuffle(true);
    h.player.set_stereo_fallback(false);
    h.wait_snapshot("settings", |snap| {
        snap.volume == 0.4 && snap.repeat == RepeatMode::All && snap.shuffle && snap.mode == AudioMode::Stereo
    });
    h.player.shutdown();
    assert_eq!(fs::metadata(&h.state_path).unwrap().permissions().mode() & 0o777, 0o600);
    let other = Harness::with(Setup { state_path: Some(h.state_path.clone()), ..Setup::default() });
    let snap = other.snapshot();
    assert_eq!((snap.volume, snap.mode, snap.repeat, snap.shuffle), (0.4, AudioMode::Stereo, RepeatMode::All, true));
    assert!(!other.player.stereo_fallback());
    other.player.play(vec![track("1")], 0);
    let snap = other.wait_state(PlayerState::Playing);
    assert_eq!(snap.format, Some(AudioFormat::Stereo), "the restored mode applies to playback");
    assert_eq!(other.output.st().volume, 0.4);
}

#[test]
fn output_death_and_respawn() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2")], 0);
    h.wait_state(PlayerState::Playing);
    h.output.die("pw-cat exited with status 1");
    let snap = h.wait_state(PlayerState::Error);
    assert!(snap.error.as_deref().is_some_and(|error| error.contains("pw-cat")), "{:?}", snap.error);
    let (_, _, fatal) = h.wait("the error event", || h.errors().pop());
    assert!(!fatal);
    h.player.next();
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(h.output.st().starts, 2, "one respawn on the next play command");
    assert_eq!(catalog_id(&snap), Some("2"));
    assert_eq!(snap.error, None);
}

#[test]
fn queue_editing() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2"), track("3")], 1);
    h.wait_state(PlayerState::Playing);
    h.player.play_next(vec![track("9")]);
    h.wait_until("play_next", || h.queue_keys() == ["1", "2", "9", "3"]);
    h.wait_until("prefetch of 9", || h.output.next_name().is_some_and(|name| name.starts_with("9.")));
    assert_eq!(h.snapshot().index, 1);
    h.player.remove(0);
    h.wait_until("remove before current", || h.snapshot().index == 0 && h.queue_keys() == ["2", "9", "3"]);
    assert_eq!(h.snapshot().state, PlayerState::Playing);
    h.player.remove(0);
    let snap = h.wait_snapshot("remove current starts the next", |snap| {
        catalog_id(snap) == Some("9") && snap.state == PlayerState::Playing
    });
    assert_eq!(snap.index, 0);
    h.player.clear();
    let snap = h.wait_snapshot("cleared", |snap| snap.state == PlayerState::Idle && snap.queue_length == 0);
    assert_eq!(snap.index, -1);
}

#[test]
fn parse_song_ref() {
    use amui::player::parse_song_ref;
    assert_eq!(parse_song_ref("1234567812").as_deref(), Some("1234567812"));
    let album_link = "https://music.apple.com/us/album/some-album/1234567808?i=1234567812";
    assert_eq!(parse_song_ref(album_link).as_deref(), Some("1234567812"));
    let song_link = "https://music.apple.com/gb/song/some-song/1234567812";
    assert_eq!(parse_song_ref(song_link).as_deref(), Some("1234567812"));
    assert_eq!(parse_song_ref("https://music.apple.com/us/album/some-album/1234567808"), None);
    assert_eq!(parse_song_ref("https://example.com/song/1"), None);
    assert_eq!(parse_song_ref("not an id"), None);
}

#[test]
fn adaptive_start_waits_for_a_healthy_render_rate() {
    let h = Harness::with(fast_render(0.2, 1.0));
    h.pipeline.st().durations.insert("1".to_string(), 30.0);
    // 1.5x real time: the start buffer is there after 0.13 s but the rate is poor
    h.pipeline.st().growing.insert("1".to_string(), (1.5, 0.0));
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Loading);
    let render = h.wait("the render", || h.pipeline.sources_named("1.atmos").pop());
    let started = h.wait("the render's start", || render.started_at());
    // 0.6 s into the render (a busy machine may get here later: then there is nothing to see)
    std::thread::sleep(Duration::from_millis(600).saturating_sub(started.elapsed()));
    let state = h.snapshot().state;
    if started.elapsed() < Duration::from_millis(900) {
        assert_eq!(state, PlayerState::Loading, "a slow render is not started on the start buffer alone");
    }
    let snap = h.wait_state(PlayerState::Playing);
    let waited = started.elapsed().as_secs_f64();
    assert!(waited >= 0.9, "the capped wait (1 s here, 6 s in production) applies: {waited}");
    assert!(snap.buffered > 1.0, "more than the start buffer was buffered by the time it started");

    // a healthy render starts as soon as the start buffer exists and the rate is known: long
    // before the capped wait (which is out of reach here, so that only the rate can start it)
    let h2 = Harness::with(fast_render(0.2, 120.0));
    h2.pipeline.st().durations.insert("2".to_string(), 600.0);
    h2.pipeline.st().growing.insert("2".to_string(), (8.0, 0.0));
    h2.player.play(vec![track("2")], 0);
    let snap = h2.wait_state(PlayerState::Playing);
    let render = h2.pipeline.sources_named("2.atmos").pop().unwrap();
    assert!(!render.done(), "it plays while the render is still at work");
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
}

#[test]
fn renders_are_serialized_with_priority_for_the_current_track() {
    let h = Harness::with(fast_render(0.2, 1.0));
    for id in ["1", "2"] {
        h.pipeline.st().durations.insert(id.to_string(), 3.0);
        h.pipeline.st().growing.insert(id.to_string(), (4.0, 0.0)); // done after 0.75 s
    }
    h.player.play(vec![track("1"), track("2")], 0);
    h.wait_state(PlayerState::Playing);
    let second =
        h.wait("prefetch render after the current render finished", || h.pipeline.sources_named("2.atmos").pop());
    assert_eq!(
        *support::lock(&second.running_at_start),
        Vec::<String>::new(),
        "the prefetch render waits while the current track still renders"
    );
    let first = h.pipeline.sources_named("1.atmos").remove(0);
    assert!(first.done());
    h.wait_until("prefetch armed", || h.output.next().is_some());
}

#[test]
fn current_render_freezes_a_prefetch_render_and_resumes_it() {
    let h = Harness::with(fast_render(0.2, 0.6));
    h.pipeline.st().durations.insert("2".to_string(), 120.0);
    h.pipeline.st().durations.insert("3".to_string(), 3.0);
    // a slow, long prefetch render (handed over at the 0.6 s cap, keeps rendering)
    h.pipeline.st().growing.insert("2".to_string(), (1.0, 0.0));
    h.pipeline.st().growing.insert("3".to_string(), (4.0, 0.0));
    h.player.play(vec![track("1"), track("2"), track("3")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("track 2 armed as next", || h.output.next().is_some());
    let slow = h.pipeline.sources_named("2.atmos").remove(0);
    assert!(!slow.paused());
    h.player.jump(2);
    h.wait_until("the prefetch render frozen for the current track's render", || slow.pauses() >= 1);
    let third = h.wait("the current track's render", || h.pipeline.sources_named("3.atmos").pop());
    assert_eq!(*support::lock(&third.running_at_start), Vec::<String>::new(), "it was frozen when that one started");
    let snap = h.wait_playing(2);
    assert_eq!(catalog_id(&snap), Some("3"));
    h.wait_until("the frozen render resumed once the current one finished", || !slow.paused());
    assert_eq!(slow.error(), None, "the previous track's render was resumed, not stopped");
}

#[test]
fn prefetch_render_takes_over_from_a_previous_tracks_render() {
    let h = Harness::with(fast_render(0.2, 0.6));
    h.pipeline.st().durations.insert("1".to_string(), 120.0);
    h.pipeline.st().durations.insert("3".to_string(), 3.0);
    h.pipeline.st().growing.insert("1".to_string(), (1.0, 0.0)); // the first track renders slowly for a long time
    h.pipeline.st().growing.insert("3".to_string(), (4.0, 0.0));
    h.pipeline.st().no_atmos.insert("2".to_string()); // track 2 is a complete stereo decode: no render of its own
    h.player.play(vec![track("1"), track("2"), track("3")], 0);
    h.wait_state(PlayerState::Playing);
    let first = h.pipeline.sources_named("1.atmos").remove(0);
    h.player.next();
    h.wait_snapshot("track 2", |snap| snap.index == 1 && snap.format == Some(AudioFormat::Stereo));
    // track 1 is now the previous track: its unfinished render yields to the prefetch of track 3
    h.wait_until("previous render frozen for the next track's render", || first.pauses() >= 1);
    h.wait_until("track 3 prefetched", || h.output.next_name().is_some_and(|name| name.starts_with("3.atmos")));
    let third = h.pipeline.sources_named("3.atmos").remove(0);
    assert_eq!(*support::lock(&third.running_at_start), Vec::<String>::new());
    h.wait_until("previous render resumed once the prefetch finished", || !first.paused());
    assert_eq!(first.error(), None);
}

#[test]
fn sound_check_trims_loud_tracks_only_and_persists() {
    let h = Harness::new();
    h.pipeline.set_loudness("1.atmos", -9.0); // a loud master: 7 dB above the -16 LUFS target
    h.pipeline.set_loudness("2.atmos", -20.0); // quieter than the target: never boosted
    h.player.play(vec![track("1"), track("2")], 0);
    let snap = h.wait_snapshot("trim known", |snap| snap.track_gain_db.is_some());
    assert!(snap.sound_check);
    assert!((snap.track_gain_db.unwrap() + 7.0).abs() < 1e-9);
    h.wait_until("the trim on the output", || h.output.trim("1.atmos:render") == Some(-7.0));
    let json = snap.to_json();
    assert!(json.get("buffering").is_some() && json.get("underruns").is_some());
    h.wait_until("next track's trim", || h.output.trim("2.atmos:render").is_some());
    assert_eq!(h.output.trim("2.atmos:render"), Some(0.0));
    h.player.set_sound_check(false);
    let snap = h.wait_snapshot("sound check off", |snap| !snap.sound_check);
    assert_eq!(snap.track_gain_db, None);
    h.wait_until("the output told", || !h.output.st().sound_check);
    h.player.shutdown();
    let other = Harness::with(Setup { state_path: Some(h.state_path.clone()), ..Setup::default() });
    assert!(!other.player.sound_check(), "persisted");
    assert!(!other.snapshot().sound_check);
}

#[test]
fn sound_check_trim_is_known_before_the_track_starts() {
    // a complete decode: the stream meter finishes within the start wait, the track begins at its trim
    let h = Harness::new();
    h.pipeline.set_loudness("1.atmos", -9.0);
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    assert_eq!(h.output.trim_at_start("1.atmos:render"), Some(-7.0), "trim applied before set_source");
    assert_eq!(h.output.trim("1.atmos:render"), Some(-7.0));
    let stream = h.pipeline.st().streams[0].clone();
    assert!(support::lock(&stream).finished);
    // fed in 10 s slices so a long file still yields an early estimate
    assert_eq!(support::lock(&stream).feeds[0], 10 * RATE);
    assert_eq!(support::lock(&stream).fed, 200 * RATE);
    assert!(h.pipeline.st().measured.contains(&"1.atmos:render.f32le".to_string()));
    assert!(h.log_text().contains("loudness 1.atmos: -9.0 LUFS"), "{}", h.log_text());
}

#[test]
fn growing_render_starts_at_the_estimate_and_refines_slowly() {
    let h = Harness::new();
    h.pipeline.st().durations.insert("1".to_string(), 60.0);
    h.pipeline.st().growing.insert("1".to_string(), (15.0, 0.0)); // 15x real time: 60 s of audio in 4 s
    h.pipeline.set_estimate("1.atmos", -12.0); // what the first seconds measure (trim -4)
    h.pipeline.set_loudness("1.atmos", -9.0); // the whole track (trim -7)
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    assert_eq!(h.output.trim_at_start("1.atmos:render"), Some(-4.0), "started at the estimate");
    let render = h.pipeline.sources_named("1.atmos").remove(0);
    // set before the source started: the quick ramp (until the render is complete and measured)
    let ramp = h.output.ramp("1.atmos:render");
    assert!(ramp == Some(0.05) || render.done(), "{ramp:?}");
    h.wait_until("final measurement applied", || h.output.trim("1.atmos:render") == Some(-7.0));
    assert_eq!(h.output.ramp("1.atmos:render"), Some(1.5)); // the final value while playing: the slow ramp
    let snap = h.wait_snapshot("the final trim", |snap| snap.track_gain_db == Some(-7.0));
    assert_eq!(snap.state, PlayerState::Playing);
    let log = h.log_text();
    let estimate = log.lines().find(|line| line.contains("loudness 1.atmos: -12.0 LUFS after ")).unwrap_or_default();
    let (_, rest) = estimate.split_once(" LUFS after ").unwrap_or_default();
    let (seconds, tail) = rest.split_once(' ').unwrap_or_default();
    assert!(!seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit()), "{estimate}");
    assert_eq!(tail, "s, trim -4.0 dB (estimate)", "{estimate}");
    assert!(log.contains("trim -7.0 dB (estimate was -12.0)"), "{log}");
}

#[test]
fn without_a_stream_meter_the_complete_file_is_measured() {
    let h = Harness::new();
    h.pipeline.st().stream_meter = false;
    h.pipeline.set_loudness("1.atmos", -9.0);
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("the measurement applied", || h.output.trim("1.atmos:render") == Some(-7.0));
    assert!(h.pipeline.st().streams.is_empty());
    assert_eq!(h.pipeline.st().measured, ["1.atmos:render.f32le"]);
}

#[test]
fn dump_recent_needs_an_output() {
    let h = Harness::new();
    // no output before the first play
    let dir = tempfile::tempdir().unwrap();
    let config = scratch_config(dir.path());
    let options = PlayerOptions {
        pcm_cache: Some(Arc::new(FakePcmCache::new(dir.path()))),
        state_path: Some(dir.path().join("bare.json")),
        ..PlayerOptions::default()
    };
    let log = Arc::new(PlaybackLog::new(dir.path().join("playback.log")));
    let bare = Player::new(config, FakePipeline::new(dir.path()), log, |_| {}, options).unwrap();
    assert!(bare.dump_recent(Some(&dir.path().join("none.wav"))).is_err());
    bare.shutdown();
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    let wanted = h.dir().join("recent.wav");
    let path = h.player.dump_recent(Some(&wanted)).unwrap();
    assert_eq!(path, wanted);
    assert!(path.is_file());
}

#[test]
fn in_use_markers_follow_prepared_sources() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2"), track("3")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("current and next marked", || h.pcm.is_marked("1.atmos") && h.pcm.is_marked("2.atmos"));
    h.player.next();
    h.wait_playing(1);
    h.wait_until("new next marked", || h.pcm.is_marked("3.atmos"));
    h.player.next();
    h.wait_playing(2);
    h.wait_until("dropped source unmarked", || !h.pcm.is_marked("1.atmos"));
    assert!(h.pcm.is_marked("2.atmos") && h.pcm.is_marked("3.atmos"));
    h.player.shutdown();
    assert!(h.pcm.marked().is_empty(), "shutdown releases every marker");
}

#[test]
fn format_detail_helper() {
    let acquired = |variant: Value, duration_s: Option<f64>| AcquireResult {
        catalog_id: "1".to_string(),
        path: "/x".to_string(),
        bytes: 1,
        variant: variant.as_object().cloned().unwrap(),
        duration_s,
        from_cache: false,
    };
    let atmos =
        acquired(json!({"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"}), Some(200.0));
    let stereo =
        acquired(json!({"audio_group": "audio-stereo-256", "codecs": "mp4a.40.2", "bandwidth": "256000"}), Some(200.0));
    assert_eq!(format_detail_for(Kind::Atmos, Some(&atmos), None), "Dolby Atmos · E-AC-3 JOC 768 kbps");
    let summary = json!({"joc_object_count_max": 15});
    assert_eq!(format_detail_for(Kind::Atmos, Some(&atmos), summary.as_object()), ATMOS_DETAIL);
    assert_eq!(format_detail_for(Kind::Binaural, Some(&atmos), None), "Dolby Atmos · E-AC-3 JOC 768 kbps · binaural");
    assert_eq!(format_detail_for(Kind::Stereo, Some(&stereo), None), "AAC 256 kbps");
    assert_eq!(format_detail_for(Kind::Stereo, Some(&acquired(json!({}), None)), None), "AAC");
    // a master without audio groups: the rate, not the peak
    let ungrouped =
        [("302212", "AAC 256 kbps"), ("156065", "AAC 128 kbps"), ("80139", "AAC 64 kbps"), ("40000", "AAC 40 kbps")];
    for (bandwidth, label) in ungrouped {
        let variant = json!({"audio_group": "", "codecs": "mp4a.40.2", "bandwidth": bandwidth});
        assert_eq!(format_detail_for(Kind::Stereo, Some(&acquired(variant, None)), None), label);
    }
    let lossless = acquired(
        json!({
            "audio_group": "audio-alac-stereo-48000-24", "codecs": "alac", "bandwidth": "1846745",
            "sample_rate": 48000, "bit_depth": 24,
        }),
        Some(200.0),
    );
    assert_eq!(format_detail_for(Kind::Lossless, Some(&lossless), None), "Lossless · ALAC 24-bit/48 kHz");
    let hires = acquired(json!({"audio_group": "audio-alac-stereo-96000-24", "codecs": "alac"}), Some(200.0));
    assert_eq!(format_detail_for(Kind::Lossless, Some(&hires), None), "Hi-Res Lossless · ALAC 24-bit/96 kHz");
    assert_eq!(format_detail_for(Kind::Lossless, Some(&acquired(json!({}), None)), None), "Lossless · ALAC");
}

// ---- lossless (ALAC) kind

#[test]
fn stereo_mode_plays_lossless_when_the_wrapper_is_available() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![track("31"), track("32")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.mode, AudioMode::Stereo);
    assert_eq!(snap.format, Some(AudioFormat::Lossless));
    assert_eq!(snap.format_detail, "Lossless · ALAC 24-bit/48 kHz");
    assert_eq!(snap.to_json()["lossless"], true);
    assert_eq!(snap.to_json()["lossless_available"], true);
    assert_eq!(h.pipeline.acquired()[0], ("31".to_string(), "lossless".to_string()));
    assert_eq!(h.pipeline.st().decoded[0], "31.lossless.m4a"); // the prefetch of 32 may follow at once
    assert_eq!(h.pcm.claimed()[0], "31.lossless");
    assert_eq!(h.output.active().unwrap().channels(), 2);
    assert_eq!(h.output.active_name().as_deref(), Some("31.lossless:lossless.f32le"));
    h.wait_until("prefetch handoff", || h.output.next().is_some());
    assert!(h.pipeline.acquired().contains(&("32".to_string(), "lossless".to_string())));
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn stereo_mode_stays_aac_without_the_wrapper() {
    let h = Harness::new();
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![track("41")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format, Some(AudioFormat::Stereo));
    assert_eq!(snap.format_detail, "AAC 256 kbps");
    assert_eq!(snap.to_json()["lossless_available"], false);
    assert_eq!(h.pipeline.acquired(), pairs(&[("41", "stereo")]));
}

#[test]
fn no_atmos_fallback_goes_to_lossless_when_available() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.pipeline.st().no_atmos.insert("77".to_string());
    h.player.play(vec![named("77", "Mono Song")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.mode, AudioMode::Atmos);
    assert_eq!(snap.format, Some(AudioFormat::Lossless));
    assert_eq!(h.pipeline.acquired(), pairs(&[("77", "atmos"), ("77", "lossless")]));
    // (no notice for the fallback itself, where the Python player had one)
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    // switching Atmos off and on again does not re-try the Atmos render of a song known to lack it
    h.player.set_mode(AudioMode::Stereo);
    h.settle();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    h.player.set_mode(AudioMode::Atmos);
    h.settle();
    std::thread::sleep(Duration::from_millis(100)); // a job that should not exist would have asked by now
    assert_eq!(h.pipeline.acquisitions("77", "atmos"), 1);
    assert_eq!(h.snapshot().format, Some(AudioFormat::Lossless));
    // the switch to Atmos could not take effect for this song: that is what is reported
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].0.contains("no Dolby Atmos version"), "{}", errors[0].0);
}

#[test]
fn a_song_whose_traits_name_no_atmos_version_goes_straight_to_its_stereo_version() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    let mut song = named("77", "Mono Song");
    song.audio_traits = vec!["lossless".into(), "lossy-stereo".into()];
    h.player.play(vec![song], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.mode, snap.format), (AudioMode::Atmos, Some(AudioFormat::Lossless)));
    // no attempt at an Atmos version Apple does not list
    assert_eq!(h.pipeline.acquired(), pairs(&[("77", "lossless")]));
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_song_whose_traits_name_no_lossless_version_plays_its_aac_version() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.player.set_mode(AudioMode::Stereo);
    let mut song = named("78", "Old Song");
    song.audio_traits = vec!["lossy-stereo".into()];
    h.player.play(vec![song], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format, Some(AudioFormat::Stereo));
    assert_eq!(h.pipeline.acquired(), pairs(&[("78", "stereo")]));
}

#[test]
fn a_song_without_traits_is_asked_as_before() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("79".to_string());
    h.player.play(vec![named("79", "Library Song")], 0);
    h.wait_state(PlayerState::Playing);
    assert_eq!(h.pipeline.acquired(), pairs(&[("79", "atmos"), ("79", "stereo")]));
}

#[test]
fn lossless_failure_falls_back_to_aac_with_a_notice_and_is_remembered() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.pipeline.st().no_lossless.insert("51".to_string());
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![named("51", "Rare Song"), track("52")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format, Some(AudioFormat::Stereo));
    assert_eq!(snap.format_detail, "AAC 256 kbps");
    assert_eq!(h.pipeline.acquired()[..2], pairs(&[("51", "lossless"), ("51", "stereo")]));
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].0.contains("playing the AAC version"), "{}", errors[0].0);
    assert!(errors[0].0.contains("no lossless"), "{}", errors[0].0);
    assert!(!errors[0].2);
    // the next song still gets the lossless attempt; the failed one is not retried on the way back
    h.wait_until("lossless prefetch of the next song", || h.pipeline.acquisitions("52", "lossless") > 0);
    h.player.set_mode(AudioMode::Atmos);
    h.wait_snapshot("atmos switch", |snap| snap.format == Some(AudioFormat::Atmos));
    h.player.set_mode(AudioMode::Stereo);
    h.wait_snapshot("back to AAC", |snap| snap.format == Some(AudioFormat::Stereo));
    assert_eq!(h.pipeline.acquisitions("51", "lossless"), 1);
}

#[test]
fn lossless_prefetch_failure_prefetches_aac_instead() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.pipeline.st().no_lossless.insert("62".to_string());
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![track("61"), track("62")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("AAC prefetch after the lossless failure", || h.output.next().is_some());
    assert!(h.pipeline.acquired().contains(&("62".to_string(), "lossless".to_string())));
    assert!(h.pipeline.acquired().contains(&("62".to_string(), "stereo".to_string())));
    assert_eq!(h.output.next_name().as_deref(), Some("62.stereo:stereo.f32le"));
    assert!(h.errors().is_empty(), "{:?}", h.errors()); // prefetch failures are quiet; the fallback just happens
}

#[test]
fn set_lossless_switches_the_playing_song_and_persists() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![track("71")], 0);
    h.wait_snapshot("lossless playback", |snap| snap.format == Some(AudioFormat::Lossless));
    h.output.set_position(40 * RATE);
    h.player.set_lossless(false);
    let snap = h.wait_snapshot("switch to AAC", |snap| snap.format == Some(AudioFormat::Stereo));
    assert_eq!(snap.to_json()["lossless"], false);
    assert_eq!(h.output.set_sources().pop().unwrap().1, 40 * RATE);
    assert_eq!(h.output.active_name().as_deref(), Some("71.stereo:stereo.f32le"));
    h.player.set_lossless(true);
    h.wait_snapshot("back to lossless", |snap| snap.format == Some(AudioFormat::Lossless));
    assert_eq!(h.pipeline.acquisitions("71", "lossless"), 1); // kept prepared: instant on the way back
    h.player.set_lossless(false);
    h.wait_snapshot("AAC again", |snap| snap.format == Some(AudioFormat::Stereo));
    h.player.shutdown();
    assert_eq!(h.pipeline.st().lossless_shutdowns, 1);
    let h2 = Harness::with(Setup { state_path: Some(h.state_path.clone()), ..Setup::default() });
    assert!(!h2.player.lossless());
    assert_eq!(h2.snapshot().to_json()["lossless"], false);
}

#[test]
fn lossless_unavailable_mid_switch_stays_with_aac() {
    let h = Harness::new();
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![named("81", "Some Song")], 0);
    h.wait_snapshot("AAC playback", |snap| snap.format == Some(AudioFormat::Stereo));
    h.pipeline.st().lossless_ok = true;
    let refused = "the lossless service does not answer (ConnectionRefusedError)";
    h.pipeline.st().fail.insert("81".to_string(), LosslessError::Unavailable(refused.to_string()).into());
    h.player.set_lossless(false);
    h.player.set_lossless(true); // asks for the lossless kind now that the wrapper looks available
    h.wait_until("switch failure notice", || {
        h.errors().iter().any(|(message, _, _)| message.contains("staying with the AAC version"))
    });
    assert_eq!(h.snapshot().format, Some(AudioFormat::Stereo));
    assert!(!h.snapshot().switching);
}

// ---- beyond the Python tests

#[test]
fn events_have_the_python_payloads() {
    let h = Harness::new();
    h.player.play(vec![track("1"), track("2")], 0);
    h.wait_state(PlayerState::Playing);
    h.wait_until("a position event", || h.names().contains(&"position"));
    h.player.set_preamp(99.0);
    h.wait_until("the refusal", || !h.errors().is_empty());
    let events = h.events();
    let payload = |name: &str| events.iter().find(|event| event.name() == name).map(PlayerEvent::to_json).unwrap();
    let queue = json!({"keys": ["1", "2"], "index": 0, "ids": [1, 2], "autoplay": [false, false]});
    assert_eq!(payload("queue"), queue);
    assert_eq!(payload("progress"), json!({"phase": "manifest", "fraction": null, "catalog_id": "1"}));
    let refusal = json!({"message": "preamp must be between -60 and +20 dB", "catalog_id": null, "fatal": false});
    assert_eq!(payload("error"), refusal);
    let position = payload("position");
    assert_eq!(position.as_object().unwrap().keys().collect::<Vec<_>>(), ["position", "buffered"]);
    let state = events.iter().rev().find(|event| event.name() == "state").map(PlayerEvent::to_json).unwrap();
    assert_eq!(state["state"], "playing");
    assert_eq!(state["track"]["key"], "1");
    for key in [
        "state",
        "track",
        "index",
        "queue_length",
        "position",
        "duration",
        "buffered",
        "volume",
        "loading_phase",
        "loading_progress",
        "error",
        "repeat",
        "shuffle",
        "mode",
        "format",
        "format_detail",
        "downmix_profile",
        "switching",
        "preamp",
        "buffering",
        "underruns",
        "sound_check",
        "track_gain_db",
        "lossless",
        "lossless_available",
    ] {
        assert!(state.get(key).is_some(), "the snapshot has no {key}");
    }
    assert_eq!((state["format"].clone(), state["preamp"].clone()), (json!("atmos"), json!(-11.0)));
}

#[test]
fn the_preamp_is_set_once_per_change_and_the_latest_value_wins() {
    let h = Harness::new();
    h.wait_snapshot("the sink's preamp", |snap| snap.preamp == Some(-11.0));
    h.player.set_preamp(-11.0); // nothing to change: the shared sink is left alone
    h.settle();
    assert!(h.pipeline.st().preamps_set.is_empty());
    h.player.set_preamp(-9.0);
    h.wait_snapshot("the new preamp", |snap| snap.preamp == Some(-9.0));
    assert_eq!(h.pipeline.st().preamps_set, [-9.0]);
    h.player.set_preamp(-61.0);
    h.wait_until("the refusal", || h.errors().iter().any(|(message, _, _)| message.contains("preamp must be between")));
    assert_eq!(h.snapshot().preamp, Some(-9.0));
}

#[test]
fn a_queue_of_unplayable_songs_stops_without_playing() {
    let h = Harness::new();
    let unplayable = |number: usize| amui::types::Track::new(None, format!("Song {number}"), "Artist");
    h.player.play((0..200).map(unplayable).collect(), 0);
    h.wait_until("every song reported", || h.errors().len() == 200);
    let snap = h.wait_state(PlayerState::Idle);
    assert_eq!(snap.queue_length, 200);
    assert!(h.errors().iter().all(|(message, _, fatal)| message.ends_with("is not available for playback") && !fatal));
    assert!(h.pipeline.acquired().is_empty());
}

#[test]
fn the_same_song_and_kind_is_prepared_once_at_a_time() {
    // repeat one with a change of mode: the prefetch of the song itself and the switch of the
    // playing song ask for the same PCM cache entry
    let h = Harness::new();
    h.pipeline.st().delay = 0.3;
    h.player.set_repeat(RepeatMode::One);
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    h.player.set_mode(AudioMode::Stereo);
    h.wait_snapshot("the switch", |snap| snap.format == Some(AudioFormat::Stereo) && !snap.switching);
    h.wait_until("the song armed as its own successor", || match (h.output.next(), h.output.active()) {
        (Some(next), Some(active)) => Arc::ptr_eq(&next, &active),
        _ => false,
    });
    let claims = h.pcm.claimed().iter().filter(|key| *key == "1.stereo").count();
    assert_eq!(claims, 1, "{:?}", h.pcm.claimed());
    assert_eq!(h.pipeline.acquisitions("1", "stereo"), 1);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn the_output_is_made_on_the_first_play() {
    let h = Harness::with(Setup { without_output: true, ..Setup::default() });
    assert!(h.player.dump_recent(None).is_err());
    h.player.play(vec![track("1")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    let saved = h.player.dump_recent(None).unwrap();
    assert!(saved.starts_with(&h.config.log_dir), "{saved:?}");
    let name = saved.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("recent-") && name.ends_with(".wav") && name.len() == "recent-20260101-000000.wav".len());
    assert_eq!(h.output.st().starts, 0, "the harness's own output was not handed over");
}

#[test]
fn an_output_that_cannot_be_made_or_started_is_a_fatal_error() {
    // the factory fails: nothing can play
    let refuse: OutputFactory = Box::new(|_| Err(OutputError::Other("no sink".to_string())));
    let h = Harness::with(Setup { without_output: true, output_factory: Some(refuse), ..Setup::default() });
    h.player.play(vec![track("1"), track("2")], 0);
    let snap = h.wait_state(PlayerState::Error);
    assert_eq!(snap.error.as_deref(), Some("audio output unavailable: no sink"));
    let reported = h.wait("the error event", || h.errors().pop());
    assert_eq!(reported, ("audio output unavailable: no sink".to_string(), None, true));
    assert_eq!((snap.queue_length, snap.index), (2, 0), "the queue stays as it is");

    // the factory's output is used; it fails to start, then starts on the next play command
    let made = Arc::new(support::FakeOutput::new(1.0));
    made.st().start_error = Some(OutputError::Other("pw-cat exited with status 1: no such sink".to_string()));
    let handed = made.clone();
    let factory: OutputFactory = Box::new(move |volume| {
        handed.st().volume = volume;
        Ok(handed.clone() as Arc<dyn Output>)
    });
    let h = Harness::with(Setup { without_output: true, output_factory: Some(factory), ..Setup::default() });
    h.player.set_volume(0.25);
    h.player.play(vec![track("1")], 0);
    let snap = h.wait_state(PlayerState::Error);
    let expected = "audio output failed to start: pw-cat exited with status 1: no such sink";
    assert_eq!(snap.error.as_deref(), Some(expected));
    assert_eq!(h.wait("the error event", || h.errors().pop()), (expected.to_string(), None, true));
    // (one look at the stand-in per statement: its notes are behind a lock)
    assert_eq!(made.st().volume, 0.25, "the factory is given the volume");
    assert_eq!(made.st().starts, 0);
    made.st().start_error = None;
    h.player.toggle();
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.error, made.st().starts), (None, 1));
    assert!(made.active().is_some());
}

#[test]
fn a_listener_that_panics_does_not_stop_the_player() {
    let dir = tempfile::tempdir().unwrap();
    let config = scratch_config(dir.path());
    let output = Arc::new(support::FakeOutput::new(1.0));
    let options = PlayerOptions {
        pcm_cache: Some(Arc::new(FakePcmCache::new(dir.path()))),
        output: Some(output.clone() as Arc<dyn Output>),
        state_path: Some(dir.path().join("player.json")),
        ..PlayerOptions::default()
    };
    let log = Arc::new(PlaybackLog::new(dir.path().join("playback.log")));
    let listener = |_: PlayerEvent| panic!("a listener's own trouble");
    let player = Player::new(config, FakePipeline::new(dir.path()), log, listener, options).unwrap();
    player.play(vec![track("1")], 0);
    let deadline = Instant::now() + support::PATIENCE;
    while player.snapshot().state != PlayerState::Playing {
        assert!(Instant::now() < deadline, "{:?}", player.snapshot());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(output.active().is_some());
}
