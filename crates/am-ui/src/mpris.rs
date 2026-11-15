//! The desktop's media controls (MPRIS on D-Bus): media keys, the controls in GNOME's panel and
//! on the lock screen show and steer the player.

use std::rc::Rc;

use adw::prelude::*;
use amui::types::{PlayerState, RepeatMode, Track};
use gtk::glib;
use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player as Remote, Time, zbus};

use crate::app::ctx;
use crate::playback::PlayerView;

/// What was last told to the desktop, to tell it only what changed.
#[derive(Default, PartialEq, Clone)]
struct Told {
    key: Option<String>,
    status: Option<PlayerState>,
    repeat: Option<RepeatMode>,
    shuffle: bool,
    volume: f64,
    has_track: bool,
    duration: Option<i64>,
}

fn status_of(state: PlayerState) -> PlaybackStatus {
    match state {
        PlayerState::Playing | PlayerState::Loading => PlaybackStatus::Playing,
        PlayerState::Paused => PlaybackStatus::Paused,
        PlayerState::Idle | PlayerState::Error => PlaybackStatus::Stopped,
    }
}

fn loop_of(repeat: RepeatMode) -> LoopStatus {
    match repeat {
        RepeatMode::Off => LoopStatus::None,
        RepeatMode::All => LoopStatus::Playlist,
        RepeatMode::One => LoopStatus::Track,
    }
}

fn repeat_of(status: LoopStatus) -> RepeatMode {
    match status {
        LoopStatus::None => RepeatMode::Off,
        LoopStatus::Playlist => RepeatMode::All,
        LoopStatus::Track => RepeatMode::One,
    }
}

/// An object path made of the letters and digits of a song's key.
fn track_path(key: &str) -> String {
    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("/dev/amui/Music/track/{}", if safe.is_empty() { "none" } else { &safe })
}

fn metadata_of(track: &Track, duration: Option<f64>) -> Metadata {
    let mut builder =
        Metadata::builder().title(track.name.clone()).artist([track.artist.clone()]).album(track.album.clone());
    if let Ok(path) = zbus::ObjectPath::try_from(track_path(&track.key())) {
        builder = builder.trackid(path);
    }
    if let Some(seconds) = duration {
        builder = builder.length(Time::from_micros((seconds * 1e6) as i64));
    }
    if let Some(artwork) = &track.artwork {
        builder = builder.art_url(artwork.url(512));
    }
    if let Some(number) = track.track_number {
        builder = builder.track_number(number as i32);
    }
    builder.build()
}

async fn tell(remote: &Remote, view: &PlayerView, duration: Option<f64>, told: &mut Told) -> zbus::Result<()> {
    let now = Told {
        key: view.track.as_ref().map(Track::key),
        status: Some(view.state),
        repeat: Some(view.repeat),
        shuffle: view.shuffle,
        volume: view.volume,
        has_track: view.track.is_some(),
        duration: duration.map(|seconds| (seconds * 1000.0) as i64),
    };
    if now.key != told.key || now.duration != told.duration {
        let metadata = view.track.as_ref().map(|track| metadata_of(track, duration)).unwrap_or_default();
        remote.set_metadata(metadata).await?;
        if now.key != told.key {
            remote.seeked(Time::ZERO).await?;
        }
    }
    if now.status != told.status {
        remote.set_playback_status(status_of(view.state)).await?;
    }
    if now.repeat != told.repeat {
        remote.set_loop_status(loop_of(view.repeat)).await?;
    }
    if now.shuffle != told.shuffle {
        remote.set_shuffle(view.shuffle).await?;
    }
    if (now.volume - told.volume).abs() > 0.004 {
        remote.set_volume(view.volume).await?;
    }
    if now.has_track != told.has_track {
        remote.set_can_play(now.has_track).await?;
        remote.set_can_pause(now.has_track).await?;
        remote.set_can_seek(now.has_track).await?;
        remote.set_can_go_next(now.has_track).await?;
        remote.set_can_go_previous(now.has_track).await?;
    }
    *told = now;
    Ok(())
}

