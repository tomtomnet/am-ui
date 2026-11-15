//! A library song whose catalog version was withdrawn (Apple refuses it to the account with
//! "Not Entitled"): the catalog's current version plays in its place and is remembered, or the
//! song is reported as no longer available and passed over. On the stand-ins of the player's
//! other tests and a stand-in radio.

#[path = "player/support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use amui::acquire::AcquireError;
use amui::api::ApiError;
use amui::player::EQUIVALENTS_FILE;
use amui::types::{PlayerState, Track};
use amui::util;
use amui::web::WebError;
use serde_json::json;

use support::{Harness, RadioCall, Setup};

const WITHDRAWN: &str = "1000000001";
const CURRENT: &str = "1000000101";
const OTHER: &str = "1000000002";
const DAY_S: f64 = 24.0 * 3600.0;

fn withdrawn() -> Track {
    Track { library_id: Some("i.AbCdEf01".to_string()), ..Track::new(Some(WITHDRAWN), "Song A", "Artist A") }
}

fn other() -> Track {
    Track::new(Some(OTHER), "Song B", "Artist A")
}

fn not_entitled() -> AcquireError {
    let message = "amp-api.music.apple.com does not let this account have this item (HTTP 403: Not Entitled)";
    AcquireError::Web(WebError::NotEntitled(message.to_string()))
}

fn pairs(acquired: &[(&str, &str)]) -> Vec<(String, String)> {
    acquired.iter().map(|(id, variant)| (id.to_string(), variant.to_string())).collect()
}

fn gone() -> (String, Option<String>, bool) {
    ("Song A is no longer available on Apple Music".to_string(), Some(WITHDRAWN.to_string()), false)
}

/// A harness with the stand-in radio in which Apple refuses the withdrawn song, whose current
/// version is `current`; Autoplay stays out of the way.
fn harness(current: Option<&str>, setup: Setup) -> Harness {
    let h = Harness::with(Setup { radio: true, ..setup });
    h.player.set_autoplay(false);
    h.pipeline.st().fail.insert(WITHDRAWN.to_string(), not_entitled());
    let version = current.map(|id| Track::new(Some(id), "Song A", "Artist A"));
    h.radio.st().equivalents.insert(WITHDRAWN.to_string(), version);
    h
}

fn equivalents_file(h: &Harness) -> std::path::PathBuf {
    h.config.state_dir.join(EQUIVALENTS_FILE)
}

/// An equivalents file that maps the withdrawn song to `current`, found `age_s` ago.
fn saved(current: &str, age_s: f64) -> (String, String) {
    let found_at = util::unix_now() - age_s;
    let content = json!({WITHDRAWN: {"catalog_id": current, "found_at": found_at}}).to_string();
    (EQUIVALENTS_FILE.to_string(), content)
}

#[test]
fn a_withdrawn_song_plays_its_current_version() {
    let h = harness(Some(CURRENT), Setup::default());
    h.player.play(vec![withdrawn()], 0);
    let snap = h.wait_state(PlayerState::Playing);
    // the entry is the song as the library has it; what plays is its current version
    assert_eq!(snap.track, Some(withdrawn()));
    assert_eq!(snap.playing_catalog_id.as_deref(), Some(CURRENT));
    assert_eq!(h.queue_keys(), [WITHDRAWN]);
    assert_eq!(h.pipeline.acquired(), pairs(&[(WITHDRAWN, "atmos"), (CURRENT, "atmos")]));
    assert!(h.output.active_name().is_some_and(|name| name.starts_with(&format!("{CURRENT}.atmos"))));
    assert_eq!(h.radio.calls(), [RadioCall::Equivalent(WITHDRAWN.to_string())]);
    assert!(h.errors().is_empty(), "never shown, and no sign-in problem: {:?}", h.errors());
    assert!(h.log_text().contains(&format!("equivalent {WITHDRAWN}: {CURRENT}")), "{}", h.log_text());
    // remembered in the state folder, privately, with the time it was found
    let path = equivalents_file(&h);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let saved = util::read_json_object(&path).unwrap();
    assert_eq!(saved[WITHDRAWN]["catalog_id"], CURRENT);
    let found_at = saved[WITHDRAWN]["found_at"].as_f64().unwrap();
    assert!((util::unix_now() - found_at).abs() < 600.0, "{found_at}");

    // and in memory: the next play goes to the current version at once
    h.player.play(vec![other()], 0);
    h.wait_snapshot("the other song", |snap| {
        snap.state == PlayerState::Playing && snap.playing_catalog_id.as_deref() == Some(OTHER)
    });
    h.player.play(vec![withdrawn()], 0);
    let snap = h.wait_snapshot("the song again", |snap| {
        snap.state == PlayerState::Playing && snap.track.as_ref().is_some_and(|track| track.key() == WITHDRAWN)
    });
    assert_eq!(snap.playing_catalog_id.as_deref(), Some(CURRENT));
    assert_eq!(h.pipeline.acquisitions(WITHDRAWN, "atmos"), 1);
    assert_eq!(h.radio.calls().len(), 1);
}

