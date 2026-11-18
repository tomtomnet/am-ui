//! What can be done with songs, albums, playlists, artists and stations, wherever they appear:
//! play, queue, open, mark as favourite.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::time::Duration;

use amui::types::{Playlist, Track};
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::app::ctx;
use crate::data::{Item, Music, Ref, Res};
use crate::library::{Kind, PAGE, track_of};
use crate::nav::Dest;
use crate::signal::background;

/// Play the songs of a list model from the one at `position`.
pub fn play_model(model: &gio::ListModel, position: u32) {
    let tracks: Vec<Track> = (0..model.n_items()).filter_map(|i| model.item(i).as_ref().and_then(track_of)).collect();
    play_tracks(tracks, position as usize);
}

/// Play `tracks` from the one at `index`. Songs that cannot be played are left out.
pub fn play_tracks(tracks: Vec<Track>, index: usize) {
    let ctx = ctx();
    let Some(wanted) = tracks.get(index) else { return };
    if !wanted.playable() {
        ctx.toast.emit(&format!("{} is not available on Apple Music", wanted.name));
        return;
    }
    // the position of the wanted song among the playable ones
    let start = tracks[..index].iter().filter(|track| track.playable()).count();
    ctx.player.playback.play(tracks.into_iter().filter(Track::playable).collect(), start);
}

pub fn reference(catalog_id: &Option<String>, library_id: &Option<String>) -> Ref {
    Ref { catalog_id: catalog_id.clone(), library_id: library_id.clone() }
}

/// The page of an album, playlist or artist; a song or a station plays.
pub fn open(item: &Item) {
    let ctx = ctx();
    match item {
        Item::Album(album) => ctx.nav.go(Dest::Album(reference(&album.catalog_id, &album.library_id))),
        Item::Playlist(playlist) => ctx.nav.go(Dest::Playlist(reference(&playlist.catalog_id, &playlist.library_id))),
        Item::Artist(artist) => ctx.nav.go(Dest::Artist(reference(&artist.catalog_id, &artist.library_id))),
        Item::Track(_) | Item::Station(_) => play(item),
    }
}

/// Every song of an item, fetched on a worker thread (a long playlist takes several requests).
fn tracks_of(music: &dyn Music, item: &Item) -> Res<Vec<Track>> {
    match item {
        Item::Track(track) => Ok(vec![track.clone()]),
        Item::Album(album) => Ok(music.album(&reference(&album.catalog_id, &album.library_id))?.tracks),
        Item::Playlist(playlist) => {
            let first = music.playlist(&reference(&playlist.catalog_id, &playlist.library_id))?;
            let mut tracks = first.tracks;
            let mut next = first.next_offset;
            while let (Some(offset), Some(library_id)) = (next, playlist.library_id.as_deref()) {
                let page = music.playlist_tracks(library_id, offset, PAGE)?;
                next = page.next_offset.filter(|next| *next > offset);
                tracks.extend(page.items);
            }
            Ok(tracks)
        }
        Item::Artist(artist) => Ok(music.artist(&reference(&artist.catalog_id, &artist.library_id))?.top_songs),
        // a station is played as a whole (`play`): its songs are not known in advance
        Item::Station(_) => Err(crate::data::Problem::new("The songs of a station cannot be listed")),
    }
}

/// Fetch the songs of `item` and hand them to `then` on the user interface thread; a problem
/// becomes a message for the user.
pub fn with_tracks(item: &Item, then: impl FnOnce(Vec<Track>) + 'static) {
    let ctx = ctx();
    let (music, item) = (ctx.music.clone(), item.clone());
    background(
        move || tracks_of(music.as_ref(), &item),
        move |result| match result {
            Ok(tracks) if tracks.is_empty() => crate::app::ctx().toast.emit(&"There are no songs to play".to_string()),
            Ok(tracks) => then(tracks),
            Err(problem) => crate::app::ctx().toast.emit(&problem.message),
        },
    );
}

pub fn play(item: &Item) {
    if let Item::Station(station) = item {
        return ctx().player.playback.play_station(station.clone());
    }
    with_tracks(item, |tracks| play_tracks(tracks, 0));
}

pub fn shuffle(item: &Item) {
    with_tracks(item, |tracks| {
        let ctx = ctx();
        ctx.player.playback.set_shuffle(true);
        play_tracks(tracks, 0);
    });
}

