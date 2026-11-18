//! The audio quality: which 2-channel version of a song plays, what happens when a song does not
//! have it, and that the snapshot says what is audible. On the stand-ins of the player's other
//! tests.

#[path = "player/support.rs"]
mod support;

use amui::types::{AudioFormat, AudioMode, AudioQuality, PlayerState, Track};

use support::{Harness, RATE, Setup, named, track};

const HIRES: &str = "Hi-Res Lossless · ALAC 24-bit/96 kHz · resampled to 48 kHz";
const LOSSLESS: &str = "Lossless · ALAC 24-bit/48 kHz";
const AAC: &str = "AAC 256 kbps";

fn pairs(acquired: &[(&str, &str)]) -> Vec<(String, String)> {
    acquired.iter().map(|(id, variant)| (id.to_string(), variant.to_string())).collect()
}

/// A song of the catalog: Apple lists what it has.
fn listed(catalog_id: &str, traits: &[&str]) -> Track {
    Track { audio_traits: traits.iter().map(|name| name.to_string()).collect(), ..track(catalog_id) }
}

/// A player with Dolby Atmos off, the lossless route ready and this quality.
fn at(quality: AudioQuality) -> Harness {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.player.set_mode(AudioMode::Stereo);
    h.player.set_quality(quality);
    h
}

