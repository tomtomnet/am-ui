//! What the pages of an album, a playlist and an artist have in common: the place where a page
//! shows what it has (a column that scrolls as a whole, a spinner while nothing is known, what
//! went wrong and a way to try again), a small cache so that a page opened again is there at
//! once, the pages named on the command line, and the words for dates and formats.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

use crate::app::ctx;
use crate::data::{Item, Problem, Ref};
use crate::library::{Kind, item_of};
use crate::nav::Dest;
use crate::pages::scrolled;
use crate::widgets::embedded::Embedded;
use crate::widgets::hero::Hero;

/// How long a fetched page counts as fresh: an older one is shown and fetched again quietly.
pub const FRESH: Duration = Duration::from_secs(10 * 60);
/// A request that takes longer than this gets a spinner.
const SPINNER_AFTER: Duration = Duration::from_millis(350);
/// Pages kept per cache.
const KEPT: usize = 40;
/// Space kept around a widget that is scrolled into view for the keyboard.
const FOCUS_MARGIN: f64 = 16.0;

/// What pages showed last, by destination.
pub struct Cache<T> {
    /// The most recently used last; with the time it was fetched (`None`: to be fetched again).
    entries: Vec<(Dest, Rc<T>, Option<Instant>)>,
}

impl<T> Cache<T> {
    pub const fn new() -> Cache<T> {
        Cache { entries: Vec::new() }
    }

    /// The copy kept for `dest` and when it was fetched.
    pub fn get(&mut self, dest: &Dest) -> Option<(Rc<T>, Option<Instant>)> {
        let index = self.entries.iter().position(|(key, _, _)| key == dest)?;
        let entry = self.entries.remove(index);
        let found = (entry.1.clone(), entry.2);
        self.entries.push(entry);
        Some(found)
    }

    /// Keep `value`, fetched at `fetched`, for `dest`.
    pub fn put(&mut self, dest: &Dest, value: Rc<T>, fetched: Instant) {
        self.entries.retain(|(key, _, _)| key != dest);
        self.entries.push((dest.clone(), value, Some(fetched)));
        if self.entries.len() > KEPT {
            let excess = self.entries.len() - KEPT;
            self.entries.drain(..excess);
        }
    }

    /// The copy kept for a destination that `wanted` names, if there is one.
    pub fn find(&self, wanted: impl Fn(&Dest) -> bool) -> Option<Rc<T>> {
        self.entries.iter().find(|(key, _, _)| wanted(key)).map(|(_, value, _)| value.clone())
    }

    /// The copies of the destinations `old` names are to be fetched again when they are shown.
    pub fn expire(&mut self, old: impl Fn(&Dest) -> bool) {
        for (key, _, fetched) in &mut self.entries {
            if old(key) {
                *fetched = None;
            }
        }
    }

    /// Drop the copies of the destinations `gone` names.
    pub fn forget(&mut self, gone: impl Fn(&Dest) -> bool) {
        self.entries.retain(|(key, _, _)| !gone(key));
    }
}

/// Whether what was fetched at `fetched` can still be shown at `now` without asking again.
pub fn is_fresh(fetched: Option<Instant>, now: Instant) -> bool {
    fetched.is_some_and(|fetched| now.saturating_duration_since(fetched) < FRESH)
}

/// Where a page shows what it has.
pub struct Frame {
    /// What goes into the page's shell.
    pub stack: gtk::Stack,
    viewport: gtk::Viewport,
    scroller: gtk::ScrolledWindow,
    header: adw::HeaderBar,
    failed: adw::StatusPage,
    again: gtk::Button,
    /// The widget that names the page in its content: the header bar does so once it is out of
    /// sight.
    title: Rc<RefCell<glib::WeakRef<gtk::Widget>>>,
    /// Counts the waits: a spinner belongs to the wait that asked for it.
    waits: Rc<Cell<u32>>,
}