pub fn play_next(item: &Item) {
    let title = item.title().to_string();
    with_tracks(item, move |tracks| {
        let ctx = ctx();
        ctx.player.playback.play_next(tracks.into_iter().filter(Track::playable).collect());
        ctx.toast.emit(&format!("{title} plays next"));
    });
}

pub fn add_to_queue(item: &Item) {
    let title = item.title().to_string();
    with_tracks(item, move |tracks| {
        let ctx = ctx();
        ctx.player.playback.enqueue(tracks.into_iter().filter(Track::playable).collect());
        ctx.toast.emit(&format!("{title} added to the queue"));
    });
}

/// Whether the album of a song can be opened.
pub fn has_album(track: &Track) -> bool {
    track.album_catalog_id.is_some()
        || track.album_library_id.is_some()
        || track.library_id.is_some()
        || track.catalog_id.is_some()
}

/// Whether the artist of a song can be opened.
pub fn has_artist(track: &Track) -> bool {
    track.artist_catalog_id.is_some() || track.catalog_id.is_some() || track.library_id.is_some()
}

/// Where the album and the artist of a song are, looked up when the song does not say: the
/// library's album and artist for a song of the library, the catalog's for any other.
fn links_of(music: &dyn Music, track: &Track, want_album: bool) -> Option<Ref> {
    let known_album = track.album_catalog_id.is_some() || track.album_library_id.is_some();
    if want_album && known_album {
        return Some(reference(&track.album_catalog_id, &track.album_library_id));
    }
    if !want_album && track.artist_catalog_id.is_some() {
        return Some(reference(&track.artist_catalog_id, &None));
    }
    // the artist page of the catalog has more to show than the library's: ask the catalog first
    let from_catalog = || {
        let song = music.song(track.catalog_id.as_deref()?).ok()?;
        let id = if want_album { song.album_catalog_id } else { song.artist_catalog_id };
        id.map(|id| reference(&Some(id), &None))
    };
    let from_library = || {
        let (album, artist) = music.library_song_links(track.library_id.as_deref()?).ok()?;
        let id = if want_album { album } else { artist };
        id.map(|id| reference(&None, &Some(id)))
    };
    if want_album { from_library().or_else(from_catalog) } else { from_catalog().or_else(from_library) }
}

fn open_link_of(track: &Track, want_album: bool) {
    let (music, song) = (ctx().music.clone(), track.clone());
    background(
        move || links_of(music.as_ref(), &song, want_album),
        move |found| {
            let ctx = ctx();
            match found {
                Some(target) if want_album => ctx.nav.go(Dest::Album(target)),
                Some(target) => ctx.nav.go(Dest::Artist(target)),
                None => {
                    let what = if want_album { "album" } else { "artist" };
                    ctx.toast.emit(&format!("The {what} of this song cannot be found"));
                }
            }
        },
    );
}

/// The album page of a song (looked up when the song does not say which album it is from).
pub fn open_album_of(track: &Track) {
    open_link_of(track, true);
}

/// Whether "Go to Song" can lead to `track` in the library's Songs: it is there (by its library
/// id or its catalog id), or it is a song of the library and that list is still on its way.
pub fn in_library(track: &Track) -> bool {
    let library = &ctx().library;
    library.has_song(track) || (track.library_id.is_some() && !library.is_loaded(Kind::Songs))
}

/// "Go to Song": the library's Songs page, in its order, with `track` in the middle of it,
/// selected and lit up for a moment. A song that has not arrived yet is waited for.
pub fn go_to_song(track: &Track) {
    crate::pages::view_options::show_song(track);
}

/// The artist page of a song (looked up when the song does not say who its artist is).
pub fn open_artist_of(track: &Track) {
    open_link_of(track, false);
}

/// Run a change of the library on a worker thread; a problem becomes a message, success runs
/// `then` on the user interface thread.
fn change<T: Send + 'static>(work: impl FnOnce(&dyn Music) -> Res<T> + Send + 'static, then: impl FnOnce(T) + 'static) {
    let music = ctx().music.clone();
    background(
        move || work(music.as_ref()),
        move |result| match result {
            Ok(value) => then(value),
            Err(problem) => ctx().toast.emit(&problem.message),
        },
    );
}