#[test]
fn the_next_player_does_not_fail_first() {
    let h = harness(Some(CURRENT), Setup { state_files: vec![saved(CURRENT, DAY_S)], ..Setup::default() });
    h.player.play(vec![withdrawn()], 0);
    // while it loads, the snapshot names the catalog id that is acquired
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.track, snap.playing_catalog_id.as_deref()), (Some(withdrawn()), Some(CURRENT)));
    assert_eq!(h.pipeline.acquired(), pairs(&[(CURRENT, "atmos")]));
    assert!(h.radio.calls().is_empty());
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_current_version_found_more_than_thirty_days_ago_is_looked_up_again() {
    let newer = "1000000103";
    let h = harness(Some(newer), Setup { state_files: vec![saved(CURRENT, 31.0 * DAY_S)], ..Setup::default() });
    h.player.play(vec![withdrawn()], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.playing_catalog_id.as_deref(), Some(newer));
    assert_eq!(h.pipeline.acquired(), pairs(&[(WITHDRAWN, "atmos"), (newer, "atmos")]));
    assert_eq!(h.radio.calls(), [RadioCall::Equivalent(WITHDRAWN.to_string())]);
    let saved = util::read_json_object(&equivalents_file(&h)).unwrap();
    assert_eq!(saved[WITHDRAWN]["catalog_id"], newer);
}

#[test]
fn without_a_current_version_the_song_is_reported_and_passed_over() {
    let h = harness(None, Setup::default());
    h.player.play(vec![withdrawn(), other()], 0);
    let snap = h.wait_playing(1);
    assert_eq!(snap.track, Some(other()));
    assert_eq!(snap.error, None);
    assert_eq!(h.errors(), [gone()]);
    assert!(h.states().contains(&PlayerState::Error));
    assert!(!equivalents_file(&h).exists());
    // known now: going back to it reports it again without asking the radio again
    h.player.jump(0);
    h.wait_until("the second notice", || h.errors().len() == 2);
    h.wait_playing(1);
    assert_eq!(h.errors(), [gone(), gone()]);
    assert_eq!(h.radio.calls().len(), 1);
    // a lookup that fails is not remembered
    let h = harness(None, Setup::default());
    h.radio.st().equivalent_error = Some(ApiError::Web(WebError::other("amp-api.music.apple.com: HTTP 500")));
    h.player.play(vec![withdrawn(), other()], 0);
    h.wait_playing(1);
    assert_eq!(h.errors(), [gone()]);
    h.radio.st().equivalent_error = None;
    h.radio.st().equivalents.insert(WITHDRAWN.to_string(), Some(Track::new(Some(CURRENT), "Song A", "Artist A")));
    h.player.jump(0);
    let snap = h.wait_snapshot("the current version", |snap| {
        snap.state == PlayerState::Playing && snap.playing_catalog_id.as_deref() == Some(CURRENT)
    });
    assert_eq!(snap.index, 0);
    assert_eq!(h.radio.calls().len(), 2);
}

