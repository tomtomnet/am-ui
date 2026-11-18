//! The page of an artist: the picture and the name, the top songs, the albums, the singles and
//! EPs. The grids of cards are embedded in the column (`widgets::embedded`): only the cards in
//! view exist, however many releases there are.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use amui::types::{Album, Track};
use gtk::{gdk, gio, glib};

use crate::actions;
use crate::app::ctx;
use crate::data::{ArtistPage, Item, Problem, Ref};
use crate::library::boxed;
use crate::nav::Dest;
use crate::pages::detail::{self, Cache, Frame};
use crate::pages::grid::card_grid;
use crate::pages::songs::{Line, Place, bind_row, new_row, row_handler};
use crate::pages::{card_action, shell};
use crate::refresh::Task;
use crate::signal::background;
use crate::widgets::card::CardHandler;
use crate::widgets::columns::Columns;
use crate::widgets::cover::{Blank, Cover, initials};
use crate::widgets::embedded::Embedded;
use crate::widgets::hero::Hero;
use crate::widgets::track_row::RowStyle;

/// The side of the picture, from a narrow window to a wide one.
const PICTURE: (i32, i32) = (132, 184);
/// The top songs fill two columns once each can be this wide.
const SONG_COLUMN: i32 = 400;

thread_local! {
    static CACHE: RefCell<Cache<ArtistPage>> = const { RefCell::new(Cache::new()) };
    static HOOKED: Cell<bool> = const { Cell::new(false) };
}

/// The page of one artist.
struct View {
    dest: Dest,
    target: Ref,
    page: glib::WeakRef<adw::NavigationPage>,
    frame: Frame,
    shown: RefCell<Option<Rc<ArtistPage>>>,
    /// Counts the fetches: an answer to an earlier one is dropped.
    generation: Cell<u64>,
    busy: Cell<bool>,
    /// When what is shown was fetched (`None`: to be fetched again once the page is seen).
    fetched: Cell<Option<Instant>>,
}

/// The page of the artist `target` (the destination `dest`).
pub fn page(dest: &Dest, target: &Ref) -> adw::NavigationPage {
    if !HOOKED.replace(true) {
        ctx().refreshed.connect_forever(|_| CACHE.with(|cache| cache.borrow_mut().expire(|_| true)));
    }
    let holder = adw::Bin::new();
    let shell = shell(dest.title(), &holder);
    let frame = Frame::new(&shell.header);
    holder.set_child(Some(&frame.stack));
    let view = Rc::new(View {
        dest: dest.clone(),
        target: target.clone(),
        page: shell.page.downgrade(),
        frame,
        shown: RefCell::default(),
        generation: Cell::new(0),
        busy: Cell::new(false),
        fetched: Cell::new(None),
    });
    let weak = Rc::downgrade(&view);
    view.frame.connect_retry(move || {
        if let Some(view) = weak.upgrade() {
            view.fetch(None);
        }
    });
    let weak = Rc::downgrade(&view);
    ctx().refreshed.connect(&view.frame.stack, move |_, refreshing| {
        if let Some(view) = weak.upgrade() {
            view.fetched.set(None);
            if view.frame.stack.is_mapped() {
                view.fetch(Some(refreshing.task()));
            }
        }
    });
    // The page owns its view. Each time it is shown again it looks whether what it shows is
    // still fresh.
    let seen = view.clone();
    shell.page.connect_showing(move |_| {
        if !seen.busy.get() && (!seen.frame.has_content() || !detail::is_fresh(seen.fetched.get(), Instant::now())) {
            seen.fetch(None);
        }
    });
    match CACHE.with(|cache| cache.borrow_mut().get(dest)) {
        Some((artist, fetched)) => {
            view.fetched.set(fetched);
            view.show(artist);
            if !detail::is_fresh(fetched, Instant::now()) {
                view.fetch(None);
            }
        }
        None => view.fetch(None),
    }
    shell.page
}

