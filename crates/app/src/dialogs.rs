//! The dialogs for playlists: new, rename, edit description, delete.

use adw::prelude::*;
use amui::types::Track;

use crate::actions;

/// Ask for a name and a description. `done(name, description)` runs when the user confirms.
fn playlist_form(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    name: &str,
    description: &str,
    confirm: &str,
    focus_description: bool,
    done: impl Fn(String, String) + 'static,
) {
    let name_row = adw::EntryRow::new();
    name_row.set_title("Name");
    name_row.set_text(name);
    let description_row = adw::EntryRow::new();
    description_row.set_title("Description");
    description_row.set_text(description);
    let group = adw::PreferencesGroup::new();
    group.add(&name_row);
    group.add(&description_row);
    group.set_size_request(340, -1);

    let dialog = adw::AlertDialog::new(Some(heading), None);
    dialog.set_extra_child(Some(&group));
    dialog.add_responses(&[("cancel", "Cancel"), ("save", confirm)]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("save", !name.trim().is_empty());
    {
        let dialog = dialog.clone();
        name_row.connect_changed(move |row| dialog.set_response_enabled("save", !row.text().trim().is_empty()));
    }
    for row in [&name_row, &description_row] {
        // Enter in a field confirms, as the default button would
        let (dialog, name_row) = (dialog.clone(), name_row.clone());
        row.connect_entry_activated(move |_| {
            if !name_row.text().trim().is_empty() {
                dialog.set_close_response("save");
                dialog.close();
            }
        });
    }
    {
        let (name_row, description_row) = (name_row.clone(), description_row.clone());
        dialog.connect_response(None, move |_, response| {
            if response == "save" {
                done(name_row.text().trim().to_string(), description_row.text().trim().to_string());
            }
        });
    }
    dialog.present(Some(parent));
    if focus_description {
        description_row.grab_focus();
    } else {
        name_row.grab_focus();
    }
}

/// "New Playlist": with `tracks`, the playlist starts with them.
pub fn new_playlist(parent: &impl IsA<gtk::Widget>, tracks: Vec<Track>) {
    playlist_form(parent, "New Playlist", "", "", "Create", false, move |name, description| {
        actions::create_playlist(&name, &description, tracks.clone());
    });
}

/// "Rename" and "Edit Description" are the same form with another field in focus.
pub fn edit_playlist(
    parent: &impl IsA<gtk::Widget>,
    library_id: &str,
    name: &str,
    description: &str,
    focus_description: bool,
) {
    let library_id = library_id.to_string();
    playlist_form(parent, "Edit Playlist", name, description, "Save", focus_description, move |name, description| {
        actions::rename_playlist(&library_id, &name, &description);
    });
}

pub fn delete_playlist(parent: &impl IsA<gtk::Widget>, library_id: &str, name: &str) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Delete “{name}”?")),
        Some("The playlist is removed from your library on all your devices."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let (library_id, name) = (library_id.to_string(), name.to_string());
    dialog.connect_response(None, move |_, response| {
        if response == "delete" {
            actions::delete_playlist(&library_id, &name);
        }
    });
    dialog.present(Some(parent));
}