#[test]
fn hi_res_fetches_the_rendition_above_48_khz_and_keeps_it_apart() {
    let h = at(AudioQuality::HiRes);
    h.player.play(vec![listed("1", &["hi-res-lossless", "lossless", "lossy-stereo"]), track("2")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.format, snap.format_detail.as_str()), (Some(AudioFormat::Lossless), HIRES));
    assert_eq!(snap.quality, AudioQuality::HiRes);
    assert_eq!(h.pipeline.acquired()[0], ("1".to_string(), "hires".to_string()));
    assert_eq!(h.pipeline.st().decoded[0], "1.hires.m4a");
    assert_eq!(h.pcm.claimed()[0], "1.hires");
    assert_eq!(h.output.active_name().as_deref(), Some("1.hires:hires.f32le"));
    // the next song is prepared in the same version
    h.wait_until("the next song's prefetch", || h.pipeline.acquisitions("2", "hires") > 0);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn each_quality_asks_for_its_own_version() {
    for (quality, variant, detail, format) in [
        (AudioQuality::Aac, "stereo", AAC, AudioFormat::Stereo),
        (AudioQuality::Lossless, "lossless", LOSSLESS, AudioFormat::Lossless),
        (AudioQuality::HiRes, "hires", HIRES, AudioFormat::Lossless),
    ] {
        let h = at(quality);
        h.player.play(vec![track("1")], 0);
        let snap = h.wait_state(PlayerState::Playing);
        assert_eq!((snap.format, snap.format_detail.as_str()), (Some(format), detail), "{quality}");
        assert_eq!(h.pipeline.acquired(), pairs(&[("1", variant)]), "{quality}");
    }
}

#[test]
fn a_song_without_hi_res_plays_lossless_quietly_and_is_remembered() {
    let h = at(AudioQuality::HiRes);
    h.pipeline.st().no_hires.insert("1".to_string());
    h.player.play(vec![named("1", "Song A")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.format, snap.format_detail.as_str()), (Some(AudioFormat::Lossless), LOSSLESS));
    assert_eq!(h.pipeline.acquired(), pairs(&[("1", "hires"), ("1", "lossless")]));
    assert_eq!(snap.quality, AudioQuality::HiRes, "the setting stays what the user chose");
    // what is known about the song is used: Apple is not asked again
    h.player.set_quality(AudioQuality::Aac);
    h.wait_snapshot("AAC", |snap| snap.format == Some(AudioFormat::Stereo));
    h.player.set_quality(AudioQuality::HiRes);
    h.wait_snapshot("lossless again", |snap| snap.format_detail == LOSSLESS);
    h.settle();
    assert_eq!(h.pipeline.acquisitions("1", "hires"), 1);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_song_without_any_alac_goes_to_aac_after_one_question() {
    let h = at(AudioQuality::HiRes);
    h.pipeline.st().no_lossless.insert("1".to_string());
    h.player.play(vec![named("1", "Song A")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.format, snap.format_detail.as_str()), (Some(AudioFormat::Stereo), AAC));
    assert_eq!(h.pipeline.acquired(), pairs(&[("1", "hires"), ("1", "stereo")]));
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn the_audio_traits_spare_the_attempts_that_cannot_succeed() {
    let h = at(AudioQuality::HiRes);
    h.player.play(vec![listed("1", &["lossless", "lossy-stereo"])], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format_detail, LOSSLESS);
    assert_eq!(h.pipeline.acquired(), pairs(&[("1", "lossless")]));

    let h = at(AudioQuality::HiRes);
    h.player.play(vec![listed("2", &["lossy-stereo"])], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format_detail, AAC);
    assert_eq!(h.pipeline.acquired(), pairs(&[("2", "stereo")]));

    // "hi-res-lossless" alone also means there is a lossless version
    let h = at(AudioQuality::Lossless);
    h.player.play(vec![listed("3", &["hi-res-lossless"])], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!(snap.format_detail, LOSSLESS);
    assert_eq!(h.pipeline.acquired(), pairs(&[("3", "lossless")]));
}

#[test]
fn a_failing_route_says_so_and_plays_what_is_left() {
    let h = at(AudioQuality::HiRes);
    h.pipeline.st().route_failure = Some("the lossless service does not answer".to_string());
    h.player.play(vec![named("1", "Song A")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.format, snap.format_detail.as_str()), (Some(AudioFormat::Stereo), AAC));
    assert_eq!(h.pipeline.acquired(), pairs(&[("1", "hires"), ("1", "lossless"), ("1", "stereo")]));
    let notices: Vec<String> = h.errors().into_iter().map(|(message, _, _)| message).collect();
    assert_eq!(
        notices,
        [
            "Song A: the lossless service does not answer; playing the lossless version",
            "Song A: the lossless service does not answer; playing the AAC version",
        ]
    );
}

#[test]
fn changing_the_quality_swaps_the_version_at_the_position() {
    let h = at(AudioQuality::Lossless);
    h.player.play(vec![track("1")], 0);
    h.wait_snapshot("lossless", |snap| snap.format_detail == LOSSLESS);
    h.output.set_position(40 * RATE);
    h.player.set_quality(AudioQuality::HiRes);
    h.wait_snapshot("Hi-Res", |snap| snap.format_detail == HIRES && !snap.switching);
    assert_eq!(h.output.set_sources().pop().unwrap(), (Some("1.hires:hires.f32le".to_string()), 40 * RATE));
    assert_eq!(h.snapshot().state, PlayerState::Playing);
    // both versions stay prepared: going back asks for nothing
    h.player.set_quality(AudioQuality::Lossless);
    h.wait_snapshot("lossless again", |snap| snap.format_detail == LOSSLESS);
    assert_eq!(h.output.active_name().as_deref(), Some("1.lossless:lossless.f32le"));
    h.player.set_quality(AudioQuality::HiRes);
    h.wait_snapshot("Hi-Res again", |snap| snap.format_detail == HIRES);
    assert_eq!((h.pipeline.acquisitions("1", "lossless"), h.pipeline.acquisitions("1", "hires")), (1, 1));
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn a_switch_to_hi_res_that_the_song_cannot_follow_changes_nothing_audible() {
    let h = at(AudioQuality::Lossless);
    h.pipeline.st().no_hires.insert("1".to_string());
    h.player.play(vec![named("1", "Song A")], 0);
    h.wait_snapshot("lossless", |snap| snap.format_detail == LOSSLESS);
    h.output.set_position(10 * RATE);
    let sources = h.output.set_sources().len();
    h.player.set_quality(AudioQuality::HiRes);
    h.wait_until("the attempt", || h.pipeline.acquisitions("1", "hires") == 1);
    h.wait_snapshot("the attempt's end", |snap| !snap.switching);
    h.settle();
    let snap = h.snapshot();
    assert_eq!((snap.format_detail.as_str(), snap.quality), (LOSSLESS, AudioQuality::HiRes));
    assert_eq!(h.output.set_sources().len(), sources, "the output was left alone");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[test]
fn without_the_route_every_quality_plays_aac_and_the_reason_is_there() {
    let h = Harness::new();
    h.pipeline.st().lossless_problem = Some("Lossless needs Apple's own sign-in".to_string());
    h.player.set_mode(AudioMode::Stereo);
    h.player.set_quality(AudioQuality::HiRes);
    h.player.play(vec![track("1")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.format, snap.format_detail.as_str()), (Some(AudioFormat::Stereo), AAC));
    assert_eq!(h.pipeline.acquired(), pairs(&[("1", "stereo")]));
    assert!(!snap.lossless_available);
    assert_eq!(snap.quality, AudioQuality::HiRes, "the choice is kept for when the route is back");
    assert_eq!(snap.lossless_problem.as_deref(), Some("Lossless needs Apple's own sign-in"));
    // no reason while the route works
    let h = at(AudioQuality::Lossless);
    h.pipeline.st().lossless_problem = Some("not asked".to_string());
    assert_eq!(h.snapshot().lossless_problem, None);
}

#[test]
fn a_song_in_dolby_atmos_is_not_touched_by_the_quality() {
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.player.play(vec![track("1")], 0);
    let snap = h.wait_snapshot("Dolby Atmos", |snap| snap.format == Some(AudioFormat::Atmos));
    assert_eq!(snap.quality, AudioQuality::Lossless);
    h.player.set_quality(AudioQuality::HiRes);
    h.settle();
    assert_eq!(h.snapshot().format, Some(AudioFormat::Atmos));
    assert_eq!(h.pipeline.acquired(), pairs(&[("1", "atmos")]));
    // and the 2-channel version it would fall back to is the one the quality says
    let h = Harness::new();
    h.pipeline.st().lossless_ok = true;
    h.pipeline.st().no_atmos.insert("2".to_string());
    h.player.set_quality(AudioQuality::HiRes);
    h.player.play(vec![named("2", "Song B")], 0);
    let snap = h.wait_state(PlayerState::Playing);
    assert_eq!((snap.mode, snap.format_detail.as_str()), (AudioMode::Atmos, HIRES));
    assert_eq!(h.pipeline.acquired(), pairs(&[("2", "atmos"), ("2", "hires")]));
}

#[test]
fn the_quality_survives_a_restart() {
    let h = at(AudioQuality::HiRes);
    h.player.play(vec![track("1")], 0);
    h.wait_state(PlayerState::Playing);
    h.player.shutdown();
    let again = Harness::with(Setup { state_path: Some(h.state_path.clone()), ..Setup::default() });
    assert_eq!(again.player.quality(), AudioQuality::HiRes);
}