impl Frame {
    pub fn new(header: &adw::HeaderBar) -> Frame {
        // The column scrolls through a viewport. Scrolling to the widget that has the keyboard
        // is done here and not by the viewport: it would not know where the rows of an
        // embedded list are.
        let viewport = gtk::Viewport::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        viewport.set_scroll_to_focus(false);
        let scroller = scrolled(&viewport);
        keep_focus_in_view(&scroller, &viewport);

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(120);
        stack.add_named(&scroller, Some("content"));
        stack.add_named(&adw::Bin::new(), Some("blank"));
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        stack.add_named(&spinner, Some("waiting"));
        let failed = adw::StatusPage::new();
        let again = gtk::Button::with_label("Try Again");
        again.add_css_class("pill");
        again.set_halign(gtk::Align::Center);
        failed.set_child(Some(&again));
        stack.add_named(&failed, Some("failed"));
        stack.set_visible_child_name("blank");

        let frame = Frame {
            stack,
            viewport,
            scroller,
            header: header.clone(),
            failed,
            again,
            title: Rc::new(RefCell::new(glib::WeakRef::new())),
            waits: Rc::new(Cell::new(0)),
        };
        let (title, header, viewport) = (frame.title.clone(), frame.header.clone(), frame.viewport.downgrade());
        frame.adjustment().connect_value_changed(move |adjustment| {
            let (Some(viewport), Some(title)) = (viewport.upgrade(), title.borrow().upgrade()) else { return };
            let below = viewport.child().and_then(|content| title.compute_bounds(&content));
            let hidden = below.is_some_and(|bounds| adjustment.value() > (bounds.y() + bounds.height() - 6.0) as f64);
            if header.shows_title() != hidden {
                header.set_show_title(hidden);
            }
        });
        frame
    }

    /// What "Try Again" does.
    pub fn connect_retry(&self, retry: impl Fn() + 'static) {
        self.again.connect_clicked(move |_| retry());
    }

    pub fn adjustment(&self) -> gtk::Adjustment {
        self.scroller.vadjustment()
    }

    pub fn has_content(&self) -> bool {
        self.viewport.child().is_some()
    }

    /// Show `column` (in place of what was shown, at the same scroll position). `title` is the
    /// widget in it that names the page.
    pub fn show(&self, column: &impl IsA<gtk::Widget>, title: &impl IsA<gtk::Widget>) {
        self.waits.set(self.waits.get() + 1);
        self.title.borrow().set(Some(title.upcast_ref()));
        // new content starts at its top, where it names itself; content that replaces what was
        // shown stays where the page is scrolled to, and so does the header bar
        let scrolled_to = self.has_content().then(|| self.adjustment().value()).filter(|value| *value > 0.0);
        if !self.has_content() {
            self.header.set_show_title(false);
        }
        self.viewport.set_child(Some(column));
        self.stack.set_visible_child_name("content");
        if let Some(value) = scrolled_to {
            self.scroll_back_to(value);
        }
    }

