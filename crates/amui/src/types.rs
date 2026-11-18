//! Plain data types shared by the engine, the command line and the application.
//!
//! Every type has a `to_json()` so values can be passed around and printed as JSON objects.
//! Identity rules:
//!
//! * A song is playable only through its Apple Music *catalog* id (the id used by
//!   `/v1/play/assets`). Library items carry it in `playParams.catalogId`; when a library song
//!   has no catalog id (an upload that never matched), `catalog_id` is `None` and the UI shows it
//!   as unavailable.
//! * `library_id` is the "i.…" id of a library item, used for library endpoints.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

fn object<T: Serialize>(value: &T) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Apple's artwork URL template, e.g. `https://…/{w}x{h}{c}.{f}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Artwork {
    pub url_template: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Hex without '#', when Apple supplies it.
    pub bg_color: Option<String>,
}

impl Artwork {
    pub fn new(url_template: impl Into<String>) -> Artwork {
        Artwork { url_template: url_template.into(), ..Artwork::default() }
    }

    /// Concrete JPEG URL for a square of `size` pixels (bounded by Apple's limits).
    pub fn url(&self, size: i64) -> String {
        self.url_as(size, "jpg")
    }

    /// Concrete URL for a square of `size` pixels in the image format `fmt`.
    pub fn url_as(&self, size: i64, fmt: &str) -> String {
        let size = size.clamp(32, 3000).to_string();
        self.url_template.replace("{w}", &size).replace("{h}", &size).replace("{c}", "bb").replace("{f}", fmt)
    }

