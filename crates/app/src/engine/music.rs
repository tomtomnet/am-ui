//! The music service on the engine: Apple's API through `amui::api::MusicApi`, artwork through
//! the engine's artwork cache and its network layer (anonymous, Apple's image hosts only).

use std::collections::HashMap;
use std::sync::Arc;

use amui::api::{ApiError, MusicApi};
use amui::cache::ArtCache;
use amui::config::Config;
use amui::types::{Album, Artist, Lyrics, Page, Playlist, SearchResults, Track};
use amui::web::Http;

use crate::data::{ArtistPage, Collection, Item, Music, Problem, Ref, Region, Res, Shelf};

/// Told when Apple rejects the sign-in (from a worker thread).
pub type OnRejected = Box<dyn Fn() + Send + Sync>;

pub struct EngineMusic {
    api: Arc<MusicApi>,
    http: Arc<dyn Http>,
    art: ArtCache,
    on_rejected: OnRejected,
}

impl EngineMusic {
    pub fn new(config: &Config, api: Arc<MusicApi>, http: Arc<dyn Http>, on_rejected: OnRejected) -> EngineMusic {
        EngineMusic { api, http, art: ArtCache::new(config), on_rejected }
    }

    /// The answer of the API client as the pages take it; a rejected sign-in is passed on.
    fn told<T>(&self, answer: Result<T, ApiError>) -> Res<T> {
        answer.map_err(|error| {
            let auth = error.is_auth();
            if auth {
                (self.on_rejected)();
            }
            Problem { message: error.to_string(), auth }
        })
    }
}

fn present(id: &Option<String>) -> Option<&str> {
    id.as_deref().filter(|id| !id.is_empty())
}

/// Two requests at the same time.
fn both<A: Send, B: Send>(first: impl FnOnce() -> A + Send, second: impl FnOnce() -> B + Send) -> (A, B) {
    std::thread::scope(|scope| {
        let second = scope.spawn(second);
        let first = first();
        (first, second.join().expect("a request does not panic"))
    })
}

impl Music for EngineMusic {
    fn library_albums(&self, offset: i64, limit: i64) -> Res<Page<Album>> {
        self.told(self.api.library_albums(offset, limit, None, false))
    }

    fn library_artists(&self, offset: i64, limit: i64) -> Res<Page<Artist>> {
        // with the catalog's artist: that is where the artwork comes from
        self.told(self.api.library_artists(offset, limit, true))
    }

    fn library_songs(&self, offset: i64, limit: i64) -> Res<Page<Track>> {
        // the most recently added first: the order "Recently Added" shows (the songs do not say
        // when they were added)
        self.told(self.api.library_songs_sorted(offset, limit, false, Some("-dateAdded")))
    }

    fn library_playlists(&self, offset: i64, limit: i64) -> Res<Page<Playlist>> {
        self.told(self.api.library_playlists(offset, limit))
    }

    fn library_recently_added(&self, offset: i64, limit: i64) -> Res<Page<Item>> {
        self.told(self.api.library_recently_added(offset, limit))
    }

    fn album(&self, target: &Ref) -> Res<Collection> {
        if let Some(id) = present(&target.library_id) {
            let (album, tracks) = both(|| self.api.library_album(id), || self.api.library_album_tracks(id));
            let (album, tracks) = (self.told(album)?, self.told(tracks)?);
            return Ok(Collection { item: Item::Album(album), tracks, next_offset: None, total: None });
        }
        let Some(id) = present(&target.catalog_id) else {
            return Err(Problem::new("This album has no Apple Music identifier"));
        };
        let (album, tracks) = self.told(self.api.catalog_album(id))?;
        Ok(Collection { item: Item::Album(album), tracks, next_offset: None, total: None })
    }

    fn playlist(&self, target: &Ref) -> Res<Collection> {
        if let Some(id) = present(&target.library_id) {
            let (playlist, page) = both(
                || self.api.library_playlist(id),
                || self.api.library_playlist_tracks(id, 0, amui::api::LIBRARY_LIMIT),
            );
            let (playlist, page) = (self.told(playlist)?, self.told(page)?);
            return Ok(Collection {
                item: Item::Playlist(playlist),
                tracks: page.items,
                next_offset: page.next_offset,
                total: page.total,
            });
        }
        let Some(id) = present(&target.catalog_id) else {
            return Err(Problem::new("This playlist has no Apple Music identifier"));
        };
        let (playlist, tracks) = self.told(self.api.catalog_playlist(id))?;
        Ok(Collection { item: Item::Playlist(playlist), tracks, next_offset: None, total: None })
    }

