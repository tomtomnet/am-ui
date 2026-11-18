//! Home, New and Radio: a large title and under it one shelf after the other.
//!
//! The shelves are asked for when the page is first opened and kept for the session. When the
//! page is opened again and they are older than ten minutes, or when the user asks for fresh
//! data, they are asked for again while the page stays as it is, and rebuilt only when the answer
//! differs. While the first answer is on its way the page shows grey shapes (not during the first
//! third of a second); a problem is shown with a button to try again, a missing sign-in with one
//! to sign in.
//!
//! For looking at a row that has been scrolled where there is no pointer (screenshots), the
//! environment variable `AMUI_SHELF_PRESS=N:KEYS` presses the arrow buttons of the page's Nth
//! shelf (counted from 0) once the shelves are there, `n` for "next" and `p` for "previous"
//! (`1:nnp`), and shows that shelf's arrows.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

use crate::account::AccountState;
use crate::app::ctx;
use crate::data::{self, Music, Problem, Res};
use crate::nav::Dest;
use crate::pages::{card_action, scrolled, shell};
use crate::signal::background;
use crate::widgets::card::CardHandler;
use crate::widgets::shelf::{self, SIDE, Shelf};

/// Shelves older than this are asked for again when their page is opened.
const FRESH: Duration = Duration::from_secs(600);
/// A first answer that takes longer than this is waited for with placeholders.
const PATIENCE: Duration = Duration::from_millis(350);
/// Between two shelves.
const SPACING: i32 = 22;

/// What a page asks the service for.
fn ask(dest: &Dest, music: &dyn Music) -> Res<Vec<data::Shelf>> {
    match dest {
        Dest::New => music.browse(),
        Dest::Radio => music.radio(),
        _ => music.home(),
    }
}

/// The shelves worth a place on the page: the service sends one without items now and then.
fn worth_showing(shelves: Vec<data::Shelf>) -> Vec<data::Shelf> {
    shelves.into_iter().filter(|shelf| !shelf.items.is_empty()).collect()
}

/// Whether shelves that were given `age` ago are asked for again when their page is opened.
fn outdated(age: Duration) -> bool {
    age >= FRESH
}

/// The arrow presses `AMUI_SHELF_PRESS` asks for: which shelf, and "next" (true) or "previous"
/// for each press.
fn presses(spec: &str) -> Option<(usize, Vec<bool>)> {
    let (shelf, keys) = spec.split_once(':')?;
    let keys = keys
        .chars()
        .map(|key| match key {
            'n' => Some(true),
            'p' => Some(false),
            _ => None,
        })
        .collect::<Option<Vec<bool>>>()?;
    Some((shelf.trim().parse().ok()?, keys))
}

fn signed_out() -> bool {
    ctx().account.state() == AccountState::SignedOut
}

struct Page {
    dest: Dest,
    /// "content" (the title and the shelves), "empty", "failed" or "signed-out".
    stack: gtk::Stack,
    /// Where the shelves go, under the title.
    body: gtk::Box,
    failed: adw::StatusPage,
    again: gtk::Button,
    /// The shelves on show, as the service gave them.
    shelves: RefCell<Vec<data::Shelf>>,
    /// When it gave them; `None` while the page has none to show.
    answered: Cell<Option<Instant>>,
    /// The user asked for fresh data while the page was not on show.
    stale: Cell<bool>,
    loading: Cell<bool>,
    /// Counts the requests, so that a timer knows whether it still waits for its own.
    request: Cell<u32>,
}

impl Page {
    /// The page came on show: the first request, or a quiet one when the shelves are old.
    fn opened(self: &Rc<Self>) {
        if signed_out() {
            return self.show_sign_in();
        }
        match self.answered.get() {
            None => self.load(false),
            Some(at) if self.stale.get() || outdated(at.elapsed()) => self.load(true),
            Some(_) => {}
        }
    }

    /// The user asked for fresh data (or signed in, or chose another region).
    fn refresh(self: &Rc<Self>) {
        if !self.stack.is_mapped() {
            self.stale.set(true);
        } else if !signed_out() {
            self.load(self.answered.get().is_some());
        }
    }

