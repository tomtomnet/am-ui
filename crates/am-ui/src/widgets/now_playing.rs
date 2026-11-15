//! Now Playing: the artwork with the song's title and artist on a glow of the artwork's own
//! colours, and the lyrics beside it. The view covers the sidebar and the pages (the player bar
//! stays), slides up from the bottom and back down, and is dark in both appearances.
//!
//! One widget places everything by hand and draws the artwork itself. Opening and closing move
//! the content by a transform, so nothing is laid out again while the view slides; the artwork
//! block glides the same way to the middle of the view when a song has no lyrics.

use std::cell::{Cell, OnceCell, RefCell};
use std::time::{Duration, Instant};

use adw::prelude::*;
use amui::types::{Artwork, PlayerState};
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::app::ctx;
use crate::art::{self, Key};
use crate::signal::background;
use crate::widgets::glow::{self, Glow};
use crate::widgets::lyrics::Lyrics;
use crate::widgets::player_bar::badge_text;
use crate::widgets::{activity, with_alpha};

/// The artwork is at most this large.
const ART_LARGEST: f32 = 480.0;
/// The artwork above the lyrics of a narrow view.
const ART_SMALL: f32 = 72.0;
/// Narrower than this, the lyrics go under a small artwork instead of beside a large one.
const STACK_BELOW: f32 = 640.0;
/// Milliseconds the view takes to slide in or out.
const SLIDE: u32 = 300;
/// Milliseconds the artwork block takes to its new place when lyrics come or go.
const SHIFT: u32 = 350;
/// The pointer hides when it has rested this long.
const POINTER_REST: Duration = Duration::from_secs(3);
/// A spinner shows when the lyrics take longer than this.
const SPINNER_AFTER: Duration = Duration::from_millis(350);
/// How long the artwork of the song before stays while the next one loads.
const ART_STAYS: Duration = Duration::from_millis(500);
/// A timer may fire a moment before its time: what is due this soon counts as due.
const SOON: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Place {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The value `part` of the way from `from` to `to` (exactly those at 0 and at 1).
fn between(from: f32, to: f32, part: f32) -> f32 {
    from * (1.0 - part) + to * part
}

impl Place {
    fn new(x: f32, y: f32, width: f32, height: f32) -> Place {
        Place { x, y, width, height }
    }

    /// The place `part` of the way from here to `to`, with the size of `to`.
    fn toward(&self, to: &Place, part: f32) -> Place {
        Place { x: between(self.x, to.x, part), y: between(self.y, to.y, part), ..*to }
    }
}

/// What the arrangement needs to know of its parts: the heights of the header bar, of the title,
/// of the line with artist and album and of the note, and the width and height of the badge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sizes {
    pub header: f32,
    pub title: f32,
    pub artist: f32,
    pub badge: (f32, f32),
    pub note: f32,
}

/// Where everything goes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Plan {
    /// The artwork, a square.
    pub art: Place,
    pub title: Place,
    pub artist: Place,
    pub badge: Place,
    /// The line that says there are no lyrics.
    pub note: Place,
    /// The lyrics, when the song has any.
    pub lyrics: Option<Place>,
    /// How much of the lyrics' place the header bar covers: lines fade out before they get there.
    pub inset: f32,
    /// Where the texts stand in their places: 0 at the left, 0.5 in the middle.
    pub align: f32,
}

impl Plan {
    /// The arrangement `part` of the way from this one to `to`: everything on its way to its new
    /// place, the artwork growing or shrinking as it goes.
    fn toward(&self, to: &Plan, part: f32) -> Plan {
        let side = between(self.art.width, to.art.width, part);
        Plan {
            art: Place { width: side, height: side, ..self.art.toward(&to.art, part) },
            title: self.title.toward(&to.title, part),
            artist: self.artist.toward(&to.artist, part),
            badge: self.badge.toward(&to.badge, part),
            note: self.note.toward(&to.note, part),
            align: between(self.align, to.align, part),
            ..*to
        }
    }
}