    fn playlist_tracks(&self, library_id: &str, offset: i64, limit: i64) -> Res<Page<Track>> {
        self.told(self.api.library_playlist_tracks(library_id, offset, limit))
    }

    fn artist(&self, target: &Ref) -> Res<ArtistPage> {
        // an artist of the library: what the library holds of them
        if let Some(id) = present(&target.library_id) {
            let (artist, albums) = both(|| self.api.library_artist(id), || self.api.library_artist_albums(id));
            let (artist, albums) = (self.told(artist)?, self.told(albums)?);
            return Ok(ArtistPage { artist, top_songs: Vec::new(), albums });
        }
        let Some(id) = present(&target.catalog_id) else {
            return Err(Problem::new("This artist has no Apple Music identifier"));
        };
        let (artist, top_songs, albums) = self.told(self.api.catalog_artist(id))?;
        Ok(ArtistPage { artist, top_songs, albums })
    }

    fn song(&self, catalog_id: &str) -> Res<Track> {
        self.told(self.api.catalog_song(catalog_id))
    }

    fn library_song_links(&self, library_id: &str) -> Res<(Option<String>, Option<String>)> {
        self.told(self.api.library_song_links(library_id))
    }

    fn search_library(&self, term: &str, limit: i64) -> Res<SearchResults> {
        self.told(self.api.search_library(term, limit))
    }

    fn search_catalog(&self, term: &str, limit: i64) -> Res<SearchResults> {
        self.told(self.api.search_catalog(term, limit))
    }

    fn lyrics(&self, catalog_id: &str) -> Res<Option<Lyrics>> {
        self.told(self.api.lyrics(catalog_id))
    }

    fn home(&self) -> Res<Vec<Shelf>> {
        self.told(self.api.home())
    }

    fn browse(&self) -> Res<Vec<Shelf>> {
        self.told(self.api.browse())
    }

    fn radio(&self) -> Res<Vec<Shelf>> {
        self.told(self.api.radio())
    }

    fn favorites(&self, catalog_ids: &[String]) -> Res<HashMap<String, bool>> {
        self.told(self.api.is_favorite(catalog_ids))
    }

    fn set_favorite(&self, track: &Track, favorite: bool) -> Res<()> {
        self.told(if favorite { self.api.favorite(track) } else { self.api.unfavorite(track) })
    }

    fn add_to_library(&self, item: &Item) -> Res<()> {
        let (id, kind) = match item {
            Item::Track(track) => (present(&track.catalog_id), "songs"),
            Item::Album(album) => (present(&album.catalog_id), "albums"),
            Item::Playlist(playlist) => (present(&playlist.catalog_id), "playlists"),
            Item::Artist(_) | Item::Station(_) => (None, ""),
        };
        let Some(id) = id else {
            return Err(Problem::new(format!("{} cannot be added to the library", item.title())));
        };
        self.told(self.api.add_to_library(id, kind))
    }

    fn create_playlist(&self, name: &str, description: &str, tracks: &[Track]) -> Res<Playlist> {
        self.told(self.api.create_playlist(name, description, tracks))
    }

    fn add_to_playlist(&self, library_id: &str, tracks: &[Track]) -> Res<()> {
        self.told(self.api.add_to_playlist(library_id, tracks))
    }

    fn rename_playlist(&self, library_id: &str, name: &str, description: &str) -> Res<()> {
        self.told(self.api.rename_playlist(library_id, Some(name), Some(description)))
    }

    fn delete_playlist(&self, library_id: &str) -> Res<()> {
        self.told(self.api.delete_playlist(library_id))
    }

    fn remove_from_playlist(&self, library_id: &str, tracks: &[Track]) -> Res<()> {
        self.told(self.api.remove_from_playlist(library_id, tracks))
    }

    fn refresh(&self) {
        self.api.clear_cache();
    }

    fn forget_library(&self) {
        self.api.invalidate(&[]);
    }

    fn catalog_region(&self) -> Res<Region> {
        let account = self.told(self.api.storefront())?;
        Ok(Region { account, chosen: self.api.catalog_storefront_override() })
    }

    fn set_catalog_region(&self, code: Option<&str>) -> Res<()> {
        self.told(self.api.set_catalog_storefront(code))
    }

    fn artwork(&self, url: &str) -> Res<Vec<u8>> {
        if let Ok(Some(bytes)) = self.art.get(url) {
            return Ok(bytes);
        }
        let bytes = self.http.get_bytes(url).map_err(|error| Problem::new(error.to_string()))?;
        // a cache that cannot be written costs a download next time, no more
        let _ = self.art.put(url, &bytes);
        Ok(bytes)
    }
}
