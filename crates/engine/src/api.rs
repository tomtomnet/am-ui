//! Apple Music web API client (library, catalog, search, metadata, lyrics).
//!
//! Endpoints (all on <https://amp-api.music.apple.com>, credentials via [`web::Http`](crate::web::Http)):
//!
//! ```text
//! GET /v1/me/storefront                                   -> storefront id ("us", "jp", ...)
//! GET /v1/me/library/albums?limit&offset                  -> library albums (default order; sort=-dateAdded accepted)
//! GET /v1/me/library/recently-added?limit&offset          -> albums and playlists mixed (limit <= 25)
//! GET /v1/me/library/playlists?limit&offset&extend[library-playlists]=tags   (tags: "favorited" = Favorite Songs)
//! GET /v1/me/library/songs?limit&offset
//! GET /v1/me/library/artists?limit&offset&include=catalog   (the catalog artist brings the picture)
//! GET /v1/me/library/albums/{id}/tracks
//! GET /v1/me/library/playlists/{id}/tracks?limit&offset
//! GET /v1/me/library/artists/{id}/albums
//! GET /v1/catalog/{sf}/songs/{id}                         (attributes.audioTraits lists "atmos")
//! GET /v1/catalog/{sf}/albums/{id}?include=tracks
//! GET /v1/catalog/{sf}/artists/{id}?views=top-songs,full-albums,singles
//! GET /v1/catalog/{sf}/playlists/{id}?include=tracks
//! GET /v1/catalog/{sf}/search?term&types=songs,albums,artists,playlists&limit&with=topResults
//! GET /v1/me/library/search?term&types=library-songs,library-albums,library-artists,library-playlists&limit
//! GET /v1/catalog/{sf}/songs/{id}?include=lyrics,syllable-lyrics&extend=ttmlLocalizations
//!     -> relationships.lyrics.data[0].attributes.ttml and
//!        relationships["syllable-lyrics"].data[0].attributes.ttmlLocalizations (web-player extension)
//! GET /v1/me/recent/played?types=albums,playlists&limit    (limit <= 10)
//! GET /v1/me/recent/played/tracks?types=songs&limit
//! GET /v1/me/recommendations
//! GET /v1/catalog/{sf}/charts?types=albums,songs,playlists&limit
//!
//! Library editing (documented mutations only; see research/engine-api.md "Library editing"):
//! POST /v1/me/library?ids[songs]=…                        -> 202, empty body (add catalog items to the library)
//! POST /v1/me/library/playlists                           -> 201 {data:[library-playlists]} (attributes.name/description,
//!                                                            relationships.tracks.data[{id,type}])
//! POST /v1/me/library/playlists/{id}/tracks               -> 204 (body {data:[{id,type}]}, type songs|library-songs)
//! GET /v1/me/ratings/songs?ids=…                          -> {data:[{id, type:"ratings", attributes.value 1|-1}]}
//! PUT /v1/me/ratings/songs/{id} {type,attributes.value:1} -> 200; DELETE /v1/me/ratings/songs/{id} -> 204
//!     (favorites are the rating 1; library-only songs use /v1/me/ratings/library-songs/{id})
//! Undocumented but what music.apple.com itself sends (read from its public bundle, 2026-11-08):
//! PATCH  /v1/me/library/playlists/{id} {attributes:{name,description}}            -> rename / edit description
//! DELETE /v1/me/library/playlists/{id}                                             -> delete the playlist
//! DELETE /v1/me/library/playlists/{id}/tracks?ids[library-songs]={i.…}&mode=all    -> remove a song (all copies)
//! ```
//!
//! `{sf}` is [`MusicApi::catalog_storefront`]: the account's storefront unless
//! [`MusicApi::set_catalog_storefront`] chose another region. The region decides what the catalog
//! shows (search, charts, the pages of albums, artists, playlists and stations); what concerns a
//! song the account plays is asked in the account's own storefront ([`MusicApi::lyrics`],
//! [`MusicApi::catalog_song`], [`MusicApi::equivalent_song`]). An item one of the two storefronts
//! does not know is looked for in the other one.
//!
//! Library items carry the playable catalog id in `attributes.playParams.catalogId` (songs only;
//! albums/playlists need `include=catalog` -> `relationships.catalog`). Artwork comes as
//! `attributes.artwork` {url template, width, height, bgColor?}. The exact field names this
//! module relies on are listed in research/engine-api.md.
//!
//! Responses are cached in memory per URL for a short TTL (default 60 s; catalog metadata 10 min,
//! lyrics 1 h) so UI navigation stays snappy; [`MusicApi::clear_cache`] drops everything.
//! Instances are thread-safe. A [`WebError`] comes back unchanged inside [`ApiError::Web`]
//! (`WebError::Auth` so the UI can show "reconnect").
//!
//! Deliberate differences from amui/api.py:
//!
//! * A client is built from its dependencies: `MusicApi::new(config, http)`, then `with_cache_dir`,
//!   `with_ttl` and `with_clock` for what Python took as constructor arguments or patched
//!   (`amui.web`, `api._clock`). Parameters have no defaults; each method names Python's.
//! * One error type, [`ApiError`]: `Web` is Python's `web.WebError` family, `Value` its
//!   `ValueError`, `NotSupported` the class of that name.
//! * Results that were dictionaries or unions are types: [`Item`] (a track, album, playlist or
//!   artist), [`Recommendation`], [`Charts`]; a song handed in as `Track | str` is a [`TrackRef`].
//! * A resource is a `serde_json::Value`. Where Python failed with an `AttributeError` or a
//!   `TypeError` on a shape Apple does not send (a list where an object belongs), the odd part
//!   counts as missing here.
//! * Where text is expected, a JSON string or number is taken (the number as Python's `str()`
//!   spells it, apart from floats in exponent form). A boolean, a list or an object counts as
//!   missing; Python kept its `str()` ("True", "['a']").
//! * An empty id is no id. Python took the first id of an included `catalog` relationship as it
//!   was, so a library song or playlist could get "" for a catalog id (and the song count as
//!   playable); here it gets `None`.
//! * A number in text form is an optional sign and ASCII digits. Python's `int()` and `isdigit()`
//!   also took the digits of other scripts and "1_000"; its `$` let a storefront id or an album
//!   URL end in a line feed. White space is Rust's (Python adds U+001C to U+001F).
//! * `fetched_at` of the saved storefront is read in the form both engines write
//!   (`2026-01-02T03:04:05.678901+00:00`, also with `Z` or another `±HH:MM` offset). The other
//!   spellings `datetime.fromisoformat` knows, and times without an offset, count as unreadable:
//!   the storefront is asked for again.
//! * The two cache files hold the same text as Python's; their folder is created with mode 0700
//!   at every missing level (Python: the last one).
//! * The JSON body of an edit is compact and keeps non-ASCII text as UTF-8 (Python: `", "` and
//!   `": "` separators, `\uXXXX` escapes). It is the same JSON.
//! * The chosen catalog region applies to what the catalog shows, not to the account's own songs.
//!   In Python, lyrics and song lookups followed the override too: a song the chosen region does
//!   not offer came back as "not found", and lyrics, which Apple gives for the account's
//!   storefront, stayed away. The pages of albums, artists, playlists and stations keep to the
//!   region and fall back to the account's storefront for an id the region does not know.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::lyrics::parse_ttml;
pub use crate::types::Item;
use crate::types::{Album, Artist, Artwork, Lyrics, Page, Playlist, SearchResults, Track};
use crate::util::{self, python_repr};
use crate::web::{Call, Http, WebError};

mod discover;
pub use discover::{
    CONTINUOUS_SEEDS, EQUIVALENT_CANDIDATES, EQUIVALENT_DURATION_MS, NEXT_TRACKS_LIMIT, RECENT_STATIONS_LIMIT, Seed,
    parse_shelf_item, parse_station,
};

/// `https://` + [`config::API_HOST`](crate::config::API_HOST).
pub const API: &str = "https://amp-api.music.apple.com";

/// Apple's maximum page size for `/v1/me/library/*`.
pub const LIBRARY_LIMIT: i64 = 100;
/// `/v1/me/library/recently-added` rejects more with HTTP 400.
pub const RECENTLY_ADDED_LIMIT: i64 = 25;
/// Catalog and library search.
pub const SEARCH_LIMIT: i64 = 25;
/// `/v1/me/recent/played` rejects more with HTTP 400.
pub const RECENT_PLAYED_LIMIT: i64 = 10;
pub const CHARTS_LIMIT: i64 = 20;
/// Safety cap when following `next` links.
pub const MAX_PAGES: usize = 60;

pub const TTL_DEFAULT: f64 = 60.0;
pub const TTL_CATALOG: f64 = 600.0;
pub const TTL_LYRICS: f64 = 3600.0;
/// Seconds a saved storefront id is trusted.
pub const STOREFRONT_MAX_AGE: f64 = 7.0 * 24.0 * 3600.0;
const MAX_CACHE_ENTRIES: usize = 512;

/// `ids[<kind>]` for `POST /v1/me/library`.
pub const LIBRARY_KINDS: [&str; 4] = ["songs", "albums", "playlists", "music-videos"];
/// Catalog ids per `GET /v1/me/ratings/songs?ids=…` request.
pub const RATINGS_LIMIT: usize = 100;
/// `Ratings.Attributes.value`: 1 = like/favorite, -1 = dislike.
pub const FAVORITE_RATING: i64 = 1;

/// ISO 3166-1 alpha-2 codes (249) plus "xk", the user-assigned code of Apple's Kosovo storefront.
pub const STOREFRONT_CODES: [&str; 250] = [
    "ad", "ae", "af", "ag", "ai", "al", "am", "ao", "aq", "ar", "as", "at", "au", "aw", "ax", "az", "ba", "bb", "bd",
    "be", "bf", "bg", "bh", "bi", "bj", "bl", "bm", "bn", "bo", "bq", "br", "bs", "bt", "bv", "bw", "by", "bz", "ca",
    "cc", "cd", "cf", "cg", "ch", "ci", "ck", "cl", "cm", "cn", "co", "cr", "cu", "cv", "cw", "cx", "cy", "cz", "de",
    "dj", "dk", "dm", "do", "dz", "ec", "ee", "eg", "eh", "er", "es", "et", "fi", "fj", "fk", "fm", "fo", "fr", "ga",
    "gb", "gd", "ge", "gf", "gg", "gh", "gi", "gl", "gm", "gn", "gp", "gq", "gr", "gs", "gt", "gu", "gw", "gy", "hk",
    "hm", "hn", "hr", "ht", "hu", "id", "ie", "il", "im", "in", "io", "iq", "ir", "is", "it", "je", "jm", "jo", "jp",
    "ke", "kg", "kh", "ki", "km", "kn", "kp", "kr", "kw", "ky", "kz", "la", "lb", "lc", "li", "lk", "lr", "ls", "lt",
    "lu", "lv", "ly", "ma", "mc", "md", "me", "mf", "mg", "mh", "mk", "ml", "mm", "mn", "mo", "mp", "mq", "mr", "ms",
    "mt", "mu", "mv", "mw", "mx", "my", "mz", "na", "nc", "ne", "nf", "ng", "ni", "nl", "no", "np", "nr", "nu", "nz",
    "om", "pa", "pe", "pf", "pg", "ph", "pk", "pl", "pm", "pn", "pr", "ps", "pt", "pw", "py", "qa", "re", "ro", "rs",
    "ru", "rw", "sa", "sb", "sc", "sd", "se", "sg", "sh", "si", "sj", "sk", "sl", "sm", "sn", "so", "sr", "ss", "st",
    "sv", "sx", "sy", "sz", "tc", "td", "tf", "tg", "th", "tj", "tk", "tl", "tm", "tn", "to", "tr", "tt", "tv", "tw",
    "tz", "ua", "ug", "um", "us", "uy", "uz", "va", "vc", "ve", "vg", "vi", "vn", "vu", "wf", "ws", "ye", "yt", "za",
    "zm", "zw", "xk",
];

