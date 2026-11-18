//! The sound settings of the player bar: the audio quality and Dolby Atmos, then how Dolby Atmos
//! becomes stereo (a choice of three, in sight), then the rest. The menu is as tall as a window
//! of ordinary height allows without scrolling; a lower window scrolls it.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use amui::types::{AudioMode, AudioQuality};
use gtk::glib;

use crate::app::ctx;
use crate::playback::PlayerView;

/// The choices of the quality: what it is called, and what it is.
const QUALITIES: [(AudioQuality, &str, &str); 3] = [
    (AudioQuality::Aac, "High Quality", "AAC 256 kbps"),
    (AudioQuality::Lossless, "Lossless", "ALAC up to 24-bit/48 kHz"),
    (AudioQuality::HiRes, "Hi-Res Lossless", "ALAC up to 24-bit/192 kHz"),
];

/// For a player that names no reason of its own.
const NO_LOSSLESS: &str = "Lossless is not available";

fn switch_row(title: &str) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    row
}

/// What a downmix profile sounds like, in a short line.
fn profile_subtitle(key: &str) -> &'static str {
    match key {
        "peqdb" => "Every channel at full level, LFE boosted",
        "apple" => "macOS's own fold-down, without the LFE",
        "itu" => "The broadcast standard",
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

/// The place of a quality in the list.
fn quality_position(quality: AudioQuality) -> u32 {
    QUALITIES.iter().position(|(known, _, _)| *known == quality).unwrap_or(0) as u32
}

/// The quality the list shows: what is chosen, as far as it can be had. The choice itself is kept
/// for when the lossless route is back.
fn shown_quality(view: &PlayerView) -> AudioQuality {
    if view.lossless_available { view.quality } else { AudioQuality::Aac }
}

/// The line that goes with the quality: what plays, and why lossless cannot be chosen when it
/// cannot. Empty when there is nothing to say.
pub fn quality_note(view: &PlayerView) -> String {
    let mut lines = Vec::new();
    if view.format.is_some() && !view.format_detail.is_empty() {
        lines.push(format!("Playing: {}", view.format_detail));
    }
    if !view.lossless_available {
        lines.push(view.lossless_problem.clone().unwrap_or_else(|| NO_LOSSLESS.to_string()));
    }
    lines.join("\n")
}

/// The rows of the open list: name, what it is, a check mark on the current one. The choices that
/// need the lossless route cannot be taken without it.
fn quality_items(lossless: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        let what = gtk::Label::new(None);
        what.set_xalign(0.0);
        what.add_css_class("caption");
        what.add_css_class("dim-label");
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.append(&name);
        text.append(&what);
        let check = gtk::Image::from_icon_name("am-object-select-symbolic");
        item.bind_property("selected", &check, "visible").sync_create().build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&text);
        row.append(&check);
        item.set_child(Some(&row));
    });
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let Some((quality, name, what)) = QUALITIES.get(item.position() as usize) else { return };
        let Some(row) = item.child() else { return };
        let labels = row.first_child().and_then(|text| {
            let first = text.first_child()?;
            let second = first.next_sibling()?;
            Some((first.downcast::<gtk::Label>().ok()?, second.downcast::<gtk::Label>().ok()?))
        });
        if let Some((first, second)) = labels {
            first.set_text(name);
            second.set_text(what);
        }
        let open = lossless || *quality == AudioQuality::Aac;
        item.set_activatable(open);
        item.set_selectable(open);
        item.set_focusable(open);
        row.set_sensitive(open);
    });
    factory
}

/// "Audio Quality": a drop-down of the qualities that follows the player's setting and sets it.
/// `note` gets the row and what goes with it (`quality_note`) now and whenever the player changes.
pub fn quality_row(note: impl Fn(&adw::ComboRow, &str) + 'static) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title("Audio Quality");
    // what goes with it may come from the engine: text, not markup
    row.set_use_markup(false);
    let names: Vec<&str> = QUALITIES.iter().map(|(_, name, _)| *name).collect();
    row.set_model(Some(&gtk::StringList::new(&names)));
    let view = ctx().player.view();
    row.set_list_factory(Some(&quality_items(view.lossless_available)));
    row.set_selected(quality_position(shown_quality(&view)));
    note(&row, &quality_note(&view));

    // while the row is set from the player, nothing goes back to it
    let updating = Rc::new(Cell::new(false));
    let lossless = Cell::new(view.lossless_available);
    let setting = updating.clone();
    ctx().player.changed.connect(&row, move |row, _| {
        let view = crate::app::ctx().player.view();
        setting.set(true);
        row.set_selected(quality_position(shown_quality(&view)));
        setting.set(false);
        if lossless.replace(view.lossless_available) != view.lossless_available {
            row.set_list_factory(Some(&quality_items(view.lossless_available)));
        }
        note(row, &quality_note(&view));
    });
    row.connect_selected_notify(move |row| {
        if let Some((quality, _, _)) = QUALITIES.get(row.selected() as usize)
            && !updating.get()
        {
            crate::app::ctx().player.playback.set_quality(*quality);
        }
    });
    row
}

