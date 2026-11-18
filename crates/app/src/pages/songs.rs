//! Lists of songs.

use std::rc::Rc;

use adw::prelude::*;
use amui::types::Track;
use gtk::gio;

use crate::actions;
use crate::app::ctx;
use crate::library::track_of;
use crate::menus::TrackTarget;
use crate::widgets::track_row::{RowAction, RowHandler, RowLine, RowStyle, TrackRow};

/// What a row shows under the title of its song.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Line {
    /// What the row's style says: the artist, or nothing.
    #[default]
    Style,
    /// The artist when it is not this one (the songs of an album by that artist).
    ArtistUnless(String),
    /// The album (the songs of one artist).
    Album,
}

/// What the rows of a list know about the page that shows them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Place {
    /// The library playlist the songs are in, when it can be edited: the menu of a row offers
    /// to remove its song from that playlist.
    pub playlist: Option<String>,
    pub line: Line,
    /// The list is the library's Songs page: its rows are where "Go to Song" leads.
    pub songs_page: bool,
}

/// What a row does when it is used; `play(position)` plays its list from that row on.
pub fn row_handler(play: impl Fn(u32) + 'static, place: &Place) -> RowHandler {
    let (playlist, songs_page) = (place.playlist.clone(), place.songs_page);
    Rc::new(move |position, track, action, row| {
        let ctx = ctx();
        match action {
            RowAction::Play => play(position),
            RowAction::ToggleFavorite => ctx.favorites.toggle(&ctx.music, track),
            RowAction::Menu(x, y) => {
                let target = TrackTarget { playlist: playlist.clone(), songs_page, ..TrackTarget::new(track) };
                crate::menus::track_menu(&target, row, x, y)
            }
            RowAction::OpenArtist => actions::open_artist_of(track),
            RowAction::OpenAlbum => actions::open_album_of(track),
        }
    })
}

/// A row that follows the favourites and the song that is playing while it shows a song.
pub fn new_row(style: RowStyle, handler: RowHandler) -> TrackRow {
    let row = TrackRow::new(style, handler);
    let ctx = ctx();
    ctx.favorites.changed.connect(&row, |row, _| {
        if let Some(track) = row.track() {
            row.set_favorite(crate::app::ctx().favorites.is_favorite(&track));
        }
    });
    ctx.player.song_changed.connect(&row, |row, _| {
        if let Some(track) = row.track() {
            row.set_playing(crate::app::ctx().player.now_key().as_deref() == Some(track.key().as_str()));
        }
    });
    row
}

/// Show `track`, the song at `position` of its list, in `row`.
pub fn bind_row(row: &TrackRow, position: u32, track: &Track, place: &Place) {
    let ctx = ctx();
    let playing = ctx.player.now_key().as_deref() == Some(track.key().as_str());
    row.bind(position, track, playing, ctx.favorites.is_favorite(track));
    match &place.line {
        Line::Style => {}
        Line::ArtistUnless(artist) => row.set_line((track.artist != *artist).then_some(RowLine::Artist)),
        Line::Album => row.set_line((!track.album.is_empty()).then_some(RowLine::Album)),
    }
    ctx.favorites.ensure(&ctx.music, std::slice::from_ref(track));
}

/// A list view of the songs of `model` (rows carrying `Track`).
pub fn track_list(model: &impl IsA<gio::ListModel>, style: RowStyle) -> gtk::ListView {
    track_list_in(model, style, Place::default())
}

/// A list view of the songs of `model` as the page described by `place` shows them.
pub fn track_list_in(model: &impl IsA<gio::ListModel>, style: RowStyle, place: Place) -> gtk::ListView {
    let source = model.clone().upcast::<gio::ListModel>();
    let handler = row_handler(move |position| actions::play_model(&source, position), &place);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        let Some(cell) = object.downcast_ref::<gtk::ListItem>() else { return };
        let row = new_row(style, handler.clone());
        row.set_cell(cell);
        cell.set_child(Some(&row));
    });
    factory.connect_bind(move |_, object| {
        let Some(cell) = object.downcast_ref::<gtk::ListItem>() else { return };
        let (Some(row), Some(track)) =
            (cell.child().and_downcast::<TrackRow>(), cell.item().as_ref().and_then(track_of))
        else {
            return;
        };
        bind_row(&row, cell.position(), &track, &place);
    });
    factory.connect_unbind(|_, object| {
        if let Some(row) =
            object.downcast_ref::<gtk::ListItem>().and_then(|cell| cell.child()).and_downcast::<TrackRow>()
        {
            row.unbind();
        }
    });
    let view = gtk::ListView::new(Some(gtk::MultiSelection::new(Some(model.clone()))), Some(factory));
    view.add_css_class("tracks");
    view.connect_activate(|view, position| {
        if let Some(model) = view.model() {
            actions::play_model(model.upcast_ref(), position);
        }
    });
    view
}
