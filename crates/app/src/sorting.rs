//! The orders the lists of the library can be put in, and the keys that put them there.
//!
//! A row has one key per order: a string made of the fields the order goes by, the first one
//! first, such that comparing two keys byte by byte compares the rows. A sorted list model makes
//! the key of each of its rows once and sorts by comparing those (`pages::view_options`).
//!
//! Titles and names are ordered as people expect them (`write_text_key`): capitals and accents make no
//! difference, a leading "The", "A" or "An" is passed over, punctuation separates words and
//! nothing more, numbers go by their value and come before letters. A row that lacks what an
//! order goes by (an album without a release date) comes after all that have it.
//!
//! A search compares texts in the same way (`write_search_text`), except that a number is what
//! was typed and a leading article stays: someone who types "10" or "the" looks for them.

use std::fmt::Write;

use amui::types::{Album, Artist, Item, Playlist, Track};
use gtk::glib::{DecompositionKind, Unichar};

/// What a list of the library is ordered by. The directions are fixed, as in Apple Music: titles
/// and names from A to Z, dates from the newest to the oldest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Order {
    Title,
    Artist,
    Album,
    RecentlyAdded,
    ReleaseDate,
}

impl Order {
    pub const ALL: [Order; 5] = [Order::Title, Order::Artist, Order::Album, Order::RecentlyAdded, Order::ReleaseDate];

    /// The name on the command line and in the settings.
    pub fn name(self) -> &'static str {
        match self {
            Order::Title => "title",
            Order::Artist => "artist",
            Order::Album => "album",
            Order::RecentlyAdded => "recently-added",
            Order::ReleaseDate => "release-date",
        }
    }

    /// The name in a menu.
    pub fn label(self) -> &'static str {
        match self {
            Order::Title => "Title",
            Order::Artist => "Artist",
            Order::Album => "Album",
            Order::RecentlyAdded => "Recently Added",
            Order::ReleaseDate => "Release Date",
        }
    }

    pub fn parse(name: &str) -> Option<Order> {
        Order::ALL.into_iter().find(|order| order.name() == name)
    }
}

/// Between the fields of a key: lower than every character of a field, so that a field that
/// ends comes before one that goes on.
const FIELD_END: char = '\u{1}';
/// In place of a field that has no value: after every value.
const MISSING: char = '\u{10FFFF}';
/// Before a title that is made of signs only: before every digit and letter.
const SIGNS: char = ' ';
/// The largest number `date_key` gives is below this.
const DATE_END: u64 = 100_000_000_000_000;
/// `date_key` divided by this is the year.
const YEAR: u64 = 10_000_000_000;

/// How `Words` writes a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Numbers {
    /// By its value, so that keys put 9 before 10 (`Words::number`).
    ByValue,
    /// As it is written, its digits as letters of their word (what a search compares).
    AsWritten,
}

/// The words of a text as they are compared, written to the end of a key.
struct Words<'a> {
    key: &'a mut String,
    /// Where the words begin in `key`.
    start: usize,
    numbers: Numbers,
    /// The digits of a number that is being read.
    digits: String,
    /// Something that separates words was read since the last letter or digit.
    gap: bool,
    /// The last thing written was a letter.
    in_word: bool,
}

/// Signs that stand for an apostrophe: part of their word ("Don't" is "dont").
fn is_apostrophe(character: char) -> bool {
    matches!(character, '\'' | '`' | '\u{b4}' | '\u{2018}' | '\u{2019}' | '\u{2bc}' | '\u{2032}')
}

