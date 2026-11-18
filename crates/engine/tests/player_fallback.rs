//! A song without a Dolby Atmos version: its 2-channel version plays without a notice; a notice
//! comes only for a switch to Dolby Atmos that cannot take effect for the playing song, and
//! when the song is skipped because the fallback is off. On the stand-ins of the player's other
//! tests.

#[path = "player/support.rs"]
mod support;

use amui::types::{AudioFormat, AudioMode, PlayerState};

use support::{Harness, RATE, named, track};

const STAYING: &str = "Song B has no Dolby Atmos version; staying with the stereo version";

fn notice() -> (String, Option<String>, bool) {
    (STAYING.to_string(), Some("2".to_string()), false)
}

#[test]
fn a_song_without_atmos_plays_its_stereo_version_and_nothing_is_reported() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("2".to_string());
    h.player.play(vec![track("1"), named("2", "Song B"), track("3")], 0);
    h.wait_playing(0);
    // the prefetch of the second song finds out and prepares the stereo version instead
    h.wait_until("the stereo version armed", || h.output.next_name().is_some_and(|name| name.starts_with("2.stereo")));
    h.output.finish_current(true);
    let snap = h.wait_snapshot("the second song", |snap| snap.index == 1);
    assert_eq!(
        (snap.state, snap.mode, snap.format),
        (PlayerState::Playing, AudioMode::Atmos, Some(AudioFormat::Stereo))
    );
    assert_eq!(snap.format_detail, "AAC 256 kbps");
    h.wait_until("the third song armed", || h.output.next_name().is_some_and(|name| name.starts_with("3.atmos")));
    h.output.finish_current(true);
    let snap = h.wait_snapshot("the third song", |snap| snap.index == 2);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    // back to it: what is known about the song is used, Apple is not asked again
    h.output.set_position(RATE);
    h.player.previous();
    let snap = h.wait_playing(1);
    assert_eq!(snap.format, Some(AudioFormat::Stereo));
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 1);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_song_that_is_loaded_in_the_atmos_mode_falls_back_without_a_notice() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("2".to_string());
    h.pipeline.st().lossless_ok = true;
    h.player.play(vec![named("2", "Song B")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.mode, snap.format), (AudioMode::Atmos, Some(AudioFormat::Lossless)));
    assert_eq!(snap.error, None);
    h.settle();
    assert!(h.errors().is_empty(), "{:?}", h.errors());

    // the mode changes while such a song still loads: it is not the playing song yet
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("2".to_string());
    h.pipeline.st().delay = 0.3;
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![named("2", "Song B")], 0);
    h.wait_state(PlayerState::Loading);
    h.player.set_mode(AudioMode::Atmos);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.mode, snap.format), (AudioMode::Atmos, Some(AudioFormat::Stereo)));
    h.settle();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_switch_to_atmos_that_cannot_take_effect_is_reported() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("2".to_string());
    h.player.set_mode(AudioMode::Stereo);
    h.player.play(vec![named("2", "Song B"), track("3")], 0);
    h.wait_snapshot("the stereo version", |snap| snap.format == Some(AudioFormat::Stereo));
    h.wait_until("the next song armed", || h.output.next().is_some());
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 0);

    // not known yet that the song has no Atmos version: it is asked for, and the switch fails
    h.player.set_mode(AudioMode::Atmos);
    assert_eq!(h.wait("the notice", || h.errors().pop()), notice());
    let snap = h.wait_snapshot("the switch given up", |snap| !snap.switching);
    assert_eq!(
        (snap.mode, snap.format, snap.state),
        (AudioMode::Atmos, Some(AudioFormat::Stereo), PlayerState::Playing)
    );
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 1);

    // known now: the same switch is answered at once, and Apple is not asked again
    h.player.set_mode(AudioMode::Stereo);
    h.player.set_mode(AudioMode::Atmos);
    h.wait_until("the second notice", || h.errors().len() == 2);
    h.settle();
    assert_eq!(h.errors(), [notice(), notice()]);
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 1);
    assert_eq!(h.snapshot().format, Some(AudioFormat::Stereo));

    // the song that follows has an Atmos version: it plays in Atmos, and a switch on it says nothing
    h.wait_until("the next song armed in Atmos", || {
        h.output.next_name().is_some_and(|name| name.starts_with("3.atmos"))
    });
    h.output.finish_current(true);
    let snap = h.wait_snapshot("the next song", |snap| snap.index == 1);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    h.player.set_mode(AudioMode::Stereo);
    h.wait_snapshot("its stereo version", |snap| snap.format == Some(AudioFormat::Stereo));
    h.player.set_mode(AudioMode::Atmos);
    h.wait_snapshot("its Atmos version", |snap| snap.format == Some(AudioFormat::Atmos));
    assert_eq!(h.errors().len(), 2);
}

#[test]
fn with_the_fallback_off_the_song_is_skipped_and_that_is_reported() {
    let h = Harness::new();
    h.pipeline.st().no_atmos.insert("2".to_string());
    h.player.set_stereo_fallback(false);
    h.player.play(vec![track("1"), named("2", "Song B"), track("3")], 0);
    h.wait_playing(0);
    // the prefetch finds out; nothing is armed and nothing is said yet
    h.wait_until("the prefetch asked for", || h.pipeline.acquisitions("2", "atmos") == 1);
    h.settle();
    assert!(h.output.next().is_none());
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    // its turn comes: it is skipped, with a notice
    h.output.finish_current(false);
    let snap = h.wait_playing(2);
    assert_eq!(snap.format, Some(AudioFormat::Atmos));
    let skipped = ("Song B has no Dolby Atmos version; skipped".to_string(), Some("2".to_string()), false);
    assert_eq!(h.errors(), std::slice::from_ref(&skipped));
    assert_eq!(h.pipeline.acquisitions("2", "atmos"), 1);
    assert_eq!(h.pipeline.acquisitions("2", "stereo"), 0);
    // and again when the user goes back to it
    h.output.set_position(RATE);
    h.player.previous();
    h.wait_until("the second notice", || h.errors().len() == 2);
    let snap = h.wait_playing(2);
    assert_eq!(snap.track.unwrap().key(), "3");
    assert_eq!(h.errors(), [skipped.clone(), skipped]);
}