/// When the service is asked again after a change of the library, in seconds. It applies changes
/// a moment after it has accepted them; what was changed shows meanwhile (`edits`).
const FOLLOW_UPS: [f64; 2] = [2.5, 12.0];

/// What is waiting to be asked for again after changes of the library.
#[derive(Default)]
struct FollowUp {
    /// Counts the changes: only the timers of the last one go off.
    stamp: Cell<u64>,
    lists: RefCell<HashSet<Kind>>,
    playlists: RefCell<HashSet<String>>,
}

thread_local! {
    static FOLLOW_UP: FollowUp = FollowUp::default();
}

/// Ask for `lists` and for the pages of `playlist` again in a moment. Changes that follow each
/// other closely share their requests.
fn follow_up(lists: &[Kind], playlist: Option<&str>) {
    let stamp = FOLLOW_UP.with(|follow_up| {
        follow_up.lists.borrow_mut().extend(lists);
        follow_up.playlists.borrow_mut().extend(playlist.map(str::to_string));
        follow_up.stamp.set(follow_up.stamp.get() + 1);
        follow_up.stamp.get()
    });
    for (number, seconds) in FOLLOW_UPS.into_iter().enumerate() {
        glib::timeout_add_local_once(Duration::from_secs_f64(seconds), move || {
            let (lists, playlists) = FOLLOW_UP.with(|follow_up| {
                if follow_up.stamp.get() != stamp {
                    return Default::default();
                }
                // the last time: nothing is waiting any more
                if number + 1 == FOLLOW_UPS.len() {
                    (follow_up.lists.take(), follow_up.playlists.take())
                } else {
                    (follow_up.lists.borrow().clone(), follow_up.playlists.borrow().clone())
                }
            });
            ask_again(lists, playlists);
        });
    }
}

/// Fetch `lists` and the pages of `playlists` again, past the answers kept from before.
fn ask_again(lists: HashSet<Kind>, playlists: HashSet<String>) {
    if lists.is_empty() && playlists.is_empty() {
        return;
    }
    let music = ctx().music.clone();
    background(
        move || music.forget_library(),
        move |()| {
            let ctx = ctx();
            for kind in lists {
                ctx.library.reload_kind(&ctx.music, kind);
            }
            for playlist in playlists {
                ctx.playlist_changed.emit(&playlist);
            }
        },
    );
}

/// The library's lists a moment from now: Apple applies additions asynchronously.
pub fn reload_library_soon() {
    follow_up(&Kind::ALL, None);
}

pub fn add_to_library(item: &Item) {
    let (item, title) = (item.clone(), item.title().to_string());
    change(
        move |music| music.add_to_library(&item),
        move |()| {
            ctx().toast.emit(&format!("{title} added to your library"));
            reload_library_soon();
        },
    );
}

/// The lists that show a playlist.
const PLAYLIST_LISTS: [Kind; 2] = [Kind::Playlists, Kind::Recent];

/// Create a playlist (with `tracks` in it) and open it. It is in the lists at once: the service
/// will have it in its own a moment later.
pub fn create_playlist(name: &str, description: &str, tracks: Vec<Track>) {
    let (name, description, songs) = (name.to_string(), description.to_string(), tracks.clone());
    change(
        move |music| music.create_playlist(&name, &description, &tracks),
        move |mut playlist: Playlist| {
            let ctx = ctx();
            // made here: it can be edited, and it is the newest (when the answer does not say)
            playlist.can_edit = true;
            playlist.can_delete = true;
            playlist.date_added = playlist.date_added.or_else(now_as_text);
            ctx.library.playlist_created(&playlist, &songs);
            ctx.toast.emit(&format!("Playlist “{}” created", playlist.name));
            follow_up(&PLAYLIST_LISTS, playlist.library_id.as_deref());
            if playlist.library_id.is_some() {
                crate::pages::collection::remember_created(&playlist);
                ctx.nav.go(Dest::Playlist(reference(&playlist.catalog_id, &playlist.library_id)));
            }
        },
    );
}

/// The present moment as the service writes dates.
fn now_as_text() -> Option<String> {
    glib::DateTime::now_utc().ok().and_then(|now| now.format_iso8601().ok()).map(String::from)
}