impl View {
    /// Ask the service. While the page shows something that happens quietly: the content is
    /// replaced only if the answer differs, and a failure is only told to `task`, the refresh
    /// that waits for it.
    fn fetch(self: &Rc<Self>, task: Option<Task>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.busy.set(true);
        if !self.frame.has_content() {
            self.frame.wait();
        }
        let (music, target, weak) = (ctx().music.clone(), self.target.clone(), Rc::downgrade(self));
        background(
            move || music.artist(&target),
            move |result| {
                let Some(view) = weak.upgrade().filter(|view| view.generation.get() == generation) else { return };
                view.busy.set(false);
                match result {
                    Ok(artist) => view.arrived(Rc::new(artist)),
                    Err(problem) => view.failed(&problem, task.as_ref()),
                }
            },
        );
    }

    fn arrived(self: &Rc<Self>, artist: Rc<ArtistPage>) {
        let now = Instant::now();
        self.fetched.set(Some(now));
        CACHE.with(|cache| cache.borrow_mut().put(&self.dest, artist.clone(), now));
        if self.shown.borrow().as_deref() != Some(&*artist) {
            self.show(artist);
        }
    }

    fn failed(&self, problem: &Problem, task: Option<&Task>) {
        if let Some(task) = task {
            task.failed(problem);
        }
        if !self.frame.has_content() {
            self.frame.fail("Couldn't Load This Artist", problem);
        }
    }

    /// Put `artist` on the page, in place of what was there.
    fn show(self: &Rc<Self>, artist: Rc<ArtistPage>) {
        let (column, name) = build(&artist);
        self.frame.show(&column, &name);
        if let Some(page) = self.page.upgrade() {
            page.set_title(&artist.artist.name);
        }
        self.shown.replace(Some(artist));
        if let Some(offset) = detail::initial_scroll() {
            self.frame.scroll_soon(offset);
        }
    }
}

/// The column of the page, and the widget in it that names the page.
fn build(page: &ArtistPage) -> (gtk::Box, gtk::Label) {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let (hero, name) = header(page);
    column.append(&hero);
    if !page.top_songs.is_empty() {
        column.append(&detail::section_title("Top Songs"));
        column.append(&top_songs(&page.top_songs));
    }
    let (albums, singles) = releases(&page.albums);
    for (title, albums) in [("Albums", albums), ("Singles & EPs", singles)] {
        if !albums.is_empty() {
            column.append(&detail::section_title(title));
            column.append(&Embedded::new(&cards(&albums)));
        }
    }
    if page.top_songs.is_empty() && page.albums.is_empty() {
        let note = gtk::Label::new(Some("There is no music by this artist to show."));
        note.set_xalign(0.0);
        note.add_css_class("page-note");
        column.append(&note);
    }
    (column, name)
}

/// The picture (or the initials), the name, and Play for the top songs.
fn header(page: &ArtistPage) -> (Hero, gtk::Label) {
    let artist = &page.artist;
    let cover = Cover::new(PICTURE.0);
    cover.set_round(true);
    cover.set_blank(Blank::Initials(initials(&artist.name)));
    cover.set_artworks(artist.artwork.as_slice());

    // the name and the button in the middle of the picture's height
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let filler = || {
        let filler = gtk::Box::new(gtk::Orientation::Vertical, 0);
        filler.set_vexpand(true);
        filler
    };
    text.append(&filler());
    let name = gtk::Label::new(Some(&artist.name));
    name.set_xalign(0.0);
    name.set_wrap(true);
    name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    name.set_lines(2);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.add_css_class("hero-name");
    text.append(&name);
    let genre = detail::meta_line(std::slice::from_ref(&artist.genre));
    if !genre.is_empty() {
        let label = gtk::Label::new(Some(&genre));
        label.set_xalign(0.0);
        label.add_css_class("hero-meta");
        text.append(&label);
    }
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    buttons.add_css_class("hero-actions");
    let play = detail::pill("am-media-playback-start-symbolic", "Play");
    play.set_sensitive(page.top_songs.iter().any(Track::playable));
    let songs = page.top_songs.clone();
    play.connect_clicked(move |_| {
        if let Some(start) = songs.iter().position(Track::playable) {
            actions::play_tracks(songs.clone(), start);
        }
    });
    buttons.append(&play);
    text.append(&buttons);
    text.append(&filler());
    (Hero::new(&cover, &text, PICTURE.0, PICTURE.1), name)
}