/// Arrange a view of `width` x `height`. With lyrics the artwork stands on the left with its
/// texts under it and the lyrics fill the right; in a narrow view a small artwork with the texts
/// beside it stands above the lyrics. Without lyrics the artwork and its texts are centred.
pub fn plan(width: f32, height: f32, sizes: &Sizes, lyrics: bool) -> Plan {
    let pad = (width * 0.05).clamp(24.0, 72.0);
    // nothing but the lyrics goes under the header bar
    let top = sizes.header + 8.0;
    let (badge_width, badge_height) = sizes.badge;
    if lyrics && width < STACK_BELOW {
        let text = pad + ART_SMALL + 14.0;
        let room = (width - pad - text - badge_width - 12.0).max(40.0);
        let first = top + ((ART_SMALL - sizes.title - sizes.artist) / 2.0).max(0.0);
        let below = top + ART_SMALL + 10.0;
        return Plan {
            art: Place::new(pad, top, ART_SMALL, ART_SMALL),
            title: Place::new(text, first, room, sizes.title),
            artist: Place::new(text, first + sizes.title, room, sizes.artist),
            badge: Place::new(
                width - pad - badge_width,
                top + (ART_SMALL - badge_height) / 2.0,
                badge_width,
                badge_height,
            ),
            note: Place::default(),
            lyrics: Some(Place::new(pad, below, width - 2.0 * pad, (height - below).max(1.0))),
            inset: 0.0,
            align: 0.0,
        };
    }
    // the lyrics begin a little right of the middle; the artwork is centred in what is left of it
    let split = width * 0.52;
    let column = split - (width * 0.03).clamp(24.0, 44.0) - pad;
    // a wide view gives the artwork the same size with and without lyrics (it only moves when
    // they come or go), and leaves some air between it and them
    let room = if width < STACK_BELOW { width - 2.0 * pad } else { column * 0.9 };
    let texts = 18.0 + sizes.title + 2.0 + sizes.artist + 10.0 + badge_height;
    let side = ART_LARGEST.min(room).min(height - 2.0 * top - texts).max(ART_SMALL);
    let x = if lyrics { pad + (column - side) / 2.0 } else { (width - side) / 2.0 };
    let y = ((height - side - texts) / 2.0).max(top);
    let title = y + side + 18.0;
    let artist = title + sizes.title + 2.0;
    let badge = artist + sizes.artist + 10.0;
    Plan {
        art: Place::new(x, y, side, side),
        title: Place::new(x, title, side, sizes.title),
        artist: Place::new(x, artist, side, sizes.artist),
        badge: Place::new(if lyrics { x } else { x + (side - badge_width) / 2.0 }, badge, badge_width, badge_height),
        note: Place::new(x, badge + badge_height + 14.0, side, sizes.note),
        lyrics: lyrics.then(|| Place::new(split, 0.0, (width - pad - split).max(1.0), height)),
        inset: sizes.header,
        align: if lyrics { 0.0 } else { 0.5 },
    }
}

mod imp {
    use super::*;

    pub struct NowPlaying {
        pub glow: Glow,
        pub header: adw::HeaderBar,
        pub close: gtk::Button,
        pub title: gtk::Label,
        pub artist: gtk::Label,
        pub badge: gtk::Label,
        pub note: gtk::Label,
        pub spinner: adw::Spinner,
        pub lyrics: Lyrics,
        pub scroller: gtk::ScrolledWindow,

        /// The artwork of the song, the request for it and its picture (drawn by this widget).
        pub artwork: RefCell<Option<Artwork>>,
        pub key: RefCell<Option<Key>>,
        pub texture: RefCell<Option<gdk::Texture>>,
        pub art_timer: RefCell<Option<glib::SourceId>>,
        /// The artwork whose colours the glow has or is being given.
        pub colours_of: RefCell<Option<String>>,

        /// Where everything is at the moment.
        pub plan: Cell<Plan>,
        /// The lyrics have their place (the song has some, or they are expected).
        pub beside: Cell<bool>,
        /// How far the artwork block has come to its place after lyrics came or went, 0 to 1.
        pub shift: Cell<f64>,
        pub shift_animation: OnceCell<adw::TimedAnimation>,

        pub open: Cell<bool>,
        /// How far the view has slid in, 0 to 1.
        pub slide: Cell<f64>,
        pub slide_animation: OnceCell<adw::TimedAnimation>,
        /// What had the keyboard before the view opened.
        pub focus_before: glib::WeakRef<gtk::Widget>,

        pub pointer: Cell<Option<(f64, f64)>>,
        pub pointer_moved: Cell<Option<Instant>>,
        pub pointer_timer: RefCell<Option<glib::SourceId>>,
        pub spinner_timer: RefCell<Option<glib::SourceId>>,
    }

