//! Where the window can go, and how it got there.
//!
//! The sidebar's sections are root pages that live for the whole session (each keeps its scroll
//! position). Opening an album, a playlist or an artist pushes a page on top. Back and forward
//! walk the whole history, across sections too.
//!
//! A page that is opened, or that comes back when one is closed, fades in; only the back swipe
//! slides (the navigation view's own animation, which follows the fingers).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::data::Ref;
use crate::signal::Signal;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Dest {
    Home,
    New,
    Radio,
    Recent,
    Artists,
    Albums,
    Songs,
    Playlists,
    Search,
    Album(Ref),
    Playlist(Ref),
    Artist(Ref),
}

impl Dest {
    /// The destinations of the sidebar, in its order.
    pub const ROOTS: [Dest; 8] =
        [Dest::Home, Dest::New, Dest::Radio, Dest::Recent, Dest::Artists, Dest::Albums, Dest::Songs, Dest::Playlists];

    pub fn parse(name: &str) -> Option<Dest> {
        Dest::ROOTS.iter().chain([Dest::Search].iter()).find(|dest| dest.name() == name).cloned()
    }

    /// A page of its own that other pages are pushed onto.
    pub fn is_root(&self) -> bool {
        !matches!(self, Dest::Album(_) | Dest::Playlist(_) | Dest::Artist(_))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Dest::Home => "home",
            Dest::New => "new",
            Dest::Radio => "radio",
            Dest::Recent => "recent",
            Dest::Artists => "artists",
            Dest::Albums => "albums",
            Dest::Songs => "songs",
            Dest::Playlists => "playlists",
            Dest::Search => "search",
            Dest::Album(_) => "album",
            Dest::Playlist(_) => "playlist",
            Dest::Artist(_) => "artist",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Dest::Home => "Home",
            Dest::New => "New",
            Dest::Radio => "Radio",
            Dest::Recent => "Recently Added",
            Dest::Artists => "Artists",
            Dest::Albums => "Albums",
            Dest::Songs => "Songs",
            Dest::Playlists => "Playlists",
            Dest::Search => "Search",
            Dest::Album(_) => "Album",
            Dest::Playlist(_) => "Playlist",
            Dest::Artist(_) => "Artist",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Dest::Home => "am-user-home-symbolic",
            Dest::New => "am-view-app-grid-symbolic",
            Dest::Radio => "am-radio-symbolic",
            Dest::Recent => "am-document-open-recent-symbolic",
            Dest::Artists | Dest::Artist(_) => "am-audio-input-microphone-symbolic",
            Dest::Albums | Dest::Album(_) => "am-album-symbolic",
            Dest::Songs => "am-audio-x-generic-symbolic",
            Dest::Playlists | Dest::Playlist(_) => "am-view-list-symbolic",
            Dest::Search => "am-system-search-symbolic",
        }
    }
}

struct Entry {
    dest: Dest,
    page: adw::NavigationPage,
}

type Builder = Box<dyn Fn(&Dest) -> adw::NavigationPage>;

pub struct Navigator {
    pub view: adw::NavigationView,
    build: RefCell<Option<Builder>>,
    roots: RefCell<HashMap<Dest, adw::NavigationPage>>,
    history: RefCell<Vec<Entry>>,
    index: Cell<usize>,
    /// The view is being changed by `show()`, not by the user's back gesture.
    internal: Cell<bool>,
    /// The page that fades in, and its animation.
    fading: RefCell<Option<(adw::NavigationPage, adw::TimedAnimation)>>,
    /// The current destination changed.
    pub changed: Signal<Dest>,
}

const HISTORY: usize = 60;
/// Milliseconds a page takes to fade in.
const FADE_MS: u32 = 180;

impl Navigator {
    pub fn new() -> Rc<Navigator> {
        let navigator = Rc::new(Navigator {
            view: adw::NavigationView::new(),
            build: RefCell::new(None),
            roots: RefCell::default(),
            history: RefCell::default(),
            index: Cell::new(0),
            internal: Cell::new(false),
            fading: RefCell::default(),
            changed: Signal::new(),
        });
        // pushing and popping fade (`fade_in`) instead of sliding
        navigator.view.set_animate_transitions(false);
        // the view's own back gesture or Alt+Left popped a page
        let weak = Rc::downgrade(&navigator);
        navigator.view.connect_popped(move |_, _| {
            let Some(navigator) = weak.upgrade() else { return };
            if !navigator.internal.get() && navigator.index.get() > 0 {
                navigator.index.set(navigator.index.get() - 1);
                navigator.announce();
            }
        });
        navigator
    }