impl<'a> Words<'a> {
    fn new(key: &'a mut String, numbers: Numbers) -> Words<'a> {
        let start = key.len();
        Words { key, start, numbers, digits: String::new(), gap: false, in_word: false }
    }

    fn is_empty(&self) -> bool {
        self.key.len() == self.start && self.digits.is_empty()
    }

    /// One space between two words, none before the first.
    fn space(&mut self) {
        if self.gap && self.key.len() > self.start {
            self.key.push(' ');
        }
        self.gap = false;
    }

    fn letter(&mut self, letter: char) {
        self.number();
        self.space();
        self.key.push(letter);
        self.in_word = true;
    }

    fn separator(&mut self) {
        self.number();
        self.gap = true;
        self.in_word = false;
    }

    /// Write the number whose digits were read: how many digits it has, then the digits, so
    /// that 9 comes before 10 and both before any letter.
    fn number(&mut self) {
        if self.digits.is_empty() {
            return;
        }
        self.space();
        let digits = self.digits.trim_start_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        match digits.len() {
            count @ 1..=8 => self.key.push(char::from(b'0' + count as u8)),
            count => {
                let _ = write!(self.key, "9{:03}", count.min(999));
            }
        }
        self.key.push_str(digits);
        self.digits.clear();
        self.in_word = false;
    }

    /// Read one character that is in lower case and has no accent of its own.
    fn read(&mut self, character: char) {
        match character {
            'a'..='z' => self.letter(character),
            'A'..='Z' => self.letter(character.to_ascii_lowercase()),
            '0'..='9' if self.numbers == Numbers::AsWritten => self.letter(character),
            '0'..='9' => self.digits.push(character),
            _ if character.is_ascii() => self.separator(),
            // letters that are written as two, or with a stroke
            'ß' => self.letters("ss"),
            'æ' => self.letters("ae"),
            'œ' => self.letters("oe"),
            'þ' => self.letters("th"),
            'ø' => self.letter('o'),
            'đ' | 'ð' => self.letter('d'),
            'ł' => self.letter('l'),
            'ı' => self.letter('i'),
            'ς' => self.letter('σ'),
            _ if character.is_alphanumeric() => self.letter(character),
            // a vowel sign and the like belong to the letter before them
            _ if character.is_mark() => {
                if self.in_word {
                    self.key.push(character);
                }
            }
            _ => self.separator(),
        }
    }

    fn letters(&mut self, letters: &str) {
        letters.chars().for_each(|letter| self.letter(letter));
    }

    fn text(&mut self, text: &str) {
        for character in text.chars() {
            if is_apostrophe(character) {
                continue;
            }
            if character.is_ascii() {
                self.read(character);
                continue;
            }
            // a letter without its accents, a wide or a joined form as its plain letters
            for part in character.fully_decompose(DecompositionKind::Compatibility) {
                if part.combining_class() == 0 {
                    part.to_lowercase().for_each(|lower| self.read(lower));
                }
            }
        }
        self.number();
    }

    /// Forget what was written.
    fn clear(&mut self) {
        self.key.truncate(self.start);
        self.digits.clear();
        self.gap = false;
        self.in_word = false;
    }
}

/// `text` without a leading "The", "A" or "An" that has more after it.
fn without_article(text: &str) -> &str {
    for article in ["the", "an", "a"] {
        let Some(head) = text.get(..article.len()) else { continue };
        if !head.eq_ignore_ascii_case(article) {
            continue;
        }
        let rest = &text[article.len()..];
        let after = rest.trim_start();
        if after.len() < rest.len() && !after.is_empty() {
            return after;
        }
    }
    text
}

/// Write the key of a title or a name to the end of `key`; nothing for a text without one.
/// Texts compare as their keys do, byte by byte.
///
/// Capitals and accents make no difference ("Élan" is "elan"), a leading "The", "A" or "An" is
/// left out, an apostrophe is dropped, other punctuation and signs separate words, a number
/// counts by its value (2 before 10) and comes before any letter, and what is written in other
/// scripts follows the Latin letters.
fn write_text_key(key: &mut String, text: &str) {
    let text = text.trim();
    let rest = without_article(text);
    let mut words = Words::new(key, Numbers::ByValue);
    words.text(rest);
    if words.is_empty() && rest.len() < text.len() {
        // an article and signs: the article is the title
        words.clear();
        words.text(text);
    }
    if words.is_empty() && !text.is_empty() {
        // signs only: among themselves, before everything that has a letter or a digit
        key.push(SIGNS);
        key.extend(text.chars().filter(|character| !character.is_control()).flat_map(char::to_lowercase));
    }
}

/// Write `text` as a search compares it to the end of `out`: its words as a key has them
/// (capitals and accents make no difference, an apostrophe is dropped, other signs separate
/// words) with one space between them, but a number as it is written and a leading article
/// kept. A text of signs only stays as it is, in lower case, so that it can still be found.
pub fn write_search_text(out: &mut String, text: &str) {
    let text = text.trim();
    let start = out.len();
    Words::new(out, Numbers::AsWritten).text(text);
    if out.len() == start {
        out.extend(text.chars().filter(|character| !character.is_control()).flat_map(char::to_lowercase));
    }
}

/// `text` as a search compares it (`write_search_text`).
pub fn search_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    write_search_text(&mut out, text);
    out
}

