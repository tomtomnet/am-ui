//! What the user interface asks of the music service, independent of where the answers come
//! from: the engine (`amui`) for the real application, generated data for the demonstration.
//!
//! Every method blocks and is called on worker threads (`signal::background`).

use std::collections::HashMap;

use amui::types::{Album, Artist, Lyrics, Page, Playlist, SearchResults, Track};
pub use amui::types::{Item, Shelf, ShelfLayout, Station};

/// Why a request failed, in words that can be shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub message: String,
    /// The sign-in is missing or was rejected: the user has to connect again.
    pub auth: bool,
}

impl Problem {
    pub fn new(message: impl Into<String>) -> Problem {
        Problem { message: message.into(), auth: false }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Res<T> = Result<T, Problem>;

/// An album or a playlist with its songs.
#[derive(Debug, Clone, PartialEq)]
pub struct Collection {
    pub item: Item,
    pub tracks: Vec<Track>,
    /// More songs exist than `tracks` holds (a long library playlist): the offset to go on from.
    pub next_offset: Option<i64>,
    pub total: Option<i64>,
}

/// An artist's page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArtistPage {
    pub artist: Artist,
    pub top_songs: Vec<Track>,
    pub albums: Vec<Album>,
}

/// The region of the catalog: two-letter codes in lower case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Region {
    /// The account's own region.
    pub account: String,
    /// The region chosen instead of it, when one is.
    pub chosen: Option<String>,
}

/// Which album, playlist or artist a page is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ref {
    /// The catalog id when there is one.
    pub catalog_id: Option<String>,
    /// The library id when the item is in the library.
    pub library_id: Option<String>,
}

pub trait Music: Send + Sync + 'static {
    fn library_albums(&self, offset: i64, limit: i64) -> Res<Page<Album>>;
    fn library_artists(&self, offset: i64, limit: i64) -> Res<Page<Artist>>;
    /// The library's songs, the most recently added first.
    fn library_songs(&self, offset: i64, limit: i64) -> Res<Page<Track>>;
    fn library_playlists(&self, offset: i64, limit: i64) -> Res<Page<Playlist>>;
    /// Albums and playlists, newest first.
    fn library_recently_added(&self, offset: i64, limit: i64) -> Res<Page<Item>>;

    fn album(&self, target: &Ref) -> Res<Collection>;
    /// The first page of a playlist; `playlist_tracks` continues a long one.
    fn playlist(&self, target: &Ref) -> Res<Collection>;
    fn playlist_tracks(&self, library_id: &str, offset: i64, limit: i64) -> Res<Page<Track>>;
    fn artist(&self, target: &Ref) -> Res<ArtistPage>;
    /// One song of the catalog by its id (what a link to a song names).
    fn song(&self, catalog_id: &str) -> Res<Track>;
    /// The album and the artist of a song of the library: (album id "l.…", artist id "r.…").
    /// A song of a library list or a playlist does not say.
    fn library_song_links(&self, library_id: &str) -> Res<(Option<String>, Option<String>)>;

    fn search_library(&self, term: &str, limit: i64) -> Res<SearchResults>;
    fn search_catalog(&self, term: &str, limit: i64) -> Res<SearchResults>;
    fn lyrics(&self, catalog_id: &str) -> Res<Option<Lyrics>>;

    fn home(&self) -> Res<Vec<Shelf>>;
    fn browse(&self) -> Res<Vec<Shelf>>;
    fn radio(&self) -> Res<Vec<Shelf>>;

    fn favorites(&self, catalog_ids: &[String]) -> Res<HashMap<String, bool>>;
    fn set_favorite(&self, track: &Track, favorite: bool) -> Res<()>;
    fn add_to_library(&self, item: &Item) -> Res<()>;
    fn create_playlist(&self, name: &str, description: &str, tracks: &[Track]) -> Res<Playlist>;
    fn add_to_playlist(&self, library_id: &str, tracks: &[Track]) -> Res<()>;
    fn rename_playlist(&self, library_id: &str, name: &str, description: &str) -> Res<()>;
    fn delete_playlist(&self, library_id: &str) -> Res<()>;
    fn remove_from_playlist(&self, library_id: &str, tracks: &[Track]) -> Res<()>;

    /// Forget cached answers, so the next requests ask the service again.
    fn refresh(&self);
    /// Forget the cached answers about the library (the service applies changes to it a moment
    /// after it has accepted them).
    fn forget_library(&self);

    /// The region whose catalog is shown.
    fn catalog_region(&self) -> Res<Region>;
    /// Show another region's catalog (search, New, charts, catalog pages); `None`: the account's.
    fn set_catalog_region(&self, code: Option<&str>) -> Res<()>;

    /// The bytes of an artwork image (from the disk cache or the network).
    fn artwork(&self, url: &str) -> Res<Vec<u8>>;
}
