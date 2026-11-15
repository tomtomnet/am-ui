//! Generated data for `am-ui --demo`: a library, a catalog, playlists, shelves and lyrics made
//! from the invented albums of `crates/am-ui/demo` (`gen_demo.py` there makes their covers). No
//! account, no network.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use amui::types::{Album, Artist, Artwork, Line, Lyrics, Page, Playlist, SearchResults, Timing, Track, Word};

use crate::data::{ArtistPage, Collection, Item, Music, Problem, Ref, Res, Shelf, ShelfLayout, Station};

const LIBRARY_ALBUMS: usize = 160;
const WORDS: [&str; 24] = [
    "Amber", "Harbor", "Velvet", "Orbit", "Cascade", "Lantern", "Paper", "Thunder", "Quiet", "Signal", "Meadow",
    "Glass", "Winter", "Honey", "Static", "Juniper", "Marble", "Neon", "Drift", "Solace", "Ivory", "Cinder", "Tide",
    "Mirage",
];

#[derive(serde::Deserialize, Clone)]
struct Source {
    title: String,
    artist: String,
    cover: String,
}

struct StoredPlaylist {
    playlist: Playlist,
    tracks: Vec<Track>,
}

pub struct DemoMusic {
    assets: PathBuf,
    albums: Vec<Source>,
    lines: Vec<String>,
    playlists: Mutex<Vec<StoredPlaylist>>,
    favorites: Mutex<HashSet<String>>,
    /// The songs of the library, made when they are first asked for.
    songs: OnceLock<Vec<Track>>,
    delay: Duration,
    /// `AMUI_DEMO_SEARCH_MS`: how long a search takes, when it is not the usual delay (twice it
    /// for Apple Music's catalog).
    search_delay: Option<Duration>,
    /// `AMUI_DEMO_SHELVES`: how Home, New and Radio answer (see `shelves`).
    shelf_mode: String,
    shelf_requests: AtomicUsize,
}

fn page<T: Clone>(items: &[T], offset: i64, limit: i64) -> Page<T> {
    let start = (offset.max(0) as usize).min(items.len());
    let end = (start + limit.max(0) as usize).min(items.len());
    let next = (end < items.len()).then_some(end as i64);
    Page::new(items[start..end].to_vec(), next, Some(items.len() as i64))
}

/// The timed words of a line sung from `begin` for 3.8 seconds, as Apple's word timing has them:
/// a word of seven letters or more in two syllables (timed one after the other, without a space
/// between them), and with `held` the last word as long as three others.
fn sung(text: &str, begin: f64, held: bool) -> Vec<Word> {
    let pieces: Vec<&str> = text.split(' ').collect();
    let weight = |k: usize| if held && k + 1 == pieces.len() { 3.0 } else { 1.0 };
    let step = 3.8 / (0..pieces.len()).map(weight).sum::<f64>().max(1.0);
    let mut at = begin;
    let mut words = Vec::new();
    for (k, piece) in pieces.iter().enumerate() {
        let letters = piece.chars().count();
        let middle = piece.char_indices().nth(letters / 2).map_or(piece.len(), |(index, _)| index);
        let syllables = if letters >= 7 { vec![&piece[..middle], &piece[middle..]] } else { vec![*piece] };
        let length = weight(k) * step / syllables.len() as f64;
        for (s, syllable) in syllables.iter().enumerate() {
            let space = if s + 1 == syllables.len() && k + 1 < pieces.len() { " " } else { "" };
            words.push(Word { begin: at, end: Some(at + length), text: format!("{syllable}{space}") });
            at += length;
        }
    }
    words
}

