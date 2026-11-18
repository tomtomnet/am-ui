//! What the Python engine's client did not have: stations, the pages made of shelves, and the
//! catalog's current version of a song.
//!
//! * [`parse_station`] and [`parse_shelf_item`] read the resources.
//! * [`MusicApi::home`], [`MusicApi::browse`] and [`MusicApi::radio`] are the pages Home, New and
//!   Radio as lists of [`Shelf`].
//! * [`MusicApi::station_next_tracks`] hands out the songs of a station, a batch per call, and
//!   [`MusicApi::continuous_station`] makes the station that goes on where a queue ends
//!   (Autoplay).
//! * [`MusicApi::equivalent_song`] finds the version of a song the catalog serves today, for a
//!   library song that points to a version Apple withdrew.
//! * [`MusicApi::library_album`], [`MusicApi::library_playlist`] and [`MusicApi::library_artist`]
//!   look up one item of the library by its id, for a page that is opened from a link;
//!   [`MusicApi::library_song_links`] the album and the artist of a song of the library.
//!
//! The requests are those Apple's web player makes for the same pages and for its own Autoplay;
//! they are undocumented and may change. A shelf this client cannot show (links, videos,
//! curators, concerts) is left out, as is an item of a kind it does not know.

use serde_json::{Value, json};

use super::{
    ApiError, MusicApi, Object, Param, TTL_CATALOG, clamp, data, failed, param, parse_album, parse_artist,
    parse_artwork, parse_item, parse_playlist, path_id, present, query_id, rel_items, single, text, text_or_empty,
    truthy,
};
use crate::types::{Album, Artist, Item, Playlist, Shelf, ShelfLayout, Station, Track};

/// Stations the Radio page lists as recently played.
pub const RECENT_STATIONS_LIMIT: i64 = 30;
/// Songs of a queue a continuous station is made from: Apple's web player sends the last ten.
pub const CONTINUOUS_SEEDS: usize = 10;
/// Most songs asked of a station in one request.
pub const NEXT_TRACKS_LIMIT: i64 = 25;
/// Songs looked at to find the current version of a song.
pub const EQUIVALENT_CANDIDATES: i64 = 25;
/// Two versions of a song are the same recording when their lengths differ by no more.
pub const EQUIVALENT_DURATION_MS: i64 = 1500;

// Apple's numbers for the elements of an editorial page (`editorialElementKind`).
const HERO_LIST: &str = "316";
const TRACK_SWOOSH: &str = "327";
const RECENT_MARKER: &str = "332";
const LINK_LIST: &str = "391";
const PARAGRAPH: &str = "404";
const SINGLE_LINK: &str = "405";
const LIVE_RADIO: &str = "488";

/// stations resource -> [`Station`].
pub fn parse_station(item: &Value) -> Option<Station> {
    let item = item.as_object()?;
    let attrs = item.get("attributes")?.as_object()?;
    let id = text(item.get("id"))?;
    let name = text_or_empty(attrs.get("name"));
    let live = truthy(attrs.get("isLive"));
    // Apple's short note or tagline; its `name` repeats the station's name
    let note = ["plainEditorialNotes", "editorialNotes"].iter().find_map(|notes| {
        let notes = attrs.get(*notes)?.as_object()?;
        ["short", "tagline"].iter().find_map(|key| text(notes.get(*key))).filter(|note| *note != name)
    });
    let subtitle = note.unwrap_or_else(|| if live { "Live Radio" } else { "Station" }.to_string());
    Some(Station {
        id,
        name,
        subtitle,
        artwork: attrs.get("artwork").and_then(parse_artwork),
        live,
        kind: text_or_empty(attrs.get("kind")),
        format: text_or_empty(attrs.get("playParams").and_then(|play| play.as_object()?.get("format"))),
    })
}

/// A resource a shelf can show: what [`parse_item`] takes, and stations.
pub fn parse_shelf_item(item: &Value) -> Option<Item> {
    match text_or_empty(item.as_object()?.get("type")).as_str() {
        "stations" => parse_station(item).map(Item::Station),
        _ => parse_item(item),
    }
}

/// A songs resource -> [`Track`]; `None` for a resource of another type.
fn parse_song(item: &Value) -> Option<Track> {
    match parse_item(item)? {
        Item::Track(track) => Some(track),
        _ => None,
    }
}

/// A song a continuous station is made from, with the album, playlist or station it was played
/// from when that is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub id: String,
    /// "songs" for a catalog id, "library-songs" for a library id.
    pub kind: &'static str,
    /// (type, id) of what the song was played from: ("albums", "1234567890"),
    /// ("library-playlists", "p.…").
    pub container: Option<(String, String)>,
}