// --- errors -------------------------------------------------------------------

/// An operation the API has no endpoint for. Kept for callers; since 2026-11-08 no [`MusicApi`]
/// method returns it (playlist renaming, deletion and track removal use the requests
/// music.apple.com itself makes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct NotSupported {
    pub reason: String,
}

impl NotSupported {
    pub fn new(reason: impl Into<String>) -> NotSupported {
        NotSupported { reason: reason.into() }
    }
}

/// Why a [`MusicApi`] call failed, with a message that is safe to show and log.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ApiError {
    /// The request failed or Apple's answer cannot be used: what the HTTP layer reported,
    /// unchanged (`WebError::Auth`: reconnect needed), or this module's own finding.
    #[error(transparent)]
    Web(#[from] WebError),
    /// An argument no request is made for (Python: `ValueError`): a blank playlist name, an
    /// unknown library kind or country code.
    #[error("{0}")]
    Value(String),
    #[error(transparent)]
    NotSupported(#[from] NotSupported),
}

impl ApiError {
    /// The HTTP layer's error, when that is what this is.
    pub fn web(&self) -> Option<&WebError> {
        match self {
            ApiError::Web(error) => Some(error),
            _ => None,
        }
    }

    /// No sign-in on this machine, or Apple rejected it: the UI shows "reconnect".
    pub fn is_auth(&self) -> bool {
        self.web().is_some_and(WebError::is_auth)
    }

    fn is_not_found(&self) -> bool {
        matches!(self, ApiError::Web(WebError::NotFound(_)))
    }
}

fn failed(message: &str) -> ApiError {
    ApiError::Web(WebError::other(message))
}

/// `Ok(None)` for Apple's 404 (and for an answer without a resource).
fn unless_not_found<T>(result: Result<T, ApiError>) -> Result<Option<T>, ApiError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error),
    }
}

// --- JSON -> types ------------------------------------------------------------

type Object = Map<String, Value>;

/// Python's truth value of a JSON value: false for a missing one, null, false, 0, "", [] and {}.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// A JSON string as it is, a JSON number spelled out; nothing else is text.
fn spelled(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Python's `_str`: the text of a value, `None` when it is missing or empty.
fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(spelled).filter(|text| !text.is_empty())
}

/// Python's `str(value or "")`.
fn text_or_empty(value: Option<&Value>) -> String {
    value.filter(|value| truthy(Some(value))).and_then(spelled).unwrap_or_default()
}

/// Python's `_int`: a JSON number (truncated), or digits in a string; never a boolean.
fn int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => {
            let text = text.trim();
            let digits = text.trim_start_matches('-');
            let numeric = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit());
            if numeric { text.parse().ok() } else { None }
        }
        _ => None,
    }
}

fn first_genre(attrs: &Object) -> Option<String> {
    text(attrs.get("genreNames")?.as_array()?.first())
}

fn traits(attrs: &Object) -> Vec<String> {
    attrs
        .get("audioTraits")
        .and_then(Value::as_array)
        .map(|traits| traits.iter().filter_map(spelled).collect())
        .unwrap_or_default()
}

/// The objects of a container's `data` list: a relationship, a view, a search bucket, a chart.
fn listed(container: Option<&Value>) -> impl Iterator<Item = &Value> {
    let data = container.and_then(|container| container.get("data")?.as_array());
    data.into_iter().flatten().filter(|entry| entry.is_object())
}

/// The objects of `relationships.<name>.data`.
fn rel_items<'a>(item: &'a Object, name: &str) -> impl Iterator<Item = &'a Value> + use<'a> {
    listed(item.get("relationships").and_then(|relationships| relationships.get(name)))
}

fn rel_ids(item: &Object, name: &str) -> Vec<String> {
    rel_items(item, name).filter_map(|entry| entry.get("id").and_then(spelled)).collect()
}

/// The catalog id an included `catalog` relationship gives a library item: its first id.
fn related_catalog_id(item: &Object) -> Option<String> {
    rel_ids(item, "catalog").into_iter().next().filter(|id| !id.is_empty())
}

fn is_numeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn album_id_from_url(url: Option<&Value>) -> Option<String> {
    static ALBUM_URL: OnceLock<Regex> = OnceLock::new();
    let pattern = ALBUM_URL.get_or_init(|| Regex::new(r"/album/[^/?#]*/([0-9]+)(?:[?#]|$)").expect("a valid pattern"));
    pattern.captures(url?.as_str()?).map(|found| found[1].to_string())
}

/// `attributes.artwork` {url, width, height, bgColor} -> [`Artwork`] (template kept verbatim).
pub fn parse_artwork(value: &Value) -> Option<Artwork> {
    let url = value.get("url")?.as_str().filter(|url| !url.is_empty())?;
    Some(Artwork {
        url_template: url.to_string(),
        width: int(value.get("width")),
        height: int(value.get("height")),
        bg_color: text(value.get("bgColor")),
    })
}

fn is_library_item(item: &Object, prefix: &str) -> bool {
    text_or_empty(item.get("type")).starts_with("library-") || text_or_empty(item.get("id")).starts_with(prefix)
}

/// songs / library-songs resource -> [`Track`] (`None` for stubs without attributes). `album`
/// (Python's keyword argument, default `None`) is the album the song was listed under: it fills
/// what the song itself does not say.
pub fn parse_track(item: &Value, album: Option<&Album>) -> Option<Track> {
    let item = item.as_object()?;
    let attrs = item.get("attributes")?.as_object()?;
    let item_id = text(item.get("id"));
    let play = |key: &str| text(attrs.get("playParams")?.as_object()?.get(key));
    let library = is_library_item(item, "i.");
    let (album_ids, artist_ids) = (rel_ids(item, "albums"), rel_ids(item, "artists"));
    let album_catalog_id = || album.and_then(|album| album.catalog_id.clone());
    let (library_id, catalog_id, album_library_id, album_catalog_id, artist_catalog_id) = if library {
        let catalog_id = play("catalogId").or_else(|| related_catalog_id(item));
        let album_library_id = album_ids.first().filter(|id| id.starts_with("l.")).cloned();
        (item_id, catalog_id, album_library_id, album_catalog_id(), None)
    } else {
        let numeric = |ids: &[String]| ids.iter().find(|id| is_numeric(id)).cloned();
        let album_id = numeric(&album_ids).or_else(|| album_id_from_url(attrs.get("url"))).or_else(album_catalog_id);
        (None, item_id.or_else(|| play("id")), None, album_id, numeric(&artist_ids))
    };
    Some(Track {
        catalog_id,
        name: text_or_empty(attrs.get("name")),
        artist: text_or_empty(attrs.get("artistName")),
        album: match attrs.get("albumName") {
            name if truthy(name) => text_or_empty(name),
            _ => album.map(|album| album.name.clone()).unwrap_or_default(),
        },
        library_id,
        album_catalog_id,
        album_library_id: album_library_id.or_else(|| album.filter(|_| library)?.library_id.clone()),
        artist_catalog_id,
        duration_ms: int(attrs.get("durationInMillis")),
        track_number: int(attrs.get("trackNumber")),
        disc_number: int(attrs.get("discNumber")),
        artwork: attrs.get("artwork").and_then(parse_artwork).or_else(|| album?.artwork.clone()),
        has_lyrics: attrs.get("hasLyrics").and_then(Value::as_bool),
        audio_traits: traits(attrs),
        content_rating: text(attrs.get("contentRating")),
        genre: first_genre(attrs),
        release_date: text(attrs.get("releaseDate")),
        isrc: text(attrs.get("isrc")),
        date_added: text(attrs.get("dateAdded")),
    })
}

/// albums / library-albums resource -> [`Album`].
pub fn parse_album(item: &Value) -> Option<Album> {
    let item = item.as_object()?;
    let attrs = item.get("attributes")?.as_object()?;
    let item_id = text(item.get("id"));
    let library = is_library_item(item, "l.");
    let mut audio_traits = traits(attrs);
    let catalog_id = if library {
        // include=catalog: the matching catalog album carries the id and the audio traits
        let catalog = rel_items(item, "catalog").next();
        if audio_traits.is_empty()
            && let Some(catalog_attrs) = catalog.and_then(|catalog| catalog.get("attributes")?.as_object())
        {
            audio_traits = traits(catalog_attrs);
        }
        catalog.and_then(|catalog| text(catalog.get("id")))
    } else {
        item_id.clone()
    };
    Some(Album {
        catalog_id,
        name: text_or_empty(attrs.get("name")),
        artist: text_or_empty(attrs.get("artistName")),
        library_id: item_id.filter(|_| library),
        artwork: attrs.get("artwork").and_then(parse_artwork),
        track_count: int(attrs.get("trackCount")),
        release_date: text(attrs.get("releaseDate")),
        date_added: text(attrs.get("dateAdded")),
        genre: first_genre(attrs),
        is_single: attrs.get("isSingle").and_then(Value::as_bool),
        is_compilation: attrs.get("isCompilation").and_then(Value::as_bool),
        artist_catalog_id: rel_ids(item, "artists").into_iter().find(|id| is_numeric(id)),
        audio_traits,
    })
}

/// playlists / library-playlists resource -> [`Playlist`].
pub fn parse_playlist(item: &Value) -> Option<Playlist> {
    let item = item.as_object()?;
    let attrs = item.get("attributes")?.as_object()?;
    let item_id = text(item.get("id"));
    let library = is_library_item(item, "p.");
    let description = match attrs.get("description") {
        Some(Value::Object(texts)) => {
            [texts.get("standard"), texts.get("short")].into_iter().find(|value| truthy(*value)).flatten()
        }
        other => other,
    };
    let catalog_id = if library {
        let global_id = text(attrs.get("playParams").and_then(|play| play.as_object()?.get("globalId")));
        global_id.or_else(|| related_catalog_id(item))
    } else {
        item_id.clone()
    };
    let tagged_favorited =
        attrs.get("tags").and_then(Value::as_array).is_some_and(|tags| tags.iter().any(|tag| tag == "favorited"));
    Some(Playlist {
        library_id: item_id.filter(|_| library),
        name: text_or_empty(attrs.get("name")),
        catalog_id,
        description: text_or_empty(description),
        artwork: attrs.get("artwork").and_then(parse_artwork),
        track_count: int(attrs.get("trackCount")),
        can_edit: library && truthy(attrs.get("canEdit")),
        can_delete: library && truthy(attrs.get("canDelete")),
        is_favorites: library && tagged_favorited,
        is_library: library,
        curator: text(attrs.get("curatorName")),
        date_added: text(attrs.get("dateAdded")),
        last_modified: text(attrs.get("lastModifiedDate")),
    })
}

/// artists / library-artists resource -> [`Artist`].
pub fn parse_artist(item: &Value) -> Option<Artist> {
    let item = item.as_object()?;
    let attrs = item.get("attributes")?.as_object()?;
    let item_id = text(item.get("id"));
    let library = is_library_item(item, "r.");
    let mut catalog_id = if library { None } else { item_id.clone() };
    let mut artwork = attrs.get("artwork").and_then(parse_artwork);
    let mut genre = first_genre(attrs);
    if library {
        // include=catalog: the matching catalog artist carries the picture (and the genres)
        for catalog in rel_items(item, "catalog") {
            let catalog_attrs = catalog.get("attributes").and_then(Value::as_object);
            catalog_id = catalog_id.or_else(|| text(catalog.get("id")).filter(|id| is_numeric(id)));
            artwork = artwork.or_else(|| catalog_attrs?.get("artwork").and_then(parse_artwork));
            genre = genre.or_else(|| first_genre(catalog_attrs?));
        }
    }
    Some(Artist {
        catalog_id,
        name: text_or_empty(attrs.get("name")),
        library_id: item_id.filter(|_| library),
        artwork,
        genre,
    })
}

