//! What the library holds for a search term, found in its lists in memory as the term is typed,
//! without asking Apple.
//!
//! The texts of every row of the library's songs, albums, artists and playlists (its title, its
//! artist, its album) are kept in the form a search compares (`sorting::write_search_text`: no
//! capitals, no accents), made once when the row enters its list: a key press only compares.
//! They are made a few thousand at a time while the window is idle (a list of tens of thousands
//! of songs arrives at once from the copy of the last run), and what is still missing when a
//! search is asked is made then. A term finds a row when each of its words is in one of those
//! texts, in any order ("amber glass" finds the song "Amber Harbor" by Glass Orbit).
//!
//! What a term finds is ranked by how it matches, best first (`Tier`): the title is the term; the
//! title begins with it; its words make up the whole title, in any order, and the others begin
//! words of the artist ("amber glass" for "Amber" by Glass Orbit); each of its words begins a word
//! of the title; the artist begins with the term (what an artist sings); the album begins with it;
//! each word begins a word of the title, the artist or the album; some are found inside words. Of
//! rows that match alike, the one with fewer words of the term outside its title comes first,
//! then the one with fewer found in its album only (a song and who sings it are looked for more
//! often than a song and its album), then the one added last (the library's songs are kept newest
//! first), then the list's own order.
//!
//! A term that goes on from the previous one (a letter typed at its end) can only find rows the
//! previous one found: only those are compared again.

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::rc::Rc;

use amui::types::{SearchResults, Track};
use gtk::glib;
use gtk::prelude::*;

use crate::data::Item;
use crate::library::{Kind, Library, item_of, track_of};
use crate::sorting::{date_key, search_text, write_search_text};

/// The lists a search looks in, in the order its results show them.
const LISTS: [Kind; 4] = [Kind::Songs, Kind::Albums, Kind::Artists, Kind::Playlists];

fn slot(kind: Kind) -> Option<usize> {
    LISTS.iter().position(|list| *list == kind)
}

/// Between the texts of a row: no word of a term holds it, so none is found across two texts.
const BETWEEN: char = '\u{1}';
/// The rows whose texts are made at once while the window is idle (about a millisecond).
const CHUNK: usize = 2000;

/// The texts of a row as a search compares them, one after the other with `BETWEEN` between
/// them: the title, the artist, the album.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    text: String,
    /// Where the title ends in `text`, and where the artist ends.
    title_end: usize,
    artist_end: usize,
    /// When the row was added to the library (`sorting::date_key`), when it says so.
    added: Option<u64>,
}

impl Entry {
    pub fn new(title: &str, artist: &str, album: &str, added: Option<&str>) -> Entry {
        let mut text = String::with_capacity(title.len() + artist.len() + album.len() + 2);
        write_search_text(&mut text, title);
        let title_end = text.len();
        text.push(BETWEEN);
        write_search_text(&mut text, artist);
        let artist_end = text.len();
        text.push(BETWEEN);
        write_search_text(&mut text, album);
        Entry { text, title_end, artist_end, added: added.and_then(date_key) }
    }

    pub fn of_track(track: &Track) -> Entry {
        Entry::new(&track.name, &track.artist, &track.album, track.date_added.as_deref())
    }

    pub fn of_item(item: &Item) -> Entry {
        match item {
            Item::Track(track) => Entry::of_track(track),
            Item::Album(album) => Entry::new(&album.name, &album.artist, "", album.date_added.as_deref()),
            Item::Playlist(playlist) => Entry::new(&playlist.name, "", "", playlist.date_added.as_deref()),
            Item::Artist(artist) => Entry::new(&artist.name, "", "", None),
            Item::Station(station) => Entry::new(&station.name, "", "", None),
        }
    }

    fn title(&self) -> &str {
        &self.text[..self.title_end]
    }

    fn artist(&self) -> &str {
        &self.text[self.title_end + BETWEEN.len_utf8()..self.artist_end]
    }

    fn album(&self) -> &str {
        &self.text[self.artist_end + BETWEEN.len_utf8()..]
    }

    /// The title and the artist (without the album).
    fn title_and_artist(&self) -> &str {
        &self.text[..self.artist_end]
    }
}

/// A term as it is compared: its words in the form of the rows' texts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// The words, one space between them.
    text: String,
    words: Vec<String>,
}