impl Seed {
    /// The seed of a song: its catalog id, else its library id; `None` for a song without either.
    pub fn of(track: &Track) -> Option<Seed> {
        let (id, kind) = match (present(&track.catalog_id), present(&track.library_id)) {
            (Some(id), _) => (id, "songs"),
            (None, Some(id)) => (id, "library-songs"),
            (None, None) => return None,
        };
        Some(Seed { id: id.to_string(), kind, container: None })
    }

    pub fn within(mut self, kind: &str, id: &str) -> Seed {
        self.container = Some((kind.to_string(), id.to_string()));
        self
    }

    fn to_json(&self) -> Result<Value, ApiError> {
        let mut entry = json!({"id": query_id(&self.id)?, "type": self.kind});
        if let Some((kind, id)) = &self.container {
            entry["meta"] = json!({"container": {"id": query_id(id)?, "type": query_id(kind)?}});
        }
        Ok(entry)
    }
}

/// The items of a shelf with the line that goes over each (empty where there is none).
fn shelf_items<'a>(entries: impl Iterator<Item = (&'a Value, String)>) -> (Vec<Item>, Vec<String>) {
    entries.filter_map(|(entry, note)| Some((parse_shelf_item(entry)?, note))).unzip()
}

/// Cards, or the songs layout when the shelf holds nothing but songs.
fn layout_of(items: &[Item]) -> ShelfLayout {
    if items.iter().all(|item| matches!(item, Item::Track(_))) { ShelfLayout::Songs } else { ShelfLayout::Cards }
}

/// Why Apple put an item on Home: `meta.reason.stringForDisplay`.
fn reason(entry: &Value) -> String {
    text_or_empty(entry.get("meta").and_then(|meta| meta.get("reason")?.get("stringForDisplay")))
}

/// One element of an editorial page -> (its kind, the shelf); `None` for an element with nothing
/// this client can show.
fn editorial_shelf(element: &Object) -> Option<(String, Shelf)> {
    let attrs = element.get("attributes")?.as_object()?;
    let kind = text_or_empty(attrs.get("editorialElementKind"));
    let title = Some(text_or_empty(attrs.get("name"))).filter(|name| !name.is_empty());
    let title = title.unwrap_or_else(|| text_or_empty(attrs.get("title")));
    // an element lists its items itself, or through child elements that hold one item each
    let children = || {
        rel_items(element, "children").filter_map(|child| {
            let child = child.as_object()?;
            let badge = child.get("attributes").and_then(|attrs| attrs.get("designBadge"));
            Some((rel_items(child, "contents").next()?, text_or_empty(badge)))
        })
    };
    let (layout, items, notes) = match kind.as_str() {
        RECENT_MARKER | LINK_LIST | PARAGRAPH | SINGLE_LINK => return None,
        HERO_LIST => {
            let (items, notes) = shelf_items(children());
            (ShelfLayout::Hero, items, notes)
        }
        TRACK_SWOOSH => {
            let songs = rel_items(element, "contents").filter_map(parse_song);
            (ShelfLayout::Songs, songs.map(Item::Track).collect(), Vec::new())
        }
        _ => {
            let (mut items, _) = shelf_items(rel_items(element, "contents").map(|entry| (entry, String::new())));
            if items.is_empty() {
                (items, _) = shelf_items(children());
            }
            (layout_of(&items), items, Vec::new())
        }
    };
    if items.is_empty() {
        return None;
    }
    Some((kind, Shelf { id: text(element.get("id")), title, layout, items, notes }))
}

/// The one of a storefront's language tags that fits the language wanted: the same tag, else
/// the first of the same language ("en-GB" for "en-US"); `None` when the storefront has none.
fn fitting_language(wanted: &str, offered: &[String]) -> Option<String> {
    let language = |tag: &str| tag.split('-').next().unwrap_or_default().to_ascii_lowercase();
    let same = offered.iter().find(|tag| tag.eq_ignore_ascii_case(wanted));
    same.or_else(|| offered.iter().find(|tag| language(tag) == language(wanted))).cloned()
}

