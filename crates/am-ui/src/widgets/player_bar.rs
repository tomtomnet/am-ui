//! The player bar along the bottom of the window: what plays, the transport, the progress, and
//! the buttons for lyrics, the queue, the sound settings and the volume.

use std::time::Duration;

use adw::prelude::*;
use amui::types::{AudioFormat, PlayerState, RepeatMode};
use gtk::{gdk, glib};

use crate::actions;
use crate::app::ctx;
use crate::widgets::seek_bar::SeekBar;
use crate::widgets::thumb::Thumb;
use crate::widgets::{clock, sound_menu};

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tip));
    button.set_valign(gtk::Align::Center);
    button.set_focus_on_click(false);
    button
}

/// A toggle for a view of the window. It is bound to the window's action for that view, which is
/// on while the view shows: the button follows the view however it was opened or closed, and a
/// click opens or closes it.
fn view_toggle(icon: &str, tip: &str, action: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.set_icon_name(icon);
    button.add_css_class("flat");
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tip));
    button.set_valign(gtk::Align::Center);
    button.set_focus_on_click(false);
    button.set_action_name(Some(action));
    button
}

/// A line about the song. It asks for no width of its own (it takes what its part of the bar
/// has), so that a longer name or a loading text does not move the parts beside it.
fn text(class: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_single_line_mode(true);
    label.set_max_width_chars(1);
    label.add_css_class(class);
    label
}

/// A label that does something when it is clicked.
fn link(label: &gtk::Label, action: impl Fn() + 'static) {
    let click = gtk::GestureClick::new();
    click.connect_released(move |_, _, _, _| action());
    label.add_controller(click);
    label.set_cursor_from_name(Some("pointer"));
}

/// "Dolby Atmos", "Lossless", "Hi-Res Lossless"; nothing for plain AAC.
pub fn badge_text(format: Option<AudioFormat>, detail: &str) -> &'static str {
    match format {
        Some(AudioFormat::Atmos) => "Dolby Atmos",
        Some(AudioFormat::Lossless) if detail.starts_with("Hi-Res") => "Hi-Res Lossless",
        Some(AudioFormat::Lossless) => "Lossless",
        _ => "",
    }
}