    impl Default for NowPlaying {
        fn default() -> Self {
            let label = |class: &str| {
                let label = gtk::Label::new(None);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_single_line_mode(true);
                label.set_xalign(0.0);
                label.add_css_class(class);
                label
            };
            let lyrics = Lyrics::new();
            let scroller = lyrics.scrolled();
            let note = label("np-note");
            note.set_text("No Lyrics Available");
            note.set_xalign(0.5);
            let spinner = adw::Spinner::new();
            spinner.set_size_request(32, 32);
            NowPlaying {
                glow: Glow::new(),
                header: adw::HeaderBar::new(),
                close: gtk::Button::from_icon_name("go-down-symbolic"),
                title: label("np-title"),
                artist: label("np-artist"),
                badge: label("format-badge"),
                note,
                spinner,
                lyrics,
                scroller,
                artwork: RefCell::default(),
                key: RefCell::default(),
                texture: RefCell::default(),
                art_timer: RefCell::default(),
                colours_of: RefCell::default(),
                plan: Cell::default(),
                beside: Cell::new(false),
                shift: Cell::new(1.0),
                shift_animation: OnceCell::new(),
                open: Cell::new(false),
                slide: Cell::new(0.0),
                slide_animation: OnceCell::new(),
                focus_before: glib::WeakRef::new(),
                pointer: Cell::new(None),
                pointer_moved: Cell::new(None),
                pointer_timer: RefCell::default(),
                spinner_timer: RefCell::default(),
            }
        }
    }

    impl NowPlaying {
        /// The children from the lowest to the topmost, without the glow and the lyrics (the
        /// artwork is drawn between those and the rest).
        fn front(&self) -> [&gtk::Widget; 6] {
            [
                self.title.upcast_ref(),
                self.artist.upcast_ref(),
                self.badge.upcast_ref(),
                self.note.upcast_ref(),
                self.spinner.upcast_ref(),
                self.header.upcast_ref(),
            ]
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NowPlaying {
        const NAME: &'static str = "AmNowPlaying";
        type Type = super::NowPlaying;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("nowplaying");
        }
    }

    impl ObjectImpl for NowPlaying {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            // the content slides in from below: what is not in yet is not drawn
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.set_visible(false);

            self.header.set_show_title(false);
            self.close.set_tooltip_text(Some("Close"));
            self.close.connect_clicked(|_| ctx().show_now_playing.emit(&false));
            self.header.pack_start(&self.close);
            self.note.set_visible(false);
            self.spinner.set_visible(false);
            self.scroller.set_visible(false);
            self.badge.set_child_visible(false);
            self.glow.set_parent(&*obj);
            self.scroller.set_parent(&*obj);
            for child in self.front() {
                child.set_parent(&*obj);
            }

            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_enter(move |_, x, y| {
                if let Some(view) = weak.upgrade() {
                    view.pointer_at(x, y);
                }
            });
            let weak = obj.downgrade();
            motion.connect_motion(move |_, x, y| {
                if let Some(view) = weak.upgrade() {
                    view.pointer_at(x, y);
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(view) = weak.upgrade() {
                    view.pointer_gone();
                }
            });
            obj.add_controller(motion);

            // Escape closes the view wherever the keyboard is, unless something there (a menu, a
            // dialog, a text field) has a use for the key itself
            let keys = gtk::ShortcutController::new();
            keys.set_scope(gtk::ShortcutScope::Global);
            keys.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(gdk::Key::Escape, gdk::ModifierType::empty())),
                Some(gtk::CallbackAction::new(|widget, _| {
                    let open = widget.downcast_ref::<super::NowPlaying>().is_some_and(super::NowPlaying::is_open);
                    if open {
                        ctx().show_now_playing.emit(&false);
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                })),
            ));
            obj.add_controller(keys);