/// A title or a name without what differs between spellings: case, spaces and punctuation.
fn folded(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

impl MusicApi {
    // --- the language of Apple's own words

    /// The `l` parameter for requests whose answers carry Apple's own words (the titles of
    /// shelves, notes, station names): the language of the desktop when `storefront` has it,
    /// else none, and Apple answers in the storefront's own language. What a storefront offers
    /// (`GET /v1/storefronts/{id}`) is asked for once.
    fn language(&self, storefront: &str) -> Option<String> {
        let wanted = self.language.as_deref()?;
        let known = self.state().languages.get(storefront).cloned();
        let offered = match known {
            Some(offered) => offered,
            None => {
                let path = format!("/v1/storefronts/{}", path_id(storefront).ok()?);
                let body = self.get(&path, &[], Some(TTL_CATALOG)).ok()?;
                let tags = single(&body).ok()?.get("attributes")?.get("supportedLanguageTags")?.as_array()?;
                let offered: Vec<String> = tags.iter().filter_map(|tag| tag.as_str().map(str::to_owned)).collect();
                self.state().languages.insert(storefront.to_string(), offered.clone());
                offered
            }
        };
        fitting_language(wanted, &offered)
    }

    // --- pages of shelves

    /// The Home page: `GET /v1/me/recommendations?name=listen-now…`, one shelf per recommendation
    /// that holds something this client can show. The shelf of top picks is a
    /// [`ShelfLayout::Hero`] shelf whose notes say why each item is there.
    pub fn home(&self) -> Result<Vec<Shelf>, ApiError> {
        let types = "activities,albums,apple-curators,artists,curators,editorial-items,library-albums,\
                     library-playlists,music-videos,playlists,songs,stations,uploaded-audios,uploaded-videos";
        let params: [Param; 16] = [
            ("name", param("listen-now")),
            ("extend[playlists]", param("artistNames")),
            ("include[albums]", param("artists")),
            ("include[library-playlists]", param("catalog")),
            ("include[stations]", param("radio-show")),
            ("include[personal-recommendation]", param("primary-content")),
            ("fields[artists]", param("name,artwork,url")),
            ("omit[resource]", param("autos")),
            ("extend[stations]", param("airTime,supportsAirTimeUpdates")),
            ("meta[stations]", param("inflectionPoints")),
            ("types", param(types)),
            ("with", param("library")),
            ("extend", param("plainEditorialNotes,plainEditorialCard,editorialVideo")),
            ("platform", param("web")),
            ("l", self.language(&self.storefront()?)),
            ("timezone", param("+00:00")),
        ];
        let body = self.get("/v1/me/recommendations", &params, Some(TTL_CATALOG))?;
        let mut shelves = Vec::new();
        for recommendation in data(&body)?.into_iter().filter_map(Value::as_object) {
            let attrs = recommendation.get("attributes").and_then(Value::as_object);
            let title = match attrs.and_then(|attrs| attrs.get("title")) {
                Some(Value::Object(title)) => title.get("stringForDisplay"),
                other => other,
            };
            let display = attrs.and_then(|attrs| attrs.get("display")?.as_object()?.get("kind"));
            let (items, notes) = shelf_items(rel_items(recommendation, "contents").map(|entry| (entry, reason(entry))));
            if items.is_empty() {
                continue;
            }
            let hero = text_or_empty(display).contains("Hero");
            shelves.push(Shelf {
                id: text(recommendation.get("id")),
                title: text_or_empty(title),
                layout: if hero { ShelfLayout::Hero } else { layout_of(&items) },
                items,
                notes: if hero { notes } else { Vec::new() },
            });
        }
        Ok(shelves)
    }

    /// The elements of an editorial page (`GET /v1/editorial/{sf}/groupings?name=…`) as shelves,
    /// each with Apple's number for its kind.
    fn editorial(&self, name: &str) -> Result<Vec<(String, Shelf)>, ApiError> {
        let storefront = self.catalog_storefront()?;
        let album_fields = "artistName,artistUrl,artwork,contentRating,editorialArtwork,plainEditorialNotes,name,\
                            playParams,releaseDate,url,trackCount";
        let params: [Param; 14] = [
            ("name", param(name)),
            ("omit[resource:artists]", param("autos")),
            ("relate[songs]", param("albums")),
            ("include[albums]", param("artists")),
            ("include[songs]", param("artists")),
            ("include[music-videos]", param("artists")),
            ("include[stations]", param("events,radio-show")),
            ("extend[station-events]", param("editorialVideo")),
            ("fields[artists]", param("name,url,artwork,editorialArtwork,genreNames,plainEditorialNotes")),
            ("fields[albums]", param(album_fields)),
            ("tabs", param("subscriber")),
            ("extend", param("editorialArtwork,artistUrl,plainEditorialNotes")),
            ("platform", param("web")),
            ("l", self.language(&storefront)),
        ];
        let path = format!("/v1/editorial/{storefront}/groupings");
        let body = self.get(&path, &params, Some(TTL_CATALOG))?;
        let grouping = data(&body)?.into_iter().next().and_then(Value::as_object);
        let tab = grouping.and_then(|grouping| rel_items(grouping, "tabs").next()?.as_object());
        let elements = tab.into_iter().flat_map(|tab| rel_items(tab, "children")).filter_map(Value::as_object);
        Ok(elements.filter_map(editorial_shelf).collect())
    }

    /// The New page: what Apple's editors put forward in the catalog's storefront.
    pub fn browse(&self) -> Result<Vec<Shelf>, ApiError> {
        Ok(self.editorial("music")?.into_iter().map(|(_, shelf)| shelf).collect())
    }

    /// The Radio page: Apple's radio page with the recently played stations after its opening
    /// rows (the featured stations and the live ones). The page is shown without them when they
    /// cannot be had, unless the reason is the sign-in.
    pub fn radio(&self) -> Result<Vec<Shelf>, ApiError> {
        let page = self.editorial("radio")?;
        let recent = match self.recent_stations(RECENT_STATIONS_LIMIT) {
            Ok(stations) => stations,
            Err(error) if error.is_auth() => return Err(error),
            Err(_) => Vec::new(),
        };
        let opening = page.iter().take_while(|(kind, _)| matches!(kind.as_str(), HERO_LIST | LIVE_RADIO)).count();
        let mut shelves: Vec<Shelf> = page.into_iter().map(|(_, shelf)| shelf).collect();
        if !recent.is_empty() {
            let recent = Shelf {
                title: "Recently Played".to_string(),
                items: recent.into_iter().map(Item::Station).collect(),
                ..Shelf::default()
            };
            shelves.insert(opening, recent);
        }
        Ok(shelves)
    }

    // --- one item of the library

    /// `GET /v1/me/library/albums/{id}?include=catalog`: the album with its catalog id and audio
    /// traits when it has a catalog version. Its songs: [`MusicApi::library_album_tracks`].
    pub fn library_album(&self, library_id: &str) -> Result<Album, ApiError> {
        let path = format!("/v1/me/library/albums/{}", path_id(library_id)?);
        let body = self.get(&path, &[("include", param("catalog"))], None)?;
        parse_album(single(&body)?).ok_or_else(|| failed("Apple returned an album without attributes"))
    }

    /// `GET /v1/me/library/playlists/{id}`. Its songs: [`MusicApi::library_playlist_tracks`].
    pub fn library_playlist(&self, library_id: &str) -> Result<Playlist, ApiError> {
        let path = format!("/v1/me/library/playlists/{}", path_id(library_id)?);
        let body = self.get(&path, &[("extend[library-playlists]", param("tags"))], None)?;
        parse_playlist(single(&body)?).ok_or_else(|| failed("Apple returned a playlist without attributes"))
    }

    /// `GET /v1/me/library/artists/{id}?include=catalog`: the artist with the catalog's artwork
    /// and id when there is a catalog artist. The albums: [`MusicApi::library_artist_albums`].
    pub fn library_artist(&self, library_id: &str) -> Result<Artist, ApiError> {
        let path = format!("/v1/me/library/artists/{}", path_id(library_id)?);
        let body = self.get(&path, &[("include", param("catalog"))], None)?;
        parse_artist(single(&body)?).ok_or_else(|| failed("Apple returned an artist without attributes"))
    }

    /// The album and the artist of a song of the library, as the library has them: (album id
    /// "l.…", artist id "r.…"), each `None` when Apple names none. A song of a library list
    /// does not say (`GET /v1/me/library/songs/{id}?include=albums,artists`).
    pub fn library_song_links(&self, library_id: &str) -> Result<(Option<String>, Option<String>), ApiError> {
        let path = format!("/v1/me/library/songs/{}", path_id(library_id)?);
        let body = self.get(&path, &[("include", param("albums,artists"))], Some(TTL_CATALOG))?;
        let song = single(&body)?.as_object().ok_or_else(|| failed("Apple returned a song without attributes"))?;
        let first = |name: &str, prefix: &str| {
            rel_items(song, name).filter_map(|item| text(item.get("id"))).find(|id| id.starts_with(prefix))
        };
        Ok((first("albums", "l."), first("artists", "r.")))
    }

    // --- stations

    /// `GET /v1/me/recent/radio-stations`: the stations the account played last, newest first.
    pub fn recent_stations(&self, limit: i64) -> Result<Vec<Station>, ApiError> {
        let params = [
            ("limit", param(clamp(limit, RECENT_STATIONS_LIMIT))),
            ("include[stations]", param("radio-show")),
            ("omit[resource]", param("autos")),
            ("l", self.language(&self.storefront()?)),
        ];
        let body = self.get("/v1/me/recent/radio-stations", &params, None)?;
        Ok(data(&body)?.into_iter().filter_map(parse_station).collect())
    }

    /// `GET /v1/catalog/{sf}/stations/{id}`.
    pub fn station(&self, station_id: &str) -> Result<Station, ApiError> {
        self.shown(|storefront| {
            let path = format!("/v1/catalog/{storefront}/stations/{}", path_id(station_id)?);
            let body = self.get(&path, &[("l", self.language(storefront))], Some(TTL_CATALOG))?;
            let station = data(&body)?.into_iter().find_map(parse_station);
            station.ok_or_else(|| failed("Apple returned no station"))
        })
    }

    /// The next songs of a station: `POST /v1/me/stations/next-tracks/{id}`. Every call moves the
    /// station on, so the answer is never kept. `limit` is the number of songs wanted (Apple
    /// decides how many without one); a station that has run out answers with none.
    pub fn station_next_tracks(&self, station_id: &str, limit: Option<i64>) -> Result<Vec<Track>, ApiError> {
        let path = format!("/v1/me/stations/next-tracks/{}", path_id(station_id)?);
        let params = [("limit", limit.map(|limit| clamp(limit, NEXT_TRACKS_LIMIT).to_string()))];
        let Some(answer) = self.mutate("POST", &path, &params, None, &[])? else { return Ok(Vec::new()) };
        Ok(data(&answer)?.into_iter().filter_map(parse_song).collect())
    }

    /// The station that goes on where a queue ends: `POST /v1/me/stations/continuous` with the
    /// last songs of the queue (at most [`CONTINUOUS_SEEDS`] are sent, the last ones). Its songs
    /// come from [`MusicApi::station_next_tracks`].
    pub fn continuous_station(&self, seeds: &[Seed]) -> Result<Station, ApiError> {
        let last = &seeds[seeds.len().saturating_sub(CONTINUOUS_SEEDS)..];
        if last.is_empty() {
            return Err(ApiError::Value("A station needs at least one song to start from".to_string()));
        }
        let entries = last.iter().map(Seed::to_json).collect::<Result<Vec<_>, _>>()?;
        let body = json!({"data": entries});
        let answer = self.mutate("POST", "/v1/me/stations/continuous", &[], Some(&body), &[])?;
        // one station, or a list holding it
        let station = answer.as_ref().and_then(|answer| answer.get("results")?.get("station"));
        let station = match station {
            Some(Value::Array(stations)) => stations.first(),
            other => other,
        };
        station.and_then(parse_station).ok_or_else(|| failed("Apple returned no station for these songs"))
    }

    // --- the current version of a song

    /// The catalog's current version of `track`, for a song whose own catalog version Apple no
    /// longer serves (a library song added from a release that was withdrawn and published
    /// again): a song with the same title and artist whose length differs by at most
    /// [`EQUIVALENT_DURATION_MS`], from the same album and with the same content rating when
    /// there is a choice. `None` when the catalog has no such song. The song has to play for the
    /// account: it is looked for in the account's storefront, whatever region the catalog shows.
    pub fn equivalent_song(&self, track: &Track) -> Result<Option<Track>, ApiError> {
        let (name, artist) = (folded(&track.name), folded(&track.artist));
        if name.is_empty() || artist.is_empty() {
            return Ok(None);
        }
        let term = format!("{} {}", track.name, track.artist);
        let found = self.search_in(Some(&self.storefront()?), &term, EQUIVALENT_CANDIDATES)?;
        let album = folded(&track.album);
        let apart = |song: &Track| match (song.duration_ms, track.duration_ms) {
            (Some(theirs), Some(ours)) => (theirs - ours).abs(),
            _ => 0,
        };
        let same = found.songs.into_iter().filter(|song| {
            song.playable()
                && song.catalog_id != track.catalog_id
                && folded(&song.name) == name
                && folded(&song.artist) == artist
                && apart(song) <= EQUIVALENT_DURATION_MS
        });
        // the same album first, then the same rating, then the closest length; Apple's order last
        Ok(same.min_by_key(|song| {
            (folded(&song.album) != album, song.content_rating != track.content_rating, apart(song))
        }))
    }
}
