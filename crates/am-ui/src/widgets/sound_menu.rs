//! The sound settings of the player bar: Dolby Atmos, lossless, Sound Check, the downmix
//! profile (a choice of three, in sight) and the output's preamp.

use adw::prelude::*;
use amui::types::AudioMode;
use gtk::glib;

use crate::app::ctx;

fn switch_row(title: &str, subtitle: &str) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

/// What a downmix profile sounds like, in a line.
fn profile_subtitle(key: &str) -> &'static str {
    match key {
        "peqdb" => "Every channel at full level, the LFE boosted",
        "apple" => "macOS's own fold-down, without the LFE",
        "itu" => "The broadcast standard: surrounds and heights lower",
        _ => "",
    }
}

/// One row per downmix profile, with a radio button; the rows and their buttons.
fn profile_rows(profiles: &[crate::playback::Profile]) -> Vec<(adw::ActionRow, gtk::CheckButton)> {
    let mut rows: Vec<(adw::ActionRow, gtk::CheckButton)> = Vec::new();
    for profile in profiles {
        let check = gtk::CheckButton::new();
        check.set_valign(gtk::Align::Center);
        if let Some((_, first)) = rows.first() {
            check.set_group(Some(first));
        }
        let row = adw::ActionRow::new();
        row.set_title(&profile.label);
        row.set_subtitle(profile_subtitle(&profile.key));
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        rows.push((row, check));
    }
    rows
}

pub fn button() -> gtk::MenuButton {
    let atmos = switch_row("Dolby Atmos", "Play the Atmos version when a song has one");
    let lossless = switch_row("Lossless", "Play the lossless version instead of AAC");
    let sound_check = switch_row("Sound Check", "Play every song at a similar loudness");
    let fallback = switch_row("Stereo When There Is No Atmos", "Otherwise such songs are skipped");
    let autoplay = switch_row("Autoplay", "Keep playing similar music when the queue ends");

    let profiles = ctx().player.playback.downmix_profiles();
    let profile_rows = profile_rows(&profiles);

    let preamp = gtk::Scale::with_range(gtk::Orientation::Horizontal, -30.0, 6.0, 0.5);
    preamp.set_hexpand(true);
    preamp.set_draw_value(true);
    preamp.set_value_pos(gtk::PositionType::Right);
    preamp.set_format_value_func(|_, value| format!("{value:+.1} dB"));
    preamp.set_size_request(200, -1);
    let preamp_row = adw::ActionRow::new();
    preamp_row.set_title("Preamp");
    preamp_row.add_suffix(&preamp);

    let playback = adw::PreferencesGroup::new();
    playback.add(&atmos);
    playback.add(&lossless);
    playback.add(&sound_check);
    playback.add(&autoplay);
    let downmix = adw::PreferencesGroup::new();
    downmix.set_title("Downmix");
    downmix.set_description(Some("How the Dolby Atmos channels become stereo"));
    for (row, _) in &profile_rows {
        downmix.add(row);
    }
    downmix.set_visible(!profile_rows.is_empty());
    let output = adw::PreferencesGroup::new();
    output.set_title("Output");
    output.add(&preamp_row);
    output.add(&fallback);
    output.add(&crate::widgets::equalizer::menu_row());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.add_css_class("sound-menu");
    content.append(&playback);
    content.append(&downmix);
    content.append(&output);
    content.set_size_request(400, -1);

    // a window lower than the menu scrolls it
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_child(Some(&content));
    let popover = gtk::Popover::new();
    popover.set_child(Some(&scrolled));
    popover.connect_show(move |popover| {
        let room = popover.parent().and_then(|parent| parent.root()).map_or(0, |root| root.height());
        scrolled.set_max_content_height((room - 120).max(240));
    });
    let button = gtk::MenuButton::new();
    button.set_icon_name("am-sound-symbolic");
    button.set_tooltip_text(Some("Sound"));
    button.add_css_class("flat");
    button.add_css_class("circular");
    button.set_valign(gtk::Align::Center);
    button.set_popover(Some(&popover));

    // the rows show the player's settings; while they are being set from the player, changes
    // are not sent back to it
    let updating = std::rc::Rc::new(std::cell::Cell::new(false));
    let update = {
        let (atmos, lossless, sound_check, fallback, autoplay, preamp, updating) = (
            atmos.clone(),
            lossless.clone(),
            sound_check.clone(),
            fallback.clone(),
            autoplay.clone(),
            preamp.clone(),
            updating.clone(),
        );
        let checks: Vec<gtk::CheckButton> = profile_rows.iter().map(|(_, check)| check.clone()).collect();
        let profiles = profiles.clone();
        move || {
            let view = crate::app::ctx().player.view();
            updating.set(true);
            atmos.set_active(view.mode == AudioMode::Atmos);
            lossless.set_active(view.lossless);
            lossless.set_sensitive(view.lossless_available);
            lossless.set_subtitle(if view.lossless_available {
                "Play the lossless version instead of AAC"
            } else {
                "Needs the lossless route and Apple's own sign-in"
            });
            sound_check.set_active(view.sound_check);
            fallback.set_active(view.stereo_fallback);
            autoplay.set_active(view.autoplay);
            for (profile, check) in profiles.iter().zip(&checks) {
                check.set_active(Some(&profile.key) == view.downmix_profile.as_ref());
            }
            if let Some(db) = view.preamp
                && (preamp.value() - db).abs() > 0.05
            {
                preamp.set_value(db);
            }
            updating.set(false);
        }
    };
    update();
    ctx().player.changed.connect(&button, move |_, _| update());

    let send = |row: &adw::SwitchRow,
                updating: &std::rc::Rc<std::cell::Cell<bool>>,
                apply: fn(&dyn crate::playback::Playback, bool)| {
        let updating = updating.clone();
        row.connect_active_notify(move |row| {
            if !updating.get() {
                apply(crate::app::ctx().player.playback.as_ref(), row.is_active());
            }
        });
    };
    send(&atmos, &updating, |playback, on| playback.set_mode(if on { AudioMode::Atmos } else { AudioMode::Stereo }));
    send(&lossless, &updating, |playback, on| playback.set_lossless(on));
    send(&sound_check, &updating, |playback, on| playback.set_sound_check(on));
    send(&fallback, &updating, |playback, on| playback.set_stereo_fallback(on));
    send(&autoplay, &updating, |playback, on| playback.set_autoplay(on));
    for (profile, (_, check)) in profiles.iter().zip(&profile_rows) {
        let (updating, key) = (updating.clone(), profile.key.clone());
        check.connect_toggled(move |check| {
            if !updating.get() && check.is_active() {
                crate::app::ctx().player.playback.set_downmix_profile(&key);
            }
        });
    }
    {
        let updating = updating.clone();
        preamp.connect_change_value(move |_, _, value| {
            if !updating.get() {
                crate::app::ctx().player.playback.set_preamp(value);
            }
            glib::Propagation::Proceed
        });
    }
    // `--show sound`: open at start (for screenshots)
    if ctx().options.show.as_deref() == Some("sound") {
        let shown = std::cell::Cell::new(false);
        button.connect_map(move |button| {
            if !shown.replace(true) {
                let button = button.clone();
                glib::idle_add_local_once(move || button.popup());
            }
        });
    }
    button
}