/// The top songs: rows with artwork, the album under the title, in two columns when the page
/// is wide.
fn top_songs(songs: &[Track]) -> Columns {
    let columns = Columns::new(SONG_COLUMN);
    columns.add_css_class("top-songs");
    let place = Place { playlist: None, line: Line::Album, songs_page: false };
    let style = RowStyle { numbered: false, artist: true, album: false };
    let tracks = songs.to_vec();
    let play = Rc::new(move |position: u32| actions::play_tracks(tracks.clone(), position as usize));
    for (position, track) in songs.iter().enumerate() {
        let position = position as u32;
        let handler = {
            let play = play.clone();
            row_handler(move |position| play(position), &place)
        };
        let row = new_row(style, handler);
        bind_row(&row, position, track, &place);
        // what the list does in a list view: a double click or Enter plays from the row, the
        // arrow keys go from row to row
        row.set_focusable(true);
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let (weak, start) = (row.downgrade(), play.clone());
        click.connect_pressed(move |_, presses, x, _| {
            let Some(row) = weak.upgrade() else { return };
            row.grab_focus();
            // the artwork plays on the first click already
            if presses == 2 && !row.on_lead(x) {
                start(position);
            }
        });
        row.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        let start = play.clone();
        keys.connect_key_pressed(move |keys, key, _, _| {
            let row = keys.widget();
            let other = match key {
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => {
                    start(position);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Down => row.and_then(|row| row.next_sibling()),
                gdk::Key::Up => row.and_then(|row| row.prev_sibling()),
                _ => None,
            };
            // past the first and the last row the keys are the window's again
            match other {
                Some(other) => {
                    other.grab_focus();
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        row.add_controller(keys);
        columns.append(&row);
    }
    columns
}

/// The albums and the singles and EPs, each newest first.
fn releases(albums: &[Album]) -> (Vec<Album>, Vec<Album>) {
    let mut sorted: Vec<Album> = albums.to_vec();
    // dates are written year first: their order as text is their order in time
    sorted.sort_by(|a, b| b.release_date.cmp(&a.release_date));
    sorted.into_iter().partition(|album| album.is_single != Some(true))
}

/// A grid of cards for `albums`. Every card is by this artist, so the line under the title
/// says when the album came out instead.
fn cards(albums: &[Album]) -> gtk::GridView {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let mut originals: HashMap<String, Item> = HashMap::new();
    for album in albums {
        let year = album.release_date.as_deref().and_then(detail::year).unwrap_or_default();
        let shown = Item::Album(Album { artist: year, ..album.clone() });
        originals.insert(shown.key(), Item::Album(album.clone()));
        store.append(&boxed(shown));
    }
    // what a card does, it does with the album as the service gave it
    let handler: CardHandler =
        Rc::new(move |item, action, card| card_action(originals.get(&item.key()).unwrap_or(item), action, card));
    let grid = card_grid(&store, handler);
    grid.add_css_class("embedded");
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_are_split_and_newest_first() {
        let album = |name: &str, date: Option<&str>, single: Option<bool>| Album {
            release_date: date.map(str::to_string),
            is_single: single,
            ..Album::new(Some(name), name, "Artist A")
        };
        let all = [
            album("a", Some("2011-03-01"), Some(false)),
            album("b", Some("2019-10-25"), None),
            album("c", Some("2015-06-12"), Some(true)),
            album("d", None, Some(false)),
            album("e", Some("2021-01-08"), Some(true)),
            album("f", Some("2019-02-01"), Some(false)),
        ];
        let (albums, singles) = releases(&all);
        let names = |albums: &[Album]| albums.iter().map(|album| album.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&albums), ["b", "f", "a", "d"]);
        assert_eq!(names(&singles), ["e", "c"]);
        assert_eq!(releases(&[]), (Vec::new(), Vec::new()));
    }
}