    /// Go back to `value` as soon as the new content is tall enough for it (it has no height
    /// before its first layout).
    fn scroll_back_to(&self, value: f64) {
        let adjustment = self.adjustment();
        let frames = Cell::new(0);
        self.scroller.add_tick_callback(move |_, _| {
            frames.set(frames.get() + 1);
            if adjustment.upper() - adjustment.page_size() >= value {
                adjustment.set_value(value);
                return glib::ControlFlow::Break;
            }
            if frames.get() < 60 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
    }

    /// Another widget in the content names the page from now on.
    pub fn set_title(&self, title: &impl IsA<gtk::Widget>) {
        self.title.borrow().set(Some(title.upcast_ref()));
    }

    /// Nothing to show yet: a spinner once the wait has lasted a moment.
    pub fn wait(&self) {
        let wait = self.waits.get() + 1;
        self.waits.set(wait);
        self.header.set_show_title(true);
        self.stack.set_visible_child_name("blank");
        let (waits, stack) = (self.waits.clone(), self.stack.downgrade());
        glib::timeout_add_local_once(SPINNER_AFTER, move || {
            if let Some(stack) = stack.upgrade()
                && waits.get() == wait
            {
                stack.set_visible_child_name("waiting");
            }
        });
    }

    /// Show what went wrong in place of the content, with "Try Again".
    pub fn fail(&self, title: &str, problem: &Problem) {
        self.status("am-network-error-symbolic", title, &problem.message, true);
    }

    /// Say that what the page showed is no more.
    pub fn gone(&self, icon: &str, title: &str, text: &str) {
        self.status(icon, title, text, false);
    }

    fn status(&self, icon: &str, title: &str, text: &str, retry: bool) {
        self.waits.set(self.waits.get() + 1);
        self.viewport.set_child(gtk::Widget::NONE);
        self.header.set_show_title(true);
        self.failed.set_icon_name(Some(icon));
        self.failed.set_title(title);
        self.failed.set_description(Some(text));
        self.again.set_visible(retry);
        self.stack.set_visible_child_name("failed");
    }

    /// Scroll down by `offset` pixels once the content has its size (`--scroll`).
    pub fn scroll_soon(&self, offset: f64) {
        let adjustment = self.adjustment();
        glib::timeout_add_local_once(Duration::from_millis(120), move || {
            adjustment.set_value(offset.min(adjustment.upper() - adjustment.page_size()).max(0.0));
        });
    }
}

/// Scroll the page to the widget that gets the keyboard focus, unless that widget is a row of
/// an embedded list: the list sees to its rows and the page follows it.
fn keep_focus_in_view(scroller: &gtk::ScrolledWindow, viewport: &gtk::Viewport) {
    let connection: Rc<RefCell<Option<(gtk::Window, glib::SignalHandlerId)>>> = Rc::default();
    let (weak, held) = (viewport.downgrade(), connection.clone());
    scroller.connect_realize(move |scroller| {
        let Some(window) = scroller.root().and_downcast::<gtk::Window>() else { return };
        let (weak, adjustment) = (weak.clone(), scroller.vadjustment());
        let handler = window.connect_focus_widget_notify(move |window| {
            let (Some(viewport), Some(focus)) = (weak.upgrade(), gtk::prelude::GtkWindowExt::focus(window)) else {
                return;
            };
            let Some(content) = viewport.child() else { return };
            if !focus.is_ancestor(&content) || focus.ancestor(Embedded::static_type()).is_some() {
                return;
            }
            // a button of the header brings the whole header into view when the page has room
            // for it
            let page = adjustment.page_size();
            let header = focus.ancestor(Hero::static_type()).filter(|hero| hero.height() as f64 <= page);
            let Some(bounds) = header.unwrap_or(focus).compute_bounds(&content) else { return };
            let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
            if let Some(value) = revealed(adjustment.value(), page, top, bottom) {
                adjustment.set_value(value);
            }
        });
        if let Some((window, old)) = held.replace(Some((window, handler))) {
            window.disconnect(old);
        }
    });
    scroller.connect_unrealize(move |_| {
        if let Some((window, handler)) = connection.take() {
            window.disconnect(handler);
        }
    });
}

/// Where a page that shows `page` pixels from `value` on has to be scrolled to for what lies
/// between `top` and `bottom` of its content to be in view, with a little room around it; `None`
/// when it is in view.
fn revealed(value: f64, page: f64, top: f64, bottom: f64) -> Option<f64> {
    if top - FOCUS_MARGIN < value {
        Some((top - FOCUS_MARGIN).max(0.0))
    } else if bottom + FOCUS_MARGIN > value + page {
        Some(bottom + FOCUS_MARGIN - page)
    } else {
        None
    }
}

/// A page named on the command line (`--open`).
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Page(Dest),
    /// An artist by name: looked up in the library once that has loaded.
    ArtistNamed(String),
}

impl Target {
    /// "album:l.3", "album:al7", "playlist:p.0", "playlist:pl.4", "artist:r.2", "artist:ar1234"
    /// or "artist:A Name". Ids that begin with "l.", "p." and "r." are the library's, others
    /// the catalog's.
    pub fn parse(text: &str) -> Result<Target, String> {
        let wanted = || format!("--open wants album:ID, playlist:ID or artist:NAME, not {text}");
        let (kind, value) = text.split_once(':').ok_or_else(wanted)?;
        let value = value.trim();
        if value.is_empty() {
            return Err(wanted());
        }
        let by = |library: bool| Ref {
            catalog_id: (!library).then(|| value.to_string()),
            library_id: library.then(|| value.to_string()),
        };
        match kind {
            "album" => Ok(Target::Page(Dest::Album(by(value.starts_with("l."))))),
            "playlist" => Ok(Target::Page(Dest::Playlist(by(value.starts_with("p."))))),
            "artist" if value.starts_with("r.") => Ok(Target::Page(Dest::Artist(by(true)))),
            "artist" if is_artist_id(value) => Ok(Target::Page(Dest::Artist(by(false)))),
            "artist" => Ok(Target::ArtistNamed(value.to_string())),
            _ => Err(wanted()),
        }
    }
}

/// A catalog id of an artist: digits (after "ar" in the demonstration).
fn is_artist_id(value: &str) -> bool {
    let digits = value.strip_prefix("ar").unwrap_or(value);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Go to the page named on the command line.
pub fn open(target: &Target) {
    let name = match target {
        Target::Page(dest) => return ctx().nav.go(dest.clone()),
        Target::ArtistNamed(name) => name.to_lowercase(),
    };
    // the artist of that name, once the library's artists are there
    let done = Cell::new(false);
    let look = move || {
        let ctx = ctx();
        if done.get() || !ctx.library.is_loaded(Kind::Artists) {
            return;
        }
        done.set(true);
        let artists = ctx.library.artists.clone();
        let found = (0..artists.n_items()).filter_map(|i| artists.item(i).as_ref().and_then(item_of)).find_map(
            |item| match item {
                Item::Artist(artist) if artist.name.to_lowercase() == name => Some(artist),
                _ => None,
            },
        );
        match found {
            Some(artist) => {
                ctx.nav.go(Dest::Artist(Ref { catalog_id: artist.catalog_id, library_id: artist.library_id }))
            }
            None => ctx.toast.emit(&"The library has no artist of that name".to_string()),
        }
    };
    look();
    ctx().library.changed.connect_forever(move |kind| {
        if *kind == Kind::Artists {
            look();
        }
    });
}

thread_local! {
    static SCROLLED: Cell<bool> = const { Cell::new(false) };
}

/// How far `--scroll` wants the page scrolled: for the first page that has its content.
pub fn initial_scroll() -> Option<f64> {
    let offset = ctx().options.scroll?;
    (!SCROLLED.replace(true)).then_some(offset)
}

/// The heading of a part of a page ("Top Songs").
pub fn section_title(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("section-title");
    label
}

/// A button with an icon and a word in the accent colour ("Play").
pub fn pill(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_child(Some(&adw::ButtonContent::builder().icon_name(icon).label(label).build()));
    button.add_css_class("pill");
    button.add_css_class("suggested-action");
    button
}

/// A round button with an icon; `tip` says what it does (also to a screen reader).
pub fn round(icon: &str, tip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tip));
    button.update_property(&[gtk::accessible::Property::Label(tip)]);
    button.set_valign(gtk::Align::Center);
    button
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Year, month and day of "2013-05-17" (also with a time after it); month and day may be
/// missing.
fn date_parts(date: &str) -> Option<(u32, Option<u32>, Option<u32>)> {
    let date = date.split(['T', ' ']).next()?;
    let mut parts = date.split('-');
    let year = parts.next().filter(|year| year.len() == 4).and_then(|year| year.parse().ok())?;
    let month = parts.next().and_then(|month| month.parse().ok()).filter(|month| (1..=12).contains(month));
    let day = month.and(parts.next()).and_then(|day| day.parse().ok()).filter(|day| (1..=31).contains(day));
    Some((year, month, day))
}

/// "2013-05-17" → "17 May 2013"; as much of it as the date has.
pub fn long_date(date: &str) -> Option<String> {
    let (year, month, day) = date_parts(date)?;
    Some(match (month, day) {
        (Some(month), Some(day)) => format!("{day} {} {year}", MONTHS[month as usize - 1]),
        (Some(month), None) => format!("{} {year}", MONTHS[month as usize - 1]),
        _ => year.to_string(),
    })
}

/// "2013-05-17" → "2013".
pub fn year(date: &str) -> Option<String> {
    date_parts(date).map(|(year, _, _)| year.to_string())
}

/// The small line of a header: what is known, in capitals ("ALTERNATIVE · 2013").
pub fn meta_line(parts: &[Option<String>]) -> String {
    let known: Vec<&str> = parts.iter().flatten().map(|part| part.trim()).filter(|part| !part.is_empty()).collect();
    known.join(" · ").to_uppercase()
}

/// The formats an album or a song is offered in beyond plain stereo, as Apple Music names them.
pub fn badges(audio_traits: &[String]) -> Vec<&'static str> {
    let has = |name: &str| audio_traits.iter().any(|audio_trait| audio_trait == name);
    let mut badges = Vec::new();
    if has("atmos") {
        badges.push("Dolby Atmos");
    }
    if has("hi-res-lossless") {
        badges.push("Hi-Res Lossless");
    } else if has("lossless") {
        badges.push("Lossless");
    }
    badges
}