/// Dispatch on the resource `type`; `None` for unsupported types (stations, music videos,
/// curators...).
pub fn parse_item(item: &Value) -> Option<Item> {
    match text_or_empty(item.as_object()?.get("type")).as_str() {
        "songs" | "library-songs" => parse_track(item, None).map(Item::Track),
        "albums" | "library-albums" => parse_album(item).map(Item::Album),
        "playlists" | "library-playlists" => parse_playlist(item).map(Item::Playlist),
        "artists" | "library-artists" => parse_artist(item).map(Item::Artist),
        _ => None,
    }
}

/// What the home listings keep: albums and playlists.
fn album_or_playlist(item: &Value) -> Option<Item> {
    parse_item(item).filter(|item| matches!(item, Item::Album(_) | Item::Playlist(_)))
}

// --- pagination helpers -------------------------------------------------------

/// The objects of the answer's `data` list.
fn data(body: &Object) -> Result<Vec<&Value>, ApiError> {
    match body.get("data") {
        Some(Value::Array(items)) => Ok(items.iter().filter(|item| item.is_object()).collect()),
        _ => Err(failed("Apple returned a response without a data list")),
    }
}

/// The `next` link of a page or a relationship, when there is one.
fn next_link(container: &Object) -> Option<&str> {
    container.get("next")?.as_str().filter(|link| !link.is_empty())
}

/// Offset encoded in the `next` link (e.g. "/v1/me/library/albums?offset=100"), else `None`.
fn next_offset(container: &Object, offset: i64, count: usize) -> Option<i64> {
    let link = next_link(container)?;
    let query = link.split('#').next().and_then(|link| link.split_once('?')).map_or("", |(_, query)| query);
    // the first `offset` with a value, as `urllib.parse.parse_qs` hands it out
    let stated =
        url::form_urlencoded::parse(query.as_bytes()).find(|(name, value)| name == "offset" && !value.is_empty());
    let stated = stated.and_then(|(_, value)| {
        let value = value.trim();
        let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
        if is_numeric(digits) { value.parse::<i64>().ok() } else { None }
    });
    stated.or_else(|| (count != 0).then(|| offset + count as i64))
}

fn total(body: &Object) -> Option<i64> {
    int(body.get("meta")?.as_object()?.get("total"))
}

fn clamp(value: i64, maximum: i64) -> i64 {
    value.min(maximum).max(1)
}

fn invalid_identifier() -> ApiError {
    failed("Invalid Apple Music identifier")
}

/// Percent-encode an Apple resource id for use inside a URL path.
fn path_id(value: &str) -> Result<String, ApiError> {
    let text = value.trim();
    if text.is_empty() || text.contains('/') {
        return Err(invalid_identifier());
    }
    Ok(util::quote(text, ""))
}

/// Validate an Apple resource id for a query string or JSON body (no encoding needed, but no junk
/// either): numeric catalog ids, i./l./p./pl.u- library ids.
fn query_id(value: &str) -> Result<String, ApiError> {
    let text = value.trim();
    let plain = text.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !(1..=128).contains(&text.len()) || !plain {
        return Err(invalid_identifier());
    }
    Ok(text.to_string())
}

/// One query parameter; a `None` value is left out of the URL.
type Param<'a> = (&'a str, Option<String>);

fn param(value: impl ToString) -> Option<String> {
    Some(value.to_string())
}

/// The URL of an API path. Comma lists and square brackets stay literal in the query.
fn url(path: &str, params: &[Param]) -> String {
    let encode = |text: &str| util::quote(text, " ,[]").replace(' ', "+");
    let mut url = format!("{API}{path}");
    if !params.is_empty() {
        let pairs =
            params.iter().filter_map(|(key, value)| Some(format!("{}={}", encode(key), encode(value.as_ref()?))));
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&pairs.collect::<Vec<_>>().join("&"));
    }
    url
}

// --- results and arguments ------------------------------------------------------

/// One group of `/v1/me/recommendations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub id: Option<String>,
    /// Apple's title for the group, in the storefront's language.
    pub title: String,
    /// Albums and playlists.
    pub items: Vec<Item>,
}

impl Recommendation {
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "title": self.title, "items": self.items.iter().map(Item::to_json).collect::<Vec<_>>()})
    }
}

/// The storefront's charts: the first chart Apple lists for each kind ("most-played").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Charts {
    pub albums: Vec<Album>,
    pub songs: Vec<Track>,
    pub playlists: Vec<Playlist>,
    /// Chart name by kind ("albums", "songs", "playlists"), for the kinds Apple sent a chart of;
    /// `None` for a chart without a name.
    pub titles: BTreeMap<String, Option<String>>,
}

impl Charts {
    pub fn to_json(&self) -> Value {
        json!({
            "albums": self.albums.iter().map(Album::to_json).collect::<Vec<_>>(),
            "songs": self.songs.iter().map(Track::to_json).collect::<Vec<_>>(),
            "playlists": self.playlists.iter().map(Playlist::to_json).collect::<Vec<_>>(),
            "titles": self.titles,
        })
    }
}

/// A song handed to an editing call: a [`Track`] or a bare id (Python: `Track | str`). A bare id
/// is a catalog song id, except for [`MusicApi::remove_from_playlist`], which takes library song
/// ids ("i.…").
#[derive(Debug, Clone, Copy)]
pub enum TrackRef<'a> {
    Track(&'a Track),
    Id(&'a str),
}

impl<'a> From<&'a Track> for TrackRef<'a> {
    fn from(track: &'a Track) -> TrackRef<'a> {
        TrackRef::Track(track)
    }
}

impl<'a> From<&&'a Track> for TrackRef<'a> {
    fn from(track: &&'a Track) -> TrackRef<'a> {
        TrackRef::Track(track)
    }
}

impl<'a> From<&'a str> for TrackRef<'a> {
    fn from(id: &'a str) -> TrackRef<'a> {
        TrackRef::Id(id)
    }
}

impl<'a> From<&&'a str> for TrackRef<'a> {
    fn from(id: &&'a str) -> TrackRef<'a> {
        TrackRef::Id(id)
    }
}

impl<'a> From<&'a String> for TrackRef<'a> {
    fn from(id: &'a String) -> TrackRef<'a> {
        TrackRef::Id(id)
    }
}

/// An id that is there and not empty.
fn present(id: &Option<String>) -> Option<&str> {
    id.as_deref().filter(|id| !id.is_empty())
}

/// Playlist request entries: catalog id as type `songs`, otherwise library id as `library-songs`.
fn track_refs<'a>(tracks: impl IntoIterator<Item = impl Into<TrackRef<'a>>>) -> Result<Vec<Value>, ApiError> {
    let entry = |track: TrackRef| -> Result<Value, ApiError> {
        let (id, kind) = match track {
            TrackRef::Id(id) => (id, "songs"),
            TrackRef::Track(track) => match (present(&track.catalog_id), present(&track.library_id)) {
                (Some(id), _) => (id, "songs"),
                (None, Some(id)) => (id, "library-songs"),
                (None, None) => {
                    return Err(failed("A track without an Apple Music identifier cannot be added to a playlist"));
                }
            },
        };
        Ok(json!({"id": query_id(id)?, "type": kind}))
    };
    tracks.into_iter().map(|track| entry(track.into())).collect()
}

fn rating_path(track: TrackRef) -> Result<String, ApiError> {
    let (kind, id) = match track {
        TrackRef::Id(id) => ("songs", id),
        TrackRef::Track(track) => match (present(&track.catalog_id), present(&track.library_id)) {
            (Some(id), _) => ("songs", id),
            (None, Some(id)) => ("library-songs", id),
            (None, None) => return Err(failed("This track has no Apple Music identifier to rate")),
        },
    };
    Ok(format!("/v1/me/ratings/{kind}/{}", path_id(id)?))
}

// --- the saved storefront files ---------------------------------------------------

/// A lower-case storefront id as Apple spells it ("us", "jp"...).
fn is_storefront_id(value: &str) -> bool {
    (2..=3).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// The JSON object in a regular file (not behind a symbolic link); `None` for anything else.
fn read_object(path: &Path) -> Option<Object> {
    if !fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    util::read_json_object(path)
}

/// Write a small private file in place (mode 0600 when it is created, never through a symbolic
/// link), creating its folder.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        util::create_private_dirs(folder)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// Days from 1970-01-01 to a date of the Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let (era, year_of_era) = (year.div_euclid(400), year.rem_euclid(400));
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

/// (year, month, day) of a day counted from 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let (era, day_of_era) = (days.div_euclid(146_097), days.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day_of_year - (153 * shifted_month + 2) / 5 + 1)
}

/// Python's `datetime.isoformat()` of a UTC time: `2026-01-02T03:04:05.678901+00:00`, without
/// the fraction when it is zero.
fn utc_iso(since_epoch: Duration) -> String {
    let seconds = since_epoch.as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let (hour, minute, second) = (seconds / 3600 % 24, seconds / 60 % 60, seconds % 60);
    let fraction = match since_epoch.subsec_micros() {
        0 => String::new(),
        micros => format!(".{micros:06}"),
    };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}+00:00")
}

fn utc_now_iso() -> String {
    utc_iso(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default())
}

