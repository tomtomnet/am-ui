//! Preferences and About.

use adw::prelude::*;
use amui::types::AudioMode;

use crate::app::ctx;

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

/// Which country's catalog search, New and the catalog pages show: the account's, or another.
fn region_row() -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title("Catalog Region");
    row.set_subtitle("Search and browse the catalog of another country");
    row.set_sensitive(false);
    let names = gtk::StringList::new(&["Account"]);
    for (_, name) in crate::regions::REGIONS {
        names.append(name);
    }
    row.set_model(Some(&names));
    // typing finds a country
    let text = gtk::PropertyExpression::new(gtk::StringObject::static_type(), None::<&gtk::Expression>, "string");
    row.set_expression(Some(&text));
    row.set_enable_search(true);

    // the account's region and the one chosen come from the service
    let music = ctx().music.clone();
    let weak = row.downgrade();
    crate::signal::background(
        move || music.catalog_region(),
        move |region| {
            let Some(row) = weak.upgrade() else { return };
            let Ok(region) = region else { return };
            names.splice(0, 1, &[&format!("Account ({})", crate::regions::name(&region.account))]);
            let chosen = region.chosen.as_deref().and_then(|code| {
                crate::regions::REGIONS.iter().position(|(known, _)| *known == code).map(|index| index + 1)
            });
            row.set_selected(chosen.unwrap_or(0) as u32);
            row.set_sensitive(true);
            row.connect_selected_notify(|row| {
                let code = match row.selected() as usize {
                    0 => None,
                    index => crate::regions::REGIONS.get(index - 1).map(|(code, _)| code.to_string()),
                };
                let music = ctx().music.clone();
                crate::signal::background(
                    move || music.set_catalog_region(code.as_deref()).map(|()| music.refresh()),
                    |done| {
                        let ctx = ctx();
                        match done {
                            // the pages show the other catalog from now on
                            Ok(()) => ctx.refreshed.emit(&()),
                            Err(problem) => ctx.toast.emit(&problem.message),
                        }
                    },
                );
            });
        },
    );
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
    let lossless = switch("Lossless", "Play the lossless version instead of AAC", view.lossless, |on| {
        crate::app::ctx().player.playback.set_lossless(on)
    });
    lossless.set_sensitive(view.lossless_available);
    playback.add(&lossless);
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
    refresh.set_title("Refresh Library Now");
    refresh.connect_activated(|row| {
        let _ = row.activate_action("win.refresh", None);
    });
    let library = adw::PreferencesGroup::new();
    library.set_title("Library");
    library.add(&region_row());
    library.add(&refresh);

    let page = adw::PreferencesPage::new();
    page.set_title("General");
    page.set_icon_name(Some("preferences-system-symbolic"));
    page.add(&playback);
    page.add(&look);
    page.add(&library);
    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(Some(parent));
}

pub fn show_about(parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AboutDialog::new();
    dialog.set_application_name("Music");
    dialog.set_application_icon("audio-x-generic");
    dialog.set_version(env!("CARGO_PKG_VERSION"));
    dialog.set_comments("A native Apple Music client for the GNOME desktop, with Dolby Atmos and lossless playback.");
    dialog.present(Some(parent));
}