    pub fn to_json(&self) -> Value {
        Value::Object(object(self))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Track {
    pub catalog_id: Option<String>,
    pub name: String,
    pub artist: String,
    pub album: String,
    pub library_id: Option<String>,
    pub album_catalog_id: Option<String>,
    pub album_library_id: Option<String>,
    pub artist_catalog_id: Option<String>,
    pub duration_ms: Option<i64>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub artwork: Option<Artwork>,
    pub has_lyrics: Option<bool>,
    /// `attributes.audioTraits` when known, e.g. `["atmos", "lossless"]`.
    pub audio_traits: Vec<String>,
    pub content_rating: Option<String>,
    pub genre: Option<String>,
    pub release_date: Option<String>,
    pub isrc: Option<String>,
    pub date_added: Option<String>,
}

impl Track {
    pub fn new(catalog_id: Option<&str>, name: impl Into<String>, artist: impl Into<String>) -> Track {
        Track {
            catalog_id: catalog_id.map(str::to_owned),
            name: name.into(),
            artist: artist.into(),
            ..Track::default()
        }
    }

    pub fn playable(&self) -> bool {
        self.catalog_id.is_some()
    }

    /// `Some(true/false)` when Apple listed audio traits; `None` when unknown (library-only
    /// metadata).
    pub fn atmos_known(&self) -> Option<bool> {
        if self.audio_traits.is_empty() { None } else { Some(self.audio_traits.iter().any(|t| t == "atmos")) }
    }

    pub fn key(&self) -> String {
        self.catalog_id
            .clone()
            .or_else(|| self.library_id.clone())
            .unwrap_or_else(|| format!("{}/{}/{}", self.artist, self.album, self.name))
    }

    pub fn to_json(&self) -> Value {
        let mut map = object(self);
        map.insert("playable".into(), json!(self.playable()));
        map.insert("atmos_known".into(), json!(self.atmos_known()));
        map.insert("key".into(), json!(self.key()));
        Value::Object(map)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Album {
    pub catalog_id: Option<String>,
    pub name: String,
    pub artist: String,
    pub library_id: Option<String>,
    pub artwork: Option<Artwork>,
    pub track_count: Option<i64>,
    pub release_date: Option<String>,
    pub date_added: Option<String>,
    pub genre: Option<String>,
    pub is_single: Option<bool>,
    pub is_compilation: Option<bool>,
    pub artist_catalog_id: Option<String>,
    pub audio_traits: Vec<String>,
}

impl Album {
    pub fn new(catalog_id: Option<&str>, name: impl Into<String>, artist: impl Into<String>) -> Album {
        Album {
            catalog_id: catalog_id.map(str::to_owned),
            name: name.into(),
            artist: artist.into(),
            ..Album::default()
        }
    }

    pub fn key(&self) -> String {
        self.library_id
            .clone()
            .or_else(|| self.catalog_id.clone())
            .unwrap_or_else(|| format!("{}/{}", self.artist, self.name))
    }

    pub fn to_json(&self) -> Value {
        let mut map = object(self);
        map.insert("key".into(), json!(self.key()));
        Value::Object(map)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub library_id: Option<String>,
    pub name: String,
    pub catalog_id: Option<String>,
    pub description: String,
    pub artwork: Option<Artwork>,
    pub track_count: Option<i64>,
    pub can_edit: bool,
    /// `attributes.canDelete`: false for Apple's own library playlists (Favorite Songs).
    pub can_delete: bool,
    /// The account's Favorite Songs playlist (`attributes.tags` has "favorited").
    pub is_favorites: bool,
    pub is_library: bool,
    pub curator: Option<String>,
    pub date_added: Option<String>,
    pub last_modified: Option<String>,
}

impl Default for Playlist {
    fn default() -> Playlist {
        Playlist {
            library_id: None,
            name: String::new(),
            catalog_id: None,
            description: String::new(),
            artwork: None,
            track_count: None,
            can_edit: false,
            can_delete: false,
            is_favorites: false,
            is_library: true,
            curator: None,
            date_added: None,
            last_modified: None,
        }
    }
}

impl Playlist {
    pub fn new(library_id: Option<&str>, name: impl Into<String>) -> Playlist {
        Playlist { library_id: library_id.map(str::to_owned), name: name.into(), ..Playlist::default() }
    }

    pub fn key(&self) -> String {
        self.library_id.clone().or_else(|| self.catalog_id.clone()).unwrap_or_else(|| self.name.clone())
    }

    pub fn to_json(&self) -> Value {
        let mut map = object(self);
        map.insert("key".into(), json!(self.key()));
        Value::Object(map)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Artist {
    pub catalog_id: Option<String>,
    pub name: String,
    pub library_id: Option<String>,
    pub artwork: Option<Artwork>,
    pub genre: Option<String>,
}

impl Artist {
    pub fn new(catalog_id: Option<&str>, name: impl Into<String>) -> Artist {
        Artist { catalog_id: catalog_id.map(str::to_owned), name: name.into(), ..Artist::default() }
    }

    pub fn key(&self) -> String {
        self.library_id.clone().or_else(|| self.catalog_id.clone()).unwrap_or_else(|| self.name.clone())
    }

    pub fn to_json(&self) -> Value {
        let mut map = object(self);
        map.insert("key".into(), json!(self.key()));
        Value::Object(map)
    }
}

/// A radio station: a live broadcast, or an endless sequence of songs Apple chooses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Station {
    pub id: String,
    pub name: String,
    /// The line under the name (Apple's short note, or what the station is).
    pub subtitle: String,
    pub artwork: Option<Artwork>,
    /// A live broadcast (`attributes.isLive`).
    pub live: bool,
    /// `attributes.kind`: "editorial", "query", "artistSeeded", "songSeeded", "streaming"...
    pub kind: String,
    /// `attributes.playParams.format`: "tracks" (songs to ask for one batch after the other) or
    /// "stream" (one continuous stream).
    pub format: String,
}

impl Station {
    /// Whether the station is played by asking Apple for its next songs.
    pub fn has_tracks(&self) -> bool {
        self.format == "tracks" && !self.live
    }

    pub fn to_json(&self) -> Value {
        let mut map = object(self);
        map.insert("key".into(), json!(self.id));
        Value::Object(map)
    }
}

/// Anything a shelf or a search can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "item", rename_all = "lowercase")]
pub enum Item {
    Track(Track),
    Album(Album),
    Playlist(Playlist),
    Artist(Artist),
    Station(Station),
}

impl Item {
    pub fn kind(&self) -> &'static str {
        match self {
            Item::Track(_) => "track",
            Item::Album(_) => "album",
            Item::Playlist(_) => "playlist",
            Item::Artist(_) => "artist",
            Item::Station(_) => "station",
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Item::Track(item) => &item.name,
            Item::Album(item) => &item.name,
            Item::Playlist(item) => &item.name,
            Item::Artist(item) => &item.name,
            Item::Station(item) => &item.name,
        }
    }

    /// The line under the title.
    pub fn subtitle(&self) -> String {
        match self {
            Item::Track(item) => item.artist.clone(),
            Item::Album(item) => item.artist.clone(),
            Item::Playlist(item) => item.curator.clone().unwrap_or_else(|| "Playlist".to_string()),
            Item::Artist(_) => "Artist".to_string(),
            Item::Station(item) => item.subtitle.clone(),
        }
    }

    pub fn artwork(&self) -> Option<&Artwork> {
        match self {
            Item::Track(item) => item.artwork.as_ref(),
            Item::Album(item) => item.artwork.as_ref(),
            Item::Playlist(item) => item.artwork.as_ref(),
            Item::Artist(item) => item.artwork.as_ref(),
            Item::Station(item) => item.artwork.as_ref(),
        }
    }

    /// A key that tells items of one kind apart.
    pub fn key(&self) -> String {
        match self {
            Item::Track(item) => item.key(),
            Item::Album(item) => item.key(),
            Item::Playlist(item) => item.key(),
            Item::Artist(item) => item.key(),
            Item::Station(item) => item.id.clone(),
        }
    }

    /// The item's own JSON object.
    pub fn to_json(&self) -> Value {
        match self {
            Item::Track(item) => item.to_json(),
            Item::Album(item) => item.to_json(),
            Item::Playlist(item) => item.to_json(),
            Item::Artist(item) => item.to_json(),
            Item::Station(item) => item.to_json(),
        }
    }

    /// The item's JSON object with a `kind` member, for lists that mix kinds.
    pub fn to_json_tagged(&self) -> Value {
        let mut value = self.to_json();
        if let Value::Object(map) = &mut value {
            map.insert("kind".into(), json!(self.kind()));
        }
        value
    }
}

/// How a shelf lays out its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShelfLayout {
    /// A row of cards that scrolls sideways.
    #[default]
    Cards,
    /// Songs in columns of four.
    Songs,
    /// A row of large cards, each under a line that says why it is there.
    Hero,
}

impl ShelfLayout {
    pub fn as_str(self) -> &'static str {
        match self {
            ShelfLayout::Cards => "cards",
            ShelfLayout::Songs => "songs",
            ShelfLayout::Hero => "hero",
        }
    }
}

/// One titled row of a page such as Home, New or Radio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shelf {
    /// Apple's id of the shelf, when it has one.
    pub id: Option<String>,
    /// May be empty: the row at the top of New and Radio has no title.
    pub title: String,
    pub layout: ShelfLayout,
    pub items: Vec<Item>,
    /// For a [`ShelfLayout::Hero`] shelf, the line over each item ("New Album", "Made for You");
    /// as long as `items`, with an empty text where Apple gave none. Empty for other layouts.
    pub notes: Vec<String>,
}

impl Shelf {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "layout": self.layout.as_str(),
            "items": self.items.iter().map(Item::to_json_tagged).collect::<Vec<_>>(),
            "notes": self.notes,
        })
    }
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// `None` = no more pages.
    pub next_offset: Option<i64>,
    pub total: Option<i64>,
}