pub fn build() -> gtk::Widget {
    let ctx = ctx();
    let player = ctx.player.clone();

    // --- what plays
    let art = Thumb::new(44, 6.0);
    let art_button = gtk::Button::new();
    art_button.set_child(Some(&art));
    art_button.add_css_class("flat");
    art_button.add_css_class("art-button");
    art_button.set_tooltip_text(Some("Now Playing"));
    art_button.set_valign(gtk::Align::Center);
    art_button.connect_clicked(|_| crate::app::ctx().show_now_playing.emit(&true));
    let title = text("player-title");
    let subtitle = text("player-subtitle");
    subtitle.add_css_class("dim-label");
    link(&title, || {
        if let Some(track) = crate::app::ctx().player.track() {
            actions::open_album_of(&track);
        }
    });
    link(&subtitle, || {
        if let Some(track) = crate::app::ctx().player.track() {
            actions::open_artist_of(&track);
        }
    });
    let names = gtk::Box::new(gtk::Orientation::Vertical, 1);
    names.set_valign(gtk::Align::Center);
    names.set_hexpand(true);
    names.append(&title);
    names.append(&subtitle);
    let badge = gtk::Label::new(None);
    badge.add_css_class("format-badge");
    badge.set_valign(gtk::Align::Center);
    let star = icon_button("am-non-starred-symbolic", "Favorite");
    star.connect_clicked(|_| {
        let ctx = crate::app::ctx();
        if let Some(track) = ctx.player.track() {
            ctx.favorites.toggle(&ctx.music, &track);
        }
    });
    let start = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    start.append(&art_button);
    start.append(&names);
    start.append(&badge);
    start.append(&star);
    // a right click on the song shows its menu, as in a list
    let menu = gtk::GestureClick::new();
    menu.set_button(gdk::BUTTON_SECONDARY);
    menu.connect_pressed(|gesture, _, x, y| {
        let (Some(widget), Some(track)) = (gesture.widget(), crate::app::ctx().player.track()) else { return };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        crate::menus::track_menu(&crate::menus::TrackTarget::new(&track), &widget, x, y);
    });
    start.add_controller(menu);
    // the song's part and the buttons' part share the room beside the progress alike, whatever
    // they show: the progress stays in the middle and keeps its width
    start.set_size_request(300, -1);
    start.set_hexpand(true);

    // --- transport and progress
    let shuffle = gtk::ToggleButton::new();
    shuffle.set_icon_name("am-media-playlist-shuffle-symbolic");
    shuffle.add_css_class("flat");
    shuffle.add_css_class("circular");
    shuffle.set_tooltip_text(Some("Shuffle"));
    shuffle.set_valign(gtk::Align::Center);
    shuffle.set_focus_on_click(false);
    let previous = icon_button("am-media-skip-backward-symbolic", "Previous");
    let play = icon_button("am-media-playback-start-symbolic", "Play");
    play.add_css_class("play-button");
    let next = icon_button("am-media-skip-forward-symbolic", "Next");
    let repeat = gtk::ToggleButton::new();
    repeat.set_icon_name("am-media-playlist-repeat-symbolic");
    repeat.add_css_class("flat");
    repeat.add_css_class("circular");
    repeat.set_tooltip_text(Some("Repeat"));
    repeat.set_valign(gtk::Align::Center);
    repeat.set_focus_on_click(false);
    previous.connect_clicked(|_| crate::app::ctx().player.playback.previous());
    play.connect_clicked(|_| crate::app::ctx().player.playback.toggle());
    next.connect_clicked(|_| crate::app::ctx().player.playback.next());
    // the buttons show the player's state; a click asks the player, which answers with a state
    shuffle.connect_clicked(|button| {
        let player = crate::app::ctx().player.clone();
        let wanted = !player.view().shuffle;
        button.set_active(player.view().shuffle);
        player.playback.set_shuffle(wanted);
    });
    repeat.connect_clicked(|button| {
        let player = crate::app::ctx().player.clone();
        let current = player.view().repeat;
        button.set_active(current != RepeatMode::Off);
        player.playback.set_repeat(match current {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        });
    });
    let transport = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    transport.set_halign(gtk::Align::Center);
    for button in [
        shuffle.upcast_ref::<gtk::Widget>(),
        previous.upcast_ref(),
        play.upcast_ref(),
        next.upcast_ref(),
        repeat.upcast_ref(),
    ] {
        transport.append(button);
    }
    let elapsed = gtk::Label::new(Some("0:00"));
    let remaining = gtk::Label::new(Some("0:00"));
    for label in [&elapsed, &remaining] {
        label.add_css_class("numeric");
        label.add_css_class("dim-label");
        label.add_css_class("player-time");
        label.set_width_chars(5);
    }
    elapsed.set_xalign(1.0);
    remaining.set_xalign(0.0);
    let seek = SeekBar::new();
    seek.connect_seek(|fraction| {
        let player = crate::app::ctx().player.clone();
        if let Some(duration) = player.duration() {
            player.playback.seek(fraction * duration);
        }
    });
    let progress = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    progress.append(&elapsed);
    progress.append(&seek);
    progress.append(&remaining);
    let center = gtk::Box::new(gtk::Orientation::Vertical, 0);
    center.set_valign(gtk::Align::Center);
    center.set_hexpand(true);
    center.append(&transport);
    center.append(&progress);
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(620);
    clamp.set_child(Some(&center));
    clamp.set_hexpand(true);

    // --- lyrics, queue, sound, volume
    // (the first two show whether their view is open, and open or close it)
    let lyrics = view_toggle("am-lyrics-symbolic", "Lyrics", "win.now-playing");
    let queue = view_toggle("am-queue-symbolic", "Playing Next", "win.queue");
    let sound = sound_menu::button();
    let volume_icon = gtk::Image::from_icon_name("am-audio-volume-high-symbolic");
    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    volume.set_size_request(96, -1);
    volume.set_draw_value(false);
    volume.set_valign(gtk::Align::Center);
    volume.set_tooltip_text(Some("Volume"));
    volume.connect_change_value(|_, _, value| {
        crate::app::ctx().player.playback.set_volume(value.clamp(0.0, 1.0));
        glib::Propagation::Proceed
    });
    let end = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    end.set_halign(gtk::Align::End);
    end.append(&lyrics);
    end.append(&queue);
    end.append(&sound);
    end.append(&volume_icon);
    end.append(&volume);
    end.set_size_request(300, -1);
    end.set_hexpand(true);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    bar.add_css_class("player-bar");
    bar.append(&start);
    bar.append(&clamp);
    bar.append(&end);

    // --- follow the player
    let update = {
        let (art, title, subtitle, badge, star, play, shuffle, repeat, seek, volume, volume_icon) = (
            art.clone(),
            title.clone(),
            subtitle.clone(),
            badge.clone(),
            star.clone(),
            play.clone(),
            shuffle.clone(),
            repeat.clone(),
            seek.clone(),
            volume.clone(),
            volume_icon.clone(),
        );
        let (previous, next) = (previous.clone(), next.clone());
        move || {
            let ctx = crate::app::ctx();
            let view = ctx.player.view();
            let track = view.track.clone();
            art.set_artwork(track.as_ref().and_then(|track| track.artwork.as_ref()));
            title.set_text(track.as_ref().map(|track| track.name.as_str()).unwrap_or("Not Playing"));
            let line = if !view.loading.is_empty() {
                view.loading.clone()
            } else if view.buffering {
                "Buffering…".to_string()
            } else {
                track.as_ref().map(|track| format!("{} — {}", track.artist, track.album)).unwrap_or_default()
            };
            subtitle.set_text(&line);
            subtitle.set_visible(!line.is_empty());
            let text = badge_text(view.format, &view.format_detail);
            badge.set_text(text);
            badge.set_visible(!text.is_empty());
            badge.set_tooltip_text(Some(&view.format_detail));
            if view.switching {
                badge.add_css_class("switching");
            } else {
                badge.remove_css_class("switching");
            }
            let favorite = track.as_ref().is_some_and(|track| ctx.favorites.is_favorite(track));
            star.set_icon_name(if favorite { "am-starred-symbolic" } else { "am-non-starred-symbolic" });
            star.set_tooltip_text(Some(if favorite { "Undo Favorite" } else { "Favorite" }));
            star.set_visible(track.is_some());
            if favorite {
                star.add_css_class("favorite");
            } else {
                star.remove_css_class("favorite");
            }
            let playing = matches!(view.state, PlayerState::Playing | PlayerState::Loading);
            play.set_icon_name(if playing { "am-media-playback-pause-symbolic" } else { "am-media-playback-start-symbolic" });
            play.set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
            for button in [&play, &previous, &next] {
                button.set_sensitive(track.is_some());
            }
            shuffle.set_active(view.shuffle);
            repeat.set_active(view.repeat != RepeatMode::Off);
            repeat.set_icon_name(if view.repeat == RepeatMode::One {
                "am-media-playlist-repeat-song-symbolic"
            } else {
                "am-media-playlist-repeat-symbolic"
            });
            seek.set_enabled(track.is_some());
            if (volume.value() - view.volume).abs() > 0.004 {
                volume.set_value(view.volume);
            }
            volume_icon.set_icon_name(Some(match view.volume {
                v if v <= 0.0 => "am-audio-volume-muted-symbolic",
                v if v < 0.34 => "am-audio-volume-low-symbolic",
                v if v < 0.67 => "am-audio-volume-medium-symbolic",
                _ => "am-audio-volume-high-symbolic",
            }));
        }
    };
    update();
    {
        let update_favorite = update.clone();
        ctx.favorites.changed.connect(&bar, move |_, _| update_favorite());
        player.changed.connect(&bar, move |_, _| update());
    }

    // the progress moves a pixel or two per second: ten looks per second are plenty
    let show_progress = {
        let (seek, elapsed, remaining) = (seek.clone(), elapsed.clone(), remaining.clone());
        move || {
            let player = crate::app::ctx().player.clone();
            let duration = player.duration().unwrap_or(0.0);
            let position = match seek.dragging() {
                Some(fraction) => fraction * duration,
                None => player.position(),
            };
            let fraction = if duration > 0.0 { position / duration } else { 0.0 };
            seek.set_progress(fraction, if duration > 0.0 { player.buffered() / duration } else { 0.0 });
            // room for the longest time of the song, so that the progress keeps its width
            let longest = clock(duration).chars().count().max(4) as i32;
            if remaining.width_chars() != longest + 1 {
                elapsed.set_width_chars(longest);
                remaining.set_width_chars(longest + 1);
            }
            let (now, left) = (clock(position), format!("-{}", clock((duration - position).max(0.0))));
            if elapsed.text() != now {
                elapsed.set_text(&now);
            }
            if remaining.text() != left {
                remaining.set_text(&left);
            }
        }
    };
    show_progress();
    {
        let show = show_progress.clone();
        player.position_changed.connect(&bar, move |_, _| show());
        let show = show_progress.clone();
        player.changed.connect(&bar, move |_, _| show());
    }
    let weak = bar.downgrade();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(bar) = weak.upgrade() else { return glib::ControlFlow::Break };
        if bar.is_mapped() && (crate::app::ctx().player.is_playing() || seek.dragging().is_some()) {
            show_progress();
        }
        glib::ControlFlow::Continue
    });
    bar.upcast()
}