/// Seconds since the epoch of a time in the form [`utc_iso`] and Python's `isoformat()` write
/// (also with a space for the `T`, a comma for the point, `Z` or another `±HH:MM` offset).
fn parse_utc_iso(text: &str) -> Option<f64> {
    let number = |from: usize, to: usize| text.get(from..to).filter(|digits| is_numeric(digits))?.parse::<i64>().ok();
    let punctuation = (text.get(4..5)?, text.get(7..8)?, text.get(10..11)?, text.get(13..14)?, text.get(16..17)?);
    if !matches!(punctuation, ("-", "-", "T" | " ", ":", ":")) {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let mut rest = text.get(19..)?;
    let mut fraction = 0.0;
    if let Some(digits) = rest.strip_prefix(['.', ',']) {
        let length = digits.bytes().take_while(u8::is_ascii_digit).count();
        fraction = format!("0.{}", digits.get(..length).filter(|digits| !digits.is_empty())?).parse().ok()?;
        rest = &digits[length..];
    }
    let offset = if rest == "Z" {
        0
    } else {
        let sign = match rest.get(..1)? {
            "+" => 1,
            "-" => -1,
            _ => return None,
        };
        let (hours, minutes) = rest.get(1..)?.split_once(':')?;
        if hours.len() != 2 || minutes.len() != 2 || !is_numeric(hours) || !is_numeric(minutes) {
            return None;
        }
        let (hours, minutes) = (hours.parse::<i64>().ok()?, minutes.parse::<i64>().ok()?);
        if hours > 23 || minutes > 59 {
            return None;
        }
        sign * (hours * 3600 + minutes * 60)
    };
    let days = days_from_civil(year, month, day);
    if year < 1 || civil_from_days(days) != (year, month, day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64 + fraction)
}

// --- the client -------------------------------------------------------------------

type Body = Arc<Object>;
type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

/// Seconds on a clock that never goes back (Python: `time.monotonic`).
fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

struct Cached {
    expires: f64,
    body: Body,
    /// Place in the order of first storing: the oldest entry is dropped first.
    order: u64,
}

#[derive(Default)]
struct State {
    responses: HashMap<String, Cached>,
    stored: u64,
    /// Counts the times answers were dropped: an answer that was on its way meanwhile is not kept.
    dropped: u64,
    storefront: Option<String>,
    /// The language tags of the storefronts that were asked for theirs.
    languages: HashMap<String, Vec<String>>,
    /// `None`: the file was not read yet; `Some("")`: no override; else a code.
    storefront_override: Option<String>,
}

const LIBRARY: &[&str] = &["/v1/me/library"];
const RATINGS: &[&str] = &["/v1/me/ratings"];

/// Synchronous client; call from worker threads. Instances are thread-safe.
pub struct MusicApi {
    http: Arc<dyn Http>,
    /// The language Apple's own words are wanted in (`Config::language`).
    language: Option<String>,
    cache_dir: PathBuf,
    ttl: f64,
    clock: Clock,
    state: Mutex<State>,
}

impl MusicApi {
    /// A client that sends its requests through `http`, keeps the storefront files in the state
    /// folder's `api` (Python's default `cache_dir`) and caches answers for [`TTL_DEFAULT`]
    /// seconds.
    pub fn new(config: Arc<Config>, http: Arc<dyn Http>) -> MusicApi {
        MusicApi {
            http,
            language: config.language.clone(),
            cache_dir: config.state_dir.join("api"),
            ttl: TTL_DEFAULT,
            clock: Box::new(monotonic),
            state: Mutex::default(),
        }
    }

    /// Keep the storefront files in this folder (Python: the `cache_dir` argument).
    pub fn with_cache_dir(mut self, cache_dir: impl Into<PathBuf>) -> MusicApi {
        self.cache_dir = cache_dir.into();
        self
    }

    /// Seconds an answer is reused when the request has no TTL of its own (Python: `ttl`).
    pub fn with_ttl(mut self, ttl: f64) -> MusicApi {
        self.ttl = ttl;
        self
    }

    /// The clock answers expire by, in seconds (tests; Python patched `api._clock`).
    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) -> MusicApi {
        self.clock = Box::new(clock);
        self
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn ttl(&self) -> f64 {
        self.ttl
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // nothing a panicking thread can leave half done in there
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // --- transport + cache

    pub fn clear_cache(&self) {
        let mut state = self.state();
        state.responses.clear();
        state.dropped += 1;
    }

    fn get(&self, path: &str, params: &[Param], ttl: Option<f64>) -> Result<Body, ApiError> {
        let url = url(path, params);
        let dropped = {
            let state = self.state();
            if let Some(hit) = state.responses.get(&url)
                && hit.expires > (self.clock)()
            {
                return Ok(hit.body.clone());
            }
            state.dropped
        };
        // network outside the lock; errors (WebError::Auth among them) come back unchanged
        let body: Body = Arc::new(self.http.get_json(&url, &[])?);
        let mut state = self.state();
        if state.dropped != dropped {
            // answers were dropped while this one was on its way: it may be from before that
            return Ok(body);
        }
        if state.responses.len() >= MAX_CACHE_ENTRIES {
            let now = (self.clock)();
            state.responses.retain(|_, cached| cached.expires > now);
            while state.responses.len() >= MAX_CACHE_ENTRIES {
                let Some(oldest) =
                    state.responses.iter().min_by_key(|(_, cached)| cached.order).map(|(url, _)| url.clone())
                else {
                    break;
                };
                state.responses.remove(&oldest);
            }
        }
        let expires = (self.clock)() + ttl.unwrap_or(self.ttl);
        if let Some(cached) = state.responses.get_mut(&url) {
            (cached.expires, cached.body) = (expires, body.clone());
        } else {
            state.stored += 1;
            let order = state.stored;
            state.responses.insert(url, Cached { expires, body: body.clone(), order });
        }
        Ok(body)
    }

    /// Fetch a relative `next` link ("/v1/...") returned by Apple.
    fn follow(&self, link: &str, ttl: Option<f64>) -> Result<Body, ApiError> {
        if !link.starts_with("/v1/") {
            return Err(failed("Apple returned an unexpected pagination link"));
        }
        self.get(link, &[], ttl)
    }

    fn page<T>(
        &self,
        path: &str,
        params: &[Param],
        parser: impl Fn(&Value) -> Option<T>,
        offset: i64,
        limit: i64,
        maximum: i64,
    ) -> Result<Page<T>, ApiError> {
        let (limit, offset) = (clamp(limit, maximum), offset.max(0));
        let mut params = params.to_vec();
        params.extend([("limit", param(limit)), ("offset", (offset != 0).then(|| offset.to_string()))]);
        let body = self.get(path, &params, None)?;
        let data = data(&body)?;
        let items = data.iter().filter_map(|item| parser(item)).collect();
        Ok(Page::new(items, next_offset(&body, offset, data.len()), total(&body)))
    }

    fn all_pages<T>(
        &self,
        path: &str,
        params: &[Param],
        parser: impl Fn(&Value) -> Option<T>,
    ) -> Result<Vec<T>, ApiError> {
        let mut body = self.get(path, params, None)?;
        let mut items: Vec<T> = data(&body)?.into_iter().filter_map(&parser).collect();
        let mut pages = 1;
        while pages < MAX_PAGES
            && let Some(link) = next_link(&body).map(str::to_owned)
        {
            body = self.follow(&link, None)?;
            items.extend(data(&body)?.into_iter().filter_map(&parser));
            pages += 1;
        }
        Ok(items)
    }

    // --- account

    /// The account's storefront id ("us", "jp"...): asked for once, then kept in memory and for
    /// [`STOREFRONT_MAX_AGE`] in `storefront.json` of the cache folder.
    pub fn storefront(&self) -> Result<String, ApiError> {
        {
            let mut state = self.state();
            if let Some(known) = &state.storefront {
                return Ok(known.clone());
            }
            if let Some(saved) = self.read_storefront() {
                state.storefront = Some(saved.clone());
                return Ok(saved);
            }
        }
        let body = self.http.get_json(&url("/v1/me/storefront", &[]), &[])?;
        let storefront = text_or_empty(single(&body)?.get("id")).to_lowercase();
        if !is_storefront_id(&storefront) {
            return Err(failed("Apple returned an unexpected storefront id"));
        }
        let mut state = self.state();
        state.storefront = Some(storefront.clone());
        self.write_storefront(&storefront);
        Ok(storefront)
    }

    fn storefront_file(&self) -> PathBuf {
        self.cache_dir.join("storefront.json")
    }

    fn read_storefront(&self) -> Option<String> {
        let saved = read_object(&self.storefront_file())?;
        let (value, fetched) = (text_or_empty(saved.get("id")), text_or_empty(saved.get("fetched_at")));
        let age = if fetched.is_empty() { STOREFRONT_MAX_AGE } else { util::unix_now() - parse_utc_iso(&fetched)? };
        (is_storefront_id(&value) && age < STOREFRONT_MAX_AGE).then_some(value)
    }

    fn write_storefront(&self, storefront: &str) {
        // the value in memory is enough for this session when the file cannot be written
        let _ = write_private(
            &self.storefront_file(),
            &format!("{{\"id\": \"{storefront}\", \"fetched_at\": \"{}\"}}\n", utc_now_iso()),
        );
    }

    // --- library (pages; next_offset None when exhausted)

    /// A page of the library's albums (Python defaults: `offset=0, limit=100, sort=None,
    /// include_catalog=False`). `sort` e.g. "-dateAdded" (newest first) or "name";
    /// `include_catalog` adds catalog ids/audio traits (bigger responses).
    pub fn library_albums(
        &self,
        offset: i64,
        limit: i64,
        sort: Option<&str>,
        include_catalog: bool,
    ) -> Result<Page<Album>, ApiError> {
        let params = [("sort", sort.map(str::to_owned)), ("include", include_catalog.then(|| "catalog".to_owned()))];
        self.page("/v1/me/library/albums", &params, parse_album, offset, limit, LIBRARY_LIMIT)
    }

    /// Recently added albums and playlists, mixed (Python defaults: `offset=0, limit=25`).
    pub fn library_recently_added(&self, offset: i64, limit: i64) -> Result<Page<Item>, ApiError> {
        self.page("/v1/me/library/recently-added", &[], parse_item, offset, limit, RECENTLY_ADDED_LIMIT)
    }

    /// A page of the library's playlists (Python defaults: `offset=0, limit=100`).
    /// `extend[library-playlists]=tags` marks Apple's Favorite Songs playlist
    /// (`Playlist::is_favorites`).
    pub fn library_playlists(&self, offset: i64, limit: i64) -> Result<Page<Playlist>, ApiError> {
        let params = [("extend[library-playlists]", param("tags"))];
        self.page("/v1/me/library/playlists", &params, parse_playlist, offset, limit, LIBRARY_LIMIT)
    }

    /// A page of the library's songs (Python defaults: `offset=0, limit=100,
    /// include_albums=False`). `include_albums` fills `Track::album_library_id` (response ~2x
    /// larger, ~60% slower).
    pub fn library_songs(&self, offset: i64, limit: i64, include_albums: bool) -> Result<Page<Track>, ApiError> {
        self.library_songs_sorted(offset, limit, include_albums, None)
    }

    /// [`MusicApi::library_songs`] in an order Apple keeps: `sort` e.g. "-dateAdded" (the most
    /// recently added first; the songs themselves do not say when they were added).
    pub fn library_songs_sorted(
        &self,
        offset: i64,
        limit: i64,
        include_albums: bool,
        sort: Option<&str>,
    ) -> Result<Page<Track>, ApiError> {
        let params = [("include", include_albums.then(|| "albums".to_owned())), ("sort", sort.map(str::to_owned))];
        self.page("/v1/me/library/songs", &params, |item| parse_track(item, None), offset, limit, LIBRARY_LIMIT)
    }

    /// A page of the library's artists (Python defaults: `offset=0, limit=100,
    /// include_catalog=True`). `include_catalog` (what music.apple.com requests) fills
    /// `Artist::artwork`/`catalog_id`/`genre` from the matching catalog artist; responses are
    /// ~20x larger (260 KB per 100 artists) and ~1 s instead of ~0.3 s per page.
    pub fn library_artists(&self, offset: i64, limit: i64, include_catalog: bool) -> Result<Page<Artist>, ApiError> {
        let params = [("include", include_catalog.then(|| "catalog".to_owned()))];
        self.page("/v1/me/library/artists", &params, parse_artist, offset, limit, LIBRARY_LIMIT)
    }

    pub fn library_album_tracks(&self, library_id: &str) -> Result<Vec<Track>, ApiError> {
        let path = format!("/v1/me/library/albums/{}/tracks", path_id(library_id)?);
        self.all_pages(&path, &[("limit", param(LIBRARY_LIMIT))], |item| parse_track(item, None))
    }

    /// A page of a library playlist's songs (Python defaults: `offset=0, limit=100`).
    pub fn library_playlist_tracks(&self, library_id: &str, offset: i64, limit: i64) -> Result<Page<Track>, ApiError> {
        let path = format!("/v1/me/library/playlists/{}/tracks", path_id(library_id)?);
        let page = self.page(&path, &[], |item| parse_track(item, None), offset, limit, LIBRARY_LIMIT);
        // Apple answers 404 for a playlist without tracks
        Ok(unless_not_found(page)?.unwrap_or_else(|| Page::new(Vec::new(), None, Some(0))))
    }

    pub fn library_artist_albums(&self, library_id: &str) -> Result<Vec<Album>, ApiError> {
        let path = format!("/v1/me/library/artists/{}/albums", path_id(library_id)?);
        self.all_pages(&path, &[("limit", param(LIBRARY_LIMIT))], parse_album)
    }

    // --- catalog

    /// An item the catalog shows, asked for by its id: the chosen region's, and the account's
    /// when the region does not know the id (the album or the artist of a song of the account,
    /// say).
    fn shown<T>(&self, ask: impl Fn(&str) -> Result<T, ApiError>) -> Result<T, ApiError> {
        in_either(&self.catalog_storefront()?, || self.storefront().ok(), ask)
    }

    /// Something about a song the account plays: asked in the account's storefront, whatever
    /// region the catalog shows, and in the chosen region only for a song the account's
    /// storefront does not know.
    fn for_account<T>(&self, ask: impl Fn(&str) -> Result<T, ApiError>) -> Result<T, ApiError> {
        in_either(&self.storefront()?, || self.catalog_storefront_override(), ask)
    }

    /// One song of the catalog, as the account's storefront has it.
    pub fn catalog_song(&self, catalog_id: &str) -> Result<Track, ApiError> {
        self.for_account(|storefront| {
            let body = self.get(&catalog_path(storefront, "songs", catalog_id)?, &[], Some(TTL_CATALOG))?;
            parse_track(single(&body)?, None).ok_or_else(|| failed("Apple returned a song without attributes"))
        })
    }

    /// Embedded relationship data plus any further pages behind its `next` link.
    fn relationship<T>(
        &self,
        item: &Value,
        name: &str,
        parser: impl Fn(&Value) -> Option<T>,
    ) -> Result<Vec<T>, ApiError> {
        let relationship = item.get("relationships").and_then(|relationships| relationships.get(name));
        let mut items: Vec<T> = listed(relationship).filter_map(&parser).collect();
        let mut link = relationship.and_then(Value::as_object).and_then(next_link).map(str::to_owned);
        let mut pages = 1;
        while pages < MAX_PAGES
            && let Some(next) = link
        {
            let body = self.follow(&next, Some(TTL_CATALOG))?;
            items.extend(data(&body)?.into_iter().filter_map(&parser));
            link = next_link(&body).map(str::to_owned);
            pages += 1;
        }
        Ok(items)
    }

    /// A catalog album and its tracks.
    pub fn catalog_album(&self, catalog_id: &str) -> Result<(Album, Vec<Track>), ApiError> {
        self.shown(|storefront| {
            let path = catalog_path(storefront, "albums", catalog_id)?;
            let body = self.get(&path, &[("include", param("tracks"))], Some(TTL_CATALOG))?;
            let item = single(&body)?;
            let album = parse_album(item).ok_or_else(|| failed("Apple returned an album without attributes"))?;
            let tracks = self.relationship(item, "tracks", |track| parse_track(track, Some(&album)))?;
            Ok((album, tracks))
        })
    }

    /// Artist, top songs, albums (the full-albums view followed by the singles view,
    /// de-duplicated).
    pub fn catalog_artist(&self, catalog_id: &str) -> Result<(Artist, Vec<Track>, Vec<Album>), ApiError> {
        let params = [
            ("views", param("top-songs,full-albums,singles")),
            ("limit[top-songs]", param(20)),
            ("limit[full-albums]", param(50)),
            ("limit[singles]", param(50)),
        ];
        self.shown(|storefront| {
            let body = self.get(&catalog_path(storefront, "artists", catalog_id)?, &params, Some(TTL_CATALOG))?;
            let item = single(&body)?;
            let artist = parse_artist(item).ok_or_else(|| failed("Apple returned an artist without attributes"))?;
            let view = |name: &str| listed(item.get("views").and_then(|views| views.get(name)));
            let top = view("top-songs").filter_map(|song| parse_track(song, None)).collect();
            let mut albums: Vec<Album> = Vec::new();
            let mut seen = HashSet::new();
            for album in view("full-albums").chain(view("singles")).filter_map(parse_album) {
                if seen.insert(album.key()) {
                    albums.push(album);
                }
            }
            Ok((artist, top, albums))
        })
    }

    /// A catalog playlist and its tracks; a playlist that does not say how many it has gets the
    /// number loaded.
    pub fn catalog_playlist(&self, catalog_id: &str) -> Result<(Playlist, Vec<Track>), ApiError> {
        self.shown(|storefront| {
            let path = catalog_path(storefront, "playlists", catalog_id)?;
            let body = self.get(&path, &[("include", param("tracks"))], Some(TTL_CATALOG))?;
            let item = single(&body)?;
            let mut playlist =
                parse_playlist(item).ok_or_else(|| failed("Apple returned a playlist without attributes"))?;
            let tracks = self.relationship(item, "tracks", |track| parse_track(track, None))?;
            playlist.track_count = playlist.track_count.or(Some(tracks.len() as i64));
            Ok((playlist, tracks))
        })
    }

    // --- search

    /// Search the catalog (Python default: `limit=10`); a blank term makes no request.
    pub fn search_catalog(&self, term: &str, limit: i64) -> Result<SearchResults, ApiError> {
        self.search_in(None, term, limit)
    }

    /// Search the catalog of `storefront` (`None`: the one the catalog shows).
    fn search_in(&self, storefront: Option<&str>, term: &str, limit: i64) -> Result<SearchResults, ApiError> {
        let term = term.trim();
        if term.is_empty() {
            return Ok(SearchResults::default());
        }
        let params = [
            ("term", param(term)),
            ("types", param("songs,albums,artists,playlists")),
            ("limit", param(clamp(limit, SEARCH_LIMIT))),
            ("with", param("topResults")),
        ];
        let storefront = match storefront {
            Some(storefront) => storefront.to_string(),
            None => self.catalog_storefront()?,
        };
        let body = self.get(&format!("/v1/catalog/{storefront}/search"), &params, Some(TTL_CATALOG))?;
        let results = body.get("results");
        let top = Some(bucket(results, "topResults", parse_item)).filter(|top| !top.is_empty());
        let top = top.unwrap_or_else(|| bucket(results, "top", parse_item));
        Ok(SearchResults {
            songs: bucket(results, "songs", |item| parse_track(item, None)),
            albums: bucket(results, "albums", parse_album),
            artists: bucket(results, "artists", parse_artist),
            playlists: bucket(results, "playlists", parse_playlist),
            top: top.into_iter().next(),
        })
    }

    /// Search the library (Python default: `limit=10`); a blank term makes no request.
    pub fn search_library(&self, term: &str, limit: i64) -> Result<SearchResults, ApiError> {
        let term = term.trim();
        if term.is_empty() {
            return Ok(SearchResults::default());
        }
        let params = [
            ("term", param(term)),
            ("types", param("library-songs,library-albums,library-artists,library-playlists")),
            ("limit", param(clamp(limit, SEARCH_LIMIT))),
        ];
        let body = self.get("/v1/me/library/search", &params, None)?;
        let results = body.get("results");
        Ok(SearchResults {
            songs: bucket(results, "library-songs", |item| parse_track(item, None)),
            albums: bucket(results, "library-albums", parse_album),
            artists: bucket(results, "library-artists", parse_artist),
            playlists: bucket(results, "library-playlists", parse_playlist),
            top: None,
        })
    }

    // --- lyrics (parsed TTML; None when the song has none)

    /// The song's lyrics: word-timed when Apple has them, else line-timed; `None` when the song
    /// has none (or Apple does not know the song). Apple gives lyrics for the account's
    /// storefront: they are asked for there, whatever region the catalog shows.
    pub fn lyrics(&self, catalog_id: &str) -> Result<Option<Lyrics>, ApiError> {
        let params = [("include", param("lyrics,syllable-lyrics")), ("extend", param("ttmlLocalizations"))];
        let song = |storefront: &str| {
            let body = self.get(&catalog_path(storefront, "songs", catalog_id)?, &params, Some(TTL_LYRICS))?;
            single(&body)?; // an answer without the song is one that does not know it
            Ok(body)
        };
        let Some(body) = unless_not_found(self.for_account(song))? else { return Ok(None) };
        let Some(item) = unless_not_found(single(&body))? else { return Ok(None) };
        let (ttml, source) = match ttml_text(item, "syllable-lyrics", "ttmlLocalizations") {
            Some(ttml) => (ttml, "syllable-lyrics"),
            None => match ttml_text(item, "lyrics", "ttml") {
                Some(ttml) => (ttml, "lyrics"),
                None => return Ok(None),
            },
        };
        let parsed = parse_ttml(ttml, source).map_err(|_| failed("Apple returned lyrics that could not be parsed"))?;
        Ok(Some(parsed).filter(|lyrics| !lyrics.lines.is_empty()))
    }

    // --- home / browse

    /// `GET /v1/me/recent/played?types=albums,playlists` (limit <= 10, so page with `offset`;
    /// user playlists appear as catalog "pl.u-" playlists). Python defaults: `limit=10,
    /// offset=0`.
    pub fn recently_played(&self, limit: i64, offset: i64) -> Result<Vec<Item>, ApiError> {
        let params = [
            ("types", param("albums,playlists")),
            ("limit", param(clamp(limit, RECENT_PLAYED_LIMIT))),
            ("offset", (offset > 0).then(|| offset.to_string())),
        ];
        let body = self.get("/v1/me/recent/played", &params, None)?;
        Ok(data(&body)?.into_iter().filter_map(album_or_playlist).collect())
    }

    /// `GET /v1/me/recent/played/tracks?types=songs` (limit <= 10, so page with `offset`).
    /// Python defaults: `limit=10, offset=0`.
    pub fn recently_played_tracks(&self, limit: i64, offset: i64) -> Result<Vec<Track>, ApiError> {
        let params = [
            ("types", param("songs")),
            ("limit", param(clamp(limit, RECENT_PLAYED_LIMIT))),
            ("offset", (offset > 0).then(|| offset.to_string())),
        ];
        let body = self.get("/v1/me/recent/played/tracks", &params, None)?;
        Ok(data(&body)?.into_iter().filter_map(|item| parse_track(item, None)).collect())
    }

    /// `GET /v1/me/recommendations`. Stations and other unsupported content are skipped;
    /// recommendations left empty are dropped.
    pub fn recommendations(&self) -> Result<Vec<Recommendation>, ApiError> {
        let body = self.get("/v1/me/recommendations", &[], Some(TTL_CATALOG))?;
        let mut groups = Vec::new();
        for recommendation in data(&body)?.into_iter().filter_map(Value::as_object) {
            let title = match recommendation.get("attributes").and_then(|attrs| attrs.as_object()?.get("title")) {
                Some(Value::Object(title)) => title.get("stringForDisplay"),
                other => other,
            };
            let items: Vec<Item> = rel_items(recommendation, "contents").filter_map(album_or_playlist).collect();
            if !items.is_empty() {
                groups.push(Recommendation { id: text(recommendation.get("id")), title: text_or_empty(title), items });
            }
        }
        Ok(groups)
    }

    /// `GET /v1/catalog/{sf}/charts?types=albums,songs,playlists&limit=20`: the top albums, songs
    /// and playlists of the storefront with the charts' names.
    pub fn charts(&self) -> Result<Charts, ApiError> {
        let params = [("types", param("albums,songs,playlists")), ("limit", param(CHARTS_LIMIT))];
        let body =
            self.get(&format!("/v1/catalog/{}/charts", self.catalog_storefront()?), &params, Some(TTL_CATALOG))?;
        let mut charts = Charts::default();
        for kind in ["albums", "songs", "playlists"] {
            // of each kind the first chart, when Apple sent one
            let listing = body.get("results").and_then(|results| results.get(kind)?.as_array()?.first());
            let Some(chart) = listing.filter(|chart| chart.is_object()) else { continue };
            charts.titles.insert(kind.to_string(), text(chart.get("name")));
            let entries = listed(Some(chart));
            match kind {
                "albums" => charts.albums = entries.filter_map(parse_album).collect(),
                "songs" => charts.songs = entries.filter_map(|song| parse_track(song, None)).collect(),
                _ => charts.playlists = entries.filter_map(parse_playlist).collect(),
            }
        }
        Ok(charts)
    }

    // --- library editing (documented mutations; every one drops the affected cache entries)

    /// Drop cached responses whose URL path starts with any of `prefixes` (e.g.
    /// "/v1/me/library").
    fn drop_cached(&self, prefixes: &[&str]) {
        let dropped = |path: &str| prefixes.iter().any(|prefix| path.starts_with(prefix));
        let mut state = self.state();
        state.responses.retain(|url, _| !url.strip_prefix(API).is_some_and(dropped));
        state.dropped += 1;
    }

    /// Forget cached answers under these paths (none given: the whole library) so the next read
    /// hits Apple; for changes Apple applies asynchronously (favorites, library additions) after
    /// the mutation returned.
    pub fn invalidate(&self, prefixes: &[&str]) {
        self.drop_cached(if prefixes.is_empty() { LIBRARY } else { prefixes });
    }

    /// Authenticated POST/PUT/PATCH/DELETE; the JSON object of the answer, `None` for an empty
    /// 202/204 answer. The cache entries under `invalidate` are dropped even when the request
    /// fails (it may have been applied).
    fn mutate(
        &self,
        method: &str,
        path: &str,
        params: &[Param],
        body: Option<&Value>,
        invalidate: &[&str],
    ) -> Result<Option<Object>, ApiError> {
        let url = url(path, params);
        let data = body.map(Value::to_string);
        let call = Call {
            body: data.as_deref().map(str::as_bytes),
            content_type: data.as_ref().map(|_| "application/json"),
            timeout: 30.0,
            ..Call::new(method, &url, true)
        };
        let answer = self.http.request(&call);
        self.drop_cached(invalidate);
        let raw = answer?;
        if raw.iter().all(|byte| byte.is_ascii_whitespace() || *byte == 0x0b) {
            return Ok(None);
        }
        match serde_json::from_slice(&raw) {
            Ok(Value::Object(answer)) => Ok(Some(answer)),
            Ok(_) => Ok(None),
            Err(_) => Err(failed("Apple returned a response that is not JSON")),
        }
    }

    /// Favorite a song: `PUT /v1/me/ratings/songs/{catalog id}` with value 1 (library-only songs:
    /// library-songs).
    pub fn favorite<'a>(&self, track: impl Into<TrackRef<'a>>) -> Result<(), ApiError> {
        let body = json!({"type": "ratings", "attributes": {"value": FAVORITE_RATING}});
        self.mutate("PUT", &rating_path(track.into())?, &[], Some(&body), RATINGS)?;
        Ok(())
    }

    /// Remove the favorite: `DELETE /v1/me/ratings/songs/{id}`; a song that has no rating counts
    /// as done.
    pub fn unfavorite<'a>(&self, track: impl Into<TrackRef<'a>>) -> Result<(), ApiError> {
        let path = rating_path(track.into())?;
        unless_not_found(self.mutate("DELETE", &path, &[], None, RATINGS))?;
        Ok(())
    }

    /// {catalog id: true when the personal rating is 1} via `GET /v1/me/ratings/songs?ids=…`
    /// ([`RATINGS_LIMIT`] per request, cached like other reads; [`favorite`](Self::favorite) and
    /// [`unfavorite`](Self::unfavorite) drop the cache). Ids without a rating map to false; blank
    /// ids are skipped.
    pub fn is_favorite<S: AsRef<str>>(&self, catalog_ids: &[S]) -> Result<HashMap<String, bool>, ApiError> {
        let mut ids: Vec<String> = Vec::new();
        let mut result: HashMap<String, bool> = HashMap::new();
        for id in catalog_ids.iter().map(AsRef::as_ref).filter(|id| !id.trim().is_empty()) {
            let id = query_id(id)?;
            if result.insert(id.clone(), false).is_none() {
                ids.push(id);
            }
        }
        for chunk in ids.chunks(RATINGS_LIMIT) {
            // a 404: none of these songs is rated
            let Some(body) =
                unless_not_found(self.get("/v1/me/ratings/songs", &[("ids", param(chunk.join(",")))], None))?
            else {
                continue;
            };
            for item in data(&body)? {
                let rating = item.get("attributes").and_then(|attrs| int(attrs.as_object()?.get("value")));
                if rating == Some(FAVORITE_RATING)
                    && let Some(favorite) = text(item.get("id")).and_then(|id| result.get_mut(&id))
                {
                    *favorite = true;
                }
            }
        }
        Ok(result)
    }

    /// `POST /v1/me/library?ids[<kind>]=<catalog id>` (Apple answers 202 and applies it
    /// asynchronously). `kind` is one of [`LIBRARY_KINDS`] (Python default: "songs").
    pub fn add_to_library(&self, catalog_id: &str, kind: &str) -> Result<(), ApiError> {
        if !LIBRARY_KINDS.contains(&kind) {
            return Err(ApiError::Value(format!("kind must be one of {}", LIBRARY_KINDS.join(", "))));
        }
        let ids = format!("ids[{kind}]");
        self.mutate("POST", "/v1/me/library", &[(ids.as_str(), param(query_id(catalog_id)?))], None, LIBRARY)?;
        Ok(())
    }

    /// `POST /v1/me/library/playlists` -> the new [`Playlist`] (library_id "p.…"). `tracks` may
    /// mix catalog and library songs. [`ApiError::Value`] for a blank name. Python defaults:
    /// `description="", tracks=()`; an empty description is not sent.
    pub fn create_playlist<'a>(
        &self,
        name: &str,
        description: &str,
        tracks: impl IntoIterator<Item = impl Into<TrackRef<'a>>>,
    ) -> Result<Playlist, ApiError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ApiError::Value("A playlist needs a name".to_string()));
        }
        let mut body = json!({"attributes": {"name": name}});
        if !description.is_empty() {
            body["attributes"]["description"] = json!(description);
        }
        let refs = track_refs(tracks)?;
        if !refs.is_empty() {
            body["relationships"] = json!({"tracks": {"data": refs}});
        }
        let answer = self.mutate("POST", "/v1/me/library/playlists", &[], Some(&body), LIBRARY)?;
        let created = match answer.as_ref().filter(|answer| !answer.is_empty()) {
            Some(answer) => data(answer)?.into_iter().next().and_then(parse_playlist),
            None => None,
        };
        created.ok_or_else(|| failed("Apple created the playlist but did not return it; refresh the library"))
    }

    /// `POST /v1/me/library/playlists/{id}/tracks` with `{"data": [{"id", "type"}]}` (204).
    /// Nothing sent for no tracks.
    pub fn add_to_playlist<'a>(
        &self,
        library_id: &str,
        tracks: impl IntoIterator<Item = impl Into<TrackRef<'a>>>,
    ) -> Result<(), ApiError> {
        let path = format!("/v1/me/library/playlists/{}/tracks", path_id(library_id)?);
        let refs = track_refs(tracks)?;
        if !refs.is_empty() {
            self.mutate("POST", &path, &[], Some(&json!({"data": refs})), LIBRARY)?;
        }
        Ok(())
    }

    /// `PATCH /v1/me/library/playlists/{id}` with `{"attributes": {"name", "description"}}`: the
    /// request music.apple.com's "Edit playlist" makes (204). An empty/`None` name keeps the
    /// current one; `None` keeps the description, "" clears it. Nothing is sent when neither
    /// changes.
    pub fn rename_playlist(
        &self,
        library_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut attributes = Object::new();
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            attributes.insert("name".to_string(), json!(name));
        }
        if let Some(description) = description {
            attributes.insert("description".to_string(), json!(description));
        }
        if attributes.is_empty() {
            return Ok(());
        }
        let path = format!("/v1/me/library/playlists/{}", path_id(library_id)?);
        self.mutate("PATCH", &path, &[], Some(&json!({"attributes": attributes})), LIBRARY)?;
        Ok(())
    }

    /// `DELETE /v1/me/library/playlists/{id}`: music.apple.com's "Delete from Library" for a
    /// playlist (204). Apple refuses it for its own playlists (canDelete false, e.g. Favorite
    /// Songs).
    pub fn delete_playlist(&self, library_id: &str) -> Result<(), ApiError> {
        self.mutate("DELETE", &format!("/v1/me/library/playlists/{}", path_id(library_id)?), &[], None, LIBRARY)?;
        Ok(())
    }

    /// `DELETE /v1/me/library/playlists/{id}/tracks?ids[library-songs]={i.…}&mode=all`, one
    /// request per song: music.apple.com's "Remove from Playlist" (204; `mode=all` removes every
    /// copy of that song). Songs are identified by the library id the playlist's own track
    /// listing carries; a track without one is refused.
    pub fn remove_from_playlist<'a>(
        &self,
        library_id: &str,
        tracks: impl IntoIterator<Item = impl Into<TrackRef<'a>>>,
    ) -> Result<(), ApiError> {
        let path = format!("/v1/me/library/playlists/{}/tracks", path_id(library_id)?);
        let mut ids: Vec<String> = Vec::new();
        for track in tracks {
            let library_track = match track.into() {
                TrackRef::Id(id) => Some(id).filter(|id| !id.is_empty()),
                TrackRef::Track(track) => present(&track.library_id),
            };
            let Some(library_track) = library_track else {
                return Err(failed("Only songs listed by the playlist itself (library ids) can be removed from it"));
            };
            let id = query_id(library_track)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        for id in ids {
            self.mutate("DELETE", &path, &[("ids[library-songs]", param(id)), ("mode", param("all"))], None, LIBRARY)?;
        }
        Ok(())
    }

    // --- catalog storefront override (what the catalog shows)

    fn storefront_override_file(&self) -> PathBuf {
        self.cache_dir.join("storefront-override.json")
    }

    /// The saved override code, "" when none (unreadable or unknown codes are ignored).
    fn load_storefront_override(&self) -> String {
        let saved = read_object(&self.storefront_override_file());
        let value = saved.map(|saved| text_or_empty(saved.get("id")).to_lowercase()).unwrap_or_default();
        if STOREFRONT_CODES.contains(&value.as_str()) { value } else { String::new() }
    }

    fn override_code(&self) -> String {
        let mut state = self.state();
        if state.storefront_override.is_none() {
            state.storefront_override = Some(self.load_storefront_override());
        }
        state.storefront_override.clone().unwrap_or_default()
    }

    /// Storefront of what the catalog shows (search, charts, the pages of its items): the
    /// override when set, else the account's. What concerns the account's own songs goes by
    /// [`Self::storefront`].
    pub fn catalog_storefront(&self) -> Result<String, ApiError> {
        match self.override_code() {
            code if code.is_empty() => self.storefront(),
            code => Ok(code),
        }
    }

    /// The active override code, or `None` while catalog requests follow the account's
    /// storefront.
    pub fn catalog_storefront_override(&self) -> Option<String> {
        Some(self.override_code()).filter(|code| !code.is_empty())
    }

    /// Browse another country's catalog ("jp", "FR", …; ISO 3166-1 alpha-2). `None` or "" returns
    /// to the account's storefront. Saved in `storefront-override.json` of the cache folder
    /// (0600). [`ApiError::Value`] for unknown codes.
    pub fn set_catalog_storefront(&self, code: Option<&str>) -> Result<(), ApiError> {
        let code = code.map(str::trim).unwrap_or("");
        let value = code.to_lowercase();
        if !value.is_empty() && !STOREFRONT_CODES.contains(&value.as_str()) {
            return Err(ApiError::Value(format!("{} is not an ISO 3166-1 country code", python_repr(code))));
        }
        let path = self.storefront_override_file();
        let mut state = self.state();
        // the value in memory still applies for this session when the file cannot be changed
        let _ = if value.is_empty() {
            fs::remove_file(&path)
        } else {
            write_private(&path, &format!("{{\"id\": \"{value}\", \"set_at\": \"{}\"}}\n", utc_now_iso()))
        };
        state.storefront_override = Some(value);
        Ok(())
    }
}