pub fn button() -> gtk::MenuButton {
    let note = gtk::Label::new(None);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    note.add_css_class("caption");
    note.add_css_class("dim-label");
    note.add_css_class("sound-note");
    let quality = {
        let note = note.clone();
        quality_row(move |_, text| {
            note.set_text(text);
            note.set_visible(!text.is_empty());
        })
    };
    let atmos = switch_row("Dolby Atmos");
    let sound_check = switch_row("Sound Check");
    let fallback = switch_row("Stereo When There Is No Atmos");
    fallback.set_tooltip_text(Some("Otherwise songs without a Dolby Atmos version are skipped"));

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
    playback.add(&quality);
    playback.add(&atmos);
    let top = gtk::Box::new(gtk::Orientation::Vertical, 6);
    top.append(&playback);
    top.append(&note);
    // always there: how Dolby Atmos reaches two ears is what this menu is most often opened for
    let downmix = adw::PreferencesGroup::new();
    downmix.set_title("Dolby Atmos Downmix");
    for (row, _) in &profile_rows {
        downmix.add(row);
    }
    if profile_rows.is_empty() {
        let none = adw::ActionRow::new();
        none.set_title("No Downmix Profiles");
        none.set_subtitle("The player offers none right now");
        downmix.add(&none);
    }
    let output = adw::PreferencesGroup::new();
    output.set_title("Output");
    output.add(&preamp_row);
    output.add(&sound_check);
    output.add(&fallback);
    output.add(&crate::widgets::equalizer::menu_row());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.add_css_class("sound-menu");
    content.append(&top);
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
    let updating = Rc::new(Cell::new(false));
    let update = {
        let (atmos, sound_check, fallback, preamp, updating) =
            (atmos.clone(), sound_check.clone(), fallback.clone(), preamp.clone(), updating.clone());
        let checks: Vec<gtk::CheckButton> = profile_rows.iter().map(|(_, check)| check.clone()).collect();
        let profiles = profiles.clone();
        move || {
            let view = crate::app::ctx().player.view();
            updating.set(true);
            atmos.set_active(view.mode == AudioMode::Atmos);
            sound_check.set_active(view.sound_check);
            fallback.set_active(view.stereo_fallback);
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

    let send = |row: &adw::SwitchRow, updating: &Rc<Cell<bool>>, apply: fn(&dyn crate::playback::Playback, bool)| {
        let updating = updating.clone();
        row.connect_active_notify(move |row| {
            if !updating.get() {
                apply(crate::app::ctx().player.playback.as_ref(), row.is_active());
            }
        });
    };
    send(&atmos, &updating, |playback, on| playback.set_mode(if on { AudioMode::Atmos } else { AudioMode::Stereo }));
    send(&sound_check, &updating, |playback, on| playback.set_sound_check(on));
    send(&fallback, &updating, |playback, on| playback.set_stereo_fallback(on));
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
        let shown = Cell::new(false);
        button.connect_map(move |button| {
            if !shown.replace(true) {
                let button = button.clone();
                glib::idle_add_local_once(move || button.popup());
            }
        });
    }
    button
}

#[cfg(test)]
mod tests {
    use super::*;
    use amui::types::AudioFormat;

    #[test]
    fn the_list_offers_the_three_qualities_in_their_order() {
        let names: Vec<&str> = QUALITIES.iter().map(|(_, name, _)| *name).collect();
        assert_eq!(names, ["High Quality", "Lossless", "Hi-Res Lossless"]);
        for (position, (quality, _, _)) in QUALITIES.iter().enumerate() {
            assert_eq!(quality_position(*quality), position as u32);
        }
    }

    #[test]
    fn without_the_route_the_list_shows_what_can_be_had() {
        let mut view = PlayerView { quality: AudioQuality::HiRes, lossless_available: true, ..PlayerView::default() };
        assert_eq!(shown_quality(&view), AudioQuality::HiRes);
        view.lossless_available = false;
        assert_eq!(shown_quality(&view), AudioQuality::Aac);
    }

    #[test]
    fn the_note_says_what_plays_and_why_lossless_cannot_be_chosen() {
        let mut view = PlayerView { lossless_available: true, ..PlayerView::default() };
        assert_eq!(quality_note(&view), "", "nothing plays and nothing is wrong");
        view.format = Some(AudioFormat::Lossless);
        view.format_detail = "Hi-Res Lossless · ALAC 24-bit/96 kHz · resampled to 48 kHz".into();
        assert_eq!(quality_note(&view), "Playing: Hi-Res Lossless · ALAC 24-bit/96 kHz · resampled to 48 kHz");
        view.lossless_available = false;
        view.lossless_problem = Some("Lossless needs Apple's own sign-in: use Sign In".into());
        view.format = Some(AudioFormat::Stereo);
        view.format_detail = "AAC 256 kbps".into();
        assert_eq!(quality_note(&view), "Playing: AAC 256 kbps\nLossless needs Apple's own sign-in: use Sign In");
        view.format = None;
        view.format_detail.clear();
        view.lossless_problem = None;
        assert_eq!(quality_note(&view), NO_LOSSLESS);
    }
}
