//! One song of a shelf: artwork, title and artist in a compact row.
//!
//! A click plays the song, the "more" button and a right click ask for its menu, and the artist
//! is a link to their page. Pointing at the row shows the play glyph over the artwork and the
//! button; the song that is playing has its title in the accent colour and a small level meter
//! over the artwork. Built from ordinary widgets placed by hand, like `TrackRow`: a shelf holds
//! a few dozen of these at most. Its size comes from the fonts of its labels (`Metrics`): taller
//! with two taller lines of text, wider with wider letters.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use amui::types::Track;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};

use crate::actions::has_artist;
use crate::widgets::thumb::Thumb;
use crate::widgets::track_row::{draw_meter, draw_play};
use crate::widgets::{font_serial, label_line, make_link, set_link, with_alpha};

/// From the edge of the row (and of its highlight) to the artwork.
pub const PAD: i32 = 6;
/// The smallest artwork: it grows with the two lines of text beside it.
const ART: i32 = 44;
/// How much taller the artwork is than the two lines of text.
const ART_OVER_TEXT: i32 = 11;
/// The text has room for about this many letters of average width.
const TEXT_LETTERS: i32 = 36;
/// The narrowest row (the usual text size).
pub const WIDTH: i32 = 340;
const GAP: i32 = 12;
const BUTTON: i32 = 30;
/// Between the title and the mark of an explicit song, and between the text and the button.
const MARK_GAP: i32 = 5;
const BUTTON_GAP: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SongAction {
    Play,
    /// The context menu, at this point of the row.
    Menu(f64, f64),
    /// The artist's page (a click on the link).
    OpenArtist,
}

/// Called with the song's place in its shelf, the song, and what the user did.
pub type SongHandler = Rc<dyn Fn(usize, &Track, SongAction, &SongCell)>;

/// The sizes of a row that come from its fonts, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metrics {
    /// A line of the title, and a line of the artist.
    pub title: i32,
    pub artist: i32,
    /// The average width of a letter of the title (Pango units).
    pub letter: i32,
}

impl Metrics {
    pub fn art(&self) -> i32 {
        (self.title + self.artist + ART_OVER_TEXT).max(ART)
    }

    pub fn height(&self) -> i32 {
        self.art() + 2 * PAD
    }

    /// Room for the artwork, the text and the button.
    pub fn width(&self) -> i32 {
        let text = TEXT_LETTERS * self.letter / pango::SCALE;
        (PAD + self.art() + GAP + text + BUTTON_GAP + BUTTON + PAD).max(WIDTH)
    }
}

mod imp {
    use super::*;

    pub struct SongCell {
        pub track: RefCell<Option<Track>>,
        pub index: Cell<usize>,
        pub handler: RefCell<Option<SongHandler>>,
        pub thumb: Thumb,
        pub title: gtk::Label,
        pub explicit: gtk::Image,
        pub artist: gtk::Label,
        pub more: gtk::Button,
        pub hover: Cell<bool>,
        pub playing: Cell<bool>,
        /// A hairline under the row: it is not the last of its column.
        pub separator: Cell<bool>,
        /// The metrics of the fonts, with the `font_serial` they were measured at.
        pub metrics: Cell<Option<(u64, Metrics)>>,
    }