/// The path of a catalog resource in a storefront.
fn catalog_path(storefront: &str, kind: &str, catalog_id: &str) -> Result<String, ApiError> {
    Ok(format!("/v1/catalog/{storefront}/{kind}/{}", path_id(catalog_id)?))
}

/// What `ask` finds in the storefront `first`; when Apple does not know the id there, what it
/// finds in `second` (asked for only then; nothing when that is no other storefront).
fn in_either<T>(
    first: &str,
    second: impl FnOnce() -> Option<String>,
    ask: impl Fn(&str) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let missing = match ask(first) {
        Err(error) if error.is_not_found() => error,
        answer => return answer,
    };
    match second().filter(|other| other != first) {
        Some(other) => ask(&other),
        None => Err(missing),
    }
}

/// The one resource of an answer.
fn single(body: &Object) -> Result<&Value, ApiError> {
    let found = data(body)?.into_iter().next();
    found.ok_or_else(|| ApiError::Web(WebError::NotFound("Apple returned no resource for this id".to_string())))
}

/// The entities of `results.<key>.data`.
fn bucket<T>(results: Option<&Value>, key: &str, parser: impl Fn(&Value) -> Option<T>) -> Vec<T> {
    listed(results.and_then(|results| results.get(key))).filter_map(parser).collect()
}