/// The moment a date of the service names ("2019", "2019-03-08", "2024-11-02T10:41:07Z") as a
/// number that grows with time; `None` for a text that names no year.
pub fn date_key(date: &str) -> Option<u64> {
    let date = date.trim();
    let (day, clock) = date.split_once(['T', ' ']).unwrap_or((date, ""));
    let mut day = day.split('-');
    let year: u64 = day.next().filter(|year| year.len() == 4).and_then(|year| year.parse().ok())?;
    let part = |text: Option<&str>, most: u64| -> u64 {
        text.and_then(|text| text.parse().ok()).filter(|number| *number <= most).unwrap_or(0)
    };
    let month = part(day.next(), 12);
    let day = if month == 0 { 0 } else { part(day.next(), 31) };
    let mut clock = clock.split(|character: char| !character.is_ascii_digit());
    let (hour, minute, second) = (part(clock.next(), 23), part(clock.next(), 59), part(clock.next(), 60));
    Some(((((year * 100 + month) * 100 + day) * 100 + hour) * 100 + minute) * 100 + second)
}

/// A key made of several fields: rows are ordered by the first, rows that are equal in it by
/// the second, and so on.
struct Key(String);

impl Key {
    fn new() -> Key {
        Key(String::with_capacity(96))
    }

    fn end(mut self) -> Key {
        self.0.push(FIELD_END);
        self
    }

    /// A title or a name, from A to Z; rows without one last.
    fn text(mut self, text: &str) -> Key {
        let start = self.0.len();
        write_text_key(&mut self.0, text);
        if self.0.len() == start {
            self.0.push(MISSING);
        }
        self.end()
    }

    /// A date, the newest first; rows without one last.
    fn newest(mut self, date: Option<&str>) -> Key {
        match date.and_then(date_key) {
            Some(moment) => {
                let _ = write!(self.0, "{:014}", DATE_END - 1 - moment.min(DATE_END - 1));
            }
            None => self.0.push(MISSING),
        }
        self.end()
    }

    /// The year of a date, the oldest first; rows without one last. It keeps apart what shares a
    /// title: two albums of one name, and the songs of each.
    fn year(mut self, date: Option<&str>) -> Key {
        match date.and_then(date_key) {
            Some(moment) => {
                let _ = write!(self.0, "{:04}", moment / YEAR);
            }
            None => self.0.push(MISSING),
        }
        self.end()
    }

    /// A small number (of a disc, of a song on it), from the lowest; rows without one last.
    fn number(mut self, number: Option<i64>) -> Key {
        match number {
            Some(number) => {
                let _ = write!(self.0, "{:05}", number.clamp(0, 99_999));
            }
            None => self.0.push(MISSING),
        }
        self.end()
    }

    /// Rows for which `first` holds come before all others.
    fn first(mut self, first: bool) -> Key {
        self.0.push(if first { '0' } else { '1' });
        self.end()
    }
}

/// The key of an album in `order`. Albums that are equal in what the order goes by follow each
/// other by artist, then by title (then by year: an artist's albums of one name).
pub fn album_key(album: &Album, order: Order) -> String {
    let key = match order {
        Order::Title | Order::Album => Key::new().text(&album.name).text(&album.artist),
        Order::Artist => Key::new().text(&album.artist).text(&album.name),
        Order::RecentlyAdded => Key::new().newest(album.date_added.as_deref()).text(&album.artist).text(&album.name),
        Order::ReleaseDate => Key::new().newest(album.release_date.as_deref()).text(&album.artist).text(&album.name),
    };
    key.year(album.release_date.as_deref()).0
}

