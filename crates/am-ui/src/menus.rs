//! The context menus of songs and of albums, playlists, artists and stations.

use adw::prelude::*;
use amui::types::{Playlist, Track};
use gtk::{gdk, gio, glib};

use crate::app::ctx;
use crate::data::Item;
use crate::library::item_of;
use crate::{actions, dialogs};

/// A song and where it was clicked.
#[derive(Debug, Clone)]
pub struct TrackTarget {
    pub track: Track,
    /// The library playlist whose page shows the song, when that playlist can be edited.
    pub playlist: Option<String>,
    /// The song's place in the queue, when it was clicked there.
    pub queue_index: Option<usize>,
    /// It was clicked on the library's Songs page, where "Go to Song" would lead.
    pub songs_page: bool,
}

impl TrackTarget {
    pub fn new(track: &Track) -> TrackTarget {
        TrackTarget { track: track.clone(), playlist: None, queue_index: None, songs_page: false }
    }
}

/// The playlists of the library that songs can be added to.
fn editable_playlists() -> Vec<Playlist> {
    let store = ctx().library.playlists.clone();
    (0..store.n_items())
        .filter_map(|i| store.item(i).as_ref().and_then(item_of))
        .filter_map(|item| match item {
            Item::Playlist(playlist) if playlist.can_edit && playlist.library_id.is_some() => Some(playlist),
            _ => None,
        })
        .collect()
}

struct Builder {
    menu: gio::Menu,
    section: gio::Menu,
    actions: gio::SimpleActionGroup,
}

impl Builder {
    fn new() -> Builder {
        Builder { menu: gio::Menu::new(), section: gio::Menu::new(), actions: gio::SimpleActionGroup::new() }
    }

    /// An entry of the current section.
    fn item(&self, label: &str, name: &str, run: impl Fn() + 'static) {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| run());
        self.actions.add_action(&action);
        self.section.append(Some(label), Some(&format!("menu.{name}")));
    }

    /// End the current section (a separator follows when more comes).
    fn divide(&mut self) {
        if self.section.n_items() > 0 {
            self.menu.append_section(None, &self.section);
            self.section = gio::Menu::new();
        }
    }

    /// "Add to Playlist" with "New Playlist…" and the editable playlists. `add(library id, name)`
    /// adds to one of them, `create()` opens the dialog for a new one.
    fn playlists(&self, create: impl Fn() + 'static, add: impl Fn(&str, &str) + 'static) {
        let submenu = gio::Menu::new();
        let first = gio::Menu::new();
        first.append(Some("New Playlist…"), Some("menu.new-playlist"));
        submenu.append_section(None, &first);
        let action = gio::SimpleAction::new("new-playlist", None);
        action.connect_activate(move |_, _| create());
        self.actions.add_action(&action);

        let playlists = editable_playlists();
        let existing = gio::Menu::new();
        for playlist in &playlists {
            let entry = gio::MenuItem::new(Some(&playlist.name), None);
            let id = playlist.library_id.clone().unwrap_or_default();
            entry.set_action_and_target_value(Some("menu.add-to-playlist"), Some(&id.to_variant()));
            existing.append_item(&entry);
        }
        if existing.n_items() > 0 {
            submenu.append_section(None, &existing);
        }
        let action = gio::SimpleAction::new("add-to-playlist", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, parameter| {
            let Some(id) = parameter.and_then(|value| value.str().map(str::to_owned)) else { return };
            let name = playlists
                .iter()
                .find(|p| p.library_id.as_deref() == Some(id.as_str()))
                .map(|p| p.name.clone())
                .unwrap_or_default();
            add(&id, &name);
        });
        self.actions.add_action(&action);
        self.section.append_submenu(Some("Add to Playlist"), &submenu);
    }