    /// How a destination becomes a page.
    pub fn set_builder(&self, build: impl Fn(&Dest) -> adw::NavigationPage + 'static) {
        self.build.replace(Some(Box::new(build)));
    }

    pub fn current(&self) -> Option<Dest> {
        self.history.borrow().get(self.index.get()).map(|entry| entry.dest.clone())
    }

    pub fn can_go_back(&self) -> bool {
        self.index.get() > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index.get() + 1 < self.history.borrow().len()
    }

    fn page_for(&self, dest: &Dest) -> Option<adw::NavigationPage> {
        if dest.is_root()
            && let Some(page) = self.roots.borrow().get(dest)
        {
            return Some(page.clone());
        }
        let page = (self.build.borrow().as_ref()?)(dest);
        if dest.is_root() {
            self.roots.borrow_mut().insert(dest.clone(), page.clone());
        }
        Some(page)
    }

    pub fn go(&self, dest: Dest) {
        if self.current().as_ref() == Some(&dest) {
            return;
        }
        let Some(page) = self.page_for(&dest) else { return };
        {
            let mut history = self.history.borrow_mut();
            let keep = (self.index.get() + 1).min(history.len());
            history.truncate(keep);
            history.push(Entry { dest, page });
            if history.len() > HISTORY {
                let excess = history.len() - HISTORY;
                history.drain(..excess);
            }
            self.index.set(history.len() - 1);
        }
        self.show();
    }

    pub fn back(&self) {
        if self.can_go_back() {
            self.index.set(self.index.get() - 1);
            self.show();
        }
    }

    pub fn forward(&self) {
        if self.can_go_forward() {
            self.index.set(self.index.get() + 1);
            self.show();
        }
    }

    /// The pages from the nearest root at or before the current entry up to it.
    fn chain(&self) -> Vec<adw::NavigationPage> {
        let history = self.history.borrow();
        let index = self.index.get().min(history.len().saturating_sub(1));
        let start = (0..=index).rev().find(|i| history[*i].dest.is_root()).unwrap_or(0);
        history
            .get(start..=index)
            .map(|entries| entries.iter().map(|entry| entry.page.clone()).collect())
            .unwrap_or_default()
    }

    fn show(&self) {
        let wanted = self.chain();
        let shown: Vec<adw::NavigationPage> = {
            let stack = self.view.navigation_stack();
            (0..stack.n_items()).filter_map(|i| stack.item(i).and_downcast()).collect()
        };
        self.internal.set(true);
        // a page opened or closed fades in; switching sections shows the section at once
        let fade = if wanted.len() == shown.len() + 1 && wanted[..shown.len()] == shown[..] {
            self.view.push(&wanted[shown.len()]);
            true
        } else if wanted.len() + 1 == shown.len() && shown[..wanted.len()] == wanted[..] {
            self.view.pop();
            true
        } else {
            if wanted != shown {
                self.view.replace(&wanted);
            }
            false
        };
        self.internal.set(false);
        if fade {
            self.fade_in();
        } else {
            self.stop_fading();
        }
        self.announce();
    }

    /// Fade the visible page in. libadwaita ends the animation at once when the desktop asks for
    /// less motion.
    fn fade_in(&self) {
        self.stop_fading();
        let Some(page) = self.view.visible_page() else { return };
        let weak = page.downgrade();
        let target = adw::CallbackAnimationTarget::new(move |value| {
            if let Some(page) = weak.upgrade() {
                page.set_opacity(value);
            }
        });
        let animation = adw::TimedAnimation::new(&page, 0.0, 1.0, FADE_MS, target);
        animation.set_easing(adw::Easing::EaseOutCubic);
        page.set_opacity(0.0);
        animation.play();
        self.fading.replace(Some((page, animation)));
    }

    /// Finish a fade that is still running: its page shows fully.
    fn stop_fading(&self) {
        if let Some((page, animation)) = self.fading.take() {
            animation.skip();
            page.set_opacity(1.0);
        }
    }

    fn announce(&self) {
        if let Some(dest) = self.current() {
            self.changed.emit(&dest);
        }
    }
}