            // another screen may want the artwork at another size
            obj.connect_scale_factor_notify(|view| {
                if view.is_open() {
                    view.load_artwork();
                }
            });
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for NowPlaying {
        fn unmap(&self) {
            // hidden: no timer of this view is left running
            for timer in [&self.pointer_timer, &self.spinner_timer, &self.art_timer] {
                if let Some(timer) = timer.take() {
                    timer.remove();
                }
            }
            self.pointer.set(None);
            self.parent_unmap();
        }

        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // as large as what it covers, whatever that is
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            use gtk::Orientation::{Horizontal, Vertical};
            // every child is measured before it is placed, as GTK asks
            let natural = |child: &gtk::Widget, orientation| child.measure(orientation, -1).1 as f32;
            let size = |child: &gtk::Widget| (natural(child, Horizontal), natural(child, Vertical));
            let sizes = Sizes {
                header: size(self.header.upcast_ref()).1,
                title: size(self.title.upcast_ref()).1,
                artist: size(self.artist.upcast_ref()).1,
                badge: size(self.badge.upcast_ref()),
                note: size(self.note.upcast_ref()).1,
            };
            let _ = (size(self.glow.upcast_ref()), size(self.scroller.upcast_ref()), size(self.spinner.upcast_ref()));

            let (wide, high) = (width as f32, height as f32);
            let to = plan(wide, high, &sizes, self.beside.get());
            let part = self.shift.get() as f32;
            // while the artwork block is on its way: between where it was and where it goes
            let now = if part < 1.0 { plan(wide, high, &sizes, !self.beside.get()).toward(&to, part) } else { to };
            self.plan.set(now);

            // the whole content lies this far below its place while the view slides in or out
            let below = ((1.0 - self.slide.get()) * height as f64) as f32;
            let put = |child: &gtk::Widget, place: Place| {
                let at = graphene::Point::new(place.x.round(), place.y.round() + below);
                child.allocate(
                    (place.width.round() as i32).max(1),
                    (place.height.round() as i32).max(1),
                    -1,
                    Some(gsk::Transform::new().translate(&at)),
                );
            };
            put(self.glow.upcast_ref(), Place::new(0.0, 0.0, wide, high));
            put(self.header.upcast_ref(), Place::new(0.0, 0.0, wide, sizes.header));
            if let Some(place) = now.lyrics {
                self.lyrics.set_frame(place.y, now.inset);
                put(self.scroller.upcast_ref(), place);
                let side = 32.0;
                put(
                    self.spinner.upcast_ref(),
                    Place::new(place.x + (place.width - side) / 2.0, place.y + (place.height - side) / 2.0, side, side),
                );
            }
            for label in [&self.title, &self.artist] {
                if label.xalign() != now.align {
                    label.set_xalign(now.align);
                }
            }
            put(self.title.upcast_ref(), now.title);
            put(self.artist.upcast_ref(), now.artist);
            put(self.badge.upcast_ref(), now.badge);
            put(self.note.upcast_ref(), now.note);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            obj.snapshot_child(&self.glow, snapshot);
            obj.snapshot_child(&self.scroller, snapshot);

            // the artwork: rounded, on a soft shadow
            let art = self.plan.get().art;
            let below = ((1.0 - self.slide.get()) * obj.height() as f64) as f32;
            let bounds =
                graphene::Rect::new(art.x.round(), art.y.round() + below, art.width.round(), art.width.round());
            let shape = gsk::RoundedRect::from_rect(bounds, (art.width * 0.025).clamp(5.0, 12.0));
            let shadow = gdk::RGBA::new(0.0, 0.0, 0.0, 0.4);
            snapshot.append_outset_shadow(&shape, &shadow, 0.0, art.width * 0.04, 0.0, art.width * 0.12);
            snapshot.push_rounded_clip(&shape);
            match self.texture.borrow().as_ref() {
                Some(texture) => snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Trilinear, &bounds),
                None => snapshot.append_color(&with_alpha(&obj.color(), 0.08), &bounds),
            }
            snapshot.pop();