    impl Default for SongCell {
        fn default() -> Self {
            let label = |class: &str| {
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(pango::EllipsizeMode::End);
                label.set_single_line_mode(true);
                label.add_css_class(class);
                label
            };
            let artist = label("song-artist");
            artist.add_css_class("dim-label");
            // its size is the style's (-gtk-icon-size): it grows with the title
            let explicit = gtk::Image::from_icon_name("am-explicit-symbolic");
            explicit.set_icon_size(gtk::IconSize::Inherit);
            explicit.add_css_class("song-explicit");
            explicit.add_css_class("dim-label");
            explicit.set_tooltip_text(Some("Explicit"));
            explicit.set_visible(false);
            let more = gtk::Button::from_icon_name("view-more-horizontal-symbolic");
            more.add_css_class("flat");
            more.add_css_class("circular");
            more.add_css_class("hover-only");
            more.set_tooltip_text(Some("More"));
            more.set_focus_on_click(false);
            more.set_focusable(false);
            SongCell {
                track: RefCell::default(),
                index: Cell::new(0),
                handler: RefCell::default(),
                thumb: Thumb::new(ART, 5.0),
                title: label("song-title"),
                explicit,
                artist,
                more,
                hover: Cell::new(false),
                playing: Cell::new(false),
                separator: Cell::new(false),
                metrics: Cell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SongCell {
        const NAME: &'static str = "AmSongCell";
        type Type = super::SongCell;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("songcell");
            class.set_accessible_role(gtk::AccessibleRole::Button);
        }
    }

    impl ObjectImpl for SongCell {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            for child in [
                self.thumb.upcast_ref::<gtk::Widget>(),
                self.title.upcast_ref(),
                self.explicit.upcast_ref(),
                self.artist.upcast_ref(),
                self.more.upcast_ref(),
            ] {
                child.set_parent(&*obj);
            }
            let weak = obj.downgrade();
            self.more.connect_clicked(move |button| {
                if let Some(cell) = weak.upgrade() {
                    let at = button
                        .compute_point(&cell, &graphene::Point::new(BUTTON as f32 / 2.0, BUTTON as f32))
                        .unwrap_or_default();
                    cell.emit(SongAction::Menu(at.x() as f64, at.y() as f64));
                }
            });
            let weak = obj.downgrade();
            make_link(&self.artist, move || {
                if let Some(cell) = weak.upgrade() {
                    cell.emit(SongAction::OpenArtist);
                }
            });

            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_enter(move |_, _, _| {
                if let Some(cell) = weak.upgrade() {
                    cell.imp().hover.set(true);
                    cell.queue_draw();
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(cell) = weak.upgrade() {
                    cell.imp().hover.set(false);
                    cell.queue_draw();
                }
            });
            obj.add_controller(motion);

            let click = gtk::GestureClick::new();
            click.set_button(0);
            let weak = obj.downgrade();
            click.connect_pressed(move |gesture, _, x, y| {
                if let Some(cell) = weak.upgrade()
                    && gesture.current_button() == gdk::BUTTON_SECONDARY
                {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    cell.emit(SongAction::Menu(x, y));
                }
            });
            let weak = obj.downgrade();
            click.connect_released(move |gesture, _, x, y| {
                if let Some(cell) = weak.upgrade()
                    && gesture.current_button() == gdk::BUTTON_PRIMARY
                    && cell.contains(x, y)
                {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    cell.emit(SongAction::Play);
                }
            });
            obj.add_controller(click);

            // the keyboard: Enter plays (Space is the player's), the Menu key or Shift+F10 asks
            // for the menu
            let keys = gtk::EventControllerKey::new();
            let weak = obj.downgrade();
            keys.connect_key_pressed(move |_, key, _, modifiers| {
                let Some(cell) = weak.upgrade() else { return glib::Propagation::Proceed };
                let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
                match key {
                    gdk::Key::Return | gdk::Key::KP_Enter => cell.emit(SongAction::Play),
                    gdk::Key::Menu => cell.emit(cell.menu_at_middle()),
                    gdk::Key::F10 if shift => cell.emit(cell.menu_at_middle()),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            });
            obj.add_controller(keys);
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SongCell {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let metrics = self.obj().metrics();
            let size = if orientation == gtk::Orientation::Horizontal { metrics.width() } else { metrics.height() };
            (size, size, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let metrics = self.obj().metrics();
            // every child is measured before it is placed, as GTK asks: (natural width, height)
            let measure = |child: &gtk::Widget| {
                let (_, natural, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
                (natural, child.measure(gtk::Orientation::Vertical, -1).1)
            };
            // `child` in the middle of the line from `top` that is `high` tall
            let put = |child: &gtk::Widget, x: i32, wide: i32, top: i32, high: i32| {
                let (_, own) = measure(child);
                child.size_allocate(&gtk::Allocation::new(x, top + (high - own) / 2, wide.max(1), own), -1);
            };
            let art = metrics.art();
            let _ = measure(self.thumb.upcast_ref());
            self.thumb.size_allocate(&gtk::Allocation::new(PAD, (height - art) / 2, art, art), -1);
            let more_x = width - PAD - BUTTON;
            put(self.more.upcast_ref(), more_x, BUTTON, 0, height);

            let text_x = PAD + art + GAP;
            let text_width = (more_x - BUTTON_GAP - text_x).max(1);
            let top = (height - metrics.title - metrics.artist) / 2;
            let (title_natural, _) = measure(self.title.upcast_ref());
            let mut title_width = text_width;
            if self.explicit.is_visible() {
                // the mark follows the title's last letter, or its ellipsis
                let (mark, _) = measure(self.explicit.upcast_ref());
                title_width = title_natural.min(text_width - mark - MARK_GAP).max(1);
                put(self.explicit.upcast_ref(), text_x + title_width + MARK_GAP, mark, top, metrics.title);
            }
            put(self.title.upcast_ref(), text_x, title_width, top, metrics.title);
            // a link: no wider than its text
            let (artist_natural, _) = measure(self.artist.upcast_ref());
            put(self.artist.upcast_ref(), text_x, artist_natural.min(text_width), top + metrics.title, metrics.artist);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            let art = obj.metrics().art() as f32;
            let (hover, playing) = (self.hover.get(), self.playing.get());
            if self.separator.get() && !hover {
                // one device pixel, from where the text begins
                let line = 1.0 / obj.scale_factor().max(1) as f32;
                let x = PAD as f32 + art + GAP as f32;
                let bounds = graphene::Rect::new(x, height - line, width - x - PAD as f32, line);
                snapshot.append_color(&with_alpha(&obj.color(), 0.14), &bounds);
            }
            let mut child = obj.first_child();
            while let Some(widget) = child {
                if widget.is_mapped() {
                    obj.snapshot_child(&widget, snapshot);
                }
                child = widget.next_sibling();
            }
            if hover || playing {
                // the glyphs grow with the artwork
                let scale = art / ART as f32;
                let bounds = graphene::Rect::new(PAD as f32, (height - art) / 2.0, art, art);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(bounds, 5.0 * scale));
                snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.45), &bounds);
                snapshot.pop();
                let white = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
                if playing && !hover {
                    draw_meter(snapshot, &bounds, &white, scale);
                } else {
                    draw_play(snapshot, &bounds, &white, scale);
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct SongCell(ObjectSubclass<imp::SongCell>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SongCell {
    /// The row of `track`, the song at `index` of its shelf.
    pub fn new(index: usize, track: &Track, handler: SongHandler) -> SongCell {
        let cell: SongCell = glib::Object::new();
        let imp = cell.imp();
        imp.index.set(index);
        imp.handler.replace(Some(handler));
        imp.title.set_text(&track.name);
        imp.artist.set_text(&track.artist);
        set_link(&imp.artist, has_artist(track));
        imp.explicit.set_visible(track.content_rating.as_deref() == Some("explicit"));
        imp.thumb.set_artwork(track.artwork.as_ref());
        if !track.playable() {
            cell.add_css_class("unavailable");
        }
        cell.update_property(&[gtk::accessible::Property::Label(&format!("{}, {}", track.name, track.artist))]);
        imp.track.replace(Some(track.clone()));
        cell
    }

    pub fn track(&self) -> Option<Track> {
        self.imp().track.borrow().clone()
    }

    /// The sizes that come from the fonts of the labels, measured again when one of those fonts
    /// changes.
    pub fn metrics(&self) -> Metrics {
        let imp = self.imp();
        let serial = font_serial(&[imp.title.upcast_ref(), imp.artist.upcast_ref()]);
        if let Some((measured, metrics)) = imp.metrics.get()
            && measured == serial
        {
            return metrics;
        }
        let letter = imp.title.pango_context().metrics(None, None).approximate_char_width();
        let metrics = Metrics { title: label_line(&imp.title), artist: label_line(&imp.artist), letter };
        imp.metrics.set(Some((serial, metrics)));
        metrics
    }

    /// Draw a hairline under the row (every row of a column but its last).
    pub fn set_separator(&self, separator: bool) {
        if self.imp().separator.replace(separator) != separator {
            self.queue_draw();
        }
    }

    pub fn set_playing(&self, playing: bool) {
        if self.imp().playing.replace(playing) != playing {
            self.queue_draw();
        }
        if playing {
            self.add_css_class("playing");
        } else {
            self.remove_css_class("playing");
        }
    }

    fn menu_at_middle(&self) -> SongAction {
        SongAction::Menu(self.width() as f64 / 2.0, self.height() as f64 / 2.0)
    }

    fn emit(&self, action: SongAction) {
        let handler = self.imp().handler.borrow().clone();
        let track = self.imp().track.borrow().clone();
        if let (Some(handler), Some(track)) = (handler, track) {
            handler(self.imp().index.get(), &track, action, self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_song_of_a_shelf_is_as_large_as_its_text() {
        // this desktop's usual text size (10 points): the size the rows were drawn with
        let usual = Metrics { title: 17, artist: 16, letter: 6731 };
        assert_eq!((usual.art(), usual.height(), usual.width()), (ART, 56, WIDTH));
        // text twice as large: artwork as tall as the two lines, and as many letters
        let double = Metrics { title: 33, artist: 31, letter: 13464 };
        assert_eq!((double.art(), double.height(), double.width()), (75, 87, 608));
    }
}
