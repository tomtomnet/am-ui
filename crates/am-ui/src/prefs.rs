//! Preferences and About.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use amui::types::AudioMode;

use crate::app::ctx;
use crate::data::Region;
use crate::refresh::Refreshing;
use crate::signal::background;
use crate::widgets::region_picker;

fn switch(title: &str, subtitle: &str, active: bool, changed: impl Fn(bool) + 'static) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_active(active);
    row.connect_active_notify(move |row| changed(row.is_active()));
    row
}

/// The appearance the user chose: "system", "dark" or "light".
pub fn apply_appearance(choice: &str) {
    adw::StyleManager::default().set_color_scheme(match choice {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::PreferDark,
    });
}

/// Show another region's catalog (`None`: the account's) from now on.
fn set_region(code: Option<String>) {
    let music = ctx().music.clone();
    background(
        move || music.set_catalog_region(code.as_deref()).map(|()| music.refresh()),
        |done| {
            let ctx = ctx();
            match done {
                // the pages show the other catalog from now on
                Ok(()) => ctx.refreshed.emit(&Refreshing::unobserved()),
                Err(problem) => ctx.toast.emit(&problem.message),
            }
        },
    );
}

/// Which country's catalog search, New and the catalog pages show: the account's, or another.
/// The row names it; activating it opens the list of the regions.
fn region_row() -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_title("Catalog Region");
    row.set_subtitle("Search and browse the catalog of another country");
    row.set_activatable(true);
    row.set_sensitive(false);
    // a long name takes a second line
    let value = gtk::Label::new(None);
    value.add_css_class("dim-label");
    value.set_wrap(true);
    value.set_xalign(1.0);
    value.set_justify(gtk::Justification::Right);
    value.set_max_width_chars(24);
    row.add_suffix(&value);
    row.add_suffix(&gtk::Image::from_icon_name("am-go-next-symbolic"));

    // the account's region and the one chosen come from the service
    let known: Rc<RefCell<Option<Region>>> = Rc::default();
    let (music, weak, answer) = (ctx().music.clone(), row.downgrade(), known.clone());
    let (shown, value_weak) = (known.clone(), value.downgrade());
    background(
        move || music.catalog_region(),
        move |region| {
            let (Some(row), Some(value), Ok(region)) = (weak.upgrade(), value_weak.upgrade(), region) else { return };
            value.set_text(&region_picker::label(&region));
            answer.replace(Some(region));
            row.set_sensitive(true);
        },
    );
    row.connect_activated(move |row| {
        let Some(region) = shown.borrow().clone() else { return };
        let (known, value) = (known.clone(), value.clone());
        region_picker::show(row, &region, move |code| {
            let mut region = known.borrow_mut();
            let Some(region) = region.as_mut().filter(|region| region.chosen != code) else { return };
            region.chosen = code.clone();
            value.set_text(&region_picker::label(region));
            set_region(code);
        });
    });
    row
}

pub fn show_preferences(parent: &impl IsA<gtk::Widget>) {
    let ctx = ctx();
    let view = ctx.player.view();

    let playback = adw::PreferencesGroup::new();
    playback.set_title("Playback");
    playback.add(&switch(
        "Dolby Atmos",
        "Play the Atmos version when a song has one",
        view.mode == AudioMode::Atmos,
        |on| crate::app::ctx().player.playback.set_mode(if on { AudioMode::Atmos } else { AudioMode::Stereo }),
    ));
    playback.add(&crate::widgets::sound_menu::quality_row(|row, note| row.set_subtitle(note)));
    playback.add(&switch("Sound Check", "Play every song at a similar loudness", view.sound_check, |on| {
        crate::app::ctx().player.playback.set_sound_check(on)
    }));
    playback.add(&switch("Autoplay", "Keep playing similar music when the queue ends", view.autoplay, |on| {
        crate::app::ctx().player.playback.set_autoplay(on)
    }));

    let appearance = adw::ComboRow::new();
    appearance.set_title("Appearance");
    appearance.set_model(Some(&gtk::StringList::new(&["Follow the Desktop", "Dark", "Light"])));
    let choices = ["system", "dark", "light"];
    let current = ctx.settings.text("appearance", "system");
    appearance.set_selected(choices.iter().position(|choice| *choice == current).unwrap_or(0) as u32);
    appearance.connect_selected_notify(move |row| {
        let choice = choices[(row.selected() as usize).min(2)];
        crate::app::ctx().settings.set("appearance", choice);
        apply_appearance(choice);
    });
    let look = adw::PreferencesGroup::new();
    look.set_title("Window");
    look.add(&appearance);

    let refresh = adw::ButtonRow::new();
    refresh.connect_activated(|row| {
        let _ = row.activate_action("win.refresh", None);
    });
    // what a running refresh looks like here: the window's spinner is behind this dialog
    let follow = |row: &adw::ButtonRow, running: bool| {
        row.set_title(if running { "Updating Library…" } else { "Refresh Library Now" });
        row.set_sensitive(!running);
    };
    follow(&refresh, ctx.refresh.is_running());
    ctx.refresh.running.connect(&refresh, move |row, running| follow(row, *running));
    let library = adw::PreferencesGroup::new();
    library.set_title("Library");
    library.add(&region_row());
    library.add(&refresh);

    let page = adw::PreferencesPage::new();
    page.set_title("General");
    page.set_icon_name(Some("am-preferences-system-symbolic"));
    page.add(&playback);
    page.add(&look);
    page.add(&library);
    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    // how a refresh ended, in the dialog that covers the window's messages
    ctx.refresh.finished.connect(&dialog, |dialog, (message, updated)| {
        let toast = adw::Toast::builder()
            .title(message.as_str())
            .use_markup(false)
            .timeout(if *updated { 3 } else { 6 })
            .build();
        dialog.add_toast(toast);
    });
    dialog.present(Some(parent));
}

pub fn show_about(parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AboutDialog::new();
    dialog.set_application_name("Music");
    dialog.set_application_icon(crate::APP_ID);
    // the commit tells which build this is ("+": with changes that were not committed)
    dialog.set_version(concat!(env!("CARGO_PKG_VERSION"), " (", env!("AMUI_BUILD"), ")"));
    dialog.set_comments("A native Apple Music client for Linux, with Dolby Atmos and lossless playback.");
    dialog.present(Some(parent));
}