impl<T> Default for Page<T> {
    fn default() -> Page<T> {
        Page { items: Vec::new(), next_offset: None, total: None }
    }
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, next_offset: Option<i64>, total: Option<i64>) -> Page<T> {
        Page { items, next_offset, total }
    }

    /// The page as a JSON object, each item converted with `item`.
    pub fn to_json(&self, item: impl Fn(&T) -> Value) -> Value {
        json!({
            "items": self.items.iter().map(item).collect::<Vec<_>>(),
            "next_offset": self.next_offset,
            "total": self.total,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResults {
    pub songs: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
    /// Apple's top result when supplied.
    pub top: Option<Item>,
}

impl SearchResults {
    pub fn to_json(&self) -> Value {
        json!({
            "songs": self.songs.iter().map(Track::to_json).collect::<Vec<_>>(),
            "albums": self.albums.iter().map(Album::to_json).collect::<Vec<_>>(),
            "artists": self.artists.iter().map(Artist::to_json).collect::<Vec<_>>(),
            "playlists": self.playlists.iter().map(Playlist::to_json).collect::<Vec<_>>(),
            "top": self.top.as_ref().map(Item::to_json),
            "top_kind": self.top.as_ref().map(Item::kind),
        })
    }
}

// --- lyrics (parsed from Apple's TTML; text stays in memory, never in logs or reports)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub begin: f64,
    pub end: Option<f64>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Line {
    /// Seconds; `None` when the TTML has no timing.
    pub begin: Option<f64>,
    pub end: Option<f64>,
    pub text: String,
    /// Empty when there is no word timing.
    pub words: Vec<Word>,
    /// `role="x-bg"` (background vocals).
    pub background: bool,
    /// `ttm:agent` (singer) when present.
    pub agent: Option<String>,
}

impl Line {
    pub fn to_json(&self) -> Value {
        Value::Object(object(self))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Timing {
    Word,
    Line,
    #[default]
    None,
}

impl Timing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Timing::Word => "word",
            Timing::Line => "line",
            Timing::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Lyrics {
    pub lines: Vec<Line>,
    pub timing: Timing,
    pub language: Option<String>,
    /// "syllable-lyrics" or "lyrics".
    pub source: String,
}

impl Lyrics {
    pub fn to_json(&self) -> Value {
        json!({
            "lines": self.lines.iter().map(Line::to_json).collect::<Vec<_>>(),
            "timing": self.timing.as_str(),
            "language": self.language,
            "source": self.source,
        })
    }

    /// Index of the line active at `seconds` (-1 before the first timed line).
    pub fn index_at(&self, seconds: f64) -> i64 {
        let mut current = -1;
        for (i, line) in self.lines.iter().enumerate() {
            let Some(begin) = line.begin else { continue };
            if begin <= seconds {
                current = i as i64;
            } else {
                break;
            }
        }
        current
    }
}

// --- streaming pipeline results

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcquireResult {
    pub catalog_id: String,
    /// Decrypted .m4a in the media cache (E-AC-3 JOC, AAC or ALAC).
    pub path: String,
    pub bytes: u64,
    /// `{"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"}`; lossless
    /// adds `sample_rate`/`bit_depth`.
    pub variant: Map<String, Value>,
    /// From ffprobe when available.
    pub duration_s: Option<f64>,
    pub from_cache: bool,
}

impl AcquireResult {
    pub fn to_json(&self) -> Value {
        Value::Object(object(self))
    }
}

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<$name> {
                match text { $($text => Some($name::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(PlayerState { Idle => "idle", Loading => "loading", Playing => "playing", Paused => "paused", Error => "error" });
text_enum!(RepeatMode { Off => "off", All => "all", One => "one" });
text_enum!(
    /// What the user asked for (the Dolby Atmos toggle).
    AudioMode { Atmos => "atmos", Stereo => "stereo" }
);
text_enum!(
    /// What is actually playing (stereo = AAC, lossless = ALAC).
    AudioFormat { Atmos => "atmos", Stereo => "stereo", Lossless => "lossless" }
);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaybackSnapshot {
    pub state: PlayerState,
    pub track: Option<Track>,
    /// Position in the queue, -1 when empty.
    pub index: i64,
    pub queue_length: usize,
    /// Seconds into the track.
    pub position: f64,
    /// Seconds, `None` until known.
    pub duration: Option<f64>,
    /// Seconds of audio rendered and ready beyond `position`.
    pub buffered: f64,
    /// 0..1 linear software gain.
    pub volume: f64,
    /// "manifest" | "license" | "download" | "decrypt" | "decode" | "render" | `None`.
    pub loading_phase: Option<String>,
    /// 0..1 when the phase has a measurable progress.
    pub loading_progress: Option<f64>,
    pub error: Option<String>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Requested: Dolby Atmos on/off.
    pub mode: AudioMode,
    /// Actual format of the playing audio (`None` while idle/loading).
    pub format: Option<AudioFormat>,
    /// E.g. "E-AC-3 JOC 768 kbps, 15 objects", "AAC 256 kbps", "Lossless · ALAC 24-bit/48 kHz".
    pub format_detail: String,
    /// Active downmix profile key.
    pub downmix_profile: Option<String>,
    /// True while a mode/profile change is being prepared mid-song.
    pub switching: bool,
    /// The downmix sink's preamp in dB (shared with the other apps using the sink).
    pub preamp: Option<f64>,
    /// The feeder is waiting for a growing render to get ahead of the cursor.
    pub buffering: bool,
    /// Starvation episodes since the output started.
    pub underruns: u64,
    /// Sound Check (loudness normalisation) is on.
    pub sound_check: bool,
    /// Sound Check trim applied to the current track in dB; `None` when unknown or off.
    pub track_gain_db: Option<f64>,
    /// Prefer ALAC over AAC for 2-channel playback (the setting).
    pub lossless: bool,
    /// The lossless route is built and signed in (and not backing off).
    pub lossless_available: bool,
    // What the Python engine's snapshot did not have; not part of `to_json()`.
    /// Autoplay is on (the setting): songs like the queue's follow where it ends.
    #[serde(skip)]
    pub autoplay: bool,
    /// The station whose songs the queue holds, while it does (`Player::play_station()`).
    #[serde(skip)]
    pub station: Option<Station>,
    /// The catalog id acquired for `track`: the catalog's current version when the song's own
    /// one was withdrawn, else the song's own id (what lyrics are asked for).
    #[serde(skip)]
    pub playing_catalog_id: Option<String>,
}

impl Default for PlaybackSnapshot {
    fn default() -> PlaybackSnapshot {
        PlaybackSnapshot {
            state: PlayerState::Idle,
            track: None,
            index: -1,
            queue_length: 0,
            position: 0.0,
            duration: None,
            buffered: 0.0,
            volume: 1.0,
            loading_phase: None,
            loading_progress: None,
            error: None,
            repeat: RepeatMode::Off,
            shuffle: false,
            mode: AudioMode::Atmos,
            format: None,
            format_detail: String::new(),
            downmix_profile: None,
            switching: false,
            preamp: None,
            buffering: false,
            underruns: 0,
            sound_check: true,
            track_gain_db: None,
            lossless: true,
            lossless_available: false,
            autoplay: true,
            station: None,
            playing_catalog_id: None,
        }
    }
}

impl PlaybackSnapshot {
    pub fn to_json(&self) -> Value {
        let mut map = object(self);
        map.insert("track".into(), self.track.as_ref().map(Track::to_json).unwrap_or(Value::Null));
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_url_is_bounded() {
        let art = Artwork::new("https://example.mzstatic.com/a/{w}x{h}{c}.{f}");
        assert_eq!(art.url(300), "https://example.mzstatic.com/a/300x300bb.jpg");
        assert_eq!(art.url_as(8, "webp"), "https://example.mzstatic.com/a/32x32bb.webp");
        assert_eq!(art.url(9000), "https://example.mzstatic.com/a/3000x3000bb.jpg");
    }

    #[test]
    fn track_identity() {
        let mut track = Track::new(Some("1234567890"), "Song A", "Artist A");
        assert!(track.playable());
        assert_eq!(track.key(), "1234567890");
        assert_eq!(track.atmos_known(), None);
        track.audio_traits = vec!["lossless".into(), "atmos".into()];
        assert_eq!(track.atmos_known(), Some(true));
        track.catalog_id = None;
        track.album = "Album A".into();
        assert!(!track.playable());
        assert_eq!(track.key(), "Artist A/Album A/Song A");
        track.library_id = Some("i.abc".into());
        assert_eq!(track.key(), "i.abc");
        let json = track.to_json();
        assert_eq!(json["playable"], false);
        assert_eq!(json["atmos_known"], true);
        assert_eq!(json["key"], "i.abc");
        assert_eq!(json["audio_traits"], json!(["lossless", "atmos"]));
        assert_eq!(json["artwork"], Value::Null);
    }

    #[test]
    fn collection_keys_prefer_the_library_id() {
        let mut album = Album::new(Some("111"), "Album A", "Artist A");
        assert_eq!(album.key(), "111");
        album.library_id = Some("l.abc".into());
        assert_eq!(album.key(), "l.abc");
        let playlist = Playlist::new(None, "Mix");
        assert_eq!(playlist.key(), "Mix");
        assert!(playlist.is_library && !playlist.can_edit);
        assert_eq!(Artist::new(Some("42"), "Artist A").to_json()["key"], "42");
    }

    #[test]
    fn stations_and_shelves() {
        let station = Station {
            id: "ra.1234567890".into(),
            name: "Station A".into(),
            format: "tracks".into(),
            ..Station::default()
        };
        assert!(station.has_tracks());
        assert!(!Station { live: true, ..station.clone() }.has_tracks());
        assert!(!Station { format: "stream".into(), ..station.clone() }.has_tracks());
        let item = Item::Station(station);
        assert_eq!((item.kind(), item.title(), item.key().as_str()), ("station", "Station A", "ra.1234567890"));
        assert_eq!(Item::Album(Album::new(Some("1"), "Album A", "Artist A")).subtitle(), "Artist A");
        let shelf = Shelf {
            title: "Shelf A".into(),
            layout: ShelfLayout::Songs,
            items: vec![item, Item::Track(Track::new(Some("2"), "Song A", "Artist A"))],
            ..Shelf::default()
        };
        let json = shelf.to_json();
        assert_eq!(json["layout"], "songs");
        assert_eq!(json["items"][0]["kind"], "station");
        assert_eq!(json["items"][1]["playable"], true);
    }

    #[test]
    fn search_results_name_the_top_kind() {
        let results = SearchResults { top: Some(Item::Album(Album::new(Some("1"), "A", "B"))), ..Default::default() };
        let json = results.to_json();
        assert_eq!(json["top_kind"], "album");
        assert_eq!(json["top"]["name"], "A");
        assert_eq!(SearchResults::default().to_json()["top"], Value::Null);
    }

    #[test]
    fn lyrics_index() {
        let line = |begin: Option<f64>| Line { begin, text: "x".into(), ..Line::default() };
        let lyrics = Lyrics {
            lines: vec![line(Some(1.0)), line(None), line(Some(5.0))],
            timing: Timing::Line,
            ..Lyrics::default()
        };
        assert_eq!(lyrics.index_at(0.5), -1);
        assert_eq!(lyrics.index_at(1.0), 0);
        assert_eq!(lyrics.index_at(4.9), 0);
        assert_eq!(lyrics.index_at(7.0), 2);
        assert_eq!(lyrics.to_json()["timing"], "line");
    }

    #[test]
    fn enums_round_trip_as_text() {
        assert_eq!(RepeatMode::parse("all"), Some(RepeatMode::All));
        assert_eq!(RepeatMode::parse("twice"), None);
        assert_eq!(AudioFormat::Lossless.to_string(), "lossless");
        let snapshot = PlaybackSnapshot::default().to_json();
        assert_eq!(snapshot["state"], "idle");
        assert_eq!(snapshot["mode"], "atmos");
        assert_eq!(snapshot["format"], Value::Null);
        assert_eq!(snapshot["track"], Value::Null);
    }

    #[test]
    fn the_snapshot_json_has_the_python_engine_s_keys() {
        let snapshot = PlaybackSnapshot {
            station: Some(Station { id: "ra.1000000001".into(), name: "Station A".into(), ..Station::default() }),
            playing_catalog_id: Some("1000000002".into()),
            ..PlaybackSnapshot::default()
        };
        assert!(snapshot.autoplay);
        let json = snapshot.to_json();
        let keys = json.as_object().unwrap();
        assert_eq!(keys.len(), 25, "{keys:?}");
        assert!(["autoplay", "station", "playing_catalog_id"].iter().all(|key| !keys.contains_key(*key)));
    }
}