/// The key of a song in `order`. Songs that are equal in what the order goes by follow each
/// other by artist, then by album, then as they are on the album.
///
/// A song of the library does not say which album it is on beyond the album's title, nor who
/// the album is by: the songs of one album are those with its title and its year. That keeps an
/// album together whoever sings its songs (guests, a compilation), and two albums of one title
/// apart from each other.
pub fn track_key(track: &Track, order: Order) -> String {
    let on_album = |key: Key| {
        key.text(&track.album).year(track.release_date.as_deref()).number(track.disc_number).number(track.track_number)
    };
    let key = match order {
        Order::Title => on_album(Key::new().text(&track.name).text(&track.artist)),
        Order::Album => on_album(Key::new()).text(&track.artist),
        Order::Artist => on_album(Key::new().text(&track.artist)),
        Order::RecentlyAdded => on_album(Key::new().newest(track.date_added.as_deref()).text(&track.artist)),
        Order::ReleaseDate => on_album(Key::new().newest(track.release_date.as_deref()).text(&track.artist)),
    };
    key.text(&track.name).0
}

/// The key of a playlist in `order`: by title, or the one added last first. Apple's Favorite
/// Songs playlist comes first in every order.
pub fn playlist_key(playlist: &Playlist, order: Order) -> String {
    let key = Key::new().first(playlist.is_favorites);
    match order {
        Order::RecentlyAdded => key.newest(playlist.date_added.as_deref()).text(&playlist.name).0,
        _ => key.text(&playlist.name).0,
    }
}

/// The key of an artist: by name.
pub fn artist_key(artist: &Artist) -> String {
    Key::new().text(&artist.name).0
}

