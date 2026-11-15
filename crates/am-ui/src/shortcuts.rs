//! The window's actions and their keyboard shortcuts.
//!
//! Space plays and pauses unless a text field has the keyboard. Plain arrow keys stay with the
//! lists and grids; the player listens to them with Control held.

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::app::ctx;
use crate::dialogs;

const SEEK_STEP: f64 = 10.0;
const VOLUME_STEP: f64 = 0.05;

/// (action, accelerators, title in the shortcuts overview, group there)
const SHORTCUTS: &[(&str, &[&str], &str, &str)] = &[
    ("win.play-pause", &["space"], "Play or Pause", "Playback"),
    ("win.next", &["<Control>Right"], "Next Song", "Playback"),
    ("win.previous", &["<Control>Left"], "Previous Song", "Playback"),
    ("win.seek-forward", &["<Control><Shift>Right"], "Skip Forward 10 Seconds", "Playback"),
    ("win.seek-back", &["<Control><Shift>Left"], "Skip Back 10 Seconds", "Playback"),
    ("win.volume-up", &["<Control>Up"], "Volume Up", "Playback"),
    ("win.volume-down", &["<Control>Down"], "Volume Down", "Playback"),
    ("win.now-playing", &["<Control>l"], "Lyrics and Now Playing", "View"),
    ("win.queue", &["<Control>u"], "Playing Next", "View"),
    ("win.fullscreen", &["F11"], "Full Screen", "View"),
    ("win.back", &["<Alt>Left", "<Control>bracketleft"], "Back", "Navigation"),
    ("win.forward", &["<Alt>Right", "<Control>bracketright"], "Forward", "Navigation"),
    ("win.search", &["<Control>f"], "Search", "Navigation"),
    ("win.new-playlist", &["<Control>n"], "New Playlist", "Library"),
    ("win.refresh", &["F5", "<Control>r"], "Refresh", "Library"),
    ("app.preferences", &["<Control>comma"], "Preferences", "General"),
    ("win.shortcuts", &["<Control>question"], "Keyboard Shortcuts", "General"),
    ("app.quit", &["<Control>q"], "Quit", "General"),
];

fn action(map: &impl IsA<gio::ActionMap>, name: &str, run: impl Fn() + 'static) {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| run());
    map.add_action(&action);
}

/// An action for a view that `signal` shows and hides: it is on while the view shows, activating
/// it shows or hides the view, and it follows whatever else does (so a toggle button bound to it
/// is always right).
fn view_action(
    map: &impl IsA<gio::ActionMap>,
    name: &str,
    signal: fn(&crate::app::Ctx) -> &crate::signal::Signal<bool>,
) {
    let action = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
    action.connect_change_state(move |_, wanted| {
        if let Some(shown) = wanted.and_then(|wanted| wanted.get::<bool>()) {
            signal(&ctx()).emit(&shown);
        }
    });
    signal(&ctx()).connect(&action, |action, shown| action.set_state(&shown.to_variant()));
    map.add_action(&action);
}

/// Whether the keyboard is in a text field.
fn typing(window: &adw::ApplicationWindow) -> bool {
    gtk::prelude::GtkWindowExt::focus(window)
        .is_some_and(|focus| focus.is::<gtk::Text>() || focus.is::<gtk::TextView>())
}

fn show_shortcuts(window: &adw::ApplicationWindow) {
    let dialog = adw::ShortcutsDialog::new();
    let mut groups: Vec<(&str, adw::ShortcutsSection)> = Vec::new();
    for (_, accelerators, title, group) in SHORTCUTS {
        let position = match groups.iter().position(|(name, _)| name == group) {
            Some(position) => position,
            None => {
                groups.push((group, adw::ShortcutsSection::new(Some(group))));
                groups.len() - 1
            }
        };
        groups[position].1.add(adw::ShortcutsItem::new(title, accelerators[0]));
    }
    for (_, section) in groups {
        dialog.add(section);
    }
    dialog.present(Some(window));
}

pub fn install(application: &adw::Application, window: &adw::ApplicationWindow) {
    action(window, "play-pause", || ctx().player.playback.toggle());
    action(window, "next", || ctx().player.playback.next());
    action(window, "previous", || ctx().player.playback.previous());
    action(window, "seek-forward", || {
        let player = ctx().player.clone();
        player.playback.seek(player.position() + SEEK_STEP);
    });
    action(window, "seek-back", || {
        let player = ctx().player.clone();
        player.playback.seek((player.position() - SEEK_STEP).max(0.0));
    });
    action(window, "volume-up", || {
        let player = ctx().player.clone();
        player.playback.set_volume((player.view().volume + VOLUME_STEP).min(1.0));
    });
    action(window, "volume-down", || {
        let player = ctx().player.clone();
        player.playback.set_volume((player.view().volume - VOLUME_STEP).max(0.0));
    });
    action(window, "back", || ctx().nav.back());
    action(window, "forward", || ctx().nav.forward());
    // "Go to Song" for the song that is playing (its menu in the player bar has it too)
    action(window, "go-to-song", || {
        if let Some(track) = ctx().player.track().filter(crate::actions::in_library) {
            crate::actions::go_to_song(&track);
        }
    });
    action(window, "refresh", || {
        let ctx = ctx();
        ctx.music.refresh();
        ctx.library.reload(&ctx.music);
        ctx.refreshed.emit(&());
    });
    // (Escape closes Now Playing as well: the view itself listens for it)
    view_action(window, "now-playing", |ctx| &ctx.show_now_playing);
    view_action(window, "queue", |ctx| &ctx.show_queue);
    {
        let weak = window.downgrade();
        action(window, "new-playlist", move || {
            if let Some(window) = weak.upgrade() {
                dialogs::new_playlist(&window, Vec::new());
            }
        });
        let weak = window.downgrade();
        action(window, "fullscreen", move || {
            if let Some(window) = weak.upgrade() {
                window.set_fullscreened(!window.is_fullscreen());
            }
        });
        let weak = window.downgrade();
        action(window, "shortcuts", move || {
            if let Some(window) = weak.upgrade() {
                show_shortcuts(&window);
            }
        });
    }
    {
        let app = application.clone();
        action(application, "quit", move || app.quit());
    }
    for (name, accelerators, _, _) in SHORTCUTS {
        // Space is handled below: as an accelerator it would also fire while typing
        if *name != "win.play-pause" {
            application.set_accels_for_action(name, accelerators);
        }
    }

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(window) = weak.upgrade() else { return glib::Propagation::Proceed };
        let plain = !modifiers
            .intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
        if key == gdk::Key::space && plain && !typing(&window) {
            ctx().player.playback.toggle();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
}