/// Offer the player on the session bus as `org.mpris.MediaPlayer2.<name>`. Without a session bus
/// (or when the name is taken) the application simply has no media keys.
pub fn start(application: &adw::Application, name: &str) {
    let (application, name) = (application.clone(), name.to_string());
    glib::spawn_future_local(async move {
        let built = Remote::builder(&name)
            .identity("Music")
            .desktop_entry(crate::APP_ID)
            .can_raise(true)
            .can_quit(true)
            .can_control(true)
            .build()
            .await;
        let Ok(remote) = built else { return };
        let remote = Rc::new(remote);

        remote.connect_play_pause(|_| ctx().player.playback.toggle());
        remote.connect_play(|_| {
            let player = ctx().player.clone();
            if !player.is_playing() {
                player.playback.toggle();
            }
        });
        let pause = || {
            let player = ctx().player.clone();
            if player.is_playing() {
                player.playback.toggle();
            }
        };
        remote.connect_pause(move |_| pause());
        remote.connect_stop(move |_| pause());
        remote.connect_next(|_| ctx().player.playback.next());
        remote.connect_previous(|_| ctx().player.playback.previous());
        remote.connect_seek(|_, offset| {
            let player = ctx().player.clone();
            player.playback.seek((player.position() + offset.as_micros() as f64 / 1e6).max(0.0));
        });
        remote.connect_set_position(|_, _, position| ctx().player.playback.seek(position.as_micros() as f64 / 1e6));
        remote.connect_set_loop_status(|_, status| ctx().player.playback.set_repeat(repeat_of(status)));
        remote.connect_set_shuffle(|_, shuffle| ctx().player.playback.set_shuffle(shuffle));
        remote.connect_set_volume(|_, volume| ctx().player.playback.set_volume(volume.clamp(0.0, 1.0)));
        {
            let raised = application.clone();
            remote.connect_raise(move |_| {
                if let Some(window) = raised.active_window() {
                    window.present();
                }
            });
            let application = application.clone();
            remote.connect_quit(move |_| application.quit());
        }
        glib::spawn_future_local(remote.run());

        // one task tells the desktop about changes, in order
        let (changes, pending) = async_channel::unbounded::<bool>();
        let player = ctx().player.clone();
        {
            let state = changes.clone();
            player.changed.connect_forever(move |_| {
                let _ = state.try_send(false);
            });
            let position = changes.clone();
            player.position_changed.connect_forever(move |_| {
                let _ = position.try_send(true);
            });
        }
        let _ = changes.try_send(false);
        let mut told = Told { volume: -1.0, ..Told::default() };
        let mut expected = 0.0f64;
        while let Ok(position_report) = pending.recv().await {
            let player = ctx().player.clone();
            let position = player.position();
            remote.set_position(Time::from_micros((position * 1e6) as i64));
            if position_report {
                // a report far from where the clock would be is a seek
                if (position - expected).abs() > 1.5
                    && remote.seeked(Time::from_micros((position * 1e6) as i64)).await.is_err()
                {
                    break;
                }
                expected = position + 0.25;
                continue;
            }
            expected = position;
            if tell(&remote, &player.view(), player.duration(), &mut told).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_paths_are_valid_object_paths() {
        for key in ["1234567890", "i.AbC-123", "Artist A/Album A/Song A", ""] {
            let path = track_path(key);
            assert!(zbus::ObjectPath::try_from(path.clone()).is_ok(), "{path}");
        }
        assert_eq!(track_path("i.AbC-123"), "/dev/amui/Music/track/i_AbC_123");
    }

    #[test]
    fn states_map_to_the_desktop_s_words() {
        assert_eq!(status_of(PlayerState::Loading), PlaybackStatus::Playing);
        assert_eq!(status_of(PlayerState::Idle), PlaybackStatus::Stopped);
        for repeat in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(repeat_of(loop_of(repeat)), repeat);
        }
    }
}