/// The key of a row of a list of albums, artists or playlists in `order`.
pub fn item_key(item: &Item, order: Order) -> String {
    match item {
        Item::Album(album) => album_key(album, order),
        Item::Playlist(playlist) => playlist_key(playlist, order),
        Item::Artist(artist) => artist_key(artist),
        Item::Track(track) => track_key(track, order),
        Item::Station(station) => Key::new().text(&station.name).0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key of a title or a name alone (an empty text has an empty key).
    fn text_key(text: &str) -> String {
        let mut key = String::new();
        write_text_key(&mut key, text);
        key
    }

    /// `texts` in the order of their keys; texts with the same key stay as they were.
    fn ordered<'a>(texts: &[&'a str]) -> Vec<&'a str> {
        let mut texts = texts.to_vec();
        texts.sort_by_key(|text| text_key(text));
        texts
    }

    #[test]
    fn capitals_and_accents_make_no_difference() {
        assert_eq!(text_key("Harbor Glass"), text_key("HARBOR glass"));
        assert_eq!(text_key("Élan Meadow"), text_key("elan meadow"));
        assert_eq!(text_key("Åmber Föld"), "amber fold");
        assert_eq!(text_key("Señor Çinder"), "senor cinder");
        // an accent typed as a character of its own
        assert_eq!(text_key("Cafe\u{301} Meadow"), text_key("Café Meadow"));
        // letters written as two, or with a stroke
        assert_eq!(text_key("Straße"), "strasse");
        assert_eq!(text_key("Ærø"), "aero");
        assert_eq!(text_key("Łódź"), "lodz");
        // wide forms and ligatures
        assert_eq!(text_key("Ｗｉｄｅ ｆｏｒｍ"), "wide form");
        assert_eq!(text_key("ﬁre"), "fire");
        assert_eq!(
            ordered(&["Zenith", "élan", "Ember", "Ørbit", "apex", "Öl", "Echo"]),
            ["apex", "Echo", "élan", "Ember", "Öl", "Ørbit", "Zenith"]
        );
    }

    #[test]
    fn a_leading_article_is_passed_over() {
        assert_eq!(text_key("The Paper Lanterns"), "paper lanterns");
        assert_eq!(text_key("the paper lanterns"), "paper lanterns");
        assert_eq!(text_key("A Quiet Harbor"), "quiet harbor");
        assert_eq!(text_key("An Amber Evening"), "amber evening");
        assert_eq!(text_key("  The   Tide"), "tide");
        // only at the beginning, and only as a word of its own
        assert_eq!(text_key("Over the Meadow"), "over the meadow");
        assert_eq!(text_key("Theory of Glass"), "theory of glass");
        assert_eq!(text_key("Another Winter"), "another winter");
        assert_eq!(text_key("A-Side Stories"), "a side stories");
        assert_eq!(text_key("A.M. Signal"), "a m signal");
        // an article that is the whole title stays
        assert_eq!(text_key("The"), "the");
        assert_eq!(text_key("A"), "a");
        assert_eq!(text_key("The ..."), "the");
        assert_eq!(text_key("The The"), "the");
        assert_eq!(
            ordered(&["The Velvet Hour", "An Orbit", "Marble", "A Lantern", "Then Came Winter"]),
            ["A Lantern", "Marble", "An Orbit", "Then Came Winter", "The Velvet Hour"]
        );
    }

    #[test]
    fn digits_come_before_letters_and_numbers_count_by_their_value() {
        assert_eq!(
            ordered(&["Amber", "9 Lanterns", "10 Lanterns", "100 Tides", "2 Harbors", "Zero"]),
            ["2 Harbors", "9 Lanterns", "10 Lanterns", "100 Tides", "Amber", "Zero"]
        );
        assert_eq!(
            ordered(&["Volume 10", "Volume 2", "Volume 1", "Volume", "Volume 2 (Live)"]),
            ["Volume", "Volume 1", "Volume 2", "Volume 2 (Live)", "Volume 10"]
        );
        // a number that follows an article is a number
        assert_eq!(ordered(&["Amber", "The 1999 Tapes", "3 Lanterns"]), ["3 Lanterns", "The 1999 Tapes", "Amber"]);
        // zeros in front do not count
        assert_eq!(text_key("Track 007"), text_key("Track 7"));
        assert_eq!(text_key("000"), text_key("0"));
        // a very long number is still a number
        assert!(text_key("Take 99999999") < text_key("Take 100000000"));
        assert!(text_key("Take 999999999") < text_key("Take 1000000000"));
        // wide digits are digits
        assert_eq!(text_key("Ｎｏ． ４２"), text_key("No. 42"));
    }

    #[test]
    fn punctuation_separates_words_and_nothing_more() {
        assert_eq!(text_key("(Glass) Meadow"), "glass meadow");
        assert_eq!(text_key("...and the Tide"), "and the tide");
        assert_eq!(text_key("“Juniper”"), "juniper");
        assert_eq!(text_key("Amber, Amber!"), "amber amber");
        assert_eq!(text_key("Neon/Static"), "neon static");
        assert_eq!(text_key("Atlas & The Drift"), "atlas the drift");
        // an apostrophe is part of its word, whichever way it is typed
        assert_eq!(text_key("Don't Drift"), "dont drift");
        assert_eq!(text_key("Don’t Drift"), "dont drift");
        assert_eq!(text_key("Don´t Drift"), "dont drift");
        assert_eq!(text_key("Don`t Drift"), "dont drift");
        assert_eq!(text_key("'Til Winter"), "til winter");
        // a word that ends comes before one that goes on
        assert_eq!(ordered(&["Blackbird Signal", "Black Tide", "Black"]), ["Black", "Black Tide", "Blackbird Signal"]);
        // signs only: among themselves, before digits and letters
        assert_eq!(ordered(&["Amber", "1", "÷", "+", "?!"]), ["+", "?!", "÷", "1", "Amber"]);
        assert_eq!(text_key(""), "");
        assert_eq!(text_key("   "), "");
    }

    #[test]
    fn other_scripts_follow_the_latin_letters() {
        assert_eq!(
            ordered(&["夏の灯台", "Zenith", "밤의 정원", "Amber", "Ωmega", "Ясный"]),
            ["Amber", "Zenith", "Ωmega", "Ясный", "밤의 정원", "夏の灯台"]
        );
        // capitals and accents of other scripts
        assert_eq!(text_key("ΆΛΦΑ"), text_key("αλφα"));
        assert_eq!(text_key("ОЗЕРО"), text_key("озеро"));
        // a voiced kana sorts with its plain one, and a syllable with its parts
        assert_eq!(text_key("がらす"), text_key("からす"));
        assert!(text_key("가나") < text_key("각"));
        // a vowel sign stays with its letter
        assert_ne!(text_key("कि"), text_key("कु"));
        // a picture is a sign
        assert_eq!(text_key("Honey 🍯 Tide"), "honey tide");
    }

    #[test]
    fn a_search_compares_words_as_keys_do_with_numbers_and_articles_as_typed() {
        assert_eq!(search_text("  Élan,  Straße & Co. "), "elan strasse co");
        assert_eq!(search_text("Don’t Drift"), "dont drift");
        assert_eq!(search_text("Cafe\u{301} Meadow"), search_text("Café meadow"));
        assert_eq!(search_text("ΆΛΦΑ"), search_text("αλφα"));
        assert_eq!(search_text("Ｎｏ． ４２"), "no 42");
        // a leading article stays, numbers are as they are written
        assert_eq!(search_text("The Paper Lanterns"), "the paper lanterns");
        assert_eq!(search_text("10 Lanterns, Track 007"), "10 lanterns track 007");
        assert_eq!(search_text("1st Light"), "1st light");
        // signs only: as they are, in lower case; nothing for nothing
        assert_eq!(search_text("?!"), "?!");
        assert_eq!(search_text("   "), "");
        // written after what is there
        let mut text = String::from("amber\u{1}");
        write_search_text(&mut text, "Glass Orbit");
        assert_eq!(text, "amber\u{1}glass orbit");
    }

    #[test]
    fn dates_grow_with_time() {
        assert_eq!(date_key("2019"), Some(20_190_000_000_000));
        assert_eq!(date_key("2019-03"), Some(20_190_300_000_000));
        assert_eq!(date_key("2019-03-08"), Some(20_190_308_000_000));
        assert_eq!(date_key("2024-11-02T10:41:07Z"), Some(20_241_102_104_107));
        assert_eq!(date_key("2024-11-02T10:41:07.250Z"), Some(20_241_102_104_107));
        assert_eq!(date_key(" 2024-11-02 10:41 "), Some(20_241_102_104_100));
        assert!(date_key("2019-12-31") < date_key("2020"));
        assert!(date_key("2024-11-02T10:41:07Z") < date_key("2024-11-02T10:41:08Z"));
        // what is not a month or a day counts as unknown
        assert_eq!(date_key("2019-13-40"), date_key("2019"));
        assert_eq!(date_key(""), None);
        assert_eq!(date_key("soon"), None);
        assert_eq!(date_key("19"), None);
        assert_eq!(date_key("2019-03-08").map(|moment| moment / YEAR), Some(2019));
    }

    fn album(name: &str, artist: &str, released: Option<&str>, added: Option<&str>) -> Album {
        Album {
            release_date: released.map(str::to_owned),
            date_added: added.map(str::to_owned),
            ..Album::new(None, name, artist)
        }
    }

    fn albums_in(order: Order, albums: &[Album]) -> Vec<&str> {
        let mut albums: Vec<&Album> = albums.iter().collect();
        albums.sort_by_key(|album| album_key(album, order));
        albums.into_iter().map(|album| album.name.as_str()).collect()
    }

    #[test]
    fn albums_in_every_order() {
        let albums = [
            album("Winter Signal", "Nova Vale", Some("2019-03-08"), Some("2026-01-05T09:00:00Z")),
            album("The Amber Hour", "The Paper Lanterns", Some("2021"), Some("2026-03-01T09:00:00Z")),
            album("amber hour", "Ada Quill", Some("2004-06-01"), None),
            album("Élan", "nova vale", None, Some("2026-03-01T18:30:00Z")),
            album("10 Lanterns", "Glass Orbit", Some("2021-02-14"), Some("2025-12-24T20:00:00Z")),
            album("Marble", "", Some("1998-10-10"), Some("2025-12-24T20:00:00Z")),
        ];
        // by title; albums of one title by artist
        assert_eq!(
            albums_in(Order::Title, &albums),
            ["10 Lanterns", "amber hour", "The Amber Hour", "Élan", "Marble", "Winter Signal"]
        );
        // by artist, then by title; an album without an artist last
        assert_eq!(
            albums_in(Order::Artist, &albums),
            ["amber hour", "10 Lanterns", "Élan", "Winter Signal", "The Amber Hour", "Marble"]
        );
        // the one added last first, albums added at the same moment by artist; never added last
        assert_eq!(
            albums_in(Order::RecentlyAdded, &albums),
            ["Élan", "The Amber Hour", "Winter Signal", "10 Lanterns", "Marble", "amber hour"]
        );
        // the newest release first (a year alone is its beginning); no release date last
        assert_eq!(
            albums_in(Order::ReleaseDate, &albums),
            ["10 Lanterns", "The Amber Hour", "Winter Signal", "amber hour", "Marble", "Élan"]
        );
    }

    #[test]
    fn albums_of_one_name_by_one_artist_follow_their_years() {
        let albums = [
            album("Nova Vale", "Nova Vale", Some("2008-05-01"), None),
            album("Nova Vale", "Nova Vale", Some("1994-05-10"), None),
            album("Nova Vale", "Nova Vale", None, None),
        ];
        let years = |order: Order| {
            let mut albums: Vec<&Album> = albums.iter().collect();
            albums.sort_by_key(|album| album_key(album, order));
            albums.into_iter().map(|album| album.release_date.as_deref().unwrap_or("-")).collect::<Vec<_>>()
        };
        assert_eq!(years(Order::Title), ["1994-05-10", "2008-05-01", "-"]);
        assert_eq!(years(Order::Artist), ["1994-05-10", "2008-05-01", "-"]);
    }

    fn song(name: &str, artist: &str, album: &str, disc: i64, number: i64, added: Option<&str>) -> Track {
        Track {
            album: album.to_string(),
            disc_number: Some(disc),
            track_number: Some(number),
            date_added: added.map(str::to_owned),
            release_date: Some("2019-03-08".into()),
            ..Track::new(None, name, artist)
        }
    }

    fn songs_in(order: Order, songs: &[Track]) -> Vec<&str> {
        let mut songs: Vec<&Track> = songs.iter().collect();
        songs.sort_by_key(|song| track_key(song, order));
        songs.into_iter().map(|song| song.name.as_str()).collect()
    }

    #[test]
    fn songs_in_every_order() {
        let songs = [
            song("Tide", "Nova Vale", "Winter Signal", 1, 2, Some("2026-01-05T09:00:00Z")),
            song("Cinder", "Nova Vale", "Winter Signal", 1, 10, Some("2026-01-05T09:00:00Z")),
            song("The Drift", "Nova Vale", "Winter Signal", 2, 1, Some("2026-01-05T09:00:00Z")),
            song("Amber", "Nova Vale & Ada Quill", "Winter Signal", 1, 1, Some("2026-01-05T09:00:00Z")),
            song("Amber", "Ada Quill", "Honey", 1, 3, Some("2026-02-11T12:00:00Z")),
            song("Solace", "Ada Quill", "A Quiet Harbor", 1, 1, None),
            song("Mirage", "Ada Quill", "", 1, 1, Some("2026-02-11T12:00:00Z")),
        ];
        // by title; songs of one title by artist
        assert_eq!(
            songs_in(Order::Title, &songs),
            ["Amber", "Amber", "Cinder", "The Drift", "Mirage", "Solace", "Tide"]
        );
        assert!(track_key(&songs[4], Order::Title) < track_key(&songs[3], Order::Title));
        // by artist, then by album (the songs without one last), then as they are on the album
        assert_eq!(
            songs_in(Order::Artist, &songs),
            ["Amber", "Solace", "Mirage", "Tide", "Cinder", "The Drift", "Amber"]
        );
        // by album: its songs stay together as they are on it, whoever sings them
        assert_eq!(
            songs_in(Order::Album, &songs),
            ["Amber", "Solace", "Amber", "Tide", "Cinder", "The Drift", "Mirage"]
        );
        // the ones added last first, then by artist, album and place on it; never added last
        assert_eq!(
            songs_in(Order::RecentlyAdded, &songs),
            ["Amber", "Mirage", "Tide", "Cinder", "The Drift", "Amber", "Solace"]
        );
    }

    #[test]
    fn two_albums_of_one_title_keep_their_songs_apart() {
        let on = |name: &str, artist: &str, released: &str, number: i64| Track {
            release_date: Some(released.into()),
            ..song(name, artist, "Night Signals", 1, number, None)
        };
        let songs = [
            on("Tide", "Nova Vale", "2012-04-02", 1),
            on("Ember", "Glass Orbit", "2003-09-15", 1),
            on("Cinder", "Nova Vale", "2012-04-02", 2),
            on("Solace", "Glass Orbit & Ada Quill", "2003-09-15", 2),
            on("Mirage", "Nova Vale", "2012-04-02", 3),
            on("Signal", "Glass Orbit", "2003-09-15", 3),
        ];
        // the older album first, each in its own order, whoever sings the songs
        assert_eq!(songs_in(Order::Album, &songs), ["Ember", "Solace", "Signal", "Tide", "Cinder", "Mirage"]);
        // songs without a disc or a number come after those with one
        let mut loose = songs[0].clone();
        (loose.name, loose.track_number) = ("Drift".into(), None);
        let mut all = songs.to_vec();
        all.push(loose);
        assert_eq!(songs_in(Order::Album, &all)[3..], ["Tide", "Cinder", "Mirage", "Drift"]);
    }

    #[test]
    fn playlists_keep_the_favorite_songs_first() {
        let playlist = |name: &str, added: Option<&str>, favorites: bool| Playlist {
            date_added: added.map(str::to_owned),
            is_favorites: favorites,
            ..Playlist::new(None, name)
        };
        let playlists = [
            playlist("Quiet Evenings", Some("2026-05-02T09:00:00Z"), false),
            playlist("Favorite Songs", Some("2024-01-01T09:00:00Z"), true),
            playlist("A Drive", Some("2026-09-30T09:00:00Z"), false),
            playlist("amber mix", None, false),
        ];
        let names_in = |order: Order| {
            let mut playlists: Vec<&Playlist> = playlists.iter().collect();
            playlists.sort_by_key(|playlist| playlist_key(playlist, order));
            playlists.into_iter().map(|playlist| playlist.name.as_str()).collect::<Vec<_>>()
        };
        assert_eq!(names_in(Order::Title), ["Favorite Songs", "amber mix", "A Drive", "Quiet Evenings"]);
        assert_eq!(names_in(Order::RecentlyAdded), ["Favorite Songs", "A Drive", "Quiet Evenings", "amber mix"]);
    }

    #[test]
    fn rows_of_a_list_have_the_key_of_what_they_show() {
        let artist = Artist::new(None, "The Paper Lanterns");
        assert_eq!(artist_key(&artist), "paper lanterns\u{1}");
        assert_eq!(item_key(&Item::Artist(artist.clone()), Order::RecentlyAdded), artist_key(&artist));
        let album = album("Marble", "Glass Orbit", None, None);
        assert_eq!(item_key(&Item::Album(album.clone()), Order::Artist), album_key(&album, Order::Artist));
        assert_eq!(album_key(&album, Order::Title), "marble\u{1}glass orbit\u{1}\u{10FFFF}\u{1}");
        // an artist without a name comes last
        assert!(artist_key(&artist) < artist_key(&Artist::new(None, "")));
        assert!(artist_key(&Artist::new(None, "夏の終わり")) < artist_key(&Artist::new(None, " ")));
        // a key holds no NUL: a sorter reads it as a C string
        let track = Track { album: "\0".into(), ..Track::new(None, "A\0B", "\u{1}") };
        assert!(Order::ALL.iter().all(|order| !track_key(&track, *order).contains('\0')));
    }

    #[test]
    fn orders_have_names() {
        for order in Order::ALL {
            assert_eq!(Order::parse(order.name()), Some(order));
            assert!(!order.label().is_empty());
        }
        assert_eq!(Order::parse("recently-added"), Some(Order::RecentlyAdded));
        assert_eq!(Order::parse("Title"), None);
        assert_eq!(Order::parse(""), None);
    }
}