impl Query {
    pub fn new(term: &str) -> Query {
        let text = search_text(term);
        let words = text.split(' ').filter(|word| !word.is_empty()).map(str::to_owned).collect();
        Query { text, words }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Every row this query finds is found by `earlier` too: this is `earlier` typed on (each
    /// word of `earlier` begins a word of this one).
    fn narrows(&self, earlier: &Query) -> bool {
        !earlier.is_empty() && self.text.starts_with(&earlier.text)
    }
}

/// How a row matches a term, from the best to the least good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// The title is the term.
    Title,
    /// The title begins with the term.
    TitleStart,
    /// The words of the term make up the whole title, in any order; the others begin words of
    /// the artist.
    WholeTitle,
    /// Each word of the term begins a word of the title.
    TitleWords,
    /// The artist begins with the term: what an artist sings, the albums of an artist.
    ArtistStart,
    /// The album begins with the term: the songs of an album.
    AlbumStart,
    /// Each word of the term begins a word of the title, the artist or the album.
    Words,
    /// The words of the term are there, some inside words.
    Inside,
}

/// How a row matches a term: the smaller, the better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rank {
    pub tier: Tier,
    /// Words of the term that are not in the title...
    pub elsewhere: usize,
    /// ...and of those, the words that are in the album only.
    pub album_only: usize,
}

/// Whether `word` begins a word of `text`: it is found at its start, or after a space or the
/// end of another text.
fn begins_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| at == 0 || matches!(text.as_bytes()[at - 1], b' ' | b'\x01'))
}

/// Whether each word of `title` is begun by one of `words`.
fn makes_up(title: &str, words: &[String]) -> bool {
    !title.is_empty() && title.split(' ').all(|part| words.iter().any(|word| part.starts_with(word.as_str())))
}

/// How `entry` matches `query`; `None` when a word of the query is not in its texts.
fn rank(entry: &Entry, query: &Query) -> Option<Rank> {
    let text = entry.text.as_str();
    if !query.words.iter().all(|word| text.contains(word.as_str())) {
        return None;
    }
    let (title, title_and_artist) = (entry.title(), entry.title_and_artist());
    let elsewhere = query.words.iter().filter(|word| !title.contains(word.as_str())).count();
    let album_only = query.words.iter().filter(|word| !title_and_artist.contains(word.as_str())).count();
    let words = &query.words;
    let tier = if title == query.text {
        Tier::Title
    } else if title.starts_with(&query.text) {
        Tier::TitleStart
    } else if makes_up(title, words) && words.iter().all(|word| begins_word(title_and_artist, word)) {
        Tier::WholeTitle
    } else if words.iter().all(|word| begins_word(title, word)) {
        Tier::TitleWords
    } else if entry.artist().starts_with(&query.text) {
        Tier::ArtistStart
    } else if entry.album().starts_with(&query.text) {
        Tier::AlbumStart
    } else if words.iter().all(|word| begins_word(text, word)) {
        Tier::Words
    } else {
        Tier::Inside
    };
    Some(Rank { tier, elsewhere, album_only })
}

/// A row that a term found: its place in its list, and how it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub position: u32,
    pub rank: Rank,
}

/// The library's lists (`LISTS`) as a search compares them, row for row as their stores have
/// them.
#[derive(Debug, Default)]
pub struct Index {
    /// The rows of each list; `None` for a row whose texts are still to be made (`make`).
    lists: [Vec<Option<Entry>>; 4],
    /// Every row of a list before this position has its texts.
    ready: [usize; 4],
    /// The last query, and every row it found in each list (in the lists' order), while no list
    /// has changed since.
    last: Option<(Query, [Vec<u32>; 4])>,
    /// Counts the changes of the lists.
    generation: u64,
}

impl Index {
    /// The rows `at..at + removed` of the list of `kind` were replaced by `added` rows, whose
    /// texts are still to be made.
    pub fn splice(&mut self, kind: Kind, at: usize, removed: usize, added: usize) {
        let Some(slot) = slot(kind).filter(|_| removed > 0 || added > 0) else { return };
        let list = &mut self.lists[slot];
        let at = at.min(list.len());
        let end = at.saturating_add(removed).min(list.len());
        list.splice(at..end, std::iter::repeat_n(None, added));
        self.ready[slot] = self.ready[slot].min(at);
        self.last = None;
        self.generation += 1;
    }