/// The TTML text a lyrics relationship carries in `attribute`.
fn ttml_text<'a>(item: &'a Value, relationship: &str, attribute: &str) -> Option<&'a str> {
    let filled = |value: &'a Value| value.as_str().filter(|text| !text.trim().is_empty());
    rel_items(item.as_object()?, relationship).find_map(|entry| {
        match entry.get("attributes")?.as_object()?.get(attribute)? {
            // defensive: a per-language map
            Value::Object(localizations) => localizations.values().find_map(filled),
            value => filled(value),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::web::testing::{ANDROID_TOKEN, Answer, FakeTransport, Seen, scratch_config, sign_in};
    use crate::web::{Source, Web};

    fn object(value: Value) -> Object {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn the_host_is_the_configured_one() {
        assert_eq!(API, format!("https://{}", config::API_HOST));
    }

    #[test]
    fn a_client_can_be_shared_between_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<MusicApi>();
    }

    /// Reference values: `MusicApi._url` of amui/api.py.
    #[test]
    fn urls_keep_commas_and_brackets() {
        let albums =
            [("sort", param("-dateAdded")), ("include", param("catalog")), ("limit", param(100)), ("offset", None)];
        assert_eq!(
            url("/v1/me/library/albums", &albums),
            format!("{API}/v1/me/library/albums?sort=-dateAdded&include=catalog&limit=100")
        );
        let views = [
            ("views", param("top-songs,full-albums,singles")),
            ("limit[top-songs]", param(20)),
            ("limit[full-albums]", param(50)),
            ("limit[singles]", param(50)),
        ];
        assert_eq!(
            url("/v1/catalog/us/artists/1", &views),
            format!(
                "{API}/v1/catalog/us/artists/1?views=top-songs,full-albums,singles&limit[top-songs]=20&limit[full-albums]=50&limit[singles]=50"
            )
        );
        let search =
            [("term", param("synth artist & co/é ~x+y")), ("types", param("songs,albums")), ("limit", param(25))];
        assert_eq!(
            url("/v1/catalog/us/search", &search),
            format!("{API}/v1/catalog/us/search?term=synth+artist+%26+co%2F%C3%A9+~x%2By&types=songs,albums&limit=25")
        );
        let removal = [("ids[library-songs]", param("i.AbCdEf4")), ("mode", param("all"))];
        assert_eq!(
            url("/v1/me/library/playlists/p.x/tracks", &removal),
            format!("{API}/v1/me/library/playlists/p.x/tracks?ids[library-songs]=i.AbCdEf4&mode=all")
        );
        assert_eq!(url("/v1/x?offset=2", &[("limit", param(5))]), format!("{API}/v1/x?offset=2&limit=5"));
        assert_eq!(url("/v1/x", &[("a", None)]), format!("{API}/v1/x?"));
        assert_eq!(url("/v1/x", &[]), format!("{API}/v1/x"));
    }

    #[test]
    fn identifiers() {
        let encoded: Vec<String> =
            ["l.AbCdEf1", " p.x y ", "pl.u-é", "a%b", "a?b#c"].iter().map(|id| path_id(id).unwrap()).collect();
        assert_eq!(encoded, ["l.AbCdEf1", "p.x%20y", "pl.u-%C3%A9", "a%25b", "a%3Fb%23c"]);
        for bad in ["", "  ", "a/b"] {
            assert_eq!(path_id(bad).unwrap_err().to_string(), "Invalid Apple Music identifier");
        }
        let longest = "a".repeat(128);
        for (id, cleaned) in [
            ("1000000002", "1000000002"),
            (" i.AbCdEf4 ", "i.AbCdEf4"),
            ("pl.u-Ab_c", "pl.u-Ab_c"),
            (&longest, &longest),
        ] {
            assert_eq!(query_id(id).unwrap(), cleaned);
        }
        for bad in ["", "1,2", "1 2", &"a".repeat(129), "é", "1&x=2", "a/b"] {
            assert_eq!(query_id(bad).unwrap_err().to_string(), "Invalid Apple Music identifier");
        }
    }

    /// Reference values: `_int`, `_str`, `_clamp` of amui/api.py.
    #[test]
    fn numbers_and_text() {
        let ints =
            [json!(5), json!(5.9), json!(-5.9), json!(true), json!("42"), json!(" 42 "), json!("-7"), json!("x")];
        let ints: Vec<Option<i64>> = ints.iter().map(|value| int(Some(value))).collect();
        assert_eq!(ints, [Some(5), Some(5), Some(-5), None, Some(42), Some(42), Some(-7), None]);
        let more = [json!(""), json!(null), json!("4.5"), json!([1]), json!("1e3"), json!(0), json!("007")];
        let more: Vec<Option<i64>> = more.iter().map(|value| int(Some(value))).collect();
        assert_eq!(more, [None, None, None, None, None, Some(0), Some(7)]);
        assert_eq!(int(None), None);
        let texts = [json!(null), json!(""), json!("x"), json!(5), json!(5.0), json!(0), json!(1.5)];
        let texts: Vec<Option<String>> = texts.iter().map(|value| text(Some(value))).collect();
        let expected = [None, None, Some("x"), Some("5"), Some("5.0"), Some("0"), Some("1.5")];
        assert_eq!(texts, expected.map(|text| text.map(str::to_owned)));
        assert_eq!(text_or_empty(Some(&json!(0))), "");
        assert_eq!(text_or_empty(Some(&json!(7))), "7");
        assert_eq!([0, -5, 1, 10, 25, 99].map(|value| clamp(value, 25)), [1, 1, 1, 10, 25, 25]);
    }

    /// Reference values: `_next_offset` and `_album_id_from_url` of amui/api.py.
    #[test]
    fn next_links_and_album_urls() {
        let next = |link: Value| next_offset(&object(json!({"next": link})), 0, 3);
        for (link, expected) in [
            (json!("/v1/me/library/albums?offset=100"), Some(100)),
            (json!("/v1/x?limit=5&offset=25&sort=-dateAdded"), Some(25)),
            (json!("/v1/x"), Some(3)),
            (json!(""), None),
            (json!(null), None),
            (json!("/v1/x?offset=abc"), Some(3)),
            (json!("/v1/x?offset="), Some(3)),
            (json!("/v1/x?offset=%31%30"), Some(10)),
            (json!("/v1/x?offset=+7"), Some(7)),
            (json!("/v1/x?offset=-3"), Some(-3)),
            (json!("/v1/x?a=1#offset=9"), Some(3)),
            (json!(5), None),
            (json!("/v1/x?offset=5&offset=6"), Some(5)),
            (json!("/v1/x?Offset=5"), Some(3)),
            (json!("/v1/x?offset=%20+12+"), Some(12)),
        ] {
            assert_eq!(next(link.clone()), expected, "{link}");
        }
        assert_eq!(next_offset(&Object::new(), 0, 3), None);
        assert_eq!(next_offset(&object(json!({"next": "/v1/x"})), 4, 0), None);
        assert_eq!(next_offset(&object(json!({"next": "/v1/x"})), 4, 2), Some(6));

        for (url, expected) in [
            (json!("https://music.apple.com/us/album/synthetic-album/1000000001?i=1000000002"), Some("1000000001")),
            (json!("https://music.apple.com/us/album/x/123"), Some("123")),
            (json!("https://music.apple.com/us/album/x/123#frag"), Some("123")),
            (json!("https://music.apple.com/us/album/123"), None),
            (json!("https://music.apple.com/us/album/x/12a"), None),
            (json!("https://music.apple.com/us/album//77?x"), Some("77")),
            (json!(null), None),
            (json!(5), None),
            (json!("/album/a/1/album/b/22"), Some("22")),
        ] {
            assert_eq!(album_id_from_url(Some(&url)).as_deref(), expected, "{url}");
        }
    }

    /// Reference values: `datetime.isoformat()` and `datetime.fromisoformat(...).timestamp()`.
    #[test]
    fn timestamps_are_pythons() {
        assert_eq!(utc_iso(Duration::new(1_794_609_665, 123_456_000)), "2026-11-13T22:41:05.123456+00:00");
        assert_eq!(utc_iso(Duration::new(1_767_323_045, 0)), "2026-01-02T03:04:05+00:00");
        assert_eq!(utc_iso(Duration::new(1_709_251_199, 999_999_999)), "2024-02-29T23:59:59.999999+00:00");
        assert_eq!(utc_iso(Duration::ZERO), "1970-01-01T00:00:00+00:00");
        for (text, expected) in [
            ("2026-11-13T22:41:05.123456+00:00", 1_794_609_665.123_456),
            ("2026-01-02T03:04:05+00:00", 1_767_323_045.0),
            ("2026-11-13T22:41:05.123456Z", 1_794_609_665.123_456),
            ("2026-11-13 22:41:05+01:00", 1_794_606_065.0),
            ("2026-11-13T22:41:05.5-05:30", 1_794_629_465.5),
            ("2024-02-29T23:59:59.999999+00:00", 1_709_251_199.999_999),
            ("1970-01-01T00:00:00+00:00", 0.0),
            ("2026-11-13T22:41:05,25+00:00", 1_794_609_665.25),
            ("1969-12-31T23:59:59+00:00", -1.0),
        ] {
            let seconds = parse_utc_iso(text).unwrap_or_else(|| panic!("{text}"));
            assert!((seconds - expected).abs() < 1e-6, "{text}: {seconds}");
        }
        for unreadable in [
            "",
            "2026-11-13",
            "2026-11-13T22:41:05",
            "2026-11-13T22:41+00:00",
            "2026-11-13T22:41:05+0000",
            "2026-11-13T22:41:05+00",
            "2026-02-30T00:00:00+00:00",
            "2026-13-01T00:00:00+00:00",
            "2026-11-13T24:00:00+00:00",
            "2026-11-13T22:41:05.+00:00",
            "2026-11-13T22:41:05+24:00",
            "2026-11-13T22:41:05+0é:00",
            "0000-01-01T00:00:00+00:00",
            "next tuesday",
        ] {
            assert_eq!(parse_utc_iso(unreadable), None, "{unreadable}");
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!((parse_utc_iso(&utc_iso(now)).unwrap() - now.as_secs_f64()).abs() < 1e-5);
    }

    /// What the module's top comment says about text.
    #[test]
    fn only_strings_and_numbers_are_text() {
        let odd = json!({"id": 1000000001, "type": "albums", "attributes": {
            "name": ["a list"], "artistName": {"an": "object"}, "genreNames": [true], "audioTraits": ["atmos", null, 5, ["x"]],
            "releaseDate": false, "trackCount": "12"}});
        let album = parse_album(&odd).unwrap();
        assert_eq!(
            (album.catalog_id.as_deref(), album.name.as_str(), album.artist.as_str()),
            (Some("1000000001"), "", "")
        );
        assert_eq!((album.genre, album.release_date, album.track_count), (None, None, Some(12)));
        assert_eq!(album.audio_traits, ["atmos", "5"]);
        // shapes Python failed on count as missing
        let broken = json!({"id": "i.x", "type": "library-songs", "relationships": ["not", "an", "object"], "attributes": {
            "name": "Song A", "playParams": "none", "artwork": [1], "genreNames": {}}});
        let track = parse_track(&broken, None).unwrap();
        assert_eq!((track.library_id.as_deref(), track.catalog_id, track.name.as_str()), (Some("i.x"), None, "Song A"));
        assert_eq!((track.artwork, track.genre), (None, None));
        for not_a_resource in
            [json!(null), json!("songs"), json!([1]), json!({"type": "songs"}), json!({"attributes": []})]
        {
            assert_eq!(parse_track(&not_a_resource, None), None);
            assert_eq!(parse_item(&not_a_resource), None);
        }
        assert_eq!(parse_artwork(&json!({"url": ""})), None);
        assert_eq!(parse_artwork(&json!({"url": 5})), None);
    }

    /// What the module's top comment says about ids.
    #[test]
    fn an_empty_id_is_no_id() {
        let catalog =
            json!({"catalog": {"data": [{"id": "", "type": "songs"}, {"id": "1000000002", "type": "songs"}]}});
        let song = json!({"id": "i.AbCdEf4", "type": "library-songs", "attributes": {"name": "Song A"}, "relationships": catalog});
        let track = parse_track(&song, None).unwrap();
        assert_eq!((track.catalog_id.as_deref(), track.playable(), track.key().as_str()), (None, false, "i.AbCdEf4"));
        let list = json!({"id": "p.AbCdEf2", "type": "library-playlists", "attributes": {"name": "List A"}, "relationships": catalog});
        let playlist = parse_playlist(&list).unwrap();
        assert_eq!((playlist.catalog_id.as_deref(), playlist.key().as_str()), (None, "p.AbCdEf2"));
        // an id that is there is the first one, as in Python
        let catalog = json!({"catalog": {"data": [{"id": "1000000003"}, {"id": "1000000002"}]}});
        let song = json!({"id": "i.AbCdEf4", "type": "library-songs", "attributes": {}, "relationships": catalog});
        assert_eq!(parse_track(&song, None).unwrap().catalog_id.as_deref(), Some("1000000003"));
    }

    /// The real HTTP layer over a network that answers every request with `answer(request)`.
    struct Served {
        _dir: tempfile::TempDir,
        config: Arc<Config>,
        web: Arc<Web>,
        api: MusicApi,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Served {
        fn new(answer: impl Fn(&Seen) -> Answer + Send + Sync + 'static) -> Served {
            let (dir, config) = scratch_config();
            sign_in(&config, Source::Android, ANDROID_TOKEN);
            let (transport, seen) = FakeTransport::new(move |request| Ok(answer(request)));
            let web = Arc::new(Web::with_transport(config.clone(), Box::new(transport)));
            web.preset_developer_token("developer", util::unix_now() + 90.0 * 86_400.0);
            let api = MusicApi::new(config.clone(), web.clone());
            Served { _dir: dir, config, web, api, seen }
        }

        /// Another client on the same folders, as after a restart.
        fn restarted(&self) -> MusicApi {
            MusicApi::new(self.config.clone(), self.web.clone())
        }

        fn requests(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    #[test]
    fn requests_reach_the_wire_as_python_sends_them() {
        let served = Served::new(|request| match request.method.as_str() {
            "GET" => Answer::ok(
                br#"{"data": [{"id": "2000000001", "type": "artists", "attributes": {"name": "Artist A"}}]}"#,
            ),
            "POST" => Answer::ok(
                br#"{"data": [{"id": "p.AbCdEf2", "type": "library-playlists", "attributes": {"name": "List A"}}]}"#,
            ),
            _ => Answer::status(204),
        });
        let api = &served.api;
        api.set_catalog_storefront(Some("jp")).unwrap();
        assert_eq!(api.catalog_artist("2000000001").unwrap().0.name, "Artist A");
        api.remove_from_playlist("p.AbCdEf2", ["i.AbCdEf4"]).unwrap();
        let created = api.create_playlist("List Ä", "", [] as [&str; 0]).unwrap();
        assert_eq!(created.library_id.as_deref(), Some("p.AbCdEf2"));
        let seen = served.seen.lock().unwrap();
        let views = "views=top-songs,full-albums,singles&limit[top-songs]=20&limit[full-albums]=50&limit[singles]=50";
        assert_eq!(seen[0].url, format!("{API}/v1/catalog/jp/artists/2000000001?{views}"));
        assert_eq!((seen[0].method.as_str(), seen[0].headers["Accept"].as_str()), ("GET", "application/json"));
        assert_eq!(seen[0].token().as_deref(), Some(ANDROID_TOKEN));
        let removal = "/v1/me/library/playlists/p.AbCdEf2/tracks?ids[library-songs]=i.AbCdEf4&mode=all";
        assert_eq!((seen[1].method.as_str(), seen[1].url.as_str()), ("DELETE", format!("{API}{removal}").as_str()));
        assert_eq!((&seen[1].body, seen[1].headers.get("Content-Type")), (&None, None));
        assert_eq!(
            (seen[2].method.as_str(), seen[2].url.as_str()),
            ("POST", format!("{API}/v1/me/library/playlists").as_str())
        );
        assert_eq!(seen[2].body.as_deref(), Some(r#"{"attributes":{"name":"List Ä"}}"#.as_bytes()));
        assert_eq!(seen[2].headers["Content-Type"], "application/json");
        assert_eq!(seen[2].token().as_deref(), Some(ANDROID_TOKEN));
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn the_storefront_is_kept_in_the_state_folder_as_python_writes_it() {
        let served = Served::new(|_| Answer::ok(br#"{"data": [{"id": "US", "type": "storefronts"}]}"#));
        let api = &served.api;
        assert_eq!(api.cache_dir(), served.config.state_dir.join("api"));
        assert_eq!(api.ttl(), TTL_DEFAULT);
        assert_eq!(api.storefront().unwrap(), "us");
        let file = api.cache_dir().join("storefront.json");
        let fetched = read_object(&file).unwrap()["fetched_at"].as_str().unwrap().to_string();
        assert!((util::unix_now() - parse_utc_iso(&fetched).unwrap()).abs() < 60.0, "{fetched}");
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("{{\"id\": \"us\", \"fetched_at\": \"{fetched}\"}}\n"));
        api.set_catalog_storefront(Some(" GB ")).unwrap();
        let file = api.cache_dir().join("storefront-override.json");
        let set = read_object(&file).unwrap()["set_at"].as_str().unwrap().to_string();
        assert!((util::unix_now() - parse_utc_iso(&set).unwrap()).abs() < 60.0, "{set}");
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("{{\"id\": \"gb\", \"set_at\": \"{set}\"}}\n"));
        assert_eq!(served.requests(), 1);
    }

    /// Reference values: the Python client against the same endless listing.
    #[test]
    fn following_next_links_stops_at_the_page_cap() {
        let served = Served::new(|request| {
            let path = request.url.strip_prefix(API).unwrap();
            let body = if path == "/v1/me/storefront" {
                json!({"data": [{"id": "us", "type": "storefronts"}]})
            } else if path == "/v1/catalog/us/albums/1?include=tracks" {
                let tracks = json!({"data": [{"id": "10", "type": "songs", "attributes": {"name": "Song A"}}],
                                    "next": "/v1/catalog/us/albums/1/tracks?offset=1"});
                json!({"data": [{"id": "1", "type": "albums", "attributes": {"name": "Album A"}, "relationships": {"tracks": tracks}}]})
            } else {
                // every page names a next one
                let (base, query) = path.split_once('?').unwrap();
                let offset = query.strip_prefix("offset=").map_or(0, |offset| offset.parse::<u32>().unwrap());
                let song =
                    json!({"id": format!("i.{offset}"), "type": "library-songs", "attributes": {"name": "Song A"}});
                json!({"data": [song], "next": format!("{base}?offset={}", offset + 1)})
            };
            Answer::ok(body.to_string().as_bytes())
        });
        assert_eq!(served.api.library_album_tracks("l.x").unwrap().len(), MAX_PAGES);
        assert_eq!(served.requests(), MAX_PAGES);
        let last = |served: &Served| served.seen.lock().unwrap().last().unwrap().url.clone();
        assert_eq!(last(&served), format!("{API}/v1/me/library/albums/l.x/tracks?offset=59"));
        // an album's first tracks come with the album: one page less to ask for
        let (_, tracks) = served.api.catalog_album("1").unwrap();
        assert_eq!(tracks.len(), MAX_PAGES);
        assert_eq!(served.requests(), MAX_PAGES + 2 + MAX_PAGES - 1); // + the storefront and the album
        assert_eq!(last(&served), format!("{API}/v1/catalog/us/albums/1/tracks?offset=59"));
    }

    #[test]
    fn a_full_cache_drops_expired_answers_then_the_oldest() {
        let served = Served::new(|_| Answer::ok(br#"{"data": []}"#));
        let now = Arc::new(Mutex::new(0.0));
        let clock = now.clone();
        let api = served.restarted().with_clock(move || *clock.lock().unwrap());
        let tracks = |id: usize| api.library_album_tracks(&format!("l.{id}")).unwrap();
        for id in 0..MAX_CACHE_ENTRIES {
            tracks(id);
        }
        tracks(0);
        tracks(MAX_CACHE_ENTRIES - 1);
        assert_eq!(served.requests(), MAX_CACHE_ENTRIES); // all of them still cached
        tracks(1000); // one too many: the first one stored goes
        tracks(1);
        assert_eq!(served.requests(), MAX_CACHE_ENTRIES + 1);
        tracks(0); // asked for again, and stored at the expense of the second one
        tracks(2);
        assert_eq!(served.requests(), MAX_CACHE_ENTRIES + 2);
        tracks(1);
        assert_eq!(served.requests(), MAX_CACHE_ENTRIES + 3);
        assert_eq!(api.state().responses.len(), MAX_CACHE_ENTRIES);
        // expired answers go first, all of them
        *now.lock().unwrap() = TTL_DEFAULT + 1.0;
        tracks(2000);
        assert_eq!(api.state().responses.len(), 1);
    }

    #[test]
    fn an_answer_on_its_way_while_the_cache_is_emptied_is_not_kept() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let client: Arc<OnceLock<Arc<MusicApi>>> = Arc::default();
        let emptying = Arc::new(AtomicBool::new(false));
        let (during, flag) = (client.clone(), emptying.clone());
        let served = Served::new(move |_| {
            if flag.swap(false, Ordering::SeqCst) {
                during.get().unwrap().clear_cache();
            }
            Answer::ok(br#"{"data": []}"#)
        });
        let api = client.get_or_init(|| Arc::new(served.restarted())).clone();
        // an answer is kept for the next time
        api.library_album_tracks("l.1").unwrap();
        api.library_album_tracks("l.1").unwrap();
        assert_eq!(served.requests(), 1);
        // the cache is emptied while the request runs: its answer is not kept
        emptying.store(true, Ordering::SeqCst);
        api.library_album_tracks("l.2").unwrap();
        api.library_album_tracks("l.2").unwrap();
        assert_eq!(served.requests(), 3);
        api.library_album_tracks("l.2").unwrap();
        assert_eq!(served.requests(), 3);
    }

    #[test]
    fn a_stale_or_odd_storefront_file_is_not_used() {
        let served = Served::new(|_| Answer::ok(br#"{"data": [{"id": "us", "type": "storefronts"}]}"#));
        let folder = served.api.cache_dir();
        let file = folder.join("storefront.json");
        util::create_private_dirs(folder).unwrap();
        // a week old, without a time, with a time in another form, of another shape: asked again
        let old = utc_iso(Duration::from_secs_f64(util::unix_now() - STOREFRONT_MAX_AGE - 60.0));
        let unusable = [
            format!(r#"{{"id": "jp", "fetched_at": "{old}"}}"#),
            r#"{"id": "jp"}"#.to_string(),
            r#"{"id": "jp", "fetched_at": "2026-11-13"}"#.to_string(),
            r#"{"id": "JP", "fetched_at": "2999-01-01T00:00:00+00:00"}"#.to_string(),
            r#"["jp"]"#.to_string(),
            "{not json".to_string(),
        ];
        for (count, saved) in unusable.iter().enumerate() {
            fs::write(&file, saved).unwrap();
            assert_eq!(served.restarted().storefront().unwrap(), "us", "{saved}");
            assert_eq!(served.requests(), count + 1, "{saved}");
        }
        // what the request left there is used, and so is a file not yet a week old
        assert_eq!(served.restarted().storefront().unwrap(), "us");
        let recent = utc_iso(Duration::from_secs_f64(util::unix_now() - STOREFRONT_MAX_AGE + 60.0));
        fs::write(&file, format!("{{\"id\": \"jp\", \"fetched_at\": \"{recent}\"}}\n")).unwrap();
        assert_eq!(served.restarted().storefront().unwrap(), "jp");
        assert_eq!(served.requests(), unusable.len());
        // never through a symbolic link, to read or to write
        let elsewhere = folder.join("elsewhere.json");
        fs::rename(&file, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
        assert_eq!(served.restarted().storefront().unwrap(), "us");
        assert_eq!(served.requests(), unusable.len() + 1);
        assert!(fs::read_to_string(&elsewhere).unwrap().contains("\"jp\""));
        assert!(fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
    }
}