/// Add the songs of `item` to the library playlist `library_id` (named `name`).
pub fn add_to_playlist(library_id: &str, name: &str, item: &Item) {
    let (library_id, name) = (library_id.to_string(), name.to_string());
    with_tracks(item, move |tracks| {
        let id = library_id.clone();
        let songs = tracks.clone();
        change(
            move |music| music.add_to_playlist(&id, &tracks),
            move |()| {
                let ctx = ctx();
                ctx.library.songs_added(&library_id, &songs, &crate::pages::collection::known_songs(&library_id));
                ctx.toast.emit(&format!("{} added to “{name}”", crate::pages::detail::songs(songs.len())));
                ctx.playlist_changed.emit(&library_id);
                follow_up(&PLAYLIST_LISTS, Some(&library_id));
            },
        );
    });
}

pub fn rename_playlist(library_id: &str, name: &str, description: &str) {
    let (id, new_name, new_description) = (library_id.to_string(), name.to_string(), description.to_string());
    let (library_id, name, description) = (library_id.to_string(), name.to_string(), description.to_string());
    change(
        move |music| music.rename_playlist(&id, &new_name, &new_description),
        move |()| {
            let ctx = ctx();
            ctx.library.playlist_renamed(&library_id, &name, &description);
            ctx.playlist_changed.emit(&library_id);
            follow_up(&PLAYLIST_LISTS, Some(&library_id));
        },
    );
}

pub fn delete_playlist(library_id: &str, name: &str) {
    let (id, name, deleted) = (library_id.to_string(), name.to_string(), library_id.to_string());
    change(
        move |music| music.delete_playlist(&id),
        move |()| {
            let ctx = ctx();
            ctx.library.playlist_deleted(&deleted);
            ctx.toast.emit(&format!("“{name}” deleted"));
            ctx.playlist_deleted.emit(&deleted);
            follow_up(&PLAYLIST_LISTS, None);
        },
    );
}

pub fn remove_from_playlist(library_id: &str, track: &Track) {
    let (id, track, library_id) = (library_id.to_string(), track.clone(), library_id.to_string());
    let song = track.clone();
    change(
        move |music| music.remove_from_playlist(&id, std::slice::from_ref(&track)),
        move |()| {
            let ctx = ctx();
            ctx.library.song_removed(&library_id, &song);
            ctx.playlist_changed.emit(&library_id);
            follow_up(&[], Some(&library_id));
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::DemoMusic;

    fn demo() -> DemoMusic {
        DemoMusic::new(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
    }

    #[test]
    fn a_library_song_finds_its_album_in_the_library_and_its_artist_in_the_catalog() {
        let music = demo();
        let listed = music.library_songs(0, 1).unwrap().items.remove(0);
        // as Apple's library lists have it: no album, no artist
        assert!(listed.album_catalog_id.is_none() && listed.album_library_id.is_none());
        assert!(listed.artist_catalog_id.is_none() && listed.library_id.is_some());
        assert!(has_album(&listed) && has_artist(&listed));
        let album = links_of(&music, &listed, true).unwrap();
        assert!(album.catalog_id.is_none() && album.library_id.as_deref().is_some_and(|id| id.starts_with("l.")));
        let artist = links_of(&music, &listed, false).unwrap();
        assert!(artist.library_id.is_none() && artist.catalog_id.as_deref().is_some_and(|id| id.starts_with("ar")));
        // the pages open on what was found
        assert!(music.album(&album).is_ok() && music.artist(&artist).is_ok());
    }

    #[test]
    fn a_song_that_says_where_it_is_from_needs_no_lookup_and_one_without_ids_has_none() {
        let music = demo();
        let song = music.song("1000105").unwrap();
        let album = links_of(&music, &song, true).unwrap();
        assert_eq!(
            (album.catalog_id, album.library_id),
            (song.album_catalog_id.clone(), song.album_library_id.clone())
        );
        assert_eq!(links_of(&music, &song, false).unwrap().catalog_id, song.artist_catalog_id);

        // a library song whose catalog version is gone still finds its artist in the library
        let mut withdrawn = music.library_songs(0, 1).unwrap().items.remove(0);
        withdrawn.catalog_id = Some("999".into());
        assert!(links_of(&music, &withdrawn, false).unwrap().library_id.is_some_and(|id| id.starts_with("r.")));

        let nothing = Track::new(None, "Song A", "Artist A");
        assert!(!has_album(&nothing) && !has_artist(&nothing));
        assert!(links_of(&music, &nothing, true).is_none() && links_of(&music, &nothing, false).is_none());
    }
}