    /// The next row whose texts are still to be made.
    pub fn next_unmade(&mut self) -> Option<(Kind, usize)> {
        for (slot, list) in self.lists.iter().enumerate() {
            let ready = &mut self.ready[slot];
            while list.get(*ready).is_some_and(Option::is_some) {
                *ready += 1;
            }
            if *ready < list.len() {
                return Some((LISTS[slot], *ready));
            }
        }
        None
    }

    /// The texts of a row, made.
    pub fn make(&mut self, kind: Kind, position: usize, entry: Entry) {
        if let Some(row) = slot(kind).and_then(|slot| self.lists[slot].get_mut(position)) {
            *row = Some(entry);
            self.last = None;
        }
    }

    /// A number that changes whenever a list changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The rows of each list (in the order of `LISTS`) that `query` finds, the best first, at
    /// most `limit` of each. A row whose texts are not made yet is not found.
    pub fn find(&mut self, query: &Query, limit: usize) -> [Vec<Hit>; 4] {
        let mut best: [Vec<Hit>; 4] = Default::default();
        if query.is_empty() {
            self.last = None;
            return best;
        }
        // a term typed on: only what the previous one found can be found
        let earlier = self.last.take().filter(|(last, _)| query.narrows(last)).map(|(_, found)| found);
        let mut found: [Vec<u32>; 4] = Default::default();
        for (slot, entries) in self.lists.iter().enumerate() {
            let mut hits = Vec::new();
            let mut compare = |position: u32| {
                let entry = entries.get(position as usize).and_then(Option::as_ref);
                if let Some(rank) = entry.and_then(|entry| rank(entry, query)) {
                    found[slot].push(position);
                    hits.push(Hit { position, rank });
                }
            };
            match &earlier {
                Some(earlier) => earlier[slot].iter().copied().for_each(&mut compare),
                None => (0..entries.len() as u32).for_each(&mut compare),
            }
            best[slot] = first(hits, limit, entries);
        }
        self.last = Some((query.clone(), found));
        best
    }
}

/// The best `limit` of `hits`, in order: by how they match, then the row added last, then the
/// list's order.
fn first(mut hits: Vec<Hit>, limit: usize, entries: &[Option<Entry>]) -> Vec<Hit> {
    let added = |hit: &Hit| entries[hit.position as usize].as_ref().and_then(|entry| entry.added);
    let key = |hit: &Hit| (hit.rank, Reverse(added(hit)), hit.position);
    if limit > 0 && hits.len() > limit {
        hits.select_nth_unstable_by_key(limit - 1, key);
    }
    hits.truncate(limit);
    hits.sort_unstable_by_key(key);
    hits
}

/// What the library holds for a term.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    /// Songs, albums, artists and playlists, each the best first.
    pub results: Rc<SearchResults>,
    /// The albums, artists and playlists of `results` together, the best first; of those that
    /// match alike, albums come first, then artists, then playlists.
    pub items: Vec<Item>,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.results.songs.is_empty() && self.items.is_empty()
    }
}

/// The library's lists made ready for searches, and followed as they change.
pub struct Finder {
    library: Rc<Library>,
    index: RefCell<Index>,
    /// An idle callback is making the texts of new rows.
    scheduled: Cell<bool>,
}

impl Finder {
    pub fn new(library: &Rc<Library>) -> Rc<Finder> {
        let finder =
            Rc::new(Finder { library: library.clone(), index: RefCell::default(), scheduled: Cell::new(false) });
        for kind in LISTS {
            finder.take(kind, 0, 0, library.store(kind).n_items());
            let weak = Rc::downgrade(&finder);
            library.store(kind).connect_items_changed(move |_, at, removed, added| {
                if let Some(finder) = weak.upgrade() {
                    finder.take(kind, at, removed, added);
                }
            });
        }
        finder
    }

    /// The rows `at..at + removed` of a store were replaced by `added` rows.
    fn take(self: &Rc<Self>, kind: Kind, at: u32, removed: u32, added: u32) {
        self.index.borrow_mut().splice(kind, at as usize, removed as usize, added as usize);
        if added > 0 {
            self.schedule();
        }
    }