    /// Ask for the shelves. A quiet request leaves the page as it is until an answer differs
    /// from what is shown; a problem with it is not the user's concern.
    fn load(self: &Rc<Self>, quiet: bool) {
        if self.loading.replace(true) {
            return;
        }
        self.stale.set(false);
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        if !quiet {
            self.again.set_sensitive(false);
            // placeholders, once the answer takes its time
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(PATIENCE, move || {
                if let Some(page) = weak.upgrade()
                    && page.loading.get()
                    && page.request.get() == request
                {
                    page.show_placeholders();
                }
            });
        }
        let (music, dest, weak) = (ctx().music.clone(), self.dest.clone(), Rc::downgrade(self));
        background(
            move || ask(&dest, music.as_ref()),
            move |answer| {
                if let Some(page) = weak.upgrade() {
                    page.answer(answer, quiet);
                }
            },
        );
    }

    fn answer(&self, answer: Res<Vec<data::Shelf>>, quiet: bool) {
        self.loading.set(false);
        self.again.set_sensitive(true);
        if signed_out() {
            // the account was signed out while the request was on its way
            return self.show_sign_in();
        }
        match answer {
            Ok(shelves) => {
                let shelves = worth_showing(shelves);
                let first = self.answered.replace(Some(Instant::now())).is_none();
                if first || *self.shelves.borrow() != shelves {
                    self.shelves.replace(shelves);
                    self.rebuild();
                }
            }
            // what is shown stays; the page asks again when it is opened next
            Err(_) if quiet => {}
            Err(problem) if problem.auth => self.show_sign_in(),
            Err(problem) => self.show_problem(&problem),
        }
    }

    fn clear(&self) {
        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }
    }

    fn show_placeholders(&self) {
        self.clear();
        for _ in 0..3 {
            self.body.append(&shelf::placeholder());
        }
        self.stack.set_visible_child_name("content");
    }

    fn show_problem(&self, problem: &Problem) {
        self.clear();
        self.failed.set_description(Some(&problem.message));
        self.stack.set_visible_child_name("failed");
    }

    /// Nothing of the account that was signed in stays on the page.
    fn show_sign_in(&self) {
        self.clear();
        self.shelves.borrow_mut().clear();
        self.answered.set(None);
        self.stack.set_visible_child_name("signed-out");
    }

    /// Put the shelves on the page.
    fn rebuild(&self) {
        self.clear();
        let shelves = self.shelves.borrow();
        if shelves.is_empty() {
            self.stack.set_visible_child_name("empty");
            return;
        }
        let on_card: CardHandler = Rc::new(card_action);
        let built: Vec<Shelf> = shelves.iter().map(|shelf| Shelf::new(shelf, on_card.clone())).collect();
        for shelf in &built {
            self.body.append(shelf);
        }
        self.stack.set_visible_child_name("content");
        if let Some((index, keys)) = std::env::var("AMUI_SHELF_PRESS").ok().as_deref().and_then(presses)
            && let Some(shelf) = built.get(index).cloned()
        {
            // once the row has its width
            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                shelf.set_pointer_inside(true);
                for forward in keys {
                    shelf.turn_page(forward);
                }
            });
        }
    }
}