impl DemoMusic {
    pub fn new(root: &Path) -> DemoMusic {
        let assets = root.join("crates/am-ui/demo");
        let albums: Vec<Source> = std::fs::read(assets.join("albums.json"))
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        let lines = std::fs::read(assets.join("nowplaying.json"))
            .ok()
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
            .and_then(|value| serde_json::from_value(value["lines"].clone()).ok())
            .unwrap_or_default();
        let delay = std::env::var("AMUI_DEMO_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
        let demo = DemoMusic {
            assets,
            albums,
            lines,
            playlists: Mutex::new(Vec::new()),
            favorites: Mutex::new(HashSet::new()),
            songs: OnceLock::new(),
            delay: Duration::from_millis(delay),
            search_delay: std::env::var("AMUI_DEMO_SEARCH_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(Duration::from_millis),
            shelf_mode: std::env::var("AMUI_DEMO_SHELVES").unwrap_or_default(),
            shelf_requests: AtomicUsize::new(0),
        };
        let mut playlists = Vec::new();
        if !demo.albums.is_empty() {
            let favorites: Vec<Track> = (0..500).map(|n| demo.track(n * 7 % LIBRARY_ALBUMS, n % 8)).collect();
            for track in &favorites {
                demo.favorites.lock().unwrap().extend(track.catalog_id.clone());
            }
            let mut favorite_songs = demo.playlist_named(0, "Favorite Songs", favorites.len());
            favorite_songs.is_favorites = true;
            favorite_songs.can_edit = false;
            favorite_songs.can_delete = false;
            playlists.push(StoredPlaylist { playlist: favorite_songs, tracks: favorites });
            for n in 1..10usize {
                let count = 12 + n * 9;
                let tracks: Vec<Track> =
                    (0..count).map(|k| demo.track((n * 31 + k * 13) % LIBRARY_ALBUMS, k % 7)).collect();
                let name = format!("{} {}", WORDS[n * 5 % WORDS.len()], ["Mix", "Evenings", "Drive", "Focus"][n % 4]);
                playlists.push(StoredPlaylist { playlist: demo.playlist_named(n, &name, count), tracks });
            }
            // one without songs and without artwork
            playlists.push(StoredPlaylist { playlist: demo.playlist_named(12, "Winter Notes", 0), tracks: Vec::new() });
        }
        *demo.playlists.lock().unwrap() = playlists;
        demo
    }

    fn wait(&self) {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
    }

    /// Take as long as a search: `AMUI_DEMO_SEARCH_MS`, or `times` the usual delay.
    fn wait_searching(&self, times: u32) {
        match self.search_delay {
            Some(delay) => std::thread::sleep(delay),
            None => (0..times).for_each(|_| self.wait()),
        }
    }

    fn artwork_of(&self, index: usize) -> Option<Artwork> {
        let source = self.albums.get(index % self.albums.len().max(1))?;
        Some(Artwork {
            url_template: format!("file://{}", self.assets.join(&source.cover).display()),
            width: Some(600),
            height: Some(600),
            bg_color: None,
        })
    }

    fn artist_id(&self, name: &str) -> String {
        let hash = name.bytes().fold(5381u32, |hash, byte| hash.wrapping_mul(33) ^ byte as u32);
        format!("ar{}", hash % 100_000)
    }

    fn in_library(index: usize) -> bool {
        index < LIBRARY_ALBUMS
    }

    fn album(&self, index: usize) -> Album {
        let source = &self.albums[index];
        Album {
            catalog_id: Some(format!("al{index}")),
            name: source.title.clone(),
            artist: source.artist.clone(),
            library_id: Self::in_library(index).then(|| format!("l.{index}")),
            artwork: self.artwork_of(index),
            track_count: Some(Self::track_count(index) as i64),
            release_date: Some(format!("{}-{:02}-{:02}", 1998 + index % 28, 1 + index % 12, 1 + index % 27)),
            date_added: Self::in_library(index)
                .then(|| format!("2026-{:02}-{:02}T10:00:00Z", 11 - (index / 16) % 11, 1 + index % 27)),
            genre: Some(["Alternative", "Electronic", "Jazz", "Pop", "Ambient"][index % 5].to_string()),
            is_single: Some(Self::track_count(index) <= 2),
            is_compilation: Some(false),
            artist_catalog_id: Some(self.artist_id(&source.artist)),
            audio_traits: Self::traits(index),
        }
    }

    fn traits(index: usize) -> Vec<String> {
        match index % 4 {
            0 => vec!["atmos".into(), "lossless".into(), "lossy-stereo".into()],
            1 => vec!["lossless".into(), "lossy-stereo".into()],
            2 => vec!["hi-res-lossless".into(), "lossless".into(), "lossy-stereo".into()],
            _ => vec!["lossy-stereo".into()],
        }
    }

    /// Every seventh album is a single.
    fn single(index: usize) -> bool {
        index % 7 == 6
    }

    /// Every twentieth album has two or three discs.
    fn discs(index: usize) -> usize {
        if index % 20 == 5 && !Self::single(index) { 2 + index / 20 % 2 } else { 1 }
    }

    fn track_count(index: usize) -> usize {
        if Self::single(index) {
            1 + index % 2
        } else if Self::discs(index) > 1 {
            7 * Self::discs(index)
        } else {
            8 + index * 5 % 7
        }
    }

    /// A few sentences (a description that does not fit in three lines).
    fn long_text(seed: usize) -> String {
        let word = |n: usize| WORDS[(seed * 7 + n * 5) % WORDS.len()].to_lowercase();
        (0..6)
            .map(|n| {
                format!(
                    "{} songs for the {} hours, from {} to {} and back again by way of the {}.",
                    WORDS[(seed + n * 3) % WORDS.len()],
                    word(n),
                    word(n + 1),
                    word(n + 2),
                    word(n + 3)
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn track(&self, album: usize, number: usize) -> Track {
        let album = album % self.albums.len().max(1);
        let source = &self.albums[album];
        let seed = album * 17 + number * 5;
        let name = match number % 3 {
            0 => format!("{} {}", WORDS[seed % WORDS.len()], WORDS[(seed / 3 + 7) % WORDS.len()]),
            1 => WORDS[(seed + 11) % WORDS.len()].to_string(),
            _ => format!("{} in the {}", WORDS[seed % WORDS.len()], WORDS[(seed + 5) % WORDS.len()]),
        };
        // on every fifth album some songs have a guest
        let guest = &self.albums[(album * 7 + number * 3 + 11) % self.albums.len()].artist;
        let artist = if album % 5 == 3 && number % 4 == 2 && *guest != source.artist {
            format!("{} & {}", source.artist, guest)
        } else {
            source.artist.clone()
        };
        let discs = Self::discs(album);
        let per_disc = (Self::track_count(album) / discs).max(1);
        Track {
            catalog_id: Some(format!("{}", 1_000_000 + album * 100 + number)),
            name,
            artist,
            album: source.title.clone(),
            library_id: Self::in_library(album).then(|| format!("i.{album}.{number}")),
            album_catalog_id: Some(format!("al{album}")),
            album_library_id: Self::in_library(album).then(|| format!("l.{album}")),
            artist_catalog_id: Some(self.artist_id(&source.artist)),
            duration_ms: Some((150 + seed * 13 % 190) as i64 * 1000),
            track_number: Some((number % per_disc) as i64 + 1),
            disc_number: Some((number / per_disc).min(discs - 1) as i64 + 1),
            artwork: self.artwork_of(album),
            // as `lyrics()` decides it for this song's catalog id
            has_lyrics: Some((album * 100 + number) % 5 != 4),
            audio_traits: Self::traits(album),
            content_rating: seed.is_multiple_of(9).then(|| "explicit".to_string()),
            genre: Some(["Alternative", "Electronic", "Jazz", "Pop", "Ambient"][album % 5].to_string()),
            release_date: Some(format!("{}-{:02}-{:02}", 1998 + album % 28, 1 + album % 12, 1 + album % 27)),
            isrc: None,
            date_added: Self::in_library(album)
                .then(|| format!("2026-{:02}-{:02}T10:00:00Z", 11 - (album / 16) % 11, 1 + album % 27)),
        }
    }

    fn album_tracks(&self, index: usize) -> Vec<Track> {
        (0..Self::track_count(index)).map(|number| self.track(index, number)).collect()
    }

    /// The songs of the library's albums. `AMUI_DEMO_SONGS=N` adds copies of them until there
    /// are at least N (for checks of long lists): each copy has a title, an album, ids and a
    /// moment it was added of its own.
    fn library_song_list(&self) -> &[Track] {
        self.songs.get_or_init(|| {
            let songs: Vec<Track> = (0..LIBRARY_ALBUMS.min(self.albums.len()))
                .flat_map(|index| self.album_tracks(index))
                .map(Self::as_listed)
                .collect();
            let wanted = std::env::var("AMUI_DEMO_SONGS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
            let mut all = songs.clone();
            for copy in 1.. {
                if all.len() >= wanted || songs.is_empty() {
                    break;
                }
                all.extend(songs.iter().enumerate().map(|(n, song)| Track {
                    catalog_id: Some(format!("{}", 5_000_000 + copy * 100_000 + n)),
                    library_id: song.library_id.as_ref().map(|id| format!("{id}.{copy}")),
                    name: format!("{} {}", WORDS[(copy * 7 + n) % WORDS.len()], song.name),
                    album: format!("{} {}", song.album, copy + 1),
                    date_added:
                        song.date_added.as_ref().map(|added| added.replace(":00:00Z", &format!(":{copy:02}:00Z"))),
                    ..song.clone()
                }));
            }
            // as Apple hands them out: the most recently added first
            all.sort_by(|a, b| b.date_added.cmp(&a.date_added));
            all
        })
    }

    /// A song as a list of the library has it: Apple's library songs name neither their album
    /// nor their artist (`library_song_links` does).
    fn as_listed(mut track: Track) -> Track {
        if track.library_id.is_some() {
            (track.album_catalog_id, track.album_library_id, track.artist_catalog_id) = (None, None, None);
        }
        track
    }

    fn playlist_named(&self, n: usize, name: &str, count: usize) -> Playlist {
        Playlist {
            library_id: Some(format!("p.{n}")),
            name: name.to_string(),
            catalog_id: None,
            description: match n {
                5 => Self::long_text(n),
                _ if n % 2 == 1 => format!("Songs for {}.", name.to_lowercase()),
                _ => String::new(),
            },
            artwork: if n.is_multiple_of(3) { None } else { self.artwork_of(300 + n * 7) },
            track_count: Some(count as i64),
            can_edit: true,
            can_delete: true,
            is_favorites: false,
            is_library: true,
            curator: None,
            date_added: Some(format!("2026-{:02}-{:02}T09:00:00Z", 1 + n % 11, 2 + n % 26)),
            last_modified: None,
        }
    }

    fn catalog_playlist(&self, n: usize) -> Playlist {
        Playlist {
            library_id: None,
            name: format!("{} {}", WORDS[(n * 3 + 1) % WORDS.len()], ["Essentials", "Radar", "Chill", "Hits"][n % 4]),
            catalog_id: Some(format!("pl.{n}")),
            description: if n.is_multiple_of(3) {
                Self::long_text(n)
            } else {
                "A playlist of the demonstration catalog.".to_string()
            },
            artwork: self.artwork_of(500 + n * 3),
            track_count: Some(25),
            can_edit: false,
            can_delete: false,
            is_favorites: false,
            is_library: false,
            curator: Some("Apple Music".to_string()),
            date_added: None,
            last_modified: Some("2026-11-01T00:00:00Z".to_string()),
        }
    }

    fn artists(&self, library_only: bool) -> Vec<Artist> {
        let mut seen = HashSet::new();
        let mut artists = Vec::new();
        let limit = if library_only { LIBRARY_ALBUMS.min(self.albums.len()) } else { self.albums.len() };
        for (index, source) in self.albums[..limit].iter().enumerate() {
            if seen.insert(source.artist.clone()) {
                artists.push(Artist {
                    catalog_id: Some(self.artist_id(&source.artist)),
                    name: source.artist.clone(),
                    library_id: library_only.then(|| format!("r.{index}")),
                    artwork: if index % 4 == 3 { None } else { self.artwork_of(700 + index) },
                    genre: Some(["Alternative", "Electronic", "Jazz", "Pop", "Ambient"][index % 5].to_string()),
                });
            }
        }
        artists.sort_by_key(|artist| artist.name.to_lowercase());
        artists
    }

    fn index_of(target: &Ref, catalog_prefix: &str, library_prefix: &str) -> Option<usize> {
        let from = |id: &Option<String>, prefix: &str| id.as_deref()?.strip_prefix(prefix)?.parse().ok();
        from(&target.catalog_id, catalog_prefix).or_else(|| from(&target.library_id, library_prefix))
    }

    fn station(&self, n: usize, live: bool) -> Station {
        Station {
            id: format!("ra.{n}"),
            name: if live { format!("Music {}", n + 1) } else { format!("{} Station", WORDS[(n * 7) % WORDS.len()]) },
            subtitle: if live { "Live".to_string() } else { "Apple Music".to_string() },
            artwork: self.artwork_of(800 + n * 5),
            live,
            kind: if live { "streaming" } else { "editorial" }.to_string(),
            format: if live { "stream" } else { "tracks" }.to_string(),
        }
    }

    fn missing() -> Problem {
        Problem::new("The demonstration has no such item")
    }

    /// The answer of Home, New or Radio. `AMUI_DEMO_SHELVES` shows their other states: `fail`
    /// (a problem every time), `fail-once` (a problem the first time, so that "Try Again"
    /// helps), `empty` (no shelves), `rotate` (another shelf comes first with every answer:
    /// what a refresh has to notice).
    fn shelves(&self, mut shelves: Vec<Shelf>) -> Res<Vec<Shelf>> {
        let asked = self.shelf_requests.fetch_add(1, Ordering::Relaxed);
        match self.shelf_mode.as_str() {
            "fail" => Err(Problem::new("The demonstration was told to fail.")),
            "fail-once" if asked == 0 => Err(Problem::new("The demonstration was told to fail once.")),
            "empty" => Ok(Vec::new()),
            "rotate" => {
                let first = asked % shelves.len().max(1);
                shelves.rotate_left(first);
                Ok(shelves)
            }
            _ => Ok(shelves),
        }
    }
}

impl Music for DemoMusic {
    fn library_albums(&self, offset: i64, limit: i64) -> Res<Page<Album>> {
        self.wait();
        let albums: Vec<Album> = (0..LIBRARY_ALBUMS.min(self.albums.len())).map(|index| self.album(index)).collect();
        Ok(page(&albums, offset, limit))
    }

    fn library_artists(&self, offset: i64, limit: i64) -> Res<Page<Artist>> {
        self.wait();
        Ok(page(&self.artists(true), offset, limit))
    }

    fn library_songs(&self, offset: i64, limit: i64) -> Res<Page<Track>> {
        self.wait();
        Ok(page(self.library_song_list(), offset, limit))
    }

    fn library_playlists(&self, offset: i64, limit: i64) -> Res<Page<Playlist>> {
        self.wait();
        let playlists: Vec<Playlist> =
            self.playlists.lock().unwrap().iter().map(|stored| stored.playlist.clone()).collect();
        Ok(page(&playlists, offset, limit))
    }

    fn library_recently_added(&self, offset: i64, limit: i64) -> Res<Page<Item>> {
        self.wait();
        let mut items: Vec<(String, Item)> = (0..LIBRARY_ALBUMS.min(self.albums.len()))
            .map(|index| self.album(index))
            .map(|album| (album.date_added.clone().unwrap_or_default(), Item::Album(album)))
            .collect();
        for stored in self.playlists.lock().unwrap().iter() {
            items.push((
                stored.playlist.date_added.clone().unwrap_or_default(),
                Item::Playlist(stored.playlist.clone()),
            ));
        }
        items.sort_by(|a, b| b.0.cmp(&a.0));
        let items: Vec<Item> = items.into_iter().map(|(_, item)| item).collect();
        Ok(page(&items, offset, limit))
    }

    fn album(&self, target: &Ref) -> Res<Collection> {
        self.wait();
        let index =
            Self::index_of(target, "al", "l.").filter(|index| *index < self.albums.len()).ok_or_else(Self::missing)?;
        Ok(Collection {
            item: Item::Album(self.album(index)),
            tracks: self.album_tracks(index),
            next_offset: None,
            total: None,
        })
    }

    fn playlist(&self, target: &Ref) -> Res<Collection> {
        self.wait();
        if let Some(library_id) = &target.library_id {
            let playlists = self.playlists.lock().unwrap();
            let stored = playlists
                .iter()
                .find(|stored| stored.playlist.library_id.as_ref() == Some(library_id))
                .ok_or_else(Self::missing)?;
            let first = page(&stored.tracks, 0, 100);
            return Ok(Collection {
                item: Item::Playlist(stored.playlist.clone()),
                tracks: first.items.into_iter().map(Self::as_listed).collect(),
                next_offset: first.next_offset,
                total: first.total,
            });
        }
        let n = Self::index_of(target, "pl.", "p.").ok_or_else(Self::missing)?;
        let tracks = (0..25).map(|k| self.track(n * 37 + k * 11, k % 6)).collect();
        Ok(Collection { item: Item::Playlist(self.catalog_playlist(n)), tracks, next_offset: None, total: None })
    }

    fn playlist_tracks(&self, library_id: &str, offset: i64, limit: i64) -> Res<Page<Track>> {
        // each page of a long playlist takes its time, as the real service's does
        std::thread::sleep(self.delay * 6);
        let playlists = self.playlists.lock().unwrap();
        let stored = playlists
            .iter()
            .find(|stored| stored.playlist.library_id.as_deref() == Some(library_id))
            .ok_or_else(Self::missing)?;
        let listed = page(&stored.tracks, offset, limit);
        Ok(Page::new(listed.items.into_iter().map(Self::as_listed).collect(), listed.next_offset, listed.total))
    }

    fn artist(&self, target: &Ref) -> Res<ArtistPage> {
        self.wait();
        let library = self.artists(true);
        let all = self.artists(false);
        let artist = library
            .iter()
            .chain(all.iter())
            .find(|artist| {
                (target.catalog_id.is_some() && artist.catalog_id == target.catalog_id)
                    || (target.library_id.is_some() && artist.library_id == target.library_id)
            })
            .cloned()
            .ok_or_else(Self::missing)?;
        let albums: Vec<Album> = self
            .albums
            .iter()
            .enumerate()
            .filter(|(_, source)| source.artist == artist.name)
            .map(|(index, _)| self.album(index))
            .collect();
        let top_songs = self
            .albums
            .iter()
            .enumerate()
            .filter(|(_, source)| source.artist == artist.name)
            .flat_map(|(index, _)| self.album_tracks(index))
            .take(10)
            .collect();
        Ok(ArtistPage { artist, top_songs, albums })
    }

    fn song(&self, catalog_id: &str) -> Res<Track> {
        self.wait();
        // the ids `track()` hands out: a million, plus a hundred per album, plus the track
        let number = catalog_id.parse::<usize>().ok().and_then(|id| id.checked_sub(1_000_000));
        let (album, track) = number.map(|number| (number / 100, number % 100)).ok_or_else(Self::missing)?;
        if album >= self.albums.len() || track >= Self::track_count(album) {
            return Err(Self::missing());
        }
        Ok(self.track(album, track))
    }

    fn library_song_links(&self, library_id: &str) -> Res<(Option<String>, Option<String>)> {
        self.wait();
        // the ids `track()` hands out: "i.ALBUM.NUMBER"
        let album = library_id.strip_prefix("i.").and_then(|rest| rest.split('.').next()?.parse::<usize>().ok());
        let album =
            album.filter(|album| Self::in_library(*album) && *album < self.albums.len()).ok_or_else(Self::missing)?;
        let artist = &self.albums[album].artist;
        let first =
            self.albums.iter().position(|source| &source.artist == artist).filter(|first| Self::in_library(*first));
        Ok((Some(format!("l.{album}")), first.map(|first| format!("r.{first}"))))
    }

    fn search_library(&self, term: &str, limit: i64) -> Res<SearchResults> {
        self.wait_searching(1);
        Ok(self.search(term, limit as usize, true))
    }

    fn search_catalog(&self, term: &str, limit: i64) -> Res<SearchResults> {
        self.wait_searching(2);
        Ok(self.search(term, limit as usize, false))
    }

    fn lyrics(&self, catalog_id: &str) -> Res<Option<Lyrics>> {
        self.wait();
        let seed: usize = catalog_id.parse().unwrap_or(0);
        if seed % 5 == 4 || self.lines.is_empty() {
            return Ok(None);
        }
        let timing = [Timing::Word, Timing::Line, Timing::None][seed % 3];
        let lines = (0..36)
            .map(|n| {
                let text = self.lines[(n + seed) % self.lines.len()].clone();
                let begin = 6.0 + n as f64 * 4.5;
                Line {
                    begin: (timing != Timing::None).then_some(begin),
                    end: (timing != Timing::None).then_some(begin + 4.0),
                    // every third line ends on a held word
                    words: if timing == Timing::Word { sung(&text, begin, n % 3 == 1) } else { Vec::new() },
                    background: n % 9 == 8,
                    agent: None,
                    text,
                }
            })
            .collect();
        Ok(Some(Lyrics { lines, timing, language: Some("en".into()), source: "demo".into() }))
    }

    fn home(&self) -> Res<Vec<Shelf>> {
        self.wait();
        let albums = |range: std::ops::Range<usize>| {
            range.map(|index| Item::Album(self.album(index % self.albums.len()))).collect::<Vec<_>>()
        };
        let playlists =
            |range: std::ops::Range<usize>| range.map(|n| Item::Playlist(self.catalog_playlist(n))).collect::<Vec<_>>();
        // every seventh artist: some of them have no picture
        let artists = self.artists(false).into_iter().skip(3).step_by(7).take(12).map(Item::Artist).collect();
        // what leads the page, each under a line that says why it is there (one has none, one a
        // long one)
        let picks = vec![
            (Item::Playlist(self.catalog_playlist(21)), "Made for You"),
            (Item::Album(self.album(331)), "New Release"),
            (Item::Station(self.station(16, false)), "Recently Played"),
            (Item::Album(self.album(347)), ""),
            (Item::Playlist(self.catalog_playlist(26)), "Updated Playlist"),
            (Item::Album(self.album(358)), "Because You Listen to Quiet Music Late in the Evening"),
            (Item::Album(self.album(364)), "New Release"),
            (Item::Playlist(self.catalog_playlist(33)), "Made for You"),
        ];
        let (picks, reasons): (Vec<Item>, Vec<&str>) = picks.into_iter().unzip();
        self.shelves(vec![
            Shelf {
                title: "Top Picks for You".into(),
                layout: ShelfLayout::Hero,
                items: picks,
                notes: reasons.into_iter().map(String::from).collect(),
                ..Shelf::default()
            },
            Shelf {
                title: "Recently Played".into(),
                layout: ShelfLayout::Cards,
                items: albums(3..15),
                ..Shelf::default()
            },
            Shelf {
                title: "Made for You".into(),
                layout: ShelfLayout::Cards,
                items: playlists(0..8),
                ..Shelf::default()
            },
            Shelf {
                title: "Stations for You".into(),
                layout: ShelfLayout::Cards,
                items: (0..8).map(|n| Item::Station(self.station(n, false))).collect(),
                ..Shelf::default()
            },
            Shelf { title: "Favorite Artists".into(), layout: ShelfLayout::Cards, items: artists, ..Shelf::default() },
            Shelf {
                title: "New Releases".into(),
                layout: ShelfLayout::Cards,
                items: albums(200..216),
                ..Shelf::default()
            },
            // the service sends a shelf without items now and then: a page leaves it out
            Shelf { title: "Coming Soon".into(), ..Shelf::default() },
            Shelf {
                title: "More to Explore".into(),
                layout: ShelfLayout::Cards,
                items: albums(400..416),
                ..Shelf::default()
            },
        ])
    }

    fn browse(&self) -> Res<Vec<Shelf>> {
        self.wait();
        let albums = |range: std::ops::Range<usize>| {
            range.map(|index| Item::Album(self.album(index % self.albums.len()))).collect::<Vec<_>>()
        };
        let badges = ["New Album", "New Single", "Updated Playlist", "", "Spatial Audio", "New Album"];
        self.shelves(vec![
            Shelf {
                // the row that opens the page has no title
                layout: ShelfLayout::Hero,
                items: albums(820..826),
                notes: badges.into_iter().map(String::from).collect(),
                ..Shelf::default()
            },
            Shelf {
                title: "New Music".into(),
                layout: ShelfLayout::Cards,
                items: albums(600..616),
                ..Shelf::default()
            },
            Shelf {
                title: "Top Songs".into(),
                layout: ShelfLayout::Songs,
                // four full columns and a short one; every sixth song is explicit
                items: (0..18).map(|n| Item::Track(self.track(250 + n * 9, n % 6))).collect(),
                ..Shelf::default()
            },
            Shelf {
                title: "Top Albums".into(),
                layout: ShelfLayout::Cards,
                items: albums(250..266),
                ..Shelf::default()
            },
            Shelf {
                // a heading longer than a window is wide
                title: "Spatial Audio: New Albums and Playlists Mixed in Dolby Atmos, Chosen by Our Editors and \
                        Updated Every Friday With the Releases of the Week"
                    .into(),
                layout: ShelfLayout::Cards,
                items: albums(700..712),
                ..Shelf::default()
            },
            Shelf {
                title: "Top Playlists".into(),
                layout: ShelfLayout::Cards,
                items: (8..20).map(|n| Item::Playlist(self.catalog_playlist(n))).collect(),
                ..Shelf::default()
            },
        ])
    }

    fn radio(&self) -> Res<Vec<Shelf>> {
        self.wait();
        let station = |(n, live): (usize, bool)| Item::Station(self.station(n, live));
        self.shelves(vec![
            Shelf {
                // the row that opens the page has no title, and its items no lines over them
                layout: ShelfLayout::Hero,
                items: [(0, true), (17, false), (1, true), (18, false), (2, true), (19, false)].map(station).to_vec(),
                ..Shelf::default()
            },
            Shelf {
                title: "Live Radio".into(),
                layout: ShelfLayout::Cards,
                items: (0..3).map(|n| Item::Station(self.station(n, true))).collect(),
                ..Shelf::default()
            },
            Shelf {
                // stations that are on air among others
                title: "Recently Played".into(),
                layout: ShelfLayout::Cards,
                items: [(1, true), (5, false), (9, false), (2, true), (12, false), (7, false), (0, true), (14, false)]
                    .map(station)
                    .to_vec(),
                ..Shelf::default()
            },
            Shelf {
                title: "Stations by Genre".into(),
                layout: ShelfLayout::Cards,
                items: (3..15).map(|n| Item::Station(self.station(n, false))).collect(),
                ..Shelf::default()
            },
        ])
    }

    fn favorites(&self, catalog_ids: &[String]) -> Res<HashMap<String, bool>> {
        self.wait();
        let favorites = self.favorites.lock().unwrap();
        Ok(catalog_ids.iter().map(|id| (id.clone(), favorites.contains(id))).collect())
    }

    fn set_favorite(&self, track: &Track, favorite: bool) -> Res<()> {
        self.wait();
        let Some(id) = track.catalog_id.clone() else {
            return Err(Problem::new("This song cannot be marked as a favorite"));
        };
        let mut favorites = self.favorites.lock().unwrap();
        if favorite {
            favorites.insert(id);
        } else {
            favorites.remove(&id);
        }
        Ok(())
    }

    fn add_to_library(&self, _item: &Item) -> Res<()> {
        self.wait();
        Ok(())
    }

    fn create_playlist(&self, name: &str, description: &str, tracks: &[Track]) -> Res<Playlist> {
        self.wait();
        let mut playlists = self.playlists.lock().unwrap();
        let mut playlist = self.playlist_named(1000 + playlists.len(), name, tracks.len());
        playlist.description = description.to_string();
        playlist.artwork = tracks.first().and_then(|track| track.artwork.clone());
        playlists.push(StoredPlaylist { playlist: playlist.clone(), tracks: tracks.to_vec() });
        Ok(playlist)
    }

    fn add_to_playlist(&self, library_id: &str, tracks: &[Track]) -> Res<()> {
        self.wait();
        let mut playlists = self.playlists.lock().unwrap();
        let stored = playlists
            .iter_mut()
            .find(|stored| stored.playlist.library_id.as_deref() == Some(library_id))
            .ok_or_else(Self::missing)?;
        stored.tracks.extend(tracks.iter().cloned());
        stored.playlist.track_count = Some(stored.tracks.len() as i64);
        Ok(())
    }

    fn rename_playlist(&self, library_id: &str, name: &str, description: &str) -> Res<()> {
        self.wait();
        let mut playlists = self.playlists.lock().unwrap();
        let stored = playlists
            .iter_mut()
            .find(|stored| stored.playlist.library_id.as_deref() == Some(library_id))
            .ok_or_else(Self::missing)?;
        stored.playlist.name = name.to_string();
        stored.playlist.description = description.to_string();
        Ok(())
    }

    fn delete_playlist(&self, library_id: &str) -> Res<()> {
        self.wait();
        self.playlists.lock().unwrap().retain(|stored| stored.playlist.library_id.as_deref() != Some(library_id));
        Ok(())
    }

    fn remove_from_playlist(&self, library_id: &str, tracks: &[Track]) -> Res<()> {
        self.wait();
        let mut playlists = self.playlists.lock().unwrap();
        let stored = playlists
            .iter_mut()
            .find(|stored| stored.playlist.library_id.as_deref() == Some(library_id))
            .ok_or_else(Self::missing)?;
        let gone: HashSet<String> = tracks.iter().map(Track::key).collect();
        stored.tracks.retain(|track| !gone.contains(&track.key()));
        stored.playlist.track_count = Some(stored.tracks.len() as i64);
        Ok(())
    }

    fn refresh(&self) {}

    fn artwork(&self, url: &str) -> Res<Vec<u8>> {
        let path = url.strip_prefix("file://").ok_or_else(|| Problem::new("The demonstration has no network"))?;
        std::fs::read(path).map_err(|_| Problem::new("Artwork file is missing"))
    }

    fn catalog_region(&self) -> Res<crate::data::Region> {
        self.wait();
        Ok(crate::data::Region { account: "us".to_string(), chosen: DEMO_REGION.lock().unwrap().clone() })
    }

    fn set_catalog_region(&self, code: Option<&str>) -> Res<()> {
        *DEMO_REGION.lock().unwrap() = code.map(str::to_owned);
        Ok(())
    }
}

/// The catalog region chosen in the demonstration.
static DEMO_REGION: Mutex<Option<String>> = Mutex::new(None);

impl DemoMusic {
    fn search(&self, term: &str, limit: usize, library: bool) -> SearchResults {
        let needle = term.trim().to_lowercase();
        if needle.is_empty() {
            return SearchResults::default();
        }
        let range = if library { 0..LIBRARY_ALBUMS.min(self.albums.len()) } else { 0..self.albums.len() };
        let hit = |text: &str| text.to_lowercase().contains(&needle);
        let albums: Vec<Album> = range
            .clone()
            .filter(|index| hit(&self.albums[*index].title) || hit(&self.albums[*index].artist))
            .take(limit)
            .map(|index| self.album(index))
            .collect();
        let songs: Vec<Track> = range
            .clone()
            .flat_map(|index| self.album_tracks(index))
            .filter(|track| hit(&track.name) || hit(&track.artist))
            .take(limit)
            .collect();
        let artists: Vec<Artist> =
            self.artists(library).into_iter().filter(|artist| hit(&artist.name)).take(limit).collect();
        let playlists: Vec<Playlist> = if library {
            self.playlists
                .lock()
                .unwrap()
                .iter()
                .map(|stored| stored.playlist.clone())
                .filter(|playlist| hit(&playlist.name))
                .take(limit)
                .collect()
        } else {
            (0..40).map(|n| self.catalog_playlist(n)).filter(|playlist| hit(&playlist.name)).take(limit).collect()
        };
        let top = if library {
            None
        } else {
            artists
                .first()
                .cloned()
                .map(amui::types::Item::Artist)
                .or_else(|| albums.first().cloned().map(amui::types::Item::Album))
                .or_else(|| songs.first().cloned().map(amui::types::Item::Track))
        };
        SearchResults { songs, albums, artists, playlists, top }
    }
}