    /// Make the texts of new rows, `most` at most; true when some are still to be made.
    fn make(&self, most: usize) -> bool {
        let mut index = self.index.borrow_mut();
        for _ in 0..most {
            let Some((kind, position)) = index.next_unmade() else { return false };
            let row = self.library.store(kind).item(position as u32);
            index.make(kind, position, row.map(|row| entry_of(&row)).unwrap_or_default());
        }
        index.next_unmade().is_some()
    }

    /// Make the texts of new rows a chunk at a time while the window has nothing else to do.
    fn schedule(self: &Rc<Self>) {
        // tests run no main loop: a search makes what is missing when it is asked
        if cfg!(test) || self.scheduled.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
            let Some(finder) = weak.upgrade() else { return glib::ControlFlow::Break };
            if finder.make(CHUNK) {
                return glib::ControlFlow::Continue;
            }
            finder.scheduled.set(false);
            glib::ControlFlow::Break
        });
    }

    /// A number that changes whenever a list of the library changes.
    pub fn generation(&self) -> u64 {
        self.index.borrow().generation()
    }

    /// Whether the lists hold the whole library: each was fetched in full, and none is being
    /// fetched again (a list that is loaded anew holds only its first pages for a while).
    pub fn complete(&self) -> bool {
        LISTS.iter().all(|kind| self.library.is_loaded(*kind) && !self.library.is_loading(*kind))
    }

    /// What the library holds for `term`: at most `limit` songs, albums, artists and playlists.
    pub fn find(&self, term: &str, limit: usize) -> Found {
        // the texts that are still to be made, now
        self.make(usize::MAX);
        let hits = self.index.borrow_mut().find(&Query::new(term), limit);
        let rows = |kind: Kind| {
            let store = self.library.store(kind);
            hits[slot(kind).unwrap_or_default()]
                .iter()
                .filter_map(move |hit| Some((hit.rank, store.item(hit.position)?)))
        };
        let songs = rows(Kind::Songs).filter_map(|(_, row)| track_of(&row)).collect();
        // albums, artists and playlists together, the best first
        let mut items: Vec<(Rank, usize, Item)> = [Kind::Albums, Kind::Artists, Kind::Playlists]
            .into_iter()
            .enumerate()
            .flat_map(|(order, kind)| rows(kind).filter_map(move |(rank, row)| Some((rank, order, item_of(&row)?))))
            .collect();
        // a stable sort: each kind keeps its own order among rows that match alike
        items.sort_by_key(|(rank, order, _)| (*rank, *order));
        let mut results = SearchResults { songs, ..SearchResults::default() };
        for (_, _, item) in &items {
            match item {
                Item::Album(album) => results.albums.push(album.clone()),
                Item::Artist(artist) => results.artists.push(artist.clone()),
                Item::Playlist(playlist) => results.playlists.push(playlist.clone()),
                Item::Track(_) | Item::Station(_) => {}
            }
        }
        Found { results: Rc::new(results), items: items.into_iter().map(|(_, _, item)| item).collect() }
    }
}