#[test]
fn without_a_radio_a_withdrawn_song_is_reported_and_passed_over() {
    let h = Harness::new();
    h.pipeline.st().fail.insert(WITHDRAWN.to_string(), not_entitled());
    h.player.play(vec![withdrawn(), other()], 0);
    let snap = h.wait_playing(1);
    assert_eq!(snap.playing_catalog_id.as_deref(), Some(OTHER));
    assert_eq!(h.errors(), [gone()]);
}

#[test]
fn a_prefetch_that_is_refused_finds_the_current_version_for_a_gapless_handover() {
    let h = harness(Some(CURRENT), Setup::default());
    h.player.play(vec![other(), withdrawn()], 0);
    h.wait_playing(0);
    h.wait_until("the current version armed", || {
        h.output.next_name().is_some_and(|name| name.starts_with(&format!("{CURRENT}.atmos")))
    });
    h.output.finish_current(true);
    let snap = h.wait_playing(1);
    assert_eq!((snap.track, snap.playing_catalog_id.as_deref()), (Some(withdrawn()), Some(CURRENT)));
    assert_eq!(h.pipeline.acquisitions(WITHDRAWN, "atmos"), 1);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_current_version_that_is_refused_as_well_is_forgotten() {
    let h = harness(Some(CURRENT), Setup { state_files: vec![saved(CURRENT, DAY_S)], ..Setup::default() });
    h.pipeline.st().fail.insert(CURRENT.to_string(), not_entitled());
    h.player.play(vec![withdrawn(), other()], 0);
    let snap = h.wait_playing(1);
    assert_eq!(snap.track, Some(other()));
    assert_eq!(h.errors(), [gone()]);
    assert_eq!(h.pipeline.acquisitions(CURRENT, "atmos"), 1);
    assert!(h.radio.calls().is_empty(), "not looked up again in this session");
    assert!(util::read_json_object(&equivalents_file(&h)).unwrap().is_empty());
}

#[test]
fn a_withdrawn_song_and_its_current_version_share_their_audio() {
    let h = harness(Some(CURRENT), Setup { state_files: vec![saved(CURRENT, DAY_S)], ..Setup::default() });
    let current = Track::new(Some(CURRENT), "Song A", "Artist A");
    h.player.play(vec![withdrawn(), current.clone()], 0);
    h.wait_playing(0);
    h.wait_until("the next entry armed", || h.output.next().is_some());
    // one preparation of the audio both entries play, armed as its own successor
    let claims = h.pcm.claimed().iter().filter(|key| *key == "1000000101.atmos").count();
    assert_eq!(claims, 1, "{:?}", h.pcm.claimed());
    assert!(Arc::ptr_eq(&h.output.next().unwrap(), &h.output.active().unwrap()));
    h.output.finish_current(true);
    let snap = h.wait_playing(1);
    assert_eq!((snap.track, snap.playing_catalog_id.as_deref()), (Some(current), Some(CURRENT)));
    assert_eq!(h.pipeline.acquisitions(CURRENT, "atmos"), 1);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_lookup_that_answers_after_the_player_moved_on_is_kept_for_later() {
    let h = harness(Some(CURRENT), Setup::default());
    h.radio.hold();
    h.player.play(vec![withdrawn(), other()], 0);
    h.wait_until("the lookup", || h.radio.calls().len() == 1);
    let snap = h.snapshot();
    assert_eq!((snap.state, snap.playing_catalog_id.as_deref()), (PlayerState::Loading, Some(WITHDRAWN)));
    h.player.next();
    h.wait_playing(1);
    h.radio.release();
    h.wait_until("the current version remembered", || equivalents_file(&h).exists());
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.index, snap.state, snap.track), (1, PlayerState::Playing, Some(other())));
    // when the song's turn comes again, its current version plays at once
    h.player.jump(0);
    let snap = h.wait_playing(0);
    assert_eq!(snap.playing_catalog_id.as_deref(), Some(CURRENT));
    assert_eq!(h.pipeline.acquisitions(WITHDRAWN, "atmos"), 1);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}
