//! The playlists of the library as the sidebar lists them: a heading with a "+" button, then
//! one row per playlist (Apple's Favorite Songs first, with a star).

use adw::prelude::*;
use amui::types::Playlist;
use gtk::gdk;

use crate::app::ctx;
use crate::data::Item;
use crate::library::item_of;
use crate::nav::Dest;
use crate::{actions, dialogs, menus};

pub struct PlaylistList {
    pub widget: gtk::Box,
    list: gtk::ListBox,
}

fn playlist_of(row: &gtk::ListBoxRow) -> Option<Playlist> {
    let store = ctx().library.playlists.clone();
    match store.item(row.index() as u32).as_ref().and_then(item_of) {
        Some(Item::Playlist(playlist)) => Some(playlist),
        _ => None,
    }
}

fn dest_of(playlist: &Playlist) -> Dest {
    Dest::Playlist(actions::reference(&playlist.catalog_id, &playlist.library_id))
}

pub fn build() -> PlaylistList {
    let heading = gtk::Label::new(Some("Playlists"));
    heading.set_xalign(0.0);
    heading.set_hexpand(true);
    heading.add_css_class("sidebar-heading");
    let add = gtk::Button::from_icon_name("am-list-add-symbolic");
    add.add_css_class("flat");
    add.add_css_class("circular");
    add.add_css_class("sidebar-add");
    add.set_tooltip_text(Some("New Playlist"));
    add.set_valign(gtk::Align::End);
    add.connect_clicked(|button| dialogs::new_playlist(button, Vec::new()));
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.append(&heading);
    header.append(&add);

    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.bind_model(Some(&ctx().library.playlists), |object| {
        let playlist = match item_of(object) {
            Some(Item::Playlist(playlist)) => playlist,
            _ => Playlist::default(),
        };
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.append(&gtk::Image::from_icon_name(if playlist.is_favorites {
            "am-starred-symbolic"
        } else {
            "am-view-list-symbolic"
        }));
        let label = gtk::Label::new(Some(&playlist.name));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        content.append(&label);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&content));
        row.set_tooltip_text(Some(&playlist.name));

        // the playlist's menu on a right click
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        let weak = row.downgrade();
        click.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = weak.upgrade() else { return };
            if let Some(playlist) = playlist_of(&row) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                menus::item_menu(&Item::Playlist(playlist), &row, x, y);
            }
        });
        row.add_controller(click);
        row.upcast()
    });
    list.connect_row_activated(|_, row| {
        if let Some(playlist) = playlist_of(row) {
            ctx().nav.go(dest_of(&playlist));
        }
    });
    // the selection says where the window is and nothing else: when the row with the keyboard
    // focus goes away (its playlist was deleted or renamed), the list would select the one that
    // gets the focus
    list.connect_selected_rows_changed(|list| {
        if let Some(dest) = ctx().nav.current() {
            mark(list, &dest);
        }
    });

    let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
    widget.append(&header);
    widget.append(&list);
    PlaylistList { widget, list }
}

/// Select the playlist of `dest` in `list` (or none of them).
fn mark(list: &gtk::ListBox, dest: &Dest) {
    let store = ctx().library.playlists.clone();
    let wanted = match dest {
        Dest::Playlist(target) => (0..store.n_items()).find(|i| {
            matches!(store.item(*i).as_ref().and_then(item_of), Some(Item::Playlist(playlist)) if playlist.library_id.is_some() && playlist.library_id == target.library_id)
        }),
        _ => None,
    };
    match wanted.and_then(|index| list.row_at_index(index as i32)) {
        Some(row) => list.select_row(Some(&row)),
        None => list.unselect_all(),
    }
}

impl PlaylistList {
    /// Mark the playlist of `dest` as the current page (or none of them).
    pub fn show(&self, dest: &Dest) {
        mark(&self.list, dest);
    }
}