    fn popup(mut self, widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
        self.divide();
        let popover = gtk::PopoverMenu::from_model(Some(&self.menu));
        popover.set_parent(widget);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.insert_action_group("menu", Some(&self.actions));
        popover.connect_closed(|popover| {
            // after the chosen entry ran
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }
}

/// Show the menu of a song at (`x`, `y`) of `widget`.
pub fn track_menu(target: &TrackTarget, widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
    let ctx = ctx();
    let track = target.track.clone();
    let item = Item::Track(track.clone());
    let mut menu = Builder::new();
    if track.playable() {
        let it = item.clone();
        menu.item("Play Next", "play-next", move || actions::play_next(&it));
        let it = item.clone();
        menu.item("Add to Queue", "add-to-queue", move || actions::add_to_queue(&it));
    }
    if let Some(index) = target.queue_index {
        menu.item("Remove from Queue", "remove-from-queue", move || crate::app::ctx().player.playback.remove(index));
    }
    menu.divide();

    if track.catalog_id.is_some() {
        let favorite = ctx.favorites.is_favorite(&track);
        let t = track.clone();
        menu.item(if favorite { "Undo Favorite" } else { "Favorite" }, "favorite", move || {
            let ctx = crate::app::ctx();
            ctx.favorites.toggle(&ctx.music, &t);
        });
    }
    if track.library_id.is_none() && track.catalog_id.is_some() {
        let it = item.clone();
        menu.item("Add to Library", "add-to-library", move || actions::add_to_library(&it));
    }
    {
        let (parent, t, it) = (widget.clone().upcast::<gtk::Widget>(), track.clone(), item.clone());
        menu.playlists(
            move || dialogs::new_playlist(&parent, vec![t.clone()]),
            move |id, name| actions::add_to_playlist(id, name, &it),
        );
    }
    if let Some(playlist) = target.playlist.clone() {
        let t = track.clone();
        menu.item("Remove from Playlist", "remove-from-playlist", move || actions::remove_from_playlist(&playlist, &t));
    }
    menu.divide();

    if !target.songs_page && actions::in_library(&track) {
        let t = track.clone();
        menu.item("Go to Song", "go-to-song", move || actions::go_to_song(&t));
    }
    if actions::has_album(&track) {
        let t = track.clone();
        menu.item("Go to Album", "go-to-album", move || actions::open_album_of(&t));
    }
    if actions::has_artist(&track) {
        let t = track.clone();
        menu.item("Go to Artist", "go-to-artist", move || actions::open_artist_of(&t));
    }
    menu.popup(widget, x, y);
}

/// Show the menu of an album, playlist, artist or station at (`x`, `y`) of `widget`.
pub fn item_menu(item: &Item, widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
    let mut menu = Builder::new();
    let it = item.clone();
    menu.item("Play", "play", move || actions::play(&it));
    if !matches!(item, Item::Station(_) | Item::Track(_)) {
        let it = item.clone();
        menu.item("Shuffle", "shuffle", move || actions::shuffle(&it));
    }
    if !matches!(item, Item::Station(_)) {
        let it = item.clone();
        menu.item("Play Next", "play-next", move || actions::play_next(&it));
        let it = item.clone();
        menu.item("Add to Queue", "add-to-queue", move || actions::add_to_queue(&it));
    }
    menu.divide();

    let in_library = match item {
        Item::Album(album) => album.library_id.is_some(),
        Item::Playlist(playlist) => playlist.library_id.is_some(),
        _ => true,
    };
    if !in_library {
        let it = item.clone();
        menu.item("Add to Library", "add-to-library", move || actions::add_to_library(&it));
    }
    if matches!(item, Item::Album(_) | Item::Playlist(_)) {
        let (parent, it, source) = (widget.clone().upcast::<gtk::Widget>(), item.clone(), item.clone());
        menu.playlists(
            move || {
                let parent = parent.clone();
                actions::with_tracks(&source, move |tracks| dialogs::new_playlist(&parent, tracks));
            },
            move |id, name| actions::add_to_playlist(id, name, &it),
        );
    }
    if let Item::Playlist(playlist) = item
        && let Some(library_id) = playlist.library_id.clone()
    {
        menu.divide();
        if playlist.can_edit {
            let (parent, id, name, description) = (
                widget.clone().upcast::<gtk::Widget>(),
                library_id.clone(),
                playlist.name.clone(),
                playlist.description.clone(),
            );
            menu.item("Rename…", "rename", move || dialogs::edit_playlist(&parent, &id, &name, &description, false));
            let (parent, id, name, description) = (
                widget.clone().upcast::<gtk::Widget>(),
                library_id.clone(),
                playlist.name.clone(),
                playlist.description.clone(),
            );
            menu.item("Edit Description…", "edit-description", move || {
                dialogs::edit_playlist(&parent, &id, &name, &description, true)
            });
        }
        if playlist.can_delete {
            let (parent, id, name) =
                (widget.clone().upcast::<gtk::Widget>(), library_id.clone(), playlist.name.clone());
            menu.item("Delete from Library…", "delete", move || dialogs::delete_playlist(&parent, &id, &name));
        }
    }
    menu.divide();
    if let Item::Album(album) = item
        && album.artist_catalog_id.is_some()
    {
        let artist = actions::reference(&album.artist_catalog_id, &None);
        menu.item("Go to Artist", "go-to-artist", move || {
            crate::app::ctx().nav.go(crate::nav::Dest::Artist(artist.clone()))
        });
    }
    menu.popup(widget, x, y);
}