/// "1 song", "500 songs".
pub fn songs(count: usize) -> String {
    if count == 1 { "1 song".to_string() } else { format!("{count} songs") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str) -> Dest {
        Dest::Album(Ref { catalog_id: Some(id.to_string()), library_id: None })
    }

    #[test]
    fn dates_are_written_out() {
        assert_eq!(long_date("2013-05-17").as_deref(), Some("17 May 2013"));
        assert_eq!(long_date("1998-01-01").as_deref(), Some("1 January 1998"));
        assert_eq!(long_date("2026-11-01T00:00:00Z").as_deref(), Some("1 November 2026"));
        assert_eq!(long_date("2013-05").as_deref(), Some("May 2013"));
        assert_eq!(long_date("2013").as_deref(), Some("2013"));
        // what is not a month or a day is left out
        assert_eq!(long_date("2013-13-40").as_deref(), Some("2013"));
        assert_eq!(long_date("2013-02-00").as_deref(), Some("February 2013"));
        assert_eq!(long_date(""), None);
        assert_eq!(long_date("soon"), None);
        assert_eq!(year("2013-05-17").as_deref(), Some("2013"));
        assert_eq!(year("17 May"), None);
    }

    #[test]
    fn the_page_scrolls_to_what_has_the_keyboard() {
        // in view: the page stays
        assert_eq!(revealed(300.0, 600.0, 400.0, 440.0), None);
        // above what is shown, and below it
        assert_eq!(revealed(300.0, 600.0, 250.0, 290.0), Some(234.0));
        assert_eq!(revealed(300.0, 600.0, 880.0, 920.0), Some(336.0));
        // near the top of the page: no further than its beginning
        assert_eq!(revealed(300.0, 600.0, 8.0, 40.0), Some(0.0));
    }

    #[test]
    fn header_words() {
        assert_eq!(meta_line(&[Some("Alternative".into()), Some("2013".into())]), "ALTERNATIVE · 2013");
        assert_eq!(meta_line(&[None, Some("2013".into())]), "2013");
        assert_eq!(meta_line(&[Some(" ".into()), None]), "");
        let traits = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        assert_eq!(badges(&traits(&["atmos", "lossless", "lossy-stereo"])), ["Dolby Atmos", "Lossless"]);
        assert_eq!(badges(&traits(&["hi-res-lossless", "lossless"])), ["Hi-Res Lossless"]);
        assert!(badges(&traits(&["lossy-stereo"])).is_empty());
        assert_eq!(songs(1), "1 song");
        assert_eq!(songs(500), "500 songs");
    }

    #[test]
    fn the_cache_keeps_recent_pages_and_knows_their_age() {
        let start = Instant::now();
        let mut cache: Cache<&str> = Cache::new();
        assert!(cache.get(&album("1")).is_none());
        cache.put(&album("1"), Rc::new("one"), start);
        let (value, fetched) = cache.get(&album("1")).unwrap();
        assert_eq!((*value, fetched), ("one", Some(start)));
        // fresh for ten minutes, then to be fetched again (and still there to be shown)
        assert!(is_fresh(fetched, start + Duration::from_secs(599)));
        assert!(!is_fresh(fetched, start + FRESH));
        assert!(!is_fresh(None, start));
        // a newer copy replaces it
        cache.put(&album("1"), Rc::new("uno"), start + FRESH);
        assert_eq!(
            cache.get(&album("1")).map(|(value, fetched)| (*value, fetched)),
            Some(("uno", Some(start + FRESH)))
        );
        // a copy can be marked as old without losing it
        cache.expire(|dest| *dest == album("1"));
        assert_eq!(cache.get(&album("1")).map(|(value, fetched)| (*value, fetched)), Some(("uno", None)));
        // the least recently used go first
        for n in 2..KEPT + 3 {
            cache.put(&album(&n.to_string()), Rc::new("more"), start);
            let _ = cache.get(&album("2"));
        }
        assert!(cache.get(&album("1")).is_none());
        assert!(cache.get(&album("2")).is_some());
        assert!(cache.get(&album("3")).is_none());
        assert!(cache.get(&album(&(KEPT + 2).to_string())).is_some());
        cache.forget(|dest| *dest == album("2"));
        assert!(cache.get(&album("2")).is_none());
    }

    #[test]
    fn pages_named_on_the_command_line() {
        let by = |catalog: Option<&str>, library: Option<&str>| Ref {
            catalog_id: catalog.map(str::to_string),
            library_id: library.map(str::to_string),
        };
        assert_eq!(Target::parse("album:l.3"), Ok(Target::Page(Dest::Album(by(None, Some("l.3"))))));
        assert_eq!(Target::parse("album:al200"), Ok(Target::Page(Dest::Album(by(Some("al200"), None)))));
        assert_eq!(Target::parse("playlist:p.0"), Ok(Target::Page(Dest::Playlist(by(None, Some("p.0"))))));
        assert_eq!(Target::parse("playlist:pl.4"), Ok(Target::Page(Dest::Playlist(by(Some("pl.4"), None)))));
        assert_eq!(Target::parse("artist:r.12"), Ok(Target::Page(Dest::Artist(by(None, Some("r.12"))))));
        assert_eq!(Target::parse("artist:ar4711"), Ok(Target::Page(Dest::Artist(by(Some("ar4711"), None)))));
        assert_eq!(Target::parse("artist:4711"), Ok(Target::Page(Dest::Artist(by(Some("4711"), None)))));
        assert_eq!(Target::parse("artist:Nova Vale"), Ok(Target::ArtistNamed("Nova Vale".into())));
        assert_eq!(Target::parse("artist:arden"), Ok(Target::ArtistNamed("arden".into())));
        assert!(Target::parse("album").is_err());
        assert!(Target::parse("album:").is_err());
        assert!(Target::parse("station:ra.1").is_err());
    }
}