/// The page of Home, New or Radio.
pub fn page(dest: &Dest) -> adw::NavigationPage {
    let title = gtk::Label::builder()
        .label(dest.title())
        .accessible_role(gtk::AccessibleRole::Heading)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(SIDE)
        .margin_end(SIDE)
        .build();
    title.add_css_class("shelves-title");
    let body = gtk::Box::new(gtk::Orientation::Vertical, SPACING);
    body.set_margin_top(14);
    body.set_margin_bottom(20);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&title);
    column.append(&body);

    let empty = adw::StatusPage::new();
    empty.set_icon_name(Some(dest.icon()));
    empty.set_title("Nothing Here Yet");
    empty.set_description(Some("Apple Music has nothing for this page right now."));

    let failed = adw::StatusPage::new();
    failed.set_icon_name(Some("am-network-error-symbolic"));
    failed.set_title(&format!("Couldn't Load {}", dest.title()));
    let again = gtk::Button::with_label("Try Again");
    again.add_css_class("pill");
    again.set_halign(gtk::Align::Center);
    failed.set_child(Some(&again));

    let signed_out = adw::StatusPage::new();
    signed_out.set_icon_name(Some("am-avatar-default-symbolic"));
    signed_out.set_title("Sign In to Apple Music");
    signed_out.set_description(Some(match dest {
        Dest::New => "What is new on Apple Music appears here once you are signed in.",
        Dest::Radio => "Apple Music's stations appear here once you are signed in.",
        _ => "Music picked for you appears here once you are signed in.",
    }));
    let sign_in = gtk::Button::with_label("Sign In");
    sign_in.add_css_class("pill");
    sign_in.add_css_class("suggested-action");
    sign_in.set_halign(gtk::Align::Center);
    sign_in.connect_clicked(crate::widgets::signin::show);
    signed_out.set_child(Some(&sign_in));

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);
    stack.add_named(&scrolled(&column), Some("content"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&failed, Some("failed"));
    stack.add_named(&signed_out, Some("signed-out"));

    let page = Rc::new(Page {
        dest: dest.clone(),
        stack,
        body,
        failed,
        again,
        shelves: RefCell::default(),
        answered: Cell::new(None),
        stale: Cell::new(false),
        loading: Cell::new(false),
        request: Cell::new(0),
    });
    let weak = Rc::downgrade(&page);
    page.again.connect_clicked(move |_| {
        if let Some(page) = weak.upgrade() {
            page.load(false);
        }
    });
    let weak = Rc::downgrade(&page);
    ctx().refreshed.connect(&page.stack, move |_, _| {
        if let Some(page) = weak.upgrade() {
            page.refresh();
        }
    });
    // signed out: at once; signed in: the application asks every page for fresh data
    let weak = Rc::downgrade(&page);
    ctx().account.changed.connect(&page.stack, move |_, state| {
        if let Some(page) = weak.upgrade()
            && *state == AccountState::SignedOut
        {
            page.show_sign_in();
        }
    });

    let shell = shell(dest.title(), &page.stack);
    // the title is in the page, large
    shell.header.set_show_title(false);
    // the navigation page keeps the state alive; every time it comes on show it looks at the
    // age of its shelves
    shell.page.connect_map(move |_| page.opened());
    shell.page
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Item, Station};

    fn shelf(title: &str, items: usize) -> data::Shelf {
        let station = |n: usize| Item::Station(Station { id: format!("ra.{n}"), ..Station::default() });
        data::Shelf { title: title.to_string(), items: (0..items).map(station).collect(), ..data::Shelf::default() }
    }

    #[test]
    fn shelves_without_items_are_left_out() {
        let shown = worth_showing(vec![shelf("First", 3), shelf("Nothing", 0), shelf("Last", 1)]);
        assert_eq!(shown.iter().map(|shelf| shelf.title.as_str()).collect::<Vec<_>>(), ["First", "Last"]);
        assert!(worth_showing(vec![shelf("Nothing", 0)]).is_empty());
    }

    #[test]
    fn shelves_are_asked_for_again_after_ten_minutes() {
        assert!(!outdated(Duration::ZERO));
        assert!(!outdated(Duration::from_secs(9 * 60 + 59)));
        assert!(outdated(Duration::from_secs(10 * 60)));
        assert!(outdated(Duration::from_secs(3 * 3600)));
    }

    #[test]
    fn an_answer_is_compared_with_what_is_shown() {
        // the same shelves again: nothing to rebuild; another order, title or item: something
        let shown = vec![shelf("First", 3), shelf("Last", 1)];
        assert_eq!(shown, vec![shelf("First", 3), shelf("Last", 1)]);
        assert_ne!(shown, vec![shelf("Last", 1), shelf("First", 3)]);
        assert_ne!(shown, vec![shelf("First", 3), shelf("Latest", 1)]);
        assert_ne!(shown, vec![shelf("First", 4), shelf("Last", 1)]);
    }

    #[test]
    fn arrow_presses_for_a_screenshot() {
        assert_eq!(presses("1:nnp"), Some((1, vec![true, true, false])));
        assert_eq!(presses("0:"), Some((0, Vec::new())));
        assert_eq!(presses("2:n x"), None);
        assert_eq!(presses("nn"), None);
        assert_eq!(presses("first:n"), None);
    }
}