            for child in self.front() {
                obj.snapshot_child(child, snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct NowPlaying(ObjectSubclass<imp::NowPlaying>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for NowPlaying {
    fn default() -> Self {
        NowPlaying::new()
    }
}

impl NowPlaying {
    pub fn new() -> NowPlaying {
        let view: NowPlaying = glib::Object::new();
        let player = ctx().player.clone();
        player.song_changed.connect(&view, |view, _| view.show_song());
        player.changed.connect(&view, |view, _| view.show_state());
        player.lyrics_changed.connect(&view, |view, _| view.show_lyrics());
        view
    }

    pub fn is_open(&self) -> bool {
        self.imp().open.get()
    }

    /// Slide the view in over what it covers, or out again.
    pub fn set_open(&self, open: bool) {
        let imp = self.imp();
        if imp.open.replace(open) == open {
            return;
        }
        if open {
            self.set_visible(true);
            // what happened while the view was closed
            self.show_song();
            self.show_state();
            self.show_lyrics();
            // the keyboard comes along, and goes back where it was afterwards
            imp.focus_before.set(self.root().and_then(|root| root.focus()).as_ref());
            imp.close.grab_focus();
        } else {
            self.pointer_gone();
            let inside = self.root().and_then(|root| root.focus()).is_none_or(|focus| focus.is_ancestor(self));
            if let Some(before) = imp.focus_before.upgrade().filter(|before| inside && before.is_mapped()) {
                before.grab_focus();
            }
        }
        let animation = imp.slide_animation.get_or_init(|| {
            let weak = self.downgrade();
            let target = adw::CallbackAnimationTarget::new(move |value| {
                activity::note(activity::Source::View);
                if let Some(view) = weak.upgrade() {
                    view.imp().slide.set(value);
                    view.queue_allocate();
                    view.queue_draw();
                }
            });
            // libadwaita shows the end at once when the desktop asks for reduced motion
            let animation = adw::TimedAnimation::new(self, 0.0, 1.0, SLIDE, target);
            animation.set_easing(adw::Easing::EaseOutCubic);
            let weak = self.downgrade();
            animation.connect_done(move |_| {
                if let Some(view) = weak.upgrade().filter(|view| !view.is_open()) {
                    view.leave();
                }
            });
            animation
        });
        animation.set_value_from(imp.slide.get());
        animation.set_value_to(if open { 1.0 } else { 0.0 });
        animation.play();
    }

    /// The view has slid out: it is hidden and lets go of what it only needs while it shows.
    fn leave(&self) {
        let imp = self.imp();
        self.set_visible(false);
        if let Some(key) = imp.key.take() {
            art::loader().cancel(&key, self.as_ptr() as usize);
        }
        imp.texture.replace(None);
    }

    /// The song changed (or the view opens): its name, its artwork and the colours of that.
    fn show_song(&self) {
        let imp = self.imp();
        if !imp.open.get() {
            return;
        }
        let track = ctx().player.track();
        imp.title.set_text(track.as_ref().map_or("Not Playing", |track| track.name.as_str()));
        let line =
            track.as_ref().map_or(String::new(), |track| match (track.artist.is_empty(), track.album.is_empty()) {
                (false, false) => format!("{} — {}", track.artist, track.album),
                (false, true) => track.artist.clone(),
                _ => track.album.clone(),
            });
        imp.artist.set_text(&line);
        imp.artwork.replace(track.and_then(|track| track.artwork));
        self.load_artwork();
        self.load_colours();
    }

    /// The player's state changed: what plays in which format, and whether the glow drifts.
    fn show_state(&self) {
        let imp = self.imp();
        if !imp.open.get() {
            return;
        }
        let view = ctx().player.view();
        let text = badge_text(view.format, &view.format_detail);
        if imp.badge.text() != text {
            imp.badge.set_text(text);
        }
        imp.badge.set_tooltip_text((!view.format_detail.is_empty()).then_some(view.format_detail.as_str()));
        if view.switching {
            imp.badge.add_css_class("switching");
        } else {
            imp.badge.remove_css_class("switching");
        }
        // the badge keeps its row when it is empty: the artwork does not move when it appears
        if imp.badge.is_child_visible() == text.is_empty() {
            imp.badge.set_child_visible(!text.is_empty());
            self.queue_draw();
        }
        imp.glow.set_drifting(view.state == PlayerState::Playing && !view.buffering);
    }

    /// The lyrics of the song arrived, are on their way or are not to be had.
    fn show_lyrics(&self) {
        let imp = self.imp();
        if !imp.open.get() {
            return;
        }
        let player = ctx().player.clone();
        let (loading, track) = (player.lyrics_loading(), player.track());
        let written = |line: &amui::types::Line| !line.text.trim().is_empty() || !line.words.is_empty();
        let text = player.lyrics().filter(|text| text.lines.iter().any(written));
        // on their way: the song says whether to expect any; when it does not, the view stays as
        // it is until they are there
        let beside = match (&text, loading) {
            (Some(_), _) => true,
            (None, true) => track.as_ref().and_then(|track| track.has_lyrics).unwrap_or(imp.beside.get()),
            (None, false) => false,
        };
        imp.lyrics.set_text(text);
        imp.scroller.set_visible(beside);
        imp.note.set_visible(!beside && !loading && track.is_some());
        self.set_beside(beside);

        if loading && beside {
            if imp.spinner_timer.borrow().is_none() && !imp.spinner.is_visible() {
                let weak = self.downgrade();
                let timer = glib::timeout_add_local_once(SPINNER_AFTER, move || {
                    activity::note(activity::Source::View);
                    let Some(view) = weak.upgrade() else { return };
                    let imp = view.imp();
                    imp.spinner_timer.replace(None);
                    imp.spinner.set_visible(ctx().player.lyrics_loading() && imp.beside.get());
                });
                imp.spinner_timer.replace(Some(timer));
            }
        } else {
            if let Some(timer) = imp.spinner_timer.take() {
                timer.remove();
            }
            imp.spinner.set_visible(false);
        }
    }

    /// Give the lyrics their place or take it away; the artwork block glides to its new place.
    fn set_beside(&self, beside: bool) {
        let imp = self.imp();
        if imp.beside.replace(beside) == beside {
            return;
        }
        let animation = imp.shift_animation.get_or_init(|| {
            let weak = self.downgrade();
            let target = adw::CallbackAnimationTarget::new(move |value| {
                activity::note(activity::Source::View);
                if let Some(view) = weak.upgrade() {
                    view.imp().shift.set(value);
                    view.queue_allocate();
                    view.queue_draw();
                }
            });
            let animation = adw::TimedAnimation::new(self, 0.0, 1.0, SHIFT, target);
            animation.set_easing(adw::Easing::EaseInOutCubic);
            animation
        });
        // turned around on its way, the block goes back from where it is
        animation.set_value_from(1.0 - imp.shift.get());
        animation.play();
        self.queue_allocate();
    }

    /// Ask for the artwork of the song at the size of the largest artwork this view draws: one
    /// picture serves every size of the window.
    fn load_artwork(&self) {
        let imp = self.imp();
        let owner = self.as_ptr() as usize;
        let side = ART_LARGEST as u32 * self.scale_factor().max(1) as u32;
        let wanted = imp.artwork.borrow().as_ref().map(|artwork| Key::new(artwork, side));
        if *imp.key.borrow() == wanted {
            return;
        }
        if let Some(old) = imp.key.take() {
            art::loader().cancel(&old, owner);
        }
        let found = wanted.as_ref().and_then(|key| {
            let weak = self.downgrade();
            art::loader().request(
                key,
                owner,
                Box::new(move |ready, texture| {
                    if let Some(view) = weak.upgrade()
                        && view.imp().key.borrow().as_ref() == Some(ready)
                    {
                        view.show_artwork(Some(texture.clone()));
                    }
                }),
            )
        });
        let missing = wanted.is_none();
        imp.key.replace(wanted);
        if found.is_some() || missing {
            self.show_artwork(found);
        } else if let Some(small) = self.small_artwork() {
            self.show_artwork(Some(small));
        } else if imp.art_timer.borrow().is_none() {
            // the artwork of the song before stays for a moment while the next one loads
            let weak = self.downgrade();
            let timer = glib::timeout_add_local_once(ART_STAYS, move || {
                activity::note(activity::Source::View);
                if let Some(view) = weak.upgrade() {
                    view.imp().art_timer.replace(None);
                    view.imp().texture.replace(view.small_artwork());
                    view.queue_draw();
                }
            });
            imp.art_timer.replace(Some(timer));
        }
    }

    /// A small copy of the song's artwork that is in memory already (the player bar and the
    /// lists show one): it stands in, enlarged, until the large one is there.
    fn small_artwork(&self) -> Option<gdk::Texture> {
        let artwork = self.imp().artwork.borrow();
        let scale = self.scale_factor().max(1) as u32;
        [44, 40].into_iter().find_map(|side| art::loader().cached(&Key::new(artwork.as_ref()?, side * scale)))
    }

    fn show_artwork(&self, texture: Option<gdk::Texture>) {
        let imp = self.imp();
        if let Some(timer) = imp.art_timer.take() {
            timer.remove();
        }
        imp.texture.replace(texture);
        self.queue_draw();
    }

    /// Give the glow the colours of the song's artwork: a small copy of it is fetched, decoded
    /// and counted on a worker thread.
    fn load_colours(&self) {
        let imp = self.imp();
        let artwork = imp.artwork.borrow().clone();
        let template = artwork.as_ref().map(|artwork| artwork.url_template.clone());
        if *imp.colours_of.borrow() == template {
            return;
        }
        imp.colours_of.replace(template.clone());
        let Some(artwork) = artwork else { return self.show_colours(glow::neutral()) };
        let (music, weak) = (ctx().music.clone(), self.downgrade());
        background(
            move || {
                art::small_rgba(music.as_ref(), &artwork, glow::SAMPLE)
                    .map(|pixels| glow::palette(&pixels, glow::COLOURS))
            },
            move |colours| {
                // a newer song may have replaced the one these colours are of
                if let Some(view) = weak.upgrade().filter(|view| *view.imp().colours_of.borrow() == template) {
                    view.show_colours(colours.unwrap_or_else(glow::neutral));
                }
            },
        );
    }

    /// The glow changes to the colours of the next song over a moment; a view that is only
    /// sliding in comes with its colours.
    fn show_colours(&self, colours: Vec<glow::Colour>) {
        let imp = self.imp();
        imp.glow.set_colours(colours, imp.slide.get() >= 1.0);
    }

    /// The pointer is over the view at this point.
    fn pointer_at(&self, x: f64, y: f64) {
        let imp = self.imp();
        if imp.pointer.replace(Some((x, y))) == Some((x, y)) {
            // the view moved under a pointer at rest
            return;
        }
        imp.pointer_moved.set(Some(Instant::now()));
        self.hide_pointer(false);
        if imp.pointer_timer.borrow().is_none() && self.is_open() {
            self.watch_pointer(POINTER_REST);
        }
    }

    fn pointer_gone(&self) {
        let imp = self.imp();
        imp.pointer.set(None);
        if let Some(timer) = imp.pointer_timer.take() {
            timer.remove();
        }
        self.hide_pointer(false);
    }

    /// Hide the pointer once it has rested long enough.
    fn watch_pointer(&self, wait: Duration) {
        let weak = self.downgrade();
        let timer = glib::timeout_add_local_once(wait, move || {
            activity::note(activity::Source::View);
            let Some(view) = weak.upgrade() else { return };
            let imp = view.imp();
            imp.pointer_timer.replace(None);
            match imp.pointer_moved.get().map(|at| at.elapsed() + SOON) {
                // moved again in the meantime: wait for the rest of its rest
                Some(rested) if rested < POINTER_REST => view.watch_pointer(POINTER_REST - rested + SOON),
                _ => view.hide_pointer(true),
            }
        });
        self.imp().pointer_timer.replace(Some(timer));
    }

    fn hide_pointer(&self, hidden: bool) {
        self.set_cursor_from_name(hidden.then_some("none"));
        self.imp().lyrics.set_pointer_hidden(hidden);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: Sizes = Sizes { header: 46.0, title: 30.0, artist: 22.0, badge: (96.0, 20.0), note: 18.0 };

    /// The bottom of the texts under the artwork.
    fn bottom(plan: &Plan) -> f32 {
        plan.badge.y + plan.badge.height
    }

    #[test]
    fn a_wide_view_has_the_artwork_left_and_the_lyrics_right() {
        let plan = plan(1440.0, 827.0, &SIZES, true);
        assert_eq!(plan.art.width, 480.0);
        assert_eq!(plan.art.width, plan.art.height);
        let lyrics = plan.lyrics.unwrap();
        // the artwork and its texts keep clear of the lyrics and of the left edge
        assert!(plan.art.x >= 72.0 && plan.art.x + plan.art.width < lyrics.x - 24.0);
        // the block of artwork and texts is centred between top and bottom
        assert!((plan.art.y - (827.0 - bottom(&plan))).abs() < 0.01);
        // the lyrics reach from the top to the bottom and stop before the right edge
        assert_eq!((lyrics.y, lyrics.height), (0.0, 827.0));
        assert!((lyrics.x + lyrics.width - (1440.0 - 72.0)).abs() < 0.01);
        assert_eq!(plan.inset, 46.0);
        // the texts stand under the artwork, flush with its left edge
        assert_eq!((plan.title.x, plan.title.width), (plan.art.x, 480.0));
        assert_eq!(plan.badge.x, plan.art.x);
        assert_eq!(plan.align, 0.0);
    }

    #[test]
    fn without_lyrics_the_artwork_block_is_centred() {
        let plan = plan(1440.0, 827.0, &SIZES, false);
        assert_eq!(plan.lyrics, None);
        assert!((plan.art.x + plan.art.width / 2.0 - 720.0).abs() < 0.01);
        assert_eq!(plan.align, 0.5);
        // the badge and the note are centred under it too
        assert!((plan.badge.x + plan.badge.width / 2.0 - 720.0).abs() < 0.01);
        assert_eq!((plan.note.x, plan.note.width), (plan.art.x, plan.art.width));
        assert!(plan.note.y > bottom(&plan) && plan.note.y + plan.note.height < 827.0);
    }

    #[test]
    fn the_artwork_only_moves_when_lyrics_come_or_go() {
        for (width, height) in [(1440.0, 827.0), (900.0, 627.0), (700.0, 500.0), (2560.0, 1300.0)] {
            let (with, without) = (plan(width, height, &SIZES, true), plan(width, height, &SIZES, false));
            assert_eq!(with.art.width, without.art.width, "{width} x {height}");
            assert_eq!(with.art.y, without.art.y, "{width} x {height}");
            assert!(with.art.x < without.art.x);
        }
    }

    #[test]
    fn the_artwork_is_as_large_as_fits() {
        // a small window: the artwork shrinks to leave room for its texts under the header bar
        let plan = plan(900.0, 627.0, &SIZES, true);
        assert!(plan.art.width < 480.0 && plan.art.width > 300.0, "{}", plan.art.width);
        assert!(plan.art.y >= 54.0 && bottom(&plan) <= 627.0 - 54.0 + 0.01);
        assert!(plan.art.x + plan.art.width < plan.lyrics.unwrap().x);
        // a very large one: never more than 480
        assert_eq!(super::plan(3840.0, 2000.0, &SIZES, false).art.width, 480.0);
    }

    #[test]
    fn a_narrow_view_stacks_the_lyrics_under_a_small_artwork() {
        let plan = plan(560.0, 627.0, &SIZES, true);
        assert_eq!((plan.art.x, plan.art.y, plan.art.width), (28.0, 54.0, 72.0));
        // title and artist beside the artwork, the badge at the right
        assert!(plan.title.x > plan.art.x + plan.art.width && plan.title.y >= plan.art.y);
        assert!(plan.artist.y + plan.artist.height <= plan.art.y + plan.art.height + 0.01);
        assert!(plan.title.x + plan.title.width < plan.badge.x);
        assert!((plan.badge.x + plan.badge.width - (560.0 - 28.0)).abs() < 0.01);
        // the lyrics take the whole width below
        let lyrics = plan.lyrics.unwrap();
        assert_eq!((lyrics.x, lyrics.width), (28.0, 504.0));
        assert!(lyrics.y > plan.art.y + plan.art.height && (lyrics.y + lyrics.height - 627.0).abs() < 0.01);
        assert_eq!(plan.inset, 0.0);
        // without lyrics the same view centres a large artwork
        let alone = super::plan(560.0, 627.0, &SIZES, false);
        assert!(alone.art.width > 300.0 && (alone.art.x + alone.art.width / 2.0 - 280.0).abs() < 0.01);
    }

    #[test]
    fn the_block_glides_from_one_arrangement_to_the_other() {
        let (from, to) = (plan(1440.0, 827.0, &SIZES, true), plan(1440.0, 827.0, &SIZES, false));
        assert_eq!(from.toward(&to, 1.0), to);
        let start = from.toward(&to, 0.0);
        assert_eq!((start.art, start.title, start.align), (from.art, from.title, from.align));
        let half = from.toward(&to, 0.5);
        assert!((half.art.x - (from.art.x + to.art.x) / 2.0).abs() < 0.01);
        assert_eq!(half.align, 0.25);
        // from the small artwork of a narrow view to the large one: it grows on the way
        let (small, large) = (plan(560.0, 627.0, &SIZES, true), plan(560.0, 627.0, &SIZES, false));
        let between = small.toward(&large, 0.5);
        assert!((between.art.width - (72.0 + large.art.width) / 2.0).abs() < 0.01);
        assert_eq!(between.art.width, between.art.height);
    }
}