/// The texts of a row of a store (a song, or an album, artist or playlist).
fn entry_of(row: &glib::Object) -> Entry {
    let Some(boxed) = row.downcast_ref::<glib::BoxedAnyObject>() else { return Entry::default() };
    if let Ok(track) = boxed.try_borrow::<Track>() {
        return Entry::of_track(&track);
    }
    boxed.try_borrow::<Item>().map(|item| Entry::of_item(&item)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use amui::types::{Album, Artist, Playlist};

    use super::*;
    use crate::library::boxed;

    fn song(title: &str, artist: &str, album: &str) -> Entry {
        Entry::new(title, artist, album, None)
    }

    /// Rows `at..at + removed` of a list replaced by `entries`, made at once.
    fn put(index: &mut Index, kind: Kind, at: usize, removed: usize, entries: Vec<Entry>) {
        index.splice(kind, at, removed, entries.len());
        for (offset, entry) in entries.into_iter().enumerate() {
            index.make(kind, at + offset, entry);
        }
    }

    /// An index whose list of songs is `songs`, in that order.
    fn songs_index(songs: Vec<Entry>) -> Index {
        let mut index = Index::default();
        put(&mut index, Kind::Songs, 0, 0, songs);
        index
    }

    /// The positions of the songs `term` finds, the best first.
    fn found(index: &mut Index, term: &str) -> Vec<u32> {
        let hits = index.find(&Query::new(term), 50);
        hits[0].iter().map(|hit| hit.position).collect()
    }

    fn tier(entry: &Entry, term: &str) -> Option<Tier> {
        rank(entry, &Query::new(term)).map(|rank| rank.tier)
    }

    #[test]
    fn a_term_finds_its_words_in_the_title_the_artist_or_the_album_in_any_order() {
        let entry = song("Amber Harbor", "Glass Orbit", "Tide Lanterns");
        for term in ["amber", "harbor", "harbor amber", "glass", "amber glass", "glass amber", "tide amber orbit"] {
            assert!(rank(&entry, &Query::new(term)).is_some(), "{term}");
        }
        // the start of a word, or a part of one
        for term in ["amb", "harb gla", "mber", "rbit"] {
            assert!(rank(&entry, &Query::new(term)).is_some(), "{term}");
        }
        // every word has to be there, and a word is not found across two texts or two words
        for term in ["amber velvet", "velvet", "amberharbor", "harborglass", "orbittide"] {
            assert_eq!(rank(&entry, &Query::new(term)), None, "{term}");
        }
        // nothing is found by an empty term, and nothing in an empty row
        assert!(Query::new("  ").is_empty());
        assert_eq!(rank(&Entry::default(), &Query::new("amber")), None);
    }

    #[test]
    fn capitals_accents_and_signs_make_no_difference() {
        let entry = song("Élan, Straße & Co.", "Don’t Drift", "Ｗｉｄｅ Ｍｅａｄｏｗ");
        for term in ["elan", "ÉLAN", "Élan", "strasse", "straße", "elan strasse co", "dont", "don't", "wide meadow"]
        {
            assert!(rank(&entry, &Query::new(term)).is_some(), "{term}");
        }
        // a sign between words separates them, as a space does
        assert_eq!(tier(&song("Neon/Static", "", ""), "neon static"), Some(Tier::Title));
        assert_eq!(tier(&song("Neon Static", "", ""), "neon-static"), Some(Tier::Title));
        // numbers are found as they are written; a leading article is part of the title
        assert_eq!(tier(&song("10 Lanterns", "", ""), "10"), Some(Tier::TitleStart));
        assert_eq!(tier(&song("Track 007", "", ""), "007"), Some(Tier::TitleWords));
        assert_eq!(tier(&song("The Paper Lanterns", "", ""), "the paper"), Some(Tier::TitleStart));
        // a title of signs only can be found
        assert_eq!(tier(&song("?!", "", ""), "?!"), Some(Tier::Title));
    }

    #[test]
    fn how_a_row_matches() {
        let amber = song("Amber", "Glass Orbit", "Tide");
        assert_eq!(tier(&amber, "amber"), Some(Tier::Title));
        assert_eq!(tier(&amber, "amb"), Some(Tier::TitleStart));
        assert_eq!(tier(&song("Amber Harbor", "", ""), "amber ha"), Some(Tier::TitleStart));
        // the title in another order, or the title and who sings it (not the title and its album)
        assert_eq!(tier(&song("Amber Harbor", "", ""), "harbor amber"), Some(Tier::WholeTitle));
        assert_eq!(tier(&amber, "amber glass"), Some(Tier::WholeTitle));
        assert_eq!(tier(&amber, "glass amber orb"), Some(Tier::WholeTitle));
        assert_eq!(tier(&amber, "amber tide"), Some(Tier::Words));
        // words that begin words of the title
        assert_eq!(tier(&song("Orbit Amber Harbor", "", ""), "amber"), Some(Tier::TitleWords));
        assert_eq!(tier(&song("Orbit Amber Harbor", "", ""), "harb amb"), Some(Tier::TitleWords));
        // what an artist sings, the songs of an album
        assert_eq!(tier(&song("Lantern", "Amber Glass", ""), "amber"), Some(Tier::ArtistStart));
        assert_eq!(tier(&song("Neon Winter", "Glass Orbit", "Tide"), "glass orb"), Some(Tier::ArtistStart));
        assert_eq!(tier(&song("Harbor", "Nova Vale", "Amber Days"), "amber d"), Some(Tier::AlbumStart));
        // words that begin words of any text
        assert_eq!(tier(&song("Amber Harbor", "Glass Orbit", ""), "amber glass"), Some(Tier::Words));
        assert_eq!(tier(&song("Orbit", "Mira Reyes", "Velvet Glass"), "glass orbit"), Some(Tier::Words));
        assert_eq!(tier(&song("Neon Winter", "Glass Orbit", "Tide"), "orbit glass"), Some(Tier::Words));
        // a word found inside a word
        assert_eq!(tier(&song("Lambert", "", ""), "amber"), Some(Tier::Inside));
        assert_eq!(tier(&song("Amber", "Glass Orbit", ""), "amber rbit"), Some(Tier::Inside));
        // how many words are not in the title
        let elsewhere = |entry: &Entry, term: &str| rank(entry, &Query::new(term)).map(|rank| rank.elsewhere);
        assert_eq!(elsewhere(&amber, "amber glass orbit"), Some(2));
        assert_eq!(elsewhere(&song("Lantern", "Amber Glass", ""), "amber glass"), Some(2));
        assert_eq!(elsewhere(&amber, "amb"), Some(0));
    }

    #[test]
    fn the_best_matches_come_first() {
        let mut index = songs_index(vec![
            song("Lambert", "Nova Vale", "Winter"),
            song("Lantern", "Amber Glass", "Winter"),
            song("Orbit Amber", "Nova Vale", "Winter"),
            song("Amber in the Tide", "Nova Vale", "Winter"),
            song("Amber", "Nova Vale", "Winter"),
            song("Velvet", "Nova Vale", "Winter"),
            song("Harbor", "Nova Vale", "Amber Days"),
        ]);
        // the title, its beginning, a word of it, the artist's beginning, the album's, a part of
        // a word
        assert_eq!(found(&mut index, "amber"), [4, 3, 2, 1, 6, 0]);
        // the title, the title and who sings it, the artist, then words here and there
        let mut index = songs_index(vec![
            song("Lantern", "Amber Glass", ""),
            song("Amber Harbor", "Glass Orbit", ""),
            song("Amber", "Glass Orbit", ""),
            song("Glass Amber", "Nova Vale", ""),
        ]);
        assert_eq!(found(&mut index, "amber glass"), [3, 2, 0, 1]);
        // an artist's name: the artist's songs before a title with an album that has the rest
        let mut index = songs_index(vec![
            song("Orbit", "Mira Reyes", "Velvet Glass"),
            song("Neon Winter", "Glass Orbit", "Tide"),
            song("Orbit in the Quiet", "Glass Orbit", "Tide"),
        ]);
        assert_eq!(found(&mut index, "glass orbit"), [2, 1, 0]);
    }

    #[test]
    fn a_word_of_the_artist_counts_for_more_than_one_of_the_album() {
        let in_album = song("Paper in the Glass", "Nova Vale", "Amber Horizon");
        let by_artist = song("Amber in the Lantern", "Glass Orbit", "Tide");
        let ranked =
            |entry: &Entry| rank(entry, &Query::new("amber glass")).map(|rank| (rank.elsewhere, rank.album_only));
        assert_eq!((ranked(&in_album), ranked(&by_artist)), (Some((1, 1)), Some((1, 0))));
        // the song with the word in its album was added last, and still comes second
        let mut index = songs_index(vec![in_album, by_artist]);
        assert_eq!(found(&mut index, "amber glass"), [1, 0]);
        assert_eq!(found(&mut index, "glass amber"), [1, 0]);
    }

    #[test]
    fn of_songs_that_match_alike_the_one_added_last_comes_first() {
        // the library's songs are kept the most recently added first: the list's order decides
        let mut index = songs_index(vec![
            song("Amber Harbor", "Nova Vale", ""),
            song("Amber", "Nova Vale", ""),
            song("Amber Tide", "Glass Orbit", ""),
            song("Amber", "Glass Orbit", ""),
        ]);
        assert_eq!(found(&mut index, "amber"), [1, 3, 0, 2]);
        // a row that says when it was added goes by that, before rows that do not say
        let mut index = songs_index(vec![
            Entry::new("Amber", "Nova Vale", "", None),
            Entry::new("Amber", "Glass Orbit", "", Some("2025-01-10T10:00:00Z")),
            Entry::new("Amber", "Ada Quill", "", Some("2026-03-01T10:00:00Z")),
        ]);
        assert_eq!(found(&mut index, "amber"), [2, 1, 0]);
    }

    #[test]
    fn at_most_the_limit_of_each_list() {
        let mut index = songs_index((0..30).map(|n| song(&format!("Amber {n}"), "Nova Vale", "")).collect());
        let albums = (0..5).map(|n| Entry::new(&format!("Amber Days {n}"), "Nova Vale", "", None)).collect();
        put(&mut index, Kind::Albums, 0, 0, albums);
        let hits = index.find(&Query::new("amber"), 8);
        assert_eq!(hits[0].iter().map(|hit| hit.position).collect::<Vec<_>>(), (0..8).collect::<Vec<_>>());
        assert_eq!(hits[1].len(), 5);
        assert!(hits[2].is_empty() && hits[3].is_empty());
        assert!(index.find(&Query::new("amber"), 0).iter().all(Vec::is_empty));
        assert!(index.find(&Query::new(""), 8).iter().all(Vec::is_empty));
    }

    /// Songs with titles, artists and albums made of a few words, in many combinations.
    fn many(count: usize) -> Vec<Entry> {
        const WORDS: [&str; 16] = [
            "Amber", "Harbor", "Velvet", "Orbit", "Cascade", "Lantern", "Paper", "Thunder", "Quiet", "Signal",
            "Meadow", "Glass", "Winter", "Honey", "Élan", "Straße",
        ];
        let word = |n: usize| WORDS[n % WORDS.len()];
        (0..count)
            .map(|n| {
                let title = format!("{} {} {}", word(n), word(n / 16 + 3), word(n / 256 + 7));
                let artist = format!("{} {}", word(n / 3 + 1), word(n / 5 + 2));
                let album = format!("{} {}", word(n / 7 + 5), word(n / 11 + 9));
                song(&title, &artist, &album)
            })
            .collect()
    }

    #[test]
    fn a_term_typed_on_finds_what_it_would_find_anew() {
        let mut typed = songs_index(many(5000));
        let terms = ["a", "am", "amb", "amber", "amber ", "amber h", "amber ha", "amber har", "amber", "am", "q", "qu"];
        for term in terms {
            let mut fresh = songs_index(many(5000));
            let query = Query::new(term);
            assert_eq!(typed.find(&query, 20), fresh.find(&query, 20), "{term}");
        }
    }

    #[test]
    fn a_change_to_a_list_is_followed() {
        let mut index = songs_index(vec![song("Amber", "", ""), song("Velvet", "", ""), song("Amber Tide", "", "")]);
        assert_eq!(found(&mut index, "amber"), [0, 2]);
        let generation = index.generation();
        // a row before the others goes, two come in its place
        put(&mut index, Kind::Songs, 0, 1, vec![song("Harbor", "", ""), song("Amber Glass", "", "")]);
        assert_ne!(index.generation(), generation);
        assert_eq!(found(&mut index, "amber"), [1, 3]);
        assert_eq!(found(&mut index, "amber t"), [3]);
        // the list is replaced, then grows at its end
        put(&mut index, Kind::Songs, 0, 4, vec![song("Velvet", "", "")]);
        assert_eq!(found(&mut index, "amber t"), Vec::<u32>::new());
        put(&mut index, Kind::Songs, 1, 0, vec![song("Amber Tide", "", "")]);
        assert_eq!(found(&mut index, "amber t"), [1]);
        // a list that is not searched changes nothing
        let generation = index.generation();
        put(&mut index, Kind::Recent, 0, 0, vec![song("Amber", "", "")]);
        assert_eq!(index.generation(), generation);
    }

    #[test]
    fn the_texts_of_new_rows_are_made_one_after_the_other() {
        let mut index = songs_index(vec![song("Amber", "", ""), song("Velvet", "", "")]);
        assert_eq!(index.next_unmade(), None);
        // rows that come in are not found before their texts are made
        index.splice(Kind::Songs, 1, 0, 2);
        index.splice(Kind::Albums, 0, 0, 1);
        assert_eq!(found(&mut index, "amber"), [0]);
        assert_eq!(index.next_unmade(), Some((Kind::Songs, 1)));
        index.make(Kind::Songs, 1, song("Amber Tide", "", ""));
        assert_eq!(index.next_unmade(), Some((Kind::Songs, 2)));
        index.make(Kind::Songs, 2, song("Amber Glass", "", ""));
        assert_eq!(index.next_unmade(), Some((Kind::Albums, 0)));
        index.make(Kind::Albums, 0, song("Amber Days", "", ""));
        assert_eq!(index.next_unmade(), None);
        assert_eq!(found(&mut index, "amber"), [0, 1, 2]);
        // a change before the rows that are made: what follows it is looked at again
        index.splice(Kind::Songs, 0, 1, 1);
        assert_eq!(index.next_unmade(), Some((Kind::Songs, 0)));
        index.make(Kind::Songs, 0, song("Harbor", "", ""));
        assert_eq!(index.next_unmade(), None);
        assert_eq!(found(&mut index, "amber"), [1, 2]);
    }

    #[test]
    fn the_finder_follows_the_stores_of_the_library() {
        let library = Rc::new(Library::default());
        let track = |name: &str, artist: &str, id: &str| Track {
            library_id: Some(id.to_string()),
            album: "Winter Signal".into(),
            ..Track::new(None, name, artist)
        };
        library.songs.extend_from_slice(&[
            boxed(track("Amber Harbor", "Nova Vale", "i.1")),
            boxed(track("Velvet", "Glass Orbit", "i.2")),
        ]);
        library.albums.extend_from_slice(&[boxed(Item::Album(Album::new(None, "Amber Days", "Nova Vale")))]);
        let finder = Finder::new(&library);
        // rows that come after the finder was made are followed too
        library.artists.extend_from_slice(&[boxed(Item::Artist(Artist::new(None, "Amber")))]);
        library.playlists.extend_from_slice(&[boxed(Item::Playlist(Playlist::new(Some("p.1"), "Amber Mix")))]);
        library.songs.insert(0, &boxed(track("Amber", "Glass Orbit", "i.3")));

        let found = finder.find("amber", 8);
        let names = |tracks: &[Track]| tracks.iter().map(|track| track.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&found.results.songs), ["Amber", "Amber Harbor"]);
        assert_eq!(found.results.albums.len(), 1);
        assert_eq!(found.results.artists.len(), 1);
        assert_eq!(found.results.playlists.len(), 1);
        // the artist who is the term comes before the album and the playlist that begin with it
        let titles: Vec<&str> = found.items.iter().map(Item::title).collect();
        assert_eq!(titles, ["Amber", "Amber Days", "Amber Mix"]);

        let generation = finder.generation();
        library.songs.remove(0);
        assert_ne!(finder.generation(), generation);
        assert_eq!(names(&finder.find("amber", 8).results.songs), ["Amber Harbor"]);
        assert_eq!(finder.find("cinder", 8), Found::default());
    }

    /// How long the lists of a large library take to be made ready, and a search as it is typed
    /// (`cargo test --release -p am-ui -- --ignored --nocapture searching_a_large_library`).
    #[test]
    #[ignore]
    fn searching_a_large_library() {
        use std::time::Instant;
        const SONGS: usize = 50_000;
        let tracks: Vec<Track> = (0..SONGS)
            .map(|n| Track {
                album: format!("Album {} {}", n / 12, ["Winter", "Harbor", "Élan"][n % 3]),
                ..Track::new(None, format!("{} {}", ["Amber", "Velvet Tide", "Straße"][n % 3], n), "Glass Orbit")
            })
            .collect();
        let started = Instant::now();
        let entries: Vec<Entry> = tracks.iter().map(Entry::of_track).collect();
        println!("{SONGS} songs made ready in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        let mut index = songs_index(entries);
        for term in ["a", "am", "amb", "ambe", "amber", "amber 4", "amber 49", "q", "glass orbit", "zz"] {
            let started = Instant::now();
            let hits = index.find(&Query::new(term), 12);
            println!("{term:>12}: {:.2} ms, {} shown", started.elapsed().as_secs_f64() * 1000.0, hits[0].len());
        }
    }
}
